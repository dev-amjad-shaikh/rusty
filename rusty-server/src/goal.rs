//! The goal measured on the agent's own runs — the one number the Goal
//! card shows and the Coach's brief starts from, computed in one place so
//! the two agree: the agent's live runs of the last seven days (an
//! evaluation replaying a case or a rehearsal in a world exercises the
//! agent without serving anyone), as the share that met the metric, and one
//! value per day for the trend.
use std::sync::Arc;

use axum::extract::{Path, Query, State as AxumState};
use axum::{Extension, Json};
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::TenantContext;
use crate::error::ApiError;
use crate::routes::AppState;

/// The metrics the Goal card offers, by name; anything else measures as
/// the first.
pub const METRICS: [&str; 3] = ["Outcome verified", "Resolved without a pause", "Runs that finished"];
const WINDOW_DAYS: i64 = 7;
const RUNS_READ: usize = 300;

#[derive(Debug, Clone, PartialEq)]
pub struct Measure {
    pub metric: String,
    /// The share that met the metric, 0–100, rounded; `None` when no run counted.
    pub current: Option<f64>,
    pub sample: u64,
    /// One value per day, oldest first, seven days; a day without a run reads 0.
    pub trend: Vec<f64>,
}

impl Measure {
    pub fn to_value(&self) -> Value {
        json!({
            "metric": self.metric,
            "current": self.current,
            "sample": self.sample,
            "trend": self.trend,
            "unit": "%",
            "window": format!("the agent's live runs of the last {WINDOW_DAYS} days"),
            "runs_read": RUNS_READ,
        })
    }
}

/// Whether a run counts for `metric`, and 1 when it met it, from the run as
/// the list serves it.
fn score(metric: &str, run: &Value) -> (bool, u64) {
    let status = run.get("status").and_then(Value::as_str).unwrap_or("");
    match metric {
        "Resolved without a pause" => (status != "pending" && status != "running", if status == "interrupted" || run.get("decision").is_some() { 0 } else { 1 }),
        "Runs that finished" => (status != "pending" && status != "running", if status == "success" { 1 } else { 0 }),
        _ => (matches!(status, "success" | "error" | "failed"), if run.pointer("/verification/verdict").and_then(Value::as_str) == Some("verified") { 1 } else { 0 }),
    }
}

pub async fn measure(state: &Arc<AppState>, tenant: &TenantContext, external: &str, metric: &str) -> Measure {
    let metric = if METRICS.contains(&metric) { metric } else { METRICS[0] };
    let now = Utc::now();
    let since = now - Duration::days(WINDOW_DAYS);
    let recalled = crate::routes::recall_runs(state, tenant, RUNS_READ).await.unwrap_or_default();
    let (mut counted, mut met) = (0u64, 0u64);
    let mut days: Vec<(u64, u64)> = vec![(0, 0); WINDOW_DAYS as usize];
    for run in recalled.into_iter().map(crate::routes::RecalledRun::into_wire) {
        if run.get("assistant_id").and_then(Value::as_str) != Some(external) {
            continue;
        }
        let live = run.pointer("/metadata/channel").and_then(Value::as_str) != Some("evaluation") && run.get("worlds").and_then(Value::as_array).is_none_or(|w| w.is_empty());
        let Some(at) = run.get("created_at").and_then(Value::as_str).and_then(|t| t.parse::<DateTime<Utc>>().ok()) else { continue };
        if !live || at < since {
            continue;
        }
        let (counts, value) = score(metric, &run);
        if counts {
            counted += 1;
            met += value;
            let day = (WINDOW_DAYS - 1 - (now.date_naive() - at.date_naive()).num_days()).clamp(0, WINDOW_DAYS - 1) as usize;
            days[day].0 += 1;
            days[day].1 += value;
        }
    }
    let pct = |n: u64, m: u64| if n > 0 { ((m as f64 / n as f64) * 100.0).round() } else { 0.0 };
    Measure {
        metric: metric.to_owned(),
        current: if counted > 0 { Some(pct(counted, met)) } else { None },
        sample: counted,
        trend: days.iter().map(|(n, m)| pct(*n, *m)).collect(),
    }
}

#[derive(Deserialize)]
pub struct MeasureQuery {
    pub metric: Option<String>,
}

/// `GET /assistants/{id}/goal?metric=` — the measure the card and the brief share.
pub(crate) async fn get_measure(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(assistant_id): Path<String>,
    Query(query): Query<MeasureQuery>,
) -> Result<Json<Value>, ApiError> {
    let scoped = tenant.scope(&assistant_id);
    let record = state.server_store.get_assistant(&scoped).await.map_err(ApiError::internal)?.ok_or_else(|| ApiError::not_found(format!("unknown assistant `{assistant_id}`")))?;
    let external = tenant.unscope(&record.assistant_id).unwrap_or(&record.assistant_id).to_owned();
    let saved = record.metadata.pointer("/studio/goal/metric").and_then(Value::as_str).map(str::to_owned);
    let metric = query.metric.or(saved).unwrap_or_else(|| METRICS[0].to_owned());
    let m = measure(&state, &tenant, &external, &metric).await;
    let mut body = m.to_value();
    body["goal"] = record.metadata.pointer("/studio/goal").cloned().unwrap_or(Value::Null);
    Ok(Json(body))
}
