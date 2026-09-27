//! Notices: the platform telling a person something — the work they
//! delegated is done, waits for them, is blocked, was cancelled — kept
//! apart from the record of what was done. An assignment's progress says
//! what happened; a notice says the owner was told, and when they saw it.
//! Delivery is a fact of its own, so the two can disagree honestly: work
//! finished that nobody has read yet reads as *done, not yet seen*.
//!
//! The one channel today is the studio's Inbox. A notice is written once
//! per (assignment, state, moment) — the persist point makes it, and the
//! key keeps a re-persist from telling twice.
use std::sync::Arc;

use axum::extract::{Path, State as AxumState};
use axum::{Extension, Json};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::auth::TenantContext;
use crate::error::ApiError;
use crate::routes::AppState;

const NAMESPACE_PREFIX: &str = "notices";

fn namespace(tenant: &str) -> String {
    format!("{NAMESPACE_PREFIX}:{tenant}")
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Notice {
    pub notice_id: String,
    pub tenant: String,
    /// Whom it is for: an attribution (`principal_id`, `name`, `kind`).
    pub to: Value,
    /// What it is about: `{kind: "assignment", assignment_id, state}`.
    pub about: Value,
    /// Once per fact: `assignment:<id>:<state>:<updated_at>`.
    pub key: String,
    pub title: String,
    pub text: String,
    /// The channel it went by. `inbox` is the studio's.
    pub channel: String,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seen_at: Option<DateTime<Utc>>,
}

async fn all(state: &AppState, tenant: &str) -> Vec<Notice> {
    all_in(&state.server_store, tenant).await
}

async fn all_in(
    store: &std::sync::Arc<dyn crate::server_store::ServerStore>,
    tenant: &str,
) -> Vec<Notice> {
    match store.kv_list(&namespace(tenant)).await {
        Ok(items) => items
            .into_iter()
            .filter_map(|i| serde_json::from_value(i.value).ok())
            .collect(),
        Err(error) => {
            tracing::warn!(%error, "notices could not be read");
            Vec::new()
        }
    }
}

async fn keep(state: &AppState, notice: &Notice) {
    keep_in(&state.server_store, notice).await
}

async fn keep_in(store: &std::sync::Arc<dyn crate::server_store::ServerStore>, notice: &Notice) {
    if let Err(error) = store
        .kv_put(
            &namespace(&notice.tenant),
            &notice.notice_id,
            serde_json::to_value(notice).unwrap_or(Value::Null),
        )
        .await
    {
        tracing::warn!(%error, notice = %notice.notice_id, "notice not kept");
    }
}

/// Tell `to` once about the fact `key` names. Answers the notice made, or
/// `None` when this fact was already told.
pub(crate) async fn tell(
    state: &AppState,
    tenant: &str,
    to: &Value,
    key: &str,
    about: Value,
    title: &str,
    text: &str,
) -> Option<Notice> {
    tell_in(&state.server_store, tenant, to, key, about, title, text).await
}

/// The same, from a run's own path, which holds the store and not the
/// whole state.
pub(crate) async fn tell_in(
    store: &std::sync::Arc<dyn crate::server_store::ServerStore>,
    tenant: &str,
    to: &Value,
    key: &str,
    about: Value,
    title: &str,
    text: &str,
) -> Option<Notice> {
    if all_in(store, tenant).await.iter().any(|n| n.key == key) {
        return None;
    }
    let notice = Notice {
        notice_id: uuid::Uuid::new_v4().to_string(),
        tenant: tenant.to_owned(),
        to: to.clone(),
        about,
        key: key.to_owned(),
        title: title.to_owned(),
        text: text.to_owned(),
        channel: "inbox".to_owned(),
        created_at: Utc::now(),
        seen_at: None,
    };
    keep_in(store, &notice).await;
    tracing::info!(notice = %notice.notice_id, %key, "a person was told");
    Some(notice)
}

/// The fact `key` names is settled — decided, done, gone — so every notice
/// about it stops asking: marked seen, kept as the record of the telling.
pub(crate) async fn settle(state: &AppState, tenant: &str, key: &str) {
    for mut notice in all(state, tenant)
        .await
        .into_iter()
        .filter(|n| n.key == key && n.seen_at.is_none())
    {
        notice.seen_at = Some(Utc::now());
        keep(state, &notice).await;
    }
}

/// The notices about one assignment, oldest first — its delivery record.
pub(crate) async fn for_assignment(
    state: &AppState,
    tenant: &str,
    assignment_id: &str,
) -> Vec<Notice> {
    let mut mine: Vec<Notice> = all(state, tenant)
        .await
        .into_iter()
        .filter(|n| n.about.get("assignment_id").and_then(Value::as_str) == Some(assignment_id))
        .collect();
    mine.sort_by_key(|a| a.created_at);
    mine
}

/// One word for whether the owner was told: `seen`, `delivered`, or `none`.
pub(crate) fn told(notices: &[Notice]) -> &'static str {
    match notices.last() {
        None => "none",
        Some(n) if n.seen_at.is_some() => "seen",
        Some(_) => "delivered",
    }
}

