//! Triggers: event-driven bindings from signed inbound webhooks to agent
//! actions.
//!
//! A trigger is the event-driven counterpart of a cron: where a cron fires
//! runs on a schedule, a trigger fires on an external event delivered to
//! `POST /triggers/{id}/webhook`. It binds a **target** (an assistant or a
//! thread), an **action** (`start_run` / `resume_thread` / `send_message`),
//! and an **input template** — a JSON value whose `{{event.*}}` placeholders
//! are filled from the event payload before the action executes.
//!
//! - **Signature.** Webhooks authenticate by HMAC-SHA256 over the raw body
//!   with the trigger's per-trigger secret, presented as
//!   `X-Rusty-Signature: sha256=<hex>` and verified in constant time. The
//!   webhook route is deliberately *not* behind the API-key middleware:
//!   external senders (GitHub, Stripe, …) cannot present an `X-Api-Key`,
//!   so the signature is the credential. Tenant resolution rides on the
//!   signature: among the triggers whose external id matches the path, the
//!   one whose secret verifies the body owns the event.
//! - **Event log + dead-letter.** Every received event is recorded (payload
//!   hash, payload, action, run id if any, status). Events whose action
//!   fails land in the dead-letter list (`GET /triggers/{id}/dead-letter`)
//!   and can be re-driven with
//!   `POST /triggers/{id}/events/{event_id}/replay`.
//! - **Debounce.** A trigger with `debounce_ms` coalesces a burst of events
//!   inside the window into one action whose `{{event.*}}` context is the
//!   array of payloads; each contributing event is marked `coalesced` with
//!   the shared run id. The buffer is in-memory: a restart mid-window leaves
//!   the burst's events `pending` (inspectable, replayable) — durability of
//!   the *effect* lives in the checkpoint log, like every other run.
//!
//! Records are persisted as one JSON file per trigger under
//! `{store_path}/triggers/{trigger_id}.json` and events under
//! `{store_path}/trigger_events/{trigger_id}/{event_id}.json` (the
//! `server_triggers` / `server_trigger_events` tables on Postgres), reloaded
//! when the router is built. Ids carry the `{tenant}/` prefix internally,
//! exactly like assistants and crons.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path as AxumPath, State as AxumState};
use axum::http::{HeaderMap, StatusCode};
use axum::{Extension, Json};
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::auth::{scope_id, strip_owned, tenant_of_internal, TenantContext};
use crate::error::ApiError;
use crate::routes::{internal_err, validate_client_id, AppState};
use crate::runs::{self, MultitaskStrategy, RunPayload};
use crate::server_store::ServerStore;
use crate::tasks;
use crate::threads::ThreadRecord;

/// Events retained per trigger (oldest pruned). The log is an inspection
/// surface, not an unbounded journal — a chatty webhook must not become a
/// disk-full outage. The Postgres backend enforces the same cap in SQL.
pub(crate) const MAX_EVENTS_PER_TRIGGER: usize = 256;

/// Payloads coalesced into one debounced action before the buffer flushes
/// immediately, window or not. Bounds memory per burst.
pub(crate) const MAX_DEBOUNCE_BATCH: usize = 64;

/// Upper bound for `debounce_ms` (five minutes). Longer windows pile up
/// sleeper tasks and blur what "burst" means.
pub(crate) const MAX_DEBOUNCE_MS: u64 = 300_000;

// --------------------------------------------------------------------- //
// Records
// --------------------------------------------------------------------- //

/// What a trigger fires on: an assistant (`start_run`) or a thread
// (`resume_thread` / `send_message`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum TriggerTarget {
    Assistant { id: String },
    Thread { id: String },
}

impl TriggerTarget {
    /// The external (unprefixed) target id.
    pub(crate) fn id(&self) -> &str {
        match self {
            TriggerTarget::Assistant { id } | TriggerTarget::Thread { id } => id,
        }
    }
}

/// What a valid event does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TriggerAction {
    /// Create a fresh thread on the target assistant's graph and schedule a
    /// background run with the rendered input (the cron firing's shape).
    StartRun,
    /// Resume the target thread's interrupted run with the rendered value
    /// as `command.resume`.
    ResumeThread,
    /// Schedule a run on the target thread whose input is the rendered
    /// template (typically a `messages` channel update the graph's reducers
    /// merge).
    SendMessage,
}

/// Outcome of one received event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TriggerEventStatus {
    /// Buffered inside a debounce window; the action has not run yet.
    Pending,
    /// The action executed; `run_id` is set.
    Executed,
    /// The action failed; the event is on the dead-letter list until
    /// replayed successfully (the original record keeps its failure —
    /// history, not a queue slot).
    Failed,
    /// Coalesced with sibling events into one debounced action; `run_id` is
    /// the shared run.
    Coalesced,
}

/// One trigger: a signed-webhook binding from events to an agent action.
/// `trigger_id` is the internal (tenant-scoped) id; the wire always shows
/// the external one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TriggerRecord {
    pub trigger_id: String,
    pub name: String,
    pub target: TriggerTarget,
    pub action: TriggerAction,
    /// JSON template rendered against the event payload; `{{event.*}}`
    /// placeholders are substituted (see [`render`]).
    pub input_template: Value,
    pub enabled: bool,
    /// HMAC secret for `X-Rusty-Signature` (server-generated when omitted
    /// at creation; returned on create/read so the operator can configure
    /// the sender).
    pub secret: String,
    /// Debounce window in milliseconds (absent = execute every event
    /// immediately).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub debounce_ms: Option<u64>,
    pub created_at: DateTime<Utc>,
    /// Events received (bookkeeping, best-effort like the cron counters).
    #[serde(default)]
    pub events_received: u64,
    /// Runs fired (a debounced burst counts once — one action, one run).
    #[serde(default)]
    pub runs_fired: u64,
}

