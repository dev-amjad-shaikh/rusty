# Benchmarks — Rusty Core (`rusty-agent-runtime`)

Initial Criterion benchmark suite for the core engine. These numbers exist so
that performance claims about the runtime are backed by published, reproducible
measurements rather than intuition.

> **Status: baseline.** This is the first published run (2026-08-06). It
> establishes the measurement harness and a single-machine baseline; it is not
> a regression history yet.

## How to reproduce

```bash
export PATH="$HOME/.cargo/bin:$PATH"

# Compile-only check:
cargo bench -p rusty-agent-runtime --no-run

# Full suite (takes ~7 minutes with Criterion defaults):
cargo bench -p rusty-agent-runtime

# Individual targets:
cargo bench -p rusty-agent-runtime --bench graph_compile
cargo bench -p rusty-agent-runtime --bench node_execution
cargo bench -p rusty-agent-runtime --bench parallel_fanout
cargo bench -p rusty-agent-runtime --bench reducers
cargo bench -p rusty-agent-runtime --bench checkpoint
cargo bench -p rusty-agent-runtime --bench interrupt_resume
cargo bench -p rusty-agent-runtime --bench state_clone
cargo bench -p rusty-agent-runtime --bench checkpoint_placement
cargo bench -p rusty-agent-runtime --bench headroom_experiment
```

Results (JSON estimates) are written to `target/criterion/<group>/<id>/new/estimates.json`.
The suite uses Criterion's default configuration: 3 s warm-up, 100 samples per
benchmark, 95 % confidence intervals. Async benchmarks drive the executor via a
single multi-threaded tokio runtime created outside the measurement loops.

## Environment

| | |
|---|---|
| CPU | Apple M2 Max (12 cores: 8 performance + 4 efficiency) |
| RAM | 96 GB |
| OS | macOS 26.5.1 (Build 25F80), arm64 |
| Rust | rustc 1.97.1 (8bab26f4f 2026-07-14) |
| Criterion | 0.5.1 (default features off: no plotters/rayon) |
| Date of run | 2026-08-06 |
| Crate version | `rusty-agent-runtime` 0.4.0 |
| Load | single-user machine, no other heavy processes |

Absolute numbers are only meaningful on comparable hardware; treat ratios
(scaling behavior) as the portable signal.

## Results

All values are Criterion mean estimates with 95 % confidence intervals in
brackets.

### Graph compilation — `GraphBuilder::compile()`

Linear chains (n nodes, n−1 static edges). Builder wiring excluded; only
`compile()` (validation + assembly) is measured.

| Nodes | Mean | 95 % CI |
|---|---|---|
| 10 | 1.01 µs | [1.01, 1.03] µs |
| 100 | 10.88 µs | [10.78, 11.00] µs |
| 1000 | 126.26 µs | [125.32, 127.50] µs |

### Sequential node execution — chain graph end-to-end

`Executor::run` with no checkpointer. Each node reads the previous channel,
increments, writes its own channel (real work, not a no-op graph).

| Chain length | Mean | 95 % CI | Approx. per super-step |
|---|---|---|---|
| 10 nodes | 106.63 µs | [104.84, 108.98] µs | ~10.7 µs |
| 50 nodes | 593.65 µs | [586.61, 602.57] µs | ~11.9 µs |
| 100 nodes | 1.344 ms | [1.334, 1.357] ms | ~13.4 µs |

### Parallel fan-out / fan-in

Static fan-out: `source → N branch nodes (same super-step, parallel tasks) → sink`,
branch writes merged via `Reducer::Append` at the barrier. `Executor::run`,
no checkpointer.

| Branches | Mean | 95 % CI |
|---|---|---|
| 2 | 33.39 µs | [33.15, 33.76] µs |
| 8 | 49.28 µs | [48.98, 49.68] µs |
| 32 | 144.16 µs | [142.42, 146.72] µs |

### Reducer merge cost — `StateSpec::apply_single`

The real barrier merge path (channel validation + reducer), one write per
measurement.

**Overwrite** (replace value of given size):

| Value size | Mean | 95 % CI |
|---|---|---|
| 1 KB | 206.36 ns | [205.08, 207.66] ns |
| 100 KB | 235.64 ns | [230.36, 242.58] ns |
| 1 MB | 253.12 ns | [242.78, 265.07] ns |

**Append** (push one element onto an existing array):

| Existing array length | Mean | 95 % CI |
|---|---|---|
| 10 | 1.45 µs | [1.40, 1.49] µs |
| 100 | 12.45 µs | [12.13, 12.73] µs |
| 1,000 | 123.50 µs | [120.50, 125.96] µs |
| 10,000 | 1.184 ms | [1.175, 1.193] ms |

**DeepMerge** (merge a 10 %-overlap object into an existing object):

| Existing object keys | Mean | 95 % CI |
|---|---|---|
| 100 | 34.50 µs | [33.66, 35.24] µs |
| 1,000 | 412.24 µs | [404.56, 418.71] µs |
| 10,000 | 3.918 ms | [3.863, 3.981] ms |

### Checkpoint serialization + save

Checkpoint carrying a state with a single string payload of the given size.

**Serialize only** (`serde_json::to_vec_pretty`, pure CPU):

| State size | Mean | 95 % CI |
|---|---|---|
| 1 KB | 843.31 ns | [838.21, 849.33] ns |
| 100 KB | 34.57 µs | [34.32, 34.92] µs |
| 1 MB | 368.49 µs | [366.19, 371.17] µs |

**InMemoryCheckpointer::put** (mutex + move into store; payload-independent
because the checkpoint is moved, not copied):

| State size | Mean | 95 % CI |
|---|---|---|
| 1 KB | 1.76 µs | [1.66, 1.85] µs |
| 100 KB | 1.57 µs | [1.48, 1.65] µs |
| 1 MB | 804.09 ns | [758.04, 842.56] ns |

**JsonFileCheckpointer::put** (serialize + atomic temp-write + rename +
latest-pointer write):

| State size | Mean | 95 % CI |
|---|---|---|
| 1 KB | 487.67 µs | [396.72, 611.72] µs |
| 100 KB | 628.27 µs | [588.89, 669.46] µs |
| 1 MB | 1.138 ms | [1.063, 1.226] ms |

**JsonFileCheckpointer::get_latest** (pointer read + file read + deserialize —
the resume-path load):

| State size | Mean | 95 % CI |
|---|---|---|
| 1 KB | 48.27 µs | [47.63, 49.03] µs |
| 100 KB | 67.79 µs | [67.15, 68.54] µs |
| 1 MB | 256.81 µs | [252.52, 264.34] µs |

### Interrupt / resume round-trip

Full HITL cycle through the real executor + `InMemoryCheckpointer`: phase 1
runs and suspends at the interrupting node (checkpoint persisted), phase 2
restores the checkpoint and completes.

| Carried state | Mean | 95 % CI |
|---|---|---|
| Empty | 38.57 µs | [37.82, 39.62] µs |
| 100 KB blob channel | 85.78 µs | [84.13, 87.74] µs |

### State cloning cost

`State::clone()` (deep clone of the underlying `serde_json` map) and the full
serialize → parse round-trip a durable checkpoint pays in both directions.
Payload: one JSON string of the given size plus a small `meta` object.

| Payload size | `State::clone()` | serde round-trip |
|---|---|---|
| 1 KB | 221.30 ns [220.23, 222.75] | 1.08 µs [1.07, 1.09] |
| 100 KB | 1.92 µs [1.91, 1.94] | 47.39 µs [47.18, 47.63] |
| 1 MB | 17.50 µs [17.29, 17.83] | 483.92 µs [477.16, 492.55] |
| 10 MB | 248.65 µs [246.14, 251.26] | 4.61 ms [4.52, 4.76] |

