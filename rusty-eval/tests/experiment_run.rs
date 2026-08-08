//! Experiment-run integration tests: a deterministic ReAct agent (scripted
//! model + real calculator tool, no live LLM) driven through the experiment
//! runner end to end — evidence distillation, assertion grading, reports,
//! judge verdicts, and baseline-vs-candidate comparison.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use rusty_agent_runtime::error::Result as RuntimeResult;
use rusty_agent_runtime::journal::Journal;
use rusty_agent_runtime::llm::{ChatMessage, ChatModel, ChatResponse, ToolCall};
use rusty_agent_runtime::react::{create_react_agent_with_recording, MESSAGES_CHANNEL};
use rusty_agent_runtime::state::{Reducer, StateSpec};
use rusty_agent_runtime::tool::{Tool, ToolRegistry};

use rusty_eval::{
    compare, AssertionPassRate, CaseChange, CompareThresholds, Dataset, EvalCase, Expectation,
    ExperimentConfig, ExperimentReport, ExperimentRunner, ExpectedToolCall, LatencyStats,
    PreparedRun, Regression, ReportSummary, RuleBasedJudge, RunStatus, StatePredicate,
};

// ---------- the agent under test: scripted model + calculator tool ----------

/// A scripted model: pops one canned response per `chat` call. Deterministic,
/// no network — the experiment is fully reproducible.
struct ScriptedModel {
    script: Mutex<VecDeque<ChatMessage>>,
}

impl ScriptedModel {
    fn new(script: Vec<ChatMessage>) -> Self {
        Self {
            script: Mutex::new(script.into()),
        }
    }
}

#[async_trait::async_trait]
impl ChatModel for ScriptedModel {
    async fn chat(&self, _messages: &[ChatMessage], _tools: &[Value]) -> RuntimeResult<ChatResponse> {
        let message = self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .expect("test script is exactly as long as the ReAct loop");
        Ok(ChatResponse {
            message,
            model: Some("scripted-eval-1".into()),
            usage: None,
        })
    }
}

/// A deterministic four-function calculator (subset: add/mul).
struct CalculatorTool;

#[async_trait::async_trait]
impl Tool for CalculatorTool {
    fn name(&self) -> &str {
        "calculator"
    }

    fn description(&self) -> &str {
        "Evaluate an arithmetic operation on two numbers."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "op": {"type": "string", "enum": ["add", "mul"]},
                "a": {"type": "number"},
                "b": {"type": "number"},
            },
            "required": ["op", "a", "b"],
        })
    }

    async fn call(&self, args: Value) -> RuntimeResult<Value> {
        let a = args["a"].as_f64().unwrap_or(0.0);
        let b = args["b"].as_f64().unwrap_or(0.0);
        let result = match args["op"].as_str() {
            Some("add") => a + b,
            Some("mul") => a * b,
            other => panic!("test script only issues add/mul, got {other:?}"),
        };
        Ok(json!(result))
    }
}

/// The good agent's script, per case: the correct tool calls, then the final
/// answer.
fn good_script(case_id: &str) -> Vec<ChatMessage> {
    match case_id {
        "add-two-numbers" => vec![
            ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                "c1",
                "calculator",
                json!({"op": "add", "a": 2, "b": 3}),
            )]),
            ChatMessage::assistant("the answer is 5"),
        ],
        "mul-then-add" => vec![
            ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                "c1",
                "calculator",
                json!({"op": "mul", "a": 2, "b": 3}),
            )]),
            ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                "c2",
                "calculator",
                json!({"op": "add", "a": 6, "b": 4}),
            )]),
            ChatMessage::assistant("the answer is 10"),
        ],
        other => panic!("unknown case {other}"),
    }
}

/// The regressed agent: answers directly, never calls a tool.
fn bad_script(case_id: &str) -> Vec<ChatMessage> {
    let answer = match case_id {
        "add-two-numbers" => "the answer is 5",
        "mul-then-add" => "the answer is 10",
        other => panic!("unknown case {other}"),
    };
    vec![ChatMessage::assistant(answer)]
}

