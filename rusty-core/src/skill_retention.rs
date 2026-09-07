//! Retention scoring and the deterministic curator (EP-07-S03): a
//! promoted skill's [`retention score](SkillRetentionRecord::score_milli)
//! decays with idleness and replenishes on load; crossing the cold
//! threshold moves it `Promoted → Cold` and out of the prompt index; the
//! configured idle period moves it `Cold → Archived`. The curator's
//! consolidation merges near-duplicates into one umbrella and archives
//! the originals — every step an append-only ledger mutation, and no code
//! path anywhere in the module deletes.
//!
//! The design rules:
//!
//! - **The clock is a parameter.** Every entry point takes `now`; the
//!   module never reads one. Turn traffic and clock advance are the
//!   caller's to supply, which is what makes a full lifecycle —
//!   `Promoted → Cold → Archived` — drivable in a test with no sleep.
//! - **Decay is accounted, not recomputed.** Each record carries the
//!   instant its score is accounted to; a tick charges only the
//!   unaccounted interval, so tick cadence never changes the outcome and
//!   a double tick is a no-op.
//! - **Ledger on mutation, never on drift.** Loads and decay ticks move
//!   the score continuously and journal nothing; the ledger records the
//!   discrete acts — transitions, pins, restores, consolidations — so the
//!   ledger stays an audit trail, not a time series.
//! - **The prompt-index coupling is one field.** Selection
//!   ([`crate::skills::select_skills`]) excludes catalog entries whose
//!   lifecycle is `Cold` or `Archived` with a typed reason — a cooled
//!   skill leaves the index structurally, and the exclusion is visible in
//!   the selection's audit trail.
//!
//! What this module does not do: schedule the curator (idle-time
//! scheduling is the gateway's, per the EP-04 seam), run the curator as a
//! guarded side-stamped session (the review-fork slice, EP-07-S04), or
//! merge skill *content* — absorbing an archived original's unique
//! material into the umbrella is a drafting act and belongs to that
//! session; the consolidation here is structural: one live umbrella name,
//! the originals archived and pointing at it.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::skill::{SkillMetadata, SkillPromotionStatus};

/// The retention score scale: scores are per-mille, `0..=1000`, matching
/// the gap ledger's similarity arithmetic — integer math, no floats in a
/// governance rule.
pub const RETENTION_SCALE_MILLI: u32 = 1000;

/// The seconds in one day of idleness — decay granularity.
const SECS_PER_DAY: u64 = 86_400;

/// The retention rule set, pinned alongside the assembly policy and
/// versioned with it — never hard-coded into the book.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionPolicy {
    /// Below this score a promoted skill goes `Cold` and leaves the
    /// prompt index.
    pub cold_threshold_milli: u32,

    /// How long a `Cold` skill may stay idle (no loads) before it
    /// archives.
    pub idle_archive_secs: u64,

    /// Score lost per idle day while `Promoted` (per-mille points).
    pub decay_per_idle_day_milli: u32,

    /// Score regained per load — a skill-view of the skill during a turn.
    pub load_replenish_milli: u32,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            cold_threshold_milli: 300,
            idle_archive_secs: 30 * SECS_PER_DAY,
            decay_per_idle_day_milli: 100,
            load_replenish_milli: 200,
        }
    }
}

impl RetentionPolicy {
    /// The invariants: the threshold lives inside the scale, decay and
    /// replenish are nonzero and within the scale, and the idle period is
    /// nonzero. A misconfigured policy is an error at the boundary, never
    /// a silent zero inside the book.
    pub fn validate(&self) -> Result<(), RetentionError> {
        if self.cold_threshold_milli == 0 || self.cold_threshold_milli >= RETENTION_SCALE_MILLI {
            return Err(RetentionError::InvalidPolicy {
                reason: "cold_threshold_milli must be inside the per-mille scale",
            });
        }
        if self.decay_per_idle_day_milli == 0
            || self.decay_per_idle_day_milli > RETENTION_SCALE_MILLI
        {
            return Err(RetentionError::InvalidPolicy {
                reason: "decay_per_idle_day_milli must be in 1..=1000",
            });
        }
        if self.load_replenish_milli == 0 || self.load_replenish_milli > RETENTION_SCALE_MILLI {
            return Err(RetentionError::InvalidPolicy {
                reason: "load_replenish_milli must be in 1..=1000",
            });
        }
        if self.idle_archive_secs == 0 {
            return Err(RetentionError::InvalidPolicy {
                reason: "idle_archive_secs must be nonzero",
            });
        }
        Ok(())
    }
}

