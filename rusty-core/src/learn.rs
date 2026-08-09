//! The learning loop's candidate pipeline (R0.8 Rusty Learn, wave 3): the
//! candidate contract, the evaluation composition seam, the promotion
//! envelope and its gate, canary binding by seeded draw, and the
//! active-version pointer with byte-exact rollback.
//!
//! The design doc is `docs/learn-design.md` ("The learning loop"). Its
//! learning rule governs everything here: **no learning process may
//! silently rewrite a production prompt, graph, policy, memory, or tool
//! permission.** Learning produces an immutable candidate; the candidate is
//! evaluated against recorded evidence; promotion is a journaled runtime
//! transition bounded by a declared envelope; rollback re-points an
//! immutable version pointer. What a framework does with a config file
//! edit and a restart, this module does as an evidence-carrying state
//! transition.
//!
//! - [`Candidate`] — the distiller's output: an immutable, versioned,
//!   content-addressed declaration of a proposed change. Identity is
//!   `sha256` over the canonical content ([`derive_candidate_id`]), so two
//!   distillations of the same change converge on one id and a tampered
//!   candidate fails its own address ([`Candidate::verify_address`]).
//!   Closed enum [`CandidateKind`]: `prompt` / `policy` / `memory_set` /
//!   `tool_permission`.
//! - [`CandidateEvaluation`] — the journaled evaluation payload: the
//!   replay divergence summary, the experiment report pair, the dataset
//!   version, and the comparison verdict. The evaluator itself is a seam
//!   ([`CandidateEvaluator`]), not runtime code — the same boundary open
//!   question 2 draws for distillers, forced by the workspace's dependency
//!   direction: `rusty-eval` links the runtime, never the reverse, so the
//!   runtime owns the journaled *shape* of the evaluation and the
//!   application owns producing it through `rusty-eval`'s
//!   `ExperimentRunner` + `compare()`. [`EvaluationVerdict`] and
//!   [`EvaluationThresholds`] are the deliberate mirrors of `compare()`'s
//!   output and `CompareThresholds` — one serde-pinned record reference,
//!   the same way `rusty-eval` already meets the runtime at the `Journal`.
//! - [`PromotionEnvelope`] — the declared, per-deployment standing
//!   approval: per candidate kind, what may auto-promote (the evidence
//!   thresholds), what requires a human approval, and what promotes into
//!   a canary. The gate is [`admit_promotion`], a pure function; refusal
//!   is a typed error ([`PromotionRefusal`]), never a silent no-op.
//!   Out-of-envelope promotion requires an
//!   [`crate::effects::ApprovalToken`] scoped to
//!   [`promotion_effect_id`] — derived over the candidate's content hash
//!   and target scope, so an approval for one candidate is
//!   non-transferable to another. This composes the effect kernel rather
//!   than inventing an approval parallel to it.
//! - [`VersionPointer`] — the active version per surface (prompt name,
//!   policy scope, memory scope, tool grant) as an immutable pointer to a
//!   [`CandidateId`]. Every promotion is a pointer move; rollback
//!   re-points to the previous candidate and is byte-exact because
//!   candidates are content-addressed and immutable — the restored
//!   version is the one that previously served, not a reconstruction.
//!
//! Every lifecycle transition (created → evaluated → promoted → rolled
//! back) journals through four additive [`RunEventKind`](crate::record::RunEventKind) variants —
//! `CandidateCreated` / `CandidateEvaluated` / `CandidatePromoted` /
//! `CandidateRolledBack` — the same evolution rule R0.6's `EffectReceipt`
//! and R0.7's agent variants followed; old journals keep deserializing.
//! Refused transitions are errors ([`LearnError`]), not silent no-ops.
//!
//! Golden-file tests under `tests/golden/` pin every wire shape in this
//! module; any accidental contract drift fails CI.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::effects::{derive_effect_id, ApprovalToken, EffectId};
use crate::error::{Result, RustyError};
use crate::journal::JournalSnapshot;
use crate::memory::{
    apply_query, MemoryQuery, MemoryRecord, MemoryScope, MemoryStore, ProvenanceAuthor,
    ScopeAddress,
};
use crate::record::{sha256_hex, DecisionFamily, RunManifest};

fn invalid(message: impl Into<String>) -> RustyError {
    // Candidate construction and application are state updates to the
    // governed learning plane; contract validation failures reuse the
    // invalid-update class rather than growing the error taxonomy for one
    // module (the memory module's own convention).
    RustyError::InvalidUpdate(message.into())
}

// --------------------------------------------------------------------- //
// The candidate contract
// --------------------------------------------------------------------- //

/// A content-addressed candidate identity: lowercase hex SHA-256 over the
/// candidate's canonical content ([`derive_candidate_id`]).
///
/// Transparent newtype so the type system — not convention — keeps
/// candidate ids distinct from memory addresses, effect ids, and other
/// digest strings (the [`PolicyVersion`](crate::record::PolicyVersion) /
/// [`EffectId`] precedent).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CandidateId(String);

impl CandidateId {
    /// The hex digest.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CandidateId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for CandidateId {
    /// Wrap a digest the caller already trusts — rehydration from storage,
    /// a rollback receipt's `previous`. Minting ids from content is
    /// [`derive_candidate_id`]'s job alone; nothing here re-verifies.
    fn from(digest: String) -> Self {
        Self(digest)
    }
}

/// What a candidate would change. Closed enum — the gate, the pointer,
/// and the evaluator match exhaustively on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateKind {
    /// New prompt text for one named prompt.
    Prompt,
    /// Executor policy parameters for one decision family.
    Policy,
    /// A set of memory records (adds and supersessions).
    MemorySet,
    /// A narrowed or widened tool grant.
    ToolPermission,
}

/// The direction of a [`CandidateContent::ToolPermission`] change.
/// Closed enum: the direction is the R0.8 contract — grant mechanics
/// (which capabilities a narrowed grant drops, how a widened one is
/// bounded) are R0.9's capsule-manifest work, and this release gates
/// every tool-permission candidate behind a human approval regardless.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantDirection {
    /// The grant shrinks (a capability is withdrawn).
    Narrow,
    /// The grant grows (a capability is added).
    Widen,
}

/// The change a candidate declares, per [`CandidateKind`]. Serialized with
/// internal tagging (`{"kind": "memory_set", …}`): the kind is part of
/// the content address — a prompt text and a policy parameter set that
/// happened to serialize alike must never converge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CandidateContent {
    /// New prompt text. The text's digest — `sha256_hex` over its UTF-8
    /// bytes — is exactly the pin [`RunManifest::pin_prompt`] records, so
    /// the candidate and the run manifest speak one content address.
    Prompt {
        /// The prompt's name (the manifest pin's key).
        name: String,
        /// The proposed prompt text.
        prompt: String,
    },
    /// Executor policy parameters for one
    /// [`crate::record::DecisionFamily`] — backoff caps, timeout bounds,
    /// concurrency limits. The closed legal-action sets stay closed: a
    /// learned policy chooses among declared actions, never free-form
    /// outputs.
    Policy {
        /// The decision family the parameters govern.
        family: DecisionFamily,
        /// The parameter set (schema is the policy registry's, wave 4).
        parameters: Value,
    },
    /// A set of memory records at one scope: adds and supersessions.
    /// Carrying full [`MemoryRecord`]s — not ids — is what lets the set
    /// carry wave 2's attributed correction candidates: the records'
    /// provenance (and therefore their own content addresses) is inside
    /// this candidate's address.
    MemorySet {
        /// The scope the set applies at.
        scope: ScopeAddress,
        /// The records the set adds (already content-addressed, already
        /// immutable — a record in two sets is the same record).
        adds: Vec<MemoryRecord>,
        /// Bare memory content addresses the set supersedes beyond the
        /// adds' own `supersedes` fields.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        supersedes: Vec<String>,
    },
    /// A narrowed or widened tool grant.
    ToolPermission {
        /// The tool whose grant changes.
        tool: String,
        /// The direction of the change (see [`GrantDirection`]).
        direction: GrantDirection,
    },
}

/// The content address of a candidate: `sha256` over the canonical
/// serialization of its content — the one hashing primitive shared with
/// artifact references and journal heads, over the canonical `serde_json`
/// serialization [`crate::record::PayloadRef::content_hash`] also relies
/// on (object keys sort deterministically).
pub fn derive_candidate_id(content: &CandidateContent) -> Result<CandidateId> {
    Ok(CandidateId(sha256_hex(&serde_json::to_vec(content)?)))
}

