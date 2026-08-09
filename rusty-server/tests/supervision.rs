//! Agent Fabric wave 2 (R0.7) integration tests: supervision and the
//! cancellation tree, over the default JSON-file backend, driven
//! in-process via `tower::ServiceExt::oneshot` (the `agents.rs`
//! convention).
//!
//! The two wave-2 exit criteria live here:
//!
//! 1. **Crash-loop escalation** — an agent whose every turn fails
//!    escalates to its supervisor's *mailbox* once the restart budget is
//!    exhausted, with the full attempt history intact and the decision
//!    journaled (`crash_looping_agent_escalates_to_the_supervisor_with_history`).
//!    The loop is driven through the fail path rather than real SIGKILLs:
//!    supervision triggers on the durable failure *record*, not on the
//!    crash itself — the record is what survives one.
//! 2. **Team cancel leaves no orphans** — every member's queued,
//!    retry-scheduled, and leased mailbox traffic ends terminal
//!    (`team_cancel_leaves_zero_orphan_tasks`).
//!
//! Around them: the DLQ root default, the manual restart's latch reset,
//! agent-level deadline composition, per-agent cancel idempotency, and
//! tenant isolation. Live-Postgres coverage of the same semantics is
//! gated in `postgres_supervision.rs`.

use std::path::PathBuf;

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

const ACME: (&str, &str) = ("x-api-key", "acme-secret");
const GLOBEX: (&str, &str) = ("x-api-key", "globex-secret");

/// Unique temp store root, removed at the end of each test (best effort).
fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!(
        "rusty-server-supervision-test-{}",
        uuid::Uuid::new_v4()
    ))
}

/// Open-mode (single `default` tenant) app over a fresh store.
fn app() -> (Router, PathBuf) {
    let store = temp_store();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone());
    (router(GraphRegistry::new(), config), store)
}

/// Two-tenant app for the isolation test.
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

/// The base manifest (one accepted kind, `work`), with wave-2 fields
/// (`supervision`, `budget`, extra `accepts`) merged in per test.
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

/// A `SupervisionPolicy` wire object (60 s window everywhere: the tests
/// never leave it, so the intensity count is the whole history).
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

/// Activate an agent, asserting success; returns the fencing ordinal.
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

// --------------------------------------------------------------------- //
// Exit criterion 1: crash-loop escalation with attempt history
// --------------------------------------------------------------------- //

