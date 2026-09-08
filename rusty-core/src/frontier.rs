//! The frontier-expansion cycle (EP-07-S11): widening the intent map
//! speculatively along the business's own adjacencies, mastery-gated and
//! under operator-owned bounds.
//!
//! W1 landed the speculative machinery in the gap ledger — the
//! trust-ordered [`AdjacencySource`], the `Speculative` origin that cannot
//! reach the hunting queue unvalidated, probes that promote or park, and
//! the declining-frequency decay clock. What it deliberately left open is
//! the *cycle*: who decides a domain is mastered enough to widen, how many
//! entries one pass may open, and how many probes it may run. This module
//! is that cycle.
//!
//! - **[`assess_domain`]** — the mastery gate (AC5). A domain masters when
//!   every one of its intents carries enough measured outcomes with the
//!   failure rate below threshold (the behavioral signal, EP-07-S12) *and*
//!   every one of its skills sits churn-free in the retention book for the
//!   stability window (EP-07-S03). Mastery makes the domain's learning
//!   budget eligible for reallocation from deepening to widening; where
//!   the budget itself lives is the blueprint's concern, not the ledger's.
//!   Expansion on an unmastered domain is a typed refusal, never a quiet
//!   skip.
//! - **[`run_expansion_cycle`]** — one bounded pass (AC6). At most
//!   `max_open_per_cycle` proposals open as speculative entries, at most
//!   `max_probes_per_cycle` probe observations record; everything beyond
//!   the dials is deferred and named in the report. The decisions
//!   themselves journal through the ledger's own mutations — the filing
//!   carries the adjacency source and edge citation, the probe carries
//!   its volumes — so "why did the agent decide laptops imply VPN" is
//!   answerable from the record.
//! - **The probe schedule** — [`crate::gaps::GapLedgerEntry::probe_schedule`]
//!   and [`crate::gaps::GapLedger::probes_due`] make the parked decay clock
//!   readable: when the next probe is due, and when an entry that keeps
//!   probing empty expires for good (AC3's `Parked { next_probe_at,
//!   expires_at }` and AC4's declining frequency, as data).
//!
//! The cycle is a pure function over the two books and the caller's
//! clock: no scheduling, no background task — when it runs and against
//! which domain is the platform's call.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::gaps::{
    AdjacencySource, Citation, ClosureCriteria, GapError, GapLedger, GapSubject, IntentTally,
};
use crate::skill_retention::{RetentionBook, RetentionMutation};

/// The operator-owned dials for one expansion cycle: when a domain counts
/// as mastered, and how much one cycle may do. A plain value type, the
/// `RetentionPolicy` precedent — the dials' deployment home is the
/// blueprint's [`crate::learning::LearningPolicy`] (EP-08-S07): the hunting
/// budget governs the probe dial, the frontier-specific dials keep their
/// defaults until the policy grows vocabulary for them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpansionPolicy {
    /// An intent passes when its measured failure rate is strictly below
    /// this per-mille threshold.
    pub mastery_failure_threshold_millis: u64,
    /// ... and carries at least this many decisive outcomes — an
    /// unmeasured intent is not a passing intent.
    pub mastery_min_outcomes: u64,
    /// A skill is stable when no churn act (a cooling, an archive, a
    /// restore, an outcome penalty) journals within this many seconds of
    /// the assessment instant.
    pub stability_window_secs: u64,
    /// At most this many speculative entries open per cycle.
    pub max_open_per_cycle: u32,
    /// At most this many probe observations record per cycle.
    pub max_probes_per_cycle: u32,
}

impl Default for ExpansionPolicy {
    fn default() -> Self {
        Self {
            mastery_failure_threshold_millis: 200,
            mastery_min_outcomes: 10,
            stability_window_secs: 7 * 86_400,
            max_open_per_cycle: 3,
            max_probes_per_cycle: 10,
        }
    }
}

impl ExpansionPolicy {
    /// The policy's own honesty rules: a threshold within the per-mille
    /// scale, a nonzero evidence floor, and nonzero dials — a cycle
    /// configured to do nothing is a configuration error, not a quiet
    /// no-op.
    pub fn validate(&self) -> Result<(), FrontierError> {
        if self.mastery_failure_threshold_millis == 0
            || self.mastery_failure_threshold_millis > 1000
        {
            return Err(FrontierError::InvalidPolicy(format!(
                "mastery_failure_threshold_millis must be 1..=1000, got {}",
                self.mastery_failure_threshold_millis
            )));
        }
        if self.mastery_min_outcomes == 0 {
            return Err(FrontierError::InvalidPolicy(
                "mastery_min_outcomes must be at least 1".to_string(),
            ));
        }
        if self.max_open_per_cycle == 0 || self.max_probes_per_cycle == 0 {
            return Err(FrontierError::InvalidPolicy(
                "the per-cycle dials must be at least 1".to_string(),
            ));
        }
        Ok(())
    }

