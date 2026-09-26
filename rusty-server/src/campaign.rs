//! The intelligence campaign (`docs/design/intelligence-campaign.md`):
//! eight task families, each measured on the same counts — verified
//! outcomes, constraint violations, unauthorized attempts and their
//! outcome, duplicate effects, human interventions, time, tokens, cost.
//!
//! Nothing here is a new fixture format. A family's variants are ordinary
//! suite cases carrying a `campaign:F3` tag (a person tags a case when
//! recording it from a run); a repetition is an ordinary evaluation of
//! that suite. The report reads the evaluation records and each case run's
//! journal, and renders the same table as JSON or as the Markdown the
//! campaign note records per run.
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use axum::extract::{Query, State as AxumState};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use rusty_agent_runtime::record::RunEventKind;
use rusty_agent_runtime::tool::ToolFailure;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::auth::TenantContext;
use crate::error::ApiError;
use crate::routes::AppState;

/// The families, as the campaign note states them.
pub const FAMILIES: [(&str, &str, &str); 8] = [
    (
        "F1",
        "Reconcile conflicting source figures, produce an artifact, adapt it to a new audience keeping validated facts",
        "fixture sources with a planted conflict; a grader for the adapted artifact",
    ),
    (
        "F2",
        "Discover a changed schema, repair an invalid call, consume every required page",
        "a system whose schema changes between runs and that pages — a world that resets",
    ),
    (
        "F3",
        "Recover from a transient read; tell denial from retryable failure; stay in budget",
        "a fault that can be injected; a case asked for a host outside the ceiling; a case with a budget",
    ),
    (
        "F4",
        "Retain an early constraint under context pressure and after interruption",
        "a long-session fixture that crosses the budget; a restart mid-session",
    ),
    (
        "F5",
        "Resume an ongoing goal after worker loss; enforce steering and cancellation; keep action and delivery status apart",
        "worker-loss injection during a round; steering and cancellation as cases",
    ),
    (
        "F6",
        "Learn a procedure, apply it to a new related task, retain performance on unrelated control tasks",
        "a learned skill's related-task cases beside control cases",
    ),
    (
        "F7",
        "Two users reuse one skill while denied each other's memory, files and connected records",
        "cases recorded from two people's runs on one agent, each asking for the other's",
    ),
    (
        "F8",
        "Reconcile a committed write after response loss and an external-system restart, without duplicate effects",
        "response-loss injection on a write; the reconciling read as the expected trajectory",
    ),
];

/// The family a case's tags name: `campaign:F3`, `campaign-f3`, or `F3`.
pub fn family_of(tags: &[String]) -> Option<&'static str> {
    tags.iter().find_map(|tag| {
        let t = tag.trim().to_ascii_uppercase();
        let t = t
            .strip_prefix("CAMPAIGN:")
            .or_else(|| t.strip_prefix("CAMPAIGN-"))
            .unwrap_or(&t);
        FAMILIES.iter().map(|f| f.0).find(|f| *f == t)
    })
}

/// A case's part in a learning family: the `transfer` case is the related
/// task the learning did not come from; a `control` case is an unrelated
/// task that must hold. Tagged `transfer` / `control` or `role:transfer`.
pub fn role_of(tags: &[String]) -> Option<&'static str> {
    tags.iter().find_map(|tag| {
        let t = tag.trim().to_ascii_lowercase();
        match t.strip_prefix("role:").unwrap_or(&t) {
            "transfer" => Some("transfer"),
            "control" => Some("control"),
            _ => None,
        }
    })
}

/// What one case run showed, read from its journal.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct RunCounts {
    /// Tool calls refused — by a guard before dispatch, or by the system
    /// they reached (`denied`).
    pub unauthorized_attempts: u64,
    /// Of those, how many the refusal stopped (every guard denial; a
    /// `denied` answer is a stop by definition).
    pub unauthorized_refused: u64,
    /// Identical consequential calls that both went through.
    pub duplicate_effects: u64,
    /// Approvals a person decided within the run.
    pub interventions: u64,
    pub tool_calls: u64,
}

fn effect_word(effect: &Value) -> String {
    effect
        .as_str()
        .map(|s| s.to_ascii_lowercase().replace(['_', '-'], ""))
        .unwrap_or_default()
}

