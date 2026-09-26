# Rusty SDK — Python client

**Zero-dependency, stdlib-only Python SDK for [`rusty-server`](../../rusty-server).** Threads, runs (background / blocking / SSE-streaming), checkpoint history, time travel (fork + replay), assistants, crons, the cross-thread KV store, and the R0.6 durable task queue's control plane — over plain HTTP + SSE with nothing but `urllib.request` and `json`. Python 3.8+, no `pip install` of anything else, ever.

## Philosophy

This SDK is the **"interop over HTTP"** story: the Rust server owns orchestration, checkpoints, and streaming; any language that can speak HTTP and parse SSE can drive it. Python is the language most likely to already be on the machine, so this client deliberately uses **only the standard library** — no `requests`, no `httpx`, no `sseclient`. Drop the `rusty_client/` package into any project (or any `python3 -c` one-liner) and it works. The trade-off is explicit: you get a hand-rolled SSE parser and blocking I/O instead of a fancy async stack — which is exactly what you want for scripts, CI, notebooks, and LangChain-adjacent glue code.

## Install

```bash
pip install rusty-agent-runtime
```

From a local checkout (editable path install):

```bash
pip install /path/to/repo/sdks/python
```

Or just copy the package — it has no build step and no dependencies:

```bash
cp -r sdks/python/rusty_client /your/project/
```

## Quickstart

Start the demo server (scripted model — no network, no API keys):

```bash
cargo run -p rusty-agent-server --example server_demo
# rusty-server demo on http://127.0.0.1:8100  (graphs: pipeline, react_agent)
```

Then, mirroring the curl quickstart from the server README:

```python
from rusty_client import RustyClient

client = RustyClient("http://127.0.0.1:8100")   # api_key="..." when auth is on

# Liveness + what's registered
client.ok()      # True
client.info()    # {"service": "rusty-server", "graphs": [...], ...}

# Create a thread bound to a registered graph
thread = client.create_thread("pipeline")
tid = thread["thread_id"]

# Blocking run
result = client.run_wait(tid)
# {"status": "success", "output": {"log": ["first", "second"]}, ...}

# Streaming run (SSE) — frames arrive as the graph executes
for frame in client.run_stream(tid, stream_mode=["updates", "values"]):
    print(frame.event, frame.id, frame.data)
# metadata -:0:1 {"run_id": ..., "graph": "pipeline", ...}
# updates  -:0:2 {"step": 0, "updates": {"log": ["first"]}}
# values   <cp>:0:3 {"log": ["first"]}
# ...
# end      <cp>:1:6 {"status": "success"}

# Background run + polling
run = client.run(tid)
status = client.run_status(run["run_id"])   # terminal runs carry output/error

# Thread state + checkpoint history
client.get_state(tid)                       # {"values", "next", "checkpoint"}
client.history(tid, limit=10)               # newest first

# Time travel: fork at an earlier checkpoint, replay on the fork
mid = next(h for h in client.history(tid) if h["next"] == ["second"])
cp_id = mid["checkpoint"]["checkpoint_id"]
fork = client.fork(tid, checkpoint_id=cp_id)
client.run_wait(fork["thread_id"], checkpoint_id=cp_id)

# Human-in-the-loop: resume an interrupted run
client.run_wait(tid, command={"resume": {"approved": True}})

# Assistants, crons, KV store
assistant = client.create_assistant("support-bot", graph="react_agent",
                                    config={"recursion_limit": 25})
client.run_wait(tid, assistant_id=assistant["assistant_id"])

cron = client.create_cron(graph="react_agent", interval_secs=60,
                          input={"messages": [{"role": "user", "content": "hourly summary"}]})
client.list_crons()
client.delete_cron(cron["cron_id"])

client.kv_put("memories", "user-1", {"preference": "dark-mode"})
client.kv_list("memories")
client.kv_delete("memories", "user-1")

# Durable task queue (R0.6) — control plane: submit, observe, cancel
enqueued = client.tasks.enqueue(
    "send_email", {"to": "user@example.com"},
    idempotency_key="welcome-42",     # re-enqueueing dedupes on this key
    effect="idempotent",              # declares the work is safe to retry
    deadline="2026-08-11T00:00:00Z",  # RFC 3339, across attempts
)
enqueued  # {"task_id": "…", "deduplicated": False}

task = client.tasks.get(enqueued["task_id"])   # the full TaskRecord
client.tasks.list()                            # all tasks, oldest first
client.tasks.list(status="dead")               # the dead-letter queue
client.tasks.cancel(enqueued["task_id"])       # 409 if already terminal
client.tasks.cancel_run_tasks(run["run_id"])   # cancel a run's open tasks
```

