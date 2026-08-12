//! Studio evaluation workbench persistence (Phase 3).
//!
//! Provides durable, tenant-isolated datasets and experiment records. The
//! evaluation, comparison, and gate semantics themselves come from
//! `rusty-eval` and the existing candidate evaluator; this module only
//! stores the Studio-facing records and routes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use rusty_agent_runtime::record::sha256_hex;
use rusty_eval::{Dataset, EvalCase};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Digest;
use tokio::sync::Mutex;

use crate::error::ApiError;
use crate::routes::{internal_err, validate_client_id};

// ------------------------------------------------------------------
// File layout
// ------------------------------------------------------------------

fn datasets_dir(root: &Path) -> PathBuf {
    root.join("evaluations").join("datasets")
}

fn experiments_dir(root: &Path) -> PathBuf {
    root.join("evaluations").join("experiments")
}

fn gates_dir(root: &Path) -> PathBuf {
    root.join("evaluations").join("gates")
}

fn tenant_dir(parent: &Path, tenant: &str) -> PathBuf {
    parent.join(tenant)
}

fn dataset_path(root: &Path, tenant: &str, name: &str, version: &str) -> PathBuf {
    tenant_dir(&datasets_dir(root), tenant).join(format!("{name}@{version}.jsonl"))
}

fn experiment_path(root: &Path, tenant: &str, id: &str) -> PathBuf {
    tenant_dir(&experiments_dir(root), tenant).join(format!("{id}.json"))
}

fn gate_path(root: &Path, tenant: &str, name: &str) -> PathBuf {
    tenant_dir(&gates_dir(root), tenant).join(format!("{name}.json"))
}

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

async fn write_json_atomically(path: &Path, value: &impl Serialize) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_extension("tmp");
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, path).await
}

// ------------------------------------------------------------------
// Dataset records
// ------------------------------------------------------------------

/// Studio-facing dataset version metadata. The canonical cases live in
/// the companion JSONL file so `rusty-eval` can load them directly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DatasetVersionRecord {
    pub name: String,
    pub version: String,
    pub created_at: DateTime<Utc>,
    pub case_count: usize,
    pub digest: String,
}

/// In-memory index of all dataset versions per tenant.
pub(crate) struct EvaluationIndex {
    datasets: HashMap<String, Vec<DatasetVersionRecord>>,
    experiments: HashMap<String, ExperimentRecord>,
    gates: HashMap<String, GateRecord>,
}

impl EvaluationIndex {
    pub fn load(root: &Path) -> Self {
        let mut datasets: HashMap<String, Vec<DatasetVersionRecord>> = HashMap::new();
        let mut files = Vec::new();
        collect_json_files(&datasets_dir(root), &mut files);
        for path in files {
            let Some((tenant, _name, _version)) = parse_dataset_path(&datasets_dir(root), &path) else {
                tracing::warn!(path = %path.display(), "skipping malformed dataset path");
                continue;
            };
            match Dataset::load(&path) {
                Ok(dataset) => {
                    let record = DatasetVersionRecord {
                        name: dataset.name().to_owned(),
                        version: dataset.version().to_owned(),
                        created_at: Utc::now(),
                        case_count: dataset.cases().len(),
                        digest: dataset_digest(dataset.cases()),
                    };
                    datasets.entry(tenant).or_default().push(record);
                }
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "skipping unreadable dataset");
                }
            }
        }
        for tenant_records in datasets.values_mut() {
            tenant_records.sort_by(|a, b| b.version.cmp(&a.version));
        }

        let mut experiments: HashMap<String, ExperimentRecord> = HashMap::new();
        let mut exp_files = Vec::new();
        collect_json_files(&experiments_dir(root), &mut exp_files);
        for path in exp_files {
            let Some((tenant, id)) = parse_tenant_json_path(&experiments_dir(root), &path) else {
                tracing::warn!(path = %path.display(), "skipping malformed experiment path");
                continue;
            };
            let parsed = std::fs::read_to_string(&path)
                .ok()
                .and_then(|raw| serde_json::from_str::<ExperimentRecord>(&raw).ok());
            if let Some(record) = parsed {
                experiments.insert(format!("{tenant}/{id}"), record);
            } else {
                tracing::warn!(path = %path.display(), "skipping unreadable experiment file");
            }
        }

        let mut gates: HashMap<String, GateRecord> = HashMap::new();
        let mut gate_files = Vec::new();
        collect_json_files(&gates_dir(root), &mut gate_files);
        for path in gate_files {
            let Some((tenant, name)) = parse_tenant_json_path(&gates_dir(root), &path) else {
                continue;
            };
            let parsed = std::fs::read_to_string(&path)
                .ok()
                .and_then(|raw| serde_json::from_str::<GateRecord>(&raw).ok());
            if let Some(record) = parsed {
                gates.insert(format!("{tenant}/{name}"), record);
            }
        }

        Self {
            datasets,
            experiments,
            gates,
        }
    }
}

