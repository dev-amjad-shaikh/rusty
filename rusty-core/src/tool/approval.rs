//! The journaled approval gate (evidence and admission wave): one shared,
//! closed approval vocabulary for every gate in the harness, with the
//! asked/decided pair journaled as it happens.
//!
//! The vocabulary lives with the evidence contracts:
//! [`crate::record::ApprovalDecision`] is the closed set of outcomes every
//! gate decides in — only `ApprovedOnce` grants; `Rejected`, `Cancelled`,
//! and `Unavailable` all deny, so the gate is fail-closed by construction
//! and no outcome can be forgotten into a grant.
//!
//! The gate here is the journaling half. [`ApprovalGate::decide`] takes a
//! resolved decision, journals [`RunEventKind::ApprovalAsked`] before it and
//! [`RunEventKind::ApprovalDecided`] after — parented into the open turn,
//! the decided event parented to its ask — and returns the decision for the
//! caller to act on. The gate never decides by itself: the decision source
//! (an operator prompt, a presented token, a policy) stays with the asking
//! plane, so the evidence records *what was decided*, honestly, whoever
//! decided it.
//!
//! # Adopting the vocabulary
//!
//! The composer plane's publish gate is the reference adoption
//! ([`crate::composer::PublishComposedSkillTool`]). The capability packs'
//! approval paths — the CLI and computer tools' irreversible effects,
//! admitted through the effect kernel's
//! [`crate::effects::EffectAdmissionContext`] — present a different shape
//! today: an [`crate::effects::ApprovalToken`] consumed at admission. That
//! consumption *is* a decision in this vocabulary (`token present and
//! scoped` → `ApprovedOnce { approved_by: token.approved_by() }`; no token
//! → `Unavailable`); journaling the asked/decided pair at that boundary is
//! the documented adapter point, deferred because the admission context
//! lives outside this wave's file scope. Packs building new gates should
//! take an [`ApprovalGate`] and decide in [`crate::record::ApprovalDecision`]
//! directly.

use std::sync::Arc;

use serde_json::Value;

use crate::journal::{EventDraft, Journal};
use crate::record::{ApprovalDecision, ApprovalOutcome, ApprovalRequest, Effect, RunEventKind};

/// A decision source for gates that ask (parity wave): maps an
/// [`ApprovalRequest`] into the closed [`ApprovalDecision`] vocabulary.
///
/// Shared by the permission presets ([`crate::capability`]) and the Claude
/// Code hook bridge ([`crate::hooks`]) so every asking plane speaks exactly
/// one vocabulary — and so a run can wire one answerer (an operator prompt,
/// a policy engine) behind every gate. An answerer that cannot answer
/// returns [`ApprovalDecision::Unavailable`], which denies like any other
/// non-grant: the vocabulary stays fail-closed however it is sourced.
pub type ApprovalAnswerer = Arc<dyn Fn(&ApprovalRequest) -> ApprovalDecision + Send + Sync>;

/// A one-line rendering of a decision for guard denial reasons
/// (`pub(crate)`: the preset and hook guards phrase their denials with it).
pub(crate) fn decision_summary(decision: &ApprovalDecision) -> String {
    match decision {
        ApprovalDecision::ApprovedOnce { approved_by } => {
            format!("approved once by `{approved_by}`")
        }
        ApprovalDecision::Rejected { decided_by, reason } => match reason {
            Some(reason) => format!("rejected by `{decided_by}`: {reason}"),
            None => format!("rejected by `{decided_by}`"),
        },
        ApprovalDecision::Cancelled { reason } => match reason {
            Some(reason) => format!("cancelled: {reason}"),
            None => "cancelled".to_owned(),
        },
        ApprovalDecision::Unavailable { reason } => match reason {
            Some(reason) => format!("no decider available: {reason}"),
            None => "no decider available".to_owned(),
        },
    }
}

/// A journaling approval gate: writes the asked/decided pair of
/// [`crate::record::ApprovalDecision`]s into a run's journal.
///
/// Construct per decision scope, mirroring the recording wrappers'
/// discipline: [`ApprovalGate::for_turn`] anchors the pair inside the open
/// turn (the causal parent is the invocation's node-input event id, the
/// `PARENT_EVENT_KEY` discipline [`crate::react`] parents model and tool
/// effects with); [`ApprovalGate::new`] journals at run level for gates
/// that stand outside any node invocation.
///
/// Cheap to clone (the journal is a shared handle).
#[derive(Debug, Clone)]
pub struct ApprovalGate {
    journal: Journal,
    parent: Option<String>,
}

impl ApprovalGate {
    /// A gate journaling into `journal` without a causal anchor — for gates
    /// that stand outside any node invocation. Prefer
    /// [`ApprovalGate::for_turn`] inside a run: an unanchored pair is
    /// evidence without its turn.
    pub fn new(journal: &Journal) -> Self {
        Self {
            journal: journal.clone(),
            parent: None,
        }
    }

    /// A gate journaling into `journal`, causally parented to `parent` —
    /// the open turn's node-input event id.
    pub fn for_turn(journal: &Journal, parent: impl Into<String>) -> Self {
        Self {
            journal: journal.clone(),
            parent: Some(parent.into()),
        }
    }

    /// The journal this gate writes into.
    pub fn journal(&self) -> &Journal {
        &self.journal
    }

    /// Journal the asked/decided pair for `request` and return `decision`.
    ///
    /// The ask is journaled first and the decision parented to it, so the
    /// pair reads as one causal unit inside the open turn. The decision is
    /// journaled exactly as resolved — the gate does not second-guess its
    /// source — and returned unchanged; the caller enforces it (only
    /// [`ApprovalDecision::grants`] admits).
    pub fn decide(
        &self,
        request: &ApprovalRequest,
        decision: ApprovalDecision,
    ) -> ApprovalDecision {
        let mut asked = EventDraft::new(RunEventKind::ApprovalAsked, Effect::Pure)
            .input(serde_json::to_value(request).expect("an ApprovalRequest always serializes"));
        if let Some(parent) = &self.parent {
            asked = asked.parent(parent.clone());
        }
        let asked_id = self.journal.record(asked);
        self.journal.record(
            EventDraft::new(RunEventKind::ApprovalDecided, Effect::Pure)
                .output(
                    serde_json::to_value(ApprovalOutcome {
                        kind: request.kind.clone(),
                        effect_id: request.effect_id.clone(),
                        decision: decision.clone(),
                    })
                    .expect("an ApprovalOutcome always serializes"),
                )
                .parent(asked_id),
        );
        decision
    }
}

/// Convenience: the `detail` payload for an ask, as a JSON object.
///
/// Askers build `{"content_hash": …}`-style context through here so the
/// payload is always an object — the shape auditors pattern-match on.
pub fn ask_detail(entries: impl IntoIterator<Item = (impl Into<String>, Value)>) -> Value {
    entries
        .into_iter()
        .map(|(key, value)| (key.into(), value))
        .collect::<serde_json::Map<String, Value>>()
        .into()
}
