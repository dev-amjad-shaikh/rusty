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
