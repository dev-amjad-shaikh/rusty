//! Durable activities (R0.6): [`ActivityWorker`], the leased-task execution
//! loop against the rusty-agent-server task queue.
//!
//! Where [`crate::serve`] answers one-shot `/execute` calls pushed by a
//! [`RemoteNode`](rusty_agent_runtime::remote::RemoteNode), an
//! `ActivityWorker` *pulls* work: it claims tasks from the server under a
//! lease, renews that lease with heartbeats while the handler runs, and
//! settles the task with a complete or fail call. The server re-queues tasks
//! whose lease expires, so a crashed worker never strands work. The promise
//! is *effectively-once* execution when activity side effects are idempotent
//! — not universal exactly-once.
//!
//! ## The task model
//!
//! A task is a generic unit of durable work: a `kind` label plus an
//! arbitrary JSON `payload`, enqueued via `POST /tasks` and claimed by pool.
//! The worker registers one [`Activity`] handler per `kind`; the handler
//! receives an [`ActivityContext`] (task id, attempt, idempotency key,
//! payload) and returns the JSON result stored on the task record.
//!
//! ## The lease protocol (client side)
//!
//! - `POST {base}/tasks/claim` — body `{worker_id, pools?, worker_version?,
//!   lease_ms}`; a `200` carries `{"task": {…}}` (a [`ClaimedTask`]), a `204`
//!   means no work is available in the claimed pools. `worker_version` (R0.6
//!   wave 3) is the worker's advertised version: the server matches it
//!   exactly against a task's version pin, so a run pinned to a version
//!   keeps dispatching to workers running it until the run finishes.
//! - `POST {base}/tasks/{task_id}/heartbeat` — body `{worker_id, lease_ms}`,
//!   sent from a background task every `lease / 3`; a `200` renews the lease
//!   (`{lease_expires_at, cancel_requested}`), a `409` means the lease is
//!   lost.
//! - `POST {base}/tasks/{task_id}/complete` — body `{worker_id, result}`,
//!   where `result` is the handler's JSON return value; an activity that
//!   performed an idempotent effect additionally carries its `receipt`
//!   (the provider's confirmation — see [`Activity::run_with_receipt`]),
//!   which the server journals into the task's run as an `effect_receipt`
//!   event.
//! - `POST {base}/tasks/{task_id}/fail` — body
//!   `{worker_id, error_class, message, retryable}`; the reply
//!   `{requeued, next_attempt_at, dead}` is logged for operators.
//!
//! A `409` from any of these calls means the server considers the task lost
//! or already settled; the worker then abandons the activity without further
//! calls, so the server's reassignment stays the single source of truth.
//!
//! ## Semantics
//!
//! - **One activity in flight.** The loop claims, executes, and settles one
//!   task at a time. Run several `ActivityWorker` instances for parallelism;
//!   pool routing ([`ActivityWorker::with_pools`]) keeps them on disjoint
//!   queues.
//! - **Lease loss aborts execution.** When a heartbeat answers `409`, the
//!   handler future is aborted (dropped at its next yield point) and the
//!   worker makes no settle call — the server will reassign the task.
//!   Handler code must tolerate being abandoned mid-effect; that is the
//!   effectively-once contract, and it is why activity side effects should
//!   be keyed by [`ActivityContext::task_id`] /
//!   [`ActivityContext::idempotency_key`].
//! - **Cancellation aborts promptly, and reports.** When a heartbeat
//!   carries `cancel_requested: true` — or the task's whole-task
//!   `deadline` passes mid-attempt — the handler is aborted the same way,
//!   but the worker *does* settle: it reports the attempt as
//!   [`ErrorClass::Cancelled`] through `/fail`, which the server's shared
//!   retry policy fails outright, so the record ends terminal-cancelled —
//!   never retried, never dead-lettered. A task claimed with the deadline
//!   already passed (or the cancel flag already set) is reported cancelled
//!   without running the handler at all.
//! - **Graceful drain (R0.6 wave 2c).** Cancelling the shutdown
//!   [`CancellationToken`] *is* the drain request — idempotent, and
//!   race-safe against an in-flight claim poll (the poll itself is
//!   `select!`ed against the token). Draining stops claiming immediately;
//!   an in-flight activity keeps heartbeating and settles normally,
//!   bounded by [`ActivityWorker::with_drain_grace`] (default
//!   [`DEFAULT_DRAIN_GRACE`], 25 s — under the 30 s default lease and
//!   Kubernetes' 30 s pod-termination grace). An attempt that outlives
//!   the grace is aborted and deliberately **not** settled: reporting it
//!   [`ErrorClass::Cancelled`] would kill the task (cancelled is
//!   terminal), while leaving it unsettled releases it back to visibility
//!   at lease expiry for another worker — the design's "finishes or
//!   fast-fails in-flight attempts within a grace period, and releases
//!   the rest".
//! - **Failure classification.** Handler errors reach `/fail` under the
//!   frozen [`ErrorClass`] taxonomy (shared with the server scheduler via
//!   `rusty_agent_runtime::durable`), with the `retryable` flag mirroring
//!   the executor's judgment: `Llm` and `Tool` failures are the transient
//!   classes; everything else is reported non-retryable.
//!
//! ```no_run
//! use std::time::Duration;
//! use rusty_worker::{ActivityContext, ActivityWorker};
//! use tokio_util::sync::CancellationToken;
//!
//! # async fn demo() {
//! let worker = ActivityWorker::new("http://127.0.0.1:8080")
//!     .register("send_receipt", |ctx: ActivityContext| async move {
//!         // `task_id` / `idempotency_key` are the side-effect correlation
//!         // handles that make redelivery effectively-once.
//!         let _dedup_key = ctx.idempotency_key().unwrap_or_else(|| ctx.task_id());
//!         let to = ctx.payload()["to"].as_str().unwrap_or("unknown");
//!         // … perform the effect …
//!         Ok(serde_json::json!({"sent": true, "to": to}))
//!     })
//!     .with_worker_id("email-worker-1")
//!     .with_lease(Duration::from_secs(30))
//!     .with_pools(["email"]);
//!
//! // Cancel the token (e.g. from a SIGTERM handler) to drain and exit.
//! worker.run(CancellationToken::new()).await;
//! # }
//! ```

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::StatusCode;
use rusty_agent_runtime::durable::ErrorClass;
use rusty_agent_runtime::error::{Result, RustyError};
use rusty_agent_runtime::record::EffectReceipt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use uuid::Uuid;

/// Default lease requested on claim and renewed by heartbeats (30 s).
pub const DEFAULT_LEASE: Duration = Duration::from_secs(30);

