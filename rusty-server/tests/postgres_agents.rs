//! Live-Postgres integration tests for the Agent Fabric (R0.7, wave 1):
//! the `/agents` surface over the `server_agents` / `server_agent_leases`
//! tables and the mailbox columns of `server_tasks` — auto-migration,
//! activation claim/conflict/steal/heartbeat/release, turn-serialized
//! mailbox draining, pool-claim exclusion, and the concurrency proofs
//! (racing activation claims; racing turn claims).
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
//!   cargo test --features postgres --test postgres_agents -- --ignored
//! ```

#![cfg(feature = "postgres")]

use std::path::PathBuf;

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
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

/// An app whose server store (registry, leases, tasks) is Postgres-backed.
///
/// Every call runs as the dedicated `pg-agents` tenant: the Postgres test
/// binaries run in parallel against one scratch database, and the sibling
/// suites' claims drain the open-mode `default` tenant's pool — an
/// unscoped pool claim here could steal *their* tasks (and theirs could
/// make this suite's "pool claim answers 204" assertions flaky). Tenant
/// isolation keeps the suites blind to each other.
fn postgres_app() -> Router {
    let store_path: PathBuf =
        std::env::temp_dir().join(format!("rusty-server-pg-agents-{}", uuid::Uuid::new_v4()));
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store_path)
        .with_postgres(database_url())
        .with_tenant_key("pg-agents", "pg-agents-secret");
    router(GraphRegistry::new(), config)
}

/// Send a request as the suite's tenant; returns `(status,
/// json-body-or-null)`.
async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("x-api-key", "pg-agents-secret");
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

/// The manifest the tests register (two accepted kinds).
fn manifest() -> Value {
    json!({
        "agent_kind": "researcher",
        "manifest_version": "researcher/1.4.0",
        "accepts": {
            "summarize": {"kind": "application/json"},
            "triage": {"kind": "application/json", "max_bytes": 65536}
        }
    })
}

