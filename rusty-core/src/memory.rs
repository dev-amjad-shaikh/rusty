//! Governed memory (R0.8 Rusty Learn, waves 1–2): the record model, the
//! structured retrieval contract with its token-bounded deterministic
//! assembly, the journaled read/write seam, and — from wave 2 — the
//! correction loop and the memory operations (consolidation, conflict
//! detection, forgetting) as journaled, evidence-carrying transitions.
//!
//! The design doc is `docs/learn-design.md` ("Governed memory"); the
//! learning rule it answers to: **no learning process may silently rewrite a
//! production prompt, graph, policy, memory, or tool permission.** Memory is
//! therefore a governed record store, not a JSON blob an agent edits through
//! a tool call:
//!
//! - [`MemoryRecord`] — one serde-versioned, content-addressed record:
//!   identity is `sha256` over content plus provenance, so a changed record
//!   is a new id and there is no in-place update anywhere in the model.
//!   Supersession is a chain of immutable records; the superseded record is
//!   retained as evidence but filtered from default retrieval.
//! - [`MemoryScope`] — the five-scope taxonomy: `run` / `agent` / `team` /
//!   `user` / `tenant`. It maps onto R0.7's four [`crate::agents::StateScope`]s
//!   with one honest adaptation, stated exactly as the design draws it:
//!   `StateScope` has no `run` member, because state outlives runs by
//!   design; run scope is new here (memory whose lifetime is bound to one
//!   run's thread); and **`agent` scope is `StateScope::Private` under its
//!   memory name**. The taxonomy is a superset, not a fork: an agent
//!   manifest's declared `StateScope`s translate one-to-one into the memory
//!   scopes it may write ([`MemoryScope::from_state_scope`]).
//! - [`MemoryProvenance`] — who wrote it (`agent:{id}` / `human:{id}` /
//!   `distiller:{name}` / `system`), from what evidence (run id + journal
//!   event ids, a correction id, a candidate id, the source record ids of a
//!   consolidation), and when. Mandatory: a record that cannot name its
//!   origin cannot be audited.
//! - [`MemoryQuery`] + [`ContextBudget`] — deliberately structural
//!   retrieval (scope, kind, key/tag equality, validity-at-time, minimum
//!   confidence, exclude-expired, exclude-superseded, authored-by; **no
//!   similarity search** in R0.8), packed into a prompt assembly by a
//!   deterministic rank (explicit priority, then confidence, then recency,
//!   with the content address as the final tie-break) until the token budget
//!   is exhausted.
//!
//! # The journaled seam
//!
//! A memory read is an [`Effect::ReadOnly`] effect journaled as
//! [`RunEventKind::MemoryRead`], with the resolved query plus budget as the
//! input payload and the assembly itself — the record ids and their order,
//! plus the token accounting — as the output payload. Two properties the
//! design requires follow from this module's discipline:
//!
//! - **Determinism.** Equal store state and equal budget produce byte-equal
//!   assemblies ([`assemble`] is a pure function with a total order), and
//!   the resolved query — including the `as_of` timestamp, stamped through
//!   the run's clock when unset — travels in the journaled request, so the
//!   request hash fully determines the result.
//! - **Replay-serving.** Exact replay serves the journaled assembly instead
//!   of re-querying the store, per the rule the Flight Recorder already
//!   applies to journaled model and tool calls. [`MemoryReplaySource`] is
//!   the serving cursor (sequence + request hash, the same matching rule
//!   [`crate::replay::ReplaySource`] applies to model/tool calls), and
//!   [`JournaledMemory::read`] re-journals the served event byte-identically
//!   — including clock-read parity with the live path, so a logical clock's
//!   tick sequence stays aligned with the recorded run.
//!
//! A memory write is an [`Effect::Idempotent`] effect under the derived key
//! `memory:{scope}:{memory_id}` ([`memory_effect_key`]): retried submissions
//! converge, and the write is journaled as [`RunEventKind::MemoryWrite`]
//! with its provenance. Writes are never served during replay — a replayed
//! run issuing a write has diverged from its evidence.
//!
//! # The correction loop and the memory operations (wave 2)
//!
//! Wave 2 lands the correction loop's record-plane half: [`Correction`] is
//! the highest-trust input the learning system has, so the loop's rule is
//! **a correction becomes an attributed candidate memory or example — never
//! an in-place rewrite of what it corrects**. The derived record carries
//! `human:{author}` provenance with the correction id in evidence (the
//! design's `human:{author} via correction:{id}` attribution), confidence
//! 1.0, and — at agent scope or wider — a [`Candidacy`] mark: candidacy,
//! not adoption, because a wrong human correction at tenant scope is a
//! production incident with a name attached and evaluation is cheap
//! insurance. Run scope adopts directly: it affects only the run that
//! produced it. Same-key correction-sourced writes auto-supersede the prior
//! record (open question 5's asymmetry: corrections are trusted because
//! they are attributed; distillations are not because they are inferred).
//!
//! The three memory operations are journaled transitions over the store,
//! never background daemons:
//!
//! - **Consolidation** — [`consolidation_summary`] builds the one `summary`
//!   record that distills N sources: it names them in
//!   [`MemoryEvidence::source_memory_ids`] (which is also what makes
//!   dependent-summary invalidation computable on forgetting) and
//!   supersedes them — [`apply_query`] treats a summary's sources as
//!   superseded, so default retrieval serves the summary alone. The
//!   distillation semantics stay with the caller; the runtime owns the
//!   record's invariants.
//! - **Conflict detection** — [`detect_conflicts`] flags live records that
//!   share a key, overlap in validity, and carry contradictory content.
//!   It flags ([`MemoryConflict`], a review item); it never resolves.
//! - **Forgetting** — [`plan_forget`] computes the erasure before anything
//!   is deleted: the targets plus the dependent summaries invalidated by
//!   walking the source naming in reverse, transitively. Deletion is real
//!   (derived state is erasable; run journals are hash-chained evidence and
//!   are not — open question 4), and the receipt is the journaled
//!   [`RunEventKind::MemoryForget`] tombstone: [`MemoryForgetTombstone`]
//!   carries the id, scope, reason, and dependent invalidations — metadata
//!   by construction, with no content field a careless serializer could
//!   leak the forgotten bytes through.
//!
//! # Token accounting
//!
//! The budget is enforced in **estimated tokens: serialized content bytes ÷
//! 4, plus a declared safety margin** ([`ContextBudget::margin_percent`],
//! default [`DEFAULT_TOKEN_MARGIN_PERCENT`]), recorded as such on every
//! assembly ([`TokenAccounting`]). This is the design's open-question-3
//! default, stated plainly: provider-precise tokenizers differ per model and
//! are heavyweight; model-precise counting plugs in later behind the same
//! [`ContextBudget`] type, and until then the margin is the honest hedge.
//!
//! Golden-file tests under `tests/golden/` pin every wire shape in this
//! module; any accidental contract drift fails CI.

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::agents::StateScope;
use crate::error::{Result, RustyError};
use crate::journal::{EventDraft, Journal, JournalSnapshot};
use crate::record::{
    sha256_hex, ArtifactRef, Effect, PayloadRef, RunEvent, RunEventKind, INLINE_PAYLOAD_MAX_BYTES,
};
use crate::replay::ServedEffect;

/// The memory schema version a run pins in its manifest
/// ([`crate::record::RunManifest::memory_schema`]). Bump only on a breaking
/// change to the record model; additive evolution uses serde defaults so
/// previously written records keep deserializing.
pub const MEMORY_SCHEMA_VERSION: &str = "memory-v1";

/// The divisor of the estimated-token accounting: estimated tokens are
/// serialized content bytes ÷ `TOKEN_BYTES_PER_ESTIMATE`, rounded up, before
/// the safety margin. See the module docs for why the estimate — not a
/// provider-precise tokenizer — is the R0.8 default.
pub const TOKEN_BYTES_PER_ESTIMATE: u64 = 4;

/// The default safety margin of the estimated-token accounting, in percent
/// (design open question 3's "declared safety margin"): a record's estimated
/// cost is `ceil(bytes / 4) * (1 + margin)`, so tokenizer drift against the
/// estimate eats the margin before it eats the budget.
pub const DEFAULT_TOKEN_MARGIN_PERCENT: u32 = 20;

fn invalid(message: impl Into<String>) -> RustyError {
    // A memory write is a state update to the governed store; contract
    // validation failures reuse the invalid-update class rather than
    // growing the error taxonomy for one module.
    RustyError::InvalidUpdate(message.into())
}

fn replay_error(message: impl Into<String>) -> RustyError {
    RustyError::Replay(message.into())
}

// --------------------------------------------------------------------- //
// The record model
// --------------------------------------------------------------------- //

/// What a memory record is. Closed enum — retrieval and consolidation match
/// exhaustively on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    /// An asserted fact about the world the agent operates in.
    Fact,
    /// A stated preference (a user's, a team's), meant to shape future
    /// behavior within its scope.
    Preference,
    /// A corrected input/output pair — the correction loop's output (R0.8
    /// wave 2): the input a run saw and the behavior it should have
    /// produced.
    Example,
    /// Consolidation's output: one record distilling N sources, which it
    /// names in [`MemoryEvidence::source_memory_ids`] — the naming is what
    /// makes dependent-summary invalidation computable on forgetting.
    Summary,
}

