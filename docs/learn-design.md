# Rusty Learn design (R0.8)

Rusty's Learn release gives the runtime a governed memory and a governed
learning loop. The learning rule, stated precisely: **no learning process may
silently rewrite a production prompt, graph, policy, memory, or tool
permission. Learning produces an immutable candidate; the candidate is
evaluated against recorded evidence; promotion is a journaled runtime
transition bounded by a declared envelope; rollback re-points an immutable
version pointer.** What a framework does with a config file edit and a
restart, Rusty does as an evidence-carrying state transition — attributable
to an author, an evaluation, and an approver, and reversible in one
operation.

The release has four parts, each composable on its own: **governed memory**
(records with provenance, confidence, validity, expiration, supersession, and
scope), the **correction loop** (human corrections become attributed
candidate memories and examples, never in-place rewrites), the **learning
loop** (observe → distill → evaluate → promote → monitor → roll back), and
the **executor policy plane v1** (epoch-bounded immutable policy versions
over the closed action sets the R0.5 contract already froze). Contracts land
first, in `rusty-core/src/memory.rs` and `rusty-core/src/learn.rs` — as with
`durable.rs` and `agents.rs` before them, the core crate, the server, and the
SDKs must agree on the shapes byte-for-byte, and golden-file tests pin them.

## Why this belongs in the runtime

