//! Remote node execution: the worker wire protocol + [`RemoteNode`].
//!
//! Phase B of the distribution story: **one [`Node`] trait, remote impls
//! behind the same trait**. A [`RemoteNode`] is registered in a graph exactly
//! like any local node; when the executor runs it, the invocation is
//! serialized into a [`NodeTask`] and POSTed to a worker's `/execute`
//! endpoint. The worker replies with a [`NodeTaskResponse`] which is turned
//! back into a [`NodeOutput`] (or an error / interrupt) locally.
//!
//! ## Wire protocol (version 1)
//!
//! `POST {base_url}/execute` with a JSON [`NodeTask`]:
//!
//! ```json
//! {
//!   "protocol_version": 1,
//!   "node": "doubler",
//!   "state": { "n": 21 },
//!   "config": { "thread_id": "t-1", "step": 3, "resume": null, "extra": {} }
//! }
//! ```
//!
//! The response is a JSON [`NodeTaskResponse`] carrying **exactly one** of:
//!
//! - `output`   — the node's partial updates + optional routing command,
//! - `error`    — a worker-side failure message, surfaced locally as
//!   [`RustyError::Node`],
//! - `interrupt`— an interrupt payload, surfaced locally as
//!   [`RustyError::Interrupt`] so HITL works across the wire
//!   (a bare `{ "interrupt": <value> }` body is also accepted).
//!
//! ## Reliability
//!
//! [`RemoteNode`] applies a per-attempt timeout plus configurable retries
//! with capped, jittered exponential backoff. Only *transport-class*
//! failures are retried (connect errors, timeouts, HTTP 5xx / 408 / 429; a
//! `Retry-After` header floors the delay). Worker-reported errors and
//! interrupts are never retried — the worker already made a definitive
//! decision, and node logic is only contractually idempotent across
//! *executor-level* re-execution, not silent client replays.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Result, RustyError};
use crate::llm::{backoff_delay, truncate_body};
use crate::node::{Command, Node, NodeContext, NodeOutput};
use crate::state::State;

/// The wire protocol version spoken by this client.
///
/// Workers must reject tasks whose `protocol_version` they do not support.
/// Responses are accepted regardless of their version field so newer workers
/// can serve older clients (additive-only evolution within v1).
pub const PROTOCOL_VERSION: u32 = 1;

/// Default per-attempt HTTP timeout.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Default number of retries *after* the initial attempt.
pub const DEFAULT_MAX_RETRIES: u32 = 2;

/// Default base delay for exponential backoff (attempt *n* waits roughly
/// `base * 2^n`, exponent capped and jittered, before retry *n+1*).
pub const DEFAULT_BASE_BACKOFF: Duration = Duration::from_millis(100);

/// A node invocation sent to a worker (`POST {base_url}/execute`).
///
/// The same envelope can be embedded as the payload of a durable task
/// (Durable Work R0.6, `rusty_agent_runtime::durable::TaskEnvelope`) — in
/// that flow [`NodeTask::task_id`] carries the leased task's identity.
/// Direct `/execute` invocations built by [`RemoteNode`] always leave it
/// `None`, so previously written tasks keep deserializing unchanged
/// (additive-only evolution within v1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeTask {
    /// Protocol version of the sender. See [`PROTOCOL_VERSION`].
    pub protocol_version: u32,

    /// The handler name the worker should dispatch to. This is the *remote*
    /// identity of the node (the name registered in the worker's registry),
    /// which by [`RemoteNode`] convention equals the graph node name.
    pub node: String,

    /// The immutable super-step state snapshot (same visibility rules as a
    /// local node: this is the state as of the start of the super-step).
    pub state: State,

    /// The per-run, per-node configuration (thread id, step, resume value,
    /// extensions). Carried verbatim so `resume` round-trips across the wire.
    pub config: crate::node::NodeConfig,

    /// The durable task identity assigned by the server's task queue,
    /// present only when this invocation rides the durable-task flow (R0.6)
    /// rather than a direct `/execute` call. It lets a remote node handler
    /// correlate external side effects with the durable task (idempotency
    /// keys, receipts) when a run dispatches node work through the queue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
}

