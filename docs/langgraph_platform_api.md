# LangGraph Platform / Agent Server — HTTP API Reference Spec

**Purpose:** design reference for `rusty-agent-server` (Rust). Covers the commercial
LangGraph server surface — formerly "LangGraph Platform" / `langgraph-api` / "LangGraph
Server", now branded **LangSmith Deployment** with the runtime called the **Agent Server**.

Researched: 2026-08-04. Primary sources: docs.langchain.com (LangSmith docs, OpenAPI-backed
endpoint reference), langchain-ai/agent-protocol GitHub repo, LangChain forum/GitHub issues.

---

## 0. Product naming & positioning

| Era | Name | Notes |
|---|---|---|
| 2024–2025 | LangGraph Platform / LangGraph Server / `langgraph-api` | Commercial Docker image `docker.io/langchain/langgraph-api:<py-version>` |
| 2025–2026 | **LangSmith Deployment**, runtime = **Agent Server** | Same product, dual-named during rename |

The Agent Server serves an OpenAPI doc per deployment at `/docs`
(e.g. `http://localhost:8124/docs`) and the raw spec at
`https://docs.langchain.com/langsmith/agent-server-openapi.json`
([source](https://docs.langchain.com/langsmith/server-api-ref)).

LangChain also published **Agent Protocol** — an open HTTP+SSE spec centered on
Runs / Threads / Store — and states "LangGraph Platform implements a superset of this
protocol" ([github.com/langchain-ai/agent-protocol](https://github.com/langchain-ai/agent-protocol)).
The open protocol is the interoperability contract third-party servers (aegra, skein-js)
re-implement so the LangChain SDKs (`langgraph_sdk`, `@langchain/langgraph-sdk`, `useStream`,
Agent Chat UI, LangGraph Studio) work unmodified against them.

API resource groups (from the official reference sidebar,
[source](https://docs.langchain.com/langsmith/server-api-ref)):

- **Assistants** — configured instances of a graph
- **Threads** — accumulated outputs of a group of runs
- **Thread Runs** — invocations of a graph/assistant on a thread
- **Stateless Runs** — invocations with no state persistence
- **Crons** — periodic runs on a schedule
- **Store** — persistent key-value store for long-term memory
- **A2A** — Agent-to-Agent protocol endpoints (`/a2a/{assistant_id}`, agent cards)
- **MCP** — Model Context Protocol endpoints
- **System** — health checks, server info, metrics
- **Streaming** — thread-centric streaming (SSE + WebSocket command/event surface)

---

## 1. Core resources & data model

### 1.1 Graph (implicit resource)
Compiled `langgraph.graph.graph.CompiledGraph` (or factory function returning one),
registered by name via `langgraph.json` `graphs` map: `{"agent": "./pkg/file.py:variable"}`.
A graph name is a valid `assistant_id` anywhere the API takes one — the server auto-creates
a default assistant per graph
([CronCreate schema](https://docs.langchain.com/langsmith/agent-server-api/crons/create-cron.md)).

### 1.2 Assistant
"A configured instance of a graph" — graph + config + context + metadata, versioned.

```json
{
  "assistant_id": "3c90c3cc-... (uuid)",
  "graph_id": "agent",
  "config": {"tags": ["..."], "recursion_limit": 25, "configurable": {}},
  "context": {},
  "metadata": {},
  "name": "Untitled",
  "description": null,
  "version": 1,
  "created_at": "...", "updated_at": "..."
}
```
Create payload: `graph_id` (required), optional `assistant_id`, `config`, `context`,
`metadata`, `name`, `description`, `if_exists: "raise"|"do_nothing"`.
([Create Assistant](https://docs.langchain.com/langsmith/agent-server-api/assistants/create-assistant))
Assistants have **versions** — every PATCH bumps `version`; endpoints exist to list versions
and set the "latest" pointer.

### 1.3 Thread
Conversation/session container; owns a checkpoint log.

```json
{
  "thread_id": "9dde5490-... (uuid)",
  "created_at": "...", "updated_at": "...",
  "metadata": {},
  "config": {},
  "status": "idle",              // idle | busy | interrupted | error
  "values": null                 // materialized on read
}
```
([cron-jobs guide sample output](https://docs.langchain.com/langsmith/cron-jobs))

### 1.4 Run
One invocation of an assistant/graph, optionally on a thread.

Fields (SDK + API): `run_id`, `thread_id`, `assistant_id`, `created_at`, `updated_at`,
`status` ∈ `pending | running | error | success | timeout | interrupted`, `metadata`,
`kwargs` (echo of creation payload), `multitask_strategy`.
Run access control is inherited from the parent thread
([auth docs](https://docs.langchain.com/langsmith/auth)).

### 1.5 ThreadState / Checkpoint
The checkpoint is the unit of persistence and time-travel:

```json
{
  "values": {...},               // full state at this checkpoint
  "next": ["node_a"],            // nodes scheduled to run next
  "checkpoint": {                // CheckpointConfig
    "thread_id": "...",
    "checkpoint_ns": "",         // subgraph namespace, "" = root
    "checkpoint_id": "...",
    "checkpoint_map": {}
  },
  "metadata": {...},             // step, source, writes...
  "created_at": "...",
  "parent_checkpoint": {...},
  "tasks": [ {"id","name","error","interrupts":[{"value","id"}],"checkpoint","state"} ],
  "interrupts": [ {"value": ..., "id": "..."} ]
}
```
([Get Thread State](https://docs.langchain.com/langsmith/agent-server-api/threads/get-thread-state),
[Get Thread History Post](https://docs.langchain.com/langsmith/agent-server-api/threads/get-thread-history-post.md))

### 1.6 Store item (long-term memory / "memories")
Cross-thread key-value store with hierarchical namespaces (list of strings, like a path):

```json
{ "namespace": ["user_profiles"], "key": "profile_jane",
  "value": { /* arbitrary JSON document */ },
  "created_at": "...", "updated_at": "..." }
```
Search supports listing by `namespace_prefix` + `filter` (ordered by `updated_at`) or
semantic `query` (natural-language search when an `embed` function is configured in
`langgraph.json`), plus `limit`/`offset`/`refresh_ttl`.
([Search items](https://docs.langchain.com/langsmith/agent-server-api/store/search-or-list-items-within-a-namespace-prefix.md),
[CLI/store config](https://docs.langchain.com/langsmith/cli))

### 1.7 Cron
```json
{ "cron_id": "uuid", "assistant_id": "uuid|null", "thread_id": "uuid|null",
  "schedule": "27 15 * * *", "timezone": null,   // IANA tz; null = UTC
  "end_time": null, "next_run_date": "...",
  "payload": { /* full run-create payload */ },
  "user_id": null, "metadata": {}, "enabled": true,
  "created_at": "...", "updated_at": "..." }
```
Stateless crons create a fresh thread per execution; `on_run_completed: "delete"|"keep"`
controls cleanup. Thread crons run repeatedly on one thread.
([Create Cron](https://docs.langchain.com/langsmith/agent-server-api/crons/create-cron.md),
[cron-jobs guide](https://docs.langchain.com/langsmith/cron-jobs))

### 1.8 Relationships
```
langgraph.json ──registers──> Graph (name)
Graph ──instantiated by──> Assistant (graph_id + config + context; versioned)
Assistant ──executed as──> Run ──writes──> Checkpoint ──appended to──> Thread
Cron ──spawns──> Run (on new or existing Thread)
Store: flat KV with namespace paths; orthogonal to threads (cross-thread memory)
Webhook URL: per-run callback, invoked when the run finishes
```

---

## 2. Endpoint inventory

Exact paths from the official endpoint reference index
([docs index](https://docs.langchain.com/llms.txt) → `agent-server-api/*` pages).

### Assistants
| Method | Path | Purpose |
|---|---|---|
| POST | `/assistants` | Create assistant (`graph_id` required, `if_exists`) |
| POST | `/assistants/search` | Search/list (metadata filter, `graph_id`, limit/offset) |
| POST | `/assistants/count` | Count matching assistants |
| GET | `/assistants/{assistant_id}` | Get one |
| PATCH | `/assistants/{assistant_id}` | Update (creates new version) |
| DELETE | `/assistants/{assistant_id}` | Delete |
| GET | `/assistants/{assistant_id}/graph` | Graph topology (nodes/edges; `?xray=` depth) |
| GET | `/assistants/{assistant_id}/schemas` | input/output/state/config/context JSON Schemas |
| GET | `/assistants/{assistant_id}/subgraphs` | List subgraphs (also `/subgraphs/{namespace}`) |
| GET | `/assistants/{assistant_id}/versions` | List versions |
| POST | `/assistants/{assistant_id}/latest` | Set latest version pointer |

### Threads
| Method | Path | Purpose |
|---|---|---|
| POST | `/threads` | Create (optional client-supplied `thread_id`, `metadata`, `if_exists`) |
| POST | `/threads/search` | Search by metadata/status/values; limit/offset |
| POST | `/threads/count` | Count |
| GET | `/threads/{thread_id}` | Get one |
| PATCH | `/threads/{thread_id}` | Update metadata / values (creates history revision) |
| DELETE | `/threads/{thread_id}` | Delete |
| POST | `/threads/{thread_id}/copy` | Fork thread into an independent copy |
| POST | `/threads/prune` | Bulk-delete threads by age/status (TTL housekeeping) |
| GET | `/threads/{thread_id}/state` | Latest ThreadState (get_state) |
| GET | `/threads/{thread_id}/state/{checkpoint_id}` | State at a specific checkpoint (time-travel read) |
| POST | `/threads/{thread_id}/state` | **Update state** (update_state): body `{values, checkpoint, as_node}` — writes a new checkpoint, optionally attributing the write `as_node`; returns `{checkpoint}` |
| GET / POST | `/threads/{thread_id}/history` | State history (time-travel browse). POST body `ThreadStateSearch{limit(≤1000), before: CheckpointConfig, metadata, checkpoint}` returns `ThreadState[]` newest-first |
| GET | `/threads/{thread_id}/stream` | **Thread stream** (SSE): join all runs on a thread; `stream_modes` ∈ `run_modes`(default) / `lifecycle` / `state_update`; `Last-Event-ID` resume (`"-"` = replay all) |

### Thread Runs (`/threads/{tid}/runs...`)
| Method | Path | Purpose |
|---|---|---|
| POST | `/threads/{tid}/runs` | **Background run** — returns run immediately, executes async |
| POST | `/threads/{tid}/runs/stream` | Create run + stream output over SSE |
| POST | `/threads/{tid}/runs/wait` | Create run, block, return final output |
| GET | `/threads/{tid}/runs` | List runs (limit/offset/status) |
| POST | `/threads/{tid}/runs/search` | Search runs (metadata/status filter) |
| GET | `/threads/{tid}/runs/{run_id}` | Get run |
| DELETE | `/threads/{tid}/runs/{run_id}` | Delete (must be finished/cancelled first) |
| POST | `/threads/{tid}/runs/{run_id}/cancel` | Cancel. Query: `wait=false`, `action=interrupt\|rollback` — `rollback` also deletes the run and its checkpoints |
| POST | `/threads/{tid}/runs/cancel` | Cancel several runs at once |
| GET | `/threads/{tid}/runs/{run_id}/join` | Wait for completion, get final output |
| GET | `/threads/{tid}/runs/{run_id}/stream` | **Join live stream** of an active background run (not buffered — only output after join) |

### Stateless Runs (no thread, ephemeral)
| Method | Path | Purpose |
|---|---|---|
| POST | `/runs` | Background stateless run |
| POST | `/runs/stream` | Stateless run + SSE stream |
| POST | `/runs/wait` | Stateless run, block for output |
| POST | `/runs/batch` | Batch of stateless runs in one request |

### Crons
| Method | Path | Purpose |
|---|---|---|
| POST | `/runs/crons` | Create stateless cron (new thread per fire) |
| POST | `/threads/{tid}/runs/crons` | Create cron bound to a thread |
| POST | `/runs/crons/search` | Search crons |
| POST | `/runs/crons/count` | Count |
| GET | `/runs/crons/{cron_id}` | Get |
| PATCH | `/runs/crons/{cron_id}` | Update (schedule, enabled, payload...) |
| DELETE | `/runs/crons/{cron_id}` | Delete |

### Store
| Method | Path | Purpose |
|---|---|---|
| PUT | `/store/items` | Upsert `{namespace: [...], key, value}` |
| GET | `/store/items?namespace=..&key=..` | Get one item |
| DELETE | `/store/items` | Delete `{namespace, key}` |
| POST | `/store/items/search` | List by `namespace_prefix`+`filter`, or semantic `query`; `limit/offset/refresh_ttl` |
| POST | `/store/namespaces` | List namespaces (with match conditions) |

### System / misc
| Method | Path | Purpose |
|---|---|---|
| GET | `/ok` | Health check |
| GET | `/info` | Server info |
| GET | `/metrics` | Prometheus-style metrics |
| GET | `/docs` | OpenAPI UI per deployment |
| * | `/a2a/{assistant_id}` | A2A protocol endpoint; agent cards auto-generated; A2A `contextId` mapped to `thread_id` |
| * | MCP endpoints | Expose an agent as an MCP server |
| POST/GET | `/threads/{tid}/stream` (+ WS) | "Protocol v2" thread-centric streaming: SSE event stream, WebSocket upgrade, and HTTP command sidecar |

### Run-create payload (the core request shape)
Shared by `POST /threads/{tid}/runs`, `/runs/stream`, `/runs/wait`, stateless variants,
and cron `payload`
([Create Run, Wait for Output](https://docs.langchain.com/langsmith/agent-server-api/thread-runs/create-run-wait-for-output)):

```json
{
  "assistant_id": "uuid-or-graph-name",        // required
  "input": { /* graph input */ },
  "command": { "update": {}, "resume": {}, "goto": {"node": "...", "input": {}} },
  "metadata": {},
  "config": { "tags": [], "recursion_limit": 25, "configurable": {} },
  "context": {},
  "checkpoint": { /* CheckpointConfig: resume from specific checkpoint */ },
  "webhook": "https://...",                    // called when the API call/run finishes
  "interrupt_before": "*" | ["node", ...],
  "interrupt_after":  "*" | ["node", ...],
  "stream_mode": ["values", ...] | "values",   // see §3
  "stream_subgraphs": false,
  "stream_resumable": false,                   // persist chunks for Last-Event-ID resume
  "feedback_keys": [],
  "multitask_strategy": "enqueue",             // reject|interrupt|rollback|enqueue
  "if_not_exists": "reject",                   // thread auto-create behavior: reject|create
  "after_seconds": 0,                          // delayed start
  "checkpoint_during": true,                   // checkpoint mid-run vs only at end
  "durability": "async",                       // sync|async|exit
  "on_disconnect": "continue"                  // continue|cancel when SSE client drops
}
```

`command.resume` is the human-in-the-loop resume channel (reply to `interrupt()`);
`command.update`+`goto` implement state edits and jumps — the HTTP projection of
LangGraph's `Command` primitive.

---

## 3. Streaming protocol (SSE)

Transport: `text/event-stream`; each frame is `event: <type>` + `id: <opaque>` + `data: <json>`.
([Streaming guide](https://docs.langchain.com/langsmith/streaming))

### Stream modes (`stream_mode`, single string or list for multiplexing)
| Mode | Content |
|---|---|
| `values` | Full graph state after each super-step |
| `updates` | `{node_name: state_update}` per step |
| `messages` / `messages-tuple` | LLM tokens as `[message_chunk, metadata]` tuples; metadata carries `langgraph_node`, tags, run ids for filtering |
| `debug` | Maximal detail (task scheduling, checkpoints) |
| `events` | Raw `astream_events` payloads (LCEL-style) |
| `custom` | User-defined data written from inside nodes (writer API) |
| `tasks` | Task start/finish lifecycle |
| `checkpoints` | Checkpoint records as written |

(OpenAPI enum from [CronCreate](https://docs.langchain.com/langsmith/agent-server-api/crons/create-cron.md):
`values, messages, messages-tuple, tasks, checkpoints, updates, events, debug, custom`.)

### SSE event types on a run stream
- `metadata` — first frame: `{"run_id": ..., "attempt": 1, ...}`
- one frame per stream-mode datum; when multiple modes are requested the event name is the mode
  (e.g. `event: updates`, `event: messages`, `event: messages/partial`, `messages/complete`, `messages/metadata`)
- `error` — `{"error": "...", "message": "..."}` on failure
- `end` — run finished

### Resume semantics
- **Run streams**: `stream_resumable: true` makes the server persist stream chunks; a dropped
  client reconnects with the SSE `Last-Event-ID` header and resumes without data loss.
- **Thread streams** (`GET /threads/{tid}/stream`): `Last-Event-ID: <id>` resumes;
  `Last-Event-ID: "-"` replays from the beginning. Modes: `run_modes` (default, full run
  output), `lifecycle` (run start/end only), `state_update` (thread state after each run).
- **Join stream** (`GET /threads/{tid}/runs/{run_id}/stream`): attaches to a live background
  run; **output is not buffered** — anything emitted before joining is lost
  ([streaming guide](https://docs.langchain.com/langsmith/streaming)).
- `on_disconnect: "continue"|"cancel"` decides whether the run survives client disconnect.

### Subgraphs
`stream_subgraphs: true` includes subgraph output; streamed chunks carry namespace
prefixes identifying which (sub)graph emitted them.

---

## 4. Config & deployment model

### 4.1 `langgraph.json`
Single declarative config read by the CLI (`dev`/`build`/`up`/`deploy`) and by the platform
image build ([application structure](https://docs.langchain.com/langsmith/application-structure),
[langchain-skills CLI reference](https://github.com/langchain-ai/langchain-skills/blob/main/config/skills/langgraph-cli/SKILL.md)):

```json
{
  "dependencies": [".", "langchain_openai", "./local_package"],
  "graphs": { "agent": "./my_agent/agent.py:graph" },
  "env": "./.env",
  "python_version": "3.12",            // or "node_version": "20"
  "pip_config_file": "./pip.conf",
  "dockerfile_lines": ["RUN apt-get ..."],
  "http": { "cors": {...}, "disable_assistants": false, ... },
  "auth": { "path": "./src/auth.py:auth" },
  "store": { "index": { "embed": "openai:text-embedding-3-small", "dims": 1536, "fields": ["$"] } },
  "checkpointer": { "ttl": { "default_ttl": 4320, "sweep_interval_minutes": 60 } },
  "image_distro": "wolfi"
}
```

- **`graphs`** maps graph-id → `./path/file.py:variable` (JS: `file.js:function`). The
  variable must be a `CompiledGraph` or a factory returning one. Multiple graphs per server.
- **`dependencies`**: `"."` resolves local package manifests (`pyproject.toml` /
  `requirements.txt` / `package.json`); also subdirs and registry package names.
- **`env`**: path to `.env` or an inline map.
- Graph loading: the server imports each path at startup and instantiates the compiled
  graph; each registered graph name becomes a default assistant (`graph_id` usable as
  `assistant_id`).
- Framework-agnostic: only the *deployment interface* must be a LangGraph graph; node code
  can be anything (`deployments-wrap-sdk` wraps ADK/CrewAI/Strands/AutoGen).

### 4.2 CLI / Docker flow
([CLI docs](https://docs.langchain.com/langsmith/cli))

| Command | Behavior |
|---|---|
| `langgraph dev` | In-process dev server, hot reload, **no Docker**; in-memory persistence; free (no key needed beyond LangSmith key for Studio) |
| `langgraph build` | Builds production image **FROM `langchain/langgraph-api:<py>`**, copies app, installs `dependencies`, applies `dockerfile_lines` |
| `langgraph dockerfile` | Emits the Dockerfile for custom builds |
| `langgraph up` | Docker compose: API server + Postgres (+ Redis); needs `LANGSMITH_API_KEY` for local dev, `LANGGRAPH_CLOUD_LICENSE_KEY` for production |

Runtime architecture (self-hosted standalone,
[deploy-standalone-server](https://langchain-5e9cc07a.mintlify.app/langsmith/deploy-standalone-server)):
- **Postgres** (`DATABASE_URI`): assistants, threads, runs, checkpoints, store, task-queue
  state with "exactly once" semantics.
- **Redis** (`REDIS_URI`): pub/sub broker fanning out real-time output of background runs
  to streaming clients.
- **Queue workers**: background runs are consumed from a Postgres-backed task queue
  (separate queue container in the compose topology).

### 4.3 Auth
([auth docs](https://docs.langchain.com/langsmith/auth),
[server-api-ref](https://docs.langchain.com/langsmith/server-api-ref))
- Managed LangSmith deployments: `X-Api-Key` header with a LangSmith API key, default.
- Self-hosted: **no default auth**.
- Custom auth (Enterprise / all LangSmith plans): user-supplied `Auth()` object wired via
  `langgraph.json` `auth.path`:
  - `@auth.authenticate` — middleware per request; returns `{identity, is_authenticated, permissions, ...custom}`; user info injected into run config (`langgraph_auth_user`).
  - `@auth.on.<resource>[.<action>]` — authorization handlers returning metadata filters
    (`$eq` shorthand, `$contains`; dict = AND) for threads/assistants/crons (`create`,
    `read`, `update`, `delete`, `search`, `threads.create_run`) and store ops (which rewrite
    `namespace` instead of returning filters). Runs inherit thread ACLs.

### 4.4 Double-texting / concurrency (multitask strategies)
One active run per thread; `multitask_strategy` on run creation governs collisions
([double-texting docs](https://docs.langchain.com/langsmith/double-texting)):

| Strategy | Behavior |
|---|---|
| `enqueue` (default) | Queue new run; executes after current run finishes |
| `reject` | 4xx the new run while one is active |
| `interrupt` | Cancel current run, keep its progress/checkpoints, then run new input |
| `rollback` | Cancel current run **and delete it + its checkpoints**, then start fresh |

Cancellation mirror: `POST .../runs/{run_id}/cancel?action=interrupt|rollback&wait=false`
([Cancel Run](https://docs.langchain.com/langsmith/agent-server-api/thread-runs/cancel-run.md)).
Rollback semantics rely on `get_state_history` to find the checkpoint to restore —
see the open-source reimplementation discussion
([aegra#191](https://github.com/aegra/aegra/issues/191)).

---

## 5. Open vs closed

**Open source (MIT, github.com/langchain-ai/langgraph):**
- The graph runtime itself (`langgraph` Python / `@langchain/langgraph` JS), checkpointers
  (in-memory, Postgres, Redis, SQLite libraries), the store interface.
- `langgraph dev` **in-memory dev server** — full HTTP surface, but ephemeral storage,
  single-process, no license needed.
- The SDKs (`langgraph_sdk`, `@langchain/langgraph-sdk`, React `useStream`).
- **Agent Protocol** spec + OpenAPI + generated server stubs — the open subset of the API.
- `LangGraph.js API` — an open-source in-memory JS implementation of the protocol.

**Closed / commercial (`langchain/langgraph-api` Docker image):**
- The production server: durable Postgres persistence, Redis pub/sub streaming fan-out,
  the exactly-once background task queue, cron scheduler, webhooks, TTL/prune, metrics.
- Distributed as a **pre-built image only** (source not published). Community analysis notes
  the core server runs without a key but license-gated features check
  `LANGGRAPH_CLOUD_LICENSE_KEY` once at startup
  ([forum thread](https://forum.langchain.com/t/question-about-the-license/2242),
  [issue #6341](https://github.com/langchain-ai/langgraph/issues/6341)).

**Editions**
([LangGraph Data Plane docs](https://nightcat.cloudns.asia:9981/sitedoc/langgraph/v0.4.3/concepts/langgraph_data_plane/),
[LangGraph Server page](https://langchain-5e9cc07a-preview-eugene-1754316337-1141f3a.mintlify.app/langgraph-platform/langgraph-server),
[agentailor 2026 guide](https://blog.agentailor.com/posts/is-langchain-worth-it-2026)):
- **Lite** — standalone container, free up to 1M node executions/year; **no cron jobs, no
  custom auth**.
- **Enterprise** — Cloud SaaS, Self-Hosted Data Plane, Self-Hosted Control Plane, or
  standalone container with license key; full feature set. Custom auth errors with
  "only available in Managed Cloud or Enterprise" without it
  ([issue #5390](https://github.com/langchain-ai/langgraph/issues/5390)).
- Practitioners confirm there is effectively **no supported self-host path without LangChain
  keys** for the production image ([forum, May 2026](https://forum.langchain.com/t/best-practices-for-self-hosting-langgraph-server-oss-without-langgraph-keys/3779)) — which is exactly the gap open
  reimplementations (aegra, skein-js) and `rusty-agent-server` target.

---

## Implications for Rusty Server

1. **Implement the Agent Protocol subset first, byte-for-byte compatible.** Threads/runs/store
   CRUD + `POST /threads`, `POST /threads/{tid}/runs{,/stream,/wait}`, stateless `/runs/*`,
   `POST /store/items...`. Wire-compatibility buys the entire LangChain client ecosystem
   (`langgraph_sdk`, `useStream`, Agent Chat UI, Studio) for free — this is the single
   highest-leverage design decision, proven by aegra/skein-js.
2. **Copy the resource model verbatim: graph registry + assistants + threads + runs +
   checkpoint log + namespaced KV store.** Especially: `assistant_id` accepting a raw graph
   name (auto-default assistant), assistant versioning with a "latest" pointer, and
   `if_exists`/`if_not_exists` idempotency knobs. These details remove real friction.
3. **Copy the SSE contract exactly: `stream_mode` multiplexing, `metadata`/`error`/`end`
   frames, `Last-Event-ID` resume with `stream_resumable`, thread-level streams, and
   join-stream.** Buffering discipline matters: LangGraph does *not* buffer join-stream
   output — decide explicitly whether rusty-agent-server matches (cheaper) or improves
   (replay buffer per run) and document it.
4. **Improve on durability/primitives, don't clone the queue.** LangGraph needs
   Postgres + Redis + queue workers for exactly-once runs and streaming fan-out. In Rust, a
   single embedded store (e.g. redb/SQLite) + tokio broadcast channels can deliver the same
   semantics with one binary — a genuine deployment-story win over the 3-container compose.
5. **Adopt `multitask_strategy` (enqueue/reject/interrupt/rollback) and
   `cancel?action=interrupt|rollback` as first-class API surface** — this concurrency
   semantics is the hardest-won lesson in the LangGraph API and most clones get it wrong.
   Rollback = delete run + its checkpoints, not just "stop".
6. **Steal the `langgraph.json` pattern, generalized.** One declarative file mapping
   `name → graph entrypoint`, plus env/deps/auth/store-index config. For Rust this becomes
   e.g. `rusty.toml` mapping names to compiled graph modules/WASM/dylibs or remote
   graph services — same ergonomics, no Dockerfile step needed.
7. **Skip / de-prioritize:** A2A and MCP server endpoints (bolt on later), the
   WebSocket "protocol v2" command surface (SSE + HTTP sidecar is sufficient), `feedback_keys`
   (LangSmith tracing coupling), and Lite/Enterprise feature-gating — rusty-agent-server
   should ship crons + custom auth ungated, since LangChain's gating is the main
   self-hosting grievance.
8. **Auth: copy the shape, not the handler-in-Python mechanism.** LangSmith's
   `@auth.authenticate` + `@auth.on.<resource>.<action>` filter-dict model
   (`$eq`/`$contains`, metadata-scoped resources, runs inheriting thread ACLs) is a clean
   authorization algebra. Express it in Rust as a middleware trait + config-driven rules
   (or a WASM/Lua hook) rather than requiring user code embedded in the server.
