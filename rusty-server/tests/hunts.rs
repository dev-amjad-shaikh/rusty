//! Hunting-loop integration tests (demand-side learning, wave 4): the
//! `/hunts` surface over the default JSON-file backend — the bounded
//! cycle spending its budget on the work order's top entries, the
//! draft-candidate linkage (`hunting → trial_pending`, candidates must
//! exist), the contradiction path parking a gap on the business with
//! its evidence cited, and the closure hook: a candidate promoted
//! through the learning gate closes every gap whose criteria name it.
//!
//! Driven in-process via `tower::ServiceExt::oneshot` (no sockets), the
//! `gaps.rs` convention; the candidate fixtures and the scripted
//! evaluator follow `learn_gate.rs` — creating, evaluating, and
//! promoting a candidate needs journaled runs, so the pipeline graph is
//! registered in every candidate-touching app here.

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Utc};
use rusty_agent_runtime::error::Result as RuntimeResult;
use rusty_agent_runtime::learn::{
    Candidate, CandidateContent, CandidateEvaluation, CandidateEvaluator, EvaluationRequest,
    EvaluationVerdict, EvidenceSpan, ReplaySummary,
};
use rusty_agent_runtime::memory::{
    MemoryKind, MemoryProvenance, MemoryRecord, MemoryScope, ProvenanceAuthor, ScopeAddress,
    ValidityWindow,
};
use rusty_agent_server::{GraphRegistry, ServerConfig, router};
use serde_json::{Value, json};
use tower::ServiceExt;

// --------------------------------------------------------------------- //
// Harness
// --------------------------------------------------------------------- //

/// Unique temp store root, removed at the end of each test (best effort).
fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!("rusty-server-hunts-test-{}", uuid::Uuid::new_v4()))
}

/// A fixed timestamp, so candidates minted here are deterministic.
fn ts(millis: i64) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp_millis(millis).unwrap()
}

/// The scripted evaluator (the `learn_gate.rs` fixture): a clean verdict
/// showing improvement — enough to clear every auto bar these tests set.
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

/// The pipeline graph (`first -> second`), so candidate lifecycle events
/// have completed runs to journal against.
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

/// Open-mode app with the pipeline graph and the scripted evaluator —
/// the full candidate lifecycle is available.
fn app() -> (Router, PathBuf) {
    let store = temp_store();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_candidate_evaluator(Arc::new(FixedEvaluator));
    (router(pipeline_registry(), config), store)
}

/// Two-tenant app for the isolation test (no candidates needed there).
fn multi_tenant_app() -> (Router, PathBuf) {
    let store = temp_store();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_tenant_key("acme", "acme-secret")
        .with_tenant_key("globex", "globex-secret");
    (router(GraphRegistry::new(), config), store)
}

const ACME: (&str, &str) = ("x-api-key", "acme-secret");
const GLOBEX: (&str, &str) = ("x-api-key", "globex-secret");

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
    let (status, v) = call(
        &app.clone(),
        "POST",
        "/threads",
        Some(json!({"graph": "pipeline"})),
    )
    .await;
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

/// A `memory_set` candidate at agent scope — the default envelope's
/// auto-promotable shape, so promotion needs no approval token.
fn memory_set_candidate(millis: i64) -> Candidate {
    let record = MemoryRecord::new(
        MemoryKind::Fact,
        ScopeAddress::new(MemoryScope::Agent, "support-1"),
        MemoryProvenance {
            author: ProvenanceAuthor::Human {
                human_id: "amjad".into(),
            },
            evidence: Default::default(),
            written_at: ts(millis - 1_000),
        },
        1.0,
        ValidityWindow::starting(ts(millis - 2_000)),
        ts(millis - 1_000),
        json!({"tone": "warm"}),
    )
    .unwrap()
    .with_key("tone");
    Candidate::new(
        CandidateContent::MemorySet {
            scope: ScopeAddress::new(MemoryScope::Agent, "support-1"),
            adds: vec![record],
            supersedes: Vec::new(),
        },
        ProvenanceAuthor::Distiller {
            name: "correction-loop".into(),
        },
        EvidenceSpan {
            run_ids: vec!["run-abc".into()],
            ..EvidenceSpan::default()
        },
        ts(millis),
    )
    .unwrap()
}