#[tokio::test]
async fn crash_looping_agent_escalates_to_the_supervisor_with_history() {
    let (app, store) = app();
    // The supervisor declares the escalation kind; the looper declares a
    // permanent policy with a restart budget of 2 per 60 s, pointed at it.
    register_with(
        &app,
        "boss",
        manifest_with(json!({
            "accepts": {"escalated": {"kind": "application/json"}}
        })),
        None,
    )
    .await;
    register_with(
        &app,
        "looper",
        manifest_with(json!({
            "supervision": policy("permanent", 2, Some("boss"))
        })),
        None,
    )
    .await;
    let looper_fence = activate_one(&app, "looper", "host-1", 60_000).await;
    let boss_fence = activate_one(&app, "boss", "host-boss", 60_000).await;

    // One crash-looping message: every attempt fails, the queue redelivers
    // it (full-jitter backoff capped at 1s/2s/4s for attempts 1/2/3), and
    // supervision counts every failure. Attempts 1 and 2 restart within
    // budget; attempt 3 exhausts it and escalates.
    let task_id = send_one(&app, "looper", json!({"max_attempts": 10})).await;
    let mut escalation_report = Value::Null;
    for (round, backoff_ms) in [(1u64, 1_100u64), (2, 2_100), (3, 4_100)] {
        let task = next_one(&app, "looper", "host-1", looper_fence).await;
        assert_eq!(task["task_id"], json!(task_id));
        assert_eq!(task["attempt"], json!(round));
        let v = fail_turn(&app, &task_id, "host-1", "transient").await;
        if round == 3 {
            escalation_report = v["escalation"].clone();
        } else {
            // Within the restart budget: no escalation, no evidence leak.
            assert_eq!(v["escalation"], Value::Null);
        }
        // Let the redelivery backoff elapse before the next round.
        tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
    }
    assert_eq!(escalation_report["kind"], json!("mailbox"));
    assert_eq!(escalation_report["deduplicated"], json!(false));

    // The escalation is a *message* in the supervisor's mailbox, carrying
    // the full attempt history of the episode.
    let escalation = next_one(&app, "boss", "host-boss", boss_fence).await;
    assert_eq!(escalation["kind"], json!("escalated"));
    assert_eq!(escalation["idempotency_key"], json!("escalation:looper:3"));
    let notice = &escalation["payload"];
    assert_eq!(notice["agent_id"], json!("looper"));
    assert_eq!(notice["policy"]["restart"], json!("permanent"));
    assert_eq!(notice["policy"]["intensity"], json!(2));
    assert_eq!(notice["policy"]["supervisor"], json!("boss"));
    assert!(notice["escalated_at"].is_string());
    let attempts = notice["attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 3);
    for (i, attempt) in attempts.iter().enumerate() {
        assert_eq!(attempt["ordinal"], json!(i as u64 + 1));
        assert_eq!(attempt["trigger"], json!("turn_failed"));
        assert_eq!(attempt["error_class"], json!("transient"));
        assert_eq!(attempt["task_id"], json!(task_id));
        assert!(attempt["message"].as_str().unwrap().contains("crashed"));
    }

    // The decision trail is journaled: restart, restart, escalate — with
    // the escalation event carrying the history too.
    let v = supervision(&app, "looper").await;
    assert_eq!(v["escalated"], json!(true));
    assert_eq!(v["suppressed_failures"], json!(0));
    assert_eq!(
        v["journal_run_id"],
        json!("agent-supervision:default:looper")
    );
    assert_eq!(v["attempts"].as_array().unwrap().len(), 3);
    let events = v["events"].as_array().unwrap();
    assert_eq!(events.len(), 3);
    for (event, decision) in events.iter().zip(["restart", "restart", "escalate"]) {
        assert_eq!(event["kind"], json!("supervision_event"));
        assert_eq!(event["status"], json!("ok"));
        assert_eq!(event["output"]["value"]["decision"], json!(decision));
        assert_eq!(event["output"]["value"]["trigger"], json!("turn_failed"));
    }
    assert_eq!(
        events[2]["output"]["value"]["attempts"]
            .as_array()
            .unwrap()
            .len(),
        3
    );

    // The latched escalation suppresses the rest of the crash loop:
    // counted, never re-escalated, never re-journaled — the supervisor's
    // mailbox is not flooded.
    let task = next_one(&app, "looper", "host-1", looper_fence).await;
    assert_eq!(task["attempt"], json!(4));
    let v = fail_turn(&app, &task_id, "host-1", "transient").await;
    assert_eq!(v["escalation"], Value::Null);
    let v = supervision(&app, "looper").await;
    assert_eq!(v["suppressed_failures"], json!(1));
    assert_eq!(v["events"].as_array().unwrap().len(), 3);
    let (status, _) = next(&app, "boss", "host-boss", boss_fence).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Exit criterion 2: team cancel leaves zero orphan tasks
// --------------------------------------------------------------------- //

#[tokio::test]
async fn team_cancel_leaves_zero_orphan_tasks() {
    let (app, store) = app();
    for id in ["m1", "m2", "m3"] {
        register_with(&app, id, manifest_with(json!({})), Some("squad-1")).await;
    }
    register_with(&app, "outsider", manifest_with(json!({})), None).await;
    // m1: one queued message and one leased turn; m2/m3: one queued each.
    let m1_leased = send_one(&app, "m1", json!({})).await;
    let m1_queued = send_one(&app, "m1", json!({})).await;
    let m2_msg = send_one(&app, "m2", json!({})).await;
    let m3_msg = send_one(&app, "m3", json!({})).await;
    let outsider_msg = send_one(&app, "outsider", json!({})).await;
    let fence = activate_one(&app, "m1", "host-1", 30_000).await;
    let turn = next_one(&app, "m1", "host-1", fence).await;
    assert_eq!(turn["task_id"], json!(m1_leased)); // oldest first

    let (status, v) = call(&app, "POST", "/teams/squad-1/cancel", None).await;
    assert_eq!(status, StatusCode::OK, "team cancel failed: {v}");
    assert_eq!(v["team_id"], json!("squad-1"));
    let members = v["members"].as_array().unwrap();
    assert_eq!(members.len(), 3);
    let by_id: std::collections::HashMap<&str, &Value> = members
        .iter()
        .map(|m| (m["agent_id"].as_str().unwrap(), m))
        .collect();
    assert_eq!(by_id["m1"]["cancelled"], json!([m1_queued]));
    assert_eq!(by_id["m1"]["signalled"], json!([m1_leased]));
    assert_eq!(by_id["m2"]["cancelled"], json!([m2_msg]));
    assert_eq!(by_id["m3"]["cancelled"], json!([m3_msg]));
    for member in members {
        assert!(
            member["exit_event"].is_string(),
            "member missing its AgentExit: {member}"
        );
    }

    // The signalled holder reports the attempt as cancelled through the
    // ordinary fail path (a cancel is a hint for promptness; the lease
    // protocol stays the settlement surface).
    let v = fail_turn(&app, &m1_leased, "host-1", "cancelled").await;
    assert_eq!(v["requeued"], json!(false));

    // Queue inspection: zero orphans across every member's mailbox — no
    // queued, no leased, no retry-scheduled message survives.
    let (status, v) = call(&app, "GET", "/tasks", None).await;
    assert_eq!(status, StatusCode::OK);
    let tasks = v.as_array().unwrap();
    let member_recipients = ["agent:m1", "agent:m2", "agent:m3"];
    let is_member = |t: &&Value| member_recipients.contains(&t["recipient"].as_str().unwrap_or(""));
    let orphans: Vec<&Value> = tasks
        .iter()
        .filter(is_member)
        .filter(|t| match t["status"].as_str().unwrap() {
            "queued" | "leased" => true,
            "failed" => t["next_attempt_at"].is_string(),
            _ => false,
        })
        .collect();
    assert!(
        orphans.is_empty(),
        "orphan tasks survived the team cancel: {orphans:?}"
    );
    for t in tasks.iter().filter(is_member) {
        assert_eq!(t["status"], json!("cancelled"), "member task: {t}");
        assert_eq!(t["error_class"], json!("cancelled"), "member task: {t}");
    }
    // The non-team agent's message is untouched.
    let outsider = tasks
        .iter()
        .find(|t| t["task_id"] == json!(outsider_msg))
        .unwrap();
    assert_eq!(outsider["status"], json!("queued"));

    // Every member's exit is journaled exactly once — for unsupervised
    // agents too: cancellation is an operator action, not a policy matter.
    for id in ["m1", "m2", "m3"] {
        let v = supervision(&app, id).await;
        let events = v["events"].as_array().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["kind"], json!("agent_exit"));
        assert_eq!(
            events[0]["output"]["value"]["disposition"],
            json!("cancelled")
        );
    }
    // A repeated cancel of a quiescent team is a no-op and journals nothing.
    let (status, v) = call(&app, "POST", "/teams/squad-1/cancel", None).await;
    assert_eq!(status, StatusCode::OK, "re-cancel failed: {v}");
    for member in v["members"].as_array().unwrap() {
        assert_eq!(member["cancelled"], json!([]));
        assert_eq!(member["signalled"], json!([]));
        assert_eq!(member["exit_event"], Value::Null);
    }
    let v = supervision(&app, "m1").await;
    assert_eq!(v["events"].as_array().unwrap().len(), 1);
    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// The root default: temporary policy, no supervisor → DLQ
// --------------------------------------------------------------------- //

#[tokio::test]
async fn temporary_root_agent_escalates_straight_to_the_dlq() {
    let (app, store) = app();
    // temporary never restarts; with no supervisor declared the first
    // failure already lands at the root default.
    register_with(
        &app,
        "tempy",
        manifest_with(json!({
            "supervision": policy("temporary", 5, None)
        })),
        None,
    )
    .await;
    let fence = activate_one(&app, "tempy", "host-1", 30_000).await;
    let task_id = send_one(&app, "tempy", json!({})).await;
    let task = next_one(&app, "tempy", "host-1", fence).await;
    assert_eq!(task["task_id"], json!(task_id));

    let v = fail_turn(&app, &task_id, "host-1", "transient").await;
    assert_eq!(v["escalation"]["kind"], json!("dead_letter"));
    let dead_letter_id = v["escalation"]["task_id"].as_str().unwrap().to_string();

    // Open question 2's chosen default: the notice is a DLQ record with
    // the full evidence chain attached (`GET /tasks?status=dead` is the
    // operator's surface).
    let (status, v) = call(&app, "GET", "/tasks?status=dead", None).await;
    assert_eq!(status, StatusCode::OK);
    let entries = v.as_array().unwrap();
    assert_eq!(entries.len(), 1);
    let dead = &entries[0];
    assert_eq!(dead["task_id"], json!(dead_letter_id));
    assert_eq!(dead["status"], json!("dead"));
    assert_eq!(dead["kind"], json!("escalated"));
    assert_eq!(dead["idempotency_key"], json!("escalation:tempy:1"));
    assert_eq!(dead["payload"]["agent_id"], json!("tempy"));
    assert_eq!(dead["payload"]["attempts"].as_array().unwrap().len(), 1);
    assert!(dead["last_error"]
        .as_str()
        .unwrap()
        .contains("dead-lettered"));

    let v = supervision(&app, "tempy").await;
    assert_eq!(v["escalated"], json!(true));
    let events = v["events"].as_array().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["output"]["value"]["decision"], json!("escalate"));
    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// The operator's reset: manual restart
// --------------------------------------------------------------------- //

#[tokio::test]
async fn manual_restart_journals_and_clears_the_latches() {
    let (app, store) = app();
    // The operator outranks the declaration: restart works with no policy.
    register_with(&app, "plain", manifest_with(json!({})), None).await;
    let (status, v) = call(
        &app,
        "POST",
        "/agents/plain/restart",
        Some(json!({"reason": "patched the prompt"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "restart failed: {v}");
    assert_eq!(v["restarted"], json!(true));
    assert_eq!(v["restart_ordinal"], json!(1));
    assert!(v["event"].is_string());
    let v = supervision(&app, "plain").await;
    let events = v["events"].as_array().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["output"]["value"]["decision"], json!("restart"));
    assert_eq!(
        events[0]["output"]["value"]["trigger"],
        json!("manual_restart")
    );
    assert_eq!(
        events[0]["output"]["value"]["message"],
        json!("patched the prompt")
    );

    // Crash an intensity-1 agent into escalation (restart once, escalate
    // on the second failure), then reset it.
    register_with(
        &app,
        "flaky",
        manifest_with(json!({
            "supervision": policy("permanent", 1, None)
        })),
        None,
    )
    .await;
    let fence = activate_one(&app, "flaky", "host-1", 60_000).await;
    let task_id = send_one(&app, "flaky", json!({"max_attempts": 10})).await;
    next_one(&app, "flaky", "host-1", fence).await;
    fail_turn(&app, &task_id, "host-1", "transient").await;
    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
    next_one(&app, "flaky", "host-1", fence).await;
    fail_turn(&app, &task_id, "host-1", "transient").await;
    let v = supervision(&app, "flaky").await;
    assert_eq!(v["escalated"], json!(true));

    let (status, _) = call(&app, "POST", "/agents/flaky/restart", None).await;
    assert_eq!(status, StatusCode::OK, "restart failed: {v}");
    let v = supervision(&app, "flaky").await;
    assert_eq!(v["escalated"], json!(false));
    assert_eq!(v["deadline_breached"], json!(false));
    assert_eq!(v["suppressed_failures"], json!(0));
    // The history is kept (ordinal 3 is the manual restart); only the
    // latches clear.
    assert_eq!(v["attempts"].as_array().unwrap().len(), 3);
    assert_eq!(v["events"].as_array().unwrap().len(), 3);

    // A new failure is decided afresh — never suppressed by the cleared
    // latch. The window still holds the earlier crashes, so intensity 1
    // escalates again, under the new episode's own idempotency key.
    tokio::time::sleep(std::time::Duration::from_millis(2_100)).await;
    next_one(&app, "flaky", "host-1", fence).await;
    let v = fail_turn(&app, &task_id, "host-1", "transient").await;
    assert_eq!(v["escalation"]["kind"], json!("dead_letter"));
    let v = supervision(&app, "flaky").await;
    assert_eq!(v["escalated"], json!(true));
    assert_eq!(v["suppressed_failures"], json!(0));
    assert_eq!(v["events"].as_array().unwrap().len(), 4);
    let (status, v) = call(&app, "GET", "/tasks?status=dead", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v.as_array().unwrap().len(), 2);
    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// The agent-level deadline: cancellation by clock, supervised once
// --------------------------------------------------------------------- //

#[tokio::test]
async fn agent_deadline_breach_cancels_then_supervises_once() {
    let (app, store) = app();
    register_with(
        &app,
        "clocked",
        manifest_with(json!({
            "budget": {"deadline": "2020-01-01T00:00:00Z"},
            "supervision": policy("permanent", 3, None)
        })),
        None,
    )
    .await;
    let fence = activate_one(&app, "clocked", "host-1", 30_000).await;
    let task_id = send_one(&app, "clocked", json!({})).await;

    // The first claim past the whole-activity deadline: the outstanding
    // message is cancelled (children before parent), the policy decides
    // over the wreckage, and the claim answers empty.
    let (status, _) = next(&app, "clocked", "host-1", fence).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, v) = call(&app, "GET", &format!("/tasks/{task_id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["status"], json!("cancelled"));
    assert_eq!(v["error_class"], json!("cancelled"));

    let v = supervision(&app, "clocked").await;
    assert_eq!(v["deadline_breached"], json!(true));
    let events = v["events"].as_array().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0]["output"]["value"]["trigger"],
        json!("deadline_breached")
    );
    // permanent restarts after any termination class, the clock included.
    assert_eq!(events[0]["output"]["value"]["decision"], json!("restart"));
    assert_eq!(events[0]["output"]["value"]["error_class"], Value::Null);

    // The breach is latched: further claims answer empty without
    // re-cancelling or re-journaling.
    let (status, _) = next(&app, "clocked", "host-1", fence).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let v = supervision(&app, "clocked").await;
    assert_eq!(v["events"].as_array().unwrap().len(), 1);
    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Per-agent cancel: semantics and idempotency
// --------------------------------------------------------------------- //

#[tokio::test]
async fn agent_cancel_is_idempotent_and_journals_one_exit() {
    let (app, store) = app();
    register_with(&app, "x", manifest_with(json!({})), None).await;
    let first = send_one(&app, "x", json!({})).await;
    let second = send_one(&app, "x", json!({})).await;

    let (status, v) = call(&app, "POST", "/agents/x/cancel", None).await;
    assert_eq!(status, StatusCode::OK, "cancel failed: {v}");
    assert_eq!(v["agent_id"], json!("x"));
    let mut cancelled: Vec<String> = v["cancelled"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t.as_str().unwrap().to_string())
        .collect();
    cancelled.sort();
    let mut expected = vec![first, second];
    expected.sort();
    assert_eq!(cancelled, expected);
    assert_eq!(v["signalled"], json!([]));
    assert!(v["exit_event"].is_string());

    // Terminal-cancelled with the class recorded — never requeued, never
    // dead-lettered.
    let (status, v) = call(&app, "GET", "/tasks?status=cancelled", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v.as_array().unwrap().len(), 2);

    // Idempotent: the repeated cancel touches nothing and journals nothing.
    let (status, v) = call(&app, "POST", "/agents/x/cancel", None).await;
    assert_eq!(status, StatusCode::OK, "re-cancel failed: {v}");
    assert_eq!(v["cancelled"], json!([]));
    assert_eq!(v["signalled"], json!([]));
    assert_eq!(v["exit_event"], Value::Null);
    let v = supervision(&app, "x").await;
    let events = v["events"].as_array().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["kind"], json!("agent_exit"));
    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Tenant isolation
// --------------------------------------------------------------------- //

#[tokio::test]
async fn wave_2_endpoints_are_tenant_isolated() {
    let (app, store) = multi_tenant_app();
    let (status, v) = call_as(
        &app,
        Some(ACME),
        "POST",
        "/agents",
        Some(json!({
            "agent_id": "acme-agent",
            "manifest": manifest_with(json!({})),
            "team_id": "acme-team"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "register failed: {v}");

    // Unknown and cross-tenant stay deliberately indistinguishable: 404.
    for (method, uri) in [
        ("GET", "/agents/acme-agent/supervision"),
        ("POST", "/agents/acme-agent/cancel"),
        ("POST", "/agents/acme-agent/restart"),
        ("POST", "/teams/acme-team/cancel"),
    ] {
        let (status, _) = call_as(&app, Some(GLOBEX), method, uri, None).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{method} {uri} must be tenant-isolated"
        );
    }
    // …and well-formed for the owning tenant.
    let (status, _) = call_as(
        &app,
        Some(ACME),
        "GET",
        "/agents/acme-agent/supervision",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call_as(&app, Some(ACME), "POST", "/teams/acme-team/cancel", None).await;
    assert_eq!(status, StatusCode::OK);
    let _ = std::fs::remove_dir_all(store);
}
