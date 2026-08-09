# Rusty Server quickstart — 10 minutes to a served agent graph

In this tutorial you will:

1. Create a new Rust binary project and depend on `rusty-agent-runtime` + `rusty-agent-server` (path deps).
2. Define a two-node graph — `draft → approve` — with a human-in-the-loop interrupt.
3. Serve it over HTTP + SSE with `GraphRegistry` and `serve()`.
4. Drive it with `curl`: create a thread, run to the interrupt, inspect state, stream the resume over SSE, and list checkpoint history.
5. Branch the timeline: fork the thread at an earlier checkpoint and replay the run on the fork.

**Prerequisites:** a Rust toolchain (`rustup`), `curl`, and ~10 minutes. No Docker, no database, no Redis — everything runs in one process. Commands below assume you create the project as a *sibling* of the `rusty-core` and `rusty-server` checkouts; adjust the `path =` values if your layout differs.

---

## 1. Create the project (1 min)

```bash
cargo new agent-server-demo
cd agent-server-demo
```

Add the dependencies to `Cargo.toml`:

```toml
[dependencies]
rusty-agent-runtime = { path = "../rusty-core" }
rusty-agent-server = { path = "../rusty-server" }
tokio = { version = "1", features = ["full"] }
serde_json = "1"
```

That's the whole setup story: **Cargo.toml is the new langgraph.json**. There is no config file declaring your graphs — the declaration is your `main.rs`, checked by the compiler.

## 2. Define the graph (3 min)

Replace `src/main.rs` with:

```rust
use rusty_agent_runtime::prelude::*;
use rusty_agent_server::{serve, GraphRegistry, ServerConfig};
use serde_json::{json, Value};

#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    // -----------------------------------------------------------------
    // State schema: one channel per key, each with its reducer. Each
    // channel here has exactly one writer node, so Overwrite (last-value)
    // semantics are correct everywhere.
    // -----------------------------------------------------------------
    let spec = StateSpec::new()
        .channel("draft", Reducer::Overwrite)
        .channel("approval", Reducer::Overwrite);

    // -----------------------------------------------------------------
    // Graph: draft -> approve. `approve` suspends the run until a human
    // decision arrives — the canonical interrupt/resume pattern: check
    // `ctx.resume_value()` FIRST; only interrupt when there is none.
    // -----------------------------------------------------------------
    let mut builder = GraphBuilder::new();

    builder.add_node("draft", |ctx: NodeContext| async move {
        // If the run's `input` seeded a draft, keep it; otherwise write one.
        let draft = ctx.state().get("draft").cloned().unwrap_or_else(|| {
            json!("Rusty Server serves durable agent graphs from one binary.")
        });
        println!("[draft] {draft}");
        Ok(NodeOutput::update("draft", draft))
    });

    builder.add_node("approve", |ctx: NodeContext| async move {
        match ctx.resume_value() {
            // Phase 2: a human decision arrived via `command.resume`.
            Some(decision) => {
                println!("[approve] resumed with decision: {decision}");
                Ok(NodeOutput::update("approval", decision.clone()))
            }
            // Phase 1: no decision yet — suspend the whole run. The payload
            // is surfaced to the HTTP caller as the run's `interrupt` value.
            None => {
                let draft = ctx.state().get("draft").cloned().unwrap_or(Value::Null);
                println!("[approve] no decision — interrupting for human review");
                Err(ctx.interrupt(json!({
                    "kind": "approval_request",
                    "prompt": "Approve this draft for publication?",
                    "draft": draft,
                })))
            }
        }
    });

    builder.set_entry_point("draft");
    builder.add_edge("draft", "approve");
    let graph = builder.compile()?; // validates topology before serving anything

    // -----------------------------------------------------------------
    // The registry: the Rust analog of langgraph.json's `graphs` map.
    // A name plus the two things the executor needs — Graph + StateSpec.
    // -----------------------------------------------------------------
    let mut registry = GraphRegistry::new();
    registry.register("publisher", graph, spec);

    // -----------------------------------------------------------------
    // Serve. Blocks on the axum/tokio runtime. The bind address and the
    // checkpoint store directory are code; a JsonFileCheckpointer rooted
    // at `store_path` is wired in for you.
    // -----------------------------------------------------------------
    let config = ServerConfig::new(
        "0.0.0.0:8080".parse()?,  // bind address
        "./data/checkpoints",     // JsonFileCheckpointer root
    );
    println!("listening on http://localhost:8080 (dev mode: no API key set)");
    serve(registry, config).await?;
    Ok(())
}
```