/// Serializable form of [`NodeOutput`].
///
/// [`NodeOutput`] itself is deliberately not `serde`-enabled in the core API;
/// this wire type converts in both directions losslessly.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WireNodeOutput {
    /// Partial state updates, keyed by channel name.
    #[serde(default)]
    pub updates: HashMap<String, Value>,

    /// Optional dynamic routing decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Command>,
}

impl From<NodeOutput> for WireNodeOutput {
    fn from(output: NodeOutput) -> Self {
        Self {
            updates: output.updates,
            command: output.command,
        }
    }
}

impl From<WireNodeOutput> for NodeOutput {
    fn from(wire: WireNodeOutput) -> Self {
        Self {
            updates: wire.updates,
            command: wire.command,
        }
    }
}

/// The worker's reply to a [`NodeTask`].
///
/// Exactly one of `output` / `error` / `interrupt` must be set; anything else
/// is a malformed response. All fields are optional on the wire so a minimal
/// worker can answer with e.g. `{ "interrupt": <value> }`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NodeTaskResponse {
    /// Protocol version of the responder. Informational on the client side
    /// (v1 evolution is additive-only); workers should always set it.
    #[serde(default)]
    pub protocol_version: u32,

    /// Success payload: the node's output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<WireNodeOutput>,

    /// Failure payload: a human-readable worker-side error message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,

    /// Suspend payload: the node called `interrupt(value)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interrupt: Option<Value>,
}

impl NodeTaskResponse {
    /// A successful response carrying the node's output.
    pub fn ok(output: impl Into<WireNodeOutput>) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            output: Some(output.into()),
            error: None,
            interrupt: None,
        }
    }

    /// A failure response. Surfaced client-side as [`RustyError::Node`].
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            output: None,
            error: Some(message.into()),
            interrupt: None,
        }
    }

    /// An interrupt (suspend) response. Surfaced client-side as
    /// [`RustyError::Interrupt`], so the executor's HITL machinery works
    /// unchanged for remote nodes.
    pub fn interrupt(value: Value) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            output: None,
            error: None,
            interrupt: Some(value),
        }
    }

    /// Convert into a local node result.
    ///
    /// Mapping:
    ///
    /// - `output`   → `Ok(NodeOutput)`
    /// - `error`    → `Err(RustyError::Node(message))`
    /// - `interrupt`→ `Err(RustyError::Interrupt { value })`
    ///
    /// Zero or multiple set payloads are a malformed-response node error.
    pub fn into_result(self) -> Result<NodeOutput> {
        let set = self.output.is_some() as u8
            + self.error.is_some() as u8
            + self.interrupt.is_some() as u8;
        if set != 1 {
            return Err(RustyError::Node(format!(
                "malformed worker response: expected exactly one of \
                 output/error/interrupt, found {set} set"
            )));
        }
        if let Some(value) = self.interrupt {
            return Err(RustyError::Interrupt { value });
        }
        if let Some(message) = self.error {
            return Err(RustyError::Node(message));
        }
        // `set == 1` guarantees `output` is the remaining possibility.
        Ok(self.output.expect("checked exactly-one above").into())
    }
}

/// Internal classification of a failed HTTP attempt.
#[derive(Debug)]
enum AttemptError {
    /// Transport-class failure eligible for retry (connect, timeout, 5xx,
    /// 408, 429), with an optional server-provided `Retry-After` floor.
    Retryable {
        error: RustyError,
        retry_after: Option<Duration>,
    },
    /// Definitive failure; never retried (other 4xx, decode errors, ...).
    Fatal(RustyError),
}

