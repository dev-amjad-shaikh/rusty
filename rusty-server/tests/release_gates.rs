//! The release-gate integration tests (R0.12 Operations Plane, wave 4):
//! the environment's declared gate runs ahead of every gated pointer
//! move and journals its decision — allowed or refused; approval-required
//! environments demand a token scoped to the revision's own promotion
//! effect id; a canary binds a reproducible seeded subset of new runs and
//! every recorded run re-derives its assignment from its journaled
//! resolution alone; a shadow run replays a recorded run's twin behind
//! the shadow admission boundary, refusing and serving from the recorded
//! world; and the health board reports pointers, canaries, and gate
//! decisions derived from journaled data alone. The exit clauses live
//! here:
//!
//! - a failing gate refuses the promotion with the decision journaled,
//!   and an unavailable gate fails closed (`409`);
//! - a token minted for one revision admits no other;
//! - the canary draw re-binds from the journaled resolution: every run's
//!   recorded slot equals the seeded draw recomputed over its run id;
//! - a shadow run completes with its above-read-only effects refused
//!   (typed, classified, journaled), served from the recorded world, and
//!   its verdict naming the divergence in both directions;
//! - the health board reports both environments' pointers, canary state,
//!   and last gate decision without any new store.
//!
//! Driven in-process via `tower::ServiceExt::oneshot`, the
//! `deployments.rs` harness convention — the learn pipeline is scenery
//! (a scripted candidate evaluator); the gate evaluator is scripted
//! (allow or block); the release machinery is the proof.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use chrono::{DateTime, Utc};
use rusty_agent_runtime::deploy::{
    deployment_surface, revision_promotion_effect_id, DeploymentRevision, GateCheckRecord,
    GateDeclaration, GateEvaluation, GateVerdict, RevisionGateEvaluator,
};
use rusty_agent_runtime::effects::ApprovalToken;
use rusty_agent_runtime::error::Result as RuntimeResult;
use rusty_agent_runtime::executor::{Executor, RunConfig};
use rusty_agent_runtime::journal::{Clock, Journal};
use rusty_agent_runtime::learn::{
    canary_admits, promotion_effect_id, CanaryBinding, Candidate, CandidateContent,
    CandidateEvaluation, CandidateEvaluator, CandidateId, EnvironmentTag, EvaluationRequest,
    EvaluationVerdict, EvidenceSpan, ReplaySummary,
};
use rusty_agent_runtime::llm::{ChatMessage, ChatModel, ChatResponse, ToolCall};
use rusty_agent_runtime::memory::ProvenanceAuthor;
use rusty_agent_runtime::react::{
    create_react_agent, create_react_agent_with_recording, MESSAGES_CHANNEL,
};
use rusty_agent_runtime::record::Effect;
use rusty_agent_runtime::state::{Reducer, State, StateSpec};
use rusty_agent_runtime::tool::{Tool, ToolRegistry};
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

// --------------------------------------------------------------------- //
// Harness (the deployments.rs shapes, verbatim where the semantics match)
// --------------------------------------------------------------------- //

/// Unique temp store root, removed at the end of each test (best effort).
fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!(
        "rusty-server-release-gates-test-{}",
        uuid::Uuid::new_v4()
    ))
}

/// A fixed timestamp, so candidates minted here are deterministic.
fn ts(millis: i64) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp_millis(millis).unwrap()
}

/// The scripted candidate evaluator (scenery — the learn pipeline feeds
/// revisions their pins).
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

/// The scripted gate evaluator: names the declaration back (the seam
/// contract) and answers allow or block, so the test owns the verdict
/// and the machinery under test is everything around it.
#[derive(Debug)]
struct FixedGateEvaluator {
    allow: bool,
}

#[async_trait::async_trait]
impl RevisionGateEvaluator for FixedGateEvaluator {
    async fn evaluate(
        &self,
        _revision: &DeploymentRevision,
        _baseline: Option<&DeploymentRevision>,
        gate: &GateDeclaration,
    ) -> RuntimeResult<GateEvaluation> {
        Ok(GateEvaluation {
            policy: gate.policy.clone(),
            dataset_version: gate.dataset_version.clone(),
            outcome: if self.allow {
                GateVerdict::Allow
            } else {
                GateVerdict::Block
            },
            checks: vec![GateCheckRecord {
                metric: "\"minimum_run_pass_rate\"".into(),
                passed: self.allow,
                observed: json!(0.91),
                required: json!({"minimum": 0.85}),
                detail: if self.allow {
                    "aggregate run pass rate 0.91 clears the 0.85 floor".into()
                } else {
                    "aggregate run pass rate 0.91 below the 0.95 floor".into()
                },
            }],
        })
    }
}

/// The pipeline graph (`first -> second`, appending to a `log` channel).
fn pipeline_registry() -> GraphRegistry {
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
    let mut registry = GraphRegistry::new();
    registry.register("pipeline", builder.compile().unwrap(), spec);
    registry
}

