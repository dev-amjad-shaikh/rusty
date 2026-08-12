//! The experiment runner: dataset in, evidence-graded report out.
//!
//! [`ExperimentRunner`] drives one agent over a [`Dataset`], N times per
//! case, through the real `rusty-agent-runtime` executor — no simulation
//! harness. Each run records into its own Flight Recorder journal; the
//! journal plus the run outcome become [`RunEvidence`], which the case's
//! assertions grade. Runs are sequential by default or explicitly
//! bounded-parallel; each gets a fresh journal, so repetition *i*'s evidence
//! can never bleed into repetition *i+1*'s.
//!
//! The output is a serializable [`ExperimentReport`]: per-case-run detail
//! (assertion verdicts with evidence, status, latency, cost), pass rates per
//! assertion, latency percentiles, and totals — everything
//! [`crate::compare()`] needs to judge a candidate against a baseline.
//! Parallel completion never controls artifact ordering or which
//! infrastructure error is returned.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use futures::{stream::FuturesUnordered, StreamExt};
use serde::{Deserialize, Serialize};

use rusty_agent_runtime::error::Result as RuntimeResult;
use rusty_agent_runtime::executor::{ExecutionOutcome, Executor, RunConfig};
use rusty_agent_runtime::graph::Graph;
use rusty_agent_runtime::journal::{Clock, Journal};
use rusty_agent_runtime::state::{State, StateSpec};
use serde_json::Value;

use crate::assertion::AssertionResult;
use crate::dataset::{Dataset, EvalCase};
use crate::error::{EvalError, Result};
use crate::evidence::{RunEvidence, RunStatus};
use crate::judge::{JudgeModel, JudgeRequest, JudgeVerdict};

/// The report format version this build writes and reads.
pub const REPORT_FORMAT_VERSION: u64 = 1;

/// Everything the runner needs to execute one case repetition.
///
/// Built by the agent factory per run: graphs are cheap to compile, and a
/// fresh build per repetition guarantees no state leaks between runs. The
/// factory receives the run's [`Journal`] so recording-wired graphs (e.g.
/// `create_react_agent_with_recording`) journal their model and tool calls
/// into the run's evidence.
pub struct PreparedRun {
    /// The compiled graph under test.
    pub graph: Graph,

    /// The state schema (channels + reducers).
    pub spec: StateSpec,

    /// Explicit initial state. `None` derives it from the case's `input`
    /// payload (which must then be a JSON object of channel values).
    pub initial_state: Option<State>,
}

impl PreparedRun {
    /// A prepared run whose initial state derives from the case input.
    pub fn new(graph: Graph, spec: StateSpec) -> Self {
        Self {
            graph,
            spec,
            initial_state: None,
        }
    }

    /// Override the initial state (bypasses derivation from the case input).
    pub fn with_initial_state(mut self, state: State) -> Self {
        self.initial_state = Some(state);
        self
    }
}

/// Experiment configuration.
pub struct ExperimentConfig {
    runs_per_case: usize,
    max_concurrency: usize,
    judge: Option<Arc<dyn JudgeModel>>,
}

impl Default for ExperimentConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl ExperimentConfig {
    /// One run per case, no judge.
    pub fn new() -> Self {
        Self {
            runs_per_case: 1,
            max_concurrency: 1,
            judge: None,
        }
    }

    /// Run each case `n` times. Repetitions exist to expose flaky behavior;
    /// pass rates are computed over all repetitions.
    pub fn with_runs_per_case(mut self, n: usize) -> Self {
        self.runs_per_case = n.max(1);
        self
    }

    /// Allow at most `n` case runs to execute concurrently.
    ///
    /// The default is `1`, preserving sequential execution and uncontended
    /// latency measurement. Zero is normalized to one, matching
    /// [`Self::with_runs_per_case`]. Parallel runs still receive isolated
    /// graphs and journals, and the report remains ordered by dataset case
    /// then repetition regardless of completion order.
    pub fn with_max_concurrency(mut self, n: usize) -> Self {
        self.max_concurrency = n.max(1);
        self
    }

    /// Attach a judge: every case run is additionally scored through it and
    /// the verdict recorded in the report.
    pub fn with_judge(mut self, judge: Arc<dyn JudgeModel>) -> Self {
        self.judge = Some(judge);
        self
    }
}

impl std::fmt::Debug for ExperimentConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExperimentConfig")
            .field("runs_per_case", &self.runs_per_case)
            .field("max_concurrency", &self.max_concurrency)
            .field("judge", &self.judge.as_ref().map(|_| "<judge>"))
            .finish()
    }
}

/// The experiment runner. Stateless across experiments.
pub struct ExperimentRunner {
    config: ExperimentConfig,
}

