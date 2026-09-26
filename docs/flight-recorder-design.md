# Flight Recorder design (R0.5)

Rusty's Flight Recorder is the **evidence system for a learning runtime**. It
records what a run *did to the world* — not observability spans for humans to
eyeball, but a causally linked, tamper-evident journal of effects that later
waves replay against, evaluate policies on, and roll back from.

This document covers the first two work items of R0.5: the contract freeze,
the determinism seams, and the effect journal (work item 1), and the replay
engine — exact replay, branch diff, and portable fixtures (work item 2).
Everything is in `rusty-core` (`rusty-agent-runtime`).

## Why: replay before learning, evidence over claims

The roadmap's sequencing rule is **replay before learning**: no learning
mechanism may ship before the run evidence it learns from can be faithfully
recorded, evaluated, and rolled back. The Flight Recorder exists to make that
rule implementable.

Three commitments fall out of it:

1. **Contracts freeze first.** Replay engines, server endpoints, Studio views,
   and the R0.8+ learning loop all consume the same shapes. If those shapes
   drift, every downstream wave drifts with them. So the schemas are frozen
   now — with golden-file tests that make accidental drift a CI failure —
   even though several of them (decisions, propensities) have no producer
   yet.
2. **Determinism is a seam, not a property.** You cannot bolt exact replay
   onto an executor that reads wall time and OS entropy from deep inside its
   loop. Time and randomness are injectable providers *now*, while the
   surface is small.
3. **Effects are classified at declaration time.** "Was that call safe to
   retry?" is not answerable after the fact from a log line. The producer
   (node, model, tool) declares its effect class; the journal records it;
   policy consumes it.

## The contracts (`rusty-core/src/record.rs`)

All types are `Serialize`/`Deserialize`, and their JSON shapes are pinned by
golden files under `rusty-core/tests/golden/`.

### `Effect` — the taxonomy

Every journaled event declares the effect class of whatever produced it:

| Class | Re-execution | Retry / replay guarantee |
|---|---|---|
| `Pure` | safe and equivalent | unconstrained; output may be re-derived or reused |
| `ReadOnly` | safe, not equivalent | exact replay serves the journaled output; live replay re-reads |
| `Idempotent` | safe under a stable key | retry with the same idempotency key; replay may serve the receipt |
| `Compensatable` | duplicates the effect | retry only with care; rollback pairs effect with compensation |
| `NonIdempotent` | duplicates, no compensation | never silently retried or replayed; re-execution is explicit |

The class is the input to three later policies: retry (R0.6), replay (R0.5
later waves), and capsule capability grants (R0.9). Defaults are honest and
conservative: plain nodes are `Pure`, models and tools are `NonIdempotent`,
remote and WASM nodes are `NonIdempotent`. Each trait carries a documented
override point (`Node::effect`, `Tool::effect`, `ChatModel::effect`).

### `RunEvent` — one recorded fact

The atomic evidence unit: `id` (`{run_id}:{seq}`, deterministic), `run_id`,
`thread_id`, `node_id`, monotonic `seq`, `kind` (a closed enum: super-step
start/end, node input/output, model call, tool call, remote call, WASM call,
interrupt, resume, routing decision, checkpoint written), `effect`, input and
output as `PayloadRef` (inline ≤ 4 KiB, `ArtifactRef` with SHA-256 content
hash above), `latency_ms`, `tokens`, `cost_usd`, `status`, `parent` (causal
parent event id), and `recorded_at` read from the run's clock.

Two properties make it evidence rather than logs: `seq` is the total order
(wall time is just an attribute), and `parent` forms the causal chain — a
node input's parent is its super-step start; a model call's parent is the
invocation that made it (delivered to node code via the reserved
`NodeConfig::extra` key `rusty.parent_event`); a checkpoint write's parent is
the routing decision that ended the step.

### `DecisionEvent` — the learning contract, frozen early

Executor learning arrives in R0.8+, but the contract freezes now so R0.5
journals are already learnable evidence. A `DecisionEvent` records: `family`
(closed enum: retry, timeout, worker placement, concurrency, checkpoint
placement), `features` (free-form JSON — the feature schema evolves with the
policy; the envelope does not), `legal_actions` (full closed `DecisionAction`
set, not just the winner), `selected`, `propensity`, `policy_version`, and
`outcome` (`None` until completion).

