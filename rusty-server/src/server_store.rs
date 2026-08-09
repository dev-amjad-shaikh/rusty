//! Server-side persistence for the platform surface: assistants, crons,
//! threads, and the cross-thread KV store.
//!
//! [`ServerStore`] is the async CRUD trait the routes program against. Two
//! implementations ship:
//!
//! - [`JsonFileStore`] — the default. Existing v0.2 behavior, extracted:
//!   assistants, crons, and threads live in an in-memory index persisted as
//!   one JSON file per record under `{store_path}/{assistants,crons,threads}/`;
//!   KV items are pure file-backed reads/writes under `{store_path}/store/`;
//!   Flight Recorder journals are one file per run under
//!   `{store_path}/journals/`; durable tasks are an in-memory index persisted
//!   as one file per task under `{store_path}/tasks/` (R0.6).
//! - [`PostgresStore`] (feature `postgres`) — tables `server_assistants`,
//!   `server_crons`, `server_threads`, `server_kv`, `server_journals`,
//!   `server_tasks` (the R0.6 durable task queue, column-mapped for
//!   `FOR UPDATE SKIP LOCKED` claiming), `server_outbox` (the R0.6
//!   wave-2b transactional outbox), and `server_triggers` /
//!   `server_trigger_events` (event-driven triggers), auto-migrated on
//!   (lazy) connect. Selected via `ServerConfig::with_postgres(url)`.
//! - Governed memory (R0.8) lives in `{store_path}/memory/` (one JSON
//!   file per record, artifact-referenced bodies spilled to
//!   `{store_path}/memory_artifacts/`) on the file backend and the
//!   column-mapped `server_memory` table on Postgres — see
//!   [`crate::memory`] for the layout and the spill/resolve discipline.
//!
//! All trait errors are plain `String`s; routes map them to 500s — no store
//! error is ever a client error (validation happens before the store call).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusty_agent_runtime::checkpoint::{Checkpoint, Checkpointer, JsonFileCheckpointer};
use rusty_agent_runtime::journal::{FileArtifactStore, JournalSnapshot};
use rusty_agent_runtime::learn::{CandidateRecord, CandidateStatus, VersionPointer};
use rusty_agent_runtime::memory::{apply_query, MemoryQuery, MemoryRecord};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::agents::{
    self, ActivationLease, ActivationMutation, ActivationOutcome, AgentRecord, MailboxClaim,
    MailboxClaimScope,
};
use crate::assistants::{self, AssistantRecord};
use crate::coordination::{self, CoordinationRecord};
use crate::crons::{self, CronRecord};
use crate::journals;
use crate::learn;
use crate::memory;
use crate::outbox::{self, OutboxRecord};
use crate::policy::{self, PolicyActivation, PolicyBinding, PolicyRecord, PolicyWrite};
use crate::store::{self, StoreItem};
use crate::tasks::{self, CancelOutcome, MutationOutcome, RunCancellation, TaskRecord, TaskStatus};
use crate::threads::{self, ThreadRecord};
use crate::triggers::{self, TriggerEventRecord, TriggerRecord};

/// Store operation result. The `String` payload is a 500-class internal
/// failure (IO error, DB error, serialization bug).
pub(crate) type StoreResult<T> = Result<T, String>;

/// Async CRUD backing the assistants / crons / KV routes.
///
/// `create_*` methods are check-and-insert: they return `false` when the id
/// already exists **without writing**, so routes can answer 409. `upsert_*`
/// unconditionally overwrites (used by the cron scheduler's bookkeeping).
/// The KV `kv_put` returns the item plus a `created` flag, preserving
/// `created_at` across overwrites.
#[async_trait::async_trait]
pub(crate) trait ServerStore: Send + Sync {
    /// Insert a new assistant; `false` (no write) when the id exists.
    async fn create_assistant(&self, record: &AssistantRecord) -> StoreResult<bool>;
    /// Fetch one assistant by id.
    async fn get_assistant(&self, assistant_id: &str) -> StoreResult<Option<AssistantRecord>>;
    /// All assistants (order unspecified; routes sort).
    async fn list_assistants(&self) -> StoreResult<Vec<AssistantRecord>>;

    /// Insert a new cron; `false` (no write) when the id exists.
    async fn create_cron(&self, record: &CronRecord) -> StoreResult<bool>;
    /// Overwrite a cron (scheduler bookkeeping: `last_run_at`, `runs_fired`).
    async fn upsert_cron(&self, record: &CronRecord) -> StoreResult<()>;
    /// Fetch one cron by id.
    async fn get_cron(&self, cron_id: &str) -> StoreResult<Option<CronRecord>>;
    /// All crons (order unspecified; routes sort).
    async fn list_crons(&self) -> StoreResult<Vec<CronRecord>>;
    /// Delete a cron; `true` when it existed.
    async fn delete_cron(&self, cron_id: &str) -> StoreResult<bool>;

    // -- Triggers (event-driven webhook bindings) ----------------------- //

    /// Insert a new trigger; `false` (no write) when the id exists.
    async fn create_trigger(&self, record: &TriggerRecord) -> StoreResult<bool>;
    /// Overwrite a trigger (updates, bookkeeping counters).
    async fn upsert_trigger(&self, record: &TriggerRecord) -> StoreResult<()>;
    /// Fetch one trigger by (internal, tenant-scoped) id.
    async fn get_trigger(&self, trigger_id: &str) -> StoreResult<Option<TriggerRecord>>;
    /// All triggers across all tenants (routes filter; the webhook resolver
    /// scans for the external id and lets the HMAC signature decide).
    async fn list_triggers(&self) -> StoreResult<Vec<TriggerRecord>>;
    /// Delete a trigger and its whole event log; `true` when it existed.
    async fn delete_trigger(&self, trigger_id: &str) -> StoreResult<bool>;
    /// Append an event to a trigger's log, or overwrite it on a status
    /// transition (`pending` → `coalesced`/`failed`). Upserts on event id
    /// and prunes the oldest entries past
    /// [`crate::triggers::MAX_EVENTS_PER_TRIGGER`].
    async fn append_trigger_event(&self, record: &TriggerEventRecord) -> StoreResult<()>;
    /// Fetch one event of one trigger.
    async fn get_trigger_event(
        &self,
        trigger_id: &str,
        event_id: &str,
    ) -> StoreResult<Option<TriggerEventRecord>>;
    /// A trigger's event log, oldest first.
    async fn list_trigger_events(&self, trigger_id: &str) -> StoreResult<Vec<TriggerEventRecord>>;

    /// Insert a new thread under its internal (tenant-scoped) id; `false`
    /// (no write) when the id exists. Thread records are durable so
    /// pre-restart checkpoints stay reachable through the API.
    async fn create_thread(&self, internal_id: &str, record: &ThreadRecord) -> StoreResult<bool>;
    /// Fetch one thread by internal (tenant-scoped) id.
    async fn get_thread(&self, internal_id: &str) -> StoreResult<Option<ThreadRecord>>;

    /// Insert or replace a KV item. Returns the stored item plus `true`
    /// when the key was newly created (`created_at` preserved on replace).
    async fn kv_put(
        &self,
        namespace: &str,
        key: &str,
        value: Value,
    ) -> StoreResult<(StoreItem, bool)>;
    /// Fetch one KV item (`None` when absent).
    async fn kv_get(&self, namespace: &str, key: &str) -> StoreResult<Option<StoreItem>>;
    /// Delete one KV item; `true` when it existed.
    async fn kv_delete(&self, namespace: &str, key: &str) -> StoreResult<bool>;
    /// All items in one namespace, sorted by key (empty for unknown
    /// namespaces).
    async fn kv_list(&self, namespace: &str) -> StoreResult<Vec<StoreItem>>;

    /// Persist a run's Flight Recorder journal snapshot, replacing any
    /// earlier snapshot of the same run (the journal grows at every
    /// checkpoint boundary; the final write lands at run completion).
    async fn put_journal(&self, snapshot: &JournalSnapshot) -> StoreResult<()>;
    /// Fetch the journal snapshot stored for `run_id` (`None` when none was
    /// persisted — e.g. a queued run, or one that failed before its first
    /// checkpoint boundary).
    async fn get_journal(&self, run_id: &str) -> StoreResult<Option<JournalSnapshot>>;

    // -- Durable task queue (R0.6) -------------------------------------- //

    /// Enqueue a task. With an idempotency key, a live task already carrying
    /// that key (same tenant) is returned unchanged with `deduplicated:
    /// true` — enqueue is safe to retry. Without a key the insert always
    /// creates (`false`).
    async fn enqueue_task(&self, record: &TaskRecord) -> StoreResult<(TaskRecord, bool)>;
    /// Atomically claim the oldest claimable task in `scope.pools` for
    /// `worker_id` (tenant-scoped): queued tasks, backoff-elapsed failed
    /// tasks, and leased tasks past their visibility timeout. `None` when
    /// nothing is claimable (route answers 204).
    ///
    /// Wave-3 placement rules, both applied before a candidate is chosen:
    /// `scope.pool_limits` caps each pool's live (unexpired) leases — a
    /// pool at its cap hands out nothing, so pools coexist without
    /// starving each other; `scope.worker_version` is the version the
    /// worker advertises, matched exactly against a task's version pin
    /// ([`TaskRecord::worker_version`]) — unpinned tasks match any worker.
    ///
    /// On the JSON-file backend both rules hold exactly (the pick runs under
    /// the one index lock). On Postgres the pool-capacity count and the
    /// claim run in one transaction, but concurrent claim transactions do
    /// not serialize against each other: claims racing inside the same
    /// commit window can transiently overshoot a pool's cap by up to the
    /// number of racing claimers. The cap is a scheduling guardrail keeping
    /// pools from starving each other, not a hard invariant — overshoot
    /// self-corrects on the next claim round, and no task is ever leased
    /// twice (the row lock, which *is* exact, guarantees that).
    async fn claim_task(
        &self,
        tenant: &str,
        worker_id: &str,
        scope: &tasks::ClaimScope<'_>,
        lease_ms: u64,
        now: chrono::DateTime<chrono::Utc>,
    ) -> StoreResult<Option<TaskRecord>>;
    /// Extend the lease held by `worker_id` (heartbeat).
    async fn heartbeat_task(
        &self,
        tenant: &str,
        task_id: &str,
        worker_id: &str,
        lease_ms: u64,
        now: chrono::DateTime<chrono::Utc>,
    ) -> StoreResult<MutationOutcome>;
    /// Settle the task held by `worker_id` as completed, storing the
    /// worker's [`tasks::CompletionReport`]: result payload, effect receipt,
    /// and settlement cost evidence (see [`crate::tasks::TaskRecord::receipt`]
    /// and [`crate::tasks::TaskRecord::tokens`]).
    async fn complete_task(
        &self,
        tenant: &str,
        task_id: &str,
        worker_id: &str,
        report: tasks::CompletionReport,
        now: chrono::DateTime<chrono::Utc>,
    ) -> StoreResult<MutationOutcome>;
    /// Record a failed attempt on the task held by `worker_id`: requeue with
    /// backoff, dead-letter, or fail outright — decided by core's shared
    /// [`classify_retry`](rusty_agent_runtime::durable::classify_retry)
    /// policy inside [`crate::tasks::TaskRecord::fail`].
    async fn fail_task(
        &self,
        tenant: &str,
        task_id: &str,
        worker_id: &str,
        report: tasks::FailureReport,
        now: chrono::DateTime<chrono::Utc>,
    ) -> StoreResult<MutationOutcome>;
    /// Fetch one task, tenant-scoped (`None` for unknown or cross-tenant
    /// ids — the two are indistinguishable by design).
    async fn get_task(&self, tenant: &str, task_id: &str) -> StoreResult<Option<TaskRecord>>;
    /// List a tenant's tasks, optionally filtered to one status (the DLQ
    /// listing is `status == dead`), oldest first.
    async fn list_tasks(
        &self,
        tenant: &str,
        status: Option<TaskStatus>,
    ) -> StoreResult<Vec<TaskRecord>>;
    /// Cancel a non-terminal task (control-plane operation, not
    /// lease-guarded): queued and retry-scheduled tasks move to the
    /// terminal `cancelled` state immediately; a leased task keeps its
    /// lease with `cancel_requested` set, so the holder learns on its next
    /// heartbeat. Terminal tasks answer [`CancelOutcome::Terminal`].
    async fn cancel_task(
        &self,
        tenant: &str,
        task_id: &str,
        now: chrono::DateTime<chrono::Utc>,
    ) -> StoreResult<CancelOutcome>;
    /// Cancel every non-terminal task of one run (tenant-scoped), the
    /// run-level propagation of `POST /runs/{run_id}/cancel`. Applies the
    /// same two transitions as [`ServerStore::cancel_task`] and returns
    /// both sets so the route can report what was finalized versus
    /// signalled.
    async fn cancel_run_tasks(
        &self,
        tenant: &str,
        run_id: &str,
        now: chrono::DateTime<chrono::Utc>,
    ) -> StoreResult<RunCancellation>;

    /// Cancel every non-terminal task addressed to one agent's mailbox
    /// (`recipient` = `agent:{agent_id}`, R0.7 wave 2) — the agent-scoped
    /// form of [`ServerStore::cancel_run_tasks`], and the cancellation
    /// tree's "children before parent" step: cancelling an agent cancels
    /// its outstanding mailbox traffic first, so a cancelled agent (or
    /// team) never leaves an orphan task that would re-activate it.
    /// Applies the same two transitions as [`ServerStore::cancel_task`]:
    /// queued and retry-scheduled messages go terminal-`cancelled`
    /// immediately; a leased turn keeps its lease with `cancel_requested`
    /// set for the holder to learn on its next heartbeat.
    async fn cancel_agent_tasks(
        &self,
        tenant: &str,
        recipient: &str,
        now: chrono::DateTime<chrono::Utc>,
    ) -> StoreResult<RunCancellation>;

    /// Insert a task directly into the terminal `dead` state (R0.7 wave 2)
    /// — the runtime's own dead-letter write path, used by supervision's
    /// root escalation: the notice must land in the DLQ with its evidence
    /// attached even though no attempt ever ran (today only
    /// `classify_retry`'s `RetryDecision::Dead` writes `dead`, and only
    /// after a failed attempt). Dedupes on the record's idempotency key
    /// exactly like [`ServerStore::enqueue_task`], so a retried escalation
    /// cannot double the DLQ entry.
    async fn dead_letter_task(&self, record: &TaskRecord) -> StoreResult<(TaskRecord, bool)>;

    /// The tenant's queue pressure (R0.6 wave 3) — the three gauges the
    /// submission quota gate enforces. Read-only; the gauge definitions
    /// (including why pending outbox rows count as queued) live on
    /// [`crate::tasks::TaskUsage`].
    async fn task_usage(&self, tenant: &str) -> StoreResult<tasks::TaskUsage>;

    /// Per-pool autoscaling signals (R0.6 wave 3) for
    /// `GET /tasks/metrics`, computed at `now`. Pools with no tasks are
    /// absent — the route adds configured-but-empty pools itself, since
    /// only the config knows their limits.
    async fn task_pool_stats(
        &self,
        tenant: &str,
        now: chrono::DateTime<chrono::Utc>,
    ) -> StoreResult<Vec<tasks::PoolStat>>;

    // -- Transactional outbox (R0.6 wave 2b) ---------------------------- //

    /// Write a task into the transactional outbox instead of the queue. The
    /// row is pending until the relay publishes it via
    /// [`ServerStore::outbox_publish_pending`]; use this when the submission
    /// must commit atomically with a state change (see
    /// [`ServerStore::checkpoint_and_enqueue`]) rather than become claimable
    /// immediately. Re-writing the same task id returns the pending row
    /// unchanged with `deduplicated: true` — outbox writes are safe to
    /// retry.
    async fn outbox_enqueue(&self, record: &TaskRecord) -> StoreResult<(TaskRecord, bool)>;
    /// Publish up to `limit` pending outbox rows into the task queue,
    /// oldest first, returning the tasks the publish made visible. Each
    /// row's publish — the queue insert and the mark-published — is atomic
    /// per row, and the queue insert dedupes on the task's idempotency key
    /// (the same mechanism as [`ServerStore::enqueue_task`]), so a caller
    /// that dies mid-publish and retries can neither lose nor double a row;
    /// a deduped publish resolves to the pre-existing task carrying the
    /// key. This is the relay's unit of work; it is also the crash-recovery
    /// path: rows pending at startup publish on the first call.
    async fn outbox_publish_pending(
        &self,
        limit: usize,
        now: chrono::DateTime<chrono::Utc>,
    ) -> StoreResult<Vec<TaskRecord>>;
    /// Write a checkpoint and submit tasks in one atomic unit: on Postgres,
    /// one transaction — a crash between the two writes is impossible, so a
    /// run can never land "state saved, task lost" or the reverse. On the
    /// JSON-file backend (which cannot transact across files) the outbox
    /// rows are written first and the checkpoint second: a crash may leave
    /// tasks whose checkpoint never landed, but never a checkpoint whose
    /// tasks are silently gone. Cross-record atomicity is Postgres-only.
    async fn checkpoint_and_enqueue(
        &self,
        checkpoint: &Checkpoint,
        tasks: &[TaskRecord],
    ) -> StoreResult<()>;

    // -- Agent registry (R0.7 Agent Fabric, wave 1) --------------------- //

    /// Insert a new agent registration; `false` (no write) when the id
    /// exists. Agent ids are tenant-scoped in the id itself (the
    /// assistants/crons convention), so there is no separate tenant
    /// argument.
    async fn create_agent(&self, record: &AgentRecord) -> StoreResult<bool>;
    /// Overwrite an existing agent registration (R0.7 wave 2 — the
    /// supervision state mutates in place); `false` (no write) when the id
    /// does not exist. Last-writer-wins: safe because supervision triggers
    /// for one agent are serialized by the turn protocol (only the turn's
    /// lease holder can settle it, and the latches make escalation and
    /// breach handling exactly-once — see `crate::supervision`).
    async fn update_agent(&self, record: &AgentRecord) -> StoreResult<bool>;
    /// Fetch one agent by its tenant-scoped id.
    async fn get_agent(&self, agent_id: &str) -> StoreResult<Option<AgentRecord>>;
    /// All registered agents (order unspecified; routes sort and filter by
    /// tenant).
    async fn list_agents(&self) -> StoreResult<Vec<AgentRecord>>;

    // -- Coordination patterns (R0.7 wave 3) ---------------------------- //

    /// Insert a new coordination record; `false` (no write) when the id
    /// exists. Coordination ids are tenant-scoped in the id itself (the
    /// agents convention), so there is no separate tenant argument.
    async fn create_coordination(&self, record: &CoordinationRecord) -> StoreResult<bool>;
    /// Overwrite an existing coordination record (the drive's settlement
    /// latches mutate it in place); `false` (no write) when the id does
    /// not exist.
    async fn update_coordination(&self, record: &CoordinationRecord) -> StoreResult<bool>;
    /// Fetch one coordination by its tenant-scoped id.
    async fn get_coordination(
        &self,
        coordination_id: &str,
    ) -> StoreResult<Option<CoordinationRecord>>;
    /// Persist a journal snapshot and submit tasks through the
    /// transactional outbox as one unit (R0.7 wave 3 — the coordination
    /// drive's commit point): on Postgres, one transaction — the evidence
    /// and the work commit together or not at all. On the JSON-file
    /// backend the outbox rows land first and the journal second, the
    /// ordering [`ServerStore::checkpoint_and_enqueue`] documents: a crash
    /// may leave tasks whose journal events never landed (visible, and
    /// re-journaled by the next drive), but never a journal claiming
    /// submissions that do not exist.
    async fn journal_and_enqueue(
        &self,
        snapshot: &JournalSnapshot,
        tasks: &[TaskRecord],
    ) -> StoreResult<()>;

    // -- Activation leases (R0.7 wave 1) -------------------------------- //
    //
    // The single-activation mechanism behind turn-serialized mailbox
    // draining — see `crate::agents` for the model. Every operation is
    // keyed by the tenant-scoped agent id.

    /// Claim the agent's activation lease for `owner`: a fresh claim when
    /// no lease exists or the current one has expired (a steal — the dead
    /// host's replacement), bumping the fencing ordinal; [`ActivationOutcome::Held`]
    /// when a live lease belongs to another owner (route answers 409 with
    /// the current record).
    ///
    /// On Postgres the claim is one atomic transaction (the existing
    /// lease row is locked `FOR UPDATE` before the steal decision), so
    /// two racing claimants can never both win — the activation-lease
    /// equivalent of `FOR UPDATE SKIP LOCKED`. On the JSON-file backend the
    /// one index lock gives the same exactness in-process; the documented
    /// one-writer-process precondition covers the rest (design open
    /// question 1's chosen default).
    async fn claim_activation(
        &self,
        agent_id: &str,
        owner: &str,
        lease_ms: u64,
        now: chrono::DateTime<chrono::Utc>,
    ) -> StoreResult<ActivationOutcome>;
    /// Renew the activation lease held by `owner` under `fencing`
    /// (heartbeat). The owner + fencing + liveness check is atomic with the
    /// renewal: a stale holder can never resurrect its activation.
    async fn renew_activation(
        &self,
        agent_id: &str,
        owner: &str,
        fencing: u64,
        lease_ms: u64,
        now: chrono::DateTime<chrono::Utc>,
    ) -> StoreResult<ActivationMutation>;
    /// Release the activation lease held by `owner` under `fencing` (a
    /// draining host letting another activate promptly instead of waiting
    /// out the expiry). Same atomic owner + fencing guard as the renewal.
    async fn release_activation(
        &self,
        agent_id: &str,
        owner: &str,
        fencing: u64,
        now: chrono::DateTime<chrono::Utc>,
    ) -> StoreResult<ActivationMutation>;
    /// Fetch the agent's current activation lease (live or expired), for
    /// status reads; `None` when the agent has never been activated.
    async fn get_activation(&self, agent_id: &str) -> StoreResult<Option<ActivationLease>>;

    // -- Turn-serialized mailbox claim (R0.7 wave 1) -------------------- //

    /// Claim the oldest claimable task addressed to `scope.recipient` (the
    /// agent's mailbox) as one turn of work, leased to `scope.owner`.
    ///
    /// Two gates, both applied before a candidate is chosen:
    ///
    /// 1. **Activation.** The caller must hold the agent's live activation
    ///    lease (`scope.owner` + `scope.fencing`); otherwise
    ///    [`MailboxClaim::ActivationLost`] — the host must (re-)activate.
    /// 2. **Turn serialization.** A live-leased message already in flight
    ///    for this recipient makes the whole mailbox answer
    ///    [`MailboxClaim::Empty`]: one message at a time per agent is
    ///    server-enforced, not host discipline. On Postgres the claim
    ///    transaction locks the activation-lease row (`FOR UPDATE`), so
    ///    concurrent claims by one holder serialize on it and the gate is
    ///    exact; on the file backend the index locks give the same
    ///    exactness in-process.
    ///
    /// The claim runs the same cancellation/deadline finalization sweep as
    /// [`ServerStore::claim_task`], and the task lease it grants is the
    /// ordinary one — the turn settles through the unchanged
    /// `/tasks/{id}/heartbeat|complete|fail` protocol. Pool capacity and
    /// worker-version pins do not apply here: pools are deployment-level
    /// worker groups, and the manifest pin (not the worker pin) is the
    /// agent-level version story.
    async fn claim_agent_task(
        &self,
        tenant: &str,
        scope: &MailboxClaimScope<'_>,
        lease_ms: u64,
        now: chrono::DateTime<chrono::Utc>,
    ) -> StoreResult<MailboxClaim>;

    // -- Governed memory (R0.8 Rusty Learn, wave 1) --------------------- //

    /// Store a memory record under its tenant-scoped content address;
    /// `false` (no write) when the address is already present — content
    /// addressing makes the write idempotent by construction (the
    /// `Effect::Idempotent` write converges). When the record's content
    /// is artifact-referenced, `content` (the value the address was
    /// minted from) is spilled into the backend's artifact store first;
    /// reads re-inline it, so served records are always self-contained.
    async fn put_memory(
        &self,
        tenant: &str,
        record: &MemoryRecord,
        content: &Value,
    ) -> StoreResult<bool>;
    /// Fetch one memory record by its (bare) content address,
    /// tenant-scoped (`None` for unknown or cross-tenant addresses — the
    /// two are indistinguishable by design). Artifact-referenced content
    /// is resolved before returning.
    async fn get_memory(&self, tenant: &str, memory_id: &str) -> StoreResult<Option<MemoryRecord>>;
    /// The tenant's records matching `query`, expiry evaluated at `now`
    /// (the route-resolved [`MemoryQuery::as_of`]). Semantics are core's
    /// [`apply_query`](rusty_agent_runtime::memory::apply_query) exactly:
    /// the JSON backend scans and applies it directly; Postgres
    /// pre-filters on columns and applies the same matcher to the
    /// reduced set, so filter semantics live in exactly one place.
    async fn query_memory(
        &self,
        tenant: &str,
        query: &MemoryQuery,
        now: chrono::DateTime<chrono::Utc>,
    ) -> StoreResult<Vec<MemoryRecord>>;
    /// Remove one memory record by its (bare) content address,
    /// tenant-scoped (`false` for unknown or cross-tenant addresses).
    /// Forgetting (R0.8 wave 2) is real deletion of derived state —
    /// journals are hash-chained evidence and are never touched, and
    /// spilled content-addressed blobs stay (shared evidence under the
    /// same boundary, design open question 4).
    async fn delete_memory(&self, tenant: &str, memory_id: &str) -> StoreResult<bool>;

    // -- Learning candidates (R0.8 Rusty Learn, wave 3) ----------------- //

    /// Store a candidate record under its tenant-scoped candidate id;
    /// `false` (no write) when the id is already present — candidates
    /// are content-addressed, so creation converges the way memory
    /// writes do (the `Effect::Idempotent` create).
    async fn put_candidate(&self, tenant: &str, record: &CandidateRecord) -> StoreResult<bool>;
    /// Fetch one candidate record by its (bare) candidate id,
    /// tenant-scoped (`None` for unknown or cross-tenant ids — the two
    /// are indistinguishable by design).
    async fn get_candidate(
        &self,
        tenant: &str,
        candidate_id: &str,
    ) -> StoreResult<Option<CandidateRecord>>;
    /// The tenant's candidate records (order unspecified; routes sort).
    async fn list_candidates(&self, tenant: &str) -> StoreResult<Vec<CandidateRecord>>;
    /// Apply a lifecycle transition: the record's live status must be
    /// `expect`; on a match, atomically replace the record with `next`
    /// and, when `pointer` is set, move that surface's version pointer.
    /// One transaction on Postgres, one lock pair (candidates →
    /// versions, the only order taken) on the file backend — a promoted
    /// candidate whose pointer never moved (or the inverse) must not be
    /// a reachable state. Unknown/cross-tenant ids answer
    /// [`CandidateTransition::Unknown`] (`404`); a status mismatch
    /// answers [`CandidateTransition::Conflict`] with the live status
    /// (`409`) and changes nothing.
    async fn transition_candidate(
        &self,
        tenant: &str,
        candidate_id: &str,
        expect: CandidateStatus,
        next: &CandidateRecord,
        pointer: Option<&VersionPointer>,
    ) -> StoreResult<CandidateTransition>;
    /// Fetch the version pointer for a surface, tenant-scoped (`None`
    /// when nothing was ever promoted onto the surface — the static
    /// version serves).
    async fn get_version_pointer(
        &self,
        tenant: &str,
        surface: &str,
    ) -> StoreResult<Option<VersionPointer>>;
    /// The tenant's version pointers (order unspecified; routes sort).
    async fn list_version_pointers(&self, tenant: &str) -> StoreResult<Vec<VersionPointer>>;

    // -- The executor policy registry (R0.8 Rusty Learn, wave 4) -------- //

    /// Register one immutable policy body under its tenant-scoped version.
    /// [`PolicyWrite::Created`] when the version is new;
    /// [`PolicyWrite::Converged`] when it already names exactly this body
    /// (the idempotent create — content addressing makes re-registration
    /// converge the way memory writes do); [`PolicyWrite::Conflict`] when
    /// it names a different body — registry immutability refuses the
    /// overwrite, so a version string stays a commitment to one exact
    /// parameter set.
    async fn put_policy(&self, tenant: &str, record: &PolicyRecord) -> StoreResult<PolicyWrite>;
    /// Fetch one policy body by version, tenant-scoped (`None` for unknown
    /// or cross-tenant versions — the two are indistinguishable by
    /// design). The static floor is not in the store (it is never
    /// registered); callers synthesize it.
    async fn get_policy(&self, tenant: &str, version: &str) -> StoreResult<Option<PolicyRecord>>;
    /// The tenant's registered policy bodies (order unspecified; routes
    /// sort).
    async fn list_policies(&self, tenant: &str) -> StoreResult<Vec<PolicyRecord>>;
    /// Append one activation to the tenant's log (append-only; the active
    /// version is the latest entry). Ordering is carried by
    /// `activated_at` in the record — and by insertion order on Postgres
    /// (the serial key) — never by filename.
    async fn append_policy_activation(
        &self,
        tenant: &str,
        activation: &PolicyActivation,
    ) -> StoreResult<()>;
    /// The tenant's activation log, oldest first — the active version is
    /// the last entry. This is the registry's epoch history.
    async fn list_policy_activations(&self, tenant: &str) -> StoreResult<Vec<PolicyActivation>>;
    /// Record one admission binding (`false`-free upsert on the checkpoint
    /// id: a re-put of the same checkpoint's binding is the same fact).
    async fn put_policy_binding(&self, tenant: &str, binding: &PolicyBinding) -> StoreResult<()>;
    /// The tenant's recorded bindings (order unspecified; the epoch
    /// derivation sorts).
    async fn list_policy_bindings(&self, tenant: &str) -> StoreResult<Vec<PolicyBinding>>;
}

/// The outcome of a candidate lifecycle transition
/// ([`ServerStore::transition_candidate`]) — the task-mutation
/// convention ([`MutationOutcome`]): unknown and cross-tenant ids are
/// indistinguishable, a status mismatch is a conflict, and anything but
/// `Applied` changes nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CandidateTransition {
    /// The transition applied: record replaced, pointer moved when given.
    Applied,
    /// No such candidate in this tenant.
    Unknown,
    /// The candidate's live status is not the expected one.
    Conflict(CandidateStatus),
}

// --------------------------------------------------------------------- //
// JsonFileStore — default, extracted v0.2 behavior
// --------------------------------------------------------------------- //

