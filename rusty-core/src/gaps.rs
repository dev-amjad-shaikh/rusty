//! The demand-side learning loop: interaction events, the gap ledger, and
//! frontier expansion.
//!
//! The design lineage is the best-of-breed study's cold-start document
//! (`reference/learning-loops-and-cold-start.md` in the research library):
//! every studied platform has the *supply* half of self-knowledge — ledgers
//! of what was learned (this crate's [`crate::learn`] candidates,
//! [`crate::memory`] provenance columns) — and none represents the *demand*
//! half: a ledger of what has not been learned yet, held to the same
//! evidentiary standard. This module is that ledger.
//!
//! Three concepts, each deliberately small:
//!
//! 1. **The interaction event** ([`InteractionEvent`]) is the uniform
//!    record every demand signal normalizes into: actor, channel,
//!    utterance, resolution path, outcome, timestamps, and links back to
//!    the source rows. Append-only; `intent_id` is the only mutable fact
//!    about an event and it mutates through versioned
//!    [`IntentAssignment`] records, never in place. The ingestion rule the
//!    design calls non-negotiable — *preserve the failures* — is
//!    structural here: `NoResult`, `NoClick`, `Reopened`, `Escalated`,
//!    `Abandoned`, and `Unresolved` are first-class variants, not
//!    filtering criteria.
//! 2. **The gap-ledger entry** ([`GapLedgerEntry`]) names one thing the
//!    agent does not know: a statement, a non-empty evidence list (an
//!    entry without citations is invalid by schema — never a bare
//!    assertion), an [`GapOrigin`] class, a priority derived from volume
//!    and failure-cost, typed [`ClosureCriteria`], and a validated status
//!    machine. Origin classes are load-bearing: a ledger that drives
//!    autonomous background work needs provenance at least as much as
//!    memory does, so `speculative` entries — filed by frontier expansion,
//!    subtyped by [`AdjacencySource`] — cannot reach the hunting queue
//!    until a demand probe validates them.
//! 3. **The mutation log** ([`GapMutation`]) is the ledger's persistence
//!    grammar: append-only, content-addressed, chained (each mutation's id
//!    hashes the previous head, so a gap's history is a hash chain), and
//!    rollback-capable — a `RolledBack` mutation re-folds the entry from
//!    the mutation prefix ending at the target, so any prior state is one
//!    command from restored and the restore is exact, not a
//!    reconstruction. Entries are projections of the log, never the store
//!    of record.
//!
//! Runtime filing is the permanence the design insists on: every
//! escalation, correction, and zero-recall lookup files or reinforces an
//! entry through [`GapLedger::file_escalation`],
//! [`GapLedger::file_correction`], and [`GapLedger::file_zero_recall`].
//! Reinforcement — not duplication — is the dedupe rule: a second filing
//! against an already-open gap adds volume, failure-cost, and evidence to
//! the existing entry, and a filing against a *closed* gap reopens it,
//! because the ledger never forgets a gap that was closed on paper but
//! not in practice.
//!
//! The behavioral signal closes measurement: [`GapLedger::record_outcome`]
//! tallies accepted / corrected / redone outcomes per intent (a
//! redo-or-correct is negative signal even when it reads as a polite new
//! instruction — the process-reward insight), and
//! [`GapLedger::sweep_reopens`] reopens closed entries whose measured
//! failure rate says otherwise. Closure itself is mechanical:
//! [`GapLedger::evaluate_closure`] checks the entry's typed criteria
//! against supplied evidence and closes with a resolution link, or
//! refuses.
//!
//! Everything here is pure: no IO, no clocks, no global state. Timestamps
//! are injected, iteration order is `BTreeMap` order, and the whole
//! ledger serializes as one versioned snapshot ([`LEDGER_FORMAT_VERSION`])
//! so a server wave can persist it without a translation layer.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::record::sha256_hex;

// --------------------------------------------------------------------- //
// Constants
// --------------------------------------------------------------------- //

/// The persisted ledger snapshot's format version. Reading a snapshot
/// that declares anything else fails closed — a ledger the plane cannot
/// interpret is evidence to preserve, not to guess at.
pub const LEDGER_FORMAT_VERSION: u32 = 1;

/// [`InteractionEvent`] id prefix.
pub const EVENT_ID_PREFIX: &str = "ie-";
/// [`GapLedgerEntry`] id prefix.
pub const GAP_ID_PREFIX: &str = "gap-";
/// [`GapMutation`] id prefix.
pub const MUTATION_ID_PREFIX: &str = "gm-";
/// [`OutcomeAnnotation`] id prefix.
pub const ANNOTATION_ID_PREFIX: &str = "oa-";

/// Bounds on entry text. An entry is an operator-facing artifact; the
/// bounds keep one filing from spending the ledger's readability.
pub const MAX_STATEMENT_BYTES: usize = 512;
/// See [`MAX_STATEMENT_BYTES`].
pub const MAX_UTTERANCE_BYTES: usize = 4096;
/// See [`MAX_STATEMENT_BYTES`].
pub const MAX_QUESTION_SHAPE_BYTES: usize = 512;
/// See [`MAX_STATEMENT_BYTES`].
pub const MAX_CITATION_NOTE_BYTES: usize = 256;
/// The most citations one entry may carry. Evidence accrues through
/// reinforcement; the bound keeps one entry from becoming its own
/// database.
pub const MAX_EVIDENCE_PER_ENTRY: usize = 64;
/// The most linked events one interaction event may declare.
pub const MAX_EVENT_LINKS: usize = 32;

/// The probe-backoff base, in milliseconds. A parked speculative entry
/// re-probes at declining frequency — `base * 2^probe_count` after each
/// empty probe — so the frontier cannot silt up with the model's
/// ungrounded guesses.
pub const PROBE_BACKOFF_BASE_MILLIS: i64 = 86_400_000; // one day
/// The most empty probes a parked entry survives before
/// [`GapLedger::expire_parked`] closes it as `expired:no-demand`.
pub const MAX_EMPTY_PROBES: u32 = 4;

// --------------------------------------------------------------------- //
// Errors
// --------------------------------------------------------------------- //

/// Every refusal the ledger produces is typed. A ledger that drives
/// autonomous work must never fail silently.
#[derive(Debug, Error)]
pub enum GapError {
    /// A required text field was empty after trimming.
    #[error("empty field: {0}")]
    EmptyField(&'static str),
    /// A text field exceeded its byte bound.
    #[error("field too long: {field} ({len} > {max} bytes)")]
    FieldTooLong {
        /// The field that overflowed.
        field: &'static str,
        /// Its length in bytes.
        len: usize,
        /// The bound it exceeded.
        max: usize,
    },
    /// A gap entry was filed without citations. An entry without evidence
    /// is invalid by schema — the ledger holds ignorance to the same
    /// evidentiary standard as knowledge.
    #[error("gap entry filed without evidence citations")]
    EmptyEvidence,
    /// An outcome annotation was scored without judge samples. The score
    /// is majority-voted across the samples; a score with no samples is
    /// no measurement at all.
    #[error("outcome annotation scored without judge votes")]
    EmptyVotes,
    /// An event with this id is already recorded.
    #[error("interaction event already recorded: {0}")]
    EventExists(String),
    /// An annotation with this id is already recorded with different
    /// content. Re-scoring the same turn converges by identity; a
    /// collision is a typed error, never a silent overwrite.
    #[error("outcome annotation already recorded: {0}")]
    AnnotationExists(String),
    /// No event with this id.
    #[error("unknown interaction event: {0}")]
    UnknownEvent(String),
    /// No gap with this id.
    #[error("unknown gap: {0}")]
    UnknownGap(String),
    /// No mutation with this id on this gap's chain.
    #[error("unknown mutation {mutation} on gap {gap}")]
    UnknownMutation {
        /// The gap whose chain was searched.
        gap: String,
        /// The mutation id that was not on it.
        mutation: String,
    },
    /// A status transition the machine does not admit.
    #[error("illegal status transition: {from} -> {to}")]
    IllegalTransition {
        /// The current status.
        from: GapStatus,
        /// The refused target.
        to: GapStatus,
    },
    /// A speculative entry was sent hunting without a validating probe.
    /// Speculation cannot cite itself as evidence; a demand probe must
    /// confirm it first.
    #[error("speculative gap {0} has not been demand-validated")]
    UnvalidatedSpeculation(String),
    /// A non-speculative entry was parked. Parked is the speculative
    /// decay state; observed gaps never park.
    #[error("only speculative gaps can park: {0}")]
    NotSpeculative(String),
    /// A probe was recorded against an entry that is not speculative.
    #[error("probes apply to speculative gaps only: {0}")]
    ProbeOnObserved(String),
    /// Supplied closure evidence did not satisfy the entry's criteria.
    #[error("closure criteria unsatisfied for gap {gap}: {reason}")]
    ClosureUnsatisfied {
        /// The gap whose closure was refused.
        gap: String,
        /// Why the evidence did not satisfy the criteria.
        reason: String,
    },
    /// A snapshot declared a format version this build cannot read.
    #[error("unsupported ledger format version: {0}")]
    UnsupportedFormat(u32),
    /// Serialization failed.
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}

/// The ledger's result type.
pub type Result<T> = std::result::Result<T, GapError>;

// --------------------------------------------------------------------- //
// Interaction events
// --------------------------------------------------------------------- //

/// The channel an interaction arrived on. Channels are demand shapes:
/// a portal search and an escalated incident are different kinds of
/// evidence about the same need.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionChannel {
    /// A portal or knowledge-base search.
    PortalSearch,
    /// A chat or virtual-agent conversation.
    Chat,
    /// A catalog or service request.
    Request,
    /// An incident ticket.
    Incident,
    /// An HR / facilities / general case.
    Case,
    /// An escalation between support tiers.
    Escalation,
}

