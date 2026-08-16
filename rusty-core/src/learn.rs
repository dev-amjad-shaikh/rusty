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
//!   `tool_permission`, extended additively in R0.11 (wave 1) with
//!   `tool_contract` / `model_settings` / `memory_configuration` /
//!   `middleware_composition` — the registry families the Extension Plane
//!   design's artifact table names as new kinds.
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
    apply_query, ContextBudget, MemoryQuery, MemoryRecord, MemoryScope, MemoryStore,
    ProvenanceAuthor, ScopeAddress,
};
use crate::memory_tiers::{ConsolidationPolicy, RankPolicy};
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
/// digest strings (the [`PolicyVersion`] / [`EffectId`] precedent).
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
///
/// R0.11 (Extension Plane, wave 1) extends the enum additively with the
/// registry families the design's artifact table names as new kinds:
/// tool contracts, model settings, memory configurations, and middleware
/// compositions. The evolution rule is the one R0.10 applied when it
/// weighed a `Speculation` family and deferred it: new variants append,
/// golden files pin every wire shape, and old records keep deserializing.
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
    /// The JSON schema a tool's manifest pin digests (R0.11).
    ToolContract,
    /// A model id plus its parameters (R0.11).
    ModelSettings,
    /// Memory retrieval/assembly settings — distinct from
    /// [`CandidateKind::MemorySet`], which carries *records*; this kind
    /// carries the configuration that shapes what reads return (R0.11).
    MemoryConfiguration,
    /// An ordered middleware layer list plus per-layer configuration
    /// (R0.11).
    MiddlewareComposition,
    /// A context assembly policy — section layouts, budget splits, the
    /// tokenizer pin, the compaction trigger (R0.13 wave 1). Surface
    /// `context:{name}`.
    ContextPolicy,
}

impl CandidateKind {
    /// The wire name (`prompt` / `policy` / `memory_set` /
    /// `tool_permission` / `tool_contract` / `model_settings` /
    /// `memory_configuration` / `middleware_composition` /
    /// `context_policy`).
    pub fn as_str(&self) -> &'static str {
        match self {
            CandidateKind::Prompt => "prompt",
            CandidateKind::Policy => "policy",
            CandidateKind::MemorySet => "memory_set",
            CandidateKind::ToolPermission => "tool_permission",
            CandidateKind::ToolContract => "tool_contract",
            CandidateKind::ModelSettings => "model_settings",
            CandidateKind::MemoryConfiguration => "memory_configuration",
            CandidateKind::MiddlewareComposition => "middleware_composition",
            CandidateKind::ContextPolicy => "context_policy",
        }
    }
}

impl std::fmt::Display for CandidateKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
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
    /// The JSON schema a tool's pin digests (R0.11). The digest —
    /// canonical-JSON SHA-256 — is exactly what
    /// [`RunManifest::pin_tool_schema`] records, so the candidate and the
    /// manifest speak one content address, the
    /// [`CandidateContent::Prompt`] precedent applied to tool schemas.
    ToolContract {
        /// The tool whose contract changes (the manifest pin's key).
        tool: String,
        /// The parameters schema the tool declares.
        schema: Value,
    },
    /// A model id plus its parameters (R0.11). The pair is what
    /// [`RunManifest::pin_model`] pins: the provider-precise model id
    /// verbatim, the parameters as a canonical-JSON digest.
    ModelSettings {
        /// The settings' name (the registry artifact's key part).
        name: String,
        /// The provider-precise model identifier (an alias is not a pin).
        model: String,
        /// The parameter set (temperature, seed, token limits, ...).
        parameters: Value,
    },
    /// Memory retrieval/assembly settings (R0.11): the budget and default
    /// filters that shape what a run's memory reads return, plus the
    /// record-schema version the settings assume (the
    /// `manifest.memory_schema` pin). Deliberately a new kind rather than
    /// a [`CandidateContent::MemorySet`] reuse: a memory set carries
    /// *records* at a scope; this carries the configuration reads run
    /// under. Conflating the two would fake coverage.
    MemoryConfiguration {
        /// The configuration's name (the registry artifact's key part).
        name: String,
        /// The default assembly budget reads run under.
        budget: ContextBudget,
        /// The default filters reads start from (a run's own query
        /// narrows further; it never widens past the configuration).
        default_filters: MemoryQuery,
        /// The memory record-schema version the settings assume.
        schema_version: String,
        /// The utility re-rank policy (R0.13 wave 2): how far the derived
        /// utility signal may move retrieval order, and the over-fetch the
        /// two-stage assembly reads. Additive and optional — absent from
        /// the wire while unset, so pre-R0.13 `memory_config` artifacts
        /// keep their shape (and their content addresses) byte-for-byte.
        /// The floor is `RankPolicy::default()` — utility weight zero, no
        /// over-fetch — the `static-v0` of retrieval.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rank: Option<RankPolicy>,
        /// The consolidation schedule (R0.13 wave 2): declarative trigger
        /// thresholds per scope and key domain. Additive and optional —
        /// absent from the wire while empty. A maintenance change alters
        /// *when distillation is proposed*, never a record, so the gate
        /// math is unchanged.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        maintenance: Vec<ConsolidationPolicy>,
    },
    /// An ordered middleware layer list plus per-layer configuration
    /// (R0.11): interception policy versioned like everything else. The
    /// layers are code — the registry versions their composition and
    /// configuration, not their behavior; a layer's logic change is a
    /// deploy, covered by the checkpoint header's `graph_hash` story.
    MiddlewareComposition {
        /// The composition's name (the registry artifact's key part).
        name: String,
        /// The layers in declared order — before-hooks run in this order,
        /// after-hooks in reverse (the chain's onion semantics, governed
        /// here, untouched there).
        layers: Vec<MiddlewareLayerConfig>,
    },
    /// A context assembly policy (R0.13 wave 1): the section layouts,
    /// budget splits, tokenizer pin, and compaction trigger the context
    /// pipeline ([`crate::context`]) assembles under. Deliberately a new
    /// kind rather than an optional-field overload: `memory_config` governs
    /// what reads return; the context policy governs the whole assembly.
    /// `Value`-bodied while the policy schema moves — the shipped
    /// [`CandidateContent::Policy`] precedent (`parameters: Value`); the
    /// typed parse is `ContextPolicy::from_value`, fail-closed on an
    /// unknown schema version.
    ContextPolicy {
        /// The policy's name (the registry artifact's key part).
        name: String,
        /// The policy body (a `context-policy-v1` document).
        policy: Value,
    },
}

