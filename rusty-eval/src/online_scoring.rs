//! Online scoring: async production hooks on sampled live traffic.
//!
//! [`OnlineScoringPolicy`] configures which turns to sample, which scorers to
//! run, and how much judge-call budget is available per day.  The
//! [`OnlineScoringRunner`] executes scoring tasks off the latency path,
//! re-using the same [`JudgeModel`] seam that offline experiments use, and
//! produces [`OutcomeAnnotation`] rows that join to turn stamps for
//! per-intent quality curves.
//!
//! ## Quick sketch
//!
//! ```no_run
//! use rusty_eval::online_scoring::{OnlineScoringPolicy, OnlineScoringRunner, SamplingDecision, ScorerRegistry};
//!
//! # fn demo() {
//! let policy = OnlineScoringPolicy::new("tenant-1", "blueprint-a", 0.1).unwrap();
//! let decision = policy.decide("turn-42");
//! # }
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::dataset::Expectation;
use crate::error::{EvalError, Result};
use crate::evidence::RunEvidence;
use crate::judge::{JudgeModel, JudgeRequest};

/// Format version written into [`OutcomeAnnotation`] records.
pub const OUTCOME_ANNOTATION_FORMAT_VERSION: u64 = 1;

/// Per-tenant, per-blueprint policy for online scoring.
///
/// Configured by an operator and persisted by the server layer; the eval
/// crate treats it as plain data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OnlineScoringPolicy {
    /// Owning tenant.
    pub tenant_id: String,

    /// Target blueprint.
    pub blueprint_id: String,

    /// Fraction of completed turns to sample, in `0.0..=1.0`.
    pub sampling_rate: f64,

    /// Maximum judge (LLM) calls allowed per calendar day.  When exhausted
    /// the runner degrades to code-only scorers and records the fact.
    pub daily_judge_budget: u64,

    /// Scorers to run on every sampled turn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scorer_bindings: Vec<ScorerBinding>,
}

impl OnlineScoringPolicy {
    /// Create a minimal policy with the given identity and sampling rate.
    pub fn new(
        tenant_id: impl Into<String>,
        blueprint_id: impl Into<String>,
        sampling_rate: f64,
    ) -> Result<Self> {
        if !(0.0..=1.0).contains(&sampling_rate) {
            return Err(EvalError::Dataset(format!(
                "sampling_rate must be in 0.0..=1.0, got {sampling_rate}"
            )));
        }
        Ok(Self {
            tenant_id: tenant_id.into(),
            blueprint_id: blueprint_id.into(),
            sampling_rate,
            daily_judge_budget: 0,
            scorer_bindings: Vec::new(),
        })
    }

    /// Set the daily judge-call budget.
    pub fn with_daily_judge_budget(mut self, budget: u64) -> Self {
        self.daily_judge_budget = budget;
        self
    }

    /// Attach scorer bindings.
    pub fn with_scorer_bindings(mut self, bindings: Vec<ScorerBinding>) -> Self {
        self.scorer_bindings = bindings;
        self
    }

    /// Decide whether a given turn should be sampled.
    ///
    /// The decision is deterministic for a given `(policy, turn_id)` pair:
    /// hashing the turn id and comparing against the rate gives a stable,
    /// testable outcome without storing per-turn state.
    pub fn decide(&self, turn_id: &str) -> SamplingDecision {
        if self.sampling_rate <= 0.0 {
            return SamplingDecision::Skipped {
                reason: "sampling_rate is zero".to_owned(),
            };
        }
        if self.sampling_rate >= 1.0 {
            return SamplingDecision::Sampled;
        }
        // Deterministic sampling via simple hash of (tenant, blueprint, turn).
        let mut hash_input = String::with_capacity(
            self.tenant_id.len() + self.blueprint_id.len() + turn_id.len() + 2,
        );
        hash_input.push_str(&self.tenant_id);
        hash_input.push('\0');
        hash_input.push_str(&self.blueprint_id);
        hash_input.push('\0');
        hash_input.push_str(turn_id);
        let hash = fxhash::hash64(&hash_input);
        let normalized = (hash as f64) / (u64::MAX as f64);
        if normalized < self.sampling_rate {
            SamplingDecision::Sampled
        } else {
            SamplingDecision::Skipped {
                reason: "turn hash above sampling_rate".to_owned(),
            }
        }
    }
}

