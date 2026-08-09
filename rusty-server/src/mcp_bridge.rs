//! `POST /mcp` — the Model Context Protocol bridge (R0.9 wave 4).
//!
//! Every registered graph is exposed as one MCP tool: `tools/list` derives
//! the tool set from the [`GraphRegistry`] (never a static list, so a newly
//! registered graph appears without a redeploy), and `tools/call` runs the
//! graph on a fresh thread and answers with its terminal state. The tool's
//! `inputSchema` is derived from the graph's [`StateSpec`]: each declared
//! channel becomes one property whose JSON Schema shape follows the
//! channel's reducer (append-channels are arrays, deep-merge channels are
//! objects, overwrite channels accept anything).
//!
//! Two answer shapes, chosen by the request's `Accept` header:
//!
//! - plain JSON (default): the handler blocks on the run's terminal watch
//!   and answers with the standard `tools/call` result;
//! - `text/event-stream`: an SSE stream of `notifications/progress`
//!   messages (one per run frame, only when the call carried a
//!   `_meta.progressToken`) followed by the final JSON-RPC response as the
//!   last event. A client that disconnects mid-stream cancels the run —
//!   the answer it paid for can never arrive, so leaving the run executing
//!   would be pure waste (the same reasoning as the run-level cancel
//!   endpoint, R0.7 wave 2).
//!
//! Errors travel in the JSON-RPC envelope with HTTP 200, not in the status
//! code: MCP clients dispatch on the envelope's `error.code`, and a
//! non-200 status would be indistinguishable from a transport failure.
//! Notifications (no `id`) answer `202` with an empty body, as JSON-RPC
//! forbids replying to them.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State as AxumState;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use chrono::Utc;
use futures::{stream, Stream};
use rusty_agent_runtime::state::Reducer;
use serde_json::{json, Value};
use tokio::sync::{broadcast, watch};

use crate::auth::TenantContext;
use crate::routes::AppState;
use crate::runs::{self, MultitaskStrategy, RunManager, RunPayload, SseFrame};
use crate::threads::ThreadRecord;

/// The MCP revision this bridge speaks. Pinned, not negotiated: Streamable
/// HTTP (the transport this route implements — one POST endpoint answering
/// JSON or SSE) landed in the 2025-03-26 revision, and the pin is what the
/// conformance evidence records, matching the runtime's own protocol pins
/// (see `A2A_PROTOCOL_VERSION` in core).
pub(crate) const MCP_BRIDGE_PROTOCOL_VERSION: &str = "2025-03-26";

/// Server-side ceiling for the blocking `tools/call` wait — a graph that
/// never terminates must not pin the handler task forever. The run keeps
/// executing; only the wait is bounded. Mirrors `MAX_RUN_WAIT` on the
/// native wait endpoint.
const MAX_CALL_WAIT: Duration = Duration::from_secs(3600);

// JSON-RPC 2.0 error codes (the subset this bridge can raise).
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;

/// In-flight `tools/call` runs, keyed by the JSON string of the request
/// `id` — the lookup `notifications/cancelled` performs with its
/// `params.requestId`. A std mutex, not a tokio one: the disconnect guard
/// removes its entry from `Drop`, which is synchronous, and every critical
/// section here is a trivial map op (the `lock_recover` convention from
/// [`crate::runs`] applies).
#[derive(Debug, Clone, Default)]
pub(crate) struct McpBridgeState {
    pending: Arc<StdMutex<HashMap<String, String>>>,
}

impl McpBridgeState {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn register(&self, request_key: String, run_id: String) {
        runs::lock_recover(&self.pending).insert(request_key, run_id);
    }

    fn lookup(&self, request_key: &str) -> Option<String> {
        runs::lock_recover(&self.pending).get(request_key).cloned()
    }

    fn remove(&self, request_key: &str) {
        runs::lock_recover(&self.pending).remove(request_key);
    }
}

