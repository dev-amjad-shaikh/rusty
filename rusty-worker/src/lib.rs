//! # rusty-worker
//!
//! The worker-side SDK for `rusty-agent-runtime` remote node execution: *one
//! `Node` trait, remote impls behind the same trait*. A worker is just an HTTP
//! service that hosts [`Node`] handlers by name; a graph node registered as
//! [`rusty_agent_runtime::remote::RemoteNode`] calls into it transparently.
//!
//! ## Endpoints
//!
//! - `POST /execute` — accepts a JSON [`NodeTask`], dispatches to the handler
//!   registered under [`NodeTask::node`], and replies with a JSON
//!   [`NodeTaskResponse`]:
//!   - `Ok(output)` → `{ "output": ... }`
//!   - `Err(interrupt)` → `{ "interrupt": <value> }` (HITL across the wire)
//!   - `Err(e)` → `{ "error": "<message>" }`
//! - `GET /ok` — liveness + capability probe: protocol version and the
//!   registered handler names.
//!
//! ## Registering handlers
//!
//! [`WorkerRegistry::register`] accepts **anything that implements
//! [`Node`]** — which, thanks to the blanket impl in the core crate, includes
//! ordinary async closures `Fn(NodeContext) -> impl Future<Output =
//! Result<NodeOutput>>`, named `Node` impls, and `Arc<dyn Node>`.
//!
//! ```no_run
//! use rusty_agent_runtime::prelude::*;
//! use rusty_worker::{serve, WorkerRegistry};
//!
//! # async fn demo() -> std::io::Result<()> {
//! let mut registry = WorkerRegistry::new();
//! registry.register("greeter", |ctx: NodeContext| async move {
//!     let name = ctx
//!         .state()
//!         .get("name")
//!         .and_then(|v| v.as_str())
//!         .unwrap_or("world")
//!         .to_string();
//!     Ok(NodeOutput::update("greeting", serde_json::json!(format!("hello, {name}!"))))
//! });
//!
//! serve(registry, "127.0.0.1:8200").await
//! # }
//! ```
//!
//! On the graph side, point a `RemoteNode` at the same handler name:
//!
//! ```ignore
//! builder.add_node("greeter", RemoteNode::new("greeter", "http://127.0.0.1:8200"));
//! ```
//!
//! ## Error semantics across the wire
//!
//! Handler errors are flattened to a message string in
//! [`NodeTaskResponse::error`] and arrive client-side as
//! `RustyError::Node`, which the executor treats as a **hard failure** —
//! the retryable classes (`Llm`, `Tool`) do not survive the wire. A remote
//! node whose transient failures should be retried must therefore rely on
//! transport-level retry (connection/timeout/5xx on the client) or surface
//! retryable outcomes through its own protocol on top of `extra`.
//!
//! ## Durable activities (R0.6)
//!
//! [`ActivityWorker`] is the pull-based counterpart to [`serve`]: it claims
//! leased tasks (`kind` + JSON `payload`) from the rusty-agent-server task queue
//! (`POST /tasks/claim`), executes the [`activity::Activity`] registered for
//! the task's kind while a background heartbeat renews the lease every
//! `lease / 3`, and settles with `complete` or a classified `fail`. Lease
//! loss (`409`) aborts the activity via a `CancellationToken`, and
//! cancelling the shutdown token drains the worker: claiming stops, the
//! in-flight activity settles within a bounded grace (default
//! [`activity::DEFAULT_DRAIN_GRACE`]), and an attempt that outlives the
//! grace is aborted and left for the server to reassign at lease expiry.
//! See the [`activity`] module for the protocol and semantics.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::State as AxumState;
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::{get, post};
use axum::Router;
use rusty_agent_runtime::error::RustyError;
use rusty_agent_runtime::node::{Node, NodeContext};
use rusty_agent_runtime::remote::{NodeTask, NodeTaskResponse, PROTOCOL_VERSION};
use serde::Serialize;
use serde_json::{json, Value};
use tracing::Instrument;
use uuid::Uuid;

pub mod activity;

pub use activity::{Activity, ActivityCompletion, ActivityContext, ActivityWorker};

/// The registry of named node handlers a worker serves.
///
/// Cheap to clone (handlers are `Arc`'d); build one up front, then hand it to
/// [`router`] or [`serve`].
#[derive(Clone, Default)]
pub struct WorkerRegistry {
    handlers: HashMap<String, Arc<dyn Node>>,
}

