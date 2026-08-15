# Rusty capability harness

Status: implementation design for the first end-to-end harness foundation.

## Product outcome

Rusty Studio must let a person choose a behavior, see the actions that behavior can actually execute, create an agent, run it, and inspect the resulting model and tool evidence. A tool shown in Studio must exist in the exact runtime graph. A runtime tool that is not advertised must not be presented as configured. Free-form names are requirements, not capabilities, and do not belong in the primary creation path.

The first shipped slice is complete only when this journey works against the real demo server:

1. `GET /info` returns each graph and its executable tool catalog.
2. Agent Builder derives the tool section from the selected graph instead of accepting invented tool names.
3. The reviewed version stores the same tool names and effect classes that the graph advertises.
4. Starting the agent executes the registered graph and at least one of those tools.
5. Trace shows the journaled tool call and result.

This slice does not claim that arbitrary assistant metadata dynamically rewires a compiled graph. The registered graph remains the runtime authority. Later slices add immutable per-run capability composition without changing this truth.

## Vocabulary

Rusty uses five distinct concepts. They must not be collapsed into one generic plugin list.

- **Tool** — a typed action the model may call. It has a stable name, description, JSON input schema, effect class, cancellable execution path, and canonical JSON result.
- **Connector** — a lifecycle-managed provider of tools or resources. Native Rust, MCP, HTTP/SaaS, and remote-agent connectors are implementations behind the same boundary. Connectors own health, authentication, discovery, and shutdown; they do not grant an agent authority by themselves.
- **Skill** — versioned procedural knowledge, normally a `SKILL.md` package plus optional references, scripts, and assets. Skills alter model context through progressive disclosure. They do not execute actions or carry credentials.
- **Knowledge source** — governed facts and documents that retrieval may cite. It is distinct from procedural skills and from short-term thread memory.
- **Capability set** — the immutable, content-addressed composition selected for one agent version and resolved for one run: model, tools, connector generations, skills, knowledge sources, memory policy, guardrails, approvals, and budgets.

The architectural rule is simple: tools and connectors provide action; skills and knowledge provide context; a capability set composes both under policy.

## Lessons adopted from other harnesses

### DeepSeek Harness

Adopt:

- replaceable capability seams rather than a central god object;
- deterministic registration and reversible lifecycle effects;
- durable `tool/call` before execution and immutable `tool/result` after normalization;
- a policy/approval/timeout/retry pipeline around the tool body;
- model-visible state reconstructable from the durable session log;
- exact capability generation inherited by child agents instead of re-resolving floating names.

Do not copy:

- preview-stage APIs or TypeScript-specific plugin machinery;
- plugin indirection where Rust traits and owned registries already provide the seam.

### Hermes Agent

Adopt:

- progressive-disclosure skill packages;
- explicit installation provenance, digest, trust level, and security scan result;
- isolated execution backends for file/process tools;
- toolsets as a discovery and authoring convenience, not a new execution primitive.

### CrewAI

Adopt its useful taxonomy: local tools, remote/MCP tools, SaaS apps, skills, and knowledge are different user concepts even when action capabilities eventually become model tool schemas.

Do not copy implicit or stringly composition. Rusty keeps typed effects, content addresses, tenant isolation, and exact receipts.

### OpenAI Agents SDK and LangGraph

Adopt a small public harness surface: agent, tool, handoff, guardrail, session, and trace. Keep durable graph execution, checkpointed threads, human approval, and long-term stores below that surface.

### Rig and Anda

Adopt provider-neutral traits, serializable run state, scoped runtime context, and offline cassette/provider tests. Rusty already has stronger journal and effect semantics; the new layer composes them rather than replacing them.

## Existing Rusty foundations

Rusty already provides the difficult lower layers:

- `Tool`, `ToolRegistry`, parallel `ToolExecutor`, middleware, and effect admission;
- ReAct graph construction and OpenAI-compatible/model-provider adapters;
- MCP tools adapted into the same `Tool` contract;
- graph execution, checkpointing, cancellation, durable pending runs, and HITL;
- journaled model/tool/remote/capsule effects, receipts, replay, and branch diff;
- registry pins for prompts, tool schemas, models, and middleware;
- capsule capability grants, brokered credentials, deployment revisions, gates, and evaluation.

The gap is composition and product truth. Today a compiled graph owns its real tools while Studio stores free-form `studio_intent.tools` text that the generic assistant runner does not enforce. The first slice closes that visible gap by making the graph advertise its real tool registry.

## Architecture

### 1. Executable tool catalog

`ToolCapability` is derived from a real `Tool` instance:

```text
name + description + parameters_schema + effect
```

`ToolRegistry::capabilities()` returns a stable name-sorted list. The catalog is derived, never separately authored, so schema/effect drift cannot produce two truths.

`GraphRegistry` stores an optional catalog beside the compiled graph and state specification. Existing `register` remains tool-agnostic and reports an empty catalog. `register_with_tools` receives the same `ToolRegistry` used to build the graph and snapshots its derived catalog.

`GET /info` adds `tools` to every graph. The HTTP API is additive. Studio validates bounds and exact effect values before trusting the catalog.

### 2. Safe built-in capability pack

Reusable native tools live under `rusty_agent_runtime::tool::builtins`:

- `CalculatorTool` — pure bounded arithmetic;
- `TextInspectorTool` — pure word/line/character statistics;
- `KnowledgeSearchTool` — read-only bounded lexical search over an immutable document set;
- `SandboxedDocumentReaderTool` — read-only UTF-8 access under one canonical root for text, Markdown, JSON, CSV, HTML, and XML, with path traversal, symlink escape, binary content, and byte ceilings rejected.