/// `POST /mcp` — JSON-RPC dispatch. Raw body bytes (not `Json<Value>`):
/// a parse failure must answer with a JSON-RPC `-32700` envelope, which
/// axum's `Json` extractor's own rejection would preempt.
pub(crate) async fn handle(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let request: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(e) => return rpc_error(Value::Null, PARSE_ERROR, format!("parse error: {e}")),
    };
    // Batch requests are refused: one envelope per HTTP POST keeps the
    // SSE answer shape (one stream per call) unambiguous.
    if !request.is_object() {
        return rpc_error(
            Value::Null,
            INVALID_REQUEST,
            "invalid request: batch arrays are not supported".to_string(),
        );
    }
    let id = request.get("id").cloned();
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let params = request.get("params").cloned().unwrap_or(Value::Null);

    match method {
        "initialize" => rpc_result(
            id.unwrap_or(Value::Null),
            json!({
                "protocolVersion": MCP_BRIDGE_PROTOCOL_VERSION,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": {
                    "name": "rusty-server",
                    "version": env!("CARGO_PKG_VERSION"),
                },
            }),
        ),
        "ping" => rpc_result(id.unwrap_or(Value::Null), json!({})),
        "tools/list" => rpc_result(id.unwrap_or(Value::Null), tool_list(&state)),
        "tools/call" => match id {
            Some(id) => call_tool(&state, &tenant, &headers, id, params).await,
            // A request method without an id is a notification; JSON-RPC
            // forbids answering it, and a fire-and-forget tool call has no
            // defined result channel — accepted and ignored.
            None => StatusCode::ACCEPTED.into_response(),
        },
        "notifications/initialized" => StatusCode::ACCEPTED.into_response(),
        "notifications/cancelled" => {
            if let Some(request_id) = params.get("requestId") {
                let key = request_id.to_string();
                if let Some(run_id) = state.mcp_bridge.lookup(&key) {
                    // A terminal or unknown run is a no-op inside
                    // `cancel_run` — cancellation is control flow,
                    // idempotent by no-op.
                    state.run_deps.manager.cancel_run(&run_id).await;
                }
            }
            StatusCode::ACCEPTED.into_response()
        }
        // Unknown notifications are ignored (JSON-RPC: never answered);
        // unknown requests get METHOD_NOT_FOUND.
        _ => match id {
            Some(id) => rpc_error(id, METHOD_NOT_FOUND, format!("method `{method}` not found")),
            None => StatusCode::ACCEPTED.into_response(),
        },
    }
}

fn rpc_result(id: Value, result: Value) -> Response {
    Json(json!({ "jsonrpc": "2.0", "id": id, "result": result })).into_response()
}

fn rpc_error(id: Value, code: i64, message: String) -> Response {
    Json(json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    }))
    .into_response()
}

/// `tools/list`: one tool per registered graph, derived from the registry
/// on every call — a static list would drift the moment an embedder
/// registers another graph.
fn tool_list(state: &AppState) -> Value {
    let tools = state
        .registry
        .names()
        .into_iter()
        .map(|name| {
            json!({
                "name": name,
                "description": tool_description(state, &name),
                "inputSchema": tool_schema(state, &name),
            })
        })
        .collect::<Vec<_>>();
    json!({ "tools": tools })
}

fn tool_description(state: &AppState, name: &str) -> String {
    let channels = state.registry.channel_names(name);
    format!("Rusty graph `{name}` (channels: {})", channels.join(", "))
}

/// The tool input schema is the graph's state schema: one property per
/// declared channel, shaped by its reducer. Append-channels take arrays
/// (`add_messages` channels: arrays of message objects), deep-merge
/// channels take objects, and overwrite channels take any value — the
/// reducer is the only honest signal about what shape a channel accepts.
fn tool_schema(state: &AppState, name: &str) -> Value {
    let Some((_graph, spec)) = state.registry.get(name) else {
        return json!({ "type": "object", "properties": {} });
    };
    let mut channels: Vec<&str> = spec.channel_names().collect();
    channels.sort_unstable();
    let mut properties = serde_json::Map::new();
    for channel in channels {
        let schema = match spec.try_reducer_for(channel).unwrap_or_default() {
            Reducer::Append => json!({ "type": "array" }),
            Reducer::AddMessages => json!({ "type": "array", "items": { "type": "object" } }),
            Reducer::DeepMerge => json!({ "type": "object" }),
            Reducer::Overwrite => json!({}),
        };
        properties.insert(channel.to_string(), schema);
    }
    json!({ "type": "object", "properties": Value::Object(properties) })
}