## Interpretation — what these numbers do and do not show

**What they are.** Single-machine microbenchmarks of the core engine in
isolation: graph compilation, the super-step loop, reducer merges, checkpoint
ser/de and savers, the interrupt/resume protocol, and state cloning. All node
"bodies" are small deterministic computations; there is **no network, no LLM
call, no database, no SSE streaming** anywhere in these measurements.

**What they show.**

- **Engine overhead is small relative to real agent work.** A full super-step
  (plan → snapshot → parallel node run → barrier merge → route) costs on the
  order of **10–13 µs** in the sequential-chain measurements. Any node that
  calls an LLM (hundreds of ms to seconds) dwarfs this by 4–5 orders of
  magnitude; engine overhead is not the bottleneck for LLM-bound workloads.
- **Graph compilation is effectively free at realistic sizes** (~1 µs for 10
  nodes, ~126 µs even for a 1000-node chain) and scales linearly.
- **Fan-out scales sub-quadratically in branch count**: 2→8→32 branches costs
  33 µs → 49 µs → 144 µs — roughly linear with a small fixed per-super-step
  base, as expected for barrier scheduling of trivial tasks.
- **`Reducer::Overwrite` is O(1) in value size** (~250 ns flat from 1 KB to
  1 MB): the update is moved, not merged.
- **`Reducer::Append` and `Reducer::DeepMerge` are O(N) per write** because
  each merge clones the current channel value (Append: ~1.4 µs at 10 elements
  → ~1.18 ms at 10,000; DeepMerge: ~35 µs at 100 keys → ~3.9 ms at 10,000).
  Long-lived `Append` channels that grow unboundedly (e.g. accumulating every
  event of a long run into one array) make each subsequent write linearly
  more expensive — a super-step writing into a 10 k-element array pays ~1.2 ms
  for the merge alone. This is the clearest scaling hazard in the current
  design.
- **Checkpointing is cheap until payloads grow.** Serialization runs at
  ~2.8 GB/s (1 MB in ~370 µs); the JSON-file saver adds a roughly constant
  ~450–600 µs of filesystem work (two atomic writes) on top, so it only
  becomes payload-bound past ~1 MB. `InMemoryCheckpointer::put` is
  payload-independent (move semantics, sub-2 µs).
- **Interrupt/resume protocol overhead is ~39 µs** with an empty state —
  i.e. negligible next to any human-in-the-loop latency — rising to ~86 µs
  when carrying a 100 KB state (the checkpoint write + load of the payload).

**On state cloning specifically.** The executor hands every node a full state
snapshot, so cloning is on the hot path. Measured: a 1 MB state clones in
~17.5 µs and a 10 MB state in ~249 µs. Two honest caveats:

1. These payloads are one large JSON **string** — memcpy-bound, a best case
   per byte. A structurally *deep* 10 MB state (hundreds of thousands of small
   values) will clone measurably slower per byte because of per-value
   allocation. The `Append`/`DeepMerge` numbers above are the better proxy for
   structured data.
2. Even so, full-state cloning stays below ~1 ms per super-step up to ~10 MB
   payload on this machine. It becomes *visible* — i.e. comparable to the
   engine's own per-step overhead and worth avoiding — in the **1–10 MB
   range and beyond** (17–250 µs per clone, multiplied by every snapshot the
   executor takes per super-step, plus the ~0.5–4.6 ms serde round-trip when a
   durable checkpointer is attached). Below ~100 KB it is noise (< 2 µs).

**What they do NOT show (explicitly out of scope).**

- **Server load-testing is not covered yet.** No concurrent threads/sessions,
  no SSE streaming throughput, no Postgres checkpointer contention, no
  multi-tenant executor sharing. These are tracked as follow-up work
  (server-level load suite against `rusty-agent-server`, including
  `PostgresCheckpointer` under concurrent writers).
- No cross-machine or cross-OS comparison; no regression history (this is the
  baseline run); no memory-usage or allocation profiling; no comparison
  against LangGraph or other runtimes.
- Criterion measures wall-clock latency of single operations; throughput
  under contention can behave differently.

## Checkpoint-placement headroom — the R0.5 first experiment (2026-08-07)

The [roadmap](roadmap.md) gates R0.10's checkpoint-placement learning on one
question: **after the checkpoints a run is forced to keep — the boundary
after every super-step containing a `NonIdempotent` effect — does any
placement freedom remain worth learning, or does the mandatory floor already
behave like checkpointing every super-step?** This section publishes the
measurement. It ran on the same machine and toolchain as the baseline above,
against the R0.5 Flight Recorder kernel (unreleased, on top of v0.4.0).

