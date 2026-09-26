//! Approvals: a run that pauses to ask before an irreversible effect, and the
//! decision that resumes it.
//!
//! The ReAct tools node raises an interrupt of kind `approval` before it
//! executes any call admission would refuse for want of a token (see
//! `rusty_agent_runtime::react::APPROVAL_INTERRUPT_KIND`). The run goes
//! `interrupted`; this plane records what it asked as one JSON file per run
//! under `{store_path}/approvals/`. A person decides through
//! `POST /approvals/{run_id}/decide`: approving mints one token per request,
//! scoped to that request's effect id and to nothing else, and resumes the
//! run with the tokens on its config; denying resumes it with the refusal in
//! the resume value, which the tools node turns into a tool result the model
//! reads and finishes on. Either way the resumed run is attributed to the
//! person who decided, and the record says who, when, and which run.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Path as AxumPath, State as AxumState};
use axum::{Extension, Json};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::auth::TenantContext;
use crate::error::ApiError;
use crate::routes::AppState;
use crate::runs::{CommandPayload, RunPayload};

/// What a paused run asked, and what was decided.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRecord {
    pub run_id: String,
    /// External thread id — the run to resume lives on it.
    pub thread_id: String,
    pub graph: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant_id: Option<String>,
    /// The calls the run will not make without a decision, as the tools node
    /// described them (`ApprovalNeeded`, wire form).
    pub requests: Vec<Value>,
    pub requested_at: DateTime<Utc>,
    /// Who started the paused run, as its metadata says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_by: Option<Value>,
    /// `pending` → `approved` | `denied`.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_by: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The run the decision started on the same thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed_run_id: Option<String>,
    /// The world (by id) the paused run acts in, when it was put in one:
    /// the approved effect lands there, not in the live system. Served
    /// with the world's name and host so the person deciding sees it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub world: Option<String>,
}

/// A person's decision that stands: the same call — this agent, this tool,
/// these arguments — is approved without asking until `until`. Made from
/// a decision in the Inbox; a scheduled agent that posts the same kind of
/// message every morning need not ask every morning.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StandingApproval {
    pub id: String,
    pub tenant: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant_id: Option<String>,
    pub tool: String,
    /// The call's arguments, exactly; a call that differs in one field asks.
    pub arguments: Value,
    /// Who made it, and from which paused run.
    pub by: Value,
    pub from_run: String,
    pub made_at: DateTime<Utc>,
    pub until: DateTime<Utc>,
    /// How many pauses it decided.
    #[serde(default)]
    pub uses: u64,
}

impl StandingApproval {
    /// Whether this standing approval decides `request` for `record`.
    fn covers(&self, record: &ApprovalRecord, request: &Value, now: DateTime<Utc>) -> bool {
        now < self.until
            && self.assistant_id == record.assistant_id
            && request.get("tool").and_then(Value::as_str) == Some(self.tool.as_str())
            && request.get("arguments").cloned().unwrap_or(Value::Null) == self.arguments
    }
}

/// Where approval records live: one JSON file per paused run; standing
/// approvals beside them, one file each.
#[derive(Debug)]
pub struct ApprovalPlane {
    root: PathBuf,
    standing_root: PathBuf,
    /// Every pause is announced here — `(tenant, run_id)` — for the
    /// standing decider the server runs.
    paused_tx: tokio::sync::mpsc::UnboundedSender<(String, String)>,
    paused_rx: std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<(String, String)>>>,
}

impl ApprovalPlane {
    pub fn new(store_path: &Path) -> Self {
        let (paused_tx, paused_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            root: store_path.join("approvals"),
            standing_root: store_path.join("approvals").join("standing"),
            paused_tx,
            paused_rx: std::sync::Mutex::new(Some(paused_rx)),
        }
    }

    /// Announce a pause to the standing decider.
    fn paused(&self, tenant: &str, run_id: &str) {
        let _ = self.paused_tx.send((tenant.to_owned(), run_id.to_owned()));
    }

    /// The pause announcements, taken once by the decider task.
    pub(crate) fn take_paused(
        &self,
    ) -> Option<tokio::sync::mpsc::UnboundedReceiver<(String, String)>> {
        self.paused_rx.lock().ok().and_then(|mut rx| rx.take())
    }

