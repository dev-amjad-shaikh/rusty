//! The compiled knowledge unit an agent consumes: a person picks the
//! sources, the server compiles their text into one unit ranked by
//! provenance — the organization's own first, then vendor documentation,
//! then generic guidance — bounded, versioned, and carried in the agent's
//! first turn beside its charter and goal (the cached prefix). Compiling
//! is refused while any chosen source is contested (`knowledge_conflicts`):
//! a contested fact is not published into an agent. A source that lost a
//! ruling compiles with the ruling beside it. A unit goes stale when a
//! source gains a version; a conflict filed after compiling is named in
//! the unit at run time.
use std::sync::Arc;

use axum::extract::{Path, State as AxumState};
use axum::{Extension, Json};
use chrono::{DateTime, Utc};
use rusty_agent_runtime::knowledge::SourceProvenance;
use rusty_agent_runtime::memory::MemoryScope;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::auth::TenantContext;
use crate::error::ApiError;
use crate::routes::AppState;

const NAMESPACE: &str = "knowledge_units";
/// Characters a unit holds by default: enough for a few runbooks, small
/// beside the window.
pub const UNIT_CHARS_DEFAULT: usize = 12_000;
const UNIT_CHARS_MAX: usize = 60_000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UnitSource {
    pub source_id: String,
    pub title: String,
    pub provenance: SourceProvenance,
    pub version: u64,
    pub content_hash: String,
    /// Characters of it the unit carries, and whether it was cut.
    pub chars: usize,
    pub cut: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KnowledgeUnit {
    pub tenant: String,
    pub agent_id: String,
    pub version: u64,
    pub compiled_at: DateTime<Utc>,
    pub compiled_by: Value,
    pub char_limit: usize,
    pub sources: Vec<UnitSource>,
    pub text: String,
}

fn key(tenant: &str, agent: &str) -> String {
    format!("{tenant}:{agent}")
}

pub(crate) async fn load(state: &AppState, tenant: &str, agent: &str) -> Option<KnowledgeUnit> {
    state
        .server_store
        .kv_get(NAMESPACE, &key(tenant, agent))
        .await
        .ok()
        .flatten()
        .and_then(|i| serde_json::from_value(i.value).ok())
}

/// A source's whole text, chunk by chunk, in order.
async fn body_of(
    state: &AppState,
    tenant: &TenantContext,
    content_hash: &str,
) -> Result<String, ApiError> {
    let base = crate::knowledge::knowledge_base(state, tenant);
    let mut chunks = state
        .knowledge
        .chunks_of(tenant.tenant(), content_hash)
        .await
        .map_err(ApiError::internal)?;
    chunks.sort_by_key(|c| c.chunk_index);
    let mut out = String::new();
    for chunk in chunks {
        if let Some(text) = base
            .chunk_content(&chunk.content_address)
            .await
            .map_err(|e| ApiError::internal(e.to_string()))?
        {
            out.push_str(&text);
        }
    }
    Ok(out)
}

#[derive(Debug, Deserialize)]
pub(crate) struct CompilePayload {
    pub source_ids: Vec<String>,
    #[serde(default)]
    pub char_limit: Option<usize>,
}

/// `POST /assistants/{id}/knowledge/unit {source_ids, char_limit?}` —
/// compile. Refused (409) while a chosen source is contested, naming the
/// conflicts; refused (400) for a source the agent may not read.
pub(crate) async fn compile(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(agent): Path<String>,
    Json(payload): Json<CompilePayload>,
) -> Result<Json<Value>, ApiError> {
    state
        .server_store
        .get_assistant(&tenant.scope(&agent))
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("agent `{agent}` not found")))?;
    let mut ids: Vec<String> = payload
        .source_ids
        .iter()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect();
    ids.sort();
    ids.dedup();
    if ids.is_empty() {
        return Err(ApiError::bad_request("pick at least one source".to_owned()));
    }
    let limit = payload
        .char_limit
        .unwrap_or(UNIT_CHARS_DEFAULT)
        .clamp(1_000, UNIT_CHARS_MAX);
    // Publish is blocked while a chosen source is contested.
    let conflicts = crate::knowledge_conflicts::all_in(&state.server_store, tenant.tenant()).await;
    let contested: Vec<Value> = conflicts
        .iter()
        .filter(|c| c.state == "open" && (ids.contains(&c.source_a) || ids.contains(&c.source_b)))
        .map(|c| json!({"conflict_id": c.conflict_id, "claim": c.claim, "between": [c.title_a, c.title_b]}))
        .collect();
    if !contested.is_empty() {
        return Err(ApiError::new(
            axum::http::StatusCode::CONFLICT,
            "contested",
            format!("the chosen sources are contested on {} claim{} — rule on the Knowledge page first: {}", contested.len(), if contested.len() == 1 { "" } else { "s" }, contested.iter().map(|c| format!("\"{}\"", c["claim"].as_str().unwrap_or(""))).collect::<Vec<_>>().join(", ")),
        ));
    }
    let base = crate::knowledge::knowledge_base(&state, &tenant);
    let mut chosen = Vec::new();
    for id in &ids {
        let versions = base
            .versions_of(id)
            .await
            .map_err(|e| ApiError::internal(e.to_string()))?;
        let latest = versions
            .last()
            .cloned()
            .ok_or_else(|| ApiError::not_found(format!("no knowledge source `{id}`")))?;
        let readable = latest.scope.scope == MemoryScope::Tenant
            || (latest.scope.scope == MemoryScope::Agent && latest.scope.id == agent);
        if !readable {
            return Err(ApiError::bad_request(format!(
                "`{}` is another agent's source — this agent may not read it",
                latest.title
            )));
        }
        chosen.push(latest);
    }
    // The organization's own first, then a vendor's, then generic; then by title.
    chosen.sort_by(|a, b| {
        a.provenance
            .rank()
            .cmp(&b.provenance.rank())
            .then_with(|| a.title.cmp(&b.title))
    });
    let mut text = format!(
        "Compiled {} from {} source{}, ranked by provenance: the organization's own first, then vendor documentation, then generic guidance. Where they disagree, the one listed first stands. Cite a section by its source_id; read the rest of a cut source with knowledge.read.",
        Utc::now().format("%Y-%m-%d"), chosen.len(), if chosen.len() == 1 { "" } else { "s" }
    );
    let mut listed = Vec::new();
    let per_source = limit / chosen.len().max(1);
    for source in &chosen {
        let body = body_of(&state, &tenant, &source.content_hash).await?;
        let body = body.trim();
        let cut = body.chars().count() > per_source;
        let kept: String = body.chars().take(per_source).collect();
        text.push_str(&format!(
            "\n\n### {} — {} · `{}` v{}\n",
            source.title,
            source.provenance.label(),
            source.source_id,
            source.version
        ));
        for c in conflicts.iter().filter(|c| {
            c.state == "ruled" && (c.source_a == source.source_id || c.source_b == source.source_id)
        }) {
            if c.stands.as_deref().is_some_and(|s| s != source.source_id) {
                let winner = if c.source_a == source.source_id {
                    &c.title_b
                } else {
                    &c.title_a
                };
                text.push_str(&format!(
                    "Overruled on \"{}\": {} stands — do not follow this source on that.\n",
                    c.claim, winner
                ));
            }
        }
        text.push_str(&kept);
        if cut {
            text.push_str(&format!(
                "\n… (cut — read the rest with knowledge.read `{}`)",
                source.source_id
            ));
        }
        listed.push(UnitSource {
            source_id: source.source_id.clone(),
            title: source.title.clone(),
            provenance: source.provenance,
            version: source.version,
            content_hash: source.content_hash.clone(),
            chars: kept.chars().count(),
            cut,
        });
    }
    let previous = load(&state, tenant.tenant(), &agent).await;
    let unit = KnowledgeUnit {
        tenant: tenant.tenant().to_owned(),
        agent_id: agent.clone(),
        version: previous.map(|p| p.version + 1).unwrap_or(1),
        compiled_at: Utc::now(),
        compiled_by: tenant.attribution(),
        char_limit: limit,
        sources: listed,
        text,
    };
    state
        .server_store
        .kv_put(
            NAMESPACE,
            &key(tenant.tenant(), &agent),
            serde_json::to_value(&unit).map_err(|e| ApiError::internal(e.to_string()))?,
        )
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(
        json!({"unit": unit, "chars": unit.text.chars().count()}),
    ))
}