/// The evidence span a distiller read to produce a candidate — journaled
/// with creation, because a candidate that cannot name its observations
/// cannot be audited. Every field is optional and absent from the wire
/// when empty: a hand-authored candidate may carry no span at all, and
/// sparse evidence must not change the shape for dense readers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceSpan {
    /// The recorded runs whose journals the distiller read. "Completed"
    /// is load-bearing (the design's observe stage): learning reads
    /// terminal evidence, never in-flight state.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub run_ids: Vec<String>,

    /// The corrections (R0.8 wave 2) the candidate folds in — the
    /// reference distiller's input.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub correction_ids: Vec<String>,

    /// The memory records the distiller read (e.g. the attributed
    /// candidates a `memory_set` carries).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub memory_ids: Vec<String>,
}

impl EvidenceSpan {
    /// `true` when no span is carried (the sparse wire shape).
    pub fn is_empty(&self) -> bool {
        self.run_ids.is_empty() && self.correction_ids.is_empty() && self.memory_ids.is_empty()
    }
}

/// A proposed change: immutable, versioned, content-addressed.
///
/// The address covers the **content** — the change itself — and nothing
/// else: two distillations of the same change converge on one id even
/// when different distillers read different evidence to reach it (the
/// design's identity-is-integrity rule). Attribution is not identity, so
/// the distiller, the evidence span, and the learning instant travel
/// beside the address rather than inside it — the inverse of
/// [`MemoryRecord`]'s provenance-in-identity rule, deliberate: a memory
/// record answers "who claims this," a candidate answers "what would
/// change."
///
/// Immutable by construction: a changed candidate is a new id, and there
/// is no in-place update anywhere in the pipeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    /// The content address ([`derive_candidate_id`]).
    pub candidate_id: CandidateId,

    /// The change, per kind.
    pub content: CandidateContent,

    /// The distiller's identity. Mandatory — journaled with creation
    /// (`CandidateCreated`), because a candidate that cannot name its
    /// distiller is indistinguishable from a config edit.
    pub distilled_by: ProvenanceAuthor,

    /// The evidence span the distiller read. Absent from the wire when
    /// empty.
    #[serde(default, skip_serializing_if = "EvidenceSpan::is_empty")]
    pub evidence: EvidenceSpan,

    /// When the distiller minted the candidate.
    pub created_at: DateTime<Utc>,
}

impl Candidate {
    /// Build a candidate, deriving its content address from `content`.
    pub fn new(
        content: CandidateContent,
        distilled_by: ProvenanceAuthor,
        evidence: EvidenceSpan,
        created_at: DateTime<Utc>,
    ) -> Result<Self> {
        let candidate_id = derive_candidate_id(&content)?;
        Ok(Self {
            candidate_id,
            content,
            distilled_by,
            evidence,
            created_at,
        })
    }

    /// The candidate's kind.
    pub fn kind(&self) -> CandidateKind {
        match &self.content {
            CandidateContent::Prompt { .. } => CandidateKind::Prompt,
            CandidateContent::Policy { .. } => CandidateKind::Policy,
            CandidateContent::MemorySet { .. } => CandidateKind::MemorySet,
            CandidateContent::ToolPermission { .. } => CandidateKind::ToolPermission,
        }
    }

    /// The production surface this candidate would change — the pointer
    /// key: `prompt:{name}` / `policy:{family}` / `memory:{scope}` /
    /// `tool:{tool}`. One surface admits one active version, which is
    /// what makes promotion a pointer move and rollback a re-pointing.
    pub fn surface(&self) -> SurfaceKey {
        match &self.content {
            CandidateContent::Prompt { name, .. } => SurfaceKey::new(format!("prompt:{name}")),
            CandidateContent::Policy { family, .. } => {
                let family = serde_json::to_value(family)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_else(|| format!("{family:?}").to_lowercase());
                SurfaceKey::new(format!("policy:{family}"))
            }
            CandidateContent::MemorySet { scope, .. } => {
                SurfaceKey::new(format!("memory:{}", scope.as_address()))
            }
            CandidateContent::ToolPermission { tool, .. } => {
                SurfaceKey::new(format!("tool:{tool}"))
            }
        }
    }

    /// Re-derive the content address and compare: `Ok` when the candidate
    /// is the one its id names. Identity is integrity — the gate runs
    /// this before any promotion, so a tampered candidate fails closed at
    /// the boundary, not after a pointer moved.
    pub fn verify_address(&self) -> std::result::Result<(), LearnError> {
        let derived = derive_candidate_id(&self.content)
            .map_err(|e| LearnError::UnaddressableContent(e.to_string()))?;
        if derived != self.candidate_id {
            return Err(LearnError::AddressMismatch {
                claimed: self.candidate_id.clone(),
                derived,
            });
        }
        Ok(())
    }

    /// Apply a `prompt` candidate to a manifest: substitute the named
    /// prompt's pin with the candidate's — the same digest
    /// [`RunManifest::pin_prompt`] records, so the evaluation's manifest
    /// speaks the production surface's content address. Every other kind
    /// fails: a candidate applies only to the surface its kind declares.
    pub fn apply_to_manifest(&self, manifest: &mut RunManifest) -> Result<()> {
        let CandidateContent::Prompt { name, prompt } = &self.content else {
            return Err(invalid(format!(
                "only a `prompt` candidate applies to a run manifest; `{}` candidates apply \
                 to their own surfaces",
                self.kind_str()
            )));
        };
        manifest
            .prompts
            .insert(name.clone(), sha256_hex(prompt.as_bytes()));
        Ok(())
    }

    fn kind_str(&self) -> &'static str {
        match self.kind() {
            CandidateKind::Prompt => "prompt",
            CandidateKind::Policy => "policy",
            CandidateKind::MemorySet => "memory_set",
            CandidateKind::ToolPermission => "tool_permission",
        }
    }
}

/// A candidate's production surface: the pointer key. Transparent newtype
/// over the canonical `{kind}:{scope}` string ([`Candidate::surface`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SurfaceKey(String);

impl SurfaceKey {
    /// Wrap a surface string (built by [`Candidate::surface`]).
    pub fn new(surface: impl Into<String>) -> Self {
        Self(surface.into())
    }

    /// The surface string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SurfaceKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// --------------------------------------------------------------------- //
// Effect keys and the approval scope
// --------------------------------------------------------------------- //

/// The derived idempotency key of a candidate creation:
/// `candidate:{candidate_id}`. Content addressing makes retried
/// submissions converge on one key.
pub fn candidate_effect_key(candidate_id: &CandidateId) -> String {
    format!("candidate:{candidate_id}")
}

/// The derived idempotency key of an evaluation:
/// `evaluation:{candidate_id}:{dataset_version}`. Re-evaluation against
/// the same dataset version converges; a new dataset version is a new
/// evaluation by construction.
pub fn evaluation_effect_key(candidate_id: &CandidateId, dataset_version: &str) -> String {
    format!("evaluation:{candidate_id}:{dataset_version}")
}

/// The derived idempotency key of a promotion: `promotion:{candidate_id}`
/// (the design's mechanism decision). Retried promotions converge, and
/// recovery re-derives the same key.
pub fn promotion_effect_key(candidate_id: &CandidateId) -> String {
    format!("promotion:{candidate_id}")
}

/// The derived idempotency key of a rollback:
/// `rollback:{surface}:{candidate_id}` — one rollback per candidate per
/// surface.
pub fn rollback_effect_key(surface: &SurfaceKey, candidate_id: &CandidateId) -> String {
    format!("rollback:{surface}:{candidate_id}")
}

/// The effect kind promotions derive their deterministic id under.
pub const PROMOTION_EFFECT_KIND: &str = "candidate_promotion";

/// The deterministic effect id of a candidate's promotion —
/// [`derive_effect_id`] over the target surface (scope), the promotion
/// kind, the candidate's content hash (input hash), and the promotion
/// effect key. This is the id an out-of-envelope [`ApprovalToken`] must
/// be scoped to: the scope check makes an approval for one candidate
/// non-transferable to another, and the token's `approved_by` gives the
/// journaled promotion its attribution.
pub fn promotion_effect_id(candidate: &Candidate) -> EffectId {
    derive_effect_id(
        candidate.surface().as_str(),
        PROMOTION_EFFECT_KIND,
        candidate.candidate_id.as_str(),
        Some(&promotion_effect_key(&candidate.candidate_id)),
    )
}

// --------------------------------------------------------------------- //
// The evaluation composition seam
// --------------------------------------------------------------------- //

/// The comparison thresholds an evaluation ran under — the deliberate
/// mirror of `rusty-eval`'s `CompareThresholds`, serde-identical by
/// contract: the evaluator (which links `rusty-eval`) maps one onto the
/// other field-for-field, and the journaled payload speaks this shape so
/// the runtime never links the eval crate (the workspace's dependency
/// direction is `rusty-eval` → runtime, never the reverse).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EvaluationThresholds {
    /// Maximum tolerable absolute pass-rate drop, per assertion and per
    /// case, before the comparison flags a regression.
    pub max_pass_rate_drop: f64,

