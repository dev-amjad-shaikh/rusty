//! Durable task queue (R0.6 — Durable Work): effectively-once activities.
//!
//! A *task* is a unit of work enqueued by a control-plane caller (`POST
//! /tasks` directly, or `POST /tasks/outbox` through the wave-2b
//! transactional outbox — the relay publishes it into this queue) and
//! executed by an out-of-process worker. The server owns the substrate:
//! durable records, leases with visibility timeouts, heartbeats, retries, a
//! dead-letter queue, and cancellation propagation (wave 2a): the cancel
//! endpoints move non-terminal tasks to the terminal `cancelled` state —
//! immediately for queued work, via a `cancel_requested` heartbeat hint for
//! leased work — and the claim path finalizes unanswered cancels and
//! elapsed whole-task deadlines instead of re-leasing. The retry policy is
//! not local: failed attempts are
//! classified into core's shared [`ErrorClass`] taxonomy and decided by
//! core's [`classify_retry_with_policy`] — the same classifier the worker
//! SDK runs — against the acting executor policy's resolved retry
//! parameters (R0.10 wave 4; the static floor resolves to exactly the
//! pre-wave-4 constants), so
//! server and workers can never disagree about a retry (see
//! `docs/durable-work-design.md`). The guarantee is *effectively once*: a
//! task may be delivered more than once (lease expiry reclaims it), so
//! workers make their effects idempotent — the enqueue-side
//! `idempotency_key` dedups task creation, and the lease/409 protocol
//! ensures only the current lease holder can settle a task.
//!
//! Records are durable: one JSON file per task under
//! `{store_path}/tasks/{task_id}.json` on the default backend (atomic
//! temp+rename writes, reloaded at startup), or the auto-migrated
//! `server_tasks` table with the `postgres` feature. Task ids are
//! server-minted UUIDs; tenant isolation goes through the record's `tenant`
//! field (set from the request's [`crate::auth::TenantContext`]), which every
//! store operation is scoped by — a cross-tenant id simply does not resolve.
//!
//! Wave 3 layers placement and pressure controls onto the same records:
//! named pools carry per-pool concurrency limits (the claim path counts live
//! leases against the configured cap and stops handing out leases at it, so
//! a GPU-bound pool and an IO-bound pool coexist without starving each
//! other), tenant quotas cap tasks queued / in flight / dead-lettered at
//! submission (`429`), an optional exact-match `worker_version` pin keeps a
//! run dispatching to the worker version it started against, and
//! `GET /tasks/metrics` publishes the per-pool autoscaling signals (queue
//! depth, oldest-visible age, lease saturation) an operator's autoscaler
//! consumes — metrics, never a built-in autoscaler.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use rusty_agent_runtime::durable::{
    classify_retry_with_policy, ErrorClass, ResolvedRetryParameters, RetryDecision,
};
use rusty_agent_runtime::llm::Usage;
use rusty_agent_runtime::record::ExecutorPolicy;
use rusty_agent_runtime::record::{Effect, EffectReceipt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// The pool every task lands in when the enqueue payload names none.
pub(crate) const DEFAULT_POOL: &str = "default";

/// The task kind a memory consolidation enqueues as (R0.8 Rusty Learn,
/// wave 2): distilling N records in a scope into the one `summary` record
/// that names and supersedes them. The queue machinery is the unchanged
/// R0.6 one — leased, retried under the shared [`ErrorClass`] taxonomy,
/// dead-lettered with evidence, quota-counted per tenant; the payload
/// names exactly the records the task reads (explicit ids, the auditable
/// selector) plus the `written_at` minted at enqueue, so a retried
/// execution names the same learning instant and its content-addressed
/// summary write converges. The distillation semantics stay with the
/// claiming worker, never the queue — the same boundary the design draws
/// for distillers.
pub(crate) const MEMORY_CONSOLIDATION_KIND: &str = "memory_consolidation";

/// Default attempt ceiling when the enqueue payload sets none. Three
/// attempts = the initial try plus two retries before dead-lettering.
pub(crate) const DEFAULT_MAX_ATTEMPTS: u32 = 3;

/// Hard ceiling on `max_attempts`; bounds the retry schedule and keeps the
/// `u32 -> i32` Postgres column mapping lossless by construction.
pub(crate) const MAX_ATTEMPTS_LIMIT: u32 = 100;

/// Lease bounds accepted on claim/heartbeat: 100 ms (anything shorter is an
/// instant expiry — claimable again before the holder can act) to one hour.
pub(crate) const MIN_LEASE_MS: u64 = 100;
/// See [`MIN_LEASE_MS`].
pub(crate) const MAX_LEASE_MS: u64 = 3_600_000;

/// Lifecycle of a durable task.
///
/// ```text
/// queued ──claim──> leased ──complete──> completed   (terminal)
///                     │
///                     ├──fail──> RetryDecision::Retry ──> failed (next_attempt_at set)
///                     │            ──backoff elapsed──> claimable again
///                     ├──fail──> RetryDecision::Dead  ──> dead     (terminal, DLQ)
///                     ├──fail──> RetryDecision::Fail  ──> failed (next_attempt_at null;
///                     │                                     terminal, *not* the DLQ)
///                     └──lease expires──> claimable again (new attempt)
///
/// cancelled (terminal) is reached three ways, all spelled `cancelled`:
///   - the cancel endpoint on a queued or retry-scheduled task (immediate),
///   - a worker reporting ErrorClass::Cancelled through the fail path,
///   - the claim path finalizing a cancel-requested or deadline-expired
///     task instead of re-leasing it.
/// ```
///
/// `Failed` covers both failure resting states, distinguished by
/// `next_attempt_at`: set = a retry is scheduled (claimable once it
/// passes); null = the shared retry policy
/// ([`classify_retry_with_policy`]) failed the task outright — a
/// non-retryable class or
/// work the worker declared unsafe to re-drive. Terminal failure that a
/// human can act on is `Dead` (the DLQ, `RetryDecision::Dead`); the DLQ
/// never holds outright fails, per the design's "DLQ is for actionable
/// work, not a graveyard" rule. `Cancelled` is terminal too — control
/// flow, not failure: never retried, never dead-lettered, never re-queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum TaskStatus {
    Queued,
    Leased,
    Failed,
    Completed,
    Dead,
    Cancelled,
}

impl TaskStatus {
    /// The wire/storage spelling (also the Postgres `status` column value).
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Leased => "leased",
            Self::Failed => "failed",
            Self::Completed => "completed",
            Self::Dead => "dead",
            Self::Cancelled => "cancelled",
        }
    }

    /// Parse a `?status=` filter value; `None` for unknown statuses (the
    /// route answers 400 — a silently ignored filter would hide DLQ entries).
    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s {
            "queued" => Some(Self::Queued),
            "leased" => Some(Self::Leased),
            "failed" => Some(Self::Failed),
            "completed" => Some(Self::Completed),
            "dead" => Some(Self::Dead),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

/// A live lease: the worker currently allowed to settle the task, and the
/// visibility timeout. Past `expires_at` the task is claimable again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TaskLease {
    pub owner: String,
    pub expires_at: DateTime<Utc>,
}

