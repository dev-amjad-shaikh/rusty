//! The registry admission-resolution integration tests (R0.11 Extension
//! Plane, wave 2): a run declares the named configuration artifacts it
//! uses (and the environment it targets) at submission; at admission each
//! artifact resolves through its environment-tagged version pointer — the
//! active version, or the canary when the run's seeded draw admits — the
//! resolved content pins the run manifest through the R0.7 pin functions,
//! and one `config_resolved` event per artifact joins the journal ahead
//! of the run's own events. The exit criteria live here:
//!
//! - a promotion without a redeploy rebinds the *next* run;
//! - a run admitted before the promotion keeps its pins (its journal and
//!   receipt still name the exact candidate it used);
//! - rollback re-points byte-exactly — a post-rollback run resolves the
//!   same candidate id and digest the pre-promotion run did;
//! - the evidence walks end to end: receipt → manifest pin → resolution
//!   event → candidate → commit → author, with the resolution event under
//!   the receipt's signature (tampering with it fails verification);
//! - an unbound run is the pre-R0.11 behavior, byte-identically.
//!
//! Driven in-process via `tower::ServiceExt::oneshot` (no sockets), the
//! `registry.rs` harness convention — including its scripted evaluator:
//! promotion is scenery; admission is the proof.

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
    ContextBudget, MemoryQuery, ProvenanceAuthor, MEMORY_SCHEMA_VERSION,
};
use rusty_agent_runtime::record::sha256_hex;
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

// --------------------------------------------------------------------- //
// Harness (the registry.rs shapes, verbatim where the semantics match)
// --------------------------------------------------------------------- //

/// Unique temp store root, removed at the end of each test (best effort).
fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!(
        "rusty-server-admission-test-{}",
        uuid::Uuid::new_v4()
    ))
}

/// A fixed timestamp, so candidates minted here are deterministic.
fn ts(millis: i64) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp_millis(millis).unwrap()
}

/// The scripted evaluator: a clean replay and an improving verdict —
/// enough to clear the approval *and* the auto/canary bars. This wave
/// proves admission; the evaluator is scenery.
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
/// config customized by `configure` (tenant keys, the default tag, the
/// promotion envelope).
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

/// The registry binding payload shape every bound run submits.
fn binding(environment: Option<&str>, artifacts: Value) -> Value {
    match environment {
        Some(tag) => json!({"environment": tag, "artifacts": artifacts}),
        None => json!({"artifacts": artifacts}),
    }
}

/// Create a thread and submit a registry-bound run; returns
/// `(status, body)` — the admission-failures test asserts the error, so
/// nothing asserts here.
async fn submit_bound(app: &Router, registry: Value) -> (StatusCode, Value) {
    submit_bound_as(app, None, registry).await
}

/// The auth-carrying form of [`submit_bound`].
async fn submit_bound_as(
    app: &Router,
    auth: Option<(&str, &str)>,
    registry: Value,
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
        Some(json!({"registry": registry})),
    )
    .await
}

/// Create a thread and run it to completion under a registry binding;
/// asserts admission and completion, returns the run id.
async fn run_pipeline_bound(app: &Router, registry: Value) -> String {
    let (status, v) = submit_bound(app, registry).await;
    assert_eq!(status, StatusCode::OK, "bound run failed: {v}");
    v["run_id"].as_str().unwrap().to_string()
}

/// Export the run's journal snapshot (via the portable fixture).
async fn snapshot_of(app: &Router, run_id: &str) -> Value {
    let (status, v) = call(app, "GET", &format!("/runs/{run_id}/fixture"), None).await;
    assert_eq!(status, StatusCode::OK, "fixture failed: {v}");
    v["journal"].clone()
}

/// Mint (or serve) the run's receipt; asserts 200.
async fn receipt_of(app: &Router, run_id: &str) -> Value {
    let (status, v) = call(app, "GET", &format!("/runs/{run_id}/receipt"), None).await;
    assert_eq!(status, StatusCode::OK, "receipt failed: {v}");
    v
}

