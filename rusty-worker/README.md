# Rusty Worker

Worker-side SDK for [`rusty-agent-runtime`](../rusty-core) remote node
execution: *one `Node` trait, remote impls behind the same trait*. A worker is
an HTTP service that hosts `Node` handlers by name; a graph node registered as
`RemoteNode` calls into it transparently.

## Endpoints

- `POST /execute` — accepts a JSON `NodeTask`, dispatches to the handler
  registered under `NodeTask::node`, and replies with a JSON
  `NodeTaskResponse`:
  - `Ok(output)` → `{ "output": ... }`
  - `Err(interrupt)` → `{ "interrupt": <value> }` (HITL across the wire)
  - `Err(e)` → `{ "error": "<message>" }`
- `GET /ok` — liveness + capability probe: protocol version and the
  registered handler names (sorted).

Status codes:

- `200 OK` for all handler-level outcomes (success, handler error, interrupt,
  unknown handler, handler panic) — the outcome lives in the body, so
  `RemoteNode` never mistakes a worker-side application error for a
  transport failure. Handler panics are caught and returned as an error body:
  a dropped connection would read as a transport failure client-side and be
  retried, silently replaying node logic.
- `400 Bad Request` when the protocol version is unsupported — a
  client/worker mismatch the client treats as fatal (never retried).

## Registering handlers

`WorkerRegistry::register` accepts **anything that implements `Node`** —
which, thanks to the blanket impl in the core crate, includes ordinary async
closures `Fn(NodeContext) -> impl Future<Output = Result<NodeOutput>>`,
named `Node` impls, and `Arc<dyn Node>`. The ergonomics match
`GraphBuilder::add_node` exactly; registering the same name twice replaces
the previous handler.

```rust,no_run
use rusty_agent_runtime::prelude::*;
use rusty_worker::{serve, WorkerRegistry};

# async fn demo() -> std::io::Result<()> {
let mut registry = WorkerRegistry::new();
registry.register("greeter", |ctx: NodeContext| async move {
    let name = ctx
        .state()
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("world")
        .to_string();
    Ok(NodeOutput::update("greeting", serde_json::json!(format!("hello, {name}!"))))
});

serve(registry, "127.0.0.1:8200").await
# }
```

On the graph side, point a `RemoteNode` at the same handler name:

```rust,ignore
builder.add_node("greeter", RemoteNode::new("greeter", "http://127.0.0.1:8200"));
```

## Error semantics across the wire

Handler errors are flattened to a message string in `NodeTaskResponse::error`
and arrive client-side as `RustyError::Node`, which the executor treats
as a **hard failure** — the retryable classes (`Llm`, `Tool`) do not survive
the wire. A remote node whose transient failures should be retried must rely
on transport-level retry (connection/timeout/5xx on the client) or surface
retryable outcomes through its own protocol on top of the `extra` config
channel.

## Durable activities (R0.6): `ActivityWorker`

`ActivityWorker` is the pull-based counterpart to `serve`: instead of
answering one-shot `/execute` calls, it claims leased tasks from the
rusty-agent-server task queue, executes the `Activity` registered for the task's
`kind` while a background heartbeat renews the lease, and settles the task
with a complete or fail call. The server re-queues tasks whose lease
expires, so a crashed worker never strands work — the promise is
*effectively-once* execution when activity side effects are idempotent, not
universal exactly-once. (The full design lives in
[`docs/durable-work-design.md`](../docs/durable-work-design.md); the failure
taxonomy is the frozen `rusty_agent_runtime::durable::ErrorClass`.)

A task is a generic unit of durable work — a `kind` label plus an arbitrary
JSON `payload`. The handler receives an `ActivityContext` (`task_id`,
`attempt`, `idempotency_key`, `payload`) and returns the JSON value stored
as the task's `result`.

Protocol (client side of the server task endpoints):

| Call | Body | Success | Meaning of `409` |
|------|------|---------|------------------|
| `POST {base}/tasks/claim` | `{worker_id, pools?, lease_ms}` | `200 {"task": {…}}` (the task record); `204` = no work | — |
| `POST {base}/tasks/{id}/heartbeat` | `{worker_id, lease_ms}` (every `lease / 3`) | `200 {lease_expires_at}` | lease lost → abort the activity, no settle call |
| `POST {base}/tasks/{id}/complete` | `{worker_id, result}` (any JSON) | `200` with the updated task record | task already lost/settled → outcome dropped |
| `POST {base}/tasks/{id}/fail` | `{worker_id, error_class, message, retryable}` | `200 {requeued, next_attempt_at, dead}` | task already lost/settled → outcome dropped |

Semantics:

- **One activity in flight** per worker loop; run several `ActivityWorker`
  instances for parallelism and route them with `with_pools`.
- **Lease loss aborts execution**: on a heartbeat `409` the handler future
  is aborted (dropped at its next yield point, via a `CancellationToken`)
  and the worker returns to claiming without settling — the server's
  reassignment is authoritative. Handlers must tolerate being abandoned
  mid-effect and should key external side effects by
  `ActivityContext::task_id()` / `idempotency_key()` so redelivery is
  effectively-once.
