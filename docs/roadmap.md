# Rusty platform roadmap

Where the platform has been, what's landing this cycle, and what's next. Crates are versioned independently (`rusty-agent-runtime` core, `rusty-server`, `rusty-worker`); phases below group work across the monorepo. Named releases: **R0.1 — Ignition**, **R0.2 — Persistence**, **R0.3 — Interop**, **R0.4 — Time Travel** (all implemented), **R0.5 — Flight Recorder**, **R0.6 — Durable Work**, **R0.7 — Agent Fabric**, **R0.8 — Rusty Learn**, **R0.9 — Capsules**, **R0.10 — Adaptation** (upcoming), and **R1.0 — Unleashed** (the stability release). History lives in [../CHANGELOG.md](../CHANGELOG.md); per-crate detail lives in each crate's README.

The forward plan (R0.5 onward) comes from the product/technical strategy review of 2026-08-07. Its sequencing rule: **replay before learning** — no learning mechanism ships before the run evidence it learns from can be faithfully recorded, evaluated, and rolled back.

The direction, sharpened by the 2026-08-08 architecture review: **Rusty is becoming a verifiable, adaptive Agent OS — not merely a faster agent graph framework.** Five planes, each mapping to releases below: the Trust Kernel (typed effects, policies, versioning — R0.5 shipped the taxonomy, R0.7 adds compile-time enforcement and the versioned run manifest), the Evidence Layer (events, checkpoints, signed receipts — R0.5/R0.6 shipped the hash-chained journal and effect receipts, R0.9 signs them), the Execution OS (budgets, scheduling, backpressure — R0.6 shipped pools/quotas/drain, R0.7 adds budget inheritance), the Agent Fabric (R0.7), and the Adaptive Plane (R0.8/R0.10 — candidates evaluated in the digital twin, promoted through governance, never self-rewriting). Two commitments from that review stand above the rest: retries, recovery, and speculation are only safe behind a **typed effect kernel** (exactly-once *business outcomes* where the effect protocol supports it — never a pretend exactly-once execution), and the effect kernel lands **before** the multi-agent and learning layers expand on top of it.

## Status at a glance

