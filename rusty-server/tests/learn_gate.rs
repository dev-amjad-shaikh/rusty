//! The learning-candidate lifecycle and promotion gate integration tests
//! (R0.8 Rusty Learn, wave 3): the `/learn/candidates` and
//! `/learn/versions` surfaces over the default JSON-file backend — the
//! envelope gate (in-envelope auto-promotion, out-of-envelope approval
//! tokens scoped per candidate), byte-exact rollback, all four lifecycle
//! events journaled with causal parentage (the wave's exit criteria),
//! canary binding, tenant isolation, and restart durability. Live-Postgres
//! coverage of the same semantics is the gated section at the bottom
//! (`RUSTY_TEST_DATABASE_URL`).
//!
//! Driven in-process via `tower::ServiceExt::oneshot` (no sockets), the
//! `corrections.rs` convention. The journaled-evidence tests need
//! completed runs' persisted journals, so the pipeline graph is
//! registered in every app here. The evaluator is scripted (`FixedEvaluator`
//! — a clean verdict, improvement past the bar): this wave proves the
//! machinery; wave 4 drives it with `rusty-eval`.

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use chrono::{DateTime, Utc};
use rusty_agent_runtime::effects::ApprovalToken;
use rusty_agent_runtime::error::Result as RuntimeResult;
use rusty_agent_runtime::learn::{
    promotion_effect_id, AutoPromotion, Candidate, CandidateContent, CandidateEvaluation,
    CandidateEvaluator, EnvelopeRule, EvaluationRequest, EvaluationVerdict, EvidenceSpan,
    PromotionEnvelope, ReplaySummary,
};
use rusty_agent_runtime::memory::{
    MemoryKind, MemoryProvenance, MemoryRecord, MemoryScope, ProvenanceAuthor, ScopeAddress,
    ValidityWindow,
};
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

// --------------------------------------------------------------------- //
// Harness
// --------------------------------------------------------------------- //

/// Unique temp store root, removed at the end of each test (best effort).
fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!(
        "rusty-server-learn-gate-test-{}",
        uuid::Uuid::new_v4()
    ))
}

/// A fixed timestamp, so candidates minted here are deterministic.
fn ts(millis: i64) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp_millis(millis).unwrap()
}

/// The scripted evaluator: a clean replay, reports in the
/// `ExperimentReport` summary shape, and a verdict showing improvement
/// (0.5 → 1.0, delta 0.5, no regression) — enough to clear every auto
/// bar the tests set. Named candidate and dataset version follow the
/// seam contract.
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

/// The pipeline graph (`first -> second`, appending to a `log` channel),
/// the flight-recorder harness's minimal pipeline: the journaled-evidence
/// tests need completed runs.
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
/// config customized by `configure` (envelope overrides, tenant keys).
fn app_with(store: PathBuf, configure: impl FnOnce(ServerConfig) -> ServerConfig) -> Router {
    let config = configure(ServerConfig::new("127.0.0.1:0".parse().unwrap(), store))
        .with_candidate_evaluator(Arc::new(FixedEvaluator));
    router(pipeline_registry(), config)
}

/// Open-mode (single `default` tenant) app over a fresh store, with the
/// default envelope.
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

/// The run's journaled events (Flight Recorder).
async fn events_of(app: &Router, run_id: &str) -> Vec<Value> {
    let (status, v) = call(app, "GET", &format!("/runs/{run_id}/events"), None).await;
    assert_eq!(status, StatusCode::OK, "events failed: {v}");
    v["events"].as_array().unwrap().clone()
}

// --------------------------------------------------------------------- //
// Candidate fixtures (built through core, serialized for the wire)
// --------------------------------------------------------------------- //

