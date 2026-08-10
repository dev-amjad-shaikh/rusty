# Rusty Studio experience roadmap

Rusty Studio is the experience layer for the Rusty platform: the place where a person can create, run, understand, improve, govern, and operate agent systems without translating the platform's internal APIs into a workflow by hand.

This document defines the product contract for that experience. It complements the runtime roadmap in [roadmap.md](roadmap.md); it does not replace it.

## North star

**From an idea to a trustworthy agent system in one continuous workspace.**

A new user should be able to create and run a useful local agent in under five minutes. An experienced operator should be able to explain a failed production run, compare a proposed change, approve a learned improvement, and roll it back without leaving the same evidence trail.

The core journey is:

> **Shape → Run → Understand → Improve → Govern → Operate**

Rusty Studio is complete only when this journey works for single agents and durable agent teams, on a laptop and in a deployed environment, with evidence and safety visible at every decision point.

## Product position

Most agent interfaces separate construction, tracing, evaluation, and operations into different products or mental models. Rusty should make the run record the connective tissue between them.

The distinctive experience is an **evidence rail** that follows every agent version:

- what is configured;
- what it is allowed to do;
- what happened in each run;
- what changed between versions;
- what the evaluations proved;
- what was learned and who promoted it;
- which version is active and how to return to the previous one.

This is where Rusty's runtime advantages become product advantages:

| Rusty capability | Studio experience |
|---|---|
| Hash-chained journal and Flight Recorder | A causal execution story, not a bag of logs |
| Replay evidence and checkpoint forks | Reproduce replay-eligible runs today; expand recorded-effect serving so model, tool, remote, WASM, resumed, and coordinated team runs can be replayed safely |
| Typed effects and receipts | Show retry and side-effect risk before an action is allowed |
| Durable mailboxes, supervision, and coordination contracts | Make agent teams inspectable and recoverable |
| Versioned run-manifest pins | Bind each run to resolvable, immutable model, prompt, tool, memory, policy, and agent-configuration versions |
| Governed memory and learning candidates | Turn self-learning into an attributable, evaluated, reversible workflow |
| Rust/WASM execution | Offer local-first, low-overhead, capability-scoped execution with clear boundaries |
| Budgets and deterministic scheduling | Make cost, time, concurrency, and risk constraints part of configuration and traces |

The Studio should not imitate a generic workflow canvas. Its signature is the connection between **the system being shaped** and **the evidence proving how it behaves**.

## Experience principles

1. **Start with intent.** Ask what the agent is responsible for before exposing graph identifiers or JSON.
2. **Progressive disclosure.** Common choices are plain-language controls; exact wire configuration remains available in an advanced inspector.
3. **Evidence beside action.** A change, promotion, retry, replay, or rollback shows the evidence and consequence at the point of decision.
4. **One vocabulary.** Agent, team, run, thread, version, tool, memory, evaluation, candidate, and deployment keep the same meaning everywhere.
5. **Local first, deployment ready.** The full build/debug loop works against a local Rusty server without an account or hosted dependency.
6. **Safe by construction.** Permissions, effects, credentials, budgets, approvals, and destructive actions are visible and explicit.
7. **No false affordances.** The UI never presents a control that the connected server cannot honor.
8. **Raw JSON is an escape hatch.** It is never the default experience for a supported schema.
9. **Failure teaches the next action.** Empty, offline, incompatible, interrupted, and failed states explain what happened and how to proceed.
10. **Accessibility is a release condition.** Keyboard operation, visible focus, screen-reader names, contrast, reduced motion, and responsive behavior ship with every feature.

## The product model

Studio is organized around a persistent **agent workspace**, not around API endpoints.

| Workspace surface | Primary question | Essential capabilities |
|---|---|---|
| **Home** | What needs attention? | Recent agents and runs, failures, pending reviews, budget alerts, local-server health |
| **Build** | What should this system do? | Identity, instructions, model, tools, memory, output, guardrails, budgets, triggers, versions |
| **Team** | How should agents collaborate? | Visual roles, typed handoffs, delegate/fan-out/race/quorum contracts, private/shared state, supervision |
| **Run** | Does it work on a real task? | Conversational and structured input, streaming output, approvals, interrupts, artifacts, cancellation |
| **Inspect** | Why did it behave this way? | Flight Recorder, causal paths, state/checkpoints, effects, receipts, costs, replay, fork comparison |
| **Evaluate** | Is this version better and safe? | Datasets, evaluators, experiments, comparisons, failure clusters, release gates |
| **Learn** | What should improve? | Corrections, memory records, conflicts, candidates, shadow results, approvals, promotion, rollback |
| **Operate** | What is live and healthy? | Versions, environments, triggers, schedules, tasks, deployments, fleet health, audit trail |

