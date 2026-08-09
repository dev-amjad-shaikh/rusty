//! A2A (Agent2Agent) client support: [`A2aNode`], the durable half of the
//! A2A bridge (R0.9 Rusty Capsules, wave 4).
//!
//! An [`A2aNode`] is registered in a graph exactly like any local node —
//! the [`RemoteNode`](crate::remote::RemoteNode) precedent: *one `Node`
//! trait, remote agents behind the same trait*. When the executor runs it,
//! the node's state snapshot is sent to the remote agent's `POST {base}/a2a`
//! JSON-RPC endpoint as a `message/send`, the resulting A2A **task** is
//! polled (`tasks/get`) to a terminal state, and the task's artifacts become
//! the node's output. `tasks/cancel` propagates run cancellation to the
//! remote task.
//!
//! ## Evidence and the trust posture
//!
//! Every delegation is journaled as one [`RunEventKind::RemoteCall`]
//! event — the canonical replay-servable kind — carrying the exact request
//! params as input and the terminal task as output. The boundary is the
//! honest one: **what we sent and what came back is our evidence; the
//! remote agent's conduct is its own receipt's problem.** A2A gives the
//! caller no window into the callee's internals, and this module pretends
//! otherwise nowhere. Replaying ([`A2aNode::replaying`]) serves the
//! journaled outcome from the recorded journal and holds no HTTP client —
//! a replayed run issues no outbound calls by construction.
//!
//! ## Idempotency
//!
//! The A2A `messageId` is the delegation's idempotency handle: derived as
//! `a2a-{thread_id}-{step}-{node_name}`, it is stable across executor-level
//! re-execution of the same super-step, so a server that dedupes on
//! `messageId` (rusty-server's bridge does, mapping it onto the durable
//! task queue's idempotency key) turns a resubmission into the same task
//! rather than a second one. The assigned task id then names the remote
//! work for polling and cancellation.
//!
//! ## Spec pin
//!
//! [`A2A_PROTOCOL_VERSION`] pins the A2A revision this client speaks
//! (recorded in requests, not negotiated — the same discipline as
//! [`MCP_PROTOCOL_VERSION`](crate::mcp::MCP_PROTOCOL_VERSION)): `0.3.0`
//! renamed the well-known Agent Card path and settled the task/artifact
//! shapes this module relies on. A second revision lands as additive
//! evolution, never a rewrite of the pinned shapes.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::{Result, RustyError};
use crate::journal::{Journal, PARENT_EVENT_KEY};
use crate::llm::backoff_delay;
use crate::node::{Node, NodeContext, NodeOutput};
use crate::record::{Effect, EventStatus, RunEventKind};
use crate::replay::ReplaySource;

/// The A2A spec revision this client speaks (see the module docs). Sent as
/// the request's `protocolVersion`; the server's revision is recorded in
/// the journaled task, not validated.
pub const A2A_PROTOCOL_VERSION: &str = "0.3.0";

/// Default per-request HTTP timeout.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Default number of retries *after* the initial attempt (transport-class
/// failures only — a JSON-RPC error is the server's definitive answer).
pub const DEFAULT_MAX_RETRIES: u32 = 2;

/// Default base delay for exponential backoff between retries.
pub const DEFAULT_BASE_BACKOFF: Duration = Duration::from_millis(100);

/// Default interval between `tasks/get` polls while the remote task runs.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// A node's delegation to a remote A2A agent, behind the same [`Node`]
/// trait as a local node.
///
/// **Error semantics across the wire.** A task the remote agent settles
/// `failed` flattens to [`RustyError::Node`] — a hard, non-retryable
/// failure, the [`RemoteNode`](crate::remote::RemoteNode) rule: the callee
/// already made a definitive decision, and retry classification does not
/// survive a process boundary. This client's own retries cover
/// transport-class failures only (connect errors, timeouts, HTTP 5xx / 408
/// / 429 — see the module docs). A task found `canceled` on poll (someone
/// else cancelled it) is likewise definitive.
///
/// ```no_run
/// use std::time::Duration;
/// use rusty_agent_runtime::a2a::A2aNode;
/// use rusty_agent_runtime::graph::GraphBuilder;
///
/// let node = A2aNode::new("researcher", "http://127.0.0.1:8300")
///     .with_timeout(Duration::from_secs(10))
///     .with_poll_interval(Duration::from_millis(250));
/// let mut builder = GraphBuilder::new();
/// builder.add_node("research", node);
/// ```
#[derive(Clone)]
pub struct A2aNode {
    /// The graph node name, also sent as the message's skill hint.
    name: String,
    /// Full URL of the agent's JSON-RPC endpoint (`{base}/a2a`).
    rpc_url: String,
    /// HTTP client (carries the per-request timeout); `None` in replay
    /// mode — a replayed run cannot issue outbound calls by construction.
    client: Option<reqwest::Client>,
    timeout: Duration,
    max_retries: u32,
    base_backoff: Duration,
    poll_interval: Duration,
    /// The state channel the terminal outcome is written to.
    channel: String,
    /// Optional `X-Api-Key` credential (rusty-server's tenant auth).
    api_key: Option<String>,
    /// The run's journal; attached per run the way
    /// [`crate::replay::RecordingTool`] attaches.
    journal: Option<Journal>,
    /// Run cancellation: firing it cancels the remote task, then errors.
    cancellation: Option<tokio_util::sync::CancellationToken>,
    /// The recorded journal's serving cursor; `Some` in replay mode.
    replay: Option<ReplaySource>,
}

