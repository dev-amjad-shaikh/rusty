//! Flight Recorder contracts: the canonical, serde-versioned evidence schema.
//!
//! This module freezes the wire shapes that every later wave of the Flight
//! Recorder (replay engine, server API, Studio UI, executor learning) builds
//! on. Nothing here performs I/O or execution — these are pure data
//! contracts plus the small hashing helpers they need.
//!
//! The four pillars:
//!
//! - [`Effect`] — the effect taxonomy. Every journaled event declares which
//!   class of side effect produced it; the class is what later lets the
//!   runtime decide whether an effect may be retried, served from a journal
//!   during exact replay, or must be re-executed.
//! - [`RunEvent`] — one recorded fact about a run (a super-step boundary, a
//!   node input/output, a model/tool/remote/WASM call, an interrupt, a
//!   routing decision, a checkpoint write), with causal parentage.
//! - [`DecisionEvent`] — one policy decision with the context needed for
//!   offline learning: features, the closed legal-action set, the selected
//!   action, its propensity, and the policy version that made the choice.
//!   Executor learning lands in R0.8+; the contract freezes now so R0.5
//!   journals are already learnable evidence.
//! - [`CheckpointHeader`] — format version, graph version/hash, active
//!   policy version, and logical clock, carried by every checkpoint so old
//!   snapshots can be interpreted and replayed faithfully.
//!
//! Golden-file tests under `tests/golden/` pin these serialized shapes;
//! any accidental contract drift fails CI.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::error::RustyError;
use crate::llm::Usage;

/// The current on-disk format version of [`CheckpointHeader`].
///
/// Bump only on a breaking change to the checkpoint envelope; additive
/// evolution uses serde defaults instead so previously written checkpoints
/// keep deserializing.
pub const CURRENT_FORMAT_VERSION: u32 = 1;

/// Payloads at or below this many serialized bytes travel inline in a
/// [`RunEvent`]; larger ones are content-addressed as [`ArtifactRef`]s.
///
/// The journal keeps the artifact bytes itself (in-memory impl), so a
/// journal snapshot is always self-contained — the reference is a size and
/// dedup optimization, not a pointer to external storage.
pub const INLINE_PAYLOAD_MAX_BYTES: usize = 4096;

/// Lowercase hex SHA-256 digest of `bytes`. The one hashing primitive shared
/// by artifact references, journal heads, and graph topology hashes.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// The effect taxonomy: what a journaled event did to the world outside the
/// run's own state.
///
/// The classification is declared by the producer (node/model/tool traits
/// carry a default with an override point) and recorded on every
/// [`RunEvent`]. It is the input to three later policies:
///
/// - **Retry** (R0.6): which failed effects may be re-attempted at all, and
///   under what key.
/// - **Replay** (R0.5 later waves): which effects exact replay may serve
///   from the journal versus must re-execute.
/// - **Capsules** (R0.9): which effects a sandboxed capsule may perform at
///   all under its capability grants.
///
/// The order of variants is a severity ladder: each class permits strictly
/// less automation freedom than the one before. The `Ord` derive is that
/// ladder made mechanical (declaration order), which is what capsule
/// manifests (R0.9) compare declared effects against grant-implied minima
/// with.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    /// No observable effect beyond its return value: a deterministic function
    /// of its inputs. Re-execution is always safe and always equivalent, so
    /// replay may either re-run it or reuse the journaled output, and retries
    /// are unconstrained. Default for plain compute nodes.
    Pure,

    /// Reads external state but writes nothing (a GET, a file read, a
    /// lookup). Re-execution is safe but **not** necessarily equivalent — the
    /// world may have changed — so exact replay serves the journaled output
    /// while live replay re-reads. Retries are unconstrained.
    ReadOnly,

    /// Writes external state, but repeating the same call with the same
    /// idempotency key has the same effect as calling once (PUT semantics,
    /// upserts). Safe to retry under a stable key; exact replay may serve
    /// the journaled receipt instead of re-sending.
    Idempotent,

    /// Writes external state and repeating it duplicates the effect, but a
    /// declared compensating action can logically undo it (charge/refund).
    /// Retry only with care; replay and rollback policy must pair the effect
    /// with its compensation. v1 records the classification only —
    /// compensation registration arrives with durable work (R0.6).
    Compensatable,

    /// Writes external state with no safe automatic repetition (send an
    /// email, charge a card, POST without a key). Never silently retried,
    /// never served from a journal in any replay mode that claims fidelity —
    /// re-execution is an explicit, caller-approved decision. Default for
    /// model and tool calls, which the runtime cannot prove otherwise.
    NonIdempotent,
}

impl Effect {
    /// Whether re-executing this effect during replay or retry is
    /// unconditionally safe (no duplication risk). `Compensatable` and
    /// `NonIdempotent` are the only classes requiring human or policy
    /// approval before re-execution.
    pub fn is_freely_repeatable(self) -> bool {
        matches!(self, Effect::Pure | Effect::ReadOnly | Effect::Idempotent)
    }
}

/// A versioned identity for the executor policy that was active during a
/// run or made a [`DecisionEvent`].
///
/// Newtype over `String` so the type system — not convention — keeps policy
/// versions distinct from graph versions, model names, and other strings.
/// The default is the static, no-learning floor: every run before the
/// policy plane lands (R0.8) records this version.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PolicyVersion(pub String);

impl PolicyVersion {
    /// The static default policy: no learned behavior, fixed executor
    /// constants. This is the floor that learned policies (R0.10) are
    /// evaluated against and the version every pre-learning run records.
    pub const STATIC_V0: &'static str = "static-v0";

    /// Wrap a version string.
    pub fn new(version: impl Into<String>) -> Self {
        Self(version.into())
    }

    /// The version string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for PolicyVersion {
    fn default() -> Self {
        Self(Self::STATIC_V0.to_owned())
    }
}

impl std::fmt::Display for PolicyVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A content-addressed reference to a payload too large to travel inline in
/// an event (see [`INLINE_PAYLOAD_MAX_BYTES`]).
///
/// The hash is the identity: two events referencing the same `sha256`
/// reference the same bytes. Consumers resolve references through the
/// journal snapshot's artifact map; nothing here points outside the
/// snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ArtifactRef {
    /// Lowercase hex SHA-256 of the canonical JSON serialization of the
    /// payload.
    pub sha256: String,

    /// Serialized size of the payload in bytes.
    pub bytes: u64,
}

/// How an event's input or output payload is carried.
///
/// Small values are embedded ([`PayloadRef::Inline`]); large values are
/// content-addressed ([`PayloadRef::Artifact`]) with their bytes held in the
/// journal's artifact map. The split keeps events cheap to scan (sequences,
/// causal links, statuses) without forcing payloads out of the snapshot.
///
/// Serialized with adjacent tagging (`{"kind": "inline", "value": …}`):
/// payloads are arbitrary JSON, so the tag must not be flattened into the
/// payload the way internal tagging would require.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum PayloadRef {
    /// The payload itself, embedded in the event.
    Inline(Value),

    /// A content hash of the payload; bytes live in the journal snapshot's
    /// artifact map under the same hash.
    Artifact(ArtifactRef),
}

impl PayloadRef {
    /// Always-inline reference (test convenience and small-value paths).
    pub fn inline(value: Value) -> Self {
        PayloadRef::Inline(value)
    }

    /// The content hash of the payload, whether inline or referenced.
    ///
    /// Hashing is over the canonical `serde_json` serialization (object keys
    /// sort deterministically — see `canonicalize_value`), so equal
    /// payloads hash equal regardless of which representation carried them.
    pub fn content_hash(&self) -> Result<String, serde_json::Error> {
        match self {
            PayloadRef::Inline(value) => {
                let bytes = serde_json::to_vec(&canonicalize_value(value))?;
                Ok(sha256_hex(&bytes))
            }
            PayloadRef::Artifact(reference) => Ok(reference.sha256.clone()),
        }
    }
}

/// The effect's own confirmation of an `Idempotent` side effect, journaled
/// as the output payload of a [`RunEventKind::EffectReceipt`] event (R0.6
/// Durable Work).
///
/// A receipt is the proof the *provider* accepted the effect exactly once:
/// its own confirmation id, under the idempotency key the caller supplied.
/// Two consumers depend on it:
///
/// - **Operators** auditing a run can trace every effect to the provider's
///   record of it (`provider` + `provider_id`).
/// - **Exact replay** serves the journaled receipt instead of re-sending the
///   effect — the same rule the Flight Recorder applies to journaled model
///   and tool calls, extended across the crash boundary between a run and
///   its queue-dispatched tasks. The replay lookup is keyed on
///   [`EffectReceipt::idempotency_key`] (see
///   [`crate::journal::JournalSnapshot::find_effect_receipt`]), not on event
///   sequence: a task completes outside the run's super-step order.
///
/// Serialized inside the event's output [`PayloadRef`], so the event
/// envelope stays unchanged and old journals keep deserializing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EffectReceipt {
    /// The system that confirmed the effect (a provider name — `stripe`,
    /// `sendgrid` — or any store with idempotent-put semantics).
    pub provider: String,

    /// The provider's own confirmation id (charge id, message id, version
    /// stamp) — the handle an audit uses to find the effect at the provider.
    pub provider_id: String,

    /// The idempotency key the effect was performed under — the key the task
    /// envelope carried and the recipient passed to the provider. This is
    /// the replay lookup key: a re-driven run asks "did this key already
    /// land?" and the journal answers with this receipt.
    pub idempotency_key: String,

    /// The durable task whose completion produced this receipt, when the
    /// effect was queue-dispatched. `None` for effects a run performed
    /// in-process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,

    /// The deterministic effect id (R0.7 effect kernel) of the effect this
    /// receipt confirms — [`crate::effects::derive_effect_id`] over the
    /// run scope, effect kind, input hash, and idempotency key. Recovery
    /// re-derives the id of the effect it is about to perform and looks the
    /// receipt up by it (see
    /// [`crate::journal::JournalSnapshot::find_effect_receipt_by_effect_id`]),
    /// which is how "did this exact effect already commit?" becomes
    /// answerable without re-execution.
    ///
    /// Additive like `task_id`: absent (not null) on the wire when unset, so
    /// pre-R0.7 receipts and consumers see no shape change. `None` is honest
    /// for receipts journaled by writers that predate the typed kernel; the
    /// [`EffectReceipt::idempotency_key`] lookup keeps serving them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect_id: Option<String>,
}
/// One incremental chunk of a streaming assistant response, journaled
/// durably before in-memory assembly consumes it (EP-01-S11).
///
/// Chunks are fidelity evidence: they replay byte-for-byte so a UI can
/// reconstruct the exact stream that originally rendered, while
/// `derive_messages` uses only the assembled `ChatMessage`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantChunk {
    /// The incremental text produced since the previous chunk.
    pub delta: String,
    /// Strictly monotonic index from 0 within the step.
    pub stream_index: u64,
    /// `true` on the terminal chunk of the stream.
    #[serde(default)]
    pub finish: bool,
}

/// The outcome status of a journaled event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EventStatus {
    /// Completed normally.
    Ok,
    /// Failed. The error description travels in the event's output payload.
    Error,
    /// Suspended the run (a node called `interrupt()`). Control flow, not a
    /// failure — the payload is the interrupt value.
    Interrupted,
}

