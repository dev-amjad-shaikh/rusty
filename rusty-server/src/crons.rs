//! Crons: durable schedule records plus a tokio scheduler that fires runs.
//!
//! A cron is `{graph, schedule, input?}` where the schedule is either a
//! fixed interval in seconds (`interval_secs`) or a 5-field cron expression
//! (`cron_expr`, minute resolution, evaluated in UTC). Records are persisted
//! as one JSON file per cron under `{store_path}/crons/{cron_id}.json` and
//! reloaded when the router is built. A single background task (spawned by
//! [`crate::routes::router`]) ticks every 200 ms, fires due crons — each
//! firing creates a fresh thread bound to the cron's graph and schedules a
//! background run on it — and honors `on_run_completed`: `keep` (default)
//! leaves the cron active, `delete` removes it once the first fired run
//! reaches a terminal state.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::routes::AppState;
use crate::runs::{self, MultitaskStrategy, RunPayload};
use crate::threads::ThreadRecord;

/// What happens to a cron after one of its runs reaches a terminal state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OnRunCompleted {
    /// Keep the cron firing on its schedule.
    #[default]
    Keep,
    /// Delete the cron once the first fired run finishes.
    Delete,
}

impl OnRunCompleted {
    /// Parse the wire value (`None` defaults to `keep`).
    pub(crate) fn parse(raw: Option<&str>) -> Result<Self, String> {
        match raw {
            None | Some("keep") => Ok(Self::Keep),
            Some("delete") => Ok(Self::Delete),
            Some(other) => Err(format!(
                "unknown on_run_completed `{other}` (expected `keep` or `delete`)"
            )),
        }
    }
}

/// One cron: a schedule that fires runs of a registered graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CronRecord {
    pub cron_id: String,
    /// Registered graph the fired runs execute.
    pub graph: String,
    /// Fixed-interval schedule: seconds between firings (XOR `cron_expr`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_secs: Option<u64>,
    /// 5-field cron expression (`min hour day-of-month month day-of-week`,
    /// UTC), evaluated with a leading `0` seconds field (XOR
    /// `interval_secs`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cron_expr: Option<String>,
    /// Initial state for fired runs (must be a JSON object when present).
    #[serde(default)]
    pub input: Option<Value>,
    /// The agent the fired runs go through (its external id): charter,
    /// tools and skills apply exactly as they do to a run started from the
    /// API. Absent, the cron fires the bare graph — the pre-agent shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant_id: Option<String>,
    /// Who created the schedule; every run it fires is attributed to them,
    /// so the journal says who set the agent going.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<Value>,
    #[serde(default)]
    pub metadata: Value,
    #[serde(default)]
    pub on_run_completed: OnRunCompleted,
    pub created_at: DateTime<Utc>,
    /// Wall-clock of the most recent firing (scheduler-maintained).
    #[serde(default)]
    pub last_run_at: Option<DateTime<Utc>>,
    /// Total runs fired by this cron (scheduler-maintained).
    #[serde(default)]
    pub runs_fired: u64,
    /// The world (by id) every fired run acts in, when the schedule was
    /// put in one: standing work rehearsed in the stand-in before it is
    /// pointed at the live system. Absent, the runs act in the live
    /// systems.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub world: Option<String>,
    /// The world's name, for the schedule's row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub world_name: Option<String>,
    /// Every world the fired runs act in when the agent touches several
    /// systems (ids, `world` first) and their names.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub worlds: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub world_names: Vec<String>,
    /// Set while the schedule cannot fire at all — its agent is gone or
    /// archived — in words for its row; cleared by the next firing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stalled: Option<String>,
    /// The most runs this schedule may ever fire — a budget that follows
    /// the schedule, not the run. Reached, it stalls with the reason in
    /// its row; `None` is no cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_runs: Option<u64>,
    /// The most tokens the runs this schedule fires may spend, all told —
    /// the spend that follows the schedule. Reached, it stalls with the
    /// reason in its row; `None` is no cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    /// What the runs it fired have spent so far, in tokens, folded from
    /// each run's terminal as it lands.
    #[serde(default)]
    pub tokens_spent: u64,
    /// Set while the schedule is held: a run it fired is waiting on a
    /// person's decision, and the schedule does not fire again until it is
    /// decided — one pause per schedule, not a pile of identical ones in
    /// the Inbox. `{run_id, since}`; cleared by the next firing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held: Option<Value>,
}