/// How the interaction was resolved — the distribution the intent miner
/// ranks demand by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionPath {
    /// The user self-served against existing knowledge.
    SelfService,
    /// An automated surface answered without a human.
    Deflected,
    /// A human resolved it.
    HumanResolved,
    /// It was never resolved.
    Unresolved,
    /// The user gave up.
    Abandoned,
}

/// The interaction's outcome. The failure variants are the highest-value
/// records in any demand corpus: they are the demand signal that supply
/// visibly failed to meet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionOutcome {
    /// Resolved and stayed resolved.
    Resolved,
    /// Resolved, then reopened — the fix did not hold.
    Reopened,
    /// Handed up to a human or a higher tier.
    Escalated,
    /// A search returned nothing.
    NoResult,
    /// A search returned results and the user clicked none.
    NoClick,
}

/// Where the event came from — the citation anchor back into the source
/// system. Every downstream artifact (intent clusters, gap entries,
/// coverage claims) cites events, and citation is only meaningful against
/// an immutable record.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventSource {
    /// The source system (e.g. `servicenow`).
    pub system: String,
    /// The table or stream within it (e.g. `sp_log`, `incident`).
    pub stream: String,
    /// The record id within the stream.
    pub record_id: String,
}

/// Who acted, role-typed and pseudonymizable. The id is whatever the
/// ingesting connector can stably supply — a pseudonym is fine; the
/// ledger needs cohorts, not people.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActorRef {
    /// The actor's role (e.g. `employee`, `desk-agent`, `approver`).
    pub role: String,
    /// A stable, possibly pseudonymized identifier.
    pub id: String,
}

/// One normalized demand signal. Append-only: the only fact about an
/// event that may change is its intent assignment, and that changes
/// through [`IntentAssignment`] records, never in place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteractionEvent {
    /// Content-derived id: `ie-<sha256>` over the canonical record minus
    /// the id itself. Two ingestions of the same source row converge on
    /// one id, so re-running a connector cannot double-count demand.
    pub event_id: String,
    /// The citation anchor back into the source system.
    pub source: EventSource,
    /// Who acted.
    pub actor: ActorRef,
    /// The channel the interaction arrived on.
    pub channel: InteractionChannel,
    /// The expressed need: query text, message, or short description.
    pub utterance: String,
    /// How it was resolved.
    pub resolution_path: ResolutionPath,
    /// What happened in the end.
    pub outcome: InteractionOutcome,
    /// When the interaction occurred (injected by the connector).
    pub occurred_at: DateTime<Utc>,
    /// When it resolved, if it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<DateTime<Utc>>,
    /// Related event ids: the search that preceded the ticket, the
    /// escalation that followed the chat. Journeys are data, not
    /// inference.
    pub links: Vec<String>,
}

/// The canonical, id-free serialization an event id hashes.
#[derive(Serialize)]
struct EventAddress<'a> {
    source: &'a EventSource,
    actor: &'a ActorRef,
    channel: InteractionChannel,
    utterance: &'a str,
    resolution_path: ResolutionPath,
    outcome: InteractionOutcome,
    occurred_at: DateTime<Utc>,
}

/// Derive an event's content address.
pub fn derive_event_id(
    source: &EventSource,
    actor: &ActorRef,
    channel: InteractionChannel,
    utterance: &str,
    resolution_path: ResolutionPath,
    outcome: InteractionOutcome,
    occurred_at: DateTime<Utc>,
) -> Result<String> {
    let address = EventAddress {
        source,
        actor,
        channel,
        utterance,
        resolution_path,
        outcome,
        occurred_at,
    };
    Ok(format!(
        "{EVENT_ID_PREFIX}{}",
        sha256_hex(&serde_json::to_vec(&address)?)
    ))
}

impl InteractionEvent {
    /// Construct an event, deriving its content address. Validates the
    /// fields the schema owns; the connector owns everything else.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        source: EventSource,
        actor: ActorRef,
        channel: InteractionChannel,
        utterance: impl Into<String>,
        resolution_path: ResolutionPath,
        outcome: InteractionOutcome,
        occurred_at: DateTime<Utc>,
        resolved_at: Option<DateTime<Utc>>,
        links: Vec<String>,
    ) -> Result<Self> {
        let utterance = utterance.into();
        check_field("utterance", &utterance, MAX_UTTERANCE_BYTES)?;
        if links.len() > MAX_EVENT_LINKS {
            return Err(GapError::FieldTooLong {
                field: "links",
                len: links.len(),
                max: MAX_EVENT_LINKS,
            });
        }
        let event_id = derive_event_id(
            &source,
            &actor,
            channel,
            &utterance,
            resolution_path,
            outcome,
            occurred_at,
        )?;
        Ok(Self {
            event_id,
            source,
            actor,
            channel,
            utterance,
            resolution_path,
            outcome,
            occurred_at,
            resolved_at,
            links,
        })
    }
}

/// One intent assignment to an event. Assignments are versioned rather
/// than mutable: as clustering improves, events are re-assigned, and the
/// history of who assigned what when is itself evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntentAssignment {
    /// The event being classified.
    pub event_id: String,
    /// The intent it was assigned to.
    pub intent_id: String,
    /// When the assignment was made (injected).
    pub assigned_at: DateTime<Utc>,
    /// What made it — `miner:{pass}` at induction, `runtime:classifier`,
    /// or an operator id.
    pub assigner: String,
}

// --------------------------------------------------------------------- //
// Gap-ledger entries
// --------------------------------------------------------------------- //

/// What the gap is about. Intents are the mined vocabulary; a question
/// shape covers gaps outside the current intent vocabulary — a zero-recall
/// query the clustering has never seen is still a gap.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GapSubject {
    /// A mined intent.
    Intent {
        /// The intent's id in the intent map.
        intent_id: String,
    },
    /// A free question shape outside the intent vocabulary.
    QuestionShape {
        /// The question shape, normalized (trimmed, lowercased) for
        /// matching; the original phrasing lives in the statement.
        text: String,
    },
}

impl GapSubject {
    /// Normalize a free-text question shape for matching: collapsed
    /// whitespace, lowercased. Two filings of "the VPN question" with
    /// different casing reinforce one gap, not two.
    pub fn question_shape(text: &str) -> Result<Self> {
        let normalized = text
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        check_field("question_shape", &normalized, MAX_QUESTION_SHAPE_BYTES)?;
        Ok(GapSubject::QuestionShape { text: normalized })
    }

    /// The intent id this subject names, if it names one.
    pub fn intent_id(&self) -> Option<&str> {
        match self {
            GapSubject::Intent { intent_id } => Some(intent_id),
            GapSubject::QuestionShape { .. } => None,
        }
    }
}

/// The adjacency a speculative entry was expanded along, in descending
/// order of trust. The record of *which* source justified a widening is
/// what keeps frontier expansion explicable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdjacencySource {
    /// A curated graph the business maintains (CMDB edges, catalog
    /// trees, KB taxonomy) — an assertion the business made about
    /// itself, not an inference.
    Structural,
    /// Co-occurrence in the interaction exhaust: intents that recur in
    /// the same tickets, conversations, and journeys.
    Statistical,
    /// The model's world knowledge proposing neighbors. Cheapest and
    /// least trustworthy: a prior about enterprises in general, not
    /// evidence about this one.
    ModelPrior,
}

