# Rusty SDK — TypeScript client

Zero-dependency JS/TS client for [`rusty-server`](../../rusty-server) — the HTTP + SSE face of Rusty graphs. ESM-only, works in **Node.js >= 18** and **modern browsers** (global `fetch`, `ReadableStream`, `TextDecoder`, `AbortController`). Hand-written TypeScript declarations included; full JSDoc in the source.

```bash
npm install @rusty-runtime/client

# from a checkout: no build step, import it directly
node --test test/   # e2e suite (builds + launches the real server demo binary)
```

## Quickstart

```js
import { RustyClient } from '@rusty-runtime/client';

const client = new RustyClient('http://localhost:8100', {
  // apiKey: '…',   // sent as X-Api-Key when the server configures one
  // timeout: 30_000, // ms per request (default); 0 disables
});

// What's registered?
const info = await client.info();
// { service: 'rusty-server', version: '0.4.0', graphs: [{ name: 'pipeline', channels: ['log'] }, …] }

// Create a thread bound to a graph and run it to completion
const { thread_id } = await client.createThread('react_agent');
const terminal = await client.runWait(thread_id, {
  input: { messages: [{ role: 'user', content: 'What is 17 + 25?' }] },
});
console.log(terminal.status, terminal.output);

// Stream a run over SSE
for await (const frame of client.runStream(thread_id, { input: { /* … */ } })) {
  // frame: { event: 'metadata'|'updates'|'values'|'messages'|'error'|'end', data, id? }
  if (frame.event === 'end') console.log('done:', frame.data.status);
}

// Durable task queue (R0.6) — control plane: submit, observe, cancel
const enqueued = await client.tasks.enqueue('send_email', { to: 'user@example.com' }, {
  idempotencyKey: 'welcome-42',        // re-enqueueing dedupes on this key
  effect: 'idempotent',                // declares the work is safe to retry
  deadline: '2026-08-11T00:00:00Z',    // RFC 3339, across attempts
});
// → { task_id, deduplicated: false }

const task = await client.tasks.get(enqueued.task_id);   // the full TaskRecord
await client.tasks.list();                               // all tasks, oldest first
await client.tasks.list({ status: 'dead' });             // the dead-letter queue
await client.tasks.cancel(enqueued.task_id);             // 409 if already terminal
await client.tasks.cancelRunTasks(runId);                // cancel a run's open tasks
```

## API surface

| Method | HTTP | Returns |
|---|---|---|
| `ok()` | `GET /ok` | `{ ok: true }` |
| `info()` | `GET /info` | service version, checkpointer, registered graphs |
| `createThread(graph, { metadata?, threadId? })` | `POST /threads` | thread record (`thread_id`, …) |
| `getState(threadId)` | `GET /threads/{id}/state` | latest checkpoint `{ values, next, checkpoint }` |
| `updateState(threadId, values, { asNode?, nextNodes? })` | `POST /threads/{id}/state` | written checkpoint state |
| `history(threadId, { limit?, before? })` | `POST /threads/{id}/history` | checkpoints, newest first |
| `fork(threadId, { newThreadId?, checkpointId? })` | `POST /threads/{id}/fork` | `{ thread_id, checkpoints_copied }` |
| `run(threadId, payload)` | `POST /threads/{id}/runs` | background run record (`run_id`, `status`) |
| `runWait(threadId, payload, { timeout? })` | `POST /threads/{id}/runs/wait` | terminal JSON (`status`, `output` ‖ `interrupt`) |
| `runStream(threadId, payload, options)` | `POST /threads/{id}/runs/stream` | async generator of SSE frames |
| `runStatus(runId)` | `GET /runs/{run_id}` | run status (+ `output`/`error`/`interrupt` when terminal) |
| `runEvents(runId)` | `GET /runs/{id}/events` | `{ run_id, events, complete }` (Flight Recorder journal) |
| `getFixture(runId)` | `GET /runs/{id}/fixture` | portable `ReplayFixture` bundle for CI replay |
| `replayRun(runId)` | `POST /runs/replay` | `{ run_id, verified, expected_events, actual_events, first_divergence }` |
| `diffRuns(base, branch)` | `GET /runs/diff?base=…&branch=…` | `BranchDiff` (`first_divergent_seq`, `added`, `removed`, `step_diffs`, totals) |
| `deleteRun(threadId, runId)` | `DELETE /threads/{id}/runs/{run_id}` | rollback a finished run's checkpoints |
| `createAssistant({ name, graph, config?, metadata?, assistantId? })` | `POST /assistants` | assistant record |
| `listAssistants()` / `getAssistant(id)` | `GET /assistants[/{id}]` | assistant list / record |
| `createCron({ graph, intervalSecs‖cronExpr, input?, metadata?, onRunCompleted? })` | `POST /crons` | cron record |
| `listCrons()` / `deleteCron(id)` | `GET /crons` · `DELETE /crons/{id}` | cron list / delete |
| `kvGet(ns, key)` / `kvPut(ns, key, value)` / `kvDelete(ns, key)` / `kvList(ns)` | `GET/PUT/DELETE /store/{ns}/{key}` · `GET /store/{ns}` | KV items |
| `tasks.enqueue(kind, payload, { pool?, maxAttempts?, idempotencyKey?, effect?, runId?, threadId?, deadline? })` | `POST /tasks` | `{ task_id, deduplicated }` |
| `tasks.enqueueOutbox(kind, payload, …same opts…)` | `POST /tasks/outbox` | `202` `{ task_id, deduplicated }` |
| `tasks.get(taskId)` | `GET /tasks/{id}` | task record |
| `tasks.list({ status? })` | `GET /tasks[?status=…]` | task records, oldest first (`dead` = DLQ) |
| `tasks.cancel(taskId)` | `POST /tasks/{id}/cancel` | updated record (`409` when terminal) |
| `tasks.cancelRunTasks(runId)` | `POST /runs/{id}/cancel` | `{ run_id, cancelled, signalled }` |

