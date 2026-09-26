# Durable Work design (R0.6)

Rusty's Durable Work release turns workers from remote-execution helpers into
a **durable activity system**: tasks that survive server crashes, worker
deaths, and deployments — and that retry safely because the runtime knows
what each task does to the world.

The promise, stated precisely: **effectively-once execution when
applications use idempotency — not universal exactly-once side effects.**
Delivery through the queue is at-least-once; the idempotency key, carried on
every envelope and passed to the effect itself, is what collapses duplicate
deliveries into one visible effect. Where an effect cannot be made
idempotent, Rusty does not pretend: the retry machinery refuses to re-drive
it silently (see the effect gate below).

This document is the design for the whole release. The shared contracts —
the retry taxonomy and the task envelope — land first, in
`rusty-core/src/durable.rs`, because the queue (server), the workers, and
the SDKs must all agree on them byte-for-byte. Golden-file tests under
`rusty-core/tests/golden/` pin the wire shapes; drift fails CI.

## Lineage, named

Durable Work stands on established patterns, and says so:

- **Saga / process-manager patterns** (Garcia-Molina & Salem 1987; the
  process-manager routing of enterprise integration) — long-running work
  decomposed into steps whose state lives outside any single process,
  recovered by re-driving from durable state rather than by keeping a
  process alive.
- **Temporal-style activity retries** — activities with declared retry
  policy (maximum attempts, backoff coefficients, non-retryable error
  types), server-side scheduling, and heartbeating workers. Our
  `ErrorClass` closed enum is the statically-typed version of Temporal's
  non-retryable error list.
- **SQS visibility timeouts** — a delivered message is invisible, not
  deleted; expiry returns it to the queue. Our leases are the same idea with
  an explicit owner and heartbeat.
- **Transactional outbox** (the microservices pattern) — state change and
  message emission committed in one transaction, published by a relay, so a
  crash can never produce "state saved, task lost" or the reverse.

## What Rusty does differently

Two things, both consequences of the Flight Recorder (R0.5) landing first:

1. **Effect classification drives retry safety.** Every journaled event —
   and now every `TaskEnvelope` — carries a declared [`Effect`] from the
   frozen taxonomy (`Pure` / `ReadOnly` / `Idempotent` / `Compensatable` /
   `NonIdempotent`). The retry policy does not guess from error strings or
   ask the application for a retryable-error list: it gates on the effect
   class. Work that is not `is_freely_repeatable()` is never silently
   retried, in any failure mode — including `Timeout`, where the work may
   already have happened. The `Idempotent` declaration *plus a stable
   idempotency key* is what unlocks automatic retry. This check lives in one
   function, `classify_retry`, shared verbatim by server and workers.
2. **Evidence is first-class.** Task lifecycle transitions — submitted,
   leased, attempt failed with class, retried, dead-lettered, completed,
   cancelled — are journaled as `RunEvent`s with causal parentage into the
   run that spawned them, and the envelope's `parent` field links a task
   tree into the run's causal chain. A dead-lettered task is not a log line;
   it is inspectable, replayable evidence with its full attempt history.
   Retry decisions are `DecisionFamily::Retry` decisions in the learning
   contract, so the R0.10 policy plane can later learn backoff policy from
   recorded outcomes — replay before learning.

## The contracts (`rusty-core/src/durable.rs`)

All types are `Serialize`/`Deserialize`, additive-evolution only: optional
fields carry serde defaults, `format_version` pins the envelope, and the
conservative default effect (`NonIdempotent`) means an undeclared task is
never silently retried.

### `ErrorClass` — why the attempt failed

Closed enum, declared by whoever ran the work (worker, transport, or lease
reaper), never inferred from logs:

| Class | Retry semantics |
|---|---|
| `transient` | Retry with backoff; expected to succeed later. |
| `rate_limited` | Retry with backoff; callee `Retry-After` floors the delay (scheduler-side). |
| `timeout` | Retry with backoff — but the attempt may have partially executed, so the effect gate decides first. |
| `invalid_input` | Never retried; the same bytes fail the same way. Fails the task immediately. |
| `dependency_failure` | Retry with backoff; distinct from `transient` so telemetry separates "their outage" from "our wiring". |
| `resource_exhausted` | Retry with backoff, ideally placed elsewhere (scheduler's concern). |
| `cancelled` | Never retried, never dead-lettered — control flow, not failure. Keeps the retry machinery out of the cancellation path. |
| `unknown` | Retry to the attempt limit, then dead-letter. Unclassified handler errors and lease-expiry reaping land here; unknowns are the DLQ's primary input. |

### `RetryDecision` + `classify_retry` — one policy, shared verbatim

A failed attempt maps to exactly one decision — `retry { after_ms }`,
`dead` (dead-letter), or `fail` — through four gates, in order:

1. **Effect gate** — not `Effect::is_freely_repeatable()` → `fail`. Never
   silently re-drive a non-idempotent or compensatable effect.
2. **Class gate** — `invalid_input` / `cancelled` → `fail`.
3. **Attempt gate** — attempts exhausted (`attempt >= max_attempts`) →
   `dead`.
4. Otherwise → `retry` with `backoff_delay_ms(attempt, uniform)`.

### Backoff policy

Exponential with **full jitter**: retry `n` (1-based) draws uniformly from
`[0, 1s × 2^(n−1)]`, **capped at 5 minutes** (`BASE_RETRY_DELAY_MS = 1_000`,
`MAX_RETRY_DELAY_MS = 300_000`). Full jitter — uniform over the whole
exponential range, not a fixed delay plus noise — is what decorrelates a
fleet of tasks that failed together when a shared dependency recovers (the
thundering-herd problem; the AWS Architecture Blog's "Exponential Backoff
And Jitter" analysis is the reference). The jitter sample is a parameter,
not an internal draw: schedulers source it from the run's seeded
`RngSource`, so a recorded run reproduces its retry schedule exactly under
replay. **Attempt limits** come from the envelope's `TaskBudget`
(`max_attempts`, per-attempt `timeout_ms`); a task without a budget takes
the queue's defaults.

### `TaskEnvelope` — the unit of work

One serde-versioned struct carrying: `task_id`; `parent` (causal link into
the run's event tree); `sender` / `recipient` (a worker pool name, or a
pinned worker identity); `input` as a Flight Recorder `PayloadRef` (inline
≤ 4 KiB, content-addressed above — the queue row stays cheap to scan and
artifact addressing is shared with the journal); `output_contract` (an
`ArtifactContract`: kind + optional size bound; full payload schema
validation is R0.7's typed-contract work); `deadline` (whole-task, across
attempts); `budget`; `idempotency_key`; and the declared `effect`.

The idempotency key is load-bearing, not decorative: the queue refuses a
duplicate submission with an existing key, and the recipient passes the key
to the effect it performs. `None` is honest only for `Pure` / `ReadOnly`
work.

## The lease / visibility-timeout model (wave 1)

The queue is a Postgres table in `rusty-agent-server` (same store family as
`server_journals`; advisory-locked auto-migrations as established). Rows
carry the envelope, status, attempt count, lease owner, and lease expiry.

- **Delivery is a lease, not a deletion.** A worker that pops a task takes
  a lease (default 30 s); the task is invisible to other workers until the
  lease expires. This is the SQS visibility-timeout idea with an explicit
  owner identity.
- **Heartbeats renew.** A healthy worker heartbeats to extend the lease
  while the attempt runs (per-attempt `timeout_ms` bounds how far). A
  worker that dies stops heartbeating; the lease expires and the task
  returns to visibility with its attempt counter incremented — safe
  reassignment with no double execution beyond the at-least-once the
  idempotency key already absorbs.
- **Lease-expiry reaping classifies as `unknown`.** A dead worker tells us
  nothing about whether the effect fired; the effect gate and attempt
  budget handle it like any other unclassified failure.
- **Crash recovery is the release proof:** kill the server and a worker
  mid-effect, restart, and the run completes without losing state or
  duplicating the external effect — the checkpointed run state resumes, the
  leased task returns to visibility, and the idempotency key makes the
  re-attempt a no-op at the effect.

  **As implemented (wave 3c), the proof is an automated integration test,
  `rusty-server/tests/crash_recovery.rs` — not a manual demo.** It spawns
  the real demo binaries as processes (`server_demo` on a JSON-file store
  in a temp dir, `activity_worker_demo` running `send_receipt` against a
  file-backed idempotent "provider" — a ledger file, outside the server's
  store, keyed by the task's idempotency key; both binaries take
  `RUSTY_DEMO_*` env hooks whose defaults leave the interactive demos
  unchanged). Attempt 1 fires the effect: the worker appends the
  invocation to the provider ledger, fsyncs it, and then pauses — the
  classic window, **effect durable at the provider, completion never
  reported**. Inside that window the test SIGKILLs the worker and then the
  server (no drain, no signal handling — the graceful path is covered in
  `shutdown.rs`). Both restart from the same store dir / ledger, and the
  test asserts the promise end to end: the leased task returns to
  visibility at lease expiry and a second attempt runs (the record ends
  `completed` with `attempt == 2`, its idempotency key and attempt history
  intact — no lost state); the provider ledger holds exactly **one**
  invocation across both worker processes, and the stored result carries
  the first attempt's provider confirmation with `deduplicated: true` —
  the re-attempt was a no-op **at the effect**, not just at the queue; and
  the effect receipt on the completed record matches that confirmation.
  Flake resistance is by construction: 1 s leases make expiry fast, every
  wait is a poll against a deadline (never a fixed sleep), and the
  post-effect pause (30 s) can never be outrun by the SIGKILL. The whole
  proof runs in seconds on the JSON-file backend — the restart-durability
  reference; the lease, retry, and receipt semantics under test are
  backend-shared, so Postgres parity is covered by the existing gated
  suites rather than re-killed here. (The proof also caught a real bug on
  first run: `Activity for Arc<dyn Activity>` delegated only `run`, so the
  worker's stored handlers dropped reported receipts — fixed by
  delegating `run_with_receipt`, with a regression test.)

## Dead-letter policy (wave 1)

A task dead-letters when a retryable failure class exhausts its attempt
budget (gate 3) or an `unknown` failure keeps recurring. Non-retryable
classes (`invalid_input`) and non-repeatable effects do **not** dead-letter
— they `fail` immediately, because re-driving the same input fixes nothing
and the DLQ is for actionable work, not a graveyard. DLQ entries keep the
full envelope plus the attempt history (classes, decisions, timings) as
evidence; operators inspect them, fix the cause, and re-drive by hand.
`cancelled` never enters the DLQ. Tenant quotas (wave 3) count DLQ depth
against the tenant — an unbounded DLQ is a quiet disk-full outage.

## Transactional outbox + effect receipts (wave 2)

**Both are implemented (wave 2b); wiring them into the run executor is the
documented later integration point.**

The split-brain the outbox kills: a node completes, writes state (or a
checkpoint), and submits a task — and crashes between the two. Wave 2b makes
state change and task submission one durable unit:

- **The outbox is a table, not a flag.** `POST /tasks/outbox` and
  `update_state`'s new `enqueue` list write outbox rows (1:1 with their
  task, `outbox_id == task_id`) and answer `202 Accepted`; the tasks become
  claimable only when the relay publishes them. On Postgres,
  `update_state`'s checkpoint write and outbox inserts are **one
  transaction** — a duplicate checkpoint id aborts the whole unit, so
  "state saved, task lost" and the reverse are both impossible. The
  JSON-file backend cannot transact across files, so it writes the outbox
  rows *first* and the checkpoint second: a crash may leave published
  tasks whose checkpoint never landed (visible and inspectable), but never
  a checkpoint whose tasks silently vanished. Cross-record atomicity is
  Postgres-only.
- **The relay is a poller** (default 250 ms, `ServerConfig::
  with_outbox_relay_interval`), publishing up to 100 pending rows per
  pass, oldest first. On Postgres each row publishes in its own
  transaction — pick under `FOR UPDATE SKIP LOCKED`, insert the task, mark
  the row published, commit — so a crash mid-publish leaves the row
  pending for the next pass (or the next process), and concurrent relays
  take distinct rows. Correctness never depends on the interval; pending
  rows survive restarts and publish on the first pass after boot.
- **Publish dedupes on the task's idempotency key** (the same partial
  unique index `POST /tasks` uses), so a retried publish — or two
  submissions of the same effect, outbox and direct — resolves to one
  task. Delivery stays at-least-once; visibility is exactly-once.

