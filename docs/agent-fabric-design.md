# Agent Fabric design (R0.7)

Rusty's Agent Fabric release turns single durable runs into **durable agent teams**: agents
with stable identity and private state that survive crashes, talking through mailboxes the
runtime persists and retries, supervised with declared restart and escalation policy,
coordinated through four typed patterns whose guarantees the runtime — not the application —
enforces. The same release scales the state underneath: copy-on-write state, delta
checkpoints, content-addressed artifacts.

Why this needs to live in the runtime, stated plainly. Framework-level multi-agent code — an
orchestrator holding references to workers, passing messages through in-memory channels —
loses three things on any crash: the messages in flight (the channel is process memory), the
causal record of who asked whom for what (each agent's logs are separate streams with no
shared parentage), and the supervision policy (restart logic written as application code dies
with the process). Rusty already durably executes one graph (R0.1–R0.5) and durably executes
queue-dispatched work (R0.6); an agent team is the composition of the two — each agent is a
checkpointed thread whose behavior loop is driven by mailbox messages arriving as durable
tasks. The release proof is the composition: kill the team mid-coordination, restart, and it
resumes with its causal history one connected tree.

The promise, stated precisely, mirrors Durable Work's: **turn-sequential processing and
at-least-once delivery per agent; total message order and exactly-once effects are not
promised.** No message is silently lost, no agent processes two messages concurrently, no
restart happens without a journaled decision, no coordination pattern settles without an
evidence record. Ordering, idempotency, and dead-letter semantics below are mapped onto what
the R0.6 queue already provides; what must be added is named as new. Contracts land first, in
a new `rusty-core/src/agents.rs` — as with `durable.rs` before the queue, server, agent
hosts, and SDKs must agree on them byte-for-byte, and golden-file tests pin the shapes.

## Lineage, named

Agent Fabric stands on established models, and says so:

- **Erlang/OTP supervision** (erlang.org `doc/system/sup_princ.html`) — declared restart
  policy per child (`permanent` / `transient` / `temporary`), restart *intensity* and
  *period* bounding how much failure a supervisor tolerates before escalating to its parent.
  Rusty adopts the vocabulary almost verbatim — it is the most operationally tested failure
  model in existence — with one structural change: escalation is a *message*, not a process
  exit, because Rusty agents are data and runs, not processes.
- **Orleans virtual actors** (learn.microsoft.com `/dotnet/orleans/overview`) — the
  stable-identity insight: a grain is addressed by identity, activated on first message,
  re-activated transparently after failure. Rusty takes identity-addressing, on-demand
  activation, and turn-based execution (one message at a time per activation); it does *not*
  take clustering or placement — see the not-built list.
- **Temporal entity workflows** — long-lived entities recovered from durable history rather
  than kept alive as processes. Rusty's recovery authority is the checkpoint log, not history
  replay (the journal is evidence *about* the run, not the state of it — the distinction
  `docs/flight-recorder-design.md` draws against event sourcing), but the entity shape is the
  same: mailbox-driven, durable, resumable.
- **Persistent data structures** (Okasaki, *Purely Functional Data Structures*, 1996/1998) —
  structural sharing: a new version shares everything it did not change. State scaling here
  is that idea applied to Rusty's `State`, at a granularity chosen by measurement.

## What Rusty does differently

Two things, both consequences of the substrate R0.5 and R0.6 already shipped:

1. **Agent identity is a checkpoint log, and restart is re-driving it.** OTP restarts a
   *process*: the state is gone by definition, and keeping it is the programmer's problem. A
   Rusty agent's private state is a thread's checkpoint history (`rusty-core/src/checkpoint.rs`,
   `Checkpoint`); "restart" means a new run on the same thread, restoring the latest
   checkpoint and continuing from its `next_nodes` — interrupt/resume's mechanism, pointed at
   failure recovery. Supervision therefore restarts agents *with their state*, and the
   restart decision itself is journaled evidence, not a supervisor log line.
2. **Coordination inherits the effect taxonomy.** Every mailbox message is a `TaskEnvelope`
   (`rusty-core/src/durable.rs`), which already carries the declared [`Effect`], the
   idempotency key, the deadline, and the causal `parent`. The race pattern's "idempotent
   candidates only" rule is not a convention the application must honor — it is the same
   effect gate `classify_retry` enforces, applied at submission.

And three Rust-specific enablers, stated concretely:

- **Ownership makes copy-on-write state cheap and safe.** Channel values behind `Arc`:
  cloning a snapshot is a refcount bump, and the borrow checker proves no node holds a
  mutable alias into a snapshot another node reads — the bug class that forces defensive deep
  clones in a GC'd incumbent is unrepresentable, so the sharing needs no read-path locks.
- **Type-state makes contracts enforceable.** The coordination patterns are builder states:
  a `Race` that has not declared its candidates' effect class does not compile, rather than
  failing a runtime check after paid model calls have run — `GraphBuilder::compile()`'s
  discipline, extended to teams.
- **Fearless concurrency makes supervision ordinary code.** A supervisor is one more `tokio`
  task reading durable records and writing journaled decisions — the same
  `JoinSet`/`CancellationToken` discipline the executor and the R0.6 drain path run on. No
  separate supervision runtime.

## Durable agents

### Identity and the capability manifest

An **agent** is a triple: a stable `AgentId` (tenant-namespaced like every other server id,
so `{tenant}/researcher-7` inherits the v0.5 isolation model unchanged — cross-tenant agents
resolve to nothing), a **thread** holding its private state (`thread_id == agent:{agent_id}` —
a convention, not a new store: the checkpointer, time travel, and fork-on-replay work on
agent state unmodified), and a **versioned capability manifest** (new, `CapabilityManifest`).
Identity survives redeploys and crashes because it is names and records, not processes — the
Orleans insight without Orleans' directory service. The manifest:

- `agent_kind` — the graph/assistant this agent runs (a graph plus config, as the server's
  assistant registry already models it).