**Durable tasks (R0.6).** `client.tasks` is the **control plane** of the durable task queue: submit work, observe records, cancel. Task records carry the full envelope — `kind`, `payload`, `pool`, `status` (`queued` / `leased` / `failed` / `completed` / `dead` / `cancelled`), `attempt` / `max_attempts`, the live `lease`, `error_class` + `last_error` from the last failed attempt, `idempotency_key`, `result` / `receipt` when settled, run/thread linkage, `cancel_requested`, `deadline`, and timestamps. `enqueue` makes a task claimable immediately; `enqueueOutbox` writes through the transactional outbox (202 accepted — the relay publishes it into the queue within one poll interval, deduped on the idempotency key). Cancellation is a hint for promptness: a queued or retry-scheduled task goes terminal-`cancelled` immediately, while a leased task keeps its lease with `cancel_requested` set so its holder aborts cleanly on the next heartbeat. The worker-machine endpoints (`claim` / `heartbeat` / `complete` / `fail`) are deliberately absent — they are lease-guarded by `worker_id` and belong to the worker SDK (`rusty-worker`'s `ActivityWorker`); a control-plane client never holds leases.

**Run payload** (all methods that take `payload`): `{ input, command: { resume }, config: { recursion_limit }, checkpoint: { checkpoint_id }, metadata, stream_mode, multitask_strategy, assistant_id }` — see the [server README](../../rusty-server/README.md#http-api) for semantics (resume for human-in-the-loop, `checkpoint_id` for replay).

### Streaming & resume

`runStream` yields parsed frames — multi-line `data:` is reassembled and JSON-parsed, `event:`/`id:` fields are preserved, comments/keepalives are skipped:

```js
let lastId;
for await (const frame of client.runStream(threadId, { input })) {
  lastId = frame.id;              // "{checkpoint_id}:{step}:{seq}"
  if (frame.event === 'values') render(frame.data);
}
// … reconnect later, skipping frames already seen:
const resumed = client.runStream(threadId, { input }, { lastEventId: lastId });
```

`options.streamMode` (e.g. `['updates', 'values']`) is a convenience for `payload.stream_mode`; `metadata`, `error`, and `end` frames are always emitted by the server.

### Time travel

```js
const history = await client.history(threadId);
const earliest = history.at(-1).checkpoint.checkpoint_id;
const { thread_id: forkId } = await client.fork(threadId, { checkpointId: earliest });
await client.runWait(forkId, { checkpoint: { checkpoint_id: earliest } }); // replay on the fork
```

## Errors, timeouts, aborts

- Non-2xx responses throw `RustyError` with `.status` (HTTP code) and `.body` (parsed JSON or raw text).
- Timeouts throw `RustyTimeoutError` (a subclass; `.status === 0`, `.timeoutMs` set). The client timeout is per request via `AbortController`; for `runStream` it covers **establishing** the stream, not its lifetime.
- Every method accepts `{ signal }` (and `runStream` takes `options.signal`) so you can abort with your own `AbortController`.

## Node vs browser notes

- **Node >= 18**: zero setup — global `fetch` (undici), web `ReadableStream`, and `TextDecoder` are built in. `engines` is pinned to `>=18`.
- **Browsers**: the server ships permissive CORS (`access-control-allow-origin: *`, preflights answered before auth), so the client works cross-origin out of the box — including from `file://` pages. SSE is consumed via `fetch` + `ReadableStream` (no `EventSource`), which is what makes `POST` streaming and the `Last-Event-ID` resume header possible; use a Chromium/Firefox/Safari version with streaming `fetch` support.
- **Custom fetch**: pass `{ fetch }` to the constructor for tests, proxies, or polyfills.
- ESM only (`"type": "module"`). From CommonJS use `await import('@rusty-runtime/client')`.

## Development

```bash
# Build the demo server (once, from the repo root) and run the e2e suite against it:
export PATH="$HOME/.cargo/bin:$PATH"
cargo build -p rusty-agent-server --example server_demo
node --test test/
```

The suite has two halves. `test/client.test.js` is e2e: it spawns the real `server_demo` binary (it binds `127.0.0.1:8100`, so that port must be free), polls `/ok`, exercises the full API — threads, blocking/background/streamed runs, SSE frame collection, fork + checkpoint replay, assistants, crons, KV CRUD, error shapes, timeouts — then kills the child and removes its scratch store. The 401 test self-skips because the demo binary runs with auth disabled. `test/tasks.test.js` is no-I/O: the tasks control plane (`client.tasks`) is exercised against a fake `fetch`, so it runs without the server — `node --test test/tasks.test.js`.

## License

MIT OR Apache-2.0, same as the server.