**Effect receipts** close the loop the other way: when a task performing an
`Idempotent` effect completes, the worker reports the receipt (the effect's
own confirmation — `provider`, `provider_id`, `idempotency_key`, optional
`task_id`) on `POST /tasks/{id}/complete`. The server rejects a receipt
whose key does not match the task's (`400` — evidence of a wiring bug),
stores it on the task record (an additive `receipt JSONB` column on
Postgres), and journals it into the task's run as an `effect_receipt`
`RunEvent` (`Effect::Idempotent`, the receipt as the output payload). The
causal parent is the journal's current head — the honest parent while task
lifecycle events are not yet journaled; once they are, the receipt's parent
becomes the task's completion event. Exact replay's
`JournalSnapshot::find_effect_receipt` then serves the receipt instead of
re-sending the effect — the same rule the Flight Recorder already applies
to journaled model and tool calls, extended across a crash boundary. Two
honest edges, by design: journaling is best-effort (the receipt is already
durable on the task record), and while the run is still live its next
checkpoint-boundary journal flush would rewrite the stored snapshot —
run-side task-lifecycle journaling is the durable fix and the integration
point for a later wave.

## Cancellation propagation + drain (wave 2)

**Both are implemented: cancellation propagation in wave 2a, drain in wave
2c.**

