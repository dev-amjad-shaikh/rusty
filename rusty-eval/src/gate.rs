//! Deterministic release gates over evaluation evidence.
//!
//! A gate policy combines absolute candidate requirements (sample size, pass
//! rates, assertion/tag slices, and cost) with optional baseline-comparison
//! requirements (regression count, cost growth, and removed cases). Evaluation
//! performs no I/O and consults no clock, so the same reports and policy always
//! produce the same auditable decision.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::compare::{compare, CaseChange, CompareThresholds};
use crate::error::{EvalError, Result};
use crate::experiment::{
    AssertionPassRate, ExperimentReport, LatencyStats, ReportSummary, REPORT_FORMAT_VERSION,
};

/// Policy schema version loaded and written by this build.
pub const GATE_POLICY_FORMAT_VERSION: u64 = 1;
/// Decision schema version loaded and written by this build.
pub const GATE_DECISION_FORMAT_VERSION: u64 = 1;

/// A versioned set of release requirements.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GatePolicy {
    format_version: u64,
    name: String,
    minimum_runs: Option<usize>,
    minimum_run_pass_rate: Option<f64>,
    minimum_case_pass_rate: Option<f64>,
    minimum_assertion_pass_rates: BTreeMap<String, f64>,
    minimum_tag_pass_rates: BTreeMap<String, f64>,
    maximum_total_cost_usd: Option<f64>,
    maximum_cost_ratio: Option<f64>,
    maximum_regressions: Option<usize>,
    forbid_removed_cases: bool,
    comparison_thresholds: CompareThresholds,
}

impl GatePolicy {
    /// Create an empty policy. Add only the checks the release requires.
    pub fn new(name: impl Into<String>) -> Result<Self> {
        let name = name.into();
        require_non_empty("gate policy name", &name)?;
        let policy = Self {
            format_version: GATE_POLICY_FORMAT_VERSION,
            name,
            minimum_runs: None,
            minimum_run_pass_rate: None,
            minimum_case_pass_rate: None,
            minimum_assertion_pass_rates: BTreeMap::new(),
            minimum_tag_pass_rates: BTreeMap::new(),
            maximum_total_cost_usd: None,
            maximum_cost_ratio: None,
            maximum_regressions: None,
            forbid_removed_cases: false,
            comparison_thresholds: CompareThresholds::default(),
        };
        Ok(policy)
    }

    /// A conservative default: at least one run, perfect aggregate pass rates,
    /// no comparison regressions, and no removed cases.
    pub fn strict(name: impl Into<String>) -> Result<Self> {
        Ok(Self::new(name)?
            .with_minimum_runs(1)?
            .with_minimum_run_pass_rate(1.0)?
            .with_minimum_case_pass_rate(1.0)?
            .with_maximum_regressions(0)
            .with_forbid_removed_cases(true))
    }

    /// Stable policy name carried into decisions.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Require at least this many candidate runs.
    pub fn with_minimum_runs(mut self, minimum: usize) -> Result<Self> {
        if minimum == 0 {
            return gate_error("minimum runs must be greater than zero");
        }
        self.minimum_runs = Some(minimum);
        Ok(self)
    }

    /// Require the aggregate run pass rate.
    pub fn with_minimum_run_pass_rate(mut self, minimum: f64) -> Result<Self> {
        validate_rate("minimum run pass rate", minimum)?;
        self.minimum_run_pass_rate = Some(minimum);
        Ok(self)
    }

    /// Require the mean per-case pass rate.
    pub fn with_minimum_case_pass_rate(mut self, minimum: f64) -> Result<Self> {
        validate_rate("minimum case pass rate", minimum)?;
        self.minimum_case_pass_rate = Some(minimum);
        Ok(self)
    }

    /// Require one named assertion to appear and meet a pass-rate floor.
    pub fn with_assertion_minimum(
        mut self,
        assertion: impl Into<String>,
        minimum: f64,
    ) -> Result<Self> {
        let assertion = assertion.into();
        require_non_empty("assertion name", &assertion)?;
        validate_rate("assertion pass-rate minimum", minimum)?;
        self.minimum_assertion_pass_rates.insert(assertion, minimum);
        Ok(self)
    }