fn spec() -> StateSpec {
    StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages)
}

fn dataset() -> Dataset {
    let cases = vec![
        EvalCase {
            id: "add-two-numbers".to_owned(),
            input: json!({MESSAGES_CHANNEL: [{"role": "user", "content": "what is 2 + 3?"}]}),
            expect: Expectation {
                tool_trajectory: vec![ExpectedToolCall {
                    name: "calculator".to_owned(),
                    args: serde_json::Map::from_iter([("/op".to_owned(), json!("add"))]),
                }],
                state: vec![StatePredicate {
                    pointer: "/messages/3/content".to_owned(),
                    expected: json!("the answer is 5"),
                }],
                forbid_tools: vec!["shell".to_owned()],
                max_cost_usd: Some(0.01),
                max_latency_ms: Some(60_000),
            },
            tags: vec!["math".to_owned(), "smoke".to_owned()],
        },
        EvalCase {
            id: "mul-then-add".to_owned(),
            input: json!({MESSAGES_CHANNEL: [{"role": "user", "content": "what is 2 * 3 + 4?"}]}),
            expect: Expectation {
                tool_trajectory: vec![
                    ExpectedToolCall {
                        name: "calculator".to_owned(),
                        args: serde_json::Map::from_iter([("/op".to_owned(), json!("mul"))]),
                    },
                    ExpectedToolCall {
                        name: "calculator".to_owned(),
                        args: serde_json::Map::from_iter([("/op".to_owned(), json!("add"))]),
                    },
                ],
                state: vec![StatePredicate {
                    pointer: "/messages/5/content".to_owned(),
                    expected: json!("the answer is 10"),
                }],
                forbid_tools: vec![],
                max_cost_usd: None,
                max_latency_ms: None,
            },
            tags: vec!["math".to_owned()],
        },
    ];
    Dataset::new("math-tools", "1.0.0", cases).unwrap()
}

/// Build the recording ReAct graph for one case run with `script`.
fn build_run(script: Vec<ChatMessage>, journal: &Journal) -> RuntimeResult<PreparedRun> {
    let model = Arc::new(ScriptedModel::new(script));
    let mut tools = ToolRegistry::new();
    tools.register(CalculatorTool);
    let graph = create_react_agent_with_recording(model, tools, journal.clone())?;
    Ok(PreparedRun::new(graph, spec()))
}

async fn run_experiment(good: bool, runs_per_case: usize, judge: bool) -> ExperimentReport {
    let dataset = dataset();
    let mut config = ExperimentConfig::new().with_runs_per_case(runs_per_case);
    if judge {
        config = config.with_judge(Arc::new(RuleBasedJudge::new()));
    }
    let runner = ExperimentRunner::new(config);
    let prepare = |case: &EvalCase, journal: &Journal| -> RuntimeResult<PreparedRun> {
        let script = if good {
            good_script(&case.id)
        } else {
            bad_script(&case.id)
        };
        build_run(script, journal)
    };
    runner.run(&dataset, prepare).await.unwrap()
}

// ---------- tests ----------

