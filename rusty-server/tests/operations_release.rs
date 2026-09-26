//! The operations release proof (R0.12 Operations Plane, wave 4): one
//! release, end to end, on the receipt chain. A candidate ships through
//! the learn pipeline into a frozen revision; the gated environment
//! admits it (the decision journaled); a canary binds a reproducible
//! seeded subset of new runs; the canary's own run commits a named
//! artifact whose lineage walks receipt → signed journal head →
//! journaled resolution → pin-set digest → revision → candidate →
//! bytes; the canary graduates through the gate; and the rollback
//! re-points byte-exactly at what served before — every act attributed
//! on one hash-chained evidence chain, and the canary run's receipt
//! still verifying after the pointer has moved twice. The proof:
//!
//! - the control-plane chain is exactly the release's acts, in order —
//!   declarations, registrations, gate decisions, the canary act, the
//!   promotions, the rollback — each naming its author;
//! - the canary-bound run re-derives its assignment from its journaled
//!   resolution alone (the seeded draw recomputed over its run id);
//! - the audit hops are facts, not references: the receipt's signed
//!   journal head equals the exported snapshot's recomputed head hash,
//!   the resolution's pin-set digest recomputes from the revision's own
//!   pins, the revision's address verifies against its content, and the
//!   committed bytes round-trip by their content address;
//! - the receipt outlives the release: minted over the canary run, it
//!   verifies unchanged after graduation and rollback.
//!
//! Driven in-process via `tower::ServiceExt::oneshot`, the
//! `deployments.rs` / `release_gates.rs` harness convention — the learn
//! pipeline is scenery (a scripted candidate evaluator), the gate
//! evaluator is scripted to allow, and the release machinery is the
//! proof. `prod` stands declared as the train's next hop; the proof
//! exercises the gated hop (`staging`) end to end.

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use chrono::{DateTime, Utc};
use rusty_agent_runtime::artifact::ArtifactCommitment;
use rusty_agent_runtime::broker::hex_encode;
use rusty_agent_runtime::deploy::{
    deployment_surface, pin_set_digest, GateCheckRecord, GateDeclaration, GateEvaluation,
    GateVerdict, RegistryPin, RevisionGateEvaluator,
};
use rusty_agent_runtime::effects::{derive_effect_id, ApprovalToken};
use rusty_agent_runtime::error::Result as RuntimeResult;
use rusty_agent_runtime::learn::{
    canary_admits, promotion_effect_id, CanaryBinding, Candidate, CandidateContent,
    CandidateEvaluation, CandidateEvaluator, CandidateId, EnvironmentTag, EvaluationRequest,
    EvaluationVerdict, EvidenceSpan, ReplaySummary,
};
use rusty_agent_runtime::memory::ProvenanceAuthor;
use rusty_agent_runtime::record::sha256_hex;
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

// --------------------------------------------------------------------- //
// Harness (the release_gates.rs shapes, verbatim where the semantics
// match; the artifact helpers lift the artifacts.rs commit path)
// --------------------------------------------------------------------- //