Every `rusty-agent-runtime` call above is the same API the library examples use (`rusty-core/examples/human_in_loop.rs`) — `StateSpec`, `GraphBuilder`, `NodeContext::interrupt` / `resume_value`, `NodeOutput::update`. The server crate adds exactly three names you call directly: `GraphRegistry`, `ServerConfig`, `serve` (plus `router` if you want to embed the routes in a larger axum app).

## 3. Run it (1 min)

```bash
cargo run
```

You should see `listening on http://localhost:8080`. `ServerConfig` has no API key by default, so the server is in dev mode (no auth). In another terminal:

```bash
curl localhost:8080/ok
# {"ok":true}

curl localhost:8080/info
# {"service":"rusty-server","version":"0.5.0","checkpointer":"json_file",
#  "store_path":"./data/checkpoints",
#  "graphs":[{"channels":["approval","draft"],"name":"publisher"}]}
```

> With auth configured — `ServerConfig::new(…).with_api_key("secret")` — add
> `-H "X-Api-Key: secret"` to every request below.

## 4. Create a thread (1 min)

A thread binds to one registered graph at creation:

```bash
curl -s -X POST localhost:8080/threads \
  -H 'Content-Type: application/json' \
  -d '{"graph": "publisher", "metadata": {"owner": "quickstart"}}'
# 201 {"thread_id": "3f2b9c4e-…", "graph": "publisher",
#      "metadata": {"owner": "quickstart"}, "created_at": "2026-08-05T…Z"}

TID=<paste the thread_id here>
```

## 5. Run to the interrupt (1 min)

`POST /threads/{id}/runs/wait` blocks until the run finishes — or suspends:

```bash
curl -s -X POST localhost:8080/threads/$TID/runs/wait \
  -H 'Content-Type: application/json' \
  -d '{"input": {"draft": "Rust agents, one binary."}}'
```

Response (the run's terminal JSON):

```json
{
  "run_id": "7c1e…",
  "thread_id": "3f2b9c4e-…",
  "status": "interrupted",
  "interrupt": {
    "kind": "approval_request",
    "prompt": "Approve this draft for publication?",
    "draft": "Rust agents, one binary."
  },
  "checkpoint_id": "a94f…",
  "state": { "draft": "Rust agents, one binary." }
}
```

The run suspended inside `approve`, and the executor persisted a checkpoint for the thread — this is durable, so you could restart the server right now and lose nothing (re-create the thread with the same `thread_id` after a restart: thread records live in memory, checkpoints are durable on disk). Verify:

```bash
curl -s localhost:8080/threads/$TID/state
# {"values": {"draft": "Rust agents, one binary."}, "next": ["approve"],
#  "checkpoint": {"checkpoint_id": "a94f…", "thread_id": "3f2b…", "step": 1,
#                 "created_at": "…"}}
```

`next: ["approve"]` tells you exactly where the run is parked.

## 6. Resume — streamed over SSE (2 min)

Resume with the human's decision via `command.resume`, and this time take the run as a Server-Sent Events stream (`-N` disables curl's buffering — required for SSE):

```bash
curl -N -X POST localhost:8080/threads/$TID/runs/stream \
  -H 'Content-Type: application/json' \
  -d '{"command": {"resume": {"approved": true, "reviewer": "alice"}},
       "stream_mode": ["updates", "values"]}'
```

You'll see frames like (ids are `{checkpoint_id}:{step}:{seq}`; frames emitted before the run's first checkpoint use `-` as the checkpoint component):