/// A [`Node`] that executes its work on a remote worker over HTTP.
///
/// Registered in a graph exactly like a local node — this is the whole point
/// of the design: *one `Node` trait, remote impls behind the same trait*.
///
/// **Error semantics across the wire.** A worker-side failure arrives as a
/// plain message string and flattens to [`RustyError::Node`] — a hard,
/// non-retryable failure. Even when the remote failure originated in the
/// worker's LLM or tool layer, that retryability does not survive the wire:
/// the executor's retry classification ([`RustyError::Llm`] /
/// [`RustyError::Tool`] are the transient classes) only applies to
/// errors raised by *local* nodes, and [`RustyError::Node`] is never in
/// it. This client's own retries cover transport-class failures only (see
/// the module-level reliability notes).
///
/// ```no_run
/// use std::time::Duration;
/// use rusty_agent_runtime::graph::GraphBuilder;
/// use rusty_agent_runtime::remote::RemoteNode;
///
/// let node = RemoteNode::new("doubler", "http://127.0.0.1:8200")
///     .with_timeout(Duration::from_secs(5))
///     .with_retries(2);
/// let mut builder = GraphBuilder::new();
/// builder.add_node("double", node);
/// ```
#[derive(Clone)]
pub struct RemoteNode {
    /// The handler name sent in [`NodeTask::node`].
    node_name: String,
    /// Full URL of the worker's execute endpoint (`{base}/execute`).
    execute_url: String,
    /// HTTP client (carries the per-attempt timeout).
    client: reqwest::Client,
    /// Per-attempt timeout (kept so `with_*` builders can rebuild `client`).
    timeout: Duration,
    /// Retries after the initial attempt.
    max_retries: u32,
    /// Base delay for exponential backoff.
    base_backoff: Duration,
}

impl RemoteNode {
    /// A remote node dispatched to the worker handler `node_name`, served by
    /// the worker at `base_url` (e.g. `"http://127.0.0.1:8200"`).
    ///
    /// A trailing `/` is trimmed and `/execute` appended. If `base_url`
    /// already ends in `/execute` it is used verbatim.
    pub fn new(node_name: impl Into<String>, base_url: impl Into<String>) -> Self {
        let base = base_url.into();
        let base = base.trim_end_matches('/');
        let execute_url = if base.ends_with("/execute") {
            base.to_owned()
        } else {
            format!("{base}/execute")
        };
        Self {
            node_name: node_name.into(),
            execute_url,
            client: Self::build_client(DEFAULT_TIMEOUT),
            timeout: DEFAULT_TIMEOUT,
            max_retries: DEFAULT_MAX_RETRIES,
            base_backoff: DEFAULT_BASE_BACKOFF,
        }
    }

    /// Override the per-attempt timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self.client = Self::build_client(timeout);
        self
    }

    /// Override the number of retries after the initial attempt
    /// (`0` = single attempt, no retries).
    pub fn with_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = max_retries;
        self
    }

    /// Override the base backoff delay (attempt *n* waits roughly
    /// `base * 2^n`, exponent capped and jittered).
    pub fn with_backoff(mut self, base_backoff: Duration) -> Self {
        self.base_backoff = base_backoff;
        self
    }

    /// The handler name sent to the worker.
    pub fn node_name(&self) -> &str {
        &self.node_name
    }

    /// The full execute-endpoint URL.
    pub fn execute_url(&self) -> &str {
        &self.execute_url
    }

    fn build_client(timeout: Duration) -> reqwest::Client {
        // Invariant: the builder only sets a timeout — no TLS roots, proxy,
        // or redirect config — and the rustls backend needs no platform
        // initialization, so construction cannot realistically fail. Kept
        // infallible to preserve `RemoteNode::new`'s non-`Result` signature.
        reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .expect("reqwest client builder with rustls must succeed")
    }

    /// One HTTP attempt. `Ok` means the worker replied with a well-formed
    /// [`NodeTaskResponse`] (which may still carry `error`/`interrupt`);
    /// `Err` is classified for retry. Error bodies are truncated
    /// ([`crate::llm::truncate_body`]) so a verbose worker cannot bloat logs.
    async fn try_once(
        &self,
        task: &NodeTask,
    ) -> std::result::Result<NodeTaskResponse, AttemptError> {
        let response = self
            .client
            .post(&self.execute_url)
            .json(task)
            .send()
            .await
            .map_err(|e| {
                let err = RustyError::Node(format!(
                    "remote node `{}`: POST {} failed: {e}",
                    self.node_name, self.execute_url
                ));
                if e.is_timeout() || e.is_connect() {
                    AttemptError::Retryable {
                        error: err,
                        retry_after: None,
                    }
                } else {
                    AttemptError::Fatal(err)
                }
            })?;

        let status = response.status();
        if !status.is_success() {
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.trim().parse::<u64>().ok())
                .map(Duration::from_secs);
            let body = response.text().await.unwrap_or_default();
            let err = RustyError::Node(format!(
                "remote node `{}`: worker at {} returned {status}: {}",
                self.node_name,
                self.execute_url,
                truncate_body(&body)
            ));
            // 5xx and 408/429 are transient by convention (same policy as
            // the LLM client); other 4xx are definitive.
            let retryable = status.is_server_error()
                || status == reqwest::StatusCode::REQUEST_TIMEOUT
                || status == reqwest::StatusCode::TOO_MANY_REQUESTS;
            return Err(if retryable {
                AttemptError::Retryable {
                    error: err,
                    retry_after,
                }
            } else {
                AttemptError::Fatal(err)
            });
        }

        response.json::<NodeTaskResponse>().await.map_err(|e| {
            AttemptError::Fatal(RustyError::Node(format!(
                "remote node `{}`: could not decode worker response: {e}",
                self.node_name
            )))
        })
    }
}

