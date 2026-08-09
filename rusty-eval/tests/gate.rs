use std::collections::BTreeMap;

use rusty_eval::{
    evaluate_gate, AssertionPassRate, AssertionResult, CaseReport, CaseRunReport, EvalError,
    ExperimentReport, GateDecision, GateMetric, GateOutcome, GatePolicy, LatencyStats,
    ReportSummary, RunStatus, GATE_DECISION_FORMAT_VERSION, GATE_POLICY_FORMAT_VERSION,
    REPORT_FORMAT_VERSION,
};

fn case(id: &str, pass_rate: f64, tags: &[&str]) -> CaseReport {
    CaseReport {
        case_id: id.to_owned(),
        tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
        pass_rate,
        runs: vec![],
    }
}

fn report(
    name: &str,
    runs_per_case: usize,
    _run_pass_rate: f64,
    cost: f64,
    mut cases: Vec<CaseReport>,
    assertions: &[(&str, f64)],
) -> ExperimentReport {
    let total_runs = cases.len() * runs_per_case;
    let cost_per_run = if total_runs == 0 {
        0.0
    } else {
        cost / total_runs as f64
    };
    for case in &mut cases {
        let passed_runs = (runs_per_case as f64 * case.pass_rate).round() as usize;
        case.runs = (0..runs_per_case)
            .map(|repetition| {
                let passed = repetition < passed_runs;
                let mut results = vec![AssertionResult {
                    assertion: "case_quality".to_owned(),
                    passed,
                    expected: serde_json::json!(true),
                    observed: serde_json::json!(passed),
                    detail: (!passed).then(|| "case quality failed".to_owned()),
                }];
                results.extend(assertions.iter().map(|(name, rate)| {
                    let assertion_passed =
                        repetition < (runs_per_case as f64 * rate).round() as usize;
                    AssertionResult {
                        assertion: (*name).to_owned(),
                        passed: assertion_passed,
                        expected: serde_json::json!(true),
                        observed: serde_json::json!(assertion_passed),
                        detail: (!assertion_passed).then(|| format!("{name} failed")),
                    }
                }));
                CaseRunReport {
                    repetition,
                    status: RunStatus::Done,
                    passed,
                    assertions: results,
                    judge: None,
                    tool_calls: 0,
                    latency_ms: 10,
                    cost_usd: cost_per_run,
                    total_tokens: 10,
                }
            })
            .collect();
    }
    let runs = cases.iter().map(|case| case.runs.len()).sum::<usize>();
    let runs_passed = cases
        .iter()
        .flat_map(|case| &case.runs)
        .filter(|run| run.passed)
        .count();
    let mut assertion_totals: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for run in cases.iter().flat_map(|case| &case.runs) {
        for assertion in &run.assertions {
            let entry = assertion_totals
                .entry(assertion.assertion.clone())
                .or_default();
            entry.0 += usize::from(assertion.passed);
            entry.1 += 1;
        }
    }
    let case_pass_rate = if cases.is_empty() {
        0.0
    } else {
        cases.iter().map(|case| case.pass_rate).sum::<f64>() / cases.len() as f64
    };
    ExperimentReport {
        format_version: REPORT_FORMAT_VERSION,
        name: name.to_owned(),
        dataset_name: "release".to_owned(),
        dataset_version: "1".to_owned(),
        runs_per_case,
        summary: ReportSummary {
            cases: cases.len(),
            runs,
            runs_passed,
            run_pass_rate: if runs == 0 {
                0.0
            } else {
                runs_passed as f64 / runs as f64
            },
            case_pass_rate,
            assertions: assertion_totals
                .into_iter()
                .map(|(assertion, (passed, total))| AssertionPassRate {
                    assertion,
                    passed,
                    total,
                    rate: passed as f64 / total as f64,
                })
                .collect(),
            latency_ms: LatencyStats {
                min: 10,
                p50: 10,
                p95: 10,
                max: 10,
                mean: 10.0,
            },
            total_cost_usd: cost_per_run * runs as f64,
            total_tokens: runs as u64 * 10,
        },
        cases,
    }
}

fn clean_reports() -> (ExperimentReport, ExperimentReport) {
    (
        report(
            "baseline",
            20,
            1.0,
            1.0,
            vec![case("critical", 1.0, &["smoke"]), case("normal", 1.0, &[])],
            &[("grounded", 1.0), ("safe", 1.0)],
        ),
        report(
            "candidate",
            20,
            1.0,
            1.0,
            vec![case("critical", 1.0, &["smoke"]), case("normal", 1.0, &[])],
            &[("grounded", 1.0), ("safe", 1.0)],
        ),
    )
}

