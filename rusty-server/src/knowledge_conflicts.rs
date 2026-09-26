//! Fact conflicts: two knowledge sources disagree on a claim. An agent
//! that notices while working flags it (`knowledge.flag_conflict`), or a
//! person does from the Knowledge page. Until a person rules, every search
//! hit from either source carries the conflict and the agent is told to
//! state neither side as fact — the contested claim is withheld from the
//! answer, said to be contested, and the decision named as a person's. A
//! person rules which source stands; from then on the losing source's hits
//! say they were overruled on that claim.
use std::sync::Arc;

use axum::extract::{Path, State as AxumState};
use axum::{Extension, Json};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::auth::TenantContext;
use crate::error::ApiError;
use crate::routes::AppState;
use crate::server_store::ServerStore;

const NAMESPACE: &str = "knowledge_conflicts";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KnowledgeConflict {
    pub conflict_id: String,
    pub tenant: String,
    pub source_a: String,
    pub source_b: String,
    pub title_a: String,
    pub title_b: String,
    /// What they disagree about, in one line.
    pub claim: String,
    pub a_says: String,
    pub b_says: String,
    /// How it was found: the run, the record, the date.
    pub why: String,
    /// `{kind: agent, agent_id, run_id}` or the person's attribution.
    pub filed_by: Value,
    pub filed_at: DateTime<Utc>,
    /// open | ruled
    pub state: String,
    /// The source that stands, once ruled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stands: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ruled_by: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ruled_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl KnowledgeConflict {
    fn names(&self, source_id: &str) -> bool {
        self.source_a == source_id || self.source_b == source_id
    }
    fn same_pair(&self, a: &str, b: &str) -> bool {
        (self.source_a == a && self.source_b == b) || (self.source_a == b && self.source_b == a)
    }
    fn other(&self, source_id: &str) -> (&str, &str, &str, &str) {
        // (other title, this says, other says, other id)
        if self.source_a == source_id {
            (&self.title_b, &self.a_says, &self.b_says, &self.source_b)
        } else {
            (&self.title_a, &self.b_says, &self.a_says, &self.source_a)
        }
    }
}

pub(crate) async fn all_in(store: &Arc<dyn ServerStore>, tenant: &str) -> Vec<KnowledgeConflict> {
    let Ok(items) = store.kv_list(NAMESPACE).await else { return Vec::new() };
    let mut out: Vec<KnowledgeConflict> = items.into_iter().filter_map(|i| serde_json::from_value(i.value).ok()).filter(|c: &KnowledgeConflict| c.tenant == tenant).collect();
    out.sort_by(|a, b| (a.state != "open").cmp(&(b.state != "open")).then_with(|| b.filed_at.cmp(&a.filed_at)));
    out
}

async fn keep(store: &Arc<dyn ServerStore>, conflict: &KnowledgeConflict) -> Result<(), String> {
    store.kv_put(NAMESPACE, &conflict.conflict_id, serde_json::to_value(conflict).map_err(|e| e.to_string())?).await.map(|_| ()).map_err(|e| e.to_string())
}

/// What a filing needs, from a person or an agent.
#[derive(Debug, Deserialize)]
pub(crate) struct FilePayload {
    pub source_a: String,
    pub source_b: String,
    pub claim: String,
    pub a_says: String,
    pub b_says: String,
    #[serde(default)]
    pub why: String,
}

/// File a conflict: both sources held and different, one open conflict per
/// pair. Answers the conflict, or the open one already filed for the pair.
pub(crate) async fn file(state: &AppState, tenant: &TenantContext, payload: FilePayload, filed_by: Value) -> Result<(KnowledgeConflict, bool), ApiError> {
    let (a, b) = (payload.source_a.trim().to_owned(), payload.source_b.trim().to_owned());
    let claim = payload.claim.trim().chars().take(300).collect::<String>();
    if a.is_empty() || b.is_empty() || a == b {
        return Err(ApiError::bad_request("name two different sources, by the source_id a citation carries".to_owned()));
    }
    if claim.is_empty() || payload.a_says.trim().is_empty() || payload.b_says.trim().is_empty() {
        return Err(ApiError::bad_request("say what they disagree about and what each says".to_owned()));
    }
    let base = crate::knowledge::knowledge_base(state, tenant);
    let mut titles = Vec::new();
    for id in [&a, &b] {
        let versions = base.versions_of(id).await.map_err(|e| ApiError::internal(e.to_string()))?;
        let Some(latest) = versions.last() else {
            return Err(ApiError::not_found(format!("no knowledge source `{id}` — use the source_id exactly as the citation carries it")));
        };
        titles.push(latest.title.clone());
    }
    let held = all_in(&state.server_store, tenant.tenant()).await;
    if let Some(open) = held.into_iter().find(|c| c.state == "open" && c.same_pair(&a, &b)) {
        return Ok((open, false));
    }
    let conflict = KnowledgeConflict {
        conflict_id: uuid::Uuid::new_v4().to_string(),
        tenant: tenant.tenant().to_owned(),
        source_a: a,
        source_b: b,
        title_a: titles[0].clone(),
        title_b: titles[1].clone(),
        claim,
        a_says: payload.a_says.trim().chars().take(600).collect(),
        b_says: payload.b_says.trim().chars().take(600).collect(),
        why: payload.why.trim().chars().take(2_000).collect(),
        filed_by,
        filed_at: Utc::now(),
        state: "open".to_owned(),
        stands: None,
        ruled_by: None,
        ruled_at: None,
        note: None,
    };
    keep(&state.server_store, &conflict).await.map_err(ApiError::internal)?;
    Ok((conflict, true))
}