- `manifest_version` — an exact version string: the agent-level form of R0.6's worker version
  pinning (`TaskEnvelope::worker_version`, exact match, surviving retries). A team started
  against manifest `researcher/1.4.0` pins its mailbox traffic and delegated tasks to that
  manifest, so a mid-team redeploy never changes semantics under an in-flight coordination.
  Semver ranges are deferred for R0.6's reason: exact match is the only rule that cannot
  surprise.
- `accepts` — the message kinds the mailbox accepts, each with an `ArtifactContract`. R0.6's
  contract carries kind + size bound and names full schema validation as R0.7's work; this
  release adds an optional JSON Schema (additive, serde default) and validates at submission —
  an unacceptable message fails fast as `ErrorClass::InvalidInput` (never retried) instead of
  dead-lettering after three attempts.
- `scopes` — the declared `StateScope`s the agent may read and write (below); journaled with
  the spawn event, checked at every access.
- `budget` — an agent-level ceiling (tokens/cost, deadline), enforced the way `TaskBudget`
  already bounds tasks.

### Typed durable mailboxes on the R0.6 queue

**Decision: a mailbox is an addressing discipline on the existing task queue —
`recipient = agent:{agent_id}` on a `TaskEnvelope` — not a new queue, and not a pool per
agent.** The rejected alternatives:

- *A pool per agent.* `TaskRecord.pool` is a deployment-level worker group: limits are static
  server config (`ServerConfig::with_pool_limit`), metrics aggregate per pool
  (`task_pool_stats`), and the Postgres claim path filters `pool = ANY($2)`. Thousands of
  agents as pools would turn static config into per-agent config and per-pool metrics into
  noise.
- *A separate mailbox table.* Duplicates leases, retries, DLQ, quotas, deadlines,
  cancellation, and the outbox — every mechanism R0.6 built and proved against real SIGKILLs
  (`rusty-server/tests/crash_recovery.rs`). One substrate, one set of invariants.

One additive change is required: the server's `TaskRecord`/`NewTask` does not carry
`recipient` today (the wire record flattens to `pool`), so R0.7 adds a `recipient`
column/field, and the enqueue surfaces accept `agent:{id}` recipients. Everything else the
mailbox needs, the queue already has:

| Mailbox semantic | What provides it |
|---|---|
| Durability | The queue itself: `server_tasks` rows / JSON-file records, atomic writes, reloaded at boot. |
| Acknowledgement | Task settlement: the turn completes (`complete_task`) or fails (`fail_task`); an unsettled task's lease expires and it returns to visibility. The "ack lost after processing" case is exactly the re-delivery the idempotency key absorbs. |
| Idempotency | The envelope's `idempotency_key` with the queue's submission dedupe — effectively-once *submission* for free; effectively-once *effect* remains the recipient's declaration, as in R0.6. |
| Dead-letter | The DLQ, with full attempt history; tenant quotas already count DLQ depth. An agent whose mailbox is dead-lettering is a supervision signal, not a silent graveyard. |
| Ordering | The claim path selects the **oldest claimable** task (`CLAIM_SELECT_SQL`: `ORDER BY created_at, task_id ... FOR UPDATE SKIP LOCKED`), so sends to one agent are processed oldest-first. |

The honest edge on ordering: oldest-first is not total FIFO. A retry-scheduled message
re-enters visibility *behind* later messages (its `next_attempt_at` gates it), and a
lease-expired message re-enters behind its successors. The runtime therefore promises
**turn-sequential processing** — one message at a time per agent, each settled before the
next is claimed — and *approximate* FIFO on the happy path, not total order. Applications
needing strict order carry a sender-side sequence number and gate on it; the envelope records
`sender` and `parent`, so out-of-order handling is detectable in evidence. This is Orleans'
promise shape, deliberately weaker than Erlang's per-process FIFO — because durability, not
process memory, is the mailbox.

**Turn serialization — the one genuinely new mechanism.** Sequential processing requires at
most one activation of an agent at a time, across all agent-host workers. R0.7 introduces the
**activation lease** (new): a per-agent record (`server_agent_leases` on Postgres; a file
record on the JSON backend) carrying `TaskLease`'s shape — owner and expiry, renewed by
heartbeat while held. A host claims the lease, then drains the mailbox one message at a time;
a host that dies stops heartbeating, the lease expires, and another host re-activates the
agent from its thread's latest checkpoint. "No two concurrent turns" rests on the lease the
same way "no two owners of a task" rests on `FOR UPDATE SKIP LOCKED`; on the JSON-file
backend, the documented one-writer-process precondition (`JsonFileCheckpointer`'s rule) is
what makes it exact — a known limitation, stated.

Each turn — one message, one settlement — is journaled: `MailboxSend` on the sender's side,
`MailboxReceive` on the recipient's, both new additive `RunEventKind` variants, linked by the
envelope's `parent` (below).

### State scopes

The roadmap's four scopes map onto stores that already exist; the new contract is the
`StateScope` enum naming them and the manifest field declaring access:

- **Private** — the agent's own thread. The checkpoint log *is* the private state: per-turn
  writes land in channels, the boundary checkpoint persists them, restart restores them.
  Delta checkpoints (below) make per-turn checkpointing cheap enough to default to.
- **Team** — a thread shared by the team's members (`thread_id == team:{team_id}`), written
  only through mailbox-driven turns, so every mutation has a journaled author. Shared mutable
  team state outside the turn discipline is not offered — that way lies the shared-state bug
  class the channel/reducer model was built to kill.
- **User** and **Tenant** — the server's KV store (`server_store.rs::kv_put/kv_get`,
  namespaced JSON documents under the `{tenant}/` isolation prefix): user scope is a
  `user:{user_id}` namespace; tenant scope is the tenant namespace itself.

Two rules keep scopes honest. Access is checked against the manifest's declared `scopes` — an
undeclared access fails fast before any I/O, the same shape as a write to an undeclared
channel failing at the barrier. And every cross-scope access is journaled with its effect
class: reads are `Effect::ReadOnly` (replay serves the journaled value), writes are
`Idempotent` under a key or `NonIdempotent` and gated accordingly.

### Supervision: restart and escalation

