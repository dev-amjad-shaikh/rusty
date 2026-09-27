//! Run scheduling, execution, and bookkeeping.
//!
//! A run goes: *schedule* (strategy check + handle insert) → *execute*
//! (drive [`Executor`] in a spawned task, forwarding [`GraphEvent`]s to a
//! per-run SSE frame log + broadcast channel) → *terminate* (terminal status
//! + JSON recorded, waiters woken, next queued run for the thread spawned).
//!
//! Multitask: there is always at most one **active** run per thread. The
//! `reject` strategy returns 409 when the thread is busy; `enqueue` appends
//! to a per-thread FIFO queue (depth-capped by
//! `ServerConfig::max_concurrent_runs_per_thread`) that drains automatically
//! as runs finish. The FIFO is store-backed: a queued run has no checkpoint
//! coverage (it never executed, so there is nothing to resume *from*), so
//! its queue entry is persisted on enqueue ([`crate::pending_runs`]),
//! deleted when the run leaves the queue, and replayed into the scheduler
//! on boot ([`restore_pending_runs`]) — a restart no longer strands
//! accepted-but-never-started runs.
//!
//! Retention: terminal runs are kept for `GET /runs/{id}` polling up to
//! [`MAX_RETAINED_RUNS`] per process; the oldest terminal runs are evicted
//! beyond that (active and queued runs are never evicted). Run history is
//! in-memory by design — durability of executed runs lives in the
//! checkpoint log, durability of queued runs in the pending-run records.
//!
//! Drain (R0.6 wave 2c): the server's shutdown token is threaded into every
//! run's executor ([`RunConfig::with_cancellation`]). When it fires, each
//! in-flight run stops at its next super-step boundary — a point where a
//! checkpoint was just persisted — and ends terminal-[`RunStatus::Cancelled`],
//! resumable by simply re-running the thread; new submissions answer 503 and
//! queued runs are not promoted. Anything still mid-step when the grace
//! window closes is abandoned, which is the crash case the checkpoint log
//! already covers.
//!
//! Flight Recorder: every run is journaled. The journal is attached to the
//! executor at run start, flushed to the server store at every checkpoint
//! boundary (in [`forward_events`]) and once more at run completion, and
//! served read-only by `GET /runs/{id}/events`.

use rusty_agent_runtime::memory::{MemoryScope, MemorySource, MemoryStore, ScopeAddress};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use futures::FutureExt;
use rusty_agent_runtime::checkpoint::Checkpointer;
use rusty_agent_runtime::error::RustyError;
use rusty_agent_runtime::executor::{ExecutionOutcome, Executor, GraphEvent, RunConfig};
use rusty_agent_runtime::journal::{Clock, EventDraft, Journal};
use rusty_agent_runtime::record::{Effect, RunEventKind};
use rusty_agent_runtime::state::State;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, watch, Mutex};

use crate::error::ApiError;
use crate::pending_runs::PendingRunRecord;
use crate::server_store::ServerStore;
use crate::GraphRegistry;

// --------------------------------------------------------------------- //
// Run payload (accepted by all three run endpoints)
// --------------------------------------------------------------------- //

/// The `command` field of a run payload: `{ "resume": <value> }` continues
/// an interrupted thread via [`RunConfig::with_resume`].
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct CommandPayload {
    /// Resume value delivered to the interrupted node.
    #[serde(default)]
    pub resume: Option<Value>,
}

/// The `config` field of a run payload.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct RunConfigPayload {
    /// The skills the agent follows this run (context skills-section
    /// entries, wire form). Defaulted from the assistant's
    /// `config.studio_intent.skills`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<Value>,
    /// Maps to [`RunConfig::with_max_steps`] (LangGraph `recursion_limit`).
    #[serde(default)]
    pub recursion_limit: Option<usize>,

    /// Exact subset of the graph's tools available to this run
    /// ([`RunConfig::tool_allowlist`]). Validated against the graph's
    /// executable catalog at admission: unknown or duplicate names are a
    /// structured 400. Absent preserves the graph's complete registry
    /// (byte-identical prior behavior); `[]` is a deliberately tool-free
    /// run. Mutually exclusive with `capability_set`.
    #[serde(default)]
    pub tool_allowlist: Option<Vec<String>>,

    /// Tools this run must not call, whatever the allowlist admits: a
    /// guard refuses each attempt and journals it as a denial. The
    /// constraint a person states once — at delegation — and the platform
    /// holds every round, so the record shows a constraint tested, not
    /// only stated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forbidden_tools: Option<Vec<String>>,

    /// The agent's standing instructions for this run. Defaulted from the
    /// assistant's `config.studio_intent.instructions`, the way
    /// `recursion_limit` and the tool allowlist already are — an explicit
    /// value on the payload wins.
    #[serde(default)]
    pub instructions: Option<String>,

    /// The provider this run calls, by the deployment's id. Defaulted from
    /// the assistant's `config.studio_intent.model`; an explicit value on the
    /// payload wins; an id the deployment does not hold runs on the primary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    /// The run's own fallback provider, by the deployment's id. Defaulted
    /// from the assistant's `config.studio_intent.fallback_model`; absent,
    /// the deployment's fallback stands behind the run's model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_model: Option<String>,
    /// The agent's sampling temperature (0–2), from its configuration or the
    /// working copy under test; absent, the provider's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// The cap on the work this run starts — every delegated round and
    /// queued task of the chain together — from the working copy under test;
    /// absent, the agent's published one, else the platform's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain_max_tokens: Option<u64>,

    /// Values for the `{{name}}` placeholders in the charter and the skills,
    /// by name. Layered over the agent's own settings
    /// (`studio_intent.variables[].value`) at admission: a trigger fills the
    /// ones it sources from its event, the builder's Test panel sends its
    /// test values. A placeholder no value reaches stays literal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variables: Option<std::collections::BTreeMap<String, String>>,

    /// Skills by name, for a run that has no agent to take them from — a
    /// draft tried from the builder before it is created. Resolved at
    /// admission into `skills` exactly as an agent's are, and the door to
    /// read any other skill comes with them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_names: Option<Vec<String>>,

    /// The builder's when-to-use note per tool, for a run that has no agent
    /// to take them from. Resolved at admission into `tool_overlays` the way
    /// an agent's `studio_intent.tools[].when` are.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_notes: Option<std::collections::BTreeMap<String, String>>,
    /// A draft run's long-term memory switch (`none` | `read_write`), as
    /// the working copy has it; `apply_draft_config` folds it into the
    /// context override and the allowlist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_access: Option<String>,
    /// The working copy's declared memory blocks (`label`, `description`,
    /// `char_limit`), when a draft under test declares its own: rendered
    /// into this run and editable by `memory.block_edit` before the draft
    /// is published.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_blocks_declared: Option<Vec<Value>>,
    /// The agent's curated memory blocks, rendered at run start by
    /// `apply_assistant_defaults` for the situation section.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_blocks: Option<String>,

    /// Inline capability-set declaration: the composition
    /// [`rusty_agent_runtime::capability::CapabilitySet`] resolves at
    /// admission, validated against the graph's catalog. Its tool members
    /// become the run's allowlist and its content address pins into the
    /// run's manifest. Mutually exclusive with `tool_allowlist`.
    #[serde(default)]
    pub capability_set: Option<CapabilitySetPayload>,

    /// Per-tool selection overlays by tool name
    /// ([`rusty_agent_runtime::tool_select::ToolSelectionOverlay`], wire
    /// form): the builder's when-to-use note and the rest of the governed
    /// metadata. Defaulted from the assistant's `config.studio_intent.tools`
    /// (`{name, when}`), one overlay per tool with a note. Every name must
    /// be a tool the run may call — validated at admission, fail closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_overlays: Option<Value>,

    /// What the run may spend ([`rusty_agent_runtime::meter::RunBudget`]:
    /// `max_tokens`, `max_cost_usd`). Defaulted from the assistant's
    /// `config.studio_intent.budget`. The executor stops the run at the step
    /// that crosses a bound; the terminal says so and carries `spend`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<rusty_agent_runtime::meter::RunBudget>,
    /// The world this run acts in, when a suite (or a person) put it in
    /// one: the world answers the run's connector calls in place of the
    /// system it stands in for. Stamped into the execution block every
    /// tool reads; the world must be the run's tenant's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub world: Option<String>,
    /// The worlds a run acts in when it touches more than one system —
    /// each by name or id, one per connector, routed by the host each
    /// stands in for. `world` is the first of them (kept for every reader
    /// of one world); a run naming only `world` acts in that one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worlds: Option<Vec<String>>,
    /// The context this agent works with, where it differs from the
    /// deployment's policy: its window in estimated tokens and how many
    /// recent messages compaction keeps verbatim. Defaulted from the
    /// assistant's `config.studio_intent.context`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<ContextOverride>,
}

/// What a builder sets per agent about its context; everything else in
/// the policy is the deployment's. A window below 4,096 tokens is raised
/// to it, as the standard policy does: smaller than that, no section can
/// hold a real turn.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_recent_messages: Option<usize>,
    /// `Some(false)` when the builder turned long-term memory off for this
    /// agent: its runs get no memory section and no memory store. Before
    /// this the studio's switch wrote a setting nothing read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<bool>,
}

impl ContextOverride {
    pub fn is_empty(&self) -> bool {
        self.budget_tokens.is_none() && self.keep_recent_messages.is_none() && self.memory.is_none()
    }

    /// `true` when the builder turned long-term memory off.
    pub fn memory_off(&self) -> bool {
        self.memory == Some(false)
    }

    /// The deployment's policy, tailored to this agent: a new window keeps
    /// the deployment's memory section (resized to an eighth, as the
    /// deployment sizes it) and its tokenizer; a kept-messages count lands
    /// on the compaction the policy already has.
    pub fn apply(
        &self,
        base: &rusty_agent_runtime::context::ContextPolicy,
    ) -> rusty_agent_runtime::context::ContextPolicy {
        let mut policy = match self.budget_tokens {
            Some(window) => {
                let mut resized = rusty_agent_runtime::context::ContextPolicy::standard(window);
                resized.tokenizer = base.tokenizer.clone();
                if base.memory.is_some() {
                    resized =
                        resized.with_memory_section((window.max(4_096) / 8).clamp(512, 4_096));
                    if let Some(recall) = &base.recall {
                        resized = resized.with_recall(recall.budget_tokens, recall.top_k);
                    }
                }
                resized
            }
            None => base.clone(),
        };
        if let (Some(keep), Some(compaction)) =
            (self.keep_recent_messages, policy.compaction.as_mut())
        {
            // The studio counts turns: the number is the steps kept, and
            // the message floor with it.
            compaction.keep_recent_messages = keep.max(1);
            compaction.keep_recent_steps = keep.max(1);
        }
        if self.memory_off() {
            policy.memory = None;
            policy.recall = None;
        }
        policy
    }
}

/// The inline `config.capability_set` member list.
///
/// Tool members are exact names validated against the graph's catalog.
/// Skill members are opaque references with kind tags, recorded into the
/// set's content address today and validated by the skill plane when that
/// lands.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct CapabilitySetPayload {
    /// Exact tool names the set composes.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Opaque skill references (the skill plane interprets them).
    #[serde(default)]
    pub skills: Vec<String>,
}

impl CapabilitySetPayload {
    /// The shape-checked skill references, kind-tagged.
    pub fn refs(
        &self,
    ) -> rusty_agent_runtime::error::Result<Vec<rusty_agent_runtime::capability::CapabilityRef>>
    {
        self.skills
            .iter()
            .map(|reference| {
                rusty_agent_runtime::capability::CapabilityRef::skill(reference.clone())
            })
            .collect()
    }
}

/// The `checkpoint` field of a run payload: `{ "checkpoint_id": "…" }`
/// replays the thread from that checkpoint (time travel) instead of the
/// latest, via [`RunConfig::with_checkpoint_id`].
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct CheckpointPayload {
    /// Id of a checkpoint of this thread (see `POST /threads/{id}/history`).
    pub checkpoint_id: String,
}

/// The payload accepted by `POST /threads/{id}/runs{,/wait,/stream}`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct RunPayload {
    /// Initial state (must be a JSON object). Ignored when resuming: the
    /// checkpointed state takes precedence.
    #[serde(default)]
    pub input: Option<Value>,

    /// `{ "resume": <value> }` — the human-in-the-loop channel.
    #[serde(default)]
    pub command: Option<CommandPayload>,

    /// `{ "recursion_limit": n }`.
    #[serde(default)]
    pub config: Option<RunConfigPayload>,

    /// `{ "checkpoint_id": "…" }` — time travel: replay the run from that
    /// checkpoint of this thread instead of the latest (`404` when the
    /// checkpoint is unknown). Prefer forking first
    /// (`POST /threads/{id}/fork`) and replaying on the fork.
    #[serde(default)]
    pub checkpoint: Option<CheckpointPayload>,

    /// Free-form run metadata (stored, not interpreted).
    #[serde(default)]
    pub metadata: Option<Value>,

    /// Which frame families to emit on the SSE stream. Default:
    /// `["values", "updates"]`. `metadata`, `error`, and `end` frames are
    /// always emitted.
    #[serde(default)]
    pub stream_mode: Option<Vec<String>>,

    /// `"reject"` (409 when the thread is busy) or `"enqueue"` (default:
    /// queue onto the per-thread run queue).
    #[serde(default)]
    pub multitask_strategy: Option<String>,

    /// Run through a named assistant (see `POST /assistants`). The
    /// assistant must be bound to the same graph as the thread; its
    /// `config.recursion_limit` applies when the payload does not set one.
    #[serde(default)]
    pub assistant_id: Option<String>,

    /// Optional optimistic guard for a named assistant. When present, run
    /// admission fails unless the assistant still serves this exact immutable
    /// version. This keeps a reviewed UI handoff exact across concurrent
    /// activation or rollback.
    #[serde(default)]
    pub expected_active_version_id: Option<String>,

    /// The run's registry declaration (R0.11 Extension Plane, wave 2):
    /// the named configuration artifacts the run uses and the
    /// environment it targets. At admission each artifact resolves
    /// through its environment-tagged version pointer and the resolved
    /// content pins the run's manifest, with one `config_resolved` event
    /// per artifact journaled ahead of the run's own events. Absent is
    /// the pre-R0.11 behavior, byte-identically: no resolution, no
    /// manifest, no new events.
    #[serde(default)]
    pub registry: Option<crate::registry::RegistryRunBinding>,

    /// The run's deployment declaration (R0.12 Operations Plane, wave 3):
    /// the environment the run is admitted to. At admission the
    /// environment's deployment pointer binds a revision — identity and
    /// topology checked against the registered graph — with one
    /// `deployment_resolved` event journaled ahead of the run's own
    /// events (chained after the registry resolutions, one causal unit).
    /// Absent is the pre-R0.12 behavior, byte-identically: no
    /// resolution, no new event.
    #[serde(default)]
    pub deployment: Option<crate::deploy::DeploymentRunBinding>,
}

