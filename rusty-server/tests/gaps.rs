//! Gap-ledger integration tests (demand-side learning, wave 2): the
//! `/gaps` HTTP surface over the default JSON-file backend — event
//! ingest with content-address convergence, filing and reinforcement,
//! the validated status machine, speculative entries and their probes,
//! mechanical closure, exact rollback, the behavioral signal, tenant
//! isolation, restart durability, and the two runtime filing hooks
//! (zero-recall and operator corrections).
//!
//! Driven in-process via `tower::ServiceExt::oneshot` (no sockets), the
//! `memory.rs` convention.

use std::path::PathBuf;

use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::http::{Request, StatusCode};
use rusty_agent_server::{GraphRegistry, ServerConfig, router};
use serde_json::{Value, json};
use tower::ServiceExt;

// --------------------------------------------------------------------- //
// Harness
// --------------------------------------------------------------------- //

/// Unique temp store root, removed at the end of each test (best effort).
fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!("rusty-server-gaps-test-{}", uuid::Uuid::new_v4()))
}

/// Open-mode (single `default` tenant) app over a fresh store.
fn app() -> (Router, PathBuf) {
    let store = temp_store();
    (app_at(store.clone()), store)
}

/// Open-mode app over a given store root (restart tests build it twice).
fn app_at(store: PathBuf) -> Router {
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store);
    router(GraphRegistry::new(), config)
}

/// Two-tenant app for the isolation tests.
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

/// An interaction-event payload; fields merge over the defaults. The
/// `occurred_at` is pinned: the dedupe contract expects the connector to
/// send the source row's own timestamp, so convergence holds.
fn event_payload(overrides: Value) -> Value {
    let mut base = json!({
        "source": {"system": "servicenow", "stream": "incident", "record_id": "INC0001"},
        "actor": {"role": "employee", "id": "u-100"},
        "channel": "incident",
        "utterance": "VPN drops every hour",
        "resolution_path": "human_resolved",
        "outcome": "escalated",
        "occurred_at": "2026-08-20T09:15:00Z",
    });
    let base_map = base.as_object_mut().unwrap();
    for (key, value) in overrides.as_object().unwrap() {
        base_map.insert(key.clone(), value.clone());
    }
    base
}

