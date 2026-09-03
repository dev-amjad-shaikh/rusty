//! # Rusty Eval
//!
//! Rusty Eval is the foundation of Rusty's Agent TestOps plane: it turns the
//! Flight Recorder's run evidence into release decisions. Composable pieces,
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
//!   detail, latency percentiles, total cost. Runs are sequential by default
//!   or bounded-parallel with deterministic report ordering.
//! - **Comparison** ([`mod@compare`]) — [`compare::compare`] diffs a candidate
//!   report against a baseline: per-assertion deltas, per-case regressions
//!   and improvements, and threshold-based regression flags (pass-rate drop,
//!   p95 latency growth).
//! - **Failure clustering** ([`clustering`]) — deterministic signatures group
//!   failed runs by termination, assertions, and judge outcome, preserving
//!   source evidence under stable cluster ids.
//! - **Judges** ([`judge`]) — [`judge::RuleBasedJudge`] scores deterministic
//!   expectations, while [`judge::ModelJudge`] adapts any runtime `ChatModel`
//!   into a strict structured evaluator with local threshold enforcement.
//! - **Human feedback** ([`feedback`]) — deterministic pairwise annotation
//!   queues with reviewer leases, rubric validation, consensus resolution,
//!   disagreement adjudication, corrections, and promotion into versioned
//!   evaluation cases.
//! - **Release gates** ([`gate`]) — versioned policies turn candidate reports
//!   and baseline comparisons into deterministic allow/block decisions with
//!   machine-readable evidence for every configured check.
//! - **Span trees** ([`trace`]) — [`trace::SpanTree`] distills a run's Flight
//!   Recorder journal into a queryable execution tree (`rusty.run` →
//!   `rusty.super_step` → `rusty.node` → leaf calls), with ordering and
//!   ancestry derived from journal positions, never wall-clock races.
//! - **Span queries** ([`span_query`]) — serializable structural assertions
//!   over the span tree: selection by name and attribute predicates,
//!   existence/count/ordering/ancestry/concurrency/budget constraints, a
//!   versioned attribute vocabulary validated at authoring time, and failure
//!   reports that diagnose the miss without opening the raw trace.
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
pub mod clustering;
pub mod compare;
pub mod dataset;
pub mod error;
pub mod evidence;
pub mod experiment;
pub mod feedback;
pub mod gate;
pub mod judge;
pub mod online_scoring;
pub mod span_query;
pub mod statistics;
pub mod trace;

pub use assertion::{Assertion, AssertionResult};
pub use clustering::{
    AssertionFailureKey, ExecutionFailureCategory, FAILURE_CLUSTER_REPORT_FORMAT_VERSION,
    FailureCause, FailureCluster, FailureClusterReport, FailureEvidenceRef, FailureOccurrence,
    FailureSignature, FailureTermination, cluster_failures,
};
pub use compare::{
    AssertionDelta, CaseChange, CaseDelta, CompareThresholds, ComparisonReport, LatencyDelta,
    Regression, compare,
};
pub use dataset::{
    DATASET_FORMAT_VERSION, Dataset, EvalCase, Expectation, ExpectedToolCall, StatePredicate,
};
pub use error::{EvalError, Result};
pub use evidence::{RunEvidence, RunStatus, ToolCallRecord};
pub use experiment::{
    AssertionPassRate, CaseReport, CaseRunReport, ExperimentConfig, ExperimentReport,
    ExperimentRunner, LatencyStats, PreparedRun, REPORT_FORMAT_VERSION, ReportSummary,
};
pub use feedback::{
    AnnotationQueue, AnnotationStatus, AnnotationTask, FEEDBACK_FORMAT_VERSION,
    ResolutionAuthority, ReviewCandidate, ReviewDecision, ReviewLease, ReviewResolution,
    ReviewRubric, ReviewSubmission, RubricCriterion, StoredReview, TraceRef,
};
pub use gate::{
    GATE_DECISION_FORMAT_VERSION, GATE_POLICY_FORMAT_VERSION, GateCheck, GateDecision, GateMetric,
    GateOutcome, GatePolicy, evaluate_gate,
};
pub use judge::{
    DEFAULT_MODEL_JUDGE_MAX_REQUEST_BYTES, DEFAULT_MODEL_JUDGE_MAX_RESPONSE_BYTES,
    DEFAULT_MODEL_JUDGE_PASS_SCORE, JudgeModel, JudgeRequest, JudgeVerdict,
    MAX_MODEL_JUDGE_RATIONALE_BYTES, ModelJudge, RuleBasedJudge,
};
pub use online_scoring::{
    BudgetTracker, InMemoryBudgetTracker, OUTCOME_ANNOTATION_FORMAT_VERSION, OnlineScoringPolicy,
    OnlineScoringRunner, OutcomeAnnotation, SamplingDecision, ScorerBinding, ScorerOutcome,
    ScorerRegistry, ScoringTask,
};
pub use span_query::{
    AttributeKind, AttributePredicate, PredicateOp, QueryFailure, QueryVerdict, SpanConstraint,
    SpanQuery, SpanSelection, SpanSummary, SPAN_VOCABULARY, SPAN_VOCABULARY_VERSION,
    VocabularyEntry, evaluate_all, evaluate_query,
};
pub use trace::{AttributeValue, SPAN_NAMES, SpanTree, TraceSpan};
pub use statistics::{
    STATISTICAL_REGRESSION_FORMAT_VERSION, StatisticalDecision, StatisticalRegressionConfig,
    StatisticalRegressionReport, detect_pass_rate_regression,
};
