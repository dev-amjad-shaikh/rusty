//! Editorial-governance integration tests (EP-07-S03): the rung
//! distribution endpoint reads the tenant's candidate store as the ledger
//! — automated skill mutations land on patch-before-create rungs with
//! their provenance intact, the window filters, malformed windows are
//! caller errors, and every rung reports zero-filled.
//!
//! Driven in-process via `tower::ServiceExt::oneshot`, the `hunts.rs`
//! convention; candidate creation journals against a completed run, so
//! the pipeline graph is registered in the app.

use std::path::PathBuf;

use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Utc};
use rusty_agent_runtime::learn::{Candidate, CandidateContent, EvidenceSpan};
use rusty_agent_runtime::memory::ProvenanceAuthor;
use rusty_agent_runtime::skill_editorial::{EditorialProvenance, PatchPreference};
use rusty_agent_server::{GraphRegistry, ServerConfig, router};
use serde_json::{Value, json};
use tower::ServiceExt;

/// Unique temp store root, removed at the end of each test (best effort).
fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!(
        "rusty-server-skill-editorial-test-{}",
        uuid::Uuid::new_v4()
    ))
}

/// A fixed timestamp, so candidates minted here are deterministic.
fn ts(millis: i64) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp_millis(millis).unwrap()
}

/// The pipeline graph (`first -> second`), so candidate lifecycle events
/// have a completed run to journal against.
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

/// Open-mode app with the pipeline graph.
fn app() -> (Router, PathBuf) {
    let store = temp_store();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone());
    (router(pipeline_registry(), config), store)
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

/// A skill candidate with editorial provenance, minted at `millis`.
fn skill_candidate(name: &str, millis: i64, provenance: EditorialProvenance) -> Candidate {
    Candidate::new(
        CandidateContent::Skill {
            name: name.to_owned(),
            content_hash: format!("sha256:{name}"),
            binding: None,
        },
        ProvenanceAuthor::Distiller {
            name: "test-distiller".into(),
        },
        EvidenceSpan::default(),
        ts(millis),
    )
    .expect("the candidate builds")
    .with_editorial_provenance(provenance)
    .expect("the provenance validates")
}

/// Register a candidate through the learning surface; returns the id.
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

/// The count one rung reports in a distribution body.
fn rung_count(body: &Value, rung: &str) -> u64 {
    body["rungs"]
        .as_array()
        .expect("rungs is an array")
        .iter()
        .find(|row| row["rung"] == json!(rung))
        .unwrap_or_else(|| panic!("rung `{rung}` reports"))["mutations"]
        .as_u64()
        .unwrap()
}

