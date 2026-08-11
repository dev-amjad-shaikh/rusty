//! Effect kernel v2 (R0.7): retry-safety moves from runtime convention into
//! the type system.
//!
//! R0.5 froze the [`Effect`] taxonomy as a wire contract and R0.6 made it a
//! runtime convention — [`crate::durable::classify_retry`] refuses to silently
//! retry anything that is not [`Effect::is_freely_repeatable`]. That gate is a
//! function call away from being ignored: nothing in the type of a node,
//! tool, or durable task says what its failure modes are. This module adds
//! the compile-time half. An effect type declares its safety class by
//! implementing one of the marker traits below; generic infrastructure (a
//! retry loop, a speculator, the R0.7 race pattern) can then *require* a
//! class at the type level instead of re-checking a convention at runtime.
//!
//! The marker traits **compose with** the wire enum, they do not replace it:
//! [`Effect`] stays the serde/golden-pinned contract journals and envelopes
//! are written in; the typed kernel is the in-process API that enforcement
//! points are built from. The mapping is one-to-one, with one deliberate
//! naming reconciliation:
//!
//! | Marker trait            | Wire [`Effect`]   | What the class unlocks |
//! |-------------------------|-------------------|------------------------|
//! | [`PureEffect`]          | `Pure`            | caching, speculation, unrestricted retry |
//! | [`ReadOnlyEffect`]      | `ReadOnly`        | unrestricted retry; replay serves the journaled value |
//! | [`IdempotentEffect`]    | `Idempotent`      | automatic retry under a stable idempotency key |
//! | [`CompensatableEffect`] | `Compensatable`   | execution only with a registered rollback handler |
//! | [`IrreversibleEffect`]  | `NonIdempotent`   | execution only behind an explicit [`ApprovalToken`] |
//!
//! The 2026-08-08 review names the last rung *Irreversible*; the wire enum
//! calls it `NonIdempotent`. We keep the wire name for two reasons: the
//! enum's serde shape is golden-pinned evidence (an additive rename would
//! churn every journal for zero semantic gain), and `NonIdempotent` states
//! what the runtime can actually *verify* — the absence of a declared
//! idempotency story — whereas irreversibility is a claim about the world it
//! cannot check. The typed API adopts the review's vocabulary
//! ([`IrreversibleEffect`]) because that is the word that makes the approval
//! boundary legible at a call site; the two names denote one class.
//!
//! Two further mechanisms close the loop with the R0.6 receipt ledger:
//!
//! - **Deterministic effect ids** ([`derive_effect_id`], [`EffectId`]) — a
//!   content-addressed identity derived from the run scope, the effect kind,
//!   the input hash, and the idempotency key. On recovery the runtime asks
//!   "did this exact effect already commit?" and the journal answers through
//!   [`crate::journal::JournalSnapshot::find_effect_receipt_by_effect_id`].
//! - **The approval boundary** ([`ApprovalToken`], [`admit_irreversible`]) —
//!   an irreversible effect executes only when presented with a token scoped
//!   to its derived effect id. The token is a proof-of-explicit-decision
//!   *within the process*: it makes approval a value that must be
//!   constructed, not a boolean that can be silently defaulted. Cross-process
//!   attestation of approvals is R0.9's signed-receipt work.
//!
//! Runtime enforcement is opt-in: enabling effect admission on an executor
//! propagates a scoped [`EffectAdmissionContext`] through
//! [`crate::node::NodeContext`]. The prebuilt ReAct tools node carries it into
//! [`crate::tool::ToolExecutor`] automatically; custom nodes must do the same
//! when they construct a tool executor. Calls on that guarded path are
//! admitted after middleware has finalized their name and arguments but
//! before the tool body runs. Direct calls to [`crate::tool::Tool::call`] are
//! outside this cooperative boundary. Executors that do not enable it keep
//! the pre-R0.7 behavior, so existing graphs remain source-compatible.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::record::{sha256_hex, Effect};

/// Domain-separation and versioning prefix for [`derive_effect_id`].
///
/// Effect ids are content addresses; the prefix keeps them collision-free
/// against every other SHA-256 digest in the system (artifact references,
/// journal heads, topology hashes) and gives the derivation formula an
/// explicit version handle if it ever needs to change.
pub const EFFECT_ID_DOMAIN: &str = "rusty/effect-id/v1";

/// A deterministic, content-addressed identity for one logical effect
/// within a run scope.
///
/// Derived — never minted — by [`derive_effect_id`]: the same `(scope, kind,
/// input hash, idempotency key)` tuple always yields the same id, so a
/// recovered run re-derives the id of the effect it was about to perform and
/// asks the journal whether a receipt already exists for it. That lookup is
/// what turns "retry carefully" into "do not re-execute what already
/// committed" — exactly-once *business outcomes* where the effect protocol
/// supports it, never a pretend exactly-once execution.
///
/// Transparent newtype over the lowercase hex digest: serializes as a bare
/// string, consistent with [`crate::record::ArtifactRef`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EffectId(String);

