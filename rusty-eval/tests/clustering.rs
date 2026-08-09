use std::collections::BTreeMap;

use rusty_eval::{
    cluster_failures, AssertionPassRate, AssertionResult, CaseReport, CaseRunReport,
    ExecutionFailureCategory, ExperimentReport, FailureCause, FailureClusterReport,
    FailureTermination, JudgeVerdict, LatencyStats, ReportSummary, RunStatus,
    FAILURE_CLUSTER_REPORT_FORMAT_VERSION, REPORT_FORMAT_VERSION,
};
use serde_json::json;

fn passed(repetition: usize) -> CaseRunReport {
    run(repetition, RunStatus::Done, Vec::new(), None)
}

fn execution_failure(repetition: usize, error: &str) -> CaseRunReport {
    run(
        repetition,
        RunStatus::Failed {
            error: error.to_owned(),
        },
        Vec::new(),
        None,
    )
}

fn interrupted(repetition: usize) -> CaseRunReport {
    run(repetition, RunStatus::Interrupted, Vec::new(), None)
}

fn assertion_failure(repetition: usize, name: &str, detail: &str) -> CaseRunReport {
    assertion_failure_expected(repetition, name, detail, json!(true))
}

fn assertion_failure_expected(
    repetition: usize,
    name: &str,
    detail: &str,
    expected: serde_json::Value,
) -> CaseRunReport {
    run(
        repetition,
        RunStatus::Done,
        vec![AssertionResult {
            assertion: name.to_owned(),
            passed: false,
            expected,
            observed: json!(false),
            detail: Some(detail.to_owned()),
        }],
        None,
    )
}

fn judge_failure(repetition: usize, rationale: &str) -> CaseRunReport {
    run(
        repetition,
        RunStatus::Done,
        Vec::new(),
        Some(JudgeVerdict {
            score: 0.2,
            passed: false,
            rationale: rationale.to_owned(),
        }),
    )
}

fn compound_failure(repetition: usize, assertion: &str, rationale: &str) -> CaseRunReport {
    run(
        repetition,
        RunStatus::Done,
        vec![failed_assertion(assertion, "assertion rejected output")],
        Some(JudgeVerdict {
            score: 0.1,
            passed: false,
            rationale: rationale.to_owned(),
        }),
    )
}

fn failed_assertion(name: &str, detail: &str) -> AssertionResult {
    AssertionResult {
        assertion: name.to_owned(),
        passed: false,
        expected: json!(true),
        observed: json!(false),
        detail: Some(detail.to_owned()),
    }
}

fn run(
    repetition: usize,
    status: RunStatus,
    assertions: Vec<AssertionResult>,
    judge: Option<JudgeVerdict>,
) -> CaseRunReport {
    let passed = status.is_done()
        && assertions.iter().all(|assertion| assertion.passed)
        && judge.as_ref().is_none_or(|judge| judge.passed);
    CaseRunReport {
        repetition,
        status,
        passed,
        assertions,
        judge,
        tool_calls: 0,
        latency_ms: 0,
        cost_usd: 0.0,
        total_tokens: 0,
    }
}

fn case(id: &str, tags: &[&str], runs: Vec<CaseRunReport>) -> CaseReport {
    let passed = runs.iter().filter(|run| run.passed).count();
    CaseReport {
        case_id: id.to_owned(),
        tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
        pass_rate: passed as f64 / runs.len() as f64,
        runs,
    }
}

