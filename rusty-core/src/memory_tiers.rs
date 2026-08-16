//! Memory organization and optimization (R0.13 wave 2): the tier overlay on
//! the shipped scope taxonomy, the hierarchical key grammar with its write
//! gate (validation + content-equal dedup), declarative consolidation
//! scheduling, and the derived utility index with its two-stage
//! over-fetch + driver-side re-rank assembly discipline.
//!
//! The design doc is `docs/agent-core-design.md` ("Memory organization",
//! "Memory optimization"). Everything here **composes** `memory.rs` — the
//! record model, [`assemble`], [`JournaledMemory`], [`plan_forget`],
//! [`detect_conflicts`], [`consolidation_summary`] are consumed unchanged;
//! nothing in this module mutates a record, adds a journal event kind, or
//! re-opens a shipped contract.
//!
//! # Tiers as a retrieval-policy overlay
//!
//! The shipped scopes answer *whose memory it is*; [`MemoryTier`] answers
//! *how long it lives and how it gets into context*: **working** (the run's
//! own scratch — `run` scope), **episodic** (what happened — `agent`/`team`
//! scope `summary` records distilled from completed runs), **semantic**
//! (what is true — facts, preferences, examples at `agent` scope and wider).
//! Classification is a pure function of `(scope, kind)`
//! ([`MemoryTier::classify`]): tiers shape assembly, never storage, and
//! promotion between tiers is consolidation — a distillation hop through
//! the shipped [`consolidation_summary`], naming its sources so the shipped
//! [`plan_forget`] transitive invalidation walks the chain.
//!
//! # Key grammar and the write gate
//!
//! Keys are the retrieval contract while vectors stay deferred, so they get
//! a grammar instead of a convention: `domain.name`, lowercase snake
//! segments, the domain segment declared in a [`KeyGrammar`] and validated
//! at the write gate ([`WriteGate`]). The gate also closes the benign
//! duplicate the shipped content addressing cannot: two *independent*
//! writes of the same claim differ in provenance (`written_at` moves, so
//! the content address moves), and the same key ends up held by two records
//! saying the same thing. The rule (the design's dedup section): **same
//! scope, same key, content-equal-up-to-normalization** — canonical-content
//! hash equality, the same equality [`detect_conflicts`] uses — **converges
//! onto the existing record's id**. Same key, *different* content is not
//! dedup: it is supersession or a flagged conflict, unchanged.
//!
//! # Consolidation scheduling
//!
//! [`ConsolidationPolicy`] makes *when consolidation runs* declarative:
//! per scope and key domain, trigger thresholds (record count, aggregate
//! token footprint, age of the oldest unconsolidated record) and the
//! distiller to invoke. It travels as the additive optional `maintenance`
//! member of `memory_config` candidates, so a schedule change is a
//! governed, promotable, environment-tagged pointer move — and it alters
//! *when distillation is proposed*, never a record. [`consolidation_due`]
//! is the pure evaluation; the durable task that runs it and the summary
//! it lands are the shipped machinery.
//!
//! **Server seam (later wave).** The scheduler is the server's cron
//! machinery evaluating these thresholds against store statistics, and the
//! utility index below wants durable-task roll-up plus derived-index
//! storage on both backends. This module ships the core types and the pure
//! functions only; the server half is queued behind this wave (the design
//! doc's coordination notes, `rusty-server` entry).
//!
//! # The utility signal and the two-stage assembly
//!
//! The one genuinely new derivation: **which memories proved useful in
//! successful runs.** The evidence is already journaled — every
//! `MemoryRead` carries the assembly's record ids; every run carries a
//! terminal status and, where the eval plane graded it, a score.
//! [`build_utility_index`] rolls completed journals into a [`UtilityIndex`]:
//! per record, successful-run and failed-run appearance counts, derived —
//! never stored on the record — and rebuildable from journals
//! byte-identically.
//!
//! Consumption is the two-stage discipline the design fixes
//! ([`TieredMemoryDriver`]):
//!
//! 1. **Over-fetch.** The driver's stage one is the shipped [`assemble`]
//!    rank against an over-fetched budget (the policy-declared
//!    [`RankPolicy::over_fetch_percent`]), so the candidate pool holds
//!    records the base rank would have dropped at the true budget.
//! 2. **Re-rank and re-pack in the driver.** The pool is re-ordered
//!    tier-major (working → episodic → semantic), and within a tier by the
//!    policy-pinned utility weight times the record's smoothed success rate
//!    — integer arithmetic only, the [`crate::tool_select`] weights
//!    vocabulary — with the shipped rank as the tie-breaking floor beneath
//!    the re-rank. The final pack against the section's true budget is
//!    where utility changes *selection*, not just order.
//!
//! The journaled seam, stated exactly. The driver journals **one**
//! [`RunEventKind::MemoryRead`] per assembly — the shipped request shape
//! ([`memory_read_request`]: resolved query plus the section's true budget,
//! `as_of` stamped through the run's clock) with the final tier-ranked
//! assembly as the output, extended by a `section_manifest` member carrying
//! the pins the design requires in the journaled section manifest: the
//! [`RankPolicy`] weights, the utility snapshot stamp, and the over-fetched
//! pool's ids (content addresses — the pool re-resolves byte-identically
//! from the store while its records live, so the journaled assembly
//! re-derives from the journal plus the pins). The extension is additive:
//! [`MemoryAssembly`] deserializes the output ignoring the extra member, so
//! the shipped replay path ([`MemoryReplaySource`], `JournaledMemory`)
//! serves a tiered read unchanged — that is how the context pipeline
//! consumes the driver's section without `context.rs` changing. The
//! design's two-piece shape (the journaled read returning the raw superset,
//! the pipeline pinning the manifest natively) is the seam a later wave
//! lands when the pipeline is unclaimed; the discipline — over-fetch
//! through the shipped rank, re-rank outside it, weights and snapshot stamp
//! journaled — is identical.
//!
//! The floor is always legal: [`RankPolicy::default()`] is utility weight
//! zero and no over-fetch, under which the driver's within-tier order *is*
//! the shipped rank — the `static-v0` of retrieval, one pointer move away.
//!
//! Golden-file tests under `tests/golden/` pin every wire shape this module
//! adds; any accidental drift fails CI.

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{Result, RustyError};
use crate::journal::{EventDraft, Journal, JournalSnapshot};
use crate::memory::{
    apply_query, assemble, estimated_tokens, memory_read_request, ContextBudget, JournaledMemory,
    MemoryAssembly, MemoryKind, MemoryQuery, MemoryRecord, MemoryReplaySource, MemoryScope,
    MemoryStore, TokenAccounting, TOKEN_BYTES_PER_ESTIMATE,
};
use crate::record::{Effect, PayloadRef, RunEvent, RunEventKind};