The same version identity and evidence rail appear across all surfaces. A person can move from a failed run to its configuration, from the configuration to a comparison experiment, and from an experiment to a promotion without copying identifiers.

## Current state — August 2026

Studio is a useful engineering console, not yet the complete experience described above.

### Available now

- Start from a coherent Home mission board that shows the complete evidence-led journey, distinguishes
  current server catalog state from bounded browser recall, recommends the next action, and continues
  directly into the latest remembered agent or team evidence without retaining prompts or results.
- Connect to a local or remote Rusty server.
- View registered graphs and create or attach threads.
- Create an assistant through the Agent Workbench.
- Safely duplicate an assistant, review the configuration contract, and import or export a bounded,
  versioned assistant manifest without copying run history.
- Distinguish runnable assistant configurations from mailbox-addressed durable identities, browse
  identities by declared team label, inspect activation / mailbox / supervision evidence, and
  investigate a known coordination through its typed member state and read-only TeamTrace.
- Compose delegate, bounded fan-out, safe race, and deterministic quorum work from a selected team label
  without hand-editing a coordination payload. Studio pins member manifests and accepted kinds,
  generates a stable retry key, validates narrowed context and effect admission, exposes cancellation,
  discarded-work, threshold, resolver, tie, and compensation risk, requires an explicit durable-work
  acknowledgement, reconciles deduplicated keys to the actual durable contract, and opens the created
  coordination directly in its evidence view.
- Save, import, export, and reopen connection-scoped structural team blueprints. Their topology score
  keeps roles and convergence readable; live-registry reconciliation blocks missing roles, removed
  contracts, widened scope, and missing recipients while making manifest-pin drift reviewable. Prompts,
  task inputs, deadlines, run identities, results, acknowledgements, and receipts are excluded by contract.
- Return to recently started or attached team coordinations through a browser-scoped Team Run Desk.
  Search and lifecycle filters, an accessible settlement pulse rail, bounded manual reconciliation,
  visibility-aware live following, and explicit stale-evidence recovery make current team work usable
  without claiming the server can discover every coordination.
- Follow a guided Create → Run → Inspect first-run path.
- Run in background, blocking, or streaming modes.
- Inspect state, checkpoint history, interrupts, and resumes.
- Turn a detected memory conflict into an exact, reviewed consolidation task. Studio binds the sorted
  source set, non-run scope, distiller, summary policy, and queue pool to a corroborated durable-task
  receipt, then follows that task from either the receipt or Durable Work into an exact scoped summary
  search. Only one summary with the same source set, attribution, learning instant, key, tags, and priority
  becomes resolution proof; mismatches, duplicates, and completed-without-summary states remain explicit
  attention. Task settlement is separate evidence.
- Preview the exact governed memory context for one scope before a run: compose every supported structural
  query filter, pin expiry time, inspect the budgeted assembly's included rank and token accounting, and use
  a separately labelled live comparison for non-atomic truncation corroboration. Hard-budget responses do
  not invent an omission record, and an unavailable ancillary comparison never hides exact budgeted evidence.
  Each streamed inspection response has an explicit 8 MiB ceiling. The preview is session-only, non-journaled, and can hand one returned record
  into the bounded provenance ledger.
- Investigate Flight Recorder evidence through a causal run story: journal finding, recorded error event
  or unresolved pause, recovery boundary, effect risk, and direct links into the technical timeline.
- Mint and cryptographically verify a finalized run's signed chain of custody through the connected
  deployment verifier. Studio binds the exact
  journal head to its carried manifest, resolved capsules, effect/denial ledger, executor/Cedar policy,
  and active, retired, or historical deployment signer. The operator sees the mint-on-read consequence before the
  action and an explicit boundary: local receipt integrity is not model quality, provider truth, or
  remote/KMS transparency attestation. Key rotation remains an operator/platform concern outside Studio.