#[test]
fn strict_policy_allows_a_clean_candidate() {
    let (baseline, candidate) = clean_reports();
    let policy = GatePolicy::strict("production")
        .unwrap()
        .with_assertion_minimum("safe", 1.0)
        .unwrap()
        .with_tag_minimum("smoke", 1.0)
        .unwrap()
        .with_maximum_total_cost_usd(2.0)
        .unwrap()
        .with_maximum_cost_ratio(1.1)
        .unwrap();

    let decision = evaluate_gate(&policy, &candidate, Some(&baseline)).unwrap();
    assert!(decision.allowed());
    assert_eq!(decision.outcome(), GateOutcome::Allow);
    assert!(decision.checks().iter().all(|check| check.passed));
    assert_eq!(decision.baseline(), Some("baseline"));
}

#[test]
fn policy_reports_every_candidate_failure_in_stable_order() {
    let candidate = report(
        "candidate",
        4,
        0.5,
        3.0,
        vec![case("critical", 0.25, &["smoke"])],
        &[("safe", 0.5)],
    );
    let policy = GatePolicy::new("quality")
        .unwrap()
        .with_minimum_runs(10)
        .unwrap()
        .with_minimum_run_pass_rate(0.9)
        .unwrap()
        .with_minimum_case_pass_rate(0.8)
        .unwrap()
        .with_assertion_minimum("grounded", 0.9)
        .unwrap()
        .with_assertion_minimum("safe", 0.95)
        .unwrap()
        .with_tag_minimum("smoke", 0.9)
        .unwrap()
        .with_maximum_total_cost_usd(1.0)
        .unwrap();

    let decision = evaluate_gate(&policy, &candidate, None).unwrap();
    assert_eq!(decision.outcome(), GateOutcome::Block);
    assert_eq!(decision.failures().count(), 7);
    assert_eq!(decision.checks()[0].metric, GateMetric::MinimumRuns);
    assert_eq!(decision.checks()[1].metric, GateMetric::MinimumRunPassRate);
    assert_eq!(decision.checks()[2].metric, GateMetric::MinimumCasePassRate);
    assert_eq!(
        decision.checks()[3].metric,
        GateMetric::AssertionPassRate {
            assertion: "grounded".to_owned()
        }
    );
    assert_eq!(decision.checks()[3].observed, serde_json::Value::Null);
}

#[test]
fn comparison_requirements_block_when_comparison_is_missing() {
    let (_, candidate) = clean_reports();
    let policy = GatePolicy::new("comparison-required")
        .unwrap()
        .with_maximum_regressions(0);

    let decision = evaluate_gate(&policy, &candidate, None).unwrap();
    assert!(!decision.allowed());
    assert_eq!(decision.checks().len(), 1);
    assert_eq!(decision.checks()[0].metric, GateMetric::ComparisonAvailable);
}

#[test]
fn cost_growth_and_removed_cases_are_independent_failures() {
    let baseline = report(
        "baseline",
        2,
        1.0,
        0.0,
        vec![case("keep", 1.0, &[]), case("removed", 1.0, &[])],
        &[],
    );
    let candidate = report("candidate", 1, 1.0, 0.01, vec![case("keep", 1.0, &[])], &[]);
    let policy = GatePolicy::new("cost-and-coverage")
        .unwrap()
        .with_maximum_cost_ratio(2.0)
        .unwrap()
        .with_forbid_removed_cases(true);

    let decision = evaluate_gate(&policy, &candidate, Some(&baseline)).unwrap();
    assert_eq!(decision.failures().count(), 2);
    assert_eq!(decision.checks()[0].metric, GateMetric::MaximumCostRatio);
    assert!(decision.checks()[0].observed["ratio"].is_null());
    assert_eq!(decision.checks()[1].metric, GateMetric::NoRemovedCases);
    assert_eq!(
        decision.checks()[1].observed,
        serde_json::json!(["removed"])
    );
}

#[test]
fn baseline_must_describe_the_same_dataset() {
    let (mut baseline, candidate) = clean_reports();
    baseline.dataset_name = "other".to_owned();
    let policy = GatePolicy::new("p").unwrap().with_maximum_regressions(0);
    let error = evaluate_gate(&policy, &candidate, Some(&baseline)).unwrap_err();
    assert!(error.to_string().contains("baseline dataset"), "{error}");
}

#[test]
fn policy_rejects_invalid_numeric_and_empty_dimensions() {
    let error = GatePolicy::new("p")
        .unwrap()
        .with_minimum_runs(0)
        .unwrap_err();
    assert!(error.to_string().contains("greater than zero"), "{error}");
    let error = GatePolicy::new("p")
        .unwrap()
        .with_minimum_run_pass_rate(1.1)
        .unwrap_err();
    assert!(error.to_string().contains("between 0 and 1"), "{error}");
    let error = GatePolicy::new("p")
        .unwrap()
        .with_maximum_cost_ratio(f64::NAN)
        .unwrap_err();
    assert!(error.to_string().contains("finite"), "{error}");
    let error = GatePolicy::new("p")
        .unwrap()
        .with_tag_minimum(" ", 1.0)
        .unwrap_err();
    assert!(error.to_string().contains("must not be empty"), "{error}");
}

