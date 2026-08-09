//! Integration tests for the rusty-agent-server HTTP API, driven in-process
//! via `tower::ServiceExt::oneshot` (no sockets).

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

/// A single gate node that interrupts until resumed.
fn interrupt_graph() -> (Graph, StateSpec) {
    let spec = StateSpec::new().channel("answer", Reducer::Overwrite);
    let mut builder = GraphBuilder::new();
    builder.add_node("gate", |ctx: NodeContext| async move {
        match ctx.resume_value() {
            Some(v) => Ok(NodeOutput::update("answer", v.clone())),
            None => Err(ctx.interrupt(json!({"question": "approve?"}))),
        }
    });
    builder.set_entry_point("gate");
    (builder.compile().unwrap(), spec)
}

/// A slow node, for multitask-strategy tests.
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
    std::env::temp_dir().join(format!("rusty-server-test-{}", uuid::Uuid::new_v4()))
}

fn test_app(api_key: Option<&str>) -> (Router, PathBuf) {
    let store = temp_store();
    let (pipeline, pipeline_spec) = pipeline_graph();
    let (gate, gate_spec) = interrupt_graph();
    let (slow, slow_spec) = slow_graph();

    let mut registry = GraphRegistry::new();
    registry.register("pipeline", pipeline, pipeline_spec);
    registry.register("interrupt_gate", gate, gate_spec);
    registry.register("slow", slow, slow_spec);

    let mut config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone());
    config.api_key = api_key.map(str::to_owned);
    (router(registry, config), store)
}

/// Send a request and return `(status, parsed-json-body-or-null)`.
async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let (status, bytes) = call_raw(app, method, uri, body, &[]).await;
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

/// Send a request and return `(status, raw-body-bytes)`.
async fn call_raw(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    extra_headers: &[(&str, &str)],
) -> (StatusCode, Bytes) {
    let mut builder = Request::builder().method(method).uri(uri);
    for (k, v) in extra_headers {
        builder = builder.header(*k, *v);
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
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, bytes)
}

/// Create a thread on `graph`; returns its thread id.
async fn create_thread(app: &Router, graph: &str) -> String {
    let (status, v) = call(app, "POST", "/threads", Some(json!({"graph": graph}))).await;
    assert_eq!(status, StatusCode::CREATED, "thread creation failed: {v}");
    v["thread_id"].as_str().unwrap().to_string()
}

// --------------------------------------------------------------------- //
// Tests
// --------------------------------------------------------------------- //

