//! Memory consolidation, first slice: the mechanical pass.
//!
//! Notes an agent writes accumulate: the same fact stated twice under one
//! key, the same sentence with a date changed, four versions of "the article
//! was rewritten and filed". This pass folds each such group into one
//! `Summary` record — the newest text, the group's key, the highest
//! importance — whose sources become superseded, through the core's
//! `consolidation_summary` (confidence the minimum of its sources, validity
//! spanning them, author `distiller:consolidation`, every source named so a
//! forget walks back through it). Nothing is edited or deleted; a person's
//! own notes and corrections are never folded; blocks are left to their
//! tool. Zero model calls. A cadence gate and a high-water mark keep it from
//! churning: it runs when enough new notes have landed since the last pass.
//! The model-written typed plan (merge by meaning, rewrite relative dates,
//! propose block edits) is the second slice.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use rusty_agent_runtime::memory::{
    consolidation_summary, superseded_set, MemoryKind, MemoryRecord, ProvenanceAuthor, ScopeAddress,
};
use rusty_agent_runtime::record::PayloadRef;
use serde_json::{json, Value};

use crate::auth::TenantContext;
use crate::routes::AppState;

const REPORT_KEY: &str = "memory-consolidation";
/// How many new live notes since the last pass make the next one due.
const DUE_AFTER_NEW_NOTES: usize = 10;
/// Two notes whose meaning-bearing words overlap this much say the same
/// thing.
const NEAR_DUPLICATE_JACCARD: f64 = 0.85;
const DISTILLER: &str = "consolidation";

/// What the last pass did.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct ConsolidationReport {
    pub stamp: Option<DateTime<Utc>>,
    /// Groups folded, each: the summary written and the sources it supersedes.
    pub folded: Vec<FoldedGroup>,
    /// `true` when the gate held the pass back (nothing new enough).
    pub skipped: bool,
    pub new_notes_seen: usize,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct FoldedGroup {
    pub scope: ScopeAddress,
    pub key: Option<String>,
    pub summary: String,
    pub sources: Vec<String>,
    pub why: String,
}

fn note_text(record: &MemoryRecord) -> Option<String> {
    match &record.content {
        PayloadRef::Inline(v) => v.get("text").and_then(Value::as_str).map(str::to_owned),
        _ => None,
    }
}

/// The meaning-bearing tokens of a note: words of four letters or more,
/// and every number however short — a day, a count, an incident number
/// is exactly what tells two otherwise identical notes apart.
fn terms(text: &str) -> BTreeSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 4 || w.chars().any(|c| c.is_ascii_digit()))
        .map(|w| w.to_lowercase())
        .collect()
}

fn jaccard(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let inter = a.intersection(b).count() as f64;
    let union = a.union(b).count() as f64;
    inter / union
}

/// A note the pass may fold: written by an agent or an earlier pass, not a
/// block, not a person's own words, not a correction.
fn foldable(record: &MemoryRecord) -> bool {
    matches!(
        record.provenance.author,
        ProvenanceAuthor::Agent { .. } | ProvenanceAuthor::Distiller { .. }
    ) && !record.tags.iter().any(|t| t == "block")
        && record.provenance.evidence.correction_id.is_none()
        && matches!(record.kind, MemoryKind::Fact | MemoryKind::Summary)
        && note_text(record).is_some()
}

/// The groups to fold in one scope: same key with two or more live notes,
/// then near-duplicate texts among the rest. Each note lands in one group.
fn groups(live: &[&MemoryRecord]) -> Vec<(Vec<usize>, String)> {
    let mut taken = vec![false; live.len()];
    let mut out: Vec<(Vec<usize>, String)> = Vec::new();
    let mut by_key: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, r) in live.iter().enumerate() {
        if let Some(k) = r.key.as_deref() {
            by_key.entry(k).or_default().push(i);
        }
    }
    for (key, members) in by_key {
        if members.len() >= 2 {
            for &i in &members {
                taken[i] = true;
            }
            let why = format!("{} live notes under the key `{key}`", members.len());
            out.push((members, why));
        }
    }
    let term_sets: Vec<BTreeSet<String>> = live
        .iter()
        .map(|r| terms(&note_text(r).unwrap_or_default()))
        .collect();
    for i in 0..live.len() {
        if taken[i] || term_sets[i].len() < 4 {
            continue;
        }
        let mut members = vec![i];
        for j in (i + 1)..live.len() {
            if taken[j] || term_sets[j].len() < 4 {
                continue;
            }
            if jaccard(&term_sets[i], &term_sets[j]) >= NEAR_DUPLICATE_JACCARD {
                members.push(j);
            }
        }
        if members.len() >= 2 {
            for &m in &members {
                taken[m] = true;
            }
            let why = format!(
                "{} notes that say the same thing (word overlap ≥ {:.0}%)",
                members.len(),
                NEAR_DUPLICATE_JACCARD * 100.0
            );
            out.push((members, why));
        }
    }
    out
}