#[tokio::test]
async fn experiment_grades_a_good_agent_as_passing() {
    let report = run_experiment(true, 2, false).await;

    assert_eq!(report.name, "math-tools@1.0.0");
    assert_eq!(report.runs_per_case, 2);
    assert_eq!(report.cases.len(), 2);

    // Every run completed, passed, and journaled exactly its expected calls.
    for case in &report.cases {
        assert_eq!(case.pass_rate, 1.0, "case {}", case.case_id);
        for run in &case.runs {
            assert_eq!(run.status, RunStatus::Done);
            assert!(run.passed, "case {} run {}", case.case_id, run.repetition);
            assert!(run.assertions.iter().all(|result| result.passed));
            assert_eq!(run.cost_usd, 0.0);
        }
    }
    assert_eq!(report.cases[0].runs[0].tool_calls, 1);
    assert_eq!(report.cases[1].runs[0].tool_calls, 2);

    let summary = &report.summary;
    assert_eq!(summary.cases, 2);
    assert_eq!(summary.runs, 4);
    assert_eq!(summary.runs_passed, 4);
    assert_eq!(summary.run_pass_rate, 1.0);
    assert_eq!(summary.case_pass_rate, 1.0);
    // Both repetitions of both cases evaluated the shared assertion kinds.
    let rate_of = |name: &str| -> &AssertionPassRate {
        summary
            .assertions
            .iter()
            .find(|rate| rate.assertion == name)
            .unwrap_or_else(|| panic!("missing assertion aggregate {name}"))
    };
    assert_eq!(rate_of("tool_call_order").rate, 1.0);
    assert_eq!(rate_of("tool_call_order").total, 4);
    assert_eq!(rate_of("no_tool_call").total, 2); // only the first case declares it
    assert_eq!(rate_of("max_cost").total, 2);
    assert_eq!(rate_of("max_latency").total, 2);
    // Percentiles exist and are ordered (exact values are timing-dependent).
    assert!(summary.latency_ms.min <= summary.latency_ms.p50);
    assert!(summary.latency_ms.p50 <= summary.latency_ms.p95);
    assert!(summary.latency_ms.p95 <= summary.latency_ms.max);
}

#[tokio::test]
async fn experiment_grades_a_tool_skipping_agent_as_failing() {
    let report = run_experiment(false, 1, false).await;

    assert_eq!(report.summary.run_pass_rate, 0.0);
    for case in &report.cases {
        let run = &case.runs[0];
        // The run completes — it is just wrong: trajectory and final-state
        // assertions fail with evidence, the run still finishes cleanly.
        assert_eq!(run.status, RunStatus::Done);
        assert!(!run.passed);
        assert_eq!(run.tool_calls, 0);
        let trajectory = run
            .assertions
            .iter()
            .find(|result| result.assertion == "tool_call_order")
            .unwrap();
        assert!(!trajectory.passed);
        assert_eq!(trajectory.observed, json!([]));
        assert!(trajectory.detail.is_some());
    }
}

#[tokio::test]
async fn rule_based_judge_scores_runs_without_a_live_model() {
    let good = run_experiment(true, 1, true).await;
    for case in &good.cases {
        let verdict = case.runs[0].judge.as_ref().expect("judge attached");
        assert!(verdict.passed);
        assert_eq!(verdict.score, 1.0);
    }

    let bad = run_experiment(false, 1, true).await;
    // The tool-skipping agent still completes cheaply and legally, so it
    // meets the bound/blacklist expectations while failing trajectory and
    // state: score = fraction of expectations met, per case.
    let add = &bad.cases[0].runs[0].judge.as_ref().expect("judge attached");
    assert!(!add.passed);
    assert_eq!(add.score, 0.6); // 3 of 5: no_tool_call, max_cost, max_latency
    assert!(add.rationale.contains("failed:"), "{}", add.rationale);

    let mul = &bad.cases[1].runs[0].judge.as_ref().expect("judge attached");
    assert!(!mul.passed);
    assert_eq!(mul.score, 0.0); // 0 of 2: trajectory + state both fail
}

#[tokio::test]
async fn comparison_flags_the_regressed_candidate() {
    let baseline = run_experiment(true, 1, false).await;
    let candidate = run_experiment(false, 1, false).await;

    let verdict = compare(&baseline, &candidate, &CompareThresholds::default());
    assert!(verdict.regressed);

    // Per-assertion: trajectory and state predicates dropped 1.0 -> 0.0.
    let order = verdict
        .assertion_deltas
        .iter()
        .find(|delta| delta.assertion == "tool_call_order")
        .unwrap();
    assert_eq!(order.baseline_rate, 1.0);
    assert_eq!(order.candidate_rate, 0.0);
    assert_eq!(order.delta, -1.0);

    assert!(verdict
        .regressions
        .iter()
        .any(|flag| matches!(flag, Regression::AssertionPassRate { assertion, .. } if assertion == "tool_call_order")));

    // Both cases regressed and flagged.
    assert_eq!(verdict.case_deltas.len(), 2);
    for delta in &verdict.case_deltas {
        assert_eq!(delta.change, CaseChange::Regressed);
        assert_eq!(delta.baseline_pass_rate, Some(1.0));
        assert_eq!(delta.candidate_pass_rate, Some(0.0));
    }
    assert_eq!(
        verdict
            .regressions
            .iter()
            .filter(|flag| matches!(flag, Regression::CasePassRate { .. }))
            .count(),
        2
    );
}

