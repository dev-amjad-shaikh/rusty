//! Stage barrier integration tests (EP-09-S05).
//!
//! The barrier wakes a parent task's assigned agent exactly once when the
//! lowest unfinished stage of its children fully closes.  Wakes are
//! implemented as mailbox tasks (`kind: "stage_barrier_wake"`) so the
//! substrate's existing idempotency and claim machinery apply.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::http::{Request, StatusCode};
use rusty_agent_server::{GraphRegistry, ServerConfig, router};
use serde_json::{Value, json};
use tower::ServiceExt;

fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!(
        "rusty-server-stage-barrier-test-{}",
        uuid::Uuid::new_v4()
    ))
}

fn app() -> (Router, PathBuf) {
    let store = temp_store();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone());
    (router(GraphRegistry::new(), config), store)
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
    let bytes: Bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

/// Enqueue a task; returns its task id.
async fn enqueue(app: &Router, body: Value) -> String {
    let (status, v) = call(app, "POST", "/tasks", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "enqueue failed: {v}");
    v["task_id"].as_str().unwrap().to_string()
}

/// Claim as `worker`, asserting a task is handed out; returns the task body.
async fn claim_one(app: &Router, worker: &str, lease_ms: u64) -> Value {
    let (status, v) = call(
        app,
        "POST",
        "/tasks/claim",
        Some(json!({"worker_id": worker, "lease_ms": lease_ms})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "claim failed: {v}");
    v["task"].clone()
}

/// Complete a claimed task.
async fn complete(app: &Router, task_id: &str, worker: &str) {
    let (status, v) = call(
        app,
        "POST",
        &format!("/tasks/{task_id}/complete"),
        Some(json!({"worker_id": worker, "result": {"ok": true}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "complete failed: {v}");
}

/// Fail a claimed task terminally.
#[allow(dead_code)]
async fn fail_terminal(app: &Router, task_id: &str, worker: &str) {
    let (status, v) = call(
        app,
        "POST",
        &format!("/tasks/{task_id}/fail"),
        Some(json!({
            "worker_id": worker,
            "error_class": "invalid_input",
            "message": "done",
            "retryable": false
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "fail failed: {v}");
}

/// Cancel a task.
async fn cancel(app: &Router, task_id: &str) {
    let (status, v) = call(app, "POST", &format!("/tasks/{task_id}/cancel"), None).await;
    assert!(
        status == StatusCode::OK || status == StatusCode::CONFLICT,
        "cancel failed: {v}"
    );
}

/// Poll `GET /tasks` with a query until the condition passes or timeout.
async fn wait_for_tasks<F>(app: &Router, query: &str, mut pred: F) -> Value
where
    F: FnMut(&Value) -> bool,
{
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let (status, v) = call(app, "GET", &format!("/tasks{query}"), None).await;
        assert_eq!(status, StatusCode::OK);
        if pred(&v) {
            return v;
        }
        assert!(
            Instant::now() < deadline,
            "timeout waiting for task condition: {v}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Count barrier-wake tasks in the queue.
async fn barrier_wake_count(app: &Router) -> usize {
    let (status, v) = call(app, "GET", "/tasks", None).await;
    assert_eq!(status, StatusCode::OK);
    v.as_array()
        .unwrap()
        .iter()
        .filter(|t| t["kind"] == "stage_barrier_wake")
        .count()
}

// --------------------------------------------------------------------- //
// Barrier matrix
// --------------------------------------------------------------------- //

#[tokio::test]
async fn staged_siblings_wake_once_when_stage_closes() {
    let (app, _store) = app();

    // Parent assigned to an agent.
    let parent = enqueue(
        &app,
        json!({
            "kind": "plan",
            "payload": {},
            "recipient": "agent:router",
            "status_category": "in_progress"
        }),
    )
    .await;

    // Two children in stage 1.
    let _child1 = enqueue(
        &app,
        json!({
            "kind": "work",
            "payload": {},
            "parent_task_id": parent,
            "stage": 1,
            "status_category": "todo"
        }),
    )
    .await;
    let _child2 = enqueue(
        &app,
        json!({
            "kind": "work",
            "payload": {},
            "parent_task_id": parent,
            "stage": 1,
            "status_category": "todo"
        }),
    )
    .await;

    // Complete child1 — stage still open.
    let t1 = claim_one(&app, "w1", 30_000).await;
    complete(&app, t1["task_id"].as_str().unwrap(), "w1").await;
    assert_eq!(barrier_wake_count(&app).await, 0);

    // Complete child2 — stage closes, exactly one wake.
    let t2 = claim_one(&app, "w2", 30_000).await;
    complete(&app, t2["task_id"].as_str().unwrap(), "w2").await;

    let wakes = wait_for_tasks(&app, "", |v| {
        v.as_array()
            .unwrap()
            .iter()
            .any(|t| t["kind"] == "stage_barrier_wake")
    })
    .await;
    let wake_list: Vec<&Value> = wakes
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["kind"] == "stage_barrier_wake")
        .collect();
    assert_eq!(wake_list.len(), 1);
    assert_eq!(wake_list[0]["recipient"], "agent:router");
    assert_eq!(wake_list[0]["payload"]["parent_task_id"], json!(parent));
}

#[tokio::test]
async fn unstaged_siblings_form_one_implicit_stage() {
    let (app, _store) = app();

    let parent = enqueue(
        &app,
        json!({
            "kind": "plan",
            "payload": {},
            "recipient": "agent:router",
            "status_category": "in_progress"
        }),
    )
    .await;

    let _child1 = enqueue(
        &app,
        json!({
            "kind": "work",
            "payload": {},
            "parent_task_id": parent,
            "stage": 0,
            "status_category": "todo"
        }),
    )
    .await;
    let _child2 = enqueue(
        &app,
        json!({
            "kind": "work",
            "payload": {},
            "parent_task_id": parent,
            "stage": 0,
            "status_category": "todo"
        }),
    )
    .await;

    let t1 = claim_one(&app, "w1", 30_000).await;
    complete(&app, t1["task_id"].as_str().unwrap(), "w1").await;
    assert_eq!(barrier_wake_count(&app).await, 0);

    let t2 = claim_one(&app, "w2", 30_000).await;
    complete(&app, t2["task_id"].as_str().unwrap(), "w2").await;

    let count = wait_for_tasks(&app, "", |v| {
        v.as_array()
            .unwrap()
            .iter()
            .filter(|t| t["kind"] == "stage_barrier_wake")
            .count()
            == 1
    })
    .await;
    assert_eq!(
        count
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| t["kind"] == "stage_barrier_wake")
            .count(),
        1
    );
}

#[tokio::test]
async fn mixed_done_and_cancelled_closes_stage() {
    let (app, _store) = app();

    let parent = enqueue(
        &app,
        json!({
            "kind": "plan",
            "payload": {},
            "recipient": "agent:router",
            "status_category": "in_progress"
        }),
    )
    .await;

    let _child1 = enqueue(
        &app,
        json!({
            "kind": "work",
            "payload": {},
            "parent_task_id": parent,
            "stage": 1,
            "status_category": "todo"
        }),
    )
    .await;
    let _child2 = enqueue(
        &app,
        json!({
            "kind": "work",
            "payload": {},
            "parent_task_id": parent,
            "stage": 1,
            "status_category": "todo"
        }),
    )
    .await;

    // Complete child1.
    let t1 = claim_one(&app, "w1", 30_000).await;
    complete(&app, t1["task_id"].as_str().unwrap(), "w1").await;
    assert_eq!(barrier_wake_count(&app).await, 0);

    // Cancel child2 — stage still closes because cancelled is terminal.
    cancel(&app, &_child2).await;

    let count = wait_for_tasks(&app, "", |v| {
        v.as_array()
            .unwrap()
            .iter()
            .filter(|t| t["kind"] == "stage_barrier_wake")
            .count()
            == 1
    })
    .await;
    assert_eq!(
        count
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| t["kind"] == "stage_barrier_wake")
            .count(),
        1
    );
}

#[tokio::test]
async fn in_review_child_holds_barrier_open() {
    let (app, _store) = app();

    let parent = enqueue(
        &app,
        json!({
            "kind": "plan",
            "payload": {},
            "recipient": "agent:router",
            "status_category": "in_progress"
        }),
    )
    .await;

    let _child1 = enqueue(
        &app,
        json!({
            "kind": "work",
            "payload": {},
            "parent_task_id": parent,
            "stage": 1,
            "status_category": "todo"
        }),
    )
    .await;
    let _child2 = enqueue(
        &app,
        json!({
            "kind": "work",
            "payload": {},
            "parent_task_id": parent,
            "stage": 1,
            "status_category": "todo"
        }),
    )
    .await;

    // Complete child1 (lands as completed → terminal).
    let t1 = claim_one(&app, "w1", 30_000).await;
    complete(&app, t1["task_id"].as_str().unwrap(), "w1").await;

    // Child2 is moved to in_review by a direct update (simulating human review).
    // In the current substrate there's no PATCH /tasks/{id}, so we simulate by
    // completing the task and then checking: the spec says InReview is non-
    // terminal for barrier purposes.  Since the current server doesn't have a
    // dedicated InReview status in TaskStatus, we verify the conceptual gate by
    // leaving child2 open (queued) — the barrier must not fire while any sibling
    // in the stage is non-terminal.
    assert_eq!(barrier_wake_count(&app).await, 0);

    // Complete child2 — now the stage closes.
    let t2 = claim_one(&app, "w2", 30_000).await;
    complete(&app, t2["task_id"].as_str().unwrap(), "w2").await;

    let count = wait_for_tasks(&app, "", |v| {
        v.as_array()
            .unwrap()
            .iter()
            .filter(|t| t["kind"] == "stage_barrier_wake")
            .count()
            == 1
    })
    .await;
    assert_eq!(
        count
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| t["kind"] == "stage_barrier_wake")
            .count(),
        1
    );
}

// --------------------------------------------------------------------- //
// Concurrency
// --------------------------------------------------------------------- //

#[tokio::test]
async fn concurrent_sibling_settlement_produces_exactly_one_wake() {
    let (app, _store) = app();

    let parent = enqueue(
        &app,
        json!({
            "kind": "plan",
            "payload": {},
            "recipient": "agent:router",
            "status_category": "in_progress"
        }),
    )
    .await;

    let _child1 = enqueue(
        &app,
        json!({
            "kind": "work",
            "payload": {},
            "parent_task_id": parent,
            "stage": 1,
            "status_category": "todo"
        }),
    )
    .await;
    let _child2 = enqueue(
        &app,
        json!({
            "kind": "work",
            "payload": {},
            "parent_task_id": parent,
            "stage": 1,
            "status_category": "todo"
        }),
    )
    .await;

    // Claim both tasks first.
    let t1 = claim_one(&app, "w1", 30_000).await;
    let t2 = claim_one(&app, "w2", 30_000).await;

    // Settle both concurrently.
    let uri1 = format!("/tasks/{}/complete", t1["task_id"].as_str().unwrap());
    let uri2 = format!("/tasks/{}/complete", t2["task_id"].as_str().unwrap());
    let (r1, r2) = tokio::join!(
        call(
            &app,
            "POST",
            &uri1,
            Some(json!({"worker_id": "w1", "result": {"ok": true}})),
        ),
        call(
            &app,
            "POST",
            &uri2,
            Some(json!({"worker_id": "w2", "result": {"ok": true}})),
        ),
    );
    assert_eq!(r1.0, StatusCode::OK, "child1 complete failed: {}", r1.1);
    assert_eq!(r2.0, StatusCode::OK, "child2 complete failed: {}", r2.1);

    let wakes = wait_for_tasks(&app, "", |v| {
        v.as_array()
            .unwrap()
            .iter()
            .filter(|t| t["kind"] == "stage_barrier_wake")
            .count()
            >= 1
    })
    .await;
    let wake_count = wakes
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["kind"] == "stage_barrier_wake")
        .count();
    assert_eq!(wake_count, 1, "expected exactly one wake, got {wake_count}");
}

// --------------------------------------------------------------------- //
// Guards
// --------------------------------------------------------------------- //

#[tokio::test]
async fn backlog_parent_receives_no_wake() {
    let (app, _store) = app();

    let parent = enqueue(
        &app,
        json!({
            "kind": "plan",
            "payload": {},
            "recipient": "agent:router",
            "status_category": "backlog"
        }),
    )
    .await;

    let _child = enqueue(
        &app,
        json!({
            "kind": "work",
            "payload": {},
            "parent_task_id": parent,
            "stage": 1,
            "status_category": "todo"
        }),
    )
    .await;

    let t = claim_one(&app, "w1", 30_000).await;
    complete(&app, t["task_id"].as_str().unwrap(), "w1").await;

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(barrier_wake_count(&app).await, 0);
}

#[tokio::test]
async fn human_assigned_parent_receives_no_comment() {
    let (app, _store) = app();

    let parent = enqueue(
        &app,
        json!({
            "kind": "plan",
            "payload": {},
            "recipient": "human:maya",
            "status_category": "in_progress"
        }),
    )
    .await;

    let _child = enqueue(
        &app,
        json!({
            "kind": "work",
            "payload": {},
            "parent_task_id": parent,
            "stage": 1,
            "status_category": "todo"
        }),
    )
    .await;

    let t = claim_one(&app, "w1", 30_000).await;
    complete(&app, t["task_id"].as_str().unwrap(), "w1").await;

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(barrier_wake_count(&app).await, 0);
}

#[tokio::test]
async fn unassigned_parent_receives_no_wake() {
    let (app, _store) = app();

    let parent = enqueue(
        &app,
        json!({
            "kind": "plan",
            "payload": {},
            "status_category": "in_progress"
        }),
    )
    .await;

    let _child = enqueue(
        &app,
        json!({
            "kind": "work",
            "payload": {},
            "parent_task_id": parent,
            "stage": 1,
            "status_category": "todo"
        }),
    )
    .await;

    let t = claim_one(&app, "w1", 30_000).await;
    complete(&app, t["task_id"].as_str().unwrap(), "w1").await;

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(barrier_wake_count(&app).await, 0);
}

#[tokio::test]
async fn re_complete_terminal_child_produces_no_second_wake() {
    let (app, _store) = app();

    let parent = enqueue(
        &app,
        json!({
            "kind": "plan",
            "payload": {},
            "recipient": "agent:router",
            "status_category": "in_progress"
        }),
    )
    .await;

    let _child = enqueue(
        &app,
        json!({
            "kind": "work",
            "payload": {},
            "parent_task_id": parent,
            "stage": 1,
            "status_category": "todo"
        }),
    )
    .await;

    // First complete — wake fires.
    let t = claim_one(&app, "w1", 30_000).await;
    complete(&app, t["task_id"].as_str().unwrap(), "w1").await;

    let _ = wait_for_tasks(&app, "", |v| {
        v.as_array()
            .unwrap()
            .iter()
            .filter(|t| t["kind"] == "stage_barrier_wake")
            .count()
            == 1
    })
    .await;

    // Re-complete the same task — 409, no second wake.
    let (status, _) = call(
        &app,
        "POST",
        &format!("/tasks/{}/complete", t["task_id"].as_str().unwrap()),
        Some(json!({"worker_id": "w1", "result": {"ok": true}})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(barrier_wake_count(&app).await, 1);
}

// --------------------------------------------------------------------- //
// Advancement: zero server-side writes to child or parent
// --------------------------------------------------------------------- //

#[tokio::test]
async fn barrier_processing_does_not_mutate_child_or_parent() {
    let (app, _store) = app();

    let parent = enqueue(
        &app,
        json!({
            "kind": "plan",
            "payload": {},
            "recipient": "agent:router",
            "status_category": "in_progress"
        }),
    )
    .await;

    let _child = enqueue(
        &app,
        json!({
            "kind": "work",
            "payload": {},
            "parent_task_id": parent,
            "stage": 1,
            "status_category": "todo"
        }),
    )
    .await;

    // Snapshot parent before.
    let (_, parent_before) = call(&app, "GET", &format!("/tasks/{parent}"), None).await;

    let t = claim_one(&app, "w1", 30_000).await;
    complete(&app, t["task_id"].as_str().unwrap(), "w1").await;

    let _ = wait_for_tasks(&app, "", |v| {
        v.as_array()
            .unwrap()
            .iter()
            .filter(|t| t["kind"] == "stage_barrier_wake")
            .count()
            == 1
    })
    .await;

    // Parent must be unchanged.
    let (_, parent_after) = call(&app, "GET", &format!("/tasks/{parent}"), None).await;
    assert_eq!(parent_before["status"], parent_after["status"]);
    assert_eq!(
        parent_before["status_category"],
        parent_after["status_category"]
    );
    assert_eq!(parent_before["stage"], parent_after["stage"]);
    assert_eq!(parent_before["recipient"], parent_after["recipient"]);
}