/// Whether the pass is due: no pass yet, or enough live notes newer than
/// the last one.
fn due(universe: &[MemoryRecord], last: Option<DateTime<Utc>>) -> (bool, usize) {
    let Some(last) = last else {
        return (true, universe.len());
    };
    let new = universe.iter().filter(|r| r.created_at > last).count();
    (new >= DUE_AFTER_NEW_NOTES, new)
}

/// Run the pass over every scope the tenant holds.
pub(crate) async fn consolidate(
    state: &AppState,
    force: bool,
) -> Result<ConsolidationReport, String> {
    let tenant = TenantContext::new(crate::auth::DEFAULT_TENANT.to_owned(), Vec::new());
    let now = Utc::now();
    let universe = crate::routes::memory_universe(state, &tenant)
        .await
        .map_err(|e| format!("{e:?}"))?;
    let last = load(state).and_then(|r| r.stamp);
    let (is_due, new_notes_seen) = due(&universe, last);
    if !is_due && !force {
        let report = ConsolidationReport {
            stamp: last,
            folded: Vec::new(),
            skipped: true,
            new_notes_seen,
        };
        return Ok(report);
    }
    let superseded = superseded_set(&universe);
    let mut by_scope: BTreeMap<String, Vec<&MemoryRecord>> = BTreeMap::new();
    for r in &universe {
        let live =
            !superseded.contains(r.memory_id.as_str()) && r.expires_at.is_none_or(|e| e > now);
        if live && foldable(r) {
            by_scope.entry(r.scope.as_address()).or_default().push(r);
        }
    }
    let mut report = ConsolidationReport {
        stamp: Some(now),
        folded: Vec::new(),
        skipped: false,
        new_notes_seen,
    };
    for (_, mut live) in by_scope {
        live.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.memory_id.cmp(&b.memory_id))
        });
        for (members, why) in groups(&live) {
            let sources: Vec<MemoryRecord> = members.iter().map(|&i| live[i].clone()).collect();
            let newest = sources
                .iter()
                .max_by(|a, b| {
                    a.created_at
                        .cmp(&b.created_at)
                        .then_with(|| a.memory_id.cmp(&b.memory_id))
                })
                .expect("non-empty");
            let text = note_text(newest).unwrap_or_default();
            let key = sources
                .iter()
                .filter_map(|r| r.key.clone())
                .next()
                .or_else(|| newest.key.clone());
            let priority = sources.iter().map(|r| r.priority).max().unwrap_or(5);
            let mut tags: Vec<String> = sources
                .iter()
                .flat_map(|r| r.tags.iter().cloned())
                .collect();
            tags.sort();
            tags.dedup();
            let mut summary = match consolidation_summary(
                newest.scope.clone(),
                DISTILLER,
                &sources,
                json!({ "text": text }),
                now,
            ) {
                Ok(s) => s,
                Err(error) => {
                    tracing::warn!(%error, "a consolidation group was not folded");
                    continue;
                }
            }
            .with_priority(priority)
            .with_tags(tags);
            if let Some(k) = &key {
                summary = summary.with_key(k.clone());
            }
            if let Err(error) = state
                .server_store
                .put_memory(tenant.tenant(), &summary, &json!({ "text": text }))
                .await
            {
                tracing::warn!(%error, "consolidation summary not written");
                continue;
            }
            report.folded.push(FoldedGroup {
                scope: newest.scope.clone(),
                key,
                summary: summary.memory_id.clone(),
                sources: sources.iter().map(|r| r.memory_id.clone()).collect(),
                why,
            });
        }
    }
    crate::connectors::persist_json(&state.config.store_path, REPORT_KEY, &report)
        .await
        .map_err(|e| e.to_string())?;
    tracing::info!(
        groups = report.folded.len(),
        new_notes = new_notes_seen,
        "memory consolidation pass done"
    );
    Ok(report)
}

pub(crate) fn load(state: &AppState) -> Option<ConsolidationReport> {
    let bytes = std::fs::read(state.config.store_path.join(format!("{REPORT_KEY}.json"))).ok()?;
    serde_json::from_slice(&bytes).ok()
}