fn invalid(message: impl Into<String>) -> RustyError {
    // Write-gate, policy, and assembly failures are configuration or
    // contract validation errors; the invalid-update class covers them
    // without growing the error taxonomy (the memory module's convention).
    RustyError::InvalidUpdate(message.into())
}

fn replay_error(message: impl Into<String>) -> RustyError {
    RustyError::Replay(message.into())
}

/// The [`TierSectionManifest`] format version, recorded inside every
/// manifest.
pub const TIER_MANIFEST_FORMAT_VERSION: &str = "memory-tier-manifest-v1";

/// The default over-fetch: 100% of the section budget — no over-fetch, the
/// floor every re-rank candidate is measured against.
pub const DEFAULT_OVER_FETCH_PERCENT: u32 = 100;

/// The smoothed success rate of a record with no recorded use, in basis
/// points: the neutral prior. The signal never buries the unobserved.
pub const NEUTRAL_SUCCESS_BPS: u32 = 5_000;

// --------------------------------------------------------------------- //
// The tier overlay
// --------------------------------------------------------------------- //

/// How long a memory lives and how it gets into context. Declaration order
/// is the assembly order: working first, semantic last (the derived `Ord`
/// is the tier-major sort key the driver packs with).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryTier {
    /// The run's own scratch: intermediate findings, plans, partial
    /// results. Expires with the thread; never consolidated upward
    /// directly.
    Working,
    /// What happened: episode summaries distilled from completed runs'
    /// journals, naming their source records and the run id in evidence.
    Episodic,
    /// What is true: facts, preferences, correction examples. Supersession
    /// chains, validity windows, forgetting only through the tombstoned
    /// operation.
    Semantic,
}

impl MemoryTier {
    /// The tier of a `(scope, kind)` pair — the overlay's whole mapping:
    /// run scope is working memory whatever the kind (its lifetime is the
    /// thread's); a `summary` at `agent`/`team` scope is an episode; every
    /// other combination is semantic (including `user`/`tenant` summaries —
    /// distillations of what is true, not of what happened).
    pub fn classify(scope: MemoryScope, kind: MemoryKind) -> MemoryTier {
        match (scope, kind) {
            (MemoryScope::Run, _) => MemoryTier::Working,
            (MemoryScope::Agent | MemoryScope::Team, MemoryKind::Summary) => MemoryTier::Episodic,
            _ => MemoryTier::Semantic,
        }
    }

    /// The record's tier.
    pub fn of(record: &MemoryRecord) -> MemoryTier {
        Self::classify(record.scope.scope, record.kind)
    }

    /// The wire name (`working` / `episodic` / `semantic`).
    pub fn as_str(&self) -> &'static str {
        match self {
            MemoryTier::Working => "working",
            MemoryTier::Episodic => "episodic",
            MemoryTier::Semantic => "semantic",
        }
    }
}

// --------------------------------------------------------------------- //
// The key grammar
// --------------------------------------------------------------------- //

/// A parsed hierarchical key: `domain.name` (e.g. `user.timezone`,
/// `tool.search.quirks`). The domain segment — everything before the first
/// dot — is what consolidation policies and scoped retention rules target;
/// the name may itself carry dots.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HierarchicalKey {
    /// The first segment; the namespace half of "namespace = scope address
    /// + key domain".
    pub domain: String,

    /// Everything after the first dot.
    pub name: String,
}

impl HierarchicalKey {
    /// Parse and validate a key: at least one dot, no empty segments, every
    /// segment lowercase ASCII alphanumeric plus `_`. The grammar is
    /// deliberately small — keys are the retrieval contract, and a contract
    /// stays readable.
    pub fn parse(key: &str) -> Result<HierarchicalKey> {
        let valid_segment = |segment: &str| {
            !segment.is_empty()
                && segment
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        };
        let Some((domain, name)) = key.split_once('.') else {
            return Err(invalid(format!(
                "memory key `{key}` is not hierarchical: the grammar is `domain.name` — the \
                 domain segment is what consolidation policies and scoped retention rules target"
            )));
        };
        if !valid_segment(domain) || !name.split('.').all(valid_segment) {
            return Err(invalid(format!(
                "memory key `{key}` violates the segment grammar: segments are non-empty \
                 lowercase ASCII alphanumeric plus `_`, dot-separated"
            )));
        }
        Ok(HierarchicalKey {
            domain: domain.to_owned(),
            name: name.to_owned(),
        })
    }
}