**What the runtime watches.** Three signals, all already durable records: task failures on
the agent's mailbox (classified into `ErrorClass` by the worker running the turn),
activation-lease liveness, and deadline breaches (the claim path already finalizes
deadline-expired tasks as cancelled). No new failure-detection machinery.

**What it does.** Policy is declared per agent in the manifest, in OTP's vocabulary because
that vocabulary is the reference implementation of operational restart semantics:

- `restart`: `permanent` / `transient` / `temporary` — where "restart" means: a new run on
  the agent's thread, restoring the latest checkpoint; the mailbox is untouched, so the
  crashed turn's message returns to visibility at its own lease expiry and is re-delivered
  under its idempotency key. State loss on restart is *bounded and explicit*: the turn in
  flight re-executes from its start (the idempotency contract every resumable node already
  honors), everything checkpointed survives. This is the structural difference from OTP: the
  state is the checkpoint log, so "let it crash" does not mean "lose the data".
- `intensity` / `period`: the maximum restarts tolerated in a sliding window before the
  supervisor stops restarting and **escalates**.

**Escalation is a message, not an exit.** In OTP, an exhausted supervisor terminates itself
and its children, and the parent reacts to the exit signal. Rusty agents are not processes
and there is no exit signal to trap; the equivalent is that the exhausted supervisor submits
an `Escalated` message to the *parent agent's mailbox* — durable, journaled,
retry-policy-bearing like any other — naming the failed agent, the attempt history, and the
classification. The supervisor hierarchy is thus itself made of durable agents all the way
up, and the root's escalation lands in the DLQ for an operator with the full evidence chain
attached. Messages over exits also makes escalation observable after the fact, which is what
the journal is for: every supervision action is recorded — the restart decision as a
`SupervisionEvent` (new additive `RunEventKind`: policy, triggering failure class, restart
ordinal), the escalation with its attempt history — and recordable as `DecisionEvent`s for
the R0.10 policy plane. Policy is static in R0.7, but the evidence is learnable from day one,
per replay-before-learning.

### Deadlines and the cancellation tree

An agent-level deadline is a whole-activity bound across turns, the way
`TaskEnvelope::deadline` is a whole-task bound across attempts; expiry is **cancellation by
clock**, R0.6's phrase, applied one level up. Cancellation composes into a tree, and the
order matters:

1. Cancelling an **agent** (new: `POST /agents/{id}/cancel`) first cancels its *outstanding
   mailbox traffic and delegated tasks* — the existing `cancel_run_tasks` linkage rule
   (`server_store.rs`), extended to agent-id scoping — then cooperatively cancels the agent's
   live run via the existing `RunConfig::cancellation` token, observed at a super-step
   boundary after the boundary checkpoint has landed. Children before parent: the agent's own
   terminal `cancelled` is recorded only after its outstanding work has settled, so a
   cancelled team never leaves an orphan task that would re-activate a cancelled member.
2. Cancelling a **team** cancels every member by the rule above, in any order — each
   member's cancellation is self-contained.
3. A single **message** can still be cancelled directly (`POST /tasks/{task_id}/cancel`,
   unchanged).

`ErrorClass::Cancelled` keeps its R0.6 semantics throughout — control flow, never retried,
never dead-lettered — so a cancelled team's cleanup does not pollute the DLQ, and a turn
cancelled mid-effect leaves the effect's idempotency key as the record of what may have
fired. Cancellation remains a hint for promptness, not the correctness mechanism: a host
that never observes it is cleaned up by lease expiry, exactly as in R0.6.

## Coordination patterns with runtime guarantees

Four patterns ship as typed contracts in `rusty-core/src/agents.rs`, each a thin composition
over mailbox submission through the transactional outbox. Two shared rules first.

**One connected tree across agents.** Event ids are `{run_id}:{seq}` (`rusty-core/src/record.rs`)
— globally unique across runs by construction — and `TaskEnvelope::parent` already links a
task to the run event or task that created it. So the cross-agent causal tree needs no new id
scheme and no physical super-journal: when agent A delegates to agent B, B's spawn and first
`MailboxReceive` record their `parent` as the event id in A's journal that sent the message;
A's journal records the `MailboxSend`. The team's evidence is a *forest stitched by parent
ids*, assembled at read time (a new read-side assembly, `TeamTrace`; journals stay per-run
and unchanged). The release proof's rule: from any event in any member's journal, walking
`parent` reaches the team's root spawn event. The new `RunEventKind` variants — `AgentSpawn`,
`AgentExit`, `MailboxSend`, `MailboxReceive`, `SupervisionEvent`, `CoordinationStart`,
`CoordinationEnd` — are additive to the closed enum, the same evolution rule R0.6's
`EffectReceipt` variant followed; old journals keep deserializing.

**Patterns submit through the outbox.** A pattern submitting N tasks must not crash between
checkpoint and submission — exactly the split-brain the outbox (`rusty-server/src/outbox.rs`)
kills. Submissions go through `checkpoint_and_enqueue`, so a pattern's task set and the run
state that spawned it are one durable unit on Postgres (the file backend keeps its documented
outbox-first ordering).

### Delegate / handoff

**Contract.** A typed ask: target agent (identity + pinned manifest version), input payload
(`PayloadRef` — inline ≤ 4 KiB, content-addressed above), an `ArtifactContract` for the
result, and a **scoped context transfer** declaring which scopes and channels the delegate
may see (the manifest's `scopes` intersected with the grant — the grant can only narrow,
mirroring the R0.9 rule that capability overlays may only narrow). *Handoff* is delegation
plus a terminal mark: the delegator ends its turn-set and the delegate becomes the causal
continuation, journaled as such.

**Runtime guarantee.** Submission through the outbox with the caller's event id as `parent`
and a deadline; the result (or failure) is delivered back to the delegator's mailbox as a
typed message correlated by the delegation's task id. The application never correlates
callbacks by hand.

**Journaled.** `CoordinationStart` in the delegator, the `MailboxSend`/`MailboxReceive` pair,
the delegate's turn set under that parentage, `CoordinationEnd` carrying the result
`PayloadRef` back.