```text
event: metadata
id: -:0:1
data: {"run_id": "…", "thread_id": "3f2b9c4e-…", "graph": "publisher",
       "attempt": 2, "metadata": null}

event: updates
id: a94f…:2:2
data: {"step": 2, "updates": {"approve": {"approval": {"approved": true, "reviewer": "alice"}}}}

event: values
id: a94f…:2:3
data: {"draft": "Rust agents, one binary.", "approval": {"approved": true, "reviewer": "alice"}}

event: end
id: a94f…:2:4
data: {"status": "success"}
```

The executor restored the checkpoint, re-executed `approve` from its start with `ctx.resume_value()` set (this is why node logic must be idempotent), and the run completed.

### Last-Event-ID dedup

Every SSE frame carries `id: {checkpoint_id}:{step}:{seq}`, where `seq` is a per-run monotonically increasing sequence number. A client that reconnects with a `Last-Event-ID` header skips every frame whose sequence number it has already seen — the server replays the run's in-memory event-log tail after that point (capacity: `ServerConfig::event_log_capacity`, default 1000 frames) before streaming live frames:

```bash
curl -N -X POST localhost:8080/threads/$TID/runs/stream \
  -H 'Content-Type: application/json' \
  -H 'Last-Event-ID: a94f…:2:2' \
  -d '{"command": {"resume": {"approved": true}}, "stream_mode": ["updates", "values"]}'
```

## 7. Time travel: checkpoint history (1 min)

Every super-step boundary was checkpointed. List them, newest first:

```bash
curl -s -X POST localhost:8080/threads/$TID/history \
  -H 'Content-Type: application/json' \
  -d '{"limit": 10}'
```

And to undo a finished run — delete the checkpoints it created, re-anchoring the thread to the pre-run checkpoint:

```bash
curl -s -X DELETE localhost:8080/threads/$TID/runs/$RUN_ID
# {"run_id": "…", "thread_id": "…", "deleted_checkpoints": 2,
#  "remaining_checkpoints": 1}
```

## 8. Time travel: fork & replay (1 min)

Rollback rewinds a thread; fork branches it. `POST /threads/{id}/fork` copies the thread's checkpoint history into a new thread on the same graph, and every run endpoint accepts `"checkpoint": {"checkpoint_id": …}` to replay from that checkpoint instead of the latest:

```bash
# Pick an earlier checkpoint from the §7 history listing
CP_ID=<a checkpoint_id from the history above>

# Fork the thread at that checkpoint (omit checkpoint_id for a full-history fork)
curl -s -X POST localhost:8080/threads/$TID/fork \
  -H 'Content-Type: application/json' \
  -d '{"new_thread_id": "branch-a", "checkpoint_id": "'$CP_ID'"}'
# 201 {"thread_id": "branch-a", "checkpoints_copied": 1}

# Replay the run from the same checkpoint, on the fork
curl -s -X POST localhost:8080/threads/branch-a/runs/wait \
  -H 'Content-Type: application/json' \
  -d '{"checkpoint": {"checkpoint_id": "'$CP_ID'"}}'
```

The safe pattern is fork first, replay on the fork: the branch gets its own thread id and its own history, while replaying on the original thread appends new checkpoints on top of the old timeline (supported, but rarely what you want). Errors: `404` for an unknown thread or checkpoint id, `400` when the source thread has no checkpoints to copy, `409` when `new_thread_id` is already taken.

## 9. Inspect the run's journal (Flight Recorder, 1 min)

Every run is journaled: super-step boundaries, node inputs/outputs, interrupts, routing decisions, and checkpoint writes are recorded as `RunEvent`s — causally linked (`parent`), totally ordered (`seq`), with effect classifications. Fetch any run's evidence:

```bash
RUN_ID=<a run_id from any runs/wait response above>

curl -s localhost:8080/runs/$RUN_ID/events | jq '{complete, first: .events[0], count: (.events | length)}'
# {
#   "complete": true,
#   "first": {
#     "id": "7c1e…:0",
#     "run_id": "7c1e…",
#     "thread_id": "3f2b9c4e-…",
#     "node_id": null,
#     "seq": 0,
#     "kind": "super_step_start",
#     "effect": "pure",
#     ...
#   },
#   "count": 12
# }
```