    /// Maximum tolerable p95 latency ratio `candidate / baseline`.
    pub max_latency_p95_ratio: f64,
}

impl Default for EvaluationThresholds {
    /// 5-point pass-rate drop tolerance, 25% p95 latency tolerance — the
    /// same defaults `CompareThresholds` declares.
    fn default() -> Self {
        Self {
            max_pass_rate_drop: 0.05,
            max_latency_p95_ratio: 1.25,
        }
    }
}

/// The comparison verdict the promotion gate reads — the deliberate
/// mirror of the release-gate bits of `rusty-eval`'s `ComparisonReport`
/// (`regressed` plus the target metric's movement), stated in terms the
/// envelope's evidence thresholds consume: no regression **and**
/// improvement on the target metric over the named dataset version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvaluationVerdict {
    /// `compare()`'s release-gate bit: any threshold breach.
    pub regressed: bool,

    /// The metric the envelope's improvement bar reads (e.g.
    /// `run_pass_rate`, `case:{case_id}`, `assertion:{key}`).
    pub target_metric: String,

    /// The metric's baseline value, when the baseline names it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<f64>,

    /// The metric's candidate value, when the candidate names it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate: Option<f64>,

    /// `candidate - baseline` (negative is worse); absent when either
    /// side lacks the metric — an improvement bar cannot clear on a
    /// metric the evidence does not name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta: Option<f64>,
}

/// One recorded run that diverged under replay with the candidate
/// applied: the fixture and the first disagreement, summarized (the full
/// evidence is the replayed journal, reachable by fixture id).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayDivergence {
    /// The replay fixture (a recorded run's identity) that diverged.
    pub fixture_id: String,

    /// The first disagreement, summarized (kind, sequence, request hash —
    /// enough to locate it without dumping payloads).
    pub detail: String,
}

/// The replay half of an evaluation: recorded runs re-driven with the
/// candidate applied. Exact replay serves journaled effects, so the
/// candidate's behavior is measured against identical evidence with zero
/// outbound calls; divergence detection is the replay engine's own
/// contract, consumed — not re-implemented — here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplaySummary {
    /// Every fixture re-driven (recorded run identities), sorted for
    /// determinism.
    pub fixture_ids: Vec<String>,

    /// How many fixtures replayed without divergence.
    pub matched: usize,

    /// Every fixture that diverged, sorted by fixture id.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub divergences: Vec<ReplayDivergence>,
}

impl ReplaySummary {
    /// `true` when every fixture replayed clean. The auto-promotion bar
    /// requires it: a candidate that cannot reproduce its evidence has no
    /// business in front of a comparison verdict.
    pub fn is_clean(&self) -> bool {
        self.divergences.is_empty() && self.matched == self.fixture_ids.len()
    }
}

/// The journaled evaluation payload — the `CandidateEvaluated` event's
/// output. The evaluation is evidence, not a log line: the verdict, the
/// report pair, the dataset version, and the replay fixture ids, so the
/// improvement is explainable afterward by walking ids (candidate →
/// evaluation → reports → fixtures).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CandidateEvaluation {
    /// The evaluated candidate.
    pub candidate_id: CandidateId,

    /// The versioned dataset both reports ran against. Versioned, never
    /// edited in place — the envelope's auto-promotion bar may name the
    /// exact version a clean verdict must come from.
    pub dataset_version: String,

    /// The replay half: divergence summary plus the fixture ids.
    pub replay: ReplaySummary,

    /// The baseline experiment report, verbatim (`rusty-eval`'s
    /// `ExperimentReport` serialized; carried as a value because the
    /// runtime does not link the eval crate — see the module docs).
    pub baseline_report: Value,

    /// The candidate experiment report, same convention.
    pub candidate_report: Value,

    /// The comparison verdict the gate reads.
    pub verdict: EvaluationVerdict,

    /// The thresholds the comparison applied, echoed so an auditor reads
    /// the bar the verdict was judged against, not a later default.
    pub thresholds: EvaluationThresholds,

    /// Who ran the evaluation (attribution is mandatory here too).
    pub evaluated_by: ProvenanceAuthor,

    /// When the evaluation ran.
    pub evaluated_at: DateTime<Utc>,
}

/// What an evaluator is asked to do: run `candidate` over the declared
/// evidence and produce the journaled [`CandidateEvaluation`].
/// Wire-complete: the server's evaluate route accepts it as the request
/// body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvaluationRequest {
    /// The dataset version both reports must name.
    pub dataset_version: String,

    /// The metric the promotion gate reads.
    pub target_metric: String,

    /// The comparison thresholds.
    pub thresholds: EvaluationThresholds,

    /// The recorded run journals to re-drive with the candidate applied
    /// (the replay half's evidence), already integrity-loadable.
    pub replay_evidence: Vec<JournalSnapshot>,
}

/// The evaluation seam: composition, not duplication. The implementation
/// builds `PreparedRun`s with the candidate applied (for `memory_set`,
/// the candidate's records visible at their scope via
/// [`CandidateOverlay`]; for `prompt`, the pinned prompt hash substituted
/// via [`Candidate::apply_to_manifest`]) and drives them through the real
/// executor — replay over [`EvaluationRequest::replay_evidence`] for the
/// divergence half, `rusty-eval`'s `ExperimentRunner` over the versioned
/// dataset for the report half, `compare()` for the verdict.
///
/// The runtime owns this trait rather than an implementation for the same
/// reason it owns the distiller contract but not distillers (design open
/// question 2), compounded by the workspace's dependency direction:
/// `rusty-eval` links the runtime, never the reverse. Wave 4's release
/// proof implements this trait over `rusty-eval`'s public API; wave 3
/// proves the seam with scripted nodes.
#[async_trait]
pub trait CandidateEvaluator: Send + Sync + std::fmt::Debug {
    /// Evaluate `candidate` against `request`'s evidence. The returned
    /// evaluation must name the candidate and the request's dataset
    /// version — the gate refuses mismatches.
    async fn evaluate(
        &self,
        candidate: &Candidate,
        request: &EvaluationRequest,
    ) -> Result<CandidateEvaluation>;
}

// --------------------------------------------------------------------- //
// Candidate application: the memory overlay
// --------------------------------------------------------------------- //

/// A `memory_set` candidate applied as a read lens over a live store:
/// the candidate's records answer at their scope, its supersessions drop
/// out of default retrieval, and everything else passes through. This is
/// how evaluation (and, post-promotion, new traffic) sees candidate
/// memory without a store migration — the candidate applied, as a store.
///
/// The lens is deliberately read-only: writes fail. An evaluation that
/// writes through the candidate's lens would be mutating production
/// memory mid-measurement — the silent-rewrite pattern the pipeline
/// exists to forbid.
#[derive(Debug)]
pub struct CandidateOverlay {
    base: Arc<dyn MemoryStore>,
    adds: Vec<MemoryRecord>,
    supersedes: HashSet<String>,
}

impl CandidateOverlay {
    /// Apply `candidate` over `base`. Fails for non-`memory_set`
    /// candidates: a candidate applies only to the surface its kind
    /// declares.
    pub fn new(base: Arc<dyn MemoryStore>, candidate: &Candidate) -> Result<Self> {
        let CandidateContent::MemorySet {
            adds, supersedes, ..
        } = &candidate.content
        else {
            return Err(invalid(format!(
                "only a `memory_set` candidate applies as a memory overlay; `{}` candidates \
                 apply to their own surfaces",
                match candidate.kind() {
                    CandidateKind::Prompt => "prompt",
                    CandidateKind::Policy => "policy",
                    CandidateKind::MemorySet => unreachable!(),
                    CandidateKind::ToolPermission => "tool_permission",
                }
            )));
        };
        Ok(Self {
            base,
            adds: adds.clone(),
            supersedes: supersedes.iter().cloned().collect(),
        })
    }
}

#[async_trait]
impl MemoryStore for CandidateOverlay {
    async fn put(&self, _record: &MemoryRecord) -> Result<bool> {
        Err(invalid(
            "the candidate overlay is a read lens: writes through it would mutate production \
             memory mid-evaluation — the silent-rewrite pattern the candidate pipeline exists \
             to forbid",
        ))
    }

