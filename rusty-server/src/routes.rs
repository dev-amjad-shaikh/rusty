//! HTTP handlers and application state (Agent-Protocol subset, design doc §3).

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, Query, State as AxumState};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{middleware, Extension, Json, Router};
use chrono::{DateTime, Utc};
use futures::Stream;
use rusty_agent_runtime::agents::{
    AgentId, CapabilityManifest, CoordinationContract, DelegateContract, FanOutContract,
    QuorumContract, RaceContract, StateScope, COORDINATION_RESULT_KIND,
};
use rusty_agent_runtime::checkpoint::{
    Checkpoint, Checkpointer, InMemoryCheckpointer, JsonFileCheckpointer,
};
use rusty_agent_runtime::effects::ApprovalToken;
use rusty_agent_runtime::journal::{Clock, EventDraft, Journal, JournalSnapshot, RngSource};
use rusty_agent_runtime::learn::{
    admit_promotion, candidate_effect_key, evaluation_effect_key, promotion_effect_key,
    rollback_effect_key, Candidate, CandidateRecord, CandidateStatus, EvaluationRequest,
    LearnError, PromotionReceipt, PromotionRefusal, RollbackReceipt, VersionPointer,
};
use rusty_agent_runtime::llm::Usage;
use rusty_agent_runtime::memory::{
    assemble, detect_conflicts, memory_effect_key, memory_forget_effect_key, memory_read_request,
    plan_forget, Candidacy, ContextBudget, Correction, CorrectionTarget, ForgetReason,
    MemoryEvidence, MemoryForgetTombstone, MemoryKind, MemoryProvenance, MemoryQuery, MemoryRecord,
    MemoryScope, ProvenanceAuthor, ScopeAddress, ValidityWindow,
};
use rusty_agent_runtime::record::{sha256_hex, Effect, EffectReceipt, PayloadRef, RunEventKind};
use rusty_agent_runtime::replay::{BranchDiff, ExactReplay, ReplayFixture, ReplayParams};
use rusty_agent_runtime::state::State;
use rusty_agent_runtime::team_trace::TeamTrace;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::agents::{
    self, ActivationMutation, ActivationOutcome, AgentRecord, MailboxClaim, MailboxClaimScope,
};
use crate::assistants::AssistantRecord;
use crate::auth::TenantContext;
use crate::coordination;
use crate::crons::{self, CronRecord, OnRunCompleted};
use crate::error::ApiError;
use crate::runs::{
    self, MultitaskStrategy, RunConfigPayload, RunDeps, RunManager, RunPayload, RunStatus,
};
use crate::server_store::{CandidateTransition, JsonFileStore, ServerStore};
use crate::sse;
use crate::supervision;
use crate::tasks::{self, CancelOutcome, MutationOutcome, TaskRecord, TaskStatus};
use crate::threads::ThreadRecord;
use crate::triggers;
use crate::{store, GraphRegistry, ServerConfig, RESERVED_NAMES};

/// Shared application state.
pub(crate) struct AppState {
    pub registry: GraphRegistry,
    pub config: ServerConfig,
    pub checkpointer: Arc<dyn Checkpointer>,
    pub run_deps: RunDeps,
    /// Assistants / crons / threads / KV persistence (JSON files or
    /// Postgres). Thread records live here — not in a route-local map — so
    /// they survive restarts alongside their checkpoints.
    pub server_store: Arc<dyn ServerStore>,
    /// Per-thread locks serializing `update_state`'s read-modify-write:
    /// without one, two concurrent writes could mint the same `step`.
    pub state_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// The cooperative drain control (R0.6 wave 2c): cancelling it stops
    /// the cron scheduler and the outbox relay, rejects new runs with 503,
    /// and parks every in-flight run at its next checkpoint boundary.
    /// [`crate::router`] wires a token that never fires;
    /// [`crate::serve_with_shutdown`] wires the real one.
    pub shutdown: tokio_util::sync::CancellationToken,
    /// Per-trigger debounce buffers (in-memory): events received inside a
    /// trigger's `debounce_ms` window accumulate here and coalesce into one
    /// action carrying the array of payloads. Keyed by internal trigger id.
    pub trigger_debounce: Mutex<HashMap<String, crate::triggers::DebounceBuffer>>,
}

/// Build the checkpointer + server-store backends for `config`. The default
/// is JSON files under `store_path`; `ServerConfig::with_postgres(url)`
/// (feature `postgres`) switches both to Postgres. Postgres connections are
/// established lazily on first use, keeping this builder synchronous.
fn build_backends(config: &ServerConfig) -> (Arc<dyn Checkpointer>, Arc<dyn ServerStore>) {
    #[cfg(feature = "postgres")]
    if let Some(url) = &config.database_url {
        return (
            Arc::new(crate::server_store::LazyPostgresCheckpointer::new(
                url.clone(),
            )),
            Arc::new(crate::server_store::PostgresStore::new(url.clone())),
        );
    }
    #[cfg(not(feature = "postgres"))]
    assert!(
        config.database_url.is_none(),
        "`ServerConfig::database_url` requires the `postgres` feature \
         (rebuild rusty-server with `--features postgres`)"
    );
    (
        Arc::new(JsonFileCheckpointer::new(config.store_path.clone())),
        Arc::new(JsonFileStore::load(&config.store_path)),
    )
}

/// Build the full router with an explicit drain control (used by
/// [`crate::router`] with a never-fired token, and by
/// [`crate::router_with_shutdown`] with the real one).
pub(crate) fn router_with_shutdown(
    registry: GraphRegistry,
    config: ServerConfig,
    shutdown: tokio_util::sync::CancellationToken,
) -> Router {
    let (checkpointer, server_store) = build_backends(&config);
    let run_deps = RunDeps {
        registry: registry.clone(),
        checkpointer: Arc::clone(&checkpointer),
        manager: RunManager::new(),
        server_store: Arc::clone(&server_store),
        queue_cap: config.max_concurrent_runs_per_thread.max(1),
        log_capacity: config.event_log_capacity.max(16),
        shutdown: shutdown.clone(),
    };
    let outbox_relay_interval = config.outbox_relay_interval;
    let state = Arc::new(AppState {
        registry,
        config,
        checkpointer,
        run_deps,
        server_store,
        state_locks: Mutex::new(HashMap::new()),
        shutdown,
        trigger_debounce: Mutex::new(HashMap::new()),
    });
    crons::spawn_scheduler(Arc::clone(&state));
    // The outbox relay: publishes pending outbox rows into the task queue
    // (R0.6 wave 2b). Also the crash-recovery path — rows pending at
    // startup publish on its first tick.
    crate::outbox::spawn_relay(
        Arc::clone(&state.server_store),
        outbox_relay_interval,
        state.shutdown.clone(),
    );

    let authed = Router::new()
        .route("/ok", get(ok))
        .route("/info", get(info))
        .route("/threads", post(create_thread))
        .route("/threads/{thread_id}/fork", post(fork_thread))
        .route(
            "/threads/{thread_id}/state",
            get(get_state).post(update_state),
        )
        .route("/threads/{thread_id}/history", post(history))
        .route("/threads/{thread_id}/runs", post(create_run))
        .route("/threads/{thread_id}/runs/wait", post(create_run_wait))
        .route("/threads/{thread_id}/runs/stream", post(create_run_stream))
        .route(
            "/threads/{thread_id}/runs/{run_id}",
            delete(delete_run_checkpoints),
        )
        .route("/runs/{run_id}", get(get_run))
        .route("/runs/{run_id}/cancel", post(cancel_run))
        .route("/runs/{run_id}/stream", get(get_run_stream))
        .route("/runs/{run_id}/events", get(get_run_events))
        .route("/runs/{run_id}/fixture", get(get_run_fixture))
        .route("/runs/replay", post(replay_run))
        .route("/runs/diff", get(diff_runs))
        .route("/assistants", post(create_assistant).get(list_assistants))
        .route("/assistants/{assistant_id}", get(get_assistant))
        .route("/crons", post(create_cron).get(list_crons))
        .route("/crons/{cron_id}", delete(delete_cron))
        .route("/store/{namespace}", get(list_store_namespace))
        .route(
            "/store/{namespace}/{key}",
            put(put_store_item)
                .get(get_store_item)
                .delete(delete_store_item),
        )
        .route("/tasks", post(enqueue_task).get(list_tasks))
        .route("/tasks/outbox", post(enqueue_task_outbox))
        .route("/tasks/claim", post(claim_task))
        .route("/tasks/metrics", get(task_metrics))
        .route("/tasks/{task_id}", get(get_task))
        .route("/tasks/{task_id}/heartbeat", post(heartbeat_task))
        .route("/tasks/{task_id}/complete", post(complete_task))
        .route("/tasks/{task_id}/fail", post(fail_task))
        .route("/tasks/{task_id}/cancel", post(cancel_task))
        .route("/agents", post(create_agent).get(list_agents))
        .route("/agents/{agent_id}", get(get_agent))
        .route("/agents/{agent_id}/mailbox", post(send_agent_message))
        .route("/agents/{agent_id}/mailbox/next", post(claim_agent_message))
        .route("/agents/{agent_id}/status", get(get_agent_status))
        .route("/agents/{agent_id}/cancel", post(cancel_agent))
        .route("/agents/{agent_id}/restart", post(restart_agent))
        .route("/agents/{agent_id}/supervision", get(get_agent_supervision))
        .route("/teams/{team_id}/cancel", post(cancel_team))
        .route("/memory", post(write_memory))
        // The static segments win over `/memory/{memory_id}` — query,
        // corrections, consolidation, conflicts, and forgetting are
        // operations, not record addresses.
        .route("/memory/query", post(query_memory))
        .route("/memory/corrections", post(submit_correction))
        .route("/memory/consolidate", post(enqueue_consolidation))
        .route("/memory/conflicts", post(list_memory_conflicts))
        .route("/memory/forget", post(forget_memory))
        .route("/memory/forget_scope", post(forget_memory_scope))
        .route("/memory/{memory_id}", get(get_memory))
        // The learning-candidate lifecycle (R0.8 wave 3): creation plus
        // the three journaled transitions, and the version-pointer
        // listing. Every transition requires `run_id` — the journal is
        // the evidence, and a transition the journal cannot take does
        // not reach the store.
        .route(
            "/learn/candidates",
            post(create_candidate).get(list_candidates),
        )
        .route("/learn/candidates/{candidate_id}", get(get_candidate))
        .route(
            "/learn/candidates/{candidate_id}/evaluate",
            post(evaluate_candidate),
        )
        .route(
            "/learn/candidates/{candidate_id}/promote",
            post(promote_candidate),
        )
        .route(
            "/learn/candidates/{candidate_id}/rollback",
            post(rollback_candidate),
        )
        .route("/learn/versions", get(list_version_pointers))
        .route("/coordination/delegate", post(submit_delegate))
        .route("/coordination/fan_out", post(submit_fan_out))
        .route("/coordination/race", post(submit_race))
        .route("/coordination/quorum", post(submit_quorum))
        .route("/coordination/{coordination_id}", get(get_coordination))
        .route(
            "/coordination/{coordination_id}/trace",
            get(get_coordination_trace),
        )
        .route("/agents/{agent_id}/activate", post(activate_agent))
        .route(
            "/agents/{agent_id}/activate/heartbeat",
            post(heartbeat_activation),
        )
        .route(
            "/agents/{agent_id}/activate/release",
            post(release_activation),
        )
        .route(
            "/triggers",
            post(triggers::create_trigger).get(triggers::list_triggers),
        )
        .route(
            "/triggers/{trigger_id}",
            get(triggers::get_trigger)
                .patch(triggers::update_trigger)
                .delete(triggers::delete_trigger),
        )
        .route(
            "/triggers/{trigger_id}/events",
            get(triggers::list_trigger_events),
        )
        .route(
            "/triggers/{trigger_id}/dead-letter",
            get(triggers::list_dead_letter),
        )
        .route(
            "/triggers/{trigger_id}/events/{event_id}/replay",
            post(triggers::replay_event),
        )
        .layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            crate::auth::require_api_key,
        ));

    Router::new()
        // The trigger webhook authenticates by HMAC signature (per-trigger
        // secret), not by API key: external senders (GitHub, Stripe, …)
        // cannot present an `X-Api-Key`, so the signature is the credential
        // — and it resolves the owning tenant among same-external-id
        // triggers. Everything else stays behind the API-key layer.
        .route("/triggers/{trigger_id}/webhook", post(triggers::webhook))
        .merge(authed)
        // Outermost layer: permissive CORS so browser clients (e.g. the
        // Studio) can call the API from any origin, and OPTIONS preflights
        // are answered before the API-key middleware runs. Production
        // deployments should replace this with a restrictive `CorsLayer`.
        .layer(tower_http::cors::CorsLayer::permissive())
        .with_state(state)
}

// --------------------------------------------------------------------- //
// Helpers
// --------------------------------------------------------------------- //

pub(crate) fn internal_err<E: std::fmt::Display>(e: E) -> ApiError {
    ApiError::internal(e.to_string())
}

/// Fetch the caller's thread record by external id. Lookup happens under
/// the tenant's internal id namespace, so another tenant's thread simply
/// does not exist here — cross-tenant access answers 404 (never 403, to
/// avoid leaking the thread's existence).
async fn require_thread(
    state: &AppState,
    tenant: &TenantContext,
    thread_id: &str,
) -> Result<ThreadRecord, ApiError> {
    state
        .server_store
        .get_thread(&tenant.scope(thread_id))
        .await
        .map_err(internal_err)?
        .ok_or_else(|| ApiError::not_found(format!("thread `{thread_id}` not found")))
}

/// Validate a client-chosen resource id (thread / assistant / cron). Ids
/// become path segments under the store root and carry a `{tenant}/`
/// prefix internally, so they must be non-empty, bounded, and free of path
/// separators; all-dots ids are rejected (parent-directory components), as
/// are the reserved layout names in [`RESERVED_NAMES`] — an id of `crons`
/// would otherwise write checkpoint files into the cron-records directory.
pub(crate) fn validate_client_id(kind: &str, id: &str) -> Result<(), ApiError> {
    let ok = !id.is_empty()
        && id.len() <= 256
        && !id.contains('/')
        && !id.contains('\\')
        && !id.chars().all(|c| c == '.')
        && !RESERVED_NAMES.contains(&id);
    if ok {
        Ok(())
    } else {
        Err(ApiError::bad_request(format!(
            "invalid {kind} `{id}` (must be non-empty, <= 256 chars, no path separators, not a reserved name)"
        )))
    }
}

/// The per-thread lock serializing `update_state` (see
/// [`AppState::state_locks`]).
async fn state_lock(state: &AppState, internal_id: &str) -> Arc<Mutex<()>> {
    state
        .state_locks
        .lock()
        .await
        .entry(internal_id.to_string())
        .or_default()
        .clone()
}

fn checkpoint_ref(cp: &Checkpoint, tenant: &TenantContext) -> Value {
    json!({
        "checkpoint_id": cp.id,
        // Checkpoints persist the internal (tenant-scoped) thread id; the
        // wire always shows the external one.
        "thread_id": tenant.unscope(&cp.thread_id).unwrap_or(&cp.thread_id),
        "step": cp.step,
        "created_at": cp.created_at,
    })
}

// --------------------------------------------------------------------- //
// Liveness & info
// --------------------------------------------------------------------- //

async fn ok() -> Json<Value> {
    Json(json!({ "ok": true }))
}

async fn info(AxumState(state): AxumState<Arc<AppState>>) -> Json<Value> {
    let graphs: Vec<Value> = state
        .registry
        .names()
        .into_iter()
        .map(|name| {
            json!({
                "name": name,
                "channels": state.registry.channel_names(&name),
            })
        })
        .collect();
    let persistence = if state.config.database_url.is_some() {
        "postgres"
    } else {
        "json_file"
    };
    Json(json!({
        "service": "rusty-server",
        "version": env!("CARGO_PKG_VERSION"),
        "checkpointer": persistence,
        "server_store": persistence,
        "store_path": state.config.store_path,
        "graphs": graphs,
    }))
}

// --------------------------------------------------------------------- //
// Threads
// --------------------------------------------------------------------- //

#[derive(Debug, Deserialize)]
struct CreateThreadPayload {
    /// Registered graph name this thread binds to.
    graph: String,
    #[serde(default)]
    metadata: Option<Value>,
    /// Client-chosen thread id (a UUID v4 is generated when omitted).
    #[serde(default)]
    thread_id: Option<String>,
}

async fn create_thread(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(payload): Json<CreateThreadPayload>,
) -> Result<(StatusCode, Json<ThreadRecord>), ApiError> {
    if !state.registry.contains(&payload.graph) {
        return Err(ApiError::bad_request(format!(
            "unknown graph `{}` (see GET /info for registered graphs)",
            payload.graph
        )));
    }
    let thread_id = payload
        .thread_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    validate_client_id("thread_id", &thread_id)?;

    let internal_id = tenant.scope(&thread_id);
    let record = ThreadRecord {
        thread_id: thread_id.clone(),
        tenant: tenant.tenant().to_string(),
        graph: payload.graph,
        metadata: payload.metadata.unwrap_or(Value::Null),
        created_at: Utc::now(),
    };
    // Check-and-insert in the store (durable, so pre-restart checkpoints
    // stay reachable through the API).
    let created = state
        .server_store
        .create_thread(&internal_id, &record)
        .await
        .map_err(internal_err)?;
    if !created {
        return Err(ApiError::conflict(format!(
            "thread `{thread_id}` already exists"
        )));
    }
    Ok((StatusCode::CREATED, Json(record)))
}

// --------------------------------------------------------------------- //
// Thread fork (time travel)
// --------------------------------------------------------------------- //

#[derive(Debug, Deserialize)]
struct ForkThreadPayload {
    /// Client-chosen id for the fork (a UUID v4 is generated when omitted).
    #[serde(default)]
    new_thread_id: Option<String>,
    /// Fork from this checkpoint: only checkpoints up to and including it
    /// are copied. Omit to copy the full history.
    #[serde(default)]
    checkpoint_id: Option<String>,
}

/// `POST /threads/{id}/fork` — copy the thread's checkpoint history (full,
/// or up to `checkpoint_id`) into a new thread bound to the same graph, via
/// [`Checkpointer::fork_thread`]. The fork is the safe time-travel target:
/// replay it with `"checkpoint": {"checkpoint_id": …}` on run-create.
async fn fork_thread(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(thread_id): Path<String>,
    Json(payload): Json<ForkThreadPayload>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let record = require_thread(&state, &tenant, &thread_id).await?;
    let new_thread_id = payload
        .new_thread_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    validate_client_id("new_thread_id", &new_thread_id)?;

    let new_internal_id = tenant.scope(&new_thread_id);
    if state
        .server_store
        .get_thread(&new_internal_id)
        .await
        .map_err(internal_err)?
        .is_some()
    {
        return Err(ApiError::conflict(format!(
            "thread `{new_thread_id}` already exists"
        )));
    }

    // Fork inside the tenant's checkpoint namespace.
    let copied = state
        .checkpointer
        .fork_thread(
            &tenant.scope(&thread_id),
            &new_internal_id,
            payload.checkpoint_id.as_deref(),
        )
        .await
        .map_err(|e| {
            let message = e.to_string();
            if message.contains("unknown checkpoint id") {
                ApiError::not_found(message)
            } else {
                // No checkpoints to fork, or src == dst id collision.
                ApiError::bad_request(message)
            }
        })?;

    let fork = ThreadRecord {
        thread_id: new_thread_id.clone(),
        tenant: tenant.tenant().to_string(),
        graph: record.graph,
        metadata: json!({
            "forked_from": thread_id,
            "fork_checkpoint_id": payload.checkpoint_id,
        }),
        created_at: Utc::now(),
    };
    // A create that loses a same-id race answers 409 (the existence check
    // above is only the fast path; the store's check-and-insert is
    // authoritative).
    let created = state
        .server_store
        .create_thread(&new_internal_id, &fork)
        .await
        .map_err(internal_err)?;
    if !created {
        return Err(ApiError::conflict(format!(
            "thread `{new_thread_id}` already exists"
        )));
    }

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "thread_id": new_thread_id,
            "checkpoints_copied": copied,
        })),
    ))
}

// --------------------------------------------------------------------- //
// Thread state & history
// --------------------------------------------------------------------- //

async fn get_state(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(thread_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    require_thread(&state, &tenant, &thread_id).await?;
    let latest = state
        .checkpointer
        .get_latest(&tenant.scope(&thread_id))
        .await
        .map_err(internal_err)?;
    Ok(Json(match latest {
        None => json!({ "values": {}, "next": [], "checkpoint": null }),
        Some(cp) => json!({
            "values": cp.state.to_value(),
            "next": cp.next_nodes,
            "checkpoint": checkpoint_ref(&cp, &tenant),
        }),
    }))
}

#[derive(Debug, Deserialize)]
struct UpdateStatePayload {
    /// The full new state (JSON object).
    values: Value,
    /// Recorded for API compatibility with LangGraph's `update_state`;
    /// checkpoints do not carry per-node metadata in v0.1.
    #[serde(default)]
    as_node: Option<String>,
    /// Override for the next-node set (defaults to the previous value).
    #[serde(default)]
    next_nodes: Option<Vec<String>>,
    /// Tasks to enqueue atomically with this checkpoint through the
    /// transactional outbox (R0.6 wave 2b): every entry is validated
    /// before anything is written, and with the Postgres backend the
    /// checkpoint write and outbox enqueue commit in one transaction, so
    /// a crash can never leave a checkpoint whose effects silently
    /// vanished. The tasks become claimable when the relay publishes
    /// them; the response returns after the durable outbox write.
    #[serde(default)]
    enqueue: Option<Vec<EnqueueTaskPayload>>,
}

async fn update_state(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(thread_id): Path<String>,
    Json(payload): Json<UpdateStatePayload>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    require_thread(&state, &tenant, &thread_id).await?;
    let UpdateStatePayload {
        values,
        as_node,
        next_nodes,
        enqueue,
    } = payload;
    let _ = as_node;

    // Validate every enqueued task before any write: a malformed entry
    // fails the whole request with nothing persisted, matching the
    // all-or-nothing contract the Postgres transaction enforces.
    let outbox_tasks = enqueue
        .map(|payloads| {
            payloads
                .into_iter()
                .map(|p| build_task_record(p, &tenant))
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?;
    // The quota gate runs before any write, like validation: over quota
    // fails the whole request — checkpoint included — preserving the
    // all-or-nothing contract the Postgres transaction enforces.
    if let Some(tasks) = &outbox_tasks {
        if !tasks.is_empty() {
            enforce_task_quota(&state, &tenant, tasks.len()).await?;
        }
    }

    let internal_id = tenant.scope(&thread_id);
    let new_state = State::from_value(values)
        .map_err(|e| ApiError::bad_request(format!("`values` must be a JSON object: {e}")))?;
    // Serialize the read-modify-write per thread: two concurrent
    // `update_state` calls must not mint two checkpoints with the same
    // `step`. (Held across the checkpointer IO on purpose — this is a
    // per-thread serializer, not a global lock.)
    let lock = state_lock(&state, &internal_id).await;
    let _guard = lock.lock().await;
    let latest = state
        .checkpointer
        .get_latest(&internal_id)
        .await
        .map_err(internal_err)?;
    let (step, prev_next) = latest
        .map(|cp| (cp.step + 1, cp.next_nodes))
        .unwrap_or((0, Vec::new()));

    let cp = Checkpoint::new(
        &internal_id,
        step,
        new_state,
        next_nodes.unwrap_or(prev_next),
    );
    match &outbox_tasks {
        // Checkpoint + outbox enqueue as one durable unit (a single
        // transaction on Postgres; outbox-first ordering on the file
        // backend — see `ServerStore::checkpoint_and_enqueue`).
        Some(tasks) => state
            .server_store
            .checkpoint_and_enqueue(&cp, tasks)
            .await
            .map_err(internal_err)?,
        None => state
            .checkpointer
            .put(cp.clone())
            .await
            .map_err(internal_err)?,
    }

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "values": cp.state.to_value(),
            "next": cp.next_nodes,
            "checkpoint": checkpoint_ref(&cp, &tenant),
        })),
    ))
}

