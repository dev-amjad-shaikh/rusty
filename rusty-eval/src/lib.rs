//! # Rusty Eval
//!
//! Rusty Eval is the foundation of Rusty's Agent TestOps plane: it turns the
//! Flight Recorder's run evidence into release decisions. Five pieces, each
//! usable on its own:
//!
//! - **Datasets** ([`dataset`]) — versioned JSONL datasets: a header
//!   (`name`, `version`, `format_version`) plus one [`dataset::EvalCase`]
//!   per line, each carrying an input payload, an expected tool-call
//!   trajectory with argument matchers, final-state predicates, and tags.
//!   Loading validates the schema version; serialization is canonical, so
//!   datasets diff cleanly in git.
//! - **Assertions** ([`assertion`]) — deterministic checks over a recorded
//!   run's [`evidence::RunEvidence`]: tool-call order (subsequence match),
//!   tool-call count, state predicates by JSON pointer, tool blacklists,
//!   cost and latency bounds. Every verdict carries its evidence — expected
//!   vs observed — into the report.
//! - **Experiments** ([`experiment`]) — [`experiment::ExperimentRunner`]
//!   drives one agent over a dataset N times per case through the real
//!   `rusty-agent-runtime` executor, distills each run's journal into
//!   evidence, grades it, and aggregates an
//!   [`experiment::ExperimentReport`]: pass rate per assertion, per-case
//!   detail, latency percentiles, total cost.
//! - **Comparison** ([`compare`]) — [`compare::compare`] diffs a candidate
//!   report against a baseline: per-assertion deltas, per-case regressions
//!   and improvements, and threshold-based regression flags (pass-rate drop,
//!   p95 latency growth).
//! - **Judges** ([`judge`]) — the [`judge::JudgeModel`] trait is the seam
//!   for future LLM-as-judge evaluators, mirroring the runtime's `ChatModel`
//!   style; [`judge::RuleBasedJudge`] is the deterministic implementation,
//!   scoring runs by the fraction of expectations met.
//!
//! ## Quick sketch
//!
//! ```no_run
//! use rusty_eval::{Dataset, ExperimentConfig, ExperimentRunner, PreparedRun, compare, CompareThresholds};
//!
//! # async fn demo() -> rusty_eval::Result<()> {
//! let dataset = Dataset::load("evals/math_tools_v1.jsonl")?;
//!
//! let runner = ExperimentRunner::new(ExperimentConfig::new().with_runs_per_case(3));
//! let baseline = runner
//!     .run(&dataset, |case, journal| {
//!         // Build the agent under test for this case run; `journal` is the
//!         // run's Flight Recorder journal (wire it into recording graphs so
//!         // model/tool calls become assertion evidence).
//!         # let _ = (case, journal);
//!         # unimplemented!()
//!     })
//!     .await?;
//!
//! let candidate = /* run the changed agent the same way */ baseline.clone();
//! let verdict = compare(&baseline, &candidate, &CompareThresholds::default());
//! assert!(!verdict.regressed);
//! # Ok(())
//! # }
//! ```

pub mod assertion;
pub mod compare;
pub mod dataset;
pub mod error;
pub mod evidence;
pub mod experiment;
pub mod judge;

pub use assertion::{Assertion, AssertionResult};
pub use compare::{
    compare, AssertionDelta, CaseChange, CaseDelta, CompareThresholds, ComparisonReport,
    LatencyDelta, Regression,
};
pub use dataset::{
    Dataset, EvalCase, Expectation, ExpectedToolCall, StatePredicate, DATASET_FORMAT_VERSION,
};
pub use error::{EvalError, Result};
pub use evidence::{RunEvidence, RunStatus, ToolCallRecord};
pub use experiment::{
    AssertionPassRate, CaseReport, CaseRunReport, ExperimentConfig, ExperimentReport,
    ExperimentRunner, LatencyStats, PreparedRun, ReportSummary, REPORT_FORMAT_VERSION,
};
pub use judge::{JudgeModel, JudgeRequest, JudgeVerdict, RuleBasedJudge};