/// Who filed the gap. Origin classes are structurally distinct because
/// the ledger drives autonomous background work, and work orders need
/// provenance at least as much as memories do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GapOrigin {
    /// Filed by induction analytics over the ingested corpus.
    Induction,
    /// Filed at runtime by an escalation to a human.
    RuntimeEscalation,
    /// Filed at runtime by an operator or user correction.
    RuntimeCorrection,
    /// Filed at runtime by a memory recall that returned nothing.
    ZeroRecall,
    /// Filed by an operator directly.
    Operator,
    /// Derived from untrusted content (a vendor email, a scraped page).
    /// Structurally distinct so governance can treat it more skeptically.
    UntrustedDerived,
    /// Filed by frontier expansion along an adjacency. Cannot reach the
    /// hunting queue until a demand probe validates it.
    Speculative {
        /// The adjacency the expansion walked.
        adjacency: AdjacencySource,
    },
}

impl GapOrigin {
    /// Whether entries of this origin are speculative — unvalidated until
    /// a demand probe confirms them.
    pub fn is_speculative(&self) -> bool {
        matches!(self, GapOrigin::Speculative { .. })
    }
}

/// The kind of record a citation points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CitationKind {
    /// An interaction event in this ledger.
    InteractionEvent,
    /// An edge of the coverage map (intent → supporting artifact).
    CoverageEdge,
    /// The adjacency edge a speculative expansion walked.
    AdjacencyEdge,
    /// The result of a demand or supply probe.
    ProbeResult,
    /// A finalized run's receipt chain.
    RunReceipt,
    /// A governed memory record.
    MemoryRecord,
}

/// One piece of evidence for a gap entry. A citation is a pointer into an
/// append-only record; the note is for humans, never for matching.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Citation {
    /// What kind of record is cited.
    pub kind: CitationKind,
    /// The cited record's id.
    pub id: String,
    /// A short human note on what the citation shows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl Citation {
    /// Construct a citation, validating its fields.
    pub fn new(kind: CitationKind, id: impl Into<String>, note: Option<String>) -> Result<Self> {
        let id = id.into();
        check_field("citation.id", &id, MAX_STATEMENT_BYTES)?;
        if let Some(note) = &note {
            check_field("citation.note", note, MAX_CITATION_NOTE_BYTES)?;
        }
        Ok(Self { kind, id, note })
    }
}

/// The observable conditions under which an entry may be marked closed.
/// Typed, because closure is mechanical: the ledger checks criteria
/// against evidence, never against a narrative.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClosureCriteria {
    /// Closes when the named candidate (skill patch, prompt, memory set)
    /// promotes through the learning gate.
    ArtifactPromoted {
        /// The candidate id whose promotion closes the gap.
        candidate_id: String,
    },
    /// Closes when the named declared memory block is filled.
    BlockFilled {
        /// The block label whose fill closes the gap.
        block_label: String,
    },
    /// Closes when the subject intent's measured failure rate drops
    /// below the threshold (per-mille: corrected + redone per thousand
    /// scored outcomes).
    FailureRateBelow {
        /// The per-mille failure-rate ceiling.
        threshold_millis: u32,
    },
    /// Cannot close mechanically — the gap is a policy contradiction the
    /// business itself must decide. The ledger's deliverable here is the
    /// documented contradiction.
    BusinessDecisionRequired,
}

/// The entry's lifecycle. `Parked` is the speculative decay state;
/// `Reopened` is distinct from `Open` because a gap that was closed on
/// paper and reopened on evidence is a different operational fact than a
/// gap that was never closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GapStatus {
    /// Filed and awaiting a hunt.
    Open,
    /// A background hunt is working it.
    Hunting,
    /// A hunt produced a candidate; governance is deciding.
    TrialPending,
    /// The gap is a business decision, not a learning target.
    BlockedOnBusiness,
    /// Speculative, probe-empty, decaying toward expiry.
    Parked,
    /// Closed with a resolution link.
    Closed,
    /// Was closed; new evidence says otherwise.
    Reopened,
}

impl std::fmt::Display for GapStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let wire = match self {
            GapStatus::Open => "open",
            GapStatus::Hunting => "hunting",
            GapStatus::TrialPending => "trial_pending",
            GapStatus::BlockedOnBusiness => "blocked_on_business",
            GapStatus::Parked => "parked",
            GapStatus::Closed => "closed",
            GapStatus::Reopened => "reopened",
        };
        f.write_str(wire)
    }
}

/// One thing the agent does not know. Entries are projections of the
/// mutation log — this struct is what a fold of the log yields, and it is
/// never the store of record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GapLedgerEntry {
    /// Content-derived id: `gap-<sha256>` over `(subject, statement,
    /// origin)`. The gap's identity is what is unknown, not who filed it
    /// first, so two filings of the same ignorance converge on one entry.
    pub gap_id: String,
    /// What the gap is about.
    pub subject: GapSubject,
    /// What the agent does not know, in one sentence.
    pub statement: String,
    /// The citations justifying the belief that the gap exists.
    /// Non-empty by schema.
    pub evidence: Vec<Citation>,
    /// Who filed it.
    pub origin: GapOrigin,
    /// Observed demand volume behind the entry (events, searches,
    /// tickets). Accrues through reinforcement.
    pub volume: u64,
    /// The failure-cost estimate in milli-units (human resolution
    /// minutes, escalation depth, abandonment — the deployer's weighting,
    /// carried opaquely). Priority is `volume * failure_cost_millis`.
    pub failure_cost_millis: u64,
    /// The observable closure conditions.
    pub closure_criteria: ClosureCriteria,
    /// The lifecycle state.
    pub status: GapStatus,
    /// What closed it: the promoting ledger mutation, the approving
    /// obligation, or `expired:no-demand`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
    /// Whether a demand probe has confirmed a speculative entry. Always
    /// true for non-speculative origins.
    pub observed: bool,
    /// Empty probes recorded against a parked speculative entry. Drives
    /// the declining-frequency re-probe clock and eventual expiry.
    pub empty_probes: u32,
    /// When the entry was filed.
    pub filed_at: DateTime<Utc>,
    /// When the entry last changed.
    pub updated_at: DateTime<Utc>,
}

impl GapLedgerEntry {
    /// The entry's standing priority: volume x failure-cost. The hunting
    /// loop's work order is this score, descending.
    pub fn priority_score(&self) -> u64 {
        self.volume.saturating_mul(self.failure_cost_millis)
    }
}

// --------------------------------------------------------------------- //
// Mutations
// --------------------------------------------------------------------- //

/// What one mutation did. Mutations are the store of record; the entry is
/// their fold. `Filed` carries the full filing payload, so a chain rebuilds
/// its entry with no state beside the log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GapMutationKind {
    /// The entry was filed, with everything the entry is seeded from.
    Filed {
        /// What the gap is about.
        subject: GapSubject,
        /// What the agent does not know, in one sentence.
        statement: String,
        /// The filing citations (non-empty by schema).
        evidence: Vec<Citation>,
        /// Who filed it.
        origin: GapOrigin,
        /// The observable closure conditions.
        closure_criteria: ClosureCriteria,
        /// Initial demand volume.
        volume: u64,
        /// Initial failure-cost estimate in milli-units.
        failure_cost_millis: u64,
        /// Whether the entry is demand-observed at filing (always true
        /// for non-speculative origins).
        observed: bool,
    },
    /// Demand accrued against an existing entry: volume, failure-cost,
    /// and evidence extended.
    Reinforced {
        /// Volume added by this filing.
        added_volume: u64,
        /// Failure-cost added by this filing.
        added_failure_cost_millis: u64,
        /// Newly attached citations.
        new_evidence: Vec<Citation>,
    },
    /// The status machine moved.
    StatusChanged {
        /// The status before.
        from: GapStatus,
        /// The status after.
        to: GapStatus,
    },
    /// A demand/supply probe result against a speculative entry.
    ProbeResultRecorded {
        /// Matching demand found in the event store.
        demand_hits: u64,
        /// Whether reachable supply already covers the intent.
        supply_covered: bool,
    },
    /// The entry was closed with a resolution link.
    Closed {
        /// What closed it.
        resolution: String,
    },
    /// The entry was reopened on new evidence.
    Reopened {
        /// Why.
        reason: String,
    },
    /// The entry was rolled back to a prior mutation. Applying this
    /// mutation re-folds the entry from the chain prefix ending at the
    /// target — the restore is exact, not a reconstruction.
    RolledBack {
        /// The mutation id state was restored to.
        to_mutation_id: String,
    },
}

