//! The promotion gate (Astra R07, the vision's "learning and release"):
//! a version of an agent activates only with current evidence — every
//! suite whose cases were recorded from this agent, at its newest version,
//! evaluated against *this* version, every case passed. Without it the
//! activation is refused, naming what is missing or failing; an admin may
//! override with a reason, and the override is kept with the evidence it
//! overrode. A gate that is advisory is not a gate; this one is the
//! backend's, not a button's.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::auth::TenantContext;
use crate::error::ApiError;
use crate::routes::AppState;

/// One suite's verdict on a version.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SuiteEvidence {
    pub name: String,
    pub version: String,
    pub cases: usize,
    /// passed | failed | missing | running | stale | over budget
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluation_id: Option<String>,
    #[serde(default)]
    pub passed: usize,
    #[serde(default)]
    pub total: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluated_at: Option<DateTime<Utc>>,
    /// The active version's latest result on the same suite, for the
    /// comparison a person reads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<Value>,
    /// Why a passing evaluation no longer counts: what changed since.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stale_because: Vec<String>,
    /// Why it failed, per case: the judge's words or the assertion that
    /// did not hold — the gate says why, not only how many.
    #[serde(default)]
    pub failures: Vec<Value>,
    /// Why a finished evaluation did not clear the bar: under the agent's
    /// pass rate, or fewer passed than the version that runs now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub below: Option<String>,
}

/// The bar a version clears at the gate, set per agent in the
/// configuration that runs — so a proposal cannot lower its own bar.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct GateBar {
    /// Share of each suite's cases to pass, 0.5–1.0 (default 1.0).
    pub pass_rate: f64,
    /// The most judging one suite may spend, in USD.
    pub budget_usd: f64,
}

/// Why `passed` of `total` does not clear the bar, or `None` when it does:
/// at least `pass_rate` of the cases, and no fewer than the version that
/// runs now passed on the same suite.
pub(crate) fn short_of_bar(passed: usize, total: usize, pass_rate: f64, baseline_passed: Option<usize>) -> Option<String> {
    if total == 0 {
        return Some("no case ran".to_owned());
    }
    if (passed as f64) + 1e-9 < pass_rate * total as f64 {
        return Some(format!("{passed} of {total} is under the agent's bar of {:.0}%", pass_rate * 100.0));
    }
    if let Some(b) = baseline_passed.filter(|b| passed < *b) {
        return Some(format!("{passed} passed, fewer than the {b} the version running now passes"));
    }
    None
}

#[cfg(test)]
mod bar_tests {
    use super::short_of_bar;

    #[test]
    fn the_bar_is_a_share_of_the_cases_and_never_a_step_back() {
        assert_eq!(short_of_bar(3, 3, 1.0, None), None);
        assert!(short_of_bar(2, 3, 1.0, None).unwrap().contains("under the agent's bar of 100%"));
        assert_eq!(short_of_bar(2, 3, 0.6, None), None, "two of three clears 60%");
        assert_eq!(short_of_bar(2, 4, 0.5, Some(2)), None, "half, and as many as now");
        let back = short_of_bar(3, 4, 0.5, Some(4)).unwrap();
        assert!(back.contains("fewer than the 4 the version running now passes"), "{back}");
        assert_eq!(short_of_bar(0, 0, 0.5, None).as_deref(), Some("no case ran"));
    }
}

pub(crate) fn bar_of(record: &crate::assistants::AssistantRecord) -> GateBar {
    let gate = record.config.pointer("/studio_intent/gate");
    let pass_rate = gate.and_then(|g| g.get("pass_rate")).and_then(Value::as_f64).map(|p| (p / 100.0).clamp(0.5, 1.0)).unwrap_or(1.0);
    let budget_usd = gate.and_then(|g| g.get("budget_usd")).and_then(Value::as_f64).filter(|b| b.is_finite() && *b > 0.0).map(|b| b.clamp(0.05, 20.0)).unwrap_or(CANDIDATE_BUDGET_USD);
    GateBar { pass_rate, budget_usd }
}

/// What a version depends on, as it stands now: the revision of every
/// skill it follows, the connection (and manifest) behind every tool it
/// names, the model route. Kept on each evaluation; compared at the gate.
pub(crate) async fn dependency_snapshot(state: &AppState, tenant: &str, assistant: &crate::assistants::AssistantRecord) -> Value {
    dependency_snapshot_pinned(state, tenant, assistant, &[]).await
}