/// Whether a completed turn is sampled for online scoring.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum SamplingDecision {
    /// The turn will be scored asynchronously.
    Sampled,
    /// The turn is not sampled.
    Skipped { reason: String },
}

/// One scorer bound to a policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScorerBinding {
    /// Scorer name — keys into the [`ScorerRegistry`].
    pub name: String,
    /// Scorer version — recorded on every [`ScorerOutcome`].
    pub version: String,
    /// Passing threshold in `0.0..=1.0`.
    pub threshold: f64,
    /// `true` when this scorer requires an LLM judge call (counts against
    /// the daily budget).  Code-only scorers are always run regardless of
    /// budget state.
    pub requires_judge: bool,
}

impl ScorerBinding {
    /// Validate invariants, returning `Err` on bad configuration.
    pub fn validate(&self) -> Result<()> {
        if self.name.is_empty() {
            return Err(EvalError::Dataset(
                "scorer binding name must not be empty".to_owned(),
            ));
        }
        if !(0.0..=1.0).contains(&self.threshold) {
            return Err(EvalError::Dataset(format!(
                "scorer `{}` threshold must be in 0.0..=1.0, got {}",
                self.name, self.threshold
            )));
        }
        Ok(())
    }
}

/// A registry of named scorers available for online scoring.
///
/// The server layer populates this from configured scorer instances; the
/// eval crate only needs lookup by name.
pub struct ScorerRegistry {
    scorers: HashMap<String, Arc<dyn JudgeModel>>,
}

impl ScorerRegistry {
    /// Empty registry.
    pub fn new() -> Self {
        Self {
            scorers: HashMap::new(),
        }
    }

    /// Register a scorer under `name`.  Overwrites any existing entry.
    pub fn register(&mut self, name: impl Into<String>, scorer: Arc<dyn JudgeModel>) {
        self.scorers.insert(name.into(), scorer);
    }

    /// Look up a scorer by name.
    pub fn get(&self, name: &str) -> Option<Arc<dyn JudgeModel>> {
        self.scorers.get(name).cloned()
    }

    /// Number of registered scorers.
    pub fn len(&self) -> usize {
        self.scorers.len()
    }

    /// `true` when no scorers are registered.
    pub fn is_empty(&self) -> bool {
        self.scorers.is_empty()
    }
}