**Reproduce:**

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo bench -p rusty-agent-runtime --bench checkpoint_placement
```

Takes ~4 minutes with the bench's tuned Criterion budgets. Timing estimates
land in `target/criterion/placement_*/`; deterministic per-run checkpoint
counts and bytes are printed to stdout with a `PLACEMENT-ACCOUNT` prefix and
asserted against the analytic placement schedule inside the bench.

### Method

Synthetic super-step **chains of 50 / 200 / 1000 steps** (one node per step;
node bodies do real work: read previous channel, increment, write own
channel). Nodes carry **declared `Effect` classes** in a deterministic mix:
mostly `Pure`, 20 % `ReadOnly`, and `NonIdempotent` at **2 % and 10 %
densities**, evenly spread. Checkpointed **state sizes: 10 KB / 1 MB**
(dominant payload: one string channel carried through every checkpoint).
Backend: `JsonFileCheckpointer` — the durable baseline from the checkpoint
bench above; an in-memory backend makes placement meaningless (its `put` is
a payload-independent move, sub-2 µs).

Four placement policies decide, per super-step boundary, whether a
checkpoint is written:

| Policy | Keeps the boundary after super-step `s` when… |
|---|---|
| `uniform` | always — the executor's current behavior |
| `terminal_only` | `s` is the final step — the durability floor |
| `mandatory_only` | the step contained a `NonIdempotent` effect — the floor exact replay imposes |
| `mandatory_periodic_10` | mandatory, or every 10th boundary |

Two measurement layers:

- **End-to-end** (`placement_e2e_chain*`): full `Executor::run` wall time
  behind a bench-local checkpointer that drops the `put`s its policy does
  not select. Includes node execution, Flight Recorder journaling, and
  per-step checkpoint minting — the whole R0.5 system as shipped. State size
  10 KB (at 1 MB the in-memory journal, which retains every step's input
  payload, dominates the run — a separate scaling question from placement).
- **Checkpoint stream** (`placement_stream_*`): the persistence half in
  isolation — for each boundary the policy keeps, mint + durable `put` of a
  real checkpoint, no executor. This is the cost a placement policy actually
  controls, at both state sizes.

Metrics: wall time (Criterion), per-run checkpoint count, total bytes
written (both deterministic — measured by an asserted accounting pass, not
inferred from timing).

### Results

**Checkpoints written per run** (deterministic; bytes = count × 10.73 KB at
10 KB state, × 1.049 MB at 1 MB state):

| Chain | NI density | uniform | terminal_only | mandatory_only | mandatory+periodic-10 |
|---|---|---|---|---|---|
| 50 | 2 % | 50 | 1 | 1 | 6 |
| 50 | 10 % | 50 | 1 | 5 | 10 |
| 200 | 2 % | 200 | 1 | 4 | 24 |
| 200 | 10 % | 200 | 1 | 20 | 40 |
| 1000 | 2 % | 1,000 | 1 | 20 | 120 |
| 1000 | 10 % | 1,000 | 1 | 100 | 200 |

At 1 MB state the 1000-step uniform run writes **1.05 GB**; mandatory-only at
2 % density writes **21.0 MB** — 50× less. Note the periodic term dominates
`mandatory_periodic_10` (every 10th boundary = 10 % of steps, 5× the
mandatory count at 2 % density): it is the chosen cadence that costs, not
the mandatory floor.

**Checkpoint-stream wall time, 1 MB state** (mean; CIs within ±3 % unless
noted):

| Chain | NI density | uniform | terminal_only | mandatory_only | mandatory+periodic-10 |
|---|---|---|---|---|---|
| 50 | 2 % | 43.04 ms | 874 µs | 883 µs | 5.15 ms |
| 50 | 10 % | 42.75 ms | 872 µs | 4.38 ms | 8.48 ms |
| 200 | 2 % | 173.95 ms | 882 µs | 3.43 ms | 20.52 ms |
| 200 | 10 % | 174.98 ms | 879 µs | 17.02 ms | 35.02 ms |
| 1000 | 2 % | 881.71 ms | 861 µs | 17.93 ms | 104.46 ms |
| 1000 | 10 % | 861.73 ms | 868 µs | 85.97 ms | 176.08 ms |

**Checkpoint-stream wall time, 10 KB state:**

| Chain | NI density | uniform | terminal_only | mandatory_only | mandatory+periodic-10 |
|---|---|---|---|---|---|
| 50 | 2 % | 18.31 ms | 381 µs | 438 µs | 2.22 ms |
| 50 | 10 % | 19.72 ms | 402 µs | 1.89 ms | 3.77 ms |
| 200 | 2 % | 76.57 ms | 383 µs | 1.50 ms | 8.66 ms |
| 200 | 10 % | 75.87 ms | 379 µs | 8.36 ms | 14.79 ms |
| 1000 | 2 % | 388.27 ms | 379 µs | 7.23 ms | 53.64 ms |
| 1000 | 10 % | 364.10 ms | 405 µs | 35.94 ms | 75.39 ms |

Per kept boundary: ~0.37 ms at 10 KB, ~0.86 ms at 1 MB — consistent with
the `json_file_put` microbenchmarks above, and wall time tracks checkpoint
count almost exactly (placement cost is linear in kept boundaries; there is
no fixed per-policy overhead to game).

**End-to-end run wall time, 10 KB state** (executor + journal + policy
placement):

| Chain | NI density | uniform | terminal_only | mandatory_only | mandatory+periodic-10 |
|---|---|---|---|---|---|
| 50 | 2 % | 22.11 ms | 4.19 ms | 4.44 ms | 6.31 ms |
| 50 | 10 % | 22.21 ms | 4.22 ms | 5.46 ms | 7.21 ms |
| 200 | 2 % | 98.92 ms | 17.19 ms | 18.68 ms | 25.39 ms |
| 200 | 10 % | 97.74 ms | 17.35 ms | 24.19 ms | 33.86 ms |
| 1000 | 2 % | 552.43 ms | 160.02 ms | 163.47 ms | 206.95 ms |
| 1000 | 10 % | 550.12 ms | 161.30 ms | 194.35 ms | 226.49 ms |

Two consistency checks hold: the end-to-end saving from dropping puts equals
the isolated stream cost within ~1 % (552.43 − 160.02 = 392.4 ms vs 388.3 ms
stream-uniform for the 1000-step 2 % case) — the filtering-checkpointer
model is faithful — and at 2 % density `mandatory_only` is statistically
indistinguishable from the `terminal_only` floor end-to-end (CIs overlap on
all three chains).

### Interpretation

- **The mandatory floor binds almost nothing.** At 2 % density,
  mandatory-only keeps 2 % of boundaries; its count, bytes, and wall time
  sit on the terminal-only floor. Even at 10 % density it stays ~10× below
  uniform on every metric. Mandatory-only is nowhere near uniform, so the
  experiment's kill condition (mandatory ≈ uniform ⇒ placement learning is a
  dead wedge) does not trigger: **90–98 % of placement decisions remain
  free**.
- **Placement cost is linear in kept boundaries** (~0.37 ms at 10 KB,
  ~0.86 ms at 1 MB per durable write), so the value of any learned placement
  policy is exactly bounded by the checkpoints it avoids — no hidden fixed
  costs.
- **Where the freedom is worth real money:** engine-bound, large-state, or
  high-step-rate workloads. In the 1000-step 10 KB end-to-end run, uniform
  checkpointing is **71 % of run wall** (392 ms of 552 ms); mandatory-only
  recovers essentially all of it. At 1 MB state the stakes are 1.05 GB vs
  21 MB written per 1000-step run.
- **Where it is not:** LLM-bound agent runs. A durable checkpoint costs
  ~0.4–0.9 ms; an LLM call costs 100 ms–seconds. Even uniform checkpointing
  is <1 % overhead there, so no placement policy can harvest anything
  meaningful — the wedge's payoff lives in durable-work and state-heavy
  workloads (R0.6/R0.7 territory), not in model-call loops.
- **One incidental observation** (out of this experiment's scope, flagged
  for follow-up): the checkpoint-free end-to-end floor at 10 KB state is
  ~160 µs per super-step for the 1000-node chain — well above the ~13 µs
  pre-R0.5 engine overhead. The Flight Recorder journal (per-step input
  capture and in-memory retention), not checkpointing, is the largest
  placement-independent per-step cost in engine-bound runs at this state
  size.

### Verdict

> **At realistic non-idempotent densities (2–10 % of super-steps), the
> Flight Recorder's mandatory checkpoint floor pins only 2–10 % of
> super-step boundaries — mandatory-only placement wrote 10–50× fewer
> checkpoints and bytes than checkpoint-every-super-step and ran within
> ~2 % of the terminal-only floor — so checkpoint-placement freedom survives
> mandatory checkpoints, and the placement wedge remains open for R0.10
> wherever durable checkpointing is a material share of run cost.**

Corollary for R0.10 prioritization: that material share exists in
engine-bound, large-state, and high-step-rate runs (up to ~71 % of wall here)
and does not exist in LLM-bound runs (<1 %), so checkpoint placement should
rank behind retry/timeout learning and be evaluated against durable-work
workloads, not model-call loops.

## State scaling — the R0.7 wave 4 before/after (2026-08-08)

Wave 4 shipped channel-granularity copy-on-write state (`Arc<Value>` per
channel), delta checkpoints in both durable checkpointers, and a
content-addressed artifact store (see `docs/agent-fabric-design.md`, "State
scaling"). This section publishes the exit numbers against the baseline
above, per the wave's evidence-over-claims gate.

**Method.** *Before* is not the 2026-08-06 baseline re-quoted: it is the
wave's base commit (`583fe9a`, R0.7 W3) re-measured on 2026-08-08 with the
identical Criterion targets, so both columns are same-day, same-machine,
same-harness. Where the base re-measurement drifts from the published
baseline (run-to-run variance on memcpy-bound payloads), both values are
noted. *After* is the wave-4 branch, same day. The `checkpoint_json_file_*`
groups are pinned to `DeltaPolicy::full_only()` so they stay like-for-like
regression guards; new `*_delta` groups measure the new path, and a
`DELTA-ACCOUNT` accounting pass (untimed, asserted) measures on-disk bytes.

| | |
|---|---|
| CPU | Apple M2 Max (12 cores: 8 performance + 4 efficiency) |
| RAM | 96 GB |
| OS | macOS 26.5.1 (Build 25F80), arm64 |
| Rust | rustc 1.97.1 (8bab26f4f 2026-07-14) |
| Criterion | 0.5.1 (default features off: no plotters/rayon) |
| Date of run | 2026-08-08 |
| Crate version | R0.7 wave 4, unreleased, on top of `rusty-agent-runtime` 0.6.0 (before: base commit `583fe9a`) |
| Load | single-user machine, no other heavy processes |

### Exit metric 1 — snapshot cost per super-step

`State::clone()` is now one refcount bump per channel — O(channels), flat in
payload size. The `superstep_snapshot_clones` group measures the executor's
actual per-step fan-out (pre-step snapshot + 4 node clones + checkpoint
copy = 6 clones); pre-wave 4 that cost 6 × the full-clone column.

| Payload | Before: `State::clone()` | After: `State::clone()` | After: 6-clone super-step fan-out |
|---|---|---|---|
| 1 KB | 227.53 ns [224.87, 231.91] | 3.93 ns [3.89, 3.99] | — |
| 100 KB | 1.89 µs [1.87, 1.91] | 3.94 ns [3.91, 3.97] | — |
| 1 MB | 17.16 µs [16.96, 17.37] | 3.88 ns [3.85, 3.91] | 26.17 ns [25.95, 26.38] |
| 10 MB | 312.78 µs [308.70, 317.01] | 3.95 ns [3.92, 4.00] | 26.07 ns [25.85, 26.29] |

The full super-step snapshot fan-out at 1 MB / 10 MB — the wave's first
exit number, ~105 µs / ~1.9 ms per step before (6 × the clone column) —
is now **~26 ns, flat in payload size**. (The base re-measurement of the
10 MB clone, 312.78 µs, sits above the published baseline's 248.65 µs; the
payload is one memcpy-bound string and run-to-run variance there is large.
The 1 MB re-measurement, 17.16 µs vs the published 17.50 µs, matches.)

The serde contract is unchanged and priced: the checkpoint serialize/parse
round-trip pays the same after as before (1 MB: 471.65 µs vs 479.62 µs;
10 MB: 4.43 ms vs 4.60 ms — within noise, no regression), because
serialization walks the whole `Value` regardless of sharing. CoW removes
clone cost; it does not touch serde cost — that is what deltas are for.

### Exit metric 2 — reducer merge cost

The barrier merge now mutates a channel **in place** when the state's
`Arc<Value>` is uniquely owned (Append / DeepMerge / AddMessages), and
copies only that channel when a snapshot still shares it.

**Append** (push one element onto an existing array):

| Existing length | Before | After: unique ownership | After: shared with live snapshot |
|---|---|---|---|
| 10 | 934.31 ns [916.33, 964.51] | 257.12 ns [256.24, 258.06] | 874.72 ns [839.87, 909.98] |
| 100 | 7.77 µs [7.52, 8.03] | 454.93 ns [446.06, 462.22] | 5.93 µs [5.60, 6.28] |
| 1,000 | 68.03 µs [66.61, 69.49] | 1.30 µs [1.28, 1.32] | 58.88 µs [55.56, 61.80] |
| 10,000 | 696.79 µs [693.91, 699.82] | **7.57 µs [7.47, 7.67]** | 496.67 µs [482.89, 511.69] |

**DeepMerge** (10 %-overlap object into existing object):

| Existing keys | Before | After: unique ownership |
|---|---|---|
| 100 | 26.78 µs [26.20, 27.40] | 2.44 µs [2.34, 2.55] |
| 1,000 | 292.41 µs [286.05, 299.58] | 26.21 µs [25.42, 27.05] |
| 10,000 | 3.08 ms [3.03, 3.14] | **270.38 µs [264.48, 276.73]** |

The wave's second exit number — Append at 10,000 elements, 1.18 ms on the
published baseline and 696.79 µs re-measured at the base commit — is
**7.57 µs** when the merge owns the channel (~92× against the base
re-measurement) and **496.67 µs** when a live snapshot forces the
copy-on-write clone (~1.4× — one channel clone instead of the old two).

**The honest gate for `im` (persistent within-channel structures):** the
executor's pre-step checkpoint holds the pre-merge state, so a *durable*
run's barrier merges take the shared column — still O(channel). The
unique-ownership column is what non-durable runs get (the executor drops
its snapshot before the barrier when no checkpointer is attached, so the
merge reaches refcount 1 and mutates in place — verified by pointer
equality in `state.rs` tests). The design's `im` gate — adopt if reducer
merges exceed an agreed share of turn latency — therefore stays **open on
evidence**: a durable run appending into a 10 k-element channel still pays
~0.5 ms per merge. Overwrite is untouched and flat (257.62 ns at 1 MB vs
227.63 ns before; ~13 % overhead from the Arc indirection, absolute cost
still sub-µs).

### Exit metric 3 — checkpoint bytes for the 1000-step / 1 MB run

Delta checkpoints: `Checkpoint` gains an additive `base: Option<String>`;
opting-in checkpointers (`JsonFileCheckpointer`, `PostgresCheckpointer`,
the server store) persist only the channels that changed since the chain
head, bounded by chain length *K* = 32 and a byte ratio (delta ≥ 80 % of
full ⇒ write full), with `fork_thread` compacting eagerly to full
snapshots. All reads fold the chain internally — the trait and the
materialized-`Checkpoint` contract are unchanged.

`DELTA-ACCOUNT` (1000 checkpoints × 1 MB state, `blob` channel constant,
small `meta` channel rewritten per step; asserted, untimed):

| Policy | On-disk bytes | Wall time |
|---|---|---|
| full-only (pre-W4 path) | 1,049,035,813 (1.05 GB) | 884 ms |
| delta (default, K = 32) | **32,994,615 (33.0 MB)** | 996 ms |

The wave's third exit number: **31.8× fewer bytes** for the motivating
uniform-1000-step case (1.05 GB → 33.0 MB; the full-only arm reproduces
the placement experiment's 1.05 GB exactly, on-tree). Wall time is flat
(+13 %): the write path is dominated by per-file atomic-write overhead,
not payload bytes, and the delta arm additionally re-anchors a full write
every 32 steps and reads the chain head per put. Deltas are a *bytes*
optimization — disk, replication, and backup cost — not a latency one.

**Per-operation timing** (mean [95 % CI]):

| Group | 1 KB | 100 KB | 1 MB |
|---|---|---|---|
| `json_file_put`, full-only (regression guard) | 445.55 µs [407.12, 490.47] (before: 443.49 µs) | 518.24 µs [499.40, 536.84] (before: 609.50 µs) | 1.02 ms [0.96, 1.10] (before: 1.09 ms) |
| `json_file_put_delta` (steady chain) | 640.63 µs [525.84, 794.17] | 452.83 µs [418.66, 506.35] | 921.63 µs [853.51, 1046.20] |
| `json_file_get_latest`, full-only | 45.69 µs [45.28, 46.18] (before: 46.33 µs) | 64.91 µs [64.49, 65.37] (before: 65.39 µs) | 236.05 µs [233.31, 239.32] (before: 234.66 µs) |
| `json_file_get_latest_delta` (worst case: head K−1 = 31 deltas above its base) | 480.23 µs [412.94, 543.57] | 502.43 µs [435.87, 565.87] | 692.11 µs [622.04, 757.80] |

Reads: the full-only guard confirms no regression on the pre-W4 load path
(236.05 µs vs 234.66 µs at 1 MB — unchanged). The delta resume path at its
bounded worst (31 delta files + 1 full base read + fold) costs ~2.9× the
single-file load at 1 MB — the design's predicted "sub-millisecond at
CoW-sharing sizes" holds (692 µs), and *K* = 32 caps it there by
construction. `checkpoint_serialize` (357.06 µs → 353.86 µs at 1 MB) and
`checkpoint_in_memory_put` (871.17 ns → 729.48 ns — InMemory opts out of
deltas; the small gain is the CoW checkpoint move) are likewise unchanged
in shape.

### Artifact store (adoption note, not a benchmark)

The content-addressed store shipped with both backends
(`FileArtifactStore`: `{dir}/{sha256}` under the same atomic
temp-write-then-rename discipline as checkpoints; `PostgresArtifactStore`:
`rusty_artifacts` table, `ON CONFLICT DO NOTHING` dedupe), integrity
verification on every read (re-hash against the address; corruption fails
the read rather than returning bad bytes), and the journal persistence
seam (`snapshot_externalized` / `from_snapshot_with_store`) — snapshots
still embed bytes by default, keeping replay fixtures self-contained per
the design. Mailbox/checkpoint-channel spill adoption is deferred (see the
wave-4 annotation in `docs/agent-fabric-design.md`), so no benchmark here
claims end-to-end artifact savings yet.

## Adaptation headroom — the R0.10 gate experiment (2026-08-09)

The [adaptation design](adaptation-design.md) gates the whole R0.10 release
on one question, pre-registered before the bench ran: **per decision family,
can any policy beat the `static-v0` floor, net of the telemetry overhead
that learning imposes?** This section publishes the measurement. The
clairvoyant oracle arm is the point: if even an oracle deciding with full
knowledge of the recorded outcome cannot beat the floor by more than the
instrumentation costs, the family is closed regardless of learner quality,
and the design's negative branch (twin machinery plus published evidence, no
promoted learner) is the outcome. No family hit its kill condition in this
run — but the table below is written to be re-run, and the bar does not
move.

**Reproduce:**

```bash
export PATH="$HOME/.cargo/bin:$PATH"

