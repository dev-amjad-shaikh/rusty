//! What this deployment carries, measured rather than asserted.
//!
//! "How many agents can one box run?" is the question a buyer asks last
//! and nobody here could answer from the product. The numbers were in the
//! journals all along: every run records when each step happened, what it
//! spent and how long it took. This reads them.
//!
//! `GET /capacity` is the standing picture — what is running right now,
//! what the last hour actually did (runs finished, how fast, how much),
//! what the store holds, the bounds this deployment is configured with,
//! and how long the last restore took. `POST /capacity/probe` is the
//! honest part: it runs a real agent N times at once and reports what
//! happened, because a throughput number nobody has run is a guess.
//!
//! Every figure names the window it came from. Where the numbers do not
//! support a claim, the report says so rather than filling the gap.
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Query, State as AxumState};
use axum::{Extension, Json};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::auth::TenantContext;
use crate::error::ApiError;
use crate::routes::AppState;

/// The window the standing report reads, unless asked for another.
const DEFAULT_MINUTES: i64 = 60;
/// The most copies a probe may run at once: enough to find a bound, not
/// enough to be a load test nobody asked for.
const MAX_COPIES: usize = 25;

#[derive(Debug, Deserialize)]
pub struct WindowQuery {
    /// How far back the window reaches; 60 minutes by default.
    #[serde(default)]
    pub minutes: Option<i64>,
}

fn percentile(sorted: &[i64], p: f64) -> Option<i64> {
    if sorted.is_empty() {
        return None;
    }
    let at = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted.get(at).copied()
}

/// One run, as the window counts it. `took_ms` is the *execution span*
/// the journal shows — first recorded step to last. The wait before a run
/// starts is not in it; only the probe sees that.
struct Finished {
    ended_at: DateTime<Utc>,
    took_ms: i64,
    tokens: u64,
    model_calls: u64,
    failed: bool,
}

/// What the journals say happened in the window.
fn read_window(journals: &[rusty_agent_runtime::journal::JournalSnapshot], since: DateTime<Utc>) -> Vec<Finished> {
    let mut out = Vec::new();
    for snapshot in journals {
        let Some(first) = snapshot.events.first() else { continue };
        let Some(last) = snapshot.events.last() else { continue };
        if last.recorded_at < since {
            continue;
        }
        let tokens: u64 = snapshot.events.iter().filter_map(|e| e.tokens.as_ref()).map(|u| u.total_tokens).sum();
        let model_calls = snapshot
            .events
            .iter()
            .filter(|e| matches!(e.kind, rusty_agent_runtime::record::RunEventKind::ModelCall))
            .count() as u64;
        let failed = snapshot.events.iter().any(|e| matches!(e.status, rusty_agent_runtime::record::EventStatus::Error));
        out.push(Finished {
            ended_at: last.recorded_at,
            took_ms: (last.recorded_at - first.recorded_at).num_milliseconds().max(0),
            tokens,
            model_calls,
            failed,
        });
    }
    out.sort_by_key(|f| f.ended_at);
    out
}

/// The sentence the numbers support about what would bite first, and
/// nothing beyond it.
fn bites_first(running: usize, queued: usize, per_thread: usize, finished: &[Finished], p95_ms: Option<i64>) -> String {
    if queued > 0 {
        return format!(
            "{queued} run(s) are waiting behind {running} running: this deployment admits {per_thread} run(s) per thread at a time, so a thread with work stacked on it is the bound in front of you now."
        );
    }
    if finished.is_empty() {
        return "Nothing finished in this window, so nothing here says what would bite first. Run a probe to find out.".to_owned();
    }
    match p95_ms {
        // A run whose execution alone runs long is bound by what it waits
        // on inside the run — the model route, or a system it reads.
        Some(p95) if p95 > 20_000 && finished.iter().map(|f| f.model_calls).sum::<u64>() > 0 => format!(
            "Nothing is queuing here. The slow half of these runs spends {}s executing, mostly in model calls, so the model route is the bound before this box is.",
            p95 / 1000
        ),
        _ => "Nothing is queuing here, and an execution span does not show the wait before a run starts. Run a probe to measure end to end and find a bound.".to_owned(),
    }
}

