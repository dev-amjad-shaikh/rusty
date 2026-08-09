//! Integration tests for the v0.2 platform surface: run-status polling,
//! assistants, crons, and the cross-thread KV store. Driven in-process via
//! `tower::ServiceExt::oneshot` (no sockets).

use std::path::PathBuf;
use std::time::Duration;

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::prelude::*;
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

// --------------------------------------------------------------------- //
// Test graphs
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

/// A slow single node, so a background run has an observable active phase.
fn slow_graph() -> (Graph, StateSpec) {
    let spec = StateSpec::new().channel("done", Reducer::Overwrite);
    let mut builder = GraphBuilder::new();
    builder.add_node("slow", |_ctx: NodeContext| async {
        tokio::time::sleep(Duration::from_millis(400)).await;
        Ok(NodeOutput::update("done", json!(true)))
    });
    builder.set_entry_point("slow");
    (builder.compile().unwrap(), spec)
}

// --------------------------------------------------------------------- //
// App + request helpers
// --------------------------------------------------------------------- //

/// Unique temp store root, removed at the end of each test (best effort).
fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!("rusty-server-v02-test-{}", uuid::Uuid::new_v4()))
}

fn test_app() -> (Router, PathBuf) {
    let store = temp_store();
    let (pipeline, pipeline_spec) = pipeline_graph();
    let (slow, slow_spec) = slow_graph();

    let mut registry = GraphRegistry::new();
    registry.register("pipeline", pipeline, pipeline_spec);
    registry.register("slow", slow, slow_spec);

    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone());
    (router(registry, config), store)
}

/// Send a request and return `(status, parsed-json-body-or-null)`.
async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
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

/// Create a thread on `graph`; returns its thread id.
async fn create_thread(app: &Router, graph: &str) -> String {
    let (status, v) = call(app, "POST", "/threads", Some(json!({"graph": graph}))).await;
    assert_eq!(status, StatusCode::CREATED, "thread creation failed: {v}");
    v["thread_id"].as_str().unwrap().to_string()
}