/// What a [`RunEvent`] records. Closed set; replay and analysis code matches
/// exhaustively on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunEventKind {
    /// A super-step began; input lists the activated node set.
    SuperStepStart,
    /// A super-step merged at the barrier; output carries the post-reducer
    /// channel values (the reducer result for the step).
    SuperStepEnd,
    /// A node invocation was scheduled; input is its (scoped) state snapshot.
    NodeInput,
    /// A node invocation finished; output is its partial updates plus any
    /// routing command, with the measured latency.
    NodeOutput,
    /// A chat-model call; input is the request (messages + tool schemas),
    /// output the response, with model identity, token usage, and cost where
    /// reported.
    ModelCall,
    /// A tool invocation; input is the arguments, output the result.
    ToolCall,
    /// A remote-node call to a worker over the wire protocol; input is the
    /// `NodeTask`, output the `NodeTaskResponse` payload.
    RemoteCall,
    /// A WASM guest-module invocation; input is the guest input, output the
    /// guest output.
    WasmCall,
    /// A node suspended the run; input is the interrupt payload.
    Interrupt,
    /// The run resumed from a checkpoint; input carries the checkpoint id
    /// and, when present, the resume value.
    Resume,
    /// The routing phase selected the next active set; output describes the
    /// planned invocations (including `Send` fan-outs).
    RoutingDecision,
    /// A checkpoint was persisted; output carries the checkpoint id, step,
    /// and journal head reference stamped into it.
    CheckpointWritten,
    /// An `Idempotent` effect's own confirmation, journaled by the effect's
    /// recipient (R0.6 Durable Work): output carries the [`EffectReceipt`] —
    /// the provider's confirmation id plus the idempotency key the effect
    /// ran under. Exact replay serves the receipt instead of re-sending the
    /// effect: the same journaled-output rule model and tool calls follow,
    /// extended across the crash boundary between a run and its durable
    /// tasks.
    EffectReceipt,

    /// An agent was spawned (R0.7 Agent Fabric): input carries the agent id,
    /// its pinned [`crate::agents::CapabilityManifest`], and the declared
    /// [`crate::agents::StateScope`]s — the manifest's scopes are journaled
    /// with the spawn so later access checks replay against the same
    /// declaration. Wave 1 lands the variant as an inert contract; the agent
    /// host emits it.
    AgentSpawn,

    /// An agent terminated (R0.7): output carries the terminal disposition
    /// (completed, failed, cancelled) and the final checkpoint reference.
    /// Wired in wave 2 for the cancellation tree: the server journals it
    /// into the agent's supervision journal when an agent/team cancel
    /// actually touches work. Host-emitted terminal exits land with the
    /// agent host.
    AgentExit,

    /// A mailbox message was submitted to an agent's mailbox (R0.7): output
    /// carries the recipient (`agent:{agent_id}`), the message kind, and the
    /// task id the queue assigned. The sender-side half of the mailbox
    /// journal pair; the envelope's `parent` links it into the team's causal
    /// tree. Wired in wave 3: the coordination runtime journals it into the
    /// pattern's journal for every member task it submits. Host-emitted
    /// sends land with the agent host.
    MailboxSend,

    /// An agent's activation began a turn on a mailbox message (R0.7): input
    /// carries the task id and idempotency key the turn is processing.
    /// The recipient-side half of the mailbox pair. Wired in wave 3 as the
    /// settlement observation: the coordination runtime journals it when a
    /// member task settles, with the terminal status and result — the
    /// pattern's evidence half of the pair. Host-emitted turn begins land
    /// with the agent host.
    MailboxReceive,

    /// A supervision decision was made (R0.7 wave 2): output carries the
    /// policy (`permanent` / `transient` / `temporary`), the triggering
    /// failure's [`EventStatus`] / error class, and the restart ordinal —
    /// the journaled decision record that makes "no restart without a
    /// journaled decision" auditable. Wired in wave 2: the server journals
    /// it into the agent's supervision journal on restart / escalate /
    /// manual-restart decisions.
    SupervisionEvent,

    /// A coordination pattern (delegate / fan-out / race / quorum) began
    /// (R0.7 wave 3): output carries the pattern's typed contract — members,
    /// thresholds, effect declarations. The team's causal root for every
    /// event the pattern spawns. Wired in wave 3: the coordination runtime
    /// journals it on the pattern's first drive, before any member task
    /// exists.
    CoordinationStart,

    /// A coordination pattern settled (R0.7 wave 3): output carries the
    /// result reference and the per-member dispositions (completed, failed,
    /// cancelled) — the fan-in evidence record. Wired in wave 3: the
    /// coordination runtime journals it exactly once, as the pattern's
    /// terminal fact.
    CoordinationEnd,

    /// A governed memory retrieval was performed (R0.8 Rusty Learn, wave 1):
    /// input is the canonical request — the resolved
    /// [`crate::memory::MemoryQuery`] (with `as_of` stamped through the
    /// run's clock) plus the [`crate::memory::ContextBudget`] — and output
    /// is the journaled [`crate::memory::MemoryAssembly`]: the record ids
    /// and their order, the packed records, and the token accounting.
    /// Declared [`Effect::ReadOnly`]: exact replay serves the journaled
    /// assembly instead of re-querying the store — the same
    /// journaled-output rule model and tool calls follow, applied to the
    /// memory seam so candidate evaluation is reproducible.
    MemoryRead,

    /// A governed memory write landed (R0.8 wave 1): an
    /// [`Effect::Idempotent`] effect under the derived key
    /// `memory:{scope}:{memory_id}` (see [`crate::memory::memory_effect_key`]),
    /// so retried submissions converge. Input carries the effect key and the
    /// memory id; output carries the stored
    /// [`crate::memory::MemoryRecord`] with its provenance — the write's
    /// attribution is journaled, not implied.
    MemoryWrite,

    /// A memory record was forgotten (R0.8 wave 2): real deletion of
    /// derived state, with the journaled tombstone as the erasure receipt.
    /// An [`Effect::Idempotent`] effect under the derived key
    /// `memory_forget:{scope}:{memory_id}` (see
    /// [`crate::memory::memory_forget_effect_key`]), so retried erasures
    /// converge. Input carries the effect key and the memory id; output
    /// carries the [`crate::memory::MemoryForgetTombstone`] — the id, scope,
    /// reason, and dependent invalidations, metadata by construction:
    /// the tombstone has no content field, so the forgotten bytes cannot
    /// leak through the receipt of their erasure. Corrections are not a
    /// separate variant: they journal through `MemoryWrite` with the
    /// correction's attribution in the derived record's provenance.
    MemoryForget,

    /// A learning candidate was created (R0.8 wave 3): a distiller's
    /// proposed change landed as an immutable, content-addressed
    /// [`crate::learn::Candidate`] — identity is integrity, so two
    /// distillations of the same change converge on one id and a tampered
    /// candidate fails its own address. An [`Effect::Idempotent`] effect
    /// under the derived key `candidate:{candidate_id}` (see
    /// [`crate::learn::candidate_effect_key`]), so retried submissions
    /// converge. Input carries the effect key and the candidate id; output
    /// carries the candidate itself — the distiller's identity and the
    /// evidence span it read are journaled with it, not implied.
    CandidateCreated,

    /// A candidate was evaluated against recorded evidence (R0.8 wave 3):
    /// replay re-drove recorded runs with the candidate applied, the
    /// experiment runner graded it over the versioned dataset, and the
    /// comparison diffed it against the baseline. An
    /// [`Effect::Idempotent`] effect under the derived key
    /// `evaluation:{candidate_id}:{dataset_version}` (see
    /// [`crate::learn::evaluation_effect_key`]): re-evaluation against the
    /// same dataset version converges; a new dataset version is a new
    /// evaluation. Output carries the journaled
    /// [`crate::learn::CandidateEvaluation`] — the replay divergence
    /// summary, the report pair, the dataset version, and the verdict.
    /// The evaluation is evidence, not a log line.
    CandidateEvaluated,

    /// A candidate was promoted (R0.8 wave 3): the active-version pointer
    /// for its production surface moved. An [`Effect::Idempotent`] effect
    /// under the derived key `promotion:{candidate_id}` (see
    /// [`crate::learn::promotion_effect_key`]) — retried promotions
    /// converge, and recovery re-derives the same key. Output carries the
    /// [`crate::learn::PromotionReceipt`]: the pointer's previous value
    /// and the promotion's authority — the envelope version (the standing
    /// approval) or the approval token's `approved_by` for an
    /// out-of-envelope promotion.
    CandidatePromoted,

    /// A promoted candidate was rolled back (R0.8 wave 3): the surface's
    /// active-version pointer re-pointed to the previous candidate.
    /// Byte-exact because candidates are content-addressed and immutable —
    /// the restored version is the one that previously served, not a
    /// reconstruction. An [`Effect::Idempotent`] effect under the derived
    /// key `rollback:{surface}:{candidate_id}` (see
    /// [`crate::learn::rollback_effect_key`]). Output carries the
    /// [`crate::learn::RollbackReceipt`]: from, to, and the causing
    /// evidence. New runs bind the re-pointed version at admission;
    /// in-flight runs keep the version their checkpoint header pins.
    CandidateRolledBack,

    /// An executor policy decision was made and recorded (R0.8 wave 4): the
    /// policy plane emitted a [`DecisionEvent`] at a decision point — v1
    /// wires the retry classifier (`crate::durable::classify_retry`). An
    /// [`Effect::Pure`] record: the decision was already applied by the
    /// scheduler that emitted it; this event is the *evidence* of why, not a
    /// command to apply it again. Output carries the journaled
    /// [`DecisionEvent`] — features, the closed legal-action set, the
    /// selected action, the propensity, and the policy version that decided.
    PolicyDecision,

    /// A run manifest's capsule version pin was resolved to a content
    /// address (R0.9 Rusty Capsules, wave 1): the server's capsule registry
    /// mapped `(identity, version)` →
    /// [`crate::capsule::CapsuleId`] at admission and journaled the
    /// resolution, so the full chain — header pin → journaled resolution →
    /// receipt — reaches the manifest digest. An [`Effect::ReadOnly`]
    /// record (a registry lookup): output carries the journaled
    /// [`crate::capsule::CapsuleResolution`] — the pin name, the version
    /// string, the resolved capsule id, and the build digest the registry
    /// holds for it. A pin that resolves to a manifest failing its own
    /// content address fails admission instead — tampering is an admission
    /// error, never a journaled resolution.
    CapsuleResolved,

    /// A granted capsule capability was exercised (R0.9 wave 1): the guest
    /// called a linked import and the host performed (or attempted) the
    /// operation. The capsule rule's "every use is journaled" half: output
    /// carries the journaled [`crate::capsule::CapsuleUse`] — the capsule
    /// id, the capability kind, the operation, and the request/response
    /// summaries. The effect class is the operation's own (a `GET` fetch
    /// records [`Effect::ReadOnly`]; a writing method records
    /// [`Effect::NonIdempotent`]), and a failed operation is journaled with
    /// [`EventStatus::Error`] — a granted call that fails is still a use.
    CapsuleCall,

    /// A capsule capability attempt was refused (R0.9 wave 1): either the
    /// guest probed an import its manifest's grants never linked (the
    /// structural denial — the import does not exist), or it called a
    /// granted import outside the grant's scope (a `network` grant naming
    /// host A, a fetch attempted against host B). An [`Effect::Pure`]
    /// record: nothing executed, so there is no external effect to
    /// classify — the event is the evidence that nothing happened. Output
    /// carries the journaled [`crate::capsule::CapsuleDenial`] — the
    /// capsule id, the requested capability, and **the manifest grant that
    /// was absent** (the grant that would have permitted the attempt), so
    /// the denial is attributable to a declaration, not to a stack trace.
    CapsuleDenied,

    /// The deployment's receipt signing key changed (R0.9 wave 3): a
    /// rotation was performed (or a host with no local secret joined a
    /// shared store) and the server journaled the new key id into the
    /// deployment's receipts journal — the run id
    /// `receipt-keys`, not any tenant's run. An [`Effect::Pure`] record:
    /// the key operation is local key material, so there is no external
    /// effect to classify — the event is the lineage evidence. Output
    /// carries the journaled [`crate::receipt::SigningKeyRotation`] — the
    /// previous key id (`None` on genesis), the new key id, its public
    /// half, and the rotation instant — so "which key signed what, from
    /// when" is a chained fact, and old receipts keep verifying against
    /// the key history the journal attests.
    SigningKeyRotated,

    /// A registry artifact was resolved into a run at admission (R0.11
    /// Extension Plane, wave 2): the run declared named configuration
    /// artifacts at submission, and each one resolved through its
    /// environment-tagged version pointer to the candidate the run then
    /// pinned. An [`Effect::ReadOnly`] record (a registry lookup), the
    /// [`RunEventKind::CapsuleResolved`] precedent applied to configuration. Output
    /// carries the journaled [`crate::registry::ConfigResolution`] — the
    /// artifact, the environment tag, the candidate id, the pointer slot
    /// that admitted it (active or canary), and the digest the manifest
    /// pins. This is the digest ↔ version join the manifest alone cannot
    /// express: the manifest's wire shape stays frozen (digests only),
    /// and "which candidate produced this pin" reads from here — the
    /// event sits in the signature-covered journal, so the audit walk
    /// signed receipt → manifest pin → resolution event → candidate →
    /// author is covered end to end.
    ConfigResolved,

    /// A broker connection was registered (R0.11 Extension Plane, wave
    /// 3): a named, tenant-scoped binding of a provider account to its
    /// consent scope set landed in the broker. Journaled into the
    /// deployment's broker evidence chain (the `receipt-keys` precedent
    /// applied to a second control plane). An [`Effect::Pure`] record:
    /// registration is local broker state, so the event is the lineage
    /// evidence. Output carries the
    /// [`crate::broker::ConnectionRecord`] — metadata by construction;
    /// the credential bytes live only in the sealed store envelope and
    /// can never appear here.
    ConnectionRegistered,

    /// A connection's consent act was recorded (R0.11 wave 3): the
    /// human's grant at the provider, written down as the connection's
    /// new scope ceiling. An [`Effect::Pure`] record. Output carries the
    /// journaled [`crate::broker::ConnectionConsent`] — the connection,
    /// the subject, and the recorded scope set. Scope widening is only
    /// ever this event: a new consent act, journaled; there is no silent
    /// widening path.
    ConnectionConsented,

    /// A connection's token material rotated beneath an unchanged
    /// consent set (R0.11 wave 3): a recorded credential rotation. An
    /// [`Effect::Pure`] record. Output carries the journaled
    /// [`crate::broker::ConnectionRefresh`] — the connection and the new
    /// expiry, never the bytes. Rotation changes nothing a run pinned:
    /// the pin names the connection id and the consent set, not the
    /// secret of the moment.
    ConnectionRefreshed,

    /// A connection was revoked (R0.11 wave 3). The status flip and this
    /// event commit together, and resolution reads live connection
    /// state, so revocation takes effect at the next tool call — not the
    /// next deploy. An [`Effect::Pure`] record. Output carries the
    /// journaled [`crate::broker::ConnectionRevocation`]: the connection
    /// and the grant that stopped holding.
    ConnectionRevoked,

    /// A credential handle was issued (R0.11 wave 3): a tool (or
    /// capsule) declaring a credential need received a short-lived,
    /// opaque handle — scope-narrowed against the consent ceiling at
    /// issuance. An [`Effect::Pure`] record. Output carries the
    /// journaled [`crate::broker::HandleIssuance`] — the full claims, so
    /// the run's evidence pins the connection id and the consent scope
    /// set it resolved.
    CredentialHandleIssued,

    /// A credential handle was resolved at use (R0.11 wave 3): the
    /// broker checked live connection state, expiry, and scope coverage,
    /// and handed the credential to the host-side connector. An
    /// [`Effect::ReadOnly`] record (the resolution itself performs no
    /// external effect — the authenticated call it enables is the
    /// connector's own evidence). Output carries the journaled
    /// [`crate::broker::CredentialUse`] — handle, connection, scopes
    /// checked — never bytes (the `CapsuleCall` precedent).
    CredentialUse,

    /// A handle issuance or resolution was refused (R0.11 wave 3):
    /// revoked connection, expired handle, scope beyond the bound set,
    /// `needs_reauth`, unknown handle or connection, or a broker that
    /// could not perform the check. An [`Effect::Pure`] record: nothing
    /// executed, so there is no external effect to classify — the event
    /// is the evidence that nothing happened (the `CapsuleDenied`
    /// precedent). Output carries the journaled
    /// [`crate::broker::BrokerDenial`], attributable to the connection
    /// and the grant — never the bytes.
    CredentialDenied,

    /// A connection's refresh path failed terminally and its status
    /// flipped to `needs_reauth` (R0.11 wave 4): the provider's
    /// `invalid_grant` (or an expired refresh token) means a human must
    /// record a new consent act before the connection serves again. The
    /// flip and this event commit together, and calls fail closed with a
    /// typed re-auth signal from the next use — never silent retries with
    /// stale material, because a stale credential retried looks exactly
    /// like an attack retried. An [`Effect::Pure`] record. Output carries
    /// the journaled [`crate::broker::ConnectionReauthRequired`] — the
    /// connection, the classified failure that decided the flip, and the
    /// consent set that stopped being servable.
    ConnectionNeedsReauth,

    /// A run artifact was committed (R0.12 Operations Plane, wave 1): an
    /// output the run produced — a generated file, image, audio, an
    /// exported dataset — became a content-addressed, retainable object
    /// in the artifact plane, committed from an SDK-declared output or
    /// from a journaled [`PayloadRef::Artifact`] the producing node
    /// opted into. An [`Effect::Pure`] record: the event *is* the
    /// commitment, so the signed receipt's head covers it transitively —
    /// the audit walk is signed receipt → journal head → this event →
    /// the producing [`crate::effects::EffectId`] → the effect's
    /// journaled record → the bytes behind the address. Output carries
    /// the journaled [`crate::artifact::ArtifactCommitment`] — the
    /// content address, the name and version index when named, the media
    /// kind, the byte count, the producing effect id, and the declared
    /// retention. The bytes never enter the journal; the journal carries
    /// the reference and the commitment, and the plane carries the rest.
    ArtifactCommitted,

    /// The retention sweeper pruned an artifact's bytes (R0.12
    /// Operations Plane, wave 2): every record naming the address was
    /// expired or released, and no verified signed receipt still covered
    /// it. Journaled onto the deployment's artifact evidence chain (the
    /// run id `run-artifacts`, not any tenant's run — the
    /// `receipt-keys` / `credential-broker` precedent) *before* the
    /// bytes are deleted, so a crash mid-sweep leaves the intention
    /// auditable and the bytes recoverable. An [`Effect::Pure`] record:
    /// the event is the evidence of the enforcement act. Output carries
    /// the journaled [`crate::artifact::ArtifactPrune`] — the address,
    /// the name when named, the cause, and the sweep instant.
    ArtifactPruned,

    /// An operator released an artifact's retention pin (R0.12 wave 2):
    /// the explicit, attributed act that is the *only* path pruning an
    /// address a live signed receipt covers or a `pinned` policy holds.
    /// Shortening evidence retention is a governance decision with a
    /// name on it, never a sweeper optimization, so the act journals
    /// onto the deployment's artifact evidence chain before any byte
    /// moves. An [`Effect::Pure`] record. Output carries the journaled
    /// [`crate::artifact::ArtifactRelease`] — the address, the tenant
    /// and name, the operator identity, the optional reason, and the
    /// release instant.
    ArtifactRetentionReleased,

    /// A read found a live artifact record whose bytes are gone (R0.12
    /// wave 2): the typed miss (`410 artifact_unavailable`) a retention
    /// audit reads as the difference between "no such artifact" and "the
    /// record exists, the bytes do not" — and the shape an exact replay
    /// against pruned bytes fails closed with. Journaled onto the
    /// deployment's artifact evidence chain (best-effort; the read's
    /// typed answer is the contract, the event is the evidence) so the
    /// miss is attributable without rewriting the producing run's
    /// receipt-covered journal. An [`Effect::Pure`] record. Output
    /// carries the journaled [`crate::artifact::ArtifactUnavailability`]
    /// — the address, the tenant and name, the surface that missed, and
    /// the observation instant.
    ArtifactUnavailable,

    /// A run was admitted under a deployment revision (R0.12 Operations
    /// Plane, wave 3): the run declared an environment at submission, and
    /// the environment's deployment pointer bound a revision for it (the
    /// full-traffic `active`, or the `canary` when the seeded draw
    /// admitted). An [`Effect::ReadOnly`] record (a pointer lookup), the
    /// [`RunEventKind::ConfigResolved`] precedent lifted from
    /// configuration to deployments. Output carries the journaled
    /// [`crate::deploy::DeploymentResolved`] — the environment, the bound
    /// `revision_id`, the pointer slot, and the pin-set digest. This is
    /// the audit walk's hinge: signed receipt → journal head → this event
    /// → the content-addressed revision → the frozen pin set → the
    /// candidates and their authors, every hop signature-covered. A
    /// pointer serving nothing binds nothing — the run fails admission
    /// instead (there is no implicit "latest"), so the journaled event
    /// only ever names a revision that served.
    DeploymentResolved,

    /// A deployment revision was registered (R0.12 wave 3): an immutable,
    /// content-addressed declaration of what may serve landed in the
    /// control plane. Journaled onto the deployment evidence chain (the
    /// run id `deployment-control` — the `receipt-keys` /
    /// `credential-broker` precedent for a third control plane), never a
    /// run's journal: a deployment transition is not any run's event.
    /// An [`Effect::Pure`] record: registration is local control-plane
    /// state, so the event is the lineage evidence. Output carries the
    /// journaled [`crate::deploy::RevisionRegistration`] — the tenant and
    /// the revision, whose own fields carry the author and the frozen
    /// pin set.
    RevisionRegistered,

    /// A revision was promoted into an environment (R0.12 wave 3): the
    /// environment's deployment pointer moved `active` and cleared any
    /// canary — a full promotion supersedes the experiment it graduated
    /// from. The journaled transition and the pointer move commit in one
    /// transaction (the learn store's rule: a crash cannot leave a
    /// promoted revision whose pointer never moved). An [`Effect::Pure`]
    /// record on the deployment evidence chain. Output carries the
    /// journaled [`crate::deploy::RevisionPromotion`] — the environment,
    /// the promoted revision, the displaced `previous` (the rollback
    /// path's whole story), the author, and the instant.
    RevisionPromoted,

    /// A serving revision was rolled back (R0.12 wave 3): the
    /// environment's pointer re-pointed `active` to the previously
    /// serving revision — byte-exact, because the restored revision is
    /// the immutable record that served before, re-derived from the
    /// chain's own transition history, never a reconstruction. An
    /// [`Effect::Pure`] record on the deployment evidence chain. Output
    /// carries the journaled [`crate::deploy::RevisionRollback`] — the
    /// environment, from, to, the cause, and the author. New runs bind
    /// the re-pointed revision at admission; in-flight runs keep the
    /// revision their journaled resolution names.
    RevisionRolledBack,

    /// An environment was declared (R0.12 wave 3): an R0.11 promotion
    /// tag became a first-class record — the name, the gate and approval
    /// declarations that will govern promotions into it (wired in
    /// wave 4), and the creation metadata. An [`Effect::Pure`] record on
    /// the deployment evidence chain, so an audit reads the declaration
    /// in force when a promotion happened, not a later edit. Output
    /// carries the journaled [`crate::deploy::EnvironmentDeclaration`].
    EnvironmentDeclared,

    /// An environment-scoped secret was set or rotated (R0.12 wave 3):
    /// custody, not brokerage — a static value envelope-encrypted at
    /// rest under the deployment master key, stored as ciphertext on
    /// both backends, rotated by replacement beneath the stable scoped
    /// name. An [`Effect::Pure`] record on the deployment evidence
    /// chain, metadata by construction: output carries the journaled
    /// [`crate::deploy::EnvSecretAct`] — the record, never the bytes;
    /// `rotated_at` marks a rotation beneath the stable name.
    EnvSecretSet,

    /// An environment-scoped secret was revoked by deletion (R0.12
    /// wave 3): the only revocation path — there is no disable flag to
    /// forget, and the tombstone is the evidence the scope once held a
    /// value. An [`Effect::Pure`] record on the deployment evidence
    /// chain. Output carries the journaled
    /// [`crate::deploy::EnvSecretRevocation`] — the scoped name, the
    /// revoker, and the instant.
    EnvSecretRevoked,

    /// An environment-secret resolution was refused on scope (R0.12
    /// wave 3): the request asked for a scope outside the environment
    /// the requester holds — the `CapsuleDenied` discipline (attributable
    /// to a declaration, not a stack trace) applied to environment scope.
    /// An [`Effect::Pure`] record: nothing resolved, so there is no
    /// external effect to classify — the event is the evidence that
    /// nothing happened. Output carries the journaled
    /// [`crate::deploy::EnvSecretDenial`], naming the scope requested
    /// and the scope the holder holds.
    EnvSecretDenied,

    /// A release gate evaluated a promotion into a gated environment
    /// (R0.12 wave 4): the decision journals **before** the pointer move
    /// it governs, allowed or refused — a gate whose refusals leave no
    /// evidence is a gate an audit cannot distinguish from never having
    /// run. An [`Effect::Pure`] record on the deployment evidence chain.
    /// Output carries the journaled [`crate::deploy::GateDecisionRecord`]:
    /// policy, dataset version, every check observed, and the verdict.
    GateDecisionRecorded,

    /// A canary was declared on an environment's pointer (R0.12 wave 4):
    /// one revision bound to a declared fraction of new runs while the
    /// active revision serves the rest. An [`Effect::Pure`] record on the
    /// deployment evidence chain. Output carries the journaled
    /// [`crate::deploy::CanaryDeclaration`] — the binding and its author,
    /// so the seeded draw an audit replays has a declared origin.
    CanaryDeclared,

    /// A canary was cleared from an environment's pointer (R0.12 wave
    /// 4): the experiment ended without graduating (graduation is a
    /// promotion, which clears the slot itself). An [`Effect::Pure`]
    /// record on the deployment evidence chain. Output carries the
    /// journaled [`crate::deploy::CanaryClearance`], naming the revision
    /// the slot held — the clearance is evidence about a specific
    /// binding, not a bare "slot emptied".
    CanaryCleared,

    /// A shadow run started against a recorded source run (R0.12 wave
    /// 4): the twin of the source under a candidate revision, executed
    /// against the shadow admission boundary. An [`Effect::Pure`] record
    /// in the shadow run's own journal. Output carries the journaled
    /// [`crate::deploy::ShadowRunStarted`] — naming the source run and
    /// pinning `role: shadow`, so the twin-pair discipline is data the
    /// journal carries, not a comment.
    ShadowRunStarted,

    /// A shadow run's admission boundary refused an effect above
    /// read-only (R0.12 wave 4): the refusal is the shadow's reason to
    /// exist — proof the candidate cannot touch the world — so it
    /// journals served-or-not with the request it refused. An
    /// [`Effect::Pure`] record in the shadow run's journal: nothing
    /// executed, so there is no external effect to classify. Output
    /// carries the journaled [`crate::effects::ShadowRefusal`].
    ShadowEffectRefused,

    /// A shadow run completed and its verdict journaled (R0.12 wave 4):
    /// refusals, how many were served from the recorded world, and which
    /// recorded calls the candidate never requested — the divergence
    /// evidence. An [`Effect::Pure`] record in the shadow run's journal.
    /// Output carries the journaled [`crate::deploy::ShadowVerdict`].
    ShadowVerdict,

    /// The run declared its configuration envelope at start (evidence and
    /// admission wave): the graph version and topology hash, the active
    /// policy version, the tool allowlist, and the pinned [`RunManifest`] —
    /// the config half of the model-call envelope, so a journaled run's
    /// requests are a pure function of the log. Journaled once as the run's
    /// first event, and only when the run declares something beyond the
    /// static floor — absent means undeclared, never a default (the
    /// [`RunManifest`] discipline applied to the whole envelope), so
    /// journals recorded before this variant keep replaying
    /// byte-identically. An [`Effect::Pure`] record: the declaration changes
    /// nothing, it *is* the fact. Output carries the journaled
    /// [`RunConfigDeclaration`]. Exact replay re-derives the declaration
    /// from the recorded event and asserts field-level agreement, naming
    /// the diverging field when they differ.
    RunConfigDeclared,

    /// A run-registered tool guard denied a dispatch (evidence and
    /// admission wave): the guard layer evaluated the finalized call —
    /// after allowlist admission, before the effect boundary, so a denial
    /// never burns a one-shot approval — and at least one guard returned a
    /// [`crate::tool::GuardDenial`]. Guards are deny-only
    /// ([`crate::tool::ToolGuard::check`] returns `Option<GuardDenial>`;
    /// there is no allow result), every guard is evaluated, and any denial
    /// blocks the dispatch — no ordering of guards can turn a denial into
    /// permission. An [`Effect::Pure`] record: nothing executed, so there
    /// is no external effect to classify — the event is the evidence that
    /// nothing happened (the [`RunEventKind::CapsuleDenied`] precedent).
    /// Input carries the canonical [`crate::replay::tool_call_request`]
    /// shape; output carries the journaled [`crate::tool::GuardDenialRecord`]
    /// — every denying guard named, so the denial is attributable to a
    /// declaration, not a stack trace. Exact replay re-derives the denial
    /// by re-running the same registered guards; the event is never served.
    ToolCallDenied,

    /// An approval gate asked for a decision (evidence and admission wave):
    /// the asked half of the closed approval pair, journaled **before** the
    /// decision resolves — a gate whose asks leave no evidence is a gate an
    /// audit cannot distinguish from never having run (the
    /// [`RunEventKind::GateDecisionRecorded`] principle). An
    /// [`Effect::Pure`] record. Input carries the journaled
    /// [`ApprovalRequest`] — the asking surface and, when occurrence-scoped,
    /// the effect id. The event's causal parent anchors the pair inside the
    /// open turn (the node-input event id, the same discipline
    /// [`crate::react`] parents model and tool effects with).
    ApprovalAsked,

    /// An approval gate's decision landed (evidence and admission wave):
    /// the decided half of the pair, parented to its `ApprovalAsked` event.
    /// An [`Effect::Pure`] record. Output carries the journaled
    /// [`ApprovalOutcome`] — the closed [`ApprovalDecision`] vocabulary, in
    /// which only `approved_once` grants and every other outcome denies.
    /// Exact replay re-journals the pair from the record when it is nested
    /// inside a served effect (the gated call never re-executes), and
    /// refuses journals whose gate evidence is not so nested — see
    /// [`crate::replay`].
    ApprovalDecided,

    /// A message was settled into the run's durable inbox (R0.13 parity
    /// wave): a send the executor observed at a settlement point — a
    /// super-step boundary, the turn-end check, the cancellation check —
    /// entered its queue. An [`Effect::Pure`] record: the intake is
    /// run-control evidence, not an external effect. Output carries the
    /// journaled [`crate::inbox::InboxMessage`] — the inbox's monotonic
    /// intake sequence, the queue kind, the sender provenance, and the
    /// content. Journaled at settlement rather than at send time so the
    /// journal — not wall-clock arrival — fixes each message's position in
    /// the run's evidence, which is what makes exact replay of inbox-driven
    /// runs possible ([`crate::inbox::Inbox::replaying`]).
    InboxIntake,

    /// A batch of messages left the run's durable inbox into the execution
    /// (R0.13 parity wave): steering drained at a super-step boundary,
    /// staged injections riding a wake, or follow-ups consumed at the
    /// turn-end check to extend the run into another turn. An
    /// [`Effect::Pure`] record. Output carries the journaled
    /// [`crate::inbox::InboxConsumption`] — the
    /// [`crate::inbox::ConsumptionPoint`], the super-step, and the exact
    /// batch in intake order.
    InboxConsumed,

    /// The run was cancelled through its durable inbox with a typed cause
    /// (R0.13 parity wave): [`crate::inbox::Inbox::cancel`] latched the
    /// request and the executor observed it at a super-step boundary — the
    /// same transactional granularity as the cooperative-cancellation token,
    /// extended with the *who*. An [`Effect::Pure`] record: cancellation is
    /// control flow, never a failure. Output carries the journaled
    /// [`crate::inbox::RunCancellation`] — the closed
    /// [`crate::inbox::CancelCause`], the `keep_inbox` disposition, and what
    /// a dropping cancellation discarded.
    /// One incremental chunk of a streaming assistant response (EP-01-S11).
    /// Input is empty; output carries the [`AssistantChunk`] — the delta,
    /// the monotonic `stream_index`, and the finish flag. Journaled before
    /// the assembled [`ModelCall`] so chunks are durable-first and replay
    /// can reconstruct the exact token sequence that originally rendered.
    AssistantChunk,
    RunCancelled,
}

