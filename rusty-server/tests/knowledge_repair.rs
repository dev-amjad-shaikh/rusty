//! Knowledge-level repair filing integration tests (EP-10-S09).
//!
//! Covers: cause classification (knowledge vs environmental),
//! plan-reality divergence filing, dedup with evidence accrual,
//! and side-band latency (filing never blocks the caller).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

// --------------------------------------------------------------------- //
// Harness
// --------------------------------------------------------------------- //

fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!(
        "rusty-server-knowledge-repair-test-{}",
        uuid::Uuid::new_v4()
    ))
}

fn app_at(store: PathBuf) -> Router {
    use rusty_agent_runtime::prelude::*;
    let spec = StateSpec::new().channel("log", Reducer::Append);
    let mut builder = GraphBuilder::new();
    builder.add_node("first", |_ctx: NodeContext| async {
        Ok(NodeOutput::update("log", json!("first")))
    });
    builder.set_entry_point("first");
    let mut registry = GraphRegistry::new();
    registry.register("pipeline", builder.compile().unwrap(), spec);
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store);
    router(registry, config)
}

fn app() -> (Router, PathBuf) {
    let store = temp_store();
    (app_at(store.clone()), store)
}

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

/// Poll `GET /gaps` until at least `min` entries appear or timeout.
async fn wait_for_gaps(app: &Router, min: usize, timeout_ms: u64) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    while Instant::now() < deadline {
        let (status, body) = call(app, "GET", "/gaps", None).await;
        assert_eq!(status, StatusCode::OK);
        if let Some(work_order) = body.get("work_order").and_then(|v| v.as_array()) {
            if work_order.len() >= min {
                return work_order.clone();
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {min} gap entries");
}

// --------------------------------------------------------------------- //
// AC 1 — Cause classifier: knowledge signatures file; environmental does not
// --------------------------------------------------------------------- //

#[tokio::test]
async fn knowledge_signature_files_gap_with_runtime_correction_origin() {
    let (app, _store) = app();

    let (status, body) = call(
        &app,
        "POST",
        "/repairs/knowledge",
        Some(json!({
            "failure_signature": "unknown_tool:search/web",
            "occurrence_count": 3,
            "session_id": "sess-1",
            "attempt_id": "att-1",
            "evidence": ["ev-1", "ev-2"],
            "repair_chain": ["rr-1"],
        })),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "filing accepted: {body}");
    assert_eq!(body["accepted"], true);
    assert_eq!(body["cause"], "knowledge");

    let gaps = wait_for_gaps(&app, 1, 2000).await;
    assert_eq!(gaps.len(), 1);
    let gap = &gaps[0];
    assert_eq!(gap["origin"], "runtime_correction");
    assert_eq!(gap["status"], "open");
    assert!(
        gap["statement"].as_str().unwrap().contains("unknown_tool"),
        "statement cites failure: {}",
        gap["statement"]
    );
}

#[tokio::test]
async fn environmental_failure_does_not_file_gap() {
    let (app, _store) = app();

    let (status, body) = call(
        &app,
        "POST",
        "/repairs/knowledge",
        Some(json!({
            "failure_signature": "provider_timeout",
            "occurrence_count": 1,
            "evidence": ["ev-1"],
            "repair_chain": [],
        })),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["cause"], "environmental");

    // Give any side-band task a moment to (not) run.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let (status, body) = call(&app, "GET", "/gaps", None).await;
    assert_eq!(status, StatusCode::OK);
    let work_order = body["work_order"].as_array().unwrap();
    assert!(
        work_order.is_empty(),
        "environmental failure must not file a gap"
    );
}

// --------------------------------------------------------------------- //
// AC 2 — Plan-reality divergence cites manifest hash and observed events
// --------------------------------------------------------------------- //

#[tokio::test]
async fn divergence_entry_cites_manifest_hash_and_events() {
    let (app, _store) = app();

    let manifest_hash = "abc123def456";
    let (status, _body) = call(
        &app,
        "POST",
        "/repairs/knowledge",
        Some(json!({
            "failure_signature": "GET /v1/legacy_endpoint returned 404",
            "occurrence_count": 3,
            "session_id": "sess-diverge",
            "attempt_id": "att-diverge",
            "evidence": ["evt-404-1", "evt-404-2"],
            "repair_chain": ["rr-diverge-1"],
            "skill_manifest_hash": manifest_hash,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let gaps = wait_for_gaps(&app, 1, 2000).await;
    assert_eq!(gaps.len(), 1);
    let gap = &gaps[0];
    let statement = gap["statement"].as_str().unwrap();
    assert!(
        statement.contains(manifest_hash),
        "statement cites manifest hash: {statement}"
    );
    assert!(
        statement.contains("legacy_endpoint"),
        "statement cites failing step: {statement}"
    );
    assert_eq!(gap["origin"], "runtime_correction");
    assert_eq!(gap["status"], "open");
}

// --------------------------------------------------------------------- //
// AC 3 — Dedup: ten identical failures become one entry with accrued evidence
// --------------------------------------------------------------------- //

#[tokio::test]
async fn ten_identical_failures_yield_one_gap_with_higher_volume() {
    let (app, _store) = app();

    for i in 0..10 {
        let (status, _body) = call(
            &app,
            "POST",
            "/repairs/knowledge",
            Some(json!({
                "failure_signature": "unknown_tool:calendar/book",
                "occurrence_count": 3,
                "session_id": format!("sess-{i}"),
                "attempt_id": format!("att-{i}"),
                "evidence": [format!("ev-{i}")],
                "repair_chain": [format!("rr-{i}")],
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        // Brief pause so side-band tasks serialize on the store file;
        // the latency test below proves the POST itself is not blocked.
        tokio::time::sleep(Duration::from_millis(150)).await;
    }

    let gaps = wait_for_gaps(&app, 1, 5000).await;
    assert_eq!(gaps.len(), 1, "identical failures must dedupe to one gap");

    let gap = &gaps[0];
    let volume = gap["volume"].as_u64().unwrap();
    assert!(
        volume >= 2,
        "volume should accrue across filings, got {volume}"
    );

    // Priority should have increased with volume.
    let priority = gap["priority_score"].as_f64().unwrap();
    assert!(
        priority > 0.0,
        "priority should be positive after dedup: {priority}"
    );
}

// --------------------------------------------------------------------- //
// AC 5 — Side-band: POST latency is low even when filing is slow
// --------------------------------------------------------------------- //

#[tokio::test]
async fn knowledge_filing_is_side_band_low_latency() {
    let (app, _store) = app();

    let start = Instant::now();
    let (status, body) = call(
        &app,
        "POST",
        "/repairs/knowledge",
        Some(json!({
            "failure_signature": "unknown_tool:database/query",
            "occurrence_count": 5,
            "session_id": "sess-latency",
            "attempt_id": "att-latency",
            "evidence": ["ev-latency"],
            "repair_chain": ["rr-latency"],
        })),
    )
    .await;
    let elapsed = start.elapsed();

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["accepted"], true);
    assert!(
        elapsed < Duration::from_millis(200),
        "POST /repairs/knowledge must return immediately (side-band), took {elapsed:?}"
    );

    // Verify the gap was still filed in the background.
    let gaps = wait_for_gaps(&app, 1, 2000).await;
    assert_eq!(gaps.len(), 1);
}
