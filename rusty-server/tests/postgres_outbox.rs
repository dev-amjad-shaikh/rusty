//! Live-Postgres integration tests for the transactional outbox + effect
//! receipts (R0.6 wave 2b): the `server_outbox` table over HTTP — relay
//! publishing, publish-time idempotency dedup, `update_state`'s atomic
//! checkpoint+enqueue, and the receipt's JSONB round trip.
//!
//! Gated two ways — none of this runs in the default test suite:
//!
//! 1. compile-time: the whole file is `cfg(feature = "postgres")`;
//! 2. run-time: every test is `#[ignore]` and requires `DATABASE_URL`.
//!
//! Run them with:
//!
//! ```bash
//! DATABASE_URL=postgres://user:pass@localhost/rusty_test \
//!   cargo test --features postgres --test postgres_outbox -- --ignored
//! ```

#![cfg(feature = "postgres")]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::prelude::*;
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

/// The database these tests run against; panics with guidance when unset.
fn database_url() -> String {
    std::env::var("DATABASE_URL").expect(
        "DATABASE_URL must point at a scratch Postgres database \
         (e.g. postgres://user:pass@localhost/rusty_test)",
    )
}

/// `first -> second`, appending to a `log` channel (thread creation needs
/// a registered graph).
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

/// A Postgres-backed app with the relay polling every 50 ms.
fn postgres_app() -> Router {
    let (pipeline, pipeline_spec) = pipeline_graph();
    let mut registry = GraphRegistry::new();
    registry.register("pipeline", pipeline, pipeline_spec);
    let store_path: PathBuf =
        std::env::temp_dir().join(format!("rusty-server-pg-outbox-{}", uuid::Uuid::new_v4()));
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store_path)
        .with_postgres(database_url())
        .with_outbox_relay_interval(Duration::from_millis(50));
    router(registry, config)
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

/// Unique fragment so repeated runs against a shared scratch database
/// never collide.
fn uniq() -> String {
    uuid::Uuid::new_v4().simple().to_string()
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
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(task) = tasks_with_key(app, key).await.into_iter().next() {
            return task;
        }
        assert!(
            Instant::now() < deadline,
            "no task with key `{key}` ever published"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
#[ignore = "requires a live Postgres (DATABASE_URL)"]
async fn postgres_outbox_relay_publishes_and_dedupes_on_the_key() {
    let app = postgres_app();
    let key = format!("charge-{}", uniq());
    // A unique pool keeps the published tasks from ever being claimed by
    // another test — suites here share the scratch database.
    let pool = format!("outbox-{}", uniq());

    // Two submissions under one idempotency key: two outbox rows (distinct
    // task ids), but the per-row publish transactions dedupe on the key's
    // unique index — exactly one task ever exists.
    for _ in 0..2 {
        let (status, v) = call(
            &app,
            "POST",
            "/tasks/outbox",
            Some(json!({"kind": "charge", "payload": {"cents": 500}, "idempotency_key": key, "pool": pool})),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "outbox enqueue failed: {v}");
        assert_eq!(v["deduplicated"], json!(false));
    }

    let surviving = wait_key_published(&app, &key).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let matches = tasks_with_key(&app, &key).await;
    assert_eq!(
        matches.len(),
        1,
        "publish dedupes on the idempotency key: {matches:?}"
    );
    assert_eq!(matches[0]["task_id"], surviving["task_id"]);

    // A direct enqueue under the key dedupes against the published task.
    let (status, v) = call(
        &app,
        "POST",
        "/tasks",
        Some(json!({"kind": "charge", "payload": {"cents": 500}, "idempotency_key": key, "pool": pool})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "dedup enqueue failed: {v}");
    assert_eq!(v["deduplicated"], json!(true));
    assert_eq!(v["task_id"], surviving["task_id"]);
}

#[tokio::test]
#[ignore = "requires a live Postgres (DATABASE_URL)"]
async fn postgres_update_state_enqueue_commits_checkpoint_and_outbox() {
    let app = postgres_app();
    let (status, v) = call(&app, "POST", "/threads", Some(json!({"graph": "pipeline"}))).await;
    assert_eq!(status, StatusCode::CREATED, "thread creation failed: {v}");
    let thread_id = v["thread_id"].as_str().unwrap().to_string();
    let key = format!("state-charge-{}", uniq());
    // Unique pool, same shared-database isolation as the other tests.
    let pool = format!("outbox-state-{}", uniq());

    // One transaction: the checkpoint write and the outbox insert commit
    // together (the atomicity the file backend cannot offer).
    let (status, v) = call(
        &app,
        "POST",
        &format!("/threads/{thread_id}/state"),
        Some(json!({
            "values": {"log": ["manual"]},
            "enqueue": [{
                "kind": "charge",
                "payload": {"cents": 900},
                "idempotency_key": key,
                "thread_id": thread_id,
                "pool": pool,
            }],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "update_state failed: {v}");
    assert_eq!(v["values"]["log"], json!(["manual"]));

    let (status, v) = call(&app, "GET", &format!("/threads/{thread_id}/state"), None).await;
    assert_eq!(status, StatusCode::OK, "get state failed: {v}");
    assert_eq!(v["values"]["log"], json!(["manual"]));

    let task = wait_key_published(&app, &key).await;
    assert_eq!(task["kind"], json!("charge"));
    assert_eq!(task["thread_id"], json!(thread_id));
}

#[tokio::test]
#[ignore = "requires a live Postgres (DATABASE_URL)"]
async fn postgres_complete_with_receipt_round_trips_through_jsonb() {
    let app = postgres_app();
    let key = format!("charge-{}", uniq());
    // A unique pool keeps the claim from stealing another run's leftover
    // tasks — tests here share the scratch database.
    let pool = format!("receipt-{}", uniq());

    let (status, v) = call(
        &app,
        "POST",
        "/tasks",
        Some(json!({"kind": "charge", "payload": {"cents": 500}, "idempotency_key": key, "pool": pool})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "enqueue failed: {v}");
    let task_id = v["task_id"].as_str().unwrap().to_string();

    let (status, v) = call(
        &app,
        "POST",
        "/tasks/claim",
        Some(json!({"worker_id": "worker-1", "pools": [pool], "lease_ms": 30_000})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "claim failed");
    assert_eq!(v["task"]["task_id"], json!(task_id));

    // A receipt under a different key is a wiring bug, answered 400.
    let (status, _) = call(
        &app,
        "POST",
        &format!("/tasks/{task_id}/complete"),
        Some(json!({
            "worker_id": "worker-1",
            "result": {"charged": true},
            "receipt": {"provider": "stripe", "provider_id": "ch_x", "idempotency_key": "nope"},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // The matching receipt settles the task and persists through the
    // additive `receipt JSONB` column.
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
                "idempotency_key": key,
                "task_id": task_id,
            },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "complete failed: {v}");
    assert_eq!(v["receipt"]["provider_id"], json!("ch_3PKdY2eZvKYlo2C0"));

    let (status, v) = call(&app, "GET", &format!("/tasks/{task_id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["receipt"]["provider"], json!("stripe"));
    assert_eq!(v["receipt"]["idempotency_key"], json!(key));
}