/// One link in a gap's hash chain. The id hashes the previous head, so
/// the chain is tamper-evident: rewrite history and every descendant id
/// breaks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GapMutation {
    /// Content address: `gm-<sha256>` over `(gap_id, previous head id,
    /// kind, actor, at)`.
    pub mutation_id: String,
    /// The gap this mutation belongs to.
    pub gap_id: String,
    /// The previous head of this gap's chain, or `None` for `Filed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<String>,
    /// What changed.
    pub kind: GapMutationKind,
    /// Who made the change — `induction`, `runtime:escalation`,
    /// `hunter:{name}`, `probe`, an operator id.
    pub actor: String,
    /// When (injected — the ledger keeps no clock).
    pub at: DateTime<Utc>,
}

/// The canonical serialization a mutation id hashes.
#[derive(Serialize)]
struct MutationAddress<'a> {
    gap_id: &'a str,
    previous: Option<&'a str>,
    kind: &'a GapMutationKind,
    actor: &'a str,
    at: DateTime<Utc>,
}

/// Derive a mutation's content address.
pub fn derive_mutation_id(
    gap_id: &str,
    previous: Option<&str>,
    kind: &GapMutationKind,
    actor: &str,
    at: DateTime<Utc>,
) -> Result<String> {
    let address = MutationAddress {
        gap_id,
        previous,
        kind,
        actor,
        at,
    };
    Ok(format!(
        "{MUTATION_ID_PREFIX}{}",
        sha256_hex(&serde_json::to_vec(&address)?)
    ))
}

/// Derive a gap's content address: what is unknown, stated how, filed by
/// which origin class.
pub fn derive_gap_id(subject: &GapSubject, statement: &str, origin: &GapOrigin) -> Result<String> {
    #[derive(Serialize)]
    struct GapAddress<'a> {
        subject: &'a GapSubject,
        statement: &'a str,
        origin: &'a GapOrigin,
    }
    let address = GapAddress {
        subject,
        statement,
        origin,
    };
    Ok(format!(
        "{GAP_ID_PREFIX}{}",
        sha256_hex(&serde_json::to_vec(&address)?)
    ))
}

// --------------------------------------------------------------------- //
// The behavioral signal
// --------------------------------------------------------------------- //

/// The outcome class one served turn earned, per the process-reward
/// insight: a redo, rephrase, or correction is negative signal even when
/// it reads as a polite new instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeClass {
    /// The next user message moved on — the answer held.
    Accepted,
    /// The user corrected the agent.
    Corrected,
    /// The user asked for a redo or rephrase.
    Redone,
    /// No decisive signal: the conversation gave no verdict, or the
    /// jury split. Neutral turns are recorded — they stay out of the
    /// failure-rate denominator, because an undecided turn is neither
    /// evidence the answer held nor evidence it failed.
    Neutral,
}

/// Per-intent outcome tallies. The behavioral signal's whole state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntentTally {
    /// Turns whose answers held.
    pub accepted: u64,
    /// Turns the user corrected.
    pub corrected: u64,
    /// Turns the user asked to redo.
    pub redone: u64,
    /// Turns with no decisive verdict (including split juries). Kept out
    /// of [`IntentTally::total`] and the failure rate — an undecided
    /// turn dilutes nothing. Defaults to zero so snapshots written
    /// before the neutral class existed still load.
    #[serde(default)]
    pub neutral: u64,
}

impl IntentTally {
    /// Total decisive outcomes — neutral turns carry no verdict and do
    /// not count.
    pub fn total(&self) -> u64 {
        self.accepted + self.corrected + self.redone
    }

    /// Failure rate per mille: (corrected + redone) / total. `None` when
    /// nothing has been scored — an unmeasured intent is not a passing
    /// intent.
    pub fn failure_rate_millis(&self) -> Option<u64> {
        let total = self.total();
        if total == 0 {
            return None;
        }
        Some((self.corrected + self.redone).saturating_mul(1000) / total)
    }
}

// --------------------------------------------------------------------- //
// Outcome annotations — the behavioral signal with its evidence attached
// --------------------------------------------------------------------- //

/// One judge's verdict on a served turn. Every sample is recorded: the
/// majority vote decides the outcome, and the votes are the provenance
/// the score can always be re-derived from.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JudgeVote {
    /// Which judge sampled — a model id, a human id, a heuristic's name.
    pub judge: String,
    /// The verdict this sample returned.
    pub vote: OutcomeClass,
}

/// A scored main-line turn, joined to its intent. The outcome is
/// majority-voted across the judge samples at construction and stored
/// denormalized — the score can never disagree with its evidence.
/// Content-addressed over `(turn_ref, intent_id, votes, scored_at)`, so
/// re-scoring the same turn converges by identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutcomeAnnotation {
    /// Content address, [`ANNOTATION_ID_PREFIX`]-prefixed.
    pub annotation_id: String,
    /// The scored turn's anchor back into the log (a `session:turn`
    /// pair, a run id — opaque here; the provider-seam turn-stamp
    /// contract gives it structure).
    pub turn_ref: String,
    /// The intent the turn was joined to, in the miner's vocabulary.
    pub intent_id: String,
    /// The majority-voted outcome — derived from `judge_votes`, never
    /// supplied.
    pub outcome: OutcomeClass,
    /// Every judge sample, recorded.
    pub judge_votes: Vec<JudgeVote>,
    /// When the score was produced — the curve's x-axis.
    pub scored_at: DateTime<Utc>,
}

impl OutcomeAnnotation {
    /// Score a turn from its judge samples. The outcome is the majority
    /// vote; a tie abstains to [`OutcomeClass::Neutral`] — a split jury
    /// is no verdict.
    pub fn from_votes(
        turn_ref: impl Into<String>,
        intent_id: impl Into<String>,
        mut judge_votes: Vec<JudgeVote>,
        scored_at: DateTime<Utc>,
    ) -> Result<Self> {
        let turn_ref = turn_ref.into();
        let intent_id = intent_id.into();
        check_field("turn_ref", &turn_ref, MAX_STATEMENT_BYTES)?;
        check_field("intent_id", &intent_id, MAX_STATEMENT_BYTES)?;
        if judge_votes.is_empty() {
            return Err(GapError::EmptyVotes);
        }
        for vote in &judge_votes {
            check_field("judge", &vote.judge, MAX_STATEMENT_BYTES)?;
        }
        // Canonical order: the same sample set converges on the same
        // content address regardless of submission order.
        judge_votes.sort();
        let outcome = majority_vote(&judge_votes);
        let annotation_id = derive_annotation_id(&turn_ref, &intent_id, &judge_votes, scored_at)?;
        Ok(Self {
            annotation_id,
            turn_ref,
            intent_id,
            outcome,
            judge_votes,
            scored_at,
        })
    }
}

/// What recording an annotation did: the annotation's id, and the gaps
/// the fresh measurement closed (failure-rate closure is automatic —
/// no human bookkeeping).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordedAnnotation {
    /// The recorded annotation's id.
    pub annotation_id: String,
    /// Entries whose `FailureRateBelow` criterion this measurement
    /// satisfied, closed in the same mutation.
    pub closed_gap_ids: Vec<String>,
}

/// Majority rule over judge samples: the class with the most votes
/// wins; any tie for the lead abstains to `Neutral`.
fn majority_vote(votes: &[JudgeVote]) -> OutcomeClass {
    let mut counts = [0_u64; 4];
    for vote in votes {
        let index = match vote.vote {
            OutcomeClass::Accepted => 0,
            OutcomeClass::Corrected => 1,
            OutcomeClass::Redone => 2,
            OutcomeClass::Neutral => 3,
        };
        counts[index] += 1;
    }
    let classes = [
        OutcomeClass::Accepted,
        OutcomeClass::Corrected,
        OutcomeClass::Redone,
        OutcomeClass::Neutral,
    ];
    let lead = counts.iter().max().copied().unwrap_or(0);
    let winners: Vec<OutcomeClass> = classes
        .into_iter()
        .zip(counts)
        .filter(|(_, count)| *count == lead)
        .map(|(class, _)| class)
        .collect();
    match winners.as_slice() {
        [winner] => *winner,
        _ => OutcomeClass::Neutral,
    }
}