- Read the verified run manifest as a signed runtime bill of materials: model identity and parameter
  digest, prompt-content pins, tool-schema pins, memory schema, and capsule versions. Missing surfaces
  remain explicitly unpinned, partial model pins stay partial, unknown signed fields remain visible but
  uninterpreted, and digest evidence never expands into secret-adjacent configuration values.
- Review a selected run's interrupt at an explicit human decision boundary: bounded request preview,
  corroborated run/thread/checkpoint identities, schema-led approve/deny or custom exact response,
  checkpoint-bound resume, super-step re-execution warning, competing-run gate, wait or live-event resume,
  and distinct handling for confirmed rejection versus an uncertain terminal response.
- Exact-replay eligible deterministic journals and compare forked branches. Journals containing model, tool, remote, WASM, or resume effects are not yet accepted by exact replay.
- Inspect and cancel durable tasks.
- Use the core flow on desktop and mobile with keyboard support.

### Material gaps

- Assistant editing, versioning, archive, restore, and deletion are absent. Safe duplication is available.
- The configuration workshop covers the current persisted contract: graph, runtime step limit, catalog
  metadata, exact advanced JSON, and manifest portability. Model, tool, memory, output, guardrail, and
  budget configuration still lack typed server discovery and first-class runtime forms.
- Thread and run discovery is browser-local; there is no durable server-side run desk.
- The selected-thread interrupt review draft is browser-session-only, while the executor's interrupt and
  resume events follow the configured server journal. There is no durable assigned human-review inbox,
  authority model, reservation, deadline, decision audit record, or cross-thread discovery yet.
- Assistant configuration and durable identity are explained together, but there is no lifecycle link
  that binds an assistant version to a registered durable identity.
- The Team Observatory provides inventory, all four typed coordination launches, read-only coordination
  evidence, browser-scoped run recall with a selected-run live settlement overlay, and portable structural
  blueprints with a visual topology score and live-roster drift preflight. Server-persisted visual topology,
  durable team definitions, creation/editing lifecycle, server-side coordination discovery,
  supervisor and recovery controls, richer active-member evidence, and coordinated replay remain.
- The general-purpose dataset and evaluation experiment workspace remains library-only; governed
  learning-candidate evaluation is now server-backed in Studio.
- Memory records and structural conflicts have a human-readable audit workspace. Selected-memory
  corrections now append attributed records through an immutable original → correction → result
  path, with candidacy, finalized-run adoption, receipt validation, reconciliation, and exact retry.
  Conflict review now also produces a receipt-bound durable consolidation task and keeps queued work
  explicitly separate from a governed summary record that names and supersedes its sources. The visible,
  connection-bound outcome path follows task lifecycle into that exact summary and opens the result in the
  ledger without persisting content locally. Run-event/prompt correction capture, candidate approval,
  expiration, and deliberately approval-gated forgetting remain. The context assembly lab makes current
  deterministic rank and token packing visible, but the server still needs pagination, an atomic read/version
  receipt, an explicit live-versus-active-overlay indicator, and model-specific tokenizer accounting.
- Learning candidates, replay-backed evaluation, scoped promotion approval, active/canary version
  pointers, rollback receipts, and guided prompt/policy/tool-permission creation now have a governed
  control room. Memory-set and automatic/correction-driven distillation, before/after case analysis,
  envelope discovery, assigned or signed approval identities, drift/canary monitoring, and complete
  runtime activation proof remain.
- Triggers, crons, credentials, artifacts, deployments, and fleet operations are not unified.
- Authentication identifies a tenant API key, not an attributed human or service principal with scoped roles and auditable authority.
- Local Studio now defaults access keys to a session-only boundary and requires explicit warning-backed
  consent before device-local persistence. A deployed Studio still needs the server-side session or
  credential-broker boundary defined below.
- `GET /info` does not yet advertise a versioned capability contract, so route probing is the only reliable compatibility fallback.
- Run-manifest pins are not yet bound to a resolvable immutable assistant/configuration version on the evidence surface.
- The single-file implementation is approaching the point where feature isolation and maintainability require a modular frontend architecture.

## Roadmap

Each milestone is a complete user outcome. A milestone is not complete because its screens exist; its state, failure, accessibility, testing, and real-server paths must also work.

### ES0 — First ignition · foundation