    async fn get(&self, memory_id: &str) -> Result<Option<MemoryRecord>> {
        if let Some(record) = self.adds.iter().find(|r| r.memory_id == memory_id) {
            return Ok(Some(record.clone()));
        }
        self.base.get(memory_id).await
    }

    async fn all(&self) -> Result<Vec<MemoryRecord>> {
        // Merge by content address: a candidate record already in the base
        // is the same record (identity is integrity), so the add wins by
        // construction and nothing doubles.
        let mut merged: Vec<MemoryRecord> = self
            .base
            .all()
            .await?
            .into_iter()
            .filter(|record| {
                !self
                    .adds
                    .iter()
                    .any(|add| add.memory_id == record.memory_id)
            })
            .collect();
        merged.extend(self.adds.iter().cloned());
        Ok(merged)
    }

    async fn remove(&self, _memory_id: &str) -> Result<bool> {
        Err(invalid(
            "the candidate overlay is a read lens: deletion through it would mutate production \
             memory mid-evaluation",
        ))
    }

    async fn query(&self, query: &MemoryQuery, now: DateTime<Utc>) -> Result<Vec<MemoryRecord>> {
        // Core's matcher supplies every filter semantic, including the
        // superseded set it computes from the merged universe (the adds'
        // own `supersedes` fields included). The candidate's explicit
        // supersession list is the overlay's own contribution: drop those
        // ids unless the query asked for superseded records.
        let matched = apply_query(&self.all().await?, query, now);
        if query.include_superseded || self.supersedes.is_empty() {
            return Ok(matched);
        }
        Ok(matched
            .into_iter()
            .filter(|record| !self.supersedes.contains(&record.memory_id))
            .collect())
    }
}

// --------------------------------------------------------------------- //
// The promotion envelope and its gate
// --------------------------------------------------------------------- //

/// The evidence thresholds an auto-promotion must clear (the design's
/// rule): a clean replay, `compare()` showing no regression, **and**
/// improvement on the target metric — over the named dataset version,
/// when the deployment names one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutoPromotion {
    /// The dataset version a clean verdict must come from. `None` accepts
    /// the verdict's own version — honest for deployments that version
    /// datasets per evaluation; a named version pins the promotion to the
    /// exact evidence the deployment reviewed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dataset_version: Option<String>,

    /// The target-metric delta must exceed this (strictly). `0.0` is the
    /// design's default: any improvement clears, parity does not.
    pub min_improvement: f64,

    /// The memory scopes a `memory_set` candidate may auto-promote at
    /// (ignored for other kinds; empty = unrestricted). The R0.8 default
    /// names `run` and `agent` (open question 6): wider scopes always
    /// require a human approval this release.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<MemoryScope>,
}

/// What the envelope declares for one candidate kind. Closed enum — the
/// gate matches exhaustively.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum EnvelopeRule {
    /// May auto-promote when the evaluation clears the declared evidence
    /// thresholds. Inside the envelope, the envelope itself is the
    /// standing approval — versioned and declared, not a silent default.
    Auto(AutoPromotion),

    /// Always requires a human [`ApprovalToken`] scoped to
    /// [`promotion_effect_id`], whatever the evidence says. The token's
    /// `approved_by` is the promotion's attribution.
    Approval,

    /// Auto-promotes into a canary: the candidate binds to the declared
    /// fraction of new runs (admission picks by seeded draw —
    /// [`canary_admits`] — so a recorded run reproduces its assignment),
    /// the static version serving the rest. The evidence thresholds gate
    /// entry into the canary, exactly as for [`EnvelopeRule::Auto`].
    Canary {
        /// The fraction of new runs the candidate serves, in `(0, 1]`.
        fraction: f64,
        /// The evidence thresholds gating canary entry.
        auto: AutoPromotion,
    },
}

/// The declared, per-deployment promotion envelope: per candidate kind,
/// what may promote automatically, what requires review, and what
/// canaries. Versioned (`envelope_version`) because the envelope is the
/// standing approval — a journaled promotion names the envelope version
/// it cleared, so the audit reads the declaration that was in force, not
/// a later edit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromotionEnvelope {
    /// The deployment-declared envelope version.
    pub envelope_version: String,

    /// The rule for `prompt` candidates.
    pub prompt: EnvelopeRule,

    /// The rule for `policy` candidates.
    pub policy: EnvelopeRule,

    /// The rule for `memory_set` candidates.
    pub memory_set: EnvelopeRule,

    /// The rule for `tool_permission` candidates.
    pub tool_permission: EnvelopeRule,
}

impl PromotionEnvelope {
    /// The R0.8 default (design open question 6): `memory_set` candidates
    /// at run and agent scope with a clean verdict may auto-promote;
    /// `prompt`, `policy`, and `tool_permission` always require an
    /// approval token this release.
    pub fn r08_default() -> Self {
        Self {
            envelope_version: "r0.8-default".to_owned(),
            prompt: EnvelopeRule::Approval,
            policy: EnvelopeRule::Approval,
            memory_set: EnvelopeRule::Auto(AutoPromotion {
                dataset_version: None,
                min_improvement: 0.0,
                scopes: vec![MemoryScope::Run, MemoryScope::Agent],
            }),
            tool_permission: EnvelopeRule::Approval,
        }
    }

    /// The rule for `kind`.
    pub fn rule_for(&self, kind: CandidateKind) -> &EnvelopeRule {
        match kind {
            CandidateKind::Prompt => &self.prompt,
            CandidateKind::Policy => &self.policy,
            CandidateKind::MemorySet => &self.memory_set,
            CandidateKind::ToolPermission => &self.tool_permission,
        }
    }

    /// The declaration's own validity: canary fractions in `(0, 1]` and
    /// improvement bars non-negative. Checked where envelopes are
    /// declared (deployment configuration), so a malformed envelope fails
    /// at startup, not at a promotion.
    pub fn validate(&self) -> Result<()> {
        for rule in [
            &self.prompt,
            &self.policy,
            &self.memory_set,
            &self.tool_permission,
        ] {
            match rule {
                EnvelopeRule::Auto(auto) => check_auto(auto)?,
                EnvelopeRule::Canary { fraction, auto } => {
                    if !(fraction.is_finite() && *fraction > 0.0 && *fraction <= 1.0) {
                        return Err(invalid(format!(
                            "canary fraction must be in (0, 1], got {fraction} — a fraction \
                             outside the interval binds nothing or everything, and both are \
                             silent full promotions or silent no-ops"
                        )));
                    }
                    check_auto(auto)?;
                }
                EnvelopeRule::Approval => {}
            }
        }
        Ok(())
    }
}

fn check_auto(auto: &AutoPromotion) -> Result<()> {
    if !(auto.min_improvement.is_finite() && auto.min_improvement >= 0.0) {
        return Err(invalid(format!(
            "min_improvement must be finite and non-negative, got {} — a negative bar would \
             auto-promote regressions, which is the one outcome the gate exists to refuse",
            auto.min_improvement
        )));
    }
    Ok(())
}

/// A canary binding: the candidate serves `fraction` of new runs on its
/// surface, admission picking by seeded draw ([`canary_admits`]); the
/// static version (or the pointer's full `active`) serves the rest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanaryBinding {
    /// The canaried candidate.
    pub candidate_id: CandidateId,

    /// The fraction of new runs the candidate serves, in `(0, 1]`.
    pub fraction: f64,
}

/// Domain-separation prefix for [`canary_admits`] — canary draws must
/// never collide with effect ids, journal heads, or any other digest in
/// the system.
pub const CANARY_DRAW_DOMAIN: &str = "rusty/canary-draw/v1";

/// The canary admission draw: whether `run_id` binds `binding`'s
/// candidate on `surface`.
///
/// **Seed derivation, documented (the design's reproducibility rule).**
/// The draw is `SHA-256` over the newline-joined tuple
/// `CANARY_DRAW_DOMAIN "\n" surface "\n" candidate_id "\n" run_id`,
/// read as a `u64` from the digest's first eight bytes (big-endian) and
/// compared against `fraction × u64::MAX`. Every input is journaled
/// evidence — the surface and candidate id are on the pointer the run
/// bound, the run id is the journal's own identity — so a recorded run
/// re-derives its assignment exactly, and the draw needs no RNG state of
/// its own. The candidate id's presence makes two canaries on one
/// surface independent draws; the run id's presence makes one canary's
/// assignments uniform across traffic.
pub fn canary_admits(binding: &CanaryBinding, surface: &SurfaceKey, run_id: &str) -> bool {
    if binding.fraction >= 1.0 {
        return true;
    }
    let material = [
        CANARY_DRAW_DOMAIN,
        surface.as_str(),
        binding.candidate_id.as_str(),
        run_id,
    ]
    .join("\n");
    // The first eight bytes of the digest as a big-endian u64: uniform
    // over the digest space, so `draw < fraction × MAX` admits a
    // `fraction` share of runs.
    let mut hasher = Sha256::new();
    hasher.update(material.as_bytes());
    let digest = hasher.finalize();
    let draw = u64::from_be_bytes(digest[..8].try_into().expect("eight bytes"));
    (draw as f64) < binding.fraction * (u64::MAX as f64)
}

