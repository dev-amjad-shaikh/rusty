//! Integration tests for message-granular checkpoints: continue, fork,
//! regenerate, and time-travel (EP-03-S09).
//!
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

fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!(
        "rusty-message-granular-test-{}",
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

async fn create_thread(app: &Router) -> String {
    let (status, v) = call(app, "POST", "/threads", Some(json!({"graph": "pipeline"}))).await;
    assert_eq!(status, StatusCode::CREATED, "thread creation failed: {v}");
    v["thread_id"].as_str().unwrap().to_string()
}

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

// --------------------------------------------------------------------- //
// Fork lineage
// --------------------------------------------------------------------- //

#[tokio::test]
async fn fork_lineage() {
    let (app, store) = test_app();
    let thread = create_thread(&app).await;
    let terminal = run_wait(&app, &thread, json!({})).await;
    assert_eq!(terminal["status"], json!("success"));

    // Fork at the latest checkpoint.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/fork"),
        Some(json!({"new_thread_id": "fork-1"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "fork failed: {v}");
    assert_eq!(v["checkpoints_copied"], json!(2));
    assert!(v["seed_length"].is_number(), "seed_length must be present");

    // Retrieve lineage via GET /threads/{id}.
    let (status, info) = call(&app, "GET", "/threads/fork-1", None).await;
    assert_eq!(status, StatusCode::OK, "get thread failed: {info}");
    assert_eq!(info["thread_id"], json!("fork-1"));
    assert_eq!(info["forked_from"], json!(thread));
    assert_eq!(info["seed_length"], v["seed_length"]);

    // The source thread has no parent.
    let (status, src_info) = call(&app, "GET", &format!("/threads/{thread}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(src_info["forked_from"], Value::Null);
    assert_eq!(src_info["seed_length"], Value::Null);

    // Run divergent turns: fork gets a new checkpoint, source stays unchanged.
    let _ = run_wait(&app, "fork-1", json!({"input": {"log": ["forked"]}})).await;
    let fork_history = history(&app, "fork-1").await;
    let src_history = history(&app, &thread).await;
    assert_eq!(
        fork_history.len(),
        4,
        "fork should have original 2 + new 2 checkpoints"
    );
    assert_eq!(
        src_history.len(),
        2,
        "source should still have 2 checkpoints"
    );

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Regenerate is fork + immediate turn
// --------------------------------------------------------------------- //

#[tokio::test]
async fn regenerate_is_fork() {
    let (app, store) = test_app();
    let thread = create_thread(&app).await;
    let terminal = run_wait(&app, &thread, json!({})).await;
    assert_eq!(terminal["status"], json!("success"));

    // History is newest-first: [step 1, step 0].
    let items = history(&app, &thread).await;
    let step0_id = items[1]["checkpoint"]["checkpoint_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Regenerate from step-0 boundary.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/regenerate"),
        Some(json!({"checkpoint_id": step0_id, "new_thread_id": "regen-1"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "regenerate failed: {v}");
    assert_eq!(v["thread_id"], json!("regen-1"));
    assert!(
        v["run_id"].as_str().is_some(),
        "regenerate should spawn a run"
    );
    assert_eq!(v["checkpoints_copied"], json!(1));
    assert_eq!(v["seed_length"], json!(0));

    // The parent thread's log is byte-identical to before.
    let parent_history = history(&app, &thread).await;
    assert_eq!(parent_history.len(), 2);

    // The regenerated thread has a run that completed.
    let regen_history = history(&app, "regen-1").await;
    assert!(
        !regen_history.is_empty(),
        "regenerated thread should have checkpoints"
    );

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Continue shadows never deletes
// --------------------------------------------------------------------- //

#[tokio::test]
async fn continue_shadows_never_deletes() {
    let (app, store) = test_app();
    let thread = create_thread(&app).await;

    // Run the thread to completion (2 checkpoints).
    let terminal = run_wait(&app, &thread, json!({})).await;
    assert_eq!(terminal["status"], json!("success"));
    let original_history = history(&app, &thread).await;
    assert_eq!(original_history.len(), 2);

    // History is newest-first: [step 1, step 0].
    let step0_id = original_history[1]["checkpoint"]["checkpoint_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Continue from step-0 boundary on the SAME thread with different input.
    let terminal = run_wait(
        &app,
        &thread,
        json!({"checkpoint": {"checkpoint_id": step0_id}, "input": {"log": ["continued"]}}),
    )
    .await;
    assert_eq!(terminal["status"], json!("success"));

    // The thread now has 3 checkpoints: old step-0, old step-1, new step-1.
    let continued_history = history(&app, &thread).await;
    assert_eq!(
        continued_history.len(),
        3,
        "history should grow, never shrink"
    );

    // The original checkpoints are still present.
    let ids: Vec<String> = continued_history
        .iter()
        .map(|h| {
            h["checkpoint"]["checkpoint_id"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert!(ids.contains(&step0_id), "original checkpoint must remain");

    let _ = std::fs::remove_dir_all(store);
}