**Outcome:** connect, create an agent, run a task, and inspect the run.

Status: **usable foundation delivered**. The guided path and evidence-led Home mission board work,
including honest local/server signals and direct continuation into recent agent or team evidence.
The Connection Hub now delivers guided onboarding, bounded non-secret server profiles, session-only
secrets by default, explicit device-local opt-in, verified server identity, safe compatibility evidence,
and recovery-preserving connection switching. A deployed credential boundary and a versioned server
capability contract remain.

Connection and session requirements:

- Saved profiles persist non-secret server metadata by default.
- API keys and other secrets are session-only unless the user explicitly opts into local persistence after a clear warning.
- A deployed Studio uses an authenticated session, backend-for-frontend, or equivalent credential boundary rather than exposing durable platform credentials to browser storage.

Exit criteria:

- A first successful local run takes less than five minutes from opening Studio.
- Server/version incompatibilities are diagnosed without exposing network jargon first.
- The user can always tell what server, tenant, agent version, thread, and run are active.
- No first-run step requires hand-written JSON for a conversational agent.

### ES1 — Agent workshop · lifecycle and configuration

**Outcome:** create and safely manage a real agent configuration.

Status: **partially delivered**. Creation, safe duplication, a readable configuration contract, bounded
manifest import/export, validation, and real-run handoff work against the current assistant API. Editing,
immutable versions, archive/delete, typed capability discovery, and governed runtime configuration remain.

Scope:

- Edit, duplicate, version, archive, restore, and delete an assistant.
- Guided configuration for instructions, model/provider, tools, memory policy, output schema, guardrails, budgets, and runtime behavior.
- Searchable capability catalog with permission and effect-risk summaries.
- Draft versus active version, visual configuration diff, validation, and rollback.
- Configuration portability: export and import a human-readable manifest.
- Clear explanation of the relationship between a graph blueprint, assistant configuration, and durable agent identity.

Platform dependencies:

- Assistant update/delete/version endpoints.
- Model/provider and tool/capability discovery contracts.
- A governed configuration registry built on the versioned run manifest.
- Credential handles rather than raw secrets in assistant configuration.

Exit criteria:

- A common agent can be configured without raw JSON.
- Every configuration field maps to a real persisted/runtime contract.
- Editing an active agent creates a reviewable version instead of mutating history.
- Destructive actions state their blast radius and require deliberate confirmation.

### ES2 — Run desk · conversational testing and evidence

**Outcome:** run agents repeatedly and understand success, cost, and failure from one place.

Scope:

- Durable run and thread history with search, filters, tags, status, duration, cost, model, version, and environment.
- Conversational and schema-generated structured inputs, file/artifact inputs, and saved test inputs.
- Streaming transcript with tool calls, handoffs, approvals, and artifacts inline.
- Cancellation, retry, fork, replay, and compare actions with effect-safety guidance.
- Flight Recorder redesigned as a coordinated timeline, topology, state, and evidence inspector.
- Shareable run links and portable evidence bundles with redaction controls.

Platform dependencies:

- Server-side thread/run listing and durable run index.
- Artifact retrieval and trace-safe redaction contracts.
- Recorded-effect serving and eligibility reporting for model, tool, remote, WASM, resumed, and coordinated runs.
- An immutable configuration-version registry, run-to-version binding, and resolution of manifest pins on the run evidence surface.

Exit criteria:

- A user can find any retained run without having saved its identifier locally.
- A failed run's causal failure and last safe recovery point can be found in under two minutes.
- Replay and comparison never imply that a non-idempotent effect will be repeated silently.

### ES3 — Team foundry · multi-agent systems

**Outcome:** design, test, and recover a durable agent team visually.

Status: **usable coordination, reusable-structure, and run-monitoring foundation delivered**. Delegate, fan-out, race, and
quorum can be composed without raw payloads, with pinned manifests, pattern-specific safety preflight,
stable retry, exact receipt checks, and direct TeamTrace investigation. The Team Run Desk adds
privacy-minimized browser recall, bounded reconciliation, trustworthy terminal settlement progress,
visibility-aware live following, and last-evidence preservation during outages. Connection-scoped
`rusty.team-blueprint/v1` manifests add a readable topology score, safe import/export, exact role and
policy preservation, and live-registry drift gates without retaining task or run content. Server-persisted team topology
and lifecycle, durable server-side discovery, richer active-member execution evidence, supervision
control, recovery actions, shared/versioned template lifecycle, and coordinated replay remain.

