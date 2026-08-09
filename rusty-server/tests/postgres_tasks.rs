//! Live-Postgres integration tests for the durable task queue: the
//! `/tasks` surface over the `server_tasks` table — auto-migration, the
//! enqueue dedup unique index, `FOR UPDATE SKIP LOCKED` claiming under
//! concurrency, lease reclaim, retry / dead-letter, and tenant isolation.
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
//!   cargo test --features postgres --test postgres_tasks -- --ignored
//! ```

#![cfg(feature = "postgres")]

use std::path::PathBuf;

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_server::{router, GraphRegistry, ServerConfig, TaskQuota};
use serde_json::{json, Value};
use tower::ServiceExt;

/// The database these tests run against; panics with guidance when unset.
fn database_url() -> String {
    std::env::var("DATABASE_URL").expect(
        "DATABASE_URL must point at a scratch Postgres database \
         (e.g. postgres://user:pass@localhost/rusty_test)",
    )
}

/// An app whose server store (including `server_tasks`) is Postgres-backed.
fn postgres_app() -> Router {
    let store_path: PathBuf =
        std::env::temp_dir().join(format!("rusty-server-pg-tasks-{}", uuid::Uuid::new_v4()));
    let config =
        ServerConfig::new("127.0.0.1:0".parse().unwrap(), store_path).with_postgres(database_url());
    router(GraphRegistry::new(), config)
}

/// Two-tenant Postgres app for the isolation test.
fn postgres_tenant_app() -> Router {
    let store_path: PathBuf =
        std::env::temp_dir().join(format!("rusty-server-pg-tasks-{}", uuid::Uuid::new_v4()));
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store_path)
        .with_postgres(database_url())
        .with_tenant_key("acme", "acme-secret")
        .with_tenant_key("globex", "globex-secret");
    router(GraphRegistry::new(), config)
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

/// Unique fragment so repeated runs against a shared scratch database
/// never collide.
fn uniq() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

