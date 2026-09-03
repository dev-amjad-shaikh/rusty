//! The A2A (Agent2Agent) bridge (R0.9 wave 4): the agent card at
//! `GET /.well-known/agent-card.json` and the JSON-RPC task surface at
//! `POST /a2a`.
//!
//! The mapping is deliberately thin — A2A concepts land on machinery the
//! server already has, never on a parallel path:
//!
//! - an A2A **task** is a durable task-queue record (`kind = "a2a"`), so
//!   persistence, tenant isolation, quotas, and idempotent redelivery are
//!   the queue's own guarantees. The idempotency key is the message's
//!   `messageId`, namespaced (`a2a:{messageId}`) so an A2A redelivery can
//!   never collide with a native `POST /tasks` key.
//! - an A2A **context** is one Flight Recorder journal (`a2a-{contextId}`),
//!   so every capsule execution the bridge drives leaves its evidence on
//!   the context's causal chain, readable through the native
//!   `GET /runs/{id}/events` and `/fixture` endpoints.
//! - a message carrying a **capsule data part** (`{"capsule": {name,
//!   version}, "input": …}`) is executed in-process by the bridge's own
//!   executor over the `a2a-capsule` pool; plain messages queue on the
//!   `a2a` pool for external workers — the bridge executes code it can
//!   sandbox (a verified component against a wired connector), and
//!   nothing else.
//!
//! `message/stream` answers SSE: the task's current state first, then a
//! `TaskStatusUpdateEvent`-shaped event per settlement, fed by the task
//! lifecycle hooks ([`publish_task_update`]). A reconnecting client
//! re-reads the durable task through `tasks/get` — the stream is a live
//! attachment, not a log.

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::State as AxumState;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use chrono::Utc;
use futures::{stream, Stream, StreamExt};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::Mutex;

use crate::auth::TenantContext;
use crate::routes::{a2a_enqueue, internal_err, AppState};
use crate::tasks::{self, TaskRecord, TaskStatus};
use crate::threads::ThreadRecord;

use rusty_agent_runtime::journal::{Clock, Journal};

// The bridge executor acts on the static floor (see `drain_capsule_pool`)
// — needed with and without the `capsules` feature.
use rusty_agent_runtime::durable::ResolvedRetryParameters;
use rusty_agent_runtime::record::ExecutorPolicy;

#[cfg(feature = "capsules")]
use rusty_agent_runtime::capsule::{any_grant_of_kind, CapabilityKind, CapsuleDenial};
#[cfg(feature = "capsules")]
use rusty_agent_runtime::capsule_host::{CapsuleHost, CapsuleInvocation};
#[cfg(feature = "capsules")]
use rusty_agent_runtime::durable::ErrorClass;
#[cfg(feature = "capsules")]
use rusty_agent_runtime::journal::EventDraft;
#[cfg(feature = "capsules")]
use rusty_agent_runtime::record::{sha256_hex, Effect, EventStatus, RunEventKind};

/// The A2A specification revision this bridge speaks. Pinned, not
/// negotiated — the same posture as the runtime's other protocol pins:
/// the conformance evidence records what the bridge implements, and
/// 0.3.0 is the revision that renamed the well-known agent-card path.
pub(crate) const A2A_SPEC_VERSION: &str = "0.3.0";

/// The well-known path the agent card is served at (A2A 0.3.0 renamed it
/// from `agent.json`; the pin above records the revision this matches).
pub(crate) const AGENT_CARD_PATH: &str = "/.well-known/agent-card.json";

/// The graph name the A2A context thread records bind to. The native
/// evidence endpoints (`GET /runs/{id}/events`, `/fixture`) resolve a
/// run's journal through its thread record's graph binding, so the
/// context thread must name a registered graph — and `a2a` is the name
/// this bridge uses. Deployments exposing Flight Recorder evidence over
/// A2A contexts register a (trivial) graph under it; the release proof
/// does exactly that.
pub(crate) const A2A_THREAD_GRAPH: &str = "a2a";

/// The pool plain A2A messages queue on: work for external workers, which
/// claim it through the native task API like any other pool.
const A2A_POOL: &str = "a2a";

/// The pool capsule messages queue on: claimed only by the bridge's own
/// in-process executor (the pool *is* the addressing — a plain message
/// can never be claimed into a sandbox it did not ask for, and an
/// operator can cap the two pools independently).
const A2A_CAPSULE_POOL: &str = "a2a-capsule";