/// One recorded fact about a run: the Flight Recorder's atomic evidence.
///
/// Events form a causal chain via `parent` (the event that caused this one —
/// e.g. a node input's parent is its super-step start), and a total order
/// via `seq`, a monotonic sequence number assigned by the journal at record
/// time. `seq` — not wall time — is the ordering guarantee: recorded runs
/// re-driven against a seeded clock reproduce the same sequence.
///
/// Event ids are `{run_id}:{seq}` — deterministic for a given journal, so a
/// re-driven run with the same seed mints the same ids.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RunEvent {
    /// Deterministic event id (`{run_id}:{seq}`).
    pub id: String,

    /// The run this event belongs to. One `Executor::run` call = one run id.
    pub run_id: String,

    /// The thread (session) the run belongs to.
    pub thread_id: String,

    /// The node this event is about, where applicable (`None` for run-wide
    /// events such as super-step boundaries and checkpoint writes).
    pub node_id: Option<String>,

    /// Monotonic sequence number within the journal, assigned at record
    /// time. The total order of the run's evidence.
    pub seq: u64,

    /// What happened.
    pub kind: RunEventKind,

    /// The declared effect classification of whatever produced this event.
    pub effect: Effect,

    /// Input payload (arguments, request, snapshot), inline or referenced.
    pub input: Option<PayloadRef>,

    /// Output payload (result, response, updates), inline or referenced.
    pub output: Option<PayloadRef>,

    /// Wall/logical latency of the recorded operation in milliseconds, when
    /// measured. Sourced from the run's clock, so a logical clock yields
    /// reproducible values.
    pub latency_ms: Option<u64>,

    /// Token usage for model calls, when the provider reported it.
    pub tokens: Option<Usage>,

    /// Monetary cost in USD for the recorded operation, when known. `f64`
    /// micro-costs are fine here: this is evidence, not accounting — the
    /// ledger aggregates elsewhere.
    pub cost_usd: Option<f64>,

    /// How the event ended.
    pub status: EventStatus,

    /// The id of the event that caused this one, when there is one. A node
    /// input's parent is its super-step start; a tool call's parent is the
    /// node that invoked it; a checkpoint write's parent is the routing
    /// decision that ended the step.
    pub parent: Option<String>,

    /// When the event was recorded, read from the run's clock (system wall
    /// clock by default; the configured logical clock for seeded runs).
    pub recorded_at: DateTime<Utc>,
}