/// The governance state of a correction-derived record (R0.8 wave 2).
/// Closed enum with the single honest state this wave needs — wave 3's
/// evaluation outcomes extend it additively, the same evolution rule
/// [`RunEventKind`] follows. Not part of the content address: candidacy is
/// governance state, not identity — the same reading `tags` and `priority`
/// already take.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Candidacy {
    /// Attributed and stored, but unevaluated. The design's scope-decides-
    /// the-path rule: a correction at agent scope or wider becomes a
    /// candidate, because a wrong human correction at tenant scope is a
    /// production incident with a name attached, and the evaluation step is
    /// cheap insurance against it.
    Pending,
}

/// The scope a memory lives at: whose memory it is and how far its
/// visibility reaches. Closed enum; the superset mapping onto R0.7's
/// [`StateScope`] taxonomy is the module's load-bearing contract (see the
/// module docs and [`MemoryScope::from_state_scope`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScope {
    /// Memory whose lifetime is bound to one run's thread. New in R0.8:
    /// `StateScope` has no `run` member, because state outlives runs by
    /// design. Run scope is written by the runtime on the run's behalf.
    Run,
    /// One agent's own memory — `StateScope::Private` under its memory
    /// name. Scope authorization is the manifest check: an agent may write
    /// agent scope only when its `CapabilityManifest` declares
    /// `StateScope::Private`.
    Agent,
    /// Memory shared by a team's members — `StateScope::Team` under its
    /// memory name, under the same turn discipline that governs team state.
    Team,
    /// One end user's memory, shared across that user's agents and threads —
    /// `StateScope::User` under its memory name.
    User,
    /// Configuration-grade memory for the whole tenant — `StateScope::Tenant`
    /// under its memory name. Writable by operators, not by agents; tenant
    /// isolation is the `{tenant}/` id-namespacing unchanged.
    Tenant,
}

impl MemoryScope {
    /// The memory scope a declared [`StateScope`] translates into, one-to-one
    /// (`private` → `agent`): the manifest check's mapping. `None` is
    /// unreachable — the four `StateScope`s all have memory names — but the
    /// signature keeps the mapping honest about direction: this is the
    /// superset reading the subset.
    pub fn from_state_scope(scope: StateScope) -> Option<MemoryScope> {
        Some(match scope {
            StateScope::Private => MemoryScope::Agent,
            StateScope::Team => MemoryScope::Team,
            StateScope::User => MemoryScope::User,
            StateScope::Tenant => MemoryScope::Tenant,
        })
    }

    /// The [`StateScope`] this memory scope corresponds to, when it has one:
    /// `agent` → `private`, `team` → `team`, `user` → `user`,
    /// `tenant` → `tenant`, and `run` → `None` — run scope has no
    /// `StateScope` member because state outlives runs by design.
    pub fn to_state_scope(self) -> Option<StateScope> {
        match self {
            MemoryScope::Run => None,
            MemoryScope::Agent => Some(StateScope::Private),
            MemoryScope::Team => Some(StateScope::Team),
            MemoryScope::User => Some(StateScope::User),
            MemoryScope::Tenant => Some(StateScope::Tenant),
        }
    }
}

/// A [`MemoryScope`] plus the concrete scope id (agent id, user id, team id,
/// run id, or the tenant name). Scope ids are tenant-relative names: the
/// store namespaces every record under its tenant, so an id never needs to
/// re-encode the tenant it belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ScopeAddress {
    /// The taxonomy member.
    pub scope: MemoryScope,

    /// The concrete id inside the scope (e.g. `researcher-7` for
    /// `agent`-scoped memory).
    pub id: String,
}

impl ScopeAddress {
    /// Build an address; validation of the id happens at the write gate,
    /// not here (addresses also arrive from stored evidence).
    pub fn new(scope: MemoryScope, id: impl Into<String>) -> Self {
        Self {
            scope,
            id: id.into(),
        }
    }

    /// The canonical `"{scope}:{id}"` string form, used in derived effect
    /// keys (`memory:{scope}:{memory_id}`) and human-facing provenance.
    pub fn as_address(&self) -> String {
        let scope = serde_json::to_value(self.scope)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_else(|| format!("{:?}", self.scope).to_lowercase());
        format!("{scope}:{}", self.id)
    }
}

/// Who wrote a memory record. Closed enum: a record that cannot name its
/// origin cannot be audited, so provenance is mandatory, not optional — and
/// the attribution travels with the record through every later consumer
/// (distiller, evaluator, auditor).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProvenanceAuthor {
    /// An agent wrote it (in a governed write inside a run): `agent:{id}`.
    Agent {
        /// The writing agent's id (tenant-relative).
        agent_id: String,
    },
    /// A human wrote it — a correction, the highest-trust input the learning
    /// system has: `human:{id}`. Human-authored records default to
    /// confidence 1.0 (the claim is the person's, stated plainly).
    Human {
        /// The correcting human's identity. Mandatory because a correction
        /// that cannot name its corrector is indistinguishable from a
        /// prompt edit.
        human_id: String,
    },
    /// A distiller produced it from recorded evidence: `distiller:{name}`.
    Distiller {
        /// The distiller's name (application code, not runtime code).
        name: String,
    },
    /// The runtime itself wrote it (e.g. a run-scope record written on the
    /// run's behalf): `system`.
    System,
}

impl ProvenanceAuthor {
    /// The canonical id string (`agent:{id}` / `human:{id}` /
    /// `distiller:{name}` / `system`), as the design's provenance
    /// vocabulary spells it.
    pub fn as_id_string(&self) -> String {
        match self {
            ProvenanceAuthor::Agent { agent_id } => format!("agent:{agent_id}"),
            ProvenanceAuthor::Human { human_id } => format!("human:{human_id}"),
            ProvenanceAuthor::Distiller { name } => format!("distiller:{name}"),
            ProvenanceAuthor::System => "system".to_owned(),
        }
    }
}

/// What a memory record was derived from: the evidence an auditor walks.
/// Every field is optional and absent from the wire when unset — a
/// human-authored fact may carry no derivation at all, and sparse evidence
/// must not change the shape for dense readers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryEvidence {
    /// The run whose journal the record was distilled or corrected from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,

    /// The journal event ids inside `run_id` the record draws on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub event_ids: Vec<String>,

    /// The correction (R0.8 wave 2) the record was derived from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correction_id: Option<String>,

    /// The candidate (R0.8 wave 3) whose promotion carried this record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_id: Option<String>,

    /// The records a `summary` consolidated — the naming that makes
    /// dependent-summary invalidation computable on forgetting.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_memory_ids: Vec<String>,
}

impl MemoryEvidence {
    /// `true` when no evidence is carried (the sparse wire shape).
    pub fn is_empty(&self) -> bool {
        self.run_id.is_none()
            && self.event_ids.is_empty()
            && self.correction_id.is_none()
            && self.candidate_id.is_none()
            && self.source_memory_ids.is_empty()
    }
}

/// A record's origin: who wrote it, from what evidence, and when. Part of
/// the content address ([`MemoryRecord::new`]) — two records with identical
/// content but different origins are different records, because they answer
/// differently to an audit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryProvenance {
    /// Who wrote it.
    pub author: ProvenanceAuthor,

    /// What it was derived from. Absent from the wire when empty.
    #[serde(default, skip_serializing_if = "MemoryEvidence::is_empty")]
    pub evidence: MemoryEvidence,

    /// When the system learned it. Distinct from
    /// [`ValidityWindow::valid_from`] (when the record claims to be true) —
    /// the bitemporal split, kept as two plain timestamps (Zep's validity
    /// window as a flat field).
    pub written_at: DateTime<Utc>,
}

/// The interval the record claims to be true: `[valid_from, valid_until)`,
/// open-ended when `valid_until` is `None`. Contradiction is handled by
/// time (and supersession), never by deletion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidityWindow {
    /// Inclusive start of the claimed-true interval.
    pub valid_from: DateTime<Utc>,

    /// Exclusive end of the claimed-true interval; `None` = open-ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_until: Option<DateTime<Utc>>,
}

impl ValidityWindow {
    /// A window starting at `valid_from`, open-ended.
    pub fn starting(valid_from: DateTime<Utc>) -> Self {
        Self {
            valid_from,
            valid_until: None,
        }
    }

