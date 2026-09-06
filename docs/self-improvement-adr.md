# ADR: Rusty is scaffolding-only self-improvement (EP-17-S01)

Status: decided, 2026-09-06. Revisit only under the conditions in "Reopening"
below.

## Context

Rusty describes itself as a self-improvement platform, and the question that
predictably follows is whether its agents rewrite themselves. The survey of
self-improvement in modern agentic systems (arXiv:2607.13104, notes in
`docs/self-improving-agents-survey-notes.md`) gives the vocabulary for a
precise answer. It splits persistent self-updates into a parametric loop
(weight updates) and a scaffolding loop (updates to the operational scaffold
`Σ = (prompts, memory, tools, graph)`), and splits scaffolding improvement
into four branches. The fourth — full scaffolding, the Darwin Gödel Machine /
STOP / ADAS / AlphaEvolve line — is self-referential: the system rewrites the
machinery that decides what gets rewritten. That branch is where the safety
literature is thinnest and the demos are flashiest.

## Decision

**Every persistent update to a production agent's scaffold gates through
governed promotion, and nothing else.** Prompts, memories, skills, policies,
and tool permissions change only as an immutable candidate, evaluated against
recorded evidence, promoted as a journaled runtime transition, and reversible
in one operation (the EP-12-S08 promotion contract; `docs/learn-design.md`).
No learning process rewrites a production artifact in place, silently or
otherwise.

**Rusty does not build control-logic self-modification.** The components that
decide what may change — the promotion gates, the evaluators, the regression
packs, the held-out enforcement, the learning loop itself — are platform code,
versioned and reviewed like any other platform code. No agent, learned
component, or generated artifact may modify, replace, or supply them. The
survey's branch 2.4 stays out of the runtime.

## Why

The governance value of a gate is bounded by what the gate cannot be talked
into. A promotion gate an agent could rewrite is a gate the agent already
controls, which makes every receipt it issues meaningless — the audit trail
would record decisions the audited party authored. Keeping the
decision-making machinery outside the blast radius of the learned system is
what makes the receipts worth journaling in the first place.

This is also where the leverage is. The survey's own evaluation lens —
report regressions on previously solved tasks, held-out transfer, attribution
to the updated component — is what separates measured improvement from
anecdote, and it is cheap inside the existing machinery: regression packs
(EP-17-S02), held-out enforcement (EP-17-S03), and attribution on eval
artifacts (EP-17-S04) all landed as rollups over the promotion journal. The
governed scaffolding loop plus that measurement story is a complete, honest
answer to "does the platform improve itself" — yes, measurably, within an
envelope — without taking on the one branch the field cannot yet operate
safely.

## Consequences

What this rules in: agents that improve their prompts, memories, skills, and
policies through candidates, gates, and rollback; capability evolution that is
attributable to an author, an evaluation, and an approver at every step.

What this rules out: agents editing their own eval suites to pass them;
learned "improvements" to the promotion path itself; any runtime mechanism
where generated content becomes gate logic without a human-reviewed code
change. Proposals in this direction are rejected at design review, not
shipped and then gated.

## Reopening

This decision holds until the gates mature: regression packs, held-out
enforcement, and attribution running in production long enough that their
absence would be noticed, plus an external review of the gate machinery.
Reopening is itself a governed change — a written amendment to this record,
not a pull request that quietly widens what agents may touch.
