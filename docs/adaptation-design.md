# Rusty Adaptation design (R0.10)

Rusty's Adaptation release makes the executor's *mechanical* decisions learnable: whether to
retry a failed effect and with what backoff, what timeout bound to apply, which equivalent
worker to place work on, how much concurrency to admit, when to speculate, and where to
place checkpoints. The governing claim, stated precisely: **these decisions have dense
objective signals (cost, latency, completion) and closed action spaces, so they are
learnable the way the roadmap's mechanical-learning-first principle requires — but no
learned policy reaches production without offline and shadow evaluation inside a runtime
digital twin and governed promotion through the R0.8 candidate pipeline.** The digital twin
is the safety mechanism, not a benchmarking convenience: it lets a candidate be compared
against the static floor on identical evidence — including faults that never happened in
production — before a single real run binds it.

The release has three parts, in dependency order: the **headroom experiment** (the roadmap's
gate on the whole release), the **runtime digital twin**, and the **learned decision
families** landed in priority order through the executor policy plane. Everything composes
machinery that exists — the Flight Recorder's determinism seams and effect journal, the
candidate/promotion pipeline (`rusty-core/src/learn.rs`), the policy plane v1
(`rusty-server/src/policy.rs`, `rusty-core/src/record.rs`). What R0.10 adds is the twin, the
application loop that lets non-floor parameters actually steer decisions, and —
conditionally, per family — the learned policies.

## Why this belongs in the runtime

Executor-level adaptation built at framework level — a smarter retry wrapper around one HTTP
client, a hand-tuned timeout constant — loses the same three things framework-level memory
lost before R0.8. **Evidence** is absent: a wrapper sees its own calls, while the journal
already records every effect's class, latency, cost, failure classification, and causal
parentage. **Evaluation** is impossible: "what would this timeout have done on the runs we
already recorded" is only well-posed with the determinism seams and effect journal that make
the recorded world re-runnable. **Governance** is a convention: a tuned constant ships
because someone edited it, with no candidate, no comparison against the floor, no way back
but another edit.

Two roadmap boundaries are load-bearing and unchanged. **Agent and model selection is a
governed semantic policy, never an automatic one**: choosing *which model* or *which agent*
answers is a semantic judgment with sparse, delayed, contested signals; it moves through the
R0.8 candidate pipeline as a human-governed change, not through this release's learned
families. **Interrupt policy is deferred**: the error an interruption prevented never
happens, so there is no outcome signal to learn from, and no twin can manufacture one.

## The headroom experiment (Wave 1, the gate)

The roadmap gates R0.10 on a question that must be answered *before* any learned policy
ships: **can any policy beat the `static-v0` floor, net of the telemetry overhead that
learning imposes, per decision family?** The R0.5 cycle ran this experiment for one family —
checkpoint placement — and published the verdict in [benchmarks.md](benchmarks.md):
placement freedom survives the mandatory floor (90–98 % of boundaries stay free at realistic
non-idempotent densities), but the payoff lives in engine-bound and large-state workloads,
not LLM-bound runs. Wave 1 generalizes that protocol to the remaining families. It measures
*headroom*, not a learner: whether the achievable improvement over the floor exceeds the
cost of the instrumentation — because if it does not, no learner can pay for itself.

### Measurement protocol

**Workloads, three classes**, each reproducible from committed artifacts:

- **Engine-bound** — the `checkpoint_placement` bench family: synthetic super-step chains
  with declared effect mixes, durable checkpointers, the real executor. Prices checkpoint
  placement, concurrency, speculation.
- **Durable-work** — queue-and-worker workloads through the real scheduler (`classify_retry`
  deciding, leases expiring, workers failing on a scripted fault schedule): transient
  errors, rate limits, timeouts, resource exhaustion in declared proportions. Prices retry,
  timeout, placement.
- **LLM-bound scripted** — recorded fixtures whose journaled model and tool calls carry
  realistic latencies and costs, replayed exactly with decisions varied. The control class:
  R0.5 predicts near-zero placement headroom here; the experiment exists partly to confirm
  predictions like it for retry and timeout rather than assume them.