/// How a second run on a busy thread is handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MultitaskStrategy {
    /// Queue behind the active run (default).
    Enqueue,
    /// Fail immediately with 409.
    Reject,
}

impl MultitaskStrategy {
    /// Parse the wire value (`None` defaults to `enqueue`).
    pub fn parse(raw: Option<&str>) -> Result<Self, String> {
        match raw {
            None | Some("enqueue") => Ok(Self::Enqueue),
            Some("reject") => Ok(Self::Reject),
            Some(other) => Err(format!(
                "unknown multitask_strategy `{other}` (expected `enqueue` or `reject`)"
            )),
        }
    }
}

// --------------------------------------------------------------------- //
// Run bookkeeping
// --------------------------------------------------------------------- //

/// Lifecycle status of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    /// Queued behind another run on the same thread.
    Pending,
    /// Currently executing.
    Running,
    /// Terminated normally.
    Success,
    /// Suspended on an interrupt; resumable via `command.resume`.
    Interrupted,
    /// Failed.
    Error,
    /// Stopped by the graceful-shutdown drain (R0.6 wave 2c) at a
    /// super-step boundary. Control flow, not failure — the boundary
    /// checkpoint is intact, so re-running the thread resumes the run from
    /// exactly where it stopped.
    Cancelled,
}

impl RunStatus {
    /// The wire representation of the status.
    pub fn as_str(&self) -> &'static str {
        match self {
            RunStatus::Pending => "pending",
            RunStatus::Running => "running",
            RunStatus::Success => "success",
            RunStatus::Interrupted => "interrupted",
            RunStatus::Error => "error",
            RunStatus::Cancelled => "cancelled",
        }
    }

    /// `true` once the run can no longer make progress in this process
    /// (terminal statuses, including `Cancelled`: a drained run resumes in
    /// a *new* run, never in place).
    pub(crate) fn is_terminal(self) -> bool {
        matches!(
            self,
            RunStatus::Success | RunStatus::Interrupted | RunStatus::Error | RunStatus::Cancelled
        )
    }
}

/// One SSE frame as recorded in the per-run event log and broadcast live.
/// `id` follows the design doc's `{checkpoint_id}:{step}:{seq}` format.
#[derive(Debug, Clone)]
pub struct SseFrame {
    /// Frame id: `{checkpoint_id}:{step}:{seq}`.
    pub id: String,
    /// SSE event name (`metadata`, `updates`, `values`, `error`, `end`).
    pub event: String,
    /// JSON payload.
    pub data: Value,
    /// Per-run monotonically increasing sequence number (1-based).
    pub seq: u64,
}

/// Shared frame producer for one run: assigns sequence numbers, appends to
/// the bounded event log, and fans out over the broadcast channel.
#[derive(Clone)]
pub(crate) struct FrameSink {
    log: Arc<StdMutex<VecDeque<SseFrame>>>,
    bcast: broadcast::Sender<SseFrame>,
    seq: Arc<AtomicU64>,
    last_checkpoint: Arc<StdMutex<String>>,
    last_step: Arc<AtomicU64>,
    capacity: usize,
}

/// Lock a std mutex, recovering from poisoning. Every guard obtained
/// through this helper wraps a simple clone/push/assign critical section,
/// so a panicked holder cannot leave the value structurally inconsistent —
/// and unwinding a whole run (or wedging its thread slot) over a poisoned
/// frame log is the worse outcome.
pub(crate) fn lock_recover<T>(mutex: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl FrameSink {
    fn new(capacity: usize, bcast: broadcast::Sender<SseFrame>) -> Self {
        Self {
            log: Arc::new(StdMutex::new(VecDeque::new())),
            bcast,
            seq: Arc::new(AtomicU64::new(0)),
            last_checkpoint: Arc::new(StdMutex::new("-".to_string())),
            last_step: Arc::new(AtomicU64::new(0)),
            capacity,
        }
    }

    /// Record and broadcast one frame.
    pub(crate) fn push(&self, event: &str, step: usize, data: Value) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        self.last_step.store(step as u64, Ordering::Relaxed);
        let checkpoint = lock_recover(&self.last_checkpoint).clone();
        let frame = SseFrame {
            id: format!("{checkpoint}:{step}:{seq}"),
            event: event.to_string(),
            data,
            seq,
        };
        {
            let mut log = lock_recover(&self.log);
            if log.len() >= self.capacity {
                log.pop_front();
            }
            log.push_back(frame.clone());
        }
        // No live subscribers is normal (background runs); not an error.
        let _ = self.bcast.send(frame);
    }

    /// Point subsequent frame ids at a freshly persisted checkpoint.
    pub(crate) fn note_checkpoint(&self, checkpoint_id: &str) {
        *lock_recover(&self.last_checkpoint) = checkpoint_id.to_string();
    }

    /// The super-step of the most recently pushed frame.
    pub(crate) fn current_step(&self) -> usize {
        self.last_step.load(Ordering::Relaxed) as usize
    }
}

/// Everything the executor task needs, snapshotted from a [`RunHandle`].
pub(crate) struct RunSnapshot {
    /// Internal (tenant-scoped) thread id: used for the checkpointer, the
    /// executor config, and RunManager bookkeeping.
    pub thread_id: String,
    /// External thread id as the client knows it — the only form that may
    /// appear on the wire (SSE frames, terminal JSON).
    pub wire_thread_id: String,
    pub graph: String,
    pub attempt: usize,
    pub payload: RunPayload,
    /// The registry binding resolved at admission (R0.11 wave 2).
    pub admission: Option<crate::registry::RegistryAdmission>,
    /// The deployment binding resolved at admission (R0.12 wave 3).
    pub deployment: Option<crate::deploy::DeploymentAdmission>,
    pub sink: FrameSink,
    pub checkpoint_ids: Arc<StdMutex<Vec<String>>>,
    /// This run's own cancellation token (R0.7 wave 2): a child of the
    /// server's drain token, so a run-level cancel ([`RunManager::cancel_run`]
    /// — the cancellation tree's run half) stops this run at its next
    /// super-step boundary without touching any other run, while a server
    /// drain still stops them all. Observed by the executor exactly where
    /// the drain token always was.
    pub cancel: tokio_util::sync::CancellationToken,
}

/// Read-only view of a run (used by the rollback and status endpoints).
pub(crate) struct RunInfo {
    /// Internal (tenant-scoped) thread id — handlers check tenant ownership
    /// against it before revealing anything about the run.
    pub thread_id: String,
    /// External thread id for wire responses.
    pub wire_thread_id: String,
    pub graph: String,
    /// External assistant identity captured in the accepted run payload.
    pub assistant_id: Option<String>,
    /// Accepted run metadata, including Studio's exact objective when present.
    pub metadata: Option<Value>,
    /// Exact accepted input used to bind derived evaluation cases.
    pub input: Option<Value>,
    /// The run's declared tool selection, captured at admission: the
    /// effective allowlist (explicit, capability-set members, or the
    /// assistant default) — `None` for an unrestricted run. The replay
    /// endpoint re-validates it against the current catalog rather than
    /// silently widening or narrowing a replayed run.
    pub capability_tools: Option<Vec<String>>,
    /// The worlds the run acts in, by id — `worlds` when it named several,
    /// else the one `world`; empty for a run in the live systems.
    pub worlds: Vec<String>,
    /// Stable server acceptance time used to bind downstream evidence.
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub attempt: usize,
    pub status: RunStatus,
    /// The terminal JSON once the run has finished (`None` while active).
    pub terminal: Option<Value>,
    pub checkpoint_ids: Arc<StdMutex<Vec<String>>>,
}

/// Build the read-only view of one handle (the [`RunManager::info`] /
/// [`RunManager::list`] mapping).
fn run_info_of(h: &RunHandle) -> RunInfo {
    RunInfo {
        thread_id: h.thread_id.clone(),
        wire_thread_id: h.wire_thread_id.clone(),
        graph: h.graph.clone(),
        assistant_id: h.payload.assistant_id.clone(),
        metadata: h.payload.metadata.clone(),
        input: h.payload.input.clone(),
        capability_tools: h.payload.config.as_ref().and_then(|config| {
            config
                .capability_set
                .as_ref()
                .map(|set| set.tools.clone())
                .or_else(|| config.tool_allowlist.clone())
        }),
        worlds: worlds_of(h.payload.config.as_ref()),
        created_at: h.created_at,
        attempt: h.attempt,
        status: h.status,
        terminal: h.terminal.borrow().clone(),
        checkpoint_ids: Arc::clone(&h.checkpoint_ids),
    }
}

/// The worlds a run's config puts it in, by id: `worlds` when several,
/// else the one `world`, else none.
pub(crate) fn worlds_of(config: Option<&RunConfigPayload>) -> Vec<String> {
    let Some(config) = config else {
        return Vec::new();
    };
    match (&config.worlds, &config.world) {
        (Some(many), _) if many.len() > 1 => many.clone(),
        (_, Some(one)) => vec![one.clone()],
        _ => Vec::new(),
    }
}

/// Handle for one scheduled run, owned by the [`RunManager`]. Crate-private
/// surface: external users interact with runs over HTTP, not this type.
pub struct RunHandle {
    /// Run id (UUID v4).
    pub(crate) run_id: String,
    /// Internal (tenant-scoped) thread id this run executes against.
    pub(crate) thread_id: String,
    /// External thread id reported on the wire.
    pub(crate) wire_thread_id: String,
    /// Registered graph name.
    pub(crate) graph: String,
    /// 1-based attempt counter for the thread.
    pub(crate) attempt: usize,
    /// Lifecycle status.
    pub(crate) status: RunStatus,
    /// Original run payload.
    pub(crate) payload: RunPayload,
    pub(crate) created_at: chrono::DateTime<chrono::Utc>,
    /// The registry binding resolved at admission (R0.11 wave 2) — `None`
    /// for an unbound run, which behaves byte-identically to before.
    pub(crate) admission: Option<crate::registry::RegistryAdmission>,
    /// The deployment binding resolved at admission (R0.12 wave 3) —
    /// `None` for an undeclared run, byte-identical to before.
    pub(crate) deployment: Option<crate::deploy::DeploymentAdmission>,
    sink: FrameSink,
    terminal: watch::Sender<Option<Value>>,
    checkpoint_ids: Arc<StdMutex<Vec<String>>>,
    /// This run's own cancellation token (see [`RunSnapshot::cancel`]).
    /// Firing it is the run-level half of the R0.7 cancellation tree; the
    /// executor observes it at super-step boundaries, after the boundary
    /// checkpoint has landed.
    cancel: tokio_util::sync::CancellationToken,
}

impl RunHandle {
    /// Subscribe to the live frame stream.
    pub(crate) fn subscribe(&self) -> broadcast::Receiver<SseFrame> {
        self.sink.bcast.subscribe()
    }

    /// A point-in-time copy of the event log (for replay).
    pub(crate) fn log_snapshot(&self) -> Vec<SseFrame> {
        lock_recover(&self.sink.log).iter().cloned().collect()
    }
}

/// What [`RunManager::insert`] decided for a freshly scheduled run.
pub(crate) enum ScheduleDecision {
    /// The thread slot was free; the run must be spawned now.
    Started,
    /// The run was queued behind the active run; carries the run's FIFO
    /// sequence, the persisted record's `seq` (durable queue).
    Queued(u64),
}

/// What [`RunManager::cancel_run`] did (R0.7 wave 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunCancel {
    /// The run was executing; its cancellation token fired. The terminal
    /// transition lands when the executor observes it at the next
    /// super-step boundary.
    Signalled,
    /// The run was queued behind another; it was dequeued and finished
    /// terminal-`cancelled` without ever starting.
    CancelledQueued,
    /// The run was already terminal; nothing changed.
    Terminal,
    /// No such run.
    Unknown,
}

/// What [`RunManager::cancel_thread_runs`] did to one thread's runs
/// (R0.7 wave 2), split by how each run's cancellation lands — mirroring
/// the task queue's [`crate::tasks::RunCancellation`] shape.
#[derive(Debug, Default)]
pub(crate) struct ThreadCancellation {
    /// Running runs whose cancellation tokens fired (terminal at their
    /// next boundary).
    pub signalled: Vec<String>,
    /// Queued runs dequeued into terminal-`cancelled` immediately.
    pub cancelled: Vec<String>,
}