/// The declared key domains a deployment accepts at the write gate. A write
/// whose key parses but names an undeclared domain fails the gate: an
/// undeclared domain is how "convention" keys rot into an ungoverned
/// namespace.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyGrammar {
    /// The declared domains, sorted (a `BTreeSet`: the serialized shape is
    /// deterministic).
    pub domains: std::collections::BTreeSet<String>,
}

impl KeyGrammar {
    /// Declare the accepted domains. Fails on a domain that is not a single
    /// valid segment — the domain is the segment before the first dot,
    /// never a pattern.
    pub fn declare(domains: impl IntoIterator<Item = impl Into<String>>) -> Result<KeyGrammar> {
        let mut set = std::collections::BTreeSet::new();
        for domain in domains {
            let domain = domain.into();
            HierarchicalKey::parse(&format!("{domain}.x"))
                .map_err(|_| {
                    invalid(format!(
                        "key grammar domain `{domain}` is not a valid key segment — the domain \
                         is the segment before the first dot, never a pattern"
                    ))
                })?;
            if domain.contains('.') {
                return Err(invalid(format!(
                    "key grammar domain `{domain}` carries a dot — a domain is one segment"
                )));
            }
            set.insert(domain);
        }
        Ok(KeyGrammar { domains: set })
    }

    /// Validate `key` against the grammar: parse it, then require its domain
    /// declared. Returns the parsed key on success.
    pub fn validate(&self, key: &str) -> Result<HierarchicalKey> {
        let parsed = HierarchicalKey::parse(key)?;
        if !self.domains.contains(&parsed.domain) {
            return Err(invalid(format!(
                "memory key `{key}` names undeclared domain `{}` — declare it in the write \
                 gate's key grammar; an undeclared domain is an ungoverned namespace",
                parsed.domain
            )));
        }
        Ok(parsed)
    }
}

// --------------------------------------------------------------------- //
// The write gate: key validation + content-equal dedup
// --------------------------------------------------------------------- //

/// What the write gate did with a submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateOutcome {
    /// The record was stored and journaled as its own write.
    Stored,
    /// An independent submission carried the same scope, key, and
    /// canonical content as a live record: the write converged onto the
    /// existing record's id — the idempotent-effect convergence the shipped
    /// seam gives retried submissions, extended to independent ones.
    Converged,
}

/// The gate's answer: the id the write resolved to, and how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatedWrite {
    /// The content address the write resolved to (the record's own id when
    /// stored, the existing record's when converged).
    pub memory_id: String,

    /// What happened.
    pub outcome: GateOutcome,
}

/// The memory write gate: key-grammar validation and content-equal dedup in
/// front of the shipped journaled write. One gate per writing context;
/// `None` grammar accepts any well-formed key.
#[derive(Debug, Clone, Default)]
pub struct WriteGate {
    grammar: Option<KeyGrammar>,
}

impl WriteGate {
    /// A gate without a declared grammar (well-formed keys only).
    pub fn new() -> Self {
        Self::default()
    }

    /// Builder-style: the declared key grammar.
    pub fn with_grammar(mut self, grammar: KeyGrammar) -> Self {
        self.grammar = Some(grammar);
        self
    }

    /// Write `record` through the gate.
    ///
    /// Order of decisions: key grammar first (a malformed or undeclared key
    /// fails loud, nothing journaled), then dedup — a live record at the
    /// same scope and key with equal canonical content makes the write
    /// converge onto its id. The convergence *is* journaled: the gate
    /// issues the journaled write of the record it converged onto, so the
    /// run's evidence names what its write resolved to under the converged
    /// effect key. Only live records dedup — an expired or superseded
    /// same-content record does not block a fresh assertion. Same key with
    /// different content is never dedup: supersession (attributed) or
    /// conflict detection (inferred), unchanged.
    ///
    /// `now` is the run-clock instant expiry is evaluated against — the
    /// caller stamps it, exactly as the shipped read stamps `as_of`.
    pub async fn write<S: MemoryStore + ?Sized>(
        &self,
        memory: &JournaledMemory,
        store: &S,
        record: &MemoryRecord,
        now: DateTime<Utc>,
        parent: Option<String>,
    ) -> Result<GatedWrite> {
        if let Some(key) = record.key.as_deref() {
            if let Some(grammar) = &self.grammar {
                grammar.validate(key)?;
            } else {
                HierarchicalKey::parse(key)?;
            }
        }
        if record.key.is_some() {
            let live = apply_query(
                &store.all().await?,
                &MemoryQuery {
                    scope: Some(record.scope.clone()),
                    key: record.key.clone(),
                    ..MemoryQuery::default()
                },
                now,
            );
            let content_hash = record.content.content_hash()?;
            for existing in &live {
                if existing.memory_id != record.memory_id
                    && existing.content.content_hash()? == content_hash
                {
                    let memory_id = memory.write(existing, parent).await?;
                    return Ok(GatedWrite {
                        memory_id,
                        outcome: GateOutcome::Converged,
                    });
                }
            }
        }
        let memory_id = memory.write(record, parent).await?;
        Ok(GatedWrite {
            memory_id,
            outcome: GateOutcome::Stored,
        })
    }
}

// --------------------------------------------------------------------- //
// Consolidation scheduling
// --------------------------------------------------------------------- //

