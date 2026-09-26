//! The wave-4 release-gate composition (R0.12 Operations Plane): core's
//! [`RevisionGateEvaluator`] seam over `rusty-eval`'s public API — the
//! [`crate::learn::EvalCandidateEvaluator`] discipline turned toward
//! deployments.
//!
//! The split is the seam's: core owns the contract (what a gate must
//! answer, and that it must name the declaration it was handed back);
//! this crate owns the composition, because this is where the
//! `rusty-eval` dependency is allowed to live. Nothing about scoring,
//! aggregation, or check evaluation is re-implemented here — the policy
//! evaluates through [`evaluate_gate`], the experiments through
//! [`ExperimentRunner`] over the same versioned [`Dataset`] the candidate
//! plane uses, and a serving baseline adds the paired statistical check
//! the coarse thresholds cannot speak for.
//!
//! Fail-closed throughout, because a gate exists to refuse: an unknown
//! policy or dataset version is an error, not a pass; an evaluation the
//! runner cannot complete is an error; and the paired comparison appends
//! a `statistical_power` check that passes only on
//! [`StatisticalDecision::NoRegression`] — `InsufficientEvidence` fails
//! closed (a comparison that cannot speak cannot clear a gate), and
//! `Regression` fails because it *is* the finding the gate exists to
//! catch. A comparison that errors at all fails the same way.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use rusty_agent_runtime::deploy::{
    DeploymentRevision, GateCheckRecord, GateDeclaration, GateEvaluation, GateVerdict,
    RevisionGateEvaluator,
};
use rusty_agent_runtime::error::{Result as RuntimeResult, RustyError};
use rusty_agent_runtime::journal::Journal;
use rusty_eval::statistics::{
    detect_pass_rate_regression, StatisticalDecision, StatisticalRegressionConfig,
};
use rusty_eval::{evaluate_gate, EvalCase, ExperimentConfig, ExperimentRunner, GatePolicy};
use rusty_eval::{ExperimentReport, PreparedRun};
use serde_json::json;

use crate::learn::DatasetSource;

/// Gate composition failures use core's `invalid` convention — the same
/// error shape the learn module's adapters produce, so the control plane
/// distinguishes composition trouble from gate verdicts the way it
/// already does.
fn invalid(message: impl Into<String>) -> RustyError {
    RustyError::InvalidUpdate(message.into())
}

/// The application-owned half of a gate evaluation: how to build the
/// agent one revision runs as. The evaluator owns the evidence
/// discipline (the versioned dataset, the run counts, the policy, the
/// statistical config); the agent owns what baseline and candidate
/// revisions *are* as runnable graphs — the
/// [`crate::learn::EvaluationAgent`] split, one level up: the revision's
/// frozen pin set is what the agent applies.
#[async_trait]
pub trait RevisionEvaluationAgent: Send + Sync + std::fmt::Debug {
    /// Build the prepared run for one case repetition under `revision`.
    /// `journal` is the run's Flight Recorder journal — wire it into
    /// recording graphs so model and tool calls become assertion
    /// evidence.
    fn prepare(
        &self,
        case: &EvalCase,
        journal: &Journal,
        revision: &DeploymentRevision,
    ) -> RuntimeResult<PreparedRun>;
}

/// Where named gate policies come from. A declaration names a policy;
/// the source resolves it. Policies are immutable per name — re-running
/// a gate must mean re-reading the same rule, so a source that edits in
/// place breaks the audit the gate journals.
pub trait GatePolicySource: Send + Sync + std::fmt::Debug {
    /// Load policy `name` (`Err` when unknown).
    fn load(&self, name: &str) -> Result<GatePolicy, String>;
}

/// Gate policies as JSON files under one directory, named
/// `{policy_name}.json` — `rusty-eval`'s canonical serialization
/// ([`GatePolicy::to_json`]), so policies diff cleanly in git and the
/// file layout is the naming.
#[derive(Debug)]
pub struct DirectoryGatePolicySource {
    root: PathBuf,
}

impl DirectoryGatePolicySource {
    /// A source serving `{root}/{policy_name}.json`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl GatePolicySource for DirectoryGatePolicySource {
    fn load(&self, name: &str) -> Result<GatePolicy, String> {
        let text = std::fs::read_to_string(self.root.join(format!("{name}.json")))
            .map_err(|e| format!("policy `{name}`: {e}"))?;
        GatePolicy::from_json(&text).map_err(|e| e.to_string())
    }
}

/// The wave-4 gate composition: core's [`RevisionGateEvaluator`] over
/// `rusty-eval`'s public API. The candidate experiment runs under the
/// revision being promoted; the baseline experiment runs under the
/// environment's serving revision when one serves, giving the gate's
/// comparative checks their evidence and arming the paired statistical
/// check.
#[derive(Debug)]
pub struct EvalRevisionGateEvaluator {
    datasets: Arc<dyn DatasetSource>,
    policies: Arc<dyn GatePolicySource>,
    agent: Arc<dyn RevisionEvaluationAgent>,
    runs_per_case: usize,
    statistical: StatisticalRegressionConfig,
}