impl ThreadCancellation {
    /// `true` when no run was touched (the thread had no active or queued
    /// runs) — the cancel route journals an `AgentExit` only when the
    /// cancellation actually landed somewhere.
    pub(crate) fn is_empty(&self) -> bool {
        self.signalled.is_empty() && self.cancelled.is_empty()
    }
}

#[derive(Default)]
struct RunManagerInner {
    runs: HashMap<String, RunHandle>,
    active_by_thread: HashMap<String, String>,
    queues: HashMap<String, VecDeque<String>>,
    attempts: HashMap<String, usize>,
    /// Process-wide monotonic FIFO sequence, stamped on every insert.
    /// Compared only within one thread (the persisted record's `seq`);
    /// a single counter avoids a per-thread watermark a restart would
    /// reset. Not itself durable — restored runs take fresh stamps in
    /// restore order, which the persisted sequence already fixed.
    queue_seq: u64,
    /// Insertion order of run ids, feeding terminal-run eviction.
    order: VecDeque<String>,
}

/// Cap on retained runs (see the module docs' retention note). Without a
/// cap, `runs` would grow by one record — payload clone, terminal JSON, and
/// up to `event_log_capacity` SSE frames — per run for the process
/// lifetime: a steady memory leak on any busy cron schedule.
const MAX_RETAINED_RUNS: usize = 1024;

/// Registry of all runs, plus per-thread scheduling state. Cheap to clone
/// (shared inner).
#[derive(Default, Clone)]
pub struct RunManager {
    inner: Arc<Mutex<RunManagerInner>>,
}

impl RunManager {
    /// An empty manager.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a new run under the given multitask strategy, assigning its
    /// per-thread attempt number.
    pub(crate) async fn insert(
        &self,
        mut handle: RunHandle,
        strategy: MultitaskStrategy,
        queue_cap: usize,
    ) -> Result<ScheduleDecision, ApiError> {
        let mut inner = self.inner.lock().await;
        let busy = inner.active_by_thread.contains_key(&handle.thread_id);
        let attempt = {
            let counter = inner.attempts.entry(handle.thread_id.clone()).or_insert(0);
            *counter += 1;
            *counter
        };
        handle.attempt = attempt;
        inner.queue_seq += 1;
        let seq = inner.queue_seq;
        inner.order.push_back(handle.run_id.clone());

        match strategy {
            MultitaskStrategy::Reject if busy => Err(ApiError::conflict(format!(
                "thread `{}` already has an active run",
                handle.thread_id
            ))),
            _ if busy => {
                let queue = inner.queues.entry(handle.thread_id.clone()).or_default();
                if queue.len() >= queue_cap {
                    return Err(ApiError::conflict(format!(
                        "thread `{}` run queue is full (cap {queue_cap})",
                        handle.thread_id
                    )));
                }
                queue.push_back(handle.run_id.clone());
                inner.runs.insert(handle.run_id.clone(), handle);
                Ok(ScheduleDecision::Queued(seq))
            }
            _ => {
                inner
                    .active_by_thread
                    .insert(handle.thread_id.clone(), handle.run_id.clone());
                handle.status = RunStatus::Running;
                inner.runs.insert(handle.run_id.clone(), handle);
                Ok(ScheduleDecision::Started)
            }
        }
    }

    /// Snapshot everything the executor task needs for `run_id`.
    pub(crate) async fn snapshot(&self, run_id: &str) -> Option<RunSnapshot> {
        let inner = self.inner.lock().await;
        inner.runs.get(run_id).map(|h| RunSnapshot {
            thread_id: h.thread_id.clone(),
            wire_thread_id: h.wire_thread_id.clone(),
            graph: h.graph.clone(),
            attempt: h.attempt,
            payload: h.payload.clone(),
            admission: h.admission.clone(),
            deployment: h.deployment.clone(),
            sink: h.sink.clone(),
            checkpoint_ids: Arc::clone(&h.checkpoint_ids),
            cancel: h.cancel.clone(),
        })
    }

    /// Read-only run info for API endpoints.
    pub(crate) async fn info(&self, run_id: &str) -> Option<RunInfo> {
        let inner = self.inner.lock().await;
        inner.runs.get(run_id).map(run_info_of)
    }

    /// Read-only infos for every run this process holds — active, queued,
    /// and retained terminal — paired with their run ids (the registry's
    /// keys, which `RunInfo` itself does not carry). Feeds the `GET /runs`
    /// recall list; iteration order is unspecified, the handler sorts.
    pub(crate) async fn list(&self) -> Vec<(String, RunInfo)> {
        let inner = self.inner.lock().await;
        inner
            .runs
            .iter()
            .map(|(run_id, h)| (run_id.clone(), run_info_of(h)))
            .collect()
    }

    /// Replay log + live subscription + internal thread id for the
    /// SSE attach endpoint (`GET /runs/{id}/stream`).
    pub(crate) async fn stream_parts(
        &self,
        run_id: &str,
    ) -> Option<(Vec<SseFrame>, broadcast::Receiver<SseFrame>, String)> {
        let inner = self.inner.lock().await;
        inner
            .runs
            .get(run_id)
            .map(|h| (h.log_snapshot(), h.subscribe(), h.thread_id.clone()))
    }

    /// `true` while the thread has an active run or a non-empty queue —
    /// rollback refuses to delete checkpoints out from under them.
    pub(crate) async fn thread_busy(&self, thread_id: &str) -> bool {
        let inner = self.inner.lock().await;
        inner.active_by_thread.contains_key(thread_id)
            || inner
                .queues
                .get(thread_id)
                .is_some_and(|queue| !queue.is_empty())
    }

    /// Cancel one run (R0.7 wave 2 — the run-level half of the
    /// cancellation tree). A running run is *signalled*: its own
    /// cancellation token fires and the executor stops it at the next
    /// super-step boundary — after the boundary checkpoint has landed —
    /// ending terminal-`cancelled` and resumable by re-running the thread,
    /// exactly like the server drain. A queued (pending) run never started,
    /// so it is dequeued and finished terminal-`cancelled` immediately —
    /// leaving it queued would let a dead run promote and execute.
    ///
    /// Manager-only primitive: callers must route through
    /// [`crate::runs::cancel_run`] so a dequeued run's durable queue record
    /// clears with the transition.
    pub(crate) async fn cancel_run(&self, run_id: &str) -> RunCancel {
        let mut inner = self.inner.lock().await;
        let Some(handle) = inner.runs.get_mut(run_id) else {
            return RunCancel::Unknown;
        };
        match handle.status {
            RunStatus::Running => {
                handle.cancel.cancel();
                RunCancel::Signalled
            }
            RunStatus::Pending => {
                let thread_id = handle.thread_id.clone();
                let wire_thread_id = handle.wire_thread_id.clone();
                if let Some(queue) = inner.queues.get_mut(&thread_id) {
                    queue.retain(|queued| queued != run_id);
                }
                let handle = inner
                    .runs
                    .get_mut(run_id)
                    .expect("the handle was resolved above");
                handle.status = RunStatus::Cancelled;
                let terminal = json!({
                    "run_id": run_id,
                    "thread_id": wire_thread_id,
                    "status": "cancelled",
                    "message": "cancelled while queued, before its first step",
                });
                handle.terminal.send_replace(Some(terminal));
                RunCancel::CancelledQueued
            }
            // Terminal runs (including an already-cancelled one) are
            // untouched — cancellation is control flow, idempotent by
            // no-op, never a second terminal transition.
            _ => RunCancel::Terminal,
        }
    }

    /// Cancel every run of one thread (R0.7 wave 2): the active run is
    /// signalled, every queued run is dequeued-cancelled — the whole
    /// per-thread run state, so cancelling an agent's thread leaves no
    /// pending run that would re-drive it.
    ///
    /// Manager-only primitive: callers must route through
    /// [`crate::runs::cancel_thread_runs`] so each dequeued run's durable
    /// queue record clears with the transition.
    pub(crate) async fn cancel_thread_runs(&self, thread_id: &str) -> ThreadCancellation {
        let (active, queued) = {
            let inner = self.inner.lock().await;
            (
                inner.active_by_thread.get(thread_id).cloned(),
                inner
                    .queues
                    .get(thread_id)
                    .map(|q| q.iter().cloned().collect::<Vec<_>>())
                    .unwrap_or_default(),
            )
        };
        let mut outcome = ThreadCancellation::default();
        if let Some(run_id) = active {
            if matches!(self.cancel_run(&run_id).await, RunCancel::Signalled) {
                outcome.signalled.push(run_id);
            }
        }
        for run_id in queued {
            if matches!(self.cancel_run(&run_id).await, RunCancel::CancelledQueued) {
                outcome.cancelled.push(run_id);
            }
        }
        outcome
    }

    /// Drop a queued run without a terminal transition: the rollback for
    /// an enqueue whose durable record could not be written — the
    /// submission failed, so no run may exist. `false` when the run is
    /// unknown or already left the queue (a promotion that outran the
    /// failed persist keeps its run; it was legitimately admitted).
    /// Drop a finished run from memory (forgetting a person): it no longer
    /// lists, whatever the store says. A run still going is left alone.
    pub(crate) async fn forget(&self, run_id: &str) -> bool {
        let mut inner = self.inner.lock().await;
        let Some(handle) = inner.runs.get(run_id) else {
            return false;
        };
        if matches!(handle.status, RunStatus::Pending | RunStatus::Running) {
            return false;
        }
        inner.runs.remove(run_id);
        inner.order.retain(|id| id != run_id);
        true
    }

    pub(crate) async fn remove_queued(&self, run_id: &str) -> bool {
        let mut inner = self.inner.lock().await;
        let Some(handle) = inner.runs.get(run_id) else {
            return false;
        };
        if handle.status != RunStatus::Pending {
            return false;
        }
        let thread_id = handle.thread_id.clone();
        if let Some(queue) = inner.queues.get_mut(&thread_id) {
            queue.retain(|queued| queued != run_id);
        }
        inner.runs.remove(run_id);
        inner.order.retain(|id| id != run_id);
        true
    }

    /// Record the terminal status + JSON, wake waiters, release the thread
    /// slot, and return the next queued run id for the thread (if any), now
    /// marked active.
    ///
    /// `draining` (the server's shutdown drain) suppresses queue promotion:
    /// the slot frees but nothing is returned — a run promoted into a
    /// shutting-down process would only be cancelled at its first boundary.
    /// Queued runs stay `Pending`; their threads' checkpoints are intact
    /// for the next process to re-drive.
    pub(crate) async fn finish(
        &self,
        run_id: &str,
        status: RunStatus,
        terminal: Value,
        draining: bool,
    ) -> Option<String> {
        let mut inner = self.inner.lock().await;
        let handle = inner.runs.get_mut(run_id)?;
        handle.status = status;
        // `send_replace` (not `send`) so the terminal JSON is stored even
        // when no waiter holds a receiver (background runs); status polling
        // via `info` reads it back through `watch::Sender::borrow`.
        handle.terminal.send_replace(Some(terminal));
        let thread_id = handle.thread_id.clone();

        if inner
            .active_by_thread
            .get(&thread_id)
            .is_some_and(|active| active == run_id)
        {
            inner.active_by_thread.remove(&thread_id);
        }

        let next = if draining {
            None
        } else {
            inner
                .queues
                .get_mut(&thread_id)
                .and_then(VecDeque::pop_front)
        };
        if let Some(next_id) = &next {
            if let Some(h) = inner.runs.get_mut(next_id) {
                h.status = RunStatus::Running;
            }
            inner.active_by_thread.insert(thread_id, next_id.clone());
        }

        // Evict the oldest terminal runs beyond the retention cap; active
        // and queued runs keep their slots in `order`.
        let mut excess = inner.runs.len().saturating_sub(MAX_RETAINED_RUNS);
        let mut skipped = Vec::new();
        while excess > 0 {
            let Some(candidate) = inner.order.pop_front() else {
                break;
            };
            let evictable = inner
                .runs
                .get(&candidate)
                .is_some_and(|h| h.status.is_terminal());
            if evictable {
                inner.runs.remove(&candidate);
                excess -= 1;
            } else {
                skipped.push(candidate);
            }
        }
        inner.order.extend(skipped);

        next
    }
}

/// What a run spent, folded from its journal for the terminal: model calls,
/// tokens, and journaled cost when the model prices itself (`null` when it
/// does not — unpriced, not free).
fn run_spend(journal: &rusty_agent_runtime::journal::Journal) -> Value {
    let mut spend = rusty_agent_runtime::meter::Spend::default();
    for event in journal.events() {
        spend.observe(&event);
    }
    json!({
        "requests": spend.requests,
        "tokens": spend.tokens,
        "cost_usd": spend.cost_usd,
    })
}

/// How many of the newest journals feed the tool-outcome snapshot a run's
/// shortlist scores against. A bound, so a busy store does not make every
/// run start with a full read.
const OUTCOME_JOURNALS: usize = 40;