/// `tools/call`: run the named graph on a fresh thread and answer with its
/// terminal state. Each call gets its own thread (a server-generated
/// uuid): MCP tools are stateless by convention, and a fresh thread keeps
/// one call's checkpoints from ever leaking into the next.
async fn call_tool(
    state: &Arc<AppState>,
    tenant: &TenantContext,
    headers: &HeaderMap,
    id: Value,
    params: Value,
) -> Response {
    let name = match params.get("name").and_then(Value::as_str) {
        Some(name) => name.to_string(),
        None => {
            return rpc_error(
                id,
                INVALID_PARAMS,
                "invalid params: `name` is required".to_string(),
            );
        }
    };
    if !state.registry.contains(&name) {
        return rpc_error(id, INVALID_PARAMS, format!("unknown tool `{name}`"));
    }
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if !arguments.is_object() {
        return rpc_error(
            id,
            INVALID_PARAMS,
            "invalid params: `arguments` must be an object".to_string(),
        );
    }
    let progress_token = params
        .get("_meta")
        .and_then(|meta| meta.get("progressToken"))
        .cloned();

    let thread_id = uuid::Uuid::new_v4().to_string();
    let record = ThreadRecord {
        thread_id: thread_id.clone(),
        tenant: tenant.tenant().to_string(),
        graph: name.clone(),
        metadata: Value::Null,
        created_at: Utc::now(),
    };
    if let Err(e) = state
        .server_store
        .create_thread(&tenant.scope(&thread_id), &record)
        .await
    {
        return rpc_error(id, INTERNAL_ERROR, format!("failed to create thread: {e}"));
    }

    let payload = RunPayload {
        input: Some(arguments),
        ..Default::default()
    };
    // Reject, not enqueue: the thread was created for this call alone, so
    // a busy slot can only mean a bug — fail loudly rather than queue
    // behind a stranger.
    let scheduled = match runs::schedule(
        &state.run_deps,
        &tenant.scope(&thread_id),
        &thread_id,
        &name,
        payload,
        MultitaskStrategy::Reject,
    )
    .await
    {
        Ok(scheduled) => scheduled,
        Err(e) => return rpc_error(id, INTERNAL_ERROR, format!("failed to start run: {e}")),
    };

    // Register the request id → run id mapping so a later
    // `notifications/cancelled` can find the run to signal.
    let request_key = id.to_string();
    state
        .mcp_bridge
        .register(request_key.clone(), scheduled.run_id.clone());

    if wants_sse(headers) {
        let stream = call_stream(
            state.mcp_bridge.clone(),
            state.run_deps.manager.clone(),
            scheduled,
            id,
            progress_token,
            request_key,
        );
        Sse::new(stream)
            .keep_alive(
                KeepAlive::new()
                    .interval(Duration::from_secs(15))
                    .text("keep-alive"),
            )
            .into_response()
    } else {
        let mut terminal = scheduled.terminal;
        let outcome = tokio::time::timeout(MAX_CALL_WAIT, terminal.wait_for(|v| v.is_some())).await;
        state.mcp_bridge.remove(&request_key);
        match outcome {
            Ok(Ok(value)) => {
                let terminal = value.clone().unwrap_or(Value::Null);
                rpc_result(id, tool_result(&terminal))
            }
            Ok(Err(_)) => rpc_error(
                id,
                INTERNAL_ERROR,
                "run ended without a terminal result".to_string(),
            ),
            Err(_) => rpc_error(
                id,
                INTERNAL_ERROR,
                format!(
                    "run did not reach a terminal state within {}s",
                    MAX_CALL_WAIT.as_secs()
                ),
            ),
        }
    }
}

fn wants_sse(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/event-stream"))
}

/// The `tools/call` result object for a terminal run payload. Success maps
/// the run's output to both `structuredContent` and a text rendering
/// (clients that ignore structured content still see the answer); any
/// other terminal status is `isError: true` with the error inline — the
/// honest shape for "the tool ran and failed", distinct from a JSON-RPC
/// error ("the call itself was malformed").
fn tool_result(terminal: &Value) -> Value {
    let status = terminal
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("error");
    if status == "success" {
        let output = terminal.get("output").cloned().unwrap_or(Value::Null);
        json!({
            "content": [{ "type": "text", "text": output.to_string() }],
            "structuredContent": output,
        })
    } else {
        let detail = terminal
            .get("error")
            .cloned()
            .unwrap_or_else(|| Value::String(format!("run ended `{status}`")));
        json!({
            "content": [{ "type": "text", "text": detail.to_string() }],
            "isError": true,
        })
    }
}

