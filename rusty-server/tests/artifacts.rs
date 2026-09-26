//! The run artifact plane integration tests (R0.12 Operations Plane,
//! waves 1 and 2): the `/artifacts` surface over the default JSON-file
//! backend — the declared commit path, the journaled-spill commit path,
//! lineage resolution (run → effect → bytes), convergence and conflict
//! rules, tenant isolation, restart durability, and the fail-closed
//! reads (corruption refused, the typed miss for gone bytes); wave 2
//! adds version accumulation, derived previews, the retention sweeper,
//! the release act, and the deployment evidence chain read. Live-
//! Postgres coverage of the same semantics is the gated section at the
//! bottom (`RUSTY_TEST_DATABASE_URL`, or `DATABASE_URL` for gate parity).
//!
//! Driven in-process via `tower::ServiceExt::oneshot` (no sockets), the
//! `registry.rs` convention. R0.7 compatibility — journal artifacts and
//! snapshots deserializing unchanged — is pinned where those contracts
//! live: `rusty-core`'s journal and flight-recorder suites run
//! unchanged, and the journaled-spill test below doubles as the
//! server-side proof (it reads an R0.7-shaped artifact reference out of
//! a persisted journal).

use std::path::PathBuf;

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::artifact::ArtifactCommitment;
use rusty_agent_runtime::broker::hex_encode;
use rusty_agent_runtime::effects::derive_effect_id;
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
        "rusty-server-artifacts-test-{}",
        uuid::Uuid::new_v4()
    ))
}

/// The pipeline graph (`first -> second`, appending to a `log` channel):
/// every app here registers it so runs have a journal to commit against.
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

/// An app over `store` with the config customized by `configure`
/// (tenant keys, the Postgres backend).
fn app_with(store: PathBuf, configure: impl FnOnce(ServerConfig) -> ServerConfig) -> Router {
    let config = configure(ServerConfig::new("127.0.0.1:0".parse().unwrap(), store));
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
    let (status, _, bytes) = call_full(app, auth, method, uri, body).await;
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

/// Send a request; returns `(status, content-type, raw body)` — the
/// bytes route's harness: its body is the artifact, not JSON.
async fn call_full(
    app: &Router,
    auth: Option<(&str, &str)>,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Option<String>, Bytes) {
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
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let bytes: Bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, content_type, bytes)
}

/// Fetch raw bytes (or the error body) from a GET route.
async fn get_bytes(
    app: &Router,
    auth: Option<(&str, &str)>,
    uri: &str,
) -> (StatusCode, Option<String>, Bytes) {
    call_full(app, auth, "GET", uri, None).await
}

/// Create a thread and run it to completion; returns the run id.
async fn run_pipeline(app: &Router) -> String {
    run_pipeline_as(app, None).await
}

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

/// The run's journaled events (the Flight Recorder read).
async fn run_events(app: &Router, run_id: &str) -> Vec<Value> {
    let (status, v) = call(app, "GET", &format!("/runs/{run_id}/events"), None).await;
    assert_eq!(status, StatusCode::OK, "events failed: {v}");
    v["events"].as_array().unwrap().clone()
}

/// The id of the last journaled event (the commitment's causal parent
/// and the lineage's producing event).
fn last_event_id(events: &[Value]) -> String {
    events.last().unwrap()["id"].as_str().unwrap().to_string()
}

/// A realistic effect id, derived the way the producing node would.
fn effect_id_for(run_id: &str) -> String {
    derive_effect_id(
        run_id,
        "render_report",
        &sha256_hex(b"weekly"),
        Some("render:1"),
    )
    .to_string()
}

/// The declared-commit payload every named-commit test shares.
fn commit_payload(run_id: &str, event_id: &str, bytes: &[u8], name: Option<&str>) -> Value {
    json!({
        "bytes_hex": hex_encode(bytes),
        "name": name,
        "media_kind": "image",
        "media_type": "image/png",
        "retention": {"policy": "days", "days": 30},
        "lineage": {
            "run_id": run_id,
            "effect_id": effect_id_for(run_id),
            "event_id": event_id,
        },
    })
}

/// Commit a named artifact against a completed run; returns
/// `(artifact_id, journal_event_id)` from the 201 response.
async fn commit_named(app: &Router, run_id: &str, bytes: &[u8], name: &str) -> (String, String) {
    let events = run_events(app, run_id).await;
    let payload = commit_payload(run_id, &last_event_id(&events), bytes, Some(name));
    let (status, v) = call(app, "POST", "/artifacts/commits", Some(payload)).await;
    assert_eq!(status, StatusCode::CREATED, "commit failed: {v}");
    (
        v["artifact_id"].as_str().unwrap().to_string(),
        v["journal_event_id"].as_str().unwrap().to_string(),
    )
}

// --------------------------------------------------------------------- //
// The declared commit path and lineage (the wave's exit criteria)
// --------------------------------------------------------------------- //

