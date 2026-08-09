//! Wave-3 (R0.6) integration tests: named pools with per-pool concurrency
//! limits, tenant quotas (`429` at submission), exact-match version pinning,
//! and the autoscaling-signals endpoint — over the default JSON-file
//! backend. Live-Postgres parity for the store-level changes is in
//! `postgres_tasks.rs`.
//!
//! Driven in-process via `tower::ServiceExt::oneshot` (no sockets). Pool
//! limits, quotas, and metrics never touch the graph registry, which stays
//! empty except where a thread is needed for `update_state`'s enqueue path.

use std::path::PathBuf;
use std::time::Duration;

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::prelude::*;
use rusty_agent_server::{router, GraphRegistry, ServerConfig, TaskQuota};
use serde_json::{json, Value};
use tower::ServiceExt;

const ACME: (&str, &str) = ("x-api-key", "acme-secret");
const GLOBEX: (&str, &str) = ("x-api-key", "globex-secret");

/// Unique temp store root, removed at the end of each test (best effort).
fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!("rusty-server-pools-test-{}", uuid::Uuid::new_v4()))
}

/// Build an open-mode app, configuring it from the default `ServerConfig`
/// through `with` (pool limits / quotas vary per test).
fn app_with(with: impl FnOnce(ServerConfig) -> ServerConfig) -> (Router, PathBuf) {
    let store = temp_store();
    let config = with(ServerConfig::new(
        "127.0.0.1:0".parse().unwrap(),
        store.clone(),
    ));
    (router(GraphRegistry::new(), config), store)
}

/// Send a request with an optional auth header; returns `(status,
/// json-body-or-null)`.
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

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    call_as(app, None, method, uri, body).await
}

/// Enqueue a task; returns its task id.
async fn enqueue(app: &Router, extra: Value) -> String {
    let mut body = json!({"kind": "work", "payload": {}});
    body.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    let (status, v) = call(app, "POST", "/tasks", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "enqueue failed: {v}");
    v["task_id"].as_str().unwrap().to_string()
}