- **Graceful drain** (R0.6 wave 2c): cancelling the shutdown
  `CancellationToken` *is* the drain request — idempotent, and race-safe
  against an in-flight claim poll. Draining stops claiming immediately; an
  in-flight activity keeps heartbeating and settles normally, bounded by
  `with_drain_grace` (default `DEFAULT_DRAIN_GRACE`, 25 s — under the 30 s
  default lease and Kubernetes' 30 s pod-termination grace). An attempt
  that outlives the grace is aborted and deliberately **not** settled:
  reporting `cancelled` would kill the task (terminal, never retried),
  while leaving it unsettled releases it back to visibility at lease
  expiry for a worker that is still serving. Wire the token to
  SIGTERM/SIGINT in your binary — see
  `examples/activity_worker_demo.rs` for the idiomatic wiring.
- **Failure classification** uses the shared `ErrorClass` taxonomy: `Llm`
  and `Tool` errors reach `/fail` as `dependency_failure` with
  `retryable: true` (the transient executor classes); `Graph` /
  `InvalidUpdate` map to `invalid_input`; everything else is `unknown` with
  `retryable: false`. An `Interrupt` error settles as a non-retryable
  `cancelled` — the task-queue protocol has no suspend semantics (HITL
  wiring is the run-outbox wave's concern).
- Claim/heartbeat/settle **transport failures are retried forever** with
  capped exponential backoff on the claim poll; in a durable system the
  server coming back is the normal case.
- `with_lease` clamps to the server's accepted range (100 ms – 1 h).

```rust,no_run
use std::time::Duration;
use rusty_worker::{ActivityContext, ActivityWorker};
use tokio_util::sync::CancellationToken;

# async fn demo() {
let worker = ActivityWorker::new("http://127.0.0.1:8080")
    .register("send_receipt", |ctx: ActivityContext| async move {
        // The task id / idempotency key make redelivery effectively-once.
        let dedup_key = ctx.idempotency_key().unwrap_or_else(|| ctx.task_id()).to_string();
        let to = ctx.payload()["to"].as_str().unwrap_or("unknown").to_string();
        // … perform the effect, keyed by `dedup_key` …
        Ok(serde_json::json!({"sent": true, "to": to}))
    })
    .with_worker_id("email-worker-1")
    .with_lease(Duration::from_secs(30))
    .with_pools(["email"]);

// Cancel the token (e.g. from a SIGTERM handler) to drain and exit.
worker.run(CancellationToken::new()).await;
# }
```

## API

| Item                                        | Purpose                                             |
|---------------------------------------------|-----------------------------------------------------|
| `WorkerRegistry`                            | Named `Node` handlers; `new` / `register` / `with` / `contains` / `len` / `names` / `handler`. |
| `router(registry) -> Router`                | axum router (`POST /execute` + `GET /ok`) for embedding or tests with an ephemeral listener. |
| `serve(registry, addr) -> io::Result<()>`   | Bind and serve until the process stops.             |
| `probe_body() -> Value`                     | A valid `NodeTask` JSON body with the current `PROTOCOL_VERSION`, for manual `curl` probes. |
| `ActivityWorker`                            | Pull-based durable worker: `new` / `register` / `with_worker_id` / `with_lease` / `with_pools` / `with_claim_backoff` / `with_drain_grace` / `worker_id` / `run(shutdown)`. |
| `Activity`, `ActivityContext`               | One durable unit of work (closure-implementable) and its input: `task_id` / `kind` / `attempt` / `idempotency_key` / `payload`. |
| `activity::ClaimedTask`                     | The claim envelope fields a worker consumes.        |
| `activity::DEFAULT_LEASE` and friends       | Defaults for lease, request timeout, claim backoff, and drain grace. |

## Demo

```sh
cargo run --example worker_demo
```

Serves a `greeter` handler and an interrupting `approval_gate` HITL handler
on `127.0.0.1:8200`, and prints the matching `RemoteNode` wiring and `curl`
probe commands.

## Tests

```sh
cargo test
```

Unit tests cover the registry (register/replace/builder, probe shape) and the
activity wire shapes (claim/heartbeat/complete/fail bodies, the `ErrorClass`
classification, the claim-envelope decode); the remote e2e suite runs real
graphs mixing local and remote nodes through the actual `Executor` — including
an interrupt → resume round trip across the wire — plus the HTTP-layer
contract (protocol-version 400, unknown handler, handler error, and handler
panic all as 200 + one-payload error bodies). The activity e2e suite drives
`ActivityWorker` against a mock task queue implementing the lease contract
exactly: claim → execute → complete, heartbeats keeping the lease, `409`
mid-run aborting the activity, graceful drain finishing in-flight work,
drain-grace expiry aborting the attempt and releasing it for reassignment
(unsettled — never `cancelled`), drain idempotence, no new claims after
drain starts even with tasks queued, failure classification with the
retryable flag, interrupt-as-cancellation,
panics, unknown kinds, and undecodable claim bodies.
