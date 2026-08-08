//! Live-Postgres integration tests for Agent Fabric wave 2 (R0.7):
//! supervision and the cancellation tree over the `server_agents` payload
//! column, the recipient-scoped cancel SQL, and the dead-letter insert —
//! the two wave-2 exit criteria plus the `update_agent` persistence
//! roundtrip (a fresh app instance over the same database must see the
//! escalated state, the journaled decision, and the dead-lettered
//! notice).
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
//!   cargo test --features postgres --test postgres_supervision -- --ignored
//! ```

#![cfg(feature = "postgres")]

use std::path::PathBuf;

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

/// The database these tests run against; panics with guidance when unset.
fn database_url() -> String {
    std::env::var("DATABASE_URL").expect(
        "DATABASE_URL must point at a scratch Postgres database \
         (e.g. postgres://user:pass@localhost/rusty_test)",
    )
}

/// An app whose server store is Postgres-backed.
///
/// Every call runs as the dedicated `pg-supervision` tenant: the Postgres
/// test binaries run in parallel against one scratch database, and tenant
/// isolation keeps the suites blind to each other (the `postgres_agents`
/// convention).
fn postgres_app() -> Router {
    let store_path: PathBuf = std::env::temp_dir().join(format!(
        "rusty-server-pg-supervision-{}",
        uuid::Uuid::new_v4()
    ));
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store_path)
        .with_postgres(database_url())
        .with_tenant_key("pg-supervision", "pg-supervision-secret");
    router(GraphRegistry::new(), config)
}

/// Send a request as the suite's tenant; returns `(status,
/// json-body-or-null)`.
async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("x-api-key", "pg-supervision-secret");
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

/// The base manifest (one accepted kind, `work`), with wave-2 fields
/// merged in per test.
fn manifest_with(extra: Value) -> Value {
    let mut manifest = json!({
        "agent_kind": "worker",
        "manifest_version": "worker/1.0.0",
        "accepts": {"work": {"kind": "application/json"}}
    });
    manifest
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    manifest
}

/// A `SupervisionPolicy` wire object (60 s window: the tests never leave
/// it, so the intensity count is the whole history).
fn policy(restart: &str, intensity: u32, supervisor: Option<&str>) -> Value {
    let mut p = json!({"restart": restart, "intensity": intensity, "period_ms": 60_000});
    if let Some(s) = supervisor {
        p["supervisor"] = json!(s);
    }
    p
}

/// Register an agent; asserts 201.
async fn register_with(app: &Router, agent_id: &str, manifest: Value, team_id: Option<&str>) {
    let mut body = json!({"agent_id": agent_id, "manifest": manifest});
    if let Some(team) = team_id {
        body["team_id"] = json!(team);
    }
    let (status, v) = call(app, "POST", "/agents", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "register failed: {v}");
}

/// Activate, asserting success; returns the fencing ordinal.
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

