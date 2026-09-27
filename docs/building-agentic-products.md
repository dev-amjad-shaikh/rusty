# Building agentic products on Rusty — the developer guide

> You found the repo. This document answers the two questions a developer landing here actually has: **what is this, precisely**, and **how do I build a product with it**. It is the orientation layer — [docs/architecture.md](architecture.md) is the anatomy deep-dive underneath it, [docs/how-to.md](how-to.md) is the Studio step-by-step, [docs/server-quickstart.md](server-quickstart.md) is the ten-minute HTTP path.

---

## 1. What Rusty is

Rusty is a **durable agent runtime, built in Rust, that you deploy as one static binary**. Three properties are load-bearing, and everything below follows from them:

1. **A run is a graph over shared state, executed in super-steps.** You do not write an agent as a prompt plus a retry loop. You declare typed state channels with reducers, you declare nodes as async functions over that state, and the engine executes the graph in bulk-synchronous super-steps: *plan → run the active set in parallel → barrier → merge → route → checkpoint*.
2. **Durability is a primitive, not a feature.** Every super-step ends in a versioned checkpoint — in memory, JSON files, or Postgres, same code path. An interrupt is not an exception you catch; it is a state the run parks in. `kill -9` mid-run costs you nothing that was already checkpointed. Resume is the same code path as start.
3. **The server is the polyglot interop layer by design.** The core (`rusty-agent-runtime`) has no HTTP. The axum server serves your compiled graphs over HTTP + SSE, and zero-dependency Python and TypeScript SDKs — plus the Studio workspace — talk to the same server. You write your graph once, in Rust; everything else rides the wire.

The platform pieces:

| Piece | Path | What you use it for |
|---|---|---|
| Rusty Core (`rusty-agent-runtime`) | [rusty-core/](../rusty-core/) | Defining graphs, state, nodes, reducers; the prebuilt ReAct agent; MCP client; remote nodes; the `ChatModel` trait for model providers. |
| Rusty Server (`rusty-agent-server`) | [rusty-server/](../rusty-server/) | Serving compiled graphs over HTTP: threads, runs (background / blocking / SSE), checkpoint history, fork + replay, assistants, crons, KV, API-key auth, multi-tenancy. You call `rusty_agent_server::serve(registry, config)` from your own `main.rs`. |
| Rusty SDK — Python / TypeScript | [sdks/python/](../sdks/python/) · [sdks/typescript/](../sdks/typescript/) | Driving the server from your product code: create threads, stream runs, resume interrupts. Zero dependencies; each is verified by an e2e suite that boots the real server binary. |
| Rusty Studio | [studio/](../studio/) · [docs/studio.md](studio.md) | The product workspace: build and debug agents, skills, tools, connectors, knowledge, and evaluation — over the same server. |
| Rusty Eval (`rusty-eval`) | [rusty-eval/](../rusty-eval/) | Versioned eval datasets, assertions over recorded runs, experiment reports, regression detection, release gates. |
| Rusty Worker (`rusty-worker`) | [rusty-worker/](../rusty-worker/) | Serving node handlers on remote services so `RemoteNode` executes graph nodes across the wire. |
| Rusty OTel (`rusty-otel`) | [rusty-otel/](../rusty-otel/) | One-call `tracing` subscriber for executors, with optional OTLP span export. |

## 2. The mental model — five concepts, and why each exists

If you keep these five straight, every API in the repo falls into place.

**Graphs are your agents' programs.** A graph is a compiled, validated program over a `StateSpec`. The ReAct loop (`agent → tools → agent`) is not call-stack recursion — it is nodes being re-scheduled across super-steps, which is why the runaway-loop guard is a step budget, not a stack limit. You compose graphs: a node of one graph can be another graph, wrapped as a tool.

**Threads are conversations.** A thread binds a graph to a durable checkpoint sequence. State is never global; it lives on the thread. Two threads on the same graph are two independent runs with independent history — which is what makes multi-tenant serving, replay, and time-travel fork cheap: they are operations over checkpoint sequences, not over live processes.

**Super-steps + checkpoints are one durability primitive.** There is exactly one thing that persists: the checkpoint written at each barrier. Resuming is starting from the latest checkpoint. Replaying is walking the checkpoint sequence. Forking is branching it. There is no separate "save state" call to forget, and no in-memory shadow state that can silently disagree with what is on disk.

**Interrupts are a state, not an exception.** A node suspends the run by calling `NodeContext::interrupt`. The run parks with a checkpoint; the process can even exit. A human (or a policy) later posts a resume command on the same thread — often with the human's decision as input — and execution continues exactly as if nothing happened. Human-in-the-loop, approval gates, and budget pauses are all the same mechanism.

