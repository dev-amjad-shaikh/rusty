//! Memory utility: which notes helped.
//!
//! Every memory read a run makes is journaled with the ids it injected, and
//! every finished run carries the judge's evidence-based verdict. Joining the
//! two gives each note a record of realised usefulness — how often it was in
//! the prompt of a run that was *verified* to succeed, and how often in one
//! that failed. That is the ranking signal the recall lanes will use and the
//! defence against a note that is locally plausible and never helps: it
//! decays because it never earns.
//!
//! The roll-up is a query over journals and verdicts, never a model call. It
//! runs at boot and every six hours, and on demand; the index is persisted at
//! `{store_path}/memory-utility.json` and served by `GET /memory/utility`.

use std::sync::Arc;

use axum::{extract::State, Extension, Json};
use chrono::Utc;
use rusty_agent_runtime::journal::JournalSnapshot;
use rusty_agent_runtime::memory_tiers::{
    build_utility_index, RunOutcome, UtilityEntry, UtilityIndex, UtilityRun,
};
use rusty_agent_runtime::record::{EventStatus, PayloadRef, RunEventKind};
use serde_json::{json, Value};

use crate::auth::TenantContext;
use crate::error::ApiError;
use crate::routes::AppState;

const INDEX_KEY: &str = "memory-utility";

/// The index this process ranks with: published by the roll-up, loaded
/// from disk on first ask, read on every scored recall.
static CACHE: std::sync::OnceLock<std::sync::RwLock<Option<UtilityIndex>>> =
    std::sync::OnceLock::new();

fn cache() -> &'static std::sync::RwLock<Option<UtilityIndex>> {
    CACHE.get_or_init(|| std::sync::RwLock::new(None))
}

/// A note's smoothed success in basis points from the cached index, or
/// `None` when the note is unmeasured or no index has been rolled up.
pub(crate) fn cached_bps(memory_id: &str) -> Option<u32> {
    cache()
        .read()
        .ok()?
        .as_ref()?
        .entries
        .get(memory_id)
        .map(|e| e.smoothed_success_bps())
}

/// The cached index's roll-up stamp, when there is one.
pub(crate) fn cached_stamp() -> Option<chrono::DateTime<Utc>> {
    cache().read().ok()?.as_ref().map(|i| i.stamp)
}

fn publish(index: &UtilityIndex) {
    if let Ok(mut slot) = cache().write() {
        *slot = Some(index.clone());
    }
}
const EVERY: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);
const FIRST_AFTER: std::time::Duration = std::time::Duration::from_secs(45);

/// The verdict files: `{store_path}/verifications/{run_id}.json`, one per
/// judged run — the roll-up's population.
fn judged_run_ids(state: &AppState) -> Vec<String> {
    let dir = state.config.store_path.join("verifications");
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            entry
                .file_name()
                .to_str()?
                .strip_suffix(".json")
                .map(str::to_owned)
        })
        // The transcript a verdict keeps beside it is not a run.
        .filter(|name| !name.ends_with(".transcript"))
        .collect()
}

/// The verdict as an outcome: verified → success, failed → failure,
/// anything else (unverified, absent) → not evidence.
/// A journaled payload as a value: inline, or the spilled artifact the
/// snapshot carries — a lane-one read's output is spilled once it holds
/// more than a few notes, and a reader that only sees inline payloads
/// takes every such read for a miss.
fn payload(snapshot: &JournalSnapshot, reference: Option<&PayloadRef>) -> Option<Value> {
    match reference? {
        PayloadRef::Inline(v) => Some(v.clone()),
        PayloadRef::Artifact(artifact) => snapshot.artifacts.get(&artifact.sha256).cloned(),
    }
}

fn outcome_of(verdict: &Value) -> Option<RunOutcome> {
    match verdict.get("verdict").and_then(Value::as_str) {
        Some("verified") => Some(RunOutcome {
            status: EventStatus::Ok,
            score_bps: None,
        }),
        Some("failed") => Some(RunOutcome {
            status: EventStatus::Error,
            score_bps: None,
        }),
        _ => None,
    }
}