/// Internal classification of a failed HTTP attempt (the
/// [`crate::remote::RemoteNode`] taxonomy).
#[derive(Debug)]
enum AttemptError {
    /// Transport-class failure eligible for retry (connect, timeout, 5xx,
    /// 408, 429), with an optional server-provided `Retry-After` floor.
    Retryable {
        error: RustyError,
        retry_after: Option<Duration>,
    },
    /// Definitive failure; never retried (other 4xx, JSON-RPC errors,
    /// decode errors).
    Fatal(RustyError),
}

impl A2aNode {
    /// A node delegating to the remote agent served at `base_url` (e.g.
    /// `"http://127.0.0.1:8300"`). A trailing `/` is trimmed and `/a2a`
    /// appended; a `base_url` already ending in `/a2a` is used verbatim.
    /// The terminal outcome is written to the `a2a_outcome` channel unless
    /// [`A2aNode::with_channel`] says otherwise.
    pub fn new(name: impl Into<String>, base_url: impl Into<String>) -> Self {
        let base = base_url.into();
        let base = base.trim_end_matches('/');
        let rpc_url = if base.ends_with("/a2a") {
            base.to_owned()
        } else {
            format!("{base}/a2a")
        };
        Self {
            name: name.into(),
            rpc_url,
            client: Some(Self::build_client(DEFAULT_TIMEOUT)),
            timeout: DEFAULT_TIMEOUT,
            max_retries: DEFAULT_MAX_RETRIES,
            base_backoff: DEFAULT_BASE_BACKOFF,
            poll_interval: DEFAULT_POLL_INTERVAL,
            channel: "a2a_outcome".to_string(),
            api_key: None,
            journal: None,
            cancellation: None,
            replay: None,
        }
    }

