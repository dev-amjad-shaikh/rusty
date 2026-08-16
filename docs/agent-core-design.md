# Agent Core design (R0.13)

R0.13 turns the shipped governed primitives into a self-learning agent core:
organized memory, engineered context, selected tools, and governed skills —
each improvable from run evidence through the candidate pipeline. The
governing claim, stated precisely: **an agent's context — what memory it
recalls, what history it carries, which tools and skills it is shown — is the
highest-leverage ungoverned surface left in the runtime, and it is learnable
the same way everything since R0.5 has been learnable: deterministic
assembly over journaled evidence, immutable candidates, gated promotion,
byte-exact rollback.** The R0.8 learning rule holds without exception:
nothing self-rewrites. Every improvement — a memory ranking, a context
budget split, a tool shortlist policy, a distilled skill — is a candidate
that passes evaluation before promotion, and every promotion is a journaled
pointer move.

The release has five parts, each composable on its own: **memory
organization** (tiers, namespaces, and indexing over the shipped record
model), **memory optimization** (consolidation scheduling, dedup, and a
derived utility signal that re-ranks retrieval from evidence), **context
engineering** (a first-class, deterministic in-run assembly pipeline with
sectioned budgets and mid-run history compaction), **tool selection and
calling optimization** (a manifest-and-shortlist layer above `ToolRegistry`,
outcome learning, argument validation), and **skills** (versioned packs
selected into context, with the flagship loop: successful trajectories
distilled into candidate skills, evaluated, promoted). Everything stands on
machinery that exists — `memory.rs`, `learn.rs`, `registry.rs`, `skill.rs`,
the twin, `rusty-eval` — and the wave plan names exactly what is new.

## Why this belongs in the runtime

Context assembly built at framework level — string concatenation in a prompt
template, a `messages` list trimmed by hand, a tool list passed wholesale to
the model — loses the same three things framework-level memory lost before
R0.8. **Evidence** is absent: nothing records which records, which history
span, which tool schemas the model actually saw, so "why did the agent do
that" is unanswerable one deploy later. **Determinism** is accidental:
`HashMap` iteration order and wall-clock reads make two runs over identical
state produce different prompts, and exact replay has nothing stable to
serve. **Improvement is silent**: a re-ranked retrieval or a rewritten
system prompt ships because someone edited a template, with no candidate, no
comparison against recorded evidence, no way back.

The runtime already holds every piece needed to do better. The journaled
`ModelCall` event's input *is* the assembled context — the Flight Recorder
froze that contract in R0.5, so a deterministic assembler gets auditability
and replay-serving for free, with no new event kinds. `memory.rs` shipped
the governed record store and the deterministic `assemble()` rank.
`learn.rs` shipped the candidate/promotion gate. `registry.rs` turned that
gate toward versioned configuration. `skill.rs` shipped content-addressed,
scanned, progressively-disclosed skill packages. R0.13 is the release where
those stops being adjacent planes and become one agent core.

## What R0.13 builds on, named

- **Governed memory** (`memory.rs`) — `MemoryRecord` / `MemoryScope` /
  `MemoryQuery` / `ContextBudget` / `assemble()` / `JournaledMemory`;
  consolidation, conflict detection, and forgetting as journaled operations.
  Unmodified: R0.13 composes it.
- **The candidate pipeline** (`learn.rs`) — `Candidate`, `CandidateKind`,
  `PromotionEnvelope`, `admit_promotion`, `VersionPointer`, canary by seeded
  draw. One additive extension, flagged under coordination: a `Skill`
  candidate kind.
- **The configuration registry** (`registry.rs`) — named artifacts indexing
  candidates, environment-tagged pointers, admission resolution. Context
  policies, consolidation policies, and tool-selection policies are registry
  artifacts of the existing `memory_config` / `tool_contract` families —
  no new configuration machinery.
- **The skill plane** (`skill.rs`) — `SkillPackage` validation,
  `scan_package`, content-addressed `SkillVersion`s, the append-only
  `SkillRegistry` with its forward-only latest pointer and three disclosure
  tiers. R0.13 adds selection and governed promotion around it; the package
  format and registry invariants are consumed unchanged.
- **The tool plane** (`tool.rs`) — `Tool`, `ToolRegistry` (with
  `restricted_to`), `ToolExecutor` (parallel dispatch, order preservation,
  failure isolation, middleware, effect admission). **Claimed by another
  stream: consumed, never modified.**
- **The prebuilt ReAct agent** (`react.rs`) — claimed likewise. R0.13's
  consumption recipe is pure composition (below).