/// The family a [`DecisionEvent`] belongs to: the closed set of executor
/// decisions a policy may learn.
///
/// The set mirrors the R0.10 priority order. Deliberately absent: model and
/// agent selection (a governed semantic policy, not an automatic one) and
/// interrupt policy (the prevented-error counterfactual is unobservable).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DecisionFamily {
    /// Whether to re-attempt a failed effect, and with what backoff.
    Retry,
    /// What timeout/stopping bound to apply to an operation.
    Timeout,
    /// Which equivalent worker a remote execution is placed on.
    WorkerPlacement,
    /// Concurrency/backpressure limits for parallel execution.
    Concurrency,
    /// Whether a checkpoint is written at a given boundary (headroom gated
    /// on the R0.5 experiment: mandatory after non-idempotent effects).
    CheckpointPlacement,
}

/// One action in a [`DecisionEvent`]'s legal set. Closed enum: learned
/// policies choose among declared actions, never free-form outputs — that is
/// what keeps the learning problem mechanical (dense signals, closed spaces)
/// instead of semantic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum DecisionAction {
    /// Re-attempt the failed operation; `attempt` is the 1-based retry
    /// ordinal.
    Retry {
        /// The 1-based retry ordinal being taken.
        attempt: u32,
    },
    /// Give up on the operation and fail the run/step.
    Abort,
    /// Apply a timeout of `millis` to the operation.
    SetTimeout {
        /// The timeout bound in milliseconds.
        millis: u64,
    },
    /// Place a remote execution on worker `worker`.
    SelectWorker {
        /// The chosen worker's identity.
        worker: String,
    },
    /// Cap concurrent executions at `limit`.
    SetConcurrency {
        /// The maximum number of concurrent executions.
        limit: u32,
    },
    /// Persist a checkpoint at this boundary.
    WriteCheckpoint,
    /// Skip the checkpoint at this boundary.
    SkipCheckpoint,
}

/// How a decided action turned out, filled in when the affected operation
/// completes. `None` on the wire until then — decisions and outcomes are
/// recorded separately so in-flight decisions are visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DecisionOutcome {
    /// The selected action led to completion.
    Success,
    /// The selected action did not lead to completion.
    Failure,
    /// The run was cancelled or superseded before the outcome materialized.
    Cancelled,
}

/// The role a [`DecisionEvent`] played in its run (R0.10 wave 2, the runtime
/// digital twin).
///
/// Additive to the R0.8 contract: absent from the wire when `None`, so every
/// pre-twin decision — all of which were made by the policy whose action
/// executed — keeps its exact shape. The twin is the first emitter that
/// records *two* decisions at one decision point (the shadow pair), which is
/// what makes the marker necessary: without it, off-policy evaluation cannot
/// tell the decision that bound the world from the one that merely scored
/// the same features.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DecisionRole {
    /// The decision whose selected action executed (the floor's, in a twin
    /// shadow run).
    Acting,
    /// A candidate policy's decision over the same features, journaled for
    /// off-policy evidence but never executed. Well-posed evidence requires
    /// the shadow's true propensity; a shadow exploring by seeded draw is a
    /// stochastic policy with known propensities by construction.
    Shadow,
}

/// One executor policy decision with everything offline learning needs to
/// evaluate it.
///
/// The learning contract (R0.8+): given `features` and `legal_actions`, the
/// policy named by `policy_version` chose `selected` with probability
/// `propensity`. Propensity is assigned **at decision time**, never
/// reconstructed — without it, off-policy evaluation (comparing a candidate
/// policy against the recorded one) is impossible. `outcome` is `None`
/// until the affected operation completes.
///
/// v1 froze this contract without emitters; R0.8 wave 4 wires the first
/// emission point (the retry classifier, see
/// [`crate::durable::retry_decision_event`]), journaled as
/// [`RunEventKind::PolicyDecision`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionEvent {
    /// Deterministic decision id (`{run_id}:d{n}` — a separate sequence from
    /// [`RunEvent`], so decision ids stay stable if event kinds are added).
    pub id: String,

    /// The run the decision was made in.
    pub run_id: String,

    /// The thread (session) the run belongs to.
    pub thread_id: String,

    /// Sequence number within the decision stream of this run.
    pub seq: u64,

    /// Which executor decision this is.
    pub family: DecisionFamily,

    /// The observation the policy decided from (latency percentiles, failure
    /// class, queue depth, ...). Free-form JSON: the feature schema evolves
    /// with the policy, but the envelope does not.
    pub features: Map<String, Value>,

    /// Every action that was legal at decision time. Off-policy evaluation
    /// needs the full set, not just the chosen one.
    pub legal_actions: Vec<DecisionAction>,

    /// The action the policy took. Must be a member of `legal_actions`
    /// (enforced by the policy plane, not by this type).
    pub selected: DecisionAction,

    /// The probability the active policy assigned to `selected` at decision
    /// time, in `(0, 1]`. First-class because learning correctness depends
    /// on it: importance weighting divides by the propensity.
    pub propensity: f64,

    /// The policy that made the decision.
    pub policy_version: PolicyVersion,

    /// Whether this decision's action executed or only scored the features
    /// (R0.10 wave 2 shadow pairs; see [`DecisionRole`]). `None` — every
    /// decision recorded before the twin — means acting: the only decisions
    /// that existed were the ones that bound the world.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<DecisionRole>,

    /// The result of the decision, `None` until completion.
    pub outcome: Option<DecisionOutcome>,

    /// When the decision was made, read from the run's clock.
    pub decided_at: DateTime<Utc>,
}