/// Default per-request HTTP timeout for claim/heartbeat/settle calls (10 s).
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Default base delay between claim polls when no work is available (or the
/// server is unreachable); consecutive empty polls back off exponentially up
/// to [`DEFAULT_CLAIM_BACKOFF_MAX`].
pub const DEFAULT_CLAIM_BACKOFF_BASE: Duration = Duration::from_millis(100);

/// Default cap for the claim-poll backoff.
pub const DEFAULT_CLAIM_BACKOFF_MAX: Duration = Duration::from_secs(5);

/// Default drain grace (25 s): once draining starts, an in-flight activity
/// gets this long to finish and settle before the worker aborts it and
/// exits, leaving the attempt for the server to reassign at lease expiry.
///
/// Chosen to sit under the [`DEFAULT_LEASE`] of 30 s — the drain never
/// outlives the visibility window the lease already provides — and under
/// Kubernetes' default 30 s pod-termination grace, so a drained worker
/// exits before SIGKILL would arrive. Override with
/// [`ActivityWorker::with_drain_grace`]; when you also change the lease,
/// keep the grace below it.
pub const DEFAULT_DRAIN_GRACE: Duration = Duration::from_secs(25);

// The server's accepted `lease_ms` range (`tasks::MIN_LEASE_MS..=MAX_LEASE_MS`
// in rusty-agent-server): below 100 ms a lease is an instant expiry, above one
// hour a lost worker strands its task for too long. `with_lease` clamps to
// this range so a misconfigured worker gets a sane lease instead of a 400
// on every claim.
const MIN_LEASE: Duration = Duration::from_millis(100);
const MAX_LEASE: Duration = Duration::from_secs(3_600);

/// The input to every activity invocation: the claimed task's identity,
/// attempt ordinal, dedup key, and payload.
///
/// `task_id` (and, when the enqueue side set one, `idempotency_key`) are
/// the correlation handles for external side effects — a redelivered task
/// (lease expiry, crash) re-runs the handler with the same ids, which is
/// what makes the effect effectively-once.
#[derive(Debug, Clone)]
pub struct ActivityContext {
    task_id: String,
    kind: String,
    attempt: u32,
    idempotency_key: Option<String>,
    payload: Value,
}

impl ActivityContext {
    /// The server-minted task id.
    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    /// The task kind this invocation was dispatched under.
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// The 1-based attempt ordinal (`1` on first delivery, higher after a
    /// requeue).
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// The dedup key supplied at enqueue time, if any.
    pub fn idempotency_key(&self) -> Option<&str> {
        self.idempotency_key.as_deref()
    }

    /// The task payload, exactly as enqueued.
    pub fn payload(&self) -> &Value {
        &self.payload
    }
}

/// An activity: one durable unit of work, dispatched by task `kind`.
///
/// Implement this directly for stateful activities, or just pass an async
/// closure `Fn(ActivityContext) -> impl Future<Output = Result<Value>>` to
/// [`ActivityWorker::register`] — a blanket impl covers it, mirroring the
/// [`Node`](rusty_agent_runtime::node::Node) ergonomics.
///
/// The returned `Value` is stored verbatim as the task's `result`. An
/// `Err` is classified into the shared [`ErrorClass`] taxonomy and reported
/// to `POST /tasks/{id}/fail`.
///
/// Activities performing an `Idempotent` effect should override
/// [`Activity::run_with_receipt`] instead: the receipt it returns — the
/// provider's own confirmation, under the task's idempotency key — travels
/// with the completion and is journaled into the task's run as evidence
/// exact replay can serve instead of re-sending the effect.
#[async_trait]
pub trait Activity: Send + Sync {
    /// Execute the activity against a claimed task.
    async fn run(&self, ctx: ActivityContext) -> Result<Value>;

    /// Execute the activity, additionally reporting an effect receipt with
    /// the result. The default wraps [`Activity::run`] with no receipt, so
    /// existing activities (and the closure blanket impl) are unaffected —
    /// a receipt is something an activity opts into reporting, never
    /// something the worker invents on its behalf: only the code that
    /// performed the effect knows the provider's confirmation id.
    async fn run_with_receipt(&self, ctx: ActivityContext) -> Result<ActivityCompletion> {
        Ok(ActivityCompletion {
            result: self.run(ctx).await?,
            receipt: None,
        })
    }
}

/// What an activity reports on success: the stored `result` plus, when the
/// activity performed an `Idempotent` effect, its [`EffectReceipt`] — the
/// effect's own confirmation (provider id, idempotency key), which the
/// server journals into the task's run as a `effect_receipt` event.
#[derive(Debug, Clone)]
pub struct ActivityCompletion {
    /// The value stored as the task's `result` (any JSON).
    pub result: Value,

    /// The effect's confirmation, when the activity performed an idempotent
    /// effect and the provider confirmed it. `None` for pure/read-only work
    /// and for effects that carry no confirmation handle.
    pub receipt: Option<EffectReceipt>,
}

/// Blanket implementation for async closures/functions:
/// `Fn(ActivityContext) -> impl Future<Output = Result<Value>>`.
#[async_trait]
impl<F, Fut> Activity for F
where
    F: Fn(ActivityContext) -> Fut + Send + Sync,
    Fut: Future<Output = Result<Value>> + Send,
{
    async fn run(&self, ctx: ActivityContext) -> Result<Value> {
        (self)(ctx).await
    }
}

/// Allow `Arc<dyn Activity>` itself to be registered (useful for sharing one
/// activity implementation across workers).
///
/// Both methods delegate: overriding only `run` would make the default
/// `run_with_receipt` answer for the `Arc` — wrapping the inner `run` and
/// silently dropping the receipt a receipt-carrying activity reported. The
/// worker stores every registered handler as `Arc<dyn Activity>` and drives
/// it through `run_with_receipt`, so this delegation is what carries the
/// effect receipt from the handler to the completion call.
#[async_trait]
impl Activity for Arc<dyn Activity> {
    async fn run(&self, ctx: ActivityContext) -> Result<Value> {
        self.as_ref().run(ctx).await
    }

    async fn run_with_receipt(&self, ctx: ActivityContext) -> Result<ActivityCompletion> {
        self.as_ref().run_with_receipt(ctx).await
    }
}

/// The task a worker receives on a successful claim: the fields of the
/// server's task record a worker needs (`POST /tasks/claim` →
/// `{"task": {…}}`). Fields the worker does not consume are ignored on
/// deserialization, so additive server-side evolution keeps working.
#[derive(Debug, Clone, Deserialize)]
pub struct ClaimedTask {
    /// The server-minted task id, used to address heartbeat/complete/fail.
    pub task_id: String,

    /// The dispatch label the worker's handler registry is keyed by.
    pub kind: String,

    /// The work payload, exactly as enqueued.
    #[serde(default)]
    pub payload: Value,

    /// The 1-based attempt ordinal (higher after a requeue).
    #[serde(default)]
    pub attempt: u32,

    /// The attempt ceiling configured at enqueue time.
    #[serde(default)]
    pub max_attempts: u32,

    /// The dedup key supplied at enqueue time, if any.
    #[serde(default)]
    pub idempotency_key: Option<String>,

    /// Run linkage (reserved for the run-side outbox wave); carried into the
    /// activity's tracing span when present.
    #[serde(default)]
    pub run_id: Option<String>,

    /// See [`ClaimedTask::run_id`].
    #[serde(default)]
    pub thread_id: Option<String>,

    /// Cancellation signalled before the claim was handed out. A
    /// well-behaved server never leases such a task (its claim path
    /// finalizes them as cancelled), but the worker defends anyway: a
    /// claimed task with this flag is reported cancelled without running
    /// the handler.
    #[serde(default)]
    pub cancel_requested: bool,

    /// Whole-task deadline, across attempts. The worker refuses to start
    /// an attempt past it and aborts a running attempt at it, reporting
    /// [`ErrorClass::Cancelled`] — deadline expiry is cancellation by
    /// clock, per the Durable Work design.
    #[serde(default)]
    pub deadline: Option<DateTime<Utc>>,
}

/// `POST /tasks/claim` request body.
#[derive(Debug, Serialize)]
struct ClaimRequest<'a> {
    worker_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pools: Option<&'a [String]>,
    /// The worker's advertised version (R0.6 wave 3), matched exactly
    /// against a task's `worker_version` pin. Omitted (not null) when
    /// unconfigured — an unversioned worker takes unpinned work only, and
    /// pre-wave-3 servers never see the field.
    #[serde(skip_serializing_if = "Option::is_none")]
    worker_version: Option<&'a str>,
    lease_ms: u64,
}