Scope:

- Role-based team canvas backed by durable identities and manifests.
- Typed connections for delegate, handoff, fan-out, race, and quorum contracts.
- Context-transfer, state-scope, deadline, cancellation, budget, and supervision configuration on connections.
- Preflight checks for cycles, missing capabilities, unsafe effects, unbounded fan-out, impossible quorum, and budget violations.
- Live team execution overlay showing active agent, mailbox depth, handoffs, artifacts, retries, and supervisor decisions.
- TeamTrace inspection, crash recovery, causal investigation, and coordinated replay where the effect contracts permit it.
- Reusable team templates with readable generated manifests.

Platform dependencies:

- Existing agent registry, mailbox, supervision, coordination, and TeamTrace endpoints provide the starting contract.
- Registry update/version endpoints and server-side team definitions are still needed for a complete lifecycle.
- A coordinated team-replay contract is needed; the current TeamTrace surface is inspectable but does not replay a cross-agent execution.

Exit criteria:

- A two-to-five-agent team can be created and run without editing a coordination payload manually.
- Every visual edge has a typed runtime contract and round-trips without information loss.
- Recovery explains which agent failed, what the supervisor did, and what work was replayed or preserved.

### ES4 — Quality lab · evaluation and human feedback

**Outcome:** prove that a proposed agent version is better before promotion.

Status: **comparison and page-memory review foundation delivered**. The Flight Recorder now turns two persisted run journals
into an evidence-led baseline-versus-candidate report: atomic structural divergence, state-channel
changes, exact resource totals, reconciled finalized/live journal signals, repeat-risk, and a deliberate
no-winner boundary when no quality evaluator exists. Matching finalized evidence unlocks a human verdict
docket with a fixed task-outcome/correctness/safety rubric, explicit pairwise judgment, exact run binding,
fresh edit acknowledgement, and a bounded page-memory review-packet export. Reload or workspace switch
discards the docket. The packet does not automatically include raw event payloads, exports reviewer notes
exactly as entered, and declares that it is neither durable nor a promotion gate. Durable datasets,
review assignment and disagreement handling, evaluators, experiment execution, statistical reports,
gates, and feedback queues still require the platform resources below.

Scope:

- Dataset creation from manual cases, files, and production traces.
- Rule, trajectory, model-judge, safety, latency, and cost evaluators.
- Baseline-versus-candidate experiments with bounded parallel execution.
- Statistical regression, failure clusters, case drill-down, and confidence explanations.
- Annotation queues, pairwise review, corrections, disagreement resolution, and promotion to dataset.
- Durable review inbox for interrupted runs, low-confidence outputs, failed gates, and learning approvals, including assignment, reservation, authority, deadline, decision, and safe resume.
- Reusable release gates with a readable pass/fail explanation.
- Direct path from a Flight Recorder event to a regression case.

Platform dependencies:

- Durable server resources and APIs for datasets, evaluators, experiments, reports, gates, and feedback queues.
- Background execution and progress streaming for experiments.
- Attributed human/service principals, scoped reviewer roles, approval authority, and immutable review audit records.

Exit criteria:

- A candidate version cannot be promoted through the UI without the evidence required by its gate.
- A failed gate identifies the affected cases, clustered cause, evidence, and remediation path.
- Experiment results remain attributable to exact agent, dataset, evaluator, model, and policy versions.

### ES5 — Learning control room · memory and governed adaptation

**Outcome:** understand what Rusty remembers, review what it proposes to learn, and reverse it safely.

Status: **partially delivered**. The immutable candidate inbox, proposal and provenance dossier,
finalized replay-fixture evaluation preflight, evaluation verdict, deployment-envelope gate, exact
candidate-scoped approval handoff, active/canary serving pointers, promotion and rollback receipts, and
byte-exact rollback action are available. The proposal foundry now composes exact prompt, supported
executor-policy, and tool-permission candidates; binds finalized evidence; exposes the content-address
before creation; and accepts a direct completed-run handoff from Flight Recorder. Selected-memory
correction capture is also available: it preserves the original, binds human attribution and scope,
validates the returned immutable record, routes wider scopes into candidacy, preflights finalized run
  scope, and reconciles uncertain writes before exact retry. A read-only context assembly lab now composes
  the full structural query, validates deterministic included rank and exact budget accounting, and exposes
  separately labelled non-atomic truncation evidence without journaling a run. Run-event/prompt capture, memory-set and
  automatic/correction-driven distillation, candidate approval, durable review
  assignment, before/after case comparison, drift monitoring, automatic canary decisions,
  durable correction rationale, expiration, and forgetting remain. Policy