**Arms, per family.** The floor (`static-v0`, the exact constants of
`ExecutorPolicy::static_v0()` — 1 s base / 300 s cap backoff, 3 attempts, uncapped timeout
and concurrency); a **clairvoyant upper bound** (an oracle that decides knowing the recorded
outcome — the family's achievable ceiling over its feature space); and one cheap
feature-based heuristic (a per-class backoff table, a per-tool p99-plus-margin timeout). The
clairvoyant arm is the point: if even the oracle cannot beat the floor meaningfully, the
family is closed regardless of learner quality.

**Metrics.** Cost (USD where priced, attempt-count and wasted-latency proxies where not),
latency (p50/p95 wall), completion rate, and **telemetry overhead** — the wall-time and
journal-bytes delta of emitting `DecisionEvent`s with features and propensities, measured
with emission on versus off on the same workload, charged per run because that is how a user
pays it.

**The bar.** Headroom exists for a family when, on at least one workload class, the
clairvoyant arm beats `static-v0` on cost or latency by a margin exceeding the family's
measured telemetry overhead, with confidence intervals separated, at **non-inferior
completion** — the release proof's constraint applied at the gate, not after it. Margins are
pre-registered in the bench before it runs: the R0.5 discipline of an asserted accounting
pass and a stated kill condition, applied per family. Results publish in
[benchmarks.md](benchmarks.md) in the established format — method, environment table,
results, interpretation, verdict — one section per family.

### The negative branch is a designed outcome

If no family clears the bar, R0.10 ships anyway — as the digital-twin machinery plus the
published evidence, with learned policies deferred. The roadmap's kill condition made
structural, written as a branch, not a failure mode. First, the twin is worth building
regardless: fault injection and counterfactual branches serve evaluation, debugging, and
R0.12's shadow deployments even if no learned policy ever promotes. Second, a published
negative result is platform evidence — evidence-over-claims applied to ourselves. Third, the
gate is per-family and re-runnable: a family that fails today leaves its emission points and
fixtures in place, so a workload shift or a cheaper telemetry path re-opens it by
re-measurement, not redesign. What the negative branch never produces is a learned policy
promoted on hope: the bar is the bar.

## The runtime digital twin (Wave 2)

The twin is a deterministic re-execution environment for recorded runs that answers four
questions plain replay cannot: what happens under faults the recording never saw, under
different concurrent interleavings, if one decision changes, and what a candidate policy
*would have* decided on the same evidence.

### What it reuses

The twin stands on the Flight Recorder, consumed — not re-implemented — per the composition
rule every release since R0.5 has followed:

- **Determinism seams** (`rusty-core/src/journal.rs`): injectable `Clock` and `RngSource`.
  Fault schedules and schedule randomization draw from the same seeded streams, so a twin
  run reproduces exactly from its seed and fixture.
- **Effect journal serving** (`rusty-core/src/replay.rs`): exact replay's `ReplaySource`
  cursor and the record/replay wrapper pairs answer effects from the journal by sequence and
  request hash; the twin serves recorded effects the same way.
- **Fork and branch diff**: `Checkpointer::fork_thread` forks history at any checkpoint;
  `BranchDiff::between` compares continuations logically. Counterfactual branches are
  fork-plus-diff with a decision changed at the fork point — also where R0.5's deferred
  *hybrid* replay lands: serve recorded effects up to the fork, execute with the new policy
  afterward.
- **Portable fixtures**: `ReplayFixture` is the twin's input format; a recorded production
  run becomes a twin case by export, unmodified.

### What is genuinely new

Four mechanisms, none of which exist in the codebase today:

1. **Fault injection.** A *fault schedule* — a deterministic, seeded list of faults attached
   to a twin run: a worker crash at decision `d7` (lease expiry, the `ErrorClass::Unknown`
   path), a callee timeout on the third attempt, a provider rate limit
   (`ErrorClass::RateLimited` with a `Retry-After` floor) for a window, resource exhaustion
   on one worker. Injection lands at decision points and the effect boundary, never by
   patching code: the twin's scheduler reads the schedule the way the production scheduler
   reads the world. This lets retry, timeout, and placement policies be evaluated against
   faults rarer than any recorded window contains — and makes Wave 1's durable-work class
   possible at all.
2. **Schedule randomization.** The journal contract is honest that within one super-step,
   parallel tasks' logical clock ticks interleave by schedule: the total order of evidence
   is stable, per-node latencies are not. The twin exploits the seam — the parallel task set
   driven by a seeded scheduling order — to re-run a recorded run under N interleavings, so
   a concurrency policy is evaluated against the interleaving distribution, not the one
   schedule the recording happened to get.
3. **Counterfactual branches.** Fork at a decision, apply one different legal action (a
   longer timeout, `Retry` where the floor aborted), and continue with effects served where
   the decision leaves their inputs untouched and fault-injected where it does not. The
   branch journals normally, so the comparison is `BranchDiff` evidence, not a log line.
4. **Shadow policies.** The candidate decides; `static-v0` acts. Both policies score the
   same features; the floor's action executes; both actions journal as `DecisionEvent`s —
   the candidate's marked shadow with its propensity, the floor's as the acting policy with
   its own. Where the two diverge, a counterfactual branch estimates the outcome the
   candidate would have produced. This resolves the R0.8 propensity caveat by construction:
   a shadow policy exploring by seeded draw is a stochastic policy with known propensities,
   and its journaled decisions are well-posed off-policy evidence for the *next* candidate —
   the contract frozen in R0.5 earning its keep on schedule.

**The honest edge.** The twin is a model of the runtime, not of the world. A counterfactual
decision whose downstream effects are replay-servable — retry counts, backoff delays,
timeout bounds, placement among equivalent workers, concurrency caps, checkpoint cadence —
is exactly evaluable, because those decisions change *when and whether* effects execute, not
what they return. A decision that would change an effect's *input* (a different prompt, a
reformulated request) is unevaluable: the journal has no answer to a call the recorded world
never received. The twin bounds its claims to the first class — precisely the mechanical
families this release covers — and says so in every report it emits. Semantic changes stay
with the R0.8 evaluation path, where evidence is real traffic against a real world.