    /// Override the per-request timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        if self.replay.is_none() {
            self.client = Some(Self::build_client(timeout));
        }
        self
    }

    /// Override the number of retries after the initial attempt (`0` =
    /// single attempt).
    pub fn with_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = max_retries;
        self
    }

    /// Override the base backoff delay between retries.
    pub fn with_backoff(mut self, base_backoff: Duration) -> Self {
        self.base_backoff = base_backoff;
        self
    }

    /// Override the `tasks/get` poll interval.
    pub fn with_poll_interval(mut self, poll_interval: Duration) -> Self {
        self.poll_interval = poll_interval;
        self
    }

    /// Override the state channel the terminal outcome is written to.
    pub fn with_channel(mut self, channel: impl Into<String>) -> Self {
        self.channel = channel.into();
        self
    }

    /// Present `key` as the `X-Api-Key` header on every request
    /// (rusty-server's tenant authentication; other agents ignore or map
    /// it per their own auth model).
    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    /// Attach the run's journal: the delegation is recorded as one
    /// `RemoteCall` event, parented on the invoking node-input event
    /// delivered via [`PARENT_EVENT_KEY`]. Without a journal the node
    /// still works — it just leaves no evidence, the same posture as an
    /// unwrapped [`crate::remote::RemoteNode`].
    pub fn with_journal(mut self, journal: Journal) -> Self {
        self.journal = Some(journal);
        self
    }

    /// Attach the run's cancellation token: when it fires mid-delegation
    /// the node sends `tasks/cancel` for the in-flight remote task (best
    /// effort — the cancel itself is not retried), journals the canceled
    /// outcome, and returns [`RustyError::Cancelled`].
    pub fn with_cancellation(mut self, token: tokio_util::sync::CancellationToken) -> Self {
        self.cancellation = Some(token);
        self
    }

    /// The replaying half (R0.9 wave 4): serve the delegation's outcome
    /// from the recorded run's [`ReplaySource`] instead of the network.
    /// No HTTP client is kept — a replayed run cannot reach the remote
    /// agent by construction, the
    /// [`JournaledMcpTool::replaying`](crate::mcp::JournaledMcpTool::replaying)
    /// discipline. The replay run's journal is still required: the served
    /// event is re-journaled into it.
    pub fn replaying(mut self, source: ReplaySource, journal: Journal) -> Self {
        self.client = None;
        self.replay = Some(source);
        self.journal = Some(journal);
        self
    }

    /// The derived `messageId` for one invocation: the delegation's
    /// idempotency handle (see the module docs).
    pub fn message_id(&self, thread_id: &str, step: usize) -> String {
        format!("a2a-{thread_id}-{step}-{}", self.name)
    }

    /// The full JSON-RPC endpoint URL.
    pub fn rpc_url(&self) -> &str {
        &self.rpc_url
    }

    fn build_client(timeout: Duration) -> reqwest::Client {
        // Same invariant as RemoteNode: the builder only sets a timeout on
        // the rustls backend, so construction cannot realistically fail.
        reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .expect("reqwest client builder with rustls must succeed")
    }

    fn node_err(&self, msg: impl Into<String>) -> RustyError {
        RustyError::Node(format!("a2a node `{}`: {}", self.name, msg.into()))
    }

    /// One JSON-RPC call. `Ok` carries the response's `result` value;
    /// `Err` is classified for retry.
    async fn try_rpc(
        &self,
        method: &str,
        params: &Value,
    ) -> std::result::Result<Value, AttemptError> {
        let Some(client) = &self.client else {
            return Err(AttemptError::Fatal(self.node_err(
                "replay-mode node attempted a live RPC (construction bug)",
            )));
        };
        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        });
        let mut builder = client.post(&self.rpc_url).json(&request);
        if let Some(key) = &self.api_key {
            builder = builder.header("x-api-key", key);
        }
        let response = builder.send().await.map_err(|e| {
            let err = self.node_err(format!("POST {} failed: {e}", self.rpc_url));
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
            let err = self.node_err(format!(
                "agent at {} returned {status}: {}",
                self.rpc_url,
                crate::llm::truncate_body(&body)
            ));
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

        let body: Value = response.json().await.map_err(|e| {
            AttemptError::Fatal(self.node_err(format!("could not decode JSON-RPC response: {e}")))
        })?;
        if let Some(error) = body.get("error") {
            // A JSON-RPC error is the server's definitive answer (unknown
            // method, invalid params, task not found): never retried.
            return Err(AttemptError::Fatal(self.node_err(format!(
                "JSON-RPC error from {}: {error}",
                self.rpc_url
            ))));
        }
        body.get("result").cloned().ok_or_else(|| {
            AttemptError::Fatal(
                self.node_err("JSON-RPC response carries neither `result` nor `error`"),
            )
        })
    }

    /// `try_rpc` with the retry policy applied (transport-class only).
    async fn rpc(&self, method: &str, params: &Value) -> Result<Value> {
        let mut attempt: u32 = 0;
        loop {
            match self.try_rpc(method, params).await {
                Ok(result) => return Ok(result),
                Err(AttemptError::Fatal(e)) => return Err(e),
                Err(AttemptError::Retryable { error, retry_after })
                    if attempt < self.max_retries =>
                {
                    let mut delay = backoff_delay(self.base_backoff, attempt);
                    if let Some(floor) = retry_after {
                        delay = delay.max(floor);
                    }
                    tracing::warn!(
                        node = %self.name,
                        url = %self.rpc_url,
                        method,
                        attempt = attempt + 1,
                        max_retries = self.max_retries,
                        backoff_ms = delay.as_millis() as u64,
                        error = %error,
                        "a2a attempt failed; retrying"
                    );
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
                Err(AttemptError::Retryable { error, .. }) => return Err(error),
            }
        }
    }

    /// Journal the delegation as one `RemoteCall` event (no-op when the
    /// node carries no journal). The input is the exact `message/send`
    /// params; the output is the terminal task (or the canceled task's
    /// last known state); failures record `EventStatus::Error`.
    fn journal_outcome(&self, parent: Option<String>, params: &Value, task: &Value, error: bool) {
        let Some(journal) = &self.journal else {
            return;
        };
        let mut draft = crate::journal::EventDraft::new(RunEventKind::RemoteCall, self.effect())
            .node(self.name.clone())
            .input(params.clone())
            .output(task.clone());
        if let Some(parent) = parent {
            draft = draft.parent(parent);
        }
        if error {
            draft = draft.status(EventStatus::Error);
        }
        journal.record(draft);
    }

    /// The channel update for a completed task: the task id plus its
    /// artifacts, verbatim — the caller's graph decides what to read.
    fn outcome_update(&self, task: &Value) -> NodeOutput {
        NodeOutput::update(
            self.channel.clone(),
            json!({
                "task_id": task.get("id").cloned().unwrap_or(Value::Null),
                "context_id": task.get("contextId").cloned().unwrap_or(Value::Null),
                "artifacts": task.get("artifacts").cloned().unwrap_or(Value::Null),
            }),
        )
    }
}

#[async_trait]
impl Node for A2aNode {
    fn name(&self) -> &str {
        &self.name
    }

    fn effect(&self) -> Effect {
        // A delegation crosses a process boundary into work the runtime
        // cannot inspect; the restrictive class applies, exactly as
        // RemoteNode declares.
        Effect::NonIdempotent
    }

    async fn run(&self, ctx: NodeContext) -> Result<NodeOutput> {
        let parent = ctx
            .config()
            .extra
            .get(PARENT_EVENT_KEY)
            .and_then(Value::as_str)
            .map(str::to_string);
        let message = json!({
            "role": "user",
            "messageId": self.message_id(ctx.thread_id(), ctx.step()),
            // The run's thread groups its delegations into one A2A context:
            // server-side, that context maps onto one durable run journal.
            "contextId": ctx.thread_id(),
            "parts": [{ "kind": "data", "data": ctx.state().to_value() }],
        });
        let params = json!({ "message": message });

        // Replay: serve the journaled outcome; no HTTP, by construction.
        if let Some(source) = &self.replay {
            let served = source.serve(RunEventKind::RemoteCall, &params)?;
            if let Some(journal) = &self.journal {
                served.rejournal(journal, parent.unwrap_or_default());
            }
            if served.event.status == EventStatus::Error {
                return Err(self.node_err(format!(
                    "recorded delegation failed (served from journal): {}",
                    served.output.unwrap_or(Value::Null)
                )));
            }
            let task = served.output.unwrap_or(Value::Null);
            return Ok(self.outcome_update(&task));
        }

        // Live: submit, then poll to terminal.
        let task = self.rpc("message/send", &params).await?;
        let task_id = task
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| self.node_err("`message/send` result carries no task `id`"))?
            .to_string();

        let get_params = json!({ "id": task_id });
        loop {
            let sleep = tokio::time::sleep(self.poll_interval);
            if let Some(token) = &self.cancellation {
                tokio::select! {
                    () = sleep => {}
                    () = token.cancelled() => {
                        // Best effort: the cancel is not retried — the
                        // server's own lease expiry is the backstop for a
                        // cancel that never lands.
                        let cancel_params = json!({ "id": task_id });
                        let _ = self.try_rpc("tasks/cancel", &cancel_params).await;
                        let canceled = json!({
                            "id": task_id,
                            "status": { "state": "canceled" },
                        });
                        self.journal_outcome(parent, &params, &canceled, true);
                        return Err(RustyError::Cancelled(format!(
                            "a2a node `{}`: run cancelled; remote task `{task_id}` was sent tasks/cancel",
                            self.name
                        )));
                    }
                }
            } else {
                sleep.await;
            }

            let task = self.rpc("tasks/get", &get_params).await?;
            let state = task
                .pointer("/status/state")
                .and_then(Value::as_str)
                .unwrap_or("");
            match state {
                "submitted" | "working" => continue,
                "completed" => {
                    self.journal_outcome(parent, &params, &task, false);
                    return Ok(self.outcome_update(&task));
                }
                "failed" | "rejected" => {
                    self.journal_outcome(parent, &params, &task, true);
                    let message = task
                        .pointer("/status/message/parts")
                        .and_then(Value::as_array)
                        .and_then(|parts| parts.first())
                        .and_then(|part| part.get("text"))
                        .and_then(Value::as_str)
                        .unwrap_or("the remote agent reported failure");
                    return Err(
                        self.node_err(format!("remote task `{task_id}` {state}: {message}"))
                    );
                }
                "canceled" => {
                    self.journal_outcome(parent, &params, &task, true);
                    return Err(self.node_err(format!(
                        "remote task `{task_id}` was canceled outside this run"
                    )));
                }
                other => {
                    return Err(self.node_err(format!(
                        "remote task `{task_id}` reports unknown state `{other}`"
                    )));
                }
            }
        }
    }
}