/// The tenant's recent tool outcomes, rolled up per tool from the newest
/// journals: calls, successes, validation refusals. A journal the roll-up
/// cannot read is skipped — a run never fails to start over old evidence.
/// Empty when nothing has run yet.
pub(crate) async fn recent_tool_outcomes(
    deps: &RunDeps,
) -> BTreeMap<String, rusty_agent_runtime::tool_select::ToolOutcomeStats> {
    // The newest journals by their heads — a run's admission must not
    // read every journal the store holds to learn how the last few went.
    let mut heads = match deps.server_store.list_journal_heads().await {
        Ok(heads) => heads,
        Err(_) => return BTreeMap::new(),
    };
    heads.sort_by_key(|head| std::cmp::Reverse(head.last_at));
    heads.truncate(OUTCOME_JOURNALS);
    let mut journals = Vec::with_capacity(heads.len());
    for head in &heads {
        if let Ok(Some(snapshot)) = deps.server_store.get_journal(&head.run_id).await {
            journals.push(snapshot);
        }
    }
    let stamp = chrono::Utc::now();
    let mut merged: BTreeMap<String, rusty_agent_runtime::tool_select::ToolOutcomeStats> =
        BTreeMap::new();
    for snapshot in &journals {
        let Ok(index) = rusty_agent_runtime::tool_outcomes::build_outcome_index(&[snapshot], stamp)
        else {
            continue;
        };
        for (tool, stats) in index.selection_snapshot() {
            let entry = merged.entry(tool).or_default();
            entry.calls += stats.calls;
            entry.successes += stats.successes;
            entry.validation_failures += stats.validation_failures;
        }
    }
    merged
}

/// Everything the run machinery needs from the application: registry,
/// checkpointer, manager, and caps. Cheap to clone.
#[derive(Clone)]
pub(crate) struct RunDeps {
    /// The deployment's users: a run that acts for a person the store no
    /// longer holds must not start.
    pub users: Arc<crate::users::Users>,
    pub registry: GraphRegistry,
    pub checkpointer: Arc<dyn Checkpointer>,
    pub manager: RunManager,
    /// Flight Recorder journal persistence (`GET /runs/{id}/events`).
    pub server_store: Arc<dyn ServerStore>,
    pub queue_cap: usize,
    pub log_capacity: usize,
    /// The server's drain control (R0.6 wave 2c): threaded into every run's
    /// executor, which observes it at super-step boundaries.
    pub shutdown: tokio_util::sync::CancellationToken,
    /// The deployment's default environment tag (R0.11 wave 2): the
    /// promotion target a registry-bound run resolves against when its
    /// binding names no environment (`None`: the untagged surface).
    pub default_environment_tag: Option<rusty_agent_runtime::learn::EnvironmentTag>,
    /// The deployment's context policy, applied to every run (see
    /// `ServerConfig::context_policy`).
    pub context_policy: Option<rusty_agent_runtime::context::ContextPolicy>,
    /// The repair ledger every run's in-loop repairs land on (`GET /repairs`).
    pub repair_ledger: Arc<rusty_agent_runtime::repair::FileRepairLedger>,
    /// The judge that verifies a completed run's outcome, when configured.
    pub verifier: Option<crate::verify_outcome::Verifier>,
    /// Where verdicts are kept and served from.
    pub verifications: Arc<crate::verify_outcome::VerificationPlane>,
    /// Where a run that paused to ask before an irreversible effect is
    /// recorded, and the decision that resumed it.
    pub approvals: Arc<crate::approvals::ApprovalPlane>,
    /// The connection tools, for naming the connection a paused request
    /// runs through.
    pub connection_tools: Option<Arc<crate::connectors::ConnectionTools>>,
    /// The assignments plane: a round's end wakes its driver.
    pub assignments: Option<Arc<crate::assignments::AssignmentPlane>>,
    /// What runs after a verdict lands — the post-run review, set once the
    /// server state exists (it needs the state; the state holds these
    /// deps). Absent in tests and before boot: nothing runs.
    pub after_verdict: Arc<std::sync::OnceLock<AfterVerdict>>,
}

/// The result of successfully scheduling a run: everything an endpoint
/// needs to answer (background ack, wait, or stream).
/// The hook a landed verdict fires.
pub(crate) type AfterVerdict = Arc<dyn Fn(crate::post_run_review::ReviewInput) + Send + Sync>;

pub(crate) struct Scheduled {
    pub run_id: String,
    pub status: RunStatus,
    pub terminal: watch::Receiver<Option<Value>>,
    pub broadcast: broadcast::Receiver<SseFrame>,
    pub replay: Vec<SseFrame>,
}

/// Create a run handle, apply the multitask strategy, and spawn execution
/// immediately when the thread slot is free.
///
/// `thread_id` is the internal (tenant-scoped) id used for the checkpointer,
/// executor, and RunManager bookkeeping; `wire_thread_id` is the external id
/// reported in SSE frames and terminal JSON.
pub(crate) async fn schedule(
    deps: &RunDeps,
    thread_id: &str,
    wire_thread_id: &str,
    graph: &str,
    payload: RunPayload,
    strategy: MultitaskStrategy,
) -> Result<Scheduled, ApiError> {
    // A draining server must not take new runs: the token would cancel
    // them at their first boundary anyway, and a 503 lets the caller (or
    // its load balancer) retry against a pod that is still serving.
    if deps.shutdown.is_cancelled() {
        return Err(ApiError::shutting_down(format!(
            "server is draining; resubmit run on thread `{wire_thread_id}` against a running instance"
        )));
    }
    let run_id = uuid::Uuid::new_v4().to_string();
    let (admission, deployment) =
        resolve_admissions(deps, &run_id, thread_id, graph, &payload).await?;
    let (bcast_tx, _bcast_rx) = broadcast::channel(256);
    let (terminal_tx, terminal_rx) = watch::channel(None);
    let handle = RunHandle {
        run_id: run_id.clone(),
        thread_id: thread_id.to_string(),
        wire_thread_id: wire_thread_id.to_string(),
        graph: graph.to_string(),
        attempt: 0, // assigned by RunManager::insert
        status: RunStatus::Pending,
        payload,
        created_at: chrono::Utc::now(),
        admission,
        deployment,
        sink: FrameSink::new(deps.log_capacity, bcast_tx),
        terminal: terminal_tx,
        checkpoint_ids: Arc::new(StdMutex::new(Vec::new())),
        // A child of the server drain token: the drain still stops every
        // run, and a run-level cancel (R0.7 wave 2) stops only this one.
        cancel: deps.shutdown.child_token(),
    };
    // Every accepted run leaves a durable record of exactly what was
    // accepted — the run's identity after this process is gone (`GET
    // /runs/{id}`, a case derived from it). A run the server cannot record
    // is not accepted.
    let accepted = crate::accepted_runs::AcceptedRunRecord {
        run_id: run_id.clone(),
        thread_id: thread_id.to_string(),
        wire_thread_id: wire_thread_id.to_string(),
        tenant: crate::auth::tenant_of_internal(thread_id).to_string(),
        graph: graph.to_string(),
        payload: handle.payload.clone(),
        accepted_at: handle.created_at,
    };
    if let Err(error) = deps.server_store.put_accepted_run(&accepted).await {
        return Err(ApiError::internal(format!(
            "failed to record accepted run `{run_id}`: {error}"
        )));
    }
    // Subscribe/snapshot before any execution can emit frames.
    let replay = handle.log_snapshot();
    let broadcast = handle.subscribe();
    // The durable queue record is staged only under the enqueue strategy —
    // the one path that can park the run — and persisted only when the run
    // actually lands in the FIFO: the active run's durability is the
    // checkpoint log's job, not this record's.
    let staged = matches!(strategy, MultitaskStrategy::Enqueue)
        .then(|| (handle.payload.clone(), handle.created_at));

    let decision = deps
        .manager
        .insert(handle, strategy, deps.queue_cap)
        .await?;
    let status = match decision {
        ScheduleDecision::Started => {
            spawn_execute(deps.clone(), run_id.clone());
            RunStatus::Running
        }
        ScheduleDecision::Queued(seq) => {
            let (payload, enqueued_at) = staged.expect("enqueue strategy staged the record");
            let record = PendingRunRecord {
                run_id: run_id.clone(),
                thread_id: thread_id.to_string(),
                wire_thread_id: wire_thread_id.to_string(),
                tenant: crate::auth::tenant_of_internal(thread_id).to_string(),
                graph: graph.to_string(),
                payload,
                seq,
                enqueued_at,
            };
            // A run the server cannot persist must not be accepted — the
            // accepted-but-forgotten window is exactly the gap the durable
            // queue closes — so a persist failure fails the submission and
            // the in-memory insert rolls back with it.
            if let Err(error) = deps.server_store.put_pending_run(&record).await {
                deps.manager.remove_queued(&run_id).await;
                return Err(ApiError::internal(format!(
                    "failed to persist queued run `{run_id}`: {error}"
                )));
            }
            // A promotion can outrun the persist (the active run finished
            // while the record was landing): the promotion path's delete
            // then found nothing to delete, so clear the straggler here —
            // an active run must not leave a queue record behind for the
            // next boot to restore.
            if !matches!(deps.manager.info(&run_id).await, Some(info) if info.status == RunStatus::Pending)
            {
                clear_pending_record(&deps.server_store, &run_id).await;
            }
            RunStatus::Pending
        }
    };

    Ok(Scheduled {
        run_id,
        status,
        terminal: terminal_rx,
        broadcast,
        replay,
    })
}

/// The registry (R0.11 wave 2) and deployment (R0.12 wave 3) admission
/// resolutions every scheduling path performs — fresh submissions and
/// boot restores alike, so a restored run binds exactly as if just
/// enqueued.
///
/// Registry admission: the binding resolves now, at admission — a
/// promotion landing afterwards never reaches this run (the conservatism
/// checkpoint pinning has kept since R0.7), and a queued run binds at
/// admission, not at dequeue. The tenant comes from the internal thread
/// id, so every entry point (HTTP, cron, trigger, bridge, restore)
/// resolves in the submitter's namespace without threading the request
/// context through. A resolution failure is an admission failure: the
/// run never enters the manager.
///
/// Deployment admission: the environment's pointer binds a revision now,
/// at admission — the registry admission's conservatism, lifted to
/// deployments. The revision's identity checks against the registered
/// graph (name and current topology hash), so a build the revision no
/// longer describes is refused, never run.
async fn resolve_admissions(
    deps: &RunDeps,
    run_id: &str,
    thread_id: &str,
    graph: &str,
    payload: &RunPayload,
) -> Result<
    (
        Option<crate::registry::RegistryAdmission>,
        Option<crate::deploy::DeploymentAdmission>,
    ),
    ApiError,
> {
    let admission = match &payload.registry {
        Some(binding) => Some(
            crate::registry::resolve_admission(
                &deps.server_store,
                crate::auth::tenant_of_internal(thread_id),
                deps.default_environment_tag.as_ref(),
                run_id,
                binding,
            )
            .await?,
        ),
        None => None,
    };
    let deployment = match &payload.deployment {
        Some(binding) => {
            let (graph_obj, _spec) = deps.registry.get(graph).ok_or_else(|| {
                ApiError::internal(format!(
                    "graph `{graph}` left the registry between route validation and admission"
                ))
            })?;
            Some(
                crate::deploy::resolve_admission(
                    &deps.server_store,
                    crate::auth::tenant_of_internal(thread_id),
                    run_id,
                    binding,
                    graph,
                    &graph_obj.topology_hash(),
                )
                .await?,
            )
        }
        None => None,
    };
    Ok((admission, deployment))
}

/// Cancel one run (the store-backed form of [`RunManager::cancel_run`]):
/// a queued run that never starts must also lose its durable queue
/// record, or the next boot would restore a run the caller cancelled.
pub(crate) async fn cancel_run(deps: &RunDeps, run_id: &str) -> RunCancel {
    let outcome = deps.manager.cancel_run(run_id).await;
    if outcome == RunCancel::CancelledQueued {
        clear_pending_record(&deps.server_store, run_id).await;
    }
    outcome
}

/// Cancel every run of one thread (the store-backed form of
/// [`RunManager::cancel_thread_runs`]): every dequeued run's durable
/// queue record clears with it.
pub(crate) async fn cancel_thread_runs(deps: &RunDeps, thread_id: &str) -> ThreadCancellation {
    let outcome = deps.manager.cancel_thread_runs(thread_id).await;
    for run_id in &outcome.cancelled {
        clear_pending_record(&deps.server_store, run_id).await;
    }
    outcome
}

/// Best-effort delete of a queued run's durable record. A failure is
/// logged, not raised: the record is reconcile-on-read — boot's restore
/// dedupes against the journal and clears stragglers itself.
async fn clear_pending_record(server_store: &Arc<dyn ServerStore>, run_id: &str) {
    if let Err(error) = server_store.delete_pending_run(run_id).await {
        tracing::warn!(%run_id, %error, "pending-run record delete failed");
    }
}

/// Replay the store's pending-run records into the scheduler — the boot
/// half of the durable queue, spawned once per process. Records restore
/// in per-thread FIFO order (`seq`), each scheduling exactly as if just
/// enqueued: a free thread slot starts the run immediately, in
/// **background** — its SSE client died with the last process, so frames
/// flow to the journal and frame log and clients reattach via
/// `GET /runs/{id}/stream` — and a busy one queues behind the active run.
pub(crate) async fn restore_pending_runs(deps: RunDeps) {
    // A draining server restores nothing: the records stay for a process
    // that is still serving (the same rule schedule()'s 503 enforces).
    if deps.shutdown.is_cancelled() {
        return;
    }
    let mut records = match deps.server_store.list_pending_runs().await {
        Ok(records) => records,
        Err(error) => {
            // The queue's accept state has no other copy — a boot that
            // cannot read it must say so loudly, not start empty.
            tracing::warn!(%error, "pending-run restore skipped: store listing failed");
            return;
        }
    };
    if records.is_empty() {
        return;
    }
    crate::pending_runs::sort(&mut records);
    // The depth cap guards live submissions. The runs being restored were
    // admitted legally under the cap of their own process; a restart with
    // a shrunken cap must not strand them, so each thread's restore cap
    // floors at its surviving queue depth.
    let mut depth_by_thread: HashMap<String, usize> = HashMap::new();
    for record in &records {
        *depth_by_thread.entry(record.thread_id.clone()).or_insert(0) += 1;
    }
    let mut restored = 0usize;
    for record in records {
        let cap = deps
            .queue_cap
            .max(depth_by_thread[record.thread_id.as_str()]);
        if restore_one(&deps, record, cap).await {
            restored += 1;
        }
    }
    tracing::info!(restored, "pending runs restored from the store");
}