/// The ids a `memory.recall` tool call answered with — its output names the
/// notes it returned, so a recall the agent chose to make counts as a use
/// like the pipeline's own reads.
fn recalled_ids(snapshot: &JournalSnapshot) -> Vec<String> {
    let mut ids = Vec::new();
    for event in &snapshot.events {
        if event.kind != RunEventKind::ToolCall {
            continue;
        }
        let is_recall = event
            .input
            .as_ref()
            .and_then(|p| match p {
                PayloadRef::Inline(v) => v
                    .pointer("/value/tool")
                    .or_else(|| v.get("tool"))
                    .and_then(Value::as_str)
                    .map(|t| t == crate::platform_tools::MEMORY_RECALL),
                _ => None,
            })
            .unwrap_or(false);
        if !is_recall {
            continue;
        }
        let Some(out) = payload(snapshot, event.output.as_ref()) else {
            continue;
        };
        let value = out.get("value").unwrap_or(&out);
        let mut notes: Vec<&Value> = Vec::new();
        if let Some(list) = value.get("notes").and_then(Value::as_array) {
            notes.extend(list.iter());
        }
        if let Some(answers) = value.get("answers").and_then(Value::as_array) {
            for a in answers {
                if let Some(list) = a.get("notes").and_then(Value::as_array) {
                    notes.extend(list.iter());
                }
            }
        }
        ids.extend(
            notes
                .iter()
                .filter_map(|n| n.get("memory_id").and_then(Value::as_str))
                .map(str::to_owned),
        );
    }
    ids
}

/// Roll up every judged run's journal into the index and persist it.
/// Every judged run's journal with its outcome: what the roll-up and the
/// sweep read.
async fn load_judged(state: &AppState) -> Vec<(JournalSnapshot, RunOutcome)> {
    let mut loaded: Vec<(JournalSnapshot, RunOutcome)> = Vec::new();
    for run_id in judged_run_ids(state) {
        let Some(verdict) = state.verifications.load(&run_id) else {
            continue;
        };
        let Some(outcome) = outcome_of(&verdict) else {
            continue;
        };
        match state.server_store.get_journal(&run_id).await {
            Ok(Some(snapshot)) => loaded.push((snapshot, outcome)),
            Ok(None) => {}
            Err(error) => {
                tracing::debug!(%run_id, %error, "journal not read for the utility roll-up")
            }
        }
    }
    loaded
}

/// The sweep's pass over recall misses: every zero-recall gap whose
/// question a later run's recall answered closes now, without waiting
/// for the nightly roll-up. Every run with a verdict record counts, the
/// unverified ones too: a reply from memory alone calls no tool, so the
/// judge has nothing to verify it by — but the recall answered.
pub(crate) async fn close_answered_misses(state: &AppState) -> Vec<String> {
    // Bounded: the newest verdicts only. Every run's own hits close its
    // misses the moment its verdict lands (`close_hits_of_run`); this pass
    // is the catch-up for runs judged before that hook existed.
    let dir = state.config.store_path.join("verifications");
    let mut newest: Vec<(std::time::SystemTime, String)> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry
                .file_name()
                .to_str()?
                .strip_suffix(".json")?
                .to_owned();
            if name.ends_with(".transcript") {
                return None;
            }
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, name))
        })
        .collect();
    newest.sort_by_key(|b| std::cmp::Reverse(b.0));
    let mut loaded: Vec<(JournalSnapshot, RunOutcome)> = Vec::new();
    for (_, run_id) in newest.into_iter().take(SWEEP_RUNS) {
        if let Ok(Some(snapshot)) = state.server_store.get_journal(&run_id).await {
            loaded.push((
                snapshot,
                RunOutcome {
                    status: EventStatus::Ok,
                    score_bps: None,
                },
            ));
        }
    }
    close_recall_hits(state, &loaded).await
}

