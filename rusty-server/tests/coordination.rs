//! Agent Fabric wave 3 (R0.7) integration tests: the four coordination
//! patterns — delegate, fan-out, race, quorum — over the default JSON-file
//! backend, driven in-process via `tower::ServiceExt::oneshot` (the
//! `supervision.rs` convention).
//!
//! The wave-3 exit criteria live here: every pattern's guarantee is
//! exercised end to end, and every pattern is crashed mid-flight:
//!
//! - **delegate** — happy path with the full causal chain
//!   (`CoordinationStart` → `MailboxSend` → `MailboxReceive` →
//!   `CoordinationEnd` → outcome message), a member crash mid-turn
//!   (lease lapse, re-claim, settle), and a deadline expiry that only the
//!   reconcile-on-read drive can settle.
//! - **fan-out** — the in-flight window as backpressure, byte-deterministic
//!   merge in member task-id order, partial failure (the merge keeps the
//!   survivors, the missing member is journaled), and fail-fast (one
//!   terminal failure cancel-signals the rest).
//! - **race** — the submission-time effect gate (a candidate that is not
//!   freely repeatable is a 400 before any write), first-completion-wins
//!   with waste accounting, and all-candidates-failed dead-lettering.
//! - **quorum** — majority resolution over the first k with a crashed
//!   juror, determinism across repeated reads, threshold unreachability
//!   (fail open, never downgrade k), and the contract 400s.
//!
//! Around them: the TeamTrace read, restart durability, submission
//! deduplication, submission validation, and tenant isolation. Member
//! tasks are outbox-submitted by the runtime, so the app polls the relay
//! at 50 ms and tests wait for publish before claiming (the `outbox.rs`
//! convention). Live-Postgres coverage is gated in
//! `postgres_coordination.rs`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

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
        "rusty-server-coordination-test-{}",
        uuid::Uuid::new_v4()
    ))
}

/// Open-mode (single `default` tenant) app over a fresh store, relay
/// polling every 50 ms so outbox-submitted member tasks land quickly.
fn app() -> (Router, PathBuf) {
    app_over(temp_store())
}

/// Open-mode app over an existing store root (the restart-durability test).
fn app_over(store: PathBuf) -> (Router, PathBuf) {
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_outbox_relay_interval(Duration::from_millis(50));
    (router(GraphRegistry::new(), config), store)
}