/// One durable task. Persisted whole (JSON file) or column-mapped
/// (Postgres); the wire representation is [`TaskRecord::wire`], which omits
/// `tenant` (an internal isolation detail, like the `{tenant}/` id prefixes
/// elsewhere in the server).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TaskRecord {
    pub task_id: String,
    /// Owning tenant (`default` in open/dev mode). Every store operation is
    /// scoped by it; cross-tenant ids resolve to nothing → 404.
    pub tenant: String,
    pub kind: String,
    /// Work payload: arbitrary JSON. Large payloads are out of scope until
    /// the artifact store lands (R0.7); a caller can always pass a reference
    /// object of its own making — the server stores the value verbatim.
    pub payload: Value,
    pub pool: String,
    /// Recipient addressing (R0.7 Agent Fabric wave 1): the queue recipient
    /// this task is addressed to, today always a mailbox address of the
    /// form `agent:{agent_id}` (see
    /// [`rusty_agent_runtime::agents::AGENT_RECIPIENT_PREFIX`]). `None` (the
    /// default) is ordinary pool work, claimable through `POST
    /// /tasks/claim`. A recipient-addressed task is mailbox traffic: the
    /// pool claim path never hands it out, and it is drained one message at
    /// a time through the turn-serialized agent claim
    /// ([`ServerStore::claim_agent_task`](crate::server_store::ServerStore::claim_agent_task)).
    /// Additive — records written before R0.7 deserialize with `None`.
    #[serde(default)]
    pub recipient: Option<String>,
    pub status: TaskStatus,
    /// 1-based number of the current/last attempt (0 = never claimed).
    pub attempt: u32,
    pub max_attempts: u32,
    /// The lease, present exactly while `status == leased`.
    #[serde(default)]
    pub lease: Option<TaskLease>,
    /// Classification of the last failed attempt's error — core's closed
    /// [`ErrorClass`] taxonomy (snake_case on the wire and in storage),
    /// shared verbatim with the worker SDK so both sides of the queue agree.
    #[serde(default)]
    pub error_class: Option<ErrorClass>,
    /// The declared effect classification of the work (core's [`Effect`]
    /// taxonomy), when the enqueuer declared one. This is the effect gate's
    /// input on failure: a declared non-repeatable effect is never silently
    /// retried, whatever the worker's `retryable` flag says. `None` defers
    /// to the worker's per-attempt `retryable` declaration.
    #[serde(default)]
    pub effect: Option<Effect>,
    /// The last failed attempt's error message.
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub idempotency_key: Option<String>,
    /// The completed task's result (any JSON value), set by `complete`.
    #[serde(default)]
    pub result: Option<Value>,
    /// The effect receipt reported with completion (R0.6 wave 2b): the
    /// provider's own confirmation of an `Idempotent` effect (provider id,
    /// stored idempotency key), carried through the complete call. The
    /// server journals it into the task's run as an `effect_receipt` event
    /// (see `docs/durable-work-design.md`); the record keeps a durable copy
    /// so the evidence survives independent of the journal. Additive —
    /// `None` for tasks that report no receipt.
    #[serde(default)]
    pub receipt: Option<EffectReceipt>,
    /// Settlement cost evidence (R0.7 wave 3): the token usage and monetary
    /// cost the worker reported with the terminal settle (complete or
    /// fail). Persisted on the record — not only journaled — because the
    /// coordination runtime's waste accounting must survive the settle →
    /// journal crash window: a server that crashes between the two still
    /// recomputes the same outcome from the same records. Additive — `None`
    /// for tasks whose workers report nothing.
    #[serde(default)]
    pub tokens: Option<Usage>,
    /// See [`TaskRecord::tokens`].
    #[serde(default)]
    pub cost_usd: Option<f64>,
    /// Run/thread linkage: the run this task belongs to. Set at enqueue
    /// time; `POST /runs/{run_id}/cancel` cancels every non-terminal task
    /// carrying its run id (the outbox wave will set these from the run
    /// itself). See [`TaskRecord::thread_id`].
    #[serde(default)]
    pub run_id: Option<String>,
    /// See [`TaskRecord::run_id`].
    #[serde(default)]
    pub thread_id: Option<String>,
    /// Causal parentage (R0.7 wave 3): the journal event id this task was
    /// submitted under — for coordination member tasks, the `MailboxSend`
    /// event in the pattern's journal. This is the link TeamTrace stitches
    /// the task's own run journal onto. Additive — `None` for tasks
    /// submitted outside any journaled causality.
    #[serde(default)]
    pub parent: Option<String>,
    /// Cancellation signalled to the lease holder: set by the cancel
    /// endpoint on a leased task, surfaced on heartbeat responses, and
    /// honored by the worker aborting the attempt and reporting
    /// [`ErrorClass::Cancelled`]. Cancellation is a hint for promptness —
    /// if the holder never asks, the claim path finalizes the task as
    /// cancelled once the lease lapses instead of re-leasing it.
    #[serde(default)]
    pub cancel_requested: bool,
    /// Whole-task deadline, across attempts. The claim path never leases a
    /// task whose deadline has passed (it finalizes it as cancelled); the
    /// worker treats an expired deadline mid-attempt as
    /// [`ErrorClass::Cancelled`] — deadline expiry is cancellation by clock.
    #[serde(default)]
    pub deadline: Option<DateTime<Utc>>,
    /// Version pin (R0.6 wave 3): the exact worker version string the claim
    /// path may lease this task to. A run stamps its tasks with the worker
    /// version it started against, so a mid-run deploy never changes
    /// semantics under an in-flight execution. `None` (the default) means
    /// unpinned — any worker may claim it. Exact string match only; semver
    /// ranges are documented future work (see `docs/durable-work-design.md`).
    #[serde(default)]
    pub worker_version: Option<String>,
    /// When a `failed` task becomes claimable again (`None` while queued,
    /// leased, or terminal).
    #[serde(default)]
    pub next_attempt_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// The enqueuer-supplied shape of a new task: everything [`TaskRecord::new`]
/// needs beyond its timestamps. Grouping these keeps the constructor (and
/// its call sites) readable.
pub(crate) struct NewTask {
    pub task_id: String,
    pub tenant: String,
    pub kind: String,
    pub payload: Value,
    pub pool: String,
    /// Recipient addressing (R0.7 wave 1); see [`TaskRecord::recipient`].
    pub recipient: Option<String>,
    pub max_attempts: u32,
    pub idempotency_key: Option<String>,
    pub effect: Option<Effect>,
    /// Run/thread linkage and whole-task deadline (see [`TaskRecord`]).
    pub run_id: Option<String>,
    pub thread_id: Option<String>,
    pub deadline: Option<DateTime<Utc>>,
    /// Causal parentage (R0.7 wave 3; see [`TaskRecord::parent`]).
    pub parent: Option<String>,
    /// Version pin (see [`TaskRecord::worker_version`]).
    pub worker_version: Option<String>,
}

/// A worker's report of a failed attempt: the shared [`ErrorClass`]
/// classification, a human-readable message, and whether the worker judges
/// the work safe to re-drive. Grouped so the store trait's `fail_task` stays
/// within the argument ceiling.
pub(crate) struct FailureReport {
    pub error_class: ErrorClass,
    pub message: String,
    pub retryable: bool,
    /// The cost evidence the worker reported with the failure (R0.7 wave
    /// 3); see [`TaskRecord::tokens`].
    pub cost: SettlementCost,
    /// The retry parameters the acting executor policy resolves to for this
    /// failure class (R0.10 wave 4), computed by the route handler before
    /// the store call — the policy registry and run-version pin live at that
    /// layer, so the store just applies the decision. Under the static floor
    /// (or anything that fails closed) this equals
    /// [`ResolvedRetryParameters::floor`] of the task's budget, which is
    /// byte-for-byte the pre-wave-4 behavior.
    pub retry: ResolvedRetryParameters,
}

/// A worker's report of a successful settle: the result payload, the effect
/// receipt proving declared effects ran under policy, and the cost evidence
/// reported with the settle. Grouped so the store trait's `complete_task`
/// stays within the argument ceiling, mirroring [`FailureReport`].
pub(crate) struct CompletionReport {
    pub result: Value,
    pub receipt: Option<EffectReceipt>,
    pub cost: SettlementCost,
}

/// The cost evidence a worker reports with a terminal settle (R0.7 wave 3):
/// token usage and monetary cost, independently optional — a worker may
/// know one and not the other. `Copy` so it threads through the settle
/// paths without ceremony.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct SettlementCost {
    pub tokens: Option<Usage>,
    pub cost_usd: Option<f64>,
}

impl SettlementCost {
    /// No cost evidence reported — the pre-wave-3 default for every settle.
    /// Used by the unit tests' settle calls; production settle paths build
    /// the struct from the worker's payload fields.
    #[allow(dead_code)]
    pub(crate) const NONE: Self = Self {
        tokens: None,
        cost_usd: None,
    };
}