/// How many of the newest judged runs the sweep's catch-up reads.
const SWEEP_RUNS: usize = 60;

/// One run's recall hits close the zero-recall gaps they answer — run
/// from the after-verdict hook, so a miss closes the moment a later run
/// answers it, with no sweep in between.
pub(crate) async fn close_hits_of_run(state: Arc<AppState>, run_id: String) {
    let Ok(Some(snapshot)) = state.server_store.get_journal(&run_id).await else {
        return;
    };
    let loaded = vec![(
        snapshot,
        RunOutcome {
            status: EventStatus::Ok,
            score_bps: None,
        },
    )];
    let closed = close_recall_hits(&state, &loaded).await;
    if !closed.is_empty() {
        tracing::info!(run = %run_id, closed = closed.len(), "zero-recall gaps closed by this run's recall");
    }
}

pub(crate) async fn roll_up(state: &AppState) -> Result<UtilityIndex, String> {
    let stamp = Utc::now();
    let loaded = load_judged(state).await;
    let runs: Vec<UtilityRun<'_>> = loaded
        .iter()
        .map(|(snapshot, outcome)| UtilityRun {
            snapshot,
            outcome: *outcome,
        })
        .collect();
    // The core builder counts the pipeline's reads and refuses an
    // inconsistent journal; a run whose spilled payload is missing is
    // skipped rather than failing the whole roll-up.
    let mut index = match build_utility_index(&runs, None, stamp) {
        Ok(index) => index,
        Err(error) => {
            tracing::warn!(%error, "utility roll-up over all runs failed; rolling up run by run");
            let mut index = UtilityIndex {
                stamp,
                entries: Default::default(),
            };
            for run in &runs {
                if let Ok(one) = build_utility_index(std::slice::from_ref(run), None, stamp) {
                    for (id, entry) in one.entries {
                        let slot = index.entries.entry(id).or_default();
                        slot.successful_uses += entry.successful_uses;
                        slot.failed_uses += entry.failed_uses;
                    }
                }
            }
            index
        }
    };
    // The agent's own recalls, one use per run per note.
    for (snapshot, outcome) in &loaded {
        let mut seen = std::collections::BTreeSet::new();
        for id in recalled_ids(snapshot) {
            if !seen.insert(id.clone()) {
                continue;
            }
            let slot = index
                .entries
                .entry(id)
                .or_insert_with(UtilityEntry::default);
            match outcome.status {
                EventStatus::Ok => slot.successful_uses += 1,
                EventStatus::Error => slot.failed_uses += 1,
                EventStatus::Interrupted => {}
            }
        }
    }
    let misses = file_recall_misses(state, &loaded).await;
    if !misses.is_empty() {
        tracing::info!(
            misses = misses.len(),
            "zero-recall gaps filed from failed runs"
        );
    }
    let answered = close_recall_hits(state, &loaded).await;
    if !answered.is_empty() {
        tracing::info!(
            closed = answered.len(),
            "zero-recall gaps closed by runs whose recall answered"
        );
    }
    crate::connectors::persist_json(&state.config.store_path, INDEX_KEY, &index)
        .await
        .map_err(|e| e.to_string())?;
    publish(&index);
    tracing::info!(
        runs = loaded.len(),
        notes = index.entries.len(),
        "memory utility rolled up"
    );
    Ok(index)
}

