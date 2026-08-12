//! End-to-end contract tests for Studio's durable evaluation lane.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use chrono::{DateTime, Utc};
use rusty_agent_runtime::learn::{Candidate, CandidateContent, EvidenceSpan};
use rusty_agent_runtime::memory::ProvenanceAuthor;
use rusty_agent_server::{
    router, ExperimentOutcome, GraphRegistry, ServerConfig, StudioExperimentConfig,
    StudioExperimentEvaluator,
};
use rusty_eval::{Dataset, ExperimentReport};
use serde_json::{json, Value};
use tower::ServiceExt;

fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!(
        "rusty-server-evaluations-test-{}",
        uuid::Uuid::new_v4()
    ))
}

fn ts() -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp_millis(1_754_953_200_000).unwrap()
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
struct FixedStudioEvaluator;

#[async_trait::async_trait]
impl StudioExperimentEvaluator for FixedStudioEvaluator {
    async fn evaluate(
        &self,
        _candidate: &Candidate,
        dataset: &Dataset,
        config: &StudioExperimentConfig,
    ) -> Result<ExperimentOutcome, String> {
        Ok(ExperimentOutcome {
            baseline_report: report(dataset, config, true),
            candidate_report: report(dataset, config, false),
        })
    }
}

#[derive(Debug)]
struct NeverStudioEvaluator;

#[async_trait::async_trait]
impl StudioExperimentEvaluator for NeverStudioEvaluator {
    async fn evaluate(
        &self,
        _candidate: &Candidate,
        _dataset: &Dataset,
        _config: &StudioExperimentConfig,
    ) -> Result<ExperimentOutcome, String> {
        std::future::pending().await
    }
}

#[derive(Debug)]
struct MalformedStudioEvaluator;

#[derive(Debug)]
struct RetaggingStudioEvaluator;

#[derive(Debug)]
struct OversizedFailureStudioEvaluator;

#[async_trait::async_trait]
impl StudioExperimentEvaluator for MalformedStudioEvaluator {
    async fn evaluate(
        &self,
        _candidate: &Candidate,
        dataset: &Dataset,
        config: &StudioExperimentConfig,
    ) -> Result<ExperimentOutcome, String> {
        let baseline_report = report(dataset, config, true);
        let mut candidate_report = report(dataset, config, false);
        candidate_report.summary.total_tokens += 1;
        Ok(ExperimentOutcome {
            baseline_report,
            candidate_report,
        })
    }
}

#[async_trait::async_trait]
impl StudioExperimentEvaluator for RetaggingStudioEvaluator {
    async fn evaluate(
        &self,
        _candidate: &Candidate,
        dataset: &Dataset,
        config: &StudioExperimentConfig,
    ) -> Result<ExperimentOutcome, String> {
        let baseline_report = report(dataset, config, true);
        let mut candidate_report = report(dataset, config, false);
        candidate_report.cases[0].tags = vec!["forged-release-slice".to_owned()];
        Ok(ExperimentOutcome {
            baseline_report,
            candidate_report,
        })
    }
}

#[async_trait::async_trait]
impl StudioExperimentEvaluator for OversizedFailureStudioEvaluator {
    async fn evaluate(
        &self,
        _candidate: &Candidate,
        _dataset: &Dataset,
        _config: &StudioExperimentConfig,
    ) -> Result<ExperimentOutcome, String> {
        Err("sensitive-provider-detail".repeat(300_000))
    }
}