#[derive(Debug, Default, Deserialize)]
struct HistoryPayload {
    #[serde(default)]
    limit: Option<usize>,
    /// Return only checkpoints older than this checkpoint id.
    #[serde(default)]
    before: Option<String>,
}

async fn history(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(thread_id): Path<String>,
    Json(payload): Json<HistoryPayload>,
) -> Result<Json<Value>, ApiError> {
    require_thread(&state, &tenant, &thread_id).await?;
    let mut checkpoints = state
        .checkpointer
        .list(&tenant.scope(&thread_id))
        .await
        .map_err(internal_err)?;
    checkpoints.reverse(); // newest first

    if let Some(before) = &payload.before {
        match checkpoints.iter().position(|cp| &cp.id == before) {
            Some(pos) => {
                checkpoints.drain(..=pos);
            }
            // A cursor that silently resets to the full history sends
            // paginating clients into infinite loops — answer 400 instead.
            None => {
                return Err(ApiError::bad_request(format!(
                    "unknown `before` checkpoint `{before}`"
                )));
            }
        }
    }
    if let Some(limit) = payload.limit {
        checkpoints.truncate(limit);
    }

    let items: Vec<Value> = checkpoints
        .iter()
        .map(|cp| {
            json!({
                "values": cp.state.to_value(),
                "next": cp.next_nodes,
                "checkpoint": checkpoint_ref(cp, &tenant),
            })
        })
        .collect();
    Ok(Json(Value::Array(items)))
}

// --------------------------------------------------------------------- //
// Runs
// --------------------------------------------------------------------- //

async fn schedule_for_thread(
    state: &Arc<AppState>,
    tenant: &TenantContext,
    thread_id: &str,
    mut payload: RunPayload,
) -> Result<runs::Scheduled, ApiError> {
    let record = require_thread(state, tenant, thread_id).await?;
    let internal_id = tenant.scope(thread_id);
    if let Some(input) = &payload.input {
        if !input.is_object() {
            return Err(ApiError::bad_request(
                "`input` must be a JSON object".to_string(),
            ));
        }
    }
    if let Some(assistant_id) = &payload.assistant_id {
        // The id arrives in a JSON body, not a path segment, so it must be
        // validated here like every other client-chosen id: the default
        // tenant's `scope()` is the identity function, and an unvalidated
        // `"tenant/id"` value would resolve (and run) another tenant's
        // assistant record.
        validate_client_id("assistant_id", assistant_id)?;
        // Assistants are tenant-scoped: another tenant's assistant id
        // resolves to nothing here → 404.
        let assistant = state
            .server_store
            .get_assistant(&tenant.scope(assistant_id))
            .await
            .map_err(internal_err)?
            .ok_or_else(|| ApiError::not_found(format!("assistant `{assistant_id}` not found")))?;
        if assistant.graph != record.graph {
            return Err(ApiError::bad_request(format!(
                "assistant `{assistant_id}` is bound to graph `{}` but thread `{thread_id}` uses `{}`",
                assistant.graph, record.graph
            )));
        }
        // Assistant config supplies a default recursion limit; an explicit
        // `config.recursion_limit` on the payload wins.
        let payload_limit = payload.config.as_ref().and_then(|c| c.recursion_limit);
        if payload_limit.is_none() {
            if let Some(limit) = assistant
                .config
                .get("recursion_limit")
                .and_then(Value::as_u64)
            {
                payload
                    .config
                    .get_or_insert_with(RunConfigPayload::default)
                    .recursion_limit = Some(limit as usize);
            }
        }
    }
    if let Some(checkpoint) = &payload.checkpoint {
        // Time travel: the checkpoint must exist on this thread, or the
        // replay would fail deep inside the executor — answer 404 up front.
        let found = state
            .checkpointer
            .get_by_id(&internal_id, &checkpoint.checkpoint_id)
            .await
            .map_err(internal_err)?;
        if found.is_none() {
            return Err(ApiError::not_found(format!(
                "thread `{thread_id}` has no checkpoint `{}`",
                checkpoint.checkpoint_id
            )));
        }
    }
    let strategy = MultitaskStrategy::parse(payload.multitask_strategy.as_deref())
        .map_err(ApiError::bad_request)?;
    runs::schedule(
        &state.run_deps,
        &internal_id,
        thread_id,
        &record.graph,
        payload,
        strategy,
    )
    .await
}

/// `POST /threads/{id}/runs` — background run: `202 + run_id`.
async fn create_run(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(thread_id): Path<String>,
    Json(payload): Json<RunPayload>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let scheduled = schedule_for_thread(&state, &tenant, &thread_id, payload).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "run_id": scheduled.run_id,
            "thread_id": thread_id,
            "status": scheduled.status.as_str(),
        })),
    ))
}

/// Server-side ceiling for the blocking wait endpoint: a graph that never
/// terminates must not pin the handler task forever. The run itself keeps
/// executing — only the wait is bounded.
const MAX_RUN_WAIT: Duration = Duration::from_secs(3600);

/// `POST /threads/{id}/runs/wait` — blocking run: terminal result as JSON.
async fn create_run_wait(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(thread_id): Path<String>,
    Json(payload): Json<RunPayload>,
) -> Result<Json<Value>, ApiError> {
    let scheduled = schedule_for_thread(&state, &tenant, &thread_id, payload).await?;
    let mut terminal = scheduled.terminal;
    let result = tokio::time::timeout(MAX_RUN_WAIT, terminal.wait_for(|v| v.is_some()))
        .await
        .map_err(|_| {
            ApiError::new(
                StatusCode::GATEWAY_TIMEOUT,
                "timeout",
                format!(
                    "run did not reach a terminal state within {}s",
                    MAX_RUN_WAIT.as_secs()
                ),
            )
        })?
        .map_err(|_| ApiError::internal("run ended without a terminal result".to_string()))?;
    let value = result.clone().expect("wait_for predicate guarantees Some");
    Ok(Json(value))
}

/// Shared SSE response assembly for the two streaming endpoints.
fn sse_response(
    replay: Vec<runs::SseFrame>,
    broadcast: tokio::sync::broadcast::Receiver<runs::SseFrame>,
    skip_through_seq: u64,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    Sse::new(sse::frame_stream(replay, broadcast, skip_through_seq)).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    )
}

/// `POST /threads/{id}/runs/stream` — run with SSE streaming. A fresh run
/// starts a new frame sequence, so `Last-Event-ID` is deliberately ignored
/// here (a stale value from a previous run would silently drop the new
/// run's first frames); replay lives on `GET /runs/{id}/stream`.
async fn create_run_stream(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(thread_id): Path<String>,
    Json(payload): Json<RunPayload>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let scheduled = schedule_for_thread(&state, &tenant, &thread_id, payload).await?;
    Ok(sse_response(scheduled.replay, scheduled.broadcast, 0))
}

/// `GET /runs/{id}/stream` — attach to an existing run's SSE stream:
/// replays the event log (honoring `Last-Event-ID`, so a reconnecting
/// client skips frames it has already seen) and then follows live frames.
/// Cross-tenant runs answer 404, like `GET /runs/{id}`.
async fn get_run_stream(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(run_id): Path<String>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let (replay, broadcast, internal_thread_id) = state
        .run_deps
        .manager
        .stream_parts(&run_id)
        .await
        .ok_or_else(|| ApiError::not_found(format!("run `{run_id}` not found")))?;
    if !tenant.owns(&internal_thread_id) {
        return Err(ApiError::not_found(format!("run `{run_id}` not found")));
    }
    let last_seen =
        sse::parse_last_event_id(headers.get("last-event-id").and_then(|v| v.to_str().ok()));
    Ok(sse_response(replay, broadcast, last_seen))
}

/// `DELETE /threads/{id}/runs/{run_id}` — rollback: delete the checkpoints a
/// finished run created, re-anchoring the thread to the pre-run checkpoint.
///
/// The `Checkpointer` trait has no delete operation, so removal goes
/// through the JSON-file layout directly; on the Postgres backend the
/// endpoint answers 409 rather than silently deleting nothing.
///
/// Reachability: the in-memory run record is the fast path; a run lost to
/// a restart (or evicted past the retention cap) resolves through its
/// persisted journal ([`run_evidence`]) — terminal by construction once no
/// live writer remains, with its checkpoint ids recovered from the
/// journaled `checkpoint_written` events — so rollback answers 409 (or
/// applies, on the file backend) instead of 404ing on process-local state.
async fn delete_run_checkpoints(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path((thread_id, run_id)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    require_thread(&state, &tenant, &thread_id).await?;
    let (wire_thread_id, checkpoint_ids) = match state.run_deps.manager.info(&run_id).await {
        Some(info) => {
            // Cross-tenant runs are invisible (404, not 403).
            if !tenant.owns(&info.thread_id) {
                return Err(ApiError::not_found(format!("run `{run_id}` not found")));
            }
            if matches!(info.status, RunStatus::Pending | RunStatus::Running) {
                return Err(ApiError::conflict(
                    "run is still active; rollback applies to finished runs".to_string(),
                ));
            }
            (
                info.wire_thread_id,
                runs::lock_recover(&info.checkpoint_ids).clone(),
            )
        }
        None => {
            let evidence = run_evidence(&state, &tenant, &run_id).await?;
            (evidence.wire_thread_id, evidence.checkpoint_ids)
        }
    };
    if wire_thread_id != thread_id {
        return Err(ApiError::bad_request(format!(
            "run `{run_id}` does not belong to thread `{thread_id}`"
        )));
    }
    if state.config.database_url.is_some() {
        return Err(ApiError::conflict(
            "rollback is not supported with the Postgres checkpointer".to_string(),
        ));
    }

    let internal_id = tenant.scope(&thread_id);
    // Mutual exclusion with scheduling: a queued or newly-started run
    // could be executing from the very checkpoints this endpoint deletes.
    if state.run_deps.manager.thread_busy(&internal_id).await {
        return Err(ApiError::conflict(
            "thread has an active or queued run; rollback applies to idle threads".to_string(),
        ));
    }

    let ids = checkpoint_ids;
    // Rollback is only well-defined when the run's checkpoints are the
    // tail of the current history: deleting mid-history checkpoints would
    // punch holes while the endpoint claims to re-anchor the thread to
    // the pre-run checkpoint.
    let history = state
        .checkpointer
        .list(&internal_id)
        .await
        .map_err(internal_err)?;
    let is_suffix = history.len() >= ids.len()
        && history[history.len() - ids.len()..]
            .iter()
            .map(|cp| cp.id.as_str())
            .eq(ids.iter().map(String::as_str));
    if !is_suffix {
        return Err(ApiError::conflict(
            "the run's checkpoints are not the latest on this thread; \
             rollback would punch holes mid-history"
                .to_string(),
        ));
    }

    let dir = state.config.store_path.join(&internal_id);
    let mut deleted = 0usize;
    for id in &ids {
        let path = dir.join(format!("{id}.json"));
        match tokio::fs::remove_file(&path).await {
            Ok(()) => deleted += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(ApiError::internal(format!(
                    "failed to delete `{}`: {e}",
                    path.display()
                )))
            }
        }
    }

    // Re-anchor the latest pointer to the newest remaining checkpoint,
    // with the same atomic temp+rename discipline the checkpointer itself
    // uses (a crash mid-write must not leave a truncated pointer).
    let remaining = state
        .checkpointer
        .list(&internal_id)
        .await
        .map_err(internal_err)?;
    let latest_path = dir.join("latest");
    match remaining.last() {
        Some(cp) => atomic_write(&latest_path, cp.id.as_bytes())
            .await
            .map_err(internal_err)?,
        None => match tokio::fs::remove_file(&latest_path).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(path = %latest_path.display(), %e, "failed to remove latest pointer")
            }
        },
    }

    Ok(Json(json!({
        "run_id": run_id,
        "thread_id": thread_id,
        "deleted_checkpoints": deleted,
        "remaining_checkpoints": remaining.len(),
    })))
}

/// Write `bytes` to `path` atomically (temp file + rename), mirroring the
/// checkpointer's durability discipline for its `latest` pointer.
async fn atomic_write(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, path).await
}

// --------------------------------------------------------------------- //
// Run status polling
// --------------------------------------------------------------------- //

/// `GET /runs/{run_id}` — poll a run's lifecycle status; once terminal, the
/// response carries the run's `output` / `error` / `interrupt` fields.
/// Runs are tenant-scoped through their thread: a run whose thread belongs
/// to another tenant answers 404.
async fn get_run(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(run_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let info = state
        .run_deps
        .manager
        .info(&run_id)
        .await
        .ok_or_else(|| ApiError::not_found(format!("run `{run_id}` not found")))?;
    if !tenant.owns(&info.thread_id) {
        return Err(ApiError::not_found(format!("run `{run_id}` not found")));
    }
    let mut body = json!({
        "run_id": run_id,
        "thread_id": info.wire_thread_id,
        "graph": info.graph,
        "attempt": info.attempt,
        "status": info.status.as_str(),
    });
    if let Some(terminal) = info.terminal {
        if let (Some(body), Some(terminal)) = (body.as_object_mut(), terminal.as_object()) {
            for (key, value) in terminal {
                body.insert(key.clone(), value.clone());
            }
        }
    }
    Ok(Json(body))
}

/// `POST /runs/{run_id}/cancel` — propagate cancellation into the run's
/// outstanding durable tasks: every non-terminal task enqueued with this
/// `run_id` in the caller's tenant. Queued and retry-scheduled tasks move
/// to the terminal `cancelled` state (reported under `cancelled`); leased
/// tasks keep their leases with `cancel_requested` set so their holders
/// abort and report (`signalled`). Run resolution and tenant scoping
/// follow `GET /runs/{id}` — unknown or cross-tenant runs answer 404.
///
/// Scope note: this wave wires run cancellation to the *queue*. Stopping
/// the run's in-process executor is the drain half of wave 2; a task
/// enqueued after this call is not retroactively cancelled.
async fn cancel_run(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(run_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let info = state
        .run_deps
        .manager
        .info(&run_id)
        .await
        .ok_or_else(|| ApiError::not_found(format!("run `{run_id}` not found")))?;
    if !tenant.owns(&info.thread_id) {
        return Err(ApiError::not_found(format!("run `{run_id}` not found")));
    }
    let outcome = state
        .server_store
        .cancel_run_tasks(tenant.tenant(), &run_id, Utc::now())
        .await
        .map_err(internal_err)?;
    let ids =
        |tasks: Vec<TaskRecord>| -> Vec<String> { tasks.into_iter().map(|t| t.task_id).collect() };
    Ok(Json(json!({
        "run_id": run_id,
        "cancelled": ids(outcome.cancelled),
        "signalled": ids(outcome.signalled),
    })))
}

// --------------------------------------------------------------------- //
// Flight Recorder
// --------------------------------------------------------------------- //

/// A run's Flight Recorder evidence plus the metadata the read endpoints
/// need, resolved from the live run manager while the run lives in this
/// process and from the durable store otherwise.
struct RunEvidence {
    /// The graph the run executed (manager record, or the thread's binding).
    graph: String,
    /// Internal (tenant-scoped) thread id, for checkpoint read-backs.
    internal_thread_id: String,
    /// External thread id — the only form that may appear on the wire.
    wire_thread_id: String,
    /// The run's persisted journal, integrity re-verified on read. `None`
    /// when the run is known but nothing was persisted yet (queued, or
    /// before its first checkpoint boundary).
    journal: Option<JournalSnapshot>,
    /// Ids of the checkpoints the run wrote, in write order (from the
    /// manager's bookkeeping, or recovered from the journal's
    /// `checkpoint_written` events on the store path).
    checkpoint_ids: Vec<String>,
    /// `true` when the served journal is final: the run is terminal per the
    /// manager, or the manager no longer knows the run at all — evicted after
    /// termination or lost with a process restart; either way no live writer
    /// remains, so the persisted snapshot cannot grow.
    complete: bool,
}

/// Re-verify a stored snapshot's chained head hash before it is served or
/// replayed (via [`Journal::from_snapshot`]): tampered or corrupt evidence
/// answers 500 rather than being served as fact.
fn reverify_journal(run_id: &str, snapshot: JournalSnapshot) -> Result<JournalSnapshot, ApiError> {
    Journal::from_snapshot(snapshot.clone(), Clock::System).map_err(|e| {
        ApiError::internal(format!(
            "stored journal for run `{run_id}` failed its integrity check: {e}"
        ))
    })?;
    Ok(snapshot)
}

/// Resolve a run's evidence for the Flight Recorder endpoints.
///
/// Fast path: the in-memory run manager, authoritative while the run lives in
/// this process. Fallback: the server store — journals persist per run id, so
/// the evidence stays fetchable after the run's record was evicted or the
/// process restarted. The fallback's tenant check goes through the journal's
/// external thread id: looking the thread record up under the caller's tenant
/// scope doubles as the ownership proof (a cross-tenant id resolves to
/// nothing → 404, never 403) and yields the graph the run executed. A run
/// known to neither answers 404.
async fn run_evidence(
    state: &AppState,
    tenant: &TenantContext,
    run_id: &str,
) -> Result<RunEvidence, ApiError> {
    if let Some(info) = state.run_deps.manager.info(run_id).await {
        // Cross-tenant runs are invisible (404, not 403).
        if !tenant.owns(&info.thread_id) {
            return Err(ApiError::not_found(format!("run `{run_id}` not found")));
        }
        let journal = state
            .server_store
            .get_journal(run_id)
            .await
            .map_err(internal_err)?
            .map(|snapshot| reverify_journal(run_id, snapshot))
            .transpose()?;
        return Ok(RunEvidence {
            graph: info.graph,
            internal_thread_id: info.thread_id,
            wire_thread_id: info.wire_thread_id,
            journal,
            checkpoint_ids: runs::lock_recover(&info.checkpoint_ids).clone(),
            complete: info.status.is_terminal(),
        });
    }

    // Store fallback: the run is unknown to this process. A persisted journal
    // is the proof it existed — and the only handle on its ownership.
    let Some(snapshot) = state
        .server_store
        .get_journal(run_id)
        .await
        .map_err(internal_err)?
    else {
        return Err(ApiError::not_found(format!("run `{run_id}` not found")));
    };
    let internal_thread_id = tenant.scope(&snapshot.thread_id);
    let thread = state
        .server_store
        .get_thread(&internal_thread_id)
        .await
        .map_err(internal_err)?
        .ok_or_else(|| ApiError::not_found(format!("run `{run_id}` not found")))?;
    let journal = reverify_journal(run_id, snapshot)?;
    let checkpoint_ids = journal
        .events
        .iter()
        .filter(|event| event.kind == RunEventKind::CheckpointWritten)
        .filter_map(|event| crate::replay::resolve(&journal, event.output.as_ref()))
        .filter_map(|output| output.get("checkpoint_id")?.as_str().map(str::to_owned))
        .collect();
    Ok(RunEvidence {
        graph: thread.graph,
        internal_thread_id,
        wire_thread_id: journal.thread_id.clone(),
        journal: Some(journal),
        checkpoint_ids,
        complete: true,
    })
}

/// `GET /runs/{run_id}/events` — the run's journaled evidence (Flight
/// Recorder), as `{run_id, events, complete}`. `events` are core's
/// `RunEvent`s in `seq` order, in the exact golden-pinned wire shape
/// (`rusty-core/tests/golden/run_event.json`).
///
/// `complete` is `true` once the run is terminal, i.e. the served snapshot
/// is the run's final journal; while the run is active the snapshot trails
/// the live journal by at most one checkpoint boundary (it is flushed per
/// `CheckpointSaved` and at completion), and a queued run serves an empty
/// event list. Unknown and cross-tenant runs answer 404, exactly like
/// `GET /runs/{id}`.
///
/// Reachability ([`run_evidence`]): once the live run record is gone —
/// evicted past the retention cap, or lost with a restart — the events stay
/// fetchable from the persisted journal for as long as the store holds it,
/// served as `complete` (no live writer remains). The stored snapshot's
/// chained head hash is re-verified on every read: tampered or corrupt
/// evidence answers 500 rather than being served as fact.
async fn get_run_events(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(run_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let evidence = run_evidence(&state, &tenant, &run_id).await?;
    let events = evidence
        .journal
        .map(|snapshot| snapshot.events)
        .unwrap_or_default();
    Ok(Json(json!({
        "run_id": run_id,
        "events": events,
        "complete": evidence.complete,
    })))
}

/// `GET /runs/{run_id}/fixture` — download the run as a portable
/// [`ReplayFixture`]: the recorded journal (integrity-verified before
/// serving), the graph's topology hash, the run's final checkpoint, and
/// provenance metadata. CI replays the bundle with
/// `ReplayFixture::import`.
///
/// Same 404 / tenant-isolation semantics as `GET /runs/{id}`, and the same
/// store fallback as `GET /runs/{id}/events` ([`run_evidence`]): after run
/// eviction or a restart the fixture stays downloadable, with the final
/// checkpoint recovered from the journal's last `checkpoint_written` event.
/// A run with no persisted journal yet (still queued, or before its first
/// checkpoint boundary) answers `409` — the fixture would be empty evidence.
/// Server runs record under the system clock and OS entropy, so the fixture
/// carries no logical-clock / RNG-seed parameters: `exact_replay` sessions
/// work, byte-identical CI replay requires runs recorded with determinism
/// seams (a later wave's concern).
///
/// The served checkpoint's `thread_id` is rewritten to the external id —
/// the internal tenant-scoped id stored by the checkpointer must never
/// appear in a downloaded fixture.
async fn get_run_fixture(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(run_id): Path<String>,
) -> Result<Json<ReplayFixture>, ApiError> {
    let evidence = run_evidence(&state, &tenant, &run_id).await?;
    let snapshot = evidence.journal.ok_or_else(|| {
        ApiError::conflict(format!(
            "run `{run_id}` has no persisted journal yet (queued or pre-checkpoint)"
        ))
    })?;
    let (graph, _spec) = state.registry.get(&evidence.graph).ok_or_else(|| {
        ApiError::conflict(format!(
            "graph `{}` is no longer registered; cannot capture a fixture for run `{run_id}`",
            evidence.graph
        ))
    })?;

    let final_checkpoint = match evidence.checkpoint_ids.last() {
        Some(id) => state
            .checkpointer
            .get_by_id(&evidence.internal_thread_id, id)
            .await
            .map_err(internal_err)?
            .map(|mut cp| {
                cp.thread_id = evidence.wire_thread_id.clone();
                cp
            }),
        None => None,
    };

    let fixture = ReplayFixture::capture(
        format!("{} run {run_id}", evidence.graph),
        &graph,
        "unversioned",
        snapshot,
        final_checkpoint,
        None,
        None,
    );
    Ok(Json(fixture))
}

/// The effect kinds server-side replay cannot re-drive: journaled outbound
/// calls (model, tool, remote, WASM). Exact replay serves them from the
/// journal in CI via the replaying wrappers; re-executing the registered
/// graph would issue them live, breaking the zero-outbound guarantee.
fn carries_servable_effects(snapshot: &JournalSnapshot) -> bool {
    snapshot.events.iter().any(|event| {
        matches!(
            event.kind,
            RunEventKind::ModelCall
                | RunEventKind::ToolCall
                | RunEventKind::RemoteCall
                | RunEventKind::WasmCall
        )
    })
}

#[derive(Debug, Deserialize)]
struct ReplayRunPayload {
    /// The run to re-drive and verify.
    run_id: String,
}

/// `POST /runs/replay` — re-drive a journaled run server-side and verify the
/// replayed evidence against the recorded journal. Body: `{"run_id": "…"}`.
///
/// The replay runs the graph code registered in this process (not a
/// downloaded copy) against a throwaway in-memory checkpointer — the shared
/// checkpoint log is never touched — and answers exactly:
///
/// ```json
/// { "run_id": "…", "verified": true, "expected_events": 12,
///   "actual_events": 12, "first_divergence": null }
/// ```
///
/// `verified` is the evidence comparison of [`crate::replay`]: same event
/// kinds, nodes, sequences, effect classes, statuses, and payloads, with
/// per-run minted identity (checkpoint ids) and wall-clock measurements
/// excluded. `first_divergence` is the `seq` of the first disagreeing event
/// (or of the first recorded event the replay never produced).
///
/// Statuses: `404` unknown or cross-tenant run; `409` no persisted journal
/// yet (same as `/fixture`), or the run is still executing — replay verifies
/// a final journal; `422` when the run's graph is not registered in this
/// process, when the journal carries recorded model/tool/remote/WASM calls
/// (server-side replay cannot serve them — export the fixture and replay in
/// CI), or when the run resumed from a checkpoint (core's [`ExactReplay`]
/// rejects mid-run evidence).
async fn replay_run(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(payload): Json<ReplayRunPayload>,
) -> Result<Json<Value>, ApiError> {
    let run_id = payload.run_id;
    let evidence = run_evidence(&state, &tenant, &run_id).await?;
    let snapshot = evidence.journal.ok_or_else(|| {
        ApiError::conflict(format!(
            "run `{run_id}` has no persisted journal yet (queued or pre-checkpoint)"
        ))
    })?;
    if !evidence.complete {
        return Err(ApiError::conflict(format!(
            "run `{run_id}` is still executing; replay verifies a run's final journal"
        )));
    }
    let (graph, spec) = state.registry.get(&evidence.graph).ok_or_else(|| {
        ApiError::unprocessable(format!(
            "graph `{}` is not registered in this server process; cannot replay run `{run_id}`",
            evidence.graph
        ))
    })?;
    if carries_servable_effects(&snapshot) {
        return Err(ApiError::unprocessable(format!(
            "run `{run_id}` journaled model/tool/remote/WASM calls; server-side replay \
             re-executes node code and cannot serve recorded effects — download the fixture \
             (GET /runs/{run_id}/fixture) and replay it in CI with ReplayFixture"
        )));
    }
    // Pre-check the boundary ExactReplay::new enforces, so unreplayable
    // evidence answers 422 (client-actionable), not a 500.
    if snapshot
        .events
        .first()
        .is_some_and(|event| event.kind == RunEventKind::Resume)
    {
        return Err(ApiError::unprocessable(format!(
            "run `{run_id}` resumed from a checkpoint; its journal begins mid-run against \
             state it does not carry — replay the original run's journal instead"
        )));
    }
    let replay = ExactReplay::new(snapshot.clone()).map_err(|e| {
        ApiError::internal(format!(
            "stored journal for run `{run_id}` failed its integrity check: {e}"
        ))
    })?;

    let initial = crate::replay::initial_state_from(&snapshot);
    let journal = replay.fresh_journal(Clock::System);
    let params = ReplayParams::new(journal.clone(), RngSource::default())
        .with_checkpointer(Arc::new(InMemoryCheckpointer::new()));
    // A replay error (graph code changed and now fails, a reducer rejects an
    // update, …) is divergence evidence, not an HTTP error: whatever the
    // replay journaled before stopping is compared below.
    let _ = replay.run(&graph, &spec, initial, params).await;
    let replayed = journal.snapshot();
    let report = crate::replay::compare_journals(&snapshot, &replayed);
    Ok(Json(json!({
        "run_id": run_id,
        "verified": report.verified,
        "expected_events": snapshot.events.len(),
        "actual_events": replayed.events.len(),
        "first_divergence": report.first_divergence,
    })))
}

#[derive(Debug, Deserialize)]
struct DiffQuery {
    /// Base run id (the branch is diffed against it).
    base: String,
    /// Branch run id.
    branch: String,
}

/// The run's persisted journal for the diff/replay endpoints: 409 when the
/// run is known but nothing was persisted yet.
async fn require_journal(
    state: &AppState,
    tenant: &TenantContext,
    run_id: &str,
) -> Result<JournalSnapshot, ApiError> {
    let evidence = run_evidence(state, tenant, run_id).await?;
    evidence.journal.ok_or_else(|| {
        ApiError::conflict(format!(
            "run `{run_id}` has no persisted journal yet (queued or pre-checkpoint)"
        ))
    })
}

/// `GET /runs/diff?base=<run_id>&branch=<run_id>` — the structural diff of
/// two runs' journals, in core's [`BranchDiff`] serde shape as-is:
/// `first_divergent_seq`, the events `added` (branch) and `removed` (base)
/// at and after the divergence point, per-super-step state-channel
/// `step_diffs`, and token/cost `base_totals` / `branch_totals`. Events
/// compare logically — identity and timing fields excluded — so two branches
/// of one fork show their shared prefix as equal.
///
/// 404 semantics are the usual ones (unknown or cross-tenant run on either
/// side, via [`run_evidence`] — including the post-eviction / post-restart
/// store fallback); `409` when either run has no persisted journal yet.
async fn diff_runs(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Query(query): Query<DiffQuery>,
) -> Result<Json<BranchDiff>, ApiError> {
    let base = require_journal(&state, &tenant, &query.base).await?;
    let branch = require_journal(&state, &tenant, &query.branch).await?;
    Ok(Json(BranchDiff::between(&base, &branch)))
}

// --------------------------------------------------------------------- //
// Assistants
// --------------------------------------------------------------------- //

#[derive(Debug, Deserialize)]
struct CreateAssistantPayload {
    /// Human-readable name (need not be unique).
    name: String,
    /// Registered graph this assistant runs.
    graph: String,
    /// Client-chosen assistant id (a UUID v4 is generated when omitted).
    #[serde(default)]
    assistant_id: Option<String>,
    /// Free-form config metadata; `recursion_limit` is honored as a run
    /// default.
    #[serde(default)]
    config: Option<Value>,
    #[serde(default)]
    metadata: Option<Value>,
}

async fn create_assistant(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(payload): Json<CreateAssistantPayload>,
) -> Result<(StatusCode, Json<AssistantRecord>), ApiError> {
    if payload.name.trim().is_empty() {
        return Err(ApiError::bad_request(
            "`name` must not be empty".to_string(),
        ));
    }
    if !state.registry.contains(&payload.graph) {
        return Err(ApiError::bad_request(format!(
            "unknown graph `{}` (see GET /info for registered graphs)",
            payload.graph
        )));
    }
    let assistant_id = payload
        .assistant_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    validate_client_id("assistant_id", &assistant_id)?;

    // Persist under the tenant's internal id; the wire shows the external id.
    let record = AssistantRecord {
        assistant_id: tenant.scope(&assistant_id),
        name: payload.name,
        graph: payload.graph,
        config: payload.config.unwrap_or(Value::Null),
        metadata: payload.metadata.unwrap_or(Value::Null),
        created_at: Utc::now(),
    };
    let created = state
        .server_store
        .create_assistant(&record)
        .await
        .map_err(internal_err)?;
    if !created {
        return Err(ApiError::conflict(format!(
            "assistant `{assistant_id}` already exists"
        )));
    }
    let mut wire = record;
    wire.assistant_id = assistant_id;
    Ok((StatusCode::CREATED, Json(wire)))
}

async fn list_assistants(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
) -> Result<Json<Value>, ApiError> {
    let records = state
        .server_store
        .list_assistants()
        .await
        .map_err(internal_err)?;
    // Only this tenant's assistants, reported with their external ids.
    let mut records: Vec<AssistantRecord> = records
        .into_iter()
        .filter_map(|mut record| {
            let external = tenant.unscope(&record.assistant_id)?.to_string();
            record.assistant_id = external;
            Some(record)
        })
        .collect();
    records.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.assistant_id.cmp(&b.assistant_id))
    });
    Ok(Json(json!(records)))
}