impl WorkerRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a handler under `name`.
    ///
    /// Accepts any [`Node`] implementation — including plain async closures
    /// via the core blanket impl, so the ergonomics match
    /// `GraphBuilder::add_node` exactly.
    ///
    /// Registering the same name twice replaces the previous handler.
    pub fn register<N>(&mut self, name: impl Into<String>, node: N) -> &mut Self
    where
        N: Node + 'static,
    {
        self.handlers.insert(name.into(), Arc::new(node));
        self
    }

    /// Builder-style variant of [`WorkerRegistry::register`].
    pub fn with<N>(mut self, name: impl Into<String>, node: N) -> Self
    where
        N: Node + 'static,
    {
        self.register(name, node);
        self
    }

    /// `true` if a handler is registered under `name`.
    pub fn contains(&self, name: &str) -> bool {
        self.handlers.contains_key(name)
    }

    /// Number of registered handlers.
    pub fn len(&self) -> usize {
        self.handlers.len()
    }

    /// `true` if no handlers are registered.
    pub fn is_empty(&self) -> bool {
        self.handlers.is_empty()
    }

    /// All registered handler names.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.handlers.keys().map(String::as_str)
    }

    /// Look up a handler by name.
    pub fn handler(&self, name: &str) -> Option<Arc<dyn Node>> {
        self.handlers.get(name).cloned()
    }
}

/// The shared state handed to axum handlers.
type SharedRegistry = Arc<WorkerRegistry>;

/// Liveness response for `GET /ok`.
#[derive(Debug, Serialize)]
struct OkResponse {
    status: &'static str,
    protocol_version: u32,
    nodes: Vec<String>,
}

/// `GET /ok`: liveness + capability probe.
async fn ok_handler(AxumState(registry): AxumState<SharedRegistry>) -> Json<OkResponse> {
    let mut nodes: Vec<String> = registry.names().map(str::to_owned).collect();
    nodes.sort();
    Json(OkResponse {
        status: "ok",
        protocol_version: PROTOCOL_VERSION,
        nodes,
    })
}

/// `POST /execute`: dispatch a [`NodeTask`] to its handler and shape the
/// outcome as a [`NodeTaskResponse`].
///
/// Status codes:
///
/// - `200 OK` for all handler-level outcomes (success, handler error,
///   interrupt, unknown handler, handler panic) — outcome lives in the
///   response body, so `RemoteNode` never mistakes a worker-side application
///   error for a transport failure. A panicking handler is caught and
///   returned as an error body rather than dropped connection, because the
///   client retries transport failures and node logic must not be replayed
///   silently.
/// - `400 Bad Request` when the protocol version is unsupported (a
///   client/worker mismatch the client should not retry blindly).
async fn execute_handler(
    AxumState(registry): AxumState<SharedRegistry>,
    Json(task): Json<NodeTask>,
) -> (StatusCode, Json<NodeTaskResponse>) {
    let request_id = Uuid::new_v4();
    let span = tracing::info_span!(
        "rusty.execute",
        %request_id,
        node = %task.node,
        thread_id = %task.config.thread_id,
        step = task.config.step,
        protocol_version = task.protocol_version,
    );
    // Attached via `.instrument()` (never `.enter()`) so no span guard is
    // held across `.await` points — the same discipline as the core
    // executor, which otherwise misattributes spans on multi-threaded
    // runtimes.
    execute_task(registry, task).instrument(span).await
}

