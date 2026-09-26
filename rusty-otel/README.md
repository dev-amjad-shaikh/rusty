# rusty-otel

OpenTelemetry export layer for the [`rusty-agent-runtime`](../rusty-core)
engine: one call installs a global `tracing` subscriber that routes the
executor's structured spans to pretty stderr logs and — optionally — an
OTLP/HTTP span exporter feeding any OTel-compatible backend (collector,
Jaeger, Tempo, ...).

## What gets traced

Rusty's executor is already instrumented; this crate only wires the export.
The span taxonomy emitted by `rusty-agent-runtime` v0.4.0:

| Span / event            | Level | Fields                                   | Meaning                                            |
|-------------------------|-------|------------------------------------------|----------------------------------------------------|
| `rusty.run`             | INFO  | `thread_id`, `max_steps`                 | One per `Executor::run` call; root of the trace.   |
| `rusty.super_step`      | DEBUG | `step`, `active_nodes`                   | One per Pregel/BSP super-step (plan → barrier → merge → route → checkpoint). |
| `rusty.node`            | INFO  | `node`, `step`                           | One per spawned node task (attached via `.instrument()`). |
| barrier-merge event     | DEBUG | channels written                         | Reducer merge at each super-step barrier.          |
| run-complete event      | INFO  | `steps`, `duration_ms`                   | Run finished.                                      |
| interrupt event         | INFO  | `node`, `step`                           | Run interrupted (human-in-the-loop).               |
| error events            | WARN  | `node`, `step`, `error`, `retryable`     | Node/routing failures.                             |

Because spans nest (`run` → `super_step` → `node`), a single `rusty.run`
trace in Jaeger fans out into the full execution tree with `thread_id`,
`step`, and `node` attributes on every span.

## Setup

### Local only (no collector)

```rust
let _guard = rusty_otel::init_local("my-agent")?;
```

Pretty span logs go to stderr, filtered by `RUST_LOG` or the built-in
default `info,rusty_agent_runtime=debug` (which surfaces the DEBUG
`super_step` spans without flooding other crates). The filter is
**per-layer**: it gates the stderr logs only and never throttles OTLP span
export, so a restrictive `RUST_LOG=warn` still ships the full trace tree to
the collector.

### With a collector (OTLP export)

```rust
let mut guard = rusty_otel::init(rusty_otel::OTelConfig {
    service_name: "my-agent".into(),
    otlp_endpoint: Some("http://localhost:4318/v1/traces".into()),
    log_filter: None, // RUST_LOG, else the default above
})?;

// ... run graphs ...

guard.shutdown(); // flush buffered spans before exit (idempotent; also on drop)
```

Notes:

- `init` may succeed **once per process** (the subscriber is global). A
  second call returns `OTelError::SubscriberAlreadyInstalled`.
- The exporter batches spans on a dedicated thread, so `init` works with or
  without a running Tokio runtime.
- Keep the `OTelGuard` alive until shutdown/drop or batched spans are lost.

### See the traces: docker compose

This crate ships a ready-made local stack (`docker-compose.yml` +
`otel-collector-config.yaml`): an OTel Collector receiving OTLP on
`4317`/`4318`, forwarding to Jaeger all-in-one:

```sh
docker compose up -d

# Run the demo against it (2-node pipeline + ReAct agent with a mock LLM):
OTEL_DEMO_ENDPOINT=http://localhost:4318/v1/traces cargo run --example otel_demo

# Open the Jaeger UI and select the `rusty-otel-demo` service:
open http://localhost:16686

# Tear down:
docker compose down
```

Without the endpoint variable, `cargo run --example otel_demo` runs fmt-only
so you can still see the span tree on stderr. The collector also logs span
summaries via its `debug` exporter (`docker compose logs -f otel-collector`).

## API

| Item                              | Purpose                                                        |
|-----------------------------------|----------------------------------------------------------------|
| `init(OTelConfig) -> Result<OTelGuard>` | Install the global subscriber: filtered `fmt` layer on stderr (+ unfiltered OTLP layer when `otlp_endpoint` is set). |
| `init_local(&str) -> Result<OTelGuard>` | Shorthand for fmt-only local logging.                      |
| `OTelConfig { service_name, otlp_endpoint, log_filter }` | Service name (OTel `service.name` resource), optional OTLP/HTTP endpoint, optional filter directive. |
| `OTelGuard::shutdown(&mut self)`  | Flush + shut down the tracer provider. Idempotent; also runs on drop. |
| `OTelError`                       | `SubscriberAlreadyInstalled` (second `init`), `ExporterBuild` (bad endpoint/config), or `FilterParse` (invalid `log_filter` directive). |
| `DEFAULT_FILTER`                  | `info,rusty_agent_runtime=debug` — the fallback `EnvFilter`.  |

## Tests

```sh
cargo test
```

Covers once-per-process init (second init errors gracefully), `EnvFilter`
resolution (explicit → `RUST_LOG` → default), and idempotent guard shutdown.
No live collector is required.