/// Content-address an annotation over everything that defines it.
fn derive_annotation_id(
    turn_ref: &str,
    intent_id: &str,
    judge_votes: &[JudgeVote],
    scored_at: DateTime<Utc>,
) -> Result<String> {
    #[derive(Serialize)]
    struct AnnotationAddress<'a> {
        turn_ref: &'a str,
        intent_id: &'a str,
        judge_votes: &'a [JudgeVote],
        scored_at: DateTime<Utc>,
    }
    let address = AnnotationAddress {
        turn_ref,
        intent_id,
        judge_votes,
        scored_at,
    };
    Ok(format!(
        "{ANNOTATION_ID_PREFIX}{}",
        sha256_hex(&serde_json::to_vec(&address)?)
    ))
}

// --------------------------------------------------------------------- //
// The ledger
// --------------------------------------------------------------------- //

/// The demand-side ledger: interaction events, intent assignments, gap
/// entries as projections of their mutation chains, and per-intent
/// outcome tallies. In-memory and fully serde-serializable; a server wave
/// persists the snapshot, it does not translate it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GapLedger {
    /// Snapshot format version — checked on load, fail-closed.
    pub format_version: u32,
    /// Events by id.
    events: BTreeMap<String, InteractionEvent>,
    /// Versioned intent assignments by event id, oldest first.
    assignments: BTreeMap<String, Vec<IntentAssignment>>,
    /// The current entry projection by gap id.
    entries: BTreeMap<String, GapLedgerEntry>,
    /// The mutation chains by gap id, oldest first. The store of record.
    mutations: BTreeMap<String, Vec<GapMutation>>,
    /// Per-intent behavioral tallies.
    tallies: BTreeMap<String, IntentTally>,
    /// Scored-turn annotations by id — the provenance-rich behavioral
    /// record the tallies summarize. Defaults to empty so snapshots
    /// written before annotations existed still load.
    #[serde(default)]
    annotations: BTreeMap<String, OutcomeAnnotation>,
}

impl Default for GapLedger {
    fn default() -> Self {
        Self::new()
    }
}

impl GapLedger {
    /// An empty ledger at the current format version.
    pub fn new() -> Self {
        Self {
            format_version: LEDGER_FORMAT_VERSION,
            events: BTreeMap::new(),
            assignments: BTreeMap::new(),
            entries: BTreeMap::new(),
            mutations: BTreeMap::new(),
            tallies: BTreeMap::new(),
            annotations: BTreeMap::new(),
        }
    }

    /// Restore a ledger from a persisted snapshot. Unknown format
    /// versions fail closed.
    pub fn from_snapshot(value: serde_json::Value) -> Result<Self> {
        let ledger: GapLedger = serde_json::from_value(value)?;
        if ledger.format_version != LEDGER_FORMAT_VERSION {
            return Err(GapError::UnsupportedFormat(ledger.format_version));
        }
        Ok(ledger)
    }

    /// Serialize the whole ledger as one snapshot value.
    pub fn to_snapshot(&self) -> Result<serde_json::Value> {
        Ok(serde_json::to_value(self)?)
    }

    // ------------------------- events ------------------------- //

    /// Record an interaction event. Re-recording the same source row is a
    /// no-op by identity: the content address converges, so a re-run
    /// connector cannot double-count demand — but a *different* event
    /// colliding on an existing id is a typed error, never a silent
    /// overwrite.
    pub fn record_event(&mut self, event: InteractionEvent) -> Result<String> {
        let id = event.event_id.clone();
        match self.events.get(&id) {
            Some(existing) if *existing == event => Ok(id),
            Some(_) => Err(GapError::EventExists(id)),
            None => {
                self.events.insert(id.clone(), event);
                Ok(id)
            }
        }
    }

    /// Read an event.
    pub fn event(&self, event_id: &str) -> Option<&InteractionEvent> {
        self.events.get(event_id)
    }