fn report(name: &str, cases: Vec<CaseReport>) -> ExperimentReport {
    let runs_per_case = cases.first().map(|case| case.runs.len()).unwrap_or(1);
    assert!(cases.iter().all(|case| case.runs.len() == runs_per_case));
    let runs: Vec<_> = cases.iter().flat_map(|case| &case.runs).collect();
    let runs_passed = runs.iter().filter(|run| run.passed).count();
    let mut assertion_totals: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for run in &runs {
        for assertion in &run.assertions {
            let entry = assertion_totals
                .entry(assertion.assertion.clone())
                .or_default();
            entry.1 += 1;
            entry.0 += usize::from(assertion.passed);
        }
    }
    let assertions = assertion_totals
        .into_iter()
        .map(|(assertion, (passed, total))| AssertionPassRate {
            assertion,
            passed,
            total,
            rate: passed as f64 / total as f64,
        })
        .collect();
    let run_pass_rate = if runs.is_empty() {
        0.0
    } else {
        runs_passed as f64 / runs.len() as f64
    };
    let case_pass_rate = if cases.is_empty() {
        0.0
    } else {
        cases.iter().map(|case| case.pass_rate).sum::<f64>() / cases.len() as f64
    };
    ExperimentReport {
        format_version: REPORT_FORMAT_VERSION,
        name: name.to_owned(),
        dataset_name: "support".to_owned(),
        dataset_version: "v4".to_owned(),
        runs_per_case,
        max_concurrency: 1,
        summary: ReportSummary {
            cases: cases.len(),
            runs: runs.len(),
            runs_passed,
            run_pass_rate,
            case_pass_rate,
            assertions,
            latency_ms: LatencyStats {
                min: 0,
                p50: 0,
                p95: 0,
                max: 0,
                mean: 0.0,
            },
            total_cost_usd: 0.0,
            total_tokens: 0,
        },
        cases,
    }
}

fn mixed_failures() -> ExperimentReport {
    report(
        "candidate",
        vec![
            case(
                "alpha",
                &["priority", "billing", "priority"],
                vec![
                    execution_failure(0, "connection reset for request 12"),
                    execution_failure(1, "connection reset for request 99"),
                    assertion_failure(2, "tool_call_order", "calculator was never called"),
                ],
            ),
            case(
                "beta",
                &["support"],
                vec![
                    assertion_failure(0, "tool_call_order", "search was never called"),
                    judge_failure(1, "answer cites an unsupported policy"),
                    passed(2),
                ],
            ),
        ],
    )
}

fn find_cluster(
    report: &FailureClusterReport,
    predicate: impl Fn(&rusty_eval::FailureCluster) -> bool,
) -> &rusty_eval::FailureCluster {
    report
        .clusters
        .iter()
        .find(|cluster| predicate(cluster))
        .expect("signature should be clustered")
}

#[test]
fn failures_group_by_explainable_signature() {
    let result = cluster_failures(&mixed_failures()).unwrap();

    assert_eq!(result.format_version, FAILURE_CLUSTER_REPORT_FORMAT_VERSION);
    assert_eq!(result.total_runs, 6);
    assert_eq!(result.total_failures, 5);
    assert_eq!(result.clusters.len(), 3);
    assert!(result
        .clusters
        .windows(2)
        .all(|pair| pair[0].occurrences >= pair[1].occurrences));

    let execution = find_cluster(&result, |cluster| {
        matches!(
            cluster.signature.cause,
            FailureCause::Execution {
                category: ExecutionFailureCategory::Transport,
                ..
            }
        )
    });
    assert_eq!(execution.occurrences, 2);
    assert_eq!(execution.share_of_failures, 0.4);
    assert_eq!(execution.id.len(), 35);
    assert!(execution.id.starts_with("fc_"));
    assert_eq!(execution.members[0].evidence, execution.members[1].evidence);
    assert_eq!(execution.members[0].tags, vec!["billing", "priority"]);
    let serialized = result.to_json().unwrap();
    assert!(!serialized.contains("connection reset"));
    assert!(!serialized.contains("unsupported policy"));

    let assertion = find_cluster(&result, |cluster| {
        cluster.signature.termination == FailureTermination::Done
            && cluster
                .signature
                .failed_assertions
                .iter()
                .any(|failure| failure.assertion == "tool_call_order")
            && cluster.signature.judge_fingerprint.is_none()
    });
    assert_eq!(assertion.occurrences, 2);
    assert_eq!(assertion.members[0].case_id, "alpha");
    assert_eq!(assertion.members[1].case_id, "beta");

    let judge = find_cluster(&result, |cluster| {
        cluster.signature.termination == FailureTermination::Done
            && cluster.signature.failed_assertions.is_empty()
            && cluster.signature.judge_fingerprint.is_some()
    });
    assert_eq!(judge.occurrences, 1);
    assert_eq!(judge.members[0].evidence.len(), 1);
}