| Release | Phase | Contents | Version target | Status | Date |
|---|---|---|---|---|---|
| **R0.1 — Ignition** | Core kernel | State channels + reducers, Pregel/BSP executor, checkpoints, HITL interrupts, `Send` fan-out, `ChatModel`/`ToolExecutor`, prebuilt ReAct agent | `rusty-agent-runtime` v0.1.0 | ✅ Implemented | 2026-07-31 |
| **R0.2 — Persistence** | Durability & streaming + server Phase A | Postgres checkpointer, token streaming (`messages` mode), live example; axum server: threads, runs, SSE, auth | `rusty-agent-runtime` v0.2.0, `rusty-server` v0.1.0 | ✅ Implemented | 2026-08-05 |
| **R0.3 — Interop** | interop & distribution | MCP client, remote nodes + worker SDK, server API completion, executor tracing | `rusty-agent-runtime` v0.3.0, `rusty-server` v0.2.0, `rusty-worker` v0.1.0 | ✅ Implemented | 2026-08-05 |
| **R0.4 — Time Travel** | production hardening | WASM nodes, time-travel core + server API, Postgres server store, OpenTelemetry export, Studio UI, permissive CORS | `rusty-agent-runtime` v0.4.0, `rusty-server` v0.3.0, `rusty-otel` v0.1.0 | ✅ Implemented | 2026-08-05 |
| v0.5 (pre-1.0) | SDKs & tenancy | Python SDK (stdlib-only), TypeScript SDK (zero-dep ESM), multi-tenant auth with full isolation, live-LLM validation + calculator fix | `rusty-server` v0.4.0, `sdks/*` v0.1.0 | ✅ Implemented | 2026-08-05 |
| **R0.5 — Flight Recorder** | evidence | Canonical contracts (RunEvent, DecisionEvent, Effect, checkpoint header with `format_version`), effect journal, determinism seams, exact/live/hybrid replay, fork + branch diff, portable replay fixtures | `rusty-agent-runtime` v0.5.0, `rusty-server` v0.5.0 | ✅ Implemented | 2026-08-07 |
| **R0.6 — Durable Work** | effectively-once activities | Task queue (file + Postgres), leases + heartbeats, retry taxonomy with backoff + DLQ, cancellation propagation, idempotency keys, transactional outbox, effect receipts, worker draining, pools/quotas/version pinning, crash-recovery release proof | `rusty-agent-runtime` v0.6.0, `rusty-server` v0.6.0, `rusty-worker` v0.3.0 | ✅ Implemented | 2026-08-08 |
| **R0.7 — Agent Fabric** | durable agent teams + state scaling | Durable agent identities, versioned capability manifests, typed mailboxes, supervision, task/artifact contracts; **effect kernel v2** (marker-trait enforcement, approval boundary for irreversible effects); **versioned run manifest** (prompts, tool schemas, model config pinned per run); coordination patterns + TeamTrace; copy-on-write state, delta checkpoints, content-addressed artifacts | `rusty-agent-runtime` v0.7.0, `rusty-server` v0.7.0, `rusty-worker` v0.3.1 | ✅ Implemented | 2026-08-08 |
| **R0.8 — Rusty Learn** | governed learning | Memory record model with provenance/scopes, correction loop, candidate distillation, shadow evaluation, promotion/rollback by version pointer, executor policy plane v1 | `rusty-agent-runtime` v0.8.0, `rusty-server` v0.8.0, `rusty-worker` v0.3.2, `rusty-eval` v0.1.1 | ✅ Implemented | 2026-08-09 |
| **R0.9 — Capsules** | secure isolation + federation | WASM **Component Model** capsules (WIT worlds as language-neutral contracts), deny-by-default capability host, resource budgets (fuel/epoch interruption via Wasmtime), Cedar policy engine, policy overlays; **signed run receipts** over the hash-chained journal; MCP server bridge, A2A server/client with durable tasks/artifacts | `rusty-agent-runtime` v0.9.0, `rusty-server` v0.9.0, `rusty-worker` v0.3.3 | ✅ Implemented | 2026-08-09 |
| **R0.10 — Adaptation** | executor policy learning | Checkpoint placement, retry, timeout, worker placement, concurrency policies; **runtime digital twin** (replay with recorded effects, fault/schedule injection, counterfactual branches) for offline + shadow evaluation, drift detection, revert-to-default | `rusty-agent-runtime` v0.10.0 | 📋 Planned (gated on headroom experiment) | — |
| **R1.0 — Unleashed** | stable platform | Stable public APIs, event schema, checkpoint format, capsule manifest, migration policy; independent security review; documented capacity envelope; three production-shaped case studies | v1.0.0 all crates | 🚧 Upcoming | — |

## Implemented

### R0.1 — Ignition · core kernel — `rusty-agent-runtime` v0.1.0 (2026-07-31)

The LangGraph execution model rebuilt on tokio: state channels with per-key `Reducer`s over schema-declared, runtime-validated JSON state, graph validation when you call `GraphBuilder::compile()`, the Pregel/BSP super-step executor (*plan → parallel → barrier → merge → route → checkpoint*), versioned thread-scoped checkpoints (in-memory + JSON-file), interrupt/resume HITL, `Route::Send` dynamic fan-out, typed `GraphEvent` streaming, the minimal `ChatModel` trait with an OpenAI-compatible client, parallel `ToolExecutor`, and `react::create_react_agent`. Details: [CHANGELOG 2026-07-31](../CHANGELOG.md).

### R0.2 — Persistence · durability & streaming + server Phase A — `rusty-agent-runtime` v0.2.0, `rusty-server` v0.1.0 (2026-08-05)

