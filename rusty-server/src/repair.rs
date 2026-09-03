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
    FileRepairLedger, RepairComponent, RepairLedger, RepairOutcome, RepairQuery, RepairRecord,
};

use crate::auth::TenantContext;
use crate::error::ApiError;
use crate::routes::{internal_err, AppState};

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
