//! The executor: a Pregel/BSP-inspired super-step run loop.
//!
//! Execution proceeds in discrete **super-steps** (Google Pregel /
//! Bulk-Synchronous-Parallel), each super-step being:
//!
//! 1. **Plan** — determine the active node set (entry point on step 0;
//!    afterwards the routing result of the previous step, including
//!    [`crate::node::Command::goto`] overrides and [`crate::graph::Send`]
//!    fan-outs).
//! 2. **Compute** — run all active nodes concurrently in a
//!    `tokio::task::JoinSet`, each receiving an **immutable snapshot** of
//!    the state as of the start of the step. No node can observe another's
//!    in-progress writes.
//! 3. **Barrier** — wait for all active nodes. The step is *transactional*:
//!    if any node fails, the step's writes are discarded. An
//!    [`RustyError::Interrupt`] suspends the whole run instead.
//! 4. **Merge** — apply all node updates to the state via
//!    [`crate::state::StateSpec::apply_super_step`] (per-channel reducers +
//!    `LastValue` single-write validation).
//! 5. **Route** — evaluate outgoing edges / commands against the
//!    post-barrier state to determine the next active set; `Route::End` (or
//!    an empty next set) terminates the run.
//! 6. **Checkpoint** — persist a [`crate::checkpoint::Checkpoint`] recording
//!    step, state, and next nodes, and emit [`GraphEvent`]s for streaming.
//!
//! A graph *cycle* (e.g. the ReAct loop `agent → tools → agent`) is not
//! call-stack recursion — it is nodes being re-scheduled across super-steps,
//! which is why the guard is `max_steps`, not a stack limit.
//!
//! # Observability
//!
//! The executor emits `tracing` telemetry throughout a run (no subscriber is
//! installed by the library — the application chooses one):
//!
//! - `rusty.run` (INFO span) — one per [`Executor::run`] call, carrying
//!   `thread_id` and `max_steps`; parent of everything below.
//! - `rusty.super_step` (DEBUG span) — one per super-step, carrying
//!   `step` and `active_nodes`; covers plan → barrier → merge → route.
//! - `rusty.node` (INFO span) — one per spawned node task, carrying
//!   `node` and `step`; attached to the `JoinSet` task via `.instrument()`.
//! - DEBUG event on each barrier merge (channels written), INFO events on
//!   interrupt and run completion (`steps`, `duration_ms`), WARN events on
//!   node failure (with a `retryable` classification).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::checkpoint::{Checkpoint, Checkpointer};
use crate::effects::{CompensationRegistry, EffectAdmissionContext};
use crate::error::{Result, RustyError};
use crate::graph::{Edge, Graph, Route};
use crate::journal::{Clock, EventDraft, Journal, RngSource};
use crate::middleware::{Middleware, MiddlewareChain, NodeCall};
use crate::node::{Command, NodeConfig, NodeContext, NodeOutput};
use crate::record::{
    CheckpointHeader, Effect, EventStatus, PolicyVersion, RunEventKind, RunManifest,
    CURRENT_FORMAT_VERSION,
};
use crate::state::{State, StateSpec};

/// How a run ended.
#[derive(Debug)]
pub enum ExecutionOutcome {
    /// The run terminated normally (routing reached `Route::End` or no
    /// nodes remained active). Carries the final state.
    Done(State),

    /// A node called `interrupt(payload)`: the run is suspended and
    /// resumable. Carry on by calling [`Executor::run`] again with the same
    /// `thread_id` and `RunConfig::resume` set.
    ///
    /// Suspension is run-wide, not node-local. The in-flight super-step is
    /// transactional: every write of the step is discarded (including writes
    /// from sibling nodes that completed before the interrupt was observed
    /// at the barrier), still-running siblings are aborted, and the
    /// suspension checkpoint re-schedules **every** node of the step. On
    /// resume all of them re-execute from their start, so node logic must be
    /// idempotent.
    Interrupted {
        /// The payload passed to `interrupt()` (surfaced to the caller,
        /// e.g. a human-approval request).
        value: Value,
        /// The state as of the suspension point.
        state: State,
        /// The checkpoint persisted at the suspension point, for resuming
        /// or time travel. When the executor has no checkpointer attached,
        /// the run still suspends but nothing is persisted: the id is then
        /// only an opaque handle and can never be replayed.
        checkpoint_id: String,
    },
}

impl ExecutionOutcome {
    /// The final (or suspension-point) state, regardless of variant.
    pub fn state(&self) -> &State {
        match self {
            ExecutionOutcome::Done(s) => s,
            ExecutionOutcome::Interrupted { state, .. } => state,
        }
    }

    /// Whether the run ended in [`ExecutionOutcome::Interrupted`] (suspended,
    /// resumable) rather than [`ExecutionOutcome::Done`].
    pub fn is_interrupted(&self) -> bool {
        matches!(self, ExecutionOutcome::Interrupted { .. })
    }
}

/// Per-run configuration (the LangGraph `RunnableConfig` analog).
#[derive(Debug, Clone)]
pub struct RunConfig {
    /// Thread (session) id. Stable across interrupt/resume; namespaces all
    /// checkpoints for this run. Required for persistence and resume.
    pub thread_id: String,

    /// Maximum number of super-steps before the run aborts with
    /// [`crate::error::RustyError::Graph`] (the LangGraph
    /// `recursion_limit` / `GraphRecursionError` guard). Default: 1000.
    pub max_steps: usize,

    /// Resume value for continuing an interrupted run. When set, the
    /// executor restores the latest checkpoint for `thread_id` and
    /// re-executes the checkpointed next-node set with
    /// [`crate::node::NodeContext::resume_value`] returning this value.
    ///
    /// The value is **broadcast**: every node scheduled in the first
    /// super-step after the resume observes it, not only the node that
    /// originally interrupted (a suspension checkpoint re-schedules the
    /// whole active set — see [`ExecutionOutcome::Interrupted`]). Nodes that
    /// should react only when they themselves were resumed must key off
    /// their own state, not the presence of a resume value.
    pub resume: Option<Value>,

    /// Replay/time-travel handle: the id of a specific checkpoint of
    /// `thread_id` to resume from. When set, the executor loads **that**
    /// checkpoint (not the latest) and continues the run from its state and
    /// next-node set. Requires a checkpointer on the executor.
    ///
    /// Combines with `resume`: `checkpoint_id` selects **where** the run
    /// restarts, `resume` (when also set) is delivered as the resume value to
    /// the first super-step, exactly as in interrupt/resume.
    ///
    /// Safe pattern: replaying on the *same* thread appends new history on
    /// top of the old timeline, so prefer forking first —
    /// [`crate::checkpoint::Checkpointer::fork_thread`] the thread into a new
    /// thread id, then run the fork with `checkpoint_id` set. Direct replay
    /// on the original thread is supported for cases where appended history
    /// is acceptable.
    pub checkpoint_id: Option<String>,

    /// Optional event sink for streaming: the executor emits [`GraphEvent`]s
    /// as the run progresses (node start/end, state updates, checkpoints,
    /// super-step boundaries). Consumers implement LangGraph's stream modes
    /// (`values` / `updates` / `tasks` / ...) as filters over this stream.
    pub event_tx: Option<mpsc::Sender<GraphEvent>>,

    /// Flight Recorder determinism seam (R0.5): the run's time source. Every
    /// executor timestamp — event `recorded_at`, node latencies, checkpoint
    /// `created_at` — is read through it. `None` (the default) is the system
    /// wall clock, byte-identical to pre-R0.5 behavior. Attach
    /// [`Clock::Logical`] to make a recorded run re-drivable.
    pub clock: Option<Clock>,

    /// Flight Recorder determinism seam (R0.5): the run's randomness source.
    /// Checkpoint ids (and the run id, when the executor creates the
    /// journal) are minted through it. `None` (the default) is OS entropy,
    /// byte-identical to pre-R0.5 behavior; [`RngSource::Seeded`] makes the
    /// id stream reproducible.
    pub rng: Option<RngSource>,

    /// Flight Recorder (R0.5): attach a pre-built journal for this run. Node
    /// closures capture a clone of the same [`Journal`] to record their own
    /// model/tool/remote/WASM calls into the run's evidence (the journal
    /// stamps sequence numbers and timestamps itself). When `None`, the
    /// executor creates a fresh journal per run; either way the run's
    /// journal is retrievable afterwards via [`Executor::journal`].
    pub journal: Option<Journal>,

    /// Flight Recorder (R0.5): the executor policy active for this run,
    /// stamped into every checkpoint header. `None` records the static
    /// default ([`PolicyVersion::STATIC_V0`]) — except on resume (R0.8 wave
    /// 4), where the run inherits the version its checkpoint header pins so
    /// an in-flight run is immune to mid-run promotions. An explicit pin
    /// always wins over inheritance.
    pub policy_version: Option<PolicyVersion>,

    /// Flight Recorder (R0.5): the application's own graph version string,
    /// stamped into every checkpoint header next to the topology hash.
    /// `None` records `"unversioned"`. Bump it when node bodies change in
    /// ways the topology hash cannot see.
    pub graph_version: Option<String>,

    /// Cooperative cancellation (R0.6 wave 2c, drain). When set, the
    /// executor checks the token **at every super-step boundary** and, once
    /// it is cancelled, stops the run before starting the next super-step,
    /// returning [`RustyError::Cancelled`].
    ///
    /// The boundary is the only cancellation point, deliberately: a
    /// super-step is transactional (its nodes run off an immutable snapshot
    /// and its writes merge atomically), and its boundary checkpoint is
    /// already persisted by the time the token is observed — so a cancelled
    /// run resumes from exactly where it stopped, with no torn step and no
    /// lost writes. Cancellation granularity is therefore one super-step;
    /// work inside the in-flight step runs to its barrier. This is the hook
    /// graceful shutdown uses: a draining server cancels the token and every
    /// in-flight run parks itself at a resumable checkpoint instead of being
    /// torn down mid-step.
    pub cancellation: Option<CancellationToken>,