/// Claim as `worker` from `pools`, asserting a task is handed out.
async fn claim_one(app: &Router, worker: &str, pools: &[&str]) -> Value {
    let (status, v) = call(
        app,
        "POST",
        "/tasks/claim",
        Some(json!({"worker_id": worker, "pools": pools, "lease_ms": 30_000})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "claim failed: {v}");
    v["task"].clone()
}

/// Claim as `worker` from `pools`, asserting nothing is handed out.
async fn claim_none(app: &Router, worker: &str, pools: &[&str]) {
    let (status, v) = call(
        app,
        "POST",
        "/tasks/claim",
        Some(json!({"worker_id": worker, "pools": pools, "lease_ms": 30_000})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "expected 204, got: {v}");
}

/// Claim advertising `worker_version`, asserting a task is handed out.
async fn claim_versioned(app: &Router, worker: &str, version: &str) -> Value {
    let (status, v) = call(
        app,
        "POST",
        "/tasks/claim",
        Some(json!({"worker_id": worker, "worker_version": version, "lease_ms": 30_000})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "versioned claim failed: {v}");
    v["task"].clone()
}

/// Claim advertising `worker_version`, asserting nothing is handed out.
async fn claim_versioned_none(app: &Router, worker: &str, version: &str) {
    let (status, v) = call(
        app,
        "POST",
        "/tasks/claim",
        Some(json!({"worker_id": worker, "worker_version": version, "lease_ms": 30_000})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "expected 204, got: {v}");
}

// --------------------------------------------------------------------- //
// Pool concurrency limits
// --------------------------------------------------------------------- //

#[tokio::test]
async fn pool_concurrency_limits_isolate_gpu_from_io() {
    let (app, store) = app_with(|c| c.with_pool_limit("gpu", 1).with_pool_limit("io", 2));

    // Three tasks in each bounded pool.
    for _ in 0..3 {
        enqueue(&app, json!({"pool": "gpu"})).await;
        enqueue(&app, json!({"pool": "io"})).await;
    }

    // The gpu pool's cap of 1 leases exactly one task; the second claim
    // sees the pool saturated even though work is queued.
    let gpu_task = claim_one(&app, "gpu-worker", &["gpu"]).await;
    assert_eq!(gpu_task["pool"], json!("gpu"));
    claim_none(&app, "gpu-worker-2", &["gpu"]).await;

    // The io pool is unaffected by gpu's saturation — the coexistence
    // proof: two leases up to its own cap, then its own 204.
    claim_one(&app, "io-worker-1", &["io"]).await;
    claim_one(&app, "io-worker-2", &["io"]).await;
    claim_none(&app, "io-worker-3", &["io"]).await;

    // Settling the gpu task frees capacity: the pool leases again.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/tasks/{}/complete", gpu_task["task_id"].as_str().unwrap()),
        Some(json!({"worker_id": "gpu-worker", "result": {"ok": true}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "complete failed: {v}");
    claim_one(&app, "gpu-worker-2", &["gpu"]).await;
    claim_none(&app, "gpu-worker-3", &["gpu"]).await;

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn unconfigured_pools_stay_uncapped() {
    // No pool limits configured: the pre-wave-3 behavior is unchanged —
    // every queued task leases, however many are in flight.
    let (app, store) = app_with(|c| c);
    for _ in 0..5 {
        enqueue(&app, json!({})).await;
    }
    for i in 0..5 {
        claim_one(&app, &format!("w-{i}"), &["default"]).await;
    }
    claim_none(&app, "w-6", &["default"]).await;
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn an_expired_lease_holds_no_pool_capacity() {
    let (app, store) = app_with(|c| c.with_pool_limit("gpu", 1));
    enqueue(&app, json!({"pool": "gpu"})).await;
    enqueue(&app, json!({"pool": "gpu"})).await;

    // A worker takes a very short lease and dies (no heartbeat). While the
    // lease lives the pool is saturated…
    let (status, _) = call(
        &app,
        "POST",
        "/tasks/claim",
        Some(json!({"worker_id": "gpu-gone", "pools": ["gpu"], "lease_ms": 100})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    claim_none(&app, "gpu-worker", &["gpu"]).await;

    // …but once it lapses the capacity returns: the next task leases (as a
    // new attempt of the expired one, then the fresh one).
    tokio::time::sleep(Duration::from_millis(150)).await;
    claim_one(&app, "gpu-worker", &["gpu"]).await;
    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Tenant quotas
// --------------------------------------------------------------------- //

#[tokio::test]
async fn queued_quota_rejects_submission_with_429() {
    let (app, store) = app_with(|c| {
        c.with_task_quota(TaskQuota {
            max_queued: Some(2),
            ..TaskQuota::default()
        })
    });

    enqueue(&app, json!({})).await;
    enqueue(&app, json!({})).await;

    // The third submission would pass the backlog cap: 429, with the
    // gauge named in the error body.
    let (status, v) = call(
        &app,
        "POST",
        "/tasks",
        Some(json!({"kind": "work", "payload": {}})),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "over quota: {v}");
    assert_eq!(v["error"], json!("quota_exceeded"));
    assert!(
        v["message"].as_str().unwrap().contains("queued"),
        "the 429 names the gauge: {v}"
    );

    // Claiming drains the backlog; submissions are accepted again.
    claim_one(&app, "w", &["default"]).await;
    enqueue(&app, json!({})).await;
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn in_flight_quota_backpressures_submission() {
    let (app, store) = app_with(|c| {
        c.with_task_quota(TaskQuota {
            max_in_flight: Some(1),
            ..TaskQuota::default()
        })
    });

    let task_id = enqueue(&app, json!({})).await;
    // Nothing in flight yet: a second submission is fine.
    enqueue(&app, json!({})).await;

    // One lease takes the tenant to the in-flight cap: submissions are
    // rejected until a worker settles.
    claim_one(&app, "w", &["default"]).await;
    let (status, v) = call(
        &app,
        "POST",
        "/tasks",
        Some(json!({"kind": "work", "payload": {}})),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "over quota: {v}");
    assert_eq!(v["error"], json!("quota_exceeded"));
    assert!(
        v["message"].as_str().unwrap().contains("in flight"),
        "the 429 names the gauge: {v}"
    );

    let (status, v) = call(
        &app,
        "POST",
        &format!("/tasks/{task_id}/complete"),
        Some(json!({"worker_id": "w", "result": null})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "complete failed: {v}");
    enqueue(&app, json!({})).await;
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn dlq_depth_counts_against_the_quota() {
    let (app, store) = app_with(|c| {
        c.with_task_quota(TaskQuota {
            max_dlq: Some(1),
            ..TaskQuota::default()
        })
    });

    // Drive one task to the DLQ (single attempt, exhausted budget).
    let task_id = enqueue(&app, json!({"max_attempts": 1})).await;
    claim_one(&app, "w", &["default"]).await;
    let (status, v) = call(
        &app,
        "POST",
        &format!("/tasks/{task_id}/fail"),
        Some(json!({"worker_id": "w", "error_class": "unknown",
                    "message": "gave up", "retryable": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "fail failed: {v}");
    assert_eq!(v["dead"], json!(true));

    // With the DLQ at its cap the tenant cannot submit more work until an
    // operator drains it — an unbounded DLQ is a disk-full outage.
    let (status, v) = call(
        &app,
        "POST",
        "/tasks",
        Some(json!({"kind": "work", "payload": {}})),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "over quota: {v}");
    assert_eq!(v["error"], json!("quota_exceeded"));
    assert!(
        v["message"].as_str().unwrap().contains("DLQ"),
        "the 429 names the gauge: {v}"
    );
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn quotas_are_tenant_scoped() {
    let store = temp_store();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_tenant_key("acme", "acme-secret")
        .with_tenant_key("globex", "globex-secret")
        .with_tenant_quota(
            "acme",
            TaskQuota {
                max_queued: Some(1),
                ..TaskQuota::default()
            },
        );
    let app = router(GraphRegistry::new(), config);

    // Acme is capped at one queued task; globex is uncapped.
    let (status, v) = call_as(
        &app,
        Some(ACME),
        "POST",
        "/tasks",
        Some(json!({"kind": "work", "payload": {}})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "acme enqueue failed: {v}");
    let (status, v) = call_as(
        &app,
        Some(ACME),
        "POST",
        "/tasks",
        Some(json!({"kind": "work", "payload": {}})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "acme over quota: {v}"
    );

    for _ in 0..3 {
        let (status, v) = call_as(
            &app,
            Some(GLOBEX),
            "POST",
            "/tasks",
            Some(json!({"kind": "work", "payload": {}})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "globex is uncapped: {v}");
    }

    // And globex's backlog does not count against acme's gauge: draining
    // acme's one task frees acme's submission, however much globex queued.
    let (status, _) = call_as(
        &app,
        Some(ACME),
        "POST",
        "/tasks/claim",
        Some(json!({"worker_id": "a", "lease_ms": 30_000})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, v) = call_as(
        &app,
        Some(ACME),
        "POST",
        "/tasks",
        Some(json!({"kind": "work", "payload": {}})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "acme freed up: {v}");
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn outbox_submission_counts_against_the_queued_quota() {
    // A pending outbox row is accepted work in the pipeline: it counts
    // against the backlog gauge, so the outbox is not a quota bypass. The
    // relay interval is set long so the row stays pending for the test.
    let (app, store) = app_with(|c| {
        c.with_task_quota(TaskQuota {
            max_queued: Some(1),
            ..TaskQuota::default()
        })
        .with_outbox_relay_interval(Duration::from_secs(3_600))
    });

    let (status, v) = call(
        &app,
        "POST",
        "/tasks/outbox",
        Some(json!({"kind": "work", "payload": {}})),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "outbox enqueue failed: {v}");

    // One pending row at the cap: both submission surfaces 429.
    for uri in ["/tasks", "/tasks/outbox"] {
        let (status, v) = call(
            &app,
            "POST",
            uri,
            Some(json!({"kind": "work", "payload": {}})),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::TOO_MANY_REQUESTS,
            "{uri} over quota: {v}"
        );
    }
    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Version pinning
// --------------------------------------------------------------------- //

#[tokio::test]
async fn version_pin_matches_only_the_exact_worker_version() {
    let (app, store) = app_with(|c| c);
    let pinned = enqueue(&app, json!({"worker_version": "activity-worker/1.4.0"})).await;

    // An unversioned worker cannot take pinned work; neither can a worker
    // advertising a different version. The exact version matches.
    claim_none(&app, "unversioned", &["default"]).await;
    claim_versioned_none(&app, "newer-worker", "activity-worker/1.5.0").await;
    let task = claim_versioned(&app, "pinned-worker", "activity-worker/1.4.0").await;
    assert_eq!(task["task_id"], json!(pinned));
    assert_eq!(task["worker_version"], json!("activity-worker/1.4.0"));
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn version_pin_survives_retries_until_the_task_finishes() {
    let (app, store) = app_with(|c| c);
    let pinned = enqueue(&app, json!({"worker_version": "w1", "max_attempts": 3})).await;

    // First attempt fails retryably: the pin rides along with the requeue.
    claim_versioned(&app, "w1-worker", "w1").await;
    let (status, v) = call(
        &app,
        "POST",
        &format!("/tasks/{pinned}/fail"),
        Some(json!({"worker_id": "w1-worker", "error_class": "transient",
                    "message": "hiccup", "retryable": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "fail failed: {v}");
    assert_eq!(v["requeued"], json!(true));

    // The first-retry backoff tops out at one second. Past it the task is
    // claimable — still only by w1-capable workers, never by w2.
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    claim_versioned_none(&app, "w2-worker", "w2").await;
    claim_none(&app, "unversioned", &["default"]).await;
    let task = claim_versioned(&app, "w1-worker-2", "w1").await;
    assert_eq!(task["task_id"], json!(pinned));
    assert_eq!(task["attempt"], json!(2));

    // Completion frees the version: an unpinned follow-up task is open to
    // any worker, versioned or not.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/tasks/{pinned}/complete"),
        Some(json!({"worker_id": "w1-worker-2", "result": {"ok": true}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "complete failed: {v}");
    let unpinned = enqueue(&app, json!({})).await;
    let task = claim_versioned(&app, "w2-worker", "w2").await;
    assert_eq!(task["task_id"], json!(unpinned));
    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Autoscaling signals
// --------------------------------------------------------------------- //

#[tokio::test]
async fn metrics_endpoint_reports_per_pool_signals() {
    let (app, store) = app_with(|c| {
        c.with_pool_limit("gpu", 4)
            // A configured-but-empty pool still reports (autoscaling to
            // zero needs the zero, not an absent entry).
            .with_pool_limit("quiet", 2)
    });

    enqueue(&app, json!({"pool": "gpu"})).await;
    enqueue(&app, json!({"pool": "gpu"})).await;
    enqueue(&app, json!({})).await; // default pool, uncapped

    // One gpu lease: live capacity 1/4 → saturation 0.25.
    claim_one(&app, "gpu-worker", &["gpu"]).await;

    let (status, v) = call(&app, "GET", "/tasks/metrics", None).await;
    assert_eq!(status, StatusCode::OK, "metrics failed: {v}");
    assert!(v["now"].is_string(), "the response carries server time");
    let pools = v["pools"].as_array().expect("pools is an array");
    let by_name = |name: &str| {
        pools
            .iter()
            .find(|p| p["pool"] == json!(name))
            .unwrap_or_else(|| panic!("pool `{name}` missing from {pools:?}"))
            .clone()
    };

    let gpu = by_name("gpu");
    assert_eq!(gpu["queue_depth"], json!(1));
    assert_eq!(gpu["leased"], json!(1));
    assert_eq!(gpu["concurrency_limit"], json!(4));
    assert_eq!(gpu["lease_saturation"], json!(0.25));
    assert!(
        gpu["oldest_visible_task_age_ms"].as_i64().is_some(),
        "one visible gpu task reports an age: {gpu}"
    );

    let default = by_name("default");
    assert_eq!(default["queue_depth"], json!(1));
    assert_eq!(default["leased"], json!(0));
    assert!(
        default["concurrency_limit"].is_null() && default["lease_saturation"].is_null(),
        "uncapped pools report no limit and no invented saturation: {default}"
    );

    let quiet = by_name("quiet");
    assert_eq!(quiet["queue_depth"], json!(0));
    assert_eq!(quiet["leased"], json!(0));
    assert_eq!(quiet["concurrency_limit"], json!(2));
    assert_eq!(quiet["lease_saturation"], json!(0.0));
    assert!(
        quiet["oldest_visible_task_age_ms"].is_null(),
        "nothing visible, no age: {quiet}"
    );

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn metrics_are_tenant_scoped() {
    let store = temp_store();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_tenant_key("acme", "acme-secret")
        .with_tenant_key("globex", "globex-secret");
    let app = router(GraphRegistry::new(), config);

    let (status, _) = call_as(
        &app,
        Some(ACME),
        "POST",
        "/tasks",
        Some(json!({"kind": "work", "payload": {}})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Acme's metrics show its task; globex sees an empty report — tenant
    // isolation covers the signals the same as the queue itself.
    let (status, v) = call_as(&app, Some(ACME), "GET", "/tasks/metrics", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["pools"].as_array().unwrap().len(), 1);
    let (status, v) = call_as(&app, Some(GLOBEX), "GET", "/tasks/metrics", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["pools"].as_array().unwrap().len(), 0);
    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// update_state's atomic enqueue respects the quota gate
// --------------------------------------------------------------------- //

/// `first -> second`, appending to a `log` channel — the smallest graph a
/// thread can be created against (mirrors tests/outbox.rs).
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

#[tokio::test]
async fn update_state_enqueue_is_quota_gated_before_any_write() {
    let store = temp_store();
    let (pipeline, pipeline_spec) = pipeline_graph();
    let mut registry = GraphRegistry::new();
    registry.register("pipeline", pipeline, pipeline_spec);
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_task_quota(TaskQuota {
            max_queued: Some(1),
            ..TaskQuota::default()
        })
        // Keep submissions pending (queued gauge counts them); the test
        // does not depend on relay timing.
        .with_outbox_relay_interval(Duration::from_secs(3_600));
    let app = router(registry, config);

    let (status, v) = call(&app, "POST", "/threads", Some(json!({"graph": "pipeline"}))).await;
    assert_eq!(status, StatusCode::CREATED, "thread creation failed: {v}");
    let thread_id = v["thread_id"].as_str().unwrap().to_string();

    // The first atomic enqueue fills the backlog gauge.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/threads/{thread_id}/state"),
        Some(json!({
            "values": {"log": ["one"]},
            "enqueue": [{"kind": "work", "payload": {}, "idempotency_key": "e-1"}],
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "first update_state failed: {v}"
    );

    // The second is over quota: 429, and — the all-or-nothing contract —
    // neither the task nor the checkpoint landed.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/threads/{thread_id}/state"),
        Some(json!({
            "values": {"log": ["two"]},
            "enqueue": [{"kind": "work", "payload": {}, "idempotency_key": "e-2"}],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "over quota: {v}");
    assert_eq!(v["error"], json!("quota_exceeded"));

    let (status, v) = call(&app, "GET", &format!("/threads/{thread_id}/state"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        v["values"]["log"],
        json!(["one"]),
        "the rejected update wrote no checkpoint"
    );

    let _ = std::fs::remove_dir_all(store);
}