/// The persisted index, if a roll-up has run.
/// The questions a run's lane-one recall answered nothing to: the text of
/// every journaled memory read that carried a query text (lane one reads
/// the turn's last message) and assembled no note.
fn recall_misses(snapshot: &JournalSnapshot) -> Vec<String> {
    let mut misses = Vec::new();
    for event in &snapshot.events {
        if event.kind != RunEventKind::MemoryRead {
            continue;
        }
        let Some(input) = payload(snapshot, event.input.as_ref()) else {
            continue;
        };
        let Some(text) = input
            .pointer("/query/text")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty())
        else {
            continue;
        };
        let answered = payload(snapshot, event.output.as_ref())
            .and_then(|out| {
                out.get("memory_ids")
                    .and_then(Value::as_array)
                    .map(Vec::len)
            })
            .unwrap_or(0);
        if answered == 0 && !misses.iter().any(|m| m == text) {
            misses.push(text.to_owned());
        }
    }
    misses
}

/// The questions a run's lane-one recall answered: the text of every
/// journaled memory read that carried a query text and assembled a note.
fn recall_hits(snapshot: &JournalSnapshot) -> Vec<String> {
    let mut hits = Vec::new();
    for event in &snapshot.events {
        if event.kind != RunEventKind::MemoryRead {
            continue;
        }
        let Some(input) = payload(snapshot, event.input.as_ref()) else {
            continue;
        };
        let Some(text) = input
            .pointer("/query/text")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty())
        else {
            continue;
        };
        let answered = payload(snapshot, event.output.as_ref())
            .and_then(|out| {
                out.get("memory_ids")
                    .and_then(Value::as_array)
                    .map(Vec::len)
            })
            .unwrap_or(0);
        if answered > 0 && !hits.iter().any(|h| h == text) {
            hits.push(text.to_owned());
        }
    }
    hits
}

/// The other half of the miss: a run whose recall answered its question
/// closes the zero-recall gap that question filed — the memory it said
/// was missing is there now. A run that failed for another reason still
/// had its answer from memory; only a failed recall says memory was short.
async fn close_recall_hits(
    state: &AppState,
    loaded: &[(JournalSnapshot, RunOutcome)],
) -> Vec<String> {
    let tenant = TenantContext::new(crate::auth::DEFAULT_TENANT.to_owned(), Vec::new());
    let mut closed = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    let mut hits_total = 0usize;
    for (snapshot, _outcome) in loaded {
        let hits = recall_hits(snapshot);
        hits_total += hits.len();
        for question in hits {
            let question: String = question.chars().take(200).collect();
            if seen.contains(&question) {
                continue;
            }
            seen.push(question.clone());
            closed.extend(crate::routes::close_zero_recall_gaps(state, &tenant, &question).await);
        }
    }
    tracing::info!(
        runs = loaded.len(),
        hits = hits_total,
        questions = seen.len(),
        closed = closed.len(),
        "recall hits read for zero-recall closure"
    );
    closed
}