**Tools, skills, and connectors are packaging layers over nodes.** A tool is a typed function the model can call. A skill is a reusable capability (prompt + tools + graph fragment) that agents compose. A connector is a packaged integration — a model provider or an MCP server — with managed credentials. All three compile down to the same primitives; the layers exist so Studio and the server can discover, version, and serve them.

## 3. The product-building path

This is the order that works. Each stage is independently shippable — you can stop after stage 2 and embed the core in your own binary, or go all the way to stage 6.

**Stage 1 — Model the graph locally.** Write the graph as a Rust crate. Start from the prebuilt ReAct agent (`create_react_agent`) and the five examples in [rusty-core/examples/](../rusty-core/examples/): `react_agent` (scripted model, zero network), `parallel_fanout` (dynamic fan-out / map-reduce via `Send`), `human_in_loop` (interrupt + resume from a JSON-file checkpointer), `react_record_replay` (the Flight Recorder — journal every call, re-drive with zero outbound calls, byte-identical evidence), and `live_agent` (real OpenAI-compatible endpoint). Test with the scripted model — deterministic, offline, free — before you spend a token on a live one.

```text
cd rusty-core
cargo run --example human_in_loop
```

**Stage 2 — Make it durable.** Swap the checkpointer: memory → JSON files → Postgres, one constructor argument, same code path. Decide your interrupt points (`NodeContext::interrupt`) — approvals, budget limits, missing-input cases. This is the stage where Rusty stops being an agent library and becomes infrastructure: a run you can interrupt for a week and resume.

**Stage 3 — Serve it.** Move the graph into a `main.rs` that calls `rusty_agent_server::serve`. You get threads, runs, SSE streaming, checkpoint history, fork + replay, crons, and auth as one static binary. Follow [docs/server-quickstart.md](server-quickstart.md) — zero to a served graph with interrupt/resume over HTTP in ten minutes.

**Stage 4 — Drive it from your product.** Use the Python or TypeScript SDK to create threads and stream runs from your application. The SDKs are thin over the HTTP/SSE protocol — no native bindings, no codegen — so what you can do from Rust, you can do from any language with an HTTP client.

**Stage 5 — Observe and gate.** Point Studio at the server for the interactive workspace — build skills and connectors in the UI, inspect checkpoints, replay runs, watch the flight recorder. Wire `rusty-eval` into CI: versioned datasets, assertions over recorded runs, release gates. An agent that cannot be regression-tested is not a product feature; it is a demo.

**Stage 6 — Deploy.** One static binary, the checkpointer pointing at Postgres, OTLP spans going wherever your observability stack lives. That is the whole topology.

## 4. Which layer do I build on?

| You want… | Build on | Not on |
|---|---|---|
| An agent embedded in an existing Rust service, no network | Rusty Core, in-process | the server — you do not need HTTP |
| A multi-tenant agent API for web/mobile clients | Rusty Server + an SDK | hand-rolling threads/checkpoints — the server already owns that |
| Human approval gates, long-running background jobs | core or server — same interrupt/resume primitive both ways | a job queue + polling — resume *is* the queue |
| A low-code team building skills/connectors | Rusty Studio over the server | asking non-Rust engineers to compile graphs |
| Regression-tested agents in CI | `rusty-eval` over recorded runs | prompt vibes |
| Remote compute for heavy nodes | `RemoteNode` + Rusty Worker | shipping your graph as a distributed system by hand |

## 5. The docs map

| Question | Go to |
|---|---|
| How does one run flow through the engine? | [docs/architecture.md](architecture.md) — eight diagrams, named failure modes |
| How do I build an agent / skill / tool / connector in Studio? | [docs/how-to.md](how-to.md) |
| How do I serve a graph over HTTP, fast? | [docs/server-quickstart.md](server-quickstart.md) |
| How do durability, recovery, and replay actually work? | [docs/durable-work-design.md](durable-work-design.md) · [docs/recovery.md](recovery.md) |
| What are the version and stability guarantees? | [docs/versioning.md](versioning.md) · [docs/stability.md](stability.md) · [docs/releasing.md](releasing.md) |
| What is implemented, and what is explicitly rejected? | [docs/roadmap.md](roadmap.md) |
| What does it cost to checkpoint? | [docs/benchmarks.md](benchmarks.md) |
| Working code I can run in five minutes? | [rusty-core/examples/](../rusty-core/examples/) |
| The connector contract? | [docs/connector-standard.md](connector-standard.md) |