/// Two-tenant app for the isolation test.
fn multi_tenant_app() -> (Router, PathBuf) {
    let store = temp_store();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_outbox_relay_interval(Duration::from_millis(50))
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

/// The member manifest: accepts the `work` kind, declares both state
/// scopes (so a narrowing context grant is legal), `worker/1.0.0` — the
/// version every test delegation pins.
fn member_manifest() -> Value {
    json!({
        "agent_kind": "worker",
        "manifest_version": "worker/1.0.0",
        "accepts": {"work": {"kind": "application/json"}},
        "scopes": ["private", "team"]
    })
}

/// The delegator manifest: additionally accepts the reserved
/// `coordination_result` kind, without which a delegation is a 400.
fn delegator_manifest() -> Value {
    json!({
        "agent_kind": "delegator",
        "manifest_version": "delegator/1.0.0",
        "accepts": {"coordination_result": {"kind": "application/json"}}
    })
}

/// Register an agent; asserts 201.
async fn register(app: &Router, agent_id: &str, manifest: Value) {
    register_as(app, None, agent_id, manifest).await;
}

/// Register an agent under a tenant; asserts 201.
async fn register_as(app: &Router, auth: Option<(&str, &str)>, agent_id: &str, manifest: Value) {
    let (status, v) = call_as(
        app,
        auth,
        "POST",
        "/agents",
        Some(json!({"agent_id": agent_id, "manifest": manifest})),
    )
    .await;
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

/// One mailbox claim attempt with an explicit task lease; `(status, body)`.
async fn next_with_lease(
    app: &Router,
    agent_id: &str,
    worker: &str,
    fencing: u64,
    lease_ms: u64,
) -> (StatusCode, Value) {
    call(
        app,
        "POST",
        &format!("/agents/{agent_id}/mailbox/next"),
        Some(json!({"worker_id": worker, "fencing": fencing, "lease_ms": lease_ms})),
    )
    .await
}

/// Poll `GET /tasks/{id}` until the relay has published the task (or fail
/// after 5 s — the relay polls every 50 ms, so this is generous).
async fn wait_task(app: &Router, task_id: &str) -> Value {
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

/// Poll the mailbox claim until `task_id` is leased to this worker (or
/// fail after 5 s). Member tasks arrive through the relay, so the first
/// claims may answer 204.
async fn claim_task(
    app: &Router,
    agent_id: &str,
    worker: &str,
    fencing: u64,
    task_id: &str,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let (status, v) = next_with_lease(app, agent_id, worker, fencing, 30_000).await;
        if status == StatusCode::OK {
            assert_eq!(
                v["task"]["task_id"],
                json!(task_id),
                "claimed an unexpected task: {v}"
            );
            return v["task"].clone();
        }
        assert!(
            Instant::now() < deadline,
            "task `{task_id}` never became claimable for `{agent_id}`"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Complete the held task; asserts 200. `extra` carries settlement cost
/// evidence (`tokens`, `cost_usd`) when the test reports any.
async fn complete_task(app: &Router, task_id: &str, worker: &str, result: Value, extra: Value) {
    let mut body = json!({"worker_id": worker, "result": result});
    body.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    let (status, v) = call(
        app,
        "POST",
        &format!("/tasks/{task_id}/complete"),
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "complete failed: {v}");
}

/// Fail the held task; asserts 200 and returns the body.
async fn fail_task(
    app: &Router,
    task_id: &str,
    worker: &str,
    error_class: &str,
    retryable: bool,
    extra: Value,
) -> Value {
    let mut body = json!({"worker_id": worker, "error_class": error_class,
                "message": format!("turn failed ({error_class})"), "retryable": retryable});
    body.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    let (status, v) = call(app, "POST", &format!("/tasks/{task_id}/fail"), Some(body)).await;
    assert_eq!(status, StatusCode::OK, "fail failed: {v}");
    v
}

/// Fetch the coordination record (reconciling on read); asserts 200.
async fn get_coordination(app: &Router, coordination_id: &str) -> Value {
    let (status, v) = call(
        app,
        "GET",
        &format!("/coordination/{coordination_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "get coordination failed: {v}");
    v
}

/// A `Delegation` wire object pinning `worker/1.0.0` and the `work` kind;
/// `extra` merges per-test fields (`effect`, `deadline`).
fn delegation(member: &str, agent_id: &str, input: Value, extra: Value) -> Value {
    let mut d = json!({
        "member": member,
        "agent_id": agent_id,
        "manifest_version": "worker/1.0.0",
        "kind": "work",
        "input": {"kind": "inline", "value": input}
    });
    d.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    d
}

/// Submit a pattern; returns `(status, body)` — no assertion, the 400
/// tests need the raw pair.
async fn submit(app: &Router, path: &str, body: Value) -> (StatusCode, Value) {
    call(app, "POST", &format!("/coordination/{path}"), Some(body)).await
}

/// Submit a pattern, asserting 201; returns the body.
async fn submit_created(app: &Router, path: &str, body: Value) -> Value {
    let (status, v) = submit(app, path, body).await;
    assert_eq!(status, StatusCode::CREATED, "submit failed: {v}");
    v
}

/// The journal events of a settled-or-open coordination, asserted present.
fn events(record: &Value) -> &Vec<Value> {
    record["journal"]["events"]
        .as_array()
        .expect("a driven coordination always has a journal")
}

/// Count the journal events of one kind.
fn kind_count(record: &Value, kind: &str) -> usize {
    events(record)
        .iter()
        .filter(|e| e["kind"] == json!(kind))
        .count()
}

/// The deterministic member task id in the open-mode (`default`) tenant.
fn member_task_id(coordination_id: &str, member: &str) -> String {
    format!("default--{coordination_id}--{member}")
}

// --------------------------------------------------------------------- //
// delegate
// --------------------------------------------------------------------- //

#[tokio::test]
async fn delegate_happy_path_journals_the_full_causal_chain() {
    let (app, store) = app();
    register(&app, "boss", delegator_manifest()).await;
    register(&app, "writer", member_manifest()).await;

    let created = submit_created(
        &app,
        "delegate",
        json!({
            "coordination_id": "d-1",
            "delegator": "boss",
            "delegate": {
                "delegate": delegation("solo", "writer", json!({"brief": "q3 summary"}),
                                       json!({"effect": "idempotent"})),
                "context": {"scopes": ["private"], "channels": ["thread:team-7"]},
                "handoff": true
            }
        }),
    )
    .await;
    assert_eq!(created["coordination_id"], json!("d-1"));
    assert_eq!(created["start_event"], json!("coordination:default:d-1:0"));
    assert_eq!(
        created["submitted"],
        json!([{"member": "solo", "task_id": "default--d-1--solo"}])
    );

    // The member task is the derived-id, derived-key coordination message.
    let task = wait_task(&app, "default--d-1--solo").await;
    assert_eq!(task["recipient"], json!("agent:writer"));
    assert_eq!(task["kind"], json!("work"));
    assert_eq!(task["idempotency_key"], json!("coordination:d-1:solo"));
    assert_eq!(task["effect"], json!("idempotent"));
    let message = &task["payload"];
    assert_eq!(message["coordination_id"], json!("d-1"));
    assert_eq!(message["member"], json!("solo"));
    assert_eq!(message["pattern"], json!("delegate"));
    assert_eq!(
        message["input"],
        json!({"kind": "inline", "value": {"brief": "q3 summary"}})
    );
    assert_eq!(
        message["context"],
        json!({"scopes": ["private"], "channels": ["thread:team-7"]})
    );

    // Run the turn and settle it with cost evidence.
    let fence = activate_one(&app, "writer", "host-w", 60_000).await;
    let turn = claim_task(&app, "writer", "host-w", fence, "default--d-1--solo").await;
    assert_eq!(turn["attempt"], json!(1));
    complete_task(
        &app,
        "default--d-1--solo",
        "host-w",
        json!({"draft": "revenue up 12%"}),
        json!({"tokens": {"prompt_tokens": 100, "completion_tokens": 50, "total_tokens": 150},
               "cost_usd": 0.01}),
    )
    .await;

    // The pattern settled on the settlement hook's drive.
    let record = get_coordination(&app, "d-1").await;
    assert_eq!(record["settled"], json!(true));
    let outcome = &record["outcome"];
    assert_eq!(outcome["coordination_id"], json!("d-1"));
    assert_eq!(outcome["pattern"], json!("delegate"));
    assert_eq!(outcome["status"], json!("completed"));
    assert_eq!(
        outcome["result"],
        json!({"kind": "inline", "value": {"draft": "revenue up 12%"}})
    );
    // The contributing member is not waste.
    assert!(outcome["wasted_tokens"].is_null());
    assert!(outcome["wasted_cost_usd"].is_null());
    assert_eq!(
        outcome["members"],
        json!([{
            "member": "solo",
            "task_id": "default--d-1--solo",
            "settlement": "completed",
            "result": {"kind": "inline", "value": {"draft": "revenue up 12%"}},
            "tokens": {"prompt_tokens": 100, "completion_tokens": 50, "total_tokens": 150},
            "cost_usd": 0.01
        }])
    );

    // The causal chain: start → send → receive → end, parented
    // start/send/start respectively.
    let events = events(&record);
    assert_eq!(events.len(), 4);
    let kinds: Vec<&str> = events.iter().map(|e| e["kind"].as_str().unwrap()).collect();
    assert_eq!(
        kinds,
        [
            "coordination_start",
            "mailbox_send",
            "mailbox_receive",
            "coordination_end"
        ]
    );
    assert!(events[0]["parent"].is_null());
    assert_eq!(events[1]["parent"], events[0]["id"]);
    assert_eq!(events[2]["parent"], events[1]["id"]);
    assert_eq!(events[3]["parent"], events[0]["id"]);
    // The member task hangs off the send event; the outcome task off the end.
    assert_eq!(task["parent"], events[1]["id"]);
    assert_eq!(
        events[0]["output"]["value"]["contract"]["pattern"],
        json!("delegate")
    );
    assert_eq!(events[2]["status"], json!("ok"));
    assert_eq!(
        events[2]["output"]["value"]["settlement"],
        json!("completed")
    );
    assert_eq!(events[3]["output"]["value"]["status"], json!("completed"));

    // The outcome is one `coordination_result` message in the delegator's
    // mailbox, correlated by its deterministic task id.
    let outcome_task = wait_task(&app, "default--d-1--outcome").await;
    assert_eq!(outcome_task["recipient"], json!("agent:boss"));
    assert_eq!(outcome_task["kind"], json!("coordination_result"));
    assert_eq!(
        outcome_task["idempotency_key"],
        json!("coordination:d-1:outcome")
    );
    assert_eq!(outcome_task["parent"], events[3]["id"]);
    assert_eq!(outcome_task["payload"], *outcome);

    let boss_fence = activate_one(&app, "boss", "host-b", 60_000).await;
    let delivered = claim_task(&app, "boss", "host-b", boss_fence, "default--d-1--outcome").await;
    assert_eq!(delivered["payload"]["status"], json!("completed"));
    complete_task(
        &app,
        "default--d-1--outcome",
        "host-b",
        json!({"ack": true}),
        json!({}),
    )
    .await;

    let _ = std::fs::remove_dir_all(store);
}

/// Exit-criterion crash coverage, delegate: the member crashes mid-turn
/// (claimed, never settled, lease lapses); the queue re-leases the turn
/// and the retried attempt settles the pattern.
#[tokio::test]
async fn delegate_member_crash_mid_turn_recovers_and_settles() {
    let (app, store) = app();
    register(&app, "boss", delegator_manifest()).await;
    register(&app, "writer", member_manifest()).await;
    submit_created(
        &app,
        "delegate",
        json!({
            "coordination_id": "d-crash",
            "delegator": "boss",
            "delegate": {
                "delegate": delegation("solo", "writer", json!({"n": 1}),
                                       json!({"effect": "idempotent"}))
            }
        }),
    )
    .await;
    wait_task(&app, "default--d-crash--solo").await;

    // Attempt 1: claimed with a 100 ms lease, then the "crash" — no
    // settle, no heartbeat. The lease lapse is the crash signal; the
    // durable failure record is what survives one (the supervision
    // convention: crash semantics are lease semantics).
    let fence = activate_one(&app, "writer", "host-w", 60_000).await;
    let (status, v) = next_with_lease(&app, "writer", "host-w", fence, 100).await;
    assert_eq!(status, StatusCode::OK, "first claim failed: {v}");
    assert_eq!(v["task"]["attempt"], json!(1));
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Attempt 2: the lapsed lease makes the same turn claimable again.
    let retry = claim_task(&app, "writer", "host-w", fence, "default--d-crash--solo").await;
    assert_eq!(retry["attempt"], json!(2));
    complete_task(
        &app,
        "default--d-crash--solo",
        "host-w",
        json!({"recovered": true}),
        json!({}),
    )
    .await;

    let record = get_coordination(&app, "d-crash").await;
    assert_eq!(record["settled"], json!(true));
    assert_eq!(record["outcome"]["status"], json!("completed"));
    assert_eq!(
        record["outcome"]["result"],
        json!({"kind": "inline", "value": {"recovered": true}})
    );
    // Exactly one settlement observation, from the surviving attempt.
    assert_eq!(kind_count(&record, "mailbox_receive"), 1);

    // The delegator still receives the outcome.
    let outcome_task = wait_task(&app, "default--d-crash--outcome").await;
    assert_eq!(outcome_task["payload"]["status"], json!("completed"));
    let _ = std::fs::remove_dir_all(store);
}

/// The member's deadline expires unclaimed: the claim-path finalization
/// cancels the task, and only the reconcile-on-read drive can settle the
/// pattern (no route hook fires on the claim path for this pattern).
#[tokio::test]
async fn delegate_deadline_expiry_settles_cancelled_on_reconcile() {
    let (app, store) = app();
    register(&app, "writer", member_manifest()).await;
    let deadline = (chrono::Utc::now() + chrono::Duration::milliseconds(300)).to_rfc3339();
    // No delegator: a control-plane submission observed through GET alone.
    submit_created(
        &app,
        "delegate",
        json!({
            "coordination_id": "d-deadline",
            "delegate": {
                "delegate": delegation("solo", "writer", json!({"n": 1}),
                                       json!({"deadline": deadline}))
            }
        }),
    )
    .await;
    let task = wait_task(&app, "default--d-deadline--solo").await;
    assert!(task["deadline"].is_string());
    tokio::time::sleep(Duration::from_millis(500)).await;

    // The claim-path finalization sweep turns the expired task
    // terminal-cancelled instead of leasing it; the claim answers empty.
    let fence = activate_one(&app, "writer", "host-w", 60_000).await;
    let (status, _) = next_with_lease(&app, "writer", "host-w", fence, 30_000).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let task = wait_task(&app, "default--d-deadline--solo").await;
    assert_eq!(task["status"], json!("cancelled"));

    // Reconcile-on-read: the GET drives the pattern to its settle.
    let record = get_coordination(&app, "d-deadline").await;
    assert_eq!(record["settled"], json!(true));
    assert_eq!(record["outcome"]["status"], json!("cancelled"));
    assert!(record["outcome"]["result"].is_null());
    assert_eq!(
        record["outcome"]["members"][0]["settlement"],
        json!("cancelled")
    );
    let events = events(&record);
    assert_eq!(events.len(), 4);
    assert_eq!(events[2]["status"], json!("error"));
    assert_eq!(
        events[2]["output"]["value"]["settlement"],
        json!("cancelled")
    );
    // No delegator, so no outcome message exists.
    let (status, _) = call(&app, "GET", "/tasks/default--d-deadline--outcome", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// fan-out
// --------------------------------------------------------------------- //

/// The window is the backpressure guarantee (two in flight until one
/// settles), and the merge is byte-deterministic in member task-id order —
/// never completion order.
#[tokio::test]
async fn fan_out_window_backpressure_and_deterministic_merge() {
    let (app, store) = app();
    for agent in ["fa", "fb", "fc", "fd"] {
        register(&app, agent, member_manifest()).await;
    }
    // Declaration order is deliberately NOT task-id order: the merge must
    // sort, not follow the contract.
    let created = submit_created(
        &app,
        "fan_out",
        json!({
            "coordination_id": "fo-1",
            "fan_out": {
                "members": [
                    delegation("b", "fb", json!({"r": "B"}), json!({})),
                    delegation("a", "fa", json!({"r": "A"}), json!({})),
                    delegation("d", "fd", json!({"r": "D"}), json!({})),
                    delegation("c", "fc", json!({"r": "C"}), json!({}))
                ],
                "max_in_flight": 2,
                "on_member_failure": "partial"
            }
        }),
    )
    .await;
    // The initial window: exactly the first two declared members.
    assert_eq!(
        created["submitted"],
        json!([
            {"member": "b", "task_id": "default--fo-1--b"},
            {"member": "a", "task_id": "default--fo-1--a"}
        ])
    );
    wait_task(&app, "default--fo-1--b").await;
    wait_task(&app, "default--fo-1--a").await;
    let record = get_coordination(&app, "fo-1").await;
    let submitted: std::collections::HashMap<String, bool> = record["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["member"].as_str().unwrap().to_string(),
                m["submitted"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        submitted,
        std::collections::HashMap::from([
            ("b".to_string(), true),
            ("a".to_string(), true),
            ("d".to_string(), false),
            ("c".to_string(), false),
        ])
    );

    // Settle b: the window opens for d (the next declared member).
    let fb = activate_one(&app, "fb", "host-b", 60_000).await;
    claim_task(&app, "fb", "host-b", fb, "default--fo-1--b").await;
    complete_task(
        &app,
        "default--fo-1--b",
        "host-b",
        json!({"r": "B"}),
        json!({}),
    )
    .await;
    wait_task(&app, "default--fo-1--d").await;
    let record = get_coordination(&app, "fo-1").await;
    assert_eq!(record["members"][2]["submitted"], json!(true));
    assert_eq!(record["members"][3]["submitted"], json!(false));
    assert_eq!(record["settled"], json!(false));

    // Settle a, d, c — completion order b, a, d, c.
    let fa = activate_one(&app, "fa", "host-a", 60_000).await;
    claim_task(&app, "fa", "host-a", fa, "default--fo-1--a").await;
    complete_task(
        &app,
        "default--fo-1--a",
        "host-a",
        json!({"r": "A"}),
        json!({}),
    )
    .await;
    wait_task(&app, "default--fo-1--c").await;
    let fd = activate_one(&app, "fd", "host-d", 60_000).await;
    claim_task(&app, "fd", "host-d", fd, "default--fo-1--d").await;
    complete_task(
        &app,
        "default--fo-1--d",
        "host-d",
        json!({"r": "D"}),
        json!({}),
    )
    .await;
    let fc = activate_one(&app, "fc", "host-c", 60_000).await;
    claim_task(&app, "fc", "host-c", fc, "default--fo-1--c").await;
    complete_task(
        &app,
        "default--fo-1--c",
        "host-c",
        json!({"r": "C"}),
        json!({}),
    )
    .await;

    // The merge follows member task-id order (a, b, c, d), byte-exact —
    // completion order (b, a, d, c) and declaration order (b, a, d, c)
    // would both produce a different array.
    let record = get_coordination(&app, "fo-1").await;
    assert_eq!(record["settled"], json!(true));
    assert_eq!(record["outcome"]["status"], json!("completed"));
    assert_eq!(
        record["outcome"]["result"],
        json!({"kind": "inline", "value": [{"r": "A"}, {"r": "B"}, {"r": "C"}, {"r": "D"}]})
    );
    // Dispositions are in contract declaration order, every member present.
    let members = record["outcome"]["members"].as_array().unwrap();
    let names: Vec<&str> = members
        .iter()
        .map(|m| m["member"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["b", "a", "d", "c"]);
    for member in members {
        assert_eq!(member["settlement"], json!("completed"));
    }
    // 1 start + 4 sends + 4 receives + 1 end.
    assert_eq!(kind_count(&record, "coordination_start"), 1);
    assert_eq!(kind_count(&record, "mailbox_send"), 4);
    assert_eq!(kind_count(&record, "mailbox_receive"), 4);
    assert_eq!(kind_count(&record, "coordination_end"), 1);

    // Reconcile-on-read is convergent: a second read is byte-identical.
    let again = get_coordination(&app, "fo-1").await;
    assert_eq!(again["outcome"], record["outcome"]);
    assert_eq!(
        again["journal"]["events"].as_array().unwrap().len(),
        events(&record).len()
    );
    let _ = std::fs::remove_dir_all(store);
}

/// Exit-criterion crash coverage, fan-out partial: one member dies
/// terminally; the merge keeps the survivors and the missing member is
/// journaled evidence, never silent.
#[tokio::test]
async fn fan_out_partial_failure_merges_survivors_and_journal_the_missing() {
    let (app, store) = app();
    for agent in ["m1", "m2", "m3"] {
        register(&app, agent, member_manifest()).await;
    }
    submit_created(
        &app,
        "fan_out",
        json!({
            "coordination_id": "fo-partial",
            "fan_out": {
                "members": [
                    delegation("x", "m1", json!({"r": 1}), json!({})),
                    delegation("y", "m2", json!({"r": 2}), json!({})),
                    delegation("z", "m3", json!({"r": 3}), json!({}))
                ],
                "max_in_flight": 3,
                "on_member_failure": "partial"
            }
        }),
    )
    .await;
    for member in ["x", "y", "z"] {
        wait_task(&app, &member_task_id("fo-partial", member)).await;
    }

    // m3's worker dies for good: terminal failure, no retry.
    let f3 = activate_one(&app, "m3", "host-3", 60_000).await;
    claim_task(&app, "m3", "host-3", f3, "default--fo-partial--z").await;
    fail_task(
        &app,
        "default--fo-partial--z",
        "host-3",
        "unknown",
        false,
        json!({}),
    )
    .await;
    // The partial policy keeps the pattern open for the survivors.
    let record = get_coordination(&app, "fo-partial").await;
    assert_eq!(record["settled"], json!(false));

    let f1 = activate_one(&app, "m1", "host-1", 60_000).await;
    claim_task(&app, "m1", "host-1", f1, "default--fo-partial--x").await;
    complete_task(
        &app,
        "default--fo-partial--x",
        "host-1",
        json!({"r": 1}),
        json!({}),
    )
    .await;
    let f2 = activate_one(&app, "m2", "host-2", 60_000).await;
    claim_task(&app, "m2", "host-2", f2, "default--fo-partial--y").await;
    complete_task(
        &app,
        "default--fo-partial--y",
        "host-2",
        json!({"r": 2}),
        json!({}),
    )
    .await;

    let record = get_coordination(&app, "fo-partial").await;
    assert_eq!(record["settled"], json!(true));
    assert_eq!(record["outcome"]["status"], json!("completed"));
    // The merge carries only completed members, in task-id order.
    assert_eq!(
        record["outcome"]["result"],
        json!({"kind": "inline", "value": [{"r": 1}, {"r": 2}]})
    );
    // The failed member is journaled with its error evidence — the
    // missing member is in `members`, which is where partial-failure
    // evidence belongs.
    let z = &record["outcome"]["members"][2];
    assert_eq!(z["member"], json!("z"));
    assert_eq!(z["settlement"], json!("failed"));
    assert_eq!(z["error_class"], json!("unknown"));
    assert!(z["error"].as_str().unwrap().contains("turn failed"));
    // Its settlement observation is an error event in the journal.
    let receive = events(&record)
        .iter()
        .find(|e| {
            e["kind"] == json!("mailbox_receive") && e["output"]["value"]["member"] == json!("z")
        })
        .expect("z's settlement is journaled");
    assert_eq!(receive["status"], json!("error"));
    let _ = std::fs::remove_dir_all(store);
}

/// Exit-criterion crash coverage, fan-out fail-fast: one terminal failure
/// ends the pattern; a leased member is cancel-signalled (hint) and a
/// queued member goes terminal-cancelled.
#[tokio::test]
async fn fan_out_fail_fast_cancels_the_remaining_members() {
    let (app, store) = app();
    for agent in ["m1", "m2", "m3"] {
        register(&app, agent, member_manifest()).await;
    }
    submit_created(
        &app,
        "fan_out",
        json!({
            "coordination_id": "fo-ff",
            "fan_out": {
                "members": [
                    delegation("x", "m1", json!({"r": 1}), json!({})),
                    delegation("y", "m2", json!({"r": 2}), json!({})),
                    delegation("z", "m3", json!({"r": 3}), json!({}))
                ],
                "max_in_flight": 3,
                "on_member_failure": "fail_fast"
            }
        }),
    )
    .await;
    for member in ["x", "y", "z"] {
        wait_task(&app, &member_task_id("fo-ff", member)).await;
    }

    // m1 holds a lease (in flight) when m2 dies terminally.
    let f1 = activate_one(&app, "m1", "host-1", 60_000).await;
    claim_task(&app, "m1", "host-1", f1, "default--fo-ff--x").await;
    let f2 = activate_one(&app, "m2", "host-2", 60_000).await;
    claim_task(&app, "m2", "host-2", f2, "default--fo-ff--y").await;
    fail_task(
        &app,
        "default--fo-ff--y",
        "host-2",
        "unknown",
        false,
        json!({}),
    )
    .await;

    // Fail fast settled the pattern on the settlement hook's drive.
    let record = get_coordination(&app, "fo-ff").await;
    assert_eq!(record["settled"], json!(true));
    assert_eq!(record["outcome"]["status"], json!("failed"));
    assert!(record["outcome"]["result"].is_null());
    let members = record["outcome"]["members"].as_array().unwrap();
    assert_eq!(members[0]["settlement"], json!("cancelled")); // leased: signalled
    assert_eq!(members[1]["settlement"], json!("failed"));
    assert_eq!(members[2]["settlement"], json!("cancelled")); // queued: terminal

    // The leased member learned through the hint channel: still leased,
    // cancel_requested set — lease semantics stay the settlement surface.
    let leased = wait_task(&app, "default--fo-ff--x").await;
    assert_eq!(leased["status"], json!("leased"));
    assert_eq!(leased["cancel_requested"], json!(true));
    let v = fail_task(
        &app,
        "default--fo-ff--x",
        "host-1",
        "cancelled",
        true,
        json!({}),
    )
    .await;
    assert_eq!(v["requeued"], json!(false));
    let queued = wait_task(&app, "default--fo-ff--z").await;
    assert_eq!(queued["status"], json!("cancelled"));
    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// race
// --------------------------------------------------------------------- //

/// Exit criterion, the race effect gate: a candidate that is not freely
/// repeatable — declared or by the default — is a 400 before any write:
/// no record, no member tasks.
#[tokio::test]
async fn race_effect_gate_rejects_unsafe_candidates_before_any_write() {
    let (app, store) = app();
    for agent in ["ra", "rb"] {
        register(&app, agent, member_manifest()).await;
    }
    let safe = || delegation("b", "rb", json!({"bid": 2}), json!({"effect": "pure"}));
    let cases = [
        (
            "declared non_idempotent",
            delegation(
                "a",
                "ra",
                json!({"bid": 1}),
                json!({"effect": "non_idempotent"}),
            ),
        ),
        (
            "declared compensatable",
            delegation(
                "a",
                "ra",
                json!({"bid": 1}),
                json!({"effect": "compensatable"}),
            ),
        ),
        // The default is non_idempotent — an undeclared effect is never
        // eligible to race (the gate fails closed).
        (
            "undeclared (defaults non_idempotent)",
            delegation("a", "ra", json!({"bid": 1}), json!({})),
        ),
    ];
    for (i, (case, candidate)) in cases.into_iter().enumerate() {
        let cid = format!("race-gate-{i}");
        let (status, v) = submit(
            &app,
            "race",
            json!({
                "coordination_id": cid,
                "race": {"candidates": [candidate, safe()]}
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{case}: {v}");
        assert_eq!(v["error"], json!("bad_request"), "{case}: {v}");
        assert!(
            v["message"]
                .as_str()
                .unwrap()
                .contains("not freely repeatable"),
            "{case}: unexpected error: {v}"
        );
        // Before any write: no record, and no member tasks exist.
        let (status, _) = call(&app, "GET", &format!("/coordination/{cid}"), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{case}");
        let (_, tasks) = call(&app, "GET", "/tasks", None).await;
        let leaked: Vec<&Value> = tasks
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| t["task_id"].as_str().unwrap_or("").contains(&cid))
            .collect();
        assert!(leaked.is_empty(), "{case}: leaked tasks: {leaked:?}");
    }
    let _ = std::fs::remove_dir_all(store);
}

/// Exit criterion, race with the effect gate honored: the first completed
/// candidate wins, losers are cancel-signalled, and the loser's reported
/// cost lands in the outcome's waste accounting.
#[tokio::test]
async fn race_first_completion_wins_and_waste_is_accounted() {
    let (app, store) = app();
    register(&app, "boss", delegator_manifest()).await;
    for agent in ["ra", "rb", "rc", "rd"] {
        register(&app, agent, member_manifest()).await;
    }
    submit_created(
        &app,
        "race",
        json!({
            "coordination_id": "race-1",
            "delegator": "boss",
            "race": {
                "candidates": [
                    delegation("a", "ra", json!({"bid": "A"}), json!({"effect": "idempotent"})),
                    delegation("b", "rb", json!({"bid": "B"}), json!({"effect": "idempotent"})),
                    delegation("c", "rc", json!({"bid": "C"}), json!({"effect": "idempotent"})),
                    delegation("d", "rd", json!({"bid": "D"}), json!({"effect": "idempotent"}))
                ]
            }
        }),
    )
    .await;
    for member in ["a", "b", "c", "d"] {
        wait_task(&app, &member_task_id("race-1", member)).await;
    }

    // d runs first and dies terminally, having reported its cost. Race
    // candidates declare freely-repeatable effects (the gate), so a
    // retryable error class would re-queue the turn instead — the
    // terminal failure of a candidate is a non-retryable class.
    let fd = activate_one(&app, "rd", "host-d", 60_000).await;
    claim_task(&app, "rd", "host-d", fd, "default--race-1--d").await;
    fail_task(
        &app,
        "default--race-1--d",
        "host-d",
        "invalid_input",
        false,
        json!({"tokens": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15},
               "cost_usd": 0.05}),
    )
    .await;
    // b completes: the only completion, so the winner — the pattern
    // settles on this settlement hook's drive.
    let fb = activate_one(&app, "rb", "host-b", 60_000).await;
    claim_task(&app, "rb", "host-b", fb, "default--race-1--b").await;
    complete_task(
        &app,
        "default--race-1--b",
        "host-b",
        json!({"bid": "B", "answer": 42}),
        json!({"tokens": {"prompt_tokens": 100, "completion_tokens": 50, "total_tokens": 150},
               "cost_usd": 0.02}),
    )
    .await;

    let record = get_coordination(&app, "race-1").await;
    assert_eq!(record["settled"], json!(true));
    let outcome = &record["outcome"];
    assert_eq!(outcome["pattern"], json!("race"));
    assert_eq!(outcome["status"], json!("completed"));
    assert_eq!(
        outcome["result"],
        json!({"kind": "inline", "value": {"bid": "B", "answer": 42}})
    );
    // Waste accounting: the queued losers reported nothing, so the waste
    // is exactly d's reported evidence; the winner's cost is not waste.
    assert_eq!(outcome["wasted_cost_usd"], json!(0.05));
    assert_eq!(outcome["wasted_tokens"], json!(15));
    // Dispositions in contract order: cancelled, winner, cancelled, failed.
    let members = outcome["members"].as_array().unwrap();
    let settlements: Vec<&str> = members
        .iter()
        .map(|m| m["settlement"].as_str().unwrap())
        .collect();
    assert_eq!(
        settlements,
        ["cancelled", "completed", "cancelled", "failed"]
    );
    assert_eq!(members[3]["cost_usd"], json!(0.05));
    assert_eq!(members[3]["tokens"]["total_tokens"], json!(15));
    // Every settlement is journaled — the losers' too.
    assert_eq!(kind_count(&record, "mailbox_receive"), 4);
    // The queued losers went terminal-cancelled.
    for member in ["a", "c"] {
        let task = wait_task(&app, &member_task_id("race-1", member)).await;
        assert_eq!(task["status"], json!("cancelled"), "member {member}");
    }
    // The delegator receives the winning outcome.
    let outcome_task = wait_task(&app, "default--race-1--outcome").await;
    assert_eq!(outcome_task["payload"]["status"], json!("completed"));
    let boss = activate_one(&app, "boss", "host-boss", 60_000).await;
    claim_task(&app, "boss", "host-boss", boss, "default--race-1--outcome").await;
    let _ = std::fs::remove_dir_all(store);
}

/// Exit criterion, race all-failed: every candidate dies terminally, the
/// pattern settles failed, and the outcome dead-letters for an operator
/// (the supervision root-escalation precedent).
#[tokio::test]
async fn race_all_candidates_failed_settles_failed_and_dead_letters() {
    let (app, store) = app();
    register(&app, "boss", delegator_manifest()).await;
    for agent in ["rx", "ry"] {
        register(&app, agent, member_manifest()).await;
    }
    submit_created(
        &app,
        "race",
        json!({
            "coordination_id": "race-dead",
            "delegator": "boss",
            "race": {
                "candidates": [
                    delegation("x", "rx", json!({"bid": "X"}), json!({"effect": "pure"})),
                    delegation("y", "ry", json!({"bid": "Y"}), json!({"effect": "pure"}))
                ]
            }
        }),
    )
    .await;
    for member in ["x", "y"] {
        wait_task(&app, &member_task_id("race-dead", member)).await;
    }
    let fx = activate_one(&app, "rx", "host-x", 60_000).await;
    claim_task(&app, "rx", "host-x", fx, "default--race-dead--x").await;
    fail_task(
        &app,
        "default--race-dead--x",
        "host-x",
        "invalid_input",
        false,
        json!({}),
    )
    .await;
    let fy = activate_one(&app, "ry", "host-y", 60_000).await;
    claim_task(&app, "ry", "host-y", fy, "default--race-dead--y").await;
    fail_task(
        &app,
        "default--race-dead--y",
        "host-y",
        "invalid_input",
        false,
        json!({}),
    )
    .await;

    let record = get_coordination(&app, "race-dead").await;
    assert_eq!(record["settled"], json!(true));
    assert_eq!(record["outcome"]["status"], json!("failed"));
    assert!(record["outcome"]["result"].is_null());
    let settlements: Vec<&str> = record["outcome"]["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["settlement"].as_str().unwrap())
        .collect();
    assert_eq!(settlements, ["failed", "failed"]);

    // The DLQ obligation: one dead entry carrying the outcome as evidence,
    // bypassing the quota (evidence must not be dropped under pressure).
    let (status, dead) = call(&app, "GET", "/tasks?status=dead", None).await;
    assert_eq!(status, StatusCode::OK, "dlq list failed: {dead}");
    let entry = dead
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["task_id"] == json!("default--race-dead--race-dlq"))
        .expect("the race's dead-letter entry exists");
    assert_eq!(entry["kind"], json!("coordination_result"));
    assert_eq!(
        entry["idempotency_key"],
        json!("coordination:race-dead:race-dlq")
    );
    assert_eq!(entry["payload"]["status"], json!("failed"));
    assert!(entry["last_error"]
        .as_str()
        .unwrap()
        .contains("every candidate failed"));

    // The delegator still learns the pattern failed — a failure is an
    // outcome, not silence.
    let outcome_task = wait_task(&app, "default--race-dead--outcome").await;
    assert_eq!(outcome_task["payload"]["status"], json!("failed"));
    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// quorum
// --------------------------------------------------------------------- //

/// Exit criterion, quorum with a deterministic resolver: a crashed juror
/// mid-pattern does not stop the vote; the first k completions resolve by
/// strict majority, the resolver record carries the exact inputs it saw,
/// and repeated reads reproduce the identical outcome.
#[tokio::test]
async fn quorum_majority_resolves_with_a_crashed_juror_deterministically() {
    let (app, store) = app();
    for agent in ["j1", "j2", "j3", "j4", "j5"] {
        register(&app, agent, member_manifest()).await;
    }
    let juror = |member: &str, agent: &str| {
        delegation(member, agent, json!({"case": "fraud-review"}), json!({}))
    };
    submit_created(
        &app,
        "quorum",
        json!({
            "coordination_id": "q-1",
            "quorum": {
                "members": [
                    juror("p", "j1"),
                    juror("q", "j2"),
                    juror("r", "j3"),
                    juror("s", "j4"),
                    juror("t", "j5")
                ],
                "threshold": 3,
                "resolver": {"resolver": "majority_equal"}
            }
        }),
    )
    .await;
    for member in ["p", "q", "r", "s", "t"] {
        wait_task(&app, &member_task_id("q-1", member)).await;
    }

    // j4 crashes mid-pattern: claimed, never settled, lease lapses. The
    // vote does not wait for it.
    let f4 = activate_one(&app, "j4", "host-4", 60_000).await;
    let (status, v) = next_with_lease(&app, "j4", "host-4", f4, 100).await;
    assert_eq!(status, StatusCode::OK, "j4 claim failed: {v}");
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Three jurors land: X, X, Y in completion order.
    let f1 = activate_one(&app, "j1", "host-1", 60_000).await;
    claim_task(&app, "j1", "host-1", f1, "default--q-1--p").await;
    complete_task(
        &app,
        "default--q-1--p",
        "host-1",
        json!({"answer": "X"}),
        json!({}),
    )
    .await;
    let f2 = activate_one(&app, "j2", "host-2", 60_000).await;
    claim_task(&app, "j2", "host-2", f2, "default--q-1--q").await;
    complete_task(
        &app,
        "default--q-1--q",
        "host-2",
        json!({"answer": "X"}),
        json!({}),
    )
    .await;
    let f3 = activate_one(&app, "j3", "host-3", 60_000).await;
    claim_task(&app, "j3", "host-3", f3, "default--q-1--r").await;
    complete_task(
        &app,
        "default--q-1--r",
        "host-3",
        json!({"answer": "Y"}),
        json!({}),
    )
    .await;

    let record = get_coordination(&app, "q-1").await;
    assert_eq!(record["settled"], json!(true));
    let outcome = &record["outcome"];
    assert_eq!(outcome["status"], json!("completed"));
    // Two X out of three accepted is a strict majority: decided X.
    assert_eq!(
        outcome["result"],
        json!({"kind": "inline", "value": {"answer": "X"}})
    );
    // The resolver record is the audit trail: the accepted inputs in the
    // deterministic (member task-id) order the resolver saw them.
    let resolver = &outcome["resolver"];
    assert_eq!(resolver["resolver"], json!({"resolver": "majority_equal"}));
    assert_eq!(
        resolver["inputs"],
        json!([{"answer": "X"}, {"answer": "X"}, {"answer": "Y"}])
    );
    assert_eq!(resolver["output"], json!({"answer": "X"}));
    assert_eq!(resolver["decided"], json!(true));
    // The crashed juror and the never-claimed one are journaled cancelled
    // — crash evidence is a disposition, never silence.
    let members = outcome["members"].as_array().unwrap();
    let settlements: Vec<&str> = members
        .iter()
        .map(|m| m["settlement"].as_str().unwrap())
        .collect();
    assert_eq!(
        settlements,
        [
            "completed",
            "completed",
            "completed",
            "cancelled",
            "cancelled"
        ]
    );
    // j4's cancel is the R0.6 hint: its record is still `leased` with
    // `cancel_requested` set (a record stays `leased` until a claim
    // finalizes the lapse) — the pattern journaled it `cancelled` and
    // moved on, and a completion that arrives anyway would not change
    // the settled outcome.
    let crashed = wait_task(&app, "default--q-1--s").await;
    assert_eq!(crashed["cancel_requested"], json!(true));

    // Determinism: a repeated read reproduces the identical outcome and
    // journal (the drive is convergent — nothing re-appends).
    let again = get_coordination(&app, "q-1").await;
    assert_eq!(again["outcome"], record["outcome"]);
    assert_eq!(again["journal"]["events"], record["journal"]["events"]);
    let _ = std::fs::remove_dir_all(store);
}

/// Fewer than k members can still complete: the pattern fails open as
/// `unreachable` with the evidence journaled — k is never silently
/// downgraded.
#[tokio::test]
async fn quorum_threshold_unreachable_fails_open() {
    let (app, store) = app();
    for agent in ["k1", "k2", "k3", "k4"] {
        register(&app, agent, member_manifest()).await;
    }
    let juror = |member: &str, agent: &str| delegation(member, agent, json!({}), json!({}));
    submit_created(
        &app,
        "quorum",
        json!({
            "coordination_id": "q-unreachable",
            "quorum": {
                "members": [
                    juror("a", "k1"),
                    juror("b", "k2"),
                    juror("c", "k3"),
                    juror("d", "k4")
                ],
                "threshold": 3,
                "resolver": {"resolver": "first_k"}
            }
        }),
    )
    .await;
    for member in ["a", "b", "c", "d"] {
        wait_task(&app, &member_task_id("q-unreachable", member)).await;
    }

    // Two jurors die terminally: 4 - 2 = 2 < 3, the threshold is
    // unreachable, and the second failure's drive settles it.
    let f1 = activate_one(&app, "k1", "host-1", 60_000).await;
    claim_task(&app, "k1", "host-1", f1, "default--q-unreachable--a").await;
    fail_task(
        &app,
        "default--q-unreachable--a",
        "host-1",
        "unknown",
        false,
        json!({}),
    )
    .await;
    let record = get_coordination(&app, "q-unreachable").await;
    assert_eq!(record["settled"], json!(false)); // 3 of 3 remaining: still reachable
    let f2 = activate_one(&app, "k2", "host-2", 60_000).await;
    claim_task(&app, "k2", "host-2", f2, "default--q-unreachable--b").await;
    fail_task(
        &app,
        "default--q-unreachable--b",
        "host-2",
        "unknown",
        false,
        json!({}),
    )
    .await;

    let record = get_coordination(&app, "q-unreachable").await;
    assert_eq!(record["settled"], json!(true));
    assert_eq!(record["outcome"]["status"], json!("unreachable"));
    assert!(record["outcome"]["result"].is_null());
    // No resolution ran — there is no resolver record to audit.
    assert!(record["outcome"]["resolver"].is_null());
    let settlements: Vec<&str> = record["outcome"]["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["settlement"].as_str().unwrap())
        .collect();
    assert_eq!(settlements, ["failed", "failed", "cancelled", "cancelled"]);
    let _ = std::fs::remove_dir_all(store);
}

/// The contract 400s: threshold outside `1..=members`, duplicate member
/// names, empty members, and the pinned-but-unimplemented custom resolver
/// — all rejected before any write.
#[tokio::test]
async fn quorum_bad_contracts_are_rejected_before_any_write() {
    let (app, store) = app();
    for agent in ["v1", "v2"] {
        register(&app, agent, member_manifest()).await;
    }
    let juror = |member: &str, agent: &str| delegation(member, agent, json!({}), json!({}));
    let cases: Vec<(&str, Value)> = vec![
        (
            "threshold zero",
            json!({"members": [juror("a", "v1"), juror("b", "v2")],
                   "threshold": 0, "resolver": {"resolver": "first_k"}}),
        ),
        (
            "threshold above membership",
            json!({"members": [juror("a", "v1"), juror("b", "v2")],
                   "threshold": 3, "resolver": {"resolver": "first_k"}}),
        ),
        (
            "duplicate member names",
            json!({"members": [juror("a", "v1"), juror("a", "v2")],
                   "threshold": 1, "resolver": {"resolver": "first_k"}}),
        ),
        (
            "no members",
            json!({"members": [], "threshold": 1, "resolver": {"resolver": "first_k"}}),
        ),
        (
            "custom resolver (pinned, not implemented)",
            json!({"members": [juror("a", "v1")],
                   "threshold": 1, "resolver": {"resolver": "custom", "name": "my-policy"}}),
        ),
    ];
    for (i, (case, quorum)) in cases.into_iter().enumerate() {
        let cid = format!("q-bad-{i}");
        let (status, v) = submit(
            &app,
            "quorum",
            json!({"coordination_id": cid, "quorum": quorum}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{case}: {v}");
        let (status, _) = call(&app, "GET", &format!("/coordination/{cid}"), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{case}: a record leaked");
    }
    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// TeamTrace
// --------------------------------------------------------------------- //

/// The TeamTrace read: one connected causal tree over the pattern's
/// journal — start at depth 0, sends at 1, the settlement observation at
/// 2, the end at 1 — exactly one root.
#[tokio::test]
async fn team_trace_is_one_connected_tree_with_depths() {
    let (app, store) = app();
    register(&app, "writer", member_manifest()).await;
    submit_created(
        &app,
        "delegate",
        json!({
            "coordination_id": "t-1",
            "delegate": {
                "delegate": delegation("solo", "writer", json!({"n": 1}), json!({}))
            }
        }),
    )
    .await;
    wait_task(&app, "default--t-1--solo").await;
    let fence = activate_one(&app, "writer", "host-w", 60_000).await;
    claim_task(&app, "writer", "host-w", fence, "default--t-1--solo").await;
    complete_task(
        &app,
        "default--t-1--solo",
        "host-w",
        json!({"done": true}),
        json!({}),
    )
    .await;

    let (status, v) = call(&app, "GET", "/coordination/t-1/trace", None).await;
    assert_eq!(status, StatusCode::OK, "trace failed: {v}");
    assert_eq!(v["coordination_id"], json!("t-1"));
    assert_eq!(v["connected"], json!(true));
    let trace = &v["trace"];
    // Only the pattern's own journal contributes in this test: member
    // tasks carry no run linkage until a worker journals its turn.
    assert_eq!(trace["run_ids"], json!(["coordination:default:t-1"]));
    // Exactly one root: the CoordinationStart event.
    assert_eq!(trace["roots"], json!(["coordination:default:t-1:0"]));
    let nodes = trace["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 4);
    let by_kind = |kind: &str| {
        nodes
            .iter()
            .find(|n| n["kind"] == json!(kind))
            .unwrap_or_else(|| panic!("missing {kind} node"))
    };
    let start = by_kind("coordination_start");
    let send = by_kind("mailbox_send");
    let receive = by_kind("mailbox_receive");
    let end = by_kind("coordination_end");
    assert_eq!(start["depth"], json!(0));
    assert_eq!(send["depth"], json!(1));
    assert_eq!(receive["depth"], json!(2));
    assert_eq!(end["depth"], json!(1));
    // The children adjacency: start → {send, end}, send → {receive}.
    assert_eq!(
        start["children"],
        json!([send["event_id"].clone(), end["event_id"].clone()])
    );
    assert_eq!(send["children"], json!([receive["event_id"].clone()]));
    assert!(end["children"].is_null() || end["children"] == json!([]));

    // Unknown coordination: the trace read is a 404 like the record read.
    let (status, _) = call(&app, "GET", "/coordination/nope/trace", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// restart durability, dedup, validation, isolation
// --------------------------------------------------------------------- //

/// The record, journal, and outbox rows written by one server drive the
/// pattern to its settle under a restarted server over the same store.
#[tokio::test]
async fn coordination_settles_across_a_server_restart() {
    let store = temp_store();
    let (app1, _) = app_over(store.clone());
    register(&app1, "writer", member_manifest()).await;
    submit_created(
        &app1,
        "delegate",
        json!({
            "coordination_id": "d-restart",
            "delegate": {
                "delegate": delegation("solo", "writer", json!({"n": 1}),
                                       json!({"effect": "idempotent"}))
            }
        }),
    )
    .await;
    // The first server's drive wrote the record, the journal start, and
    // the outbox row; then it "dies" before the relay publishes.
    drop(app1);

    // The restarted server loads the same store: its relay publishes the
    // pending row (the crash-recovery path), and the pattern completes.
    let (app2, _) = app_over(store.clone());
    let task = wait_task(&app2, "default--d-restart--solo").await;
    assert_eq!(task["payload"]["coordination_id"], json!("d-restart"));
    let fence = activate_one(&app2, "writer", "host-w", 60_000).await;
    claim_task(&app2, "writer", "host-w", fence, "default--d-restart--solo").await;
    complete_task(
        &app2,
        "default--d-restart--solo",
        "host-w",
        json!({"after": "restart"}),
        json!({}),
    )
    .await;

    let record = get_coordination(&app2, "d-restart").await;
    assert_eq!(record["settled"], json!(true));
    assert_eq!(record["outcome"]["status"], json!("completed"));
    assert_eq!(
        record["outcome"]["result"],
        json!({"kind": "inline", "value": {"after": "restart"}})
    );
    // The journal survived intact: the full chain, seq 1..=4.
    let events = events(&record);
    assert_eq!(events.len(), 4);
    for (i, event) in events.iter().enumerate() {
        assert_eq!(event["seq"], json!(i as u64));
    }
    let _ = std::fs::remove_dir_all(store);
}

/// Re-submitting a caller-supplied coordination id converges on the
/// existing pattern instead of forking a second one (the enqueue
/// idempotency-key discipline applied to whole patterns).
#[tokio::test]
async fn submission_with_an_existing_coordination_id_is_deduplicated() {
    let (app, store) = app();
    register(&app, "writer", member_manifest()).await;
    let body = || {
        json!({
            "coordination_id": "d-dup",
            "delegate": {
                "delegate": delegation("solo", "writer", json!({"n": 1}), json!({}))
            }
        })
    };
    submit_created(&app, "delegate", body()).await;
    let (status, v) = submit(&app, "delegate", body()).await;
    assert_eq!(status, StatusCode::OK, "dedup retry failed: {v}");
    assert_eq!(v["coordination_id"], json!("d-dup"));
    assert_eq!(v["deduplicated"], json!(true));

    // One pattern, one member task, one journal.
    wait_task(&app, "default--d-dup--solo").await;
    let (_, tasks) = call(&app, "GET", "/tasks", None).await;
    let member_tasks: Vec<&Value> = tasks
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["task_id"].as_str().unwrap_or("").contains("d-dup"))
        .collect();
    assert_eq!(member_tasks.len(), 1);
    let record = get_coordination(&app, "d-dup").await;
    assert_eq!(events(&record).len(), 2); // start + one send, never duplicated
    let _ = std::fs::remove_dir_all(store);
}

/// Submission-time validation against the registry: every member must
/// target a registered agent at the exact pinned manifest version with a
/// declared kind; the delegate's grant may only narrow; reserved member
/// names and outcome-deaf delegators are refused.
#[tokio::test]
async fn submission_validates_members_grants_and_the_delegator() {
    let (app, store) = app();
    register(&app, "writer", member_manifest()).await;
    register(&app, "deaf", member_manifest()).await; // no coordination_result
    let solo = |extra: Value| delegation("solo", "writer", json!({"n": 1}), extra);
    let cases: Vec<(&str, Value)> = vec![
        (
            "unregistered member agent",
            json!({"delegate": {"delegate": delegation("solo", "ghost", json!({}), json!({}))}}),
        ),
        (
            "manifest version pin mismatch",
            json!({"delegate": {"delegate": delegation("solo", "writer", json!({}),
                   json!({"manifest_version": "worker/9.9.9"}))}}),
        ),
        (
            "kind the manifest does not accept",
            json!({"delegate": {"delegate": delegation("solo", "writer", json!({}),
                   json!({"kind": "translate"}))}}),
        ),
        (
            "reserved member name",
            json!({"delegate": {"delegate": delegation("outcome", "writer", json!({}), json!({}))}}),
        ),
        (
            "context grant widens declared scopes",
            json!({"delegate": {"delegate": solo(json!({})),
                   "context": {"scopes": ["tenant"]}}}),
        ),
        (
            "unregistered delegator",
            json!({"delegator": "ghost", "delegate": {"delegate": solo(json!({}))}}),
        ),
        (
            "delegator deaf to coordination_result",
            json!({"delegator": "deaf", "delegate": {"delegate": solo(json!({}))}}),
        ),
    ];
    for (i, (case, body)) in cases.into_iter().enumerate() {
        let mut body = body;
        body["coordination_id"] = json!(format!("val-{i}"));
        let (status, v) = submit(&app, "delegate", body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{case}: {v}");
        assert_eq!(v["error"], json!("bad_request"), "{case}: {v}");
        let (status, _) = call(&app, "GET", &format!("/coordination/val-{i}"), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{case}: a record leaked");
    }
    let _ = std::fs::remove_dir_all(store);
}

/// Coordination records, journals, member tasks, and outcome messages are
/// tenant-scoped like every other id: another tenant cannot read, claim,
/// or collide with them.
#[tokio::test]
async fn coordination_records_are_tenant_isolated() {
    let (app, store) = multi_tenant_app();
    register_as(&app, Some(ACME), "writer", member_manifest()).await;
    register_as(&app, Some(GLOBEX), "writer", member_manifest()).await;

    let (status, v) = call_as(
        &app,
        Some(ACME),
        "POST",
        "/coordination/delegate",
        Some(json!({
            "coordination_id": "iso-1",
            "delegate": {
                "delegate": delegation("solo", "writer", json!({"n": 1}), json!({}))
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "acme submit failed: {v}");
    // The member task id carries acme's tenant prefix.
    assert_eq!(
        v["submitted"],
        json!([{"member": "solo", "task_id": "acme--iso-1--solo"}])
    );

    // Acme reads its pattern; globex gets the indistinguishable 404.
    let (status, _) = call_as(&app, Some(ACME), "GET", "/coordination/iso-1", None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call_as(&app, Some(GLOBEX), "GET", "/coordination/iso-1", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call_as(&app, Some(GLOBEX), "GET", "/coordination/iso-1/trace", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Globex cannot see or claim the member task, even with a registered
    // agent of the same external id and an active activation.
    let (status, _) = call_as(&app, Some(GLOBEX), "GET", "/tasks/acme--iso-1--solo", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, v) = call_as(
        &app,
        Some(GLOBEX),
        "POST",
        "/agents/writer/activate",
        Some(json!({"worker_id": "host-g", "lease_ms": 60_000})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "globex activate failed: {v}");
    tokio::time::sleep(Duration::from_millis(150)).await; // let the relay publish
    let (status, _) = call_as(
        &app,
        Some(GLOBEX),
        "POST",
        "/agents/writer/mailbox/next",
        Some(json!({"worker_id": "host-g", "fencing": v["fencing"], "lease_ms": 30_000})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Same external id under globex is an independent pattern.
    let (status, _) = call_as(
        &app,
        Some(GLOBEX),
        "POST",
        "/coordination/delegate",
        Some(json!({
            "coordination_id": "iso-1",
            "delegate": {
                "delegate": delegation("solo", "writer", json!({"n": 2}), json!({}))
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, v) = call_as(&app, Some(GLOBEX), "GET", "/coordination/iso-1", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["journal"]["run_id"], json!("coordination:globex:iso-1"));
    let _ = std::fs::remove_dir_all(store);
}