/// `GET /capacity` — what is running, what the window did, what the store
/// holds, what the bounds are, and what would bite first.
pub(crate) async fn get_capacity(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Query(query): Query<WindowQuery>,
) -> Result<Json<Value>, ApiError> {
    let minutes = query.minutes.unwrap_or(DEFAULT_MINUTES).clamp(1, 7 * 24 * 60);
    let since = Utc::now() - Duration::minutes(minutes);

    // Now: what the run manager is holding.
    let live = state.run_deps.manager.list().await;
    let running = live.iter().filter(|(_, i)| i.status.as_str() == "running").count();
    let queued = live.iter().filter(|(_, i)| i.status.as_str() == "pending").count();

    // Now: delegated outcomes and queued work.
    let assignments = state.server_store.list_assignments(Some(tenant.tenant())).await.unwrap_or_default();
    let mut by_state: BTreeMap<String, usize> = BTreeMap::new();
    for a in &assignments {
        *by_state.entry(a.state.clone()).or_default() += 1;
    }

    // The window, from the journals.
    let journals = state.server_store.list_journals().await.map_err(ApiError::internal)?;
    let finished = read_window(&journals, since);
    let mut durations: Vec<i64> = finished.iter().map(|f| f.took_ms).collect();
    durations.sort_unstable();
    let p50 = percentile(&durations, 0.50);
    let p95 = percentile(&durations, 0.95);
    let tokens: u64 = finished.iter().map(|f| f.tokens).sum();
    let failures = finished.iter().filter(|f| f.failed).count();
    let per_minute = if minutes > 0 { finished.len() as f64 / minutes as f64 } else { 0.0 };

    let counts = crate::estate::counts(&state.config.store_path);
    let restored = crate::estate::restored(&state.config.store_path);

    Ok(Json(json!({
        "now": {
            "runs_running": running,
            "runs_queued": queued,
            "assignments": by_state,
            "journals_held": journals.len(),
        },
        "window": {
            "minutes": minutes,
            "since": since,
            "runs_finished": finished.len(),
            "runs_per_minute": (per_minute * 100.0).round() / 100.0,
            "median_execution_ms": p50,
            "slow_half_execution_ms": p95,
            "slowest_execution_ms": durations.last().copied(),
            "note": "Execution spans from the journals: a run's first recorded step to its last. The wait before a run starts is not in them — a probe measures end to end.",
            "tokens": tokens,
            "model_calls": finished.iter().map(|f| f.model_calls).sum::<u64>(),
            "failures": failures,
        },
        "store": {
            "kind": if state.config.database_url.is_some() { "postgres" } else { "files" },
            "path": state.config.store_path.display().to_string(),
            "runs": counts.runs,
            "threads": counts.threads,
            "agents": counts.agents,
        },
        "bounds": {
            "runs_per_thread": state.config.max_concurrent_runs_per_thread,
            "sweep_at": state.config.sweep_at.map(|(h, m)| format!("{h:02}:{m:02}")),
        },
        "restore": restored,
        "bites_first": bites_first(running, queued, state.config.max_concurrent_runs_per_thread, &finished, p95),
    })))
}

#[derive(Debug, Deserialize)]
pub struct ProbePayload {
    /// The agent to run; a cheap one, since every copy is a real run.
    pub assistant_id: String,
    /// How many at once. 1–25.
    #[serde(default)]
    pub copies: Option<usize>,
    /// What to ask it. Kept short on purpose.
    #[serde(default)]
    pub message: Option<String>,
}