    /// The cycle dials under a blueprint learning policy (EP-08-S07 AC 2):
    /// the declared hunting budget governs the probe dial exactly;
    /// frontier-specific dials (the mastery gate, the stability window,
    /// the per-cycle open bound) have no policy vocabulary yet and keep
    /// this policy's own values.
    pub fn with_learning_policy(mut self, policy: &crate::learning::LearningPolicy) -> Self {
        self.max_probes_per_cycle = policy.hunting_budget.max_probes_per_cycle;
        self
    }

    /// The default dials governed by a blueprint learning policy — the
    /// construction every policy-homed frontier cycle makes.
    pub fn from_learning_policy(policy: &crate::learning::LearningPolicy) -> Self {
        Self::default().with_learning_policy(policy)
    }
}

/// Every way the expansion cycle can refuse or fail.
#[derive(Debug, thiserror::Error)]
pub enum FrontierError {
    /// The policy itself is misconfigured.
    #[error("invalid expansion policy: {0}")]
    InvalidPolicy(String),
    /// The domain is not mastered; each reason names the intent or skill
    /// and the number that failed the gate. Expansion never triggers on
    /// an unmastered domain.
    #[error("domain `{domain}` is not mastered: {}", .reasons.join("; "))]
    DomainNotMastered {
        /// The domain that failed the gate.
        domain: String,
        /// Every cause, one per failing intent or skill.
        reasons: Vec<String>,
    },
    /// The ledger refused an open or a probe.
    #[error(transparent)]
    Gap(#[from] GapError),
}

/// The mastery assessment: `mastered` plus the full reason list. Every
/// failing intent and skill is named with its number, so the report
/// itself answers "why not yet".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MasteryReport {
    /// `true` when every intent passes and every skill is stable.
    pub mastered: bool,
    /// Each cause of failure, named (empty when mastered).
    pub reasons: Vec<String>,
}

/// A domain's membership as the caller knows it: its name, its intents
/// (the miner's vocabulary), and its retention-scored skills. The intent
/// map owns which intents belong to a domain; the cycle takes membership
/// as given.
#[derive(Debug, Clone, Copy)]
pub struct DomainMembership<'a> {
    /// The domain's name — carried into reports and refusals.
    pub name: &'a str,
    /// The domain's intents, whose failure rates the gate reads.
    pub intents: &'a [String],
    /// The domain's skills, whose churn the gate reads.
    pub skills: &'a [String],
}

/// Assess one domain (AC5): its intents' measured failure rates against
/// the threshold with the evidence floor, and its skills' churn against
/// the stability window. Churn is a lifecycle-moving act — `CooledToCold`,
/// `Archived`, `Restored`, `OutcomePenalized` — journaled within the
/// window; pins and unpins are operator governance, not instability, and
/// loads never journal at all. A skill the book does not track is not
/// retention-scored, so it cannot count as stable.
pub fn assess_domain(
    domain: &DomainMembership<'_>,
    tally_of: &dyn Fn(&str) -> Option<IntentTally>,
    book: &RetentionBook,
    policy: &ExpansionPolicy,
    now: DateTime<Utc>,
) -> MasteryReport {
    let mut reasons = Vec::new();
    for intent in domain.intents {
        match tally_of(intent) {
            Some(tally) => {
                let total = tally.total();
                if total < policy.mastery_min_outcomes {
                    reasons.push(format!(
                        "intent `{intent}` carries {total} decisive outcomes, fewer than {}",
                        policy.mastery_min_outcomes
                    ));
                } else {
                    let rate = tally.failure_rate_millis().unwrap_or(u64::MAX);
                    if rate >= policy.mastery_failure_threshold_millis {
                        reasons.push(format!(
                            "intent `{intent}` fails at {rate} per mille, at or above {}",
                            policy.mastery_failure_threshold_millis
                        ));
                    }
                }
            }
            None => reasons.push(format!("intent `{intent}` has no measured outcomes")),
        }
    }
    let window = chrono::Duration::seconds(policy.stability_window_secs as i64);
    for skill in domain.skills {
        match book.get(skill) {
            Some(record) => {
                let churned = record
                    .ledger
                    .iter()
                    .filter(|row| {
                        matches!(
                            row.mutation,
                            RetentionMutation::CooledToCold { .. }
                                | RetentionMutation::Archived { .. }
                                | RetentionMutation::Restored
                                | RetentionMutation::OutcomePenalized { .. }
                        )
                    })
                    .any(|row| now - row.recorded_at < window);
                if churned {
                    reasons.push(format!(
                        "skill `{skill}` churned within the {}s stability window",
                        policy.stability_window_secs
                    ));
                }
            }
            None => reasons.push(format!("skill `{skill}` is not retention-scored")),
        }
    }
    MasteryReport {
        mastered: reasons.is_empty(),
        reasons,
    }
}

