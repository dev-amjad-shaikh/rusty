# Studio Phase 3 — Evaluation and Experiment Workbench

## Customer outcome

A person can turn a reviewed run into a durable evaluation case, publish an immutable dataset version, run a controlled experiment against an immutable agent version, monitor progress, compare it with a baseline, investigate regressions with exact trace links, and save a fully reviewed gate.

Evaluation remains the third Work stage. There is no new primary Evaluations or Datasets destination.

## Current backend state

The runtime (`rusty-eval`) already implements canonical datasets, reports, comparisons, statistics, clustering, and gate semantics. The server has candidate evaluation at `POST /learn/candidates/{candidate_id}/evaluate`, which reads a dataset version from a server-local directory source configured at startup.

What is missing for the Studio product journey:

- A tenant-scoped dataset store with named dataset versions created from Studio cases.
- A durable experiment record (configuration, baseline/candidate bindings, progress, report reference).
- REST endpoints to create/list/get datasets and versions.
- REST endpoints to create/list/get experiments, read progress, and retrieve reports.
- A comparison endpoint (or report-driven comparison) for baseline/candidate pairing.
- A gate store and endpoints to save/list gates with full policy review.

Without these endpoints the UI cannot meet Phase 3 acceptance. Phase 3 therefore begins with a small, coherent server surface and then wires it into the existing Work Evaluate stage.

## Proposed server surface (smallest coherent set)

### Datasets

- `POST /datasets` — create or append to a dataset from a list of exact run-provenance cases.
  Body: `{ name, cases[], base_version? }` where each case carries `run_id`, `thread_id`, `agent_name`, `objective`, `pointer`, `expected`, optional `tags`, optional `redactions`.
  Response: `{ name, version, created, case_count, digest }`.
  Validation rejects invalid UTF-8, duplicate typed JSON fields, lone surrogates, excessive depth/nodes, invalid typed integers, and Rust-incompatible canonicalization by delegating to `rusty-eval` `Dataset` serialization.

- `GET /datasets` — list dataset names.
- `GET /datasets/{name}` — list versions newest first.
- `GET /datasets/{name}/versions/{version}` — fetch one immutable version.
- `GET /datasets/{name}/versions/{version}/cases` — fetch cases (optional pagination by cursor).

### Experiments

- `POST /experiments` — create an experiment bound to exact agent/deployment version, dataset version, evaluator config, runs per case, concurrency, deterministic controls, baseline candidate id, and thresholds.
  Response: `{ experiment_id, status: queued, queued_cases }`.
- `GET /experiments/{experiment_id}` — status, progress matrix, latest report reference.
- `GET /experiments/{experiment_id}/report` — canonical report.
- `POST /experiments/{experiment_id}/cancel` — cancellation where supported.

### Comparisons

- `GET /experiments/compare?baseline={id}&candidate={id}` — Rust-authoritative paired comparison with shared dataset coverage, pass rates, win/loss/unchanged, latency/token/cost only where observed, and exact trace links.

### Gates

- `POST /gates` — save a gate only after complete policy review.
  Body: `{ name, blocked_target, metric, threshold, min_evidence, unavailable_evaluator_behavior, require_approval, dataset_version, baseline_experiment_id? }`.
- `GET /gates` — list gates.
- `GET /gates/{name}` — read gate policy and latest decision.

## UI journey

### Evaluate stage (existing)

Keep the current Work Evaluate stage but replace the page-memory dataset with durable dataset publishing:

1. Review frozen input from the exact run and complete journal.
2. Show selected artifact, tool trajectory, source event.
3. Write expected answer and acknowledge.
4. Add to an existing dataset or create a new one.
5. Publish immutable version.

### Dataset workbench

Contextual route: `/work/evaluations/datasets/{datasetId}/versions/{versionId}`

- List cases with run/evidence links.
- Show conflict review before sealing a new version.
- Export portable JSONL as advanced action.

### Experiment workbench

Contextual routes:
- `/work/evaluations/experiments/{experimentId}`
- `/work/evaluations/compare?baseline=:id&candidate=:id`

- Configure experiment: pick dataset version, candidate/baseline, runs per case, concurrency, thresholds.
- Show estimated run count and unknown cost honestly.
- Progress matrix: queued, running, passed, failed, evaluator error, cancelled, unavailable.
- Comparison: paired lane per case, output/tool diffs, trace links.

### Gate designer

Contextual route: `/work/evaluations/gates/{gateName}`

- Review complete policy before saving.
- Show latest decision if any.
- Fail-closed when evaluator is unavailable.

## Files planned

- `rusty-server/src/evaluations.rs` (new) — dataset/experiment/gate store and routes.
- `rusty-server/src/server_store.rs` — persistence helpers.
- `rusty-server/src/routes.rs` — route registration.
- `rusty-server/tests/evaluations.rs` (new) — server tests.
- `studio/ui/src/lib/api/evaluations.ts` (new) — typed client.
- `studio/ui/src/features/work/evaluate/**` (new) — Evaluate stage refactor and workbench components.
- `studio/ui/src/router.tsx` — new contextual routes.
- `docs/studio-evaluation-experiment-workbench-design.md` (this file).

## Implementation status

The server surface and a first UI slice have been committed on the
`feat/studio-evaluation-experiment-workbench` branch.

### What landed

- Datasets: exactly the endpoints above are implemented in
  `rusty-server/src/evaluations.rs`, persisted as canonical JSONL under
  `{store_path}/evaluations/datasets/{tenant}/{name}/{version}.jsonl`.
- Experiments: durable experiment records with `POST /experiments`,
  `GET /experiments`, `GET /experiments/{id}`, and
  `GET /experiments/{id}/report`. The actual evaluation currently reuses
  the existing candidate evaluator path; the Studio workbench stores the
  configuration and can hold a captured report for later reference.
- Gates: durable gate policy storage with `POST /gates`, `GET /gates`,
  and `GET /gates/{name}`.
- Rust integration tests in `rusty-server/tests/evaluations.rs` cover
  dataset creation/listing/case retrieval, duplicate-version rejection,
  tenant isolation, and experiment/gate record round-trips.
- Studio UI: the Evaluate stage now hosts `DatasetPublisher` and
  `ExperimentWorkbench` components for publishing dataset versions,
  selecting them, saving experiment configurations, and saving gate
  policies. Dedicated contextual routes for the dataset/experiment/gate
  workbench remain for a follow-up refinement.
- Typed client in `studio/ui/src/lib/api/evaluations.ts`.

### Deferred / follow-up

- `GET /experiments/compare` returns `501 Not Implemented` until a
  dedicated comparison surface is needed.
- Running an experiment directly from the workbench requires wiring the
  candidate evaluator into the server surface; the current path keeps
  evaluation execution in the existing learning plane while the
  workbench owns the durable configuration and report references.

## Acceptance blocked on

The server surface above must exist before the UI can satisfy Phase 3 acceptance. This stream will implement the server surface first, then the UI, then tests, build, merge.
