//! Conformance suites: first-class eval types for testing platform seams.
//!
//! A conformance suite is a collection of [`ConformanceCase`]s, each
//! declaring a check to run against a target implementation (a storage
//! backend, a seam handler, or a channel adapter).  The
//! [`ConformanceRunner`] executes every case and produces a
//! [`ConformanceReport`] whose shape is compatible with
//! [`crate::experiment::ExperimentReport`] so that gates, comparisons,
//! and the REST surface treat conformance results identically to
//! behavioural eval results.
//!
//! Check implementations live in their owning crates; `rusty-eval` owns
//! the suite format, the runner, and the report schema.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{EvalError, Result};
use crate::experiment::{
    CaseReport, CaseRunReport, ExperimentReport, ReportSummary, REPORT_FORMAT_VERSION,
};

/// Schema version of the conformance suite format.
pub const CONFORMANCE_SUITE_FORMAT_VERSION: u64 = 1;

/// Schema version of the conformance report format.
pub const CONFORMANCE_REPORT_FORMAT_VERSION: u64 = 1;

/// Severity of a conformance case: blocking failures fail the suite,
/// warnings surface in the report but do not fail the suite verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConformanceSeverity {
    Blocking,
    Warning,
}

/// One case inside a conformance suite: a declarative check
/// parameterised by `parameters` and executed by the check
/// implementation named by `check_type`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConformanceCase {
    /// Stable case id, unique within the suite.
    pub id: String,

    /// Human-readable description of what this check validates.
    pub description: String,

    /// Whether a failure here blocks the suite.
    pub severity: ConformanceSeverity,

    /// Identifies the check implementation (e.g. `store::round_trip`).
    pub check_type: String,

    /// JSON parameters forwarded to the check implementation.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub parameters: Value,
}

/// A conformance suite: a versioned, ordered set of cases.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConformanceSuite {
    /// Suite format version.
    pub format_version: u64,

    /// Suite name.
    pub name: String,

    /// Suite version (semver).
    pub version: String,

    /// Ordered cases.
    pub cases: Vec<ConformanceCase>,
}

impl ConformanceSuite {
    /// Create a new suite, validating invariants.
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Result<Self> {
        let name = name.into();
        let version = version.into();
        if name.is_empty() {
            return Err(EvalError::Validation("suite name must not be empty".into()));
        }
        if version.is_empty() {
            return Err(EvalError::Validation(
                "suite version must not be empty".into(),
            ));
        }
        Ok(Self {
            format_version: CONFORMANCE_SUITE_FORMAT_VERSION,
            name,
            version,
            cases: Vec::new(),
        })
    }

    /// Append a case.
    pub fn with_case(mut self, case: ConformanceCase) -> Self {
        self.cases.push(case);
        self
    }

    /// Serialize as canonical JSON.
    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// Parse a suite, validating the format version.
    pub fn from_json(text: &str) -> Result<Self> {
        let suite: Self = serde_json::from_str(text)?;
        if suite.format_version != CONFORMANCE_SUITE_FORMAT_VERSION {
            return Err(EvalError::UnsupportedVersion {
                found: suite.format_version,
                supported: CONFORMANCE_SUITE_FORMAT_VERSION,
            });
        }
        Ok(suite)
    }
}

/// The result of executing one conformance case.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConformanceVerdict {
    /// The case id.
    pub case_id: String,

    /// `true` when the check passed.
    pub passed: bool,

    /// Human-readable detail, especially on failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,

    /// Wall time of the check in milliseconds.
    pub latency_ms: u64,
}

/// The graded output of one conformance run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConformanceReport {
    /// Report format version.
    pub format_version: u64,

    /// Suite identity: `{name}@{version}`.
    pub name: String,

    /// Suite name.
    pub suite_name: String,

    /// Suite version.
    pub suite_version: String,

    /// The target the suite ran against.
    pub target: String,

    /// Overall verdict: `true` when every blocking case passed.
    pub passed: bool,

    /// Per-case results.
    pub cases: Vec<ConformanceVerdict>,

    /// When the run started.
    pub started_at: DateTime<Utc>,

    /// When the run finished.
    pub finished_at: DateTime<Utc>,
}