**Member crash mid-pattern.** The delegate's turn task returns to visibility at lease expiry
and is re-delivered to a re-activated delegate (idempotency key intact); the delegation's
whole-task deadline bounds the wait — expiry cancels the delegation, and the delegator's
pending result arrives as a cancellation, not a hang.

### Fan-out / map

**Contract.** N delegations from one parent over a list of items, with a declared parallelism
bound — the cross-agent form of what `Route::Send` already is inside a graph
(`rusty-core/src/graph.rs`).

**Runtime guarantee.** Bounded parallelism enforced by the runtime (at most `k` delegations
in flight, the next submitted as each settles), and a **deterministic merge**: results keyed
by delegation task id, merged in sorted-id order — the canonicalization
`StateSpec::apply_super_step` applies to barrier writes (sort by name, because completion
order is scheduling-dependent), applied to mailbox results. Equal inputs and equal member
behavior produce byte-equal merged outputs.

**Journaled.** Each child is a causal child of the `CoordinationStart` via its delegation;
the merge is one journaled event carrying every child's result reference — the fan-in is
auditable child by child.

**Member crash mid-pattern.** One child's crash is one delegation's lease-expiry/re-delivery
cycle; the join waits. If the child dead-letters, the declared `on_member_failure` policy —
`fail_fast` (cancel the rest through the cancellation tree, surface the dead-letter) or
`partial` (merge what completed, the missing member journaled as missing — never silently) —
decides. There is no third, implicit option.

### Race

**Contract.** N candidate delegations over equivalent agents; the first *successful*
completion wins. The contract refuses, at submission, any candidate whose declared effect is
not `Effect::is_freely_repeatable()`: cancelling losers is only sound if losing is undoable.
This is the effect gate as a submission rule — the runtime's guarantee is that an unsound
race cannot be declared.

**Runtime guarantee.** On the winning completion the runtime cancels the remaining candidates
(queued ones go terminal-`cancelled` immediately; leased ones get the `cancel_requested`
hint, finalized at lease lapse if unanswered — the R0.6 wave-2a mechanics, unmodified). The
losers' settlements are journaled with their spent cost — the `cost_usd`/`tokens` evidence
fields already on `RunEvent` — because wasted cost is a decision input, and honesty about
waste is the price of offering races at all.

**Member crash mid-pattern.** A crashed candidate is a loser that failed instead of being
cancelled: the race proceeds; if every candidate fails, the race dead-letters as a whole with
all attempt histories attached.

### Quorum

**Contract.** N delegations, a declared threshold `k` over an explicit, named membership (the
list is part of the contract, so "who voted" is never ambiguous), and a **deterministic
resolver** — majority-equal, first-k, or an application-supplied pure function — applied to
the `k` accepted outputs in sorted-task-id order. Deterministic is a hard requirement: the
resolver is `Effect::Pure` code the runtime re-executes during replay, so a quorum's recorded
decision reproduces exactly.

**Runtime guarantee.** The quorum settles at `k` acceptances; remaining members are cancelled
as in the race. If membership minus failures drops below `k`, the quorum fails open —
journaled as unreachable, surfaced to the caller, never silently downgraded to a smaller `k`.

**Journaled.** `CoordinationStart` with the full membership, one result event per member
(accepted, failed, cancelled), `CoordinationEnd` with the resolver's inputs and output — the
evidence record an auditor needs to check the vote.

**Member crash mid-pattern.** Fan-out's per-member handling; the only quorum-specific edge is
the reachability check above.

## State scaling

### What full-state cloning costs today — measured, not guessed

The published baseline (`docs/benchmarks.md`) bounds the problem precisely:

- **Per-super-step snapshots.** The executor clones the full `State` once per super-step and
  again per active node (`snapshot.clone()` per invocation, `rusty-core/src/executor.rs`).
  Measured: 1 MB in ~17.5 µs, 10 MB in ~249 µs — on one large JSON *string*, the memcpy-bound
  best case; deep states clone slower per byte. The benchmark's verdict: cloning becomes
  *visible* in the 1–10 MB range, multiplied by every snapshot per step, plus the serde
  round-trip (~0.5–4.6 ms) each durable checkpoint pays.
- **Reducer merges.** `Reducer::Append` and `Reducer::DeepMerge` clone the current channel
  value per write: Append degrades ~1.4 µs at 10 elements to ~1.18 ms at 10,000; DeepMerge
  ~35 µs at 100 keys to ~3.9 ms at 10,000 — the baseline's "clearest scaling hazard in the
  current design."
- **Checkpoint bytes.** The placement experiment prices full snapshots: a 1000-step run at
  1 MB state writes **1.05 GB** under uniform checkpointing; ~0.86 ms per kept boundary at
  1 MB.

Agent teams sharpen all three: many threads checkpointing concurrently, per turn, and
team-scope channels (a shared event log, a growing artifact index) are exactly the
unbounded-Append case. The targets fall out of the numbers: kill the per-node full clone,
kill the O(N) Append merge, stop writing full snapshots per boundary.

### Copy-on-write state: the choice

**Decision: channel-granularity CoW — `Arc<Value>` per channel inside `State` — ships as the
mechanism; persistent within-channel structures (the `im` crate) are evaluated and deferred
behind a measurement gate.**

*What ships.* `State`'s interior becomes a map from channel name to `Arc<Value>` (the
top-level map behind one `Arc`). Cloning a `State` — the per-step snapshot, the per-node
snapshot, the checkpoint's copy — becomes a refcount bump per channel: O(channels), not
O(bytes). A reducer writing a channel clones only that channel's value (`Arc::make_mut`
semantics); unchanged channels are shared between pre- and post-step states and between every
node's snapshot, which is also what delta checkpoints diff against — sharing and deltas are
the same structural observation. The public surface does not change: `State::get` still
returns `Option<&Value>`, serde still sees the same JSON, and every downstream contract —
checkpoints, the wire, the SDKs, golden files — stays byte-identical.