/// `POST /tasks/claim` success body: the task, wrapped.
#[derive(Debug, Deserialize)]
struct ClaimResponse {
    task: ClaimedTask,
}

/// `POST /tasks/{id}/heartbeat` request body.
#[derive(Debug, Serialize)]
struct HeartbeatRequest<'a> {
    worker_id: &'a str,
    lease_ms: u64,
}

/// `POST /tasks/{id}/heartbeat` success body. `lease_expires_at` is an
/// RFC 3339 timestamp; the worker treats it as informational (the server's
/// `409` is the authoritative lease-loss signal) and logs it.
/// `cancel_requested` is the cancellation hint: on `true` the worker
/// aborts the in-flight attempt and reports it [`ErrorClass::Cancelled`].
/// Absent on pre-wave-2 servers, hence the default.
#[derive(Debug, Deserialize)]
struct HeartbeatResponse {
    lease_expires_at: Option<String>,
    #[serde(default)]
    cancel_requested: bool,
}

/// `POST /tasks/{id}/complete` request body. `receipt` is omitted (not
/// null) when the activity reports none, keeping the body identical for
/// pre-receipt activities.
#[derive(Debug, Serialize)]
struct CompleteRequest<'a> {
    worker_id: &'a str,
    result: &'a Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    receipt: Option<&'a EffectReceipt>,
}

/// `POST /tasks/{id}/fail` request body. `error_class` serializes to the
/// frozen snake_case wire spelling of [`ErrorClass`].
#[derive(Debug, Serialize)]
struct FailRequest<'a> {
    worker_id: &'a str,
    error_class: ErrorClass,
    message: &'a str,
    retryable: bool,
}

/// `POST /tasks/{id}/fail` success body: the server's retry decision.
#[derive(Debug, Deserialize)]
struct FailResponse {
    requeued: bool,
    next_attempt_at: Option<String>,
    dead: bool,
}

/// Classify a handler error for the `/fail` call.
///
/// Two independent judgments, both owned by the executor of the work:
///
/// - `error_class` — the frozen [`ErrorClass`] taxonomy shared with the
///   server scheduler. `Llm` and `Tool` failures are upstream
///   ([`ErrorClass::DependencyFailure`]); a classified
///   [`RustyError::LlmFailure`] keeps its finer class through the
///   provider-layer mapping (`From<LlmErrorClass>`), so a rate limit
///   retries as a rate limit and a rejected credential fails instead of
///   burning the attempt budget. `Graph` and `InvalidUpdate` are
///   contract violations ([`ErrorClass::InvalidInput`]); everything
///   unclassifiable is [`ErrorClass::Unknown`]. An `Interrupt` error is
///   [`ErrorClass::Cancelled`]: the task-queue protocol has no suspend
///   semantics (HITL wiring is the run-outbox wave's concern), so a handler
///   interrupt settles as a non-retryable cancellation.
/// - `retryable` — the executor's own taxonomy: `Llm` and `Tool` are the
///   transient classes; a classified LLM failure retries iff its mapped
///   class does; everything else is a hard failure.
fn classify_error(error: &RustyError) -> (ErrorClass, bool) {
    match error {
        RustyError::Llm(_) => (ErrorClass::DependencyFailure, true),
        RustyError::LlmFailure { class, .. } => {
            let class = ErrorClass::from(*class);
            (class, class.is_retryable())
        }
        RustyError::Tool(_) => (ErrorClass::DependencyFailure, true),
        RustyError::Node(_) => (ErrorClass::Unknown, false),
        RustyError::Graph(_) => (ErrorClass::InvalidInput, false),
        RustyError::InvalidUpdate(_) => (ErrorClass::InvalidInput, false),
        // Catalog-surface errors (package, doctor, registry) are contract
        // violations, like `Graph`: retrying the claim changes nothing.
        RustyError::Plugin(_) | RustyError::Doctor(_) | RustyError::Catalog(_) => {
            (ErrorClass::InvalidInput, false)
        }
        RustyError::Checkpoint(_) => (ErrorClass::Unknown, false),
        RustyError::Replay(_) => (ErrorClass::Unknown, false),
        RustyError::Serialization(_) => (ErrorClass::Unknown, false),
        RustyError::Interrupt { .. } => (ErrorClass::Cancelled, false),
        // A handler whose own inner run was drained propagates the
        // cancellation class: control flow, never retried.
        RustyError::Cancelled(_) => (ErrorClass::Cancelled, false),
        RustyError::Plugin(_) => (ErrorClass::Unknown, false),
    }
}

/// The outcome of one claim poll.
enum ClaimOutcome {
    /// A task was claimed and must be executed and settled. Boxed: the
    /// claimed-task payload dwarfs the other variants and the wasm-enabled
    /// feature combination pushes it past clippy's size threshold.
    Task(Box<ClaimedTask>),
    /// The server answered `204`: no work available.
    Empty,
    /// The poll failed (transport error, unexpected status, undecodable
    /// body); the caller backs off and polls again.
    Unavailable,
}

