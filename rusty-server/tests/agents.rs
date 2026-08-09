//! Agent Fabric integration tests (R0.7, wave 1): the `/agents` HTTP
//! surface over the default JSON-file backend — registry CRUD and tenant
//! isolation, manifest-validated mailbox sends, the activation lease
//! (claim / conflict / steal / heartbeat / release), turn-serialized
//! mailbox draining, and restart durability.
//!
//! Driven in-process via `tower::ServiceExt::oneshot` (no sockets), the
//! `tasks.rs` convention. The agent surface never touches the graph
//! registry, so the registry stays empty here. Live-Postgres coverage of
//! the same semantics (plus concurrent activation claims) is gated in
//! `postgres_agents.rs`; the crash-survival exit criterion is
//! `agent_recovery.rs`.

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
    std::env::temp_dir().join(format!("rusty-server-agents-test-{}", uuid::Uuid::new_v4()))
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

/// The manifest the tests register: two accepted message kinds, one with a
/// declared payload schema (stored, not yet validated — wave 1).
fn manifest() -> Value {
    json!({
        "agent_kind": "researcher",
        "manifest_version": "researcher/1.4.0",
        "accepts": {
            "summarize": {"kind": "application/json"},
            "triage": {
                "kind": "application/json",
                "max_bytes": 65536,
                "schema": {
                    "$schema": "https://json-schema.org/draft/2020-12/schema",
                    "type": "object"
                }
            }
        },
        "scopes": ["private", "team"],
        "budget": {"max_tokens": 250000}
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
    assert_eq!(v["agent_id"], json!(agent_id));
    assert_eq!(v["manifest"]["agent_kind"], json!("researcher"));
}

/// Activate an agent; returns `(status, body)`.
async fn activate(
    app: &Router,
    agent_id: &str,
    worker: &str,
    lease_ms: u64,
) -> (StatusCode, Value) {
    call(
        app,
        "POST",
        &format!("/agents/{agent_id}/activate"),
        Some(json!({"worker_id": worker, "lease_ms": lease_ms})),
    )
    .await
}

/// Activate, asserting success; returns the granted fencing ordinal.
async fn activate_one(app: &Router, agent_id: &str, worker: &str, lease_ms: u64) -> u64 {
    let (status, v) = activate(app, agent_id, worker, lease_ms).await;
    assert_eq!(status, StatusCode::OK, "activate failed: {v}");
    assert_eq!(v["owner"], json!(worker));
    v["fencing"].as_u64().unwrap()
}

/// Send a mailbox message; returns `(status, body)`.
async fn send(app: &Router, agent_id: &str, kind: &str, extra: Value) -> (StatusCode, Value) {
    let mut body = json!({"kind": kind, "payload": {"n": 1}});
    body.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    call(
        app,
        "POST",
        &format!("/agents/{agent_id}/mailbox"),
        Some(body),
    )
    .await
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

// --------------------------------------------------------------------- //
// Registry
// --------------------------------------------------------------------- //

#[tokio::test]
async fn registry_create_get_list_and_conflict() {
    let (app, store) = app();
    register(&app, "researcher-1").await;

    // Fetch: the full record, external id on the wire (tenant is internal).
    let (status, v) = call(&app, "GET", "/agents/researcher-1", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["agent_id"], json!("researcher-1"));
    assert_eq!(v["manifest"]["manifest_version"], json!("researcher/1.4.0"));
    assert_eq!(
        v["manifest"]["accepts"]["triage"]["schema"]["type"],
        json!("object"),
        "the declared payload schema is stored verbatim"
    );
    assert!(v["created_at"].is_string());

    // Unknown id is a 404.
    let (status, _) = call(&app, "GET", "/agents/nobody", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Re-registering the id conflicts.
    let (status, _) = call(
        &app,
        "POST",
        "/agents",
        Some(json!({"agent_id": "researcher-1", "manifest": manifest()})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // A manifest that does not parse as a CapabilityManifest is a 400.
    let (status, v) = call(
        &app,
        "POST",
        "/agents",
        Some(json!({"agent_id": "bad", "manifest": {"agent_kind": "x"}})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "missing version: {v}");

    // List: oldest first, both registrations present.
    register(&app, "researcher-2").await;
    let (status, v) = call(&app, "GET", "/agents", None).await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<&str> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["agent_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["researcher-1", "researcher-2"]);

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn registry_and_mailboxes_are_tenant_isolated() {
    let (app, store) = multi_tenant_app();
    let (status, v) = call_as(
        &app,
        Some(ACME),
        "POST",
        "/agents",
        Some(json!({"agent_id": "acme-agent", "manifest": manifest()})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "register failed: {v}");

    // Globex cannot see, address, or activate acme's agent — every handle
    // is a 404, indistinguishable from never registered.
    let (status, _) = call_as(&app, Some(GLOBEX), "GET", "/agents/acme-agent", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, v) = call_as(&app, Some(GLOBEX), "GET", "/agents", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v, json!([]));
    let (status, _) = call_as(
        &app,
        Some(GLOBEX),
        "POST",
        "/agents/acme-agent/mailbox",
        Some(json!({"kind": "summarize", "payload": {}})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call_as(
        &app,
        Some(GLOBEX),
        "POST",
        "/agents/acme-agent/activate",
        Some(json!({"worker_id": "w", "lease_ms": 1000})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A same-named globex agent is a distinct registration, not a conflict.
    let (status, _) = call_as(
        &app,
        Some(GLOBEX),
        "POST",
        "/agents",
        Some(json!({"agent_id": "acme-agent", "manifest": manifest()})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Mailbox sends
// --------------------------------------------------------------------- //

#[tokio::test]
async fn mailbox_send_validates_agent_and_kind() {
    let (app, store) = app();

    // Unknown agent: 404 before any kind check.
    let (status, _) = send(&app, "ghost", "summarize", json!({})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    register(&app, "researcher-1").await;

    // A kind the manifest does not declare fails fast at submission.
    let (status, v) = send(&app, "researcher-1", "exfiltrate", json!({})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "undeclared kind: {v}");

    // A declared kind is accepted and lands as an addressable task.
    let (status, v) = send(&app, "researcher-1", "summarize", json!({})).await;
    assert_eq!(status, StatusCode::CREATED, "send failed: {v}");
    assert_eq!(v["deduplicated"], json!(false));
    let task_id = v["task_id"].as_str().unwrap().to_string();
    let (status, v) = call(&app, "GET", &format!("/tasks/{task_id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["recipient"], json!("agent:researcher-1"));
    assert_eq!(v["kind"], json!("summarize"));

    // Idempotency: re-sending with the same key deduplicates.
    let keyed = json!({"idempotency_key": "msg-1"});
    let (status, v) = send(&app, "researcher-1", "triage", keyed.clone()).await;
    assert_eq!(status, StatusCode::CREATED, "first keyed send: {v}");
    let first_id = v["task_id"].as_str().unwrap().to_string();
    let (status, v) = send(&app, "researcher-1", "triage", keyed).await;
    assert_eq!(status, StatusCode::OK, "dedup resend: {v}");
    assert_eq!(v["deduplicated"], json!(true));
    assert_eq!(v["task_id"], json!(first_id));

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn direct_enqueue_accepts_and_validates_a_recipient() {
    let (app, store) = app();

    // POST /tasks is the embedders' direct-queue path: a well-formed
    // recipient is stored and round-trips on the wire.
    let (status, v) = call(
        &app,
        "POST",
        "/tasks",
        Some(json!({
            "kind": "summarize",
            "payload": {},
            "recipient": "agent:embedded-1",
            "idempotency_key": "direct-1"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "direct send: {v}");
    let task_id = v["task_id"].as_str().unwrap().to_string();
    let (status, v) = call(&app, "GET", &format!("/tasks/{task_id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["recipient"], json!("agent:embedded-1"));

    // A malformed recipient is rejected — strict now, loosening later is
    // the compatible direction.
    for bad in ["embedded-1", "agent:", "agent:has space", "agent:has/slash"] {
        let (status, v) = call(
            &app,
            "POST",
            "/tasks",
            Some(json!({"kind": "k", "payload": {}, "recipient": bad})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "recipient `{bad}`: {v}");
    }

    // Tasks without a recipient show `null` on the wire — the pre-R0.7
    // shape gains one nullable field, nothing else.
    let (status, v) = call(
        &app,
        "POST",
        "/tasks",
        Some(json!({"kind": "k", "payload": {}})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "plain enqueue: {v}");
    let task_id = v["task_id"].as_str().unwrap().to_string();
    let (_, v) = call(&app, "GET", &format!("/tasks/{task_id}"), None).await;
    assert_eq!(v["recipient"], Value::Null);

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn pool_claim_never_hands_out_mailbox_traffic() {
    let (app, store) = app();
    register(&app, "researcher-1").await;
    let (status, v) = send(&app, "researcher-1", "summarize", json!({})).await;
    assert_eq!(status, StatusCode::CREATED, "send failed: {v}");

    // The pool claim — any pool, no version pin — must never lease a
    // message addressed to an agent, even when nothing else is queued.
    let (status, _) = call(
        &app,
        "POST",
        "/tasks/claim",
        Some(json!({"worker_id": "pool-worker", "lease_ms": 30_000})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "pool claim must not see mailbox traffic"
    );

    // Ordinary and mailbox work coexist: the pool claim takes the ordinary
    // task, the mailbox message stays put for the agent claim.
    let (status, _) = call(
        &app,
        "POST",
        "/tasks",
        Some(json!({"kind": "reindex", "payload": {}})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, v) = call(
        &app,
        "POST",
        "/tasks/claim",
        Some(json!({"worker_id": "pool-worker", "lease_ms": 30_000})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["task"]["kind"], json!("reindex"));
    assert_eq!(v["task"]["recipient"], Value::Null);

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Activation leases
// --------------------------------------------------------------------- //

#[tokio::test]
async fn activation_claim_conflict_steal_and_fencing() {
    let (app, store) = app();
    register(&app, "researcher-1").await;

    // First claim: fencing 1.
    let fencing = activate_one(&app, "researcher-1", "worker-1", 30_000).await;
    assert_eq!(fencing, 1);

    // A second host cannot take a live activation; the 409 names the
    // holder so the loser can back off until expiry.
    let (status, v) = activate(&app, "researcher-1", "worker-2", 30_000).await;
    assert_eq!(status, StatusCode::CONFLICT, "live steal: {v}");

    // A same-owner re-claim of a live lease is also Held — the heartbeat
    // is the renewal path, not a second activate.
    let (status, v) = activate(&app, "researcher-1", "worker-1", 30_000).await;
    assert_eq!(status, StatusCode::CONFLICT, "same-owner re-claim: {v}");
    let _ = std::fs::remove_dir_all(&store);

    // Fresh store for the steal half: short lease, wait it out, steal.
    let store = temp_store();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone());
    let app = router(GraphRegistry::new(), config);
    register(&app, "researcher-1").await;
    let fencing = activate_one(&app, "researcher-1", "worker-1", 100).await;
    assert_eq!(fencing, 1);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let (status, v) = activate(&app, "researcher-1", "worker-2", 30_000).await;
    assert_eq!(status, StatusCode::OK, "expired steal: {v}");
    assert_eq!(v["owner"], json!("worker-2"));
    assert_eq!(v["fencing"], json!(2), "steal bumps the fencing ordinal");

    // The stale holder's fencing pair is rejected everywhere now.
    let (status, _) = call(
        &app,
        "POST",
        "/agents/researcher-1/activate/heartbeat",
        Some(json!({"worker_id": "worker-1", "fencing": 1, "lease_ms": 30_000})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "stale heartbeat must lose");

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn activation_heartbeat_and_release() {
    let (app, store) = app();
    register(&app, "researcher-1").await;
    let fencing = activate_one(&app, "researcher-1", "worker-1", 30_000).await;

    // Heartbeat extends the held lease.
    let (status, v) = call(
        &app,
        "POST",
        "/agents/researcher-1/activate/heartbeat",
        Some(json!({"worker_id": "worker-1", "fencing": fencing, "lease_ms": 60_000})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "heartbeat failed: {v}");
    assert_eq!(v["fencing"], json!(fencing));

    // The wrong owner or the wrong fencing ordinal is a 409.
    let (status, _) = call(
        &app,
        "POST",
        "/agents/researcher-1/activate/heartbeat",
        Some(json!({"worker_id": "worker-2", "fencing": fencing, "lease_ms": 60_000})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = call(
        &app,
        "POST",
        "/agents/researcher-1/activate/heartbeat",
        Some(json!({"worker_id": "worker-1", "fencing": fencing + 1, "lease_ms": 60_000})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Release frees the activation promptly: a replacement claims it
    // without waiting out the expiry (fencing restarts — the old lease is
    // gone, not stolen).
    let (status, v) = call(
        &app,
        "POST",
        "/agents/researcher-1/activate/release",
        Some(json!({"worker_id": "worker-1", "fencing": fencing})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "release failed: {v}");
    assert_eq!(v["released"], json!(true));
    let (status, _) = call(
        &app,
        "POST",
        "/agents/researcher-1/activate/heartbeat",
        Some(json!({"worker_id": "worker-1", "fencing": fencing, "lease_ms": 60_000})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "a released lease is gone");
    let fencing = activate_one(&app, "researcher-1", "worker-2", 30_000).await;
    assert_eq!(fencing, 1);

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Turn-serialized mailbox draining
// --------------------------------------------------------------------- //

#[tokio::test]
async fn mailbox_next_requires_the_activation_lease() {
    let (app, store) = app();
    register(&app, "researcher-1").await;
    let (status, _) = send(&app, "researcher-1", "summarize", json!({})).await;
    assert_eq!(status, StatusCode::CREATED);

    // No activation held: 409, and the message stays queued.
    let (status, v) = next(&app, "researcher-1", "worker-1", 1).await;
    assert_eq!(status, StatusCode::CONFLICT, "no activation: {v}");

    // The wrong fencing pair is the same answer.
    let fencing = activate_one(&app, "researcher-1", "worker-1", 30_000).await;
    let (status, _) = next(&app, "researcher-1", "worker-1", fencing + 1).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = next(&app, "researcher-1", "worker-2", fencing).await;
    assert_eq!(status, StatusCode::CONFLICT);

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn mailbox_drains_one_turn_at_a_time_oldest_first() {
    let (app, store) = app();
    register(&app, "researcher-1").await;
    let fencing = activate_one(&app, "researcher-1", "worker-1", 30_000).await;

    let (status, v) = send(&app, "researcher-1", "summarize", json!({})).await;
    assert_eq!(status, StatusCode::CREATED);
    let first_id = v["task_id"].as_str().unwrap().to_string();
    let (status, v) = send(&app, "researcher-1", "triage", json!({})).await;
    assert_eq!(status, StatusCode::CREATED);
    let second_id = v["task_id"].as_str().unwrap().to_string();

    // Status before the first turn: two queued, none in flight.
    let (status, v) = call(&app, "GET", "/agents/researcher-1/status", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        v["mailbox"],
        json!({"queued": 2, "in_flight": 0, "dead": 0})
    );
    assert_eq!(v["activation"]["owner"], json!("worker-1"));

    // First claim: the oldest message, leased to the holder.
    let (status, v) = next(&app, "researcher-1", "worker-1", fencing).await;
    assert_eq!(status, StatusCode::OK, "first turn: {v}");
    assert_eq!(v["task"]["task_id"], json!(first_id));
    assert_eq!(v["task"]["recipient"], json!("agent:researcher-1"));
    assert_eq!(v["task"]["attempt"], json!(1));

    // Turn serialization is server-enforced: while the first message is
    // leased, the mailbox answers empty — not the second message.
    let (status, _) = next(&app, "researcher-1", "worker-1", fencing).await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "a turn in flight makes the whole mailbox unclaimable"
    );
    let (_, v) = call(&app, "GET", "/agents/researcher-1/status", None).await;
    assert_eq!(
        v["mailbox"],
        json!({"queued": 1, "in_flight": 1, "dead": 0})
    );

    // Settle the turn through the ordinary task protocol; the next
    // message becomes claimable.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/tasks/{first_id}/complete"),
        Some(json!({"worker_id": "worker-1", "result": {"summary": "done"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "complete failed: {v}");
    let (status, v) = next(&app, "researcher-1", "worker-1", fencing).await;
    assert_eq!(status, StatusCode::OK, "second turn: {v}");
    assert_eq!(v["task"]["task_id"], json!(second_id));

    // Drained: 204.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/tasks/{second_id}/complete"),
        Some(json!({"worker_id": "worker-1", "result": {"triaged": true}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "complete failed: {v}");
    let (status, _) = next(&app, "researcher-1", "worker-1", fencing).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn registrations_leases_and_mailboxes_survive_a_restart() {
    let store = temp_store();
    let (app, _) = {
        let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone());
        (router(GraphRegistry::new(), config), ())
    };
    register(&app, "researcher-1").await;
    let fencing = activate_one(&app, "researcher-1", "worker-1", 30_000).await;
    let (status, v) = send(&app, "researcher-1", "summarize", json!({})).await;
    assert_eq!(status, StatusCode::CREATED);
    let task_id = v["task_id"].as_str().unwrap().to_string();
    drop(app);

    // A new router over the same store path: the JSON files are the
    // state, so everything is exactly where the crash left it.
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone());
    let app = router(GraphRegistry::new(), config);
    let (status, v) = call(&app, "GET", "/agents/researcher-1", None).await;
    assert_eq!(status, StatusCode::OK, "registration survives: {v}");
    let (_, v) = call(&app, "GET", "/agents/researcher-1/status", None).await;
    assert_eq!(v["activation"]["fencing"], json!(fencing));
    assert_eq!(v["mailbox"]["queued"], json!(1));

    // The held activation still gates, and still claims, after the
    // restart.
    let (status, v) = next(&app, "researcher-1", "worker-1", fencing).await;
    assert_eq!(status, StatusCode::OK, "claim after restart: {v}");
    assert_eq!(v["task"]["task_id"], json!(task_id));

    let _ = std::fs::remove_dir_all(store);
}