/// One received event: the trigger's evidence log entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TriggerEventRecord {
    pub event_id: String,
    /// Internal (tenant-scoped) trigger id.
    pub trigger_id: String,
    /// SHA-256 hex of the raw request body (the signed bytes).
    pub payload_hash: String,
    /// The parsed event payload (replays re-execute from this).
    pub payload: Value,
    pub action: TriggerAction,
    pub status: TriggerEventStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Set on the *new* event a replay produces, pointing at the original.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replayed_from: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// `start_run` binds assistants; `resume_thread` / `send_message` bind
/// threads. Anything else is a create-time 400, not a dead-letter surprise.
pub(crate) fn validate_binding(
    action: TriggerAction,
    target: &TriggerTarget,
) -> Result<(), String> {
    match (action, target) {
        (TriggerAction::StartRun, TriggerTarget::Assistant { .. }) => Ok(()),
        (
            TriggerAction::ResumeThread | TriggerAction::SendMessage,
            TriggerTarget::Thread { .. },
        ) => Ok(()),
        (TriggerAction::StartRun, TriggerTarget::Thread { .. }) => {
            Err("`start_run` requires an assistant target".to_string())
        }
        (_, TriggerTarget::Assistant { .. }) => {
            Err("`resume_thread`/`send_message` require a thread target".to_string())
        }
    }
}

// --------------------------------------------------------------------- //
// Input-template rendering
// --------------------------------------------------------------------- //

/// Render a template against an event: every string containing
/// `{{ path }}` placeholders is substituted, arrays and objects recurse.
/// A string that *is* one placeholder takes the resolved value verbatim
/// (preserving its JSON type); placeholders embedded in longer strings
/// interpolate (strings raw, other JSON compact). Unresolvable paths render
/// as `null` (whole-string) or the empty string (embedded) — a typo'd path
/// must not 500 the webhook.
fn render(template: &Value, event: &Value) -> Value {
    match template {
        Value::String(s) => render_string(s, event),
        Value::Array(items) => Value::Array(items.iter().map(|v| render(v, event)).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), render(v, event)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Substitute the placeholders of one string (see [`render`]).
fn render_string(s: &str, event: &Value) -> Value {
    let trimmed = s.trim();
    if let Some(path) = whole_placeholder(trimmed) {
        return resolve_path(event, path).cloned().unwrap_or(Value::Null);
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find("}}") {
            Some(end) => {
                let path = after[..end].trim();
                match resolve_path(event, path) {
                    Some(Value::String(v)) => out.push_str(v),
                    Some(v) => out.push_str(&v.to_string()),
                    None => {}
                }
                rest = &after[end + 2..];
            }
            // Unclosed placeholder: literal text, not an error.
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    Value::String(out)
}

/// The path when `s` is exactly one `{{ path }}` placeholder, else `None`.
fn whole_placeholder(s: &str) -> Option<&str> {
    let inner = s.strip_prefix("{{")?;
    let inner = inner.strip_suffix("}}")?;
    let inner = inner.trim();
    // `{{a}}{{b}}` is two placeholders, not one.
    if inner.contains("{{") || inner.contains("}}") {
        return None;
    }
    Some(inner)
}

/// Resolve a dotted path (`event.user.id`) against the event. Numeric
/// segments index arrays. The root segment must be `event`.
fn resolve_path<'a>(event: &'a Value, path: &str) -> Option<&'a Value> {
    let mut segments = path.split('.');
    if segments.next()? != "event" {
        return None;
    }
    let mut current = event;
    for segment in segments {
        current = match current {
            Value::Object(map) => map.get(segment)?,
            Value::Array(items) => items.get(segment.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(current)
}

// --------------------------------------------------------------------- //
// Webhook signature (HMAC-SHA256)
// --------------------------------------------------------------------- //

type HmacSha256 = Hmac<Sha256>;

/// The hex SHA-256 of `bytes` (the event's payload hash).
fn sha256_hex(bytes: &[u8]) -> String {
    hex_encode(&Sha256::digest(bytes))
}

/// Verify `X-Rusty-Signature` (`sha256=<hex>`) against the raw body with
/// the trigger's secret. Comparison is constant-time (hmac's
/// `verify_slice`); a malformed header is simply invalid.
fn verify_signature(secret: &str, body: &[u8], header: &str) -> bool {
    let Some(hex) = header.strip_prefix("sha256=") else {
        return false;
    };
    let Some(provided) = hex_decode(hex) else {
        return false;
    };
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key size");
    mac.update(body);
    mac.verify_slice(&provided).is_ok()
}

/// A fresh per-trigger secret: 64 lowercase hex chars (256 bits of UUID
/// entropy). Server-generated secrets keep webhook setup to one API call.
fn generate_secret() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn hex_decode(hex: &str) -> Option<Vec<u8>> {
    let bytes = hex.as_bytes();
    if bytes.len() % 2 != 0 {
        return None;
    }
    let nibble = |b: u8| -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    };
    bytes
        .chunks_exact(2)
        .map(|pair| Some((nibble(pair[0])? << 4) | nibble(pair[1])?))
        .collect()
}

// --------------------------------------------------------------------- //
// Persistence helpers (JSON-file layout; Postgres lives in server_store)
// --------------------------------------------------------------------- //

/// The on-disk directory holding one JSON file per trigger.
pub(crate) fn dir(store_root: &Path) -> PathBuf {
    store_root.join("triggers")
}

/// The on-disk directory holding each trigger's event log
/// (`trigger_events/{trigger_id}/{event_id}.json`).
pub(crate) fn events_dir(store_root: &Path) -> PathBuf {
    store_root.join("trigger_events")
}

/// The file path of one event record.
pub(crate) fn event_path(store_root: &Path, record: &TriggerEventRecord) -> PathBuf {
    events_dir(store_root)
        .join(&record.trigger_id)
        .join(format!("{}.json", record.event_id))
}

/// Load all persisted triggers, skipping (with a warning) any file that
/// fails to parse. Tenant-scoped records live one directory deeper
/// (`triggers/{tenant}/{trigger_id}.json`), so the walk is recursive.
pub(crate) fn load(store_root: &Path) -> HashMap<String, TriggerRecord> {
    let mut out = HashMap::new();
    let mut files = Vec::new();
    collect_json_files(&dir(store_root), &mut files);
    for path in files {
        let parsed = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<TriggerRecord>(&raw).ok());
        match parsed {
            Some(record) => {
                out.insert(record.trigger_id.clone(), record);
            }
            None => {
                tracing::warn!(path = %path.display(), "skipping unreadable trigger file")
            }
        }
    }
    out
}

/// Load all persisted events, keyed by (internal) trigger id, each list
/// sorted oldest-first by `(created_at, event_id)`.
pub(crate) fn load_events(store_root: &Path) -> HashMap<String, Vec<TriggerEventRecord>> {
    let mut out: HashMap<String, Vec<TriggerEventRecord>> = HashMap::new();
    let mut files = Vec::new();
    collect_json_files(&events_dir(store_root), &mut files);
    for path in files {
        let parsed = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<TriggerEventRecord>(&raw).ok());
        match parsed {
            Some(record) => out
                .entry(record.trigger_id.clone())
                .or_default()
                .push(record),
            None => {
                tracing::warn!(path = %path.display(), "skipping unreadable trigger event file")
            }
        }
    }
    for events in out.values_mut() {
        events.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.event_id.cmp(&b.event_id))
        });
    }
    out
}

