//! Gap-ledger persistence (demand-side learning, wave 2): the file
//! layout behind the gap-ledger store backends.
//!
//! One snapshot per tenant under `{store_path}/gaps/` (`gaps` is a
//! reserved layout name, see [`crate::RESERVED_NAMES`]): the default
//! tenant's ledger is `gaps/ledger.json`, a named tenant's is
//! `gaps/{tenant}/ledger.json` — the memory layout's path-keyed
//! tenancy rule, adapted to one file per tenant because the ledger is
//! a single versioned snapshot (core's
//! [`GapLedger::to_snapshot`](rusty_agent_runtime::gaps::GapLedger::to_snapshot)),
//! not a record collection. Writes are atomic (temp file + rename, the
//! durability discipline every file record in the server shares); loads
//! skip unparseable files with a warning — one corrupt ledger must not
//! take the plane down at boot, and the mutation chains inside a
//! healthy snapshot are the evidence an operator restores from.
//!
//! Postgres keeps the same snapshot-per-tenant shape column-mapped
//! (`server_gap_ledgers`: tenant primary key, snapshot JSONB,
//! `updated_at`). The ledger mutates as a whole under the route's
//! per-tenant lock, so neither backend ever merges — it stores exactly
//! what the lock serialized, and a crash between mutation and persist
//! replays from the last durable snapshot, never from a torn one.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use rusty_agent_runtime::gaps::GapLedger;

/// The gap-ledger directory under the store root.
pub(crate) fn dir(root: &Path) -> PathBuf {
    root.join("gaps")
}

/// The tenant's snapshot file (`gaps/ledger.json` for the default
/// tenant, `gaps/{tenant}/ledger.json` for named tenants).
fn ledger_path(root: &Path, tenant: &str, default_tenant: &str) -> PathBuf {
    if tenant == default_tenant {
        dir(root).join("ledger.json")
    } else {
        dir(root).join(tenant).join("ledger.json")
    }
}

/// Persist one tenant's ledger snapshot atomically (temp file +
/// rename): a crash mid-write must never leave a truncated snapshot
/// behind — the last durable snapshot is always whole.
pub(crate) async fn persist(
    root: &Path,
    tenant: &str,
    default_tenant: &str,
    ledger: &GapLedger,
) -> io::Result<()> {
    let path = ledger_path(root, tenant, default_tenant);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let bytes = serde_json::to_vec_pretty(ledger)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_extension("tmp");
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, path).await
}

/// Recursively collect `ledger.json` files under `root` (tenant
/// subdirectories hold that tenant's snapshot), mirroring the memory
/// loader's walk.
fn collect_ledger_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_ledger_files(&path, out);
        } else if path.file_name().and_then(|n| n.to_str()) == Some("ledger.json") {
            out.push(path);
        }
    }
}

/// Load all tenant ledgers under `dir`, keyed by tenant (the default
/// tenant's key is `default_tenant`, derived from the path — the
/// snapshot body is tenant-neutral, the same rule the memory loader
/// applies to content addresses). Files that fail to parse are skipped
/// with a warning (the corrupt-tolerance rule every loader here
/// shares).
pub(crate) fn load_ledgers(root: &Path, default_tenant: &str) -> HashMap<String, GapLedger> {
    let dir = dir(root);
    let mut out = HashMap::new();
    let mut files = Vec::new();
    collect_ledger_files(&dir, &mut files);
    for path in files {
        let tenant = path
            .parent()
            .and_then(|parent| parent.strip_prefix(&dir).ok())
            .map(|relative| {
                relative
                    .components()
                    .map(|component| component.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/")
            })
            .filter(|relative| !relative.is_empty())
            .unwrap_or_else(|| default_tenant.to_string());
        let parsed = std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|body| serde_json::from_str::<GapLedger>(&body).map_err(|e| e.to_string()));
        match parsed {
            Ok(ledger) => {
                out.insert(tenant, ledger);
            }
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "skipping unparseable gap-ledger snapshot");
            }
        }
    }
    out
}

/// Where the claims runs made on gaps are kept until their verdicts land:
/// one entry per run, the gaps it said it answered. Keyed by run alone —
/// the verdict hook knows the run, not the tenant — with the tenant inside.
const CLAIMS_NAMESPACE: &str = "gap_claims";