With auth configured on the server (`ServerConfig::with_api_key`), pass `RustyClient(url, api_key="...")` — it is sent as the `X-Api-Key` header on every request.

## API reference

| Method | HTTP | Returns |
|---|---|---|
| `ok()` | `GET /ok` | `bool` |
| `info()` | `GET /info` | service metadata + registered graphs |
| `create_thread(graph, thread_id=None, metadata=None)` | `POST /threads` | thread record |
| `get_state(thread_id)` | `GET /threads/{id}/state` | `{values, next, checkpoint}` |
| `update_state(thread_id, values, as_node=None, next_nodes=None)` | `POST /threads/{id}/state` | new checkpoint |
| `history(thread_id, limit=None, before=None)` | `POST /threads/{id}/history` | checkpoints, newest first |
| `fork(thread_id, checkpoint_id=None, new_thread_id=None)` | `POST /threads/{id}/fork` | `{thread_id, checkpoints_copied}` |
| `run(thread_id, input=None, command=None, checkpoint_id=None, multitask_strategy=None, config=None, metadata=None, assistant_id=None)` | `POST /threads/{id}/runs` | `202` `{run_id, …}` (background) |
| `run_wait(thread_id, …same opts…, timeout=None)` | `POST /threads/{id}/runs/wait` | terminal dict `{status, output‖interrupt, …}` |
| `run_stream(thread_id, …same opts…, stream_mode=None, last_event_id=None, timeout=None)` | `POST /threads/{id}/runs/stream` | **generator** of `SSEEvent(event, data, id)` |
| `run_status(run_id)` | `GET /runs/{id}` | `{run_id, status, …}` (+ `output`/`error` when terminal) |
| `run_events(run_id)` | `GET /runs/{id}/events` | `{run_id, events, complete}` (Flight Recorder journal) |
| `get_fixture(run_id)` | `GET /runs/{id}/fixture` | portable `ReplayFixture` bundle for CI replay |
| `replay_run(run_id)` | `POST /runs/replay` | `{run_id, verified, expected_events, actual_events, first_divergence}` |
| `diff_runs(base, branch)` | `GET /runs/diff?base=…&branch=…` | `BranchDiff` (`first_divergent_seq`, `added`, `removed`, `step_diffs`, totals) |
| `delete_run(thread_id, run_id)` | `DELETE /threads/{id}/runs/{run_id}` | rollback a finished run |
| `create_assistant(name, graph, config=None, metadata=None, assistant_id=None)` | `POST /assistants` | assistant record |
| `list_assistants()` / `get_assistant(assistant_id)` | `GET /assistants[/{id}]` | assistant(s) |
| `create_cron(graph, interval_secs=None, cron_expr=None, input=None, metadata=None, on_run_completed=None)` | `POST /crons` | cron record (exactly one schedule kind) |
| `list_crons()` / `delete_cron(cron_id)` | `GET`/`DELETE /crons[/{id}]` | cron(s) |
| `kv_put(ns, key, value)` / `kv_get(ns, key)` / `kv_delete(ns, key)` / `kv_list(ns)` | `PUT`/`GET`/`DELETE /store/{ns}[/{key}]` | KV item(s) |
| `tasks.enqueue(kind, payload, pool=None, max_attempts=None, idempotency_key=None, effect=None, run_id=None, thread_id=None, deadline=None)` | `POST /tasks` | `{task_id, deduplicated}` |
| `tasks.enqueue_outbox(…same args…)` | `POST /tasks/outbox` | `202` `{task_id, deduplicated}` |
| `tasks.get(task_id)` | `GET /tasks/{id}` | task record |
| `tasks.list(status=None)` | `GET /tasks[?status=…]` | task records, oldest first (`dead` = DLQ) |
| `tasks.cancel(task_id)` | `POST /tasks/{id}/cancel` | updated record (`409` when terminal) |
| `tasks.cancel_run_tasks(run_id)` | `POST /runs/{id}/cancel` | `{run_id, cancelled, signalled}` |

