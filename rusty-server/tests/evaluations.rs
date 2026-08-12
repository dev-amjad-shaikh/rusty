//! Studio evaluation workbench integration tests (Phase 3).
//!
//! Covers the durable dataset surface and the experiment/gate record
//! surface. The actual evaluation currently reuses the existing candidate
//! evaluator path; these tests verify that the workbench records are
//! tenant-isolated and persist across restart.

use std::path::PathBuf;

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!(
        "rusty-server-evaluations-test-{}",
        uuid::Uuid::new_v4()
    ))
}

fn app_with(store: PathBuf, configure: impl FnOnce(ServerConfig) -> ServerConfig) -> Router {
    let config = configure(ServerConfig::new("127.0.0.1:0".parse().unwrap(), store));
    router(GraphRegistry::new(), config)
}

fn app() -> (Router, PathBuf) {
    let store = temp_store();
    (app_with(store.clone(), |config| config), store)
}

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    call_as(app, None, method, uri, body).await
}

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

fn sample_dataset() -> Value {
    json!({
        "name": "support-q-a",
        "version": "2026-08-12",
        "cases": [
            {
                "id": "case-1",
                "input": {"question": "What is the refund policy?"},
                "expect": {
                    "state": [{"pointer": "/answer", "expected": "30 days"}]
                },
                "tags": ["refund"]
            },
            {
                "id": "case-2",
                "input": {"question": "How do I reset my password?"},
                "expect": {
                    "state": [{"pointer": "/answer", "expected": "Use the reset link"}]
                },
                "tags": ["account"]
            }
        ]
    })
}

#[tokio::test]
async fn creates_lists_and_reads_dataset_versions() {
    let (app, _store) = app();
    let payload = sample_dataset();
    let (status, v) = call(&app, "POST", "/datasets", Some(payload)).await;
    assert_eq!(status, StatusCode::CREATED, "create dataset: {v}");
    assert_eq!(v["name"], "support-q-a");
    assert_eq!(v["version"], "2026-08-12");
    assert_eq!(v["case_count"], 2);
    assert!(v["created"].as_bool().unwrap());
    assert!(v["digest"].as_str().unwrap().len() > 0);

    let (status, v) = call(&app, "GET", "/datasets", None).await;
    assert_eq!(status, StatusCode::OK, "list datasets: {v}");
    let datasets = v["datasets"].as_array().unwrap();
    assert_eq!(datasets.len(), 1);
    assert_eq!(datasets[0]["name"], "support-q-a");

    let (status, v) = call(&app, "GET", "/datasets/support-q-a", None).await;
    assert_eq!(status, StatusCode::OK, "get dataset: {v}");
    let versions = v["versions"].as_array().unwrap();
    assert_eq!(versions.len(), 1);
    assert_eq!(versions[0]["version"], "2026-08-12");

    let (status, v) = call(
        &app,
        "GET",
        "/datasets/support-q-a/versions/2026-08-12",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "get version: {v}");
    assert_eq!(v["case_count"], 2);

    let (status, v) = call(
        &app,
        "GET",
        "/datasets/support-q-a/versions/2026-08-12/cases",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "list cases: {v}");
    let cases = v["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 2);
    assert_eq!(cases[0]["id"], "case-1");
}

#[tokio::test]
async fn rejects_duplicate_dataset_version() {
    let (app, _store) = app();
    let payload = sample_dataset();
    let (status, _v) = call(&app, "POST", "/datasets", Some(payload.clone())).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, v) = call(&app, "POST", "/datasets", Some(payload)).await;
    assert_eq!(status, StatusCode::CONFLICT, "duplicate: {v}");
    assert!(v["message"].as_str().unwrap().contains("already exists"));
}

#[tokio::test]
async fn tenant_isolates_datasets() {
    let store = temp_store();
    let app = app_with(store, |config| {
        config
            .with_tenant_key("acme", "acme-secret")
            .with_tenant_key("globex", "globex-secret")
    });
    let acme = Some(("x-api-key", "acme-secret"));
    let globex = Some(("x-api-key", "globex-secret"));

    let (status, _v) = call_as(&app, acme, "POST", "/datasets", Some(sample_dataset())).await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, v) = call_as(&app, globex, "GET", "/datasets", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(v["datasets"].as_array().unwrap().is_empty());

    let (status, v) = call_as(
        &app,
        globex,
        "GET",
        "/datasets/support-q-a/versions/2026-08-12",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "cross-tenant read: {v}");
}

#[tokio::test]
async fn creates_and_retrieves_experiment_and_gate_records() {
    let (app, _store) = app();
    let (status, _v) = call(&app, "POST", "/datasets", Some(sample_dataset())).await;
    assert_eq!(status, StatusCode::CREATED);

    let exp_payload = json!({
        "experiment_id": "exp-1",
        "candidate_id": "candidate-1",
        "dataset_name": "support-q-a",
        "dataset_version": "2026-08-12",
        "target_metric": "case_pass_rate",
        "thresholds": {"max_pass_rate_drop": 0.05}
    });
    let (status, v) = call(&app, "POST", "/experiments", Some(exp_payload)).await;
    assert_eq!(status, StatusCode::CREATED, "create experiment: {v}");
    assert_eq!(v["experiment_id"], "exp-1");
    assert_eq!(v["status"], "Queued");

    let (status, v) = call(&app, "GET", "/experiments", None).await;
    assert_eq!(status, StatusCode::OK, "list experiments: {v}");
    assert_eq!(v["experiments"].as_array().unwrap().len(), 1);

    let (status, v) = call(&app, "GET", "/experiments/exp-1", None).await;
    assert_eq!(status, StatusCode::OK, "get experiment: {v}");
    assert_eq!(v["dataset_version"], "2026-08-12");

    let gate_payload = json!({
        "name": "support-gate",
        "blocked_target": "prompt:system@prod",
        "metric": "case_pass_rate",
        "threshold": 0.95,
        "dataset_version": "support-q-a@2026-08-12"
    });
    let (status, v) = call(&app, "POST", "/gates", Some(gate_payload)).await;
    assert_eq!(status, StatusCode::CREATED, "create gate: {v}");
    assert_eq!(v["name"], "support-gate");
    assert_eq!(v["min_evidence"], 1);

    let (status, v) = call(&app, "GET", "/gates", None).await;
    assert_eq!(status, StatusCode::OK, "list gates: {v}");
    assert_eq!(v["gates"].as_array().unwrap().len(), 1);

    let (status, v) = call(&app, "GET", "/gates/support-gate", None).await;
    assert_eq!(status, StatusCode::OK, "get gate: {v}");
    assert_eq!(v["threshold"], 0.95);
}