/// Upper bound for `interval_secs` (one year). Unbounded values are a
/// security problem, not just a UX one: `u64` seconds above `i64::MAX`
/// would wrap negative in the duration cast (a next-due in the past fires
/// every 200 ms tick — a self-inflicted fire-storm), and large positive
/// values can overflow chrono's timestamp math inside the single scheduler
/// task, killing the schedule for every tenant.
const MAX_INTERVAL_SECS: u64 = 31_536_000;

/// Validate a create-payload schedule pair: exactly one of interval or
/// expression, interval within `1..=MAX_INTERVAL_SECS`, expression
/// parseable.
pub(crate) fn validate_schedule(
    interval_secs: Option<u64>,
    cron_expr: Option<&str>,
) -> Result<(), String> {
    match (interval_secs, cron_expr) {
        (Some(0), None) => Err("`interval_secs` must be >= 1".to_string()),
        (Some(secs), None) if secs > MAX_INTERVAL_SECS => Err(format!(
            "`interval_secs` must be <= {MAX_INTERVAL_SECS} (one year)"
        )),
        (Some(_), None) => Ok(()),
        (None, Some(expr)) => parse_expr(expr).map(|_| ()),
        (None, None) => {
            Err("exactly one of `interval_secs` or `cron_expr` is required".to_string())
        }
        (Some(_), Some(_)) => {
            Err("`interval_secs` and `cron_expr` are mutually exclusive".to_string())
        }
    }
}

/// Parse a 5-field cron expression via the `cron` crate (which expects a
/// leading seconds field, so we pin it to `0`).
fn parse_expr(expr: &str) -> Result<cron::Schedule, String> {
    cron::Schedule::from_str(&format!("0 {expr}"))
        .map_err(|e| format!("invalid cron expression `{expr}`: {e}"))
}

/// The next firing strictly after `now`; `None` for corrupt records (the
/// clamp + checked math also keep pre-validation records persisted by older
/// versions from panicking the scheduler loop).
/// When a schedule the scheduler has not seen yet (at boot, or just made)
/// is first due. An interval schedule counts from when it last fired, or
/// from its making — not from the boot: a restart must not push every
/// hourly schedule out by an hour, and one that fell due while the server
/// was down fires now. A cron expression's next occurrence is its own.
fn first_due(cron: &CronRecord, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    match (cron.interval_secs, &cron.cron_expr) {
        (Some(secs), None) => {
            let secs = i64::try_from(secs.clamp(1, MAX_INTERVAL_SECS)).ok()?;
            let anchor = cron.last_run_at.unwrap_or(cron.created_at);
            let due = anchor.checked_add_signed(chrono::Duration::seconds(secs))?;
            Some(due.max(now))
        }
        _ => next_after(cron, now),
    }
}

fn next_after(cron: &CronRecord, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    match (cron.interval_secs, &cron.cron_expr) {
        (Some(secs), None) => {
            let secs = i64::try_from(secs.clamp(1, MAX_INTERVAL_SECS)).ok()?;
            now.checked_add_signed(chrono::Duration::seconds(secs))
        }
        (None, Some(expr)) => parse_expr(expr).ok()?.upcoming(Utc).next(),
        _ => None,
    }
}

/// The on-disk directory holding one JSON file per cron.
pub(crate) fn dir(store_root: &Path) -> PathBuf {
    store_root.join("crons")
}

/// Load all persisted crons, skipping (with a warning) any file that fails
/// to parse. Tenant-scoped crons live one directory deeper
/// (`crons/{tenant}/{cron_id}.json`), so the walk is recursive.
pub(crate) fn load(store_root: &Path) -> HashMap<String, CronRecord> {
    let mut out = HashMap::new();
    let mut files = Vec::new();
    collect_json_files(&dir(store_root), &mut files);
    for path in files {
        let parsed = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<CronRecord>(&raw).ok());
        match parsed {
            Some(record) => {
                out.insert(record.cron_id.clone(), record);
            }
            None => {
                tracing::warn!(path = %path.display(), "skipping unreadable cron file")
            }
        }
    }
    out
}

/// Recursively collect `*.json` files under `root` (tenant subdirectories
/// hold that tenant's records).
fn collect_json_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_json_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("json") {
            out.push(path);
        }
    }
}

/// Persist one cron record (create or overwrite). The id may carry a
/// `{tenant}/` prefix, so the parent directory is created, not just the
/// flat crons dir.
pub(crate) async fn persist(store_root: &Path, record: &CronRecord) -> std::io::Result<()> {
    let path = dir(store_root).join(format!("{}.json", record.cron_id));
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let raw = serde_json::to_vec_pretty(record).expect("cron serialization is infallible");
    tokio::fs::write(path, raw).await
}