#[tokio::test]
#[ignore = "requires a live Postgres (DATABASE_URL)"]
async fn postgres_task_lifecycle_and_dedup() {
    let app = postgres_app();
    let key = format!("charge-{}", uniq());

    // Enqueue with an idempotency key and a declared effect.
    let (status, v) = call(
        &app,
        "POST",
        "/tasks",
        Some(json!({
            "kind": "charge_card",
            "payload": {"amount": 100},
            "idempotency_key": key,
            "effect": "idempotent",
            "max_attempts": 2,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "enqueue failed: {v}");
    let task_id = v["task_id"].as_str().unwrap().to_string();

    // The partial unique index dedups the retry, returning the same row.
    let (status, v) = call(
        &app,
        "POST",
        "/tasks",
        Some(json!({"kind": "charge_card", "payload": {"amount": 100},
                    "idempotency_key": key})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "dedup enqueue failed: {v}");
    assert_eq!(v["task_id"], json!(task_id));
    assert_eq!(v["deduplicated"], json!(true));

    // Claim → heartbeat → fail (retryable, budget left) → requeued.
    let (status, v) = call(
        &app,
        "POST",
        "/tasks/claim",
        Some(json!({"worker_id": "pg-worker", "lease_ms": 60_000})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "claim failed: {v}");
    assert_eq!(v["task"]["task_id"], json!(task_id));
    assert_eq!(v["task"]["attempt"], json!(1));
    assert_eq!(v["task"]["effect"], json!("idempotent"));
    assert_eq!(v["task"]["max_attempts"], json!(2));

    let (status, v) = call(
        &app,
        "POST",
        &format!("/tasks/{task_id}/heartbeat"),
        Some(json!({"worker_id": "pg-worker", "lease_ms": 60_000})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "heartbeat failed: {v}");
    assert!(v["lease_expires_at"].is_string());

    let (status, v) = call(
        &app,
        "POST",
        &format!("/tasks/{task_id}/fail"),
        Some(
            json!({"worker_id": "pg-worker", "error_class": "rate_limited",
                    "message": "429", "retryable": true}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "fail failed: {v}");
    assert_eq!(v["requeued"], json!(true));
    assert_eq!(v["dead"], json!(false));

    // Attempt 2 (after the ≤1s first-retry backoff) dead-letters at the
    // budget; the DLQ listing comes out of Postgres with the evidence.
    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
    let (status, v) = call(
        &app,
        "POST",
        "/tasks/claim",
        Some(json!({"worker_id": "pg-worker-2", "lease_ms": 60_000})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "re-claim failed: {v}");
    assert_eq!(v["task"]["attempt"], json!(2));
    assert_eq!(v["task"]["error_class"], json!("rate_limited"));

    let (status, v) = call(
        &app,
        "POST",
        &format!("/tasks/{task_id}/fail"),
        Some(
            json!({"worker_id": "pg-worker-2", "error_class": "rate_limited",
                    "message": "429 again", "retryable": true}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["dead"], json!(true));

    let (status, v) = call(&app, "GET", "/tasks?status=dead", None).await;
    assert_eq!(status, StatusCode::OK);
    let entry = v
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["task_id"] == json!(task_id))
        .expect("dead-lettered task missing from the DLQ");
    assert_eq!(entry["error_class"], json!("rate_limited"));
    assert_eq!(entry["last_error"], json!("429 again"));
}

#[tokio::test]
#[ignore = "requires a live Postgres (DATABASE_URL)"]
async fn postgres_concurrent_claims_hand_one_task_to_one_worker() {
    let app = postgres_app();
    let (status, v) = call(
        &app,
        "POST",
        "/tasks",
        Some(json!({"kind": "solo", "payload": {}})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "enqueue failed: {v}");

    // Eight workers race one task: SKIP LOCKED hands it to exactly one.
    let claims: Vec<_> = (0..8)
        .map(|i| {
            let app = app.clone();
            async move {
                call(
                    &app,
                    "POST",
                    "/tasks/claim",
                    Some(json!({"worker_id": format!("racer-{i}"), "lease_ms": 60_000})),
                )
                .await
            }
        })
        .collect();
    let outcomes = futures::future::join_all(claims).await;
    let winners: Vec<&(StatusCode, Value)> = outcomes
        .iter()
        .filter(|(status, _)| *status == StatusCode::OK)
        .collect();
    let losers = outcomes
        .iter()
        .filter(|(status, _)| *status == StatusCode::NO_CONTENT)
        .count();
    assert_eq!(winners.len(), 1, "exactly one claim may win: {outcomes:?}");
    assert_eq!(losers, 7);
    assert_eq!(winners[0].1["task"]["attempt"], json!(1));
}

#[tokio::test]
#[ignore = "requires a live Postgres (DATABASE_URL)"]
async fn postgres_lease_expiry_reclaims_and_tenants_are_isolated() {
    let app = postgres_tenant_app();

    let (status, v) = call_as(
        &app,
        Some(("x-api-key", "acme-secret")),
        "POST",
        "/tasks",
        Some(json!({"kind": "k", "payload": {}})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "acme enqueue failed: {v}");
    let task_id = v["task_id"].as_str().unwrap().to_string();

    // Acme worker takes a 100 ms lease; globex sees nothing meanwhile.
    let (status, _) = call_as(
        &app,
        Some(("x-api-key", "acme-secret")),
        "POST",
        "/tasks/claim",
        Some(json!({"worker_id": "a", "lease_ms": 100})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call_as(
        &app,
        Some(("x-api-key", "globex-secret")),
        "GET",
        &format!("/tasks/{task_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    // The expired lease is reclaimable as attempt 2; the lost holder 409s.
    let (status, v) = call_as(
        &app,
        Some(("x-api-key", "acme-secret")),
        "POST",
        "/tasks/claim",
        Some(json!({"worker_id": "b", "lease_ms": 60_000})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "reclaim failed: {v}");
    assert_eq!(v["task"]["attempt"], json!(2));
    let (status, _) = call_as(
        &app,
        Some(("x-api-key", "acme-secret")),
        "POST",
        &format!("/tasks/{task_id}/complete"),
        Some(json!({"worker_id": "a", "result": null})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

// --------------------------------------------------------------------- //
// Cancellation propagation (R0.6 wave 2a) — parity with the file backend
// --------------------------------------------------------------------- //

#[tokio::test]
#[ignore = "requires a live Postgres (DATABASE_URL)"]
async fn postgres_cancel_matches_the_file_backends_semantics() {
    let app = postgres_app();
    // Tests in this file share one scratch database and run concurrently;
    // a unique pool per test keeps claims from stealing each other's tasks.
    let pool = format!("cancel-{}", uniq());

    // Queued task: cancel is immediate-terminal, never leased, never DLQ.
    let (status, v) = call(
        &app,
        "POST",
        "/tasks",
        Some(json!({"kind": "k", "payload": {}, "pool": pool,
                    "idempotency_key": format!("cancel-q-{}", uniq())})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "enqueue failed: {v}");
    let queued = v["task_id"].as_str().unwrap().to_string();
    let (status, v) = call(&app, "POST", &format!("/tasks/{queued}/cancel"), None).await;
    assert_eq!(status, StatusCode::OK, "cancel failed: {v}");
    assert_eq!(v["status"], json!("cancelled"));
    assert_eq!(v["error_class"], json!("cancelled"));
    // Re-cancelling a terminal task is 409.
    let (status, _) = call(&app, "POST", &format!("/tasks/{queued}/cancel"), None).await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Leased task: cancel signals; the holder's heartbeat carries the
    // hint; the cancelled fail report lands terminal-cancelled.
    let (status, v) = call(
        &app,
        "POST",
        "/tasks",
        Some(json!({"kind": "k", "payload": {}, "pool": pool,
                    "idempotency_key": format!("cancel-l-{}", uniq())})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "enqueue failed: {v}");
    let leased = v["task_id"].as_str().unwrap().to_string();
    let (status, v) = call(
        &app,
        "POST",
        "/tasks/claim",
        Some(json!({"worker_id": "pg-worker", "pools": [pool], "lease_ms": 60_000})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "claim failed: {v}");
    assert_eq!(v["task"]["task_id"], json!(leased));

    let (status, v) = call(&app, "POST", &format!("/tasks/{leased}/cancel"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["status"], json!("leased"));
    assert_eq!(v["cancel_requested"], json!(true));

    let (status, v) = call(
        &app,
        "POST",
        &format!("/tasks/{leased}/heartbeat"),
        Some(json!({"worker_id": "pg-worker", "lease_ms": 60_000})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "heartbeat failed: {v}");
    assert_eq!(v["cancel_requested"], json!(true));

    let (status, v) = call(
        &app,
        "POST",
        &format!("/tasks/{leased}/fail"),
        Some(json!({"worker_id": "pg-worker", "error_class": "cancelled",
                    "message": "cancelled by the control plane", "retryable": false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "fail failed: {v}");
    assert_eq!(v["dead"], json!(false));
    assert_eq!(v["requeued"], json!(false));
    let (status, v) = call(&app, "GET", &format!("/tasks/{leased}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["status"], json!("cancelled"));

    // Neither task is claimable or dead-lettered.
    let (status, _) = call(
        &app,
        "POST",
        "/tasks/claim",
        Some(json!({"worker_id": "pg-worker-2", "pools": [pool], "lease_ms": 60_000})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, v) = call(&app, "GET", "/tasks?status=dead", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !v.as_array()
            .unwrap()
            .iter()
            .any(|t| t["task_id"] == json!(queued) || t["task_id"] == json!(leased)),
        "cancelled tasks must never dead-letter"
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres (DATABASE_URL)"]
async fn postgres_cancel_and_claim_race_leaves_a_consistent_record() {
    let app = postgres_app();
    // Unique pool per round: tests here share the scratch database (see
    // the parity test above).
    let pool = format!("race-{}", uniq());

    // Five rounds of one-task races: whichever lands first, the record
    // ends in exactly one of the two coherent states.
    for round in 0..5 {
        let (status, v) = call(
            &app,
            "POST",
            "/tasks",
            Some(
                json!({"kind": "race", "payload": {"round": round}, "pool": pool,
                        "idempotency_key": format!("race-{round}-{}", uniq())}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "enqueue failed: {v}");
        let task_id = v["task_id"].as_str().unwrap().to_string();

        let (claim, cancel) = futures::future::join(
            call(
                &app,
                "POST",
                "/tasks/claim",
                Some(
                    json!({"worker_id": format!("racer-{round}"), "pools": [pool],
                            "lease_ms": 60_000}),
                ),
            ),
            call(&app, "POST", &format!("/tasks/{task_id}/cancel"), None),
        )
        .await;

        let (status, v) = call(&app, "GET", &format!("/tasks/{task_id}"), None).await;
        assert_eq!(status, StatusCode::OK);
        match (claim.0, cancel.0) {
            // The cancel landed first: claim saw nothing, task is terminal.
            (StatusCode::NO_CONTENT, StatusCode::OK) => {
                assert_eq!(v["status"], json!("cancelled"));
                assert_eq!(v["lease"], Value::Null);
            }
            // The claim landed first: the task is leased with the
            // cancellation signalled to the holder.
            (StatusCode::OK, StatusCode::OK) => {
                assert_eq!(v["status"], json!("leased"));
                assert_eq!(v["cancel_requested"], json!(true));
                assert_eq!(v["lease"]["owner"], json!(format!("racer-{round}")));
            }
            other => panic!("incoherent race outcome: {other:?}"),
        }
    }
}

// --------------------------------------------------------------------- //
// Pools, pinning, quotas, and signals (R0.6 wave 3a) — parity with the
// file backend's `pools.rs`
// --------------------------------------------------------------------- //

#[tokio::test]
#[ignore = "requires a live Postgres (DATABASE_URL)"]
async fn postgres_pool_limits_pinning_and_metrics_match_the_file_backend() {
    let pool = format!("pg-pools-{}", uniq());
    let idle_pool = format!("pg-idle-{}", uniq());
    // Tests in this file share the scratch database and run concurrently,
    // and pre-existing tests claim across *all* pools in the default
    // tenant — so this test works in its own tenant, where its tasks are
    // invisible to every other test's claims (and theirs to its metrics).
    let tenant = format!("pools-{}", uniq());
    let secret = "pools-secret";
    let store_path: PathBuf =
        std::env::temp_dir().join(format!("rusty-server-pg-tasks-{}", uuid::Uuid::new_v4()));
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store_path)
        .with_postgres(database_url())
        .with_tenant_key(&tenant, secret)
        .with_pool_limit(&pool, 2)
        .with_pool_limit(&idle_pool, 4);
    let app = router(GraphRegistry::new(), config);
    let auth = Some(("x-api-key", secret));

    // One pinned, one unpinned task in the capped pool.
    let version = format!("pg-worker/{}", uniq());
    let (status, v) = call_as(
        &app,
        auth,
        "POST",
        "/tasks",
        Some(json!({"kind": "plain", "payload": {}, "pool": pool})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "enqueue failed: {v}");
    let (status, v) = call_as(
        &app,
        auth,
        "POST",
        "/tasks",
        Some(json!({"kind": "pinned", "payload": {}, "pool": pool,
                    "worker_version": version})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "pinned enqueue failed: {v}");

    // An unversioned worker gets the unpinned task; the pinned one is
    // invisible to it even with pool capacity to spare.
    let (status, v) = call_as(
        &app,
        auth,
        "POST",
        "/tasks/claim",
        Some(json!({"worker_id": "pg-w1", "pools": [pool], "lease_ms": 60_000})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "claim failed: {v}");
    assert_eq!(v["task"]["kind"], json!("plain"));
    let (status, _) = call_as(
        &app,
        auth,
        "POST",
        "/tasks/claim",
        Some(json!({"worker_id": "pg-w1", "pools": [pool], "lease_ms": 60_000})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "unversioned worker saw pinned work"
    );

    // A different version does not match; the exact string does.
    let (status, _) = call_as(
        &app,
        auth,
        "POST",
        "/tasks/claim",
        Some(
            json!({"worker_id": "pg-w2", "pools": [pool], "lease_ms": 60_000,
                    "worker_version": format!("{version}-other")}),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "wrong version saw pinned work"
    );
    let (status, v) = call_as(
        &app,
        auth,
        "POST",
        "/tasks/claim",
        Some(
            json!({"worker_id": "pg-w2", "pools": [pool], "lease_ms": 60_000,
                    "worker_version": version}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "versioned claim failed: {v}");
    assert_eq!(v["task"]["kind"], json!("pinned"));

    // Both leases live: the pool is at its limit of 2, so a third worker
    // gets nothing even though an uncapped pool would still hand out work.
    let (status, _) = call_as(
        &app,
        auth,
        "POST",
        "/tasks/claim",
        Some(json!({"worker_id": "pg-w3", "pools": [pool], "lease_ms": 60_000})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "saturated pool handed out work"
    );

    // The autoscaling signals: the capped pool reads fully saturated, and
    // the configured-but-idle pool reports zeros rather than vanishing.
    let (status, v) = call_as(&app, auth, "GET", "/tasks/metrics", None).await;
    assert_eq!(status, StatusCode::OK, "metrics failed: {v}");
    assert!(v["now"].is_string());
    let pools = v["pools"].as_array().unwrap();
    let entry = pools
        .iter()
        .find(|p| p["pool"] == json!(pool))
        .expect("capped pool missing from metrics");
    assert_eq!(entry["queue_depth"], json!(0));
    assert_eq!(entry["leased"], json!(2));
    assert_eq!(entry["concurrency_limit"], json!(2));
    assert_eq!(entry["lease_saturation"], json!(1.0));
    assert_eq!(entry["oldest_visible_task_age_ms"], Value::Null);
    let idle = pools
        .iter()
        .find(|p| p["pool"] == json!(idle_pool))
        .expect("configured-but-idle pool missing from metrics");
    assert_eq!(idle["queue_depth"], json!(0));
    assert_eq!(idle["leased"], json!(0));
    assert_eq!(idle["concurrency_limit"], json!(4));
    assert_eq!(idle["lease_saturation"], json!(0.0));
}

#[tokio::test]
#[ignore = "requires a live Postgres (DATABASE_URL)"]
async fn postgres_quota_rejects_submission_with_429() {
    // Tests in this file share the scratch database, so the quota attaches
    // to a unique tenant — the default tenant carries other tests' rows.
    let tenant = format!("quota-{}", uniq());
    let secret = "quota-secret";
    let store_path: PathBuf =
        std::env::temp_dir().join(format!("rusty-server-pg-tasks-{}", uuid::Uuid::new_v4()));
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store_path)
        .with_postgres(database_url())
        .with_tenant_key(&tenant, secret)
        .with_tenant_quota(
            &tenant,
            TaskQuota {
                max_queued: Some(1),
                ..TaskQuota::default()
            },
        );
    let app = router(GraphRegistry::new(), config);
    let auth = Some(("x-api-key", secret));

    let (status, v) = call_as(
        &app,
        auth,
        "POST",
        "/tasks",
        Some(json!({"kind": "work", "payload": {}})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "first enqueue failed: {v}");

    // Backlog at the cap: the next submission is refused, and the refusal
    // names the gauge so an operator knows what to drain or raise.
    let (status, v) = call_as(
        &app,
        auth,
        "POST",
        "/tasks",
        Some(json!({"kind": "work", "payload": {}})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "over-quota enqueue: {v}"
    );
    assert_eq!(v["error"], json!("quota_exceeded"));
    assert!(
        v["message"].as_str().unwrap().contains("queued"),
        "429 message must name the gauge: {v}"
    );
}