/// Restore one record: dedupe defensively, re-resolve admissions, insert,
/// and start immediately when the thread slot is free. `false` means the
/// record stays for the next boot (a deferral, never a silent drop).
async fn restore_one(deps: &RunDeps, record: PendingRunRecord, queue_cap: usize) -> bool {
    let run_id = record.run_id.clone();
    // Dedupe before anything else: a record can outlive its run — a crash
    // mid-promotion (the promotion path's delete lost the race) or a
    // best-effort delete that failed. Any run that executed at all has a
    // journal (flushed at the first checkpoint boundary and again at
    // completion), so a journal under this run id means the queue entry
    // is a zombie: clear it, never double-schedule.
    match deps.server_store.get_journal(&run_id).await {
        Ok(Some(_)) => {
            tracing::info!(%run_id, "skipping pending-run restore: the run already has a journal");
            clear_pending_record(&deps.server_store, &run_id).await;
            return false;
        }
        Ok(None) => {}
        Err(error) => {
            // Indeterminate: defer to the next boot rather than risk
            // double-scheduling.
            tracing::warn!(%run_id, %error, "pending-run restore deferred: journal read failed");
            return false;
        }
    }
    if deps.manager.info(&run_id).await.is_some() {
        // Two routers over one store in a single process (an embedding,
        // not the shipped server) restore the same records; the first
        // wins.
        tracing::info!(%run_id, "skipping pending-run restore: the run is already scheduled");
        return false;
    }
    let (admission, deployment) = match resolve_admissions(
        deps,
        &run_id,
        &record.thread_id,
        &record.graph,
        &record.payload,
    )
    .await
    {
        Ok(resolved) => resolved,
        Err(error) => {
            // Defer, keeping the record: the binding may resolve under a
            // later deployment, and dropping it here would strand
            // accepted work.
            tracing::warn!(%run_id, %error, "pending-run restore deferred: admission failed");
            return false;
        }
    };
    let (bcast_tx, _bcast_rx) = broadcast::channel(256);
    let (terminal_tx, _terminal_rx) = watch::channel(None);
    let handle = RunHandle {
        run_id: run_id.clone(),
        thread_id: record.thread_id,
        wire_thread_id: record.wire_thread_id,
        graph: record.graph,
        attempt: 0, // assigned by RunManager::insert
        status: RunStatus::Pending,
        payload: record.payload,
        created_at: record.enqueued_at,
        admission,
        deployment,
        sink: FrameSink::new(deps.log_capacity, bcast_tx),
        terminal: terminal_tx,
        checkpoint_ids: Arc::new(StdMutex::new(Vec::new())),
        // A child of the server drain token, exactly as at schedule time:
        // a drain starting mid-restore stops the restored run at its
        // first boundary like any other.
        cancel: deps.shutdown.child_token(),
    };
    match deps
        .manager
        .insert(handle, MultitaskStrategy::Enqueue, queue_cap)
        .await
    {
        Ok(ScheduleDecision::Started) => {
            // The thread freed between restarts: the run starts now. Its
            // record leaves the store with the promotion, same as the
            // live promotion path.
            clear_pending_record(&deps.server_store, &run_id).await;
            spawn_execute(deps.clone(), run_id);
            true
        }
        // Still queued — its record stays, mirroring the live enqueue
        // path's persist.
        Ok(ScheduleDecision::Queued(_)) => true,
        Err(error) => {
            tracing::warn!(%run_id, %error, "pending-run restore deferred: the queue refused the run");
            false
        }
    }
}

/// The initial state for a plain (non-resume, non-time-travel) run: the
/// thread's latest checkpoint state with `input` merged into it, or the
/// input alone on a thread with no history. Declared channels merge
/// through their reducers; undeclared channels seed the state directly —
/// the same latitude a fresh run's input has always had. A merge a
/// reducer refuses (an aggregating reducer over a non-array current
/// value) is the caller's error to surface.
/// What the turn-start repair found: the calls it answered and the
/// checkpoint the gap was in.
struct TurnRepair {
    calls: Vec<rusty_agent_runtime::react::RepairedCall>,
    checkpoint_id: String,
}

async fn continued_state(
    checkpointer: &Arc<dyn Checkpointer>,
    thread_id: &str,
    input: Option<&Value>,
    spec: &rusty_agent_runtime::state::StateSpec,
    repeatable: &(dyn Fn(&str) -> bool + Sync),
) -> std::result::Result<(State, Option<TurnRepair>), RustyError> {
    let input_state = input
        .cloned()
        .and_then(|v| State::from_value(v).ok())
        .unwrap_or_default();
    let Some(checkpoint) = checkpointer.get_latest(thread_id).await? else {
        return Ok((input_state, None));
    };
    let mut state = checkpoint.state;
    // A thread whose last step left tool calls without results — a crash, a
    // stopped loop, a failed tools step — is answered before anything new
    // joins it: a read's result is marked lost, a write's outcome unknown.
    // The repaired messages are the run's initial state, journaled with its
    // first step, so the model reads only what the log holds.
    let mut repair = None;
    if let Ok(Some(mut messages)) = state.get_as::<Vec<rusty_agent_runtime::llm::ChatMessage>>(
        rusty_agent_runtime::react::MESSAGES_CHANNEL,
    ) {
        let calls =
            rusty_agent_runtime::react::repair_unpaired_tool_calls(&mut messages, repeatable);
        if !calls.is_empty() {
            state.insert(
                rusty_agent_runtime::react::MESSAGES_CHANNEL,
                serde_json::to_value(&messages)?,
            );
            repair = Some(TurnRepair {
                calls,
                checkpoint_id: checkpoint.id.clone(),
            });
        }
    }
    let mut reduced: HashMap<String, Value> = HashMap::new();
    for (channel, value) in input_state.iter() {
        if spec.try_reducer_for(channel).is_some() {
            reduced.insert(channel.to_owned(), value.clone());
        } else {
            state.insert(channel, value.clone());
        }
    }
    if !reduced.is_empty() {
        spec.apply_single(&mut state, "input", reduced)?;
    }
    Ok((state, repair))
}

/// Ask the judge about a run that stopped: by the evidence of the thread,
/// against the graph's catalog. Nothing without a judge.
/// Returns the verdict with the messages the judge read: the thread, and —
/// when the run read memory — the notes it was shown, as one REMEMBERED
/// message in the turn, so what the agent remembered is evidence the
/// judge weighs and a later re-judging reads the same.
async fn verify_done(
    deps: &RunDeps,
    snap: &RunSnapshot,
    state: &State,
    journal: &Journal,
) -> Option<(
    crate::verify_outcome::Verdict,
    Vec<rusty_agent_runtime::llm::ChatMessage>,
)> {
    let judge = deps.verifier.as_ref()?;
    let mut messages: Vec<rusty_agent_runtime::llm::ChatMessage> = state
        .get_as(rusty_agent_runtime::react::MESSAGES_CHANNEL)
        .ok()
        .flatten()
        .unwrap_or_default();
    let charter = messages
        .first()
        .filter(|m| m.role == rusty_agent_runtime::llm::Role::System)
        .and_then(|m| m.content.clone());
    let remembered = crate::verify_outcome::remembered_in(journal);
    let mut names: HashMap<String, String> = HashMap::new();
    for record in &remembered {
        if let rusty_agent_runtime::memory::ProvenanceAuthor::Agent { agent_id } =
            &record.provenance.author
        {
            if !names.contains_key(agent_id) {
                if let Ok(Some(agent)) = deps.server_store.get_assistant(agent_id).await {
                    names.insert(agent_id.clone(), agent.name);
                }
            }
        }
    }
    if let Some(remembered) = crate::verify_outcome::remembered_message(&remembered, &names) {
        let at = messages
            .iter()
            .rposition(|m| m.role == rusty_agent_runtime::llm::Role::User)
            .map(|i| i + 1)
            .unwrap_or(messages.len());
        messages.insert(at, remembered);
    }
    let catalog = deps.registry.tool_capabilities(&snap.graph);
    let mut verdict =
        crate::verify_outcome::verify(judge.0.as_ref(), charter.as_deref(), &messages, &catalog)
            .await;
    verdict.graph = Some(snap.graph.clone());
    Some((verdict, messages))
}

/// One outcome-repair episode on the ledger: the judge's word that sent
/// the model back, and how the second verdict came out.
fn record_outcome_repair(
    deps: &RunDeps,
    thread_id: &str,
    first: &crate::verify_outcome::Verdict,
    second: Option<&crate::verify_outcome::Verdict>,
) {
    use rusty_agent_runtime::repair::{
        RepairAction, RepairComponent, RepairLedger, RepairOutcome, RepairRecordBuilder,
        RepairRung, RepairTrigger,
    };
    let now = chrono::Utc::now();
    let outcome = match second.map(|v| v.verdict.as_str()) {
        Some("verified") => RepairOutcome::Repaired,
        _ => RepairOutcome::Failed,
    };
    let mut builder = RepairRecordBuilder::new()
        .component(RepairComponent::OutcomeVerifier)
        .trigger(RepairTrigger::OutcomeNotAchieved {
            reason: first.reason.clone(),
        })
        .action(RepairAction::RepairTurn {
            rung: RepairRung::InTurn,
        })
        .outcome(outcome)
        .start_time(first.at)
        .end_time(now)
        .session_id(thread_id)
        .attempt_count(1)
        .citation(format!("first:{}", first.reason));
    if let Some(second) = second {
        builder = builder.citation(format!("second:{}:{}", second.verdict, second.reason));
    }
    if let Err(error) = deps.repair_ledger.append(builder.build()) {
        tracing::warn!(%error, "outcome repair: the repair record was not appended");
    }
}

/// One crash-repair episode on the ledger: which calls were answered, in
/// which checkpoint the gap was.
fn record_turn_repair(deps: &RunDeps, thread_id: &str, repair: &TurnRepair) {
    use rusty_agent_runtime::repair::{
        RepairAction, RepairComponent, RepairLedger, RepairOutcome, RepairRecordBuilder,
        RepairRung, RepairTrigger,
    };
    let now = chrono::Utc::now();
    let mut builder = RepairRecordBuilder::new()
        .component(RepairComponent::CrashRepair)
        .trigger(RepairTrigger::Crash {
            kill_point: "tools_step".to_owned(),
            last_checkpoint_id: Some(repair.checkpoint_id.clone()),
        })
        .action(RepairAction::CrashRepairWalk {
            rung: RepairRung::Subsystem,
        })
        .outcome(RepairOutcome::Repaired)
        .start_time(now)
        .end_time(now)
        .session_id(thread_id)
        .attempt_count(repair.calls.len() as u32);
    for call in &repair.calls {
        builder = builder.citation(format!(
            "{}:{}:{}",
            call.tool_call_id,
            call.tool,
            if call.repeatable { "lost" } else { "unknown" }
        ));
    }
    if let Err(error) = deps.repair_ledger.append(builder.build()) {
        tracing::warn!(%error, "turn repair: the repair record was not appended");
    }
}