/// Send a `work` message; asserts 201 and returns the task id.
async fn send_one(app: &Router, agent_id: &str, extra: Value) -> String {
    let mut body = json!({"kind": "work", "payload": {"n": 1}});
    body.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    let (status, v) = call(
        app,
        "POST",
        &format!("/agents/{agent_id}/mailbox"),
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "send failed: {v}");
    v["task_id"].as_str().unwrap().to_string()
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

/// Claim, asserting a message was leased; returns the task wire object.
async fn next_one(app: &Router, agent_id: &str, worker: &str, fencing: u64) -> Value {
    let (status, v) = next(app, agent_id, worker, fencing).await;
    assert_eq!(status, StatusCode::OK, "claim failed: {v}");
    v["task"].clone()
}

/// Report the held turn as failed (retryable); asserts 200 and returns
/// the failure-settlement body.
async fn fail_turn(app: &Router, task_id: &str, worker: &str, error_class: &str) -> Value {
    let (status, v) = call(
        app,
        "POST",
        &format!("/tasks/{task_id}/fail"),
        Some(json!({"worker_id": worker, "error_class": error_class,
                    "message": format!("turn crashed ({error_class})"), "retryable": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "fail failed: {v}");
    v
}

/// Fetch the agent's supervision evidence; asserts 200.
async fn supervision(app: &Router, agent_id: &str) -> Value {
    let (status, v) = call(app, "GET", &format!("/agents/{agent_id}/supervision"), None).await;
    assert_eq!(status, StatusCode::OK, "supervision failed: {v}");
    v
}

/// Exit criterion 1, live-Postgres: the crash-loop escalation with the
/// attempt history intact and the decisions journaled.
#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn pg_crash_loop_escalates_to_the_supervisor_with_history() {
    let app = postgres_app();
    let run = uniq();
    let boss = format!("boss-{run}");
    let looper = format!("looper-{run}");
    register_with(
        &app,
        &boss,
        manifest_with(json!({
            "accepts": {"escalated": {"kind": "application/json"}}
        })),
        None,
    )
    .await;
    register_with(
        &app,
        &looper,
        manifest_with(json!({
            "supervision": policy("permanent", 2, Some(&boss))
        })),
        None,
    )
    .await;
    let looper_fence = activate_one(&app, &looper, "host-1", 60_000).await;
    let boss_fence = activate_one(&app, &boss, "host-boss", 60_000).await;

    // Three failed attempts of one message: restart, restart, escalate.
    let task_id = send_one(&app, &looper, json!({"max_attempts": 10})).await;
    let mut escalation_report = Value::Null;
    for (round, backoff_ms) in [(1u64, 1_100u64), (2, 2_100), (3, 4_100)] {
        let task = next_one(&app, &looper, "host-1", looper_fence).await;
        assert_eq!(task["task_id"], json!(task_id));
        assert_eq!(task["attempt"], json!(round));
        let v = fail_turn(&app, &task_id, "host-1", "transient").await;
        if round == 3 {
            escalation_report = v["escalation"].clone();
        } else {
            assert_eq!(v["escalation"], Value::Null);
        }
        tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
    }
    assert_eq!(escalation_report["kind"], json!("mailbox"));

    // The escalation rides the supervisor's mailbox with the full history.
    let escalation = next_one(&app, &boss, "host-boss", boss_fence).await;
    assert_eq!(escalation["kind"], json!("escalated"));
    assert_eq!(
        escalation["idempotency_key"],
        json!(format!("escalation:{looper}:3"))
    );
    let notice = &escalation["payload"];
    assert_eq!(notice["agent_id"], json!(looper));
    assert_eq!(notice["policy"]["restart"], json!("permanent"));
    let attempts = notice["attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 3);
    for (i, attempt) in attempts.iter().enumerate() {
        assert_eq!(attempt["ordinal"], json!(i as u64 + 1));
        assert_eq!(attempt["trigger"], json!("turn_failed"));
        assert_eq!(attempt["task_id"], json!(task_id));
    }

    // The journaled trail and the latched state read back through the
    // dedicated endpoint (tenant-scoped run id in the journal name).
    let v = supervision(&app, &looper).await;
    assert_eq!(v["escalated"], json!(true));
    assert_eq!(
        v["journal_run_id"],
        json!(format!("agent-supervision:pg-supervision:{looper}"))
    );
    let events = v["events"].as_array().unwrap();
    assert_eq!(events.len(), 3);
    for (event, decision) in events.iter().zip(["restart", "restart", "escalate"]) {
        assert_eq!(event["kind"], json!("supervision_event"));
        assert_eq!(event["output"]["value"]["decision"], json!(decision));
    }
}

/// Exit criterion 2, live-Postgres: the recipient-scoped cancel SQL
/// leaves zero orphan tasks across the team's mailboxes.
#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn pg_team_cancel_leaves_zero_orphan_tasks() {
    let app = postgres_app();
    let run = uniq();
    let team = format!("squad-{}", &run[..8]);
    let members: Vec<String> = (1..=3).map(|i| format!("m{i}-{run}")).collect();
    for member in &members {
        register_with(&app, member, manifest_with(json!({})), Some(&team)).await;
    }
    // m1: one leased turn and one queued message; m2/m3: one queued each.
    let m1_leased = send_one(&app, &members[0], json!({})).await;
    send_one(&app, &members[0], json!({})).await;
    send_one(&app, &members[1], json!({})).await;
    send_one(&app, &members[2], json!({})).await;
    let fence = activate_one(&app, &members[0], "host-1", 30_000).await;
    let turn = next_one(&app, &members[0], "host-1", fence).await;
    assert_eq!(turn["task_id"], json!(m1_leased));

    let (status, v) = call(&app, "POST", &format!("/teams/{team}/cancel"), None).await;
    assert_eq!(status, StatusCode::OK, "team cancel failed: {v}");
    let cancelled_members = v["members"].as_array().unwrap();
    assert_eq!(cancelled_members.len(), 3);
    for member in cancelled_members {
        assert!(member["exit_event"].is_string(), "member: {member}");
    }

    // The signalled holder reports the attempt as cancelled.
    let v = fail_turn(&app, &m1_leased, "host-1", "cancelled").await;
    assert_eq!(v["requeued"], json!(false));

    // Queue inspection across the whole tenant: no member message is
    // queued, leased, or retry-scheduled — every one terminal-cancelled.
    let (status, v) = call(&app, "GET", "/tasks", None).await;
    assert_eq!(status, StatusCode::OK);
    let tasks = v.as_array().unwrap();
    let recipients: Vec<String> = members.iter().map(|m| format!("agent:{m}")).collect();
    let member_tasks: Vec<&Value> = tasks
        .iter()
        .filter(|t| {
            t["recipient"]
                .as_str()
                .is_some_and(|r| recipients.iter().any(|m| m == r))
        })
        .collect();
    assert_eq!(member_tasks.len(), 4);
    for t in &member_tasks {
        assert_eq!(t["status"], json!("cancelled"), "orphan survived: {t}");
        assert_eq!(t["error_class"], json!("cancelled"), "orphan survived: {t}");
    }
}

/// The `update_agent` persistence roundtrip: supervision state written by
/// one app instance reads back through a *fresh* instance over the same
/// database — escalated latch, attempt history, journaled decision, and
/// the dead-lettered root escalation.
#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn pg_supervision_state_survives_a_fresh_store_instance() {
    let run = uniq();
    let tempy = format!("tempy-{run}");

    let app = postgres_app();
    register_with(
        &app,
        &tempy,
        manifest_with(json!({
            "supervision": policy("temporary", 5, None)
        })),
        None,
    )
    .await;
    let fence = activate_one(&app, &tempy, "host-1", 30_000).await;
    let task_id = send_one(&app, &tempy, json!({})).await;
    next_one(&app, &tempy, "host-1", fence).await;
    let v = fail_turn(&app, &task_id, "host-1", "transient").await;
    assert_eq!(v["escalation"]["kind"], json!("dead_letter"));
    drop(app);

    // A fresh app (fresh connection pool, no shared memory) over the same
    // database sees everything the episode wrote.
    let app = postgres_app();
    let v = supervision(&app, &tempy).await;
    assert_eq!(v["escalated"], json!(true));
    assert_eq!(v["attempts"].as_array().unwrap().len(), 1);
    assert_eq!(v["attempts"][0]["trigger"], json!("turn_failed"));
    let events = v["events"].as_array().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["output"]["value"]["decision"], json!("escalate"));

    let (status, v) = call(&app, "GET", "/tasks?status=dead", None).await;
    assert_eq!(status, StatusCode::OK);
    let dead: Vec<&Value> = v
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["payload"]["agent_id"] == json!(tempy))
        .collect();
    assert_eq!(dead.len(), 1);
    assert_eq!(dead[0]["kind"], json!("escalated"));
    assert_eq!(
        dead[0]["idempotency_key"],
        json!(format!("escalation:{tempy}:1"))
    );
}