/// One copy's outcome.
#[derive(Debug, Serialize)]
struct Copy {
    run_id: Option<String>,
    status: String,
    took_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// `POST /capacity/probe` — run a real agent N times at once and report
/// what happened. Every copy is a real run on its own thread: it spends
/// tokens, it is journaled, and it is attributed to the caller like any
/// other run. A number nobody has run is a guess, so this runs it.
pub(crate) async fn probe(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(payload): Json<ProbePayload>,
) -> Result<Json<Value>, ApiError> {
    let copies = payload.copies.unwrap_or(5).clamp(1, MAX_COPIES);
    let message = payload
        .message
        .map(|m| m.trim().to_owned())
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| "Reply with the single word: ready.".to_owned());
    let internal = crate::auth::scope_id(tenant.tenant(), &payload.assistant_id);
    let assistant = state
        .server_store
        .get_assistant(&internal)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("unknown agent `{}`", payload.assistant_id)))?;

    let began = Instant::now();
    let started_at = Utc::now();
    let mut running = Vec::with_capacity(copies);
    for _ in 0..copies {
        let state = Arc::clone(&state);
        let tenant = tenant.clone();
        let assistant = assistant.clone();
        let message = message.clone();
        running.push(tokio::spawn(async move { one_copy(&state, &tenant, &assistant, &message).await }));
    }
    let mut out = Vec::with_capacity(copies);
    for handle in running {
        match handle.await {
            Ok(copy) => out.push(copy),
            Err(error) => out.push(Copy { run_id: None, status: "lost".to_owned(), took_ms: 0, error: Some(error.to_string()) }),
        }
    }
    let wall_ms = began.elapsed().as_millis() as i64;
    let mut durations: Vec<i64> = out.iter().filter(|c| c.status == "success").map(|c| c.took_ms).collect();
    durations.sort_unstable();
    let succeeded = durations.len();
    let per_minute = if wall_ms > 0 { succeeded as f64 * 60_000.0 / wall_ms as f64 } else { 0.0 };
    // What the runs actually did. An agent whose graph never reaches a
    // model answers in milliseconds, and a throughput number from that
    // measures the scheduler and the store, not agent work. The report
    // says which it measured rather than letting the number speak for
    // something it did not do.
    let mut model_calls = 0u64;
    let mut tokens = 0u64;
    for copy in out.iter().filter(|c| c.run_id.is_some()) {
        let usage = crate::llm_providers::run_usage(&state, copy.run_id.as_deref().unwrap_or_default()).await;
        model_calls += usage.get("model_calls").and_then(Value::as_u64).unwrap_or(0);
        tokens += usage.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0)
            + usage.get("completion_tokens").and_then(Value::as_u64).unwrap_or(0);
    }
    let measured = if model_calls == 0 {
        "the run plumbing only: these runs reached no model, so this is what the scheduler, the graph and the store carry, not what agents carry"
    } else {
        "agent work: these runs reached the model, so this is end-to-end throughput at this concurrency"
    };
    tracing::info!(copies, succeeded, wall_ms, model_calls, "capacity probe");

    Ok(Json(json!({
        "agent": assistant.name,
        "assistant_id": payload.assistant_id,
        "copies": copies,
        "started_at": started_at,
        "wall_ms": wall_ms,
        "succeeded": succeeded,
        "failed": copies - succeeded,
        "runs_per_minute": (per_minute * 100.0).round() / 100.0,
        "median_ms": percentile(&durations, 0.50),
        "slow_half_ms": percentile(&durations, 0.95),
        "slowest_ms": durations.last().copied(),
        "model_calls": model_calls,
        "tokens": tokens,
        "measured": measured,
        "copies_detail": out,
        "note": match (succeeded == copies, model_calls) {
            (true, 0) => format!(
                "{copies} at once, all through, in {}s wall — but none of them reached a model, so this measures the run plumbing. Probe an agent whose graph calls one to measure agent throughput.",
                wall_ms / 1000
            ),
            (true, calls) => format!("{copies} at once, all through, in {}s wall, {calls} model call(s) between them. The bound is above this.", wall_ms / 1000),
            (false, _) => format!("{succeeded} of {copies} through in {}s wall; the rest are in `copies_detail` with why.", wall_ms / 1000),
        },
    })))
}