/// The worker id the in-process executor claims and settles under.
const A2A_EXECUTOR_WORKER: &str = "a2a-bridge";

/// The executor's claim lease: long enough for a bounded guest run, short
/// enough that a crashed bridge makes the task claimable again promptly.
const A2A_EXECUTOR_LEASE_MS: u64 = 30_000;

// JSON-RPC 2.0 error codes, plus the A2A-specific task errors.
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;
const TASK_NOT_FOUND: i64 = -32001;
const TASK_NOT_CANCELABLE: i64 = -32002;

// --------------------------------------------------------------------- //
// Agent card
// --------------------------------------------------------------------- //

/// The agent card, derived from the registry on every read: one skill per
/// registered graph. Derived, never static — a static card would drift
/// the moment an embedder registers another graph, and the card is the
/// discovery surface clients route by. Deterministic (no timestamps, no
/// random ids): the card is golden-testable evidence of the bridge's
/// shape.
pub(crate) fn agent_card(state: &AppState) -> Value {
    let skills = state
        .registry
        .names()
        .into_iter()
        .map(|name| {
            let channels = state.registry.channel_names(&name);
            json!({
                "id": name,
                "name": name,
                "description": format!(
                    "Rusty graph `{name}` (channels: {})",
                    channels.join(", ")
                ),
                "tags": channels,
                "inputModes": ["application/json"],
                "outputModes": ["application/json"],
            })
        })
        .collect::<Vec<_>>();
    json!({
        "name": "rusty-server",
        "description": "Rusty graph runtime — every registered graph is an A2A skill",
        "protocolVersion": A2A_SPEC_VERSION,
        "version": env!("CARGO_PKG_VERSION"),
        "url": "/a2a",
        "capabilities": { "streaming": true, "pushNotifications": false },
        "defaultInputModes": ["application/json"],
        "defaultOutputModes": ["application/json"],
        "skills": skills,
        "provider": { "organization": "rusty" },
    })
}

/// `GET /.well-known/agent-card.json`.
pub(crate) async fn agent_card_route(AxumState(state): AxumState<Arc<AppState>>) -> Json<Value> {
    Json(agent_card(&state))
}

// --------------------------------------------------------------------- //
// JSON-RPC dispatch
// --------------------------------------------------------------------- //

