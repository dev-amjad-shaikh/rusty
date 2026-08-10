//! The R0.11 wave-4 release proof: the Extension Plane's wave-4 surfaces —
//! the OAuth connection lifecycle, the registry-admitted middleware
//! composition — walk end to end against the *production* paths (HTTP
//! routes, envelope-sealed store, broker custody, admission, journal,
//! receipt), in the `adaptation_release.rs` / `registry_admission.rs`
//! harness convention.
//!
//! Test one (`the_oauth_lifecycle_survives_promotion_rotation_and_revocation`)
//! is the lifecycle story, chapters marked in the body:
//!
//! 1. **Consent registers.** A connection registers through the
//!    authorization-code path: the code exchanges at the scripted provider
//!    seam, the grant seals before it touches the store. A prompt promotes
//!    to `prod`; a bound run's node — the connector boundary, the one place
//!    credential bytes may exist — issues and resolves through the app's
//!    own broker and observes the v1 access token. The token never enters
//!    state, journal, or receipt: the node journals `{"observed": true}`.
//! 2. **Re-consent rotates beneath the stable id.** A second
//!    authorization code re-consents the same connection; the act journals
//!    `connection_refreshed` and the connection id stands.
//! 3. **Promotion rebinds; history holds.** Prompt v2 promotes to `prod`;
//!    the next bound run pins v2's digest and resolves the rotated grant
//!    (v2 token) through the same connection — while run 1's journal still
//!    names v1's candidate, its re-served receipt is byte-identical, and
//!    the receipt still verifies against the exported journal.
//! 4. **Revocation fails closed at the next use.** Revoking the grant
//!    turns the very next run's issuance into a typed, journaled
//!    `connection_revoked` denial naming the grant — the run errors; the
//!    broker journal tail reads revocation → denial.
//! 5. **The bytes were never evidence.** Every file under the store root,
//!    plus the broker journal, the run journals, and the receipts over the
//!    wire, are scanned for both access tokens, both refresh tokens, and
//!    both authorization codes: absent everywhere.
//!
//! Test two (`a_middleware_composition_promotes_per_environment`) is the
//! composition story: a two-layer composition promotes to `prod`, a
//! one-layer successor promotes to `staging`, and runs bound to each
//! environment pin their own chain — the journaled resolution names the
//! resolved layer order, the manifest's `middleware` pin is the journaled
//! digest, and the `staging` promotion leaves `prod`'s pin untouched.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use chrono::{DateTime, Utc};
use rusty_agent_runtime::broker::{
    CredentialBroker, CredentialRequirement, IssueRequest, ScriptedOAuthProvider, TokenGrant,
};
use rusty_agent_runtime::effects::ApprovalToken;
use rusty_agent_runtime::error::{Result as RuntimeResult, RustyError};
use rusty_agent_runtime::learn::{
    promotion_effect_id, Candidate, CandidateContent, CandidateEvaluation, CandidateEvaluator,
    EvaluationRequest, EvaluationVerdict, MiddlewareLayerConfig, ReplaySummary,
};
use rusty_agent_runtime::memory::ProvenanceAuthor;
use rusty_agent_runtime::prelude::{GraphBuilder, NodeContext, NodeOutput, Reducer, StateSpec};
use rusty_agent_runtime::record::sha256_hex;
use rusty_agent_server::{router_with_broker, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

// --------------------------------------------------------------------- //
// Harness (the registry_admission.rs / broker.rs shapes, verbatim where
// the semantics match)
// --------------------------------------------------------------------- //

/// Unique temp store root, removed at the end of each test (best effort).
fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!(
        "rusty-server-extension-release-test-{}",
        uuid::Uuid::new_v4()
    ))
}

/// A fixed timestamp, so candidates minted here are deterministic.
fn ts(millis: i64) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp_millis(millis).unwrap()
}

/// The scripted evaluator: a clean replay and an improving verdict —
/// enough to clear the approval bar. This wave proves the lifecycle and
/// the admission; the evaluator is scenery (the registry_admission.rs
/// convention, verbatim).
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