#[test]
fn empty_and_misspelled_policies_fail_closed() {
    let (_, candidate) = clean_reports();
    let empty = GatePolicy::new("empty").unwrap();
    let error = evaluate_gate(&empty, &candidate, None).unwrap_err();
    assert!(error.to_string().contains("at least one check"), "{error}");

    let policy = GatePolicy::new("quality")
        .unwrap()
        .with_minimum_run_pass_rate(0.9)
        .unwrap();
    let mut value: serde_json::Value = serde_json::from_str(&policy.to_json().unwrap()).unwrap();
    let minimum = value
        .as_object_mut()
        .unwrap()
        .remove("minimum_run_pass_rate")
        .unwrap();
    value["minimum_run_pas_rate"] = minimum;
    let error = GatePolicy::from_json(&serde_json::to_string(&value).unwrap()).unwrap_err();
    assert!(
        error.to_string().contains("missing field") || error.to_string().contains("unknown field"),
        "{error}"
    );
}

#[test]
fn candidate_summary_must_match_raw_case_run_evidence() {
    let (_, mut candidate) = clean_reports();
    candidate.summary.runs += 1;
    candidate.summary.total_cost_usd = -10.0;
    let policy = GatePolicy::strict("production").unwrap();

    let error = evaluate_gate(&policy, &candidate, None).unwrap_err();
    assert!(
        error.to_string().contains("summary does not match"),
        "{error}"
    );
}

#[test]
fn policy_and_decision_round_trip_stably_with_version_guards() {
    let (baseline, candidate) = clean_reports();
    let policy = GatePolicy::strict("production").unwrap();
    let policy_json = policy.to_json().unwrap();
    let loaded_policy = GatePolicy::from_json(&policy_json).unwrap();
    assert_eq!(loaded_policy, policy);
    assert_eq!(loaded_policy.to_json().unwrap(), policy_json);

    let decision = evaluate_gate(&policy, &candidate, Some(&baseline)).unwrap();
    let decision_json = decision.to_json().unwrap();
    let loaded_decision =
        GateDecision::from_json(&decision_json, &policy, &candidate, Some(&baseline)).unwrap();
    assert_eq!(loaded_decision, decision);
    assert_eq!(loaded_decision.to_json().unwrap(), decision_json);

    let future_policy = r#"{"format_version":99,"future":true}"#;
    assert!(matches!(
        GatePolicy::from_json(future_policy).unwrap_err(),
        EvalError::UnsupportedGateVersion {
            artifact: "policy",
            found: 99,
            supported: GATE_POLICY_FORMAT_VERSION
        }
    ));
    let future_decision = r#"{"format_version":99,"future":true}"#;
    assert!(matches!(
        GateDecision::from_json(future_decision, &policy, &candidate, Some(&baseline)).unwrap_err(),
        EvalError::UnsupportedGateVersion {
            artifact: "decision",
            found: 99,
            supported: GATE_DECISION_FORMAT_VERSION
        }
    ));
}

#[test]
fn decision_loader_rejects_an_outcome_that_disagrees_with_checks() {
    let (_, candidate) = clean_reports();
    let policy = GatePolicy::new("p")
        .unwrap()
        .with_minimum_runs(100)
        .unwrap();
    let decision = evaluate_gate(&policy, &candidate, None).unwrap();
    let mut value: serde_json::Value = serde_json::from_str(&decision.to_json().unwrap()).unwrap();
    value["outcome"] = serde_json::json!("allow");

    let error = GateDecision::from_json(
        &serde_json::to_string(&value).unwrap(),
        &policy,
        &candidate,
        None,
    )
    .unwrap_err();
    assert!(error.to_string().contains("does not match"), "{error}");
}

#[test]
fn decision_loader_rejects_removed_failures_even_if_outcome_is_flipped() {
    let (_, candidate) = clean_reports();
    let policy = GatePolicy::new("p")
        .unwrap()
        .with_minimum_runs(100)
        .unwrap();
    let decision = evaluate_gate(&policy, &candidate, None).unwrap();
    let mut value: serde_json::Value = serde_json::from_str(&decision.to_json().unwrap()).unwrap();
    value["checks"] = serde_json::json!([{
        "metric": {"metric": "minimum_runs"},
        "passed": true,
        "observed": 100,
        "required": {"minimum": 100},
        "detail": "forged"
    }]);
    value["outcome"] = serde_json::json!("allow");

    let error = GateDecision::from_json(
        &serde_json::to_string(&value).unwrap(),
        &policy,
        &candidate,
        None,
    )
    .unwrap_err();
    assert!(error.to_string().contains("recomputed evidence"), "{error}");
}