## The decision families (Wave 3)

Six families in roadmap priority order. For each: features, the legal action set (the closed
`DecisionAction` enum — learned policies choose among declared members, never free-form
outputs), reward signal, telemetry cost, and the landing recommendation. "Land" means
production promotion through the policy plane in R0.10; "shadow-only" means twin and shadow
evaluation ship but the promotion bar stays closed pending the family's headroom row.

### 1. Same-operation retry with classified failures — land

The decision `classify_retry` (`rusty-core/src/durable.rs`) already makes: effect gate
(never silently retry work that is not freely repeatable), class gate (`InvalidInput` and
`Cancelled` fail immediately), attempt gate (dead-letter at the budget), full-jitter
exponential backoff. Learning tunes the numbers per context — never the gates.
**Reformulation is never an ordinary retry**: re-attempting with changed input is a new
effect, outside this family and outside the twin's counterfactual reach; an `InvalidInput`
fails immediately because the same bytes fail the same way, and no learned policy may route
around that gate.

- **Features**: `ErrorClass` (the eight-member closed enum, declared by the executor of the
  work), attempt ordinal, effect class, callee identity, the callee's recent failure and
  latency rates, the task's attempt history.
- **Legal actions**: `Retry { attempt }` with a policy-parameterized delay within declared
  bounds, or `Abort`. Both members exist today.
- **Reward**: expected attempts-and-latency to completion, charged with wasted-attempt cost;
  dead-letter rate as the completion-side penalty.
- **Telemetry cost**: lowest of any family — the emission point is wired
  (`retry_decision_event`, journaled as `PolicyDecision` on the fail-task path since R0.8);
  learning adds fields to an event that exists.
- **Why first**: emission, parameter contract (`RetryPolicyParameters`), and the floor's
  exact constants all exist; the improvement mechanism — shorter expected recovery under
  transient faults, earlier abort under permanent ones — is legible, and the durable-work
  class prices it directly.

### 2. Timeout/stopping from per-tool latency and hazard — land

Today nothing imposes operation timeouts (`TimeoutPolicyParameters` pins the shape;
`static-v0` leaves both fields `None` — "no timeout policy in force"). The family learns a
bound from the journal's own latency evidence: per-tool latency distributions and the hazard
rate — given an operation has run this long, the probability it completes at all.

- **Features**: the callee's journaled latency percentiles (p50/p95/p99) and hazard at the
  elapsed time, effect class, attempt ordinal, the run's remaining budget (`RunBudgets`,
  R0.7).
- **Legal actions**: `SetTimeout { millis }` from a bounded ladder — discrete rungs between
  a floor (below which everything aborts early, a correctness hazard) and `max_millis`. A
  learned policy picks a rung, never an arbitrary integer.
- **Reward**: completion at non-inferior latency — hung-work wall time reclaimed, minus
  premature-abort cost (an aborted attempt that would have completed is a wasted attempt
  and, for non-idempotent effects, a gated failure).