Core gained the `sqlx`-backed `PostgresCheckpointer` (`postgres` feature), real token streaming (`ChatModel::chat_stream` → `GraphEvent::Token`, the LangGraph `messages` stream mode), and a live-agent example against any OpenAI-compatible endpoint. The new `rusty-server` crate implemented Phase A of the Agent-Protocol surface: threads, background/blocking/SSE runs, checkpoint history, per-thread run queue, API-key auth — 10 integration tests green. Details: [CHANGELOG 2026-08-05](../CHANGELOG.md), [server design doc](rusty-server-design.md), [server quickstart](server-quickstart.md).

### R0.3 — Interop — `rusty-agent-runtime` v0.3.0, `rusty-server` v0.2.0, `rusty-worker` v0.1.0 (2026-08-05)

Four workstreams landed concurrently this cycle:

- **MCP client** (`rusty-core/src/mcp.rs`) — call any MCP server's tools from `rusty-agent-runtime` `Tool` impls over stdio transport. MCP tool servers plug into `ToolRegistry` / `ToolExecutor` exactly like native tools, so the prebuilt ReAct agent can drive them with no graph changes.
- **Remote nodes + `rusty-worker`** (`rusty-core/src/remote.rs`, new crate) — `RemoteNode` POSTs node execution to worker services over HTTP; the `rusty-worker` SDK serves user handlers; HITL interrupts cross the wire, so a remote node can suspend the run and resume it with a human payload just like a local node.
- **Server API completion** (`rusty-server` v0.2) — fills out the Agent-Protocol surface from the [design doc](rusty-server-design.md): `GET /runs/{id}`, assistants, crons, and the KV store — 20 integration tests green.
- **Executor tracing** — `tracing` instrumentation through the super-step loop (spans per super-step, node, checkpoint), the foundation for the OpenTelemetry export candidate below.

### R0.4 — Time Travel · production hardening — `rusty-agent-runtime` v0.4.0, `rusty-server` v0.3.0, `rusty-otel` v0.1.0 (2026-08-05)

Five workstreams landed concurrently this cycle:

- **WASM nodes** (`rusty-core/src/wasm_node.rs`, feature `wasm`) — `WasmNode` runs sandboxed WebAssembly modules as graph nodes via Wasmtime: untrusted-code isolation behind the same `Node` trait, without a separate worker fleet.
- **Time travel** — core gained `Checkpointer::get_by_id` / `Checkpointer::fork_thread` and `RunConfig::with_checkpoint_id`; the server exposes them as `POST /threads/{id}/fork` (full- or mid-history forks) and `"checkpoint": {"checkpoint_id": …}` replay on all three run endpoints. Fork first, replay on the fork.
- **Postgres server store** (`rusty-server`, feature `postgres`) — `ServerConfig::with_postgres(url)` moves run checkpoints *and* the assistants/crons/KV surface into Postgres (`server_*` tables, auto-migrated on first use; migrations serialize on a transaction-scoped advisory lock, so concurrent cold boots are safe).
- **OpenTelemetry export** (new `rusty-otel` crate) — one-call tracing subscriber setup with optional OTLP span export, completing the v0.3 executor instrumentation story.
- **Studio** (`studio/`, zero-build single-file UI) — connect bar, graph/thread panels, state + checkpoint-history viewers, all three run modes, interrupt/resume, and fork/replay against the real time-travel endpoints. The server now layers permissive CORS in `router()`, so the Studio can call it cross-origin (restrict it in production). See [docs/studio.md](studio.md).

### v0.5 — SDKs & tenancy (pre-1.0) — `rusty-server` v0.4.0, `sdks/*` v0.1.0 (2026-08-05)