    /// Require the mean pass rate of cases carrying `tag`.
    pub fn with_tag_minimum(mut self, tag: impl Into<String>, minimum: f64) -> Result<Self> {
        let tag = tag.into();
        require_non_empty("case tag", &tag)?;
        validate_rate("tag pass-rate minimum", minimum)?;
        self.minimum_tag_pass_rates.insert(tag, minimum);
        Ok(self)
    }

    /// Cap the candidate experiment's total recorded cost.
    pub fn with_maximum_total_cost_usd(mut self, maximum: f64) -> Result<Self> {
        validate_non_negative("maximum total cost", maximum)?;
        self.maximum_total_cost_usd = Some(maximum);
        Ok(self)
    }

    /// Cap `candidate cost / baseline cost`. Requires a comparison report.
    pub fn with_maximum_cost_ratio(mut self, maximum: f64) -> Result<Self> {
        validate_non_negative("maximum cost ratio", maximum)?;
        self.maximum_cost_ratio = Some(maximum);
        Ok(self)
    }

    /// Cap the comparison report's threshold breaches.
    pub fn with_maximum_regressions(mut self, maximum: usize) -> Self {
        self.maximum_regressions = Some(maximum);
        self
    }

    /// Block when a baseline case is absent from the candidate.
    pub fn with_forbid_removed_cases(mut self, forbid: bool) -> Self {
        self.forbid_removed_cases = forbid;
        self
    }

    /// Configure the pass-rate and latency thresholds used when the gate
    /// recomputes a baseline comparison.
    pub fn with_comparison_thresholds(mut self, thresholds: CompareThresholds) -> Result<Self> {
        validate_rate(
            "maximum comparison pass-rate drop",
            thresholds.max_pass_rate_drop,
        )?;
        if !thresholds.max_latency_p95_ratio.is_finite() || thresholds.max_latency_p95_ratio < 0.0 {
            return gate_error(
                "maximum comparison p95 latency ratio must be finite and non-negative",
            );
        }
        self.comparison_thresholds = thresholds;
        Ok(self)
    }

    /// Serialize as a stable, versioned JSON artifact.
    pub fn to_json(&self) -> Result<String> {
        self.validate()?;
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// Parse and validate a policy artifact.
    pub fn from_json(text: &str) -> Result<Self> {
        let header: FormatHeader = serde_json::from_str(text)?;
        if header.format_version != GATE_POLICY_FORMAT_VERSION {
            return Err(EvalError::UnsupportedGateVersion {
                artifact: "policy",
                found: header.format_version,
                supported: GATE_POLICY_FORMAT_VERSION,
            });
        }
        require_policy_fields(text)?;
        let wire: GatePolicyWire = serde_json::from_str(text)?;
        let policy = wire.into_policy();
        policy.validate()?;
        Ok(policy)
    }

    fn needs_comparison(&self) -> bool {
        self.maximum_cost_ratio.is_some()
            || self.maximum_regressions.is_some()
            || self.forbid_removed_cases
    }

    fn validate(&self) -> Result<()> {
        require_non_empty("gate policy name", &self.name)?;
        if self.minimum_runs == Some(0) {
            return gate_error("minimum runs must be greater than zero");
        }
        if let Some(rate) = self.minimum_run_pass_rate {
            validate_rate("minimum run pass rate", rate)?;
        }
        if let Some(rate) = self.minimum_case_pass_rate {
            validate_rate("minimum case pass rate", rate)?;
        }
        for (assertion, rate) in &self.minimum_assertion_pass_rates {
            require_non_empty("assertion name", assertion)?;
            validate_rate("assertion pass-rate minimum", *rate)?;
        }
        for (tag, rate) in &self.minimum_tag_pass_rates {
            require_non_empty("case tag", tag)?;
            validate_rate("tag pass-rate minimum", *rate)?;
        }
        if let Some(cost) = self.maximum_total_cost_usd {
            validate_non_negative("maximum total cost", cost)?;
        }
        if let Some(ratio) = self.maximum_cost_ratio {
            validate_non_negative("maximum cost ratio", ratio)?;
        }
        validate_rate(
            "maximum comparison pass-rate drop",
            self.comparison_thresholds.max_pass_rate_drop,
        )?;
        validate_non_negative(
            "maximum comparison p95 latency ratio",
            self.comparison_thresholds.max_latency_p95_ratio,
        )?;
        if self.minimum_runs.is_none()
            && self.minimum_run_pass_rate.is_none()
            && self.minimum_case_pass_rate.is_none()
            && self.minimum_assertion_pass_rates.is_empty()
            && self.minimum_tag_pass_rates.is_empty()
            && self.maximum_total_cost_usd.is_none()
            && !self.needs_comparison()
        {
            return gate_error("gate policy must configure at least one check");
        }
        Ok(())
    }
}

/// The metric evaluated by one gate check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "metric", rename_all = "snake_case")]
pub enum GateMetric {
    MinimumRuns,
    MinimumRunPassRate,
    MinimumCasePassRate,
    AssertionPassRate { assertion: String },
    TagPassRate { tag: String },
    MaximumTotalCostUsd,
    ComparisonAvailable,
    MaximumCostRatio,
    MaximumRegressions,
    NoRemovedCases,
}

/// One explainable policy check with machine-readable evidence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateCheck {
    pub metric: GateMetric,
    pub passed: bool,
    pub observed: Value,
    pub required: Value,
    pub detail: String,
}