/// Recursively collect `*.json` files under `root` (tenant subdirectories
/// hold that tenant's records).
fn collect_json_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_json_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("json") {
            out.push(path);
        }
    }
}

/// Persist one trigger record (create or overwrite). The id may carry a
/// `{tenant}/` prefix, so the parent directory is created, not just the
/// flat triggers dir.
pub(crate) async fn persist(store_root: &Path, record: &TriggerRecord) -> std::io::Result<()> {
    let path = dir(store_root).join(format!("{}.json", record.trigger_id));
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let raw = serde_json::to_vec_pretty(record).expect("trigger serialization is infallible");
    tokio::fs::write(path, raw).await
}

/// Persist one event record (create or overwrite for status transitions).
pub(crate) async fn persist_event(
    store_root: &Path,
    record: &TriggerEventRecord,
) -> std::io::Result<()> {
    let path = event_path(store_root, record);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let raw = serde_json::to_vec_pretty(record).expect("event serialization is infallible");
    tokio::fs::write(path, raw).await
}

// --------------------------------------------------------------------- //
// Action execution
// --------------------------------------------------------------------- //

/// Execute a trigger's bound action against `event` (a single payload, or
/// the array of payloads of a debounced burst). Returns the scheduled run
/// id; every failure is a `String` for the dead-letter record. Reuses the
/// run machinery directly (`runs::schedule`) — no HTTP hop.
async fn execute_action(
    state: &Arc<AppState>,
    trigger: &TriggerRecord,
    event: &Value,
    source_event_ids: &[String],
) -> Result<String, String> {
    let tenant = tenant_of_internal(&trigger.trigger_id);
    let external_trigger_id =
        strip_owned(tenant, &trigger.trigger_id).unwrap_or(&trigger.trigger_id);
    let rendered = render(&trigger.input_template, event);
    let run_metadata = json!({
        "trigger_id": external_trigger_id,
        "trigger": "webhook",
        "event_ids": source_event_ids,
    });

    match (&trigger.action, &trigger.target) {
        (TriggerAction::StartRun, TriggerTarget::Assistant { id }) => {
            let assistant = state
                .server_store
                .get_assistant(&scope_id(tenant, id))
                .await
                .map_err(|e| format!("load assistant: {e}"))?
                .ok_or_else(|| format!("assistant `{id}` not found"))?;
            // Same fresh-thread-per-firing shape as the cron scheduler.
            let thread_id = uuid::Uuid::new_v4().to_string();
            let internal_thread_id = scope_id(tenant, &thread_id);
            let record = ThreadRecord {
                thread_id: thread_id.clone(),
                tenant: tenant.to_string(),
                graph: assistant.graph.clone(),
                metadata: json!({"trigger_id": external_trigger_id, "trigger": "webhook"}),
                forked_from: None,
                seed_length: None,
                created_at: Utc::now(),
            };
            state
                .server_store
                .create_thread(&internal_thread_id, &record)
                .await
                .map_err(|e| format!("persist thread: {e}"))?;
            let payload = RunPayload {
                input: Some(require_object(rendered, "input")?),
                metadata: Some(run_metadata),
                assistant_id: Some(id.clone()),
                ..RunPayload::default()
            };
            schedule_run(
                state,
                &internal_thread_id,
                &thread_id,
                &assistant.graph,
                payload,
            )
            .await
        }
        (TriggerAction::ResumeThread, TriggerTarget::Thread { id }) => {
            let (internal_thread_id, record) = load_thread(state, tenant, id).await?;
            let payload = RunPayload {
                command: Some(runs::CommandPayload {
                    resume: Some(rendered),
                }),
                metadata: Some(run_metadata),
                ..RunPayload::default()
            };
            schedule_run(state, &internal_thread_id, id, &record.graph, payload).await
        }
        (TriggerAction::SendMessage, TriggerTarget::Thread { id }) => {
            let (internal_thread_id, record) = load_thread(state, tenant, id).await?;
            let payload = RunPayload {
                input: Some(require_object(rendered, "input")?),
                metadata: Some(run_metadata),
                ..RunPayload::default()
            };
            schedule_run(state, &internal_thread_id, id, &record.graph, payload).await
        }
        // Combinations the create/update validation rejects; persisted
        // records pre-dating the validation fail the event honestly.
        _ => Err(format!(
            "action `{}` does not match the trigger's target",
            serde_json::to_value(trigger.action)
                .expect("action serialization is infallible")
                .as_str()
                .expect("action serializes to a string")
        )),
    }
}