/// JSON-file-backed store rooted at `ServerConfig::store_path`.
///
/// Assistants, crons, and threads are served from an in-memory index
/// (loaded from disk at construction) with one file per record written
/// through on every mutation — exactly the v0.2 route behavior. KV items go
/// straight to the file system, serialized by `kv_lock` so `created_at`
/// preservation cannot race.
pub(crate) struct JsonFileStore {
    root: PathBuf,
    assistants: Mutex<HashMap<String, AssistantRecord>>,
    crons: Mutex<HashMap<String, CronRecord>>,
    threads: Mutex<HashMap<String, ThreadRecord>>,
    kv_lock: Mutex<()>,
    tasks: Mutex<HashMap<String, TaskRecord>>,
    outbox: Mutex<HashMap<String, OutboxRecord>>,
    agents: Mutex<HashMap<String, AgentRecord>>,
    coordinations: Mutex<HashMap<String, CoordinationRecord>>,
    agent_leases: Mutex<HashMap<String, ActivationLease>>,
    // One long-lived checkpointer for the checkpoint write path (W4): the
    // delta-head cache lives on the instance, so a fresh checkpointer per
    // write would re-walk the on-disk chain on every put.
    checkpointer: JsonFileCheckpointer,
    triggers: Mutex<HashMap<String, TriggerRecord>>,
    /// Per-trigger event logs keyed by internal trigger id, each oldest
    /// first (appends are chronological; status-transition upserts keep
    /// position), so pruning drops from the front.
    trigger_events: Mutex<HashMap<String, Vec<TriggerEventRecord>>>,
    /// Governed memory (R0.8): the in-memory index keyed by
    /// tenant-scoped content address (`{tenant}/{address}`), persisted
    /// as one file per record under `{store_path}/memory/`. Records are
    /// stored with artifact-referenced content; reads re-inline via
    /// `memory_artifacts`.
    memories: Mutex<HashMap<String, MemoryRecord>>,
    /// The artifact store spilled memory bodies live in (a sibling of
    /// the records dir, so the recursive record loader never picks up a
    /// blob).
    memory_artifacts: FileArtifactStore,
    /// Learning candidates (R0.8 wave 3): the in-memory index keyed by
    /// tenant-scoped candidate id (`{tenant}/{candidate_id}`), persisted
    /// as one file per record under `{store_path}/learn/candidates/`.
    candidates: Mutex<HashMap<String, CandidateRecord>>,
    /// Version pointers keyed by tenant-scoped surface key, one file
    /// per pointer under `{store_path}/learn/versions/` (the filename is
    /// the key's hash; the file body carries the key).
    versions: Mutex<HashMap<String, VersionPointer>>,
    /// The executor policy registry (R0.8 wave 4): policy bodies keyed by
    /// tenant-scoped version, one file per record under
    /// `{store_path}/policy/versions/`.
    policies: Mutex<HashMap<String, PolicyRecord>>,
    /// The activation log keyed by tenant-scoped file name
    /// (`{tenant}/{millis:013}-{version}`), one append-only file per
    /// activation under `{store_path}/policy/activations/`. Ordering comes
    /// from the record's `activated_at`, never the key.
    activations: Mutex<HashMap<String, PolicyActivation>>,
    /// Admission bindings keyed by tenant-scoped checkpoint id, one file
    /// per binding under `{store_path}/policy/bindings/`.
    bindings: Mutex<HashMap<String, PolicyBinding>>,
}

impl JsonFileStore {
    /// Load the persisted assistants/crons/threads under `root` into memory.
    pub(crate) fn load(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            assistants: Mutex::new(assistants::load(root)),
            crons: Mutex::new(crons::load(root)),
            threads: Mutex::new(threads::load(root)),
            kv_lock: Mutex::new(()),
            tasks: Mutex::new(tasks::load(root)),
            outbox: Mutex::new(outbox::load(root)),
            agents: Mutex::new(agents::load(root)),
            coordinations: Mutex::new(coordination::load(root)),
            agent_leases: Mutex::new(agents::load_leases(root)),
            checkpointer: JsonFileCheckpointer::new(root),
            triggers: Mutex::new(triggers::load(root)),
            trigger_events: Mutex::new(triggers::load_events(root)),
            memories: Mutex::new(memory::load(root)),
            memory_artifacts: memory::artifact_store(root),
            candidates: Mutex::new(learn::load_candidates(root)),
            versions: Mutex::new(learn::load_versions(root)),
            policies: Mutex::new(policy::load_policies(root)),
            activations: Mutex::new(policy::load_activations(root)),
            bindings: Mutex::new(policy::load_bindings(root)),
        }
    }
}

fn io_err(context: &str) -> impl Fn(std::io::Error) -> String + '_ {
    move |e| format!("{context}: {e}")
}

#[async_trait::async_trait]
impl ServerStore for JsonFileStore {
    async fn create_assistant(&self, record: &AssistantRecord) -> StoreResult<bool> {
        let mut map = self.assistants.lock().await;
        if map.contains_key(&record.assistant_id) {
            return Ok(false);
        }
        // Hold the lock across the file write so a concurrent create of the
        // same id can't interleave.
        assistants::persist(&self.root, record)
            .await
            .map_err(io_err("persist assistant"))?;
        map.insert(record.assistant_id.clone(), record.clone());
        Ok(true)
    }

    async fn get_assistant(&self, assistant_id: &str) -> StoreResult<Option<AssistantRecord>> {
        Ok(self.assistants.lock().await.get(assistant_id).cloned())
    }

    async fn list_assistants(&self) -> StoreResult<Vec<AssistantRecord>> {
        Ok(self.assistants.lock().await.values().cloned().collect())
    }

    async fn create_cron(&self, record: &CronRecord) -> StoreResult<bool> {
        let mut map = self.crons.lock().await;
        if map.contains_key(&record.cron_id) {
            return Ok(false);
        }
        crons::persist(&self.root, record)
            .await
            .map_err(io_err("persist cron"))?;
        map.insert(record.cron_id.clone(), record.clone());
        Ok(true)
    }

    async fn upsert_cron(&self, record: &CronRecord) -> StoreResult<()> {
        let mut map = self.crons.lock().await;
        crons::persist(&self.root, record)
            .await
            .map_err(io_err("persist cron"))?;
        map.insert(record.cron_id.clone(), record.clone());
        Ok(())
    }

    async fn get_cron(&self, cron_id: &str) -> StoreResult<Option<CronRecord>> {
        Ok(self.crons.lock().await.get(cron_id).cloned())
    }

    async fn list_crons(&self) -> StoreResult<Vec<CronRecord>> {
        Ok(self.crons.lock().await.values().cloned().collect())
    }

    async fn delete_cron(&self, cron_id: &str) -> StoreResult<bool> {
        let mut map = self.crons.lock().await;
        let Some(record) = map.remove(cron_id) else {
            return Ok(false);
        };
        let path = crons::dir(&self.root).join(format!("{cron_id}.json"));
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(true),
            // The file is already gone; the in-memory index was authoritative.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
            // On removal failure the record must stay in memory: dropping it
            // here would let the orphaned file resurrect the cron on the
            // next restart while the API already answered `deleted: true`.
            Err(e) => {
                map.insert(cron_id.to_string(), record);
                Err(format!("remove cron file: {e}"))
            }
        }
    }

    async fn create_trigger(&self, record: &TriggerRecord) -> StoreResult<bool> {
        let mut map = self.triggers.lock().await;
        if map.contains_key(&record.trigger_id) {
            return Ok(false);
        }
        // Hold the lock across the file write so a concurrent create of the
        // same id can't interleave.
        triggers::persist(&self.root, record)
            .await
            .map_err(io_err("persist trigger"))?;
        map.insert(record.trigger_id.clone(), record.clone());
        Ok(true)
    }

    async fn upsert_trigger(&self, record: &TriggerRecord) -> StoreResult<()> {
        let mut map = self.triggers.lock().await;
        triggers::persist(&self.root, record)
            .await
            .map_err(io_err("persist trigger"))?;
        map.insert(record.trigger_id.clone(), record.clone());
        Ok(())
    }

    async fn get_trigger(&self, trigger_id: &str) -> StoreResult<Option<TriggerRecord>> {
        Ok(self.triggers.lock().await.get(trigger_id).cloned())
    }

    async fn list_triggers(&self) -> StoreResult<Vec<TriggerRecord>> {
        Ok(self.triggers.lock().await.values().cloned().collect())
    }

    async fn delete_trigger(&self, trigger_id: &str) -> StoreResult<bool> {
        let mut map = self.triggers.lock().await;
        let Some(record) = map.remove(trigger_id) else {
            return Ok(false);
        };
        let path = triggers::dir(&self.root).join(format!("{trigger_id}.json"));
        match tokio::fs::remove_file(&path).await {
            Ok(()) => {}
            // The file is already gone; the in-memory index was authoritative.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            // Same resurrection discipline as cron delete.
            Err(e) => {
                map.insert(trigger_id.to_string(), record);
                return Err(format!("remove trigger file: {e}"));
            }
        }
        // The trigger is gone; its event log goes with it (memory + files).
        // An event-dir removal failure only orphans files for a trigger that
        // no longer resolves — logged, not fatal.
        let mut events = self.trigger_events.lock().await;
        events.remove(trigger_id);
        let events_path = triggers::events_dir(&self.root).join(trigger_id);
        if let Err(e) = tokio::fs::remove_dir_all(&events_path).await {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(path = %events_path.display(), %e, "remove trigger events dir failed")
            }
        }
        Ok(true)
    }

    async fn append_trigger_event(&self, record: &TriggerEventRecord) -> StoreResult<()> {
        let mut map = self.trigger_events.lock().await;
        let events = map.entry(record.trigger_id.clone()).or_default();
        // Upsert on event id (status transitions rewrite the entry in
        // place); new events append, keeping the list chronological.
        if let Some(existing) = events.iter_mut().find(|e| e.event_id == record.event_id) {
            *existing = record.clone();
        } else {
            events.push(record.clone());
        }
        triggers::persist_event(&self.root, record)
            .await
            .map_err(io_err("persist trigger event"))?;
        // Retention: prune the oldest entries past the cap (and their
        // files) — the log is an inspection surface, not an unbounded
        // journal.
        while events.len() > triggers::MAX_EVENTS_PER_TRIGGER {
            let dropped = events.remove(0);
            let path = triggers::event_path(&self.root, &dropped);
            if let Err(e) = tokio::fs::remove_file(&path).await {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(path = %path.display(), %e, "prune trigger event file failed")
                }
            }
        }
        Ok(())
    }

    async fn get_trigger_event(
        &self,
        trigger_id: &str,
        event_id: &str,
    ) -> StoreResult<Option<TriggerEventRecord>> {
        Ok(self
            .trigger_events
            .lock()
            .await
            .get(trigger_id)
            .and_then(|events| events.iter().find(|e| e.event_id == event_id).cloned()))
    }

    async fn list_trigger_events(&self, trigger_id: &str) -> StoreResult<Vec<TriggerEventRecord>> {
        Ok(self
            .trigger_events
            .lock()
            .await
            .get(trigger_id)
            .cloned()
            .unwrap_or_default())
    }

    async fn create_thread(&self, internal_id: &str, record: &ThreadRecord) -> StoreResult<bool> {
        let mut map = self.threads.lock().await;
        if map.contains_key(internal_id) {
            return Ok(false);
        }
        // Hold the lock across the file write so a concurrent create of the
        // same id can't interleave.
        threads::persist(&self.root, internal_id, record)
            .await
            .map_err(io_err("persist thread"))?;
        map.insert(internal_id.to_string(), record.clone());
        Ok(true)
    }

    async fn get_thread(&self, internal_id: &str) -> StoreResult<Option<ThreadRecord>> {
        Ok(self.threads.lock().await.get(internal_id).cloned())
    }

    async fn kv_put(
        &self,
        namespace: &str,
        key: &str,
        value: Value,
    ) -> StoreResult<(StoreItem, bool)> {
        let _guard = self.kv_lock.lock().await;
        store::put(&self.root, namespace, key, value)
            .await
            .map_err(io_err("put store item"))
    }

    async fn kv_get(&self, namespace: &str, key: &str) -> StoreResult<Option<StoreItem>> {
        store::get(&self.root, namespace, key)
            .await
            .map_err(io_err("get store item"))
    }

    async fn kv_delete(&self, namespace: &str, key: &str) -> StoreResult<bool> {
        let _guard = self.kv_lock.lock().await;
        store::delete(&self.root, namespace, key)
            .await
            .map_err(io_err("delete store item"))
    }

    async fn kv_list(&self, namespace: &str) -> StoreResult<Vec<StoreItem>> {
        store::list(&self.root, namespace)
            .await
            .map_err(io_err("list store namespace"))
    }

    async fn put_journal(&self, snapshot: &JournalSnapshot) -> StoreResult<()> {
        journals::persist(&self.root, snapshot)
            .await
            .map_err(io_err("persist journal"))
    }

    async fn get_journal(&self, run_id: &str) -> StoreResult<Option<JournalSnapshot>> {
        journals::get(&self.root, run_id)
            .await
            .map_err(io_err("get journal"))
    }

    async fn enqueue_task(&self, record: &TaskRecord) -> StoreResult<(TaskRecord, bool)> {
        let mut map = self.tasks.lock().await;
        self.enqueue_locked(&mut map, record).await
    }

    async fn outbox_enqueue(&self, record: &TaskRecord) -> StoreResult<(TaskRecord, bool)> {
        let mut map = self.outbox.lock().await;
        if let Some(existing) = map.get(&record.task_id) {
            return Ok((existing.task.clone(), true));
        }
        let row = OutboxRecord::new(record.clone(), chrono::Utc::now());
        outbox::persist(&self.root, &row)
            .await
            .map_err(io_err("persist outbox row"))?;
        map.insert(row.outbox_id.clone(), row);
        Ok((record.clone(), false))
    }

    async fn outbox_publish_pending(
        &self,
        limit: usize,
        now: chrono::DateTime<chrono::Utc>,
    ) -> StoreResult<Vec<TaskRecord>> {
        // Both locks held for the whole pass: a publish is pick + enqueue +
        // mark-published, and two concurrent passes must never publish the
        // same row — the file backend's SKIP LOCKED equivalent (as with
        // claim). Lock order is always outbox-then-tasks.
        let mut outbox_map = self.outbox.lock().await;
        let mut tasks_map = self.tasks.lock().await;
        let mut pending: Vec<(chrono::DateTime<chrono::Utc>, String)> = outbox_map
            .values()
            .filter(|row| row.published_at.is_none())
            .map(|row| (row.created_at, row.outbox_id.clone()))
            .collect();
        // Oldest first, with the id tie-break making the order total.
        pending.sort();
        pending.truncate(limit);
        let mut published = Vec::new();
        for (_, outbox_id) in pending {
            let mut row = outbox_map
                .get(&outbox_id)
                .cloned()
                .expect("publish candidate came from the outbox index");
            // The enqueue dedupe absorbs a task that already exists (a
            // publish retried after a crash, or one submitted directly);
            // marking the row published is still correct — the task exists.
            let (task, _deduplicated) = self.enqueue_locked(&mut tasks_map, &row.task).await?;
            row.published_at = Some(now);
            outbox::persist(&self.root, &row)
                .await
                .map_err(io_err("persist outbox row"))?;
            outbox_map.insert(outbox_id, row);
            published.push(task);
        }
        Ok(published)
    }

    async fn checkpoint_and_enqueue(
        &self,
        checkpoint: &Checkpoint,
        tasks: &[TaskRecord],
    ) -> StoreResult<()> {
        // Outbox-first ordering — one JSON file cannot transact across
        // checkpoint and queue, so the file backend guarantees only this: a
        // crash may leave published tasks whose checkpoint never landed
        // (visible, inspectable, and the idempotency key still correlates
        // them), but never a checkpoint whose tasks are silently gone.
        // Cross-record atomicity is Postgres-only.
        {
            let mut map = self.outbox.lock().await;
            for record in tasks {
                // A retried checkpoint+enqueue skips rows already pending.
                if map.contains_key(&record.task_id) {
                    continue;
                }
                let row = OutboxRecord::new(record.clone(), chrono::Utc::now());
                outbox::persist(&self.root, &row)
                    .await
                    .map_err(io_err("persist outbox row"))?;
                map.insert(row.outbox_id.clone(), row);
            }
        }
        // The checkpoint write goes through the store's long-lived
        // JSON-file checkpointer (delta-head cache warm across writes),
        // rooted at the same store path as the run routes'.
        self.checkpointer
            .put(checkpoint.clone())
            .await
            .map_err(|e| format!("put checkpoint: {e}"))
    }

    async fn journal_and_enqueue(
        &self,
        snapshot: &JournalSnapshot,
        tasks: &[TaskRecord],
    ) -> StoreResult<()> {
        // Outbox-first, journal-second — the ordering
        // `checkpoint_and_enqueue` documents: a crash may leave submitted
        // tasks whose journal events never landed (the next drive
        // re-journals them), but never a journal claiming submissions that
        // do not exist. One JSON file cannot transact across the two.
        {
            let mut map = self.outbox.lock().await;
            for record in tasks {
                // A retried drive skips rows already pending.
                if map.contains_key(&record.task_id) {
                    continue;
                }
                let row = OutboxRecord::new(record.clone(), chrono::Utc::now());
                outbox::persist(&self.root, &row)
                    .await
                    .map_err(io_err("persist outbox row"))?;
                map.insert(row.outbox_id.clone(), row);
            }
        }
        self.put_journal(snapshot).await
    }

    async fn claim_task(
        &self,
        tenant: &str,
        worker_id: &str,
        scope: &tasks::ClaimScope<'_>,
        lease_ms: u64,
        now: chrono::DateTime<chrono::Utc>,
    ) -> StoreResult<Option<TaskRecord>> {
        let mut map = self.tasks.lock().await;
        self.finalize_due_locked(&mut map, tenant, now).await?;
        // Pool capacity (wave 3): live — unexpired — leases per pool. An
        // expired lease holds no capacity: its task is visible again. A pool
        // at its configured limit is excluded from this claim, so one
        // saturated pool can never starve the others.
        let mut live_leases: HashMap<&str, u64> = HashMap::new();
        for task in map.values() {
            if task.tenant == tenant
                && task.status == TaskStatus::Leased
                && task.lease.as_ref().is_some_and(|l| l.expires_at > now)
            {
                *live_leases.entry(task.pool.as_str()).or_insert(0) += 1;
            }
        }
        let saturated = |pool: &str| {
            scope
                .pool_limits
                .get(pool)
                .is_some_and(|&limit| live_leases.get(pool).copied().unwrap_or(0) >= limit as u64)
        };
        // The whole claim (pick + mutate + persist) runs under the one
        // index lock, so two concurrent claims can never take the same
        // task — the file backend's SKIP LOCKED equivalent. Mailbox traffic
        // (`recipient` set, R0.7) is excluded: it drains only through the
        // turn-serialized agent claim, never through a pool.
        let candidate = map
            .values()
            .filter(|t| {
                t.tenant == tenant
                    && t.recipient.is_none()
                    && scope.pools.iter().any(|p| p == &t.pool)
                    && t.claimable_at(now)
                    && !saturated(&t.pool)
                    && t.matches_worker_version(scope.worker_version)
            })
            .min_by(|a, b| {
                a.created_at
                    .cmp(&b.created_at)
                    .then_with(|| a.task_id.cmp(&b.task_id))
            })
            .map(|t| t.task_id.clone());
        let Some(task_id) = candidate else {
            return Ok(None);
        };
        let mut task = map
            .get(&task_id)
            .cloned()
            .expect("claim candidate came from the task index");
        task.claim(worker_id, lease_ms, now);
        tasks::persist(&self.root, &task)
            .await
            .map_err(io_err("persist task"))?;
        map.insert(task_id, task.clone());
        Ok(Some(task))
    }

    async fn heartbeat_task(
        &self,
        tenant: &str,
        task_id: &str,
        worker_id: &str,
        lease_ms: u64,
        now: chrono::DateTime<chrono::Utc>,
    ) -> StoreResult<MutationOutcome> {
        self.mutate_task(tenant, task_id, worker_id, |task| {
            task.renew_lease(lease_ms, now);
        })
        .await
    }

    async fn complete_task(
        &self,
        tenant: &str,
        task_id: &str,
        worker_id: &str,
        report: tasks::CompletionReport,
        now: chrono::DateTime<chrono::Utc>,
    ) -> StoreResult<MutationOutcome> {
        self.mutate_task(tenant, task_id, worker_id, |task| {
            task.complete(report.result, report.receipt, report.cost, now);
        })
        .await
    }

    async fn fail_task(
        &self,
        tenant: &str,
        task_id: &str,
        worker_id: &str,
        report: tasks::FailureReport,
        now: chrono::DateTime<chrono::Utc>,
    ) -> StoreResult<MutationOutcome> {
        self.mutate_task(tenant, task_id, worker_id, |task| {
            task.fail(
                report.error_class,
                &report.message,
                report.retryable,
                report.cost,
                now,
            );
        })
        .await
    }

    async fn get_task(&self, tenant: &str, task_id: &str) -> StoreResult<Option<TaskRecord>> {
        Ok(self
            .tasks
            .lock()
            .await
            .get(task_id)
            .filter(|t| t.tenant == tenant)
            .cloned())
    }

    async fn list_tasks(
        &self,
        tenant: &str,
        status: Option<TaskStatus>,
    ) -> StoreResult<Vec<TaskRecord>> {
        let mut tasks: Vec<TaskRecord> = self
            .tasks
            .lock()
            .await
            .values()
            .filter(|t| t.tenant == tenant && status.is_none_or(|s| t.status == s))
            .cloned()
            .collect();
        tasks.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.task_id.cmp(&b.task_id))
        });
        Ok(tasks)
    }

    async fn cancel_task(
        &self,
        tenant: &str,
        task_id: &str,
        now: chrono::DateTime<chrono::Utc>,
    ) -> StoreResult<CancelOutcome> {
        let mut map = self.tasks.lock().await;
        let Some(current) = map.get(task_id) else {
            return Ok(CancelOutcome::Unknown);
        };
        // Cross-tenant ids are indistinguishable from unknown ones (404).
        if current.tenant != tenant {
            return Ok(CancelOutcome::Unknown);
        }
        let mut task = current.clone();
        let Some(_transition) = task.cancel(now) else {
            return Ok(CancelOutcome::Terminal(task.status));
        };
        tasks::persist(&self.root, &task)
            .await
            .map_err(io_err("persist task"))?;
        map.insert(task_id.to_string(), task.clone());
        Ok(CancelOutcome::Applied(Box::new(task)))
    }

    async fn cancel_run_tasks(
        &self,
        tenant: &str,
        run_id: &str,
        now: chrono::DateTime<chrono::Utc>,
    ) -> StoreResult<RunCancellation> {
        let mut map = self.tasks.lock().await;
        let targets: Vec<String> = map
            .values()
            .filter(|t| t.tenant == tenant && t.run_id.as_deref() == Some(run_id))
            .map(|t| t.task_id.clone())
            .collect();
        let mut outcome = RunCancellation::default();
        for task_id in targets {
            let mut task = map
                .get(&task_id)
                .cloned()
                .expect("cancel target came from the task index");
            let Some(transition) = task.cancel(now) else {
                continue; // already terminal: nothing to propagate
            };
            tasks::persist(&self.root, &task)
                .await
                .map_err(io_err("persist task"))?;
            map.insert(task_id, task.clone());
            match transition {
                tasks::CancelTransition::Cancelled => outcome.cancelled.push(task),
                tasks::CancelTransition::Signalled => outcome.signalled.push(task),
            }
        }
        Ok(outcome)
    }

    async fn cancel_agent_tasks(
        &self,
        tenant: &str,
        recipient: &str,
        now: chrono::DateTime<chrono::Utc>,
    ) -> StoreResult<RunCancellation> {
        // The agent-scoped twin of `cancel_run_tasks`: same two
        // transitions, same persist-before-swap discipline, the recipient
        // filter in place of the run-id one.
        let mut map = self.tasks.lock().await;
        let targets: Vec<String> = map
            .values()
            .filter(|t| t.tenant == tenant && t.recipient.as_deref() == Some(recipient))
            .map(|t| t.task_id.clone())
            .collect();
        let mut outcome = RunCancellation::default();
        for task_id in targets {
            let mut task = map
                .get(&task_id)
                .cloned()
                .expect("cancel target came from the task index");
            let Some(transition) = task.cancel(now) else {
                continue; // already terminal: nothing to propagate
            };
            tasks::persist(&self.root, &task)
                .await
                .map_err(io_err("persist task"))?;
            map.insert(task_id, task.clone());
            match transition {
                tasks::CancelTransition::Cancelled => outcome.cancelled.push(task),
                tasks::CancelTransition::Signalled => outcome.signalled.push(task),
            }
        }
        Ok(outcome)
    }

    async fn dead_letter_task(&self, record: &TaskRecord) -> StoreResult<(TaskRecord, bool)> {
        // The file backend persists records verbatim, so the shared
        // dedupe-and-insert path carries the dead status as-is.
        let mut map = self.tasks.lock().await;
        self.enqueue_locked(&mut map, record).await
    }

    async fn task_usage(&self, tenant: &str) -> StoreResult<tasks::TaskUsage> {
        // Both locks held for the whole count, in the publish path's order
        // (outbox before tasks — see `outbox_publish_pending`). Two separate
        // scans would race the relay: a publish landing between them moves a
        // row from pending to queued while *both* scans miss it (tasks
        // scanned before the insert, outbox scanned after the mark), and the
        // quota gate would read zero for work that exists.
        let outbox_map = self.outbox.lock().await;
        let tasks_map = self.tasks.lock().await;
        let mut usage = tasks::TaskUsage::default();
        for task in tasks_map.values() {
            if task.tenant != tenant {
                continue;
            }
            match task.status {
                TaskStatus::Queued => usage.queued += 1,
                TaskStatus::Failed if task.next_attempt_at.is_some() => usage.queued += 1,
                TaskStatus::Leased => usage.in_flight += 1,
                TaskStatus::Dead => usage.dlq += 1,
                _ => {}
            }
        }
        // Pending outbox rows count against the queued gauge: they are
        // accepted submissions not yet visible, and the quota exists to
        // bound the whole pipeline.
        usage.queued += outbox_map
            .values()
            .filter(|row| row.tenant == tenant && row.published_at.is_none())
            .count() as u64;
        Ok(usage)
    }

    async fn task_pool_stats(
        &self,
        tenant: &str,
        now: chrono::DateTime<chrono::Utc>,
    ) -> StoreResult<Vec<tasks::PoolStat>> {
        let mut by_pool: HashMap<String, tasks::PoolStat> = HashMap::new();
        for task in self.tasks.lock().await.values() {
            if task.tenant != tenant {
                continue;
            }
            let stat = by_pool
                .entry(task.pool.clone())
                .or_insert_with(|| tasks::PoolStat {
                    pool: task.pool.clone(),
                    queue_depth: 0,
                    leased: 0,
                    oldest_visible_at: None,
                });
            match task.status {
                TaskStatus::Queued => stat.queue_depth += 1,
                TaskStatus::Failed if task.next_attempt_at.is_some() => stat.queue_depth += 1,
                TaskStatus::Leased if task.lease.as_ref().is_some_and(|l| l.expires_at > now) => {
                    stat.leased += 1
                }
                _ => {}
            }
            // Visible right now = what the claim path would consider:
            // queued, backoff-elapsed, or lease-expired.
            let visible = task.status == TaskStatus::Queued
                || (task.status == TaskStatus::Failed
                    && task.next_attempt_at.is_some_and(|at| at <= now))
                || (task.status == TaskStatus::Leased
                    && task.lease.as_ref().is_some_and(|l| l.expires_at <= now));
            if visible {
                stat.oldest_visible_at = match stat.oldest_visible_at {
                    Some(oldest) if oldest <= task.created_at => Some(oldest),
                    _ => Some(task.created_at),
                };
            }
        }
        let mut stats: Vec<tasks::PoolStat> = by_pool.into_values().collect();
        stats.sort_by(|a, b| a.pool.cmp(&b.pool));
        Ok(stats)
    }

    async fn create_agent(&self, record: &AgentRecord) -> StoreResult<bool> {
        let mut map = self.agents.lock().await;
        if map.contains_key(&record.agent_id) {
            return Ok(false);
        }
        // Hold the lock across the file write so a concurrent create of the
        // same id can't interleave (the assistants convention).
        agents::persist(&self.root, record)
            .await
            .map_err(io_err("persist agent"))?;
        map.insert(record.agent_id.clone(), record.clone());
        Ok(true)
    }

    async fn update_agent(&self, record: &AgentRecord) -> StoreResult<bool> {
        let mut map = self.agents.lock().await;
        if !map.contains_key(&record.agent_id) {
            return Ok(false);
        }
        // Same lock discipline as create: persist before the index swap, so
        // a failed write cannot leave state a restart would silently rewind.
        agents::persist(&self.root, record)
            .await
            .map_err(io_err("persist agent"))?;
        map.insert(record.agent_id.clone(), record.clone());
        Ok(true)
    }

    async fn get_agent(&self, agent_id: &str) -> StoreResult<Option<AgentRecord>> {
        Ok(self.agents.lock().await.get(agent_id).cloned())
    }

    async fn list_agents(&self) -> StoreResult<Vec<AgentRecord>> {
        Ok(self.agents.lock().await.values().cloned().collect())
    }

    async fn create_coordination(&self, record: &CoordinationRecord) -> StoreResult<bool> {
        let mut map = self.coordinations.lock().await;
        if map.contains_key(&record.coordination_id) {
            return Ok(false);
        }
        // Hold the lock across the file write so a concurrent create of the
        // same id can't interleave (the agents convention).
        coordination::persist(&self.root, record)
            .await
            .map_err(io_err("persist coordination"))?;
        map.insert(record.coordination_id.clone(), record.clone());
        Ok(true)
    }

    async fn update_coordination(&self, record: &CoordinationRecord) -> StoreResult<bool> {
        let mut map = self.coordinations.lock().await;
        if !map.contains_key(&record.coordination_id) {
            return Ok(false);
        }
        // Persist before the index swap: a failed write must not leave
        // in-memory state a restart would silently rewind.
        coordination::persist(&self.root, record)
            .await
            .map_err(io_err("persist coordination"))?;
        map.insert(record.coordination_id.clone(), record.clone());
        Ok(true)
    }

    async fn get_coordination(
        &self,
        coordination_id: &str,
    ) -> StoreResult<Option<CoordinationRecord>> {
        Ok(self
            .coordinations
            .lock()
            .await
            .get(coordination_id)
            .cloned())
    }

    async fn claim_activation(
        &self,
        agent_id: &str,
        owner: &str,
        lease_ms: u64,
        now: chrono::DateTime<chrono::Utc>,
    ) -> StoreResult<ActivationOutcome> {
        let mut map = self.agent_leases.lock().await;
        // The whole claim (check + insert + persist) runs under the one
        // index lock, so two racing claimants can never both win — the file
        // backend's row-lock equivalent.
        if let Some(current) = map.get(agent_id) {
            if current.expires_at > now {
                return Ok(ActivationOutcome::Held(Box::new(current.clone())));
            }
        }
        let fencing = map.get(agent_id).map_or(1, |lease| lease.fencing + 1);
        let lease = ActivationLease {
            agent_id: agent_id.to_string(),
            owner: owner.to_string(),
            fencing,
            expires_at: now + agents::lease_duration(lease_ms),
            acquired_at: now,
        };
        agents::persist_lease(&self.root, &lease)
            .await
            .map_err(io_err("persist agent lease"))?;
        map.insert(agent_id.to_string(), lease.clone());
        Ok(ActivationOutcome::Claimed(Box::new(lease)))
    }

    async fn renew_activation(
        &self,
        agent_id: &str,
        owner: &str,
        fencing: u64,
        lease_ms: u64,
        now: chrono::DateTime<chrono::Utc>,
    ) -> StoreResult<ActivationMutation> {
        let mut map = self.agent_leases.lock().await;
        let Some(current) = map.get(agent_id) else {
            return Ok(ActivationMutation::Unknown);
        };
        if !current.held_by(owner, fencing, now) {
            return Ok(ActivationMutation::FencingLost);
        }
        let mut lease = current.clone();
        lease.expires_at = now + agents::lease_duration(lease_ms);
        agents::persist_lease(&self.root, &lease)
            .await
            .map_err(io_err("persist agent lease"))?;
        map.insert(agent_id.to_string(), lease.clone());
        Ok(ActivationMutation::Applied(Box::new(lease)))
    }

    async fn release_activation(
        &self,
        agent_id: &str,
        owner: &str,
        fencing: u64,
        now: chrono::DateTime<chrono::Utc>,
    ) -> StoreResult<ActivationMutation> {
        let mut map = self.agent_leases.lock().await;
        let Some(current) = map.get(agent_id) else {
            return Ok(ActivationMutation::Unknown);
        };
        // Release names the fencing ordinal the holder believes it holds;
        // an expired lease is already stealable, so only a live holder
        // match releases — anyone else gets FencingLost either way.
        if !current.held_by(owner, fencing, now) {
            return Ok(ActivationMutation::FencingLost);
        }
        let lease = current.clone();
        // Remove from the index first, then the file: a failed file removal
        // resurrects the lease at next boot, which is safe (it simply
        // expires); an index entry without a file would lie to status reads
        // until restart.
        map.remove(agent_id);
        agents::remove_lease(&self.root, agent_id)
            .await
            .map_err(io_err("remove agent lease"))?;
        Ok(ActivationMutation::Applied(Box::new(lease)))
    }

    async fn get_activation(&self, agent_id: &str) -> StoreResult<Option<ActivationLease>> {
        Ok(self.agent_leases.lock().await.get(agent_id).cloned())
    }

    async fn claim_agent_task(
        &self,
        tenant: &str,
        scope: &MailboxClaimScope<'_>,
        lease_ms: u64,
        now: chrono::DateTime<chrono::Utc>,
    ) -> StoreResult<MailboxClaim> {
        // Lock order is always leases-then-tasks, and both are held for the
        // whole claim: the activation check, the turn gate, and the task
        // claim must be one atomic observation (the file backend's
        // transaction equivalent).
        let leases = self.agent_leases.lock().await;
        let mut map = self.tasks.lock().await;

        // Gate 1 — activation: the caller must hold the agent's live lease.
        let Some(lease) = leases.get(scope.agent_id) else {
            return Ok(MailboxClaim::ActivationLost);
        };
        if !lease.held_by(scope.owner, scope.fencing, now) {
            return Ok(MailboxClaim::ActivationLost);
        }

        // The same finalization sweep the pool claim runs: unanswered
        // cancels and elapsed deadlines turn terminal-cancelled instead of
        // being re-leased to a turn.
        self.finalize_due_locked(&mut map, tenant, now).await?;

        // Gate 2 — turn serialization: a live-leased message already in
        // flight for this recipient makes the whole mailbox unclaimable.
        // One message at a time per agent is server-enforced.
        let turn_in_flight = map.values().any(|t| {
            t.tenant == tenant
                && t.recipient.as_deref() == Some(scope.recipient)
                && t.status == TaskStatus::Leased
                && t.lease.as_ref().is_some_and(|l| l.expires_at > now)
        });
        if turn_in_flight {
            return Ok(MailboxClaim::Empty);
        }

        // The oldest claimable message for this mailbox — approximate FIFO
        // on the happy path, per the design's honest ordering edge. Pool
        // capacity and worker-version pins do not apply (see the trait
        // contract).
        let candidate = map
            .values()
            .filter(|t| {
                t.tenant == tenant
                    && t.recipient.as_deref() == Some(scope.recipient)
                    && t.claimable_at(now)
            })
            .min_by(|a, b| {
                a.created_at
                    .cmp(&b.created_at)
                    .then_with(|| a.task_id.cmp(&b.task_id))
            })
            .map(|t| t.task_id.clone());
        let Some(task_id) = candidate else {
            return Ok(MailboxClaim::Empty);
        };
        let mut task = map
            .get(&task_id)
            .cloned()
            .expect("claim candidate came from the task index");
        task.claim(scope.owner, lease_ms, now);
        tasks::persist(&self.root, &task)
            .await
            .map_err(io_err("persist task"))?;
        map.insert(task_id, task.clone());
        Ok(MailboxClaim::Claimed(Box::new(task)))
    }

    async fn put_memory(
        &self,
        tenant: &str,
        record: &MemoryRecord,
        content: &Value,
    ) -> StoreResult<bool> {
        let scoped = crate::auth::scope_id(tenant, &record.memory_id);
        let mut map = self.memories.lock().await;
        if map.contains_key(&scoped) {
            return Ok(false);
        }
        // Spill before the record write, and hold the index lock across
        // both (the assistants convention): the blob is content-
        // addressed, so a failed record write leaves at worst a reusable
        // orphan — never a record pointing at missing bytes.
        memory::spill_content(&self.memory_artifacts, record, content).await?;
        memory::persist(&self.root, &scoped, record)
            .await
            .map_err(io_err("persist memory"))?;
        map.insert(scoped, record.clone());
        Ok(true)
    }

    async fn get_memory(&self, tenant: &str, memory_id: &str) -> StoreResult<Option<MemoryRecord>> {
        let scoped = crate::auth::scope_id(tenant, memory_id);
        let found = self.memories.lock().await.get(&scoped).cloned();
        let Some(mut record) = found else {
            return Ok(None);
        };
        memory::resolve_content(&self.memory_artifacts, &mut record).await?;
        Ok(Some(record))
    }

    async fn query_memory(
        &self,
        tenant: &str,
        query: &MemoryQuery,
        now: chrono::DateTime<chrono::Utc>,
    ) -> StoreResult<Vec<MemoryRecord>> {
        // The query universe is exactly the caller's tenant namespace —
        // tenancy rides the key prefix, and core's `apply_query`
        // supplies every filter semantic (including the superseded set,
        // computed over the whole universe).
        let universe: Vec<MemoryRecord> = {
            let map = self.memories.lock().await;
            map.iter()
                .filter(|(scoped, _)| crate::auth::strip_owned(tenant, scoped).is_some())
                .map(|(_, record)| record.clone())
                .collect()
        };
        let mut matched = apply_query(&universe, query, now);
        // Resolve after filtering: only served records pay the artifact
        // read.
        for record in &mut matched {
            memory::resolve_content(&self.memory_artifacts, record).await?;
        }
        Ok(matched)
    }

    async fn delete_memory(&self, tenant: &str, memory_id: &str) -> StoreResult<bool> {
        let scoped = crate::auth::scope_id(tenant, memory_id);
        // File before index (the put path's ordering, inverted): a crash
        // between the two leaves an in-memory entry the next reload
        // clears, never a record file no index remembers.
        memory::remove(&self.root, &scoped).await?;
        Ok(self.memories.lock().await.remove(&scoped).is_some())
    }

    async fn put_candidate(&self, tenant: &str, record: &CandidateRecord) -> StoreResult<bool> {
        let scoped = crate::auth::scope_id(tenant, record.candidate.candidate_id.as_str());
        let mut map = self.candidates.lock().await;
        if map.contains_key(&scoped) {
            return Ok(false);
        }
        // Hold the lock across the file write (the assistants
        // convention): a concurrent create of the same id can't
        // interleave.
        learn::persist_candidate(&self.root, &scoped, record)
            .await
            .map_err(io_err("persist candidate"))?;
        map.insert(scoped, record.clone());
        Ok(true)
    }

    async fn get_candidate(
        &self,
        tenant: &str,
        candidate_id: &str,
    ) -> StoreResult<Option<CandidateRecord>> {
        let scoped = crate::auth::scope_id(tenant, candidate_id);
        Ok(self.candidates.lock().await.get(&scoped).cloned())
    }

    async fn list_candidates(&self, tenant: &str) -> StoreResult<Vec<CandidateRecord>> {
        let map = self.candidates.lock().await;
        Ok(map
            .iter()
            .filter(|(scoped, _)| crate::auth::strip_owned(tenant, scoped).is_some())
            .map(|(_, record)| record.clone())
            .collect())
    }

    async fn transition_candidate(
        &self,
        tenant: &str,
        candidate_id: &str,
        expect: CandidateStatus,
        next: &CandidateRecord,
        pointer: Option<&VersionPointer>,
    ) -> StoreResult<CandidateTransition> {
        let scoped = crate::auth::scope_id(tenant, candidate_id);
        // Lock order is candidates → versions everywhere (promotion and
        // rollback take both): one fixed order, no deadlock. The lock
        // pair is the file backend's transaction — the status check and
        // both index swaps cannot interleave with another transition.
        let mut candidates = self.candidates.lock().await;
        let Some(current) = candidates.get(&scoped) else {
            return Ok(CandidateTransition::Unknown);
        };
        if current.status != expect {
            return Ok(CandidateTransition::Conflict(current.status));
        }
        // Files before index swaps (the `mutate_task` convention): a
        // failed write never leaves state a restart would silently
        // rewind. Two files cannot land atomically — but the candidate
        // object is immutable and complete inside the record, so a crash
        // between the writes leaves a consistent serving picture under
        // either interleaving (the pointer resolves a fully formed
        // candidate either way; only the lifecycle metadata can lag, and
        // the retry converges it). Postgres makes the pair one
        // transaction instead.
        learn::persist_candidate(&self.root, &scoped, next)
            .await
            .map_err(io_err("persist candidate"))?;
        if let Some(pointer) = pointer {
            let scoped_surface = crate::auth::scope_id(tenant, pointer.surface.as_str());
            learn::persist_version(&self.root, &scoped_surface, pointer)
                .await
                .map_err(io_err("persist version pointer"))?;
            self.versions
                .lock()
                .await
                .insert(scoped_surface, pointer.clone());
        }
        candidates.insert(scoped, next.clone());
        Ok(CandidateTransition::Applied)
    }

    async fn get_version_pointer(
        &self,
        tenant: &str,
        surface: &str,
    ) -> StoreResult<Option<VersionPointer>> {
        let scoped = crate::auth::scope_id(tenant, surface);
        Ok(self.versions.lock().await.get(&scoped).cloned())
    }

    async fn list_version_pointers(&self, tenant: &str) -> StoreResult<Vec<VersionPointer>> {
        let map = self.versions.lock().await;
        Ok(map
            .iter()
            .filter(|(scoped, _)| crate::auth::strip_owned(tenant, scoped).is_some())
            .map(|(_, pointer)| pointer.clone())
            .collect())
    }

    async fn put_policy(&self, tenant: &str, record: &PolicyRecord) -> StoreResult<PolicyWrite> {
        let scoped = crate::auth::scope_id(tenant, record.version.as_str());
        let mut map = self.policies.lock().await;
        if let Some(existing) = map.get(&scoped) {
            // Immutability: the same version naming the same body converges;
            // naming a different body conflicts. The comparison is the whole
            // record minus its registration instant — a converged re-post
            // carries a fresh `registered_at`, and the stored instant wins.
            if existing.version == record.version
                && existing.policy == record.policy
                && existing.source == record.source
            {
                return Ok(PolicyWrite::Converged);
            }
            return Ok(PolicyWrite::Conflict);
        }
        // Hold the lock across the file write (the assistants convention):
        // a concurrent create of the same version can't interleave.
        policy::persist_policy(&self.root, &scoped, record)
            .await
            .map_err(io_err("persist policy"))?;
        map.insert(scoped, record.clone());
        Ok(PolicyWrite::Created)
    }

    async fn get_policy(&self, tenant: &str, version: &str) -> StoreResult<Option<PolicyRecord>> {
        let scoped = crate::auth::scope_id(tenant, version);
        Ok(self.policies.lock().await.get(&scoped).cloned())
    }

    async fn list_policies(&self, tenant: &str) -> StoreResult<Vec<PolicyRecord>> {
        let map = self.policies.lock().await;
        Ok(map
            .iter()
            .filter(|(scoped, _)| crate::auth::strip_owned(tenant, scoped).is_some())
            .map(|(_, record)| record.clone())
            .collect())
    }

    async fn append_policy_activation(
        &self,
        tenant: &str,
        activation: &PolicyActivation,
    ) -> StoreResult<()> {
        let mut map = self.activations.lock().await;
        // File before index swap (the mutate convention): a failed write
        // never leaves state a restart would silently rewind. The append is
        // keyed by timestamp + version, so a retried append of the same
        // activation lands on the same file — the log converges.
        policy::append_activation(&self.root, tenant, activation)
            .await
            .map_err(io_err("persist policy activation"))?;
        let file_name = crate::auth::scope_id(tenant, &policy::activation_file_name(activation));
        map.insert(file_name, activation.clone());
        Ok(())
    }

    async fn list_policy_activations(&self, tenant: &str) -> StoreResult<Vec<PolicyActivation>> {
        let map = self.activations.lock().await;
        let mut out: Vec<PolicyActivation> = map
            .iter()
            .filter(|(scoped, _)| crate::auth::strip_owned(tenant, scoped).is_some())
            .map(|(_, activation)| activation.clone())
            .collect();
        // Ordering comes from the record, never the key (the layout doc):
        // a hand-edited filename cannot reorder history.
        out.sort_by(|a, b| {
            a.activated_at
                .cmp(&b.activated_at)
                .then_with(|| a.version.as_str().cmp(b.version.as_str()))
        });
        Ok(out)
    }

    async fn put_policy_binding(&self, tenant: &str, binding: &PolicyBinding) -> StoreResult<()> {
        let scoped = crate::auth::scope_id(tenant, &binding.checkpoint_id);
        let mut map = self.bindings.lock().await;
        policy::persist_binding(&self.root, tenant, binding)
            .await
            .map_err(io_err("persist policy binding"))?;
        map.insert(scoped, binding.clone());
        Ok(())
    }

    async fn list_policy_bindings(&self, tenant: &str) -> StoreResult<Vec<PolicyBinding>> {
        let map = self.bindings.lock().await;
        Ok(map
            .iter()
            .filter(|(scoped, _)| crate::auth::strip_owned(tenant, scoped).is_some())
            .map(|(_, binding)| binding.clone())
            .collect())
    }
}

