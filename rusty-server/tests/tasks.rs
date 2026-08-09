//! Durable task queue integration tests (R0.6): the `/tasks` HTTP surface
//! over the default JSON-file backend — full lifecycle, idempotency dedup,
//! lease-expiry reclaim, classified retry / dead-letter, tenant isolation,
//! 409-on-lost-lease, validation, and restart durability.
//!
//! Driven in-process via `tower::ServiceExt::oneshot` (no sockets). The
//! task surface never touches the graph registry, so the registry stays
//! empty here. Live-Postgres coverage of the same semantics (plus
//! SKIP LOCKED concurrency) is gated in `postgres_tasks.rs`.

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

/// Unique temp store root, removed at the end of each test (best effort).
fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!("rusty-server-tasks-test-{}", uuid::Uuid::new_v4()))
}

/// Open-mode (single `default` tenant) app over a fresh store.
fn app() -> (Router, PathBuf) {
    let store = temp_store();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone());
    (router(GraphRegistry::new(), config), store)
}

/// Two-tenant app for the isolation tests.
fn multi_tenant_app() -> (Router, PathBuf) {
    let store = temp_store();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_tenant_key("acme", "acme-secret")
        .with_tenant_key("globex", "globex-secret");
    (router(GraphRegistry::new(), config), store)
}

/// Send a request; returns `(status, json-body-or-null)`.
async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    call_as(app, None, method, uri, body).await
}

/// Send a request with an optional auth header.
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