impl EffectId {
    /// The hex digest.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for EffectId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Derive the deterministic id of an effect occurrence.
///
/// The formula is `SHA-256` over the newline-joined, domain-prefixed tuple:
///
/// ```text
/// EFFECT_ID_DOMAIN "\n" scope "\n" kind "\n" input_hash "\n" (idempotency_key | "-")
/// ```
///
/// - `scope` is the run (or sub-run) identity the effect belongs to — two
///   runs performing the same call get different ids, so a receipt from one
///   run can never masquerade as another's.
/// - `kind` is the effect type's stable identifier ([`TypedEffect::kind`]).
/// - `input_hash` is the lowercase hex SHA-256 of the canonical input (the
///   same canonical `serde_json` serialization [`crate::record::PayloadRef::content_hash`]
///   uses), precomputed by the caller so this function stays IO-free.
/// - `idempotency_key` distinguishes intentional replays under one key from
///   distinct effects that happen to share an input; `-` fills the slot when
///   the effect has no key, so keyed and unkeyed derivations can never
///   collide.
///
/// Newline-joining (rather than concatenation) keeps the tuple unambiguous
/// without a length prefix — the same framing [`crate::graph::Graph::topology_hash`]
/// uses for topology lines.
pub fn derive_effect_id(
    scope: &str,
    kind: &str,
    input_hash: &str,
    idempotency_key: Option<&str>,
) -> EffectId {
    let material = [
        EFFECT_ID_DOMAIN,
        scope,
        kind,
        input_hash,
        idempotency_key.unwrap_or("-"),
    ]
    .join("\n");
    EffectId(sha256_hex(material.as_bytes()))
}

/// The runtime description of one effect occurrence awaiting admission.
///
/// [`TypedEffect`] is the compile-time declaration surface; this value is its
/// object-safe dispatch counterpart. A [`crate::tool::Tool`] produces one for
/// each call after middleware has finalized the call. Its input hash is
/// computed with the same canonical JSON convention as journal payloads, so
/// approval tokens derived before execution match the id the dispatcher
/// re-derives at the boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectRequest {
    kind: String,
    effect: Effect,
    input_hash: String,
    idempotency_key: Option<String>,
}

impl EffectRequest {
    /// Describe an effect over canonical JSON `input`.
    pub fn new(
        kind: impl Into<String>,
        effect: Effect,
        input: &Value,
        idempotency_key: Option<String>,
    ) -> Self {
        let input_hash = crate::record::PayloadRef::inline(input.clone())
            .content_hash()
            .expect("a serde_json::Value always serializes");
        Self {
            kind: kind.into(),
            effect,
            input_hash,
            idempotency_key,
        }
    }

    /// Bridge a typed declaration into the runtime admission surface.
    pub fn from_typed<E: TypedEffect>(effect: &E) -> Self {
        Self {
            kind: effect.kind().to_owned(),
            effect: E::EFFECT,
            input_hash: effect.input_hash().to_owned(),
            idempotency_key: effect.idempotency_key().map(str::to_owned),
        }
    }

    /// Stable application-defined effect kind.
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// Wire-level safety class recorded for this occurrence.
    pub fn effect(&self) -> Effect {
        self.effect
    }

    /// Canonical input digest used in the deterministic effect id.
    pub fn input_hash(&self) -> &str {
        &self.input_hash
    }

    /// Stable idempotency key, when the declaration requires one.
    pub fn idempotency_key(&self) -> Option<&str> {
        self.idempotency_key.as_deref()
    }

    /// This occurrence's deterministic id in `scope`.
    pub fn effect_id(&self, scope: &str) -> EffectId {
        derive_effect_id(
            scope,
            self.kind(),
            self.input_hash(),
            self.idempotency_key(),
        )
    }
}

/// The typed half of the effect taxonomy: an effect type's contract with the
/// kernel.
///
/// Implement this trait plus exactly one marker trait ([`PureEffect`],
/// [`ReadOnlyEffect`], [`IdempotentEffect`], [`CompensatableEffect`],
/// [`IrreversibleEffect`]) to declare an effect's safety class. The marker is
/// the compile-time claim; [`TypedEffect::EFFECT`] is the wire-level class it
/// must agree with — every admission helper checks the two against each
/// other, so a type whose marker and declared class disagree is rejected at
/// the enforcement point, not silently misclassified.
pub trait TypedEffect {
    /// The wire-level [`Effect`] class this type records in journals and
    /// envelopes. Must agree with the marker trait the type implements (see
    /// the module-level mapping table); the admission helpers enforce the
    /// agreement.
    const EFFECT: Effect;

    /// The effect kind: a stable, application-chosen identifier
    /// (`"charge_card"`, `"index_document"`). Part of the effect id and the
    /// key under which rollback handlers are registered, so it must not
    /// change between deployments that share a journal.
    fn kind(&self) -> &str;

    /// Lowercase hex SHA-256 of the effect's canonical input. Two calls with
    /// equal inputs must hash equal — use the canonical `serde_json`
    /// serialization, as [`crate::record::PayloadRef::content_hash`] does.
    fn input_hash(&self) -> &str;