candidates depend on the connected server's runtime policy plane for activation proof.

Scope:

- Memory browser by scope, kind, provenance, confidence, validity, supersession, and use in runs.
- Context preview showing which memories fit the current budget and why they were selected.
- Conflict inbox, consolidation preview, correction capture, expiration, and complete forgetting workflow.
- Candidate inbox for prompt, policy, memory, and configuration improvements.
- Candidate evidence, shadow evaluation, envelope decision, approval, canary, promotion, drift, and rollback.
- Before/after run comparison that explains the observed improvement.
- Active version pointers and immutable history per learning surface.

Platform dependencies:

- Memory query, correction, conflict, consolidation, and forgetting endpoints are available.
- Candidate lifecycle, evaluation, promotion, rollback, and version-pointer contracts must be available on the connected server.
- Drift monitoring and automatic rollback signals remain future work.
- Attributed principals, scoped promotion authority, and immutable audit records are required before governed promotion can be considered complete.

Exit criteria:

- No learned change can silently rewrite an active prompt, policy, memory, graph, or permission.
- Every promotion answers: what changed, why, based on which runs, evaluated how, authorized by whom, and how to roll back.
- Forgetting removes dependent retrieval artifacts and reports the completed scope.

### ES6 — Mission control · deployment and fleet operations

**Outcome:** publish, monitor, govern, and roll back agent systems across environments.

Scope:

- Local, development, staging, and production environments.
- Immutable deployments, health, logs, capacity, usage, budgets, and alerts.
- Evaluation-gated promotion, canary/shadow rollout, and instant rollback.
- Trigger, cron, webhook, task, and dead-letter operations in the agent context.
- Agent catalog with ownership, versions, dependencies, permissions, health, cost, quality, and active sessions.
- Fleet-wide impact search and bulk safety operations.
- Publish as API, SDK client, MCP/A2A endpoint, webhook target, or embeddable conversation surface.

Platform dependencies:

- Human and service-principal identity, scoped RBAC/ABAC, approval authority, and immutable audit records.
- Secure browser session and credential-broker contracts for deployed operation.
- Durable deployment, environment, health, usage, and fleet resource APIs.

Exit criteria:

- The active version and its evidence are visible for every environment.
- A bad deployment can be detected and rolled back without losing causal history.
- Fleet operations retain tenant isolation, authorization, and an auditable reason.

## Delivery sequence

The milestones overlap in enabling contracts, but delivery remains vertical and user-visible. The recommended order is:

