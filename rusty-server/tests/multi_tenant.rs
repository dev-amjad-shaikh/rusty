//! Multi-tenancy integration tests: API-key → tenant resolution and
//! tenant isolation across threads, runs, KV namespaces, assistants, and
//! crons. Driven in-process via `tower::ServiceExt::oneshot` (no sockets).
//!
//! Two tenants are used throughout: `acme` (key `acme-secret`) and
//! `globex` (key `globex-secret`). Cross-tenant access must answer 404 —
//! never 403 — so one tenant cannot probe another tenant's resources.

use std::path::PathBuf;

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::prelude::*;
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

const ACME: (&str, &str) = ("x-api-key", "acme-secret");
const GLOBEX: (&str, &str) = ("x-api-key", "globex-secret");

// --------------------------------------------------------------------- //
// App + request helpers
// --------------------------------------------------------------------- //

/// `first -> second`, appending to a `log` channel.
fn pipeline_graph() -> (Graph, StateSpec) {
    let spec = StateSpec::new().channel("log", Reducer::Append);
    let mut builder = GraphBuilder::new();
    builder.add_node("first", |_ctx: NodeContext| async {
        Ok(NodeOutput::update("log", json!("first")))
    });
    builder.add_node("second", |_ctx: NodeContext| async {
        Ok(NodeOutput::update("log", json!("second")))
    });
    builder.set_entry_point("first");
    builder.add_edge("first", "second");
    (builder.compile().unwrap(), spec)
}

/// Unique temp store root, removed at the end of each test (best effort).
fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!("rusty-server-mt-test-{}", uuid::Uuid::new_v4()))
}

fn registry() -> GraphRegistry {
    let (pipeline, pipeline_spec) = pipeline_graph();
    let mut registry = GraphRegistry::new();
    registry.register("pipeline", pipeline, pipeline_spec);
    registry
}

/// The two-tenant app used by most tests.
fn multi_tenant_app() -> (Router, PathBuf) {
    let store = temp_store();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_tenant_key("acme", "acme-secret")
        .with_tenant_key("globex", "globex-secret");
    (router(registry(), config), store)
}