#[tokio::test]
async fn rung_distribution_reads_the_candidate_ledger() {
    let (app, store) = app();
    let run_id = run_pipeline(&app).await;

    // Three automated mutations: two patch landings and one stated
    // create-new, plus one candidate with no editorial provenance at all
    // (an operator-authored prompt change — not an automated skill
    // mutation, so it never enters the distribution).
    let create_id = create_candidate(
        &app,
        &run_id,
        &skill_candidate(
            "refund-reconciliation",
            1_000_000,
            EditorialProvenance::create_new(
                "no existing skill covers refund reconciliation; the umbrellas are read-only",
            )
            .unwrap(),
        ),
    )
    .await;
    let patch_id = create_candidate(
        &app,
        &run_id,
        &skill_candidate(
            "deploy-web-service",
            2_000_000,
            EditorialProvenance::patch(PatchPreference::PatchLoadedSkill).unwrap(),
        ),
    )
    .await;
    create_candidate(
        &app,
        &run_id,
        &skill_candidate(
            "umbrella-support-file",
            3_000_000,
            EditorialProvenance::patch(PatchPreference::AddSupportFile).unwrap(),
        ),
    )
    .await;
    let prompt_candidate = Candidate::new(
        CandidateContent::Prompt {
            name: "system".to_owned(),
            prompt: "Answer briefly.".to_owned(),
        },
        ProvenanceAuthor::Distiller {
            name: "test-distiller".into(),
        },
        EvidenceSpan::default(),
        ts(4_000_000),
    )
    .unwrap();
    create_candidate(&app, &run_id, &prompt_candidate).await;

    // The full distribution: every rung present, zero-filled, in
    // preference order; the prompt candidate is not a data point.
    let (status, v) = call(&app, "GET", "/skills/editorial/rung-distribution", None).await;
    assert_eq!(status, StatusCode::OK, "distribution failed: {v}");
    assert_eq!(v["total"], json!(3));
    let rungs: Vec<&str> = v["rungs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["rung"].as_str().unwrap())
        .collect();
    assert_eq!(
        rungs,
        vec![
            "patch_loaded_skill",
            "patch_existing_umbrella",
            "add_support_file",
            "create_new"
        ],
        "every rung reports, in preference order"
    );
    assert_eq!(rung_count(&v, "patch_loaded_skill"), 1);
    assert_eq!(rung_count(&v, "patch_existing_umbrella"), 0);
    assert_eq!(rung_count(&v, "add_support_file"), 1);
    assert_eq!(rung_count(&v, "create_new"), 1);

    // The window filters on the mutation instant: `since` includes,
    // `until` excludes. Candidates minted at 1000 s (create_new),
    // 2000 s (patch_loaded_skill), 3000 s (add_support_file) — the window
    // below keeps the latter two.
    let (status, v) = call(
        &app,
        "GET",
        "/skills/editorial/rung-distribution?since=1970-01-01T00:16:41Z&until=1970-01-01T00:50:01Z",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "windowed distribution failed: {v}");
    assert_eq!(v["total"], json!(2));
    assert_eq!(rung_count(&v, "create_new"), 0);
    assert_eq!(rung_count(&v, "patch_loaded_skill"), 1);
    assert_eq!(rung_count(&v, "add_support_file"), 1);

    // The exact boundary: `until` excludes the instant it names.
    let (status, v) = call(
        &app,
        "GET",
        "/skills/editorial/rung-distribution?until=1970-01-01T00:16:40Z",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "boundary window failed: {v}");
    assert_eq!(v["total"], json!(0), "1000 s exactly is excluded");

    // The provenance rides the stored record — the ledger entry carries
    // the recorded rung (the record itself is the response body).
    let (status, v) = call(&app, "GET", &format!("/learn/candidates/{patch_id}"), None).await;
    assert_eq!(status, StatusCode::OK, "fetch failed: {v}");
    assert_eq!(
        v["candidate"]["editorial"]["rung"],
        json!("patch_loaded_skill")
    );

    // And a `create_new` landing's stated reason survives the
    // round-trip — the hunt-produced ledger entry carries its recorded
    // justification.
    let (status, v) = call(&app, "GET", &format!("/learn/candidates/{create_id}"), None).await;
    assert_eq!(status, StatusCode::OK, "fetch failed: {v}");
    assert_eq!(
        v["candidate"]["editorial"]["create_new_justification"],
        json!("no existing skill covers refund reconciliation; the umbrellas are read-only")
    );

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn malformed_and_inverted_windows_are_caller_errors() {
    let (app, store) = app();

    let (status, _) = call(
        &app,
        "GET",
        "/skills/editorial/rung-distribution?since=yesterday",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = call(
        &app,
        "GET",
        "/skills/editorial/rung-distribution?since=2026-09-02T00:00:00Z&until=2026-09-01T00:00:00Z",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // An empty store reports a full, zeroed distribution — no data is
    // not an error.
    let (status, v) = call(&app, "GET", "/skills/editorial/rung-distribution", None).await;
    assert_eq!(status, StatusCode::OK, "empty distribution failed: {v}");
    assert_eq!(v["total"], json!(0));
    assert_eq!(v["rungs"].as_array().unwrap().len(), 4);

    let _ = std::fs::remove_dir_all(store);
}