impl ExperimentRunner {
    /// A runner with `config`.
    pub fn new(config: ExperimentConfig) -> Self {
        Self { config }
    }

    /// Run `prepare`'s agent over every case of `dataset`,
    /// `runs_per_case` times each, and grade the evidence.
    ///
    /// `prepare` builds the graph under test for one case run and receives
    /// the run's journal for evidence wiring. At most the configured number
    /// of runs are in flight. The default sequential path retains fail-fast
    /// behavior. In parallel mode, observing an infrastructure error stops
    /// admission of new runs; the already-active window settles, and the
    /// earliest dataset-case/repetition error from that window wins
    /// deterministically.
    pub async fn run<F>(&self, dataset: &Dataset, prepare: F) -> Result<ExperimentReport>
    where
        F: Fn(&EvalCase, &Journal) -> RuntimeResult<PreparedRun>,
    {
        if self.config.max_concurrency == 1 {
            let mut case_reports = Vec::with_capacity(dataset.cases().len());
            for case in dataset.cases() {
                let mut runs = Vec::with_capacity(self.config.runs_per_case);
                for repetition in 0..self.config.runs_per_case {
                    runs.push(
                        self.run_case_once(dataset, case, repetition, &prepare)
                            .await?,
                    );
                }
                case_reports.push(case_report(case, runs));
            }
            return Ok(self.finish_report(dataset, case_reports));
        }

        let mut jobs = dataset
            .cases()
            .iter()
            .enumerate()
            .flat_map(|(case_index, case)| {
                (0..self.config.runs_per_case).map(move |repetition| (case_index, case, repetition))
            });
        let prepare = &prepare;
        let run_job = |(case_index, case, repetition)| async move {
            (
                case_index,
                repetition,
                self.run_case_once(dataset, case, repetition, prepare).await,
            )
        };
        let mut active = FuturesUnordered::new();
        for _ in 0..self.config.max_concurrency {
            let Some(job) = jobs.next() else {
                break;
            };
            active.push(run_job(job));
        }

        let mut completed = Vec::new();
        let mut admission_stopped = false;
        while let Some(result) = active.next().await {
            admission_stopped |= result.2.is_err();
            completed.push(result);
            if !admission_stopped {
                if let Some(job) = jobs.next() {
                    active.push(run_job(job));
                }
            }
        }

        completed.sort_unstable_by_key(|(case_index, repetition, _)| (*case_index, *repetition));
        let mut runs_by_case: Vec<Vec<CaseRunReport>> = (0..dataset.cases().len())
            .map(|_| Vec::with_capacity(self.config.runs_per_case))
            .collect();
        for (case_index, _, result) in completed {
            runs_by_case[case_index].push(result?);
        }

        let mut case_reports = Vec::with_capacity(dataset.cases().len());
        for (case, runs) in dataset.cases().iter().zip(runs_by_case) {
            case_reports.push(case_report(case, runs));
        }

        Ok(self.finish_report(dataset, case_reports))
    }

    fn finish_report(&self, dataset: &Dataset, case_reports: Vec<CaseReport>) -> ExperimentReport {
        let summary = ReportSummary::compute(&case_reports);
        tracing::info!(
            dataset = %dataset.name(),
            version = %dataset.version(),
            max_concurrency = self.config.max_concurrency,
            cases = summary.cases,
            runs = summary.runs,
            run_pass_rate = summary.run_pass_rate,
            "experiment complete"
        );
        ExperimentReport {
            format_version: REPORT_FORMAT_VERSION,
            name: format!("{}@{}", dataset.name(), dataset.version()),
            dataset_name: dataset.name().to_owned(),
            dataset_version: dataset.version().to_owned(),
            runs_per_case: self.config.runs_per_case,
            max_concurrency: self.config.max_concurrency,
            cases: case_reports,
            summary,
        }
    }