/// Memory misses that mattered: a judged run that was not verified and
/// whose recall answered nothing to its question files a zero-recall gap
/// on that question shape — the demand side of memory, M1's "miss → gap".
/// A verified run that recalled nothing needed nothing; only the failures
/// say memory was short. Idempotent through the ledger: the same shape
/// re-filed reinforces one gap.
async fn file_recall_misses(
    state: &AppState,
    loaded: &[(JournalSnapshot, RunOutcome)],
) -> Vec<(String, String)> {
    use rusty_agent_runtime::gaps::{
        Citation, CitationKind, ClosureCriteria, GapOrigin, GapSubject,
    };
    let mut wanted: Vec<(String, String)> = Vec::new();
    for (snapshot, outcome) in loaded {
        if outcome.status != EventStatus::Error {
            continue;
        }
        for question in recall_misses(snapshot) {
            let question: String = question.chars().take(200).collect();
            // A question: an instruction that failed ("say hello and stop",
            // "list the open incidents") did not fail for want of memory.
            let asks = question.contains('?')
                || question.split(['.', ':', ';', '!']).any(|s| {
                    let first = s
                        .split_whitespace()
                        .next()
                        .unwrap_or("")
                        .trim_matches(|c: char| !c.is_alphanumeric())
                        .to_lowercase();
                    matches!(
                        first.as_str(),
                        "what"
                            | "which"
                            | "who"
                            | "where"
                            | "when"
                            | "why"
                            | "how"
                            | "is"
                            | "are"
                            | "does"
                            | "do"
                            | "can"
                            | "could"
                            | "would"
                            | "should"
                            | "did"
                            | "was"
                            | "were"
                            | "has"
                            | "have"
                    )
                });
            if asks && !wanted.iter().any(|(q, _)| q == &question) {
                wanted.push((question, snapshot.run_id.clone()));
            }
        }
    }
    tracing::info!(
        failed_runs = loaded
            .iter()
            .filter(|(_, o)| o.status == EventStatus::Error)
            .count(),
        candidates = wanted.len(),
        "recall misses considered"
    );
    if wanted.is_empty() {
        return Vec::new();
    }
    let tenant = TenantContext::new(crate::auth::DEFAULT_TENANT.to_owned(), Vec::new());
    let now = Utc::now();
    let filed = crate::routes::mutate_gap_ledger(state, &tenant, |ledger| {
        let mut out = Vec::new();
        for (question, run_id) in &wanted {
            let subject = match GapSubject::question_shape(question) {
                Ok(subject) => subject,
                Err(error) => {
                    tracing::warn!(%error, %question, "recall miss subject not built");
                    continue;
                }
            };
            match ledger.file_gap(
                subject,
                format!("Memory answered nothing to this question and the run was not verified: {question}"),
                vec![Citation { kind: CitationKind::RunReceipt, id: run_id.clone(), note: Some("the failed run whose recall answered nothing".to_owned()) }],
                GapOrigin::ZeroRecall,
                ClosureCriteria::BlockFilled { block_label: question.clone() },
                1,
                0,
                "runtime:recall-miss",
                now,
            ) {
                Ok(id) => out.push((id, run_id.clone())),
                Err(error) => tracing::warn!(%error, %question, "recall miss not filed"),
            }
        }
        Ok(out)
    })
    .await;
    match filed {
        Ok(ids) => {
            if !ids.is_empty() {
                tracing::info!(count = ids.len(), "recall misses filed as gaps");
            }
            ids
        }
        Err(error) => {
            tracing::warn!(?error, "recall misses not filed");
            Vec::new()
        }
    }
}

pub(crate) fn load(state: &AppState) -> Option<UtilityIndex> {
    if let Some(index) = cache().read().ok().and_then(|c| c.clone()) {
        return Some(index);
    }
    let bytes = std::fs::read(state.config.store_path.join(format!("{INDEX_KEY}.json"))).ok()?;
    let index: UtilityIndex = serde_json::from_slice(&bytes).ok()?;
    publish(&index);
    Some(index)
}

/// A note that is old, agent-written, and has never been in the prompt of
/// a verified run: flagged for a person (and, later, the consolidation
/// pass), never reaped on its own — a fact can be true and unasked.
const NEVER_HELPED_AFTER_DAYS: i64 = 30;

/// What the sweep did and found, persisted beside the index.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct SweepReport {
    pub stamp: Option<chrono::DateTime<Utc>>,
    /// Expired and never useful: forgotten, with a tombstone each.
    pub reaped: Vec<String>,
    /// Old, agent-written, never in a verified run's prompt: for a person.
    pub never_helped: Vec<String>,
    /// Of those, the ones with no source run behind them: decayed — marked
    /// in the index so they rank last at recall until a verified run uses
    /// them. A sourced fact can be true and unasked; an unsourced one that
    /// never helped is noise the agent should stop improving from.
    #[serde(default)]
    pub decayed: Vec<String>,
}

const SWEEP_KEY: &str = "memory-sweep";