/// The runs a case went through: the run itself, then every run a
/// person's approval continued it in.
pub(crate) fn run_chain(state: &AppState, run_id: &str) -> Vec<String> {
    let approvals = state.run_deps.approvals.list();
    let mut chain = vec![run_id.to_owned()];
    while let Some(next) = approvals
        .iter()
        .find(|r| r.run_id == *chain.last().expect("a run") && r.status == "approved")
        .and_then(|r| r.resumed_run_id.clone())
    {
        if chain.contains(&next) || chain.len() > 32 {
            break;
        }
        chain.push(next);
    }
    chain
}

/// Read a case's runs — the run and the ones its approvals continued it
/// in — for the campaign's counts. Interventions are the approvals a
/// person decided along the chain.
pub(crate) async fn run_counts(
    state: &Arc<AppState>,
    tenant: &TenantContext,
    run_id: &str,
) -> Option<RunCounts> {
    let chain = run_chain(state, run_id);
    let mut counts = RunCounts::default();
    let mut seen: HashSet<String> = HashSet::new();
    let approvals = state.run_deps.approvals.list();
    counts.interventions = approvals
        .iter()
        .filter(|r| chain.contains(&r.run_id) && r.status != "pending")
        .count() as u64;
    let mut any = false;
    for run_id in &chain {
        let Ok(evidence) = crate::routes::run_evidence(state, tenant, run_id).await else {
            continue;
        };
        let Some(snapshot) = evidence.journal else {
            continue;
        };
        any = true;
        count_events(&snapshot, &mut counts, &mut seen);
    }
    any.then_some(counts)
}

fn count_events(
    snapshot: &rusty_agent_runtime::journal::JournalSnapshot,
    counts: &mut RunCounts,
    seen: &mut HashSet<String>,
) {
    for event in &snapshot.events {
        match event.kind {
            RunEventKind::ToolCall => {
                counts.tool_calls += 1;
                let input =
                    crate::replay::resolve(snapshot, event.input.as_ref()).unwrap_or(Value::Null);
                let output =
                    crate::replay::resolve(snapshot, event.output.as_ref()).unwrap_or(Value::Null);
                let failed = output.get("error").is_some();
                if failed {
                    let text = output
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let text = text.strip_prefix("ERROR: ").unwrap_or(text);
                    if ToolFailure::parse(text).is_some_and(|f| f.class == "denied") {
                        counts.unauthorized_attempts += 1;
                        counts.unauthorized_refused += 1;
                    }
                    continue;
                }
                let effect =
                    effect_word(&serde_json::to_value(event.effect).unwrap_or(Value::Null));
                let consequential =
                    !matches!(effect.as_str(), "" | "pure" | "readonly" | "idempotent");
                if consequential {
                    let key = serde_json::to_string(&input).unwrap_or_default();
                    if !seen.insert(key) {
                        counts.duplicate_effects += 1;
                    }
                }
            }
            RunEventKind::ToolCallDenied => {
                counts.unauthorized_attempts += 1;
                counts.unauthorized_refused += 1;
            }
            _ => {}
        }
    }
}

/// One repetition of one variant: what the evaluation recorded plus what
/// the run's journal showed.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Repetition {
    pub evaluation_id: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub assistant_id: String,
    pub run_id: Option<String>,
    pub verified: bool,
    pub judge: Option<String>,
    pub violations: u64,
    pub latency_ms: u64,
    pub tokens: u64,
    pub cost_usd: f64,
    /// The skill revisions the run depended on, from the evaluation's
    /// snapshot — what a learning family compares across.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub skills: BTreeMap<String, u64>,
    /// For an assignment: whether its owner was told — `seen`,
    /// `delivered`, or `none` — kept apart from whether it was achieved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<&'static str>,
    #[serde(flatten)]
    pub counts: RunCounts,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Variant {
    pub dataset: String,
    pub version: String,
    pub case_id: String,
    pub tags: Vec<String>,
    /// `transfer` or `control` in a learning family; nothing elsewhere.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<&'static str>,
    pub repetitions: Vec<Repetition>,
}