/// `POST /a2a`. Raw body bytes (not `Json<Value>`), the MCP bridge's
/// reasoning: a parse failure must answer with a JSON-RPC `-32700`
/// envelope, which axum's `Json` extractor's own rejection would preempt.
/// Errors travel in the envelope, not the HTTP status — A2A clients
/// dispatch on `error.code`.
pub(crate) async fn handle(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    body: axum::body::Bytes,
) -> Response {
    let request: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(e) => return rpc_error(Value::Null, PARSE_ERROR, format!("parse error: {e}")),
    };
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

    match (method, id) {
        ("message/send", Some(id)) => send_message(&state, &tenant, id, params, false).await,
        ("message/stream", Some(id)) => send_message(&state, &tenant, id, params, true).await,
        ("tasks/get", Some(id)) => get_task_rpc(&state, &tenant, id, params).await,
        ("tasks/cancel", Some(id)) => cancel_task_rpc(&state, &tenant, id, params).await,
        // Notifications (no id) are accepted and ignored: JSON-RPC forbids
        // answering them.
        (_, None) => axum::http::StatusCode::ACCEPTED.into_response(),
        (_, Some(id)) => rpc_error(id, METHOD_NOT_FOUND, format!("method `{method}` not found")),
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

// --------------------------------------------------------------------- //
// message/send + message/stream
// --------------------------------------------------------------------- //

/// The capsule invocation a message asks for, parsed out of its first
/// qualifying data part: `{"capsule": {"name", "version"}, "input": …}`.
/// `None` for plain messages.
fn capsule_request(message: &Value) -> Option<(String, String, Value)> {
    message.get("parts")?.as_array()?.iter().find_map(|part| {
        if part.get("kind").and_then(Value::as_str) != Some("data") {
            return None;
        }
        let data = part.get("data")?;
        let capsule = data.get("capsule")?;
        let name = capsule.get("name").and_then(Value::as_str)?.to_string();
        let version = capsule.get("version").and_then(Value::as_str)?.to_string();
        let input = data.get("input").cloned().unwrap_or(Value::Null);
        Some((name, version, input))
    })
}

/// `message/send` / `message/stream`: enqueue the durable task, ensure the
/// context's journal exists, and (capsule payloads only) spawn the
/// in-process executor. The answer is the A2A task in its `submitted`
/// state — execution is asynchronous by design.
async fn send_message(
    state: &Arc<AppState>,
    tenant: &TenantContext,
    id: Value,
    params: Value,
    stream: bool,
) -> Response {
    let Some(message) = params.get("message").cloned() else {
        return rpc_error(
            id,
            INVALID_PARAMS,
            "invalid params: `message` is required".to_string(),
        );
    };
    let Some(message_id) = message.get("messageId").and_then(Value::as_str) else {
        return rpc_error(
            id,
            INVALID_PARAMS,
            "invalid params: `message.messageId` is required — it is the task's idempotency key"
                .to_string(),
        );
    };
    let context_id = message
        .get("contextId")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    // The run id embeds the tenant: journal keys are bare run ids
    // (globally-unique uuids everywhere else), while A2A context ids are
    // client-chosen — two tenants picking the same context id must not
    // share a journal.
    let run_id = format!("a2a-{}-{context_id}", tenant.tenant());
    let capsule = capsule_request(&message);

    let record = TaskRecord::new(
        tasks::NewTask {
            task_id: uuid::Uuid::new_v4().to_string(),
            tenant: tenant.tenant().to_string(),
            kind: "a2a".to_string(),
            payload: json!({ "message": message, "context_id": context_id }),
            pool: if capsule.is_some() {
                A2A_CAPSULE_POOL.to_string()
            } else {
                A2A_POOL.to_string()
            },
            recipient: None,
            max_attempts: tasks::DEFAULT_MAX_ATTEMPTS,
            // Namespaced: an A2A redelivery dedups against its own
            // messageId, never against a native task's key.
            idempotency_key: Some(format!("a2a:{message_id}")),
            effect: None,
            run_id: Some(run_id.clone()),
            thread_id: Some(run_id.clone()),
            deadline: None,
            parent: None,
            parent_task_id: None,
            stage: 0,
            status_category: crate::tasks::StatusCategory::Todo,
            worker_version: None,
        },
        Utc::now(),
    );

    let (task, deduplicated) = match a2a_enqueue(state, tenant, &record).await {
        Ok(outcome) => outcome,
        Err(e) => return rpc_error(id, INTERNAL_ERROR, e.to_string()),
    };

    // The context's journal + thread record, get-or-create on every send
    // (a redelivery after a crash between enqueue and journal creation
    // must still find its context). Best-effort failure here is an
    // internal error: the evidence substrate half of the submission is as
    // much the contract as the queue row.
    if let Err(e) = ensure_a2a_context(state, tenant, &run_id).await {
        return rpc_error(id, INTERNAL_ERROR, e);
    }

    // For streams the sender registers before the executor can settle the
    // task — a terminal publish landing before registration would leave
    // the client hanging on a stream whose task already finished. A
    // deduplicated redelivery names the pre-existing task, so the sender
    // moves to the stored id (its state arrives as the initial event).
    let stream_pair = if stream {
        let (sender, receiver) = tokio::sync::broadcast::channel(64);
        state
            .a2a_streams
            .lock()
            .await
            .insert(task.task_id.clone(), sender);
        Some(receiver)
    } else {
        None
    };

    if capsule.is_some() && !deduplicated {
        spawn_capsule_drainer(Arc::clone(state), tenant.tenant().to_string());
    }

    if let Some(receiver) = stream_pair {
        let body = task_stream(state.clone(), task, receiver);
        Sse::new(body)
            .keep_alive(
                KeepAlive::new()
                    .interval(Duration::from_secs(15))
                    .text("keep-alive"),
            )
            .into_response()
    } else {
        rpc_result(id, a2a_task_json(&task))
    }
}

/// Get-or-create the context's Flight Recorder journal and the thread
/// record the native evidence endpoints resolve through, under the
/// per-context journal lock (a concurrent send of the same context must
/// not double-create).
async fn ensure_a2a_context(
    state: &Arc<AppState>,
    tenant: &TenantContext,
    run_id: &str,
) -> Result<(), String> {
    let lock = {
        let mut locks = state.journal_locks.lock().await;
        locks
            .entry(run_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    };
    let _guard = lock.lock().await;
    let journal_present = state
        .server_store
        .get_journal(run_id)
        .await
        .map_err(|e| format!("load context journal: {e}"))?
        .is_some();
    if journal_present {
        return Ok(());
    }
    let record = ThreadRecord {
        thread_id: run_id.to_string(),
        tenant: tenant.tenant().to_string(),
        graph: A2A_THREAD_GRAPH.to_string(),
        metadata: Value::Null,
        created_at: Utc::now(),
    };
    state
        .server_store
        .create_thread(&tenant.scope(run_id), &record)
        .await
        .map_err(|e| format!("create context thread: {e}"))?;
    let journal = Journal::new(run_id, run_id, Clock::System);
    state
        .server_store
        .put_journal(&journal.snapshot())
        .await
        .map_err(|e| format!("persist context journal: {e}"))?;
    Ok(())
}

// --------------------------------------------------------------------- //
// tasks/get + tasks/cancel
// --------------------------------------------------------------------- //

async fn get_task_rpc(
    state: &Arc<AppState>,
    tenant: &TenantContext,
    id: Value,
    params: Value,
) -> Response {
    let Some(task_id) = params.get("id").and_then(Value::as_str) else {
        return rpc_error(
            id,
            INVALID_PARAMS,
            "invalid params: `id` is required".to_string(),
        );
    };
    match state.server_store.get_task(tenant.tenant(), task_id).await {
        Ok(Some(task)) if task.kind == "a2a" => rpc_result(id, a2a_task_json(&task)),
        // Cross-kind ids answer not-found, the tenant-isolation posture:
        // the A2A surface does not confirm a native task exists.
        Ok(_) => rpc_error(id, TASK_NOT_FOUND, format!("task `{task_id}` not found")),
        Err(e) => rpc_error(id, INTERNAL_ERROR, internal_err(e).to_string()),
    }
}

async fn cancel_task_rpc(
    state: &Arc<AppState>,
    tenant: &TenantContext,
    id: Value,
    params: Value,
) -> Response {
    let Some(task_id) = params.get("id").and_then(Value::as_str) else {
        return rpc_error(
            id,
            INVALID_PARAMS,
            "invalid params: `id` is required".to_string(),
        );
    };
    match state
        .server_store
        .cancel_task(tenant.tenant(), task_id, Utc::now())
        .await
    {
        Ok(tasks::CancelOutcome::Applied(task)) => {
            publish_task_update(state, &task).await;
            rpc_result(id, a2a_task_json(&task))
        }
        Ok(tasks::CancelOutcome::Terminal(status)) => rpc_error(
            id,
            TASK_NOT_CANCELABLE,
            format!(
                "task `{task_id}` is already terminal ({}) and cannot be cancelled",
                status.as_str()
            ),
        ),
        Ok(tasks::CancelOutcome::Unknown) => {
            rpc_error(id, TASK_NOT_FOUND, format!("task `{task_id}` not found"))
        }
        Err(e) => rpc_error(id, INTERNAL_ERROR, internal_err(e).to_string()),
    }
}

// --------------------------------------------------------------------- //
// Task mapping + stream fan-out
// --------------------------------------------------------------------- //

/// One durable task record as an A2A Task: `queued` → `submitted`,
/// `leased` → `working`, `completed` → `completed`, `failed`/`dead` →
/// `failed` (with the error as the status message), `cancelled` →
/// `canceled`. A completed task's artifacts come from its result — the
/// executor puts them there, so the body a client reads is the durable
/// record, never a side channel.
pub(crate) fn a2a_task_json(record: &TaskRecord) -> Value {
    let context_id = record
        .payload
        .get("context_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let state_name = match record.status {
        TaskStatus::Queued => "submitted",
        TaskStatus::Leased => "working",
        TaskStatus::Completed => "completed",
        TaskStatus::Failed | TaskStatus::Dead => "failed",
        TaskStatus::Cancelled => "canceled",
    };
    let mut status = json!({
        "state": state_name,
        "timestamp": record.updated_at.to_rfc3339(),
    });
    if matches!(record.status, TaskStatus::Failed | TaskStatus::Dead) {
        status["message"] = json!({
            "role": "agent",
            "parts": [{
                "kind": "text",
                "text": record.last_error.clone().unwrap_or_default(),
            }],
        });
    }
    let mut task = json!({
        "id": record.task_id,
        "contextId": context_id,
        "status": status,
    });
    if record.status == TaskStatus::Completed {
        if let Some(artifacts) = record.result.as_ref().and_then(|r| r.get("artifacts")) {
            task["artifacts"] = artifacts.clone();
        }
    }
    task
}

/// Fan a task transition out to any live `message/stream` attachment.
/// Called by the native settle/cancel route handlers (external workers
/// settling A2A tasks), by `tasks/cancel` here, and by the in-process
/// executor — one publish path, so a stream sees the same transitions
/// regardless of who settled. Terminal transitions also remove the
/// sender: a stream's attachment ends with the task.
pub(crate) async fn publish_task_update(state: &AppState, task: &TaskRecord) {
    if task.kind != "a2a" {
        return;
    }
    let sender = state.a2a_streams.lock().await.get(&task.task_id).cloned();
    if let Some(sender) = sender {
        // A send error means no receivers — the client is gone; the
        // sender's removal is the stream drop guard's business.
        let _ = sender.send(a2a_task_json(task));
    }
    if task.is_terminal() {
        state.a2a_streams.lock().await.remove(&task.task_id);
    }
}

/// Removes the stream's sender when the SSE body drops — a disconnected
/// client's attachment must not pin the map entry for the task's whole
/// life (a queued-for-days task would otherwise leak it).
struct StreamGuard {
    state: Arc<AppState>,
    task_id: String,
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        let state = Arc::clone(&self.state);
        let task_id = std::mem::take(&mut self.task_id);
        tokio::spawn(async move {
            state.a2a_streams.lock().await.remove(&task_id);
        });
    }
}

/// The SSE item stream for `message/stream`: the task's current state
/// first (a client always learns where the task stands), then one event
/// per published transition, ending on the terminal one.
fn task_stream(
    state: Arc<AppState>,
    task: TaskRecord,
    receiver: tokio::sync::broadcast::Receiver<Value>,
) -> impl Stream<Item = Result<Event, Infallible>> + Send {
    struct Live {
        receiver: tokio::sync::broadcast::Receiver<Value>,
        finished: bool,
        // Held for its `Drop` (sender-map removal), never read.
        _guard: StreamGuard,
    }
    let initial = stream::iter([Ok(Event::default().data(a2a_task_json(&task).to_string()))]);
    let live = stream::unfold(
        Live {
            receiver,
            finished: task.is_terminal(),
            _guard: StreamGuard {
                state,
                task_id: task.task_id.clone(),
            },
        },
        |mut st| async move {
            if st.finished {
                return None;
            }
            loop {
                match st.receiver.recv().await {
                    Ok(update) => {
                        let terminal = update
                            .get("status")
                            .and_then(|s| s.get("state"))
                            .and_then(Value::as_str)
                            .is_some_and(|s| matches!(s, "completed" | "failed" | "canceled"));
                        if terminal {
                            st.finished = true;
                        }
                        let event = Event::default().data(update.to_string());
                        return Some((Ok(event), st));
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(
                            skipped,
                            "A2A stream lagged; status events dropped (re-read via tasks/get)"
                        );
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                }
            }
        },
    );
    initial.chain(live)
}

// --------------------------------------------------------------------- //
// The in-process capsule executor
// --------------------------------------------------------------------- //

/// Spawn a drainer for the `a2a-capsule` pool: claim and execute capsule
/// tasks until none are claimable, then exit. One drainer per fresh
/// capsule submission; concurrent drainers race claims atomically (the
/// queue's own claim protocol), so no task executes twice and a busy
/// moment simply finishes the pool sooner.
fn spawn_capsule_drainer(state: Arc<AppState>, tenant: String) {
    tokio::spawn(async move {
        if let Err(e) = drain_capsule_pool(&state, &tenant).await {
            tracing::warn!(%e, "A2A capsule drainer exited on error");
        }
    });
}

async fn drain_capsule_pool(state: &Arc<AppState>, tenant: &str) -> Result<(), String> {
    let pools = [A2A_CAPSULE_POOL.to_string()];
    // The bridge executor is a fixed, server-internal worker: it claims and
    // settles on the static floor (no tenant policy lookup), the pre-R0.10
    // behavior for this out-of-band path.
    let floor = ExecutorPolicy::static_v0();
    loop {
        let claimed = state
            .server_store
            .claim_task(
                tenant,
                A2A_EXECUTOR_WORKER,
                &tasks::ClaimScope {
                    pools: &pools,
                    pool_limits: &state.config.task_pool_limits,
                    worker_version: None,
                    timeout_policy: &floor,
                },
                A2A_EXECUTOR_LEASE_MS,
                Utc::now(),
            )
            .await
            .map_err(|e| format!("claim capsule task: {e}"))?;
        let Some(task) = claimed else {
            return Ok(());
        };
        publish_task_update(state, &task).await;
        execute_capsule_task(state, tenant, &task).await;
    }
}

/// Execute one claimed capsule task and settle it. Every failure is
/// permanent (`retryable: false`): resolution, admission, and guest
/// failures all fail the same way on a re-drive, and an executor crash
/// mid-guest — the only retry-shaped failure — is the lease protocol's
/// business, not this path's.
#[cfg(feature = "capsules")]
async fn execute_capsule_task(state: &Arc<AppState>, tenant: &str, task: &TaskRecord) {
    let fail = |message: String| async {
        let outcome = state
            .server_store
            .fail_task(
                tenant,
                &task.task_id,
                A2A_EXECUTOR_WORKER,
                tasks::FailureReport {
                    error_class: ErrorClass::InvalidInput,
                    message,
                    retryable: false,
                    cost: tasks::SettlementCost::default(),
                    // Permanent failure (`retryable: false` → Fail): the
                    // resolved parameters are inert, and the bridge executor
                    // acts on the static floor regardless.
                    retry: ResolvedRetryParameters::floor(task.max_attempts),
                },
                Utc::now(),
            )
            .await;
        if let Ok(tasks::MutationOutcome::Applied(task)) = outcome {
            publish_task_update(state, &task).await;
        }
    };

    let Some((name, version, input)) = task.payload.get("message").and_then(capsule_request) else {
        fail("capsule task payload carries no capsule data part".to_string()).await;
        return;
    };
    let Some(record) = state
        .server_store
        .get_capsule_by_version(tenant, &name, &version)
        .await
        .ok()
        .flatten()
    else {
        fail(format!(
            "capsule `{name}` version `{version}` is not registered"
        ))
        .await;
        return;
    };
    let Some(bytes) = state
        .server_store
        .get_capsule_blob(tenant, record.capsule_id.as_str())
        .await
        .ok()
        .flatten()
    else {
        fail(format!(
            "capsule `{name}` version `{version}` has no component blob uploaded"
        ))
        .await;
        return;
    };
    let Some(connector) = state.config.capsule_connector.clone() else {
        fail(
            "capsule_execution_unavailable: the deployment wired no capsule connector — \
             the bridge never executes guest code against an egress path the operator \
             did not explicitly provide"
                .to_string(),
        )
        .await;
        return;
    };

    // The journal half of the execution, serialized per context: load →
    // append → persist under the per-context lock, so concurrent tasks of
    // one A2A context never clobber each other's freshly journaled events
    // (snapshot persistence is whole-journal replace).
    let run_id = task.run_id.clone().unwrap_or_default();
    let lock = {
        let mut locks = state.journal_locks.lock().await;
        locks
            .entry(run_id.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    };
    let _guard = lock.lock().await;
    let Some(snapshot) = state.server_store.get_journal(&run_id).await.ok().flatten() else {
        drop(_guard);
        fail(format!("context journal `{run_id}` is missing")).await;
        return;
    };
    let journal = match Journal::from_snapshot(snapshot, Clock::System) {
        Ok(journal) => journal,
        Err(e) => {
            drop(_guard);
            fail(format!("context journal failed its integrity check: {e}")).await;
            return;
        }
    };
    let parent = journal.events().last().map(|event| event.id.clone());

    // Admission, the bridge half: the v1 world has no filesystem import,
    // so a guest cannot even name the capability — but the *caller* can
    // declare `requires: ["filesystem"]`, and the refusal must leave the
    // same evidence the host's structural gate would: an unscoped
    // (empty-scope) denial, journaled before any execution.
    let requires_fs = input
        .get("requires")
        .and_then(Value::as_array)
        .is_some_and(|r| r.iter().any(|v| v.as_str() == Some("filesystem")));
    if requires_fs && !any_grant_of_kind(&record.manifest.capabilities, CapabilityKind::Filesystem)
    {
        let denial = CapsuleDenial::unscoped(
            record.capsule_id.clone(),
            CapabilityKind::Filesystem,
            "the caller requires `filesystem`, which the v1 world does not import and the \
             manifest does not grant — refused at admission, before any guest code ran",
        );
        if let Ok(output) = serde_json::to_value(&denial) {
            let mut draft = EventDraft::new(RunEventKind::CapsuleDenied, Effect::Pure)
                .output(output)
                .status(EventStatus::Ok);
            if let Some(parent) = parent.clone() {
                draft = draft.parent(parent);
            }
            journal.record(draft);
        }
        let _ = state.server_store.put_journal(&journal.snapshot()).await;
        drop(_guard);
        fail(format!(
            "capsule `{name}` was refused at admission: capability `filesystem` is not granted \
             (denial journaled on the context)"
        ))
        .await;
        return;
    }

    let host = match CapsuleHost::from_bytes(record.manifest.clone(), &bytes) {
        Ok(host) => host
            .with_connector(connector)
            .with_grant_recheck(state.capsule_plane.rechecker(tenant)),
        Err(e) => {
            drop(_guard);
            fail(format!("capsule admission failed: {e}")).await;
            return;
        }
    };
    let outcome = host
        .invoke(CapsuleInvocation::new(input).with_journal(journal.clone(), parent))
        .await;
    // The invocation's events are on the journal either way — capability
    // uses, scope denials, and the refusal of a failed call are all
    // evidence.
    if let Err(e) = state.server_store.put_journal(&journal.snapshot()).await {
        tracing::warn!(%e, "capsule execution journal could not be persisted");
    }
    drop(_guard);

    match outcome {
        Ok(outcome) => {
            // Content-addressed by construction: the artifact id is
            // derived from the canonical output bytes, the body lives on
            // the durable task record, and the digest is journaled — no
            // second artifact store, and both store backends stay equal.
            let canonical = serde_json::to_vec(&outcome.output).unwrap_or_default();
            let artifact_id = sha256_hex(&canonical);
            let result = json!({
                "output": outcome.output,
                "fuel_consumed": outcome.fuel_consumed,
                "artifacts": [{
                    "artifactId": artifact_id,
                    "name": "output",
                    "parts": [{ "kind": "data", "data": outcome.output }],
                }],
            });
            let outcome = state
                .server_store
                .complete_task(
                    tenant,
                    &task.task_id,
                    A2A_EXECUTOR_WORKER,
                    tasks::CompletionReport {
                        result,
                        receipt: None,
                        cost: tasks::SettlementCost::default(),
                    },
                    Utc::now(),
                )
                .await;
            if let Ok(tasks::MutationOutcome::Applied(task)) = outcome {
                publish_task_update(state, &task).await;
            }
        }
        Err(e) => {
            fail(format!("capsule execution failed: {e}")).await;
        }
    }
}

/// Without the `capsules` feature the bridge still accepts capsule
/// messages (the queue surface is feature-independent), but execution
/// fails closed with a typed message — the same posture as the policy
/// plane's `503 capsule_policy_unavailable`.
#[cfg(not(feature = "capsules"))]
async fn execute_capsule_task(state: &Arc<AppState>, tenant: &str, task: &TaskRecord) {
    let outcome = state
        .server_store
        .fail_task(
            tenant,
            &task.task_id,
            A2A_EXECUTOR_WORKER,
            tasks::FailureReport {
                error_class: rusty_agent_runtime::durable::ErrorClass::InvalidInput,
                message: "capsule_execution_unavailable: this server was built without the \
                          `capsules` feature"
                    .to_string(),
                retryable: false,
                cost: tasks::SettlementCost::default(),
                retry: ResolvedRetryParameters::floor(task.max_attempts),
            },
            Utc::now(),
        )
        .await;
    if let Ok(tasks::MutationOutcome::Applied(task)) = outcome {
        publish_task_update(state, &task).await;
    }
}