/// The sweep: reap what `forgetting_candidates` names — expired and never
/// useful — through the same plan and tombstone as a person's forget, and
/// list the never-helped for the drawer.
/// The never-helped notes with no source run behind them: what the sweep
/// decays. Pure, so the rule is testable without a store.
pub(crate) fn decay_candidates(
    universe: &[rusty_agent_runtime::memory::MemoryRecord],
    index: &UtilityIndex,
    now: chrono::DateTime<Utc>,
) -> Vec<String> {
    use rusty_agent_runtime::memory::ProvenanceAuthor;
    let superseded = rusty_agent_runtime::memory::superseded_set(universe);
    let cutoff = now - chrono::Duration::days(NEVER_HELPED_AFTER_DAYS);
    let mut out: Vec<String> = universe
        .iter()
        .filter(|r| {
            matches!(r.provenance.author, ProvenanceAuthor::Agent { .. })
                && r.provenance.evidence.run_id.is_none()
                && r.created_at <= cutoff
                && !superseded.contains(r.memory_id.as_str())
                && !r.tags.iter().any(|t| t == "block")
                && index
                    .entries
                    .get(&r.memory_id)
                    .is_none_or(|e| e.successful_uses == 0)
        })
        .map(|r| r.memory_id.clone())
        .collect();
    out.sort();
    out
}

pub(crate) async fn sweep(
    state: &AppState,
    index: &mut UtilityIndex,
) -> Result<SweepReport, String> {
    use rusty_agent_runtime::memory::{plan_forget, MemoryForgetTombstone, ProvenanceAuthor};
    use rusty_agent_runtime::memory_tiers::forgetting_candidates;
    let tenant = TenantContext::new(crate::auth::DEFAULT_TENANT.to_owned(), Vec::new());
    let now = Utc::now();
    let universe = crate::routes::memory_universe(state, &tenant)
        .await
        .map_err(|e| format!("{e:?}"))?;
    let mut report = SweepReport {
        stamp: Some(now),
        ..Default::default()
    };
    for memory_id in forgetting_candidates(index, &universe, now) {
        let Some(record) = universe.iter().find(|r| r.memory_id == memory_id) else {
            continue;
        };
        let plan = plan_forget(&universe, std::slice::from_ref(&memory_id));
        for id in plan.forgotten.iter().chain(plan.invalidated.iter()) {
            if let Err(error) = state.server_store.delete_memory(tenant.tenant(), id).await {
                tracing::warn!(%id, %error, "expired note not forgotten");
            }
        }
        let tombstone = MemoryForgetTombstone {
            memory_id: memory_id.clone(),
            scope: record.scope.clone(),
            reason: rusty_agent_runtime::memory::ForgetReason::Expired,
            invalidated: plan.invalidated,
        };
        tracing::info!(memory_id = %memory_id, invalidated = tombstone.invalidated.len(), "expired note reaped");
        report.reaped.push(memory_id);
    }
    let superseded = rusty_agent_runtime::memory::superseded_set(&universe);
    let cutoff = now - chrono::Duration::days(NEVER_HELPED_AFTER_DAYS);
    report.never_helped = universe
        .iter()
        .filter(|r| {
            matches!(r.provenance.author, ProvenanceAuthor::Agent { .. })
                && r.created_at <= cutoff
                && !superseded.contains(r.memory_id.as_str())
                && !r.tags.iter().any(|t| t == "block")
                && index
                    .entries
                    .get(&r.memory_id)
                    .is_none_or(|e| e.successful_uses == 0)
        })
        .map(|r| r.memory_id.clone())
        .collect();
    report.never_helped.sort();
    // Decay: unsourced and never helped — marked in the index, ranked last.
    report.decayed = decay_candidates(&universe, index, now);
    let mut newly = 0usize;
    for id in &report.decayed {
        let entry = index.entries.entry(id.clone()).or_default();
        if entry.decayed_at.is_none() {
            entry.decayed_at = Some(now);
            newly += 1;
        }
    }
    if newly > 0 {
        crate::connectors::persist_json(&state.config.store_path, INDEX_KEY, &*index)
            .await
            .map_err(|e| e.to_string())?;
        publish(index);
        tracing::info!(decayed = newly, "unsourced notes that never helped decayed");
    }
    crate::connectors::persist_json(&state.config.store_path, SWEEP_KEY, &report)
        .await
        .map_err(|e| e.to_string())?;
    tracing::info!(
        reaped = report.reaped.len(),
        never_helped = report.never_helped.len(),
        "memory sweep done"
    );
    Ok(report)
}