    /// The idempotency key the effect commits under, when it has one. The
    /// key participates in the effect id, so retried derivation under the
    /// same key converges on the id a recovery pass looks up. `None` is
    /// honest only for effects that are freely repeatable without a key.
    fn idempotency_key(&self) -> Option<&str> {
        None
    }

    /// This effect's deterministic id within `scope` (usually the run id) —
    /// [`derive_effect_id`] over the type's own contract fields.
    fn effect_id(&self, scope: &str) -> EffectId {
        derive_effect_id(
            scope,
            self.kind(),
            self.input_hash(),
            self.idempotency_key(),
        )
    }
}

/// Marker: a pure function of its inputs with no observable effect beyond
/// its return value (wire class [`Effect::Pure`]).
///
/// Pure work may be cached, memoized, and speculated freely: re-execution is
/// always safe and always equivalent, so a speculative result and an
/// on-demand result are indistinguishable. Declare it by implementing this
/// trait; [`admit_speculation`] is the enforcement point speculators call.
pub trait PureEffect: TypedEffect {}

/// Marker: reads external state but writes nothing (wire class
/// [`Effect::ReadOnly`]).
///
/// Re-execution is safe but not necessarily equivalent — the world may have
/// changed — so exact replay serves the journaled value while live replay
/// re-reads. Retries are unconstrained, which is why there is no admission
/// helper for this class: nothing needs gating.
pub trait ReadOnlyEffect: TypedEffect {}

/// Marker: repeating the call under the same idempotency key has the same
/// effect as calling once (wire class [`Effect::Idempotent`]).
///
/// This is the class that unlocks automatic retry — the typed form of the
/// gate [`crate::durable::classify_retry`] applies to the untyped enum.
/// [`admit_retry`] additionally requires the key itself: retrying an
/// "idempotent" effect with no key is the exact unsoundness the taxonomy
/// exists to prevent, and the helper refuses it.
pub trait IdempotentEffect: TypedEffect {}

/// Marker: repeating the call duplicates the effect, but a declared
/// compensating action can logically undo it — charge/refund (wire class
/// [`Effect::Compensatable`]).
///
/// The compensation is not optional coloring: [`admit_compensatable`]
/// requires a rollback handler registered for the effect's kind before the
/// effect may execute, and returns that handler so the caller holds the undo
/// path from the start. Automatic compensation remains out of scope
/// (unchanged from R0.6: `Compensatable` fails closed in the untyped retry
/// path) — the handler is invoked by policy, not by the kernel.
pub trait CompensatableEffect: TypedEffect {}

/// Marker: no safe automatic repetition and no logical undo — send the
/// email, fire the charge without a key (wire class [`Effect::NonIdempotent`];
/// see the module docs for the deliberate naming reconciliation).
///
/// Execution requires an explicit [`ApprovalToken`] scoped to the effect's
/// derived id, presented to [`admit_irreversible`]. Unknown or undeclared
/// effects belong here by default: the wire enum already defaults to
/// `NonIdempotent`, and the typed kernel keeps that convention — what is not
/// declared is never silently retried and never executes without approval.
pub trait IrreversibleEffect: TypedEffect {}

/// Why the kernel refused an admission decision.
///
/// A dedicated error type (not [`crate::error::RustyError`]) because these
/// are contract violations surfaced by the typed API, not runtime execution
/// failures; the executor maps them into its own error path when the
/// enforcement wave lands.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum EffectViolation {
    /// The marker trait and [`TypedEffect::EFFECT`] disagree — a type-level
    /// bug, caught at the enforcement point rather than trusted.
    #[error(
        "effect kind `{kind}` is marked with the {marker:?} marker trait but declares \
         {declared:?}: the marker and TypedEffect::EFFECT must agree"
    )]
    DeclarationMismatch {
        /// The effect's kind identifier.
        kind: String,
        /// The wire class the marker trait implies.
        marker: Effect,
        /// The wire class the type actually declared.
        declared: Effect,
    },

    /// An [`IdempotentEffect`] presented for retry admission without the
    /// idempotency key the declaration is meaningless without.
    #[error(
        "idempotent effect kind `{kind}` has no idempotency key: a stable key is what the \
         idempotency declaration means at the wire"
    )]
    MissingIdempotencyKey {
        /// The effect's kind identifier.
        kind: String,
    },

    /// A [`CompensatableEffect`] with no rollback handler registered for its
    /// kind.
    #[error(
        "compensatable effect kind `{kind}` has no registered rollback handler: register one \
         in a CompensationRegistry before executing it"
    )]
    MissingCompensation {
        /// The effect's kind identifier.
        kind: String,
    },

    /// An [`IrreversibleEffect`] presented for execution with no approval
    /// token at all.
    #[error(
        "irreversible effect kind `{kind}` ({effect_id}) requires an explicit approval token \
         scoped to its effect id"
    )]
    MissingApproval {
        /// The effect's kind identifier.
        kind: String,
        /// The effect id the approval must be scoped to.
        effect_id: EffectId,
    },

    /// A token was presented, but it approves a different effect id — an
    /// approval is scoped to exactly one effect occurrence and is not
    /// transferable.
    #[error(
        "approval token for {presented} does not admit irreversible effect kind `{kind}` \
         ({required})"
    )]
    ApprovalScopeMismatch {
        /// The effect's kind identifier.
        kind: String,
        /// The effect id the admission requires.
        required: EffectId,
        /// The effect id the presented token approves.
        presented: EffectId,
    },

    /// An effect above [`Effect::ReadOnly`] was attempted under a shadow
    /// admission context (R0.12 Operations Plane, wave 4). The shadow's
    /// whole promise is that the candidate's effects never reach the
    /// world, and "idempotent" means safe to retry under one key — not
    /// safe to execute twice from two revisions — so `Idempotent` is
    /// refused alongside the classes above it. The violation carries the
    /// classification and the derived id because it is evidence, not a
    /// stack trace: the shadow's report shows which effects the candidate
    /// would have attempted, classified, never executed.
    #[error(
        "shadow admission refused {effect:?} effect kind `{kind}` ({effect_id}): a shadow \
         admits Pure and ReadOnly effects only — the candidate's effects never reach the world"
    )]
    ShadowRefused {
        /// The effect's kind identifier.
        kind: String,
        /// The wire-level safety class the shadow refused.
        effect: Effect,
        /// The effect id the occurrence derived.
        effect_id: EffectId,
    },
}