/// The snapshot with skill revisions pinned — what an evaluation of a
/// candidate skill revision ran under.
pub(crate) async fn dependency_snapshot_pinned(state: &AppState, tenant: &str, assistant: &crate::assistants::AssistantRecord, pins: &[(String, u64)]) -> Value {
    let mut skills = serde_json::Map::new();
    for name in assistant.config.pointer("/studio_intent/skills").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
        let revision = match pins.iter().find(|(n, _)| n == name) {
            Some((_, r)) => json!(r),
            None => state.skills.resolve(tenant, name).await.map(|v| json!(v.revision())).unwrap_or(Value::Null),
        };
        skills.insert(name.to_owned(), revision);
    }
    let mut connections = serde_json::Map::new();
    for tool in assistant.config.pointer("/studio_intent/tools").and_then(Value::as_array).into_iter().flatten().filter_map(|t| t.get("name").and_then(Value::as_str)) {
        let Some(cell) = &state.connection_tools else { break };
        if let Some(instance_id) = cell.instance_of(tool) {
            let manifest = state.connectors.get_instance(tenant, &instance_id).await.ok().flatten().map(|i| i.manifest_hash).unwrap_or_default();
            connections.insert(tool.to_owned(), json!({"instance_id": instance_id, "manifest_hash": manifest}));
        }
    }
    json!({
        "skills": skills,
        "connections": connections,
        "model": state.model_handle.as_ref().map(|h| h.label()),
        "graph": assistant.graph,
    })
}

/// What changed between the snapshot an evaluation kept and now — empty
/// when the evidence still stands.
pub fn stale_because(then: &Value, now: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let map = |v: &Value, key: &str| v.get(key).and_then(Value::as_object).cloned().unwrap_or_default();
    let (skills_then, skills_now) = (map(then, "skills"), map(now, "skills"));
    for (name, revision_now) in &skills_now {
        match skills_then.get(name) {
            Some(revision_then) if revision_then == revision_now => {}
            Some(revision_then) => out.push(format!("skill {name}: revision {revision_then} → {revision_now}")),
            None => out.push(format!("skill {name} is followed now and was not when this ran")),
        }
    }
    for name in skills_then.keys().filter(|n| !skills_now.contains_key(*n)) {
        out.push(format!("skill {name} was followed when this ran and is not now"));
    }
    let (conn_then, conn_now) = (map(then, "connections"), map(now, "connections"));
    for (tool, now_binding) in &conn_now {
        match conn_then.get(tool) {
            Some(then_binding) if then_binding == now_binding => {}
            Some(then_binding) => out.push(format!(
                "tool {tool}: connection {} → {}",
                then_binding.get("instance_id").and_then(Value::as_str).unwrap_or("?"),
                now_binding.get("instance_id").and_then(Value::as_str).unwrap_or("?")
            )),
            None => out.push(format!("tool {tool} runs through a connection now and did not when this ran")),
        }
    }
    for tool in conn_then.keys().filter(|t| !conn_now.contains_key(*t)) {
        out.push(format!("tool {tool}: its connection is gone"));
    }
    if then.get("model") != now.get("model") {
        out.push(format!(
            "model route: {} → {}",
            then.get("model").and_then(Value::as_str).unwrap_or("none"),
            now.get("model").and_then(Value::as_str).unwrap_or("none")
        ));
    }
    out
}

/// What the gate knows about a version.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Evidence {
    pub version_id: String,
    pub suites: Vec<SuiteEvidence>,
    /// Every suite passed (vacuously true with no suite).
    pub ok: bool,
    /// No suite is bound to this agent: nothing judged it.
    pub unevaluated: bool,
    /// The share of each suite's cases a version must pass — the agent's
    /// bar, from the configuration that runs (1.0 unless a person set it).
    #[serde(default = "full_bar")]
    pub pass_rate_min: f64,
}

fn full_bar() -> f64 {
    1.0
}

/// An activation that happened, with what the gate saw.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Promotion {
    pub version_id: String,
    pub by: Value,
    pub at: DateTime<Utc>,
    pub evidence: Evidence,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub override_reason: Option<String>,
}

fn namespace(tenant: &str) -> String {
    format!("promotions:{tenant}")
}