`complete: true` means the served snapshot is the run's final journal; while a run is active, the snapshot trails the live journal by at most one checkpoint boundary (it is flushed per checkpoint and at completion). The interrupted §5 run shows a `"kind": "interrupt"` event, and its §6 resume — a separate run with its own journal — starts with a `"kind": "resume"` event. Unknown runs answer `404`, exactly like `GET /runs/{id}`.

To take the evidence with you — say, to re-drive this run in CI — download it as a portable replay fixture:

```bash
curl -s localhost:8080/runs/$RUN_ID/fixture -o fixture.json
# journal + graph topology hash + final checkpoint; load in Rust with
# rusty_agent_runtime::replay::ReplayFixture::import(&json)
```

## 10. Replay a run server-side & diff two branches (2 min)

Downloading a fixture replays in CI; `POST /runs/replay` replays **in place**: the server re-drives the journaled run against the graph code registered in the process (zero outbound calls, over a throwaway checkpointer — your real checkpoint history is untouched) and verifies the replayed evidence against the recorded journal:

```bash
curl -s -X POST localhost:8080/runs/replay \
  -H 'Content-Type: application/json' \
  -d '{"run_id": "'$RUN_ID'"}'
# {"run_id": "7c1e…", "verified": true, "expected_events": 12,
#  "actual_events": 12, "first_divergence": null}
```

`verified: true` means the same graph code, given the same input, reproduced the recorded run's decisions and state transitions — compared on kinds, nodes, sequences, effect classes, statuses, and payloads, with per-run minted checkpoint ids and wall-clock measurements excluded. When it reports `false`, `first_divergence` is the journal `seq` where the replay parted ways with the evidence (change a node body, replay an old run, and watch it point at the first diverging event). Statuses worth knowing: `404` unknown run, `409` while the run is still executing or has no journal yet, `422` when the run's graph is not registered in this server process, when the journal holds recorded model/tool calls (those replay through the CI fixture — server-side replay cannot serve effects), or when the run resumed from a checkpoint.

Forks are where this gets interesting. Diff the §8 branch against the original run:

```bash
curl -s "localhost:8080/runs/diff?base=$RUN_ID&branch=$FORK_RUN_ID" | jq \
  '{first_divergent_seq, added: (.added | length), removed: (.removed | length),
    step_diffs, base_totals, branch_totals}'
```

The response is core's `BranchDiff` shape as-is: events compare logically (identity and timing excluded), so the shared prefix of a fork reads as equal, `first_divergent_seq` marks where the branches parted, `added`/`removed` carry the events at and after that point, and `step_diffs` shows which state channels changed at which super-step. Diff a run against itself and `first_divergent_seq` comes back `null`. The Studio's compare/replay UI consumes both endpoints with exactly these shapes, and both SDKs wrap them (`replay_run(run_id)` / `diff_runs(base, branch)` in Python, `replayRun` / `diffRuns` in TypeScript).

All four Flight Recorder endpoints stay reachable by run id even after the run's in-memory record is evicted or the server restarts — reads fall back to the persisted journal, so last week's run replays and diffs exactly like one from a minute ago.

---

## Where to go next

- **Run in the background instead of blocking:** `POST /threads/{id}/runs` returns `202` + a `run_id` immediately; control same-thread concurrency with `"multitask_strategy": "enqueue" | "reject"`.
- **Serve a real LLM agent:** register `create_react_agent(model, tools)` (see `rusty-core/examples/react_agent.rs`) the same way, with an `OpenAiCompatibleClient` as the model (e.g. `OpenAiCompatibleClient::from_env("https://api.openai.com/v1", "OPENAI_API_KEY", "gpt-4o-mini")`; `rusty-core/examples/live_agent.rs` shows the full live setup). Add `"messages"` to `stream_mode` to stream LLM token deltas when the node uses `ChatModel::chat_stream`.
- **Deploy:** `cargo build --release` produces one static binary; see the `FROM scratch` Dockerfile and the `ServerConfig` reference in the [rusty-server README](../rusty-server/README.md#deployment).
- **Design rationale:** endpoint mapping, SSE semantics, and the phased roadmap (gRPC workers, WASM nodes, crons, assistants) are in [docs/rusty-server-design.md](rusty-server-design.md).