/// A discrete retention act, journaled into the skill's append-only
/// ledger. Closed enum — every variant carries what an auditor needs to
/// reconstruct why the lifecycle moved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RetentionMutation {
    /// The score crossed the cold threshold; the skill went
    /// `Promoted → Cold` and left the prompt index.
    CooledToCold {
        /// The score at the crossing.
        score_milli: u32,
    },
    /// The idle period elapsed while `Cold`; the skill archived.
    Archived {
        /// How long the skill had been idle when it archived.
        idle_secs: u64,
    },
    /// An operator restored an archived skill to `Promoted` at full
    /// score — the only way back, and always a person, never the loop.
    Restored,
    /// The curator pinned the skill: retention-exempt until unpinned.
    Pinned,
    /// The pin lifted; decay resumes from the pin's accounting instant.
    Unpinned,
    /// The curator consolidated this skill into an umbrella: this record
    /// archives and names where its material now lives.
    ConsolidatedIntoUmbrella {
        /// The surviving umbrella skill.
        umbrella: String,
    },
    /// The curator designated this skill the umbrella for a cluster of
    /// near-duplicates.
    DesignatedUmbrella {
        /// The skills archived into this one (name-sorted).
        merged: Vec<String>,
    },
}

/// One append-only retention ledger row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetentionLedgerEntry {
    /// The 1-based position in this skill's retention ledger.
    pub seq: u64,
    /// Who performed the act (`retention` for ticks, `curator` for a
    /// pass, the operator's id for restores and pins).
    pub actor: String,
    /// What happened.
    pub mutation: RetentionMutation,
    /// When the act was recorded (the caller's clock).
    pub recorded_at: DateTime<Utc>,
}

/// One skill's retention state: the score, the load and accounting
/// instants, the lifecycle the score drives, the pin, and the ledger.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillRetentionRecord {
    /// Where the skill sits in the retention half of the lifecycle
    /// (`Promoted`, `Cold`, `Archived`). `Draft`/`Trial` never appear —
    /// the book tracks promoted skills only; the eval gate owns the rest.
    pub lifecycle: SkillPromotionStatus,

    /// The retention score, `0..=1000` per-mille.
    pub score_milli: u32,

    /// When the skill entered the book (promotion instant).
    pub registered_at: DateTime<Utc>,

    /// The most recent load (a skill-view of the skill during a turn).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_load_at: Option<DateTime<Utc>>,

    /// The instant the score is decay-accounted to. Private: ticks and
    /// loads move it, and no reader may reason from it — the score is the
    /// public truth.
    #[serde(default = "epoch")]
    decay_accounted_until: DateTime<Utc>,

    /// When the skill went `Cold` (the idle-archive clock's base).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entered_cold_at: Option<DateTime<Utc>>,

    /// `true` while pinned: retention-exempt — no decay, no transitions.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pinned: bool,

    /// The append-only retention ledger.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ledger: Vec<RetentionLedgerEntry>,
}

fn epoch() -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp(0, 0).expect("the epoch is a valid timestamp")
}

impl SkillRetentionRecord {
    fn record(&mut self, actor: &str, mutation: RetentionMutation, at: DateTime<Utc>) {
        let seq = self.ledger.len() as u64 + 1;
        self.ledger.push(RetentionLedgerEntry {
            seq,
            actor: actor.to_owned(),
            mutation,
            recorded_at: at,
        });
    }
}