impl JsonFileStore {
    /// The dedupe-and-insert half of enqueue, shared by direct enqueue and
    /// the outbox relay's publish: the caller holds the task index lock. The
    /// linear dedup scan is correct at the file backend's scale (it backs
    /// single-binary deployments); the Postgres backend enforces the same
    /// rule with a unique index instead.
    async fn enqueue_locked(
        &self,
        map: &mut HashMap<String, TaskRecord>,
        record: &TaskRecord,
    ) -> StoreResult<(TaskRecord, bool)> {
        if let Some(key) = &record.idempotency_key {
            if let Some(existing) = map
                .values()
                .find(|t| t.tenant == record.tenant && t.idempotency_key.as_deref() == Some(key))
            {
                return Ok((existing.clone(), true));
            }
        }
        tasks::persist(&self.root, record)
            .await
            .map_err(io_err("persist task"))?;
        map.insert(record.task_id.clone(), record.clone());
        Ok((record.clone(), false))
    }

    /// The cancellation/deadline finalization sweep, shared by the pool
    /// claim and the agent-mailbox claim (the caller holds the task index
    /// lock). Finalize before handing out work: a cancel request the lease
    /// holder never acknowledged, or a whole-task deadline that has passed,
    /// turns the task terminal-cancelled — never re-leased.
    async fn finalize_due_locked(
        &self,
        map: &mut HashMap<String, TaskRecord>,
        tenant: &str,
        now: chrono::DateTime<chrono::Utc>,
    ) -> StoreResult<()> {
        let due: Vec<String> = map
            .values()
            .filter(|t| t.tenant == tenant && t.cancellation_due(now))
            .map(|t| t.task_id.clone())
            .collect();
        for task_id in due {
            let mut task = map
                .get(&task_id)
                .cloned()
                .expect("finalization candidate came from the task index");
            task.apply_cancellation(now);
            tasks::persist(&self.root, &task)
                .await
                .map_err(io_err("persist task"))?;
            map.insert(task_id, task);
        }
        Ok(())
    }

    /// Shared skeleton of the lease-guarded task mutations (heartbeat /
    /// complete / fail): resolve tenant-scoped, check the lease, mutate a
    /// copy, persist, then swap the index. Persisting before the swap keeps
    /// a failed write from leaving state a restart would silently rewind.
    async fn mutate_task(
        &self,
        tenant: &str,
        task_id: &str,
        worker_id: &str,
        mutate: impl FnOnce(&mut TaskRecord),
    ) -> StoreResult<MutationOutcome> {
        let mut map = self.tasks.lock().await;
        let Some(current) = map.get(task_id) else {
            return Ok(MutationOutcome::Unknown);
        };
        // Cross-tenant ids are indistinguishable from unknown ones (404).
        if current.tenant != tenant {
            return Ok(MutationOutcome::Unknown);
        }
        if !current.leased_to(worker_id) {
            return Ok(MutationOutcome::LeaseLost);
        }
        let mut task = current.clone();
        mutate(&mut task);
        tasks::persist(&self.root, &task)
            .await
            .map_err(io_err("persist task"))?;
        map.insert(task_id.to_string(), task.clone());
        Ok(MutationOutcome::Applied(Box::new(task)))
    }
}

// --------------------------------------------------------------------- //
// PostgresStore — feature `postgres`
// --------------------------------------------------------------------- //

#[cfg(feature = "postgres")]
mod postgres {
    use chrono::{DateTime, Utc};
    use rusty_agent_runtime::checkpoint::{encode_delta, Checkpoint, DeltaPolicy};
    use rusty_agent_runtime::journal::JournalSnapshot;
    use rusty_agent_runtime::learn::{CandidateRecord, CandidateStatus, VersionPointer};
    use rusty_agent_runtime::memory::{MemoryQuery, MemoryRecord};
    use rusty_agent_runtime::record::PolicyVersion;
    use serde_json::Value;
    use sqlx::{PgPool, Row};
    use tokio::sync::OnceCell;

    use super::{CandidateTransition, ServerStore, StoreResult};
    use crate::agents::{
        self, ActivationLease, ActivationMutation, ActivationOutcome, AgentRecord, MailboxClaim,
        MailboxClaimScope,
    };
    use crate::assistants::AssistantRecord;
    use crate::coordination::CoordinationRecord;
    use crate::crons::CronRecord;
    use crate::memory;
    use crate::outbox::OutboxRecord;
    use crate::policy::{PolicyActivation, PolicyBinding, PolicyRecord, PolicyWrite};
    use crate::store::StoreItem;
    use crate::tasks::{
        self, CancelOutcome, MutationOutcome, RunCancellation, TaskLease, TaskRecord, TaskStatus,
    };
    use crate::threads::ThreadRecord;
    use crate::triggers::{TriggerEventRecord, TriggerRecord};

    // -- Schema (auto-migrated on connect) ------------------------------ //

    /// `server_assistants`: one row per assistant, whole record as JSONB.
    pub(crate) const CREATE_ASSISTANTS_SQL: &str = "
        CREATE TABLE IF NOT EXISTS server_assistants (
            assistant_id TEXT PRIMARY KEY,
            payload      JSONB NOT NULL,
            created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
        )";

    /// `server_crons`: one row per cron, whole record as JSONB.
    pub(crate) const CREATE_CRONS_SQL: &str = "
        CREATE TABLE IF NOT EXISTS server_crons (
            cron_id    TEXT PRIMARY KEY,
            payload    JSONB NOT NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT now()
        )";

    /// `server_triggers`: one row per trigger, whole record as JSONB (the
    /// assistants/crons pattern — nothing filters on trigger fields).
    pub(crate) const CREATE_TRIGGERS_SQL: &str = "
        CREATE TABLE IF NOT EXISTS server_triggers (
            trigger_id TEXT PRIMARY KEY,
            payload    JSONB NOT NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT now()
        )";

    /// `server_trigger_events`: one row per received event, whole record as
    /// JSONB; `trigger_id` is a real column because the log is listed and
    /// pruned per trigger.
    pub(crate) const CREATE_TRIGGER_EVENTS_SQL: &str = "
        CREATE TABLE IF NOT EXISTS server_trigger_events (
            event_id   TEXT PRIMARY KEY,
            trigger_id TEXT NOT NULL,
            payload    JSONB NOT NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT now()
        )";

    /// The per-trigger listing (and retention prune) scans exactly this.
    pub(crate) const CREATE_TRIGGER_EVENTS_INDEX_SQL: &str = "
        CREATE INDEX IF NOT EXISTS server_trigger_events_by_trigger
            ON server_trigger_events (trigger_id, created_at, event_id)";

    /// `server_threads`: one row per thread, whole record as JSONB.
    pub(crate) const CREATE_THREADS_SQL: &str = "
        CREATE TABLE IF NOT EXISTS server_threads (
            thread_id  TEXT PRIMARY KEY,
            payload    JSONB NOT NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT now()
        )";

    /// `server_kv`: one row per (namespace, key), JSONB value plus explicit
    /// created/updated timestamps (`created_at` preserved across replaces).
    pub(crate) const CREATE_KV_SQL: &str = r#"
        CREATE TABLE IF NOT EXISTS server_kv (
            namespace  TEXT NOT NULL,
            "key"      TEXT NOT NULL,
            value      JSONB NOT NULL,
            created_at TIMESTAMPTZ NOT NULL,
            updated_at TIMESTAMPTZ NOT NULL,
            PRIMARY KEY (namespace, "key")
        )"#;

    /// `server_journals`: one row per run, the Flight Recorder journal
    /// snapshot as JSONB (`updated_at` tracks the journal's growth across
    /// checkpoint boundaries).
    pub(crate) const CREATE_JOURNALS_SQL: &str = "
        CREATE TABLE IF NOT EXISTS server_journals (
            run_id     TEXT PRIMARY KEY,
            payload    JSONB NOT NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
            updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
        )";

    /// `server_tasks`: the durable task queue (R0.6). Unlike the record
    /// tables above this one is column-mapped, not JSONB-payloaded: claiming
    /// filters and locks on `status` / `pool` / lease columns, so they must
    /// be real columns. `status` spells [`crate::tasks::TaskStatus::as_str`].
    pub(crate) const CREATE_TASKS_SQL: &str = "
        CREATE TABLE IF NOT EXISTS server_tasks (
            task_id          TEXT PRIMARY KEY,
            tenant           TEXT NOT NULL,
            kind             TEXT NOT NULL,
            payload          JSONB NOT NULL,
            pool             TEXT NOT NULL,
            status           TEXT NOT NULL,
            lease_owner      TEXT,
            lease_expires_at TIMESTAMPTZ,
            attempt          INTEGER NOT NULL,
            max_attempts     INTEGER NOT NULL,
            error_class      TEXT,
            effect           TEXT,
            last_error       TEXT,
            idempotency_key  TEXT,
            result           JSONB,
            receipt          JSONB,
            run_id           TEXT,
            thread_id        TEXT,
            cancel_requested BOOLEAN NOT NULL DEFAULT FALSE,
            deadline         TIMESTAMPTZ,
            worker_version   TEXT,
            recipient        TEXT,
            parent           TEXT,
            tokens           JSONB,
            cost_usd         DOUBLE PRECISION,
            next_attempt_at  TIMESTAMPTZ,
            created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
            updated_at       TIMESTAMPTZ NOT NULL DEFAULT now()
        )";

    /// Enqueue dedup: at most one live task per (tenant, idempotency_key).
    /// Partial — keyless tasks (NULLs) never conflict.
    pub(crate) const CREATE_TASKS_IDEMPOTENCY_INDEX_SQL: &str = "
        CREATE UNIQUE INDEX IF NOT EXISTS server_tasks_idempotency_unique
            ON server_tasks (tenant, idempotency_key)
            WHERE idempotency_key IS NOT NULL";

    /// Claim scans filter on exactly these three columns.
    pub(crate) const CREATE_TASKS_CLAIMABLE_INDEX_SQL: &str = "
        CREATE INDEX IF NOT EXISTS server_tasks_claimable
            ON server_tasks (tenant, pool, status)";

    /// Wave-2 additive columns for databases whose `server_tasks` predates
    /// cancellation propagation (fresh databases get them from
    /// [`CREATE_TASKS_SQL`]; `ADD COLUMN IF NOT EXISTS` makes both paths
    /// converge without a versioned migration table).
    pub(crate) const ALTER_TASKS_ADD_CANCEL_REQUESTED_SQL: &str = "
        ALTER TABLE server_tasks
            ADD COLUMN IF NOT EXISTS cancel_requested BOOLEAN NOT NULL DEFAULT FALSE";

    /// See [`ALTER_TASKS_ADD_CANCEL_REQUESTED_SQL`].
    pub(crate) const ALTER_TASKS_ADD_DEADLINE_SQL: &str = "
        ALTER TABLE server_tasks
            ADD COLUMN IF NOT EXISTS deadline TIMESTAMPTZ";

    /// Wave-2b additive column for databases whose `server_tasks` predates
    /// effect receipts (fresh databases get it from [`CREATE_TASKS_SQL`]).
    /// JSONB, not TEXT: the receipt is a structured core contract
    /// ([`rusty_agent_runtime::record::EffectReceipt`]), stored verbatim.
    pub(crate) const ALTER_TASKS_ADD_RECEIPT_SQL: &str = "
        ALTER TABLE server_tasks
            ADD COLUMN IF NOT EXISTS receipt JSONB";

    /// Wave-3 additive column for databases whose `server_tasks` predates
    /// version pinning: the exact worker version string the claim path may
    /// lease the task to (`NULL` = unpinned, any worker). TEXT like the
    /// other taxonomy-adjacent columns; exact-match filtering only, so no
    /// index beyond the claimable one is needed.
    pub(crate) const ALTER_TASKS_ADD_WORKER_VERSION_SQL: &str = "
        ALTER TABLE server_tasks
            ADD COLUMN IF NOT EXISTS worker_version TEXT";

    /// R0.7 wave-1 additive column for databases whose `server_tasks`
    /// predates the agent fabric: the mailbox recipient (`agent:{id}`) the
    /// task is addressed to (`NULL` = ordinary pool work). The agent claim
    /// scans filter on `(tenant, recipient, status)` — covered by the
    /// claimable index's leading columns closely enough at wave-1 scale;
    /// a dedicated partial index can land with the scale wave.
    pub(crate) const ALTER_TASKS_ADD_RECIPIENT_SQL: &str = "
        ALTER TABLE server_tasks
            ADD COLUMN IF NOT EXISTS recipient TEXT";

    /// R0.7 wave-3 additive column for databases whose `server_tasks`
    /// predates coordination patterns: the causal parent — the journal
    /// event id the task was submitted under (`NULL` = submitted outside
    /// any journaled causality). TEXT like `run_id`/`thread_id`: it names
    /// an event, nothing filters on it, so no index.
    pub(crate) const ALTER_TASKS_ADD_PARENT_SQL: &str = "
        ALTER TABLE server_tasks
            ADD COLUMN IF NOT EXISTS parent TEXT";

    /// See [`ALTER_TASKS_ADD_PARENT_SQL`]: the settlement token usage the
    /// worker reported (`NULL` = none reported). JSONB because the value is
    /// a structured core contract ([`rusty_agent_runtime::llm::Usage`]),
    /// stored verbatim like `receipt`.
    pub(crate) const ALTER_TASKS_ADD_TOKENS_SQL: &str = "
        ALTER TABLE server_tasks
            ADD COLUMN IF NOT EXISTS tokens JSONB";

    /// See [`ALTER_TASKS_ADD_PARENT_SQL`]: the settlement monetary cost the
    /// worker reported (`NULL` = none reported). DOUBLE PRECISION matches
    /// the evidence-not-accounting rule of
    /// [`rusty_agent_runtime::record::RunEvent::cost_usd`].
    pub(crate) const ALTER_TASKS_ADD_COST_USD_SQL: &str = "
        ALTER TABLE server_tasks
            ADD COLUMN IF NOT EXISTS cost_usd DOUBLE PRECISION";

    /// `server_outbox`: the transactional outbox (R0.6 wave 2b). One row per
    /// pending task submission, 1:1 with the task it carries (`outbox_id` is
    /// the task id — re-writing the same row is a no-op). The task travels
    /// as JSONB: the relay re-inserts it column-wise into `server_tasks`,
    /// and column-mapping the outbox row itself would buy nothing — nothing
    /// filters on task fields here, only on `published_at IS NULL`.
    pub(crate) const CREATE_OUTBOX_SQL: &str = "
        CREATE TABLE IF NOT EXISTS server_outbox (
            outbox_id    TEXT PRIMARY KEY,
            tenant       TEXT NOT NULL,
            task         JSONB NOT NULL,
            published_at TIMESTAMPTZ,
            created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
        )";

    /// The relay's poll is exactly this partial index: pending rows, oldest
    /// first. Published rows fall out of the index, so the poll stays cheap
    /// no matter how much history the table holds.
    pub(crate) const CREATE_OUTBOX_PENDING_INDEX_SQL: &str = "
        CREATE INDEX IF NOT EXISTS server_outbox_pending
            ON server_outbox (created_at, outbox_id)
            WHERE published_at IS NULL";

    /// `server_agents` (R0.7 Agent Fabric, wave 1): the agent registry.
    /// Same shape discipline as `server_assistants` — the id is the primary
    /// key (tenant-scoped inside the id itself) and the record travels as
    /// one JSONB payload.
    pub(crate) const CREATE_AGENTS_SQL: &str = "
        CREATE TABLE IF NOT EXISTS server_agents (
            agent_id   TEXT PRIMARY KEY,
            payload    JSONB NOT NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT now()
        )";

    /// `server_agent_leases` (R0.7 wave 1): one row per agent's activation
    /// lease — the single-activation record. The fencing ordinal and lease
    /// columns are real columns, not JSONB: the claim/renew/release
    /// statements filter and compare on them directly, so the stale-holder
    /// guard is enforced by the database, not by read-modify-write.
    pub(crate) const CREATE_AGENT_LEASES_SQL: &str = "
        CREATE TABLE IF NOT EXISTS server_agent_leases (
            agent_id    TEXT PRIMARY KEY,
            owner       TEXT NOT NULL,
            fencing     BIGINT NOT NULL,
            expires_at  TIMESTAMPTZ NOT NULL,
            acquired_at TIMESTAMPTZ NOT NULL DEFAULT now()
        )";

