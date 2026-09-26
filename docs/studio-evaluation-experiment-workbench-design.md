# Studio Phase 3 — Evaluation and Experiment Workbench

## Product outcome

Evaluation is one continuous lane inside **Work**. A person can turn exact run evidence into reviewed cases,
publish those cases as an immutable dataset version, challenge one exact candidate, compare paired outcomes,
open the source trace for a regression, and save a release gate only after reviewing the policy it binds.

The primary experience is not a collection of dataset, experiment, and gate admin forms. Those are durable
objects behind one user task: **prove that the next version is better**.

## Experience

The Evaluate stage presents four connected steps:

1. **Dataset** — publish the reviewed cases currently held in the Work session.
2. **Run** — choose an immutable dataset and a real candidate from Rusty's catalog, then set repetitions and
   concurrency.
3. **Compare** — follow execution status and review baseline-to-candidate metrics plus a paired row for every
   case. Each row links back to the exact source run from which the case was created.
4. **Gate** — name the protected release target, disclose the complete policy, require a fresh acknowledgement,
   and persist Rust's allow/block decision.

The lane is embedded in Work so trace review, case authoring, experimentation, and release evidence stay in
one context. Empty, running, failed, cancelled, complete, and unavailable states are distinct. Mobile layouts
preserve the same order and evidence without introducing a second workflow.

## Durable contract

All evaluation records use the shared `ServerStore`, so JSON-file and PostgreSQL deployments have the same
tenant isolation and restart behavior.

### Datasets

- `POST /datasets` publishes `{name, version, cases}`.
- Every case is validated by `rusty-eval::Dataset`; the server also proves that its run belongs to the
  current tenant, matches the exact thread, agent, frozen input, and terminal state, then replaces the
  client timestamp with the run's stable acceptance time. Provenance includes:
  `run_id`, `thread_id`, `agent_id`, and `captured_at`.
- A dataset version is immutable. Repeating the same exact publish converges; reusing the identity for different
  cases conflicts.
- Admission is bounded to 100 cases and 512 KiB of complete provenance plus evaluation content. With the
  20-repetition ceiling, one experiment can schedule at most 2,000 runs per arm.
- `GET /datasets` exposes the 200 most recent durable summaries. Exact version and `/cases` reads remain
  addressable by name/version, so older evidence is not lost merely because it leaves the browsing window.

### Experiments

- `POST /experiments` binds a stable experiment ID to an exact dataset version, exact candidate ID, repetition
  count, concurrency, metric, and comparison thresholds.
- The server refuses to claim execution unless the application configured a `StudioExperimentEvaluator`.
- The evaluator returns canonical baseline and candidate `rusty-eval::ExperimentReport` values. The server
  recomputes their summaries, repetition indices, case sets, dataset identity, and requested concurrency,
  computes the paired `ComparisonReport` with `rusty-eval`, and
  persists the complete result.
- `GET /experiments` returns at most 200 recent summaries; `GET /experiments/{id}` loads one selected report.
  Complete evidence is capped at 6 MiB so a large history cannot make the catalog unreadable.
- `GET /experiments/{id}/report` exposes the paired reports and comparison only after completion.
- `GET /experiments/compare?baseline=…&candidate=…` compares two complete experiment candidates through
  Rust's comparison semantics when their dataset versions match.
- `POST /experiments/{id}/cancel` requests cancellation from the server instance that owns the execution.
- A queued/running record carries a durable, renewed ownership lease. Another server respects a live owner;
  an expired owner is projected as failed and cannot silently invite reuse of the same experiment identity.

`EvalStudioExperimentEvaluator` is the supplied adapter for memory-set candidates. Prompt, tool, model, and
other candidate kinds require an application evaluator that can apply that candidate to the actual runnable
graph. The server fails explicitly when that capability is absent; it never compares two identical executions
and calls one a candidate.

### Gates

- `POST /gates` accepts a complete `rusty-eval::GatePolicy`, the exact completed experiment, protected target,
  and `acknowledged: true`.
- Rust evaluates the gate; Studio does not manufacture a decision in the browser.
- Gate identities are immutable. An exact repeated save converges to the original timestamp and decision;
  changing policy or evidence under the same name conflicts.
- `GET /gates` exposes the 200 most recent durable decisions; `GET /gates/{name}` remains the exact lookup.

## Safety and ownership

- Dataset, experiment, candidate, and gate reads are tenant-scoped.
- Mutation receipts bind HTTP status and the complete reviewed dataset, experiment plan, or gate policy.
  Ambiguous responses reconcile that same immutable operation against authoritative detail before offering
  another attempt.
- Late responses from an old connection cannot update the current tenant's UI.
- Exact cases remain in page memory until deliberately published; changing connection clears draft selections.
- Source links prove where a case came from. They do not claim that the evaluation runner created a server Run
  journal unless the configured evaluator actually persists one.
- Cost, latency, or result fields are shown only when present in the canonical report. Unavailable is not zero.

## Validation

Release validation covers:

- immutable/convergent dataset publishing and exact provenance;
- tenant isolation and restart durability;
- real asynchronous evaluation through an application evaluator;
- paired Rust comparison, explicit cancellation state, bounded catalogs, and leased ownership recovery;
- acknowledgement-gated, Rust-evaluated, convergent gate persistence;
- connection ownership and ambiguous-mutation reconciliation in the typed UI;
- desktop and mobile layout, keyboard operation, accessible progress/status, and exact trace links;
- strict TypeScript, component tests, Rust tests/clippy, production build parity, and legacy Studio regressions.

## Remaining platform extension

Intermediate per-run progress depends on evaluator callbacks that the current `rusty-eval::ExperimentRunner`
does not expose. Until that contract exists, Studio says that it is preparing the exact paired run set, then
shows the complete matrix when every run settles. Evaluation-execution trace links likewise require an
application evaluator that persists its journals; source-run links remain available for every published case.