/// The rendered run/message input must be a JSON object (state channels are
/// keyed); anything else is a template bug the dead letter should show.
fn require_object(rendered: Value, what: &str) -> Result<Value, String> {
    if rendered.is_object() {
        Ok(rendered)
    } else {
        Err(format!(
            "rendered {what} must be a JSON object (got {})",
            match &rendered {
                Value::Null => "null",
                Value::Bool(_) => "a boolean",
                Value::Number(_) => "a number",
                Value::String(_) => "a string",
                Value::Array(_) => "an array",
                Value::Object(_) => unreachable!(),
            }
        ))
    }
}

/// Resolve the target thread inside its tenant's namespace.
async fn load_thread(
    state: &Arc<AppState>,
    tenant: &str,
    thread_id: &str,
) -> Result<(String, ThreadRecord), String> {
    let internal_thread_id = scope_id(tenant, thread_id);
    let record = state
        .server_store
        .get_thread(&internal_thread_id)
        .await
        .map_err(|e| format!("load thread: {e}"))?
        .ok_or_else(|| format!("thread `{thread_id}` not found"))?;
    Ok((internal_thread_id, record))
}

/// Schedule the action's run through the shared run machinery.
async fn schedule_run(
    state: &Arc<AppState>,
    internal_thread_id: &str,
    wire_thread_id: &str,
    graph: &str,
    payload: RunPayload,
) -> Result<String, String> {
    let scheduled = runs::schedule(
        &state.run_deps,
        internal_thread_id,
        wire_thread_id,
        graph,
        payload,
        // Enqueue, not reject: a burst of events on one thread serializes
        // instead of dead-lettering every event but the first.
        MultitaskStrategy::Enqueue,
    )
    .await
    .map_err(|e| e.to_string())?;
    Ok(scheduled.run_id)
}

// --------------------------------------------------------------------- //
// Debounce
// --------------------------------------------------------------------- //

/// One trigger's pending burst (in-memory; see the module docs for the
/// restart semantics).
#[derive(Debug, Default)]
pub(crate) struct DebounceBuffer {
    pub event_ids: Vec<String>,
    pub payloads: Vec<Value>,
    /// Bumped on every push; flush tasks older than the current generation
    /// exit without draining (a newer push re-armed the window).
    pub generation: u64,
}

/// Arm a flush task firing `window` after this push. One task per push;
/// only the task whose generation still matches at wake drains the buffer.
fn arm_flush(state: &Arc<AppState>, internal_id: &str, generation: u64, window: Duration) {
    let state = Arc::clone(state);
    let internal_id = internal_id.to_string();
    tokio::spawn(async move {
        tokio::select! {
            _ = tokio::time::sleep(window) => {}
            // Draining mid-shutdown would schedule runs the drain is about
            // to cancel; the buffered events stay `pending` (inspectable,
            // replayable) for the next process.
            _ = state.shutdown.cancelled() => return,
        }
        let drained = {
            let mut map = state.trigger_debounce.lock().await;
            match map.get(&internal_id) {
                Some(buf) if buf.generation == generation => map.remove(&internal_id),
                _ => None,
            }
        };
        if let Some(buffer) = drained {
            flush_buffer(state, &internal_id, buffer).await;
        }
    });
}

/// Execute the coalesced action for a drained burst and transition every
/// contributing event (`coalesced` with the shared run id, or `failed`).
async fn flush_buffer(state: Arc<AppState>, internal_id: &str, buffer: DebounceBuffer) {
    // Re-read the trigger: deleted or disabled since receipt fails the burst.
    let trigger = match state.server_store.get_trigger(internal_id).await {
        Ok(Some(trigger)) if trigger.enabled => trigger,
        Ok(_) => {
            transition_events(
                &state,
                internal_id,
                &buffer,
                TriggerEventStatus::Failed,
                None,
                Some("trigger deleted or disabled before the debounce window closed".to_string()),
            )
            .await;
            return;
        }
        Err(error) => {
            transition_events(
                &state,
                internal_id,
                &buffer,
                TriggerEventStatus::Failed,
                None,
                Some(format!("trigger reload failed: {error}")),
            )
            .await;
            return;
        }
    };
    let event = Value::Array(buffer.payloads.clone());
    match execute_action(&state, &trigger, &event, &buffer.event_ids).await {
        Ok(run_id) => {
            transition_events(
                &state,
                internal_id,
                &buffer,
                TriggerEventStatus::Coalesced,
                Some(run_id),
                None,
            )
            .await;
            bump_counters(&state.server_store, internal_id, 0, 1).await;
        }
        Err(error) => {
            transition_events(
                &state,
                internal_id,
                &buffer,
                TriggerEventStatus::Failed,
                None,
                Some(error),
            )
            .await;
        }
    }
}