/// Drive one run to its terminal state and chain the next queued run.
async fn execute(deps: RunDeps, run_id: String) {
    let Some(snap) = deps.manager.snapshot(&run_id).await else {
        tracing::warn!(%run_id, "scheduled run vanished before execution");
        return;
    };
    let sink = snap.sink.clone();
    sink.push(
        "metadata",
        0,
        json!({
            "run_id": run_id,
            "thread_id": snap.wire_thread_id,
            "graph": snap.graph,
            "attempt": snap.attempt,
            "metadata": snap.payload.metadata,
        }),
    );

    let Some((graph, spec)) = deps.registry.get(&snap.graph) else {
        let message = format!("graph `{}` is no longer registered", snap.graph);
        tracing::error!(%run_id, %message);
        sink.push(
            "error",
            0,
            json!({"error": "unknown_graph", "message": message}),
        );
        sink.push("end", 0, json!({"status": "error"}));
        let terminal = json!({
            "run_id": run_id,
            "thread_id": snap.wire_thread_id,
            "status": "error",
            "error": "unknown_graph",
            "message": message,
        });
        terminate(&deps, &run_id, RunStatus::Error, terminal).await;
        return;
    };

    let modes: Vec<String> = snap
        .payload
        .stream_mode
        .clone()
        .unwrap_or_else(|| vec!["values".to_string(), "updates".to_string()]);
    // Flight Recorder: one journal per run, keyed by the server-minted run
    // id. Events carry the external (wire) thread id — the internal
    // tenant-scoped id must never appear in served evidence. The journal's
    // clock is the default system clock, so timestamps match pre-R0.5
    // behavior; attaching it makes the executor read time through it.
    let journal = Journal::new(run_id.clone(), snap.wire_thread_id.clone(), Clock::System);
    let (evt_tx, evt_rx) = mpsc::channel::<GraphEvent>(256);
    let forwarder = tokio::spawn(forward_events(
        evt_rx,
        sink.clone(),
        ForwardDeps {
            checkpointer: Arc::clone(&deps.checkpointer),
            server_store: Arc::clone(&deps.server_store),
            journal: journal.clone(),
            thread_id: snap.thread_id.clone(),
            checkpoint_ids: Arc::clone(&snap.checkpoint_ids),
            modes,
        },
    ));

    let mut config = RunConfig::new(snap.thread_id.clone())
        .with_event_tx(evt_tx)
        .with_journal(journal.clone())
        // Cancellation hook: this run's own token (a child of the server
        // drain token). When either fires, the run stops at its next
        // super-step boundary — a point where a checkpoint was just
        // persisted — instead of being torn down mid-step.
        .with_cancellation(snap.cancel.clone());
    if let Some(blocks) = snap
        .payload
        .config
        .as_ref()
        .and_then(|c| c.memory_blocks.clone())
    {
        config = config.with_memory_blocks(blocks);
    }
    // Registry admission (R0.11 wave 2): the binding resolved at schedule
    // time becomes evidence now, ahead of the run's own events — one
    // `config_resolved` per artifact (chained: each resolution's parent
    // is the previous, so the admission reads as one causal unit) — and
    // the resolved manifest stamps every checkpoint header, which is how
    // the receipt reads it back. Resolution decides nothing about what
    // will run, so the events are read-only (the `CapsuleResolved`
    // precedent); a serialization failure here is a bug, not a runtime
    // condition — the payload type is the server's own.
    let mut parent = None;
    if let Some(admission) = &snap.admission {
        for resolution in &admission.resolutions {
            let output =
                serde_json::to_value(resolution).expect("ConfigResolution always serializes");
            let mut draft =
                EventDraft::new(RunEventKind::ConfigResolved, Effect::ReadOnly).output(output);
            if let Some(parent) = parent {
                draft = draft.parent(parent);
            }
            parent = Some(journal.record(draft));
        }
        config = config.with_manifest(admission.manifest.clone());
    }
    // Deployment admission (R0.12 wave 3): the revision the environment's
    // pointer bound becomes evidence the same way — one
    // `deployment_resolved`, chained after the registry resolutions, so
    // the receipt's walk reads journal head → this event → the bound
    // revision → its frozen pins. Read-only, like the resolutions above.
    if let Some(deployment) = &snap.deployment {
        let output =
            serde_json::to_value(&deployment.resolution).expect("DeploymentResolved serializes");
        let mut draft =
            EventDraft::new(RunEventKind::DeploymentResolved, Effect::ReadOnly).output(output);
        if let Some(parent) = parent {
            draft = draft.parent(parent);
        }
        journal.record(draft);
    }
    if let Some(command) = &snap.payload.command {
        if let Some(resume) = &command.resume {
            // An approving decision carries the tokens the tools node's
            // calls are admitted on; they ride the resumed run's config.
            let tokens = crate::approvals::tokens_in(resume);
            if !tokens.is_empty() {
                config = config.with_effect_approvals(tokens);
            }
            config = config.with_resume(resume.clone());
        }
    }
    if let Some(checkpoint) = &snap.payload.checkpoint {
        config = config.with_checkpoint_id(checkpoint.checkpoint_id.clone());
    }
    if let Some(run_cfg) = &snap.payload.config {
        if let Some(limit) = run_cfg.recursion_limit {
            config = config.with_max_steps(limit);
        }
        if let Some(instructions) = &run_cfg.instructions {
            config = config.with_instructions(instructions.clone());
        }
        if let Some(model) = &run_cfg.model {
            config = config.with_model(model.clone());
        }
        if let Some(model) = &run_cfg.fallback_model {
            config = config.with_fallback_model(model.clone());
        }
        if let Some(t) = run_cfg
            .temperature
            .filter(|t| t.is_finite() && (0.0..=2.0).contains(t))
        {
            config = config.with_temperature(t);
        }
        if let Some(skills) = &run_cfg.skills {
            config.skills = Some(skills.clone());
        }
        if let Some(budget) = &run_cfg.budget {
            config = config.with_budget(*budget);
        }
    }
    // The deployment's context policy: the agent node assembles every model
    // call through it — charter pinned, history compacted, tools budgeted.
    // Under a policy the tools section walks the governed path: the
    // builder's overlays (a when-to-use note per tool) and the tenant's
    // recent tool outcomes ride in, the shortlist ranks against them, and
    // the ranking is journaled with the request.
    if let Some(policy) = &deps.context_policy {
        // The agent's own context, where the builder set one.
        let mut tailored = snap
            .payload
            .config
            .as_ref()
            .and_then(|c| c.context.as_ref())
            .filter(|c| !c.is_empty())
            .map(|c| c.apply(policy));
        // Lane-one recall ranks with the utility index this process holds,
        // named by its stamp so the journaled request says which one.
        if let Some(stamp) = crate::memory_utility::cached_stamp() {
            let mut with_stamp = tailored.take().unwrap_or_else(|| policy.clone());
            if let Some(section) = with_stamp.memory.as_mut() {
                section.query.utility_stamp = Some(stamp);
            }
            tailored = Some(with_stamp);
        }
        // The agent's memory blocks ride in the task section: it gets the
        // room they need (plus the situation's own lines), so an agent
        // with full blocks runs at a small window instead of failing on
        // "the task section does not fit".
        if let Some(blocks) = snap
            .payload
            .config
            .as_ref()
            .and_then(|c| c.memory_blocks.as_deref())
        {
            let base = tailored.take().unwrap_or_else(|| policy.clone());
            tailored = Some(base.with_task_room(blocks.len() + 512));
        }
        config = config
            .with_context_policy(tailored.as_ref().unwrap_or(policy))
            .with_compaction_state();
        let overlays: BTreeMap<String, rusty_agent_runtime::tool_select::ToolSelectionOverlay> =
            snap.payload
                .config
                .as_ref()
                .and_then(|c| c.tool_overlays.as_ref())
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
        config = config.with_tool_overlays(&overlays);
        let outcomes = recent_tool_outcomes(&deps).await;
        config = config.with_tool_outcomes(&outcomes);
    }
    // In-loop repairs — a stuck turn the tools node catches — are audited on
    // the same ledger as every other repair.
    let ledger: Arc<dyn rusty_agent_runtime::repair::RepairLedger> = deps.repair_ledger.clone();
    config = config.with_repair_ledger(rusty_agent_runtime::repair::RepairLedgerHandle(ledger));
    let created_by = snap
        .payload
        .metadata
        .as_ref()
        .and_then(|m| m.get("created_by"))
        .cloned();
    if let Some(created_by) = &created_by {
        config = config.with_attribution(created_by.clone());
    }
    if let Some(assistant_id) = &snap.payload.assistant_id {
        config = config.with_agent_id(assistant_id.clone());
    }
    // The person this run is a conversation with: the signed-in person who
    // sent the message. A run a schedule or a webhook fired is nobody's
    // conversation — whoever created the channel is its attribution, not
    // its counterpart — so it acts for the agent: what it remembers and
    // reads is the agent's own.
    // Fired means a schedule, a webhook or the queue started it. An
    // evaluation replaying a person's case, or another agent asking on a
    // person's behalf, is not fired: it names the person and is theirs.
    let channel_fired = snap.payload.metadata.as_ref().is_some_and(|m| {
        ["cron_id", "trigger_id", "task_id", "trigger"]
            .iter()
            .any(|k| m.get(k).is_some())
            || matches!(
                m.get("channel").and_then(Value::as_str),
                Some("schedule") | Some("webhook") | Some("pool")
            )
    });
    // The person the run acts for: the server's execution block says so
    // when admission wrote one (its `subject`); a resumed run's
    // `on_behalf_of` and the `created_by` are the older forms, both the
    // server's — a client's are refused at admission.
    let subject = snap
        .payload
        .metadata
        .as_ref()
        .and_then(|m| m.pointer("/execution/subject"))
        .cloned()
        .filter(|v| v.is_object());
    let on_behalf_of = snap
        .payload
        .metadata
        .as_ref()
        .and_then(|m| m.get("on_behalf_of"))
        .cloned();
    let person = subject
        .as_ref()
        .or(on_behalf_of.as_ref())
        .or(created_by.as_ref())
        .filter(|_| !channel_fired)
        .filter(|c| c.get("kind").and_then(Value::as_str) == Some("user"))
        .and_then(|c| c.get("principal_id").and_then(Value::as_str))
        .map(str::to_owned);
    config = match &person {
        Some(person) => config.with_acting_for(person.clone()),
        None => config.with_no_counterpart(),
    };
    // The execution block every tool reads: admission's, when it stamped
    // one; else synthesized here from what the server's own path wrote —
    // the tenant from the thread's id, the actor from `created_by`, the
    // subject from the person derived above, the path from the channel.
    let execution = snap
        .payload
        .metadata
        .as_ref()
        .and_then(|m| m.get("execution"))
        .cloned()
        .filter(|e| e.is_object())
        .unwrap_or_else(|| {
            let via = snap
                .payload
                .metadata
                .as_ref()
                .and_then(|m| m.get("channel"))
                .and_then(Value::as_str)
                .unwrap_or("http")
                .to_owned();
            json!({
                "tenant": crate::auth::tenant_of_internal(&snap.thread_id),
                "actor": created_by.clone().unwrap_or(Value::Null),
                "subject": subject.clone().or(on_behalf_of.clone()).or(created_by.clone()).unwrap_or(Value::Null),
                "via": via,
            })
        });
    // A run in a world says so where every tool reads it.
    let execution = match snap.payload.config.as_ref().and_then(|c| c.world.clone()) {
        Some(world) => {
            let mut block = execution;
            block["world"] = json!(world);
            if let Some(worlds) = snap
                .payload
                .config
                .as_ref()
                .and_then(|c| c.worlds.clone())
                .filter(|w| w.len() > 1)
            {
                block["worlds"] = json!(worlds);
            }
            block
        }
        None => execution,
    };
    // The floor under every path: a run that would act for a person the
    // deployment no longer holds does not start, whoever admitted it.
    let acts_for = execution
        .get("subject")
        .filter(|s| s.is_object())
        .or_else(|| execution.get("actor"))
        .cloned();
    if let Some(who) = acts_for {
        if !deps.users.still_present(&who) {
            let message =
                format!(
                "this run acts for a person the deployment no longer holds ({}); it will not run",
                who.get("principal_id").and_then(Value::as_str).unwrap_or("?")
            );
            tracing::warn!(%run_id, %message);
            sink.push(
                "error",
                0,
                json!({"error": "person_gone", "message": message}),
            );
            sink.push("end", 0, json!({"status": "error"}));
            let terminal = json!({
                "run_id": run_id,
                "thread_id": snap.wire_thread_id,
                "status": "error",
                "error": "person_gone",
                "message": message,
            });
            terminate(&deps, &run_id, RunStatus::Error, terminal).await;
            return;
        }
    }
    config = config.with_execution(execution);
    // What the run remembers: when the deployment's policy has a memory
    // section, every model call carries what this agent has learned and
    // what it knows about the person the run is for — and nothing about
    // anyone else. The store is the tenant's; the source narrows it to
    // those two scopes before a record can reach the pipeline.
    // The builder's switch: an agent with long-term memory off gets no
    // memory store on its runs, whatever the deployment's policy says.
    let memory_off = snap
        .payload
        .config
        .as_ref()
        .and_then(|c| c.context.as_ref())
        .is_some_and(|c| c.memory_off());
    if !memory_off
        && deps
            .context_policy
            .as_ref()
            .is_some_and(|p| p.memory.is_some())
    {
        let tenant = crate::auth::tenant_of_internal(&snap.thread_id).to_string();
        let mut scopes = Vec::new();
        if let Some(person) = &person {
            scopes.push(ScopeAddress::new(MemoryScope::User, person.clone()));
        }
        if let Some(assistant_id) = &snap.payload.assistant_id {
            scopes.push(ScopeAddress::new(MemoryScope::Agent, assistant_id.clone()));
        }
        let store: Arc<dyn MemoryStore> = Arc::new(crate::learn::ServerMemoryStore::new(
            Arc::clone(&deps.server_store),
            &tenant,
        ));
        config = config.with_memory_source(MemorySource::Store(Arc::new(
            crate::memory_scope::ScopedMemoryStore::new(store, scopes),
        )));
    }
    if let Some(run_cfg) = &snap.payload.config {
        // The tools the run was told never to call are held by a guard:
        // the model still sees them, an attempt is refused and journaled.
        if let Some(forbidden) = run_cfg.forbidden_tools.as_ref().filter(|f| !f.is_empty()) {
            config =
                config.with_tool_guards(vec![Arc::new(ForbiddenTools::new(forbidden.clone()))]);
        }
        // Capability admission: the selection validated at schedule time
        // becomes the run's exact tool allowlist, and the composed set's
        // content address pins into the manifest alongside the registry
        // and deployment resolutions above. A bare `tool_allowlist`
        // composes an anonymous (reference-free) set so replay can
        // re-validate the selection by the same rule.
        let selection = match &run_cfg.capability_set {
            Some(set) => Some((set.tools.clone(), set.refs())),
            None => run_cfg
                .tool_allowlist
                .clone()
                .map(|tools| (tools, Ok(Vec::new()))),
        };
        if let Some((tools, refs)) = selection {
            let catalog = deps.registry.tool_capabilities(&snap.graph);
            let composed = refs.and_then(|refs| {
                rusty_agent_runtime::capability::CapabilitySet::compose(&tools, &refs, &catalog)
            });
            match composed {
                Ok(set) => {
                    config = config.with_tool_allowlist(set.resolve_allowlist());
                    let manifest = config
                        .manifest
                        .clone()
                        .unwrap_or_default()
                        .pin_capability_set(&set);
                    config = config.with_manifest(manifest);
                }
                Err(error) => {
                    // Admission validated this selection and the registry
                    // is immutable after boot; reaching this branch means
                    // the graph's catalog changed mid-flight.
                    let message = format!(
                        "capability selection validated at admission no longer resolves: {error}"
                    );
                    tracing::error!(%run_id, %message);
                    sink.push(
                        "error",
                        0,
                        json!({"error": "capability_unresolved", "message": message}),
                    );
                    sink.push("end", 0, json!({"status": "error"}));
                    let terminal = json!({
                        "run_id": run_id,
                        "thread_id": snap.wire_thread_id,
                        "status": "error",
                        "error": "capability_unresolved",
                        "message": message,
                    });
                    terminate(&deps, &run_id, RunStatus::Error, terminal).await;
                    return;
                }
            }
        }
    }
    // A new run on a thread that already has history continues the
    // conversation (LangGraph parity): the latest checkpoint's state is the
    // base and the posted input merges into it through the spec's own
    // reducers — `AddMessages` appends the turn, `Append` extends,
    // `LastValue` replaces. Resume and time-travel keep their own restore
    // semantics (the executor discards this state on those paths), and a
    // fresh thread starts from the input alone.
    let plain_turn = snap.payload.checkpoint.is_none()
        && snap
            .payload
            .command
            .as_ref()
            .is_none_or(|command| command.resume.is_none());
    let catalog = deps.registry.tool_capabilities(&snap.graph);
    let repeatable = |tool: &str| {
        catalog
            .iter()
            .find(|capability| capability.name == tool)
            .map(|capability| capability.effect.is_freely_repeatable())
            .unwrap_or(false)
    };
    let initial = if plain_turn {
        continued_state(
            &deps.checkpointer,
            &snap.thread_id,
            snap.payload.input.as_ref(),
            &spec,
            &repeatable,
        )
        .await
        .map(|(state, repair)| {
            if let Some(repair) = &repair {
                tracing::warn!(
                    %run_id,
                    calls = repair.calls.len(),
                    "turn repair: unanswered tool calls answered with notices"
                );
                record_turn_repair(&deps, &snap.thread_id, repair);
            }
            state
        })
    } else {
        Ok(snap
            .payload
            .input
            .clone()
            .and_then(|v| State::from_value(v).ok())
            .unwrap_or_default())
    };
    let initial = match initial {
        Ok(state) => state,
        Err(error) => {
            let message = format!("input does not merge into the thread's state: {error}");
            tracing::warn!(%run_id, %message);
            sink.push(
                "error",
                0,
                json!({"error": "invalid_input", "message": message}),
            );
            sink.push("end", 0, json!({"status": "error"}));
            let terminal = json!({
                "run_id": run_id,
                "thread_id": snap.wire_thread_id,
                "status": "error",
                "error": "invalid_input",
                "message": message,
            });
            terminate(&deps, &run_id, RunStatus::Error, terminal).await;
            return;
        }
    };

    let mut executor = Executor::with_checkpointer(Arc::clone(&deps.checkpointer));
    // The resolved middleware chain (R0.11 wave 4) attaches in journaled
    // order — the same layers the manifest's `middleware` digest pins and
    // the admission resolution's `layers` field names. Attached after the
    // admission journal writes above, so the evidence of *what* serves
    // precedes the run it serves.
    if let Some(admission) = &snap.admission {
        if let Some(chain) = &admission.middleware {
            for layer in chain.layers() {
                executor = executor.layer_shared(Arc::clone(layer));
            }
        }
    }
    // A second config for one repair turn: the model stopped, the judge
    // said the outcome was not achieved, and the model is told so once and
    // runs again on the same thread. Kept only while a judge is there.
    let mut repair_config = deps.verifier.as_ref().map(|_| config.clone());
    let mut result = executor.run(&graph, &spec, initial, config).await;
    // The model stopped; whether the outcome is achieved is a separate
    // question, answered by the evidence and a judge, and kept beside the
    // run rather than in its journal.
    let (mut verification, mut judged) = match &result {
        Ok(ExecutionOutcome::Done(state)) => verify_done(&deps, &snap, state, &journal)
            .await
            .map(|(v, m)| (Some(v), Some(m)))
            .unwrap_or((None, None)),
        _ => (None, None),
    };
    // Sent back: a failed verdict; or one the judge could not settle while
    // nothing wrote and the agent has tools that write — the shape of a
    // claimed-but-absent write a weak judge lets through.
    let could_write = catalog
        .iter()
        .any(|c| !matches!(c.effect, Effect::Pure | Effect::ReadOnly));
    let sent_back = verification
        .as_ref()
        .filter(|v| crate::verify_outcome::sends_back(v, could_write))
        .cloned();
    if let (Some(first), Some(config)) = (sent_back, repair_config.take()) {
        let notice = json!({ rusty_agent_runtime::react::MESSAGES_CHANNEL: [
            rusty_agent_runtime::llm::ChatMessage::system(crate::verify_outcome::repair_notice(&first.verdict, &first.reason)),
        ]});
        match continued_state(
            &deps.checkpointer,
            &snap.thread_id,
            Some(&notice),
            &spec,
            &repeatable,
        )
        .await
        {
            Ok((again, _)) => {
                tracing::info!(%run_id, reason = %first.reason, "outcome not achieved: one repair turn");
                result = executor.run(&graph, &spec, again, config).await;
                if let Ok(ExecutionOutcome::Done(state)) = &result {
                    (verification, judged) = verify_done(&deps, &snap, state, &journal)
                        .await
                        .map(|(mut second, read)| {
                            second.repaired =
                                Some(json!({"verdict": first.verdict, "reason": first.reason}));
                            (Some(second), Some(read))
                        })
                        .unwrap_or((None, None));
                }
                record_outcome_repair(&deps, &snap.thread_id, &first, verification.as_ref());
            }
            Err(error) => {
                tracing::warn!(%run_id, %error, "outcome not achieved, and the thread could not take the repair notice")
            }
        }
    }
    // Every sender is dropped with the runs; the forwarder drains what
    // remains and exits.
    drop(repair_config);
    let _ = forwarder.await;

    // Final journal write: the complete evidence of the run, including the
    // events recorded after the last checkpoint boundary. Persisted before
    // the run goes terminal so `complete: true` on the events endpoint never
    // races ahead of the snapshot it serves. Evidence of a failed run is
    // still evidence — this write happens on every outcome.
    persist_journal(&deps.server_store, &journal).await;

    let step = sink.current_step();
    let (status, terminal) = match result {
        Ok(ExecutionOutcome::Done(state)) => {
            let verification = match verification.take() {
                Some(verdict) => {
                    tracing::info!(%run_id, verdict = %verdict.verdict, reason = %verdict.reason, repaired = verdict.repaired.is_some(), "outcome verified");
                    deps.verifications.persist(&run_id, &verdict).await;
                    // What the judge read, kept beside the verdict for judging again.
                    let messages: Vec<rusty_agent_runtime::llm::ChatMessage> =
                        judged.take().unwrap_or_else(|| {
                            state
                                .get_as::<Vec<rusty_agent_runtime::llm::ChatMessage>>(
                                    rusty_agent_runtime::react::MESSAGES_CHANNEL,
                                )
                                .ok()
                                .flatten()
                                .unwrap_or_default()
                        });
                    deps.verifications
                        .persist_transcript(&run_id, &messages)
                        .await;
                    if let Some(hook) = deps.after_verdict.get() {
                        let person_id = snap
                            .payload
                            .metadata
                            .as_ref()
                            .and_then(|m| m.get("created_by"))
                            .filter(|c| c.get("kind").and_then(Value::as_str) == Some("user"))
                            .and_then(|c| c.get("principal_id"))
                            .and_then(Value::as_str)
                            .map(str::to_owned);
                        let rehearsal = snap.payload.config.as_ref().is_some_and(|c| {
                            c.world.is_some() || c.worlds.as_ref().is_some_and(|w| !w.is_empty())
                        });
                        hook(crate::post_run_review::ReviewInput {
                            run_id: run_id.clone(),
                            verdict: verdict.verdict.clone(),
                            messages,
                            assistant_id: snap.payload.assistant_id.clone(),
                            person_id,
                            rehearsal,
                        });
                    }
                    serde_json::to_value(&verdict).ok()
                }
                None => None,
            };
            let mut end = json!({"status": "success"});
            let mut terminal = json!({
                "run_id": run_id,
                "thread_id": snap.wire_thread_id,
                "status": "success",
                "output": state.to_value(),
                "spend": run_spend(&journal),
            });
            if let Some(verification) = verification {
                end["verification"] = verification.clone();
                terminal["verification"] = verification;
            }
            sink.push("end", step, end);
            (RunStatus::Success, terminal)
        }
        Ok(ExecutionOutcome::Interrupted {
            value,
            state,
            checkpoint_id,
        }) => {
            // A run that stopped to ask before an irreversible effect is a
            // pending approval from here until a person decides.
            crate::approvals::record_if_approval(
                &deps,
                &run_id,
                &snap.wire_thread_id,
                &snap.graph,
                &snap.payload,
                &value,
            )
            .await;
            sink.push(
                "end",
                step,
                json!({"status": "interrupted", "interrupt": value}),
            );
            let terminal = json!({
                "run_id": run_id,
                "thread_id": snap.wire_thread_id,
                "status": "interrupted",
                "interrupt": value,
                "checkpoint_id": checkpoint_id,
                "state": state.to_value(),
                "spend": run_spend(&journal),
            });
            (RunStatus::Interrupted, terminal)
        }
        Err(error @ RustyError::Cancelled(_)) => {
            // Drain, not failure: the executor stopped at a super-step
            // boundary, so the run's last checkpoint is intact and a fresh
            // run on the thread resumes from it. The wire status is
            // `cancelled` — matching the task queue's treatment of
            // cancellation as control flow, never an error.
            let message = error.to_string();
            tracing::info!(%run_id, "run drained at a checkpoint boundary; resumable");
            sink.push("end", step, json!({"status": "cancelled"}));
            let terminal = json!({
                "run_id": run_id,
                "thread_id": snap.wire_thread_id,
                "status": "cancelled",
                "message": message,
                "spend": run_spend(&journal),
            });
            (RunStatus::Cancelled, terminal)
        }
        Err(error) => {
            let kind = error_kind(&error);
            let message = error.to_string();
            tracing::warn!(%run_id, %error, "run failed");
            sink.push("error", step, json!({"error": kind, "message": message}));
            sink.push("end", step, json!({"status": "error"}));
            let terminal = json!({
                "run_id": run_id,
                "thread_id": snap.wire_thread_id,
                "status": "error",
                "error": kind,
                "message": message,
                "spend": run_spend(&journal),
            });
            (RunStatus::Error, terminal)
        }
    };
    // A run in a chain of agent-started work: its spend joins the chain's
    // — before the assignments plane is woken, so the next round reads it.
    crate::chain_spend::note_run_end(
        deps.server_store.as_ref(),
        snap.payload.metadata.as_ref(),
        &terminal,
    )
    .await;
    if let Some(plane) = &deps.assignments {
        plane.note_run_end(snap.payload.metadata.as_ref(), &run_id, &terminal);
    }
    terminate(&deps, &run_id, status, terminal).await;
}