Agent memory built at framework level — a vector store wired into a prompt
template, a JSON blob the agent edits through a tool call — loses the same
three things framework-level multi-agent code lost before R0.7. **Scope** is
a convention: nothing stops a support agent's write from landing in the pool
every agent reads, because "user memory" and "team memory" are strings in
application code. **Provenance** is absent: a record cannot answer who wrote
it, from which run's evidence, with what confidence, and against which
superseded fact — so when behavior changes, there is nothing to audit.
**Mutation is silent**: the self-editing-memory pattern (an LLM calling
`memory_replace`) rewrites production behavior in place with no candidate, no
evaluation, and no way back but a database restore. The runtime already holds
every piece needed to do better — scoped stores (R0.7's `StateScope`), a
frozen learning contract (`DecisionEvent`, R0.5), an evaluation harness
(`rusty-eval`), and journaled, replayable evidence — so R0.8 builds memory
and learning where those primitives live, the same argument that put
supervision in the runtime rather than in a process table.

## Lineage, named

Rusty Learn stands on established work, and says so:

- **MemGPT / Letta** (Packer et al., arXiv:2310.08560) — tiered agent memory
  with the OS paging analogy: core memory always in context, archival memory
  outside it, the agent paging between tiers through explicit tool calls. We
  adopt the explicit-operation discipline — memory writes are declared
  operations, not implicit side effects — and reject the self-editing part:
  in Rusty, an agent's write produces a governed record under scope and
  provenance rules, and anything that changes *production behavior* beyond
  the record itself goes through the candidate pipeline, never through a
  direct rewrite.
- **Zep / Graphiti** (Rasmussen et al., arXiv:2501.13956) — temporal agent
  memory: facts carry validity windows and bitemporal annotation (when the
  fact was true, when the system learned it), so contradiction is handled by
  time rather than by deletion. Our validity interval and supersession chain
  are this idea as flat records instead of a graph database.
- **Mem0** (arXiv:2504.19413) — extraction pipelines with conflict detection
  and a user/session/agent scope hierarchy. The scope taxonomy and the
  conflict-detection-as-first-class-operation stance carry over; the
  auto-resolution does not (see conflict detection below).
- **Reflexion** (Shinn et al., NeurIPS 2023) and **CLIN** (Majumder et al.,
  COLM 2024) — agents that improve from verbal feedback persisted as memory.
  This is the correction loop's research lineage; the difference is that our
  corrections are attributed and promoted through governance instead of
  appended to a prompt.
- **Logged bandit feedback / off-policy evaluation** (Bottou et al., JMLR
  2013; Dudík, Langford & Li, ICML 2011; Swaminathan & Joachims, ICML 2015) —
  the counterfactual-evaluation literature: to compare a candidate policy
  against the one that logged the data, the log must carry the legal action
  set and the propensity of the action taken, recorded at decision time,
  never reconstructed. This is exactly what `DecisionEvent` froze in R0.5
  (`rusty-core/src/record.rs`): features, `legal_actions`, `selected`,
  `propensity`, `policy_version`, `outcome`. R0.5's contract freeze was
  written against this literature; R0.8 is where the contract starts being
  consumed.
- **Canary / shadow deployment** (release-engineering practice; the SRE
  workbook's canarying chapter is the reference shape) — a candidate serves a
  bounded fraction of traffic against a live baseline before full promotion.
  Our promotion envelope's review/canary branch is this, with the canary
  itself journaled.
- **Machine unlearning and the right to erasure** (Cao & Yang, IEEE S&P 2015;
  GDPR Art. 17) — forgetting as a systems problem: deletion must reach
  derived artifacts, not just the row. Our forgetting operation deletes
  records *and their dependents* (caches, dependent summaries) and journals a
  tombstone, rather than pretending a `DELETE` on one table is erasure.

## What Rusty does differently

Two things, both consequences of the sequencing rule the roadmap set in R0.5:
replay before learning.

1. **Evidence-native.** The learning contract was frozen before any learning
   shipped, so every journal written since R0.5 is already learnable
   evidence: effect classes, causal parentage, cost and latency, policy
   version pins in every `CheckpointHeader`, and (from this release)
   `DecisionEvent` emissions with propensity. Memory and learning do not
   introduce a parallel telemetry system; they consume the Flight Recorder
   the same way Durable Work and the Agent Fabric did.
2. **Replay-gated promotion.** A candidate never reaches production because a
   distiller scored it highly. It reaches production after exact/hybrid
   replay and `rusty-eval` experiments demonstrate it against recorded
   evaluations, inside a declared promotion envelope — and every stage
   transition (created, evaluated, promoted, canaried, rolled back) is a
   journaled `RunEvent`. The improvement is explainable afterward by walking
   ids: correction → candidate → evaluation report → promotion event → the
   later runs' journals. That walk is the release proof.

## Governed memory

### The record model (`rusty-core/src/memory.rs`)

One serde-versioned struct, `MemoryRecord`, additive-evolution only, golden
pinned. Every field exists because a downstream operation needs it:

- `memory_id` — content address: `sha256_hex` over the canonical
  serialization of the record's content plus provenance, the one hashing
  primitive shared with artifact references and journal heads. Immutable by
  construction: a changed record is a new id.
- `kind` — closed enum `MemoryKind`: `fact`, `preference`, `example`,
  `summary`. `example` is the correction loop's output (a corrected
  input/output pair); `summary` is consolidation's output and names its
  sources, which is what makes dependent-summary invalidation computable.
- `scope` — closed enum `MemoryScope`: `run` / `agent` / `team` / `user` /
  `tenant`, plus the concrete scope id (agent id, user id, ...). The
  roadmap's five scopes map onto R0.7's four `StateScope`s
  (`rusty-core/src/agents.rs`) with one honest adaptation: `StateScope` has
  no `run` member, because state outlives runs by design. Run scope is new
  here — memory whose lifetime is bound to one run's thread — and `agent`
  scope is `StateScope::Private` under its memory name. The taxonomy is a
  superset, not a fork: an agent manifest's declared `StateScope`s translate
  one-to-one into the memory scopes it may write.
- `provenance` — `MemoryProvenance`: who wrote it (`agent:{id}`,
  `human:{id}`, `distiller:{name}`, `system`), from what evidence (run id +
  journal event ids, a correction id, a candidate id, the source record ids
  of a consolidation), and when. A record that cannot name its origin cannot
  be audited, so provenance is mandatory, not optional.
- `confidence` — `f64` in `(0, 1]`, declared by the writer. A human
  correction defaults to `1.0`; a distilled record carries the distiller's
  estimate. Retrieval filters on it; nothing in the runtime *computes* it —
  honesty about confidence being a claim, not a measurement.
- `validity` — a `ValidityWindow { valid_from, valid_until }`: the interval
  the record claims to be true (Zep's validity window as a flat field),
  distinct from `created_at` (when the system learned it) — the bitemporal
  split, kept as two plain timestamps.
- `expires_at` — optional TTL. Expiration is a retrieval filter plus a
  forgetting trigger, not a silent reaper.
- `supersedes` — the memory id this record replaces, when it does.
  Supersession is a chain of immutable records; the superseded record is
  retained as evidence but filtered from default retrieval. There is no
  in-place update anywhere in the model.
- `content` — a `PayloadRef` (inline ≤ 4 KiB, content-addressed above, the
  journal's own discipline), so memory bodies share artifact storage and the
  large-body story needs nothing new.

### The write path

Writes are governed in three ways, all checked before any I/O:

1. **Scope authorization.** An agent may write only the scopes its
   `CapabilityManifest` declares (the `StateScope` check, extended one
   member); an undeclared scope write fails fast, the same shape as a write
   to an undeclared channel at the barrier. Run scope is written by the
   runtime on the run's behalf. Human corrections may target any scope but
   arrive as *candidates* past run scope (see the correction loop). Tenant
   scope is configuration-grade: writable by operators, not by agents, and
   tenant isolation is the v0.5 `{tenant}/` id-namespacing unchanged —
   another tenant's memories do not exist in your namespace, 404 never 403.
2. **Effect classification.** A memory write is an `Effect::Idempotent`
   effect under a derived key (`memory:{scope}:{memory_id}`): retried
   submissions converge, and the write is journaled with causal parentage
   into the writing run (`RunEventKind::MemoryWrite`, additive). A memory
   *read* is `Effect::ReadOnly` and journaled as `MemoryRead` — which is
   what makes candidate evaluation reproducible: exact replay serves the
   journaled retrieval instead of re-querying the store, per the rule the
   Flight Recorder already applies to journaled model and tool calls.
3. **No silent behavioral rewrites.** A memory write changes what future
   retrievals return — nothing else. If the write is meant to change a
   prompt, a policy, or a permission, it enters the candidate pipeline.

### Retrieval: structured filters + context budget

`MemoryQuery` is deliberately structural: scope, kind, key/tag equality,
validity-at-time, minimum confidence, exclude-expired, exclude-superseded,
authored-by. No similarity search — vector retrieval is deferred (see the
not-built list), and the consequences are stated plainly below.

Assembly for a prompt is **token-bounded**: `ContextBudget` (a max-token
figure plus an overflow policy) packs filtered records by a deterministic
rank — explicit priority, then confidence, then recency — until the budget
is exhausted, and the *assembly itself is journaled* (the record ids and
their order, as the `MemoryRead` event's output payload). Two properties
follow. Determinism: equal store state and equal budget produce byte-equal
assemblies, so a replayed run re-derives or re-serves the same prompt
content. Auditability: the prompt a model saw is reconstructable from the
journal, not from a store query re-run later against mutated state.

**Honest edges on deferred vectors.** Structured retrieval can answer "what
is current, scoped, attributed, and tagged" — which covers scoped facts,
preferences, and correction examples, the R0.8 use cases. It cannot answer
"what is semantically similar to this situation," and no amount of filtering
fakes that. The design consequence: writers must key and tag records
deliberately (the distiller's job), and consumers should treat absence of a
hit as absence of a key, not absence of a fact. The record model reserves an
additive `embedding` field so vector retrieval slots in without a wire
change when the roadmap's de-prioritization lifts; nothing else in this
release depends on it.

### Consolidation, conflict detection, forgetting — runtime operations with evidence

All three are operations over the store that journal what they did; none is
a background daemon with invisible effects.

- **Consolidation** distills N records into one `summary` record naming its
  sources in provenance, superseding them. It runs as a durable task (R0.6
  machinery: leased, retried, journaled), because consolidation over a large
  scope is exactly the kind of work that must survive a crash mid-pass.
- **Conflict detection** flags records that share a key and overlap in
  validity with contradictory content. It *flags* — it never auto-resolves.
  A conflict surfaces as a review item (or a correction candidate); Zep and
  Mem0 resolve contradictions inside the ingestion pipeline with an LLM,
  which is precisely the silent-mutation pattern the learning rule exists to
  forbid. Detection is evidence; resolution is governance.
- **Forgetting** is real deletion with a receipt: `forget(memory_id)` (or
  `forget_scope`, for erasure requests) removes the record, invalidates
  derived artifacts (retrieval caches, and — by walking `supersedes` in
  reverse — summaries that named the record as a source, which are
  re-derived or invalidated), and journals a **tombstone** (`RunEventKind::
  MemoryForget`): the id, the scope, the reason (`expired` / `retracted` /
  `erasure_request`), and the dependent invalidations — metadata, never the
  forgotten content. The unlearning lineage applies directly: erasing the
  row while leaving derived summaries and caches is not forgetting (Cao &
  Yang's point), and the tombstone is what makes the erasure auditable
  afterward. The boundary with journal immutability is drawn under open
  question 4: memory records are derived state and are erasable; run
  journals are the system's own hash-chained evidence and are not.

### Storage

Both server-store backends, per the established conventions
(`rusty-server/src/server_store.rs`): one JSON file per record under
`{store}/memory/` on the JSON backend (atomic temp-write-then-rename, the
one-writer-process precondition documented), and a `server_memory` table on
Postgres — column-mapped for the structured filters (scope columns, kind,
validity, expiration, supersession, confidence) the way `server_tasks` is
column-mapped for claiming, auto-migrated under the advisory lock. Content
bodies above the inline threshold spill to the R0.7 artifact store
(`FileArtifactStore` / `PostgresArtifactStore`) and dedupe by content
address. Retrieval at server scale runs against columns, not record scans;
the JSON backend scans, which is honest about its dev-scale role.

## The correction loop

A human correction is the highest-trust input the learning system has, and
the loop treats it accordingly: **a correction becomes an attributed
candidate memory or example — never an in-place rewrite of what it
corrects.**

The contract, `Correction` (in `memory.rs`, golden pinned): `correction_id`;
`author` (a human identity — attribution is mandatory, because a correction
that cannot name its corrector is indistinguishable from a prompt edit);
`target` (what it corrects: a journaled run event id, a memory id, or a
pinned prompt hash from the run manifest); the corrected content; the scope
the result should live at; and an optional rationale. Three rules:

1. **Attribution travels with the derived record.** The candidate
   `MemoryRecord` produced from a correction carries `provenance:
   human:{author} via correction:{id}` and `confidence` defaulting to 1.0.
   Every later consumer — distiller, evaluator, auditor — can trace the
   record to the person and the moment.
2. **Scope decides the path.** A correction at run scope is adopted directly
   (it affects only the run that produced it). A correction at agent scope
   or wider becomes a **candidate**: it is evaluated before promotion,
   because a wrong human correction at tenant scope is a production incident
   with a name attached, and the evaluation step is cheap insurance against
   it.
3. **Corrections enter evaluation as examples.** A correction whose target
   is a run event also yields an `example`-kind record — the input the run
   saw and the corrected behavior — which the distiller folds into a
   versioned `rusty-eval` dataset (a new dataset version, never an edit in
   place: datasets are canonical JSONL precisely so this diff is visible in
   version control). The candidate built from the correction is then
   evaluated against a dataset that *contains the failing case* — the
   correction is both the fix and the regression test.

**Boundary with the parallel feedback stream.** A parallel stream is
building human-feedback operations in `rusty-eval` (`feedback.rs`): capture
and normalization of human feedback — ratings, edits, thumbs — into
structured, versioned records in the eval plane. The boundary, drawn
explicitly: `rusty-eval` owns **capture and normalization** of feedback; it
does not write memory, and it does not promote anything. R0.8's correction
loop **consumes** feedback records as one input source (a `Correction` can
reference a feedback record id as its origin) and owns everything downstream:
attribution into the record model, candidacy, evaluation composition, and
promotion governance. No memory storage in `rusty-eval`; no promotion logic
in `rusty-eval`; no feedback collection endpoints in `rusty-core` beyond the
correction surface. The two streams meet at one serde-pinned record
reference, the same way `rusty-eval` already meets the runtime at the
`Journal`.

## The learning loop

Six stages, each a durable, journaled transition. The loop never runs inside
a production run; it runs between runs, over recorded evidence.

**Observe.** Completed runs' journals and their `rusty-eval` experiment
reports are the input. "Completed" is load-bearing: learning reads terminal
evidence, never in-flight state.

**Distill.** A distiller — application code, not runtime code (open question
2) — reads observations and produces a `Candidate` (`rusty-core/src/
learn.rs`): an immutable, versioned, content-addressed declaration of a
proposed change. Four candidate kinds, closed enum `CandidateKind`:

| Kind | What it carries | Production surface it would change |
|---|---|---|
| `prompt` | New prompt text (content-hashed, matching `RunManifest::pin_prompt`) | A prompt pin in the run manifest |
| `policy` | Executor policy parameters for one `DecisionFamily` | A `PolicyVersion` in the policy plane |
| `memory_set` | A set of `MemoryRecord`s (adds and supersessions) | Scoped memory content |
| `tool_permission` | A narrowed or widened tool grant | The tool surface a run may call |

Candidates are content-addressed (`sha256_hex` over canonical content), so
identity is integrity: two distillations of the same change converge on one
id, and a tampered candidate fails its own address. Creation is journaled
(`RunEventKind::CandidateCreated`) with the distiller's identity and the
evidence span it read.

**Evaluate.** Composition, not duplication: the candidate is evaluated with
the machinery that already exists. Exact/hybrid **replay** (`rusty-core/src/
replay.rs`) re-drives recorded runs with the candidate applied — replay
serves journaled effects, so the candidate's behavior is measured against
identical evidence with zero outbound calls; divergence detection is the
replay engine's own contract. `rusty-eval`'s `ExperimentRunner` runs the
candidate over the versioned dataset (which now includes the correction
examples) through the real executor, and `compare()` diffs the candidate
report against the baseline with `CompareThresholds`. The verdict, the
report pair, the dataset version, and the replay fixture ids are journaled
(`CandidateEvaluated`) — the evaluation is evidence, not a log line.

**Promote.** Promotion is gated by a **promotion envelope**: a declared,
per-deployment `PromotionEnvelope` naming, per candidate kind, what may
promote automatically (the evidence thresholds: `compare()` shows no
regression *and* improvement on the target metric, over the named dataset
version), and what requires review or a canary. The mechanism decision, and
its justification: promotion executes as an `Effect::Idempotent` effect
under the derived key `promotion:{candidate_id}` — retried promotions
converge, and recovery re-derives the same key — but when the candidate
falls *outside* the envelope, promotion requires a human
`ApprovalToken` (`rusty-core/src/effects.rs`) scoped to an effect id derived
over the candidate's content hash and target scope via `derive_effect_id`.
This composes the effect kernel rather than inventing an approval parallel
to it: the token's `approved_by` gives attribution, the scope check makes an
approval for one candidate non-transferable to another, and the honest edge
is inherited unchanged — the token is an in-process proof of explicit
decision, so the approval must be journaled to survive a restart (it is, on
the `CandidatePromoted` event), and cross-process attestation stays R0.9's
signed-receipt work. Inside the envelope, the envelope itself is the
standing approval, versioned and declared — not a silent default. A canary
promotion binds the candidate to a declared fraction of new runs (admission
picks by seeded draw, so a recorded run reproduces its assignment) with the
static version serving the rest.

**Monitor drift.** Post-promotion, the promoted version's runs are sampled
into scheduled experiments against the promotion-time dataset. Drift is
declared thresholds on journaled metrics — pass-rate drop, p95 latency
growth beyond the `compare()` thresholds — not statistical process control,
and it is honest about that: the monitor answers "is the promoted version
regressing against the evidence that promoted it," nothing deeper.

**Roll back.** Every promotion is a pointer move. The active version for
each surface (prompt name, policy scope, memory scope, tool grant) is an
immutable pointer to a `CandidateId`; rollback re-points to the previous
candidate and journals `CandidateRolledBack` with the drift evidence that
caused it. Because candidates are content-addressed and immutable, rollback
is exact: the restored version is byte-identical to the one that previously
served, not a reconstruction. New runs bind the re-pointed version at
admission; in-flight runs keep the version their checkpoint header pins —
the same conservatism as worker-version pinning and manifest pinning.

## Executor policy plane v1

The R0.5 contract already froze everything the plane needs
(`rusty-core/src/record.rs`): `PolicyVersion` (newtype, `STATIC_V0` as the
documented floor), `policy_version` pinned in every `CheckpointHeader`,
`DecisionFamily` and `DecisionAction` as closed Rust enums, and
`DecisionEvent` carrying features, `legal_actions`, `selected`, `propensity`
assigned at decision time, and `outcome`. What R0.8 adds is the plane
itself:

- **A policy registry.** Server-side, additive: `PolicyVersion` → immutable
  `ExecutorPolicy` (parameters per `DecisionFamily` — backoff caps, timeout
  bounds, concurrency limits). Immutable per version; a changed policy is a
  new version, the candidate pipeline's `policy` kind.
- **Epoch-bounded versions.** A policy version is active from its promotion
  until the next promotion (the epoch). New runs bind the active version at
  admission; in-flight runs keep the version their checkpoints pin. An epoch
  is thus a set of runs delimited by journaled promotions — replay of any
  run in the epoch reproduces that epoch's decisions because the version is
  pinned in the header.
- **Emission.** One honest gap the codebase states outright: the
  `DecisionEvent` docs say "v1 freezes this contract but the executor does
  not yet emit decision events." R0.8 wave 4 closes it, starting with
  `DecisionFamily::Retry` at the `classify_retry` decision point
  (`rusty-core/src/durable.rs`) — the decision the R0.6 design already
  earmarked as learning evidence — with features (error class, attempt,
  dependency latency), the closed legal set, and propensity from the active
  policy version.
- **The static floor.** `static-v0` — deterministic, no learning — remains
  the default and the floor: every candidate policy is evaluated against it,
  and revert-to-default is always a legal rollback. Closed action sets stay
  closed: a learned policy chooses among the `legal_actions` enum members,
  never a free-form output — that is what keeps this plane mechanical
  (dense signals, closed spaces), per the roadmap's mechanical-learning-first
  principle.

**The propensity caveat, stated plainly.** The off-policy literature the
contract was built against assumes a *stochastic* logging policy: inverse
propensity weighting needs support — the logged policy must assign nonzero
probability to actions the candidate would take. A deterministic floor logs
propensity `1.0` for the action it took and implicitly zero for everything
else, so propensity-weighted off-policy evaluation against `static-v0`
evidence degenerates to "we know what the floor did." R0.8's evaluation gate
is therefore **replay plus experiment comparison**, not importance
weighting: the candidate runs against the same recorded evidence and the
same datasets, and `compare()` answers the release question. Propensity
earns its keep the moment canary exploration exists — a canary that assigns
candidates by seeded draw is a stochastic policy with known propensities,
and the logged `DecisionEvent`s from canary traffic are well-posed
off-policy evidence for the *next* candidate. The contract was frozen early
so this would be true when needed; R0.8 does not pretend it is needed yet.

## Composition with the shipped systems

Four systems, one system seen from four sides:

- **Flight Recorder.** Memory reads and writes, candidate lifecycle
  transitions, corrections, and forgetting are journaled `RunEvent`s with
  causal parentage — seven additive `RunEventKind` variants (`MemoryRead`,
  `MemoryWrite`, `MemoryForget`, `CandidateCreated`, `CandidateEvaluated`,
  `CandidatePromoted`, `CandidateRolledBack`), the same evolution rule
  R0.6's `EffectReceipt` and R0.7's agent variants followed; old journals
  keep deserializing. Journaled retrieval is what lets exact replay serve a
  memory read; the journal's artifact discipline carries memory bodies; the
  `DecisionEvent` contract is the policy plane's evidence.
- **Durable Work.** Consolidation and distillation run as durable tasks —
  leased, retried under the `ErrorClass` taxonomy, dead-lettered with
  evidence, quota-counted per tenant. Promotion is an idempotent effect with
  a derived key; its journaled result plays the effect receipt's role for
  the learning plane. The outbox rule applies unchanged: a candidate state
  transition and the run evidence that caused it must not split-brain.
- **Agent Fabric.** Memory scopes are the `StateScope` taxonomy extended by
  one member; scope authorization is the manifest check; team memory lives
  at team scope under the same turn discipline that governs team state;
  supervision and coordination journals are distillation input (a
  crash-looping agent's attempt history is exactly the evidence a candidate
  should learn from). `TeamTrace` gives the distiller the team's causal tree
  to read.
- **rusty-eval.** The evaluation gate *is* `ExperimentRunner` + `compare()`
  + versioned datasets — composed, not re-implemented: the learning loop
  builds `PreparedRun`s with the candidate applied and calls the runner.
  `feedback.rs` (parallel stream) is a correction input source at the
  boundary drawn above. The `JudgeModel` seam stays the eval plane's: a
  semantic judge for candidate evaluation plugs in through
  `ExperimentConfig::with_judge`, not through anything new here.

## What R0.8 deliberately does NOT build

- **No vector retrieval.** The roadmap de-prioritizes a generic vector
  abstraction; structured retrieval with a context budget covers the scoped,
  attributed use cases this release exists for, and the `embedding` field is
  reserved so vectors arrive additively. The cost is stated above: no fuzzy
  semantic recall, and writers must key records deliberately.
- **No online learning.** Nothing updates a parameter, prompt, or weight
  inside a live run. Learning happens between runs, over terminal evidence,
  through the candidate pipeline. An in-run "learning" write is a governed
  memory write — scoped, journaled, and inert beyond retrieval.
- **No self-modifying graphs.** Graph topology is code, pinned by
  `graph_hash` in the checkpoint header; no candidate kind touches it. The
  roadmap's de-prioritized "open-ended self-modification" stays
  de-prioritized.
- **No cross-tenant learning.** Tenant isolation is id-namespacing; memory
  scopes top out at `tenant`; distillation reads one tenant's evidence. A
  cross-tenant distiller would be a tenant-isolation breach wearing a
  learning costume.
- **No learned agent/model selection.** The roadmap names it a governed
  semantic policy, not an automatic one; R0.8's plane covers the mechanical
  families in `DecisionFamily` only.
- **No model-weight training.** Unchanged from the roadmap's de-priorities.

## Wave plan and release proof

**Wave 1 — memory contracts and store.** `rusty-core/src/memory.rs`
(`MemoryRecord`, `MemoryKind`, `MemoryScope`, `MemoryProvenance`,
`ValidityWindow`, `MemoryQuery`, `ContextBudget`) with golden files; both
store backends; journaled reads/writes; token-bounded deterministic
assembly. Exit: memory survives a server restart on both backends, and an
exact replay of a memory-reading run serves the journaled assembly
byte-identically.

> **Wave 1 status: implemented.** The contracts and goldens landed as
> written (`MemoryRecord` / `MemoryKind` / `MemoryScope` /
> `MemoryProvenance` / `ValidityWindow` / `MemoryQuery` / `ContextBudget`,
> plus the journaled seam `JournaledMemory` with `MemoryReplaySource`
> serving exact replay), with two additive refinements: `MemoryRecord`
> carries an explicit `priority` field (the design's rank input needed a
> home), and only the two variants this wave wires — `memory_read` /
> `memory_write` — joined `RunEventKind` (the remaining five land with
> their waves). The server surface is `POST /memory` (scope-authorization
> gates at the write: run scope runtime-only, agent scope
> manifest-checked, tenant scope self-only), `GET /memory/{id}`, and
> `POST /memory/query` (structured filters plus the token-bounded
> `MemoryAssembly`), on both store backends — one JSON file per record
> under `{store_path}/memory/` with artifact-spilled bodies re-inlined on
> read, and the column-mapped `server_memory` table on Postgres. Both exit
> criteria are automated tests: restart survival on both backends
> (`rusty-server/tests/memory.rs`, JSON + gated Postgres) and the
> byte-identical exact-replay proof
> (`rusty-core/tests/memory.rs::exact_replay_serves_the_journaled_assembly_byte_identically`).

**Wave 2 — correction loop and memory operations.** The `Correction`
contract and endpoint, attributed candidate derivation, consolidation /
conflict detection / forgetting as durable operations with tombstones.
Exit: a correction at agent scope produces an attributed candidate memory
and a dataset example; `forget` removes the record and invalidates its
dependent summaries, with the tombstone journaled and the store verified
clean by query.

**Wave 3 — candidates and the promotion gate.** `rusty-core/src/learn.rs`
(`CandidateKind`, `Candidate`, `PromotionEnvelope`, the active-version
pointer), the evaluation composition (replay + `ExperimentRunner` +
`compare()`), envelope-gated promotion with `ApprovalToken` for
out-of-envelope candidates, canary binding by seeded draw, rollback by
pointer. Exit: an out-of-envelope promotion without an approval is refused;
a promoted candidate rolls back byte-exactly; every transition is in the
journal.

**Wave 4 — policy plane v1 and the release proof.** The policy registry,
epoch binding at admission, `DecisionEvent` emission at `classify_retry`,
static-floor enforcement. Exit: the release proof below.

**Release proof (the whole release).** The roadmap's sentence, automated as
an integration test in the crash-recovery family
(`rusty-server/tests/learn_release.rs`): *apply a correction, evaluate the
derived candidate, promote it, and explain the later improvement —
attributable and reversible.* Concretely: a scripted agent with a planted
behavioral defect (the live-demo calculator's class of bug: a malformed tool
argument the agent mishandles) runs and its journal is recorded. The test
applies a human correction through the correction endpoint; asserts the
derived candidate memory and dataset example carry the author's attribution;
runs the evaluation — replay against the recorded run plus an experiment
over the corrected dataset version — and asserts the `compare()` verdict
shows improvement on the corrected case with no regression; promotes inside
the envelope; then runs new traffic and asserts the corrected behavior in
the new runs' journals. The explanation is asserted, not narrated: from the
improved run's journal, walking ids reaches the promotion event, the
evaluation reports, the candidate, and the correction — one attributable
chain. Finally the test rolls back by pointer and asserts the defect
behavior returns, byte-exact. Attributable and reversible, as a test.

## Open questions

Flagged for the owner before wave 1 lands:

1. **Evaluation without support.** The deterministic floor makes
   propensity-weighted off-policy evaluation degenerate, so the gate is
   replay + experiment comparison — but replay comparison is only as strong
   as the recorded evidence's coverage of the cases the candidate changes.
   Leaning: accept it; the correction loop's rule 3 (the correction becomes
   the regression test) is the coverage mechanism, and propensity-weighted
   evaluation is deferred until canary traffic produces stochastic logs.
2. **Where distillation lives.** Distillation semantics are
   application-specific (what counts as a pattern worth a prompt change is a
   product decision), but an unguided distiller makes the loop theoretical.
   Leaning: the runtime owns the candidate contract, storage, gates, and
   journaling; applications own distillers; R0.8 ships exactly one reference
   distiller — correction → candidate — which the release proof exercises.
3. **Token accounting for the context budget.** Provider-precise tokenizers
   differ per model and are heavyweight. Leaning: the budget is enforced in
   estimated tokens (bytes ÷ 4) with a declared safety margin, recorded as
   such on the assembly; model-precise counting plugs in later behind the
   same `ContextBudget` type.
4. **Forgetting vs journal immutability.** Erasure requests target memory
   records and derived artifacts; run journals are hash-chained evidence and
   are not rewritten — a journal may still contain the content a memory
   derived from. Leaning: document the boundary as designed (derived state
   is erasable, evidence is not, tombstones carry the erasure receipt) and
   treat journal-level erasure as R0.9+ compliance work, stated plainly
   rather than smoothed over.
5. **Supersession discipline.** A same-key correction-sourced write
   auto-supersedes (attributed, high-confidence); a distiller-sourced
   conflict produces a review item, never an auto-resolution. Leaning: this
   asymmetry — corrections are trusted because they are attributed;
   distillations are not because they are inferred.
6. **Envelope defaults.** What may auto-promote with zero human approval in
   R0.8? Leaning: `memory_set` candidates at run and agent scope with a
   clean `compare()` verdict; `prompt`, `policy`, and `tool_permission`
   candidates always require an `ApprovalToken` this release — the envelope
   widens only with operational evidence that the gate is sound.