    /// Effect kernel v2 (R0.7): the versioned run manifest — prompts, tool
    /// schemas, model and parameters, memory schema, capsule versions the run
    /// pins. Stamped into every checkpoint header so a resumed run keeps
    /// executing against its pinned versions (see
    /// [`crate::record::RunManifest`] for the upgrade-safety contract).
    /// `None` (the default) pins nothing and leaves checkpoint bytes
    /// byte-identical to R0.5/R0.6.
    pub manifest: Option<RunManifest>,

    /// Effect kernel v2 (R0.7): the approval tokens this run carries for its
    /// irreversible effects (see [`crate::effects::ApprovalToken`]). The
    /// executor consults them when [`Executor::with_effect_admission`] has
    /// enabled the guarded tool path. Empty by default; an irreversible call
    /// dispatched through that path then fails admission before its body
    /// runs.
    pub effect_approvals: Vec<crate::effects::ApprovalToken>,

    /// Shadow runs (R0.12 wave 4): an explicit admission context for this
    /// run, overriding the one the executor would otherwise build from
    /// [`Executor::with_effect_admission`]. A shadow deployment builds a
    /// context whose boundary refuses every effect above read-only and serves
    /// refused calls from the recorded world (see
    /// [`crate::effects::EffectAdmissionContext::shadow`]); injecting it here
    /// is what makes the run execute against that boundary instead of the
    /// production one. `None` (the default) is byte-identical to prior
    /// behavior.
    pub effect_admission: Option<EffectAdmissionContext>,
}

impl Default for RunConfig {
    /// A config with an empty `thread_id` and the default step limit —
    /// identical to `RunConfig::new("")`. Derived `Default` would zero
    /// `max_steps`, so any `default()`-built run would instantly trip the
    /// step guard; keep this impl in sync with [`RunConfig::new`].
    fn default() -> Self {
        Self::new(String::new())
    }
}

impl RunConfig {
    /// A config for `thread_id` with the default step limit.
    pub fn new(thread_id: impl Into<String>) -> Self {
        Self {
            thread_id: thread_id.into(),
            max_steps: DEFAULT_MAX_STEPS,
            resume: None,
            checkpoint_id: None,
            event_tx: None,
            clock: None,
            rng: None,
            journal: None,
            policy_version: None,
            graph_version: None,
            cancellation: None,
            manifest: None,
            effect_approvals: Vec::new(),
            effect_admission: None,
        }
    }

    /// Builder-style: override the step limit.
    pub fn with_max_steps(mut self, max_steps: usize) -> Self {
        self.max_steps = max_steps;
        self
    }

    /// Builder-style: set the resume value.
    pub fn with_resume(mut self, value: Value) -> Self {
        self.resume = Some(value);
        self
    }

    /// Builder-style: replay from a specific checkpoint of `thread_id`
    /// (time travel). See the [`RunConfig::checkpoint_id`] field docs for
    /// semantics and the fork-first safe pattern.
    pub fn with_checkpoint_id(mut self, checkpoint_id: impl Into<String>) -> Self {
        self.checkpoint_id = Some(checkpoint_id.into());
        self
    }

    /// Builder-style: attach a streaming event sink.
    pub fn with_event_tx(mut self, tx: mpsc::Sender<GraphEvent>) -> Self {
        self.event_tx = Some(tx);
        self
    }

    /// Builder-style: source the run's time from `clock` (Flight Recorder
    /// determinism seam; see the [`RunConfig::clock`] field docs).
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = Some(clock);
        self
    }

    /// Builder-style: source the run's randomness from `rng` (Flight
    /// Recorder determinism seam; see the [`RunConfig::rng`] field docs).
    pub fn with_rng(mut self, rng: RngSource) -> Self {
        self.rng = Some(rng);
        self
    }

    /// Builder-style: record the run into `journal` (Flight Recorder; see
    /// the [`RunConfig::journal`] field docs). When the config carries no
    /// explicit [`RunConfig::clock`], the executor also reads time from the
    /// attached journal's clock, keeping one time source per run.
    pub fn with_journal(mut self, journal: Journal) -> Self {
        self.journal = Some(journal);
        self
    }

    /// Builder-style: stamp the active executor policy version into every
    /// checkpoint header (see the [`RunConfig::policy_version`] field docs).
    pub fn with_policy_version(mut self, version: PolicyVersion) -> Self {
        self.policy_version = Some(version);
        self
    }

    /// Builder-style: stamp the application's graph version into every
    /// checkpoint header (see the [`RunConfig::graph_version`] field docs).
    pub fn with_graph_version(mut self, version: impl Into<String>) -> Self {
        self.graph_version = Some(version.into());
        self
    }

    /// Builder-style: make the run cooperatively cancellable (see the
    /// [`RunConfig::cancellation`] field docs). The token is observed only at
    /// super-step boundaries: cancelling it stops the run *after* the
    /// in-flight step's barrier and boundary checkpoint, so the run is left
    /// resumable from exactly that checkpoint with [`RustyError::Cancelled`].
    pub fn with_cancellation(mut self, token: CancellationToken) -> Self {
        self.cancellation = Some(token);
        self
    }

    /// Builder-style: pin the run's versioned manifest into every checkpoint
    /// header (see the [`RunConfig::manifest`] field docs).
    pub fn with_manifest(mut self, manifest: RunManifest) -> Self {
        self.manifest = Some(manifest);
        self
    }

    /// Builder-style: carry approval tokens for the run's irreversible
    /// effects (see the [`RunConfig::effect_approvals`] field docs).
    pub fn with_effect_approvals(
        mut self,
        approvals: impl IntoIterator<Item = crate::effects::ApprovalToken>,
    ) -> Self {
        self.effect_approvals = approvals.into_iter().collect();
        self
    }

    /// Builder-style: inject an explicit admission context for this run (see
    /// the [`RunConfig::effect_admission`] field docs). Shadow deployments
    /// use it to run the graph against the shadow boundary instead of the
    /// production one.
    pub fn with_effect_admission_context(mut self, context: EffectAdmissionContext) -> Self {
        self.effect_admission = Some(context);
        self
    }

    /// A clone of the event sink sender, for wiring into nodes that stream
    /// [`GraphEvent::Token`] deltas (LangGraph's `messages` stream mode).
    ///
    /// Typical flow: create the channel, call `config.token_tx()` to obtain
    /// the clone a node closure captures, then hand `config` to
    /// [`Executor::run`]. `None` when no sink is attached. See the
    /// [`crate::llm::ChatModel`] rustdoc for the full pattern.
    pub fn token_tx(&self) -> Option<mpsc::Sender<GraphEvent>> {
        self.event_tx.clone()
    }
}

/// Default super-step limit. Deliberately far above LangGraph's default
/// `recursion_limit` of 25: ReAct-style loops burn one super-step per
/// agent/tool hop, so long tool chains legitimately exceed 25.
pub const DEFAULT_MAX_STEPS: usize = 1000;

/// Streaming events emitted by the executor during a run. All of LangGraph's
/// stream modes are views over this single typed event stream.
///
/// The enum is serializable so event streams can cross process / FFI
/// boundaries (e.g. a WebSocket bridge or a persisted event log).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GraphEvent {
    /// A node began executing.
    NodeStart {
        /// Node name.
        node: String,
        /// Super-step index.
        step: usize,
    },
    /// A node finished executing (successfully).
    NodeEnd {
        /// Node name.
        node: String,
        /// Super-step index.
        step: usize,
    },
    /// A single LLM token delta (the LangGraph `messages` stream mode).
    ///
    /// The executor itself never emits this variant: tokens originate inside
    /// nodes that call [`crate::llm::ChatModel::chat_stream`] and forward
    /// each [`crate::llm::TokenChunk`] into the run's event channel (see the
    /// `ChatModel` rustdoc for the wiring pattern and
    /// [`RunConfig::token_tx`] / [`Executor::token_tx`] for sender handles).
    Token {
        /// Node that produced the token.
        node: String,
        /// Incremental text produced since the previous token.
        delta: String,
    },
    /// State was updated at a super-step barrier (`updates` stream mode).
    StateUpdate {
        /// Super-step index at which the update was applied.
        step: usize,
        /// Every channel written in this step mapped to its **post-reducer**
        /// value (e.g. the full appended list for an `Append` channel), read
        /// back from the merged state — not the raw per-node partials.
        updates: serde_json::Map<String, Value>,
    },
    /// A checkpoint was persisted at a super-step boundary.
    CheckpointSaved {
        /// The checkpoint id.
        checkpoint_id: String,
        /// Super-step index at the boundary.
        step: usize,
    },
    /// A super-step began; lists the nodes activated in it.
    SuperStep {
        /// Super-step index.
        step: usize,
        /// Nodes active in this step.
        active_nodes: Vec<String>,
    },
}

/// The graph executor. Holds an optional checkpointer and an optional
/// middleware chain; stateless with respect to individual runs, so one
/// `Executor` can drive many concurrent runs (each with its own
/// `thread_id`).
#[derive(Default)]
pub struct Executor {
    checkpointer: Option<Arc<dyn Checkpointer>>,
    token_tx: Option<mpsc::Sender<GraphEvent>>,
    // The most recent run's Flight Recorder journal. Interior mutability
    // keeps `run` taking `&self`; overwritten at the start of every run.
    journal: Mutex<Option<Journal>>,
    // Middleware/Interceptor SDK: ordered layers wrapping every node
    // invocation (and, via `NodeContext::middleware`, the tool/model calls
    // node code makes). An empty chain takes the original dispatch path.
    middleware: MiddlewareChain,
    // R0.8 wave 1: the governed-memory source this executor's runs answer
    // memory reads from (live store or replay cursor). The executor never
    // touches memory itself this wave — the full run-loop integration is a
    // later wave; this is the installation point node factories use (see
    // `Executor::memory`), mirroring `with_token_tx`.
    memory_source: Option<crate::memory::MemorySource>,
    // Present only when guarded tool effect admission is enabled. The run's
    // thread id and approval tokens are combined with these rollback handlers
    // into an EffectAdmissionContext for each node invocation.
    effect_compensations: Option<CompensationRegistry>,
}

impl Executor {
    /// An executor without persistence (runs cannot be interrupted/resumed
    /// durably; interrupts will still surface but resume requires a
    /// checkpointer).
    pub fn new() -> Self {
        Self::default()
    }