/// Everything [`forward_events`] needs beyond the frame sink, bundled to
/// keep the task's argument list readable.
struct ForwardDeps {
    checkpointer: Arc<dyn Checkpointer>,
    server_store: Arc<dyn ServerStore>,
    journal: Journal,
    /// Internal (tenant-scoped) thread id, for checkpoint read-backs.
    thread_id: String,
    checkpoint_ids: Arc<StdMutex<Vec<String>>>,
    modes: Vec<String>,
}

/// Map executor events to SSE frames per the design doc's §4 table. Also the
/// Flight Recorder's checkpoint-boundary persistence point: every
/// `CheckpointSaved` event flushes the journal's current snapshot to the
/// server store, so the stored evidence trails the live journal by at most
/// one super-step.
async fn forward_events(mut rx: mpsc::Receiver<GraphEvent>, sink: FrameSink, deps: ForwardDeps) {
    let ForwardDeps {
        checkpointer,
        server_store,
        journal,
        thread_id,
        checkpoint_ids,
        modes,
    } = deps;
    while let Some(event) = rx.recv().await {
        match event {
            GraphEvent::StateUpdate { step, updates } => {
                if modes.iter().any(|m| m == "updates") {
                    sink.push("updates", step, json!({"step": step, "updates": updates}));
                }
            }
            GraphEvent::Token { node, delta } => {
                if modes.iter().any(|m| m == "messages") {
                    let step = sink.current_step();
                    sink.push("messages", step, json!({"node": node, "delta": delta}));
                }
            }
            GraphEvent::CheckpointSaved {
                checkpoint_id,
                step,
            } => {
                lock_recover(&checkpoint_ids).push(checkpoint_id.clone());
                sink.note_checkpoint(&checkpoint_id);
                persist_journal(&server_store, &journal).await;
                if modes.iter().any(|m| m == "values") {
                    match read_back_state(&*checkpointer, &thread_id, &checkpoint_id).await {
                        Ok(Some(values)) => sink.push("values", step, values),
                        Ok(None) => {
                            tracing::debug!(%checkpoint_id, "checkpoint not found for values frame")
                        }
                        Err(error) => {
                            tracing::warn!(%checkpoint_id, %error, "values frame read-back failed")
                        }
                    }
                }
            }
            // Reserved for the future `tasks` / `debug` stream modes.
            GraphEvent::SuperStep { .. }
            | GraphEvent::NodeStart { .. }
            | GraphEvent::NodeEnd { .. } => {}
        }
    }
}