/// Record an event; asserts 201 and returns its id.
async fn record_event(app: &Router, overrides: Value) -> String {
    let (status, v) = call(app, "POST", "/gaps/events", Some(event_payload(overrides))).await;
    assert_eq!(status, StatusCode::CREATED, "event ingest failed: {v}");
    assert_eq!(v["created"], json!(true));
    v["event_id"].as_str().unwrap().to_string()
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

/// The tenant's work-order listing.
async fn work_order(app: &Router) -> Vec<Value> {
    let (status, v) = call(app, "GET", "/gaps", None).await;
    assert_eq!(status, StatusCode::OK, "work order failed: {v}");
    v["work_order"].as_array().unwrap().clone()
}

/// One entry with its chain.
async fn get_gap(app: &Router, gap_id: &str) -> Value {
    let (status, v) = call(app, "GET", &format!("/gaps/{gap_id}"), None).await;
    assert_eq!(status, StatusCode::OK, "get failed: {v}");
    v
}

// --------------------------------------------------------------------- //
// Event ingest
// --------------------------------------------------------------------- //

#[tokio::test]
async fn event_ingest_round_trip_and_reingest_converges() {
    let (app, store) = app();

    let event_id = record_event(&app, json!({})).await;
    assert!(
        event_id.starts_with("ie-"),
        "content-addressed id: {event_id}"
    );

    // Re-ingesting the same source row converges: 200, same id, not
    // created — a re-run connector cannot double-count demand.
    let (status, v) = call(&app, "POST", "/gaps/events", Some(event_payload(json!({})))).await;
    assert_eq!(status, StatusCode::OK, "re-ingest failed: {v}");
    assert_eq!(v["created"], json!(false));
    assert_eq!(v["event_id"], json!(event_id));

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Filing, reinforcement, work order
// --------------------------------------------------------------------- //

#[tokio::test]
async fn filed_gap_appears_in_the_work_order_with_its_chain() {
    let (app, store) = app();

    let gap_id = file_gap(&app, json!({})).await;

    let order = work_order(&app).await;
    assert_eq!(order.len(), 1);
    assert_eq!(order[0]["gap_id"], json!(gap_id));
    assert_eq!(order[0]["status"], json!("open"));
    assert_eq!(order[0]["volume"], json!(1));

    let detail = get_gap(&app, &gap_id).await;
    assert_eq!(detail["entry"]["origin"], json!("operator"));
    assert_eq!(detail["entry"]["observed"], json!(true));
    let chain = detail["chain"].as_array().unwrap();
    assert_eq!(chain.len(), 1, "a fresh entry is its Filed mutation");
    assert!(chain[0]["mutation_id"].as_str().unwrap().starts_with("gm-"));

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn refiling_reinforces_and_answers_200() {
    let (app, store) = app();

    let gap_id = file_gap(&app, json!({})).await;
    let (status, v) = call(&app, "POST", "/gaps/file", Some(file_payload(json!({})))).await;
    assert_eq!(status, StatusCode::OK, "reinforcement failed: {v}");
    assert_eq!(v["created"], json!(false));
    assert_eq!(v["gap_id"], json!(gap_id));

    let detail = get_gap(&app, &gap_id).await;
    assert_eq!(
        detail["entry"]["volume"],
        json!(2),
        "dedupe is reinforcement"
    );
    assert_eq!(detail["chain"].as_array().unwrap().len(), 2);

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn filing_with_empty_evidence_is_a_400() {
    let (app, store) = app();

    let (status, _) = call(
        &app,
        "POST",
        "/gaps/file",
        Some(file_payload(json!({"evidence": []}))),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Runtime filing endpoints
// --------------------------------------------------------------------- //

#[tokio::test]
async fn runtime_filings_cite_their_event_and_unknown_events_404() {
    let (app, store) = app();

    let event_id = record_event(&app, json!({})).await;
    let (status, v) = call(
        &app,
        "POST",
        "/gaps/file/escalation",
        Some(json!({
            "event_id": event_id,
            "statement": "Agent could not resolve the VPN drop pattern",
            "closure_criteria": {"block_filled": {"block_label": "vpn-runbook"}},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "escalation filing failed: {v}");
    let gap_id = v["gap_id"].as_str().unwrap().to_string();
    let detail = get_gap(&app, &gap_id).await;
    assert_eq!(detail["entry"]["origin"], json!("runtime_escalation"));
    assert_eq!(
        detail["entry"]["evidence"][0]["kind"],
        json!("interaction_event")
    );

    let (status, _) = call(
        &app,
        "POST",
        "/gaps/file/correction",
        Some(json!({
            "event_id": "ie-does-not-exist",
            "statement": "unknown",
            "closure_criteria": {"block_filled": {"block_label": "x"}},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Status machine
// --------------------------------------------------------------------- //

#[tokio::test]
async fn the_status_machine_admits_legal_edges_and_refuses_the_rest() {
    let (app, store) = app();

    let gap_id = file_gap(&app, json!({})).await;

    let (status, v) = call(
        &app,
        "POST",
        &format!("/gaps/{gap_id}/transition"),
        Some(json!({"to": "hunting"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "open -> hunting failed: {v}");
    assert_eq!(v["entry"]["status"], json!("hunting"));

    // Closure goes through /close — a bare transition to closed is
    // closure without criteria, and core refuses it.
    let (status, _) = call(
        &app,
        "POST",
        &format!("/gaps/{gap_id}/transition"),
        Some(json!({"to": "closed"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Parked is the speculative decay state; observed gaps never park.
    let (status, _) = call(
        &app,
        "POST",
        &format!("/gaps/{gap_id}/transition"),
        Some(json!({"to": "parked"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    let (status, _) = call(
        &app,
        "POST",
        "/gaps/gap-unknown/transition",
        Some(json!({"to": "hunting"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Speculation and probes
// --------------------------------------------------------------------- //

#[tokio::test]
async fn speculation_waits_for_a_validating_probe() {
    let (app, store) = app();

    let (status, v) = call(
        &app,
        "POST",
        "/gaps/speculative",
        Some(json!({
            "subject": {"question_shape": {"text": "printer firmware"}},
            "statement": "Printer firmware failures may track the VPN drops",
            "adjacency": "statistical",
            "edge_citation": {"kind": "adjacency_edge", "id": "edge-vpn-printer"},
            "closure_criteria": {"block_filled": {"block_label": "printer-runbook"}},
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "speculative filing failed: {v}"
    );
    assert_eq!(v["observed"], json!(false));
    let gap_id = v["gap_id"].as_str().unwrap().to_string();

    // Unvalidated speculation never appears in the work order and
    // cannot be sent hunting — it cannot cite itself as evidence.
    assert!(work_order(&app).await.is_empty());
    let (status, _) = call(
        &app,
        "POST",
        &format!("/gaps/{gap_id}/transition"),
        Some(json!({"to": "hunting"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // An empty probe parks the entry under the decay clock.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/gaps/{gap_id}/probe"),
        Some(json!({"demand_hits": 0, "supply_covered": false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "empty probe failed: {v}");
    assert_eq!(v["entry"]["status"], json!("parked"));

    // Demand found validates the entry into the ordinary queue.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/gaps/{gap_id}/probe"),
        Some(json!({"demand_hits": 4, "supply_covered": false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "validating probe failed: {v}");
    assert_eq!(v["entry"]["observed"], json!(true));
    assert_eq!(v["entry"]["status"], json!("open"));
    assert_eq!(work_order(&app).await.len(), 1);

    // Probes apply to speculative entries only.
    let observed_id = file_gap(&app, json!({})).await;
    let (status, _) = call(
        &app,
        "POST",
        &format!("/gaps/{observed_id}/probe"),
        Some(json!({"demand_hits": 1, "supply_covered": false})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Mechanical closure and reopening
// --------------------------------------------------------------------- //

#[tokio::test]
async fn closure_checks_the_typed_criteria_against_the_evidence() {
    let (app, store) = app();

    let gap_id = file_gap(&app, json!({})).await;

    // Evidence that does not match the declared criteria refuses.
    let (status, _) = call(
        &app,
        "POST",
        &format!("/gaps/{gap_id}/close"),
        Some(json!({"evidence": {"block_filled": {"block_label": "other-runbook"}}})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Evidence that satisfies it closes with a resolution link.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/gaps/{gap_id}/close"),
        Some(json!({"evidence": {"block_filled": {"block_label": "vpn-runbook"}}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "close failed: {v}");
    assert_eq!(v["entry"]["status"], json!("closed"));
    assert!(
        v["entry"]["resolution"].is_string(),
        "closure records a resolution link: {v}"
    );

    // A closed gap leaves the work order.
    assert!(work_order(&app).await.is_empty());

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn refiling_against_a_closed_gap_reopens_it() {
    let (app, store) = app();

    let gap_id = file_gap(&app, json!({})).await;
    let (status, _) = call(
        &app,
        "POST",
        &format!("/gaps/{gap_id}/close"),
        Some(json!({"evidence": {"block_filled": {"block_label": "vpn-runbook"}}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // The ledger never forgets a gap closed on paper but not in
    // practice: the same filing against a closed entry reopens it.
    let (status, v) = call(&app, "POST", "/gaps/file", Some(file_payload(json!({})))).await;
    assert_eq!(status, StatusCode::OK, "reopen filing failed: {v}");
    assert_eq!(v["created"], json!(false));
    let detail = get_gap(&app, &gap_id).await;
    assert_eq!(detail["entry"]["status"], json!("reopened"));
    assert_eq!(work_order(&app).await.len(), 1, "reopened is actionable");

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Rollback
// --------------------------------------------------------------------- //

#[tokio::test]
async fn rollback_restores_an_exact_chain_prefix() {
    let (app, store) = app();

    let gap_id = file_gap(&app, json!({})).await;
    let filed_mutation = get_gap(&app, &gap_id).await["chain"][0]["mutation_id"]
        .as_str()
        .unwrap()
        .to_string();

    let (status, _) = call(
        &app,
        "POST",
        &format!("/gaps/{gap_id}/transition"),
        Some(json!({"to": "hunting"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, v) = call(
        &app,
        "POST",
        &format!("/gaps/{gap_id}/rollback"),
        Some(json!({"to_mutation_id": filed_mutation})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "rollback failed: {v}");
    assert_eq!(
        v["entry"]["status"],
        json!("open"),
        "the restore re-folds the prefix ending at the target"
    );

    let (status, _) = call(
        &app,
        "POST",
        &format!("/gaps/{gap_id}/rollback"),
        Some(json!({"to_mutation_id": "gm-not-on-this-chain"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// The behavioral signal and the sweep
// --------------------------------------------------------------------- //

#[tokio::test]
async fn outcomes_drive_the_per_mille_failure_rate() {
    let (app, store) = app();

    // An unmeasured intent is not a passing intent.
    let (status, v) = call(
        &app,
        "POST",
        "/gaps/outcomes",
        Some(json!({"intent_id": "vpn", "outcome": "accepted"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "outcome failed: {v}");
    assert_eq!(v["failure_rate_millis"], json!(0));

    let (status, v) = call(
        &app,
        "POST",
        "/gaps/outcomes",
        Some(json!({"intent_id": "vpn", "outcome": "corrected", "count": 3})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "outcomes failed: {v}");
    // 3 failures of 4 scored outcomes = 750 per mille.
    assert_eq!(v["failure_rate_millis"], json!(750));

    let (status, _) = call(
        &app,
        "POST",
        "/gaps/outcomes",
        Some(json!({"intent_id": "vpn", "outcome": "accepted", "count": 0})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn the_sweep_reopens_closed_gaps_whose_failure_rate_says_otherwise() {
    let (app, store) = app();

    // A gap whose closure criterion is a measured failure rate.
    let gap_id = file_gap(
        &app,
        json!({
            "subject": {"intent": {"intent_id": "vpn"}},
            "closure_criteria": {"failure_rate_below": {"threshold_millis": 100}},
        }),
    )
    .await;

    // Score a clean run and close on the measurement.
    let (status, _) = call(
        &app,
        "POST",
        "/gaps/outcomes",
        Some(json!({"intent_id": "vpn", "outcome": "accepted", "count": 10})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, v) = call(
        &app,
        "POST",
        &format!("/gaps/{gap_id}/close"),
        Some(json!({"evidence": "failure_rate_measured"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "close failed: {v}");
    assert_eq!(v["entry"]["status"], json!("closed"));

    // The fix does not hold: corrections pile up past the criterion.
    let (status, _) = call(
        &app,
        "POST",
        "/gaps/outcomes",
        Some(json!({"intent_id": "vpn", "outcome": "corrected", "count": 5})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // The self-honesty pass reopens it.
    let (status, v) = call(
        &app,
        "POST",
        "/gaps/sweep",
        Some(json!({"threshold_millis": 100})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "sweep failed: {v}");
    assert_eq!(v["reopened"], json!([gap_id.clone()]));
    let detail = get_gap(&app, &gap_id).await;
    assert_eq!(detail["entry"]["status"], json!("reopened"));

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Runtime filing hooks: zero-recall and corrections
// --------------------------------------------------------------------- //

/// A human-authored memory write payload; fields merge over the
/// defaults (the `memory.rs` shape).
fn write_payload(overrides: Value) -> Value {
    let mut base = json!({
        "kind": "fact",
        "scope": {"scope": "user", "id": "user-7"},
        "content": {"timezone": "Asia/Dubai"},
        "author": {"type": "human", "human_id": "amjad"},
    });
    let base_map = base.as_object_mut().unwrap();
    for (key, value) in overrides.as_object().unwrap() {
        base_map.insert(key.clone(), value.clone());
    }
    base
}

#[tokio::test]
async fn a_zero_recall_query_files_a_gap_against_the_question_shape() {
    let (app, store) = app();

    // A hit files nothing.
    let (status, v) = call(
        &app,
        "POST",
        "/memory",
        Some(write_payload(json!({"key": "timezone"}))),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "write failed: {v}");
    let (status, v) = call(
        &app,
        "POST",
        "/memory/query",
        Some(json!({"key": "timezone"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "query failed: {v}");
    assert_eq!(v["records"].as_array().unwrap().len(), 1);
    assert!(work_order(&app).await.is_empty());

    // A named-question miss files a gap — evidence of a question the
    // declared schema did not anticipate.
    let (status, v) = call(
        &app,
        "POST",
        "/memory/query",
        Some(json!({"key": "vpn-runbook"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "query failed: {v}");
    assert!(v["records"].as_array().unwrap().is_empty());
    let order = work_order(&app).await;
    assert_eq!(order.len(), 1, "the miss filed a gap");
    assert_eq!(order[0]["origin"], json!("zero_recall"));
    assert_eq!(
        order[0]["subject"],
        json!({"question_shape": {"text": "vpn-runbook"}})
    );

    // The miss is durable evidence, not a log line: a second identical
    // miss reinforces the same entry instead of minting another.
    let (status, _) = call(
        &app,
        "POST",
        "/memory/query",
        Some(json!({"key": "vpn-runbook"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let order = work_order(&app).await;
    assert_eq!(order.len(), 1);
    assert_eq!(order[0]["volume"], json!(2));

    // An unfiltered browse that finds nothing is not a learning signal.
    let store2 = temp_store();
    let app2 = app_at(store2.clone());
    let (status, _) = call(&app2, "POST", "/memory/query", Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK);
    let (status, v) = call(&app2, "GET", "/gaps", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(v["work_order"].as_array().unwrap().is_empty());

    let _ = std::fs::remove_dir_all(store);
    let _ = std::fs::remove_dir_all(store2);
}

#[tokio::test]
async fn an_operator_correction_files_a_runtime_gap() {
    let (app, store) = app();

    let (status, v) = call(
        &app,
        "POST",
        "/memory",
        Some(write_payload(json!({"key": "expense-policy"}))),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "write failed: {v}");
    let memory_id = v["memory_id"].as_str().unwrap().to_string();

    let correction = json!({
        "correction_id": "corr-gap-1",
        "author": "amjad",
        "target": {"type": "memory", "memory_id": memory_id},
        "corrected": {"per_diem": "320 AED"},
        "scope": {"scope": "user", "id": "user-7"},
        "rationale": "the 2025 rate changed",
    });
    let (status, v) = call(
        &app,
        "POST",
        "/memory/corrections",
        Some(correction.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "correction failed: {v}");
    let derived_id = v["memory_id"].as_str().unwrap().to_string();

    // The correction is evidence the recalled knowledge was wrong, not
    // missing: a runtime-correction gap cites the derived record.
    let order = work_order(&app).await;
    assert_eq!(order.len(), 1, "the correction filed a gap");
    assert_eq!(order[0]["origin"], json!("runtime_correction"));
    assert_eq!(
        order[0]["subject"],
        json!({"question_shape": {"text": "expense-policy"}})
    );
    let gap_id = order[0]["gap_id"].as_str().unwrap().to_string();
    let detail = get_gap(&app, &gap_id).await;
    assert_eq!(
        detail["entry"]["evidence"][0],
        json!({
            "kind": "memory_record",
            "id": derived_id,
            "note": "correction:corr-gap-1",
        })
    );

    // A retried submission converges on the correction id and does not
    // re-file: demand volume stays 1.
    let (status, v) = call(&app, "POST", "/memory/corrections", Some(correction)).await;
    assert_eq!(status, StatusCode::OK, "retry failed: {v}");
    assert_eq!(v["created"], json!(false));
    let order = work_order(&app).await;
    assert_eq!(order.len(), 1);
    assert_eq!(order[0]["volume"], json!(1));

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Tenant isolation and restart durability
// --------------------------------------------------------------------- //

#[tokio::test]
async fn one_tenants_ledger_is_invisible_to_another() {
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

    let (status, v) = call_as(&app, Some(GLOBEX), "GET", "/gaps", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(v["work_order"].as_array().unwrap().is_empty());

    // Unknown and cross-tenant are the same 404, by design.
    let (status, _) = call_as(&app, Some(GLOBEX), "GET", &format!("/gaps/{gap_id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, v) = call_as(&app, Some(ACME), "GET", "/gaps", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["work_order"].as_array().unwrap().len(), 1);

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn the_ledger_survives_a_restart() {
    let (app, store) = app();

    let gap_id = file_gap(&app, json!({})).await;
    let (status, _) = call(
        &app,
        "POST",
        &format!("/gaps/{gap_id}/transition"),
        Some(json!({"to": "hunting"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    drop(app);

    let app2 = app_at(store.clone());
    let detail = get_gap(&app2, &gap_id).await;
    assert_eq!(detail["entry"]["status"], json!("hunting"));
    assert_eq!(detail["chain"].as_array().unwrap().len(), 2);

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Outcome annotations (the behavioral signal's provenance-rich path)
// --------------------------------------------------------------------- //

/// A scored-turn annotation payload; fields merge over the defaults.
fn annotation_payload(overrides: Value) -> Value {
    let mut base = json!({
        "turn_ref": "session-9:turn-4",
        "intent_id": "odyssey-login",
        "judge_votes": [
            {"judge": "gpt-4o", "vote": "corrected"},
            {"judge": "claude", "vote": "corrected"},
            {"judge": "heuristic", "vote": "accepted"},
        ],
        "scored_at": "2026-08-25T10:00:00Z",
    });
    let base_map = base.as_object_mut().unwrap();
    for (key, value) in overrides.as_object().unwrap() {
        base_map.insert(key.clone(), value.clone());
    }
    base
}

#[tokio::test]
async fn annotations_ingest_scores_and_the_curve_answers() {
    let (app, store) = app();

    let (status, v) = call(
        &app,
        "POST",
        "/gaps/annotations",
        Some(annotation_payload(json!({}))),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "ingest failed: {v}");
    let annotation_id = v["annotation_id"].as_str().unwrap().to_string();
    assert!(annotation_id.starts_with("oa-"));
    assert_eq!(v["outcome"], json!("corrected"), "the majority decides");
    assert_eq!(v["failure_rate_millis"], json!(1000));
    assert_eq!(v["closed_gap_ids"], json!([]));

    // Re-recording the same score converges by identity: 200, same id —
    // a re-run scorer cannot double-count a turn.
    let (status, v) = call(
        &app,
        "POST",
        "/gaps/annotations",
        Some(annotation_payload(json!({}))),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "re-ingest failed: {v}");
    assert_eq!(v["annotation_id"], json!(annotation_id));

    // The curve answers with the tally, the rate, and every annotation.
    let (status, v) = call(&app, "GET", "/gaps/intents/odyssey-login/outcomes", None).await;
    assert_eq!(status, StatusCode::OK, "curve failed: {v}");
    assert_eq!(v["tally"]["corrected"], json!(1));
    assert_eq!(v["failure_rate_millis"], json!(1000));
    let curve = v["curve"].as_array().unwrap();
    assert_eq!(curve.len(), 1);
    assert_eq!(curve[0]["annotation_id"], json!(annotation_id));
    assert_eq!(
        curve[0]["judge_votes"].as_array().unwrap().len(),
        3,
        "every judge sample is recorded"
    );

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn a_crossing_measurement_closes_the_gap_automatically() {
    let (app, store) = app();

    let gap_id = file_gap(
        &app,
        json!({
            "subject": {"intent": {"intent_id": "odyssey-login"}},
            "statement": "Odyssey login guidance fails too often",
            "closure_criteria": {"failure_rate_below": {"threshold_millis": 500}},
        }),
    )
    .await;

    // One corrected turn: rate 1000 per mille — the criterion is not
    // met, and that is not an error.
    let (status, v) = call(
        &app,
        "POST",
        "/gaps/annotations",
        Some(annotation_payload(json!({}))),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "first score failed: {v}");
    assert_eq!(v["closed_gap_ids"], json!([]));

    // Two accepted turns drop the rate to 333 per mille; the second
    // recording closes the entry in the same call.
    let mut closed = vec![];
    for (turn, at) in [
        ("turn-5", "2026-08-25T10:05:00Z"),
        ("turn-6", "2026-08-25T10:06:00Z"),
    ] {
        let (status, v) = call(
            &app,
            "POST",
            "/gaps/annotations",
            Some(annotation_payload(json!({
                "turn_ref": format!("session-9:{turn}"),
                "judge_votes": [{"judge": "gpt-4o", "vote": "accepted"}],
                "scored_at": at,
            }))),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "score failed: {v}");
        closed = v["closed_gap_ids"].as_array().unwrap().clone();
    }
    assert_eq!(closed, vec![json!(gap_id)]);

    let detail = get_gap(&app, &gap_id).await;
    assert_eq!(detail["entry"]["status"], json!("closed"));
    assert_eq!(
        detail["entry"]["resolution"],
        json!("failure-rate:333:below:500")
    );

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn annotations_validate_their_votes() {
    let (app, store) = app();

    let (status, _v) = call(
        &app,
        "POST",
        "/gaps/annotations",
        Some(annotation_payload(json!({"judge_votes": []}))),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "no samples is no measurement"
    );

    let (status, _v) = call(
        &app,
        "POST",
        "/gaps/annotations",
        Some(annotation_payload(
            json!({"judge_votes": [{"judge": "", "vote": "accepted"}]}),
        )),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "an unnamed judge is no provenance"
    );

    let _ = std::fs::remove_dir_all(store);
}