*Evaluated and deferred.* The `im` crate's persistent maps/vectors (RRB trees, path copying)
would make within-channel sharing structural: an Append push onto a 10,000-element vector
becomes O(log n) instead of an O(N) clone. Three reasons to defer: (1) the serialization
bridge is real — checkpoint and journal payloads are `serde_json::Value` by contract, so a
persistent structure needs a lossless, golden-pinned JSON bridge, and a bug there corrupts
durable state, not a process; (2) channel-granularity CoW already removes the dominant
per-step cost, and the Append hazard only binds on channels that actually grow to thousands
of elements — the gate is: adopt `im`-backed channels, opt-in per channel in `StateSpec`, if
wave-4 benchmarks show reducer merges above an agreed share of turn latency; (3) one
structural change per wave is the discipline that keeps the release provable. A custom
in-repo persistent vector was also rejected: writing a new persistent data structure to avoid
a dependency is how subtle bugs get into durable state.

*Honest scope.* CoW does not help the serde round-trip a durable checkpoint pays
(serialization walks the whole `Value` regardless of sharing) — that is what delta
checkpoints and artifact addressing are for.

### Delta checkpoints

**What a delta is, against the current `Checkpoint`.** Today `Checkpoint.state` is the full
channel state and `put` enforces no-overwrite by id (`rusty-core/src/checkpoint.rs`).
Additively, `Checkpoint` gains `base: Option<String>` (the checkpoint this one diffs against)
and a channel-level delta (channels written since the base, as full per-channel values —
channel-granularity, matching the CoW layer). A checkpoint with `base == None` is a full
snapshot, exactly as today; old checkpoints deserialize unchanged, and old *readers* see only
full snapshots because — the key compatibility rule — **materialization is
checkpointer-internal**: every read method (`get_latest`, `get_by_id`, `list`,
`fork_thread`) returns a fully materialized `Checkpoint` by folding the chain onto its base.
The trait signature does not change; only implementations that opt in write deltas. Both
durable backends (`JsonFileCheckpointer`, `PostgresCheckpointer`) and the server store opt
in; `InMemoryCheckpointer` does not (its `put` is a sub-2 µs move; deltas would buy nothing).

**Chain depth and compaction.** An unbounded delta chain makes every resume an O(chain) fold —
the event-sourcing failure mode this system explicitly avoids (checkpoints, not journals, are
the resume authority). Bounded two ways, measured at write time: chain length (a full
snapshot every *K* boundaries; default *K* = 32 — resume folds at most 31 channel-sets,
sub-millisecond at CoW-sharing sizes; configurable via `ServerConfig`) and byte ratio (a full
snapshot when the accumulated delta approaches the size of the state it would replace).
`fork_thread` materializes eagerly: a fork is a new timeline and the natural compaction
point, so time-travel reads never fold another timeline's chain. Deltas compose with the
R0.5 mandatory placement floor unchanged: a mandatory boundary writes a checkpoint either
way; delta encoding decides its *bytes* — the 1.05 GB uniform-1000-step case is the
motivating measurement, published as a number against the baseline, not asserted here.

### Content-addressed artifacts

`PayloadRef`/`ArtifactRef` already exist (`rusty-core/src/record.rs`): payloads over
`INLINE_PAYLOAD_MAX_BYTES` (4 KiB) are content-addressed by SHA-256 — *but their bytes live
inside the journal snapshot's artifact map*, and the server's `TaskRecord` stores payloads
verbatim with the explicit note "Large payloads are out of scope until the artifact store
lands (R0.7)" (`rusty-server/src/tasks.rs`). The addressing half of the contract exists and
is golden-pinned; the new half is the store.

**The artifact store (new).** A content-addressed blob store behind the server:
`{store}/artifacts/{sha256}` files on the JSON backend (the same atomic temp-write-then-rename
discipline every file record uses), a `server_artifacts` table (`sha256` primary key,
`bytes`, `created_at`) on Postgres. Writes dedupe by construction — the hash is the identity —
which makes artifact-addressed retry traffic cheap: re-sent large inputs reference the same
object. Three consumers adopt it additively:

1. **Mailbox/task payloads.** The enqueue path spills payloads above the inline threshold and
   keeps the `ArtifactRef` on the record; queue rows shrink to scan-cheap references.
2. **Checkpoint state channels.** The checkpoint write path spills oversized channel *values*
   to the store and records the references in a new additive `artifact_manifest` field on
   `Checkpoint` — channel values stay JSON; the manifest is the spill ledger. The 1
   MB-string-channel benchmark case then stops dominating checkpoint bytes: the blob is
   stored once per content, not once per boundary.
3. **Journal artifacts.** The journal's artifact map gains a persistence seam; snapshot
   export (`JournalSnapshot`) still embeds bytes, keeping replay fixtures self-contained —
   the fixture contract outranks the size optimization.

**Garbage collection is deliberately conservative:** reference-counted by reachability from
live threads, tasks, and journals, and a manual operator command in R0.7, not a background
reaper — an over-eager reaper deleting an artifact a crashed run still needs is a durability
bug of exactly the class this release exists to eliminate.

### What stays JSON

Stated plainly: the SDK boundary, the wire protocol, checkpoint payloads, journal payloads,
and channel values all remain `serde_json` JSON. CoW changes representation *inside* the
process; artifacts change *where bytes live*; deltas change *what is written per boundary*.
None changes what a client sees, what a golden file pins, or what an old checkpoint
deserializes into. Typed state paths exist for Rust embedders (`State::get_as`); typed
polyglot SDKs are a schema-generation project, not this release.

## What R0.7 deliberately does NOT build

- **No clustering or agent placement.** Single server plus worker pools, as R0.6 ships.
  Orleans-style multi-silo activation with a directory service is named so nobody mistakes
  its absence for oversight: the activation lease is the single-node-correct version, written
  so a directory could later arbitrate it.
- **No reentrancy or interleaved turns.** One message at a time per agent. Orleans'
  `[Reentrant]` exists because non-reentrant grains deadlock on request cycles; Rusty's
  answer is the classic one — delegate the slow work through the mailbox and end the turn.
- **No learned supervision.** Policy is static and declared; the journaled decisions are the
  R0.10 policy plane's training data, per replay-before-learning.
- **No shared mutable team state outside the turn discipline, and no CRDTs.** Team scope is a
  thread mutated through turns; concurrent-merge semantics belong to the channel reducers
  inside a graph, not across agents.