#[tokio::test]
async fn comparison_of_identical_reports_is_clean() {
    let baseline = run_experiment(true, 1, false).await;
    let verdict = compare(&baseline, &baseline, &CompareThresholds::default());

    assert!(!verdict.regressed);
    assert!(verdict.regressions.is_empty());
    assert!(verdict.assertion_deltas.iter().all(|delta| delta.delta == 0.0));
    assert!(verdict
        .case_deltas
        .iter()
        .all(|delta| delta.change == CaseChange::Unchanged));
}

// ---------- fabricated reports: latency regression + report persistence ----------

/// A minimal report with controlled summary numbers (no case detail), for
/// exercising comparison math without running agents.
fn fabricated_report(name: &str, p50: u64, p95: u64, cost: f64) -> ExperimentReport {
    ExperimentReport {
        format_version: rusty_eval::REPORT_FORMAT_VERSION,
        name: name.to_owned(),
        dataset_name: "d".to_owned(),
        dataset_version: "1".to_owned(),
        runs_per_case: 1,
        cases: vec![],
        summary: ReportSummary {
            cases: 0,
            runs: 0,
            runs_passed: 0,
            run_pass_rate: 0.0,
            case_pass_rate: 0.0,
            assertions: vec![],
            latency_ms: LatencyStats {
                min: p50,
                p50,
                p95,
                max: p95,
                mean: p50 as f64,
            },
            total_cost_usd: cost,
            total_tokens: 0,
        },
    }
}

#[test]
fn comparison_flags_p95_latency_regression_beyond_threshold() {
    let baseline = fabricated_report("base", 100, 100, 0.0);
    let candidate = fabricated_report("cand", 100, 200, 0.0);

    let verdict = compare(&baseline, &candidate, &CompareThresholds::default());
    assert!(verdict.regressed);
    assert!(verdict.regressions.iter().any(|flag| matches!(
        flag,
        Regression::LatencyP95 { baseline_ms: 100, candidate_ms: 200, ratio } if *ratio == 2.0
    )));
    assert_eq!(verdict.latency.p95_ratio, Some(2.0));

    // Within threshold: 20% growth under a 25% tolerance is clean.
    let ok = fabricated_report("cand", 100, 120, 0.0);
    assert!(!compare(&baseline, &ok, &CompareThresholds::default()).regressed);
}

#[test]
fn comparison_treats_zero_baseline_latency_as_breached_by_any_growth() {
    let baseline = fabricated_report("base", 0, 0, 0.0);
    let candidate = fabricated_report("cand", 0, 5, 0.0);
    let verdict = compare(&baseline, &candidate, &CompareThresholds::default());
    assert!(verdict.regressions.iter().any(|flag| matches!(
        flag,
        Regression::LatencyP95 { ratio, .. } if *ratio == -1.0
    )));
}

#[test]
fn report_json_round_trip_and_version_guard() {
    let report = fabricated_report("r", 1, 2, 0.5);
    let json = report.to_json().unwrap();
    let parsed = ExperimentReport::from_json(&json).unwrap();
    assert_eq!(parsed, report);

    let bumped = json.replace(
        &format!("\"format_version\": {}", rusty_eval::REPORT_FORMAT_VERSION),
        "\"format_version\": 99",
    );
    assert_ne!(bumped, json, "test precondition: version string replaced");
    assert!(ExperimentReport::from_json(&bumped).is_err());
}
