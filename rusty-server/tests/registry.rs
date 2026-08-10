//! The configuration-registry integration tests (R0.11 Extension Plane,
//! wave 1): the `/registry/artifacts` surface over the default JSON-file
//! backend — declaration (converge, conflict, naming rules), the commit
//! history walk, diff views computed on read, environment-tagged
//! promotion through the unchanged learn pointer machinery, tenant
//! isolation, and restart durability. Live-Postgres coverage of the same
//! semantics is the gated section at the bottom (`RUSTY_TEST_DATABASE_URL`).
//!
//! Driven in-process via `tower::ServiceExt::oneshot` (no sockets), the
//! `learn_gate.rs` convention — including its scripted evaluator: a commit
//! *is* a candidate, so the fixtures here travel the learn pipeline
//! (create → evaluate) before joining an artifact's history, proving the
//! registry indexes the candidate pipeline instead of forking it.

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
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

// --------------------------------------------------------------------- //
// Harness (the learn_gate.rs shapes, verbatim where the semantics match)
// --------------------------------------------------------------------- //

/// Unique temp store root, removed at the end of each test (best effort).
fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!(
        "rusty-server-registry-test-{}",
        uuid::Uuid::new_v4()
    ))
}

/// A fixed timestamp, so candidates minted here are deterministic.
fn ts(millis: i64) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp_millis(millis).unwrap()
}

/// The scripted evaluator: a clean replay and an improving verdict —
/// enough to clear the default envelope's bars. This wave proves the
/// registry machinery; the evaluator is scenery.
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

/// The pipeline graph (`first -> second`, appending to a `log` channel):
/// candidate creation journals against a completed run, so every app here
/// registers the pipeline.
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
/// config customized by `configure` (tenant keys, the Postgres backend).
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

// --------------------------------------------------------------------- //
// Fixtures
// --------------------------------------------------------------------- //

fn owner() -> ProvenanceAuthor {
    ProvenanceAuthor::Human {
        human_id: "amjad".into(),
    }
}

/// A `prompt` candidate (`prompt:{name}`) — approval-ruled under the
/// R0.8 default envelope.
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

/// A `model_settings` candidate — one of the wave's four new families,
/// here to prove the new kinds flow through the unchanged candidate
/// pipeline and diff structurally.
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