| Order | Vertical slice | Status | Why now |
|---:|---|---|---|
| 1 | Recent runs in the Agent Workbench, with status and one-click Inspect | Delivered locally; durable discovery remains in order 4 | Extends the delivered first-run journey into a repeatable daily workflow without pretending the whole Studio is complete |
| 2 | Assistant edit and duplicate, with a readable configuration summary | Partial — safe duplicate and summary delivered; edit/version endpoints pending | Establishes lifecycle management as soon as the server route surface is free to extend |
| 3 | Configuration workshop for instructions, behavior, limits, and advanced manifest | Partial — current runtime contract and manifest portability delivered; typed model/tool/memory controls pending | Replaces the current create-only form and raw configuration gap |
| 4 | Run desk with search/filter and durable server discovery | Platform contract needed | Removes browser-local run/thread dependence |
| 5 | Flight Recorder investigation layout and manifest/effect context | Usable foundation — causal run story, recovery boundary, effect-risk guidance, exact signed run-proof chain, and five-surface runtime bill of materials delivered; cross-run manifest drift navigation and remote/KMS attestation remain | Makes Rusty's strongest runtime advantage understandable |
| 6 | Memory browser, context assembly, provenance, conflicts, corrections, and forgetting | Partial — browser, deterministic token-budget context preview, provenance, conflict review, selected-memory correction, exact durable consolidation launch, and task-to-summary follow-through delivered; candidate approval, expiration, and approval-gated forgetting pending | Uses server contracts that already exist and makes a distinctive Rusty capability legible early |
| 7 | Durable interrupt and human-review inbox | Partial — selected-thread decision boundary delivered with corroborated checkpoint identity, exact response preview, checkpoint-bound resume, suspension/re-execution evidence, and duplicate-resistant uncertain handling; durable discovery, assignment, authority, deadline, and audit remain | Establishes safe assignment, authority, decisions, and resume before quality and learning approvals depend on it |
| 8 | Durable agent/team inventory and read-only TeamTrace visualization | Usable foundation delivered — declared-team inventory, bounded member health, supervision evidence, browser-scoped Team Run Desk, selected-run live follow, coordination evidence, and connected/incomplete TeamTrace states; durable discovery and team lifecycle remain | Exposes the already-shipped Agent Fabric before adding editing complexity |
| 9 | Visual team creation for delegate and fan-out | Usable launch and reusable-structure foundation delivered — selected-group roster, per-role pinned contract, topology-score blueprints, safe structural import/export, live-roster drift gates, stable retry/deduplication identity, bounded effect/context preflight, fan-out policy, explicit acknowledgement, and direct evidence handoff; server-persisted team definitions and topology editing remain | Delivers the most common multi-agent patterns first |
| 10 | Race, quorum, supervision, recovery, and team preflight | Partial — race effect admission, quorum threshold/resolver, cancellation/waste guidance, exact receipt checks, direct evidence handoff, bounded run reconciliation, and stale-evidence recovery delivered; supervision control, operator recovery actions, topology-wide preflight, and replay remain | Completes safe multi-agent construction |
| 11 | Evaluation experiment workspace and comparison report | Partial — evidence-led run comparison plus exact-pair page-memory verdict docket and bounded review export delivered; durable reviews/datasets, evaluators, experiment execution, statistical reports, and version-attributed quality gates still need platform APIs | Converts the existing quality library into a product workflow |
| 12 | Failure clusters, annotation queues, and release gates | Platform API needed | Closes the human quality loop |
| 13 | Learning candidate inbox, proposal foundry, promotion, canary, and rollback | Usable governance foundation delivered — immutable dossiers, guided prompt/policy/tool proposal creation with exact content seals and finalized evidence, replay-fixture evaluation, exact scoped approval, serving pointers, and rollback; automatic and memory-set distillation, drift automation, attributed authority, and complete policy activation remain | Delivers governed self-improvement on top of identity, evidence, review, and evaluation |
| 14 | Environment, deployment, and fleet surfaces | Not started | Builds operations on stable identity, version, quality, and audit concepts |

If a required server contract is actively changing, work proceeds on the highest-value independent Studio slice rather than inventing a temporary API or blocking visible progress.

## Experience architecture

The current zero-build Studio proved the contracts quickly. The next stage should preserve its instant local launch while making the UI maintainable.

Target architecture:

- Static, self-hostable application served by `rusty-agent-server` or the existing local proxy.
- Framework choice made only after a short migration spike; no runtime dependency is added merely for styling.
- Feature modules aligned to workspace surfaces, with a shared API client, state model, router, design tokens, and test utilities.
- A versioned capabilities document exposed by `GET /info`, covering supported routes, feature-contract versions, optional evaluators, authentication mode, and service limits. The current information response is insufficient for reliable negotiation and must be extended before Studio relies on it.
- Generated or hand-validated wire types pinned to server contracts.
- URL-addressable agent, version, thread, run, experiment, candidate, and deployment views.
- Local preferences separated from durable platform state; browser storage never masquerades as the source of truth.
- Progressive loading, cancellable requests, bounded polling/stream reconnect, and explicit stale-data indicators.
- Sensitive values redacted by default. Local Studio keeps them in memory unless the user explicitly chooses persistence after a warning; deployed Studio uses a server-side session or equivalent credential boundary.

The migration should happen incrementally behind the existing local launch command. A rewrite that pauses product delivery is not a milestone.

## Visual and interaction direction

Rusty should feel like an instrument for operating a live system: precise, grounded, and calm under failure.

