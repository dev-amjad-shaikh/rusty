//! Live-Postgres integration tests for Agent Fabric wave 3 (R0.7): the
//! coordination patterns over the `server_coordinations` payload column,
//! the `parent` / `tokens` / `cost_usd` task columns, and the single
//! transaction behind `journal_and_enqueue` (journal upsert + outbox rows
//! commit atomically — the file backend's crash-window imperfection does
//! not exist here).
//!
//! The detailed guarantee assertions live in `coordination.rs` over the
//! JSON-file backend; this suite proves parity for the three patterns
//! whose settle paths differ most: delegate (the causal chain), race
//! (waste accounting off the settlement-cost columns), and quorum (the
//! resolver record through one crash).
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
//!   cargo test --features postgres --test postgres_coordination -- --ignored
//! ```

#![cfg(feature = "postgres")]

use std::path::PathBuf;
use std::time::{Duration, Instant};

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

/// An app whose server store is Postgres-backed, relay polling every
/// 50 ms so outbox-submitted member tasks land quickly.
///
/// Every call runs as the dedicated `pg-coordination` tenant: the
/// Postgres test binaries run in parallel against one scratch database,
/// and tenant isolation keeps the suites blind to each other (the
/// `postgres_supervision` convention).
fn postgres_app() -> Router {
    let store_path: PathBuf = std::env::temp_dir().join(format!(
        "rusty-server-pg-coordination-{}",
        uuid::Uuid::new_v4()
    ));
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store_path)
        .with_postgres(database_url())
        .with_outbox_relay_interval(Duration::from_millis(50))
        .with_tenant_key("pg-coordination", "pg-coordination-secret");
    router(GraphRegistry::new(), config)
}

/// Send a request as the suite's tenant; returns `(status,
/// json-body-or-null)`.
async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("x-api-key", "pg-coordination-secret");
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

/// The member manifest: accepts `work`, declares both scopes.
fn member_manifest() -> Value {
    json!({
        "agent_kind": "worker",
        "manifest_version": "worker/1.0.0",
        "accepts": {"work": {"kind": "application/json"}},
        "scopes": ["private", "team"]
    })
}

/// The delegator manifest: accepts the reserved `coordination_result`.
fn delegator_manifest() -> Value {
    json!({
        "agent_kind": "delegator",
        "manifest_version": "delegator/1.0.0",
        "accepts": {"coordination_result": {"kind": "application/json"}}
    })
}

