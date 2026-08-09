# Rust Server Plugin Patterns — Precedent Research for `rusty-agent-server`

**Author role:** Rust_Server_Patterns_Researcher
**Date:** 2026-08-04
**Workspace:** `/Users/amjad.shaikh/claude-work/claude-white-papers/05 - RUST`
**Question:** In a compiled language, how does USER code (agent graphs/nodes) get into a server? Python's `langgraph.json` just imports modules at runtime — Rust cannot. This document surveys the six viable patterns, their real-world precedents, and ranks them for `rusty-agent-server` (crate: `rusty-core/`, a LangGraph-style engine with schema-declared state channels and per-key reducers, Pregel/BSP super-step execution, checkpoints, interrupts, and streaming).

---

## Pattern 1 — Library-Embedded Server ("server as a crate")

**Mechanism.** The server ships as a library crate (`rusty-agent-server`), not a binary. Users write their own `main.rs`, build their graphs with the `rusty-agent-runtime` crate, register them into a `GraphRegistry`, and call `rusty_agent_server::serve(registry).await`. Compilation statically links user code and server code into one binary. Deployment = "ship the user's binary."

```rust
#[tokio::main]
async fn main() {
    let mut registry = GraphRegistry::new();
    registry.register("support_agent", build_support_graph());
    registry.register("research_agent", build_research_graph());
    rusty_agent_server::serve(registry, "0.0.0.0:8080").await.unwrap();
}
```

