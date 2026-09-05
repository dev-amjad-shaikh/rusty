# Self-improving agents: survey notes for Rusty

Source: [Awesome-Self-Improving-Agents](https://github.com/selfimproving-agent/Awesome-Self-Improving-Agents),
the curated repository accompanying *Self-Improvements in Modern Agentic Systems:
A Survey* (Ren, Chen, Guo, Rong, Li, Xiong, Lan, Wang, Nanbo, Yang, Zhuge,
Schmidhuber — arXiv:2607.13104, 2026). The repo indexes ~250 papers, benchmarks,
blogs, and courses; its `README_AGENT.md` condenses the survey's formal model,
taxonomy, and design implications. These notes extract what matters for Rusty's
foundation and point at the modules each lesson lands in.

## 1. The formal model, and where Rusty sits in it

The survey defines an FM-based agent at iteration *t* as `A_t = (θ_t, Σ_t)`:

- `θ_t` — foundation-model parameters
- `Σ_t = (p_t, m_t, T_t, g_t)` — the operational scaffold: prompts, memory,
  tools, and control logic (routing, scheduling, safety constraints)

Self-improvement is a **durable, self-induced update** to `θ` or `Σ` produced
from signals arising in the agent's own execution. Transient execution state —
dialogue history, intermediate plans, KV cache — explicitly does **not** count.

Rusty is a scaffolding-improvement platform: the fast, non-parametric loop
(`Σ_t → Σ_t+1` with `θ` fixed). Weight-level updates are out of scope, but two
of our seams already anticipate them:

- the provider ABI (`rusty-api`, EP-02-S01) treats the model as a replaceable
  backend, so a fine-tuned successor model is a routing change, not a
  rewrite;
- turn stamps (EP-02-S10) separate main from side traffic, which is exactly
  the bookkeeping the survey's "slow loop" needs to attribute gains to a
  parametric update versus a scaffold update.

The durable/transient line is already load-bearing in Rusty: the journal is the
durable record, context assembly is transient. The survey gives us vocabulary
to keep that line sharp — a "memory write" that never leaves the context
window is not improvement, and our docs should stop calling it learning.

## 2. Taxonomy mapped to Rusty

The survey splits scaffolding improvement into four branches. Mapping each to
what exists on `main` today (2026-09-03, `6aef720`):

| Survey branch | Representative methods | Rusty coverage | Status |
|---|---|---|---|
| 2.1 Prompt optimization | Self-Refine, Reflexion, GEPA, TextGrad, Promptbreeder | Frozen three-tier assembly with violation detection (EP-02-S09); declarative composition vocabulary (EP-02-S11) | Partial — we freeze and verify prompts, we do not yet *optimize* them |
| 2.2 Memory | ExpeL, A-MEM, Mem0, Zep, ReasoningBank, Agent Workflow Memory | MemoryRecord with provenance/scope/validity (EP-06-S01–S03), conflict detection, planned forgetting, consolidation gating + high-water marks (EP-06-S08), loss-bounded hash-checked rewrites (EP-06-S09), hierarchical summary index (EP-06-S12) | Strong — this is our best-covered branch |
| 2.3 Tool improvement | Voyager skill library, SkillWeaver, Alita, LATM | Toolset combinators (EP-05-S04), sandbox seam (EP-05-S05/S12), in-process MCP bridge (EP-05-S09), skill packs with evals (EP-15-S08), connector packs (EP-15-S05/S06) | Partial — tool *packaging and routing* yes, tool *creation and refinement* no |
| 2.4 Full scaffolding | Darwin Gödel Machine, STOP, ADAS, AlphaEvolve, ShinkaEvolve | Promotion gates with rollback receipts (EP-12-S08), blueprint/managed-agent versioning | Deliberately thin — self-modifying control logic is a governance problem before it is a capability |

And the evaluation lens:

| Survey concern | Rusty coverage |
|---|---|
| Metric-based measurement | Conformance suites as first-class evals (EP-12-S09), experiment reports |
| Judge-based measurement | Reviewer tooling, eval agents (EP-12) |
| Mechanism benchmarks | Skill-pack eval gates run through the real gate path |
| Improvement as a *trajectory* | Gap ledger + hunts (EP-07), online scoring hooks (EP-12-S06), drift baselines |

## 3. The survey's three design principles, checked against Rusty

The discussion section distills three system-level principles. We score well
on two and have a real gap on the third.

**1. Fast scaffold-level exploration, slow parametric consolidation.** Covered
by construction — Rusty is the fast loop, and the provider seam keeps the slow
loop possible downstream.

**2. The critic/verifier is governed infrastructure, separate from the
generator.** This is the strongest point of convergence. Evidence-admission
guards, the bounded exec-reviewer (EP-05-S11), promotion gates that refuse with
typed receipts (EP-12-S08), and RBAC scopes on interfaces as security
principals (EP-11-S10) all implement exactly this: the component that judges a
change is not the component that produced it, and it runs under its own
permissions. The survey treats this as the distinguishing mark of production
self-improvement versus demo self-improvement.

**3. Persistent updates are gated through layered validation, permission
boundaries, regression testing, and rollback.** Mostly covered: EP-06-S09
rewrite validation (hash check + loss bound + justification), EP-12-S08
promotion/refusal/rollback receipts. **The gap is regression testing as a
first-class input to the gate.** The survey's evaluation lens insists an
improvement claim report *regressions on previously solved tasks* and *held-out
transfer*; our promotion gates check the candidate against its own eval suite
but do not yet re-run a canonical regression pack across unrelated, previously
passing skills before admitting a change. See recommendation R2 below.

## 4. The evaluation lens: what "improved" must prove

The survey treats self-improvement as a trajectory over iterations, and lists
what an evaluation claim must report. Translated into rusty-eval terms, a
promotion decision should be able to answer:

1. Performance across update iterations under a fixed budget — we have
   experiment reports; iteration-over-iteration trending is thin.
2. Held-out transfer beyond the data used for improvement — not yet enforced;
   a skill can promote on the same suite it was tuned against.
3. Cost accounting (compute, API, wall-clock, supervision) — partially there:
   priced model calls journal `cost_usd` (EP-02 follow-ons); eval-side cost
   rollups are not surfaced in promotion decisions.
4. Regressions on previously solved tasks — the gap named above.
5. Safety violations and tail risks — egress policy (EP-11-S03) and scope
   census give the enforcement layer; eval-side safety cases exist via
   conformance severity but are not mandatory for promotion.
6. Attribution to the updated component — turn stamps give us the raw
   material; nothing yet attributes a score delta to "the memory rewrite" vs
   "the prompt change" vs "the model swap".
7. Evaluator independence when a judge is used — the reviewer path is
   separate infrastructure; worth documenting as an explicit invariant.

## 5. Risk register the survey names, and our mitigations

| Risk | Survey branch that raises it | Rusty mitigation today | Exposure |
|---|---|---|---|
| Memory poisoning / corruption | 2.2 (DrunkAgent benchmark) | Content-addressed records, provenance, scope, write gate, dedup-on-converge, planned forgetting with tombstones | Low for direct writes; **open** for learned summaries — a poisoned consolidation output currently passes if loss-bounded |
| Prompt drift | 2.1 | Frozen tiers with pre-dispatch verification (EP-02-S09) | Low for frozen tiers; user-tier prompt evolution has no drift tripwire |
| Reward hacking / judge capture | evaluation | Separate reviewer/eval infrastructure, typed refusals | Medium — judge independence is convention, not enforcement |
| Unsafe self-modification | 2.4 (Gödel machines) | Full-scaffold self-rewriting not built | None today; keep it that way until gates mature |
| Rollback correctness | 2.4 | RollbackReceipt, version pointers (EP-12-S08) | Low |

## 6. Papers worth reading closely, in Rusty priority order

Not a reading list for its own sake — each entry names the Rusty module it
informs.

1. **ReasoningBank** (2025) and **Agent Workflow Memory** (ICML 2025) — memory
   objects that store *reasoning patterns* and *reusable workflows*, not raw
   episodes. Directly relevant to what our consolidation (EP-06-S08) should be
   promoting into long-term storage once unblocked.
2. **Agentic Context Engineering** (ICLR 2026) — evolving the context itself
   as the improvement substrate. Closest published neighbor to our frozen-tier
   + suffix assembly; read before touching EP-02-S09 again.
3. **GEPA: Reflective Prompt Evolution** (ICLR 2026) — population-based prompt
   evolution reported to outperform RL on some tasks, with text feedback. The
   plausible first prompt-optimization mechanism if we open that branch.
4. **ExpeL** (AAAI 2024) — experiential learning: extract rules from
   success/failure episode pairs. Simple, proven, and maps cleanly onto our
   gap-ledger → hunt → skill-pack pipeline (EP-07 → EP-15-S08).
5. **Voyager** (2023) and **SkillWeaver** (2025) — the canonical tool/skill
   library loops: propose, verify in-environment, store, reuse. Our skill-pack
   format (EP-15-S01/S08) already has the storage shape; these define the
   discovery loop we have not built.
6. **Dynamic Cheatsheet** (2025) — test-time adaptive memory with no weight
   updates. A minimal-risk first rung for persistent adaptation.
7. **Mem0** (2025) and **Zep** (2025) — production memory systems; useful as
   engineering references for the hierarchical index (EP-06-S12), not as
   research.
8. **Darwin Gödel Machine** (2025) / **ShinkaEvolve** (2025) — full-scaffold
   evolution with archive-based selection. Read for the *selection and
   archive* mechanics (how they decide which self-modification survives), not
   the self-rewriting.
9. **Agent-as-a-Judge** (ICML 2025) — the judge-based measurement reference if
   reviewer-driven evals expand.
10. **RSI-Bench** / **PAST-Bench** (2026) — early benchmarks for recursive
    self-improvement; watch them rather than adopt them.

## 7. Recommendations for the backlog

Ordered by leverage against the current 77%-landed backlog. None of these
replace existing stories; they are candidates for the next epic grooming.

- **R1 — Keep Rusty scaffolding-only for now; write it down.** The survey's
  2.4 branch (self-referential scaffold rewriting) is where the safety
  literature is thinnest and the demos are flashiest. A short ADR stating that
  Rusty gates all persistent updates through EP-12-S08-style promotion and
  does not build control-logic self-modification would preempt the most
  frequently asked question about a "self-improving agent platform".
- **R2 — Regression packs as a promotion-gate input (new story, EP-12).** A
  canonical suite of previously-passing skill evals re-run on every promotion
  decision; failure blocks with a typed refusal. This closes the survey's
  "regressions on previously solved tasks" requirement and is cheap: the
  conformance registry (EP-12-S09) already has persistence and versioning.
- **R3 — Held-out split enforcement in eval gates (new story, EP-12).** A
  candidate that was tuned against suite S cannot promote on S alone; the gate
  requires a held-out suite or a version bump. This is the survey's
  "held-out transfer" requirement and pairs with EP-12-S09's
  version-bump-invalidation mechanism, which already exists.
- **R4 — Attribution fields on eval artifacts (small, EP-12).** When a
  promotion candidate is evaluated, record which scaffold components changed
  relative to the baseline (prompt tier hash, memory high-water mark, skill
  pack version, model stamp). The journal already carries every input; this is
  a rollup, not new instrumentation.
- **R5 — Consolidation output poisoning check (EP-06, after EP-06-S08
  unblocks).** Extend EP-06-S09's loss-bounded validation with a
  provenance-diversity check: a consolidation that collapses N independent
  sources into claims attributable to fewer sources should require stronger
  justification. This is the concrete form "memory poisoning" takes in our
  model.
- **R6 — Adopt the survey's vocabulary in docs.** "Durable vs transient
  update", "scaffold vs parametric loop", "generator vs governed critic".
  `learn-design.md` and `gap-ledger-design.md` already implement these ideas;
  naming them makes the design legible to anyone arriving from the
  literature.

## 8. One-paragraph summary

The survey's taxonomy confirms Rusty's center of gravity — governed,
non-parametric scaffolding improvement — is where production systems
converge. Our memory and gating infrastructure covers the survey's memory
branch unusually well, and the critic-as-governed-infrastructure principle is
already our architecture. The real gaps the literature exposes are
evaluation-side: regression testing across previously solved tasks, held-out
transfer enforcement, and gain attribution, plus a deliberate, documented
decision to stay out of full-scaffold self-modification until the gates
mature. Items R2–R4 are small and sit entirely inside EP-12's existing
machinery; they would move Rusty from "implements self-improvement" to
"provably measures it".
