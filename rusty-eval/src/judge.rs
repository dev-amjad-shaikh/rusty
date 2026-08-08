//! The judge seam: scored evaluation beyond deterministic assertions.
//!
//! [`JudgeModel`] is the eval-plane analogue of the runtime's `ChatModel`:
//! one async method, structured request in, structured verdict out. Semantic
//! checks an assertion cannot express (answer quality, faithfulness,
//! tone) plug in here as LLM-backed implementations later; the trait is the
//! stable seam, and the experiment runner treats any judge uniformly.
//!
//! [`RuleBasedJudge`] is the deterministic implementation shipped now: it
//! scores a run by the fraction of the case's own expectations it met, so
//! the judge path is testable end to end without a live model — and so a
//! judge verdict never disagrees silently with the assertion ledger.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::dataset::Expectation;
use crate::error::Result;
use crate::evidence::RunEvidence;

/// One case run handed to a judge for scoring.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JudgeRequest {
    /// The case being judged.
    pub case_id: String,

    /// The case's input payload, as run.
    pub input: Value,

    /// The case's declared expectations. Rule-based judges evaluate them;
    /// model-backed judges use them as rubric context.
    pub expectations: Expectation,

    /// The run's distilled evidence.
    pub evidence: RunEvidence,
}

/// A judge's verdict on one case run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JudgeVerdict {
    /// Score in `0.0..=1.0`. Aggregation and thresholds treat it as an
    /// opaque quality scalar; each judge documents its own scale.
    pub score: f64,

    /// The judge's pass/fail decision at its configured threshold.
    pub passed: bool,

    /// Why, in one or two sentences. Written into reports verbatim, so it
    /// must stand alone as evidence.
    pub rationale: String,
}

/// The judge interface used by the experiment runner.
///
/// Mirrors `ChatModel`'s minimalism: implementors get the full case and
/// evidence, return a verdict. Judge failures are infrastructure errors
/// ([`crate::error::EvalError::Judge`]) and abort the experiment — a judge
/// that cannot answer must not fabricate a score.
#[async_trait]
pub trait JudgeModel: Send + Sync {
    /// Score one case run against its expectations.
    async fn judge(&self, request: &JudgeRequest) -> Result<JudgeVerdict>;
}

/// A deterministic judge: the score is the fraction of the case's
/// expectations met, zeroed for runs that did not complete.
///
/// A case with no expectations scores by completion alone (1.0 for a
/// finished run, 0.0 otherwise) — there is nothing to grade but finishing.
/// The verdict passes when the score reaches `pass_score` (default 1.0:
/// every expectation met on a completed run).
pub struct RuleBasedJudge {
    pass_score: f64,
}

impl RuleBasedJudge {
    /// A judge requiring a perfect score (all expectations met, run done).
    pub fn new() -> Self {
        Self { pass_score: 1.0 }
    }

    /// Override the passing threshold (clamped to `0.0..=1.0`).
    pub fn with_pass_score(mut self, pass_score: f64) -> Self {
        self.pass_score = pass_score.clamp(0.0, 1.0);
        self
    }
}

impl Default for RuleBasedJudge {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for RuleBasedJudge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuleBasedJudge")
            .field("pass_score", &self.pass_score)
            .finish()
    }
}

#[async_trait]
impl JudgeModel for RuleBasedJudge {
    async fn judge(&self, request: &JudgeRequest) -> Result<JudgeVerdict> {
        let results: Vec<_> = request
            .expectations
            .assertions()
            .iter()
            .map(|assertion| assertion.evaluate(&request.evidence))
            .collect();
        let met = results.iter().filter(|result| result.passed).count();
        let completion = if request.evidence.status.is_done() {
            1.0
        } else {
            0.0
        };
        let score = if results.is_empty() {
            completion
        } else {
            (met as f64 / results.len() as f64) * completion
        };

        let failed: Vec<&str> = results
            .iter()
            .filter(|result| !result.passed)
            .map(|result| result.assertion.as_str())
            .collect();
        let rationale = if results.is_empty() {
            format!("no expectations declared; run {}", request.evidence.status)
        } else if failed.is_empty() {
            format!(
                "{met}/{} expectations met; run {}",
                results.len(),
                request.evidence.status
            )
        } else {
            format!(
                "{met}/{} expectations met; failed: {}; run {}",
                results.len(),
                failed.join(", "),
                request.evidence.status
            )
        };

        Ok(JudgeVerdict {
            score,
            passed: score >= self.pass_score,
            rationale,
        })
    }
}
