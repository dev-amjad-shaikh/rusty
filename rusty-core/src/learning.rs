//! The blueprint's learning policy (EP-08-S07): which background loops run,
//! on what cadence, under what budgets, promoting through which gates —
//! declared on the immutable blueprint version, resolved from the session's
//! pin, and enforced by the platform so background autonomy is configuration
//! the operator owns, not emergent behavior they discover.
//!
//! The policy is the deployment home the EP-07 loops were built against:
//! the hunting loop's per-cycle budget (EP-07-S10), the frontier cycle's
//! probe budget (EP-07-S11, [`crate::frontier::ExpansionPolicy`]), the
//! promotion gate's eval-suite reference and approval scope (EP-07-S10 ACs
//! 4–5), the review-fork switch (EP-07-S04), and the consolidation cadence
//! (EP-06-S08, [`crate::memory::ConsolidationCadence`]) all read this one
//! declaration — exclusively, so two loops never disagree about what the
//! agent may do in the background.
//!
//! Declaration lives on the assistant version's `config.learning_policy`
//! object (the server's blueprint analogue); the version's content-derived
//! id covers it, so a policy change is a new version by construction and
//! running sessions adopt it only at a safe boundary (EP-08-S08). A version
//! that declares no policy resolves to [`LearningPolicy::floor`] — the exact
//! behavior every pre-policy deployment already had.

use serde::{Deserialize, Serialize};

use crate::memory::ConsolidationCadence;

/// The platform ceiling on one hunting cycle's picks, mirrored from the
/// server's hunt-cycle bound: a misconfigured budget must never drain the
/// queue in one call. Declared budgets above the ceiling fail validation.
pub const MAX_HUNTS_PER_CYCLE: u32 = 64;

/// The floor hunting budget: what a version that declares no policy may
/// spend per cycle — the platform ceiling, so an undeclared agent behaves
/// exactly as it did before policies existed (the caller's own bound still
/// applies within it).
pub const FLOOR_MAX_HUNTS_PER_CYCLE: u32 = MAX_HUNTS_PER_CYCLE;

/// The floor probe budget, matching [`crate::frontier::ExpansionPolicy`]'s
/// default — the frontier cycle's behavior before a policy governed it.
pub const FLOOR_MAX_PROBES_PER_CYCLE: u32 = 10;

/// The hunting loop's per-cycle spend (EP-07-S10 AC 1, EP-08-S07 AC 2):
/// how many gaps one cycle may hunt and how many probe observations one
/// frontier expansion may record. Enforced exactly as declared — never
/// silently exceeded; a cycle that exhausts its budget with work remaining
/// stops with the stoppage logged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HuntingBudget {
    /// At most this many gaps move `Open → Hunting` in one cycle.
    pub max_hunts_per_cycle: u32,
    /// At most this many probe observations one frontier cycle records.
    pub max_probes_per_cycle: u32,
}

impl Default for HuntingBudget {
    fn default() -> Self {
        Self {
            max_hunts_per_cycle: FLOOR_MAX_HUNTS_PER_CYCLE,
            max_probes_per_cycle: FLOOR_MAX_PROBES_PER_CYCLE,
        }
    }
}

/// The promotion gate (EP-07-S10 ACs 4–5, EP-08-S07 AC 3): which eval suite
/// a skill promotion must pass and, when behavior changes are consequential,
/// the operator scope whose approval the promotion demands.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromotionGate {
    /// The eval suite reference governing promotions. `Some` overrides the
    /// skill's own `eval_gate` declaration — the blueprint, not the
    /// artifact, decides what evidence admits a behavior change. A
    /// reference that no longer resolves fails the promotion loudly; no
    /// code path promotes ungated. `None` leaves the skill's own
    /// declaration in force (the pre-policy behavior).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eval_suite_ref: Option<String>,
    /// The operator scope an approval must come from when the policy
    /// demands one (EP-07-S10 AC 5's "operator scope defined in the
    /// blueprint's promotion gate"). `Some` refuses any promotion whose
    /// approval token was not minted by this scope; `None` demands no
    /// policy-level approval.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_scope: Option<String>,
}