/// When consolidation runs: the declarative trigger half of the shipped
/// journaled consolidation operation. Carried as the additive optional
/// `maintenance` member of `memory_config` candidates (the design's home
/// decision: consolidation scheduling is memory-plane configuration, so it
/// extends the existing family rather than minting a variant).
///
/// Trigger semantics: **any** declared threshold crossed makes the policy
/// due — each threshold is an independent tripwire, and a policy declares
/// the ones it cares about. At least one threshold must be declared
/// ([`ConsolidationPolicy::validate`]): a policy with no trigger never
/// fires, which is a schedule that exists only to be promoted — a
/// configuration error, not a policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsolidationPolicy {
    /// The scope family whose records the policy watches (any concrete id
    /// inside the scope).
    pub scope: MemoryScope,

    /// The key domain the policy targets (`episode` targets `episode.*`).
    pub key_domain: String,

    /// Trigger: the live, unconsolidated record count in the scope+domain
    /// reaches this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_records: Option<u32>,

    /// Trigger: the aggregate estimated-token footprint of the live,
    /// unconsolidated records reaches this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,

    /// Trigger: the oldest live, unconsolidated record is older than this
    /// many milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_age_ms: Option<u64>,

    /// The distiller the triggered consolidation invokes (application code;
    /// the runtime owns the record invariants).
    pub distiller: String,
}

impl ConsolidationPolicy {
    /// The contract checks the gate cannot express structurally: at least
    /// one trigger declared, the distiller named, the domain a single valid
    /// key segment.
    pub fn validate(&self) -> Result<()> {
        if self.max_records.is_none() && self.max_tokens.is_none() && self.max_age_ms.is_none() {
            return Err(invalid(
                "a consolidation policy declares no trigger — a schedule that never fires is a \
                 configuration error, not a policy",
            ));
        }
        if self.distiller.trim().is_empty() {
            return Err(invalid(
                "a consolidation policy must name its distiller — the distillation semantics \
                 are application code, and an unnamed one cannot be invoked or audited",
            ));
        }
        KeyGrammar::declare([self.key_domain.clone()])?;
        Ok(())
    }
}

/// Which thresholds fired, in declared order. Closed enum — consumers match
/// exhaustively.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsolidationTrigger {
    /// The record-count threshold fired.
    RecordCount,
    /// The aggregate token-footprint threshold fired.
    TokenFootprint,
    /// The oldest-record age threshold fired.
    Age,
}

/// A due consolidation: the source records to distill (live,
/// domain-matched, oldest-first), the thresholds that fired, and the
/// distiller to invoke. The distiller's output then lands through the
/// shipped [`crate::memory::consolidation_summary`], which names these
/// sources — superseding them in default retrieval and keeping
/// dependent-summary invalidation computable on forgetting.
#[derive(Debug, Clone, PartialEq)]
pub struct ConsolidationDue {
    /// The consolidation inputs, oldest first (ties by content address).
    pub sources: Vec<MemoryRecord>,

    /// The thresholds that fired, in declared order.
    pub triggered_by: Vec<ConsolidationTrigger>,

    /// The policy's distiller.
    pub distiller: String,
}

/// Evaluate `policy` against `universe` (one tenant's namespace) at `now`:
/// the live, unconsolidated records in the policy's scope family and key
/// domain, and whether any declared threshold is crossed. Pure — the
/// server-seam note in the module docs names who runs it and when.
///
/// "Unconsolidated" is the shipped definition: a record a `summary` names
/// in its sources is superseded ([`crate::memory::superseded_set`]), and
/// superseded records are out — compression precedes erasure, and a
/// consolidated record is already compressed.
pub fn consolidation_due(
    policy: &ConsolidationPolicy,
    universe: &[MemoryRecord],
    now: DateTime<Utc>,
    margin_percent: u32,
) -> Result<Option<ConsolidationDue>> {
    policy.validate()?;
    let superseded = crate::memory::superseded_set(universe);
    let domain_prefix = format!("{}.", policy.key_domain);
    let mut sources: Vec<MemoryRecord> = universe
        .iter()
        .filter(|record| {
            record.scope.scope == policy.scope
                && record
                    .key
                    .as_deref()
                    .is_some_and(|key| key.starts_with(&domain_prefix))
                && !superseded.contains(record.memory_id.as_str())
                && record.expires_at.is_none_or(|expires| expires > now)
        })
        .cloned()
        .collect();
    sources.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.memory_id.cmp(&b.memory_id))
    });

    let mut triggered_by = Vec::new();
    if policy
        .max_records
        .is_some_and(|max| sources.len() as u32 >= max)
    {
        triggered_by.push(ConsolidationTrigger::RecordCount);
    }
    if policy.max_tokens.is_some_and(|max| {
        sources
            .iter()
            .map(|record| estimated_tokens(record.content_bytes(), margin_percent))
            .sum::<u32>()
            >= max
    }) {
        triggered_by.push(ConsolidationTrigger::TokenFootprint);
    }
    if let (Some(max_age_ms), Some(oldest)) = (policy.max_age_ms, sources.first()) {
        let age_ms = (now - oldest.created_at).num_milliseconds().max(0) as u64;
        if age_ms >= max_age_ms {
            triggered_by.push(ConsolidationTrigger::Age);
        }
    }
    if triggered_by.is_empty() {
        return Ok(None);
    }
    Ok(Some(ConsolidationDue {
        sources,
        triggered_by,
        distiller: policy.distiller.clone(),
    }))
}

// --------------------------------------------------------------------- //
// The rank policy (the `rank` member of memory_config candidates)
// --------------------------------------------------------------------- //