/// Apply a debounce-flush outcome to every event of the burst (the store's
/// append upserts on event id).
async fn transition_events(
    state: &Arc<AppState>,
    internal_id: &str,
    buffer: &DebounceBuffer,
    status: TriggerEventStatus,
    run_id: Option<String>,
    error: Option<String>,
) {
    for event_id in &buffer.event_ids {
        let fetched = state
            .server_store
            .get_trigger_event(internal_id, event_id)
            .await;
        match fetched {
            Ok(Some(mut event)) => {
                event.status = status;
                event.run_id = run_id.clone();
                event.error = error.clone();
                if let Err(error) = state.server_store.append_trigger_event(&event).await {
                    tracing::warn!(%event_id, %error, "trigger event transition failed");
                }
            }
            Ok(None) => {
                tracing::warn!(%event_id, "debounced event vanished before transition")
            }
            Err(error) => {
                tracing::warn!(%event_id, %error, "debounced event reload failed")
            }
        }
    }
}

/// Best-effort bookkeeping, the cron scheduler's discipline: a lost counter
/// update never fails the request.
async fn bump_counters(store: &Arc<dyn ServerStore>, internal_id: &str, events: u64, runs: u64) {
    match store.get_trigger(internal_id).await {
        Ok(Some(mut record)) => {
            record.events_received += events;
            record.runs_fired += runs;
            if let Err(error) = store.upsert_trigger(&record).await {
                tracing::warn!(trigger_id = %internal_id, %error, "trigger bookkeeping failed");
            }
        }
        Ok(None) => {} // deleted between receipt and bookkeeping
        Err(error) => {
            tracing::warn!(trigger_id = %internal_id, %error, "trigger bookkeeping read failed")
        }
    }
}

// --------------------------------------------------------------------- //
// HTTP handlers — registry (API-key authenticated, tenant-scoped)
// --------------------------------------------------------------------- //

/// The default input template: forward the event under an `event` key
/// (`{"event": <payload>}` — the whole-string placeholder preserves the
/// payload's JSON type).
fn default_template() -> Value {
    json!({"event": "{{event}}"})
}

#[derive(Debug, Deserialize)]
pub(crate) struct CreateTriggerPayload {
    name: String,
    target: TriggerTarget,
    action: TriggerAction,
    /// Input template with `{{event.*}}` placeholders (default:
    /// `{"event": "{{event}}"}` — the payload under an `event` key).
    #[serde(default)]
    input_template: Option<Value>,
    #[serde(default = "default_enabled")]
    enabled: bool,
    /// Per-trigger HMAC secret (server-generated when omitted; min 16 chars
    /// when provided).
    #[serde(default)]
    secret: Option<String>,
    /// Debounce window in milliseconds (1..=300_000).
    #[serde(default)]
    debounce_ms: Option<u64>,
    /// Client-chosen trigger id (a UUID v4 is generated when omitted).
    #[serde(default)]
    trigger_id: Option<String>,
}

fn default_enabled() -> bool {
    true
}

/// Validate name / target id / binding / debounce / secret, shared by
/// create and update.
fn validate_fields(
    name: &str,
    target: &TriggerTarget,
    action: TriggerAction,
    debounce_ms: Option<u64>,
    secret: Option<&str>,
) -> Result<(), ApiError> {
    // Same label convention as task kinds / worker ids: non-empty,
    // bounded, content free-form (the name is stored, never pathed).
    tasks::validate_label("name", name, 128).map_err(ApiError::bad_request)?;
    validate_client_id("target id", target.id())?;
    validate_binding(action, target).map_err(ApiError::bad_request)?;
    if let Some(ms) = debounce_ms {
        if !(1..=MAX_DEBOUNCE_MS).contains(&ms) {
            return Err(ApiError::bad_request(format!(
                "`debounce_ms` must be within 1..={MAX_DEBOUNCE_MS}"
            )));
        }
    }
    if let Some(secret) = secret {
        if secret.len() < 16 {
            return Err(ApiError::bad_request(
                "`secret` must be at least 16 chars (omit it to have one generated)".to_string(),
            ));
        }
    }
    Ok(())
}

/// The external (wire) form of a trigger record: internal id unscoped.
fn wire_trigger(record: &TriggerRecord) -> Value {
    let mut wire = record.clone();
    wire.trigger_id = strip_owned(tenant_of_internal(&record.trigger_id), &record.trigger_id)
        .unwrap_or(&record.trigger_id)
        .to_string();
    serde_json::to_value(&wire).expect("trigger serialization is infallible")
}

/// The external (wire) form of an event record.
fn wire_event(record: &TriggerEventRecord) -> Value {
    let mut wire = record.clone();
    wire.trigger_id = strip_owned(tenant_of_internal(&record.trigger_id), &record.trigger_id)
        .unwrap_or(&record.trigger_id)
        .to_string();
    serde_json::to_value(&wire).expect("event serialization is infallible")
}

/// Fetch the caller's trigger by external id (cross-tenant answers 404).
async fn require_trigger(
    state: &AppState,
    tenant: &TenantContext,
    trigger_id: &str,
) -> Result<TriggerRecord, ApiError> {
    state
        .server_store
        .get_trigger(&tenant.scope(trigger_id))
        .await
        .map_err(internal_err)?
        .ok_or_else(|| ApiError::not_found(format!("trigger `{trigger_id}` not found")))
}