#[async_trait]
impl Node for RemoteNode {
    fn name(&self) -> &str {
        &self.node_name
    }

    fn effect(&self) -> crate::record::Effect {
        // A remote invocation crosses a process boundary and performs work
        // the runtime cannot inspect; the restrictive class applies until
        // workers declare narrower effects (a worker manifest field is the
        // R0.6+ mechanism for that).
        crate::record::Effect::NonIdempotent
    }

    async fn run(&self, ctx: NodeContext) -> Result<NodeOutput> {
        let task = NodeTask {
            protocol_version: PROTOCOL_VERSION,
            node: self.node_name.clone(),
            state: ctx.state().clone(),
            config: ctx.config().clone(),
            // Direct `/execute` calls are not leased; only the server's task
            // queue assigns `task_id` in the durable-task flow.
            task_id: None,
        };

        let mut attempt: u32 = 0;
        loop {
            match self.try_once(&task).await {
                Ok(response) => return response.into_result(),
                Err(AttemptError::Fatal(e)) => return Err(e),
                Err(AttemptError::Retryable { error, retry_after })
                    if attempt < self.max_retries =>
                {
                    // Capped exponent + jitter (crate::llm::backoff_delay):
                    // uncapped growth turns with_retries(20) into a ~14-hour
                    // sleep, and lockstep retries stampede a recovering
                    // worker.
                    let mut delay = backoff_delay(self.base_backoff, attempt);
                    if let Some(floor) = retry_after {
                        // A worker that says Retry-After knows better than
                        // our backoff guess.
                        delay = delay.max(floor);
                    }
                    tracing::warn!(
                        node = %self.node_name,
                        url = %self.execute_url,
                        attempt = attempt + 1,
                        max_retries = self.max_retries,
                        backoff_ms = delay.as_millis() as u64,
                        error = %error,
                        "remote node attempt failed; retrying"
                    );
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
                Err(AttemptError::Retryable { error, .. }) => return Err(error),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::NodeConfig;
    use serde_json::json;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::task::JoinHandle;

    // ---------- serde roundtrips ----------

    #[test]
    fn node_task_serde_roundtrip() {
        let task = NodeTask {
            protocol_version: PROTOCOL_VERSION,
            node: "doubler".into(),
            state: State::from_value(json!({"n": 21, "xs": [1, 2]})).unwrap(),
            config: NodeConfig {
                thread_id: "t-1".into(),
                step: 3,
                resume: Some(json!({"approved": true})),
                extra: HashMap::from([("tag".to_string(), json!("demo"))]),
            },
            task_id: Some("task-7".into()),
        };
        let json_str = serde_json::to_string(&task).unwrap();
        let back: NodeTask = serde_json::from_str(&json_str).unwrap();
        assert_eq!(back.protocol_version, PROTOCOL_VERSION);
        assert_eq!(back.node, "doubler");
        assert_eq!(back.state.get("n"), Some(&json!(21)));
        assert_eq!(back.config.thread_id, "t-1");
        assert_eq!(back.config.step, 3);
        assert_eq!(back.config.resume, Some(json!({"approved": true})));
        assert_eq!(back.config.extra.get("tag"), Some(&json!("demo")));
        assert_eq!(back.task_id.as_deref(), Some("task-7"));
    }

    #[test]
    fn node_task_task_id_is_additive_within_v1() {
        // Tasks written before `task_id` existed (the direct `/execute`
        // shape) must keep deserializing...
        let legacy = r#"{
            "protocol_version": 1,
            "node": "doubler",
            "state": {"n": 21},
            "config": {"thread_id": "t-1", "step": 3, "resume": null, "extra": {}}
        }"#;
        let task: NodeTask = serde_json::from_str(legacy).unwrap();
        assert_eq!(task.task_id, None);

        // ...and a task with no lease identity must serialize byte-for-byte
        // into the old shape (no `task_id` key) so old workers accept it.
        let task = NodeTask {
            protocol_version: PROTOCOL_VERSION,
            node: "doubler".into(),
            state: State::new(),
            config: NodeConfig::default(),
            task_id: None,
        };
        let value = serde_json::to_value(&task).unwrap();
        assert!(value.get("task_id").is_none());
    }

    #[test]
    fn node_task_response_serde_roundtrips_all_variants() {
        let ok = NodeTaskResponse::ok(
            NodeOutput::update("x", json!(1)).with_command(Command::goto("next")),
        );
        let back: NodeTaskResponse =
            serde_json::from_str(&serde_json::to_string(&ok).unwrap()).unwrap();
        assert_eq!(back.protocol_version, ok.protocol_version);
        assert_eq!(
            back.output.as_ref().unwrap().updates,
            ok.output.as_ref().unwrap().updates
        );
        assert_eq!(back.error, ok.error);
        assert_eq!(back.interrupt, ok.interrupt);
        assert_eq!(
            back.output.as_ref().unwrap().command.as_ref().unwrap().goto,
            vec!["next".to_string()]
        );

        let err = NodeTaskResponse::error("boom");
        let back: NodeTaskResponse =
            serde_json::from_str(&serde_json::to_string(&err).unwrap()).unwrap();
        assert_eq!(back.error.as_deref(), Some("boom"));
        assert!(back.output.is_none() && back.interrupt.is_none());

        let interrupt = NodeTaskResponse::interrupt(json!({"question": "approve?"}));
        let back: NodeTaskResponse =
            serde_json::from_str(&serde_json::to_string(&interrupt).unwrap()).unwrap();
        assert_eq!(back.interrupt, Some(json!({"question": "approve?"})));
        assert!(back.output.is_none() && back.error.is_none());
    }

    #[test]
    fn loose_interrupt_shape_parses_and_maps_to_interrupt() {
        // A minimal worker may reply with just `{ "interrupt": <value> }`.
        let response: NodeTaskResponse =
            serde_json::from_str(r#"{"interrupt": {"question": "approve?"}}"#).unwrap();
        let err = response.into_result().unwrap_err();
        assert!(err.is_interrupt());
        assert_eq!(
            err.interrupt_value(),
            Some(&json!({"question": "approve?"}))
        );
    }

    #[test]
    fn response_requires_exactly_one_payload() {
        let none = NodeTaskResponse::default();
        let err = none.into_result().unwrap_err();
        assert!(matches!(err, RustyError::Node(_)));
        assert!(err.to_string().contains("malformed"));

        let mut both = NodeTaskResponse::ok(NodeOutput::empty());
        both.error = Some("also an error".into());
        let err = both.into_result().unwrap_err();
        assert!(matches!(err, RustyError::Node(_)));
        assert!(err.to_string().contains("malformed"));
    }

    #[test]
    fn wire_node_output_converts_losslessly_both_ways() {
        let output = NodeOutput::update("a", json!(1))
            .with_update("b", json!([2, 3]))
            .with_command(Command::goto_many(["n1", "n2"]));
        let wire = WireNodeOutput::from(output.clone());
        let back: NodeOutput = wire.clone().into();
        assert_eq!(back.updates, output.updates);
        assert_eq!(
            back.command.as_ref().unwrap().goto,
            output.command.as_ref().unwrap().goto
        );
        // And through JSON.
        let wire2: WireNodeOutput =
            serde_json::from_str(&serde_json::to_string(&wire).unwrap()).unwrap();
        assert_eq!(wire2.updates, wire.updates);
        assert_eq!(
            wire2.command.as_ref().unwrap().goto,
            wire.command.as_ref().unwrap().goto
        );
    }

    // ---------- hand-rolled mock HTTP server (no extra deps) ----------

    /// What the mock should do with a request.
    enum MockBehavior {
        /// Reply with a status + JSON body.
        Respond { status: u16, body: String },
        /// Never reply (the connection just hangs) — exercises timeouts.
        Hang,
    }

    struct MockServer {
        addr: SocketAddr,
        attempts: Arc<AtomicUsize>,
        bodies: Arc<std::sync::Mutex<Vec<String>>>,
        _handle: JoinHandle<()>,
    }

    /// Start a mock HTTP/1.1 server on an ephemeral port. `handler` receives
    /// the 1-based attempt number and the request body.
    fn start_mock<F>(handler: F) -> MockServer
    where
        F: Fn(usize, String) -> MockBehavior + Send + Sync + 'static,
    {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let bodies: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));

        let attempts2 = attempts.clone();
        let bodies2 = bodies.clone();
        let handler = Arc::new(handler);
        let handle = tokio::spawn(async move {
            let listener = TcpListener::from_std(listener).unwrap();
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    continue;
                };
                let attempts = attempts2.clone();
                let bodies = bodies2.clone();
                let handler = handler.clone();
                tokio::spawn(async move {
                    let Some(body) = read_http_request(&mut stream).await else {
                        return; // malformed request: just drop the connection
                    };
                    let n = attempts.fetch_add(1, Ordering::SeqCst) + 1;
                    bodies.lock().unwrap().push(body.clone());
                    match handler(n, body) {
                        MockBehavior::Hang => {
                            // Hold the connection open forever (until dropped).
                            std::future::pending::<()>().await;
                        }
                        MockBehavior::Respond { status, body } => {
                            let reason = match status {
                                200 => "OK",
                                400 => "Bad Request",
                                500 => "Internal Server Error",
                                _ => "Status",
                            };
                            let response = format!(
                                "HTTP/1.1 {status} {reason}\r\n\
                                 content-type: application/json\r\n\
                                 content-length: {}\r\n\
                                 connection: close\r\n\
                                 \r\n\
                                 {body}",
                                body.len()
                            );
                            let _ = stream.write_all(response.as_bytes()).await;
                            let _ = stream.flush().await;
                        }
                    }
                });
            }
        });
        MockServer {
            addr,
            attempts,
            bodies,
            _handle: handle,
        }
    }

