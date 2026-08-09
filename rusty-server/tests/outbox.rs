//! Transactional-outbox + effect-receipt integration tests (R0.6 wave 2b):
//! `POST /tasks/outbox` and `update_state`'s atomic `enqueue` over the
//! default JSON-file backend — relay publishing, idempotency dedup across
//! the outbox boundary, checkpoint+enqueue visibility, and the complete
//! path's receipt validation, storage, and Flight Recorder journaling.
//!
//! Driven in-process via `tower::ServiceExt::oneshot` (no sockets). The
//! relay runs at a 50 ms poll interval so publishes land quickly while the
//! tests still observe the 202-accepted-then-published transition. Live
//! Postgres coverage (per-row publish transactions, checkpoint+enqueue
//! atomicity) is gated in `postgres_outbox.rs`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::prelude::*;
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

/// `first -> second`, appending to a `log` channel (the receipt journaling
/// test needs a real run with a persisted journal).
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
    std::env::temp_dir().join(format!("rusty-server-outbox-test-{}", uuid::Uuid::new_v4()))
}

/// Open-mode app over a fresh store, relay polling every 50 ms.
fn app() -> (Router, PathBuf) {
    let (pipeline, pipeline_spec) = pipeline_graph();
    let mut registry = GraphRegistry::new();
    registry.register("pipeline", pipeline, pipeline_spec);
    let store = temp_store();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_outbox_relay_interval(Duration::from_millis(50));
    (router(registry, config), store)
}

/// Send a request; returns `(status, json-body-or-null)`.
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

