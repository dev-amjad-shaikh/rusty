//! Integration tests for the time-travel surface: `POST /threads/{id}/fork`
//! and run-create checkpoint replay (`"checkpoint": {"checkpoint_id": …}`).
//! Driven in-process via `tower::ServiceExt::oneshot` (no sockets).

use std::path::PathBuf;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::prelude::*;
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

// --------------------------------------------------------------------- //
// Test graph
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

// --------------------------------------------------------------------- //
// App + request helpers
// --------------------------------------------------------------------- //

/// Unique temp store root, removed at the end of each test (best effort).
fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!(
        "rusty-server-timetravel-test-{}",
        uuid::Uuid::new_v4()
    ))
}

fn test_app() -> (Router, PathBuf) {
    let store = temp_store();
    let (pipeline, pipeline_spec) = pipeline_graph();
    let mut registry = GraphRegistry::new();
    registry.register("pipeline", pipeline, pipeline_spec);
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
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

/// Create a thread on `pipeline`; returns its thread id.
async fn create_thread(app: &Router) -> String {
    let (status, v) = call(app, "POST", "/threads", Some(json!({"graph": "pipeline"}))).await;
    assert_eq!(status, StatusCode::CREATED, "thread creation failed: {v}");
    v["thread_id"].as_str().unwrap().to_string()
}

/// Run a thread to completion; returns the terminal JSON.
async fn run_wait(app: &Router, thread: &str, payload: Value) -> Value {
    let (status, v) = call(
        app,
        "POST",
        &format!("/threads/{thread}/runs/wait"),
        Some(payload),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "run/wait failed: {v}");
    v
}

/// Newest-first history of a thread.
async fn history(app: &Router, thread: &str) -> Vec<Value> {
    let (status, v) = call(
        app,
        "POST",
        &format!("/threads/{thread}/history"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "history failed: {v}");
    v.as_array().unwrap().clone()
}

/// GET state of a thread.
async fn state(app: &Router, thread: &str) -> Value {
    let (status, v) = call(app, "GET", &format!("/threads/{thread}/state"), None).await;
    assert_eq!(status, StatusCode::OK, "get state failed: {v}");
    v
}

// --------------------------------------------------------------------- //
// Fork
// --------------------------------------------------------------------- //

#[tokio::test]
async fn fork_full_history_copies_all_checkpoints() {
    let (app, store) = test_app();
    let thread = create_thread(&app).await;
    let terminal = run_wait(&app, &thread, json!({})).await;
    assert_eq!(terminal["status"], json!("success"));
    assert_eq!(history(&app, &thread).await.len(), 2);

    // Full fork: both checkpoints are copied; the fork is a live thread.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/fork"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "fork failed: {v}");
    assert_eq!(v["checkpoints_copied"], json!(2));
    let fork = v["thread_id"].as_str().unwrap().to_string();
    assert_ne!(fork, thread);

    let fork_state = state(&app, &fork).await;
    assert_eq!(fork_state["values"]["log"], json!(["first", "second"]));
    assert_eq!(fork_state["next"], json!([]));
    assert_eq!(
        fork_state["checkpoint"]["checkpoint_id"],
        // Checkpoint ids are preserved across the fork.
        history(&app, &thread).await[0]["checkpoint"]["checkpoint_id"]
    );

    // The fork's history is independent: a run on it does not touch the
    // source thread's history.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/threads/{fork}/state"),
        Some(json!({"values": {"log": ["forked"]}})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "fork update_state failed: {v}");
    assert_eq!(history(&app, &fork).await.len(), 3);
    assert_eq!(history(&app, &thread).await.len(), 2);

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn fork_mid_history_copies_up_to_checkpoint() {
    let (app, store) = test_app();
    let thread = create_thread(&app).await;
    run_wait(&app, &thread, json!({})).await;

    // History is newest-first: [step 1, step 0].
    let items = history(&app, &thread).await;
    let step0_id = items[1]["checkpoint"]["checkpoint_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Fork at the step-0 boundary: only the first checkpoint is copied.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/fork"),
        Some(json!({"checkpoint_id": step0_id, "new_thread_id": "fork-at-step0"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "mid-history fork failed: {v}");
    assert_eq!(v["thread_id"], json!("fork-at-step0"));
    assert_eq!(v["checkpoints_copied"], json!(1));

    // The fork sits at the step-0 boundary: `first` done, `second` pending.
    let fork_state = state(&app, "fork-at-step0").await;
    assert_eq!(fork_state["values"]["log"], json!(["first"]));
    assert_eq!(fork_state["next"], json!(["second"]));
    assert_eq!(fork_state["checkpoint"]["checkpoint_id"], json!(step0_id));

    // Replaying the fork from that same checkpoint re-runs `second`.
    let terminal = run_wait(
        &app,
        "fork-at-step0",
        json!({"checkpoint": {"checkpoint_id": step0_id}}),
    )
    .await;
    assert_eq!(terminal["status"], json!("success"));
    assert_eq!(terminal["output"]["log"], json!(["first", "second"]));

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn fork_error_cases() {
    let (app, store) = test_app();

    // Unknown source thread → 404.
    let (status, v) = call(
        &app,
        "POST",
        "/threads/no-such-thread/fork",
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(v["error"], json!("not_found"));

    // Source thread exists but has no checkpoints → 400.
    let empty = create_thread(&app).await;
    let (status, v) = call(
        &app,
        "POST",
        &format!("/threads/{empty}/fork"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "empty fork: {v}");
    assert_eq!(v["error"], json!("bad_request"));

    // Unknown checkpoint_id → 404.
    let thread = create_thread(&app).await;
    run_wait(&app, &thread, json!({})).await;
    let (status, v) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/fork"),
        Some(json!({"checkpoint_id": "no-such-checkpoint"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "bad checkpoint fork: {v}");
    assert_eq!(v["error"], json!("not_found"));

    // Duplicate client-chosen fork id → 409.
    let (status, _) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/fork"),
        Some(json!({"new_thread_id": "dup-fork"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, v) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/fork"),
        Some(json!({"new_thread_id": "dup-fork"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "duplicate fork id: {v}");
    assert_eq!(v["error"], json!("conflict"));

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Checkpoint replay on run-create
// --------------------------------------------------------------------- //

#[tokio::test]
async fn run_with_checkpoint_replays_from_that_checkpoint() {
    let (app, store) = test_app();
    let thread = create_thread(&app).await;
    let terminal = run_wait(&app, &thread, json!({})).await;
    assert_eq!(terminal["output"]["log"], json!(["first", "second"]));

    // History is newest-first: [step 1, step 0].
    let items = history(&app, &thread).await;
    assert_eq!(items.len(), 2);
    let step0_id = items[1]["checkpoint"]["checkpoint_id"]
        .as_str()
        .unwrap()
        .to_string();
    let step1_id = items[0]["checkpoint"]["checkpoint_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Replay from the step-0 boundary: `second` re-runs on top of the
    // checkpointed state; history grows by exactly one checkpoint.
    let terminal = run_wait(
        &app,
        &thread,
        json!({"checkpoint": {"checkpoint_id": step0_id}}),
    )
    .await;
    assert_eq!(terminal["status"], json!("success"));
    assert_eq!(terminal["output"]["log"], json!(["first", "second"]));
    assert_eq!(history(&app, &thread).await.len(), 3);

    // Replay from the final checkpoint (empty next set): the run completes
    // immediately with the checkpointed state and appends no history.
    let terminal = run_wait(
        &app,
        &thread,
        json!({"checkpoint": {"checkpoint_id": step1_id}}),
    )
    .await;
    assert_eq!(terminal["status"], json!("success"));
    assert_eq!(terminal["output"]["log"], json!(["first", "second"]));
    assert_eq!(history(&app, &thread).await.len(), 3);

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn run_with_unknown_checkpoint_is_404_on_all_run_endpoints() {
    let (app, store) = test_app();
    let thread = create_thread(&app).await;
    run_wait(&app, &thread, json!({})).await;

    for suffix in ["runs", "runs/wait", "runs/stream"] {
        let (status, v) = call(
            &app,
            "POST",
            &format!("/threads/{thread}/{suffix}"),
            Some(json!({"checkpoint": {"checkpoint_id": "no-such-checkpoint"}})),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "endpoint `{suffix}` accepted unknown checkpoint: {v}"
        );
        assert_eq!(v["error"], json!("not_found"));
    }

    // A background run with a valid checkpoint is accepted (202).
    let items = history(&app, &thread).await;
    let step1_id = items[0]["checkpoint"]["checkpoint_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, v) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/runs"),
        Some(json!({"checkpoint": {"checkpoint_id": step1_id}})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "background replay failed: {v}"
    );
    assert!(v["run_id"].as_str().is_some());

    let _ = std::fs::remove_dir_all(store);
}