/// The last sweep's report, if one has run.
pub(crate) fn load_sweep(state: &AppState) -> Option<SweepReport> {
    let bytes = std::fs::read(state.config.store_path.join(format!("{SWEEP_KEY}.json"))).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Roll up shortly after boot, then every six hours.
pub(crate) fn spawn_roll_up(state: Arc<AppState>) {
    // The last index serves until the first roll-up of this process.
    let _ = load(&state);
    tokio::spawn(async move {
        tokio::time::sleep(FIRST_AFTER).await;
        loop {
            let _one_at_a_time = ROLL_LOCK.lock().await;
            match roll_up(&state).await {
                Ok(mut index) => {
                    if let Err(error) = sweep(&state, &mut index).await {
                        tracing::warn!(%error, "memory sweep failed");
                    }
                    if let Err(error) =
                        crate::memory_consolidation::consolidate(&state, false).await
                    {
                        tracing::warn!(%error, "memory consolidation failed");
                    }
                }
                Err(error) => tracing::warn!(%error, "memory utility roll-up failed"),
            }
            tokio::time::sleep(EVERY).await;
        }
    });
}

/// Whether a roll-up asked for by hand is running in this process: the
/// on-demand roll-up answers at once and works in the background, and a
/// second ask while one runs joins it rather than starting another.
static ROLLING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// One roll-up at a time in this process: the scheduled one, the one
/// asked for by hand and the one a first read falls back to all write
/// the same index file, and two at once raced on its rename.
static ROLL_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// `GET /memory/utility` — the index: per note, how many verified runs it
/// was read into, how many failed ones, and the smoothed success rate.
pub(crate) async fn get_memory_utility(
    State(state): State<Arc<AppState>>,
    Extension(_tenant): Extension<TenantContext>,
) -> Result<Json<Value>, ApiError> {
    let index = match load(&state) {
        Some(index) => index,
        None => {
            let _one_at_a_time = ROLL_LOCK.lock().await;
            match load(&state) {
                Some(index) => index,
                None => roll_up(&state).await.map_err(ApiError::internal)?,
            }
        }
    };
    let entries: serde_json::Map<String, Value> = index
        .entries
        .iter()
        .map(|(id, e)| {
            (
                id.clone(),
                json!({
                    "successful_uses": e.successful_uses,
                    "failed_uses": e.failed_uses,
                    "smoothed_success_bps": e.smoothed_success_bps(),
                }),
            )
        })
        .collect();
    let sweep = load_sweep(&state).unwrap_or_default();
    let consolidation = crate::memory_consolidation::load(&state).unwrap_or_default();
    Ok(Json(
        json!({ "stamp": index.stamp, "notes": entries.len(), "entries": entries, "sweep": sweep, "consolidation": consolidation, "rolling": ROLLING.load(std::sync::atomic::Ordering::SeqCst) }),
    ))
}

/// `POST /memory/utility/roll-up` — roll up now: answered at once with
/// `202 {started, rolling, stamp}` (the stamp of the index that serves
/// meanwhile), the roll-up, the sweep and the consolidation running in
/// the background; `GET /memory/utility` says `rolling` until they end
/// and then carries the new stamp. Asked again while one runs, it answers
/// `started: false` and the running one stands.
pub(crate) async fn post_memory_utility_roll_up(
    State(state): State<Arc<AppState>>,
    Extension(_tenant): Extension<TenantContext>,
) -> Result<(axum::http::StatusCode, Json<Value>), ApiError> {
    use std::sync::atomic::Ordering;
    let stamp = load(&state).map(|index| index.stamp);
    if ROLLING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Ok((
            axum::http::StatusCode::ACCEPTED,
            Json(json!({ "started": false, "rolling": true, "stamp": stamp })),
        ));
    }
    let worker = Arc::clone(&state);
    tokio::spawn(async move {
        let _one_at_a_time = ROLL_LOCK.lock().await;
        match roll_up(&worker).await {
            Ok(mut index) => {
                match sweep(&worker, &mut index).await {
                    Ok(report) => tracing::info!(
                        notes = index.entries.len(),
                        reaped = report.reaped.len(),
                        never_helped = report.never_helped.len(),
                        "memory utility rolled up by hand"
                    ),
                    Err(error) => tracing::warn!(%error, "memory sweep failed"),
                }
                if let Err(error) = crate::memory_consolidation::consolidate(&worker, true).await {
                    tracing::warn!(%error, "memory consolidation failed");
                }
            }
            Err(error) => tracing::warn!(%error, "memory utility roll-up failed"),
        }
        ROLLING.store(false, Ordering::SeqCst);
    });
    Ok((
        axum::http::StatusCode::ACCEPTED,
        Json(json!({ "started": true, "rolling": true, "stamp": stamp })),
    ))
}