/// One claim: a run said it answered a gap.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub(crate) struct GapClaim {
    pub tenant: String,
    pub gap_id: String,
    pub how: String,
    pub agent: String,
}

pub(crate) async fn claims_of(state: &crate::routes::AppState, run_id: &str) -> Vec<GapClaim> {
    state
        .server_store
        .kv_get(CLAIMS_NAMESPACE, run_id)
        .await
        .ok()
        .flatten()
        .and_then(|item| serde_json::from_value(item.value).ok())
        .unwrap_or_default()
}

pub(crate) async fn record_claim(state: &crate::routes::AppState, run_id: &str, claim: GapClaim) {
    let mut all = claims_of(state, run_id).await;
    if !all.iter().any(|c| c.gap_id == claim.gap_id) {
        all.push(claim);
    }
    if let Err(error) = state.server_store.kv_put(CLAIMS_NAMESPACE, run_id, serde_json::to_value(&all).unwrap_or(serde_json::Value::Null)).await {
        tracing::warn!(%error, run = %run_id, "gap claim not kept");
    }
}

/// The run's verdict landed: every gap it claimed closes when the verdict
/// is `verified` — the run answered what was missing and the verifier
/// confirmed the answer — and goes back to the queue on any other
/// verdict. The claims are spent either way.
pub(crate) async fn settle_claims(state: std::sync::Arc<crate::routes::AppState>, run_id: String, verdict: String) {
    let claims = claims_of(&state, &run_id).await;
    if claims.is_empty() {
        return;
    }
    let now = chrono::Utc::now();
    for claim in &claims {
        let tenant = crate::auth::TenantContext::new(claim.tenant.clone(), Vec::new());
        let verified = verdict == "verified";
        let actor = format!("verdict:{run_id}");
        let outcome = crate::routes::mutate_gap_ledger(&state, &tenant, |ledger| {
            if verified {
                ledger.evaluate_closure(&claim.gap_id, &rusty_agent_runtime::gaps::ClosureEvidence::VerifiedRun { run_id: run_id.clone() }, &actor, now)
            } else {
                ledger.release_claim(&claim.gap_id, &actor, now)
            }
        })
        .await;
        match outcome {
            Ok(()) => tracing::info!(run = %run_id, gap = %claim.gap_id, %verdict, closed = verified, "gap claim settled"),
            Err(error) => tracing::warn!(run = %run_id, gap = %claim.gap_id, ?error, "gap claim could not be settled"),
        }
    }
    let _ = state.server_store.kv_delete(CLAIMS_NAMESPACE, &run_id).await;
}

/// How long a claim waits for its run's verdict before the sweep gives
/// the gap back to the queue.
pub const STALE_CLAIM: chrono::Duration = chrono::Duration::hours(1);

/// The sweep's pass over claims: a claim whose run got a verdict meanwhile
/// settles on it; one whose run ended without a verdict, or that has
/// waited longer than [`STALE_CLAIM`], is released — the gap goes back to
/// the queue rather than sit trial-pending forever. Says what it did.
pub(crate) async fn release_stale_claims(state: &std::sync::Arc<crate::routes::AppState>, tenant: &str, now: chrono::DateTime<chrono::Utc>) -> Vec<serde_json::Value> {
    let Ok(items) = state.server_store.kv_list(CLAIMS_NAMESPACE).await else { return Vec::new() };
    let mut out = Vec::new();
    for item in items {
        let run_id = item.key.clone();
        let claims: Vec<GapClaim> = serde_json::from_value(item.value).unwrap_or_default();
        if !claims.iter().any(|c| c.tenant == tenant) {
            continue;
        }
        if let Some(verdict) = state.verifications.load(&run_id).and_then(|v| v.get("verdict").and_then(|x| x.as_str()).map(str::to_owned)) {
            settle_claims(std::sync::Arc::clone(state), run_id.clone(), verdict.clone()).await;
            for c in &claims {
                out.push(serde_json::json!({"run_id": run_id, "gap_id": c.gap_id, "settled": verdict}));
            }
            continue;
        }
        let ended = match state.run_deps.manager.info(&run_id).await {
            Some(info) => info.terminal.is_some(),
            None => state.server_store.get_journal(&run_id).await.ok().flatten().is_some(),
        };
        let waited = now.signed_duration_since(item.created_at);
        let why = if ended {
            "its run ended without a verdict"
        } else if waited > STALE_CLAIM {
            "its run gave no verdict within the hour"
        } else {
            continue;
        };
        let tenant_ctx = crate::auth::TenantContext::new(tenant.to_owned(), Vec::new());
        for c in &claims {
            let actor = format!("sweep:{run_id}");
            let released = crate::routes::mutate_gap_ledger(state, &tenant_ctx, |ledger| ledger.release_claim(&c.gap_id, &actor, now)).await;
            out.push(serde_json::json!({"run_id": run_id, "gap_id": c.gap_id, "released": why, "ok": released.is_ok()}));
        }
        let _ = state.server_store.kv_delete(CLAIMS_NAMESPACE, &run_id).await;
    }
    out
}