    /// Minimal HTTP/1.1 request reader: headers up to `\r\n\r\n`, then
    /// `content-length` bytes of body. Returns the body as a string.
    async fn read_http_request(stream: &mut tokio::net::TcpStream) -> Option<String> {
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 4096];
        // Read until end of headers.
        let header_end = loop {
            if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                break pos + 4;
            }
            let n = stream.read(&mut chunk).await.ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
        };
        let headers = String::from_utf8_lossy(&buf[..header_end]).to_lowercase();
        let content_length: usize = headers
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
        while buf.len() < header_end + content_length {
            let n = stream.read(&mut chunk).await.ok()?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        String::from_utf8(buf[header_end..header_end + content_length].to_vec()).ok()
    }

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    fn test_ctx() -> NodeContext {
        NodeContext::new(
            State::from_value(json!({"n": 21})).unwrap(),
            NodeConfig {
                thread_id: "t-test".into(),
                step: 2,
                resume: None,
                extra: HashMap::new(),
            },
        )
    }

    // ---------- RemoteNode over HTTP ----------

    #[tokio::test]
    async fn remote_node_success_over_http() {
        let server = start_mock(|_n, _body| MockBehavior::Respond {
            status: 200,
            body: serde_json::to_string(&NodeTaskResponse::ok(
                NodeOutput::update("doubled", json!(42)).with_command(Command::goto("final")),
            ))
            .unwrap(),
        });

        let node = RemoteNode::new("doubler", format!("http://{}", server.addr)).with_retries(0);
        let out = Node::run(&node, test_ctx()).await.unwrap();
        assert_eq!(out.updates.get("doubled"), Some(&json!(42)));
        assert_eq!(
            out.command.as_ref().unwrap().goto,
            vec!["final".to_string()]
        );
        assert_eq!(node.name(), "doubler");
        assert!(node.execute_url().ends_with("/execute"));
        assert_eq!(server.attempts.load(Ordering::SeqCst), 1);

        // The request carried a well-formed NodeTask.
        let sent = server.bodies.lock().unwrap()[0].clone();
        let task: NodeTask = serde_json::from_str(&sent).unwrap();
        assert_eq!(task.protocol_version, PROTOCOL_VERSION);
        assert_eq!(task.node, "doubler");
        assert_eq!(task.state.get("n"), Some(&json!(21)));
        assert_eq!(task.config.thread_id, "t-test");
        assert_eq!(task.config.step, 2);
    }

    #[tokio::test]
    async fn remote_node_retries_on_5xx_then_succeeds() {
        let server = start_mock(|n, _body| {
            if n < 3 {
                MockBehavior::Respond {
                    status: 500,
                    body: r#"{"error":"worker overloaded"}"#.to_string(),
                }
            } else {
                MockBehavior::Respond {
                    status: 200,
                    body: serde_json::to_string(&NodeTaskResponse::ok(NodeOutput::update(
                        "x",
                        json!("third time lucky"),
                    )))
                    .unwrap(),
                }
            }
        });

        let node = RemoteNode::new("flaky", format!("http://{}", server.addr))
            .with_retries(5)
            .with_backoff(Duration::from_millis(1));
        let out = Node::run(&node, test_ctx()).await.unwrap();
        assert_eq!(out.updates.get("x"), Some(&json!("third time lucky")));
        assert_eq!(server.attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn remote_node_gives_up_after_retries_exhausted() {
        let server = start_mock(|_n, _body| MockBehavior::Respond {
            status: 500,
            body: r#"{"error":"always down"}"#.to_string(),
        });

        let node = RemoteNode::new("down", format!("http://{}", server.addr))
            .with_retries(2)
            .with_backoff(Duration::from_millis(1));
        let err = Node::run(&node, test_ctx()).await.unwrap_err();
        assert!(matches!(err, RustyError::Node(_)));
        assert!(err.to_string().contains("500"));
        assert!(!err.is_interrupt());
        // 1 initial + 2 retries = 3 attempts.
        assert_eq!(server.attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn remote_node_timeout_is_retryable_and_bounded() {
        let server = start_mock(|_n, _body| MockBehavior::Hang);

        let node = RemoteNode::new("slow", format!("http://{}", server.addr))
            .with_timeout(Duration::from_millis(100))
            .with_retries(1)
            .with_backoff(Duration::from_millis(1));
        let started = std::time::Instant::now();
        let err = Node::run(&node, test_ctx()).await.unwrap_err();
        let elapsed = started.elapsed();

        assert!(matches!(err, RustyError::Node(_)));
        // 1 initial + 1 retry = 2 attempts.
        assert_eq!(server.attempts.load(Ordering::SeqCst), 2);
        // Two 100ms timeouts + backoff: must be far below the default 30s
        // timeout, and comfortably below 5s of wall clock.
        assert!(elapsed < Duration::from_secs(5), "took {elapsed:?}");
    }

    #[tokio::test]
    async fn remote_node_worker_error_maps_to_node_error_without_retry() {
        let server = start_mock(|_n, _body| MockBehavior::Respond {
            status: 200,
            body: serde_json::to_string(&NodeTaskResponse::error(
                "handler `doubler` panicked: divide by zero",
            ))
            .unwrap(),
        });

        let node = RemoteNode::new("doubler", format!("http://{}", server.addr))
            .with_retries(5)
            .with_backoff(Duration::from_millis(1));
        let err = Node::run(&node, test_ctx()).await.unwrap_err();
        match err {
            RustyError::Node(message) => {
                assert!(message.contains("divide by zero"), "got: {message}")
            }
            other => panic!("expected Node error, got {other:?}"),
        }
        // Worker errors are definitive: no retries despite with_retries(5).
        assert_eq!(server.attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn remote_node_interrupt_passes_through_without_retry() {
        let server = start_mock(|_n, _body| MockBehavior::Respond {
            status: 200,
            body: r#"{"interrupt": {"question": "approve deployment?"}}"#.to_string(),
        });

        let node = RemoteNode::new("gate", format!("http://{}", server.addr))
            .with_retries(5)
            .with_backoff(Duration::from_millis(1));
        let err = Node::run(&node, test_ctx()).await.unwrap_err();
        assert!(err.is_interrupt());
        assert_eq!(
            err.interrupt_value(),
            Some(&json!({"question": "approve deployment?"}))
        );
        // Interrupts are control flow, not failures: never retried.
        assert_eq!(server.attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn remote_node_resume_value_crosses_the_wire() {
        let server = start_mock(|_n, body| {
            let task: NodeTask = serde_json::from_str(&body).unwrap();
            // Echo the resume value back like a resumable worker handler would.
            let response = match task.config.resume {
                Some(v) => NodeTaskResponse::ok(NodeOutput::update("approved", v)),
                None => NodeTaskResponse::interrupt(json!({"question": "approve?"})),
            };
            MockBehavior::Respond {
                status: 200,
                body: serde_json::to_string(&response).unwrap(),
            }
        });

        let node = RemoteNode::new("gate", format!("http://{}", server.addr)).with_retries(0);

        // First invocation: no resume value → interrupt.
        let err = Node::run(&node, test_ctx()).await.unwrap_err();
        assert!(err.is_interrupt());

        // Second invocation: resume value in NodeConfig → success echoing it.
        let resumed = NodeContext::new(
            test_ctx().state().clone(),
            NodeConfig {
                resume: Some(json!(true)),
                ..NodeConfig::default()
            },
        );
        let out = Node::run(&node, resumed).await.unwrap();
        assert_eq!(out.updates.get("approved"), Some(&json!(true)));
        assert_eq!(server.attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn remote_node_retries_on_429_then_succeeds() {
        let server = start_mock(|n, _body| {
            if n < 2 {
                MockBehavior::Respond {
                    status: 429,
                    body: r#"{"error":"rate limited"}"#.to_string(),
                }
            } else {
                MockBehavior::Respond {
                    status: 200,
                    body: serde_json::to_string(&NodeTaskResponse::ok(NodeOutput::update(
                        "x",
                        json!("through"),
                    )))
                    .unwrap(),
                }
            }
        });

        let node = RemoteNode::new("limited", format!("http://{}", server.addr))
            .with_retries(2)
            .with_backoff(Duration::from_millis(1));
        let out = Node::run(&node, test_ctx()).await.unwrap();
        assert_eq!(out.updates.get("x"), Some(&json!("through")));
        assert_eq!(server.attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn remote_node_does_not_retry_fatal_4xx() {
        let server = start_mock(|_n, _body| MockBehavior::Respond {
            status: 400,
            body: r#"{"error":"malformed task"}"#.to_string(),
        });

        let node = RemoteNode::new("bad", format!("http://{}", server.addr))
            .with_retries(5)
            .with_backoff(Duration::from_millis(1));
        let err = Node::run(&node, test_ctx()).await.unwrap_err();
        assert!(matches!(err, RustyError::Node(_)));
        assert!(err.to_string().contains("400"));
        // Definitive failure: no retries despite with_retries(5).
        assert_eq!(server.attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn execute_url_is_used_verbatim_when_already_ending_in_execute() {
        let node = RemoteNode::new("n", "http://127.0.0.1:1/execute");
        assert_eq!(node.execute_url(), "http://127.0.0.1:1/execute");
        let node = RemoteNode::new("n", "http://127.0.0.1:1/");
        assert_eq!(node.execute_url(), "http://127.0.0.1:1/execute");
    }
}