impl EvalRevisionGateEvaluator {
    /// An evaluator reading policies through `policies`, datasets through
    /// `datasets`, building agents through `agent`, running each case
    /// `runs_per_case` times (normalized to at least one), and judging
    /// the paired comparison under `statistical`.
    pub fn new(
        datasets: Arc<dyn DatasetSource>,
        policies: Arc<dyn GatePolicySource>,
        agent: Arc<dyn RevisionEvaluationAgent>,
        runs_per_case: usize,
        statistical: StatisticalRegressionConfig,
    ) -> Self {
        Self {
            datasets,
            policies,
            agent,
            runs_per_case: runs_per_case.max(1),
            statistical,
        }
    }

    async fn run_experiment(
        &self,
        dataset: &rusty_eval::Dataset,
        revision: &DeploymentRevision,
    ) -> RuntimeResult<ExperimentReport> {
        let runner =
            ExperimentRunner::new(ExperimentConfig::new().with_runs_per_case(self.runs_per_case));
        let agent = self.agent.clone();
        let revision_id = revision.revision_id.to_string();
        let revision = revision.clone();
        runner
            .run(dataset, move |case, journal| {
                agent.prepare(case, journal, &revision)
            })
            .await
            .map_err(|e| invalid(format!("experiment under revision `{revision_id}`: {e}")))
    }
}

#[async_trait]
impl RevisionGateEvaluator for EvalRevisionGateEvaluator {
    async fn evaluate(
        &self,
        revision: &DeploymentRevision,
        baseline: Option<&DeploymentRevision>,
        gate: &GateDeclaration,
    ) -> RuntimeResult<GateEvaluation> {
        let policy = self
            .policies
            .load(&gate.policy)
            .map_err(|e| invalid(format!("gate policy: {e}")))?;
        let dataset = self
            .datasets
            .load(&gate.dataset_version)
            .map_err(|e| invalid(format!("gate dataset: {e}")))?;

        let candidate_report = self.run_experiment(&dataset, revision).await?;
        let baseline_report = match baseline {
            Some(baseline) => Some(self.run_experiment(&dataset, baseline).await?),
            None => None,
        };

        let decision = evaluate_gate(&policy, &candidate_report, baseline_report.as_ref())
            .map_err(|e| invalid(format!("gate evaluation: {e}")))?;
        let mut checks: Vec<GateCheckRecord> = decision
            .checks()
            .iter()
            .map(|check| GateCheckRecord {
                // The eval metric's own serde form, carried as a string:
                // a new eval metric needs no core release to journal.
                metric: serde_json::to_string(&check.metric)
                    .unwrap_or_else(|_| "\"unserializable\"".to_owned()),
                passed: check.passed,
                observed: check.observed.clone(),
                required: check.required.clone(),
                detail: check.detail.clone(),
            })
            .collect();

        // The paired check the coarse thresholds cannot speak for.
        // Passes only on `NoRegression`: `InsufficientEvidence` fails
        // closed (a comparison that cannot speak cannot clear a gate),
        // `Regression` fails because it is the finding the gate exists
        // to catch, and a comparison that errors at all fails the same
        // way — an unevaluable comparison is not evidence of safety.
        if let Some(baseline_report) = &baseline_report {
            let (passed, observed, detail) = match detect_pass_rate_regression(
                baseline_report,
                &candidate_report,
                &self.statistical,
            ) {
                Ok(report) => {
                    let passed = report.decision == StatisticalDecision::NoRegression;
                    (
                        passed,
                        json!({
                            "decision": report.decision,
                            "pairs": report.pairs,
                            "pass_rate_drop": report.pass_rate_drop,
                        }),
                        format!(
                            "paired comparison over {} matched runs decided {:?}",
                            report.pairs, report.decision
                        ),
                    )
                }
                Err(e) => (
                    false,
                    json!({ "error": e.to_string() }),
                    format!(
                        "the paired comparison could not be evaluated ({e}) — an \
                             unevaluable comparison fails closed"
                    ),
                ),
            };
            checks.push(GateCheckRecord {
                metric: "\"statistical_power\"".to_owned(),
                passed,
                observed,
                required: json!({ "minimum_pairs": self.statistical.minimum_pairs }),
                detail,
            });
        }

        let outcome = if checks.iter().all(|check| check.passed) {
            GateVerdict::Allow
        } else {
            GateVerdict::Block
        };
        Ok(GateEvaluation {
            policy: gate.policy.clone(),
            dataset_version: gate.dataset_version.clone(),
            outcome,
            checks,
        })
    }
}