/// Spawn the background scheduler task. Lives for the app's lifetime (until
/// the drain token fires); the returned task is deliberately detached.
pub(crate) fn spawn_scheduler(state: Arc<AppState>) {
    tokio::spawn(async move {
        // Per-cron next-due bookkeeping, rebuilt as crons come and go.
        let mut next_due: HashMap<String, DateTime<Utc>> = HashMap::new();
        // One-shot (`on_run_completed: "delete"`) crons that have fired but
        // whose run hasn't reached a terminal state yet. Without this
        // tombstone a one-shot cron would keep firing on schedule until its
        // first run finishes and deletes it.
        let mut oneshot_fired: HashSet<String> = HashSet::new();
        let mut ticker = tokio::time::interval(Duration::from_millis(200));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                // Drain: firing a cron against a shutting-down server would
                // only schedule a run the drain is about to cancel — crons
                // are durable records, the next process's scheduler
                // re-derives their due times from the schedule.
                _ = state.shutdown.cancelled() => {
                    tracing::info!("cron scheduler shutting down");
                    break;
                }
            }
            let crons = match state.server_store.list_crons().await {
                Ok(crons) => crons,
                Err(error) => {
                    tracing::warn!(%error, "cron scheduler: listing crons failed");
                    continue;
                }
            };
            // Prune bookkeeping for deleted crons: a stale entry would
            // accumulate forever, and a later cron reusing the id would
            // inherit the stale due time (immediate off-schedule firing).
            let listed: HashSet<&str> = crons.iter().map(|c| c.cron_id.as_str()).collect();
            next_due.retain(|id, _| listed.contains(id.as_str()));
            oneshot_fired.retain(|id| listed.contains(id.as_str()));
            let now = Utc::now();
            for cron in crons {
                if oneshot_fired.contains(&cron.cron_id) {
                    continue; // one-shot draining: its first run deletes it
                }
                if !next_due.contains_key(&cron.cron_id) {
                    match first_due(&cron, now) {
                        Some(due) => {
                            next_due.insert(cron.cron_id.clone(), due);
                        }
                        None => continue,
                    }
                }
                let due = next_due.get_mut(&cron.cron_id).expect("inserted above");
                if now < *due {
                    continue;
                }
                match next_after(&cron, now) {
                    Some(next) => *due = next,
                    None => {
                        next_due.remove(&cron.cron_id);
                    }
                }
                if cron.on_run_completed == OnRunCompleted::Delete {
                    oneshot_fired.insert(cron.cron_id.clone());
                }
                tokio::spawn(fire(Arc::clone(&state), cron));
            }
        }
    });
}

/// The pending approval of a run this schedule fired, if any: the fired
/// run names its schedule in its metadata.
async fn pending_run_of(
    state: &AppState,
    external_cron_id: &str,
) -> Option<crate::approvals::ApprovalRecord> {
    for a in state
        .run_deps
        .approvals
        .list()
        .into_iter()
        .filter(|a| a.status == "pending")
    {
        let Ok(Some(accepted)) = state.server_store.get_accepted_run(&a.run_id).await else {
            continue;
        };
        if accepted
            .payload
            .metadata
            .as_ref()
            .and_then(|m| m.get("cron_id"))
            .and_then(Value::as_str)
            == Some(external_cron_id)
        {
            return Some(a);
        }
    }
    None
}