/// How far the utility signal may move retrieval order, and the over-fetch
/// the two-stage assembly reads. The additive optional `rank` member of
/// `memory_config` candidates — promoted through the gate with replay +
/// experiment evidence, rolled back by pointer. Integer arithmetic only,
/// the [`crate::tool_select::SelectionWeights`] vocabulary: scores are
/// byte-reproducible on every platform.
///
/// The default is the floor — utility weight zero, no over-fetch — under
/// which the driver's within-tier order is the shipped rank. Always legal,
/// always the baseline every candidate is measured against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RankPolicy {
    /// Points per smoothed success basis point. Zero is the floor: utility
    /// moves nothing and the shipped rank decides within each tier.
    pub utility_weight: u32,

    /// The stage-one over-fetch, in percent of the section's true budget:
    /// 200 reads twice the budget into the candidate pool the re-rank
    /// selects from. Must be at least [`DEFAULT_OVER_FETCH_PERCENT`] — an
    /// under-fetch would shrink the pool below the shipped read.
    pub over_fetch_percent: u32,
}

impl Default for RankPolicy {
    fn default() -> Self {
        Self {
            utility_weight: 0,
            over_fetch_percent: DEFAULT_OVER_FETCH_PERCENT,
        }
    }
}

impl RankPolicy {
    /// The one validity rule: no under-fetch.
    pub fn validate(&self) -> Result<()> {
        if self.over_fetch_percent < DEFAULT_OVER_FETCH_PERCENT {
            return Err(invalid(format!(
                "rank policy over-fetch {}% under-fetches the section budget (minimum {}%) — \
                 the candidate pool must hold at least what the shipped read would return",
                self.over_fetch_percent, DEFAULT_OVER_FETCH_PERCENT
            )));
        }
        Ok(())
    }
}

// --------------------------------------------------------------------- //
// The utility index: derived, rebuildable, never on the record
// --------------------------------------------------------------------- //

/// A completed run's outcome, as the roll-up consumes it: the terminal
/// status plus the eval plane's score where one graded the run. Supplied by
/// the caller — the roll-up reads journals, not run receipts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunOutcome {
    /// The run's terminal status. Only [`crate::record::EventStatus::Ok`]
    /// runs count as successes; `Error` runs count as failures;
    /// `Interrupted` runs are excluded — a suspended run is not terminal
    /// evidence of memory quality.
    pub status: crate::record::EventStatus,

    /// The eval score, in basis points (0–10 000), where the run was
    /// graded.
    pub score_bps: Option<u32>,
}

/// One completed run's evidence for the roll-up: its journal plus its
/// outcome.
pub struct UtilityRun<'a> {
    /// The run's journal (integrity-verified by the caller's replay
    /// machinery, the [`MemoryReplaySource`] assumption).
    pub snapshot: &'a JournalSnapshot,

    /// The run's outcome.
    pub outcome: RunOutcome,
}

/// One record's utility: the counts of successful-run and failed-run
/// assemblies it appeared in. Raw counts only — the smoothing is the
/// consumption rule ([`UtilityEntry::smoothed_success_bps`]), so the index
/// stores honest evidence and the prior lives in exactly one place.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UtilityEntry {
    /// Assemblies of successful runs the record appeared in.
    pub successful_uses: u64,

    /// Assemblies of failed runs the record appeared in.
    pub failed_uses: u64,
}

impl UtilityEntry {
    /// The smoothed success rate in basis points: Laplace-smoothed
    /// `(successes + 1) / (uses + 2)`, so an unobserved record sits at the
    /// neutral prior and a single success does not read as certainty.
    /// Integer arithmetic, byte-reproducible everywhere.
    pub fn smoothed_success_bps(&self) -> u32 {
        ((self.successful_uses + 1) * 10_000 / (self.successful_uses + self.failed_uses + 2))
            as u32
    }
}

/// The derived utility index: per memory id, the usefulness counts rolled
/// up from completed journals, stamped with the instant the roll-up read
/// as-of. **Derived, never stored on the record** — the index is a
/// disposable projection: rebuilding it from the same journals, outcomes,
/// and stamp reproduces it byte-identically (the checkpoint/artifact
/// discipline applied to an index).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UtilityIndex {
    /// The roll-up's as-of stamp — the pin the journaled section manifest
    /// carries, so a consumer (and an auditor) knows which snapshot moved
    /// the rank.
    pub stamp: DateTime<Utc>,

    /// Per-record counts, keyed by content address (a `BTreeMap`: the
    /// serialized shape is deterministic).
    pub entries: BTreeMap<String, UtilityEntry>,
}

impl UtilityIndex {
    /// The record's smoothed success rate in basis points; the neutral
    /// prior ([`NEUTRAL_SUCCESS_BPS`]) when the index has never seen the
    /// record.
    pub fn success_bps(&self, memory_id: &str) -> u32 {
        self.entries
            .get(memory_id)
            .map(UtilityEntry::smoothed_success_bps)
            .unwrap_or(NEUTRAL_SUCCESS_BPS)
    }
}

/// Roll completed runs' journals into the utility index at `stamp`.
///
/// Per run, a record counts once (appearance is per-run, not per-read — a
/// run that read the same record twice was helped once). Success is the
/// terminal status plus, where the run was graded *and*
/// `success_min_score_bps` is set, the score bar: a graded run below the
/// bar counts as a failure — it completed, poorly. An ungraded run counts
/// by status alone regardless of the bar. Pure over its inputs; equal
/// inputs produce a byte-identical index.
pub fn build_utility_index(
    runs: &[UtilityRun],
    success_min_score_bps: Option<u32>,
    stamp: DateTime<Utc>,
) -> Result<UtilityIndex> {
    let mut entries: BTreeMap<String, UtilityEntry> = BTreeMap::new();
    for run in runs {
        use crate::record::EventStatus;
        let succeeded = match run.outcome.status {
            EventStatus::Ok => {
                match (run.outcome.score_bps, success_min_score_bps) {
                    (Some(score), Some(min)) => score >= min,
                    _ => true,
                }
            }
            EventStatus::Error => false,
            // A suspended run is not terminal evidence of memory quality.
            EventStatus::Interrupted => continue,
        };
        let mut seen = std::collections::BTreeSet::new();
        for event in &run.snapshot.events {
            if event.kind != RunEventKind::MemoryRead {
                continue;
            }
            for memory_id in assembly_ids(run.snapshot, event)? {
                seen.insert(memory_id);
            }
        }
        for memory_id in seen {
            let entry = entries.entry(memory_id).or_default();
            if succeeded {
                entry.successful_uses += 1;
            } else {
                entry.failed_uses += 1;
            }
        }
    }
    Ok(UtilityIndex { stamp, entries })
}