/// Register an agent as the suite's tenant; asserts 201.
async fn register(app: &Router, agent_id: &str, manifest: Value) {
    let (status, v) = call(
        app,
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

/// Poll `GET /tasks/{id}` until the relay has published the task.
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

/// Poll the mailbox claim until `task_id` is leased to this worker.
async fn claim_task(app: &Router, agent_id: &str, worker: &str, fencing: u64, task_id: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let (status, v) = call(
            app,
            "POST",
            &format!("/agents/{agent_id}/mailbox/next"),
            Some(json!({"worker_id": worker, "fencing": fencing, "lease_ms": 30_000})),
        )
        .await;
        if status == StatusCode::OK {
            assert_eq!(
                v["task"]["task_id"],
                json!(task_id),
                "claimed an unexpected task: {v}"
            );
            return;
        }
        assert!(
            Instant::now() < deadline,
            "task `{task_id}` never became claimable for `{agent_id}`"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Complete the held task; asserts 200. `extra` carries cost evidence.
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

/// Fail the held task terminally (a non-retryable class); asserts 200.
async fn fail_task(app: &Router, task_id: &str, worker: &str, extra: Value) {
    let mut body = json!({"worker_id": worker, "error_class": "invalid_input",
                "message": "turn failed (invalid_input)", "retryable": false});
    body.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    let (status, v) = call(app, "POST", &format!("/tasks/{task_id}/fail"), Some(body)).await;
    assert_eq!(status, StatusCode::OK, "fail failed: {v}");
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

/// A `Delegation` wire object pinning `worker/1.0.0` and the `work` kind.
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

/// The deterministic member task id for the suite's tenant.
fn member_task_id(coordination_id: &str, member: &str) -> String {
    format!("pg-coordination--{coordination_id}--{member}")
}

/// Delegate over Postgres: the full causal chain, the `parent` column
/// carrying the send event id onto the member task, and the outcome
/// message delivered to the delegator's mailbox.
#[tokio::test]
#[ignore = "requires DATABASE_URL (scratch Postgres)"]
async fn pg_delegate_journals_the_full_causal_chain() {
    let app = postgres_app();
    let cid = format!("pgd-{}", &uniq()[..12]);
    register(&app, "boss", delegator_manifest()).await;
    register(&app, "writer", member_manifest()).await;

    let (status, v) = call(
        &app,
        "POST",
        "/coordination/delegate",
        Some(json!({
            "coordination_id": cid,
            "delegator": "boss",
            "delegate": {
                "delegate": delegation("solo", "writer", json!({"brief": "q3"}),
                                       json!({"effect": "idempotent"})),
                "handoff": true
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "submit failed: {v}");
    assert_eq!(
        v["submitted"],
        json!([{"member": "solo", "task_id": member_task_id(&cid, "solo")}])
    );

    let fence = activate_one(&app, "writer", "host-w", 60_000).await;
    claim_task(
        &app,
        "writer",
        "host-w",
        fence,
        &member_task_id(&cid, "solo"),
    )
    .await;
    complete_task(
        &app,
        &member_task_id(&cid, "solo"),
        "host-w",
        json!({"draft": "revenue up 12%"}),
        json!({"tokens": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15},
               "cost_usd": 0.01}),
    )
    .await;

    let record = get_coordination(&app, &cid).await;
    assert_eq!(record["settled"], json!(true));
    assert_eq!(record["outcome"]["status"], json!("completed"));
    assert_eq!(
        record["outcome"]["result"],
        json!({"kind": "inline", "value": {"draft": "revenue up 12%"}})
    );
    // The journal rows round-trip through the journal upsert: full chain,
    // parented start/send/start.
    let events = record["journal"]["events"].as_array().unwrap();
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
    // The member task's `parent` column carries the send event id.
    let task = wait_task(&app, &member_task_id(&cid, "solo")).await;
    assert_eq!(task["parent"], events[1]["id"]);
    // The outcome message reaches the delegator's mailbox.
    let outcome_id = format!("pg-coordination--{cid}--outcome");
    let outcome_task = wait_task(&app, &outcome_id).await;
    assert_eq!(outcome_task["recipient"], json!("agent:boss"));
    assert_eq!(outcome_task["payload"]["status"], json!("completed"));
}

/// Race over Postgres: the winner's result, the loser's settlement cost
/// read off the `tokens` / `cost_usd` columns into the outcome's waste
/// accounting.
#[tokio::test]
#[ignore = "requires DATABASE_URL (scratch Postgres)"]
async fn pg_race_first_completion_wins_and_waste_is_accounted() {
    let app = postgres_app();
    let cid = format!("pgr-{}", &uniq()[..12]);
    for agent in ["ra", "rb", "rc"] {
        register(&app, agent, member_manifest()).await;
    }
    let (status, v) = call(
        &app,
        "POST",
        "/coordination/race",
        Some(json!({
            "coordination_id": cid,
            "race": {
                "candidates": [
                    delegation("a", "ra", json!({"bid": "A"}), json!({"effect": "idempotent"})),
                    delegation("b", "rb", json!({"bid": "B"}), json!({"effect": "idempotent"})),
                    delegation("c", "rc", json!({"bid": "C"}), json!({"effect": "idempotent"}))
                ]
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "submit failed: {v}");
    for member in ["a", "b", "c"] {
        wait_task(&app, &member_task_id(&cid, member)).await;
    }

    // c dies terminally with reported cost; b completes and wins; a is
    // cancel-signalled queued.
    let fc = activate_one(&app, "rc", "host-c", 60_000).await;
    claim_task(&app, "rc", "host-c", fc, &member_task_id(&cid, "c")).await;
    fail_task(
        &app,
        &member_task_id(&cid, "c"),
        "host-c",
        json!({"tokens": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15},
               "cost_usd": 0.05}),
    )
    .await;
    let fb = activate_one(&app, "rb", "host-b", 60_000).await;
    claim_task(&app, "rb", "host-b", fb, &member_task_id(&cid, "b")).await;
    complete_task(
        &app,
        &member_task_id(&cid, "b"),
        "host-b",
        json!({"bid": "B"}),
        json!({}),
    )
    .await;

    let record = get_coordination(&app, &cid).await;
    assert_eq!(record["settled"], json!(true));
    let outcome = &record["outcome"];
    assert_eq!(outcome["status"], json!("completed"));
    assert_eq!(
        outcome["result"],
        json!({"kind": "inline", "value": {"bid": "B"}})
    );
    assert_eq!(outcome["wasted_cost_usd"], json!(0.05));
    assert_eq!(outcome["wasted_tokens"], json!(15));
    let settlements: Vec<&str> = outcome["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["settlement"].as_str().unwrap())
        .collect();
    assert_eq!(settlements, ["cancelled", "completed", "failed"]);
}

/// Quorum over Postgres: a crashed juror mid-pattern, the strict-majority
/// resolution, and the resolver record round-tripping the payload column.
#[tokio::test]
#[ignore = "requires DATABASE_URL (scratch Postgres)"]
async fn pg_quorum_majority_resolves_with_a_crashed_juror() {
    let app = postgres_app();
    let cid = format!("pgq-{}", &uniq()[..12]);
    for agent in ["j1", "j2", "j3", "j4"] {
        register(&app, agent, member_manifest()).await;
    }
    let juror = |member: &str, agent: &str| delegation(member, agent, json!({}), json!({}));
    let (status, v) = call(
        &app,
        "POST",
        "/coordination/quorum",
        Some(json!({
            "coordination_id": cid,
            "quorum": {
                "members": [
                    juror("p", "j1"),
                    juror("q", "j2"),
                    juror("r", "j3"),
                    juror("s", "j4")
                ],
                "threshold": 3,
                "resolver": {"resolver": "majority_equal"}
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "submit failed: {v}");
    for member in ["p", "q", "r", "s"] {
        wait_task(&app, &member_task_id(&cid, member)).await;
    }

    // j4 crashes mid-pattern: claimed with a 100 ms lease, never settled.
    let f4 = activate_one(&app, "j4", "host-4", 60_000).await;
    let (status, _) = call(
        &app,
        "POST",
        "/agents/j4/mailbox/next",
        Some(json!({"worker_id": "host-4", "fencing": f4, "lease_ms": 100})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "j4 claim failed");
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Three jurors land X, X, Y — the pattern settles without j4.
    let f1 = activate_one(&app, "j1", "host-1", 60_000).await;
    claim_task(&app, "j1", "host-1", f1, &member_task_id(&cid, "p")).await;
    complete_task(
        &app,
        &member_task_id(&cid, "p"),
        "host-1",
        json!({"answer": "X"}),
        json!({}),
    )
    .await;
    let f2 = activate_one(&app, "j2", "host-2", 60_000).await;
    claim_task(&app, "j2", "host-2", f2, &member_task_id(&cid, "q")).await;
    complete_task(
        &app,
        &member_task_id(&cid, "q"),
        "host-2",
        json!({"answer": "X"}),
        json!({}),
    )
    .await;
    let f3 = activate_one(&app, "j3", "host-3", 60_000).await;
    claim_task(&app, "j3", "host-3", f3, &member_task_id(&cid, "r")).await;
    complete_task(
        &app,
        &member_task_id(&cid, "r"),
        "host-3",
        json!({"answer": "Y"}),
        json!({}),
    )
    .await;

    let record = get_coordination(&app, &cid).await;
    assert_eq!(record["settled"], json!(true));
    let outcome = &record["outcome"];
    assert_eq!(outcome["status"], json!("completed"));
    assert_eq!(
        outcome["result"],
        json!({"kind": "inline", "value": {"answer": "X"}})
    );
    let resolver = &outcome["resolver"];
    assert_eq!(resolver["resolver"], json!({"resolver": "majority_equal"}));
    assert_eq!(
        resolver["inputs"],
        json!([{"answer": "X"}, {"answer": "X"}, {"answer": "Y"}])
    );
    assert_eq!(resolver["decided"], json!(true));
    // The crashed juror is journaled cancelled — evidence, not silence.
    let settlements: Vec<&str> = outcome["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["settlement"].as_str().unwrap())
        .collect();
    assert_eq!(
        settlements,
        ["completed", "completed", "completed", "cancelled"]
    );
}