/// One copy: a fresh thread and a real run of the agent, waited out.
async fn one_copy(state: &Arc<AppState>, tenant: &TenantContext, assistant: &crate::assistants::AssistantRecord, message: &str) -> Copy {
    let began = Instant::now();
    let thread_id = uuid::Uuid::new_v4().to_string();
    let internal_thread_id = crate::auth::scope_id(tenant.tenant(), &thread_id);
    let record = crate::threads::ThreadRecord {
        thread_id: thread_id.clone(),
        tenant: tenant.tenant().to_owned(),
        graph: assistant.graph.clone(),
        metadata: json!({"channel": "capacity-probe", "created_by": tenant.attribution()}),
        forked_from: None,
        seed_length: None,
        created_at: Utc::now(),
    };
    if let Err(error) = state.server_store.create_thread(&internal_thread_id, &record).await {
        return Copy { run_id: None, status: "refused".to_owned(), took_ms: began.elapsed().as_millis() as i64, error: Some(error.to_string()) };
    }
    let mut payload = crate::runs::RunPayload {
        input: Some(json!({"messages": [{"role": "user", "content": message}]})),
        metadata: Some(json!({"channel": "capacity-probe", "created_by": tenant.attribution(), "on_behalf_of": tenant.attribution()})),
        assistant_id: Some(assistant.assistant_id.clone()),
        ..crate::runs::RunPayload::default()
    };
    crate::routes::apply_assistant_defaults(state, tenant.tenant(), &internal_thread_id, assistant, &mut payload).await;
    let scheduled = match crate::runs::schedule(
        &state.run_deps,
        &internal_thread_id,
        &thread_id,
        &assistant.graph,
        payload,
        crate::runs::MultitaskStrategy::Enqueue,
    )
    .await
    {
        Ok(scheduled) => scheduled,
        Err(error) => return Copy { run_id: None, status: "refused".to_owned(), took_ms: began.elapsed().as_millis() as i64, error: Some(format!("{error:?}")) },
    };
    let run_id = scheduled.run_id.clone();
    // Wait it out, the way a caller of /runs/wait does.
    for _ in 0..1200 {
        match state.run_deps.manager.info(&run_id).await {
            Some(info) if info.status.is_terminal() => {
                return Copy { run_id: Some(run_id), status: info.status.as_str().to_owned(), took_ms: began.elapsed().as_millis() as i64, error: None };
            }
            Some(_) => {}
            None => break,
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    Copy { run_id: Some(run_id), status: "unfinished".to_owned(), took_ms: began.elapsed().as_millis() as i64, error: Some("the run had not finished when the probe stopped waiting".to_owned()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_percentile_reads_the_sorted_middle_and_tail() {
        let sorted = vec![10, 20, 30, 40, 100];
        assert_eq!(percentile(&sorted, 0.50), Some(30));
        assert_eq!(percentile(&sorted, 0.95), Some(100));
        assert_eq!(percentile(&[], 0.5), None);
        assert_eq!(percentile(&[7], 0.95), Some(7));
    }

    fn finished(took_ms: i64, model_calls: u64) -> Finished {
        Finished { ended_at: Utc::now(), took_ms, tokens: 100, model_calls, failed: false }
    }

    #[test]
    fn what_bites_first_says_only_what_the_numbers_support() {
        // Something waiting is the bound in front of you, and it is named.
        let queueing = bites_first(1, 3, 1, &[finished(500, 1)], Some(500));
        assert!(queueing.contains("3 run(s) are waiting"), "{queueing}");
        assert!(queueing.contains("1 run(s) per thread"), "{queueing}");

        // Nothing finished: the report refuses to guess and says what to do.
        let empty = bites_first(0, 0, 1, &[], None);
        assert!(empty.contains("nothing here says what would bite first"), "{empty}");
        assert!(empty.contains("probe"), "{empty}");

        // Slow runs full of model calls: the route, not the box.
        let slow = bites_first(0, 0, 1, &[finished(45_000, 6)], Some(45_000));
        assert!(slow.contains("model route is the bound"), "{slow}");
        assert!(slow.contains("45s"), "{slow}");

        // Fast and idle: no claim at all.
        let idle = bites_first(0, 0, 1, &[finished(400, 1)], Some(400));
        assert!(idle.contains("does not show the wait before a run starts"), "{idle}");
    }
}