/// One layer in a [`CandidateContent::MiddlewareComposition`]: the layer's
/// registered name plus its configuration. A `ToolCallBlocklist`'s policy
/// is configuration; the layer's code is not.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MiddlewareLayerConfig {
    /// The layer's registered name (the
    /// [`crate::middleware::MiddlewareChain::names`] vocabulary).
    pub layer: String,

    /// The layer's configuration, when it declares one — absent from the
    /// wire when unset, so a configuration-free layer (a request logger)
    /// carries no placeholder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<Value>,
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
            CandidateContent::ToolContract { .. } => CandidateKind::ToolContract,
            CandidateContent::ModelSettings { .. } => CandidateKind::ModelSettings,
            CandidateContent::MemoryConfiguration { .. } => CandidateKind::MemoryConfiguration,
            CandidateContent::MiddlewareComposition { .. } => CandidateKind::MiddlewareComposition,
            CandidateContent::ContextPolicy { .. } => CandidateKind::ContextPolicy,
        }
    }

    /// The production surface this candidate would change — the pointer
    /// key: `prompt:{name}` / `policy:{family}` / `memory:{scope}` /
    /// `tool:{tool}` / `tool_contract:{tool}` / `model_settings:{name}` /
    /// `memory_config:{name}` / `middleware:{name}`. One surface admits
    /// one active version, which is what makes promotion a pointer move
    /// and rollback a re-pointing.
    pub fn surface(&self) -> SurfaceKey {
        match &self.content {
            CandidateContent::Prompt { name, .. } => surface_for_kind(CandidateKind::Prompt, name),
            CandidateContent::Policy { family, .. } => {
                let family = serde_json::to_value(family)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_else(|| format!("{family:?}").to_lowercase());
                surface_for_kind(CandidateKind::Policy, &family)
            }
            CandidateContent::MemorySet { scope, .. } => {
                surface_for_kind(CandidateKind::MemorySet, &scope.as_address())
            }
            CandidateContent::ToolPermission { tool, .. } => {
                surface_for_kind(CandidateKind::ToolPermission, tool)
            }
            CandidateContent::ToolContract { tool, .. } => {
                surface_for_kind(CandidateKind::ToolContract, tool)
            }
            CandidateContent::ModelSettings { name, .. } => {
                surface_for_kind(CandidateKind::ModelSettings, name)
            }
            CandidateContent::MemoryConfiguration { name, .. } => {
                surface_for_kind(CandidateKind::MemoryConfiguration, name)
            }
            CandidateContent::MiddlewareComposition { name, .. } => {
                surface_for_kind(CandidateKind::MiddlewareComposition, name)
            }
            CandidateContent::ContextPolicy { name, .. } => {
                surface_for_kind(CandidateKind::ContextPolicy, name)
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
        self.kind().as_str()
    }
}

/// The surface key for one kind's named surface: `prompt:{name}` /
/// `policy:{family}` / `memory:{scope-address}` / `tool:{tool}` /
/// `tool_contract:{tool}` / `model_settings:{name}` /
/// `memory_config:{name}` / `middleware:{name}` / `context:{name}`.
///
/// [`Candidate::surface`] builds its key through this function, and the
/// configuration registry (`crate::registry`) keys artifacts by the same
/// rule — one surface admits one artifact, so a candidate can only ever
/// join the artifact its own surface names.
pub fn surface_for_kind(kind: CandidateKind, name: &str) -> SurfaceKey {
    let prefix = match kind {
        CandidateKind::Prompt => "prompt",
        CandidateKind::Policy => "policy",
        CandidateKind::MemorySet => "memory",
        CandidateKind::ToolPermission => "tool",
        CandidateKind::ToolContract => "tool_contract",
        CandidateKind::ModelSettings => "model_settings",
        CandidateKind::MemoryConfiguration => "memory_config",
        CandidateKind::MiddlewareComposition => "middleware",
        CandidateKind::ContextPolicy => "context",
    };
    SurfaceKey::new(format!("{prefix}:{name}"))
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
// Environment tags on surfaces (R0.11 Extension Plane, wave 1)
// --------------------------------------------------------------------- //

/// The separator between a surface key and its environment tag
/// (`prompt:system@prod`). One character, reserved: artifact names never
/// carry it (the registry's naming rule), so the last separator in a key
/// always introduces the tag.
pub const SURFACE_TAG_SEPARATOR: char = '@';

/// An environment tag on a promotion target: `dev` / `staging` / `prod`
/// by convention, deployment-declared rather than enumerated — R0.11's
/// tags are labels on promotion targets, not deployments (R0.12 builds
/// environments as a control plane). What a tag is *not*: not an isolated
/// store, not a trust boundary. One deployment serves the prod surface
/// and the staging surface from one registry, with envelope strictness
/// per tag.
///
/// The tag rides inside the surface key (`prompt:system@prod`), so the
/// pointer store, hash-named files, transactional moves, and canary slots
/// all work unchanged — the design's open question 1, settled as leaned.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct EnvironmentTag(String);

impl EnvironmentTag {
    /// The longest a tag may be. Tags live inside pointer keys, receipts,
    /// and journaled payloads; a bound keeps a configuration typo from
    /// minting an unbounded key.
    pub const MAX_LEN: usize = 64;

    /// Mint a tag. Refused: empty, over-long, carrying whitespace or
    /// control characters, or carrying [`SURFACE_TAG_SEPARATOR`] or `/`
    /// (the tenant id-prefix separator) — the characters that would make
    /// the tagged key ambiguous downstream. Validation failures reuse the
    /// invalid-update class, the module's convention for contract
    /// violations at construction time.
    pub fn new(tag: impl Into<String>) -> Result<Self> {
        let tag = tag.into();
        let refuse = |reason: &str| invalid(format!("invalid environment tag {tag:?}: {reason}"));
        if tag.is_empty() {
            return Err(refuse(
                "empty — an absent tag is the untagged surface, spelled `None`",
            ));
        }
        if tag.len() > Self::MAX_LEN {
            return Err(refuse("over 64 bytes"));
        }
        if tag
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == SURFACE_TAG_SEPARATOR || c == '/')
        {
            return Err(refuse(
                "carries whitespace, a control character, `@`, or `/` — the first two have no \
                 business in a key, the last two are the tag and tenant separators",
            ));
        }
        Ok(Self(tag))
    }

    /// The tag string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for EnvironmentTag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for EnvironmentTag {
    /// Validated at deserialization (the correction-author precedent): a
    /// malformed tag in a request payload fails the parse, so no code path
    /// downstream ever holds an unvalidated one.
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        EnvironmentTag::new(raw).map_err(serde::de::Error::custom)
    }
}