/// The suites bound to an agent: each dataset's newest version whose
/// cases were recorded from it — the sweep's rule.
async fn suites_of(state: &AppState, tenant: &TenantContext, assistant_id: &str) -> Result<Vec<(crate::evaluations::DatasetVersionRecord, usize)>, ApiError> {
    let catalog = crate::evaluations::list_datasets(&state.server_store, tenant.tenant()).await?;
    let mut newest: Vec<crate::evaluations::DatasetVersionRecord> = Vec::new();
    for dataset in catalog.datasets {
        match newest.iter().position(|n| n.name == dataset.name) {
            Some(at) if dataset.created_at > newest[at].created_at => newest[at] = dataset,
            Some(_) => {}
            None => newest.push(dataset),
        }
    }
    let mut out = Vec::new();
    for dataset in newest {
        let cases = crate::evaluations::load_dataset_cases(&state.server_store, tenant.tenant(), &dataset.name, &dataset.version).await?;
        if cases.first().is_some_and(|c| c.source.agent_id == assistant_id) {
            out.push((dataset, cases.len()));
        }
    }
    Ok(out)
}

/// The gate's view of `version_id`: every suite bound to the agent, its
/// latest evaluation of that version, and whether that evaluation still
/// stands — the dependencies it ran under compared with now.
pub(crate) async fn evidence_for(state: &Arc<AppState>, tenant: &TenantContext, record: &crate::assistants::AssistantRecord, version_id: &str, active_version_id: Option<&str>) -> Result<Evidence, ApiError> {
    let assistant_id = record.assistant_id.as_str();
    let suites = suites_of(state, tenant, assistant_id).await?;
    let pinned = record.at_version(version_id).unwrap_or_else(|| record.clone());
    let now = dependency_snapshot(state, tenant.tenant(), &pinned).await;
    // Evidence is on the content, not the id: a version with the same
    // configuration as this one (the candidate a person applied and then
    // published, the version restored from an older one) was judged when
    // that one was.
    let twins: Vec<&str> = record.versions.iter().filter(|v| v.config == pinned.config && v.graph == pinned.graph).map(|v| v.version_id.as_str()).collect();
    let bar = bar_of(record);
    let mut out = Vec::with_capacity(suites.len());
    for (dataset, cases) in suites {
        let evaluations = crate::dataset_runs::list(state, tenant.tenant(), &dataset.name, &dataset.version).await?;
        let latest = |vid: &str| {
            evaluations.iter().find(|e| e.status != "error" && (e.assistant_version_id == vid || (vid == version_id && twins.contains(&e.assistant_version_id.as_str()))))
        };
        let mine = latest(version_id);
        let base_eval = active_version_id.filter(|a| *a != version_id).and_then(latest);
        let baseline = base_eval.map(|e| json!({"version_id": e.assistant_version_id, "passed": e.passed, "total": e.total, "status": e.status}));
        // The bar: at least the agent's share of the cases, and never fewer
        // than the version that runs now passed on the same suite.
        let baseline_passed = base_eval.filter(|b| b.status == "done" && b.total > 0).map(|b| b.passed);
        let short_of = |e: &crate::dataset_runs::DatasetEvaluation| short_of_bar(e.passed, e.total, bar.pass_rate, baseline_passed);
        let mut below = None;
        let mut stale = Vec::new();
        let (state_word, evaluation_id, passed, total, at) = match mine {
            Some(e) if e.status == "running" => ("running", Some(e.evaluation_id.clone()), e.passed, e.total, Some(e.started_at)),
            Some(e) if e.status == "over_budget" => ("over budget", Some(e.evaluation_id.clone()), e.passed, e.total, e.finished_at),
            Some(e) if short_of(e).is_none() => {
                stale = match &e.dependencies {
                    Some(then) => stale_because(then, &now),
                    None => vec!["evaluated before dependencies were recorded".to_owned()],
                };
                (if stale.is_empty() { "passed" } else { "stale" }, Some(e.evaluation_id.clone()), e.passed, e.total, e.finished_at)
            }
            Some(e) => {
                below = short_of(e);
                ("failed", Some(e.evaluation_id.clone()), e.passed, e.total, e.finished_at)
            }
            None => ("missing", None, 0, cases, None),
        };
        // The cases that failed, whether or not the suite cleared the bar:
        // a person reads what a version gets wrong before they decide.
        let failures = match mine {
            Some(e) if state_word != "running" && e.passed < e.total => crate::dataset_runs::failures_of(e),
            _ => Vec::new(),
        };
        out.push(SuiteEvidence { name: dataset.name, version: dataset.version, cases, state: state_word.to_owned(), evaluation_id, passed, total, evaluated_at: at, baseline, stale_because: stale, failures, below });
    }
    let unevaluated = out.is_empty();
    let ok = out.iter().all(|s| s.state == "passed");
    Ok(Evidence { version_id: version_id.to_owned(), suites: out, ok, unevaluated, pass_rate_min: bar.pass_rate })
}