pub(crate) async fn create_trigger(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(payload): Json<CreateTriggerPayload>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    validate_fields(
        &payload.name,
        &payload.target,
        payload.action,
        payload.debounce_ms,
        payload.secret.as_deref(),
    )?;
    let trigger_id = payload
        .trigger_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    validate_client_id("trigger_id", &trigger_id)?;

    let record = TriggerRecord {
        trigger_id: tenant.scope(&trigger_id),
        name: payload.name,
        target: payload.target,
        action: payload.action,
        input_template: payload.input_template.unwrap_or_else(default_template),
        enabled: payload.enabled,
        secret: payload.secret.unwrap_or_else(generate_secret),
        debounce_ms: payload.debounce_ms,
        created_at: Utc::now(),
        events_received: 0,
        runs_fired: 0,
    };
    let created = state
        .server_store
        .create_trigger(&record)
        .await
        .map_err(internal_err)?;
    if !created {
        return Err(ApiError::conflict(format!(
            "trigger `{trigger_id}` already exists"
        )));
    }
    Ok((StatusCode::CREATED, Json(wire_trigger(&record))))
}

pub(crate) async fn list_triggers(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
) -> Result<Json<Value>, ApiError> {
    let records = state
        .server_store
        .list_triggers()
        .await
        .map_err(internal_err)?;
    // Only this tenant's triggers, reported with their external ids.
    let mut records: Vec<TriggerRecord> = records
        .into_iter()
        .filter(|record| tenant.owns(&record.trigger_id))
        .collect();
    records.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.trigger_id.cmp(&b.trigger_id))
    });
    let wire: Vec<Value> = records.iter().map(wire_trigger).collect();
    Ok(Json(json!(wire)))
}