impl SurfaceKey {
    /// The tagged surface for an environment (`prompt:system` at `prod` →
    /// `prompt:system@prod`). Promotion composes the existing pointer
    /// machinery: the tagged key is just another surface, so the pointer
    /// store, canary slots, and byte-exact rollback work unchanged.
    pub fn tagged(&self, tag: &EnvironmentTag) -> SurfaceKey {
        SurfaceKey::new(format!(
            "{}{}{}",
            self.0,
            SURFACE_TAG_SEPARATOR,
            tag.as_str()
        ))
    }

    /// Split a possibly tagged key into its base surface and tag, at the
    /// last [`SURFACE_TAG_SEPARATOR`] (names never carry the separator, so
    /// the last one always introduces the tag). The untagged key splits to
    /// `(itself, None)`.
    pub fn split_tag(&self) -> (SurfaceKey, Option<EnvironmentTag>) {
        match self.0.rfind(SURFACE_TAG_SEPARATOR) {
            Some(at) => {
                let base = SurfaceKey::new(&self.0[..at]);
                // The constructor cannot fail here: the tag came out of a
                // key this module's own rules built (or a caller's raw
                // string, which re-validates cleanly by construction of
                // the split — any character that would be refused would
                // have been refused when the key was tagged).
                let tag = EnvironmentTag(self.0[at + 1..].to_owned());
                (base, Some(tag))
            }
            None => (self.clone(), None),
        }
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
                candidate.kind().as_str()
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
///
/// R0.11 (wave 1) grows the envelope additively for the registry kinds:
/// the four new fields default to [`EnvelopeRule::Approval`] and are
/// absent from the wire at that default, so R0.8-era envelopes keep
/// deserializing and their serialization stays byte-stable — the
/// established contract-evolution rule. Approval is the honest default
/// for the new kinds: a schema tightening or an ordering change is a
/// contract judgment, not a metric, and a fabricated auto bar is worse
/// than an honest approval (the design's governance wiring).
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

    /// The rule for `tool_contract` candidates (R0.11; defaults to
    /// approval, absent from the wire at the default).
    #[serde(
        default = "approval_envelope_rule",
        skip_serializing_if = "is_approval_envelope_rule"
    )]
    pub tool_contract: EnvelopeRule,

    /// The rule for `model_settings` candidates (R0.11; same evolution
    /// rule as `tool_contract`).
    #[serde(
        default = "approval_envelope_rule",
        skip_serializing_if = "is_approval_envelope_rule"
    )]
    pub model_settings: EnvelopeRule,

    /// The rule for `memory_configuration` candidates (R0.11; same
    /// evolution rule as `tool_contract`).
    #[serde(
        default = "approval_envelope_rule",
        skip_serializing_if = "is_approval_envelope_rule"
    )]
    pub memory_configuration: EnvelopeRule,

    /// The rule for `middleware_composition` candidates (R0.11; same
    /// evolution rule as `tool_contract`).
    #[serde(
        default = "approval_envelope_rule",
        skip_serializing_if = "is_approval_envelope_rule"
    )]
    pub middleware_composition: EnvelopeRule,
}

/// The R0.11 additive fields' default: approval. A contract judgment
/// (schema, settings, composition) earns a human's name by default; a
/// deployment that wants evidence-gated automation declares it.
fn approval_envelope_rule() -> EnvelopeRule {
    EnvelopeRule::Approval
}

/// The sparse-wire predicate for the R0.11 additive fields: approval is
/// the default, so an approval rule serializes as absence.
fn is_approval_envelope_rule(rule: &EnvelopeRule) -> bool {
    *rule == EnvelopeRule::Approval
}

/// The wave-1 envelope answer for `context_policy` candidates (see
/// [`PromotionEnvelope::rule_for`]): approval, always, declared as a
/// constant rather than an envelope field so the shipped envelope wire
/// shape is untouched.
const CONTEXT_POLICY_ENVELOPE_RULE: EnvelopeRule = EnvelopeRule::Approval;

