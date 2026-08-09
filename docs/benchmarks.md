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