**Propensity is first-class and assigned at decision time.** Off-policy
evaluation — comparing a candidate policy against the recorded one — divides
by the propensity of the action actually taken. A propensity reconstructed
after the fact is fiction; that is why the field is on the contract, not
derivable. Policy versions are epoch-bounded, immutable `PolicyVersion`
newtype strings (`static-v0` is the pre-learning floor every current run
records).

### `CheckpointHeader` + `JournalRef` — provenance in the envelope

Every checkpoint now carries a header: `format_version` (currently 1),
`graph_version` (application-declared), `graph_hash` (SHA-256 of the compiled
topology via `Graph::topology_hash` — node bodies are opaque, so semantic
changes are the application's `graph_version` responsibility), the active
`policy_version`, and the `logical_clock` value at creation. Checkpoints also
carry `journal_ref`: the journal's event count and chained head hash at the
boundary, binding state and evidence together — a checkpoint pins not just
*how much* evidence existed but *which* evidence.

Both fields use serde defaults: **pre-R0.5 checkpoints still deserialize**,
loading with the default header (current format version, unversioned graph,
static policy) and no journal reference. A back-compat test pins an old-shape
JSON checkpoint.

## The determinism model (`rusty-core/src/journal.rs`)

The executor sources every timestamp and every random id through two
providers, configured per run:

- **`Clock`** — `System` (default; `Utc::now`, byte-identical to pre-R0.5) or
  `Clock::logical(start_ms, tick_ms)`, which ticks deterministically on every
  read. Event timestamps, node latencies, checkpoint `created_at`, and header
  `logical_clock` all read through it.
- **`RngSource`** — `System` (default; `Uuid::new_v4`) or
  `RngSource::seeded(u64)` (a shared ChaCha8 stream). Checkpoint ids and
  executor-minted run ids draw from it.

A run with an attached journal and no explicit clock reads time from the
journal's clock — one time source per run, including effects recorded from
inside node code. With a logical clock and seeded RNG, two drives of the same
graph produce byte-identical journal snapshots (proven by test). Node-output
events are journaled in active-set order, not `JoinSet` finish order, so the
sequence is stable even where scheduling is not; within one super-step,
parallel tasks' clock *ticks* still interleave by schedule, so per-node
logical latencies in multi-node steps are scheduling-dependent — the total
order of evidence (`seq`) is not.

## The journal

`Journal` is append-only, in-memory, cheap to clone (clones share), and
thread-safe. The executor writes super-step boundaries, node inputs/outputs
(with declared effects and clock-measured latencies), interrupts, resumes,
routing decisions, and checkpoint writes; nodes record their own model, tool,
remote, and WASM calls through the same journal via the `EventDraft` builder.
The journal assigns `seq` and ids, stamps `recorded_at` from its clock,
promotes oversized payloads into a content-addressed artifact map, and chains
a SHA-256 head hash over every event. `Executor::journal()` exposes the most
recent run's journal (set at run start — evidence of a failed run is still
evidence); `RunConfig::with_journal` attaches a pre-built one (how node
closures get a handle to record into).

`JournalSnapshot` is the serde-complete export (events + artifacts + head
hash) — the unit portable replay fixtures are built from.
`Journal::from_snapshot` re-verifies the head hash on load, so edited or
corrupt fixtures fail at the boundary, not deep inside a replay.

## Replay modes (`rusty-core/src/replay.rs`)

The second work item of R0.5 lands the replay engine. Three modes are
envisioned; only **exact** is implemented.

### Exact replay — implemented

Exact replay re-drives a recorded run from its `JournalSnapshot`: the same
graph topology runs with the same determinism seams, and every outbound
effect — model, tool, remote, or WASM call — is **served from the journal
instead of executed**. The seam is a pair of wrapper types per effect kind,
so the *same graph code* runs in both modes and only the effect
implementations are swapped:

- **Record**: `RecordingChatModel` / `RecordingTool` wrap the real
  implementations, journal each call in a canonical request/response shape
  (`model_call_request` / `model_call_response` / `tool_call_request`), and
  return the real response. Construct per node invocation with the causal
  parent from `rusty.parent_event`.