    /// `server_coordinations` (R0.7 wave 3): the coordination registry.
    /// Same shape discipline as `server_agents` — the id is the primary key
    /// (tenant-scoped inside the id itself) and the record travels as one
    /// JSONB payload. Nothing filters on record fields: drives are
    /// triggered by member-task settlements and reads, both of which
    /// address the record by id.
    pub(crate) const CREATE_COORDINATIONS_SQL: &str = "
        CREATE TABLE IF NOT EXISTS server_coordinations (
            coordination_id TEXT PRIMARY KEY,
            payload         JSONB NOT NULL,
            created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
        )";

    /// `server_memory` (R0.8 Rusty Learn, wave 1): the governed memory
    /// store. Column-mapped like `server_tasks`, not JSONB-payloaded:
    /// retrieval filters on scope / kind / key / confidence / validity /
    /// expiry, so those must be real columns. The record itself travels
    /// as JSONB verbatim (artifact-referenced content form; bodies spill
    /// into core's `rusty_artifacts` table via `PostgresArtifactStore`
    /// and are re-inlined on read). `memory_id` is the tenant-scoped
    /// content address; `supersedes` carries the *bare* address (records
    /// are tenant-neutral) — the superseded set is scoped-ified in Rust,
    /// where `MemoryQuery::matches` applies it.
    pub(crate) const CREATE_MEMORY_SQL: &str = r#"
        CREATE TABLE IF NOT EXISTS server_memory (
            memory_id   TEXT PRIMARY KEY,
            tenant      TEXT NOT NULL,
            kind        TEXT NOT NULL,
            scope       TEXT NOT NULL,
            scope_id    TEXT NOT NULL,
            "key"       TEXT,
            tags        JSONB NOT NULL,
            confidence  DOUBLE PRECISION NOT NULL,
            valid_from  TIMESTAMPTZ NOT NULL,
            valid_until TIMESTAMPTZ,
            expires_at  TIMESTAMPTZ,
            supersedes  TEXT,
            payload     JSONB NOT NULL,
            created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
        )"#;

    /// The retrieval scan's leading columns: every memory query is
    /// tenant-scoped, most declare a scope address and/or kinds.
    pub(crate) const CREATE_MEMORY_QUERY_INDEX_SQL: &str = "
        CREATE INDEX IF NOT EXISTS server_memory_query
            ON server_memory (tenant, scope, scope_id, kind)";

    /// Learning candidates (R0.8 wave 3): one row per candidate record,
    /// keyed by tenant-scoped candidate id. Lifecycle columns are real
    /// columns (the transition transaction reads `status FOR UPDATE`;
    /// listings filter on surface/status); the record itself travels as
    /// JSONB, the `server_memory` discipline.
    pub(crate) const CREATE_LEARN_CANDIDATES_SQL: &str = r#"
        CREATE TABLE IF NOT EXISTS server_learn_candidates (
            candidate_id TEXT PRIMARY KEY,
            tenant       TEXT NOT NULL,
            kind         TEXT NOT NULL,
            surface      TEXT NOT NULL,
            status       TEXT NOT NULL,
            payload      JSONB NOT NULL,
            created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
        )"#;

    /// The lifecycle listing's leading columns: every candidate query is
    /// tenant-scoped, most filter on a surface or a status.
    pub(crate) const CREATE_LEARN_CANDIDATES_INDEX_SQL: &str = "
        CREATE INDEX IF NOT EXISTS server_learn_candidates_listing
            ON server_learn_candidates (tenant, surface, status)";

    /// Version pointers (R0.8 wave 3): one row per tenant-scoped
    /// surface, upserted on every promotion and rollback inside the
    /// transition's transaction.
    pub(crate) const CREATE_LEARN_VERSIONS_SQL: &str = r#"
        CREATE TABLE IF NOT EXISTS server_learn_versions (
            surface    TEXT PRIMARY KEY,
            tenant     TEXT NOT NULL,
            payload    JSONB NOT NULL,
            updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
        )"#;

    /// The executor policy registry (R0.8 wave 4): one row per immutable
    /// policy body, keyed by tenant-scoped version. Immutability is the
    /// insert's `ON CONFLICT DO NOTHING` plus a payload comparison in
    /// Rust (the file backend's rule): same body converges, different
    /// body conflicts — the table never updates.
    pub(crate) const CREATE_POLICIES_SQL: &str = r#"
        CREATE TABLE IF NOT EXISTS server_policies (
            policy_id  TEXT PRIMARY KEY,
            tenant     TEXT NOT NULL,
            version    TEXT NOT NULL,
            payload    JSONB NOT NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT now()
        )"#;

    /// The registry listing's leading column: every policy query is
    /// tenant-scoped.
    pub(crate) const CREATE_POLICIES_INDEX_SQL: &str = "
        CREATE INDEX IF NOT EXISTS server_policies_listing
            ON server_policies (tenant, version)";

    /// The activation log (R0.8 wave 4): append-only, one row per move of
    /// the active-version pointer. The serial key is the insertion order —
    /// the log's truth — and `activated_at` rides along as the record's
    /// own timestamp (the epoch derivation reads it).
    pub(crate) const CREATE_POLICY_ACTIVATIONS_SQL: &str = r#"
        CREATE TABLE IF NOT EXISTS server_policy_activations (
            id           BIGSERIAL PRIMARY KEY,
            tenant       TEXT NOT NULL,
            version      TEXT NOT NULL,
            activated_at TIMESTAMPTZ NOT NULL
        )"#;

    /// Reading the log is always tenant-scoped and ordered.
    pub(crate) const CREATE_POLICY_ACTIVATIONS_INDEX_SQL: &str = "
        CREATE INDEX IF NOT EXISTS server_policy_activations_log
            ON server_policy_activations (tenant, id)";

    /// Admission bindings (R0.8 wave 4): one row per stamped checkpoint,
    /// keyed by tenant-scoped checkpoint id. Denormalized evidence — the
    /// checkpoint header is authoritative — so a re-put of the same
    /// checkpoint's binding converges (`ON CONFLICT DO NOTHING`).
    pub(crate) const CREATE_POLICY_BINDINGS_SQL: &str = r#"
        CREATE TABLE IF NOT EXISTS server_policy_bindings (
            binding_id TEXT PRIMARY KEY,
            tenant     TEXT NOT NULL,
            thread_id  TEXT NOT NULL,
            version    TEXT NOT NULL,
            payload    JSONB NOT NULL,
            bound_at   TIMESTAMPTZ NOT NULL
        )"#;

    /// The epoch derivation's scan: one tenant's bindings by bind time.
    pub(crate) const CREATE_POLICY_BINDINGS_INDEX_SQL: &str = "
        CREATE INDEX IF NOT EXISTS server_policy_bindings_listing
            ON server_policy_bindings (tenant, bound_at)";

    /// All idempotent migration statements, executed in order on connect.
    pub(crate) const MIGRATION_SQL: &[&str] = &[
        CREATE_ASSISTANTS_SQL,
        CREATE_CRONS_SQL,
        CREATE_THREADS_SQL,
        CREATE_KV_SQL,
        CREATE_JOURNALS_SQL,
        CREATE_TASKS_SQL,
        CREATE_TASKS_IDEMPOTENCY_INDEX_SQL,
        CREATE_TASKS_CLAIMABLE_INDEX_SQL,
        ALTER_TASKS_ADD_CANCEL_REQUESTED_SQL,
        ALTER_TASKS_ADD_DEADLINE_SQL,
        ALTER_TASKS_ADD_RECEIPT_SQL,
        ALTER_TASKS_ADD_WORKER_VERSION_SQL,
        ALTER_TASKS_ADD_RECIPIENT_SQL,
        ALTER_TASKS_ADD_PARENT_SQL,
        ALTER_TASKS_ADD_TOKENS_SQL,
        ALTER_TASKS_ADD_COST_USD_SQL,
        CREATE_OUTBOX_SQL,
        CREATE_OUTBOX_PENDING_INDEX_SQL,
        CREATE_AGENTS_SQL,
        CREATE_AGENT_LEASES_SQL,
        CREATE_COORDINATIONS_SQL,
        CREATE_TRIGGERS_SQL,
        CREATE_TRIGGER_EVENTS_SQL,
        CREATE_TRIGGER_EVENTS_INDEX_SQL,
        CREATE_MEMORY_SQL,
        CREATE_MEMORY_QUERY_INDEX_SQL,
        CREATE_LEARN_CANDIDATES_SQL,
        CREATE_LEARN_CANDIDATES_INDEX_SQL,
        CREATE_LEARN_VERSIONS_SQL,
        CREATE_POLICIES_SQL,
        CREATE_POLICIES_INDEX_SQL,
        CREATE_POLICY_ACTIVATIONS_SQL,
        CREATE_POLICY_ACTIVATIONS_INDEX_SQL,
        CREATE_POLICY_BINDINGS_SQL,
        CREATE_POLICY_BINDINGS_INDEX_SQL,
    ];

    /// Transaction-scoped advisory lock key serializing concurrent
    /// first-use migrations of the server tables.
    const MIGRATION_LOCK_KEY: i64 = 0x6167_7376_5f6d_6967; // "agsv_mig"

    // -- CRUD statements ------------------------------------------------ //

    /// Insert-only assistant create; returns no row on conflict → 409.
    pub(crate) const INSERT_ASSISTANT_SQL: &str = "
        INSERT INTO server_assistants (assistant_id, payload)
        VALUES ($1, $2)
        ON CONFLICT (assistant_id) DO NOTHING
        RETURNING assistant_id";

    pub(crate) const SELECT_ASSISTANT_SQL: &str =
        "SELECT payload FROM server_assistants WHERE assistant_id = $1";

    pub(crate) const LIST_ASSISTANTS_SQL: &str = "SELECT payload FROM server_assistants";

    /// Insert-only cron create; returns no row on conflict → 409.
    pub(crate) const INSERT_CRON_SQL: &str = "
        INSERT INTO server_crons (cron_id, payload)
        VALUES ($1, $2)
        ON CONFLICT (cron_id) DO NOTHING
        RETURNING cron_id";

    /// Full upsert for scheduler bookkeeping.
    pub(crate) const UPSERT_CRON_SQL: &str = "
        INSERT INTO server_crons (cron_id, payload)
        VALUES ($1, $2)
        ON CONFLICT (cron_id) DO UPDATE SET payload = EXCLUDED.payload";

    pub(crate) const SELECT_CRON_SQL: &str = "SELECT payload FROM server_crons WHERE cron_id = $1";

    pub(crate) const LIST_CRONS_SQL: &str = "SELECT payload FROM server_crons";

    pub(crate) const DELETE_CRON_SQL: &str = "DELETE FROM server_crons WHERE cron_id = $1";

    /// Insert-only trigger create; returns no row on conflict → 409.
    pub(crate) const INSERT_TRIGGER_SQL: &str = "
        INSERT INTO server_triggers (trigger_id, payload)
        VALUES ($1, $2)
        ON CONFLICT (trigger_id) DO NOTHING
        RETURNING trigger_id";

    /// Full upsert for updates and bookkeeping counters.
    pub(crate) const UPSERT_TRIGGER_SQL: &str = "
        INSERT INTO server_triggers (trigger_id, payload)
        VALUES ($1, $2)
        ON CONFLICT (trigger_id) DO UPDATE SET payload = EXCLUDED.payload";

    pub(crate) const SELECT_TRIGGER_SQL: &str =
        "SELECT payload FROM server_triggers WHERE trigger_id = $1";

    pub(crate) const LIST_TRIGGERS_SQL: &str = "SELECT payload FROM server_triggers";

    pub(crate) const DELETE_TRIGGER_SQL: &str = "DELETE FROM server_triggers WHERE trigger_id = $1";

    pub(crate) const DELETE_TRIGGER_EVENTS_SQL: &str =
        "DELETE FROM server_trigger_events WHERE trigger_id = $1";

    /// Event append, upserting on event id so debounce status transitions
    /// (`pending` → `coalesced`/`failed`) rewrite the row in place.
    pub(crate) const UPSERT_TRIGGER_EVENT_SQL: &str = "
        INSERT INTO server_trigger_events (event_id, trigger_id, payload, created_at)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (event_id) DO UPDATE SET payload = EXCLUDED.payload";

    pub(crate) const SELECT_TRIGGER_EVENT_SQL: &str =
        "SELECT payload FROM server_trigger_events WHERE trigger_id = $1 AND event_id = $2";

    pub(crate) const LIST_TRIGGER_EVENTS_SQL: &str = "
        SELECT payload FROM server_trigger_events
        WHERE trigger_id = $1 ORDER BY created_at, event_id";

    /// Retention: keep the newest 256 events per trigger
    /// ([`crate::triggers::MAX_EVENTS_PER_TRIGGER`]); the file backend
    /// enforces the same cap in Rust.
    pub(crate) const PRUNE_TRIGGER_EVENTS_SQL: &str = "
        DELETE FROM server_trigger_events
        WHERE trigger_id = $1 AND event_id NOT IN (
            SELECT event_id FROM server_trigger_events
            WHERE trigger_id = $1
            ORDER BY created_at DESC, event_id DESC
            LIMIT 256
        )";

    /// Insert-only thread create; returns no row on conflict → 409.
    pub(crate) const INSERT_THREAD_SQL: &str = "
        INSERT INTO server_threads (thread_id, payload)
        VALUES ($1, $2)
        ON CONFLICT (thread_id) DO NOTHING
        RETURNING thread_id";

    pub(crate) const SELECT_THREAD_SQL: &str =
        "SELECT payload FROM server_threads WHERE thread_id = $1";

    /// KV upsert that preserves `created_at` on replace and reports whether
    /// the row pre-existed (the `created` flag drives 201 vs 200).
    pub(crate) const UPSERT_KV_SQL: &str = r#"
        WITH existing AS (
            SELECT created_at FROM server_kv WHERE namespace = $1 AND "key" = $2
        ), upserted AS (
            INSERT INTO server_kv (namespace, "key", value, created_at, updated_at)
            VALUES ($1, $2, $3, $4, $5)
            ON CONFLICT (namespace, "key") DO UPDATE
                SET value = EXCLUDED.value, updated_at = EXCLUDED.updated_at
            RETURNING created_at, updated_at
        )
        SELECT u.created_at, u.updated_at, (e.created_at IS NULL) AS created
        FROM upserted u LEFT JOIN existing e ON TRUE"#;

    pub(crate) const SELECT_KV_SQL: &str = r#"
        SELECT value, created_at, updated_at
        FROM server_kv WHERE namespace = $1 AND "key" = $2"#;

    pub(crate) const DELETE_KV_SQL: &str =
        r#"DELETE FROM server_kv WHERE namespace = $1 AND "key" = $2"#;

    pub(crate) const LIST_KV_SQL: &str = r#"
        SELECT "key", value, created_at, updated_at
        FROM server_kv WHERE namespace = $1 ORDER BY "key""#;

    /// Journal upsert: the snapshot is rewritten at every checkpoint
    /// boundary, so `updated_at` moves while `created_at` is preserved.
    pub(crate) const UPSERT_JOURNAL_SQL: &str = "
        INSERT INTO server_journals (run_id, payload)
        VALUES ($1, $2)
        ON CONFLICT (run_id) DO UPDATE
            SET payload = EXCLUDED.payload, updated_at = now()";

    pub(crate) const SELECT_JOURNAL_SQL: &str =
        "SELECT payload FROM server_journals WHERE run_id = $1";

    // -- Task queue statements (R0.6) ------------------------------------ //

    /// Insert-only enqueue; `ON CONFLICT DO NOTHING` absorbs both the
    /// (effectively impossible) task-id collision and the idempotency-key
    /// dedup — a no-row result with a key set means *deduplicated*.
    pub(crate) const INSERT_TASK_SQL: &str = "
        INSERT INTO server_tasks (
            task_id, tenant, kind, payload, pool, status,
            attempt, max_attempts, error_class, effect, idempotency_key,
            run_id, thread_id, deadline, worker_version, recipient, parent, next_attempt_at, created_at, updated_at
        ) VALUES (
            $1, $2, $3, $4, $5, 'queued', 0, $6, NULL, $7, $8, $9, $10, $11, $12, $13, $15, NULL, $14, $14
        )
        ON CONFLICT DO NOTHING
        RETURNING task_id";

    /// The dedup read-back after an absorbed idempotency conflict.
    pub(crate) const SELECT_TASK_BY_IDEMPOTENCY_SQL: &str = "
        SELECT task_id, tenant, kind, payload, pool, status, lease_owner, lease_expires_at, \
            attempt, max_attempts, error_class, effect, last_error, idempotency_key, result, \
            run_id, thread_id, cancel_requested, deadline, receipt, worker_version, recipient, parent, tokens, cost_usd, next_attempt_at, created_at, updated_at
        FROM server_tasks
        WHERE tenant = $1 AND idempotency_key = $2";

    /// Claim finalization, run before [`CLAIM_SELECT_SQL`] in the same
    /// transaction: a cancel request the lease holder never acknowledged
    /// (its lease lapsed) or a whole-task deadline that has passed makes
    /// the task terminal-cancelled instead of re-leasable. Leased tasks
    /// with a live lease are the worker's concern (it reports the
    /// expired deadline as cancelled through the fail path).
    pub(crate) const CLAIM_FINALIZE_SQL: &str = "
        UPDATE server_tasks
        SET status = 'cancelled', error_class = 'cancelled', lease_owner = NULL,
            lease_expires_at = NULL, next_attempt_at = NULL, updated_at = $2
        WHERE tenant = $1
          AND (cancel_requested OR (deadline IS NOT NULL AND deadline <= $2))
          AND (
              status = 'queued'
              OR (status = 'failed' AND next_attempt_at IS NOT NULL AND next_attempt_at <= $2)
              OR (status = 'leased' AND lease_expires_at <= $2)
          )";

    /// Claim candidate selection, run inside a transaction: `FOR UPDATE
    /// SKIP LOCKED` makes concurrent workers take distinct tasks without
    /// blocking each other. Claimable = queued, backoff-elapsed failed, or
    /// leased past its visibility timeout.
    ///
    /// Wave-3 placement predicates: `$4` is the list of pools already at
    /// their configured concurrency limit (computed by
    /// [`CLAIM_INFLIGHT_SQL`] in the same transaction), excluded so one
    /// saturated pool never starves the others; `$5` is the worker's
    /// advertised version, matched exactly against the task's pin — NULL
    /// (unpinned) tasks match everyone, a NULL advertisement matches only
    /// unpinned tasks. This is the SQL spelling of
    /// [`crate::tasks::TaskRecord::matches_worker_version`]; the two must
    /// agree.
    ///
    /// R0.7: mailbox traffic (`recipient` set) is excluded — it drains only
    /// through the turn-serialized agent claim
    /// ([`AGENT_CLAIM_SELECT_SQL`]), never through a pool.
    pub(crate) const CLAIM_SELECT_SQL: &str = "
        SELECT task_id, attempt FROM server_tasks
        WHERE tenant = $1
          AND pool = ANY($2)
          AND recipient IS NULL
          AND NOT (pool = ANY($4))
          AND (worker_version IS NULL OR worker_version = $5)
          AND (
              (status IN ('queued', 'failed')
                  AND (next_attempt_at IS NULL OR next_attempt_at <= $3))
              OR (status = 'leased' AND lease_expires_at <= $3)
          )
        ORDER BY created_at, task_id
        LIMIT 1
        FOR UPDATE SKIP LOCKED";

    /// Live-lease counts per pool, run before [`CLAIM_SELECT_SQL`] in the
    /// same transaction: pools at their configured limit go into `$4` of
    /// the candidate select. Only *unexpired* leases hold capacity — an
    /// expired lease's task is visible again. Restricted to the limited
    /// pools (`$3`): counting the whole tenant's leases on every claim
    /// would make unconfigured pools pay for limits they never asked for.
    pub(crate) const CLAIM_INFLIGHT_SQL: &str = "
        SELECT pool, COUNT(*) AS live FROM server_tasks
        WHERE tenant = $1 AND status = 'leased' AND lease_expires_at > $2 AND pool = ANY($3)
        GROUP BY pool";

    /// The claim itself, applied to the row locked by [`CLAIM_SELECT_SQL`]
    /// in the same transaction.
    pub(crate) const CLAIM_UPDATE_SQL: &str = "
        UPDATE server_tasks
        SET lease_owner = $2, lease_expires_at = $3, attempt = $4,
            status = 'leased', next_attempt_at = NULL, updated_at = $5
        WHERE task_id = $1
        RETURNING task_id, tenant, kind, payload, pool, status, lease_owner, lease_expires_at, \
            attempt, max_attempts, error_class, effect, last_error, idempotency_key, result, \
            run_id, thread_id, cancel_requested, deadline, receipt, worker_version, recipient, parent, tokens, cost_usd, next_attempt_at, created_at, updated_at";

    /// Heartbeat: extends the lease only while the caller holds it. No row
    /// means unknown/cross-tenant (404) or lease lost (409), distinguished
    /// by [`TASK_EXISTS_SQL`]. The returned row carries `cancel_requested`
    /// so the route can surface a pending cancellation to the holder.
    pub(crate) const HEARTBEAT_TASK_SQL: &str = "
        UPDATE server_tasks
        SET lease_expires_at = $4, updated_at = $5
        WHERE task_id = $1 AND tenant = $2 AND lease_owner = $3 AND status = 'leased'
        RETURNING task_id, tenant, kind, payload, pool, status, lease_owner, lease_expires_at, \
            attempt, max_attempts, error_class, effect, last_error, idempotency_key, result, \
            run_id, thread_id, cancel_requested, deadline, receipt, worker_version, recipient, parent, tokens, cost_usd, next_attempt_at, created_at, updated_at";

    /// Complete: settle only the caller's own lease, storing the result,
    /// the effect receipt (`$6`, JSONB — `NULL` when none), and the
    /// settlement cost evidence (`$7` JSONB usage, `$8` cost — `NULL`s
    /// when the worker reported nothing) (R0.7 wave 3).
    pub(crate) const COMPLETE_TASK_SQL: &str = "
        UPDATE server_tasks
        SET status = 'completed', result = $4, lease_owner = NULL,
            lease_expires_at = NULL, next_attempt_at = NULL, updated_at = $5,
            receipt = $6, tokens = $7, cost_usd = $8
        WHERE task_id = $1 AND tenant = $2 AND lease_owner = $3 AND status = 'leased'
        RETURNING task_id, tenant, kind, payload, pool, status, lease_owner, lease_expires_at, \
            attempt, max_attempts, error_class, effect, last_error, idempotency_key, result, \
            run_id, thread_id, cancel_requested, deadline, receipt, worker_version, recipient, parent, tokens, cost_usd, next_attempt_at, created_at, updated_at";

    /// Fail, step 1: lock the row (the requeue-vs-dead decision needs the
    /// current attempt count, and concurrent settlement must serialize).
    pub(crate) const FAIL_SELECT_SQL: &str = "
        SELECT task_id, tenant, kind, payload, pool, status, lease_owner, lease_expires_at, \
            attempt, max_attempts, error_class, effect, last_error, idempotency_key, result, \
            run_id, thread_id, cancel_requested, deadline, receipt, worker_version, recipient, parent, tokens, cost_usd, next_attempt_at, created_at, updated_at
        FROM server_tasks
        WHERE task_id = $1 AND tenant = $2
        FOR UPDATE";

    /// Fail, step 2: apply the decision computed in Rust
    /// ([`crate::tasks::TaskRecord::fail`]) to the locked row, including
    /// the settlement cost evidence (`$7` JSONB usage, `$8` cost) the
    /// worker reported with the failure (R0.7 wave 3).
    pub(crate) const FAIL_UPDATE_SQL: &str = "
        UPDATE server_tasks
        SET status = $2, error_class = $3, last_error = $4,
            lease_owner = NULL, lease_expires_at = NULL,
            next_attempt_at = $5, updated_at = $6, tokens = $7, cost_usd = $8
        WHERE task_id = $1
        RETURNING task_id, tenant, kind, payload, pool, status, lease_owner, lease_expires_at, \
            attempt, max_attempts, error_class, effect, last_error, idempotency_key, result, \
            run_id, thread_id, cancel_requested, deadline, receipt, worker_version, recipient, parent, tokens, cost_usd, next_attempt_at, created_at, updated_at";

    /// Cancel, step 1: lock the row (the terminal check and the transition
    /// must serialize against claims and settlements).
    pub(crate) const CANCEL_SELECT_SQL: &str = "
        SELECT task_id, tenant, kind, payload, pool, status, lease_owner, lease_expires_at, \
            attempt, max_attempts, error_class, effect, last_error, idempotency_key, result, \
            run_id, thread_id, cancel_requested, deadline, receipt, worker_version, recipient, parent, tokens, cost_usd, next_attempt_at, created_at, updated_at
        FROM server_tasks
        WHERE task_id = $1 AND tenant = $2
        FOR UPDATE";

    /// Cancel, step 2: apply the transition computed in Rust
    /// ([`crate::tasks::TaskRecord::cancel`]) to the locked row. The lease
    /// columns are bound from the mutated record — cleared for the
    /// immediate transition, kept for the signal-a-leased-holder one.
    pub(crate) const CANCEL_UPDATE_SQL: &str = "
        UPDATE server_tasks
        SET status = $2, error_class = $3, cancel_requested = $4,
            lease_owner = $5, lease_expires_at = $6,
            next_attempt_at = $7, updated_at = $8
        WHERE task_id = $1
        RETURNING task_id, tenant, kind, payload, pool, status, lease_owner, lease_expires_at, \
            attempt, max_attempts, error_class, effect, last_error, idempotency_key, result, \
            run_id, thread_id, cancel_requested, deadline, receipt, worker_version, recipient, parent, tokens, cost_usd, next_attempt_at, created_at, updated_at";

    /// Run cancel, part 1: the run's non-leased, non-terminal tasks move to
    /// the terminal `cancelled` state immediately.
    pub(crate) const CANCEL_RUN_FINALIZE_SQL: &str = "
        UPDATE server_tasks
        SET status = 'cancelled', error_class = 'cancelled', lease_owner = NULL,
            lease_expires_at = NULL, next_attempt_at = NULL, updated_at = $3
        WHERE tenant = $1 AND run_id = $2
          AND (status = 'queued' OR (status = 'failed' AND next_attempt_at IS NOT NULL))
        RETURNING task_id, tenant, kind, payload, pool, status, lease_owner, lease_expires_at, \
            attempt, max_attempts, error_class, effect, last_error, idempotency_key, result, \
            run_id, thread_id, cancel_requested, deadline, receipt, worker_version, recipient, parent, tokens, cost_usd, next_attempt_at, created_at, updated_at";

    /// Run cancel, part 2: the run's leased tasks keep their leases and
    /// get `cancel_requested` set — their holders learn on the next
    /// heartbeat and report the attempt as cancelled.
    pub(crate) const CANCEL_RUN_SIGNAL_SQL: &str = "
        UPDATE server_tasks
        SET cancel_requested = TRUE, updated_at = $3
        WHERE tenant = $1 AND run_id = $2 AND status = 'leased'
        RETURNING task_id, tenant, kind, payload, pool, status, lease_owner, lease_expires_at, \
            attempt, max_attempts, error_class, effect, last_error, idempotency_key, result, \
            run_id, thread_id, cancel_requested, deadline, receipt, worker_version, recipient, parent, tokens, cost_usd, next_attempt_at, created_at, updated_at";

    /// Tenant-scoped existence probe distinguishing 404 from 409 after a
    /// lease-guarded update matched no row.
    pub(crate) const TASK_EXISTS_SQL: &str =
        "SELECT task_id FROM server_tasks WHERE task_id = $1 AND tenant = $2";

    pub(crate) const SELECT_TASK_SQL: &str =
        "SELECT task_id, tenant, kind, payload, pool, status, lease_owner, lease_expires_at, \
            attempt, max_attempts, error_class, effect, last_error, idempotency_key, result, \
            run_id, thread_id, cancel_requested, deadline, receipt, worker_version, recipient, parent, tokens, cost_usd, next_attempt_at, created_at, updated_at
        FROM server_tasks WHERE task_id = $1 AND tenant = $2";

    pub(crate) const LIST_TASKS_SQL: &str = "
        SELECT task_id, tenant, kind, payload, pool, status, lease_owner, lease_expires_at, \
            attempt, max_attempts, error_class, effect, last_error, idempotency_key, result, \
            run_id, thread_id, cancel_requested, deadline, receipt, worker_version, recipient, parent, tokens, cost_usd, next_attempt_at, created_at, updated_at
        FROM server_tasks
        WHERE tenant = $1 ORDER BY created_at, task_id";

    pub(crate) const LIST_TASKS_BY_STATUS_SQL: &str = "
        SELECT task_id, tenant, kind, payload, pool, status, lease_owner, lease_expires_at, \
            attempt, max_attempts, error_class, effect, last_error, idempotency_key, result, \
            run_id, thread_id, cancel_requested, deadline, receipt, worker_version, recipient, parent, tokens, cost_usd, next_attempt_at, created_at, updated_at
        FROM server_tasks
        WHERE tenant = $1 AND status = $2 ORDER BY created_at, task_id";

    /// The tenant's queue pressure (R0.6 wave 3) in one statement: the
    /// three gauges [`crate::tasks::TaskUsage`] defines — backlog (queued +
    /// retry-scheduled + pending outbox rows), in flight (status `leased`),
    /// DLQ depth. `FILTER` keeps it one pass over the tenant's rows, and the
    /// outbox count rides along as a subquery so the whole read is a single
    /// MVCC snapshot: two statements would let a relay publish slip between
    /// them and the moving row would be counted by neither.
    pub(crate) const TASK_USAGE_SQL: &str = "
        SELECT
            COUNT(*) FILTER (WHERE status = 'queued'
                OR (status = 'failed' AND next_attempt_at IS NOT NULL)) AS queued,
            COUNT(*) FILTER (WHERE status = 'leased') AS in_flight,
            COUNT(*) FILTER (WHERE status = 'dead') AS dlq,
            (SELECT COUNT(*) FROM server_outbox
                WHERE tenant = $1 AND published_at IS NULL) AS pending_outbox
        FROM server_tasks
        WHERE tenant = $1";

    /// Per-pool autoscaling signals (R0.6 wave 3) for `GET /tasks/metrics`,
    /// in one grouped scan at `$2` = now. Mirrors the JSON backend's
    /// `task_pool_stats` exactly: backlog counts due *and* scheduled
    /// retries; `leased` counts only unexpired leases (the saturation
    /// numerator); `oldest_visible_at` is the oldest task a claim right now
    /// would hand out. Pools with no rows are absent — the route adds
    /// configured-but-empty pools itself.
    pub(crate) const POOL_STATS_SQL: &str = "
        SELECT
            pool,
            COUNT(*) FILTER (WHERE status = 'queued'
                OR (status = 'failed' AND next_attempt_at IS NOT NULL)) AS queue_depth,
            COUNT(*) FILTER (WHERE status = 'leased' AND lease_expires_at > $2) AS leased,
            MIN(created_at) FILTER (WHERE status = 'queued'
                OR (status = 'failed' AND next_attempt_at IS NOT NULL AND next_attempt_at <= $2)
                OR (status = 'leased' AND lease_expires_at <= $2)) AS oldest_visible_at
        FROM server_tasks
        WHERE tenant = $1
        GROUP BY pool
        ORDER BY pool";