async fn get_assistant(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(assistant_id): Path<String>,
) -> Result<Json<AssistantRecord>, ApiError> {
    state
        .server_store
        .get_assistant(&tenant.scope(&assistant_id))
        .await
        .map_err(internal_err)?
        .map(|mut record| {
            record.assistant_id = assistant_id.clone();
            Json(record)
        })
        .ok_or_else(|| ApiError::not_found(format!("assistant `{assistant_id}` not found")))
}

// --------------------------------------------------------------------- //
// Crons
// --------------------------------------------------------------------- //

#[derive(Debug, Deserialize)]
struct CreateCronPayload {
    /// Registered graph the fired runs execute.
    graph: String,
    /// Fixed-interval schedule in seconds (XOR `cron_expr`).
    #[serde(default)]
    interval_secs: Option<u64>,
    /// 5-field cron expression, UTC (XOR `interval_secs`).
    #[serde(default)]
    cron_expr: Option<String>,
    /// Initial state for fired runs (must be a JSON object when present).
    #[serde(default)]
    input: Option<Value>,
    /// Client-chosen cron id (a UUID v4 is generated when omitted).
    #[serde(default)]
    cron_id: Option<String>,
    #[serde(default)]
    metadata: Option<Value>,
    /// `"keep"` (default) or `"delete"` (remove the cron after its first
    /// run reaches a terminal state).
    #[serde(default)]
    on_run_completed: Option<String>,
}

async fn create_cron(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(payload): Json<CreateCronPayload>,
) -> Result<(StatusCode, Json<CronRecord>), ApiError> {
    if !state.registry.contains(&payload.graph) {
        return Err(ApiError::bad_request(format!(
            "unknown graph `{}` (see GET /info for registered graphs)",
            payload.graph
        )));
    }
    crons::validate_schedule(payload.interval_secs, payload.cron_expr.as_deref())
        .map_err(ApiError::bad_request)?;
    if let Some(input) = &payload.input {
        if !input.is_object() {
            return Err(ApiError::bad_request(
                "`input` must be a JSON object".to_string(),
            ));
        }
    }
    let on_run_completed = OnRunCompleted::parse(payload.on_run_completed.as_deref())
        .map_err(ApiError::bad_request)?;
    let cron_id = payload
        .cron_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    validate_client_id("cron_id", &cron_id)?;

    // Persist under the tenant's internal id (same scoping as assistants);
    // the wire shows the external id and the scheduler derives the owning
    // tenant back from the prefix.
    let record = CronRecord {
        cron_id: tenant.scope(&cron_id),
        graph: payload.graph,
        interval_secs: payload.interval_secs,
        cron_expr: payload.cron_expr,
        input: payload.input,
        metadata: payload.metadata.unwrap_or(Value::Null),
        on_run_completed,
        created_at: Utc::now(),
        last_run_at: None,
        runs_fired: 0,
    };
    let created = state
        .server_store
        .create_cron(&record)
        .await
        .map_err(internal_err)?;
    if !created {
        return Err(ApiError::conflict(format!(
            "cron `{cron_id}` already exists"
        )));
    }
    let mut wire = record;
    wire.cron_id = cron_id;
    Ok((StatusCode::CREATED, Json(wire)))
}

async fn list_crons(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
) -> Result<Json<Value>, ApiError> {
    let records = state
        .server_store
        .list_crons()
        .await
        .map_err(internal_err)?;
    // Only this tenant's crons, reported with their external ids.
    let mut records: Vec<CronRecord> = records
        .into_iter()
        .filter_map(|mut record| {
            let external = tenant.unscope(&record.cron_id)?.to_string();
            record.cron_id = external;
            Some(record)
        })
        .collect();
    records.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.cron_id.cmp(&b.cron_id))
    });
    Ok(Json(json!(records)))
}

async fn delete_cron(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(cron_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    if state
        .server_store
        .delete_cron(&tenant.scope(&cron_id))
        .await
        .map_err(internal_err)?
    {
        Ok(Json(json!({ "cron_id": cron_id, "deleted": true })))
    } else {
        Err(ApiError::not_found(format!("cron `{cron_id}` not found")))
    }
}

// --------------------------------------------------------------------- //
// Store (cross-thread KV)
// --------------------------------------------------------------------- //

async fn put_store_item(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path((namespace, key)): Path<(String, String)>,
    Json(value): Json<Value>,
) -> Result<(StatusCode, Json<store::StoreItem>), ApiError> {
    store::validate_segment("namespace", &namespace)?;
    store::validate_segment("key", &key)?;
    // KV namespaces are tenant-scoped: the internal namespace carries the
    // `{tenant}/` prefix, the wire item reports the external namespace.
    let (mut item, created) = state
        .server_store
        .kv_put(&tenant.scope(&namespace), &key, value)
        .await
        .map_err(internal_err)?;
    item.namespace = namespace;
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(item)))
}

async fn get_store_item(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path((namespace, key)): Path<(String, String)>,
) -> Result<Json<store::StoreItem>, ApiError> {
    store::validate_segment("namespace", &namespace)?;
    store::validate_segment("key", &key)?;
    state
        .server_store
        .kv_get(&tenant.scope(&namespace), &key)
        .await
        .map_err(internal_err)?
        .map(|mut item| {
            item.namespace = namespace.clone();
            Json(item)
        })
        .ok_or_else(|| ApiError::not_found(format!("no store item at `{namespace}/{key}`")))
}

async fn delete_store_item(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path((namespace, key)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    store::validate_segment("namespace", &namespace)?;
    store::validate_segment("key", &key)?;
    if state
        .server_store
        .kv_delete(&tenant.scope(&namespace), &key)
        .await
        .map_err(internal_err)?
    {
        Ok(Json(
            json!({ "namespace": namespace, "key": key, "deleted": true }),
        ))
    } else {
        Err(ApiError::not_found(format!(
            "no store item at `{namespace}/{key}`"
        )))
    }
}

async fn list_store_namespace(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(namespace): Path<String>,
) -> Result<Json<Value>, ApiError> {
    store::validate_segment("namespace", &namespace)?;
    let items = state
        .server_store
        .kv_list(&tenant.scope(&namespace))
        .await
        .map_err(internal_err)?;
    let items: Vec<store::StoreItem> = items
        .into_iter()
        .map(|mut item| {
            item.namespace = namespace.clone();
            item
        })
        .collect();
    Ok(Json(json!(items)))
}

// --------------------------------------------------------------------- //
// Durable task queue (R0.6)
// --------------------------------------------------------------------- //

#[derive(Debug, Deserialize)]
struct EnqueueTaskPayload {
    /// Work classification the worker fleet dispatches on (free-form).
    kind: String,
    /// Work payload: any JSON value, stored verbatim.
    payload: Value,
    /// Named pool (default `default`); workers claim from named pools.
    #[serde(default)]
    pool: Option<String>,
    /// Attempt ceiling before dead-lettering (default 3, max 100).
    #[serde(default)]
    max_attempts: Option<u32>,
    /// Dedup key, unique per tenant across live tasks: re-enqueueing with
    /// the same key returns the existing task (`deduplicated: true`).
    #[serde(default)]
    idempotency_key: Option<String>,
    /// Declared effect classification of the work (`pure` / `read_only` /
    /// `idempotent` / `compensatable` / `non_idempotent`, the Flight
    /// Recorder taxonomy). The retry policy's effect gate: a declared
    /// non-repeatable effect is never silently retried. Optional — when
    /// absent, the worker's per-attempt `retryable` flag decides.
    #[serde(default)]
    effect: Option<String>,
    /// Run linkage: the run this task belongs to.
    /// `POST /runs/{run_id}/cancel` cancels every non-terminal task
    /// carrying its run id — the run-level half of cancellation
    /// propagation. Optional; the outbox wave sets this from the run
    /// itself.
    #[serde(default)]
    run_id: Option<String>,
    /// Thread linkage (companion to `run_id`).
    #[serde(default)]
    thread_id: Option<String>,
    /// Whole-task deadline (RFC 3339), across attempts. Past it the claim
    /// path finalizes the task as cancelled instead of leasing it, and a
    /// worker that sees it pass mid-attempt reports the attempt cancelled.
    #[serde(default)]
    deadline: Option<String>,
    /// Version pin (R0.6 wave 3): the exact worker version string this task
    /// may be leased to — a run stamps its tasks with the version it started
    /// against, so a mid-run deploy never changes semantics under an
    /// in-flight execution. Exact match only; absent = unpinned, any worker.
    #[serde(default)]
    worker_version: Option<String>,
    /// Mailbox recipient (R0.7 Agent Fabric, wave 1): when set, the task is
    /// a message addressed to one agent (`agent:{id}`) and drains only
    /// through the turn-serialized `POST /agents/{id}/mailbox/next` claim —
    /// pool claims never hand it out. Pool capacity and worker-version pins
    /// do not apply to mailbox traffic. `POST /agents/{id}/mailbox` is the
    /// manifest-validating front door; this field is the direct-queue
    /// equivalent for embedders.
    #[serde(default)]
    recipient: Option<String>,
    /// Causal parentage (R0.7 wave 3): the journal event id this task is
    /// submitted under, stitched into the team's trace by TeamTrace.
    /// Optional — coordination member tasks carry it (the runtime sets it,
    /// not the member); ordinary submissions leave it absent.
    #[serde(default)]
    parent: Option<String>,
}

/// Validate an enqueue payload and build the fresh [`TaskRecord`] it
/// describes (server-minted id, caller's tenant). Shared by `POST /tasks`,
/// `POST /tasks/outbox`, and `update_state`'s atomic `enqueue` list — one
/// validation surface, so the three submission paths can never drift apart.
fn build_task_record(
    payload: EnqueueTaskPayload,
    tenant: &TenantContext,
) -> Result<TaskRecord, ApiError> {
    tasks::validate_label("kind", &payload.kind, 256).map_err(ApiError::bad_request)?;
    let pool = payload
        .pool
        .unwrap_or_else(|| tasks::DEFAULT_POOL.to_string());
    tasks::validate_pool(&pool).map_err(ApiError::bad_request)?;
    let max_attempts = payload.max_attempts.unwrap_or(tasks::DEFAULT_MAX_ATTEMPTS);
    if !(1..=tasks::MAX_ATTEMPTS_LIMIT).contains(&max_attempts) {
        return Err(ApiError::bad_request(format!(
            "`max_attempts` must be within 1..={}",
            tasks::MAX_ATTEMPTS_LIMIT
        )));
    }
    if let Some(key) = &payload.idempotency_key {
        tasks::validate_label("idempotency_key", key, 256).map_err(ApiError::bad_request)?;
    }
    let effect = payload
        .effect
        .as_deref()
        .map(tasks::parse_effect)
        .transpose()
        .map_err(ApiError::bad_request)?;
    if let Some(run_id) = &payload.run_id {
        tasks::validate_label("run_id", run_id, 256).map_err(ApiError::bad_request)?;
    }
    if let Some(thread_id) = &payload.thread_id {
        tasks::validate_label("thread_id", thread_id, 256).map_err(ApiError::bad_request)?;
    }
    let deadline = payload
        .deadline
        .as_deref()
        .map(|raw| {
            chrono::DateTime::parse_from_rfc3339(raw)
                .map(|dt| dt.with_timezone(&Utc))
                .map_err(|_| {
                    ApiError::bad_request(format!(
                        "`deadline` must be an RFC 3339 timestamp (got `{raw}`)"
                    ))
                })
        })
        .transpose()?;
    if let Some(version) = &payload.worker_version {
        tasks::validate_label("worker_version", version, 256).map_err(ApiError::bad_request)?;
    }
    if let Some(recipient) = &payload.recipient {
        agents::validate_recipient(recipient).map_err(ApiError::bad_request)?;
    }
    if let Some(parent) = &payload.parent {
        tasks::validate_label("parent", parent, 512).map_err(ApiError::bad_request)?;
    }

    Ok(TaskRecord::new(
        tasks::NewTask {
            task_id: uuid::Uuid::new_v4().to_string(),
            tenant: tenant.tenant().to_string(),
            kind: payload.kind,
            payload: payload.payload,
            pool,
            max_attempts,
            idempotency_key: payload.idempotency_key,
            effect,
            run_id: payload.run_id,
            thread_id: payload.thread_id,
            deadline,
            worker_version: payload.worker_version,
            recipient: payload.recipient,
            parent: payload.parent,
        },
        Utc::now(),
    ))
}

/// The wave-3 tenant quota gate, shared by every task submission surface
/// (`POST /tasks`, `POST /tasks/outbox`, `update_state`'s atomic `enqueue`
/// list) — one enforcement point, so the paths can never drift apart the
/// way [`build_task_record`] keeps validation singular. Runs **before any
/// write**: over quota answers `429 quota_exceeded` and nothing persists
/// (the update_state path keeps its all-or-nothing shape).
///
/// Semantics per gauge (see [`crate::tasks::TaskUsage`] for the counts):
/// `max_queued` counts the `additional` would-be tasks against the backlog;
/// `max_in_flight` and `max_dlq` are pure backpressure — already at/over
/// the cap rejects, since a submission adds neither. A submission that
/// would have deduplicated on its idempotency key can also answer 429
/// under pressure: safe (the pre-existing task is untouched) and simpler
/// than reaching inside the store's dedup decision.
async fn enforce_task_quota(
    state: &AppState,
    tenant: &TenantContext,
    additional: usize,
) -> Result<(), ApiError> {
    let quota = state.config.quota_for(tenant.tenant());
    if quota.is_unlimited() {
        return Ok(());
    }
    let usage = state
        .server_store
        .task_usage(tenant.tenant())
        .await
        .map_err(internal_err)?;
    if let Some(max) = quota.max_queued {
        if usage.queued as usize + additional > max {
            return Err(ApiError::too_many_requests(format!(
                "tenant task quota exceeded: {} tasks queued (+{additional} submitted) would pass the limit of {max} — let workers drain the queue or raise the quota",
                usage.queued
            )));
        }
    }
    if let Some(max) = quota.max_in_flight {
        if usage.in_flight as usize >= max {
            return Err(ApiError::too_many_requests(format!(
                "tenant task quota exceeded: {} tasks in flight at the limit of {max} — wait for workers to settle or raise the quota",
                usage.in_flight
            )));
        }
    }
    if let Some(max) = quota.max_dlq {
        if usage.dlq as usize >= max {
            return Err(ApiError::too_many_requests(format!(
                "tenant task quota exceeded: DLQ depth {} at the limit of {max} — inspect and re-drive the dead-letter queue before submitting more work",
                usage.dlq
            )));
        }
    }
    Ok(())
}

/// `POST /tasks` — enqueue a durable task. `201 {task_id, deduplicated:
/// false}` on creation, `200 {task_id, deduplicated: true}` when the
/// idempotency key already names a live task in this tenant. `429` when the
/// tenant is over its configured task quota (R0.6 wave 3).
async fn enqueue_task(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(payload): Json<EnqueueTaskPayload>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let record = build_task_record(payload, &tenant)?;
    enforce_task_quota(&state, &tenant, 1).await?;
    let (task, deduplicated) = state
        .server_store
        .enqueue_task(&record)
        .await
        .map_err(internal_err)?;
    let status = if deduplicated {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    Ok((
        status,
        Json(json!({
            "task_id": task.task_id,
            "deduplicated": deduplicated,
        })),
    ))
}

/// `POST /tasks/outbox` — enqueue through the transactional outbox (R0.6
/// wave 2b): the same payload as `POST /tasks`, but the task is written to
/// the outbox and becomes claimable only when the relay publishes it into
/// the queue (within one poll interval). `202 {task_id, deduplicated}` —
/// accepted, not yet queued. Delivery is at-least-once: the relay publishes
/// pending rows on every poll and on startup, deduped on the task's
/// idempotency key, so a crash anywhere in the pipe neither loses nor
/// doubles the task. Use this (or `update_state`'s `enqueue`) when the
/// submission must commit atomically with a state change.
async fn enqueue_task_outbox(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(payload): Json<EnqueueTaskPayload>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let record = build_task_record(payload, &tenant)?;
    // Same quota gate as direct enqueue: a pending outbox row counts
    // against the tenant's backlog, so the outbox is not a quota bypass.
    enforce_task_quota(&state, &tenant, 1).await?;
    let (task, deduplicated) = state
        .server_store
        .outbox_enqueue(&record)
        .await
        .map_err(internal_err)?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "task_id": task.task_id,
            "deduplicated": deduplicated,
        })),
    ))
}

#[derive(Debug, Deserialize)]
struct ClaimTaskPayload {
    /// Stable worker identity; only this id may heartbeat/settle the lease.
    worker_id: String,
    /// Pools to claim from (default `["default"]`); an explicit empty list
    /// is a 400 — it could never match a task.
    #[serde(default)]
    pools: Option<Vec<String>>,
    /// The worker's version (R0.6 wave 3), matched exactly against a task's
    /// `worker_version` pin: versioned workers take pinned and unpinned
    /// work they match; a claim without a version takes unpinned work only.
    #[serde(default)]
    worker_version: Option<String>,
    /// Visibility timeout in milliseconds (100..=3_600_000).
    lease_ms: u64,
}