/// The envelope any learned backoff cap must stay within, in milliseconds
/// (one hour). The floor's own cap is [`crate::durable::MAX_RETRY_DELAY_MS`];
/// a learned policy may widen the schedule, but never past the envelope —
/// beyond it a stuck dependency parks work for longer than any operator
/// would accept discovering by surprise.
pub const POLICY_MAX_DELAY_ENVELOPE_MS: u64 = 3_600_000;

/// The envelope any learned attempt budget must stay within. Ten attempts
/// is already generous past the floor's three; beyond the envelope a
/// policy is re-trying as a substitute for fixing the cause.
pub const POLICY_MAX_ATTEMPTS_ENVELOPE: u32 = 10;

/// The minimum timeout bound any policy may impose, in milliseconds.
/// Below it ordinary work aborts early — a correctness hazard no policy
/// may cross (the same floor the Wave 1 headroom experiment
/// pre-registered). The twin's ladder shares this constant
/// ([`crate::twin::MIN_TIMEOUT_RUNG_MS`] re-exports it) so the evaluation
/// harness and the production contract enforce one bound.
pub const MIN_TIMEOUT_RUNG_MS: u64 = 100;

/// One backoff schedule: the numbers a retry decision is made with.
///
/// This is the shape both the flat policy-wide schedule and each per-class
/// override take (R0.10 wave 3): the backoff draws from
/// `[0, base_delay_ms * 2^(attempt-1)]` capped at `max_delay_ms`, and a
/// retryable failure dead-letters once `attempt >= max_attempts`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackoffParameters {
    /// Base of the exponential backoff schedule, in milliseconds.
    pub base_delay_ms: u64,

    /// Cap of the backoff schedule, in milliseconds.
    pub max_delay_ms: u64,

    /// Attempt budget: the number of attempts counting the initial one.
    /// `0` means no retries at all — every retryable failure dead-letters.
    pub max_attempts: u32,
}

impl BackoffParameters {
    /// The contract every backoff schedule — flat or per-class — must
    /// satisfy: a positive base no larger than the cap, a cap inside the
    /// declared envelope, and a budget inside the declared envelope.
    /// Anything else is rejected (`Err`), so an out-of-envelope parameter
    /// set can never become an active policy — the gate fails closed.
    pub fn validate(&self) -> crate::error::Result<()> {
        let invalid = |message: String| RustyError::InvalidUpdate(message);
        if self.base_delay_ms == 0 {
            return Err(invalid(
                "backoff base_delay_ms must be positive — a zero base retries immediately, \
                 a retry storm by another name"
                    .to_owned(),
            ));
        }
        if self.base_delay_ms > self.max_delay_ms {
            return Err(invalid(format!(
                "backoff base_delay_ms {} exceeds max_delay_ms {} — the schedule would clamp \
                 to the cap from the first retry, an exponential in name only",
                self.base_delay_ms, self.max_delay_ms
            )));
        }
        if self.max_delay_ms > POLICY_MAX_DELAY_ENVELOPE_MS {
            return Err(invalid(format!(
                "backoff max_delay_ms {} exceeds the {POLICY_MAX_DELAY_ENVELOPE_MS} ms \
                 envelope",
                self.max_delay_ms
            )));
        }
        if self.max_attempts > POLICY_MAX_ATTEMPTS_ENVELOPE {
            return Err(invalid(format!(
                "backoff max_attempts {} exceeds the {POLICY_MAX_ATTEMPTS_ENVELOPE}-attempt \
                 envelope",
                self.max_attempts
            )));
        }
        Ok(())
    }
}

/// The retry family's parameters of an [`ExecutorPolicy`].
///
/// The flat schedule is the default every class falls back to. `per_class`
/// (R0.10 wave 3) carries learned per-[`crate::durable::ErrorClass`]
/// overrides: a class with an entry decides by that schedule (its budget
/// narrowing the task's declared budget, never widening it); a class
/// without one decides by the flat schedule. Absent from the wire when
/// `None`, so the floor's serialized shape is unchanged from v1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryPolicyParameters {
    /// Base of the exponential backoff schedule, in milliseconds.
    pub base_delay_ms: u64,

    /// Cap of the backoff schedule, in milliseconds.
    pub max_delay_ms: u64,

    /// Attempt budget: the number of attempts counting the initial one.
    /// `0` means no retries at all — every retryable failure dead-letters.
    pub max_attempts: u32,

    /// Learned per-error-class schedules (R0.10 wave 3). `None` — every
    /// pre-learning policy — means the flat schedule decides for all
    /// classes, exactly the v1 behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_class: Option<BTreeMap<crate::durable::ErrorClass, BackoffParameters>>,
}

impl RetryPolicyParameters {
    /// The flat schedule as [`BackoffParameters`].
    pub fn flat(&self) -> BackoffParameters {
        BackoffParameters {
            base_delay_ms: self.base_delay_ms,
            max_delay_ms: self.max_delay_ms,
            max_attempts: self.max_attempts,
        }
    }

    /// The family's contract: the flat schedule validates, and every
    /// per-class entry validates. One bad entry condemns the whole set —
    /// there is no "apply the valid half" path, because half-applied
    /// policy is how a deployment drifts from what was evaluated.
    pub fn validate(&self) -> crate::error::Result<()> {
        self.flat().validate()?;
        if let Some(per_class) = &self.per_class {
            for (class, schedule) in per_class {
                schedule.validate().map_err(|e| {
                    RustyError::InvalidUpdate(format!("per-class backoff for {class:?}: {e}"))
                })?;
            }
        }
        Ok(())
    }
}

/// The timeout family's parameters of an [`ExecutorPolicy`].
///
/// `None` means uncapped — the honest encoding of "no timeout policy
/// is in force", distinct from any concrete millisecond bound.
/// `per_callee` (R0.10 wave 3) carries learned per-callee bounds: the
/// bound that applies to an operation is its callee's entry when one
/// exists, the default otherwise, and no bound at all under the floor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeoutPolicyParameters {
    /// Default timeout applied to operations without their own bound, in
    /// milliseconds. `None` means no default is imposed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_millis: Option<u64>,

    /// Hard ceiling any operation timeout must stay under, in milliseconds.
    /// `None` means uncapped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_millis: Option<u64>,

    /// Learned per-callee bounds in milliseconds (R0.10 wave 3), keyed by
    /// the callee identity the decision point journals (a tool name, a
    /// node id). `None` — every pre-learning policy — means no per-callee
    /// bounds, exactly the v1 behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_callee: Option<BTreeMap<String, u64>>,
}

impl TimeoutPolicyParameters {
    /// The family's contract: every bound respects the minimum rung (below
    /// it ordinary work aborts early — a correctness hazard), the default
    /// stays under the ceiling, and every per-callee entry stays under the
    /// ceiling when one is declared. One bad bound condemns the whole set.
    pub fn validate(&self) -> crate::error::Result<()> {
        let invalid = |message: String| RustyError::InvalidUpdate(message);
        let check_floor = |what: &str, millis: u64| -> crate::error::Result<()> {
            if millis < MIN_TIMEOUT_RUNG_MS {
                return Err(invalid(format!(
                    "timeout {what} {millis} ms is below the {MIN_TIMEOUT_RUNG_MS} ms minimum \
                     rung — below it ordinary work aborts early, a correctness hazard no \
                     policy may cross"
                )));
            }
            Ok(())
        };
        if let Some(default) = self.default_millis {
            check_floor("default_millis", default)?;
        }
        if let Some(max) = self.max_millis {
            check_floor("max_millis", max)?;
        }
        if let (Some(default), Some(max)) = (self.default_millis, self.max_millis) {
            if default > max {
                return Err(invalid(format!(
                    "timeout default_millis {default} exceeds max_millis {max} — the default \
                     bound would sit above its own ceiling"
                )));
            }
        }
        if let Some(per_callee) = &self.per_callee {
            for (callee, millis) in per_callee {
                check_floor(&format!("per_callee[{callee}]"), *millis)?;
                if let Some(max) = self.max_millis {
                    if *millis > max {
                        return Err(invalid(format!(
                            "timeout per_callee[{callee}] {millis} ms exceeds max_millis \
                             {max} — a learned bound may only narrow the declared ceiling"
                        )));
                    }
                }
            }
        }
        Ok(())
    }
}

/// The concurrency family's parameters of an [`ExecutorPolicy`].
///
/// v1 pins the shape only: no executor decision point consumes these values
/// yet. `None` means unlimited — the honest encoding of "no concurrency
/// policy is in force".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConcurrencyPolicyParameters {
    /// Maximum number of parallel executions. `None` means unlimited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_parallel: Option<u32>,
}

/// The executor policy contract (R0.8 wave 4): one versioned bundle of the
/// parameters the executor's mechanical decision points agree to decide
/// with.
///
/// An `ExecutorPolicy` is what a [`PolicyVersion`] *names*: the registry
/// (server side) stores immutable policy bodies under content-derived
/// versions, runs bind the active version at admission, and every
/// [`DecisionEvent`] records which version decided. Because the version is
/// derived from the content ([`derive_policy_version`]), two registries that
/// agree on a version string agree on the exact parameters — promotion and
/// rollback move versions, never mutate bodies.
///
/// v1 wires versions into admission binding and decision evidence; R0.10
/// wave 3 adds the application loop: the retry classifier and the timeout
/// decision point read the bound version's parameters through
/// [`crate::durable::resolve_retry_parameters`] and
/// [`crate::durable::resolve_timeout_bound_ms`], with the static floor's
/// constants as the read path when the version names no override (or names
/// an invalid one — invalid parameters fail closed to the floor).
/// The static floor ([`ExecutorPolicy::static_v0`]) is the behavior every
/// pre-learning run already had: the retry constants of
/// [`crate::durable::classify_retry`], no timeout bound, no concurrency
/// limit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutorPolicy {
    /// Retry parameters — the family v1 actually emits decisions for.
    pub retry: RetryPolicyParameters,

    /// Timeout parameters (shape pinned; unconsumed in v1).
    pub timeout: TimeoutPolicyParameters,

    /// Concurrency parameters (shape pinned; unconsumed in v1).
    pub concurrency: ConcurrencyPolicyParameters,
}

impl ExecutorPolicy {
    /// The static floor: the fixed behavior of every run before the policy
    /// plane landed, named by [`PolicyVersion::STATIC_V0`].
    ///
    /// The retry parameters mirror the `crate::durable` backoff constants and
    /// the server's default attempt budget (`DEFAULT_MAX_ATTEMPTS = 3`); the
    /// timeout and concurrency families are uncapped, matching the executor's
    /// behavior when no policy is in force.
    pub fn static_v0() -> Self {
        Self {
            retry: RetryPolicyParameters {
                base_delay_ms: crate::durable::BASE_RETRY_DELAY_MS,
                max_delay_ms: crate::durable::MAX_RETRY_DELAY_MS,
                max_attempts: 3,
                per_class: None,
            },
            timeout: TimeoutPolicyParameters {
                default_millis: None,
                max_millis: None,
                per_callee: None,
            },
            concurrency: ConcurrencyPolicyParameters { max_parallel: None },
        }
    }

    /// `true` when this policy is exactly the static floor — the read path
    /// every decision point falls back to. Compared by value, not by
    /// version name: a registered body identical to the floor *is* the
    /// floor's behavior, whatever string names it.
    pub fn is_static_floor(&self) -> bool {
        self == &Self::static_v0()
    }

    /// The whole bundle's contract: every family's parameters validate.
    /// Decision points call this (or the per-family equivalents) before
    /// reading parameters into a decision — an invalid bundle steers
    /// nothing; the floor decides instead.
    pub fn validate(&self) -> crate::error::Result<()> {
        self.retry.validate()?;
        self.timeout.validate()?;
        if let Some(0) = self.concurrency.max_parallel {
            return Err(RustyError::InvalidUpdate(
                "concurrency max_parallel 0 admits no work at all — a policy that halts the \
                 fleet is not a learning outcome, it is an outage"
                    .to_owned(),
            ));
        }
        Ok(())
    }