/// Unique temp store root, removed at the end of the test (best effort).
fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!(
        "rusty-server-operations-release-test-{}",
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
/// contract) and allows, so the proof owns the verdict and the machinery
/// under test is everything around it.
#[derive(Debug)]
struct AllowGateEvaluator;

#[async_trait::async_trait]
impl RevisionGateEvaluator for AllowGateEvaluator {
    async fn evaluate(
        &self,
        _revision: &rusty_agent_runtime::deploy::DeploymentRevision,
        _baseline: Option<&rusty_agent_runtime::deploy::DeploymentRevision>,
        gate: &GateDeclaration,
    ) -> RuntimeResult<GateEvaluation> {
        Ok(GateEvaluation {
            policy: gate.policy.clone(),
            dataset_version: gate.dataset_version.clone(),
            outcome: GateVerdict::Allow,
            checks: vec![GateCheckRecord {
                metric: "\"minimum_run_pass_rate\"".into(),
                passed: true,
                observed: json!(0.97),
                required: json!({"minimum": 0.95}),
                detail: "aggregate run pass rate 0.97 clears the 0.95 floor".into(),
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

/// Open-mode app over a temp store with the scripted evaluators.
fn app() -> (Router, PathBuf) {
    let store = temp_store();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_candidate_evaluator(Arc::new(FixedEvaluator))
        .with_revision_gate_evaluator(Arc::new(AllowGateEvaluator));
    (router(pipeline_registry(), config), store)
}

/// Send a request; returns `(status, json-body-or-null)`.
async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    let body = if let Some(v) = body {
        builder = builder.header("content-type", "application/json");
        Body::from(v.to_string())
    } else {
        Body::empty()
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

/// Fetch raw bytes from a GET route (the artifact byte read).
async fn get_raw(app: &Router, uri: &str) -> (StatusCode, Option<String>, Bytes) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let bytes: Bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, content_type, bytes)
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

/// Mint (or serve) the run's receipt; asserts 200.
async fn receipt_of(app: &Router, run_id: &str) -> Value {
    let (status, v) = call(app, "GET", &format!("/runs/{run_id}/receipt"), None).await;
    assert_eq!(status, StatusCode::OK, "receipt failed: {v}");
    v
}

/// The run's journaled events (the Flight Recorder read).
async fn run_events(app: &Router, run_id: &str) -> Vec<Value> {
    let (status, v) = call(app, "GET", &format!("/runs/{run_id}/events"), None).await;
    assert_eq!(status, StatusCode::OK, "events failed: {v}");
    v["events"].as_array().unwrap().clone()
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

/// The evaluate payload (the scripted evaluator clears it).
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

/// Declare the `prompt:system` artifact and commit the candidate id.
async fn declare_and_commit(app: &Router, candidate_id: &str) {
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
    let (status, v) = call(
        app,
        "POST",
        "/registry/artifacts/prompt/system/commits",
        Some(json!({"candidate_id": candidate_id})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "commit failed: {v}");
}

/// Commit an additional candidate id to `prompt:system`; asserts 200.
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

/// Declare an environment; asserts 201. Gate optional.
async fn declare_env_full(app: &Router, name: &str, gate: Option<Value>) {
    let (status, v) = call(
        app,
        "POST",
        "/deployments/environments",
        Some(json!({
            "name": name,
            "gate": gate,
            "approval_required": false,
            "author": author(),
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "declare {name} failed: {v}");
}

/// The gate declaration the release ships through.
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

/// Promote a revision into an environment; asserts 201 and returns the
/// body (the moved pointer).
async fn deploy_promote(app: &Router, environment: &str, revision_id: &str) -> Value {
    let (status, v) = call(
        app,
        "POST",
        &format!("/deployments/environments/{environment}/promote"),
        Some(json!({
            "revision_id": revision_id,
            "author": author(),
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "promote failed: {v}");
    v
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

/// A realistic effect id, derived the way the producing node would.
fn effect_id_for(run_id: &str) -> String {
    derive_effect_id(
        run_id,
        "render_report",
        &sha256_hex(b"weekly-canary"),
        Some("render:1"),
    )
    .to_string()
}

// --------------------------------------------------------------------- //
// The release proof
// --------------------------------------------------------------------- //

#[tokio::test]
async fn release_proof_ship_evaluate_canary_rollback_on_the_receipt_chain() {
    let (app, store) = app();

    // The release train: dev plain, staging gated (the hop the proof
    // exercises), prod plain — the train's next hop, declared.
    declare_env_full(&app, "dev", None).await;
    declare_env_full(&app, "staging", Some(gate_declaration())).await;
    declare_env_full(&app, "prod", None).await;

    // v1 ships: candidate → evaluation → registry commit → learn
    // promotion → frozen revision → gated promotion into staging.
    let journal_run = run_pipeline(&app).await;
    let v1 = prompt_candidate("system", "You are terse.", 1_750_000_002_000);
    let id1 = create_and_evaluate(&app, &journal_run, &v1).await;
    declare_and_commit(&app, &id1).await;
    promote(&app, &journal_run, &v1, &id1, "staging").await;
    let r1 = create_revision(&app, "staging", &["prompt:system"]).await;
    let v = deploy_promote(&app, "staging", &r1).await;
    assert_eq!(v["pointer"]["active"], json!(r1), "r1 serves staging");

    // v2 follows the same pipeline into its own revision — a changed pin
    // set is a new address.
    let v2 = prompt_candidate("system", "You are expansive.", 1_750_000_003_000);
    let id2 = create_and_evaluate(&app, &journal_run, &v2).await;
    commit_candidate(&app, &id2).await;
    promote(&app, &journal_run, &v2, &id2, "staging").await;
    let r2 = create_revision(&app, "staging", &["prompt:system"]).await;
    assert_ne!(r1, r2, "a changed pin set is a new revision");

    // The canary: r2 serves a seeded tenth of new runs while r1 keeps
    // serving — the gate ran first (a canary into a gated environment
    // serves real traffic), then the declaration journaled.
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

    // Bound runs until one binds the canary — every run's journaled
    // resolution slot equals the seeded draw recomputed over its run id
    // (the recorded run re-derives its assignment from the journaled
    // resolution alone). Two hundred draws at 0.1 miss with probability
    // under one in a billion.
    let mut canary_run: Option<String> = None;
    for _ in 0..200 {
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
            canary_run = Some(run_id);
            break;
        }
    }
    let canary_run =
        canary_run.expect("the seeded draw binds a subset — two hundred runs must meet the canary");

    // The canary run commits its named artifact: the bytes address by
    // their hash, and the commitment journals on the run's own chain,
    // parented to the producing event.
    let bytes = b"\x89PNG pretend canary report bytes".as_slice();
    let producing_event = run_events(&app, &canary_run).await.last().unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, v) = call(
        &app,
        "POST",
        "/artifacts/commits",
        Some(json!({
            "bytes_hex": hex_encode(bytes),
            "name": "canary-weekly-report",
            "media_kind": "image",
            "media_type": "image/png",
            "retention": {"policy": "days", "days": 30},
            "lineage": {
                "run_id": canary_run,
                "effect_id": effect_id_for(&canary_run),
                "event_id": producing_event,
            },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "artifact commit failed: {v}");
    let artifact_id = v["artifact_id"].as_str().unwrap().to_string();
    assert_eq!(artifact_id, sha256_hex(bytes), "identity is integrity");

    // The receipt over the canary run — minted after the commit, so the
    // signed head covers the artifact commitment — and the snapshot
    // exported for verification.
    let snapshot = snapshot_of(&app, &canary_run).await;
    let receipt = receipt_of(&app, &canary_run).await;

    // The audit walk, hop by hop, each a recomputed fact:
    //
    // receipt → journal head: the receipt's signed head equals the
    // exported snapshot's recomputed head hash (the manifest digest of
    // this run's world).
    assert!(is_hex64(&snapshot["head_hash"]));
    assert_eq!(
        receipt["journal_head"]["sha256"], snapshot["head_hash"],
        "the receipt signs exactly the exported head"
    );

    // journal head → resolution: the journaled admission names the
    // canary slot and r2.
    let resolutions = deployment_resolutions(&snapshot);
    assert_eq!(resolutions.len(), 1);
    let resolution = &resolutions[0];
    assert_eq!(resolution["environment"], json!("staging"));
    assert_eq!(resolution["pointer"], json!("canary"));
    assert_eq!(resolution["revision_id"], json!(r2));

    // resolution → revision: the pin-set digest recomputes from the
    // revision's own pins, and the revision's address verifies against
    // its content (the GET serves the addressed record).
    let revision_body = get_revision(&app, &r2).await;
    assert_eq!(revision_body["revision"]["revision_id"], json!(r2));
    let pins_value = revision_body["revision"]["content"]["pins"].clone();
    let pins: Vec<RegistryPin> = serde_json::from_value(pins_value.clone()).unwrap();
    assert_eq!(pins.len(), 1, "one frozen surface");
    assert_eq!(
        pins_value[0]["candidate_id"],
        json!(id2),
        "the pin names the promoted candidate"
    );
    assert_eq!(
        resolution["pin_set_digest"],
        json!(pin_set_digest(&pins)),
        "the journaled digest recomputes from the revision's pins"
    );

    // revision → candidate: the record reads back with its lifecycle
    // state — evaluated and promoted, attributed to the distiller.
    let (status, record) = call(&app, "GET", &format!("/learn/candidates/{id2}"), None).await;
    assert_eq!(status, StatusCode::OK, "candidate read failed: {record}");
    assert_eq!(record["candidate"]["candidate_id"], json!(id2));
    assert_eq!(record["status"], json!("promoted"));

    // run → artifact: the journaled commitment walks to the address, the
    // version, the producing effect, and the byte count — and the bytes
    // round-trip by that address with their declared media type.
    let events = run_events(&app, &canary_run).await;
    let commits: Vec<&Value> = events
        .iter()
        .filter(|event| event["kind"] == "artifact_committed")
        .collect();
    assert_eq!(commits.len(), 1);
    assert_eq!(commits[0]["parent"], json!(producing_event));
    let commitment: ArtifactCommitment =
        serde_json::from_value(commits[0]["output"]["value"].clone()).unwrap();
    assert_eq!(commitment.artifact_id, artifact_id);
    assert_eq!(commitment.name.as_deref(), Some("canary-weekly-report"));
    assert_eq!(commitment.version, Some(0));
    assert_eq!(commitment.effect_id.as_str(), effect_id_for(&canary_run));
    assert_eq!(commitment.bytes, bytes.len() as u64);
    let (status, content_type, body) =
        get_raw(&app, &format!("/artifacts/{artifact_id}/bytes")).await;
    assert_eq!(status, StatusCode::OK, "bytes failed: {body:?}");
    assert_eq!(content_type.as_deref(), Some("image/png"));
    assert_eq!(body.as_ref(), bytes);

    // Graduation: r2 promotes through the gate (the third journaled
    // decision); a full promotion supersedes the experiment it
    // graduated from — the canary slot clears with the pointer move.
    let v = deploy_promote(&app, "staging", &r2).await;
    assert_eq!(v["pointer"]["active"], json!(r2));
    assert_eq!(v["pointer"]["canary"], Value::Null);

    // Rollback: the pointer re-points byte-exactly at what served
    // before — the same content address, never a reconstruction.
    let (status, v) = call(
        &app,
        "POST",
        "/deployments/environments/staging/rollback",
        Some(json!({
            "author": author(),
            "cause": "canary cohort pass rate dipped under review — incident R0.12-7",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "rollback failed: {v}");
    assert_eq!(v["pointer"]["active"], json!(r1));
    let pointer = pointer_of(&app, "staging").await;
    assert_eq!(
        pointer["active"],
        json!(r1),
        "byte-exact: the address that served"
    );
    assert_eq!(pointer["canary"], Value::Null);

    // The chain is exactly the release's acts, in order — nothing else
    // journaled, nothing missing.
    let events = chain_events(&app).await;
    assert_eq!(
        kinds(&events),
        vec![
            "environment_declared",
            "environment_declared",
            "environment_declared",
            "revision_registered",
            "gate_decision_recorded",
            "revision_promoted",
            "revision_registered",
            "gate_decision_recorded",
            "canary_declared",
            "gate_decision_recorded",
            "revision_promoted",
            "revision_rolled_back",
        ],
        "the chain is the control plane's journal, in order: {events:?}"
    );

    // Every act names its author — an unattributed act is
    // indistinguishable from an untracked edit.
    let author_json = json!({"type": "human", "human_id": "amjad"});
    for event in &events {
        let value = &event["output"]["value"];
        let actor = match event["kind"].as_str().unwrap() {
            "environment_declared" => &value["environment"]["created_by"],
            "revision_registered" => &value["revision"]["author"],
            "revision_promoted" | "revision_rolled_back" | "canary_declared" => &value["author"],
            _ => continue, // the gate decision attributes through the promotion it guards
        };
        assert_eq!(
            actor, &author_json,
            "{}: every act names its author",
            event["kind"]
        );
    }

    // The receipt outlives the release: minted over the canary run
    // before the pointer moved twice, it verifies unchanged against the
    // exported snapshot.
    let (status, v) = call(
        &app,
        "POST",
        "/receipts/verify",
        Some(json!({"snapshot": snapshot, "receipt": receipt})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "verify failed: {v}");
    assert_eq!(v["run_id"], json!(canary_run));
    assert_eq!(
        v["journal_head"]["sha256"],
        receipt["journal_head"]["sha256"]
    );
    assert_eq!(v["signer"], receipt["signer"]);

    let _ = std::fs::remove_dir_all(store);
}