# The new classes (durable-work, LLM-bound scripted) and the overhead
# measurement; ~30 s with the bench's tuned Criterion budgets:
cargo bench -p rusty-agent-runtime --bench headroom_experiment

# The engine-bound class (the R0.5 checkpoint-placement family):
cargo bench -p rusty-agent-runtime --bench checkpoint_placement
```

Deterministic family metrics (simulated latency, attempts, cost, completion)
come from an untimed, asserted accounting pass printed with `HEADROOM-*`
prefixes; the telemetry overhead is real wall time, Criterion-timed on the
production emission path and re-measured untimed inside the accounting pass
so every verdict row is self-contained.

### Method

**Workload classes.** Three, per the design's measurement protocol:

- **Engine-bound** — the existing `checkpoint_placement` family (the R0.5
  section above). Its row carries over; it was not re-run this cycle.
- **Durable-work** — 400 tasks across four callee profiles (a fast
  payment-style write path, a search read, a notification send, a heavy
  report builder with a fat tail), 10 % declared `NonIdempotent`, failing on
  a scripted schedule: transient errors, rate limits with `Retry-After`
  floors, timeouts, dependency failures, resource exhaustion, and
  permanent-invalid inputs in declared proportions, drawn from committed
  seeds. Two queue-and-worker sub-experiments price the remaining families:
  a 4-worker fleet with scripted degradation windows (placement), and one
  shared callee behind a hard concurrency ceiling of 8 (concurrency). Retry
  decisions are made by the real `classify_retry` / `backoff_delay_ms` /
  `retry_legal_actions` — the floor arm *is* `static-v0`, sourced from
  `ExecutorPolicy::static_v0()`, not restated.
- **LLM-bound scripted** — 40 recorded runs of 24 steps (one model call per
  step, a tool call on 60 % of steps) with realistic latencies (model p50
  2.2 s, σ = 0.45) and per-attempt USD costs, replayed exactly with
  decisions varied. The generator plus its seed is the committed artifact —
  the same discipline as the placement bench's analytic schedules.

**Arms, per family.** The floor (the floor's exact constants: 1 s base /
300 s cap full-jitter backoff, 3 attempts, uncapped timeout and
concurrency); a **clairvoyant oracle** (retries only when a remaining
in-budget attempt succeeds, with no delay beyond the world's own
`Retry-After`; bounds a hang at the 100 ms minimum rung and a completing
attempt exactly at its true latency; never places work on a worker inside
its degradation window; caps concurrency exactly at the ceiling); and one
**cheap feature-based heuristic** (a per-class backoff table; a per-callee
rolling p99-plus-25 % timeout; quarantine-after-`ResourceExhausted`
placement; AIMD concurrency). Each family is priced in isolation: the
family's arm varies while the other decision dimension is pinned at the
floor.

**World constraints, not policy choices** (they bound every arm equally): a
hung attempt with no timeout in force surfaces at the queue's 300 s
lease/visibility boundary and classifies `Unknown`; a callee-supplied
`Retry-After` floors any arm's delay; the timeout ladder's minimum rung is
100 ms. The gates never move for any arm: effect gate, class gate, attempt
budget.

**The pre-registered bar.** Headroom exists for a family when, on at least
one workload class, the clairvoyant arm beats `static-v0` on cost or
latency per run by a margin exceeding the family's measured per-run
telemetry overhead, at non-inferior completion. Telemetry overhead is
charged per run at the instrumented decision rate (the larger of the
floor's and the oracle's) — emission on versus off, the granularity a user
pays. Simulated-world margins are deterministic by construction, so the
confidence intervals that matter are Criterion's, on the overhead term
(measured on the real emission path: `DecisionEvent` construction, feature
assembly, and journaling with the hash chain, the retry emitter calling the
same `retry_decision_event` the scheduler uses).

### Environment

| | |
|---|---|
| CPU | Apple M2 Max (12 cores: 8 performance + 4 efficiency) |
| RAM | 96 GB |
| OS | macOS 26.5.1 (Build 25F80), arm64 |
| Rust | rustc 1.97.1 (8bab26f4f 2026-07-14) |
| Criterion | 0.5.1 (default features off: no plotters/rayon) |
| Date of run | 2026-08-09 |
| Crate version | R0.10 wave 1, unreleased, on top of `rusty-agent-runtime` 0.9.0 |
| Load | single-user machine, no other heavy processes |

### Results

**Telemetry overhead per decision** (Criterion mean [95 % CI]; journal
bytes from the accounting pass's snapshot-delta measurement):

| Emission | Mean | 95 % CI | Journal bytes |
|---|---|---|---|
| Retry decision (wired since R0.8) | 9.99 µs | [9.94, 10.06] µs | 731 |
| Timeout decision (with percentile features) | 14.94 µs | [14.82, 15.11] µs | 871 |
| Timeout feature snapshot alone (p50/p95/p99 over 256 samples) | 3.63 µs | [3.61, 3.65] µs | — |
| Placement decision (with worker-health features) | 12.53 µs | [12.48, 12.59] µs | 867 |
| Concurrency decision | 11.33 µs | [11.29, 11.38] µs | 876 |

**Retry family, durable-work** (400 tasks; latency is per-task wall):

| Arm | Mean | p50 | p95 | Attempts | Wasted | Completion | Dead-lettered |
|---|---|---|---|---|---|---|---|
| floor | 905.4 ms | 218 ms | 3,624 ms | 459 | 64 | 98.8 % | 1 |
| clairvoyant | 818.8 ms | 191 ms | 3,546 ms | 457 | 62 | 98.8 % | 0 |
| heuristic | 976.4 ms | 224 ms | 4,141 ms | 459 | 64 | 98.8 % | 1 |

**Timeout family, durable-work** (tapes with hangs; retry pinned at floor):

| Arm | Mean | p50 | p95 | Attempts | Wasted | Completion | Dead-lettered |
|---|---|---|---|---|---|---|---|
| floor | 14,418 ms | 247 ms | 6,625 ms | 471 | 82 | 97.3 % | 2 |
| clairvoyant | 929.8 ms | 237 ms | 3,984 ms | 471 | 82 | 97.3 % | 2 |
| heuristic | 1,173.2 ms | 247 ms | 4,878 ms | 477 | 88 | 97.3 % | 2 |

**Retry family, LLM-bound** (40 runs; latency is per-run wall; cost in USD):

| Arm | Mean | p50 | p95 | Attempts | Wasted | Cost | Completion |
|---|---|---|---|---|---|---|---|
| floor | 62,266 ms | 67,214 ms | 86,335 ms | 1,448 | 144 | $3.3985 | 70.0 % |
| clairvoyant | 60,793 ms | 65,966 ms | 85,445 ms | 1,442 | 138 | $3.3944 | 70.0 % |
| heuristic | 63,107 ms | 67,564 ms | 88,595 ms | 1,448 | 144 | $3.3985 | 70.0 % |

**Timeout family, LLM-bound** (tapes with hangs; retry pinned at floor):

| Arm | Mean | p50 | p95 | Attempts | Wasted | Cost | Completion |
|---|---|---|---|---|---|---|---|
| floor | 279,212 ms | 364,596 ms | 672,883 ms | 1,629 | 175 | $3.7736 | 85.0 % |
| clairvoyant | 69,070 ms | 71,619 ms | 83,958 ms | 1,629 | 175 | $3.7736 | 85.0 % |
| heuristic | 77,309 ms | 77,076 ms | 111,008 ms | 1,595 | 196 | $3.7291 | 77.5 % |

**Placement family, durable-work** (240 tasks, 4 workers, two scripted
degradation windows; retry pinned at floor):

| Arm | Mean | p50 | p95 | Attempts | Wasted | Completion | Dead-lettered |
|---|---|---|---|---|---|---|---|
| floor | 4,299 ms | 4,391 ms | 8,026 ms | 375 | 144 | 96.3 % | 9 |
| clairvoyant | 4,245 ms | 4,399 ms | 7,872 ms | 240 | 0 | 100 % | 0 |
| heuristic | 4,538 ms | 4,368 ms | 9,245 ms | 242 | 2 | 100 % | 0 |

**Concurrency family, durable-work** (120 tasks against a hard ceiling of
8 in-flight; retry pinned at floor):

| Arm | Mean | p50 | p95 | Attempts | Wasted | Completion | Dead-lettered |
|---|---|---|---|---|---|---|---|
| floor | 2,126 ms | 2,030 ms | 3,119 ms | 336 | 280 | 46.7 % | 64 |
| clairvoyant | 1,600 ms | 1,535 ms | 2,996 ms | 120 | 0 | 100 % | 0 |
| heuristic | 2,169 ms | 2,080 ms | 4,279 ms | 122 | 2 | 100 % | 0 |

**The verdict rows** (margin = floor − clairvoyant mean latency, per run;
overhead = instrumented decision rate × measured per-decision cost, per
run; the bar is margin > overhead at non-inferior completion):

| Family | Class | Floor mean | Clairvoyant | Margin / run | Overhead / run | Completion floor→oracle | Headroom |
|---|---|---|---|---|---|---|---|
| Retry | durable-work | 905.4 ms | 818.8 ms | 34.6 s | 0.6 ms | 98.8 → 98.8 % | **YES** |
| Timeout | durable-work | 14,418 ms | 929.8 ms | 5,395 s | 8.4 ms | 97.3 → 97.3 % | **YES** |
| Retry | LLM-bound | 62.3 s | 60.8 s | 1.47 s | 0.03 ms | 70.0 → 70.0 % | **YES, thin** |
| Timeout | LLM-bound | 279.2 s | 69.1 s | 210.1 s | 0.69 ms | 85.0 → 85.0 % | **YES** |
| Placement | durable-work | 4,299 ms | 4,245 ms | 12.9 s | 6.2 ms | 96.3 → 100 % | **YES** |
| Concurrency | durable-work | 2,126 ms | 1,600 ms | 63.2 s | 4.0 ms | 46.7 → 100 % | **YES** |
| Checkpoint placement | engine-bound | — | — | — | — | — | **YES** (R0.5 row above) |
| Checkpoint placement | LLM-bound | — | — | — | — | — | **NO** (R0.5: <1 % of run wall) |

### Interpretation

- **Timeout is the blowout the design predicted.** "No bound is a policy,
  and rarely the right one": with hangs in the fault schedule, the floor
  discovers each one at the 300 s lease boundary while the oracle pays
  100 ms. Mean latency falls 14.4 s → 0.93 s per task in durable-work and
  279 s → 69 s per run in LLM-bound — margins of four to six orders of
  magnitude over the ~10–15 µs emission cost, at identical completion. The
  heuristic captures ~90 % of the oracle's win, with one honest scar: on
  the heavy-tailed model endpoint its p99-plus-margin bound aborts real
  work, dropping completion 85.0 → 77.5 % (3 dead-lettered runs vs the
  floor's 1). A learned timeout must clear the non-inferiority bar the
  heuristic failed here.
- **Retry headroom is real but bounded by the world's own floors.** In
  durable-work the oracle saves ~10 % of mean latency — most of the floor's
  remaining delay is `Retry-After` and fail latency no policy may skip. In
  LLM-bound runs the margin thins to ~2.4 % of latency and 0.12 % of cost:
  faults are infrequent and delays are small next to minute-long runs, and
  the dominant run-failure source is the non-retryable tail (the effect
  gate plus invalid input), which no retry policy may touch. Both clear the
  bar by orders of magnitude regardless. Note the cheap per-class table
  *underperforms* the floor's jittered exponential on latency in both
  classes (976 vs 905 ms durable; 63.1 vs 62.3 s LLM) — it over-waits on
  `Unknown` and `ResourceExhausted`. The wedge is real; the obvious static
  table does not harvest it.
- **Placement headroom shows up in completion and wasted attempts, not mean
  latency.** The oracle's latency margin is thin (4,299 → 4,245 ms) because
  only tasks landing in a degradation window are affected — but it
  eliminates all 144 wasted attempts and all 9 dead-letters. The
  quarantine heuristic nearly matches it (2 wasted, 100 % completion) at a
  small latency cost for idling a worker through quarantine. This is the
  family's predicted shape: value concentrates in fleets under faults.
- **Concurrency is a completion family.** The uncapped floor thundering 120
  tasks into a ceiling of 8 with a 3-attempt budget produces a rejection
  storm that dead-letters 64 of 120 tasks; the oracle's exact cap completes
  everything with zero waste. The AIMD heuristic also completes everything
  but does not beat the floor's latency (its halving churns through the
  same rejections) — backpressure's value is completion insurance, and its
  engine-bound row is zero by construction (no shared constrained resource
  in that class's definition).
- **The LLM-bound control class confirms the R0.5 pattern and adds one
  exception.** Cost headroom is ~0.1 % and retry latency headroom ~2 % —
  the near-zero prediction holds for retry — but timeout is the exception:
  a 2.5 % hang rate per model call puts a 300 s discovery wait inside the
  median run (floor p50: 365 s), and bounding it is the largest single
  margin in the experiment.
- **Overhead is not the gate anywhere.** The dearest emission (timeout,
  with its percentile snapshot) costs ~15 µs and 871 journal bytes per
  decision; the cheapest margin above is 1.47 s per run. Instrumentation
  would have to get ~10⁵× more expensive, or workloads ~10⁵× thinner, to
  close any row that opened here.

One discipline note: the clairvoyant arm is a *ceiling over the feature
space*, not a learner. A promoted policy will land between the heuristic
and the oracle; the gate establishes only that the wedge exists net of
telemetry. Wave 3's twin evaluation is where a specific candidate proves
its own row.

### Verdict

> **Headroom exists for every family the Wave-1 experiment priced, net of
> telemetry overhead, at non-inferior completion — retry (durable-work and
> LLM-bound, the latter thin), timeout (both classes, the largest margins
> in the experiment), placement and concurrency (durable-work,
> completion-driven) — while checkpoint placement keeps its R0.5 split
> verdict (engine-bound yes, LLM-bound no). Per the design's leanings the
> landing order stands: retry and timeout proceed to Wave 3's landing
> track; placement, concurrency, and checkpoint placement ship shadow-only
> with their emission points and fixtures in place; speculation stays
> deferred. No family hit its kill condition, so R0.10's negative branch
> does not trigger — but the bar, the fixtures, and the overhead
> measurement are committed and re-runnable, so any future row closes by
> re-measurement, not redesign.**

## Adaptation release proof — a promoted policy in production, net of telemetry (2026-08-09)

Wave 1 established that headroom *exists*; wave 3 that the twin gate can
price a candidate. This section publishes the release gate itself: **a
retry policy distilled from twin evidence, promoted through the full
pipeline, measurably improves production traffic net of the telemetry the
improvement costs, at completion parity — and activating `static-v0`
restores the floor byte-for-byte.** The whole proof is one integration
test (`rusty-server/tests/adaptation_release.rs`), so the numbers below
reproduce on any machine that can run the suite; the twin half is
deterministic by construction, the production half is the real queue,
store, registry, and journal.

**Reproduce:**

```bash
export PATH="$HOME/.cargo/bin:$PATH"

