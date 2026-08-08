//! Trigger integration tests: signed webhook ingress (accept/reject), the
//! three actions (start-run / send-message / resume-thread), debounce
//! coalescing, the event log + dead-letter + replay, and tenant isolation.
//! Driven in-process via `tower::ServiceExt::oneshot` (no sockets), the
//! crate's existing test pattern.

use std::path::PathBuf;
use std::time::Duration;

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use hmac::{Hmac, Mac};
use rusty_agent_runtime::prelude::*;
use rusty_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

const ACME: (&str, &str) = ("x-api-key", "acme-secret");
const GLOBEX: (&str, &str) = ("x-api-key", "globex-secret");

// --------------------------------------------------------------------- //
// App + request helpers
// --------------------------------------------------------------------- //

/// `first -> second`, appending to a `log` channel.
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

/// A single gate node that interrupts until resumed.
fn interrupt_graph() -> (Graph, StateSpec) {
    let spec = StateSpec::new().channel("answer", Reducer::Overwrite);
    let mut builder = GraphBuilder::new();
    builder.add_node("gate", |ctx: NodeContext| async move {
        match ctx.resume_value() {
            Some(v) => Ok(NodeOutput::update("answer", v.clone())),
            None => Err(ctx.interrupt(json!({"question": "approve?"}))),
        }
    });
    builder.set_entry_point("gate");
    (builder.compile().unwrap(), spec)
}

/// Unique temp store root, removed at the end of each test (best effort).
fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!(
        "rusty-server-triggers-test-{}",
        uuid::Uuid::new_v4()
    ))
}

fn registry() -> GraphRegistry {
    let (pipeline, pipeline_spec) = pipeline_graph();
    let (gate, gate_spec) = interrupt_graph();
    let mut registry = GraphRegistry::new();
    registry.register("pipeline", pipeline, pipeline_spec);
    registry.register("interrupt_gate", gate, gate_spec);
    registry
}

/// Open (dev) mode: no API keys, default tenant.
fn open_app() -> (Router, PathBuf) {
    let store = temp_store();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone());
    (router(registry(), config), store)
}

/// Two tenants: `acme` and `globex`.
fn multi_tenant_app() -> (Router, PathBuf) {
    let store = temp_store();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_tenant_key("acme", "acme-secret")
        .with_tenant_key("globex", "globex-secret");
    (router(registry(), config), store)
}