    pub async fn persist_standing(&self, standing: &StandingApproval) {
        if let Err(error) =
            crate::connectors::persist_json(&self.standing_root, &standing.id, standing).await
        {
            tracing::warn!(id = %standing.id, %error, "standing approval not persisted");
        }
    }

    /// A tenant's standing approvals, live ones first, newest first.
    pub fn list_standing(&self, tenant: &str) -> Vec<StandingApproval> {
        let mut records: Vec<StandingApproval> = std::fs::read_dir(&self.standing_root)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| std::fs::read(entry.path()).ok())
            .filter_map(|bytes| serde_json::from_slice::<StandingApproval>(&bytes).ok())
            .filter(|s| s.tenant == tenant)
            .collect();
        records.sort_by_key(|b| std::cmp::Reverse(b.made_at));
        records
    }

    pub fn remove_standing(&self, tenant: &str, id: &str) -> bool {
        let path = self.standing_root.join(format!("{id}.json"));
        let mine = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<StandingApproval>(&b).ok())
            .is_some_and(|s| s.tenant == tenant);
        mine && std::fs::remove_file(path).is_ok()
    }

    /// The standing approvals that decide every request of `record`, one
    /// per request, when each has one; otherwise the run asks.
    pub fn standing_for(
        &self,
        tenant: &str,
        record: &ApprovalRecord,
    ) -> Option<Vec<StandingApproval>> {
        let now = Utc::now();
        let live = self.list_standing(tenant);
        record
            .requests
            .iter()
            .map(|request| {
                live.iter()
                    .find(|s| s.covers(record, request, now))
                    .cloned()
            })
            .collect()
    }

    pub async fn persist(&self, record: &ApprovalRecord) {
        if let Err(error) =
            crate::connectors::persist_json(&self.root, &record.run_id, record).await
        {
            tracing::warn!(run_id = %record.run_id, %error, "approval not persisted");
        }
    }

    pub fn load(&self, run_id: &str) -> Option<ApprovalRecord> {
        let bytes = std::fs::read(self.root.join(format!("{run_id}.json"))).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Every record, newest request first. Corrupt files read as absent.
    pub fn list(&self) -> Vec<ApprovalRecord> {
        let mut records: Vec<ApprovalRecord> = std::fs::read_dir(&self.root)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| std::fs::read(entry.path()).ok())
            .filter_map(|bytes| serde_json::from_slice(&bytes).ok())
            .collect();
        records.sort_by_key(|b| std::cmp::Reverse(b.requested_at));
        records
    }
}