/// One firing: create a fresh thread, schedule the cron's run on it, update
/// bookkeeping, and honor `on_run_completed`.
///
/// The scheduler lists crons across **all** tenants; each cron's id carries
/// its `{tenant}/` prefix, so the firing is scoped back into the owning
/// tenant: the fresh thread lives under the tenant's internal id namespace
/// and its record reports the external (unprefixed) cron id.
async fn fire(state: Arc<AppState>, cron: CronRecord) {
    let tenant = crate::auth::tenant_of_internal(&cron.cron_id);
    let external_cron_id = crate::auth::strip_owned(tenant, &cron.cron_id).unwrap_or(&cron.cron_id);
    let thread_id = uuid::Uuid::new_v4().to_string();
    let internal_thread_id = crate::auth::scope_id(tenant, &thread_id);
    // A schedule bound to an agent runs that agent, or nothing: a deleted
    // agent must not leave a bare-graph run firing under its name.
    let assistant = match &cron.assistant_id {
        Some(id) => match state
            .server_store
            .get_assistant(&crate::auth::scope_id(tenant, id))
            .await
        {
            Ok(Some(assistant)) if assistant.archived_at.is_none() => Some(assistant),
            Ok(found) => {
                // Once: the row says why, and the person who set it going
                // is told — a schedule that skips every due time in silence
                // is a brief nobody gets and nobody misses until it matters.
                let why = match found {
                    Some(archived) => format!("its agent {} is archived", archived.name),
                    None => format!("its agent {id} is gone"),
                };
                tracing::warn!(cron_id = %cron.cron_id, assistant_id = %id, "cron stalled: {why}");
                if cron.stalled.as_deref() != Some(why.as_str()) {
                    if let Some(who) = cron
                        .created_by
                        .as_ref()
                        .filter(|w| w.get("principal_id").is_some())
                    {
                        crate::notices::tell_in(
                            &state.server_store,
                            tenant,
                            who,
                            &format!("schedule:{external_cron_id}:stalled"),
                            json!({"kind": "schedule", "cron_id": external_cron_id, "state": "stalled", "assistant_id": id}),
                            &format!("Your schedule cannot run: {why}"),
                            "Nothing happens while the agent is put away. Restore the agent from the Agents list, or remove the schedule on its Triggers.",
                        )
                        .await;
                    }
                    // The row changes only once the maker has been told, so whoever
                    // sees the schedule stalled also finds the notice that says why.
                    if let Ok(Some(mut record)) = state.server_store.get_cron(&cron.cron_id).await {
                        record.stalled = Some(why.clone());
                        let _ = state.server_store.upsert_cron(&record).await;
                    }
                }
                return;
            }
            Err(error) => {
                tracing::warn!(cron_id = %cron.cron_id, %error, "cron skipped: agent read failed");
                return;
            }
        },
        None => None,
    };
    // A run this schedule fired is still waiting on a decision: the
    // schedule holds. Firing again would queue the same question behind
    // The schedule's own budget: fired as many times as it was allowed,
    // it stops and says so — once in its row, once to the person.
    if let Some(cap) = cron.max_runs {
        if cron.runs_fired >= cap {
            let why = format!(
                "it has run the {cap} time{} it was allowed",
                if cap == 1 { "" } else { "s" }
            );
            if cron.stalled.as_deref() != Some(why.as_str()) {
                tracing::info!(cron_id = %cron.cron_id, cap, fired = cron.runs_fired, "cron stalled: run cap reached");
                if let Some(who) = cron
                    .created_by
                    .as_ref()
                    .filter(|w| w.get("principal_id").is_some())
                {
                    crate::notices::tell_in(
                        &state.server_store,
                        tenant,
                        who,
                        &format!("schedule:{external_cron_id}:capped"),
                        json!({"kind": "schedule", "cron_id": external_cron_id, "state": "capped", "assistant_id": cron.assistant_id}),
                        &format!("Your schedule has stopped: {why}"),
                        "To keep it going, allow it more runs on the agent's Triggers, or remove the schedule there.",
                    )
                    .await;
                }
                // The row changes only once the maker has been told, so whoever
                // sees the schedule stalled also finds the notice that says why.
                if let Ok(Some(mut record)) = state.server_store.get_cron(&cron.cron_id).await {
                    record.stalled = Some(why.clone());
                    let _ = state.server_store.upsert_cron(&record).await;
                }
            }
            return;
        }
    }
    if let Some(cap) = cron.max_tokens {
        if cron.tokens_spent >= cap {
            let why = format!(
                "it has used up its spending limit ({} of {cap} tokens)",
                cron.tokens_spent
            );
            if cron.stalled.as_deref() != Some(why.as_str()) {
                tracing::info!(cron_id = %cron.cron_id, cap, spent = cron.tokens_spent, "cron stalled: token budget spent");
                if let Some(who) = cron
                    .created_by
                    .as_ref()
                    .filter(|w| w.get("principal_id").is_some())
                {
                    crate::notices::tell_in(
                        &state.server_store,
                        tenant,
                        who,
                        &format!("schedule:{external_cron_id}:capped"),
                        json!({"kind": "schedule", "cron_id": external_cron_id, "state": "capped", "assistant_id": cron.assistant_id}),
                        &format!("Your schedule has stopped: {why}"),
                        "To keep it going, raise its spending limit on the agent's Triggers, or remove the schedule there.",
                    )
                    .await;
                }
                // The row changes only once the maker has been told, so whoever
                // sees the schedule stalled also finds the notice that says why.
                if let Ok(Some(mut record)) = state.server_store.get_cron(&cron.cron_id).await {
                    record.stalled = Some(why.clone());
                    let _ = state.server_store.upsert_cron(&record).await;
                }
            }
            return;
        }
    }
    // the one nobody has answered yet; the row says it is held and why.
    if let Some(pending) = pending_run_of(&state, external_cron_id).await {
        let held = json!({"run_id": pending.run_id, "since": pending.requested_at});
        if cron.held.as_ref() != Some(&held) {
            if let Ok(Some(mut record)) = state.server_store.get_cron(&cron.cron_id).await {
                record.held = Some(held);
                if let Err(error) = state.server_store.upsert_cron(&record).await {
                    tracing::warn!(cron_id = %cron.cron_id, %error, "cron hold persistence failed");
                }
            }
        }
        tracing::info!(cron_id = %cron.cron_id, run_id = %pending.run_id, "cron held: a run it fired waits on a decision");
        return;
    }
    let record = ThreadRecord {
        thread_id: thread_id.clone(),
        tenant: tenant.to_string(),
        graph: cron.graph.clone(),
        metadata: json!({"cron_id": external_cron_id, "trigger": "cron", "assistant_id": cron.assistant_id}),
        forked_from: None,
        seed_length: None,
        created_at: Utc::now(),
    };
    // Persist like API-created threads: a cron-fired thread's checkpoints
    // must survive a restart too. (A `false` here means a UUID collision —
    // practically impossible; the run is still scheduled.)
    if let Err(error) = state
        .server_store
        .create_thread(&internal_thread_id, &record)
        .await
    {
        tracing::warn!(cron_id = %cron.cron_id, %error, "cron thread persistence failed");
        return;
    }

    // A schedule whose creator the deployment no longer holds does not
    // fire: the run would act in the name of someone who may not act.
    if let Some(created_by) = &cron.created_by {
        if !state.users.still_present(created_by) {
            tracing::warn!(cron_id = %cron.cron_id, "cron not fired: its creator is no longer a user of this deployment");
            return;
        }
    }
    let mut run_metadata = json!({"cron_id": external_cron_id, "channel": "schedule"});
    if let Some(created_by) = &cron.created_by {
        run_metadata["created_by"] = created_by.clone();
    }
    let mut payload = RunPayload {
        input: cron.input.clone(),
        metadata: Some(run_metadata),
        assistant_id: cron.assistant_id.clone(),
        // A schedule in a world fires every run in it; admission refuses the
        // run if the world is gone, so nothing reaches the live system.
        config: crate::worlds::run_config_for(cron.world.as_ref(), &cron.worlds),
        ..RunPayload::default()
    };
    if let Some(assistant) = &assistant {
        crate::routes::apply_assistant_defaults(
            &state,
            tenant,
            &internal_thread_id,
            assistant,
            &mut payload,
        )
        .await;
    }
    let fired_at = Utc::now();
    let scheduled = runs::schedule(
        &state.run_deps,
        &internal_thread_id,
        &thread_id,
        &cron.graph,
        payload,
        MultitaskStrategy::Enqueue,
    )
    .await;

    // Bookkeeping + persistence (best effort on the write).
    match state.server_store.get_cron(&cron.cron_id).await {
        Ok(Some(mut record)) => {
            record.last_run_at = Some(fired_at);
            record.runs_fired += 1;
            record.held = None;
            record.stalled = None;
            if let Err(error) = state.server_store.upsert_cron(&record).await {
                tracing::warn!(cron_id = %cron.cron_id, %error, "cron persistence failed");
            }
        }
        Ok(None) => {} // deleted between listing and firing
        Err(error) => {
            tracing::warn!(cron_id = %cron.cron_id, %error, "cron bookkeeping read failed")
        }
    }

    match scheduled {
        Ok(scheduled) => {
            // The run's spend follows the schedule: when its terminal lands,
            // its tokens are added to what the schedule has spent.
            {
                let mut terminal = scheduled.terminal.clone();
                let state = Arc::clone(&state);
                let cron_id = cron.cron_id.clone();
                tokio::spawn(async move {
                    let _ = terminal.wait_for(|v| v.is_some()).await;
                    let tokens = terminal
                        .borrow()
                        .as_ref()
                        .and_then(|v| v.get("spend"))
                        .and_then(|s| s.get("tokens"))
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    if tokens == 0 {
                        return;
                    }
                    if let Ok(Some(mut record)) = state.server_store.get_cron(&cron_id).await {
                        record.tokens_spent = record.tokens_spent.saturating_add(tokens);
                        if let Err(error) = state.server_store.upsert_cron(&record).await {
                            tracing::warn!(cron_id = %cron_id, %error, "cron spend bookkeeping failed");
                        }
                    }
                });
            }
            if cron.on_run_completed == OnRunCompleted::Delete {
                let mut terminal = scheduled.terminal;
                let _ = terminal.wait_for(|v| v.is_some()).await;
                match state.server_store.delete_cron(&cron.cron_id).await {
                    Ok(_) => {
                        tracing::info!(cron_id = %cron.cron_id, "one-shot cron deleted after run")
                    }
                    Err(error) => {
                        tracing::warn!(cron_id = %cron.cron_id, %error, "one-shot cron delete failed")
                    }
                }
            }
        }
        Err(error) => {
            tracing::warn!(cron_id = %cron.cron_id, %error, "cron run scheduling failed")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interval_cron(secs: u64) -> CronRecord {
        CronRecord {
            world: None,
            world_name: None,
            cron_id: "c".to_string(),
            graph: "g".to_string(),
            interval_secs: Some(secs),
            cron_expr: None,
            input: None,
            assistant_id: None,
            created_by: None,
            metadata: Value::Null,
            on_run_completed: OnRunCompleted::Keep,
            created_at: Utc::now(),
            last_run_at: None,
            held: None,
            stalled: None,
            max_runs: None,
            max_tokens: None,
            tokens_spent: 0,
            worlds: Vec::new(),
            world_names: Vec::new(),
            runs_fired: 0,
        }
    }

    #[test]
    fn validate_schedule_accepts_one_schedule_in_bounds() {
        assert!(validate_schedule(Some(1), None).is_ok());
        assert!(validate_schedule(Some(MAX_INTERVAL_SECS), None).is_ok());
        assert!(validate_schedule(None, Some("0 9 * * *")).is_ok());
    }

    #[test]
    fn validate_schedule_rejects_zero_huge_and_double_schedules() {
        assert!(validate_schedule(Some(0), None).is_err());
        // The fire-storm / scheduler-killing inputs from the DoS review.
        assert!(validate_schedule(Some(MAX_INTERVAL_SECS + 1), None).is_err());
        assert!(validate_schedule(Some(u64::MAX), None).is_err());
        assert!(validate_schedule(None, None).is_err());
        assert!(validate_schedule(Some(60), Some("0 9 * * *")).is_err());
    }

    #[test]
    fn an_interval_schedule_counts_from_its_last_firing_not_from_the_boot() {
        let now = Utc::now();
        // Made forty minutes ago, never fired: due twenty minutes on, not an hour.
        let mut cron = interval_cron(3600);
        cron.created_at = now - chrono::Duration::minutes(40);
        let due = first_due(&cron, now).unwrap();
        assert!(
            due > now + chrono::Duration::minutes(19) && due < now + chrono::Duration::minutes(21),
            "{due} vs {now}"
        );
        // Last fired ninety minutes ago: overdue, so due now — once.
        cron.last_run_at = Some(now - chrono::Duration::minutes(90));
        assert_eq!(first_due(&cron, now).unwrap(), now);
        // Last fired ten minutes ago: fifty to go.
        cron.last_run_at = Some(now - chrono::Duration::minutes(10));
        let due = first_due(&cron, now).unwrap();
        assert!(
            due > now + chrono::Duration::minutes(49) && due < now + chrono::Duration::minutes(51)
        );
        // A cron expression is its own next occurrence, boot or not.
        let mut expr = interval_cron(60);
        expr.interval_secs = None;
        expr.cron_expr = Some("0 9 * * *".to_string());
        assert!(first_due(&expr, now).unwrap() > now);
    }

    #[test]
    fn next_after_never_panics_and_never_returns_the_past() {
        let now = Utc::now();
        // In-range interval: next firing is in the near future.
        let due = next_after(&interval_cron(60), now).unwrap();
        assert!(due > now);
        assert!(due <= now + chrono::Duration::seconds(61));
        // Out-of-range legacy records are clamped, never negative-overflowed
        // or panicked on — a next-due in the past would fire every tick.
        let due = next_after(&interval_cron(u64::MAX), now).unwrap();
        assert!(due > now);
        let due = next_after(&interval_cron(0), now).unwrap();
        assert!(due > now);
    }
}