- **No exactly-once delivery and no total mailbox order.** At-least-once with turn-sequential
  processing; the idempotency key collapses duplicates, as in R0.6.
- **No automatic compensation.** Unchanged from R0.6: `Compensatable` fails closed.
- **No agent marketplace, no dynamic discovery.** Manifests are registered, versioned, and
  pinned; discovery waits for signed capsules (R0.9).

## Wave plan and release proof

**Wave 1 — contracts, identity, mailboxes.** `rusty-core/src/agents.rs` (`AgentId`,
`CapabilityManifest`, `StateScope`, the additive `ArtifactContract` schema field, the new
`RunEventKind` variants) with golden files; recipient addressing on the enqueue surfaces, the
agent registry, the activation lease, turn-serialized mailbox draining. Exit: an agent
survives a server kill mid-turn and re-processes its mailbox message exactly once at the
effect.

> **Wave 1 status: implemented.** The core contracts and goldens landed as written
> (`AgentId` / `agent:{id}` addressing, `CapabilityManifest`, `StateScope`, the inert
> `RunEventKind` variants, `ArtifactContract.schema` pinned to JSON Schema draft 2020-12 —
> field stored and golden-tested, payload validation deferred per open question 5). The
> server surface is the `/agents` registry, mailbox send with manifest kind-membership
> validation, the activation lease (claim / heartbeat / release, fencing ordinals), and
> turn-serialized `mailbox/next` draining on both backends; pool claims never hand out
> mailbox traffic (`recipient IS NULL`), and pool caps / worker-version pins deliberately do
> not apply to agent claims. The exit criterion is automated as
> `rusty-server/tests/agent_recovery.rs`: server and agent host SIGKILLed mid-turn,
> mid-effect — the replacement host steals the expired activation (fencing bumped),
> re-claims the turn, and the provider ledger holds exactly one effect invocation.

**Wave 2 — supervision, deadlines, cancellation tree.** Restart policy with
intensity/period, escalation-as-message, agent/team cancel endpoints composing
`cancel_run_tasks` with `RunConfig::cancellation`, all journaled. Exit: a crash-looping agent
escalates to its supervisor's mailbox with its attempt history intact; cancelling a team
leaves zero orphan tasks (asserted by queue inspection).

> **Wave 2 status: implemented.** The supervision contracts landed as written
> (`SupervisionPolicy { restart, intensity, period_ms, supervisor }` — an additive optional
> manifest field, golden-tested; OTP `permanent` / `transient` / `temporary`, where
> `transient` restarts after failure classes but never after cancellations, the clock, or the
> operator; `EscalationNotice` under the `escalated` message kind). The server decides at
> three trigger points, all on durable records: turn-failure settlement (non-`cancelled`
> classes), the mailbox claim past the agent-level deadline (latched, fires once), and
> `POST /agents/{id}/restart` — the operator's reset, which clears the escalation and
> deadline latches and works with or without a declared policy. Escalation is a message to
> the supervisor's mailbox (idempotency key `escalation:{agent}:{ordinal}`, submitted
> quota-free like the outbox relay — evidence is never dropped under pressure); when no
> supervisor exists or accepts the kind, the notice dead-letters with the full evidence
> chain — open question 2's leaned default (DLQ + operator, no runtime-level root policy).
> The cancellation tree: `POST /agents/{id}/cancel` and `POST /teams/{team_id}/cancel` —
> team addressing is the declared `team_id` registration label, the one degree of freedom
> the design left open — compose a recipient-scoped `cancel_run_tasks` twin with per-run
> `RunConfig::cancellation` tokens, children before parent, journaled as `AgentExit` only
> when the cancel actually touched something. Agent deadlines stamp the earlier of
> message/budget deadline onto mailbox traffic and breach into supervision on the claim
> path. Every decision lands in the agent's supervision journal
> (`agent-supervision:{tenant}:{agent}`, integrity re-verified on read) behind
> `GET /agents/{id}/supervision`; the registry wire projection strips supervision state, so
> that endpoint is the only evidence surface. Both exit criteria are automated in
> `rusty-server/tests/supervision.rs` — crash-loop → supervisor mailbox with the
> three-attempt history, the journaled restart / restart / escalate trail, and latch
> suppression proven by a fourth failure; team cancel → zero orphan tasks by queue
> inspection with a per-member `AgentExit` — and again over live Postgres in
> `rusty-server/tests/postgres_supervision.rs` (plus the supervision-state persistence
> roundtrip through a fresh store instance). The crash-loop test drives the fail path
> in-process rather than SIGKILLing hosts: supervision triggers on the durable failure
> record, not the crash — the record is what survives one. The honesty boundary: the server
> half is decision + journal + escalation message + latch state; the restart itself (a new
> run re-driven from the latest checkpoint) remains the agent host's integration point via
> the unmodified wave-1 machinery.

**Wave 3 — coordination patterns.** The four typed patterns on the wave-1 substrate,
outbox-submitted, with `TeamTrace` cross-journal assembly. Exit: per-pattern integration
tests covering member crash mid-pattern.