/// Record what an interrupted run asked, when the interrupt is an approval.
/// Called from the run's terminal path; anything else is not ours.
pub(crate) async fn record_if_approval(
    deps: &crate::runs::RunDeps,
    run_id: &str,
    wire_thread_id: &str,
    graph: &str,
    payload: &RunPayload,
    interrupt: &Value,
) {
    if interrupt.get("kind").and_then(Value::as_str)
        != Some(rusty_agent_runtime::react::APPROVAL_INTERRUPT_KIND)
    {
        return;
    }
    let plane = &deps.approvals;
    let connections = deps.connection_tools.as_deref();
    // Each request names the connection its tool runs through, as bound
    // now — the identity Catalog and Security show, kept with the request.
    let requests: Vec<Value> = interrupt
        .get("requests")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|mut request| {
            if let (Some(cell), Some(tool)) =
                (connections, request.get("tool").and_then(Value::as_str))
            {
                if let Some(binding) = cell.active_binding(tool) {
                    request["connection"] =
                        json!({"instance_id": binding.instance_id, "name": binding.connection});
                }
            }
            request
        })
        .collect();
    let record = ApprovalRecord {
        run_id: run_id.to_owned(),
        thread_id: wire_thread_id.to_owned(),
        graph: graph.to_owned(),
        assistant_id: payload.assistant_id.clone(),
        requests,
        requested_at: Utc::now(),
        requested_by: payload
            .metadata
            .as_ref()
            .and_then(|m| m.get("created_by"))
            .cloned(),
        status: "pending".to_owned(),
        decided_at: None,
        decided_by: None,
        reason: None,
        resumed_run_id: None,
        world: payload.config.as_ref().and_then(|c| c.world.clone()),
    };
    plane.persist(&record).await;
    tracing::info!(%run_id, requests = record.requests.len(), "run paused for approval");
    plane.paused(crate::auth::tenant_of_internal(wire_thread_id), run_id);
    // A run nobody is sitting in — a schedule's, a queue's, a webhook's, an
    // assignment's round, an agent asked by one of those — waits on a
    // person who is not looking at it. Tell the person it acts for, once:
    // the Inbox card is the decision; the notice is how they learn it is
    // there. A person's own conversation needs no telling; they are in it.
    let metadata = payload.metadata.as_ref();
    let delegated = metadata.is_some_and(|m| m.get("delegation").is_some());
    let channel = metadata
        .and_then(|m| m.get("channel"))
        .and_then(Value::as_str)
        .unwrap_or(if delegated { "delegation" } else { "http" });
    // An evaluation in a world decides for itself the moment the run
    // pauses (see `dataset_runs::run_case`): telling a person about a
    // decision that is not theirs to make is noise, so nobody is told.
    let evaluation_decides = channel == "evaluation" && record.world.is_some();
    let fired = (channel != "http" || delegated) && !evaluation_decides;
    let person = metadata
        .and_then(|m| m.get("on_behalf_of").or_else(|| m.get("created_by")))
        .filter(|p| p.get("principal_id").is_some())
        .cloned();
    if let (true, Some(person)) = (fired, person) {
        let tenant = crate::auth::tenant_of_internal(wire_thread_id).to_string();
        let tools: Vec<String> = record
            .requests
            .iter()
            .filter_map(|r| r.get("tool").and_then(Value::as_str).map(str::to_owned))
            .collect();
        // The agent by name, never an id in a sentence a person reads.
        let agent = match &record.assistant_id {
            Some(id) => match deps
                .server_store
                .get_assistant(&crate::auth::scope_id(&tenant, id))
                .await
            {
                Ok(Some(assistant)) => assistant.name,
                _ => id.clone(),
            },
            None => graph.to_owned(),
        };
        let title = format!("Needs you: {agent} paused before {}", tools.join(", "));
        let text = format!(
            "A run of yours that nobody is sitting in ({channel}) stopped before an irreversible action{} and waits for your decision. Decide in the Inbox; it continues from there.",
            if record.world.is_some() {
                " — in a stand-in world, so the effect lands there, not in the live system"
            } else {
                ""
            }
        );
        crate::notices::tell_in(
            &deps.server_store,
            &tenant,
            &person,
            &format!("approval:{run_id}"),
            json!({"kind": "approval", "run_id": run_id, "assistant_id": record.assistant_id, "channel": channel, "world": record.world}),
            &title,
            &text,
        )
        .await;
    }
}

/// The approval tokens a resume value carries, when it is an approving
/// decision: minted by the decide handler, scoped one per request.
pub(crate) fn tokens_in(resume: &Value) -> Vec<rusty_agent_runtime::effects::ApprovalToken> {
    if resume.get("kind").and_then(Value::as_str)
        != Some(rusty_agent_runtime::react::APPROVAL_INTERRUPT_KIND)
        || resume.get("decision").and_then(Value::as_str) != Some("approve")
    {
        return Vec::new();
    }
    resume
        .get("tokens")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|t| serde_json::from_value(t.clone()).ok())
        .collect()
}

async fn owned(state: &AppState, tenant: &TenantContext, record: &ApprovalRecord) -> bool {
    matches!(
        state
            .server_store
            .get_thread(&tenant.scope(&record.thread_id))
            .await,
        Ok(Some(_))
    )
}

/// `GET /approvals` — every approval on this tenant's threads, newest first;
/// `?status=pending` narrows to what still needs a decision.
pub(crate) async fn list_approvals(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    axum::extract::Query(query): axum::extract::Query<ListQuery>,
) -> Result<Json<Value>, ApiError> {
    let mut out = Vec::new();
    for record in state.run_deps.approvals.list() {
        if let Some(status) = &query.status {
            if &record.status != status {
                continue;
            }
        }
        if owned(&state, &tenant, &record).await {
            let mut served =
                serde_json::to_value(&record).map_err(|e| ApiError::internal(e.to_string()))?;
            mark_revoked_connections(&state, &mut served);
            say_who_and_where(&state, &tenant, &record, &mut served).await;
            out.push(served);
        }
    }
    Ok(Json(json!({ "approvals": out })))
}