/// `POST /tasks/claim` — take the oldest claimable task: `200 {"task": {…}}`
/// with a fresh lease, or `204` (empty body) when nothing is claimable.
/// Claimable means queued, failed past its backoff schedule, or leased past
/// its visibility timeout (safe reassignment after worker loss) — and, since
/// wave 3, in a pool below its configured concurrency limit
/// ([`ServerConfig::with_pool_limit`]) and matched by the worker's
/// advertised `worker_version` when the task is pinned.
async fn claim_task(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(payload): Json<ClaimTaskPayload>,
) -> Result<Response, ApiError> {
    tasks::validate_label("worker_id", &payload.worker_id, 256).map_err(ApiError::bad_request)?;
    tasks::validate_lease_ms(payload.lease_ms).map_err(ApiError::bad_request)?;
    if let Some(version) = &payload.worker_version {
        tasks::validate_label("worker_version", version, 256).map_err(ApiError::bad_request)?;
    }
    let pools = payload
        .pools
        .unwrap_or_else(|| vec![tasks::DEFAULT_POOL.to_string()]);
    if pools.is_empty() {
        return Err(ApiError::bad_request(
            "`pools` must name at least one pool".to_string(),
        ));
    }
    for pool in &pools {
        tasks::validate_pool(pool).map_err(ApiError::bad_request)?;
    }

    let claimed = state
        .server_store
        .claim_task(
            tenant.tenant(),
            &payload.worker_id,
            &tasks::ClaimScope {
                pools: &pools,
                pool_limits: &state.config.task_pool_limits,
                worker_version: payload.worker_version.as_deref(),
            },
            payload.lease_ms,
            Utc::now(),
        )
        .await
        .map_err(internal_err)?;
    Ok(match claimed {
        Some(task) => Json(json!({ "task": task.wire() })).into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    })
}

#[derive(Debug, Deserialize)]
struct HeartbeatTaskPayload {
    worker_id: String,
    /// New visibility timeout in milliseconds, from now.
    lease_ms: u64,
}

#[derive(Debug, Deserialize)]
struct CompleteTaskPayload {
    worker_id: String,
    /// The task's result: any JSON value, stored on the record.
    result: Value,
    /// The effect receipt (R0.6 wave 2b): the provider's confirmation of an
    /// idempotent effect performed by this task, journaled into the task's
    /// run as an `effect_receipt` event when the task carries run linkage.
    #[serde(default)]
    receipt: Option<EffectReceipt>,
    /// Settlement cost evidence (R0.7 wave 3): the token usage this task
    /// consumed, reported at completion. Stored on the record, where the
    /// coordination runtime's waste accounting reads it.
    #[serde(default)]
    tokens: Option<Usage>,
    /// See `tokens`.
    #[serde(default)]
    cost_usd: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct FailTaskPayload {
    worker_id: String,
    /// Free-form error classification (`timeout`, `rate_limit`, `bug`, …),
    /// stored for DLQ triage.
    error_class: String,
    /// The failure message, stored as the task's `last_error`.
    message: String,
    /// The worker's permanence judgment: `false` dead-letters immediately,
    /// regardless of remaining attempts.
    retryable: bool,
    /// Settlement cost evidence (R0.7 wave 3): what the failed attempt
    /// consumed, when the worker knows. A race loser's reported waste
    /// survives on the record.
    #[serde(default)]
    tokens: Option<Usage>,
    /// See `tokens`.
    #[serde(default)]
    cost_usd: Option<f64>,
}

/// Shared 404/409 mapping for the lease-guarded mutations: 404 when the task
/// is unknown to this tenant, 409 when it exists but the caller does not
/// hold its lease (never leased, already settled, or reclaimed by another
/// worker after the visibility timeout expired).
fn lease_outcome(
    outcome: MutationOutcome,
    task_id: &str,
    worker_id: &str,
) -> Result<TaskRecord, ApiError> {
    match outcome {
        MutationOutcome::Applied(task) => Ok(*task),
        MutationOutcome::LeaseLost => Err(ApiError::conflict(format!(
            "task `{task_id}` is not leased to worker `{worker_id}` (lost, expired and reclaimed, or already settled)"
        ))),
        MutationOutcome::Unknown => {
            Err(ApiError::not_found(format!("task `{task_id}` not found")))
        }
    }
}

/// `POST /tasks/{id}/heartbeat` — extend the held lease → `200
/// {"lease_expires_at": "…", "cancel_requested": bool}`; `409` when the
/// lease is lost. `cancel_requested` is the cancellation hint: the holder
/// should abort the attempt and report it as `cancelled` through the fail
/// path (a holder that never asks is finalized by the claim path once its
/// lease lapses).
async fn heartbeat_task(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(task_id): Path<String>,
    Json(payload): Json<HeartbeatTaskPayload>,
) -> Result<Json<Value>, ApiError> {
    tasks::validate_label("worker_id", &payload.worker_id, 256).map_err(ApiError::bad_request)?;
    tasks::validate_lease_ms(payload.lease_ms).map_err(ApiError::bad_request)?;
    let outcome = state
        .server_store
        .heartbeat_task(
            tenant.tenant(),
            &task_id,
            &payload.worker_id,
            payload.lease_ms,
            Utc::now(),
        )
        .await
        .map_err(internal_err)?;
    let task = lease_outcome(outcome, &task_id, &payload.worker_id)?;
    let expires_at = task.lease.as_ref().map(|lease| lease.expires_at);
    Ok(Json(json!({
        "lease_expires_at": expires_at,
        "cancel_requested": task.cancel_requested,
    })))
}

/// `POST /tasks/{id}/complete` — settle the held lease successfully, storing
/// `result` → `200` with the updated task record; `409` when the lease is
/// lost. A `receipt` in the payload is stored on the record and journaled
/// into the task's run (see [`journal_effect_receipt`]); its idempotency key
/// must match the task's — a receipt under a different key is evidence of a
/// wiring bug, answered `400`.
async fn complete_task(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(task_id): Path<String>,
    Json(payload): Json<CompleteTaskPayload>,
) -> Result<Json<Value>, ApiError> {
    tasks::validate_label("worker_id", &payload.worker_id, 256).map_err(ApiError::bad_request)?;
    if let Some(receipt) = &payload.receipt {
        // The receipt claims to confirm *this* task's effect; a key
        // mismatch means the worker confirmed something else. Checked
        // against the stored record before settling (an unknown task skips
        // the check — the lease protocol's 404/409 decides instead).
        if let Some(task) = state
            .server_store
            .get_task(tenant.tenant(), &task_id)
            .await
            .map_err(internal_err)?
        {
            if task.idempotency_key.as_ref() != Some(&receipt.idempotency_key) {
                return Err(ApiError::bad_request(format!(
                    "`receipt.idempotency_key` `{}` does not match the task's idempotency key `{:?}`",
                    receipt.idempotency_key, task.idempotency_key
                )));
            }
        }
    }
    let outcome = state
        .server_store
        .complete_task(
            tenant.tenant(),
            &task_id,
            &payload.worker_id,
            tasks::CompletionReport {
                result: payload.result,
                receipt: payload.receipt,
                cost: tasks::SettlementCost {
                    tokens: payload.tokens,
                    cost_usd: payload.cost_usd,
                },
            },
            Utc::now(),
        )
        .await
        .map_err(internal_err)?;
    let task = lease_outcome(outcome, &task_id, &payload.worker_id)?;
    journal_effect_receipt(&state, &tenant, &task).await;
    // Coordination trigger (R0.7 wave 3): a settled member task drives its
    // pattern forward. The settlement is already durable; the drive
    // composes after it, never inside the lease guard (the supervision
    // precedent).
    coordination::on_task_settled(
        &state.server_store,
        state.config.quota_for(tenant.tenant()),
        &tenant,
        &task,
        Utc::now(),
    )
    .await
    .map_err(internal_err)?;
    Ok(Json(task.wire()))
}

/// Journal a completed task's effect receipt into its run's persisted
/// Flight Recorder journal (R0.6 wave 2b): an `effect_receipt` RunEvent
/// whose causal parent is the journal's current head — the honest parent
/// while task lifecycle events (submission, lease, completion) are not yet
/// journaled; once they are, the receipt's parent becomes the task's
/// completion event. Exact replay's receipt lookup
/// (`JournalSnapshot::find_effect_receipt`) then serves the receipt instead
/// of re-sending the effect.
///
/// Deliberately best-effort: the receipt is already durable on the task
/// record, so a journaling failure (a live run whose journal is not yet
/// persisted, a cross-tenant run linkage, a store error) is logged, never
/// surfaced as a request failure. One honest gap, by design: while the run
/// is still live, its next checkpoint-boundary journal flush rewrites the
/// stored snapshot and would drop an appended receipt — the durable fix is
/// the run-side wiring (the run journaling its task lifecycle itself), the
/// documented integration point for a later wave.
async fn journal_effect_receipt(state: &AppState, tenant: &TenantContext, task: &TaskRecord) {
    let (Some(receipt), Some(run_id)) = (&task.receipt, &task.run_id) else {
        return;
    };
    if let Err(error) = try_journal_effect_receipt(state, tenant, receipt, run_id).await {
        tracing::warn!(
            task_id = %task.task_id,
            %run_id,
            %error,
            "effect receipt stays on the task record; journaling skipped"
        );
    }
}

/// The fallible body of [`journal_effect_receipt`], split out so the caller
/// owns the logging decision.
async fn try_journal_effect_receipt(
    state: &AppState,
    tenant: &TenantContext,
    receipt: &EffectReceipt,
    run_id: &str,
) -> Result<(), String> {
    let Some(snapshot) = state
        .server_store
        .get_journal(run_id)
        .await
        .map_err(|e| format!("load journal: {e}"))?
    else {
        return Err("run has no persisted journal yet".to_string());
    };
    // Ownership proof, the same shape as the run-evidence fallback: the
    // journal's wire thread id scoped to this tenant must resolve, or the
    // task's run linkage names another tenant's run and journaling into it
    // would leak evidence across the isolation boundary.
    let internal_thread_id = tenant.scope(&snapshot.thread_id);
    let owned = state
        .server_store
        .get_thread(&internal_thread_id)
        .await
        .map_err(|e| format!("resolve thread: {e}"))?
        .is_some();
    if !owned {
        return Err("run does not resolve in this tenant".to_string());
    }
    let journal = Journal::from_snapshot(snapshot, Clock::System)
        .map_err(|e| format!("journal failed its integrity check: {e}"))?;
    let parent = journal.events().last().map(|event| event.id.clone());
    journal.record_effect_receipt(receipt, parent);
    state
        .server_store
        .put_journal(&journal.snapshot())
        .await
        .map_err(|e| format!("persist journal: {e}"))
}

// --------------------------------------------------------------------- //
// Governed memory (R0.8 Rusty Learn, wave 1)
//
// The write/read surface over core's memory contracts (`docs/learn-
// design.md`, wave 1): content-addressed, immutable, scoped, attributed
// records; structured retrieval with an optional token-bounded
// deterministic assembly. Scope authorization is enforced here, at the
// write gate — the store trusts what the route admitted.
// --------------------------------------------------------------------- //

#[derive(Debug, Deserialize)]
struct WriteMemoryPayload {
    /// What the record is.
    kind: MemoryKind,
    /// Whose memory it is: `{scope, id}` (`run` scope is rejected — the
    /// runtime writes run-scoped memory on a run's behalf).
    scope: ScopeAddress,
    /// The record body. Inline at or below the journal's payload
    /// threshold; above it the body spills, content-addressed, into the
    /// artifact store and reads re-inline it — the served record is
    /// always self-contained.
    content: Value,
    /// Who writes it (`agent:{id}` / `human:{id}` / `distiller:{name}` /
    /// `system`). Provenance is mandatory: a record that cannot name its
    /// origin cannot be audited.
    author: ProvenanceAuthor,
    /// The writer-declared lookup key, when the record answers a named
    /// question.
    #[serde(default)]
    key: Option<String>,
    /// Writer-declared tags (retrieval matches by equality).
    #[serde(default)]
    tags: Vec<String>,
    /// The assembly rank's first input (default 0).
    #[serde(default)]
    priority: i64,
    /// What the record was derived from.
    #[serde(default)]
    evidence: Option<MemoryEvidence>,
    /// The writer-declared confidence in `(0, 1]`. Optional for human
    /// authors (defaults to 1.0 — the claim is the person's, stated
    /// plainly); required for every other author.
    #[serde(default)]
    confidence: Option<f64>,
    /// When the system learned it (default: now). Part of the content
    /// address — provenance is identity — so an importer (or a retried
    /// submission naming the same learning instant) converges on one
    /// record, while two genuinely different learnings of the same
    /// content stay distinct records.
    #[serde(default)]
    written_at: Option<DateTime<Utc>>,
    /// Inclusive start of the claimed-true interval (default: now).
    #[serde(default)]
    valid_from: Option<DateTime<Utc>>,
    /// Exclusive end of the claimed-true interval (default: open-ended).
    #[serde(default)]
    valid_until: Option<DateTime<Utc>>,
    /// Optional TTL — expiration is a retrieval filter, not a reaper.
    #[serde(default)]
    expires_at: Option<DateTime<Utc>>,
    /// The (bare) content address this record replaces, when it does.
    #[serde(default)]
    supersedes: Option<String>,
    /// Journal the write into this run's Flight Recorder journal as a
    /// `memory_write` event (best-effort — the write is durable in the
    /// memory store either way), with `parent` as its causal parent.
    #[serde(default)]
    run_id: Option<String>,
    /// The causal parent journal-event id for the journaled write
    /// (default: the journal's current head, the receipt precedent).
    #[serde(default)]
    parent: Option<String>,
}

/// The scope-authorization write gate (the design's gates), shared by
/// `POST /memory` and the wave-2 correction/consolidation surfaces so the
/// paths can never drift apart:
/// - `run` scope → `400` unless `allow_run`: the runtime writes
///   run-scoped memory on a run's behalf, and the correction loop is the
///   one governed client path that may join it (a run-scope correction is
///   adopted directly — it affects only the run that produced it).
/// - `agent` scope → the agent must be registered in this tenant (`404`)
///   and its manifest must declare `StateScope::Private` (`403`) — agent
///   memory is the agent's own, and the manifest is what grants it.
/// - `tenant` scope → the scope id must be the caller's own tenant
///   (`403`): tenant isolation is not a scope a caller can cross.
/// - `team` / `user` scopes ride tenant namespacing unchanged.
async fn check_memory_scope_gate(
    state: &AppState,
    tenant: &TenantContext,
    scope: &ScopeAddress,
    allow_run: bool,
) -> Result<(), ApiError> {
    tasks::validate_label("scope.id", &scope.id, 256).map_err(ApiError::bad_request)?;
    match scope.scope {
        MemoryScope::Run if !allow_run => Err(ApiError::bad_request(
            "`run`-scoped memory is runtime-only: the runtime writes it on a run's \
             behalf — the API accepts `agent`, `team`, `user`, and `tenant` scopes"
                .to_string(),
        )),
        MemoryScope::Run => Ok(()),
        MemoryScope::Agent => {
            let scoped_agent = tenant.scope(&scope.id);
            let agent = state
                .server_store
                .get_agent(&scoped_agent)
                .await
                .map_err(internal_err)?
                .ok_or_else(|| ApiError::not_found(format!("agent `{}` not found", scope.id)))?;
            if !agent.manifest.scopes.contains(&StateScope::Private) {
                return Err(ApiError::new(
                    StatusCode::FORBIDDEN,
                    "forbidden",
                    format!(
                        "agent `{}` does not declare the `private` state scope in its manifest \
                         — agent-scoped memory is the agent's own, and the manifest is what \
                         grants it",
                        scope.id
                    ),
                ));
            }
            Ok(())
        }
        MemoryScope::Tenant => {
            if scope.id != tenant.tenant() {
                return Err(ApiError::new(
                    StatusCode::FORBIDDEN,
                    "forbidden",
                    format!(
                        "tenant-scoped memory id `{}` is not the caller's tenant `{}` — \
                         tenant isolation is not a scope a caller can cross",
                        scope.id,
                        tenant.tenant()
                    ),
                ));
            }
            Ok(())
        }
        MemoryScope::Team | MemoryScope::User => Ok(()),
    }
}

/// `POST /memory` — write a governed memory record → `201 {memory_id,
/// created, record}`; `200` + `created: false` when the content address
/// is already stored (content addressing makes the write idempotent by
/// construction — the `Effect::Idempotent` write converges).
///
/// The write gates (the design's scope authorization):
/// - `run` scope → `400`: runtime-only, never client-written.
/// - `agent` scope → the agent must be registered in this tenant (`404`)
///   and its manifest must declare `StateScope::Private` (`403`) — agent
///   memory is the agent's own, and the manifest is what grants it.
/// - `tenant` scope → the scope id must be the caller's own tenant
///   (`403`): tenant isolation is not a scope a caller can cross.
/// - `team` / `user` scopes ride tenant namespacing unchanged.
async fn write_memory(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(payload): Json<WriteMemoryPayload>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    check_memory_scope_gate(&state, &tenant, &payload.scope, false).await?;
    let confidence = match (payload.confidence, &payload.author) {
        (Some(confidence), _) => confidence,
        (None, ProvenanceAuthor::Human { .. }) => 1.0,
        (None, _) => {
            return Err(ApiError::bad_request(
                "`confidence` is required for non-human authors — human-authored records \
                 default to 1.0 (the claim is the person's); every other author must declare \
                 its confidence explicitly"
                    .to_string(),
            ));
        }
    };
    if let Some(key) = &payload.key {
        tasks::validate_label("key", key, 256).map_err(ApiError::bad_request)?;
    }
    let now = Utc::now();
    let written_at = payload.written_at.unwrap_or(now);
    let provenance = MemoryProvenance {
        author: payload.author,
        evidence: payload.evidence.unwrap_or_default(),
        written_at,
    };
    let validity = ValidityWindow {
        valid_from: payload.valid_from.unwrap_or(now),
        valid_until: payload.valid_until,
    };
    let mut record = MemoryRecord::new(
        payload.kind,
        payload.scope,
        provenance,
        confidence,
        validity,
        // `created_at` duplicates `provenance.written_at` deliberately
        // (the record stays self-contained when a consumer summarizes
        // provenance away) — so it follows an explicit `written_at`.
        written_at,
        payload.content.clone(),
    )
    .map_err(|e| ApiError::bad_request(e.to_string()))?;
    if let Some(key) = payload.key {
        record = record.with_key(key);
    }
    if !payload.tags.is_empty() {
        record = record.with_tags(payload.tags);
    }
    if payload.priority != 0 {
        record = record.with_priority(payload.priority);
    }
    if let Some(expires_at) = payload.expires_at {
        record = record.with_expires_at(expires_at);
    }
    if let Some(supersedes) = payload.supersedes {
        record = record.with_supersedes(supersedes);
    }

    let created = state
        .server_store
        .put_memory(tenant.tenant(), &record, &payload.content)
        .await
        .map_err(internal_err)?;
    if let Some(run_id) = &payload.run_id {
        journal_memory_write(&state, &tenant, run_id, &record, payload.parent).await;
    }
    // Serve the *stored* record, re-read through the store: artifact-
    // spilled bodies come back re-inlined (self-contained), and on a
    // dedupe the caller sees the record that is actually stored — the
    // content address covers content + provenance only, so a re-write
    // with different tags or priority does not update them, and the
    // response must not pretend it did.
    let stored = state
        .server_store
        .get_memory(tenant.tenant(), &record.memory_id)
        .await
        .map_err(internal_err)?
        .ok_or_else(|| {
            ApiError::internal("memory record missing immediately after write".to_string())
        })?;
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((
        status,
        Json(json!({
            "memory_id": stored.memory_id,
            "created": created,
            "record": stored,
        })),
    ))
}

/// `GET /memory/{memory_id}` — fetch one record by content address
/// (`404` unknown/cross-tenant — the two are indistinguishable by
/// design). Artifact-spilled bodies are re-inlined by the store, so the
/// served record is self-contained.
async fn get_memory(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(memory_id): Path<String>,
) -> Result<Json<MemoryRecord>, ApiError> {
    state
        .server_store
        .get_memory(tenant.tenant(), &memory_id)
        .await
        .map_err(internal_err)?
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("memory `{memory_id}` not found")))
}

#[derive(Debug, Deserialize)]
struct QueryMemoryPayload {
    /// The structured filters (all optional; an empty query matches the
    /// whole tenant namespace, minus expired and superseded records —
    /// the two defaults core's `MemoryQuery` declares).
    #[serde(flatten)]
    query: MemoryQuery,
    /// Pack the matches into a token-bounded deterministic assembly.
    /// Required when `run_id` is set: journaled reads are budgeted
    /// reads — the journaled request is the resolved query plus the
    /// budget it was assembled under (core's `memory_read_request`
    /// shape).
    #[serde(default)]
    budget: Option<ContextBudget>,
    /// Journal the read into this run's journal as a `memory_read`
    /// event (best-effort), with `parent` as its causal parent.
    #[serde(default)]
    run_id: Option<String>,
    /// The causal parent journal-event id (default: the journal's
    /// current head).
    #[serde(default)]
    parent: Option<String>,
}

/// `POST /memory/query` — structured retrieval (deliberately not
/// semantic: R0.8 has no similarity search, so writers key and tag
/// deliberately and absence of a hit is absence of a key, not absence
/// of a fact). `as_of` resolves at read time when unset. With `budget`,
/// answers the deterministic token-bounded `MemoryAssembly` (`422` when
/// a hard budget overflows); without, the rank-ordered records — ranked
/// through the assembly's total order, so the two read shapes agree on
/// ordering by construction.
async fn query_memory(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(payload): Json<QueryMemoryPayload>,
) -> Result<Json<Value>, ApiError> {
    let mut query = payload.query;
    let as_of = query.as_of.unwrap_or_else(Utc::now);
    query.as_of = Some(as_of);
    if payload.run_id.is_some() && payload.budget.is_none() {
        return Err(ApiError::bad_request(
            "`budget` is required with `run_id`: a journaled memory read is a budgeted \
             read — the journaled request is the resolved query plus the budget it was \
             assembled under"
                .to_string(),
        ));
    }
    let records = state
        .server_store
        .query_memory(tenant.tenant(), &query, as_of)
        .await
        .map_err(internal_err)?;
    match payload.budget {
        Some(budget) => {
            let assembly =
                assemble(records, &budget).map_err(|e| ApiError::unprocessable(e.to_string()))?;
            if let Some(run_id) = &payload.run_id {
                journal_memory_read(
                    &state,
                    &tenant,
                    run_id,
                    &query,
                    &budget,
                    &assembly,
                    payload.parent,
                )
                .await;
            }
            serde_json::to_value(&assembly)
                .map(Json)
                .map_err(internal_err)
        }
        None => {
            // An unbounded budget packs everything, so `assemble` doubles
            // as the ranking definition — the two read shapes can never
            // drift apart on ordering.
            let ranked = assemble(records, &ContextBudget::new(u32::MAX)).map_err(internal_err)?;
            Ok(Json(json!({ "records": ranked.records })))
        }
    }
}

/// Journal a memory write into the given run's persisted Flight
/// Recorder journal — the same best-effort discipline as
/// [`journal_effect_receipt`]: the write is already durable in the
/// memory store, so a journaling failure (a live run whose journal is
/// not yet persisted, a cross-tenant run linkage, a store error) is
/// logged, never surfaced as a request failure. The event shape mirrors
/// core's `JournaledMemory::write` exactly, so a route-journaled write
/// is indistinguishable from a runtime-journaled one.
async fn journal_memory_write(
    state: &AppState,
    tenant: &TenantContext,
    run_id: &str,
    record: &MemoryRecord,
    parent: Option<String>,
) {
    let draft = EventDraft::new(RunEventKind::MemoryWrite, Effect::Idempotent).input(json!({
        "effect_key": memory_effect_key(&record.scope, &record.memory_id),
        "memory_id": record.memory_id,
    }));
    let draft = match serde_json::to_value(record) {
        Ok(output) => draft.output(output),
        Err(error) => {
            tracing::warn!(%run_id, %error, "memory record failed to serialize; journaling skipped");
            return;
        }
    };
    if let Err(error) = try_journal_memory_event(state, tenant, run_id, parent, draft).await {
        tracing::warn!(
            %run_id,
            memory_id = %record.memory_id,
            %error,
            "memory write is durable in the store; journaling skipped"
        );
    }
}