/// An explicit, scoped approval to execute one irreversible effect
/// occurrence.
///
/// The token is the approval *boundary made into a value*: constructing one
/// is the decision, and [`admit_irreversible`] accepts it only for the exact
/// [`EffectId`] it was minted against, so an approval for one charge cannot
/// launder another. Within a process this is a proof of explicit decision —
/// it cannot stop code that is determined to forge one, and it is not meant
/// to; cross-process attestation is R0.9's signed-receipt work. What it does
/// guarantee is structural: there is no code path through the typed kernel
/// that executes an irreversible effect on a defaulted or forgotten
/// approval.
///
/// Tokens ride the run via [`crate::executor::RunConfig::with_effect_approvals`]
/// so the approval set travels with the run's configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalToken {
    /// The effect occurrence this token approves.
    effect_id: EffectId,

    /// Who approved — a human operator id, a policy name, a supervision
    /// decision reference. Evidence, not authentication: recorded so the
    /// journal can attribute the boundary when approval journaling lands.
    approved_by: String,
}

impl ApprovalToken {
    /// Mint an approval for exactly one effect id. This constructor is the
    /// approval act: call sites should read as decisions (`let approval =
    /// ApprovalToken::approve(id, "ops:amjad")`), not as plumbing.
    pub fn approve(effect_id: EffectId, approved_by: impl Into<String>) -> Self {
        Self {
            effect_id,
            approved_by: approved_by.into(),
        }
    }

    /// Mint an approval scoped to `effect`'s derived id within `scope` — the
    /// common case, approving the occurrence a run is about to execute.
    pub fn for_effect<E: IrreversibleEffect>(
        effect: &E,
        scope: &str,
        approved_by: impl Into<String>,
    ) -> Self {
        Self::approve(effect.effect_id(scope), approved_by)
    }

    /// The effect id this token approves.
    pub fn effect_id(&self) -> &EffectId {
        &self.effect_id
    }

    /// Who approved.
    pub fn approved_by(&self) -> &str {
        &self.approved_by
    }

    /// Whether this token admits `effect_id`. Scoped, not transferable: only
    /// the exact occurrence it was minted against.
    pub fn admits(&self, effect_id: &EffectId) -> bool {
        &self.effect_id == effect_id
    }
}

/// A registered rollback handler for a [`CompensatableEffect`] kind.
///
/// The handler receives the journaled output of the effect occurrence to
/// undo and returns a compensation record (what was done to undo it) for
/// journaling. Errors are plain strings because compensation failures cross
/// the retry taxonomy's string-classified boundary — a failed compensation
/// is an operator signal, classified and surfaced, never silently retried by
/// the kernel (automatic compensation stays out of scope, as in R0.6).
///
/// `Arc`-wrapped so an admission can hand the caller a cheap clone of the
/// registered undo path.
pub type CompensationHandler = Arc<dyn Fn(&Value) -> Result<Value, String> + Send + Sync>;

/// The registry of rollback handlers backing [`admit_compensatable`].
///
/// Registration is the mechanism that makes a `Compensatable` declaration
/// meaningful: the handler for an effect kind must exist *before* the effect
/// executes, so the undo path is a precondition, not a postmortem. Handlers
/// are keyed by [`TypedEffect::kind`]; re-registering a kind replaces its
/// handler (and returns the old one) so deployments can evolve compensation
/// logic deliberately.
#[derive(Default, Clone)]
pub struct CompensationRegistry {
    handlers: BTreeMap<String, CompensationHandler>,
}

impl std::fmt::Debug for CompensationRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Handlers are opaque closures; the registered kinds are the useful
        // debug surface.
        f.debug_set().entries(self.handlers.keys()).finish()
    }
}

