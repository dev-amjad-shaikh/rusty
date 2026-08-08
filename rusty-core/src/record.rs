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
/// less automation freedom than the one before.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// sort deterministically), so equal payloads hash equal regardless of
    /// which representation carried them.
    pub fn content_hash(&self) -> Result<String, serde_json::Error> {
        match self {
            PayloadRef::Inline(value) => {
                let bytes = serde_json::to_vec(value)?;
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

/// The outcome status of a journaled event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
    /// tree. Inert in wave 1 — emission lands with the agent host.
    MailboxSend,

    /// An agent's activation began a turn on a mailbox message (R0.7): input
    /// carries the task id and idempotency key the turn is processing.
    /// The recipient-side half of the mailbox pair. Inert in wave 1.
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
    /// event the pattern spawns. Contract only in wave 1.
    CoordinationStart,

    /// A coordination pattern settled (R0.7 wave 3): output carries the
    /// result reference and the per-member dispositions (completed, failed,
    /// cancelled) — the fan-in evidence record. Contract only in wave 1.
    CoordinationEnd,
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionOutcome {
    /// The selected action led to completion.
    Success,
    /// The selected action did not lead to completion.
    Failure,
    /// The run was cancelled or superseded before the outcome materialized.
    Cancelled,
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
/// v1 freezes this contract but the executor does not yet emit decision
/// events; the R0.5 journal already records the state/evidence decisions
/// would be evaluated against.
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

    /// The result of the decision, `None` until completion.
    pub outcome: Option<DecisionOutcome>,

    /// When the decision was made, read from the run's clock.
    pub decided_at: DateTime<Utc>,
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
}

/// SHA-256 of the canonical `serde_json` serialization of `value` — the
/// digest convention shared by every manifest pin over JSON content.
fn canonical_json_digest(value: &Value) -> String {
    // Serializing a `Value` is infallible in practice (its maps always have
    // string keys); `PayloadRef::content_hash` documents why the result is
    // canonical.
    let bytes = serde_json::to_vec(value).expect("a serde_json::Value always serializes");
    sha256_hex(&bytes)
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