impl PromotionEnvelope {
    /// The R0.8 default (design open question 6): `memory_set` candidates
    /// at run and agent scope with a clean verdict may auto-promote;
    /// `prompt`, `policy`, and `tool_permission` always require an
    /// approval token this release. The R0.11 stance extends it: the
    /// registry kinds (tool contracts, model settings, memory
    /// configurations, middleware compositions) default to approval —
    /// a schema or ordering change is a contract judgment, and a
    /// fabricated metric is worse than an honest approval.
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
            tool_contract: EnvelopeRule::Approval,
            model_settings: EnvelopeRule::Approval,
            memory_configuration: EnvelopeRule::Approval,
            middleware_composition: EnvelopeRule::Approval,
        }
    }

    /// The rule for `kind`.
    ///
    /// R0.13's `context_policy` has no envelope field yet: the wave-1 rule
    /// is the conservative constant — a context policy steers every run's
    /// assembly, the semantic blast radius the registry kinds already price
    /// — so it answers [`EnvelopeRule::Approval`] without growing the
    /// envelope's wire shape. A declarable rule lands with its wave's
    /// golden, additively, if a deployment needs one.
    pub fn rule_for(&self, kind: CandidateKind) -> &EnvelopeRule {
        match kind {
            CandidateKind::Prompt => &self.prompt,
            CandidateKind::Policy => &self.policy,
            CandidateKind::MemorySet => &self.memory_set,
            CandidateKind::ToolPermission => &self.tool_permission,
            CandidateKind::ToolContract => &self.tool_contract,
            CandidateKind::ModelSettings => &self.model_settings,
            CandidateKind::MemoryConfiguration => &self.memory_configuration,
            CandidateKind::MiddlewareComposition => &self.middleware_composition,
            CandidateKind::ContextPolicy => &CONTEXT_POLICY_ENVELOPE_RULE,
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
            &self.tool_contract,
            &self.model_settings,
            &self.memory_configuration,
            &self.middleware_composition,
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

// --------------------------------------------------------------------- //
// R0.10 wave 3: the learned retry/timeout families
//
// Three pieces, all consuming the same journaled evidence:
//
// - **Distillation** ([`distill_retry_parameters`],
//   [`distill_timeout_parameters`]) — closed-form learners that read
//   `DecisionEvent`s and effect events and emit family parameter sets.
//   The fitting is a grid search over declared envelopes, per design open
//   question 1: the runtime owns small closed-form scorers serialized as
//   the policy's parameters; the decision path stays a lookup.
// - **The twin-backed evaluator** ([`TwinCandidateEvaluator`]) — the
//   `CandidateEvaluator` seam's policy-candidate implementation: replay
//   the evidence span's fixtures with the candidate shadowing nothing and
//   acting head-to-head against the floor on identical seeds and fault
//   schedules, then verdict. A candidate that loses to the floor produces
//   a regressed verdict, and the gate refuses it — promotion through the
//   pipeline, never around it.
// - **Drift detection** ([`detect_policy_drift`]) — the promoted
//   version's journaled outcomes against the baseline its promotion
//   evaluation recorded; the revert itself is one activation of
//   `static-v0` away (server side).
// --------------------------------------------------------------------- //

use crate::durable::ErrorClass;
use crate::record::{
    derive_policy_version, BackoffParameters, DecisionAction, DecisionEvent, DecisionOutcome,
    Effect, EventStatus, ExecutorPolicy, PolicyVersion, RetryPolicyParameters, RunEvent,
    TimeoutPolicyParameters,
};
use crate::twin::{
    percentile, FaultSchedule, ParameterizedPolicy, Twin, TwinMetrics, TwinPolicy, TwinRunConfig,
    DEFAULT_TIMEOUT_LADDER,
};

/// The charged cost of a dead-lettered item in the retry learner's model,
/// in milliseconds: the lease boundary — what a task that never
/// recovers costs the run before the queue's worst-case discovery. The
/// same accounting the Wave 1 experiment and the twin apply.
const DEAD_LETTER_PENALTY_MS: u64 = crate::durable::MAX_RETRY_DELAY_MS;

/// How the retry distiller fits per-class schedules (R0.10 wave 3).
///
/// The grid is the learner's whole search space — closed-form scores over
/// declared values, so the fitted policy is inspectable as data and the
/// fitting itself is deterministic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetryLearningConfig {
    /// Minimum evidence (retries observed plus terminal outcomes) before a
    /// class earns its own entry. Below it the class keeps the floor —
    /// sparse evidence must not produce confident policy.
    pub min_samples: usize,

    /// The estimated wall-time margin over the floor a candidate schedule
    /// must clear to be emitted, in milliseconds. The Wave 1 experiment's
    /// published scar is that a cheap per-class table *underperformed* the
    /// floor's jittered exponential on latency; the margin is what keeps
    /// a marginal fit from demoting the floor.
    pub min_improvement_ms: u64,

    /// Candidate backoff bases, in milliseconds.
    pub base_grid_ms: Vec<u64>,

    /// Candidate backoff caps, in milliseconds.
    pub cap_grid_ms: Vec<u64>,

    /// Candidate attempt budgets.
    pub max_attempts_grid: Vec<u32>,
}

impl Default for RetryLearningConfig {
    fn default() -> Self {
        Self {
            min_samples: 4,
            min_improvement_ms: 2_000,
            base_grid_ms: vec![100, 250, 500, 1_000, 2_000],
            cap_grid_ms: vec![30_000, 120_000, 300_000],
            max_attempts_grid: vec![1, 2, 3, 5],
        }
    }
}

/// One class's distilled retry evidence.
#[derive(Debug, Default)]
struct ClassEvidence {
    /// Decisions that selected `Retry`.
    retries: u64,
    /// Decisions journaled with a success outcome.
    successes: u64,
    /// Decisions journaled with a failure outcome (terminal: dead-letter
    /// or outright fail).
    failures: u64,
    /// Callee-supplied `Retry-After` floors observed at decision time.
    retry_after_ms: Vec<u64>,
}

/// The estimated expected cost of one schedule on one class's evidence, in
/// milliseconds: expected backoff wall before recovery, plus the
/// dead-letter penalty weighted by the probability the budget runs out.
/// `p` is the estimated per-retry success probability; `retry_after` is
/// the world's own floor on any delay (a delay shorter than the callee's
/// `Retry-After` is not a shorter delay — it is the callee's delay).
fn retry_schedule_score(
    base_ms: u64,
    cap_ms: u64,
    max_attempts: u32,
    p: f64,
    retry_after: u64,
) -> f64 {
    let mut wall = 0.0;
    let mut survival = 1.0;
    for k in 0..max_attempts {
        let mean_delay =
            ((base_ms.saturating_mul(1u64 << k.min(20))).min(cap_ms) / 2).max(retry_after) as f64;
        wall += survival * mean_delay;
        survival *= 1.0 - p;
    }
    wall + survival * DEAD_LETTER_PENALTY_MS as f64
}

/// Distill the retry family's parameters from journaled retry
/// [`DecisionEvent`]s (R0.10 wave 3).
///
/// What is learned, per error class with sufficient evidence: the backoff
/// base and cap and the attempt budget that minimize expected wall-to-
/// recovery charged with dead-letter risk — **never the schedule's
/// shape** (full jitter over a doubling window is the floor's; Wave 1's
/// scar is that fixed per-class delays lose to it), and never the gates.
/// A class whose failures never turned into completions earns the
/// permanent-failure stance (`max_attempts: 1` — abort after the first
/// failure, the design's "earlier abort under permanent ones"); a class
/// whose best grid score does not beat the floor's by
/// [`RetryLearningConfig::min_improvement_ms`] earns nothing — the floor
/// decides for it. The returned set carries the floor's flat schedule;
/// `per_class` is `None` when no class cleared the margin, in which case
/// the honest next step is no candidate at all.
pub fn distill_retry_parameters(
    events: &[DecisionEvent],
    config: &RetryLearningConfig,
) -> RetryPolicyParameters {
    let floor = ExecutorPolicy::static_v0().retry;
    let mut classes: std::collections::BTreeMap<ErrorClass, ClassEvidence> =
        std::collections::BTreeMap::new();
    for event in events {
        if event.family != crate::record::DecisionFamily::Retry {
            continue;
        }
        let class = event
            .features
            .get("failure_class")
            .and_then(|value| serde_json::from_value(value.clone()).ok())
            .unwrap_or(ErrorClass::Unknown);
        let evidence = classes.entry(class).or_default();
        if matches!(event.selected, DecisionAction::Retry { .. }) {
            evidence.retries += 1;
        }
        match event.outcome {
            Some(DecisionOutcome::Success) => evidence.successes += 1,
            Some(DecisionOutcome::Failure) => evidence.failures += 1,
            _ => {}
        }
        if let Some(retry_after) = event
            .features
            .get("retry_after_ms")
            .and_then(serde_json::Value::as_u64)
        {
            evidence.retry_after_ms.push(retry_after);
        }
    }

    let mut table = std::collections::BTreeMap::new();
    for (class, evidence) in &classes {
        let total = evidence.retries + evidence.failures;
        if (total as usize) < config.min_samples {
            continue;
        }
        // A class with retries observed but no recorded success estimates
        // at the boundary — the permanent-failure stance — only when the
        // failures are themselves evidence (terminal outcomes), never on
        // absence alone.
        if evidence.successes == 0 && evidence.retries > 0 {
            if evidence.failures >= config.min_samples as u64 {
                table.insert(
                    *class,
                    BackoffParameters {
                        base_delay_ms: floor.base_delay_ms,
                        max_delay_ms: floor.max_delay_ms,
                        max_attempts: 1,
                    },
                );
            }
            continue;
        }
        if evidence.retries == 0 || evidence.successes == 0 {
            continue;
        }
        let p = evidence.successes as f64 / evidence.retries as f64;
        let retry_after = percentile(&evidence.retry_after_ms, 50.0);
        let floor_score = retry_schedule_score(
            floor.base_delay_ms,
            floor.max_delay_ms,
            floor.max_attempts,
            p,
            retry_after,
        );
        let mut best: Option<(f64, BackoffParameters)> = None;
        for &base in &config.base_grid_ms {
            for &cap in &config.cap_grid_ms {
                if base == 0 || base > cap {
                    continue;
                }
                for &max_attempts in &config.max_attempts_grid {
                    let score = retry_schedule_score(base, cap, max_attempts, p, retry_after);
                    let candidate = BackoffParameters {
                        base_delay_ms: base,
                        max_delay_ms: cap,
                        max_attempts,
                    };
                    if best
                        .as_ref()
                        .is_none_or(|(best_score, _)| score < *best_score)
                    {
                        best = Some((score, candidate));
                    }
                }
            }
        }
        if let Some((score, schedule)) = best {
            if floor_score - score >= config.min_improvement_ms as f64 {
                table.insert(*class, schedule);
            }
        }
    }

    RetryPolicyParameters {
        per_class: if table.is_empty() { None } else { Some(table) },
        ..floor
    }
}

/// How the timeout distiller fits per-callee bounds (R0.10 wave 3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimeoutLearningConfig {
    /// Minimum completions a callee must have journaled before a bound is
    /// fit to it. Below it the callee keeps no bound — sparse evidence
    /// must not produce confident policy.
    pub min_samples: usize,

    /// The largest premature-abort fraction a bound may carry: the share
    /// of journaled completions that ran longer than the bound. The Wave
    /// 1 scar — a p99-plus-margin bound prematurely aborting heavy-tailed
    /// work — is a percentile *estimate* multiplied by a constant missing
    /// the tail mass the estimate cannot see; reading the abort fraction
    /// off the empirical distribution directly is the fix, and when no
    /// rung satisfies the tolerance the learner abstains (the floor keeps
    /// the callee) rather than shipping a bound that fails
    /// non-inferiority.
    pub abort_tolerance: f64,

    /// The ladder the fitted bounds are chosen from, ascending. The top
    /// rung models "no bound in force" and is never emitted — a bound
    /// equal to it reclaims nothing the floor does not.
    pub ladder: Vec<u64>,
}