    /// All recorded events, in id order — the corpus induction mines.
    pub fn events(&self) -> impl Iterator<Item = &InteractionEvent> + '_ {
        self.events.values()
    }

    /// Assign an intent to an event, appending to the versioned history.
    pub fn assign_intent(
        &mut self,
        event_id: &str,
        intent_id: impl Into<String>,
        assigner: impl Into<String>,
        at: DateTime<Utc>,
    ) -> Result<()> {
        if !self.events.contains_key(event_id) {
            return Err(GapError::UnknownEvent(event_id.to_string()));
        }
        let intent_id = intent_id.into();
        check_field("intent_id", &intent_id, MAX_STATEMENT_BYTES)?;
        let assigner = assigner.into();
        check_field("assigner", &assigner, MAX_STATEMENT_BYTES)?;
        self.assignments
            .entry(event_id.to_string())
            .or_default()
            .push(IntentAssignment {
                event_id: event_id.to_string(),
                intent_id,
                assigned_at: at,
                assigner,
            });
        Ok(())
    }

    /// The event's current intent: the latest assignment, if any.
    pub fn current_intent(&self, event_id: &str) -> Option<&str> {
        self.assignments
            .get(event_id)
            .and_then(|history| history.last())
            .map(|assignment| assignment.intent_id.as_str())
    }

    /// Attach evidence to an existing entry without touching its demand
    /// tallies — a zero-volume `Reinforced` mutation. The hunting loop
    /// uses this to document what a hunt found (a contradiction, a
    /// deliverable) on the entry itself, so the ledger stays the one
    /// place that knows why a gap moved.
    pub fn add_evidence(
        &mut self,
        gap_id: &str,
        evidence: Vec<Citation>,
        actor: &str,
        at: DateTime<Utc>,
    ) -> Result<()> {
        if !self.entries.contains_key(gap_id) {
            return Err(GapError::UnknownGap(gap_id.to_string()));
        }
        if evidence.is_empty() {
            return Err(GapError::EmptyEvidence);
        }
        self.append_mutation(
            gap_id,
            GapMutationKind::Reinforced {
                added_volume: 0,
                added_failure_cost_millis: 0,
                new_evidence: evidence,
            },
            actor,
            at,
        )
        .map(|_| ())
    }

    // ------------------------- filing ------------------------- //

    /// File a gap, or reinforce the matching open one. Returns the gap id
    /// either way. The dedupe rule: same `(subject, statement, origin)`
    /// converges on one content address, so a second filing against an
    /// actionable entry adds volume, failure-cost, and evidence to it; a
    /// filing against a *closed* entry reopens it — the ledger never
    /// forgets a gap closed on paper but not in practice.
    #[allow(clippy::too_many_arguments)]
    pub fn file_gap(
        &mut self,
        subject: GapSubject,
        statement: impl Into<String>,
        evidence: Vec<Citation>,
        origin: GapOrigin,
        closure_criteria: ClosureCriteria,
        volume: u64,
        failure_cost_millis: u64,
        actor: &str,
        at: DateTime<Utc>,
    ) -> Result<String> {
        let statement = statement.into();
        check_field("statement", &statement, MAX_STATEMENT_BYTES)?;
        check_field("actor", actor, MAX_STATEMENT_BYTES)?;
        if evidence.is_empty() {
            return Err(GapError::EmptyEvidence);
        }
        let gap_id = derive_gap_id(&subject, &statement, &origin)?;

        match self.entries.get(&gap_id).map(|entry| entry.status) {
            // An actionable entry exists: reinforce it.
            Some(
                GapStatus::Open
                | GapStatus::Hunting
                | GapStatus::TrialPending
                | GapStatus::BlockedOnBusiness
                | GapStatus::Parked
                | GapStatus::Reopened,
            ) => {
                self.append_mutation(
                    &gap_id,
                    GapMutationKind::Reinforced {
                        added_volume: volume,
                        added_failure_cost_millis: failure_cost_millis,
                        new_evidence: evidence,
                    },
                    actor,
                    at,
                )?;
                Ok(gap_id)
            }
            // A closed entry: the gap is back. Reopen with the new
            // evidence attached.
            Some(GapStatus::Closed) => {
                self.append_mutation(
                    &gap_id,
                    GapMutationKind::Reinforced {
                        added_volume: volume,
                        added_failure_cost_millis: failure_cost_millis,
                        new_evidence: evidence,
                    },
                    actor,
                    at,
                )?;
                self.append_mutation(
                    &gap_id,
                    GapMutationKind::Reopened {
                        reason: "new demand filed against a closed gap".to_string(),
                    },
                    actor,
                    at,
                )?;
                Ok(gap_id)
            }
            // New gap: the Filed mutation carries the whole filing
            // payload, so the chain alone rebuilds the entry.
            None => {
                let observed = !origin.is_speculative();
                self.append_mutation(
                    &gap_id,
                    GapMutationKind::Filed {
                        subject,
                        statement,
                        evidence,
                        origin,
                        closure_criteria,
                        volume,
                        failure_cost_millis,
                        observed,
                    },
                    actor,
                    at,
                )?;
                Ok(gap_id)
            }
        }
    }

    /// File from an escalation event: a human had to resolve what the
    /// agent could not. The subject is the event's current intent, or a
    /// question shape from its utterance when clustering has not claimed
    /// it yet.
    pub fn file_escalation(
        &mut self,
        event_id: &str,
        statement: impl Into<String>,
        closure_criteria: ClosureCriteria,
        failure_cost_millis: u64,
        actor: &str,
        at: DateTime<Utc>,
    ) -> Result<String> {
        let (subject, citation) = self.subject_and_citation_for_event(event_id)?;
        self.file_gap(
            subject,
            statement,
            vec![citation],
            GapOrigin::RuntimeEscalation,
            closure_criteria,
            1,
            failure_cost_millis,
            actor,
            at,
        )
    }

    /// File from an operator or user correction.
    pub fn file_correction(
        &mut self,
        event_id: &str,
        statement: impl Into<String>,
        closure_criteria: ClosureCriteria,
        failure_cost_millis: u64,
        actor: &str,
        at: DateTime<Utc>,
    ) -> Result<String> {
        let (subject, citation) = self.subject_and_citation_for_event(event_id)?;
        self.file_gap(
            subject,
            statement,
            vec![citation],
            GapOrigin::RuntimeCorrection,
            closure_criteria,
            1,
            failure_cost_millis,
            actor,
            at,
        )
    }

    /// File from a memory recall that returned nothing. A zero-recall
    /// lookup is not merely a miss to log — it is evidence of a question
    /// the declared schema did not anticipate.
    pub fn file_zero_recall(
        &mut self,
        query: &str,
        closure_criteria: ClosureCriteria,
        failure_cost_millis: u64,
        actor: &str,
        at: DateTime<Utc>,
    ) -> Result<String> {
        let subject = GapSubject::question_shape(query)?;
        let citation = Citation::new(
            CitationKind::MemoryRecord,
            format!("zero-recall:{}", subject_key(&subject)),
            Some("recall returned no rows for this query".to_string()),
        )?;
        self.file_gap(
            subject,
            format!("No recalled knowledge answers: {query}"),
            vec![citation],
            GapOrigin::ZeroRecall,
            closure_criteria,
            1,
            failure_cost_millis,
            actor,
            at,
        )
    }

    /// File a speculative entry from frontier expansion. The adjacency
    /// edge that justified the widening is the citation — which source
    /// spoke, and which edge, stays in the record.
    #[allow(clippy::too_many_arguments)]
    pub fn open_speculative(
        &mut self,
        subject: GapSubject,
        statement: impl Into<String>,
        adjacency: AdjacencySource,
        edge_citation: Citation,
        closure_criteria: ClosureCriteria,
        actor: &str,
        at: DateTime<Utc>,
    ) -> Result<String> {
        if edge_citation.kind != CitationKind::AdjacencyEdge {
            return Err(GapError::ClosureUnsatisfied {
                gap: "(unfiled)".to_string(),
                reason: "a speculative entry must cite the adjacency edge that justified it"
                    .to_string(),
            });
        }
        self.file_gap(
            subject,
            statement,
            vec![edge_citation],
            GapOrigin::Speculative { adjacency },
            closure_criteria,
            0,
            0,
            actor,
            at,
        )
    }

    /// Resolve an event into its filing subject and citation.
    fn subject_and_citation_for_event(&self, event_id: &str) -> Result<(GapSubject, Citation)> {
        let event = self
            .events
            .get(event_id)
            .ok_or_else(|| GapError::UnknownEvent(event_id.to_string()))?;
        let subject = match self.current_intent(event_id) {
            Some(intent_id) => GapSubject::Intent {
                intent_id: intent_id.to_string(),
            },
            None => GapSubject::question_shape(&event.utterance)?,
        };
        let citation = Citation::new(CitationKind::InteractionEvent, event_id, None)?;
        Ok((subject, citation))
    }

    // ------------------------- the status machine ------------------------- //

    /// Move an entry through the validated status machine. The machine is
    /// small on purpose: the interesting decisions live in closure
    /// evaluation and probe validation, not in transition trivia.
    pub fn transition(
        &mut self,
        gap_id: &str,
        to: GapStatus,
        actor: &str,
        at: DateTime<Utc>,
    ) -> Result<()> {
        let entry = self
            .entries
            .get(gap_id)
            .ok_or_else(|| GapError::UnknownGap(gap_id.to_string()))?;
        let from = entry.status;

        // Speculation cannot reach the hunting queue unvalidated: a
        // speculative entry that no demand probe has confirmed cannot
        // leave Open for Hunting.
        if to == GapStatus::Hunting
            && entry.origin.is_speculative()
            && !entry.observed
            && from != GapStatus::Reopened
        {
            return Err(GapError::UnvalidatedSpeculation(gap_id.to_string()));
        }
        // Parked is the speculative decay state; observed gaps never park.
        if to == GapStatus::Parked && !entry.origin.is_speculative() {
            return Err(GapError::NotSpeculative(gap_id.to_string()));
        }
        // Closure goes through evaluate_closure — a bare transition to
        // Closed would be closure without criteria.
        if to == GapStatus::Closed {
            return Err(GapError::IllegalTransition { from, to });
        }

        let legal = matches!(
            (from, to),
            (GapStatus::Open, GapStatus::Hunting)
                | (GapStatus::Open, GapStatus::BlockedOnBusiness)
                | (GapStatus::Open, GapStatus::Parked)
                | (GapStatus::Hunting, GapStatus::TrialPending)
                | (GapStatus::Hunting, GapStatus::Open)
                | (GapStatus::Hunting, GapStatus::BlockedOnBusiness)
                | (GapStatus::TrialPending, GapStatus::Open)
                | (GapStatus::BlockedOnBusiness, GapStatus::Open)
                | (GapStatus::Parked, GapStatus::Open)
                | (GapStatus::Reopened, GapStatus::Hunting)
                | (GapStatus::Reopened, GapStatus::BlockedOnBusiness)
        );
        if !legal {
            return Err(GapError::IllegalTransition { from, to });
        }
        self.append_mutation(
            gap_id,
            GapMutationKind::StatusChanged { from, to },
            actor,
            at,
        )
        .map(|_| ())
    }

    /// Record a demand/supply probe against a speculative entry. Demand
    /// found promotes the entry to observed — it enters the ordinary
    /// priority queue with the probe as added citation. An empty probe
    /// parks the entry under a decay clock; enough empty probes and
    /// [`GapLedger::expire_parked`] closes it.
    pub fn record_probe(
        &mut self,
        gap_id: &str,
        demand_hits: u64,
        supply_covered: bool,
        actor: &str,
        at: DateTime<Utc>,
    ) -> Result<()> {
        let entry = self
            .entries
            .get(gap_id)
            .ok_or_else(|| GapError::UnknownGap(gap_id.to_string()))?;
        if !entry.origin.is_speculative() {
            return Err(GapError::ProbeOnObserved(gap_id.to_string()));
        }
        self.append_mutation(
            gap_id,
            GapMutationKind::ProbeResultRecorded {
                demand_hits,
                supply_covered,
            },
            actor,
            at,
        )?;
        if demand_hits > 0 {
            let citation = Citation::new(
                CitationKind::ProbeResult,
                format!("probe:{gap_id}:demand:{demand_hits}"),
                Some(format!(
                    "demand probe found {demand_hits} matching events; supply_covered={supply_covered}"
                )),
            )?;
            self.append_mutation(
                gap_id,
                GapMutationKind::Reinforced {
                    added_volume: demand_hits,
                    added_failure_cost_millis: 0,
                    new_evidence: vec![citation],
                },
                actor,
                at,
            )?;
            // A parked entry that demand found re-enters the queue.
            if self.entries[gap_id].status == GapStatus::Parked {
                self.append_mutation(
                    gap_id,
                    GapMutationKind::StatusChanged {
                        from: GapStatus::Parked,
                        to: GapStatus::Open,
                    },
                    actor,
                    at,
                )?;
            }
        } else if self.entries[gap_id].status != GapStatus::Parked {
            let from = self.entries[gap_id].status;
            self.append_mutation(
                gap_id,
                GapMutationKind::StatusChanged {
                    from,
                    to: GapStatus::Parked,
                },
                actor,
                at,
            )?;
        }
        Ok(())
    }

    /// Close parked speculative entries whose decay clock has run out.
    /// After the n-th empty probe the grace interval is
    /// `base * 2^(n-1)` — each miss doubles the grace, so re-probes run
    /// at declining frequency — and entries past [`MAX_EMPTY_PROBES`]
    /// empty probes expire regardless. Expiry is a closure with the
    /// resolution `expired:no-demand` — the mutation log keeps the full
    /// history, so nothing is ever truly deleted.
    pub fn expire_parked(&mut self, now: DateTime<Utc>, actor: &str) -> Result<Vec<String>> {
        let expired: Vec<String> = self
            .entries
            .values()
            .filter(|entry| entry.status == GapStatus::Parked)
            .filter(|entry| {
                if entry.empty_probes >= MAX_EMPTY_PROBES {
                    return true;
                }
                if entry.empty_probes == 0 {
                    return false;
                }
                let grace_millis = PROBE_BACKOFF_BASE_MILLIS.saturating_mul(
                    1_i64
                        .checked_shl(entry.empty_probes - 1)
                        .unwrap_or(i64::MAX),
                );
                let deadline = entry.updated_at + chrono::Duration::milliseconds(grace_millis);
                now >= deadline
            })
            .map(|entry| entry.gap_id.clone())
            .collect();
        for gap_id in &expired {
            self.append_mutation(
                gap_id,
                GapMutationKind::Closed {
                    resolution: "expired:no-demand".to_string(),
                },
                actor,
                now,
            )?;
        }
        Ok(expired)
    }

    // ------------------------- closure and the behavioral signal ------------------------- //

    /// The evidence a closure check runs against.
    pub fn evaluate_closure(
        &mut self,
        gap_id: &str,
        evidence: &ClosureEvidence,
        actor: &str,
        at: DateTime<Utc>,
    ) -> Result<()> {
        let entry = self
            .entries
            .get(gap_id)
            .ok_or_else(|| GapError::UnknownGap(gap_id.to_string()))?;
        if entry.status == GapStatus::Closed {
            return Ok(());
        }
        let resolution = match (&entry.closure_criteria, evidence) {
            (
                ClosureCriteria::ArtifactPromoted { candidate_id },
                ClosureEvidence::ArtifactPromoted {
                    candidate_id: promoted,
                },
            ) if promoted == candidate_id => format!("candidate:{promoted}"),
            (
                ClosureCriteria::BlockFilled { block_label },
                ClosureEvidence::BlockFilled {
                    block_label: filled,
                },
            ) if filled == block_label => format!("block:{filled}"),
            (
                ClosureCriteria::FailureRateBelow { threshold_millis },
                ClosureEvidence::FailureRateMeasured,
            ) => {
                let intent =
                    entry
                        .subject
                        .intent_id()
                        .ok_or_else(|| GapError::ClosureUnsatisfied {
                            gap: gap_id.to_string(),
                            reason: "failure-rate criteria require an intent subject".to_string(),
                        })?;
                let rate = self
                    .tallies
                    .get(intent)
                    .and_then(IntentTally::failure_rate_millis)
                    .ok_or_else(|| GapError::ClosureUnsatisfied {
                        gap: gap_id.to_string(),
                        reason: "no measured outcomes for the intent yet".to_string(),
                    })?;
                if rate >= *threshold_millis as u64 {
                    return Err(GapError::ClosureUnsatisfied {
                        gap: gap_id.to_string(),
                        reason: format!(
                            "failure rate {rate} per mille >= threshold {threshold_millis}"
                        ),
                    });
                }
                format!("failure-rate:{rate}:below:{threshold_millis}")
            }
            (
                ClosureCriteria::BusinessDecisionRequired,
                ClosureEvidence::BusinessDecision { decision_ref },
            ) => format!("business-decision:{decision_ref}"),
            (criteria, _) => {
                return Err(GapError::ClosureUnsatisfied {
                    gap: gap_id.to_string(),
                    reason: format!("evidence does not match criteria {criteria:?}"),
                });
            }
        };
        self.append_mutation(gap_id, GapMutationKind::Closed { resolution }, actor, at)
            .map(|_| ())
    }

    /// Score one served turn's outcome against its intent.
    pub fn record_outcome(&mut self, intent_id: &str, outcome: OutcomeClass) {
        let tally = self.tallies.entry(intent_id.to_string()).or_default();
        match outcome {
            OutcomeClass::Accepted => tally.accepted += 1,
            OutcomeClass::Corrected => tally.corrected += 1,
            OutcomeClass::Redone => tally.redone += 1,
            OutcomeClass::Neutral => tally.neutral += 1,
        }
    }

    /// Record a scored turn — the provenance-rich path. Re-recording the
    /// same annotation is a no-op by identity (the content address
    /// converges); a collision with different content is a typed error.
    ///
    /// Recording a measurement evaluates failure-rate closure for every
    /// un-closed entry on the annotation's intent in the same mutation:
    /// a criterion the fresh numbers satisfy closes without human
    /// bookkeeping; a criterion they do not is the measurement not
    /// having moved enough yet, never an error.
    pub fn record_annotation(
        &mut self,
        annotation: OutcomeAnnotation,
        actor: &str,
        at: DateTime<Utc>,
    ) -> Result<RecordedAnnotation> {
        let id = annotation.annotation_id.clone();
        match self.annotations.get(&id) {
            Some(existing) if *existing == annotation => {
                return Ok(RecordedAnnotation {
                    annotation_id: id,
                    closed_gap_ids: Vec::new(),
                });
            }
            Some(_) => return Err(GapError::AnnotationExists(id)),
            None => {}
        }
        let intent_id = annotation.intent_id.clone();
        let outcome = annotation.outcome;
        self.annotations.insert(id.clone(), annotation);
        self.record_outcome(&intent_id, outcome);

        let candidates: Vec<String> = self
            .entries
            .values()
            .filter(|entry| entry.status != GapStatus::Closed)
            .filter(|entry| {
                matches!(
                    entry.closure_criteria,
                    ClosureCriteria::FailureRateBelow { .. }
                )
            })
            .filter(|entry| entry.subject.intent_id() == Some(intent_id.as_str()))
            .map(|entry| entry.gap_id.clone())
            .collect();
        let mut closed_gap_ids = Vec::new();
        for gap_id in candidates {
            match self.evaluate_closure(&gap_id, &ClosureEvidence::FailureRateMeasured, actor, at) {
                Ok(()) => closed_gap_ids.push(gap_id),
                Err(GapError::ClosureUnsatisfied { .. }) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(RecordedAnnotation {
            annotation_id: id,
            closed_gap_ids,
        })
    }

    /// Read an annotation.
    pub fn annotation(&self, annotation_id: &str) -> Option<&OutcomeAnnotation> {
        self.annotations.get(annotation_id)
    }

    /// An intent's outcome curve: every annotation against it, oldest
    /// first — the queryable per-intent efficacy record. "This skill cut
    /// this intent's correction rate from 31% to 9%" renders from this
    /// alone.
    pub fn outcome_curve(&self, intent_id: &str) -> Vec<&OutcomeAnnotation> {
        let mut curve: Vec<&OutcomeAnnotation> = self
            .annotations
            .values()
            .filter(|annotation| annotation.intent_id == intent_id)
            .collect();
        curve.sort_by(|a, b| {
            a.scored_at
                .cmp(&b.scored_at)
                .then_with(|| a.annotation_id.cmp(&b.annotation_id))
        });
        curve
    }

    /// An intent's current tally, if anything has been scored against it.
    pub fn tally(&self, intent_id: &str) -> Option<&IntentTally> {
        self.tallies.get(intent_id)
    }

    /// An intent's measured failure rate per mille, if it has been
    /// scored.
    pub fn failure_rate_millis(&self, intent_id: &str) -> Option<u64> {
        self.tallies
            .get(intent_id)
            .and_then(IntentTally::failure_rate_millis)
    }

    /// Reopen closed entries whose subject intent's measured failure rate
    /// meets or exceeds the threshold. This is the self-honesty pass:
    /// gaps that were closed on paper but whose numbers did not move come
    /// back, with the measurement as the reason.
    pub fn sweep_reopens(
        &mut self,
        threshold_millis: u32,
        actor: &str,
        at: DateTime<Utc>,
    ) -> Result<Vec<String>> {
        let reopen: Vec<(String, u64)> = self
            .entries
            .values()
            .filter(|entry| entry.status == GapStatus::Closed)
            .filter_map(|entry| {
                let intent = entry.subject.intent_id()?;
                let rate = self.failure_rate_millis(intent)?;
                (rate >= threshold_millis as u64).then(|| (entry.gap_id.clone(), rate))
            })
            .collect();
        for (gap_id, rate) in &reopen {
            self.append_mutation(
                gap_id,
                GapMutationKind::Reopened {
                    reason: format!(
                        "measured failure rate {rate} per mille >= threshold {threshold_millis}"
                    ),
                },
                actor,
                at,
            )?;
        }
        Ok(reopen.into_iter().map(|(gap_id, _)| gap_id).collect())
    }

    // ------------------------- rollback ------------------------- //

    /// Roll a gap back to a prior mutation. Appends a `RolledBack` link
    /// and re-folds the entry from the chain prefix ending at the target.
    /// The restore is exact because every mutation is content-addressed
    /// and immutable — the restored state is the state that was, not a
    /// reconstruction.
    pub fn rollback(
        &mut self,
        gap_id: &str,
        to_mutation_id: &str,
        actor: &str,
        at: DateTime<Utc>,
    ) -> Result<()> {
        let chain = self
            .mutations
            .get(gap_id)
            .ok_or_else(|| GapError::UnknownGap(gap_id.to_string()))?;
        if !chain.iter().any(|m| m.mutation_id == to_mutation_id) {
            return Err(GapError::UnknownMutation {
                gap: gap_id.to_string(),
                mutation: to_mutation_id.to_string(),
            });
        }
        self.append_mutation(
            gap_id,
            GapMutationKind::RolledBack {
                to_mutation_id: to_mutation_id.to_string(),
            },
            actor,
            at,
        )
        .map(|_| ())
    }

    // ------------------------- reads ------------------------- //

    /// Read an entry.
    pub fn entry(&self, gap_id: &str) -> Option<&GapLedgerEntry> {
        self.entries.get(gap_id)
    }

    /// All entries, in id order — the hunting loop's closure sweep scans
    /// this for entries whose criteria a promotion just satisfied.
    pub fn entries(&self) -> impl Iterator<Item = &GapLedgerEntry> + '_ {
        self.entries.values()
    }

    /// A gap's full mutation chain, oldest first.
    pub fn chain(&self, gap_id: &str) -> Option<&[GapMutation]> {
        self.mutations.get(gap_id).map(Vec::as_slice)
    }

    /// The hunting loop's standing work order: actionable entries
    /// (Open or Reopened, speculation validated) ranked by priority,
    /// ties broken by filing order. Speculative-unvalidated and parked
    /// entries never appear — the frontier cannot spend hunting budget
    /// on its own guesses.
    pub fn work_order(&self) -> Vec<&GapLedgerEntry> {
        let mut actionable: Vec<&GapLedgerEntry> = self
            .entries
            .values()
            .filter(|entry| matches!(entry.status, GapStatus::Open | GapStatus::Reopened))
            .filter(|entry| entry.observed)
            .collect();
        actionable.sort_by(|a, b| {
            b.priority_score()
                .cmp(&a.priority_score())
                .then_with(|| a.filed_at.cmp(&b.filed_at))
                .then_with(|| a.gap_id.cmp(&b.gap_id))
        });
        actionable
    }

    // ------------------------- internals ------------------------- //

    /// Append a mutation to a gap's chain and re-fold the projection.
    fn append_mutation(
        &mut self,
        gap_id: &str,
        kind: GapMutationKind,
        actor: &str,
        at: DateTime<Utc>,
    ) -> Result<String> {
        check_field("actor", actor, MAX_STATEMENT_BYTES)?;
        let chain = self.mutations.entry(gap_id.to_string()).or_default();
        let previous = chain.last().map(|m| m.mutation_id.clone());
        let mutation_id = derive_mutation_id(gap_id, previous.as_deref(), &kind, actor, at)?;
        chain.push(GapMutation {
            mutation_id: mutation_id.clone(),
            gap_id: gap_id.to_string(),
            previous,
            kind,
            actor: actor.to_string(),
            at,
        });
        if let Some(entry) = fold_chain(self.mutations[gap_id].as_slice()) {
            self.entries.insert(gap_id.to_string(), entry);
        }
        Ok(mutation_id)
    }
}

