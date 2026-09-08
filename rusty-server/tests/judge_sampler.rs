//! Judge-sampler integration tests (EP-07-S12 AC2): the platform scores a
//! turn's next-state signal through its configured judge sampler — the
//! heuristic reference here — and records the resulting annotation with
//! its votes as provenance; the explicit-votes path is unchanged; the
//! exactly-one-evidence-path rule and the unconfigured-sampler refusal
//! are named caller/deployment errors, never silent neutrals.
//!
//! Driven in-process via `tower::ServiceExt::oneshot`, the `gaps.rs`
//! convention.

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::http::{Request, StatusCode};
use rusty_agent_runtime::judge::HeuristicJudgeSampler;
use rusty_agent_server::{GraphRegistry, ServerConfig, router};
use serde_json::{Value, json};
use tower::ServiceExt;

/// Unique temp store root, removed at the end of each test (best effort).
fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!(
        "rusty-server-judge-sampler-test-{}",
        uuid::Uuid::new_v4()
    ))
}

/// Open-mode app with the heuristic judge sampler configured.
fn app() -> (Router, PathBuf) {
    let store = temp_store();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_judge_sampler(Arc::new(HeuristicJudgeSampler::new("heuristic-v1")));
    (router(GraphRegistry::new(), config), store)
}

/// Open-mode app with no judge sampler — the unconfigured deployment.
fn unsampled_app() -> (Router, PathBuf) {
    let store = temp_store();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone());
    (router(GraphRegistry::new(), config), store)
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

/// A signal-scored annotation payload; fields merge over the defaults.
fn signal_payload(overrides: Value) -> Value {
    let mut base = json!({
        "turn_ref": "session-9:turn-4",
        "intent_id": "odyssey-login",
        "signal": {"kind": "user_message", "text": "that's not quite right, thanks though"},
        "scored_at": "2026-08-25T10:00:00Z",
    });
    let base_map = base.as_object_mut().unwrap();
    for (key, value) in overrides.as_object().unwrap() {
        base_map.insert(key.clone(), value.clone());
    }
    base
}