/// Enqueue a minimal task; returns its task id.
async fn enqueue(app: &Router, extra: Value) -> String {
    let mut body = json!({"kind": "send_email", "payload": {"to": "a@b.c"}});
    body.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    let (status, v) = call(app, "POST", "/tasks", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "enqueue failed: {v}");
    assert_eq!(v["deduplicated"], json!(false));
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

// --------------------------------------------------------------------- //
// Lifecycle
// --------------------------------------------------------------------- //

#[tokio::test]
async fn full_lifecycle_enqueue_claim_heartbeat_complete() {
    let (app, store) = app();
    let task_id = enqueue(&app, json!({})).await;

    // Claim: attempt 1, leased to the worker, full record on the wire.
    let task = claim_one(&app, "worker-1", 30_000).await;
    assert_eq!(task["task_id"], json!(task_id));
    assert_eq!(task["kind"], json!("send_email"));
    assert_eq!(task["payload"], json!({"to": "a@b.c"}));
    assert_eq!(task["pool"], json!("default"));
    assert_eq!(task["status"], json!("leased"));
    assert_eq!(task["attempt"], json!(1));
    assert_eq!(task["max_attempts"], json!(3));
    assert_eq!(task["lease"]["owner"], json!("worker-1"));
    assert!(task["lease"]["expires_at"].is_string());
    assert!(task.get("tenant").is_none(), "tenant is internal");
    assert_eq!(task["result"], Value::Null);
    assert_eq!(task["run_id"], Value::Null);
    assert_eq!(task["thread_id"], Value::Null);
    assert!(task["created_at"].is_string() && task["updated_at"].is_string());

    // The lease hides the task from other claims.
    let (status, _) = call(
        &app,
        "POST",
        "/tasks/claim",
        Some(json!({"worker_id": "worker-2", "lease_ms": 30_000})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Heartbeat extends the lease.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/tasks/{task_id}/heartbeat"),
        Some(json!({"worker_id": "worker-1", "lease_ms": 60_000})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "heartbeat failed: {v}");
    assert!(v["lease_expires_at"].is_string());

    // Complete settles with a result.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/tasks/{task_id}/complete"),
        Some(json!({"worker_id": "worker-1", "result": {"sent": true}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "complete failed: {v}");
    assert_eq!(v["status"], json!("completed"));
    assert_eq!(v["result"], json!({"sent": true}));
    assert_eq!(v["lease"], Value::Null);

    // The record stays fetchable; nothing is claimable anymore.
    let (status, v) = call(&app, "GET", &format!("/tasks/{task_id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["status"], json!("completed"));
    let (status, _) = call(
        &app,
        "POST",
        "/tasks/claim",
        Some(json!({"worker_id": "worker-2", "lease_ms": 30_000})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn claim_on_an_empty_queue_is_204() {
    let (app, store) = app();
    let (status, v) = call(
        &app,
        "POST",
        "/tasks/claim",
        Some(json!({"worker_id": "w", "lease_ms": 30_000})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(v.is_null(), "204 carries no body");
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn pool_routing_claims_only_named_pools() {
    let (app, store) = app();
    enqueue(&app, json!({"pool": "gpu"})).await;

    // A default-pool claim does not see the gpu task.
    let (status, _) = call(
        &app,
        "POST",
        "/tasks/claim",
        Some(json!({"worker_id": "w", "lease_ms": 30_000})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // An explicit pools list reaches it.
    let (status, v) = call(
        &app,
        "POST",
        "/tasks/claim",
        Some(json!({"worker_id": "w", "pools": ["default", "gpu"], "lease_ms": 30_000})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["task"]["pool"], json!("gpu"));
    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Idempotency dedup
// --------------------------------------------------------------------- //

#[tokio::test]
async fn idempotency_key_dedups_enqueue() {
    let (app, store) = app();
    let first = enqueue(&app, json!({"idempotency_key": "charge-42"})).await;

    // Same key → 200 with the existing task id, flagged deduplicated.
    let (status, v) = call(
        &app,
        "POST",
        "/tasks",
        Some(json!({
            "kind": "send_email",
            "payload": {"to": "other@b.c"},
            "idempotency_key": "charge-42",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "dedup enqueue failed: {v}");
    assert_eq!(v["task_id"], json!(first));
    assert_eq!(v["deduplicated"], json!(true));

    // A different key creates; keyless tasks never dedup.
    let second = enqueue(&app, json!({"idempotency_key": "charge-43"})).await;
    assert_ne!(first, second);
    let third = enqueue(&app, json!({})).await;
    assert_ne!(first, third);

    // Exactly the two non-deduplicated keyed tasks plus the keyless one are
    // claimable — the deduped enqueue created nothing.
    for expected in [&first, &second, &third] {
        let task = claim_one(&app, "w", 30_000).await;
        assert_eq!(task["task_id"], json!(expected));
    }
    let (status, _) = call(
        &app,
        "POST",
        "/tasks/claim",
        Some(json!({"worker_id": "w", "lease_ms": 30_000})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Leases
// --------------------------------------------------------------------- //

#[tokio::test]
async fn expired_lease_is_reclaimed_and_lost_lease_is_409() {
    let (app, store) = app();
    let task_id = enqueue(&app, json!({})).await;

    // Worker A takes a very short lease and dies (no heartbeat).
    let task = claim_one(&app, "worker-a", 100).await;
    assert_eq!(task["lease"]["owner"], json!("worker-a"));

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    // Worker B reclaims the expired lease as a new attempt.
    let task = claim_one(&app, "worker-b", 30_000).await;
    assert_eq!(task["task_id"], json!(task_id));
    assert_eq!(task["attempt"], json!(2));
    assert_eq!(task["lease"]["owner"], json!("worker-b"));

    // Worker A's lost lease settles nothing: heartbeat, complete, and fail
    // all answer 409.
    for (suffix, body) in [
        (
            "heartbeat",
            json!({"worker_id": "worker-a", "lease_ms": 30_000}),
        ),
        ("complete", json!({"worker_id": "worker-a", "result": null})),
        (
            "fail",
            json!({"worker_id": "worker-a", "error_class": "unknown",
                   "message": "zombie", "retryable": true}),
        ),
    ] {
        let (status, v) = call(
            &app,
            "POST",
            &format!("/tasks/{task_id}/{suffix}"),
            Some(body),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{suffix} by lost lease: {v}");
    }

    // Worker B settles normally.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/tasks/{task_id}/complete"),
        Some(json!({"worker_id": "worker-b", "result": {"ok": true}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "complete by holder failed: {v}");
    assert_eq!(v["status"], json!("completed"));
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn settlement_by_a_non_holder_is_409_and_unknown_task_is_404() {
    let (app, store) = app();
    let task_id = enqueue(&app, json!({})).await;
    claim_one(&app, "worker-a", 30_000).await;

    // Another worker never held the lease.
    let (status, _) = call(
        &app,
        "POST",
        &format!("/tasks/{task_id}/complete"),
        Some(json!({"worker_id": "worker-b", "result": null})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Unknown ids answer 404 on every task endpoint.
    for (method, uri, body) in [
        ("GET", "/tasks/nope".to_string(), None),
        (
            "POST",
            "/tasks/nope/heartbeat".to_string(),
            Some(json!({"worker_id": "w", "lease_ms": 30_000})),
        ),
        (
            "POST",
            "/tasks/nope/complete".to_string(),
            Some(json!({"worker_id": "w", "result": null})),
        ),
        (
            "POST",
            "/tasks/nope/fail".to_string(),
            Some(json!({"worker_id": "w", "error_class": "unknown",
                        "message": "x", "retryable": true})),
        ),
    ] {
        let (status, v) = call(&app, method, &uri, body).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}: {v}");
    }

    // A completed task cannot be settled again — even by its last holder.
    let (status, _) = call(
        &app,
        "POST",
        &format!("/tasks/{task_id}/complete"),
        Some(json!({"worker_id": "worker-a", "result": {"ok": true}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, v) = call(
        &app,
        "POST",
        &format!("/tasks/{task_id}/heartbeat"),
        Some(json!({"worker_id": "worker-a", "lease_ms": 30_000})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "settled task heartbeat: {v}");
    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Failure classification, retry, dead-letter
// --------------------------------------------------------------------- //

#[tokio::test]
async fn retryable_failure_requeues_with_backoff_then_completes() {
    let (app, store) = app();
    let task_id = enqueue(&app, json!({})).await;
    claim_one(&app, "w", 30_000).await;

    // Attempt 1 fails transiently: requeued with a scheduled next attempt.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/tasks/{task_id}/fail"),
        Some(json!({"worker_id": "w", "error_class": "timeout",
                    "message": "upstream timed out", "retryable": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "fail failed: {v}");
    assert_eq!(v["requeued"], json!(true));
    assert_eq!(v["dead"], json!(false));
    assert!(v["next_attempt_at"].is_string());

    // The record shows the failure evidence.
    let (status, v) = call(&app, "GET", &format!("/tasks/{task_id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["status"], json!("failed"));
    assert_eq!(v["error_class"], json!("timeout"));
    assert_eq!(v["last_error"], json!("upstream timed out"));
    assert_eq!(v["lease"], Value::Null);

    // Backoff for attempt 1 tops out at one second; after it the task is
    // claimable as attempt 2, carrying the previous failure's evidence.
    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
    let task = claim_one(&app, "w2", 30_000).await;
    assert_eq!(task["attempt"], json!(2));
    assert_eq!(task["error_class"], json!("timeout"));
    assert_eq!(task["next_attempt_at"], Value::Null);

    let (status, v) = call(
        &app,
        "POST",
        &format!("/tasks/{task_id}/complete"),
        Some(json!({"worker_id": "w2", "result": {"recovered": true}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["status"], json!("completed"));
    // A completed task keeps its earlier failure as history.
    assert_eq!(v["error_class"], json!("timeout"));
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn exhausted_attempts_dead_letter_and_the_dlq_lists_them() {
    let (app, store) = app();
    let task_id = enqueue(&app, json!({"max_attempts": 1})).await;
    claim_one(&app, "w", 30_000).await;

    let (status, v) = call(
        &app,
        "POST",
        &format!("/tasks/{task_id}/fail"),
        Some(json!({"worker_id": "w", "error_class": "unknown",
                    "message": "gave up", "retryable": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "fail failed: {v}");
    assert_eq!(v["requeued"], json!(false));
    assert_eq!(v["dead"], json!(true));
    assert_eq!(v["next_attempt_at"], Value::Null);

    // The DLQ listing carries the entry with its failure evidence; dead
    // tasks are never claimable.
    let (status, v) = call(&app, "GET", "/tasks?status=dead", None).await;
    assert_eq!(status, StatusCode::OK);
    let entries = v.as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["task_id"], json!(task_id));
    assert_eq!(entries[0]["status"], json!("dead"));
    assert_eq!(entries[0]["error_class"], json!("unknown"));
    let (status, _) = call(
        &app,
        "POST",
        "/tasks/claim",
        Some(json!({"worker_id": "w", "lease_ms": 30_000})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn outright_failures_never_dead_letter() {
    let (app, store) = app();

    // Worker-declared unsafe-to-retry (retryable: false).
    let a = enqueue(&app, json!({})).await;
    claim_one(&app, "w", 30_000).await;
    let (status, v) = call(
        &app,
        "POST",
        &format!("/tasks/{a}/fail"),
        Some(json!({"worker_id": "w", "error_class": "timeout",
                    "message": "maybe it fired", "retryable": false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["requeued"], json!(false));
    assert_eq!(v["dead"], json!(false), "outright fail is not the DLQ");
    assert_eq!(v["next_attempt_at"], Value::Null);

    // Non-retryable class, despite the worker's retryable flag.
    let b = enqueue(&app, json!({})).await;
    claim_one(&app, "w", 30_000).await;
    let (status, v) = call(
        &app,
        "POST",
        &format!("/tasks/{b}/fail"),
        Some(json!({"worker_id": "w", "error_class": "invalid_input",
                    "message": "bad schema", "retryable": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["dead"], json!(false));
    assert_eq!(v["requeued"], json!(false));

    // Declared non-repeatable effect at enqueue time, despite retryable.
    let c = enqueue(&app, json!({"effect": "non_idempotent"})).await;
    claim_one(&app, "w", 30_000).await;
    let (status, v) = call(
        &app,
        "POST",
        &format!("/tasks/{c}/fail"),
        Some(json!({"worker_id": "w", "error_class": "timeout",
                    "message": "charged maybe", "retryable": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["dead"], json!(false));
    assert_eq!(v["requeued"], json!(false), "effect gate fails outright");

    // The DLQ stays empty; the failed listing shows all three as terminal
    // (null next_attempt_at), and the effect declaration is on the record.
    let (status, v) = call(&app, "GET", "/tasks?status=dead", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v.as_array().unwrap().len(), 0);
    let (status, v) = call(&app, "GET", "/tasks?status=failed", None).await;
    assert_eq!(status, StatusCode::OK);
    let failed = v.as_array().unwrap();
    assert_eq!(failed.len(), 3);
    assert!(failed.iter().all(|t| t["next_attempt_at"].is_null()));
    let (status, v) = call(&app, "GET", &format!("/tasks/{c}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["effect"], json!("non_idempotent"));
    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Tenant isolation
// --------------------------------------------------------------------- //

#[tokio::test]
async fn tasks_are_fully_isolated_per_tenant() {
    let (app, store) = multi_tenant_app();

    // Acme enqueues with an idempotency key.
    let (status, v) = call_as(
        &app,
        Some(ACME),
        "POST",
        "/tasks",
        Some(json!({"kind": "k", "payload": {}, "idempotency_key": "shared-key"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "acme enqueue failed: {v}");
    let task_id = v["task_id"].as_str().unwrap().to_string();

    // Globex cannot see, claim, or settle acme's task — 404 / 204, never 403.
    let (status, _) = call_as(
        &app,
        Some(GLOBEX),
        "GET",
        &format!("/tasks/{task_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call_as(
        &app,
        Some(GLOBEX),
        "POST",
        "/tasks/claim",
        Some(json!({"worker_id": "g", "lease_ms": 30_000})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call_as(
        &app,
        Some(GLOBEX),
        "POST",
        &format!("/tasks/{task_id}/heartbeat"),
        Some(json!({"worker_id": "g", "lease_ms": 30_000})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, v) = call_as(&app, Some(GLOBEX), "GET", "/tasks", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v.as_array().unwrap().len(), 0);

    // The same idempotency key is free for the other tenant (per-tenant
    // uniqueness), and the two tasks coexist independently.
    let (status, v) = call_as(
        &app,
        Some(GLOBEX),
        "POST",
        "/tasks",
        Some(json!({"kind": "k", "payload": {}, "idempotency_key": "shared-key"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_ne!(v["task_id"], json!(task_id));

    // Acme claims its own task; globex's queue is untouched.
    let (status, v) = call_as(
        &app,
        Some(ACME),
        "POST",
        "/tasks/claim",
        Some(json!({"worker_id": "a", "lease_ms": 30_000})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["task"]["task_id"], json!(task_id));
    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Validation
// --------------------------------------------------------------------- //

#[tokio::test]
async fn malformed_requests_are_400() {
    let (app, store) = app();
    let task_id = enqueue(&app, json!({})).await;

    let cases: Vec<(&str, String, Value)> = vec![
        (
            "POST",
            "/tasks".into(),
            json!({"kind": "  ", "payload": {}}),
        ),
        (
            "POST",
            "/tasks".into(),
            json!({"kind": "k", "payload": {}, "max_attempts": 0}),
        ),
        (
            "POST",
            "/tasks".into(),
            json!({"kind": "k", "payload": {}, "max_attempts": 101}),
        ),
        (
            "POST",
            "/tasks".into(),
            json!({"kind": "k", "payload": {}, "pool": "bad/pool"}),
        ),
        (
            "POST",
            "/tasks".into(),
            json!({"kind": "k", "payload": {}, "idempotency_key": ""}),
        ),
        (
            "POST",
            "/tasks".into(),
            json!({"kind": "k", "payload": {}, "effect": "side_effecty"}),
        ),
        (
            "POST",
            "/tasks/claim".into(),
            json!({"worker_id": "", "lease_ms": 30_000}),
        ),
        (
            "POST",
            "/tasks/claim".into(),
            json!({"worker_id": "w", "lease_ms": 99}),
        ),
        (
            "POST",
            "/tasks/claim".into(),
            json!({"worker_id": "w", "lease_ms": 3_600_001}),
        ),
        (
            "POST",
            "/tasks/claim".into(),
            json!({"worker_id": "w", "pools": [], "lease_ms": 30_000}),
        ),
        (
            "POST",
            format!("/tasks/{task_id}/heartbeat"),
            json!({"worker_id": "w", "lease_ms": 1}),
        ),
        (
            "POST",
            format!("/tasks/{task_id}/fail"),
            json!({"worker_id": "w", "error_class": "bug", "message": "x", "retryable": true}),
        ),
        (
            "POST",
            format!("/tasks/{task_id}/fail"),
            json!({"worker_id": "w", "error_class": "unknown", "message": "", "retryable": true}),
        ),
    ];
    for (method, uri, body) in cases {
        let (status, v) = call(&app, method, &uri, Some(body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{method} {uri}: {v}");
    }
    let (status, v) = call(&app, "GET", "/tasks?status=zombie", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "status filter: {v}");
    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Restart durability (JSON-file backend)
// --------------------------------------------------------------------- //

#[tokio::test]
async fn tasks_survive_a_router_rebuild() {
    let store = temp_store();
    let config = || ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone());

    let app = router(GraphRegistry::new(), config());
    // One completed task, one dead-lettered, one leased (whose lease will
    // have expired by the rebuild), one freshly queued.
    let done = enqueue(&app, json!({})).await;
    claim_one(&app, "w", 30_000).await;
    let (status, _) = call(
        &app,
        "POST",
        &format!("/tasks/{done}/complete"),
        Some(json!({"worker_id": "w", "result": {"kept": 1}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let dead = enqueue(&app, json!({"max_attempts": 1})).await;
    claim_one(&app, "w", 30_000).await;
    let (status, v) = call(
        &app,
        "POST",
        &format!("/tasks/{dead}/fail"),
        Some(
            json!({"worker_id": "w", "error_class": "dependency_failure",
                    "message": "db down", "retryable": true}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["dead"], json!(true));

    let leased = enqueue(&app, json!({})).await;
    claim_one(&app, "worker-gone", 100).await;
    let queued = enqueue(&app, json!({})).await;
    drop(app);

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    // The rebuild reloads every record from disk, state intact.
    let app2 = router(GraphRegistry::new(), config());
    let (status, v) = call(&app2, "GET", &format!("/tasks/{done}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["status"], json!("completed"));
    assert_eq!(v["result"], json!({"kept": 1}));

    let (status, v) = call(&app2, "GET", "/tasks?status=dead", None).await;
    assert_eq!(status, StatusCode::OK);
    let entries = v.as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["task_id"], json!(dead));
    assert_eq!(entries[0]["error_class"], json!("dependency_failure"));

    // The dead worker's lease expired while the server was down: the task
    // is reclaimable (attempt 2), ahead of the fresh one in creation order.
    let task = claim_one(&app2, "worker-new", 30_000).await;
    assert_eq!(task["task_id"], json!(leased));
    assert_eq!(task["attempt"], json!(2));
    let task = claim_one(&app2, "worker-new", 30_000).await;
    assert_eq!(task["task_id"], json!(queued));
    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Cancellation propagation (R0.6 wave 2a)
// --------------------------------------------------------------------- //

#[tokio::test]
async fn cancel_queued_task_is_terminal_and_never_leased() {
    let (app, store) = app();
    let task_id = enqueue(&app, json!({})).await;

    let (status, v) = call(&app, "POST", &format!("/tasks/{task_id}/cancel"), None).await;
    assert_eq!(status, StatusCode::OK, "cancel failed: {v}");
    assert_eq!(v["status"], json!("cancelled"));
    assert_eq!(v["error_class"], json!("cancelled"));
    assert_eq!(v["cancel_requested"], json!(false), "no holder to signal");

    // Terminal: never leased, not in the DLQ, listed under `cancelled`.
    let (status, _) = call(
        &app,
        "POST",
        "/tasks/claim",
        Some(json!({"worker_id": "w", "lease_ms": 30_000})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, v) = call(&app, "GET", "/tasks?status=dead", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v.as_array().unwrap().len(), 0);
    let (status, v) = call(&app, "GET", "/tasks?status=cancelled", None).await;
    assert_eq!(status, StatusCode::OK);
    let entries = v.as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["task_id"], json!(task_id));
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn cancel_leased_task_signals_via_heartbeat_then_settles_cancelled() {
    let (app, store) = app();
    let task_id = enqueue(&app, json!({})).await;
    claim_one(&app, "worker-1", 60_000).await;

    // The cancel keeps the lease and flags the record: the holder learns
    // on its next heartbeat, not through a 409.
    let (status, v) = call(&app, "POST", &format!("/tasks/{task_id}/cancel"), None).await;
    assert_eq!(status, StatusCode::OK, "cancel failed: {v}");
    assert_eq!(v["status"], json!("leased"));
    assert_eq!(v["cancel_requested"], json!(true));
    assert_eq!(v["lease"]["owner"], json!("worker-1"));

    let (status, v) = call(
        &app,
        "POST",
        &format!("/tasks/{task_id}/heartbeat"),
        Some(json!({"worker_id": "worker-1", "lease_ms": 60_000})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "heartbeat failed: {v}");
    assert_eq!(
        v["cancel_requested"],
        json!(true),
        "the hint reaches the holder"
    );

    // The holder aborts and reports the attempt as cancelled through the
    // fail path: the record ends terminal-cancelled, never the DLQ.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/tasks/{task_id}/fail"),
        Some(json!({"worker_id": "worker-1", "error_class": "cancelled",
                    "message": "cancelled by the control plane", "retryable": false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "fail failed: {v}");
    assert_eq!(v["requeued"], json!(false));
    assert_eq!(v["dead"], json!(false));
    let (status, v) = call(&app, "GET", &format!("/tasks/{task_id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["status"], json!("cancelled"));
    assert_eq!(v["error_class"], json!("cancelled"));
    let (status, v) = call(&app, "GET", "/tasks?status=dead", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v.as_array().unwrap().len(), 0);
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn unanswered_cancel_is_finalized_by_the_claim_path() {
    let (app, store) = app();
    let task_id = enqueue(&app, json!({})).await;
    // A worker takes a short lease and never asks (partition, slow handler).
    claim_one(&app, "worker-gone", 100).await;
    let (status, v) = call(&app, "POST", &format!("/tasks/{task_id}/cancel"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["cancel_requested"], json!(true));

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    // The next claim finalizes the task instead of re-leasing it:
    // cancellation outlives the lease.
    let (status, _) = call(
        &app,
        "POST",
        "/tasks/claim",
        Some(json!({"worker_id": "worker-new", "lease_ms": 30_000})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, v) = call(&app, "GET", &format!("/tasks/{task_id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["status"], json!("cancelled"));
    assert_eq!(v["error_class"], json!("cancelled"));
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn cancel_terminal_task_is_409_and_unknown_is_404() {
    let (app, store) = app();
    let done = enqueue(&app, json!({})).await;
    claim_one(&app, "w", 30_000).await;
    let (status, _) = call(
        &app,
        "POST",
        &format!("/tasks/{done}/complete"),
        Some(json!({"worker_id": "w", "result": null})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, v) = call(&app, "POST", &format!("/tasks/{done}/cancel"), None).await;
    assert_eq!(status, StatusCode::CONFLICT, "terminal cancel: {v}");
    let (status, v) = call(&app, "POST", "/tasks/nope/cancel", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "unknown cancel: {v}");
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn cancel_is_tenant_scoped() {
    let (app, store) = multi_tenant_app();
    let (status, v) = call_as(
        &app,
        Some(ACME),
        "POST",
        "/tasks",
        Some(json!({"kind": "k", "payload": {}})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "acme enqueue failed: {v}");
    let task_id = v["task_id"].as_str().unwrap().to_string();

    // Globex cannot cancel acme's task — 404, never 403.
    let (status, _) = call_as(
        &app,
        Some(GLOBEX),
        "POST",
        &format!("/tasks/{task_id}/cancel"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Acme's own cancel lands.
    let (status, v) = call_as(
        &app,
        Some(ACME),
        "POST",
        &format!("/tasks/{task_id}/cancel"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["status"], json!("cancelled"));
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn deadline_expired_task_is_cancelled_instead_of_leased() {
    let (app, store) = app();

    // A task enqueued already past its whole-task deadline is finalized as
    // cancelled by the claim path — never leased.
    let past = enqueue(&app, json!({"deadline": "2020-01-01T00:00:00Z"})).await;
    // A second task with a future deadline is claimable, proving the first
    // was skipped rather than the queue being empty.
    let future = enqueue(&app, json!({"deadline": "2999-01-01T00:00:00Z"})).await;

    let task = claim_one(&app, "w", 30_000).await;
    assert_eq!(task["task_id"], json!(future));
    assert!(task["deadline"].is_string());

    let (status, _) = call(
        &app,
        "POST",
        "/tasks/claim",
        Some(json!({"worker_id": "w", "lease_ms": 30_000})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, v) = call(&app, "GET", &format!("/tasks/{past}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["status"], json!("cancelled"));
    assert_eq!(v["error_class"], json!("cancelled"));
    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Run-level propagation
// --------------------------------------------------------------------- //

/// `first -> second`, appending to a `log` channel — the smallest graph a
/// run can execute, for the run-cancel tests.
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

/// Open-mode app with the `pipeline` graph registered.
fn app_with_graph() -> (Router, PathBuf) {
    let store = temp_store();
    let (pipeline, pipeline_spec) = pipeline_graph();
    let mut registry = GraphRegistry::new();
    registry.register("pipeline", pipeline, pipeline_spec);
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone());
    (router(registry, config), store)
}

/// Create a thread and start a run on it; returns the run id. The run
/// stays known to the manager (terminal runs are retained), so the
/// run-cancel route can resolve it.
async fn start_run(app: &Router) -> String {
    let (status, v) = call(app, "POST", "/threads", Some(json!({"graph": "pipeline"}))).await;
    assert_eq!(status, StatusCode::CREATED, "create thread failed: {v}");
    let thread_id = v["thread_id"].as_str().unwrap().to_string();
    let (status, v) = call(
        app,
        "POST",
        &format!("/threads/{thread_id}/runs"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "create run failed: {v}");
    v["run_id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn run_cancel_cancels_the_runs_outstanding_tasks() {
    let (app, store) = app_with_graph();
    let run_id = start_run(&app).await;

    // The run's tasks in three states: completed (terminal — propagation
    // must not touch it), leased (signalled), queued (finalized). Claims
    // are oldest-first, so the enqueue order arranges the states.
    let done = enqueue(&app, json!({"run_id": run_id})).await;
    let leased = enqueue(&app, json!({"run_id": run_id})).await;
    let queued = enqueue(&app, json!({"run_id": run_id})).await;
    // Another run's task must be unaffected.
    let other = enqueue(&app, json!({"run_id": "run-other"})).await;

    let task = claim_one(&app, "worker-1", 60_000).await;
    assert_eq!(task["task_id"], json!(done));
    let (status, _) = call(
        &app,
        "POST",
        &format!("/tasks/{done}/complete"),
        Some(json!({"worker_id": "worker-1", "result": {"ok": true}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let task = claim_one(&app, "worker-1", 60_000).await;
    assert_eq!(task["task_id"], json!(leased));

    let (status, v) = call(&app, "POST", &format!("/runs/{run_id}/cancel"), None).await;
    assert_eq!(status, StatusCode::OK, "run cancel failed: {v}");
    assert_eq!(v["cancelled"], json!([queued]));
    assert_eq!(v["signalled"], json!([leased]));

    // The queued task is terminal-cancelled; the leased one is signalled
    // for its holder; the completed one kept its outcome.
    let (status, v) = call(&app, "GET", &format!("/tasks/{queued}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["status"], json!("cancelled"));
    let (status, v) = call(
        &app,
        "POST",
        &format!("/tasks/{leased}/heartbeat"),
        Some(json!({"worker_id": "worker-1", "lease_ms": 60_000})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["cancel_requested"], json!(true));
    let (status, v) = call(&app, "GET", &format!("/tasks/{done}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["status"], json!("completed"));

    // Only the other run's task remains claimable.
    let task = claim_one(&app, "worker-2", 30_000).await;
    assert_eq!(task["task_id"], json!(other));
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn run_cancel_of_an_unknown_run_is_404() {
    let (app, store) = app_with_graph();
    let (status, v) = call(&app, "POST", "/runs/run-nope/cancel", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "unknown run cancel: {v}");
    let _ = std::fs::remove_dir_all(store);
}