    /// This policy with one family's parameters replaced by `parameters`
    /// (parsed as that family's parameter type).
    ///
    /// This is how a promoted policy candidate — whose content is a family
    /// plus a free-form parameter value — becomes a concrete
    /// `ExecutorPolicy`: the candidate's parameters are parsed against the
    /// family's contract and checked against the family's declared envelope
    /// (R0.10 wave 3), then overlaid onto the policy that was active (the
    /// floor when nothing was promoted yet). Out-of-envelope parameters are
    /// rejected — a malformed candidate can never become an active policy.
    /// Families without a parameter contract
    /// ([`DecisionFamily::WorkerPlacement`],
    /// [`DecisionFamily::CheckpointPlacement`]) are rejected: those families
    /// are shadow-only, and a shadow family's parameters cannot activate.
    pub fn with_family_parameters(
        &self,
        family: DecisionFamily,
        parameters: serde_json::Value,
    ) -> crate::error::Result<Self> {
        let mut policy = self.clone();
        match family {
            DecisionFamily::Retry => {
                policy.retry = serde_json::from_value(parameters).map_err(|e| {
                    RustyError::InvalidUpdate(format!(
                        "retry policy parameters do not match the contract: {e}"
                    ))
                })?;
                policy.retry.validate().map_err(|e| {
                    RustyError::InvalidUpdate(format!(
                        "retry policy parameters fall outside the envelope: {e}"
                    ))
                })?;
            }
            DecisionFamily::Timeout => {
                policy.timeout = serde_json::from_value(parameters).map_err(|e| {
                    RustyError::InvalidUpdate(format!(
                        "timeout policy parameters do not match the contract: {e}"
                    ))
                })?;
                policy.timeout.validate().map_err(|e| {
                    RustyError::InvalidUpdate(format!(
                        "timeout policy parameters fall outside the envelope: {e}"
                    ))
                })?;
            }
            DecisionFamily::Concurrency => {
                policy.concurrency = serde_json::from_value(parameters).map_err(|e| {
                    RustyError::InvalidUpdate(format!(
                        "concurrency policy parameters do not match the contract: {e}"
                    ))
                })?;
                if let Some(0) = policy.concurrency.max_parallel {
                    return Err(RustyError::InvalidUpdate(
                        "concurrency policy parameters fall outside the envelope: \
                         max_parallel 0 admits no work at all"
                            .to_owned(),
                    ));
                }
            }
            other => {
                return Err(RustyError::InvalidUpdate(format!(
                    "decision family `{other:?}` has no executor-policy parameter contract \
                     in this version"
                )));
            }
        }
        Ok(policy)
    }
}

/// Derive the content-addressed version of an [`ExecutorPolicy`]:
/// `policy-{first 12 hex of sha256(canonical_json(policy))}`.
///
/// Content addressing is what makes the registry's immutability enforceable:
/// a version string *is* a commitment to one exact parameter set, so
/// "register a different body under an existing version" is detectable as a
/// conflict rather than silently overwriting behavior. The static floor is
/// exempt — it keeps its human-readable [`PolicyVersion::STATIC_V0`] name
/// because it predates the registry and never needs registration.
pub fn derive_policy_version(policy: &ExecutorPolicy) -> crate::error::Result<PolicyVersion> {
    let bytes = serde_json::to_vec(policy)?;
    let hash = sha256_hex(&bytes);
    Ok(PolicyVersion::new(format!("policy-{}", &hash[..12])))
}

/// A capsule version pin — the R0.7 placeholder for R0.9's capsule manifest
/// identity.
///
/// R0.9 gives capsules a full manifest (build digest, declared interface,
/// capability grants); R0.7 pins only the version string so a run's manifest
/// can already record *which* capsule build influenced it. Typing it now —
/// rather than storing a bare string — localizes the R0.9 evolution to one
/// type instead of every consumer of the manifest map. Transparent over
/// `String`: the wire shape is the version string itself.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CapsuleVersion(pub String);

impl CapsuleVersion {
    /// Wrap a version string.
    pub fn new(version: impl Into<String>) -> Self {
        Self(version.into())
    }

    /// The version string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CapsuleVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The versioned run manifest (R0.7): everything beyond the graph itself
/// that can influence a run, pinned at checkpoint time.
///
/// The R0.5 header pins the checkpoint format, the graph version and
/// topology, and the executor policy — but a run's behavior is also shaped
/// by its prompts, its tools' schemas, its model and parameters, and (as
/// they land) its memory schema and capsule builds. The manifest pins all of
/// them by content address (lowercase hex SHA-256, the one hashing primitive
/// shared with artifact references and journal heads), so a checkpoint
/// answers not just "which graph produced this?" but "which *configuration
/// of everything else* produced this?".
///
/// **Upgrade safety.** Pinning is what lets long-running agents survive
/// platform upgrades: a run resumed from a checkpoint keeps executing
/// against the versions its manifest pins, while new versions shadow for new
/// runs. An absent field means *unpinned*, never a default — consumers must
/// not invent one. Migration contracts between pinned versions (when a run
/// may move from pin A to pin B, and how) are R1.0's stability work,
/// deliberately: this wave records the evidence migrations will be judged
/// against.
///
/// Every field is optional and omitted from the wire when unset
/// (`skip_serializing_if`), so a header carrying no manifest — or a manifest
/// carrying only some pins — produces no shape change for pre-R0.7 readers.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RunManifest {
    /// Prompt digests: prompt name → SHA-256 of the exact prompt text
    /// (`sha256_hex` over its UTF-8 bytes). Prompt text, not a name alone,
    /// because an edited prompt under an unchanged name is exactly the drift
    /// the manifest exists to catch.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub prompts: BTreeMap<String, String>,

    /// Tool schema digests: tool name → SHA-256 of the canonical
    /// `serde_json` serialization of its parameters schema (the same
    /// canonicalization [`PayloadRef::content_hash`] relies on: object keys
    /// sort deterministically, so equal schemas hash equal).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tool_schemas: BTreeMap<String, String>,

    /// The model identifier the run pinned (a provider-precise string such
    /// as `gpt-5.2-2026-06-01`, not a floating alias like `gpt-5.2-latest` —
    /// an alias is not a pin).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    /// SHA-256 digest of the canonical JSON of the model parameters
    /// (temperature, seed, token limits, ...) the run pinned. A digest
    /// rather than the parameters themselves: the header stays cheap to scan
    /// and parameter sets with secrets-adjacent values never land in
    /// evidence verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_params: Option<String>,

    /// The memory schema version the run pinned (R0.8's memory record
    /// model). Declared in R0.7 so runs that outlive the R0.8 upgrade can
    /// still be interpreted against the schema their memories were written
    /// under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_schema: Option<String>,

    /// Capsule version pins: capsule name → [`CapsuleVersion`] (placeholder
    /// for R0.9's capsule manifests — see that type's docs).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub capsules: BTreeMap<String, CapsuleVersion>,

    /// SHA-256 digest of the canonical JSON of the middleware composition
    /// the run pinned (R0.11 Extension Plane, wave 4): the ordered layer
    /// list plus per-layer configuration the run's chain was instantiated
    /// from. This is the release's one additive manifest field — the
    /// design's named exception to the wire-frozen manifest — because no
    /// existing slot covers interception policy, and the deviation is
    /// smaller than leaving it unpinned. A digest rather than the
    /// composition itself, per the `model_params` reasoning; the digest ↔
    /// version join is the journaled resolution event, exactly as for the
    /// other families. Absent when the run bound no composition artifact —
    /// absent means unpinned, never a default, and old manifests are
    /// byte-stable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub middleware: Option<String>,

    /// The content address ([`crate::capability::CapabilitySet::id`]) of
    /// the capability set the run resolved at admission. Unlike the digest
    /// pins above, the address itself is the join: the set's canonical
    /// member list hashes to exactly this value, so re-resolution either
    /// reproduces the id or fails — see
    /// [`crate::capability::CapabilitySet::replay_guard`]. Absent means the
    /// run declared no capability set, never a default; old manifests are
    /// byte-stable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability_set: Option<String>,
}

impl RunManifest {
    /// An empty manifest: nothing pinned. Equivalent to [`Default`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Pin a prompt by content: records the SHA-256 of its exact UTF-8 text
    /// under `name`.
    pub fn pin_prompt(mut self, name: impl Into<String>, prompt: &str) -> Self {
        self.prompts
            .insert(name.into(), sha256_hex(prompt.as_bytes()));
        self
    }

    /// Pin a tool's parameters schema by content: records the SHA-256 of its
    /// canonical `serde_json` serialization under `name`.
    pub fn pin_tool_schema(mut self, name: impl Into<String>, schema: &Value) -> Self {
        self.tool_schemas
            .insert(name.into(), canonical_json_digest(schema));
        self
    }

    /// Pin the model identifier and the content digest of its parameters.
    pub fn pin_model(mut self, model: impl Into<String>, parameters: &Value) -> Self {
        self.model = Some(model.into());
        self.model_params = Some(canonical_json_digest(parameters));
        self
    }

    /// Pin the memory schema version (R0.8's record model; see the field
    /// docs).
    pub fn with_memory_schema(mut self, version: impl Into<String>) -> Self {
        self.memory_schema = Some(version.into());
        self
    }

    /// Pin a capsule version (R0.9 placeholder; see [`CapsuleVersion`]).
    pub fn pin_capsule(mut self, name: impl Into<String>, version: CapsuleVersion) -> Self {
        self.capsules.insert(name.into(), version);
        self
    }

    /// Pin a middleware composition by content: records the SHA-256 of the
    /// canonical `serde_json` serialization of its ordered layer list —
    /// the same digest convention every JSON pin here follows, computed by
    /// the same rule the registry's `resolution_pin` applies, so the
    /// journaled resolution digest and this pin can never diverge.
    pub fn pin_middleware(mut self, composition: &Value) -> Self {
        self.middleware = Some(canonical_json_digest(composition));
        self
    }

    /// Pin the capability set the run resolved at admission, by content
    /// address. The set id already *is* the SHA-256 of the set's canonical
    /// member list, so the pin records the id verbatim rather than hashing
    /// a second time.
    pub fn pin_capability_set(mut self, set: &crate::capability::CapabilitySet) -> Self {
        self.capability_set = Some(set.id().to_owned());
        self
    }
}

/// The value with every object map rebuilt in sorted key order,
/// recursively — the canonical form every content hash in this crate
/// covers.
///
/// `serde_json`'s default map is BTreeMap-backed, so serializing a `Value`
/// already emits object keys sorted; under the default backend this
/// transform is an identity and every pre-existing hash is preserved
/// byte-for-byte. The hazard is serde_json's `preserve_order` feature,
/// which `cedar-policy-core` (R0.9's Cedar engine, behind the server's
/// `capsules` feature) enables unconditionally: feature unification then
/// swaps the workspace-wide map for an insertion-ordered IndexMap, and
/// the same logical payload serializes — and hashes — differently
/// depending on the order its keys happened to be inserted. Routing every
/// hash through this canonical form keeps content addresses stable across
/// both map backends and across builds with and without the feature.
pub(crate) fn canonicalize_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by_key(|(key, _)| *key);
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, item)| (key.clone(), canonicalize_value(item)))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(canonicalize_value).collect()),
        scalar => scalar.clone(),
    }
}

/// SHA-256 of the canonical `serde_json` serialization of `value` — the
/// digest convention shared by every manifest pin over JSON content.
fn canonical_json_digest(value: &Value) -> String {
    // Serializing a `Value` is infallible in practice (its maps always have
    // string keys); `PayloadRef::content_hash` documents why the result is
    // canonical.
    let bytes = serde_json::to_vec(&canonicalize_value(value))
        .expect("a serde_json::Value always serializes");
    sha256_hex(&bytes)
}

/// The configuration envelope a run declares at start (evidence and
/// admission wave), journaled as [`RunEventKind::RunConfigDeclared`].
///
/// The model-call event already pins the request half of the envelope — the
/// messages and the resolved, post-allowlist tool schemas, canonically
/// ordered, hashed, and matched on replay. This is the other half: the run
/// configuration those requests were shaped by. With both halves journaled,
/// a recorded run's model requests are a pure function of the log, and
/// exact replay asserts the whole envelope rather than trusting it.
///
/// Deliberately excluded: driver bounds that shape no request — the step
/// limit and the cancellation token are safety rails of the driving
/// process, not evidence about what the run asked of the world.
///
/// Sparse on the wire: the allowlist and the manifest are absent when the
/// run declared neither, so the declaration of a run that pinned only its
/// graph version carries exactly that pin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunConfigDeclaration {
    /// The application-declared graph version, resolved as the checkpoint
    /// headers stamp it (`"unversioned"` when the application pins none).
    pub graph_version: String,

    /// SHA-256 of the compiled graph topology (`Graph::topology_hash`).
    pub graph_hash: String,

    /// The executor policy version the run bound, resolved as the
    /// checkpoint headers stamp it.
    pub policy_version: PolicyVersion,

    /// The run's normalized tool allowlist (sorted, deduplicated — see
    /// [`crate::executor::RunConfig::with_tool_allowlist`]). The resolved
    /// schema set itself is pinned per call in the model-call input; this
    /// pins the admission decision that produced it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_allowlist: Option<Vec<String>>,

    /// The versioned run manifest the run pinned (model identity, model
    /// parameters digest, prompt and tool-schema digests, and the later
    /// pin families) — carried whole so the journaled envelope names the
    /// exact configuration, not a digest of it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest: Option<RunManifest>,
}