/// The journaled `config_resolved` outputs, in journal order.
fn resolutions(snapshot: &Value) -> Vec<Value> {
    snapshot["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["kind"] == json!("config_resolved"))
        .map(|event| event["output"]["value"].clone())
        .collect()
}

// --------------------------------------------------------------------- //
// Fixtures
// --------------------------------------------------------------------- //

fn owner() -> ProvenanceAuthor {
    ProvenanceAuthor::Human {
        human_id: "amjad".into(),
    }
}

/// A distiller-authored `prompt` candidate (`prompt:{name}`) —
/// approval-ruled under the R0.8 default envelope.
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

/// An operator-authored `prompt` candidate — the human provenance the
/// receipt walk ends at (the registry's authored-artifact discipline).
fn operator_prompt_candidate(name: &str, text: &str, millis: i64) -> Candidate {
    Candidate::new(
        CandidateContent::Prompt {
            name: name.into(),
            prompt: text.into(),
        },
        owner(),
        EvidenceSpan::default(),
        ts(millis),
    )
    .unwrap()
}

/// A `model_settings` candidate — the family whose resolution also pins
/// the manifest's `model` slot.
fn model_settings_candidate(name: &str, temperature: f64, millis: i64) -> Candidate {
    Candidate::new(
        CandidateContent::ModelSettings {
            name: name.into(),
            model: "gpt-4o".into(),
            parameters: json!({"temperature": temperature, "max_tokens": 512}),
        },
        ProvenanceAuthor::Distiller {
            name: "correction-loop".into(),
        },
        EvidenceSpan::default(),
        ts(millis),
    )
    .unwrap()
}

/// A `memory_configuration` candidate — a family with no manifest digest
/// slot in this wave, so its resolution must be refused (never faked).
fn memory_configuration_candidate(name: &str, millis: i64) -> Candidate {
    Candidate::new(
        CandidateContent::MemoryConfiguration {
            name: name.into(),
            budget: ContextBudget::new(4096),
            default_filters: MemoryQuery::default(),
            schema_version: MEMORY_SCHEMA_VERSION.to_owned(),
            rank: None,
            maintenance: Vec::new(),
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

/// Declare an artifact; returns `(status, body)`.
async fn declare(app: &Router, family: &str, name: &str) -> (StatusCode, Value) {
    call(
        app,
        "POST",
        "/registry/artifacts",
        Some(json!({
            "family": family,
            "name": name,
            "owner": serde_json::to_value(owner()).unwrap(),
        })),
    )
    .await
}

/// Commit a candidate to an already-declared artifact; asserts 200.
async fn commit(app: &Router, family: &str, name: &str, candidate_id: &str) {
    let (status, v) = call(
        app,
        "POST",
        &format!("/registry/artifacts/{family}/{name}/commits"),
        Some(json!({"candidate_id": candidate_id})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "commit failed: {v}");
    assert_eq!(v["committed"], json!(true));
}

/// Declare `{family}:{name}` and commit each candidate id in order.
async fn declare_and_commit(app: &Router, family: &str, name: &str, candidate_ids: &[String]) {
    let (status, v) = declare(app, family, name).await;
    assert_eq!(status, StatusCode::CREATED, "declare failed: {v}");
    for candidate_id in candidate_ids {
        commit(app, family, name, candidate_id).await;
    }
}

/// Promote `candidate` to `tag` (approval-ruled and canary-ruled alike —
/// the token is ignored inside the envelope); asserts 200 and returns
/// the response body.
async fn promote(
    app: &Router,
    run_id: &str,
    candidate: &Candidate,
    candidate_id: &str,
    tag: Option<&str>,
) -> Value {
    promote_as(app, None, run_id, candidate, candidate_id, tag).await
}

/// The auth-carrying form of [`promote`].
async fn promote_as(
    app: &Router,
    auth: Option<(&str, &str)>,
    run_id: &str,
    candidate: &Candidate,
    candidate_id: &str,
    tag: Option<&str>,
) -> Value {
    let mut body = json!({
        "run_id": run_id,
        "approval": serde_json::to_value(ApprovalToken::approve(
            promotion_effect_id(candidate),
            "ops:amjad",
        ))
        .unwrap(),
    });
    if let Some(tag) = tag {
        body["tag"] = json!(tag);
    }
    let (status, v) = call_as(
        app,
        auth,
        "POST",
        &format!("/learn/candidates/{candidate_id}/promote"),
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "promote failed: {v}");
    v
}

// --------------------------------------------------------------------- //
// Exit criterion: promote without a redeploy — the *next* run rebinds
// --------------------------------------------------------------------- //

#[tokio::test]
async fn promotion_without_redeploy_rebinds_the_next_run() {
    let (app, store) = app();
    let journal_run = run_pipeline(&app).await;
    let v1 = prompt_candidate("system", "You are terse.", 1_750_000_002_000);
    let id1 = create_and_evaluate(&app, &journal_run, &v1).await;
    declare_and_commit(&app, "prompt", "system", std::slice::from_ref(&id1)).await;
    promote(&app, &journal_run, &v1, &id1, Some("prod")).await;

    // The first bound run resolves v1 through the prod pointer and pins
    // its content: the journaled digest is the manifest pin is the
    // prompt's content address — one derivation, three addresses.
    let prompt_binding = || {
        binding(
            Some("prod"),
            json!([{"family": "prompt", "name": "system"}]),
        )
    };
    let run_a = run_pipeline_bound(&app, prompt_binding()).await;
    let snapshot_a = snapshot_of(&app, &run_a).await;
    let resolved_a = resolutions(&snapshot_a);
    assert_eq!(
        resolved_a.len(),
        1,
        "one artifact resolves one event: {resolved_a:?}"
    );
    assert_eq!(resolved_a[0]["surface"], json!("prompt:system"));
    assert_eq!(resolved_a[0]["tag"], json!("prod"));
    assert_eq!(resolved_a[0]["candidate_id"], json!(id1));
    assert_eq!(resolved_a[0]["pointer"], json!("active"));
    let digest_v1 = sha256_hex("You are terse.".as_bytes());
    assert_eq!(resolved_a[0]["digest"], json!(digest_v1));
    let receipt_a = receipt_of(&app, &run_a).await;
    assert_eq!(
        receipt_a["manifest"]["prompts"]["system"],
        json!(digest_v1),
        "the manifest pin is the journaled resolution digest"
    );

    // v2 commits and promotes — the same deployment, no redeploy, no
    // restart. The next bound run resolves v2; the first run's evidence
    // is untouched (the next test proves that in full).
    let v2 = prompt_candidate("system", "You are warm and thorough.", 1_750_000_003_000);
    let id2 = create_and_evaluate(&app, &journal_run, &v2).await;
    commit(&app, "prompt", "system", &id2).await;
    promote(&app, &journal_run, &v2, &id2, Some("prod")).await;

    let run_b = run_pipeline_bound(&app, prompt_binding()).await;
    let resolved_b = resolutions(&snapshot_of(&app, &run_b).await);
    assert_eq!(resolved_b.len(), 1);
    assert_eq!(resolved_b[0]["candidate_id"], json!(id2));
    let digest_v2 = sha256_hex("You are warm and thorough.".as_bytes());
    assert_eq!(resolved_b[0]["digest"], json!(digest_v2));
    assert_ne!(digest_v1, digest_v2, "the versions are distinct content");
    let receipt_b = receipt_of(&app, &run_b).await;
    assert_eq!(receipt_b["manifest"]["prompts"]["system"], json!(digest_v2));

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Conservatism: an admitted run keeps its pins when the pointer moves
// --------------------------------------------------------------------- //

#[tokio::test]
async fn an_admitted_run_keeps_its_pins_when_the_pointer_moves() {
    let (app, store) = app();
    let journal_run = run_pipeline(&app).await;
    let v1 = prompt_candidate("system", "You are terse.", 1_750_000_002_000);
    let id1 = create_and_evaluate(&app, &journal_run, &v1).await;
    declare_and_commit(&app, "prompt", "system", std::slice::from_ref(&id1)).await;
    promote(&app, &journal_run, &v1, &id1, Some("prod")).await;

    let run_a = run_pipeline_bound(
        &app,
        binding(
            Some("prod"),
            json!([{"family": "prompt", "name": "system"}]),
        ),
    )
    .await;

    // The pointer moves *after* run A admitted.
    let v2 = prompt_candidate("system", "You are warm and thorough.", 1_750_000_003_000);
    let id2 = create_and_evaluate(&app, &journal_run, &v2).await;
    commit(&app, "prompt", "system", &id2).await;
    promote(&app, &journal_run, &v2, &id2, Some("prod")).await;

    // Run A's journal still reports the exact candidate it used — the
    // admission events are immutable evidence, not a live view of the
    // pointer.
    let snapshot_a = snapshot_of(&app, &run_a).await;
    let resolved_a = resolutions(&snapshot_a);
    assert_eq!(
        resolved_a.len(),
        1,
        "a later promotion appends nothing to an admitted run"
    );
    assert_eq!(resolved_a[0]["candidate_id"], json!(id1));
    let digest_v1 = sha256_hex("You are terse.".as_bytes());
    assert_eq!(resolved_a[0]["digest"], json!(digest_v1));

    // And its receipt still covers v1's pin — and still verifies against
    // the exported journal.
    let receipt_a = receipt_of(&app, &run_a).await;
    assert_eq!(receipt_a["manifest"]["prompts"]["system"], json!(digest_v1));
    let (status, v) = call(
        &app,
        "POST",
        "/receipts/verify",
        Some(json!({"snapshot": snapshot_a, "receipt": receipt_a})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the admitted run's receipt verifies after the pointer moved: {v}"
    );

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Canary composition: the tagged admission draws against the pointer's
// canary binding, seeded by run id
// --------------------------------------------------------------------- //

#[tokio::test]
async fn canary_admission_composes_the_tagged_binding_with_the_seeded_draw() {
    let store = temp_store();

    // First app (default envelope): v1 promotes to prod full-traffic.
    let first = app_with(store.clone(), |config| config);
    let journal_run = run_pipeline(&first).await;
    let v1 = prompt_candidate("system", "You are terse.", 1_750_000_002_000);
    let id1 = create_and_evaluate(&first, &journal_run, &v1).await;
    declare_and_commit(&first, "prompt", "system", std::slice::from_ref(&id1)).await;
    promote(&first, &journal_run, &v1, &id1, Some("prod")).await;
    drop(first);

    let prompt_binding = || {
        binding(
            Some("prod"),
            json!([{"family": "prompt", "name": "system"}]),
        )
    };
    let canary_envelope = |fraction: f64| PromotionEnvelope {
        prompt: EnvelopeRule::Canary {
            fraction,
            auto: AutoPromotion {
                dataset_version: None,
                min_improvement: 0.0,
                scopes: Vec::new(),
            },
        },
        ..PromotionEnvelope::r08_default()
    };

    // Second app (canary envelope, fraction 1.0): v2 binds the canary
    // slot — and every run's draw admits, so the next run resolves the
    // canary, journaled as `pointer: "canary"`.
    let second = app_with(store.clone(), |config| {
        config.with_promotion_envelope(canary_envelope(1.0))
    });
    let journal_run2 = run_pipeline(&second).await;
    let v2 = prompt_candidate("system", "You are experimental.", 1_750_000_003_000);
    let id2 = create_and_evaluate(&second, &journal_run2, &v2).await;
    commit(&second, "prompt", "system", &id2).await;
    let promoted = promote(&second, &journal_run2, &v2, &id2, Some("prod")).await;
    assert_eq!(
        promoted["pointer"]["canary"]["candidate_id"],
        json!(id2),
        "a canary promotion binds the slot, leaving active untouched: {promoted}"
    );
    assert_eq!(promoted["pointer"]["active"], json!(id1));

    let run_canary = run_pipeline_bound(&second, prompt_binding()).await;
    let resolved = resolutions(&snapshot_of(&second, &run_canary).await);
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0]["candidate_id"], json!(id2));
    assert_eq!(resolved[0]["pointer"], json!("canary"));
    assert_eq!(
        resolved[0]["digest"],
        json!(sha256_hex("You are experimental.".as_bytes()))
    );
    drop(second);

    // Third app (canary envelope, a draw no run wins): v3 takes the
    // canary slot, but the seeded draw never admits it, so the run
    // resolves the *active* version — the canary steers admission
    // without ever touching it forcibly.
    let third = app_with(store.clone(), |config| {
        config.with_promotion_envelope(canary_envelope(f64::MIN_POSITIVE))
    });
    let journal_run3 = run_pipeline(&third).await;
    let v3 = prompt_candidate("system", "You are a rumor.", 1_750_000_004_000);
    let id3 = create_and_evaluate(&third, &journal_run3, &v3).await;
    commit(&third, "prompt", "system", &id3).await;
    let promoted = promote(&third, &journal_run3, &v3, &id3, Some("prod")).await;
    assert_eq!(promoted["pointer"]["canary"]["candidate_id"], json!(id3));

    let run_active = run_pipeline_bound(&third, prompt_binding()).await;
    let resolved = resolutions(&snapshot_of(&third, &run_active).await);
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0]["candidate_id"], json!(id1));
    assert_eq!(resolved[0]["pointer"], json!("active"));
    assert_eq!(
        resolved[0]["digest"],
        json!(sha256_hex("You are terse.".as_bytes()))
    );

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Exit criterion: rollback re-points byte-exactly
// --------------------------------------------------------------------- //

#[tokio::test]
async fn rollback_restores_the_prior_version_byte_exactly() {
    let (app, store) = app();
    let journal_run = run_pipeline(&app).await;
    let v1 = prompt_candidate("system", "You are terse.", 1_750_000_002_000);
    let id1 = create_and_evaluate(&app, &journal_run, &v1).await;
    declare_and_commit(&app, "prompt", "system", std::slice::from_ref(&id1)).await;
    promote(&app, &journal_run, &v1, &id1, Some("prod")).await;

    let prompt_binding = || {
        binding(
            Some("prod"),
            json!([{"family": "prompt", "name": "system"}]),
        )
    };
    let run_before = run_pipeline_bound(&app, prompt_binding()).await;
    let resolved_before = resolutions(&snapshot_of(&app, &run_before).await);
    assert_eq!(resolved_before[0]["candidate_id"], json!(id1));

    // v2 promotes; a bound run resolves it.
    let v2 = prompt_candidate("system", "You are warm and thorough.", 1_750_000_003_000);
    let id2 = create_and_evaluate(&app, &journal_run, &v2).await;
    commit(&app, "prompt", "system", &id2).await;
    promote(&app, &journal_run, &v2, &id2, Some("prod")).await;
    let run_v2 = run_pipeline_bound(&app, prompt_binding()).await;
    let resolved_v2 = resolutions(&snapshot_of(&app, &run_v2).await);
    assert_eq!(resolved_v2[0]["candidate_id"], json!(id2));

    // Rolling v2 back re-points prod at v1 — the pointer's `previous`,
    // not a reconstruction.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/learn/candidates/{id2}/rollback"),
        Some(json!({
            "run_id": journal_run,
            "cause": "prod soak regressed the tone metric",
            "tag": "prod",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "rollback failed: {v}");
    assert_eq!(v["receipt"]["to"], json!(id1));
    assert_eq!(v["pointer"]["active"], json!(id1));

    // The post-rollback run resolves exactly what the pre-promotion run
    // did: same candidate id, same digest, same manifest pin — the
    // byte-exactness the exit criterion names.
    let run_after = run_pipeline_bound(&app, prompt_binding()).await;
    let resolved_after = resolutions(&snapshot_of(&app, &run_after).await);
    assert_eq!(resolved_after.len(), 1);
    assert_eq!(
        resolved_after[0]["candidate_id"],
        resolved_before[0]["candidate_id"]
    );
    assert_eq!(resolved_after[0]["digest"], resolved_before[0]["digest"]);
    let receipt_before = receipt_of(&app, &run_before).await;
    let receipt_after = receipt_of(&app, &run_after).await;
    assert_eq!(
        receipt_before["manifest"]["prompts"]["system"],
        receipt_after["manifest"]["prompts"]["system"],
        "the manifest pin is byte-identical across the rollback"
    );
    assert_eq!(
        receipt_before["manifest_digest"], receipt_after["manifest_digest"],
        "and so is the manifest's own digest"
    );

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Exit criterion: the receipt walk — receipt → manifest pin → resolution
// event → candidate → commit → author
// --------------------------------------------------------------------- //

#[tokio::test]
async fn the_receipt_walks_from_pin_to_resolution_to_candidate_to_author() {
    let (app, store) = app();
    let journal_run = run_pipeline(&app).await;
    // The operator-authored discipline: the registry versions what a
    // named human wrote, promoted under a named approver's token.
    let candidate = operator_prompt_candidate("system", "You are terse.", 1_750_000_002_000);
    let candidate_id = create_and_evaluate(&app, &journal_run, &candidate).await;
    declare_and_commit(
        &app,
        "prompt",
        "system",
        std::slice::from_ref(&candidate_id),
    )
    .await;
    promote(&app, &journal_run, &candidate, &candidate_id, Some("prod")).await;

    let run_id = run_pipeline_bound(
        &app,
        binding(
            Some("prod"),
            json!([{"family": "prompt", "name": "system"}]),
        ),
    )
    .await;
    let snapshot = snapshot_of(&app, &run_id).await;
    let receipt = receipt_of(&app, &run_id).await;

    // 1. The receipt's manifest pin is the prompt's content address…
    let digest = sha256_hex("You are terse.".as_bytes());
    assert_eq!(receipt["manifest"]["prompts"]["system"], json!(digest));

    // 2. …and the journaled resolution names it: exactly one
    // `config_resolved` event, carrying the surface, the environment, the
    // candidate, the pointer slot, and the same digest.
    let resolved = resolutions(&snapshot);
    assert_eq!(resolved.len(), 1, "one artifact, one resolution event");
    assert_eq!(resolved[0]["surface"], json!("prompt:system"));
    assert_eq!(resolved[0]["tag"], json!("prod"));
    assert_eq!(resolved[0]["candidate_id"], json!(candidate_id));
    assert_eq!(resolved[0]["pointer"], json!("active"));
    assert_eq!(resolved[0]["digest"], json!(digest));

    // 3. The event sits inside the signed head the receipt attests.
    let event = snapshot["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == json!("config_resolved"))
        .unwrap();
    assert!(
        event["seq"].as_u64().unwrap() < receipt["journal_head"]["events"].as_u64().unwrap(),
        "the resolution event is covered by the receipt's signed journal head"
    );

    // 4. The receipt verifies against the exported journal — the walk's
    //    signature link.
    let (status, v) = call(
        &app,
        "POST",
        "/receipts/verify",
        Some(json!({"snapshot": snapshot, "receipt": receipt})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "verify failed: {v}");

    // 5. The candidate the resolution names is the operator's: human
    //    provenance, a journaled evaluation, and the promotion receipt
    //    naming its approver.
    let (status, record) = call(
        &app,
        "GET",
        &format!("/learn/candidates/{candidate_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "candidate read failed: {record}");
    assert_eq!(
        record["candidate"]["distilled_by"],
        json!({"type": "human", "human_id": "amjad"}),
        "the walk ends at the operator who authored the candidate"
    );
    assert!(
        record["evaluation"].is_object(),
        "the promoted candidate carries its journaled evaluation"
    );
    assert_eq!(
        record["promotion"]["decision"]["authority"],
        json!({"authority": "approval", "approved_by": "ops:amjad"}),
        "the promotion names its approver"
    );
    assert_eq!(record["promotion"]["surface"], json!("prompt:system@prod"));

    // 6. The artifact's commit history joins the candidate to its
    //    author — the same lineage, one round trip earlier.
    let (status, walk) = call(
        &app,
        "GET",
        "/registry/artifacts/prompt/system/commits",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "commit walk failed: {walk}");
    assert_eq!(walk["commits"][0]["candidate_id"], json!(candidate_id));
    assert_eq!(
        walk["commits"][0]["author"],
        json!({"type": "human", "human_id": "amjad"})
    );

    // 7. Tamper with the resolution event in the exported snapshot and
    //    verification fails, naming the journal head — the resolution is
    //    signature-covered evidence, not a log line.
    let mut tampered = snapshot_of(&app, &run_id).await;
    for event in tampered["events"].as_array_mut().unwrap() {
        if event["kind"] == json!("config_resolved") {
            event["output"]["value"]["digest"] = json!("0".repeat(64));
        }
    }
    let (status, v) = call(
        &app,
        "POST",
        "/receipts/verify",
        Some(json!({"snapshot": tampered, "receipt": receipt})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(v["error"], json!("receipt_verification_failed"));
    assert!(
        v["message"].as_str().unwrap().starts_with("journal_head:"),
        "tampering with the resolution event fails at the signed head: {v}"
    );

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Conservatism: an unbound run is the pre-R0.11 behavior, byte-identical
// --------------------------------------------------------------------- //

#[tokio::test]
async fn an_unbound_run_journals_no_resolution_and_pins_no_manifest() {
    let (app, store) = app();
    // A promoted artifact exists — irrelevant to a run that declares
    // nothing.
    let journal_run = run_pipeline(&app).await;
    let v1 = prompt_candidate("system", "You are terse.", 1_750_000_002_000);
    let id1 = create_and_evaluate(&app, &journal_run, &v1).await;
    declare_and_commit(&app, "prompt", "system", std::slice::from_ref(&id1)).await;
    promote(&app, &journal_run, &v1, &id1, Some("prod")).await;

    let run_id = run_pipeline(&app).await;
    let snapshot = snapshot_of(&app, &run_id).await;
    assert!(
        resolutions(&snapshot).is_empty(),
        "an unbound run journals no resolution events"
    );
    let receipt = receipt_of(&app, &run_id).await;
    assert!(
        receipt.get("manifest").is_none(),
        "an unbound run pins no manifest: {receipt}"
    );
    assert!(receipt.get("manifest_digest").is_none());

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// The model_settings family: resolution pins model id and parameters
// --------------------------------------------------------------------- //

#[tokio::test]
async fn model_settings_resolution_pins_model_and_parameters() {
    let (app, store) = app();
    let journal_run = run_pipeline(&app).await;
    let settings = model_settings_candidate("chat", 0.2, 1_750_000_002_000);
    let settings_id = create_and_evaluate(&app, &journal_run, &settings).await;
    declare_and_commit(
        &app,
        "model_settings",
        "chat",
        std::slice::from_ref(&settings_id),
    )
    .await;
    promote(&app, &journal_run, &settings, &settings_id, Some("prod")).await;

    let run_id = run_pipeline_bound(
        &app,
        binding(
            Some("prod"),
            json!([{"family": "model_settings", "name": "chat"}]),
        ),
    )
    .await;
    let resolved = resolutions(&snapshot_of(&app, &run_id).await);
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0]["surface"], json!("model_settings:chat"));
    assert_eq!(resolved[0]["candidate_id"], json!(settings_id));
    assert_eq!(resolved[0]["model"], json!("gpt-4o"));

    // The manifest carries the pair: the provider-precise model id
    // verbatim, the parameters as the journaled digest.
    let receipt = receipt_of(&app, &run_id).await;
    assert_eq!(receipt["manifest"]["model"], json!("gpt-4o"));
    assert_eq!(
        receipt["manifest"]["model_params"], resolved[0]["digest"],
        "the parameters pin is the journaled resolution digest"
    );

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Admission failures: the run never starts
// --------------------------------------------------------------------- //

#[tokio::test]
async fn admission_failures_stop_the_run_before_it_starts() {
    let (app, store) = app();
    let artifacts = || json!([{"family": "prompt", "name": "system"}]);

    // Nothing was ever promoted for the environment: 404 — an
    // unpromoted artifact is unresolvable, never an invented default.
    let (status, v) = submit_bound(&app, binding(Some("prod"), artifacts())).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "unpromoted artifact: {v}");
    assert!(
        v["message"]
            .as_str()
            .unwrap()
            .contains("no version pointer"),
        "the refusal says why: {v}"
    );

    // A binding that names no artifacts is a configuration error, not an
    // unbound run (omit the field for that): 422.
    let (status, v) = submit_bound(&app, binding(Some("prod"), json!([]))).await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "empty binding: {v}"
    );

    // The same artifact twice: 422 — one artifact binds one version.
    let (status, v) = submit_bound(
        &app,
        binding(
            Some("prod"),
            json!([
                {"family": "prompt", "name": "system"},
                {"family": "prompt", "name": "system"},
            ]),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "duplicate artifact: {v}"
    );

    // Two model_settings artifacts: 422 — the manifest's `model` slot is
    // singular.
    let (status, v) = submit_bound(
        &app,
        binding(
            Some("prod"),
            json!([
                {"family": "model_settings", "name": "chat"},
                {"family": "model_settings", "name": "backup"},
            ]),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "two model_settings: {v}"
    );

    // A malformed environment tag fails the payload parse (validated at
    // deserialization), a client error — never a silent untagged run.
    let (status, v) = submit_bound(&app, binding(Some("bad tag!"), artifacts())).await;
    assert!(
        status.is_client_error(),
        "a malformed tag is refused: {status} {v}"
    );

    // A pointer that serves nothing: promote v1 to prod, then roll it
    // back to default — the pointer exists with `active: null`, and the
    // run refuses (404) instead of guessing.
    let journal_run = run_pipeline(&app).await;
    let v1 = prompt_candidate("system", "You are terse.", 1_750_000_002_000);
    let id1 = create_and_evaluate(&app, &journal_run, &v1).await;
    declare_and_commit(&app, "prompt", "system", std::slice::from_ref(&id1)).await;
    promote(&app, &journal_run, &v1, &id1, Some("prod")).await;
    let (status, v) = call(
        &app,
        "POST",
        &format!("/learn/candidates/{id1}/rollback"),
        Some(json!({
            "run_id": journal_run,
            "cause": "revert to the static default",
            "tag": "prod",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "rollback failed: {v}");
    assert!(v["pointer"]["active"].is_null());

    let (status, v) = submit_bound(&app, binding(Some("prod"), artifacts())).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a pointer serving nothing refuses: {v}"
    );
    assert!(
        v["message"].as_str().unwrap().contains("serves nothing"),
        "the refusal says why: {v}"
    );

    // A family with no manifest digest slot — promoted, resolvable as a
    // *pointer*, refused as a *pin* (422): wave 2 never fakes coverage
    // the manifest cannot express.
    let settings = memory_configuration_candidate("default", 1_750_000_003_000);
    let settings_id = create_and_evaluate(&app, &journal_run, &settings).await;
    declare_and_commit(
        &app,
        "memory_configuration",
        "default",
        std::slice::from_ref(&settings_id),
    )
    .await;
    promote(&app, &journal_run, &settings, &settings_id, None).await;
    let (status, v) = submit_bound(
        &app,
        binding(
            None,
            json!([{"family": "memory_configuration", "name": "default"}]),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "memory_configuration has no pin slot this wave: {v}"
    );
    assert!(
        v["message"]
            .as_str()
            .unwrap()
            .contains("memory_configuration"),
        "the refusal names the family: {v}"
    );

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Environment resolution: the run's declaration, else the deployment's
// declared default — never an invented tag
// --------------------------------------------------------------------- //

#[tokio::test]
async fn the_default_environment_tag_applies_when_the_run_declares_none() {
    let store = temp_store();
    let app = app_with(store.clone(), |config| {
        config.with_default_environment_tag("prod")
    });
    let journal_run = run_pipeline(&app).await;
    let v1 = prompt_candidate("system", "You are terse.", 1_750_000_002_000);
    let id1 = create_and_evaluate(&app, &journal_run, &v1).await;
    let v2 = prompt_candidate("system", "You are rehearsal.", 1_750_000_003_000);
    let id2 = create_and_evaluate(&app, &journal_run, &v2).await;
    declare_and_commit(&app, "prompt", "system", &[id1.clone(), id2.clone()]).await;
    promote(&app, &journal_run, &v1, &id1, Some("prod")).await;
    promote(&app, &journal_run, &v2, &id2, Some("staging")).await;

    // No environment on the run: the deployment's declared default
    // (`prod`) supplies the tag — journaled, so the audit reads which
    // environment the run resolved against.
    let run_default = run_pipeline_bound(
        &app,
        binding(None, json!([{"family": "prompt", "name": "system"}])),
    )
    .await;
    let resolved = resolutions(&snapshot_of(&app, &run_default).await);
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0]["tag"], json!("prod"));
    assert_eq!(resolved[0]["candidate_id"], json!(id1));

    // An explicit environment overrides the deployment default.
    let run_staging = run_pipeline_bound(
        &app,
        binding(
            Some("staging"),
            json!([{"family": "prompt", "name": "system"}]),
        ),
    )
    .await;
    let resolved = resolutions(&snapshot_of(&app, &run_staging).await);
    assert_eq!(resolved[0]["tag"], json!("staging"));
    assert_eq!(resolved[0]["candidate_id"], json!(id2));

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn a_run_without_a_tag_and_no_default_resolves_the_untagged_surface() {
    let (app, store) = app();
    let journal_run = run_pipeline(&app).await;
    let v1 = prompt_candidate("system", "You are terse.", 1_750_000_002_000);
    let id1 = create_and_evaluate(&app, &journal_run, &v1).await;
    declare_and_commit(&app, "prompt", "system", std::slice::from_ref(&id1)).await;
    // Promoted untagged — the pre-R0.11 pointer.
    promote(&app, &journal_run, &v1, &id1, None).await;

    let run_id = run_pipeline_bound(
        &app,
        binding(None, json!([{"family": "prompt", "name": "system"}])),
    )
    .await;
    let resolved = resolutions(&snapshot_of(&app, &run_id).await);
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0]["surface"], json!("prompt:system"));
    assert!(
        resolved[0].get("tag").is_none(),
        "an untagged resolution carries no invented tag: {resolved:?}"
    );
    assert_eq!(resolved[0]["candidate_id"], json!(id1));

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Tenant scoping: the run resolves in the submitting tenant's namespace
// --------------------------------------------------------------------- //

#[tokio::test]
async fn bound_runs_resolve_in_the_submitting_tenants_namespace() {
    let store = temp_store();
    let app = app_with(store.clone(), |config| {
        config
            .with_tenant_key("acme", "acme-secret")
            .with_tenant_key("globex", "globex-secret")
    });
    let acme = Some(("x-api-key", "acme-secret"));
    let globex = Some(("x-api-key", "globex-secret"));

    // acme promotes v1 to prod inside its own namespace.
    let journal_run = run_pipeline_as(&app, acme).await;
    let v1 = prompt_candidate("system", "You are terse.", 1_750_000_002_000);
    let id1 = create_and_evaluate_as(&app, acme, &journal_run, &v1).await;
    let (status, v) = call_as(
        &app,
        acme,
        "POST",
        "/registry/artifacts",
        Some(json!({
            "family": "prompt",
            "name": "system",
            "owner": serde_json::to_value(owner()).unwrap(),
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "declare failed: {v}");
    let (status, v) = call_as(
        &app,
        acme,
        "POST",
        "/registry/artifacts/prompt/system/commits",
        Some(json!({"candidate_id": id1})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "commit failed: {v}");
    promote_as(&app, acme, &journal_run, &v1, &id1, Some("prod")).await;

    let registry = binding(
        Some("prod"),
        json!([{"family": "prompt", "name": "system"}]),
    );

    // Sanity: acme's own bound run resolves (the tenant comes from the
    // thread, so no caller threads the request context through).
    let (status, v) = submit_bound_as(&app, acme, registry.clone()).await;
    assert_eq!(status, StatusCode::OK, "acme's bound run failed: {v}");

    // globex submits the identical binding and gets 404 —
    // indistinguishable from nothing ever promoted, so one tenant's
    // registry never leaks into another's admission.
    let (status, v) = submit_bound_as(&app, globex, registry).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "globex must not resolve acme's pointer: {v}"
    );

    let _ = std::fs::remove_dir_all(store);
}