#[test]
fn clustering_is_invariant_to_case_and_run_order() {
    let source = mixed_failures();
    let expected = cluster_failures(&source).unwrap();
    let mut reordered = source.clone();
    reordered.cases.reverse();
    for case in &mut reordered.cases {
        case.runs.reverse();
    }

    let actual = cluster_failures(&reordered).unwrap();

    assert_eq!(actual, expected);
    assert_eq!(actual.to_json().unwrap(), expected.to_json().unwrap());
}

#[test]
fn runtime_error_text_does_not_fragment_cluster_identity() {
    let first = report(
        "candidate",
        vec![case("only", &[], vec![execution_failure(0, "timeout 1")])],
    );
    let second = report(
        "candidate",
        vec![case("only", &[], vec![execution_failure(0, "timeout 999")])],
    );

    let first = cluster_failures(&first).unwrap();
    let second = cluster_failures(&second).unwrap();

    assert_eq!(first.clusters[0].id, second.clusters[0].id);
    assert_eq!(first.clusters[0].signature, second.clusters[0].signature);
    assert_eq!(first.clusters[0].members, second.clusters[0].members);
    assert!(!first.to_json().unwrap().contains("timeout 1"));
    assert!(!second.to_json().unwrap().contains("timeout 999"));
}

#[test]
fn embedded_volatile_ids_do_not_fragment_cluster_identity() {
    let errors = [
        "upstream failed request_id=550e8400-e29b-41d4-a716-446655440000 trace=deadbeef12345678",
        "upstream failed request_id=123e4567-e89b-12d3-a456-426614174000 trace=0123456789abcdef",
        "upstream failed {\"request_id\":\"550e8400-e29b-41d4-a716-446655440000\",\"trace\":\"deadbeef12345678\"}",
    ];
    let clustered: Vec<_> = errors
        .iter()
        .map(|error| {
            cluster_failures(&report(
                "candidate",
                vec![case("only", &[], vec![execution_failure(0, error)])],
            ))
            .unwrap()
        })
        .collect();

    assert!(clustered
        .windows(2)
        .all(|pair| pair[0].clusters[0].signature == pair[1].clusters[0].signature));
}

#[test]
fn assertion_expected_objects_are_fingerprinted_in_canonical_key_order() {
    let mut first_nested = serde_json::Map::new();
    first_nested.insert("beta".to_owned(), json!([2, {"z": false, "a": null}]));
    first_nested.insert("alpha".to_owned(), json!(1));
    let mut second_nested = serde_json::Map::new();
    second_nested.insert("alpha".to_owned(), json!(1));
    second_nested.insert("beta".to_owned(), json!([2, {"a": null, "z": false}]));

    let source = report(
        "candidate",
        vec![case(
            "only",
            &[],
            vec![
                assertion_failure_expected(
                    0,
                    "shape",
                    "first order",
                    serde_json::Value::Object(first_nested),
                ),
                assertion_failure_expected(
                    1,
                    "shape",
                    "second order",
                    serde_json::Value::Object(second_nested),
                ),
            ],
        )],
    );

    let result = cluster_failures(&source).unwrap();

    assert_eq!(result.clusters.len(), 1);
    assert_eq!(result.clusters[0].occurrences, 2);
}

#[test]
fn executor_categories_separate_actionable_root_causes() {
    let source = report(
        "candidate",
        vec![case(
            "only",
            &[],
            vec![
                execution_failure(0, "request timed out after 30 seconds"),
                execution_failure(1, "401 unauthorized token for request 77"),
            ],
        )],
    );

    let result = cluster_failures(&source).unwrap();

    assert_eq!(result.clusters.len(), 2);
    let categories: Vec<_> = result
        .clusters
        .iter()
        .map(|cluster| match cluster.signature.cause {
            FailureCause::Execution { category, .. } => category,
            _ => panic!("expected execution failure"),
        })
        .collect();
    assert!(categories.contains(&ExecutionFailureCategory::Timeout));
    assert!(categories.contains(&ExecutionFailureCategory::Authentication));
    assert_ne!(result.clusters[0].id, result.clusters[1].id);
}