/// The closed approval vocabulary (evidence and admission wave): every
/// approval gate in the harness decides in these terms and no others.
///
/// Exactly one variant grants — [`ApprovalDecision::ApprovedOnce`], with
/// allowed-once semantics: the decision admits the single occurrence it was
/// asked about and is spent by admitting it. Every other outcome denies,
/// so the vocabulary is fail-closed by construction: an unanswered ask
/// (`Unavailable`), a withdrawn ask (`Cancelled`), and a refused ask
/// (`Rejected`) are three different facts with one shared consequence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum ApprovalDecision {
    /// The ask was approved for this occurrence only. The only granting
    /// outcome.
    ApprovedOnce {
        /// Who approved — a human operator id, a policy name, a token's
        /// `approved_by`. Evidence, not authentication.
        approved_by: String,
    },

    /// A decider considered the ask and refused it.
    Rejected {
        /// Who refused.
        decided_by: String,
        /// Why, when the decider gave a reason.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },

    /// The ask was withdrawn before a decider answered.
    Cancelled {
        /// Why, when the withdrawal gave a reason.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },

    /// No decider was available to answer the ask — a missing token, an
    /// unreachable approver, a gate with no decision source. Denies,
    /// indistinguishable from a refusal at the gate and deliberately
    /// distinguishable from one in the evidence.
    Unavailable {
        /// Why no decider answered.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

impl ApprovalDecision {
    /// Whether this decision grants the ask. Only
    /// [`ApprovalDecision::ApprovedOnce`] does — the fail-closed half of the
    /// vocabulary, one line long so it cannot drift.
    pub fn grants(&self) -> bool {
        matches!(self, Self::ApprovedOnce { .. })
    }

    /// Who approved, when the decision is the granting variant.
    pub fn approved_by(&self) -> Option<&str> {
        match self {
            Self::ApprovedOnce { approved_by } => Some(approved_by),
            _ => None,
        }
    }
}

/// The asked half of an approval pair (evidence and admission wave),
/// journaled as the input of [`RunEventKind::ApprovalAsked`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    /// The surface asking — an effect kind (`publish_composed_skill`), a
    /// capability pack's gate name. Free-form within the asking plane's own
    /// stability rules: it joins the pair to the surface's evidence.
    pub kind: String,

    /// The occurrence id the ask is scoped to, when occurrence-scoped (the
    /// effect kernel's derived effect id, as a bare digest string).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect_id: Option<String>,

    /// Asker-supplied context for the decider and the audit (the draft's
    /// content hash, the command a CLI gate would run). Bounded by the
    /// asking plane; never secret-bearing — the journal is evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<Value>,
}

/// The decided half of an approval pair (evidence and admission wave),
/// journaled as the output of [`RunEventKind::ApprovalDecided`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalOutcome {
    /// The surface that asked — echoed from the [`ApprovalRequest`] so the
    /// decided event reads standalone.
    pub kind: String,

    /// The occurrence id the decision binds, echoing the ask.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect_id: Option<String>,

    /// The decision, in the closed vocabulary.
    pub decision: ApprovalDecision,
}

/// The provenance header stamped into every checkpoint.
///
/// Answers, for any stored checkpoint: which checkpoint format wrote it
/// (`format_version`), which graph produced it (`graph_version` +
/// `graph_hash`), under which policy (`policy_version`), and where it sits
/// on the run's logical clock. Without this, a checkpoint is data; with it,
/// a checkpoint is interpretable evidence — replay can refuse (or migrate)
/// checkpoints whose format or graph no longer matches.
///
/// Added to `Checkpoint` with serde defaults: checkpoints written before
/// R0.5 (no header) deserialize into [`CheckpointHeader::default`]. R0.7
/// extends the header additively with the [`RunManifest`] pin set — same
/// rule: unset fields stay absent from the wire and old headers keep
/// deserializing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckpointHeader {
    /// Checkpoint envelope format version; [`CURRENT_FORMAT_VERSION`] for
    /// anything written now.
    pub format_version: u32,

    /// Application-declared graph version (via `RunConfig::with_graph_version`),
    /// or `"unversioned"` when the application does not version its graph.
    pub graph_version: String,

    /// SHA-256 content hash of the compiled graph topology (node names and
    /// edge shape — see `Graph::topology_hash`). Detects graph drift between
    /// a checkpoint and the code about to resume it; semantic node-body
    /// changes are the application's responsibility via `graph_version`.
    pub graph_hash: String,

    /// The executor policy active when the checkpoint was written.
    pub policy_version: PolicyVersion,

    /// The run's logical clock value (milliseconds) at creation. Under the
    /// default system clock this is epoch milliseconds; under a logical
    /// clock it is the deterministic tick — either way it is the ordering
    /// and replay handle, not wall time.
    pub logical_clock: u64,

    /// The versioned run manifest (R0.7): prompts, tool schemas, model and
    /// parameters, memory schema, and capsule versions pinned for the run —
    /// see [`RunManifest`] for the upgrade-safety contract. Stamped from
    /// `RunConfig::with_manifest` at checkpoint time.
    ///
    /// Additive: `None` (the default) is absent from the wire, so headers
    /// written before R0.7 — and headers of runs that pin nothing — keep
    /// their exact R0.5 shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest: Option<RunManifest>,

    /// The durable inbox's queue state at this checkpoint (R0.13 parity
    /// wave): the steering / follow-up / staged queues and the next intake
    /// sequence, so a resumed run's inbox continues exactly where the
    /// checkpoint was taken. Stamped from [`crate::executor::RunConfig`]'s
    /// inbox at checkpoint time; resume seeds a fresh
    /// [`crate::inbox::Inbox`] from it.
    ///
    /// Additive: `None` (the default) is absent from the wire, so headers of
    /// runs without an inbox — and of inbox runs that never accepted a send
    /// — keep their exact prior shape (the [`RunManifest`] discipline).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inbox: Option<crate::inbox::InboxSnapshot>,
}

impl Default for CheckpointHeader {
    /// The header for a checkpoint written without run context: current
    /// format, unversioned/empty graph identity, static policy, clock zero,
    /// no manifest. Also the deserialization fallback for pre-R0.5
    /// checkpoints.
    fn default() -> Self {
        Self {
            format_version: CURRENT_FORMAT_VERSION,
            graph_version: "unversioned".to_owned(),
            graph_hash: String::new(),
            policy_version: PolicyVersion::default(),
            logical_clock: 0,
            manifest: None,
            inbox: None,
        }
    }
}

/// A reference to the journal head at a checkpoint boundary, stamped into
/// the checkpoint so evidence and state travel together.
///
/// The hash is the journal's running head hash (chained SHA-256 over
/// recorded events), so a checkpoint pins not just *how many* events existed
/// but *which* events — tamper-evident linkage between state and evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalRef {
    /// Number of events in the journal at the boundary.
    pub events: u64,

    /// Journal head hash (chained SHA-256) at the boundary.
    pub sha256: String,
}

// ---------------------------------------------------------------------------
// Pause envelope: versioned snapshot with tool-identity rebinding (EP-03-S06)
// ---------------------------------------------------------------------------

/// The semver schema version of a [`PauseEnvelope`]. Bump only on breaking
/// changes to the envelope; additive evolution uses serde defaults instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PauseSchemaVersion {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl PauseSchemaVersion {
    /// The current envelope schema version (1.0.0).
    pub const CURRENT: Self = Self {
        major: 1,
        minor: 0,
        patch: 0,
    };

    /// The minimum version this build can read. Older envelopes deserialize
    /// under the additive-evolution contract; newer envelopes fail the floor
    /// check with a typed error naming both versions.
    pub const MINIMUM: Self = Self::CURRENT;

    /// Check that `self >= MINIMUM`. Fails closed with a message naming the
    /// envelope's version, the required minimum, and the feature that raised
    /// the floor.
    pub fn check_floor(&self, feature: &str) -> crate::error::Result<()> {
        if self < &Self::MINIMUM {
            return Err(RustyError::Checkpoint(format!(
                "pause envelope schema version {}.{}.{} is below the minimum                  {}.{}.{} required by feature `{feature}` — upgrade the runtime                  to resume; incompatible envelopes are never silently reinterpreted",
                self.major, self.minor, self.patch,
                Self::MINIMUM.major, Self::MINIMUM.minor, Self::MINIMUM.patch,
            )));
        }
        Ok(())
    }

    /// String representation for logging and error messages.
    pub fn as_string(&self) -> String {
        format!("{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl Default for PauseSchemaVersion {
    fn default() -> Self {
        Self::CURRENT
    }
}

impl PartialOrd for PauseSchemaVersion {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PauseSchemaVersion {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.major
            .cmp(&other.major)
            .then_with(|| self.minor.cmp(&other.minor))
            .then_with(|| self.patch.cmp(&other.patch))
    }
}

/// A key that uniquely identifies a tool within an agent's scope, used for
/// rebinding tool identities across pause/resume boundaries.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ToolIdentityKey {
    /// The agent path that owns the tool (e.g. `"main"` or a subagent path).
    pub agent_path: String,

    /// The fully qualified tool name, including any combinator prefixes
    /// (e.g. `"filtered:prefix:tool_name"`).
    pub qualified_tool_name: String,
}

impl ToolIdentityKey {
    /// A canonical string representation for indexing and error messages.
    pub fn canonical(&self) -> String {
        format!("{}/{}", self.agent_path, self.qualified_tool_name)
    }
}

/// The status of a [`RunObligation`] in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObligationStatus {
    /// The obligation is pending and blocks resume (when blocking).
    Open,
    /// The obligation was answered and admitted.
    Satisfied,
    /// The obligation was answered and refused.
    Rejected,
    /// The obligation expired unanswered.
    Expired,
}

/// The kind of pause obligation, carrying the data a surface needs to render
/// its form and validate answers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ObligationKind {
    /// A tool call requiring human confirmation.
    Approval {
        /// The scope of who may approve.
        scope: String,
        /// Whether "always allow" is permitted.
        #[serde(default)]
        sticky_allowed: bool,
    },

    /// Typed input requested from a human or external system.
    StructuredInput {
        /// JSON Schema describing the expected answer shape.
        input_schema: Value,
    },

    /// Output review requested.
    Feedback {
        /// The event id whose output is under review.
        subject_event_id: String,
    },

    /// A tool whose execution is delegated to an external caller.
    ExternalExecution {
        /// The tool name being delegated.
        tool: String,
        /// The arguments the tool was called with.
        arguments: Value,
        /// JSON Schema describing the expected result shape.
        result_schema: Value,
    },
}

/// One outstanding requirement on a paused run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunObligation {
    /// Stable obligation id (UUID v4).
    pub id: String,

    /// The tool call id this obligation is tied to, when tool-tied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,

    /// The kind and its kind-specific data.
    pub kind: ObligationKind,

    /// Current lifecycle status.
    pub status: ObligationStatus,

    /// When the obligation expires, if ever.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,

    /// For nested member runs: the run id of the subagent that raised this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member_run_id: Option<String>,
}

/// A sticky approval record: an "always allow" or "always deny" decision
/// persisted in the envelope so resumed runs honour it without re-asking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StickyApproval {
    /// The tool identity this sticky record binds to.
    pub tool_key: ToolIdentityKey,

    /// Whether the sticky record grants or denies.
    pub grants: bool,

    /// Who set the sticky record (for audit).
    pub set_by: String,

    /// When the sticky record was set.
    pub set_at: DateTime<Utc>,
}

/// The export-boundary snapshot of a paused run. Small by design: transcript,
/// memory, and scheduler state are re-derived from the log at resume.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PauseEnvelope {
    /// Semver schema version of this envelope.
    pub schema_version: PauseSchemaVersion,

    /// The run that paused.
    pub run_id: String,

    /// The session the run belongs to.
    pub session_id: String,

    /// Log position (journal event count) at the pause boundary.
    pub log_position: u64,

    /// Open obligations that must be satisfied before resume.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub obligations: Vec<RunObligation>,

    /// Sticky approvals carried forward from prior decisions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sticky_approvals: Vec<StickyApproval>,

    /// Tool identity keys present at pause time, used for rebinding on resume.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_identities: Vec<ToolIdentityKey>,

    /// The checkpoint id at the suspension point (resume loads this checkpoint).
    pub checkpoint_id: String,

    /// When the envelope was created.
    pub created_at: DateTime<Utc>,
}

impl PauseEnvelope {
    /// A new envelope with the current schema version.
    pub fn new(
        run_id: impl Into<String>,
        session_id: impl Into<String>,
        log_position: u64,
        checkpoint_id: impl Into<String>,
    ) -> Self {
        Self {
            schema_version: PauseSchemaVersion::CURRENT,
            run_id: run_id.into(),
            session_id: session_id.into(),
            log_position,
            obligations: Vec::new(),
            sticky_approvals: Vec::new(),
            tool_identities: Vec::new(),
            checkpoint_id: checkpoint_id.into(),
            created_at: Utc::now(),
        }
    }

    /// Validate the envelope's schema version against this build's floor.
    pub fn check_version(&self) -> crate::error::Result<()> {
        self.schema_version.check_floor("pause-envelope")
    }

    /// Whether the envelope carries no open obligations (resume would proceed
    /// immediately, used for sanity checks).
    pub fn is_resumable(&self) -> bool {
        self.obligations
            .iter()
            .all(|o| !matches!(o.status, ObligationStatus::Open))
    }
}

/// The result of attempting to rebind tool identities from a [`PauseEnvelope`]
/// to a live toolset.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolRebindingResult {
    /// Every key bound to exactly one live tool.
    Bound {
        /// Map from canonical key string to the live tool name that was bound.
        bindings: std::collections::HashMap<String, String>,
    },
    /// One or more keys could not be bound. Resume must fail with the list
    /// of unbindable keys.
    Unbindable {
        /// The keys that could not be matched against the live toolset.
        keys: Vec<ToolIdentityKey>,
    },
}