/// Every way the retention plane can refuse an operation. Refusals are
/// typed — a refused transition is an error, never a silent no-op.
#[derive(Debug, Error)]
pub enum RetentionError {
    /// The skill is not in the book. The book tracks promoted skills;
    /// anything else is a caller bug.
    #[error("skill `{0}` is not tracked — the retention book holds promoted skills only")]
    UntrackedSkill(String),

    /// The skill is already in the book; re-registration would silently
    /// reset its score and history.
    #[error("skill `{0}` is already tracked — re-registration is not a retention act")]
    AlreadyTracked(String),

    /// A transition the retention state machine does not admit (restoring
    /// a skill that is not archived, pinning an archived one, loading an
    /// archived skill back into view).
    #[error(
        "invalid retention transition for `{skill}`: cannot {action} from `{from:?}` — \
         refused transitions are errors, not silent no-ops"
    )]
    InvalidTransition {
        /// The skill the transition was attempted on.
        skill: String,
        /// The lifecycle state the attempt started from.
        from: SkillPromotionStatus,
        /// The attempted action.
        action: &'static str,
    },

    /// A policy that fails [`RetentionPolicy::validate`].
    #[error("invalid retention policy: {reason}")]
    InvalidPolicy {
        /// The rule the policy broke.
        reason: &'static str,
    },
}

/// One lifecycle move a tick produced — the audit summary callers report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionTransition {
    /// The skill that moved.
    pub skill: String,
    /// The state it left.
    pub from: SkillPromotionStatus,
    /// The state it entered.
    pub to: SkillPromotionStatus,
    /// The score at the move.
    pub score_milli: u32,
}

/// The retention book: per-skill retention records, name-sorted, one
/// append-only ledger each. The book is data; the policy arrives per call,
/// so a policy change never requires a book migration.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetentionBook {
    skills: BTreeMap<String, SkillRetentionRecord>,
}

impl RetentionBook {
    /// An empty book.
    pub fn new() -> Self {
        Self::default()
    }

    /// One skill's record, when tracked.
    pub fn get(&self, name: &str) -> Option<&SkillRetentionRecord> {
        self.skills.get(name)
    }

    /// Every tracked skill, name-sorted (deterministic iteration).
    pub fn list(&self) -> impl Iterator<Item = &SkillRetentionRecord> {
        self.skills.values()
    }

    /// The number of tracked skills.
    pub fn len(&self) -> usize {
        self.skills.len()
    }

    /// `true` when no skills are tracked.
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// Enter a freshly promoted skill at full score. The promotion itself
    /// journals through the promotion ledger; the book's entry is the
    /// score's starting gun, not a ledger act.
    pub fn register_promoted(
        &mut self,
        name: impl Into<String>,
        now: DateTime<Utc>,
    ) -> Result<(), RetentionError> {
        let name = name.into();
        if self.skills.contains_key(&name) {
            return Err(RetentionError::AlreadyTracked(name));
        }
        self.skills.insert(
            name,
            SkillRetentionRecord {
                lifecycle: SkillPromotionStatus::Promoted,
                score_milli: RETENTION_SCALE_MILLI,
                registered_at: now,
                last_load_at: None,
                decay_accounted_until: now,
                entered_cold_at: None,
                pinned: false,
                ledger: Vec::new(),
            },
        );
        Ok(())
    }

    /// A load — a skill-view of the skill during a turn: the score
    /// replenishes and the decay clock rebases. Loads never journal (turn
    /// traffic is not a ledger act), never transition (the state machine
    /// admits no `Cold → Promoted` edge — a cooled skill is restored by
    /// an operator, not by traffic), and refuse an archived skill: a load
    /// of an archived skill is the operator's `restore`, not traffic.
    pub fn record_load(
        &mut self,
        name: &str,
        now: DateTime<Utc>,
        policy: &RetentionPolicy,
    ) -> Result<u32, RetentionError> {
        policy.validate()?;
        let record = self
            .skills
            .get_mut(name)
            .ok_or_else(|| RetentionError::UntrackedSkill(name.to_owned()))?;
        if record.lifecycle == SkillPromotionStatus::Archived {
            return Err(RetentionError::InvalidTransition {
                skill: name.to_owned(),
                from: record.lifecycle,
                action: "load",
            });
        }
        record.score_milli = record
            .score_milli
            .saturating_add(policy.load_replenish_milli)
            .min(RETENTION_SCALE_MILLI);
        record.last_load_at = Some(now);
        record.decay_accounted_until = now;
        Ok(record.score_milli)
    }