impl TaskRecord {
    /// A freshly enqueued task: `queued`, attempt 0, claimable immediately.
    pub(crate) fn new(new: NewTask, now: DateTime<Utc>) -> Self {
        let NewTask {
            task_id,
            tenant,
            kind,
            payload,
            pool,
            recipient,
            max_attempts,
            idempotency_key,
            effect,
            run_id,
            thread_id,
            deadline,
            worker_version,
            parent,
        } = new;
        Self {
            task_id,
            tenant,
            kind,
            payload,
            pool,
            recipient,
            status: TaskStatus::Queued,
            attempt: 0,
            max_attempts,
            lease: None,
            error_class: None,
            effect,
            last_error: None,
            idempotency_key,
            result: None,
            receipt: None,
            tokens: None,
            cost_usd: None,
            run_id,
            thread_id,
            parent,
            cancel_requested: false,
            deadline,
            worker_version,
            next_attempt_at: None,
            created_at: now,
            updated_at: now,
        }
    }

    /// `true` when a claim at `now` may take this task: fresh or
    /// backoff-elapsed, or leased past its visibility timeout (safe
    /// reassignment after worker loss).
    pub(crate) fn claimable_at(&self, now: DateTime<Utc>) -> bool {
        match self.status {
            TaskStatus::Queued => true,
            TaskStatus::Failed => self.next_attempt_at.is_some_and(|at| at <= now),
            TaskStatus::Leased => self.lease.as_ref().is_some_and(|l| l.expires_at <= now),
            TaskStatus::Completed | TaskStatus::Dead | TaskStatus::Cancelled => false,
        }
    }

    /// `true` when a worker advertising `worker_version` may claim this
    /// task (R0.6 wave 3): an unpinned task matches any worker — including
    /// one advertising no version — while a pinned task matches only the
    /// exact version string it names. The Postgres claim path expresses the
    /// same rule in SQL (`worker_version IS NULL OR worker_version = $5`);
    /// the two must agree, and the SQL-shape tests pin that they do.
    pub(crate) fn matches_worker_version(&self, worker_version: Option<&str>) -> bool {
        self.worker_version
            .as_deref()
            .is_none_or(|pin| Some(pin) == worker_version)
    }

    /// `true` for a record cancellation can no longer change: `completed`,
    /// `dead`, `cancelled`, or `failed` outright with no retry scheduled.
    /// A `failed` task *with* a retry outstanding is non-terminal — it
    /// would otherwise re-queue, which cancellation exists to prevent.
    pub(crate) fn is_terminal(&self) -> bool {
        match self.status {
            TaskStatus::Completed | TaskStatus::Dead | TaskStatus::Cancelled => true,
            TaskStatus::Failed => self.next_attempt_at.is_none(),
            TaskStatus::Queued | TaskStatus::Leased => false,
        }
    }

    /// `true` when `worker_id` currently holds this task's lease. Past-expiry
    /// leases that no other worker has reclaimed still count: the lease check
    /// is atomic with the mutation, so the holder can never double-settle —
    /// a reclaimed task has a new owner and fails this check.
    pub(crate) fn leased_to(&self, worker_id: &str) -> bool {
        self.status == TaskStatus::Leased
            && self.lease.as_ref().is_some_and(|l| l.owner == worker_id)
    }

    /// Take the task as a new attempt for `worker_id`.
    pub(crate) fn claim(&mut self, worker_id: &str, lease_ms: u64, now: DateTime<Utc>) {
        self.status = TaskStatus::Leased;
        self.attempt = self.attempt.saturating_add(1);
        self.lease = Some(TaskLease {
            owner: worker_id.to_string(),
            expires_at: now + lease_duration(lease_ms),
        });
        self.next_attempt_at = None;
        self.updated_at = now;
    }

    /// Extend the held lease (heartbeat). Caller checked [`Self::leased_to`].
    pub(crate) fn renew_lease(&mut self, lease_ms: u64, now: DateTime<Utc>) {
        if let Some(lease) = &mut self.lease {
            lease.expires_at = now + lease_duration(lease_ms);
        }
        self.updated_at = now;
    }

    /// Settle the task successfully, storing `result`, the effect `receipt`,
    /// and the settlement `cost` the worker reported with completion (when
    /// any). Caller checked [`Self::leased_to`]. `error_class` / `last_error`
    /// from earlier failed attempts are kept — they are the history of what
    /// this task survived.
    pub(crate) fn complete(
        &mut self,
        result: Value,
        receipt: Option<EffectReceipt>,
        cost: SettlementCost,
        now: DateTime<Utc>,
    ) {
        self.status = TaskStatus::Completed;
        self.result = Some(result);
        self.receipt = receipt;
        self.tokens = cost.tokens;
        self.cost_usd = cost.cost_usd;
        self.lease = None;
        self.next_attempt_at = None;
        self.updated_at = now;
    }

    /// Cancel a non-terminal task; `None` (no change) when the task is
    /// already terminal. See [`CancelTransition`] for the two paths — the
    /// leased path is what makes cancellation a *hint*: the holder keeps
    /// its lease and learns of the request on its next heartbeat, and the
    /// terminal transition lands when it reports
    /// [`ErrorClass::Cancelled`] (or, if it never asks, when the claim path
    /// finalizes the task after the lease lapses).
    pub(crate) fn cancel(&mut self, now: DateTime<Utc>) -> Option<CancelTransition> {
        if self.is_terminal() {
            return None;
        }
        if self.status == TaskStatus::Leased {
            self.cancel_requested = true;
            self.updated_at = now;
            Some(CancelTransition::Signalled)
        } else {
            self.apply_cancellation(now);
            Some(CancelTransition::Cancelled)
        }
    }

    /// The terminal transition shared by [`Self::cancel`] and the claim
    /// path's finalization sweep. The lease and any retry schedule are
    /// cleared so nothing re-queues; `error_class` records *why* the task
    /// ended (control flow, not failure — never the DLQ).
    pub(crate) fn apply_cancellation(&mut self, now: DateTime<Utc>) {
        self.status = TaskStatus::Cancelled;
        self.error_class = Some(ErrorClass::Cancelled);
        self.lease = None;
        self.next_attempt_at = None;
        self.updated_at = now;
    }

    /// `true` when this claimable-at-`now` task must be finalized as
    /// cancelled instead of (re-)leased: a cancellation the lease holder
    /// never acknowledged before its lease lapsed, or a whole-task
    /// deadline that has passed.
    pub(crate) fn cancellation_due(&self, now: DateTime<Utc>) -> bool {
        self.claimable_at(now) && (self.cancel_requested || self.deadline.is_some_and(|d| d <= now))
    }

    /// Record a failed attempt, deciding through core's shared
    /// [`classify_retry_with_policy`] classifier against the caller-resolved
    /// [`ResolvedRetryParameters`]. Caller checked [`Self::leased_to`].
    ///
    /// The effect gate's input: the task's declared [`Effect`] when the
    /// enqueuer supplied one (a declared non-repeatable effect is never
    /// silently retried — the declaration outranks the worker's flag);
    /// otherwise the worker's `retryable` bool stands in, as the executor's
    /// own declaration that re-driving this work is safe (`true` →
    /// [`Effect::Idempotent`], `false` → the conservative
    /// [`Effect::NonIdempotent`]).
    ///
    /// The decision lands as: `Retry` → [`TaskStatus::Failed`] with
    /// `next_attempt_at` set (backoff + full jitter, capped at the resolved
    /// parameters' ceiling); `Dead` → [`TaskStatus::Dead`] (the DLQ);
    /// `Fail` → [`TaskStatus::Failed`] with `next_attempt_at` null —
    /// terminal, but *not* dead-lettered.
    pub(crate) fn fail(
        &mut self,
        error_class: ErrorClass,
        message: &str,
        retryable: bool,
        cost: SettlementCost,
        retry: &ResolvedRetryParameters,
        now: DateTime<Utc>,
    ) {
        let effect = self.effect.unwrap_or(if retryable {
            Effect::Idempotent
        } else {
            Effect::NonIdempotent
        });
        // The attempt budget the classifier sees is the *resolved* one: a
        // promoted policy may narrow the task's declared budget, and the
        // dead-letter boundary must sit where the acting policy puts it.
        match classify_retry_with_policy(effect, error_class, self.attempt, retry, uniform()) {
            RetryDecision::Retry { after_ms } => {
                self.status = TaskStatus::Failed;
                self.next_attempt_at =
                    Some(now + Duration::milliseconds(after_ms.min(i64::MAX as u64) as i64));
            }
            RetryDecision::Dead => {
                self.status = TaskStatus::Dead;
                self.next_attempt_at = None;
            }
            RetryDecision::Fail => {
                // A cancelled attempt lands in the dedicated terminal state
                // rather than the outright-failure one — same finality, but
                // spelled as control flow so `?status=failed` stays a list
                // of failures and `?status=cancelled` a list of cancels.
                self.status = if error_class == ErrorClass::Cancelled {
                    TaskStatus::Cancelled
                } else {
                    TaskStatus::Failed
                };
                self.next_attempt_at = None;
            }
        }
        self.error_class = Some(error_class);
        self.last_error = Some(message.to_string());
        // Cost evidence is per-attempt: a later settle overwrites it, the
        // same discipline as `last_error`.
        self.tokens = cost.tokens;
        self.cost_usd = cost.cost_usd;
        self.lease = None;
        self.updated_at = now;
    }