/// Create a candidate; asserts 201 and returns its id.
async fn create_candidate(app: &Router, run_id: &str, candidate: &Candidate) -> String {
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
    v["candidate_id"].as_str().unwrap().to_string()
}

/// Evaluate a candidate against a completed run; asserts 200.
async fn evaluate_candidate(app: &Router, run_id: &str, candidate_id: &str) {
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
}

/// A file-gap payload; fields merge over the defaults.
fn file_payload(overrides: Value) -> Value {
    let mut base = json!({
        "subject": {"question_shape": {"text": "vpn stability"}},
        "statement": "No runbook covers intermittent VPN drops",
        "evidence": [{"kind": "interaction_event", "id": "ie-seed"}],
        "origin": "operator",
        "closure_criteria": {"block_filled": {"block_label": "vpn-runbook"}},
    });
    let base_map = base.as_object_mut().unwrap();
    for (key, value) in overrides.as_object().unwrap() {
        base_map.insert(key.clone(), value.clone());
    }
    base
}

/// File a gap; asserts 201 and returns its id.
async fn file_gap(app: &Router, overrides: Value) -> String {
    let (status, v) = call(app, "POST", "/gaps/file", Some(file_payload(overrides))).await;
    assert_eq!(status, StatusCode::CREATED, "filing failed: {v}");
    assert_eq!(v["created"], json!(true));
    v["gap_id"].as_str().unwrap().to_string()
}

/// One entry with its chain.
async fn get_gap(app: &Router, gap_id: &str) -> Value {
    let (status, v) = call(app, "GET", &format!("/gaps/{gap_id}"), None).await;
    assert_eq!(status, StatusCode::OK, "get failed: {v}");
    v
}

/// The tenant's work-order listing.
async fn work_order(app: &Router) -> Vec<Value> {
    let (status, v) = call(app, "GET", "/gaps", None).await;
    assert_eq!(status, StatusCode::OK, "work order failed: {v}");
    v["work_order"].as_array().unwrap().clone()
}

// --------------------------------------------------------------------- //
// The cycle and its budget
// --------------------------------------------------------------------- //