/// The most a candidate's judgment may spend on one suite, in USD, unless
/// the agent sets its own (`studio_intent.gate.budget_usd`). A proposal
/// that costs more to judge is not judged further; the gate reads it as
/// over budget.
pub const CANDIDATE_BUDGET_USD: f64 = 1.0;

/// The candidate gate: every suite bound to the agent starts against
/// `version_id`, capped at [`CANDIDATE_BUDGET_USD`] each, so the evidence
/// a person reads on the proposal is there before they decide. One entry
/// per suite, with the evaluation started or why it was not.
pub(crate) async fn gate_candidate(state: &Arc<AppState>, tenant: &TenantContext, record: &crate::assistants::AssistantRecord, version_id: &str, mut started_by: Value) -> Vec<Value> {
    let Some(pinned) = record.at_version(version_id) else {
        return vec![json!({"error": format!("assistant version `{version_id}` not found")})];
    };
    if !started_by.is_object() {
        started_by = json!({});
    }
    started_by["kind"] = json!("candidate");
    started_by["version_id"] = json!(version_id);
    started_by["budget_usd"] = json!(bar_of(record).budget_usd);
    let suites = match suites_of(state, tenant, &record.assistant_id).await {
        Ok(suites) => suites,
        Err(error) => return vec![json!({"error": format!("{error:?}")})],
    };
    let mut started = Vec::with_capacity(suites.len());
    for (dataset, cases) in suites {
        let mut entry = json!({"dataset": dataset.name, "version": dataset.version, "cases": cases});
        let loaded = match crate::evaluations::load_dataset_cases(&state.server_store, tenant.tenant(), &dataset.name, &dataset.version).await {
            Ok(cases) => cases,
            Err(error) => {
                entry["error"] = json!(format!("{error:?}"));
                started.push(entry);
                continue;
            }
        };
        match crate::dataset_runs::start(Arc::clone(state), tenant.tenant().to_owned(), dataset.name.clone(), dataset.version.clone(), pinned.clone(), loaded, Some(started_by.clone()), None).await {
            Ok(evaluation) => entry["evaluation_id"] = json!(evaluation.evaluation_id),
            Err(error) => entry["error"] = json!(format!("{error:?}")),
        }
        started.push(entry);
    }
    started
}

/// `POST /assistants/{id}/versions/{vid}/evidence` — judge a version by
/// hand: every suite bound to the agent runs against it, under the
/// candidate budget. For evidence gone stale, or a version nothing judged.
pub(crate) async fn judge_version(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::Extension(tenant): axum::Extension<TenantContext>,
    axum::extract::Path((assistant_id, version_id)): axum::extract::Path<(String, String)>,
) -> Result<(axum::http::StatusCode, axum::Json<Value>), ApiError> {
    let record = state
        .server_store
        .get_assistant(&tenant.scope(&assistant_id))
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("assistant `{assistant_id}` not found")))?;
    if !record.versions.iter().any(|v| v.version_id == version_id) {
        return Err(ApiError::not_found(format!("assistant version `{version_id}` not found for `{assistant_id}`")));
    }
    let started = gate_candidate(&state, &tenant, &record, &version_id, json!({"by": tenant.attribution()})).await;
    let evidence = evidence_for(&state, &tenant, &record, &version_id, record.active_version_id.as_deref()).await?;
    Ok((axum::http::StatusCode::ACCEPTED, axum::Json(json!({"started": started, "evidence": evidence}))))
}

/// The agent's promotion policy, from the configuration that runs:
/// `person` (default — a proposal the gate passes waits for a person) or
/// `auto` (a proposal the gate passes activates by itself, while the
/// verifier's evidence stands under it).
pub(crate) fn policy_of(record: &crate::assistants::AssistantRecord) -> &str {
    match record.config.pointer("/studio_intent/promotion").and_then(Value::as_str) {
        Some("auto") => "auto",
        _ => "person",
    }
}