- **Propagation.** Cancellation is a tree: cancelling a run
  (`POST /runs/{run_id}/cancel`) cancels its outstanding tasks — every
  non-terminal task enqueued with that `run_id` (the linkage rule: a task
  is a run's outstanding work iff its `run_id` matches, tenant-scoped);
  cancelling a task (`POST /tasks/{task_id}/cancel`) moves a queued or
  retry-scheduled task to the terminal `cancelled` state immediately, and
  signals a leased task's holder, which aborts the attempt and reports
  `cancelled` — never retried, never dead-lettered. The signal is
  `cancel_requested`: set on the record, carried on heartbeat responses
  (the lease itself is untouched, so the holder's fail report still
  passes the owner check). Deadline expiry is cancellation by clock: the
  claim path finalizes a task whose whole-task `deadline` has passed as
  cancelled instead of leasing it, and the worker treats an expired
  deadline mid-attempt as `cancelled`.
- **Cancellation is a hint, not the correctness mechanism.** A worker
  that misses the signal (partition, slow handler) is cleaned up by
  ordinary lease expiry — with one refinement: the claim path finalizes a
  cancel-requested task whose lease lapsed unanswered as cancelled rather
  than re-leasing it, so "never re-queued" holds whether or not the
  holder ever asks.
- **Drain.** A worker asked to drain (deployment, scale-down) stops taking
  new leases, finishes or fast-fails in-flight attempts within a grace
  period, and releases the rest — which return to visibility for other
  workers. The server drains its per-thread run queues the same way, so a
  rolling deploy never strands a leased task longer than one lease period.

  As implemented (wave 2c):

  - **Worker side.** `ActivityWorker` has no separate `drain()` method:
    cancelling the `CancellationToken` passed to `run` *is* the drain
    request — idempotent by construction, and race-safe because the claim
    poll itself is `select!`ed against the token (a drain starting
    mid-poll still wins). Draining stops claiming immediately; an
    in-flight attempt keeps heartbeating and settles normally, bounded by
    `with_drain_grace` (default `DEFAULT_DRAIN_GRACE` = **25 s**, under the
    30 s default lease and Kubernetes' 30 s default pod-termination
    grace). An attempt that outlives the grace is aborted and
    **deliberately left unsettled** — this is the documented choice for
    "releases the rest": reporting `ErrorClass::Cancelled` would be a
    fast-fail, but `cancelled` is terminal (never retried, never
    dead-lettered), so a fast-fail would kill the task outright, which is
    exactly what a deployment must not do. Left unsettled, the attempt
    returns to visibility at lease expiry and a worker that is still
    serving claims it. Signal handling stays with the embedding binary
    (`examples/activity_worker_demo.rs` wires SIGTERM/SIGINT to the
    token); the library exposes the token, per the usual Rust idiom.
  - **Server side.** `serve` drains on SIGINT/SIGTERM
    (`serve_with_shutdown` for embedders, `shutdown_signal` as the default
    signal source, `router_with_shutdown` for self-hosted routers), in a
    fixed order: (1) axum stops accepting connections and the shared drain
    token fires — new run submissions answer `503 shutting_down`, the cron
    scheduler stops; (2) in-flight requests complete, in-flight runs are
    cooperatively cancelled at their next super-step boundary via the new
    `RunConfig::cancellation` hook (the executor returns
    `RustyError::Cancelled`; the server ends them terminal-`cancelled`),
    and the outbox relay finishes its current pass and stops; (3) the
    whole drain is bounded by `ServerConfig::with_shutdown_grace` (default
    **25 s**, matching the worker's grace). **The checkpoint-resume safety
    net:** cancellation is only ever observed at a super-step boundary —
    a point where a checkpoint was *just* persisted — so a drained run
    resumes by re-running the thread from its last checkpoint; and past
    the grace bound the server stops anyway, which is precisely the crash
    case lease expiry and the checkpoint log already cover. Drain never
    decides correctness; it only makes the common case fast. The
    rolling-deploy property is tested directly: a task leased to a server
    that goes away mid-lease is claimable by the replacement instance
    within one lease period.

## Pools, quotas, version pinning, autoscaling signals (wave 3)

**Implemented (wave 3a).** The run-side wiring — an executor pinning a
run's tasks to the worker version that started it — is the documented
later integration point; the queue-side mechanics below are live today.

- **Named pools** with per-pool concurrency limits (`ServerConfig::
  with_pool_limit(pool, max)`; unconfigured pools stay uncapped, and a
  limit of `0` pauses a pool). The claim path counts the pool's
  *unexpired* leases and skips a saturated pool, so a GPU-bound pool and
  an IO-bound pool coexist without starving each other, and an expired
  lease holds no capacity. Honest edge, by design: on Postgres the count
  and the claim are one transaction, but two concurrent claim
  transactions can each see the pool one lease short of the cap and both
  succeed — the limit is a guardrail, not an invariant.
- **Tenant quotas** — tasks queued, tasks in flight, DLQ depth
  (`ServerConfig::with_task_quota` for the server-wide default,
  `with_tenant_quota` per tenant) — enforced at submission under the
  existing `{tenant}/` id-namespacing isolation: `POST /tasks`,
  `POST /tasks/outbox`, and `update_state`'s `enqueue` answer
  `429 quota_exceeded` (naming the gauge) before any write, preserving
  `update_state`'s all-or-nothing contract. "Queued" counts the whole
  accepted backlog — queued tasks, retry-scheduled failures, *and*
  pending outbox rows — so the outbox is not a quota bypass. Enforcement
  is at submission on purpose: a submission check is a backpressure
  signal, while work already accepted must still flow. A submission that
  would have deduplicated on its idempotency key can also answer `429`
  under pressure — safe (the pre-existing task is untouched) and simpler
  than reaching inside the store's dedup decision.
- **Version pinning** is an exact string match: a task carrying
  `worker_version` is claimable only by a worker advertising that exact
  string (`POST /tasks/claim`'s additive `worker_version`; the activity
  worker sets it with `ActivityWorker::with_worker_version`), and the pin
  survives retries until the task finishes, so a deploy mid-flight never
  changes semantics under an in-flight execution (the same fork-first
  conservatism as time travel). Semver ranges are deliberately future
  work — exact match is the only rule that cannot surprise.
- **Autoscaling signals** are metrics, not mechanisms: `GET
  /tasks/metrics` reports, per pool and tenant-scoped like every other
  server resource, the queue depth, the oldest visible task's age, the
  live-lease count, the configured limit, and lease saturation
  (`leased / limit`, `null` for uncapped pools) — configured-but-idle
  pools report zeros rather than vanishing. Rusty publishes the signals;
  the autoscaler is the operator's HPA/KEDA/etc. Published under-load
  numbers for these signals against the [benchmarks](benchmarks.md)
  baseline remain evidence work for a later wave — the endpoint ships;
  the numbers do not yet.

## Composition with the Flight Recorder

The two systems are one system seen from two sides:

- **Task lifecycle is journaled.** Submission, lease, failure-with-class,
  retry decision, dead-letter, completion, cancellation — each a `RunEvent`
  in the run's journal, causally parented, so a run that fans out into
  durable tasks is one connected evidence tree from super-step to queue to
  effect receipt.
- **`Effect::Idempotent` is the safety contract.** The Flight Recorder
  froze the taxonomy; Durable Work is the first policy that *consumes* it.
  Retry safety is a classification check, not a hope.
- **Retry decisions are learning evidence.** Each `classify_retry` outcome
  is recordable as a `DecisionFamily::Retry` `DecisionEvent` (features:
  error class, attempt, dependency latency; legal actions: retry/abort;
  propensity from the active policy version), which is what makes the
  R0.10 retry-policy learning wedge well-posed from day one.
- **Determinism carries through.** Backoff jitter draws from the run's
  seeded `RngSource`; task event timestamps read from the run's clock. A
  recorded run's retry schedule is reproducible.

## Explicitly not promised

- **No universal exactly-once.** Delivery is at-least-once; exactly-once
  *effects* require idempotency, and Rusty enforces the honesty of that by
  refusing to silently retry effects that don't declare it.
- **No mid-attempt checkpointing.** The activity boundary is the
  granularity, same as the super-step boundary in the executor; partial
  progress inside an attempt is lost on lease expiry and re-executed.
- **No automatic compensation in v1.** `Compensatable` effects fail closed
  today; pairing effects with their compensations (the saga half of the
  lineage) is later work, gated on real demand.
- **No built-in autoscaler.** Signals in wave 3; scaling decisions stay
  with the operator's infrastructure.