impl CompensationRegistry {
    /// An empty registry: every `Compensatable` admission fails until a
    /// handler is registered — the fail-closed default.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `handler` as the rollback path for `kind`, returning the
    /// handler it replaced, if any.
    pub fn register(
        &mut self,
        kind: impl Into<String>,
        handler: CompensationHandler,
    ) -> Option<CompensationHandler> {
        self.handlers.insert(kind.into(), handler)
    }

    /// The handler registered for `kind`, if any.
    pub fn handler_for(&self, kind: &str) -> Option<&CompensationHandler> {
        self.handlers.get(kind)
    }
}

/// One shadow refusal, structured for journaling (R0.12 wave 4): the
/// effect the candidate would have attempted, classified, with the
/// recorded-outcome disposition. The shadow's report is built from these
/// — which effects it refused, and which of them the source run's journal
/// could answer for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShadowRefusal {
    /// The refused effect's kind identifier.
    pub kind: String,

    /// The wire-level safety class the shadow refused.
    pub effect: Effect,

    /// The effect id the occurrence derived in the shadow's scope.
    pub effect_id: EffectId,

    /// The canonical input digest of the refused call.
    pub input_hash: String,

    /// Whether the recorded world answered for the refused effect: the
    /// source run's journal held a matching outcome, and the shadow
    /// served it (the hybrid-replay rule — pin the effect, re-run the
    /// decision). `false` means the candidate diverged from the
    /// recording at this call: the journal has no answer to a call the
    /// recorded world never received.
    pub served: bool,
}

/// The recorded world a shadow serves refused effects from (R0.12
/// wave 4). Implemented over a source run's journal
/// ([`crate::replay::JournalShadowSource`]); the kernel owns the
/// boundary, the journal owns the evidence.
///
/// `recorded_request` is the request payload in the *recorded* shape the
/// journal holds (for tools, [`crate::replay::tool_call_request`]) — the
/// caller builds it from the live call so the source can match name and
/// arguments, not merely the effect kind.
pub trait ShadowOutcomeSource: Send + Sync + std::fmt::Debug {
    /// The recorded outcome of one refused call, when the source run
    /// recorded it. `None` is a divergence, not an error: the candidate
    /// attempted something the recorded world never saw.
    fn serve(&self, kind: &str, recorded_request: &Value) -> Option<Value>;
}

/// Where a shadow's refusals go the moment they happen: the shadow run's
/// own journal, in the server's construction. A refusal the shadow did
/// not record is a refusal that can be retroactively denied.
pub type ShadowRefusalSink = Arc<dyn Fn(&ShadowRefusal) + Send + Sync>;

/// A run-scoped, fail-closed admission boundary for runtime effect requests.
///
/// The scope is normally a thread id. Approvals are indexed by their exact
/// deterministic [`EffectId`], and compensations are captured when a request
/// is admitted so the rollback path remains alive for the entire call.
///
/// A context built by [`EffectAdmissionContext::shadow`] is the R0.12
/// shadow boundary: it admits [`Effect::Pure`] and [`Effect::ReadOnly`]
/// and refuses everything above — `Idempotent` included, because
/// "idempotent" means safe to retry under one key, not safe to execute
/// twice from two revisions, and a shadowed charge is a charge. The
/// shadow holds no approval tokens to consume and no retry path around
/// the refusal: there is no code path through a shadow context that
/// executes an effect above `ReadOnly`.
#[derive(Clone)]
pub struct EffectAdmissionContext {
    scope: String,
    approvals: Arc<Mutex<BTreeMap<EffectId, ApprovalToken>>>,
    compensations: CompensationRegistry,
    /// Present only for shadow contexts: the recorded world refused
    /// effects are served from, and the sink refusals report to.
    shadow: Option<ShadowBoundary>,
}

/// The shadow half of an [`EffectAdmissionContext`], cloned cheaply.
#[derive(Clone)]
struct ShadowBoundary {
    source: Arc<dyn ShadowOutcomeSource>,
    sink: ShadowRefusalSink,
}

impl std::fmt::Debug for EffectAdmissionContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let approvals = self.approvals.lock().unwrap_or_else(|e| e.into_inner());
        f.debug_struct("EffectAdmissionContext")
            .field("scope", &self.scope)
            .field("approval_effect_ids", &approvals.keys())
            .field("compensations", &self.compensations)
            .field("shadow", &self.shadow.is_some())
            .finish()
    }
}

impl EffectAdmissionContext {
    /// Start an admission boundary for `scope` with no approvals or rollback
    /// handlers. This is intentionally fail-closed for compensatable and
    /// irreversible effects.
    pub fn new(scope: impl Into<String>) -> Self {
        Self {
            scope: scope.into(),
            approvals: Arc::new(Mutex::new(BTreeMap::new())),
            compensations: CompensationRegistry::new(),
            shadow: None,
        }
    }