/// The delivery record as an assignment shows it.
pub(crate) fn delivery(notices: &[Notice]) -> Value {
    json!(notices.iter().map(|n| json!({"notice_id": n.notice_id, "state": n.about.get("state").cloned().unwrap_or(Value::Null), "at": n.created_at, "seen_at": n.seen_at, "channel": n.channel})).collect::<Vec<_>>())
}

fn is_for(notice: &Notice, tenant: &TenantContext) -> bool {
    let me = tenant.attribution();
    notice.to.get("principal_id") == me.get("principal_id")
}

/// `GET /notices` — the caller's notices, newest first, unseen before seen.
pub(crate) async fn list_notices(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
) -> Result<Json<Value>, ApiError> {
    let mut mine: Vec<Notice> = all(&state, tenant.tenant())
        .await
        .into_iter()
        .filter(|n| is_for(n, &tenant))
        .collect();
    mine.sort_by(|a, b| {
        a.seen_at
            .is_some()
            .cmp(&b.seen_at.is_some())
            .then(b.created_at.cmp(&a.created_at))
    });
    Ok(Json(json!({"notices": mine})))
}

/// `POST /notices/{id}/seen` — the addressee read it; the moment is kept.
pub(crate) async fn mark_seen(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let item = state
        .server_store
        .kv_get(&namespace(tenant.tenant()), &id)
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let mut notice: Notice = item
        .and_then(|i| serde_json::from_value(i.value).ok())
        .ok_or_else(|| ApiError::not_found(format!("no notice `{id}`")))?;
    if !is_for(&notice, &tenant) {
        return Err(ApiError::not_found(format!("no notice `{id}`")));
    }
    if notice.seen_at.is_none() {
        notice.seen_at = Some(Utc::now());
        keep(&state, &notice).await;
    }
    Ok(Json(serde_json::to_value(&notice).unwrap_or(Value::Null)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notice(seen: bool) -> Notice {
        Notice {
            notice_id: "n".into(),
            tenant: "t".into(),
            to: json!({"principal_id": "bob"}),
            about: json!({"kind": "assignment", "assignment_id": "a", "state": "done"}),
            key: "k".into(),
            title: "Done".into(),
            text: "".into(),
            channel: "inbox".into(),
            created_at: Utc::now(),
            seen_at: seen.then(Utc::now),
        }
    }

    #[test]
    fn told_is_the_last_notices_word() {
        assert_eq!(told(&[]), "none");
        assert_eq!(told(&[notice(false)]), "delivered");
        assert_eq!(told(&[notice(false), notice(true)]), "seen");
        let shown = delivery(&[notice(true)]);
        assert_eq!(shown[0]["state"], "done");
        assert!(shown[0]["seen_at"].is_string());
    }
}