/// Why the attempt runner stopped waiting on the handler future: finished
/// normally, the lease is gone (abandon — the server reassigns), or the
/// task was cancelled (abort, then report [`ErrorClass::Cancelled`]
/// through the fail path while we still hold the lease), or the drain
/// grace expired (abort, then leave the attempt unsettled — see
/// [`ActivityWorker::run`]'s drain semantics).
enum AttemptOutcome {
    /// The handler future completed (possibly with an error or panic).
    Finished(std::result::Result<Result<ActivityCompletion>, tokio::task::JoinError>),
    /// A heartbeat answered `409`: the server considers the task lost or
    /// settled. The worker makes no further calls for it.
    LeaseLost,
    /// Cancellation reached the worker — a heartbeat carried
    /// `cancel_requested: true`, or the whole-task deadline expired. The
    /// payload is the reason recorded on the fail report.
    Cancelled(&'static str),
    /// The worker is draining and the in-flight attempt outlived the
    /// configured grace. Like [`AttemptOutcome::LeaseLost`] the attempt is
    /// abandoned without a settle call — but deliberately, not because the
    /// lease is gone: settling `cancelled` would kill the task outright,
    /// while leaving it unsettled lets lease expiry hand it to a worker
    /// that is not being deployed away.
    DrainGraceExpired,
}

/// The deadline arm of the attempt runner's `select!`: fires when the
/// whole-task deadline passes, or pends forever when the task has none —
/// one arm shape either way.
async fn sleep_until_deadline(until: Option<Duration>) {
    match until {
        Some(delay) => tokio::time::sleep(delay).await,
        None => std::future::pending().await,
    }
}

/// A durable worker that claims leased tasks from the rusty-agent-server task
/// queue and executes them behind heartbeats.
///
/// This is the pull-based counterpart to [`crate::serve`]: instead of
/// answering one-shot `/execute` calls, it polls `POST /tasks/claim`, runs
/// the [`Activity`] registered for the claimed task's `kind`, renews the
/// lease via a background heartbeat every `lease / 3`, and settles with
/// `complete` or `fail` (classified error). See the
/// [module documentation](self) for the full protocol and semantics.
///
/// Run several instances for parallelism; pool routing keeps them on
/// disjoint queues.
pub struct ActivityWorker {
    handlers: HashMap<String, Arc<dyn Activity>>,
    /// Server base URL, trailing slashes trimmed.
    base_url: String,
    /// The identity sent on every claim/heartbeat/settle call.
    worker_id: String,
    client: reqwest::Client,
    /// Requested lease duration; heartbeats fire at `lease / 3`.
    lease: Duration,
    /// Pool names to claim from; empty means the server's default pool.
    pools: Vec<String>,
    /// The version advertised on every claim (R0.6 wave 3), matched
    /// exactly against tasks' `worker_version` pins; `None` claims
    /// unpinned work only.
    worker_version: Option<String>,
    claim_backoff_base: Duration,
    claim_backoff_max: Duration,
    /// How long an in-flight activity may run after draining starts before
    /// the worker aborts it and exits (see [`DEFAULT_DRAIN_GRACE`]).
    drain_grace: Duration,
}

impl ActivityWorker {
    /// A worker claiming tasks from the rusty-agent-server at `base_url` (e.g.
    /// `"http://127.0.0.1:8080"`).
    ///
    /// Register at least one [`Activity`] with [`ActivityWorker::register`]
    /// before running. The worker id defaults to `worker-{uuid}`; set a
    /// stable one with [`ActivityWorker::with_worker_id`] if operators
    /// should recognize the worker across restarts.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            handlers: HashMap::new(),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            worker_id: format!("worker-{}", Uuid::new_v4()),
            // Same construction invariant as `RemoteNode`: the builder only
            // sets a timeout and the rustls backend needs no platform
            // initialization, so this cannot realistically fail.
            client: reqwest::Client::builder()
                .timeout(DEFAULT_REQUEST_TIMEOUT)
                .build()
                .expect("reqwest client builder with rustls must succeed"),
            lease: DEFAULT_LEASE,
            pools: Vec::new(),
            worker_version: None,
            claim_backoff_base: DEFAULT_CLAIM_BACKOFF_BASE,
            claim_backoff_max: DEFAULT_CLAIM_BACKOFF_MAX,
            drain_grace: DEFAULT_DRAIN_GRACE,
        }
    }

    /// Register the activity executed for tasks of `kind`.
    ///
    /// Accepts any [`Activity`] implementation — including plain async
    /// closures via the blanket impl. Registering the same kind twice
    /// replaces the previous handler.
    pub fn register<A>(mut self, kind: impl Into<String>, activity: A) -> Self
    where
        A: Activity + 'static,
    {
        self.handlers.insert(kind.into(), Arc::new(activity));
        self
    }

    /// Override the worker identity sent on every call to the server.
    pub fn with_worker_id(mut self, worker_id: impl Into<String>) -> Self {
        self.worker_id = worker_id.into();
        self
    }

    /// Override the requested lease duration. Heartbeats renew the lease
    /// from a background task every `lease / 3` (clamped to ≥ 1 ms), so a
    /// lease must comfortably outlive three heartbeat intervals of clock
    /// drift and scheduling jitter — the [`DEFAULT_LEASE`] of 30 s is a good
    /// floor for real deployments. The value is clamped to the server's
    /// accepted range (100 ms – 1 h).
    pub fn with_lease(mut self, lease: Duration) -> Self {
        self.lease = lease.clamp(MIN_LEASE, MAX_LEASE);
        self
    }

    /// Restrict claiming to the named pools (`"pools": [...]` on the claim
    /// call). With no pools configured the claim omits the field and the
    /// server's default pool applies.
    pub fn with_pools<I, S>(mut self, pools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.pools = pools.into_iter().map(Into::into).collect();
        self
    }

    /// Advertise this worker's version on every claim (R0.6 wave 3). The
    /// server matches it **exactly** against a task's `worker_version`
    /// pin: tasks a run pinned to this version are leased to this worker
    /// (and its version-mates) until the run finishes, so a mid-run deploy
    /// never changes semantics under an in-flight execution; unpinned
    /// tasks remain claimable regardless. With no version configured the
    /// claim omits the field and the worker takes unpinned work only —
    /// a worker that should serve pinned runs must advertise the version
    /// it actually runs. Free-form string, but make it the real build
    /// identity (crate version, image tag): a stale advertisement defeats
    /// the pin's purpose.
    pub fn with_worker_version(mut self, version: impl Into<String>) -> Self {
        self.worker_version = Some(version.into());
        self
    }

    /// Override the claim-poll backoff: empty or failed polls wait roughly
    /// `base * 2^n` (capped at `max`) until a task is claimed.
    pub fn with_claim_backoff(mut self, base: Duration, max: Duration) -> Self {
        self.claim_backoff_base = base;
        self.claim_backoff_max = max;
        self
    }

    /// Override the drain grace (default [`DEFAULT_DRAIN_GRACE`]): how long
    /// an in-flight activity may keep running after the shutdown token
    /// fires before the worker aborts it and exits. An aborted attempt is
    /// deliberately **not** settled — it returns to visibility at lease
    /// expiry for another worker — so keep the grace under the lease:
    /// beyond the lease the abort buys nothing (the server would reassign
    /// at the same moment either way).
    pub fn with_drain_grace(mut self, grace: Duration) -> Self {
        self.drain_grace = grace;
        self
    }

    /// The identity this worker presents to the server.
    pub fn worker_id(&self) -> &str {
        &self.worker_id
    }

    /// Run the claim → execute → settle loop until `shutdown` is cancelled,
    /// then drain: stop claiming, settle any in-flight activity, and return.
    ///
    /// Cancelling `shutdown` **is** the drain request — there is no separate
    /// `drain()` because the token already is the control: idempotent
    /// (cancelling twice changes nothing), race-safe (a cancel racing an
    /// in-flight claim poll still wins, because the claim itself is
    /// `select!`ed against the token), and shareable (every clone of the
    /// token observes the same signal). Wire it to SIGTERM/SIGINT in the
    /// embedding binary; see the crate's `activity_worker_demo` example.
    ///
    /// Drain semantics: claiming stops immediately; an in-flight activity
    /// keeps heartbeating and settles normally, bounded by
    /// [`ActivityWorker::with_drain_grace`]. An attempt still running when
    /// the grace expires is aborted and deliberately **not** settled —
    /// reporting it [`ErrorClass::Cancelled`] would kill the task
    /// (cancelled is terminal: never retried, never dead-lettered), which
    /// is exactly what a deployment must not do. Left unsettled, the
    /// attempt returns to visibility at lease expiry and another worker
    /// picks it up — the design's "releases the rest, which return to
    /// visibility for other workers".
    ///
    /// The loop never gives up on the server: claim/heartbeat/settle
    /// transport failures are logged and retried with backoff, because in a
    /// durable system the server coming back is the normal case.
    pub async fn run(self, shutdown: CancellationToken) {
        tracing::info!(
            worker_id = %self.worker_id,
            base_url = %self.base_url,
            lease_ms = self.lease_ms(),
            pools = ?self.pools,
            worker_version = %self.worker_version.as_deref().unwrap_or(""),
            kinds = ?self.registered_kinds(),
            drain_grace_ms = self.drain_grace.as_millis() as u64,
            "activity worker started"
        );
        let mut backoff = self.claim_backoff_base;
        loop {
            // The claim poll itself is raced against the token: a drain
            // that starts mid-poll still stops this worker from taking the
            // lease. (If the server answered just as the poll was dropped,
            // the task is leased to a worker that never runs it — it
            // returns at lease expiry, the same backstop as a crash.)
            let claim = tokio::select! {
                _ = shutdown.cancelled() => break,
                outcome = self.claim() => outcome,
            };
            match claim {
                ClaimOutcome::Task(task) => {
                    backoff = self.claim_backoff_base;
                    self.run_activity(*task, &shutdown).await;
                }
                ClaimOutcome::Empty | ClaimOutcome::Unavailable => {
                    let delay = backoff;
                    backoff = backoff
                        .checked_mul(2)
                        .unwrap_or(self.claim_backoff_max)
                        .min(self.claim_backoff_max);
                    tokio::select! {
                        _ = tokio::time::sleep(delay) => {}
                        _ = shutdown.cancelled() => {}
                    }
                }
            }
        }
        tracing::info!(worker_id = %self.worker_id, "activity worker drained and stopped");
    }

    fn registered_kinds(&self) -> Vec<&str> {
        let mut kinds: Vec<&str> = self.handlers.keys().map(String::as_str).collect();
        kinds.sort_unstable();
        kinds
    }

    fn lease_ms(&self) -> u64 {
        u64::try_from(self.lease.as_millis()).unwrap_or(u64::MAX)
    }

    /// One claim poll against `POST /tasks/claim`.
    async fn claim(&self) -> ClaimOutcome {
        let body = ClaimRequest {
            worker_id: &self.worker_id,
            pools: if self.pools.is_empty() {
                None
            } else {
                Some(self.pools.as_slice())
            },
            worker_version: self.worker_version.as_deref(),
            lease_ms: self.lease_ms(),
        };
        let response = match self
            .client
            .post(format!("{}/tasks/claim", self.base_url))
            .json(&body)
            .send()
            .await
        {
            Ok(response) => response,
            Err(e) => {
                tracing::warn!(error = %e, "claim request failed");
                return ClaimOutcome::Unavailable;
            }
        };
        match response.status() {
            StatusCode::OK => match response.json::<ClaimResponse>().await {
                Ok(claimed) => ClaimOutcome::Task(Box::new(claimed.task)),
                Err(e) => {
                    tracing::warn!(error = %e, "claim returned an undecodable task body");
                    ClaimOutcome::Unavailable
                }
            },
            StatusCode::NO_CONTENT => ClaimOutcome::Empty,
            status => {
                tracing::warn!(%status, "claim rejected by server");
                ClaimOutcome::Unavailable
            }
        }
    }

    /// Execute one claimed task and settle it, inside a `rusty.activity`
    /// span. The one case without a settle call is lease loss: the server
    /// owns the task again, so any call from us would `409`. (Drain-grace
    /// expiry is the deliberate second case — see
    /// [`AttemptOutcome::DrainGraceExpired`].)
    async fn run_activity(&self, task: ClaimedTask, shutdown: &CancellationToken) {
        let span = tracing::info_span!(
            "rusty.activity",
            worker_id = %self.worker_id,
            task_id = %task.task_id,
            kind = %task.kind,
            attempt = task.attempt,
            max_attempts = task.max_attempts,
            run_id = %task.run_id.as_deref().unwrap_or(""),
            thread_id = %task.thread_id.as_deref().unwrap_or(""),
        );
        async move {
            let Some(handler) = self.handlers.get(&task.kind).cloned() else {
                self.fail(
                    &task.task_id,
                    ErrorClass::InvalidInput,
                    format!(
                        "no activity registered for kind `{}` on this worker (registered: {:?})",
                        task.kind,
                        self.registered_kinds()
                    ),
                    false,
                )
                .await;
                return;
            };

            // Work that is already cancelled must not start: report the
            // attempt cancelled without running the handler. (A
            // well-behaved server never leases such a task; this is the
            // worker-side defense of the same rule.)
            if task.cancel_requested {
                tracing::warn!("claimed task was already cancel-requested; reporting cancelled");
                self.fail(
                    &task.task_id,
                    ErrorClass::Cancelled,
                    format!("activity `{}` cancelled before dispatch", task.kind),
                    false,
                )
                .await;
                return;
            }
            // Wall-clock remaining until the whole-task deadline; a
            // deadline already passed is reported cancelled without
            // running the handler.
            let until_deadline = match task.deadline {
                Some(deadline) => match (deadline - Utc::now()).to_std() {
                    Ok(remaining) => Some(remaining),
                    Err(_) => {
                        tracing::warn!(%deadline, "claimed task's deadline already passed; reporting cancelled");
                        self.fail(
                            &task.task_id,
                            ErrorClass::Cancelled,
                            format!(
                                "activity `{}` cancelled: whole-task deadline already passed",
                                task.kind
                            ),
                            false,
                        )
                        .await;
                        return;
                    }
                },
                None => None,
            };

            let ctx = ActivityContext {
                task_id: task.task_id.clone(),
                kind: task.kind.clone(),
                attempt: task.attempt,
                idempotency_key: task.idempotency_key.clone(),
                payload: task.payload.clone(),
            };
            let task_id = task.task_id.clone();

            let lease_lost = CancellationToken::new();
            let cancel_requested = CancellationToken::new();
            let heartbeat =
                self.spawn_heartbeat(&task_id, lease_lost.clone(), cancel_requested.clone());

            // The handler runs on its own task for the same reason as in
            // `/execute`: a panic must surface as an outcome, not a
            // torn-down worker.
            let mut handle = tokio::spawn(async move { handler.run_with_receipt(ctx).await });
            // The drain bound: starts counting when the drain starts (not
            // at claim time), so a worker idle for hours and drained
            // mid-attempt still gets the full grace for that attempt.
            let drain_grace = self.drain_grace;
            let drained = shutdown.clone();
            let outcome = tokio::select! {
                joined = &mut handle => AttemptOutcome::Finished(joined),
                _ = lease_lost.cancelled() => AttemptOutcome::LeaseLost,
                _ = cancel_requested.cancelled() => AttemptOutcome::Cancelled("cancelled by the control plane"),
                _ = sleep_until_deadline(until_deadline) => AttemptOutcome::Cancelled("whole-task deadline expired mid-attempt"),
                _ = async {
                    drained.cancelled().await;
                    tokio::time::sleep(drain_grace).await;
                } => AttemptOutcome::DrainGraceExpired,
            };
            heartbeat.abort();

            let joined = match outcome {
                AttemptOutcome::Finished(joined) => joined,
                AttemptOutcome::LeaseLost => {
                    // Dropping the JoinHandle alone would detach the handler and
                    // let it keep running; abort it and await the abort so the
                    // handler future is really dropped before the next claim.
                    handle.abort();
                    let _ = handle.await;
                    tracing::warn!("lease lost; activity aborted and left for the server to reassign");
                    return;
                }
                AttemptOutcome::DrainGraceExpired => {
                    // Same abort discipline as lease loss, and the same "no
                    // settle call" — but by choice: the lease is still
                    // held, and reporting `cancelled` would kill the task
                    // (terminal, never retried). Leaving it unsettled
                    // releases the task back to visibility at lease expiry
                    // for a worker that is still serving.
                    handle.abort();
                    let _ = handle.await;
                    tracing::warn!(
                        drain_grace_ms = drain_grace.as_millis() as u64,
                        "drain grace expired; activity aborted and left for reassignment at lease expiry"
                    );
                    return;
                }
                AttemptOutcome::Cancelled(reason) => {
                    // Same abort discipline as lease loss, but here we still
                    // hold the lease: settle the attempt as cancelled so the
                    // record ends terminal-cancelled rather than waiting out
                    // the lease.
                    handle.abort();
                    let _ = handle.await;
                    tracing::warn!(reason, "activity aborted");
                    self.fail(
                        &task_id,
                        ErrorClass::Cancelled,
                        format!("activity `{}` {reason}", task.kind),
                        false,
                    )
                    .await;
                    return;
                }
            };

            match joined {
                Ok(Ok(completion)) => {
                    tracing::info!(receipt = completion.receipt.is_some(), "activity succeeded");
                    self.complete(&task_id, &completion.result, completion.receipt.as_ref())
                        .await;
                }
                Ok(Err(e)) => {
                    let (error_class, retryable) = classify_error(&e);
                    tracing::warn!(error = %e, ?error_class, retryable, "activity failed");
                    self.fail(
                        &task_id,
                        error_class,
                        format!("activity `{}` failed: {e}", task.kind),
                        retryable,
                    )
                    .await;
                }
                Err(join_err) => {
                    let detail = if join_err.is_panic() {
                        let payload = join_err.into_panic();
                        let message = payload
                            .downcast_ref::<&str>()
                            .map(|s| (*s).to_owned())
                            .or_else(|| payload.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "non-string panic payload".to_owned());
                        format!("handler panicked: {message}")
                    } else {
                        "handler task cancelled".to_owned()
                    };
                    tracing::warn!(%detail, "activity panicked");
                    self.fail(
                        &task_id,
                        ErrorClass::Unknown,
                        format!("activity `{}` {detail}", task.kind),
                        false,
                    )
                    .await;
                }
            }
        }
        .instrument(span)
        .await
    }

    /// Spawn the background heartbeat loop for one claimed task: renew the
    /// lease every `lease / 3`. The loop ends the worker's wait on the
    /// handler in two cases: a `409` cancels `lease_lost` (the server
    /// reassigns; the worker abandons), and a `200` carrying
    /// `cancel_requested: true` cancels `cancel_requested` (the worker
    /// aborts promptly and reports the attempt as cancelled through the
    /// fail path — the whole point of the hint).
    fn spawn_heartbeat(
        &self,
        task_id: &str,
        lease_lost: CancellationToken,
        cancel_requested: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        let client = self.client.clone();
        let url = format!("{}/tasks/{task_id}/heartbeat", self.base_url);
        let worker_id = self.worker_id.clone();
        let lease_ms = self.lease_ms();
        let interval = (self.lease / 3).max(Duration::from_millis(1));
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            // A delayed tick must not burst: heartbeat spacing is a liveness
            // signal, not a quota to catch up on.
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                let body = HeartbeatRequest {
                    worker_id: &worker_id,
                    lease_ms,
                };
                match client.post(&url).json(&body).send().await {
                    Ok(response) if response.status() == StatusCode::OK => {
                        match response.json::<HeartbeatResponse>().await {
                            Ok(renewed) => {
                                if renewed.cancel_requested {
                                    tracing::warn!(
                                        "heartbeat carried cancel_requested; aborting the attempt"
                                    );
                                    cancel_requested.cancel();
                                    return;
                                }
                                let expires_at = renewed
                                    .lease_expires_at
                                    .unwrap_or_else(|| "<missing>".to_owned());
                                tracing::debug!(lease_expires_at = %expires_at, "lease renewed");
                            }
                            Err(_) => {
                                // An undecodable renewal is not a lease-loss
                                // signal; the 409 is. Keep heartbeating.
                                tracing::warn!("heartbeat reply undecodable; will retry");
                            }
                        }
                    }
                    Ok(response) if response.status() == StatusCode::CONFLICT => {
                        tracing::warn!("heartbeat rejected (409): lease lost");
                        lease_lost.cancel();
                        return;
                    }
                    Ok(response) => {
                        // Any other status is treated as transient: the
                        // server will answer 409 once the lease is really
                        // gone, which is the authoritative signal.
                        tracing::warn!(status = %response.status(), "heartbeat failed; will retry");
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "heartbeat request failed; will retry");
                    }
                }
            }
        })
    }

    /// Settle a task successfully via `POST /tasks/{id}/complete`, carrying
    /// the effect receipt when the activity reported one.
    async fn complete(&self, task_id: &str, result: &Value, receipt: Option<&EffectReceipt>) {
        let body = CompleteRequest {
            worker_id: &self.worker_id,
            result,
            receipt,
        };
        let response = self
            .client
            .post(format!("{}/tasks/{task_id}/complete", self.base_url))
            .json(&body)
            .send()
            .await;
        self.log_settle("complete", response).await;
    }

    /// Settle a task as failed via `POST /tasks/{id}/fail` and log the
    /// server's retry decision (`{requeued, next_attempt_at, dead}`).
    async fn fail(&self, task_id: &str, error_class: ErrorClass, message: String, retryable: bool) {
        let body = FailRequest {
            worker_id: &self.worker_id,
            error_class,
            message: &message,
            retryable,
        };
        let response = self
            .client
            .post(format!("{}/tasks/{task_id}/fail", self.base_url))
            .json(&body)
            .send()
            .await;
        match response {
            Ok(response) if response.status() == StatusCode::OK => {
                match response.json::<FailResponse>().await {
                    Ok(outcome) => tracing::info!(
                        requeued = outcome.requeued,
                        next_attempt_at = ?outcome.next_attempt_at,
                        dead = outcome.dead,
                        "task settled as failed"
                    ),
                    Err(e) => {
                        tracing::warn!(error = %e, "task settled as failed; reply undecodable")
                    }
                }
            }
            other => self.log_settle("fail", other).await,
        }
    }

    /// Shared settle-call logging: a `409` means the server already considers
    /// the task lost or settled (our outcome is dropped, its reassignment is
    /// authoritative); a transport failure means the task returns to the
    /// queue when the lease expires.
    async fn log_settle(&self, call: &str, response: reqwest::Result<reqwest::Response>) {
        match response {
            Ok(response) if response.status() == StatusCode::OK => {
                tracing::debug!("{call} accepted");
            }
            Ok(response) if response.status() == StatusCode::CONFLICT => {
                tracing::warn!(
                    "{call} rejected (409): task already lost or settled; \
                     outcome dropped in favor of the server's reassignment"
                );
            }
            Ok(response) => {
                tracing::warn!(status = %response.status(), "{call} call failed");
            }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "{call} call failed; the task returns to the queue at lease expiry"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusty_agent_runtime::error::LlmErrorClass;
    use serde_json::json;

    #[test]
    fn closure_and_arc_implement_activity() {
        let closure = |ctx: ActivityContext| async move { Ok(ctx.payload().clone()) };
        fn assert_activity<T: Activity>(_: &T) {}
        assert_activity(&closure);

        let arc: Arc<dyn Activity> = Arc::new(closure);
        assert_activity(&arc);
    }

    #[tokio::test]
    async fn run_with_receipt_defaults_to_no_receipt() {
        // Pre-receipt activities (and the closure blanket impl) complete
        // through the provided method unchanged: their result is preserved
        // and no receipt is invented on their behalf.
        let closure = |ctx: ActivityContext| async move { Ok(json!({"echo": ctx.payload()})) };
        let ctx = ActivityContext {
            task_id: "t-1".into(),
            kind: "k".into(),
            attempt: 1,
            idempotency_key: None,
            payload: json!({"n": 1}),
        };
        let completion = closure.run_with_receipt(ctx).await.unwrap();
        assert_eq!(completion.result, json!({"echo": {"n": 1}}));
        assert!(completion.receipt.is_none());
    }

    #[tokio::test]
    async fn run_with_receipt_carries_the_reported_receipt() {
        struct Charger;
        #[async_trait]
        impl Activity for Charger {
            async fn run(&self, _ctx: ActivityContext) -> Result<Value> {
                unreachable!("the worker drives run_with_receipt")
            }
            async fn run_with_receipt(&self, ctx: ActivityContext) -> Result<ActivityCompletion> {
                Ok(ActivityCompletion {
                    result: json!({"charged": true}),
                    receipt: Some(EffectReceipt {
                        provider: "stripe".into(),
                        provider_id: "ch_3PKd".into(),
                        idempotency_key: ctx.idempotency_key().unwrap_or_default().to_owned(),
                        task_id: Some(ctx.task_id().to_owned()),
                        // R0.7: the typed-kernel effect id is unset on this
                        // pre-kernel receipt path; the idempotency-key lookup
                        // keeps serving it.
                        effect_id: None,
                    }),
                })
            }
        }
        let ctx = ActivityContext {
            task_id: "t-7".into(),
            kind: "charge_card".into(),
            attempt: 1,
            idempotency_key: Some("run-9:charge:7".into()),
            payload: json!({}),
        };
        let completion = Charger.run_with_receipt(ctx).await.unwrap();
        let receipt = completion.receipt.expect("receipt reported");
        assert_eq!(receipt.provider_id, "ch_3PKd");
        assert_eq!(receipt.idempotency_key, "run-9:charge:7");
        assert_eq!(receipt.task_id.as_deref(), Some("t-7"));
    }

    #[tokio::test]
    async fn arc_dyn_activity_delegates_run_with_receipt() {
        // Regression: the worker stores every registered handler as
        // `Arc<dyn Activity>` and drives it through `run_with_receipt`.
        // When the `Arc<dyn Activity>` impl overrode only `run`, the
        // default `run_with_receipt` answered for the Arc and silently
        // dropped the receipt — the crash-recovery proof caught it.
        struct Charger;
        #[async_trait]
        impl Activity for Charger {
            async fn run(&self, _ctx: ActivityContext) -> Result<Value> {
                unreachable!("the worker drives run_with_receipt")
            }
            async fn run_with_receipt(&self, ctx: ActivityContext) -> Result<ActivityCompletion> {
                Ok(ActivityCompletion {
                    result: json!({"charged": true}),
                    receipt: Some(EffectReceipt {
                        provider: "stripe".into(),
                        provider_id: "ch_arc".into(),
                        idempotency_key: ctx.idempotency_key().unwrap_or_default().to_owned(),
                        task_id: Some(ctx.task_id().to_owned()),
                        // R0.7: the typed-kernel effect id is unset on this
                        // pre-kernel receipt path; the idempotency-key lookup
                        // keeps serving it.
                        effect_id: None,
                    }),
                })
            }
        }
        let activity: Arc<dyn Activity> = Arc::new(Charger);
        let ctx = ActivityContext {
            task_id: "t-9".into(),
            kind: "charge_card".into(),
            attempt: 2,
            idempotency_key: Some("run-1:charge:9".into()),
            payload: json!({}),
        };
        let completion = activity.run_with_receipt(ctx).await.unwrap();
        assert_eq!(completion.result, json!({"charged": true}));
        let receipt = completion
            .receipt
            .expect("the Arc must delegate the receipt");
        assert_eq!(receipt.provider_id, "ch_arc");
        assert_eq!(receipt.idempotency_key, "run-1:charge:9");
    }

    #[test]
    fn classify_error_maps_to_the_shared_taxonomy() {
        // The transient executor classes are upstream dependency failures.
        assert_eq!(
            classify_error(&RustyError::Llm("rate limited".into())),
            (ErrorClass::DependencyFailure, true)
        );
        // A classified LLM failure keeps its finer class: rate limits and
        // provider outages retry as such; credentials and undecodable
        // responses fail without burning the attempt budget.
        assert_eq!(
            classify_error(&RustyError::LlmFailure {
                class: LlmErrorClass::RateLimited,
                message: "429".into(),
            }),
            (ErrorClass::RateLimited, true)
        );
        assert_eq!(
            classify_error(&RustyError::LlmFailure {
                class: LlmErrorClass::Server,
                message: "503".into(),
            }),
            (ErrorClass::DependencyFailure, true)
        );
        assert_eq!(
            classify_error(&RustyError::LlmFailure {
                class: LlmErrorClass::Auth,
                message: "401".into(),
            }),
            (ErrorClass::InvalidInput, false)
        );
        assert_eq!(
            classify_error(&RustyError::Tool("backend exploded".into())),
            (ErrorClass::DependencyFailure, true)
        );
        // Contract violations are invalid input.
        assert_eq!(
            classify_error(&RustyError::Graph("cycle".into())),
            (ErrorClass::InvalidInput, false)
        );
        assert_eq!(
            classify_error(&RustyError::InvalidUpdate("double write".into())),
            (ErrorClass::InvalidInput, false)
        );
        // Everything else is unclassified and hard.
        assert_eq!(
            classify_error(&RustyError::Node("boom".into())),
            (ErrorClass::Unknown, false)
        );
        assert_eq!(
            classify_error(&RustyError::Checkpoint("io".into())),
            (ErrorClass::Unknown, false)
        );
        assert_eq!(
            classify_error(&RustyError::Replay("diverged".into())),
            (ErrorClass::Unknown, false)
        );
        let serde_err = serde_json::from_str::<Value>("{").unwrap_err();
        assert_eq!(
            classify_error(&RustyError::Serialization(serde_err)),
            (ErrorClass::Unknown, false)
        );
        // Interrupts have no suspend semantics here; they settle as a
        // non-retryable cancellation.
        assert_eq!(
            classify_error(&RustyError::Interrupt { value: json!(null) }),
            (ErrorClass::Cancelled, false)
        );
    }

    #[test]
    fn claimed_task_decodes_the_server_wire_shape() {
        // The claim response wraps the task record: `{"task": {…}}`. Fields
        // the worker does not model (pool, status, lease, timestamps) must
        // be tolerated.
        let body = json!({
            "task": {
                "task_id": "t-1",
                "kind": "send_email",
                "payload": {"to": "a@b.c"},
                "pool": "default",
                "status": "leased",
                "attempt": 2,
                "max_attempts": 3,
                "error_class": null,
                "last_error": "previous attempt blew up",
                "idempotency_key": "run-9:charge:7",
                "result": null,
                "run_id": "run-9",
                "thread_id": "thread-1",
                "cancel_requested": false,
                "deadline": "2026-08-07T13:00:00Z",
                "lease": {"owner": "w-1", "expires_at": "2026-08-07T12:00:30Z"},
                "next_attempt_at": null,
                "created_at": "2026-08-07T12:00:00Z",
                "updated_at": "2026-08-07T12:00:01Z"
            }
        });
        let claimed: ClaimResponse = serde_json::from_value(body).unwrap();
        let task = claimed.task;
        assert_eq!(task.task_id, "t-1");
        assert_eq!(task.kind, "send_email");
        assert_eq!(task.payload, json!({"to": "a@b.c"}));
        assert_eq!(task.attempt, 2);
        assert_eq!(task.max_attempts, 3);
        assert_eq!(task.idempotency_key.as_deref(), Some("run-9:charge:7"));
        assert_eq!(task.run_id.as_deref(), Some("run-9"));
        assert_eq!(task.thread_id.as_deref(), Some("thread-1"));
        assert!(!task.cancel_requested);
        assert_eq!(
            task.deadline,
            DateTime::parse_from_rfc3339("2026-08-07T13:00:00Z")
                .ok()
                .map(|dt| dt.with_timezone(&Utc))
        );
    }

    #[test]
    fn claimed_task_defaults_optional_fields() {
        // A pre-wave-2 server omits the cancellation fields entirely.
        let task: ClaimedTask =
            serde_json::from_value(json!({"task_id": "t-1", "kind": "k"})).unwrap();
        assert_eq!(task.payload, Value::Null);
        assert_eq!(task.attempt, 0);
        assert!(task.idempotency_key.is_none());
        assert!(task.run_id.is_none() && task.thread_id.is_none());
        assert!(!task.cancel_requested);
        assert!(task.deadline.is_none());
    }

    #[test]
    fn claim_request_omits_pools_when_unconfigured() {
        let body = ClaimRequest {
            worker_id: "w-1",
            pools: None,
            worker_version: None,
            lease_ms: 100,
        };
        let value = serde_json::to_value(&body).unwrap();
        assert_eq!(value, json!({"worker_id": "w-1", "lease_ms": 100}));

        let pools = vec!["gpu".to_string()];
        let body = ClaimRequest {
            worker_id: "w-1",
            pools: Some(pools.as_slice()),
            worker_version: None,
            lease_ms: 100,
        };
        let value = serde_json::to_value(&body).unwrap();
        assert_eq!(
            value,
            json!({"worker_id": "w-1", "pools": ["gpu"], "lease_ms": 100})
        );
    }

    #[test]
    fn claim_request_carries_the_advertised_worker_version() {
        // Wave 3: a configured version travels on the claim; unconfigured
        // the field is omitted entirely (pre-wave-3 servers never see it).
        let body = ClaimRequest {
            worker_id: "w-1",
            pools: None,
            worker_version: Some("activity-worker/1.4.0"),
            lease_ms: 100,
        };
        let value = serde_json::to_value(&body).unwrap();
        assert_eq!(
            value,
            json!({"worker_id": "w-1", "worker_version": "activity-worker/1.4.0", "lease_ms": 100})
        );
    }

    #[test]
    fn settle_request_shapes_match_the_contract() {
        let heartbeat = HeartbeatRequest {
            worker_id: "w-1",
            lease_ms: 100,
        };
        assert_eq!(
            serde_json::to_value(&heartbeat).unwrap(),
            json!({"worker_id": "w-1", "lease_ms": 100})
        );

        let result = json!({"sent": true});
        let complete = CompleteRequest {
            worker_id: "w-1",
            result: &result,
            receipt: None,
        };
        assert_eq!(
            serde_json::to_value(&complete).unwrap(),
            json!({"worker_id": "w-1", "result": {"sent": true}})
        );

        // An activity reporting a receipt carries it with the completion;
        // without one the body is byte-identical to the pre-receipt shape.
        let receipt = EffectReceipt {
            provider: "stripe".into(),
            provider_id: "ch_3PKd".into(),
            idempotency_key: "run-9:charge:7".into(),
            task_id: Some("t-1".into()),
            effect_id: None,
        };
        let complete = CompleteRequest {
            worker_id: "w-1",
            result: &result,
            receipt: Some(&receipt),
        };
        assert_eq!(
            serde_json::to_value(&complete).unwrap(),
            json!({
                "worker_id": "w-1",
                "result": {"sent": true},
                "receipt": {
                    "provider": "stripe",
                    "provider_id": "ch_3PKd",
                    "idempotency_key": "run-9:charge:7",
                    "task_id": "t-1"
                }
            })
        );

        let fail = FailRequest {
            worker_id: "w-1",
            error_class: ErrorClass::DependencyFailure,
            message: "backend exploded",
            retryable: true,
        };
        assert_eq!(
            serde_json::to_value(&fail).unwrap(),
            json!({
                "worker_id": "w-1",
                "error_class": "dependency_failure",
                "message": "backend exploded",
                "retryable": true
            })
        );
    }

    #[test]
    fn heartbeat_and_fail_responses_decode() {
        let heartbeat: HeartbeatResponse =
            serde_json::from_value(json!({"lease_expires_at": "2026-08-07T12:00:30Z"})).unwrap();
        assert_eq!(
            heartbeat.lease_expires_at.as_deref(),
            Some("2026-08-07T12:00:30Z")
        );
        // The cancellation hint defaults off (pre-wave-2 servers omit it).
        assert!(!heartbeat.cancel_requested);

        let heartbeat: HeartbeatResponse = serde_json::from_value(
            json!({"lease_expires_at": "2026-08-07T12:00:30Z", "cancel_requested": true}),
        )
        .unwrap();
        assert!(heartbeat.cancel_requested);

        let fail: FailResponse = serde_json::from_value(json!({
            "requeued": true,
            "next_attempt_at": "2026-08-07T12:01:00Z",
            "dead": false
        }))
        .unwrap();
        assert!(fail.requeued && !fail.dead);
        assert_eq!(
            fail.next_attempt_at.as_deref(),
            Some("2026-08-07T12:01:00Z")
        );
    }

    #[test]
    fn lease_is_clamped_to_the_server_bounds() {
        let worker = ActivityWorker::new("http://127.0.0.1:1").with_lease(Duration::from_millis(5));
        assert_eq!(worker.lease, MIN_LEASE);
        let worker =
            ActivityWorker::new("http://127.0.0.1:1").with_lease(Duration::from_secs(86_400));
        assert_eq!(worker.lease, MAX_LEASE);
    }
}