impl ConformanceReport {
    /// Serialize as pretty JSON.
    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// Parse a report, validating format version.
    pub fn from_json(text: &str) -> Result<Self> {
        let report: Self = serde_json::from_str(text)?;
        if report.format_version != CONFORMANCE_REPORT_FORMAT_VERSION {
            return Err(EvalError::UnsupportedVersion {
                found: report.format_version,
                supported: CONFORMANCE_REPORT_FORMAT_VERSION,
            });
        }
        Ok(report)
    }

    /// Convert into an [`ExperimentReport`] so that gates and comparisons
    /// consume conformance results identically to behavioural results.
    ///
    /// Each conformance case becomes one experiment case with a single
    /// run.  Blocking severity maps to the `pass_rate` semantics (a
    /// blocking failure yields `pass_rate = 0.0`); warning severity maps
    /// to `pass_rate = 1.0` even on failure so that gates do not block
    /// on warnings.
    pub fn into_experiment_report(&self) -> ExperimentReport {
        let case_reports: Vec<CaseReport> = self
            .cases
            .iter()
            .map(|verdict| {
                let run_passed = verdict.passed
                    || self
                        .cases
                        .iter()
                        .find(|c| c.case_id == verdict.case_id)
                        .map(|_| false)
                        .unwrap_or(true);
                // Look up the original case to determine severity.
                // Since we don't have the suite here, we use passed
                // directly.  Callers that need severity-aware
                // translation can use the suite-aware variant below.
                CaseReport {
                    case_id: verdict.case_id.clone(),
                    tags: Vec::new(),
                    pass_rate: if run_passed { 1.0 } else { 0.0 },
                    runs: vec![CaseRunReport {
                        repetition: 0,
                        status: crate::evidence::RunStatus::Done,
                        passed: run_passed,
                        assertions: Vec::new(),
                        judge: None,
                        tool_calls: 0,
                        latency_ms: verdict.latency_ms,
                        cost_usd: 0.0,
                        total_tokens: 0,
                    }],
                }
            })
            .collect();

        let summary = ReportSummary::compute(&case_reports);

        ExperimentReport {
            format_version: REPORT_FORMAT_VERSION,
            name: format!("{}@{}", self.suite_name, self.suite_version),
            dataset_name: self.suite_name.clone(),
            dataset_version: self.suite_version.clone(),
            runs_per_case: 1,
            max_concurrency: 1,
            cases: case_reports,
            summary,
        }
    }
}

/// An executable conformance check.
///
/// Owninig crates (storage, seam dispatch, channel adapters, sandbox
/// executors) implement this trait for their checks.  The runner
/// parameterises each check with a `target` string (e.g. a backend
/// identity) and optional JSON parameters.
#[async_trait::async_trait]
pub trait ConformanceCheck: Send + Sync {
    /// Unique check type identifier (must match
    /// [`ConformanceCase::check_type`]).
    fn check_type(&self) -> &'static str;

    /// Execute the check against `target` with `parameters`.
    async fn run(&self, target: &str, parameters: &Value) -> Result<ConformanceVerdict>;
}

/// The conformance runner.  Stateless across runs.
pub struct ConformanceRunner {
    /// Registry of check implementations keyed by `check_type`.
    checks: BTreeMap<String, Box<dyn ConformanceCheck>>,
}

impl Default for ConformanceRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl ConformanceRunner {
    /// Create an empty runner.
    pub fn new() -> Self {
        Self {
            checks: BTreeMap::new(),
        }
    }

    /// Register a check implementation.
    pub fn register(&mut self, check: Box<dyn ConformanceCheck>) {
        self.checks.insert(check.check_type().to_owned(), check);
    }

    /// Run `suite` against `target`, executing every case through the
    /// registered checks.  A missing check implementation yields a
    /// failing verdict with a typed reason.
    pub async fn run(&self, suite: &ConformanceSuite, target: &str) -> ConformanceReport {
        let started_at = Utc::now();
        let mut cases = Vec::with_capacity(suite.cases.len());
        let mut passed = true;

        for case in &suite.cases {
            let started = std::time::Instant::now();
            let verdict = match self.checks.get(&case.check_type) {
                Some(check) => match check.run(target, &case.parameters).await {
                    Ok(mut v) => {
                        v.case_id = case.id.clone();
                        v
                    }
                    Err(error) => ConformanceVerdict {
                        case_id: case.id.clone(),
                        passed: false,
                        reason: Some(format!("check implementation error: {error}")),
                        latency_ms: started.elapsed().as_millis() as u64,
                    },
                },
                None => ConformanceVerdict {
                    case_id: case.id.clone(),
                    passed: false,
                    reason: Some(format!(
                        "no check implementation registered for type `{}`",
                        case.check_type
                    )),
                    latency_ms: started.elapsed().as_millis() as u64,
                },
            };

            if case.severity == ConformanceSeverity::Blocking && !verdict.passed {
                passed = false;
            }

            cases.push(verdict);
        }

        let finished_at = Utc::now();

        ConformanceReport {
            format_version: CONFORMANCE_REPORT_FORMAT_VERSION,
            name: format!("{}@{}", suite.name, suite.version),
            suite_name: suite.name.clone(),
            suite_version: suite.version.clone(),
            target: target.to_owned(),
            passed,
            cases,
            started_at,
            finished_at,
        }
    }
}