impl Default for ScorerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ScorerRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScorerRegistry")
            .field("names", &self.scorers.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Tracks judge-call budget consumption.
///
/// The default in-memory tracker is suitable for unit tests; the server
/// layer should provide a persistent implementation backed by a counter
/// store.
#[async_trait]
pub trait BudgetTracker: Send + Sync {
    /// Increment the judge-call count for `(tenant_id, blueprint_id)` and
    /// return the new total for today.
    async fn record_judge_call(&self, tenant_id: &str, blueprint_id: &str) -> Result<u64>;

    /// Current judge-call count for today, without incrementing.
    async fn current_count(&self, tenant_id: &str, blueprint_id: &str) -> Result<u64>;
}

/// In-memory budget tracker for testing.
pub struct InMemoryBudgetTracker {
    counts: std::sync::Mutex<HashMap<(String, String), u64>>,
}

impl InMemoryBudgetTracker {
    /// Create a fresh tracker with all counts at zero.
    pub fn new() -> Self {
        Self {
            counts: std::sync::Mutex::new(HashMap::new()),
        }
    }
}

impl Default for InMemoryBudgetTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl BudgetTracker for InMemoryBudgetTracker {
    async fn record_judge_call(&self, tenant_id: &str, blueprint_id: &str) -> Result<u64> {
        let mut counts = self.counts.lock().unwrap();
        let key = (tenant_id.to_owned(), blueprint_id.to_owned());
        let entry = counts.entry(key).or_insert(0);
        *entry += 1;
        Ok(*entry)
    }

    async fn current_count(&self, tenant_id: &str, blueprint_id: &str) -> Result<u64> {
        let counts = self.counts.lock().unwrap();
        let key = (tenant_id.to_owned(), blueprint_id.to_owned());
        Ok(counts.get(&key).copied().unwrap_or(0))
    }
}

/// One async scoring task for a completed turn.
#[derive(Debug, Clone)]
pub struct ScoringTask {
    /// Turn stamp the task scores.
    pub turn_stamp_id: String,
    /// Parent session.
    pub session_id: String,
    /// Owning tenant.
    pub tenant_id: String,
    /// Blueprint under which the turn ran.
    pub blueprint_id: String,
    /// Distilled evidence of the turn.
    pub evidence: RunEvidence,
    /// Intent attribution, when the intent map resolves one.
    pub intent_id: Option<String>,
    /// Scorers to run, drawn from the policy.
    pub bindings: Vec<ScorerBinding>,
}

/// The outcome of scoring one turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutcomeAnnotation {
    /// Schema version.
    pub format_version: u64,

    /// Turn stamp this outcome annotates.
    pub turn_stamp_id: String,

    /// Parent session.
    pub session_id: String,

    /// Owning tenant.
    pub tenant_id: String,

    /// Blueprint under which the turn ran.
    pub blueprint_id: String,

    /// Intent attribution, when resolvable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent_id: Option<String>,

    /// Per-scorer results.
    pub scores: Vec<ScorerOutcome>,

    /// `true` when the runner degraded to code-only scorers because the
    /// daily judge budget was exhausted.
    pub degraded: bool,

    /// Traffic classification — always `"side"` for online scoring.
    pub traffic: String,
}

/// One scorer's result on one turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScorerOutcome {
    /// Scorer name.
    pub name: String,
    /// Scorer version.
    pub version: String,
    /// Score in `0.0..=1.0`.
    pub score: f64,
    /// Pass/fail at the policy threshold.
    pub passed: bool,
    /// Human-readable justification.
    pub rationale: String,
}

/// Executes scoring tasks asynchronously, off the latency path.
///
/// The runner is cheap to construct and clone (it holds only `Arc`s), so
/// the scheduler can keep one instance and feed it tasks from a queue.
pub struct OnlineScoringRunner {
    registry: Arc<ScorerRegistry>,
    budget: Arc<dyn BudgetTracker>,
}

impl OnlineScoringRunner {
    /// Create a runner with the given scorer registry and budget tracker.
    pub fn new(registry: Arc<ScorerRegistry>, budget: Arc<dyn BudgetTracker>) -> Self {
        Self { registry, budget }
    }

