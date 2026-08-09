//! The executor-policy plane integration tests (R0.8 Rusty Learn, wave 4):
//! the `/policy/*` registry surface over the default JSON-file backend —
//! immutable versioned bodies, the append-only activation log, the derived
//! epoch history — plus the admission binding (new runs pin the active
//! version, resumed runs keep their pin), the `policy_decision` journal
//! emission on task failure, the promotion/rollback hooks that move the
//! registry when a *learned* policy arrives, tenant isolation, and restart
//! durability. Live-Postgres coverage of the restart semantics is the gated
//! section at the bottom (`RUSTY_TEST_DATABASE_URL`).
//!
//! Driven in-process via `tower::ServiceExt::oneshot` (no sockets), the
//! `learn_gate.rs` convention.

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use chrono::{DateTime, Utc};
use rusty_agent_runtime::effects::ApprovalToken;
use rusty_agent_runtime::error::Result as RuntimeResult;
use rusty_agent_runtime::learn::{
    promotion_effect_id, Candidate, CandidateContent, CandidateEvaluation, CandidateEvaluator,
    EvaluationRequest, EvaluationVerdict, EvidenceSpan, ReplaySummary,
};
use rusty_agent_runtime::memory::ProvenanceAuthor;
use rusty_agent_runtime::record::{
    derive_policy_version, DecisionFamily, ExecutorPolicy, PolicyVersion,
};
use rusty_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

// --------------------------------------------------------------------- //
// Harness
// --------------------------------------------------------------------- //

/// Unique temp store root, removed at the end of each test (best effort).
fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!("rusty-server-policy-test-{}", uuid::Uuid::new_v4()))
}

/// A fixed timestamp, so candidates minted here are deterministic.
fn ts(millis: i64) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp_millis(millis).unwrap()
}

/// The scripted evaluator (the wave-3 fixture): a clean verdict showing
/// improvement, enough to clear every auto bar. Wave 4's registry tests
/// need promotions to succeed; the *real* evaluator composition is the
/// release proof's business (`learn_release.rs`).
#[derive(Debug)]
struct FixedEvaluator;

#[async_trait::async_trait]
impl CandidateEvaluator for FixedEvaluator {
    async fn evaluate(
        &self,
        candidate: &Candidate,
        request: &EvaluationRequest,
    ) -> RuntimeResult<CandidateEvaluation> {
        let report = |rate: f64| {
            json!({
                "format_version": 1,
                "name": format!("scripted@{}", request.dataset_version),
                "dataset_version": request.dataset_version,
                "summary": {"run_pass_rate": rate},
            })
        };
        Ok(CandidateEvaluation {
            candidate_id: candidate.candidate_id.clone(),
            dataset_version: request.dataset_version.clone(),
            replay: ReplaySummary {
                fixture_ids: Vec::new(),
                matched: 0,
                divergences: Vec::new(),
            },
            baseline_report: report(0.5),
            candidate_report: report(1.0),
            verdict: EvaluationVerdict {
                regressed: false,
                target_metric: request.target_metric.clone(),
                baseline: Some(0.5),
                candidate: Some(1.0),
                delta: Some(0.5),
            },
            thresholds: request.thresholds,
            evaluated_by: ProvenanceAuthor::Distiller {
                name: "test-evaluator".into(),
            },
            evaluated_at: Utc::now(),
        })
    }
}

/// The two graphs these tests drive: `pipeline` (two nodes, completes —
/// journaled-evidence tests need completed runs) and `interrupt_gate`
/// (suspends until resumed — the resume-keeps-the-pin test).
fn registry() -> GraphRegistry {
    use rusty_agent_runtime::prelude::*;

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
    let pipeline = builder.compile().unwrap();

    let gate_spec = StateSpec::new().channel("answer", Reducer::Overwrite);
    let mut gate = GraphBuilder::new();
    gate.add_node("gate", |ctx: NodeContext| async move {
        match ctx.resume_value() {
            Some(v) => Ok(NodeOutput::update("answer", v.clone())),
            None => Err(ctx.interrupt(json!({"question": "approve?"}))),
        }
    });
    gate.set_entry_point("gate");
    let gate = gate.compile().unwrap();

    let mut registry = GraphRegistry::new();
    registry.register("pipeline", pipeline, spec);
    registry.register("interrupt_gate", gate, gate_spec);
    registry
}