- **Python SDK** (`sdks/python/`) — zero-dependency, stdlib-only client (`urllib.request` + `json`): the full thread/run/SSE/time-travel/assistant/cron/KV surface, verified by an e2e suite that boots the real `server_demo` binary. This is the "interop over HTTP" story made concrete — the polyglot path the rejected PyO3/napi-rs bindings were traded for.
- **TypeScript SDK** (`sdks/typescript/`) — zero-dependency ESM client for Node ≥ 18 and browsers (global `fetch`, async-generator `runStream`), with hand-written type declarations and its own live-server e2e suite.
- **Multi-tenant auth** (`rusty-server` v0.4.0) — `ServerConfig::with_tenant_key(tenant, key)` maps API keys to tenants; threads, runs, assistants, crons, and KV namespaces are fully isolated via internal `{tenant}/` id prefixing, cross-tenant access answers 404 (never 403), and open/dev mode stays byte-identical to before.
- **Live-LLM validation + calculator fix** — `examples/live_agent.rs` verified end-to-end against real Ollama models ([transcript](live-demo-transcript.md)); the run exposed (and a follow-up run confirmed the fix for) a calculator arg-parsing defect: quoted numeric args (`"128"`) failed `as_f64()` and silently computed `0 op 0`. The example now coerces numeric strings and alias keys, logs raw args on failure, and carries 5 unit tests.

## Upcoming

### R0.5 — Flight Recorder · evidence

Rusty's first unmistakable flagship: record the effects required to explain and replay an agent run — not just observability spans.

- **Contract freeze** — canonical serde-versioned schemas: `RunEvent` (run, thread, node, sequence, input/output references, latency, cost, status), `DecisionEvent` (family, features, legal actions, selected action, propensity, policy version, outcome), the `Effect` taxonomy (`Pure` / `ReadOnly` / `Idempotent` / `Compensatable` / `NonIdempotent`), and a checkpoint header carrying `format_version`, graph version, and policy version. Golden-file tests pin the wire shapes; old checkpoints keep loading.
- **Determinism seams** — the executor sources time and randomness through injectable clock/RNG, so a recorded run can be re-driven exactly.
- **Effect journal** — model calls, tool calls, remote/WASM node calls, and human interrupts recorded with inputs, outcomes, latency, cost, and causal parentage.
- **Replay modes** — *exact* (zero outbound calls; every effect served from the journal), *live* (re-execute against current dependencies), *hybrid* (pin selected effects, re-run others).
- **Fork + branch diff** — fork at any checkpoint, change one model/prompt/tool input, compare state and event streams.
- **Portable fixtures** — export any run as a self-contained replay fixture; replay it in CI.
- **First experiment** — measure checkpoint-placement headroom after mandatory checkpoints for non-idempotent effects. If residual freedom is small, R0.10 stops investing in that wedge. Result published in [benchmarks.md](benchmarks.md).

**Release proof:** exact replay makes zero outbound calls and produces the same ordered event stream and state transitions.

### R0.6 — Durable Work · effectively-once activities

Workers evolve from remote-execution helpers into a durable activity system. The promise is effectively-once behavior when applications use idempotency — not universal exactly-once side effects.

- Postgres-backed task queue with leases/visibility timeouts; worker heartbeats, lease renewal, failure detection, safe reassignment.
- Retries with classified errors, exponential backoff + jitter, attempt limits, dead-letter queue.
- End-to-end cancellation propagation; worker draining during deployment.
- Task idempotency keys, deduplication, transactional outbox, effect receipts.
- Named worker pools, concurrency limits, tenant quotas; version pinning for in-flight runs.