/// Journal a memory read into the given run's persisted journal —
/// best-effort, the [`journal_memory_write`] discipline. The event
/// shape mirrors core's `JournaledMemory::read`: the request is the
/// resolved query plus budget (`memory_read_request`), the output the
/// served assembly.
async fn journal_memory_read(
    state: &AppState,
    tenant: &TenantContext,
    run_id: &str,
    query: &MemoryQuery,
    budget: &ContextBudget,
    assembly: &rusty_agent_runtime::memory::MemoryAssembly,
    parent: Option<String>,
) {
    let draft = EventDraft::new(RunEventKind::MemoryRead, Effect::ReadOnly)
        .input(memory_read_request(query, budget));
    let draft = match serde_json::to_value(assembly) {
        Ok(output) => draft.output(output),
        Err(error) => {
            tracing::warn!(%run_id, %error, "memory assembly failed to serialize; journaling skipped");
            return;
        }
    };
    if let Err(error) = try_journal_memory_event(state, tenant, run_id, parent, draft).await {
        tracing::warn!(
            %run_id,
            %error,
            "memory read answered from the store; journaling skipped"
        );
    }
}

/// The fallible body shared by the memory journalers, mirroring
/// [`try_journal_effect_receipt`]: ownership proof first (the journal's
/// thread must resolve in this tenant — journaling into another
/// tenant's run would leak evidence across the isolation boundary),
/// integrity re-check on load, append, persist. `parent` defaults to
/// the journal's current head (the receipt precedent).
///
/// The receipt journaler's documented gap applies unchanged, with one
/// addition to name honestly: appending to a *completed* run's journal
/// adds evidence the run's execution never produced, so that journal no
/// longer exactly replays — the appended event has no issuing node.
/// These events are post-hoc attribution evidence (the memory operation
/// naming the run it belongs to), not execution evidence; runs whose
/// replay must stay exact take the runtime's own journaled seam.
async fn try_journal_memory_event(
    state: &AppState,
    tenant: &TenantContext,
    run_id: &str,
    parent: Option<String>,
    draft: EventDraft,
) -> Result<(), String> {
    let Some(snapshot) = state
        .server_store
        .get_journal(run_id)
        .await
        .map_err(|e| format!("load journal: {e}"))?
    else {
        return Err("run has no persisted journal yet".to_string());
    };
    let internal_thread_id = tenant.scope(&snapshot.thread_id);
    let owned = state
        .server_store
        .get_thread(&internal_thread_id)
        .await
        .map_err(|e| format!("resolve thread: {e}"))?
        .is_some();
    if !owned {
        return Err("run does not resolve in this tenant".to_string());
    }
    let journal = Journal::from_snapshot(snapshot, Clock::System)
        .map_err(|e| format!("journal failed its integrity check: {e}"))?;
    let parent = parent.or_else(|| journal.events().last().map(|event| event.id.clone()));
    let draft = match parent {
        Some(parent) => draft.parent(parent),
        None => draft,
    };
    journal.record(draft);
    state
        .server_store
        .put_journal(&journal.snapshot())
        .await
        .map_err(|e| format!("persist journal: {e}"))
}

// --------------------------------------------------------------------- //
// The correction loop and memory operations (R0.8 Rusty Learn, wave 2)
//
// The correction loop's record-plane half (`docs/learn-design.md`, "The
// correction loop"): a correction becomes an attributed candidate memory
// or example — never an in-place rewrite of what it corrects. The memory
// operations — consolidation, conflict detection, forgetting — are
// journaled transitions over the store, never background daemons.
// --------------------------------------------------------------------- //

#[derive(Debug, Deserialize)]
struct CorrectionPayload {
    /// The correction contract (core's `Correction`, golden-pinned).
    /// Author attribution is validated at deserialization — an
    /// unattributed correction never reaches this handler.
    #[serde(flatten)]
    correction: Correction,
    /// Journal the derived writes into this run's journal as
    /// `memory_write` events (best-effort, the wave-1 discipline).
    /// Defaults to the corrected run when the target is a journaled run
    /// event — the correction names the run it belongs to.
    #[serde(default)]
    run_id: Option<String>,
    /// The causal parent journal-event id (default: the journal's head).
    #[serde(default)]
    parent: Option<String>,
}

/// The input a journaled run event saw, resolved from the snapshot:
/// artifact-referenced payloads re-inline from the snapshot's artifact map
/// (the same resolution `MemoryReplaySource` applies). `None` when the
/// event carries no input payload — the example then records null, the
/// honest shape of "the event had no input".
fn correction_event_input(
    event: &rusty_agent_runtime::record::RunEvent,
    snapshot: &JournalSnapshot,
) -> Option<Value> {
    match event.input.as_ref()? {
        PayloadRef::Inline(value) => Some(value.clone()),
        PayloadRef::Artifact(reference) => snapshot.artifacts.get(&reference.sha256).cloned(),
    }
}

/// `POST /memory/corrections` — submit a human correction → `201
/// {correction_id, attribution, candidate, memory_id, created, record,
/// superseded, example_id}` (`200` + `created: false` when this tenant
/// already holds a record derived from the same correction id — the id
/// rides the derived records' provenance evidence, so a retried
/// submission resolves what the first attempt wrote rather than minting
/// a second record with a new learning instant).
///
/// The three rules (`docs/learn-design.md`):
///
/// 1. **Attribution travels with the derived record**: `human:{author}`
///    provenance with the correction id in evidence, confidence 1.0 — the
///    claim is the person's, stated plainly.
/// 2. **Scope decides the path**: run scope is adopted directly (the one
///    place the API admits run scope, exactly because adoption affects
///    only the run that produced it); agent scope or wider becomes a
///    candidate — `candidacy: pending`, queryable via `candidates_only` —
///    because a wrong human correction at tenant scope is a production
///    incident with a name attached.
/// 3. **Corrections enter evaluation as examples**: a target of
///    `{type: run_event}` additionally yields an `example`-kind record —
///    the input the run saw (read from the journaled event, never re-asked
///    of the world) plus the corrected behavior.
///
/// A correction targeting a memory record inherits the target's key, and
/// a same-key correction-sourced write auto-supersedes the prior record
/// (open question 5: corrections are trusted because they are
/// attributed). There is no correction event kind: the derived writes
/// journal through the memory-write seam with the correction's
/// attribution in their provenance.
async fn submit_correction(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(payload): Json<CorrectionPayload>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let correction = payload.correction;
    tasks::validate_label("correction_id", &correction.correction_id, 256)
        .map_err(ApiError::bad_request)?;
    tasks::validate_label("author", &correction.author, 256).map_err(ApiError::bad_request)?;
    // The shared gate, with the correction loop's one exception.
    check_memory_scope_gate(&state, &tenant, &correction.scope, true).await?;

    // Retry convergence on the correction id: it rides the derived
    // records' provenance evidence, so a resubmission resolves what the
    // first attempt wrote instead of minting a second record with a new
    // `written_at` (hence a new content address). The search spans
    // superseded and expired records — a retried submission must
    // converge even after a later correction superseded the first's
    // record.
    let prior = state
        .server_store
        .query_memory(
            tenant.tenant(),
            &MemoryQuery {
                scope: Some(correction.scope.clone()),
                include_expired: true,
                include_superseded: true,
                ..MemoryQuery::default()
            },
            Utc::now(),
        )
        .await
        .map_err(internal_err)?;
    let names_correction = |record: &MemoryRecord| {
        record.provenance.evidence.correction_id.as_deref()
            == Some(correction.correction_id.as_str())
    };
    if let Some(prior_memory) = prior
        .iter()
        .find(|record| record.kind == MemoryKind::Fact && names_correction(record))
    {
        let example_id = prior
            .iter()
            .find(|record| record.kind == MemoryKind::Example && names_correction(record))
            .map(|record| record.memory_id.clone());
        return Ok((
            StatusCode::OK,
            Json(json!({
                "correction_id": correction.correction_id,
                "attribution": correction.attribution(),
                "candidate": prior_memory.candidacy.is_some(),
                "memory_id": prior_memory.memory_id,
                "created": false,
                "record": prior_memory,
                "superseded": prior_memory.supersedes,
                "example_id": example_id,
            })),
        ));
    }

    let now = Utc::now();
    let mut key = None;
    let mut example_input = None;
    let mut journal_run_id = payload.run_id.clone();
    match &correction.target {
        CorrectionTarget::Memory { memory_id } => {
            // The target must resolve (unknown or cross-tenant → 404, the
            // two indistinguishable by design): the derived record
            // inherits its key, which is what fires the same-key
            // auto-supersession below.
            let target = state
                .server_store
                .get_memory(tenant.tenant(), memory_id)
                .await
                .map_err(internal_err)?
                .ok_or_else(|| {
                    ApiError::not_found(format!("correction target memory `{memory_id}` not found"))
                })?;
            key = target.key;
        }
        CorrectionTarget::RunEvent { run_id, event_id } => {
            let snapshot = state
                .server_store
                .get_journal(run_id)
                .await
                .map_err(internal_err)?
                .ok_or_else(|| {
                    ApiError::not_found(format!(
                        "correction target run `{run_id}` has no persisted journal"
                    ))
                })?;
            // Ownership, the journalers' rule: correcting another
            // tenant's run answers 404, never 403.
            let internal_thread_id = tenant.scope(&snapshot.thread_id);
            let owned = state
                .server_store
                .get_thread(&internal_thread_id)
                .await
                .map_err(internal_err)?
                .is_some();
            if !owned {
                return Err(ApiError::not_found(format!("run `{run_id}` not found")));
            }
            let event = snapshot
                .events
                .iter()
                .find(|event| &event.id == event_id)
                .ok_or_else(|| {
                    ApiError::not_found(format!(
                        "run `{run_id}` has no journaled event `{event_id}`"
                    ))
                })?;
            example_input = Some(correction_event_input(event, &snapshot).unwrap_or(Value::Null));
            journal_run_id = journal_run_id.or(Some(run_id.clone()));
        }
        CorrectionTarget::Prompt { .. } => {}
    }

    // Same-key correction-sourced writes auto-supersede the prior record
    // (open question 5). The current truth at the key is the top-ranked
    // live record — the assembly's total order, unbounded. A second live
    // record at the key, when one exists, is conflict evidence, and this
    // endpoint leaves it for the review listing.
    let mut supersedes = None;
    if let Some(key) = &key {
        let live = state
            .server_store
            .query_memory(
                tenant.tenant(),
                &MemoryQuery {
                    scope: Some(correction.scope.clone()),
                    key: Some(key.clone()),
                    ..MemoryQuery::default()
                },
                now,
            )
            .await
            .map_err(internal_err)?;
        supersedes = assemble(live, &ContextBudget::new(u32::MAX))
            .map_err(internal_err)?
            .records
            .into_iter()
            .next()
            .map(|record| record.memory_id);
    }

    let candidacy = correction.is_candidate().then_some(Candidacy::Pending);
    let provenance = MemoryProvenance {
        author: correction.author_as_provenance(),
        evidence: correction.evidence(),
        written_at: now,
    };
    let build = |kind: MemoryKind, content: Value| -> Result<MemoryRecord, ApiError> {
        let mut record = MemoryRecord::new(
            kind,
            correction.scope.clone(),
            provenance.clone(),
            1.0,
            ValidityWindow::starting(now),
            now,
            content,
        )
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
        if let Some(key) = &key {
            record = record.with_key(key.clone());
        }
        if let Some(supersedes) = &supersedes {
            record = record.with_supersedes(supersedes.clone());
        }
        if let Some(candidacy) = candidacy {
            record = record.with_candidacy(candidacy);
        }
        Ok(record)
    };

    // The candidate (or, at run scope, the adopted) memory: the corrected
    // content asserted at the target scope.
    let memory = build(MemoryKind::Fact, correction.corrected.clone())?;
    let created = state
        .server_store
        .put_memory(tenant.tenant(), &memory, &correction.corrected)
        .await
        .map_err(internal_err)?;
    if let Some(run_id) = &journal_run_id {
        journal_memory_write(&state, &tenant, run_id, &memory, payload.parent.clone()).await;
    }

    // The dataset-example half of the exit criterion: a correction whose
    // target is a journaled run event also yields an `example`-kind
    // record. (Run-event targets carry no inherited key, so the example
    // never joins the supersession chain — it is dataset evidence, not a
    // contender for the key.)
    let mut example_id = None;
    if let Some(input) = example_input {
        let content = json!({
            "input": input,
            "corrected": correction.corrected,
        });
        let example = build(MemoryKind::Example, content.clone())?;
        example_id = Some(example.memory_id.clone());
        state
            .server_store
            .put_memory(tenant.tenant(), &example, &content)
            .await
            .map_err(internal_err)?;
        if let Some(run_id) = &journal_run_id {
            journal_memory_write(&state, &tenant, run_id, &example, payload.parent.clone()).await;
        }
    }

    // Serve the stored record, re-read (the write_memory rule: spilled
    // bodies re-inline, and a dedupe must show what is actually stored).
    let stored = state
        .server_store
        .get_memory(tenant.tenant(), &memory.memory_id)
        .await
        .map_err(internal_err)?
        .ok_or_else(|| {
            ApiError::internal(
                "correction-derived record missing immediately after write".to_string(),
            )
        })?;
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((
        status,
        Json(json!({
            "correction_id": correction.correction_id,
            "attribution": correction.attribution(),
            "candidate": candidacy.is_some(),
            "memory_id": stored.memory_id,
            "created": created,
            "record": stored,
            "superseded": memory.supersedes,
            "example_id": example_id,
        })),
    ))
}

#[derive(Debug, Deserialize)]
struct ConsolidatePayload {
    /// The scope every named record must live at: one scope per
    /// consolidation — a summary spans scopes never.
    scope: ScopeAddress,
    /// Exactly the records the task reads (explicit ids, the auditable
    /// selector), in any order; sorted and deduped at enqueue.
    memory_ids: Vec<String>,
    /// The distiller's name, recorded on the summary's provenance.
    distiller: String,
    /// The summary's lookup key, when it answers a named question.
    #[serde(default)]
    key: Option<String>,
    /// The summary's tags.
    #[serde(default)]
    tags: Vec<String>,
    /// The summary's explicit priority (default 0).
    #[serde(default)]
    priority: i64,
    /// The queue pool the task lands in (default `default`).
    #[serde(default)]
    pool: Option<String>,
    /// Run linkage for the task record; the executing worker passes it
    /// through to the summary write to journal it.
    #[serde(default)]
    run_id: Option<String>,
    /// Causal parentage for the task record.
    #[serde(default)]
    parent: Option<String>,
}

/// `POST /memory/consolidate` — enqueue a consolidation as a durable task
/// (`memory_consolidation`, R0.6 machinery: leased, retried under the
/// shared `ErrorClass` taxonomy, dead-lettered with evidence,
/// quota-counted) → `201 {task_id, deduplicated, kind}` (`200` +
/// `deduplicated: true` when the same scope + source set already names a
/// live task — the derived idempotency key makes retried submissions
/// converge).
///
/// Orchestration is the runtime's; the distillation semantics are the
/// claiming worker's (the distiller boundary): the worker claims the
/// task, reads the named records, and writes its summary through the
/// governed write path (`kind: summary`, the distiller author, the source
/// ids in `evidence.source_memory_ids`, the task payload's `written_at`
/// as `written_at` — minted once at enqueue, so a retried execution names
/// the same learning instant and its content-addressed write converges).
/// The summary's source naming supersedes the sources in default
/// retrieval; execution settles the task through the unchanged
/// heartbeat/complete/fail protocol.
///
/// `400` when `memory_ids` is empty or names a record outside the
/// declared scope; `404` when a named record does not resolve in this
/// tenant — a task that cannot read its inputs must not queue.
async fn enqueue_consolidation(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(payload): Json<ConsolidatePayload>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    if payload.memory_ids.is_empty() {
        return Err(ApiError::bad_request(
            "`memory_ids` must name at least one record — a summary that names no \
             sources is not a consolidation"
                .to_string(),
        ));
    }
    tasks::validate_label("distiller", &payload.distiller, 256).map_err(ApiError::bad_request)?;
    if let Some(key) = &payload.key {
        tasks::validate_label("key", key, 256).map_err(ApiError::bad_request)?;
    }
    // The shared gate, without the correction loop's run-scope exception:
    // consolidation produces an ordinary governed write, and run scope
    // stays runtime-only.
    check_memory_scope_gate(&state, &tenant, &payload.scope, false).await?;
    // Fail fast: every named record must resolve in this tenant and live
    // at the declared scope. A record forgotten between enqueue and
    // execution surfaces at claim time as an `invalid_input` failure.
    let mut sorted_ids = payload.memory_ids.clone();
    sorted_ids.sort();
    sorted_ids.dedup();
    for memory_id in &sorted_ids {
        let record = state
            .server_store
            .get_memory(tenant.tenant(), memory_id)
            .await
            .map_err(internal_err)?
            .ok_or_else(|| ApiError::not_found(format!("memory `{memory_id}` not found")))?;
        if record.scope != payload.scope {
            return Err(ApiError::bad_request(format!(
                "memory `{memory_id}` lives at `{}`, not the declared scope `{}` — a \
                 consolidation spans scopes never",
                record.scope.as_address(),
                payload.scope.as_address()
            )));
        }
    }
    // The idempotency key names the exact work — one scope, one sorted
    // source set — so retried submissions converge on the live task.
    let idempotency_key = format!(
        "memory_consolidation:{}:{}",
        payload.scope.as_address(),
        sha256_hex(sorted_ids.join(",").as_bytes())
    );
    let now = Utc::now();
    let record = build_task_record(
        EnqueueTaskPayload {
            kind: tasks::MEMORY_CONSOLIDATION_KIND.to_string(),
            payload: json!({
                "scope": payload.scope,
                "memory_ids": sorted_ids,
                "distiller": payload.distiller,
                "key": payload.key,
                "tags": payload.tags,
                "priority": payload.priority,
                "written_at": now,
                "run_id": payload.run_id,
                "parent": payload.parent,
            }),
            pool: payload.pool,
            max_attempts: None,
            idempotency_key: Some(idempotency_key),
            effect: None,
            run_id: payload.run_id,
            thread_id: None,
            deadline: None,
            worker_version: None,
            recipient: None,
            parent: payload.parent,
        },
        &tenant,
    )?;
    enforce_task_quota(&state, &tenant, 1).await?;
    let (task, deduplicated) = state
        .server_store
        .enqueue_task(&record)
        .await
        .map_err(internal_err)?;
    let status = if deduplicated {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    Ok((
        status,
        Json(json!({
            "task_id": task.task_id,
            "deduplicated": deduplicated,
            "kind": tasks::MEMORY_CONSOLIDATION_KIND,
        })),
    ))
}

#[derive(Debug, Default, Deserialize)]
struct ConflictsPayload {
    /// Restrict the review listing to one scope address.
    #[serde(default)]
    scope: Option<ScopeAddress>,
}

/// `POST /memory/conflicts` — the conflict review listing: live records
/// sharing a key with overlapping validity windows and contradictory
/// content, flagged. Detection is evidence and resolution is governance
/// (the design's rule; open question 5's distiller half): this endpoint
/// changes nothing, and nothing anywhere in the runtime resolves the
/// pairs it returns.
async fn list_memory_conflicts(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(payload): Json<ConflictsPayload>,
) -> Result<Json<Value>, ApiError> {
    let universe = memory_universe(&state, &tenant).await?;
    let mut conflicts = detect_conflicts(&universe, Utc::now());
    if let Some(scope) = &payload.scope {
        conflicts.retain(|conflict| &conflict.scope == scope);
    }
    Ok(Json(json!({ "conflicts": conflicts })))
}

#[derive(Debug, Deserialize)]
struct ForgetPayload {
    /// The record to erase (bare content address).
    memory_id: String,
    /// Why it is forgotten — carried on the tombstone.
    reason: ForgetReason,
    /// Journal the tombstone into this run's journal (best-effort, the
    /// wave-1 discipline): the deletion is durable either way; the
    /// journaled tombstone is the auditable receipt.
    #[serde(default)]
    run_id: Option<String>,
    /// The causal parent journal-event id (default: the journal's head).
    #[serde(default)]
    parent: Option<String>,
}

/// The tenant's whole memory namespace — expired and superseded records
/// included: the universe forget planning and conflict detection run
/// over. Both walk relationships (source naming, supersession), and a
/// relationship does not stop existing because a record aged out of
/// default retrieval.
async fn memory_universe(
    state: &AppState,
    tenant: &TenantContext,
) -> Result<Vec<MemoryRecord>, ApiError> {
    state
        .server_store
        .query_memory(
            tenant.tenant(),
            &MemoryQuery {
                include_expired: true,
                include_superseded: true,
                ..MemoryQuery::default()
            },
            Utc::now(),
        )
        .await
        .map_err(internal_err)
}

/// `POST /memory/forget` — erase one record → `200 {forgotten,
/// invalidated, tombstone}`. Real deletion from the store (derived state
/// is erasable; run journals are hash-chained evidence and are not —
/// open question 4), invalidation of the dependent summaries by walking
/// the source naming in reverse, transitively (they are deleted with it:
/// a summary built on erased evidence that keeps serving content
/// distilled from the forgotten record is not forgetting), and a
/// journaled `memory_forget` tombstone carrying metadata only — the id,
/// scope, reason, and dependent invalidations, never the forgotten
/// content: the tombstone struct has no content field to leak through.
/// `404` for unknown or cross-tenant addresses.
async fn forget_memory(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(payload): Json<ForgetPayload>,
) -> Result<Json<Value>, ApiError> {
    let record = state
        .server_store
        .get_memory(tenant.tenant(), &payload.memory_id)
        .await
        .map_err(internal_err)?
        .ok_or_else(|| ApiError::not_found(format!("memory `{}` not found", payload.memory_id)))?;
    let universe = memory_universe(&state, &tenant).await?;
    let plan = plan_forget(&universe, std::slice::from_ref(&payload.memory_id));
    for memory_id in plan.forgotten.iter().chain(plan.invalidated.iter()) {
        state
            .server_store
            .delete_memory(tenant.tenant(), memory_id)
            .await
            .map_err(internal_err)?;
    }
    let tombstone = MemoryForgetTombstone {
        memory_id: payload.memory_id.clone(),
        scope: record.scope.clone(),
        reason: payload.reason,
        invalidated: plan.invalidated,
    };
    if let Some(run_id) = &payload.run_id {
        journal_memory_forget(&state, &tenant, run_id, &tombstone, payload.parent).await;
    }
    Ok(Json(json!({
        "forgotten": plan.forgotten,
        "invalidated": tombstone.invalidated,
        "tombstone": tombstone,
    })))
}

#[derive(Debug, Deserialize)]
struct ForgetScopePayload {
    /// The scope address to erase wholesale (erasure requests).
    scope: ScopeAddress,
    /// Why the scope is forgotten — carried on every tombstone.
    reason: ForgetReason,
    /// Journal the tombstones into this run's journal (best-effort; one
    /// `memory_forget` event per forgotten record — the tombstone
    /// contract names a single id).
    #[serde(default)]
    run_id: Option<String>,
    /// The causal parent journal-event id (default: the journal's head).
    #[serde(default)]
    parent: Option<String>,
}

/// `POST /memory/forget_scope` — erase every record at a scope address
/// (erasure requests) → `200 {forgotten, invalidated, tombstones}`. Same
/// semantics as [`forget_memory`], scaled to the scope: each forgotten
/// record gets its own tombstone carrying the dependents attributable to
/// its own erasure, and summaries anywhere in the namespace that named a
/// forgotten record are invalidated with it.
///
/// Idempotent by construction: an empty scope answers `200` with empty
/// lists. Tenant scope requires the caller's own tenant (`403`); the
/// agent-manifest check deliberately does not apply — an erasure request
/// must not depend on the agent still being registered.
async fn forget_memory_scope(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(payload): Json<ForgetScopePayload>,
) -> Result<Json<Value>, ApiError> {
    tasks::validate_label("scope.id", &payload.scope.id, 256).map_err(ApiError::bad_request)?;
    if payload.scope.scope == MemoryScope::Tenant && payload.scope.id != tenant.tenant() {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            format!(
                "tenant-scoped erasure id `{}` is not the caller's tenant `{}` — \
                 tenant isolation is not a scope a caller can cross",
                payload.scope.id,
                tenant.tenant()
            ),
        ));
    }
    let universe = memory_universe(&state, &tenant).await?;
    let targets: Vec<String> = universe
        .iter()
        .filter(|record| record.scope == payload.scope)
        .map(|record| record.memory_id.clone())
        .collect();
    if targets.is_empty() {
        return Ok(Json(json!({
            "forgotten": [],
            "invalidated": [],
            "tombstones": 0,
        })));
    }
    let scopes: HashMap<&str, ScopeAddress> = universe
        .iter()
        .map(|record| (record.memory_id.as_str(), record.scope.clone()))
        .collect();
    let plan = plan_forget(&universe, &targets);
    for memory_id in plan.forgotten.iter().chain(plan.invalidated.iter()) {
        state
            .server_store
            .delete_memory(tenant.tenant(), memory_id)
            .await
            .map_err(internal_err)?;
    }
    // One tombstone per forgotten record, each naming the dependents
    // attributable to its own erasure (the single-target plan against
    // the pre-deletion universe).
    let mut tombstones = Vec::with_capacity(plan.forgotten.len());
    for memory_id in &plan.forgotten {
        let single = plan_forget(&universe, std::slice::from_ref(memory_id));
        let scope = scopes
            .get(memory_id.as_str())
            .expect("forgotten ids come from the universe")
            .clone();
        tombstones.push(MemoryForgetTombstone {
            memory_id: memory_id.clone(),
            scope,
            reason: payload.reason,
            invalidated: single.invalidated,
        });
    }
    if let Some(run_id) = &payload.run_id {
        for tombstone in &tombstones {
            journal_memory_forget(&state, &tenant, run_id, tombstone, payload.parent.clone()).await;
        }
    }
    Ok(Json(json!({
        "forgotten": plan.forgotten,
        "invalidated": plan.invalidated,
        "tombstones": tombstones.len(),
    })))
}