/// Poll `GET /crons` until `pred` holds for the named cron, or panic after
/// `timeout`.
async fn wait_for_cron(
    app: &Router,
    cron_id: &str,
    timeout: Duration,
    pred: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let (status, v) = call(app, "GET", "/crons", None).await;
        assert_eq!(status, StatusCode::OK);
        if let Some(cron) = v
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["cron_id"] == json!(cron_id))
        {
            if pred(cron) {
                return cron.clone();
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for cron `{cron_id}`"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// --------------------------------------------------------------------- //
// GET /runs/{run_id}
// --------------------------------------------------------------------- //

#[tokio::test]
async fn run_status_polling_reports_terminal_output() {
    let (app, store) = test_app();
    let thread = create_thread(&app, "slow").await;

    let (status, v) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/runs"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "background run failed: {v}");
    let run_id = v["run_id"].as_str().unwrap().to_string();

    // Immediately after scheduling, the run is active (no terminal output).
    let (status, v) = call(&app, "GET", &format!("/runs/{run_id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["run_id"], json!(run_id));
    assert_eq!(v["thread_id"], json!(thread));
    assert_eq!(v["graph"], json!("slow"));
    assert_eq!(v["attempt"], json!(1));
    assert!(matches!(v["status"].as_str(), Some("running" | "pending")));
    assert!(
        v.get("output").is_none(),
        "active run must not carry output"
    );

    // Poll until terminal; the success payload then carries the output.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let terminal = loop {
        let (_, v) = call(&app, "GET", &format!("/runs/{run_id}"), None).await;
        if v["status"] == json!("success") {
            break v;
        }
        assert!(std::time::Instant::now() < deadline, "run never finished");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(terminal["output"]["done"], json!(true));

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Assistants
// --------------------------------------------------------------------- //

#[tokio::test]
async fn assistant_create_list_get_and_run_by_id() {
    let (app, store) = test_app();

    // Create.
    let (status, v) = call(
        &app,
        "POST",
        "/assistants",
        Some(json!({
            "name": "support-bot",
            "graph": "pipeline",
            "config": {"recursion_limit": 10, "model": "test"},
            "metadata": {"team": "qa"},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "assistant create failed: {v}");
    let assistant_id = v["assistant_id"].as_str().unwrap().to_string();
    assert_eq!(v["name"], json!("support-bot"));
    assert_eq!(v["graph"], json!("pipeline"));
    assert_eq!(v["config"]["recursion_limit"], json!(10));

    // Persisted under the store root.
    assert!(
        store
            .join("assistants")
            .join(format!("{assistant_id}.json"))
            .exists(),
        "assistant file missing under store root"
    );

    // List + fetch.
    let (status, v) = call(&app, "GET", "/assistants", None).await;
    assert_eq!(status, StatusCode::OK);
    let listed = v.as_array().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["assistant_id"], json!(assistant_id));
    let (status, v) = call(&app, "GET", &format!("/assistants/{assistant_id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["metadata"]["team"], json!("qa"));

    // A second assistant on the other graph.
    let (status, v) = call(
        &app,
        "POST",
        "/assistants",
        Some(json!({"name": "slow-bot", "graph": "slow"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let slow_assistant = v["assistant_id"].as_str().unwrap().to_string();

    // Run by assistant id on a matching thread.
    let thread = create_thread(&app, "pipeline").await;
    let (status, v) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/runs/wait"),
        Some(json!({"assistant_id": assistant_id})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "run by assistant failed: {v}");
    assert_eq!(v["status"], json!("success"));
    assert_eq!(v["output"]["log"], json!(["first", "second"]));

    // Assistant bound to a different graph than the thread → 400.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/runs/wait"),
        Some(json!({"assistant_id": slow_assistant})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "expected graph mismatch: {v}"
    );

    // Unknown assistant → 404.
    let (status, _) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/runs/wait"),
        Some(json!({"assistant_id": "no-such-assistant"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn assistant_validation_rejects_bad_payloads() {
    let (app, store) = test_app();

    // Unknown graph → 400.
    let (status, _) = call(
        &app,
        "POST",
        "/assistants",
        Some(json!({"name": "x", "graph": "nope"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Duplicate client-chosen id → 409.
    let (status, _) = call(
        &app,
        "POST",
        "/assistants",
        Some(json!({"name": "a", "graph": "pipeline", "assistant_id": "fixed-id"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, v) = call(
        &app,
        "POST",
        "/assistants",
        Some(json!({"name": "b", "graph": "pipeline", "assistant_id": "fixed-id"})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "expected duplicate conflict: {v}"
    );

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Crons
// --------------------------------------------------------------------- //

#[tokio::test]
async fn cron_fires_run_with_one_second_interval() {
    let (app, store) = test_app();

    let (status, v) = call(
        &app,
        "POST",
        "/crons",
        Some(json!({"graph": "pipeline", "interval_secs": 1})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "cron create failed: {v}");
    let cron_id = v["cron_id"].as_str().unwrap().to_string();
    assert_eq!(v["interval_secs"], json!(1));
    assert_eq!(v["runs_fired"], json!(0));
    assert!(
        store.join("crons").join(format!("{cron_id}.json")).exists(),
        "cron file missing under store root"
    );

    // The 1s interval cron fires at least once (scheduler tick is 200 ms).
    let cron = wait_for_cron(&app, &cron_id, Duration::from_secs(5), |c| {
        c["runs_fired"].as_u64().unwrap_or(0) >= 1
    })
    .await;
    assert!(cron["last_run_at"].as_str().is_some());

    // DELETE stops it and removes the record.
    let (status, v) = call(&app, "DELETE", &format!("/crons/{cron_id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["deleted"], json!(true));
    assert!(!store.join("crons").join(format!("{cron_id}.json")).exists());
    let (status, v) = call(&app, "GET", "/crons", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(v.as_array().unwrap().is_empty());
    let (status, _) = call(&app, "DELETE", &format!("/crons/{cron_id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn cron_on_run_completed_delete_removes_cron() {
    let (app, store) = test_app();

    let (status, v) = call(
        &app,
        "POST",
        "/crons",
        Some(json!({
            "graph": "pipeline",
            "interval_secs": 1,
            "on_run_completed": "delete",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "cron create failed: {v}");
    let cron_id = v["cron_id"].as_str().unwrap().to_string();

    // After the first fired run completes, the cron deletes itself.
    let deadline = std::time::Instant::now() + Duration::from_secs(6);
    loop {
        let (_, v) = call(&app, "GET", "/crons", None).await;
        let still_there = v
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["cron_id"] == json!(cron_id));
        if !still_there {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "one-shot cron was not deleted after its run"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!store.join("crons").join(format!("{cron_id}.json")).exists());

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn cron_schedule_validation() {
    let (app, store) = test_app();

    // Both schedule kinds set → 400.
    let (status, _) = call(
        &app,
        "POST",
        "/crons",
        Some(json!({"graph": "pipeline", "interval_secs": 5, "cron_expr": "* * * * *"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Neither set → 400.
    let (status, _) = call(&app, "POST", "/crons", Some(json!({"graph": "pipeline"}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Zero interval → 400.
    let (status, _) = call(
        &app,
        "POST",
        "/crons",
        Some(json!({"graph": "pipeline", "interval_secs": 0})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Unparseable cron expression → 400.
    let (status, _) = call(
        &app,
        "POST",
        "/crons",
        Some(json!({"graph": "pipeline", "cron_expr": "not a cron"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // A valid 5-field expression is accepted.
    let (status, v) = call(
        &app,
        "POST",
        "/crons",
        Some(json!({"graph": "pipeline", "cron_expr": "0 9 * * 1-5"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "valid cron expr rejected: {v}");
    assert_eq!(v["cron_expr"], json!("0 9 * * 1-5"));
    // Clean up so the scheduler does not hold it.
    let cron_id = v["cron_id"].as_str().unwrap();
    let (status, _) = call(&app, "DELETE", &format!("/crons/{cron_id}"), None).await;
    assert_eq!(status, StatusCode::OK);

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Store (cross-thread KV)
// --------------------------------------------------------------------- //

#[tokio::test]
async fn store_crud_and_namespace_list() {
    let (app, store) = test_app();

    // Create → 201.
    let (status, v) = call(
        &app,
        "PUT",
        "/store/memories/user-1",
        Some(json!({"preference": "dark-mode"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "store put failed: {v}");
    assert_eq!(v["namespace"], json!("memories"));
    assert_eq!(v["key"], json!("user-1"));
    assert_eq!(v["value"]["preference"], json!("dark-mode"));
    let created_at = v["created_at"].as_str().unwrap().to_string();

    // Fetch.
    let (status, v) = call(&app, "GET", "/store/memories/user-1", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["value"]["preference"], json!("dark-mode"));

    // Overwrite → 200, created_at preserved.
    let (status, v) = call(
        &app,
        "PUT",
        "/store/memories/user-1",
        Some(json!({"preference": "light-mode"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["value"]["preference"], json!("light-mode"));
    assert_eq!(v["created_at"], json!(created_at));

    // A second key + a different namespace, then list.
    let (status, _) = call(&app, "PUT", "/store/memories/user-2", Some(json!([1, 2]))).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(&app, "PUT", "/store/other/x", Some(json!(true))).await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, v) = call(&app, "GET", "/store/memories", None).await;
    assert_eq!(status, StatusCode::OK);
    let keys: Vec<&str> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, ["user-1", "user-2"], "list must be sorted by key");

    // Delete → 200; subsequent fetch and delete → 404.
    let (status, v) = call(&app, "DELETE", "/store/memories/user-1", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["deleted"], json!(true));
    let (status, _) = call(&app, "GET", "/store/memories/user-1", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(&app, "DELETE", "/store/memories/user-1", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The other namespace is untouched.
    let (status, v) = call(&app, "GET", "/store/other", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v.as_array().unwrap().len(), 1);

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn store_rejects_unsafe_segments() {
    let (app, store) = test_app();

    for bad in ["bad%20space", "..", "a%2Fb", "%2E%2E"] {
        let (status, _) = call(&app, "PUT", &format!("/store/{bad}/key"), Some(json!(1))).await;
        assert!(
            matches!(status, StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND),
            "namespace `{bad}` unexpectedly accepted: {status}"
        );
    }

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// 404s across the new surface
// --------------------------------------------------------------------- //

#[tokio::test]
async fn not_found_for_unknown_resources() {
    let (app, store) = test_app();

    let (status, v) = call(&app, "GET", "/runs/no-such-run", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(v["error"], json!("not_found"));

    let (status, _) = call(&app, "GET", "/assistants/no-such-assistant", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = call(&app, "DELETE", "/crons/no-such-cron", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = call(&app, "GET", "/store/ns/missing-key", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Unknown namespace lists as empty rather than 404 (LangGraph parity:
    // namespaces are implicit).
    let (status, v) = call(&app, "GET", "/store/never-written", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v, json!([]));

    let _ = std::fs::remove_dir_all(store);
}