impl Default for TimeoutLearningConfig {
    fn default() -> Self {
        Self {
            min_samples: 16,
            abort_tolerance: 0.01,
            ladder: DEFAULT_TIMEOUT_LADDER.to_vec(),
        }
    }
}

/// Distill the timeout family's parameters from journaled effect events
/// (R0.10 wave 3). The callee identity is the event's `node_id`; only
/// completed calls (status `Ok` with a measured latency) are evidence —
/// a failure's latency says nothing about how long success takes.
///
/// Per callee: the smallest ladder rung whose empirical premature-abort
/// fraction (the share of completions that ran longer than the rung) is
/// within [`TimeoutLearningConfig::abort_tolerance`]. Callees with
/// insufficient evidence, with no qualifying rung (a tail heavier than
/// the ladder — the Wave 1 failure mode, abstained from rather than
/// shipped), or whose qualifying rung is the ladder's top get no entry.
/// `default_millis` stays `None` — the floor's stance for every callee
/// the evidence did not speak for; `max_millis` pins the ladder's top as
/// the declared ceiling when any entry exists.
pub fn distill_timeout_parameters(
    events: &[RunEvent],
    config: &TimeoutLearningConfig,
) -> TimeoutPolicyParameters {
    let mut latencies: std::collections::BTreeMap<String, Vec<u64>> =
        std::collections::BTreeMap::new();
    for event in events {
        if event.status != EventStatus::Ok {
            continue;
        }
        let (Some(callee), Some(latency)) = (event.node_id.clone(), event.latency_ms) else {
            continue;
        };
        latencies.entry(callee).or_default().push(latency);
    }

    let mut table = std::collections::BTreeMap::new();
    let top = config.ladder.last().copied().unwrap_or(u64::MAX);
    for (callee, samples) in &latencies {
        if samples.len() < config.min_samples {
            continue;
        }
        let bound = config.ladder.iter().copied().find(|rung| {
            let aborted = samples.iter().filter(|latency| *latency > rung).count();
            (aborted as f64 / samples.len() as f64) <= config.abort_tolerance
        });
        match bound {
            Some(rung) if rung < top => {
                table.insert(callee.clone(), rung);
            }
            _ => {}
        }
    }

    TimeoutPolicyParameters {
        default_millis: None,
        max_millis: if table.is_empty() { None } else { Some(top) },
        per_callee: if table.is_empty() { None } else { Some(table) },
    }
}

