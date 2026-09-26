//! Repair-record query surface (EP-10-S01 AC 3).
//!
//! Provides REST endpoints for the typed repair-record audit stream.
//! Records are persisted through [`rusty_agent_runtime::repair::FileRepairLedger`]
//! under `{store_path}/repairs/`.

use std::sync::Arc;

use axum::extract::{Extension, Path, Query, State as AxumState};
use axum::Json;
use chrono::{DateTime, Utc};
use serde::Deserialize;

use rusty_agent_runtime::repair::{
    file_knowledge_repair, FileRepairLedger, KnowledgeCause, KnowledgeClassifier, RepairComponent,
    RepairLedger, RepairOutcome, RepairQuery, RepairRecord,
};

use crate::auth::TenantContext;
use crate::error::ApiError;
use crate::routes::{internal_err, load_gap_ledger, persist_gap_ledger, AppState};

// ---------------------------------------------------------------------------
// Knowledge-level repair filing (EP-10-S09)
// ---------------------------------------------------------------------------

/// Request body for `POST /repairs/knowledge`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct KnowledgeRepairRequest {
    /// The failure signature to classify.
    pub failure_signature: String,
    /// How many times this failure has occurred.
    pub occurrence_count: u32,
    /// The session id, if any.
    pub session_id: Option<String>,
    /// The attempt id, if any.
    pub attempt_id: Option<String>,
    /// Evidence citations (record ids, log positions).
    pub evidence: Vec<String>,
    /// Repair record ids in the chain leading to this filing.
    pub repair_chain: Vec<String>,
    /// Optional skill manifest hash for divergence detection.
    pub skill_manifest_hash: Option<String>,
}

/// Response for `POST /repairs/knowledge`.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct KnowledgeRepairResponse {
    /// Whether the filing was accepted (side-band, always true for valid input).
    pub accepted: bool,
    /// The classified cause.
    pub cause: String,
}

/// Side-band knowledge-repair filing endpoint.
///
/// Classifies the failure, and if knowledge-level, spawns a background task
/// to file a gap-ledger entry and emit a repair record. Returns immediately
/// so the caller's failure path is never blocked.
pub(crate) async fn file_knowledge(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(req): Json<KnowledgeRepairRequest>,
) -> Result<Json<KnowledgeRepairResponse>, ApiError> {
    // Classify the failure.
    let cause = if req.occurrence_count >= 2 {
        KnowledgeClassifier::classify_identical_failure_across_retries(
            &req.failure_signature,
            req.occurrence_count,
        )
    } else {
        KnowledgeCause::Environmental
    };

    // If a skill manifest hash is provided, treat as divergence.
    let cause = if let Some(ref hash) = req.skill_manifest_hash {
        KnowledgeClassifier::classify_plan_reality_divergence(
            hash,
            &req.failure_signature,
            req.occurrence_count,
        )
    } else {
        cause
    };

    let cause_str = match &cause {
        KnowledgeCause::Knowledge { .. } => "knowledge",
        KnowledgeCause::Environmental => "environmental",
    };

    // Side-band: spawn the filing if knowledge-level.
    if let KnowledgeCause::Knowledge { .. } = cause {
        let tenant_id = tenant.tenant().to_string();
        let state = Arc::clone(&state);
        let evidence_ids = req.evidence.clone();
        let chain = req.repair_chain.clone();
        let session = req.session_id.clone();
        let attempt = req.attempt_id.clone();
        let cause = cause.clone();

        tokio::spawn(async move {
            let Ok(mut ledger) = load_gap_ledger(&state, &tenant_id).await else {
                return;
            };
            let evidence: Vec<rusty_agent_runtime::gaps::Citation> = evidence_ids
                .iter()
                .filter_map(|id| {
                    rusty_agent_runtime::gaps::Citation::new(
                        rusty_agent_runtime::gaps::CitationKind::RunReceipt,
                        id.clone(),
                        Some("cited evidence from failure path".to_string()),
                    )
                    .ok()
                })
                .collect();

            let _ = file_knowledge_repair(
                &mut ledger,
                state.repair_ledger.as_ref(),
                cause,
                evidence,
                session,
                attempt,
                chain,
            );

            let _ = persist_gap_ledger(&state, &tenant_id, &ledger).await;
        });
    }

    Ok(Json(KnowledgeRepairResponse {
        accepted: true,
        cause: cause_str.to_string(),
    }))
}