- **The twin and `rusty-eval`** — evaluation composition for candidates.
  The twin's honest edge applies unchanged: decisions that change model
  *inputs* (a new skill body, a different memory ranking) are unevaluable in
  the twin; they evaluate through replay divergence plus experiment
  comparison over recorded datasets, exactly as R0.8's semantic surfaces do.
- **The journal** — no new `RunEventKind` variants this release
  (`record.rs` is claimed). Section "Evidence without new event kinds"
  states how each new surface journals through existing kinds.

## Memory organization

### Tiers as a retrieval-policy overlay, not new storage

The shipped scope taxonomy (`run` / `agent` / `team` / `user` / `tenant`)
already answers *whose memory it is*. The tier vocabulary answers a
different question — *how long it lives and how it gets into context* — and
R0.13 implements tiers as an overlay on scopes and kinds, not as new tables:

| Tier | Content | Scope/kind mapping | Lifecycle |
|---|---|---|---|
| **Working** | The run's own scratch: intermediate findings, plans, partial results | `run` scope, `fact`/`summary` kinds; written by the runtime on the run's behalf | Expires with the thread; never consolidated upward directly |
| **Episodic** | What happened: episode summaries distilled from completed runs' journals | `agent`/`team` scope, `summary` kind naming its source records and the run id in evidence | Consolidated on schedule (below); superseded, never edited |
| **Semantic** | What is true: facts, preferences, correction examples | `user`/`tenant`/`agent` scope, `fact`/`preference`/`example` kinds | Supersession chains; validity windows; forgetting only through the tombstoned operation |

Two rules keep the overlay honest. **Promotion between tiers is
consolidation**: working records do not "graduate"; a consolidation task
distills an episode summary from a completed run (terminal evidence only,
the observe-stage rule), and a later consolidation distills semantic records
from episodes. Each hop names its sources, so forgetting walks the chain —
the shipped `plan_forget` transitive invalidation already computes exactly
this. **Tiers shape assembly, not storage**: the context pipeline (below)
assigns each tier a section with its own budget, so "working memory is
always in context, episodic on structural match, semantic on key/tag match"
is assembly policy — versioned, promotable, rollable-back — not store
behavior.

### Namespaces and indexing

Keys and tags are the retrieval contract while vectors stay deferred, so
R0.13 gives them a grammar instead of leaving them to convention:

- **Hierarchical keys** — `domain.name` (e.g. `user.timezone`,
  `tool.search.quirks`), validated at the write gate against a declared
  pattern; the domain segment is what consolidation policies and scoped
  retention rules target ("consolidate `episode.*` at agent scope weekly").
- **Namespace = scope address + key domain.** Tenant isolation is unchanged
  (`{tenant}/` id-namespacing, 404 never 403). Agents share memory only by
  writing a wider scope their manifest declares — the shipped scope
  authorization is the whole sharing model, and R0.13 adds nothing to it.
- **Indexing strategy** — the Postgres backend is already column-mapped for
  the structured filters; R0.13 adds one derived index (the utility index,
  below) and one inverted tag index on the server store, both *derived and
  rebuildable*: the record store stays the source of truth, indexes are
  disposable projections, and a rebuild from records must reproduce them
  byte-identically (the checkpoint/artifact discipline applied to indexes).

### The vector decision, argued

**Vector/embedding retrieval stays deferred in R0.13.** The roadmap
de-prioritizes a generic vector-database abstraction, and nothing here
smuggles one back in — but the deferred question deserves a precise answer,
because "no fuzzy semantic recall" is the shipped design's own stated cost.

The argument has three legs. First, **evidence before mechanism**: R0.13's
memory-native learning (the utility signal below) attacks recall quality
with a signal the journals already contain — which records actually appeared
in successful runs — without an embedding model, an embedding journal
contract, or an index to govern. The R0.10 discipline applies: measure
headroom before buying machinery. If utility re-ranking closes the measured
recall gap on recorded workloads, vectors have not earned their keep; if a
published gap remains, that measurement *is* the case for the embedding
field. Second, **the contract is already reserved**: `MemoryRecord.embedding`
is an additive field from R0.8, so a later similarity index lands without a
wire change — deferral costs nothing structurally. Third, **an embedding is
a model call**: the day embeddings arrive, they journal as `ModelCall`
effects with the replay/determinism rules that implies, and the similarity
index becomes one more derived, rebuildable projection — not a database the
runtime depends on. Wave 2 ships the measurement that will decide this; the
design deliberately does not pre-commit.

## Memory optimization

### Consolidation scheduling