/// The twin-backed [`CandidateEvaluator`] for `policy` candidates (R0.10
/// wave 3): the governance wiring's "evaluation happens in the twin",
/// landed. Every fixture in the request's evidence span is re-executed
/// twice on the same seed and fault schedule — the floor acting, then the
/// candidate's parameters acting through [`ParameterizedPolicy`] — and the
/// verdict is computed over the paired [`TwinMetrics`]: **non-inferior
/// completion**
/// (the release proof's constraint, applied at the gate) and improvement
/// on the request's target metric.
///
/// Verdict conventions, stated precisely: `regressed` is set when the
/// candidate's completion rate falls below the floor's by more than the
/// declared completion tolerance (see
/// [`TwinCandidateEvaluator::with_completion_tolerance`]) — on any single
/// fixture or in aggregate; `delta` is signed so that positive is better
/// on the named metric (for cost and latency metrics, lower is better,
/// so `delta = baseline − candidate`), keeping [`EvaluationVerdict`]'s
/// "negative is worse" invariant true for the gate. Both reports carry
/// every fixture's [`TwinReport`](crate::twin::TwinReport), so the
/// twin's validity bound travels with the evaluation (design open
/// question 5).
///
/// Determinism is the twin's own: same fixtures, same seed, same fault
/// schedule ⇒ the same metrics and the same verdict.
#[derive(Debug)]
pub struct TwinCandidateEvaluator {
    seed: u64,
    faults: FaultSchedule,
    workers: Vec<String>,
    max_attempts: u32,
    completion_tolerance: f64,
    evaluated_by: ProvenanceAuthor,
}

impl TwinCandidateEvaluator {
    /// An evaluator drawing from `seed`, attributing every evaluation to
    /// `evaluated_by`, over an unfaulted recorded world, one worker, the
    /// floor's attempt budget, and strict completion parity (tolerance 0).
    pub fn new(seed: u64, evaluated_by: ProvenanceAuthor) -> Self {
        Self {
            seed,
            faults: FaultSchedule::new(seed),
            workers: vec!["worker-0".to_owned()],
            max_attempts: 3,
            completion_tolerance: 0.0,
            evaluated_by,
        }
    }

    /// Builder-style: evaluate against this fault schedule — the faults
    /// rarer than any recorded window contains, which is the twin's
    /// reason to exist.
    pub fn with_faults(mut self, faults: FaultSchedule) -> Self {
        self.faults = faults;
        self
    }

    /// Builder-style: declare the worker pool placement ranks.
    pub fn with_workers(mut self, workers: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.workers = workers.into_iter().map(Into::into).collect();
        self
    }

    /// Builder-style: the attempt budget the world allows (a learned
    /// policy may narrow it, never widen it).
    pub fn with_max_attempts(mut self, max_attempts: u32) -> Self {
        self.max_attempts = max_attempts;
        self
    }

    /// Builder-style: the completion-rate band within which the candidate
    /// is non-inferior. Zero is the honest default for a mechanical
    /// family: identical completion is achievable, so parity is the bar.
    pub fn with_completion_tolerance(mut self, tolerance: f64) -> Self {
        self.completion_tolerance = tolerance;
        self
    }
}

/// Per-arm aggregates over the fixture set, computed once and shared by
/// the verdict and the reports.
#[derive(Debug, Clone, Copy, Default)]
struct ArmAggregate {
    items: usize,
    completed: usize,
    dead_lettered: usize,
    attempts: u64,
    cost_usd: f64,
    wall_time_ms: u64,
    latency_p95_ms: u64,
    runs: usize,
}

impl ArmAggregate {
    fn fold(&mut self, metrics: &TwinMetrics) {
        self.items += metrics.items;
        self.completed += metrics.completed;
        self.dead_lettered += metrics.dead_lettered;
        self.attempts += metrics.attempts;
        self.cost_usd += metrics.cost_usd;
        self.wall_time_ms += metrics.wall_time_ms;
        self.latency_p95_ms = self.latency_p95_ms.max(metrics.item_latency_p95_ms);
        self.runs += 1;
    }

    fn completion_rate(&self) -> f64 {
        if self.items == 0 {
            0.0
        } else {
            self.completed as f64 / self.items as f64
        }
    }

    fn dead_letter_rate(&self) -> f64 {
        if self.items == 0 {
            0.0
        } else {
            self.dead_lettered as f64 / self.items as f64
        }
    }

    fn mean_wall_ms(&self) -> f64 {
        if self.runs == 0 {
            0.0
        } else {
            self.wall_time_ms as f64 / self.runs as f64
        }
    }

    /// The serialized aggregate both reports carry — also the shape
    /// [`DriftBaseline::from_twin_report`] reads back.
    fn as_value(&self) -> Value {
        serde_json::json!({
            "items": self.items,
            "completed": self.completed,
            "dead_lettered": self.dead_lettered,
            "attempts": self.attempts,
            "cost_usd": self.cost_usd,
            "mean_wall_time_ms": self.mean_wall_ms(),
            "item_latency_p95_ms": self.latency_p95_ms,
            "completion_rate": self.completion_rate(),
            "dead_letter_rate": self.dead_letter_rate(),
        })
    }
}