/// What has changed since compiling: sources with a newer version, or gone.
async fn staleness(state: &AppState, tenant: &TenantContext, unit: &KnowledgeUnit) -> Vec<Value> {
    let base = crate::knowledge::knowledge_base(state, tenant);
    let mut out = Vec::new();
    for s in &unit.sources {
        match base.versions_of(&s.source_id).await.ok().and_then(|v| v.last().cloned()) {
            Some(latest) if latest.content_hash != s.content_hash => out.push(json!({"source_id": s.source_id, "title": s.title, "why": format!("v{} since, compiled v{}", latest.version, s.version)})),
            None => out.push(json!({"source_id": s.source_id, "title": s.title, "why": "retired since"})),
            _ => {}
        }
    }
    out
}

/// `GET /assistants/{id}/knowledge/unit` — the unit, what went stale, and
/// the conflicts filed on its sources since.
pub(crate) async fn get_unit(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(agent): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let Some(unit) = load(&state, tenant.tenant(), &agent).await else {
        return Ok(Json(json!({"unit": null})));
    };
    let stale = staleness(&state, &tenant, &unit).await;
    let contested: Vec<Value> = crate::knowledge_conflicts::all_in(&state.server_store, tenant.tenant())
        .await
        .into_iter()
        .filter(|c| c.state == "open" && unit.sources.iter().any(|s| s.source_id == c.source_a || s.source_id == c.source_b))
        .map(|c| json!({"conflict_id": c.conflict_id, "claim": c.claim, "between": [c.title_a, c.title_b]}))
        .collect();
    let chars = unit.text.chars().count();
    Ok(Json(
        json!({"unit": unit, "chars": chars, "stale": stale, "contested": contested}),
    ))
}