    // -- Transactional outbox statements (R0.6 wave 2b) ------------------ //

    /// Insert-only outbox write; `ON CONFLICT (outbox_id) DO NOTHING` makes
    /// a retried outbox enqueue (or a retried checkpoint+enqueue pair whose
    /// checkpoint already committed) a no-op returning the pending row.
    pub(crate) const INSERT_OUTBOX_SQL: &str = "
        INSERT INTO server_outbox (outbox_id, tenant, task, created_at)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (outbox_id) DO NOTHING
        RETURNING outbox_id";

    /// Read-back after an absorbed outbox conflict.
    pub(crate) const SELECT_OUTBOX_BY_ID_SQL: &str = "
        SELECT outbox_id, tenant, task, published_at, created_at
        FROM server_outbox WHERE outbox_id = $1";

    /// The relay's pick: the oldest pending row, locked `FOR UPDATE SKIP
    /// LOCKED` inside the publish transaction — concurrent relay instances
    /// (or a manual pump racing the background relay) take distinct rows
    /// instead of double-publishing one. One row per transaction: the task
    /// insert and the mark-published commit or roll back together, so a
    /// crash mid-batch loses nothing and a poisoned row stalls only itself.
    pub(crate) const SELECT_OUTBOX_PENDING_SQL: &str = "
        SELECT outbox_id, tenant, task, published_at, created_at
        FROM server_outbox
        WHERE published_at IS NULL
        ORDER BY created_at, outbox_id
        LIMIT 1
        FOR UPDATE SKIP LOCKED";

    /// Mark a row published, applied in the same transaction as its task
    /// insert ([`INSERT_TASK_SQL`]).
    pub(crate) const MARK_OUTBOX_PUBLISHED_SQL: &str = "
        UPDATE server_outbox SET published_at = $2 WHERE outbox_id = $1";