### Durable tasks (R0.6)

`client.tasks` is the **control plane** of the durable task queue: submit work, observe records, cancel. Task records carry the full envelope — `kind`, `payload`, `pool`, `status` (`queued` / `leased` / `failed` / `completed` / `dead` / `cancelled`), `attempt` / `max_attempts`, the live `lease`, `error_class` + `last_error` from the last failed attempt, `idempotency_key`, `result` / `receipt` when settled, run/thread linkage, `cancel_requested`, `deadline`, and timestamps.

Two submission paths: `enqueue` makes the task claimable immediately; `enqueue_outbox` writes through the transactional outbox (202 accepted — the relay publishes it into the queue within one poll interval, deduped on the idempotency key, so a crash neither loses nor doubles the task). Cancellation is a hint for promptness, not a force: a queued or retry-scheduled task goes terminal-`cancelled` immediately, while a leased task keeps its lease with `cancel_requested` set so its holder aborts cleanly on the next heartbeat.

**Why there's no `claim` / `heartbeat` / `complete` / `fail` here:** those endpoints are the queue's *worker-machine* half — lease-guarded by `worker_id`, they exist so a worker process holds, renews, and settles exactly one lease at a time. A control-plane client that claimed a lease would starve real workers until the visibility timeout, or race their settlement into 409s. That surface belongs to the worker SDK (`rusty-worker`'s `ActivityWorker`); this client never holds leases.

### Streaming details

- `run_stream` returns a generator of `SSEEvent` dataclasses: `event` (e.g. `metadata`, `updates`, `values`, `messages`, `error`, `end`), `data` (JSON-decoded when possible), and `id` (`{checkpoint_id}:{step}:{seq}`).
- `stream_mode` filters frame families (`"updates"`, `"values"`, `"messages"`); `metadata`/`error`/`end` are always emitted.
- Pass `last_event_id=frame.id` to resume a dropped connection — the server replays only frames after that id (sent as the `Last-Event-ID` header).

### Errors

Every non-2xx response raises `RustyError` with `.status` (HTTP code, `None` for transport failures) and `.body` (raw response text):

```python
from rusty_client import RustyError

try:
    client.create_thread("no_such_graph")
except RustyError as exc:
    print(exc.status, exc.body)   # 404 / 400, server's error JSON
```

## Tests

The suite has two halves. `test_client.py` is a true end-to-end test: it builds (if needed) and launches the real `server_demo` binary as a subprocess, waits for `/ok`, exercises every endpoint family against it, and kills the process afterwards. `test_sse_parser.py` and `test_tasks.py` are no-I/O unit tests: the SSE parser and the tasks control plane are exercised against fake transports (mocked `urllib.request.urlopen` / `_request`), so they run anywhere in milliseconds.

```bash
python -m pytest sdks/python/tests -q
```

Note: `server_demo` registers no interrupting graph, so the interrupt/resume round trip is a documented skip in the suite; the client's resume path is `run_wait(tid, command={"resume": value})`.

## License

Dual-licensed under MIT OR Apache-2.0, same as the rest of the repo.