/// Query parameters for `GET /repairs`.
#[derive(Debug, Clone, Deserialize, Default)]
pub(crate) struct RepairListQuery {
    /// Filter by component (comma-separated, e.g. `provider_seam,tool_pipeline`).
    #[serde(default)]
    component: Option<String>,
    /// Filter by trigger class (comma-separated, e.g. `provider_error,crash`).
    #[serde(default)]
    trigger_class: Option<String>,
    /// Filter by outcome (comma-separated, e.g. `repaired,escalated`).
    #[serde(default)]
    outcome: Option<String>,
    /// Include only records with start_time >= this (ISO 8601).
    #[serde(default)]
    from: Option<DateTime<Utc>>,
    /// Include only records with start_time < this (ISO 8601).
    #[serde(default)]
    until: Option<DateTime<Utc>>,
    /// Filter by session id.
    #[serde(default)]
    session_id: Option<String>,
    /// Filter by attempt id.
    #[serde(default)]
    attempt_id: Option<String>,
    /// Maximum records to return (default 100, hard cap 1000).
    #[serde(default)]
    limit: Option<usize>,
}

impl RepairListQuery {
    fn to_filter(&self) -> RepairQuery {
        let mut filter = RepairQuery::default();
        if let Some(ref s) = self.component {
            filter.components = s
                .split(',')
                .filter_map(|part| parse_component(part.trim()))
                .collect();
        }
        if let Some(ref s) = self.trigger_class {
            filter.trigger_classes = s
                .split(',')
                .map(|part| part.trim().to_owned())
                .filter(|part| !part.is_empty())
                .collect();
        }
        if let Some(ref s) = self.outcome {
            filter.outcomes = s
                .split(',')
                .filter_map(|part| parse_outcome(part.trim()))
                .collect();
        }
        filter.from = self.from;
        filter.until = self.until;
        filter.session_id = self.session_id.clone();
        filter.attempt_id = self.attempt_id.clone();
        filter
    }
}

fn parse_component(raw: &str) -> Option<RepairComponent> {
    match raw {
        "tool_pipeline" => Some(RepairComponent::ToolPipeline),
        "provider_seam" => Some(RepairComponent::ProviderSeam),
        "compaction_engine" => Some(RepairComponent::CompactionEngine),
        "crash_repair" => Some(RepairComponent::CrashRepair),
        "attempt_scheduler" => Some(RepairComponent::AttemptScheduler),
        "orphan_sweep" => Some(RepairComponent::OrphanSweep),
        "stuck_turn_detector" => Some(RepairComponent::StuckTurnDetector),
        "dependency_invalidation" => Some(RepairComponent::DependencyInvalidation),
        "circuit_breaker" => Some(RepairComponent::CircuitBreaker),
        _ => None,
    }
}

fn parse_outcome(raw: &str) -> Option<RepairOutcome> {
    match raw {
        "repaired" => Some(RepairOutcome::Repaired),
        "escalated" => Some(RepairOutcome::Escalated),
        "failed" => Some(RepairOutcome::Failed),
        _ => None,
    }
}

/// List repair records, filtered and limited.
pub(crate) async fn list_repairs(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(_tenant): Extension<TenantContext>,
    Query(params): Query<RepairListQuery>,
) -> Result<Json<Vec<RepairRecord>>, ApiError> {
    let filter = params.to_filter();
    let mut records = state.repair_ledger.query(&filter).map_err(internal_err)?;
    let limit = params.limit.unwrap_or(100).clamp(1, 1000);
    if records.len() > limit {
        records.truncate(limit);
    }
    Ok(Json(records))
}

/// Fetch one repair record by id.
pub(crate) async fn get_repair(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(_tenant): Extension<TenantContext>,
    Path(record_id): Path<String>,
) -> Result<Json<RepairRecord>, ApiError> {
    let filter = RepairQuery {
        ..RepairQuery::default()
    };
    let records = state.repair_ledger.query(&filter).map_err(internal_err)?;
    let record = records
        .into_iter()
        .find(|r| r.record_id == record_id)
        .ok_or_else(|| ApiError::not_found(format!("repair record `{record_id}` not found")))?;
    Ok(Json(record))
}

/// Initialize the file-backed repair ledger at `store_path`.
pub(crate) fn init_ledger(store_path: &std::path::Path) -> Arc<FileRepairLedger> {
    Arc::new(FileRepairLedger::new(store_path))
}