/// Send a request with explicit headers; returns `(status, json)`.
async fn call(
    app: &Router,
    headers: &[(&str, &str)],
    method: &str,
    uri: &str,
    body: Option<String>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        builder = builder.header(*k, *v);
    }
    let body = match body {
        Some(raw) => {
            builder = builder.header("content-type", "application/json");
            Body::from(raw)
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

/// `X-Rusty-Signature` for a body, computed the way an external sender would.
fn sign(secret: &str, body: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body.as_bytes());
    let bytes = mac.finalize().into_bytes();
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256={hex}")
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Create a trigger; returns the wire record (carries the secret).
async fn create_trigger(app: &Router, auth: Option<(&str, &str)>, payload: Value) -> Value {
    let headers: Vec<(&str, &str)> = auth.into_iter().collect();
    let (status, v) = call(
        app,
        &headers,
        "POST",
        "/triggers",
        Some(payload.to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "trigger creation failed: {v}");
    v
}

/// Fire one signed webhook event.
async fn fire(
    app: &Router,
    trigger_id: &str,
    secret: &str,
    payload: &Value,
) -> (StatusCode, Value) {
    let body = payload.to_string();
    let signature = sign(secret, &body);
    call(
        app,
        &[("x-rusty-signature", &signature)],
        "POST",
        &format!("/triggers/{trigger_id}/webhook"),
        Some(body),
    )
    .await
}

/// Poll a run until it reaches a terminal state; returns the status body.
async fn wait_run(app: &Router, auth: Option<(&str, &str)>, run_id: &str) -> Value {
    let headers: Vec<(&str, &str)> = auth.into_iter().collect();
    for _ in 0..150 {
        let (status, v) = call(app, &headers, "GET", &format!("/runs/{run_id}"), None).await;
        assert_eq!(status, StatusCode::OK, "run lookup failed: {v}");
        match v["status"].as_str().unwrap_or("") {
            "success" | "interrupted" | "error" | "cancelled" => return v,
            _ => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
    panic!("run `{run_id}` did not reach a terminal state")
}

// --------------------------------------------------------------------- //
// Signature accept / reject
// --------------------------------------------------------------------- //

#[tokio::test]
async fn webhook_rejects_unsigned_and_badly_signed_events() {
    let (app, store) = open_app();
    let trigger = create_trigger(
        &app,
        None,
        json!({
            "name": "sig-check",
            "target": {"kind": "assistant", "id": "bot"},
            "action": "start_run",
        }),
    )
    .await;
    let trigger_id = trigger["trigger_id"].as_str().unwrap().to_string();
    let secret = trigger["secret"].as_str().unwrap().to_string();
    // The assistant the trigger points at (creation validates the binding's
    // shape, not the assistant's existence — execution checks that).
    let (status, _) = call(
        &app,
        &[],
        "POST",
        "/assistants",
        Some(json!({"name": "bot", "graph": "pipeline", "assistant_id": "bot"}).to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Unknown trigger id → 404 (existence is not leaked to unsigned senders).
    let body = json!({"message": "hello"}).to_string();
    let (status, _) = call(
        &app,
        &[("x-rusty-signature", &sign(&secret, &body))],
        "POST",
        "/triggers/no-such-trigger/webhook",
        Some(body.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Missing signature → 401.
    let (status, v) = call(
        &app,
        &[],
        "POST",
        &format!("/triggers/{trigger_id}/webhook"),
        Some(body.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(v["error"], json!("unauthorized"));

    // Wrong signature (right shape, wrong secret) → 401.
    let (status, _) = call(
        &app,
        &[("x-rusty-signature", &sign("wrong-secret-0123456", &body))],
        "POST",
        &format!("/triggers/{trigger_id}/webhook"),
        Some(body.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Signature over different bytes than the body sent → 401.
    let (status, _) = call(
        &app,
        &[("x-rusty-signature", &sign(&secret, "{}"))],
        "POST",
        &format!("/triggers/{trigger_id}/webhook"),
        Some(body.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Correct signature → 202, executed. The webhook needs no API key even
    // though the rest of the API would demand one in keyed mode.
    let (status, v) = fire(&app, &trigger_id, &secret, &json!({"message": "hello"})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "signed webhook failed: {v}");
    assert_eq!(v["status"], json!("executed"));
    assert!(v["run_id"].is_string(), "executed event carries a run id");

    // The event log records the hash of the exact signed bytes.
    let (status, v) = call(
        &app,
        &[],
        "GET",
        &format!("/triggers/{trigger_id}/events"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let events = v.as_array().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["status"], json!("executed"));
    assert_eq!(
        events[0]["payload_hash"],
        json!(sha256_hex(
            json!({"message": "hello"}).to_string().as_bytes()
        ))
    );

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Actions
// --------------------------------------------------------------------- //

#[tokio::test]
async fn start_run_action_runs_the_assistant_on_a_fresh_thread() {
    let (app, store) = open_app();
    let (status, _) = call(
        &app,
        &[],
        "POST",
        "/assistants",
        Some(json!({"name": "bot", "graph": "pipeline", "assistant_id": "bot"}).to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let trigger = create_trigger(
        &app,
        None,
        json!({
            "name": "on-pr",
            "target": {"kind": "assistant", "id": "bot"},
            "action": "start_run",
            "input_template": {"log": ["{{event.message}}"]},
        }),
    )
    .await;
    let trigger_id = trigger["trigger_id"].as_str().unwrap().to_string();
    let secret = trigger["secret"].as_str().unwrap().to_string();

    let (status, v) = fire(
        &app,
        &trigger_id,
        &secret,
        &json!({"message": "webhook-seed"}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "webhook failed: {v}");
    let run_id = v["run_id"].as_str().unwrap().to_string();

    // The action scheduled a real run on a fresh thread: it completes, and
    // the rendered input seeded the thread's state.
    let run = wait_run(&app, None, &run_id).await;
    assert_eq!(run["status"], json!("success"), "run failed: {run}");
    let thread_id = run["thread_id"].as_str().unwrap().to_string();
    let (_, v) = call(
        &app,
        &[],
        "GET",
        &format!("/threads/{thread_id}/state"),
        None,
    )
    .await;
    assert_eq!(
        v["values"]["log"],
        json!(["webhook-seed", "first", "second"]),
        "rendered input must seed the run: {v}"
    );

    // The trigger's bookkeeping moved.
    let (_, v) = call(&app, &[], "GET", &format!("/triggers/{trigger_id}"), None).await;
    assert_eq!(v["events_received"], json!(1));
    assert_eq!(v["runs_fired"], json!(1));

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn send_message_action_schedules_a_run_on_the_bound_thread() {
    let (app, store) = open_app();
    let (status, v) = call(
        &app,
        &[],
        "POST",
        "/threads",
        Some(json!({"graph": "pipeline", "thread_id": "inbox"}).to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "thread failed: {v}");
    let trigger = create_trigger(
        &app,
        None,
        json!({
            "name": "on-message",
            "target": {"kind": "thread", "id": "inbox"},
            "action": "send_message",
            "input_template": {"log": ["{{event.text}}"]},
        }),
    )
    .await;
    let trigger_id = trigger["trigger_id"].as_str().unwrap().to_string();
    let secret = trigger["secret"].as_str().unwrap().to_string();

    let (status, v) = fire(&app, &trigger_id, &secret, &json!({"text": "ping"})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "webhook failed: {v}");
    let run_id = v["run_id"].as_str().unwrap().to_string();
    let run = wait_run(&app, None, &run_id).await;
    assert_eq!(run["status"], json!("success"), "run failed: {run}");
    assert_eq!(
        run["thread_id"],
        json!("inbox"),
        "the message must land on the bound thread"
    );

    let (_, v) = call(&app, &[], "GET", "/threads/inbox/state", None).await;
    assert_eq!(v["values"]["log"], json!(["ping", "first", "second"]));

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn resume_thread_action_resumes_an_interrupted_run() {
    let (app, store) = open_app();
    let (status, _) = call(
        &app,
        &[],
        "POST",
        "/threads",
        Some(json!({"graph": "interrupt_gate", "thread_id": "gate-thread"}).to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // First run suspends on the gate's interrupt.
    let (status, v) = call(
        &app,
        &[],
        "POST",
        "/threads/gate-thread/runs/wait",
        Some(json!({}).to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "first run failed: {v}");
    assert_eq!(v["status"], json!("interrupted"));

    let trigger = create_trigger(
        &app,
        None,
        json!({
            "name": "on-approval",
            "target": {"kind": "thread", "id": "gate-thread"},
            "action": "resume_thread",
            "input_template": {"approved": "{{event.approved}}"},
        }),
    )
    .await;
    let trigger_id = trigger["trigger_id"].as_str().unwrap().to_string();
    let secret = trigger["secret"].as_str().unwrap().to_string();

    // The event's payload becomes the resume value through the template.
    let (status, v) = fire(&app, &trigger_id, &secret, &json!({"approved": true})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "webhook failed: {v}");
    let run_id = v["run_id"].as_str().unwrap().to_string();
    let run = wait_run(&app, None, &run_id).await;
    assert_eq!(run["status"], json!("success"), "resume run failed: {run}");

    let (_, v) = call(&app, &[], "GET", "/threads/gate-thread/state", None).await;
    assert_eq!(v["values"]["answer"], json!({"approved": true}));

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Debounce
// --------------------------------------------------------------------- //

#[tokio::test]
async fn debounce_coalesces_a_burst_into_one_action() {
    let (app, store) = open_app();
    let (status, _) = call(
        &app,
        &[],
        "POST",
        "/assistants",
        Some(json!({"name": "bot", "graph": "pipeline", "assistant_id": "bot"}).to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let trigger = create_trigger(
        &app,
        None,
        json!({
            "name": "on-burst",
            "target": {"kind": "assistant", "id": "bot"},
            "action": "start_run",
            // `{{event}}` resolves to the array of coalesced payloads.
            "input_template": {"log": "{{event}}"},
            "debounce_ms": 250,
        }),
    )
    .await;
    let trigger_id = trigger["trigger_id"].as_str().unwrap().to_string();
    let secret = trigger["secret"].as_str().unwrap().to_string();

    // Three events inside the window: each is accepted as pending.
    for n in 1..=3 {
        let (status, v) = fire(&app, &trigger_id, &secret, &json!({"n": n})).await;
        assert_eq!(status, StatusCode::ACCEPTED, "event {n} rejected: {v}");
        assert_eq!(v["status"], json!("pending"));
        assert!(
            v["run_id"].is_null(),
            "debounced events do not execute individually"
        );
    }

    // The flush lands ~250 ms after the last event: poll the log until all
    // three events are coalesced behind one shared run id.
    let mut events = Vec::new();
    for _ in 0..100 {
        let (_, v) = call(
            &app,
            &[],
            "GET",
            &format!("/triggers/{trigger_id}/events"),
            None,
        )
        .await;
        events = v.as_array().unwrap().clone();
        if events.len() == 3 && events.iter().all(|e| e["status"] == json!("coalesced")) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        events.len(),
        3,
        "all three events must be logged: {events:?}"
    );
    assert!(
        events.iter().all(|e| e["status"] == json!("coalesced")),
        "events must coalesce: {events:?}"
    );
    let run_ids: Vec<&str> = events
        .iter()
        .map(|e| e["run_id"].as_str().unwrap())
        .collect();
    assert!(
        run_ids.windows(2).all(|w| w[0] == w[1]),
        "one coalesced action = one run id: {run_ids:?}"
    );

    // One run fired for the whole burst, carrying the array of payloads.
    let run = wait_run(&app, None, run_ids[0]).await;
    assert_eq!(
        run["status"],
        json!("success"),
        "coalesced run failed: {run}"
    );
    let thread_id = run["thread_id"].as_str().unwrap().to_string();
    let (_, v) = call(
        &app,
        &[],
        "GET",
        &format!("/threads/{thread_id}/state"),
        None,
    )
    .await;
    assert_eq!(
        v["values"]["log"],
        json!([{"n": 1}, {"n": 2}, {"n": 3}, "first", "second"]),
        "the coalesced action carries the burst's payloads in order: {v}"
    );

    // Bookkeeping: three events received, one run fired.
    let mut record = Value::Null;
    for _ in 0..100 {
        let (_, v) = call(&app, &[], "GET", &format!("/triggers/{trigger_id}"), None).await;
        record = v;
        if record["runs_fired"] == json!(1) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(record["events_received"], json!(3));
    assert_eq!(record["runs_fired"], json!(1));

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Dead-letter + replay
// --------------------------------------------------------------------- //

#[tokio::test]
async fn failed_actions_dead_letter_and_replay() {
    let (app, store) = open_app();
    // The target thread does not exist yet: every event fails.
    let trigger = create_trigger(
        &app,
        None,
        json!({
            "name": "on-message",
            "target": {"kind": "thread", "id": "ghost"},
            "action": "send_message",
            "input_template": {"log": ["{{event.text}}"]},
        }),
    )
    .await;
    let trigger_id = trigger["trigger_id"].as_str().unwrap().to_string();
    let secret = trigger["secret"].as_str().unwrap().to_string();

    let (status, v) = fire(&app, &trigger_id, &secret, &json!({"text": "first"})).await;
    assert_eq!(
        status,
        StatusCode::BAD_GATEWAY,
        "failed action must be 502: {v}"
    );
    assert_eq!(v["error"], json!("action_failed"));

    // The failure is on the event log and the dead-letter list.
    let (_, v) = call(
        &app,
        &[],
        "GET",
        &format!("/triggers/{trigger_id}/events"),
        None,
    )
    .await;
    let events = v.as_array().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["status"], json!("failed"));
    assert!(
        events[0]["error"]
            .as_str()
            .unwrap()
            .contains("thread `ghost` not found"),
        "the dead letter carries the failure detail: {events:?}"
    );
    let event_id = events[0]["event_id"].as_str().unwrap().to_string();
    let (_, v) = call(
        &app,
        &[],
        "GET",
        &format!("/triggers/{trigger_id}/dead-letter"),
        None,
    )
    .await;
    let dead = v.as_array().unwrap();
    assert_eq!(dead.len(), 1);
    assert_eq!(dead[0]["event_id"], json!(event_id));

    // Create the missing thread, then replay the dead event: it executes,
    // logged as a new event pointing at the original.
    let (status, _) = call(
        &app,
        &[],
        "POST",
        "/threads",
        Some(json!({"graph": "pipeline", "thread_id": "ghost"}).to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, v) = call(
        &app,
        &[],
        "POST",
        &format!("/triggers/{trigger_id}/events/{event_id}/replay"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "replay failed: {v}");
    assert_eq!(v["status"], json!("executed"));
    assert_eq!(v["replayed_from"], json!(event_id));
    let run = wait_run(&app, None, v["run_id"].as_str().unwrap()).await;
    assert_eq!(
        run["status"],
        json!("success"),
        "replayed run failed: {run}"
    );

    // The replayed payload landed on the thread; the original failure stays
    // on the dead-letter list (history, not a queue slot).
    let (_, v) = call(&app, &[], "GET", "/threads/ghost/state", None).await;
    assert_eq!(v["values"]["log"], json!(["first", "first", "second"]));
    let (_, v) = call(
        &app,
        &[],
        "GET",
        &format!("/triggers/{trigger_id}/events"),
        None,
    )
    .await;
    assert_eq!(v.as_array().unwrap().len(), 2);
    let (_, v) = call(
        &app,
        &[],
        "GET",
        &format!("/triggers/{trigger_id}/dead-letter"),
        None,
    )
    .await;
    assert_eq!(v.as_array().unwrap().len(), 1);

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Enable / disable
// --------------------------------------------------------------------- //

#[tokio::test]
async fn disabled_trigger_rejects_events_until_re_enabled() {
    let (app, store) = open_app();
    let (status, _) = call(
        &app,
        &[],
        "POST",
        "/assistants",
        Some(json!({"name": "bot", "graph": "pipeline", "assistant_id": "bot"}).to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let trigger = create_trigger(
        &app,
        None,
        json!({
            "name": "switchable",
            "target": {"kind": "assistant", "id": "bot"},
            "action": "start_run",
        }),
    )
    .await;
    let trigger_id = trigger["trigger_id"].as_str().unwrap().to_string();
    let secret = trigger["secret"].as_str().unwrap().to_string();

    // PATCH disables the trigger; signed events are then refused (and not
    // logged — nothing was accepted).
    let (status, v) = call(
        &app,
        &[],
        "PATCH",
        &format!("/triggers/{trigger_id}"),
        Some(json!({"enabled": false}).to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "disable failed: {v}");
    assert_eq!(v["enabled"], json!(false));
    let (status, v) = fire(&app, &trigger_id, &secret, &json!({"n": 1})).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "disabled trigger must refuse events: {v}"
    );
    let (_, v) = call(
        &app,
        &[],
        "GET",
        &format!("/triggers/{trigger_id}/events"),
        None,
    )
    .await;
    assert_eq!(v, json!([]));

    // Re-enable: events flow again.
    let (status, _) = call(
        &app,
        &[],
        "PATCH",
        &format!("/triggers/{trigger_id}"),
        Some(json!({"enabled": true}).to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, v) = fire(&app, &trigger_id, &secret, &json!({"n": 1})).await;
    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "re-enabled trigger failed: {v}"
    );
    assert_eq!(v["status"], json!("executed"));

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Tenant isolation
// --------------------------------------------------------------------- //

#[tokio::test]
async fn triggers_are_isolated_between_tenants() {
    let (app, store) = multi_tenant_app();

    // acme owns an assistant `bot` and a trigger `shared-t`.
    let (status, _) = call(
        &app,
        &[ACME],
        "POST",
        "/assistants",
        Some(json!({"name": "acme-bot", "graph": "pipeline", "assistant_id": "bot"}).to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let trigger = create_trigger(
        &app,
        Some(ACME),
        json!({
            "name": "acme-bot",
            "target": {"kind": "assistant", "id": "bot"},
            "action": "start_run",
            "trigger_id": "shared-t",
        }),
    )
    .await;
    let acme_secret = trigger["secret"].as_str().unwrap().to_string();

    // Cross-tenant reads/writes answer 404 (never 403): globex cannot see,
    // patch, delete, inspect, or replay acme's trigger.
    for (method, uri) in [
        ("GET", "/triggers/shared-t".to_string()),
        ("GET", "/triggers/shared-t/events".to_string()),
        ("GET", "/triggers/shared-t/dead-letter".to_string()),
        ("PATCH", "/triggers/shared-t".to_string()),
        ("DELETE", "/triggers/shared-t".to_string()),
        (
            "POST",
            "/triggers/shared-t/events/some-event/replay".to_string(),
        ),
    ] {
        let body = if method == "PATCH" {
            Some(json!({"enabled": false}).to_string())
        } else {
            None
        };
        let (status, v) = call(&app, &[GLOBEX], method, &uri, body).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "globex reached acme's trigger via {method} {uri}: {v}"
        );
    }
    let (_, v) = call(&app, &[GLOBEX], "GET", "/triggers", None).await;
    assert_eq!(v, json!([]));

    // The same external id is free for globex: full coexistence.
    let (status, _) = call(
        &app,
        &[GLOBEX],
        "POST",
        "/assistants",
        Some(json!({"name": "globex-bot", "graph": "pipeline", "assistant_id": "bot"}).to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let globex_trigger = create_trigger(
        &app,
        Some(GLOBEX),
        json!({
            "name": "globex-bot",
            "target": {"kind": "assistant", "id": "bot"},
            "action": "start_run",
            "trigger_id": "shared-t",
        }),
    )
    .await;
    let globex_secret = globex_trigger["secret"].as_str().unwrap().to_string();

    // Each tenant lists exactly its own trigger.
    for auth in [ACME, GLOBEX] {
        let (_, v) = call(&app, &[auth], "GET", "/triggers", None).await;
        let listed = v.as_array().unwrap();
        assert_eq!(listed.len(), 1, "tenant must see exactly one trigger");
        assert_eq!(listed[0]["trigger_id"], json!("shared-t"));
    }

    // The webhook is unauthenticated: the signature alone resolves which
    // tenant's same-id trigger owns the event.
    let (status, v) = fire(&app, "shared-t", &acme_secret, &json!({"n": 1})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "acme webhook failed: {v}");
    let acme_run = v["run_id"].as_str().unwrap().to_string();
    let (status, _) = fire(&app, "shared-t", &globex_secret, &json!({"n": 2})).await;
    assert_eq!(status, StatusCode::ACCEPTED);

    // The event landed on acme's log only; the run it fired is acme's
    // (globex gets 404 polling it), and its thread lives in acme's subtree.
    let (_, v) = call(&app, &[ACME], "GET", "/triggers/shared-t/events", None).await;
    assert_eq!(v.as_array().unwrap().len(), 1);
    assert_eq!(v[0]["payload"], json!({"n": 1}));
    let (_, v) = call(&app, &[GLOBEX], "GET", "/triggers/shared-t/events", None).await;
    assert_eq!(v.as_array().unwrap().len(), 1);
    assert_eq!(v[0]["payload"], json!({"n": 2}));
    let (status, _) = call(&app, &[GLOBEX], "GET", &format!("/runs/{acme_run}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let run = wait_run(&app, Some(ACME), &acme_run).await;
    assert_eq!(run["status"], json!("success"));
    let thread_id = run["thread_id"].as_str().unwrap().to_string();
    assert!(store.join("acme").join(&thread_id).is_dir());
    assert!(!store.join("globex").join(&thread_id).exists());

    // On disk the trigger records are separated per tenant.
    assert!(store
        .join("triggers")
        .join("acme")
        .join("shared-t.json")
        .exists());
    assert!(store
        .join("triggers")
        .join("globex")
        .join("shared-t.json")
        .exists());
    assert!(!store.join("triggers").join("shared-t.json").exists());

    let _ = std::fs::remove_dir_all(store);
}