/// The record ids a journaled [`RunEventKind::MemoryRead`] assembly carried
/// — resolved through the snapshot's artifact map, parsed as the shipped
/// [`MemoryAssembly`] (a tiered read's `section_manifest` extension member
/// is ignored, the additive-extension rule). A read with no resolvable
/// output is an inconsistent journal and fails loud.
fn assembly_ids(snapshot: &JournalSnapshot, event: &RunEvent) -> Result<Vec<String>> {
    let payload = event.output.as_ref().ok_or_else(|| {
        invalid(format!(
            "journaled memory read at seq {} carries no output payload — the journal is \
             inconsistent",
            event.seq
        ))
    })?;
    let value = match payload {
        PayloadRef::Inline(value) => value.clone(),
        PayloadRef::Artifact(reference) => snapshot
            .artifacts
            .get(&reference.sha256)
            .cloned()
            .ok_or_else(|| {
                invalid(format!(
                    "journaled memory read at seq {} references artifact {}, which the snapshot \
                     does not hold — the journal is inconsistent",
                    event.seq, reference.sha256
                ))
            })?,
    };
    let assembly: MemoryAssembly = serde_json::from_value(value).map_err(|e| {
        invalid(format!(
            "journaled memory read at seq {} does not parse as a memory assembly: {e}",
            event.seq
        ))
    })?;
    Ok(assembly.memory_ids)
}

/// Memory-hygiene candidates: expired records with no recorded successful
/// use. **Flagged, never reaped** — the output feeds review or a
/// `memory_set` candidate; forgetting stays the shipped journaled,
/// tombstoned operation, and no policy auto-deletes.
pub fn forgetting_candidates(
    index: &UtilityIndex,
    universe: &[MemoryRecord],
    now: DateTime<Utc>,
) -> Vec<String> {
    let superseded = crate::memory::superseded_set(universe);
    let mut candidates: Vec<String> = universe
        .iter()
        .filter(|record| {
            record.expires_at.is_some_and(|expires| expires <= now)
                && !superseded.contains(record.memory_id.as_str())
                && index
                    .entries
                    .get(&record.memory_id)
                    .is_none_or(|entry| entry.successful_uses == 0)
        })
        .map(|record| record.memory_id.clone())
        .collect();
    candidates.sort();
    candidates
}

// --------------------------------------------------------------------- //
// The two-stage tiered assembly driver
// --------------------------------------------------------------------- //

/// Where a tiered assembly reads from: the live store (record mode) or a
/// recorded journal (replay mode) — the [`crate::memory::MemorySource`]
/// discipline mirrored, so a driver cannot tell the difference either.
#[derive(Debug, Clone)]
pub enum TieredMemorySource {
    /// Read from a live [`MemoryStore`], journaling the assembly.
    Store(Arc<dyn MemoryStore>),

    /// Serve the journaled tiered read byte-identically and re-journal it.
    Replay(MemoryReplaySource),
}

/// One tier's portion of the packed section: the record ids it carried and
/// their estimated-token cost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierPortion {
    /// The tier.
    pub tier: MemoryTier,

    /// The packed records' content addresses, in carried order.
    pub ids: Vec<String>,

    /// The portion's estimated-token cost.
    pub used_tokens: u32,
}

/// The re-rank pin journaled with every tiered read (the `section_manifest`
/// member of the read's output payload): the weights the driver applied,
/// the utility snapshot it read, and the over-fetched candidate pool's ids
/// in the shipped rank's order — everything needed to re-derive the packed
/// section from the journal plus the store's immutable records.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TierSectionManifest {
    /// The manifest format version ([`TIER_MANIFEST_FORMAT_VERSION`]).
    pub format: String,

    /// The rank policy the assembly ran under.
    pub rank: RankPolicy,

    /// The utility snapshot's stamp.
    pub utility_stamp: DateTime<Utc>,

    /// The over-fetched candidate pool, in the shipped rank's order
    /// (content addresses — the pool re-resolves byte-identically while
    /// its records live).
    pub over_fetch_ids: Vec<String>,

    /// Per-tier packed portions, in assembly order.
    pub tiers: Vec<TierPortion>,
}

/// The driver's output: the packed, tier-ranked records; the accounting the
/// final pack applied; and the journaled manifest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TieredMemorySection {
    /// The packed records, in assembly order (tier-major, utility within
    /// tier, the shipped rank the floor).
    pub records: Vec<MemoryRecord>,

    /// The estimated-token accounting the final pack applied.
    pub token_accounting: TokenAccounting,

    /// `true` when the true budget cut the re-ranked pool short.
    pub truncated: bool,

    /// The journaled re-rank pin.
    pub manifest: TierSectionManifest,
}

impl TieredMemorySection {
    /// The packed records' content addresses, in assembly order.
    pub fn memory_ids(&self) -> Vec<String> {
        self.records
            .iter()
            .map(|record| record.memory_id.clone())
            .collect()
    }
}