- **Replay**: `ReplayingChatModel` / `ReplayingTool` wrap the same
  implementations but **never invoke them** — there is no code path from
  `chat`/`call` to the wrapped value, so replay against panic-on-call
  sentinels (or credential-less clients) is safe; tests prove the sentinels
  never fire. Each call is matched against the journal **by sequence and
  request hash** through a shared `ReplaySource` cursor, answered with the
  recorded response, and re-journaled into the replay run's journal so the
  replayed evidence reproduces the recorded evidence byte-for-byte.

Mismatch fails loudly with `RustyError::Replay`: divergence (request hash
disagrees with the journaled request at the cursor), order violation
(effects arrive out of journaled sequence), exhaustion (more work issued
than recorded), or shortfall (verification finds recorded effects unserved).
Interrupts need no serving — with every effect answered deterministically,
node logic re-derives the same interrupt and the executor journals it
identically (proven by test: an interrupted run replays to the same
suspension, checkpoint id included).

`ExactReplay` drives the flow: construction re-verifies the snapshot
(chained head hash, artifact integrity, dangling references — tampered
journals are rejected at the boundary), `fresh_journal` mints a journal with
the recorded run's identity, `run` re-drives the executor, and
`run_and_verify` additionally requires the replayed journal to equal the
recorded one event-for-event. Byte-identity requires the recorded run's
determinism seams (logical clock parameters, RNG seed); the recording and
replaying wrappers perform identical journal-clock read sequences to keep
the tick stream aligned.

### Branch diff — implemented

`BranchDiff::between(base, branch)` diffs two journal snapshots — typically
two continuations of one forked history. Events compare **logically** (kind,
node, seq, effect class, resolved payloads, latency, tokens, cost, status;
identity/timing fields excluded, since branches are separate runs). The diff
reports the first divergent `seq`, added/removed events from the divergence
onward, per-super-step state-channel value diffs (from super-step-end
records), and per-branch token/cost totals.

### Portable fixtures — implemented

`ReplayFixture` bundles a recorded run for CI: `format_version`, the graph's
topology hash, the journal snapshot, the final checkpoint, and metadata
(name, logical-clock parameters, RNG seed). `export`/`import` are the JSON
wire boundary (import re-verifies format version and journal integrity);
`replay_in_ci` replays the bundle end to end — topology check, byte-identical
journal verification, final-state comparison. A checked-in example lives at
`rusty-core/tests/fixtures/exact_replay_agent_tools.json`; regenerate after
intentional contract changes with `UPDATE_FIXTURE=1 cargo test -p
rusty-agent-runtime --test replay`.

### Live and hybrid replay — defined, deferred

- **Live replay** would re-execute effects whose class permits it
  (`ReadOnly` re-read, `Idempotent` re-sent under the recorded key) while
  serving the rest, answering "what does this run do against *today's*
  world?" It needs per-class re-execution policy and a staleness report;
  neither is designed in v1.
