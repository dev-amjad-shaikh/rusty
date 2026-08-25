//! Run recall integration tests: `GET /runs` lists the tenant's recent
//! runs — live/retained ones from the process's registry merged with
//! persisted journals, deduped by run id, newest first, bounded by
//! `?limit=`. Driven in-process via `tower::ServiceExt::oneshot`; reboot
//! coverage boots a second router over the same store dir (the
//! `crash_recovery.rs` generation pattern), so the persisted-journal half
//! of the list is exercised without sockets.

use std::path::{Path, PathBuf};

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::prelude::*;
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

const ACME: (&str, &str) = ("x-api-key", "acme-secret");
const GLOBEX: (&str, &str) = ("x-api-key", "globex-secret");

/// One node that branches on its input: `fail` unwinds the run, `interrupt`
/// suspends it, anything else completes. One node = one super-step, so a
/// completed run's journal holds exactly one checkpoint.
fn probe_graph() -> (Graph, StateSpec) {
    let spec = StateSpec::new().channel("done", Reducer::Overwrite);
    let mut builder = GraphBuilder::new();
    builder.add_node("probe", |ctx: NodeContext| async move {
        if ctx
            .state()
            .get("fail")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Err(RustyError::Node("probe failed on request".to_string()));
        }
        if let Some(value) = ctx.state().get("interrupt") {
            return Err(RustyError::Interrupt {
                value: value.clone(),
            });
        }
        Ok(NodeOutput::update("done", json!(true)))
    });
    builder.set_entry_point("probe");
    (builder.compile().unwrap(), spec)
}

/// Unique temp store root, removed at the end of each test (best effort).
fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!(
        "rusty-server-run-recall-test-{}",
        uuid::Uuid::new_v4()
    ))
}

/// A server over `store`; `keyed` adds the two-tenant API-key config, the
/// open mode otherwise.
fn app(store: &Path, keyed: bool) -> Router {
    let (graph, spec) = probe_graph();
    let mut registry = GraphRegistry::new();
    registry.register("probe", graph, spec);
    let mut config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.to_path_buf());
    if keyed {
        config = config
            .with_tenant_key("acme", "acme-secret")
            .with_tenant_key("globex", "globex-secret");
    }
    router(registry, config)
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