    /// The public representation: every field a worker needs, minus `tenant`.
    pub(crate) fn wire(&self) -> Value {
        json!({
            "task_id": self.task_id,
            "kind": self.kind,
            "payload": self.payload,
            "pool": self.pool,
            "recipient": self.recipient,
            "status": self.status.as_str(),
            "attempt": self.attempt,
            "max_attempts": self.max_attempts,
            "error_class": self.error_class,
            "effect": self.effect,
            "last_error": self.last_error,
            "idempotency_key": self.idempotency_key,
            "result": self.result,
            "receipt": self.receipt,
            "tokens": self.tokens,
            "cost_usd": self.cost_usd,
            "run_id": self.run_id,
            "thread_id": self.thread_id,
            "parent": self.parent,
            "cancel_requested": self.cancel_requested,
            "deadline": self.deadline,
            "worker_version": self.worker_version,
            "lease": self.lease.as_ref().map(|lease| json!({
                "owner": lease.owner,
                "expires_at": lease.expires_at,
            })),
            "next_attempt_at": self.next_attempt_at,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
        })
    }
}

fn lease_duration(lease_ms: u64) -> Duration {
    Duration::milliseconds(lease_ms.min(i64::MAX as u64) as i64)
}

/// Result of a lease-guarded mutation (heartbeat / complete / fail).
#[derive(Debug, Clone)]
pub(crate) enum MutationOutcome {
    /// The mutation landed; carries the updated record (boxed — the record
    /// is far larger than the other variants).
    Applied(Box<TaskRecord>),
    /// The task exists (in this tenant) but the caller does not hold its
    /// lease — already settled, never leased, or reclaimed by another
    /// worker after expiry. Routes answer 409.
    LeaseLost,
    /// No such task in this tenant (unknown or cross-tenant id — the two
    /// are deliberately indistinguishable). Routes answer 404.
    Unknown,
}

/// What [`TaskRecord::cancel`] did to a non-terminal task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CancelTransition {
    /// The task moved to the terminal `cancelled` state immediately: it
    /// was queued, or failed with a retry scheduled.
    Cancelled,
    /// The task is leased, so the cancellation is recorded as
    /// `cancel_requested` for the holder to learn on its next heartbeat;
    /// the terminal transition lands through the fail path (prompt case)
    /// or the claim path's finalization (holder never asked).
    Signalled,
}

/// Result of the cancel endpoint (`POST /tasks/{id}/cancel`). Not
/// lease-guarded — the canceller is the tenant's control plane, not the
/// lease holder — so [`MutationOutcome`] does not fit; this mirrors its
/// shape.
#[derive(Debug, Clone)]
pub(crate) enum CancelOutcome {
    /// The cancellation landed (either [`CancelTransition`]; the record's
    /// status says which).
    Applied(Box<TaskRecord>),
    /// The task exists but is already terminal; carries its status for the
    /// 409 message.
    Terminal(TaskStatus),
    /// No such task in this tenant (unknown or cross-tenant id). Routes
    /// answer 404.
    Unknown,
}

/// What a run-level cancel (`POST /runs/{run_id}/cancel`) did to the run's
/// outstanding tasks, split by how each task's cancellation lands.
#[derive(Debug, Default)]
pub(crate) struct RunCancellation {
    /// Tasks moved to the terminal `cancelled` state immediately (queued,
    /// or failed with a retry scheduled).
    pub cancelled: Vec<TaskRecord>,
    /// Leased tasks whose holders were signalled via `cancel_requested`.
    pub signalled: Vec<TaskRecord>,
}

/// What a worker will take right now (R0.6 wave 3): the pools it serves,
/// each pool's configured concurrency limit, and the exact worker version
/// it advertises. Bundled because every claim carries all three — the
/// placement rules (pool saturation, version pinning) are applied together
/// before any candidate is chosen, and passing them separately invited
/// half-applied checks.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ClaimScope<'a> {
    /// Pools the worker serves; a claim considers only these pools' tasks.
    pub pools: &'a [String],
    /// Per-pool live-lease caps. A pool absent here is uncapped; a cap of
    /// `0` pauses the pool (it hands out nothing until the cap is raised).
    pub pool_limits: &'a HashMap<String, usize>,
    /// The version the worker advertises, matched exactly against a task's
    /// pin ([`TaskRecord::worker_version`]); `None` claims unpinned work
    /// only.
    pub worker_version: Option<&'a str>,
    /// The acting executor policy (R0.10 wave 4): the store narrows the
    /// handed-out lease to the policy's timeout bound for the task's kind
    /// when it declares one
    /// ([`rusty_agent_runtime::durable::resolve_timeout_bound_ms`]). Queue
    /// scheduling is deployment-scoped, so the claim path follows the
    /// tenant's active policy rather than any run's admission pin. Under
    /// the static floor every bound is `None` and the lease is untouched —
    /// byte-for-byte the pre-wave-4 behavior.
    pub timeout_policy: &'a ExecutorPolicy,
}

/// A tenant's queue pressure (R0.6 wave 3): the three gauges tenant quotas
/// enforce at submission (`429` when exceeded — see
/// `ServerConfig::with_task_quota` and `docs/durable-work-design.md`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TaskUsage {
    /// The backlog: `queued` tasks plus `failed` ones with a retry
    /// scheduled — every task waiting for a worker — **plus outbox rows
    /// still pending publication**. The pending rows count because the
    /// quota exists to bound a tenant's work in the pipeline, and a flood
    /// through the outbox must not bypass it.
    pub queued: u64,
    /// Tasks with status `leased` — in flight with a worker. The gauge is
    /// the record's status, not the wall clock: an expired-but-unreclaimed
    /// lease still counts until the claim path takes it back, so the quota
    /// never under-counts a worker that is merely unreachable.
    pub in_flight: u64,
    /// Dead-lettered tasks. Counted against the tenant because an unbounded
    /// DLQ is a quiet disk-full outage; the cap forces inspection and
    /// re-drive before more work is accepted.
    pub dlq: u64,
}

/// One pool's autoscaling signals (R0.6 wave 3), as read by
/// `GET /tasks/metrics`: the numbers an external autoscaler (HPA, KEDA, a
/// cron-and-kubectl script) scales the pool's workers on. Rusty publishes
/// the signals; the scaling decision stays with the operator — there is no
/// built-in autoscaler, by design.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PoolStat {
    /// The pool these signals describe.
    pub pool: String,
    /// Backlog depth: tasks waiting for a worker (`queued`, plus `failed`
    /// with a retry scheduled — due or not; the autoscaler must drain all
    /// of it).
    pub queue_depth: u64,
    /// Live leases: `leased` tasks whose visibility timeout has not
    /// expired. This is the numerator of the pool's lease saturation — and
    /// the same count the claim path enforces the pool's concurrency limit
    /// against, so the signal and the mechanism can never disagree. An
    /// expired-but-unreclaimed lease is excluded: it is visible to claimers
    /// again, hence part of the backlog, not of in-flight capacity.
    pub leased: u64,
    /// Creation time of the oldest task a claim right now would hand out
    /// (`queued`, backoff-elapsed `failed`, or lease-expired). `None` when
    /// nothing is visible. Reported on the wire as an age in milliseconds
    /// against the response's `now`, so a consumer needs no clock-skew
    /// correction of its own.
    pub oldest_visible_at: Option<DateTime<Utc>>,
}