    /// Pin a skill: retention-exempt until unpinned. The pin journals —
    /// exemptions are exactly what an auditor looks for.
    pub fn pin(
        &mut self,
        name: &str,
        actor: &str,
        now: DateTime<Utc>,
    ) -> Result<(), RetentionError> {
        let record = self
            .skills
            .get_mut(name)
            .ok_or_else(|| RetentionError::UntrackedSkill(name.to_owned()))?;
        if record.pinned {
            return Ok(()); // idempotent: the pinned state is the same state
        }
        record.pinned = true;
        record.record(actor, RetentionMutation::Pinned, now);
        Ok(())
    }

    /// Lift a pin. Decay resumes from the unpin instant — time under the
    /// pin is never back-charged.
    pub fn unpin(
        &mut self,
        name: &str,
        actor: &str,
        now: DateTime<Utc>,
    ) -> Result<(), RetentionError> {
        let record = self
            .skills
            .get_mut(name)
            .ok_or_else(|| RetentionError::UntrackedSkill(name.to_owned()))?;
        if !record.pinned {
            return Ok(());
        }
        record.pinned = false;
        record.decay_accounted_until = now;
        record.record(actor, RetentionMutation::Unpinned, now);
        Ok(())
    }

    /// Restore an archived skill to `Promoted` at full score — the only
    /// way back, and an operator's act, so it journals.
    pub fn restore(
        &mut self,
        name: &str,
        actor: &str,
        now: DateTime<Utc>,
    ) -> Result<(), RetentionError> {
        let record = self
            .skills
            .get_mut(name)
            .ok_or_else(|| RetentionError::UntrackedSkill(name.to_owned()))?;
        if record.lifecycle != SkillPromotionStatus::Archived {
            return Err(RetentionError::InvalidTransition {
                skill: name.to_owned(),
                from: record.lifecycle,
                action: "restore",
            });
        }
        record.lifecycle = SkillPromotionStatus::Promoted;
        record.score_milli = RETENTION_SCALE_MILLI;
        record.entered_cold_at = None;
        record.decay_accounted_until = now;
        record.record(actor, RetentionMutation::Restored, now);
        Ok(())
    }