/// Why a promotion was refused. A dedicated error type (the
/// [`crate::effects::EffectViolation`] precedent): refusals are contract
/// outcomes surfaced to the caller, not runtime failures — and a refused
/// promotion changes nothing.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum PromotionRefusal {
    /// The candidate was never evaluated. Promotion is gated on evidence;
    /// there is no evidence-free path, not even with an approval — a
    /// human approves *reviewed evidence*, never a blank check.
    #[error(
        "candidate `{candidate_id}` has no journaled evaluation: promotion is gated on \
         evidence — a human approves reviewed evidence, never a blank check"
    )]
    NotEvaluated {
        /// The candidate refused.
        candidate_id: CandidateId,
    },

    /// The evaluation on record is not this candidate's.
    #[error(
        "evaluation names candidate `{evaluation}` but promotion was requested for \
         `{candidate_id}` — the evidence must be the candidate's own"
    )]
    EvaluationMismatch {
        /// The candidate refused.
        candidate_id: CandidateId,
        /// The candidate the journaled evaluation names.
        evaluation: CandidateId,
    },

    /// The replay half diverged: the candidate could not reproduce its
    /// recorded evidence.
    #[error(
        "candidate `{candidate_id}` diverged under replay ({divergences} fixture(s)): a \
         candidate that cannot reproduce its evidence never auto-promotes"
    )]
    ReplayDiverged {
        /// The candidate refused.
        candidate_id: CandidateId,
        /// How many fixtures diverged.
        divergences: usize,
    },

    /// `compare()` flagged a regression beyond the declared thresholds.
    #[error(
        "candidate `{candidate_id}` regressed against its baseline: the auto-promotion bar \
         is no regression AND improvement — the evidence failed the first half"
    )]
    EvaluationRegressed {
        /// The candidate refused.
        candidate_id: CandidateId,
    },

    /// The envelope names a dataset version the evaluation did not run.
    #[error(
        "envelope requires a clean verdict over dataset `{required}` but the evaluation \
         ran `{evaluated}` — the promotion bar names its evidence exactly"
    )]
    DatasetVersionMismatch {
        /// The version the envelope names.
        required: String,
        /// The version the evaluation ran.
        evaluated: String,
    },

    /// A `memory_set` candidate at a scope the auto rule does not cover.
    #[error(
        "memory_set candidates at `{scope:?}` scope do not auto-promote under this envelope — \
         the R0.8 default covers run and agent scope; wider scopes require an approval"
    )]
    ScopeNotAutoPromotable {
        /// The scope that fell outside the rule.
        scope: MemoryScope,
    },

    /// The target metric did not improve past the declared bar.
    #[error(
        "target metric moved by {delta:?} but the envelope requires improvement past \
         {required} — parity is not improvement, and the auto bar does not clear on it"
    )]
    InsufficientImprovement {
        /// The observed delta (`None`: the evidence did not name the
        /// metric at all).
        delta: Option<f64>,
        /// The bar the envelope declared.
        required: f64,
    },

    /// The candidate falls outside the envelope's auto paths: a human
    /// approval scoped to the named effect id is required.
    #[error(
        "this promotion is outside the envelope: present an approval token scoped to \
         effect id {effect_id} — the scope check makes an approval for one candidate \
         non-transferable to another"
    )]
    RequiresApproval {
        /// The effect id the approval must be scoped to
        /// ([`promotion_effect_id`]).
        effect_id: EffectId,
    },

    /// A token was presented, but it approves a different effect id.
    #[error(
        "approval token for {presented} does not admit this promotion ({required}) — an \
         approval is scoped to exactly one candidate's promotion and is not transferable"
    )]
    ApprovalMismatch {
        /// The effect id the admission requires.
        required: EffectId,
        /// The effect id the presented token approves.
        presented: EffectId,
    },
}

/// The learn plane's typed errors: address integrity, lifecycle
/// transitions, and promotion refusals. Refused operations change
/// nothing — there is no silent no-op anywhere in the pipeline.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum LearnError {
    /// A candidate's id does not re-derive from its content — the
    /// tampered-candidate case, failing closed at the gate.
    #[error(
        "candidate address mismatch: id {claimed} does not re-derive from the content \
         (derived {derived}) — identity is integrity, and a tampered candidate fails its \
         own address"
    )]
    AddressMismatch {
        /// The id the candidate claims.
        claimed: CandidateId,
        /// The id its content derives.
        derived: CandidateId,
    },

    /// The candidate's content could not be serialized for addressing —
    /// unreachable for well-formed content, surfaced rather than
    /// panicked.
    #[error("candidate content could not be addressed: {0}")]
    UnaddressableContent(String),

    /// A lifecycle transition the state machine does not admit (e.g.
    /// promoting a candidate that was never evaluated, rolling back one
    /// that was never promoted).
    #[error(
        "invalid candidate transition: cannot {action} from status `{from:?}` — refused \
         transitions are errors, not silent no-ops"
    )]
    InvalidTransition {
        /// The status the transition was attempted from.
        from: CandidateStatus,
        /// The attempted transition.
        action: &'static str,
    },

    /// A receipt presented for one candidate names another.
    #[error(
        "receipt names candidate `{named}` but the record is `{candidate_id}` — a receipt \
         applies to the candidate it was minted for, never to another"
    )]
    ReceiptMismatch {
        /// The candidate the record holds.
        candidate_id: CandidateId,
        /// The candidate the receipt names.
        named: CandidateId,
    },

    /// The promotion gate refused.
    #[error("{0}")]
    Refused(#[from] PromotionRefusal),
}

/// The promotion gate's positive decision: the authority the promotion
/// clears under, journaled on the receipt — the envelope version (the
/// standing approval) or the token's `approved_by` — plus the canary
/// binding, when the promotion is a canary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromotionDecision {
    /// Who authorized the promotion.
    pub authority: PromotionAuthority,

    /// The canary binding, when the envelope declared a canary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canary: Option<CanaryBinding>,
}

/// The authority a promotion cleared under. Closed enum — the journaled
/// attribution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "authority", rename_all = "snake_case")]
pub enum PromotionAuthority {
    /// The envelope's auto path cleared: the envelope itself is the
    /// standing approval, and the journaled version is the declaration
    /// that was in force.
    Envelope {
        /// The envelope version that admitted the promotion.
        envelope_version: String,
    },
    /// A human approved out-of-envelope: the token's `approved_by` is the
    /// attribution.
    Approval {
        /// Who approved (the token's `approved_by`).
        approved_by: String,
    },
}

/// The promotion gate (the design's promote stage), a pure function over
/// the envelope, the candidate, its journaled evaluation, and an optional
/// approval token.
///
/// Order of decisions: integrity first ([`Candidate::verify_address`] —
/// a tampered candidate fails closed before any policy reads it), then
/// evidence (a journaled evaluation of *this* candidate must exist —
/// approvals review evidence, they never replace it), then the envelope
/// rule for the candidate's kind: auto paths check the evidence
/// thresholds (clean replay, no regression, improvement past the
/// declared bar, over the named dataset version, at an admitted scope),
/// the approval path requires a token scoped to [`promotion_effect_id`].
/// Refusal is a typed [`PromotionRefusal`]; nothing here mutates
/// anything.
pub fn admit_promotion(
    envelope: &PromotionEnvelope,
    candidate: &Candidate,
    evaluation: Option<&CandidateEvaluation>,
    approval: Option<&ApprovalToken>,
) -> std::result::Result<PromotionDecision, LearnError> {
    candidate.verify_address()?;
    let evaluation = evaluation.ok_or(PromotionRefusal::NotEvaluated {
        candidate_id: candidate.candidate_id.clone(),
    })?;
    if evaluation.candidate_id != candidate.candidate_id {
        return Err(PromotionRefusal::EvaluationMismatch {
            candidate_id: candidate.candidate_id.clone(),
            evaluation: evaluation.candidate_id.clone(),
        }
        .into());
    }
    match envelope.rule_for(candidate.kind()) {
        EnvelopeRule::Auto(auto) => {
            check_evidence_bar(auto, candidate, evaluation)?;
            Ok(PromotionDecision {
                authority: PromotionAuthority::Envelope {
                    envelope_version: envelope.envelope_version.clone(),
                },
                canary: None,
            })
        }
        EnvelopeRule::Canary { fraction, auto } => {
            check_evidence_bar(auto, candidate, evaluation)?;
            Ok(PromotionDecision {
                authority: PromotionAuthority::Envelope {
                    envelope_version: envelope.envelope_version.clone(),
                },
                canary: Some(CanaryBinding {
                    candidate_id: candidate.candidate_id.clone(),
                    fraction: *fraction,
                }),
            })
        }
        EnvelopeRule::Approval => {
            let required = promotion_effect_id(candidate);
            let token = approval.ok_or(PromotionRefusal::RequiresApproval {
                effect_id: required.clone(),
            })?;
            if !token.admits(&required) {
                return Err(PromotionRefusal::ApprovalMismatch {
                    required,
                    presented: token.effect_id().clone(),
                }
                .into());
            }
            Ok(PromotionDecision {
                authority: PromotionAuthority::Approval {
                    approved_by: token.approved_by().to_owned(),
                },
                canary: None,
            })
        }
    }
}