/// What a person deciding needs said in words: the agent by name, not id,
/// and the world the effect lands in — its name and the host it stands in
/// for — or that the world is gone, in which case approving reaches
/// nothing (the call is refused by name).
async fn say_who_and_where(
    state: &AppState,
    tenant: &TenantContext,
    record: &ApprovalRecord,
    served: &mut Value,
) {
    if let Some(assistant_id) = &record.assistant_id {
        let internal = crate::auth::scope_id(tenant.tenant(), assistant_id);
        if let Ok(Some(assistant)) = state.server_store.get_assistant(&internal).await {
            served["agent_name"] = json!(assistant.name);
        }
    }
    if let Some(world_id) = &record.world {
        served["world"] = match state.worlds.find(tenant.tenant(), world_id).await {
            Ok(Some(world)) => {
                json!({"world_id": world.world_id, "name": world.name, "stands_for": world.stands_for})
            }
            _ => json!({"world_id": world_id, "gone": true}),
        };
    }
}

/// A pending request whose connection is no longer the one bound says so:
/// `connection.revoked_at` — approving it reaches nothing.
fn mark_revoked_connections(state: &AppState, served: &mut Value) {
    let Some(cell) = &state.connection_tools else {
        return;
    };
    let Some(requests) = served.get_mut("requests").and_then(Value::as_array_mut) else {
        return;
    };
    for request in requests {
        let Some(tool) = request
            .get("tool")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            continue;
        };
        let Some(instance_id) = request
            .pointer("/connection/instance_id")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            continue;
        };
        if cell.instance_of(&tool).as_deref() == Some(instance_id.as_str()) {
            continue;
        }
        if let Some(binding) = cell.binding_of(&tool, &instance_id) {
            request["connection"]["revoked_at"] = json!(binding.unbound_at);
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct ListQuery {
    #[serde(default)]
    status: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct DecidePayload {
    /// `approve` or `deny`.
    decision: String,
    #[serde(default)]
    reason: Option<String>,
    /// With `approve`: let the same call — this agent, this tool, these
    /// arguments — run without asking for this many hours (1..=720).
    #[serde(default)]
    standing_hours: Option<u64>,
}

/// The longest a standing approval lasts: thirty days.
const STANDING_MAX_HOURS: u64 = 720;

/// `POST /approvals/{run_id}/decide` — the decision, and the resumed run.
pub(crate) async fn decide(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath(run_id): AxumPath<String>,
    Json(payload): Json<DecidePayload>,
) -> Result<Json<Value>, ApiError> {
    let approve = match payload.decision.as_str() {
        "approve" => true,
        "deny" => false,
        other => {
            return Err(ApiError::bad_request(format!(
                "decision `{other}` — say `approve` or `deny`"
            )));
        }
    };
    let hours = match payload.standing_hours {
        Some(h) if !approve => {
            return Err(ApiError::bad_request(format!(
                "standing_hours ({h}) goes with `approve`; a denial does not stand"
            )));
        }
        Some(h) if !(1..=STANDING_MAX_HOURS).contains(&h) => {
            return Err(ApiError::bad_request(format!(
                "standing_hours must be 1..={STANDING_MAX_HOURS} (thirty days), got {h}"
            )));
        }
        other => other,
    };
    let record = decide_run(
        &state,
        &tenant,
        &run_id,
        approve,
        payload.reason,
        tenant.attribution(),
    )
    .await?;
    let mut wire = serde_json::to_value(&record).map_err(|e| ApiError::internal(e.to_string()))?;
    if let Some(hours) = hours {
        let now = Utc::now();
        let mut made = Vec::new();
        for request in &record.requests {
            let Some(tool) = request.get("tool").and_then(Value::as_str) else {
                continue;
            };
            let standing = StandingApproval {
                id: uuid::Uuid::new_v4().to_string(),
                tenant: tenant.tenant().to_owned(),
                assistant_id: record.assistant_id.clone(),
                tool: tool.to_owned(),
                arguments: request.get("arguments").cloned().unwrap_or(Value::Null),
                by: tenant.attribution(),
                from_run: run_id.clone(),
                made_at: now,
                until: now + chrono::Duration::hours(hours as i64),
                uses: 0,
            };
            state.run_deps.approvals.persist_standing(&standing).await;
            made.push(serde_json::to_value(&standing).unwrap_or(Value::Null));
        }
        tracing::info!(%run_id, standing = made.len(), hours, "standing approval made");
        wire["standing"] = Value::Array(made);
    }
    Ok(Json(wire))
}

/// `GET /approvals/standing` — the tenant's standing approvals.
pub(crate) async fn list_standing(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
) -> Result<Json<Value>, ApiError> {
    let now = Utc::now();
    let standing: Vec<Value> = state
        .run_deps
        .approvals
        .list_standing(tenant.tenant())
        .into_iter()
        .map(|s| {
            let live = now < s.until;
            let mut v = serde_json::to_value(&s).unwrap_or(Value::Null);
            v["live"] = json!(live);
            v
        })
        .collect();
    Ok(Json(json!({ "standing": standing })))
}

/// `DELETE /approvals/standing/{id}` — withdraw one; the next such call asks.
pub(crate) async fn withdraw_standing(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    if !state
        .run_deps
        .approvals
        .remove_standing(tenant.tenant(), &id)
    {
        return Err(ApiError::not_found(format!(
            "standing approval `{id}` not found"
        )));
    }
    Ok(Json(json!({ "withdrawn": true, "id": id })))
}

/// The standing decider: every pause is announced to it; a pause every
/// request of which a live standing approval covers is approved on the
/// spot, under the name of the person whose approval stands, and the Inbox
/// never sees it. A pause the standing approvals do not cover in full asks.
pub(crate) fn spawn_standing_decider(state: Arc<AppState>) {
    let Some(mut rx) = state.run_deps.approvals.take_paused() else {
        return;
    };
    tokio::spawn(async move {
        while let Some((tenant, run_id)) = rx.recv().await {
            let Some(record) = state.run_deps.approvals.load(&run_id) else {
                continue;
            };
            if record.status != "pending" {
                continue;
            }
            let Some(standing) = state.run_deps.approvals.standing_for(&tenant, &record) else {
                continue;
            };
            let first = standing.first().cloned();
            let Some(first) = first else { continue };
            let by_name = first
                .by
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("a person")
                .to_owned();
            let by = json!({
                "kind": "standing",
                "principal_id": first.by.get("principal_id").cloned().unwrap_or(Value::Null),
                "name": format!("{by_name}'s standing approval"),
                "standing_id": first.id,
            });
            let reason = format!(
                "a standing approval {by_name} made on {} for this call, good until {}",
                first.made_at.format("%b %-d"),
                first.until.format("%b %-d %H:%M UTC")
            );
            let context = crate::auth::TenantContext::new(tenant.clone(), Vec::new());
            match decide_run(&state, &context, &run_id, true, Some(reason), by).await {
                Ok(_) => {
                    for mut s in standing {
                        s.uses += 1;
                        state.run_deps.approvals.persist_standing(&s).await;
                    }
                    tracing::info!(%run_id, "approved by a standing approval");
                }
                Err(error) => {
                    tracing::warn!(%run_id, ?error, "standing approval could not decide the pause")
                }
            }
        }
    });
}

/// The decision itself, whoever makes it: a person from the Inbox, or an
/// evaluation standing in for one when the run is in a world. `by` is
/// the attribution the record and the resumed run carry.
pub(crate) async fn decide_run(
    state: &Arc<AppState>,
    tenant: &TenantContext,
    run_id: &str,
    approve: bool,
    reason: Option<String>,
    by: Value,
) -> Result<ApprovalRecord, ApiError> {
    let mut record = state.run_deps.approvals.load(run_id).ok_or_else(|| {
        ApiError::not_found(format!("run `{run_id}` is not waiting for approval"))
    })?;
    if !owned(state, tenant, &record).await {
        return Err(ApiError::not_found(format!(
            "run `{run_id}` is not waiting for approval"
        )));
    }
    if record.status != "pending" {
        return Err(ApiError::conflict(format!(
            "run `{run_id}` was already {} by {}",
            record.status,
            record
                .decided_by
                .as_ref()
                .and_then(|b| b.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("someone")
        )));
    }
    let by_name = by
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("a person")
        .to_owned();
    let call_ids: Vec<Value> = record
        .requests
        .iter()
        .filter_map(|r| r.get("call_id").cloned())
        .collect();
    let mut resume = json!({
        "kind": rusty_agent_runtime::react::APPROVAL_INTERRUPT_KIND,
        "decision": if approve { "approve" } else { "deny" },
        "by": by_name,
        "reason": reason.clone().unwrap_or_default(),
        "call_ids": call_ids,
    });
    if approve {
        // One token per request, scoped to its effect id: an approval for
        // one occurrence cannot launder another, and none is minted for a
        // call the run did not ask about.
        let tokens: Vec<Value> = record
            .requests
            .iter()
            .filter_map(|r| r.get("effect_id").and_then(Value::as_str))
            .map(|effect_id| json!({"effect_id": effect_id, "approved_by": format!("{by_name} ({})", by.get("principal_id").and_then(Value::as_str).unwrap_or("?"))}))
            .collect();
        resume["tokens"] = Value::Array(tokens);
    }
    // The resumed run is the paused run continued, not the decider's own
    // conversation: it keeps the channel that fired the original (a
    // schedule's run stays nobody's) and the person it was on behalf of (a
    // person's run stays theirs whoever approved it). Without this, what the
    // resumed run remembers lands in the approver's memory, and the next
    // scheduled run cannot see what its predecessor did.
    let mut metadata = serde_json::Map::new();
    metadata.insert("approval_of".to_owned(), json!(run_id));
    metadata.insert(
        "decision".to_owned(),
        json!(if approve { "approve" } else { "deny" }),
    );
    if let Some(original) = crate::routes::recall_run(state, tenant, run_id).await? {
        if let Some(Value::Object(theirs)) = original.metadata {
            for key in [
                "cron_id",
                "trigger_id",
                "channel",
                "trigger",
                "pool",
                "task_id",
                "assignment_id",
                "round",
            ] {
                if let Some(value) = theirs.get(key) {
                    metadata.insert(key.to_owned(), value.clone());
                }
            }
            if let Some(on_behalf_of) = theirs
                .get("on_behalf_of")
                .or_else(|| theirs.get("created_by"))
            {
                metadata.insert("on_behalf_of".to_owned(), on_behalf_of.clone());
            }
        }
    }
    let assignment_id = metadata
        .get("assignment_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    // And it continues under the configuration it paused under: the world
    // it acts in, the tools it must not call, its allowlist and budget. A
    // run tried in a world that resumed in the live system would carry the
    // approved effect to the very system the world stands in for.
    let config = match state.server_store.get_accepted_run(run_id).await {
        Ok(Some(accepted)) if accepted.tenant == tenant.tenant() => accepted.payload.config.clone(),
        Ok(_) => None,
        Err(error) => {
            tracing::warn!(%run_id, %error, "approval: the paused run's configuration could not be read; the resumed run continues under the defaults");
            None
        }
    };
    let run_payload = RunPayload {
        command: Some(CommandPayload {
            resume: Some(resume),
        }),
        assistant_id: record.assistant_id.clone(),
        metadata: Some(Value::Object(metadata)),
        config,
        ..RunPayload::default()
    };
    let scheduled = crate::routes::schedule_for_thread(
        state,
        tenant,
        &record.thread_id,
        run_payload,
        crate::routes::Admission::Server("resume"),
    )
    .await?;
    if let (Some(plane), Some(id)) = (&state.run_deps.assignments, &assignment_id) {
        plane.bind(&scheduled.run_id, id);
    }
    record.status = if approve { "approved" } else { "denied" }.to_owned();
    record.decided_at = Some(Utc::now());
    record.decided_by = Some(by);
    record.reason = reason.filter(|r| !r.trim().is_empty());
    record.resumed_run_id = Some(scheduled.run_id.clone());
    state.run_deps.approvals.persist(&record).await;
    // The notice that asked for this decision has its answer.
    crate::notices::settle(state, tenant.tenant(), &format!("approval:{run_id}")).await;
    tracing::info!(%run_id, resumed = %scheduled.run_id, decision = %record.status, "approval decided");
    Ok(record)
}