fn parse_dataset_path(
    dir: &Path,
    path: &Path,
) -> Option<(String, String, String)> {
    let relative = path.strip_prefix(dir).ok()?;
    let mut components = relative.components();
    let tenant = components.next()?.as_os_str().to_str()?.to_owned();
    let file = components.next()?.as_os_str().to_str()?;
    let name_version = file.strip_suffix(".jsonl")?;
    let (name, version) = name_version.rsplit_once('@')?;
    Some((tenant, name.to_owned(), version.to_owned()))
}

fn parse_tenant_json_path(dir: &Path, path: &Path) -> Option<(String, String)> {
    let relative = path.strip_prefix(dir).ok()?;
    let mut components = relative.components();
    let tenant = components.next()?.as_os_str().to_str()?.to_owned();
    let file = components.next()?.as_os_str().to_str()?;
    let id = file.strip_suffix(".json")?;
    Some((tenant, id.to_owned()))
}

fn dataset_digest(cases: &[EvalCase]) -> String {
    let mut hasher = <sha2::Sha256 as sha2::Digest>::new();
    for case in cases {
        let bytes = serde_json::to_vec(case).unwrap_or_default();
        hasher.update(&bytes);
    }
    sha256_hex(&hasher.finalize())
}

// ------------------------------------------------------------------
// Experiment records
// ------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum ExperimentStatus {
    Queued,
    Running { completed_cases: usize, total_cases: usize },
    Complete,
    Failed { reason: String },
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ExperimentRecord {
    pub experiment_id: String,
    pub dataset_name: String,
    pub dataset_version: String,
    pub candidate_id: String,
    pub target_metric: String,
    pub thresholds: Value,
    pub status: ExperimentStatus,
    pub created_at: DateTime<Utc>,
    pub evaluation: Option<Value>,
}

// ------------------------------------------------------------------
// Gate records
// ------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct GateRecord {
    pub name: String,
    pub blocked_target: String,
    pub metric: String,
    pub threshold: f64,
    pub min_evidence: usize,
    pub require_approval: bool,
    pub dataset_version: String,
    pub baseline_experiment_id: Option<String>,
    pub created_at: DateTime<Utc>,
}

// ------------------------------------------------------------------
// Shared state helpers
// ------------------------------------------------------------------

pub(crate) type EvaluationState = Arc<Mutex<EvaluationIndex>>;

pub(crate) fn init_evaluation_state(root: &Path) -> EvaluationState {
    Arc::new(Mutex::new(EvaluationIndex::load(root)))
}

pub(crate) fn tenant_id(tenant: &str, id: &str) -> String {
    format!("{tenant}/{id}")
}

// ------------------------------------------------------------------
// Datasets
// ------------------------------------------------------------------

/// Persist a dataset version atomically and return its metadata record.
/// The `cases` slice is already canonicalized by `rusty_eval::Dataset`.
pub(crate) async fn persist_dataset(
    state: &EvaluationState,
    root: &Path,
    tenant: &str,
    name: &str,
    version: &str,
    cases: Vec<EvalCase>,
) -> Result<DatasetVersionRecord, ApiError> {
    validate_client_id("dataset name", name)?;
    validate_client_id("dataset version", version)?;
    let dataset = Dataset::new(name, version, cases)
        .map_err(|e| ApiError::bad_request(format!("invalid dataset: {e}")))?;
    let path = dataset_path(root, tenant, dataset.name(), dataset.version());
    if path.exists() {
        return Err(ApiError::conflict(format!(
            "dataset version `{}@{}` already exists",
            dataset.name(),
            dataset.version()
        )));
    }
    let dataset_for_record = dataset.clone();
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(internal_err)?;
    }
    tokio::task::spawn_blocking({
        let path = path.clone();
        move || dataset.save(&path).map_err(std::io::Error::other)
    })
    .await
    .map_err(internal_err)?
    .map_err(internal_err)?;

    let record = DatasetVersionRecord {
        name: dataset_for_record.name().to_owned(),
        version: dataset_for_record.version().to_owned(),
        created_at: Utc::now(),
        case_count: dataset_for_record.cases().len(),
        digest: dataset_digest(dataset_for_record.cases()),
    };
    let mut guard = state.lock().await;
    guard
        .datasets
        .entry(tenant.to_owned())
        .or_default()
        .push(record.clone());
    guard.datasets.get_mut(tenant).unwrap().sort_by(|a, b| b.version.cmp(&a.version));
    Ok(record)
}