    /// Run one scoring task, producing an [`OutcomeAnnotation`].
    ///
    /// For each binding:
    /// - If `requires_judge` is `true`, the runner checks budget.  If budget
    ///   is exhausted, the scorer is skipped and `degraded` is set to `true`.
    /// - The scorer is looked up in the registry.  Missing scorers produce
    ///   a failed outcome with a diagnostic rationale.
    /// - The scorer receives a [`JudgeRequest`] built from the turn evidence.
    pub async fn run(
        &self,
        task: &ScoringTask,
        policy: &OnlineScoringPolicy,
    ) -> Result<OutcomeAnnotation> {
        let mut scores = Vec::with_capacity(task.bindings.len());
        let mut degraded = false;

        for binding in &task.bindings {
            binding.validate()?;

            if binding.requires_judge {
                let current = self
                    .budget
                    .current_count(&task.tenant_id, &task.blueprint_id)
                    .await?;
                if current >= policy.daily_judge_budget {
                    degraded = true;
                    continue; // skip judge scorers when budget exhausted
                }
            }

            let outcome = match self.registry.get(&binding.name) {
                Some(scorer) => {
                    if binding.requires_judge {
                        // Record the consumption *before* the call so that
                        // a concurrent task also sees budget pressure.
                        self.budget
                            .record_judge_call(&task.tenant_id, &task.blueprint_id)
                            .await?;
                    }
                    let request = JudgeRequest {
                        case_id: task.turn_stamp_id.clone(),
                        input: task.evidence.final_state.clone(),
                        expectations: Expectation::default(),
                        evidence: task.evidence.clone(),
                    };
                    match scorer.judge(&request).await {
                        Ok(verdict) => ScorerOutcome {
                            name: binding.name.clone(),
                            version: binding.version.clone(),
                            score: verdict.score,
                            passed: verdict.passed && verdict.score >= binding.threshold,
                            rationale: verdict.rationale,
                        },
                        Err(error) => ScorerOutcome {
                            name: binding.name.clone(),
                            version: binding.version.clone(),
                            score: 0.0,
                            passed: false,
                            rationale: format!("scorer error: {error}"),
                        },
                    }
                }
                None => ScorerOutcome {
                    name: binding.name.clone(),
                    version: binding.version.clone(),
                    score: 0.0,
                    passed: false,
                    rationale: format!("scorer `{}` not found in registry", binding.name),
                },
            };
            scores.push(outcome);
        }

        Ok(OutcomeAnnotation {
            format_version: OUTCOME_ANNOTATION_FORMAT_VERSION,
            turn_stamp_id: task.turn_stamp_id.clone(),
            session_id: task.session_id.clone(),
            tenant_id: task.tenant_id.clone(),
            blueprint_id: task.blueprint_id.clone(),
            intent_id: task.intent_id.clone(),
            scores,
            degraded,
            traffic: "side".to_owned(),
        })
    }
}

impl std::fmt::Debug for OnlineScoringRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OnlineScoringRunner")
            .field("registry", &self.registry)
            .finish_non_exhaustive()
    }
}