#[async_trait]
impl CandidateEvaluator for TwinCandidateEvaluator {
    async fn evaluate(
        &self,
        candidate: &Candidate,
        request: &EvaluationRequest,
    ) -> Result<CandidateEvaluation> {
        let CandidateContent::Policy { family, parameters } = &candidate.content else {
            return Err(invalid(format!(
                "the twin-backed evaluator prices `policy` candidates; `{}` candidates \
                 evaluate through the application's own evaluator",
                candidate.kind().as_str()
            )));
        };
        // The candidate as a concrete policy: parsed against the family's
        // contract and checked against its envelope — an out-of-envelope
        // candidate fails here, before a single twin run.
        let candidate_policy = ExecutorPolicy::static_v0()
            .with_family_parameters(*family, parameters.clone())
            .map_err(|e| {
                invalid(format!(
                    "policy candidate cannot be evaluated: its parameters do not apply to \
                     the floor ({e}) — an out-of-envelope candidate never reaches evidence"
                ))
            })?;
        let version = derive_policy_version(&candidate_policy)?;
        let acting: Arc<dyn TwinPolicy> =
            Arc::new(ParameterizedPolicy::new(candidate_policy, version.clone())?);

        let mut floor_aggregate = ArmAggregate::default();
        let mut candidate_aggregate = ArmAggregate::default();
        let mut fixture_ids = Vec::with_capacity(request.replay_evidence.len());
        let mut matched = 0usize;
        let mut divergences = Vec::new();
        let mut floor_runs = Vec::new();
        let mut candidate_runs = Vec::new();
        let mut completion_breached = false;

        for fixture in &request.replay_evidence {
            fixture_ids.push(fixture.run_id.clone());
            let outcome = (|| -> Result<_> {
                let twin = Twin::from_snapshot(fixture.clone())?;
                let config = TwinRunConfig::new(self.seed)
                    .with_faults(self.faults.clone())
                    .with_workers(self.workers.clone())
                    .with_max_attempts(self.max_attempts);
                let floor_run = twin.run(&config)?;
                let candidate_run = twin.run(&config.with_acting(acting.clone()))?;
                Ok((floor_run, candidate_run))
            })();
            match outcome {
                Ok((floor_run, candidate_run)) => {
                    matched += 1;
                    // Per-fixture non-inferiority: a candidate that
                    // completes less of one fixture's work than the floor
                    // is a regression the aggregate must not average away.
                    let floor_rate =
                        floor_run.metrics.completed as f64 / floor_run.metrics.items.max(1) as f64;
                    let candidate_rate = candidate_run.metrics.completed as f64
                        / candidate_run.metrics.items.max(1) as f64;
                    if candidate_rate < floor_rate - self.completion_tolerance {
                        completion_breached = true;
                    }
                    floor_aggregate.fold(&floor_run.metrics);
                    candidate_aggregate.fold(&candidate_run.metrics);
                    floor_runs.push(serde_json::json!({
                        "fixture": fixture.run_id,
                        "metrics": floor_run.metrics,
                        "report": floor_run.report,
                    }));
                    candidate_runs.push(serde_json::json!({
                        "fixture": fixture.run_id,
                        "metrics": candidate_run.metrics,
                        "report": candidate_run.report,
                    }));
                }
                Err(error) => {
                    // A fixture the twin cannot re-execute is evidence the
                    // evaluation could not verify — a divergence, fail
                    // closed, the same rule the wave-4 evaluator applies.
                    divergences.push(ReplayDivergence {
                        fixture_id: fixture.run_id.clone(),
                        detail: error.to_string(),
                    });
                }
            }
        }
        fixture_ids.sort();
        divergences.sort_by(|a, b| a.fixture_id.cmp(&b.fixture_id));

        let regressed = completion_breached
            || candidate_aggregate.completion_rate()
                < floor_aggregate.completion_rate() - self.completion_tolerance;
        let (baseline, candidate_value, delta) = match request.target_metric.as_str() {
            "completion_rate" => (
                Some(floor_aggregate.completion_rate()),
                Some(candidate_aggregate.completion_rate()),
                Some(candidate_aggregate.completion_rate() - floor_aggregate.completion_rate()),
            ),
            "cost_usd" => (
                Some(floor_aggregate.cost_usd),
                Some(candidate_aggregate.cost_usd),
                Some(floor_aggregate.cost_usd - candidate_aggregate.cost_usd),
            ),
            "wall_time_ms" => (
                Some(floor_aggregate.mean_wall_ms()),
                Some(candidate_aggregate.mean_wall_ms()),
                Some(floor_aggregate.mean_wall_ms() - candidate_aggregate.mean_wall_ms()),
            ),
            "item_latency_p95_ms" => (
                Some(floor_aggregate.latency_p95_ms as f64),
                Some(candidate_aggregate.latency_p95_ms as f64),
                Some(
                    floor_aggregate.latency_p95_ms as f64
                        - candidate_aggregate.latency_p95_ms as f64,
                ),
            ),
            // A metric the twin does not price: no values, no delta — an
            // improvement bar cannot clear on a metric the evidence does
            // not name (the gate's own rule).
            _ => (None, None, None),
        };

        Ok(CandidateEvaluation {
            candidate_id: candidate.candidate_id.clone(),
            dataset_version: request.dataset_version.clone(),
            replay: ReplaySummary {
                fixture_ids,
                matched,
                divergences,
            },
            baseline_report: serde_json::json!({
                "arm": PolicyVersion::STATIC_V0,
                "aggregate": floor_aggregate.as_value(),
                "runs": floor_runs,
            }),
            candidate_report: serde_json::json!({
                "arm": version,
                "aggregate": candidate_aggregate.as_value(),
                "runs": candidate_runs,
            }),
            verdict: EvaluationVerdict {
                regressed,
                target_metric: request.target_metric.clone(),
                baseline,
                candidate: candidate_value,
                delta,
            },
            thresholds: request.thresholds,
            evaluated_by: self.evaluated_by.clone(),
            evaluated_at: Utc::now(),
        })
    }
}

/// The promotion-time baseline drift is measured against: the metrics the
/// promoted version's own evaluation recorded for the arm it displaced
/// (the floor's recorded baseline — "is the promoted version regressing
/// against the evidence that promoted it", nothing deeper).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DriftBaseline {
    /// Baseline completion rate, in `[0, 1]`.
    pub completion_rate: f64,

    /// Baseline dead-letter rate, in `[0, 1]`.
    pub dead_letter_rate: f64,

    /// Baseline p95 decision latency (the journaled
    /// `dependency_latency_ms` feature), when the evidence carried one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_p95_ms: Option<u64>,
}

impl DriftBaseline {
    /// Read the baseline out of a serialized twin evaluation report — the
    /// `aggregate` section [`TwinCandidateEvaluator`] writes. `None` when
    /// the report names no such section (a non-twin evaluation, a
    /// hand-authored report): drift detection requires the promotion-time
    /// evidence, and it fails absent rather than guessing a baseline.
    pub fn from_twin_report(report: &Value) -> Option<Self> {
        let aggregate = report.get("aggregate")?;
        Some(Self {
            completion_rate: aggregate.get("completion_rate")?.as_f64()?,
            dead_letter_rate: aggregate.get("dead_letter_rate")?.as_f64()?,
            latency_p95_ms: aggregate.get("item_latency_p95_ms").and_then(Value::as_u64),
        })
    }
}

/// The declared thresholds a drift check applies. All comparisons are
/// against the promotion-time baseline, on the acting version's journaled
/// outcomes only — shadow decisions are evidence for the *next* candidate,
/// never for the acting version's health.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DriftThresholds {
    /// Completion-rate drop from baseline that declares drift.
    pub max_completion_drop: f64,

    /// Dead-letter-rate growth from baseline that declares drift.
    pub max_dead_letter_growth: f64,

    /// p95 latency ratio `current / baseline` that declares drift (applied
    /// only when both sides carry the latency feature).
    pub max_latency_p95_ratio: f64,

    /// Minimum terminal decisions before any verdict. Sparse evidence
    /// declares nothing — a drift verdict on three outcomes is noise
    /// wearing a label.
    pub min_decisions: usize,
}

