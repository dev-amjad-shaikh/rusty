//! Integration tests for conformance suites and runs (EP-12-S09 AC 2–5).

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_server::{router, GraphRegistry, ServerConfig, StudioExperimentEvaluator};
use serde_json::{json, Value};
use tower::ServiceExt;

fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!(
        "rusty-server-conformance-test-{}",
        uuid::Uuid::new_v4()
    ))
}

fn registry() -> GraphRegistry {
    use rusty_agent_runtime::prelude::*;
    let spec = StateSpec::new().channel("answer", Reducer::Overwrite);
    let mut graph = GraphBuilder::new();
    graph.add_node("answer", |_ctx: NodeContext| async {
        Ok(NodeOutput::update("answer", json!("ready")))
    });
    graph.set_entry_point("answer");
    let mut registry = GraphRegistry::new();
    registry.register("support", graph.compile().unwrap(), spec);
    registry
}

#[derive(Debug)]
struct DummyEvaluator;

#[async_trait::async_trait]
impl StudioExperimentEvaluator for DummyEvaluator {
    async fn evaluate(
        &self,
        _candidate: &rusty_agent_runtime::learn::Candidate,
        _dataset: &rusty_eval::Dataset,
        _config: &rusty_agent_server::StudioExperimentConfig,
    ) -> Result<rusty_agent_server::ExperimentOutcome, String> {
        std::future::pending().await
    }
}