/// Create a thread; returns its external thread id.
async fn create_thread_as(app: &Router, auth: Option<(&str, &str)>) -> String {
    let (status, v) = call_as(
        app,
        auth,
        "POST",
        "/threads",
        Some(json!({"graph": "probe"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "thread creation failed: {v}");
    v["thread_id"].as_str().unwrap().to_string()
}

/// Create a thread with a client-chosen external id.
async fn create_named_thread_as(
    app: &Router,
    auth: Option<(&str, &str)>,
    thread_id: &str,
) -> String {
    let (status, v) = call_as(
        app,
        auth,
        "POST",
        "/threads",
        Some(json!({"graph": "probe", "thread_id": thread_id})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "thread creation failed: {v}");
    v["thread_id"].as_str().unwrap().to_string()
}

/// Run a thread to its terminal state (blocking); returns the run id.
async fn run_wait_as(
    app: &Router,
    auth: Option<(&str, &str)>,
    thread: &str,
    input: Value,
    metadata: Option<Value>,
) -> (String, Value) {
    let mut payload = json!({"input": input});
    if let Some(metadata) = metadata {
        payload["metadata"] = metadata;
    }
    let (status, v) = call_as(
        app,
        auth,
        "POST",
        &format!("/threads/{thread}/runs/wait"),
        Some(payload),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "run failed to terminate: {v}");
    (v["run_id"].as_str().unwrap().to_string(), v)
}

/// `GET /runs` as a tenant; asserts 200 and returns the list.
async fn recall_as(app: &Router, auth: Option<(&str, &str)>, uri: &str) -> Vec<Value> {
    let (status, v) = call_as(app, auth, "GET", uri, None).await;
    assert_eq!(status, StatusCode::OK, "GET {uri} failed: {v}");
    v.as_array().expect("GET /runs returns an array").clone()
}

// --------------------------------------------------------------------- //
// Live + persisted merge
// --------------------------------------------------------------------- //

#[tokio::test]
async fn recalls_live_and_persisted_runs_once_each_newest_first() {
    let store = temp_store();
    let server = app(&store, false);
    let objective = "x".repeat(600);

    let first_thread = create_thread_as(&server, None).await;
    let (first_run, terminal) = run_wait_as(
        &server,
        None,
        &first_thread,
        json!({}),
        Some(json!({"studio": {"objective": objective}})),
    )
    .await;
    assert_eq!(terminal["status"], json!("success"));
    let second_thread = create_thread_as(&server, None).await;
    let (second_run, _) = run_wait_as(&server, None, &second_thread, json!({}), None).await;

    // Both runs are still manager-held AND journaled: one entry each (the
    // live record wins the dedupe), newest first, full wire shape — the
    // first run's objective rides as a bounded excerpt (600 chars in, 500
    // out), the second run declared none and carries no metadata key.
    let list = recall_as(&server, None, "/runs").await;
    assert_eq!(list.len(), 2, "one entry per run: {list:?}");
    assert_eq!(list[0]["run_id"], json!(second_run), "newest first");
    assert_eq!(list[1]["run_id"], json!(first_run));
    let first = &list[1];
    assert_eq!(first["thread_id"], json!(first_thread));
    assert_eq!(first["graph"], json!("probe"));
    assert_eq!(first["status"], json!("success"));
    assert!(
        first["created_at"].as_str().is_some_and(|s| !s.is_empty()),
        "live runs report their acceptance time: {first}"
    );
    let excerpt = first["metadata"]["studio"]["objective"]
        .as_str()
        .expect("objective excerpt present");
    assert_eq!(excerpt.chars().count(), 500, "excerpt is bounded");
    assert!(list[0].get("metadata").is_none(), "no metadata, no key");

    // Reboot over the same store: the manager is empty, so both entries
    // now come from the persisted journals — still one per run, same
    // order, status derived from the journal. The payload's metadata is
    // not journaled, so recalled-after-restart entries omit it rather
    // than reconstruct it.
    let rebooted = app(&store, false);
    let list = recall_as(&rebooted, None, "/runs").await;
    assert_eq!(list.len(), 2, "journals keep the runs reachable: {list:?}");
    assert_eq!(list[0]["run_id"], json!(second_run));
    assert_eq!(list[1]["run_id"], json!(first_run));
    for entry in &list {
        assert_eq!(entry["status"], json!("success"));
        assert_eq!(entry["graph"], json!("probe"));
        assert!(
            entry["created_at"].as_str().is_some(),
            "the journal's earliest recorded_at stands in: {entry}"
        );
        assert!(entry.get("metadata").is_none());
        assert!(entry.get("assistant_id").is_none());
    }

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Journal-derived terminal status
// --------------------------------------------------------------------- //

#[tokio::test]
async fn recalled_status_is_what_the_journal_proves() {
    let store = temp_store();
    let server = app(&store, false);

    let ok_thread = create_thread_as(&server, None).await;
    let (ok_run, terminal) = run_wait_as(&server, None, &ok_thread, json!({}), None).await;
    assert_eq!(terminal["status"], json!("success"));
    let interrupt_thread = create_thread_as(&server, None).await;
    let (interrupted_run, terminal) = run_wait_as(
        &server,
        None,
        &interrupt_thread,
        json!({"interrupt": {"ask": "approve?"}}),
        None,
    )
    .await;
    assert_eq!(terminal["status"], json!("interrupted"));
    let fail_thread = create_thread_as(&server, None).await;
    let (failed_run, terminal) =
        run_wait_as(&server, None, &fail_thread, json!({"fail": true}), None).await;
    assert_eq!(terminal["status"], json!("error"));

    // After a reboot the manager holds nothing: every status below is
    // derived from the persisted journal alone.
    let rebooted = app(&store, false);
    let list = recall_as(&rebooted, None, "/runs").await;
    let status_of = |run_id: &str| {
        list.iter()
            .find(|entry| entry["run_id"] == json!(run_id))
            .unwrap_or_else(|| panic!("run `{run_id}` not recalled: {list:?}"))["status"]
            .clone()
    };
    assert_eq!(status_of(&ok_run), json!("success"));
    assert_eq!(status_of(&interrupted_run), json!("interrupted"));
    assert_eq!(status_of(&failed_run), json!("error"));

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Tenant isolation
// --------------------------------------------------------------------- //

#[tokio::test]
async fn recall_is_tenant_scoped_live_and_after_reboot() {
    let store = temp_store();
    let server = app(&store, true);

    // Both tenants pick the same external thread id (a supported
    // collision): the persisted-journal ownership proof must still hold.
    for auth in [ACME, GLOBEX] {
        create_named_thread_as(&server, Some(auth), "shared").await;
    }
    let (acme_run, _) = run_wait_as(&server, Some(ACME), "shared", json!({}), None).await;
    let (globex_run, _) = run_wait_as(&server, Some(GLOBEX), "shared", json!({}), None).await;

    for (auth, own, foreign) in [
        (ACME, &acme_run, &globex_run),
        (GLOBEX, &globex_run, &acme_run),
    ] {
        let list = recall_as(&server, Some(auth), "/runs").await;
        assert_eq!(list.len(), 1, "only the tenant's own run: {list:?}");
        assert_eq!(list[0]["run_id"], json!(own));
        assert_ne!(list[0]["run_id"], json!(foreign));
    }

    // The same isolation holds when the list is rebuilt from journals and
    // thread records alone (post-restart), including the same-named-thread
    // collision — the journaled checkpoint only resolves in the owning
    // tenant's namespace.
    let rebooted = app(&store, true);
    for (auth, own) in [(ACME, &acme_run), (GLOBEX, &globex_run)] {
        let list = recall_as(&rebooted, Some(auth), "/runs").await;
        assert_eq!(list.len(), 1, "persisted recall stays scoped: {list:?}");
        assert_eq!(list[0]["run_id"], json!(own));
    }

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Bounds + ordering
// --------------------------------------------------------------------- //

#[tokio::test]
async fn recall_is_bounded_and_ordered() {
    let store = temp_store();
    let server = app(&store, false);

    let mut runs = Vec::new();
    for _ in 0..3 {
        let thread = create_thread_as(&server, None).await;
        runs.push(run_wait_as(&server, None, &thread, json!({}), None).await.0);
    }

    // Default bound covers all three; the list is newest first.
    let list = recall_as(&server, None, "/runs").await;
    let ids: Vec<&str> = list
        .iter()
        .map(|entry| entry["run_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [runs[2].as_str(), runs[1].as_str(), runs[0].as_str()]);

    // `limit` cuts the tail (the oldest runs fall off first).
    let list = recall_as(&server, None, "/runs?limit=2").await;
    let ids: Vec<&str> = list
        .iter()
        .map(|entry| entry["run_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [runs[2].as_str(), runs[1].as_str()]);

    // Over-max limits clamp instead of erroring; zero lists nothing.
    let list = recall_as(&server, None, "/runs?limit=1000").await;
    assert_eq!(list.len(), 3);
    let list = recall_as(&server, None, "/runs?limit=0").await;
    assert!(list.is_empty());

    let _ = std::fs::remove_dir_all(store);
}