**Precedents.**
- **axum/tokio itself**: the canonical Rust model — the HTTP server is a library; every production Rust service is "user main.rs + axum router." Nobody distributes an "axum server binary" you plug routes into at runtime.
- **Vector (timberio/vector)**: the topology (sources/transforms/sinks) is built in code via a `TopologyBuilder`; configs select from a compiled-in component catalog. Extending Vector with a genuinely new transform means writing Rust and recompiling.
- **DataFusion**: a library; users embed the query engine in their own binary (InfluxDB IOx, GreptimeDB, Ballista all do this).
- **TiKV**: the coprocessor accepts compiled-in Rust code; the "plugin" path (WASM coprocessor) was experimental and never became the main path.
- **graph-flow / rig**: the two most relevant Rust agent-framework precedents both assume the embedded model — you write a Rust `main.rs`, compose your graph/agents, and run your own binary ([graph-flow repo](https://github.com/a-agmon/rs-graph-llm), [rig ecosystem](https://github.com/0xPlaygrounds/rig/blob/main/ECOSYSTEM.md)). Neither ships a standalone "agent server you load graphs into."

**Pros.**
- Graph wiring, reducers, and node signatures are checked by the Rust compiler; state itself is schema-declared JSON (`StateSpec`) validated at runtime — channel conflicts surface as typed errors at the super-step barrier.
- Zero serialization boundary: node state stays in process; no JSON round-trip per super-step.
- Best performance; LLM-token streaming can flow straight from node futures into the SSE response without intermediate encoding.
- Operational simplicity: one binary, one deploy artifact, no version-skew between server and user code.
- Async correctness: nodes are `async fn` with real `tokio` — no FFI/async impedance mismatch.

**Cons.**
- No `langgraph dev`-style hot reload; every graph change = recompile + redeploy. (Mitigation: `cargo watch` / `bacon` + a dev-mode binary gives a decent inner loop; Rust compile times for an incremental change to one graph are seconds, not minutes.)
- Multi-tenant "platform" hosting (LangGraph Cloud's model: many users push graphs to one hosted server) is impossible in this pattern alone — each tenant needs their own binary/container.
- The server crate must be carefully API-stable because it appears in every user's dependency tree.

**Verdict:** The idiomatic Rust answer, and what every comparable Rust project actually does. This should be the **primary** model. The honest framing: "`langgraph.json` doesn't exist in Rust; your `Cargo.toml` *is* the langgraph.json."

---

## Pattern 2 — Worker / Delegate Model (server is pure infra; user code runs out-of-process)

**Mechanism.** `rusty-agent-server` becomes a durable execution + state + routing service. User graphs/nodes run in separate worker processes (any language) that long-poll the server for node-execution tasks over gRPC/HTTP, execute them, and post results back. The server owns checkpoints, super-step scheduling, interrupts, and stream fan-out; workers own user code.

**Precedent — how Temporal's split works exactly** (sources: [Temporal architecture overview](https://www.mintlify.com/temporalio/temporal/architecture/overview), [Temporal durable agents](https://www.mdjawad.com/posts/temporal-durable-agents/), [n8n vs Temporal — ZenML](https://www.zenml.io/blog/n8n-vs-temporal), [Temporal worker architecture](https://levelup.gitconnected.com/temporal-worker-architecture-and-scaling-af0c670ce6c1)):
- The **server** is four services: *Frontend* (stateless gRPC gateway; all client and worker traffic flows through it), *History* (persists every workflow event; replay drives execution; sharded for throughput), *Matching* (hosts **task queues** and dispatches work), plus persistence. Usually deployed as one binary for simplicity.
- **Workers are user-hosted, stateless external processes.** They connect outbound to the Frontend over gRPC (port 7233), **long-poll named task queues** (`PollWorkflowTaskQueue` / `PollActivityTaskQueue`), execute workflow/activity code locally, and post completions back. Workers need no inbound ports; scale is "add more worker pods polling the same queue."
- On Kubernetes: Temporal Server as 1–N pods + one Deployment per worker application, workers connecting outbound via gRPC.
- Same shape across the 2026 durable-execution field: **Inngest** (execution on the application's own compute, server calls out / workers serve HTTP), **Trigger.dev** (separately deployed long-running Bun workers, tasks up to 24h), **Hatchet** (Postgres-native, DAG task graphs, built explicitly for AI pipelines) — see [Inngest/Trigger/Hatchet comparisons](https://aiworkflowlab.dev/article/inngest-trigger-dev-hatchet-ai-workflows) and [PkgPulse guide](https://www.pkgpulse.com/guides/hatchet-vs-trigger-dev-v3-vs-inngest-durable-workflows-2026).

**Why it fits agent workloads specifically.** Agent nodes are dominated by LLM call latency (hundreds of ms to minutes per node). A gRPC hop of 1–5 ms is <1% overhead — the classic objection to out-of-process execution evaporates. Meanwhile the wins are large: **polyglot workers** (a Python worker can host the LangChain ecosystem while the Rust engine owns orchestration), independent scaling of GPU/tool-heavy nodes, crash isolation (a segfaulting tool node can't take down the checkpoint store), and per-node retry/timeout policy at the queue level. This is also the only pattern that gives a hosted multi-tenant platform story (LangGraph Cloud equivalent).

**Cons.**
- State must cross the wire: `rusty-agent-runtime`'s channels already hold serde-JSON in process, so the worker boundary adds no new encoding cost — but the server↔worker protocol becomes a versioned contract of its own (a versioned protobuf/JSON schema mitigates).
- Streaming is harder: token-level SSE from a node must be relayed worker→server→client (Hatchet shows streaming step outputs are doable; Temporal notably does *not* stream activity output well).
- Two artifacts to deploy; worker/server protocol versioning becomes a permanent maintenance surface.
- Distributed checkpoint semantics (who owns reducer application?) must be designed carefully — Temporal's answer (server owns history, workers are dumb executors) is the right template.

**Verdict:** The strongest **second** pattern, and arguably the long-term platform play. It converts `rusty-agent-server` from "a Rust library" into "an agent execution platform," and LLM latency makes the network hop a non-issue. Recommend as the **v1.5/v2 extension**, with the wire protocol designed early enough that Pattern 1 nodes and Pattern 2 workers share one `Node` trait abstraction.

---

## Pattern 3 — Dynamic Loading (cdylib plugins via `libloading`)

**Mechanism.** Users compile graphs as `cdylib` shared objects (`.so`/`.dylib`/`.dll`); the server `dlopen`s them at startup via the `libloading` crate and calls an exported `register_graph()` symbol.

**Why most projects reject it** (sources: [Plugins in Rust: Diving into Dynamic Loading — nullderef.com](https://nullderef.com/blog/plugin-dynload/), [rust-lang forum: ABI stability of dylib vs cdylib](https://users.rust-lang.org/t/abi-stability-guarantee-of-dylib-vs-cdylib/50879)):
- **Rust has no stable ABI.** `#[repr(Rust)]` layout and the `extern "Rust"` ABI are implementation details; the compiler may change type layout *between any two invocations*, even identical ones. A plugin compiled with a different rustc (or even different flags) than the server can silently corrupt memory.
- The only safe boundary is `extern "C"` + `#[repr(C)]` + a hand-rolled FFI contract — which means no `async fn`, no generics, no `String`/`Vec` across the boundary, manual lifetime management. `rusty-agent-runtime`'s async, generic-over-state `Node` trait cannot cross this boundary without an enormous marshalling layer.
- Ecosystem band-aids exist (`stabby`, `abi_stable`, `safer_ffi`) but add a heavy framework tax and still can't carry `async` cleanly.
- Operational hazards: no unloading safety (can't safely `dlclose` a library with running tokio tasks), no sandboxing (a plugin segfault kills the server), symbol/version skew debugging.
- This is why essentially no major Rust service (Vector, TiKV, Linkerd, DataFusion) offers native dylib plugins as a supported extension path.

**Pros:** native speed, no network hop, shares tokio runtime. **Cons:** everything above — it trades Rust's core safety guarantees away.

**Verdict:** **Reject.** Document this decision explicitly in the white paper; readers will ask.

---

## Pattern 4 — WASM Plugins (wasmtime / Extism / Envoy-style)

**Mechanism.** Users compile custom nodes to `wasm32-wasip2` (Component Model); the server embeds `wasmtime` (optionally via Extism for the plugin plumbing) and instantiates a sandboxed module per node invocation or per session. Host functions expose LLM calls, tool invocation, and state read/write as capabilities.

**Maturity in 2026** (sources: [Rust+WASM tooling ecosystem status, Gothar 2025-11](https://gothartech.com/en/insights/rust-wasm-containers-2025), [Wassette / hyper-mcp comparison](https://query.mt/posts/wassette/)):
- **wasmtime** is production-grade with full WASI 0.2 (Component Model) support; runs Spin/SpinKube in production. Extism is mature and battle-tested for exactly this "plugin in any language → run in my Rust host" use case; hyper-mcp builds its whole plugin architecture on Extism with OCI-registry distribution. Microsoft's Wassette runs WASM components as MCP servers.
- Envoy's WASM filter model proved the "host app + sandboxed user logic" pattern at infrastructure scale years ago.
- The 2026 twist: **MCP servers as the tool boundary** means WASM components increasingly target MCP rather than bespoke host ABIs — Microsoft's Wassette ([repo coverage](https://query.mt/posts/wassette/)) runs WebAssembly Components as MCP tools with capability-based permissions.

**Pros.**
- Real sandboxing: no syscalls except explicitly granted capabilities — the *only* pattern where you can run **untrusted** third-party node code in-process. Essential for any marketplace/multi-tenant "upload a node" story.
- Polyglot authoring (Rust, Go, JS, Python→WASM toolchains) with one host runtime.
- Distribution via OCI registries is now standard practice (hyper-mcp model) — a plausible `rusty publish` UX.
- Cold-start and per-invocation overhead (µs–low ms) is invisible next to LLM latency.

**Cons.**
- WASI's networking story is the friction point: direct HTTP from inside a module requires `wasi:http` (works but adds toolchain constraints); many hosts instead proxy LLM/tool calls through host functions, which is more code for the server team.
- Component Model tooling, while real, is still rougher than native cargo builds; debugging a WASM node is materially worse than debugging native Rust.
- State must still serialize across the boundary (WIT-defined types); `rusty-agent-runtime`'s free-form JSON channel values must map onto WIT records.
- Async/streaming across the WASM boundary is workable (`wasi:http` streaming bodies) but adds engineering effort to get token-level SSE through.

**Verdict:** **Adopt selectively.** Not the default authoring path, but the right answer for untrusted/community nodes and a future node marketplace. Position as a `Node` trait implementation (`WasmNode`) — Pattern 1 graphs can embed WASM nodes without architectural upheaval.

---

## Pattern 5 — Graph-as-Data / DSL (declarative YAML/JSON graph definitions + fixed node catalog)

**Mechanism.** The server ships with a compiled-in catalog of node types (LLM call, tool call, branch, human-approval, map/fanout); users declaratively wire them via YAML/JSON (`rusty.yaml` — the honest Rust analogue of `langgraph.json`). No user code is loaded at all.

**Where it works:** **n8n** and **Dify** prove the model at scale for integration-style and RAG-pipeline workflows — a visual/declarative graph over a fixed palette of nodes covers a large market ([Dify/n8n/activepieces landscape](https://github.com/underlines/awesome-marketing-data-science/blob/main/llm-tools.md?plain=1), [n8n vs Temporal — ZenML](https://www.zenml.io/blog/n8n-vs-temporal)).

**Where it breaks for code-heavy agents.**
- The escape hatch problem: the moment a user needs a custom reducer, a domain-specific router, or a non-trivial tool, declarative DSLs force them into "expression languages" inside YAML — the worst of both worlds. LangGraph's success comes precisely from nodes being *real functions with real code*; `rusty-agent-runtime`'s typed-channel/reducer model is even more code-centric.
- A fixed node catalog ossifies the framework: every new capability requires a server release.
- Versioning/validation of graph JSON against evolving state schemas is a quiet maintenance swamp.

**Pros.** Zero-compile iteration; non-Rust-developers can compose graphs; trivially pairs with a visual builder (LangGraph Studio equivalent); graph definitions are diffable/auditable artifacts. **Cons.** Ceiling on expressiveness; does not cover `rusty-agent-runtime`'s target users (systems engineers who chose Rust deliberately).

**Verdict:** **Useful complement, not a core answer.** Ship a small declarative format later for the 80% boilerplate graphs (ReAct, RAG, human-in-the-loop templates) and for Studio-style visualization — but never as the only door for user code.

---

## Pattern 6 — Hybrid: Compiled Core + Embedded Scripting for Custom Nodes

**Mechanism.** Core graphs and performance-critical nodes are compiled Rust (Pattern 1); a scripting engine is embedded for lightweight custom logic (routing predicates, prompt templates, small transforms) without recompiling.

**Precedents and options** (sources: [Zed's Rhai-based extensibility RFC](https://github.com/zed-industries/zed/discussions/40049), [Survey of Rust embeddable scripting languages](https://www.boringcactus.com/2020/09/16/survey-of-rust-embeddable-scripting-languages.html), [vx Starlark RFC](https://github.com/loonghao/vx/blob/main/docs/rfcs/0036-starlark-provider-support.md)):
- **Rhai** — pure-Rust, no-I/O-by-default sandbox, trivial embedding; Zed chose it over Lua for exactly these reasons (sandboxing by construction, clean WASM coexistence). Best fit for small pure functions (routers, mappers).
- **Lua via `mlua`** — mature and fast, but sandboxing is manual (you must strip `io`/`os` yourself) and the C runtime complicates WASM targets; Zed rejected it for this.
- **Starlark (`starlark-rust`, Meta)** — deterministic, hermetic, Python-like syntax; battle-tested by Buck2/Bazel; good when users should write config-like logic that must be reproducible. Heavier than Rhai.
- Real Rust services using this: Vector has a **Lua transform** and a WASM transform for custom logic; TiKV used embedded scripting experiments for coprocessors; Buck2's entire extension layer is Starlark.

**Pros.** No-recompile iteration for the most-changed code (routing/prompts); sandboxed by default (Rhai); keeps hot paths native. **Cons.** A second language in the stack; scripted nodes lose type checking against graph state; temptation to grow the script layer into a bad programming language ( Greenspun's law).

**Verdict:** **Adopt as a garnish, not a course.** Rhai (or Starlark if determinism/replay is prioritized) for *node-local* logic only — never for graph topology. Cheap to add behind the `Node` trait; high DX payoff.

---

## Cross-cutting: axum/tokio server stack norms for SSE streaming (2026)

Regardless of pattern, the serving layer norms are settled (sources: [axum SSE discussion #1670](https://github.com/tokio-rs/axum/discussions/1670), [MCP rust-sdk transports](https://github.com/modelcontextprotocol/rust-sdk), [SSE streaming engineering guide](https://ethosbytes.com.au/streaming-llm-responses-with-sse-the-2025-engineering-guide-for-australian-enterprises/)):
- **axum + `Sse::new(stream)`** over a `tokio::sync::broadcast`/mpsc-backed `Stream` is the canonical SSE pattern; `axum::response::sse::Event` with keep-alive.
- The **MCP Rust SDK (`rmcp`)** is the important 2026 precedent: Streamable HTTP responses are *either* a single JSON body *or* a `text/event-stream` SSE stream, handled transparently — this "one endpoint, two response modes" is now the expected shape for agent APIs. `rusty-agent-server` should mirror it: `POST /graphs/{id}/runs` returning either a completed run or an SSE stream of super-step/token events.
- Production gotchas to bake into docs: reverse proxies (NGINX/Cloudflare/ALB) buffer SSE by default — ship guidance on `X-Accel-Buffering: no`, chunked encoding, flush-per-event; corporate proxies kill long connections — support SSE `retry:` and `Last-Event-ID` resume (which `rusty-agent-runtime`'s checkpoint model makes natural: resume from checkpoint on reconnect — a genuine differentiator vs. naive LLM streaming).

---

## Comparison Matrix

| Criterion | 1. Embedded lib | 2. Worker/delegate | 3. cdylib | 4. WASM | 5. DSL | 6. Scripting |
|---|---|---|---|---|---|---|
| Type safety of user code | ★★★★★ | ★★ (wire boundary) | ★ (ABI-unsafe) | ★★★ (WIT) | ★★ (schema only) | ★★ |
| Runtime loading of new graphs | ✗ (recompile) | ✓ (deploy worker) | ✓ (unsafe) | ✓ | ✓ | partial |
| Polyglot user code | ✗ | ★★★★★ | ✗ | ★★★★ | n/a | ✗ |
| Untrusted-code sandbox | n/a | ★★★★ (process) | ✗ (segfault = down) | ★★★★★ | ★★★★★ | ★★★★ (Rhai) |
| Streaming (token SSE) fit | ★★★★★ | ★★★ (relay) | ★★★★ | ★★★ | ★★★★ | ★★★★ |
| LLM-latency overhead | none | <1% (hop) | none | <1% | none | none |
| Ops complexity | ★ (1 binary) | ★★★★ (2+ artifacts, protocol) | ★★ | ★★★ | ★★ | ★★ |
| Multi-tenant platform story | ✗ | ★★★★★ | ✗ | ★★★★ | ★★★★ | ★★ |
| Precedent strength in Rust | ★★★★★ (axum, DataFusion, rig, graph-flow) | ★★★★★ (Temporal/Inngest/Hatchet class) | ★ (universally avoided) | ★★★★ (wasmtime/Extism/Wassette) | ★★★★ (n8n/Dify) | ★★★ (Vector Lua, Buck2 Starlark) |
| Maturity cost to build | low | high | medium | medium-high | medium | low-medium |

---

## Ranked Recommendation for `rusty-agent-server`

1. **Pattern 1 — Library-embedded server (primary, ship first).** It is the idiomatic Rust answer, matches every comparable precedent (axum, DataFusion, rig, graph-flow), preserves `rusty-agent-runtime`'s compile-time-checked graph wiring and node signatures, and gives the best SSE streaming path. Deliverable: `rusty-agent-server` crate exposing `GraphRegistry` + `serve()` over axum, with an rmcp-style "JSON or SSE" run endpoint and checkpoint-resumable streams.
2. **Pattern 2 — Worker/delegate (design for it now, build second).** Define the `Node` execution contract and wire protocol (gRPC + versioned JSON/protobuf state) so the same `Node` trait has both an in-process and a remote implementation. LLM latency makes the hop free; Temporal/Inngest/Hatchet prove the shape; it's the only route to polyglot workers and a hosted platform. The key architectural rule: **the server owns checkpoints and scheduling; workers are stateless executors** — copied directly from Temporal's History/Worker split.
3. **Pattern 4 — WASM (scoped adoption).** Add a `WasmNode` behind the same `Node` trait when (and only when) untrusted/community nodes or a node marketplace become a goal. wasmtime/Extism are production-ready in 2026; MCP-component distribution (Wassette model) is the emerging norm.
4. **Pattern 6 — Rhai scripting (cheap DX garnish).** For routing predicates and prompt assembly inside otherwise-compiled graphs. Sandboxed by default, trivial to embed.
5. **Pattern 5 — Declarative graph format (later, for templates/visualization).** `rusty.yaml` as a *compiler target* for the 80% boilerplate graphs and a Studio-style UI — never the sole extension door.
6. **Pattern 3 — cdylib dynamic loading (rejected).** No stable Rust ABI, no async across FFI, no fault isolation. Document the rejection in the white paper to preempt the question.

**The one-sentence architecture:** `rusty-agent-server` is a *crate you call* (Pattern 1) built on a *protocol you can also speak* (Pattern 2), with WASM and scripting as capability-scoped escape hatches — "`Cargo.toml` is the new `langgraph.json`, and the worker protocol is the new `pip install`."

---

## Sources
- [Temporal architecture overview (docs)](https://www.mintlify.com/temporalio/temporal/architecture/overview)
- [Durable Execution for AI Agents: Temporal's Architecture](https://www.mdjawad.com/posts/temporal-durable-agents/)
- [n8n vs Temporal vs ZenML (ZenML blog)](https://www.zenml.io/blog/n8n-vs-temporal)
- [Temporal Worker Architecture and Scaling](https://levelup.gitconnected.com/temporal-worker-architecture-and-scaling-af0c670ce6c1)
- [Inngest vs Trigger.dev vs Hatchet for AI Workflows (2026)](https://aiworkflowlab.dev/article/inngest-trigger-dev-hatchet-ai-workflows)
- [Hatchet vs Trigger.dev vs Inngest: Workflows 2026 (PkgPulse)](https://www.pkgpulse.com/guides/hatchet-vs-trigger-dev-v3-vs-inngest-durable-workflows-2026)
- [Plugins in Rust: Diving into Dynamic Loading (nullderef.com)](https://nullderef.com/blog/plugin-dynload/)
- [rust-lang forum: ABI stability guarantee of dylib vs cdylib](https://users.rust-lang.org/t/abi-stability-guarantee-of-dylib-vs-cdylib/50879)
- [Rust + WebAssembly tooling ecosystem, 2025 status (Gothar)](https://gothartech.com/en/insights/rust-wasm-containers-2025)
- [Wassette / hyper-mcp: WASM components as MCP tools](https://query.mt/posts/wassette/)
- [graph-flow: LangGraph-inspired graph framework in Rust](https://github.com/a-agmon/rs-graph-llm)
- [rig ECOSYSTEM.md](https://github.com/0xPlaygrounds/rig/blob/main/ECOSYSTEM.md)
- [Building AI Agent Frameworks in Rust (Zylos Research, 2026-03)](https://zylos.ai/research/2026-03-31-rust-ai-agent-frameworks-infrastructure/)
- [Zed RFC: Rhai-based extensibility (vs Lua/WASM)](https://github.com/zed-industries/zed/discussions/40049)
- [A Survey of Rust Embeddable Scripting Languages](https://www.boringcactus.com/2020/09/16/survey-of-rust-embeddable-scripting-languages.html)
- [vx RFC: Starlark provider support (Lua/Deno alternatives analysis)](https://github.com/loonghao/vx/blob/main/docs/rfcs/0036-starlark-provider-support.md)
- [axum Discussion #1670: SSE with tokio channels](https://github.com/tokio-rs/axum/discussions/1670)
- [MCP official Rust SDK (rmcp) — Streamable HTTP + SSE transports](https://github.com/modelcontextprotocol/rust-sdk)
- [Streaming LLM Responses with SSE: 2025 Engineering Guide](https://ethosbytes.com.au/streaming-llm-responses-with-sse-the-2025-engineering-guide-for-australian-enterprises/)
- [awesome-ml llm-tools (n8n/Dify/activepieces landscape)](https://github.com/underlines/awesome-marketing-data-science/blob/main/llm-tools.md?plain=1)