fn report(dataset: &Dataset, config: &StudioExperimentConfig, baseline: bool) -> ExperimentReport {
    let cases: Vec<Value> = dataset
        .cases()
        .iter()
        .enumerate()
        .map(|(case_index, case)| {
            let runs: Vec<Value> = (0..config.runs_per_case)
                .map(|repetition| {
                    let passed = baseline || case_index > 0;
                    json!({
                        "repetition": repetition,
                        "status": {"status": if passed { "done" } else { "interrupted" }},
                        "passed": passed,
                        "assertions": [],
                        "judge": null,
                        "tool_calls": 0,
                        "latency_ms": if baseline { 10 } else { 12 },
                        "cost_usd": 0.001,
                        "total_tokens": 10
                    })
                })
                .collect();
            let passed_runs = runs.iter().filter(|run| run["passed"] == true).count();
            json!({
                "case_id": case.id,
                "tags": case.tags,
                "pass_rate": passed_runs as f64 / config.runs_per_case as f64,
                "runs": runs
            })
        })
        .collect();
    let total_runs = dataset.cases().len() * config.runs_per_case;
    let passed_runs = cases
        .iter()
        .map(|case| (case["pass_rate"].as_f64().unwrap() * config.runs_per_case as f64) as usize)
        .sum::<usize>();
    serde_json::from_value(json!({
        "format_version": 1,
        "name": if baseline { "serving-baseline" } else { "candidate" },
        "dataset_name": dataset.name(),
        "dataset_version": dataset.version(),
        "runs_per_case": config.runs_per_case,
        "max_concurrency": config.max_concurrency,
        "cases": cases,
        "summary": {
            "cases": dataset.cases().len(), "runs": total_runs, "runs_passed": passed_runs,
            "run_pass_rate": passed_runs as f64 / total_runs as f64,
            "case_pass_rate": passed_runs as f64 / total_runs as f64,
            "assertions": [],
            "latency_ms": {
                "min": if baseline { 10 } else { 12 },
                "p50": if baseline { 10 } else { 12 },
                "p95": if baseline { 10 } else { 12 },
                "max": if baseline { 10 } else { 12 },
                "mean": if baseline { 10.0 } else { 12.0 }
            },
            "total_cost_usd": total_runs as f64 * 0.001,
            "total_tokens": total_runs * 10
        }
    }))
    .unwrap()
}

fn app_with(store: PathBuf, configure: impl FnOnce(ServerConfig) -> ServerConfig) -> Router {
    app_with_evaluator(store, configure, Arc::new(FixedStudioEvaluator))
}