/// The auto paths' evidence bar (the design's thresholds): clean replay,
/// no regression, improvement past the declared bar on the target
/// metric, over the named dataset version, at an admitted scope.
fn check_evidence_bar(
    auto: &AutoPromotion,
    candidate: &Candidate,
    evaluation: &CandidateEvaluation,
) -> std::result::Result<(), LearnError> {
    if !evaluation.replay.is_clean() {
        return Err(PromotionRefusal::ReplayDiverged {
            candidate_id: candidate.candidate_id.clone(),
            divergences: evaluation.replay.divergences.len(),
        }
        .into());
    }
    if evaluation.verdict.regressed {
        return Err(PromotionRefusal::EvaluationRegressed {
            candidate_id: candidate.candidate_id.clone(),
        }
        .into());
    }
    if let Some(required) = &auto.dataset_version {
        if required != &evaluation.dataset_version {
            return Err(PromotionRefusal::DatasetVersionMismatch {
                required: required.clone(),
                evaluated: evaluation.dataset_version.clone(),
            }
            .into());
        }
    }
    if let CandidateContent::MemorySet { scope, .. } = &candidate.content {
        if !auto.scopes.is_empty() && !auto.scopes.contains(&scope.scope) {
            return Err(PromotionRefusal::ScopeNotAutoPromotable { scope: scope.scope }.into());
        }
    }
    let clears = evaluation
        .verdict
        .delta
        .is_some_and(|delta| delta > auto.min_improvement);
    if !clears {
        return Err(PromotionRefusal::InsufficientImprovement {
            delta: evaluation.verdict.delta,
            required: auto.min_improvement,
        }
        .into());
    }
    Ok(())
}

// --------------------------------------------------------------------- //
// The lifecycle record and the active-version pointer
// --------------------------------------------------------------------- //

/// Where a candidate is in its lifecycle. Closed enum; transitions are
/// journaled events, and refused transitions are errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateStatus {
    /// Distilled, not yet evaluated.
    Created,
    /// Evaluated against recorded evidence; the evaluation is on record.
    Evaluated,
    /// Promoted: the surface's pointer names this candidate (fully, or as
    /// the canary).
    Promoted,
    /// Rolled back: the pointer re-pointed away from this candidate.
    RolledBack,
}

/// The journaled promotion receipt — the `CandidatePromoted` event's
/// output payload. Plays the effect receipt's role for the learning
/// plane: the durable, attributable confirmation that the pointer moved.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromotionReceipt {
    /// The promoted candidate.
    pub candidate_id: CandidateId,

    /// The surface whose pointer moved.
    pub surface: SurfaceKey,

    /// The pointer's full-traffic value before the move (`None`: the
    /// static version served). Rollback re-points to exactly this — the
    /// byte-exactness comes from candidates being content-addressed and
    /// immutable, so `previous` *is* the version that served, not a
    /// reconstruction of it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<CandidateId>,

    /// The gate's decision: the authority and the canary binding.
    pub decision: PromotionDecision,

    /// When the promotion executed.
    pub promoted_at: DateTime<Utc>,
}

/// The journaled rollback receipt — the `CandidateRolledBack` event's
/// output payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RollbackReceipt {
    /// The surface whose pointer re-pointed.
    pub surface: SurfaceKey,

    /// The candidate rolled back (the version that was serving).
    pub from: CandidateId,

    /// The candidate re-pointed to (`None`: the static version serves
    /// again — revert-to-default is always a legal rollback).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<CandidateId>,

    /// The causing evidence: what prompted the rollback (a drift
    /// observation's journal ids, an operator's statement naming the
    /// evidence). Free text by design — wave 4's drift monitor will
    /// write it with the same shape operators use now.
    pub cause: String,

    /// When the rollback executed.
    pub rolled_back_at: DateTime<Utc>,
}

/// A candidate and its lifecycle state: the store row both server
/// backends persist. Transitions go through the `apply_*` methods, which
/// enforce the state machine (created → evaluated → promoted → rolled
/// back) and refuse everything else as a typed error — the store trusts
/// what the gate admitted, exactly as the memory store trusts the write
/// gate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CandidateRecord {
    /// The candidate itself (immutable).
    pub candidate: Candidate,

    /// Where it is in its lifecycle.
    pub status: CandidateStatus,

    /// The journaled evaluation (set on evaluate; replaced on
    /// re-evaluation — a new dataset version is a new evaluation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluation: Option<CandidateEvaluation>,

    /// The journaled promotion receipt (set on promote).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promotion: Option<PromotionReceipt>,

    /// The journaled rollback receipt (set on rollback).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollback: Option<RollbackReceipt>,
}

impl CandidateRecord {
    /// A freshly created candidate.
    pub fn new(candidate: Candidate) -> Self {
        Self {
            candidate,
            status: CandidateStatus::Created,
            evaluation: None,
            promotion: None,
            rollback: None,
        }
    }

    /// The candidate's id.
    pub fn candidate_id(&self) -> &CandidateId {
        &self.candidate.candidate_id
    }

    /// Record an evaluation: `created → evaluated`, or `evaluated →
    /// evaluated` for re-evaluation (a new dataset version is a new
    /// evaluation; the effect key names both). Refused from `promoted`
    /// and `rolled_back`: re-evaluating a settled candidate reads as a
    /// new distillation, and the model keeps that honest by requiring
    /// one.
    pub fn apply_evaluation(
        &mut self,
        evaluation: CandidateEvaluation,
    ) -> std::result::Result<(), LearnError> {
        match self.status {
            CandidateStatus::Created | CandidateStatus::Evaluated => {}
            other => {
                return Err(LearnError::InvalidTransition {
                    from: other,
                    action: "evaluate",
                })
            }
        }
        if evaluation.candidate_id != self.candidate.candidate_id {
            return Err(PromotionRefusal::EvaluationMismatch {
                candidate_id: self.candidate.candidate_id.clone(),
                evaluation: evaluation.candidate_id,
            }
            .into());
        }
        self.evaluation = Some(evaluation);
        self.status = CandidateStatus::Evaluated;
        Ok(())
    }

    /// Record a promotion: `evaluated → promoted`. The gate
    /// ([`admit_promotion`]) must already have cleared — this method
    /// enforces the state machine, not the evidence bar.
    pub fn apply_promotion(
        &mut self,
        receipt: PromotionReceipt,
    ) -> std::result::Result<(), LearnError> {
        if self.status != CandidateStatus::Evaluated {
            return Err(LearnError::InvalidTransition {
                from: self.status,
                action: "promote",
            });
        }
        if receipt.candidate_id != self.candidate.candidate_id {
            return Err(LearnError::ReceiptMismatch {
                candidate_id: self.candidate.candidate_id.clone(),
                named: receipt.candidate_id,
            });
        }
        self.promotion = Some(receipt);
        self.status = CandidateStatus::Promoted;
        Ok(())
    }

    /// Record a rollback: `promoted → rolled_back`.
    pub fn apply_rollback(
        &mut self,
        receipt: RollbackReceipt,
    ) -> std::result::Result<(), LearnError> {
        if self.status != CandidateStatus::Promoted {
            return Err(LearnError::InvalidTransition {
                from: self.status,
                action: "roll back",
            });
        }
        if receipt.from != self.candidate.candidate_id {
            return Err(LearnError::ReceiptMismatch {
                candidate_id: self.candidate.candidate_id.clone(),
                named: receipt.from,
            });
        }
        self.rollback = Some(receipt);
        self.status = CandidateStatus::RolledBack;
        Ok(())
    }
}