/// Poll `GET /tasks/{id}` until the relay has published the task (or fail
/// after 5 s — the relay polls every 50 ms, so this is generous).
async fn wait_published(app: &Router, task_id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let (status, v) = call(app, "GET", &format!("/tasks/{task_id}"), None).await;
        if status == StatusCode::OK {
            return v;
        }
        assert!(
            Instant::now() < deadline,
            "task `{task_id}` never published; last response {status}: {v}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// List every task carrying `idempotency_key`.
async fn tasks_with_key(app: &Router, key: &str) -> Vec<Value> {
    let (status, v) = call(app, "GET", "/tasks", None).await;
    assert_eq!(status, StatusCode::OK, "list failed: {v}");
    v.as_array()
        .unwrap()
        .iter()
        .filter(|t| t["idempotency_key"] == json!(key))
        .cloned()
        .collect()
}

/// Poll the task list until a task with `key` appears (relay-published).
async fn wait_key_published(app: &Router, key: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(task) = tasks_with_key(app, key).await.into_iter().next() {
            return task;
        }
        assert!(
            Instant::now() < deadline,
            "no task with key `{key}` ever published"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Enqueue through the outbox; asserts 202 and returns the body.
async fn outbox_enqueue(app: &Router, body: Value) -> Value {
    let (status, v) = call(app, "POST", "/tasks/outbox", Some(body)).await;
    assert_eq!(status, StatusCode::ACCEPTED, "outbox enqueue failed: {v}");
    v
}

// --------------------------------------------------------------------- //
// Relay publishing
// --------------------------------------------------------------------- //

#[tokio::test]
async fn outbox_enqueue_is_accepted_then_published_by_the_relay() {
    let (app, store) = app();
    let v = outbox_enqueue(
        &app,
        json!({"kind": "send_email", "payload": {"to": "a@b.c"}}),
    )
    .await;
    assert_eq!(v["deduplicated"], json!(false));
    let task_id = v["task_id"].as_str().unwrap().to_string();

    // The relay publishes the row within one poll interval; the published
    // record is the full queue record, claimable like any other task.
    let task = wait_published(&app, &task_id).await;
    assert_eq!(task["task_id"], json!(task_id));
    assert_eq!(task["kind"], json!("send_email"));
    assert_eq!(task["status"], json!("queued"));

    let (status, v) = call(
        &app,
        "POST",
        "/tasks/claim",
        Some(json!({"worker_id": "worker-1", "lease_ms": 30_000})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "claim failed: {v}");
    assert_eq!(v["task"]["task_id"], json!(task_id));
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn outbox_dedupes_on_idempotency_key_across_publishes() {
    let (app, store) = app();
    // Two independent submissions sharing an idempotency key (a retried
    // effect from two code paths) are two outbox rows — but the publish
    // dedupes on the key, so exactly one task ever exists.
    let first = outbox_enqueue(
        &app,
        json!({"kind": "charge", "payload": {"cents": 500}, "idempotency_key": "charge-77"}),
    )
    .await;
    let second = outbox_enqueue(
        &app,
        json!({"kind": "charge", "payload": {"cents": 500}, "idempotency_key": "charge-77"}),
    )
    .await;
    assert_ne!(
        first["task_id"], second["task_id"],
        "separate submissions get separate task ids; the dedupe happens at publish"
    );

    let surviving = wait_key_published(&app, "charge-77").await;
    // Wait for the second publish pass too, then assert the queue holds
    // exactly one task under the key — never a double charge.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let matches = tasks_with_key(&app, "charge-77").await;
    assert_eq!(
        matches.len(),
        1,
        "publish dedupes on the idempotency key: {matches:?}"
    );
    assert_eq!(matches[0]["task_id"], surviving["task_id"]);

    // A direct enqueue under the same key dedupes against the published
    // task (the same mechanism as `POST /tasks`).
    let (status, v) = call(
        &app,
        "POST",
        "/tasks",
        Some(json!({"kind": "charge", "payload": {"cents": 500}, "idempotency_key": "charge-77"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "dedup enqueue failed: {v}");
    assert_eq!(v["deduplicated"], json!(true));
    assert_eq!(v["task_id"], surviving["task_id"]);
    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// update_state's atomic enqueue
// --------------------------------------------------------------------- //

#[tokio::test]
async fn update_state_enqueue_commits_checkpoint_and_outbox() {
    let (app, store) = app();
    let (status, v) = call(&app, "POST", "/threads", Some(json!({"graph": "pipeline"}))).await;
    assert_eq!(status, StatusCode::CREATED, "thread creation failed: {v}");
    let thread_id = v["thread_id"].as_str().unwrap().to_string();

    // One call: the state update and the effect submission commit as a
    // unit. The response returns after the durable outbox write; the task
    // becomes claimable when the relay publishes it.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/threads/{thread_id}/state"),
        Some(json!({
            "values": {"log": ["manual"]},
            "enqueue": [{
                "kind": "charge",
                "payload": {"cents": 900},
                "idempotency_key": "state-charge-1",
                "thread_id": thread_id,
            }],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "update_state failed: {v}");
    assert_eq!(v["values"]["log"], json!(["manual"]));

    // The checkpoint landed.
    let (status, v) = call(&app, "GET", &format!("/threads/{thread_id}/state"), None).await;
    assert_eq!(status, StatusCode::OK, "get state failed: {v}");
    assert_eq!(v["values"]["log"], json!(["manual"]));

    // And the task publishes through the relay, carrying the linkage the
    // submission declared.
    let task = wait_key_published(&app, "state-charge-1").await;
    assert_eq!(task["kind"], json!("charge"));
    assert_eq!(task["payload"], json!({"cents": 900}));
    assert_eq!(task["thread_id"], json!(thread_id));
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn update_state_enqueue_validates_before_writing_anything() {
    let (app, store) = app();
    let (status, v) = call(&app, "POST", "/threads", Some(json!({"graph": "pipeline"}))).await;
    assert_eq!(status, StatusCode::CREATED, "thread creation failed: {v}");
    let thread_id = v["thread_id"].as_str().unwrap().to_string();

    // An invalid task entry (empty kind) fails the whole request: no
    // checkpoint, no outbox row — the all-or-nothing contract holds on the
    // validation side even before the backend's transaction does.
    let (status, _) = call(
        &app,
        "POST",
        &format!("/threads/{thread_id}/state"),
        Some(json!({
            "values": {"log": ["should never land"]},
            "enqueue": [{"kind": "", "payload": {}}],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, v) = call(&app, "GET", &format!("/threads/{thread_id}/state"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        v["checkpoint"],
        Value::Null,
        "a rejected enqueue must not leave its checkpoint behind: {v}"
    );
    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Effect receipts
// --------------------------------------------------------------------- //

/// Enqueue + claim a task directly; returns `(task_id, wire-task)`.
async fn enqueue_and_claim(app: &Router, extra: Value) -> String {
    let mut body = json!({"kind": "charge", "payload": {"cents": 500}});
    body.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    let (status, v) = call(app, "POST", "/tasks", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "enqueue failed: {v}");
    let task_id = v["task_id"].as_str().unwrap().to_string();
    let (status, v) = call(
        app,
        "POST",
        "/tasks/claim",
        Some(json!({"worker_id": "worker-1", "lease_ms": 30_000})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "claim failed: {v}");
    assert_eq!(v["task"]["task_id"], json!(task_id));
    task_id
}

#[tokio::test]
async fn complete_with_receipt_stores_and_returns_it() {
    let (app, store) = app();
    let task_id = enqueue_and_claim(&app, json!({"idempotency_key": "charge-88"})).await;

    let (status, v) = call(
        &app,
        "POST",
        &format!("/tasks/{task_id}/complete"),
        Some(json!({
            "worker_id": "worker-1",
            "result": {"charged": true},
            "receipt": {
                "provider": "stripe",
                "provider_id": "ch_3PKdY2eZvKYlo2C0",
                "idempotency_key": "charge-88",
                "task_id": task_id,
            },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "complete failed: {v}");
    assert_eq!(v["status"], json!("completed"));
    assert_eq!(v["receipt"]["provider"], json!("stripe"));
    assert_eq!(v["receipt"]["provider_id"], json!("ch_3PKdY2eZvKYlo2C0"));
    assert_eq!(v["receipt"]["idempotency_key"], json!("charge-88"));

    // The receipt is durable on the record.
    let (status, v) = call(&app, "GET", &format!("/tasks/{task_id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["receipt"]["provider_id"], json!("ch_3PKdY2eZvKYlo2C0"));
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn complete_with_a_mismatched_receipt_key_is_rejected() {
    let (app, store) = app();
    let task_id = enqueue_and_claim(&app, json!({"idempotency_key": "charge-89"})).await;

    // The receipt claims to confirm a different effect than the task's —
    // evidence of a wiring bug, answered 400; the task is not settled.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/tasks/{task_id}/complete"),
        Some(json!({
            "worker_id": "worker-1",
            "result": {"charged": true},
            "receipt": {
                "provider": "stripe",
                "provider_id": "ch_other",
                "idempotency_key": "charge-DIFFERENT",
            },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "expected 400: {v}");

    let (status, v) = call(&app, "GET", &format!("/tasks/{task_id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["status"], json!("leased"), "not settled: {v}");
    assert_eq!(v["receipt"], Value::Null);
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn effect_receipt_is_journaled_into_the_tasks_run() {
    let (app, store) = app();
    let (status, v) = call(&app, "POST", "/threads", Some(json!({"graph": "pipeline"}))).await;
    assert_eq!(status, StatusCode::CREATED, "thread creation failed: {v}");
    let thread_id = v["thread_id"].as_str().unwrap().to_string();

    // A finished run with a persisted journal.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/threads/{thread_id}/runs/wait"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "run/wait failed: {v}");
    let run_id = v["run_id"].as_str().unwrap().to_string();

    let (status, v) = call(&app, "GET", &format!("/runs/{run_id}/events"), None).await;
    assert_eq!(status, StatusCode::OK, "GET events failed: {v}");
    let before = v["events"].as_array().unwrap().clone();
    assert!(!before.is_empty());
    let head_id = before.last().unwrap()["id"].as_str().unwrap().to_string();
    let head_seq = before.last().unwrap()["seq"].as_u64().unwrap();

    // The effect task, linked to the finished run.
    let task_id = enqueue_and_claim(
        &app,
        json!({
            "idempotency_key": "charge-90",
            "run_id": run_id,
            "thread_id": thread_id,
        }),
    )
    .await;
    let (status, v) = call(
        &app,
        "POST",
        &format!("/tasks/{task_id}/complete"),
        Some(json!({
            "worker_id": "worker-1",
            "result": {"charged": true},
            "receipt": {
                "provider": "stripe",
                "provider_id": "ch_3PKdY2eZvKYlo2C0",
                "idempotency_key": "charge-90",
                "task_id": task_id,
            },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "complete failed: {v}");

    // The receipt was appended to the run's journal: an `effect_receipt`
    // event whose causal parent is the previous journal head, continuing
    // the total order.
    let (status, v) = call(&app, "GET", &format!("/runs/{run_id}/events"), None).await;
    assert_eq!(status, StatusCode::OK, "GET events failed: {v}");
    let after = v["events"].as_array().unwrap();
    assert_eq!(after.len(), before.len() + 1, "one event appended: {v}");
    let receipt_event = after.last().unwrap();
    assert_eq!(receipt_event["kind"], json!("effect_receipt"));
    assert_eq!(receipt_event["effect"], json!("idempotent"));
    assert_eq!(receipt_event["seq"], json!(head_seq + 1));
    assert_eq!(
        receipt_event["id"],
        json!(format!("{run_id}:{}", head_seq + 1))
    );
    assert_eq!(
        receipt_event["parent"],
        json!(head_id),
        "the receipt's causal parent is the previous journal head"
    );
    assert_eq!(receipt_event["output"]["kind"], json!("inline"));
    let output = &receipt_event["output"]["value"];
    assert_eq!(output["provider"], json!("stripe"));
    assert_eq!(output["provider_id"], json!("ch_3PKdY2eZvKYlo2C0"));
    assert_eq!(output["idempotency_key"], json!("charge-90"));
    assert_eq!(output["task_id"], json!(task_id));
    let _ = std::fs::remove_dir_all(store);
}