#[tokio::test]
async fn the_cycle_spends_its_budget_on_the_top_of_the_work_order() {
    let (app, store) = app();

    // Three gaps, three priorities (volume * failure-cost): alpha 1000,
    // beta 500, gamma 0. The budget-2 cycle must take alpha and beta, in
    // that order, and leave gamma queued.
    let alpha = file_gap(
        &app,
        json!({
            "subject": {"question_shape": {"text": "alpha topic"}},
            "statement": "No answer for the alpha question",
            "volume": 10, "failure_cost_millis": 100,
        }),
    )
    .await;
    let beta = file_gap(
        &app,
        json!({
            "subject": {"question_shape": {"text": "beta topic"}},
            "statement": "No answer for the beta question",
            "volume": 5, "failure_cost_millis": 100,
        }),
    )
    .await;
    let gamma = file_gap(
        &app,
        json!({
            "subject": {"question_shape": {"text": "gamma topic"}},
            "statement": "No answer for the gamma question",
            "volume": 1, "failure_cost_millis": 0,
        }),
    )
    .await;

    let (status, v) = call(&app, "POST", "/hunts/cycle", Some(json!({"max_hunts": 2}))).await;
    assert_eq!(status, StatusCode::OK, "cycle failed: {v}");
    assert_eq!(v["budget"], json!(2));
    let hunts = v["cycle_hunts"].as_array().unwrap();
    assert_eq!(hunts.len(), 2, "the budget bounds the cycle exactly");
    assert_eq!(hunts[0]["gap_id"], json!(alpha), "highest priority first");
    assert_eq!(hunts[1]["gap_id"], json!(beta));
    assert_eq!(hunts[0]["status"], json!("hunting"));

    // The hunted gaps left the work order; gamma remains.
    let order = work_order(&app).await;
    assert_eq!(order.len(), 1);
    assert_eq!(order[0]["gap_id"], json!(gamma));

    // A second cycle spends the next cycle's budget on what remains.
    let (status, v) = call(&app, "POST", "/hunts/cycle", Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK, "second cycle failed: {v}");
    let hunts = v["cycle_hunts"].as_array().unwrap();
    assert_eq!(hunts.len(), 1, "the default budget is one");
    assert_eq!(hunts[0]["gap_id"], json!(gamma));

    // An empty queue hunts nothing — no work is not an error.
    let (status, v) = call(&app, "POST", "/hunts/cycle", Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK, "idle cycle failed: {v}");
    assert_eq!(v["cycle_hunts"], json!([]));

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn the_cycle_validates_and_caps_its_budget() {
    let (app, store) = app();

    let (status, _v) = call(&app, "POST", "/hunts/cycle", Some(json!({"max_hunts": 0}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "a zero budget is a 400");

    let (status, v) = call(
        &app,
        "POST",
        "/hunts/cycle",
        Some(json!({"max_hunts": 1_000_000})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "capped cycle failed: {v}");
    assert_eq!(v["budget"], json!(64), "the budget caps at 64");

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// The draft linkage
// --------------------------------------------------------------------- //

#[tokio::test]
async fn the_draft_moves_a_hunted_gap_to_trial_pending() {
    let (app, store) = app();
    let run_id = run_pipeline(&app).await;
    let candidate_id =
        create_candidate(&app, &run_id, &memory_set_candidate(1_750_000_002_000)).await;

    let gap_id = file_gap(
        &app,
        json!({
            "closure_criteria": {"artifact_promoted": {"candidate_id": candidate_id}},
        }),
    )
    .await;

    // Drafting against an open gap skips the queue: 409.
    let (status, _v) = call(
        &app,
        "POST",
        &format!("/hunts/{gap_id}/draft"),
        Some(json!({"candidate_id": candidate_id})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "an open gap cannot draft");

    let (status, v) = call(&app, "POST", "/hunts/cycle", Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK, "cycle failed: {v}");
    assert_eq!(v["cycle_hunts"][0]["gap_id"], json!(gap_id));

    // An unknown candidate is a 404 — a hunt's output is a real
    // candidate, never a name.
    let (status, _v) = call(
        &app,
        "POST",
        &format!("/hunts/{gap_id}/draft"),
        Some(json!({"candidate_id": "cand-missing"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "unknown candidate must 404");

    let (status, v) = call(
        &app,
        "POST",
        &format!("/hunts/{gap_id}/draft"),
        Some(json!({"candidate_id": candidate_id})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "draft failed: {v}");
    assert_eq!(v["entry"]["status"], json!("trial_pending"));

    // A gap already at trial cannot be re-drafted.
    let (status, _v) = call(
        &app,
        "POST",
        &format!("/hunts/{gap_id}/draft"),
        Some(json!({"candidate_id": candidate_id})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "re-drafting must conflict");

    // Unknown gaps 404.
    let (status, _v) = call(
        &app,
        "POST",
        "/hunts/gap-missing/draft",
        Some(json!({"candidate_id": candidate_id})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "unknown gap must 404");

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// The contradiction path
// --------------------------------------------------------------------- //

#[tokio::test]
async fn the_blocked_path_parks_the_gap_with_its_contradiction_cited() {
    let (app, store) = app();
    let gap_id = file_gap(&app, json!({})).await;
    let (status, _v) = call(&app, "POST", "/hunts/cycle", Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK);

    let (status, v) = call(
        &app,
        "POST",
        &format!("/hunts/{gap_id}/blocked"),
        Some(json!({
            "contradiction": "the proposed runbook contradicts the SSO rollout plan",
            "deliverable_ref": "deliverable:hunt-report-7",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "blocked failed: {v}");
    assert_eq!(v["entry"]["status"], json!("blocked_on_business"));

    // The contradiction is documented on the entry as a citation — the
    // ledger is the one place that knows why the gap moved.
    let detail = get_gap(&app, &gap_id).await;
    let evidence = detail["entry"]["evidence"].as_array().unwrap();
    assert!(
        evidence.iter().any(|citation| {
            citation["id"] == json!("deliverable:hunt-report-7")
                && citation["note"]
                    == json!("the proposed runbook contradicts the SSO rollout plan")
        }),
        "the contradiction citation landed: {evidence:?}"
    );

    // A blocked entry leaves the work order until the business reopens it.
    assert!(work_order(&app).await.is_empty());

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Closure through the promotion gate
// --------------------------------------------------------------------- //

#[tokio::test]
async fn a_promotion_closes_the_gap_that_names_the_candidate() {
    let (app, store) = app();
    let run_id = run_pipeline(&app).await;
    let candidate_id =
        create_candidate(&app, &run_id, &memory_set_candidate(1_750_000_002_000)).await;

    // The gap the hunt serves, and a second gap naming a *different*
    // candidate — the sweep must discriminate.
    let gap_id = file_gap(
        &app,
        json!({
            "subject": {"question_shape": {"text": "tone guidance"}},
            "statement": "No governed tone guidance for the support agent",
            "closure_criteria": {"artifact_promoted": {"candidate_id": candidate_id}},
        }),
    )
    .await;
    let other_gap = file_gap(
        &app,
        json!({
            "subject": {"question_shape": {"text": "escalation policy"}},
            "statement": "No escalation policy candidate exists yet",
            "closure_criteria": {"artifact_promoted": {"candidate_id": "cand-not-yet"}},
        }),
    )
    .await;

    // Hunt, draft, evaluate, promote — the full demand-side loop.
    let (status, _v) = call(&app, "POST", "/hunts/cycle", Some(json!({"max_hunts": 2}))).await;
    assert_eq!(status, StatusCode::OK);
    let (status, v) = call(
        &app,
        "POST",
        &format!("/hunts/{gap_id}/draft"),
        Some(json!({"candidate_id": candidate_id})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "draft failed: {v}");
    assert_eq!(v["entry"]["status"], json!("trial_pending"));

    evaluate_candidate(&app, &run_id, &candidate_id).await;
    let (status, v) = call(
        &app,
        "POST",
        &format!("/learn/candidates/{candidate_id}/promote"),
        Some(json!({"run_id": run_id})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "promote failed: {v}");

    // The promotion closed the gap that named the candidate — with the
    // resolution spelling out which candidate did it, attributed to the
    // gate.
    let detail = get_gap(&app, &gap_id).await;
    assert_eq!(detail["entry"]["status"], json!("closed"));
    assert_eq!(
        detail["entry"]["resolution"],
        json!(format!("candidate:{candidate_id}"))
    );
    let chain = detail["chain"].as_array().unwrap();
    assert_eq!(chain.last().unwrap()["actor"], json!("promotion-gate"));

    // The gap naming a different candidate was untouched.
    let other = get_gap(&app, &other_gap).await;
    assert_eq!(other["entry"]["status"], json!("hunting"));

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Tenant isolation
// --------------------------------------------------------------------- //

#[tokio::test]
async fn hunts_are_tenant_scoped() {
    let (app, store) = multi_tenant_app();

    let (status, v) = call_as(
        &app,
        Some(ACME),
        "POST",
        "/gaps/file",
        Some(file_payload(json!({}))),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "acme filing failed: {v}");
    let gap_id = v["gap_id"].as_str().unwrap().to_string();

    // Globex's cycle finds no work: acme's queue is not its queue.
    let (status, v) = call_as(&app, Some(GLOBEX), "POST", "/hunts/cycle", Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK, "globex cycle failed: {v}");
    assert_eq!(v["cycle_hunts"], json!([]));

    // Acme's cycle hunts its own gap; globex cannot draft against it.
    let (status, v) = call_as(&app, Some(ACME), "POST", "/hunts/cycle", Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK, "acme cycle failed: {v}");
    assert_eq!(v["cycle_hunts"][0]["gap_id"], json!(gap_id));
    let (status, _v) = call_as(
        &app,
        Some(GLOBEX),
        "POST",
        &format!("/hunts/{gap_id}/blocked"),
        Some(json!({
            "contradiction": "cross-tenant write attempt",
            "deliverable_ref": "deliverable:none",
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "cross-tenant gaps are invisible"
    );

    let _ = std::fs::remove_dir_all(store);
}
