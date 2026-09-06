//! Induction-surface integration tests (demand-side learning, wave
//! 3b): `POST /induction/run` over the default JSON-file backend — the
//! composite pass (mine → crawl → join), versioned intent
//! reassignments across passes, ledger seeding with dry-run as the
//! default, declared blocks, threshold validation, and tenant
//! isolation.
//!
//! Driven in-process via `tower::ServiceExt::oneshot` (no sockets), the
//! `gaps.rs` convention.

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
    std::env::temp_dir().join(format!(
        "rusty-server-induction-test-{}",
        uuid::Uuid::new_v4()
    ))
}

/// Open-mode (single `default` tenant) app over a fresh store.
fn app() -> (Router, PathBuf) {
    let store = temp_store();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone());
    (router(GraphRegistry::new(), config), store)
}

/// Two-tenant app for the isolation test.
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

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    call_as(app, None, method, uri, body).await
}

/// Record one interaction event (pinned `occurred_at`, the connector's
/// dedupe contract).
async fn record_event(app: &Router, record_id: &str, channel: &str, utterance: &str, day: u64) {
    let (status, v) = call(
        app,
        "POST",
        "/gaps/events",
        Some(json!({
            "source": {"system": "servicenow", "stream": "incident", "record_id": record_id},
            "actor": {"role": "employee", "id": "u-1"},
            "channel": channel,
            "utterance": utterance,
            "resolution_path": "human_resolved",
            "outcome": "escalated",
            "occurred_at": format!("2026-08-{day:02}T09:00:00Z"),
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "event ingest failed: {v}");
}

/// Seed the corpus: a three-event vpn cluster and a two-event password
/// cluster (the core fixtures' shape).
async fn seed_corpus(app: &Router) {
    record_event(
        app,
        "INC001",
        "incident",
        "vpn connect home office certificate error",
        1,
    )
    .await;
    record_event(
        app,
        "INC002",
        "incident",
        "vpn connect home office drops hourly",
        2,
    )
    .await;
    record_event(
        app,
        "INC003",
        "escalation",
        "vpn connect home office still failing",
        3,
    )
    .await;
    record_event(app, "SRL001", "portal_search", "password reset portal", 4).await;
    record_event(
        app,
        "SRL002",
        "portal_search",
        "password reset account access",
        5,
    )
    .await;
}

/// The fixture supply: one exact-signature vpn article, one weak
/// password article.
fn artifacts() -> Value {
    json!([
        {
            "artifact_id": "KB001",
            "kind": "kb_article",
            "title": "vpn connect home office certificate error troubleshooting",
            "body": "When the vpn still drops hourly after failing to connect from home \
                     office, check the ZTNA certificate error logs.",
            "last_revised": "2026-08-01T00:00:00Z",
            "systems_referenced": ["ztna"],
        },
        {
            "artifact_id": "KB002",
            "kind": "kb_article",
            "title": "Account help",
            "body": "If you cannot sign in, the password portal can reset credentials.",
            "systems_referenced": [],
        },
    ])
}

/// Run the induction pass; asserts 200 and returns the body.
async fn run_induction(app: &Router, payload: Value) -> Value {
    let (status, v) = call(app, "POST", "/induction/run", Some(payload)).await;
    assert_eq!(status, StatusCode::OK, "induction run failed: {v}");
    v
}

// --------------------------------------------------------------------- //
// The composite pass
// --------------------------------------------------------------------- //

#[tokio::test]
async fn the_pass_mines_crawls_and_joins_in_one_call() {
    let (app, store) = app();
    seed_corpus(&app).await;

    let v = run_induction(
        &app,
        json!({"artifacts": artifacts(), "declared_blocks_top_n": 2}),
    )
    .await;

    // The intent map: two clusters, every event cited, the vpn cluster
    // ranked first (volume × failure-cost).
    let map = &v["intent_map"];
    assert_eq!(map["event_count"], json!(5));
    assert_eq!(map["intents"].as_array().unwrap().len(), 2);
    let vpn = map["intents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|intent| intent["label"].as_str().unwrap().contains("vpn"))
        .expect("the vpn cluster");
    assert_eq!(vpn["frequency"], json!(3));
    assert_eq!(vpn["event_ids"].as_array().unwrap().len(), 3);

    // First pass: every event gets an assignment.
    assert_eq!(v["reassignments"], json!(5));

    // The coverage map: exact vpn coverage, weak password coverage.
    let claims = v["coverage_map"]["claims"].as_array().unwrap();
    assert_eq!(claims.len(), 2);
    let strong = claims
        .iter()
        .find(|claim| claim["artifact"]["id"] == json!("KB001"))
        .unwrap();
    assert_eq!(strong["confidence"], json!("exact_signature"));

    // The matrix: both intents covered with zero measured failure —
    // working supply everywhere, nothing seeded (dry run is the
    // default).
    assert_eq!(v["matrix"]["rows"].as_array().unwrap().len(), 2);
    assert!(
        v["matrix"]["rows"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["cell"] == json!("working_supply"))
    );
    assert_eq!(v["seeded_gap_ids"], json!([]));
    let (status, v2) = call(&app, "GET", "/gaps", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(v2["work_order"].as_array().unwrap().is_empty());

    // Declared blocks, work order first.
    let blocks = v["declared_blocks"].as_array().unwrap();
    assert_eq!(blocks.len(), 2);
    assert!(blocks[0]["label"].as_str().unwrap().contains("vpn"));
    assert_eq!(blocks[0]["empty"], json!(false));

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn a_second_pass_appends_no_reassignments_when_nothing_moved() {
    let (app, store) = app();
    seed_corpus(&app).await;

    let first = run_induction(&app, json!({})).await;
    assert_eq!(first["reassignments"], json!(5));

    // The same corpus, the same config: the newest assignment already
    // names the mined intent, so nothing appends.
    let second = run_induction(&app, json!({})).await;
    assert_eq!(second["reassignments"], json!(0));

    // And the projections agree byte-for-byte with the first pass's
    // (modulo the injected pass timestamp).
    assert_eq!(
        first["intent_map"]["intents"],
        second["intent_map"]["intents"]
    );

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn seeding_files_the_learn_now_cell_and_converges_on_a_re_run() {
    let (app, store) = app();
    seed_corpus(&app).await;

    // No supply: every intent is learn-now, and seeding files it.
    let v = run_induction(&app, json!({"seed": true})).await;
    let seeded = v["seeded_gap_ids"].as_array().unwrap();
    assert_eq!(seeded.len(), 2);

    let (status, v2) = call(&app, "GET", "/gaps", None).await;
    assert_eq!(status, StatusCode::OK);
    let order = v2["work_order"].as_array().unwrap();
    assert_eq!(order.len(), 2);
    assert!(
        order
            .iter()
            .all(|entry| entry["origin"] == json!("induction"))
    );
    // The vpn intent leads the work order.
    assert!(
        order[0]["subject"]["intent"]["intent_id"]
            .as_str()
            .unwrap()
            .starts_with("in-")
    );

    // A seeded re-run converges: same ids, reinforcement not
    // duplication.
    let again = run_induction(&app, json!({"seed": true})).await;
    assert_eq!(&v["seeded_gap_ids"], &again["seeded_gap_ids"]);
    let (status, v3) = call(&app, "GET", "/gaps", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v3["work_order"].as_array().unwrap().len(), 2);

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn a_per_mille_threshold_above_1000_is_a_400() {
    let (app, store) = app();
    let (status, _) = call(
        &app,
        "POST",
        "/induction/run",
        Some(json!({"failing_threshold_millis": 1001})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn one_tenants_induction_never_touches_anothers_ledger() {
    let (app, store) = multi_tenant_app();

    let (status, v) = call_as(
        &app,
        Some(ACME),
        "POST",
        "/gaps/events",
        Some(json!({
            "source": {"system": "servicenow", "stream": "incident", "record_id": "INC001"},
            "actor": {"role": "employee", "id": "u-1"},
            "channel": "incident",
            "utterance": "vpn connect home office certificate error",
            "resolution_path": "human_resolved",
            "outcome": "escalated",
            "occurred_at": "2026-08-01T09:00:00Z",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "acme event failed: {v}");

    let (status, v) = call_as(
        &app,
        Some(ACME),
        "POST",
        "/induction/run",
        Some(json!({"seed": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "acme induction failed: {v}");
    assert_eq!(v["seeded_gap_ids"].as_array().unwrap().len(), 1);

    // Globex: no events, an empty projection, an empty ledger.
    let (status, v) = call_as(
        &app,
        Some(GLOBEX),
        "POST",
        "/induction/run",
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["intent_map"]["event_count"], json!(0));
    assert_eq!(v["intent_map"]["intents"], json!([]));
    let (status, v) = call_as(&app, Some(GLOBEX), "GET", "/gaps", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(v["work_order"].as_array().unwrap().is_empty());

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Vector-index clustering mode (EP-07-S06)
// --------------------------------------------------------------------- //

/// The deterministic test embedder, mirroring the core suite's: each
/// content token contributes to two fixed FNV-1a slots, so texts
/// sharing tokens land near each other and identical texts embed
/// identically across runs.
#[derive(Debug)]
struct TokenBagIndex {
    dimensions: usize,
}

impl rusty_agent_runtime::induction::EmbeddingIndex for TokenBagIndex {
    fn dimensions(&self) -> usize {
        self.dimensions
    }

    fn embed(&self, text: &str) -> Vec<f64> {
        let mut vector = vec![0.0; self.dimensions];
        for token in rusty_agent_runtime::induction::token_signature(text) {
            let mut hash = 0xcbf29ce484222325u64;
            for byte in token.as_bytes() {
                hash ^= *byte as u64;
                hash = hash.wrapping_mul(0x100000001b3);
            }
            vector[(hash % self.dimensions as u64) as usize] += 1.0;
            vector[((hash >> 32) % self.dimensions as u64) as usize] += 0.5;
        }
        vector
    }
}

fn vector_mining_config() -> Value {
    json!({
        "mode": "vector_index",
        "jaccard_threshold_millis": 400,
        "vector_threshold_millis": 550,
        "human_resolution_cost_millis": 100,
        "escalation_cost_millis": 500,
        "abandonment_cost_millis": 300,
        "ttr_over_norm_cost_millis": 10
    })
}

#[tokio::test]
async fn vector_mode_without_a_configured_index_is_a_422() {
    let (app, store) = app();
    seed_corpus(&app).await;

    let (status, v) = call(
        &app,
        "POST",
        "/induction/run",
        Some(json!({
            "artifacts": artifacts(),
            "mining_config": vector_mining_config(),
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "an unconfigured vector mode refuses typed, never silently downgrades: {v}"
    );

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn vector_mode_mines_through_the_configured_index() {
    let store = temp_store();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_embedding_index(std::sync::Arc::new(TokenBagIndex { dimensions: 4096 }));
    let app_with_index = router(GraphRegistry::new(), config);
    seed_corpus(&app_with_index).await;

    let v = run_induction(
        &app_with_index,
        json!({
            "artifacts": artifacts(),
            "mining_config": vector_mining_config(),
        }),
    )
    .await;
    assert_eq!(v["intent_map"]["mode"], json!("vector_index"));
    let intents = v["intent_map"]["intents"].as_array().unwrap();
    assert_eq!(
        intents.len(),
        2,
        "the vpn and password clusters: {intents:?}"
    );
    let mut sizes: Vec<usize> = intents
        .iter()
        .map(|i| i["event_ids"].as_array().unwrap().len())
        .collect();
    sizes.sort();
    assert_eq!(sizes, vec![2, 3]);

    // The full-text pass over the same corpus lands the same intent
    // identities: mode changes the clustering path, not the demand
    // lines downstream consumers join on.
    let (fts_app, fts_store) = app();
    seed_corpus(&fts_app).await;
    let f = run_induction(&fts_app, json!({"artifacts": artifacts()})).await;
    assert_eq!(f["intent_map"]["mode"], json!("full_text_taxonomy"));
    let vector_ids: Vec<&str> = intents
        .iter()
        .map(|i| i["intent_id"].as_str().unwrap())
        .collect();
    let mut fts_ids: Vec<&str> = f["intent_map"]["intents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["intent_id"].as_str().unwrap())
        .collect();
    fts_ids.sort();
    let mut vector_ids = vector_ids;
    vector_ids.sort();
    assert_eq!(vector_ids, fts_ids);

    let _ = std::fs::remove_dir_all(store);
    let _ = std::fs::remove_dir_all(fts_store);
}