**Release proof:** kill the server and a worker mid-effect; restart; the run completes without losing state or duplicating the external effect. Implemented as the automated integration test `rusty-server/tests/crash_recovery.rs` (real processes, real SIGKILLs — see [the design doc](durable-work-design.md#the-lease--visibility-timeout-model-wave-1)).

### R0.7 — Agent Fabric · durable agent teams + state scaling

- **Durable agents** — stable identity, versioned capability manifest, typed durable mailboxes (ordering, acknowledgement, idempotency, dead-letter), private state plus explicit team/user/tenant scopes, supervision with restart/escalation, deadlines, and a cancellation tree.
- **Effect kernel v2** — the R0.5 `Effect` taxonomy moves retry-safety from runtime convention into the type system: marker traits enforce that `Pure` work may be cached/speculated, `Idempotent` effects may retry, `Compensatable` effects require a registered rollback handler, and irreversible effects require an approval or commit boundary; unknown effects cannot automatically retry. Deterministic effect ids + the R0.6 receipt ledger close the loop: on recovery Rusty checks whether the effect already committed before executing again — exactly-once *business outcomes* where the effect protocol supports it, never a pretend exactly-once execution.
- **Versioned run manifest** — the checkpoint header (R0.5: format/graph/policy versions + topology hash) extends additively to pin everything that can influence a run: prompts, tool schemas, model + parameters, memory schema, capsule versions. Long-running agents survive platform upgrades; existing runs complete on pinned versions while new versions shadow.
- **RunBudgets with inheritance** — wall time, tokens, cost, tool calls, concurrency, risk level as first-class runtime objects; a supervisor's budget bounds its children's, and admission control enforces budgets at submission (composing R0.6 pools/quotas).
- **Coordination patterns with runtime guarantees** — delegate/handoff (typed task contract, scoped context transfer), fan-out/map (bounded parallelism, causal children, deterministic merge), race (idempotent candidates only, cancel losers, record wasted cost), quorum (explicit membership, evidence record, deterministic resolver).
- **State scaling** — replace full-state clones with copy-on-write/persistent structures, delta checkpoints, and content-addressed large artifacts; typed state path retained behind the JSON SDK boundary. Causal multi-agent state stays private-authoritative with immutable messages and explicit merge contracts; CRDT documents (Automerge-class) only for genuinely collaborative state, conflicts preserved as data. Measured against the published [baseline numbers](benchmarks.md) before any claim.

**Release proof:** an agent team resumes after a crash with causal history intact; state-scaling numbers published.

### R0.8 — Rusty Learn · governed learning

The learning rule: no learning process may silently rewrite a production prompt, graph, policy, memory, or tool permission. Learning creates an immutable candidate that must be evaluated and promoted.

- **Governed memory** — records carry provenance, confidence, validity interval, expiration, supersession, and scope (run/agent/team/user/tenant); retrieval with structured filters + context budget; consolidation, conflict detection, and real forgetting (embeddings, caches, dependent summaries). Vector retrieval deferred per the de-priorities below.
- **Correction loop** — human corrections become attributed candidate memories/examples.
- **Learning loop** — observe completed runs → distill candidate → replay against recorded evaluations → promote only within an approved envelope (otherwise review/canary) → monitor drift → roll back by immutable version pointer.
- **Executor policy plane v1** — policy pinning already lands in R0.5 contracts (policy version + propensity in the checkpoint header); epoch-bounded immutable policy versions; closed action sets as Rust enums; default static behavior as the floor.

**Release proof:** apply a correction, evaluate the derived candidate, promote it, and explain the later improvement — attributable and reversible.

### R0.9 — Capsules · secure isolation + federation

- **Capsule manifest** — identity, version, build digest, declared graph/node interface, typed inputs/outputs/effects, capability grants (filesystem paths, network hosts, secrets, tools, models), and resource budgets (CPU, memory, wall time, WASM fuel, tokens, cost, output size). Signing/attestation follows the MVP.
- **Deny by default** — no filesystem, network, secrets, or process access unless granted; network grants scoped by host/protocol/method; secrets injected as non-serializable handles; guest outputs validated before host actions; tenant overlays may only narrow capabilities.
- **Protocol bridges** — expose any graph as an MCP tool (generated schemas, streamed progress) and as an A2A agent (generated Agent Card); consume MCP servers and A2A agents as durable nodes, preserving tasks, artifacts, streaming, and cancellation.

**Release proof:** run an untrusted remote agent that attempts forbidden network/filesystem access and visibly deny it.

### R0.10 — Adaptation · executor policy learning

Gated on the R0.5 headroom experiment. Decision families in priority order: same-operation retry (classified failures; reformulation is never an ordinary retry), timeout/stopping (per-tool latency and hazard), equivalent-worker placement, concurrency/backpressure, side-effect-free speculation with budget, and checkpoint placement (if headroom exists). Agent/model selection is a governed semantic policy, not an automatic one. Interrupt policy is deferred (the prevented-error counterfactual is unobservable).

**Release proof:** a learned policy reduces cost or latency net of telemetry overhead at non-inferior completion, with the evaluation published.

### R1.0 — Unleashed · stable platform

- Stable public APIs, event schema, checkpoint format, capsule manifest, and migration policy.
- Independent security review of server multitenancy, secrets, protocol endpoints, and the WASM host.
- Documented capacity envelope and supported deployment topologies.
- At least three production-shaped case studies: durability, multi-agent coordination, sandboxed execution.
- No unresolved critical CI, data-loss, replay-integrity, or tenant-isolation defect.
- Also in scope: hosted multi-tenant control plane (tenant isolation already landed in v0.5 as the first brick; durable queues arrive in R0.6), graphs on a WASM target (browser/edge), and registry publishing across crates.io / npm / PyPI.

## Design principles

- **Runtime over framework** — win on execution guarantees, density, safety, and debuggability, not on the number of LLM wrappers.
- **Replay before learning** — an improvement system without faithful evidence, evaluation, and rollback is an uncontrolled mutation system.
- **Mechanical learning first** — learn decisions with dense objective signals and closed action spaces before learning semantic behavior.
- **Capabilities over trust** — tools and agents receive only the files, network destinations, secrets, compute, and budget they require.
- **Protocol-native** — interoperate across languages and vendors through MCP and A2A; Rusty is the durable runtime underneath.
- **Self-hosted by default** — a hosted control plane may come later; core durability, security, and debugging never require it.
- **Evidence over claims** — benchmark performance, replay fidelity, crash recovery, telemetry overhead, and learning benefit before marketing them.

## Deliberately de-prioritized

Until the runtime moat is proven, we do not build: a generic vector-database abstraction or RAG framework; long model-provider lists with no operational differentiation; voice/realtime media agents; a drag-and-drop no-code builder; an agent marketplace (before signed capsules exist); more ReAct variants and prompt templates; a hosted control plane (before durable workers, migrations, and self-hosting are excellent); model-weight training or open-ended self-modification.

## Explicitly rejected

- **napi-rs / PyO3 bindings** — REJECTED: they'd freeze a trait surface that's still moving and split maintenance across three ecosystems; the HTTP/SSE server is the polyglot interop layer instead.
- **`cdylib` / C ABI** — REJECTED: a C ABI over async tokio graphs leaks runtime-ownership and panic-safety problems across the boundary for near-zero demand; embed the Rust crate directly or talk HTTP.

## Design docs & references

- [rusty-server design](rusty-server-design.md) — endpoint mapping, SSE semantics, phased server roadmap (Phases A/B/C).
- [server quickstart](server-quickstart.md) — zero to a served graph with interrupt/resume over HTTP + SSE.
- [benchmarks](benchmarks.md) — published performance numbers and reproduction steps.
- [stability contract](stability.md) — what is stable, what may break, deprecation policy.
- [versioning](versioning.md) — compatibility matrix across all packages.
- [rusty-agent-runtime README](../rusty-core/README.md#roadmap) — core crate roadmap checklist.
- [rusty-server README](../rusty-server/README.md) — server endpoint inventory and status.
- [CHANGELOG](../CHANGELOG.md) — version history.