/// Translate a conformance report into an experiment report, using the
/// suite's severity information so that warnings do not count as
/// failures in the gate.
pub fn to_experiment_report(
    report: &ConformanceReport,
    suite: &ConformanceSuite,
) -> ExperimentReport {
    let severity_by_id: BTreeMap<String, ConformanceSeverity> = suite
        .cases
        .iter()
        .map(|c| (c.id.clone(), c.severity))
        .collect();

    let case_reports: Vec<CaseReport> = report
        .cases
        .iter()
        .map(|verdict| {
            let severity = severity_by_id.get(&verdict.case_id).copied();
            // Blocking failures reduce pass_rate; warnings keep pass_rate 1.0
            let pass_rate = if verdict.passed {
                1.0
            } else {
                match severity {
                    Some(ConformanceSeverity::Blocking) => 0.0,
                    Some(ConformanceSeverity::Warning) => 1.0,
                    None => 0.0,
                }
            };

            CaseReport {
                case_id: verdict.case_id.clone(),
                tags: Vec::new(),
                pass_rate,
                runs: vec![CaseRunReport {
                    repetition: 0,
                    status: crate::evidence::RunStatus::Done,
                    passed: verdict.passed,
                    assertions: Vec::new(),
                    judge: None,
                    tool_calls: 0,
                    latency_ms: verdict.latency_ms,
                    cost_usd: 0.0,
                    total_tokens: 0,
                }],
            }
        })
        .collect();

    let summary = ReportSummary::compute(&case_reports);

    ExperimentReport {
        format_version: REPORT_FORMAT_VERSION,
        name: format!("{}@{}", suite.name, suite.version),
        dataset_name: suite.name.clone(),
        dataset_version: suite.version.clone(),
        runs_per_case: 1,
        max_concurrency: 1,
        cases: case_reports,
        summary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct AlwaysPass;

    #[async_trait::async_trait]
    impl ConformanceCheck for AlwaysPass {
        fn check_type(&self) -> &'static str {
            "always_pass"
        }

        async fn run(&self, _target: &str, _parameters: &Value) -> Result<ConformanceVerdict> {
            Ok(ConformanceVerdict {
                case_id: "test".into(),
                passed: true,
                reason: None,
                latency_ms: 1,
            })
        }
    }

    struct AlwaysFail;

    #[async_trait::async_trait]
    impl ConformanceCheck for AlwaysFail {
        fn check_type(&self) -> &'static str {
            "always_fail"
        }

        async fn run(&self, _target: &str, _parameters: &Value) -> Result<ConformanceVerdict> {
            Ok(ConformanceVerdict {
                case_id: "test".into(),
                passed: false,
                reason: Some("expected failure".into()),
                latency_ms: 1,
            })
        }
    }

    #[tokio::test]
    async fn runner_all_pass() {
        let suite = ConformanceSuite::new("demo", "1.0.0")
            .unwrap()
            .with_case(ConformanceCase {
                id: "a".into(),
                description: "passes".into(),
                severity: ConformanceSeverity::Blocking,
                check_type: "always_pass".into(),
                parameters: Value::Null,
            });

        let mut runner = ConformanceRunner::new();
        runner.register(Box::new(AlwaysPass));
        let report = runner.run(&suite, "test-target").await;

        assert!(report.passed);
        assert_eq!(report.cases.len(), 1);
        assert!(report.cases[0].passed);
    }

    #[tokio::test]
    async fn runner_blocking_fail() {
        let suite = ConformanceSuite::new("demo", "1.0.0")
            .unwrap()
            .with_case(ConformanceCase {
                id: "a".into(),
                description: "fails".into(),
                severity: ConformanceSeverity::Blocking,
                check_type: "always_fail".into(),
                parameters: Value::Null,
            });

        let mut runner = ConformanceRunner::new();
        runner.register(Box::new(AlwaysFail));
        let report = runner.run(&suite, "test-target").await;

        assert!(!report.passed);
        assert!(!report.cases[0].passed);
    }

    #[tokio::test]
    async fn runner_warning_does_not_fail_suite() {
        let suite = ConformanceSuite::new("demo", "1.0.0")
            .unwrap()
            .with_case(ConformanceCase {
                id: "a".into(),
                description: "fails".into(),
                severity: ConformanceSeverity::Warning,
                check_type: "always_fail".into(),
                parameters: Value::Null,
            });

        let mut runner = ConformanceRunner::new();
        runner.register(Box::new(AlwaysFail));
        let report = runner.run(&suite, "test-target").await;

        assert!(report.passed); // suite passes because warning
        assert!(!report.cases[0].passed); // but case itself failed
    }

    #[tokio::test]
    async fn runner_missing_check() {
        let suite = ConformanceSuite::new("demo", "1.0.0")
            .unwrap()
            .with_case(ConformanceCase {
                id: "a".into(),
                description: "missing".into(),
                severity: ConformanceSeverity::Blocking,
                check_type: "no_such_check".into(),
                parameters: Value::Null,
            });

        let runner = ConformanceRunner::new();
        let report = runner.run(&suite, "test-target").await;

        assert!(!report.passed);
        assert!(report.cases[0]
            .reason
            .as_ref()
            .unwrap()
            .contains("no check implementation registered"));
    }

    #[tokio::test]
    async fn report_to_experiment_preserves_warning_semantics() {
        let suite = ConformanceSuite::new("demo", "1.0.0")
            .unwrap()
            .with_case(ConformanceCase {
                id: "a".into(),
                description: "fails".into(),
                severity: ConformanceSeverity::Warning,
                check_type: "always_fail".into(),
                parameters: Value::Null,
            });

        let mut runner = ConformanceRunner::new();
        runner.register(Box::new(AlwaysFail));
        let report = runner.run(&suite, "test-target").await;

        let exp = to_experiment_report(&report, &suite);
        assert_eq!(exp.cases[0].pass_rate, 1.0); // warning -> pass_rate 1.0
    }

    #[tokio::test]
    async fn report_to_experiment_blocking_fail_reduces_rate() {
        let suite = ConformanceSuite::new("demo", "1.0.0")
            .unwrap()
            .with_case(ConformanceCase {
                id: "a".into(),
                description: "fails".into(),
                severity: ConformanceSeverity::Blocking,
                check_type: "always_fail".into(),
                parameters: Value::Null,
            });

        let mut runner = ConformanceRunner::new();
        runner.register(Box::new(AlwaysFail));
        let report = runner.run(&suite, "test-target").await;

        let exp = to_experiment_report(&report, &suite);
        assert_eq!(exp.cases[0].pass_rate, 0.0); // blocking -> pass_rate 0.0
    }

    #[test]
    fn suite_round_trip() {
        let suite = ConformanceSuite::new("rt", "1.0.0")
            .unwrap()
            .with_case(ConformanceCase {
                id: "c1".into(),
                description: "d".into(),
                severity: ConformanceSeverity::Blocking,
                check_type: "t".into(),
                parameters: json!({"k": 1}),
            });
        let json = suite.to_json().unwrap();
        let parsed = ConformanceSuite::from_json(&json).unwrap();
        assert_eq!(suite, parsed);
    }

    #[test]
    fn report_round_trip() {
        let report = ConformanceReport {
            format_version: CONFORMANCE_REPORT_FORMAT_VERSION,
            name: "demo@1.0.0".into(),
            suite_name: "demo".into(),
            suite_version: "1.0.0".into(),
            target: "t".into(),
            passed: true,
            cases: vec![ConformanceVerdict {
                case_id: "a".into(),
                passed: true,
                reason: None,
                latency_ms: 5,
            }],
            started_at: Utc::now(),
            finished_at: Utc::now(),
        };
        let json = report.to_json().unwrap();
        let parsed = ConformanceReport::from_json(&json).unwrap();
        assert_eq!(report.passed, parsed.passed);
        assert_eq!(report.cases.len(), parsed.cases.len());
    }
}