/// Final release disposition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateOutcome {
    Allow,
    Block,
}

/// Versioned, auditable result of evaluating a candidate.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GateDecision {
    format_version: u64,
    policy: String,
    candidate: String,
    baseline: Option<String>,
    outcome: GateOutcome,
    checks: Vec<GateCheck>,
}

impl GateDecision {
    /// `true` only when every configured check passed.
    pub fn allowed(&self) -> bool {
        self.outcome == GateOutcome::Allow && self.checks.iter().all(|check| check.passed)
    }

    /// Final disposition.
    pub fn outcome(&self) -> GateOutcome {
        self.outcome
    }

    /// Policy name used to make this decision.
    pub fn policy(&self) -> &str {
        &self.policy
    }

    /// Candidate experiment identity.
    pub fn candidate(&self) -> &str {
        &self.candidate
    }

    /// Baseline experiment identity, when comparison checks were evaluated.
    pub fn baseline(&self) -> Option<&str> {
        self.baseline.as_deref()
    }

    /// All configured checks in deterministic policy order.
    pub fn checks(&self) -> &[GateCheck] {
        &self.checks
    }

    /// Checks that caused the release to be blocked.
    pub fn failures(&self) -> impl Iterator<Item = &GateCheck> {
        self.checks.iter().filter(|check| !check.passed)
    }

    /// Serialize as stable JSON for CI artifacts and audit logs.
    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// Parse a decision artifact with version validation.
    pub fn from_json(
        text: &str,
        policy: &GatePolicy,
        candidate: &ExperimentReport,
        baseline: Option<&ExperimentReport>,
    ) -> Result<Self> {
        let header: FormatHeader = serde_json::from_str(text)?;
        if header.format_version != GATE_DECISION_FORMAT_VERSION {
            return Err(EvalError::UnsupportedGateVersion {
                artifact: "decision",
                found: header.format_version,
                supported: GATE_DECISION_FORMAT_VERSION,
            });
        }
        let wire: GateDecisionWire = serde_json::from_str(text)?;
        let decision = wire.into_decision();
        decision.validate()?;
        let expected = evaluate_gate(policy, candidate, baseline)?;
        if decision != expected {
            return gate_error("gate decision does not match recomputed evidence");
        }
        Ok(decision)
    }

    fn validate(&self) -> Result<()> {
        require_non_empty("gate decision policy", &self.policy)?;
        require_non_empty("gate decision candidate", &self.candidate)?;
        if let Some(baseline) = &self.baseline {
            require_non_empty("gate decision baseline", baseline)?;
        }
        if self.checks.is_empty() {
            return gate_error("gate decision must contain at least one check");
        }
        let expected = if self.checks.iter().all(|check| check.passed) {
            GateOutcome::Allow
        } else {
            GateOutcome::Block
        };
        if self.outcome != expected {
            return gate_error("gate decision outcome does not match its checks");
        }
        Ok(())
    }
}