/// A `[0, 1)` jitter sample for [`classify_retry_with_policy`], from OS
/// entropy via the
/// already-linked `uuid` crate. The design's seeded-`RngSource` determinism
/// story applies to runs; queue-side retry scheduling just needs the
/// decorrelation full jitter provides.
fn uniform() -> f64 {
    let bytes = uuid::Uuid::new_v4().into_bytes();
    let roll = u64::from_le_bytes(bytes[..8].try_into().expect("uuid is 16 bytes"));
    // Top 53 bits, as f64 / 2^53 — the standard uniform-double recipe.
    (roll >> 11) as f64 / (1u64 << 53) as f64
}

/// Parse a wire string into one of core's closed taxonomies (`T` =
/// [`ErrorClass`] / [`Effect`]); `Err` for anything outside it (routes
/// answer 400 — a silently accepted free-form value would fork the contract
/// the worker SDK matches on). `expected` lists the valid wire names for the
/// error message.
fn parse_taxonomy<T: serde::de::DeserializeOwned>(
    what: &str,
    raw: &str,
    expected: &str,
) -> Result<T, String> {
    serde_json::from_value(Value::String(raw.to_string()))
        .map_err(|_| format!("unknown `{what}` `{raw}` (expected {expected})"))
}

/// The storage spelling of a taxonomy value (its serde name) — the Postgres
/// columns are TEXT, so binds go through this. Only the `postgres` feature
/// consumes it (and the shared unit tests).
#[cfg_attr(not(feature = "postgres"), allow(dead_code))]
fn taxonomy_name<T: Serialize>(value: T) -> String {
    match serde_json::to_value(value).expect("taxonomy serialization is infallible") {
        Value::String(name) => name,
        _ => unreachable!("taxonomy enums serialize to strings"),
    }
}

/// Parse a wire `error_class` string into the shared [`ErrorClass`]
/// taxonomy (routes map `Err` to 400).
pub(crate) fn parse_error_class(raw: &str) -> Result<ErrorClass, String> {
    parse_taxonomy(
        "error_class",
        raw,
        "transient|rate_limited|timeout|invalid_input|dependency_failure|resource_exhausted|cancelled|unknown",
    )
}

/// The storage spelling of an [`ErrorClass`] (its serde name).
#[cfg_attr(not(feature = "postgres"), allow(dead_code))]
pub(crate) fn error_class_name(class: ErrorClass) -> String {
    taxonomy_name(class)
}

/// Parse a wire `effect` string into the shared [`Effect`] taxonomy
/// (routes map `Err` to 400).
pub(crate) fn parse_effect(raw: &str) -> Result<Effect, String> {
    parse_taxonomy(
        "effect",
        raw,
        "pure|read_only|idempotent|compensatable|non_idempotent",
    )
}

/// The storage spelling of an [`Effect`] (its serde name).
#[cfg_attr(not(feature = "postgres"), allow(dead_code))]
pub(crate) fn effect_name(effect: Effect) -> String {
    taxonomy_name(effect)
}

// --------------------------------------------------------------------- //
// Validation (routes map `Err` to 400)
// --------------------------------------------------------------------- //

/// A short label: task `kind`, `worker_id`. Non-empty, bounded; content is
/// otherwise free-form (it is stored, never pathed). `error_class` is NOT a
/// label — it validates against the shared taxonomy via
/// [`parse_error_class`].
pub(crate) fn validate_label(what: &str, value: &str, max_len: usize) -> Result<(), String> {
    if value.trim().is_empty() || value.len() > max_len {
        return Err(format!("`{what}` must be non-empty and <= {max_len} chars"));
    }
    Ok(())
}

/// A pool name becomes a claim filter and (in the JSON layout) stays inside
/// the task record — restrict it like a KV segment so pooling stays
/// unambiguous across backends.
pub(crate) fn validate_pool(pool: &str) -> Result<(), String> {
    let ok = !pool.is_empty()
        && pool.len() <= 128
        && pool
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    if ok {
        Ok(())
    } else {
        Err("`pool` must match [A-Za-z0-9._-] and be 1..=128 chars".to_string())
    }
}

/// `lease_ms` bounds, shared by claim and heartbeat.
pub(crate) fn validate_lease_ms(lease_ms: u64) -> Result<(), String> {
    if (MIN_LEASE_MS..=MAX_LEASE_MS).contains(&lease_ms) {
        Ok(())
    } else {
        Err(format!(
            "`lease_ms` must be within {MIN_LEASE_MS}..={MAX_LEASE_MS}"
        ))
    }
}

// --------------------------------------------------------------------- //
// JSON-file persistence (`{store_path}/tasks/{task_id}.json`)
// --------------------------------------------------------------------- //

/// The tasks directory under the store root. `tasks` is a reserved layout
/// name (see [`crate::RESERVED_NAMES`]): client-chosen thread ids may not
/// claim it.
pub(crate) fn dir(root: &Path) -> PathBuf {
    root.join("tasks")
}

/// Persist one task record (create or overwrite), atomically: temp file +
/// rename, mirroring the journal store's durability discipline — a crash
/// mid-write must never leave a truncated task file behind.
pub(crate) async fn persist(root: &Path, record: &TaskRecord) -> io::Result<()> {
    let dir = dir(root);
    tokio::fs::create_dir_all(&dir).await?;
    let bytes = serde_json::to_vec_pretty(record)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let path = dir.join(format!("{}.json", record.task_id));
    let tmp = dir.join(format!("{}.tmp", record.task_id));
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, path).await
}

