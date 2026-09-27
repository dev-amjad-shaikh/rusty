//! Durable Studio evaluation workflows.
//!
//! The workbench is deliberately a composition layer. Rusty's shared
//! server store owns durability and tenant isolation; `rusty-eval` owns
//! dataset validation, experiment reports, comparisons, and gate decisions.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use rusty_agent_runtime::learn::{Candidate, CandidateContent, CandidateOverlay};
use rusty_agent_runtime::memory::MemoryStore;
use rusty_agent_runtime::record::sha256_hex;
use rusty_eval::{
    compare, evaluate_gate, CompareThresholds, ComparisonReport, Dataset, EvalCase,
    ExperimentConfig as EvalExperimentConfig, ExperimentReport, ExperimentRunner, GatePolicy,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::error::ApiError;
use crate::server_store::ServerStore;

const DATASET_NAMESPACE: &str = "studio_eval_datasets";
const DATASET_CATALOG_NAMESPACE: &str = "studio_eval_dataset_catalog";
const EXPERIMENT_NAMESPACE: &str = "studio_eval_experiments";
const EXPERIMENT_CATALOG_NAMESPACE: &str = "studio_eval_experiment_catalog";
const EXPERIMENT_CATALOG_KEY: &str = "recent";
const GATE_NAMESPACE: &str = "studio_eval_gates";
const GATE_CATALOG_NAMESPACE: &str = "studio_eval_gate_catalog";
const CATALOG_KEY: &str = "recent";
pub const MAX_DATASET_CASES: usize = 100;
const MAX_DATASET_BYTES: usize = 512 * 1024;
const MAX_EXPERIMENT_BYTES: usize = 6 * 1024 * 1024;
pub const MAX_EXPERIMENT_SUMMARIES: usize = 200;
const MAX_DATASET_SUMMARIES: usize = 200;
const MAX_GATE_SUMMARIES: usize = 200;
const MAX_CATALOG_BYTES: usize = 512 * 1024;
const EXPERIMENT_LEASE_SECONDS: i64 = 90;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DatasetCaseSource {
    pub run_id: String,
    pub thread_id: String,
    pub agent_id: String,
    /// When the case was taken from the run; now, when the publisher did
    /// not say (a run recalled without its start has no better answer).
    #[serde(default = "Utc::now")]
    pub captured_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PublishedEvalCase {
    #[serde(flatten)]
    pub case: EvalCase,
    pub source: DatasetCaseSource,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DatasetVersionRecord {
    pub name: String,
    pub version: String,
    pub created_at: DateTime<Utc>,
    pub case_count: usize,
    pub digest: String,
    /// The agent most of the cases were taken from, so a listing can say
    /// whose dataset this is. Absent on versions published before it was kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct StoredDataset {
    metadata: DatasetVersionRecord,
    cases: Vec<PublishedEvalCase>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
struct StoredDatasetCatalog {
    records: Vec<DatasetVersionRecord>,
    truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExperimentConfig {
    pub runs_per_case: usize,
    pub max_concurrency: usize,
    pub target_metric: String,
    pub thresholds: CompareThresholds,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum ExperimentStatus {
    Queued,
    Running {
        completed_runs: usize,
        total_runs: usize,
    },
    Complete,
    Failed {
        reason: String,
    },
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExperimentRecord {
    pub experiment_id: String,
    pub dataset_name: String,
    pub dataset_version: String,
    pub candidate_id: String,
    pub config: ExperimentConfig,
    pub status: ExperimentStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_report: Option<ExperimentReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_report: Option<ExperimentReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comparison: Option<ComparisonReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<ExperimentExecutionLease>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExperimentExecutionLease {
    pub owner_id: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExperimentSummary {
    pub experiment_id: String,
    pub dataset_name: String,
    pub dataset_version: String,
    pub candidate_id: String,
    pub config: ExperimentConfig,
    pub status: ExperimentStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct ExperimentCatalogEntry {
    summary: ExperimentSummary,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    execution: Option<ExperimentExecutionLease>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct ExperimentCatalog {
    pub experiments: Vec<ExperimentSummary>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
struct StoredExperimentCatalog {
    entries: Vec<ExperimentCatalogEntry>,
    truncated: bool,
}

impl From<&ExperimentRecord> for ExperimentSummary {
    fn from(record: &ExperimentRecord) -> Self {
        Self {
            experiment_id: record.experiment_id.clone(),
            dataset_name: record.dataset_name.clone(),
            dataset_version: record.dataset_version.clone(),
            candidate_id: record.candidate_id.clone(),
            config: record.config.clone(),
            status: record.status.clone(),
            created_at: record.created_at,
            updated_at: record.updated_at,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExperimentOutcome {
    pub baseline_report: ExperimentReport,
    pub candidate_report: ExperimentReport,
}

#[async_trait]
pub trait StudioExperimentEvaluator: Send + Sync + std::fmt::Debug {
    async fn evaluate(
        &self,
        candidate: &Candidate,
        dataset: &Dataset,
        config: &ExperimentConfig,
    ) -> Result<ExperimentOutcome, String>;
}

/// A standard memory-candidate evaluator: `rusty-eval::ExperimentRunner`
/// over the application's real evaluation agent, once with serving memory
/// and once with a candidate overlay. Other candidate kinds require an
/// application evaluator that can apply that exact candidate to its graph.
/// Applications opt in through `ServerConfig::with_studio_experiment_evaluator`.
#[derive(Debug)]
pub struct EvalStudioExperimentEvaluator {
    baseline_memory: Arc<dyn MemoryStore>,
    agent: Arc<dyn crate::learn::EvaluationAgent>,
}

impl EvalStudioExperimentEvaluator {
    pub fn new(
        baseline_memory: Arc<dyn MemoryStore>,
        agent: Arc<dyn crate::learn::EvaluationAgent>,
    ) -> Self {
        Self {
            baseline_memory,
            agent,
        }
    }
}

#[async_trait]
impl StudioExperimentEvaluator for EvalStudioExperimentEvaluator {
    async fn evaluate(
        &self,
        candidate: &Candidate,
        dataset: &Dataset,
        config: &ExperimentConfig,
    ) -> Result<ExperimentOutcome, String> {
        let candidate_memory: Arc<dyn MemoryStore> = match &candidate.content {
            CandidateContent::MemorySet { .. } => Arc::new(
                CandidateOverlay::new(self.baseline_memory.clone(), candidate)
                    .map_err(|error| error.to_string())?,
            ),
            _ => {
                return Err(
                    "the standard Studio evaluator only applies memory-set candidates; configure an application evaluator for this candidate kind"
                        .to_owned(),
                )
            }
        };
        let runner = ExperimentRunner::new(
            EvalExperimentConfig::new()
                .with_runs_per_case(config.runs_per_case)
                .with_max_concurrency(config.max_concurrency),
        );
        let baseline_agent = Arc::clone(&self.agent);
        let baseline_memory = Arc::clone(&self.baseline_memory);
        let baseline_report = runner
            .run(dataset, move |case, journal| {
                baseline_agent.prepare(case, journal, baseline_memory.clone())
            })
            .await
            .map_err(|error| format!("baseline experiment: {error}"))?;
        let candidate_agent = Arc::clone(&self.agent);
        let candidate_report = runner
            .run(dataset, move |case, journal| {
                candidate_agent.prepare(case, journal, candidate_memory.clone())
            })
            .await
            .map_err(|error| format!("candidate experiment: {error}"))?;
        Ok(ExperimentOutcome {
            baseline_report,
            candidate_report,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GateRecord {
    pub name: String,
    pub blocked_target: String,
    pub experiment_id: String,
    pub dataset_name: String,
    pub dataset_version: String,
    pub policy: Value,
    pub decision: Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
struct StoredGateCatalog {
    records: Vec<GateRecord>,
    truncated: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct DatasetCatalog {
    pub datasets: Vec<DatasetVersionRecord>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct GateCatalog {
    pub gates: Vec<GateRecord>,
    pub truncated: bool,
}

pub(crate) struct EvaluationRuntime {
    cancellations: Mutex<HashMap<String, CancellationToken>>,
    owner_id: String,
}

pub(crate) type EvaluationState = Arc<EvaluationRuntime>;

pub(crate) fn init_evaluation_state() -> EvaluationState {
    Arc::new(EvaluationRuntime {
        cancellations: Mutex::new(HashMap::new()),
        owner_id: uuid::Uuid::new_v4().to_string(),
    })
}

pub(crate) fn execution_lease(state: &EvaluationState) -> ExperimentExecutionLease {
    ExperimentExecutionLease {
        owner_id: state.owner_id.clone(),
        expires_at: Utc::now() + ChronoDuration::seconds(EXPERIMENT_LEASE_SECONDS),
    }
}

pub(crate) fn renew_execution_lease(state: &EvaluationState, record: &mut ExperimentRecord) {
    record.execution = Some(execution_lease(state));
}

pub(crate) fn public_experiment(mut record: ExperimentRecord) -> ExperimentRecord {
    record.execution = None;
    record
}

fn namespace(tenant: &str, suffix: &str) -> String {
    format!("{tenant}/{suffix}")
}

fn dataset_key(name: &str, version: &str) -> String {
    sha256_hex(format!("{name}\0{version}").as_bytes())
}

fn encode<T: Serialize>(value: &T) -> Result<Value, ApiError> {
    serde_json::to_value(value)
        .map_err(|error| ApiError::internal(format!("serialize evaluation record: {error}")))
}

fn decode<T: DeserializeOwned>(value: Value, kind: &str) -> Result<T, ApiError> {
    serde_json::from_value(value)
        .map_err(|error| ApiError::internal(format!("stored {kind} is invalid: {error}")))
}

fn dataset_digest(cases: &[PublishedEvalCase]) -> Result<String, ApiError> {
    let bytes = serde_json::to_vec(cases)
        .map_err(|error| ApiError::internal(format!("serialize dataset: {error}")))?;
    Ok(sha256_hex(&bytes))
}

pub(crate) async fn persist_dataset(
    _state: &EvaluationState,
    store: &Arc<dyn ServerStore>,
    tenant: &str,
    name: &str,
    version: &str,
    cases: Vec<PublishedEvalCase>,
) -> Result<(DatasetVersionRecord, bool), ApiError> {
    crate::routes::validate_client_id("dataset name", name)?;
    crate::routes::validate_client_id("dataset version", version)?;
    if cases.is_empty() || cases.len() > MAX_DATASET_CASES {
        return Err(ApiError::bad_request(format!(
            "a dataset must contain between 1 and {MAX_DATASET_CASES} cases"
        )));
    }
    let dataset_bytes = serde_json::to_vec(&cases)
        .map_err(|error| ApiError::internal(format!("serialize dataset: {error}")))?;
    if dataset_bytes.len() > MAX_DATASET_BYTES {
        return Err(ApiError::bad_request(format!(
            "dataset evidence exceeds the {} KiB boundary",
            MAX_DATASET_BYTES / 1024
        )));
    }
    let canonical: Vec<EvalCase> = cases.iter().map(|item| item.case.clone()).collect();
    Dataset::new(name, version, canonical)
        .map_err(|error| ApiError::bad_request(format!("invalid dataset: {error}")))?;
    let now = Utc::now();
    let stored = StoredDataset {
        metadata: DatasetVersionRecord {
            name: name.to_owned(),
            version: version.to_owned(),
            created_at: now,
            case_count: cases.len(),
            digest: dataset_digest(&cases)?,
            agent_id: {
                let mut counts: std::collections::BTreeMap<&str, usize> =
                    std::collections::BTreeMap::new();
                for item in &cases {
                    *counts.entry(item.source.agent_id.as_str()).or_default() += 1;
                }
                counts
                    .into_iter()
                    .max_by_key(|(_, n)| *n)
                    .map(|(id, _)| id.to_owned())
            },
        },
        cases,
    };
    let key = dataset_key(name, version);
    let namespace = namespace(tenant, DATASET_NAMESPACE);
    let created = store
        .kv_create(&namespace, &key, encode(&stored)?)
        .await
        .map_err(crate::routes::internal_err)?
        .is_some();
    let metadata = if created {
        stored.metadata
    } else {
        let existing = store
            .kv_get(&namespace, &key)
            .await
            .map_err(crate::routes::internal_err)?
            .ok_or_else(|| {
                ApiError::conflict("dataset creation raced; retry the exact request".to_owned())
            })?;
        let existing: StoredDataset = decode(existing.value, "dataset")?;
        if existing.cases != stored.cases
            || existing.metadata.name != stored.metadata.name
            || existing.metadata.version != stored.metadata.version
        {
            return Err(ApiError::conflict(format!(
                "dataset version `{name}@{version}` already exists with different cases"
            )));
        }
        existing.metadata
    };
    update_dataset_catalog(store, tenant, &metadata).await?;
    Ok((metadata, created))
}

async fn update_dataset_catalog(
    store: &Arc<dyn ServerStore>,
    tenant: &str,
    record: &DatasetVersionRecord,
) -> Result<(), ApiError> {
    let catalog_namespace = namespace(tenant, DATASET_CATALOG_NAMESPACE);
    for _ in 0..16 {
        let current = store
            .kv_get(&catalog_namespace, CATALOG_KEY)
            .await
            .map_err(crate::routes::internal_err)?;
        let mut catalog = match current.as_ref() {
            Some(item) => decode(item.value.clone(), "dataset catalog")?,
            None => StoredDatasetCatalog::default(),
        };
        catalog
            .records
            .retain(|item| item.name != record.name || item.version != record.version);
        catalog.records.push(record.clone());
        catalog.records.sort_by(|left, right| {
            right
                .created_at
                .cmp(&left.created_at)
                .then_with(|| left.name.cmp(&right.name))
                .then_with(|| left.version.cmp(&right.version))
        });
        if catalog.records.len() > MAX_DATASET_SUMMARIES {
            catalog.truncated = true;
            catalog.records.truncate(MAX_DATASET_SUMMARIES);
        }
        let value = encode(&catalog)?;
        let written = match current {
            Some(item) => store
                .kv_compare_and_swap(&catalog_namespace, CATALOG_KEY, item.updated_at, value)
                .await
                .map_err(crate::routes::internal_err)?
                .is_some(),
            None => store
                .kv_create(&catalog_namespace, CATALOG_KEY, value)
                .await
                .map_err(crate::routes::internal_err)?
                .is_some(),
        };
        if written {
            return Ok(());
        }
    }
    Err(ApiError::conflict(
        "dataset catalog changed too quickly; retry the exact request".to_owned(),
    ))
}

pub(crate) async fn list_datasets(
    store: &Arc<dyn ServerStore>,
    tenant: &str,
) -> Result<DatasetCatalog, ApiError> {
    let catalog: StoredDatasetCatalog = match store
        .kv_get(&namespace(tenant, DATASET_CATALOG_NAMESPACE), CATALOG_KEY)
        .await
        .map_err(crate::routes::internal_err)?
    {
        Some(item) => decode(item.value, "dataset catalog")?,
        None => StoredDatasetCatalog::default(),
    };
    let mut records = catalog.records;
    records.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| right.created_at.cmp(&left.created_at))
    });
    Ok(DatasetCatalog {
        datasets: records,
        truncated: catalog.truncated,
    })
}

async fn stored_dataset(
    store: &Arc<dyn ServerStore>,
    tenant: &str,
    name: &str,
    version: &str,
) -> Result<StoredDataset, ApiError> {
    store
        .kv_get(
            &namespace(tenant, DATASET_NAMESPACE),
            &dataset_key(name, version),
        )
        .await
        .map_err(crate::routes::internal_err)?
        .ok_or_else(|| ApiError::not_found(format!("dataset `{name}@{version}` not found")))
        .and_then(|item| decode(item.value, "dataset"))
}

pub(crate) async fn get_dataset_versions(
    store: &Arc<dyn ServerStore>,
    tenant: &str,
    name: &str,
) -> Result<Vec<DatasetVersionRecord>, ApiError> {
    Ok(list_datasets(store, tenant)
        .await?
        .datasets
        .into_iter()
        .filter(|record| record.name == name)
        .collect())
}

pub(crate) async fn load_dataset(
    store: &Arc<dyn ServerStore>,
    tenant: &str,
    name: &str,
    version: &str,
) -> Result<Dataset, ApiError> {
    let stored = stored_dataset(store, tenant, name, version).await?;
    Dataset::new(
        stored.metadata.name,
        stored.metadata.version,
        stored.cases.into_iter().map(|item| item.case).collect(),
    )
    .map_err(|error| ApiError::internal(format!("stored dataset is invalid: {error}")))
}

pub(crate) async fn load_dataset_cases(
    store: &Arc<dyn ServerStore>,
    tenant: &str,
    name: &str,
    version: &str,
) -> Result<Vec<PublishedEvalCase>, ApiError> {
    Ok(stored_dataset(store, tenant, name, version).await?.cases)
}

pub(crate) async fn put_experiment(
    state: &EvaluationState,
    store: &Arc<dyn ServerStore>,
    tenant: &str,
    record: &ExperimentRecord,
    create_only: bool,
) -> Result<bool, ApiError> {
    ensure_experiment_storage_bound(record).map_err(ApiError::bad_request)?;
    let experiment_namespace = namespace(tenant, EXPERIMENT_NAMESPACE);
    let (created, catalog_record) = if create_only {
        match store
            .kv_create(
                &experiment_namespace,
                &record.experiment_id,
                encode(record)?,
            )
            .await
            .map_err(crate::routes::internal_err)?
        {
            Some(_) => (true, record.clone()),
            None => {
                let existing = store
                    .kv_get(&experiment_namespace, &record.experiment_id)
                    .await
                    .map_err(crate::routes::internal_err)?
                    .ok_or_else(|| {
                        ApiError::conflict(
                            "experiment creation raced; retry the exact request".to_owned(),
                        )
                    })?;
                let existing: ExperimentRecord = decode(existing.value, "experiment")?;
                if existing.dataset_name == record.dataset_name
                    && existing.dataset_version == record.dataset_version
                    && existing.candidate_id == record.candidate_id
                    && existing.config == record.config
                {
                    (false, existing)
                } else {
                    return Err(ApiError::conflict(format!(
                        "experiment `{}` already exists with a different plan",
                        record.experiment_id
                    )));
                }
            }
        }
    } else {
        let mut updated = false;
        for _ in 0..8 {
            let current = store
                .kv_get(&experiment_namespace, &record.experiment_id)
                .await
                .map_err(crate::routes::internal_err)?
                .ok_or_else(|| {
                    ApiError::not_found(format!(
                        "experiment `{}` disappeared",
                        record.experiment_id
                    ))
                })?;
            let current_record: ExperimentRecord = decode(current.value, "experiment")?;
            if current_record.execution.as_ref().is_none_or(|lease| {
                lease.owner_id != state.owner_id || lease.expires_at <= Utc::now()
            }) {
                return Err(ApiError::conflict(format!(
                    "experiment `{}` is owned by another server",
                    record.experiment_id
                )));
            }
            if store
                .kv_compare_and_swap(
                    &experiment_namespace,
                    &record.experiment_id,
                    current.updated_at,
                    encode(record)?,
                )
                .await
                .map_err(crate::routes::internal_err)?
                .is_some()
            {
                updated = true;
                break;
            }
        }
        if !updated {
            return Err(ApiError::conflict(format!(
                "experiment `{}` changed while it was settling",
                record.experiment_id
            )));
        }
        (false, record.clone())
    };
    if let Err(error) = update_experiment_catalog(store, tenant, &catalog_record).await {
        tracing::warn!(experiment_id = %catalog_record.experiment_id, %error, "experiment committed but its browsing index update was deferred");
    }
    Ok(created)
}

async fn update_experiment_catalog(
    store: &Arc<dyn ServerStore>,
    tenant: &str,
    record: &ExperimentRecord,
) -> Result<(), ApiError> {
    let catalog_namespace = namespace(tenant, EXPERIMENT_CATALOG_NAMESPACE);
    for _ in 0..16 {
        let current = store
            .kv_get(&catalog_namespace, EXPERIMENT_CATALOG_KEY)
            .await
            .map_err(crate::routes::internal_err)?;
        let mut catalog = match current.as_ref() {
            Some(item) => decode(item.value.clone(), "experiment catalog")?,
            None => StoredExperimentCatalog::default(),
        };
        catalog
            .entries
            .retain(|entry| entry.summary.experiment_id != record.experiment_id);
        catalog.entries.push(ExperimentCatalogEntry {
            summary: ExperimentSummary::from(record),
            execution: record.execution.clone(),
        });
        catalog
            .entries
            .sort_by_key(|entry| std::cmp::Reverse(entry.summary.created_at));
        if catalog.entries.len() > MAX_EXPERIMENT_SUMMARIES {
            catalog.truncated = true;
            catalog.entries.truncate(MAX_EXPERIMENT_SUMMARIES);
        }
        while serde_json::to_vec(&catalog)
            .map_err(|error| ApiError::internal(format!("serialize experiment catalog: {error}")))?
            .len()
            > MAX_CATALOG_BYTES
        {
            if catalog.entries.pop().is_none() {
                break;
            }
            catalog.truncated = true;
        }
        let value = encode(&catalog)?;
        let written = match current {
            Some(item) => store
                .kv_compare_and_swap(
                    &catalog_namespace,
                    EXPERIMENT_CATALOG_KEY,
                    item.updated_at,
                    value,
                )
                .await
                .map_err(crate::routes::internal_err)?
                .is_some(),
            None => store
                .kv_create(&catalog_namespace, EXPERIMENT_CATALOG_KEY, value)
                .await
                .map_err(crate::routes::internal_err)?
                .is_some(),
        };
        if written {
            return Ok(());
        }
    }
    Err(ApiError::conflict(
        "experiment catalog changed too quickly; retry the exact request".to_owned(),
    ))
}

pub(crate) fn ensure_experiment_storage_bound(record: &ExperimentRecord) -> Result<(), String> {
    let bytes =
        serde_json::to_vec(record).map_err(|error| format!("serialize experiment: {error}"))?;
    if bytes.len() > MAX_EXPERIMENT_BYTES {
        return Err(format!(
            "experiment evidence exceeds the {} MiB storage boundary",
            MAX_EXPERIMENT_BYTES / (1024 * 1024)
        ));
    }
    Ok(())
}

pub(crate) async fn list_experiments(
    state: &EvaluationState,
    store: &Arc<dyn ServerStore>,
    tenant: &str,
) -> Result<ExperimentCatalog, ApiError> {
    let stored: StoredExperimentCatalog = match store
        .kv_get(
            &namespace(tenant, EXPERIMENT_CATALOG_NAMESPACE),
            EXPERIMENT_CATALOG_KEY,
        )
        .await
        .map_err(crate::routes::internal_err)?
    {
        Some(item) => decode(item.value, "experiment catalog")?,
        None => StoredExperimentCatalog::default(),
    };
    let now = Utc::now();
    let mut experiments = Vec::with_capacity(stored.entries.len());
    for entry in stored.entries {
        if matches!(
            entry.summary.status,
            ExperimentStatus::Queued | ExperimentStatus::Running { .. }
        ) && entry
            .execution
            .as_ref()
            .is_none_or(|lease| lease.expires_at <= now)
        {
            if let Some(record) =
                get_experiment(state, store, tenant, &entry.summary.experiment_id).await?
            {
                experiments.push(ExperimentSummary::from(&record));
                continue;
            }
        }
        experiments.push(entry.summary);
    }
    Ok(ExperimentCatalog {
        experiments,
        truncated: stored.truncated,
    })
}

pub(crate) async fn get_experiment(
    state: &EvaluationState,
    store: &Arc<dyn ServerStore>,
    tenant: &str,
    id: &str,
) -> Result<Option<ExperimentRecord>, ApiError> {
    let item = store
        .kv_get(&namespace(tenant, EXPERIMENT_NAMESPACE), id)
        .await
        .map_err(crate::routes::internal_err)?;
    match item {
        Some(item) => {
            let record = decode(item.value, "experiment")?;
            Ok(Some(
                reconcile_orphan(state, store, tenant, record, item.updated_at).await?,
            ))
        }
        None => Ok(None),
    }
}

async fn reconcile_orphan(
    _state: &EvaluationState,
    store: &Arc<dyn ServerStore>,
    tenant: &str,
    mut record: ExperimentRecord,
    expected_updated_at: DateTime<Utc>,
) -> Result<ExperimentRecord, ApiError> {
    if !matches!(
        record.status,
        ExperimentStatus::Queued | ExperimentStatus::Running { .. }
    ) {
        return Ok(record);
    }
    if record
        .execution
        .as_ref()
        .is_some_and(|lease| lease.expires_at > Utc::now())
    {
        return Ok(record);
    }
    record.status = ExperimentStatus::Failed {
        reason: "Rusty restarted before this experiment settled. Start a new experiment with a new identity.".to_owned(),
    };
    record.updated_at = Utc::now();
    record.execution = None;
    let experiment_namespace = namespace(tenant, EXPERIMENT_NAMESPACE);
    if store
        .kv_compare_and_swap(
            &experiment_namespace,
            &record.experiment_id,
            expected_updated_at,
            encode(&record)?,
        )
        .await
        .map_err(crate::routes::internal_err)?
        .is_some()
    {
        update_experiment_catalog(store, tenant, &record).await?;
        return Ok(record);
    }
    let latest = store
        .kv_get(&experiment_namespace, &record.experiment_id)
        .await
        .map_err(crate::routes::internal_err)?
        .ok_or_else(|| {
            ApiError::not_found(format!("experiment `{}` disappeared", record.experiment_id))
        })?;
    decode(latest.value, "experiment")
}

pub(crate) async fn register_cancellation(
    state: &EvaluationState,
    tenant: &str,
    id: &str,
) -> CancellationToken {
    let token = CancellationToken::new();
    state
        .cancellations
        .lock()
        .await
        .insert(format!("{tenant}/{id}"), token.clone());
    token
}

pub(crate) async fn clear_cancellation(state: &EvaluationState, tenant: &str, id: &str) {
    state
        .cancellations
        .lock()
        .await
        .remove(&format!("{tenant}/{id}"));
}

pub(crate) async fn cancel(state: &EvaluationState, tenant: &str, id: &str) -> bool {
    if let Some(token) = state
        .cancellations
        .lock()
        .await
        .get(&format!("{tenant}/{id}"))
    {
        token.cancel();
        true
    } else {
        false
    }
}

pub(crate) async fn compare_records(
    state: &EvaluationState,
    store: &Arc<dyn ServerStore>,
    tenant: &str,
    baseline_id: &str,
    candidate_id: &str,
    thresholds: CompareThresholds,
) -> Result<ComparisonReport, ApiError> {
    let baseline = get_experiment(state, store, tenant, baseline_id)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("experiment `{baseline_id}` not found")))?;
    let candidate = get_experiment(state, store, tenant, candidate_id)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("experiment `{candidate_id}` not found")))?;
    let baseline = baseline
        .candidate_report
        .ok_or_else(|| ApiError::conflict(format!("experiment `{baseline_id}` is not complete")))?;
    let candidate = candidate.candidate_report.ok_or_else(|| {
        ApiError::conflict(format!("experiment `{candidate_id}` is not complete"))
    })?;
    if baseline.dataset_name != candidate.dataset_name
        || baseline.dataset_version != candidate.dataset_version
    {
        return Err(ApiError::unprocessable(
            "experiments must use the same dataset version before they can be compared".to_owned(),
        ));
    }
    Ok(compare(&baseline, &candidate, &thresholds))
}

pub(crate) async fn persist_gate(
    _state: &EvaluationState,
    store: &Arc<dyn ServerStore>,
    tenant: &str,
    record: &GateRecord,
) -> Result<(GateRecord, bool), ApiError> {
    crate::routes::validate_client_id("gate name", &record.name)?;
    let namespace = namespace(tenant, GATE_NAMESPACE);
    let created = store
        .kv_create(&namespace, &record.name, encode(record)?)
        .await
        .map_err(crate::routes::internal_err)?
        .is_some();
    let durable = if created {
        record.clone()
    } else {
        let existing = store
            .kv_get(&namespace, &record.name)
            .await
            .map_err(crate::routes::internal_err)?
            .ok_or_else(|| {
                ApiError::conflict("gate creation raced; retry the exact request".to_owned())
            })?;
        let existing: GateRecord = decode(existing.value, "gate")?;
        if existing.name != record.name
            || existing.blocked_target != record.blocked_target
            || existing.experiment_id != record.experiment_id
            || existing.dataset_name != record.dataset_name
            || existing.dataset_version != record.dataset_version
            || existing.policy != record.policy
            || existing.decision != record.decision
        {
            return Err(ApiError::conflict(format!(
                "gate `{}` already exists; create a new named policy to change it",
                record.name
            )));
        }
        existing
    };
    update_gate_catalog(store, tenant, &durable).await?;
    Ok((durable, created))
}

async fn update_gate_catalog(
    store: &Arc<dyn ServerStore>,
    tenant: &str,
    record: &GateRecord,
) -> Result<(), ApiError> {
    let catalog_namespace = namespace(tenant, GATE_CATALOG_NAMESPACE);
    for _ in 0..16 {
        let current = store
            .kv_get(&catalog_namespace, CATALOG_KEY)
            .await
            .map_err(crate::routes::internal_err)?;
        let mut catalog = match current.as_ref() {
            Some(item) => decode(item.value.clone(), "gate catalog")?,
            None => StoredGateCatalog::default(),
        };
        catalog.records.retain(|item| item.name != record.name);
        catalog.records.push(record.clone());
        catalog
            .records
            .sort_by_key(|item| std::cmp::Reverse(item.created_at));
        let mut overflow = catalog.records.len() > MAX_GATE_SUMMARIES;
        catalog.records.truncate(MAX_GATE_SUMMARIES);
        while serde_json::to_vec(&catalog)
            .map_err(|error| ApiError::internal(format!("serialize gate catalog: {error}")))?
            .len()
            > MAX_CATALOG_BYTES
        {
            if catalog.records.pop().is_none() {
                break;
            }
            overflow = true;
        }
        catalog.truncated |= overflow;
        let value = encode(&catalog)?;
        let written = match current {
            Some(item) => store
                .kv_compare_and_swap(&catalog_namespace, CATALOG_KEY, item.updated_at, value)
                .await
                .map_err(crate::routes::internal_err)?
                .is_some(),
            None => store
                .kv_create(&catalog_namespace, CATALOG_KEY, value)
                .await
                .map_err(crate::routes::internal_err)?
                .is_some(),
        };
        if written {
            return Ok(());
        }
    }
    Err(ApiError::conflict(
        "gate catalog changed too quickly; retry the exact request".to_owned(),
    ))
}

pub(crate) async fn build_gate(
    state: &EvaluationState,
    store: &Arc<dyn ServerStore>,
    tenant: &str,
    name: String,
    blocked_target: String,
    experiment_id: String,
    policy: GatePolicy,
) -> Result<GateRecord, ApiError> {
    let experiment = get_experiment(state, store, tenant, &experiment_id)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("experiment `{experiment_id}` not found")))?;
    let candidate = experiment.candidate_report.as_ref().ok_or_else(|| {
        ApiError::conflict("only a complete experiment can back a release gate".to_owned())
    })?;
    let decision = evaluate_gate(&policy, candidate, experiment.baseline_report.as_ref()).map_err(
        |error| {
            ApiError::unprocessable(format!(
                "gate policy cannot evaluate this evidence: {error}"
            ))
        },
    )?;
    Ok(GateRecord {
        name,
        blocked_target,
        experiment_id,
        dataset_name: experiment.dataset_name,
        dataset_version: experiment.dataset_version,
        policy: encode(&policy)?,
        decision: encode(&decision)?,
        created_at: Utc::now(),
    })
}

pub(crate) async fn list_gates(
    store: &Arc<dyn ServerStore>,
    tenant: &str,
) -> Result<GateCatalog, ApiError> {
    let catalog: StoredGateCatalog = match store
        .kv_get(&namespace(tenant, GATE_CATALOG_NAMESPACE), CATALOG_KEY)
        .await
        .map_err(crate::routes::internal_err)?
    {
        Some(item) => decode(item.value, "gate catalog")?,
        None => StoredGateCatalog::default(),
    };
    let mut records = catalog.records;
    records.sort_by_key(|record| std::cmp::Reverse(record.created_at));
    Ok(GateCatalog {
        gates: records,
        truncated: catalog.truncated,
    })
}

pub(crate) async fn get_gate(
    store: &Arc<dyn ServerStore>,
    tenant: &str,
    name: &str,
) -> Result<Option<GateRecord>, ApiError> {
    store
        .kv_get(&namespace(tenant, GATE_NAMESPACE), name)
        .await
        .map_err(crate::routes::internal_err)?
        .map(|item| decode(item.value, "gate"))
        .transpose()
}

// ------------------------------------------------------------------
// Conformance suites and runs (EP-12-S09 server-side)
// ------------------------------------------------------------------

const CONFORMANCE_SUITE_NAMESPACE: &str = "studio_eval_conformance_suites";
const CONFORMANCE_SUITE_CATALOG_NAMESPACE: &str = "studio_eval_conformance_suite_catalog";
const CONFORMANCE_RUN_NAMESPACE: &str = "studio_eval_conformance_runs";
const CONFORMANCE_RUN_CATALOG_NAMESPACE: &str = "studio_eval_conformance_run_catalog";
const CONFORMANCE_SUITE_CATALOG_KEY: &str = "recent";
const CONFORMANCE_RUN_CATALOG_KEY: &str = "recent";
const MAX_CONFORMANCE_SUITE_SUMMARIES: usize = 200;
const MAX_CONFORMANCE_RUN_SUMMARIES: usize = 200;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConformanceSuiteRecord {
    pub name: String,
    pub version: String,
    pub suite_json: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum ConformanceRunStatus {
    Queued,
    Running,
    Complete,
    Failed { reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConformanceRunRecord {
    pub run_id: String,
    pub suite_name: String,
    pub suite_version: String,
    pub target: String,
    pub target_version: String,
    pub status: ConformanceRunStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<rusty_eval::ConformanceReport>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConformanceSuiteSummary {
    pub name: String,
    pub version: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConformanceRunSummary {
    pub run_id: String,
    pub suite_name: String,
    pub suite_version: String,
    pub target: String,
    pub target_version: String,
    pub status: ConformanceRunStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct ConformanceSuiteCatalog {
    pub suites: Vec<ConformanceSuiteSummary>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct ConformanceRunCatalog {
    pub runs: Vec<ConformanceRunSummary>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
struct StoredConformanceSuiteCatalog {
    records: Vec<ConformanceSuiteSummary>,
    truncated: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
struct StoredConformanceRunCatalog {
    records: Vec<ConformanceRunSummary>,
    truncated: bool,
}

impl From<&ConformanceSuiteRecord> for ConformanceSuiteSummary {
    fn from(record: &ConformanceSuiteRecord) -> Self {
        Self {
            name: record.name.clone(),
            version: record.version.clone(),
            created_at: record.created_at,
        }
    }
}

impl From<&ConformanceRunRecord> for ConformanceRunSummary {
    fn from(record: &ConformanceRunRecord) -> Self {
        Self {
            run_id: record.run_id.clone(),
            suite_name: record.suite_name.clone(),
            suite_version: record.suite_version.clone(),
            target: record.target.clone(),
            target_version: record.target_version.clone(),
            status: record.status.clone(),
            created_at: record.created_at,
            updated_at: record.updated_at,
        }
    }
}

fn conformance_suite_key(name: &str, version: &str) -> String {
    sha256_hex(format!("{name}\0{version}").as_bytes())
}

pub(crate) async fn persist_conformance_suite(
    store: &Arc<dyn ServerStore>,
    tenant: &str,
    record: &ConformanceSuiteRecord,
) -> Result<bool, ApiError> {
    crate::routes::validate_client_id("suite name", &record.name)?;
    crate::routes::validate_client_id("suite version", &record.version)?;
    let key = conformance_suite_key(&record.name, &record.version);
    let suite_namespace = namespace(tenant, CONFORMANCE_SUITE_NAMESPACE);
    let created = store
        .kv_create(&suite_namespace, &key, encode(record)?)
        .await
        .map_err(crate::routes::internal_err)?
        .is_some();
    let durable = if created {
        record.clone()
    } else {
        let existing = store
            .kv_get(&suite_namespace, &key)
            .await
            .map_err(crate::routes::internal_err)?
            .ok_or_else(|| {
                ApiError::conflict("suite creation raced; retry the exact request".to_owned())
            })?;
        let existing: ConformanceSuiteRecord = decode(existing.value, "conformance suite")?;
        if existing.name != record.name
            || existing.version != record.version
            || existing.suite_json != record.suite_json
        {
            return Err(ApiError::conflict(format!(
                "suite `{}@{}` already exists with different content",
                record.name, record.version
            )));
        }
        existing
    };
    update_conformance_suite_catalog(store, tenant, &durable).await?;
    Ok(created)
}

async fn update_conformance_suite_catalog(
    store: &Arc<dyn ServerStore>,
    tenant: &str,
    record: &ConformanceSuiteRecord,
) -> Result<(), ApiError> {
    let catalog_namespace = namespace(tenant, CONFORMANCE_SUITE_CATALOG_NAMESPACE);
    for _ in 0..16 {
        let current = store
            .kv_get(&catalog_namespace, CONFORMANCE_SUITE_CATALOG_KEY)
            .await
            .map_err(crate::routes::internal_err)?;
        let mut catalog = match current.as_ref() {
            Some(item) => decode(item.value.clone(), "conformance suite catalog")?,
            None => StoredConformanceSuiteCatalog::default(),
        };
        catalog
            .records
            .retain(|item| item.name != record.name || item.version != record.version);
        catalog.records.push(ConformanceSuiteSummary::from(record));
        catalog.records.sort_by(|left, right| {
            right
                .created_at
                .cmp(&left.created_at)
                .then_with(|| left.name.cmp(&right.name))
                .then_with(|| left.version.cmp(&right.version))
        });
        if catalog.records.len() > MAX_CONFORMANCE_SUITE_SUMMARIES {
            catalog.truncated = true;
            catalog.records.truncate(MAX_CONFORMANCE_SUITE_SUMMARIES);
        }
        let value = encode(&catalog)?;
        let written = match current {
            Some(item) => store
                .kv_compare_and_swap(
                    &catalog_namespace,
                    CONFORMANCE_SUITE_CATALOG_KEY,
                    item.updated_at,
                    value,
                )
                .await
                .map_err(crate::routes::internal_err)?
                .is_some(),
            None => store
                .kv_create(&catalog_namespace, CONFORMANCE_SUITE_CATALOG_KEY, value)
                .await
                .map_err(crate::routes::internal_err)?
                .is_some(),
        };
        if written {
            return Ok(());
        }
    }
    Err(ApiError::conflict(
        "conformance suite catalog changed too quickly; retry".to_owned(),
    ))
}

pub(crate) async fn list_conformance_suites(
    store: &Arc<dyn ServerStore>,
    tenant: &str,
) -> Result<ConformanceSuiteCatalog, ApiError> {
    let catalog: StoredConformanceSuiteCatalog = match store
        .kv_get(
            &namespace(tenant, CONFORMANCE_SUITE_CATALOG_NAMESPACE),
            CONFORMANCE_SUITE_CATALOG_KEY,
        )
        .await
        .map_err(crate::routes::internal_err)?
    {
        Some(item) => decode(item.value, "conformance suite catalog")?,
        None => StoredConformanceSuiteCatalog::default(),
    };
    let mut records = catalog.records;
    records.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| right.created_at.cmp(&left.created_at))
    });
    Ok(ConformanceSuiteCatalog {
        suites: records,
        truncated: catalog.truncated,
    })
}

pub(crate) async fn get_conformance_suite(
    store: &Arc<dyn ServerStore>,
    tenant: &str,
    name: &str,
    version: &str,
) -> Result<Option<ConformanceSuiteRecord>, ApiError> {
    store
        .kv_get(
            &namespace(tenant, CONFORMANCE_SUITE_NAMESPACE),
            &conformance_suite_key(name, version),
        )
        .await
        .map_err(crate::routes::internal_err)?
        .map(|item| decode(item.value, "conformance suite"))
        .transpose()
}

pub(crate) async fn persist_conformance_run(
    store: &Arc<dyn ServerStore>,
    tenant: &str,
    record: &ConformanceRunRecord,
) -> Result<bool, ApiError> {
    let run_namespace = namespace(tenant, CONFORMANCE_RUN_NAMESPACE);
    let created = store
        .kv_create(&run_namespace, &record.run_id, encode(record)?)
        .await
        .map_err(crate::routes::internal_err)?
        .is_some();
    if created {
        update_conformance_run_catalog(store, tenant, record).await?;
        return Ok(true);
    }
    let existing = store
        .kv_get(&run_namespace, &record.run_id)
        .await
        .map_err(crate::routes::internal_err)?
        .ok_or_else(|| ApiError::conflict("conformance run creation raced; retry".to_owned()))?;
    let existing: ConformanceRunRecord = decode(existing.value, "conformance run")?;
    if existing.suite_name != record.suite_name
        || existing.suite_version != record.suite_version
        || existing.target != record.target
        || existing.target_version != record.target_version
    {
        return Err(ApiError::conflict(format!(
            "run `{}` already exists with a different plan",
            record.run_id
        )));
    }
    store
        .kv_put(&run_namespace, &record.run_id, encode(record)?)
        .await
        .map_err(crate::routes::internal_err)?;
    update_conformance_run_catalog(store, tenant, record).await?;
    Ok(false)
}

async fn update_conformance_run_catalog(
    store: &Arc<dyn ServerStore>,
    tenant: &str,
    record: &ConformanceRunRecord,
) -> Result<(), ApiError> {
    let catalog_namespace = namespace(tenant, CONFORMANCE_RUN_CATALOG_NAMESPACE);
    for _ in 0..16 {
        let current = store
            .kv_get(&catalog_namespace, CONFORMANCE_RUN_CATALOG_KEY)
            .await
            .map_err(crate::routes::internal_err)?;
        let mut catalog = match current.as_ref() {
            Some(item) => decode(item.value.clone(), "conformance run catalog")?,
            None => StoredConformanceRunCatalog::default(),
        };
        catalog.records.retain(|item| item.run_id != record.run_id);
        catalog.records.push(ConformanceRunSummary::from(record));
        catalog
            .records
            .sort_by_key(|item| std::cmp::Reverse(item.created_at));
        if catalog.records.len() > MAX_CONFORMANCE_RUN_SUMMARIES {
            catalog.truncated = true;
            catalog.records.truncate(MAX_CONFORMANCE_RUN_SUMMARIES);
        }
        while serde_json::to_vec(&catalog)
            .map_err(|error| {
                ApiError::internal(format!("serialize conformance run catalog: {error}"))
            })?
            .len()
            > MAX_CATALOG_BYTES
        {
            if catalog.records.pop().is_none() {
                break;
            }
            catalog.truncated = true;
        }
        let value = encode(&catalog)?;
        let written = match current {
            Some(item) => store
                .kv_compare_and_swap(
                    &catalog_namespace,
                    CONFORMANCE_RUN_CATALOG_KEY,
                    item.updated_at,
                    value,
                )
                .await
                .map_err(crate::routes::internal_err)?
                .is_some(),
            None => store
                .kv_create(&catalog_namespace, CONFORMANCE_RUN_CATALOG_KEY, value)
                .await
                .map_err(crate::routes::internal_err)?
                .is_some(),
        };
        if written {
            return Ok(());
        }
    }
    Err(ApiError::conflict(
        "conformance run catalog changed too quickly; retry".to_owned(),
    ))
}

pub(crate) async fn get_conformance_run(
    store: &Arc<dyn ServerStore>,
    tenant: &str,
    run_id: &str,
) -> Result<Option<ConformanceRunRecord>, ApiError> {
    store
        .kv_get(&namespace(tenant, CONFORMANCE_RUN_NAMESPACE), run_id)
        .await
        .map_err(crate::routes::internal_err)?
        .map(|item| decode(item.value, "conformance run"))
        .transpose()
}

pub(crate) async fn list_conformance_runs(
    store: &Arc<dyn ServerStore>,
    tenant: &str,
) -> Result<ConformanceRunCatalog, ApiError> {
    let catalog: StoredConformanceRunCatalog = match store
        .kv_get(
            &namespace(tenant, CONFORMANCE_RUN_CATALOG_NAMESPACE),
            CONFORMANCE_RUN_CATALOG_KEY,
        )
        .await
        .map_err(crate::routes::internal_err)?
    {
        Some(item) => decode(item.value, "conformance run catalog")?,
        None => StoredConformanceRunCatalog::default(),
    };
    Ok(ConformanceRunCatalog {
        runs: catalog.records,
        truncated: catalog.truncated,
    })
}

/// Check whether `target`@`target_version` has a passing conformance run
/// for `suite_name`@`suite_version`.  Used by registration endpoints
/// (AC 2).
pub(crate) async fn target_has_passing_conformance_run(
    store: &Arc<dyn ServerStore>,
    tenant: &str,
    suite_name: &str,
    suite_version: &str,
    target: &str,
    target_version: &str,
) -> Result<Option<String>, ApiError> {
    let catalog = list_conformance_runs(store, tenant).await?;
    for run in catalog.runs {
        if run.suite_name == suite_name
            && run.suite_version == suite_version
            && run.target == target
            && run.target_version == target_version
        {
            if let ConformanceRunStatus::Complete = run.status {
                // The report itself carries `passed`; we need the full record.
                if let Some(record) = get_conformance_run(store, tenant, &run.run_id).await? {
                    if let Some(ref report) = record.report {
                        if report.passed {
                            return Ok(Some(run.run_id));
                        }
                    }
                }
            }
        }
    }
    Ok(None)
}

/// Conformance registry: holds check implementations and can run suites.
pub struct ConformanceRegistry {
    runner: rusty_eval::ConformanceRunner,
}

impl std::fmt::Debug for ConformanceRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConformanceRegistry")
            .field("runner", &"<ConformanceRunner>")
            .finish()
    }
}

impl Default for ConformanceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ConformanceRegistry {
    pub fn new() -> Self {
        Self {
            runner: rusty_eval::ConformanceRunner::new(),
        }
    }

    #[allow(dead_code)]
    pub fn register(&mut self, check: Box<dyn rusty_eval::ConformanceCheck>) {
        self.runner.register(check);
    }

    pub async fn run(
        &self,
        suite: &rusty_eval::ConformanceSuite,
        target: &str,
    ) -> rusty_eval::ConformanceReport {
        self.runner.run(suite, target).await
    }
}

/// Every suite at once: for each dataset, its newest version, against the
/// agent its cases were recorded from. What started and what was skipped
/// (and why) comes back; the verdicts arrive on each dataset's
/// evaluations as the cases finish. The button on Evals and the nightly
/// sweep are this one function.
pub(crate) async fn sweep_all(
    state: &Arc<crate::routes::AppState>,
    tenant: &crate::auth::TenantContext,
    started_by: Value,
) -> Result<Vec<Value>, ApiError> {
    let catalog = list_datasets(&state.server_store, tenant.tenant()).await?;
    let mut newest: Vec<&DatasetVersionRecord> = Vec::new();
    for dataset in &catalog.datasets {
        match newest.iter().position(|n| n.name == dataset.name) {
            Some(at) if dataset.created_at > newest[at].created_at => newest[at] = dataset,
            Some(_) => {}
            None => newest.push(dataset),
        }
    }
    let mut started = Vec::new();
    for dataset in newest {
        let cases = load_dataset_cases(
            &state.server_store,
            tenant.tenant(),
            &dataset.name,
            &dataset.version,
        )
        .await?;
        let mut entry = serde_json::json!({"name": dataset.name, "version": dataset.version, "cases": cases.len()});
        let Some(agent_id) = cases.first().map(|c| c.source.agent_id.clone()) else {
            entry["skipped"] = serde_json::json!("no case names the agent it was recorded from");
            started.push(entry);
            continue;
        };
        let assistant = state
            .server_store
            .get_assistant(&tenant.scope(&agent_id))
            .await
            .map_err(crate::routes::internal_err)?
            .filter(|a| a.archived_at.is_none());
        let Some(assistant) = assistant else {
            entry["skipped"] = serde_json::json!(format!("agent `{agent_id}` is gone or archived"));
            started.push(entry);
            continue;
        };
        entry["assistant_id"] = serde_json::json!(agent_id);
        entry["assistant"] = serde_json::json!(assistant.name);
        match crate::dataset_runs::start(
            Arc::clone(state),
            tenant.tenant().to_owned(),
            dataset.name.clone(),
            dataset.version.clone(),
            assistant,
            cases,
            Some(started_by.clone()),
            None,
        )
        .await
        {
            Ok(evaluation) => entry["evaluation_id"] = serde_json::json!(evaluation.evaluation_id),
            Err(error) => entry["skipped"] = serde_json::json!(format!("{error:?}")),
        }
        started.push(entry);
    }
    Ok(started)
}

/// Whom a sweep's news goes to: every active administrator; on a
/// deployment with no users (open mode), the developer it runs as.
async fn sweep_recipients(state: &crate::routes::AppState) -> Vec<Value> {
    let users = state.users.list().await;
    if users.is_empty() {
        return vec![
            serde_json::json!({"principal_id": "dev", "name": "Developer (open mode)", "kind": "user"}),
        ];
    }
    users
        .into_iter()
        .filter(|u| u.active && u.roles.contains(&crate::auth::Role::Admin))
        .map(|u| serde_json::json!({"principal_id": u.id, "name": u.name, "kind": "user"}))
        .collect()
}

/// After a sweep: wait for each suite it started to finish, and tell the
/// administrators about every suite that did not pass in full — which
/// agent, which dataset, how many of how many, the first reasons, and
/// whether it passed last time (a regression) — once per suite per
/// sweep, in the Inbox. A suite that passed says nothing: the morning
/// Inbox holds what needs a person, not a report.
pub(crate) fn spawn_sweep_report(
    state: Arc<crate::routes::AppState>,
    tenant: String,
    started: Vec<Value>,
    nightly: bool,
) {
    tokio::spawn(async move {
        let recipients = sweep_recipients(&state).await;
        if recipients.is_empty() {
            return;
        }
        for entry in started {
            let (Some(name), Some(version), Some(evaluation_id)) = (
                entry.get("name").and_then(Value::as_str).map(str::to_owned),
                entry
                    .get("version")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                entry
                    .get("evaluation_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            ) else {
                continue;
            };
            let agent = entry
                .get("assistant")
                .and_then(Value::as_str)
                .unwrap_or("an agent")
                .to_owned();
            // Wait for it — a suite is a handful of real runs — but not forever.
            let mut latest = None;
            for _ in 0..900 {
                let Ok(list) = crate::dataset_runs::list(&state, &tenant, &name, &version).await
                else {
                    break;
                };
                if let Some(e) = list.iter().find(|e| e.evaluation_id == evaluation_id) {
                    if e.status != "running" {
                        let mut sorted = list.clone();
                        sorted.sort_by_key(|a| a.started_at);
                        let at = sorted.iter().position(|x| x.evaluation_id == evaluation_id);
                        let previous = at
                            .and_then(|i| i.checked_sub(1))
                            .and_then(|i| sorted.get(i).cloned());
                        latest = Some((e.clone(), previous));
                        break;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
            let Some((evaluation, previous)) = latest else {
                continue;
            };
            if evaluation.status == "done" && evaluation.passed == evaluation.total {
                continue;
            }
            let regressed = previous
                .as_ref()
                .is_some_and(|p| p.status == "done" && p.total > 0 && p.passed == p.total);
            let reasons: Vec<String> = crate::dataset_runs::failures_of(&evaluation)
                .iter()
                .take(3)
                .filter_map(|r| {
                    r.as_str()
                        .map(str::to_owned)
                        .or_else(|| r.get("reason").and_then(Value::as_str).map(str::to_owned))
                        .or_else(|| Some(r.to_string()))
                })
                .collect();
            let sweep = if nightly { "Nightly sweep" } else { "Sweep" };
            let title = if evaluation.status != "done" {
                format!("{sweep}: {agent}'s suite {name} could not run")
            } else if regressed {
                format!(
                    "{sweep}: {agent}'s suite {name} regressed — {} of {} passed",
                    evaluation.passed, evaluation.total
                )
            } else {
                format!(
                    "{sweep}: {agent}'s suite {name} — {} of {} passed",
                    evaluation.passed, evaluation.total
                )
            };
            let text = if evaluation.status != "done" {
                evaluation
                    .error
                    .clone()
                    .unwrap_or_else(|| "the evaluation ended without a verdict".to_owned())
            } else {
                let mut t = if regressed {
                    "It passed in full last time. ".to_owned()
                } else {
                    String::new()
                };
                if !reasons.is_empty() {
                    t.push_str(&reasons.join(" · "));
                }
                t.push_str(" — open the suite under Evals to read each case.");
                t
            };
            let about = serde_json::json!({"kind": "sweep", "dataset": name, "version": version, "evaluation_id": evaluation_id, "assistant_id": evaluation.assistant_id, "passed": evaluation.passed, "total": evaluation.total, "regressed": regressed, "state": if evaluation.status != "done" { "failed" } else if regressed { "regressed" } else { "failing" }});
            for to in &recipients {
                let key = format!(
                    "sweep:{name}:{evaluation_id}:{}",
                    to.get("principal_id")
                        .and_then(Value::as_str)
                        .unwrap_or("?")
                );
                crate::notices::tell_in(
                    &state.server_store,
                    &tenant,
                    to,
                    &key,
                    about.clone(),
                    &title,
                    &text,
                )
                .await;
            }
            tracing::info!(dataset = %name, passed = evaluation.passed, total = evaluation.total, regressed, "sweep: a suite did not pass in full; the administrators are told");
        }
    });
}

/// How long until the next `hour:minute` UTC from `now` — later today, or
/// tomorrow when that moment has passed.
pub(crate) fn until_next(
    now: chrono::DateTime<chrono::Utc>,
    hour: u32,
    minute: u32,
) -> std::time::Duration {
    use chrono::{Duration as ChronoDuration, Timelike};
    let today = now
        .with_hour(hour)
        .and_then(|t| t.with_minute(minute))
        .and_then(|t| t.with_second(0))
        .and_then(|t| t.with_nanosecond(0))
        .unwrap_or(now);
    let next = if today > now {
        today
    } else {
        today + ChronoDuration::days(1)
    };
    (next - now)
        .to_std()
        .unwrap_or(std::time::Duration::from_secs(60))
}

/// The nightly sweep: when the server is configured with a time, every
/// suite runs at it, every day, attributed to the schedule. Nobody presses
/// anything; Evals shows the verdicts in the morning.
pub(crate) fn spawn_sweeper(state: Arc<crate::routes::AppState>) {
    let Some((hour, minute)) = state.config.sweep_at else {
        return;
    };
    tokio::spawn(async move {
        loop {
            let wait = until_next(chrono::Utc::now(), hour, minute);
            tokio::time::sleep(wait).await;
            let tenant =
                crate::auth::TenantContext::new(crate::auth::DEFAULT_TENANT.to_owned(), Vec::new());
            match sweep_all(
                &state,
                &tenant,
                serde_json::json!({"kind": "sweep", "by": "schedule"}),
            )
            .await
            {
                Ok(started) => {
                    tracing::info!(
                        suites = started
                            .iter()
                            .filter(|e| e.get("evaluation_id").is_some())
                            .count(),
                        "nightly sweep started"
                    );
                    spawn_sweep_report(
                        Arc::clone(&state),
                        tenant.tenant().to_owned(),
                        started,
                        true,
                    );
                }
                Err(error) => tracing::warn!(?error, "nightly sweep could not start"),
            }
            // Beside the suites: every learned reference re-read against
            // the system it came from, so drift is found by the platform
            // rather than by a desk answering from last month.
            // And the fleet agents made: a spawned agent idle for the default
            // fortnight is put away, its notes folded into its maker's.
            let idle_days = crate::spawned::retire_idle_days(&state, tenant.tenant()).await;
            if idle_days > 0 {
                match crate::spawned::retire_idle(&state, &tenant, idle_days, &serde_json::json!({"principal_id": "platform", "name": "the nightly sweep", "kind": "service"}), chrono::Utc::now()).await {
                    Ok(retired) if !retired.is_empty() => tracing::info!(retired = retired.len(), idle_days, "idle spawned agents retired"),
                    Ok(_) => {}
                    Err(error) => tracing::warn!(?error, "idle spawned agents could not be retired"),
                }
            }
            let checked = crate::freshness::check_all(&state, &tenant).await;
            let stale = checked
                .iter()
                .filter(|c| c["stale"] == serde_json::Value::Bool(true))
                .count();
            if !checked.is_empty() {
                tracing::info!(checked = checked.len(), stale, "nightly freshness check");
            }
            // Never twice in the same minute.
            tokio::time::sleep(std::time::Duration::from_secs(61)).await;
        }
    });
}

/// Remove a dataset — every version, its cases and its evaluations — and
/// take it out of the catalog. Refused while an evaluation of any version
/// is still running on this server: a suite mid-run is not deleted from
/// under itself. Answers how many versions went.
pub(crate) async fn delete_dataset(
    state: &Arc<crate::routes::AppState>,
    tenant: &str,
    name: &str,
) -> Result<usize, ApiError> {
    let catalog_namespace = namespace(tenant, DATASET_CATALOG_NAMESPACE);
    let mut catalog: StoredDatasetCatalog = match state
        .server_store
        .kv_get(&catalog_namespace, CATALOG_KEY)
        .await
        .map_err(crate::routes::internal_err)?
    {
        Some(item) => decode(item.value, "dataset catalog")?,
        None => StoredDatasetCatalog {
            records: Vec::new(),
            truncated: false,
        },
    };
    let versions: Vec<String> = catalog
        .records
        .iter()
        .filter(|r| r.name == name)
        .map(|r| r.version.clone())
        .collect();
    if versions.is_empty() {
        return Err(ApiError::not_found(format!("dataset `{name}` not found")));
    }
    for version in &versions {
        let running = crate::dataset_runs::stored(&state.server_store, tenant, name, version)
            .await?
            .iter()
            .any(|e| e.status == "running" && e.boot_id == state.boot_id);
        if running {
            return Err(ApiError::conflict(format!(
                "an evaluation of `{name}` v{version} is still running; wait for it, then remove"
            )));
        }
    }
    for version in &versions {
        state
            .server_store
            .kv_delete(
                &namespace(tenant, DATASET_NAMESPACE),
                &dataset_key(name, version),
            )
            .await
            .map_err(crate::routes::internal_err)?;
        state
            .server_store
            .kv_delete(
                &crate::dataset_runs::evaluation_namespace(tenant),
                &crate::dataset_runs::evaluation_key(name, version),
            )
            .await
            .map_err(crate::routes::internal_err)?;
    }
    catalog.records.retain(|r| r.name != name);
    state
        .server_store
        .kv_put(&catalog_namespace, CATALOG_KEY, encode(&catalog)?)
        .await
        .map_err(crate::routes::internal_err)?;
    Ok(versions.len())
}

#[cfg(test)]
mod sweep_tests {
    use super::until_next;
    use chrono::{TimeZone, Utc};

    #[test]
    fn the_next_sweep_is_later_today_or_tomorrow() {
        let now = Utc.with_ymd_and_hms(2026, 9, 7, 23, 30, 0).unwrap();
        assert_eq!(until_next(now, 2, 0).as_secs(), 2 * 3600 + 30 * 60);
        let now = Utc.with_ymd_and_hms(2026, 9, 7, 1, 0, 0).unwrap();
        assert_eq!(until_next(now, 2, 0).as_secs(), 3600);
        let now = Utc.with_ymd_and_hms(2026, 9, 7, 2, 0, 0).unwrap();
        assert_eq!(until_next(now, 2, 0).as_secs(), 24 * 3600);
    }
}