# The whole gate, ~5 s; the measurements print on stderr:
cargo test -p rusty-agent-server --test adaptation_release -- --nocapture
```

### Method

**The workload.** Five recorded fixtures, each one idempotent `search`
tool call (100 ms, $0.001), behind a committed fault schedule
(`FaultSchedule`, seed 42) that rate-limits the call's first two attempts
with a 50 ms `Retry-After`; attempt 3 lands in the recorded world and
completes. The same schedule plays in both worlds: injected into the
digital twin (the evaluation half) and scripted into the test's worker
(the production half — real `POST /tasks` → claim → fail → retry →
complete against the JSON-file store).

**The loop, end to end.** Floor traffic runs first and journals its
`policy_decision` events under `static-v0`. The twin re-executes every
fixture under the floor with the fault schedule; its journaled retry
decisions — outcomes annotated from the item terminals, the
application-code boundary the distiller's contract documents — distill a
per-class schedule via `distill_retry_parameters` (`rate_limited`: 100 ms
base, 30 s cap, budget 5; the flat schedule stays the floor's). The
distilled parameters become a `policy` candidate, evaluate through the
server-configured `TwinCandidateEvaluator` (every fixture twice, floor arm
vs candidate arm, wall time the target metric, non-inferior completion
enforced per fixture), and promote through the R0.8 default envelope
(approval-ruled; a scoped `ApprovalToken` admits). The registry activates
the derived version; new production traffic binds it at admission and the
fail path decides through `classify_retry_with_policy` against it —
attempt-1 delays bounded to [0, 100] ms and attempt-2 to [0, 200] ms,
bounds the floor's 1 s base cannot produce on most draws. The drift check
(`GET /policy/drift`) reads the version's journaled production decisions
against its promotion baseline. Finally `static-v0` is re-activated and
the workload replays: the journaled decisions are asserted byte-identical
to chapter one's after normalizing the volatile fields (event id, run and
thread linkage, decision instant). Server jitter draws from OS entropy, so
byte-exactness covers journaled decision content and bounds — never the
sampled delays, which are by design not journaled.

**The ledger.** Telemetry is charged the way wave 1 charged it, per run:
the journaled decision bytes read off the wire, and the emission path
(event construction, serialization, draft, journal record with the hash
chain — the work a settlement pays beyond settling) timed in-process over
10 000 iterations inside the same test, so the verdict row is
self-contained. This is an untimed-test mean on a debug build, not a
Criterion-tuned figure — wave 1's 9.99 µs row remains the tuned reference
for the same path; the proof's assertion uses its own measured number.

### Environment

| | |
|---|---|
| CPU | Apple M2 Max (12 cores: 8 performance + 4 efficiency) |
| RAM | 96 GB |
| OS | macOS 26.5.1 (Build 25F80), arm64 |
| Rust | rustc 1.97.1 (8bab26f4f 2026-07-14) |
| Date of run | 2026-08-09 |
| Crate version | R0.10 wave 4, unreleased, on top of `rusty-agent-runtime` 0.9.0 |
| Load | single-user machine, no other heavy processes |

### Results

**The twin gate** (deterministic; aggregates over the 5 fixtures, per arm):

| Arm | Mean wall / item | p95 latency / item | Attempts | Completion | Dead-lettered | Cost |
|---|---|---|---|---|---|---|
| floor | 2,130 ms | 2,130 ms | 15 | 100 % | 0 | $0.005 |
| candidate | 504 ms | 504 ms | 15 | 100 % | 0 | $0.005 |

Margin: **1,626 ms over 5 fixtures (325.2 ms per item), 4.2× mean wall
time, at identical completion, attempts, and cost** — the floor pays two
jittered backoff draws (means 500 ms and 1,000 ms) where the promoted
schedule pays 50 ms and 100 ms means on the same faults. The verdict the
gate read: `regressed: false`, `delta: +1,626 ms` on `wall_time_ms`, all 5
fixtures matched with no divergences.

**Production traffic** (same fault schedule, real queue): under the
promoted version, attempt-1 and attempt-2 scheduling delays landed inside
the learned [0, 100] ms and [0, 200] ms bounds (floor bounds: [0, 1,000]
and [0, 2,000] ms), the journaled decisions named the derived policy
version with the narrowed effective budget (`max_attempts: 3` —
`min(learned 5, task 3)`), and attempt 3 completed: completion parity with
the floor chapter. The drift check answered `drifted: false` over the
version's production decisions (below the 8-decision evidence minimum,
which itself declares nothing), and refused the floor with `422` after
reversion — the floor was never promoted, so there is no baseline.

**Telemetry, charged per item** (2 retry decisions per item):

| Term | Measured |
|---|---|
| Emission path (construction + serialization + draft + journal record) | 116 µs / decision (untimed-test mean, 10,000 iterations) |
| Journaled decision bytes | 491 bytes / decision |
| Charge per item (2 decisions) | 233 µs + 982 bytes |
| Twin margin per item | 325.2 ms |
| Margin ÷ telemetry | ≈ 1.4 × 10³ |

**The floor's return.** After activating `static-v0`, new traffic binds
the floor at admission, delays re-enter the floor's bounds, completion is
unchanged, and the journaled decision events equal chapter one's
byte-for-byte on the normalized comparison surface (legal sets, selected
actions, features, propensities, version).

### Interpretation

- **The wedge wave 1 priced is harvestable by the pipeline's own
  machinery.** Nothing in the loop was scripted to succeed: the distiller
  earned its per-class entry from twin outcomes at the declared 2 s
  margin, the twin gate priced the candidate against the floor on
  identical seeds and fault schedules, the envelope held the family at
  human approval, and the production fail path resolved exactly the
  schedule the twin priced. The 4.2× wall-time win is the same shape as
  wave 1's retry row — most of the floor's remaining delay was backoff
  the world never asked for (the `Retry-After` floor was 50 ms).
- **Telemetry is three orders of magnitude below the margin it
  measures.** Even charging the untimed (hence conservative — 12× wave
  1's Criterion figure) emission cost, instrumentation would have to get
  ~10³× more expensive, or the workload's margin ~10³× thinner, to close
  this row. The honest caveat stands the other way too: thin-margin
  workloads (wave 1's LLM-bound retry row at 2.4 %) need the tuned
  overhead figure, not this test's, before their own promotion.
- **Byte-exact reversion is the release's safety property, not a
  courtesy.** The promoted body changed decisions only where its
  parameters differ; with `static-v0` re-activated every gate, bound, and
  journaled feature returned to the floor's exact shape. Reversibility is
  what makes the approval envelope's bar safe to lower in a future wave.

### Verdict

> **The R0.10 release gate passes: a retry policy distilled from twin
> evidence and promoted through the full pipeline improved production
> traffic 4.2× on mean wall time — a 325.2 ms per-item margin against a
> 233 µs per-item telemetry charge (≈ 1.4 × 10³×) — at identical
> completion, attempts, and cost, with the evaluation published above and
> reproducible end to end; and `static-v0`'s re-activation restored the
> floor byte-for-byte. The bar for the next candidate does not move: twin
> evidence, envelope admission, margin net of telemetry, completion
> parity, byte-exact reversion.**


## Memory recall — utility re-ranking vs the zero-weight floor (2026-08-16)

R0.13 wave 2's deferred-vector decision rests on one measurement: **does
the journaled utility signal (which records appeared in successful runs)
close recall headroom that the shipped rank leaves on the table?** This
section publishes it. The whole measurement is one deterministic
integration test (`rusty-core/tests/memory_tiers.rs`,
`utility_rerank_beats_the_zero_weight_floor_on_recorded_evidence`); the
numbers below are pinned by the test's own assertions, so they reproduce
on any machine that can run the suite.

**Reproduce:**

```bash
export PATH="$HOME/.cargo/bin:$PATH"