/// Attempt to rebind every [`ToolIdentityKey`] in the envelope against the
/// live `toolset`. Returns [`ToolRebindingResult::Bound`] when every key
/// matches exactly one live tool, or [`ToolRebindingResult::Unbindable`] with
/// the failing keys otherwise.
pub fn rebind_tool_identities(envelope: &PauseEnvelope, toolset: &[String]) -> ToolRebindingResult {
    let mut bindings = std::collections::HashMap::new();
    let mut unbindable = Vec::new();

    for key in &envelope.tool_identities {
        let canonical = key.canonical();
        // Match by qualified_tool_name against the live toolset.
        // Combinator prefixes are included in the qualified name, so an
        // exact match is required.
        let matched: Vec<&String> = toolset
            .iter()
            .filter(|t| *t == &key.qualified_tool_name)
            .collect();
        if matched.len() == 1 {
            bindings.insert(canonical, matched[0].clone());
        } else {
            unbindable.push(key.clone());
        }
    }

    if unbindable.is_empty() {
        ToolRebindingResult::Bound { bindings }
    } else {
        ToolRebindingResult::Unbindable { keys: unbindable }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sha256_hex_is_stable_and_lowercase() {
        // SHA-256 of the empty input, pinned against the published digest.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(sha256_hex(b"abc").len(), 64);
    }

    #[test]
    fn policy_version_default_is_static_floor() {
        assert_eq!(PolicyVersion::default().as_str(), PolicyVersion::STATIC_V0);
        // Transparent newtype: serializes as a bare string.
        assert_eq!(
            serde_json::to_value(PolicyVersion::default()).unwrap(),
            json!("static-v0")
        );
    }

    #[test]
    fn executor_policy_static_floor_mirrors_the_durable_constants() {
        let floor = ExecutorPolicy::static_v0();
        assert_eq!(
            floor.retry.base_delay_ms,
            crate::durable::BASE_RETRY_DELAY_MS
        );
        assert_eq!(floor.retry.max_delay_ms, crate::durable::MAX_RETRY_DELAY_MS);
        // The floor pins the server's default attempt budget; timeout and
        // concurrency are uncapped — the honest encoding of "no policy".
        assert_eq!(floor.retry.max_attempts, 3);
        assert!(floor.timeout.default_millis.is_none());
        assert!(floor.timeout.max_millis.is_none());
        assert!(floor.concurrency.max_parallel.is_none());
        // Unset options stay absent from the wire.
        let wire = serde_json::to_value(floor).unwrap();
        assert_eq!(wire["timeout"], json!({}));
        assert_eq!(wire["concurrency"], json!({}));
    }

    #[test]
    fn derive_policy_version_is_content_addressed() {
        let policy = ExecutorPolicy::static_v0()
            .with_family_parameters(
                DecisionFamily::Retry,
                json!({"base_delay_ms": 500, "max_delay_ms": 60_000, "max_attempts": 5}),
            )
            .unwrap();
        let first = derive_policy_version(&policy).unwrap();
        let again = derive_policy_version(&policy).unwrap();
        assert_eq!(first, again, "version derivation must be deterministic");
        assert!(
            first.as_str().starts_with("policy-"),
            "derived versions carry the policy- prefix: {first}"
        );
        assert_eq!(first.as_str().len(), "policy-".len() + 12);

        // Any parameter change is a different version — that is what makes
        // registry immutability enforceable.
        let changed = policy
            .with_family_parameters(
                DecisionFamily::Retry,
                json!({"base_delay_ms": 500, "max_delay_ms": 60_000, "max_attempts": 6}),
            )
            .unwrap();
        assert_ne!(first, derive_policy_version(&changed).unwrap());
    }

    #[test]
    fn with_family_parameters_validates_and_overlays() {
        let base = ExecutorPolicy::static_v0();

        // Timeout and concurrency families parse their own contracts.
        let with_timeout = base
            .with_family_parameters(
                DecisionFamily::Timeout,
                json!({"default_millis": 30_000, "max_millis": 120_000}),
            )
            .unwrap();
        assert_eq!(with_timeout.timeout.default_millis, Some(30_000));
        assert_eq!(with_timeout.timeout.max_millis, Some(120_000));
        // Untouched families keep the base values.
        assert_eq!(with_timeout.retry, base.retry);

        let with_concurrency = base
            .with_family_parameters(DecisionFamily::Concurrency, json!({"max_parallel": 8}))
            .unwrap();
        assert_eq!(with_concurrency.concurrency.max_parallel, Some(8));

        // A malformed body is rejected with the family named.
        let err = base
            .with_family_parameters(DecisionFamily::Retry, json!({"base_delay_ms": "fast"}))
            .unwrap_err();
        assert!(err.to_string().contains("retry policy parameters"), "{err}");

        // Families without a v1 parameter contract are rejected.
        for family in [
            DecisionFamily::WorkerPlacement,
            DecisionFamily::CheckpointPlacement,
        ] {
            let err = base.with_family_parameters(family, json!({})).unwrap_err();
            assert!(
                err.to_string().contains("no executor-policy parameter"),
                "{err}"
            );
        }
    }

    #[test]
    fn payload_ref_content_hash_agrees_across_representations() {
        let value = json!({"b": 1, "a": [2, 3]});
        let bytes = serde_json::to_vec(&value).unwrap();
        let inline = PayloadRef::inline(value);
        let referenced = PayloadRef::Artifact(ArtifactRef {
            sha256: sha256_hex(&bytes),
            bytes: bytes.len() as u64,
        });
        assert_eq!(
            inline.content_hash().unwrap(),
            referenced.content_hash().unwrap()
        );
    }

    #[test]
    fn effect_repeatability_ladder() {
        assert!(Effect::Pure.is_freely_repeatable());
        assert!(Effect::ReadOnly.is_freely_repeatable());
        assert!(Effect::Idempotent.is_freely_repeatable());
        assert!(!Effect::Compensatable.is_freely_repeatable());
        assert!(!Effect::NonIdempotent.is_freely_repeatable());
    }

    #[test]
    fn contracts_serde_roundtrip() {
        let event = RunEvent {
            id: "r1:7".into(),
            run_id: "r1".into(),
            thread_id: "t1".into(),
            node_id: Some("agent".into()),
            seq: 7,
            kind: RunEventKind::ModelCall,
            effect: Effect::NonIdempotent,
            input: Some(PayloadRef::inline(json!({"messages": []}))),
            output: Some(PayloadRef::Artifact(ArtifactRef {
                sha256: sha256_hex(b"response"),
                bytes: 8,
            })),
            latency_ms: Some(42),
            tokens: Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                cached_tokens: None,
                reasoning_tokens: None,
            }),
            cost_usd: Some(0.0001),
            status: EventStatus::Ok,
            parent: Some("r1:3".into()),
            recorded_at: DateTime::<Utc>::from_timestamp_millis(1_000).unwrap(),
        };
        let back: RunEvent = serde_json::from_str(&serde_json::to_string(&event).unwrap()).unwrap();
        assert_eq!(event, back);

        let decision = DecisionEvent {
            id: "r1:d0".into(),
            run_id: "r1".into(),
            thread_id: "t1".into(),
            seq: 0,
            family: DecisionFamily::Retry,
            features: Map::from_iter([("failure_class".to_owned(), json!("timeout"))]),
            legal_actions: vec![DecisionAction::Retry { attempt: 1 }, DecisionAction::Abort],
            selected: DecisionAction::Retry { attempt: 1 },
            propensity: 0.75,
            policy_version: PolicyVersion::default(),
            role: None,
            outcome: None,
            decided_at: DateTime::<Utc>::from_timestamp_millis(1_000).unwrap(),
        };
        let back: DecisionEvent =
            serde_json::from_str(&serde_json::to_string(&decision).unwrap()).unwrap();
        assert_eq!(decision, back);
    }

    #[test]
    fn checkpoint_header_default_matches_pre_r05_fallback() {
        let header = CheckpointHeader::default();
        assert_eq!(header.format_version, CURRENT_FORMAT_VERSION);
        assert_eq!(header.graph_version, "unversioned");
        assert_eq!(header.policy_version, PolicyVersion::default());
        assert_eq!(header.logical_clock, 0);
        assert_eq!(header.manifest, None);
    }

    #[test]
    fn manifest_digests_are_stable_content_addresses() {
        // Constants computed independently (SHA-256 of the exact bytes): the
        // pins are content addresses, so their stability across releases is
        // the contract — a drifting digest would silently split runs from
        // their own evidence.
        let manifest = RunManifest::new()
            .pin_prompt("system", "You are a careful research agent.")
            .pin_tool_schema(
                "search",
                &json!({"type": "object", "properties": {"query": {"type": "string"}}}),
            )
            .pin_model(
                "gpt-5.2-2026-06-01",
                &json!({"temperature": 0, "seed": 42, "max_tokens": 512}),
            )
            .with_memory_schema("memory-v1")
            .pin_capsule("researcher", CapsuleVersion::new("1.4.0"));

        assert_eq!(
            manifest.prompts["system"],
            "edff20fbc61c032a9206bdc94aec3b70e05ab953383972b267b83957d9ba7bfe"
        );
        assert_eq!(
            manifest.tool_schemas["search"],
            "094ec29d007cce150c65abf0756d79ad5b62a1acfdb6e0841f69f1377ef41761"
        );
        assert_eq!(
            manifest.model_params.as_deref(),
            Some("fc004364a674799b07f8e4e6323ad74398745607ae3bd955e0d9c1d7529a0762")
        );
        assert_eq!(manifest.model.as_deref(), Some("gpt-5.2-2026-06-01"));
        assert_eq!(manifest.memory_schema.as_deref(), Some("memory-v1"));
        assert_eq!(manifest.capsules["researcher"].as_str(), "1.4.0");

        // Key order in the source JSON must not matter: canonicalization
        // sorts object keys, so semantically equal schemas pin equal.
        let reordered = RunManifest::new().pin_tool_schema(
            "search",
            &json!({"properties": {"query": {"type": "string"}}, "type": "object"}),
        );
        assert_eq!(manifest.tool_schemas, reordered.tool_schemas);
    }

    #[test]
    fn manifest_serde_roundtrip_and_sparse_wire_shape() {
        let manifest = RunManifest::new()
            .pin_prompt("system", "Be brief.")
            .pin_model("model-x", &json!({"temperature": 0.5}));
        let back: RunManifest =
            serde_json::from_str(&serde_json::to_string(&manifest).unwrap()).unwrap();
        assert_eq!(manifest, back);

        // Sparse pins stay sparse on the wire: unset fields are absent, not
        // null or empty — pre-R0.7 readers see no new keys at all.
        let value = serde_json::to_value(&manifest).unwrap();
        assert!(value.get("tool_schemas").is_none());
        assert!(value.get("memory_schema").is_none());
        assert!(value.get("capsules").is_none());
        assert!(value.get("prompts").is_some());

        // An empty manifest serializes to an empty object, and a header that
        // carries no manifest omits the field entirely.
        assert_eq!(serde_json::to_value(RunManifest::new()).unwrap(), json!({}));
        let header = CheckpointHeader::default();
        assert!(serde_json::to_value(&header)
            .unwrap()
            .get("manifest")
            .is_none());
    }

    #[test]
    fn r05_header_without_manifest_still_loads() {
        // The R0.5 shape — exactly what the golden file pins — deserializes
        // into the extended header with the manifest unset.
        let r05_shape = json!({
            "format_version": 1,
            "graph_version": "react-v3",
            "graph_hash": "2c26b46b68ffc68ff99b453c1d30413413422d706483bfa0f98a5e886266e7ae",
            "policy_version": "static-v0",
            "logical_clock": 1750000000000u64,
        });
        let header: CheckpointHeader = serde_json::from_value(r05_shape).unwrap();
        assert_eq!(header.manifest, None);
        assert_eq!(header.format_version, CURRENT_FORMAT_VERSION);

        // And a header carrying a manifest round-trips it.
        let extended = CheckpointHeader {
            manifest: Some(RunManifest::new().with_memory_schema("memory-v1")),
            ..header
        };
        let back: CheckpointHeader =
            serde_json::from_str(&serde_json::to_string(&extended).unwrap()).unwrap();
        assert_eq!(extended, back);
    }

    #[test]
    fn effect_receipt_effect_id_is_additive() {
        let receipt = EffectReceipt {
            provider: "stripe".into(),
            provider_id: "ch_3PKd".into(),
            idempotency_key: "run-1:charge:7".into(),
            task_id: None,
            effect_id: None,
        };
        // Unset: absent on the wire — pre-R0.7 receipts see no shape change.
        let value = serde_json::to_value(&receipt).unwrap();
        assert!(value.get("effect_id").is_none());

        // Set: carried as a bare digest string, surviving the round-trip.
        let with_id = EffectReceipt {
            effect_id: Some(
                "9b71d224bd62f3785d96d46ad3ea3d73319bfbc2890caadae2dff72519673ca7".into(),
            ),
            ..receipt
        };
        let back: EffectReceipt =
            serde_json::from_str(&serde_json::to_string(&with_id).unwrap()).unwrap();
        assert_eq!(with_id, back);
    }
}