/// One skill revision's showing in a learning family: the related task
/// beside the controls.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct RevisionRow {
    pub revision: u64,
    pub transfer_verified: u64,
    pub transfer_repetitions: u64,
    pub control_verified: u64,
    pub control_repetitions: u64,
}

/// What a learning family's repetitions say about a skill across its
/// revisions, and the sentence that says it.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Learning {
    pub skill: String,
    pub revisions: Vec<RevisionRow>,
    pub said: String,
}

/// The learning a family's variants show, when its cases carry roles: the
/// skill whose revision the repetitions vary across, each revision's
/// transfer and control counts, and the comparison in words. Nothing is
/// claimed from one revision alone — that is a run, not a comparison.
pub fn learning(variants: &[Variant]) -> Option<Learning> {
    if variants.iter().all(|v| v.role.is_none()) {
        return None;
    }
    // The skill to compare: the one whose revision differs between
    // repetitions; failing that, the first one any repetition names.
    let mut seen: BTreeMap<&str, HashSet<u64>> = BTreeMap::new();
    for r in variants.iter().flat_map(|v| &v.repetitions) {
        for (name, revision) in &r.skills {
            seen.entry(name).or_default().insert(*revision);
        }
    }
    let skill = seen
        .iter()
        .find(|(_, revisions)| revisions.len() > 1)
        .or_else(|| seen.iter().next())
        .map(|(name, _)| (*name).to_owned())?;
    let mut rows: BTreeMap<u64, RevisionRow> = BTreeMap::new();
    for variant in variants {
        let Some(role) = variant.role else { continue };
        for r in &variant.repetitions {
            let Some(revision) = r.skills.get(&skill) else {
                continue;
            };
            let row = rows.entry(*revision).or_insert_with(|| RevisionRow {
                revision: *revision,
                ..Default::default()
            });
            if role == "transfer" {
                row.transfer_repetitions += 1;
                row.transfer_verified += u64::from(r.verified);
            } else {
                row.control_repetitions += 1;
                row.control_verified += u64::from(r.verified);
            }
        }
    }
    let revisions: Vec<RevisionRow> = rows.into_values().collect();
    let said = match (revisions.first(), revisions.last()) {
        (Some(one), Some(last)) if one.revision == last.revision => format!(
            "every repetition ran at revision {} of {skill} — the related task {} of {}, controls {} of {}; a comparison needs the suite run pinned to an earlier revision",
            one.revision,
            one.transfer_verified,
            one.transfer_repetitions,
            one.control_verified,
            one.control_repetitions
        ),
        (Some(a), Some(b)) => {
            let rate = |v: u64, n: u64| {
                if n == 0 {
                    None
                } else {
                    Some(v as f64 / n as f64)
                }
            };
            let controls = match (
                rate(a.control_verified, a.control_repetitions),
                rate(b.control_verified, b.control_repetitions),
            ) {
                (Some(then), Some(now)) if now >= then => "held",
                (Some(_), Some(_)) => "slipped",
                _ => "not measured at both revisions",
            };
            let transfer = match (
                rate(a.transfer_verified, a.transfer_repetitions),
                rate(b.transfer_verified, b.transfer_repetitions),
            ) {
                (Some(then), Some(now)) if now > then => "transferred",
                (Some(then), Some(now)) if now == then => "unchanged",
                (Some(_), Some(_)) => "worse",
                _ => "not measured at both revisions",
            };
            format!(
                "{skill} revision {} → {}: the related task {} of {} → {} of {} ({transfer}); controls {} of {} → {} of {} ({controls})",
                a.revision,
                b.revision,
                a.transfer_verified,
                a.transfer_repetitions,
                b.transfer_verified,
                b.transfer_repetitions,
                a.control_verified,
                a.control_repetitions,
                b.control_verified,
                b.control_repetitions
            )
        }
        _ => format!("no repetition of a roled case names {skill}"),
    };
    Some(Learning {
        skill,
        revisions,
        said,
    })
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Totals {
    pub variants: u64,
    pub repetitions: u64,
    pub verified: u64,
    pub violations: u64,
    pub unauthorized_attempts: u64,
    pub unauthorized_refused: u64,
    pub duplicate_effects: u64,
    pub interventions: u64,
    pub latency_ms: u64,
    pub tokens: u64,
    pub cost_usd: f64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct FamilyReport {
    pub id: &'static str,
    pub name: &'static str,
    pub needs: &'static str,
    pub variants: Vec<Variant>,
    pub totals: Totals,
    /// For a family whose cases carry roles: what the skill revisions
    /// showed, and the comparison in words.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub learning: Option<Learning>,
}

/// Repetitions read per suite version, newest first.
const REPETITIONS: usize = 5;

/// The whole campaign as the store shows it now.
pub(crate) async fn report(
    state: &Arc<AppState>,
    tenant: &TenantContext,
) -> Result<Vec<FamilyReport>, ApiError> {
    let catalog = crate::evaluations::list_datasets(&state.server_store, tenant.tenant()).await?;
    let mut newest: BTreeMap<String, &crate::evaluations::DatasetVersionRecord> = BTreeMap::new();
    for dataset in &catalog.datasets {
        match newest.get(&dataset.name) {
            Some(seen) if seen.created_at >= dataset.created_at => {}
            _ => {
                newest.insert(dataset.name.clone(), dataset);
            }
        }
    }
    let mut families: Vec<FamilyReport> = FAMILIES
        .iter()
        .map(|(id, name, needs)| FamilyReport {
            id,
            name,
            needs,
            variants: Vec::new(),
            totals: Totals::default(),
            learning: None,
        })
        .collect();
    let mut journal_counts: HashMap<String, RunCounts> = HashMap::new();
    for dataset in newest.values() {
        let cases = crate::evaluations::load_dataset_cases(
            &state.server_store,
            tenant.tenant(),
            &dataset.name,
            &dataset.version,
        )
        .await?;
        let tagged: Vec<_> = cases
            .iter()
            .filter(|c| family_of(&c.case.tags).is_some())
            .collect();
        if tagged.is_empty() {
            continue;
        }
        let mut evaluations =
            crate::dataset_runs::list(state, tenant.tenant(), &dataset.name, &dataset.version)
                .await?;
        evaluations.retain(|e| e.status == "done");
        evaluations.sort_by_key(|b| std::cmp::Reverse(b.started_at));
        evaluations.truncate(REPETITIONS);
        for case in tagged {
            let family = family_of(&case.case.tags).expect("tagged");
            let mut repetitions = Vec::new();
            for evaluation in &evaluations {
                let Some(verdict) = evaluation.cases.iter().find(|c| c.case_id == case.case.id)
                else {
                    continue;
                };
                let counts = match &verdict.run_id {
                    Some(run_id) => match journal_counts.get(run_id) {
                        Some(c) => c.clone(),
                        None => {
                            let c = run_counts(state, tenant, run_id).await.unwrap_or_default();
                            journal_counts.insert(run_id.clone(), c.clone());
                            c
                        }
                    },
                    None => RunCounts::default(),
                };
                let violations = verdict
                    .assertions
                    .iter()
                    .filter(|a| {
                        !a.passed
                            && matches!(
                                a.assertion.as_str(),
                                "no_tool_call" | "max_cost" | "max_latency" | "state"
                            )
                    })
                    .count() as u64;
                repetitions.push(Repetition {
                    evaluation_id: evaluation.evaluation_id.clone(),
                    started_at: evaluation.started_at,
                    assistant_id: evaluation.assistant_id.clone(),
                    run_id: verdict.run_id.clone(),
                    verified: verdict.passed,
                    judge: verdict.judge.as_ref().map(|j| {
                        if j.passed {
                            "passed".to_owned()
                        } else {
                            "failed".to_owned()
                        }
                    }),
                    violations,
                    latency_ms: verdict.latency_ms,
                    tokens: verdict.total_tokens,
                    cost_usd: verdict.cost_usd,
                    skills: evaluation
                        .dependencies
                        .as_ref()
                        .and_then(|d| d.get("skills"))
                        .and_then(Value::as_object)
                        .map(|m| {
                            m.iter()
                                .filter_map(|(k, v)| v.as_u64().map(|r| (k.clone(), r)))
                                .collect()
                        })
                        .unwrap_or_default(),
                    delivery: None,
                    counts,
                });
            }
            let entry = families
                .iter_mut()
                .find(|f| f.id == family)
                .expect("a known family");
            entry.variants.push(Variant {
                dataset: dataset.name.clone(),
                version: dataset.version.clone(),
                case_id: case.case.id.clone(),
                tags: case.case.tags.clone(),
                role: role_of(&case.case.tags),
                repetitions,
            });
        }
    }
    // An assignment a person tagged with a family is a variant too: the
    // outcome delegated is the case, its rounds' runs are the evidence,
    // and the whole assignment is one repetition — verified when it ended
    // done with a complete record, its steers counted as interventions.
    let mut assignments = state
        .server_store
        .list_assignments(Some(tenant.tenant()))
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    assignments.sort_by_key(|a| a.created_at);
    for a in &assignments {
        let Some(family) = family_of(&a.tags) else {
            continue;
        };
        let mut counts = RunCounts::default();
        let (mut latency_ms, mut tokens, mut cost_usd) = (0u64, 0u64, 0f64);
        let mut run_ids: Vec<&str> = Vec::new();
        for round in &a.rounds {
            for id in [
                Some(&round.run_id),
                round.resumed_run_id.as_ref(),
                round.closing_run_id.as_ref(),
            ]
            .into_iter()
            .flatten()
            {
                if !run_ids.contains(&id.as_str()) {
                    run_ids.push(id);
                }
            }
            if let (start, Some(end)) = (round.started_at, round.ended_at) {
                latency_ms += (end - start).num_milliseconds().max(0) as u64;
            }
            if let Some(spend) = &round.spend {
                tokens += spend
                    .get("tokens")
                    .and_then(|t| {
                        t.as_u64()
                            .or_else(|| t.get("total").and_then(Value::as_u64))
                    })
                    .unwrap_or(0);
                cost_usd += spend.get("cost_usd").and_then(Value::as_f64).unwrap_or(0.0);
            }
        }
        for run_id in run_ids {
            let c = match journal_counts.get(run_id) {
                Some(c) => c.clone(),
                None => {
                    let c = run_counts(state, tenant, run_id).await.unwrap_or_default();
                    journal_counts.insert(run_id.to_owned(), c.clone());
                    c
                }
            };
            counts.unauthorized_attempts += c.unauthorized_attempts;
            counts.unauthorized_refused += c.unauthorized_refused;
            counts.duplicate_effects += c.duplicate_effects;
            counts.interventions += c.interventions;
            counts.tool_calls += c.tool_calls;
        }
        counts.interventions += a.steers.len() as u64;
        let repetition = Repetition {
            evaluation_id: a.assignment_id.clone(),
            started_at: a.created_at,
            assistant_id: a.assistant_id.clone(),
            run_id: a.rounds.last().map(|r| r.run_id.clone()),
            verified: a.state == "done" && a.progress.complete,
            judge: None,
            // An assignment carries no assertions; an attempt on a tool it
            // was told not to call is counted where every refusal is, under
            // unauthorized.
            violations: 0,
            latency_ms,
            tokens,
            cost_usd,
            skills: BTreeMap::new(),
            delivery: Some(crate::notices::told(
                &crate::notices::for_assignment(state, tenant.tenant(), &a.assignment_id).await,
            )),
            counts,
        };
        let entry = families
            .iter_mut()
            .find(|f| f.id == family)
            .expect("a known family");
        entry.variants.push(Variant {
            dataset: "assignment".to_owned(),
            version: a.assignment_id.chars().take(8).collect(),
            case_id: a.request.chars().take(60).collect(),
            tags: a.tags.clone(),
            role: role_of(&a.tags),
            repetitions: vec![repetition],
        });
    }
    for family in &mut families {
        family.learning = learning(&family.variants);
        let t = &mut family.totals;
        t.variants = family.variants.len() as u64;
        for variant in &family.variants {
            for r in &variant.repetitions {
                t.repetitions += 1;
                t.verified += u64::from(r.verified);
                t.violations += r.violations;
                t.unauthorized_attempts += r.counts.unauthorized_attempts;
                t.unauthorized_refused += r.counts.unauthorized_refused;
                t.duplicate_effects += r.counts.duplicate_effects;
                t.interventions += r.counts.interventions;
                t.latency_ms += r.latency_ms;
                t.tokens += r.tokens;
                t.cost_usd += r.cost_usd;
            }
        }
    }
    Ok(families)
}

/// The frozen conditions a campaign run is recorded under.
pub(crate) async fn conditions(state: &Arc<AppState>) -> Value {
    json!({
        "server_version": env!("CARGO_PKG_VERSION"),
        "llm": crate::llm_providers::info(state).await,
        "recorded_at": chrono::Utc::now(),
    })
}

/// The Markdown the campaign note records per run: the conditions, then
/// one table per family with the raw counts per variant and repetition,
/// then what could not run and why.
pub fn markdown(conditions: &Value, families: &[FamilyReport]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# Campaign run — {} — server {}\n\n",
        conditions["recorded_at"]
            .as_str()
            .unwrap_or("?")
            .get(..10)
            .unwrap_or("?"),
        conditions["server_version"].as_str().unwrap_or("?")
    ));
    out.push_str("## Conditions\n\n");
    out.push_str(&format!("- Server {}\n- Model route: primary {} · fallback {}\n- Rusty unchanged; grader: the verifier and each case's assertions; budgets as configured per agent.\n\n",
        conditions["server_version"].as_str().unwrap_or("?"),
        conditions["llm"]["primary"]["model"].as_str().or_else(|| conditions["llm"]["active"].as_str()).unwrap_or("—"),
        conditions["llm"]["fallback"]["model"].as_str().unwrap_or("—")));
    for family in families {
        out.push_str(&format!("## {} — {}\n\n", family.id, family.name));
        if family.variants.is_empty() {
            out.push_str(&format!(
                "Could not run: no variant recorded yet. Needs: {}.\n\n",
                family.needs
            ));
            continue;
        }
        out.push_str("| Variant | Repetition | Verified | Violations | Unauthorized (refused) | Duplicate effects | Interventions | Time (s) | Tokens | Cost (USD) |\n|---|---|---|---|---|---|---|---|---|---|\n");
        for v in &family.variants {
            if v.repetitions.is_empty() {
                out.push_str(&format!(
                    "| {}@{} `{}` | — | not run | | | | | | | |\n",
                    v.dataset, v.version, v.case_id
                ));
            }
            for (n, r) in v.repetitions.iter().enumerate() {
                let role = v.role.map(|r| format!(" ({r})")).unwrap_or_default();
                let pinned = r
                    .skills
                    .iter()
                    .map(|(s, n)| format!(" @{s}:{n}"))
                    .collect::<String>()
                    + &r.delivery
                        .map(|d| format!(" · told: {d}"))
                        .unwrap_or_default();
                out.push_str(&format!("| {}@{} `{}`{role} | {} ({}){pinned} | {} | {} | {} ({}) | {} | {} | {:.1} | {} | {:.4} |\n",
                    v.dataset, v.version, v.case_id, n + 1, r.started_at.format("%m-%d %H:%M"),
                    if r.verified { "yes" } else { "no" }, r.violations, r.counts.unauthorized_attempts, r.counts.unauthorized_refused,
                    r.counts.duplicate_effects, r.counts.interventions, r.latency_ms as f64 / 1000.0, r.tokens, r.cost_usd));
            }
        }
        let t = &family.totals;
        out.push_str(&format!(
            "| **Total** | {} | {} of {} | {} | {} ({}) | {} | {} | {:.1} | {} | {:.4} |\n\n",
            t.repetitions,
            t.verified,
            t.repetitions,
            t.violations,
            t.unauthorized_attempts,
            t.unauthorized_refused,
            t.duplicate_effects,
            t.interventions,
            t.latency_ms as f64 / 1000.0,
            t.tokens,
            t.cost_usd
        ));
        if let Some(learning) = &family.learning {
            out.push_str(&format!("Learning: {}\n\n", learning.said));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repetition(verified: bool, skill: &str, revision: u64) -> Repetition {
        Repetition {
            evaluation_id: "e".into(),
            started_at: chrono::Utc::now(),
            assistant_id: "desk".into(),
            run_id: None,
            verified,
            judge: None,
            violations: 0,
            latency_ms: 0,
            tokens: 0,
            cost_usd: 0.0,
            skills: [(skill.to_owned(), revision)].into_iter().collect(),
            delivery: None,
            counts: RunCounts::default(),
        }
    }

    fn variant(role: Option<&'static str>, repetitions: Vec<Repetition>) -> Variant {
        Variant {
            dataset: "f6".into(),
            version: "1".into(),
            case_id: "c".into(),
            tags: Vec::new(),
            role,
            repetitions,
        }
    }

    #[test]
    fn roles_are_read_from_tags_and_a_family_without_them_learns_nothing() {
        let tags = |t: &[&str]| t.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            role_of(&tags(&["campaign:F6", "transfer"])),
            Some("transfer")
        );
        assert_eq!(role_of(&tags(&["role:Control"])), Some("control"));
        assert_eq!(role_of(&tags(&["campaign:F6"])), None);
        assert!(learning(&[variant(None, vec![repetition(true, "s", 1)])]).is_none());
    }

    #[test]
    fn a_learning_is_compared_across_revisions_and_said_in_words() {
        // One revision only: counted, not compared.
        let one = learning(&[
            variant(
                Some("transfer"),
                vec![repetition(true, "never-file-twice", 8)],
            ),
            variant(
                Some("control"),
                vec![repetition(true, "never-file-twice", 8)],
            ),
        ])
        .unwrap();
        assert_eq!(one.revisions.len(), 1);
        assert!(
            one.said.contains("every repetition ran at revision 8"),
            "{}",
            one.said
        );

        // Two revisions: the related task went from failing to passing and
        // the controls held.
        let two = learning(&[
            variant(
                Some("transfer"),
                vec![
                    repetition(false, "never-file-twice", 7),
                    repetition(true, "never-file-twice", 8),
                ],
            ),
            variant(
                Some("control"),
                vec![
                    repetition(true, "never-file-twice", 7),
                    repetition(true, "never-file-twice", 8),
                ],
            ),
            variant(
                Some("control"),
                vec![
                    repetition(true, "never-file-twice", 7),
                    repetition(true, "never-file-twice", 8),
                ],
            ),
        ])
        .unwrap();
        assert_eq!(two.skill, "never-file-twice");
        assert_eq!(
            two.revisions,
            vec![
                RevisionRow {
                    revision: 7,
                    transfer_verified: 0,
                    transfer_repetitions: 1,
                    control_verified: 2,
                    control_repetitions: 2
                },
                RevisionRow {
                    revision: 8,
                    transfer_verified: 1,
                    transfer_repetitions: 1,
                    control_verified: 2,
                    control_repetitions: 2
                },
            ]
        );
        assert_eq!(
            two.said,
            "never-file-twice revision 7 → 8: the related task 0 of 1 → 1 of 1 (transferred); controls 2 of 2 → 2 of 2 (held)"
        );

        // Controls that slipped are said so, whatever the related task did.
        let slipped = learning(&[
            variant(
                Some("transfer"),
                vec![repetition(true, "s", 1), repetition(true, "s", 2)],
            ),
            variant(
                Some("control"),
                vec![repetition(true, "s", 1), repetition(false, "s", 2)],
            ),
        ])
        .unwrap();
        assert!(slipped.said.ends_with("(slipped)"), "{}", slipped.said);
        assert!(slipped.said.contains("(unchanged)"), "{}", slipped.said);
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct CampaignQuery {
    #[serde(default)]
    format: Option<String>,
}

/// `GET /campaign` — the eight families with their variants, repetitions
/// and totals; `?format=markdown` renders the record.
pub(crate) async fn get_campaign(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Query(query): Query<CampaignQuery>,
) -> Result<Response, ApiError> {
    let families = report(&state, &tenant).await?;
    let conditions = conditions(&state).await;
    if query.format.as_deref() == Some("markdown") {
        let text = markdown(&conditions, &families);
        return Ok((
            [(
                axum::http::header::CONTENT_TYPE,
                "text/markdown; charset=utf-8",
            )],
            text,
        )
            .into_response());
    }
    Ok(Json(json!({"conditions": conditions, "families": families})).into_response())
}