pub(crate) async fn get_trigger(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath(trigger_id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let record = require_trigger(&state, &tenant, &trigger_id).await?;
    Ok(Json(wire_trigger(&record)))
}

#[derive(Debug, Deserialize)]
pub(crate) struct UpdateTriggerPayload {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    target: Option<TriggerTarget>,
    #[serde(default)]
    action: Option<TriggerAction>,
    #[serde(default)]
    input_template: Option<Value>,
    #[serde(default)]
    enabled: Option<bool>,
    /// `null` clears the debounce window (absent = keep).
    #[serde(default)]
    debounce_ms: Option<Option<u64>>,
    /// Rotate the webhook secret.
    #[serde(default)]
    secret: Option<String>,
}

pub(crate) async fn update_trigger(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath(trigger_id): AxumPath<String>,
    Json(payload): Json<UpdateTriggerPayload>,
) -> Result<Json<Value>, ApiError> {
    let mut record = require_trigger(&state, &tenant, &trigger_id).await?;
    if let Some(name) = payload.name {
        record.name = name;
    }
    if let Some(target) = payload.target {
        record.target = target;
    }
    if let Some(action) = payload.action {
        record.action = action;
    }
    if let Some(template) = payload.input_template {
        record.input_template = template;
    }
    if let Some(enabled) = payload.enabled {
        record.enabled = enabled;
    }
    if let Some(debounce_ms) = payload.debounce_ms {
        record.debounce_ms = debounce_ms;
    }
    if let Some(secret) = payload.secret {
        record.secret = secret;
    }
    validate_fields(
        &record.name,
        &record.target,
        record.action,
        record.debounce_ms,
        None, // the record's secret was validated on entry
    )?;
    state
        .server_store
        .upsert_trigger(&record)
        .await
        .map_err(internal_err)?;
    Ok(Json(wire_trigger(&record)))
}

pub(crate) async fn delete_trigger(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath(trigger_id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let internal_id = tenant.scope(&trigger_id);
    if state
        .server_store
        .delete_trigger(&internal_id)
        .await
        .map_err(internal_err)?
    {
        // Drop any in-flight debounce burst: events buffered against a
        // deleted trigger must not fire it. Their records stay `pending`
        // and replayable history.
        state.trigger_debounce.lock().await.remove(&internal_id);
        Ok(Json(json!({ "trigger_id": trigger_id, "deleted": true })))
    } else {
        Err(ApiError::not_found(format!(
            "trigger `{trigger_id}` not found"
        )))
    }
}

pub(crate) async fn list_trigger_events(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath(trigger_id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let record = require_trigger(&state, &tenant, &trigger_id).await?;
    let mut events = state
        .server_store
        .list_trigger_events(&record.trigger_id)
        .await
        .map_err(internal_err)?;
    events.reverse(); // newest first, like the checkpoint history endpoint
    let wire: Vec<Value> = events.iter().map(wire_event).collect();
    Ok(Json(json!(wire)))
}

/// The dead-letter list: events whose action failed, newest first.
pub(crate) async fn list_dead_letter(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath(trigger_id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let record = require_trigger(&state, &tenant, &trigger_id).await?;
    let events = state
        .server_store
        .list_trigger_events(&record.trigger_id)
        .await
        .map_err(internal_err)?;
    let wire: Vec<Value> = events
        .iter()
        .rev()
        .filter(|event| event.status == TriggerEventStatus::Failed)
        .map(wire_event)
        .collect();
    Ok(Json(json!(wire)))
}

/// Re-execute a logged event (any status — replays are how the dead letter
/// gets re-driven, but re-running a succeeded event is legitimate too).
/// Bypasses signature and debounce: the stored payload executes immediately,
/// and the replay is itself logged as a new event pointing at the original.
pub(crate) async fn replay_event(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath((trigger_id, event_id)): AxumPath<(String, String)>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let trigger = require_trigger(&state, &tenant, &trigger_id).await?;
    if !trigger.enabled {
        return Err(ApiError::conflict(format!(
            "trigger `{trigger_id}` is disabled"
        )));
    }
    let original = state
        .server_store
        .get_trigger_event(&trigger.trigger_id, &event_id)
        .await
        .map_err(internal_err)?
        .ok_or_else(|| ApiError::not_found(format!("event `{event_id}` not found")))?;
    let payload_hash = sha256_hex(original.payload.to_string().as_bytes());
    execute_and_log(
        &state,
        &trigger,
        original.payload,
        payload_hash,
        Some(event_id),
    )
    .await
}

// --------------------------------------------------------------------- //
// HTTP handler — signed webhook ingress (NOT behind the API-key layer)
// --------------------------------------------------------------------- //

/// `POST /triggers/{id}/webhook` — receive one signed event. The route sits
/// outside the API-key middleware; the per-trigger HMAC signature is the
/// credential, and among same-external-id triggers across tenants the one
/// whose secret verifies owns the event (signature verification doubles as
/// tenant resolution).
pub(crate) async fn webhook(
    AxumState(state): AxumState<Arc<AppState>>,
    AxumPath(trigger_id): AxumPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let unauthorized = |message: &str| {
        ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            message.to_string(),
        )
    };
    let signature = headers
        .get("x-rusty-signature")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| unauthorized("an `X-Rusty-Signature` header is required"))?;

    // Candidate triggers: every tenant's record whose external id matches.
    // An unknown id 404s; a known id with a bad signature 401s — a sender
    // probing ids learns nothing beyond what a valid signature would tell it.
    let all = state
        .server_store
        .list_triggers()
        .await
        .map_err(internal_err)?;
    let candidates: Vec<TriggerRecord> = all
        .into_iter()
        .filter(|record| {
            strip_owned(tenant_of_internal(&record.trigger_id), &record.trigger_id)
                == Some(trigger_id.as_str())
        })
        .collect();
    if candidates.is_empty() {
        return Err(ApiError::not_found(format!(
            "trigger `{trigger_id}` not found"
        )));
    }
    let Some(trigger) = candidates
        .into_iter()
        .find(|record| verify_signature(&record.secret, &body, signature))
    else {
        return Err(unauthorized("invalid `X-Rusty-Signature`"));
    };

    if !trigger.enabled {
        return Err(ApiError::conflict(format!(
            "trigger `{trigger_id}` is disabled"
        )));
    }
    let payload: Value = serde_json::from_slice(&body)
        .map_err(|e| ApiError::bad_request(format!("webhook body must be valid JSON: {e}")))?;

    if trigger.debounce_ms.is_some() {
        return buffer_event(&state, trigger, payload, sha256_hex(&body)).await;
    }

    bump_counters(&state.server_store, &trigger.trigger_id, 1, 0).await;
    execute_and_log(&state, &trigger, payload, sha256_hex(&body), None).await
}

/// Shared execute-and-record for immediate webhooks and replays: the event
/// is logged exactly once, with its final status. `payload_hash` is the
/// SHA-256 of the signed raw body for webhooks, of the stored payload's
/// canonical serialization for replays. Action failures are 502 — the
/// request was authentic and understood, but the bound agent action failed;
/// the dead-letter list carries the detail.
async fn execute_and_log(
    state: &Arc<AppState>,
    trigger: &TriggerRecord,
    payload: Value,
    payload_hash: String,
    replayed_from: Option<String>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let event_id = uuid::Uuid::new_v4().to_string();
    let mut event = TriggerEventRecord {
        event_id: event_id.clone(),
        trigger_id: trigger.trigger_id.clone(),
        payload_hash,
        payload: payload.clone(),
        action: trigger.action,
        status: TriggerEventStatus::Pending,
        run_id: None,
        error: None,
        replayed_from,
        created_at: Utc::now(),
    };
    match execute_action(state, trigger, &payload, std::slice::from_ref(&event_id)).await {
        Ok(run_id) => {
            event.status = TriggerEventStatus::Executed;
            event.run_id = Some(run_id);
            bump_counters(&state.server_store, &trigger.trigger_id, 0, 1).await;
        }
        Err(error) => {
            event.status = TriggerEventStatus::Failed;
            event.error = Some(error);
        }
    }
    state
        .server_store
        .append_trigger_event(&event)
        .await
        .map_err(internal_err)?;
    if event.status == TriggerEventStatus::Failed {
        let message = event.error.clone().expect("failed events carry an error");
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            "action_failed",
            format!(
                "trigger action failed (event `{event_id}` is on the dead-letter list): {message}"
            ),
        ));
    }
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "event_id": event.event_id,
            "status": event.status,
            "run_id": event.run_id,
            "replayed_from": event.replayed_from,
        })),
    ))
}