    /// Start a shadow admission boundary for `scope` (R0.12 wave 4):
    /// admits `Pure` and `ReadOnly`, refuses everything above with
    /// [`EffectViolation::ShadowRefused`]. `source` is the recorded world
    /// refused effects are served from; `sink` receives every refusal the
    /// moment it happens, served or not — the refusal is the shadow's
    /// primary evidence.
    pub fn shadow(
        scope: impl Into<String>,
        source: Arc<dyn ShadowOutcomeSource>,
        sink: ShadowRefusalSink,
    ) -> Self {
        Self {
            scope: scope.into(),
            approvals: Arc::new(Mutex::new(BTreeMap::new())),
            compensations: CompensationRegistry::new(),
            shadow: Some(ShadowBoundary { source, sink }),
        }
    }

    /// Attach the exact approval tokens available to this run.
    pub fn with_approvals(mut self, approvals: impl IntoIterator<Item = ApprovalToken>) -> Self {
        self.approvals = Arc::new(Mutex::new(
            approvals
                .into_iter()
                .map(|token| (token.effect_id().clone(), token))
                .collect(),
        ));
        self
    }

    /// Attach the run's registered rollback handlers.
    pub fn with_compensations(mut self, compensations: CompensationRegistry) -> Self {
        self.compensations = compensations;
        self
    }

    /// The run scope used to derive effect ids.
    pub fn scope(&self) -> &str {
        &self.scope
    }

    /// Whether this is a shadow boundary (built by
    /// [`EffectAdmissionContext::shadow`]).
    pub fn is_shadow(&self) -> bool {
        self.shadow.is_some()
    }

    /// Admit one effect occurrence, or reject it before its body runs.
    ///
    /// Pure and read-only calls require no additional evidence. Idempotent
    /// calls require a stable key, compensatable calls require a registered
    /// rollback handler, and non-idempotent calls atomically consume an
    /// approval token for this exact content-addressed occurrence. Cloned
    /// contexts share the same approval ledger, so one token admits one call
    /// across parallel nodes and later super-steps.
    ///
    /// Under a shadow boundary every class above `ReadOnly` refuses with
    /// [`EffectViolation::ShadowRefused`] — the idempotency-key,
    /// compensation, and approval rules never come into play, because the
    /// shadow executes nothing above `ReadOnly` however well-declared it
    /// is.
    pub fn admit(&self, request: &EffectRequest) -> Result<EffectPermit, EffectViolation> {
        let effect_id = request.effect_id(self.scope());
        if self.is_shadow() && !matches!(request.effect(), Effect::Pure | Effect::ReadOnly) {
            return Err(EffectViolation::ShadowRefused {
                kind: request.kind().to_owned(),
                effect: request.effect(),
                effect_id,
            });
        }
        let (compensation, approval) = match request.effect() {
            Effect::Pure | Effect::ReadOnly => (None, None),
            Effect::Idempotent => {
                if request.idempotency_key().is_none() {
                    return Err(EffectViolation::MissingIdempotencyKey {
                        kind: request.kind().to_owned(),
                    });
                }
                (None, None)
            }
            Effect::Compensatable => (
                Some(
                    self.compensations
                        .handler_for(request.kind())
                        .cloned()
                        .ok_or_else(|| EffectViolation::MissingCompensation {
                            kind: request.kind().to_owned(),
                        })?,
                ),
                None,
            ),
            Effect::NonIdempotent => {
                let approval = self
                    .approvals
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&effect_id)
                    .ok_or_else(|| EffectViolation::MissingApproval {
                        kind: request.kind().to_owned(),
                        effect_id: effect_id.clone(),
                    })?;
                (None, Some(approval))
            }
        };
        Ok(EffectPermit {
            effect_id,
            compensation,
            approval,
        })
    }

    /// Serve a refused effect from the recorded world (R0.12 wave 4, the
    /// hybrid-replay rule): only meaningful for a shadow boundary and a
    /// [`EffectViolation::ShadowRefused`] violation — anything else is not
    /// the shadow's to serve, and answers `None`.
    ///
    /// Every shadow refusal reports to the sink, served or not, because
    /// the refusal is the evidence; the recorded outcome is the
    /// convenience that lets the shadow's decisions continue against the
    /// recorded world. Returns the recorded outcome when the source run's
    /// journal held one (`None`: the candidate diverged — the journal has
    /// no answer to a call the recorded world never received, and the
    /// caller surfaces the violation instead).
    pub fn serve_shadow(
        &self,
        request: &EffectRequest,
        recorded_request: &Value,
        violation: &EffectViolation,
    ) -> Option<Value> {
        let Some(boundary) = &self.shadow else {
            return None;
        };
        let EffectViolation::ShadowRefused {
            kind,
            effect,
            effect_id,
        } = violation
        else {
            return None;
        };
        let served = boundary.source.serve(kind, recorded_request);
        (boundary.sink)(&ShadowRefusal {
            kind: kind.clone(),
            effect: *effect,
            effect_id: effect_id.clone(),
            input_hash: request.input_hash().to_owned(),
            served: served.is_some(),
        });
        served
    }
}