    /// Whether the window claims to be true at `t`: inclusive start,
    /// exclusive end.
    pub fn contains(&self, t: DateTime<Utc>) -> bool {
        self.valid_from <= t && self.valid_until.is_none_or(|until| t < until)
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_zero(value: &i64) -> bool {
    *value == 0
}

/// One governed memory record: content-addressed, immutable, scoped,
/// attributed.
///
/// Every field exists because a downstream operation needs it (the design's
/// rule): retrieval filters on scope/kind/key/tags/confidence/validity/
/// expiration/supersession/author; the assembly ranks on
/// priority/confidence/recency; the audit walks provenance; forgetting walks
/// `supersedes` in reverse. The one reserved field is `embedding` —
/// reserved so vector retrieval slots in additively when the roadmap's
/// de-prioritization lifts; nothing in R0.8 reads it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryRecord {
    /// The content address: `sha256` over the canonical serialization of the
    /// record's content plus provenance (the one hashing primitive shared
    /// with artifact references and journal heads). Immutable by
    /// construction: a changed record is a new id.
    pub memory_id: String,

    /// What the record is.
    pub kind: MemoryKind,

    /// Whose memory it is.
    pub scope: ScopeAddress,

    /// The writer-declared lookup key, when the record answers a named
    /// question ("what is user-7's timezone"). With no vector retrieval in
    /// R0.8, keying is deliberate: absence of a hit is absence of a key, not
    /// absence of a fact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,

    /// Writer-declared tags; retrieval matches by equality (a record must
    /// carry every queried tag).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,

    /// The writer-declared explicit priority — the assembly rank's first
    /// input (higher packs first). `0` is the default and stays absent from
    /// the wire, so pre-priority records keep their shape.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub priority: i64,

    /// Who wrote it, from what evidence, and when. Mandatory.
    pub provenance: MemoryProvenance,

    /// The writer-declared confidence in `(0, 1]`. Retrieval filters on it;
    /// nothing in the runtime *computes* it — confidence is a claim, not a
    /// measurement, and the model is honest about that.
    pub confidence: f64,

    /// The interval the record claims to be true.
    pub validity: ValidityWindow,

    /// When the system learned it (the bitemporal split's other timestamp;
    /// duplicates [`MemoryProvenance::written_at`] deliberately — the record
    /// is self-contained when provenance is summarized away by a consumer).
    pub created_at: DateTime<Utc>,

    /// Optional TTL. Expiration is a retrieval filter (and, from wave 2, a
    /// forgetting trigger), not a silent reaper.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,

    /// The record this one replaces, when it does. Supersession is a
    /// chain of immutable records; the superseded record is retained as
    /// evidence but filtered from default retrieval.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,

    /// Set when the record is a correction-derived candidate pending
    /// evaluation (R0.8 wave 2; [`Candidacy`]). Additive: absent from the
    /// wire while unset, and outside the content address (candidacy is
    /// governance state, not identity).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidacy: Option<Candidacy>,

    /// The record body: inline at or below [`INLINE_PAYLOAD_MAX_BYTES`],
    /// content-addressed above — the journal's own payload discipline, so
    /// memory bodies share artifact storage and the large-body story needs
    /// nothing new.
    pub content: PayloadRef,

    /// Reserved for vector retrieval (deferred, per the design's not-built
    /// list). Additive by construction: absent from the wire while unset,
    /// so the field lands without a wire change when it earns its keep.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<Value>,
}

impl MemoryRecord {
    /// Build a record, deriving its content address from the content plus
    /// provenance and splitting the content per the payload discipline
    /// (inline ≤ [`INLINE_PAYLOAD_MAX_BYTES`], content-addressed above).
    ///
    /// Fails when `confidence` is outside `(0, 1]` — the one validity rule
    /// the contract enforces at construction, because a confidence outside
    /// the interval is not a claim at all.
    pub fn new(
        kind: MemoryKind,
        scope: ScopeAddress,
        provenance: MemoryProvenance,
        confidence: f64,
        validity: ValidityWindow,
        created_at: DateTime<Utc>,
        content: Value,
    ) -> Result<Self> {
        if !(confidence > 0.0 && confidence <= 1.0) {
            return Err(invalid(format!(
                "memory confidence must be in (0, 1], got {confidence} — confidence is a \
                 writer-declared claim, and a value outside the interval is not a claim at all"
            )));
        }
        let content_ref = content_ref_of(&content);
        let memory_id = derive_memory_id(&content_ref, &provenance)?;
        Ok(Self {
            memory_id,
            kind,
            scope,
            key: None,
            tags: Vec::new(),
            priority: 0,
            provenance,
            confidence,
            validity,
            created_at,
            expires_at: None,
            supersedes: None,
            candidacy: None,
            content: content_ref,
            embedding: None,
        })
    }

    /// Builder-style: set the lookup key.
    pub fn with_key(mut self, key: impl Into<String>) -> Self {
        self.key = Some(key.into());
        self
    }

    /// Builder-style: set the tags.
    pub fn with_tags(mut self, tags: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.tags = tags.into_iter().map(Into::into).collect();
        self
    }

    /// Builder-style: set the explicit priority (the assembly rank's first
    /// input).
    pub fn with_priority(mut self, priority: i64) -> Self {
        self.priority = priority;
        self
    }

    /// Builder-style: set the TTL.
    pub fn with_expires_at(mut self, expires_at: DateTime<Utc>) -> Self {
        self.expires_at = Some(expires_at);
        self
    }

    /// Builder-style: name the record this one supersedes.
    pub fn with_supersedes(mut self, supersedes: impl Into<String>) -> Self {
        self.supersedes = Some(supersedes.into());
        self
    }

    /// Builder-style: mark the record as a correction-derived candidate
    /// (R0.8 wave 2; [`Candidacy`]).
    pub fn with_candidacy(mut self, candidacy: Candidacy) -> Self {
        self.candidacy = Some(candidacy);
        self
    }

    /// The serialized size of the content in bytes — the input to the
    /// estimated-token accounting. Inline payloads serialize on demand;
    /// artifact-referenced payloads carry their size on the reference (the
    /// canonical-serialization size the address was minted from).
    pub fn content_bytes(&self) -> u64 {
        match &self.content {
            PayloadRef::Inline(value) => serde_json::to_vec(value)
                .map(|bytes| bytes.len() as u64)
                .unwrap_or(0),
            PayloadRef::Artifact(reference) => reference.bytes,
        }
    }

    /// Split `content` per the payload discipline, as [`MemoryRecord::new`]
    /// does: inline at or below [`INLINE_PAYLOAD_MAX_BYTES`],
    /// content-addressed above. The artifact bytes are the canonical
    /// serialization of `content`; the caller persists them (the artifact
    /// store shared with the journal) under the reference's address.
    pub fn content_ref_of(content: &Value) -> PayloadRef {
        content_ref_of(content)
    }
}

/// The payload-discipline split, free-standing so both the record
/// constructor and store backends share exactly one decision.
fn content_ref_of(content: &Value) -> PayloadRef {
    let Ok(bytes) = serde_json::to_vec(content) else {
        // A Value that fails to serialize is a contradiction; keep it
        // inline rather than drop evidence (the journal's own rule).
        return PayloadRef::Inline(content.clone());
    };
    if bytes.len() <= INLINE_PAYLOAD_MAX_BYTES {
        return PayloadRef::Inline(content.clone());
    }
    PayloadRef::Artifact(ArtifactRef {
        sha256: sha256_hex(&bytes),
        bytes: bytes.len() as u64,
    })
}

/// The content address of a record: `sha256` over the canonical
/// serialization of `{content, provenance}` — content by hash (so inline
/// and artifact-referenced forms of the same body address identically) and
/// provenance in full (origin is identity).
pub fn derive_memory_id(content: &PayloadRef, provenance: &MemoryProvenance) -> Result<String> {
    let content_hash = content.content_hash()?;
    let identity = json!({ "content": content_hash, "provenance": provenance });
    Ok(sha256_hex(&serde_json::to_vec(&identity)?))
}

// --------------------------------------------------------------------- //
// Retrieval: structured filters + context budget
// --------------------------------------------------------------------- //

/// A structured memory query. Deliberately **not** semantic: R0.8 has no
/// similarity search (the design's not-built list), so writers key and tag
/// records deliberately and consumers treat absence of a hit as absence of
/// a key, not absence of a fact.
///
/// Every field is optional; an empty query matches everything in the
/// namespace (subject to the two defaults: expired and superseded records
/// are excluded unless explicitly included). The query is journaled as the
/// read's request, so its serialization is part of the replay contract —
/// sparse fields stay absent, and [`MemoryQuery::as_of`] is always resolved
/// before journaling (see [`JournaledMemory::read`]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MemoryQuery {
    /// Restrict to one scope address (whose memory, concretely).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<ScopeAddress>,

    /// Restrict to these kinds (empty = all kinds).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kinds: Vec<MemoryKind>,

    /// Key equality.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,

    /// Tag equality: a record must carry every listed tag.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,

    /// Validity-at-time: the record's validity window must contain this
    /// instant (the bitemporal "true as of" filter).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_at: Option<DateTime<Utc>>,

    /// Minimum writer-declared confidence, inclusive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_confidence: Option<f64>,

    /// Include records past their `expires_at` (default: excluded).
    /// Expiration is a filter, not a reaper — the records are still there.
    #[serde(default, skip_serializing_if = "is_false")]
    pub include_expired: bool,

    /// Include records another record supersedes (default: excluded — the
    /// superseded record is retained as evidence, filtered from default
    /// retrieval).
    #[serde(default, skip_serializing_if = "is_false")]
    pub include_superseded: bool,

    /// Restrict to one author.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authored_by: Option<ProvenanceAuthor>,

    /// Restrict to correction-derived candidates pending evaluation (R0.8
    /// wave 2; default: all records, candidates or not). This is how the
    /// evaluation half of the loop finds its queue: candidacy is a
    /// governance mark, so it filters like one — structurally, never by
    /// content.
    #[serde(default, skip_serializing_if = "is_false")]
    pub candidates_only: bool,

    /// The instant expiry is evaluated against. When unset, the reader
    /// stamps it through the run's clock at read time (wall clock for live
    /// reads; the deterministic logical clock for recorded runs) — so the
    /// journaled request always carries the resolved value and a replayed
    /// read re-derives the same one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub as_of: Option<DateTime<Utc>>,
}