> **Wave 3 status: implemented.** The typed contracts landed in core as written
> (`CoordinationContract` tagged `pattern`, golden-tested — delegate with `ContextGrant`
> narrow-only grants, fan-out with the in-flight window and `partial` / `fail_fast`
> member-failure policy, race with the submission-time effect gate: every candidate must
> declare a freely-repeatable effect because a loser is cancelled at an arbitrary point,
> quorum with `majority_equal` / `first_k` resolvers — the `custom` resolver's wire shape is
> pinned but rejected at submission, the wave-3 boundary). The server runtime is one
> convergent driver (`coordination.rs`, the supervision precedent): the coordination journal
> (`coordination:{tenant}:{id}`) is the latch book — `CoordinationStart`, one `MailboxSend`
> per member, one `MailboxReceive` per settlement observation, `CoordinationEnd` — scanned
> back out of the integrity-verified journal on every drive, so nothing the pattern knows
> lives only in memory; member task ids (`{tenant}--{cid}--{member}`), idempotency keys, and
> the outcome message id are derived, not minted, so retried drives converge. Settlement
> hooks fire from the complete / terminal-fail / cancel routes after durability; claim-path
> finalizations have no route hook, so `GET /coordination/{id}` reconciles on read
> (documented impurity, convergent). The settled outcome is one fact in three views —
> `CoordinationEnd` output, record, and the `coordination_result` message to the
> delegator's mailbox (a reserved kind the delegator's manifest must declare, checked at
> submission or the pattern would strand) — carrying every member's disposition in contract
> order (missing members are journaled, never silent) and the waste accounting (race losers'
> and cancelled members' reported `tokens` / `cost_usd`, new additive settlement-evidence
> columns on `server_tasks`). A race whose candidates all fail dead-letters its outcome
> quota-free (the root-escalation precedent); a quorum whose threshold becomes unreachable
> fails open as `unreachable` and never silently downgrades k; the fan-out merge is
> byte-deterministic in member task-id order, never completion order. `TeamTrace`
> (`rusty-core/src/team_trace.rs`) assembles the cross-journal causal tree from verified
> snapshots behind `GET /coordination/{id}/trace`, deterministic by construction. Both
> backends: the file store keeps one JSON per coordination; Postgres adds the
> `server_coordinations` payload table and commits journal + outbox rows in one
> transaction (`journal_and_enqueue`). The exit criterion is automated in
> `rusty-server/tests/coordination.rs` — every pattern crashed mid-flight: delegate member
> crash mid-turn (lease lapse, re-claim at attempt 2, settle), fan-out partial and
> fail-fast member deaths, race all-candidates-failed → DLQ, quorum majority resolving
> with a crashed juror — plus the effect-gate 400s, window backpressure, deterministic
> merge, trace connectivity, restart durability, deduplication, and tenant isolation;
> parity over live Postgres in `rusty-server/tests/postgres_coordination.rs`.

**Wave 4 — state scaling and numbers.** Channel-granularity CoW in `state.rs`, delta
checkpoints in both durable checkpointers and the server store, the artifact store, and the
benchmark suite extended with team-realistic workloads. Exit: numbers published in
`docs/benchmarks.md` against the 2026-08-06 baseline — snapshot cost per super-step at
1 MB / 10 MB states (before: ~17.5 µs / ~249 µs per full clone, times every snapshot), Append
merge at 10,000 elements (before: ~1.18 ms), checkpoint bytes for the 1000-step / 1 MB run
(before: 1.05 GB uniform) — same Criterion harness and environment table, per
evidence-over-claims.

> **Wave 4 status: implemented.** CoW state shipped as designed (`State` is one
> `Arc<BTreeMap<String, Arc<Value>>>`; serde byte-identical, pinned by round-trip tests;
> `Reducer::apply_shared` mutates in place under unique ownership, and the barrier merge
> removes the channel from the map first so a uniquely-owned channel actually reaches
> refcount 1). Delta checkpoints shipped with *K* = 32 + 80 % byte ratio in both durable
> checkpointers and both server-store checkpoint paths; fork compacts eagerly (open
> question 3, resolved below); `InMemoryCheckpointer` opts out. The artifact store shipped
> as `FileArtifactStore` + `PostgresArtifactStore` (table named `rusty_artifacts`, not
> `server_artifacts` as drafted above — one store for the runtime, not server-scoped) with
> read-time integrity verification and the journal seam (`snapshot_externalized` /
> `from_snapshot_with_store`); snapshots embed by default, per the fixture contract. The
> exit numbers are published in `docs/benchmarks.md` (2026-08-08 section): super-step
> snapshot fan-out at 1 MB / 10 MB → ~26 ns flat; Append at 10 k → 7.57 µs unique /
> 496.67 µs shared; the 1000-step / 1 MB run → 33.0 MB vs 1.05 GB (31.8×), wall time flat.
> Deviations from the text above, all scope-forced: (1) the delta policy lives on the
> checkpointers (`with_delta_policy`), not `ServerConfig` — server config is out of this
> wave's file scope; (2) artifact adoption shipped as the journal seam plus both store
> backends only — mailbox-payload spill and the checkpoint `artifact_manifest` need
> `tasks.rs` / `Checkpoint` wire changes owned by other workstreams and stay deferred;
> (3) `State::as_map` was removed (it returned the interior map by reference, impossible
> under the new representation) — it had zero callers repo-wide; `State::iter` replaces
> it; (4) the `im` gate stays open on evidence — durable runs merge through the shared
> (copy-on-write) column, so the within-channel persistent-structure question is answered
> by the published shared-column numbers, not by assertion.