/// Evaluate `candidate` under `policy`, optionally recomputing a baseline
/// comparison from raw, validated experiment evidence.
pub fn evaluate_gate(
    policy: &GatePolicy,
    candidate: &ExperimentReport,
    baseline: Option<&ExperimentReport>,
) -> Result<GateDecision> {
    policy.validate()?;
    validate_report(candidate)?;
    if let Some(baseline) = baseline {
        validate_report(baseline)?;
        if baseline.dataset_name != candidate.dataset_name {
            return gate_error(format!(
                "baseline dataset `{}` does not match candidate dataset `{}`",
                baseline.dataset_name, candidate.dataset_name
            ));
        }
        if baseline.max_concurrency != candidate.max_concurrency {
            return gate_error(format!(
                "baseline maximum concurrency {} does not match candidate maximum concurrency {}",
                baseline.max_concurrency, candidate.max_concurrency
            ));
        }
    }
    let comparison =
        baseline.map(|baseline| compare(baseline, candidate, &policy.comparison_thresholds));

    let mut checks = Vec::new();
    if let Some(minimum) = policy.minimum_runs {
        checks.push(check(
            GateMetric::MinimumRuns,
            candidate.summary.runs >= minimum,
            json!(candidate.summary.runs),
            json!({"minimum": minimum}),
            format!(
                "candidate has {} runs; minimum is {minimum}",
                candidate.summary.runs
            ),
        ));
    }
    if let Some(minimum) = policy.minimum_run_pass_rate {
        checks.push(minimum_rate_check(
            GateMetric::MinimumRunPassRate,
            candidate.summary.run_pass_rate,
            minimum,
            "aggregate run pass rate",
        ));
    }
    if let Some(minimum) = policy.minimum_case_pass_rate {
        checks.push(minimum_rate_check(
            GateMetric::MinimumCasePassRate,
            candidate.summary.case_pass_rate,
            minimum,
            "mean case pass rate",
        ));
    }
    for (assertion, minimum) in &policy.minimum_assertion_pass_rates {
        let observed = candidate
            .summary
            .assertions
            .iter()
            .find(|rate| rate.assertion == *assertion)
            .map(|rate| rate.rate);
        checks.push(check(
            GateMetric::AssertionPassRate {
                assertion: assertion.clone(),
            },
            observed.is_some_and(|rate| rate >= *minimum),
            observed.map_or(Value::Null, |rate| json!(rate)),
            json!({"minimum": minimum}),
            match observed {
                Some(rate) => format!("assertion `{assertion}` pass rate is {rate:.6}"),
                None => format!("assertion `{assertion}` is absent from the candidate report"),
            },
        ));
    }
    for (tag, minimum) in &policy.minimum_tag_pass_rates {
        let matching: Vec<_> = candidate
            .cases
            .iter()
            .filter(|case| case.tags.contains(tag))
            .collect();
        let observed = (!matching.is_empty()).then(|| {
            matching.iter().map(|case| case.pass_rate).sum::<f64>() / matching.len() as f64
        });
        checks.push(check(
            GateMetric::TagPassRate { tag: tag.clone() },
            observed.is_some_and(|rate| rate >= *minimum),
            json!({"rate": observed, "cases": matching.len()}),
            json!({"minimum": minimum}),
            match observed {
                Some(rate) => format!(
                    "tag `{tag}` mean pass rate is {rate:.6} across {} cases",
                    matching.len()
                ),
                None => format!("tag `{tag}` has no candidate cases"),
            },
        ));
    }
    if let Some(maximum) = policy.maximum_total_cost_usd {
        let observed = candidate.summary.total_cost_usd;
        checks.push(check(
            GateMetric::MaximumTotalCostUsd,
            observed <= maximum,
            json!(observed),
            json!({"maximum": maximum}),
            format!("candidate total cost is ${observed:.6}; maximum is ${maximum:.6}"),
        ));
    }

    if policy.needs_comparison() && comparison.is_none() {
        checks.push(check(
            GateMetric::ComparisonAvailable,
            false,
            Value::Null,
            json!({"required": true}),
            "policy requires a baseline comparison".to_owned(),
        ));
    }
    if let Some(comparison) = comparison.as_ref() {
        if let Some(maximum) = policy.maximum_cost_ratio {
            let baseline = comparison.baseline_cost_usd;
            let candidate_cost = comparison.candidate_cost_usd;
            let ratio = (baseline > 0.0).then(|| candidate_cost / baseline);
            let passed = ratio.map_or(candidate_cost == 0.0, |ratio| ratio <= maximum);
            checks.push(check(
                GateMetric::MaximumCostRatio,
                passed,
                json!({"baseline": baseline, "candidate": candidate_cost, "ratio": ratio}),
                json!({"maximum": maximum}),
                match ratio {
                    Some(ratio) => format!("candidate/baseline cost ratio is {ratio:.6}"),
                    None => {
                        format!("baseline cost is zero and candidate cost is ${candidate_cost:.6}")
                    }
                },
            ));
        }
        if let Some(maximum) = policy.maximum_regressions {
            let observed = comparison.regressions.len();
            checks.push(check(
                GateMetric::MaximumRegressions,
                observed <= maximum,
                json!(observed),
                json!({"maximum": maximum}),
                format!("comparison contains {observed} regressions; maximum is {maximum}"),
            ));
        }
        if policy.forbid_removed_cases {
            let removed: Vec<_> = comparison
                .case_deltas
                .iter()
                .filter(|case| case.change == CaseChange::Removed)
                .map(|case| case.case_id.clone())
                .collect();
            checks.push(check(
                GateMetric::NoRemovedCases,
                removed.is_empty(),
                json!(removed),
                json!([]),
                if removed.is_empty() {
                    "candidate retains every baseline case".to_owned()
                } else {
                    format!("candidate removed {} baseline cases", removed.len())
                },
            ));
        }
    }

    let outcome = if checks.iter().all(|check| check.passed) {
        GateOutcome::Allow
    } else {
        GateOutcome::Block
    };
    Ok(GateDecision {
        format_version: GATE_DECISION_FORMAT_VERSION,
        policy: policy.name.clone(),
        candidate: candidate.name.clone(),
        baseline: comparison.map(|comparison| comparison.baseline.clone()),
        outcome,
        checks,
    })
}