/// Journal a forgetting tombstone into the given run's persisted journal
/// — best-effort, the [`journal_memory_write`] discipline: the deletion
/// is already durable in the store, so a journaling failure is logged,
/// never surfaced as a request failure. The event is an
/// [`Effect::Idempotent`] effect under the derived
/// `memory_forget:{scope}:{memory_id}` key (retried erasures converge);
/// the tombstone is metadata-only by construction — no content field
/// exists to serialize.
async fn journal_memory_forget(
    state: &AppState,
    tenant: &TenantContext,
    run_id: &str,
    tombstone: &MemoryForgetTombstone,
    parent: Option<String>,
) {
    let draft = EventDraft::new(RunEventKind::MemoryForget, Effect::Idempotent).input(json!({
        "effect_key": memory_forget_effect_key(&tombstone.scope, &tombstone.memory_id),
        "memory_id": tombstone.memory_id,
    }));
    let draft = match serde_json::to_value(tombstone) {
        Ok(output) => draft.output(output),
        Err(error) => {
            tracing::warn!(%run_id, %error, "memory tombstone failed to serialize; journaling skipped");
            return;
        }
    };
    if let Err(error) = try_journal_memory_event(state, tenant, run_id, parent, draft).await {
        tracing::warn!(
            %run_id,
            memory_id = %tombstone.memory_id,
            %error,
            "forgetting is durable in the store; tombstone journaling skipped"
        );
    }
}

// --------------------------------------------------------------------- //
// The candidate lifecycle and promotion gate (R0.8 Rusty Learn, wave 3)
//
// Candidates are content-addressed, immutable proposals; the lifecycle —
// created → evaluated → promoted → rolled back — is four journaled
// transitions over the store, never background daemons. Two disciplines
// distinguish this surface from the memory routes:
//
// - **Journaling is hard-fail, not best-effort.** Every transition is in
//   the journal (the wave's exit criterion): `run_id` is required on
//   every lifecycle payload, and a run that cannot take the event stops
//   the request (`404` when the run does not resolve in this tenant —
//   the linkage the caller named does not exist; `422` otherwise).
//   Nothing reaches the store that the journal did not record first.
// - **The gate runs at promotion, in the handler.** `admit_promotion`
//   evaluates the deployment's declared envelope against the journaled
//   evaluation; out-of-envelope promotion needs an approval token
//   scoped to the candidate's promotion effect id. Refusal is a typed
//   `PromotionRefusal` mapped to `403`/`422` — never a silent no-op.
// --------------------------------------------------------------------- //

/// The wire name of a lifecycle status, so error messages read like the
/// API's JSON.
fn status_wire(status: CandidateStatus) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("{status:?}"))
}

/// Map a gate refusal to its HTTP status: approval failures are `403`
/// (the caller holds neither the standing nor the presented approval),
/// evidence failures are `422` (the request is well-formed; the
/// evidence does not clear the bar).
fn refusal_error(refusal: &PromotionRefusal) -> ApiError {
    match refusal {
        PromotionRefusal::RequiresApproval { .. } | PromotionRefusal::ApprovalMismatch { .. } => {
            ApiError::new(StatusCode::FORBIDDEN, "forbidden", refusal.to_string())
        }
        _ => ApiError::unprocessable(refusal.to_string()),
    }
}

/// Map a lifecycle error: a refused promotion through [`refusal_error`],
/// a state-machine violation to `409` (a concurrent transition, or an
/// action out of order — retry reads the settled state), everything
/// else (address and receipt mismatches) to `422`.
fn learn_error(error: &LearnError) -> ApiError {
    match error {
        LearnError::InvalidTransition { .. } => ApiError::conflict(error.to_string()),
        LearnError::Refused(refusal) => refusal_error(refusal),
        _ => ApiError::unprocessable(error.to_string()),
    }
}

/// The learn lifecycle's journaling gate (hard-fail — see the section
/// header): an unresolvable run is a `404`, any other append failure a
/// `422`.
fn journal_gate_error(error: String) -> ApiError {
    if error.contains("no persisted journal") || error.contains("does not resolve") {
        ApiError::not_found(error)
    } else {
        ApiError::unprocessable(error)
    }
}

/// Map the store's transition outcome to the route's statuses, with the
/// settled record on `Applied`.
fn transition_outcome(outcome: CandidateTransition, candidate_id: &str) -> Result<(), ApiError> {
    match outcome {
        CandidateTransition::Applied => Ok(()),
        CandidateTransition::Unknown => Err(ApiError::not_found(format!(
            "candidate `{candidate_id}` not found"
        ))),
        CandidateTransition::Conflict(live) => Err(ApiError::conflict(format!(
            "candidate `{candidate_id}` is `{}` — a concurrent transition won the race; \
             retry against the settled state",
            status_wire(live)
        ))),
    }
}

#[derive(Debug, Deserialize)]
struct CreateCandidatePayload {
    /// The candidate. Its content address must verify (`422`) — the
    /// store holds only well-addressed candidates, so every served
    /// record re-derives its id.
    candidate: Candidate,
    /// The run whose journal the creation event joins. Required —
    /// every lifecycle transition is journaled (the wave's exit
    /// criterion), and creation is the first one.
    run_id: String,
    /// The causal parent journal-event id (default: the journal's
    /// current head, the receipt precedent).
    #[serde(default)]
    parent: Option<String>,
}

/// `POST /learn/candidates` — register a distilled candidate → `201
/// {candidate_id, created, record}`; `200` + `created: false` when the
/// candidate id is already stored (content addressing makes the create
/// converge — the `Effect::Idempotent` creation). The address is
/// verified on the way in (`422`): the store holds only well-addressed
/// candidates. The `candidate_created` event is journaled into
/// `run_id`'s journal before the store write — hard-fail (see the
/// section header).
async fn create_candidate(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(payload): Json<CreateCandidatePayload>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    payload
        .candidate
        .verify_address()
        .map_err(|e| learn_error(&e))?;
    let candidate_id = payload.candidate.candidate_id.to_string();
    // Retry convergence on the candidate id (the memory write's rule):
    // a re-posted create returns the stored record without re-journaling.
    if let Some(existing) = state
        .server_store
        .get_candidate(tenant.tenant(), &candidate_id)
        .await
        .map_err(internal_err)?
    {
        return Ok((
            StatusCode::OK,
            Json(json!({
                "candidate_id": candidate_id,
                "created": false,
                "record": existing,
            })),
        ));
    }
    let draft = EventDraft::new(RunEventKind::CandidateCreated, Effect::Idempotent)
        .input(json!({
            "effect_key": candidate_effect_key(&payload.candidate.candidate_id),
            "candidate_id": candidate_id,
        }))
        .output(
            serde_json::to_value(&payload.candidate)
                .map_err(|e| ApiError::internal(format!("serialize candidate: {e}")))?,
        );
    try_journal_memory_event(&state, &tenant, &payload.run_id, payload.parent, draft)
        .await
        .map_err(journal_gate_error)?;
    let record = CandidateRecord::new(payload.candidate);
    state
        .server_store
        .put_candidate(tenant.tenant(), &record)
        .await
        .map_err(internal_err)?;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "candidate_id": candidate_id,
            "created": true,
            "record": record,
        })),
    ))
}

/// `GET /learn/candidates` — the tenant's candidates, sorted by
/// candidate id for a deterministic listing.
async fn list_candidates(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
) -> Result<Json<Value>, ApiError> {
    let mut records = state
        .server_store
        .list_candidates(tenant.tenant())
        .await
        .map_err(internal_err)?;
    records.sort_by(|a, b| a.candidate.candidate_id.cmp(&b.candidate.candidate_id));
    Ok(Json(json!({ "candidates": records })))
}

/// `GET /learn/candidates/{candidate_id}` — fetch one candidate record
/// (`404` unknown/cross-tenant — the two are indistinguishable by
/// design).
async fn get_candidate(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(candidate_id): Path<String>,
) -> Result<Json<CandidateRecord>, ApiError> {
    state
        .server_store
        .get_candidate(tenant.tenant(), &candidate_id)
        .await
        .map_err(internal_err)?
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("candidate `{candidate_id}` not found")))
}

#[derive(Debug, Deserialize)]
struct EvaluateCandidatePayload {
    /// What to evaluate against: the dataset version, target metric,
    /// thresholds, and replay evidence.
    request: EvaluationRequest,
    /// The run whose journal the evaluation event joins (required — the
    /// evaluation is the evidence the gate reads; it must be journaled).
    run_id: String,
    /// The causal parent journal-event id (default: the journal's head).
    #[serde(default)]
    parent: Option<String>,
}

/// `POST /learn/candidates/{candidate_id}/evaluate` — drive the
/// configured [`CandidateEvaluator`](rusty_agent_runtime::learn::CandidateEvaluator)
/// over the candidate and record the evaluation → `200 {candidate_id,
/// status, evaluation}`; `404` unknown/cross-tenant; `409` when no
/// evaluator is configured (a deployment without one can hold and
/// inspect candidates but cannot produce evidence) or the lifecycle
/// forbids re-evaluation; `422` when the evaluation fails or violates
/// the seam contract (it must name this candidate and the request's
/// dataset version — mismatches the gate would refuse at promotion are
/// caught here, at the first transition they would poison).
async fn evaluate_candidate(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(candidate_id): Path<String>,
    Json(payload): Json<EvaluateCandidatePayload>,
) -> Result<Json<Value>, ApiError> {
    let mut record = state
        .server_store
        .get_candidate(tenant.tenant(), &candidate_id)
        .await
        .map_err(internal_err)?
        .ok_or_else(|| ApiError::not_found(format!("candidate `{candidate_id}` not found")))?;
    let expect = record.status;
    let Some(evaluator) = state.config.candidate_evaluator.clone() else {
        return Err(ApiError::conflict(
            "no candidate evaluator is configured on this server — promotion is gated on \
             evidence, and evidence requires an evaluator \
             (`ServerConfig::with_candidate_evaluator`)"
                .to_string(),
        ));
    };
    let evaluation = evaluator
        .evaluate(&record.candidate, &payload.request)
        .await
        .map_err(|e| ApiError::unprocessable(format!("candidate evaluation failed: {e}")))?;
    if evaluation.candidate_id != record.candidate.candidate_id
        || evaluation.dataset_version != payload.request.dataset_version
    {
        return Err(ApiError::unprocessable(
            "the evaluator returned an evaluation naming a different candidate or dataset \
             version — the CandidateEvaluator contract requires both to match the request"
                .to_string(),
        ));
    }
    record
        .apply_evaluation(evaluation.clone())
        .map_err(|e| learn_error(&e))?;
    let draft = EventDraft::new(RunEventKind::CandidateEvaluated, Effect::Idempotent)
        .input(json!({
            "effect_key": evaluation_effect_key(
                &record.candidate.candidate_id,
                &evaluation.dataset_version,
            ),
            "candidate_id": candidate_id,
        }))
        .output(
            serde_json::to_value(&evaluation)
                .map_err(|e| ApiError::internal(format!("serialize evaluation: {e}")))?,
        );
    try_journal_memory_event(&state, &tenant, &payload.run_id, payload.parent, draft)
        .await
        .map_err(journal_gate_error)?;
    transition_outcome(
        state
            .server_store
            .transition_candidate(tenant.tenant(), &candidate_id, expect, &record, None)
            .await
            .map_err(internal_err)?,
        &candidate_id,
    )?;
    Ok(Json(json!({
        "candidate_id": candidate_id,
        "status": status_wire(record.status),
        "evaluation": evaluation,
    })))
}

#[derive(Debug, Deserialize)]
struct PromoteCandidatePayload {
    /// The run whose journal the promotion event joins (required — the
    /// promotion receipt is the gate's positive decision, and it must
    /// be journaled).
    run_id: String,
    /// The approval token for out-of-envelope promotions, scoped to the
    /// candidate's promotion effect id (an approval for one candidate
    /// does not transfer to another). In-envelope promotions ignore it;
    /// approval-ruled promotions fail `403` without it.
    #[serde(default)]
    approval: Option<ApprovalToken>,
    /// The causal parent journal-event id (default: the journal's head).
    #[serde(default)]
    parent: Option<String>,
}

/// `POST /learn/candidates/{candidate_id}/promote` — run the promotion
/// gate and, on admission, move the surface's version pointer → `200
/// {candidate_id, status, receipt, pointer}`. The gate
/// (`admit_promotion`) reads the deployment's declared envelope against
/// the journaled evaluation: `403` on approval failures, `422` on
/// evidence failures, `409` when the candidate is not `evaluated`. The
/// status flip and the pointer move are one store transition (one
/// transaction on Postgres, one lock pair on the file backend).
async fn promote_candidate(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(candidate_id): Path<String>,
    Json(payload): Json<PromoteCandidatePayload>,
) -> Result<Json<Value>, ApiError> {
    let mut record = state
        .server_store
        .get_candidate(tenant.tenant(), &candidate_id)
        .await
        .map_err(internal_err)?
        .ok_or_else(|| ApiError::not_found(format!("candidate `{candidate_id}` not found")))?;
    let decision = admit_promotion(
        &state.config.promotion_envelope,
        &record.candidate,
        record.evaluation.as_ref(),
        payload.approval.as_ref(),
    )
    .map_err(|e| learn_error(&e))?;
    let surface = record.candidate.surface();
    let pointer = state
        .server_store
        .get_version_pointer(tenant.tenant(), surface.as_str())
        .await
        .map_err(internal_err)?
        .unwrap_or_else(|| VersionPointer::new(surface.clone()));
    let receipt = PromotionReceipt {
        candidate_id: record.candidate.candidate_id.clone(),
        surface: surface.clone(),
        previous: pointer.active.clone(),
        decision,
        promoted_at: Utc::now(),
    };
    record
        .apply_promotion(receipt.clone())
        .map_err(|e| learn_error(&e))?;
    let moved = pointer.promoted(&receipt);
    let draft = EventDraft::new(RunEventKind::CandidatePromoted, Effect::Idempotent)
        .input(json!({
            "effect_key": promotion_effect_key(&record.candidate.candidate_id),
            "candidate_id": candidate_id,
        }))
        .output(
            serde_json::to_value(&receipt)
                .map_err(|e| ApiError::internal(format!("serialize promotion receipt: {e}")))?,
        );
    try_journal_memory_event(&state, &tenant, &payload.run_id, payload.parent, draft)
        .await
        .map_err(journal_gate_error)?;
    transition_outcome(
        state
            .server_store
            .transition_candidate(
                tenant.tenant(),
                &candidate_id,
                CandidateStatus::Evaluated,
                &record,
                Some(&moved),
            )
            .await
            .map_err(internal_err)?,
        &candidate_id,
    )?;
    Ok(Json(json!({
        "candidate_id": candidate_id,
        "status": status_wire(record.status),
        "receipt": receipt,
        "pointer": moved,
    })))
}

#[derive(Debug, Deserialize)]
struct RollbackCandidatePayload {
    /// The run whose journal the rollback event joins (required).
    run_id: String,
    /// Why the rollback happened — the drift monitor's verdict, the
    /// operator's note. Journaled on the receipt.
    cause: String,
    /// The causal parent journal-event id (default: the journal's head).
    #[serde(default)]
    parent: Option<String>,
}

/// `POST /learn/candidates/{candidate_id}/rollback` — re-point the
/// surface to the version the promotion displaced → `200 {candidate_id,
/// status, receipt, pointer}`; `404` unknown/cross-tenant; `409` when
/// the candidate is not `promoted`, or the surface's pointer no longer
/// serves it (roll back what serves, not a superseded experiment).
/// Rollback is byte-exact: the pointer's `to` is the promotion's
/// recorded `previous`, and candidates are content-addressed — the
/// restored version is the version that served, not a reconstruction.
async fn rollback_candidate(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(candidate_id): Path<String>,
    Json(payload): Json<RollbackCandidatePayload>,
) -> Result<Json<Value>, ApiError> {
    let mut record = state
        .server_store
        .get_candidate(tenant.tenant(), &candidate_id)
        .await
        .map_err(internal_err)?
        .ok_or_else(|| ApiError::not_found(format!("candidate `{candidate_id}` not found")))?;
    if record.status != CandidateStatus::Promoted {
        return Err(ApiError::conflict(format!(
            "candidate `{candidate_id}` is `{}` — only a promoted candidate can roll back",
            status_wire(record.status)
        )));
    }
    let surface = record.candidate.surface();
    let pointer = state
        .server_store
        .get_version_pointer(tenant.tenant(), surface.as_str())
        .await
        .map_err(internal_err)?
        .ok_or_else(|| {
            ApiError::conflict(format!(
                "surface `{surface}` has no version pointer — the candidate is not serving"
            ))
        })?;
    let serves_active = pointer.active.as_ref() == Some(record.candidate_id());
    let serves_canary = pointer
        .canary
        .as_ref()
        .is_some_and(|binding| &binding.candidate_id == record.candidate_id());
    if !serves_active && !serves_canary {
        return Err(ApiError::conflict(format!(
            "candidate `{candidate_id}` is marked promoted but surface `{surface}` does not \
             serve it — the pointer moved on; roll back what serves"
        )));
    }
    // Re-point to the promotion's recorded `previous` (full-traffic
    // rollback) or clear the binding (canary rollback — the static or
    // active version keeps serving).
    let to = if serves_active {
        record
            .promotion
            .as_ref()
            .and_then(|receipt| receipt.previous.clone())
    } else {
        None
    };
    let receipt = RollbackReceipt {
        surface: surface.clone(),
        from: record.candidate.candidate_id.clone(),
        to,
        cause: payload.cause,
        rolled_back_at: Utc::now(),
    };
    record
        .apply_rollback(receipt.clone())
        .map_err(|e| learn_error(&e))?;
    let moved = pointer.rolled_back(&receipt);
    let draft = EventDraft::new(RunEventKind::CandidateRolledBack, Effect::Idempotent)
        .input(json!({
            "effect_key": rollback_effect_key(&surface, &record.candidate.candidate_id),
            "candidate_id": candidate_id,
        }))
        .output(
            serde_json::to_value(&receipt)
                .map_err(|e| ApiError::internal(format!("serialize rollback receipt: {e}")))?,
        );
    try_journal_memory_event(&state, &tenant, &payload.run_id, payload.parent, draft)
        .await
        .map_err(journal_gate_error)?;
    transition_outcome(
        state
            .server_store
            .transition_candidate(
                tenant.tenant(),
                &candidate_id,
                CandidateStatus::Promoted,
                &record,
                Some(&moved),
            )
            .await
            .map_err(internal_err)?,
        &candidate_id,
    )?;
    Ok(Json(json!({
        "candidate_id": candidate_id,
        "status": status_wire(record.status),
        "receipt": receipt,
        "pointer": moved,
    })))
}

/// `GET /learn/versions` — the tenant's version pointers, sorted by
/// surface for a deterministic listing.
async fn list_version_pointers(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
) -> Result<Json<Value>, ApiError> {
    let mut pointers = state
        .server_store
        .list_version_pointers(tenant.tenant())
        .await
        .map_err(internal_err)?;
    pointers.sort_by(|a, b| a.surface.as_str().cmp(b.surface.as_str()));
    Ok(Json(json!({ "versions": pointers })))
}

/// `POST /tasks/{id}/fail` — record a failed attempt → `200 {requeued,
/// next_attempt_at, dead}`. The decision is core's shared `classify_retry`
/// policy: a retryable failure with attempts left requeues with exponential
/// backoff + full jitter (cap 5 min, scheduled at `next_attempt_at`);
/// exhausting the attempt budget dead-letters; a non-retryable class — or
/// work not safe to re-drive (the worker's `retryable: false`, or a declared
/// non-repeatable `effect` on the task) — fails outright (terminal, *not*
/// dead-lettered: `requeued: false, dead: false, next_attempt_at: null`).
/// `400` for an `error_class` outside the shared taxonomy; `409` when the
/// lease is lost.
async fn fail_task(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(task_id): Path<String>,
    Json(payload): Json<FailTaskPayload>,
) -> Result<Json<Value>, ApiError> {
    tasks::validate_label("worker_id", &payload.worker_id, 256).map_err(ApiError::bad_request)?;
    let error_class =
        tasks::parse_error_class(&payload.error_class).map_err(ApiError::bad_request)?;
    tasks::validate_label("message", &payload.message, 4096).map_err(ApiError::bad_request)?;
    let outcome = state
        .server_store
        .fail_task(
            tenant.tenant(),
            &task_id,
            &payload.worker_id,
            tasks::FailureReport {
                error_class,
                message: payload.message,
                retryable: payload.retryable,
                cost: tasks::SettlementCost {
                    tokens: payload.tokens,
                    cost_usd: payload.cost_usd,
                },
            },
            Utc::now(),
        )
        .await
        .map_err(internal_err)?;
    let task = lease_outcome(outcome, &task_id, &payload.worker_id)?;
    // Supervision trigger (R0.7 wave 2): a failed mailbox turn is a
    // supervision signal — the declared policy decides restart vs
    // escalate, journaled. Cancellation-class failures are control flow
    // (the cancellation tree's business, not a crash), and failures on
    // ordinary pool tasks or unregistered recipients take no supervision
    // path. The settlement is already durable at this point; supervision
    // composes after it, never inside the lease guard.
    let mut escalation = None;
    if error_class != rusty_agent_runtime::durable::ErrorClass::Cancelled {
        if let Some(agent_external) = task
            .recipient
            .as_deref()
            .and_then(rusty_agent_runtime::agents::agent_id_from_recipient)
        {
            if let Some(agent) = state
                .server_store
                .get_agent(&tenant.scope(agent_external))
                .await
                .map_err(internal_err)?
            {
                let outcome = supervision::supervise(
                    &state.server_store,
                    &tenant,
                    agent_external,
                    agent,
                    supervision::Trigger::TurnFailed {
                        error_class,
                        message: task
                            .last_error
                            .clone()
                            .unwrap_or_else(|| "turn failed".to_string()),
                        task_id: task.task_id.clone(),
                    },
                    Utc::now(),
                )
                .await
                .map_err(internal_err)?;
                // The failure report's caller is the turn's holder; when
                // that report tipped the agent over its restart intensity,
                // the response carries where the escalation landed — the
                // same evidence the supervision journal holds.
                escalation = outcome.delivery.map(|delivery| match delivery {
                    supervision::EscalationDelivery::Mailbox {
                        task_id,
                        deduplicated,
                    } => json!({
                        "kind": "mailbox",
                        "task_id": task_id,
                        "deduplicated": deduplicated,
                    }),
                    supervision::EscalationDelivery::DeadLetter { task_id } => json!({
                        "kind": "dead_letter",
                        "task_id": task_id,
                    }),
                });
            }
        }
    }
    // Coordination trigger (R0.7 wave 3): a *terminally* settled member
    // task drives its pattern forward — a retry-scheduled failure is not a
    // settlement, so non-terminal failures take no coordination path. The
    // drive composes after supervision, after durability.
    if task.is_terminal() {
        coordination::on_task_settled(
            &state.server_store,
            state.config.quota_for(tenant.tenant()),
            &tenant,
            &task,
            Utc::now(),
        )
        .await
        .map_err(internal_err)?;
    }
    Ok(Json(json!({
        // A retry is outstanding exactly when a next attempt is scheduled;
        // a `failed` task with a null schedule failed outright.
        "requeued": task.status == TaskStatus::Failed && task.next_attempt_at.is_some(),
        "next_attempt_at": task.next_attempt_at,
        "dead": task.status == TaskStatus::Dead,
        "escalation": escalation,
    })))
}