fn app() -> (Router, PathBuf) {
    let store = temp_store();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_studio_experiment_evaluator(Arc::new(DummyEvaluator));
    (router(registry(), config), store)
}

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(value) => {
            builder = builder.header("content-type", "application/json");
            Body::from(value.to_string())
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
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn sample_suite(name: &str, version: &str) -> Value {
    json!({
        "format_version": 1,
        "name": name,
        "version": version,
        "cases": [
            {
                "id": "check-a",
                "description": "A sample check",
                "severity": "blocking",
                "check_type": "dummy::always_pass",
                "parameters": {}
            }
        ]
    })
}

#[tokio::test]
async fn ac2_registration_blocked_without_passing_run() {
    let (app, _store) = app();

    // Create a suite.
    let suite = sample_suite("backend-storage", "1.0.0");
    let (status, _created) = call(
        &app,
        "POST",
        "/conformance-suites",
        Some(json!({
            "name": "backend-storage",
            "version": "1.0.0",
            "suite_json": suite.to_string()
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // No run exists yet → check should report not passing.
    let (status, check) = call(
        &app,
        "GET",
        "/conformance-checks?suite_name=backend-storage&suite_version=1.0.0&target=postgres&target_version=1.0.0",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(check["passing"], false);
    assert!(check["run_id"].is_null());

    // Create a failing run by running the suite (no checks registered → fails).
    let (status, run) = call(
        &app,
        "POST",
        "/conformance-runs",
        Some(json!({
            "suite_name": "backend-storage",
            "suite_version": "1.0.0",
            "target": "postgres",
            "target_version": "1.0.0"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(run["status"]["phase"], "complete");
    assert_eq!(run["report"]["passed"], false);

    // Failing run still blocks registration.
    let (status, check) = call(
        &app,
        "GET",
        "/conformance-checks?suite_name=backend-storage&suite_version=1.0.0&target=postgres&target_version=1.0.0",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(check["passing"], false);
}

#[tokio::test]
async fn ac2_registration_allowed_with_passing_run() {
    let (app, _store) = app();

    // Create a suite with no cases — trivially passes.
    let suite = json!({
        "format_version": 1,
        "name": "backend-storage",
        "version": "1.0.0",
        "cases": []
    });
    let (status, _created) = call(
        &app,
        "POST",
        "/conformance-suites",
        Some(json!({
            "name": "backend-storage",
            "version": "1.0.0",
            "suite_json": suite.to_string()
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Run the empty suite — it passes.
    let (status, run) = call(
        &app,
        "POST",
        "/conformance-runs",
        Some(json!({
            "suite_name": "backend-storage",
            "suite_version": "1.0.0",
            "target": "postgres",
            "target_version": "1.0.0"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(run["status"]["phase"], "complete");
    assert_eq!(run["report"]["passed"], true);

    // Check now reports passing.
    let (status, check) = call(
        &app,
        "GET",
        "/conformance-checks?suite_name=backend-storage&suite_version=1.0.0&target=postgres&target_version=1.0.0",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(check["passing"], true);
    assert!(check["run_id"].as_str().unwrap().starts_with("conf-"));
}

#[tokio::test]
async fn ac3_headless_run_returns_machine_readable_results() {
    let (app, _store) = app();

    let suite = sample_suite("backend-storage", "1.0.0");
    let (status, _created) = call(
        &app,
        "POST",
        "/conformance-suites",
        Some(json!({
            "name": "backend-storage",
            "version": "1.0.0",
            "suite_json": suite.to_string()
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Headless run via POST.
    let (status, run) = call(
        &app,
        "POST",
        "/conformance-runs",
        Some(json!({
            "suite_name": "backend-storage",
            "suite_version": "1.0.0",
            "target": "postgres",
            "target_version": "1.0.0"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(run["suite_name"], "backend-storage");
    assert_eq!(run["suite_version"], "1.0.0");
    assert_eq!(run["target"], "postgres");
    assert_eq!(run["target_version"], "1.0.0");
    assert!(run["report"].is_object());
    assert!(run["report"]["cases"].is_array());
}

#[tokio::test]
async fn ac4_version_bump_invalidates_old_run() {
    let (app, _store) = app();

    // v1 suite with no cases — trivially passes.
    let suite_v1 = json!({
        "format_version": 1,
        "name": "backend-storage",
        "version": "1.0.0",
        "cases": []
    });
    let (status, _) = call(
        &app,
        "POST",
        "/conformance-suites",
        Some(json!({
            "name": "backend-storage",
            "version": "1.0.0",
            "suite_json": suite_v1.to_string()
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Run against v1.
    let (status, run) = call(
        &app,
        "POST",
        "/conformance-runs",
        Some(json!({
            "suite_name": "backend-storage",
            "suite_version": "1.0.0",
            "target": "postgres",
            "target_version": "1.0.0"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(run["report"]["passed"], true);

    // v1 run counts for v1.
    let (status, check) = call(
        &app,
        "GET",
        "/conformance-checks?suite_name=backend-storage&suite_version=1.0.0&target=postgres&target_version=1.0.0",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(check["passing"], true);

    // v2 suite — same name, bumped version.
    let suite_v2 = json!({
        "format_version": 1,
        "name": "backend-storage",
        "version": "2.0.0",
        "cases": []
    });
    let (status, _) = call(
        &app,
        "POST",
        "/conformance-suites",
        Some(json!({
            "name": "backend-storage",
            "version": "2.0.0",
            "suite_json": suite_v2.to_string()
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Old v1 run does NOT count for v2.
    let (status, check) = call(
        &app,
        "GET",
        "/conformance-checks?suite_name=backend-storage&suite_version=2.0.0&target=postgres&target_version=1.0.0",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(check["passing"], false);
}

#[tokio::test]
async fn ac5_lineage_fields_present_in_run_record() {
    let (app, _store) = app();

    let suite = sample_suite("backend-storage", "1.0.0");
    let (status, _) = call(
        &app,
        "POST",
        "/conformance-suites",
        Some(json!({
            "name": "backend-storage",
            "version": "1.0.0",
            "suite_json": suite.to_string()
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, run) = call(
        &app,
        "POST",
        "/conformance-runs",
        Some(json!({
            "suite_name": "backend-storage",
            "suite_version": "1.0.0",
            "target": "postgres",
            "target_version": "1.0.0"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let run_id = run["run_id"].as_str().unwrap();

    // Query the run and assert lineage fields.
    let (status, fetched) = call(&app, "GET", &format!("/conformance-runs/{run_id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fetched["target"], "postgres");
    assert_eq!(fetched["target_version"], "1.0.0");
    assert_eq!(fetched["suite_name"], "backend-storage");
    assert_eq!(fetched["suite_version"], "1.0.0");
}