#[cfg(test)]
mod decay_tests {
    use super::*;
    use rusty_agent_runtime::memory::{
        MemoryEvidence, MemoryKind, MemoryProvenance, MemoryRecord, MemoryScope, ProvenanceAuthor,
        ScopeAddress, ValidityWindow,
    };
    use rusty_agent_runtime::memory_tiers::UtilityEntry;

    fn note(text: &str, days_old: i64, sourced: bool, now: chrono::DateTime<Utc>) -> MemoryRecord {
        let at = now - chrono::Duration::days(days_old);
        MemoryRecord::new(
            MemoryKind::Fact,
            ScopeAddress::new(MemoryScope::Agent, "desk"),
            MemoryProvenance {
                author: ProvenanceAuthor::Agent {
                    agent_id: "desk".to_owned(),
                },
                evidence: MemoryEvidence {
                    run_id: if sourced {
                        Some("run-1".to_owned())
                    } else {
                        None
                    },
                    ..Default::default()
                },
                written_at: at,
            },
            0.8,
            ValidityWindow {
                valid_from: at,
                valid_until: None,
            },
            at,
            serde_json::json!({"text": text}),
        )
        .unwrap()
    }

    #[test]
    fn unsourced_old_never_helped_notes_decay_and_nothing_else_does() {
        let now = Utc::now();
        let old_unsourced = note("the printer is on floor 2", 40, false, now);
        let old_sourced = note("the VPN gateway is vpn.example.test", 40, true, now);
        let young_unsourced = note("the badge office opens at 9", 3, false, now);
        let old_unsourced_helped = note("the wifi password rotates monthly", 40, false, now);
        let mut index = UtilityIndex {
            stamp: now,
            entries: Default::default(),
        };
        index.entries.insert(
            old_unsourced_helped.memory_id.clone(),
            UtilityEntry {
                successful_uses: 2,
                ..Default::default()
            },
        );
        let universe = vec![
            old_unsourced.clone(),
            old_sourced,
            young_unsourced,
            old_unsourced_helped,
        ];
        let decayed = decay_candidates(&universe, &index, now);
        assert_eq!(decayed, vec![old_unsourced.memory_id.clone()]);
        // A decayed note ranks last: its utility reads zero until a verified run uses it.
        let entry = UtilityEntry {
            decayed_at: Some(now),
            ..Default::default()
        };
        assert_eq!(entry.smoothed_success_bps(), 0);
        let used = UtilityEntry {
            decayed_at: Some(now),
            successful_uses: 1,
            ..Default::default()
        };
        assert!(used.smoothed_success_bps() > 0);
    }
}