/// After a candidate's evaluation lands: under the `auto` policy, a
/// candidate every suite passed activates by itself — if the verifier's
/// evidence stands under auto-promotion ([`crate::verifier_suite::floor`]).
/// Otherwise the proposal waits for a person, and the answer says why.
/// The activation is recorded like a person's, by the candidate gate.
pub(crate) async fn after_candidate_evaluation(state: &Arc<AppState>, tenant: &str, evaluation: &crate::dataset_runs::DatasetEvaluation) -> Option<String> {
    if evaluation.started_by.as_ref().and_then(|s| s.get("kind")).and_then(Value::as_str) != Some("candidate") {
        return None;
    }
    let version_id = evaluation.assistant_version_id.clone();
    if version_id.is_empty() {
        return None;
    }
    let tenant = TenantContext::new(tenant.to_owned(), Vec::new());
    let scoped = crate::auth::scope_id(tenant.tenant(), &evaluation.assistant_id);
    let record = match state.server_store.get_assistant(&scoped).await {
        Ok(Some(record)) => record,
        _ => match state.server_store.get_assistant(&evaluation.assistant_id).await {
            Ok(Some(record)) => record,
            _ => return None,
        },
    };
    if policy_of(&record) != "auto" {
        return Some("the agent's policy is that a person approves".to_owned());
    }
    let active = record.active_version_id().to_owned();
    if active == version_id || record.decline_of(&version_id).is_some() {
        return None;
    }
    let evidence = match evidence_for(state, &tenant, &record, &version_id, Some(active.as_str())).await {
        Ok(evidence) => evidence,
        Err(error) => return Some(format!("the evidence could not be read: {error:?}")),
    };
    if !evidence.ok || evidence.unevaluated {
        return Some("the gate did not pass every suite; it waits for a person".to_owned());
    }
    let floor = crate::verifier_suite::floor(state, tenant.tenant()).await;
    if !floor.stands {
        return Some(format!("held for a person: {}", floor.why));
    }
    let outcome = match state.server_store.activate_assistant_version(&record.assistant_id, &version_id, &active).await {
        Ok(outcome) => outcome,
        Err(error) => return Some(format!("activation failed: {error}")),
    };
    if !matches!(outcome, crate::assistants::ActivateVersionOutcome::Activated { .. }) {
        return Some("the version was not activated: the agent's active version changed meanwhile".to_owned());
    }
    let external = tenant.unscope(&record.assistant_id).unwrap_or(&record.assistant_id).to_owned();
    record_promotion(
        state,
        tenant.tenant(),
        &external,
        Promotion {
            version_id: version_id.clone(),
            by: json!({"kind": "service", "principal_id": "candidate-gate", "name": "the candidate gate", "floor": floor}),
            at: Utc::now(),
            evidence,
            override_reason: None,
        },
    )
    .await;
    tracing::info!(assistant = %external, version = %version_id, "candidate auto-promoted: the gate passed and the verifier's evidence stands");
    Some(format!("activated by the candidate gate: every suite passed and {}", floor.why))
}

pub(crate) async fn promotions_of(state: &AppState, tenant: &str, assistant_id: &str) -> Vec<Promotion> {
    state
        .server_store
        .kv_get(&namespace(tenant), assistant_id)
        .await
        .ok()
        .flatten()
        .and_then(|item| serde_json::from_value(item.value).ok())
        .unwrap_or_default()
}

pub(crate) async fn record_promotion(state: &AppState, tenant: &str, assistant_id: &str, promotion: Promotion) {
    let mut all = promotions_of(state, tenant, assistant_id).await;
    all.insert(0, promotion);
    all.truncate(50);
    if let Err(error) = state.server_store.kv_put(&namespace(tenant), assistant_id, serde_json::to_value(&all).unwrap_or(Value::Null)).await {
        tracing::warn!(%error, assistant = %assistant_id, "promotion not kept");
    }
}

/// `GET /assistants/{id}/versions/{vid}/evidence`.
pub(crate) async fn get_evidence(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::Extension(tenant): axum::Extension<TenantContext>,
    axum::extract::Path((assistant_id, version_id)): axum::extract::Path<(String, String)>,
) -> Result<axum::Json<Value>, ApiError> {
    let record = state
        .server_store
        .get_assistant(&tenant.scope(&assistant_id))
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("assistant `{assistant_id}` not found")))?;
    if !record.versions.iter().any(|v| v.version_id == version_id) {
        return Err(ApiError::not_found(format!("assistant version `{version_id}` not found for `{assistant_id}`")));
    }
    let evidence = evidence_for(&state, &tenant, &record, &version_id, record.active_version_id.as_deref()).await?;
    let promotions = promotions_of(&state, tenant.tenant(), &assistant_id).await;
    Ok(axum::Json(json!({"evidence": evidence, "promotions": promotions.into_iter().filter(|p| p.version_id == version_id).collect::<Vec<_>>()})))
}