/// `POST /tasks/{id}/cancel` — cancel a non-terminal task → `200` with the
/// updated record. Queued and retry-scheduled tasks move to the terminal
/// `cancelled` state immediately (never retried, never dead-lettered,
/// never re-queued); a leased task keeps its lease with
/// `cancel_requested` set, so the holder learns on its next heartbeat and
/// reports the attempt as `cancelled` through the fail path. Cancellation
/// is a hint for promptness — lease expiry stays the correctness
/// mechanism: a holder that never asks is finalized as cancelled by the
/// claim path once its lease lapses. `409` when the task is already
/// terminal, `404` for unknown or cross-tenant ids.
async fn cancel_task(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(task_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let outcome = state
        .server_store
        .cancel_task(tenant.tenant(), &task_id, Utc::now())
        .await
        .map_err(internal_err)?;
    match outcome {
        CancelOutcome::Applied(task) => {
            // Coordination trigger (R0.7 wave 3): an immediately-cancelled
            // member task is a terminal settlement — drive its pattern. A
            // signalled (leased) task is not settled yet; its holder's
            // report lands the drive later.
            if task.is_terminal() {
                coordination::on_task_settled(
                    &state.server_store,
                    state.config.quota_for(tenant.tenant()),
                    &tenant,
                    &task,
                    Utc::now(),
                )
                .await
                .map_err(internal_err)?;
            }
            Ok(Json(task.wire()))
        }
        CancelOutcome::Terminal(status) => Err(ApiError::conflict(format!(
            "task `{task_id}` is already terminal ({}) and cannot be cancelled",
            status.as_str()
        ))),
        CancelOutcome::Unknown => Err(ApiError::not_found(format!("task `{task_id}` not found"))),
    }
}

/// `GET /tasks/metrics` — the wave-3 autoscaling signals, tenant-scoped:
/// per-pool queue depth, live leases, lease saturation against the
/// configured concurrency limit, and the age of the oldest task a claim
/// would hand out right now. These are **signals, not a mechanism**: Rusty
/// publishes the numbers an external autoscaler (HPA, KEDA, a script)
/// scales worker deployments on; the scaling decision stays with the
/// operator — there is no built-in autoscaler, by design.
///
/// Shape: `{ "pools": [{ "pool", "queue_depth", "leased",
/// "concurrency_limit", "lease_saturation", "oldest_visible_task_age_ms"
/// }…], "now" }`. `concurrency_limit` / `lease_saturation` are null for
/// uncapped pools (saturation is undefined without a limit, never
/// invented); `oldest_visible_task_age_ms` is null when nothing is
/// visible. Saturation may exceed 1.0 transiently (claims racing the
/// Postgres commit window, or a limit lowered below the current load) —
/// see the `ServerStore::claim_task` contract. Pools with a configured
/// limit but no tasks report zeros: an autoscaler scaling to zero needs
/// the zero, not an absent entry.
async fn task_metrics(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
) -> Result<Json<Value>, ApiError> {
    let now = Utc::now();
    let stats = state
        .server_store
        .task_pool_stats(tenant.tenant(), now)
        .await
        .map_err(internal_err)?;
    let mut pools: Vec<Value> = stats
        .iter()
        .map(|stat| {
            let limit = state.config.task_pool_limits.get(&stat.pool).copied();
            pool_metrics_json(stat, limit, now)
        })
        .collect();
    for (pool, &limit) in &state.config.task_pool_limits {
        if !stats.iter().any(|s| &s.pool == pool) {
            pools.push(pool_metrics_json(
                &tasks::PoolStat {
                    pool: pool.clone(),
                    queue_depth: 0,
                    leased: 0,
                    oldest_visible_at: None,
                },
                Some(limit),
                now,
            ));
        }
    }
    pools.sort_by(|a, b| a["pool"].as_str().cmp(&b["pool"].as_str()));
    Ok(Json(json!({ "pools": pools, "now": now })))
}

/// One pool's entry in the `GET /tasks/metrics` body (see [`task_metrics`]
/// for the field semantics).
fn pool_metrics_json(
    stat: &tasks::PoolStat,
    limit: Option<usize>,
    now: chrono::DateTime<Utc>,
) -> Value {
    let oldest_age_ms = stat
        .oldest_visible_at
        .map(|at| (now - at).num_milliseconds().max(0));
    json!({
        "pool": stat.pool,
        "queue_depth": stat.queue_depth,
        "leased": stat.leased,
        "concurrency_limit": limit,
        // `limit.max(1)`: a zero cap (paused pool) would divide by zero;
        // its saturation is 0 while paused and empty.
        "lease_saturation": limit.map(|max| stat.leased as f64 / max.max(1) as f64),
        "oldest_visible_task_age_ms": oldest_age_ms,
    })
}

/// `GET /tasks/{id}` — the task record (tenant-scoped; unknown or
/// cross-tenant ids answer 404).
async fn get_task(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(task_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    state
        .server_store
        .get_task(tenant.tenant(), &task_id)
        .await
        .map_err(internal_err)?
        .map(|task| Json(task.wire()))
        .ok_or_else(|| ApiError::not_found(format!("task `{task_id}` not found")))
}

#[derive(Debug, Deserialize)]
struct ListTasksQuery {
    /// Filter to one lifecycle status; `status=dead` is the DLQ listing.
    #[serde(default)]
    status: Option<String>,
}

/// `GET /tasks?status=…` — the tenant's tasks, oldest first, optionally
/// filtered by status. An unknown status answers 400 rather than silently
/// returning everything.
async fn list_tasks(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Query(query): Query<ListTasksQuery>,
) -> Result<Json<Value>, ApiError> {
    let status = query
        .status
        .as_deref()
        .map(|s| {
            TaskStatus::parse(s).ok_or_else(|| {
                ApiError::bad_request(format!(
                    "unknown task status `{s}` (expected queued|leased|failed|completed|dead|cancelled)"
                ))
            })
        })
        .transpose()?;
    let tasks = state
        .server_store
        .list_tasks(tenant.tenant(), status)
        .await
        .map_err(internal_err)?;
    let wire: Vec<Value> = tasks.iter().map(TaskRecord::wire).collect();
    Ok(Json(json!(wire)))
}

// --------------------------------------------------------------------- //
// Agent Fabric (R0.7, wave 1): registry, activation, mailboxes
// --------------------------------------------------------------------- //
//
// The HTTP face of the agent fabric contracts landed in core (`rusty_agent_runtime::agents`).
// Wave 1 is single-activation only: one active host per agent, turn-serialized
// mailbox draining, no supervision tree and no coordination sessions yet
// (those are waves 2+ — see `docs/agent-fabric-design.md`).

#[derive(Debug, Deserialize)]
struct CreateAgentPayload {
    /// Client-chosen agent id (a UUID v4 is generated when omitted).
    #[serde(default)]
    agent_id: Option<String>,
    /// The agent's capability manifest — core's `CapabilityManifest` shape
    /// (`agent_kind`, `manifest_version`, `accepts`, optional `scopes` /
    /// `budget`). Unknown fields are tolerated (the manifest is
    /// forward-compatible across waves); missing required fields are a 400.
    manifest: Value,
    /// The team this agent belongs to (R0.7 wave 2): a declared label
    /// `POST /teams/{team_id}/cancel` addresses — see
    /// [`agents::AgentRecord::team_id`].
    #[serde(default)]
    team_id: Option<String>,
    #[serde(default)]
    metadata: Option<Value>,
}

/// The activation lease as the wire shows it: external (unscoped) agent id,
/// RFC 3339 timestamps — the same conventions as the task wire.
fn activation_wire(agent_id: &str, lease: &agents::ActivationLease) -> Value {
    json!({
        "agent_id": agent_id,
        "owner": lease.owner,
        "fencing": lease.fencing,
        "lease_expires_at": lease.expires_at,
        "acquired_at": lease.acquired_at,
    })
}

/// `POST /agents` — register an agent: `201` with the record, `409` when
/// the id is taken, `400` when the manifest does not parse as a
/// `CapabilityManifest` (the registration is the one place manifest shape
/// is enforced; wave 1 stores `accepts` contracts without validating
/// message payloads against their schemas — that is a later wave).
async fn create_agent(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(payload): Json<CreateAgentPayload>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let manifest: CapabilityManifest = serde_json::from_value(payload.manifest)
        .map_err(|e| ApiError::bad_request(format!("invalid `manifest`: {e}")))?;
    tasks::validate_label("agent_kind", &manifest.agent_kind, 256)
        .map_err(ApiError::bad_request)?;
    tasks::validate_label("manifest_version", &manifest.manifest_version, 256)
        .map_err(ApiError::bad_request)?;
    let agent_id = payload
        .agent_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    validate_client_id("agent_id", &agent_id)?;
    if let Some(team_id) = &payload.team_id {
        tasks::validate_pool(team_id)
            .map_err(|e| ApiError::bad_request(e.replace("`pool`", "`team_id`")))?;
    }

    // Persist under the tenant's internal id; the wire shows the external id.
    let record = AgentRecord {
        agent_id: tenant.scope(&agent_id),
        manifest,
        team_id: payload.team_id,
        metadata: payload.metadata.unwrap_or(Value::Null),
        created_at: Utc::now(),
        supervision: agents::AgentSupervision::default(),
    };
    let created = state
        .server_store
        .create_agent(&record)
        .await
        .map_err(internal_err)?;
    if !created {
        return Err(ApiError::conflict(format!(
            "agent `{agent_id}` already exists"
        )));
    }
    let mut wire = record;
    wire.agent_id = agent_id;
    Ok((StatusCode::CREATED, Json(wire.wire())))
}

/// `GET /agents` — the tenant's registered agents, oldest first.
async fn list_agents(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
) -> Result<Json<Value>, ApiError> {
    let records = state
        .server_store
        .list_agents()
        .await
        .map_err(internal_err)?;
    // Only this tenant's agents, reported with their external ids.
    let mut records: Vec<AgentRecord> = records
        .into_iter()
        .filter_map(|mut record| {
            let external = tenant.unscope(&record.agent_id)?.to_string();
            record.agent_id = external;
            Some(record)
        })
        .collect();
    records.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.agent_id.cmp(&b.agent_id))
    });
    let wire: Vec<Value> = records.iter().map(AgentRecord::wire).collect();
    Ok(Json(json!(wire)))
}

