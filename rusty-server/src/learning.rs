//! The learning policy's server half (EP-08-S07): resolution, validation,
//! and live consumption.
//!
//! Core (`rusty_agent_runtime::learning`) owns the declaration's shape and
//! honesty rules. This module owns what the server adds:
//!
//! - **Resolution** ([`resolve`]): the effective policy for one assistant,
//!   read from the session's pinned version when a thread context names one
//!   and from the serving version otherwise — the same governing-version
//!   rule run admission uses (EP-08-S08), so a policy change reaches a
//!   running session only when the session adopts the new version at a turn
//!   boundary, never mid-cycle (AC 4). A version that declares no policy
//!   resolves to the floor, and the answer says so.
//! - **Validation** ([`validate_policy_config`]): a version's
//!   `config.learning_policy` is checked at creation, so an invalid
//!   declaration never enters the immutable lineage to fail at resolve time
//!   — the create answers `400` with every violation named.
//! - **Consumption** ([`consumption`]): the live budget picture of the
//!   current cycle — hunts in flight against the declared bound — so the
//!   console's declared-vs-actual glance (AC 5) has one platform read path.

use rusty_agent_runtime::gaps::{GapLedger, GapStatus};
use rusty_agent_runtime::learning::LearningPolicy;
use serde::Serialize;
use serde_json::Value;

use crate::assistants::AssistantRecord;
use crate::error::ApiError;
use crate::upgrades::AssistantPin;

/// The effective learning policy for one assistant: the version it was read
/// from and whether that version declared one or floors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedPolicy {
    /// The governing version — the session's pin under a thread context,
    /// the assistant's serving version otherwise.
    pub version_id: String,
    /// `true` when the version carries a `learning_policy` declaration;
    /// `false` names the floor.
    pub declared: bool,
    /// The policy itself.
    pub policy: LearningPolicy,
}

/// Resolve the effective learning policy for `record`, governed by `pin`
/// when the caller carries a session context. The pinned (or serving)
/// version's declaration is the only source — no loop reads anywhere else.
/// The caller checks the pin belongs to this assistant (a mismatch is an
/// ambiguity worth a `409`, not a silent fallback); this read trusts it.
pub(crate) fn resolve(
    record: &AssistantRecord,
    pin: Option<&AssistantPin>,
) -> Result<ResolvedPolicy, ApiError> {
    let version_id = match pin {
        Some(pin) => pin.version_id.clone(),
        None => record.active_version_id(),
    };
    let version = record.version(&version_id).ok_or_else(|| {
        ApiError::internal(format!(
            "governing version `{version_id}` no longer resolves for assistant `{}`",
            record.assistant_id
        ))
    })?;
    let policy = LearningPolicy::from_config(&version.config)
        .map_err(|error| ApiError::internal(error.to_string()))?;
    Ok(match policy {
        Some(policy) => ResolvedPolicy {
            version_id,
            declared: true,
            policy,
        },
        None => ResolvedPolicy {
            version_id,
            declared: false,
            policy: LearningPolicy::floor(),
        },
    })
}

/// Check a version-creation config's `learning_policy` declaration: absent
/// is fine, present must parse and pass every honesty rule — `400` with the
/// full violation report otherwise, so the invalid declaration never enters
/// the immutable lineage.
pub(crate) fn validate_policy_config(config: &Value) -> Result<(), ApiError> {
    match LearningPolicy::from_config(config) {
        Ok(_) => Ok(()),
        Err(error) => Err(ApiError::bad_request(error.to_string())),
    }
}

/// The current cycle's live spend against the policy's budgets: hunts
/// standing in `Hunting` (a drafted gap leaves the count — its hunt is
/// done) and speculative entries still open on the frontier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct BudgetConsumption {
    /// Gaps currently `Hunting` — the live hunts spend.
    pub hunts_in_flight: usize,
    /// Speculative entries not yet closed — the frontier's open footprint.
    pub frontier_open_speculative: usize,
}

/// Read the ledger's live consumption picture.
pub(crate) fn consumption(ledger: &GapLedger) -> BudgetConsumption {
    let mut out = BudgetConsumption {
        hunts_in_flight: 0,
        frontier_open_speculative: 0,
    };
    for entry in ledger.entries() {
        if entry.status == GapStatus::Hunting {
            out.hunts_in_flight += 1;
        }
        if entry.origin.is_speculative() && !matches!(entry.status, GapStatus::Closed) {
            out.frontier_open_speculative += 1;
        }
    }
    out
}