/// Send a request with explicit auth headers; returns `(status, json)`.
async fn call_as(
    app: &Router,
    auth: Option<(&str, &str)>,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some((k, v)) = auth {
        builder = builder.header(k, v);
    }
    let body = match body {
        Some(v) => {
            builder = builder.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes: Bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

/// Create a thread as a tenant; returns its external thread id.
async fn create_thread_as(app: &Router, auth: (&str, &str), graph: &str) -> String {
    let (status, v) = call_as(
        app,
        Some(auth),
        "POST",
        "/threads",
        Some(json!({"graph": graph})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "thread creation failed: {v}");
    v["thread_id"].as_str().unwrap().to_string()
}

// --------------------------------------------------------------------- //
// Authentication
// --------------------------------------------------------------------- //

#[tokio::test]
async fn unknown_and_missing_keys_are_401() {
    let (app, store) = multi_tenant_app();

    // No header at all → 401.
    let (status, v) = call_as(&app, None, "GET", "/ok", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(v["error"], json!("unauthorized"));

    // Unknown key → 401 (even a well-formed one).
    let (status, _) = call_as(&app, Some(("x-api-key", "no-such-key")), "GET", "/ok", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Both configured keys authenticate (each as its own tenant).
    for key in ["acme-secret", "globex-secret"] {
        let (status, v) = call_as(&app, Some(("x-api-key", key)), "GET", "/ok", None).await;
        assert_eq!(status, StatusCode::OK, "key `{key}` rejected: {v}");
        assert_eq!(v["ok"], json!(true));
    }

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Thread isolation
// --------------------------------------------------------------------- //

#[tokio::test]
async fn threads_are_isolated_between_tenants() {
    let (app, store) = multi_tenant_app();
    let thread = create_thread_as(&app, ACME, "pipeline").await;

    // acme runs its thread to completion.
    let (status, v) = call_as(
        &app,
        Some(ACME),
        "POST",
        &format!("/threads/{thread}/runs/wait"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "acme run failed: {v}");
    assert_eq!(v["status"], json!("success"));
    // The wire never leaks the internal `{tenant}/` prefix.
    assert_eq!(v["thread_id"], json!(thread));

    // Every thread-scoped endpoint 404s for globex (existence not leaked).
    for (method, uri, body) in [
        ("GET", format!("/threads/{thread}/state"), None),
        (
            "POST",
            format!("/threads/{thread}/state"),
            Some(json!({"values": {}})),
        ),
        (
            "POST",
            format!("/threads/{thread}/history"),
            Some(json!({})),
        ),
        ("POST", format!("/threads/{thread}/fork"), Some(json!({}))),
        ("POST", format!("/threads/{thread}/runs"), Some(json!({}))),
        (
            "POST",
            format!("/threads/{thread}/runs/wait"),
            Some(json!({})),
        ),
    ] {
        let (status, v) = call_as(&app, Some(GLOBEX), method, &uri, body).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "globex reached acme's thread via {method} {uri}: {v}"
        );
        assert_eq!(v["error"], json!("not_found"));
    }

    // Runs are tenant-scoped through their thread: acme's run id is
    // invisible to globex.
    let (status, v) = call_as(
        &app,
        Some(ACME),
        "POST",
        &format!("/threads/{thread}/runs"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let run_id = v["run_id"].as_str().unwrap().to_string();
    let (status, _) = call_as(&app, Some(GLOBEX), "GET", &format!("/runs/{run_id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, v) = call_as(&app, Some(ACME), "GET", &format!("/runs/{run_id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["thread_id"], json!(thread));

    // acme's own view is intact: the run wrote two checkpoints.
    let (status, v) = call_as(
        &app,
        Some(ACME),
        "POST",
        &format!("/threads/{thread}/history"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v.as_array().unwrap().len(), 2);

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn same_external_thread_id_coexists_in_both_tenants() {
    let (app, store) = multi_tenant_app();

    // Both tenants pick the same client-chosen thread id: no conflict.
    for auth in [ACME, GLOBEX] {
        let (status, v) = call_as(
            &app,
            Some(auth),
            "POST",
            "/threads",
            Some(json!({"graph": "pipeline", "thread_id": "shared"})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "create failed: {v}");
        assert_eq!(v["thread_id"], json!("shared"));
    }

    // acme writes state to its copy; globex's copy stays empty.
    let (status, _) = call_as(
        &app,
        Some(ACME),
        "POST",
        "/threads/shared/state",
        Some(json!({"values": {"log": ["acme-data"]}})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (_, v) = call_as(&app, Some(ACME), "GET", "/threads/shared/state", None).await;
    assert_eq!(v["values"]["log"], json!(["acme-data"]));
    assert_eq!(v["checkpoint"]["thread_id"], json!("shared"));
    let (_, v) = call_as(&app, Some(GLOBEX), "GET", "/threads/shared/state", None).await;
    assert_eq!(v["values"], json!({}));
    assert_eq!(v["checkpoint"], json!(Value::Null));

    // On disk the tenants live in separate subtrees.
    assert!(store.join("acme").join("shared").is_dir());
    assert!(!store.join("globex").join("shared").exists());
    assert!(!store.join("shared").exists(), "no flat default-tenant dir");

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// KV store isolation
// --------------------------------------------------------------------- //

#[tokio::test]
async fn kv_namespaces_are_isolated_between_tenants() {
    let (app, store) = multi_tenant_app();

    // acme writes an item.
    let (status, v) = call_as(
        &app,
        Some(ACME),
        "PUT",
        "/store/memories/user-1",
        Some(json!({"preference": "dark-mode"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "acme put failed: {v}");
    assert_eq!(v["namespace"], json!("memories"));

    // globex cannot see it: item 404, namespace lists empty.
    let (status, _) = call_as(&app, Some(GLOBEX), "GET", "/store/memories/user-1", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, v) = call_as(&app, Some(GLOBEX), "GET", "/store/memories", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v, json!([]));
    let (status, _) = call_as(&app, Some(GLOBEX), "DELETE", "/store/memories/user-1", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // globex writes the same namespace/key — full coexistence.
    let (status, _) = call_as(
        &app,
        Some(GLOBEX),
        "PUT",
        "/store/memories/user-1",
        Some(json!({"preference": "light-mode"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Each tenant reads back its own value only.
    let (_, v) = call_as(&app, Some(ACME), "GET", "/store/memories/user-1", None).await;
    assert_eq!(v["value"]["preference"], json!("dark-mode"));
    assert_eq!(v["namespace"], json!("memories"));
    let (_, v) = call_as(&app, Some(GLOBEX), "GET", "/store/memories/user-1", None).await;
    assert_eq!(v["value"]["preference"], json!("light-mode"));

    // acme's delete leaves globex's item untouched.
    let (status, _) = call_as(&app, Some(ACME), "DELETE", "/store/memories/user-1", None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call_as(&app, Some(GLOBEX), "GET", "/store/memories/user-1", None).await;
    assert_eq!(status, StatusCode::OK);

    // On disk the namespaces are separated per tenant.
    assert!(store.join("store").join("acme").join("memories").is_dir());
    assert!(store.join("store").join("globex").join("memories").is_dir());

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Assistant isolation
// --------------------------------------------------------------------- //

#[tokio::test]
async fn assistants_are_isolated_between_tenants() {
    let (app, store) = multi_tenant_app();

    // acme creates an assistant with a client-chosen id.
    let (status, v) = call_as(
        &app,
        Some(ACME),
        "POST",
        "/assistants",
        Some(json!({"name": "acme-bot", "graph": "pipeline", "assistant_id": "bot"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "acme assistant failed: {v}");
    assert_eq!(v["assistant_id"], json!("bot"));

    // globex: fetch 404, list empty, and running with acme's assistant id
    // is 404 (not a cross-tenant graph-mismatch 400 — existence stays hidden).
    let (status, _) = call_as(&app, Some(GLOBEX), "GET", "/assistants/bot", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, v) = call_as(&app, Some(GLOBEX), "GET", "/assistants", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v, json!([]));

    let globex_thread = create_thread_as(&app, GLOBEX, "pipeline").await;
    let (status, _) = call_as(
        &app,
        Some(GLOBEX),
        "POST",
        &format!("/threads/{globex_thread}/runs/wait"),
        Some(json!({"assistant_id": "bot"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The same external assistant id is free for globex: no 409 collision.
    let (status, _) = call_as(
        &app,
        Some(GLOBEX),
        "POST",
        "/assistants",
        Some(json!({"name": "globex-bot", "graph": "pipeline", "assistant_id": "bot"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Each tenant lists exactly its own assistant.
    for (auth, name) in [(ACME, "acme-bot"), (GLOBEX, "globex-bot")] {
        let (status, v) = call_as(&app, Some(auth), "GET", "/assistants", None).await;
        assert_eq!(status, StatusCode::OK);
        let listed = v.as_array().unwrap();
        assert_eq!(listed.len(), 1, "tenant must see exactly one assistant");
        assert_eq!(listed[0]["name"], json!(name));
        assert_eq!(listed[0]["assistant_id"], json!("bot"));
    }

    // On disk the records are separated per tenant.
    assert!(store
        .join("assistants")
        .join("acme")
        .join("bot.json")
        .exists());
    assert!(store
        .join("assistants")
        .join("globex")
        .join("bot.json")
        .exists());
    assert!(!store.join("assistants").join("bot.json").exists());

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Cron isolation
// --------------------------------------------------------------------- //

#[tokio::test]
async fn crons_are_isolated_between_tenants() {
    let (app, store) = multi_tenant_app();

    // acme creates a cron (long interval: never fires during the test).
    let (status, v) = call_as(
        &app,
        Some(ACME),
        "POST",
        "/crons",
        Some(json!({"graph": "pipeline", "interval_secs": 3600, "cron_id": "hourly"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "acme cron failed: {v}");
    assert_eq!(v["cron_id"], json!("hourly"));

    // globex: list empty, delete 404.
    let (status, v) = call_as(&app, Some(GLOBEX), "GET", "/crons", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v, json!([]));
    let (status, _) = call_as(&app, Some(GLOBEX), "DELETE", "/crons/hourly", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // acme still sees its cron and can delete it.
    let (status, v) = call_as(&app, Some(ACME), "GET", "/crons", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v.as_array().unwrap().len(), 1);
    let (status, v) = call_as(&app, Some(ACME), "DELETE", "/crons/hourly", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["deleted"], json!(true));
    let (status, v) = call_as(&app, Some(ACME), "GET", "/crons", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v, json!([]));

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Open mode & backward compatibility
// --------------------------------------------------------------------- //

#[tokio::test]
async fn open_mode_without_keys_still_works() {
    // No keys at all: dev mode — no header required, default tenant.
    let store = temp_store();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone());
    let app = router(registry(), config);

    let (status, _) = call_as(&app, None, "GET", "/ok", None).await;
    assert_eq!(status, StatusCode::OK);

    let (status, v) = call_as(
        &app,
        None,
        "POST",
        "/threads",
        Some(json!({"graph": "pipeline"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "open-mode thread failed: {v}");
    let thread = v["thread_id"].as_str().unwrap().to_string();
    assert!(!thread.contains('/'), "external id must be clean: {thread}");

    let (status, v) = call_as(
        &app,
        None,
        "POST",
        &format!("/threads/{thread}/runs/wait"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "open-mode run failed: {v}");
    assert_eq!(v["output"]["log"], json!(["first", "second"]));
    // No `default/` prefix leaks onto the wire.
    assert_eq!(v["thread_id"], json!(thread));

    let (_, v) = call_as(&app, None, "GET", &format!("/threads/{thread}/state"), None).await;
    assert_eq!(v["checkpoint"]["thread_id"], json!(thread));

    // Open mode keeps the legacy flat on-disk layout.
    assert!(store.join(&thread).is_dir());

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn legacy_single_key_is_default_tenant_and_coexists_with_tenant_keys() {
    let store = temp_store();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_api_key("legacy-secret")
        .with_tenant_key("acme", "acme-secret");
    let app = router(registry(), config);

    // Missing / unknown keys are rejected; both configured keys work.
    let (status, _) = call_as(&app, None, "GET", "/ok", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = call_as(&app, Some(("x-api-key", "wrong")), "GET", "/ok", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = call_as(
        &app,
        Some(("x-api-key", "legacy-secret")),
        "GET",
        "/ok",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // The legacy key lands in the default tenant: a client-chosen thread id
    // stays flat on disk, exactly like pre-multi-tenancy deployments.
    let legacy = ("x-api-key", "legacy-secret");
    let (status, v) = call_as(
        &app,
        Some(legacy),
        "POST",
        "/threads",
        Some(json!({"graph": "pipeline", "thread_id": "t1"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "legacy thread failed: {v}");
    assert_eq!(v["tenant"], json!("default"));
    let (status, _) = call_as(
        &app,
        Some(legacy),
        "POST",
        "/threads/t1/runs/wait",
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        store.join("t1").is_dir(),
        "default tenant keeps flat layout"
    );

    // acme can reuse the same external id; its data is namespaced away and
    // the default tenant cannot see acme's copy (404, not 403).
    let (status, _) = call_as(
        &app,
        Some(ACME),
        "POST",
        "/threads",
        Some(json!({"graph": "pipeline", "thread_id": "t1"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call_as(
        &app,
        Some(ACME),
        "POST",
        "/threads/t1/state",
        Some(json!({"values": {"log": ["acme"]}})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(store.join("acme").join("t1").is_dir());

    // The default tenant's t1 is untouched by acme's writes.
    let (_, v) = call_as(&app, Some(legacy), "GET", "/threads/t1/state", None).await;
    assert_eq!(v["values"]["log"], json!(["first", "second"]));
    let (_, v) = call_as(&app, Some(ACME), "GET", "/threads/t1/state", None).await;
    assert_eq!(v["values"]["log"], json!(["acme"]));

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn info_endpoint_is_unchanged_and_tenant_neutral() {
    let (app, store) = multi_tenant_app();

    // Seed tenant data, then confirm /info exposes only the public surface:
    // service metadata + registered graphs — no tenants, keys, or counts.
    let _ = create_thread_as(&app, ACME, "pipeline").await;
    let (status, v) = call_as(&app, Some(GLOBEX), "GET", "/info", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["service"], json!("rusty-server"));
    let names: Vec<&str> = v["graphs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["pipeline"]);
    let body = v.to_string();
    assert!(!body.contains("acme"), "/info leaks tenant data: {body}");
    assert!(!body.contains("tenant"), "/info leaks tenant data: {body}");

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Run-scoped endpoint isolation
// --------------------------------------------------------------------- //

#[tokio::test]
async fn run_scoped_endpoints_are_isolated_between_tenants() {
    let (app, store) = multi_tenant_app();

    // acme creates a thread and runs it to completion.
    let thread = create_thread_as(&app, ACME, "pipeline").await;
    let (status, v) = call_as(
        &app,
        Some(ACME),
        "POST",
        &format!("/threads/{thread}/runs/wait"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "acme run failed: {v}");
    let run_id = v["run_id"].as_str().unwrap().to_string();

    // globex cannot see the run through any run-scoped endpoint.
    for (method, uri) in [
        ("GET", format!("/runs/{run_id}")),
        ("GET", format!("/runs/{run_id}/events")),
        ("GET", format!("/runs/{run_id}/fixture")),
        ("POST", format!("/runs/{run_id}/cancel")),
    ] {
        let body = if method == "POST" {
            Some(json!({}))
        } else {
            None
        };
        let (status, v) = call_as(&app, Some(GLOBEX), method, &uri, body).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "globex reached acme's run via {method} {uri}: {v}"
        );
        assert_eq!(v["error"], json!("not_found"));
    }

    // acme's own view is intact on every endpoint.
    let (status, v) = call_as(&app, Some(ACME), "GET", &format!("/runs/{run_id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["thread_id"], json!(thread));

    let (status, v) = call_as(
        &app,
        Some(ACME),
        "GET",
        &format!("/runs/{run_id}/events"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["run_id"], json!(run_id));
    assert_eq!(v["complete"], json!(true));
    let events = v["events"].as_array().unwrap();
    assert!(!events.is_empty());

    let (status, _) = call_as(
        &app,
        Some(ACME),
        "GET",
        &format!("/runs/{run_id}/fixture"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Receipt isolation
// --------------------------------------------------------------------- //

#[tokio::test]
async fn run_receipts_are_isolated_between_tenants() {
    let (app, store) = multi_tenant_app();

    // acme creates a thread and runs it to completion.
    let thread = create_thread_as(&app, ACME, "pipeline").await;
    let (status, v) = call_as(
        &app,
        Some(ACME),
        "POST",
        &format!("/threads/{thread}/runs/wait"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "acme run failed: {v}");
    let run_id = v["run_id"].as_str().unwrap().to_string();

    // globex cannot fetch the receipt.
    let (status, v) = call_as(
        &app,
        Some(GLOBEX),
        "GET",
        &format!("/runs/{run_id}/receipt"),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "globex reached acme's receipt: {v}"
    );
    assert_eq!(v["error"], json!("not_found"));

    // acme can fetch the receipt (or get 409 if not yet minted — both
    // prove the run was found and the isolation check passed).
    let (status, _) = call_as(
        &app,
        Some(ACME),
        "GET",
        &format!("/runs/{run_id}/receipt"),
        None,
    )
    .await;
    assert!(
        status == StatusCode::OK || status == StatusCode::CONFLICT,
        "acme receipt fetch failed unexpectedly"
    );

    let _ = std::fs::remove_dir_all(store);
}