fn minimum_rate_check(metric: GateMetric, observed: f64, minimum: f64, label: &str) -> GateCheck {
    check(
        metric,
        observed >= minimum,
        json!(observed),
        json!({"minimum": minimum}),
        format!("{label} is {observed:.6}; minimum is {minimum:.6}"),
    )
}

fn check(
    metric: GateMetric,
    passed: bool,
    observed: Value,
    required: Value,
    detail: String,
) -> GateCheck {
    GateCheck {
        metric,
        passed,
        observed,
        required,
        detail,
    }
}

#[derive(Deserialize)]
struct FormatHeader {
    format_version: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GatePolicyWire {
    format_version: u64,
    name: String,
    minimum_runs: Option<usize>,
    minimum_run_pass_rate: Option<f64>,
    minimum_case_pass_rate: Option<f64>,
    minimum_assertion_pass_rates: BTreeMap<String, f64>,
    minimum_tag_pass_rates: BTreeMap<String, f64>,
    maximum_total_cost_usd: Option<f64>,
    maximum_cost_ratio: Option<f64>,
    maximum_regressions: Option<usize>,
    forbid_removed_cases: bool,
    comparison_thresholds: CompareThresholds,
}

impl GatePolicyWire {
    fn into_policy(self) -> GatePolicy {
        GatePolicy {
            format_version: self.format_version,
            name: self.name,
            minimum_runs: self.minimum_runs,
            minimum_run_pass_rate: self.minimum_run_pass_rate,
            minimum_case_pass_rate: self.minimum_case_pass_rate,
            minimum_assertion_pass_rates: self.minimum_assertion_pass_rates,
            minimum_tag_pass_rates: self.minimum_tag_pass_rates,
            maximum_total_cost_usd: self.maximum_total_cost_usd,
            maximum_cost_ratio: self.maximum_cost_ratio,
            maximum_regressions: self.maximum_regressions,
            forbid_removed_cases: self.forbid_removed_cases,
            comparison_thresholds: self.comparison_thresholds,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GateDecisionWire {
    format_version: u64,
    policy: String,
    candidate: String,
    baseline: Option<String>,
    outcome: GateOutcome,
    checks: Vec<GateCheck>,
}

fn require_policy_fields(text: &str) -> Result<()> {
    const REQUIRED: &[&str] = &[
        "format_version",
        "name",
        "minimum_runs",
        "minimum_run_pass_rate",
        "minimum_case_pass_rate",
        "minimum_assertion_pass_rates",
        "minimum_tag_pass_rates",
        "maximum_total_cost_usd",
        "maximum_cost_ratio",
        "maximum_regressions",
        "forbid_removed_cases",
        "comparison_thresholds",
    ];
    let value: Value = serde_json::from_str(text)?;
    let object = value
        .as_object()
        .ok_or_else(|| EvalError::Gate("gate policy must be a JSON object".to_owned()))?;
    for field in REQUIRED {
        if !object.contains_key(*field) {
            return gate_error(format!("gate policy is missing field `{field}`"));
        }
    }
    Ok(())
}

pub(crate) fn validate_report(report: &ExperimentReport) -> Result<()> {
    if report.format_version != REPORT_FORMAT_VERSION {
        return Err(EvalError::UnsupportedVersion {
            found: report.format_version,
            supported: REPORT_FORMAT_VERSION,
        });
    }
    require_non_empty("experiment name", &report.name)?;
    require_non_empty("dataset name", &report.dataset_name)?;
    require_non_empty("dataset version", &report.dataset_version)?;
    if report.runs_per_case == 0 {
        return gate_error(format!(
            "experiment `{}` has zero runs per case",
            report.name
        ));
    }
    if report.max_concurrency == 0 {
        return gate_error(format!(
            "experiment `{}` has zero maximum concurrency",
            report.name
        ));
    }

    let mut case_ids = BTreeSet::new();
    let mut runs = 0_usize;
    let mut runs_passed = 0_usize;
    let mut latencies = Vec::new();
    let mut total_cost_usd = 0.0_f64;
    let mut total_tokens = 0_u64;
    let mut assertion_totals: BTreeMap<String, (usize, usize)> = BTreeMap::new();

    for case in &report.cases {
        require_non_empty("case id", &case.case_id)?;
        if !case_ids.insert(case.case_id.as_str()) {
            return gate_error(format!(
                "experiment `{}` has duplicate case `{}`",
                report.name, case.case_id
            ));
        }
        if case.runs.len() != report.runs_per_case {
            return gate_error(format!(
                "case `{}` contains {} runs; expected {}",
                case.case_id,
                case.runs.len(),
                report.runs_per_case
            ));
        }
        let repetitions: BTreeSet<_> = case.runs.iter().map(|run| run.repetition).collect();
        if repetitions.len() != case.runs.len()
            || !(0..report.runs_per_case).all(|repetition| repetitions.contains(&repetition))
        {
            return gate_error(format!(
                "case `{}` has invalid repetition indices",
                case.case_id
            ));
        }

        let mut case_passed = 0_usize;
        for run in &case.runs {
            if !run.cost_usd.is_finite() || run.cost_usd < 0.0 {
                return gate_error(format!(
                    "case `{}` repetition {} has invalid cost",
                    case.case_id, run.repetition
                ));
            }
            if let Some(judge) = &run.judge {
                validate_rate("judge score", judge.score)?;
                require_non_empty("judge rationale", &judge.rationale)?;
            }
            let expected_passed = run.status.is_done()
                && run.assertions.iter().all(|assertion| assertion.passed)
                && run.judge.as_ref().is_none_or(|judge| judge.passed);
            if run.passed != expected_passed {
                return gate_error(format!(
                    "case `{}` repetition {} pass verdict contradicts its evidence",
                    case.case_id, run.repetition
                ));
            }
            for assertion in &run.assertions {
                require_non_empty("assertion name", &assertion.assertion)?;
                let entry = assertion_totals
                    .entry(assertion.assertion.clone())
                    .or_default();
                entry.1 += 1;
                entry.0 += usize::from(assertion.passed);
            }
            runs += 1;
            runs_passed += usize::from(run.passed);
            case_passed += usize::from(run.passed);
            latencies.push(run.latency_ms);
            total_cost_usd += run.cost_usd;
            total_tokens = total_tokens.checked_add(run.total_tokens).ok_or_else(|| {
                EvalError::Gate(format!(
                    "experiment `{}` token total overflows u64",
                    report.name
                ))
            })?;
        }
        let expected_rate = case_passed as f64 / case.runs.len() as f64;
        if !same_float(case.pass_rate, expected_rate) {
            return gate_error(format!(
                "case `{}` pass rate does not match its runs",
                case.case_id
            ));
        }
    }

    latencies.sort_unstable();
    let expected_assertions: Vec<_> = assertion_totals
        .into_iter()
        .map(|(assertion, (passed, total))| AssertionPassRate {
            assertion,
            passed,
            total,
            rate: passed as f64 / total as f64,
        })
        .collect();
    let latency_sum = latencies.iter().try_fold(0_u64, |sum, latency| {
        sum.checked_add(*latency).ok_or_else(|| {
            EvalError::Gate(format!(
                "experiment `{}` latency total overflows u64",
                report.name
            ))
        })
    })?;
    let expected = ReportSummary {
        cases: report.cases.len(),
        runs,
        runs_passed,
        run_pass_rate: if runs == 0 {
            0.0
        } else {
            runs_passed as f64 / runs as f64
        },
        case_pass_rate: if report.cases.is_empty() {
            0.0
        } else {
            report.cases.iter().map(|case| case.pass_rate).sum::<f64>() / report.cases.len() as f64
        },
        assertions: expected_assertions,
        latency_ms: LatencyStats {
            min: latencies.first().copied().unwrap_or(0),
            p50: gate_percentile(&latencies, 50),
            p95: gate_percentile(&latencies, 95),
            max: latencies.last().copied().unwrap_or(0),
            mean: if latencies.is_empty() {
                0.0
            } else {
                latency_sum as f64 / latencies.len() as f64
            },
        },
        total_cost_usd,
        total_tokens,
    };
    if !summary_matches(&report.summary, &expected) {
        return gate_error(format!(
            "experiment `{}` summary does not match its case-run evidence",
            report.name
        ));
    }
    Ok(())
}

fn summary_matches(actual: &ReportSummary, expected: &ReportSummary) -> bool {
    actual.cases == expected.cases
        && actual.runs == expected.runs
        && actual.runs_passed == expected.runs_passed
        && same_float(actual.run_pass_rate, expected.run_pass_rate)
        && same_float(actual.case_pass_rate, expected.case_pass_rate)
        && actual.assertions.len() == expected.assertions.len()
        && actual
            .assertions
            .iter()
            .zip(&expected.assertions)
            .all(|(actual, expected)| {
                actual.assertion == expected.assertion
                    && actual.passed == expected.passed
                    && actual.total == expected.total
                    && same_float(actual.rate, expected.rate)
            })
        && actual.latency_ms.min == expected.latency_ms.min
        && actual.latency_ms.p50 == expected.latency_ms.p50
        && actual.latency_ms.p95 == expected.latency_ms.p95
        && actual.latency_ms.max == expected.latency_ms.max
        && same_float(actual.latency_ms.mean, expected.latency_ms.mean)
        && same_float(actual.total_cost_usd, expected.total_cost_usd)
        && actual.total_tokens == expected.total_tokens
}

fn same_float(actual: f64, expected: f64) -> bool {
    actual.is_finite() && expected.is_finite() && (actual - expected).abs() <= 1e-12
}

fn gate_percentile(sorted: &[u64], percentile: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (percentile * sorted.len()).div_ceil(100);
    sorted[rank.clamp(1, sorted.len()) - 1]
}

impl GateDecisionWire {
    fn into_decision(self) -> GateDecision {
        GateDecision {
            format_version: self.format_version,
            policy: self.policy,
            candidate: self.candidate,
            baseline: self.baseline,
            outcome: self.outcome,
            checks: self.checks,
        }
    }
}

fn validate_rate(label: &str, value: f64) -> Result<()> {
    if value.is_finite() && (0.0..=1.0).contains(&value) {
        Ok(())
    } else {
        gate_error(format!("{label} must be finite and between 0 and 1"))
    }
}

fn validate_non_negative(label: &str, value: f64) -> Result<()> {
    if value.is_finite() && value >= 0.0 {
        Ok(())
    } else {
        gate_error(format!("{label} must be finite and non-negative"))
    }
}

fn require_non_empty(label: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        gate_error(format!("{label} must not be empty"))
    } else {
        Ok(())
    }
}

fn gate_error<T>(message: impl Into<String>) -> Result<T> {
    Err(EvalError::Gate(message.into()))
}