/// One speculative widening the cycle may open: the neighbor intent, the
/// adjacency it was proposed along, and the specific edge that justifies
/// it. The edge citation is the proposal's provenance — `open_speculative`
/// refuses any other citation kind.
#[derive(Debug, Clone, PartialEq)]
pub struct ExpansionProposal {
    /// The neighbor intent (or question shape) the entry covers.
    pub subject: GapSubject,
    /// What the gap is, in one line.
    pub statement: String,
    /// Which source spoke: structural, statistical, or model-prior.
    pub adjacency: AdjacencySource,
    /// The specific edge — a CMDB dependency, a co-occurrence statistic,
    /// a validated guess — as an `AdjacencyEdge` citation.
    pub edge: Citation,
    /// How the entry will know it is done.
    pub closure_criteria: ClosureCriteria,
}

/// One probe observation the cycle may record against a parked or waiting
/// speculative entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeObservation {
    /// The entry probed.
    pub gap_id: String,
    /// Demand-probe hits (matching searches, tickets, chat turns).
    pub demand_hits: u64,
    /// Whether the supply probe found coverage per the crawl.
    pub supply_covered: bool,
}

/// What one bounded cycle did. `opened` and `probed` name what the ledger
/// journaled; `deferred` names what the dials held back — a deferred
/// proposal is not refused, it waits for the next cycle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpansionReport {
    /// The domain the cycle widened.
    pub domain: String,
    /// The speculative entries opened, in proposal order.
    pub opened: Vec<String>,
    /// The entries probed, in observation order.
    pub probed: Vec<String>,
    /// What the per-cycle dials deferred, named by statement or gap id.
    pub deferred: Vec<String>,
}

/// Run one bounded expansion cycle (AC5/AC6): the mastery gate first —
/// an unmastered domain is a typed refusal with its reasons — then at
/// most `max_open_per_cycle` proposals open and at most
/// `max_probes_per_cycle` observations record, in given order. Every
/// decision journals through the ledger's own mutations: the filing
/// carries the adjacency source and its edge, the probe carries its
/// volumes.
#[allow(clippy::too_many_arguments)] // the open_speculative precedent: one logical call
pub fn run_expansion_cycle(
    ledger: &mut GapLedger,
    book: &RetentionBook,
    domain: &DomainMembership<'_>,
    proposals: &[ExpansionProposal],
    probes: &[ProbeObservation],
    policy: &ExpansionPolicy,
    actor: &str,
    now: DateTime<Utc>,
) -> Result<ExpansionReport, FrontierError> {
    policy.validate()?;
    let mastery = assess_domain(
        domain,
        &|intent| ledger.outcome_tally(intent).cloned(),
        book,
        policy,
        now,
    );
    if !mastery.mastered {
        return Err(FrontierError::DomainNotMastered {
            domain: domain.name.to_string(),
            reasons: mastery.reasons,
        });
    }
    let mut report = ExpansionReport {
        domain: domain.name.to_string(),
        opened: Vec::new(),
        probed: Vec::new(),
        deferred: Vec::new(),
    };
    let (open, deferred) =
        proposals.split_at(proposals.len().min(policy.max_open_per_cycle as usize));
    for proposal in open {
        let gap_id = ledger.open_speculative(
            proposal.subject.clone(),
            proposal.statement.clone(),
            proposal.adjacency,
            proposal.edge.clone(),
            proposal.closure_criteria.clone(),
            actor,
            now,
        )?;
        report.opened.push(gap_id);
    }
    report
        .deferred
        .extend(deferred.iter().map(|proposal| proposal.statement.clone()));
    let (run, deferred_probes) =
        probes.split_at(probes.len().min(policy.max_probes_per_cycle as usize));
    for observation in run {
        ledger.record_probe(
            &observation.gap_id,
            observation.demand_hits,
            observation.supply_covered,
            actor,
            now,
        )?;
        report.probed.push(observation.gap_id.clone());
    }
    report
        .deferred
        .extend(deferred_probes.iter().map(|probe| probe.gap_id.clone()));
    Ok(report)
}