/// Evidence that one runtime effect occurrence passed admission.
///
/// The permit deliberately owns the selected compensation handler. The tool
/// dispatcher keeps it alive until the call completes, preventing a registry
/// update from removing the rollback path after admission but before the
/// effect finishes.
#[derive(Clone)]
pub struct EffectPermit {
    effect_id: EffectId,
    compensation: Option<CompensationHandler>,
    approval: Option<ApprovalToken>,
}

impl std::fmt::Debug for EffectPermit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EffectPermit")
            .field("effect_id", &self.effect_id)
            .field("has_compensation", &self.compensation.is_some())
            .field(
                "approved_by",
                &self.approval.as_ref().map(ApprovalToken::approved_by),
            )
            .finish()
    }
}

impl EffectPermit {
    /// The admitted occurrence's deterministic id.
    pub fn effect_id(&self) -> &EffectId {
        &self.effect_id
    }

    /// The rollback handler captured for a compensatable occurrence.
    pub fn compensation(&self) -> Option<&CompensationHandler> {
        self.compensation.as_ref()
    }

    /// The one-shot approval consumed for an irreversible occurrence.
    pub fn approval(&self) -> Option<&ApprovalToken> {
        self.approval.as_ref()
    }
}

/// The marker/class agreement check every admission helper runs first.
///
/// The marker traits carry no methods, so a mismatched pair (`impl
/// IrreversibleEffect` with `EFFECT = Effect::Pure`) compiles; it must not
/// *pass*. Checking at admission keeps the typed API honest without needing
/// compile-fail machinery: a lying declaration is rejected where it would be
/// exploited.
fn check_declaration<E: TypedEffect>(effect: &E, marker: Effect) -> Result<(), EffectViolation> {
    if E::EFFECT != marker {
        return Err(EffectViolation::DeclarationMismatch {
            kind: effect.kind().to_owned(),
            marker,
            declared: E::EFFECT,
        });
    }
    Ok(())
}

/// Admit a [`PureEffect`] to caching/speculation.
///
/// Always succeeds for an honestly declared type — the helper exists so the
/// speculator's call site names the precondition it relies on, and so a
/// mismatched declaration (marker trait disagreeing with
/// [`TypedEffect::EFFECT`]) is caught here rather than after a speculated
/// side effect.
pub fn admit_speculation<E: PureEffect>(effect: &E) -> Result<(), EffectViolation> {
    check_declaration::<E>(effect, Effect::Pure)
}

/// Admit an [`IdempotentEffect`] to automatic retry.
///
/// The typed form of the [`crate::durable::classify_retry`] effect gate, with
/// the convention made checkable: the effect must carry the idempotency key
/// its declaration means nothing without. Untyped `Effect` values keep their
/// R0.6 behavior exactly — this helper gates only the typed path.
pub fn admit_retry<E: IdempotentEffect>(effect: &E) -> Result<(), EffectViolation> {
    check_declaration::<E>(effect, Effect::Idempotent)?;
    if effect.idempotency_key().is_none() {
        return Err(EffectViolation::MissingIdempotencyKey {
            kind: effect.kind().to_owned(),
        });
    }
    Ok(())
}

/// Admit a [`CompensatableEffect`] to execute, returning its registered
/// rollback handler.
///
/// The handler is returned — not merely checked — so the caller holds the
/// undo path from the moment the effect is admitted; an effect whose kind
/// has no registered handler is rejected.
pub fn admit_compensatable<E: CompensatableEffect>(
    effect: &E,
    registry: &CompensationRegistry,
) -> Result<CompensationHandler, EffectViolation> {
    check_declaration::<E>(effect, Effect::Compensatable)?;
    registry.handler_for(effect.kind()).cloned().ok_or_else(|| {
        EffectViolation::MissingCompensation {
            kind: effect.kind().to_owned(),
        }
    })
}