/// Simple 64-bit hash for deterministic sampling.
mod fxhash {
    pub fn hash64(data: &str) -> u64 {
        let mut state = 0xcbf29ce484222325u64; // FNV-1a offset basis
        for byte in data.bytes() {
            state ^= byte as u64;
            state = state.wrapping_mul(0x100000001b3); // FNV prime
        }
        state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_sampling_rate_validation() {
        assert!(OnlineScoringPolicy::new("t", "b", -0.1).is_err());
        assert!(OnlineScoringPolicy::new("t", "b", 1.1).is_err());
        assert!(OnlineScoringPolicy::new("t", "b", 0.5).is_ok());
        assert!(OnlineScoringPolicy::new("t", "b", 0.0).is_ok());
        assert!(OnlineScoringPolicy::new("t", "b", 1.0).is_ok());
    }

    #[test]
    fn deterministic_sampling() {
        let policy = OnlineScoringPolicy::new("tenant-a", "bp-1", 0.5).unwrap();
        // Same turn id → same decision every time.
        let d1 = policy.decide("turn-123");
        let d2 = policy.decide("turn-123");
        assert_eq!(d1, d2);

        // Different tenant or blueprint → different hash, may differ.
        let policy2 = OnlineScoringPolicy::new("tenant-b", "bp-1", 0.5).unwrap();
        let d3 = policy2.decide("turn-123");
        // Not guaranteed to differ, but extremely likely.
        // We only assert that the decision is stable.
        let d4 = policy2.decide("turn-123");
        assert_eq!(d3, d4);
    }

    #[test]
    fn zero_rate_skips_everything() {
        let policy = OnlineScoringPolicy::new("t", "b", 0.0).unwrap();
        assert!(
            matches!(policy.decide("any"), SamplingDecision::Skipped { .. }),
            "zero rate should skip"
        );
    }

    #[test]
    fn full_rate_samples_everything() {
        let policy = OnlineScoringPolicy::new("t", "b", 1.0).unwrap();
        assert_eq!(policy.decide("any"), SamplingDecision::Sampled);
    }

    #[test]
    fn binding_validation() {
        let ok = ScorerBinding {
            name: "quality".to_owned(),
            version: "1".to_owned(),
            threshold: 0.8,
            requires_judge: true,
        };
        assert!(ok.validate().is_ok());

        let bad_threshold = ScorerBinding {
            name: "quality".to_owned(),
            version: "1".to_owned(),
            threshold: 1.5,
            requires_judge: false,
        };
        assert!(bad_threshold.validate().is_err());

        let empty_name = ScorerBinding {
            name: "".to_owned(),
            version: "1".to_owned(),
            threshold: 0.5,
            requires_judge: false,
        };
        assert!(empty_name.validate().is_err());
    }

    #[test]
    fn outcome_annotation_serde_roundtrip() {
        let annotation = OutcomeAnnotation {
            format_version: OUTCOME_ANNOTATION_FORMAT_VERSION,
            turn_stamp_id: "stamp-1".to_owned(),
            session_id: "sess-1".to_owned(),
            tenant_id: "t".to_owned(),
            blueprint_id: "b".to_owned(),
            intent_id: Some("intent-x".to_owned()),
            scores: vec![ScorerOutcome {
                name: "quality".to_owned(),
                version: "1.0.0".to_owned(),
                score: 0.85,
                passed: true,
                rationale: "good".to_owned(),
            }],
            degraded: false,
            traffic: "side".to_owned(),
        };
        let json = serde_json::to_string(&annotation).unwrap();
        let back: OutcomeAnnotation = serde_json::from_str(&json).unwrap();
        assert_eq!(annotation, back);
    }

    #[tokio::test]
    async fn runner_with_missing_scorer_produces_failure_outcome() {
        let registry = Arc::new(ScorerRegistry::new());
        let budget = Arc::new(InMemoryBudgetTracker::new());
        let runner = OnlineScoringRunner::new(registry, budget);

        let task = ScoringTask {
            turn_stamp_id: "turn-1".to_owned(),
            session_id: "sess-1".to_owned(),
            tenant_id: "tenant-a".to_owned(),
            blueprint_id: "bp-1".to_owned(),
            evidence: RunEvidence {
                status: crate::evidence::RunStatus::Done,
                tool_calls: vec![],
                final_state: serde_json::Value::Null,
                latency_ms: 100,
                cost_usd: 0.0,
                total_tokens: 0,
            },
            intent_id: None,
            bindings: vec![ScorerBinding {
                name: "missing".to_owned(),
                version: "1".to_owned(),
                threshold: 0.8,
                requires_judge: false,
            }],
        };

        let policy = OnlineScoringPolicy::new("tenant-a", "bp-1", 1.0)
            .unwrap()
            .with_daily_judge_budget(10);

        let outcome = runner.run(&task, &policy).await.unwrap();
        assert_eq!(outcome.scores.len(), 1);
        assert!(!outcome.scores[0].passed);
        assert!(outcome.scores[0].rationale.contains("not found"));
        assert_eq!(outcome.traffic, "side");
    }

    #[tokio::test]
    async fn budget_exhaustion_skips_judge_scorers() {
        let registry = Arc::new(ScorerRegistry::new());
        let budget = Arc::new(InMemoryBudgetTracker::new());
        let runner = OnlineScoringRunner::new(registry, budget.clone());

        // Pre-consume the budget.
        for _ in 0..5 {
            budget.record_judge_call("tenant-a", "bp-1").await.unwrap();
        }

        let task = ScoringTask {
            turn_stamp_id: "turn-1".to_owned(),
            session_id: "sess-1".to_owned(),
            tenant_id: "tenant-a".to_owned(),
            blueprint_id: "bp-1".to_owned(),
            evidence: RunEvidence {
                status: crate::evidence::RunStatus::Done,
                tool_calls: vec![],
                final_state: serde_json::Value::Null,
                latency_ms: 100,
                cost_usd: 0.0,
                total_tokens: 0,
            },
            intent_id: None,
            bindings: vec![ScorerBinding {
                name: "expensive".to_owned(),
                version: "1".to_owned(),
                threshold: 0.8,
                requires_judge: true,
            }],
        };

        let policy = OnlineScoringPolicy::new("tenant-a", "bp-1", 1.0)
            .unwrap()
            .with_daily_judge_budget(5);

        let outcome = runner.run(&task, &policy).await.unwrap();
        assert!(outcome.degraded);
        assert!(outcome.scores.is_empty());
    }
}