/// An app over `store` with the scripted evaluator registered and the
/// config customized by `configure` (tenant keys, Postgres).
fn app_with(store: PathBuf, configure: impl FnOnce(ServerConfig) -> ServerConfig) -> Router {
    let config = configure(ServerConfig::new("127.0.0.1:0".parse().unwrap(), store))
        .with_candidate_evaluator(Arc::new(FixedEvaluator));
    router(registry(), config)
}

/// Open-mode (single `default` tenant) app over a fresh store.
fn app() -> (Router, PathBuf) {
    let store = temp_store();
    (app_with(store.clone(), |config| config), store)
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

// --------------------------------------------------------------------- //
// Policy + candidate fixtures (built through core, serialized for the wire)
// --------------------------------------------------------------------- //

/// The static floor with one retry parameter changed — a legal learned
/// body shape (the full `RetryPolicyParameters` is required: the family
/// overlay parses the complete parameter set).
fn policy_with_max_attempts(max_attempts: u32) -> ExecutorPolicy {
    ExecutorPolicy::static_v0()
        .with_family_parameters(
            DecisionFamily::Retry,
            json!({
                "base_delay_ms": 1_000,
                "max_delay_ms": 300_000,
                "max_attempts": max_attempts,
            }),
        )
        .unwrap()
}

/// Register `policy` (derived version); asserts 201 and returns the version.
async fn register(app: &Router, policy: &ExecutorPolicy) -> String {
    let (status, v) = call(
        app,
        "POST",
        "/policy/versions",
        Some(json!({"policy": serde_json::to_value(policy).unwrap()})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "register failed: {v}");
    v["version"].as_str().unwrap().to_string()
}

/// Activate `version`; asserts 200.
async fn activate(app: &Router, version: &str) {
    let (status, v) = call(
        app,
        "POST",
        "/policy/activations",
        Some(json!({"version": version})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "activate failed: {v}");
}

/// Create a thread bound to `graph`; returns the thread id.
async fn create_thread(app: &Router, graph: &str) -> String {
    let (status, v) = call(app, "POST", "/threads", Some(json!({"graph": graph}))).await;
    assert_eq!(status, StatusCode::CREATED, "thread failed: {v}");
    v["thread_id"].as_str().unwrap().to_string()
}

/// Run the thread to a terminal state; returns the run body.
async fn run_wait(app: &Router, thread_id: &str, body: Value) -> Value {
    let (status, v) = call(
        app,
        "POST",
        &format!("/threads/{thread_id}/runs/wait"),
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "run failed: {v}");
    v
}

/// The thread's latest checkpoint ref (`GET /threads/{id}/state`).
async fn checkpoint_of(app: &Router, thread_id: &str) -> Value {
    let (status, v) = call(app, "GET", &format!("/threads/{thread_id}/state"), None).await;
    assert_eq!(status, StatusCode::OK, "state failed: {v}");
    v["checkpoint"].clone()
}

/// Create a thread, run `pipeline` to completion; returns the run id.
async fn run_pipeline(app: &Router) -> String {
    let thread_id = create_thread(app, "pipeline").await;
    let run = run_wait(app, &thread_id, json!({})).await;
    assert_eq!(run["status"], json!("success"), "pipeline failed: {run}");
    run["run_id"].as_str().unwrap().to_string()
}

/// The run's journaled events (Flight Recorder).
async fn events_of(app: &Router, run_id: &str) -> Vec<Value> {
    let (status, v) = call(app, "GET", &format!("/runs/{run_id}/events"), None).await;
    assert_eq!(status, StatusCode::OK, "events failed: {v}");
    v["events"].as_array().unwrap().clone()
}

/// Enqueue a task; returns its task id.
async fn enqueue(app: &Router, extra: Value) -> String {
    let mut body = json!({"kind": "call_tool", "payload": {"tool": "calc"}});
    body.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    let (status, v) = call(app, "POST", "/tasks", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "enqueue failed: {v}");
    v["task_id"].as_str().unwrap().to_string()
}

/// Claim one task as `worker`; asserts a handout and returns the task body.
async fn claim_one(app: &Router, worker: &str) -> Value {
    let (status, v) = call(
        app,
        "POST",
        "/tasks/claim",
        Some(json!({"worker_id": worker, "lease_ms": 30_000})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "claim failed: {v}");
    v["task"].clone()
}

/// Fail the leased task; asserts 200 and returns the settlement body.
async fn fail_task(
    app: &Router,
    task_id: &str,
    worker: &str,
    class: &str,
    retryable: bool,
) -> Value {
    let (status, v) = call(
        app,
        "POST",
        &format!("/tasks/{task_id}/fail"),
        Some(json!({"worker_id": worker, "error_class": class,
                    "message": "attempt failed", "retryable": retryable})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "fail failed: {v}");
    v
}

/// A `policy` candidate over the retry family (full parameters — the
/// promotion overlay parses the complete set), auto-evaluable by the
/// scripted evaluator, approval-ruled under the default envelope.
fn policy_candidate(max_attempts: u32) -> Candidate {
    Candidate::new(
        CandidateContent::Policy {
            family: DecisionFamily::Retry,
            parameters: json!({
                "base_delay_ms": 1_000,
                "max_delay_ms": 300_000,
                "max_attempts": max_attempts,
            }),
        },
        ProvenanceAuthor::Distiller {
            name: "correction-loop".into(),
        },
        EvidenceSpan::default(),
        ts(1_750_000_002_000),
    )
    .unwrap()
}

/// Create + evaluate `candidate` against a completed run; returns the id.
async fn create_and_evaluate(app: &Router, run_id: &str, candidate: &Candidate) -> String {
    let (status, v) = call(
        app,
        "POST",
        "/learn/candidates",
        Some(json!({
            "candidate": serde_json::to_value(candidate).unwrap(),
            "run_id": run_id,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create failed: {v}");
    let candidate_id = v["candidate_id"].as_str().unwrap().to_string();
    let (status, v) = call(
        app,
        "POST",
        &format!("/learn/candidates/{candidate_id}/evaluate"),
        Some(json!({
            "request": {
                "dataset_version": "support-v3",
                "target_metric": "run_pass_rate",
                "thresholds": {"max_pass_rate_drop": 0.05, "max_latency_p95_ratio": 1.25},
                "replay_evidence": [],
            },
            "run_id": run_id,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "evaluate failed: {v}");
    assert_eq!(v["status"], json!("evaluated"));
    candidate_id
}

// --------------------------------------------------------------------- //
// The registry
// --------------------------------------------------------------------- //

#[tokio::test]
async fn registry_crud_converges_and_refuses_overwrites() {
    let (app, store) = app();
    let body = policy_with_max_attempts(5);

    // Create without a version: the content-derived `policy-{hash12}` name.
    let (status, v) = call(
        &app,
        "POST",
        "/policy/versions",
        Some(json!({"policy": serde_json::to_value(body).unwrap()})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create failed: {v}");
    let version = v["version"].as_str().unwrap().to_string();
    assert!(
        version.starts_with("policy-"),
        "unnamed registration mints the derived version: {version}"
    );
    assert_eq!(v["record"]["source"], json!({"source": "api"}));
    assert_eq!(v["record"]["policy"]["retry"]["max_attempts"], json!(5));

    // The idempotent create: same body re-registered converges (200), and
    // an explicit registration under the derived name converges too.
    let (status, _) = call(
        &app,
        "POST",
        "/policy/versions",
        Some(json!({"policy": serde_json::to_value(body).unwrap()})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "converged re-registration");
    let (status, _) = call(
        &app,
        "POST",
        "/policy/versions",
        Some(json!({"version": version, "policy": serde_json::to_value(body).unwrap()})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "explicit converged re-registration");

    // Immutability: the same version naming a different body conflicts.
    let other = policy_with_max_attempts(7);
    let (status, v) = call(
        &app,
        "POST",
        "/policy/versions",
        Some(json!({"version": version, "policy": serde_json::to_value(other).unwrap()})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "overwrite must refuse: {v}");

    // An operator-chosen name registers; invalid names and the reserved
    // floor are rejected.
    let (status, _) = call(
        &app,
        "POST",
        "/policy/versions",
        Some(json!({"version": "ops-tuned-1", "policy": serde_json::to_value(other).unwrap()})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "named registration");
    let (status, _) = call(
        &app,
        "POST",
        "/policy/versions",
        Some(json!({"version": "bad/name", "policy": serde_json::to_value(other).unwrap()})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "path-unsafe name");
    let (status, _) = call(
        &app,
        "POST",
        "/policy/versions",
        Some(json!({"version": PolicyVersion::STATIC_V0,
                    "policy": serde_json::to_value(ExecutorPolicy::static_v0()).unwrap()})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "the floor is reserved");

    // The listing is sorted by version and never lists the floor.
    let (status, v) = call(&app, "GET", "/policy/versions", None).await;
    assert_eq!(status, StatusCode::OK);
    let policies = v["policies"].as_array().unwrap();
    assert_eq!(policies.len(), 2, "floor is synthesized, not listed: {v}");
    assert_eq!(policies[0]["version"], json!("ops-tuned-1"));
    assert_eq!(policies[1]["version"], json!(version));

    // Fetch one: registered → 200; unknown → 404; the floor → the
    // synthetic record.
    let (status, v) = call(&app, "GET", &format!("/policy/versions/{version}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["record"]["policy"]["retry"]["max_attempts"], json!(5));
    let (status, _) = call(&app, "GET", "/policy/versions/policy-000000000000", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "unknown version");
    let (status, v) = call(&app, "GET", "/policy/versions/static-v0", None).await;
    assert_eq!(status, StatusCode::OK, "floor resolves: {v}");
    assert_eq!(v["record"]["version"], json!("static-v0"));
    assert_eq!(
        v["record"]["policy"],
        serde_json::to_value(ExecutorPolicy::static_v0()).unwrap()
    );

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn activations_move_the_pointer_and_epochs_fold_the_history() {
    let (app, store) = app();

    // The registry never moved: the active policy is the floor.
    let (status, v) = call(&app, "GET", "/policy/active", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["version"], json!("static-v0"));
    let (status, v) = call(&app, "GET", "/policy/epochs", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["epochs"], json!([]), "never moved: nothing to list");

    // Only registered bodies (and the floor) activate.
    let (status, v) = call(
        &app,
        "POST",
        "/policy/activations",
        Some(json!({"version": "policy-000000000000"})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "unregistered: {v}"
    );

    let a = register(&app, &policy_with_max_attempts(5)).await;
    let b = register(&app, &policy_with_max_attempts(7)).await;

    activate(&app, &a).await;
    let (status, v) = call(&app, "GET", "/policy/active", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["version"], json!(a));
    assert_eq!(v["record"]["policy"]["retry"]["max_attempts"], json!(5));

    activate(&app, &b).await;
    let (status, v) = call(&app, "GET", "/policy/epochs", None).await;
    assert_eq!(status, StatusCode::OK);
    let epochs = v["epochs"].as_array().unwrap();
    assert_eq!(epochs.len(), 2, "two activations, no bindings yet: {v}");
    assert_eq!(epochs[0]["version"], json!(a));
    assert!(
        epochs[0]["retired_at"].is_string(),
        "B's activation retired A"
    );
    assert_eq!(epochs[1]["version"], json!(b));
    assert!(epochs[1]["retired_at"].is_null(), "B still serves");

    // Reverting to the floor is always legal — no candidate needed.
    activate(&app, PolicyVersion::STATIC_V0).await;
    let (status, v) = call(&app, "GET", "/policy/active", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["version"], json!("static-v0"));
    let (status, v) = call(&app, "GET", "/policy/epochs", None).await;
    assert_eq!(status, StatusCode::OK);
    let epochs = v["epochs"].as_array().unwrap();
    assert_eq!(epochs.len(), 3);
    assert_eq!(epochs[2]["version"], json!("static-v0"));

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Admission binding
// --------------------------------------------------------------------- //

#[tokio::test]
async fn admission_binds_the_active_version_and_resume_keeps_the_pin() {
    let (app, store) = app();

    // Fresh runs before the registry moves bind the floor.
    let early = create_thread(&app, "pipeline").await;
    let run = run_wait(&app, &early, json!({})).await;
    assert_eq!(run["status"], json!("success"));
    assert_eq!(
        checkpoint_of(&app, &early).await["policy_version"],
        json!("static-v0"),
        "an unmoved registry binds the floor"
    );

    // Activate A; thread 1 suspends on the gate with A pinned.
    let a = register(&app, &policy_with_max_attempts(5)).await;
    activate(&app, &a).await;
    let first = create_thread(&app, "interrupt_gate").await;
    let run = run_wait(&app, &first, json!({})).await;
    assert_eq!(run["status"], json!("interrupted"));
    let suspended = checkpoint_of(&app, &first).await;
    assert_eq!(
        suspended["policy_version"],
        json!(a),
        "admission stamped the active version into the header"
    );

    // Activate B; thread 2 binds B — the move does not reach back.
    let b = register(&app, &policy_with_max_attempts(7)).await;
    activate(&app, &b).await;
    let second = create_thread(&app, "pipeline").await;
    let run = run_wait(&app, &second, json!({})).await;
    assert_eq!(run["status"], json!("success"));
    assert_eq!(
        checkpoint_of(&app, &second).await["policy_version"],
        json!(b)
    );

    // Resume thread 1 under B's reign: the pin survives — the whole point
    // of the header stamp (a mid-run policy move never changes an in-flight
    // run's semantics).
    let run = run_wait(
        &app,
        &first,
        json!({"command": {"resume": {"approved": true}}}),
    )
    .await;
    assert_eq!(run["status"], json!("success"), "resume failed: {run}");
    assert_eq!(
        checkpoint_of(&app, &first).await["policy_version"],
        json!(a),
        "a resumed run keeps the version it bound at admission"
    );

    // The epoch history carries the bindings: the pre-activation run under
    // the implicit floor epoch, then one admission per activation window.
    let (status, v) = call(&app, "GET", "/policy/epochs", None).await;
    assert_eq!(status, StatusCode::OK);
    let epochs = v["epochs"].as_array().unwrap();
    assert_eq!(epochs.len(), 3, "floor epoch + A's reign + B's reign: {v}");
    assert_eq!(epochs[0]["version"], json!("static-v0"));
    assert_eq!(epochs[0]["bindings"].as_array().unwrap().len(), 1);
    assert_eq!(epochs[1]["version"], json!(a));
    assert_eq!(epochs[1]["bindings"].as_array().unwrap().len(), 1);
    assert_eq!(
        epochs[1]["bindings"][0]["checkpoint_id"], suspended["checkpoint_id"],
        "thread 1's admission binding names its first checkpoint"
    );
    assert_eq!(epochs[2]["version"], json!(b));
    assert_eq!(epochs[2]["bindings"].as_array().unwrap().len(), 1);

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Decision evidence
// --------------------------------------------------------------------- //

#[tokio::test]
async fn failed_tasks_journal_the_policy_decision() {
    let (app, store) = app();
    let run_id = run_pipeline(&app).await;

    // A retryable failure with attempts left: the decision is `retry`, the
    // legal set is [retry, abort], and the event carries the features the
    // classifier decided from.
    let task_id = enqueue(
        &app,
        json!({"run_id": run_id, "effect": "idempotent", "max_attempts": 3}),
    )
    .await;
    claim_one(&app, "w1").await;
    let settled = fail_task(&app, &task_id, "w1", "timeout", true).await;
    assert_eq!(settled["requeued"], json!(true));

    let decisions: Vec<Value> = events_of(&app, &run_id)
        .await
        .into_iter()
        .filter(|event| event["kind"] == json!("policy_decision"))
        .collect();
    assert_eq!(decisions.len(), 1, "one failure, one decision");
    assert_eq!(decisions[0]["effect"], json!("pure"));
    // Payloads carry the adjacent-tagged payload-ref envelope; the
    // DecisionEvent is the inline value.
    let out = &decisions[0]["output"]["value"];
    assert_eq!(out["id"], json!(format!("{run_id}:d0")));
    assert_eq!(out["seq"], json!(0));
    assert_eq!(out["family"], json!("retry"));
    assert_eq!(
        out["legal_actions"],
        json!([{"action": "retry", "attempt": 2}, {"action": "abort"}])
    );
    assert_eq!(out["selected"], json!({"action": "retry", "attempt": 2}));
    assert_eq!(out["propensity"], json!(1.0));
    assert_eq!(out["policy_version"], json!("static-v0"));
    assert_eq!(out["features"]["failure_class"], json!("timeout"));
    assert_eq!(out["features"]["attempt"], json!(1));
    assert_eq!(out["features"]["max_attempts"], json!(3));
    assert_eq!(out["features"]["effect"], json!("idempotent"));
    assert!(
        out.get("outcome").is_none() || out["outcome"].is_null(),
        "a scheduled retry has no outcome yet"
    );

    // Attempt 2 fails non-retryably: `abort` with the failure outcome, the
    // next sequence number.
    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
    claim_one(&app, "w2").await;
    let settled = fail_task(&app, &task_id, "w2", "invalid_input", false).await;
    assert_eq!(settled["requeued"], json!(false));
    assert_eq!(settled["dead"], json!(false));

    let decisions: Vec<Value> = events_of(&app, &run_id)
        .await
        .into_iter()
        .filter(|event| event["kind"] == json!("policy_decision"))
        .collect();
    assert_eq!(decisions.len(), 2);
    let out = &decisions[1]["output"]["value"];
    assert_eq!(out["id"], json!(format!("{run_id}:d1")));
    assert_eq!(out["legal_actions"], json!([{"action": "abort"}]));
    assert_eq!(out["selected"], json!({"action": "abort"}));
    assert_eq!(out["outcome"], json!("failure"));

    // Cancellation is control flow, not a policy decision: it journals
    // nothing.
    let task_id = enqueue(&app, json!({"run_id": run_id})).await;
    claim_one(&app, "w3").await;
    fail_task(&app, &task_id, "w3", "cancelled", false).await;
    let decisions: Vec<Value> = events_of(&app, &run_id)
        .await
        .into_iter()
        .filter(|event| event["kind"] == json!("policy_decision"))
        .collect();
    assert_eq!(decisions.len(), 2, "cancelled attempts journal nothing");

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// The promotion / rollback hooks
// --------------------------------------------------------------------- //

#[tokio::test]
async fn policy_promotion_activates_the_learned_version_and_rollback_reverts() {
    let (app, store) = app();
    let run_id = run_pipeline(&app).await;
    let candidate = policy_candidate(5);
    let candidate_id = create_and_evaluate(&app, &run_id, &candidate).await;

    // Policy promotions are approval-ruled under the default envelope:
    // refused without a token scoped to this candidate's promotion effect.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/learn/candidates/{candidate_id}/promote"),
        Some(json!({"run_id": run_id})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "expected 403: {v}");
    let (status, v) = call(&app, "GET", "/policy/active", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        v["version"],
        json!("static-v0"),
        "a refused promotion touches nothing"
    );

    // The scoped token admits; the hook overlays the candidate's family
    // parameters onto the active (floor) policy, registers the derived
    // version with candidate provenance, and activates it.
    let token = ApprovalToken::approve(promotion_effect_id(&candidate), "ops:amjad");
    let (status, v) = call(
        &app,
        "POST",
        &format!("/learn/candidates/{candidate_id}/promote"),
        Some(json!({
            "run_id": run_id,
            "approval": serde_json::to_value(&token).unwrap(),
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "approved promote failed: {v}");

    let expected = policy_with_max_attempts(5);
    let expected_version = derive_policy_version(&expected).unwrap();
    let (status, v) = call(&app, "GET", "/policy/active", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["version"], json!(expected_version.as_str()));
    assert_eq!(
        v["record"]["source"],
        json!({"source": "candidate", "candidate_id": candidate_id})
    );
    assert_eq!(
        v["record"]["policy"],
        serde_json::to_value(expected).unwrap(),
        "the overlay: the candidate's retry parameters, the floor's other families"
    );

    // New traffic binds the learned version at admission.
    let thread = create_thread(&app, "pipeline").await;
    let run = run_wait(&app, &thread, json!({})).await;
    assert_eq!(run["status"], json!("success"));
    assert_eq!(
        checkpoint_of(&app, &thread).await["policy_version"],
        json!(expected_version.as_str())
    );

    // Rollback reverts the registry to what the promotion displaced — the
    // floor — and new traffic binds the floor again.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/learn/candidates/{candidate_id}/rollback"),
        Some(json!({"run_id": run_id, "cause": "drift monitor: pass-rate drop"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "rollback failed: {v}");
    let (status, v) = call(&app, "GET", "/policy/active", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["version"], json!("static-v0"));

    let thread = create_thread(&app, "pipeline").await;
    let run = run_wait(&app, &thread, json!({})).await;
    assert_eq!(run["status"], json!("success"));
    assert_eq!(
        checkpoint_of(&app, &thread).await["policy_version"],
        json!("static-v0")
    );

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Isolation & durability
// --------------------------------------------------------------------- //

#[tokio::test]
async fn the_registry_is_tenant_scoped() {
    let store = temp_store();
    let app = app_with(store.clone(), |config| {
        config
            .with_tenant_key("acme", "acme-secret")
            .with_tenant_key("globex", "globex-secret")
    });
    let acme = Some(("x-api-key", "acme-secret"));
    let globex = Some(("x-api-key", "globex-secret"));

    let (status, v) = call_as(
        &app,
        acme,
        "POST",
        "/policy/versions",
        Some(json!({"policy": serde_json::to_value(policy_with_max_attempts(5)).unwrap()})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "acme register failed: {v}");
    let version = v["version"].as_str().unwrap().to_string();
    let (status, v) = call_as(
        &app,
        acme,
        "POST",
        "/policy/activations",
        Some(json!({"version": version})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "acme activate failed: {v}");

    // Another tenant sees none of it: fetch 404s, the listing is empty,
    // activation of the foreign version 422s, and its active policy is
    // still the floor.
    let (status, _) = call_as(
        &app,
        globex,
        "GET",
        &format!("/policy/versions/{version}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "cross-tenant fetch");
    let (status, v) = call_as(&app, globex, "GET", "/policy/versions", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["policies"], json!([]));
    let (status, _) = call_as(
        &app,
        globex,
        "POST",
        "/policy/activations",
        Some(json!({"version": version})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, v) = call_as(&app, globex, "GET", "/policy/active", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["version"], json!("static-v0"));

    // The owner is unaffected.
    let (status, v) = call_as(&app, acme, "GET", "/policy/active", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["version"], json!(version));

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn the_registry_survives_a_restart_on_the_file_backend() {
    let store = temp_store();
    let version = {
        let app = app_with(store.clone(), |config| config);
        let version = register(&app, &policy_with_max_attempts(5)).await;
        activate(&app, &version).await;
        let thread = create_thread(&app, "pipeline").await;
        run_wait(&app, &thread, json!({})).await;
        version
    };

    // A fresh app over the same store root serves the settled registry:
    // the active version, the epoch history with its admission binding.
    let app = app_with(store.clone(), |config| config);
    let (status, v) = call(&app, "GET", "/policy/active", None).await;
    assert_eq!(status, StatusCode::OK, "active after restart: {v}");
    assert_eq!(v["version"], json!(version));
    let (status, v) = call(&app, "GET", "/policy/epochs", None).await;
    assert_eq!(status, StatusCode::OK);
    let epochs = v["epochs"].as_array().unwrap();
    assert_eq!(epochs.len(), 1);
    assert_eq!(epochs[0]["version"], json!(version));
    assert_eq!(
        epochs[0]["bindings"].as_array().unwrap().len(),
        1,
        "the pre-restart admission binding survived"
    );

    // And admission keeps binding it post-restart.
    let thread = create_thread(&app, "pipeline").await;
    let run = run_wait(&app, &thread, json!({})).await;
    assert_eq!(run["status"], json!("success"));
    assert_eq!(
        checkpoint_of(&app, &thread).await["policy_version"],
        json!(version)
    );

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Postgres backend (live database required)
//
// Gated on `RUSTY_TEST_DATABASE_URL`; unset skips cleanly so the suite is
// green without a database. Every test uses a dedicated tenant, so
// repeated runs against one scratch database never interfere; the
// database itself is throwaway.
// --------------------------------------------------------------------- //

#[cfg(feature = "postgres")]
mod postgres {
    use super::*;

    fn pg_url() -> Option<String> {
        std::env::var("RUSTY_TEST_DATABASE_URL").ok()
    }

    /// The wave-4 registry on Postgres: register, activate, bind at
    /// admission — surviving a restart.
    #[tokio::test]
    async fn postgres_registry_and_bindings_survive_a_restart() {
        let Some(url) = pg_url() else {
            eprintln!("RUSTY_TEST_DATABASE_URL unset; skipping");
            return;
        };
        let tenant = format!("policypg-{}", uuid::Uuid::new_v4());
        let auth = Some(("x-api-key", "pg-secret"));
        let build = || {
            app_with(temp_store(), |config| {
                config
                    .with_postgres(url.clone())
                    .with_tenant_key(tenant.clone(), "pg-secret")
            })
        };

        let first = build();
        let (status, v) = call_as(
            &first,
            auth,
            "POST",
            "/policy/versions",
            Some(json!({"policy": serde_json::to_value(policy_with_max_attempts(5)).unwrap()})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "pg register failed: {v}");
        let version = v["version"].as_str().unwrap().to_string();
        let (status, v) = call_as(
            &first,
            auth,
            "POST",
            "/policy/activations",
            Some(json!({"version": version})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "pg activate failed: {v}");

        let (status, v) = call_as(
            &first,
            auth,
            "POST",
            "/threads",
            Some(json!({"graph": "pipeline"})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "pg thread failed: {v}");
        let thread_id = v["thread_id"].as_str().unwrap().to_string();
        let (status, v) = call_as(
            &first,
            auth,
            "POST",
            &format!("/threads/{thread_id}/runs/wait"),
            Some(json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "pg run failed: {v}");
        let (status, v) = call_as(
            &first,
            auth,
            "GET",
            &format!("/threads/{thread_id}/state"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "pg state failed: {v}");
        assert_eq!(
            v["checkpoint"]["policy_version"],
            json!(version),
            "pg admission bound the active version"
        );
        drop(first);

        // A fresh app over the same database serves the settled registry.
        let second = build();
        let (status, v) = call_as(&second, auth, "GET", "/policy/active", None).await;
        assert_eq!(status, StatusCode::OK, "pg active after restart: {v}");
        assert_eq!(v["version"], json!(version));
        let (status, v) = call_as(&second, auth, "GET", "/policy/epochs", None).await;
        assert_eq!(status, StatusCode::OK, "pg epochs after restart: {v}");
        let epochs = v["epochs"].as_array().unwrap();
        assert_eq!(epochs.len(), 1);
        assert_eq!(epochs[0]["bindings"].as_array().unwrap().len(), 1);
    }
}