    /// One case repetition: build, run, distill evidence, grade.
    async fn run_case_once<F>(
        &self,
        dataset: &Dataset,
        case: &EvalCase,
        repetition: usize,
        prepare: &F,
    ) -> Result<CaseRunReport>
    where
        F: Fn(&EvalCase, &Journal) -> RuntimeResult<PreparedRun>,
    {
        let run_id = format!("eval:{}:{}:{repetition}", dataset.name(), case.id);
        let journal = Journal::new(run_id.clone(), run_id.clone(), Clock::default());
        let prepared = prepare(case, &journal)?;

        let initial_state = match prepared.initial_state {
            Some(state) => state,
            None => State::from_value(case.input.clone()).map_err(|e| {
                EvalError::AgentBuild(format!(
                    "case `{}`: input must be a JSON object of channel values: {e}",
                    case.id
                ))
            })?,
        };

        let executor = Executor::new();
        let started = Instant::now();
        let outcome = executor
            .run(
                &prepared.graph,
                &prepared.spec,
                initial_state,
                RunConfig::new(&run_id).with_journal(journal.clone()),
            )
            .await;
        let latency_ms = started.elapsed().as_millis() as u64;

        let (status, final_state) = match outcome {
            Ok(ExecutionOutcome::Done(state)) => (RunStatus::Done, state.to_value()),
            Ok(ExecutionOutcome::Interrupted { state, .. }) => {
                (RunStatus::Interrupted, state.to_value())
            }
            Err(error) => (
                RunStatus::Failed {
                    error: error.to_string(),
                },
                Value::Null,
            ),
        };

        let evidence = RunEvidence::from_journal(&journal, status, final_state, latency_ms);
        let assertions: Vec<AssertionResult> = case
            .expect
            .assertions()
            .iter()
            .map(|assertion| assertion.evaluate(&evidence))
            .collect();

        let judge = match &self.config.judge {
            Some(judge) => {
                let request = JudgeRequest {
                    case_id: case.id.clone(),
                    input: case.input.clone(),
                    expectations: case.expect.clone(),
                    evidence: evidence.clone(),
                };
                Some(
                    judge
                        .judge(&request)
                        .await
                        .map_err(|e| EvalError::Judge(format!("case `{}`: {e}", case.id)))?,
                )
            }
            None => None,
        };

        let passed = evidence.status.is_done()
            && assertions.iter().all(|result| result.passed)
            && judge.as_ref().map(|verdict| verdict.passed).unwrap_or(true);

        Ok(CaseRunReport {
            repetition,
            status: evidence.status,
            passed,
            assertions,
            judge,
            tool_calls: evidence.tool_calls.len(),
            latency_ms: evidence.latency_ms,
            cost_usd: evidence.cost_usd,
            total_tokens: evidence.total_tokens,
        })
    }
}

fn case_report(case: &EvalCase, runs: Vec<CaseRunReport>) -> CaseReport {
    let pass_rate = runs.iter().filter(|run| run.passed).count() as f64 / runs.len() as f64;
    CaseReport {
        case_id: case.id.clone(),
        tags: case.tags.clone(),
        pass_rate,
        runs,
    }
}

/// One case's runs within an experiment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaseReport {
    /// The case id (dataset key).
    pub case_id: String,

    /// The case's tags, carried for report slicing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,

    /// Fraction of this case's runs that passed (completed run, all
    /// assertions green, judge — when present — passing).
    pub pass_rate: f64,

    /// Per-repetition detail.
    pub runs: Vec<CaseRunReport>,
}

/// One case repetition, fully graded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaseRunReport {
    /// The 0-based repetition index.
    pub repetition: usize,

    /// How the run terminated.
    pub status: RunStatus,

    /// The case-run verdict: run completed, every assertion passed, and the
    /// judge (when configured) passed.
    pub passed: bool,

    /// Every assertion verdict, with evidence.
    pub assertions: Vec<AssertionResult>,

    /// The judge's verdict, when a judge is configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judge: Option<JudgeVerdict>,

    /// Number of journaled tool calls.
    pub tool_calls: usize,

    /// Wall latency of the run in milliseconds.
    pub latency_ms: u64,

    /// Total journaled cost of the run in USD.
    pub cost_usd: f64,

    /// Total tokens reported by the run's model calls.
    pub total_tokens: u64,
}

/// Pass-rate aggregate for one assertion key across all runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssertionPassRate {
    /// The assertion's report key ([`crate::assertion::Assertion::name`]).
    pub assertion: String,

    /// Runs where this assertion passed.
    pub passed: usize,

    /// Runs where this assertion was evaluated.
    pub total: usize,

    /// `passed / total` (0.0 when never evaluated).
    pub rate: f64,
}

/// Latency distribution over all case runs, nearest-rank percentiles.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LatencyStats {
    /// Fastest run (ms).
    pub min: u64,
    /// Median run (ms).
    pub p50: u64,
    /// 95th-percentile run (ms).
    pub p95: u64,
    /// Slowest run (ms).
    pub max: u64,
    /// Mean latency (ms).
    pub mean: f64,
}

/// Experiment-wide aggregates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReportSummary {
    /// Number of cases in the dataset.
    pub cases: usize,

    /// Total case runs executed.
    pub runs: usize,

    /// Case runs that passed.
    pub runs_passed: usize,

    /// `runs_passed / runs` (0.0 for an empty dataset).
    pub run_pass_rate: f64,

    /// Mean of per-case pass rates.
    pub case_pass_rate: f64,

    /// Pass rate per assertion key, aggregated over all runs.
    pub assertions: Vec<AssertionPassRate>,

    /// Latency distribution over all runs.
    pub latency_ms: LatencyStats,

    /// Total journaled cost across all runs (USD).
    pub total_cost_usd: f64,

    /// Total tokens across all runs.
    pub total_tokens: u64,
}