/// Cancels the run when the SSE response body is dropped before the
/// terminal answer was sent — the client is gone, so the run's output has
/// no consumer left. Also removes the `notifications/cancelled` mapping:
/// a finished call's request id must not name a stale run.
struct DisconnectGuard {
    bridge: McpBridgeState,
    manager: RunManager,
    run_id: String,
    request_key: String,
    armed: bool,
}

impl Drop for DisconnectGuard {
    fn drop(&mut self) {
        self.bridge.remove(&self.request_key);
        if self.armed {
            let manager = self.manager.clone();
            let run_id = std::mem::take(&mut self.run_id);
            tokio::spawn(async move {
                manager.cancel_run(&run_id).await;
            });
        }
    }
}

/// The streaming phases of one `tools/call`: run frames first (mapped to
/// progress notifications), then the terminal answer as the final event.
enum CallPhase {
    Frames,
    Terminal,
    Done,
}

struct CallStream {
    replay: std::vec::IntoIter<SseFrame>,
    live: broadcast::Receiver<SseFrame>,
    last_seq: u64,
    terminal: watch::Receiver<Option<Value>>,
    id: Value,
    progress_token: Option<Value>,
    phase: CallPhase,
    guard: DisconnectGuard,
}

impl CallStream {
    /// The next run frame: replayed log frames first, then live broadcast
    /// frames (skipping anything the replay already covered). `None` when
    /// the broadcast closed — the run handle is gone, so the terminal
    /// watch is the only remaining source of truth.
    async fn next_frame(&mut self) -> Option<SseFrame> {
        if let Some(frame) = self.replay.next() {
            self.last_seq = frame.seq;
            return Some(frame);
        }
        loop {
            match self.live.recv().await {
                Ok(frame) => {
                    if frame.seq <= self.last_seq {
                        continue;
                    }
                    self.last_seq = frame.seq;
                    return Some(frame);
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!(skipped, "MCP bridge stream lagged; progress frames dropped");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }

    /// A run frame as a `notifications/progress` event — only when the
    /// caller supplied a progress token; without one the intermediate
    /// frames have no defined addressee and are skipped.
    fn progress_event(&self, frame: &SseFrame) -> Option<Event> {
        let token = self.progress_token.as_ref()?;
        let notification = json!({
            "jsonrpc": "2.0",
            "method": "notifications/progress",
            "params": {
                "progressToken": token,
                "progress": frame.seq,
                "message": frame.event,
            },
        });
        Some(Event::default().data(notification.to_string()))
    }
}

/// Build the SSE item stream for one `tools/call`: progress notifications
/// for each run frame, then the final JSON-RPC response as the last event.
fn call_stream(
    bridge: McpBridgeState,
    manager: RunManager,
    scheduled: runs::Scheduled,
    id: Value,
    progress_token: Option<Value>,
    request_key: String,
) -> impl Stream<Item = Result<Event, Infallible>> + Send {
    let state = CallStream {
        replay: scheduled.replay.into_iter(),
        live: scheduled.broadcast,
        last_seq: 0,
        terminal: scheduled.terminal,
        id,
        progress_token,
        phase: CallPhase::Frames,
        guard: DisconnectGuard {
            bridge,
            manager,
            run_id: scheduled.run_id,
            request_key,
            armed: true,
        },
    };
    stream::unfold(state, |mut st| async move {
        loop {
            match st.phase {
                CallPhase::Frames => match st.next_frame().await {
                    Some(frame) => {
                        let is_end = frame.event == "end";
                        if is_end {
                            st.phase = CallPhase::Terminal;
                        }
                        match st.progress_event(&frame) {
                            Some(event) => return Some((Ok(event), st)),
                            None => continue,
                        }
                    }
                    None => st.phase = CallPhase::Terminal,
                },
                CallPhase::Terminal => {
                    let value = st
                        .terminal
                        .wait_for(|v| v.is_some())
                        .await
                        .ok()
                        .and_then(|v| v.clone());
                    st.phase = CallPhase::Done;
                    // The answer is being sent on this stream — disarm the
                    // disconnect guard before the final event goes out.
                    st.guard.armed = false;
                    let result = match value {
                        Some(terminal) => tool_result(&terminal),
                        None => json!({
                            "content": [{
                                "type": "text",
                                "text": "run ended without a terminal result",
                            }],
                            "isError": true,
                        }),
                    };
                    let response = json!({ "jsonrpc": "2.0", "id": st.id, "result": result });
                    return Some((Ok(Event::default().data(response.to_string())), st));
                }
                CallPhase::Done => return None,
            }
        }
    })
}