/// An app over `store` with the scripted candidate evaluator and,
/// optionally, a scripted gate evaluator.
fn app_with(store: PathBuf, gate: Option<Arc<dyn RevisionGateEvaluator>>) -> Router {
    let mut config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store)
        .with_candidate_evaluator(Arc::new(FixedEvaluator));
    if let Some(gate) = gate {
        config = config.with_revision_gate_evaluator(gate);
    }
    router(pipeline_registry(), config)
}

/// Open-mode app with an allowing gate evaluator.
fn app() -> (Router, PathBuf) {
    let store = temp_store();
    (
        app_with(
            store.clone(),
            Some(Arc::new(FixedGateEvaluator { allow: true })),
        ),
        store,
    )
}

/// Send a request; returns `(status, json-body-or-null)`.
async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(v) = body {
        builder = builder.header("content-type", "application/json");
        let response = app
            .clone()
            .oneshot(builder.body(Body::from(v.to_string())).unwrap())
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
    } else {
        let response = app
            .clone()
            .oneshot(builder.body(Body::empty()).unwrap())
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
}

/// Create a thread and run it to completion; returns the run id.
async fn run_pipeline(app: &Router) -> String {
    let (status, v) = call(app, "POST", "/threads", Some(json!({"graph": "pipeline"}))).await;
    assert_eq!(status, StatusCode::CREATED, "thread failed: {v}");
    let thread_id = v["thread_id"].as_str().unwrap().to_string();
    let (status, v) = call(
        app,
        "POST",
        &format!("/threads/{thread_id}/runs/wait"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "run failed: {v}");
    v["run_id"].as_str().unwrap().to_string()
}

/// Create a thread and run it to completion under a deployment binding;
/// asserts admission and completion, returns the run id.
async fn run_pipeline_bound(app: &Router, environment: &str) -> String {
    let (status, v) = call(app, "POST", "/threads", Some(json!({"graph": "pipeline"}))).await;
    assert_eq!(status, StatusCode::CREATED, "thread failed: {v}");
    let thread_id = v["thread_id"].as_str().unwrap().to_string();
    let (status, v) = call(
        app,
        "POST",
        &format!("/threads/{thread_id}/runs/wait"),
        Some(json!({"deployment": {"environment": environment}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "bound run failed: {v}");
    v["run_id"].as_str().unwrap().to_string()
}

/// Export the run's journal snapshot (via the portable fixture).
async fn snapshot_of(app: &Router, run_id: &str) -> Value {
    let (status, v) = call(app, "GET", &format!("/runs/{run_id}/fixture"), None).await;
    assert_eq!(status, StatusCode::OK, "fixture failed: {v}");
    v["journal"].clone()
}

/// The journaled `deployment_resolved` outputs, in journal order.
fn deployment_resolutions(snapshot: &Value) -> Vec<Value> {
    snapshot["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["kind"] == json!("deployment_resolved"))
        .map(|event| event["output"]["value"].clone())
        .collect()
}

/// The deployment evidence chain's events, in journal order.
async fn chain_events(app: &Router) -> Vec<Value> {
    let (status, v) = call(app, "GET", "/deployments/journal", None).await;
    assert_eq!(status, StatusCode::OK, "journal failed: {v}");
    v["events"].as_array().unwrap().clone()
}

/// The event kinds of a chain segment, in order.
fn kinds(events: &[Value]) -> Vec<&str> {
    events
        .iter()
        .map(|event| event["kind"].as_str().unwrap())
        .collect()
}

/// A 64-lowercase-hex string assertion helper.
fn is_hex64(value: &Value) -> bool {
    value
        .as_str()
        .map(|s| {
            s.len() == 64
                && s.bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        })
        .unwrap_or(false)
}

fn owner() -> ProvenanceAuthor {
    ProvenanceAuthor::Human {
        human_id: "amjad".into(),
    }
}

/// The author JSON every control-plane payload carries.
fn author() -> Value {
    serde_json::to_value(owner()).unwrap()
}

/// A distiller-authored `prompt` candidate (`prompt:{name}`).
fn prompt_candidate(name: &str, text: &str, millis: i64) -> Candidate {
    Candidate::new(
        CandidateContent::Prompt {
            name: name.into(),
            prompt: text.into(),
        },
        ProvenanceAuthor::Distiller {
            name: "correction-loop".into(),
        },
        EvidenceSpan::default(),
        ts(millis),
    )
    .unwrap()
}

/// The evaluate payload every test shares (the scripted evaluator
/// clears it).
fn evaluate_payload(run_id: &str) -> Value {
    json!({
        "request": {
            "dataset_version": "support-v3",
            "target_metric": "run_pass_rate",
            "thresholds": {"max_pass_rate_drop": 0.05, "max_latency_p95_ratio": 1.25},
            "replay_evidence": [],
        },
        "run_id": run_id,
    })
}

/// Create + evaluate `candidate` against a completed run; asserts both
/// and returns the candidate id.
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
        Some(evaluate_payload(run_id)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "evaluate failed: {v}");
    candidate_id
}

/// Declare the `prompt:system` artifact and commit each candidate id in
/// order.
async fn declare_and_commit(app: &Router, candidate_ids: &[String]) {
    let (status, v) = call(
        app,
        "POST",
        "/registry/artifacts",
        Some(json!({
            "family": "prompt",
            "name": "system",
            "owner": author(),
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "declare failed: {v}");
    for candidate_id in candidate_ids {
        let (status, v) = call(
            app,
            "POST",
            "/registry/artifacts/prompt/system/commits",
            Some(json!({"candidate_id": candidate_id})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "commit failed: {v}");
    }
}

/// Promote `candidate` to the environment tag through the learn route;
/// asserts 200.
async fn promote(app: &Router, run_id: &str, candidate: &Candidate, candidate_id: &str, tag: &str) {
    let (status, v) = call(
        app,
        "POST",
        &format!("/learn/candidates/{candidate_id}/promote"),
        Some(json!({
            "run_id": run_id,
            "approval": serde_json::to_value(ApprovalToken::approve(
                promotion_effect_id(candidate),
                "ops:amjad",
            ))
            .unwrap(),
            "tag": tag,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "learn promote failed: {v}");
}

/// Declare an environment; asserts 201. Gate and approval rule optional.
async fn declare_env_full(app: &Router, name: &str, gate: Option<Value>, approval_required: bool) {
    let (status, v) = call(
        app,
        "POST",
        "/deployments/environments",
        Some(json!({
            "name": name,
            "gate": gate,
            "approval_required": approval_required,
            "author": author(),
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "declare {name} failed: {v}");
}

/// Declare a plain environment; asserts 201.
async fn declare_env(app: &Router, name: &str) {
    declare_env_full(app, name, None, false).await;
}

/// The gate declaration the gated tests share.
fn gate_declaration() -> Value {
    json!({"policy": "r0.12-default", "dataset_version": "support-v3"})
}

/// Create a revision freezing `surfaces` from `source_environment`;
/// asserts 201 and returns the revision id.
async fn create_revision(app: &Router, source_environment: &str, surfaces: &[&str]) -> String {
    let (status, v) = call(
        app,
        "POST",
        "/deployments/revisions",
        Some(json!({
            "graph": "pipeline",
            "source_environment": source_environment,
            "surfaces": surfaces,
            "author": author(),
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create failed: {v}");
    v["revision"]["revision_id"].as_str().unwrap().to_string()
}

/// Read one revision; asserts 200 and returns the body.
async fn get_revision(app: &Router, revision_id: &str) -> Value {
    let (status, v) = call(
        app,
        "GET",
        &format!("/deployments/revisions/{revision_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "get revision failed: {v}");
    v
}

/// Promote a revision into an environment; returns `(status, body)`.
async fn deploy_promote_as(
    app: &Router,
    environment: &str,
    revision_id: &str,
    approval: Option<Value>,
) -> (StatusCode, Value) {
    call(
        app,
        "POST",
        &format!("/deployments/environments/{environment}/promote"),
        Some(json!({
            "revision_id": revision_id,
            "author": author(),
            "approval": approval,
        })),
    )
    .await
}

/// Recompute the seeded canary draw for `run_id` against the canary
/// binding — the audit's replay of the admission decision, from the
/// journaled resolution's own fields.
fn recomputed_admits(environment: &str, revision_id: &str, fraction: f64, run_id: &str) -> bool {
    let binding = CanaryBinding {
        candidate_id: CandidateId::from(revision_id.to_owned()),
        fraction,
    };
    let surface = deployment_surface(&EnvironmentTag::new(environment).unwrap());
    canary_admits(&binding, &surface, run_id)
}

// --------------------------------------------------------------------- //
// Exit clause: a failing gate refuses the promotion, journaled
// --------------------------------------------------------------------- //

#[tokio::test]
async fn a_failing_gate_refuses_the_promotion_and_journals_the_decision() {
    let store = temp_store();
    let app = app_with(
        store.clone(),
        Some(Arc::new(FixedGateEvaluator { allow: false })),
    );
    declare_env(&app, "dev").await;
    declare_env_full(&app, "prod", Some(gate_declaration()), false).await;

    // A revision freezes dev's active pin; prod's gate blocks it.
    let journal_run = run_pipeline(&app).await;
    let v1 = prompt_candidate("system", "You are terse.", 1_750_000_002_000);
    let id1 = create_and_evaluate(&app, &journal_run, &v1).await;
    declare_and_commit(&app, std::slice::from_ref(&id1)).await;
    promote(&app, &journal_run, &v1, &id1, "dev").await;
    let r1 = create_revision(&app, "dev", &["prompt:system"]).await;

    let (status, v) = deploy_promote_as(&app, "prod", &r1, None).await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a blocking gate refuses: {v}"
    );
    let message = v["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("r0.12-default") && message.contains(&r1),
        "the refusal names the gate and the revision: {v}"
    );

    // The refusal journaled: one gate_decision_recorded, verdict block,
    // naming the environment, the revision, and the failing check — and
    // the pointer never moved (nothing was ever promoted into prod).
    let events = chain_events(&app).await;
    let decisions: Vec<&Value> = events
        .iter()
        .filter(|event| event["kind"] == "gate_decision_recorded")
        .collect();
    assert_eq!(decisions.len(), 1, "the refusal journaled exactly once");
    let record = &decisions[0]["output"]["value"];
    assert_eq!(record["environment"], json!("prod"));
    assert_eq!(record["revision_id"], json!(r1));
    assert_eq!(record["outcome"], json!("block"));
    assert_eq!(record["policy"], json!("r0.12-default"));
    assert_eq!(record["dataset_version"], json!("support-v3"));
    assert_eq!(record["checks"].as_array().unwrap().len(), 1);
    assert_eq!(record["checks"][0]["passed"], json!(false));
    assert!(
        !kinds(&events).contains(&"revision_promoted"),
        "a refused promotion never journals the move: {:?}",
        kinds(&events)
    );
    let (status, _) = call(&app, "GET", "/deployments/environments/prod/pointer", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "the pointer never moved");

    // The unavailable gate fails closed the same way: a gated
    // environment with no evaluator configured refuses at 409 — a gate
    // that cannot run is a gate that did not pass.
    let bare_store = temp_store();
    let bare = app_with(bare_store.clone(), None);
    declare_env(&bare, "dev").await;
    declare_env_full(&bare, "prod", Some(gate_declaration()), false).await;
    let bare_run = run_pipeline(&bare).await;
    let bare_candidate = prompt_candidate("system", "You are terse.", 1_750_000_002_000);
    let bare_id = create_and_evaluate(&bare, &bare_run, &bare_candidate).await;
    declare_and_commit(&bare, std::slice::from_ref(&bare_id)).await;
    promote(&bare, &bare_run, &bare_candidate, &bare_id, "dev").await;
    let bare_revision = create_revision(&bare, "dev", &["prompt:system"]).await;
    let (status, v) = deploy_promote_as(&bare, "prod", &bare_revision, None).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "an unavailable gate fails closed: {v}"
    );

    let _ = std::fs::remove_dir_all(store);
    let _ = std::fs::remove_dir_all(bare_store);
}

// --------------------------------------------------------------------- //
// Exit clause: an approval token admits exactly one revision
// --------------------------------------------------------------------- //

#[tokio::test]
async fn an_approval_token_admits_exactly_the_revision_it_was_minted_for() {
    let (app, store) = app();
    declare_env(&app, "dev").await;
    declare_env_full(&app, "prod", Some(gate_declaration()), true).await;

    let journal_run = run_pipeline(&app).await;
    let v1 = prompt_candidate("system", "You are terse.", 1_750_000_002_000);
    let id1 = create_and_evaluate(&app, &journal_run, &v1).await;
    declare_and_commit(&app, std::slice::from_ref(&id1)).await;
    promote(&app, &journal_run, &v1, &id1, "dev").await;
    let r1 = create_revision(&app, "dev", &["prompt:system"]).await;
    let v2 = prompt_candidate("system", "You are expansive.", 1_750_000_003_000);
    let id2 = create_and_evaluate(&app, &journal_run, &v2).await;
    let (status, v) = call(
        &app,
        "POST",
        "/registry/artifacts/prompt/system/commits",
        Some(json!({"candidate_id": id2})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "commit v2 failed: {v}");
    promote(&app, &journal_run, &v2, &id2, "dev").await;
    let r2 = create_revision(&app, "dev", &["prompt:system"]).await;
    assert_ne!(r1, r2, "a changed pin set is a new revision");

    // No token: 403. The gate ran first — its allowed decision journaled
    // — but the pointer never moved.
    let (status, v) = deploy_promote_as(&app, "prod", &r1, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "no token refuses: {v}");
    // A token minted for the OTHER revision: 403 — scoped, not
    // transferable.
    let prod = EnvironmentTag::new("prod").unwrap();
    let r2_revision: DeploymentRevision =
        serde_json::from_value(get_revision(&app, &r2).await["revision"].clone()).unwrap();
    let wrong = serde_json::to_value(ApprovalToken::approve(
        revision_promotion_effect_id(&prod, &r2_revision),
        "ops:amjad",
    ))
    .unwrap();
    let (status, v) = deploy_promote_as(&app, "prod", &r1, Some(wrong)).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a token for another revision admits nothing here: {v}"
    );
    let (status, _) = call(&app, "GET", "/deployments/environments/prod/pointer", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "the pointer never moved");

    // The token scoped to r1's promotion effect id: 201, pointer moved.
    let r1_revision: DeploymentRevision =
        serde_json::from_value(get_revision(&app, &r1).await["revision"].clone()).unwrap();
    let right = serde_json::to_value(ApprovalToken::approve(
        revision_promotion_effect_id(&prod, &r1_revision),
        "ops:amjad",
    ))
    .unwrap();
    let (status, v) = deploy_promote_as(&app, "prod", &r1, Some(right)).await;
    assert_eq!(status, StatusCode::CREATED, "the scoped token admits: {v}");
    assert_eq!(v["pointer"]["active"], json!(r1));

    // Three gate decisions journaled — the gate runs ahead of the
    // approval check on every attempt (no token, wrong token, right
    // token), and each journals its allowed decision; the promotion
    // journaled once.
    let events = chain_events(&app).await;
    let gate_decisions = events
        .iter()
        .filter(|event| event["kind"] == "gate_decision_recorded")
        .count();
    assert_eq!(
        gate_decisions, 3,
        "each gated attempt journals its decision"
    );
    let promotions: Vec<&Value> = events
        .iter()
        .filter(|event| event["kind"] == "revision_promoted")
        .collect();
    assert_eq!(promotions.len(), 1);
    assert_eq!(promotions[0]["output"]["value"]["revision_id"], json!(r1));

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Exit clause: the canary draw re-binds from the journaled resolution
// --------------------------------------------------------------------- //

#[tokio::test]
async fn the_canary_binds_a_seeded_subset_and_every_run_re_derives_its_assignment() {
    let (app, store) = app();
    declare_env(&app, "dev").await;
    declare_env(&app, "staging").await;

    // r1 serves staging; r2 canaries at 10%.
    let journal_run = run_pipeline(&app).await;
    let v1 = prompt_candidate("system", "You are terse.", 1_750_000_002_000);
    let id1 = create_and_evaluate(&app, &journal_run, &v1).await;
    declare_and_commit(&app, std::slice::from_ref(&id1)).await;
    promote(&app, &journal_run, &v1, &id1, "staging").await;
    let r1 = create_revision(&app, "staging", &["prompt:system"]).await;
    let (status, v) = deploy_promote_as(&app, "staging", &r1, None).await;
    assert_eq!(status, StatusCode::CREATED, "promote r1 failed: {v}");
    let v2 = prompt_candidate("system", "You are expansive.", 1_750_000_003_000);
    let id2 = create_and_evaluate(&app, &journal_run, &v2).await;
    let (status, v) = call(
        &app,
        "POST",
        "/registry/artifacts/prompt/system/commits",
        Some(json!({"candidate_id": id2})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "commit v2 failed: {v}");
    promote(&app, &journal_run, &v2, &id2, "staging").await;
    let r2 = create_revision(&app, "staging", &["prompt:system"]).await;

    // Declare the canary: 201, the pointer binds r2 at 0.1 while r1
    // keeps serving. An identical re-declaration converges (200,
    // applied: false) — the binding already holds.
    let (status, v) = call(
        &app,
        "PUT",
        "/deployments/environments/staging/canary",
        Some(json!({"revision_id": r2, "fraction": 0.1, "author": author()})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "canary declare failed: {v}");
    assert_eq!(v["pointer"]["active"], json!(r1));
    assert_eq!(v["pointer"]["canary"]["revision_id"], json!(r2));
    let (status, v) = call(
        &app,
        "PUT",
        "/deployments/environments/staging/canary",
        Some(json!({"revision_id": r2, "fraction": 0.1, "author": author()})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "re-declare must converge: {v}");
    assert_eq!(v["applied"], json!(false));
    // An invalid fraction is a typed 422, not a clamp.
    let (status, v) = call(
        &app,
        "PUT",
        "/deployments/environments/staging/canary",
        Some(json!({"revision_id": r2, "fraction": 0.0, "author": author()})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "fraction 0: {v}");

    // The exit clause: forty bound runs, and every run's journaled
    // resolution slot equals the seeded draw recomputed over its run id
    // — the recorded run re-derives its assignment from the journaled
    // resolution alone.
    let mut canary_hits = 0usize;
    for _ in 0..40 {
        let run_id = run_pipeline_bound(&app, "staging").await;
        let snapshot = snapshot_of(&app, &run_id).await;
        let resolutions = deployment_resolutions(&snapshot);
        assert_eq!(resolutions.len(), 1, "one journaled resolution per run");
        let resolution = &resolutions[0];
        let expected = recomputed_admits("staging", &r2, 0.1, &run_id);
        assert_eq!(
            resolution["pointer"],
            json!(if expected { "canary" } else { "active" }),
            "run {run_id}: the journaled slot must equal the recomputed draw"
        );
        assert_eq!(
            resolution["revision_id"],
            json!(if expected { &r2 } else { &r1 }),
            "run {run_id}: the slot's revision is the journaled fact"
        );
        if expected {
            canary_hits += 1;
        }
    }

    // The draw is a seeded subset — deterministic per run id, and a
    // proper subset of a larger population (not all, not none).
    let admits: Vec<bool> = (0..200)
        .map(|i| recomputed_admits("staging", &r2, 0.1, &format!("synthetic-run-{i}")))
        .collect();
    let seeded_count = admits.iter().filter(|a| **a).count();
    assert!(
        (1..200).contains(&seeded_count),
        "the seeded draw binds a subset, not everything or nothing: {seeded_count}/200"
    );
    let recompute: Vec<bool> = (0..200)
        .map(|i| recomputed_admits("staging", &r2, 0.1, &format!("synthetic-run-{i}")))
        .collect();
    assert_eq!(admits, recompute, "the draw is deterministic per run id");

    // Clear the canary: 201, the slot empties, the declaration and the
    // clearance both journaled with their authors. Clearing again
    // converges (200, applied: false) — an empty slot is the state, not
    // an error.
    let (status, v) = call(
        &app,
        "DELETE",
        "/deployments/environments/staging/canary",
        Some(json!({"author": author()})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "canary clear failed: {v}");
    assert_eq!(v["pointer"]["canary"], Value::Null);
    assert_eq!(v["pointer"]["active"], json!(r1));
    let (status, v) = call(
        &app,
        "DELETE",
        "/deployments/environments/staging/canary",
        Some(json!({"author": author()})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "re-clear must converge: {v}");
    assert_eq!(v["applied"], json!(false));

    let events = chain_events(&app).await;
    let declared: Vec<&Value> = events
        .iter()
        .filter(|event| event["kind"] == "canary_declared")
        .collect();
    assert_eq!(
        declared.len(),
        1,
        "the converged re-declare did not journal"
    );
    assert_eq!(declared[0]["output"]["value"]["revision_id"], json!(r2));
    assert_eq!(
        declared[0]["output"]["value"]["author"],
        json!({"type": "human", "human_id": "amjad"})
    );
    let cleared: Vec<&Value> = events
        .iter()
        .filter(|event| event["kind"] == "canary_cleared")
        .collect();
    assert_eq!(cleared.len(), 1);
    assert_eq!(
        cleared[0]["output"]["value"]["cleared_revision_id"],
        json!(r2),
        "the clearance names the binding it ended"
    );

    let _ = canary_hits; // the per-run equality above is the proof; the count is scenery
    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Exit clause: the shadow run — refused, served, journaled, compared
// --------------------------------------------------------------------- //

/// A scripted chat model: pops one canned response per `chat` call (the
/// react.rs test double, verbatim).
#[derive(Debug)]
struct ScriptedModel {
    script: Mutex<VecDeque<ChatMessage>>,
}

impl ScriptedModel {
    fn new(script: Vec<ChatMessage>) -> Self {
        Self {
            script: Mutex::new(script.into()),
        }
    }
}

#[async_trait::async_trait]
impl ChatModel for ScriptedModel {
    async fn chat(
        &self,
        _messages: &[ChatMessage],
        _tools: &[Value],
    ) -> RuntimeResult<ChatResponse> {
        let message = self.script.lock().unwrap().pop_front().ok_or_else(|| {
            rusty_agent_runtime::error::RustyError::Llm("script exhausted".into())
        })?;
        Ok(ChatResponse {
            message,
            model: None,
            usage: None,
        })
    }
}

/// The irreversible tool the shadow must refuse: non-idempotent by
/// default (the `Tool::effect` floor), so the boundary refuses it and
/// the recorded world answers for it.
#[derive(Debug)]
struct Charge;

#[async_trait::async_trait]
impl Tool for Charge {
    fn name(&self) -> &str {
        "charge"
    }
    fn description(&self) -> &str {
        "Charges the account."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {"amount": {"type": "integer"}}})
    }
    async fn call(&self, _args: Value) -> RuntimeResult<Value> {
        Ok(json!({"receipt": "r-42"}))
    }
}

/// A read-only lookup: admitted by the shadow boundary — the contrast
/// that proves the refusal is classified, not blanket.
#[derive(Debug)]
struct Lookup;

#[async_trait::async_trait]
impl Tool for Lookup {
    fn name(&self) -> &str {
        "lookup"
    }
    fn description(&self) -> &str {
        "Looks up a row."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {"id": {"type": "integer"}}})
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, _args: Value) -> RuntimeResult<Value> {
        Ok(json!({"row": "x"}))
    }
}

/// Both tools, one registry (the recording run and the shadow run share
/// tool identities — schemas feed the recorded request hashes).
fn shadow_tools() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(Charge);
    registry.register(Lookup);
    registry
}

/// The ReAct spec: one `messages` channel under the AddMessages reducer.
fn shadow_spec() -> StateSpec {
    StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages)
}

/// The recorded world: a source run of the same topology whose journal
/// holds one `charge` (non-idempotent) and one `lookup` (read-only),
/// executed for real and recorded.
async fn recorded_source() -> Value {
    let journal = Journal::new("source-run-1", "source-run-1", Clock::System);
    let model: Arc<dyn ChatModel> = Arc::new(ScriptedModel::new(vec![
        ChatMessage::assistant_tool_calls(vec![
            ToolCall::new("c1", "charge", json!({"amount": 42})),
            ToolCall::new("c2", "lookup", json!({"id": 7})),
        ]),
        ChatMessage::assistant("done"),
    ]));
    let graph = create_react_agent_with_recording(model, shadow_tools(), journal.clone()).unwrap();
    let initial = State::from_value(json!({
        MESSAGES_CHANNEL: [serde_json::to_value(ChatMessage::user("bill me")).unwrap()]
    }))
    .unwrap();
    let outcome = Executor::new()
        .run(
            &graph,
            &shadow_spec(),
            initial,
            RunConfig::new("source-run-1").with_journal(journal.clone()),
        )
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        rusty_agent_runtime::executor::ExecutionOutcome::Done(_)
    ));
    serde_json::to_value(journal.snapshot()).unwrap()
}

/// An app whose registry adds `shadow-graph`: the same ReAct topology,
/// scripted to request exactly the recorded `charge` — and nothing
/// else, so the recorded `lookup` is the divergence the verdict names.
fn shadow_app(store: PathBuf) -> Router {
    let model: Arc<dyn ChatModel> = Arc::new(ScriptedModel::new(vec![
        ChatMessage::assistant_tool_calls(vec![ToolCall::new(
            "c9",
            "charge",
            json!({"amount": 42}),
        )]),
        ChatMessage::assistant("done"),
    ]));
    let graph = create_react_agent(model, shadow_tools()).unwrap();
    let mut registry = pipeline_registry();
    registry.register("shadow-graph", graph, shadow_spec());
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store)
        .with_candidate_evaluator(Arc::new(FixedEvaluator))
        .with_revision_gate_evaluator(Arc::new(FixedGateEvaluator { allow: true }));
    router(registry, config)
}

#[tokio::test]
async fn a_shadow_run_refuses_above_read_only_serves_the_recorded_world_and_journals() {
    let store = temp_store();
    let app = shadow_app(store.clone());
    declare_env(&app, "dev").await;

    // The candidate revision pins the shadow graph (empty pin set — the
    // revision exists to name the graph and its topology).
    let revision_id = {
        let (status, v) = call(
            &app,
            "POST",
            "/deployments/revisions",
            Some(json!({
                "graph": "shadow-graph",
                "source_environment": "dev",
                "surfaces": [],
                "author": author(),
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "revision failed: {v}");
        v["revision"]["revision_id"].as_str().unwrap().to_string()
    };

    let source = recorded_source().await;
    let (status, v) = call(
        &app,
        "POST",
        "/deployments/shadows",
        Some(json!({
            "revision_id": revision_id,
            "source": source,
            "input": {
                "messages": [serde_json::to_value(ChatMessage::user("bill me")).unwrap()]
            },
            "author": author(),
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "shadow failed: {v}");
    let shadow_run_id = v["shadow_run_id"].as_str().unwrap().to_string();
    assert!(shadow_run_id.starts_with("shadow-source-run-1-"));

    // The verdict: the charge was refused (typed non-idempotent) and
    // served from the recorded world; the recorded lookup the candidate
    // never requested is divergence evidence in the other direction.
    let verdict = &v["verdict"];
    assert_eq!(verdict["outcome"], json!("completed"));
    assert_eq!(verdict["source_run_id"], json!("source-run-1"));
    assert_eq!(verdict["revision_id"], json!(revision_id));
    assert_eq!(verdict["refusals"].as_array().unwrap().len(), 1);
    assert_eq!(verdict["refusals"][0]["kind"], json!("charge"));
    assert_eq!(verdict["refusals"][0]["effect"], json!("non_idempotent"));
    assert_eq!(verdict["refusals"][0]["served"], json!(true));
    assert_eq!(verdict["matched"], json!(1));
    assert_eq!(verdict["unserved"], json!(0));
    assert_eq!(verdict["unrequested"], json!(["lookup"]));

    // The shadow's own journal is the evidence: started with
    // `role: shadow` pinned and the source named; the refusal journaled
    // served-or-not as it happened; the verdict last. Shadows never
    // sign production receipts — there is no thread record, by
    // construction.
    let journal_path = store.join("journals").join(format!("{shadow_run_id}.json"));
    let journal: Value =
        serde_json::from_slice(&std::fs::read(&journal_path).expect("shadow journal persisted"))
            .unwrap();
    let events = journal["events"].as_array().unwrap();
    let shadow_kinds = kinds(events);
    // ShadowRunStarted is journaled before the executor runs — it leads.
    assert_eq!(shadow_kinds[0], "shadow_run_started");
    let started = &events[0]["output"]["value"];
    assert_eq!(started["role"], json!("shadow"));
    assert_eq!(started["source_run_id"], json!("source-run-1"));
    assert!(
        shadow_kinds.contains(&"shadow_effect_refused"),
        "the refusal journaled as it happened: {shadow_kinds:?}"
    );
    assert_eq!(shadow_kinds.last().unwrap(), &"shadow_verdict");
    let refused = events
        .iter()
        .find(|event| event["kind"] == "shadow_effect_refused")
        .unwrap();
    assert_eq!(refused["output"]["value"]["kind"], json!("charge"));
    assert_eq!(refused["output"]["value"]["served"], json!(true));
    let (status, _) = call(&app, "GET", &format!("/runs/{shadow_run_id}/receipt"), None).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "shadows never sign production receipts"
    );

    // A source journal that fails integrity is refused, never replayed.
    let mut tampered = recorded_source().await;
    tampered["events"][0]["id"] = json!("forged");
    let (status, v) = call(
        &app,
        "POST",
        "/deployments/shadows",
        Some(json!({
            "revision_id": revision_id,
            "source": tampered,
            "author": author(),
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "tampered evidence is refused: {v}"
    );

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Exit clause: the health board, from journaled data alone
// --------------------------------------------------------------------- //

#[tokio::test]
async fn the_health_board_reports_pointers_canaries_and_gate_decisions_from_journal_data() {
    let (app, store) = app();
    declare_env(&app, "dev").await;
    declare_env_full(&app, "staging", Some(gate_declaration()), false).await;

    // r1 promotes into staging through the gate; r2 canaries at 25%.
    let journal_run = run_pipeline(&app).await;
    let v1 = prompt_candidate("system", "You are terse.", 1_750_000_002_000);
    let id1 = create_and_evaluate(&app, &journal_run, &v1).await;
    declare_and_commit(&app, std::slice::from_ref(&id1)).await;
    promote(&app, &journal_run, &v1, &id1, "staging").await;
    let r1 = create_revision(&app, "staging", &["prompt:system"]).await;
    let (status, v) = deploy_promote_as(&app, "staging", &r1, None).await;
    assert_eq!(status, StatusCode::CREATED, "promote r1 failed: {v}");
    let v2 = prompt_candidate("system", "You are expansive.", 1_750_000_003_000);
    let id2 = create_and_evaluate(&app, &journal_run, &v2).await;
    let (status, v) = call(
        &app,
        "POST",
        "/registry/artifacts/prompt/system/commits",
        Some(json!({"candidate_id": id2})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "commit v2 failed: {v}");
    promote(&app, &journal_run, &v2, &id2, "staging").await;
    let r2 = create_revision(&app, "staging", &["prompt:system"]).await;
    let (status, v) = call(
        &app,
        "PUT",
        "/deployments/environments/staging/canary",
        Some(json!({"revision_id": r2, "fraction": 0.25, "author": author()})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "canary failed: {v}");

    // Twelve bound runs plus two unbound (the unbound pair journals no
    // resolution — the board must not count them).
    let mut run_ids = Vec::new();
    for _ in 0..12 {
        run_ids.push(run_pipeline_bound(&app, "staging").await);
    }
    run_pipeline(&app).await;
    run_pipeline(&app).await;
    let expected_canary = run_ids
        .iter()
        .filter(|run_id| recomputed_admits("staging", &r2, 0.25, run_id))
        .count();
    let expected_active = run_ids.len() - expected_canary;

    let (status, v) = call(&app, "GET", "/deployments/health", None).await;
    assert_eq!(status, StatusCode::OK, "health failed: {v}");
    assert!(
        is_hex64(&v["deployment_chain_head"]),
        "the chain head rides the board: {v}"
    );
    let environments = v["environments"].as_array().unwrap();
    assert_eq!(environments.len(), 2);
    // Sorted by name: dev first, staging second.
    let dev = &environments[0];
    assert_eq!(dev["environment"], json!("dev"));
    assert_eq!(dev["active_revision"], Value::Null);
    assert_eq!(dev["canary"], Value::Null);
    assert_eq!(dev["last_gate_decision"], Value::Null);
    assert_eq!(dev["recent_runs"]["active"]["runs"], json!(0));

    let staging = &environments[1];
    assert_eq!(staging["environment"], json!("staging"));
    assert_eq!(staging["active_revision"], json!(r1));
    assert_eq!(staging["canary"]["revision_id"], json!(r2));
    assert_eq!(staging["canary"]["fraction"], json!(0.25));
    // The board's gate decision is the chain's last for the environment
    // — the canary's, payload verbatim.
    assert_eq!(staging["last_gate_decision"]["outcome"], json!("allow"));
    assert_eq!(staging["last_gate_decision"]["revision_id"], json!(r2));
    assert_eq!(
        staging["last_gate_decision"]["baseline_revision_id"],
        json!(r1),
        "the canary's gate compared against the serving revision"
    );
    assert_eq!(
        staging["recent_runs"]["active"]["runs"],
        json!(expected_active),
        "the active tally equals the recomputed draw"
    );
    assert_eq!(
        staging["recent_runs"]["canary"]["runs"],
        json!(expected_canary)
    );
    assert_eq!(staging["recent_runs"]["active"]["errored"], json!(0));
    assert_eq!(staging["recent_runs"]["canary"]["errored"], json!(0));

    let _ = std::fs::remove_dir_all(store);
}