- **Telemetry cost**: an emission point to add at the operation deadline decision; feature
  assembly reads server-side rolling percentiles, journaled as the feature snapshot at
  decision time.
- **Why second**: the parameter shape exists, the feature evidence is already journaled
  (`latency_ms` on every effect event), and timeout is where the floor is most clearly
  beatable — "no bound" is a policy, and rarely the right one. Lands alongside retry.

### 3. Equivalent-worker placement — shadow-only

- **Features**: worker health and queue depth, per-worker latency history for the handler
  class, the pool's pinned version set, recent `ResourceExhausted` classifications per
  worker.
- **Legal actions**: `SelectWorker { worker }` over the pool's eligible, version-compatible
  members — equivalence is a precondition, never a learned judgment: the policy ranks
  workers the manifest already declares interchangeable.
- **Reward**: completion latency, reassignment rate after lease expiry, crash-avoidance
  (work steered off a degrading worker before the lease proves it).
- **Telemetry cost**: the heaviest feature pipeline of the six — placement features require
  worker-fleet telemetry rolled into the decision point.
- **Leaning**: shadow-only. No parameter contract exists
  (`ExecutorPolicy::with_family_parameters` rejects this family today), no emission point
  exists, and the value concentrates in multi-worker fleets under faults — exactly what the
  twin's fault injection is for. The twin measures it; promotion waits for its headroom row.

### 4. Concurrency/backpressure — shadow-only

- **Features**: queue depth, in-flight count per pool, downstream rate-limit signals
  (`RateLimited` classifications, `Retry-After` floors), the run's concurrency budget and
  its inherited bounds.
- **Legal actions**: `SetConcurrency { limit }` from a declared ladder, bounded above by
  pool and tenant quota (R0.6 machinery — a learned limit may only narrow what the quota
  admits, never widen it).
- **Reward**: throughput at bounded tail latency, minus rate-limit penalty (backing off
  before the callee asks beats backing off after).
- **Telemetry cost**: moderate — the signal is fleet-wide, so the feature snapshot is a
  cross-run read at decision time.
- **Leaning**: shadow-only. Static pools and quotas already cover the common case well, and
  the ceiling over static is the most workload-dependent of the six; the headroom experiment
  decides.

### 5. Side-effect-free speculation with budget — defer the family

Speculation executes work before it is known to be needed and keeps the result if it becomes
needed. The safety rule composes the typed effect kernel rather than inventing a parallel:
**only provably `Pure` effects may be speculated** — the `PureEffect` marker trait's
contract (`rusty-core/src/effects.rs`) is precisely "safe and equivalent to re-execute," the
speculation precondition made a type. A `ReadOnly` fetch is not speculatable (the world may
change between the speculative read and the real one); anything with side effects is not
speculatable at all. Spend is bounded by a declared speculation budget inside `RunBudgets`;
wasted speculative work is journaled with its cost, the discipline R0.7's race pattern
already applies to losing candidates.

- **Deviation, stated plainly**: `DecisionFamily` has no `Speculation` member — the R0.5
  contract froze five families (`rusty-core/src/record.rs`), so this family needs an
  additive variant plus `DecisionAction` members (sketch: `Speculate { budget_class }` /
  `DeclineSpeculation`). Additive enum growth is the established evolution rule; it is still
  contract work this release need not do if the family defers.
- **Features**: the probability the speculated branch is taken (routing history), the
  speculative work's cost, the latency it would save, idle capacity.
- **Reward**: latency saved on hits minus wasted compute on misses, charged against the
  budget.
- **Leaning**: defer the family — ship the twin machinery that can measure it
  (counterfactual branches are the measurement), not the policy. The payoff concentrates in
  engine-bound runs with predictable routing; the headroom experiment says whether that
  justifies a new contract variant, and deferring keeps the contract frozen until evidence
  asks.

### 6. Checkpoint placement — shadow-only, gated on residual freedom

The R0.5 experiment already answered the gating question with nuance: the mandatory floor
binds only 2–10 % of boundaries, so freedom survives — but the payoff lives where durable
checkpointing is a material share of run cost (up to ~71 % of wall in the 1000-step
engine-bound bench), under 1 % in LLM-bound runs. Learning placement means choosing which
free boundaries to keep, trading checkpoint bytes and wall time against resume re-execution
cost.