impl Default for DriftThresholds {
    fn default() -> Self {
        Self {
            max_completion_drop: 0.05,
            max_dead_letter_growth: 0.02,
            max_latency_p95_ratio: 1.25,
            min_decisions: 8,
        }
    }
}

/// The drift check's verdict: the acting version's journaled outcomes
/// against its promotion-time baseline, with the reasons drift was (or
/// was not) declared. Carried as data — the report is the revert's
/// attributable cause.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicyDriftReport {
    /// The version checked.
    pub version: PolicyVersion,

    /// Acting decisions examined (shadow decisions excluded).
    pub decisions: usize,

    /// Terminal decisions (outcomes recorded) the rates were computed
    /// over.
    pub terminal: usize,

    /// The acting version's journaled completion rate.
    pub completion_rate: f64,

    /// The acting version's journaled dead-letter rate.
    pub dead_letter_rate: f64,

    /// The acting version's p95 decision latency, when carried.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_p95_ms: Option<u64>,

    /// The baseline the check ran against.
    pub baseline: DriftBaseline,

    /// Whether drift was declared.
    pub drifted: bool,

    /// Why — one entry per breached threshold, or the reason nothing was
    /// declared (including insufficient evidence).
    pub reasons: Vec<String>,
}

/// Detect drift on the acting policy's journaled outcomes (R0.10 wave 3).
///
/// Reads `decisions` for those made by `version` in an acting role
/// (`role` unset — every pre-twin decision — or
/// [`crate::record::DecisionRole::Acting`]); shadow decisions are
/// off-policy evidence, not the acting version's health. The signals are
/// the design's own: completion-rate drop, dead-letter growth, p95
/// latency ratio — declared thresholds on journaled metrics, honest about
/// answering "is the promoted version regressing against the evidence
/// that promoted it", nothing deeper. Pure: the revert that a drifted
/// verdict motivates is the caller's one activation of `static-v0`.
pub fn detect_policy_drift(
    decisions: &[DecisionEvent],
    version: &PolicyVersion,
    baseline: &DriftBaseline,
    thresholds: &DriftThresholds,
) -> PolicyDriftReport {
    let acting: Vec<&DecisionEvent> = decisions
        .iter()
        .filter(|event| {
            &event.policy_version == version
                && event.role != Some(crate::record::DecisionRole::Shadow)
        })
        .collect();
    let terminal: Vec<&DecisionEvent> = acting
        .iter()
        .filter(|event| {
            matches!(
                event.outcome,
                Some(DecisionOutcome::Success) | Some(DecisionOutcome::Failure)
            )
        })
        .copied()
        .collect();
    let successes = terminal
        .iter()
        .filter(|event| event.outcome == Some(DecisionOutcome::Success))
        .count();
    // Dead-lettered, as the retry family journals it: the Abort decision
    // with the attempt budget spent while the gates were open. The legal
    // set has already collapsed to `[Abort]` at that point, so the budget
    // and the gates are read back from the journaled features — the same
    // vocabulary `retry_decision_event` and the twin both pin.
    let dead_letters = terminal
        .iter()
        .filter(|event| {
            if event.outcome != Some(DecisionOutcome::Failure)
                || event.selected != DecisionAction::Abort
            {
                return false;
            }
            let budget_spent = match (
                event.features.get("attempt").and_then(Value::as_u64),
                event.features.get("max_attempts").and_then(Value::as_u64),
            ) {
                (Some(attempt), Some(max_attempts)) => attempt >= max_attempts,
                _ => false,
            };
            let effect_open = event
                .features
                .get("effect")
                .and_then(|value| serde_json::from_value::<Effect>(value.clone()).ok())
                .is_some_and(|effect| effect.is_freely_repeatable());
            let class_open = event
                .features
                .get("failure_class")
                .and_then(|value| serde_json::from_value::<ErrorClass>(value.clone()).ok())
                .is_some_and(|class| class.is_retryable());
            budget_spent && effect_open && class_open
        })
        .count();
    let latencies: Vec<u64> = acting
        .iter()
        .filter_map(|event| {
            event
                .features
                .get("dependency_latency_ms")
                .and_then(Value::as_u64)
        })
        .collect();
    let completion_rate = if terminal.is_empty() {
        0.0
    } else {
        successes as f64 / terminal.len() as f64
    };
    let dead_letter_rate = if terminal.is_empty() {
        0.0
    } else {
        dead_letters as f64 / terminal.len() as f64
    };
    let latency_p95_ms = (!latencies.is_empty()).then(|| percentile(&latencies, 95.0));

    let mut reasons = Vec::new();
    let sufficient = terminal.len() >= thresholds.min_decisions;
    let mut drifted = false;
    if !sufficient {
        reasons.push(format!(
            "insufficient evidence: {} terminal decisions, fewer than the declared minimum {}",
            terminal.len(),
            thresholds.min_decisions
        ));
    } else {
        let completion_drop = baseline.completion_rate - completion_rate;
        if completion_drop > thresholds.max_completion_drop {
            drifted = true;
            reasons.push(format!(
                "completion rate {completion_rate:.3} is {completion_drop:.3} below the \
                 promotion-time baseline {:.3} (threshold {:.3})",
                baseline.completion_rate, thresholds.max_completion_drop
            ));
        }
        let dead_letter_growth = dead_letter_rate - baseline.dead_letter_rate;
        if dead_letter_growth > thresholds.max_dead_letter_growth {
            drifted = true;
            reasons.push(format!(
                "dead-letter rate {dead_letter_rate:.3} grew {dead_letter_growth:.3} over the \
                 promotion-time baseline {:.3} (threshold {:.3})",
                baseline.dead_letter_rate, thresholds.max_dead_letter_growth
            ));
        }
        if let (Some(current), Some(base)) = (latency_p95_ms, baseline.latency_p95_ms) {
            if base > 0 && current as f64 / base as f64 > thresholds.max_latency_p95_ratio {
                drifted = true;
                reasons.push(format!(
                    "p95 decision latency {current} ms is {:.2}x the promotion-time baseline \
                     {base} ms (threshold {:.2}x)",
                    current as f64 / base as f64,
                    thresholds.max_latency_p95_ratio
                ));
            }
        }
    }

    PolicyDriftReport {
        version: version.clone(),
        decisions: acting.len(),
        terminal: terminal.len(),
        completion_rate,
        dead_letter_rate,
        latency_p95_ms,
        baseline: *baseline,
        drifted,
        reasons,
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