/// The body of `execute_handler`, run inside the `rusty.execute` span.
async fn execute_task(
    registry: SharedRegistry,
    task: NodeTask,
) -> (StatusCode, Json<NodeTaskResponse>) {
    if task.protocol_version != PROTOCOL_VERSION {
        tracing::warn!("unsupported protocol version");
        return (
            StatusCode::BAD_REQUEST,
            Json(NodeTaskResponse::error(format!(
                "unsupported protocol_version {} (this worker speaks {})",
                task.protocol_version, PROTOCOL_VERSION
            ))),
        );
    }

    let Some(handler) = registry.handler(&task.node) else {
        tracing::warn!("no handler registered for node");
        let mut registered: Vec<&str> = registry.names().collect();
        registered.sort_unstable();
        return (
            StatusCode::OK,
            Json(NodeTaskResponse::error(format!(
                "no handler registered for node `{}` on this worker (registered: {registered:?})",
                task.node,
            ))),
        );
    };

    let node_name = task.node.clone();
    let resuming = task.config.resume.is_some();
    let ctx = NodeContext::new(task.state, task.config);
    // The handler runs on its own task so a panic surfaces as a `JoinError`
    // instead of tearing down the connection: a dropped connection reaches
    // the client as a *transport* failure, which `RemoteNode` retries —
    // silently replaying node logic the protocol says must never be
    // replayed. Mapping the panic to a 200 + error body keeps it a hard,
    // non-retried node failure.
    let outcome = tokio::spawn(async move { handler.run(ctx).await }).await;
    let result = match outcome {
        Ok(result) => result,
        Err(join_err) => {
            let detail = if join_err.is_panic() {
                let payload = join_err.into_panic();
                let message = payload
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_owned())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "non-string panic payload".to_owned());
                format!("handler panicked: {message}")
            } else {
                // Only reachable if the runtime is shutting down mid-request.
                "handler task cancelled".to_owned()
            };
            Err(RustyError::Node(format!("node `{node_name}` {detail}")))
        }
    };
    match result {
        Ok(output) => {
            tracing::info!(resuming, "node executed");
            (StatusCode::OK, Json(NodeTaskResponse::ok(output)))
        }
        Err(e) if e.is_interrupt() => {
            let value = e.interrupt_value().cloned().unwrap_or(Value::Null);
            tracing::info!(payload = %value, "node interrupted");
            (StatusCode::OK, Json(NodeTaskResponse::interrupt(value)))
        }
        Err(e) => {
            tracing::warn!(error = %e, "node failed");
            (StatusCode::OK, Json(NodeTaskResponse::error(e.to_string())))
        }
    }
}

/// Build the axum [`Router`] for a registry (`POST /execute` + `GET /ok`).
///
/// Exposed separately from [`serve`] so tests and embedders can bind their
/// own listener (e.g. an ephemeral port) and drive the app with
/// `axum::serve`.
pub fn router(registry: WorkerRegistry) -> Router {
    Router::new()
        .route("/execute", post(execute_handler))
        .route("/ok", get(ok_handler))
        .with_state(Arc::new(registry))
}

/// Serve a registry on `addr` until the process is stopped.
///
/// ```no_run
/// # use rusty_worker::{serve, WorkerRegistry};
/// # async fn demo() -> std::io::Result<()> {
/// serve(WorkerRegistry::new(), "127.0.0.1:8200").await
/// # }
/// ```
pub async fn serve(registry: WorkerRegistry, addr: impl AsRef<str>) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr.as_ref()).await?;
    let local_addr = listener.local_addr()?;
    tracing::info!(
        addr = %local_addr,
        nodes = ?registry.names().collect::<Vec<_>>(),
        "rusty worker listening"
    );
    axum::serve(listener, router(registry)).await
}

/// Convenience JSON body for quick manual probes (`curl` examples in docs).
pub fn probe_body() -> Value {
    json!({
        "protocol_version": PROTOCOL_VERSION,
        "node": "<handler-name>",
        "state": {},
        "config": { "thread_id": "t-1", "step": 0, "resume": null, "extra": {} }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusty_agent_runtime::node::NodeOutput;
    use rusty_agent_runtime::state::State;
    use serde_json::json;

    #[test]
    fn registry_register_and_lookup() {
        let mut registry = WorkerRegistry::new();
        assert!(registry.is_empty());

        registry.register("a", |_ctx: NodeContext| async { Ok(NodeOutput::empty()) });
        registry.register("b", |_ctx: NodeContext| async { Ok(NodeOutput::empty()) });

        assert_eq!(registry.len(), 2);
        assert!(registry.contains("a"));
        assert!(registry.contains("b"));
        assert!(!registry.contains("c"));
        assert!(registry.handler("a").is_some());

        let mut names: Vec<&str> = registry.names().collect();
        names.sort();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn registry_builder_style_and_replace() {
        let registry = WorkerRegistry::new()
            .with("x", |_ctx: NodeContext| async {
                Ok(NodeOutput::update("v", json!(1)))
            })
            .with("x", |_ctx: NodeContext| async {
                Ok(NodeOutput::update("v", json!(2)))
            });
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn probe_body_is_a_valid_node_task_shape() {
        let body = probe_body();
        let task: std::result::Result<NodeTask, _> = serde_json::from_value(body);
        let task = task.unwrap();
        assert_eq!(task.protocol_version, PROTOCOL_VERSION);
        assert_eq!(task.state, State::new());
    }
}