/// Mark search hits from sources in a conflict, and say what to do: an open
/// conflict withholds the claim (state neither side as fact); a ruled one
/// names the source that stands. `results` are rendered hits carrying
/// `citation.source_id`; answers the note to add, if any.
pub(crate) async fn annotate(store: &Arc<dyn ServerStore>, tenant: &str, results: &mut [Value]) -> Option<String> {
    let conflicts = all_in(store, tenant).await;
    if conflicts.is_empty() {
        return None;
    }
    let mut contested: Vec<String> = Vec::new();
    let mut overruled: Vec<String> = Vec::new();
    for hit in results.iter_mut() {
        let Some(source_id) = hit.get("citation").and_then(|c| c.get("source_id")).and_then(Value::as_str).map(str::to_owned) else { continue };
        for c in conflicts.iter().filter(|c| c.names(&source_id)) {
            let (other_title, this_says, other_says, other_id) = c.other(&source_id);
            match (c.state.as_str(), c.stands.as_deref()) {
                ("open", _) => {
                    hit["contested"] = json!({"conflict_id": c.conflict_id, "claim": c.claim, "this_says": this_says, "other_source": other_title, "other_says": other_says});
                    let line = format!("\"{}\" — {} and {} disagree", c.claim, c.title_a, c.title_b);
                    if !contested.contains(&line) {
                        contested.push(line);
                    }
                }
                ("ruled", Some(stands)) if stands == other_id => {
                    hit["overruled"] = json!({"conflict_id": c.conflict_id, "claim": c.claim, "stands": other_title, "stands_says": other_says, "note": c.note});
                    let line = format!("on \"{}\", {} stands over {}", c.claim, other_title, if c.source_a == source_id { &c.title_a } else { &c.title_b });
                    if !overruled.contains(&line) {
                        overruled.push(line);
                    }
                }
                ("ruled", Some(stands)) if stands == source_id => {
                    hit["upheld"] = json!({"conflict_id": c.conflict_id, "claim": c.claim, "over": other_title});
                }
                _ => {}
            }
        }
    }
    let mut notes = Vec::new();
    if !contested.is_empty() {
        notes.push(format!("CONTESTED — {}. A person has not ruled yet: do not state either side of a contested claim as fact. Say the sources disagree, what each says, and that a person decides on the Knowledge page.", contested.join("; ")));
    }
    if !overruled.is_empty() {
        notes.push(format!("RULED — a person ruled {}: follow the source that stands on that claim and not the other.", overruled.join("; ")));
    }
    (!notes.is_empty()).then(|| notes.join(" "))
}

/// `GET /knowledge/conflicts` — open first.
pub(crate) async fn list_conflicts(AxumState(state): AxumState<Arc<AppState>>, Extension(tenant): Extension<TenantContext>) -> Result<Json<Value>, ApiError> {
    let conflicts = all_in(&state.server_store, tenant.tenant()).await;
    let open = conflicts.iter().filter(|c| c.state == "open").count();
    Ok(Json(json!({"conflicts": conflicts, "open": open})))
}

/// `POST /knowledge/conflicts` — a person files one.
pub(crate) async fn post_conflict(AxumState(state): AxumState<Arc<AppState>>, Extension(tenant): Extension<TenantContext>, Json(payload): Json<FilePayload>) -> Result<(axum::http::StatusCode, Json<Value>), ApiError> {
    let by = tenant.attribution();
    let (conflict, created) = file(&state, &tenant, payload, by).await?;
    let status = if created { axum::http::StatusCode::CREATED } else { axum::http::StatusCode::OK };
    Ok((status, Json(json!({"conflict": conflict, "created": created}))))
}

#[derive(Debug, Deserialize)]
pub(crate) struct RulePayload {
    /// The source_id that stands.
    pub stands: String,
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /knowledge/conflicts/{id}/rule {stands, note?}` — a person rules.
pub(crate) async fn rule_conflict(AxumState(state): AxumState<Arc<AppState>>, Extension(tenant): Extension<TenantContext>, Path(id): Path<String>, Json(payload): Json<RulePayload>) -> Result<Json<Value>, ApiError> {
    let mut conflict = all_in(&state.server_store, tenant.tenant())
        .await
        .into_iter()
        .find(|c| c.conflict_id == id)
        .ok_or_else(|| ApiError::not_found(format!("unknown conflict `{id}`")))?;
    if conflict.state != "open" {
        return Err(ApiError::conflict(format!("the conflict is already {}", conflict.state)));
    }
    let stands = payload.stands.trim().to_owned();
    if !conflict.names(&stands) {
        return Err(ApiError::bad_request(format!("`{stands}` is not one of the two sources — rule `{}` or `{}`", conflict.source_a, conflict.source_b)));
    }
    conflict.state = "ruled".to_owned();
    conflict.stands = Some(stands);
    conflict.ruled_by = Some(tenant.attribution());
    conflict.ruled_at = Some(Utc::now());
    conflict.note = payload.note.map(|n| n.trim().chars().take(600).collect::<String>()).filter(|n| !n.is_empty());
    keep(&state.server_store, &conflict).await.map_err(ApiError::internal)?;
    Ok(Json(json!({"ruled": true, "conflict": conflict})))
}