/// One blueprint version's learning declaration. Every field defaults, so a
/// declaration may name only what it governs; an absent policy object
/// resolves to the floor outright.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LearningPolicy {
    /// Whether the post-turn background review fork (EP-07-S04) may fire
    /// for this agent. `false` means no review fork ever fires. The floor
    /// is `true`: gating a loop that has not yet landed must not change
    /// today's behavior when it does.
    #[serde(default = "floor_review_fork_enabled")]
    pub review_fork_enabled: bool,
    /// The cadence that alone drives EP-06 consolidation scheduling
    /// (EP-08-S07 AC 1): the scheduler fires a pass only when this cadence
    /// says one is due. `None` — the floor — means consolidation is never
    /// scheduler-fired, matching every deployment before policies existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consolidation_cadence: Option<ConsolidationCadence>,
    /// The gate every skill promotion for this agent passes through.
    #[serde(default)]
    pub skill_promotion: PromotionGate,
    /// The hunting loop's and frontier cycle's per-cycle budgets.
    #[serde(default)]
    pub hunting_budget: HuntingBudget,
}

/// The floor's review-fork switch: on, so undeclared agents keep their
/// pre-policy behavior when the fork lands.
fn floor_review_fork_enabled() -> bool {
    true
}

impl Default for LearningPolicy {
    fn default() -> Self {
        Self::floor()
    }
}

impl LearningPolicy {
    /// The behavior every pre-policy deployment already had: the review
    /// fork ungated, consolidation never scheduler-fired, promotions
    /// governed by each skill's own declaration, and the platform ceilings
    /// as the only budget bound.
    pub fn floor() -> Self {
        Self {
            review_fork_enabled: floor_review_fork_enabled(),
            consolidation_cadence: None,
            skill_promotion: PromotionGate::default(),
            hunting_budget: HuntingBudget::default(),
        }
    }

    /// The policy's honesty rules, every violation named (the
    /// `ExpansionPolicy::validate` precedent — a cycle configured to do
    /// nothing is a configuration error, not a quiet no-op).
    pub fn validate(&self) -> Result<(), LearningPolicyError> {
        let mut violations = Vec::new();
        let budget = &self.hunting_budget;
        if budget.max_hunts_per_cycle == 0 {
            violations.push("hunting_budget.max_hunts_per_cycle must be at least 1".to_string());
        } else if budget.max_hunts_per_cycle > MAX_HUNTS_PER_CYCLE {
            violations.push(format!(
                "hunting_budget.max_hunts_per_cycle must be at most {MAX_HUNTS_PER_CYCLE}, got {}",
                budget.max_hunts_per_cycle
            ));
        }
        if budget.max_probes_per_cycle == 0 {
            violations.push("hunting_budget.max_probes_per_cycle must be at least 1".to_string());
        }
        let gate = &self.skill_promotion;
        if gate.eval_suite_ref.as_deref().is_some_and(str::is_empty) {
            violations
                .push("skill_promotion.eval_suite_ref must not be empty when present".to_string());
        }
        if gate.approval_scope.as_deref().is_some_and(str::is_empty) {
            violations
                .push("skill_promotion.approval_scope must not be empty when present".to_string());
        }
        if let Some(cadence) = &self.consolidation_cadence {
            if cadence.min_turns == 0 {
                violations.push("consolidation_cadence.min_turns must be at least 1".to_string());
            }
            if cadence.max_interval_ms == Some(0) {
                violations.push(
                    "consolidation_cadence.max_interval_ms must be at least 1 when present"
                        .to_string(),
                );
            }
        }
        if violations.is_empty() {
            Ok(())
        } else {
            Err(LearningPolicyError { violations })
        }
    }

    /// The budget one hunting cycle may spend under this policy: the
    /// declared bound, narrowed by a caller that asks for less — never
    /// exceeded, never silently. A caller that names no budget spends the
    /// declared one.
    pub fn hunt_budget(&self, caller: Option<u32>) -> u32 {
        let declared = self
            .hunting_budget
            .max_hunts_per_cycle
            .min(MAX_HUNTS_PER_CYCLE);
        caller.unwrap_or(declared).min(declared)
    }

    /// Read the policy declared on a blueprint version's `config` blob —
    /// the `learning_policy` object, absent in every pre-policy version.
    /// `Ok(None)` names the floor; a present-but-invalid object is a typed
    /// error with every violation named, never a silent default.
    pub fn from_config(config: &serde_json::Value) -> Result<Option<Self>, LearningPolicyError> {
        let Some(declared) = config.get("learning_policy") else {
            return Ok(None);
        };
        let policy: LearningPolicy =
            serde_json::from_value(declared.clone()).map_err(|error| LearningPolicyError {
                violations: vec![format!("learning_policy does not parse: {error}")],
            })?;
        policy.validate()?;
        Ok(Some(policy))
    }
}

/// Every way a learning-policy declaration is invalid: the full violation
/// report, so the operator fixes the declaration in one pass.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid learning policy: {}", violations.join("; "))]
pub struct LearningPolicyError {
    /// Each rule the declaration broke, in declaration order.
    pub violations: Vec<String>,
}