The pack is deterministic and requires no credentials or internet, so it can prove the harness in CI and in the local Studio. Web search is deliberately a connector, not a hidden network call inside a built-in tool. A later connector slice supplies search providers through the same tool contract and credential broker.

### 3. Studio creation

Selecting a behavior sets the new draft's tool contracts from that graph's catalog. The Tools step becomes a read-only capability review:

- human label and description;
- effect boundary;
- input fields derived from the schema at a summary level;
- explicit empty state when the graph has no tools.

The editor does not offer “Add tool” for arbitrary names. Existing stored definitions continue to round-trip without silent rewriting; changing the behavior deliberately refreshes the tool set from the new graph.

### 4. Runtime truth

In this foundation, the compiled graph is the enforcement boundary. The assistant version records the catalog snapshot for review, but the graph registry remains authoritative at execution. Studio says “Included by this behavior,” not “dynamically installed.”

The next runtime-composition slice will resolve an immutable `CapabilitySet` at run admission, pin its generation into `RunManifest`, and hand the exact selected set to model schema generation and tool dispatch. Until that lands, Studio will not expose optional per-tool toggles.

### 5. Evidence

The existing recording path remains authoritative:

```text
assistant version
  -> registered graph + tool catalog
  -> thread + version-exact run admission
  -> model_call
  -> tool_call (typed effect + arguments + result)
  -> final run
  -> Studio Trace/Evaluate
```

No new shadow telemetry store is introduced.

## Failure-mode decisions

- Duplicate tool names still replace inside `ToolRegistry`; the derived catalog contains exactly the executable winner.
- Catalog order is deterministic by tool name.
- Invalid tool names, empty/hidden descriptions, non-object schemas, and oversized schemas fail catalog derivation before advertisement.
- Older servers without graph tools remain connectable only where the API-version policy permits; Studio represents their tool catalog as unavailable/empty, never as invented capabilities.
- A graph with no tools creates a legitimate tool-free agent.
- Selecting a different behavior is an explicit configuration change and replaces the draft tool list with that behavior's catalog.
- Existing versions with requirements not advertised by the current graph stay intact and visibly unresolved; Studio does not silently delete them.
- File tools canonicalize both root and target, reject targets outside the root, reject non-files, reject unsupported extensions, enforce the byte limit before allocation, and require valid UTF-8.
- Search results are bounded in document count, hit count, excerpt length, and output bytes.
- Tool failures remain structured tool results through the existing failure-isolating executor; they do not crash a batch.

## Subsequent complete slices

1. **Connector plane** — connector manifests, tenant-scoped instances, health/auth lifecycle, MCP and HTTP search providers, credential handles, and catalog generations.
2. **Skill plane** — `SKILL.md` package parser, progressive disclosure, references/assets, provenance, security scan, immutable versions, and skill registry UI.
3. **Resolved capability sets** — content-addressed per-agent composition, exact run-admission resolution, model/tool filtering, checkpoint pins, child-agent inheritance, and replay.
4. **Knowledge plane** — governed sources, ingestion, chunk/content addresses, hybrid retrieval, citations, corrections, retention, and Studio knowledge workspace.
5. **Harness SDK** — concise Rust builders for agents, connectors, skills, capability sets, sessions, handoffs, and guardrails, plus cassette-backed conformance tests.
6. **Ecosystem** — signed packages, registry/taps, trust policy, compatibility negotiation, observability dashboards, and release gates.

Each slice must end in a real user journey and exact evidence. None should merge as an isolated catalog, caption, or test scaffold.

## Primary research sources

- DeepSeek Harness: [repository](https://github.com/deepseek-ai/deepseek-harness), [architecture](https://github.com/deepseek-ai/deepseek-harness/blob/master/docs/architecture.md), [tool execution pipeline](https://github.com/deepseek-ai/deepseek-harness/blob/master/docs/tool-execution-pipeline.md), and [capability seams](https://github.com/deepseek-ai/deepseek-harness/blob/master/docs/capability-seams.md).
- Hermes Agent: [repository](https://github.com/NousResearch/hermes-agent), [tools](https://hermes-agent.nousresearch.com/docs/user-guide/features/tools/), and [skills](https://hermes-agent.nousresearch.com/docs/user-guide/features/skills/).
- CrewAI: [agent capabilities](https://docs.crewai.com/en/concepts/agent-capabilities), [skills](https://docs.crewai.com/en/concepts/skills), [tools](https://docs.crewai.com/en/concepts/tools), and [memory](https://docs.crewai.com/en/concepts/memory).
- OpenAI Agents SDK: [overview](https://openai.github.io/openai-agents-python/), [tools](https://openai.github.io/openai-agents-python/tools/), [MCP](https://openai.github.io/openai-agents-python/mcp/), and [guardrails](https://openai.github.io/openai-agents-python/guardrails/).
- LangGraph: [overview](https://langchain-ai.github.io/langgraph/index.html), [tools](https://langchain-ai.github.io/langgraph/agents/tools/), [memory](https://langchain-ai.github.io/langgraph/how-tos/memory/manage-conversation-history/), and [threads](https://langchain-ai.github.io/langgraph/cloud/concepts/threads/).
- Rust-native references: [Rig](https://github.com/0xPlaygrounds/rig), [Anda](https://github.com/ldclabs/anda), [Anda Brain](https://github.com/ldclabs/anda-brain), [KIP](https://github.com/ldclabs/KIP), and [BAML](https://github.com/BoundaryML/baml).