/// Admit an [`IrreversibleEffect`] to execute behind an explicit approval.
///
/// `scope` must be the run scope the effect id was (and will be) derived in
/// — the same value recovery uses to re-derive the id. The presented token
/// must be scoped to exactly that id; anything else — no token, or a token
/// for another occurrence — is rejected. The approval boundary this wave
/// lands as a helper becomes an executor enforcement point in a later wave.
pub fn admit_irreversible<E: IrreversibleEffect>(
    effect: &E,
    scope: &str,
    approval: Option<&ApprovalToken>,
) -> Result<(), EffectViolation> {
    check_declaration::<E>(effect, Effect::NonIdempotent)?;
    let required = effect.effect_id(scope);
    let token = approval.ok_or_else(|| EffectViolation::MissingApproval {
        kind: effect.kind().to_owned(),
        effect_id: required.clone(),
    })?;
    if !token.admits(&required) {
        return Err(EffectViolation::ApprovalScopeMismatch {
            kind: effect.kind().to_owned(),
            required,
            presented: token.effect_id().clone(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effect_id_derivation_is_deterministic_and_scoped() {
        let a = derive_effect_id("run-1", "charge_card", "abc123", Some("key-7"));
        let b = derive_effect_id("run-1", "charge_card", "abc123", Some("key-7"));
        assert_eq!(a, b, "same inputs must derive the same id");
        assert_eq!(a.as_str().len(), 64, "ids are hex SHA-256 digests");

        // Every tuple component is load-bearing.
        assert_ne!(
            a,
            derive_effect_id("run-2", "charge_card", "abc123", Some("key-7"))
        );
        assert_ne!(
            a,
            derive_effect_id("run-1", "refund", "abc123", Some("key-7"))
        );
        assert_ne!(
            a,
            derive_effect_id("run-1", "charge_card", "def456", Some("key-7"))
        );
        assert_ne!(
            a,
            derive_effect_id("run-1", "charge_card", "abc123", Some("key-8"))
        );
        assert_ne!(a, derive_effect_id("run-1", "charge_card", "abc123", None));
    }

    #[test]
    fn effect_id_is_serde_transparent() {
        let id = derive_effect_id("run-1", "charge_card", "abc123", None);
        let value = serde_json::to_value(&id).unwrap();
        assert_eq!(value, serde_json::json!(id.as_str()));
        let back: EffectId = serde_json::from_value(value).unwrap();
        assert_eq!(back, id);
    }

    // ---------- the shadow boundary (R0.12 wave 4) ----------

    #[derive(Debug)]
    struct MapSource {
        outcomes: BTreeMap<String, Value>,
    }

    impl ShadowOutcomeSource for MapSource {
        fn serve(&self, kind: &str, _recorded_request: &Value) -> Option<Value> {
            self.outcomes.get(kind).cloned()
        }
    }

    fn shadow_context() -> (EffectAdmissionContext, Arc<Mutex<Vec<ShadowRefusal>>>) {
        let refusals = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let refusals = Arc::clone(&refusals);
            Arc::new(move |refusal: &ShadowRefusal| {
                refusals.lock().unwrap().push(refusal.clone());
            }) as ShadowRefusalSink
        };
        let source = Arc::new(MapSource {
            outcomes: BTreeMap::from([(
                "charge_card".to_owned(),
                serde_json::json!({"id": "ch_1"}),
            )]),
        });
        (
            EffectAdmissionContext::shadow("shadow-run", source, sink),
            refusals,
        )
    }

    #[test]
    fn shadow_admits_pure_and_read_only_and_refuses_everything_above() {
        let (shadow, _refusals) = shadow_context();
        for effect in [Effect::Pure, Effect::ReadOnly] {
            let request = EffectRequest::new("lookup", effect, &serde_json::json!({}), None);
            assert!(
                shadow.admit(&request).is_ok(),
                "{effect:?} is admitted under a shadow"
            );
        }
        // Idempotent included: "safe to retry under one key" is not "safe
        // to execute twice from two revisions". No key, handler, or
        // approval changes the answer — the shadow holds none of them.
        for effect in [
            Effect::Idempotent,
            Effect::Compensatable,
            Effect::NonIdempotent,
        ] {
            let request = EffectRequest::new(
                "charge_card",
                effect,
                &serde_json::json!({"amount": 5}),
                Some("key-1".to_owned()),
            );
            match shadow.admit(&request) {
                Err(EffectViolation::ShadowRefused {
                    kind,
                    effect: refused,
                    ..
                }) => {
                    assert_eq!(kind, "charge_card");
                    assert_eq!(refused, effect);
                }
                other => panic!("{effect:?} must refuse under a shadow, got {other:?}"),
            }
        }
    }

    #[test]
    fn shadow_refusals_report_and_serve_only_from_the_recorded_world() {
        let (shadow, refusals) = shadow_context();
        let request = EffectRequest::new(
            "charge_card",
            Effect::NonIdempotent,
            &serde_json::json!({"amount": 5}),
            None,
        );
        let violation = shadow.admit(&request).unwrap_err();
        let served = shadow.serve_shadow(&request, &serde_json::json!({}), &violation);
        assert_eq!(served, Some(serde_json::json!({"id": "ch_1"})));

        // A kind the recorded world never saw is a divergence: the
        // refusal still reports, with `served: false`.
        let unknown = EffectRequest::new(
            "send_email",
            Effect::NonIdempotent,
            &serde_json::json!({}),
            None,
        );
        let violation = shadow.admit(&unknown).unwrap_err();
        assert_eq!(
            shadow.serve_shadow(&unknown, &serde_json::json!({}), &violation),
            None
        );

        let refusals = refusals.lock().unwrap();
        assert_eq!(refusals.len(), 2);
        assert!(refusals[0].served);
        assert!(!refusals[1].served);
        assert_eq!(refusals[1].kind, "send_email");

        // Non-shadow contexts and non-shadow violations are not the
        // shadow's to serve.
        let plain = EffectAdmissionContext::new("run-1");
        let violation = EffectViolation::ShadowRefused {
            kind: "charge_card".to_owned(),
            effect: Effect::NonIdempotent,
            effect_id: request.effect_id("run-1"),
        };
        assert_eq!(
            plain.serve_shadow(&request, &serde_json::json!({}), &violation),
            None
        );
    }
}