/// Record one event of a debounced burst (`pending`) and buffer it; the
/// window's flush coalesces the burst into one action.
async fn buffer_event(
    state: &Arc<AppState>,
    trigger: TriggerRecord,
    payload: Value,
    payload_hash: String,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let event_id = uuid::Uuid::new_v4().to_string();
    let event = TriggerEventRecord {
        event_id: event_id.clone(),
        trigger_id: trigger.trigger_id.clone(),
        payload_hash,
        payload: payload.clone(),
        action: trigger.action,
        status: TriggerEventStatus::Pending,
        run_id: None,
        error: None,
        replayed_from: None,
        created_at: Utc::now(),
    };
    state
        .server_store
        .append_trigger_event(&event)
        .await
        .map_err(internal_err)?;

    let window = Duration::from_millis(trigger.debounce_ms.expect("checked by the caller"));
    let (generation, immediate) = {
        let mut map = state.trigger_debounce.lock().await;
        let buffer = map.entry(trigger.trigger_id.clone()).or_default();
        buffer.event_ids.push(event_id.clone());
        buffer.payloads.push(payload);
        buffer.generation += 1;
        let generation = buffer.generation;
        // A burst at the batch cap flushes now, window or not — memory is
        // bounded, and 64 payloads is no longer a "burst" in any sense.
        let immediate = if buffer.event_ids.len() >= MAX_DEBOUNCE_BATCH {
            map.remove(&trigger.trigger_id)
        } else {
            None
        };
        (generation, immediate)
    };
    if let Some(buffer) = immediate {
        let state = Arc::clone(state);
        let internal_id = trigger.trigger_id.clone();
        tokio::spawn(async move { flush_buffer(state, &internal_id, buffer).await });
    } else {
        arm_flush(state, &trigger.trigger_id, generation, window);
    }
    bump_counters(&state.server_store, &trigger.trigger_id, 1, 0).await;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "event_id": event_id,
            "status": TriggerEventStatus::Pending,
        })),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_substitutes_whole_placeholders_verbatim() {
        let event = json!({"user": {"id": 42, "name": "ada"}, "tags": ["a", "b"]});
        // Whole-string placeholders keep the resolved JSON type.
        assert_eq!(render(&json!("{{event.user.id}}"), &event), json!(42));
        assert_eq!(
            render(&json!("{{event.user}}"), &event),
            json!({"id": 42, "name": "ada"})
        );
        assert_eq!(render(&json!("{{event.tags.1}}"), &event), json!("b"));
        assert_eq!(render(&json!("{{event}}"), &event), event);
        // Unresolvable paths render null, never an error.
        assert_eq!(render(&json!("{{event.nope}}"), &event), json!(Value::Null));
        assert_eq!(render(&json!("{{other.x}}"), &event), json!(Value::Null));
    }

    #[test]
    fn render_interpolates_embedded_placeholders_as_strings() {
        let event = json!({"pr": {"number": 7, "merged": true}});
        let template = json!({"log": ["PR #{{event.pr.number}} merged={{event.pr.merged}}"]});
        assert_eq!(
            render(&template, &event),
            json!({"log": ["PR #7 merged=true"]})
        );
        // Missing embedded paths interpolate as empty text.
        assert_eq!(render(&json!("x{{event.missing}}y"), &event), json!("xy"));
        // Non-strings and unclosed placeholders pass through untouched.
        assert_eq!(
            render(&json!({"n": 3, "ok": true}), &event),
            json!({"n": 3, "ok": true})
        );
        assert_eq!(render(&json!("{{event.pr"), &event), json!("{{event.pr"));
    }

    #[test]
    fn render_recurses_through_objects_and_arrays() {
        let event = json!({"v": "x"});
        let template = json!({"outer": [{"inner": "{{event.v}}"}, "{{event.v}}"]});
        assert_eq!(
            render(&template, &event),
            json!({"outer": [{"inner": "x"}, "x"]})
        );
    }

    #[test]
    fn signature_verification_accepts_and_rejects() {
        let secret = "test-secret-0123456789";
        let body = br#"{"hello":"world"}"#;
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        let good = format!("sha256={}", hex_encode(&mac.finalize().into_bytes()));

        assert!(verify_signature(secret, body, &good));
        assert!(verify_signature(
            secret,
            body,
            &good.to_uppercase().replace("SHA256=", "sha256=")
        ));
        // Wrong body, wrong secret, missing prefix, garbage hex: all invalid.
        assert!(!verify_signature(secret, b"{}", &good));
        assert!(!verify_signature("other-secret-0123", body, &good));
        assert!(!verify_signature(
            secret,
            body,
            &good.replace("sha256=", "sha1=")
        ));
        assert!(!verify_signature(secret, body, "sha256=not-hex"));
        assert!(!verify_signature(secret, body, "sha256=abc"));
    }

    #[test]
    fn hex_round_trip() {
        let bytes: Vec<u8> = (0u8..=255).collect();
        assert_eq!(hex_decode(&hex_encode(&bytes)), Some(bytes));
        assert_eq!(hex_decode("0g"), None);
        assert_eq!(hex_decode("abc"), None);
    }

    #[test]
    fn binding_validation_pairs_actions_with_targets() {
        let assistant = TriggerTarget::Assistant {
            id: "a".to_string(),
        };
        let thread = TriggerTarget::Thread {
            id: "t".to_string(),
        };
        assert!(validate_binding(TriggerAction::StartRun, &assistant).is_ok());
        assert!(validate_binding(TriggerAction::ResumeThread, &thread).is_ok());
        assert!(validate_binding(TriggerAction::SendMessage, &thread).is_ok());
        assert!(validate_binding(TriggerAction::StartRun, &thread).is_err());
        assert!(validate_binding(TriggerAction::ResumeThread, &assistant).is_err());
        assert!(validate_binding(TriggerAction::SendMessage, &assistant).is_err());
    }

    #[test]
    fn target_serde_shape_is_stable() {
        let target = TriggerTarget::Thread {
            id: "t1".to_string(),
        };
        assert_eq!(
            serde_json::to_value(&target).unwrap(),
            json!({"kind": "thread", "id": "t1"})
        );
        let parsed: TriggerTarget =
            serde_json::from_value(json!({"kind": "assistant", "id": "a1"})).unwrap();
        assert_eq!(
            parsed,
            TriggerTarget::Assistant {
                id: "a1".to_string()
            }
        );
        let action: TriggerAction = serde_json::from_value(json!("start_run")).unwrap();
        assert_eq!(action, TriggerAction::StartRun);
    }
}