#[tokio::test]
async fn a_signal_is_scored_into_a_recorded_annotation() {
    let (app, store) = app();

    // A polite correction: the heuristic names it `corrected` — manners
    // do not launder the negative signal.
    let (status, v) = call(
        &app,
        "POST",
        "/gaps/annotations",
        Some(signal_payload(json!({}))),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "signal scoring failed: {v}");
    assert_eq!(v["outcome"], json!("corrected"));
    assert_eq!(v["failure_rate_millis"], json!(1000));
    let annotation_id = v["annotation_id"].as_str().unwrap().to_string();
    assert!(annotation_id.starts_with("oa-"));

    // The curve carries the sampler's vote as provenance — the judge's
    // name is on the ballot.
    let (status, v) = call(&app, "GET", "/gaps/intents/odyssey-login/outcomes", None).await;
    assert_eq!(status, StatusCode::OK, "curve failed: {v}");
    assert_eq!(v["tally"]["corrected"], json!(1));
    let curve = v["curve"].as_array().unwrap();
    assert_eq!(curve.len(), 1);
    assert_eq!(curve[0]["annotation_id"], json!(annotation_id));
    assert_eq!(
        curve[0]["judge_votes"],
        json!([{"judge": "heuristic-v1", "vote": "corrected"}]),
        "the sampler's vote is the recorded provenance"
    );

    // A downstream tool success scores positive; the rate moves to 1 of
    // 2 = 500 per mille.
    let (status, v) = call(
        &app,
        "POST",
        "/gaps/annotations",
        Some(signal_payload(json!({
            "turn_ref": "session-9:turn-5",
            "signal": {"kind": "tool_result", "ok": true},
            "scored_at": "2026-08-25T10:05:00Z",
        }))),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "tool signal failed: {v}");
    assert_eq!(v["outcome"], json!("accepted"));
    assert_eq!(v["failure_rate_millis"], json!(500));

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn explicit_votes_still_ingest_on_a_sampled_app() {
    let (app, store) = app();

    let (status, v) = call(
        &app,
        "POST",
        "/gaps/annotations",
        Some(json!({
            "turn_ref": "session-9:turn-4",
            "intent_id": "odyssey-login",
            "judge_votes": [
                {"judge": "gpt-4o", "vote": "accepted"},
                {"judge": "claude", "vote": "accepted"},
            ],
            "scored_at": "2026-08-25T10:00:00Z",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "explicit votes failed: {v}");
    assert_eq!(v["outcome"], json!("accepted"));

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn the_evidence_path_is_exactly_one() {
    let (app, store) = app();

    // Both paths at once is ambiguous.
    let (status, v) = call(
        &app,
        "POST",
        "/gaps/annotations",
        Some(signal_payload(json!({
            "judge_votes": [{"judge": "gpt-4o", "vote": "accepted"}],
        }))),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "both paths: {v}");

    // Neither path is an evidence-free annotation.
    let mut neither = signal_payload(json!({}));
    neither.as_object_mut().unwrap().remove("signal");
    let (status, v) = call(&app, "POST", "/gaps/annotations", Some(neither)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "no evidence: {v}");

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn a_signal_without_a_configured_sampler_is_a_deployment_error() {
    let (app, store) = unsampled_app();

    let (status, v) = call(
        &app,
        "POST",
        "/gaps/annotations",
        Some(signal_payload(json!({}))),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "the refusal names the missing configuration: {v}"
    );

    // And the explicit-votes path is unaffected on the same deployment.
    let (status, v) = call(
        &app,
        "POST",
        "/gaps/annotations",
        Some(json!({
            "turn_ref": "session-9:turn-4",
            "intent_id": "odyssey-login",
            "judge_votes": [{"judge": "gpt-4o", "vote": "redone"}],
            "scored_at": "2026-08-25T10:00:00Z",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "explicit votes failed: {v}");
    assert_eq!(v["outcome"], json!("redone"));

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn a_sampler_produced_negative_feeds_failure_rate_closure() {
    let (app, store) = app();

    // File the intent's gap with a failure-rate criterion, then score
    // three sampler-produced accepted turns: the rate crosses and the
    // entry closes in the third call — the sampler path feeds the same
    // mechanical closure the explicit path does.
    let (status, v) = call(
        &app,
        "POST",
        "/gaps/file",
        Some(json!({
            "subject": {"intent": {"intent_id": "odyssey-login"}},
            "statement": "Odyssey login guidance fails too often",
            "evidence": [{"kind": "interaction_event", "id": "ie-seed"}],
            "origin": "operator",
            "closure_criteria": {"failure_rate_below": {"threshold_millis": 500}},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "filing failed: {v}");
    assert_eq!(v["created"], json!(true));
    let gap_id = v["gap_id"].as_str().unwrap().to_string();

    // One sampler-scored correction: rate 1000 — not yet.
    let (status, v) = call(
        &app,
        "POST",
        "/gaps/annotations",
        Some(signal_payload(json!({}))),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "first score failed: {v}");
    assert_eq!(v["closed_gap_ids"], json!([]));

    let mut closed = vec![];
    for (turn, at) in [
        ("turn-5", "2026-08-25T10:05:00Z"),
        ("turn-6", "2026-08-25T10:06:00Z"),
    ] {
        let (status, v) = call(
            &app,
            "POST",
            "/gaps/annotations",
            Some(signal_payload(json!({
                "turn_ref": format!("session-9:{turn}"),
                "signal": {"kind": "user_message", "text": "perfect, that works now"},
                "scored_at": at,
            }))),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "score failed: {v}");
        assert_eq!(v["outcome"], json!("accepted"));
        closed = v["closed_gap_ids"].as_array().unwrap().clone();
    }
    assert_eq!(
        closed,
        vec![json!(gap_id)],
        "the sampler path closes the crossing entry"
    );

    let _ = std::fs::remove_dir_all(store);
}