/// List dataset names for a tenant. Returns each newest version's metadata
/// as the representative.
pub(crate) async fn list_datasets(state: &EvaluationState, tenant: &str) -> Vec<DatasetVersionRecord> {
    state
        .lock()
        .await
        .datasets
        .get(tenant)
        .cloned()
        .unwrap_or_default()
}

/// List all versions of a dataset, newest first.
pub(crate) async fn get_dataset_versions(
    state: &EvaluationState,
    tenant: &str,
    name: &str,
) -> Vec<DatasetVersionRecord> {
    state
        .lock()
        .await
        .datasets
        .get(tenant)
        .map(|versions| {
            versions
                .iter()
                .filter(|r| r.name == name)
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// Load a dataset version from disk. The canonicalization rules of
/// `rusty_eval::Dataset` apply.
pub(crate) async fn load_dataset(
    root: &Path,
    tenant: &str,
    name: &str,
    version: &str,
) -> Result<Dataset, ApiError> {
    let path = dataset_path(root, tenant, name, version);
    tokio::task::spawn_blocking({
        let path = path.clone();
        move || Dataset::load(&path).map_err(std::io::Error::other)
    })
    .await
    .map_err(internal_err)?
    .map_err(|e| ApiError::not_found(format!("dataset `{name}@{version}`: {e}")))
}

// ------------------------------------------------------------------
// Experiments
// ------------------------------------------------------------------

/// Persist an experiment record.
pub(crate) async fn persist_experiment(
    state: &EvaluationState,
    root: &Path,
    tenant: &str,
    record: &ExperimentRecord,
) -> Result<(), ApiError> {
    let path = experiment_path(root, tenant, &record.experiment_id);
    write_json_atomically(&path, record).await.map_err(internal_err)?;
    let key = tenant_id(tenant, &record.experiment_id);
    state.lock().await.experiments.insert(key, record.clone());
    Ok(())
}

pub(crate) async fn list_experiments(state: &EvaluationState, tenant: &str) -> Vec<ExperimentRecord> {
    let prefix = format!("{tenant}/");
    state
        .lock()
        .await
        .experiments
        .iter()
        .filter(|(key, _)| key.starts_with(&prefix))
        .map(|(_, record)| record.clone())
        .collect()
}

pub(crate) async fn get_experiment(
    state: &EvaluationState,
    tenant: &str,
    id: &str,
) -> Option<ExperimentRecord> {
    let key = tenant_id(tenant, id);
    state.lock().await.experiments.get(&key).cloned()
}

// ------------------------------------------------------------------
// Gates
// ------------------------------------------------------------------

pub(crate) async fn persist_gate(
    state: &EvaluationState,
    root: &Path,
    tenant: &str,
    record: &GateRecord,
) -> Result<(), ApiError> {
    validate_client_id("gate name", &record.name)?;
    let path = gate_path(root, tenant, &record.name);
    write_json_atomically(&path, record).await.map_err(internal_err)?;
    let key = tenant_id(tenant, &record.name);
    state.lock().await.gates.insert(key, record.clone());
    Ok(())
}

pub(crate) async fn list_gates(state: &EvaluationState, tenant: &str) -> Vec<GateRecord> {
    let prefix = format!("{tenant}/");
    state
        .lock()
        .await
        .gates
        .iter()
        .filter(|(key, _)| key.starts_with(&prefix))
        .map(|(_, record)| record.clone())
        .collect()
}

pub(crate) async fn get_gate(state: &EvaluationState, tenant: &str, name: &str) -> Option<GateRecord> {
    let key = tenant_id(tenant, name);
    state.lock().await.gates.get(&key).cloned()
}
