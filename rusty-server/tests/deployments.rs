//! The deployment revision integration tests (R0.12 Extension Plane,
//! wave 3): immutable, content-addressed revisions freeze what an
//! environment serves; promotions and rollbacks move the environment's
//! deployment pointer byte-exactly through a journaled, CAS-arbitrated
//! control plane; a run declaring `deployment.environment` binds the
//! serving revision at admission — one `deployment_resolved` event under
//! the receipt's signature — and environment-scoped secrets seal at
//! rest, open only inside their scope, and journal every act and denial.
//! The exit criteria live here:
//!
//! - promote moves the pointer to exactly the promoted revision;
//!   rollback re-points byte-exactly at what served before (then at
//!   serving-nothing), and refuses when nothing serves;
//! - a bound run resolves the serving revision at admission, journaled
//!   with the pin-set digest, and the evidence walks receipt → signed
//!   head → resolution event → revision (address-verified);
//! - an admitted run keeps its resolution when the pointer moves
//!   afterwards (the registry wave-2 conservatism, lifted);
//! - env secrets never touch disk in plaintext, a cross-environment
//!   resolution fails closed typed (`environment_scope_denied`) and
//!   journaled, and revocation is an attributable, journaled act;
//! - the whole control plane — revisions, environments, pointers,
//!   secret metadata, the evidence chain — survives a restart.
//!
//! Driven in-process via `tower::ServiceExt::oneshot` (no sockets), the
//! `registry_admission.rs` harness convention — the learn pipeline is
//! scenery (a scripted evaluator promotes prompt candidates into the
//! staging environment the revisions freeze); the deployment control
//! plane is the proof.

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use chrono::{DateTime, Utc};
use rusty_agent_runtime::deploy::{pin_set_digest, DeploymentRevision};
use rusty_agent_runtime::effects::ApprovalToken;
use rusty_agent_runtime::error::Result as RuntimeResult;
use rusty_agent_runtime::learn::{
    promotion_effect_id, Candidate, CandidateContent, CandidateEvaluation, CandidateEvaluator,
    EvaluationRequest, EvaluationVerdict, EvidenceSpan, ReplaySummary,
};
use rusty_agent_runtime::memory::ProvenanceAuthor;
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

// --------------------------------------------------------------------- //
// Harness (the registry_admission.rs shapes, verbatim where the
// semantics match)
// --------------------------------------------------------------------- //

/// Unique temp store root, removed at the end of each test (best effort).
fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!(
        "rusty-server-deployments-test-{}",
        uuid::Uuid::new_v4()
    ))
}

/// A fixed timestamp, so candidates minted here are deterministic.
fn ts(millis: i64) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp_millis(millis).unwrap()
}

/// The scripted evaluator: a clean replay and an improving verdict —
/// enough to clear the approval bar. This wave proves deployments; the
/// evaluator is scenery.
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