/// A `memory_set` candidate at agent scope (`support-1`) teaching the
/// `tone` fact — the R0.8 default envelope's auto-promotable shape.
fn memory_set_candidate(tone: &str, millis: i64) -> Candidate {
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
        json!({"tone": tone}),
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

/// A `prompt` candidate — approval-ruled under the R0.8 default envelope.
fn prompt_candidate() -> Candidate {
    Candidate::new(
        CandidateContent::Prompt {
            name: "system".into(),
            prompt: "You are a careful support agent. Answer tersely.".into(),
        },
        ProvenanceAuthor::Distiller {
            name: "correction-loop".into(),
        },
        EvidenceSpan::default(),
        ts(1_750_000_002_000),
    )
    .unwrap()
}

/// A `policy` candidate — the canary fixture's kind.
fn policy_candidate() -> Candidate {
    Candidate::new(
        CandidateContent::Policy {
            family: rusty_agent_runtime::record::DecisionFamily::Retry,
            parameters: json!({"max_attempts": 5}),
        },
        ProvenanceAuthor::Distiller {
            name: "correction-loop".into(),
        },
        EvidenceSpan::default(),
        ts(1_750_000_002_000),
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
    assert_eq!(v["created"], json!(true));
    let candidate_id = v["candidate_id"].as_str().unwrap().to_string();
    let (status, v) = call(
        app,
        "POST",
        &format!("/learn/candidates/{candidate_id}/evaluate"),
        Some(evaluate_payload(run_id)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "evaluate failed: {v}");
    assert_eq!(v["status"], json!("evaluated"));
    candidate_id
}

// --------------------------------------------------------------------- //
// The gate
// --------------------------------------------------------------------- //

#[tokio::test]
async fn out_of_envelope_promotion_requires_a_scoped_approval() {
    let (app, store) = app();
    let run_id = run_pipeline(&app).await;
    let candidate = prompt_candidate();
    let candidate_id = create_and_evaluate(&app, &run_id, &candidate).await;

    // The default envelope rules prompts `approval`: promoting without a
    // token is refused `403`, and the refusal names the effect id the
    // token must be scoped to.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/learn/candidates/{candidate_id}/promote"),
        Some(json!({"run_id": run_id})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "expected 403: {v}");

    // An approval minted for a *different* candidate's promotion does
    // not transfer.
    let wrong = ApprovalToken::approve(
        promotion_effect_id(&memory_set_candidate("warm", 1_750_000_002_000)),
        "ops:amjad",
    );
    let (status, v) = call(
        &app,
        "POST",
        &format!("/learn/candidates/{candidate_id}/promote"),
        Some(json!({
            "run_id": run_id,
            "approval": serde_json::to_value(&wrong).unwrap(),
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "mis-scoped approval must not transfer: {v}"
    );

    // The correctly scoped token admits, with attribution.
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
    assert_eq!(v["status"], json!("promoted"));
    assert_eq!(
        v["receipt"]["decision"]["authority"],
        json!({"authority": "approval", "approved_by": "ops:amjad"})
    );
    assert_eq!(
        v["pointer"]["active"].as_str().unwrap(),
        candidate_id,
        "the surface's pointer moved to the approved candidate"
    );

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn in_envelope_auto_promotion_needs_no_approval() {
    let (app, store) = app();
    let run_id = run_pipeline(&app).await;
    let candidate_id = create_and_evaluate(
        &app,
        &run_id,
        &memory_set_candidate("warm", 1_750_000_002_000),
    )
    .await;

    let (status, v) = call(
        &app,
        "POST",
        &format!("/learn/candidates/{candidate_id}/promote"),
        Some(json!({"run_id": run_id})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "auto promote failed: {v}");
    assert_eq!(
        v["receipt"]["decision"]["authority"],
        json!({"authority": "envelope", "envelope_version": "r0.8-default"})
    );
    assert!(
        v["receipt"]["previous"].is_null(),
        "first promotion on the surface"
    );

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn promote_then_rollback_restores_the_prior_candidate_byte_exactly() {
    let (app, store) = app();
    let run_id = run_pipeline(&app).await;

    // A serves, then B displaces it.
    let a = memory_set_candidate("warm", 1_750_000_002_000);
    let b = memory_set_candidate("colder", 1_750_000_003_000);
    assert_ne!(
        a.candidate_id, b.candidate_id,
        "distinct changes, distinct ids"
    );
    let a_id = create_and_evaluate(&app, &run_id, &a).await;
    let (status, v) = call(
        &app,
        "POST",
        &format!("/learn/candidates/{a_id}/promote"),
        Some(json!({"run_id": run_id})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "promote A failed: {v}");
    assert!(
        v["receipt"]["previous"].is_null(),
        "A is the surface's first promotion"
    );

    let b_id = create_and_evaluate(&app, &run_id, &b).await;
    let (status, v) = call(
        &app,
        "POST",
        &format!("/learn/candidates/{b_id}/promote"),
        Some(json!({"run_id": run_id})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "promote B failed: {v}");
    assert_eq!(
        v["receipt"]["previous"].as_str().unwrap(),
        a_id,
        "B's promotion names A as the displaced version"
    );
    assert_eq!(v["pointer"]["active"].as_str().unwrap(), b_id);

    // The drift monitor fires: B rolls back, the pointer re-points to A.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/learn/candidates/{b_id}/rollback"),
        Some(json!({
            "run_id": run_id,
            "cause": "drift monitor: pass-rate drop on support@v3",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "rollback failed: {v}");
    assert_eq!(v["status"], json!("rolled_back"));
    assert_eq!(v["receipt"]["from"].as_str().unwrap(), b_id);
    assert_eq!(v["receipt"]["to"].as_str().unwrap(), a_id);
    assert_eq!(v["pointer"]["active"].as_str().unwrap(), a_id);

    // Byte-exact: the restored version is the version that served. The
    // record the API serves for A now serializes identically to A's
    // stored bytes — candidates are content-addressed and immutable, so
    // rollback is a pointer move, never a reconstruction. (Both sides
    // serialize through `Value`, so the comparison is over the same
    // canonical key ordering — the bytes compared are the encoding the
    // wire and the store agree on.)
    let (status, served) = call(&app, "GET", &format!("/learn/candidates/{a_id}"), None).await;
    assert_eq!(status, StatusCode::OK, "GET A failed: {served}");
    assert_eq!(served["status"], json!("promoted"));
    let served_bytes = serde_json::to_vec(&served["candidate"]).unwrap();
    let stored_bytes = serde_json::to_vec(&serde_json::to_value(&a).unwrap()).unwrap();
    assert_eq!(
        served_bytes, stored_bytes,
        "the candidate serving after rollback is byte-identical to A"
    );

    // And the version listing shows the pointer settled back on A.
    let (status, v) = call(&app, "GET", "/learn/versions", None).await;
    assert_eq!(status, StatusCode::OK);
    let versions = v["versions"].as_array().unwrap();
    assert_eq!(versions.len(), 1);
    assert_eq!(versions[0]["active"].as_str().unwrap(), a_id);

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// The journaled lifecycle (exit criterion: every transition is in the
// journal, parented to the transition that caused it)
// --------------------------------------------------------------------- //

#[tokio::test]
async fn the_lifecycle_journals_all_four_events_with_causal_parentage() {
    let (app, store) = app();
    let run_id = run_pipeline(&app).await;
    let candidate = memory_set_candidate("warm", 1_750_000_002_000);
    let candidate_id = create_and_evaluate(&app, &run_id, &candidate).await;
    for (action, extra) in [
        ("promote", json!({})),
        ("rollback", json!({"cause": "drift monitor"})),
    ] {
        let mut payload = json!({"run_id": run_id});
        payload
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let (status, v) = call(
            &app,
            "POST",
            &format!("/learn/candidates/{candidate_id}/{action}"),
            Some(payload),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{action} failed: {v}");
    }

    let events = events_of(&app, &run_id).await;
    let lifecycle: Vec<&Value> = events
        .iter()
        .filter(|event| {
            event["kind"]
                .as_str()
                .is_some_and(|kind| kind.starts_with("candidate_"))
        })
        .collect();
    assert_eq!(
        lifecycle
            .iter()
            .map(|event| event["kind"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec![
            "candidate_created",
            "candidate_evaluated",
            "candidate_promoted",
            "candidate_rolled_back",
        ],
        "all four transitions are in the journal, in order"
    );
    // Causal parentage: each transition hangs off the one before it (the
    // run completed before the lifecycle began, so nothing interleaves).
    for pair in lifecycle.windows(2) {
        assert_eq!(
            pair[1]["parent"].as_str().unwrap(),
            pair[0]["id"].as_str().unwrap(),
            "{} must parent to {}",
            pair[1]["kind"],
            pair[0]["kind"]
        );
    }
    // The evidence is self-contained: the created event carries the
    // candidate, the evaluated event the evaluation, the transitions
    // their receipts — the journal alone reconstructs the lifecycle.
    // (Payloads travel as `PayloadRef`; inline values sit under `value`.)
    assert_eq!(
        lifecycle[0]["output"]["value"]["candidate_id"]
            .as_str()
            .unwrap(),
        candidate_id
    );
    assert_eq!(
        lifecycle[1]["output"]["value"]["candidate_id"]
            .as_str()
            .unwrap(),
        candidate_id
    );
    assert_eq!(
        lifecycle[2]["output"]["value"]["candidate_id"]
            .as_str()
            .unwrap(),
        candidate_id
    );
    assert_eq!(
        lifecycle[3]["output"]["value"]["from"].as_str().unwrap(),
        candidate_id
    );

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Canary binding
// --------------------------------------------------------------------- //

#[tokio::test]
async fn canary_promotion_binds_the_pointer_without_moving_active() {
    let store = temp_store();
    // The deployment declares: policy candidates auto-promote into a 25%
    // canary on cleared evidence.
    let app = app_with(store.clone(), |config| {
        config.with_promotion_envelope(PromotionEnvelope {
            envelope_version: "acme-canary-1".into(),
            prompt: EnvelopeRule::Approval,
            policy: EnvelopeRule::Canary {
                fraction: 0.25,
                auto: AutoPromotion {
                    dataset_version: None,
                    min_improvement: 0.0,
                    scopes: Vec::new(),
                },
            },
            memory_set: EnvelopeRule::Approval,
            tool_permission: EnvelopeRule::Approval,
            // The R0.11 registry kinds keep their approval default.
            ..PromotionEnvelope::r08_default()
        })
    });
    let run_id = run_pipeline(&app).await;
    let candidate_id = create_and_evaluate(&app, &run_id, &policy_candidate()).await;

    let (status, v) = call(
        &app,
        "POST",
        &format!("/learn/candidates/{candidate_id}/promote"),
        Some(json!({"run_id": run_id})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "canary promote failed: {v}");
    assert_eq!(
        v["receipt"]["decision"]["authority"],
        json!({"authority": "envelope", "envelope_version": "acme-canary-1"})
    );
    assert_eq!(
        v["receipt"]["decision"]["canary"],
        json!({"candidate_id": candidate_id, "fraction": 0.25})
    );
    // The pointer binds the canary; full traffic keeps serving the
    // static version (active stays unset).
    assert!(v["pointer"]["active"].is_null());
    assert_eq!(
        v["pointer"]["canary"],
        json!({"candidate_id": candidate_id, "fraction": 0.25})
    );

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Refusals and isolation
// --------------------------------------------------------------------- //

#[tokio::test]
async fn evaluate_without_an_evaluator_configured_answers_409() {
    // A deployment with no evaluator can hold and inspect candidates but
    // cannot produce evidence — the 409 says exactly that.
    let store = temp_store();
    let app = {
        let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone());
        router(pipeline_registry(), config)
    };
    let run_id = run_pipeline(&app).await;
    let candidate = memory_set_candidate("warm", 1_750_000_002_000);
    let (status, v) = call(
        &app,
        "POST",
        "/learn/candidates",
        Some(json!({
            "candidate": serde_json::to_value(&candidate).unwrap(),
            "run_id": run_id,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create failed: {v}");
    let candidate_id = v["candidate_id"].as_str().unwrap().to_string();

    let (status, v) = call(
        &app,
        "POST",
        &format!("/learn/candidates/{candidate_id}/evaluate"),
        Some(evaluate_payload(&run_id)),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "expected 409: {v}");

    // And promotion is evidence-gated even with an evaluator-shaped
    // request: an unevaluated candidate cannot promote.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/learn/candidates/{candidate_id}/promote"),
        Some(json!({"run_id": run_id})),
    )
    .await;
    assert!(
        status == StatusCode::UNPROCESSABLE_ENTITY || status == StatusCode::CONFLICT,
        "expected 422/409 for an unevaluated candidate: {status} {v}"
    );

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn lifecycle_payloads_require_a_resolvable_run() {
    let (app, store) = app();
    let candidate = memory_set_candidate("warm", 1_750_000_002_000);
    // The journaling gate is hard-fail: a run that does not resolve
    // stops the transition before anything reaches the store.
    let (status, v) = call(
        &app,
        "POST",
        "/learn/candidates",
        Some(json!({
            "candidate": serde_json::to_value(&candidate).unwrap(),
            "run_id": "run-does-not-exist",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "expected 404: {v}");
    let candidate_id = candidate.candidate_id.to_string();
    let (status, _) = call(
        &app,
        "GET",
        &format!("/learn/candidates/{candidate_id}"),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a transition the journal refused must not reach the store"
    );

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn candidates_and_pointers_are_tenant_isolated() {
    let store = temp_store();
    let app = app_with(store.clone(), |config| {
        config
            .with_tenant_key("acme", "acme-secret")
            .with_tenant_key("globex", "globex-secret")
    });
    let acme = Some(("x-api-key", "acme-secret"));
    let globex = Some(("x-api-key", "globex-secret"));

    // A completed run under acme (threads/journals are tenant-scoped).
    let (status, v) = call_as(
        &app,
        acme,
        "POST",
        "/threads",
        Some(json!({"graph": "pipeline"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "acme thread failed: {v}");
    let thread_id = v["thread_id"].as_str().unwrap().to_string();
    let (status, v) = call_as(
        &app,
        acme,
        "POST",
        &format!("/threads/{thread_id}/runs/wait"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "acme run failed: {v}");
    let run_id = v["run_id"].as_str().unwrap().to_string();

    let candidate = memory_set_candidate("warm", 1_750_000_002_000);
    let (status, v) = call_as(
        &app,
        acme,
        "POST",
        "/learn/candidates",
        Some(json!({
            "candidate": serde_json::to_value(&candidate).unwrap(),
            "run_id": run_id,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "acme create failed: {v}");
    let candidate_id = v["candidate_id"].as_str().unwrap().to_string();

    // Acme sees it; globex does not — reads, transitions, and listings
    // alike (cross-tenant ids are indistinguishable from unknown ones).
    let (status, _) = call_as(
        &app,
        acme,
        "GET",
        &format!("/learn/candidates/{candidate_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    for (method, uri, body) in [
        ("GET", format!("/learn/candidates/{candidate_id}"), None),
        (
            "POST",
            format!("/learn/candidates/{candidate_id}/evaluate"),
            Some(json!({
                "request": {
                    "dataset_version": "support-v3",
                    "target_metric": "run_pass_rate",
                    "thresholds": {"max_pass_rate_drop": 0.05, "max_latency_p95_ratio": 1.25},
                    "replay_evidence": [],
                },
                "run_id": run_id,
            })),
        ),
        (
            "POST",
            format!("/learn/candidates/{candidate_id}/promote"),
            Some(json!({"run_id": run_id})),
        ),
        (
            "POST",
            format!("/learn/candidates/{candidate_id}/rollback"),
            Some(json!({"run_id": run_id, "cause": "cross-tenant probe"})),
        ),
    ] {
        let (status, v) = call_as(&app, globex, method, &uri, body).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "globex must not reach acme's candidate via {method} {uri}: {v}"
        );
    }
    let (status, v) = call_as(&app, globex, "GET", "/learn/candidates", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["candidates"].as_array().unwrap().len(), 0);
    let (status, v) = call_as(&app, globex, "GET", "/learn/versions", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["versions"].as_array().unwrap().len(), 0);

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Restart durability (file backend)
// --------------------------------------------------------------------- //

#[tokio::test]
async fn candidates_and_pointers_survive_a_restart() {
    let store = temp_store();
    let first = app_with(store.clone(), |config| config);
    let run_id = run_pipeline(&first).await;
    let candidate = memory_set_candidate("warm", 1_750_000_002_000);
    let candidate_id = create_and_evaluate(&first, &run_id, &candidate).await;
    let (status, v) = call(
        &first,
        "POST",
        &format!("/learn/candidates/{candidate_id}/promote"),
        Some(json!({"run_id": run_id})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "promote failed: {v}");
    let receipt = v["receipt"].clone();
    drop(first);

    // A fresh app over the same store root serves the same lifecycle
    // state — the file layout is the durability story.
    let second = app_with(store.clone(), |config| config);
    let (status, served) = call(
        &second,
        "GET",
        &format!("/learn/candidates/{candidate_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "GET after restart failed: {served}");
    assert_eq!(served["status"], json!("promoted"));
    assert_eq!(served["promotion"], receipt);
    let (status, v) = call(&second, "GET", "/learn/versions", None).await;
    assert_eq!(status, StatusCode::OK);
    let versions = v["versions"].as_array().unwrap();
    assert_eq!(versions.len(), 1);
    assert_eq!(versions[0]["active"].as_str().unwrap(), candidate_id);

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

    /// Wave-3 exit criteria on Postgres: the full lifecycle — create,
    /// evaluate, promote, roll back — through the transactional
    /// transition path, surviving a restart.
    #[tokio::test]
    async fn postgres_candidate_lifecycle_survives_a_restart() {
        let Some(url) = pg_url() else {
            eprintln!("RUSTY_TEST_DATABASE_URL unset; skipping");
            return;
        };
        let tenant = format!("learnpg-{}", uuid::Uuid::new_v4());
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
        let run_id = v["run_id"].as_str().unwrap().to_string();

        let candidate = memory_set_candidate("warm", 1_750_000_002_000);
        let (status, v) = call_as(
            &first,
            auth,
            "POST",
            "/learn/candidates",
            Some(json!({
                "candidate": serde_json::to_value(&candidate).unwrap(),
                "run_id": run_id,
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "pg create failed: {v}");
        let candidate_id = v["candidate_id"].as_str().unwrap().to_string();
        let (status, v) = call_as(
            &first,
            auth,
            "POST",
            &format!("/learn/candidates/{candidate_id}/evaluate"),
            Some(evaluate_payload(&run_id)),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "pg evaluate failed: {v}");
        let (status, v) = call_as(
            &first,
            auth,
            "POST",
            &format!("/learn/candidates/{candidate_id}/promote"),
            Some(json!({"run_id": run_id})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "pg promote failed: {v}");
        assert_eq!(v["pointer"]["active"].as_str().unwrap(), candidate_id);
        let receipt = v["receipt"].clone();

        let (status, v) = call_as(
            &first,
            auth,
            "POST",
            &format!("/learn/candidates/{candidate_id}/rollback"),
            Some(json!({
                "run_id": run_id,
                "cause": "drift monitor: pass-rate drop",
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "pg rollback failed: {v}");
        assert!(
            v["pointer"]["active"].is_null(),
            "A was the first promotion: rollback re-points to the static version"
        );
        drop(first);

        // A fresh app over the same database serves the settled state.
        let second = build();
        let (status, served) = call_as(
            &second,
            auth,
            "GET",
            &format!("/learn/candidates/{candidate_id}"),
            None,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "pg GET after restart failed: {served}"
        );
        assert_eq!(served["status"], json!("rolled_back"));
        assert_eq!(served["promotion"], receipt);
        assert_eq!(
            serde_json::to_vec(&served["candidate"]).unwrap(),
            serde_json::to_vec(&serde_json::to_value(&candidate).unwrap()).unwrap(),
            "the served candidate is byte-identical after the restart"
        );
        let (status, v) = call_as(&second, auth, "GET", "/learn/versions", None).await;
        assert_eq!(status, StatusCode::OK);
        let versions = v["versions"].as_array().unwrap();
        assert_eq!(versions.len(), 1);
        assert!(versions[0]["active"].is_null());
    }
}