#[test]
fn compound_and_termination_failures_remain_distinct() {
    let source = report(
        "candidate",
        vec![case(
            "only",
            &[],
            vec![
                compound_failure(0, "grounded", "unsupported claim"),
                assertion_failure(1, "grounded", "citation missing"),
                interrupted(2),
            ],
        )],
    );

    let result = cluster_failures(&source).unwrap();

    assert_eq!(result.total_failures, 3);
    assert_eq!(result.clusters.len(), 3);
    assert!(result.clusters.iter().any(|cluster| {
        cluster
            .signature
            .failed_assertions
            .iter()
            .any(|failure| failure.assertion == "grounded")
            && cluster.signature.judge_fingerprint.is_some()
    }));
    assert!(result.clusters.iter().any(|cluster| {
        cluster.signature.termination == FailureTermination::Interrupted
            && cluster.signature.failed_assertions.is_empty()
    }));
}

#[test]
fn successful_report_has_no_failure_clusters() {
    let source = report(
        "candidate",
        vec![case("only", &[], vec![passed(0), passed(1), passed(2)])],
    );

    let result = cluster_failures(&source).unwrap();

    assert_eq!(result.total_runs, 3);
    assert_eq!(result.total_failures, 0);
    assert!(result.clusters.is_empty());
    assert!(FailureClusterReport::from_json(&result.to_json().unwrap(), &source).is_ok());
}

#[test]
fn contradictory_source_evidence_is_rejected() {
    let mut source = mixed_failures();
    source.cases[0].runs[0].passed = true;

    let error = cluster_failures(&source).unwrap_err();

    assert!(error.to_string().contains("contradicts its evidence"));
}

#[test]
fn cluster_artifact_round_trip_rejects_future_and_forged_data() {
    let source = mixed_failures();
    let result = cluster_failures(&source).unwrap();
    let json = result.to_json().unwrap();

    let loaded = FailureClusterReport::from_json(&json, &source).unwrap();
    assert_eq!(loaded.experiment, result.experiment);
    assert_eq!(loaded.total_failures, result.total_failures);
    assert_eq!(loaded.clusters.len(), result.clusters.len());

    let mut future: serde_json::Value = serde_json::from_str(&json).unwrap();
    future["format_version"] = json!(2);
    let error = FailureClusterReport::from_json(&future.to_string(), &source).unwrap_err();
    assert!(error
        .to_string()
        .contains("unsupported failure cluster format version"));

    let mut forged: serde_json::Value = serde_json::from_str(&json).unwrap();
    forged["clusters"][0]["members"][0]["tags"]
        .as_array_mut()
        .unwrap()
        .push(json!("zz-forged"));
    let error = FailureClusterReport::from_json(&forged.to_string(), &source).unwrap_err();
    assert!(error
        .to_string()
        .contains("does not match its source evidence"));

    let mut unknown: serde_json::Value = serde_json::from_str(&json).unwrap();
    unknown["clusters"][0]["unexpected"] = json!(true);
    assert!(FailureClusterReport::from_json(&unknown.to_string(), &source).is_err());

    let mut nearby_share: serde_json::Value = serde_json::from_str(&json).unwrap();
    let supplied = nearby_share["clusters"][0]["share_of_failures"]
        .as_f64()
        .unwrap();
    nearby_share["clusters"][0]["share_of_failures"] = json!(supplied + f64::EPSILON);
    let normalized = FailureClusterReport::from_json(&nearby_share.to_string(), &source).unwrap();
    assert_eq!(
        normalized.clusters[0].share_of_failures,
        result.clusters[0].share_of_failures
    );
}

#[test]
fn stable_cluster_id_matches_golden_vector() {
    let source = report(
        "candidate",
        vec![case("only", &[], vec![execution_failure(0, "timeout 123")])],
    );

    let result = cluster_failures(&source).unwrap();

    assert_eq!(result.clusters[0].id, "fc_47b49650d957bf634d336415945dd77c");
}