impl MemoryQuery {
    /// Whether `record` matches every declared filter. `superseded` tells
    /// whether another record in the queried universe supersedes this one
    /// (computable only against the whole set, so the caller supplies it);
    /// `now` is the resolved expiry instant ([`MemoryQuery::as_of`]).
    pub fn matches(&self, record: &MemoryRecord, superseded: bool, now: DateTime<Utc>) -> bool {
        if let Some(scope) = &self.scope {
            if &record.scope != scope {
                return false;
            }
        }
        if !self.kinds.is_empty() && !self.kinds.contains(&record.kind) {
            return false;
        }
        if let Some(key) = &self.key {
            if record.key.as_deref() != Some(key.as_str()) {
                return false;
            }
        }
        if !self
            .tags
            .iter()
            .all(|tag| record.tags.iter().any(|t| t == tag))
        {
            return false;
        }
        if let Some(valid_at) = self.valid_at {
            if !record.validity.contains(valid_at) {
                return false;
            }
        }
        if let Some(min_confidence) = self.min_confidence {
            if record.confidence < min_confidence {
                return false;
            }
        }
        if !self.include_expired && record.expires_at.is_some_and(|expires| expires <= now) {
            return false;
        }
        if !self.include_superseded && superseded {
            return false;
        }
        if let Some(author) = &self.authored_by {
            if &record.provenance.author != author {
                return false;
            }
        }
        if self.candidates_only && record.candidacy.is_none() {
            return false;
        }
        true
    }
}

/// Apply `query` to a universe of records (one tenant's namespace), pure:
/// the superseded set is computed over the universe, then every declared
/// filter runs through [`MemoryQuery::matches`]. Both store backends share
/// this function — the JSON backend scans and applies it directly; Postgres
/// pre-filters on columns and applies the same function to the reduced set,
/// so filter semantics live in exactly one place.
///
/// The superseded set has two halves. A record is superseded when another
/// record names it in `supersedes` (the wave-1 rule) — **or, from wave 2,
/// when a `summary` record names it in
/// [`MemoryEvidence::source_memory_ids`]**: consolidation supersedes the
/// records it distills, and the naming is the same one dependent-summary
/// invalidation walks on forgetting.
pub fn apply_query(
    universe: &[MemoryRecord],
    query: &MemoryQuery,
    now: DateTime<Utc>,
) -> Vec<MemoryRecord> {
    let superseded: HashSet<&str> = superseded_set(universe);
    universe
        .iter()
        .filter(|record| query.matches(record, superseded.contains(record.memory_id.as_str()), now))
        .cloned()
        .collect()
}

/// The tenant namespace's superseded set, as [`apply_query`] defines it:
/// everything named in a `supersedes` field, plus everything a `summary`
/// record names as a source (see its docs). Shared by conflict detection
/// and forget planning, which must agree with retrieval about what is
/// superseded — three readers, one definition.
pub fn superseded_set(universe: &[MemoryRecord]) -> HashSet<&str> {
    universe
        .iter()
        .filter_map(|record| record.supersedes.as_deref())
        .chain(
            universe
                .iter()
                .filter(|record| record.kind == MemoryKind::Summary)
                .flat_map(|record| {
                    record
                        .provenance
                        .evidence
                        .source_memory_ids
                        .iter()
                        .map(String::as_str)
                }),
        )
        .collect()
}

/// What the assembly does when the next-ranked record does not fit the
/// remaining budget.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetOverflow {
    /// Stop packing: the assembly carries what fit, ranked, with
    /// `truncated: true`. The default — a prompt assembly that silently
    /// drops lower-ranked context is the honest behavior for a budget.
    #[default]
    Truncate,
    /// Fail the read: the caller declared the budget hard and wants to
    /// know the filtered set did not fit, rather than see a prefix of it.
    Fail,
}

/// A prompt-assembly budget, in estimated tokens (see the module docs for
/// the accounting rule). `ContextBudget` is the seam a model-precise
/// tokenizer plugs into later: the type stays, the estimate behind
/// [`estimated_tokens`] is what a later wave may sharpen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextBudget {
    /// The maximum estimated tokens the assembly may carry.
    pub max_tokens: u32,

    /// The declared safety margin of the estimate, in percent
    /// (default [`DEFAULT_TOKEN_MARGIN_PERCENT`]; absent from the wire at
    /// the default).
    #[serde(
        default = "default_margin_percent",
        skip_serializing_if = "is_default_margin_percent"
    )]
    pub margin_percent: u32,

    /// What to do when the next-ranked record does not fit (default
    /// [`BudgetOverflow::Truncate`]).
    #[serde(default)]
    pub overflow: BudgetOverflow,
}

fn default_margin_percent() -> u32 {
    DEFAULT_TOKEN_MARGIN_PERCENT
}

fn is_default_margin_percent(value: &u32) -> bool {
    *value == DEFAULT_TOKEN_MARGIN_PERCENT
}

impl ContextBudget {
    /// A budget of `max_tokens` estimated tokens with the declared default
    /// margin and truncate overflow.
    pub fn new(max_tokens: u32) -> Self {
        Self {
            max_tokens,
            margin_percent: DEFAULT_TOKEN_MARGIN_PERCENT,
            overflow: BudgetOverflow::Truncate,
        }
    }

    /// Builder-style: override the safety margin.
    pub fn with_margin_percent(mut self, margin_percent: u32) -> Self {
        self.margin_percent = margin_percent;
        self
    }

    /// Builder-style: override the overflow policy.
    pub fn with_overflow(mut self, overflow: BudgetOverflow) -> Self {
        self.overflow = overflow;
        self
    }
}

/// Estimated tokens for `bytes` of serialized content under the declared
/// accounting: `ceil(bytes / TOKEN_BYTES_PER_ESTIMATE)` scaled up by the
/// margin, saturated at `u32::MAX` (an over-budget estimate only means the
/// record does not fit — the estimate never wraps).
pub fn estimated_tokens(bytes: u64, margin_percent: u32) -> u32 {
    let base = bytes.div_ceil(TOKEN_BYTES_PER_ESTIMATE) as u128;
    let scaled = base * (100 + margin_percent as u128) / 100;
    scaled.min(u32::MAX as u128) as u32
}

/// The accounting an assembly was packed under, journaled with it: the
/// estimate rule (bytes per token and margin), the budget, and how much of
/// it the assembly used. Recorded — not recomputed — so an auditor reads
/// the accounting the read actually applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenAccounting {
    /// The estimate's divisor ([`TOKEN_BYTES_PER_ESTIMATE`]): estimated
    /// tokens are serialized content bytes divided by this.
    pub bytes_per_token: u64,

    /// The safety margin that was applied, in percent.
    pub margin_percent: u32,

    /// The budget the assembly was packed against, in estimated tokens.
    pub budget_tokens: u32,

    /// The estimated tokens the packed records sum to.
    pub used_tokens: u32,
}

/// The result of a token-bounded retrieval: the filtered records, ranked
/// and packed. This is the [`RunEventKind::MemoryRead`] event's output
/// payload — the record ids and their order are first-class
/// ([`MemoryAssembly::memory_ids`], the design's journaled assembly), and
/// the full records travel with them so the prompt a model saw is
/// reconstructable from the journal alone, never from a store query re-run
/// later against mutated state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryAssembly {
    /// The packed records' content addresses, in rank order. The journaled
    /// fact replay and audit match on.
    pub memory_ids: Vec<String>,

    /// The packed records, in the same order.
    pub records: Vec<MemoryRecord>,

    /// The accounting the packing applied.
    pub token_accounting: TokenAccounting,

    /// `true` when the budget cut the ranked set short (truncate overflow).
    pub truncated: bool,
}

/// Rank and pack `records` under `budget`, deterministically.
///
/// The rank is a total order — explicit priority descending, then
/// confidence descending, then recency (`created_at`) descending, then the
/// content address ascending as the final tie-break — so equal store state
/// and equal budget produce byte-equal assemblies. Packing walks the ranked
/// list and stops at the first record that does not fit the remaining
/// budget ([`BudgetOverflow::Truncate`]) or fails the read
/// ([`BudgetOverflow::Fail`]).
pub fn assemble(records: Vec<MemoryRecord>, budget: &ContextBudget) -> Result<MemoryAssembly> {
    let mut ranked = records;
    ranked.sort_by(|a, b| {
        b.priority
            .cmp(&a.priority)
            .then_with(|| b.confidence.total_cmp(&a.confidence))
            .then_with(|| b.created_at.cmp(&a.created_at))
            .then_with(|| a.memory_id.cmp(&b.memory_id))
    });
    let mut used_tokens: u32 = 0;
    let mut packed: Vec<MemoryRecord> = Vec::new();
    let mut truncated = false;
    for record in ranked {
        let cost = estimated_tokens(record.content_bytes(), budget.margin_percent);
        if used_tokens.saturating_add(cost) > budget.max_tokens {
            match budget.overflow {
                BudgetOverflow::Truncate => {
                    truncated = true;
                    break;
                }
                BudgetOverflow::Fail => {
                    return Err(invalid(format!(
                        "memory assembly exceeds the context budget: record `{}` costs an \
                         estimated {cost} tokens with {} of {} already used — the filtered \
                         set does not fit, and the budget was declared hard",
                        record.memory_id, used_tokens, budget.max_tokens
                    )));
                }
            }
        }
        used_tokens = used_tokens.saturating_add(cost);
        packed.push(record);
    }
    let memory_ids = packed
        .iter()
        .map(|record| record.memory_id.clone())
        .collect();
    Ok(MemoryAssembly {
        memory_ids,
        records: packed,
        token_accounting: TokenAccounting {
            bytes_per_token: TOKEN_BYTES_PER_ESTIMATE,
            margin_percent: budget.margin_percent,
            budget_tokens: budget.max_tokens,
            used_tokens,
        },
        truncated,
    })
}