/// The late-bound wiring between the graph under test and the app built
/// around it: the registry must exist before `router_with_broker` hands
/// back the broker, and the connection id exists only after the HTTP
/// registration — so the node reads both out of slots the test fills in
/// order. `observed` is the connector's one look at the credential,
/// kept in test-process memory only: the bytes never touch state,
/// journal, store, or receipt.
#[derive(Default)]
struct ConnectorSlots {
    broker: Arc<Mutex<Option<Arc<dyn CredentialBroker>>>>,
    connection: Arc<Mutex<Option<String>>>,
    observed: Arc<Mutex<Vec<String>>>,
}

/// The connector graph: one node that plays the host's HTTP/tool-call
/// boundary — issue a handle for the declared grant, resolve it, check
/// the token is the scripted provider's (the shape assertion stands in
/// for the outbound call the connector would make), and journal only
/// the fact of observation. Any broker denial fails the node closed,
/// carrying the typed reason into the run's terminal error.
fn connector_registry(slots: &ConnectorSlots) -> GraphRegistry {
    let broker_slot = Arc::clone(&slots.broker);
    let connection_slot = Arc::clone(&slots.connection);
    let observed = Arc::clone(&slots.observed);
    let spec = StateSpec::new().channel("log", Reducer::Append);
    let mut builder = GraphBuilder::new();
    builder.add_node("fetch", move |_ctx: NodeContext| {
        let broker_slot = Arc::clone(&broker_slot);
        let connection_slot = Arc::clone(&connection_slot);
        let observed = Arc::clone(&observed);
        async move {
            let broker = broker_slot
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| RustyError::Node("fetch: broker slot unfilled".to_owned()))?;
            let connection_id =
                connection_slot.lock().unwrap().clone().ok_or_else(|| {
                    RustyError::Node("fetch: connection slot unfilled".to_owned())
                })?;
            let scopes = BTreeSet::from(["drive.readonly".to_owned()]);
            let handle = broker
                .issue(&IssueRequest {
                    tenant: "default".to_owned(),
                    run_id: None,
                    requirement: CredentialRequirement {
                        connection_id,
                        scopes: scopes.clone(),
                    },
                })
                .await
                .map_err(|e| RustyError::Node(format!("fetch: issuance failed closed: {e}")))?;
            let resolved = broker
                .resolve(&handle.token(), &scopes)
                .await
                .map_err(|e| RustyError::Node(format!("fetch: resolution failed closed: {e}")))?;
            let token = resolved.material.access_token.clone();
            if !token.starts_with("oauth-token-") || !token.ends_with("-MARKER") {
                return Err(RustyError::Node(
                    "fetch: the resolved credential is not the scripted provider's".to_owned(),
                ));
            }
            observed.lock().unwrap().push(token);
            Ok(NodeOutput::update("log", json!({"observed": true})))
        }
    });
    builder.set_entry_point("fetch");
    let mut registry = GraphRegistry::new();
    registry.register("connector", builder.compile().unwrap(), spec);
    registry
}

/// The plain pipeline (`first -> second`, appending to a `log` channel) —
/// the middleware story's graph, which never touches credentials.
fn pipeline_registry() -> GraphRegistry {
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

/// An open-mode OAuth app over a fresh store: the scripted provider
/// plugged in, the evaluator registered, and the app's own broker wired
/// into the connector graph's slot.
fn oauth_app(provider: ScriptedOAuthProvider) -> (Router, PathBuf, ConnectorSlots) {
    let store = temp_store();
    let slots = ConnectorSlots::default();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_candidate_evaluator(Arc::new(FixedEvaluator))
        .with_oauth_provider(Arc::new(provider));
    let (app, broker) = router_with_broker(connector_registry(&slots), config);
    *slots.broker.lock().unwrap() = Some(broker);
    (app, store, slots)
}

/// A scripted grant expiring far beyond any refresh window (the
/// refresh-at-resolution path must not fire mid-proof).
fn grant(access_token: &str, refresh_token: Option<&str>) -> TokenGrant {
    TokenGrant {
        access_token: access_token.to_owned(),
        refresh_token: refresh_token.map(str::to_owned),
        expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
    }
}

/// Send a request; returns `(status, json-body-or-null)`.
async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
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

/// Read every byte the server wrote under the store root, concatenated —
/// the leak scan's corpus (the broker.rs shape).
fn store_bytes(store: &Path) -> Vec<u8> {
    let mut raw = Vec::new();
    let mut stack = vec![store.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                raw.extend(std::fs::read(&path).unwrap_or_default());
            }
        }
    }
    raw
}