/// Declare `prompt:{name}` and commit each candidate id in order.
async fn declare_and_commit(app: &Router, name: &str, candidate_ids: &[String]) {
    let (status, v) = declare(app, "prompt", name).await;
    assert_eq!(status, StatusCode::CREATED, "declare failed: {v}");
    for candidate_id in candidate_ids {
        let (status, v) = call(
            app,
            "POST",
            &format!("/registry/artifacts/prompt/{name}/commits"),
            Some(json!({"candidate_id": candidate_id})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "commit failed: {v}");
        assert_eq!(v["committed"], json!(true));
    }
}

// --------------------------------------------------------------------- //
// Declaration
// --------------------------------------------------------------------- //

#[tokio::test]
async fn declaration_creates_lists_and_converges() {
    let (app, store) = app();

    let (status, v) = declare(&app, "prompt", "system").await;
    assert_eq!(status, StatusCode::CREATED, "declare failed: {v}");
    assert_eq!(v["created"], json!(true));
    assert_eq!(v["surface"], json!("prompt:system"));
    assert_eq!(v["artifact"]["family"], json!("prompt"));
    assert!(
        v["artifact"].get("commits").is_none(),
        "a declared-but-uncommitted artifact carries no placeholder commits"
    );

    // An identical re-declaration is the same fact: converge, 200.
    let (status, v) = declare(&app, "prompt", "system").await;
    assert_eq!(status, StatusCode::OK, "re-declare failed: {v}");
    assert_eq!(v["created"], json!(false));

    // The same surface under a *different owner* conflicts — artifact
    // identity is immutable.
    let (status, v) = call(
        &app,
        "POST",
        "/registry/artifacts",
        Some(json!({
            "family": "prompt",
            "name": "system",
            "owner": serde_json::to_value(ProvenanceAuthor::Human {
                human_id: "mallory".into(),
            })
            .unwrap(),
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "owner change must conflict: {v}"
    );

    // A different family under the same *name* is a different surface —
    // no conflict.
    let (status, v) = declare(&app, "model_settings", "system").await;
    assert_eq!(status, StatusCode::CREATED, "sibling family failed: {v}");

    // The listing answers the tenant's artifacts, filterable by family.
    let (status, v) = call(&app, "GET", "/registry/artifacts", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["artifacts"].as_array().unwrap().len(), 2);
    let (status, v) = call(&app, "GET", "/registry/artifacts?family=prompt", None).await;
    assert_eq!(status, StatusCode::OK);
    let artifacts = v["artifacts"].as_array().unwrap();
    assert_eq!(artifacts.len(), 1);
    assert_eq!(artifacts[0]["surface"], json!("prompt:system"));

    // The single-artifact read, by route address.
    let (status, v) = call(&app, "GET", "/registry/artifacts/prompt/system", None).await;
    assert_eq!(status, StatusCode::OK, "get failed: {v}");
    assert_eq!(v["surface"], json!("prompt:system"));
    let (status, _) = call(&app, "GET", "/registry/artifacts/prompt/unknown", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn declaration_enforces_the_naming_rules_and_family_vocabulary() {
    let (app, store) = app();
    let owner = serde_json::to_value(owner()).unwrap();

    // Names outside the naming rules are 422: empty, whitespace-padded,
    // carrying the tag separator, the tenant separator, or a control.
    for name in ["", " padded", "has@tag", "has/slash", "has\tcontrol"] {
        let (status, v) = call(
            &app,
            "POST",
            "/registry/artifacts",
            Some(json!({"family": "prompt", "name": name, "owner": owner})),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "name `{name}` must be refused: {v}"
        );
    }

    // An unknown family — in the body or in the path — is a client error.
    let (status, v) = call(
        &app,
        "POST",
        "/registry/artifacts",
        Some(json!({"family": "nonsense", "name": "system", "owner": owner})),
    )
    .await;
    assert!(
        status.is_client_error(),
        "unknown family in body must fail: {status} {v}"
    );
    let (status, v) = call(&app, "GET", "/registry/artifacts/nonsense/system", None).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "unknown family in path is a malformed path: {v}"
    );

    // All four new families declare under their wire names.
    for family in [
        "tool_contract",
        "model_settings",
        "memory_configuration",
        "middleware_composition",
    ] {
        let (status, v) = declare(&app, family, "main").await;
        assert_eq!(
            status,
            StatusCode::CREATED,
            "family `{family}` must declare: {v}"
        );
    }

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Commits — the exit criterion's "two prompt versions commit"
// --------------------------------------------------------------------- //

#[tokio::test]
async fn two_prompt_versions_commit_and_the_history_walks_oldest_first() {
    let (app, store) = app();
    let run_id = run_pipeline(&app).await;
    let v1 = prompt_candidate("system", "You are terse.", 1_750_000_002_000);
    let v2 = prompt_candidate("system", "You are warm and thorough.", 1_750_000_003_000);
    let id1 = create_and_evaluate(&app, &run_id, &v1).await;
    let id2 = create_and_evaluate(&app, &run_id, &v2).await;

    declare_and_commit(&app, "system", &[id1.clone(), id2.clone()]).await;

    // The artifact carries both commits, oldest first.
    let (status, v) = call(&app, "GET", "/registry/artifacts/prompt/system", None).await;
    assert_eq!(status, StatusCode::OK, "get failed: {v}");
    let commits = v["commits"].as_array().unwrap();
    assert_eq!(commits.len(), 2);
    assert_eq!(commits[0]["candidate_id"], json!(id1));
    assert_eq!(commits[1]["candidate_id"], json!(id2));

    // The history walk joins each commit with its candidate's lifecycle
    // status and author — a reviewer reads lineage in one round trip.
    let (status, v) = call(
        &app,
        "GET",
        "/registry/artifacts/prompt/system/commits",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "walk failed: {v}");
    assert_eq!(v["family"], json!("prompt"));
    let walk = v["commits"].as_array().unwrap();
    assert_eq!(walk.len(), 2);
    assert_eq!(walk[0]["candidate_id"], json!(id1));
    assert_eq!(walk[0]["status"], json!("evaluated"));
    assert_eq!(
        walk[0]["author"],
        serde_json::to_value(ProvenanceAuthor::Distiller {
            name: "correction-loop".into(),
        })
        .unwrap()
    );

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn commit_guards_keep_the_index_honest() {
    let (app, store) = app();
    let run_id = run_pipeline(&app).await;
    let candidate = prompt_candidate("system", "You are terse.", 1_750_000_002_000);
    let candidate_id = create_and_evaluate(&app, &run_id, &candidate).await;

    // Committing to an undeclared artifact is 404.
    let (status, v) = call(
        &app,
        "POST",
        "/registry/artifacts/prompt/ghost/commits",
        Some(json!({"candidate_id": candidate_id})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "unknown artifact: {v}");

    // Committing an unknown candidate is 404.
    let (status, v) = declare(&app, "prompt", "system").await;
    assert_eq!(status, StatusCode::CREATED, "declare failed: {v}");
    let (status, v) = call(
        &app,
        "POST",
        "/registry/artifacts/prompt/system/commits",
        Some(json!({"candidate_id": "c".repeat(64)})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "unknown candidate: {v}");

    // A candidate of another family has no business in this history.
    let settings = model_settings_candidate("system", 0.2, 1_750_000_004_000);
    let settings_id = create_and_evaluate(&app, &run_id, &settings).await;
    let (status, v) = call(
        &app,
        "POST",
        "/registry/artifacts/prompt/system/commits",
        Some(json!({"candidate_id": settings_id})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "family mismatch: {v}"
    );

    // A same-family candidate of another *surface* neither.
    let other = prompt_candidate("other", "You are curt.", 1_750_000_005_000);
    let other_id = create_and_evaluate(&app, &run_id, &other).await;
    let (status, v) = call(
        &app,
        "POST",
        "/registry/artifacts/prompt/system/commits",
        Some(json!({"candidate_id": other_id})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "surface mismatch: {v}"
    );

    // The real commit lands; re-committing the same candidate converges
    // (committed: false), it does not grow the history.
    let (status, v) = call(
        &app,
        "POST",
        "/registry/artifacts/prompt/system/commits",
        Some(json!({"candidate_id": candidate_id})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "commit failed: {v}");
    assert_eq!(v["committed"], json!(true));
    assert_eq!(v["commits"], json!(1));
    let (status, v) = call(
        &app,
        "POST",
        "/registry/artifacts/prompt/system/commits",
        Some(json!({"candidate_id": candidate_id})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "re-commit failed: {v}");
    assert_eq!(v["committed"], json!(false));
    assert_eq!(v["commits"], json!(1));

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Diff views — computed on read, never stored
// --------------------------------------------------------------------- //

#[tokio::test]
async fn two_committed_prompt_versions_diff_as_text() {
    let (app, store) = app();
    let run_id = run_pipeline(&app).await;
    let v1 = prompt_candidate(
        "system",
        "You are terse.\nAnswer in one line.",
        1_750_000_002_000,
    );
    let v2 = prompt_candidate(
        "system",
        "You are warm.\nAnswer in one line.\nCite your sources.",
        1_750_000_003_000,
    );
    let id1 = create_and_evaluate(&app, &run_id, &v1).await;
    let id2 = create_and_evaluate(&app, &run_id, &v2).await;
    declare_and_commit(&app, "system", &[id1.clone(), id2.clone()]).await;

    let (status, v) = call(
        &app,
        "GET",
        &format!("/registry/artifacts/prompt/system/diff?from={id1}&to={id2}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "diff failed: {v}");
    assert_eq!(v["from"], json!(id1));
    assert_eq!(v["to"], json!(id2));
    let diff = &v["diff"];
    assert_eq!(diff["view"], json!("text"));
    let lines = diff["lines"].as_array().unwrap();
    assert!(
        lines
            .iter()
            .any(|l| l["op"] == json!("removed") && l["line"] == json!("You are terse.")),
        "the base's replaced line appears as removed: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l["op"] == json!("added") && l["line"] == json!("You are warm.")),
        "the target's replacement appears as added: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l["op"] == json!("context") && l["line"] == json!("Answer in one line.")),
        "the shared line is context: {lines:?}"
    );

    // A diff views two committed versions of one artifact, nothing
    // wider: an uncommitted endpoint is 422, an unknown artifact 404.
    let uncommitted = prompt_candidate("system", "drifter", 1_750_000_006_000);
    let uncommitted_id = create_and_evaluate(&app, &run_id, &uncommitted).await;
    let (status, v) = call(
        &app,
        "GET",
        &format!("/registry/artifacts/prompt/system/diff?from={id1}&to={uncommitted_id}"),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "uncommitted endpoint: {v}"
    );
    let (status, v) = call(
        &app,
        "GET",
        &format!("/registry/artifacts/prompt/ghost/diff?from={id1}&to={id2}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "unknown artifact: {v}");

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn a_json_family_diffs_structurally() {
    let (app, store) = app();
    let run_id = run_pipeline(&app).await;
    let v1 = model_settings_candidate("chat", 0.2, 1_750_000_002_000);
    let v2 = model_settings_candidate("chat", 0.9, 1_750_000_003_000);
    let id1 = create_and_evaluate(&app, &run_id, &v1).await;
    let id2 = create_and_evaluate(&app, &run_id, &v2).await;

    let (status, v) = declare(&app, "model_settings", "chat").await;
    assert_eq!(status, StatusCode::CREATED, "declare failed: {v}");
    for candidate_id in [&id1, &id2] {
        let (status, v) = call(
            &app,
            "POST",
            "/registry/artifacts/model_settings/chat/commits",
            Some(json!({"candidate_id": candidate_id})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "commit failed: {v}");
    }

    let (status, v) = call(
        &app,
        "GET",
        &format!("/registry/artifacts/model_settings/chat/diff?from={id1}&to={id2}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "diff failed: {v}");
    let diff = &v["diff"];
    assert_eq!(diff["view"], json!("structural"));
    let changed = diff["changed"].as_array().unwrap();
    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0]["path"], json!("/parameters/temperature"));
    assert_eq!(changed[0]["from"], json!(0.2));
    assert_eq!(changed[0]["to"], json!(0.9));
    assert!(diff["added"].as_array().unwrap().is_empty());
    assert!(diff["removed"].as_array().unwrap().is_empty());

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Environment-tagged promotion — the exit criterion's "pointer promotes
// per tag"
// --------------------------------------------------------------------- //

#[tokio::test]
async fn tagged_promotion_moves_the_tagged_pointer_only() {
    let (app, store) = app();
    let run_id = run_pipeline(&app).await;
    let candidate = prompt_candidate("system", "You are terse.", 1_750_000_002_000);
    let candidate_id = create_and_evaluate(&app, &run_id, &candidate).await;
    // The second environment serves the *next* version — the lifecycle is
    // per candidate (a promoted candidate cannot promote again), the
    // pointer per tagged surface.
    let next = prompt_candidate("system", "You are warm.", 1_750_000_003_000);
    let next_id = create_and_evaluate(&app, &run_id, &next).await;
    declare_and_commit(&app, "system", &[candidate_id.clone(), next_id.clone()]).await;

    // Prompts are approval-ruled under the default envelope: the
    // correctly scoped token admits (the learn gate's rule, unchanged —
    // the tag selects *which pointer* moves, not *whether* it may).
    let promote = |candidate: &Candidate, tag: &str| {
        json!({
            "run_id": run_id,
            "approval": serde_json::to_value(ApprovalToken::approve(
                promotion_effect_id(candidate),
                "ops:amjad",
            ))
            .unwrap(),
            "tag": tag,
        })
    };

    let (status, v) = call(
        &app,
        "POST",
        &format!("/learn/candidates/{candidate_id}/promote"),
        Some(promote(&candidate, "staging")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "tagged promote failed: {v}");
    assert_eq!(v["receipt"]["surface"], json!("prompt:system@staging"));
    assert_eq!(v["pointer"]["active"].as_str().unwrap(), candidate_id);

    // The staging pointer exists; the untagged surface was never touched.
    let (status, v) = call(&app, "GET", "/learn/versions", None).await;
    assert_eq!(status, StatusCode::OK);
    let surfaces: Vec<&str> = v["versions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["surface"].as_str().unwrap())
        .collect();
    assert_eq!(surfaces, vec!["prompt:system@staging"]);

    // A second environment promotes independently — the next version
    // serves prod while the first still serves staging.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/learn/candidates/{next_id}/promote"),
        Some(promote(&next, "prod")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "second tagged promote failed: {v}");
    assert_eq!(v["receipt"]["surface"], json!("prompt:system@prod"));
    let (status, v) = call(&app, "GET", "/learn/versions", None).await;
    assert_eq!(status, StatusCode::OK);
    let surfaces: Vec<&str> = v["versions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["surface"].as_str().unwrap())
        .collect();
    assert_eq!(
        surfaces,
        vec!["prompt:system@prod", "prompt:system@staging"]
    );

    // A malformed tag is a client error, not a silent untagged promote.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/learn/candidates/{candidate_id}/promote"),
        Some(json!({"run_id": run_id, "tag": "bad tag!"})),
    )
    .await;
    assert!(
        status.is_client_error(),
        "a malformed tag must fail validation: {status} {v}"
    );

    // Rolling back one environment leaves the other serving.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/learn/candidates/{candidate_id}/rollback"),
        Some(json!({
            "run_id": run_id,
            "cause": "staging soak failed",
            "tag": "staging",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "tagged rollback failed: {v}");
    assert!(v["pointer"]["active"].is_null());
    let (status, v) = call(&app, "GET", "/learn/versions", None).await;
    assert_eq!(status, StatusCode::OK);
    let versions = v["versions"].as_array().unwrap();
    let prod = versions
        .iter()
        .find(|p| p["surface"] == json!("prompt:system@prod"))
        .expect("the prod pointer survives the staging rollback");
    assert_eq!(prod["active"].as_str().unwrap(), next_id);

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Tenant isolation
// --------------------------------------------------------------------- //

#[tokio::test]
async fn artifacts_are_tenant_scoped() {
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
        "/registry/artifacts",
        Some(json!({
            "family": "prompt",
            "name": "system",
            "owner": serde_json::to_value(owner()).unwrap(),
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "acme declare failed: {v}");

    // Cross-tenant reads answer 404 — indistinguishable from unknown, so
    // one tenant cannot probe another's registry.
    let (status, v) = call_as(
        &app,
        globex,
        "GET",
        "/registry/artifacts/prompt/system",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "cross-tenant get: {v}");
    let (status, v) = call_as(&app, globex, "GET", "/registry/artifacts", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["artifacts"].as_array().unwrap().len(), 0);
    let (status, v) = call_as(
        &app,
        globex,
        "POST",
        "/registry/artifacts/prompt/system/commits",
        Some(json!({"candidate_id": "c".repeat(64)})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "cross-tenant commit: {v}");

    // The same declaration under globex is its own artifact, not a
    // conflict with acme's.
    let (status, v) = call_as(
        &app,
        globex,
        "POST",
        "/registry/artifacts",
        Some(json!({
            "family": "prompt",
            "name": "system",
            "owner": serde_json::to_value(ProvenanceAuthor::Human {
                human_id: "globex-ops".into(),
            })
            .unwrap(),
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "globex's own declaration must not see acme's artifact: {v}"
    );

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Restart durability (file backend) — the exit criterion's "survives
// restart"
// --------------------------------------------------------------------- //

#[tokio::test]
async fn artifacts_history_and_tagged_pointers_survive_a_restart() {
    let store = temp_store();
    let first = app_with(store.clone(), |config| config);
    let run_id = run_pipeline(&first).await;
    let v1 = prompt_candidate("system", "You are terse.", 1_750_000_002_000);
    let v2 = prompt_candidate("system", "You are warm.", 1_750_000_003_000);
    let id1 = create_and_evaluate(&first, &run_id, &v1).await;
    let id2 = create_and_evaluate(&first, &run_id, &v2).await;
    declare_and_commit(&first, "system", &[id1.clone(), id2.clone()]).await;

    let token = ApprovalToken::approve(promotion_effect_id(&v2), "ops:amjad");
    let (status, v) = call(
        &first,
        "POST",
        &format!("/learn/candidates/{id2}/promote"),
        Some(json!({
            "run_id": run_id,
            "approval": serde_json::to_value(&token).unwrap(),
            "tag": "prod",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "tagged promote failed: {v}");
    let artifact_before = call(&first, "GET", "/registry/artifacts/prompt/system", None)
        .await
        .1;
    drop(first);

    // A fresh app over the same store root serves the same registry:
    // the declaration, both commits, the diff, and the tagged pointer.
    let second = app_with(store.clone(), |config| config);
    let (status, artifact_after) =
        call(&second, "GET", "/registry/artifacts/prompt/system", None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "get after restart failed: {artifact_after}"
    );
    assert_eq!(
        artifact_after, artifact_before,
        "the artifact record is byte-identical after the restart"
    );
    let (status, v) = call(
        &second,
        "GET",
        &format!("/registry/artifacts/prompt/system/diff?from={id1}&to={id2}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "diff after restart failed: {v}");
    assert_eq!(v["diff"]["view"], json!("text"));
    let (status, v) = call(&second, "GET", "/learn/versions", None).await;
    assert_eq!(status, StatusCode::OK);
    let versions = v["versions"].as_array().unwrap();
    assert_eq!(versions.len(), 1);
    assert_eq!(versions[0]["surface"], json!("prompt:system@prod"));
    assert_eq!(versions[0]["active"].as_str().unwrap(), id2);

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Postgres backend (live database required)
//
// Gated on `RUSTY_TEST_DATABASE_URL`; unset skips cleanly so the suite is
// green without a database. Every run uses a dedicated tenant, so
// repeated runs against one scratch database never interfere; the
// database itself is throwaway.
// --------------------------------------------------------------------- //

#[cfg(feature = "postgres")]
mod postgres {
    use super::*;

    fn pg_url() -> Option<String> {
        std::env::var("RUSTY_TEST_DATABASE_URL").ok()
    }

    /// Wave-1 exit criteria on Postgres: declare, two versions commit,
    /// the diff reads, a tagged pointer promotes — and a reconnect (the
    /// restart) serves the settled registry.
    #[tokio::test]
    async fn postgres_registry_survives_a_reconnect() {
        let Some(url) = pg_url() else {
            eprintln!("RUSTY_TEST_DATABASE_URL unset; skipping");
            return;
        };
        let tenant = format!("registrypg-{}", uuid::Uuid::new_v4());
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

        let v1 = prompt_candidate("system", "You are terse.", 1_750_000_002_000);
        let v2 = prompt_candidate("system", "You are warm.", 1_750_000_003_000);
        let mut ids = Vec::new();
        for candidate in [&v1, &v2] {
            let (status, v) = call_as(
                &first,
                auth,
                "POST",
                "/learn/candidates",
                Some(json!({
                    "candidate": serde_json::to_value(candidate).unwrap(),
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
            ids.push(candidate_id);
        }

        let (status, v) = call_as(
            &first,
            auth,
            "POST",
            "/registry/artifacts",
            Some(json!({
                "family": "prompt",
                "name": "system",
                "owner": serde_json::to_value(owner()).unwrap(),
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "pg declare failed: {v}");
        for candidate_id in &ids {
            let (status, v) = call_as(
                &first,
                auth,
                "POST",
                "/registry/artifacts/prompt/system/commits",
                Some(json!({"candidate_id": candidate_id})),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "pg commit failed: {v}");
            assert_eq!(v["committed"], json!(true));
        }

        let token = ApprovalToken::approve(promotion_effect_id(&v2), "ops:amjad");
        let (status, v) = call_as(
            &first,
            auth,
            "POST",
            &format!("/learn/candidates/{}/promote", ids[1]),
            Some(json!({
                "run_id": run_id,
                "approval": serde_json::to_value(&token).unwrap(),
                "tag": "prod",
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "pg tagged promote failed: {v}");
        drop(first);

        // A fresh app over the same database serves the settled registry.
        let second = build();
        let (status, v) = call_as(
            &second,
            auth,
            "GET",
            "/registry/artifacts/prompt/system",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "pg get after restart failed: {v}");
        let commits = v["commits"].as_array().unwrap();
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0]["candidate_id"], json!(ids[0]));
        assert_eq!(commits[1]["candidate_id"], json!(ids[1]));
        let (status, v) = call_as(
            &second,
            auth,
            "GET",
            &format!(
                "/registry/artifacts/prompt/system/diff?from={}&to={}",
                ids[0], ids[1]
            ),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "pg diff after restart failed: {v}");
        assert_eq!(v["diff"]["view"], json!("text"));
        let (status, v) = call_as(&second, auth, "GET", "/learn/versions", None).await;
        assert_eq!(status, StatusCode::OK);
        let versions = v["versions"].as_array().unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0]["surface"], json!("prompt:system@prod"));
        assert_eq!(versions[0]["active"].as_str().unwrap(), ids[1]);
    }
}