Consolidation exists as a durable task; what does not exist is *when* it
runs. R0.13 makes scheduling declarative: a **`ConsolidationPolicy`** — per
scope and key domain, trigger thresholds (record count, aggregate token
footprint, age of oldest unconsolidated record), and the distiller to invoke
— stored as a registry artifact of the existing `memory_config` family, so
changing the schedule is a governed, promotable, environment-tagged change
rather than an operator edit. The scheduler itself is the server's cron
machinery evaluating the policy's thresholds against store statistics; each
triggered consolidation is the shipped journaled durable task. A policy
change never alters a record — it alters *when distillation is proposed* —
so the gate math is unchanged.

### Compression of aged memories

Consolidation's summary *is* the compression path: N records → one `summary`
record naming its sources, superseding them in default retrieval. R0.13 adds
the aging rule that feeds it — records past a policy-declared age with
utility below a declared floor become consolidation *inputs* first and
forgetting candidates only after their summary exists — so compression
always precedes erasure, and erasure always carries the tombstone. The
distiller (an LLM call, journaled as an ordinary `ModelCall` inside the
consolidation task's run) is application code; the runtime owns the record
invariants, exactly as R0.8 drew the boundary.

### Dedup

Content-addressing already dedupes byte-identical content under identical
provenance. The remaining duplicate is the *same key, near-same claim* —
which the shipped conflict detector flags when content contradicts. R0.13
adds the benign half at the write gate: same scope, same key,
content-equal-up-to-normalization → the write converges onto the existing
record's id (idempotent-effect convergence does this mechanically for
retried submissions; the gate extends it to independent submissions).
Same key, *different* content is not dedup — it is supersession (an
attributed correction) or a flagged conflict (an inferred one), the R0.8
open-question-5 asymmetry unchanged.

### The utility signal (memory-native learning)

The one genuinely new derivation in this section: **which memories proved
useful in successful runs.** The evidence is already journaled — every
`MemoryRead` carries the assembly's record ids; every run carries a terminal
status and, where `rusty-eval` graded it, a score. The **utility index** is
a derived projection: per memory id, the count of successful-run assemblies
it appeared in, the count of failed-run assemblies, and a smoothed success
rate, recomputed by a durable task over completed journals. Three
disciplines keep it honest:

1. **Derived, never stored on the record.** `MemoryRecord` is immutable;
   utility lives in the index, rebuilt from journals byte-identically. A
   record's content address never changes because a statistic moved.
2. **Assembly consumes a journaled snapshot.** Live assembly reads the
   utility index as of a stamped instant; the resolved rank inputs travel in
   the journaled `MemoryRead` request, so exact replay serves the recorded
   assembly byte-identically and the determinism contract is the shipped
   one. Utility is a *rank input*, applied by the assembly driver alongside
   priority/confidence/recency — the shipped `assemble()` total order stays
   the tie-breaking floor beneath it.
3. **The weights are configuration.** How much utility moves rank is a
   `memory_config` artifact — promoted through the candidate gate with
   replay + experiment evidence, rolled back by pointer. The floor
   (utility weight zero, the shipped rank) is the `static-v0` of retrieval:
   always legal, always the baseline every candidate is measured against.

The utility signal is also the distillation input for memory hygiene:
records with sustained zero successful-use and expired validity become
forgetting *candidates* — flagged for review or folded into a `memory_set`
candidate, never silently reaped. Forgetting stays a journaled, tombstoned
operation; no policy auto-deletes.

## Context engineering

### The assembly pipeline (`context.rs`, new module)

The core of the release: a first-class, deterministic, in-run context
assembly pipeline that turns the prompt from string concatenation into a
governed artifact. One type, `ContextPipeline`, driven by a versioned
**`ContextPolicy`** (a registry artifact, environment-tagged, promotable):

```text
┌────────────────────────────────────────────────────────────────┐
│ ContextPolicy (versioned artifact)                             │
│  sections: [identity, task, skills, tools, memory, history]    │
│  per-section: { budget_tokens, freshness, overflow }           │
│  ordering + tokenizer seam + compaction trigger                │
└────────────────────────────────────────────────────────────────┘
        │
        ▼  assemble(run_state, stores, policy) → ContextAssembly
┌────────────────────────────────────────────────────────────────┐
│ ContextAssembly — the exact message list handed to ChatModel   │
│  + section manifest: what each section carried, ids, tokens    │
└────────────────────────────────────────────────────────────────┘
```

Sections, in canonical order, each a producer behind one trait:

| Section | Content | Freshness rule |
|---|---|---|
| `identity` | System prompt, agent manifest summary | Pinned by the run manifest; resolved at admission through the registry, never re-read mid-run |
| `task` | The current task/instruction, run goals | Set at admission or by explicit task update; stable within a super-step |
| `skills` | Tier-1 metadata of shortlisted skills; tier-2 bodies of *selected* skills | Re-resolved per assembly from the pinned skill versions (below) |
| `tools` | The shortlisted tool manifests (names + descriptions + schemas) | Re-shortlisted per assembly under the pinned selection policy |
| `memory` | Tier-ordered governed recall: working → episodic → semantic | Queried per assembly through `JournaledMemory`; `as_of` stamped by the run's clock (the shipped rule) |
| `history` | The conversation prefix, compacted when triggered (below) | The `messages` channel, verbatim up to the compaction watermark |

The pipeline's invariants are the release's contract:

- **Determinism is structural.** Equal inputs and equal policy produce a
  byte-equal `ContextAssembly`. Section producers are pure functions over
  their inputs; ordering is declared, not incidental; every clock read goes
  through the run's `Clock`. This is what lets exact replay serve the
  journaled model call — and it is testable directly (Wave 1's proof).
- **Budgets compose.** The pipeline takes one `ContextBudget` (the shipped
  type, estimated-token accounting with the declared margin) and splits it
  across sections by policy weights; a section that overflows applies its
  own overflow rule (truncate for memory/history, fail for identity — a
  system prompt that does not fit is a configuration error, not a
  truncation).
- **The assembly is the journal payload.** No new event kind: the assembled
  messages *are* the `ModelCall` input the Flight Recorder has journaled
  since R0.5, and the section manifest rides inside it as a reserved
  metadata message (or the request's existing extra field). The prompt a
  model saw is reconstructable from the journal — the R0.8 auditability
  argument, now covering the whole context, not just the memory section.

### Mid-run history compaction

Long runs drown their own history. The shipped answer — nothing; the
`messages` channel grows — is honest but bounded by the model's window.
R0.13's compaction is a **pipeline operation, not a state mutation**: when
the history section exceeds its compaction trigger, the pipeline issues a
summarization model call (an ordinary journaled `ModelCall`) over the oldest
span and substitutes the summary in the *assembled* history section. The
`messages` channel itself is untouched — the journal and checkpoints keep
the verbatim history as evidence, which is what makes the compaction
revisable: a later evaluation can re-assemble with a different trigger and
compare. Compaction carries a watermark (the compacted prefix's event span)
recorded in the section manifest, so an auditor reads exactly which messages
the model stopped seeing and what summary replaced them. ReAct consumes this
unchanged: the compaction happens inside the model wrapper (below), never in
`react.rs`.

### Token accounting: the precise seam, the estimate as floor

The shipped accounting (bytes ÷ 4 plus margin) stays the floor and the
default. R0.13 adds the seam the R0.8 design anticipated: a
`TokenCounter` trait — `count(&[ChatMessage], model_id) -> u32` — with the
estimate as the built-in implementation and provider-precise tokenizers
pluggable per model id. Three rules keep it honest. The policy pins which
counter applies, so assembly stays deterministic under a pinned policy. The
`TokenAccounting` payload records which counter ran, so an auditor reads the
accounting the assembly actually applied. And a provider counter is a
*counting* dependency, not a call: it must be local and pure (a bundled
tokenizer table), because an assembly that calls out to count itself is a
replay hazard the design refuses.

## Tool selection and calling optimization

### The selection layer (`tool_select.rs`, new module)

A layer strictly above `ToolRegistry` — the registry stays the executable
truth; the selection layer decides what the model is *shown*. One new
artifact, the **`ToolManifest`**: selection metadata for one tool, derived
from the shipped `ToolCapability` (name, description, schema, effect class —
derived, never separately authored, the `tool.rs` rule) plus an
operator-governed overlay: capability tags, a when-to-use note, a cost/latency
class, `parallel_safe` and `batchable` flags, and prerequisite tools. The
overlay is a registry artifact of the existing `tool_contract` family —
governed, versioned, promotable — so selection metadata changes move through
the same gate as everything else.

**Ranked shortlisting** engages when the registry grows past a declared
cutoff: `select(features, manifests, k)` scores manifests structurally —
tag overlap with the task section, prerequisite closure, effect-class
compatibility with the run's budget ceiling, the tool's journaled outcome
statistics (below) — and returns a deterministic top-k with the full
ranking recorded in the section manifest. Structural, not semantic: no
embeddings, per the vector decision, and the features are the assembly's own
declared sections, journaled with the assembly.

**Call outcome learning** derives from evidence that already exists: every
journaled `ToolCall` carries the tool, the arguments, the outcome, the
latency, and the error class. A durable task rolls these into per-tool (and
per argument-pattern-digest) success rates, validation-failure rates, and
latency percentiles — the same derived-index discipline as memory utility.
Selection consumes the snapshot as one rank input; argument-repair learning
consumes the failure half (below).

**Batching and parallelization policy.** `ToolExecutor` already dispatches a
model-emitted batch in parallel with order stability and failure isolation;
that is its contract and this release does not touch it. What R0.13 adds is
the *policy metadata* the executor's caller needs to batch well:
`parallel_safe` (the tool declares concurrent calls safe — default false for
`NonIdempotent`, the shipped conservatism), `batchable` (N calls of this
tool collapse into one — a tool-declared capability, honored by the
dispatching node), and per-tool concurrency hints bounded above by the run's
budgets and pool quotas, which a policy may only narrow, never widen (the
R0.10 rule). The ReAct consumption of a partitioned batch is composition —
a middleware layer may reject or rewrite calls, and the tools node's own
code is application-side — not an executor change.

**Argument validation and repair.** A `ValidatingTool` wrapper — pure
composition over `Arc<dyn Tool>` — validates arguments against the tool's
JSON schema *before* dispatch and, on failure, returns a structured
`ERROR:` payload naming the violations instead of calling the tool. The
repair loop is then the model's own next iteration through the shipped
failure-isolation channel: it observes the violations and re-issues the
call. No silent coercion (the v0.5 calculator bug's lesson: quoted numerics
silently computing `0 op 0` is the failure mode this wrapper exists to make
loud); repair hints are declarative schema metadata, not guesswork.
Validation failures journal as ordinary `ToolCall` events with the violation
payload, which is exactly the evidence the outcome-learning roll-up reads —
and the evidence a distiller turns into correction examples, prompt
candidates, or skill candidates when a tool's failure pattern repeats.

### Consuming it from ReAct without touching `react.rs`

Four composition seams, all shipped, no claimed files modified:

1. **Construction-time narrowing** — the shortlist at admission builds the
   run's `ToolRegistry` via the shipped `restricted_to`; the executor's
   `TOOL_ALLOWLIST_KEY` mechanism already narrows model-visible schemas and
   dispatch per run.
2. **A `ChatModel` wrapper** — `AssemblingChatModel` runs the context
   pipeline (sections, budgets, compaction, tool shortlist) and then calls
   the inner model, exactly the pattern `RecordingChatModel` already
   establishes. `create_react_agent(model, tools)` receives the wrapper;
   `react.rs` never knows.
3. **Tool wrappers** — `ValidatingTool` (and any call-policy wrapper) wrap
   tools before registration; `ToolRegistry` holds `Arc<dyn Tool>`, so
   wrapped tools are indistinguishable from native ones.
4. **Middleware** — the shipped `MiddlewareChain` tool hooks can rewrite or
   reject calls (argument-policy enforcement), and model hooks wrap the
   same seam the assembler uses.

## Skills

### Packs, bindings, and the registry question

A skill is the shipped `skill.rs` package — versioned, content-addressed,
scanned, progressively disclosed. R0.13 adds what the package plane
deliberately left out (its module docs say the run-integration slice owns
it): **selection and governed activation**.

- **`SkillBinding`** (new, in `skills.rs`) — the run-facing half of a skill:
  when-to-use metadata beyond the description (trigger tags matched
  structurally against the task section, task-shape notes, cost class) and
  tool bindings made *enforceable*: the frontmatter's advisory
  `allowed-tools` becomes a declared tool set that narrows the run's
  allowlist while the skill is active, through the shipped
  `restricted_to`/allowlist mechanism. A skill can only narrow the run's
  tools, never widen them — capabilities-over-trust applied to context.
- **Registry composition, answered.** The R0.11 configuration registry is
  *not* the skill store — `SkillRegistry` already is, with package-native
  concerns (scan reports, disclosure tiers, member hygiene) the config
  registry has no business holding. What R0.11's machinery *does* supply is
  the governed activation: a skill's **active version per environment is a
  learn-plane `VersionPointer`** over the surface `skill:{name}`, moved by
  candidate promotion, bound at admission through the same pointer-admission
  rule prompts use. `SkillRegistry`'s forward-only latest pointer remains
  authorship history; the learn pointer is the production surface. Two
  pointers, two jobs, one content-addressed version set they both name.
- **Selection during assembly** — the skills section of the pipeline
  shortlists tier-1 metadata structurally (trigger-tag overlap, declared
  tool availability after narrowing) and loads tier-2 bodies only for the
  selected few, budgeted like every other section. Selection is
  deterministic under the pinned policy; the selected name/revision/content
  hash list is in the section manifest, so the journal answers "which skill
  text shaped this run" without a new event kind — the skill bodies are in
  the journaled `ModelCall` input, and the identifiers are in its manifest
  message.

### The flagship loop: trajectories into governed skills

The self-learning story, end to end: **a successful trajectory — or a
corrected failure — distills into a candidate skill; the candidate is
evaluated against recorded runs; promotion moves the version pointer; new
runs assemble with the promoted skill.**

1. **Evidence.** Completed runs' journals (successful ones carry the
   trajectory worth distilling; corrected ones carry the correction loop's
   attributed examples, R0.8).
2. **Distillation.** A distiller — application code, the R0.8 boundary —
   reads trajectories and drafts a `SKILL.md` package. The composer plane
   (claimed stream) owns agent-facing drafting tools; this loop's distiller
   runs *between* runs over terminal evidence and constructs the package
   directly, validating through the skill plane's own fail-closed parser and
   scanner. A draft that fails validation never becomes a candidate.
3. **Candidacy.** The validated package becomes a candidate — one additive
   `CandidateKind::Skill` carrying `{ name, content_hash, binding }`, the
   content hash being the skill plane's own address, so candidate identity
   and package identity are one digest. Creation, evaluation, promotion,
   and rollback journal through the *existing* four candidate lifecycle
   event kinds; nothing new enters `record.rs`.
4. **Evaluation.** Semantic surface, so the twin's honest edge applies: a
   skill changes model inputs, which the recorded world cannot answer. The
   gate is the R0.8 semantic path — replay divergence on recorded runs plus
   an experiment over the versioned dataset (which the correction loop
   already keeps current) through `rusty-eval`'s runner and `compare()`,
   wired through the shipped `CandidateEvaluator` seam.
5. **Promotion.** The envelope holds `skill` candidates at `Approval` by
   default — a wrong skill steers every run that selects it, the semantic
   blast radius R0.8 already priced for prompts. Promotion registers the
   package revision in `SkillRegistry` (idempotent re-registration if the
   distiller pre-registered it) and moves the `skill:{name}` pointer; new
   runs bind the promoted version at admission; in-flight runs keep what
   they pinned.
6. **Rollback** — the pointer re-points; the restored skill is the
   byte-identical previous package. The registry keeps every revision; the
   pointer chooses among them.

## The self-learning loop, end to end

```mermaid
flowchart LR
    subgraph Run evidence
        J[run journals<br/>ModelCall · ToolCall<br/>MemoryRead · MemoryWrite]
        E[rusty-eval scores<br/>+ terminal status]
    end
    subgraph Derivation — derived, rebuildable
        U[utility index<br/>memory usefulness]
        T[tool outcome stats<br/>success · latency · arg failures]
    end
    subgraph Distillation — application code
        D1[memory policy<br/>candidate]
        D2[context policy<br/>candidate]
        D3[tool manifest /<br/>selection candidate]
        D4[skill candidate<br/>from trajectories]
    end
    subgraph Gate — learn.rs, unchanged
        EV[evaluate: replay divergence<br/>+ twin where mechanical<br/>+ experiment compare]
        EN[promotion envelope<br/>+ scoped ApprovalToken]
        VP[VersionPointer move]
    end
    J --> U & T
    J --> D1 & D2 & D3 & D4
    E --> D1 & D4
    U --> D1
    T --> D3
    D1 & D2 & D3 & D4 --> EV --> EN --> VP
    VP -->|bound at admission| RUN[improved run<br/>deterministic assembly]
    RUN --> J
```

What is **new** in R0.13, exhaustively: the `context.rs` pipeline
(`ContextPipeline`, `ContextPolicy`, `ContextAssembly`, `TokenCounter`
seam, compaction), `tool_select.rs` (`ToolManifest`, `select`,
`ValidatingTool`, outcome roll-up), `skills.rs` (`SkillBinding`, skill
shortlisting), the memory utility index and consolidation policies
(`memory_tiers.rs`), one additive `CandidateKind::Skill`, and the reference
distillers. Everything else — candidates, envelopes, pointers, replay, the
twin, eval composition, journaled memory, skill packages — is shipped
machinery consumed as designed.

### Evidence without new event kinds

`record.rs` is claimed this wave, so the discipline is stated once, plainly:
context assemblies and skill selections journal **inside the journaled
`ModelCall` request** (the assembled messages plus a manifest message
carrying section budgets, record ids, tool shortlist, skill name/revision/
hash pins) — the request hash therefore covers the whole context, and exact
replay serves it unchanged. Memory operations journal through the shipped
`MemoryRead`/`MemoryWrite`/`MemoryForget` kinds; tool calls through
`ToolCall`; candidate lifecycle through the four existing candidate kinds;
policy decisions through `PolicyDecision`. The honest edge: assembly
*determinism* is proven by test (equal inputs → byte-equal assembly) rather
than pinned by a dedicated assembly event — the journaled model call is the
evidence of what the model saw, and it is sufficient because the model call
is the only consumer of the assembly.

## What R0.13 deliberately does NOT build

- **No vector retrieval or generic RAG.** Argued above: utility re-ranking
  first, measured against recorded workloads; `MemoryRecord.embedding`
  reserved so a justified similarity index lands additively later. An
  embedding provider, a vector index, and ingestion pipelines are post-R1.0
  candidates per the roadmap, unchanged.
- **No changes to claimed files.** `record.rs`, `executor.rs`, `react.rs`,
  `composer.rs`, `tool/**`, `checkpoint*` are other streams'. R0.13 is new
  modules plus additive `lib.rs` mod lines, plus one additive
  `CandidateKind` variant in the unclaimed `learn.rs` (flagged below).
- **No new journal event kinds.** The evidence strategy above rides
  existing kinds; additive `RunEventKind` growth (e.g. a dedicated
  `SkillLoaded` event, which `skill.rs`'s docs already anticipate) waits for
  a wave where `record.rs` is unclaimed.
- **No online or in-run learning.** Assembly policies, rankings, and skills
  change between runs, by pointer, through the gate. In-run writes are
  governed memory writes — journaled, scoped, inert beyond retrieval.
- **No self-modifying graphs and no model-weight training.** Graph topology
  is code pinned by `graph_hash`; weights are the roadmap's standing
  de-priority.
- **No learned agent or model selection.** A governed semantic policy, per
  the roadmap's R0.10 boundary; tool *shortlisting* is configuration-shaped
  and gated, agent/model choice stays human-governed.
- **No silent argument repair.** Validation fails loud with structured
  violations the model answers; nothing rewrites a model's call into a
  different call.
- **No cross-tenant learning.** Utility indexes, outcome stats, and
  distillers read one tenant's evidence; the isolation model is unchanged.

## Wave plan and release proof

**Wave 1 — the context pipeline and tool manifests.** New modules
`context.rs` (`ContextPipeline`, `ContextPolicy`, section producers,
per-section budgets, the `TokenCounter` seam with the shipped estimate as
the floor implementation, history compaction with watermarks) and
`tool_select.rs` (`ToolManifest` derived from `ToolCapability` plus the
governed overlay, structural `select` shortlisting, `ValidatingTool`), plus
the ReAct consumption recipe (`AssemblingChatModel`, construction-time
`restricted_to` narrowing) as an example. Depends on nothing unshipped.
*Independently valuable:* deterministic, budgeted, journaled context with
schema-validated tool calls is a win with zero learning. **Exit:** equal
inputs and pinned policy produce byte-identical assemblies across processes
(a checked-in golden pins one); an exact replay of a pipeline-assembled run
serves the journaled model calls with zero outbound calls; a registry of 40
tools shortlists to a pinned top-k with the full ranking in the section
manifest; a malformed tool call returns a structured violation and the
repaired call succeeds, both journaled.

**Wave 2 — memory organization and optimization.** New module
`memory_tiers.rs`: the tier overlay (tier-aware assembly sections),
hierarchical key grammar and write-gate validation, `ConsolidationPolicy`
as a `memory_config` artifact with threshold triggers, content-equal dedup
at the write gate, and the **utility index** — the derived projection of
journaled `MemoryRead` assemblies joined against terminal status and eval
scores — consumed as a journaled rank input with policy-pinned weights.
Includes the recall measurement that decides the vector question, published
in `benchmarks.md`. **Exit:** a scheduled consolidation policy triggers the
shipped durable consolidation on threshold, superseding sources with
dependent-summary invalidation intact; utility re-ranking under a promoted
`memory_config` candidate improves task success on the recorded dataset at
non-inferior cost, with the zero-weight floor as baseline and byte-exact
rollback; the index rebuilds from journals byte-identically.

**Wave 3 — tool selection and call-outcome learning.** The outcome roll-up
(durable task over journaled `ToolCall` events: per-tool and per
argument-pattern success, latency, validation-failure rates), selection
policies promoted through the gate as `tool_contract` artifacts, batching
metadata (`parallel_safe` / `batchable`) consumed by dispatching nodes, and
the argument-repair distiller (repeated validation failures → correction
examples and prompt candidates through the R0.8 loop). Depends on Wave 1's
manifests and Wave 2's derived-index discipline. **Exit:** a learned
selection policy distilled from journaled outcomes promotes through the
envelope (scoped approval) and measurably reduces invalid and failed tool
calls on the recorded dataset at non-inferior completion; rollback restores
the prior shortlist byte-exactly; every transition journaled through the
existing candidate kinds.

**Wave 4 — skills and the end-to-end loop.** New module `skills.rs`
(`SkillBinding`, structural skill shortlisting into the pipeline's skills
section, tool narrowing while a skill is active), the additive
`CandidateKind::Skill` with its `{ name, content_hash, binding }` content,
the trajectory distiller reference implementation, and the release proof.
Depends on Waves 1–3 (the pipeline carries the skills section; the derived
indexes feed distillation). **Exit (the release proof):** a scripted agent
with a planted behavioral defect runs and journals; a human correction lands
through the shipped correction loop; the trajectory distiller produces a
candidate skill whose package passes the skill plane's own validation and
scan; evaluation (replay divergence + experiment over the
correction-augmented dataset) shows improvement with no regression; a scoped
approval promotes it; new runs assemble with the promoted skill and exhibit
the corrected behavior — the explanation asserted by walking ids from the
improved run's journal to the promotion, the evaluation, the candidate, and
the correction; rollback re-points the skill and the defect behavior
returns, byte-exact.

## Open questions

Flagged before Wave 1 lands:

1. **`CandidateKind::Skill` placement.** The additive variant must live in
   `learn.rs` (where `CandidateContent` is), which is unclaimed but shared.
   Leaning: one variant with a content-hash reference to the skill package —
   no package bytes inside the candidate — keeping the diff minimal and
   merge-friendly. If another stream is concurrently extending
   `CandidateKind`, sequence the merges; golden files pin the outcome.
2. **Section-manifest carrier.** A reserved manifest message inside the
   journaled `ModelCall` input versus a request extra field. Leaning: the
   manifest message — providers ignore unknown content parts less reliably
   than they ignore a text segment, and the manifest must be inside the
   request hash either way.
3. **Utility weight bounds.** How far a utility signal may move rank before
   the assembly stops being predictable. Leaning: utility perturbs order
   within a declared band around the shipped rank (priority/confidence/
   recency), never overrides a hard filter, and the band width is policy
   configuration — the floor stays one pointer move away.
4. **Compaction distiller trust.** A compaction summary is a model
   product inside a live run — the one place R0.13 lets an LLM shape
   context without a gate. Leaning: compaction is reversible-by-construction
   (the verbatim history stays in the journal and checkpoints; only the
   assembled view changes), the compaction trigger and summarizer prompt are
   pinned policy, and the summary is marked as generated in the assembled
   section. A *persistent* summary of history is consolidation's job and
   goes through memory governance; in-run compaction never writes the store.
5. **Shortlist cutoff default.** Below the cutoff, shortlisting is identity
   (every tool is shown, ranked). Leaning: cutoff 20 schemas — the point
   where schema tokens measurably crowd the context budget — declared in the
   policy, not hard-coded.
6. **Where distillers run.** The trajectory distiller is application code
   per the R0.8 boundary, but an unguided distiller makes the flagship loop
   theoretical. Leaning: R0.13 ships exactly one reference distiller —
   correction-augmented trajectory → candidate skill — which the release
   proof exercises, the same minimalism R0.8 applied to its correction
   distiller.

## Coordination notes for the other streams

- **`record.rs` owner:** R0.13 adds no `RunEventKind` variants. The future
  `SkillLoaded` event `skill.rs` anticipates is deliberately deferred to a
  wave where the record plane is unclaimed; flagging it here so it lands
  once, cleanly.
- **`learn.rs`:** one additive `CandidateKind::Skill` +
  `CandidateContent::Skill` variant and its `surface_for_kind` arm
  (`skill:{name}`), appended; existing variants, goldens, and gate behavior
  untouched.
- **`tool/**` owner:** consumed read-only. `restricted_to`, `ToolCapability`,
  `ToolExecutor` batch semantics, and the middleware hooks are sufficient —
  if they stop being sufficient, that is a request to your stream, not a
  patch.
- **`react.rs` owner:** consumption is the four composition seams above; no
  edits requested.
- **`composer.rs` owner:** composer keeps agent-facing drafting/publishing;
  R0.13's trajectory distiller constructs packages between runs through the
  skill plane's validators. The two paths converge on `SkillRegistry`
  content addresses; composer's publish gate (approval-scoped) and R0.13's
  candidate gate both end at a version pointer — one surface, two governed
  authors.
- **`lib.rs`:** four additive mod lines (`context`, `memory_tiers`,
  `skills`, `tool_select`), appended in alphabetical position to minimize
  merge collisions.
