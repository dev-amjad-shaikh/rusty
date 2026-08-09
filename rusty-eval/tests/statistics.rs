use rusty_eval::{
    detect_pass_rate_regression, AssertionPassRate, CaseReport, CaseRunReport, ExperimentReport,
    LatencyStats, ReportSummary, RunStatus, StatisticalDecision, StatisticalRegressionConfig,
    StatisticalRegressionReport, REPORT_FORMAT_VERSION, STATISTICAL_REGRESSION_FORMAT_VERSION,
};

fn report(name: &str, dataset: &str, version: &str, cases: &[(&str, &[bool])]) -> ExperimentReport {
    let runs_per_case = cases.first().map(|(_, runs)| runs.len()).unwrap_or(1);
    let mut case_reports = Vec::new();
    let mut total_runs = 0;
    let mut runs_passed = 0;

    for (case_id, outcomes) in cases {
        assert_eq!(outcomes.len(), runs_per_case);
        total_runs += outcomes.len();
        runs_passed += outcomes.iter().filter(|passed| **passed).count();
        case_reports.push(CaseReport {
            case_id: (*case_id).to_owned(),
            tags: Vec::new(),
            pass_rate: outcomes.iter().filter(|passed| **passed).count() as f64
                / outcomes.len() as f64,
            runs: outcomes
                .iter()
                .enumerate()
                .map(|(repetition, passed)| CaseRunReport {
                    repetition,
                    status: if *passed {
                        RunStatus::Done
                    } else {
                        RunStatus::Interrupted
                    },
                    passed: *passed,
                    assertions: Vec::new(),
                    judge: None,
                    tool_calls: 0,
                    latency_ms: 0,
                    cost_usd: 0.0,
                    total_tokens: 0,
                })
                .collect(),
        });
    }

    let run_pass_rate = if total_runs == 0 {
        0.0
    } else {
        runs_passed as f64 / total_runs as f64
    };
    ExperimentReport {
        format_version: REPORT_FORMAT_VERSION,
        name: name.to_owned(),
        dataset_name: dataset.to_owned(),
        dataset_version: version.to_owned(),
        runs_per_case,
        cases: case_reports,
        summary: ReportSummary {
            cases: cases.len(),
            runs: total_runs,
            runs_passed,
            run_pass_rate,
            case_pass_rate: run_pass_rate,
            assertions: Vec::<AssertionPassRate>::new(),
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
    }
}

#[test]
fn clear_paired_degradation_is_a_regression() {
    let baseline_outcomes = vec![true; 40];
    let mut candidate_outcomes = vec![false; 20];
    candidate_outcomes.extend(vec![true; 20]);
    let baseline = report(
        "baseline",
        "support",
        "v1",
        &[("answer", &baseline_outcomes)],
    );
    let candidate = report(
        "candidate",
        "support",
        "v1",
        &[("answer", &candidate_outcomes)],
    );

    let result = detect_pass_rate_regression(
        &baseline,
        &candidate,
        &StatisticalRegressionConfig::default(),
    )
    .unwrap();

    assert_eq!(result.format_version, STATISTICAL_REGRESSION_FORMAT_VERSION);
    assert_eq!(result.pairs, 40);
    assert_eq!(result.both_passed, 20);
    assert_eq!(result.regressions, 20);
    assert_eq!(result.improvements, 0);
    assert_eq!(result.pass_rate_drop, 0.5);
    assert!(result.p_value.unwrap() < 0.000_001);
    assert!(result.effect_threshold_met);
    assert!(result.significance_threshold_met);
    assert_eq!(result.decision, StatisticalDecision::Regression);
}

#[test]
fn balanced_discordance_is_sampling_noise_not_regression() {
    let baseline_outcomes: Vec<bool> = (0..40).map(|index| index % 2 == 0).collect();
    let candidate_outcomes: Vec<bool> = baseline_outcomes.iter().map(|passed| !passed).collect();
    let baseline = report(
        "baseline",
        "support",
        "v1",
        &[("answer", &baseline_outcomes)],
    );
    let candidate = report(
        "candidate",
        "support",
        "v1",
        &[("answer", &candidate_outcomes)],
    );

    let result = detect_pass_rate_regression(
        &baseline,
        &candidate,
        &StatisticalRegressionConfig::default(),
    )
    .unwrap();

    assert_eq!(result.regressions, 20);
    assert_eq!(result.improvements, 20);
    assert_eq!(result.pass_rate_drop, 0.0);
    assert!(result.p_value.unwrap() > 0.5);
    assert_eq!(result.decision, StatisticalDecision::NoRegression);
}

#[test]
fn significant_improvement_is_never_labeled_a_regression() {
    let mut baseline_outcomes = vec![false; 30];
    baseline_outcomes.extend(vec![true; 10]);
    let candidate_outcomes = vec![true; 40];
    let baseline = report(
        "baseline",
        "support",
        "v1",
        &[("answer", &baseline_outcomes)],
    );
    let candidate = report(
        "candidate",
        "support",
        "v1",
        &[("answer", &candidate_outcomes)],
    );

    let result = detect_pass_rate_regression(
        &baseline,
        &candidate,
        &StatisticalRegressionConfig::default(),
    )
    .unwrap();

    assert_eq!(result.regressions, 0);
    assert_eq!(result.improvements, 30);
    assert!(result.pass_rate_drop < 0.0);
    assert_eq!(result.p_value, Some(1.0));
    assert_eq!(result.decision, StatisticalDecision::NoRegression);
}

#[test]
fn underpowered_comparison_is_explicitly_insufficient() {
    let baseline_outcomes = vec![true; 10];
    let candidate_outcomes = vec![false; 10];
    let baseline = report(
        "baseline",
        "support",
        "v1",
        &[("answer", &baseline_outcomes)],
    );
    let candidate = report(
        "candidate",
        "support",
        "v1",
        &[("answer", &candidate_outcomes)],
    );

    let result = detect_pass_rate_regression(
        &baseline,
        &candidate,
        &StatisticalRegressionConfig::default(),
    )
    .unwrap();

    assert_eq!(result.p_value, None);
    assert_eq!(result.decision, StatisticalDecision::InsufficientEvidence);
}

#[test]
fn practical_effect_threshold_is_inclusive_and_independent() {
    let mut baseline_outcomes = vec![true; 6];
    baseline_outcomes.extend(vec![false; 94]);
    let mut candidate_outcomes = vec![true];
    candidate_outcomes.extend(vec![false; 99]);
    let baseline = report(
        "baseline",
        "support",
        "v1",
        &[("answer", &baseline_outcomes)],
    );
    let candidate = report(
        "candidate",
        "support",
        "v1",
        &[("answer", &candidate_outcomes)],
    );
    let exact_threshold = StatisticalRegressionConfig::default()
        .with_minimum_pairs(1)
        .unwrap();

    let result = detect_pass_rate_regression(&baseline, &candidate, &exact_threshold).unwrap();
    assert!((result.pass_rate_drop - 0.05).abs() < 1e-12);
    assert!((result.p_value.unwrap() - 0.03125).abs() < 1e-12);
    assert_eq!(result.decision, StatisticalDecision::Regression);

    let stricter_effect = exact_threshold.with_minimum_pass_rate_drop(0.051).unwrap();
    let result = detect_pass_rate_regression(&baseline, &candidate, &stricter_effect).unwrap();
    assert!(result.significance_threshold_met);
    assert!(!result.effect_threshold_met);
    assert_eq!(result.decision, StatisticalDecision::NoRegression);
}

#[test]
fn pairing_rejects_mismatched_and_malformed_reports() {
    let outcomes = vec![true; 30];
    let baseline = report("baseline", "support", "v1", &[("answer", &outcomes)]);

    let other_dataset = report("candidate", "other", "v1", &[("answer", &outcomes)]);
    let error = detect_pass_rate_regression(
        &baseline,
        &other_dataset,
        &StatisticalRegressionConfig::default(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("same dataset version"));

    let mut missing_pair = report("candidate", "support", "v1", &[("other", &outcomes)]);
    let error = detect_pass_rate_regression(
        &baseline,
        &missing_pair,
        &StatisticalRegressionConfig::default(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("missing paired run"));

    missing_pair.cases[0].case_id = "answer".to_owned();
    missing_pair.cases[0].runs[1].repetition = 0;
    let error = detect_pass_rate_regression(
        &baseline,
        &missing_pair,
        &StatisticalRegressionConfig::default(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("invalid repetition indices"));

    let mut future = baseline.clone();
    future.format_version += 1;
    let error =
        detect_pass_rate_regression(&future, &baseline, &StatisticalRegressionConfig::default())
            .unwrap_err();
    assert!(error.to_string().contains("format version"));

    let mut contradictory = baseline.clone();
    contradictory.cases[0].runs[0].passed = false;
    contradictory.cases[0].pass_rate = 29.0 / 30.0;
    contradictory.summary.runs_passed = 29;
    contradictory.summary.run_pass_rate = 29.0 / 30.0;
    contradictory.summary.case_pass_rate = 29.0 / 30.0;
    let error = detect_pass_rate_regression(
        &contradictory,
        &baseline,
        &StatisticalRegressionConfig::default(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("contradicts its evidence"));
}

#[test]
fn subnormal_significance_threshold_uses_the_exact_log_tail() {
    let baseline_outcomes = vec![true; 1_100];
    let candidate_outcomes = vec![false; 1_100];
    let baseline = report(
        "baseline",
        "support",
        "v1",
        &[("answer", &baseline_outcomes)],
    );
    let candidate = report(
        "candidate",
        "support",
        "v1",
        &[("answer", &candidate_outcomes)],
    );
    let config = StatisticalRegressionConfig::default()
        .with_significance_level(1e-320)
        .unwrap();

    let result = detect_pass_rate_regression(&baseline, &candidate, &config).unwrap();

    assert_eq!(result.p_value, Some(f64::from_bits(1)));
    assert!(result.significance_threshold_met);
    assert_eq!(result.decision, StatisticalDecision::Regression);
}

#[test]
fn significance_threshold_is_inclusive_at_exact_equality() {
    let mut baseline_outcomes = vec![true; 10];
    baseline_outcomes.extend(vec![false; 2]);
    let mut candidate_outcomes = vec![false; 10];
    candidate_outcomes.extend(vec![true; 2]);
    let baseline = report(
        "baseline",
        "support",
        "v1",
        &[("answer", &baseline_outcomes)],
    );
    let candidate = report(
        "candidate",
        "support",
        "v1",
        &[("answer", &candidate_outcomes)],
    );
    let exact_p_value = 0.019_287_109_375;
    let config = StatisticalRegressionConfig::default()
        .with_minimum_pairs(1)
        .unwrap()
        .with_significance_level(exact_p_value)
        .unwrap();

    let result = detect_pass_rate_regression(&baseline, &candidate, &config).unwrap();

    assert!((result.p_value.unwrap() - exact_p_value).abs() < 1e-14);
    assert!(result.significance_threshold_met);
    assert_eq!(result.decision, StatisticalDecision::Regression);
}

#[test]
fn configuration_rejects_values_that_make_evidence_ambiguous() {
    assert!(StatisticalRegressionConfig::new()
        .with_significance_level(0.0)
        .is_err());
    assert!(StatisticalRegressionConfig::new()
        .with_significance_level(1.0)
        .is_err());
    assert!(StatisticalRegressionConfig::new()
        .with_significance_level(f64::NAN)
        .is_err());
    assert!(StatisticalRegressionConfig::new()
        .with_minimum_pass_rate_drop(1.1)
        .is_err());
    assert!(StatisticalRegressionConfig::new()
        .with_minimum_pairs(0)
        .is_err());
}

#[test]
fn statistical_evidence_survives_json_round_trip() {
    let baseline_outcomes = vec![true; 30];
    let candidate_outcomes = vec![false; 30];
    let baseline = report(
        "baseline",
        "support",
        "v1",
        &[("answer", &baseline_outcomes)],
    );
    let candidate = report(
        "candidate",
        "support",
        "v1",
        &[("answer", &candidate_outcomes)],
    );
    let result = detect_pass_rate_regression(
        &baseline,
        &candidate,
        &StatisticalRegressionConfig::default(),
    )
    .unwrap();

    let json = result.to_json().unwrap();
    let loaded = StatisticalRegressionReport::from_json(&json).unwrap();
    assert_eq!(loaded.format_version, result.format_version);
    assert_eq!(loaded.baseline, result.baseline);
    assert_eq!(loaded.candidate, result.candidate);
    assert_eq!(loaded.config, result.config);
    assert_eq!(loaded.pairs, result.pairs);
    assert_eq!(loaded.regressions, result.regressions);
    assert_eq!(loaded.improvements, result.improvements);
    assert_eq!(loaded.decision, result.decision);
    assert!((loaded.pass_rate_drop - result.pass_rate_drop).abs() < 1e-15);
    assert!((loaded.p_value.unwrap() - result.p_value.unwrap()).abs() < 1e-20);

    let mut future: serde_json::Value = serde_json::from_str(&json).unwrap();
    future["format_version"] = serde_json::json!(2);
    let error = StatisticalRegressionReport::from_json(&future.to_string()).unwrap_err();
    assert!(error
        .to_string()
        .contains("unsupported statistical regression format version"));

    let mut forged: serde_json::Value = serde_json::from_str(&json).unwrap();
    forged["decision"] = serde_json::json!("no_regression");
    let error = StatisticalRegressionReport::from_json(&forged.to_string()).unwrap_err();
    assert!(error.to_string().contains("does not match its evidence"));
}