/// The two-stage assembly driver: over-fetch through the shipped rank,
/// re-rank and re-pack outside it, one journaled [`RunEventKind::MemoryRead`]
/// carrying the result plus the re-rank pins. Constructed per run with the
/// promoted [`RankPolicy`] and the utility snapshot — both resolved at
/// admission, so the driver a replay builds is the driver the recorded run
/// ran.
#[derive(Debug, Clone)]
pub struct TieredMemoryDriver {
    rank: RankPolicy,
    utility: UtilityIndex,
}

impl TieredMemoryDriver {
    /// A driver under `rank` over the `utility` snapshot. Fails on an
    /// under-fetching policy ([`RankPolicy::validate`]).
    pub fn new(rank: RankPolicy, utility: UtilityIndex) -> Result<Self> {
        rank.validate()?;
        Ok(Self { rank, utility })
    }

    /// The floor: the default rank policy over an empty snapshot. Legal
    /// everywhere; the baseline every re-rank candidate is measured
    /// against.
    pub fn floor(stamp: DateTime<Utc>) -> Self {
        Self {
            rank: RankPolicy::default(),
            utility: UtilityIndex {
                stamp,
                entries: BTreeMap::new(),
            },
        }
    }

    /// The rank policy this driver assembles under.
    pub fn rank(&self) -> &RankPolicy {
        &self.rank
    }

    /// The utility snapshot this driver reads.
    pub fn utility(&self) -> &UtilityIndex {
        &self.utility
    }

    /// The record's utility score: weight times smoothed success basis
    /// points. Zero weight flattens every score to zero — the stable sort
    /// then keeps the shipped rank's order within each tier.
    fn score(&self, record: &MemoryRecord) -> u64 {
        u64::from(self.rank.utility_weight) * u64::from(self.utility.success_bps(&record.memory_id))
    }

    /// Assemble the tiered memory section for `query` under `budget` (the
    /// section's true budget), journaling exactly one
    /// [`RunEventKind::MemoryRead`] — or, replaying, serving the journaled
    /// one byte-identically and re-journaling it.
    ///
    /// Clock-read parity mirrors the shipped seam: one `as_of` stamp (when
    /// the query leaves it unset), two latency reads, one read inside
    /// [`Journal::record`], in both modes — a logical clock's tick sequence
    /// stays aligned with the recorded run.
    ///
    /// Replay divergence is loud: the served manifest's weights and utility
    /// stamp must equal the driver's — a replayed run assembling under a
    /// different rank pin than its evidence has diverged.
    pub async fn assemble_section(
        &self,
        journal: &Journal,
        source: &TieredMemorySource,
        query: &MemoryQuery,
        budget: &ContextBudget,
        parent: Option<String>,
    ) -> Result<TieredMemorySection> {
        let mut resolved = query.clone();
        let as_of = resolved.as_of.unwrap_or_else(|| journal.clock().now());
        resolved.as_of = Some(as_of);
        let request = memory_read_request(&resolved, budget);
        match source {
            TieredMemorySource::Store(store) => {
                let started = journal.clock().now();
                let records = store.query(&resolved, as_of).await?;
                let latency_ms = (journal.clock().now() - started)
                    .num_milliseconds()
                    .max(0) as u64;
                let (assembly, manifest) = self.rerank(records, budget)?;
                let mut output = serde_json::to_value(&assembly)?;
                output
                    .as_object_mut()
                    .expect("a serialized assembly is an object")
                    .insert(
                        "section_manifest".to_owned(),
                        serde_json::to_value(&manifest)?,
                    );
                let mut draft = EventDraft::new(RunEventKind::MemoryRead, Effect::ReadOnly)
                    .input(request)
                    .output(output)
                    .latency_ms(latency_ms);
                if let Some(parent) = parent {
                    draft = draft.parent(parent);
                }
                journal.record(draft);
                Ok(TieredMemorySection {
                    records: assembly.records,
                    token_accounting: assembly.token_accounting,
                    truncated: assembly.truncated,
                    manifest,
                })
            }
            TieredMemorySource::Replay(source) => {
                let served = source.serve(&request)?;
                // Clock-read parity with the live path (the two latency
                // reads), the shipped replay discipline.
                let _started = journal.clock().now();
                let _ended = journal.clock().now();
                let parent = parent
                    .or_else(|| served.event.parent.clone())
                    .unwrap_or_default();
                served.rejournal(journal, parent);
                let output = served.output.clone().ok_or_else(|| {
                    replay_error(format!(
                        "recorded memory read at seq {} carries no assembly payload — the \
                         journal is inconsistent",
                        served.event.seq
                    ))
                })?;
                let manifest_value = output.get("section_manifest").cloned().ok_or_else(|| {
                    replay_error(format!(
                        "recorded memory read at seq {} carries no section manifest — the \
                         tiered driver replays only its own journaled evidence",
                        served.event.seq
                    ))
                })?;
                let manifest: TierSectionManifest = serde_json::from_value(manifest_value)?;
                if manifest.rank != self.rank || manifest.utility_stamp != self.utility.stamp {
                    return Err(replay_error(format!(
                        "replay divergence at recorded seq {} (tiered memory read): the run \
                         assembles under rank {:?} with utility stamp {}, but the journaled \
                         manifest pins rank {:?} with stamp {} — the replayed run has diverged \
                         from its evidence",
                        served.event.seq,
                        self.rank,
                        self.utility.stamp,
                        manifest.rank,
                        manifest.utility_stamp
                    )));
                }
                let assembly: MemoryAssembly = serde_json::from_value(output)?;
                Ok(TieredMemorySection {
                    records: assembly.records,
                    token_accounting: assembly.token_accounting,
                    truncated: assembly.truncated,
                    manifest,
                })
            }
        }
    }

