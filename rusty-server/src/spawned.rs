//! The fleet an agent made: agents spawned by agents (`agents.spawn`),
//! listed with their makers and their idleness, and retired when idle —
//! put away (archived, so a person can bring one back), what they learned
//! folded into their maker's memory, and their maker's owner told. A
//! maintainer spawned per article, or a probe spawned for one question,
//! otherwise lingers in the rail for good.

use std::sync::Arc;

use axum::extract::State as AxumState;
use axum::{Extension, Json};
use chrono::{DateTime, Utc};
use rusty_agent_runtime::memory::{
    MemoryKind, MemoryQuery, MemoryRecord, MemoryScope, ScopeAddress,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::TenantContext;
use crate::error::ApiError;
use crate::routes::AppState;

/// Days without a run after which the nightly sweep retires a spawned
/// agent nobody published — the default until a person sets the
/// workspace's own; a person retiring by hand names their own.
pub const RETIRE_IDLE_DAYS_DEFAULT: u64 = 14;

const SETTINGS_NAMESPACE: &str = "spawned_settings";

/// The workspace's nightly threshold, in days; `0` means the sweep
/// retires nothing on its own. Kept per tenant on the kv.
pub(crate) async fn retire_idle_days(state: &AppState, tenant: &str) -> u64 {
    state
        .server_store
        .kv_get(SETTINGS_NAMESPACE, tenant)
        .await
        .ok()
        .flatten()
        .and_then(|item| item.value.get("retire_idle_days").and_then(Value::as_u64))
        .unwrap_or(RETIRE_IDLE_DAYS_DEFAULT)
}

#[derive(Debug, Deserialize)]
pub struct SettingsPayload {
    /// Nightly: retire spawned agents idle at least this many days; 0 never.
    pub retire_idle_days: u64,
}

/// `PUT /estate/spawned/settings {retire_idle_days}` — the nightly
/// threshold, the workspace's to set (0 switches the nightly retirement off).
pub(crate) async fn put_settings(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(payload): Json<SettingsPayload>,
) -> Result<Json<Value>, ApiError> {
    if payload.retire_idle_days > 3650 {
        return Err(ApiError::bad_request(
            "the threshold is in days — at most ten years".to_owned(),
        ));
    }
    state
        .server_store
        .kv_put(SETTINGS_NAMESPACE, tenant.tenant(), json!({"retire_idle_days": payload.retire_idle_days, "set_by": tenant.attribution(), "set_at": Utc::now()}))
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(
        json!({ "retire_idle_days": payload.retire_idle_days }),
    ))
}

/// One spawned agent as the estate lists it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SpawnedRow {
    pub assistant_id: String,
    pub name: String,
    pub spawned_by: Value,
    pub created_at: DateTime<Utc>,
    pub archived_at: Option<DateTime<Utc>>,
    pub last_run_at: Option<DateTime<Utc>>,
    /// Days since its last run — or since it was made, when it never ran.
    pub idle_days: u64,
    /// The schedules that fire it: interval or cron expression, and when
    /// each last fired. Empty when it runs only when asked.
    pub schedules: Vec<Value>,
    /// Live notes in its own memory — what a retirement would fold.
    pub notes: usize,
    /// Gaps it filed that are still open.
    pub open_gaps: usize,
    #[serde(skip)]
    pub active_version_id: String,
    #[serde(skip)]
    pub created_by: Value,
}