- **Features**: steps since the last checkpoint, state size and delta size (R0.7's delta
  checkpoints changed the cost model — placement now prices *delta* writes, not full ones),
  effect mix of recent steps, the workload's fork frequency (a boundary nobody forks from is
  worth less).
- **Legal actions**: `WriteCheckpoint` / `SkipCheckpoint` — the members exist;
  mandatory-floor boundaries are not in the legal set (a learned policy may add checkpoints,
  never drop a mandatory one).
- **Reward**: checkpoint bytes and wall time avoided, minus expected resume re-execution
  cost given the workload's crash rate.
- **Telemetry cost**: an emission point at the checkpoint decision, cheap in bytes, but the
  feature pipeline reads checkpointer internals.
- **Leaning**: shadow-only. No `ExecutorPolicy` parameter contract exists (rejected by
  `with_family_parameters`), and the value is real but narrow — durable-work and state-heavy
  workloads, not the common LLM-bound case.

## Governance wiring

Learned policies move through the R0.8 candidate pipeline unchanged in shape; R0.10's work
is wiring the pipeline's output back into the executor.

- **A learned policy is a `CandidateKind::Policy` candidate** (`CandidateContent::Policy {
  family, parameters }`): immutable, content-addressed, distilled with a journaled evidence
  span naming the twin runs and shadow evaluations it learned from. The distiller is
  application code (R0.8's boundary); the runtime owns the contract, the gates, the
  journaling.
- **Evaluation happens in the twin.** The `CandidateEvaluator` seam gains a twin-backed
  implementation: replay the evidence span's fixtures with the candidate shadowing the
  floor, inject fault schedules, diff counterfactual branches, produce the journaled
  `CandidateEvaluation`. The verdict's target metric names cost or latency net of the
  family's measured telemetry overhead, so the gate reads the same accounting Wave 1
  pre-registered.
- **Promotion is envelope-gated, and the envelope stays narrow.** The R0.8 default holds
  `policy` candidates at `EnvelopeRule::Approval` — a human `ApprovalToken` scoped to
  `promotion_effect_id`, whatever the evidence says. R0.10 keeps that rule; a family's rule
  widens to `Canary` only after shadow evidence accumulates (open question 3), never to
  unrestricted `Auto` for timeout and concurrency, whose blast radius is fleet-wide.
- **Promotion produces a registry body and an activation.** The candidate's parameters
  overlay the active policy via `ExecutorPolicy::with_family_parameters` — which must first
  *gain* the missing family contracts, a concrete deviation: it currently rejects
  `WorkerPlacement` and `CheckpointPlacement`. The result registers under its
  content-derived version (`derive_policy_version`), the activation append moves the epoch,
  new runs bind at admission through `PolicyBindingCheckpointer`, and in-flight runs keep
  their pinned version.
- **The application loop is R0.10's core executor change.** The v1 plane is deliberately
  evidence-only: it binds versions and journals decisions, but no mechanism reads non-floor
  parameters back into queue decisions (the R0.8 wave-4 annotation says so outright). Wave 3
  closes that gap for the landed families: decision points read the bound version's
  parameters — retry's backoff table and attempt budget, timeout's ladder position — with
  the floor's constants as the read path when the version names no override. The gates never
  move: effect gate, class gate, mandatory checkpoint floor, quota ceilings are
  policy-independent.
- **Drift detection extends the R0.8 monitor to policy surfaces.** The promoted version's
  runs are sampled into scheduled twin re-evaluations against the promotion-time fixtures
  plus fresh recordings; drift is declared thresholds on journaled metrics — completion-rate
  drop, p95 latency or dead-letter growth beyond the evaluation's thresholds — honest about
  answering "is the promoted version regressing against the evidence that promoted it,"
  nothing deeper.
- **Revert-to-default is always one activation away.** The floor is never registered and
  always resolvable (`rusty-server/src/policy.rs`: "activating the static floor is always
  legal"); rollback re-points the version pointer byte-exactly, or the deployment activates
  `static-v0` outright. `static-v0` remains the default and the floor forever: every
  candidate is evaluated against it, and no promotion is required for a deployment to stay
  on it indefinitely.

## What R0.10 deliberately does NOT build

- **No learned agent or model selection.** A governed semantic policy, per the roadmap:
  through the R0.8 candidate pipeline under human governance, never through this release's
  learned families.
- **No interrupt policy.** The prevented-error counterfactual is unobservable; there is no
  outcome signal and the twin cannot manufacture one. Deferred, and stated as deferred
  rather than smoothed over.
- **No open-ended self-modification.** Graph topology is code, pinned by `graph_hash`; no
  candidate kind touches it; learned policies choose among closed enum actions under gated
  bounds — mechanical-learning-first as a hard constraint.
- **No gate bypass.** There is no path — not an API, not a config flag, not a distiller with
  special standing — from a learned parameter set to a bound run that skips candidate
  creation, twin evaluation, envelope-gated promotion, and the journaled activation. The
  floor needs no promotion precisely because it predates the registry; everything else does.
- **No online learning.** Nothing updates a parameter inside a live run. Policies change
  between runs, by epoch, through the pipeline.
- **No free-form policy outputs.** A learned policy emits a member of the family's closed
  `DecisionAction` set with a declared propensity. A policy that wants an action outside the
  set is asking for a contract change, which is a release, not a promotion.

## Wave plan and release proof

**Wave 1 — the headroom experiment.** The three workload classes, the three arms per family,
telemetry-overhead measurement, pre-registered bars. Exit: the per-family headroom table
published in [benchmarks.md](benchmarks.md) in the established format, with each family's
verdict — land, shadow-only, or closed — and the negative branch written where it applies.
The experiment is the gate: Wave 3's scope is exactly the families whose rows clear the bar.

> **Wave 1 status: implemented (2026-08-09).** The experiment landed as
> `rusty-core/benches/headroom_experiment.rs` (`cargo bench -p
> rusty-agent-runtime --bench headroom_experiment`), with the engine-bound
> class carried from the R0.5 `checkpoint_placement` family. Durable-work
> and LLM-bound-scripted classes run the floor (the real `classify_retry`
> with `ExecutorPolicy::static_v0()`'s exact constants), a clairvoyant
> oracle, and one cheap heuristic per family over seeded world tapes —
> scripted fault schedules with transient errors, rate limits with
> `Retry-After` floors, hangs, and resource exhaustion in declared
> proportions — with each family priced in isolation and telemetry overhead
> Criterion-timed on the real emission path (~10–15 µs and 731–876 journal
> bytes per decision). **Every priced family cleared the pre-registered
> bar; no kill condition triggered.** Timeout is the largest margin in the
> experiment (the floor's 300 s lease-boundary hang discovery versus the
> oracle's minimum rung: ~13.5 s per durable task, ~210 s per LLM run, at
> identical completion); retry is real but bounded by `Retry-After` floors
> (~10 % of durable-task latency, thinning to ~2 % of latency and ~0.1 % of
> cost in LLM-bound runs — the R0.5 control-class prediction confirmed);
> placement and concurrency headroom is completion-driven (wasted attempts
> and dead-letters eliminated, mean latency nearly unchanged). Two honest
> scars, published with the numbers: the cheap backoff table underperforms
> the floor's jittered exponential on latency in both classes, and the
> p99-plus-margin timeout heuristic fails the non-inferiority bar on the
> heavy-tailed model endpoint (premature aborts, 77.5 % vs 85 %
> completion). Checkpoint placement keeps its R0.5 split verdict
> (engine-bound yes, LLM-bound no); speculation stays deferred with no
> measurement, as designed. The full table, method, and per-family
> interpretation are the "Adaptation headroom" section of
> [benchmarks.md](benchmarks.md); Wave 3's scope per the gate is retry and
> timeout on the landing track, placement, concurrency, and checkpoint
> placement shadow-only. No core-file changes were needed: the emission
> seams (`retry_decision_event`, `ExecutorPolicy::static_v0`, the
> `DecisionEvent` contract) already exposed everything the arms required.

**Wave 2 — the runtime digital twin.** Fault schedules, schedule randomization,
counterfactual branches (fork + hybrid continuation + `BranchDiff`), shadow decisions with
dual `DecisionEvent` emission. Exit: a recorded run replays under a fault schedule
deterministically (same seed, same journal hash); a counterfactual branch over one changed
retry decision produces a journaled branch diff; a shadow policy's decisions journal with
propensities alongside the floor's, and the pair reproduces exactly under re-run.

> **Wave 2 status: implemented (2026-08-09).** The twin landed as
> `rusty-core/src/twin.rs` with additive seams only: `DecisionRole` on
> `DecisionEvent` (absent from the wire when unset, so the R0.8 contract is
> byte-stable), `RngSource::next_f64` for seeded draws beyond id minting, and
> `replay::SERVABLE_KINDS` made public so replay and the twin share one
> servable-kind vocabulary. All four mechanisms are in: `FaultSchedule`
> (attempt, decision-point, window, and worker anchors over the four
> injectable faults — crash-as-`Unknown` at the lease boundary, callee
> timeout, rate limit with `Retry-After` floor, resource exhaustion), seeded
> schedule randomization of each super-step's parallel task set (journaled
> order stays canonical; admission waits and per-node latencies follow the
> drawn order), counterfactual forks validated against the recomputed legal
> set — illegal forks and forks at decisions the run never reaches are
> refused with a typed `UnevaluableCase` — with
> `CounterfactualFork::then_act_with` landing R0.5's deferred hybrid replay,
> and shadow policies whose decisions journal as `PolicyDecision` pairs
> (`acting`/`shadow` roles, true propensities, seeded-draw exploration)
> alongside the floor's. Every `TwinReport` carries the validity bound as a
> required field (open question 5, resolved as leaned), plus fired/declared
> fault counts and shadow divergences. Determinism is the test bar and it
> holds: same seed + fixture ⇒ byte-identical journals across repeated runs
> and across process invocations (a checked-in golden pins head hash,
> metrics, and report). Two modeling decisions, stated in the module docs
> and here: the twin's scheduler is synchronous and simulated (the fork is
> journal-level — `BranchDiff` evidence, as specified — rather than
> `Checkpointer::fork_thread`, because the twin re-drives the journaled
> effect set rather than the graph executor; checkpoint-cadence evaluation
> stays with the R0.5 checkpoint machinery), and a recorded error classifies
> `Unknown` on re-observation (the recording is all the twin knows). Wave
> 3's twin-backed `CandidateEvaluator` consumes `Twin::run`,
> `Twin::run_interleavings`, `Twin::counterfactual`, and the shadow-pair
> evidence directly.

**Wave 3 — the learned families through the policy plane.** Retry and timeout land (the
recommendation, subject to Wave 1): the application loop reads bound policy parameters at
the decision points, the timeout emission point is added, the twin-backed
`CandidateEvaluator` is wired, and the missing family parameter contracts land only for
families Wave 1 opened. Exit: a learned retry policy and a learned timeout policy each
promote through the envelope in a test deployment — distilled from twin evidence, evaluated
against the floor on identical fixtures plus fault schedules, promoted with a scoped
approval, bound at admission, rolled back byte-exactly; every transition journaled.

> **Wave 3 status: implemented (2026-08-09).** The parameter contracts
> landed as `BackoffParameters` (with a per-class table) on
> `RetryPolicyParameters` and a per-callee table on
> `TimeoutPolicyParameters` — both additive and absent from the wire when
> unset, so the floor's R0.8 shape is byte-stable — validated against
> declared envelopes (`POLICY_MAX_DELAY_ENVELOPE_MS`,
> `POLICY_MAX_ATTEMPTS_ENVELOPE`, `MIN_TIMEOUT_RUNG_MS`) at
> `ExecutorPolicy::with_family_parameters`, so an out-of-envelope
> candidate is rejected at the gate's own parse path, and a hand-built
> invalid policy that somehow reaches a decision point steers nothing
> (the resolution fails closed to the floor). The application loop is
> `resolve_retry_parameters` / `classify_retry_with_policy` for retry
> (per-class schedules, budgets narrowed — never widened — by
> `min(task, learned)`; `classify_retry` delegates with the floor's
> resolution, so the R0.5/R0.8 decision contract is byte-stable) and
> `resolve_timeout_bound_ms` with the new `timeout_decision_event`
> emission point for timeout (closed ladder legal set, smallest covering
> rung, acting version and degenerate propensity journaled exactly as the
> retry family's). `ParameterizedPolicy` steers twin runs with the same
> resolutions and the floor's stance for the shadow-only families; a
> floor-parameterized instance re-executes a recorded run byte-identically
> to the `StaticFloor`, which is what makes revert-to-default exact. The
> learners are closed-form grid searches over the declared envelopes
> (open question 1's leaning, landed): `distill_retry_parameters` keeps
> the floor's jittered shape and fits base/cap/budget per class, gated by
> a declared margin — the first Wave 1 scar, so a fit that does not beat
> the floor earns nothing — with the permanent-failure stance
> (`max_attempts: 1`) earned only by terminal-failure evidence;
> `distill_timeout_parameters` reads the premature-abort fraction off the
> empirical completion distribution and abstains when no rung fits the
> tolerance — the second Wave 1 scar, abstained from rather than shipped
> — and never emits the ladder's top rung. `TwinCandidateEvaluator` is
> the `CandidateEvaluator` seam's policy implementation: every fixture
> re-executed twice on identical seeds and fault schedules, non-inferior
> completion enforced per fixture and in aggregate, `delta` signed so
> positive is better on the request's target metric, both reports
> carrying the aggregate the drift baseline later reads back. Drift
> detection (`detect_policy_drift`) compares the acting version's
> journaled outcomes — shadow decisions excluded — against the
> promotion-time baseline on completion drop, dead-letter growth, and p95
> latency ratio, and declares nothing under the evidence minimum. The
> twin gate is exercised end to end in `tests/learn.rs`: a shorter
> backoff wins on wall time at identical completion under an injected
> rate-limit window and clears the Auto envelope's evidence bar; a
> truncating timeout bound regresses completion and is refused
> mechanically; under the R0.8 default the family's bar stays the
> human's, a scoped `ApprovalToken` admits and a foreign one mismatches.
> **Deferred to a later wave:** threading promoted bounds through
> `rusty-server`'s production fail path (`tasks.rs` / `server_store.rs`)
> and the drift-check HTTP endpoint — the core's application loop and the
> twin exercise the contracts; the server's scheduler still decides on
> the floor.

**Wave 4 — release proof and evaluation publication.** The roadmap's sentence, automated as
an integration test in the release-proof family and published as a benchmarks.md section:
**a learned policy reduces cost or latency net of telemetry overhead at non-inferior
completion, with the evaluation published.** Concretely: a durable-work workload with a
scripted fault schedule runs under the floor and its journals are recorded; a retry policy
distilled from twin evidence promotes through the pipeline; new traffic under the same fault
distribution shows the cost or latency improvement with telemetry overhead charged, at
completion parity; the evaluation — fixtures, verdict, thresholds, overhead measurement — is
the published section. The test then activates `static-v0` and asserts the floor's behavior
returns, byte-exact.

## Open questions

Flagged before Wave 1 lands:

1. **Where the learned function lives.** A learned policy must score features into a closed
   action set cheaply and deterministically at decision time. Leaning: the runtime owns
   small closed-form scorers — per-class tables, bounded ladders, threshold rules —
   serialized as the policy's parameters; the fitting (grid search up to a contextual bandit
   over the twin's shadow logs) happens offline in the distiller. No model serving inside
   the executor: the decision path stays a lookup, and the policy body stays inspectable as
   data.
2. **Speculation's contract variant.** Additive `DecisionFamily::Speculation` plus action
   members, versus modeling speculation under `Concurrency`. Leaning: the additive variant,
   and only when the family lands — budgeted execution of provably `Pure` work is
   semantically distinct from concurrency caps, and overloading would confuse the off-policy
   evidence. Deferring the family defers the variant, which is the right order.
3. **When the policy envelope widens.** R0.10 keeps `policy` candidates at `Approval`.
   Leaning: a family graduates to `Canary` (seeded-draw binding, the floor serving the
   remainder) after two releases of clean shadow evaluations, and never to unrestricted
   `Auto` for timeout and concurrency — a wrong timeout bound fails fleet-wide work, exactly
   the class of mistake the gate exists to make attributable and reversible.
4. **Timeout hazard features.** How much history the percentile snapshot carries, and at
   what granularity. Leaning: server-side rolling percentiles per (callee, effect class)
   bucket over a declared window, journaled as the feature map at decision time — the
   feature is then evidence the twin can reproduce, not a query re-run against mutated state
   (R0.8's journaled-assembly argument applied to policy features).
5. **The twin's validity boundary in reports.** Every twin evaluation states which decisions
   were exactly evaluable (replay-servable or fault-injected downstream) and which were
   excluded as input-changing. Leaning: make the boundary a required field of the evaluation
   payload rather than prose — an auditor should read the constraint from the record, not
   trust the report's framing.
6. **Overhead accounting granularity.** Telemetry overhead charged per run versus per
   decision. Leaning: per run, measured as wall-time and journal-bytes delta with emission
   on versus off on the same workload — the granularity at which the release proof's "net of
   telemetry overhead" clause is actually paid.