**Release proof (the whole release).** An automated integration test in the crash-recovery
family (`rusty-server/tests/`, real processes, real SIGKILLs — the precedent
`crash_recovery.rs` set): a three-agent team — supervisor, two workers — executes a fan-out
with a delegated follow-up; the server and one agent host are SIGKILLed after the fan-out has
partially settled (one child complete, one in flight); everything restarts from the same
store; the test asserts (1) the team completes without duplicating any idempotent effect
(provider-ledger style, as R0.6's proof), (2) the in-flight child's message was re-delivered
under its idempotency key, and (3) `TeamTrace` assembly from the persisted journals yields
**one connected causal tree**: every event in every member's journal reaches the team's root
spawn event by `parent` links, matching the golden expectation. Plus: the wave-4
state-scaling numbers are published in `docs/benchmarks.md`.

> **Release proof status: implemented.** Automated as
> `rusty-server/tests/team_recovery.rs` — the `crash_recovery.rs` /
> `agent_recovery.rs` harness grown to a team: `server_demo` plus three
> `activity_worker_demo` agent hosts (the supervisor provider-less — its
> `coordination_result` turns are not external effects). The supervisor's
> fan-out `fo-1` (`alpha` → worker-a, `beta` → worker-b, both
> `effect: idempotent`) runs with worker-a on the fast provider pause and
> worker-b on the 30 s pause; when `alpha`'s settlement is journaled, the
> delegated follow-up `d-1` (`follow` → worker-a) is submitted through the
> public API on the supervisor's behalf, carrying the causal parent its
> turn would have stamped — `alpha`'s `MailboxReceive` event, derived as
> `coordination:default:fo-1:3` (the demo host deliberately has no
> nested-submission behavior; the submission path is the one an agent's
> turn would call). `d-1` settles, and with `beta`'s effect fsynced at the
> provider and its host mid-pause, the test SIGKILLs worker-b's host, then
> the server. Everything restarts from the same store dir / ledger file.
> The three gate properties, asserted: (1) the provider ledger holds
> exactly ONE invocation per idempotency key across all host generations
> (three keys, three lines, all first-attempt fires); (2) `beta`'s message
> ends `completed` at `attempt == 2` under its derived key
> `coordination:fo-1:beta`, the result and receipt carrying the first
> attempt's provider confirmation (`deduplicated: true`) — re-delivery
> under the idempotency key; (3) both per-pattern traces
> (`GET /coordination/{id}/trace`) are connected, and the UNION of the two
> persisted journals — assembled client-side from the events the server
> exposes, with `TeamTrace`'s exact semantics — is one connected tree of
> exactly ten events rooted at `fo-1`'s `CoordinationStart`, matching the
> golden expectation event-for-event (ids, kinds, contiguous seqs, parent
> links), with `d-1`'s start stitched onto `fo-1:3`. Member *run* journals
> do not exist at this layer — the demo hosts settle turns without
> journaling runs (the wave-2 host integration boundary) — so the pattern
> journals are the team's journals here. Flake discipline: every wait is a
> poll against a deadline; the only coordination-specific rule is that
> `GET /coordination/{id}` is a reconcile DRIVE, so journal reads are
> sequenced after the outcome-message chain has proven every settlement
> hook committed (task-visibility gates are pure reads) — the file
> backend's documented one-writer boundary is never crossed.

## Composition with the Flight Recorder and Durable Work

The three systems are one system seen from three sides:

- **Agent Fabric is Durable Work with addressing and lifecycle.** The mailbox is the queue;
  the turn is the task attempt; the activation lease is the task lease's discipline pointed
  at identity; supervision consumes the `ErrorClass` taxonomy; the cancellation tree is
  cancellation propagation grown one level. Nothing replaces a queue mechanism — every
  guarantee names the R0.6 mechanism it stands on, and the honest edges (at-least-once
  delivery, hint-based cancellation, guardrail-not-invariant limits) are inherited unchanged.
- **Agent Fabric is the Flight Recorder with more than one journal.** Evidence stops being
  per-run and becomes per-team: the `parent` chain crosses journals through envelope
  parentage, the new event kinds record the team-level facts, and `TeamTrace` is the
  read-side proof that the team's evidence is one connected tree. Effect classification keeps
  driving policy — whether a race may be declared, whether a retry may happen, whether replay
  may serve a scope read — and every supervision and coordination decision is recordable as
  learning evidence for R0.10.
- **The determinism seams carry through.** Mailbox retry jitter draws from the run's seeded
  `RngSource`; turn timestamps read from the run's clock; a recorded team's retry and
  supervision schedule is reproducible. Exact replay of a *team* is out of R0.7's scope;
  replaying one member against the recorded receipts of its peers (hybrid replay's
  pin-some-re-run-others rule across journals) is the R0.8+ follow-on this release's
  cross-journal parentage makes well-posed.

The shortest honest summary: Durable Work made one effect survive a crash; Agent Fabric makes
a *society* of effects survive one — with the evidence to prove which member did what, in
what causal order, and at what cost.

## Open questions

Flagged for the owner before wave 1 lands:

1. **Activation-lease enforcement on the JSON-file backend.** Postgres gives transactional
   claiming (`FOR UPDATE SKIP LOCKED`); the file backend's correctness rests on the
   documented one-writer-process precondition. Accept that (it is already the checkpoint
   rule), or add an in-process registry serializing activation claims? Leaning: document the
   precondition; ship the registry only if multi-process file-store deployments appear.
   **Resolved for wave 1 as leaned:** the precondition is documented on the store contract;
   in-process exactness comes from the one index lock, and Postgres enforces the same rule
   with the lease row's `FOR UPDATE`.
2. **Who supervises the root.** The design escalates the root supervisor's failures to the
   DLQ for an operator. Should there be a runtime-level root policy instead (bounded root
   restart with operator notification) — server config or a reserved system agent? Leaning:
   DLQ + metrics endpoint, consistent with "signals, not mechanisms".
   **Resolved for wave 2 as leaned:** an escalation no supervisor can accept (root agent,
   unknown supervisor, undeclared kind) dead-letters with the full evidence chain attached;
   `GET /tasks?status=dead` is the operator surface. No runtime-level root policy.
3. **Eager compaction on `fork_thread`.** Materializing a delta chain during a fork is a
   latency spike on a user-facing time-travel call. Acceptable, or compact asynchronously and
   serve reads from the chain until done? Leaning: eager — forks are rare and the fold is
   bounded by *K* = 32 by construction.
   **Resolved for wave 4 as leaned:** forks write full snapshots only (`put_internal(..,
   force_full)` on both durable backends), so a forked timeline never references the source
   timeline's chain. The measured fold at the bounded worst (31 deltas + base at 1 MB) is
   692 µs — a rare, sub-millisecond spike; asynchronous compaction stays unbuilt.
4. **Per-agent quotas.** Tenant quotas (tasks queued / in flight / DLQ depth) exist; a
   runaway agent can still monopolize its tenant's budget. Per-agent queue-depth quotas in
   R0.7, or is supervision-with-intensity enough backpressure? Leaning: defer unless the
   wave-4 benchmarks show the failure mode.
5. **`ArtifactContract` schema dialect.** JSON Schema draft 2020-12 is the default assumption
   for the new validation field; if the SDKs need a lighter subset for codegen, that
   constraint should arrive before the golden files pin the field's shape. **Wave 1 shipped
   the default:** the field's shape is pinned in the goldens as draft 2020-12; payload
   validation against it is deliberately not wired yet (no validator dependency), so
   tightening the dialect later only narrows what submissions declare, never what the wire
   means.