#[tokio::test]
async fn named_artifact_commits_and_lineage_resolves_run_effect_bytes() {
    let (app, store) = app();
    let run_id = run_pipeline(&app).await;
    let bytes = b"\x89PNG pretend image bytes".as_slice();
    let (artifact_id, journal_event_id) = commit_named(&app, &run_id, bytes, "weekly-report").await;

    // The address is the bytes' SHA-256 — identity is integrity.
    assert_eq!(artifact_id, sha256_hex(bytes));

    // The record resolves by address: name, media, retention, the base
    // version, and the lineage join (run, effect, producing event).
    let (status, record) = call(&app, "GET", &format!("/artifacts/{artifact_id}"), None).await;
    assert_eq!(status, StatusCode::OK, "record failed: {record}");
    assert_eq!(record["name"], "weekly-report");
    assert_eq!(record["media_kind"], "image");
    assert_eq!(record["media_type"], "image/png");
    assert_eq!(record["retention"], json!({"policy": "days", "days": 30}));
    assert_eq!(record["lineage"]["run_id"], run_id);
    assert_eq!(record["lineage"]["effect_id"], effect_id_for(&run_id));
    assert_eq!(record["versions"].as_array().unwrap().len(), 1);
    assert_eq!(record["versions"][0]["sha256"], artifact_id);
    let producing_event = record["lineage"]["event_id"].as_str().unwrap().to_string();

    // The name index resolves to the same record.
    let (status, by_name) = call(&app, "GET", "/artifacts/names/weekly-report", None).await;
    assert_eq!(status, StatusCode::OK, "by-name failed: {by_name}");
    assert_eq!(by_name["artifact_id"], artifact_id);

    // The version history reads the base sequence.
    let (status, versions) =
        call(&app, "GET", "/artifacts/names/weekly-report/versions", None).await;
    assert_eq!(status, StatusCode::OK, "versions failed: {versions}");
    assert_eq!(versions["current"], artifact_id);
    assert_eq!(versions["versions"].as_array().unwrap().len(), 1);

    // The bytes round-trip with the declared media type.
    let (status, content_type, body) =
        get_bytes(&app, None, &format!("/artifacts/{artifact_id}/bytes")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type.as_deref(), Some("image/png"));
    assert_eq!(body.as_ref(), bytes);

    // The journaled commitment is the audit walk's first hop: the run's
    // journal now carries one `artifact_committed` event, parented to
    // the run's prior head, whose payload walks to the producing effect
    // and the address.
    let events = run_events(&app, &run_id).await;
    let commits: Vec<&Value> = events
        .iter()
        .filter(|event| event["kind"] == "artifact_committed")
        .collect();
    assert_eq!(commits.len(), 1);
    assert_eq!(commits[0]["id"], journal_event_id);
    assert_eq!(commits[0]["parent"], json!(producing_event));
    let payload = &commits[0]["output"]["value"];
    let commitment: ArtifactCommitment = serde_json::from_value(payload.clone()).unwrap();
    assert_eq!(commitment.artifact_id, artifact_id);
    assert_eq!(commitment.name.as_deref(), Some("weekly-report"));
    assert_eq!(commitment.version, Some(0));
    assert_eq!(commitment.effect_id.as_str(), effect_id_for(&run_id));
    assert_eq!(commitment.bytes, bytes.len() as u64);

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn unnamed_commit_uses_the_sparse_wire_and_default_retention() {
    let (app, store) = app();
    let run_id = run_pipeline(&app).await;
    let bytes = b"plain text export".as_slice();
    let events = run_events(&app, &run_id).await;
    let payload = json!({
        "bytes_hex": hex_encode(bytes),
        "media_kind": "file",
        "lineage": {
            "run_id": run_id,
            "effect_id": effect_id_for(&run_id),
            "event_id": last_event_id(&events),
        },
    });
    let (status, v) = call(&app, "POST", "/artifacts/commits", Some(payload)).await;
    assert_eq!(status, StatusCode::CREATED, "commit failed: {v}");
    let artifact_id = v["artifact_id"].as_str().unwrap().to_string();

    // The sparse wire: `name`, `media_type`, and `versions` are absent
    // (not null), and the retention defaults to `receipt_bound`.
    let (status, record) = call(&app, "GET", &format!("/artifacts/{artifact_id}"), None).await;
    assert_eq!(status, StatusCode::OK, "record failed: {record}");
    assert!(record.get("name").is_none());
    assert!(record.get("media_type").is_none());
    assert!(record.get("versions").is_none());
    assert_eq!(record["retention"], json!({"policy": "receipt_bound"}));

    // An unnamed artifact has no name-index entry and no bytes
    // content-type claim beyond octet-stream.
    let (status, _) = call(&app, "GET", "/artifacts/names/weekly-report", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, content_type, _) =
        get_bytes(&app, None, &format!("/artifacts/{artifact_id}/bytes")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type.as_deref(), Some("application/octet-stream"));

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn identical_recommit_converges_without_a_second_journal_event() {
    let (app, store) = app();
    let run_id = run_pipeline(&app).await;
    let bytes = b"same bytes twice".as_slice();
    let (artifact_id, _) = commit_named(&app, &run_id, bytes, "weekly-report").await;

    let events = run_events(&app, &run_id).await;
    let payload = commit_payload(
        &run_id,
        &last_event_id(&events),
        bytes,
        Some("weekly-report"),
    );
    let (status, v) = call(&app, "POST", "/artifacts/commits", Some(payload)).await;
    assert_eq!(status, StatusCode::OK, "recommit failed: {v}");
    assert_eq!(v["created"], false);
    assert_eq!(v["artifact_id"], artifact_id);

    // Convergence journals nothing: still exactly one commitment event.
    let events = run_events(&app, &run_id).await;
    let commits = events
        .iter()
        .filter(|event| event["kind"] == "artifact_committed")
        .count();
    assert_eq!(commits, 1);

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn conflicting_names_and_bytes_answer_409() {
    let (app, store) = app();
    let run_id = run_pipeline(&app).await;
    let events = run_events(&app, &run_id).await;
    let event_id = last_event_id(&events);
    let bytes = b"first bytes".as_slice();
    commit_named(&app, &run_id, bytes, "weekly-report").await;

    // Same bytes under a different name: one object carries one logical
    // name.
    let payload = commit_payload(&run_id, &event_id, bytes, Some("monthly-report"));
    let (status, v) = call(&app, "POST", "/artifacts/commits", Some(payload)).await;
    assert_eq!(status, StatusCode::CONFLICT, "expected 409: {v}");

    // A different output under the taken name is a version, not a
    // conflict (wave 2): the new head joins the sequence and journals.
    let payload = commit_payload(
        &run_id,
        &event_id,
        b"different bytes",
        Some("weekly-report"),
    );
    let (status, v) = call(&app, "POST", "/artifacts/commits", Some(payload)).await;
    assert_eq!(status, StatusCode::CREATED, "expected 201: {v}");
    assert_eq!(v["commitment"]["version"], json!(1));
    assert_eq!(v["artifact"]["versions"].as_array().unwrap().len(), 2);

    // The conflict journaled nothing; the version did: the base commit
    // plus the version append, exactly two commitment events.
    let events = run_events(&app, &run_id).await;
    let commits = events
        .iter()
        .filter(|event| event["kind"] == "artifact_committed")
        .count();
    assert_eq!(commits, 2);

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn malformed_commits_are_refused_and_journal_nothing() {
    let (app, store) = app();
    let run_id = run_pipeline(&app).await;
    let events = run_events(&app, &run_id).await;
    let event_id = last_event_id(&events);
    let base = commit_payload(&run_id, &event_id, b"bytes", Some("weekly-report"));

    // Bad hex.
    let mut payload = base.clone();
    payload["bytes_hex"] = json!("not hex!");
    let (status, _) = call(&app, "POST", "/artifacts/commits", Some(payload)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // A name outside the naming rules.
    let mut payload = base.clone();
    payload["name"] = json!("tenant/escape");
    let (status, v) = call(&app, "POST", "/artifacts/commits", Some(payload)).await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "expected 422: {v}"
    );

    // An effect id outside the derived-digest shape.
    let mut payload = base.clone();
    payload["lineage"]["effect_id"] = json!("made-up");
    let (status, v) = call(&app, "POST", "/artifacts/commits", Some(payload)).await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "expected 422: {v}"
    );

    // Nothing reached the plane or the journal.
    let (status, v) = call(&app, "GET", "/artifacts", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["artifacts"].as_array().unwrap().len(), 0);
    let events = run_events(&app, &run_id).await;
    assert_eq!(
        events
            .iter()
            .filter(|event| event["kind"] == "artifact_committed")
            .count(),
        0
    );

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// The journaled-spill commit path
// --------------------------------------------------------------------- //

/// A graph whose node emits an output larger than
/// `INLINE_PAYLOAD_MAX_BYTES` (4 KB): the executor journals it as a
/// `PayloadRef::Artifact`, the R0.7 spill the second commit path reads.
fn big_output_registry() -> GraphRegistry {
    use rusty_agent_runtime::prelude::*;
    let spec = StateSpec::new().channel("blob", Reducer::Overwrite);
    let mut builder = GraphBuilder::new();
    builder.add_node("produce", |_ctx: NodeContext| async {
        Ok(NodeOutput::update(
            "blob",
            json!({"rows": "x".repeat(8 * 1024)}),
        ))
    });
    builder.set_entry_point("produce");
    let mut registry = GraphRegistry::new();
    registry.register("producer", builder.compile().unwrap(), spec);
    registry
}

#[tokio::test]
async fn journaled_spill_commits_the_runs_own_bytes() {
    let store = temp_store();
    let app = app_with(store.clone(), |config| config);
    // The producer graph is not in the default registry here; register
    // it by building the app over the producer registry directly.
    drop(app);
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone());
    let app = router(big_output_registry(), config);

    let (status, v) = call(&app, "POST", "/threads", Some(json!({"graph": "producer"}))).await;
    assert_eq!(status, StatusCode::CREATED, "thread failed: {v}");
    let thread_id = v["thread_id"].as_str().unwrap().to_string();
    let (status, v) = call(
        &app,
        "POST",
        &format!("/threads/{thread_id}/runs/wait"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "run failed: {v}");
    let run_id = v["run_id"].as_str().unwrap().to_string();

    // Find the node output event whose payload spilled: the reference
    // is the R0.7 wire shape this path commits from.
    let events = run_events(&app, &run_id).await;
    let spilled = events
        .iter()
        .find(|event| {
            event["kind"] == "node_output" && event["output"]["kind"] == json!("artifact")
        })
        .expect("the oversized node output must spill to an artifact reference");
    let event_id = spilled["id"].as_str().unwrap().to_string();
    let reference = spilled["output"]["value"].clone();
    let expected_bytes = serde_json::to_vec(&json!({
        "updates": {"blob": {"rows": "x".repeat(8 * 1024)}},
        "command": null,
    }))
    .unwrap();
    assert_eq!(reference["sha256"], json!(sha256_hex(&expected_bytes)));

    // Commit the spill: the bytes come from the run's own journal, so
    // what the plane stores is exactly what the run produced.
    let (status, v) = call(
        &app,
        "POST",
        "/artifacts/spills",
        Some(json!({
            "run_id": run_id,
            "event_id": event_id,
            "effect_id": effect_id_for(&run_id),
            "name": "dataset-export",
            "media_kind": "data",
            "media_type": "application/json",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "spill commit failed: {v}");
    let artifact_id = v["artifact_id"].as_str().unwrap().to_string();
    assert_eq!(artifact_id, reference["sha256"].as_str().unwrap());

    let (status, _, body) = get_bytes(&app, None, &format!("/artifacts/{artifact_id}/bytes")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_ref(), expected_bytes.as_slice());

    // Lineage points at the producing event; the commitment journaled
    // into the same run.
    let (status, record) = call(&app, "GET", &format!("/artifacts/{artifact_id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(record["lineage"]["event_id"], event_id);
    let events = run_events(&app, &run_id).await;
    assert_eq!(
        events
            .iter()
            .filter(|event| event["kind"] == "artifact_committed")
            .count(),
        1
    );

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn spill_commits_refuse_unspilled_outputs_and_unknown_events() {
    let (app, store) = app();
    let run_id = run_pipeline(&app).await;
    let events = run_events(&app, &run_id).await;
    // The pipeline's outputs are small — every node output is inline.
    let inline_event = events
        .iter()
        .find(|event| event["kind"] == "node_output")
        .expect("the pipeline journals node outputs");
    let inline_id = inline_event["id"].as_str().unwrap().to_string();
    assert_eq!(inline_event["output"]["kind"], json!("inline"));

    let spill_payload = |event_id: &str| {
        json!({
            "run_id": run_id,
            "event_id": event_id,
            "effect_id": effect_id_for(&run_id),
            "media_kind": "file",
        })
    };

    // An inline (never spilled) output is not committable through this
    // path.
    let (status, v) = call(
        &app,
        "POST",
        "/artifacts/spills",
        Some(spill_payload(&inline_id)),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "expected 422: {v}"
    );

    // An event the run never journaled.
    let (status, v) = call(
        &app,
        "POST",
        "/artifacts/spills",
        Some(spill_payload(&format!("{run_id}:999"))),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "expected 422: {v}"
    );

    // An unknown run.
    let mut payload = spill_payload(&inline_id);
    payload["run_id"] = json!("run-nope");
    payload["event_id"] = json!("run-nope:1");
    let (status, v) = call(&app, "POST", "/artifacts/spills", Some(payload)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "expected 404: {v}");

    // Nothing committed.
    let (_, v) = call(&app, "GET", "/artifacts", None).await;
    assert_eq!(v["artifacts"].as_array().unwrap().len(), 0);

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Tenant isolation and the journal-first gate
// --------------------------------------------------------------------- //

#[tokio::test]
async fn unknown_run_fails_closed_and_commits_nothing() {
    let (app, store) = app();
    let payload = json!({
        "bytes_hex": hex_encode(b"orphan bytes"),
        "media_kind": "file",
        "lineage": {
            "run_id": "run-that-never-was",
            "effect_id": effect_id_for("run-that-never-was"),
            "event_id": "run-that-never-was:0",
        },
    });
    // The journal-first rule: a commit that cannot journal its event
    // does not persist the record — and here it never even puts bytes.
    let (status, v) = call(&app, "POST", "/artifacts/commits", Some(payload)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "expected 404: {v}");
    let (_, v) = call(&app, "GET", "/artifacts", None).await;
    assert_eq!(v["artifacts"].as_array().unwrap().len(), 0);

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn cross_tenant_access_answers_404_never_403() {
    let store = temp_store();
    let app = app_with(store.clone(), |config| {
        config
            .with_tenant_key("acme", "acme-secret")
            .with_tenant_key("globex", "globex-secret")
    });
    let acme = Some(("x-api-key", "acme-secret"));
    let globex = Some(("x-api-key", "globex-secret"));

    let run_id = run_pipeline_as(&app, acme).await;
    let (status, v) = call(&app, "GET", &format!("/runs/{run_id}/events"), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "auth is required: {v}");
    let events = {
        let (status, v) = call_as(&app, acme, "GET", &format!("/runs/{run_id}/events"), None).await;
        assert_eq!(status, StatusCode::OK, "events failed: {v}");
        v["events"].as_array().unwrap().clone()
    };
    let event_id = last_event_id(&events);
    let bytes = b"acme's quarterly numbers".as_slice();
    let payload = commit_payload(&run_id, &event_id, bytes, Some("quarterly"));
    let (status, v) = call_as(&app, acme, "POST", "/artifacts/commits", Some(payload)).await;
    assert_eq!(status, StatusCode::CREATED, "commit failed: {v}");
    let artifact_id = v["artifact_id"].as_str().unwrap().to_string();

    // Globex cannot read the record, the bytes, the name, or see the
    // artifact in listings — all 404/empty, never 403.
    let (status, _) = call_as(
        &app,
        globex,
        "GET",
        &format!("/artifacts/{artifact_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = get_bytes(&app, globex, &format!("/artifacts/{artifact_id}/bytes")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call_as(&app, globex, "GET", "/artifacts/names/quarterly", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, v) = call_as(&app, globex, "GET", "/artifacts", None).await;
    assert_eq!(v["artifacts"].as_array().unwrap().len(), 0);

    // Globex cannot commit against acme's run (the journal's thread
    // does not resolve in globex's namespace).
    let payload = commit_payload(&run_id, &event_id, b"forged", None);
    let (status, v) = call_as(&app, globex, "POST", "/artifacts/commits", Some(payload)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "expected 404: {v}");

    // Acme still resolves everything (the shared address grants no
    // cross-tenant read path, but the owner is unaffected).
    let (status, _) = call_as(
        &app,
        acme,
        "GET",
        &format!("/artifacts/{artifact_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, body) = get_bytes(&app, acme, &format!("/artifacts/{artifact_id}/bytes")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_ref(), bytes);

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Restart durability and the fail-closed reads
// --------------------------------------------------------------------- //

#[tokio::test]
async fn restart_preserves_records_versions_and_bytes() {
    let store = temp_store();
    let first = app_with(store.clone(), |config| config);
    let run_id = run_pipeline(&first).await;
    let named_bytes = b"restart me named".as_slice();
    let unnamed_bytes = b"restart me unnamed".as_slice();
    let (named_id, _) = commit_named(&first, &run_id, named_bytes, "weekly-report").await;
    let events = run_events(&first, &run_id).await;
    let payload = json!({
        "bytes_hex": hex_encode(unnamed_bytes),
        "media_kind": "file",
        "lineage": {
            "run_id": run_id,
            "effect_id": effect_id_for(&run_id),
            "event_id": last_event_id(&events),
        },
    });
    let (status, v) = call(&first, "POST", "/artifacts/commits", Some(payload)).await;
    assert_eq!(status, StatusCode::CREATED, "unnamed commit failed: {v}");
    let unnamed_id = v["artifact_id"].as_str().unwrap().to_string();
    drop(first);

    // The restart: a fresh app over the same store root reloads every
    // record, the name index, and the bytes.
    let second = app_with(store.clone(), |config| config);
    let (status, record) = call(&second, "GET", &format!("/artifacts/{named_id}"), None).await;
    assert_eq!(status, StatusCode::OK, "named record lost: {record}");
    assert_eq!(record["name"], "weekly-report");
    assert_eq!(record["versions"].as_array().unwrap().len(), 1);
    let (status, by_name) = call(&second, "GET", "/artifacts/names/weekly-report", None).await;
    assert_eq!(status, StatusCode::OK, "name index lost: {by_name}");
    assert_eq!(by_name["artifact_id"], named_id);
    let (status, versions) = call(
        &second,
        "GET",
        "/artifacts/names/weekly-report/versions",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "versions lost: {versions}");
    assert_eq!(versions["versions"].as_array().unwrap().len(), 1);
    let (status, _, body) = get_bytes(&second, None, &format!("/artifacts/{named_id}/bytes")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_ref(), named_bytes);
    let (status, _, body) =
        get_bytes(&second, None, &format!("/artifacts/{unnamed_id}/bytes")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_ref(), unnamed_bytes);

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn integrity_failure_is_refused_as_corruption_never_served() {
    let (app, store) = app();
    let run_id = run_pipeline(&app).await;
    let bytes = b"integrity matters".as_slice();
    let (artifact_id, _) = commit_named(&app, &run_id, bytes, "weekly-report").await;

    // Corrupt the blob on disk (same length, flipped content): the read
    // re-hashes before it serves, so the flip is detected.
    let blob = store.join("artifacts").join("blobs").join(&artifact_id);
    let mut corrupted = std::fs::read(&blob).unwrap();
    corrupted[0] ^= 0xff;
    std::fs::write(&blob, &corrupted).unwrap();

    let (status, _, body) = get_bytes(&app, None, &format!("/artifacts/{artifact_id}/bytes")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let error: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(error["error"], "artifact_corrupt");

    // The record itself still serves (metadata is intact) — the
    // corruption is in the bytes, and the plane says which.
    let (status, _) = call(&app, "GET", &format!("/artifacts/{artifact_id}"), None).await;
    assert_eq!(status, StatusCode::OK);

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn missing_bytes_answer_the_typed_miss_not_a_404() {
    let (app, store) = app();
    let run_id = run_pipeline(&app).await;
    let bytes = b"gone bytes".as_slice();
    let (artifact_id, _) = commit_named(&app, &run_id, bytes, "weekly-report").await;

    // The record lives, the bytes do not — the difference a retention
    // audit reads, so the miss is typed, not a 404.
    let blob = store.join("artifacts").join("blobs").join(&artifact_id);
    std::fs::remove_file(&blob).unwrap();

    let (status, _, body) = get_bytes(&app, None, &format!("/artifacts/{artifact_id}/bytes")).await;
    assert_eq!(status, StatusCode::GONE);
    let error: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(error["error"], "artifact_unavailable");

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// The listing's filters
// --------------------------------------------------------------------- //

#[tokio::test]
async fn list_filters_by_name_media_kind_and_run() {
    let (app, store) = app();
    let run_a = run_pipeline(&app).await;
    let run_b = run_pipeline(&app).await;
    let events_a = run_events(&app, &run_a).await;
    let events_b = run_events(&app, &run_b).await;
    let event_a = last_event_id(&events_a);
    let event_b = last_event_id(&events_b);

    let mut image = commit_payload(&run_a, &event_a, b"image one", Some("hero-image"));
    let (_, v) = call(&app, "POST", "/artifacts/commits", Some(image.clone())).await;
    let image_id = v["artifact_id"].as_str().unwrap().to_string();
    image["name"] = json!(null);
    let file_payload = json!({
        "bytes_hex": hex_encode(b"text two"),
        "media_kind": "file",
        "lineage": {
            "run_id": run_b,
            "effect_id": effect_id_for(&run_b),
            "event_id": event_b,
        },
    });
    let (status, v) = call(
        &app,
        "POST",
        "/artifacts/commits",
        Some(file_payload.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "second commit failed: {v}");
    let file_id = v["artifact_id"].as_str().unwrap().to_string();

    // Unfiltered: both, sorted by address.
    let (_, v) = call(&app, "GET", "/artifacts", None).await;
    let ids: Vec<&str> = v["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|record| record["artifact_id"].as_str().unwrap())
        .collect();
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(ids, sorted);
    assert_eq!(ids.len(), 2);

    // By name.
    let (_, v) = call(&app, "GET", "/artifacts?name=hero-image", None).await;
    let artifacts = v["artifacts"].as_array().unwrap();
    assert_eq!(artifacts.len(), 1);
    assert_eq!(artifacts[0]["artifact_id"], image_id);

    // By media kind.
    let (_, v) = call(&app, "GET", "/artifacts?media_kind=file", None).await;
    let artifacts = v["artifacts"].as_array().unwrap();
    assert_eq!(artifacts.len(), 1);
    assert_eq!(artifacts[0]["artifact_id"], file_id);
    let (status, _) = call(&app, "GET", "/artifacts?media_kind=hologram", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // By producing run (the lineage join).
    let (_, v) = call(&app, "GET", &format!("/artifacts?run_id={run_b}"), None).await;
    let artifacts = v["artifacts"].as_array().unwrap();
    assert_eq!(artifacts.len(), 1);
    assert_eq!(artifacts[0]["artifact_id"], file_id);

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Wave 2: versions, previews, the sweeper, the release act, and the
// deployment evidence chain
// --------------------------------------------------------------------- //

/// A commit payload with explicit media and retention — the wave-1
/// helper is pinned to image/png + days(30); wave 2 commits text, JSON,
/// BMP, WAV, pinned, and receipt-bound bytes.
fn commit_payload_full(
    run_id: &str,
    event_id: &str,
    bytes: &[u8],
    name: Option<&str>,
    media_kind: &str,
    media_type: Option<&str>,
    retention: Option<Value>,
) -> Value {
    json!({
        "bytes_hex": hex_encode(bytes),
        "name": name,
        "media_kind": media_kind,
        "media_type": media_type,
        "retention": retention,
        "lineage": {
            "run_id": run_id,
            "effect_id": effect_id_for(run_id),
            "event_id": event_id,
        },
    })
}

/// Commit through the full payload; returns the 201 response body.
async fn commit_full(app: &Router, payload: Value) -> Value {
    let (status, v) = call(app, "POST", "/artifacts/commits", Some(payload)).await;
    assert_eq!(status, StatusCode::CREATED, "commit failed: {v}");
    v
}

/// A 4×2 24-bit `BI_RGB` BMP: top row red, bottom row blue (stored
/// bottom-up) — the dependency-free image decodable, mirrored from
/// core's preview fixture.
fn tiny_bmp() -> Vec<u8> {
    let stride = 12; // (4 px * 3 B) padded to a 4-byte boundary
    let mut bmp = Vec::new();
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&(54u32 + 2 * stride as u32).to_le_bytes()); // file size
    bmp.extend_from_slice(&0u32.to_le_bytes()); // reserved
    bmp.extend_from_slice(&54u32.to_le_bytes()); // data offset
    bmp.extend_from_slice(&40u32.to_le_bytes()); // DIB size
    bmp.extend_from_slice(&4u32.to_le_bytes()); // width
    bmp.extend_from_slice(&2i32.to_le_bytes()); // height (bottom-up)
    bmp.extend_from_slice(&1u16.to_le_bytes()); // planes
    bmp.extend_from_slice(&24u16.to_le_bytes()); // bpp
    bmp.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    bmp.extend_from_slice(&(2 * stride as u32).to_le_bytes()); // image size
    bmp.extend_from_slice(&0u32.to_le_bytes()); // x ppm
    bmp.extend_from_slice(&0u32.to_le_bytes()); // y ppm
    bmp.extend_from_slice(&0u32.to_le_bytes()); // palette colors
    bmp.extend_from_slice(&0u32.to_le_bytes()); // important colors
    for _ in 0..4 {
        bmp.extend_from_slice(&[255, 0, 0]); // bottom row: blue (BGR)
    }
    for _ in 0..4 {
        bmp.extend_from_slice(&[0, 0, 255]); // top row: red (BGR)
    }
    bmp
}

/// A 16-sample mono 8-bit PCM WAV at 8000 Hz, ramping — the
/// dependency-free audio decodable, mirrored from core's fixture.
fn tiny_wav() -> Vec<u8> {
    let samples: Vec<u8> = (0..16).map(|i| i * 16).collect();
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36u32 + samples.len() as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&8000u32.to_le_bytes()); // rate
    wav.extend_from_slice(&8000u32.to_le_bytes()); // byte rate
    wav.extend_from_slice(&1u16.to_le_bytes()); // block align
    wav.extend_from_slice(&8u16.to_le_bytes()); // bits
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(samples.len() as u32).to_le_bytes());
    wav.extend_from_slice(&samples);
    wav
}

/// The deployment evidence chain's events (`GET /artifacts/journal`).
async fn chain_events(app: &Router) -> Vec<Value> {
    let (status, v) = call(app, "GET", "/artifacts/journal", None).await;
    assert_eq!(status, StatusCode::OK, "chain read failed: {v}");
    assert_eq!(v["run_id"], "run-artifacts");
    assert_eq!(v["complete"], false);
    v["events"].as_array().unwrap().clone()
}

/// The payload of every chain event of `kind`.
fn chain_payloads<'a>(events: &'a [Value], kind: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|event| event["kind"] == kind)
        .map(|event| &event["output"]["value"])
        .collect()
}

#[tokio::test]
async fn named_artifact_accumulates_three_versions_and_serves_each_by_address() {
    let store = temp_store();
    let first = app_with(store.clone(), |config| config);
    let run_id = run_pipeline(&first).await;

    // Three commits under one name: the base plus two appends, each
    // journaling its own commitment with the sequence index.
    let mut addresses = Vec::new();
    for (index, bytes) in [
        b"report v1".as_slice(),
        b"report v2".as_slice(),
        b"report v3".as_slice(),
    ]
    .into_iter()
    .enumerate()
    {
        let events = run_events(&first, &run_id).await;
        let payload = commit_payload(
            &run_id,
            &last_event_id(&events),
            bytes,
            Some("weekly-report"),
        );
        let v = commit_full(&first, payload).await;
        assert_eq!(v["commitment"]["version"], json!(index as u64));
        assert_eq!(
            v["artifact"]["versions"].as_array().unwrap().len(),
            index + 1
        );
        addresses.push(v["artifact_id"].as_str().unwrap().to_string());
    }

    // The name resolves the head; the history carries all three, oldest
    // first.
    let (status, by_name) = call(&first, "GET", "/artifacts/names/weekly-report", None).await;
    assert_eq!(status, StatusCode::OK, "by-name failed: {by_name}");
    assert_eq!(by_name["artifact_id"], json!(addresses[2]));
    let (status, versions) = call(
        &first,
        "GET",
        "/artifacts/names/weekly-report/versions",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "versions failed: {versions}");
    assert_eq!(versions["current"], json!(addresses[2]));
    let sequence = versions["versions"].as_array().unwrap();
    assert_eq!(sequence.len(), 3);
    for (index, entry) in sequence.iter().enumerate() {
        assert_eq!(entry["sha256"], json!(addresses[index]));
    }

    // Every version keeps serving by address — the old record is never
    // edited, so v1's record still carries the sequence prefix it was
    // committed under.
    for (index, bytes) in [
        b"report v1".as_slice(),
        b"report v2".as_slice(),
        b"report v3".as_slice(),
    ]
    .into_iter()
    .enumerate()
    {
        let (status, _, body) = get_bytes(
            &first,
            None,
            &format!("/artifacts/{}/bytes", addresses[index]),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "version {index} bytes failed");
        assert_eq!(body.as_ref(), bytes);
    }
    let (status, v1_record) =
        call(&first, "GET", &format!("/artifacts/{}", addresses[0]), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v1_record["versions"].as_array().unwrap().len(), 1);

    // The restart: a fresh app over the same store re-points the name at
    // the head and serves every version.
    drop(first);
    let second = app_with(store.clone(), |config| config);
    let (status, by_name) = call(&second, "GET", "/artifacts/names/weekly-report", None).await;
    assert_eq!(status, StatusCode::OK, "name lost on restart: {by_name}");
    assert_eq!(by_name["artifact_id"], json!(addresses[2]));
    assert_eq!(by_name["versions"].as_array().unwrap().len(), 3);
    let (status, _, body) =
        get_bytes(&second, None, &format!("/artifacts/{}/bytes", addresses[0])).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_ref(), b"report v1");

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn previews_derive_and_underivable_kinds_answer_empty() {
    let (app, store) = app();
    let run_id = run_pipeline(&app).await;
    // The preview subjects commit `pinned` so the sweep at the end
    // prunes exactly the one `days(0)` artifact it is meant to.

    // BMP → a real thumbnail with the source dims and P6 PPM pixels.
    let events = run_events(&app, &run_id).await;
    let event_id = last_event_id(&events);
    let bmp_id = commit_full(
        &app,
        commit_payload_full(
            &run_id,
            &event_id,
            &tiny_bmp(),
            None,
            "image",
            Some("image/bmp"),
            Some(json!({"policy": "pinned"})),
        ),
    )
    .await["artifact_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, v) = call(&app, "GET", &format!("/artifacts/{bmp_id}/preview"), None).await;
    assert_eq!(status, StatusCode::OK, "preview failed: {v}");
    let preview = &v["preview"];
    assert_eq!(preview["kind"], "image");
    assert_eq!(preview["format"], "bmp");
    assert_eq!(preview["width"], 4);
    assert_eq!(preview["height"], 2);
    assert_eq!(preview["thumb_width"], 4);
    assert_eq!(preview["thumb_height"], 2);
    let ppm_hex = preview["pixels_ppm_hex"].as_str().unwrap();
    assert!(
        ppm_hex.starts_with(&hex_encode(b"P6\n4 2\n255\n")),
        "the thumbnail is a P6 PPM of the source dims: {ppm_hex}"
    );

    // Text → a bounded text window.
    let events = run_events(&app, &run_id).await;
    let event_id = last_event_id(&events);
    let text_id = commit_full(
        &app,
        commit_payload_full(
            &run_id,
            &event_id,
            b"quarterly export, plain text",
            None,
            "file",
            Some("text/plain"),
            Some(json!({"policy": "pinned"})),
        ),
    )
    .await["artifact_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, v) = call(&app, "GET", &format!("/artifacts/{text_id}/preview"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["preview"]["kind"], "text");
    assert_eq!(v["preview"]["text"], "quarterly export, plain text");
    assert_eq!(v["preview"]["truncated"], false);

    // Whole JSON inside the window → the parsed document.
    let events = run_events(&app, &run_id).await;
    let event_id = last_event_id(&events);
    let json_id = commit_full(
        &app,
        commit_payload_full(
            &run_id,
            &event_id,
            br#"{"rows":[1,2,3]}"#,
            None,
            "data",
            Some("application/json"),
            Some(json!({"policy": "pinned"})),
        ),
    )
    .await["artifact_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, v) = call(&app, "GET", &format!("/artifacts/{json_id}/preview"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["preview"]["kind"], "json");
    assert_eq!(v["preview"]["value"], json!({"rows": [1, 2, 3]}));

    // WAV → waveform metadata (duration, rate, the peak envelope).
    let events = run_events(&app, &run_id).await;
    let event_id = last_event_id(&events);
    let wav_id = commit_full(
        &app,
        commit_payload_full(
            &run_id,
            &event_id,
            &tiny_wav(),
            None,
            "audio",
            Some("audio/wav"),
            Some(json!({"policy": "pinned"})),
        ),
    )
    .await["artifact_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, v) = call(&app, "GET", &format!("/artifacts/{wav_id}/preview"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["preview"]["kind"], "audio");
    assert_eq!(v["preview"]["format"], "wav");
    assert_eq!(v["preview"]["sample_rate"], 8000);
    assert_eq!(v["preview"]["channels"], 1);
    assert_eq!(v["preview"]["peaks"].as_array().unwrap().len(), 64);

    // Compressed formats and undecodable bytes answer the honest empty —
    // a codec dependency is the measured-need seam, never a half-parse.
    let events = run_events(&app, &run_id).await;
    let event_id = last_event_id(&events);
    let png_id = commit_full(
        &app,
        commit_payload_full(
            &run_id,
            &event_id,
            b"\x89PNG pretend compressed image",
            None,
            "image",
            Some("image/png"),
            Some(json!({"policy": "pinned"})),
        ),
    )
    .await["artifact_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, v) = call(&app, "GET", &format!("/artifacts/{png_id}/preview"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["preview"]["kind"], "empty");
    assert!(v["preview"]["reason"].as_str().unwrap().contains("BMP"));

    let events = run_events(&app, &run_id).await;
    let event_id = last_event_id(&events);
    let mp3_id = commit_full(
        &app,
        commit_payload_full(
            &run_id,
            &event_id,
            b"ID3 pretend compressed audio",
            None,
            "audio",
            Some("audio/mpeg"),
            Some(json!({"policy": "pinned"})),
        ),
    )
    .await["artifact_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, v) = call(&app, "GET", &format!("/artifacts/{mp3_id}/preview"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["preview"]["kind"], "empty");

    // A pruned artifact's preview answers the typed miss (and journals
    // it on the `preview` surface).
    let events = run_events(&app, &run_id).await;
    let event_id = last_event_id(&events);
    let expired_id = commit_full(
        &app,
        commit_payload_full(
            &run_id,
            &event_id,
            b"short-lived",
            None,
            "file",
            None,
            Some(json!({"policy": "days", "days": 0})),
        ),
    )
    .await["artifact_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, report) = call(&app, "POST", "/artifacts/sweep", None).await;
    assert_eq!(status, StatusCode::OK, "sweep failed: {report}");
    assert_eq!(report["pruned"], 1);
    let (status, v) = call(
        &app,
        "GET",
        &format!("/artifacts/{expired_id}/preview"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::GONE, "expected 410: {v}");
    assert_eq!(v["error"], "artifact_unavailable");
    let chain = chain_events(&app).await;
    let misses = chain_payloads(&chain, "artifact_unavailable");
    assert_eq!(misses.len(), 1);
    assert_eq!(misses[0]["artifact_id"], json!(expired_id));
    assert_eq!(misses[0]["surface"], "preview");

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn sweeper_prunes_expired_and_the_replay_read_fails_closed_journaled() {
    let (app, store) = app();
    let run_id = run_pipeline(&app).await;
    let events = run_events(&app, &run_id).await;
    let bytes = b"expiring export".as_slice();
    let artifact_id = commit_full(
        &app,
        commit_payload_full(
            &run_id,
            &last_event_id(&events),
            bytes,
            Some("daily-digest"),
            "file",
            None,
            Some(json!({"policy": "days", "days": 0})),
        ),
    )
    .await["artifact_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Live before the pass.
    let (status, _, body) = get_bytes(&app, None, &format!("/artifacts/{artifact_id}/bytes")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_ref(), bytes);

    // The operator-triggered pass prunes the expired address and reports
    // it deterministically.
    let (status, report) = call(&app, "POST", "/artifacts/sweep", None).await;
    assert_eq!(status, StatusCode::OK, "sweep failed: {report}");
    assert_eq!(report["scanned"], 1);
    assert_eq!(report["pruned"], 1);
    assert_eq!(report["failed"], 0);
    assert!(report["unverifiable_receipts"]
        .as_array()
        .unwrap()
        .is_empty());

    // The prune intention journaled before the byte moved — the chain
    // carries the cause the audit reads.
    let chain = chain_events(&app).await;
    let prunes = chain_payloads(&chain, "artifact_pruned");
    assert_eq!(prunes.len(), 1);
    assert_eq!(prunes[0]["artifact_id"], json!(artifact_id));
    assert_eq!(prunes[0]["name"], "daily-digest");
    assert_eq!(prunes[0]["cause"], "expired");

    // The replay's byte read fails closed: the record is live, the bytes
    // are gone — the typed `410`, never a 404, and the miss journals
    // (this read *is* an exact replay's byte source; server-side
    // `/runs/replay` replays control flow from the journal and never
    // touches blob bytes, the design's stated split).
    let (status, _, body) = get_bytes(&app, None, &format!("/artifacts/{artifact_id}/bytes")).await;
    assert_eq!(status, StatusCode::GONE);
    let error: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(error["error"], "artifact_unavailable");
    let events = chain_events(&app).await;
    let misses = chain_payloads(&events, "artifact_unavailable");
    assert_eq!(misses.len(), 1);
    assert_eq!(misses[0]["artifact_id"], json!(artifact_id));
    assert_eq!(misses[0]["surface"], "bytes");

    // A repeat pass converges: the intention is already journaled, the
    // bytes already gone.
    let (status, report) = call(&app, "POST", "/artifacts/sweep", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(report["pruned"], 0);
    assert_eq!(report["already_gone"], 1);
    assert_eq!(
        chain_payloads(&chain_events(&app).await, "artifact_pruned").len(),
        1
    );

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn receipt_pinned_survives_sweep_and_release_is_the_only_prune() {
    let (app, store) = app();
    let run_id = run_pipeline(&app).await;
    let events = run_events(&app, &run_id).await;
    let bytes = b"receipt-bound evidence".as_slice();
    // The default retention: receipt_bound.
    let artifact_id = commit_full(
        &app,
        commit_payload_full(
            &run_id,
            &last_event_id(&events),
            bytes,
            Some("audit-report"),
            "file",
            None,
            None,
        ),
    )
    .await["artifact_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Mint the run's receipt *after* the commit: the covered events name
    // the address, so the receipt pins it.
    let (status, receipt) = call(&app, "GET", &format!("/runs/{run_id}/receipt"), None).await;
    assert_eq!(status, StatusCode::OK, "receipt mint failed: {receipt}");

    // The sweep cannot prune a receipt-pinned address.
    let (status, report) = call(&app, "POST", "/artifacts/sweep", None).await;
    assert_eq!(status, StatusCode::OK, "sweep failed: {report}");
    assert_eq!(report["pruned"], 0);
    assert_eq!(report["protected"], 1);
    let (status, _, body) = get_bytes(&app, None, &format!("/artifacts/{artifact_id}/bytes")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_ref(), bytes);

    // The release is the only path that prunes it: journaled with the
    // operator's name, then the prune tail deletes the bytes.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/artifacts/{artifact_id}/release"),
        Some(json!({"released_by": "human:test", "reason": "retention review closed"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "release failed: {v}");
    assert_eq!(v["released"], true);
    assert_eq!(v["converged"], false);
    assert_eq!(v["pruned"], true);
    let event_id = v["journal_event_id"].as_str().unwrap().to_string();

    let (status, _, body) = get_bytes(&app, None, &format!("/artifacts/{artifact_id}/bytes")).await;
    assert_eq!(status, StatusCode::GONE);
    let error: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(error["error"], "artifact_unavailable");

    // The chain carries the release (with the operator's identity and
    // reason), the prune intention, and the miss — on the deployment
    // chain, never the producing run's receipt-covered journal.
    let events = chain_events(&app).await;
    let releases = chain_payloads(&events, "artifact_retention_released");
    assert_eq!(releases.len(), 1);
    assert_eq!(releases[0]["artifact_id"], json!(artifact_id));
    assert_eq!(releases[0]["released_by"], "human:test");
    assert_eq!(releases[0]["reason"], "retention review closed");
    let prunes = chain_payloads(&events, "artifact_pruned");
    assert_eq!(prunes.len(), 1);
    assert_eq!(prunes[0]["cause"], "released");
    assert_eq!(chain_payloads(&events, "artifact_unavailable").len(), 1);
    let run_journal = run_events(&app, &run_id).await;
    for kind in [
        "artifact_retention_released",
        "artifact_pruned",
        "artifact_unavailable",
    ] {
        assert!(
            run_journal.iter().all(|event| event["kind"] != kind),
            "retention acts never join the producing run's journal ({kind})"
        );
    }

    // A repeat release converges on the first act's event.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/artifacts/{artifact_id}/release"),
        Some(json!({"released_by": "human:test"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "repeat release failed: {v}");
    assert_eq!(v["converged"], true);
    assert_eq!(v["journal_event_id"], json!(event_id));
    assert_eq!(
        chain_payloads(&chain_events(&app).await, "artifact_retention_released").len(),
        1,
        "the repeat journaled nothing"
    );

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn pinned_retention_survives_sweep_until_the_release_act() {
    let (app, store) = app();
    let run_id = run_pipeline(&app).await;
    let events = run_events(&app, &run_id).await;
    let bytes = b"pinned forever, until a human says otherwise".as_slice();
    let artifact_id = commit_full(
        &app,
        commit_payload_full(
            &run_id,
            &last_event_id(&events),
            bytes,
            Some("golden-master"),
            "file",
            None,
            Some(json!({"policy": "pinned"})),
        ),
    )
    .await["artifact_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Pinned is protected no matter the clock or the coverage.
    let (status, report) = call(&app, "POST", "/artifacts/sweep", None).await;
    assert_eq!(status, StatusCode::OK, "sweep failed: {report}");
    assert_eq!(report["pruned"], 0);
    assert_eq!(report["protected"], 1);
    let (status, _, _) = get_bytes(&app, None, &format!("/artifacts/{artifact_id}/bytes")).await;
    assert_eq!(status, StatusCode::OK);

    // The release act is the way out — a governance decision with a name
    // on it, never housekeeping.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/artifacts/{artifact_id}/release"),
        Some(json!({"released_by": "human:ops-lead"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "release failed: {v}");
    assert_eq!(v["pruned"], true);
    let (status, _, _) = get_bytes(&app, None, &format!("/artifacts/{artifact_id}/bytes")).await;
    assert_eq!(status, StatusCode::GONE);

    // An empty operator identity is refused — the act carries a name.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/artifacts/{artifact_id}/release"),
        Some(json!({"released_by": "  "})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "expected 422: {v}"
    );

    // An unknown address 404s (unknown and cross-tenant are
    // indistinguishable).
    let (status, _) = call(
        &app,
        "POST",
        &format!("/artifacts/{}/release", "0".repeat(64)),
        Some(json!({"released_by": "human:ops-lead"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Gated on `RUSTY_TEST_DATABASE_URL`; every test is `#[ignore]` so the
// default suite stays green without a database (the `postgres_*.rs`
// convention). Every run uses a dedicated tenant, so repeated runs
// against one scratch database never interfere; the database itself is
// throwaway.
// --------------------------------------------------------------------- //

#[cfg(feature = "postgres")]
mod postgres {
    use super::*;

    fn pg_url() -> Option<String> {
        // `RUSTY_TEST_DATABASE_URL` is the repo convention; `DATABASE_URL`
        // is honored too so the gate's single env var drives every suite.
        std::env::var("RUSTY_TEST_DATABASE_URL")
            .ok()
            .or_else(|| std::env::var("DATABASE_URL").ok())
    }

    /// Wave-1 exit criteria on Postgres: an effect commits a named
    /// artifact; lineage resolves run → effect → bytes; a reconnect
    /// (the restart) preserves the record, the name index, the version
    /// sequence, and the bytes.
    #[tokio::test]
    #[ignore = "requires RUSTY_TEST_DATABASE_URL (scratch Postgres)"]
    async fn postgres_named_artifact_commits_and_survives_reconnect() {
        let Some(url) = pg_url() else {
            eprintln!("RUSTY_TEST_DATABASE_URL unset; skipping");
            return;
        };
        let tenant = format!("artifactpg-{}", uuid::Uuid::new_v4());
        let auth = Some(("x-api-key", "pg-secret"));
        let build = || {
            app_with(temp_store(), |config| {
                config
                    .with_postgres(url.clone())
                    .with_tenant_key(tenant.clone(), "pg-secret")
            })
        };

        let first = build();
        let run_id = run_pipeline_as(&first, auth).await;
        let events = {
            let (status, v) =
                call_as(&first, auth, "GET", &format!("/runs/{run_id}/events"), None).await;
            assert_eq!(status, StatusCode::OK, "events failed: {v}");
            v["events"].as_array().unwrap().clone()
        };
        let bytes = b"pg image bytes".as_slice();
        let payload = commit_payload(&run_id, &last_event_id(&events), bytes, Some("pg-report"));
        let (status, v) = call_as(&first, auth, "POST", "/artifacts/commits", Some(payload)).await;
        assert_eq!(status, StatusCode::CREATED, "commit failed: {v}");
        let artifact_id = v["artifact_id"].as_str().unwrap().to_string();
        assert_eq!(artifact_id, sha256_hex(bytes));

        // Lineage resolves on the live app.
        let (status, record) = call_as(
            &first,
            auth,
            "GET",
            &format!("/artifacts/{artifact_id}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "record failed: {record}");
        assert_eq!(record["lineage"]["run_id"], run_id);
        assert_eq!(record["lineage"]["effect_id"], effect_id_for(&run_id));
        let (status, _, body) =
            get_bytes(&first, auth, &format!("/artifacts/{artifact_id}/bytes")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.as_ref(), bytes);
        drop(first);

        // The reconnect: a fresh store instance over the same database
        // serves the settled plane.
        let second = build();
        let (status, record) = call_as(
            &second,
            auth,
            "GET",
            &format!("/artifacts/{artifact_id}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "record lost on reconnect: {record}");
        assert_eq!(record["versions"].as_array().unwrap().len(), 1);
        let (status, by_name) =
            call_as(&second, auth, "GET", "/artifacts/names/pg-report", None).await;
        assert_eq!(status, StatusCode::OK, "name index lost: {by_name}");
        assert_eq!(by_name["artifact_id"], artifact_id);
        let (status, _, body) =
            get_bytes(&second, auth, &format!("/artifacts/{artifact_id}/bytes")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.as_ref(), bytes);

        // The journaled commitment survived too — the walk's first hop.
        let (status, v) = call_as(
            &second,
            auth,
            "GET",
            &format!("/runs/{run_id}/events"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            v["events"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|event| event["kind"] == "artifact_committed")
                .count(),
            1
        );
    }

    /// The metadata table holds no byte payload (the design's asymmetry,
    /// asserted by reading the raw row), and a corrupted blob is refused
    /// as corruption on read — the fail-closed rule, exact on Postgres.
    #[tokio::test]
    #[ignore = "requires RUSTY_TEST_DATABASE_URL (scratch Postgres)"]
    async fn postgres_metadata_holds_no_bytes_and_corruption_is_refused() {
        use sqlx::Row;

        let Some(url) = pg_url() else {
            eprintln!("RUSTY_TEST_DATABASE_URL unset; skipping");
            return;
        };
        let tenant = format!("artifactpg-{}", uuid::Uuid::new_v4());
        let auth = Some(("x-api-key", "pg-secret"));
        let app = app_with(temp_store(), |config| {
            config
                .with_postgres(url.clone())
                .with_tenant_key(tenant.clone(), "pg-secret")
        });

        let run_id = run_pipeline_as(&app, auth).await;
        let bytes = b"pg bytes about to be corrupted".as_slice();
        let events = {
            let (status, v) =
                call_as(&app, auth, "GET", &format!("/runs/{run_id}/events"), None).await;
            assert_eq!(status, StatusCode::OK, "events failed: {v}");
            v["events"].as_array().unwrap().clone()
        };
        let payload = commit_payload(&run_id, &last_event_id(&events), bytes, Some("pg-bytes"));
        let (status, v) = call_as(&app, auth, "POST", "/artifacts/commits", Some(payload)).await;
        assert_eq!(status, StatusCode::CREATED, "commit failed: {v}");
        let artifact_id = v["artifact_id"].as_str().unwrap().to_string();

        let pool = sqlx::PgPool::connect(&url).await.unwrap();
        // The raw metadata row: the payload is the record only — no
        // bytes column, no byte material in the JSONB.
        let row = sqlx::query(
            "SELECT name, media_kind, run_id, payload FROM server_run_artifacts \
             WHERE artifact_key = $1",
        )
        .bind(format!("{tenant}/{artifact_id}"))
        .fetch_one(&pool)
        .await
        .unwrap();
        let payload: Value = row.get("payload");
        assert_eq!(row.get::<String, _>("name"), "pg-bytes");
        assert_eq!(row.get::<String, _>("run_id"), run_id);
        let payload_text = payload.to_string();
        assert!(
            !payload_text.contains(&hex_encode(bytes)),
            "the metadata payload must never carry the artifact bytes"
        );

        // Corrupt the blob row, then read: refused as corruption, never
        // served.
        sqlx::query("UPDATE rusty_artifacts SET payload = $2 WHERE sha256 = $1")
            .bind(&artifact_id)
            .bind(b"corrupted bytes, same length!!".as_slice())
            .execute(&pool)
            .await
            .unwrap();
        let (status, _, body) =
            get_bytes(&app, auth, &format!("/artifacts/{artifact_id}/bytes")).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        let error: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(error["error"], "artifact_corrupt");

        // And the typed miss when the bytes are gone outright.
        sqlx::query("DELETE FROM rusty_artifacts WHERE sha256 = $1")
            .bind(&artifact_id)
            .execute(&pool)
            .await
            .unwrap();
        let (status, _, body) =
            get_bytes(&app, auth, &format!("/artifacts/{artifact_id}/bytes")).await;
        assert_eq!(status, StatusCode::GONE);
        let error: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(error["error"], "artifact_unavailable");
    }

    /// Wave-2 exit criteria on Postgres: version accumulation CAS-appends
    /// under the advisory-locked name, the sweeper protects a
    /// receipt-pinned address, the release act is the only prune, and the
    /// deployment evidence chain carries every act — all through the same
    /// routes the file backend serves.
    #[tokio::test]
    #[ignore = "requires RUSTY_TEST_DATABASE_URL (scratch Postgres)"]
    async fn postgres_versions_sweep_and_release_hold() {
        use sqlx::Row;

        let Some(url) = pg_url() else {
            eprintln!("RUSTY_TEST_DATABASE_URL unset; skipping");
            return;
        };
        let tenant = format!("artifactpg-{}", uuid::Uuid::new_v4());
        let auth = Some(("x-api-key", "pg-secret"));
        let app = app_with(temp_store(), |config| {
            config
                .with_postgres(url.clone())
                .with_tenant_key(tenant.clone(), "pg-secret")
        });

        let run_id = run_pipeline_as(&app, auth).await;

        // The bytes carry the tenant so repeated gate runs against one
        // scratch database mint distinct addresses — content addressing
        // makes byte storage global, and a re-run's identical bytes would
        // share an address another tenant's record still protects (the
        // cross-tenant rule, working as designed, would then correctly
        // answer `pruned: false`).
        let v1_bytes = format!("pg report v1 {tenant}").into_bytes();
        let v2_bytes = format!("pg report v2 {tenant}").into_bytes();

        // Version accumulation: two commits under one name — the head
        // re-points and the sequence grows to two.
        let mut addresses = Vec::new();
        for bytes in [&v1_bytes, &v2_bytes] {
            let events = {
                let (status, v) =
                    call_as(&app, auth, "GET", &format!("/runs/{run_id}/events"), None).await;
                assert_eq!(status, StatusCode::OK, "events failed: {v}");
                v["events"].as_array().unwrap().clone()
            };
            let payload = commit_payload(
                &run_id,
                &last_event_id(&events),
                bytes,
                Some("pg-versioned"),
            );
            let (status, v) =
                call_as(&app, auth, "POST", "/artifacts/commits", Some(payload)).await;
            assert_eq!(status, StatusCode::CREATED, "commit failed: {v}");
            addresses.push(v["artifact_id"].as_str().unwrap().to_string());
        }
        let (status, versions) = call_as(
            &app,
            auth,
            "GET",
            "/artifacts/names/pg-versioned/versions",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "versions failed: {versions}");
        assert_eq!(versions["current"], json!(addresses[1]));
        let sequence = versions["versions"].as_array().unwrap();
        assert_eq!(sequence.len(), 2);
        assert_eq!(sequence[0]["sha256"], json!(addresses[0]));
        assert_eq!(sequence[1]["sha256"], json!(addresses[1]));

        // The raw head row: the payload carries the full sequence (both
        // version entries) and never the byte material — the design's
        // asymmetry holds on the version path too.
        let pool = sqlx::PgPool::connect(&url).await.unwrap();
        let row = sqlx::query("SELECT payload FROM server_run_artifacts WHERE artifact_key = $1")
            .bind(format!("{tenant}/{}", addresses[1]))
            .fetch_one(&pool)
            .await
            .unwrap();
        let payload: Value = row.get("payload");
        assert_eq!(payload["versions"].as_array().unwrap().len(), 2);
        let payload_text = payload.to_string();
        for bytes in [&v1_bytes, &v2_bytes] {
            assert!(
                !payload_text.contains(&hex_encode(bytes)),
                "the metadata payload must never carry the artifact bytes"
            );
        }
        // Every version keeps serving by address.
        let (status, _, body) =
            get_bytes(&app, auth, &format!("/artifacts/{}/bytes", addresses[0])).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.as_ref(), v1_bytes.as_slice());

        // The sweeper protects the receipt-pinned addresses once the
        // run's receipt is minted (the covered events name both commits).
        // The scratch database is shared with the other gated tests, so
        // the report's deployment-wide counts are not exact here — the
        // assertion is the semantic one: both addresses still serve.
        let (status, receipt) =
            call_as(&app, auth, "GET", &format!("/runs/{run_id}/receipt"), None).await;
        assert_eq!(status, StatusCode::OK, "receipt mint failed: {receipt}");
        let (status, report) = call_as(&app, auth, "POST", "/artifacts/sweep", None).await;
        assert_eq!(status, StatusCode::OK, "sweep failed: {report}");
        for (address, bytes) in [(&addresses[0], &v1_bytes), (&addresses[1], &v2_bytes)] {
            let (status, _, body) =
                get_bytes(&app, auth, &format!("/artifacts/{address}/bytes")).await;
            assert_eq!(
                status,
                StatusCode::OK,
                "the sweep pruned a receipt-pinned address: {report}"
            );
            assert_eq!(body.as_ref(), bytes.as_slice());
        }

        // The release act prunes the head's address — journaled on the
        // deployment chain, the only path past a live receipt.
        let (status, v) = call_as(
            &app,
            auth,
            "POST",
            &format!("/artifacts/{}/release", addresses[1]),
            Some(json!({"released_by": "human:pg-test"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "release failed: {v}");
        assert_eq!(v["pruned"], true);
        let (status, _, body) =
            get_bytes(&app, auth, &format!("/artifacts/{}/bytes", addresses[1])).await;
        assert_eq!(status, StatusCode::GONE);
        let error: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(error["error"], "artifact_unavailable");
        // v1's bytes are a distinct address, still receipt-pinned.
        let (status, _, body) =
            get_bytes(&app, auth, &format!("/artifacts/{}/bytes", addresses[0])).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.as_ref(), v1_bytes.as_slice());

        // The chain carries this tenant's release and the typed miss.
        // The chain is deployment-wide (shared across every gated run
        // against the scratch database), so the assertions filter to
        // this run's address.
        let (status, v) = call_as(&app, auth, "GET", "/artifacts/journal", None).await;
        assert_eq!(status, StatusCode::OK, "chain read failed: {v}");
        let events = v["events"].as_array().unwrap();
        let releases: Vec<&Value> = events
            .iter()
            .filter(|event| event["kind"] == "artifact_retention_released")
            .map(|event| &event["output"]["value"])
            .filter(|release| release["artifact_id"] == json!(addresses[1]))
            .collect();
        assert_eq!(releases.len(), 1);
        assert_eq!(releases[0]["tenant"], json!(tenant));
        assert_eq!(releases[0]["released_by"], "human:pg-test");
        assert!(
            events.iter().any(|event| {
                event["kind"] == "artifact_unavailable"
                    && event["output"]["value"]["artifact_id"] == json!(addresses[1])
            }),
            "the typed miss journaled"
        );
    }
}