// --------------------------------------------------------------------- //
// The correction loop (R0.8 wave 2)
// --------------------------------------------------------------------- //

/// Reject an empty (or all-whitespace) identity at deserialization: a
/// correction that cannot name its corrector is indistinguishable from a
/// prompt edit, so attribution is enforced where the contract is parsed,
/// not where it is consumed.
fn deserialize_non_empty_id<'de, D>(deserializer: D) -> std::result::Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    if value.trim().is_empty() {
        return Err(serde::de::Error::custom(
            "identity must be non-empty — attribution is mandatory",
        ));
    }
    Ok(value)
}

/// What a correction corrects. Closed enum — the three targets the design
/// names; consumers match exhaustively.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CorrectionTarget {
    /// A journaled run event (`{run_id}:{seq}`). The correction also yields
    /// an `example`-kind record — the input the run saw (read from the
    /// journaled event, never re-asked of the world) plus the corrected
    /// behavior — which the distiller folds into a versioned dataset: the
    /// correction is both the fix and the regression test.
    RunEvent {
        /// The run whose journal holds the event.
        run_id: String,
        /// The journaled event's id inside that run.
        event_id: String,
    },
    /// A memory record, by content address. The derived record inherits the
    /// target's key, so a same-key correction auto-supersedes the prior
    /// record (open question 5).
    Memory {
        /// The corrected record's content address.
        memory_id: String,
    },
    /// A pinned prompt hash from the run manifest
    /// ([`crate::record::RunManifest::pin_prompt`]).
    Prompt {
        /// The pinned prompt's content hash.
        prompt_hash: String,
    },
}

/// A human correction: the highest-trust input the learning system has, and
/// the loop treats it accordingly — **a correction becomes an attributed
/// candidate memory or example, never an in-place rewrite of what it
/// corrects.**
///
/// `author` is mandatory and validated at deserialization (see
/// `deserialize_non_empty_id`); attribution then travels with every
/// derived record as `human:{author}` provenance with the correction id in
/// evidence — the design's `human:{author} via correction:{id}` string —
/// so every later consumer (distiller, evaluator, auditor) can trace the
/// record to the person and the moment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Correction {
    /// The correction's own id, caller-minted; carried in the derived
    /// records' evidence (and therefore their content addresses — two
    /// corrections with identical corrected content stay distinct records).
    #[serde(deserialize_with = "deserialize_non_empty_id")]
    pub correction_id: String,

    /// The correcting human's identity. Mandatory.
    #[serde(deserialize_with = "deserialize_non_empty_id")]
    pub author: String,

    /// What it corrects.
    pub target: CorrectionTarget,

    /// The corrected content: what the derived record asserts.
    pub corrected: Value,

    /// The scope the result should live at. Scope decides the path
    /// ([`Correction::is_candidate`]): run scope is adopted directly (it
    /// affects only the run that produced it); agent scope or wider becomes
    /// a candidate pending evaluation.
    pub scope: ScopeAddress,

    /// Why the correction is right, when the corrector said. Optional; the
    /// attribution is not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rationale: Option<String>,
}

impl Correction {
    /// The derived records' author: `human:{author}`.
    pub fn author_as_provenance(&self) -> ProvenanceAuthor {
        ProvenanceAuthor::Human {
            human_id: self.author.clone(),
        }
    }

    /// The attribution string the design spells: `human:{author} via
    /// correction:{id}`. The derived record carries it structurally (author
    /// plus evidence); this is the human-facing form, for responses and
    /// audit output.
    pub fn attribution(&self) -> String {
        format!(
            "human:{} via correction:{}",
            self.author, self.correction_id
        )
    }

    /// The evidence every derived record carries: the correction id, plus
    /// the run linkage when the target is a journaled run event.
    pub fn evidence(&self) -> MemoryEvidence {
        let (run_id, event_ids) = match &self.target {
            CorrectionTarget::RunEvent { run_id, event_id } => {
                (Some(run_id.clone()), vec![event_id.clone()])
            }
            _ => (None, Vec::new()),
        };
        MemoryEvidence {
            run_id,
            event_ids,
            correction_id: Some(self.correction_id.clone()),
            ..MemoryEvidence::default()
        }
    }

    /// The scope-decides-the-path rule: `true` when this correction's
    /// derived records are candidates pending evaluation (agent scope or
    /// wider), `false` at run scope, where a correction is adopted directly
    /// — it affects only the run that produced it.
    pub fn is_candidate(&self) -> bool {
        self.scope.scope != MemoryScope::Run
    }
}

// --------------------------------------------------------------------- //
// Forgetting (R0.8 wave 2): real deletion with a journaled receipt
// --------------------------------------------------------------------- //

/// Why a record was forgotten. Closed enum, carried on the tombstone: the
/// reason is what distinguishes a reaper's expiry from a GDPR-shaped
/// erasure request in the audit trail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForgetReason {
    /// The record's TTL lapsed. Expiration stays a retrieval filter too —
    /// this is the operator-driven sweep acting on it, journaled.
    Expired,
    /// The record was wrong (or its correction superseded the need for it)
    /// and is withdrawn.
    Retracted,
    /// An erasure request (a user's, an operator's compliance pass).
    ErasureRequest,
}

/// The journaled receipt of a forgetting — the
/// [`RunEventKind::MemoryForget`] event's output payload. **Metadata by
/// construction**: the forgotten id, its scope, the reason, and the
/// dependent invalidations. There is no content field to leave unset — a
/// tombstone that *could* carry the forgotten bytes would be one careless
/// serializer away from defeating the erasure it receipts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryForgetTombstone {
    /// The forgotten record's content address.
    pub memory_id: String,

    /// The scope the record lived at.
    pub scope: ScopeAddress,

    /// Why it was forgotten.
    pub reason: ForgetReason,

    /// The dependent summaries the erasure invalidated (the reverse walk
    /// over source naming, transitively — see [`plan_forget`]). Their ids,
    /// never their content.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub invalidated: Vec<String>,
}

/// The derived idempotency key of a forgetting:
/// `memory_forget:{scope}:{memory_id}`. Retried erasures converge on one
/// key — forgetting an already-forgotten record is the same journaled
/// effect, not a second one.
pub fn memory_forget_effect_key(scope: &ScopeAddress, memory_id: &str) -> String {
    format!("memory_forget:{}:{memory_id}", scope.as_address())
}

/// What a forgetting will do, computed over the namespace **before**
/// anything is deleted: the explicitly forgotten records and the dependent
/// summaries the erasure invalidates. Planning is a pure function of store
/// state, so the route can journal exactly what it is about to do — the
/// tombstone names this plan, and the deletion then executes it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ForgetPlan {
    /// The explicitly forgotten records (the targets that exist in the
    /// namespace), sorted for determinism.
    pub forgotten: Vec<String>,

    /// The dependent summaries invalidated by the erasure, sorted: every
    /// `summary` record that names a removed record in
    /// [`MemoryEvidence::source_memory_ids`], transitively (a summary of
    /// summaries goes too). They are deleted with the targets — a summary
    /// built on erased evidence is derived state, and leaving it serving
    /// content distilled from the forgotten record is not forgetting
    /// (Cao & Yang's point, per the design).
    pub invalidated: Vec<String>,
}