#[tokio::test]
async fn ok_and_info_list_graphs() {
    let (app, store) = test_app(None);

    let (status, v) = call(&app, "GET", "/ok", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["ok"], json!(true));

    let (status, v) = call(&app, "GET", "/info", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["service"], json!("rusty-server"));
    let names: Vec<&str> = v["graphs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["interrupt_gate", "pipeline", "slow"]);
    let pipeline = &v["graphs"][1];
    assert_eq!(pipeline["channels"], json!(["log"]));

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn unknown_graph_is_rejected() {
    let (app, store) = test_app(None);
    let (status, v) = call(&app, "POST", "/threads", Some(json!({"graph": "nope"}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(v["error"], json!("bad_request"));
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn run_wait_completes_and_state_and_history_reflect_run() {
    let (app, store) = test_app(None);
    let thread = create_thread(&app, "pipeline").await;

    let (status, v) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/runs/wait"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "run/wait failed: {v}");
    assert_eq!(v["status"], json!("success"));
    assert_eq!(v["output"]["log"], json!(["first", "second"]));

    // GET state reflects the final checkpoint.
    let (status, v) = call(&app, "GET", &format!("/threads/{thread}/state"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["values"]["log"], json!(["first", "second"]));
    assert_eq!(v["next"], json!([]));
    assert!(v["checkpoint"]["checkpoint_id"].as_str().is_some());

    // History is newest-first: step 1 then step 0.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/history"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = v.as_array().unwrap();
    assert_eq!(items.len(), 2, "expected one checkpoint per super-step");
    assert_eq!(items[0]["checkpoint"]["step"], json!(1));
    assert_eq!(items[1]["checkpoint"]["step"], json!(0));
    assert_eq!(items[0]["values"]["log"], json!(["first", "second"]));
    assert_eq!(items[1]["values"]["log"], json!(["first"]));

    // limit + before filters.
    let first_id = items[0]["checkpoint"]["checkpoint_id"].as_str().unwrap();
    let (_, v) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/history"),
        Some(json!({"before": first_id})),
    )
    .await;
    assert_eq!(v.as_array().unwrap().len(), 1);
    let (_, v) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/history"),
        Some(json!({"limit": 1})),
    )
    .await;
    assert_eq!(v.as_array().unwrap().len(), 1);

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn sse_stream_emits_frames_in_order() {
    let (app, store) = test_app(None);
    let thread = create_thread(&app, "pipeline").await;

    let (status, bytes) = call_raw(
        &app,
        "POST",
        &format!("/threads/{thread}/runs/stream"),
        Some(json!({})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let body = String::from_utf8(bytes.to_vec()).unwrap();

    // Parse "event:/data:/id:" blocks.
    let mut frames: Vec<(String, Value, String)> = Vec::new();
    for block in body.split("\n\n") {
        let mut event = String::new();
        let mut data = String::new();
        let mut id = String::new();
        for line in block.lines() {
            if let Some(v) = line.strip_prefix("event:") {
                event = v.trim().to_string();
            } else if let Some(v) = line.strip_prefix("data:") {
                data = v.trim().to_string();
            } else if let Some(v) = line.strip_prefix("id:") {
                id = v.trim().to_string();
            }
        }
        if !event.is_empty() {
            frames.push((event, serde_json::from_str(&data).unwrap(), id));
        }
    }

    let events: Vec<&str> = frames.iter().map(|(e, _, _)| e.as_str()).collect();
    assert_eq!(
        events,
        ["metadata", "updates", "values", "updates", "values", "end"],
        "unexpected frame sequence; raw body:\n{body}"
    );

    // metadata carries run + thread identity.
    assert_eq!(frames[0].1["thread_id"], json!(thread));
    assert_eq!(frames[0].1["graph"], json!("pipeline"));
    assert!(frames[0].1["run_id"].as_str().is_some());

    // updates frames carry per-step POST-reducer values (core deliberately
    // changed `GraphEvent::StateUpdate.updates` to the merged state read-back:
    // the full appended list for an `Append` channel, not the raw per-node
    // partials).
    assert_eq!(frames[1].1["step"], json!(0));
    assert_eq!(frames[1].1["updates"]["log"], json!(["first"]));
    assert_eq!(frames[3].1["step"], json!(1));
    assert_eq!(frames[3].1["updates"]["log"], json!(["first", "second"]));

    // values frames carry the full state at each boundary.
    assert_eq!(frames[2].1["log"], json!(["first"]));
    assert_eq!(frames[4].1["log"], json!(["first", "second"]));

    // end frame reports success; frame ids are {checkpoint}:{step}:{seq}.
    assert_eq!(frames[5].1["status"], json!("success"));
    for (_, _, id) in &frames {
        let parts: Vec<&str> = id.split(':').collect();
        assert_eq!(parts.len(), 3, "bad frame id `{id}`");
        assert!(parts[2].parse::<u64>().is_ok(), "bad seq in id `{id}`");
    }

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn resume_after_interrupt_round_trip() {
    let (app, store) = test_app(None);
    let thread = create_thread(&app, "interrupt_gate").await;

    // First run suspends on the gate's interrupt.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/runs/wait"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["status"], json!("interrupted"));
    assert_eq!(v["interrupt"], json!({"question": "approve?"}));
    assert!(v["checkpoint_id"].as_str().is_some());

    // GET state shows the suspension point scheduling the gate node.
    let (_, v) = call(&app, "GET", &format!("/threads/{thread}/state"), None).await;
    assert_eq!(v["next"], json!(["gate"]));

    // Resume via command.resume: the run completes with the payload applied.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/runs/wait"),
        Some(json!({"command": {"resume": {"approved": true}}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "resume run failed: {v}");
    assert_eq!(v["status"], json!("success"));
    assert_eq!(v["output"]["answer"], json!({"approved": true}));

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn multitask_reject_returns_409() {
    let (app, store) = test_app(None);
    let thread = create_thread(&app, "slow").await;

    // Background run occupies the thread slot.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/runs"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "background run failed: {v}");
    assert_eq!(v["status"], json!("running"));
    assert!(v["run_id"].as_str().is_some());

    // A concurrent run with `reject` is refused.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/runs"),
        Some(json!({"multitask_strategy": "reject"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(v["error"], json!("conflict"));

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn multitask_enqueue_waits_for_its_turn() {
    let (app, store) = test_app(None);
    let thread = create_thread(&app, "slow").await;

    let (status, _) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/runs"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    // Default strategy is `enqueue`: this run queues behind the active one,
    // then runs and completes when the first run drains.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/runs/wait"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "enqueued run failed: {v}");
    assert_eq!(v["status"], json!("success"));
    assert_eq!(v["output"]["done"], json!(true));

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn update_state_writes_a_checkpoint() {
    let (app, store) = test_app(None);
    let thread = create_thread(&app, "pipeline").await;

    let (status, v) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/state"),
        Some(json!({"values": {"log": ["manual"]}, "as_node": "first"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "update_state failed: {v}");
    assert_eq!(v["checkpoint"]["step"], json!(0));

    let (_, v) = call(&app, "GET", &format!("/threads/{thread}/state"), None).await;
    assert_eq!(v["values"]["log"], json!(["manual"]));

    // Non-object values are rejected.
    let (status, _) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/state"),
        Some(json!({"values": [1, 2, 3]})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn auth_rejects_missing_key_and_accepts_valid_key() {
    let (app, store) = test_app(Some("s3cret"));

    let (status, _) = call(&app, "GET", "/ok", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _) = call_raw(&app, "GET", "/ok", None, &[("x-api-key", "wrong")]).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, bytes) = call_raw(&app, "GET", "/ok", None, &[("x-api-key", "s3cret")]).await;
    assert_eq!(status, StatusCode::OK);
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["ok"], json!(true));

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn missing_thread_returns_404() {
    let (app, store) = test_app(None);
    let (status, v) = call(&app, "GET", "/threads/no-such-thread/state", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(v["error"], json!("not_found"));
    let _ = std::fs::remove_dir_all(store);
}