/// An app over `store` with the scripted evaluator registered and the
/// config customized by `configure` (tenant keys on the Postgres leg).
fn app_with(store: PathBuf, configure: impl FnOnce(ServerConfig) -> ServerConfig) -> Router {
    let config = configure(ServerConfig::new("127.0.0.1:0".parse().unwrap(), store))
        .with_candidate_evaluator(Arc::new(FixedEvaluator));
    router(pipeline_registry(), config)
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

/// Create a thread and run it to completion; returns the run id.
async fn run_pipeline(app: &Router) -> String {
    run_pipeline_as(app, None).await
}

/// Create a thread and run it to completion under an auth context.
async fn run_pipeline_as(app: &Router, auth: Option<(&str, &str)>) -> String {
    let (status, v) = call_as(
        app,
        auth,
        "POST",
        "/threads",
        Some(json!({"graph": "pipeline"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "thread failed: {v}");
    let thread_id = v["thread_id"].as_str().unwrap().to_string();
    let (status, v) = call_as(
        app,
        auth,
        "POST",
        &format!("/threads/{thread_id}/runs/wait"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "run failed: {v}");
    v["run_id"].as_str().unwrap().to_string()
}

/// Create a thread and submit a deployment-bound run; returns
/// `(status, body)` — admission failures assert the error, so nothing
/// asserts here.
async fn submit_bound_as(
    app: &Router,
    auth: Option<(&str, &str)>,
    environment: &str,
) -> (StatusCode, Value) {
    let (status, v) = call_as(
        app,
        auth,
        "POST",
        "/threads",
        Some(json!({"graph": "pipeline"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "thread failed: {v}");
    let thread_id = v["thread_id"].as_str().unwrap().to_string();
    call_as(
        app,
        auth,
        "POST",
        &format!("/threads/{thread_id}/runs/wait"),
        Some(json!({"deployment": {"environment": environment}})),
    )
    .await
}

/// Create a thread and run it to completion under a deployment binding;
/// asserts admission and completion, returns the run id.
async fn run_pipeline_bound(app: &Router, environment: &str) -> String {
    let (status, v) = submit_bound_as(app, None, environment).await;
    assert_eq!(status, StatusCode::OK, "bound run failed: {v}");
    v["run_id"].as_str().unwrap().to_string()
}

/// Export the run's journal snapshot (via the portable fixture).
async fn snapshot_of(app: &Router, run_id: &str) -> Value {
    snapshot_of_as(app, None, run_id).await
}

/// The auth-carrying form of [`snapshot_of`].
async fn snapshot_of_as(app: &Router, auth: Option<(&str, &str)>, run_id: &str) -> Value {
    let (status, v) = call_as(app, auth, "GET", &format!("/runs/{run_id}/fixture"), None).await;
    assert_eq!(status, StatusCode::OK, "fixture failed: {v}");
    v["journal"].clone()
}

/// Mint (or serve) the run's receipt; asserts 200.
async fn receipt_of(app: &Router, run_id: &str) -> Value {
    let (status, v) = call(app, "GET", &format!("/runs/{run_id}/receipt"), None).await;
    assert_eq!(status, StatusCode::OK, "receipt failed: {v}");
    v
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

// --------------------------------------------------------------------- //
// Fixtures
// --------------------------------------------------------------------- //

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
    create_and_evaluate_as(app, None, run_id, candidate).await
}

/// The auth-carrying form of [`create_and_evaluate`].
async fn create_and_evaluate_as(
    app: &Router,
    auth: Option<(&str, &str)>,
    run_id: &str,
    candidate: &Candidate,
) -> String {
    let (status, v) = call_as(
        app,
        auth,
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
    let (status, v) = call_as(
        app,
        auth,
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
    declare_and_commit_as(app, None, candidate_ids).await
}

/// The auth-carrying form of [`declare_and_commit`].
async fn declare_and_commit_as(app: &Router, auth: Option<(&str, &str)>, candidate_ids: &[String]) {
    let (status, v) = call_as(
        app,
        auth,
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
        let (status, v) = call_as(
            app,
            auth,
            "POST",
            "/registry/artifacts/prompt/system/commits",
            Some(json!({"candidate_id": candidate_id})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "commit failed: {v}");
    }
}

/// Commit a candidate to the already-declared `prompt:system` artifact;
/// asserts 200.
async fn commit_candidate(app: &Router, candidate_id: &str) {
    let (status, v) = call(
        app,
        "POST",
        "/registry/artifacts/prompt/system/commits",
        Some(json!({"candidate_id": candidate_id})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "commit failed: {v}");
}

/// Promote `candidate` to the environment tag through the learn route
/// (the registry pointer the revision freezes); asserts 200.
async fn promote(app: &Router, run_id: &str, candidate: &Candidate, candidate_id: &str, tag: &str) {
    promote_as(app, None, run_id, candidate, candidate_id, tag).await
}

/// The auth-carrying form of [`promote`].
async fn promote_as(
    app: &Router,
    auth: Option<(&str, &str)>,
    run_id: &str,
    candidate: &Candidate,
    candidate_id: &str,
    tag: &str,
) {
    let (status, v) = call_as(
        app,
        auth,
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

// --------------------------------------------------------------------- //
// Deployment control-plane helpers
// --------------------------------------------------------------------- //

/// Declare an environment; returns `(status, body)`.
async fn declare_env_as(
    app: &Router,
    auth: Option<(&str, &str)>,
    name: &str,
) -> (StatusCode, Value) {
    call_as(
        app,
        auth,
        "POST",
        "/deployments/environments",
        Some(json!({"name": name, "author": author()})),
    )
    .await
}

/// Declare an environment; asserts 201.
async fn declare_env(app: &Router, name: &str) {
    let (status, v) = declare_env_as(app, None, name).await;
    assert_eq!(status, StatusCode::CREATED, "declare failed: {v}");
    assert_eq!(v["created"], json!(true));
    assert_eq!(v["environment"]["name"], json!(name));
}

/// Create a revision freezing `surfaces` from `source_environment`;
/// returns `(status, body)`.
async fn create_revision_as(
    app: &Router,
    auth: Option<(&str, &str)>,
    source_environment: &str,
    surfaces: &[&str],
) -> (StatusCode, Value) {
    call_as(
        app,
        auth,
        "POST",
        "/deployments/revisions",
        Some(json!({
            "graph": "pipeline",
            "source_environment": source_environment,
            "surfaces": surfaces,
            "author": author(),
        })),
    )
    .await
}

/// Create a revision; asserts 201 and returns the revision id.
async fn create_revision(app: &Router, source_environment: &str, surfaces: &[&str]) -> String {
    let (status, v) = create_revision_as(app, None, source_environment, surfaces).await;
    assert_eq!(status, StatusCode::CREATED, "create failed: {v}");
    assert_eq!(v["created"], json!(true));
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
    auth: Option<(&str, &str)>,
    environment: &str,
    revision_id: &str,
) -> (StatusCode, Value) {
    call_as(
        app,
        auth,
        "POST",
        &format!("/deployments/environments/{environment}/promote"),
        Some(json!({"revision_id": revision_id, "author": author()})),
    )
    .await
}

/// Promote a revision into an environment; asserts 201 and the moved
/// pointer.
async fn deploy_promote(app: &Router, environment: &str, revision_id: &str) -> Value {
    let (status, v) = deploy_promote_as(app, None, environment, revision_id).await;
    assert_eq!(status, StatusCode::CREATED, "promote failed: {v}");
    assert_eq!(v["applied"], json!(true));
    assert_eq!(v["journaled"], json!(true));
    assert!(v["event_id"].is_string());
    v
}

/// Roll an environment back; returns `(status, body)`.
async fn deploy_rollback(app: &Router, environment: &str, cause: &str) -> (StatusCode, Value) {
    call(
        app,
        "POST",
        &format!("/deployments/environments/{environment}/rollback"),
        Some(json!({"author": author(), "cause": cause})),
    )
    .await
}

/// The environment's deployment pointer; asserts 200.
async fn pointer_of(app: &Router, environment: &str) -> Value {
    let (status, v) = call(
        app,
        "GET",
        &format!("/deployments/environments/{environment}/pointer"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "pointer failed: {v}");
    v["pointer"].clone()
}

/// Set (or rotate) an environment secret; returns `(status, body)`.
async fn set_secret_as(
    app: &Router,
    auth: Option<(&str, &str)>,
    name: &str,
    environment: &str,
    value: Value,
) -> (StatusCode, Value) {
    call_as(
        app,
        auth,
        "PUT",
        "/deployments/secrets",
        Some(json!({
            "name": name,
            "environment": environment,
            "value": value,
            "author": author(),
        })),
    )
    .await
}

/// Resolve an environment secret under `holder`; returns `(status,
/// body)`.
async fn resolve_secret_as(
    app: &Router,
    auth: Option<(&str, &str)>,
    name: &str,
    environment: &str,
    holder: &str,
) -> (StatusCode, Value) {
    call_as(
        app,
        auth,
        "POST",
        "/deployments/secrets/resolve",
        Some(json!({
            "name": name,
            "environment": environment,
            "holder": holder,
        })),
    )
    .await
}

/// Stage the shared scenery: three environments, one prompt candidate
/// promoted into staging through the learn pipeline, and one revision
/// freezing it. Returns `(journal_run, candidate_id, revision_id)`.
async fn stage_revision(app: &Router, prompt_text: &str, millis: i64) -> (String, String, String) {
    for environment in ["dev", "staging", "prod"] {
        declare_env(app, environment).await;
    }
    let journal_run = run_pipeline(app).await;
    let candidate = prompt_candidate("system", prompt_text, millis);
    let candidate_id = create_and_evaluate(app, &journal_run, &candidate).await;
    declare_and_commit(app, std::slice::from_ref(&candidate_id)).await;
    promote(app, &journal_run, &candidate, &candidate_id, "staging").await;
    let revision_id = create_revision(app, "staging", &["prompt:system"]).await;
    (journal_run, candidate_id, revision_id)
}

// --------------------------------------------------------------------- //
// Exit criterion: promote moves the pointer; rollback re-points
// byte-exactly
// --------------------------------------------------------------------- //

#[tokio::test]
async fn promotion_and_rollback_move_the_pointer_byte_exactly() {
    let (app, store) = app();

    // Declare the three environments (201); an identical re-declaration
    // converges (200, created: false); a changed rule under the same
    // name conflicts (409) — declarations are immutable.
    for environment in ["dev", "staging", "prod"] {
        declare_env(&app, environment).await;
    }
    let (status, v) = declare_env_as(&app, None, "staging").await;
    assert_eq!(status, StatusCode::OK, "re-declare must converge: {v}");
    assert_eq!(v["created"], json!(false));
    let (status, v) = call(
        &app,
        "POST",
        "/deployments/environments",
        Some(json!({
            "name": "staging",
            "approval_required": true,
            "author": author(),
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a changed declaration is a new environment, not an edit: {v}"
    );

    // Undeclared environments refuse everywhere: 404, never an invented
    // default.
    let (status, _) = call(&app, "GET", "/deployments/environments/ghost", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, v) = deploy_promote_as(&app, None, "ghost", &"0".repeat(64)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "undeclared promote: {v}");

    // v1 promotes into staging through the learn pipeline; r1 freezes
    // staging's active pin — the content address, the graph's current
    // topology hash, the pinned candidate.
    let journal_run = run_pipeline(&app).await;
    let v1 = prompt_candidate("system", "You are terse.", 1_750_000_002_000);
    let id1 = create_and_evaluate(&app, &journal_run, &v1).await;
    declare_and_commit(&app, std::slice::from_ref(&id1)).await;
    promote(&app, &journal_run, &v1, &id1, "staging").await;
    let r1 = create_revision(&app, "staging", &["prompt:system"]).await;
    let body = get_revision(&app, &r1).await;
    assert_eq!(body["revision"]["content"]["graph"], json!("pipeline"));
    assert!(
        is_hex64(&body["revision"]["content"]["graph_hash"]),
        "the server computes the topology hash at registration: {body}"
    );
    assert_eq!(
        body["revision"]["content"]["source_environment"],
        json!("staging")
    );
    assert_eq!(
        body["revision"]["content"]["pins"],
        json!([{"surface": "prompt:system", "candidate_id": id1}]),
        "the pin froze staging's active pointer at creation"
    );

    // An identical re-registration converges: 200, created: false, the
    // same content address — and no second journal entry.
    let chain_len = chain_events(&app).await.len();
    let (status, v) = create_revision_as(&app, None, "staging", &["prompt:system"]).await;
    assert_eq!(status, StatusCode::OK, "re-register must converge: {v}");
    assert_eq!(v["created"], json!(false));
    assert_eq!(v["revision"]["revision_id"], json!(r1));
    assert_eq!(
        chain_events(&app).await.len(),
        chain_len,
        "a converged re-registration journals nothing"
    );

    // A revision against an undeclared source environment refuses (404);
    // one whose surface was never promoted there refuses (422 — an
    // unresolvable pin is no pin).
    let (status, v) = create_revision_as(&app, None, "ghost", &["prompt:system"]).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "undeclared source: {v}");
    let (status, v) = create_revision_as(&app, None, "dev", &["prompt:system"]).await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "nothing promoted in dev: {v}"
    );

    // v2 promotes into staging; r2 freezes v2 — a different declaration,
    // a different address.
    let v2 = prompt_candidate("system", "You are warm and thorough.", 1_750_000_003_000);
    let id2 = create_and_evaluate(&app, &journal_run, &v2).await;
    commit_candidate(&app, &id2).await;
    promote(&app, &journal_run, &v2, &id2, "staging").await;
    let r2 = create_revision(&app, "staging", &["prompt:system"]).await;
    assert_ne!(r1, r2, "changed pins mint a new content address");

    // Promote r1, then r2, into prod: the pointer moves byte-exactly,
    // each move journaled.
    let v = deploy_promote(&app, "prod", &r1).await;
    assert_eq!(v["pointer"]["active"], json!(r1));
    let v = deploy_promote(&app, "prod", &r2).await;
    assert_eq!(v["pointer"]["active"], json!(r2));

    // A re-issued promotion converges: 200 {applied: false}, no journal
    // noise.
    let chain_len = chain_events(&app).await.len();
    let (status, v) = deploy_promote_as(&app, None, "prod", &r2).await;
    assert_eq!(status, StatusCode::OK, "converged re-issue: {v}");
    assert_eq!(v["applied"], json!(false));
    assert_eq!(v["journaled"], json!(false));
    assert_eq!(v["pointer"]["active"], json!(r2));
    assert_eq!(
        chain_events(&app).await.len(),
        chain_len,
        "a converged re-issue journals nothing"
    );

    // An unknown revision refuses (404).
    let (status, v) = deploy_promote_as(&app, None, "prod", &"f".repeat(64)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "unknown revision: {v}");

    // Rollback re-points byte-exactly at r1 — the journaled history,
    // never a reconstruction. A second rollback restores
    // serving-nothing; a third refuses (409); a cause-less rollback
    // refuses (400 — a rollback without a stated cause is
    // indistinguishable from a fat finger).
    let (status, v) = deploy_rollback(&app, "prod", "incident-7: soak regressed").await;
    assert_eq!(status, StatusCode::CREATED, "rollback failed: {v}");
    assert_eq!(
        v["pointer"]["active"],
        json!(r1),
        "the rollback re-points byte-exactly at what served before"
    );
    let (status, v) = deploy_rollback(&app, "prod", "revert to serving nothing").await;
    assert_eq!(status, StatusCode::CREATED, "second rollback failed: {v}");
    assert!(
        v["pointer"]["active"].is_null(),
        "rolling the only promotion back restores serving-nothing: {v}"
    );
    let (status, v) = deploy_rollback(&app, "prod", "nothing left to restore").await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "nothing serves — there is nothing to roll back: {v}"
    );
    let (status, v) = deploy_rollback(&app, "prod", "   ").await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a rollback names its cause: {v}"
    );

    // The evidence chain records the control plane's history, in order,
    // one event per act — nothing converged or denied appears here.
    let events = chain_events(&app).await;
    assert_eq!(
        kinds(&events),
        vec![
            "environment_declared",
            "environment_declared",
            "environment_declared",
            "revision_registered",
            "revision_registered",
            "revision_promoted",
            "revision_promoted",
            "revision_rolled_back",
            "revision_rolled_back",
        ],
        "the chain is the control plane's journal, in order: {events:?}"
    );
    for (seq, event) in events.iter().enumerate() {
        assert_eq!(event["seq"], json!(seq as u64));
        assert_eq!(event["id"], json!(format!("deployment-control:{seq}")));
    }

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Exit criterion: the bound run resolves the serving revision at
// admission; the evidence walks receipt → head → event → revision
// --------------------------------------------------------------------- //

#[tokio::test]
async fn a_bound_run_resolves_the_serving_revision_and_the_receipt_walks() {
    let (app, store) = app();
    let (_journal_run, _candidate, r1) =
        stage_revision(&app, "You are terse.", 1_750_000_002_000).await;
    deploy_promote(&app, "prod", &r1).await;

    // The bound run resolves the serving revision at admission: one
    // `deployment_resolved` event naming the environment, the revision,
    // the pointer slot, and the pin-set digest.
    let run_a = run_pipeline_bound(&app, "prod").await;
    let snapshot_a = snapshot_of(&app, &run_a).await;
    let resolved_a = deployment_resolutions(&snapshot_a);
    assert_eq!(resolved_a.len(), 1, "one binding, one resolution event");
    assert_eq!(resolved_a[0]["environment"], json!("prod"));
    assert_eq!(resolved_a[0]["revision_id"], json!(r1));
    assert_eq!(resolved_a[0]["pointer"], json!("active"));

    // The journaled pin-set digest recomputes from the revision's own
    // pins — one derivation, two addresses — and the served revision
    // verifies its content address.
    let body = get_revision(&app, &r1).await;
    let revision: DeploymentRevision = serde_json::from_value(body["revision"].clone())
        .expect("the served revision deserializes to the contract type");
    revision
        .verify_address()
        .expect("the served revision verifies its content address");
    assert_eq!(
        resolved_a[0]["pin_set_digest"],
        json!(pin_set_digest(&revision.content.pins)),
        "the journaled digest is the revision's pin-set digest"
    );

    // The event sits inside the signed head the receipt attests, and the
    // receipt verifies against the exported journal.
    let event = snapshot_a["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == json!("deployment_resolved"))
        .unwrap();
    let receipt_a = receipt_of(&app, &run_a).await;
    assert!(
        event["seq"].as_u64().unwrap() < receipt_a["journal_head"]["events"].as_u64().unwrap(),
        "the resolution event is covered by the receipt's signed journal head"
    );
    let (status, v) = call(
        &app,
        "POST",
        "/receipts/verify",
        Some(json!({"snapshot": snapshot_a, "receipt": receipt_a})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "verify failed: {v}");

    // Conservatism: a promotion landing *after* admission never reaches
    // the admitted run — its journal still names the revision it
    // resolved.
    let v2 = prompt_candidate("system", "You are warm and thorough.", 1_750_000_003_000);
    let journal_run = run_pipeline(&app).await;
    let id2 = create_and_evaluate(&app, &journal_run, &v2).await;
    commit_candidate(&app, &id2).await;
    promote(&app, &journal_run, &v2, &id2, "staging").await;
    let r2 = create_revision(&app, "staging", &["prompt:system"]).await;
    deploy_promote(&app, "prod", &r2).await;

    let resolved_a_after = deployment_resolutions(&snapshot_of(&app, &run_a).await);
    assert_eq!(resolved_a_after.len(), 1);
    assert_eq!(
        resolved_a_after[0]["revision_id"],
        json!(r1),
        "a later promotion appends nothing to an admitted run"
    );

    // …and the *next* bound run resolves the revision now serving.
    let run_b = run_pipeline_bound(&app, "prod").await;
    let resolved_b = deployment_resolutions(&snapshot_of(&app, &run_b).await);
    assert_eq!(resolved_b.len(), 1);
    assert_eq!(resolved_b[0]["revision_id"], json!(r2));

    // Admission refusals: an undeclared environment (404); a declared
    // environment nothing was ever promoted into (404 — never an
    // invented default); and an unbound run journals no resolution, the
    // pre-R0.12 behavior byte-identically.
    let (status, v) = submit_bound_as(&app, None, "ghost").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "undeclared binding: {v}");
    let (status, v) = submit_bound_as(&app, None, "staging").await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "staging holds no deployment pointer: {v}"
    );
    assert!(
        v["message"].as_str().unwrap().contains("serves nothing"),
        "the refusal says why: {v}"
    );
    let unbound = run_pipeline(&app).await;
    assert!(
        deployment_resolutions(&snapshot_of(&app, &unbound).await).is_empty(),
        "an unbound run journals no resolution"
    );

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Exit criterion: env secrets seal at rest, open inside their scope,
// journal every act and denial
// --------------------------------------------------------------------- //

/// The plaintext marker: distinctive enough that no envelope hex could
/// contain it by accident.
const MARKER: &str = "sk-live-9f8e7d6c5b4a-plaintext-never-on-disk";

#[tokio::test]
async fn env_secrets_seal_scope_and_revoke() {
    let (app, store) = app();
    declare_env(&app, "staging").await;
    declare_env(&app, "prod").await;

    // Set (201): the response is the metadata record — the value never
    // comes back through this route.
    let (status, v) =
        set_secret_as(&app, None, "openai", "staging", json!({"api_key": MARKER})).await;
    assert_eq!(status, StatusCode::CREATED, "set failed: {v}");
    assert_eq!(v["created"], json!(true));
    assert_eq!(v["record"]["name"], json!("openai"));
    assert_eq!(v["record"]["environment"], json!("staging"));
    assert!(v["record"]["rotated_at"].is_null());
    assert!(
        v["record"].get("value").is_none() && v["record"].get("envelope").is_none(),
        "the record is metadata, never the secret: {v}"
    );
    let created_at = v["record"]["created_at"].clone();

    // Rotate (200): created_at preserved, rotated_at marks it.
    let (status, v) =
        set_secret_as(&app, None, "openai", "staging", json!({"api_key": MARKER})).await;
    assert_eq!(status, StatusCode::OK, "rotate failed: {v}");
    assert_eq!(v["created"], json!(false));
    assert_eq!(v["record"]["created_at"], created_at);
    assert!(v["record"]["rotated_at"].is_string());

    // The listing is an audit view: metadata only, filterable by
    // environment.
    let (status, v) = call(
        &app,
        "GET",
        "/deployments/secrets?environment=staging",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "list failed: {v}");
    let secrets = v["secrets"].as_array().unwrap();
    assert_eq!(secrets.len(), 1);
    assert_eq!(secrets[0]["name"], json!("openai"));
    assert!(
        secrets[0].get("value").is_none() && secrets[0].get("envelope").is_none(),
        "a listing never serves envelopes: {secrets:?}"
    );

    // Resolve inside the scope: the exact value returns.
    let (status, v) = resolve_secret_as(&app, None, "openai", "staging", "staging").await;
    assert_eq!(status, StatusCode::OK, "resolve failed: {v}");
    assert_eq!(v["value"], json!({"api_key": MARKER}));

    // The custody proof: nothing under `env-secrets/` holds the
    // plaintext — not the value, not even its field name. Every byte on
    // disk is the sealed envelope.
    let dir = store.join("env-secrets");
    let mut files = 0;
    for entry in std::fs::read_dir(&dir).expect("the env-secrets directory exists") {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            continue;
        }
        files += 1;
        let bytes = std::fs::read(entry.path()).unwrap();
        let contains = |needle: &str| bytes.windows(needle.len()).any(|w| w == needle.as_bytes());
        assert!(
            !contains(MARKER),
            "plaintext in {:?} — the custody boundary broke",
            entry.path()
        );
        assert!(
            !contains("api_key"),
            "plaintext structure in {:?} — the custody boundary broke",
            entry.path()
        );
    }
    assert!(files > 0, "the sealed envelope must be on disk");

    // Cross-scope resolution fails closed: typed 403, journaled denial —
    // a value never crosses its environment's boundary.
    let (status, v) = resolve_secret_as(&app, None, "openai", "staging", "prod").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "cross-scope resolve: {v}");
    assert_eq!(v["error"], json!("environment_scope_denied"));
    let events = chain_events(&app).await;
    let denial = events
        .iter()
        .find(|e| e["kind"] == json!("env_secret_denied"))
        .expect("the denial journaled");
    assert_eq!(
        denial["output"]["value"]["requested_environment"],
        json!("staging")
    );
    assert_eq!(denial["output"]["value"]["held_environment"], json!("prod"));
    assert_eq!(denial["output"]["value"]["name"], json!("openai"));

    // The scope is the environment's, not the value's: a prod secret
    // opens for a prod holder and is just as denied to a staging one.
    let (status, v) = set_secret_as(
        &app,
        None,
        "openai",
        "prod",
        json!({"api_key": format!("prod-{MARKER}")}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "prod set failed: {v}");
    let (status, v) = resolve_secret_as(&app, None, "openai", "prod", "staging").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "cross-scope prod: {v}");
    assert_eq!(v["error"], json!("environment_scope_denied"));
    let (status, v) = resolve_secret_as(&app, None, "openai", "prod", "prod").await;
    assert_eq!(status, StatusCode::OK, "prod resolve failed: {v}");
    assert_eq!(v["value"]["api_key"], json!(format!("prod-{MARKER}")));

    // Revocation journals, deletes, and the resolve then 404s; a
    // re-revoke is 404 (the tombstone is the chain's, not the store's).
    let (status, v) = call(
        &app,
        "DELETE",
        "/deployments/secrets/staging/openai",
        Some(json!({"author": author()})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "revoke failed: {v}");
    let (status, _) = resolve_secret_as(&app, None, "openai", "staging", "staging").await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "revoked secret must not open"
    );
    let (status, _) = call(
        &app,
        "DELETE",
        "/deployments/secrets/staging/openai",
        Some(json!({"author": author()})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "re-revoke must 404");

    // The chain records the custody history, in order — acts and
    // denials alike.
    let events = chain_events(&app).await;
    assert_eq!(
        kinds(&events),
        vec![
            "environment_declared",
            "environment_declared",
            "env_secret_set",
            "env_secret_set",
            "env_secret_denied",
            "env_secret_set",
            "env_secret_denied",
            "env_secret_revoked",
        ],
        "the chain is the custody journal, in order: {events:?}"
    );

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Exit criterion: the whole control plane survives a restart
// --------------------------------------------------------------------- //

#[tokio::test]
async fn the_control_plane_survives_a_restart() {
    let store = temp_store();
    let first = app_with(store.clone(), |config| config);
    declare_env(&first, "staging").await;
    let journal_run = run_pipeline(&first).await;
    let v1 = prompt_candidate("system", "You are terse.", 1_750_000_002_000);
    let id1 = create_and_evaluate(&first, &journal_run, &v1).await;
    declare_and_commit(&first, std::slice::from_ref(&id1)).await;
    promote(&first, &journal_run, &v1, &id1, "staging").await;
    let revision_id = create_revision(&first, "staging", &["prompt:system"]).await;
    deploy_promote(&first, "staging", &revision_id).await;
    let (status, v) = set_secret_as(
        &first,
        None,
        "openai",
        "staging",
        json!({"api_key": MARKER}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "set failed: {v}");
    let record_before = v["record"].clone();
    let chain_len_before = chain_events(&first).await.len();
    let pointer_before = pointer_of(&first, "staging").await;
    drop(first);

    // A second router over the same store root — the restart: the
    // revision (address-verifying), the environment, the pointer, the
    // secret metadata, and the evidence chain are all back.
    let second = app_with(store.clone(), |config| config);
    let body = get_revision(&second, &revision_id).await;
    let revision: DeploymentRevision = serde_json::from_value(body["revision"].clone())
        .expect("the revision deserializes after restart");
    revision
        .verify_address()
        .expect("the revision verifies its content address after restart");

    let (status, v) = call(&second, "GET", "/deployments/environments", None).await;
    assert_eq!(status, StatusCode::OK, "list environments failed: {v}");
    let environments = v["environments"].as_array().unwrap();
    assert_eq!(environments.len(), 1);
    assert_eq!(environments[0]["name"], json!("staging"));

    assert_eq!(
        pointer_of(&second, "staging").await,
        pointer_before,
        "the pointer survives the restart byte-exactly"
    );

    let (status, v) = call(
        &second,
        "GET",
        "/deployments/secrets?environment=staging",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "list secrets failed: {v}");
    assert_eq!(
        v["secrets"].as_array().unwrap()[0],
        record_before,
        "the secret metadata survives the restart byte-exactly"
    );

    // …and the secret still opens inside its scope: the master keys live
    // beside the store, so the restarted host holds them.
    let (status, v) = resolve_secret_as(&second, None, "openai", "staging", "staging").await;
    assert_eq!(status, StatusCode::OK, "post-restart resolve failed: {v}");
    assert_eq!(v["value"], json!({"api_key": MARKER}));

    // The chain reloaded and re-verified — and a fresh act journals on
    // top of it.
    assert_eq!(
        chain_events(&second).await.len(),
        chain_len_before,
        "the chain reloads intact across the restart"
    );
    let (status, v) = deploy_rollback(&second, "staging", "post-restart revert").await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "post-restart rollback failed: {v}"
    );
    assert_eq!(chain_events(&second).await.len(), chain_len_before + 1);
    assert!(pointer_of(&second, "staging").await["active"].is_null());

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Postgres backend (live database required)
//
// Gated on the `postgres` feature at compile time and on
// `RUSTY_TEST_DATABASE_URL`/`DATABASE_URL` at run time; unset skips
// cleanly so the suite is green without a database. A dedicated tenant
// per run, so repeated runs against one scratch database never
// interfere; the database itself is throwaway.
// --------------------------------------------------------------------- //

#[cfg(feature = "postgres")]
mod postgres {
    use super::*;
    use sqlx::Row;

    fn pg_url() -> Option<String> {
        std::env::var("RUSTY_TEST_DATABASE_URL")
            .ok()
            .or_else(|| std::env::var("DATABASE_URL").ok())
    }

    /// The wave's custody exit criterion on Postgres — the raw row, a
    /// database dump byte for byte, holds no plaintext secret — plus the
    /// core flow end to end on the Postgres backend: declare, freeze,
    /// promote, bind, resolve.
    #[tokio::test]
    async fn postgres_deployment_flow_and_no_plaintext_dump() {
        let Some(url) = pg_url() else {
            eprintln!("RUSTY_TEST_DATABASE_URL/DATABASE_URL unset; skipping");
            return;
        };
        let tenant = format!("deploypg-{}", uuid::Uuid::new_v4().simple());
        let auth = Some(("x-api-key", "pg-secret"));
        let build = || {
            app_with(temp_store(), |config| {
                config
                    .with_postgres(url.clone())
                    .with_tenant_key(tenant.clone(), "pg-secret")
            })
        };
        let app = build();

        // Declare, freeze, promote, bind — the core flow on Postgres.
        let (status, v) = declare_env_as(&app, auth, "staging").await;
        assert_eq!(status, StatusCode::CREATED, "declare failed: {v}");
        let journal_run = run_pipeline_as(&app, auth).await;
        let candidate = prompt_candidate("system", "You are terse.", 1_750_000_002_000);
        let candidate_id = create_and_evaluate_as(&app, auth, &journal_run, &candidate).await;
        declare_and_commit_as(&app, auth, std::slice::from_ref(&candidate_id)).await;
        promote_as(
            &app,
            auth,
            &journal_run,
            &candidate,
            &candidate_id,
            "staging",
        )
        .await;
        let (status, v) = create_revision_as(&app, auth, "staging", &["prompt:system"]).await;
        assert_eq!(status, StatusCode::CREATED, "create failed: {v}");
        let revision_id = v["revision"]["revision_id"].as_str().unwrap().to_string();
        let (status, v) = deploy_promote_as(&app, auth, "staging", &revision_id).await;
        assert_eq!(status, StatusCode::CREATED, "promote failed: {v}");
        assert_eq!(v["pointer"]["active"], json!(revision_id));

        let (status, v) = submit_bound_as(&app, auth, "staging").await;
        assert_eq!(status, StatusCode::OK, "bound run failed: {v}");
        let run_id = v["run_id"].as_str().unwrap().to_string();
        let resolved = deployment_resolutions(&snapshot_of_as(&app, auth, &run_id).await);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0]["revision_id"], json!(revision_id));

        // Set a secret, resolve it in-scope, then read the raw rows.
        let (status, v) =
            set_secret_as(&app, auth, "openai", "staging", json!({"api_key": MARKER})).await;
        assert_eq!(status, StatusCode::CREATED, "set failed: {v}");
        let (status, v) = resolve_secret_as(&app, auth, "openai", "staging", "staging").await;
        assert_eq!(status, StatusCode::OK, "resolve failed: {v}");
        assert_eq!(v["value"], json!({"api_key": MARKER}));

        let pool = sqlx::PgPool::connect(&url).await.unwrap();
        let row = sqlx::query(
            "SELECT payload::text AS payload FROM server_env_secrets WHERE tenant = $1",
        )
        .bind(&tenant)
        .fetch_one(&pool)
        .await
        .unwrap();
        let payload: String = row.get("payload");
        assert!(
            !payload.contains(MARKER),
            "the Postgres row holds the plaintext secret"
        );
        assert!(
            !payload.contains("api_key"),
            "the Postgres row holds the plaintext structure"
        );
        // The envelope's shape, at the row level: sealed, keyed, hex.
        let parsed: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(parsed["envelope"]["format_version"], json!(1));
        assert!(parsed["envelope"]["key_id"].is_string());

        // The deployments table holds the control plane: the revision,
        // the environment, the pointer — projected metadata, JSONB
        // payloads.
        let rows =
            sqlx::query("SELECT kind, environment FROM server_deployments WHERE tenant = $1")
                .bind(&tenant)
                .fetch_all(&pool)
                .await
                .unwrap();
        let mut kinds: Vec<(String, Option<String>)> = rows
            .iter()
            .map(|row| (row.get("kind"), row.get("environment")))
            .collect();
        kinds.sort();
        assert_eq!(
            kinds,
            vec![
                ("environment".to_owned(), Some("staging".to_owned())),
                ("pointer".to_owned(), Some("staging".to_owned())),
                // The revision row's `environment` projection is its
                // source environment (where its pins froze).
                ("revision".to_owned(), Some("staging".to_owned())),
            ],
            "the control plane's three kinds live in one table: {kinds:?}"
        );

        // A rebuilt router over the same database (the restart stand-in)
        // serves the settled control plane.
        drop(app);
        let second = build();
        let (status, v) = call_as(
            &second,
            auth,
            "GET",
            &format!("/deployments/revisions/{revision_id}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "post-restart revision failed: {v}");
        let (status, v) = call_as(
            &second,
            auth,
            "GET",
            "/deployments/environments/staging/pointer",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "post-restart pointer failed: {v}");
        assert_eq!(v["pointer"]["active"], json!(revision_id));
    }
}