/// A gap that names a tool, walked for the agent that filed it: the gap
/// closes when *that agent* can call the tool — not when the platform
/// merely has it; when the platform has it and the agent does not, a
/// proposal is filed on the agent to add it (gated like any candidate, a
/// person approves) and the gap stays open until a version carrying the
/// tool is the one that runs. A gap nobody's agent filed closes when the
/// platform has the tool. Run by the sweep and after a version activates.
pub(crate) async fn capability_pass(state: &std::sync::Arc<crate::routes::AppState>, tenant: &crate::auth::TenantContext, now: chrono::DateTime<chrono::Utc>) -> serde_json::Value {
    use rusty_agent_runtime::gaps::{ClosureEvidence, GapStatus};
    use serde_json::{json, Value};
    let available = crate::routes::available_tool_names(state);
    let Ok(ledger) = crate::routes::load_gap_ledger(state, tenant.tenant()).await else { return json!({"closed": [], "proposed": []}) };
    // What each open tool-naming gap needs, and who filed it.
    let due: Vec<(String, Vec<String>, Option<String>)> = ledger
        .entries()
        .filter(|e| e.status != GapStatus::Closed)
        .map(|e| (e.gap_id.clone(), e.closes_on_tools(), ledger.filer(&e.gap_id).map(str::to_owned)))
        .filter(|(_, tools, _)| !tools.is_empty())
        .collect();
    drop(ledger);
    let mut closable: Vec<(String, Vec<String>)> = Vec::new();
    let mut proposed: Vec<Value> = Vec::new();
    for (gap_id, tools, filer) in due {
        // The filer as an agent on this tenant, when it is one.
        let agent = match filer.as_deref() {
            Some(id) => crate::platform_tools::find_assistant(state, tenant, id).await.ok().flatten(),
            None => None,
        };
        match agent {
            Some(record) => {
                let mine: Vec<String> = record.config.pointer("/studio_intent/tools").and_then(Value::as_array).map(|a| a.iter().filter_map(|t| t.get("name").and_then(Value::as_str).map(str::to_owned)).collect()).unwrap_or_default();
                if tools.iter().all(|t| rusty_agent_runtime::gaps::tool_available(t, &mine)) {
                    closable.push((gap_id, mine));
                } else if tools.iter().all(|t| rusty_agent_runtime::gaps::tool_available(t, &available)) {
                    let missing: Vec<String> = tools.iter().filter(|t| !rusty_agent_runtime::gaps::tool_available(t, &mine)).cloned().collect();
                    if let Some(v) = propose_tools_from_gap(state, tenant, &record, &gap_id, &missing).await {
                        proposed.push(v);
                    }
                }
            }
            None => {
                if tools.iter().all(|t| rusty_agent_runtime::gaps::tool_available(t, &available)) {
                    closable.push((gap_id, available.clone()));
                }
            }
        }
    }
    let closed = if closable.is_empty() {
        Vec::new()
    } else {
        crate::routes::mutate_gap_ledger(state, tenant, |ledger| {
            let mut closed = Vec::new();
            for (gap_id, tools) in &closable {
                match ledger.evaluate_closure(gap_id, &ClosureEvidence::CapabilitiesAvailable { tools: tools.clone() }, "capability-sweep", now) {
                    Ok(_) => closed.push(json!({"gap_id": gap_id, "resolution": ledger.entries().find(|e| &e.gap_id == gap_id).and_then(|e| e.resolution.clone())})),
                    Err(error) => tracing::warn!(%error, %gap_id, "capability closure failed"),
                }
            }
            Ok(closed)
        })
        .await
        .unwrap_or_default()
    };
    json!({"closed": closed, "proposed": proposed})
}