    /// Charge idleness and move the skills whose scores or idle periods
    /// crossed their lines. Pure in the clock: `now` arrives as a
    /// parameter, the charged interval is `decay_accounted_until..now`,
    /// and each record's accounting instant advances — so ticks compose,
    /// and a repeated tick at the same instant is a no-op.
    ///
    /// Pinned skills are skipped whole (no decay, no transitions). The
    /// returned transitions name every move, in name order.
    pub fn tick(
        &mut self,
        policy: &RetentionPolicy,
        now: DateTime<Utc>,
        actor: &str,
    ) -> Result<Vec<RetentionTransition>, RetentionError> {
        policy.validate()?;
        let mut transitions = Vec::new();
        for (name, record) in self.skills.iter_mut() {
            if record.pinned {
                continue;
            }
            match record.lifecycle {
                SkillPromotionStatus::Promoted => {
                    let idle_secs = now
                        .signed_duration_since(record.decay_accounted_until)
                        .num_seconds()
                        .max(0) as u64;
                    let decay = (idle_secs / SECS_PER_DAY)
                        .saturating_mul(policy.decay_per_idle_day_milli as u64)
                        .min(RETENTION_SCALE_MILLI as u64) as u32;
                    record.score_milli = record.score_milli.saturating_sub(decay);
                    record.decay_accounted_until = now;
                    if record.score_milli < policy.cold_threshold_milli {
                        record.lifecycle = SkillPromotionStatus::Cold;
                        record.entered_cold_at = Some(now);
                        record.record(
                            actor,
                            RetentionMutation::CooledToCold {
                                score_milli: record.score_milli,
                            },
                            now,
                        );
                        transitions.push(RetentionTransition {
                            skill: name.clone(),
                            from: SkillPromotionStatus::Promoted,
                            to: SkillPromotionStatus::Cold,
                            score_milli: record.score_milli,
                        });
                    }
                }
                SkillPromotionStatus::Cold => {
                    // The idle-archive clock runs from the later of the
                    // cold entry and the last load: a load while Cold
                    // cannot warm the skill back, but it does signal
                    // use, and use defers the archive.
                    let base = record
                        .entered_cold_at
                        .into_iter()
                        .chain(record.last_load_at)
                        .max()
                        .unwrap_or(now);
                    let idle_secs = now.signed_duration_since(base).num_seconds().max(0) as u64;
                    if idle_secs >= policy.idle_archive_secs {
                        record.lifecycle = SkillPromotionStatus::Archived;
                        record.record(actor, RetentionMutation::Archived { idle_secs }, now);
                        transitions.push(RetentionTransition {
                            skill: name.clone(),
                            from: SkillPromotionStatus::Cold,
                            to: SkillPromotionStatus::Archived,
                            score_milli: record.score_milli,
                        });
                    }
                }
                _ => {}
            }
        }
        Ok(transitions)
    }
}

/// The curator's rule set, pinned alongside the retention policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CuratorPolicy {
    /// Token-Jaccard similarity (per-mille) at or above which two skills
    /// are near-duplicates. Deterministic and embedding-free — the
    /// release's vector decision applies to the curator too.
    pub duplicate_threshold_milli: u32,
}

impl Default for CuratorPolicy {
    fn default() -> Self {
        Self {
            duplicate_threshold_milli: 600,
        }
    }
}

/// One consolidation the curator committed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsolidationOutcome {
    /// The surviving umbrella skill.
    pub umbrella: String,
    /// The near-duplicates archived into it (name-sorted).
    pub merged: Vec<String>,
}

/// What one curator pass did: the consolidations, in umbrella order, and
/// how many tracked skills were examined. The pass's every act journaled
/// into the per-skill ledgers — the report is the summary, the ledgers
/// are the audit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CuratorReport {
    /// The consolidations committed, umbrella-name-sorted.
    pub consolidations: Vec<ConsolidationOutcome>,
    /// How many tracked, non-archived skills the pass scored.
    pub skills_examined: usize,
}

/// Lowercase alphanumeric tokens of `text`, as a set — the similarity
/// vocabulary.
fn tokens_of(text: &str) -> std::collections::BTreeSet<String> {
    text.to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Token-Jaccard similarity, per-mille: `1000 * |a ∩ b| / |a ∪ b|`.
fn similarity_milli(
    a: &std::collections::BTreeSet<String>,
    b: &std::collections::BTreeSet<String>,
) -> u32 {
    let intersection = a.intersection(b).count() as u64;
    let union = a.union(b).count() as u64;
    if union == 0 {
        return 0;
    }
    (intersection * RETENTION_SCALE_MILLI as u64 / union) as u32
}

/// A union-find over cluster members, keyed by position.
struct ClusterSet {
    parent: Vec<usize>,
}

impl ClusterSet {
    fn new(len: usize) -> Self {
        Self {
            parent: (0..len).collect(),
        }
    }

    fn root(&mut self, index: usize) -> usize {
        let mut at = index;
        while self.parent[at] != at {
            at = self.parent[at];
        }
        let mut walk = index;
        while self.parent[walk] != at {
            let next = self.parent[walk];
            self.parent[walk] = at;
            walk = next;
        }
        at
    }

    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.root(a), self.root(b));
        if ra != rb {
            self.parent[rb] = ra;
        }
    }
}

