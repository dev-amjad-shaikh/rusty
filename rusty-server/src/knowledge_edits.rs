//! Suggested edits to knowledge: an agent that finds a source wrong while
//! working proposes the corrected body with its reason; a person accepts
//! it — which mints the superseding version, authored by the person on
//! the agent's proposal — or declines it with a reason the record keeps.
//! The store's rule holds: a correction supersedes, never overwrites, and
//! nobody's agent rewrites what people read without a person's word.
use std::sync::Arc;

use axum::extract::{Path, State as AxumState};
use axum::{Extension, Json};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::auth::TenantContext;
use crate::error::ApiError;
use crate::routes::AppState;

const NAMESPACE: &str = "knowledge_edits";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KnowledgeEdit {
    pub edit_id: String,
    pub tenant: String,
    pub source_id: String,
    pub title: String,
    /// The proposed body, whole — the version a person would mint.
    pub body: String,
    /// Why, in the agent's words: what it found, and how.
    pub why: String,
    /// `{kind: agent, agent_id, run_id}`.
    pub proposed_by: Value,
    pub proposed_at: DateTime<Utc>,
    /// waiting | accepted | declined
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_by: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The version accepting it minted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u64>,
}

pub(crate) async fn all(state: &AppState, tenant: &str) -> Vec<KnowledgeEdit> {
    let Ok(items) = state.server_store.kv_list(NAMESPACE).await else {
        return Vec::new();
    };
    let mut out: Vec<KnowledgeEdit> = items
        .into_iter()
        .filter_map(|i| serde_json::from_value(i.value).ok())
        .filter(|e: &KnowledgeEdit| e.tenant == tenant)
        .collect();
    out.sort_by(|a, b| {
        (a.state != "waiting")
            .cmp(&(b.state != "waiting"))
            .then_with(|| b.proposed_at.cmp(&a.proposed_at))
    });
    out
}

pub(crate) async fn keep(state: &AppState, edit: &KnowledgeEdit) -> Result<(), String> {
    state
        .server_store
        .kv_put(
            NAMESPACE,
            &edit.edit_id,
            serde_json::to_value(edit).map_err(|e| e.to_string())?,
        )
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

async fn load(state: &AppState, tenant: &str, id: &str) -> Result<KnowledgeEdit, ApiError> {
    state
        .server_store
        .kv_get(NAMESPACE, id)
        .await
        .map_err(ApiError::internal)?
        .and_then(|i| serde_json::from_value::<KnowledgeEdit>(i.value).ok())
        .filter(|e| e.tenant == tenant)
        .ok_or_else(|| ApiError::not_found(format!("unknown suggested edit `{id}`")))
}

/// `GET /knowledge/edits` — the suggestions, waiting first.
pub(crate) async fn list_edits(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
) -> Result<Json<Value>, ApiError> {
    let edits = all(&state, tenant.tenant()).await;
    let waiting = edits.iter().filter(|e| e.state == "waiting").count();
    Ok(Json(json!({"edits": edits, "waiting": waiting})))
}

/// `POST /knowledge/edits/{id}/accept` — the person accepts: the source's
/// superseding version is minted with the proposed body, authored by the
/// person on the agent's proposal.
pub(crate) async fn accept_edit(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let mut edit = load(&state, tenant.tenant(), &id).await?;
    if edit.state != "waiting" {
        return Err(ApiError::conflict(format!(
            "the suggestion is already {}",
            edit.state
        )));
    }
    let base = crate::knowledge::knowledge_base(&state, &tenant);
    if base
        .versions_of(&edit.source_id)
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?
        .is_empty()
    {
        return Err(ApiError::conflict(format!(
            "the source `{}` is no longer held — decline the suggestion",
            edit.source_id
        )));
    }
    let agent = edit
        .proposed_by
        .get("agent_id")
        .and_then(Value::as_str)
        .unwrap_or("an agent")
        .to_owned();
    let author = format!(
        "human:{} — accepting agent:{agent}'s suggestion",
        tenant.principal().id
    );
    let now = Utc::now();
    let source = base
        .correct_source(&edit.source_id, &author, &edit.body, now)
        .await
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    edit.state = "accepted".to_owned();
    edit.decided_by = Some(tenant.attribution());
    edit.decided_at = Some(now);
    edit.version = Some(source.version);
    keep(&state, &edit).await.map_err(ApiError::internal)?;
    Ok(Json(
        json!({"accepted": true, "edit": edit, "content_hash": source.content_hash, "version": source.version}),
    ))
}

#[derive(Debug, Deserialize)]
pub(crate) struct DeclinePayload {
    pub reason: String,
}

/// `POST /knowledge/edits/{id}/decline` — the person declines, with a reason the record keeps.
pub(crate) async fn decline_edit(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(id): Path<String>,
    Json(payload): Json<DeclinePayload>,
) -> Result<Json<Value>, ApiError> {
    let mut edit = load(&state, tenant.tenant(), &id).await?;
    if edit.state != "waiting" {
        return Err(ApiError::conflict(format!(
            "the suggestion is already {}",
            edit.state
        )));
    }
    let reason = payload.reason.trim().to_owned();
    if reason.is_empty() {
        return Err(ApiError::bad_request(
            "say why: the reason is what the agent reads next time".to_owned(),
        ));
    }
    edit.state = "declined".to_owned();
    edit.decided_by = Some(tenant.attribution());
    edit.decided_at = Some(Utc::now());
    edit.reason = Some(reason);
    keep(&state, &edit).await.map_err(ApiError::internal)?;
    Ok(Json(json!({"declined": true, "edit": edit})))
}