    /// An executor persisting checkpoints through `checkpointer`.
    pub fn with_checkpointer(checkpointer: Arc<dyn Checkpointer>) -> Self {
        Self {
            checkpointer: Some(checkpointer),
            ..Self::default()
        }
    }

    /// Builder-style: attach a middleware layer (Middleware/Interceptor
    /// SDK). Layers compose in registration order — before-hooks run in
    /// `.layer()` order on the way into a node/model/tool call, after-hooks
    /// in reverse order on the way out. See [`crate::middleware`].
    pub fn layer<M: Middleware + 'static>(mut self, middleware: M) -> Self {
        self.middleware.push(Arc::new(middleware));
        self
    }

    /// Builder-style: attach a pre-shared middleware layer.
    pub fn layer_shared(mut self, middleware: Arc<dyn Middleware>) -> Self {
        self.middleware.push(middleware);
        self
    }

    /// The attached middleware chain (empty when no layers were added).
    pub fn middleware(&self) -> &MiddlewareChain {
        &self.middleware
    }

    /// Builder-style: make guarded tool effect admission available to nodes.
    ///
    /// The supplied registry may be empty. Pure and read-only calls remain
    /// automatic; idempotent calls require a key, compensatable calls require
    /// a handler in this registry, and non-idempotent calls require a matching
    /// token in [`RunConfig::effect_approvals`]. The prebuilt ReAct tools node
    /// enforces this automatically. Custom nodes must pass
    /// [`NodeContext::effect_admission`] to the [`crate::tool::ToolExecutor`]
    /// they construct; direct [`crate::tool::Tool::call`] invocations are not
    /// intercepted by the executor.
    pub fn with_effect_admission(mut self, compensations: CompensationRegistry) -> Self {
        self.effect_compensations = Some(compensations);
        self
    }

    /// Whether this executor enforces the effect admission boundary.
    pub fn effect_admission_enabled(&self) -> bool {
        self.effect_compensations.is_some()
    }

    /// Builder-style: hold a token broadcast sender that nodes can clone to
    /// publish [`GraphEvent::Token`] deltas (LangGraph's `messages` stream
    /// mode).
    ///
    /// The executor never emits `Token` events itself — tokens originate in
    /// nodes calling [`crate::llm::ChatModel::chat_stream`]. This is a
    /// convenience handle so node factories built around an `Executor` can
    /// fetch the sink via [`Executor::token_tx`] and capture a clone in each
    /// node closure. When the same channel should also receive the
    /// executor's own events, attach it to the run via
    /// [`RunConfig::with_event_tx`] instead (or as well).
    pub fn with_token_tx(mut self, token_tx: mpsc::Sender<GraphEvent>) -> Self {
        self.token_tx = Some(token_tx);
        self
    }

    /// The token broadcast sender, if one was attached via
    /// [`Executor::with_token_tx`]. Clone it into node closures to stream
    /// [`GraphEvent::Token`]s from within nodes.
    pub fn token_tx(&self) -> Option<&mpsc::Sender<GraphEvent>> {
        self.token_tx.as_ref()
    }

    /// Builder-style: install the governed-memory source this executor's
    /// runs answer memory reads from (R0.8 wave 1; see
    /// [`crate::memory::MemorySource`]).
    ///
    /// The executor never reads or writes memory itself this wave — the
    /// full run-loop integration is a later wave. This is the seam's
    /// installation point, mirroring [`Executor::with_token_tx`]: node
    /// factories built around an `Executor` fetch the journaled handle via
    /// [`Executor::memory`] once the run has started and capture a clone in
    /// each node closure. Nodes may equally bind a source to an attached
    /// journal directly ([`crate::journal::Journal::memory`]); both paths
    /// journal into the same run evidence.
    pub fn with_memory_source(mut self, source: crate::memory::MemorySource) -> Self {
        self.memory_source = Some(source);
        self
    }

    /// The configured memory source, if one was installed via
    /// [`Executor::with_memory_source`].
    pub fn memory_source(&self) -> Option<&crate::memory::MemorySource> {
        self.memory_source.as_ref()
    }

    /// The journaled memory handle for the most recent run: the run's
    /// journal (set at run start) bound to the configured source. `None`
    /// before the first run or when no source is installed. The same
    /// most-recent-run caveat as [`Executor::journal`] applies: driving
    /// several runs concurrently through one `Executor` leaves this handle
    /// pointing at whichever run started last — bind per-run handles via
    /// [`crate::journal::Journal::memory`] when runs overlap.
    pub fn memory(&self) -> Option<crate::memory::JournaledMemory> {
        let journal = self.journal()?;
        let source = self.memory_source.clone()?;
        Some(crate::memory::JournaledMemory::new(&journal, source))
    }

    /// The configured checkpointer, if any. Shared (not consumed) so one
    /// `Executor` can drive many concurrent runs over the same store.
    pub fn checkpointer(&self) -> Option<&Arc<dyn Checkpointer>> {
        self.checkpointer.as_ref()
    }

    /// The Flight Recorder journal of the most recent run started through
    /// this executor — the one attached via [`RunConfig::with_journal`], or
    /// the fresh journal the executor created for the run. `None` before the
    /// first run.
    ///
    /// The journal is set at run start, so the evidence of a failed or
    /// suspended run is retrievable exactly like a completed run's. Note the
    /// executor is deliberately stateless across runs: driving several runs
    /// concurrently through one `Executor` leaves this handle pointing at
    /// whichever run started last — attach per-run journals via the config
    /// when runs overlap.
    pub fn journal(&self) -> Option<Journal> {
        self.journal
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Run a compiled graph to completion (or interruption).
    ///
    /// - `graph`: the compiled, frozen graph topology.
    /// - `spec`: the state schema (channels + reducers) used to merge node
    ///   updates at each barrier.
    /// - `initial_state`: the starting state. When `config.resume` is set and
    ///   a checkpoint exists for `config.thread_id`, the checkpointed state
    ///   and next-node set take precedence over this argument. When
    ///   `config.checkpoint_id` is set, that specific checkpoint (rather than
    ///   the latest) is restored — replay/time travel; forking into a fresh
    ///   thread first via
    ///   [`crate::checkpoint::Checkpointer::fork_thread`] is the safe pattern,
    ///   since replaying on the same thread appends new history.
    /// - `config`: run configuration (thread id, step limit, resume value,
    ///   streaming sink).
    ///
    /// # Super-step semantics
    ///
    /// Each loop iteration runs one super-step as a transaction: the active
    /// nodes execute in parallel over an immutable start-of-step snapshot;
    /// the barrier discards the whole step's writes on any node failure and
    /// suspends the run on an interrupt; only then are writes merged via the
    /// channel reducers, routing computed against the post-barrier state,
    /// and a boundary checkpoint persisted. The module-level docs enumerate
    /// the six phases; `execute_super_step` is the implementation.
    ///
    /// The loop returns [`ExecutionOutcome::Done`] when routing yields an
    /// empty next set, [`ExecutionOutcome::Interrupted`] when a node
    /// interrupts, an [`RustyError::Graph`] error once `config.max_steps`
    /// super-steps have run without termination, and
    /// [`RustyError::Cancelled`] when [`RunConfig::cancellation`] is
    /// observed cancelled at a super-step boundary (the boundary checkpoint
    /// is intact; the run resumes from it).
    pub async fn run(
        &self,
        graph: &Graph,
        spec: &StateSpec,
        initial_state: State,
        config: RunConfig,
    ) -> Result<ExecutionOutcome> {
        // Run-level span: every super-step, node, and checkpoint trace in the
        // run attaches to it. Attached via `.instrument()` (never `.enter()`)
        // so no span guard is held across `.await` points and the returned
        // future stays `Send`.
        let run_span = tracing::info_span!(
            "rusty.run",
            thread_id = %config.thread_id,
            max_steps = config.max_steps,
            resume = config.resume.is_some(),
            replay = config.checkpoint_id.is_some(),
        );
        self.run_inner(graph, spec, initial_state, config)
            .instrument(run_span)
            .await
    }

    /// The instrumented body of [`Executor::run`]; see that method's docs for
    /// the super-step algorithm.
    async fn run_inner(
        &self,
        graph: &Graph,
        spec: &StateSpec,
        initial_state: State,
        config: RunConfig,
    ) -> Result<ExecutionOutcome> {
        let started = std::time::Instant::now();

        // ---- flight recorder setup ----
        //
        // Resolve the determinism seams and the run's journal before any
        // checkpoint work: every timestamp and id below flows through them.
        // An attached journal carries its own identity and clock; otherwise
        // the executor mints a run id from the (possibly seeded) RNG and a
        // journal over the configured clock. The run reads time from the
        // journal's clock when no explicit clock is configured, keeping one
        // time source per run.
        let rng = config.rng.clone().unwrap_or_default();
        let journal = match &config.journal {
            Some(attached) => attached.clone(),
            None => Journal::new(
                rng.uuid_string(),
                config.thread_id.clone(),
                config.clock.clone().unwrap_or_default(),
            ),
        };
        let mut recorder = Recorder {
            clock: config
                .clock
                .clone()
                .unwrap_or_else(|| journal.clock().clone()),
            rng,
            graph_version: config
                .graph_version
                .clone()
                .unwrap_or_else(|| "unversioned".to_owned()),
            graph_hash: graph.topology_hash(),
            policy_version: config.policy_version.clone().unwrap_or_default(),
            manifest: config.manifest.clone(),
            journal: journal.clone(),
        };
        // Publish before the loop: evidence of a failed or suspended run is
        // retrievable exactly like a completed run's.
        *self.journal.lock().unwrap_or_else(|e| e.into_inner()) = Some(journal);

        // ---- initialization / resume ----
        //
        // On resume the checkpointed state and next-node set take precedence
        // over `initial_state`; the resume value is delivered to the first
        // super-step (whose active set is the checkpointed next-node set —
        // after an interrupt, every node of the suspended step) via
        // `NodeContext::resume_value()`.
        //
        // Time travel: when `config.checkpoint_id` is set, THAT checkpoint is
        // restored instead of the latest — this is replay from an arbitrary
        // history point. The two knobs compose: `checkpoint_id` selects WHERE
        // the run restarts, `resume` (when also set) supplies the resume value
        // for the first super-step.
        let mut state = initial_state;
        let mut active: Vec<ActiveTask>;
        let mut step: usize = 0;
        let mut pending_resume: Option<Value> = None;
        // Causal parent for the first super-step: the resume event on a
        // resumed run, nothing on a fresh one.
        let mut step_parent: Option<String> = None;

        if config.checkpoint_id.is_some() || config.resume.is_some() {
            let checkpointer = self.checkpointer.as_ref().ok_or_else(|| {
                RustyError::Checkpoint(
                    "RunConfig.checkpoint_id/resume is set but no checkpointer is configured \
                     on the executor"
                        .into(),
                )
            })?;
            let checkpoint = match &config.checkpoint_id {
                Some(id) => checkpointer
                    .get_by_id(&config.thread_id, id)
                    .await?
                    .ok_or_else(|| {
                        RustyError::Checkpoint(format!(
                            "cannot replay thread `{}`: checkpoint `{id}` not found",
                            config.thread_id
                        ))
                    })?,
                None => checkpointer
                    .get_latest(&config.thread_id)
                    .await?
                    .ok_or_else(|| {
                        RustyError::Checkpoint(format!(
                            "cannot resume thread `{}`: no checkpoint found",
                            config.thread_id
                        ))
                    })?,
            };
            // Policy-binding continuity (R0.8 wave 4): a resumed run keeps
            // the policy version its checkpoint header pins, so a mid-run
            // promotion never changes behavior under an in-flight execution.
            // An explicit `RunConfig::with_policy_version` pin wins over
            // inheritance — the caller asked for that version deliberately.
            if config.policy_version.is_none() {
                recorder.policy_version = checkpoint.header.policy_version.clone();
            }
            step_parent = Some(recorder.record(
                EventDraft::new(RunEventKind::Resume, Effect::Pure).input(serde_json::json!({
                    "checkpoint_id": checkpoint.id,
                    "step": checkpoint.step,
                    "resume": config.resume.clone().unwrap_or(Value::Null),
                })),
            ));
            state = checkpoint.state;
            step = checkpoint.step;
            active = checkpoint
                .next_nodes
                .into_iter()
                .map(|name| ActiveTask { name, scoped: None })
                .collect();
            pending_resume = config.resume.clone();
            if active.is_empty() {
                tracing::info!(
                    steps = 0,
                    duration_ms = started.elapsed().as_millis() as u64,
                    "run complete"
                );
                return Ok(ExecutionOutcome::Done(state));
            }
        } else {
            active = vec![ActiveTask {
                name: graph.entry_point().to_owned(),
                scoped: None,
            }];
        }

        // One shared context for the whole run: clones handed to parallel
        // nodes share its approval ledger, and later super-steps observe
        // tokens already consumed by earlier calls. An explicitly injected
        // context (R0.12 shadow runs) wins over the production one derived
        // from the executor's compensation registry.
        let effect_admission = config.effect_admission.clone().or_else(|| {
            self.effect_compensations.as_ref().map(|compensations| {
                EffectAdmissionContext::new(config.thread_id.clone())
                    .with_approvals(config.effect_approvals.clone())
                    .with_compensations(compensations.clone())
            })
        });

        // ---- super-step loop ----
        let mut steps_run: usize = 0;
        loop {
            if steps_run >= config.max_steps {
                return Err(RustyError::Graph(format!(
                    "max_steps ({}) exceeded: the graph did not terminate within the step \
                     budget (possible infinite cycle; raise RunConfig::max_steps or add a \
                     terminating route)",
                    config.max_steps
                )));
            }
            // Cooperative cancellation (drain): observed only here, at a
            // super-step boundary. The last step's barrier merged cleanly
            // and its boundary checkpoint is already persisted, so
            // cancelling never tears a step — the run resumes from exactly
            // this point. Nodes of the in-flight step always run to their
            // barrier; cancellation granularity is one super-step.
            if config
                .cancellation
                .as_ref()
                .is_some_and(CancellationToken::is_cancelled)
            {
                tracing::warn!(
                    steps_run,
                    "run cancelled at a super-step boundary; resumable from the last checkpoint"
                );
                return Err(RustyError::Cancelled(format!(
                    "cancelled after {steps_run} super-step(s); the boundary checkpoint of \
                     thread `{}` is intact, so re-running the thread resumes from there",
                    config.thread_id
                )));
            }

            // The step body runs in a dedicated method so the whole body is
            // one instrumented future under the per-step span.
            let step_span =
                tracing::debug_span!("rusty.super_step", step = step, active_nodes = active.len(),);

            let transition = self
                .execute_super_step(
                    graph,
                    spec,
                    &config,
                    &recorder,
                    effect_admission.as_ref(),
                    &mut state,
                    &active,
                    step,
                    &mut pending_resume,
                    step_parent.clone(),
                )
                .instrument(step_span)
                .await?;

            match transition {
                StepTransition::Next(next, route_event) => {
                    active = next;
                    step += 1;
                    steps_run += 1;
                    step_parent = Some(route_event);
                }
                StepTransition::Finish(outcome) => {
                    if !outcome.is_interrupted() {
                        tracing::info!(
                            steps = steps_run + 1,
                            duration_ms = started.elapsed().as_millis() as u64,
                            "run complete"
                        );
                    }
                    return Ok(outcome);
                }
            }
        }
    }

    /// Executes one super-step: plan -> compute -> barrier -> merge -> route
    /// -> boundary checkpoint. Returns the next active set, or
    /// [`StepTransition::Finish`] with the terminal outcome when the run ends
    /// (`Done`) or suspends (`Interrupted`).
    ///
    /// Flight Recorder: every phase transition is journaled through
    /// `recorder` — super-step start/end, one node input/output pair per
    /// invocation (in deterministic active-set order, not finish order), the
    /// routing decision, and the checkpoint write. Node events carry the
    /// node's declared [`Effect`] classification.
    #[allow(clippy::too_many_arguments)]
    async fn execute_super_step(
        &self,
        graph: &Graph,
        spec: &StateSpec,
        config: &RunConfig,
        recorder: &Recorder,
        effect_admission: Option<&EffectAdmissionContext>,
        state: &mut State,
        active: &[ActiveTask],
        step: usize,
        pending_resume: &mut Option<Value>,
        step_parent: Option<String>,
    ) -> Result<StepTransition> {
        // -- plan.
        let active_names: Vec<String> = active.iter().map(|t| t.name.clone()).collect();
        Self::emit(
            config,
            GraphEvent::SuperStep {
                step,
                active_nodes: active_names.clone(),
            },
        );
        let mut plan_draft =
            EventDraft::new(RunEventKind::SuperStepStart, Effect::Pure).input(serde_json::json!({
                "step": step,
                "active_nodes": active_names,
            }));
        if let Some(parent) = step_parent {
            plan_draft = plan_draft.parent(parent);
        }
        let step_start_event = recorder.record(plan_draft);

        // -- compute. Scoped (Send) state is overlaid onto each invocation's
        //    private copy of the start-of-step snapshot, so fan-out items
        //    never collide in the shared state.
        let snapshot = state.clone();
        let effect_admission = effect_admission.cloned();
        let mut join_set: JoinSet<(usize, String, Result<NodeOutput>, u64)> = JoinSet::new();
        // Per-invocation journal metadata, aligned with `active`: the input
        // event id (causal parent of the matching output) and the node's
        // declared effect class. Fan-out invocations of the same node each
        // get their own entry — index, not name, is the identity.
        let mut input_events: Vec<String> = Vec::with_capacity(active.len());
        let mut invocation_effects: Vec<Effect> = Vec::with_capacity(active.len());

        for (index, task) in active.iter().enumerate() {
            let node = graph.node(&task.name).ok_or_else(|| {
                RustyError::Graph(format!("routing activated unknown node `{}`", task.name))
            })?;

            let mut node_state = snapshot.clone();
            if let Some(scoped) = &task.scoped {
                match scoped {
                    Value::Object(map) => {
                        for (channel, value) in map {
                            node_state.insert(channel.clone(), value.clone());
                        }
                    }
                    other => {
                        return Err(RustyError::InvalidUpdate(format!(
                            "Send scoped state for node `{}` must be a JSON object, \
                             got {other}",
                            task.name
                        )));
                    }
                }
            }

            let input_event = recorder.record(
                EventDraft::new(RunEventKind::NodeInput, node.effect())
                    .node(task.name.clone())
                    .input(node_state.to_value())
                    .parent(step_start_event.clone()),
            );
            invocation_effects.push(node.effect());

            let node_config = NodeConfig {
                thread_id: config.thread_id.clone(),
                step,
                resume: pending_resume.clone(),
                // Hand the invocation its own journal event id so node
                // code can parent the effects it records (model/tool
                // calls) to this invocation.
                extra: HashMap::from([(
                    crate::journal::PARENT_EVENT_KEY.to_owned(),
                    Value::String(input_event.clone()),
                )]),
            };
            input_events.push(input_event);
            let name = task.name.clone();
            Self::emit(
                config,
                GraphEvent::NodeStart {
                    node: name.clone(),
                    step,
                },
            );
            // A JoinSet polls tasks independently of the spawning task's
            // context, so the per-node span is attached to each spawned
            // future explicitly via `.instrument()`.
            let node_span = tracing::info_span!("rusty.node", node = %name, step = step);
            // Latency is read through the run's clock seam — under a logical
            // clock the value is reproducible; under the system clock it is
            // the real elapsed time.
            let clock = recorder.clock.clone();
            let chain = self.middleware.clone();
            let effect_admission = effect_admission.clone();
            join_set.spawn(
                async move {
                    let node_started = clock.now();
                    // Middleware chain: with layers attached, the invocation
                    // runs inside the onion — before-hooks may mutate the
                    // snapshot, reject the run, or short-circuit with a
                    // substitute output; after-hooks unwind over the result.
                    // No layers: the original dispatch, byte-identical.
                    let result = if chain.is_empty() {
                        node.run(
                            NodeContext::new(node_state, node_config)
                                .with_optional_effect_admission(effect_admission),
                        )
                        .await
                    } else {
                        let mut call = NodeCall::new(
                            node_config.thread_id.clone(),
                            name.clone(),
                            step,
                            node_state,
                        );
                        chain
                            .run_node(&mut call, |call| {
                                let ctx = NodeContext::new(call.state().clone(), node_config)
                                    .with_middleware(chain.clone())
                                    .with_optional_effect_admission(effect_admission);
                                let node = Arc::clone(&node);
                                async move { node.run(ctx).await }
                            })
                            .await
                    };
                    let latency_ms = (clock.now() - node_started).num_milliseconds().max(0) as u64;
                    (index, name, result, latency_ms)
                }
                .instrument(node_span),
            );
        }
        // The resume value is consumed by the first super-step after a resume.
        *pending_resume = None;

        // -- barrier: collect every node result. The step is
        //    transactional: on any failure the JoinSet is dropped
        //    (aborting stragglers) and the step's writes are discarded.
        let mut writes: Vec<(String, HashMap<String, Value>)> = Vec::new();
        let mut commands: Vec<Command> = Vec::new();
        let mut ran_nodes: Vec<String> = Vec::new();
        let mut interrupted: Option<(usize, String, Value)> = None;
        // Journal payloads of finished invocations, in finish order here;
        // journaled in active-set order after the barrier.
        let mut completed: Vec<(usize, String, u64, Value)> = Vec::new();

        while let Some(joined) = join_set.join_next().await {
            let (index, name, result, latency_ms) = joined.map_err(|e| {
                RustyError::Node(format!(
                    "node task failed to join (panic or cancellation): {e}"
                ))
            })?;
            match result {
                Ok(output) => {
                    Self::emit(
                        config,
                        GraphEvent::NodeEnd {
                            node: name.clone(),
                            step,
                        },
                    );
                    completed.push((
                        index,
                        name.clone(),
                        latency_ms,
                        serde_json::json!({
                            "updates": &output.updates,
                            "command": &output.command,
                        }),
                    ));
                    if let Some(command) = output.command {
                        if !command.goto.is_empty() {
                            commands.push(command);
                        }
                    }
                    ran_nodes.push(name.clone());
                    writes.push((name, output.updates));
                }
                Err(RustyError::Interrupt { value }) => {
                    // Record the suspension and stop the barrier loop; the
                    // JoinSet is dropped below to abort stragglers.
                    interrupted = Some((index, name, value));
                    break;
                }
                Err(e) => {
                    // A failed node is still evidence: journal the failure
                    // before the error unwinds the run.
                    recorder.record(
                        EventDraft::new(RunEventKind::NodeOutput, invocation_effects[index])
                            .node(name.clone())
                            .status(EventStatus::Error)
                            .output(serde_json::json!({ "error": e.to_string() }))
                            .latency_ms(latency_ms)
                            .parent(input_events[index].clone()),
                    );
                    // LLM and tool failures are the transient, retryable
                    // error classes; everything else is a hard failure.
                    let retryable = matches!(e, RustyError::Llm(_) | RustyError::Tool(_));
                    tracing::warn!(
                        node = %name,
                        step = step,
                        error = %e,
                        retryable = retryable,
                        "node failed; super-step aborted and its writes discarded"
                    );
                    return Err(RustyError::Node(format!(
                        "node `{name}` failed at super-step {step}: {e}"
                    )));
                }
            }
        }

        if let Some((index, name, value)) = interrupted {
            // Suspend the run. The step is transactional, so no write of
            // this step survived — not even from siblings that completed
            // before the interrupt reached the barrier. The suspension
            // checkpoint therefore re-schedules the ENTIRE active set (the
            // interrupting node plus all siblings), otherwise completed
            // siblings' discarded writes would be silently lost and aborted
            // siblings would never re-run. Dropping the JoinSet first aborts
            // stragglers, preserving the transactional suspension point.
            drop(join_set);
            let interrupt_event = recorder.record(
                EventDraft::new(RunEventKind::Interrupt, invocation_effects[index])
                    .node(name.clone())
                    .input(value.clone())
                    .status(EventStatus::Interrupted)
                    .parent(input_events[index].clone()),
            );
            tracing::info!(
                node = %name,
                step = step,
                "node interrupted; run suspended (resumable via RunConfig::resume)"
            );
            let pending: Vec<String> = active.iter().map(|t| t.name.clone()).collect();
            let checkpoint =
                recorder.mint_checkpoint(config.thread_id.clone(), step, state.clone(), pending);
            let checkpoint_id = checkpoint.id.clone();
            if let Some(checkpointer) = &self.checkpointer {
                checkpointer.put(checkpoint).await?;
                recorder.record(
                    EventDraft::new(RunEventKind::CheckpointWritten, Effect::Idempotent)
                        .output(serde_json::json!({
                            "checkpoint_id": checkpoint_id,
                            "step": step,
                            "suspension": true,
                        }))
                        .parent(interrupt_event),
                );
                Self::emit(
                    config,
                    GraphEvent::CheckpointSaved {
                        checkpoint_id: checkpoint_id.clone(),
                        step,
                    },
                );
            }
            return Ok(StepTransition::Finish(ExecutionOutcome::Interrupted {
                value,
                state: state.clone(),
                checkpoint_id,
            }));
        }

        // Journal node outputs in active-set order, not JoinSet finish
        // order: the finish order is scheduling-dependent, and the journal's
        // sequence is evidence — replay compares it.
        completed.sort_by_key(|(index, ..)| *index);
        let mut last_output_event: Option<String> = None;
        for (index, name, latency_ms, output_json) in completed {
            last_output_event = Some(
                recorder.record(
                    EventDraft::new(RunEventKind::NodeOutput, invocation_effects[index])
                        .node(name)
                        .output(output_json)
                        .latency_ms(latency_ms)
                        .parent(input_events[index].clone()),
                ),
            );
        }

        // -- merge: reducers + LastValue single-write validation. On
        //    error the mutated state is dropped with the run
        //    (transactional super-step).
        //
        // The start-of-step snapshot's job ends at the barrier — every node
        // has reported (or the step has failed). Dropping it before the
        // merge releases its per-channel shared references, so channels no
        // checkpoint still shares reach the reducer with refcount 1 and
        // merge in place (W4 copy-on-write) instead of cloning.
        drop(snapshot);
        let written_channels: HashSet<String> = writes
            .iter()
            .flat_map(|(_, updates)| updates.keys().cloned())
            .collect();
        spec.apply_super_step(state, writes)?;
        // The event carries the post-reducer values read back out of the
        // merged state: when several nodes write the same channel in one
        // step (the normal Append fan-in case), reporting the raw partials
        // would keep only the last write and hide the rest.
        let mut merged_updates = serde_json::Map::new();
        for channel in &written_channels {
            if let Some(value) = state.get(channel) {
                merged_updates.insert(channel.clone(), value.clone());
            }
        }
        let channels_written: Vec<&str> = merged_updates.keys().map(String::as_str).collect();
        tracing::debug!(
            step = step,
            channels = ?channels_written,
            "merged node updates at super-step barrier"
        );
        // The super-step's reducer result is journaled even when empty —
        // "no channel changed" is evidence too.
        let step_end_event = recorder.record(
            EventDraft::new(RunEventKind::SuperStepEnd, Effect::Pure)
                .output(Value::Object(merged_updates.clone()))
                .parent(last_output_event.unwrap_or_else(|| step_start_event.clone())),
        );
        if !merged_updates.is_empty() {
            Self::emit(
                config,
                GraphEvent::StateUpdate {
                    step,
                    updates: merged_updates,
                },
            );
        }

        // -- route: Command::goto overrides the static edge set;
        //    otherwise evaluate outgoing edges of every node that ran
        //    against the post-barrier state.
        let mut next: Vec<ActiveTask> = Vec::new();
        let mut planned: HashSet<String> = HashSet::new();

        if !commands.is_empty() {
            for command in &commands {
                for target in &command.goto {
                    if !graph.has_node(target) {
                        return Err(RustyError::Graph(format!(
                            "Command::goto references unknown node `{target}`"
                        )));
                    }
                    if planned.insert(target.clone()) {
                        next.push(ActiveTask {
                            name: target.clone(),
                            scoped: None,
                        });
                    }
                }
            }
        } else {
            let mut evaluated: HashSet<String> = HashSet::new();
            for name in &ran_nodes {
                // Fan-out invocations of the same node share one edge set;
                // evaluate it once.
                if !evaluated.insert(name.clone()) {
                    continue;
                }
                for edge in graph.outgoing_edges(name) {
                    match edge {
                        Edge::Direct { to, .. } => {
                            if planned.insert(to.clone()) {
                                next.push(ActiveTask {
                                    name: to.clone(),
                                    scoped: None,
                                });
                            }
                        }
                        Edge::Conditional { router, .. } => {
                            match router(state.clone()).await? {
                                Route::Node(target) => {
                                    if !graph.has_node(&target) {
                                        return Err(RustyError::Graph(format!(
                                            "conditional router from `{name}` returned \
                                             unknown node `{target}`"
                                        )));
                                    }
                                    if planned.insert(target.clone()) {
                                        next.push(ActiveTask {
                                            name: target,
                                            scoped: None,
                                        });
                                    }
                                }
                                Route::Send(sends) => {
                                    for send in sends {
                                        if !graph.has_node(&send.node) {
                                            return Err(RustyError::Graph(format!(
                                                "Route::Send from `{name}` targets unknown \
                                                 node `{}`",
                                                send.node
                                            )));
                                        }
                                        // Each Send is its own invocation with its own
                                        // scoped state, even when several target the
                                        // same node.
                                        next.push(ActiveTask {
                                            name: send.node,
                                            scoped: Some(send.state),
                                        });
                                    }
                                }
                                Route::End => {}
                            }
                        }
                    }
                }
            }
        }

        let route_event = recorder.record(
            EventDraft::new(RunEventKind::RoutingDecision, Effect::Pure)
                .output(serde_json::json!({
                    "next": next
                        .iter()
                        .map(|t| serde_json::json!({
                            "node": &t.name,
                            "scoped": &t.scoped,
                        }))
                        .collect::<Vec<_>>(),
                    "goto": commands
                        .iter()
                        .flat_map(|command| command.goto.iter().cloned())
                        .collect::<Vec<String>>(),
                }))
                .parent(step_end_event.clone()),
        );

        // -- checkpoint at the super-step boundary.
        if let Some(checkpointer) = &self.checkpointer {
            let next_names: Vec<String> = next.iter().map(|t| t.name.clone()).collect();
            let checkpoint =
                recorder.mint_checkpoint(config.thread_id.clone(), step, state.clone(), next_names);
            let checkpoint_id = checkpoint.id.clone();
            checkpointer.put(checkpoint).await?;
            recorder.record(
                EventDraft::new(RunEventKind::CheckpointWritten, Effect::Idempotent)
                    .output(serde_json::json!({
                        "checkpoint_id": checkpoint_id,
                        "step": step,
                        "suspension": false,
                    }))
                    .parent(route_event.clone()),
            );
            Self::emit(
                config,
                GraphEvent::CheckpointSaved {
                    checkpoint_id,
                    step,
                },
            );
        }

        // -- terminate or schedule the next super-step.
        if next.is_empty() {
            return Ok(StepTransition::Finish(ExecutionOutcome::Done(
                state.clone(),
            )));
        }
        Ok(StepTransition::Next(next, route_event))
    }

    /// Best-effort event emission: a full or closed channel never aborts a run.
    fn emit(config: &RunConfig, event: GraphEvent) {
        if let Some(tx) = &config.event_tx {
            let _ = tx.try_send(event);
        }
    }
}