/// Plan a forgetting over `universe` (one tenant's namespace): `targets`
/// are the records to erase; the dependent-summary walk runs in reverse
/// over source naming until the closure stops growing. Targets absent from
/// the universe are skipped (the caller decides whether absence is an
/// error — the single-id route 404s before planning).
pub fn plan_forget(universe: &[MemoryRecord], targets: &[String]) -> ForgetPlan {
    let mut removed: HashSet<&str> = universe
        .iter()
        .filter(|record| targets.iter().any(|t| t == &record.memory_id))
        .map(|record| record.memory_id.as_str())
        .collect();
    // The fixpoint: a summary joins the removal set whenever it names a
    // record already in it. Terminates because the set grows monotonically
    // over a finite universe.
    loop {
        let mut grew = false;
        for record in universe {
            if record.kind != MemoryKind::Summary || removed.contains(record.memory_id.as_str()) {
                continue;
            }
            if record
                .provenance
                .evidence
                .source_memory_ids
                .iter()
                .any(|source| removed.contains(source.as_str()))
            {
                removed.insert(record.memory_id.as_str());
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    let mut forgotten: Vec<String> = removed
        .iter()
        .filter(|id| targets.iter().any(|t| t.as_str() == **id))
        .map(|id| id.to_string())
        .collect();
    forgotten.sort();
    let mut invalidated: Vec<String> = removed
        .iter()
        .filter(|id| !forgotten.iter().any(|f| f.as_str() == **id))
        .map(|id| id.to_string())
        .collect();
    invalidated.sort();
    ForgetPlan {
        forgotten,
        invalidated,
    }
}

// --------------------------------------------------------------------- //
// Conflict detection (R0.8 wave 2): evidence, never resolution
// --------------------------------------------------------------------- //

/// A flagged conflict: two live records that share a key, overlap in
/// validity, and carry contradictory content. A review item — the design's
/// rule is that detection is evidence and resolution is governance, so
/// nothing here (or anywhere in the runtime) resolves the pair: Zep and
/// Mem0 resolve contradictions inside the ingestion pipeline with an LLM,
/// which is precisely the silent-mutation pattern the learning rule exists
/// to forbid.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryConflict {
    /// The scope the conflicted records live at.
    pub scope: ScopeAddress,

    /// The key they share.
    pub key: String,

    /// The two records' content addresses, sorted.
    pub memory_ids: Vec<String>,

    /// The interval both records claim to be true over — the overlap of
    /// their validity windows.
    pub overlap: ValidityWindow,
}

/// Flag the conflicts in `universe` (one tenant's namespace) at `now`.
///
/// "Live" excludes exactly what default retrieval excludes: superseded
/// records (a superseded record is replaced evidence, not a contender) and
/// records expired at `now`. "Contradictory" is structural and honest about
/// it: with no semantic judge in the runtime, contradiction means the two
/// records assert *different canonical content* for the same key over an
/// overlapping interval — semantics belong to the reviewer the flag is for.
/// A pair where one record supersedes the other is disciplined replacement,
/// not conflict. The output is deterministically ordered (scope, key, ids),
/// so equal store state flags byte-equal review items.
pub fn detect_conflicts(universe: &[MemoryRecord], now: DateTime<Utc>) -> Vec<MemoryConflict> {
    let superseded = superseded_set(universe);
    let is_live = |record: &MemoryRecord| {
        !superseded.contains(record.memory_id.as_str())
            && record.expires_at.is_none_or(|expires| expires > now)
    };
    let mut groups: BTreeMap<(String, &str), Vec<&MemoryRecord>> = BTreeMap::new();
    for record in universe.iter().filter(|r| is_live(r)) {
        let Some(key) = record.key.as_deref() else {
            continue;
        };
        groups
            .entry((record.scope.as_address(), key))
            .or_default()
            .push(record);
    }
    let mut conflicts = Vec::new();
    for ((_, key), mut group) in groups {
        group.sort_by(|a, b| a.memory_id.cmp(&b.memory_id));
        for (i, a) in group.iter().enumerate() {
            for b in &group[i + 1..] {
                if a.supersedes.as_deref() == Some(b.memory_id.as_str())
                    || b.supersedes.as_deref() == Some(a.memory_id.as_str())
                {
                    continue;
                }
                let Some(overlap) = validity_overlap(&a.validity, &b.validity) else {
                    continue;
                };
                if same_content(a, b) {
                    continue;
                }
                conflicts.push(MemoryConflict {
                    scope: a.scope.clone(),
                    key: key.to_string(),
                    memory_ids: vec![a.memory_id.clone(), b.memory_id.clone()],
                    overlap,
                });
            }
        }
    }
    conflicts.sort_by(|a, b| {
        a.scope
            .as_address()
            .cmp(&b.scope.as_address())
            .then_with(|| a.key.cmp(&b.key))
            .then_with(|| a.memory_ids.cmp(&b.memory_ids))
    });
    conflicts
}

/// The intersection of two validity windows: `[max(from), min(until))`,
/// `None` when the windows are disjoint. Open-endedness composes (open ∩
/// closed is the closed end; open ∩ open stays open).
fn validity_overlap(a: &ValidityWindow, b: &ValidityWindow) -> Option<ValidityWindow> {
    let valid_from = a.valid_from.max(b.valid_from);
    let valid_until = match (a.valid_until, b.valid_until) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (x, y) => x.or(y),
    };
    if valid_until.is_some_and(|until| valid_from >= until) {
        return None;
    }
    Some(ValidityWindow {
        valid_from,
        valid_until,
    })
}

/// Content equality by canonical hash, so inline and artifact-referenced
/// forms of the same body compare equal (the payload discipline's rule
/// applied to conflict detection).
fn same_content(a: &MemoryRecord, b: &MemoryRecord) -> bool {
    match (a.content.content_hash(), b.content.content_hash()) {
        (Ok(x), Ok(y)) => x == y,
        _ => a.content == b.content,
    }
}

// --------------------------------------------------------------------- //
// Consolidation (R0.8 wave 2): the runtime-owned summary invariants
// --------------------------------------------------------------------- //

/// Build the one `summary` record that distills `sources`, under the
/// invariants the runtime owns: the sources are named in
/// [`MemoryEvidence::source_memory_ids`] (which supersedes them — see
/// [`apply_query`] — and makes dependent-summary invalidation computable on
/// forgetting), the author is `distiller:{name}`, and the confidence is the
/// **minimum of the sources'**: a summary is a claim no stronger than its
/// weakest source, and the model is honest about that.
///
/// What the runtime deliberately does not own: `content`. The distillation
/// semantics are application code (open question 2's boundary, the same one
/// the distiller contract draws); this function is the orchestration around
/// them. The record's validity spans the sources (earliest `valid_from`;
/// the latest `valid_until` when every source is closed, open-ended
/// otherwise — a summary cannot outclaim the evidence it distills).
///
/// Fails when `sources` is empty: a summary that names no sources is not a
/// consolidation, and an unnamed derivation cannot be audited or forgotten
/// correctly.
pub fn consolidation_summary(
    scope: ScopeAddress,
    distiller: impl Into<String>,
    sources: &[MemoryRecord],
    content: Value,
    now: DateTime<Utc>,
) -> Result<MemoryRecord> {
    if sources.is_empty() {
        return Err(invalid(
            "consolidation needs at least one source record — a summary that names no \
             sources is not a consolidation, and an unnamed derivation can neither be \
             audited nor forgotten correctly",
        ));
    }
    let mut source_ids: Vec<String> = sources
        .iter()
        .map(|record| record.memory_id.clone())
        .collect();
    source_ids.sort();
    let confidence = sources
        .iter()
        .map(|record| record.confidence)
        .fold(f64::INFINITY, f64::min);
    let valid_from = sources
        .iter()
        .map(|record| record.validity.valid_from)
        .min()
        .expect("sources is non-empty");
    let valid_until = sources
        .iter()
        .map(|record| record.validity.valid_until)
        .collect::<Option<Vec<DateTime<Utc>>>>()
        .and_then(|untils| untils.into_iter().max());
    let provenance = MemoryProvenance {
        author: ProvenanceAuthor::Distiller {
            name: distiller.into(),
        },
        evidence: MemoryEvidence {
            source_memory_ids: source_ids,
            ..MemoryEvidence::default()
        },
        written_at: now,
    };
    MemoryRecord::new(
        MemoryKind::Summary,
        scope,
        provenance,
        confidence,
        ValidityWindow {
            valid_from,
            valid_until,
        },
        now,
        content,
    )
}

// --------------------------------------------------------------------- //
// The store contract
// --------------------------------------------------------------------- //

/// A governed memory store. The two server backends (JSON files, Postgres)
/// implement it over their own layouts; [`InMemoryMemoryStore`] is the
/// dev/test implementation, the same role `InMemoryCheckpointer` plays for
/// checkpoints.
///
/// The contract deliberately knows nothing about tenants: tenant isolation
/// is the server's `{tenant}/` id-namespacing, applied by the server
/// backends around this contract — a store that can see another tenant's
/// records is a breach wearing a storage costume.
#[async_trait]
pub trait MemoryStore: Send + Sync + std::fmt::Debug {
    /// Store `record` under its content address. Idempotent: storing the
    /// same record twice yields `false` (already present) and at most one
    /// stored record — the `Effect::Idempotent` write converges by
    /// construction, since the address is derived from content plus
    /// provenance.
    async fn put(&self, record: &MemoryRecord) -> Result<bool>;

    /// Fetch one record by content address (`None` when absent).
    async fn get(&self, memory_id: &str) -> Result<Option<MemoryRecord>>;

    /// The query universe: every record in the namespace. Retrieval
    /// semantics live in [`apply_query`]; dev-scale stores scan, and larger
    /// backends pre-filter on columns before applying the same function.
    async fn all(&self) -> Result<Vec<MemoryRecord>>;

    /// Remove `memory_id` from the store (`false` when absent). Forgetting
    /// (R0.8 wave 2) is real deletion of derived state — journals are
    /// hash-chained evidence and are never touched (open question 4's
    /// boundary).
    async fn remove(&self, memory_id: &str) -> Result<bool>;

    /// The records matching `query`, expiry evaluated at `now`. The default
    /// implementation scans via [`MemoryStore::all`] and applies
    /// [`apply_query`]; backends with column-mapped storage override the
    /// scan, never the semantics.
    async fn query(&self, query: &MemoryQuery, now: DateTime<Utc>) -> Result<Vec<MemoryRecord>> {
        Ok(apply_query(&self.all().await?, query, now))
    }
}

/// In-memory [`MemoryStore`] (dev and test): one map keyed by content
/// address. No persistence, no tenant concept — the honest reference for
/// the contract's semantics.
#[derive(Debug, Default)]
pub struct InMemoryMemoryStore {
    records: Mutex<BTreeMap<String, MemoryRecord>>,
}

impl InMemoryMemoryStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<String, MemoryRecord>> {
        // Poison means a store path panicked mid-write; the map is plain
        // data and stays coherent, so recovering is safe.
        self.records.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[async_trait]
impl MemoryStore for InMemoryMemoryStore {
    async fn put(&self, record: &MemoryRecord) -> Result<bool> {
        let mut map = self.lock();
        if map.contains_key(&record.memory_id) {
            return Ok(false);
        }
        map.insert(record.memory_id.clone(), record.clone());
        Ok(true)
    }

    async fn get(&self, memory_id: &str) -> Result<Option<MemoryRecord>> {
        Ok(self.lock().get(memory_id).cloned())
    }

    async fn all(&self) -> Result<Vec<MemoryRecord>> {
        Ok(self.lock().values().cloned().collect())
    }

    async fn remove(&self, memory_id: &str) -> Result<bool> {
        Ok(self.lock().remove(memory_id).is_some())
    }
}

// --------------------------------------------------------------------- //
// The journaled seam
// --------------------------------------------------------------------- //

/// The canonical JSON shape a [`JournaledMemory`] read journals (and a
/// [`MemoryReplaySource`] matches) as a memory read's **request**: the
/// resolved query (with [`MemoryQuery::as_of`] stamped) and the budget,
/// exactly as passed to the store. The hash of this value's canonical
/// serialization is the request identity exact replay matches on — the same
/// rule [`crate::replay::model_call_request`] sets for model calls.
pub fn memory_read_request(query: &MemoryQuery, budget: &ContextBudget) -> Value {
    json!({ "query": query, "budget": budget })
}

/// The derived idempotency key of a memory write:
/// `memory:{scope}:{memory_id}` (the design's write-path rule). Retried
/// submissions of the same write converge on one key, and recovery can
/// re-derive it without consulting the store.
pub fn memory_effect_key(scope: &ScopeAddress, memory_id: &str) -> String {
    format!("memory:{}:{memory_id}", scope.as_address())
}

/// Canonical-content hash of a request value — the same computation
/// `PayloadRef::content_hash` applies and the same one
/// [`crate::replay::ReplaySource`] matches model/tool requests with.
fn request_hash(request: &Value) -> Result<String> {
    Ok(sha256_hex(&serde_json::to_vec(request)?))
}

#[derive(Debug)]
struct MemoryReplayInner {
    /// The snapshot's `MemoryRead` events, in `seq` order.
    servable: Vec<RunEvent>,
    /// The snapshot's artifact map, for payload resolution.
    artifacts: BTreeMap<String, Value>,
    /// Position of the next unserved event. Strictly ordered, the same rule
    /// the Flight Recorder's replay source applies.
    cursor: usize,
}

/// The serving side of exact replay for memory reads: an ordered cursor
/// over a journaled run's recorded [`RunEventKind::MemoryRead`] events.
///
/// This is [`crate::replay::ReplaySource`] specialized to one event kind —
/// the same matching rule (sequence + request hash), the same loud failure
/// on any mismatch — kept in this module because a memory read is answered
/// from the memory seam, not from a model or tool wrapper. Matching is by
/// **sequence + request hash**: [`MemoryReplaySource::serve`] takes the next
/// unserved `MemoryRead` event in `seq` order and requires the issued
/// request's canonical hash to equal the journaled request's. Anything else
/// fails with [`RustyError::Replay`] — a replay that has drifted from its
/// evidence must stop, not improvise.
#[derive(Debug, Clone)]
pub struct MemoryReplaySource {
    inner: Arc<Mutex<MemoryReplayInner>>,
}

impl MemoryReplaySource {
    /// A source over the `MemoryRead` events of `snapshot`. The snapshot is
    /// assumed already integrity-verified (the caller's replay session
    /// verifies it); unresolved artifact references simply fail to match.
    pub fn new(snapshot: &JournalSnapshot) -> Self {
        Self {
            inner: Arc::new(Mutex::new(MemoryReplayInner {
                servable: snapshot
                    .events
                    .iter()
                    .filter(|event| event.kind == RunEventKind::MemoryRead)
                    .cloned()
                    .collect(),
                artifacts: snapshot.artifacts.clone(),
                cursor: 0,
            })),
        }
    }

    fn lock(&self) -> MutexGuard<'_, MemoryReplayInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Serve one memory read from the journal: the next unserved
    /// `MemoryRead` event whose journaled request hash-equals `request`.
    /// The returned [`ServedEffect`] carries the recorded assembly; re-journal
    /// it into the replay run's journal so the replayed evidence reproduces
    /// the recorded evidence byte-for-byte.
    pub fn serve(&self, request: &Value) -> Result<ServedEffect> {
        let issued_hash = request_hash(request)?;
        let mut inner = self.lock();
        let Some(event) = inner.servable.get(inner.cursor) else {
            return Err(replay_error(format!(
                "replay exhaustion: the run issued a memory read (request hash {issued_hash}), \
                 but the journal holds no further recorded memory reads to serve — the \
                 replayed run is doing more work than the recorded one"
            )));
        };
        let recorded_input = event
            .input
            .as_ref()
            .and_then(|payload| match payload {
                PayloadRef::Inline(value) => Some(value.clone()),
                PayloadRef::Artifact(reference) => inner.artifacts.get(&reference.sha256).cloned(),
            })
            .ok_or_else(|| {
                replay_error(format!(
                    "recorded memory read at seq {} has no input payload to match against",
                    event.seq
                ))
            })?;
        let recorded_hash = request_hash(&recorded_input)?;
        if recorded_hash != issued_hash {
            return Err(replay_error(format!(
                "replay divergence at recorded seq {} (memory read): the run issued a request \
                 whose canonical hash is {issued_hash}, but the journaled request hashes to \
                 {recorded_hash} — the replayed run has diverged from its evidence",
                event.seq
            )));
        }
        let served = ServedEffect {
            input: Some(recorded_input),
            output: event.output.as_ref().and_then(|payload| match payload {
                PayloadRef::Inline(value) => Some(value.clone()),
                PayloadRef::Artifact(reference) => inner.artifacts.get(&reference.sha256).cloned(),
            }),
            event: event.clone(),
            // Memory reads record no nested evidence; the field is the
            // approval pairs nested under model and tool calls.
            nested: Vec::new(),
        };
        inner.cursor += 1;
        Ok(served)
    }

    /// `true` when every recorded memory read has been served. Verification
    /// treats a non-exhausted source as a replay that stopped short of its
    /// evidence.
    pub fn is_exhausted(&self) -> bool {
        let inner = self.lock();
        inner.cursor == inner.servable.len()
    }
}

/// Where a journaled memory handle answers reads from: the live store
/// (record mode) or a recorded journal (replay mode).
///
/// Node code holds one [`JournaledMemory`] either way and cannot tell the
/// difference — the same discipline the Flight Recorder's recording and
/// replaying model wrappers apply: behavior that matters is journaled, and
/// replay serves the journal instead of the world.
#[derive(Debug, Clone)]
pub enum MemorySource {
    /// Read from and write to a live [`MemoryStore`], journaling every
    /// operation.
    Store(Arc<dyn MemoryStore>),

    /// Read from a recorded journal ([`MemoryReplaySource`]), re-journaling
    /// the served events byte-identically. Writes fail: a replayed run
    /// issuing a write has diverged from its evidence.
    Replay(MemoryReplaySource),
}

/// A memory handle bound to a run's journal: the journaled read/write seam
/// node code (and later waves' run integration) uses.
///
/// Cheap to clone (the journal and source are shared handles), so node
/// closures capture it exactly the way they capture a [`Journal`] for
/// recording model and tool calls. Causal parentage comes from the
/// invocation's node-input event id, passed as `parent` — node code reads
/// it from [`crate::journal::PARENT_EVENT_KEY`] in `NodeConfig::extra`.
///
/// Clock-read parity: the live and replay paths read the run's clock the
/// same number of times in the same order (one `as_of` stamp, two latency
/// reads, one read inside [`Journal::record`]), so a logical clock's tick
/// sequence — and therefore every journaled timestamp — reproduces exactly
/// on replay. This is the same parity rule the replaying model wrapper
/// documents.
#[derive(Debug, Clone)]
pub struct JournaledMemory {
    journal: Journal,
    source: MemorySource,
}

impl JournaledMemory {
    /// Bind `source` to `journal` (the run's journal — build via
    /// [`Journal::memory`]).
    pub fn new(journal: &Journal, source: MemorySource) -> Self {
        Self {
            journal: journal.clone(),
            source,
        }
    }

    /// The source this handle answers from.
    pub fn source(&self) -> &MemorySource {
        &self.source
    }

    /// Perform a governed read: resolve [`MemoryQuery::as_of`] through the
    /// run's clock when unset, then — live — query the store, assemble
    /// deterministically, and journal the assembly as a
    /// [`RunEventKind::MemoryRead`] event ([`Effect::ReadOnly`]); or —
    /// replay — serve the journaled assembly byte-identically and re-journal
    /// it. Returns the assembly either way.
    pub async fn read(
        &self,
        query: &MemoryQuery,
        budget: &ContextBudget,
        parent: Option<String>,
    ) -> Result<MemoryAssembly> {
        // The resolved query fully determines the result: as_of stamped
        // through the run's clock (deterministic under a logical clock),
        // everything else declared. One clock read here in both paths —
        // the parity the byte-identical replay property rests on.
        let mut resolved = query.clone();
        let as_of = resolved.as_of.unwrap_or_else(|| self.journal.clock().now());
        resolved.as_of = Some(as_of);
        let request = memory_read_request(&resolved, budget);
        match &self.source {
            MemorySource::Store(store) => {
                let started = self.journal.clock().now();
                let records = store.query(&resolved, as_of).await?;
                let latency_ms = (self.journal.clock().now() - started)
                    .num_milliseconds()
                    .max(0) as u64;
                let assembly = assemble(records, budget)?;
                let mut draft = EventDraft::new(RunEventKind::MemoryRead, Effect::ReadOnly)
                    .input(request)
                    .output(serde_json::to_value(&assembly)?)
                    .latency_ms(latency_ms);
                if let Some(parent) = parent {
                    draft = draft.parent(parent);
                }
                self.journal.record(draft);
                Ok(assembly)
            }
            MemorySource::Replay(source) => {
                let served = source.serve(&request)?;
                // Clock-read parity with the live path (two latency reads
                // per read) keeps the logical clock's tick sequence aligned
                // with the recorded run, so the replayed journal's
                // timestamps reproduce it exactly.
                let _started = self.journal.clock().now();
                let _ended = self.journal.clock().now();
                let parent = parent
                    .or_else(|| served.event.parent.clone())
                    .unwrap_or_default();
                served.rejournal(&self.journal, parent);
                let output = served.output.clone().ok_or_else(|| {
                    replay_error(format!(
                        "recorded memory read at seq {} carries no assembly payload — the \
                         journal is inconsistent",
                        served.event.seq
                    ))
                })?;
                let assembly: MemoryAssembly = serde_json::from_value(output)?;
                Ok(assembly)
            }
        }
    }

    /// Perform a governed write: store the record (content-addressed, so
    /// retries converge) and journal it as a [`RunEventKind::MemoryWrite`]
    /// event — [`Effect::Idempotent`] under the derived key
    /// `memory:{scope}:{memory_id}`, with the record and its provenance as
    /// the output payload. Returns the content address.
    ///
    /// A memory write changes what future retrievals return — nothing else
    /// (the write path's third gate). If the write is meant to change a
    /// prompt, a policy, or a permission, it belongs in the candidate
    /// pipeline, not here.
    ///
    /// Fails during replay: writes are never served, so a replayed run
    /// issuing one has diverged from its evidence.
    pub async fn write(&self, record: &MemoryRecord, parent: Option<String>) -> Result<String> {
        match &self.source {
            MemorySource::Store(store) => {
                store.put(record).await?;
                let effect_key = memory_effect_key(&record.scope, &record.memory_id);
                let mut draft = EventDraft::new(RunEventKind::MemoryWrite, Effect::Idempotent)
                    .input(json!({
                        "effect_key": effect_key,
                        "memory_id": record.memory_id,
                    }))
                    .output(serde_json::to_value(record)?);
                if let Some(parent) = parent {
                    draft = draft.parent(parent);
                }
                self.journal.record(draft);
                Ok(record.memory_id.clone())
            }
            MemorySource::Replay(_) => Err(replay_error(
                "memory writes are not served during replay: the run issued a write the \
                 recorded evidence does not contain — the replayed run has diverged from its \
                 evidence",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::Clock;

    fn ts(millis: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp_millis(millis).unwrap()
    }

    fn provenance() -> MemoryProvenance {
        MemoryProvenance {
            author: ProvenanceAuthor::Human {
                human_id: "amjad".into(),
            },
            evidence: MemoryEvidence::default(),
            written_at: ts(1_000),
        }
    }

    fn record(content: Value) -> MemoryRecord {
        MemoryRecord::new(
            MemoryKind::Fact,
            ScopeAddress::new(MemoryScope::User, "user-7"),
            provenance(),
            1.0,
            ValidityWindow::starting(ts(500)),
            ts(1_000),
            content,
        )
        .unwrap()
    }

    #[test]
    fn scope_mapping_is_the_documented_superset() {
        assert_eq!(
            MemoryScope::from_state_scope(StateScope::Private),
            Some(MemoryScope::Agent)
        );
        assert_eq!(
            MemoryScope::from_state_scope(StateScope::Team),
            Some(MemoryScope::Team)
        );
        assert_eq!(
            MemoryScope::from_state_scope(StateScope::User),
            Some(MemoryScope::User)
        );
        assert_eq!(
            MemoryScope::from_state_scope(StateScope::Tenant),
            Some(MemoryScope::Tenant)
        );
        assert_eq!(
            MemoryScope::Agent.to_state_scope(),
            Some(StateScope::Private)
        );
        assert_eq!(MemoryScope::Run.to_state_scope(), None);
        for scope in [
            MemoryScope::Run,
            MemoryScope::Agent,
            MemoryScope::Team,
            MemoryScope::User,
            MemoryScope::Tenant,
        ] {
            if let Some(state_scope) = scope.to_state_scope() {
                assert_eq!(MemoryScope::from_state_scope(state_scope), Some(scope));
            }
        }
    }

    #[test]
    fn memory_id_is_a_content_address() {
        let a = record(json!({"timezone": "UTC+4"}));
        let b = record(json!({"timezone": "UTC+4"}));
        let c = record(json!({"timezone": "UTC+1"}));
        // Identical content and provenance converge on one id…
        assert_eq!(a.memory_id, b.memory_id);
        assert_eq!(a.memory_id.len(), 64);
        // …changed content is a new id…
        assert_ne!(a.memory_id, c.memory_id);
        // …and so is changed provenance (origin is identity).
        let mut other_provenance = provenance();
        other_provenance.written_at = ts(2_000);
        let d = MemoryRecord::new(
            MemoryKind::Fact,
            ScopeAddress::new(MemoryScope::User, "user-7"),
            other_provenance,
            1.0,
            ValidityWindow::starting(ts(500)),
            ts(1_000),
            json!({"timezone": "UTC+4"}),
        )
        .unwrap();
        assert_ne!(a.memory_id, d.memory_id);
    }

    #[test]
    fn confidence_outside_the_interval_is_rejected() {
        for confidence in [0.0, -0.1, 1.01, f64::NAN] {
            assert!(
                MemoryRecord::new(
                    MemoryKind::Fact,
                    ScopeAddress::new(MemoryScope::User, "u"),
                    provenance(),
                    confidence,
                    ValidityWindow::starting(ts(0)),
                    ts(0),
                    json!(null),
                )
                .is_err(),
                "confidence {confidence} must be rejected"
            );
        }
    }

    #[test]
    fn content_split_follows_the_payload_discipline() {
        let small = record(json!({"k": "v"}));
        assert!(matches!(small.content, PayloadRef::Inline(_)));
        let big = record(json!({"blob": "x".repeat(INLINE_PAYLOAD_MAX_BYTES)}));
        let reference = match &big.content {
            PayloadRef::Artifact(reference) => reference.clone(),
            other => panic!("expected artifact reference, got {other:?}"),
        };
        assert_eq!(reference.bytes, big.content_bytes());
        // The address is derivable from the record itself: identity is
        // integrity.
        assert_eq!(
            derive_memory_id(&big.content, &big.provenance).unwrap(),
            big.memory_id
        );
    }

    #[test]
    fn estimated_tokens_applies_bytes_divisor_and_margin() {
        assert_eq!(estimated_tokens(0, 20), 0);
        assert_eq!(estimated_tokens(4, 0), 1);
        assert_eq!(estimated_tokens(5, 0), 2); // ceil
        assert_eq!(estimated_tokens(400, 20), 120); // 100 * 1.2
        assert_eq!(estimated_tokens(u64::MAX, 100), u32::MAX); // saturated
    }

    #[tokio::test]
    async fn journaled_write_is_idempotent_under_the_derived_key() {
        let journal = Journal::new("run-1", "thread-1", Clock::System);
        let store = Arc::new(InMemoryMemoryStore::new());
        let memory = journal.memory(MemorySource::Store(store.clone()));
        let record = record(json!({"a": 1}));
        let id = memory.write(&record, None).await.unwrap();
        assert_eq!(id, record.memory_id);
        // A retried submission of the same record converges.
        assert!(!store.put(&record).await.unwrap());

        let events = journal.events();
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.kind, RunEventKind::MemoryWrite);
        assert_eq!(event.effect, Effect::Idempotent);
        let input = event.input.as_ref().unwrap();
        let PayloadRef::Inline(input) = input else {
            panic!("write request travels inline")
        };
        assert_eq!(
            input["effect_key"].as_str().unwrap(),
            memory_effect_key(&record.scope, &record.memory_id)
        );
    }

    #[tokio::test]
    async fn write_fails_during_replay() {
        let journal = Journal::new("run-1", "thread-1", Clock::System);
        let snapshot = journal.snapshot();
        let memory = journal.memory(MemorySource::Replay(MemoryReplaySource::new(&snapshot)));
        let err = memory
            .write(&record(json!({"a": 1})), None)
            .await
            .unwrap_err();
        assert!(matches!(err, RustyError::Replay(_)));
    }
}
