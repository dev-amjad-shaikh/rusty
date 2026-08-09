//! Statistical regression detection for matched experiment reports.
//!
//! [`detect_pass_rate_regression`] pairs baseline and candidate outcomes by
//! `(case_id, repetition)` and applies a one-sided exact McNemar test to the
//! discordant pairs. This answers a narrower question than a raw threshold:
//! are there significantly more baseline-pass/candidate-fail outcomes than
//! movements in the other direction?
//!
//! The implementation is deterministic and dependency-free. It requires
//! exact pair coverage rather than silently discarding missing runs, combines
//! a statistical significance threshold with a minimum practical effect, and
//! reports insufficient sample size separately from a clean result.

use std::collections::{BTreeMap, BTreeSet};
use std::f64::consts::LN_2;

use serde::{Deserialize, Serialize};

use crate::error::{EvalError, Result};
use crate::experiment::ExperimentReport;

/// Wire version for [`StatisticalRegressionReport`].
pub const STATISTICAL_REGRESSION_FORMAT_VERSION: u64 = 1;

/// Policy for a paired pass-rate regression test.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatisticalRegressionConfig {
    /// One-sided significance level. Must be finite and strictly between 0 and 1.
    pub significance_level: f64,
    /// Smallest baseline-minus-candidate pass-rate drop worth flagging.
    pub minimum_pass_rate_drop: f64,
    /// Minimum number of matched case runs required to make a decision.
    pub minimum_pairs: usize,
}

impl StatisticalRegressionConfig {
    /// Conservative defaults: alpha 0.05, five-point drop, 30 matched runs.
    pub fn new() -> Self {
        Self {
            significance_level: 0.05,
            minimum_pass_rate_drop: 0.05,
            minimum_pairs: 30,
        }
    }

    /// Set the one-sided significance level.
    pub fn with_significance_level(mut self, significance_level: f64) -> Result<Self> {
        validate_significance_level(significance_level)?;
        self.significance_level = significance_level;
        Ok(self)
    }

    /// Set the minimum practical pass-rate drop in `0.0..=1.0`.
    pub fn with_minimum_pass_rate_drop(mut self, minimum: f64) -> Result<Self> {
        validate_rate("minimum pass-rate drop", minimum)?;
        self.minimum_pass_rate_drop = minimum;
        Ok(self)
    }

    /// Set the minimum matched sample size.
    pub fn with_minimum_pairs(mut self, minimum_pairs: usize) -> Result<Self> {
        if minimum_pairs == 0 {
            return statistics_error("minimum pairs must be greater than zero");
        }
        self.minimum_pairs = minimum_pairs;
        Ok(self)
    }

    fn validate(&self) -> Result<()> {
        validate_significance_level(self.significance_level)?;
        validate_rate("minimum pass-rate drop", self.minimum_pass_rate_drop)?;
        if self.minimum_pairs == 0 {
            return statistics_error("minimum pairs must be greater than zero");
        }
        Ok(())
    }
}

impl Default for StatisticalRegressionConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// The release interpretation of a statistical comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatisticalDecision {
    /// Fewer matched runs than the configured minimum.
    InsufficientEvidence,
    /// The practical-effect and significance thresholds were not both met.
    NoRegression,
    /// A practically meaningful, statistically significant degradation.
    Regression,
}

/// Auditable evidence from one paired pass-rate comparison.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatisticalRegressionReport {
    /// Report schema version.
    pub format_version: u64,
    /// Baseline experiment identity.
    pub baseline: String,
    /// Candidate experiment identity.
    pub candidate: String,
    /// Shared dataset name.
    pub dataset_name: String,
    /// Shared dataset version.
    pub dataset_version: String,
    /// Policy applied.
    pub config: StatisticalRegressionConfig,
    /// Number of exactly matched `(case_id, repetition)` outcomes.
    pub pairs: usize,
    /// Pairs where both experiments passed.
    pub both_passed: usize,
    /// Pairs where both experiments failed.
    pub both_failed: usize,
    /// Baseline passed and candidate failed.
    pub regressions: usize,
    /// Baseline failed and candidate passed.
    pub improvements: usize,
    /// Baseline passes divided by pairs.
    pub baseline_pass_rate: f64,
    /// Candidate passes divided by pairs.
    pub candidate_pass_rate: f64,
    /// `baseline_pass_rate - candidate_pass_rate`.
    pub pass_rate_drop: f64,
    /// One-sided exact p-value, absent when the sample-size policy is unmet.
    pub p_value: Option<f64>,
    /// Whether the configured practical-effect threshold was met.
    pub effect_threshold_met: bool,
    /// Whether the configured significance threshold was met.
    pub significance_threshold_met: bool,
    /// Final interpretation.
    pub decision: StatisticalDecision,
}