/// Per-run Flight Recorder state handed to every super-step: the journal,
/// the determinism seams, and the frozen provenance stamped into every
/// checkpoint of the run.
struct Recorder {
    journal: Journal,
    clock: Clock,
    rng: RngSource,
    graph_version: String,
    graph_hash: String,
    policy_version: PolicyVersion,
    manifest: Option<RunManifest>,
}

impl Recorder {
    /// Append one event to the run's journal; returns the event id (the
    /// causal-parent handle for whatever the event causes).
    fn record(&self, draft: EventDraft) -> String {
        self.journal.record(draft)
    }

    /// Mint a boundary checkpoint through the determinism seams: id from the
    /// run's RNG, timestamp from the run's clock, the frozen provenance
    /// header, and a journal reference pinning the evidence head as it stood
    /// *before* this checkpoint's own `checkpoint_written` event is
    /// recorded.
    fn mint_checkpoint(
        &self,
        thread_id: String,
        step: usize,
        state: State,
        next_nodes: Vec<String>,
    ) -> Checkpoint {
        Checkpoint {
            id: self.rng.uuid_string(),
            thread_id,
            step,
            state,
            next_nodes,
            created_at: self.clock.now(),
            header: CheckpointHeader {
                format_version: CURRENT_FORMAT_VERSION,
                graph_version: self.graph_version.clone(),
                graph_hash: self.graph_hash.clone(),
                policy_version: self.policy_version.clone(),
                logical_clock: self.clock.now_ms(),
                manifest: self.manifest.clone(),
            },
            journal_ref: Some(self.journal.head_ref()),
            // The executor always mints full snapshots; delta encoding is
            // decided inside the checkpointer (`checkpoint::encode_delta`).
            base: None,
        }
    }
}