/// Load all persisted tasks, skipping (with a warning) any file that fails
/// to parse — one corrupt record must not take the queue down at boot.
pub(crate) fn load(root: &Path) -> HashMap<String, TaskRecord> {
    let mut out = HashMap::new();
    let Ok(entries) = std::fs::read_dir(dir(root)) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let parsed = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<TaskRecord>(&raw).ok());
        match parsed {
            Some(record) => {
                out.insert(record.task_id.clone(), record);
            }
            None => {
                tracing::warn!(path = %path.display(), "skipping unreadable task file")
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> TaskRecord {
        TaskRecord::new(
            NewTask {
                task_id: "task-1".to_string(),
                tenant: "acme".to_string(),
                kind: "send_email".to_string(),
                payload: json!({"to": "a@b.c"}),
                pool: DEFAULT_POOL.to_string(),
                recipient: None,
                max_attempts: DEFAULT_MAX_ATTEMPTS,
                idempotency_key: None,
                effect: None,
                run_id: None,
                thread_id: None,
                deadline: None,
                worker_version: None,
                parent: None,
            },
            Utc::now(),
        )
    }

    /// The retry parameters the fail path resolved implicitly before R0.10
    /// wave 4 — every test in this module exercises pre-promotion floor
    /// behavior.
    fn floor() -> ResolvedRetryParameters {
        ResolvedRetryParameters::floor(DEFAULT_MAX_ATTEMPTS)
    }

    #[test]
    fn status_wire_spellings_round_trip() {
        for (status, s) in [
            (TaskStatus::Queued, "queued"),
            (TaskStatus::Leased, "leased"),
            (TaskStatus::Failed, "failed"),
            (TaskStatus::Completed, "completed"),
            (TaskStatus::Dead, "dead"),
            (TaskStatus::Cancelled, "cancelled"),
        ] {
            assert_eq!(status.as_str(), s);
            assert_eq!(TaskStatus::parse(s), Some(status));
            assert_eq!(serde_json::to_value(status).unwrap(), json!(s));
            assert_eq!(
                serde_json::from_value::<TaskStatus>(json!(s)).unwrap(),
                status
            );
        }
        assert_eq!(TaskStatus::parse("zombie"), None);
    }

    #[test]
    fn fail_with_retryable_false_is_terminal_but_not_dead_lettered() {
        let mut task = record();
        let t0 = Utc::now();
        task.claim("w-1", 60_000, t0);
        // The worker declares re-driving unsafe → RetryDecision::Fail via
        // the effect gate: terminal, next_attempt_at null, and NOT the DLQ.
        task.fail(
            ErrorClass::Timeout,
            "charged twice maybe",
            false,
            SettlementCost::NONE,
            &floor(),
            t0,
        );
        assert_eq!(task.status, TaskStatus::Failed);
        assert_eq!(task.next_attempt_at, None);
        assert!(!task.claimable_at(t0 + Duration::days(365)));
        assert_eq!(task.error_class, Some(ErrorClass::Timeout));
    }

    #[test]
    fn fail_with_non_retryable_class_is_terminal_despite_retryable_flag() {
        let mut task = record();
        let t0 = Utc::now();
        task.claim("w-1", 60_000, t0);
        // Class gate: invalid_input fails immediately even when the worker
        // answered retryable — the class taxonomy wins.
        task.fail(
            ErrorClass::InvalidInput,
            "bad schema",
            true,
            SettlementCost::NONE,
            &floor(),
            t0,
        );
        assert_eq!(task.status, TaskStatus::Failed);
        assert_eq!(task.next_attempt_at, None);
    }

    #[test]
    fn fail_retryable_within_budget_requeues_inside_the_jitter_window() {
        for attempt in 1..=8u32 {
            for _ in 0..64 {
                let mut task = record();
                task.max_attempts = MAX_ATTEMPTS_LIMIT;
                let t0 = Utc::now();
                task.attempt = attempt - 1;
                task.claim("w-1", 60_000, t0);
                task.fail(
                    ErrorClass::Transient,
                    "hiccup",
                    true,
                    SettlementCost::NONE,
                    &ResolvedRetryParameters::floor(MAX_ATTEMPTS_LIMIT),
                    t0,
                );
                assert_eq!(task.status, TaskStatus::Failed);
                let at = task.next_attempt_at.expect("retry scheduled");
                // Full jitter: delay in [0, base * 2^(attempt-1)], 5 min cap.
                let bound = (1_000u64 << (attempt - 1)).min(300_000) as i64;
                let delay = (at - t0).num_milliseconds();
                assert!(
                    (0..=bound).contains(&delay),
                    "delay {delay} ms outside [0, {bound}] for attempt {attempt}"
                );
            }
        }
    }

    #[test]
    fn fail_at_the_attempt_ceiling_dead_letters() {
        let mut task = record(); // DEFAULT_MAX_ATTEMPTS = 3
        let t0 = Utc::now();
        task.claim("w-1", 60_000, t0);
        task.attempt = 3;
        task.fail(
            ErrorClass::Unknown,
            "third strike",
            true,
            SettlementCost::NONE,
            &floor(),
            t0,
        );
        assert_eq!(task.status, TaskStatus::Dead);
        assert_eq!(task.next_attempt_at, None);
        assert!(!task.claimable_at(t0 + Duration::days(365)));
    }

    #[test]
    fn declared_effect_outranks_the_workers_retryable_flag() {
        // A declared non-repeatable effect never silently retries — even
        // when the worker answered retryable on this attempt.
        let mut task = record();
        task.effect = Some(Effect::NonIdempotent);
        let t0 = Utc::now();
        task.claim("w-1", 60_000, t0);
        task.fail(
            ErrorClass::Timeout,
            "maybe it fired",
            true,
            SettlementCost::NONE,
            &floor(),
            t0,
        );
        assert_eq!(task.status, TaskStatus::Failed);
        assert_eq!(task.next_attempt_at, None, "effect gate: fail outright");

        // A declared idempotent effect retries even when the worker
        // answered non-retryable — the enqueue-time declaration is the
        // stronger, shared contract.
        let mut task = record();
        task.effect = Some(Effect::Idempotent);
        task.claim("w-1", 60_000, t0);
        task.fail(
            ErrorClass::Transient,
            "hiccup",
            false,
            SettlementCost::NONE,
            &floor(),
            t0,
        );
        assert_eq!(task.status, TaskStatus::Failed);
        assert!(
            task.next_attempt_at.is_some(),
            "declared idempotent retries"
        );
    }

    #[test]
    fn error_class_names_round_trip_through_the_serde_contract() {
        for (class, name) in [
            (ErrorClass::Transient, "transient"),
            (ErrorClass::RateLimited, "rate_limited"),
            (ErrorClass::Timeout, "timeout"),
            (ErrorClass::InvalidInput, "invalid_input"),
            (ErrorClass::DependencyFailure, "dependency_failure"),
            (ErrorClass::ResourceExhausted, "resource_exhausted"),
            (ErrorClass::Cancelled, "cancelled"),
            (ErrorClass::Unknown, "unknown"),
        ] {
            assert_eq!(error_class_name(class), name);
            assert_eq!(parse_error_class(name).unwrap(), class);
        }
        assert!(parse_error_class("bug").is_err());
        assert!(parse_error_class("").is_err());
        assert!(
            parse_error_class("Transient").is_err(),
            "wire names are snake_case"
        );
    }

    #[test]
    fn effect_names_round_trip_through_the_serde_contract() {
        for (effect, name) in [
            (Effect::Pure, "pure"),
            (Effect::ReadOnly, "read_only"),
            (Effect::Idempotent, "idempotent"),
            (Effect::Compensatable, "compensatable"),
            (Effect::NonIdempotent, "non_idempotent"),
        ] {
            assert_eq!(effect_name(effect), name);
            assert_eq!(parse_effect(name).unwrap(), effect);
        }
        assert!(parse_effect("side_effecty").is_err());
        assert!(
            parse_effect("Idempotent").is_err(),
            "wire names are snake_case"
        );
    }

    #[test]
    fn uniform_stays_inside_unit_interval() {
        for _ in 0..256 {
            let u = uniform();
            assert!((0.0..1.0).contains(&u), "uniform sample {u} outside [0, 1)");
        }
    }

    #[test]
    fn lifecycle_claim_heartbeat_complete() {
        let mut task = record();
        let t0 = Utc::now();
        assert!(task.claimable_at(t0));
        assert!(!task.leased_to("w-1"));

        task.claim("w-1", 60_000, t0);
        assert_eq!(task.status, TaskStatus::Leased);
        assert_eq!(task.attempt, 1);
        assert!(task.leased_to("w-1"));
        assert!(!task.leased_to("w-2"));
        // A live lease is not claimable; an expired one is.
        assert!(!task.claimable_at(t0 + Duration::seconds(30)));
        assert!(task.claimable_at(t0 + Duration::seconds(61)));

        task.renew_lease(60_000, t0);
        let expires = task.lease.as_ref().unwrap().expires_at;
        assert_eq!(expires, t0 + Duration::seconds(60));

        task.complete(json!({"ok": true}), None, SettlementCost::NONE, t0);
        assert_eq!(task.status, TaskStatus::Completed);
        assert_eq!(task.result, Some(json!({"ok": true})));
        assert!(task.lease.is_none());
        assert!(!task.claimable_at(t0 + Duration::days(365)));
    }

    #[test]
    fn lifecycle_failed_attempt_schedules_retry_then_dead_letters() {
        let mut task = record();
        let t0 = Utc::now();
        task.claim("w-1", 60_000, t0);
        task.fail(
            ErrorClass::Timeout,
            "upstream timed out",
            true,
            SettlementCost::NONE,
            &floor(),
            t0,
        );
        assert_eq!(task.status, TaskStatus::Failed);
        assert_eq!(task.error_class, Some(ErrorClass::Timeout));
        assert_eq!(task.last_error.as_deref(), Some("upstream timed out"));
        assert!(task.lease.is_none());
        let at = task.next_attempt_at.expect("retry scheduled");
        assert!(!task.claimable_at(t0));
        assert!(task.claimable_at(at));

        // Retry the remaining attempts; the third failure dead-letters.
        task.claim("w-1", 60_000, at);
        assert_eq!(task.attempt, 2);
        assert!(task.next_attempt_at.is_none(), "claim clears the schedule");
        task.fail(
            ErrorClass::Timeout,
            "again",
            true,
            SettlementCost::NONE,
            &floor(),
            at,
        );
        assert_eq!(task.status, TaskStatus::Failed);
        let at = task.next_attempt_at.unwrap();
        task.claim("w-2", 60_000, at);
        assert_eq!(task.attempt, 3);
        task.fail(
            ErrorClass::Timeout,
            "third strike",
            true,
            SettlementCost::NONE,
            &floor(),
            at,
        );
        assert_eq!(task.status, TaskStatus::Dead);
        assert!(task.next_attempt_at.is_none());
        assert!(!task.claimable_at(at + Duration::days(365)));
    }

    #[test]
    fn expired_lease_reclaim_counts_a_new_attempt() {
        let mut task = record();
        let t0 = Utc::now();
        task.claim("w-1", 1_000, t0);
        let t1 = t0 + Duration::seconds(2);
        assert!(task.claimable_at(t1));
        // Expired but unreclaimed: the original holder may still settle —
        // the owner check is atomic with every mutation, so it can never
        // settle twice once another worker reclaims.
        assert!(task.leased_to("w-1"));
        task.claim("w-2", 1_000, t1);
        assert_eq!(task.attempt, 2);
        assert!(task.leased_to("w-2"));
        assert!(!task.leased_to("w-1"), "the lost lease no longer settles");
    }

    #[test]
    fn cancel_queued_task_is_terminal_immediately() {
        let mut task = record();
        let t0 = Utc::now();
        assert_eq!(task.cancel(t0), Some(CancelTransition::Cancelled));
        assert_eq!(task.status, TaskStatus::Cancelled);
        assert_eq!(task.error_class, Some(ErrorClass::Cancelled));
        assert!(task.next_attempt_at.is_none());
        assert!(task.lease.is_none());
        assert!(!task.cancel_requested, "no holder to signal");
        assert!(task.is_terminal());
        assert!(!task.claimable_at(t0 + Duration::days(365)));
        // A second cancel changes nothing — terminal is terminal.
        assert_eq!(task.cancel(t0), None);
    }

    #[test]
    fn cancel_retry_scheduled_task_is_terminal_immediately() {
        let mut task = record();
        let t0 = Utc::now();
        task.claim("w-1", 60_000, t0);
        task.fail(
            ErrorClass::Transient,
            "hiccup",
            true,
            SettlementCost::NONE,
            &floor(),
            t0,
        );
        assert_eq!(task.status, TaskStatus::Failed);
        assert!(task.next_attempt_at.is_some(), "a retry is outstanding");
        // Non-terminal while a retry is scheduled: cancellation must
        // prevent the re-queue.
        assert!(!task.is_terminal());
        assert_eq!(task.cancel(t0), Some(CancelTransition::Cancelled));
        assert_eq!(task.status, TaskStatus::Cancelled);
        assert!(task.next_attempt_at.is_none());
    }

    #[test]
    fn cancel_leased_task_signals_the_holder_and_keeps_its_lease() {
        let mut task = record();
        let t0 = Utc::now();
        task.claim("w-1", 60_000, t0);
        assert_eq!(task.cancel(t0), Some(CancelTransition::Signalled));
        // The lease is untouched: the holder's heartbeat renews (carrying
        // the hint) and its fail report still passes the owner check.
        assert_eq!(task.status, TaskStatus::Leased);
        assert!(task.cancel_requested);
        assert!(task.leased_to("w-1"));
        // Re-cancelling while still leased is an idempotent re-signal.
        assert_eq!(task.cancel(t0), Some(CancelTransition::Signalled));

        // The prompt path: the holder reports the aborted attempt as
        // cancelled and the record ends terminal-cancelled — never the
        // DLQ, never re-queued.
        task.fail(
            ErrorClass::Cancelled,
            "cancelled by control plane",
            false,
            SettlementCost::NONE,
            &floor(),
            t0,
        );
        assert_eq!(task.status, TaskStatus::Cancelled);
        assert_eq!(task.error_class, Some(ErrorClass::Cancelled));
        assert!(!task.claimable_at(t0 + Duration::days(365)));
    }

    #[test]
    fn cancel_terminal_task_is_refused() {
        // Completed, dead, cancelled, and failed-outright (no retry
        // scheduled) are all terminal: cancel changes nothing.
        let t0 = Utc::now();
        let mut completed = record();
        completed.claim("w-1", 60_000, t0);
        completed.complete(json!({"ok": true}), None, SettlementCost::NONE, t0);

        let mut dead = record();
        dead.claim("w-1", 60_000, t0);
        dead.attempt = dead.max_attempts;
        dead.fail(
            ErrorClass::Unknown,
            "third strike",
            true,
            SettlementCost::NONE,
            &floor(),
            t0,
        );

        let mut failed_outright = record();
        failed_outright.claim("w-1", 60_000, t0);
        failed_outright.fail(
            ErrorClass::InvalidInput,
            "bad schema",
            true,
            SettlementCost::NONE,
            &floor(),
            t0,
        );

        let mut cancelled = record();
        cancelled.cancel(t0);

        for task in [
            &mut completed,
            &mut dead,
            &mut failed_outright,
            &mut cancelled,
        ] {
            let before = task.status;
            assert!(task.is_terminal(), "{before:?} is terminal");
            assert_eq!(task.cancel(t0), None, "{before:?} accepted a cancel");
            assert_eq!(task.status, before, "cancel changed a terminal task");
        }
    }

    #[test]
    fn fail_with_cancelled_class_lands_cancelled_not_failed_or_dead() {
        let mut task = record();
        let t0 = Utc::now();
        task.claim("w-1", 60_000, t0);
        // Even with the worker answering retryable, the class gate fails
        // outright — and the cancelled class spells the terminal state
        // `cancelled` rather than the failure one.
        task.fail(
            ErrorClass::Cancelled,
            "interrupted",
            true,
            SettlementCost::NONE,
            &floor(),
            t0,
        );
        assert_eq!(task.status, TaskStatus::Cancelled);
        assert!(task.next_attempt_at.is_none());
        assert!(!task.claimable_at(t0 + Duration::days(365)));
    }

    #[test]
    fn cancellation_due_covers_the_unanswered_and_the_expired() {
        let t0 = Utc::now();

        // Cancel-requested with a live lease: the holder may still answer —
        // not yet due.
        let mut task = record();
        task.claim("w-1", 60_000, t0);
        task.cancel(t0);
        assert!(!task.cancellation_due(t0));
        // Lease lapsed unanswered: due.
        assert!(task.cancellation_due(t0 + Duration::seconds(61)));

        // Deadline passed on a queued task: due.
        let mut task = record();
        task.deadline = Some(t0 - Duration::seconds(1));
        assert!(task.cancellation_due(t0));
        // Deadline in the future: not due (and claimable normally).
        let mut task = record();
        task.deadline = Some(t0 + Duration::seconds(60));
        assert!(!task.cancellation_due(t0));
        // No flags: not due.
        assert!(!record().cancellation_due(t0));
    }

    #[test]
    fn apply_cancellation_clears_everything_that_could_requeue() {
        let mut task = record();
        let t0 = Utc::now();
        task.claim("w-1", 1_000, t0);
        task.cancel(t0);
        task.apply_cancellation(t0 + Duration::seconds(2));
        assert_eq!(task.status, TaskStatus::Cancelled);
        assert!(task.lease.is_none(), "no lease holder survives");
        assert!(task.next_attempt_at.is_none());
        assert!(!task.leased_to("w-1"));
    }

    #[test]
    fn wire_omits_tenant_and_nests_the_lease() {
        let mut task = record();
        task.claim("w-1", 60_000, Utc::now());
        let wire = task.wire();
        assert!(wire.get("tenant").is_none());
        assert_eq!(wire["task_id"], json!("task-1"));
        assert_eq!(wire["kind"], json!("send_email"));
        assert_eq!(wire["status"], json!("leased"));
        assert_eq!(wire["lease"]["owner"], json!("w-1"));
        assert!(wire["lease"]["expires_at"].is_string());
        assert_eq!(wire["idempotency_key"], Value::Null);
        assert_eq!(wire["effect"], Value::Null);
        assert_eq!(wire["run_id"], Value::Null);
        assert_eq!(wire["cancel_requested"], json!(false));
        assert_eq!(wire["deadline"], Value::Null);
        assert_eq!(wire["worker_version"], Value::Null);
        assert_eq!(wire["next_attempt_at"], Value::Null);
        assert_eq!(wire["recipient"], Value::Null);
    }

    #[test]
    fn wire_carries_the_recipient_when_addressed() {
        let mut task = record();
        task.recipient = Some("agent:researcher-7".to_string());
        let wire = task.wire();
        assert_eq!(wire["recipient"], json!("agent:researcher-7"));
    }

    #[test]
    fn record_serde_round_trip() {
        let mut task = record();
        task.idempotency_key = Some("key-1".to_string());
        task.run_id = Some("run-9".to_string());
        task.deadline = DateTime::<Utc>::from_timestamp_millis(1_800_000_000_000);
        task.claim("w-1", 60_000, Utc::now());
        task.fail(
            ErrorClass::Unknown,
            "it broke",
            true,
            SettlementCost::NONE,
            &floor(),
            Utc::now(),
        );
        let raw = serde_json::to_string(&task).unwrap();
        let back: TaskRecord = serde_json::from_str(&raw).unwrap();
        assert_eq!(back.task_id, task.task_id);
        assert_eq!(back.tenant, task.tenant);
        assert_eq!(back.status, TaskStatus::Failed);
        assert_eq!(back.attempt, 1);
        assert_eq!(back.idempotency_key.as_deref(), Some("key-1"));
        assert_eq!(back.error_class, Some(ErrorClass::Unknown));
        // The error class persists in the shared snake_case spelling.
        assert!(raw.contains("\"error_class\":\"unknown\""));
        assert_eq!(back.next_attempt_at, task.next_attempt_at);
        assert_eq!(back.run_id.as_deref(), Some("run-9"));
        assert_eq!(back.deadline, task.deadline);
        assert!(!back.cancel_requested);
        // Records written before the wave-2 cancellation fields existed
        // still load: every additive field carries a serde default.
        let legacy = json!({
            "task_id": "t", "tenant": "default", "kind": "k", "payload": null,
            "pool": "default", "status": "queued", "attempt": 0,
            "max_attempts": 3, "created_at": Utc::now(), "updated_at": Utc::now(),
        });
        let back: TaskRecord = serde_json::from_value(legacy).unwrap();
        assert_eq!(back.status, TaskStatus::Queued);
        assert!(back.lease.is_none() && back.result.is_none() && back.run_id.is_none());
        assert!(back.effect.is_none() && back.error_class.is_none());
        assert!(!back.cancel_requested && back.deadline.is_none());
        assert!(
            back.worker_version.is_none(),
            "wave-3 pin defaults to unpinned"
        );
        assert!(
            back.recipient.is_none(),
            "R0.7 recipient addressing defaults to ordinary pool work"
        );
    }

    #[test]
    fn worker_version_pin_matches_only_the_exact_string() {
        let mut pinned = record();
        pinned.worker_version = Some("activity-worker/1.4.0".to_string());
        assert!(pinned.matches_worker_version(Some("activity-worker/1.4.0")));
        assert!(!pinned.matches_worker_version(Some("activity-worker/1.5.0")));
        assert!(
            !pinned.matches_worker_version(None),
            "a worker advertising no version cannot take a pinned task"
        );
        // Unpinned work is claimable by anyone, versioned or not.
        let unpinned = record();
        assert!(unpinned.matches_worker_version(None));
        assert!(unpinned.matches_worker_version(Some("activity-worker/1.4.0")));
    }

    #[test]
    fn validation_bounds() {
        assert!(validate_label("kind", "send_email", 256).is_ok());
        assert!(validate_label("kind", "  ", 256).is_err());
        assert!(validate_label("kind", &"x".repeat(257), 256).is_err());
        assert!(validate_pool("default").is_ok());
        assert!(validate_pool("gpu-workers.eu-west").is_ok());
        assert!(validate_pool("").is_err());
        assert!(validate_pool("bad/pool").is_err());
        assert!(validate_lease_ms(100).is_ok());
        assert!(validate_lease_ms(3_600_000).is_ok());
        assert!(validate_lease_ms(99).is_err());
        assert!(validate_lease_ms(3_600_001).is_err());
    }

    #[tokio::test]
    async fn persist_then_load_round_trips() {
        let root = std::env::temp_dir().join(format!("rusty-tasks-test-{}", uuid::Uuid::new_v4()));
        let mut task = record();
        task.claim("w-1", 60_000, Utc::now());
        persist(&root, &task).await.unwrap();
        let loaded = load(&root);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded["task-1"].status, TaskStatus::Leased);
        assert!(loaded["task-1"].leased_to("w-1"));

        // Overwrite replaces; a corrupt file is skipped, not fatal.
        task.complete(json!(1), None, SettlementCost::NONE, Utc::now());
        persist(&root, &task).await.unwrap();
        std::fs::write(dir(&root).join("corrupt.json"), b"{nope").unwrap();
        let loaded = load(&root);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded["task-1"].status, TaskStatus::Completed);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn settlement_cost_is_stored_on_complete_and_fail() {
        let usage = Usage {
            prompt_tokens: 120,
            completion_tokens: 30,
            total_tokens: 150,
        };
        let cost = SettlementCost {
            tokens: Some(usage),
            cost_usd: Some(0.0042),
        };
        let mut task = record();
        let t0 = Utc::now();
        task.claim("w-1", 60_000, t0);
        task.complete(json!({"ok": true}), None, cost, t0);
        assert_eq!(task.tokens, Some(usage));
        assert_eq!(task.cost_usd, Some(0.0042));
        // The wire carries the evidence alongside the result.
        assert_eq!(task.wire()["tokens"]["total_tokens"], json!(150));
        assert_eq!(task.wire()["cost_usd"], json!(0.0042));

        // Fail stores it the same way — a race loser's reported waste must
        // survive on the record, not only in a journal.
        let mut task = record();
        task.claim("w-1", 60_000, t0);
        task.fail(ErrorClass::Unknown, "gave up", false, cost, &floor(), t0);
        assert_eq!(task.tokens, Some(usage));
        assert_eq!(task.cost_usd, Some(0.0042));

        // A settle with no reported cost leaves the fields unset — and the
        // wire spells them null, never invented zeros.
        let mut task = record();
        task.claim("w-1", 60_000, t0);
        task.complete(json!(1), None, SettlementCost::NONE, t0);
        assert_eq!(task.tokens, None);
        assert!(task.wire()["tokens"].is_null());
    }

    #[test]
    fn pre_wave3_records_deserialize_with_defaults() {
        // A record file written before wave 3 has no tokens/cost/parent
        // keys at all: additive evolution means it still loads, with the
        // new fields defaulted.
        let legacy = json!({
            "task_id": "task-1",
            "tenant": "acme",
            "kind": "send_email",
            "payload": {"to": "a@b.c"},
            "pool": "default",
            "status": "queued",
            "attempt": 0,
            "max_attempts": 3,
            "cancel_requested": false,
            "created_at": "2027-01-15T08:00:00Z",
            "updated_at": "2027-01-15T08:00:00Z",
        });
        let record: TaskRecord = serde_json::from_value(legacy).unwrap();
        assert_eq!(record.tokens, None);
        assert_eq!(record.cost_usd, None);
        assert_eq!(record.parent, None);
    }
}