    /// The checkpoint half of `checkpoint_and_enqueue`. This is the same
    /// insert core's `PostgresCheckpointer` runs (no `ON CONFLICT`: a
    /// duplicate id must abort the transaction — that abort is precisely
    /// what keeps the checkpoint and the outbox rows atomic together); it
    /// lives here because a transaction cannot span the two stores'
    /// connection pools. The W4 `base` column carries the delta-chain link
    /// the same way core's insert does (see `checkpoint_and_enqueue` for
    /// the encoding decision).
    pub(crate) const INSERT_CHECKPOINT_SQL: &str = "
        INSERT INTO rusty_checkpoints
            (thread_id, checkpoint_id, step, state, next_nodes, created_at, header, journal_ref, base)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)";

    // -- Agent fabric statements (R0.7, wave 1) -------------------------- //

    /// Insert-only agent create; returns no row on conflict → 409 (the
    /// assistants convention).
    pub(crate) const INSERT_AGENT_SQL: &str = "
        INSERT INTO server_agents (agent_id, payload)
        VALUES ($1, $2)
        ON CONFLICT (agent_id) DO NOTHING
        RETURNING agent_id";

    pub(crate) const SELECT_AGENT_SQL: &str =
        "SELECT payload FROM server_agents WHERE agent_id = $1";

    /// Whole-payload overwrite for the supervision state (R0.7 wave 2):
    /// the record travels as one JSONB payload, so the update does too —
    /// no column surgery, and the additive wave-2 fields need no
    /// migration. Last-writer-wins under the turn protocol's serialization
    /// (see the trait contract).
    pub(crate) const UPDATE_AGENT_SQL: &str = "
        UPDATE server_agents SET payload = $2
        WHERE agent_id = $1
        RETURNING agent_id";

    /// Coordination registry statements (R0.7 wave 3) — the
    /// `server_agents` discipline (payload JSONB, id primary key) applied
    /// to `server_coordinations`.
    pub(crate) const INSERT_COORDINATION_SQL: &str = "
        INSERT INTO server_coordinations (coordination_id, payload)
        VALUES ($1, $2)
        ON CONFLICT DO NOTHING
        RETURNING coordination_id";

    pub(crate) const SELECT_COORDINATION_SQL: &str =
        "SELECT payload FROM server_coordinations WHERE coordination_id = $1";

    /// Governed memory statements (R0.8 wave 1). Insert-only on the
    /// content address (writes are idempotent by construction); the
    /// `server_agents` conflict discipline applied to `server_memory`.
    pub(crate) const INSERT_MEMORY_SQL: &str = r#"
        INSERT INTO server_memory (
            memory_id, tenant, kind, scope, scope_id, "key", tags, confidence,
            valid_from, valid_until, expires_at, supersedes, payload
        ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
        ON CONFLICT (memory_id) DO NOTHING
        RETURNING memory_id"#;

    pub(crate) const SELECT_MEMORY_SQL: &str =
        "SELECT payload FROM server_memory WHERE memory_id = $1";

    /// The superseded set spans the tenant's whole namespace: a
    /// superseding record may itself fall outside a query's filters, and
    /// the record it supersedes must still drop out of default
    /// retrieval. `supersedes` carries the bare content address (the
    /// record's own identity), so the set applies directly to
    /// `MemoryQuery::matches` in Rust.
    pub(crate) const SUPERSEDED_MEMORY_SQL: &str =
        "SELECT supersedes FROM server_memory WHERE tenant = $1 AND supersedes IS NOT NULL";

    /// The summary-source half of the superseded set (R0.8 wave 2): a
    /// `summary` record supersedes the records it names in
    /// `provenance.evidence.source_memory_ids` — core's
    /// [`superseded_set`](rusty_agent_runtime::memory::superseded_set)
    /// rule, and the same naming dependent-summary invalidation walks on
    /// forgetting. Evidence is not column-mapped, so the ids come out of
    /// the JSONB payload (`#>>` yields the array's text form, parsed back
    /// in Rust); `kind = 'summary'` keeps the scan exact against the
    /// `server_memory_query` index.
    pub(crate) const SUMMARY_SOURCES_MEMORY_SQL: &str = r#"
        SELECT payload #>> '{provenance,evidence,source_memory_ids}' AS source_ids
        FROM server_memory
        WHERE tenant = $1 AND kind = 'summary'"#;

    /// Forgetting (R0.8 wave 2): real deletion of derived state, scoped by
    /// the tenant-prefixed id. The spilled blob in `rusty_artifacts`
    /// stays — shared, content-addressed evidence under the
    /// journal-erasure boundary (open question 4).
    pub(crate) const DELETE_MEMORY_SQL: &str =
        "DELETE FROM server_memory WHERE memory_id = $1 RETURNING memory_id";

    /// Learning-candidate statements (R0.8 wave 3). Creation is
    /// insert-only on the tenant-scoped candidate id (content addressing
    /// makes the create converge); lifecycle moves go through the
    /// transition pair below.
    pub(crate) const INSERT_LEARN_CANDIDATE_SQL: &str = r#"
        INSERT INTO server_learn_candidates (
            candidate_id, tenant, kind, surface, status, payload
        ) VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT (candidate_id) DO NOTHING
        RETURNING candidate_id"#;

    pub(crate) const SELECT_LEARN_CANDIDATE_SQL: &str =
        "SELECT payload FROM server_learn_candidates WHERE candidate_id = $1";

    pub(crate) const LIST_LEARN_CANDIDATES_SQL: &str =
        "SELECT payload FROM server_learn_candidates WHERE tenant = $1";

    /// Transition, step 1: lock the row — the status check and the
    /// update must serialize against a concurrent transition (the task
    /// cancel pair's discipline).
    pub(crate) const LOCK_LEARN_CANDIDATE_SQL: &str =
        "SELECT status FROM server_learn_candidates WHERE candidate_id = $1 FOR UPDATE";

    /// Transition, step 2: apply the new status and record to the locked
    /// row.
    pub(crate) const UPDATE_LEARN_CANDIDATE_SQL: &str =
        "UPDATE server_learn_candidates SET status = $2, payload = $3 WHERE candidate_id = $1";

    /// The pointer half of the transition: upsert the surface's pointer
    /// in the same transaction, so a promoted candidate whose pointer
    /// never moved (or the inverse) is not a reachable state.
    pub(crate) const UPSERT_LEARN_VERSION_SQL: &str = r#"
        INSERT INTO server_learn_versions (surface, tenant, payload)
        VALUES ($1, $2, $3)
        ON CONFLICT (surface) DO UPDATE SET payload = $3, updated_at = now()"#;

    pub(crate) const SELECT_LEARN_VERSION_SQL: &str =
        "SELECT payload FROM server_learn_versions WHERE surface = $1";

    pub(crate) const LIST_LEARN_VERSIONS_SQL: &str =
        "SELECT payload FROM server_learn_versions WHERE tenant = $1";

    /// Policy registry statements (R0.8 wave 4). Registration is
    /// insert-only on the tenant-scoped version (immutability: a conflict
    /// returns no row, and the route-level comparison decides converge
    /// vs conflict); the table never updates.
    pub(crate) const INSERT_POLICY_SQL: &str = r#"
        INSERT INTO server_policies (policy_id, tenant, version, payload)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (policy_id) DO NOTHING
        RETURNING policy_id"#;

    pub(crate) const SELECT_POLICY_SQL: &str =
        "SELECT payload FROM server_policies WHERE policy_id = $1";

    pub(crate) const LIST_POLICIES_SQL: &str =
        "SELECT payload FROM server_policies WHERE tenant = $1";

    /// The activation log: plain appends; the serial key carries the
    /// insertion order the listing reads back.
    pub(crate) const INSERT_POLICY_ACTIVATION_SQL: &str = r#"
        INSERT INTO server_policy_activations (tenant, version, activated_at)
        VALUES ($1, $2, $3)"#;

    pub(crate) const LIST_POLICY_ACTIVATIONS_SQL: &str = r#"
        SELECT version, activated_at FROM server_policy_activations
        WHERE tenant = $1
        ORDER BY id"#;

    /// Bindings converge on the checkpoint id (a re-put is the same fact).
    pub(crate) const INSERT_POLICY_BINDING_SQL: &str = r#"
        INSERT INTO server_policy_bindings (
            binding_id, tenant, thread_id, version, payload, bound_at
        ) VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT (binding_id) DO NOTHING"#;

    pub(crate) const LIST_POLICY_BINDINGS_SQL: &str =
        "SELECT payload FROM server_policy_bindings WHERE tenant = $1";

    pub(crate) const UPDATE_COORDINATION_SQL: &str = "
        UPDATE server_coordinations SET payload = $2
        WHERE coordination_id = $1
        RETURNING coordination_id";

    /// Agent cancel (R0.7 wave 2), part 1: the mailbox's queued and
    /// retry-scheduled messages go terminal-`cancelled` immediately. The
    /// recipient-scoped twin of [`CANCEL_RUN_FINALIZE_SQL`].
    pub(crate) const CANCEL_AGENT_FINALIZE_SQL: &str = "
        UPDATE server_tasks
        SET status = 'cancelled', error_class = 'cancelled', lease_owner = NULL,
            lease_expires_at = NULL, next_attempt_at = NULL, updated_at = $3
        WHERE tenant = $1 AND recipient = $2
          AND (status = 'queued' OR (status = 'failed' AND next_attempt_at IS NOT NULL))
        RETURNING task_id, tenant, kind, payload, pool, status, lease_owner, lease_expires_at, \
            attempt, max_attempts, error_class, effect, last_error, idempotency_key, result, \
            run_id, thread_id, cancel_requested, deadline, receipt, worker_version, recipient, parent, tokens, cost_usd, next_attempt_at, created_at, updated_at";

    /// Agent cancel, part 2: the mailbox's leased turn keeps its lease and
    /// gets `cancel_requested` set — its holder learns on the next
    /// heartbeat and reports the attempt as cancelled. The recipient-scoped
    /// twin of [`CANCEL_RUN_SIGNAL_SQL`].
    pub(crate) const CANCEL_AGENT_SIGNAL_SQL: &str = "
        UPDATE server_tasks
        SET cancel_requested = TRUE, updated_at = $3
        WHERE tenant = $1 AND recipient = $2 AND status = 'leased'
        RETURNING task_id, tenant, kind, payload, pool, status, lease_owner, lease_expires_at, \
            attempt, max_attempts, error_class, effect, last_error, idempotency_key, result, \
            run_id, thread_id, cancel_requested, deadline, receipt, worker_version, recipient, parent, tokens, cost_usd, next_attempt_at, created_at, updated_at";

    /// The runtime's direct DLQ write (R0.7 wave 2): a task inserted
    /// terminal-`dead` with its evidence (`last_error`, payload) and no
    /// attempt history — supervision's root escalation is the producer.
    /// The idempotency unique index makes a retried escalation a no-op,
    /// read back by key like [`INSERT_TASK_SQL`]'s dedup.
    pub(crate) const INSERT_DEAD_LETTER_SQL: &str = "
        INSERT INTO server_tasks (
            task_id, tenant, kind, payload, pool, status,
            attempt, max_attempts, error_class, effect, idempotency_key, last_error,
            run_id, thread_id, deadline, worker_version, recipient, next_attempt_at, created_at, updated_at
        ) VALUES (
            $1, $2, $3, $4, $5, 'dead', 0, $6, NULL, NULL, $7, $8, NULL, NULL, NULL, NULL, NULL, NULL, $9, $9
        )
        ON CONFLICT DO NOTHING
        RETURNING task_id";

    pub(crate) const LIST_AGENTS_SQL: &str = "SELECT payload FROM server_agents";

    /// The activation lease row, locked `FOR UPDATE`: the claim's
    /// insert-or-steal decision and the mailbox claim's activation gate
    /// both serialize on this lock inside their transactions, so two
    /// racing claimants (or two concurrent turns by one holder) can never
    /// both pass.
    pub(crate) const SELECT_ACTIVATION_FOR_UPDATE_SQL: &str = "
        SELECT agent_id, owner, fencing, expires_at, acquired_at
        FROM server_agent_leases
        WHERE agent_id = $1
        FOR UPDATE";

    pub(crate) const SELECT_ACTIVATION_SQL: &str = "
        SELECT agent_id, owner, fencing, expires_at, acquired_at
        FROM server_agent_leases
        WHERE agent_id = $1";

    /// The activation claim's insert half (no existing row): fencing
    /// starts at 1.
    pub(crate) const INSERT_ACTIVATION_SQL: &str = "
        INSERT INTO server_agent_leases (agent_id, owner, fencing, expires_at, acquired_at)
        VALUES ($1, $2, 1, $3, $4)
        RETURNING agent_id, owner, fencing, expires_at, acquired_at";

    /// The activation claim's steal half, applied to the row locked by
    /// [`SELECT_ACTIVATION_FOR_UPDATE_SQL`]: the expired holder's fencing
    /// ordinal is bumped, so the dead host's stale pair can never pass a
    /// guard again.
    pub(crate) const STEAL_ACTIVATION_SQL: &str = "
        UPDATE server_agent_leases
        SET owner = $2, fencing = fencing + 1, expires_at = $3, acquired_at = $4
        WHERE agent_id = $1
        RETURNING agent_id, owner, fencing, expires_at, acquired_at";

    /// Activation heartbeat: extends the lease only while the exact
    /// owner + fencing pair holds it live. No row means unknown (404) or
    /// fencing lost (409), distinguished by [`SELECT_ACTIVATION_SQL`].
    pub(crate) const RENEW_ACTIVATION_SQL: &str = "
        UPDATE server_agent_leases
        SET expires_at = $4
        WHERE agent_id = $1 AND owner = $2 AND fencing = $3 AND expires_at > $5
        RETURNING agent_id, owner, fencing, expires_at, acquired_at";

    /// Activation release: same owner + fencing + liveness guard as the
    /// renewal — an expired lease is already stealable, so only a live
    /// holder match releases.
    pub(crate) const RELEASE_ACTIVATION_SQL: &str = "
        DELETE FROM server_agent_leases
        WHERE agent_id = $1 AND owner = $2 AND fencing = $3 AND expires_at > $4
        RETURNING agent_id, owner, fencing, expires_at, acquired_at";

    /// Turn-serialization gate, run inside the mailbox-claim transaction
    /// after the activation-lease row is locked: a live-leased message
    /// already in flight for this recipient makes the whole mailbox
    /// answer empty. One message at a time per agent is server-enforced.
    pub(crate) const AGENT_TURN_IN_FLIGHT_SQL: &str = "
        SELECT 1 AS busy FROM server_tasks
        WHERE tenant = $1 AND recipient = $2
          AND status = 'leased' AND lease_expires_at > $3
        LIMIT 1";

    /// Mailbox candidate selection: the oldest claimable message
    /// addressed to this recipient. Same claimability rule as
    /// [`CLAIM_SELECT_SQL`] minus the pool predicates — pool capacity and
    /// worker-version pins do not apply to agent claims (see the trait
    /// contract). `FOR UPDATE SKIP LOCKED` pairs with the activation-row
    /// lock: the lease lock serializes one holder's concurrent claims,
    /// SKIP LOCKED keeps a racing *steal* from double-claiming.
    pub(crate) const AGENT_CLAIM_SELECT_SQL: &str = "
        SELECT task_id, attempt FROM server_tasks
        WHERE tenant = $1
          AND recipient = $2
          AND (
              (status IN ('queued', 'failed')
                  AND (next_attempt_at IS NULL OR next_attempt_at <= $3))
              OR (status = 'leased' AND lease_expires_at <= $3)
          )
        ORDER BY created_at, task_id
        LIMIT 1
        FOR UPDATE SKIP LOCKED";

    // -- Row <-> record mapping (unit-tested without a database) -------- //

    /// Serialize a record for the JSONB `payload` column.
    pub(crate) fn record_to_payload<T: serde::Serialize>(record: &T) -> StoreResult<Value> {
        serde_json::to_value(record).map_err(|e| format!("serialize record: {e}"))
    }

    /// Deserialize a JSONB `payload` column back into a record.
    pub(crate) fn record_from_payload<T: serde::de::DeserializeOwned>(
        what: &str,
        payload: Value,
    ) -> StoreResult<T> {
        serde_json::from_value(payload).map_err(|e| format!("corrupt {what} payload: {e}"))
    }

    /// Assemble a wire-facing [`StoreItem`] from one `server_kv` row.
    pub(crate) fn kv_row_to_item(
        namespace: &str,
        key: &str,
        value: Value,
        created_at: DateTime<Utc>,
        updated_at: DateTime<Utc>,
    ) -> StoreItem {
        StoreItem {
            namespace: namespace.to_string(),
            key: key.to_string(),
            value,
            created_at,
            updated_at,
        }
    }

    /// Assemble a [`TaskRecord`] from one `server_tasks` row (name-based, so
    /// additive columns never break the mapping). A corrupt `status` or a
    /// negative attempt count is a store error, not a panic — the same
    /// discipline as `record_from_payload`.
    pub(crate) fn task_from_row(row: &sqlx::postgres::PgRow) -> StoreResult<TaskRecord> {
        let status_raw: String = row.get("status");
        let status = TaskStatus::parse(&status_raw)
            .ok_or_else(|| format!("corrupt task status `{status_raw}`"))?;
        let attempt = u32::try_from(row.get::<i32, _>("attempt"))
            .map_err(|_| "corrupt task attempt (negative)".to_string())?;
        let max_attempts = u32::try_from(row.get::<i32, _>("max_attempts"))
            .map_err(|_| "corrupt task max_attempts (negative)".to_string())?;
        let lease = match (
            row.get::<Option<String>, _>("lease_owner"),
            row.get::<Option<DateTime<Utc>>, _>("lease_expires_at"),
        ) {
            (Some(owner), Some(expires_at)) => Some(TaskLease { owner, expires_at }),
            _ => None,
        };
        let error_class = row
            .get::<Option<String>, _>("error_class")
            .map(|raw| {
                tasks::parse_error_class(&raw)
                    .map_err(|_| format!("corrupt task error_class `{raw}`"))
            })
            .transpose()?;
        let effect = row
            .get::<Option<String>, _>("effect")
            .map(|raw| {
                tasks::parse_effect(&raw).map_err(|_| format!("corrupt task effect `{raw}`"))
            })
            .transpose()?;
        let receipt = row
            .get::<Option<Value>, _>("receipt")
            .map(|raw| {
                serde_json::from_value(raw).map_err(|e| format!("corrupt task receipt: {e}"))
            })
            .transpose()?;
        let tokens = row
            .get::<Option<Value>, _>("tokens")
            .map(|raw| serde_json::from_value(raw).map_err(|e| format!("corrupt task tokens: {e}")))
            .transpose()?;
        Ok(TaskRecord {
            task_id: row.get("task_id"),
            tenant: row.get("tenant"),
            kind: row.get("kind"),
            payload: row.get("payload"),
            pool: row.get("pool"),
            status,
            attempt,
            max_attempts,
            lease,
            error_class,
            effect,
            last_error: row.get("last_error"),
            idempotency_key: row.get("idempotency_key"),
            result: row.get("result"),
            receipt,
            tokens,
            cost_usd: row.get("cost_usd"),
            run_id: row.get("run_id"),
            thread_id: row.get("thread_id"),
            parent: row.get("parent"),
            cancel_requested: row.get("cancel_requested"),
            deadline: row.get("deadline"),
            worker_version: row.get("worker_version"),
            recipient: row.get("recipient"),
            next_attempt_at: row.get("next_attempt_at"),
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
        })
    }

    fn db_err(context: &str) -> impl Fn(sqlx::Error) -> String + '_ {
        move |e| format!("{context}: {e}")
    }

    /// `23505` — a primary-key/unique-index violation. The activation
    /// claim's fresh-insert half uses it to recognize a lost create race.
    fn is_unique_violation(e: &sqlx::Error) -> bool {
        matches!(e, sqlx::Error::Database(db) if db.code().as_deref() == Some("23505"))
    }

    /// [`INSERT_TASK_SQL`] with a record bound to it, shared by direct
    /// enqueue and the outbox relay's publish — the dedupe semantics
    /// (`ON CONFLICT DO NOTHING`) are the relay's no-double-publish
    /// mechanism, so both paths must bind the identical statement.
    fn insert_task_query(
        record: &TaskRecord,
    ) -> sqlx::query::Query<'_, sqlx::Postgres, sqlx::postgres::PgArguments> {
        sqlx::query(INSERT_TASK_SQL)
            .bind(&record.task_id)
            .bind(&record.tenant)
            .bind(&record.kind)
            .bind(&record.payload)
            .bind(&record.pool)
            .bind(record.max_attempts as i32)
            .bind(record.effect.map(tasks::effect_name))
            .bind(&record.idempotency_key)
            .bind(&record.run_id)
            .bind(&record.thread_id)
            .bind(record.deadline)
            .bind(&record.worker_version)
            .bind(&record.recipient)
            .bind(record.created_at)
            .bind(&record.parent)
    }

    /// Assemble an [`OutboxRecord`] from one `server_outbox` row; a corrupt
    /// embedded task payload is a store error, not a panic (the same
    /// discipline as [`record_from_payload`]).
    fn outbox_from_row(row: &sqlx::postgres::PgRow) -> StoreResult<OutboxRecord> {
        Ok(OutboxRecord {
            outbox_id: row.get("outbox_id"),
            tenant: row.get("tenant"),
            task: record_from_payload("outbox task", row.get::<Value, _>("task"))?,
            published_at: row.get("published_at"),
            created_at: row.get("created_at"),
        })
    }

    /// Assemble an [`ActivationLease`] from one `server_agent_leases` row.
    /// The fencing ordinal is a `BIGINT`; a negative value is corruption
    /// (a store error, not a panic — [`task_from_row`]'s discipline).
    fn activation_from_row(row: &sqlx::postgres::PgRow) -> StoreResult<ActivationLease> {
        let fencing = u64::try_from(row.get::<i64, _>("fencing"))
            .map_err(|_| "corrupt activation fencing (negative)".to_string())?;
        Ok(ActivationLease {
            agent_id: row.get("agent_id"),
            owner: row.get("owner"),
            fencing,
            expires_at: row.get("expires_at"),
            acquired_at: row.get("acquired_at"),
        })
    }

    /// `u64` fencing ordinals bind as `BIGINT`; a fencing beyond `i64::MAX`
    /// is a store error, never a wraparound.
    fn fencing_i64(fencing: u64) -> StoreResult<i64> {
        i64::try_from(fencing).map_err(|_| "fencing ordinal exceeds BIGINT".to_string())
    }

    /// The wire string of a memory enum (`MemoryKind` / `MemoryScope`):
    /// both serialize as snake_case strings, which is exactly what the
    /// `kind` / `scope` columns store. Infallible for these enums by
    /// construction — a non-string serialization would be a core wire
    /// change, surfaced as a store error rather than silently stored.
    fn memory_wire_str<T: serde::Serialize>(value: &T) -> StoreResult<String> {
        serde_json::to_value(value)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .ok_or_else(|| "memory enum did not serialize to a string".to_string())
    }

    /// Postgres-backed store: assistants / crons / KV in `server_*` tables.
    ///
    /// The connection (and idempotent auto-migration) is established lazily
    /// on first use, so [`crate::router`] can stay synchronous.
    pub(crate) struct PostgresStore {
        url: String,
        pool: OnceCell<PgPool>,
        /// Set once core's `rusty_checkpoints` table has been migrated
        /// over this store's pool (see `checkpoint_and_enqueue`).
        checkpoints_migrated: OnceCell<()>,
        /// Set once core's `rusty_artifacts` table has been migrated
        /// over this store's pool (see `memory_artifacts`): the artifact
        /// store is a separate subsystem with its own lifecycle, and the
        /// memory paths can be a deployment's first Postgres traffic —
        /// before any checkpoint operation would have created the table.
        artifacts_migrated: OnceCell<()>,
    }

    impl PostgresStore {
        /// A store that will connect to `url` on first use.
        pub(crate) fn new(url: String) -> Self {
            Self {
                url,
                pool: OnceCell::new(),
                checkpoints_migrated: OnceCell::new(),
                artifacts_migrated: OnceCell::new(),
            }
        }

        /// The artifact store spilled memory bodies persist through,
        /// migrating `rusty_artifacts` once per store (idempotent, the
        /// `checkpoints_migrated` discipline applied to core's artifact
        /// subsystem).
        async fn memory_artifacts(
            &self,
        ) -> StoreResult<rusty_agent_runtime::checkpoint_postgres::PostgresArtifactStore> {
            let pool = self.pool().await?;
            let store = rusty_agent_runtime::checkpoint_postgres::PostgresArtifactStore::from_pool(
                pool.clone(),
            );
            self.artifacts_migrated
                .get_or_try_init(|| async {
                    store
                        .migrate()
                        .await
                        .map_err(|e| format!("migrate artifacts table: {e}"))
                })
                .await?;
            Ok(store)
        }

        /// The connection pool, connecting + migrating on first call.
        ///
        /// The migration runs inside a transaction holding a
        /// transaction-scoped advisory lock, so concurrent first-use
        /// migrations (e.g. several tests or server instances booting against
        /// one fresh database) serialize instead of tripping the
        /// `CREATE TABLE IF NOT EXISTS` check-then-create race (duplicate key
        /// on `pg_type_typname_nsp_index`).
        async fn pool(&self) -> StoreResult<&PgPool> {
            self.pool
                .get_or_try_init(|| async {
                    let pool = PgPool::connect(&self.url)
                        .await
                        .map_err(db_err("connect postgres"))?;
                    let mut tx = pool
                        .begin()
                        .await
                        .map_err(db_err("migrate server tables"))?;
                    sqlx::query("SELECT pg_advisory_xact_lock($1)")
                        .bind(MIGRATION_LOCK_KEY)
                        .execute(&mut *tx)
                        .await
                        .map_err(db_err("migrate server tables"))?;
                    for stmt in MIGRATION_SQL {
                        sqlx::query(stmt)
                            .execute(&mut *tx)
                            .await
                            .map_err(db_err("migrate server tables"))?;
                    }
                    tx.commit().await.map_err(db_err("migrate server tables"))?;
                    Ok(pool)
                })
                .await
        }
    }

    #[async_trait::async_trait]
    impl ServerStore for PostgresStore {
        async fn create_assistant(&self, record: &AssistantRecord) -> StoreResult<bool> {
            let payload = record_to_payload(record)?;
            let row = sqlx::query(INSERT_ASSISTANT_SQL)
                .bind(&record.assistant_id)
                .bind(payload)
                .fetch_optional(self.pool().await?)
                .await
                .map_err(db_err("insert assistant"))?;
            Ok(row.is_some())
        }

        async fn get_assistant(&self, assistant_id: &str) -> StoreResult<Option<AssistantRecord>> {
            let row = sqlx::query(SELECT_ASSISTANT_SQL)
                .bind(assistant_id)
                .fetch_optional(self.pool().await?)
                .await
                .map_err(db_err("select assistant"))?;
            row.map(|r| record_from_payload("assistant", r.get::<Value, _>("payload")))
                .transpose()
        }

        async fn list_assistants(&self) -> StoreResult<Vec<AssistantRecord>> {
            let rows = sqlx::query(LIST_ASSISTANTS_SQL)
                .fetch_all(self.pool().await?)
                .await
                .map_err(db_err("list assistants"))?;
            rows.into_iter()
                .map(|r| record_from_payload("assistant", r.get::<Value, _>("payload")))
                .collect()
        }

        async fn create_cron(&self, record: &CronRecord) -> StoreResult<bool> {
            let payload = record_to_payload(record)?;
            let row = sqlx::query(INSERT_CRON_SQL)
                .bind(&record.cron_id)
                .bind(payload)
                .fetch_optional(self.pool().await?)
                .await
                .map_err(db_err("insert cron"))?;
            Ok(row.is_some())
        }

        async fn upsert_cron(&self, record: &CronRecord) -> StoreResult<()> {
            let payload = record_to_payload(record)?;
            sqlx::query(UPSERT_CRON_SQL)
                .bind(&record.cron_id)
                .bind(payload)
                .execute(self.pool().await?)
                .await
                .map_err(db_err("upsert cron"))?;
            Ok(())
        }

        async fn get_cron(&self, cron_id: &str) -> StoreResult<Option<CronRecord>> {
            let row = sqlx::query(SELECT_CRON_SQL)
                .bind(cron_id)
                .fetch_optional(self.pool().await?)
                .await
                .map_err(db_err("select cron"))?;
            row.map(|r| record_from_payload("cron", r.get::<Value, _>("payload")))
                .transpose()
        }

        async fn list_crons(&self) -> StoreResult<Vec<CronRecord>> {
            let rows = sqlx::query(LIST_CRONS_SQL)
                .fetch_all(self.pool().await?)
                .await
                .map_err(db_err("list crons"))?;
            rows.into_iter()
                .map(|r| record_from_payload("cron", r.get::<Value, _>("payload")))
                .collect()
        }

        async fn delete_cron(&self, cron_id: &str) -> StoreResult<bool> {
            let result = sqlx::query(DELETE_CRON_SQL)
                .bind(cron_id)
                .execute(self.pool().await?)
                .await
                .map_err(db_err("delete cron"))?;
            Ok(result.rows_affected() > 0)
        }

        async fn create_trigger(&self, record: &TriggerRecord) -> StoreResult<bool> {
            let payload = record_to_payload(record)?;
            let row = sqlx::query(INSERT_TRIGGER_SQL)
                .bind(&record.trigger_id)
                .bind(payload)
                .fetch_optional(self.pool().await?)
                .await
                .map_err(db_err("insert trigger"))?;
            Ok(row.is_some())
        }

        async fn upsert_trigger(&self, record: &TriggerRecord) -> StoreResult<()> {
            let payload = record_to_payload(record)?;
            sqlx::query(UPSERT_TRIGGER_SQL)
                .bind(&record.trigger_id)
                .bind(payload)
                .execute(self.pool().await?)
                .await
                .map_err(db_err("upsert trigger"))?;
            Ok(())
        }

        async fn get_trigger(&self, trigger_id: &str) -> StoreResult<Option<TriggerRecord>> {
            let row = sqlx::query(SELECT_TRIGGER_SQL)
                .bind(trigger_id)
                .fetch_optional(self.pool().await?)
                .await
                .map_err(db_err("select trigger"))?;
            row.map(|r| record_from_payload("trigger", r.get::<Value, _>("payload")))
                .transpose()
        }

        async fn list_triggers(&self) -> StoreResult<Vec<TriggerRecord>> {
            let rows = sqlx::query(LIST_TRIGGERS_SQL)
                .fetch_all(self.pool().await?)
                .await
                .map_err(db_err("list triggers"))?;
            rows.into_iter()
                .map(|r| record_from_payload("trigger", r.get::<Value, _>("payload")))
                .collect()
        }

        async fn delete_trigger(&self, trigger_id: &str) -> StoreResult<bool> {
            // Trigger + its event log in one transaction: a crash between
            // the two deletes must not leave an orphaned log for a deleted
            // trigger.
            let mut tx = self
                .pool()
                .await?
                .begin()
                .await
                .map_err(db_err("delete trigger"))?;
            sqlx::query(DELETE_TRIGGER_EVENTS_SQL)
                .bind(trigger_id)
                .execute(&mut *tx)
                .await
                .map_err(db_err("delete trigger events"))?;
            let result = sqlx::query(DELETE_TRIGGER_SQL)
                .bind(trigger_id)
                .execute(&mut *tx)
                .await
                .map_err(db_err("delete trigger"))?;
            tx.commit().await.map_err(db_err("delete trigger"))?;
            Ok(result.rows_affected() > 0)
        }

        async fn append_trigger_event(&self, record: &TriggerEventRecord) -> StoreResult<()> {
            let payload = record_to_payload(record)?;
            let mut tx = self
                .pool()
                .await?
                .begin()
                .await
                .map_err(db_err("upsert trigger event"))?;
            sqlx::query(UPSERT_TRIGGER_EVENT_SQL)
                .bind(&record.event_id)
                .bind(&record.trigger_id)
                .bind(payload)
                .bind(record.created_at)
                .execute(&mut *tx)
                .await
                .map_err(db_err("upsert trigger event"))?;
            // Retention prune rides in the same transaction, so the cap
            // holds exactly (the file backend prunes under its index lock).
            sqlx::query(PRUNE_TRIGGER_EVENTS_SQL)
                .bind(&record.trigger_id)
                .execute(&mut *tx)
                .await
                .map_err(db_err("prune trigger events"))?;
            tx.commit().await.map_err(db_err("upsert trigger event"))?;
            Ok(())
        }

        async fn get_trigger_event(
            &self,
            trigger_id: &str,
            event_id: &str,
        ) -> StoreResult<Option<TriggerEventRecord>> {
            let row = sqlx::query(SELECT_TRIGGER_EVENT_SQL)
                .bind(trigger_id)
                .bind(event_id)
                .fetch_optional(self.pool().await?)
                .await
                .map_err(db_err("select trigger event"))?;
            row.map(|r| record_from_payload("trigger event", r.get::<Value, _>("payload")))
                .transpose()
        }

        async fn list_trigger_events(
            &self,
            trigger_id: &str,
        ) -> StoreResult<Vec<TriggerEventRecord>> {
            let rows = sqlx::query(LIST_TRIGGER_EVENTS_SQL)
                .bind(trigger_id)
                .fetch_all(self.pool().await?)
                .await
                .map_err(db_err("list trigger events"))?;
            rows.into_iter()
                .map(|r| record_from_payload("trigger event", r.get::<Value, _>("payload")))
                .collect()
        }

        async fn create_thread(
            &self,
            internal_id: &str,
            record: &ThreadRecord,
        ) -> StoreResult<bool> {
            let payload = record_to_payload(record)?;
            let row = sqlx::query(INSERT_THREAD_SQL)
                .bind(internal_id)
                .bind(payload)
                .fetch_optional(self.pool().await?)
                .await
                .map_err(db_err("insert thread"))?;
            Ok(row.is_some())
        }

        async fn get_thread(&self, internal_id: &str) -> StoreResult<Option<ThreadRecord>> {
            let row = sqlx::query(SELECT_THREAD_SQL)
                .bind(internal_id)
                .fetch_optional(self.pool().await?)
                .await
                .map_err(db_err("select thread"))?;
            row.map(|r| record_from_payload("thread", r.get::<Value, _>("payload")))
                .transpose()
        }

        async fn kv_put(
            &self,
            namespace: &str,
            key: &str,
            value: Value,
        ) -> StoreResult<(StoreItem, bool)> {
            let now = Utc::now();
            let row = sqlx::query(UPSERT_KV_SQL)
                .bind(namespace)
                .bind(key)
                .bind(&value)
                .bind(now) // created_at (ignored on conflict)
                .bind(now) // updated_at
                .fetch_one(self.pool().await?)
                .await
                .map_err(db_err("upsert store item"))?;
            let created_at: DateTime<Utc> = row.get("created_at");
            let updated_at: DateTime<Utc> = row.get("updated_at");
            let created: bool = row.get("created");
            Ok((
                kv_row_to_item(namespace, key, value, created_at, updated_at),
                created,
            ))
        }

        async fn kv_get(&self, namespace: &str, key: &str) -> StoreResult<Option<StoreItem>> {
            let row = sqlx::query(SELECT_KV_SQL)
                .bind(namespace)
                .bind(key)
                .fetch_optional(self.pool().await?)
                .await
                .map_err(db_err("get store item"))?;
            Ok(row.map(|r| {
                kv_row_to_item(
                    namespace,
                    key,
                    r.get::<Value, _>("value"),
                    r.get::<DateTime<Utc>, _>("created_at"),
                    r.get::<DateTime<Utc>, _>("updated_at"),
                )
            }))
        }

        async fn kv_delete(&self, namespace: &str, key: &str) -> StoreResult<bool> {
            let result = sqlx::query(DELETE_KV_SQL)
                .bind(namespace)
                .bind(key)
                .execute(self.pool().await?)
                .await
                .map_err(db_err("delete store item"))?;
            Ok(result.rows_affected() > 0)
        }

        async fn kv_list(&self, namespace: &str) -> StoreResult<Vec<StoreItem>> {
            let rows = sqlx::query(LIST_KV_SQL)
                .bind(namespace)
                .fetch_all(self.pool().await?)
                .await
                .map_err(db_err("list store namespace"))?;
            Ok(rows
                .into_iter()
                .map(|r| {
                    let key: String = r.get("key");
                    kv_row_to_item(
                        namespace,
                        &key,
                        r.get::<Value, _>("value"),
                        r.get::<DateTime<Utc>, _>("created_at"),
                        r.get::<DateTime<Utc>, _>("updated_at"),
                    )
                })
                .collect())
        }

        async fn put_journal(&self, snapshot: &JournalSnapshot) -> StoreResult<()> {
            let payload = record_to_payload(snapshot)?;
            sqlx::query(UPSERT_JOURNAL_SQL)
                .bind(&snapshot.run_id)
                .bind(payload)
                .execute(self.pool().await?)
                .await
                .map_err(db_err("upsert journal"))?;
            Ok(())
        }

        async fn get_journal(&self, run_id: &str) -> StoreResult<Option<JournalSnapshot>> {
            let row = sqlx::query(SELECT_JOURNAL_SQL)
                .bind(run_id)
                .fetch_optional(self.pool().await?)
                .await
                .map_err(db_err("select journal"))?;
            row.map(|r| record_from_payload("journal", r.get::<Value, _>("payload")))
                .transpose()
        }

        async fn enqueue_task(&self, record: &TaskRecord) -> StoreResult<(TaskRecord, bool)> {
            let row = insert_task_query(record)
                .fetch_optional(self.pool().await?)
                .await
                .map_err(db_err("enqueue task"))?;
            if row.is_some() {
                return Ok((record.clone(), false));
            }
            // The insert was absorbed by a conflict. With an idempotency key
            // that is the dedup path: the live task carrying the key wins.
            let Some(key) = &record.idempotency_key else {
                return Err(format!(
                    "task id `{}` collided with an existing task",
                    record.task_id
                ));
            };
            let existing = sqlx::query(SELECT_TASK_BY_IDEMPOTENCY_SQL)
                .bind(&record.tenant)
                .bind(key)
                .fetch_optional(self.pool().await?)
                .await
                .map_err(db_err("enqueue task dedup lookup"))?;
            match existing {
                Some(row) => Ok((task_from_row(&row)?, true)),
                None => Err(format!(
                    "task insert for idempotency key `{key}` conflicted but no live task carries it"
                )),
            }
        }

        async fn claim_task(
            &self,
            tenant: &str,
            worker_id: &str,
            scope: &tasks::ClaimScope<'_>,
            lease_ms: u64,
            now: DateTime<Utc>,
        ) -> StoreResult<Option<TaskRecord>> {
            let pool = self.pool().await?;
            // Lock-and-update in one transaction: SKIP LOCKED lets
            // concurrent workers claim distinct tasks; the row lock holds
            // until the claim commits, so no two workers ever take one task.
            let mut tx = pool.begin().await.map_err(db_err("claim task"))?;
            // Finalize cancel requests the holder never acknowledged and
            // elapsed whole-task deadlines before handing out work — such
            // tasks are cancelled, never re-leased (mirrors the file
            // backend's sweep, same record rule).
            sqlx::query(CLAIM_FINALIZE_SQL)
                .bind(tenant)
                .bind(now)
                .execute(&mut *tx)
                .await
                .map_err(db_err("claim task finalization"))?;
            // Pool capacity (wave 3): pools at their configured live-lease
            // limit are excluded from the candidate select, so one
            // saturated pool never starves the others. Counted in the same
            // transaction as the claim — though concurrent claim
            // transactions do not serialize against each other, so racing
            // claims can transiently overshoot a pool's cap by up to the
            // number of racers (see the trait's `claim_task` contract: a
            // guardrail, not a hard invariant).
            let saturated: Vec<String> = if scope.pool_limits.is_empty() {
                Vec::new()
            } else {
                let limited: Vec<&String> = scope.pool_limits.keys().collect();
                let counts = sqlx::query(CLAIM_INFLIGHT_SQL)
                    .bind(tenant)
                    .bind(now)
                    .bind(&limited)
                    .fetch_all(&mut *tx)
                    .await
                    .map_err(db_err("claim task pool capacity"))?;
                let mut saturated = Vec::new();
                for row in counts {
                    let name: String = row.get("pool");
                    let live = u64::try_from(row.get::<i64, _>("live"))
                        .map_err(|_| "corrupt lease count (negative)".to_string())?;
                    if live >= scope.pool_limits[&name] as u64 {
                        saturated.push(name);
                    }
                }
                saturated
            };
            let candidate = sqlx::query(CLAIM_SELECT_SQL)
                .bind(tenant)
                .bind(scope.pools.to_vec())
                .bind(now)
                .bind(&saturated)
                .bind(scope.worker_version)
                .fetch_optional(&mut *tx)
                .await
                .map_err(db_err("claim task"))?;
            let Some(candidate) = candidate else {
                tx.rollback().await.map_err(db_err("claim task"))?;
                return Ok(None);
            };
            let expires_at =
                now + chrono::Duration::milliseconds(lease_ms.min(i64::MAX as u64) as i64);
            let updated = sqlx::query(CLAIM_UPDATE_SQL)
                .bind(candidate.get::<String, _>("task_id"))
                .bind(worker_id)
                .bind(expires_at)
                .bind(candidate.get::<i32, _>("attempt") + 1)
                .bind(now)
                .fetch_one(&mut *tx)
                .await
                .map_err(db_err("claim task"))?;
            tx.commit().await.map_err(db_err("claim task"))?;
            Ok(Some(task_from_row(&updated)?))
        }

        async fn heartbeat_task(
            &self,
            tenant: &str,
            task_id: &str,
            worker_id: &str,
            lease_ms: u64,
            now: DateTime<Utc>,
        ) -> StoreResult<MutationOutcome> {
            let expires_at =
                now + chrono::Duration::milliseconds(lease_ms.min(i64::MAX as u64) as i64);
            let updated = sqlx::query(HEARTBEAT_TASK_SQL)
                .bind(task_id)
                .bind(tenant)
                .bind(worker_id)
                .bind(expires_at)
                .bind(now)
                .fetch_optional(self.pool().await?)
                .await
                .map_err(db_err("heartbeat task"))?;
            self.lease_outcome(tenant, task_id, updated).await
        }

        async fn complete_task(
            &self,
            tenant: &str,
            task_id: &str,
            worker_id: &str,
            report: tasks::CompletionReport,
            now: DateTime<Utc>,
        ) -> StoreResult<MutationOutcome> {
            let receipt = report.receipt.as_ref().map(record_to_payload).transpose()?;
            let tokens = report
                .cost
                .tokens
                .as_ref()
                .map(record_to_payload)
                .transpose()?;
            let updated = sqlx::query(COMPLETE_TASK_SQL)
                .bind(task_id)
                .bind(tenant)
                .bind(worker_id)
                .bind(report.result)
                .bind(now)
                .bind(receipt)
                .bind(tokens)
                .bind(report.cost.cost_usd)
                .fetch_optional(self.pool().await?)
                .await
                .map_err(db_err("complete task"))?;
            self.lease_outcome(tenant, task_id, updated).await
        }

        async fn fail_task(
            &self,
            tenant: &str,
            task_id: &str,
            worker_id: &str,
            report: tasks::FailureReport,
            now: DateTime<Utc>,
        ) -> StoreResult<MutationOutcome> {
            let pool = self.pool().await?;
            let mut tx = pool.begin().await.map_err(db_err("fail task"))?;
            let locked = sqlx::query(FAIL_SELECT_SQL)
                .bind(task_id)
                .bind(tenant)
                .fetch_optional(&mut *tx)
                .await
                .map_err(db_err("fail task"))?;
            let Some(locked) = locked else {
                tx.rollback().await.map_err(db_err("fail task"))?;
                return Ok(MutationOutcome::Unknown);
            };
            let mut task = task_from_row(&locked)?;
            if !task.leased_to(worker_id) {
                tx.rollback().await.map_err(db_err("fail task"))?;
                return Ok(MutationOutcome::LeaseLost);
            }
            // Retry / dead-letter / fail-outright, computed by the same
            // record logic the file backend runs — core's shared
            // `classify_retry` (one decision, one test surface).
            task.fail(
                report.error_class,
                &report.message,
                report.retryable,
                report.cost,
                now,
            );
            let tokens = task.tokens.as_ref().map(record_to_payload).transpose()?;
            sqlx::query(FAIL_UPDATE_SQL)
                .bind(&task.task_id)
                .bind(task.status.as_str())
                .bind(task.error_class.map(tasks::error_class_name))
                .bind(&task.last_error)
                .bind(task.next_attempt_at)
                .bind(now)
                .bind(tokens)
                .bind(task.cost_usd)
                .execute(&mut *tx)
                .await
                .map_err(db_err("fail task"))?;
            tx.commit().await.map_err(db_err("fail task"))?;
            Ok(MutationOutcome::Applied(Box::new(task)))
        }

        async fn get_task(&self, tenant: &str, task_id: &str) -> StoreResult<Option<TaskRecord>> {
            let row = sqlx::query(SELECT_TASK_SQL)
                .bind(task_id)
                .bind(tenant)
                .fetch_optional(self.pool().await?)
                .await
                .map_err(db_err("get task"))?;
            row.as_ref().map(task_from_row).transpose()
        }

        async fn list_tasks(
            &self,
            tenant: &str,
            status: Option<TaskStatus>,
        ) -> StoreResult<Vec<TaskRecord>> {
            let rows = match status {
                Some(status) => sqlx::query(LIST_TASKS_BY_STATUS_SQL)
                    .bind(tenant)
                    .bind(status.as_str())
                    .fetch_all(self.pool().await?)
                    .await
                    .map_err(db_err("list tasks"))?,
                None => sqlx::query(LIST_TASKS_SQL)
                    .bind(tenant)
                    .fetch_all(self.pool().await?)
                    .await
                    .map_err(db_err("list tasks"))?,
            };
            rows.iter().map(task_from_row).collect()
        }

        async fn cancel_task(
            &self,
            tenant: &str,
            task_id: &str,
            now: DateTime<Utc>,
        ) -> StoreResult<CancelOutcome> {
            let pool = self.pool().await?;
            let mut tx = pool.begin().await.map_err(db_err("cancel task"))?;
            let locked = sqlx::query(CANCEL_SELECT_SQL)
                .bind(task_id)
                .bind(tenant)
                .fetch_optional(&mut *tx)
                .await
                .map_err(db_err("cancel task"))?;
            let Some(locked) = locked else {
                tx.rollback().await.map_err(db_err("cancel task"))?;
                return Ok(CancelOutcome::Unknown);
            };
            let mut task = task_from_row(&locked)?;
            // Immediate-terminal vs signal-the-holder, computed by the same
            // record logic the file backend runs (one rule, one test surface).
            if task.cancel(now).is_none() {
                let status = task.status;
                tx.rollback().await.map_err(db_err("cancel task"))?;
                return Ok(CancelOutcome::Terminal(status));
            }
            let (lease_owner, lease_expires_at) = match &task.lease {
                Some(lease) => (Some(lease.owner.clone()), Some(lease.expires_at)),
                None => (None, None),
            };
            let updated = sqlx::query(CANCEL_UPDATE_SQL)
                .bind(&task.task_id)
                .bind(task.status.as_str())
                .bind(task.error_class.map(tasks::error_class_name))
                .bind(task.cancel_requested)
                .bind(lease_owner)
                .bind(lease_expires_at)
                .bind(task.next_attempt_at)
                .bind(now)
                .fetch_one(&mut *tx)
                .await
                .map_err(db_err("cancel task"))?;
            tx.commit().await.map_err(db_err("cancel task"))?;
            Ok(CancelOutcome::Applied(Box::new(task_from_row(&updated)?)))
        }

        async fn cancel_run_tasks(
            &self,
            tenant: &str,
            run_id: &str,
            now: DateTime<Utc>,
        ) -> StoreResult<RunCancellation> {
            let pool = self.pool().await?;
            // One transaction: the run's outstanding tasks cancel as a unit,
            // never half-propagated.
            let mut tx = pool.begin().await.map_err(db_err("cancel run tasks"))?;
            let finalized = sqlx::query(CANCEL_RUN_FINALIZE_SQL)
                .bind(tenant)
                .bind(run_id)
                .bind(now)
                .fetch_all(&mut *tx)
                .await
                .map_err(db_err("cancel run tasks"))?;
            let signalled = sqlx::query(CANCEL_RUN_SIGNAL_SQL)
                .bind(tenant)
                .bind(run_id)
                .bind(now)
                .fetch_all(&mut *tx)
                .await
                .map_err(db_err("cancel run tasks"))?;
            tx.commit().await.map_err(db_err("cancel run tasks"))?;
            Ok(RunCancellation {
                cancelled: finalized
                    .iter()
                    .map(task_from_row)
                    .collect::<StoreResult<Vec<_>>>()?,
                signalled: signalled
                    .iter()
                    .map(task_from_row)
                    .collect::<StoreResult<Vec<_>>>()?,
            })
        }

        async fn cancel_agent_tasks(
            &self,
            tenant: &str,
            recipient: &str,
            now: DateTime<Utc>,
        ) -> StoreResult<RunCancellation> {
            let pool = self.pool().await?;
            // One transaction, the run-scoped twin's discipline: the
            // mailbox's outstanding messages cancel as a unit, never
            // half-propagated.
            let mut tx = pool.begin().await.map_err(db_err("cancel agent tasks"))?;
            let finalized = sqlx::query(CANCEL_AGENT_FINALIZE_SQL)
                .bind(tenant)
                .bind(recipient)
                .bind(now)
                .fetch_all(&mut *tx)
                .await
                .map_err(db_err("cancel agent tasks"))?;
            let signalled = sqlx::query(CANCEL_AGENT_SIGNAL_SQL)
                .bind(tenant)
                .bind(recipient)
                .bind(now)
                .fetch_all(&mut *tx)
                .await
                .map_err(db_err("cancel agent tasks"))?;
            tx.commit().await.map_err(db_err("cancel agent tasks"))?;
            Ok(RunCancellation {
                cancelled: finalized
                    .iter()
                    .map(task_from_row)
                    .collect::<StoreResult<Vec<_>>>()?,
                signalled: signalled
                    .iter()
                    .map(task_from_row)
                    .collect::<StoreResult<Vec<_>>>()?,
            })
        }

        async fn dead_letter_task(&self, record: &TaskRecord) -> StoreResult<(TaskRecord, bool)> {
            let row = sqlx::query(INSERT_DEAD_LETTER_SQL)
                .bind(&record.task_id)
                .bind(&record.tenant)
                .bind(&record.kind)
                .bind(&record.payload)
                .bind(&record.pool)
                .bind(record.max_attempts as i32)
                .bind(&record.idempotency_key)
                .bind(&record.last_error)
                .bind(record.created_at)
                .fetch_optional(self.pool().await?)
                .await
                .map_err(db_err("dead-letter task"))?;
            if row.is_some() {
                return Ok((record.clone(), false));
            }
            // The insert was absorbed by a conflict. With an idempotency
            // key — escalation always carries one — that is the dedup
            // path: the dead-letter carrying the key wins, read back the
            // way `enqueue_task` resolves its dedup.
            let Some(key) = &record.idempotency_key else {
                return Err(format!(
                    "dead-letter id `{}` collided with an existing task",
                    record.task_id
                ));
            };
            let existing = sqlx::query(SELECT_TASK_BY_IDEMPOTENCY_SQL)
                .bind(&record.tenant)
                .bind(key)
                .fetch_optional(self.pool().await?)
                .await
                .map_err(db_err("dead-letter task dedup lookup"))?;
            match existing {
                Some(row) => Ok((task_from_row(&row)?, true)),
                None => Err(format!(
                    "dead-letter insert for idempotency key `{key}` conflicted but no task carries it"
                )),
            }
        }

        async fn task_usage(&self, tenant: &str) -> StoreResult<tasks::TaskUsage> {
            let pool = self.pool().await?;
            let row = sqlx::query(TASK_USAGE_SQL)
                .bind(tenant)
                .fetch_one(pool)
                .await
                .map_err(db_err("task usage"))?;
            let count = |column: &str| {
                u64::try_from(row.get::<i64, _>(column))
                    .map_err(|_| format!("corrupt task usage count `{column}` (negative)"))
            };
            Ok(tasks::TaskUsage {
                queued: count("queued")? + count("pending_outbox")?,
                in_flight: count("in_flight")?,
                dlq: count("dlq")?,
            })
        }

        async fn task_pool_stats(
            &self,
            tenant: &str,
            now: DateTime<Utc>,
        ) -> StoreResult<Vec<tasks::PoolStat>> {
            let rows = sqlx::query(POOL_STATS_SQL)
                .bind(tenant)
                .bind(now)
                .fetch_all(self.pool().await?)
                .await
                .map_err(db_err("task pool stats"))?;
            rows.iter()
                .map(|row| {
                    let count = |column: &str| {
                        u64::try_from(row.get::<i64, _>(column))
                            .map_err(|_| format!("corrupt pool stat `{column}` (negative)"))
                    };
                    Ok(tasks::PoolStat {
                        pool: row.get("pool"),
                        queue_depth: count("queue_depth")?,
                        leased: count("leased")?,
                        oldest_visible_at: row.get("oldest_visible_at"),
                    })
                })
                .collect()
        }

        async fn outbox_enqueue(&self, record: &TaskRecord) -> StoreResult<(TaskRecord, bool)> {
            let payload = record_to_payload(record)?;
            let row = sqlx::query(INSERT_OUTBOX_SQL)
                .bind(&record.task_id)
                .bind(&record.tenant)
                .bind(payload)
                .bind(record.created_at)
                .fetch_optional(self.pool().await?)
                .await
                .map_err(db_err("enqueue outbox"))?;
            if row.is_some() {
                return Ok((record.clone(), false));
            }
            // The insert was absorbed by the outbox_id conflict: a retried
            // write of the same row — return the pending one.
            let existing = sqlx::query(SELECT_OUTBOX_BY_ID_SQL)
                .bind(&record.task_id)
                .fetch_optional(self.pool().await?)
                .await
                .map_err(db_err("enqueue outbox dedup lookup"))?;
            match existing {
                Some(row) => Ok((outbox_from_row(&row)?.task, true)),
                None => Err(format!(
                    "outbox insert for task `{}` conflicted but no row carries it",
                    record.task_id
                )),
            }
        }

        async fn outbox_publish_pending(
            &self,
            limit: usize,
            now: DateTime<Utc>,
        ) -> StoreResult<Vec<TaskRecord>> {
            let pool = self.pool().await?;
            let mut published = Vec::new();
            // One row per transaction: the task insert and the
            // mark-published commit or roll back together, so a crash
            // mid-publish leaves the row pending for the next pass (or the
            // next process) — never lost. The task insert dedupes on the
            // idempotency-key unique index (and the task-id primary key), so
            // a retry after the task already exists — however it got there —
            // can never double it: publishing is at-least-once, visibility
            // is effectively-once.
            while published.len() < limit {
                let mut tx = pool.begin().await.map_err(db_err("publish outbox"))?;
                let pending = sqlx::query(SELECT_OUTBOX_PENDING_SQL)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(db_err("publish outbox"))?;
                let Some(pending) = pending else {
                    tx.commit().await.map_err(db_err("publish outbox"))?;
                    break;
                };
                let row = outbox_from_row(&pending)?;
                let inserted = insert_task_query(&row.task)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(db_err("publish outbox task insert"))?;
                // Resolve the task the publish actually made visible. An
                // absorbed insert means either the task-id PK absorbed a
                // publish retried after a crash, or the idempotency-key
                // unique index absorbed a duplicate submission of the same
                // effect — the returned record must name the live task, the
                // same answer the file backend and direct enqueue give.
                let task = match inserted {
                    Some(_) => row.task,
                    None => {
                        let by_id = sqlx::query(SELECT_TASK_SQL)
                            .bind(&row.task.task_id)
                            .bind(&row.task.tenant)
                            .fetch_optional(&mut *tx)
                            .await
                            .map_err(db_err("publish outbox resolve"))?;
                        match by_id {
                            Some(existing) => task_from_row(&existing)?,
                            None => {
                                let key =
                                    row.task.idempotency_key.as_deref().ok_or_else(|| {
                                        format!(
                                            "outbox publish for task `{}` conflicted but no live task carries it",
                                            row.task.task_id
                                        )
                                    })?;
                                let by_key = sqlx::query(SELECT_TASK_BY_IDEMPOTENCY_SQL)
                                    .bind(&row.task.tenant)
                                    .bind(key)
                                    .fetch_optional(&mut *tx)
                                    .await
                                    .map_err(db_err("publish outbox resolve"))?
                                    .ok_or_else(|| {
                                        format!(
                                            "outbox publish for idempotency key `{key}` conflicted but no live task carries it"
                                        )
                                    })?;
                                task_from_row(&by_key)?
                            }
                        }
                    }
                };
                sqlx::query(MARK_OUTBOX_PUBLISHED_SQL)
                    .bind(&row.outbox_id)
                    .bind(now)
                    .execute(&mut *tx)
                    .await
                    .map_err(db_err("publish outbox mark published"))?;
                tx.commit().await.map_err(db_err("publish outbox"))?;
                published.push(task);
            }
            Ok(published)
        }

        async fn checkpoint_and_enqueue(
            &self,
            checkpoint: &Checkpoint,
            tasks: &[TaskRecord],
        ) -> StoreResult<()> {
            let step = i64::try_from(checkpoint.step).map_err(|_| {
                format!(
                    "checkpoint step {} does not fit into a Postgres bigint",
                    checkpoint.step
                )
            })?;
            let next_nodes = serde_json::to_value(&checkpoint.next_nodes)
                .map_err(|e| format!("serialize checkpoint next_nodes: {e}"))?;
            let header = serde_json::to_value(&checkpoint.header)
                .map_err(|e| format!("serialize checkpoint header: {e}"))?;
            let journal_ref = checkpoint
                .journal_ref
                .as_ref()
                .map(serde_json::to_value)
                .transpose()
                .map_err(|e| format!("serialize checkpoint journal ref: {e}"))?;
            let pool = self.pool().await?;
            // The checkpoint half writes into `rusty_checkpoints`, whose
            // schema is owned and auto-migrated by core's
            // PostgresCheckpointer — the run routes normally ensure it,
            // but a deployment whose first checkpoint write is an atomic
            // checkpoint+enqueue has no such guarantee. Migrate it once
            // per store, over this pool (idempotent, advisory-locked).
            self.checkpoints_migrated
                .get_or_try_init(|| async {
                    rusty_agent_runtime::checkpoint_postgres::PostgresCheckpointer::from_pool(
                        pool.clone(),
                    )
                    .migrate()
                    .await
                    .map_err(|e| format!("migrate checkpoints table: {e}"))
                })
                .await?;
            // Delta encoding (R0.7 wave 4): the same decision core's
            // `PostgresCheckpointer::put` makes, through the same public
            // helpers — this path writes through its own transaction, so it
            // cannot delegate the put. The head is read outside the
            // transaction (thread writes are single-writer by contract); a
            // foreign writer would only soften the chain bound, never
            // corrupt — a delta always names a real ancestor as its base.
            let head = rusty_agent_runtime::checkpoint_postgres::PostgresCheckpointer::from_pool(
                pool.clone(),
            )
            .delta_head(&checkpoint.thread_id)
            .await
            .map_err(|e| format!("read checkpoint delta head: {e}"))?;
            let encoding = encode_delta(checkpoint, head.as_ref(), &DeltaPolicy::default());
            let encoded_state = serde_json::to_value(&encoding.checkpoint.state)
                .map_err(|e| format!("serialize checkpoint state: {e}"))?;
            // The one transaction this wave exists for: the checkpoint and
            // every outbox row commit together or not at all. A duplicate
            // checkpoint id aborts the whole unit (no silent half-write);
            // the outbox inserts tolerate a retried pair (ON CONFLICT DO
            // NOTHING on outbox_id).
            let mut tx = pool
                .begin()
                .await
                .map_err(db_err("checkpoint and enqueue"))?;
            sqlx::query(INSERT_CHECKPOINT_SQL)
                .bind(&checkpoint.thread_id)
                .bind(&checkpoint.id)
                .bind(step)
                .bind(encoded_state)
                .bind(next_nodes)
                .bind(checkpoint.created_at)
                .bind(header)
                .bind(journal_ref)
                .bind(&encoding.checkpoint.base)
                .execute(&mut *tx)
                .await
                .map_err(db_err("checkpoint and enqueue"))?;
            for record in tasks {
                let payload = record_to_payload(record)?;
                sqlx::query(INSERT_OUTBOX_SQL)
                    .bind(&record.task_id)
                    .bind(&record.tenant)
                    .bind(payload)
                    .bind(record.created_at)
                    .execute(&mut *tx)
                    .await
                    .map_err(db_err("checkpoint and enqueue"))?;
            }
            tx.commit()
                .await
                .map_err(db_err("checkpoint and enqueue"))?;
            Ok(())
        }

        async fn create_agent(&self, record: &AgentRecord) -> StoreResult<bool> {
            let payload = record_to_payload(record)?;
            let row = sqlx::query(INSERT_AGENT_SQL)
                .bind(&record.agent_id)
                .bind(payload)
                .fetch_optional(self.pool().await?)
                .await
                .map_err(db_err("insert agent"))?;
            Ok(row.is_some())
        }

        async fn update_agent(&self, record: &AgentRecord) -> StoreResult<bool> {
            let payload = record_to_payload(record)?;
            let row = sqlx::query(UPDATE_AGENT_SQL)
                .bind(&record.agent_id)
                .bind(payload)
                .fetch_optional(self.pool().await?)
                .await
                .map_err(db_err("update agent"))?;
            Ok(row.is_some())
        }

        async fn get_agent(&self, agent_id: &str) -> StoreResult<Option<AgentRecord>> {
            let row = sqlx::query(SELECT_AGENT_SQL)
                .bind(agent_id)
                .fetch_optional(self.pool().await?)
                .await
                .map_err(db_err("select agent"))?;
            row.map(|r| record_from_payload("agent", r.get::<Value, _>("payload")))
                .transpose()
        }

        async fn list_agents(&self) -> StoreResult<Vec<AgentRecord>> {
            let rows = sqlx::query(LIST_AGENTS_SQL)
                .fetch_all(self.pool().await?)
                .await
                .map_err(db_err("list agents"))?;
            rows.into_iter()
                .map(|r| record_from_payload("agent", r.get::<Value, _>("payload")))
                .collect()
        }

        async fn create_coordination(&self, record: &CoordinationRecord) -> StoreResult<bool> {
            let payload = record_to_payload(record)?;
            let row = sqlx::query(INSERT_COORDINATION_SQL)
                .bind(&record.coordination_id)
                .bind(payload)
                .fetch_optional(self.pool().await?)
                .await
                .map_err(db_err("insert coordination"))?;
            Ok(row.is_some())
        }

        async fn update_coordination(&self, record: &CoordinationRecord) -> StoreResult<bool> {
            let payload = record_to_payload(record)?;
            let row = sqlx::query(UPDATE_COORDINATION_SQL)
                .bind(&record.coordination_id)
                .bind(payload)
                .fetch_optional(self.pool().await?)
                .await
                .map_err(db_err("update coordination"))?;
            Ok(row.is_some())
        }

        async fn get_coordination(
            &self,
            coordination_id: &str,
        ) -> StoreResult<Option<CoordinationRecord>> {
            let row = sqlx::query(SELECT_COORDINATION_SQL)
                .bind(coordination_id)
                .fetch_optional(self.pool().await?)
                .await
                .map_err(db_err("select coordination"))?;
            row.map(|r| record_from_payload("coordination", r.get::<Value, _>("payload")))
                .transpose()
        }

        async fn journal_and_enqueue(
            &self,
            snapshot: &JournalSnapshot,
            tasks: &[TaskRecord],
        ) -> StoreResult<()> {
            // The one transaction the drive's commit point needs: the
            // journal evidence and every outbox row commit together or not
            // at all. Both inserts are idempotent (UPSERT on run_id, ON
            // CONFLICT DO NOTHING on outbox_id), so a retried drive simply
            // re-commits the same facts.
            let journal = record_to_payload(snapshot)?;
            let pool = self.pool().await?;
            let mut tx = pool.begin().await.map_err(db_err("journal and enqueue"))?;
            sqlx::query(UPSERT_JOURNAL_SQL)
                .bind(&snapshot.run_id)
                .bind(journal)
                .execute(&mut *tx)
                .await
                .map_err(db_err("journal and enqueue"))?;
            for record in tasks {
                let payload = record_to_payload(record)?;
                sqlx::query(INSERT_OUTBOX_SQL)
                    .bind(&record.task_id)
                    .bind(&record.tenant)
                    .bind(payload)
                    .bind(record.created_at)
                    .execute(&mut *tx)
                    .await
                    .map_err(db_err("journal and enqueue"))?;
            }
            tx.commit().await.map_err(db_err("journal and enqueue"))?;
            Ok(())
        }

        async fn claim_activation(
            &self,
            agent_id: &str,
            owner: &str,
            lease_ms: u64,
            now: DateTime<Utc>,
        ) -> StoreResult<ActivationOutcome> {
            let pool = self.pool().await?;
            let expires_at = now + agents::lease_duration(lease_ms);
            // The claim is one atomic transaction: the existing lease row
            // (if any) is locked FOR UPDATE before the steal decision, so
            // two racing claimants can never both win — the
            // activation-lease equivalent of `FOR UPDATE SKIP LOCKED`.
            //
            // The fresh-insert half needs a retry: when the row does not
            // exist yet, EVERY racer's locked select sees no row, so all
            // of them insert and all but one violate the primary key (a
            // failed statement aborts the transaction, so the loser cannot
            // re-read inside it). The loser rolls back, re-reads the now
            // visible winner's row, and answers Held. Bounded: a row that
            // keeps vanishing (a release racing the claims) retries.
            for _ in 0..3 {
                let mut tx = pool.begin().await.map_err(db_err("claim activation"))?;
                let current = sqlx::query(SELECT_ACTIVATION_FOR_UPDATE_SQL)
                    .bind(agent_id)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(db_err("claim activation"))?;
                if let Some(row) = &current {
                    let lease = activation_from_row(row)?;
                    // A live lease holds, whoever the claimant is — even
                    // the owner itself (it should heartbeat, not
                    // re-activate).
                    if lease.expires_at > now {
                        tx.rollback().await.map_err(db_err("claim activation"))?;
                        return Ok(ActivationOutcome::Held(Box::new(lease)));
                    }
                }
                let claimed = match &current {
                    Some(_) => {
                        sqlx::query(STEAL_ACTIVATION_SQL)
                            .bind(agent_id)
                            .bind(owner)
                            .bind(expires_at)
                            .bind(now)
                            .fetch_one(&mut *tx)
                            .await
                    }
                    None => {
                        sqlx::query(INSERT_ACTIVATION_SQL)
                            .bind(agent_id)
                            .bind(owner)
                            .bind(expires_at)
                            .bind(now)
                            .fetch_one(&mut *tx)
                            .await
                    }
                };
                let claimed = match claimed {
                    Ok(row) => row,
                    Err(e) if is_unique_violation(&e) => {
                        tx.rollback().await.map_err(db_err("claim activation"))?;
                        match self.get_activation(agent_id).await? {
                            Some(lease) => {
                                return Ok(ActivationOutcome::Held(Box::new(lease)));
                            }
                            // The winner released before the re-read —
                            // the race is over; try the claim again.
                            None => continue,
                        }
                    }
                    Err(e) => return Err(db_err("claim activation")(e)),
                };
                let lease = activation_from_row(&claimed)?;
                tx.commit().await.map_err(db_err("claim activation"))?;
                return Ok(ActivationOutcome::Claimed(Box::new(lease)));
            }
            Err("claim activation: create race did not settle in 3 attempts".to_string())
        }

        async fn renew_activation(
            &self,
            agent_id: &str,
            owner: &str,
            fencing: u64,
            lease_ms: u64,
            now: DateTime<Utc>,
        ) -> StoreResult<ActivationMutation> {
            let expires_at = now + agents::lease_duration(lease_ms);
            let updated = sqlx::query(RENEW_ACTIVATION_SQL)
                .bind(agent_id)
                .bind(owner)
                .bind(fencing_i64(fencing)?)
                .bind(expires_at)
                .bind(now)
                .fetch_optional(self.pool().await?)
                .await
                .map_err(db_err("renew activation"))?;
            self.activation_outcome(agent_id, updated).await
        }

        async fn release_activation(
            &self,
            agent_id: &str,
            owner: &str,
            fencing: u64,
            now: DateTime<Utc>,
        ) -> StoreResult<ActivationMutation> {
            let updated = sqlx::query(RELEASE_ACTIVATION_SQL)
                .bind(agent_id)
                .bind(owner)
                .bind(fencing_i64(fencing)?)
                .bind(now)
                .fetch_optional(self.pool().await?)
                .await
                .map_err(db_err("release activation"))?;
            self.activation_outcome(agent_id, updated).await
        }

        async fn get_activation(&self, agent_id: &str) -> StoreResult<Option<ActivationLease>> {
            let row = sqlx::query(SELECT_ACTIVATION_SQL)
                .bind(agent_id)
                .fetch_optional(self.pool().await?)
                .await
                .map_err(db_err("select activation"))?;
            row.as_ref().map(activation_from_row).transpose()
        }

        async fn claim_agent_task(
            &self,
            tenant: &str,
            scope: &MailboxClaimScope<'_>,
            lease_ms: u64,
            now: DateTime<Utc>,
        ) -> StoreResult<MailboxClaim> {
            let pool = self.pool().await?;
            let mut tx = pool.begin().await.map_err(db_err("claim agent task"))?;

            // Gate 1 — activation: the caller must hold the agent's live
            // lease. The row is locked FOR UPDATE for the rest of the
            // transaction, which does double duty: concurrent claims by
            // the one holder serialize on it, so the turn gate below is
            // exact rather than best-effort.
            let lease = sqlx::query(SELECT_ACTIVATION_FOR_UPDATE_SQL)
                .bind(scope.agent_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(db_err("claim agent task activation"))?;
            let Some(lease) = lease else {
                tx.rollback().await.map_err(db_err("claim agent task"))?;
                return Ok(MailboxClaim::ActivationLost);
            };
            if !activation_from_row(&lease)?.held_by(scope.owner, scope.fencing, now) {
                tx.rollback().await.map_err(db_err("claim agent task"))?;
                return Ok(MailboxClaim::ActivationLost);
            }

            // The same finalization sweep the pool claim runs: unanswered
            // cancels and elapsed deadlines turn terminal-cancelled
            // instead of being re-leased to a turn.
            sqlx::query(CLAIM_FINALIZE_SQL)
                .bind(tenant)
                .bind(now)
                .execute(&mut *tx)
                .await
                .map_err(db_err("claim agent task finalization"))?;

            // Gate 2 — turn serialization: a live-leased message already
            // in flight for this recipient makes the whole mailbox
            // unclaimable. One message at a time per agent is
            // server-enforced, not host discipline.
            let busy = sqlx::query(AGENT_TURN_IN_FLIGHT_SQL)
                .bind(tenant)
                .bind(scope.recipient)
                .bind(now)
                .fetch_optional(&mut *tx)
                .await
                .map_err(db_err("claim agent task turn gate"))?;
            if busy.is_some() {
                tx.rollback().await.map_err(db_err("claim agent task"))?;
                return Ok(MailboxClaim::Empty);
            }

            // The oldest claimable message for this mailbox — approximate
            // FIFO on the happy path, per the design's honest ordering
            // edge.
            let candidate = sqlx::query(AGENT_CLAIM_SELECT_SQL)
                .bind(tenant)
                .bind(scope.recipient)
                .bind(now)
                .fetch_optional(&mut *tx)
                .await
                .map_err(db_err("claim agent task"))?;
            let Some(candidate) = candidate else {
                tx.rollback().await.map_err(db_err("claim agent task"))?;
                return Ok(MailboxClaim::Empty);
            };
            // The granted task lease is the ordinary one — the turn
            // settles through the unchanged heartbeat/complete/fail
            // protocol.
            let expires_at = now + agents::lease_duration(lease_ms);
            let updated = sqlx::query(CLAIM_UPDATE_SQL)
                .bind(candidate.get::<String, _>("task_id"))
                .bind(scope.owner)
                .bind(expires_at)
                .bind(candidate.get::<i32, _>("attempt") + 1)
                .bind(now)
                .fetch_one(&mut *tx)
                .await
                .map_err(db_err("claim agent task"))?;
            tx.commit().await.map_err(db_err("claim agent task"))?;
            Ok(MailboxClaim::Claimed(Box::new(task_from_row(&updated)?)))
        }

        async fn put_memory(
            &self,
            tenant: &str,
            record: &MemoryRecord,
            content: &Value,
        ) -> StoreResult<bool> {
            let pool = self.pool().await?;
            // Spill before the insert (the file backend's rule): the blob
            // is content-addressed, so a failed insert leaves at worst a
            // reusable orphan — never a record pointing at missing bytes.
            memory::spill_content(&self.memory_artifacts().await?, record, content).await?;
            let payload = record_to_payload(record)?;
            let tags = serde_json::to_value(&record.tags)
                .map_err(|e| format!("serialize memory tags: {e}"))?;
            let row = sqlx::query(INSERT_MEMORY_SQL)
                .bind(crate::auth::scope_id(tenant, &record.memory_id))
                .bind(tenant)
                .bind(memory_wire_str(&record.kind)?)
                .bind(memory_wire_str(&record.scope.scope)?)
                .bind(&record.scope.id)
                .bind(&record.key)
                .bind(tags)
                .bind(record.confidence)
                .bind(record.validity.valid_from)
                .bind(record.validity.valid_until)
                .bind(record.expires_at)
                .bind(&record.supersedes)
                .bind(payload)
                .fetch_optional(pool)
                .await
                .map_err(db_err("insert memory"))?;
            Ok(row.is_some())
        }

        async fn get_memory(
            &self,
            tenant: &str,
            memory_id: &str,
        ) -> StoreResult<Option<MemoryRecord>> {
            let pool = self.pool().await?;
            let row = sqlx::query(SELECT_MEMORY_SQL)
                .bind(crate::auth::scope_id(tenant, memory_id))
                .fetch_optional(pool)
                .await
                .map_err(db_err("select memory"))?;
            let Some(row) = row else {
                return Ok(None);
            };
            let mut record: MemoryRecord =
                record_from_payload("memory", row.get::<Value, _>("payload"))?;
            memory::resolve_content(&self.memory_artifacts().await?, &mut record).await?;
            Ok(Some(record))
        }

        async fn query_memory(
            &self,
            tenant: &str,
            query: &MemoryQuery,
            now: chrono::DateTime<chrono::Utc>,
        ) -> StoreResult<Vec<MemoryRecord>> {
            let pool = self.pool().await?;
            let superseded_rows = sqlx::query(SUPERSEDED_MEMORY_SQL)
                .bind(tenant)
                .fetch_all(pool)
                .await
                .map_err(db_err("memory superseded scan"))?;
            let mut superseded: std::collections::HashSet<String> = superseded_rows
                .into_iter()
                .map(|row| row.get::<String, _>("supersedes"))
                .collect();
            // The wave-2 half of the set: a summary's named sources are
            // superseded too (core's `superseded_set` — one definition,
            // two backends).
            let summary_rows = sqlx::query(SUMMARY_SOURCES_MEMORY_SQL)
                .bind(tenant)
                .fetch_all(pool)
                .await
                .map_err(db_err("memory summary-source scan"))?;
            for row in summary_rows {
                let raw: Option<String> = row.get("source_ids");
                if let Some(ids) =
                    raw.and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
                {
                    superseded.extend(ids);
                }
            }

            // SQL pre-filters on the column-mapped clauses — each clause
            // spells exactly the `MemoryQuery::matches` semantics for its
            // filter, and every value travels as a bind parameter. The
            // same matcher then runs in Rust over the reduced set
            // (covering tags and author, and harmlessly re-checking the
            // rest), so filter semantics live in exactly one place: core.
            let mut sql = String::from("SELECT payload FROM server_memory WHERE tenant = $1");
            let mut binds = 1usize;
            if query.scope.is_some() {
                binds += 2;
                sql.push_str(&format!(
                    " AND scope = ${} AND scope_id = ${}",
                    binds - 1,
                    binds
                ));
            }
            if !query.kinds.is_empty() {
                binds += 1;
                sql.push_str(&format!(" AND kind = ANY(${binds})"));
            }
            if query.key.is_some() {
                binds += 1;
                sql.push_str(&format!(" AND \"key\" = ${binds}"));
            }
            if query.valid_at.is_some() {
                binds += 1;
                sql.push_str(&format!(
                    " AND valid_from <= ${0} AND (valid_until IS NULL OR valid_until > ${0})",
                    binds
                ));
            }
            if query.min_confidence.is_some() {
                binds += 1;
                sql.push_str(&format!(" AND confidence >= ${binds}"));
            }
            if !query.include_expired {
                binds += 1;
                sql.push_str(&format!(
                    " AND (expires_at IS NULL OR expires_at > ${binds})"
                ));
            }
            let mut stmt = sqlx::query(&sql).bind(tenant);
            if let Some(scope) = &query.scope {
                stmt = stmt.bind(memory_wire_str(&scope.scope)?).bind(&scope.id);
            }
            if !query.kinds.is_empty() {
                let kinds = query
                    .kinds
                    .iter()
                    .map(memory_wire_str)
                    .collect::<StoreResult<Vec<String>>>()?;
                stmt = stmt.bind(kinds);
            }
            if let Some(key) = &query.key {
                stmt = stmt.bind(key);
            }
            if let Some(valid_at) = query.valid_at {
                stmt = stmt.bind(valid_at);
            }
            if let Some(min_confidence) = query.min_confidence {
                stmt = stmt.bind(min_confidence);
            }
            if !query.include_expired {
                stmt = stmt.bind(now);
            }
            let rows = stmt.fetch_all(pool).await.map_err(db_err("query memory"))?;
            let artifacts = self.memory_artifacts().await?;
            let mut matched = Vec::with_capacity(rows.len());
            for row in rows {
                let mut record: MemoryRecord =
                    record_from_payload("memory", row.get::<Value, _>("payload"))?;
                if query.matches(&record, superseded.contains(&record.memory_id), now) {
                    // Resolve after filtering: only served records pay
                    // the artifact read.
                    memory::resolve_content(&artifacts, &mut record).await?;
                    matched.push(record);
                }
            }
            Ok(matched)
        }

        async fn delete_memory(&self, tenant: &str, memory_id: &str) -> StoreResult<bool> {
            let row = sqlx::query(DELETE_MEMORY_SQL)
                .bind(crate::auth::scope_id(tenant, memory_id))
                .fetch_optional(self.pool().await?)
                .await
                .map_err(db_err("delete memory"))?;
            Ok(row.is_some())
        }

        async fn put_candidate(&self, tenant: &str, record: &CandidateRecord) -> StoreResult<bool> {
            let pool = self.pool().await?;
            let candidate = &record.candidate;
            let row = sqlx::query(INSERT_LEARN_CANDIDATE_SQL)
                .bind(crate::auth::scope_id(
                    tenant,
                    candidate.candidate_id.as_str(),
                ))
                .bind(tenant)
                .bind(memory_wire_str(&candidate.kind())?)
                .bind(candidate.surface().as_str())
                .bind(memory_wire_str(&record.status)?)
                .bind(record_to_payload(record)?)
                .fetch_optional(pool)
                .await
                .map_err(db_err("insert candidate"))?;
            Ok(row.is_some())
        }

        async fn get_candidate(
            &self,
            tenant: &str,
            candidate_id: &str,
        ) -> StoreResult<Option<CandidateRecord>> {
            let row = sqlx::query(SELECT_LEARN_CANDIDATE_SQL)
                .bind(crate::auth::scope_id(tenant, candidate_id))
                .fetch_optional(self.pool().await?)
                .await
                .map_err(db_err("select candidate"))?;
            row.map(|row| record_from_payload("candidate", row.get::<Value, _>("payload")))
                .transpose()
        }

        async fn list_candidates(&self, tenant: &str) -> StoreResult<Vec<CandidateRecord>> {
            let rows = sqlx::query(LIST_LEARN_CANDIDATES_SQL)
                .bind(tenant)
                .fetch_all(self.pool().await?)
                .await
                .map_err(db_err("list candidates"))?;
            rows.into_iter()
                .map(|row| record_from_payload("candidate", row.get::<Value, _>("payload")))
                .collect()
        }

        async fn transition_candidate(
            &self,
            tenant: &str,
            candidate_id: &str,
            expect: CandidateStatus,
            next: &CandidateRecord,
            pointer: Option<&VersionPointer>,
        ) -> StoreResult<CandidateTransition> {
            let pool = self.pool().await?;
            let scoped = crate::auth::scope_id(tenant, candidate_id);
            // Status flip and pointer move in one transaction: a crash
            // cannot leave a promoted candidate whose pointer never
            // moved — the file backend's lock-pair rule, exact here.
            let mut tx = pool
                .begin()
                .await
                .map_err(db_err("begin candidate transition"))?;
            let row = sqlx::query(LOCK_LEARN_CANDIDATE_SQL)
                .bind(&scoped)
                .fetch_optional(&mut *tx)
                .await
                .map_err(db_err("lock candidate"))?;
            let Some(row) = row else {
                return Ok(CandidateTransition::Unknown);
            };
            let live: String = row.get("status");
            let live_status: CandidateStatus = serde_json::from_value(Value::String(live))
                .map_err(|e| format!("corrupt candidate status: {e}"))?;
            if live_status != expect {
                return Ok(CandidateTransition::Conflict(live_status));
            }
            sqlx::query(UPDATE_LEARN_CANDIDATE_SQL)
                .bind(&scoped)
                .bind(memory_wire_str(&next.status)?)
                .bind(record_to_payload(next)?)
                .execute(&mut *tx)
                .await
                .map_err(db_err("update candidate"))?;
            if let Some(pointer) = pointer {
                sqlx::query(UPSERT_LEARN_VERSION_SQL)
                    .bind(crate::auth::scope_id(tenant, pointer.surface.as_str()))
                    .bind(tenant)
                    .bind(record_to_payload(pointer)?)
                    .execute(&mut *tx)
                    .await
                    .map_err(db_err("upsert version pointer"))?;
            }
            tx.commit()
                .await
                .map_err(db_err("commit candidate transition"))?;
            Ok(CandidateTransition::Applied)
        }

        async fn get_version_pointer(
            &self,
            tenant: &str,
            surface: &str,
        ) -> StoreResult<Option<VersionPointer>> {
            let row = sqlx::query(SELECT_LEARN_VERSION_SQL)
                .bind(crate::auth::scope_id(tenant, surface))
                .fetch_optional(self.pool().await?)
                .await
                .map_err(db_err("select version pointer"))?;
            row.map(|row| record_from_payload("version pointer", row.get::<Value, _>("payload")))
                .transpose()
        }

        async fn list_version_pointers(&self, tenant: &str) -> StoreResult<Vec<VersionPointer>> {
            let rows = sqlx::query(LIST_LEARN_VERSIONS_SQL)
                .bind(tenant)
                .fetch_all(self.pool().await?)
                .await
                .map_err(db_err("list version pointers"))?;
            rows.into_iter()
                .map(|row| record_from_payload("version pointer", row.get::<Value, _>("payload")))
                .collect()
        }

        async fn put_policy(
            &self,
            tenant: &str,
            record: &PolicyRecord,
        ) -> StoreResult<PolicyWrite> {
            let pool = self.pool().await?;
            let scoped = crate::auth::scope_id(tenant, record.version.as_str());
            let row = sqlx::query(INSERT_POLICY_SQL)
                .bind(&scoped)
                .bind(tenant)
                .bind(record.version.as_str())
                .bind(record_to_payload(record)?)
                .fetch_optional(pool)
                .await
                .map_err(db_err("insert policy"))?;
            if row.is_some() {
                return Ok(PolicyWrite::Created);
            }
            // The version is taken: same body converges, different body
            // conflicts — the file backend's immutability rule, exact here.
            let existing = sqlx::query(SELECT_POLICY_SQL)
                .bind(&scoped)
                .fetch_optional(pool)
                .await
                .map_err(db_err("select policy"))?
                .ok_or_else(|| "policy insert conflicted but the row is gone".to_string())?;
            let existing: PolicyRecord =
                record_from_payload("policy", existing.get::<Value, _>("payload"))?;
            Ok(
                if existing.version == record.version
                    && existing.policy == record.policy
                    && existing.source == record.source
                {
                    PolicyWrite::Converged
                } else {
                    PolicyWrite::Conflict
                },
            )
        }

        async fn get_policy(
            &self,
            tenant: &str,
            version: &str,
        ) -> StoreResult<Option<PolicyRecord>> {
            let row = sqlx::query(SELECT_POLICY_SQL)
                .bind(crate::auth::scope_id(tenant, version))
                .fetch_optional(self.pool().await?)
                .await
                .map_err(db_err("select policy"))?;
            row.map(|row| record_from_payload("policy", row.get::<Value, _>("payload")))
                .transpose()
        }

        async fn list_policies(&self, tenant: &str) -> StoreResult<Vec<PolicyRecord>> {
            let rows = sqlx::query(LIST_POLICIES_SQL)
                .bind(tenant)
                .fetch_all(self.pool().await?)
                .await
                .map_err(db_err("list policies"))?;
            rows.into_iter()
                .map(|row| record_from_payload("policy", row.get::<Value, _>("payload")))
                .collect()
        }

        async fn append_policy_activation(
            &self,
            tenant: &str,
            activation: &PolicyActivation,
        ) -> StoreResult<()> {
            sqlx::query(INSERT_POLICY_ACTIVATION_SQL)
                .bind(tenant)
                .bind(activation.version.as_str())
                .bind(activation.activated_at)
                .execute(self.pool().await?)
                .await
                .map_err(db_err("append policy activation"))?;
            Ok(())
        }

        async fn list_policy_activations(
            &self,
            tenant: &str,
        ) -> StoreResult<Vec<PolicyActivation>> {
            let rows = sqlx::query(LIST_POLICY_ACTIVATIONS_SQL)
                .bind(tenant)
                .fetch_all(self.pool().await?)
                .await
                .map_err(db_err("list policy activations"))?;
            Ok(rows
                .into_iter()
                .map(|row| PolicyActivation {
                    version: PolicyVersion::new(row.get::<String, _>("version")),
                    activated_at: row.get("activated_at"),
                })
                .collect())
        }

        async fn put_policy_binding(
            &self,
            tenant: &str,
            binding: &PolicyBinding,
        ) -> StoreResult<()> {
            sqlx::query(INSERT_POLICY_BINDING_SQL)
                .bind(crate::auth::scope_id(tenant, &binding.checkpoint_id))
                .bind(tenant)
                .bind(&binding.thread_id)
                .bind(binding.version.as_str())
                .bind(record_to_payload(binding)?)
                .bind(binding.bound_at)
                .execute(self.pool().await?)
                .await
                .map_err(db_err("insert policy binding"))?;
            Ok(())
        }

        async fn list_policy_bindings(&self, tenant: &str) -> StoreResult<Vec<PolicyBinding>> {
            let rows = sqlx::query(LIST_POLICY_BINDINGS_SQL)
                .bind(tenant)
                .fetch_all(self.pool().await?)
                .await
                .map_err(db_err("list policy bindings"))?;
            rows.into_iter()
                .map(|row| record_from_payload("policy binding", row.get::<Value, _>("payload")))
                .collect()
        }
    }

    impl PostgresStore {
        /// Map a lease-guarded update's outcome: the updated row means
        /// applied; no row means either the task is unknown to this tenant
        /// (404) or the lease check failed (409) — the existence probe
        /// decides.
        async fn lease_outcome(
            &self,
            tenant: &str,
            task_id: &str,
            updated: Option<sqlx::postgres::PgRow>,
        ) -> StoreResult<MutationOutcome> {
            if let Some(row) = updated {
                return Ok(MutationOutcome::Applied(Box::new(task_from_row(&row)?)));
            }
            let exists = sqlx::query(TASK_EXISTS_SQL)
                .bind(task_id)
                .bind(tenant)
                .fetch_optional(self.pool().await?)
                .await
                .map_err(db_err("task existence probe"))?;
            Ok(if exists.is_some() {
                MutationOutcome::LeaseLost
            } else {
                MutationOutcome::Unknown
            })
        }

        /// Map a guarded activation mutation's outcome (the renew and
        /// release statements share their guard shape): the returned row
        /// means applied; no row means either no lease exists at all
        /// (unknown → 404) or the owner + fencing + liveness check failed
        /// (fencing lost → 409) — the existence probe decides. Same
        /// discipline as [`PostgresStore::lease_outcome`].
        async fn activation_outcome(
            &self,
            agent_id: &str,
            updated: Option<sqlx::postgres::PgRow>,
        ) -> StoreResult<ActivationMutation> {
            if let Some(row) = updated {
                return Ok(ActivationMutation::Applied(Box::new(activation_from_row(
                    &row,
                )?)));
            }
            let exists = sqlx::query(SELECT_ACTIVATION_SQL)
                .bind(agent_id)
                .fetch_optional(self.pool().await?)
                .await
                .map_err(db_err("activation existence probe"))?;
            Ok(if exists.is_some() {
                ActivationMutation::FencingLost
            } else {
                ActivationMutation::Unknown
            })
        }
    }

    /// Lazily-connecting [`Checkpointer`] facade over core's
    /// [`PostgresCheckpointer`]: connects (and auto-migrates
    /// `rusty_checkpoints`) on first checkpoint operation, keeping
    /// [`crate::router`] synchronous.
    pub(crate) struct LazyPostgresCheckpointer {
        url: String,
        inner: OnceCell<rusty_agent_runtime::checkpoint_postgres::PostgresCheckpointer>,
    }

    impl LazyPostgresCheckpointer {
        /// A checkpointer that will connect to `url` on first use.
        pub(crate) fn new(url: String) -> Self {
            Self {
                url,
                inner: OnceCell::new(),
            }
        }

        async fn cp(
            &self,
        ) -> rusty_agent_runtime::error::Result<
            &rusty_agent_runtime::checkpoint_postgres::PostgresCheckpointer,
        > {
            self.inner
                .get_or_try_init(|| {
                    rusty_agent_runtime::checkpoint_postgres::PostgresCheckpointer::connect(
                        &self.url,
                    )
                })
                .await
        }
    }

    #[async_trait::async_trait]
    impl rusty_agent_runtime::checkpoint::Checkpointer for LazyPostgresCheckpointer {
        async fn put(
            &self,
            checkpoint: rusty_agent_runtime::checkpoint::Checkpoint,
        ) -> rusty_agent_runtime::error::Result<()> {
            self.cp().await?.put(checkpoint).await
        }

        async fn get_latest(
            &self,
            thread_id: &str,
        ) -> rusty_agent_runtime::error::Result<Option<rusty_agent_runtime::checkpoint::Checkpoint>>
        {
            self.cp().await?.get_latest(thread_id).await
        }

        async fn list(
            &self,
            thread_id: &str,
        ) -> rusty_agent_runtime::error::Result<Vec<rusty_agent_runtime::checkpoint::Checkpoint>>
        {
            self.cp().await?.list(thread_id).await
        }

        async fn get_by_id(
            &self,
            thread_id: &str,
            checkpoint_id: &str,
        ) -> rusty_agent_runtime::error::Result<Option<rusty_agent_runtime::checkpoint::Checkpoint>>
        {
            self.cp().await?.get_by_id(thread_id, checkpoint_id).await
        }

        async fn fork_thread(
            &self,
            src_thread: &str,
            dst_thread: &str,
            at_checkpoint_id: Option<&str>,
        ) -> rusty_agent_runtime::error::Result<usize> {
            self.cp()
                .await?
                .fork_thread(src_thread, dst_thread, at_checkpoint_id)
                .await
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use serde_json::json;

        #[test]
        fn migration_sql_creates_all_tables_idempotently() {
            assert_eq!(MIGRATION_SQL.len(), 35);
            for stmt in MIGRATION_SQL {
                assert!(
                    stmt.contains("IF NOT EXISTS"),
                    "migration must be idempotent: {stmt}"
                );
            }
            assert!(CREATE_ASSISTANTS_SQL.contains("server_assistants"));
            assert!(CREATE_ASSISTANTS_SQL.contains("JSONB"));
            assert!(CREATE_CRONS_SQL.contains("server_crons"));
            assert!(CREATE_CRONS_SQL.contains("JSONB"));
            assert!(CREATE_THREADS_SQL.contains("server_threads"));
            assert!(CREATE_THREADS_SQL.contains("JSONB"));
            assert!(CREATE_KV_SQL.contains("server_kv"));
            assert!(CREATE_KV_SQL.contains("JSONB"));
            assert!(CREATE_KV_SQL.contains("PRIMARY KEY (namespace"));
            assert!(CREATE_JOURNALS_SQL.contains("server_journals"));
            assert!(CREATE_JOURNALS_SQL.contains("JSONB"));
            assert!(CREATE_JOURNALS_SQL.contains("TEXT PRIMARY KEY"));
            assert!(CREATE_TASKS_SQL.contains("server_tasks"));
            assert!(CREATE_TASKS_SQL.contains("TEXT PRIMARY KEY"));
            assert!(CREATE_TASKS_IDEMPOTENCY_INDEX_SQL.contains("CREATE UNIQUE INDEX"));
            assert!(CREATE_TASKS_CLAIMABLE_INDEX_SQL.contains("CREATE INDEX"));
            // Additive columns for pre-wave-2 databases arrive as ALTERs.
            assert!(ALTER_TASKS_ADD_CANCEL_REQUESTED_SQL.contains("ALTER TABLE server_tasks"));
            assert!(ALTER_TASKS_ADD_DEADLINE_SQL.contains("ALTER TABLE server_tasks"));
            assert!(ALTER_TASKS_ADD_RECEIPT_SQL.contains("ALTER TABLE server_tasks"));
            assert!(ALTER_TASKS_ADD_RECEIPT_SQL.contains("JSONB"));
            // Wave 3: the version pin arrives the same additive way.
            assert!(ALTER_TASKS_ADD_WORKER_VERSION_SQL.contains("ALTER TABLE server_tasks"));
            assert!(ALTER_TASKS_ADD_WORKER_VERSION_SQL.contains("worker_version TEXT"));
            // The outbox table: 1:1 with its task, pending until published.
            assert!(CREATE_OUTBOX_SQL.contains("server_outbox"));
            assert!(CREATE_OUTBOX_SQL.contains("TEXT PRIMARY KEY"));
            assert!(CREATE_OUTBOX_SQL.contains("published_at TIMESTAMPTZ"));
            assert!(CREATE_OUTBOX_PENDING_INDEX_SQL.contains("CREATE INDEX"));
            assert!(CREATE_OUTBOX_PENDING_INDEX_SQL.contains("WHERE published_at IS NULL"));
            // R0.7 wave 1: the recipient column arrives additively, and the
            // agent registry + activation leases get their own tables.
            assert!(ALTER_TASKS_ADD_RECIPIENT_SQL.contains("ALTER TABLE server_tasks"));
            assert!(ALTER_TASKS_ADD_RECIPIENT_SQL.contains("recipient TEXT"));
            assert!(CREATE_AGENTS_SQL.contains("server_agents"));
            assert!(CREATE_AGENTS_SQL.contains("JSONB"));
            assert!(CREATE_AGENT_LEASES_SQL.contains("server_agent_leases"));
            assert!(CREATE_AGENT_LEASES_SQL.contains("fencing"));
            assert!(CREATE_AGENT_LEASES_SQL.contains("expires_at"));
            // R0.7 wave 3: causal parentage and settlement cost arrive
            // additively; the coordination registry gets its own table.
            assert!(ALTER_TASKS_ADD_PARENT_SQL.contains("ALTER TABLE server_tasks"));
            assert!(ALTER_TASKS_ADD_PARENT_SQL.contains("parent TEXT"));
            assert!(ALTER_TASKS_ADD_TOKENS_SQL.contains("ALTER TABLE server_tasks"));
            assert!(ALTER_TASKS_ADD_TOKENS_SQL.contains("tokens JSONB"));
            assert!(ALTER_TASKS_ADD_COST_USD_SQL.contains("ALTER TABLE server_tasks"));
            assert!(ALTER_TASKS_ADD_COST_USD_SQL.contains("cost_usd DOUBLE PRECISION"));
            assert!(CREATE_COORDINATIONS_SQL.contains("server_coordinations"));
            assert!(CREATE_COORDINATIONS_SQL.contains("JSONB"));
            // Triggers: records as JSONB like assistants/crons; events carry
            // a real trigger_id column for the per-trigger listing + prune.
            assert!(CREATE_TRIGGERS_SQL.contains("server_triggers"));
            assert!(CREATE_TRIGGERS_SQL.contains("JSONB"));
            assert!(CREATE_TRIGGER_EVENTS_SQL.contains("server_trigger_events"));
            assert!(CREATE_TRIGGER_EVENTS_SQL.contains("trigger_id TEXT NOT NULL"));
            assert!(CREATE_TRIGGER_EVENTS_INDEX_SQL.contains("(trigger_id, created_at, event_id)"));
            // R0.8 wave 1: governed memory is column-mapped (retrieval
            // filters on real columns), the record travels as JSONB, and
            // `key` is quoted like `server_kv`'s (a reserved word).
            assert!(CREATE_MEMORY_SQL.contains("server_memory"));
            assert!(CREATE_MEMORY_SQL.contains("\"key\""));
            assert!(CREATE_MEMORY_SQL.contains("confidence  DOUBLE PRECISION"));
            assert!(CREATE_MEMORY_SQL.contains("valid_until TIMESTAMPTZ"));
            assert!(CREATE_MEMORY_SQL.contains("payload     JSONB"));
            assert!(CREATE_MEMORY_QUERY_INDEX_SQL.contains("(tenant, scope, scope_id, kind)"));
            // R0.8 wave 3: candidates are column-mapped on the lifecycle
            // (the transition locks `status` FOR UPDATE; listings filter
            // on surface/status); pointers are one upserted row per
            // tenant-scoped surface.
            assert!(CREATE_LEARN_CANDIDATES_SQL.contains("server_learn_candidates"));
            assert!(CREATE_LEARN_CANDIDATES_SQL.contains("status       TEXT NOT NULL"));
            assert!(CREATE_LEARN_CANDIDATES_SQL.contains("payload      JSONB"));
            assert!(CREATE_LEARN_CANDIDATES_INDEX_SQL.contains("(tenant, surface, status)"));
            assert!(CREATE_LEARN_VERSIONS_SQL.contains("server_learn_versions"));
            assert!(CREATE_LEARN_VERSIONS_SQL.contains("surface    TEXT PRIMARY KEY"));
            // R0.8 wave 4: the policy registry — immutable bodies keyed by
            // tenant-scoped version, an append-only activation log ordered
            // by its serial key, and the denormalized binding index.
            assert!(CREATE_POLICIES_SQL.contains("server_policies"));
            assert!(CREATE_POLICIES_SQL.contains("policy_id  TEXT PRIMARY KEY"));
            assert!(CREATE_POLICIES_SQL.contains("payload    JSONB"));
            assert!(CREATE_POLICIES_INDEX_SQL.contains("(tenant, version)"));
            assert!(CREATE_POLICY_ACTIVATIONS_SQL.contains("server_policy_activations"));
            assert!(CREATE_POLICY_ACTIVATIONS_SQL.contains("BIGSERIAL PRIMARY KEY"));
            assert!(CREATE_POLICY_ACTIVATIONS_INDEX_SQL.contains("(tenant, id)"));
            assert!(CREATE_POLICY_BINDINGS_SQL.contains("server_policy_bindings"));
            assert!(CREATE_POLICY_BINDINGS_SQL.contains("binding_id TEXT PRIMARY KEY"));
            assert!(CREATE_POLICY_BINDINGS_INDEX_SQL.contains("(tenant, bound_at)"));
        }

        #[test]
        fn tasks_schema_has_claim_columns_and_scoped_idempotency() {
            // Claiming filters and locks on real columns (not JSONB).
            for col in [
                "tenant",
                "pool",
                "status",
                "lease_owner",
                "lease_expires_at",
                "next_attempt_at",
                "attempt",
                "max_attempts",
                "effect",
                "cancel_requested",
                "deadline",
                "receipt",
                "worker_version",
                "recipient",
                "parent",
                "tokens",
                "cost_usd",
            ] {
                assert!(CREATE_TASKS_SQL.contains(col), "missing column {col}");
            }
            // Dedup is per tenant and partial: keyless tasks never conflict.
            assert!(CREATE_TASKS_IDEMPOTENCY_INDEX_SQL.contains("(tenant, idempotency_key)"));
            assert!(
                CREATE_TASKS_IDEMPOTENCY_INDEX_SQL.contains("WHERE idempotency_key IS NOT NULL")
            );
        }

        #[test]
        fn claim_sql_locks_and_skips_locked_rows() {
            assert!(CLAIM_SELECT_SQL.contains("FOR UPDATE SKIP LOCKED"));
            assert!(CLAIM_SELECT_SQL.contains("pool = ANY($2)"));
            assert!(CLAIM_SELECT_SQL.contains("status IN ('queued', 'failed')"));
            assert!(CLAIM_SELECT_SQL.contains("status = 'leased'"));
            assert!(CLAIM_SELECT_SQL.contains("lease_expires_at <= $3"));
            // Wave 3: saturated pools are excluded and the version pin
            // matches exactly (NULL pin = any worker; NULL advertisement =
            // unpinned tasks only), mirroring
            // `TaskRecord::matches_worker_version`.
            assert!(CLAIM_SELECT_SQL.contains("NOT (pool = ANY($4))"));
            assert!(CLAIM_SELECT_SQL.contains("worker_version IS NULL OR worker_version = $5"));
            // R0.7: pool claims never hand out mailbox traffic.
            assert!(CLAIM_SELECT_SQL.contains("recipient IS NULL"));
            assert!(CLAIM_INFLIGHT_SQL.contains("status = 'leased'"));
            assert!(CLAIM_INFLIGHT_SQL.contains("lease_expires_at > $2"));
            assert!(CLAIM_UPDATE_SQL.contains("status = 'leased'"));
            assert!(CLAIM_UPDATE_SQL.contains("next_attempt_at = NULL"));
            // Finalization turns unanswered cancels and elapsed deadlines
            // terminal-cancelled before any candidate is selected.
            assert!(CLAIM_FINALIZE_SQL.contains("status = 'cancelled'"));
            assert!(CLAIM_FINALIZE_SQL.contains("cancel_requested"));
            assert!(CLAIM_FINALIZE_SQL.contains("deadline <= $2"));
            assert!(CLAIM_FINALIZE_SQL.contains("lease_expires_at <= $2"));
        }

        #[test]
        fn cancel_sql_locks_then_applies_the_record_transition() {
            // Same discipline as fail: lock the row, decide in Rust, write.
            assert!(CANCEL_SELECT_SQL.contains("FOR UPDATE"));
            assert!(CANCEL_UPDATE_SQL.contains("cancel_requested = $4"));
            // Run cancel splits immediate finalization from holder signalling.
            assert!(CANCEL_RUN_FINALIZE_SQL.contains("status = 'cancelled'"));
            assert!(CANCEL_RUN_FINALIZE_SQL.contains("run_id = $2"));
            assert!(CANCEL_RUN_SIGNAL_SQL.contains("cancel_requested = TRUE"));
            assert!(CANCEL_RUN_SIGNAL_SQL.contains("status = 'leased'"));
        }

        #[test]
        fn lease_guarded_updates_check_owner_tenant_and_leased_status() {
            for sql in [HEARTBEAT_TASK_SQL, COMPLETE_TASK_SQL] {
                assert!(sql.contains("task_id = $1 AND tenant = $2 AND lease_owner = $3"));
                assert!(sql.contains("status = 'leased'"));
            }
            // Complete also stores the reported effect receipt (additive
            // JSONB column; NULL when none is reported) and the wave-3
            // settlement cost evidence.
            assert!(COMPLETE_TASK_SQL.contains("receipt = $6"));
            assert!(COMPLETE_TASK_SQL.contains("tokens = $7, cost_usd = $8"));
            // Fail locks the row first: the attempt count read and the
            // requeue/dead write must serialize against other settlers.
            assert!(FAIL_SELECT_SQL.contains("FOR UPDATE"));
            assert!(FAIL_UPDATE_SQL.contains("lease_owner = NULL"));
            assert!(FAIL_UPDATE_SQL.contains("tokens = $7, cost_usd = $8"));
        }

        #[test]
        fn outbox_sql_is_locking_deduped_and_atomic_per_row() {
            // The relay picks the oldest pending row under SKIP LOCKED, so
            // concurrent publishers take distinct rows.
            assert!(SELECT_OUTBOX_PENDING_SQL.contains("WHERE published_at IS NULL"));
            assert!(SELECT_OUTBOX_PENDING_SQL.contains("FOR UPDATE SKIP LOCKED"));
            assert!(SELECT_OUTBOX_PENDING_SQL.contains("LIMIT 1"));
            // Outbox writes dedupe on outbox_id (== task_id), making
            // retried checkpoint+enqueue pairs no-ops.
            assert!(INSERT_OUTBOX_SQL.contains("ON CONFLICT (outbox_id) DO NOTHING"));
            // The checkpoint half keeps the no-overwrite contract: no
            // ON CONFLICT, so a duplicate id aborts the whole transaction.
            assert!(INSERT_CHECKPOINT_SQL.contains("INSERT INTO rusty_checkpoints"));
            assert!(!INSERT_CHECKPOINT_SQL.to_uppercase().contains("ON CONFLICT"));
        }

        #[test]
        fn usage_and_pool_stats_sql_match_the_json_backends_gauges() {
            // The quota gauges: backlog counts scheduled retries, in-flight
            // is status-based, pending outbox rows count as queued.
            assert!(TASK_USAGE_SQL.contains("status = 'queued'"));
            assert!(TASK_USAGE_SQL.contains("status = 'failed' AND next_attempt_at IS NOT NULL"));
            assert!(TASK_USAGE_SQL.contains("status = 'leased'"));
            assert!(TASK_USAGE_SQL.contains("status = 'dead'"));
            // The pending-outbox count rides in the same statement (one
            // MVCC snapshot), not a second query.
            assert!(TASK_USAGE_SQL.contains("FROM server_outbox"));
            assert!(TASK_USAGE_SQL.contains("published_at IS NULL"));
            // The autoscaling signals: saturation counts only live leases;
            // visibility is the claim path's own rule (queued, backoff-
            // elapsed, or lease-expired).
            assert!(POOL_STATS_SQL.contains("status = 'leased' AND lease_expires_at > $2"));
            assert!(POOL_STATS_SQL.contains("status = 'leased' AND lease_expires_at <= $2"));
            assert!(POOL_STATS_SQL.contains("GROUP BY pool"));
        }

        #[test]
        fn agent_fabric_sql_locks_the_lease_row_and_guards_fencing() {
            // The claim's steal decision and the mailbox claim's
            // activation gate both serialize on the lease row's FOR UPDATE.
            assert!(SELECT_ACTIVATION_FOR_UPDATE_SQL.contains("FOR UPDATE"));
            // The steal bumps the fencing ordinal, so a dead host's stale
            // owner + fencing pair can never pass a guard again.
            assert!(STEAL_ACTIVATION_SQL.contains("fencing = fencing + 1"));
            // Renew and release hold only for the exact live holder.
            for sql in [RENEW_ACTIVATION_SQL, RELEASE_ACTIVATION_SQL] {
                assert!(sql.contains("agent_id = $1 AND owner = $2 AND fencing = $3"));
                assert!(sql.contains("expires_at > $"));
            }
            // Turn serialization is server-enforced: a live-leased message
            // in flight for the recipient makes the mailbox unclaimable,
            // and the candidate select excludes nothing else.
            assert!(AGENT_TURN_IN_FLIGHT_SQL.contains("recipient = $2"));
            assert!(AGENT_TURN_IN_FLIGHT_SQL.contains("lease_expires_at > $3"));
            assert!(AGENT_CLAIM_SELECT_SQL.contains("recipient = $2"));
            assert!(AGENT_CLAIM_SELECT_SQL.contains("FOR UPDATE SKIP LOCKED"));
            assert!(!AGENT_CLAIM_SELECT_SQL.contains("pool"));
        }

        #[test]
        fn supervision_sql_mirrors_the_cancel_twin_and_dead_letters_atomically() {
            // Agent cancel is the recipient-scoped twin of run cancel:
            // same two transitions, the recipient filter in place of the
            // run-id one.
            assert!(CANCEL_AGENT_FINALIZE_SQL.contains("status = 'cancelled'"));
            assert!(CANCEL_AGENT_FINALIZE_SQL.contains("recipient = $2"));
            assert!(CANCEL_AGENT_FINALIZE_SQL.contains(
                "status = 'queued' OR (status = 'failed' AND next_attempt_at IS NOT NULL)"
            ));
            assert!(CANCEL_AGENT_SIGNAL_SQL.contains("cancel_requested = TRUE"));
            assert!(CANCEL_AGENT_SIGNAL_SQL.contains("recipient = $2"));
            assert!(CANCEL_AGENT_SIGNAL_SQL.contains("status = 'leased'"));
            // The supervision record update is a whole-payload overwrite —
            // no column surgery, no migration for the additive fields.
            assert!(UPDATE_AGENT_SQL.contains("UPDATE server_agents SET payload = $2"));
            assert!(UPDATE_AGENT_SQL.contains("WHERE agent_id = $1"));
            // The dead-letter lands terminal-'dead' in one insert; the
            // idempotency index makes a retried escalation a no-op, read
            // back by key like the enqueue dedup.
            assert!(INSERT_DEAD_LETTER_SQL.contains("'dead'"));
            assert!(INSERT_DEAD_LETTER_SQL.contains("ON CONFLICT DO NOTHING"));
            assert!(INSERT_DEAD_LETTER_SQL.contains("RETURNING task_id"));
            assert!(SELECT_TASK_BY_IDEMPOTENCY_SQL.contains("idempotency_key = $2"));
        }

        #[test]
        fn journal_upsert_sql_overwrites_payload_and_bumps_updated_at() {
            assert!(UPSERT_JOURNAL_SQL.contains("ON CONFLICT (run_id) DO UPDATE"));
            assert!(UPSERT_JOURNAL_SQL.contains("payload = EXCLUDED.payload"));
            assert!(UPSERT_JOURNAL_SQL.contains("updated_at = now()"));
        }

        #[test]
        fn journal_payload_round_trip() {
            use rusty_agent_runtime::journal::{Clock, EventDraft, Journal};
            use rusty_agent_runtime::record::{Effect, RunEventKind};

            let journal = Journal::new("run-1", "thread-1", Clock::System);
            journal.record(EventDraft::new(RunEventKind::SuperStepStart, Effect::Pure));
            let snapshot = journal.snapshot();
            let payload = record_to_payload(&snapshot).unwrap();
            let back: JournalSnapshot = record_from_payload("journal", payload).unwrap();
            assert_eq!(back.run_id, snapshot.run_id);
            assert_eq!(back.thread_id, snapshot.thread_id);
            assert_eq!(back.events, snapshot.events);
            assert_eq!(back.head_hash, snapshot.head_hash);
        }

        #[test]
        fn kv_upsert_sql_preserves_created_at_and_reports_created_flag() {
            assert!(UPSERT_KV_SQL.contains("ON CONFLICT (namespace, \"key\") DO UPDATE"));
            assert!(UPSERT_KV_SQL.contains("updated_at = EXCLUDED.updated_at"));
            // The existing-row probe feeds the 201-vs-200 `created` flag.
            assert!(UPSERT_KV_SQL.contains("e.created_at IS NULL"));
        }

        #[test]
        fn cron_upsert_sql_overwrites_payload() {
            assert!(UPSERT_CRON_SQL.contains("ON CONFLICT (cron_id) DO UPDATE"));
            assert!(UPSERT_CRON_SQL.contains("payload = EXCLUDED.payload"));
        }

        #[test]
        fn assistant_payload_round_trip() {
            let record = AssistantRecord {
                assistant_id: "a-1".to_string(),
                name: "support-bot".to_string(),
                graph: "pipeline".to_string(),
                config: json!({"recursion_limit": 10}),
                metadata: json!({"team": "qa"}),
                created_at: Utc::now(),
            };
            let payload = record_to_payload(&record).unwrap();
            let back: AssistantRecord = record_from_payload("assistant", payload).unwrap();
            assert_eq!(back.assistant_id, record.assistant_id);
            assert_eq!(back.name, record.name);
            assert_eq!(back.graph, record.graph);
            assert_eq!(back.config, record.config);
            assert_eq!(back.metadata, record.metadata);
            assert_eq!(back.created_at, record.created_at);
        }

        #[test]
        fn cron_payload_round_trip() {
            let record = CronRecord {
                cron_id: "c-1".to_string(),
                graph: "pipeline".to_string(),
                interval_secs: Some(60),
                cron_expr: None,
                input: Some(json!({"seed": 1})),
                metadata: json!(null),
                on_run_completed: Default::default(),
                created_at: Utc::now(),
                last_run_at: Some(Utc::now()),
                runs_fired: 3,
            };
            let payload = record_to_payload(&record).unwrap();
            let back: CronRecord = record_from_payload("cron", payload).unwrap();
            assert_eq!(back.cron_id, record.cron_id);
            assert_eq!(back.interval_secs, record.interval_secs);
            assert_eq!(back.cron_expr, record.cron_expr);
            assert_eq!(back.input, record.input);
            assert_eq!(back.runs_fired, record.runs_fired);
            assert_eq!(back.last_run_at, record.last_run_at);
        }

        #[test]
        fn thread_payload_round_trip() {
            let record = ThreadRecord {
                thread_id: "t-1".to_string(),
                tenant: "acme".to_string(),
                graph: "pipeline".to_string(),
                metadata: json!({"origin": "cron"}),
                created_at: Utc::now(),
            };
            let payload = record_to_payload(&record).unwrap();
            let back: ThreadRecord = record_from_payload("thread", payload).unwrap();
            assert_eq!(back.thread_id, record.thread_id);
            assert_eq!(back.tenant, record.tenant);
            assert_eq!(back.graph, record.graph);
            assert_eq!(back.metadata, record.metadata);
            assert_eq!(back.created_at, record.created_at);
        }

        #[test]
        fn corrupt_payload_is_an_error_not_a_panic() {
            let result = record_from_payload::<AssistantRecord>("assistant", json!({"nope": 1}));
            assert!(result.is_err());
            assert!(result.unwrap_err().contains("corrupt assistant payload"));
        }

        #[test]
        fn kv_row_to_item_maps_all_columns() {
            let created = Utc::now();
            let updated = created + chrono::Duration::seconds(5);
            let item = kv_row_to_item("ns", "k", json!({"v": 1}), created, updated);
            assert_eq!(item.namespace, "ns");
            assert_eq!(item.key, "k");
            assert_eq!(item.value, json!({"v": 1}));
            assert_eq!(item.created_at, created);
            assert_eq!(item.updated_at, updated);
        }

        // --------------------------------------------------------- //
        // Live-Postgres outbox tests (`--ignored`, DATABASE_URL)
        // --------------------------------------------------------- //

        /// The database the live tests run against; panics with guidance
        /// when unset (these never run in the default suite).
        fn database_url() -> String {
            std::env::var("DATABASE_URL").expect(
                "DATABASE_URL must point at a scratch Postgres database \
                 (e.g. postgres://user:pass@localhost/rusty_test)",
            )
        }

        /// Unique fragment so repeated runs against a shared scratch
        /// database never collide.
        fn uniq() -> String {
            uuid::Uuid::new_v4().simple().to_string()
        }

        /// A queued task record under `tenant` with a unique task id. The
        /// pool is unique per record: published tasks stay queued, and a
        /// unique pool keeps them from ever being claimed by another test
        /// sharing the scratch database.
        fn live_task(tenant: &str, idempotency_key: Option<String>) -> TaskRecord {
            TaskRecord::new(
                tasks::NewTask {
                    task_id: format!("task-{}", uniq()),
                    tenant: tenant.to_string(),
                    kind: "charge".to_string(),
                    payload: json!({"cents": 500}),
                    pool: format!("live-{}", uniq()),
                    recipient: None,
                    max_attempts: 3,
                    idempotency_key,
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

        /// Serializes the live tests: they share one scratch database, and
        /// a publish pass drains *any* pending row, so two concurrent tests
        /// would publish each other's rows and race the assertions.
        static LIVE_DB_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

        /// Publish until nothing is pending (bounded), clearing rows left
        /// over from previous runs against the shared scratch database so
        /// each test observes exactly its own publishes.
        async fn drain_outbox(store: &PostgresStore) {
            for _ in 0..100 {
                let published = store
                    .outbox_publish_pending(100, Utc::now())
                    .await
                    .expect("drain publish");
                if published.is_empty() {
                    return;
                }
            }
            panic!("outbox never drained — a poisoned row is stuck pending");
        }

        #[tokio::test]
        #[ignore = "requires a live Postgres (DATABASE_URL)"]
        async fn live_outbox_publish_is_idempotent_across_calls() {
            let _guard = LIVE_DB_LOCK.lock().await;
            let store = PostgresStore::new(database_url());
            drain_outbox(&store).await;
            let tenant = format!("t-{}", uniq());
            let task = live_task(&tenant, Some(format!("charge-{}", uniq())));

            // Enqueue only stages the row; nothing is claimable yet.
            let (staged, deduplicated) = store.outbox_enqueue(&task).await.unwrap();
            assert!(!deduplicated);
            assert_eq!(staged.task_id, task.task_id);
            assert!(store
                .get_task(&tenant, &task.task_id)
                .await
                .unwrap()
                .is_none());

            // The first publish inserts the task and marks the row; a
            // crash-recovery second pass (or the next relay tick) finds
            // nothing pending — at-least-once publishing, exactly-once
            // visibility.
            let published = store.outbox_publish_pending(100, Utc::now()).await.unwrap();
            assert_eq!(published.len(), 1);
            assert_eq!(published[0].task_id, task.task_id);
            assert!(store
                .get_task(&tenant, &task.task_id)
                .await
                .unwrap()
                .is_some());
            assert!(store
                .outbox_publish_pending(100, Utc::now())
                .await
                .unwrap()
                .is_empty());
        }

        #[tokio::test]
        #[ignore = "requires a live Postgres (DATABASE_URL)"]
        async fn live_outbox_publish_dedupes_against_an_existing_task() {
            let _guard = LIVE_DB_LOCK.lock().await;
            let store = PostgresStore::new(database_url());
            drain_outbox(&store).await;
            let tenant = format!("t-{}", uniq());
            let key = format!("charge-{}", uniq());

            // The task landed out-of-band (a pre-outbox enqueue, or a
            // publish whose mark-published was lost to a crash and whose
            // row is now retried): the same idempotency key is already
            // live in the queue.
            let direct = live_task(&tenant, Some(key.clone()));
            let (landed, deduplicated) = store.enqueue_task(&direct).await.unwrap();
            assert!(!deduplicated);

            // The outbox row carries a different task id under the same
            // key; publishing it must not double the effect — the queue
            // insert dedupes, and the row is still marked published.
            let staged = live_task(&tenant, Some(key));
            assert_ne!(staged.task_id, direct.task_id);
            store.outbox_enqueue(&staged).await.unwrap();
            let published = store.outbox_publish_pending(100, Utc::now()).await.unwrap();
            assert_eq!(published.len(), 1);
            assert_eq!(
                published[0].task_id, landed.task_id,
                "the publish resolves to the pre-existing task"
            );
            assert!(store
                .get_task(&tenant, &staged.task_id)
                .await
                .unwrap()
                .is_none());
            assert!(store
                .outbox_publish_pending(100, Utc::now())
                .await
                .unwrap()
                .is_empty());
        }

        #[tokio::test]
        #[ignore = "requires a live Postgres (DATABASE_URL)"]
        async fn live_checkpoint_and_enqueue_commits_or_aborts_as_one_unit() {
            let _guard = LIVE_DB_LOCK.lock().await;
            let store = PostgresStore::new(database_url());
            drain_outbox(&store).await;
            let tenant = format!("t-{}", uniq());
            let thread_id = format!("thread-{}", uniq());
            let checkpoint = Checkpoint::new(
                thread_id,
                0,
                rusty_agent_runtime::state::State::from_value(json!({"log": ["manual"]})).unwrap(),
                Vec::new(),
            );
            let first = live_task(&tenant, Some(format!("k-{}", uniq())));

            // Happy path: checkpoint + outbox row commit together; the
            // relay then publishes the task.
            store
                .checkpoint_and_enqueue(&checkpoint, std::slice::from_ref(&first))
                .await
                .unwrap();
            let published = store.outbox_publish_pending(100, Utc::now()).await.unwrap();
            assert_eq!(published.len(), 1);
            assert_eq!(published[0].task_id, first.task_id);

            // A retried pair whose checkpoint id already exists must abort
            // the WHOLE unit: the duplicate checkpoint id violates the
            // no-overwrite contract, and the transaction rolls back the
            // paired outbox insert with it — no silent half-write.
            let second = live_task(&tenant, Some(format!("k-{}", uniq())));
            let err = store
                .checkpoint_and_enqueue(&checkpoint, std::slice::from_ref(&second))
                .await;
            assert!(err.is_err(), "duplicate checkpoint id must abort");
            assert!(store
                .outbox_publish_pending(100, Utc::now())
                .await
                .unwrap()
                .is_empty());
            assert!(store
                .get_task(&tenant, &second.task_id)
                .await
                .unwrap()
                .is_none());
        }
    }
}

#[cfg(feature = "postgres")]
pub(crate) use postgres::{LazyPostgresCheckpointer, PostgresStore};