/// `DELETE /assistants/{id}/knowledge/unit` — the agent stops carrying it.
pub(crate) async fn delete_unit(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(agent): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let removed = state
        .server_store
        .kv_delete(NAMESPACE, &key(tenant.tenant(), &agent))
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(json!({"removed": removed})))
}

/// The unit as the agent's first turn carries it, with any conflict filed
/// on its sources since compiling named up front — the contested claim is
/// withheld at run time even from a unit compiled before the conflict.
pub(crate) async fn unit_text(state: &AppState, tenant: &str, agent: &str) -> Option<String> {
    let unit = load(state, tenant, agent).await?;
    let contested: Vec<String> = crate::knowledge_conflicts::all_in(&state.server_store, tenant)
        .await
        .into_iter()
        .filter(|c| {
            c.state == "open"
                && unit
                    .sources
                    .iter()
                    .any(|s| s.source_id == c.source_a || s.source_id == c.source_b)
        })
        .map(|c| format!("\"{}\" ({} vs {})", c.claim, c.title_a, c.title_b))
        .collect();
    let mut out = format!("## Knowledge unit (v{})\n", unit.version);
    if !contested.is_empty() {
        out.push_str(&format!("CONTESTED since this unit was compiled: {}. State neither side of those claims as fact; say a person decides on the Knowledge page.\n\n", contested.join("; ")));
    }
    out.push_str(&unit.text);
    Some(out)
}