    /// The two stages, pure: over-fetch under the shipped rank, then
    /// tier-major re-rank (utility within tier, the shipped rank the
    /// tie-breaking floor — the input arrives in [`assemble`] order and the
    /// sort is stable) and re-pack against the true budget.
    ///
    /// The declared overflow rule binds at the final pack. With no
    /// over-fetch, stage one *is* the shipped read — declared overflow
    /// included; with over-fetch, stage one always truncates (the pool is a
    /// candidate set, and the final pack is where the budget's rule
    /// applies).
    fn rerank(
        &self,
        records: Vec<MemoryRecord>,
        budget: &ContextBudget,
    ) -> Result<(MemoryAssembly, TierSectionManifest)> {
        let over_fetch = ContextBudget {
            max_tokens: scale_budget(budget.max_tokens, self.rank.over_fetch_percent),
            margin_percent: budget.margin_percent,
            overflow: if self.rank.over_fetch_percent > DEFAULT_OVER_FETCH_PERCENT {
                crate::memory::BudgetOverflow::Truncate
            } else {
                budget.overflow
            },
        };
        let pool = assemble(records, &over_fetch)?;
        let over_fetch_ids = pool.memory_ids.clone();

        let mut ranked = pool.records;
        ranked.sort_by(|a, b| {
            MemoryTier::of(a)
                .cmp(&MemoryTier::of(b))
                .then_with(|| self.score(b).cmp(&self.score(a)))
        });

        let mut used_tokens: u32 = 0;
        let mut packed: Vec<MemoryRecord> = Vec::new();
        let mut truncated = false;
        for record in ranked {
            let cost = estimated_tokens(record.content_bytes(), budget.margin_percent);
            if used_tokens.saturating_add(cost) > budget.max_tokens {
                match budget.overflow {
                    crate::memory::BudgetOverflow::Truncate => {
                        truncated = true;
                        break;
                    }
                    crate::memory::BudgetOverflow::Fail => {
                        return Err(invalid(format!(
                            "the tiered memory section exceeds the context budget: record `{}` \
                             costs an estimated {cost} tokens with {used_tokens} of {} already \
                             used — the re-ranked pool does not fit, and the budget was \
                             declared hard",
                            record.memory_id, budget.max_tokens
                        )));
                    }
                }
            }
            used_tokens = used_tokens.saturating_add(cost);
            packed.push(record);
        }

        let mut tiers: Vec<TierPortion> = Vec::new();
        for record in &packed {
            let tier = MemoryTier::of(record);
            let cost = estimated_tokens(record.content_bytes(), budget.margin_percent);
            match tiers.last_mut() {
                Some(portion) if portion.tier == tier => {
                    portion.ids.push(record.memory_id.clone());
                    portion.used_tokens = portion.used_tokens.saturating_add(cost);
                }
                _ => tiers.push(TierPortion {
                    tier,
                    ids: vec![record.memory_id.clone()],
                    used_tokens: cost,
                }),
            }
        }

        let memory_ids = packed
            .iter()
            .map(|record| record.memory_id.clone())
            .collect();
        let assembly = MemoryAssembly {
            memory_ids,
            records: packed,
            token_accounting: TokenAccounting {
                bytes_per_token: TOKEN_BYTES_PER_ESTIMATE,
                margin_percent: budget.margin_percent,
                budget_tokens: budget.max_tokens,
                used_tokens,
            },
            truncated,
        };
        let manifest = TierSectionManifest {
            format: TIER_MANIFEST_FORMAT_VERSION.to_owned(),
            rank: self.rank,
            utility_stamp: self.utility.stamp,
            over_fetch_ids,
            tiers,
        };
        Ok((assembly, manifest))
    }
}

/// The over-fetched budget: `max_tokens` scaled by `percent`, saturated at
/// `u32::MAX` (an over-large pool only means more re-rank candidates).
fn scale_budget(max_tokens: u32, percent: u32) -> u32 {
    ((max_tokens as u128) * (percent as u128) / 100).min(u32::MAX as u128) as u32
}

/// Re-derive a packed section from the journaled pins: the over-fetched
/// pool (re-resolved from the store by content address, in the journaled
/// order) re-ranked and re-packed under the manifest's weights and the
/// utility snapshot. Byte-equality with the journaled assembly is the
/// re-derivation property the design's wave-2 exit requires — the audit
/// walk's check that the driver packed what its pins say it packed.
pub fn rederive_section(
    driver: &TieredMemoryDriver,
    pool: Vec<MemoryRecord>,
    budget: &ContextBudget,
    manifest: &TierSectionManifest,
) -> Result<TieredMemorySection> {
    if manifest.rank != driver.rank {
        return Err(invalid(format!(
            "the journaled manifest pins rank {:?} but the driver runs {:?} — re-derivation \
             under different pins is divergence, not verification",
            manifest.rank, driver.rank
        )));
    }
    let (assembly, derived) = driver.rerank(pool, budget)?;
    if derived.over_fetch_ids != manifest.over_fetch_ids {
        return Err(invalid(
            "the re-derived candidate pool differs from the journaled one — the store no \
             longer holds the pool the recorded assembly ranked (forgotten records end \
             re-derivation; the journal remains the evidence)",
        ));
    }
    Ok(TieredMemorySection {
        records: assembly.records,
        token_accounting: assembly.token_accounting,
        truncated: assembly.truncated,
        manifest: derived,
    })
}