/// Flush the journal's current snapshot to the server store. A persistence
/// failure is logged, not raised: the run's execution must not fail because
/// its evidence could not be written, and the next checkpoint boundary (or
/// the completion write) retries.
async fn persist_journal(server_store: &Arc<dyn ServerStore>, journal: &Journal) {
    if let Err(error) = server_store.put_journal(&journal.snapshot()).await {
        tracing::warn!(run_id = %journal.run_id(), %error, "journal persistence failed");
    }
}

/// `values` frames carry the full state persisted at a super-step boundary,
/// read back from the checkpoint log (design doc §4). A point lookup, not a
/// full `list()` scan — that would be O(history) per super-step, O(n²) per
/// run.
async fn read_back_state(
    checkpointer: &dyn Checkpointer,
    thread_id: &str,
    checkpoint_id: &str,
) -> rusty_agent_runtime::error::Result<Option<Value>> {
    Ok(checkpointer
        .get_by_id(thread_id, checkpoint_id)
        .await?
        .map(|cp| cp.state.to_value()))
}

/// Spawn `execute` for a run, guarding the thread's scheduling slot: if the
/// task panics (executor bug — the poison-prone lock sites recover via
/// [`lock_recover`]), the run is force-finished as `error` so
/// `active_by_thread` releases the slot and queued runs drain instead of
/// wedging behind a ghost.
///
/// The future is boxed behind a trait object to break the
/// `execute → terminate → spawn(execute)` type cycle, which would otherwise
/// make `Send` inference recursive and fail.
fn spawn_execute(deps: RunDeps, run_id: String) {
    let fut: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> = Box::pin({
        let deps = deps.clone();
        let run_id = run_id.clone();
        async move { execute(deps, run_id).await }
    });
    tokio::spawn(async move {
        if AssertUnwindSafe(fut).catch_unwind().await.is_ok() {
            return;
        }
        tracing::error!(%run_id, "run task panicked; force-finishing as error");
        let Some(snap) = deps.manager.snapshot(&run_id).await else {
            return;
        };
        // If the panic happened after `terminate` completed, the slot is
        // already released — finishing again would double-promote the queue.
        if matches!(deps.manager.info(&run_id).await, Some(info) if info.status.is_terminal()) {
            return;
        }
        let step = snap.sink.current_step();
        snap.sink.push(
            "error",
            step,
            json!({"error": "internal_panic", "message": "run task panicked"}),
        );
        snap.sink.push("end", step, json!({"status": "error"}));
        let terminal = json!({
            "run_id": run_id,
            "thread_id": snap.wire_thread_id,
            "status": "error",
            "error": "internal_panic",
            "message": "run task panicked",
        });
        terminate(&deps, &run_id, RunStatus::Error, terminal).await;
    });
}

/// Record the terminal state and spawn the next queued run, if any. While
/// the server drains, the queue does not advance (see
/// [`RunManager::finish`]'s `draining` flag) — and queued runs keep their
/// durable records, so the next process restores them.
async fn terminate(deps: &RunDeps, run_id: &str, status: RunStatus, terminal: Value) {
    let draining = deps.shutdown.is_cancelled();
    if let Some(next) = deps
        .manager
        .finish(run_id, status, terminal, draining)
        .await
    {
        // The promoted run left the queue: its durable record goes with
        // the transition. From here the run's durability is the
        // checkpoint log's job, like any active run's.
        clear_pending_record(&deps.server_store, &next).await;
        spawn_execute(deps.clone(), next);
    }
}

/// Stable error-kind labels for the wire.
fn error_kind(error: &RustyError) -> &'static str {
    match error {
        RustyError::Graph(_) => "graph_error",
        RustyError::Node(_) => "node_error",
        RustyError::Budget(_) => "budget_exhausted",
        RustyError::Interrupt { .. } => "interrupted",
        RustyError::Checkpoint(_) => "checkpoint_error",
        RustyError::Llm(_) => "llm_error",
        // The classified variant is the same wire kind; the class travels
        // in the message, not the label.
        RustyError::LlmFailure { .. } => "llm_error",
        RustyError::Tool(_) => "tool_error",
        RustyError::Transport { .. } => "transport_error",
        RustyError::Serialization(_) => "serialization_error",
        RustyError::InvalidUpdate(_) => "invalid_update",
        RustyError::Replay(_) => "replay_error",
        RustyError::Plugin(_) => "plugin_error",
        RustyError::Doctor(_) => "doctor_error",
        RustyError::Catalog(_) => "catalog_error",
        RustyError::InvariantViolation(_) => "invariant_violation",
        RustyError::FrozenTierViolation { .. } => "frozen_tier_violation",
        RustyError::ChunkAssemblyMismatch { .. } => "chunk_assembly_mismatch",
        // Drain cancellation is control flow and takes its own terminal
        // path in `execute`; this arm exists for exhaustiveness only.
        RustyError::Gap(_) => "gap_error",
        // Drain cancellation is control flow and takes its own terminal
        // path in `execute`; this arm exists for exhaustiveness only.
        RustyError::Cancelled(_) => "cancelled",
    }
}
// --------------------------------------------------------------------- //
// Tests
// --------------------------------------------------------------------- //

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server_store::JsonFileStore;
    use rusty_agent_runtime::journal::{Clock, Journal};
    use rusty_agent_runtime::prelude::{
        Graph, GraphBuilder, JsonFileCheckpointer, NodeContext, NodeOutput, Reducer, StateSpec,
    };
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    /// Unique temp store root, removed on drop.
    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!("rusty-runs-test-{}", uuid::Uuid::new_v4())))
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A gated graph: `wait` parks the run for a minute (the occupying
    /// run in queue tests — long enough that it never finishes inside a
    /// test); without it the run completes immediately.
    fn gated_graph() -> (Graph, StateSpec) {
        let spec = StateSpec::new()
            .channel("wait", Reducer::Overwrite)
            .channel("done", Reducer::Overwrite);
        let mut builder = GraphBuilder::new();
        builder.add_node("work", |ctx: NodeContext| async move {
            if ctx
                .state()
                .get("wait")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
            Ok(NodeOutput::update("done", json!(true)))
        });
        builder.set_entry_point("work");
        (builder.compile().unwrap(), spec)
    }

    /// Run deps over a fresh JSON-file store: the same wiring the router
    /// builds, minus HTTP.
    fn test_deps(root: &Path) -> RunDeps {
        let mut registry = GraphRegistry::new();
        let (graph, spec) = gated_graph();
        registry.register("gated", graph, spec);
        RunDeps {
            users: Arc::new(crate::users::Users::load(root)),
            registry,
            checkpointer: Arc::new(JsonFileCheckpointer::new(root.to_path_buf())),
            manager: RunManager::new(),
            server_store: Arc::new(JsonFileStore::load(root)),
            queue_cap: 8,
            log_capacity: 64,
            shutdown: tokio_util::sync::CancellationToken::new(),
            default_environment_tag: None,
            context_policy: None,
            repair_ledger: Arc::new(rusty_agent_runtime::repair::FileRepairLedger::new(root)),
            verifier: None,
            verifications: Arc::new(crate::verify_outcome::VerificationPlane::new(root)),
            approvals: Arc::new(crate::approvals::ApprovalPlane::new(root)),
            connection_tools: None,
            assignments: None,
            after_verdict: Default::default(),
        }
    }

    fn payload(input: Value) -> RunPayload {
        RunPayload {
            input: Some(input),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn cancel_while_queued_clears_the_durable_record() {
        let tmp = TestDir::new();
        let deps = test_deps(&tmp.0);
        // Occupy the thread so the next submission lands in the FIFO.
        let occupier = schedule(
            &deps,
            "thread-1",
            "thread-1",
            "gated",
            payload(json!({"wait": true})),
            MultitaskStrategy::Enqueue,
        )
        .await
        .unwrap();
        assert_eq!(occupier.status, RunStatus::Running);
        let queued = schedule(
            &deps,
            "thread-1",
            "thread-1",
            "gated",
            payload(json!({})),
            MultitaskStrategy::Enqueue,
        )
        .await
        .unwrap();
        assert_eq!(queued.status, RunStatus::Pending);
        // The enqueue persisted a restore record.
        assert_eq!(
            deps.server_store.list_pending_runs().await.unwrap().len(),
            1
        );

        let outcome = cancel_run(&deps, &queued.run_id).await;
        assert_eq!(outcome, RunCancel::CancelledQueued);
        // The record left the store with the transition...
        assert!(deps
            .server_store
            .list_pending_runs()
            .await
            .unwrap()
            .is_empty());
        // ...so a fresh boot over the same store restores nothing (no
        // zombie run re-appears).
        let reboot = test_deps(&tmp.0);
        restore_pending_runs(reboot.clone()).await;
        assert!(reboot.manager.info(&queued.run_id).await.is_none());
        assert!(!reboot.manager.thread_busy("thread-1").await);
    }

    #[tokio::test]
    async fn restore_dedupes_a_run_that_already_has_a_journal() {
        let tmp = TestDir::new();
        let deps = test_deps(&tmp.0);
        // A leftover record whose run already executed — a crash
        // mid-promotion lost the record delete. The journal (flushed at
        // the first checkpoint boundary and at completion) is the ground
        // truth that the run ran.
        let record = PendingRunRecord {
            run_id: "run-stale".to_string(),
            thread_id: "thread-1".to_string(),
            wire_thread_id: "thread-1".to_string(),
            tenant: "default".to_string(),
            graph: "gated".to_string(),
            payload: RunPayload::default(),
            seq: 1,
            enqueued_at: chrono::Utc::now(),
        };
        deps.server_store.put_pending_run(&record).await.unwrap();
        let journal = Journal::new(
            "run-stale".to_string(),
            "thread-1".to_string(),
            Clock::System,
        );
        deps.server_store
            .put_journal(&journal.snapshot())
            .await
            .unwrap();

        restore_pending_runs(deps.clone()).await;

        // Not double-scheduled, and the zombie record is cleared.
        assert!(deps.manager.info("run-stale").await.is_none());
        assert!(!deps.manager.thread_busy("thread-1").await);
        assert!(deps
            .server_store
            .list_pending_runs()
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn restore_schedules_a_free_threads_run_in_background() {
        let tmp = TestDir::new();
        let deps = test_deps(&tmp.0);
        let record = PendingRunRecord {
            run_id: "run-restored".to_string(),
            thread_id: "thread-1".to_string(),
            wire_thread_id: "thread-1".to_string(),
            tenant: "default".to_string(),
            graph: "gated".to_string(),
            payload: RunPayload::default(),
            seq: 1,
            enqueued_at: chrono::Utc::now(),
        };
        deps.server_store.put_pending_run(&record).await.unwrap();

        restore_pending_runs(deps.clone()).await;

        // The slot was free, so the run started immediately — in
        // background, no SSE client — and ran to completion; its queue
        // record left the store with the promotion.
        let mut status = None;
        for _ in 0..100 {
            if let Some(info) = deps.manager.info("run-restored").await {
                status = Some(info.status);
                if info.status.is_terminal() {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(status, Some(RunStatus::Success));
        assert!(deps
            .server_store
            .list_pending_runs()
            .await
            .unwrap()
            .is_empty());
    }
}

/// The tools a run was told never to call. The model still sees them —
/// so the journal shows whether it tried — and every attempt is refused
/// with the reason and recorded as a denial.
#[derive(Debug)]
pub struct ForbiddenTools {
    names: Vec<String>,
}

impl ForbiddenTools {
    pub fn new(names: Vec<String>) -> Self {
        Self { names }
    }
}

impl rusty_agent_runtime::tool::ToolGuard for ForbiddenTools {
    fn name(&self) -> &str {
        "forbidden_tools"
    }

    fn check(
        &self,
        call: &rusty_agent_runtime::tool::GuardedCall<'_>,
    ) -> Option<rusty_agent_runtime::tool::GuardDenial> {
        self.names.iter().any(|n| n == call.tool).then(|| {
            rusty_agent_runtime::tool::GuardDenial::new(
                "forbidden_tools",
                format!("`{}` must not be called in this work — a constraint set when it was delegated, held by the platform", call.tool),
            )
        })
    }
}

#[cfg(test)]
mod forbidden_tools_tests {
    use super::ForbiddenTools;
    use rusty_agent_runtime::record::Effect;
    use rusty_agent_runtime::tool::{GuardedCall, ToolGuard};

    #[test]
    fn a_forbidden_tool_is_refused_by_name_and_nothing_else_is() {
        let guard = ForbiddenTools::new(vec!["servicenow.create-record".to_owned()]);
        let args = serde_json::json!({"table": "problem"});
        let write = GuardedCall {
            tool: "servicenow.create-record",
            arguments: &args,
            effect: Effect::Idempotent,
            scope: "t",
        };
        let read = GuardedCall {
            tool: "servicenow.list-records",
            arguments: &args,
            effect: Effect::ReadOnly,
            scope: "t",
        };
        let denial = guard.check(&write).expect("refused");
        assert_eq!(denial.guard, "forbidden_tools");
        assert!(
            denial.reason.contains("must not be called"),
            "{}",
            denial.reason
        );
        assert!(
            guard.check(&read).is_none(),
            "a read the constraint never named goes through"
        );
    }
}