# The measurement prints on stderr; the assertions pin the figures:
cargo test -p rusty-agent-runtime --test memory_tiers \
  utility_rerank_beats -- --nocapture
```

### Method

**The workload (synthetic, journaled).** One user-scope namespace holding
24 *relevant* records (3 key domains × 8 facts each: priority 0,
confidence 0.7) and 6 *stale* records (2 per domain: same tags, priority
10, confidence 0.95 — the base rank prefers them). Uniform content, so
every record costs the same estimate and the budget arithmetic is exact.
378 synthetic runs then journal real `MemoryRead` events through the
shipped `JournaledMemory` seam: each relevant record appears in 12
successful graded runs and one failed run; each stale record in 10 failed
runs and one `Ok` run graded below the 6000-bps success bar (a graded run
below the bar counts as a failure — it completed, poorly).

**The arms.** `build_utility_index` rolls the journals into the derived
index at one stamped instant. Each held-out domain query (`tag = domain`,
matching 8 relevant + 2 stale) assembles twice through
`TieredMemoryDriver`: the **floor** arm (utility weight zero, no
over-fetch — the shipped rank) and the **weighted** arm (utility weight 4,
200% over-fetch). The section budget packs exactly 6 of the 10 matched
records, so the budget-limited recall ceiling is 75%. Recall is
`|packed ∩ relevant| / |relevant|` per domain, aggregated.

### Environment

| | |
|---|---|
| CPU | Apple M2 Max (12 cores: 8 performance + 4 efficiency) |
| OS | macOS, arm64 |
| Rust | rustc 1.97.1 (8bab26f4f 2026-07-14) |
| Date of run | 2026-08-16 |
| Crate version | R0.13 wave 2, unreleased, on top of `rusty-agent-runtime` 0.12.0 |

### Results

The derived signal read back from the journaled evidence: relevant records
at **8666 bps** smoothed success rate (12 successes, 1 failure), stale at
**769 bps** (0 successes, 11 failures).

| Arm | Recall (aggregate) | Per domain | Estimated tokens spent |
|---|---|---|---|
| floor (weight 0, no over-fetch) | 12/24 = **50.0%** | 2 stale + 4 relevant packed | 288 |
| weighted (weight 4, 200% over-fetch) | 18/24 = **75.0%** | 6 relevant packed | 288 |

The weighted arm hits the budget-limited ceiling — the 2 unpacked relevant
records per domain are the budget's, not the rank's — at **identical
token cost** (the budget caps both arms; over-fetch widens the candidate
pool, not the packed section). Under the floor, zero weight flattens every
score and the stable sort keeps the shipped rank byte-for-byte within each
tier (asserted separately against `assemble()`).

### Interpretation

- **The signal the journals already hold moves recall without an
  embedding model, an embedding journal contract, or an index to
  govern.** On this planted-signal workload the headroom the base rank
  leaves (priority-10 stale records crowding out proven-useful ones) is
  closed entirely by the re-rank, at cost parity. That is the R0.10
  discipline applied: measure headroom before buying machinery.
- **The floor is one pointer move away and is the measurement's own
  baseline.** Weight zero is the shipped rank; the weighted arm is a
  `memory_config` candidate's `rank` member, promotable and rollable-back
  through the learn gate (byte-exact, proven in the same test file).
- **What this does not show.** Planted signal on synthetic journals is the
  machinery's proof, not a production recall claim: real workloads have
  noisier outcomes and less separable records. The honest reading for the
  vector decision: utility re-ranking has earned first refusal on recall
  gaps; a published gap *remaining* on recorded production workloads —
  measured this same way — is the case for the reserved
  `MemoryRecord.embedding` field, and not before.

### Verdict

> **Utility re-ranking beats the zero-weight floor on recorded evidence:
> 75.0% vs 50.0% recall of the known-relevant set (the budget-limited
> ceiling), at identical estimated-token cost, with the signal derived
> entirely from journaled `MemoryRead` assemblies joined against terminal
> status and eval scores. The vector question stays deferred — by
> measurement, not by fiat.**


## Capacity envelope (R1.0)

The R1.0 gate measures the server's capacity envelope: how much load one
rusty-agent-server process absorbs over loopback HTTP, driven exactly as a
client would drive it. The harness (`rusty-server/examples/load_envelope.rs`)
boots a real server in-process on an ephemeral port and measures four
surfaces — concurrent blocking runs on a four-node no-op probe graph,
concurrent SSE streams read to their `end` frame, an enqueue → claim →
complete loop on the durable task queue, and the checkpoint writes the runs
imply. It is a dev tool, not a CI gate: nothing references it from test
paths, it cleans up after itself, and no numbers are published here until a
maintainer runs it. Reproduce with (file backend; `WITH_POSTGRES=1`
additionally runs the concurrent-runs and task-queue scenarios against a
throwaway `postgres:17-alpine` container):

```bash
./scripts/load-envelope.sh --json target/load-envelope.json
WITH_POSTGRES=1 ./scripts/load-envelope.sh --json target/load-envelope-pg.json
```

| Scenario | Backend | Concurrency | Throughput | p50 | p95 | p99 | Errors |
|---|---|---|---|---|---|---|---|
| Concurrent runs (4-node graph) | file | 32 | — pending measurement | — pending measurement | — pending measurement | — pending measurement | — pending measurement |
| Concurrent runs (4-node graph) | postgres | 32 | — pending measurement | — pending measurement | — pending measurement | — pending measurement | — pending measurement |
| SSE fanout (`runs/stream`) | file | 32 | — pending measurement | — pending measurement | — pending measurement | — pending measurement | — pending measurement |
| Task queue (enqueue) | file | 32 | — pending measurement | — pending measurement | — pending measurement | — pending measurement | — pending measurement |
| Task queue (claim + complete) | file | 32 | — pending measurement | — pending measurement | — pending measurement | — pending measurement | — pending measurement |
| Task queue (enqueue) | postgres | 32 | — pending measurement | — pending measurement | — pending measurement | — pending measurement | — pending measurement |
| Task queue (claim + complete) | postgres | 32 | — pending measurement | — pending measurement | — pending measurement | — pending measurement | — pending measurement |
| Checkpoint writes (derived) | file | 32 | — pending measurement | — pending measurement | — pending measurement | — pending measurement | — pending measurement |