fn app_with_evaluator(
    store: PathBuf,
    configure: impl FnOnce(ServerConfig) -> ServerConfig,
    evaluator: Arc<dyn StudioExperimentEvaluator>,
) -> Router {
    let config = configure(ServerConfig::new("127.0.0.1:0".parse().unwrap(), store))
        .with_studio_experiment_evaluator(evaluator);
    router(registry(), config)
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
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some((key, value)) = auth {
        builder = builder.header(key, value);
    }
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

async fn sample_dataset_as(app: &Router, auth: Option<(&str, &str)>) -> Value {
    let (assistant_status, assistant) = call_as(
        app,
        auth,
        "POST",
        "/assistants",
        Some(json!({
            "assistant_id": "assistant-support", "name": "Support", "graph": "support",
            "config": {}, "metadata": {}
        })),
    )
    .await;
    assert!(
        assistant_status == StatusCode::CREATED || assistant_status == StatusCode::OK,
        "assistant: {assistant}"
    );
    let mut sources = Vec::new();
    for input in [
        json!({"answer": "30 days"}),
        json!({"answer": "reset link"}),
    ] {
        let (thread_status, thread) = call_as(
            app,
            auth,
            "POST",
            "/threads",
            Some(json!({"graph": "support"})),
        )
        .await;
        assert_eq!(thread_status, StatusCode::CREATED, "thread: {thread}");
        let thread_id = thread["thread_id"].as_str().unwrap();
        let (run_status, run) = call_as(
            app,
            auth,
            "POST",
            &format!("/threads/{thread_id}/runs/wait"),
            Some(json!({"assistant_id": "assistant-support", "input": input})),
        )
        .await;
        assert_eq!(run_status, StatusCode::OK, "source run: {run}");
        sources.push(json!({
            "run_id": run["run_id"], "thread_id": thread_id,
            "agent_id": "assistant-support", "captured_at": "2020-01-01T00:00:00Z"
        }));
    }
    json!({
        "name": "support-q-a",
        "version": "2026-08-12",
        "cases": [
            {
                "id": "refund", "input": {"answer": "30 days"},
                "expect": {"state": [{"pointer": "/answer", "expected": "30 days"}]},
                "tags": ["refund"],
                "source": sources[0]
            },
            {
                "id": "account", "input": {"answer": "reset link"},
                "expect": {"state": [{"pointer": "/answer", "expected": "reset link"}]},
                "tags": ["account"],
                "source": sources[1]
            }
        ]
    })
}

async fn sample_dataset(app: &Router) -> Value {
    sample_dataset_as(app, None).await
}

async fn create_candidate(app: &Router) -> String {
    let (status, thread) = call(app, "POST", "/threads", Some(json!({"graph": "support"}))).await;
    assert_eq!(status, StatusCode::CREATED, "thread: {thread}");
    let thread_id = thread["thread_id"].as_str().unwrap();
    let (status, run) = call(
        app,
        "POST",
        &format!("/threads/{thread_id}/runs/wait"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "run: {run}");
    let candidate = Candidate::new(
        CandidateContent::Prompt {
            name: "support".into(),
            prompt: "Be precise".into(),
        },
        ProvenanceAuthor::Distiller {
            name: "studio-test".into(),
        },
        EvidenceSpan::default(),
        ts(),
    )
    .unwrap();
    let id = candidate.candidate_id.to_string();
    let (status, value) = call(
        app,
        "POST",
        "/learn/candidates",
        Some(json!({"candidate": candidate, "run_id": run["run_id"]})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "candidate: {value}");
    id
}

async fn wait_for_experiment(app: &Router, id: &str) -> Value {
    for _ in 0..50 {
        let (status, value) = call(app, "GET", &format!("/experiments/{id}"), None).await;
        assert_eq!(status, StatusCode::OK, "experiment: {value}");
        if value["status"]["phase"] == "complete" || value["status"]["phase"] == "failed" {
            return value;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("experiment did not settle")
}

#[tokio::test]
async fn dataset_is_immutable_provenanced_and_restart_durable() {
    let (app, store) = app();
    let dataset = sample_dataset(&app).await;
    let source_run = dataset["cases"][0]["source"]["run_id"].clone();
    let (status, created) = call(&app, "POST", "/datasets", Some(dataset.clone())).await;
    assert_eq!(status, StatusCode::CREATED, "dataset: {created}");
    assert_eq!(created["case_count"], 2);
    assert_eq!(created["digest"].as_str().unwrap().len(), 64);

    let (status, converged) = call(&app, "POST", "/datasets", Some(dataset)).await;
    assert_eq!(status, StatusCode::OK, "converge: {converged}");
    assert_eq!(converged["created"], false);

    let restarted = app_with(store, |config| config);
    let (status, listed) = call(&restarted, "GET", "/datasets", None).await;
    assert_eq!(status, StatusCode::OK, "restart list: {listed}");
    assert_eq!(listed["datasets"].as_array().unwrap().len(), 1);
    let (status, cases) = call(
        &restarted,
        "GET",
        "/datasets/support-q-a/versions/2026-08-12/cases",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "cases: {cases}");
    assert_eq!(cases["cases"][0]["source"]["run_id"], source_run);
    assert_ne!(
        cases["cases"][0]["source"]["captured_at"],
        "2020-01-01T00:00:00Z"
    );
}

#[tokio::test]
async fn dataset_rejects_forged_sources_and_bounded_work_overflow() {
    let (app, _store) = app();
    let dataset = sample_dataset(&app).await;

    let mut wrong_input = dataset.clone();
    wrong_input["cases"][0]["input"] = json!({"answer": "a different run input"});
    let (status, error) = call(&app, "POST", "/datasets", Some(wrong_input)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "wrong input: {error}");

    let mut nonexistent = dataset.clone();
    nonexistent["cases"][0]["source"]["run_id"] = json!("run-does-not-exist");
    let (status, error) = call(&app, "POST", "/datasets", Some(nonexistent)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "missing run: {error}");

    let source_case = dataset["cases"][0].clone();
    let cases = (0..=100)
        .map(|index| {
            let mut case = source_case.clone();
            case["id"] = json!(format!("case-{index}"));
            case
        })
        .collect::<Vec<_>>();
    let oversized = json!({"name": "bounded", "version": "v1", "cases": cases});
    let (status, error) = call(&app, "POST", "/datasets", Some(oversized)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "work bound: {error}");
}

#[tokio::test]
async fn tenant_isolates_the_whole_evaluation_lane() {
    let store = temp_store();
    let app = app_with(store, |config| {
        config
            .with_tenant_key("acme", "acme-secret")
            .with_tenant_key("globex", "globex-secret")
    });
    let dataset = sample_dataset_as(&app, Some(("x-api-key", "acme-secret"))).await;
    let (status, _) = call_as(
        &app,
        Some(("x-api-key", "acme-secret")),
        "POST",
        "/datasets",
        Some(dataset),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, globex) = call_as(
        &app,
        Some(("x-api-key", "globex-secret")),
        "GET",
        "/datasets",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(globex["datasets"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn experiment_runs_compares_and_saves_a_reviewed_gate() {
    let (app, _store) = app();
    let dataset = sample_dataset(&app).await;
    let (status, _) = call(&app, "POST", "/datasets", Some(dataset)).await;
    assert_eq!(status, StatusCode::CREATED);
    let candidate_id = create_candidate(&app).await;
    let payload = json!({
        "experiment_id": "exp-release", "candidate_id": candidate_id,
        "dataset_name": "support-q-a", "dataset_version": "2026-08-12",
        "runs_per_case": 3, "max_concurrency": 2, "target_metric": "case_pass_rate",
        "thresholds": {"max_pass_rate_drop": 0.05, "max_latency_p95_ratio": 1.25}
    });
    let (status, queued) = call(&app, "POST", "/experiments", Some(payload.clone())).await;
    assert_eq!(status, StatusCode::CREATED, "start: {queued}");
    let (status, converged) = call(&app, "POST", "/experiments", Some(payload)).await;
    assert_eq!(status, StatusCode::OK, "converge: {converged}");
    let settled = wait_for_experiment(&app, "exp-release").await;
    assert_eq!(settled["status"]["phase"], "complete", "settled: {settled}");
    assert_eq!(settled["comparison"]["regressed"], true);
    assert_eq!(
        settled["comparison"]["case_deltas"][0]["case_id"],
        "account"
    );
    let (status, catalog) = call(&app, "GET", "/experiments", None).await;
    assert_eq!(status, StatusCode::OK, "catalog: {catalog}");
    assert_eq!(catalog["truncated"], false);
    assert!(catalog["experiments"][0].get("baseline_report").is_none());

    let (status, comparison) = call(
        &app,
        "GET",
        "/experiments/compare?baseline=exp-release&candidate=exp-release",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "comparison: {comparison}");
    assert_eq!(comparison["comparison"]["regressed"], false);

    let (status, invalid_threshold) = call(
        &app,
        "GET",
        "/experiments/compare?baseline=exp-release&candidate=exp-release&max_pass_rate_drop=-0.1",
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "threshold: {invalid_threshold}"
    );

    let policy = json!({
        "format_version": 1, "name": "production-quality", "minimum_runs": 3,
        "minimum_run_pass_rate": 0.95, "minimum_case_pass_rate": 0.95,
        "minimum_assertion_pass_rates": {}, "minimum_tag_pass_rates": {},
        "maximum_total_cost_usd": null, "maximum_cost_ratio": null,
        "maximum_regressions": 0, "forbid_removed_cases": true,
        "comparison_thresholds": {"max_pass_rate_drop": 0.05, "max_latency_p95_ratio": 1.25}
    });
    let (status, refused) = call(
        &app,
        "POST",
        "/gates",
        Some(json!({
            "name": "production-quality", "blocked_target": "deployment:production",
            "experiment_id": "exp-release", "policy": policy, "acknowledged": false
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "unreviewed: {refused}");
    let (status, gate) = call(
        &app,
        "POST",
        "/gates",
        Some(json!({
            "name": "production-quality", "blocked_target": "deployment:production",
            "experiment_id": "exp-release", "policy": policy, "acknowledged": true
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "gate: {gate}");
    assert_eq!(gate["decision"]["outcome"], "block");
    assert_eq!(gate["dataset_name"], "support-q-a");

    let (status, converged_gate) = call(
        &app,
        "POST",
        "/gates",
        Some(json!({
            "name": "production-quality", "blocked_target": "deployment:production",
            "experiment_id": "exp-release", "policy": policy, "acknowledged": true
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "gate convergence: {converged_gate}");
    assert_eq!(converged_gate["created_at"], gate["created_at"]);
}

#[tokio::test]
async fn malformed_evaluator_evidence_fails_closed() {
    let store = temp_store();
    let app = app_with_evaluator(store, |config| config, Arc::new(MalformedStudioEvaluator));
    let dataset = sample_dataset(&app).await;
    let (status, _) = call(&app, "POST", "/datasets", Some(dataset)).await;
    assert_eq!(status, StatusCode::CREATED);
    let candidate_id = create_candidate(&app).await;
    let (status, receipt) = call(
        &app,
        "POST",
        "/experiments",
        Some(json!({
            "experiment_id": "exp-malformed", "candidate_id": candidate_id,
            "dataset_name": "support-q-a", "dataset_version": "2026-08-12",
            "runs_per_case": 1, "max_concurrency": 1, "target_metric": "case_pass_rate",
            "thresholds": {"max_pass_rate_drop": 0.05, "max_latency_p95_ratio": 1.25}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "start: {receipt}");
    let settled = wait_for_experiment(&app, "exp-malformed").await;
    assert_eq!(settled["status"]["phase"], "failed", "settled: {settled}");
    assert!(settled.get("comparison").is_none());
}

#[tokio::test]
async fn evaluator_cannot_retag_immutable_dataset_evidence() {
    let store = temp_store();
    let app = app_with_evaluator(store, |config| config, Arc::new(RetaggingStudioEvaluator));
    let dataset = sample_dataset(&app).await;
    assert_eq!(
        call(&app, "POST", "/datasets", Some(dataset)).await.0,
        StatusCode::CREATED
    );
    let candidate_id = create_candidate(&app).await;
    let (status, receipt) = call(
        &app,
        "POST",
        "/experiments",
        Some(json!({
            "experiment_id": "exp-retagged", "candidate_id": candidate_id,
            "dataset_name": "support-q-a", "dataset_version": "2026-08-12",
            "runs_per_case": 1, "max_concurrency": 1, "target_metric": "case_pass_rate",
            "thresholds": {"max_pass_rate_drop": 0.05, "max_latency_p95_ratio": 1.25}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "start: {receipt}");
    let settled = wait_for_experiment(&app, "exp-retagged").await;
    assert_eq!(settled["status"]["phase"], "failed", "settled: {settled}");
    assert!(settled.get("comparison").is_none());
}

#[tokio::test]
async fn oversized_evaluator_failures_settle_with_a_bounded_visible_reason() {
    let store = temp_store();
    let app = app_with_evaluator(
        store,
        |config| config,
        Arc::new(OversizedFailureStudioEvaluator),
    );
    let dataset = sample_dataset(&app).await;
    assert_eq!(
        call(&app, "POST", "/datasets", Some(dataset)).await.0,
        StatusCode::CREATED
    );
    let candidate_id = create_candidate(&app).await;
    let (status, receipt) = call(
        &app,
        "POST",
        "/experiments",
        Some(json!({
            "experiment_id": "exp-bounded-failure", "candidate_id": candidate_id,
            "dataset_name": "support-q-a", "dataset_version": "2026-08-12",
            "runs_per_case": 1, "max_concurrency": 1, "target_metric": "case_pass_rate",
            "thresholds": {"max_pass_rate_drop": 0.05, "max_latency_p95_ratio": 1.25}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "start: {receipt}");
    let settled = wait_for_experiment(&app, "exp-bounded-failure").await;
    assert_eq!(settled["status"]["phase"], "failed", "settled: {settled}");
    let reason = settled["status"]["reason"].as_str().unwrap();
    assert!(reason.len() < 4_200);
    assert!(reason.ends_with("(failure detail truncated)"));
}

#[tokio::test]
async fn replicas_atomically_claim_experiments_and_preserve_the_summary_catalog() {
    let store = temp_store();
    let first = app_with_evaluator(
        store.clone(),
        |config| config,
        Arc::new(NeverStudioEvaluator),
    );
    let dataset = sample_dataset(&first).await;
    assert_eq!(
        call(&first, "POST", "/datasets", Some(dataset)).await.0,
        StatusCode::CREATED
    );
    let candidate_id = create_candidate(&first).await;
    let second = app_with_evaluator(store, |config| config, Arc::new(NeverStudioEvaluator));
    let payload = |id: &str| {
        json!({
            "experiment_id": id, "candidate_id": candidate_id,
            "dataset_name": "support-q-a", "dataset_version": "2026-08-12",
            "runs_per_case": 1, "max_concurrency": 1, "target_metric": "case_pass_rate",
            "thresholds": {"max_pass_rate_drop": 0.05, "max_latency_p95_ratio": 1.25}
        })
    };
    let (same_a, same_b) = tokio::join!(
        call(
            &first,
            "POST",
            "/experiments",
            Some(payload("exp-one-owner"))
        ),
        call(
            &second,
            "POST",
            "/experiments",
            Some(payload("exp-one-owner"))
        )
    );
    let statuses = [same_a.0, same_b.0];
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == StatusCode::CREATED)
            .count(),
        1,
        "same-id receipts: {same_a:?} / {same_b:?}"
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == StatusCode::OK)
            .count(),
        1,
        "same-id receipts: {same_a:?} / {same_b:?}"
    );

    let (different_a, different_b) = tokio::join!(
        call(
            &first,
            "POST",
            "/experiments",
            Some(payload("exp-catalog-a"))
        ),
        call(
            &second,
            "POST",
            "/experiments",
            Some(payload("exp-catalog-b"))
        )
    );
    assert_eq!(
        different_a.0,
        StatusCode::CREATED,
        "first: {}",
        different_a.1
    );
    assert_eq!(
        different_b.0,
        StatusCode::CREATED,
        "second: {}",
        different_b.1
    );
    let (status, catalog) = call(&first, "GET", "/experiments", None).await;
    assert_eq!(status, StatusCode::OK, "catalog: {catalog}");
    let ids = catalog["experiments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["experiment_id"].as_str().unwrap())
        .collect::<std::collections::BTreeSet<_>>();
    assert!(ids.contains("exp-one-owner"));
    assert!(ids.contains("exp-catalog-a"));
    assert!(ids.contains("exp-catalog-b"));
}

#[tokio::test]
async fn another_server_respects_a_live_experiment_lease() {
    let store = temp_store();
    let first = app_with_evaluator(
        store.clone(),
        |config| config,
        Arc::new(NeverStudioEvaluator),
    );
    let dataset = sample_dataset(&first).await;
    let (status, _) = call(&first, "POST", "/datasets", Some(dataset)).await;
    assert_eq!(status, StatusCode::CREATED);
    let candidate_id = create_candidate(&first).await;
    let (status, queued) = call(
        &first,
        "POST",
        "/experiments",
        Some(json!({
            "experiment_id": "exp-orphaned", "candidate_id": candidate_id,
            "dataset_name": "support-q-a", "dataset_version": "2026-08-12",
            "runs_per_case": 1, "max_concurrency": 1, "target_metric": "case_pass_rate",
            "thresholds": {"max_pass_rate_drop": 0.05, "max_latency_p95_ratio": 1.25}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "start: {queued}");
    tokio::time::sleep(Duration::from_millis(10)).await;

    let restarted = app_with(store, |config| config);
    let (status, recovered) = call(&restarted, "GET", "/experiments/exp-orphaned", None).await;
    assert_eq!(status, StatusCode::OK, "restart status: {recovered}");
    assert_eq!(recovered["status"]["phase"], "running");
}