/// The active version of one production surface: an immutable-pointer
/// move away from any version that ever served.
///
/// Two slots, exactly the design's canary shape: `active` is the
/// full-traffic version (`None` — the static version serves, the
/// documented floor); `canary` binds one candidate to a declared fraction
/// of new runs while `active` serves the rest. New runs bind the pointer
/// at admission (the canary by seeded draw — [`canary_admits`]);
/// in-flight runs keep the version their checkpoint header pins, the same
/// conservatism as worker-version pinning and manifest pinning.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VersionPointer {
    /// The surface this pointer governs.
    pub surface: SurfaceKey,

    /// The full-traffic candidate (`None`: the static version serves).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<CandidateId>,

    /// The canary binding, when one is declared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canary: Option<CanaryBinding>,
}

impl VersionPointer {
    /// A pointer with nothing promoted: the static version serves.
    pub fn new(surface: SurfaceKey) -> Self {
        Self {
            surface,
            active: None,
            canary: None,
        }
    }

    /// The pointer after a promotion: a canary promotion binds the
    /// candidate to its fraction (the full-traffic version keeps serving
    /// the rest); a full promotion moves `active` and clears any canary —
    /// a full promotion supersedes the experiment it graduated from.
    pub fn promoted(&self, receipt: &PromotionReceipt) -> VersionPointer {
        let mut next = self.clone();
        if let Some(binding) = &receipt.decision.canary {
            next.canary = Some(binding.clone());
        } else {
            next.active = Some(receipt.candidate_id.clone());
            next.canary = None;
        }
        next
    }