/// Create a thread for `graph` and submit a run, optionally registry-
/// bound; asserts the HTTP call and returns the terminal body (the
/// caller inspects the run's own `status`).
async fn run_graph(app: &Router, graph: &str, registry: Option<Value>) -> Value {
    let (status, v) = call(app, "POST", "/threads", Some(json!({"graph": graph}))).await;
    assert_eq!(status, StatusCode::CREATED, "thread failed: {v}");
    let thread_id = v["thread_id"].as_str().unwrap().to_string();
    let payload = match registry {
        Some(binding) => json!({"registry": binding}),
        None => json!({}),
    };
    let (status, v) = call(
        app,
        "POST",
        &format!("/threads/{thread_id}/runs/wait"),
        Some(payload),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "run transport failed: {v}");
    v
}

/// Create a thread and run it to success; returns the run id.
async fn run_ok(app: &Router, graph: &str, registry: Option<Value>) -> String {
    let terminal = run_graph(app, graph, registry).await;
    assert_eq!(
        terminal["status"],
        json!("success"),
        "run failed: {terminal}"
    );
    terminal["run_id"].as_str().unwrap().to_string()
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

/// The registry binding payload shape every bound run submits.
fn binding(environment: &str, artifacts: Value) -> Value {
    json!({"environment": environment, "artifacts": artifacts})
}

fn owner() -> ProvenanceAuthor {
    ProvenanceAuthor::Human {
        human_id: "amjad".into(),
    }
}

/// An operator-authored `prompt` candidate — the release-proof prompts
/// are deployment configuration a named human wrote.
fn prompt_candidate(name: &str, text: &str, millis: i64) -> Candidate {
    Candidate::new(
        CandidateContent::Prompt {
            name: name.into(),
            prompt: text.into(),
        },
        owner(),
        rusty_agent_runtime::learn::EvidenceSpan::default(),
        ts(millis),
    )
    .unwrap()
}

/// An operator-authored `middleware_composition` candidate.
fn composition_candidate(name: &str, layers: Vec<MiddlewareLayerConfig>, millis: i64) -> Candidate {
    Candidate::new(
        CandidateContent::MiddlewareComposition {
            name: name.into(),
            layers,
        },
        owner(),
        rusty_agent_runtime::learn::EvidenceSpan::default(),
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

/// Declare `{family}:{name}` and commit each candidate id in order.
async fn declare_and_commit(app: &Router, family: &str, name: &str, candidate_ids: &[String]) {
    let (status, v) = call(
        app,
        "POST",
        "/registry/artifacts",
        Some(json!({
            "family": family,
            "name": name,
            "owner": serde_json::to_value(owner()).unwrap(),
        })),
    )
    .await;
    // A re-declare converges (200, `created: false`) — the artifact's
    // identity is its surface, so the second commit's declare is a no-op.
    assert!(
        status == StatusCode::CREATED || (status == StatusCode::OK && v["created"] == json!(false)),
        "declare failed: {v}"
    );
    for candidate_id in candidate_ids {
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
}

/// Promote `candidate` to `tag`; asserts 200.
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
    assert_eq!(status, StatusCode::OK, "promote failed: {v}");
}

// --------------------------------------------------------------------- //
// The OAuth lifecycle release proof
// --------------------------------------------------------------------- //

#[tokio::test]
async fn the_oauth_lifecycle_survives_promotion_rotation_and_revocation() {
    let provider = ScriptedOAuthProvider::new()
        .with_code(
            "code-1",
            grant("oauth-token-v1-MARKER", Some("rt-v1-MARKER")),
        )
        .with_code(
            "code-2",
            grant("oauth-token-v2-MARKER", Some("rt-v2-MARKER")),
        );
    let (app, store, slots) = oauth_app(provider);

    // ---------------- Chapter 1: consent registers; a bound run
    // resolves the v1 grant at the connector boundary. ---------------- //
    let (status, v) = call(
        &app,
        "POST",
        "/connections",
        Some(json!({
            "provider": "oauth2_authorization_code",
            "subject": "user-7",
            "scopes": ["drive.readonly"],
            "authorization_code": "code-1",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "register failed: {v}");
    let connection_id = v["connection"]["connection_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(v["connection"]["status"], json!("active"));
    *slots.connection.lock().unwrap() = Some(connection_id.clone());

    // The learn flow's evidence run (unbound — the pre-R0.11 shape).
    let journal_run = run_ok(&app, "connector", None).await;

    // Prompt v1 promotes to prod.
    let v1 = prompt_candidate("system", "You are terse.", 1_750_000_002_000);
    let id1 = create_and_evaluate(&app, &journal_run, &v1).await;
    declare_and_commit(&app, "prompt", "system", std::slice::from_ref(&id1)).await;
    promote(&app, &journal_run, &v1, &id1, "prod").await;

    // The first bound run: admission pins v1, the node resolves the v1
    // grant through the app's own broker — the manifest pin, the
    // journaled resolution, and the connector's one look agree.
    let prompt_binding = || binding("prod", json!([{"family": "prompt", "name": "system"}]));
    let run1 = run_ok(&app, "connector", Some(prompt_binding())).await;
    assert_eq!(
        slots.observed.lock().unwrap().as_slice(),
        ["oauth-token-v1-MARKER", "oauth-token-v1-MARKER"],
        "the journal run and run 1 both observed the v1 grant at the connector"
    );
    let digest_v1 = sha256_hex("You are terse.".as_bytes());
    let resolved1 = resolutions(&snapshot_of(&app, &run1).await);
    assert_eq!(resolved1.len(), 1, "one artifact, one resolution event");
    assert_eq!(resolved1[0]["surface"], json!("prompt:system"));
    assert_eq!(resolved1[0]["candidate_id"], json!(id1));
    assert_eq!(resolved1[0]["digest"], json!(digest_v1));
    let receipt1 = receipt_of(&app, &run1).await;
    assert_eq!(receipt1["manifest"]["prompts"]["system"], json!(digest_v1));

    // ---------------- Chapter 2: re-consent rotates beneath the stable
    // id. ------------------------------------------------------------- //
    let (status, v) = call(
        &app,
        "POST",
        &format!("/connections/{connection_id}/consent"),
        Some(json!({"authorization_code": "code-2"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "re-consent failed: {v}");
    assert_eq!(
        v["connection"]["connection_id"],
        json!(connection_id),
        "rotation keeps the connection id — handles outlive the material"
    );
    assert_eq!(v["journaled"], json!("connection_refreshed"));

    // The tenant-wide board reads the connection live, post-rotation.
    let (status, v) = call(&app, "GET", "/connections/health", None).await;
    assert_eq!(status, StatusCode::OK, "health failed: {v}");
    let board = v["connections"].as_array().unwrap();
    assert_eq!(board.len(), 1);
    assert_eq!(board[0]["connection_id"], json!(connection_id));
    assert_eq!(board[0]["status"], json!("active"));

    // ---------------- Chapter 3: promotion rebinds the next run; the
    // admitted run's evidence holds byte-exactly. --------------------- //
    let v2 = prompt_candidate("system", "You are warm and thorough.", 1_750_000_003_000);
    let id2 = create_and_evaluate(&app, &journal_run, &v2).await;
    declare_and_commit(&app, "prompt", "system", std::slice::from_ref(&id2)).await;
    promote(&app, &journal_run, &v2, &id2, "prod").await;

    // The next bound run pins v2 — and resolves the *rotated* grant (v2
    // token) through the same connection: the lifecycle rotated the
    // material, admission moved the pin, and neither disturbed the other.
    let run2 = run_ok(&app, "connector", Some(prompt_binding())).await;
    assert_eq!(
        slots.observed.lock().unwrap().last().map(String::as_str),
        Some("oauth-token-v2-MARKER"),
        "run 2 resolved the re-consented grant through the same connection"
    );
    let digest_v2 = sha256_hex("You are warm and thorough.".as_bytes());
    let resolved2 = resolutions(&snapshot_of(&app, &run2).await);
    assert_eq!(resolved2.len(), 1);
    assert_eq!(resolved2[0]["candidate_id"], json!(id2));
    assert_eq!(resolved2[0]["digest"], json!(digest_v2));
    let receipt2 = receipt_of(&app, &run2).await;
    assert_eq!(receipt2["manifest"]["prompts"]["system"], json!(digest_v2));
    assert_ne!(
        receipt1["manifest_digest"], receipt2["manifest_digest"],
        "the versions are distinct content"
    );

    // Run 1's evidence is untouched by either move: its journal still
    // names the candidate it used, its re-served receipt is
    // byte-identical, and the receipt still verifies against the
    // exported journal.
    let resolved1_after = resolutions(&snapshot_of(&app, &run1).await);
    assert_eq!(resolved1_after.len(), 1);
    assert_eq!(resolved1_after[0]["candidate_id"], json!(id1));
    let receipt1_reserved = receipt_of(&app, &run1).await;
    assert_eq!(
        receipt1, receipt1_reserved,
        "run 1's receipt re-serves byte-identically after rotation and promotion"
    );
    let (status, v) = call(
        &app,
        "POST",
        "/receipts/verify",
        Some(json!({"snapshot": snapshot_of(&app, &run1).await, "receipt": receipt1})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "run 1's receipt verifies after the pointer moved and the grant rotated: {v}"
    );

    // ---------------- Chapter 4: revocation fails closed at the next
    // use, typed and journaled. --------------------------------------- //
    let (status, v) = call(
        &app,
        "POST",
        &format!("/connections/{connection_id}/revoke"),
        Some(json!({"reason": "the user disconnected the drive integration"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "revoke failed: {v}");
    assert_eq!(v["connection"]["status"], json!("revoked"));

    let terminal = run_graph(&app, "connector", Some(prompt_binding())).await;
    assert_eq!(
        terminal["status"],
        json!("error"),
        "a revoked grant must fail the run, not serve: {terminal}"
    );
    let message = terminal["message"].as_str().unwrap();
    assert!(
        message.contains("connection_revoked"),
        "the run error names the typed reason: {message}"
    );
    assert!(
        message.contains("drive.readonly"),
        "the run error names the revoked grant: {message}"
    );

    // The broker's evidence chain reads the act and its consequence in
    // order: revocation, then the fail-closed denial naming the grant —
    // never the bytes.
    let (status, v) = call(&app, "GET", "/broker/journal", None).await;
    assert_eq!(status, StatusCode::OK, "broker journal failed: {v}");
    let events = v["events"].as_array().unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e["kind"].as_str().unwrap()).collect();
    let revocation_at = kinds
        .iter()
        .rposition(|k| *k == "connection_revoked")
        .unwrap();
    let denial_at = kinds
        .iter()
        .rposition(|k| *k == "credential_denied")
        .unwrap();
    assert!(
        revocation_at < denial_at,
        "the denial follows the revocation it enforces: {kinds:?}"
    );
    let denied = &events[denial_at]["output"]["value"];
    assert_eq!(denied["reason"], json!("connection_revoked"));
    assert_eq!(denied["grant"], json!(["drive.readonly"]));
    assert_eq!(denied["connection_id"], json!(connection_id));

    // ---------------- Chapter 5: the bytes were never evidence. ------- //
    // Everything the server persists, then everything it serves over the
    // wire: neither the access tokens, the refresh tokens, nor the
    // authorization codes may appear anywhere.
    let raw = store_bytes(&store);
    let served = [
        serde_json::to_string(&v).unwrap(),
        serde_json::to_string(&snapshot_of(&app, &run1).await).unwrap(),
        serde_json::to_string(&snapshot_of(&app, &run2).await).unwrap(),
        serde_json::to_string(&receipt1).unwrap(),
        serde_json::to_string(&receipt2).unwrap(),
    ]
    .concat();
    for marker in [
        "oauth-token-v1-MARKER",
        "oauth-token-v2-MARKER",
        "rt-v1-MARKER",
        "rt-v2-MARKER",
        "code-1",
        "code-2",
    ] {
        assert!(
            !raw.windows(marker.len()).any(|w| w == marker.as_bytes()),
            "the store leaked {marker}"
        );
        assert!(!served.contains(marker), "the wire leaked {marker}");
    }

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// The middleware composition release proof
// --------------------------------------------------------------------- //

#[tokio::test]
async fn a_middleware_composition_promotes_per_environment() {
    let store = temp_store();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_candidate_evaluator(Arc::new(FixedEvaluator));
    let (app, _broker) = router_with_broker(pipeline_registry(), config);

    let journal_run = run_ok(&app, "pipeline", None).await;

    // v1: the logger, then the blocklist — before-hooks run in declared
    // order, so the journal is the audit of the chain's onion semantics.
    let v1 = composition_candidate(
        "default",
        vec![
            MiddlewareLayerConfig {
                layer: "request_logger".to_owned(),
                config: None,
            },
            MiddlewareLayerConfig {
                layer: "tool_call_blocklist".to_owned(),
                config: Some(json!({"blocked": ["shell"], "reason": "no shell in prod"})),
            },
        ],
        1_750_000_002_000,
    );
    let id1 = create_and_evaluate(&app, &journal_run, &v1).await;
    declare_and_commit(
        &app,
        "middleware_composition",
        "default",
        std::slice::from_ref(&id1),
    )
    .await;
    promote(&app, &journal_run, &v1, &id1, "prod").await;

    // The prod-bound run resolves v1: the journaled resolution names the
    // chain in layer order, and the manifest's `middleware` pin is the
    // journaled digest — one derivation, two addresses.
    let middleware_binding = |tag: &str| {
        binding(
            tag,
            json!([{"family": "middleware_composition", "name": "default"}]),
        )
    };
    let run_prod = run_ok(&app, "pipeline", Some(middleware_binding("prod"))).await;
    let resolved_prod = resolutions(&snapshot_of(&app, &run_prod).await);
    assert_eq!(resolved_prod.len(), 1, "one composition, one event");
    assert_eq!(resolved_prod[0]["surface"], json!("middleware:default"));
    assert_eq!(resolved_prod[0]["tag"], json!("prod"));
    assert_eq!(resolved_prod[0]["candidate_id"], json!(id1));
    assert_eq!(
        resolved_prod[0]["layers"],
        json!(["request_logger", "tool_call_blocklist"]),
        "the resolved chain is journaled in declared order"
    );
    let receipt_prod = receipt_of(&app, &run_prod).await;
    assert_eq!(
        receipt_prod["manifest"]["middleware"], resolved_prod[0]["digest"],
        "the manifest pin is the journaled resolution digest"
    );

    // v2: the logger alone — a different chain, promoted to staging only.
    let v2 = composition_candidate(
        "default",
        vec![MiddlewareLayerConfig {
            layer: "request_logger".to_owned(),
            config: None,
        }],
        1_750_000_003_000,
    );
    let id2 = create_and_evaluate(&app, &journal_run, &v2).await;
    declare_and_commit(
        &app,
        "middleware_composition",
        "default",
        std::slice::from_ref(&id2),
    )
    .await;
    promote(&app, &journal_run, &v2, &id2, "staging").await;

    // The staging-bound run pins v2's chain…
    let run_staging = run_ok(&app, "pipeline", Some(middleware_binding("staging"))).await;
    let resolved_staging = resolutions(&snapshot_of(&app, &run_staging).await);
    assert_eq!(resolved_staging.len(), 1);
    assert_eq!(resolved_staging[0]["tag"], json!("staging"));
    assert_eq!(resolved_staging[0]["candidate_id"], json!(id2));
    assert_eq!(resolved_staging[0]["layers"], json!(["request_logger"]));
    let receipt_staging = receipt_of(&app, &run_staging).await;
    assert_eq!(
        receipt_staging["manifest"]["middleware"],
        resolved_staging[0]["digest"]
    );
    assert_ne!(
        resolved_prod[0]["digest"], resolved_staging[0]["digest"],
        "the environments pin distinct chains"
    );

    // …and prod's pointer is untouched: a fresh prod-bound run re-pins
    // v1, byte-identically to the first.
    let run_prod_after = run_ok(&app, "pipeline", Some(middleware_binding("prod"))).await;
    let resolved_prod_after = resolutions(&snapshot_of(&app, &run_prod_after).await);
    assert_eq!(resolved_prod_after.len(), 1);
    assert_eq!(resolved_prod_after[0]["candidate_id"], json!(id1));
    assert_eq!(
        resolved_prod_after[0]["digest"], resolved_prod[0]["digest"],
        "the staging promotion left prod's pin untouched"
    );
    let receipt_prod_after = receipt_of(&app, &run_prod_after).await;
    assert_eq!(
        receipt_prod_after["manifest"]["middleware"], receipt_prod["manifest"]["middleware"],
        "prod's manifest pin is byte-identical across the staging promotion"
    );
    assert_eq!(
        receipt_prod_after["manifest_digest"], receipt_prod["manifest_digest"],
        "and so is the manifest's own digest"
    );

    let _ = std::fs::remove_dir_all(store);
}