impl ReportSummary {
    fn compute(cases: &[CaseReport]) -> Self {
        let runs: Vec<&CaseRunReport> = cases.iter().flat_map(|case| &case.runs).collect();
        let runs_passed = runs.iter().filter(|run| run.passed).count();

        let mut latencies: Vec<u64> = runs.iter().map(|run| run.latency_ms).collect();
        latencies.sort_unstable();
        let latency_ms = LatencyStats {
            min: latencies.first().copied().unwrap_or(0),
            p50: percentile(&latencies, 50.0),
            p95: percentile(&latencies, 95.0),
            max: latencies.last().copied().unwrap_or(0),
            mean: if latencies.is_empty() {
                0.0
            } else {
                latencies.iter().sum::<u64>() as f64 / latencies.len() as f64
            },
        };

        // BTreeMap: assertion keys aggregate in sorted order, so reports
        // serialize deterministically.
        let mut by_assertion: BTreeMap<String, (usize, usize)> = BTreeMap::new();
        for run in &runs {
            for result in &run.assertions {
                let entry = by_assertion.entry(result.assertion.clone()).or_default();
                entry.1 += 1;
                entry.0 += usize::from(result.passed);
            }
        }
        let assertions = by_assertion
            .into_iter()
            .map(|(assertion, (passed, total))| AssertionPassRate {
                assertion,
                passed,
                total,
                rate: if total == 0 {
                    0.0
                } else {
                    passed as f64 / total as f64
                },
            })
            .collect();

        Self {
            cases: cases.len(),
            runs: runs.len(),
            runs_passed,
            run_pass_rate: if runs.is_empty() {
                0.0
            } else {
                runs_passed as f64 / runs.len() as f64
            },
            case_pass_rate: if cases.is_empty() {
                0.0
            } else {
                cases.iter().map(|case| case.pass_rate).sum::<f64>() / cases.len() as f64
            },
            assertions,
            latency_ms,
            total_cost_usd: runs.iter().map(|run| run.cost_usd).sum(),
            total_tokens: runs.iter().map(|run| run.total_tokens).sum(),
        }
    }
}

/// The graded output of one experiment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentReport {
    /// Report schema version ([`REPORT_FORMAT_VERSION`]).
    pub format_version: u64,

    /// Experiment identity: `{dataset_name}@{dataset_version}`.
    pub name: String,

    /// The dataset that was run.
    pub dataset_name: String,

    /// The dataset version that was run.
    pub dataset_version: String,

    /// Repetitions per case.
    pub runs_per_case: usize,

    /// Maximum case runs allowed in flight when this report was produced.
    #[serde(default = "default_max_concurrency")]
    pub max_concurrency: usize,

    /// Per-case detail.
    pub cases: Vec<CaseReport>,

    /// Experiment-wide aggregates.
    pub summary: ReportSummary,
}

fn default_max_concurrency() -> usize {
    1
}

impl ExperimentReport {
    /// Serialize as pretty JSON (for `baseline.json`-style artifacts).
    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// Parse a report written by [`ExperimentReport::to_json`]. Refuses
    /// reports from an incompatible format version.
    pub fn from_json(text: &str) -> Result<Self> {
        let report: ExperimentReport = serde_json::from_str(text)?;
        if report.format_version != REPORT_FORMAT_VERSION {
            return Err(EvalError::UnsupportedVersion {
                found: report.format_version,
                supported: REPORT_FORMAT_VERSION,
            });
        }
        Ok(report)
    }
}

/// Validate that a report is internally coherent before it is compared,
/// persisted, or presented as evidence.
pub fn validate_report(report: &ExperimentReport) -> Result<()> {
    crate::gate::validate_report(report)
}

/// Nearest-rank percentile over a sorted slice. Empty input yields 0 — an
/// experiment with no runs has no distribution, and zero is the honest
/// placeholder rather than a panic.
fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (p / 100.0 * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_nearest_rank() {
        let values: Vec<u64> = (1..=100).collect();
        assert_eq!(percentile(&values, 50.0), 50);
        assert_eq!(percentile(&values, 95.0), 95);
        assert_eq!(percentile(&values, 100.0), 100);
        assert_eq!(percentile(&values, 0.0), 1);
    }

    #[test]
    fn percentile_small_samples() {
        assert_eq!(percentile(&[7], 95.0), 7);
        assert_eq!(percentile(&[10, 20], 50.0), 10);
        assert_eq!(percentile(&[10, 20], 95.0), 20);
        assert_eq!(percentile(&[], 50.0), 0);
    }
}
