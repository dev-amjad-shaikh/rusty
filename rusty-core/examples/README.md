# Rusty Core examples

Five end-to-end examples, ordered from "no network required" to "real LLM
endpoint". Run any of them from the crate root:

```text
cd rusty-core
cargo run --example <name>
```

| Example | Run command | What it demonstrates |
|---|---|---|
| `react_agent` | `cargo run --example react_agent` | The prebuilt ReAct agent (`create_react_agent`) with a **scripted mock model** — the full agent ⇄ tools loop (parallel tool calls, tool results, final answer) plus the `GraphEvent` stream, with zero network access. |
| `react_record_replay` | `cargo run --example react_record_replay` | The Flight Recorder on the prebuilt ReAct agent: a `create_react_agent_with_recording` run journals every model/tool call, then `create_react_agent_replaying` + `ExactReplay::run_and_verify` re-drives it with **zero outbound calls** (panic-on-call sentinels never fire) and byte-identical evidence. |
| `parallel_fanout` | `cargo run --example parallel_fanout` | Dynamic fan-out / fan-in (map-reduce) via the `Send` API: a router emits one `Send` per item at runtime, workers run in parallel in a single super-step, and `Reducer::Append` merges results behind the barrier before a `summarize` node. |
| `human_in_loop` | `cargo run --example human_in_loop` | Interrupt / resume with durable checkpoints: an `approve` node suspends the run via `NodeContext::interrupt`, a checkpoint is persisted to JSON files, and the run resumes from the same `thread_id` with the human's decision. |
| `live_agent` | `cargo run --example live_agent` | A live ReAct agent against any OpenAI-compatible endpoint (Ollama, OpenAI, vLLM, LM Studio) with three real tools (`get_current_time`, `calculator`, `word_count`) and a pretty-printed `GraphEvent` stream including live token deltas (via `create_react_agent_streaming`). Graceful exit 0 with setup instructions when no endpoint is reachable (CI-safe). |

The first four examples are fully offline and deterministic; only
`live_agent` needs a running model server.

## `live_agent` configuration

The live demo reads three environment variables:

| Variable | Default | Notes |
|---|---|---|
| `RUSTY_BASE_URL` | `http://localhost:11434/v1` | Any OpenAI-compatible `/v1` base URL |
| `RUSTY_API_KEY` | `ollama` | Any string works for Ollama; real key for OpenAI |
| `RUSTY_MODEL` | `llama3.1` | Must support tool calling |

### Option A — Ollama (local, free)

```text
ollama pull llama3.1
ollama serve                       # listens on http://localhost:11434/v1
cargo run --example live_agent
```

### Option B — OpenAI

```text
RUSTY_BASE_URL=https://api.openai.com/v1 \
RUSTY_API_KEY=sk-... \
RUSTY_MODEL=gpt-4o-mini \
cargo run --example live_agent
```

### Option C — vLLM / LM Studio

Point `RUSTY_BASE_URL` at the server's `/v1` path and set
`RUSTY_MODEL` to the served model name, e.g.:

```text
RUSTY_BASE_URL=http://localhost:1234/v1 \
RUSTY_MODEL=local-model \
cargo run --example live_agent
```

If no endpoint answers, `live_agent` prints these same setup instructions
and exits with status 0 — it never panics, so it is safe to run in CI.