    /// The pointer after a rollback: rolling back the full-traffic
    /// version re-points `active` to the receipt's `to`; rolling back the
    /// canary clears the binding. The caller validates that `from` is the
    /// serving version before building the receipt — this is the move,
    /// not the check.
    pub fn rolled_back(&self, receipt: &RollbackReceipt) -> VersionPointer {
        let mut next = self.clone();
        if self.active.as_ref() == Some(&receipt.from) {
            next.active = receipt.to.clone();
        }
        if self
            .canary
            .as_ref()
            .is_some_and(|binding| binding.candidate_id == receipt.from)
        {
            next.canary = None;
        }
        next
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{InMemoryMemoryStore, MemoryKind, MemoryProvenance, ValidityWindow};
    use serde_json::json;

    fn ts(millis: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp_millis(millis).unwrap()
    }

    fn distiller() -> ProvenanceAuthor {
        ProvenanceAuthor::Distiller {
            name: "correction-loop".into(),
        }
    }

    fn memory_record(content: Value) -> MemoryRecord {
        MemoryRecord::new(
            MemoryKind::Fact,
            ScopeAddress::new(MemoryScope::Agent, "support-1"),
            MemoryProvenance {
                author: ProvenanceAuthor::Human {
                    human_id: "amjad".into(),
                },
                evidence: Default::default(),
                written_at: ts(1_000),
            },
            1.0,
            ValidityWindow::starting(ts(500)),
            ts(1_000),
            content,
        )
        .unwrap()
    }

    fn memory_set_candidate(content: Value) -> Candidate {
        Candidate::new(
            CandidateContent::MemorySet {
                scope: ScopeAddress::new(MemoryScope::Agent, "support-1"),
                adds: vec![memory_record(content)],
                supersedes: Vec::new(),
            },
            distiller(),
            EvidenceSpan {
                run_ids: vec!["run-1".into()],
                ..EvidenceSpan::default()
            },
            ts(2_000),
        )
        .unwrap()
    }

    fn clean_evaluation(candidate: &Candidate, delta: f64) -> CandidateEvaluation {
        CandidateEvaluation {
            candidate_id: candidate.candidate_id.clone(),
            dataset_version: "v3".into(),
            replay: ReplaySummary {
                fixture_ids: vec!["run-1".into()],
                matched: 1,
                divergences: Vec::new(),
            },
            baseline_report: json!({"name": "support@v3", "summary": {"run_pass_rate": 0.5}}),
            candidate_report: json!({"name": "support@v3", "summary": {"run_pass_rate": 0.9}}),
            verdict: EvaluationVerdict {
                regressed: false,
                target_metric: "run_pass_rate".into(),
                baseline: Some(0.5),
                candidate: Some(0.5 + delta),
                delta: Some(delta),
            },
            thresholds: EvaluationThresholds::default(),
            evaluated_by: distiller(),
            evaluated_at: ts(3_000),
        }
    }

    #[test]
    fn candidate_id_is_a_content_address() {
        let a = memory_set_candidate(json!({"tone": "warm"}));
        let b = memory_set_candidate(json!({"tone": "warm"}));
        let c = memory_set_candidate(json!({"tone": "cold"}));
        // Same change converges — even across distillers and instants…
        assert_eq!(a.candidate_id, b.candidate_id);
        let d = Candidate::new(
            a.content.clone(),
            ProvenanceAuthor::System,
            EvidenceSpan::default(),
            ts(9_999),
        )
        .unwrap();
        assert_eq!(a.candidate_id, d.candidate_id);
        // …changed content is a new id.
        assert_ne!(a.candidate_id, c.candidate_id);
        assert_eq!(a.candidate_id.as_str().len(), 64);
    }

    #[test]
    fn tampered_candidate_fails_its_own_address() {
        let mut candidate = memory_set_candidate(json!({"tone": "warm"}));
        candidate.verify_address().unwrap();
        candidate.candidate_id = CandidateId("f".repeat(64));
        assert!(matches!(
            candidate.verify_address(),
            Err(LearnError::AddressMismatch { .. })
        ));
    }

    #[test]
    fn surfaces_are_the_documented_keys() {
        let memory = memory_set_candidate(json!({}));
        assert_eq!(memory.surface().as_str(), "memory:agent:support-1");
        let prompt = Candidate::new(
            CandidateContent::Prompt {
                name: "system".into(),
                prompt: "You are careful.".into(),
            },
            distiller(),
            EvidenceSpan::default(),
            ts(2_000),
        )
        .unwrap();
        assert_eq!(prompt.surface().as_str(), "prompt:system");
        let policy = Candidate::new(
            CandidateContent::Policy {
                family: DecisionFamily::Retry,
                parameters: json!({"max_attempts": 4}),
            },
            distiller(),
            EvidenceSpan::default(),
            ts(2_000),
        )
        .unwrap();
        assert_eq!(policy.surface().as_str(), "policy:retry");
        let tool = Candidate::new(
            CandidateContent::ToolPermission {
                tool: "shell".into(),
                direction: GrantDirection::Narrow,
            },
            distiller(),
            EvidenceSpan::default(),
            ts(2_000),
        )
        .unwrap();
        assert_eq!(tool.surface().as_str(), "tool:shell");
    }

    #[test]
    fn lifecycle_refuses_illegal_transitions() {
        let candidate = memory_set_candidate(json!({}));
        let mut record = CandidateRecord::new(candidate.clone());
        // Promote before evaluate: refused, typed.
        let receipt = PromotionReceipt {
            candidate_id: candidate.candidate_id.clone(),
            surface: candidate.surface(),
            previous: None,
            decision: PromotionDecision {
                authority: PromotionAuthority::Envelope {
                    envelope_version: "r0.8-default".into(),
                },
                canary: None,
            },
            promoted_at: ts(4_000),
        };
        assert!(matches!(
            record.apply_promotion(receipt.clone()),
            Err(LearnError::InvalidTransition {
                from: CandidateStatus::Created,
                action: "promote",
            })
        ));
        // Evaluate, then promote, then refuse re-evaluation and allow rollback.
        record
            .apply_evaluation(clean_evaluation(&candidate, 0.4))
            .unwrap();
        record.apply_promotion(receipt).unwrap();
        assert!(matches!(
            record.apply_evaluation(clean_evaluation(&candidate, 0.5)),
            Err(LearnError::InvalidTransition {
                from: CandidateStatus::Promoted,
                action: "evaluate",
            })
        ));
        record
            .apply_rollback(RollbackReceipt {
                surface: candidate.surface(),
                from: candidate.candidate_id.clone(),
                to: None,
                cause: "operator: drift on run-9".into(),
                rolled_back_at: ts(5_000),
            })
            .unwrap();
        assert_eq!(record.status, CandidateStatus::RolledBack);
    }

    #[test]
    fn gate_admits_in_envelope_and_refuses_out_of_it() {
        let envelope = PromotionEnvelope::r08_default();
        let candidate = memory_set_candidate(json!({"tone": "warm"}));
        let evaluation = clean_evaluation(&candidate, 0.4);
        // In-envelope: memory_set at agent scope, clean verdict → the
        // envelope version is the standing approval.
        let decision = admit_promotion(&envelope, &candidate, Some(&evaluation), None).unwrap();
        assert_eq!(
            decision.authority,
            PromotionAuthority::Envelope {
                envelope_version: "r0.8-default".into()
            }
        );
        // Out-of-envelope: prompt candidates always need a token in R0.8.
        let prompt = Candidate::new(
            CandidateContent::Prompt {
                name: "system".into(),
                prompt: "You are careful.".into(),
            },
            distiller(),
            EvidenceSpan::default(),
            ts(2_000),
        )
        .unwrap();
        let prompt_eval = clean_evaluation(&prompt, 0.4);
        let refused = admit_promotion(&envelope, &prompt, Some(&prompt_eval), None).unwrap_err();
        let effect_id = promotion_effect_id(&prompt);
        assert_eq!(
            refused,
            LearnError::Refused(PromotionRefusal::RequiresApproval {
                effect_id: effect_id.clone()
            })
        );
        // A token for another candidate's promotion does not transfer…
        let wrong = ApprovalToken::approve(promotion_effect_id(&candidate), "ops:amjad");
        assert!(matches!(
            admit_promotion(&envelope, &prompt, Some(&prompt_eval), Some(&wrong)),
            Err(LearnError::Refused(
                PromotionRefusal::ApprovalMismatch { .. }
            ))
        ));
        // …the correctly scoped token admits, with attribution.
        let token = ApprovalToken::approve(effect_id, "ops:amjad");
        let decision =
            admit_promotion(&envelope, &prompt, Some(&prompt_eval), Some(&token)).unwrap();
        assert_eq!(
            decision.authority,
            PromotionAuthority::Approval {
                approved_by: "ops:amjad".into()
            }
        );
    }

    #[test]
    fn gate_enforces_the_evidence_bar() {
        let envelope = PromotionEnvelope::r08_default();
        let candidate = memory_set_candidate(json!({}));
        // Parity is not improvement.
        let parity = clean_evaluation(&candidate, 0.0);
        assert_eq!(
            admit_promotion(&envelope, &candidate, Some(&parity), None).unwrap_err(),
            LearnError::Refused(PromotionRefusal::InsufficientImprovement {
                delta: Some(0.0),
                required: 0.0,
            })
        );
        // Regression refuses even with improvement on the target metric.
        let mut regressed = clean_evaluation(&candidate, 0.4);
        regressed.verdict.regressed = true;
        assert!(matches!(
            admit_promotion(&envelope, &candidate, Some(&regressed), None),
            Err(LearnError::Refused(
                PromotionRefusal::EvaluationRegressed { .. }
            ))
        ));
        // Divergent replay refuses.
        let mut diverged = clean_evaluation(&candidate, 0.4);
        diverged.replay.divergences = vec![ReplayDivergence {
            fixture_id: "run-1".into(),
            detail: "seq 4: request hash mismatch".into(),
        }];
        assert!(matches!(
            admit_promotion(&envelope, &candidate, Some(&diverged), None),
            Err(LearnError::Refused(PromotionRefusal::ReplayDiverged { .. }))
        ));
        // A wider scope than the R0.8 default refuses.
        let tenant_scoped = Candidate::new(
            CandidateContent::MemorySet {
                scope: ScopeAddress::new(MemoryScope::Tenant, "default"),
                adds: vec![],
                supersedes: vec![],
            },
            distiller(),
            EvidenceSpan::default(),
            ts(2_000),
        )
        .unwrap();
        let evaluation = clean_evaluation(&tenant_scoped, 0.4);
        assert!(matches!(
            admit_promotion(&envelope, &tenant_scoped, Some(&evaluation), None),
            Err(LearnError::Refused(
                PromotionRefusal::ScopeNotAutoPromotable {
                    scope: MemoryScope::Tenant
                }
            ))
        ));
        // No evaluation at all refuses, even for an in-envelope kind.
        assert!(matches!(
            admit_promotion(&envelope, &candidate, None, None),
            Err(LearnError::Refused(PromotionRefusal::NotEvaluated { .. }))
        ));
    }

    #[test]
    fn canary_draw_is_deterministic_uniform_and_scoped() {
        let candidate = memory_set_candidate(json!({}));
        let surface = candidate.surface();
        let binding = CanaryBinding {
            candidate_id: candidate.candidate_id.clone(),
            fraction: 0.25,
        };
        // Deterministic per (surface, candidate, run).
        assert_eq!(
            canary_admits(&binding, &surface, "run-7"),
            canary_admits(&binding, &surface, "run-7")
        );
        // Full fraction admits everything.
        let full = CanaryBinding {
            candidate_id: candidate.candidate_id.clone(),
            fraction: 1.0,
        };
        for run in 0..32 {
            assert!(canary_admits(&full, &surface, &format!("run-{run}")));
        }
        // A quarter fraction admits roughly a quarter of traffic (loose
        // bounds: the draw is uniform over the digest space).
        let admitted = (0..1_000)
            .filter(|run| canary_admits(&binding, &surface, &format!("run-{run}")))
            .count();
        assert!(
            (150..=350).contains(&admitted),
            "fraction 0.25 admitted {admitted}/1000 runs"
        );
    }

    #[test]
    fn pointer_moves_on_promotion_and_reponts_on_rollback() {
        let a = memory_set_candidate(json!({"v": "a"}));
        let b = memory_set_candidate(json!({"v": "b"}));
        let pointer = VersionPointer::new(a.surface());
        assert_eq!(pointer.active, None);

        let promote_a = PromotionReceipt {
            candidate_id: a.candidate_id.clone(),
            surface: a.surface(),
            previous: None,
            decision: PromotionDecision {
                authority: PromotionAuthority::Envelope {
                    envelope_version: "r0.8-default".into(),
                },
                canary: None,
            },
            promoted_at: ts(4_000),
        };
        let pointer = pointer.promoted(&promote_a);
        assert_eq!(pointer.active, Some(a.candidate_id.clone()));

        let promote_b = PromotionReceipt {
            candidate_id: b.candidate_id.clone(),
            surface: a.surface(),
            previous: Some(a.candidate_id.clone()),
            decision: promote_a.decision.clone(),
            promoted_at: ts(5_000),
        };
        let pointer = pointer.promoted(&promote_b);
        assert_eq!(pointer.active, Some(b.candidate_id.clone()));

        // Rollback re-points to the previous candidate — byte-exact,
        // because the candidate id IS the content.
        let pointer = pointer.rolled_back(&RollbackReceipt {
            surface: a.surface(),
            from: b.candidate_id.clone(),
            to: promote_b.previous.clone(),
            cause: "drift".into(),
            rolled_back_at: ts(6_000),
        });
        assert_eq!(pointer.active, Some(a.candidate_id.clone()));
    }

    #[tokio::test]
    async fn overlay_applies_the_candidate_as_a_read_lens() {
        let base = Arc::new(InMemoryMemoryStore::new());
        let existing = memory_record(json!({"tone": "flat"}));
        base.put(&existing).await.unwrap();

        let mut added = memory_record(json!({"tone": "warm"}));
        added.supersedes = Some(existing.memory_id.clone());
        let candidate = Candidate::new(
            CandidateContent::MemorySet {
                scope: ScopeAddress::new(MemoryScope::Agent, "support-1"),
                adds: vec![added.clone()],
                supersedes: Vec::new(),
            },
            distiller(),
            EvidenceSpan::default(),
            ts(2_000),
        )
        .unwrap();
        let overlay = CandidateOverlay::new(base.clone(), &candidate).unwrap();
        // The add answers…
        assert_eq!(overlay.get(&added.memory_id).await.unwrap(), Some(added));
        // …the base record still fetches by id…
        assert_eq!(
            overlay.get(&existing.memory_id).await.unwrap(),
            Some(existing.clone())
        );
        // …but drops out of default retrieval via the add's supersedes
        // field (core's matcher, over the merged universe)…
        let listed = overlay
            .query(&MemoryQuery::default(), Utc::now())
            .await
            .unwrap();
        assert_eq!(listed.len(), 1);
        // …and writes through the lens fail.
        assert!(overlay.put(&existing).await.is_err());
    }
}