/// Run one deterministic curator pass over the tracked library:
/// near-duplicates (token-Jaccard on name plus description at or above
/// the threshold) cluster; each cluster's umbrella is the pinned skill,
/// else the highest-scored, ties broken by ascending name; the rest
/// archive with a `ConsolidatedIntoUmbrella` ledger row naming the
/// umbrella, and the umbrella journals its `DesignatedUmbrella`.
///
/// Pinned skills are never merged away — a pin is the operator saying
/// *keep this one*, and the curator is not above the operator. Archived
/// skills are invisible to the pass (already out of the library). The
/// pass deletes nothing: archiving is a ledger act, the records stay.
pub fn curator_pass(
    book: &mut RetentionBook,
    catalog: &[SkillMetadata],
    policy: &CuratorPolicy,
    now: DateTime<Utc>,
    actor: &str,
) -> CuratorReport {
    // The pass's population: tracked, non-archived skills present in the
    // catalog, name-sorted — catalog order never influences the outcome.
    let mut entries: Vec<(String, std::collections::BTreeSet<String>)> = catalog
        .iter()
        .filter(|metadata| {
            book.get(&metadata.name)
                .is_some_and(|record| record.lifecycle != SkillPromotionStatus::Archived)
        })
        .map(|metadata| {
            (
                metadata.name.clone(),
                tokens_of(&format!("{} {}", metadata.name, metadata.description)),
            )
        })
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let examined = entries.len();

    let mut clusters = ClusterSet::new(entries.len());
    for left in 0..entries.len() {
        for right in (left + 1)..entries.len() {
            if similarity_milli(&entries[left].1, &entries[right].1)
                >= policy.duplicate_threshold_milli
            {
                clusters.union(left, right);
            }
        }
    }

    let mut by_root: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for index in 0..entries.len() {
        let root = clusters.root(index);
        by_root.entry(root).or_default().push(index);
    }

    let mut consolidations = Vec::new();
    for (_root, members) in by_root {
        if members.len() < 2 {
            continue;
        }
        // The umbrella: a pinned member first (the operator's word
        // outranks the score), then the highest score, then the lowest
        // name — every tiebreak total, so two passes over equal books
        // pick the same umbrella.
        let umbrella = members
            .iter()
            .min_by(|a, b| {
                let pa = book.skills[&entries[**a].0].pinned;
                let pb = book.skills[&entries[**b].0].pinned;
                pb.cmp(&pa)
                    .then_with(|| {
                        book.skills[&entries[**b].0]
                            .score_milli
                            .cmp(&book.skills[&entries[**a].0].score_milli)
                    })
                    .then_with(|| entries[**a].0.cmp(&entries[**b].0))
            })
            .copied()
            .expect("a consolidating cluster is non-empty");
        let umbrella_name = entries[umbrella].0.clone();
        let mut merged: Vec<String> = members
            .iter()
            .copied()
            .filter(|index| *index != umbrella)
            .filter(|index| {
                // Pinned members are never merged away — they stay live
                // beside the umbrella.
                !book.skills[&entries[*index].0].pinned
            })
            .map(|index| entries[index].0.clone())
            .collect();
        merged.sort();
        if merged.is_empty() {
            continue;
        }
        for name in &merged {
            let record = book
                .skills
                .get_mut(name)
                .expect("the pass only clusters tracked skills");
            record.lifecycle = SkillPromotionStatus::Archived;
            record.record(
                actor,
                RetentionMutation::ConsolidatedIntoUmbrella {
                    umbrella: umbrella_name.clone(),
                },
                now,
            );
        }
        let umbrella_record = book
            .skills
            .get_mut(&umbrella_name)
            .expect("the umbrella is tracked");
        umbrella_record.record(
            actor,
            RetentionMutation::DesignatedUmbrella {
                merged: merged.clone(),
            },
            now,
        );
        consolidations.push(ConsolidationOutcome {
            umbrella: umbrella_name,
            merged,
        });
    }
    consolidations.sort_by(|a, b| a.umbrella.cmp(&b.umbrella));

    CuratorReport {
        consolidations,
        skills_examined: examined,
    }
}