/// One proposal on the agent per gap: a version adding the tools the gap
/// names, filed by the gap with the agent's own words as the reason, and
/// judged by the candidate gate. Nothing twice: a version proposed by this
/// gap that is newer than the active one, and not declined, stands.
async fn propose_tools_from_gap(state: &std::sync::Arc<crate::routes::AppState>, tenant: &crate::auth::TenantContext, record: &crate::assistants::AssistantRecord, gap_id: &str, tools: &[String]) -> Option<serde_json::Value> {
    use serde_json::{json, Value};
    let active_id = record.active_version_id().to_owned();
    let active_at = record.version(&active_id).map(|v| v.created_at);
    let standing = record.versions.iter().any(|v| {
        v.version_id != active_id
            && v.metadata.pointer("/proposed_by/gap_id").and_then(Value::as_str) == Some(gap_id)
            && active_at.is_none_or(|at| v.created_at > at)
            && record.decline_of(&v.version_id).is_none()
    });
    if standing {
        return None;
    }
    let Ok(ledger) = crate::routes::load_gap_ledger(state, tenant.tenant()).await else { return None };
    let entry = ledger.entries().find(|e| e.gap_id == gap_id)?;
    let question = match &entry.subject { rusty_agent_runtime::gaps::GapSubject::QuestionShape { text } => text.clone(), other => format!("{other:?}") };
    let statement = entry.statement.clone();
    let askers = entry.volume;
    drop(ledger);
    let mut config = record.config.clone();
    if config.pointer("/studio_intent").is_none() {
        config["studio_intent"] = json!({});
    }
    let mut named: Vec<Value> = config["studio_intent"].get("tools").and_then(Value::as_array).cloned().unwrap_or_default();
    for t in tools {
        if !named.iter().any(|n| n.get("name").and_then(Value::as_str) == Some(t)) {
            named.push(json!({"name": t}));
        }
    }
    config["studio_intent"]["tools"] = Value::Array(named);
    let short: String = gap_id.chars().take(12).collect();
    let reason = format!("Add {} — the agent filed a gap for it: \"{}\" ({}; {} asker{}). The tool exists; nothing changes in the charter.", tools.iter().map(|t| format!("`{t}`")).collect::<Vec<_>>().join(", "), question, statement, askers, if askers == 1 { "" } else { "s" });
    let mut metadata = record.metadata.clone();
    metadata["proposed_by"] = json!({"principal_id": format!("gap:{gap_id}"), "name": format!("Gap {short}"), "kind": "gap", "gap_id": gap_id, "reason": reason});
    metadata["why"] = json!(reason);
    let version = crate::assistants::AssistantVersionRecord::new(Some(active_id.clone()), record.name.clone(), record.graph.clone(), config, metadata, chrono::Utc::now());
    let version_id = version.version_id.clone();
    if let Err(error) = state.server_store.create_assistant_version(&record.assistant_id, &active_id, &version).await {
        tracing::warn!(%error, %gap_id, agent = %record.assistant_id, "tool proposal from a gap not filed");
        return None;
    }
    if let Ok(Some(filed)) = state.server_store.get_assistant(&record.assistant_id).await {
        let _ = crate::promotion::gate_candidate(state, tenant, &filed, &version_id, json!({"proposed_by": "gap", "gap_id": gap_id})).await;
    }
    tracing::info!(%gap_id, agent = %record.assistant_id, %version_id, ?tools, "a gap proposed its tool on the agent");
    Some(json!({"gap_id": gap_id, "assistant_id": tenant.unscope(&record.assistant_id).unwrap_or(record.assistant_id.as_str()), "agent": record.name, "tools": tools, "version_id": version_id}))
}