/// The evidence a closure evaluation checks against, mirroring
/// [`ClosureCriteria`]'s variants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClosureEvidence {
    /// A candidate promoted through the learning gate.
    ArtifactPromoted {
        /// The promoted candidate's id.
        candidate_id: String,
    },
    /// A declared memory block was filled.
    BlockFilled {
        /// The filled block's label.
        block_label: String,
    },
    /// The intent's failure rate was measured; the check reads the
    /// ledger's own tallies.
    FailureRateMeasured,
    /// The business decided.
    BusinessDecision {
        /// Reference to the decision record.
        decision_ref: String,
    },
}

/// Fold a mutation chain into its entry projection. `RolledBack`
/// re-folds from the target prefix, so rollback is exact by construction.
fn fold_chain(chain: &[GapMutation]) -> Option<GapLedgerEntry> {
    let mut entry: Option<GapLedgerEntry> = None;
    for mutation in chain {
        match &mutation.kind {
            GapMutationKind::Filed {
                subject,
                statement,
                evidence,
                origin,
                closure_criteria,
                volume,
                failure_cost_millis,
                observed,
            } => {
                entry = Some(GapLedgerEntry {
                    gap_id: mutation.gap_id.clone(),
                    subject: subject.clone(),
                    statement: statement.clone(),
                    evidence: evidence.clone(),
                    origin: *origin,
                    volume: *volume,
                    failure_cost_millis: *failure_cost_millis,
                    closure_criteria: closure_criteria.clone(),
                    status: GapStatus::Open,
                    resolution: None,
                    observed: *observed,
                    empty_probes: 0,
                    filed_at: mutation.at,
                    updated_at: mutation.at,
                });
            }
            GapMutationKind::Reinforced {
                added_volume,
                added_failure_cost_millis,
                new_evidence,
            } => {
                if let Some(entry) = entry.as_mut() {
                    entry.volume = entry.volume.saturating_add(*added_volume);
                    entry.failure_cost_millis = entry
                        .failure_cost_millis
                        .saturating_add(*added_failure_cost_millis);
                    for citation in new_evidence {
                        if !entry.evidence.contains(citation)
                            && entry.evidence.len() < MAX_EVIDENCE_PER_ENTRY
                        {
                            entry.evidence.push(citation.clone());
                        }
                    }
                    entry.updated_at = mutation.at;
                }
            }
            GapMutationKind::StatusChanged { to, .. } => {
                if let Some(entry) = entry.as_mut() {
                    entry.status = *to;
                    entry.updated_at = mutation.at;
                }
            }
            GapMutationKind::ProbeResultRecorded {
                demand_hits,
                supply_covered: _,
            } => {
                if let Some(entry) = entry.as_mut() {
                    if *demand_hits > 0 {
                        entry.observed = true;
                        entry.empty_probes = 0;
                    } else {
                        entry.empty_probes = entry.empty_probes.saturating_add(1);
                    }
                    entry.updated_at = mutation.at;
                }
            }
            GapMutationKind::Closed { resolution } => {
                if let Some(entry) = entry.as_mut() {
                    entry.status = GapStatus::Closed;
                    entry.resolution = Some(resolution.clone());
                    entry.updated_at = mutation.at;
                }
            }
            GapMutationKind::Reopened { .. } => {
                if let Some(entry) = entry.as_mut() {
                    entry.status = GapStatus::Reopened;
                    entry.resolution = None;
                    entry.updated_at = mutation.at;
                }
            }
            GapMutationKind::RolledBack { to_mutation_id } => {
                let prefix_end = chain
                    .iter()
                    .position(|m| &m.mutation_id == to_mutation_id)
                    .map(|idx| idx + 1);
                if let Some(end) = prefix_end {
                    // Re-fold the prefix: `Filed` rebuilds the seed, so
                    // the restore is exact with no state beside the log.
                    entry = fold_chain(&chain[..end]);
                }
            }
        }
    }
    entry
}

/// A stable string key for a subject — used for zero-recall citation ids.
fn subject_key(subject: &GapSubject) -> String {
    match subject {
        GapSubject::Intent { intent_id } => format!("intent:{intent_id}"),
        GapSubject::QuestionShape { text } => format!("question:{text}"),
    }
}

/// Shared field validation: non-empty after trim, within the byte bound.
fn check_field(field: &'static str, value: &str, max: usize) -> Result<()> {
    if value.trim().is_empty() {
        return Err(GapError::EmptyField(field));
    }
    if value.len() > max {
        return Err(GapError::FieldTooLong {
            field,
            len: value.len(),
            max,
        });
    }
    Ok(())
}