- **Hybrid replay** would serve effects up to a fork point and execute live
  afterward — the counterfactual-probe mode ("replay to step 7, then let the
  new policy run"). It needs the live mode plus journal splicing, and is the
  natural consumer of `BranchDiff`. Both modes build on the exact-mode
  cursor and wrappers; only their serving policy differs.

## Deliberately NOT in v1

- **Live / hybrid replay execution** — defined above, deferred; exact replay
  is the fidelity floor they build on.
- **Exact replay of resumed runs** — a journal that begins with a resume
  event starts mid-run against checkpointed state the journal does not
  carry; `ExactReplay::new` rejects it. Replay the original run's journal
  instead.
- **Server endpoints and Studio UI** — later waves; they will serialize these
  same contracts, which is why the shapes are golden-pinned now.
- **Decision production** — `DecisionEvent` is frozen but nothing emits it
  yet; the policy plane (R0.8+) is the producer.
- **Persistent journal backends** — the in-memory journal plus snapshot
  export is v1; a Postgres journal is an R0.6+ decision.
- **Compensation registration** — `Compensatable` is classifiable now; the
  compensation mechanism arrives with durable work (R0.6).

## Originality note: what this is and is not

The Flight Recorder has honest intellectual cousins, and it is not any of
them:

- **Event sourcing** (journal as the source of truth, state as a fold) —
  Rusty's checkpoints, not its journal, remain the resume authority. The
  journal is evidence *about* the run, not the state of it. What Rusty takes
  from event sourcing is the append-only, hash-chained, causally ordered
  record.
- **Record/replay debuggers** (rr, Undo, time-travel tracing) — they record
  at the instruction/syscall boundary to re-execute deterministically. Rusty
  records at the *effect* boundary of an agent run, and the goal is not
  debugging fidelity but **policy**: the effect classification drives what
  retry, replay, and capsule rules apply to each call.
- **Workflow-engine histories** (Temporal event histories, Durable Functions)
  — they record to recover and to deduplicate on re-execution. Rusty's
  journal does that too, but its distinguishing payload is learning: every
  decision carries its **propensity and policy version as first-class
  fields**, because the journal's terminal consumer is an executor that
  learns mechanical policies (retry, timeout, placement) with correct
  off-policy evaluation — not a dashboard.

The shortest honest summary: observability systems record so humans can look;
event sourcing records so state can be rebuilt; workflow histories record so
execution can resume. The Flight Recorder records so a runtime can *learn —
and prove what it learned from*. Effect classification is the bridge: it
turns "what happened" into "what may safely happen again".

## API decisions later waves must know

New types and knobs (all re-exported from the prelude):

- `record`: `Effect` (+ `is_freely_repeatable`), `RunEvent`, `RunEventKind`,
  `EventStatus`, `PayloadRef`, `ArtifactRef`, `JournalRef`, `DecisionEvent`,
  `DecisionFamily`, `DecisionAction`, `DecisionOutcome`, `PolicyVersion`
  (`PolicyVersion::STATIC_V0`), `CheckpointHeader`, `CURRENT_FORMAT_VERSION`,
  `INLINE_PAYLOAD_MAX_BYTES`, `sha256_hex`.
- `journal`: `Journal` (`new`, `record`, `events`, `snapshot`, `head_ref`,
  `resolve`, `from_snapshot`), `JournalSnapshot`, `EventDraft`, `Clock`
  (`Clock::logical`), `RngSource` (`RngSource::seeded`), `PARENT_EVENT_KEY`
  (`"rusty.parent_event"` — the reserved `NodeConfig::extra` key carrying the
  invocation's journal event id).
- Trait override points (all with defaults, source-compatible):
  `Node::effect()` → `Pure`; `Tool::effect()` and `ChatModel::effect()` →
  `NonIdempotent`; `RemoteNode` and `WasmNode` override to `NonIdempotent`.
- `RunConfig` knobs: `with_clock`, `with_rng`, `with_journal`,
  `with_policy_version`, `with_graph_version`. `Executor::journal()` returns
  the last run's journal.
- `Checkpoint` gains `header: CheckpointHeader` and
  `journal_ref: Option<JournalRef>` (serde defaults — old checkpoints load);
  `Graph::topology_hash()` is the graph content hash stamped in headers.
- `replay`: `ExactReplay` (`new`, `snapshot`, `source`, `fresh_journal`,
  `run`, `verify`, `run_and_verify`), `ReplayParams` (`new`,
  `with_checkpointer`, `with_max_steps`), `ReplayOutcome`, `ReplaySource`
  (`new`, `serve`, `is_exhausted`, `remaining`), `ServedEffect`
  (`rejournal`), `RecordingChatModel` / `RecordingTool` (`new`, `node`),
  `ReplayingChatModel` / `ReplayingTool` (`new`), the canonical wire shapes
  `model_call_request` / `model_call_response` / `tool_call_request`,
  `BranchDiff` (`between`, `is_identical`) with `BranchTotals` / `StepDiff` /
  `ChannelDiff`, and `ReplayFixture` (`capture`, `export`, `import`,
  `exact_replay`, `replay_params`, `replay_in_ci`) with `FixtureMetadata` /
  `LogicalClockParams` / `FIXTURE_FORMAT_VERSION`. Replay failures are the
  new `RustyError::Replay` variant. The checked-in example fixture lives in
  `rusty-core/tests/fixtures/`; `UPDATE_FIXTURE=1 cargo test -p
  rusty-agent-runtime --test replay` regenerates it after intentional
  contract changes.
- Event ids are `{run_id}:{seq}`; decision ids are `{run_id}:d{n}` (separate
  sequence, stable under event-kind additions). Golden files live in
  `rusty-core/tests/golden/`; `UPDATE_GOLDEN=1 cargo test -p
  rusty-agent-runtime --test flight_recorder` blesses intentional contract
  changes.
