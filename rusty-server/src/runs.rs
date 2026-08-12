//! Run scheduling, execution, and bookkeeping.
//!
//! A run goes: *schedule* (strategy check + handle insert) → *execute*
//! (drive [`Executor`] in a spawned task, forwarding [`GraphEvent`]s to a
//! per-run SSE frame log + broadcast channel) → *terminate* (terminal status
//! + JSON recorded, waiters woken, next queued run for the thread spawned).
//!
//! Multitask: there is always at most one **active** run per thread. The
//! `reject` strategy returns 409 when the thread is busy; `enqueue` appends
//! to an in-memory per-thread FIFO queue (depth-capped by
//! `ServerConfig::max_concurrent_runs_per_thread`) that drains automatically
//! as runs finish.
//!
//! Retention: terminal runs are kept for `GET /runs/{id}` polling up to
//! [`MAX_RETAINED_RUNS`] per process; the oldest terminal runs are evicted
//! beyond that (active and queued runs are never evicted). Run history is
//! in-memory by design — durability lives in the checkpoint log.
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

use std::collections::{HashMap, VecDeque};
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
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, watch, Mutex};

use crate::error::ApiError;
use crate::server_store::ServerStore;
use crate::GraphRegistry;

// --------------------------------------------------------------------- //
// Run payload (accepted by all three run endpoints)
// --------------------------------------------------------------------- //

/// The `command` field of a run payload: `{ "resume": <value> }` continues
/// an interrupted thread via [`RunConfig::with_resume`].
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CommandPayload {
    /// Resume value delivered to the interrupted node.
    #[serde(default)]
    pub resume: Option<Value>,
}

/// The `config` field of a run payload.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RunConfigPayload {
    /// Maps to [`RunConfig::with_max_steps`] (LangGraph `recursion_limit`).
    #[serde(default)]
    pub recursion_limit: Option<usize>,
}

/// The `checkpoint` field of a run payload: `{ "checkpoint_id": "…" }`
/// replays the thread from that checkpoint (time travel) instead of the
/// latest, via [`RunConfig::with_checkpoint_id`].
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CheckpointPayload {
    /// Id of a checkpoint of this thread (see `POST /threads/{id}/history`).
    pub checkpoint_id: String,
}

/// The payload accepted by `POST /threads/{id}/runs{,/wait,/stream}`.
#[derive(Debug, Clone, Default, Deserialize)]
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
    /// Stable server acceptance time used to bind downstream evidence.
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub attempt: usize,
    pub status: RunStatus,
    /// The terminal JSON once the run has finished (`None` while active).
    pub terminal: Option<Value>,
    pub checkpoint_ids: Arc<StdMutex<Vec<String>>>,
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
    /// The run was queued behind the active run.
    Queued,
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
                Ok(ScheduleDecision::Queued)
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
        inner.runs.get(run_id).map(|h| RunInfo {
            thread_id: h.thread_id.clone(),
            wire_thread_id: h.wire_thread_id.clone(),
            graph: h.graph.clone(),
            assistant_id: h.payload.assistant_id.clone(),
            metadata: h.payload.metadata.clone(),
            input: h.payload.input.clone(),
            created_at: h.created_at,
            attempt: h.attempt,
            status: h.status,
            terminal: h.terminal.borrow().clone(),
            checkpoint_ids: Arc::clone(&h.checkpoint_ids),
        })
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

/// Everything the run machinery needs from the application: registry,
/// checkpointer, manager, and caps. Cheap to clone.
#[derive(Clone)]
pub(crate) struct RunDeps {
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
}

/// The result of successfully scheduling a run: everything an endpoint
/// needs to answer (background ack, wait, or stream).
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
    // Registry admission (R0.11 wave 2): the binding resolves now, at
    // admission — a promotion landing afterwards never reaches this run
    // (the conservatism checkpoint pinning has kept since R0.7), and a
    // queued run binds at admission, not at dequeue. The tenant comes
    // from the internal thread id, so every entry point (HTTP, cron,
    // trigger, bridge) resolves in the submitter's namespace without
    // threading the request context through. A resolution failure is an
    // admission failure: the run never enters the manager.
    let admission = match &payload.registry {
        Some(binding) => Some(
            crate::registry::resolve_admission(
                &deps.server_store,
                crate::auth::tenant_of_internal(thread_id),
                deps.default_environment_tag.as_ref(),
                &run_id,
                binding,
            )
            .await?,
        ),
        None => None,
    };
    // Deployment admission (R0.12 wave 3): the environment's pointer
    // binds a revision now, at admission — a promotion landing afterwards
    // never reaches this run (the registry admission's conservatism,
    // lifted to deployments). The revision's identity checks against the
    // registered graph (name and current topology hash), so a build the
    // revision no longer describes is refused, never run. A resolution
    // failure is an admission failure: the run never enters the manager.
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
                    &run_id,
                    binding,
                    graph,
                    &graph_obj.topology_hash(),
                )
                .await?,
            )
        }
        None => None,
    };
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
    // Subscribe/snapshot before any execution can emit frames.
    let replay = handle.log_snapshot();
    let broadcast = handle.subscribe();

    let decision = deps
        .manager
        .insert(handle, strategy, deps.queue_cap)
        .await?;
    let status = match decision {
        ScheduleDecision::Started => {
            spawn_execute(deps.clone(), run_id.clone());
            RunStatus::Running
        }
        ScheduleDecision::Queued => RunStatus::Pending,
    };

    Ok(Scheduled {
        run_id,
        status,
        terminal: terminal_rx,
        broadcast,
        replay,
    })
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
    }
    let initial = snap
        .payload
        .input
        .clone()
        .and_then(|v| State::from_value(v).ok())
        .unwrap_or_default();

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
    let result = executor.run(&graph, &spec, initial, config).await;
    // `config` (holding the only sender) is dropped with the run; the
    // forwarder drains what remains and exits.
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
            sink.push("end", step, json!({"status": "success"}));
            let terminal = json!({
                "run_id": run_id,
                "thread_id": snap.wire_thread_id,
                "status": "success",
                "output": state.to_value(),
            });
            (RunStatus::Success, terminal)
        }
        Ok(ExecutionOutcome::Interrupted {
            value,
            state,
            checkpoint_id,
        }) => {
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
            });
            (RunStatus::Error, terminal)
        }
    };
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
/// [`RunManager::finish`]'s `draining` flag).
async fn terminate(deps: &RunDeps, run_id: &str, status: RunStatus, terminal: Value) {
    let draining = deps.shutdown.is_cancelled();
    if let Some(next) = deps
        .manager
        .finish(run_id, status, terminal, draining)
        .await
    {
        spawn_execute(deps.clone(), next);
    }
}

/// Stable error-kind labels for the wire.
fn error_kind(error: &RustyError) -> &'static str {
    match error {
        RustyError::Graph(_) => "graph_error",
        RustyError::Node(_) => "node_error",
        RustyError::Interrupt { .. } => "interrupted",
        RustyError::Checkpoint(_) => "checkpoint_error",
        RustyError::Llm(_) => "llm_error",
        // The classified variant is the same wire kind; the class travels
        // in the message, not the label.
        RustyError::LlmFailure { .. } => "llm_error",
        RustyError::Tool(_) => "tool_error",
        RustyError::Serialization(_) => "serialization_error",
        RustyError::InvalidUpdate(_) => "invalid_update",
        RustyError::Replay(_) => "replay_error",
        // Drain cancellation is control flow and takes its own terminal
        // path in `execute`; this arm exists for exhaustiveness only.
        RustyError::Cancelled(_) => "cancelled",
    }
}