/// Register an agent under an explicit id; asserts 201.
async fn register(app: &Router, agent_id: &str) {
    let (status, v) = call(
        app,
        "POST",
        "/agents",
        Some(json!({"agent_id": agent_id, "manifest": manifest()})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "register failed: {v}");
}

/// Activate, asserting success; returns the granted fencing ordinal.
async fn activate_one(app: &Router, agent_id: &str, worker: &str, lease_ms: u64) -> u64 {
    let (status, v) = call(
        app,
        "POST",
        &format!("/agents/{agent_id}/activate"),
        Some(json!({"worker_id": worker, "lease_ms": lease_ms})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "activate failed: {v}");
    v["fencing"].as_u64().unwrap()
}

/// Claim the next mailbox message; returns `(status, body)`.
async fn next(app: &Router, agent_id: &str, worker: &str, fencing: u64) -> (StatusCode, Value) {
    call(
        app,
        "POST",
        &format!("/agents/{agent_id}/mailbox/next"),
        Some(json!({"worker_id": worker, "fencing": fencing, "lease_ms": 30_000})),
    )
    .await
}

/// The core flow over Postgres: registry, activation lifecycle, mailbox
/// send validation, turn-serialized draining, pool-claim exclusion.
#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn agents_registry_activation_and_mailbox_over_postgres() {
    let app = postgres_app();
    let agent_id = format!("pg-agent-{}", uniq());

    // Registry: create, fetch, conflict.
    register(&app, &agent_id).await;
    let (status, v) = call(&app, "GET", &format!("/agents/{agent_id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["manifest"]["agent_kind"], json!("researcher"));
    let (status, _) = call(
        &app,
        "POST",
        "/agents",
        Some(json!({"agent_id": agent_id, "manifest": manifest()})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, v) = call(&app, "GET", "/agents", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(v
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["agent_id"] == json!(agent_id)));

    // Mailbox send: undeclared kind fails fast; declared kinds land.
    let (status, _) = call(
        &app,
        "POST",
        &format!("/agents/{agent_id}/mailbox"),
        Some(json!({"kind": "exfiltrate", "payload": {}})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, v) = call(
        &app,
        "POST",
        &format!("/agents/{agent_id}/mailbox"),
        Some(json!({"kind": "summarize", "payload": {"n": 1}})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "send failed: {v}");
    let first_id = v["task_id"].as_str().unwrap().to_string();
    let (status, v) = call(
        &app,
        "POST",
        &format!("/agents/{agent_id}/mailbox"),
        Some(json!({"kind": "triage", "payload": {"n": 2}})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let second_id = v["task_id"].as_str().unwrap().to_string();

    // The pool claim never hands out mailbox traffic.
    let (status, _) = call(
        &app,
        "POST",
        "/tasks/claim",
        Some(json!({"worker_id": "pool-worker", "lease_ms": 30_000})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // No activation: the mailbox claim is a 409.
    let (status, _) = next(&app, &agent_id, "worker-1", 1).await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Activation: claim, conflict, heartbeat, expiry steal, fencing bump.
    let fencing = activate_one(&app, &agent_id, "worker-1", 30_000).await;
    assert_eq!(fencing, 1);
    let (status, _) = call(
        &app,
        "POST",
        &format!("/agents/{agent_id}/activate"),
        Some(json!({"worker_id": "worker-2", "lease_ms": 30_000})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "a live activation holds");
    let (status, v) = call(
        &app,
        "POST",
        &format!("/agents/{agent_id}/activate/heartbeat"),
        Some(json!({"worker_id": "worker-1", "fencing": fencing, "lease_ms": 60_000})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "heartbeat failed: {v}");
    let (status, _) = call(
        &app,
        "POST",
        &format!("/agents/{agent_id}/activate/heartbeat"),
        Some(json!({"worker_id": "worker-1", "fencing": fencing + 1, "lease_ms": 60_000})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "stale fencing must lose");

    // Turn-serialized draining, oldest first.
    let (status, v) = next(&app, &agent_id, "worker-1", fencing).await;
    assert_eq!(status, StatusCode::OK, "first turn: {v}");
    assert_eq!(v["task"]["task_id"], json!(first_id));
    let (status, _) = next(&app, &agent_id, "worker-1", fencing).await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "a turn in flight makes the mailbox unclaimable"
    );
    let (status, _) = call(
        &app,
        "POST",
        &format!("/tasks/{first_id}/complete"),
        Some(json!({"worker_id": "worker-1", "result": {"done": true}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, v) = next(&app, &agent_id, "worker-1", fencing).await;
    assert_eq!(status, StatusCode::OK, "second turn: {v}");
    assert_eq!(v["task"]["task_id"], json!(second_id));

    // Release: the replacement activates promptly.
    let (status, _) = call(
        &app,
        "POST",
        &format!("/agents/{agent_id}/activate/release"),
        Some(json!({"worker_id": "worker-1", "fencing": fencing})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let fencing = activate_one(&app, &agent_id, "worker-2", 30_000).await;
    assert_eq!(fencing, 1, "a released lease is gone, not stolen");
}

/// Sixteen racing activation claims against one agent: exactly one wins.
/// The `FOR UPDATE` lease-row lock serializes the steal decision, so this
/// is exact, not probabilistic.
#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn concurrent_activation_claims_exactly_one_wins() {
    let app = postgres_app();
    let agent_id = format!("pg-race-{}", uniq());
    register(&app, &agent_id).await;

    let mut handles = Vec::new();
    for i in 0..16 {
        let app = app.clone();
        let agent_id = agent_id.clone();
        handles.push(tokio::spawn(async move {
            call(
                &app,
                "POST",
                &format!("/agents/{agent_id}/activate"),
                Some(json!({"worker_id": format!("racer-{i}"), "lease_ms": 30_000})),
            )
            .await
        }));
    }
    let mut claimed = 0;
    let mut held = 0;
    for handle in handles {
        let (status, _) = handle.await.unwrap();
        match status {
            StatusCode::OK => claimed += 1,
            StatusCode::CONFLICT => held += 1,
            other => panic!("unexpected activate status {other}"),
        }
    }
    assert_eq!(claimed, 1, "exactly one racer may claim the activation");
    assert_eq!(held, 15);
}

/// Two racing turn claims by the one activation holder: exactly one
/// message is leased. The lease-row lock serializes the holder's claims,
/// so the turn gate is exact.
#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn concurrent_turn_claims_lease_exactly_one_message() {
    let app = postgres_app();
    let agent_id = format!("pg-turn-{}", uniq());
    register(&app, &agent_id).await;
    for n in 1..=2 {
        let (status, _) = call(
            &app,
            "POST",
            &format!("/agents/{agent_id}/mailbox"),
            Some(json!({"kind": "summarize", "payload": {"n": n}})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }
    let fencing = activate_one(&app, &agent_id, "worker-1", 30_000).await;

    let mut handles = Vec::new();
    for _ in 0..8 {
        let app = app.clone();
        let agent_id = agent_id.clone();
        handles.push(tokio::spawn(async move {
            next(&app, &agent_id, "worker-1", fencing).await
        }));
    }
    let mut leased = 0;
    for handle in handles {
        let (status, _) = handle.await.unwrap();
        match status {
            StatusCode::OK => leased += 1,
            StatusCode::NO_CONTENT => {}
            other => panic!("unexpected mailbox/next status {other}"),
        }
    }
    assert_eq!(
        leased, 1,
        "turn serialization: one message at a time, even under a racing holder"
    );
}