- Preserve the current industrial rust/amber identity, but reserve bright color for state and risk rather than decoration.
- Use the evidence rail as the signature element across Build, Run, Evaluate, and Learn.
- Prefer readable topology, causal paths, and state transitions over decorative node canvases.
- Keep dense evidence available without making the default path feel like an operations console.
- Motion communicates execution, transfer, and state change; reduced-motion users receive an equivalent static state.
- Every status combines text, shape, and color. Color never carries meaning alone.

## Competitive baseline

Rusty does not need feature parity for its own sake, but it must meet the interaction expectations established by mature platforms.

| Platform experience | Baseline Rusty must meet | Rusty opportunity |
|---|---|---|
| [LangSmith Studio](https://docs.langchain.com/langsmith/use-studio) | Run agents, manage assistant configurations and versions, manage threads, inspect graph/chat execution | Join configuration, replay eligibility, effect safety, and local evidence without requiring a hosted control plane |
| [LangSmith observability and experiments](https://docs.langchain.com/langsmith/observability-studio) | Move from traces to prompt iteration, datasets, and experiments | Make the evidence chain deterministic and promotion-aware from the runtime upward |
| [Microsoft Agent Framework DevUI](https://learn.microsoft.com/en-us/agent-framework/devui/) | Quickly discover, run, and visually debug local agents and workflows | Match the low-friction local loop, then extend it into production-grade durability and governance |
| [OpenAI Agents SDK tracing](https://openai.github.io/openai-agents-python/tracing/) | Trace agents, model calls, tools, handoffs, guardrails, and custom spans | Add causal replay, effect receipts, cross-agent recovery, and governed learning as first-class interactions |

## Measurement

Product metrics are measured against real local and production-shaped workflows, not screenshots.

| Journey | Target |
|---|---:|
| Open Studio with a supported local server running → first successful run | under 5 minutes |
| Create a common single agent without raw JSON | 100% of required fields |
| Find a retained run by agent/status/time | under 30 seconds |
| Identify causal failure and last safe recovery point | under 2 minutes |
| Fork, change one configuration value, and compare | at most 5 deliberate actions |
| Understand why a gate failed | affected cases and evidence visible in one view |
| Review a learning candidate | change, evidence, evaluation, authority, and rollback visible before action |
| Keyboard-complete primary journeys | 100% |
| Supported responsive width | 360 px and above without hidden primary actions |

Operational quality budgets:

- No unhandled promise rejection or uncaught browser exception in a supported journey.
- No indefinite loading state; every request has completion, cancellation, retry, or failure guidance.
- No destructive action without target, consequence, and confirmation.
- No silent client/server schema downgrade.
- Rendering remains responsive for production-shaped traces and lists through virtualization or bounded pagination where needed.

## Definition of done for every slice

A Studio slice is done only when all of the following are true:

- The journey solves one observable user problem end to end.
- It uses real server contracts and handles older or incompatible servers honestly.
- Loading, empty, success, partial, offline, authorization, validation, and server-error states are designed.
- Desktop, narrow mobile, keyboard, screen-reader naming, focus, contrast, and reduced-motion behavior are verified.
- Interaction invariants and regressions have automated tests.
- The flow is exercised against a real local Rusty server, not only fixtures.
- Changed Rust crates pass their tests and warning-free linting; Studio suites and repository CI pass.
- Documentation describes the user workflow and its actual limitations.
- An independent correctness and UX review has no unresolved material findings.

Completing one slice never means the Studio roadmap is complete. Roadmap status is reported by user outcome: **not started**, **foundation**, **usable**, **robust**, or **complete**.

## Decision log

| Decision | Reason |
|---|---|
| Rusty Studio is a first-class product layer, not a demo utility | The runtime's differentiators only matter when people can understand and control them |
| Evidence is the shared navigation model | It connects building, debugging, evaluation, learning, and operations in a way competitors do not fully unify |
| Visual construction must round-trip to typed runtime contracts | A canvas that loses detail or invents behavior is unsafe |
| Learning appears after evaluation in the primary journey | Rusty's governing rule is replay and evidence before promotion |
| Local-first remains a release requirement | It is a practical adoption advantage and keeps private agent data under user control |
| Architecture migration is incremental | Product progress and verification must continue while maintainability improves |
| Signed proof is narrower than quality or external truth | A cryptographic chain can establish evidence integrity and local signer provenance without overstating model correctness, provider honesty, or remote attestation |