impl StatisticalRegressionReport {
    /// Serialize validated statistical evidence as pretty JSON.
    pub fn to_json(&self) -> Result<String> {
        self.validate()?;
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// Parse statistical evidence, refusing unsupported versions or forged
    /// derived values.
    pub fn from_json(text: &str) -> Result<Self> {
        let header: FormatHeader = serde_json::from_str(text)?;
        if header.format_version != STATISTICAL_REGRESSION_FORMAT_VERSION {
            return statistics_error(format!(
                "unsupported statistical regression format version: found {}, this build supports {}",
                header.format_version, STATISTICAL_REGRESSION_FORMAT_VERSION
            ));
        }
        let report: Self = serde_json::from_str(text)?;
        report.validate()?;
        Ok(report)
    }

    fn validate(&self) -> Result<()> {
        if self.format_version != STATISTICAL_REGRESSION_FORMAT_VERSION {
            return statistics_error(format!(
                "unsupported statistical regression format version: found {}, this build supports {}",
                self.format_version, STATISTICAL_REGRESSION_FORMAT_VERSION
            ));
        }
        self.config.validate()?;
        require_non_empty("baseline experiment", &self.baseline)?;
        require_non_empty("candidate experiment", &self.candidate)?;
        require_non_empty("dataset name", &self.dataset_name)?;
        require_non_empty("dataset version", &self.dataset_version)?;

        let counted_pairs = self
            .both_passed
            .checked_add(self.both_failed)
            .and_then(|count| count.checked_add(self.regressions))
            .and_then(|count| count.checked_add(self.improvements))
            .ok_or_else(|| EvalError::Statistics("pair counts overflow usize".to_owned()))?;
        if self.pairs != counted_pairs {
            return statistics_error("pair count does not match the outcome counts");
        }

        let expected_baseline_rate = rate(self.both_passed + self.regressions, self.pairs);
        let expected_candidate_rate = rate(self.both_passed + self.improvements, self.pairs);
        let expected_drop = paired_pass_rate_drop(self.regressions, self.improvements, self.pairs);
        if !same_statistic(self.baseline_pass_rate, expected_baseline_rate)
            || !same_statistic(self.candidate_pass_rate, expected_candidate_rate)
            || !same_statistic(self.pass_rate_drop, expected_drop)
        {
            return statistics_error("reported pass rates do not match the paired outcomes");
        }

        let expected_effect = effect_threshold_met(
            self.regressions,
            self.improvements,
            self.pairs,
            self.config.minimum_pass_rate_drop,
        );
        let expected_log_p = (self.pairs >= self.config.minimum_pairs).then(|| {
            exact_binomial_upper_log_tail(self.regressions + self.improvements, self.regressions)
        });
        let expected_p = expected_log_p.map(display_p_value);
        if !same_optional_statistic(self.p_value, expected_p) {
            return statistics_error("reported p-value does not match the paired outcomes");
        }
        let expected_significance = expected_log_p
            .map(|value| log_probability_at_or_below(value, self.config.significance_level.ln()))
            .unwrap_or(false);
        let expected_decision = statistical_decision(
            self.pairs,
            &self.config,
            expected_effect,
            expected_significance,
        );
        if self.effect_threshold_met != expected_effect
            || self.significance_threshold_met != expected_significance
            || self.decision != expected_decision
        {
            return statistics_error("statistical decision does not match its evidence");
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct FormatHeader {
    format_version: u64,
}

/// Detect a pass-rate regression between two exactly matched experiments.
///
/// Reports must describe the same dataset version and contain identical run
/// keys. Pairing by repetition is meaningful when baseline and candidate were
/// executed under the same case/repetition design; callers should preserve
/// deterministic seeds or other experimental controls where applicable.
pub fn detect_pass_rate_regression(
    baseline: &ExperimentReport,
    candidate: &ExperimentReport,
    config: &StatisticalRegressionConfig,
) -> Result<StatisticalRegressionReport> {
    config.validate()?;
    validate_input_report("baseline", baseline)?;
    validate_input_report("candidate", candidate)?;

    if baseline.dataset_name != candidate.dataset_name
        || baseline.dataset_version != candidate.dataset_version
    {
        return statistics_error(format!(
            "reports must use the same dataset version; baseline is {}@{}, candidate is {}@{}",
            baseline.dataset_name,
            baseline.dataset_version,
            candidate.dataset_name,
            candidate.dataset_version
        ));
    }
    if baseline.runs_per_case != candidate.runs_per_case {
        return statistics_error(format!(
            "reports must use the same runs_per_case; baseline is {}, candidate is {}",
            baseline.runs_per_case, candidate.runs_per_case
        ));
    }

    let baseline_runs = run_outcomes("baseline", baseline)?;
    let candidate_runs = run_outcomes("candidate", candidate)?;
    require_identical_keys(&baseline_runs, &candidate_runs)?;

    let mut both_passed = 0;
    let mut both_failed = 0;
    let mut regressions = 0;
    let mut improvements = 0;
    for (key, baseline_passed) in &baseline_runs {
        let candidate_passed = candidate_runs[key];
        match (*baseline_passed, candidate_passed) {
            (true, true) => both_passed += 1,
            (false, false) => both_failed += 1,
            (true, false) => regressions += 1,
            (false, true) => improvements += 1,
        }
    }

    let pairs = baseline_runs.len();
    let baseline_pass_rate = rate(both_passed + regressions, pairs);
    let candidate_pass_rate = rate(both_passed + improvements, pairs);
    let pass_rate_drop = paired_pass_rate_drop(regressions, improvements, pairs);
    let effect_threshold_met = effect_threshold_met(
        regressions,
        improvements,
        pairs,
        config.minimum_pass_rate_drop,
    );

    let log_p_value = (pairs >= config.minimum_pairs)
        .then(|| exact_binomial_upper_log_tail(regressions + improvements, regressions));
    let p_value = log_p_value.map(display_p_value);
    let significance_threshold_met = log_p_value
        .map(|value| log_probability_at_or_below(value, config.significance_level.ln()))
        .unwrap_or(false);
    let decision = statistical_decision(
        pairs,
        config,
        effect_threshold_met,
        significance_threshold_met,
    );

    Ok(StatisticalRegressionReport {
        format_version: STATISTICAL_REGRESSION_FORMAT_VERSION,
        baseline: baseline.name.clone(),
        candidate: candidate.name.clone(),
        dataset_name: baseline.dataset_name.clone(),
        dataset_version: baseline.dataset_version.clone(),
        config: *config,
        pairs,
        both_passed,
        both_failed,
        regressions,
        improvements,
        baseline_pass_rate,
        candidate_pass_rate,
        pass_rate_drop,
        p_value,
        effect_threshold_met,
        significance_threshold_met,
        decision,
    })
}

type RunKey = (String, usize);

fn run_outcomes(label: &str, report: &ExperimentReport) -> Result<BTreeMap<RunKey, bool>> {
    let mut outcomes = BTreeMap::new();
    let mut case_ids = BTreeSet::new();
    for case in &report.cases {
        if case.case_id.trim().is_empty() {
            return statistics_error(format!("{label} report contains an empty case id"));
        }
        if !case_ids.insert(case.case_id.as_str()) {
            return statistics_error(format!(
                "{label} report contains duplicate case id `{}`",
                case.case_id
            ));
        }
        if case.runs.len() != report.runs_per_case {
            return statistics_error(format!(
                "{label} case `{}` has {} runs; expected {}",
                case.case_id,
                case.runs.len(),
                report.runs_per_case
            ));
        }
        for run in &case.runs {
            if run.repetition >= report.runs_per_case {
                return statistics_error(format!(
                    "{label} case `{}` has out-of-range repetition {}",
                    case.case_id, run.repetition
                ));
            }
            let key = (case.case_id.clone(), run.repetition);
            if outcomes.insert(key.clone(), run.passed).is_some() {
                return statistics_error(format!(
                    "{label} report contains duplicate run `{}#{}`",
                    key.0, key.1
                ));
            }
        }
    }
    Ok(outcomes)
}

fn require_identical_keys(
    baseline: &BTreeMap<RunKey, bool>,
    candidate: &BTreeMap<RunKey, bool>,
) -> Result<()> {
    if baseline.keys().eq(candidate.keys()) {
        return Ok(());
    }
    if let Some((case_id, repetition)) = baseline.keys().find(|key| !candidate.contains_key(*key)) {
        return statistics_error(format!(
            "candidate report is missing paired run `{case_id}#{repetition}`"
        ));
    }
    if let Some((case_id, repetition)) = candidate.keys().find(|key| !baseline.contains_key(*key)) {
        return statistics_error(format!(
            "baseline report is missing paired run `{case_id}#{repetition}`"
        ));
    }
    statistics_error("reports do not contain identical paired runs")
}

fn validate_input_report(label: &str, report: &ExperimentReport) -> Result<()> {
    crate::gate::validate_report(report)
        .map_err(|error| EvalError::Statistics(format!("{label} report is invalid: {error}")))
}

/// `P(X >= regressions)` for `X ~ Binomial(discordant, 0.5)`.
///
/// Log-space accumulation avoids overflow in binomial coefficients. Results
/// smaller than the least positive subnormal `f64` are clamped to that value so
/// a mathematically non-zero p-value is never reported as zero.
#[cfg(test)]
fn exact_binomial_upper_tail(discordant: usize, regressions: usize) -> f64 {
    display_p_value(exact_binomial_upper_log_tail(discordant, regressions))
}

fn exact_binomial_upper_log_tail(discordant: usize, regressions: usize) -> f64 {
    if discordant == 0 || regressions == 0 {
        return 0.0;
    }

    let mode = discordant / 2;
    let max_index = regressions.max(mode);
    let max_log_mass = log_binomial_mass(discordant, max_index);
    let mut log_mass = log_binomial_mass(discordant, regressions);
    let mut scaled_sum = 0.0;
    for k in regressions..=discordant {
        scaled_sum += (log_mass - max_log_mass).exp();
        if k < discordant {
            log_mass += ((discordant - k) as f64).ln() - ((k + 1) as f64).ln();
        }
    }

    let log_tail = max_log_mass + scaled_sum.ln();
    log_tail.min(0.0)
}

fn display_p_value(log_p_value: f64) -> f64 {
    const MIN_SUBNORMAL: f64 = f64::from_bits(1);
    if log_p_value >= 0.0 {
        1.0
    } else {
        log_p_value.exp().max(MIN_SUBNORMAL)
    }
}

fn log_binomial_mass(trials: usize, successes: usize) -> f64 {
    let factors = successes.min(trials - successes);
    let mut log_choose = 0.0;
    for i in 1..=factors {
        log_choose += ((trials - factors + i) as f64).ln() - (i as f64).ln();
    }
    log_choose - trials as f64 * LN_2
}

fn validate_rate(label: &str, value: f64) -> Result<()> {
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        return statistics_error(format!("{label} must be a finite number in 0.0..=1.0"));
    }
    Ok(())
}

fn rate(passed: usize, pairs: usize) -> f64 {
    if pairs == 0 {
        0.0
    } else {
        passed as f64 / pairs as f64
    }
}

fn paired_pass_rate_drop(regressions: usize, improvements: usize, pairs: usize) -> f64 {
    if pairs == 0 {
        0.0
    } else {
        (regressions as f64 - improvements as f64) / pairs as f64
    }
}

fn effect_threshold_met(
    regressions: usize,
    improvements: usize,
    pairs: usize,
    minimum_drop: f64,
) -> bool {
    regressions > improvements
        && paired_pass_rate_drop(regressions, improvements, pairs) >= minimum_drop
}

fn statistical_decision(
    pairs: usize,
    config: &StatisticalRegressionConfig,
    effect_threshold_met: bool,
    significance_threshold_met: bool,
) -> StatisticalDecision {
    if pairs < config.minimum_pairs {
        StatisticalDecision::InsufficientEvidence
    } else if effect_threshold_met && significance_threshold_met {
        StatisticalDecision::Regression
    } else {
        StatisticalDecision::NoRegression
    }
}

fn same_statistic(actual: f64, expected: f64) -> bool {
    if !actual.is_finite() || !expected.is_finite() {
        return false;
    }
    if actual == expected {
        return true;
    }
    let scale = actual.abs().max(expected.abs());
    (actual - expected).abs() <= 16.0 * f64::EPSILON * scale
}

fn same_optional_statistic(actual: Option<f64>, expected: Option<f64>) -> bool {
    match (actual, expected) {
        (Some(actual), Some(expected)) => same_statistic(actual, expected),
        (None, None) => true,
        _ => false,
    }
}

fn log_probability_at_or_below(actual: f64, threshold: f64) -> bool {
    if actual <= threshold {
        return true;
    }
    let scale = actual.abs().max(threshold.abs()).max(1.0);
    actual - threshold <= 16.0 * f64::EPSILON * scale
}

fn require_non_empty(label: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return statistics_error(format!("{label} must not be empty"));
    }
    Ok(())
}

fn validate_significance_level(value: f64) -> Result<()> {
    if !value.is_finite() || value <= 0.0 || value >= 1.0 {
        return statistics_error(
            "significance level must be a finite number strictly between 0 and 1",
        );
    }
    Ok(())
}

fn statistics_error<T>(message: impl Into<String>) -> Result<T> {
    Err(EvalError::Statistics(message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_known_upper_tails() {
        assert_eq!(exact_binomial_upper_tail(0, 0), 1.0);
        assert_eq!(exact_binomial_upper_tail(5, 5), 0.03125);
        assert!((exact_binomial_upper_tail(10, 8) - 0.054_687_5).abs() < 1e-14);
        assert_eq!(exact_binomial_upper_tail(10, 0), 1.0);
    }

    #[test]
    fn exact_tail_stays_finite_for_large_samples() {
        let tiny = exact_binomial_upper_tail(10_000, 10_000);
        assert!(tiny.is_finite());
        assert!(tiny > 0.0);
        assert!(tiny <= f64::MIN_POSITIVE);

        let balanced = exact_binomial_upper_tail(10_000, 5_000);
        assert!(balanced > 0.5 && balanced < 0.51, "{balanced}");
    }
}