/// One scheduled node invocation within a super-step. `scoped` carries the
/// per-invocation input of a [`crate::graph::Send`] fan-out, overlaid onto
/// that invocation's private state snapshot before the node runs.
struct ActiveTask {
    name: String,
    scoped: Option<Value>,
}

/// The control-flow result of a single super-step: either the next active
/// set (the loop continues) or the terminal run outcome (the loop breaks).
enum StepTransition {
    /// The next active set plus the routing-decision journal event that
    /// produced it (the causal parent of the next super-step's start event).
    Next(Vec<ActiveTask>, String),
    Finish(ExecutionOutcome),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkpoint::InMemoryCheckpointer;
    use crate::graph::GraphBuilder;
    use crate::llm::ChatModel;
    use crate::state::Reducer;
    use serde_json::json;
    use std::sync::Mutex;

    #[tokio::test]
    async fn linear_two_node_graph_executes_in_order() {
        let spec = StateSpec::new().channel("log", Reducer::Append);

        let mut builder = GraphBuilder::new();
        builder.add_node("first", |_ctx: NodeContext| async {
            Ok(NodeOutput::update("log", json!("first")))
        });
        builder.add_node("second", |ctx: NodeContext| async move {
            // The next super-step observes the previous step's merged writes.
            assert_eq!(ctx.state().get("log"), Some(&json!(["first"])));
            assert_eq!(ctx.step(), 1);
            Ok(NodeOutput::update("log", json!("second")))
        });
        builder.set_entry_point("first");
        builder.add_edge("first", "second");
        let graph = builder.compile().unwrap();

        let outcome = Executor::new()
            .run(&graph, &spec, State::new(), RunConfig::new("t-linear"))
            .await
            .unwrap();

        match outcome {
            ExecutionOutcome::Done(state) => {
                assert_eq!(state.get("log"), Some(&json!(["first", "second"])));
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn parallel_fan_in_merges_via_reducer() {
        let spec = StateSpec::new().channel("results", Reducer::Append);

        let mut builder = GraphBuilder::new();
        builder.add_node("start", |_ctx: NodeContext| async {
            Ok(NodeOutput::empty())
        });
        builder.add_node("worker_a", |ctx: NodeContext| async move {
            // Snapshot isolation: parallel workers cannot see each other.
            assert!(!ctx.state().contains("results"));
            Ok(NodeOutput::update("results", json!("a")))
        });
        builder.add_node("worker_b", |ctx: NodeContext| async move {
            assert!(!ctx.state().contains("results"));
            Ok(NodeOutput::update("results", json!("b")))
        });
        builder.set_entry_point("start");
        builder.add_edge("start", "worker_a");
        builder.add_edge("start", "worker_b");
        let graph = builder.compile().unwrap();

        let outcome = Executor::new()
            .run(&graph, &spec, State::new(), RunConfig::new("t-fan-in"))
            .await
            .unwrap();

        match outcome {
            ExecutionOutcome::Done(state) => {
                let results = state
                    .get("results")
                    .and_then(Value::as_array)
                    .expect("results channel must exist")
                    .clone();
                // Completion order across the JoinSet is nondeterministic.
                let mut items: Vec<&str> = results.iter().map(|v| v.as_str().unwrap()).collect();
                items.sort_unstable();
                assert_eq!(items, ["a", "b"]);
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn interrupt_returns_interrupted_outcome_and_resume_completes() {
        let spec = StateSpec::new().channel("answer", Reducer::Overwrite);

        let mut builder = GraphBuilder::new();
        builder.add_node("gate", |ctx: NodeContext| async move {
            match ctx.resume_value() {
                Some(v) => Ok(NodeOutput::update("answer", v.clone())),
                None => Err(ctx.interrupt(json!({"question": "approve?"}))),
            }
        });
        builder.set_entry_point("gate");
        let graph = builder.compile().unwrap();

        let checkpointer = Arc::new(InMemoryCheckpointer::new());
        let executor = Executor::with_checkpointer(checkpointer.clone());

        // First run: the gate node interrupts and the run suspends.
        let outcome = executor
            .run(&graph, &spec, State::new(), RunConfig::new("t-hitl"))
            .await
            .unwrap();

        let checkpoint_id = match outcome {
            ExecutionOutcome::Interrupted {
                value,
                checkpoint_id,
                ..
            } => {
                assert_eq!(value, json!({"question": "approve?"}));
                assert!(!checkpoint_id.is_empty());
                checkpoint_id
            }
            other => panic!("expected Interrupted, got {other:?}"),
        };

        // The suspension point was persisted and schedules the gate node.
        let stored = checkpointer.get_latest("t-hitl").await.unwrap().unwrap();
        assert_eq!(stored.id, checkpoint_id);
        assert_eq!(stored.next_nodes, vec!["gate".to_string()]);

        // Resume: the gate node re-runs with the resume value, writes its
        // answer, and the run terminates.
        let outcome = executor
            .run(
                &graph,
                &spec,
                State::new(),
                RunConfig::new("t-hitl").with_resume(json!(true)),
            )
            .await
            .unwrap();

        match outcome {
            ExecutionOutcome::Done(state) => {
                assert_eq!(state.get("answer"), Some(&json!(true)));
            }
            other => panic!("expected Done after resume, got {other:?}"),
        }
    }

    #[test]
    fn run_config_default_uses_default_step_limit() {
        // Regression: a derived `Default` would zero `max_steps`, making
        // every `RunConfig::default()` run fail immediately.
        let config = RunConfig::default();
        assert_eq!(config.max_steps, DEFAULT_MAX_STEPS);
        assert!(config.thread_id.is_empty());
        assert!(config.resume.is_none() && config.checkpoint_id.is_none());
    }

    #[tokio::test]
    async fn interrupt_reschedules_entire_active_set() {
        // Regression: the suspension checkpoint used to schedule only the
        // interrupting node, silently dropping parallel siblings — including
        // ones that had already completed, whose writes are discarded with
        // the aborted step.
        let spec = StateSpec::new()
            .channel("log", Reducer::Append)
            .channel("answer", Reducer::Overwrite);

        let mut builder = GraphBuilder::new();
        builder.add_node("start", |_ctx: NodeContext| async {
            Ok(NodeOutput::empty())
        });
        builder.add_node("gate", |ctx: NodeContext| async move {
            match ctx.resume_value() {
                Some(v) => Ok(NodeOutput::update("answer", v.clone())),
                None => Err(ctx.interrupt(json!({"question": "approve?"}))),
            }
        });
        // Completes immediately in the interrupted step; its write is
        // discarded with the step and must be recomputed on resume.
        builder.add_node("fast", |_ctx: NodeContext| async {
            Ok(NodeOutput::update("log", json!("fast")))
        });
        // Still in flight when the interrupt hits; aborted, re-run on resume.
        builder.add_node("slow", |ctx: NodeContext| async move {
            if ctx.resume_value().is_none() {
                tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            }
            Ok(NodeOutput::update("log", json!("slow")))
        });
        builder.set_entry_point("start");
        builder.add_edge("start", "gate");
        builder.add_edge("start", "slow");
        builder.add_edge("start", "fast");
        let graph = builder.compile().unwrap();

        let checkpointer = Arc::new(InMemoryCheckpointer::new());
        let executor = Executor::with_checkpointer(checkpointer.clone());

        let outcome = executor
            .run(&graph, &spec, State::new(), RunConfig::new("t-par-hitl"))
            .await
            .unwrap();
        match &outcome {
            ExecutionOutcome::Interrupted { state, .. } => {
                // Transactional suspension: fast's completed write was
                // discarded with the rest of the step.
                assert_eq!(state.get("log"), None);
            }
            other => panic!("expected Interrupted, got {other:?}"),
        }

        // The suspension checkpoint re-schedules every node of the step.
        let stored = checkpointer
            .get_latest("t-par-hitl")
            .await
            .unwrap()
            .unwrap();
        let mut scheduled = stored.next_nodes.clone();
        scheduled.sort_unstable();
        assert_eq!(scheduled, ["fast", "gate", "slow"]);

        // Resume: all three re-run (the resume value is broadcast to the
        // whole step); fast's write lands exactly once.
        let outcome = executor
            .run(
                &graph,
                &spec,
                State::new(),
                RunConfig::new("t-par-hitl").with_resume(json!(true)),
            )
            .await
            .unwrap();
        match outcome {
            ExecutionOutcome::Done(state) => {
                assert_eq!(state.get("answer"), Some(&json!(true)));
                let mut log: Vec<String> = state
                    .get("log")
                    .and_then(Value::as_array)
                    .expect("log channel must exist")
                    .iter()
                    .map(|v| v.as_str().unwrap().to_owned())
                    .collect();
                log.sort_unstable();
                assert_eq!(log, ["fast", "slow"]);
            }
            other => panic!("expected Done after resume, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn state_update_event_reports_post_reducer_values() {
        // Regression: with several writers on one channel, the event used to
        // carry raw per-node partials collapsed by last-write-wins, hiding
        // all but one write behind its documented "merged" contract.
        let spec = StateSpec::new().channel("results", Reducer::Append);

        let mut builder = GraphBuilder::new();
        builder.add_node("start", |_ctx: NodeContext| async {
            Ok(NodeOutput::empty())
        });
        builder.add_node("worker_a", |_ctx: NodeContext| async {
            Ok(NodeOutput::update("results", json!("a")))
        });
        builder.add_node("worker_b", |_ctx: NodeContext| async {
            Ok(NodeOutput::update("results", json!("b")))
        });
        builder.set_entry_point("start");
        builder.add_edge("start", "worker_a");
        builder.add_edge("start", "worker_b");
        let graph = builder.compile().unwrap();

        let (tx, mut rx) = mpsc::channel::<GraphEvent>(64);
        let outcome = Executor::new()
            .run(
                &graph,
                &spec,
                State::new(),
                RunConfig::new("t-event").with_event_tx(tx),
            )
            .await
            .unwrap();
        assert!(matches!(outcome, ExecutionOutcome::Done(_)));

        let mut merged: Option<Vec<String>> = None;
        while let Ok(event) = rx.try_recv() {
            if let GraphEvent::StateUpdate { step: 1, updates } = event {
                let values = updates
                    .get("results")
                    .and_then(Value::as_array)
                    .expect("StateUpdate must carry the results channel")
                    .iter()
                    .map(|v| v.as_str().unwrap().to_owned())
                    .collect();
                merged = Some(values);
            }
        }
        let mut merged = merged.expect("expected a StateUpdate event for step 1");
        merged.sort_unstable();
        // Both partial writes are visible in the single post-reducer value.
        assert_eq!(merged, ["a", "b"]);
    }

    #[tokio::test]
    async fn max_steps_guard_aborts_infinite_cycles() {
        let spec = StateSpec::new().channel("x", Reducer::Overwrite);
        let mut builder = GraphBuilder::new();
        builder.add_node("loop_node", |_ctx: NodeContext| async {
            Ok(NodeOutput::empty())
        });
        builder.set_entry_point("loop_node");
        builder.add_edge("loop_node", "loop_node");
        let graph = builder.compile().unwrap();

        let err = Executor::new()
            .run(
                &graph,
                &spec,
                State::new(),
                RunConfig::new("t-loop").with_max_steps(5),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, RustyError::Graph(_)));
    }

    #[test]
    fn token_event_serde_roundtrip() {
        let event = GraphEvent::Token {
            node: "agent".into(),
            delta: "Hello".into(),
        };
        let wire = serde_json::to_string(&event).unwrap();
        // Internally tagged: the variant name travels on the wire.
        assert!(
            wire.contains("\"type\":\"token\""),
            "unexpected wire: {wire}"
        );
        let back: GraphEvent = serde_json::from_str(&wire).unwrap();
        assert_eq!(event, back);

        // The other variants still roundtrip under the new serde derives.
        let step_event = GraphEvent::SuperStep {
            step: 3,
            active_nodes: vec!["a".into()],
        };
        let back: GraphEvent =
            serde_json::from_str(&serde_json::to_string(&step_event).unwrap()).unwrap();
        assert_eq!(step_event, back);
    }

    /// A mock model whose `chat_stream` override emits real multi-chunk
    /// deltas, proving the accumulation contract outside any HTTP client.
    struct StreamingMock;

    #[async_trait::async_trait]
    impl crate::llm::ChatModel for StreamingMock {
        async fn chat(
            &self,
            _messages: &[crate::llm::ChatMessage],
            _tools: &[Value],
        ) -> Result<crate::llm::ChatResponse> {
            Ok(crate::llm::ChatResponse {
                message: crate::llm::ChatMessage::assistant("Hello"),
                model: None,
                usage: None,
            })
        }

        async fn chat_stream(
            &self,
            messages: &[crate::llm::ChatMessage],
            tools: &[Value],
            on_token: &mut (dyn FnMut(crate::llm::TokenChunk) + Send),
        ) -> Result<crate::llm::ChatResponse> {
            for piece in ["Hel", "lo"] {
                on_token(crate::llm::TokenChunk {
                    delta: piece.into(),
                    finish: false,
                    raw: None,
                });
            }
            on_token(crate::llm::TokenChunk {
                delta: String::new(),
                finish: true,
                raw: None,
            });
            self.chat(messages, tools).await
        }
    }

    #[tokio::test]
    async fn node_streams_token_events_through_event_sink() {
        let spec = StateSpec::new().channel("answer", Reducer::Overwrite);

        let (tx, mut rx) = mpsc::channel::<GraphEvent>(64);
        // The wiring pattern from the ChatModel rustdoc: the node closure
        // captures a clone of the run's event sender and forwards chunks.
        let node_tx = tx.clone();

        let mut builder = GraphBuilder::new();
        builder.add_node("agent", move |_ctx: NodeContext| {
            let node_tx = node_tx.clone();
            async move {
                let model = StreamingMock;
                let mut full = String::new();
                model
                    .chat_stream(&[], &[], &mut |chunk| {
                        if !chunk.delta.is_empty() {
                            full.push_str(&chunk.delta);
                            let _ = node_tx.try_send(GraphEvent::Token {
                                node: "agent".into(),
                                delta: chunk.delta,
                            });
                        }
                    })
                    .await
                    .unwrap();
                Ok(NodeOutput::update("answer", json!(full)))
            }
        });
        builder.set_entry_point("agent");
        let graph = builder.compile().unwrap();

        let config = RunConfig::new("t-tokens").with_event_tx(tx);
        // The RunConfig helper hands out the same sender for node wiring.
        assert!(config.token_tx().is_some());
        // The Executor builder/accessor pair stores a broadcast handle.
        let executor = Executor::new().with_token_tx(config.token_tx().unwrap());
        assert!(executor.token_tx().is_some());

        let outcome = executor
            .run(&graph, &spec, State::new(), config)
            .await
            .unwrap();
        match outcome {
            ExecutionOutcome::Done(state) => {
                assert_eq!(state.get("answer"), Some(&json!("Hello")))
            }
            other => panic!("expected Done, got {other:?}"),
        }

        // Drain the event stream: token deltas interleave with executor events.
        let mut deltas = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let GraphEvent::Token { node, delta } = event {
                assert_eq!(node, "agent");
                deltas.push(delta);
            }
        }
        assert_eq!(deltas, ["Hel", "lo"]);
    }

    /// A minimal `tracing::Subscriber` that records formatted event fields
    /// (`name=value` pairs) into a shared buffer, so tests can assert on the
    /// executor's instrumentation. Implemented directly against the
    /// `tracing` crate's own `Subscriber` trait (re-exported from
    /// `tracing-core`) — no `tracing-subscriber` dependency required.
    #[derive(Clone, Default)]
    struct EventCapture {
        events: Arc<Mutex<Vec<String>>>,
    }

    /// Formats an event's fields as `"name=value "` pairs.
    struct FieldVisitor(String);

    impl tracing::field::Visit for FieldVisitor {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            use std::fmt::Write as _;
            let _ = write!(self.0, "{}={:?} ", field.name(), value);
        }
    }

    impl tracing::Subscriber for EventCapture {
        fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            // Spans are irrelevant to these assertions; one id serves all.
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            let mut visitor = FieldVisitor(String::new());
            event.record(&mut visitor);
            self.events.lock().unwrap().push(visitor.0);
        }

        fn enter(&self, _span: &tracing::span::Id) {}

        fn exit(&self, _span: &tracing::span::Id) {}
    }

    /// The instrumentation must be observability-only: installing a
    /// subscriber changes nothing about the run's outcome, and the expected
    /// telemetry (merge debug event, run-completion info event) is emitted.
    #[tokio::test]
    async fn instrumentation_emits_events_without_changing_outcome() {
        let capture = EventCapture::default();
        let events = capture.events.clone();
        // Global default subscriber, deliberately NOT a thread-local
        // `set_default`: callsite interest is cached process-wide and lazily
        // (re)registered from whichever thread first hits a callsite, so a
        // thread-local subscriber races with the other tests in this binary
        // that run graphs concurrently (they rebuild the cache against the
        // no-subscriber global default and our events get silently dropped).
        // A global default makes every thread's rebuild see this subscriber.
        // Setting it is additive — other tests neither set nor assert on
        // subscribers, and captured events from concurrent runs only help the
        // `any()` assertions below. `set_global_default` may only be called
        // once per process; this is the only test that installs a subscriber.
        tracing::subscriber::set_global_default(capture)
            .expect("no other test may install a global tracing subscriber");

        let spec = StateSpec::new().channel("log", Reducer::Append);
        let mut builder = GraphBuilder::new();
        builder.add_node("only", |_ctx: NodeContext| async {
            Ok(NodeOutput::update("log", json!("x")))
        });
        builder.set_entry_point("only");
        let graph = builder.compile().unwrap();

        let outcome = Executor::new()
            .run(&graph, &spec, State::new(), RunConfig::new("t-tracing"))
            .await
            .unwrap();

        // Identical semantics: the run completes with the expected state.
        match outcome {
            ExecutionOutcome::Done(state) => {
                assert_eq!(state.get("log"), Some(&json!(["x"])));
            }
            other => panic!("expected Done, got {other:?}"),
        }

        let captured = events.lock().unwrap();
        assert!(
            captured.iter().any(|e| e.contains("channels")),
            "expected a merge debug event listing written channels, got: {captured:?}"
        );
        assert!(
            captured
                .iter()
                .any(|e| e.contains("steps") && e.contains("duration_ms")),
            "expected a run-completion info event with steps and duration_ms, got: {captured:?}"
        );
    }

    /// A 3-node linear graph (`a -> b -> c`) appending each node name to the
    /// `log` channel.
    fn linear_three_node_graph() -> (Graph, StateSpec) {
        let spec = StateSpec::new().channel("log", Reducer::Append);
        let mut builder = GraphBuilder::new();
        for name in ["a", "b", "c"] {
            builder.add_node(name, move |_ctx: NodeContext| async move {
                Ok(NodeOutput::update("log", json!(name)))
            });
        }
        builder.set_entry_point("a");
        builder.add_edge("a", "b");
        builder.add_edge("b", "c");
        (builder.compile().unwrap(), spec)
    }

    #[tokio::test]
    async fn run_with_checkpoint_id_replays_from_earlier_state() {
        let (graph, spec) = linear_three_node_graph();
        let checkpointer = Arc::new(InMemoryCheckpointer::new());
        let executor = Executor::with_checkpointer(checkpointer.clone());

        // Full run on the source thread: checkpoints at steps 0, 1, 2.
        let outcome = executor
            .run(&graph, &spec, State::new(), RunConfig::new("t-src"))
            .await
            .unwrap();
        match outcome {
            ExecutionOutcome::Done(state) => {
                assert_eq!(state.get("log"), Some(&json!(["a", "b", "c"])));
            }
            other => panic!("expected Done, got {other:?}"),
        }

        let history = checkpointer.list("t-src").await.unwrap();
        assert_eq!(history.len(), 3);
        // The step-1 checkpoint: `a` and `b` have run, `c` is scheduled next.
        let step1 = history[1].clone();
        assert_eq!(step1.step, 1);
        assert_eq!(step1.state.get("log"), Some(&json!(["a", "b"])));
        assert_eq!(step1.next_nodes, vec!["c".to_string()]);

        // Safe pattern: fork the thread at the step-1 checkpoint, then replay
        // the fork from that checkpoint (not the fork's latest — here the cut
        // point IS the latest, but `checkpoint_id` is what selects it).
        let copied = checkpointer
            .fork_thread("t-src", "t-fork", Some(&step1.id))
            .await
            .unwrap();
        assert_eq!(copied, 2);

        let outcome = executor
            .run(
                &graph,
                &spec,
                State::new(),
                RunConfig::new("t-fork").with_checkpoint_id(step1.id.clone()),
            )
            .await
            .unwrap();

        match outcome {
            ExecutionOutcome::Done(state) => {
                // Execution continued from the step-1 state: only `c` ran,
                // `b` was not re-executed.
                assert_eq!(state.get("log"), Some(&json!(["a", "b", "c"])));
            }
            other => panic!("expected Done, got {other:?}"),
        }

        // The replay appended its own boundary checkpoint to the fork only.
        assert_eq!(checkpointer.list("t-src").await.unwrap().len(), 3);
        assert_eq!(checkpointer.list("t-fork").await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn run_with_checkpoint_id_plus_resume_combined() {
        let spec = StateSpec::new().channel("answer", Reducer::Overwrite);

        let mut builder = GraphBuilder::new();
        builder.add_node("gate", |ctx: NodeContext| async move {
            match ctx.resume_value() {
                Some(v) => Ok(NodeOutput::update("answer", v.clone())),
                None => Err(ctx.interrupt(json!({"question": "approve?"}))),
            }
        });
        builder.set_entry_point("gate");
        let graph = builder.compile().unwrap();

        let checkpointer = Arc::new(InMemoryCheckpointer::new());
        let executor = Executor::with_checkpointer(checkpointer);

        // Suspend at the gate and capture the suspension checkpoint id.
        let outcome = executor
            .run(&graph, &spec, State::new(), RunConfig::new("t-hitl"))
            .await
            .unwrap();
        let checkpoint_id = match outcome {
            ExecutionOutcome::Interrupted { checkpoint_id, .. } => checkpoint_id,
            other => panic!("expected Interrupted, got {other:?}"),
        };

        // checkpoint_id selects WHERE (the suspension checkpoint), resume
        // supplies the value delivered to the re-running gate node.
        let outcome = executor
            .run(
                &graph,
                &spec,
                State::new(),
                RunConfig::new("t-hitl")
                    .with_checkpoint_id(checkpoint_id)
                    .with_resume(json!(true)),
            )
            .await
            .unwrap();

        match outcome {
            ExecutionOutcome::Done(state) => {
                assert_eq!(state.get("answer"), Some(&json!(true)));
            }
            other => panic!("expected Done after replay+resume, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_with_checkpoint_id_errors_without_checkpointer_or_unknown_id() {
        let (graph, spec) = linear_three_node_graph();

        // No checkpointer configured: replay is impossible.
        let err = Executor::new()
            .run(
                &graph,
                &spec,
                State::new(),
                RunConfig::new("t-x").with_checkpoint_id("some-id"),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, RustyError::Checkpoint(_)));

        // Checkpointer present but the id does not exist on the thread.
        let checkpointer = Arc::new(InMemoryCheckpointer::new());
        let executor = Executor::with_checkpointer(checkpointer.clone());
        executor
            .run(&graph, &spec, State::new(), RunConfig::new("t-x"))
            .await
            .unwrap();
        let err = executor
            .run(
                &graph,
                &spec,
                State::new(),
                RunConfig::new("t-x").with_checkpoint_id("no-such-checkpoint"),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, RustyError::Checkpoint(_)));
    }

    /// A self-looping spinner: each super-step increments `n`; the router
    /// terminates the run once `n` reaches 5.
    fn spinner_graph() -> (Graph, StateSpec) {
        let spec = StateSpec::new().channel("n", Reducer::Overwrite);
        let mut builder = GraphBuilder::new();
        builder.add_node("spin", |ctx: NodeContext| async move {
            // A paced step: without it the loop would burn through the
            // default max_steps budget faster than a test can cancel it.
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            let n = ctx.state().get("n").and_then(Value::as_i64).unwrap_or(0);
            Ok(NodeOutput::update("n", json!(n + 1)))
        });
        builder.set_entry_point("spin");
        builder.add_conditional_edges("spin", |state: State| async move {
            if state.get("n").and_then(Value::as_i64).unwrap_or(0) >= 5 {
                Ok(Route::End)
            } else {
                Ok(Route::Node("spin".into()))
            }
        });
        (builder.compile().unwrap(), spec)
    }

    #[tokio::test]
    async fn pre_cancelled_token_stops_the_run_before_the_first_step() {
        let (graph, spec) = spinner_graph();
        let token = CancellationToken::new();
        token.cancel();

        let err = Executor::new()
            .run(
                &graph,
                &spec,
                State::new(),
                RunConfig::new("t-cancel").with_cancellation(token),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, RustyError::Cancelled(_)));
        assert!(
            err.to_string().contains("after 0 super-step(s)"),
            "a pre-cancelled run must not execute a single step: {err}"
        );
    }

    #[tokio::test]
    async fn cancellation_stops_at_a_boundary_and_the_run_resumes_from_it() {
        let (graph, spec) = spinner_graph();
        let checkpointer = Arc::new(InMemoryCheckpointer::new());
        let executor = Executor::with_checkpointer(checkpointer.clone());
        let token = CancellationToken::new();

        // Cancel as soon as the first boundary checkpoint lands: the stop
        // is then provably the token's doing, not the terminating route.
        let canceller = token.clone();
        let watcher = checkpointer.clone();
        tokio::spawn(async move {
            loop {
                if watcher.get_latest("t-cancel").await.unwrap().is_some() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
            canceller.cancel();
        });
        let err = executor
            .run(
                &graph,
                &spec,
                State::new(),
                RunConfig::new("t-cancel").with_cancellation(token.clone()),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, RustyError::Cancelled(_)));

        // The boundary checkpoint is intact: n counts the completed steps —
        // strictly fewer than the terminating route's 5, proving the token
        // (not the route) ended the run.
        let checkpoint = checkpointer
            .get_latest("t-cancel")
            .await
            .unwrap()
            .expect("a boundary checkpoint must survive cancellation");
        let steps = checkpoint.state.get("n").and_then(Value::as_i64).unwrap();
        assert!(
            (1..5).contains(&steps),
            "unexpected stop point: n = {steps}"
        );

        // Re-running the thread resumes the spinner from the checkpoint.
        // Drain tokens are one-shot (a cancelled token stays cancelled), so
        // the post-deploy process runs under a fresh one.
        let outcome = executor
            .run(
                &graph,
                &spec,
                State::new(),
                RunConfig::new("t-cancel")
                    .with_resume(json!(null))
                    .with_cancellation(CancellationToken::new()),
            )
            .await
            .unwrap();
        match outcome {
            ExecutionOutcome::Done(state) => {
                assert_eq!(state.get("n"), Some(&json!(5)));
            }
            other => panic!("expected Done after resume, got {other:?}"),
        }
    }
}