/// `GET /agents/{id}` — one registration (404 unknown/cross-tenant).
async fn get_agent(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(agent_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    state
        .server_store
        .get_agent(&tenant.scope(&agent_id))
        .await
        .map_err(internal_err)?
        .map(|mut record| {
            record.agent_id = agent_id.clone();
            Json(record.wire())
        })
        .ok_or_else(|| ApiError::not_found(format!("agent `{agent_id}` not found")))
}

#[derive(Debug, Deserialize)]
struct SendAgentMessagePayload {
    /// Message kind; the agent's manifest must declare it in `accepts`.
    kind: String,
    /// Message payload: any JSON value, stored verbatim. Wave 1 does not
    /// validate it against the contract's `schema` (later wave).
    payload: Value,
    /// Attempt ceiling before dead-lettering (default 3, max 100).
    #[serde(default)]
    max_attempts: Option<u32>,
    /// Dedup key, unique per tenant across live tasks: re-sending with the
    /// same key returns the existing message (`deduplicated: true`).
    #[serde(default)]
    idempotency_key: Option<String>,
    /// Declared effect classification of the work (`pure` / `read_only` /
    /// `idempotent` / `compensatable` / `non_idempotent`) — the retry
    /// policy's effect gate, exactly as for pool tasks.
    #[serde(default)]
    effect: Option<String>,
    /// Whole-message deadline (RFC 3339), across attempts.
    #[serde(default)]
    deadline: Option<String>,
}

/// `POST /agents/{id}/mailbox` — send a message into the agent's mailbox.
/// The message is a durable task addressed to the agent (`recipient` set),
/// so it inherits the queue's idempotency, retry, and deadline machinery;
/// `400` when the manifest does not declare the kind in `accepts`, `404`
/// for an unknown agent. Answers `201 {task_id, deduplicated: false}` /
/// `200 {…, deduplicated: true}`, and `429` over the tenant's task quota.
async fn send_agent_message(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(agent_id): Path<String>,
    Json(payload): Json<SendAgentMessagePayload>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let agent = state
        .server_store
        .get_agent(&tenant.scope(&agent_id))
        .await
        .map_err(internal_err)?
        .ok_or_else(|| ApiError::not_found(format!("agent `{agent_id}` not found")))?;
    tasks::validate_label("kind", &payload.kind, 256).map_err(ApiError::bad_request)?;
    if agent.manifest.accepts_kind(&payload.kind).is_none() {
        let declared: Vec<&str> = agent.manifest.accepts.keys().map(String::as_str).collect();
        return Err(ApiError::bad_request(format!(
            "agent `{agent_id}` does not accept kind `{}` (manifest declares: {})",
            payload.kind,
            declared.join(", ")
        )));
    }
    // The shared validation surface (`build_task_record`) keeps the mailbox
    // path from drifting from direct enqueue; pool and worker-version pins
    // are forced to their defaults — they do not apply to agent claims.
    let mut record = build_task_record(
        EnqueueTaskPayload {
            kind: payload.kind,
            payload: payload.payload,
            pool: None,
            max_attempts: payload.max_attempts,
            idempotency_key: payload.idempotency_key,
            effect: payload.effect,
            run_id: None,
            thread_id: None,
            deadline: payload.deadline,
            worker_version: None,
            recipient: Some(AgentId::new(agent_id.as_str()).mailbox_recipient()),
            parent: None,
        },
        &tenant,
    )?;
    // The agent-level whole-activity deadline (R0.7 wave 2) composes into
    // R0.6's task deadline — the earlier bound wins. Expiry is then
    // cancellation by clock through the ordinary claim-path finalization,
    // and the breach is a supervision signal (see `claim_agent_message`).
    if let Some(agent_deadline) = agent.manifest.budget.as_ref().and_then(|b| b.deadline) {
        record.deadline = Some(
            record
                .deadline
                .map_or(agent_deadline, |d| d.min(agent_deadline)),
        );
    }
    // Same quota gate as every other submission surface.
    enforce_task_quota(&state, &tenant, 1).await?;
    let (task, deduplicated) = state
        .server_store
        .enqueue_task(&record)
        .await
        .map_err(internal_err)?;
    let status = if deduplicated {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    Ok((
        status,
        Json(json!({
            "task_id": task.task_id,
            "deduplicated": deduplicated,
        })),
    ))
}

/// `GET /agents/{id}/status` — the agent's activation lease (or `null`)
/// plus mailbox gauges. `queued` counts messages waiting for a turn
/// (including failed ones awaiting their retry schedule and expired leases
/// back in visibility); `in_flight` counts the live-leased turn in progress
/// (never more than one — turn serialization); `dead` is the mailbox's DLQ
/// depth.
async fn get_agent_status(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(agent_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let scoped = tenant.scope(&agent_id);
    state
        .server_store
        .get_agent(&scoped)
        .await
        .map_err(internal_err)?
        .ok_or_else(|| ApiError::not_found(format!("agent `{agent_id}` not found")))?;
    let lease = state
        .server_store
        .get_activation(&scoped)
        .await
        .map_err(internal_err)?;
    let recipient = AgentId::new(agent_id.as_str()).mailbox_recipient();
    let tasks = state
        .server_store
        .list_tasks(tenant.tenant(), None)
        .await
        .map_err(internal_err)?;
    let now = Utc::now();
    let (mut queued, mut in_flight, mut dead) = (0u64, 0u64, 0u64);
    for task in tasks
        .iter()
        .filter(|t| t.recipient.as_deref() == Some(recipient.as_str()))
    {
        match task.status {
            // `Failed` is awaiting its backoff — still pending work. An
            // expired lease is visible again, so it counts as queued too.
            TaskStatus::Queued | TaskStatus::Failed => queued += 1,
            TaskStatus::Leased => {
                if task.lease.as_ref().is_some_and(|l| l.expires_at > now) {
                    in_flight += 1;
                } else {
                    queued += 1;
                }
            }
            TaskStatus::Dead => dead += 1,
            // Completed / cancelled messages are settled history.
            _ => {}
        }
    }
    Ok(Json(json!({
        "agent_id": agent_id,
        "activation": lease
            .as_ref()
            .map(|l| activation_wire(&agent_id, l))
            .unwrap_or(Value::Null),
        "mailbox": {
            "queued": queued,
            "in_flight": in_flight,
            "dead": dead,
        },
    })))
}

#[derive(Debug, Deserialize)]
struct ActivateAgentPayload {
    /// Stable worker identity claiming the activation.
    worker_id: String,
    /// Lease duration in milliseconds (100..=3_600_000).
    lease_ms: u64,
}

/// `POST /agents/{id}/activate` — claim the agent's single activation
/// lease: `200 {owner, fencing, lease_expires_at, …}` when claimed (a
/// fresh claim, or a steal of an expired lease with the fencing ordinal
/// bumped); `409` when another host holds a live lease — the body names
/// the current holder so the loser can back off until expiry.
async fn activate_agent(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(agent_id): Path<String>,
    Json(payload): Json<ActivateAgentPayload>,
) -> Result<Response, ApiError> {
    tasks::validate_label("worker_id", &payload.worker_id, 256).map_err(ApiError::bad_request)?;
    tasks::validate_lease_ms(payload.lease_ms).map_err(ApiError::bad_request)?;
    let scoped = tenant.scope(&agent_id);
    // Activation requires a registered agent: a lease for an id nobody
    // registered would strand mailbox traffic behind a phantom host.
    state
        .server_store
        .get_agent(&scoped)
        .await
        .map_err(internal_err)?
        .ok_or_else(|| ApiError::not_found(format!("agent `{agent_id}` not found")))?;
    let outcome = state
        .server_store
        .claim_activation(&scoped, &payload.worker_id, payload.lease_ms, Utc::now())
        .await
        .map_err(internal_err)?;
    Ok(match outcome {
        ActivationOutcome::Claimed(lease) => {
            Json(activation_wire(&agent_id, &lease)).into_response()
        }
        ActivationOutcome::Held(lease) => ApiError::conflict(format!(
            "agent `{agent_id}` activation is held by `{}` (fencing {}, lease expires {})",
            lease.owner,
            lease.fencing,
            lease.expires_at.to_rfc3339()
        ))
        .into_response(),
    })
}

#[derive(Debug, Deserialize)]
struct ActivationHeartbeatPayload {
    worker_id: String,
    /// The fencing ordinal the activate call granted — the stale-holder
    /// guard: a host that lost the activation to a steal can never renew.
    fencing: u64,
    lease_ms: u64,
}

/// `POST /agents/{id}/activate/heartbeat` — renew the held activation:
/// `200` with the refreshed lease, `409` when the owner + fencing pair no
/// longer holds it (stolen or expired — the host must re-activate), `404`
/// when no lease exists at all.
async fn heartbeat_activation(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(agent_id): Path<String>,
    Json(payload): Json<ActivationHeartbeatPayload>,
) -> Result<Response, ApiError> {
    tasks::validate_label("worker_id", &payload.worker_id, 256).map_err(ApiError::bad_request)?;
    tasks::validate_lease_ms(payload.lease_ms).map_err(ApiError::bad_request)?;
    let outcome = state
        .server_store
        .renew_activation(
            &tenant.scope(&agent_id),
            &payload.worker_id,
            payload.fencing,
            payload.lease_ms,
            Utc::now(),
        )
        .await
        .map_err(internal_err)?;
    Ok(match outcome {
        ActivationMutation::Applied(lease) => {
            Json(activation_wire(&agent_id, &lease)).into_response()
        }
        ActivationMutation::FencingLost => ApiError::conflict(format!(
            "activation for agent `{agent_id}` is no longer held by this owner + fencing pair"
        ))
        .into_response(),
        ActivationMutation::Unknown => {
            ApiError::not_found(format!("no activation lease for agent `{agent_id}`"))
                .into_response()
        }
    })
}

#[derive(Debug, Deserialize)]
struct ActivationReleasePayload {
    worker_id: String,
    fencing: u64,
}

/// `POST /agents/{id}/activate/release` — drop the held activation so a
/// draining host's replacement can activate promptly instead of waiting
/// out the expiry. Same owner + fencing guard as the heartbeat: `200
/// {released: true}`, `409` on fencing loss, `404` when no lease exists.
async fn release_activation(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(agent_id): Path<String>,
    Json(payload): Json<ActivationReleasePayload>,
) -> Result<Response, ApiError> {
    tasks::validate_label("worker_id", &payload.worker_id, 256).map_err(ApiError::bad_request)?;
    let outcome = state
        .server_store
        .release_activation(
            &tenant.scope(&agent_id),
            &payload.worker_id,
            payload.fencing,
            Utc::now(),
        )
        .await
        .map_err(internal_err)?;
    Ok(match outcome {
        ActivationMutation::Applied(_) => {
            Json(json!({ "released": true, "agent_id": agent_id })).into_response()
        }
        ActivationMutation::FencingLost => ApiError::conflict(format!(
            "activation for agent `{agent_id}` is no longer held by this owner + fencing pair"
        ))
        .into_response(),
        ActivationMutation::Unknown => {
            ApiError::not_found(format!("no activation lease for agent `{agent_id}`"))
                .into_response()
        }
    })
}

#[derive(Debug, Deserialize)]
struct ClaimAgentMessagePayload {
    worker_id: String,
    /// The activation fencing ordinal this claim runs under.
    fencing: u64,
    /// Task lease for the claimed turn, in milliseconds.
    lease_ms: u64,
}

/// `POST /agents/{id}/mailbox/next` — claim the oldest queued mailbox
/// message as one turn of work: `200 {task}` with a fresh task lease, `204`
/// when the mailbox is empty **or a turn is already in flight** (one
/// message at a time per agent is server-enforced), `409` when the caller
/// does not hold the activation lease. The claimed turn settles through the
/// unchanged `/tasks/{id}/heartbeat|complete|fail` protocol.
async fn claim_agent_message(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(agent_id): Path<String>,
    Json(payload): Json<ClaimAgentMessagePayload>,
) -> Result<Response, ApiError> {
    tasks::validate_label("worker_id", &payload.worker_id, 256).map_err(ApiError::bad_request)?;
    tasks::validate_lease_ms(payload.lease_ms).map_err(ApiError::bad_request)?;
    let scoped = tenant.scope(&agent_id);
    let agent = state
        .server_store
        .get_agent(&scoped)
        .await
        .map_err(internal_err)?
        .ok_or_else(|| ApiError::not_found(format!("agent `{agent_id}` not found")))?;
    let now = Utc::now();
    // Agent-level deadline (R0.7 wave 2): past the whole-activity bound,
    // the claim triggers the breach path once (latched) — outstanding
    // mailbox traffic is cancelled (children before parent), and the
    // declared policy decides restart vs escalate, journaled. The claim
    // itself then proceeds normally: the cancellation finalization makes
    // it answer empty.
    if !agent.supervision.deadline_breached
        && agent
            .manifest
            .budget
            .as_ref()
            .and_then(|b| b.deadline)
            .is_some_and(|deadline| deadline <= now)
    {
        supervision::on_deadline_breach(&state.server_store, &tenant, &agent_id, agent, now)
            .await
            .map_err(internal_err)?;
    }
    let recipient = AgentId::new(agent_id.as_str()).mailbox_recipient();
    let claimed = state
        .server_store
        .claim_agent_task(
            tenant.tenant(),
            &MailboxClaimScope {
                agent_id: &scoped,
                recipient: &recipient,
                owner: &payload.worker_id,
                fencing: payload.fencing,
            },
            payload.lease_ms,
            now,
        )
        .await
        .map_err(internal_err)?;
    Ok(match claimed {
        MailboxClaim::Claimed(task) => Json(json!({ "task": task.wire() })).into_response(),
        MailboxClaim::Empty => StatusCode::NO_CONTENT.into_response(),
        MailboxClaim::ActivationLost => ApiError::conflict(format!(
            "activation for agent `{agent_id}` is not held by this owner + fencing pair"
        ))
        .into_response(),
    })
}

// --------------------------------------------------------------------- //
// Supervision and the cancellation tree (R0.7 Agent Fabric, wave 2)
// --------------------------------------------------------------------- //

/// Cancel one agent: its outstanding mailbox traffic first, then its live
/// runs — the cancellation tree's per-member rule (children before
/// parent), shared by `POST /agents/{id}/cancel` and
/// `POST /teams/{team_id}/cancel` so a member's cancellation is
/// self-contained and the two endpoints can never drift apart.
///
/// Mailbox traffic goes through the R0.6 semantics, agent-id scoped:
/// queued and retry-scheduled messages go terminal-`cancelled`
/// immediately; a leased turn keeps its lease with `cancel_requested` set
/// (a hint for promptness — lease expiry stays the correctness mechanism).
/// Runs go through `RunConfig::cancellation`: the executor observes the
/// token at a super-step boundary, after the boundary checkpoint has
/// landed, ending terminal-`cancelled` and resumable by re-running the
/// thread. The exit is journaled as an `AgentExit` in the agent's
/// supervision journal — only when the cancellation actually touched
/// something.
async fn cancel_one_agent(
    state: &AppState,
    tenant: &TenantContext,
    agent_external: &str,
) -> Result<Value, ApiError> {
    let now = Utc::now();
    let recipient = AgentId::new(agent_external).mailbox_recipient();
    let outcome = state
        .server_store
        .cancel_agent_tasks(tenant.tenant(), &recipient, now)
        .await
        .map_err(internal_err)?;
    // The agent's thread convention, internally scoped — the manager keys
    // runs by internal thread id.
    let thread = tenant.scope(&AgentId::new(agent_external).thread_id());
    let runs = state.run_deps.manager.cancel_thread_runs(&thread).await;
    let ids =
        |tasks: Vec<TaskRecord>| -> Vec<String> { tasks.into_iter().map(|t| t.task_id).collect() };
    let cancelled = ids(outcome.cancelled);
    let signalled = ids(outcome.signalled);
    let mut exit_event = Value::Null;
    if !cancelled.is_empty() || !signalled.is_empty() || !runs.is_empty() {
        exit_event = json!(supervision::journal_agent_exit(
            &state.server_store,
            tenant,
            agent_external,
            "cancelled",
            json!({
                "cancelled_messages": cancelled,
                "signalled_messages": signalled,
                "signalled_runs": runs.signalled,
                "cancelled_runs": runs.cancelled,
            }),
        )
        .await
        .map_err(internal_err)?);
    }
    Ok(json!({
        "agent_id": agent_external,
        "cancelled": cancelled,
        "signalled": signalled,
        "runs": {
            "signalled": runs.signalled,
            "cancelled": runs.cancelled,
        },
        "exit_event": exit_event,
    }))
}

/// `POST /agents/{id}/cancel` — cancel one agent (R0.7 wave 2): its
/// outstanding mailbox traffic (agent-id-scoped `cancel_run_tasks`
/// composition) and its live runs (`RunConfig::cancellation`), journaled
/// as an `AgentExit`. Idempotent: a repeated cancel of a quiescent agent
/// answers `200` with empty lists and journals nothing. `404` for unknown
/// or cross-tenant ids.
async fn cancel_agent(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(agent_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    state
        .server_store
        .get_agent(&tenant.scope(&agent_id))
        .await
        .map_err(internal_err)?
        .ok_or_else(|| ApiError::not_found(format!("agent `{agent_id}` not found")))?;
    Ok(Json(cancel_one_agent(&state, &tenant, &agent_id).await?))
}

#[derive(Debug, Default, Deserialize)]
struct RestartAgentPayload {
    /// The operator's reason, recorded as the attempt's message.
    #[serde(default)]
    reason: Option<String>,
}

/// `POST /agents/{id}/restart` — the manual supervision action (R0.7 wave
/// 2): the operator's "I've fixed the child" reset. Records the restart
/// (journaled `SupervisionEvent`, ordinal from the attempt history), and
/// clears the escalation and deadline-breach latches so supervision
/// resumes. Works with or without a declared policy — the operator
/// outranks the declaration.
///
/// The restart itself — a new run on the agent's thread restoring the
/// latest checkpoint — is the agent host's integration point: the mailbox
/// is untouched, so the next claimed turn re-drives the thread from its
/// latest checkpoint (the W1b machinery, unmodified). This endpoint is
/// the server-side half: the journaled decision and the latch reset.
async fn restart_agent(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(agent_id): Path<String>,
    body: Option<Json<RestartAgentPayload>>,
) -> Result<Json<Value>, ApiError> {
    let agent = state
        .server_store
        .get_agent(&tenant.scope(&agent_id))
        .await
        .map_err(internal_err)?
        .ok_or_else(|| ApiError::not_found(format!("agent `{agent_id}` not found")))?;
    let reason = body
        .and_then(|Json(payload)| payload.reason)
        .unwrap_or_else(|| "manual restart".to_string());
    let outcome = supervision::supervise(
        &state.server_store,
        &tenant,
        &agent_id,
        agent,
        supervision::Trigger::ManualRestart { reason },
        Utc::now(),
    )
    .await
    .map_err(internal_err)?;
    let ordinal = match outcome.decision {
        supervision::Decision::Restart { ordinal } => ordinal,
        // `supervise` maps the manual trigger to a restart by construction.
        _ => unreachable!("a manual restart trigger always decides restart"),
    };
    Ok(Json(json!({
        "agent_id": agent_id,
        "restarted": true,
        "restart_ordinal": ordinal,
        "event": outcome.event_id,
    })))
}

/// `GET /agents/{id}/supervision` — the agent's supervision evidence
/// (R0.7 wave 2): the declared policy, the latches, the full attempt
/// history, and the journaled `SupervisionEvent` / `AgentExit` events of
/// the agent's supervision journal — integrity re-verified on read,
/// exactly like the Flight Recorder endpoints. `404` for unknown or
/// cross-tenant ids.
async fn get_agent_supervision(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(agent_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let agent = state
        .server_store
        .get_agent(&tenant.scope(&agent_id))
        .await
        .map_err(internal_err)?
        .ok_or_else(|| ApiError::not_found(format!("agent `{agent_id}` not found")))?;
    let events = supervision::supervision_events(&state.server_store, &tenant, &agent_id)
        .await
        .map_err(internal_err)?;
    Ok(Json(json!({
        "agent_id": agent_id,
        "policy": agent.manifest.supervision,
        "escalated": agent.supervision.escalated,
        "deadline_breached": agent.supervision.deadline_breached,
        "suppressed_failures": agent.supervision.suppressed_failures,
        "attempts": agent.supervision.attempts,
        "journal_run_id": supervision::supervision_journal_run_id(tenant.tenant(), &agent_id),
        "events": events,
    })))
}

/// `POST /teams/{team_id}/cancel` — cancel a whole team (R0.7 wave 2):
/// every registered agent carrying the `team_id` label is cancelled by
/// the per-member rule ([`cancel_one_agent`]) — each member's
/// cancellation is self-contained, so the order across members does not
/// matter. `404` when no agent in this tenant declares the team (an empty
/// team is indistinguishable from an unknown one, the cross-tenant rule
/// applied to the label).
async fn cancel_team(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(team_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    tasks::validate_pool(&team_id)
        .map_err(|e| ApiError::bad_request(e.replace("`pool`", "`team_id`")))?;
    let members: Vec<String> = state
        .server_store
        .list_agents()
        .await
        .map_err(internal_err)?
        .into_iter()
        .filter(|record| record.team_id.as_deref() == Some(team_id.as_str()))
        // Tenant isolation rides the id prefix, as on every agent read:
        // another tenant's same-labelled team resolves to nothing here.
        .filter_map(|record| tenant.unscope(&record.agent_id).map(str::to_owned))
        .collect();
    if members.is_empty() {
        return Err(ApiError::not_found(format!(
            "no agents registered for team `{team_id}`"
        )));
    }
    let mut cancelled = Vec::with_capacity(members.len());
    for agent_external in members {
        cancelled.push(cancel_one_agent(&state, &tenant, &agent_external).await?);
    }
    Ok(Json(json!({
        "team_id": team_id,
        "members": cancelled,
    })))
}

// --------------------------------------------------------------------- //
// Coordination patterns (R0.7 wave 3)
// --------------------------------------------------------------------- //

/// Member names the runtime reserves for its own derived task ids
/// (`{tenant}--{cid}--outcome`, `{tenant}--{cid}--race-dlq`): a member
/// carrying one would collide with the pattern's own outcome / DLQ tasks.
const RESERVED_MEMBER_NAMES: &[&str] = &["outcome", "race-dlq"];

#[derive(Debug, Deserialize)]
struct SubmitDelegatePayload {
    /// Caller-supplied id for convergent retries (minted when absent):
    /// re-submitting with the same id returns the existing pattern
    /// (`deduplicated: true`) instead of starting a second one.
    #[serde(default)]
    coordination_id: Option<String>,
    /// The delegating agent (external id). The outcome is delivered to its
    /// mailbox as a `coordination_result` message — its manifest must
    /// declare that kind, checked here (400), or the pattern it starts
    /// would be stranded. Absent = control-plane submission observed
    /// through `GET /coordination/{id}` alone.
    #[serde(default)]
    delegator: Option<String>,
    /// Causal parent event id, when the pattern is spawned by a journaled
    /// step (an outer pattern's event, a delegator's turn event).
    #[serde(default)]
    parent: Option<String>,
    /// The delegate contract.
    delegate: DelegateContract,
}

#[derive(Debug, Deserialize)]
struct SubmitFanOutPayload {
    #[serde(default)]
    coordination_id: Option<String>,
    #[serde(default)]
    delegator: Option<String>,
    #[serde(default)]
    parent: Option<String>,
    fan_out: FanOutContract,
}

#[derive(Debug, Deserialize)]
struct SubmitRacePayload {
    #[serde(default)]
    coordination_id: Option<String>,
    #[serde(default)]
    delegator: Option<String>,
    #[serde(default)]
    parent: Option<String>,
    race: RaceContract,
}

#[derive(Debug, Deserialize)]
struct SubmitQuorumPayload {
    #[serde(default)]
    coordination_id: Option<String>,
    #[serde(default)]
    delegator: Option<String>,
    #[serde(default)]
    parent: Option<String>,
    quorum: QuorumContract,
}

/// `POST /coordination/delegate` — submit a delegate pattern → `201
/// {coordination_id, start_event, submitted}`.
async fn submit_delegate(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(payload): Json<SubmitDelegatePayload>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    submit_coordination(
        &state,
        &tenant,
        payload.coordination_id,
        payload.delegator,
        payload.parent,
        CoordinationContract::Delegate(Box::new(payload.delegate)),
    )
    .await
}

/// `POST /coordination/fan_out` — submit a fan-out pattern → `201`.
async fn submit_fan_out(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(payload): Json<SubmitFanOutPayload>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    submit_coordination(
        &state,
        &tenant,
        payload.coordination_id,
        payload.delegator,
        payload.parent,
        CoordinationContract::FanOut(payload.fan_out),
    )
    .await
}

/// `POST /coordination/race` — submit a race → `201`; `400` when any
/// candidate's declared effect is not freely repeatable (the effect gate:
/// a race loser is cancel-signalled at an arbitrary point, so every
/// candidate must be safe to abandon).
async fn submit_race(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(payload): Json<SubmitRacePayload>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    submit_coordination(
        &state,
        &tenant,
        payload.coordination_id,
        payload.delegator,
        payload.parent,
        CoordinationContract::Race(payload.race),
    )
    .await
}

/// `POST /coordination/quorum` — submit a quorum → `201`; `400` for a
/// threshold outside `1..=members`, duplicate member names, or a custom
/// resolver (a pinned wire shape wave 3 does not honor).
async fn submit_quorum(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(payload): Json<SubmitQuorumPayload>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    submit_coordination(
        &state,
        &tenant,
        payload.coordination_id,
        payload.delegator,
        payload.parent,
        CoordinationContract::Quorum(payload.quorum),
    )
    .await
}

/// The shared submission pipeline for all four patterns: validate
/// everything against the registry **before any write**, create the
/// record, quota-gate, then run the first drive (which journals
/// `CoordinationStart` and submits the initial window). One surface, so
/// the patterns can never drift apart in what they accept — the
/// `build_task_record` discipline.
async fn submit_coordination(
    state: &Arc<AppState>,
    tenant: &TenantContext,
    coordination_id: Option<String>,
    delegator: Option<String>,
    parent: Option<String>,
    contract: CoordinationContract,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    // Structural validation first: the pattern's own rules (the race
    // effect gate, quorum bounds, the fan-out window) before any registry
    // lookup, and before any write.
    contract
        .validate()
        .map_err(|violation| ApiError::bad_request(violation.to_string()))?;

    let coordination_id = coordination_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    validate_client_id("coordination_id", &coordination_id)?;
    if let Some(parent) = &parent {
        tasks::validate_label("parent", parent, 512).map_err(ApiError::bad_request)?;
    }

    // Every member: the target agent must be registered at the exact
    // pinned manifest version and must accept the delegation's kind. An
    // exact-version pin is the agent-level form of R0.6's worker version
    // pinning — a redeploy never changes a pattern's semantics
    // mid-flight.
    for delegation in contract.members() {
        coordination::validate_member_label("member", &delegation.member)
            .map_err(ApiError::bad_request)?;
        if RESERVED_MEMBER_NAMES.contains(&delegation.member.as_str()) {
            return Err(ApiError::bad_request(format!(
                "member name `{}` is reserved (it would collide with the pattern's own derived tasks)",
                delegation.member
            )));
        }
        tasks::validate_label("agent_id", &delegation.agent_id, 256)
            .map_err(ApiError::bad_request)?;
        let agent = state
            .server_store
            .get_agent(&tenant.scope(&delegation.agent_id))
            .await
            .map_err(internal_err)?
            .ok_or_else(|| {
                ApiError::bad_request(format!(
                    "member `{}` target agent `{}` is not registered",
                    delegation.member, delegation.agent_id
                ))
            })?;
        if agent.manifest.manifest_version != delegation.manifest_version {
            return Err(ApiError::bad_request(format!(
                "member `{}` pins manifest version `{}` but agent `{}` is registered at `{}` — the pin must match exactly",
                delegation.member,
                delegation.manifest_version,
                delegation.agent_id,
                agent.manifest.manifest_version
            )));
        }
        if agent.manifest.accepts_kind(&delegation.kind).is_none() {
            let declared: Vec<&str> = agent.manifest.accepts.keys().map(String::as_str).collect();
            return Err(ApiError::bad_request(format!(
                "member `{}` kind `{}` is not accepted by agent `{}` (manifest declares: {})",
                delegation.member,
                delegation.kind,
                delegation.agent_id,
                declared.join(", ")
            )));
        }
        // The delegate's context grant may only narrow the target's
        // declared scopes — a delegation is never a privilege escalation.
        if let CoordinationContract::Delegate(delegate_contract) = &contract {
            if let Some(grant) = &delegate_contract.context {
                if !grant.narrows(&agent.manifest.scopes) {
                    return Err(ApiError::bad_request(format!(
                        "context grant widens agent `{}`'s declared scopes ({:?}) — grants may only narrow",
                        delegation.agent_id, agent.manifest.scopes
                    )));
                }
            }
        }
    }

    // The delegator must be able to receive the outcome — the reserved
    // kind check at the door (see `COORDINATION_RESULT_KIND`).
    if let Some(delegator) = &delegator {
        tasks::validate_label("delegator", delegator, 256).map_err(ApiError::bad_request)?;
        let agent = state
            .server_store
            .get_agent(&tenant.scope(delegator))
            .await
            .map_err(internal_err)?
            .ok_or_else(|| {
                ApiError::bad_request(format!("delegator agent `{delegator}` is not registered"))
            })?;
        if agent
            .manifest
            .accepts_kind(COORDINATION_RESULT_KIND)
            .is_none()
        {
            return Err(ApiError::bad_request(format!(
                "delegator `{delegator}` does not accept `{COORDINATION_RESULT_KIND}` (its manifest must declare the reserved kind — a delegator that cannot receive the outcome would strand every pattern it starts)"
            )));
        }
    }

    let now = Utc::now();
    let record = coordination::CoordinationRecord {
        coordination_id: tenant.scope(&coordination_id),
        delegator,
        parent,
        members: contract
            .members()
            .into_iter()
            .map(|delegation| coordination::MemberRecord {
                member: delegation.member.clone(),
                agent_id: delegation.agent_id.clone(),
                manifest_version: delegation.manifest_version.clone(),
                task_id: coordination::member_task_id(
                    tenant.tenant(),
                    &coordination_id,
                    &delegation.member,
                ),
                submitted: false,
            })
            .collect(),
        contract,
        settled: false,
        outcome: None,
        outcome_delivered: false,
        dlq_written: false,
        created_at: now,
        updated_at: now,
    };
    let created = state
        .server_store
        .create_coordination(&record)
        .await
        .map_err(internal_err)?;
    if !created {
        // Convergent retry of a caller-supplied id: the existing pattern
        // stands, the caller learns it was deduplicated — the enqueue
        // idempotency-key discipline applied to whole patterns.
        return Ok((
            StatusCode::OK,
            Json(json!({
                "coordination_id": coordination_id,
                "deduplicated": true,
            })),
        ));
    }

    // The submission quota applies to the pattern's initial window —
    // member work is real queue pressure from the first drive on. Later
    // windows are gated inside the drive itself.
    let initial_window = match &record.contract {
        CoordinationContract::FanOut(contract) => {
            (contract.max_in_flight as usize).min(contract.members.len())
        }
        _ => record.members.len(),
    };
    enforce_task_quota(state, tenant, initial_window).await?;

    let driven = coordination::drive(
        &state.server_store,
        state.config.quota_for(tenant.tenant()),
        tenant,
        record,
        now,
    )
    .await
    .map_err(internal_err)?;

    let start_event = coordination::load_journal(&state.server_store, tenant, &coordination_id)
        .await
        .map_err(internal_err)?
        .and_then(|journal| journal.events().first().map(|event| event.id.clone()));
    let submitted: Vec<Value> = driven
        .record
        .members
        .iter()
        .filter(|member| member.submitted)
        .map(|member| json!({"member": member.member, "task_id": member.task_id}))
        .collect();
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "coordination_id": coordination_id,
            "start_event": start_event,
            "submitted": submitted,
        })),
    ))
}

/// `GET /coordination/{coordination_id}` — the pattern's record, current
/// member dispositions, settled outcome (when done), and its journal
/// events (integrity-verified).
///
/// Deliberately impure: the read **drives** the pattern first
/// (reconcile-on-read). Claim-path finalizations — a member's deadline
/// expiring unclaimed, an unanswered cancel — have no route hook, so
/// without this drive a pattern whose member died silently would look
/// open forever. The drive is convergent: a read that changes nothing
/// writes nothing.
async fn get_coordination(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(coordination_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    validate_client_id("coordination_id", &coordination_id)?;
    let Some(record) = state
        .server_store
        .get_coordination(&tenant.scope(&coordination_id))
        .await
        .map_err(internal_err)?
    else {
        return Err(ApiError::not_found(format!(
            "coordination `{coordination_id}` not found"
        )));
    };
    let driven = coordination::drive(
        &state.server_store,
        state.config.quota_for(tenant.tenant()),
        &tenant,
        record,
        Utc::now(),
    )
    .await
    .map_err(internal_err)?;
    let journal = coordination::load_journal(&state.server_store, &tenant, &coordination_id)
        .await
        .map_err(internal_err)?;
    let members: Vec<Value> = driven
        .record
        .members
        .iter()
        .map(|member| {
            let disposition = driven
                .dispositions
                .iter()
                .find(|d| d.member == member.member);
            json!({
                "member": member.member,
                "agent_id": member.agent_id,
                "manifest_version": member.manifest_version,
                "task_id": member.task_id,
                "submitted": member.submitted,
                "disposition": disposition,
            })
        })
        .collect();
    let journal_wire = journal.map(|journal| {
        json!({
            "run_id": coordination::coordination_journal_run_id(tenant.tenant(), &coordination_id),
            "events": journal.events(),
        })
    });
    Ok(Json(json!({
        "coordination_id": coordination_id,
        "delegator": driven.record.delegator,
        "parent": driven.record.parent,
        "contract": driven.record.contract,
        "members": members,
        "settled": driven.record.settled,
        "outcome": driven.record.outcome,
        "journal": journal_wire,
        "created_at": driven.record.created_at,
        "updated_at": driven.record.updated_at,
    })))
}

/// `GET /coordination/{coordination_id}/trace` — the TeamTrace: one
/// connected causal tree across the pattern's journal and any member-task
/// run journals. Member *supervision* journals are deliberately excluded:
/// they carry no parent links into the pattern's tree, so including them
/// would manufacture detached roots and break the connectivity signal.
async fn get_coordination_trace(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(coordination_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    validate_client_id("coordination_id", &coordination_id)?;
    let Some(record) = state
        .server_store
        .get_coordination(&tenant.scope(&coordination_id))
        .await
        .map_err(internal_err)?
    else {
        return Err(ApiError::not_found(format!(
            "coordination `{coordination_id}` not found"
        )));
    };
    // Reconcile first (the get_coordination rationale) so the trace
    // reflects the latest evidence, then assemble from verified snapshots.
    let driven = coordination::drive(
        &state.server_store,
        state.config.quota_for(tenant.tenant()),
        &tenant,
        record,
        Utc::now(),
    )
    .await
    .map_err(internal_err)?;
    let mut snapshots = Vec::new();
    if let Some(journal) =
        coordination::load_journal(&state.server_store, &tenant, &coordination_id)
            .await
            .map_err(internal_err)?
    {
        snapshots.push(journal.snapshot());
    }
    for member in &driven.record.members {
        let Some(task) = state
            .server_store
            .get_task(tenant.tenant(), &member.task_id)
            .await
            .map_err(internal_err)?
        else {
            continue;
        };
        let Some(run_id) = &task.run_id else {
            continue;
        };
        if let Some(snapshot) = state
            .server_store
            .get_journal(run_id)
            .await
            .map_err(internal_err)?
        {
            snapshots.push(snapshot);
        }
    }
    let trace = TeamTrace::assemble(&snapshots);
    Ok(Json(json!({
        "coordination_id": coordination_id,
        "connected": trace.is_connected(),
        "trace": trace,
    })))
}