/// Every agent of the tenant an agent spawned, newest last run first.
pub(crate) async fn list(
    state: &Arc<AppState>,
    tenant: &TenantContext,
    now: DateTime<Utc>,
) -> Result<Vec<SpawnedRow>, ApiError> {
    let mut rows = Vec::new();
    let crons = state
        .server_store
        .list_crons()
        .await
        .map_err(ApiError::internal)?;
    // Open gaps by who filed them, counted once for the whole roster.
    let mut open_gaps: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    if let Ok(ledger) = crate::routes::load_gap_ledger(state, tenant.tenant()).await {
        for entry in ledger
            .entries()
            .filter(|e| e.status != rusty_agent_runtime::gaps::GapStatus::Closed)
        {
            if let Some(filer) = ledger.filer(&entry.gap_id) {
                *open_gaps.entry(filer.to_owned()).or_default() += 1;
            }
        }
    }
    for record in state
        .server_store
        .list_assistants()
        .await
        .map_err(ApiError::internal)?
    {
        let Some(external) = tenant.unscope(&record.assistant_id).map(str::to_owned) else {
            continue;
        };
        let Some(spawned_by) = record
            .metadata
            .get("spawned_by")
            .cloned()
            .filter(|v| !v.is_null())
        else {
            continue;
        };
        let last_run_at = crate::routes::recall_runs_for(state, tenant, 1, Some(&external))
            .await?
            .first()
            .and_then(|r| r.created_at);
        let since = last_run_at.unwrap_or(record.created_at);
        let idle_days = (now - since).num_days().max(0) as u64;
        let schedules: Vec<Value> = crons
            .iter()
            .filter(|c| c.assistant_id.as_deref().map(|id| tenant.unscope(id).unwrap_or(id)) == Some(external.as_str()))
            .map(|c| serde_json::json!({"interval_secs": c.interval_secs, "cron_expr": c.cron_expr, "last_run_at": c.last_run_at, "runs_fired": c.runs_fired}))
            .collect();
        let notes = {
            use rusty_agent_runtime::memory::{MemoryQuery, MemoryScope, ScopeAddress};
            let query = MemoryQuery {
                scope: Some(ScopeAddress::new(MemoryScope::Agent, external.clone())),
                ..Default::default()
            };
            state
                .server_store
                .query_memory(tenant.tenant(), &query, now)
                .await
                .map(|v| v.len())
                .unwrap_or(0)
        };
        let gaps_open = open_gaps.get(&external).copied().unwrap_or(0)
            + if record.assistant_id != external {
                open_gaps.get(&record.assistant_id).copied().unwrap_or(0)
            } else {
                0
            };
        rows.push(SpawnedRow {
            assistant_id: external,
            name: record.name.clone(),
            spawned_by,
            created_at: record.created_at,
            archived_at: record.archived_at,
            last_run_at,
            idle_days,
            schedules,
            notes,
            open_gaps: gaps_open,
            active_version_id: record.active_version_id.clone(),
            created_by: record
                .metadata
                .get("created_by")
                .cloned()
                .unwrap_or(Value::Null),
        });
    }
    rows.sort_by(|a, b| {
        b.last_run_at
            .cmp(&a.last_run_at)
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(rows)
}

/// Retire every live spawned agent idle for `idle_days` or more: archive
/// it, fold its notes into its maker's memory, tell its maker's owner.
/// Answers what was retired, with how many notes each folded.
pub(crate) async fn retire_idle(
    state: &Arc<AppState>,
    tenant: &TenantContext,
    idle_days: u64,
    by: &Value,
    now: DateTime<Utc>,
) -> Result<Vec<Value>, ApiError> {
    let mut retired = Vec::new();
    for row in list(state, tenant, now).await? {
        if row.archived_at.is_some() || row.idle_days < idle_days {
            continue;
        }
        let internal = tenant.scope(&row.assistant_id);
        state
            .server_store
            .set_assistant_archived(&internal, &row.active_version_id, true, now)
            .await
            .map_err(ApiError::internal)?;
        let maker = row
            .spawned_by
            .get("assistant_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let folded = match &maker {
            Some(maker) => {
                fold_notes(state, tenant.tenant(), &row.assistant_id, maker, now).await?
            }
            None => 0,
        };
        let maker_name = match &maker {
            Some(id) => state
                .server_store
                .get_assistant(&tenant.scope(id))
                .await
                .ok()
                .flatten()
                .map(|a| a.name)
                .unwrap_or_else(|| id.clone()),
            None => "nobody".to_owned(),
        };
        // The maker's owner is told — the person the maker was made by —
        // else whoever retired it.
        let maker_owner = match &maker {
            Some(id) => state
                .server_store
                .get_assistant(&tenant.scope(id))
                .await
                .ok()
                .flatten()
                .and_then(|a| a.metadata.get("created_by").cloned())
                .filter(|v| v.get("principal_id").is_some()),
            None => None,
        };
        let to = maker_owner
            .or_else(|| {
                row.created_by
                    .get("principal_id")
                    .is_some()
                    .then(|| row.created_by.clone())
            })
            .unwrap_or_else(|| by.clone());
        let title = format!(
            "{} retired after {} idle day{}",
            row.name,
            row.idle_days,
            if row.idle_days == 1 { "" } else { "s" }
        );
        let text = format!(
            "{} — spawned by {} — had not run for {} day{}. It is put away (Archived in the Agents rail; Restore brings it back){}.",
            row.name, maker_name, row.idle_days, if row.idle_days == 1 { "" } else { "s" },
            if folded > 0 { format!("; {folded} note{} it learned now sit in {maker_name}'s memory, marked folded", if folded == 1 { "" } else { "s" }) } else { String::new() },
        );
        crate::notices::tell(
            state,
            tenant.tenant(),
            &to,
            &format!("spawned-retired:{}", row.assistant_id),
            json!({"kind": "assistant", "assistant_id": row.assistant_id}),
            &title,
            &text,
        )
        .await;
        tracing::info!(agent = %row.assistant_id, idle_days = row.idle_days, folded, "spawned agent retired");
        retired.push(json!({
            "assistant_id": row.assistant_id,
            "name": row.name,
            "spawned_by": row.spawned_by,
            "idle_days": row.idle_days,
            "notes_folded": folded,
            "told": to,
        }));
    }
    Ok(retired)
}

/// The retired agent's live facts and preferences, copied into its
/// maker's scope — author and evidence kept, marked `folded` with where
/// from, one priority lower, never twice. Blocks and summaries stay with
/// the retired agent: they are its working notes, not what it learned.
async fn fold_notes(
    state: &Arc<AppState>,
    tenant: &str,
    from: &str,
    into: &str,
    now: DateTime<Utc>,
) -> Result<usize, ApiError> {
    let theirs = state
        .server_store
        .query_memory(
            tenant,
            &MemoryQuery {
                scope: Some(ScopeAddress::new(MemoryScope::Agent, from)),
                ..Default::default()
            },
            now,
        )
        .await
        .map_err(ApiError::internal)?;
    let held = state
        .server_store
        .query_memory(
            tenant,
            &MemoryQuery {
                scope: Some(ScopeAddress::new(MemoryScope::Agent, into)),
                ..Default::default()
            },
            now,
        )
        .await
        .map_err(ApiError::internal)?;
    let from_tag = format!("from:{from}");
    let mut folded = 0usize;
    for record in theirs {
        if !matches!(record.kind, MemoryKind::Fact | MemoryKind::Preference)
            || record
                .key
                .as_deref()
                .is_some_and(|k| k.starts_with("block."))
        {
            continue;
        }
        let content = match &record.content {
            rusty_agent_runtime::record::PayloadRef::Inline(v) => v.clone(),
            _ => continue,
        };
        let already = held.iter().any(|h| h.tags.iter().any(|t| t == &from_tag) && matches!(&h.content, rusty_agent_runtime::record::PayloadRef::Inline(v) if v == &content));
        if already {
            continue;
        }
        // A record's address is its content and provenance, not its scope:
        // the copy is written now, so it is its own record in the maker's
        // scope — the author and the evidence stay the retired agent's.
        let provenance = rusty_agent_runtime::memory::MemoryProvenance {
            written_at: now,
            ..record.provenance.clone()
        };
        let mut copy = MemoryRecord::new(
            record.kind,
            ScopeAddress::new(MemoryScope::Agent, into),
            provenance,
            record.confidence,
            record.validity.clone(),
            now,
            content.clone(),
        )
        .map_err(|e| ApiError::internal(e.to_string()))?
        .with_priority((record.priority - 1).max(0))
        .with_tags(
            record
                .tags
                .iter()
                .cloned()
                .chain(["folded".to_owned(), from_tag.clone()]),
        );
        if let Some(key) = &record.key {
            copy = copy.with_key(format!("folded.{}.{key}", &from[..from.len().min(8)]));
        }
        state
            .server_store
            .put_memory(tenant, &copy, &content)
            .await
            .map_err(ApiError::internal)?;
        folded += 1;
    }
    Ok(folded)
}

/// `GET /estate/spawned` — the fleet agents made, with makers and idleness.
pub(crate) async fn get_spawned(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
) -> Result<Json<Value>, ApiError> {
    let rows = list(&state, &tenant, Utc::now()).await?;
    let nightly = retire_idle_days(&state, tenant.tenant()).await;
    Ok(Json(
        json!({ "spawned": rows, "retire_idle_days": nightly, "retire_idle_days_default": RETIRE_IDLE_DAYS_DEFAULT }),
    ))
}

#[derive(Debug, Deserialize)]
pub struct RetirePayload {
    /// Retire the spawned agents idle for at least this many days.
    pub idle_days: u64,
}

/// `POST /estate/spawned/retire {idle_days}` — retire the idle ones now,
/// as the nightly sweep would with the default.
pub(crate) async fn post_retire(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(payload): Json<RetirePayload>,
) -> Result<Json<Value>, ApiError> {
    let by = tenant.attribution();
    let retired = retire_idle(&state, &tenant, payload.idle_days, &by, Utc::now()).await?;
    Ok(Json(
        json!({ "idle_days": payload.idle_days, "retired": retired }),
    ))
}
