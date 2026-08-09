//! Learning-candidate persistence (R0.8 Rusty Learn, wave 3) and the
//! wave-4 evaluation composition: the file layout behind the candidate
//! and version-pointer store backends, plus the concrete
//! [`CandidateEvaluator`](rusty_agent_runtime::learn::CandidateEvaluator)
//! the release proof wires in — core's seam over `rusty-eval`'s public
//! API, with the live memory namespace adapted to a core
//! [`MemoryStore`](rusty_agent_runtime::memory::MemoryStore).
//!
//! Two directories under `{store_path}/learn/` (`learn` is a reserved
//! layout name, see [`crate::RESERVED_NAMES`]):
//!
//! - `candidates/` holds one JSON file per [`CandidateRecord`], named by
//!   tenant-scoped candidate id (`candidates/{tenant}/{id}.json` —
//!   exactly the memory layout's path-keyed tenancy: the record body
//!   carries the bare content address, the key comes from where the file
//!   lives). Candidates are immutable content-addressed objects; the
//!   record file is rewritten only on a lifecycle transition (the status
//!   machine in core), never edited in place.
//! - `versions/` holds one JSON file per [`VersionPointer`]. Surface
//!   keys contain `:` and `/` (`memory:agent:support-1`, tenant-prefixed
//!   surfaces), so the file is named by the surface key's SHA-256 and
//!   the file body is an envelope carrying the true key — loads read the
//!   key back out of the envelope rather than reversing the hash.
//!
//! Postgres keeps the same two entities column-mapped
//! (`server_learn_candidates` / `server_learn_versions`), with the
//! lifecycle transition — status flip plus pointer move — inside one
//! transaction, so a crash cannot leave a promoted candidate whose
//! pointer never moved (or a moved pointer over a candidate still
//! marked `evaluated`).

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use rusty_agent_runtime::learn::{CandidateRecord, VersionPointer};
use rusty_agent_runtime::record::sha256_hex;
use serde::{Deserialize, Serialize};

/// The candidate directory under the store root
/// (`{store_path}/learn/candidates`).
pub(crate) fn candidates_dir(root: &Path) -> PathBuf {
    root.join("learn").join("candidates")
}

/// The version-pointer directory under the store root
/// (`{store_path}/learn/versions`).
pub(crate) fn versions_dir(root: &Path) -> PathBuf {
    root.join("learn").join("versions")
}

/// Persist one candidate record atomically (temp file + rename) under
/// `candidates_dir`, named by `scoped_id` — the durability discipline
/// every file record in the server shares (the `agents::persist_record`
/// pattern). The id may carry a `{tenant}/` prefix, so the parent
/// directory is created, not just the flat dir.
pub(crate) async fn persist_candidate(
    root: &Path,
    scoped_id: &str,
    record: &CandidateRecord,
) -> io::Result<()> {
    let dir = candidates_dir(root);
    tokio::fs::create_dir_all(&dir).await?;
    let bytes = serde_json::to_vec_pretty(record)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let path = dir.join(format!("{scoped_id}.json"));
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let tmp = dir.join(format!("{scoped_id}.tmp"));
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, path).await
}

/// Recursively collect `*.json` files under `root` (tenant
/// subdirectories hold that tenant's records), mirroring the memory
/// loader.
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

/// The path-derived scoped id of a record file under `dir`
/// (`{tenant}/{id}` for named tenants, the bare id for the default
/// tenant) — the memory loader's key rule: the record body carries the
/// bare content address, so the key must come from where the file lives.
fn path_scoped_id(dir: &Path, path: &Path) -> Option<String> {
    path.strip_prefix(dir)
        .ok()
        .map(|relative| relative.with_extension(""))
        .map(|relative| {
            relative
                .components()
                .map(|component| component.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/")
        })
}

/// Load all candidate records under `candidates_dir`, keyed by their
/// path-derived scoped id. Files that fail to parse are skipped with a
/// warning (the corrupt-tolerance rule every loader here shares): one
/// bad record must not take the namespace down at boot.
pub(crate) fn load_candidates(root: &Path) -> HashMap<String, CandidateRecord> {
    let dir = candidates_dir(root);
    let mut out = HashMap::new();
    let mut files = Vec::new();
    collect_json_files(&dir, &mut files);
    for path in files {
        let scoped_id = path_scoped_id(&dir, &path);
        let parsed = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<CandidateRecord>(&raw).ok());
        match (scoped_id, parsed) {
            (Some(id), Some(record)) => {
                out.insert(id, record);
            }
            _ => {
                tracing::warn!(path = %path.display(), "skipping unreadable candidate file")
            }
        }
    }
    out
}

/// The version-pointer file's body: the pointer plus the scoped surface
/// key it was written under. The key travels in the body because the
/// filename is the key's hash — surface keys are not path-safe, and a
/// one-way filename needs the true key recorded somewhere.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct VersionFile {
    /// The tenant-scoped surface key (`{tenant}/memory:agent:support-1`
    /// for named tenants).
    key: String,
    /// The pointer itself.
    pointer: VersionPointer,
}

/// The file name for a scoped surface key: its SHA-256 hex. Hashing
/// (rather than escaping) keeps every surface — prompt names, tool
/// grants, tenant-prefixed memory scopes — inside one fixed-shape,
/// collision-checked namespace; a collision would name the same file,
/// and the envelope key check on load catches it.
fn version_file_name(scoped_surface: &str) -> String {
    sha256_hex(scoped_surface.as_bytes())
}

/// Persist one version pointer atomically (temp file + rename), named
/// by the scoped surface key's hash. The pointer moves on every
/// promotion and rollback, so this is the most-rewritten file in the
/// layout — the temp+rename discipline is what makes a crash mid-move
/// safe.
pub(crate) async fn persist_version(
    root: &Path,
    scoped_surface: &str,
    pointer: &VersionPointer,
) -> io::Result<()> {
    let dir = versions_dir(root);
    tokio::fs::create_dir_all(&dir).await?;
    let file = VersionFile {
        key: scoped_surface.to_string(),
        pointer: pointer.clone(),
    };
    let bytes = serde_json::to_vec_pretty(&file)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let name = version_file_name(scoped_surface);
    let tmp = dir.join(format!("{name}.tmp"));
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, dir.join(format!("{name}.json"))).await
}

/// Load all version pointers under `versions_dir`, keyed by the scoped
/// surface key carried in each file's envelope. A file whose envelope
/// key does not hash back to its filename is corrupt (or a hash
/// collision) and is skipped with a warning, same as an unparseable
/// file: the serving path must never resolve a surface to a pointer
/// written under a different key.
pub(crate) fn load_versions(root: &Path) -> HashMap<String, VersionPointer> {
    let dir = versions_dir(root);
    let mut out = HashMap::new();
    let mut files = Vec::new();
    collect_json_files(&dir, &mut files);
    for path in files {
        let parsed = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<VersionFile>(&raw).ok());
        let matches_name = parsed.as_ref().is_some_and(|file| {
            path.file_stem().and_then(|s| s.to_str()) == Some(&*version_file_name(&file.key))
        });
        match (parsed, matches_name) {
            (Some(file), true) => {
                out.insert(file.key, file.pointer);
            }
            _ => {
                tracing::warn!(path = %path.display(), "skipping unreadable version-pointer file")
            }
        }
    }
    out
}

// --------------------------------------------------------------------- //
// Wave 4: the live memory store adapter + the real evaluator
// --------------------------------------------------------------------- //

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rusty_agent_runtime::error::{Result as RuntimeResult, RustyError};
use rusty_agent_runtime::journal::{Journal, JournalSnapshot};
use rusty_agent_runtime::learn::{
    Candidate, CandidateContent, CandidateEvaluation, CandidateEvaluator, CandidateOverlay,
    EvaluationRequest, EvaluationVerdict, ReplayDivergence, ReplaySummary,
};
use rusty_agent_runtime::memory::{MemoryQuery, MemoryRecord, MemoryStore, ProvenanceAuthor};
use rusty_agent_runtime::record::PayloadRef;
use rusty_agent_runtime::replay::ExactReplay;
use rusty_eval::{
    compare, CompareThresholds, Dataset, EvalCase, ExperimentConfig, ExperimentReport,
    ExperimentRunner, PreparedRun,
};

use crate::server_store::ServerStore;

/// Adapter failures use core's `invalid` convention — the same error
/// shape the learning module's own adapters produce, so callers
/// distinguish store trouble from gate verdicts the way they already do.
fn invalid(message: impl Into<String>) -> RustyError {
    RustyError::InvalidUpdate(message.into())
}

/// The live memory namespace as a core [`MemoryStore`] — the read lens
/// the wave-4 evaluation composition (and the serving-path candidate
/// overlay in `routes.rs`) works through. Every operation is one
/// [`ServerStore`] call under this adapter's tenant, so the overlay sees
/// exactly what the tenant's own traffic sees.
pub(crate) struct ServerMemoryStore {
    store: Arc<dyn ServerStore>,
    tenant: String,
}

// Manual: `dyn ServerStore` is not `Debug`, and the store handle carries
// no debug-relevant state the tenant does not already identify.
impl std::fmt::Debug for ServerMemoryStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerMemoryStore")
            .field("tenant", &self.tenant)
            .finish_non_exhaustive()
    }
}

impl ServerMemoryStore {
    /// Adapt `store` for `tenant`'s namespace.
    pub(crate) fn new(store: Arc<dyn ServerStore>, tenant: impl Into<String>) -> Self {
        Self {
            store,
            tenant: tenant.into(),
        }
    }
}

#[async_trait]
impl MemoryStore for ServerMemoryStore {
    async fn put(&self, record: &MemoryRecord) -> RuntimeResult<bool> {
        // The content the address was minted from must travel with the
        // write (the backend spills oversize bodies itself).
        // Artifact-referenced content arrives only from the governed write
        // path, which owns the artifact store — an adapter-level caller
        // holding an artifact ref would be skipping that path, so fail
        // rather than persist a dangling reference.
        let content = match &record.content {
            PayloadRef::Inline(value) => value.clone(),
            PayloadRef::Artifact(_) => {
                return Err(invalid(
                    "artifact-referenced memory content must be written through the governed \
                     write path",
                ));
            }
        };
        self.store
            .put_memory(&self.tenant, record, &content)
            .await
            .map_err(|e| invalid(format!("memory put: {e}")))
    }

    async fn get(&self, memory_id: &str) -> RuntimeResult<Option<MemoryRecord>> {
        self.store
            .get_memory(&self.tenant, memory_id)
            .await
            .map_err(|e| invalid(format!("memory get: {e}")))
    }

    async fn all(&self) -> RuntimeResult<Vec<MemoryRecord>> {
        // The query universe: everything, superseded and expired records
        // included — `apply_query` and the overlay own the filtering, the
        // scan must not silently pre-filter the evidence away.
        let query = MemoryQuery {
            include_superseded: true,
            include_expired: true,
            ..MemoryQuery::default()
        };
        self.store
            .query_memory(&self.tenant, &query, Utc::now())
            .await
            .map_err(|e| invalid(format!("memory scan: {e}")))
    }

    async fn remove(&self, memory_id: &str) -> RuntimeResult<bool> {
        self.store
            .delete_memory(&self.tenant, memory_id)
            .await
            .map_err(|e| invalid(format!("memory delete: {e}")))
    }

    async fn query(
        &self,
        query: &MemoryQuery,
        now: DateTime<Utc>,
    ) -> RuntimeResult<Vec<MemoryRecord>> {
        self.store
            .query_memory(&self.tenant, query, now)
            .await
            .map_err(|e| invalid(format!("memory query: {e}")))
    }
}

/// Where versioned evaluation datasets come from. A request names a
/// version; the source resolves it; both experiment reports name that
/// version back. Datasets are immutable per version — re-running an
/// evaluation must mean re-reading the same evidence.
pub trait DatasetSource: Send + Sync + std::fmt::Debug {
    /// Load dataset version `version` (`Err` when unknown).
    fn load(&self, version: &str) -> Result<Dataset, String>;
}

/// Datasets as JSONL files under one directory, named
/// `{dataset_name}@{version}.jsonl` — `rusty-eval`'s canonical
/// serialization ([`Dataset::save`]), so dataset versions diff cleanly in
/// git and the file layout is the versioning.
#[derive(Debug)]
pub struct DirectoryDatasetSource {
    root: PathBuf,
    dataset_name: String,
}

impl DirectoryDatasetSource {
    /// A source serving `{root}/{dataset_name}@{version}.jsonl`.
    pub fn new(root: impl Into<PathBuf>, dataset_name: impl Into<String>) -> Self {
        Self {
            root: root.into(),
            dataset_name: dataset_name.into(),
        }
    }
}

impl DatasetSource for DirectoryDatasetSource {
    fn load(&self, version: &str) -> Result<Dataset, String> {
        let path = self
            .root
            .join(format!("{}@{version}.jsonl", self.dataset_name));
        Dataset::load(&path).map_err(|e| e.to_string())
    }
}

/// The application-owned half of an evaluation: how to build the agent
/// under test. The evaluator owns the evidence discipline (the versioned
/// dataset, the run counts, the replay fixtures, the comparison
/// thresholds); the agent owns what the baseline and the candidate *are*
/// as runnable graphs — the same application/runtime split as the
/// distiller contract (design open question 2).
#[async_trait]
pub trait EvaluationAgent: Send + Sync + std::fmt::Debug {
    /// Build the prepared run for one case repetition. `journal` is the
    /// run's Flight Recorder journal — wire it into recording graphs so
    /// model and tool calls become assertion evidence. `memory` is the
    /// read lens for this side of the comparison: the live namespace for
    /// the baseline, the candidate's overlay for the candidate.
    fn prepare(
        &self,
        case: &EvalCase,
        journal: &Journal,
        memory: Arc<dyn MemoryStore>,
    ) -> RuntimeResult<PreparedRun>;

    /// Re-drive one recorded run with `candidate` applied, returning the
    /// replayed snapshot for divergence verification. `replay` is the
    /// evaluator's exact-replay session over the fixture — the agent
    /// builds its replay-wired graph from it (`fresh_journal`,
    /// [`ExactReplay::source`], [`ExactReplay::snapshot`]) and drives it
    /// with [`ExactReplay::run`], so the serving cursor the evaluator's
    /// later `verify` checks for exhaustion is the same one the run
    /// consumed. Exact replay serves journaled effects, so a correct
    /// implementation makes zero outbound calls; `memory` is the same
    /// read-lens split as [`EvaluationAgent::prepare`].
    async fn redrive(
        &self,
        replay: &ExactReplay,
        candidate: &Candidate,
        memory: Arc<dyn MemoryStore>,
    ) -> RuntimeResult<JournalSnapshot>;
}

/// Read one metric out of a report, in the vocabulary the promotion gate
/// speaks: `run_pass_rate`, `case_pass_rate`, `case:{case_id}`,
/// `assertion:{key}`. `None` when the report does not name it — an
/// improvement bar cannot clear on a metric the evidence does not name.
fn metric_value(report: &ExperimentReport, metric: &str) -> Option<f64> {
    if metric == "run_pass_rate" {
        return Some(report.summary.run_pass_rate);
    }
    if metric == "case_pass_rate" {
        return Some(report.summary.case_pass_rate);
    }
    if let Some(case_id) = metric.strip_prefix("case:") {
        return report
            .cases
            .iter()
            .find(|case| case.case_id == case_id)
            .map(|case| case.pass_rate);
    }
    if let Some(key) = metric.strip_prefix("assertion:") {
        return report
            .summary
            .assertions
            .iter()
            .find(|assertion| assertion.assertion == key)
            .map(|assertion| assertion.rate);
    }
    None
}

/// The wave-4 evaluation composition: core's [`CandidateEvaluator`] over
/// `rusty-eval`'s public API, exactly the seam the trait documents —
/// [`ExperimentRunner`] over a versioned [`Dataset`] for the report half,
/// exact replay over the request's fixtures for the divergence half,
/// [`fn@compare`] for the verdict. Nothing about scoring, aggregation, or
/// regression flagging is re-implemented here.
#[derive(Debug)]
pub struct EvalCandidateEvaluator {
    baseline_memory: Arc<dyn MemoryStore>,
    datasets: Arc<dyn DatasetSource>,
    agent: Arc<dyn EvaluationAgent>,
    runs_per_case: usize,
    evaluated_by: ProvenanceAuthor,
}

impl EvalCandidateEvaluator {
    /// An evaluator reading baseline memory through `baseline_memory`,
    /// datasets through `datasets`, building agents through `agent`,
    /// running each case `runs_per_case` times (normalized to at least
    /// one), and attributing every evaluation to `evaluated_by`.
    pub fn new(
        baseline_memory: Arc<dyn MemoryStore>,
        datasets: Arc<dyn DatasetSource>,
        agent: Arc<dyn EvaluationAgent>,
        runs_per_case: usize,
        evaluated_by: ProvenanceAuthor,
    ) -> Self {
        Self {
            baseline_memory,
            datasets,
            agent,
            runs_per_case: runs_per_case.max(1),
            evaluated_by,
        }
    }
}

#[async_trait]
impl CandidateEvaluator for EvalCandidateEvaluator {
    async fn evaluate(
        &self,
        candidate: &Candidate,
        request: &EvaluationRequest,
    ) -> RuntimeResult<CandidateEvaluation> {
        let dataset = self
            .datasets
            .load(&request.dataset_version)
            .map_err(|e| invalid(format!("dataset `{}`: {e}", request.dataset_version)))?;

        // The candidate as a read lens: v1 applies `memory_set`
        // candidates as an overlay over the live namespace. Other kinds
        // keep the baseline lens — their surfaces (prompt manifests,
        // executor policy, tool grants) are applied by their own serving
        // paths, and wiring those into evaluation is per-kind future
        // work, not a generic overlay.
        let candidate_memory: Arc<dyn MemoryStore> = match &candidate.content {
            CandidateContent::MemorySet { .. } => Arc::new(CandidateOverlay::new(
                self.baseline_memory.clone(),
                candidate,
            )?),
            _ => self.baseline_memory.clone(),
        };

        let runner =
            ExperimentRunner::new(ExperimentConfig::new().with_runs_per_case(self.runs_per_case));
        let baseline_memory = self.baseline_memory.clone();
        let baseline_agent = self.agent.clone();
        let baseline = runner
            .run(&dataset, |case, journal| {
                baseline_agent.prepare(case, journal, baseline_memory.clone())
            })
            .await
            .map_err(|e| invalid(format!("baseline experiment: {e}")))?;
        let candidate_agent = self.agent.clone();
        let overlay_memory = candidate_memory.clone();
        let candidate_report = runner
            .run(&dataset, move |case, journal| {
                candidate_agent.prepare(case, journal, overlay_memory.clone())
            })
            .await
            .map_err(|e| invalid(format!("candidate experiment: {e}")))?;

        let comparison = compare(
            &baseline,
            &candidate_report,
            &CompareThresholds {
                max_pass_rate_drop: request.thresholds.max_pass_rate_drop,
                max_latency_p95_ratio: request.thresholds.max_latency_p95_ratio,
            },
        );
        let baseline_metric = metric_value(&baseline, &request.target_metric);
        let candidate_metric = metric_value(&candidate_report, &request.target_metric);
        let verdict = EvaluationVerdict {
            regressed: comparison.regressed,
            target_metric: request.target_metric.clone(),
            baseline: baseline_metric,
            candidate: candidate_metric,
            delta: baseline_metric
                .zip(candidate_metric)
                .map(|(base, cand)| cand - base),
        };

        // The replay half: re-drive every recorded fixture with the
        // candidate applied; exact replay's own divergence contract
        // decides matched vs diverged. One session per fixture, shared
        // between the agent's run and this loop's verification — the
        // exhaustion check reads the same serving cursor the run
        // consumed. A fixture that cannot even build an exact replay
        // (e.g. a resumed-run journal) counts as a divergence — the
        // gate's clean-replay bar must fail closed on evidence it cannot
        // verify.
        let mut fixture_ids = Vec::with_capacity(request.replay_evidence.len());
        let mut matched = 0usize;
        let mut divergences = Vec::new();
        for fixture in &request.replay_evidence {
            fixture_ids.push(fixture.run_id.clone());
            let divergence = match ExactReplay::new(fixture.clone()) {
                Ok(replay) => match self
                    .agent
                    .redrive(&replay, candidate, candidate_memory.clone())
                    .await
                {
                    Ok(replayed) => match replay.verify(&replayed) {
                        Ok(()) => None,
                        Err(e) => Some(e.to_string()),
                    },
                    Err(e) => Some(e.to_string()),
                },
                Err(e) => Some(e.to_string()),
            };
            match divergence {
                None => matched += 1,
                Some(detail) => divergences.push(ReplayDivergence {
                    fixture_id: fixture.run_id.clone(),
                    detail,
                }),
            }
        }
        fixture_ids.sort();
        divergences.sort_by(|a, b| a.fixture_id.cmp(&b.fixture_id));

        Ok(CandidateEvaluation {
            candidate_id: candidate.candidate_id.clone(),
            dataset_version: dataset.version().to_owned(),
            replay: ReplaySummary {
                fixture_ids,
                matched,
                divergences,
            },
            baseline_report: serde_json::to_value(&baseline)?,
            candidate_report: serde_json::to_value(&candidate_report)?,
            verdict,
            thresholds: request.thresholds,
            evaluated_by: self.evaluated_by.clone(),
            evaluated_at: Utc::now(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use rusty_agent_runtime::learn::{
        Candidate, CandidateContent, EvidenceSpan, PromotionAuthority, PromotionDecision,
        PromotionReceipt,
    };
    use rusty_agent_runtime::memory::ProvenanceAuthor;

    fn ts(millis: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp_millis(millis).unwrap()
    }

    fn candidate() -> Candidate {
        Candidate::new(
            CandidateContent::Prompt {
                name: "system".into(),
                prompt: "Answer tersely.".into(),
            },
            ProvenanceAuthor::Distiller {
                name: "correction-loop".into(),
            },
            EvidenceSpan::default(),
            ts(1_750_000_002_000),
        )
        .unwrap()
    }

    fn pointer(candidate: &Candidate) -> VersionPointer {
        VersionPointer::new(candidate.surface()).promoted(&PromotionReceipt {
            candidate_id: candidate.candidate_id.clone(),
            surface: candidate.surface(),
            previous: None,
            decision: PromotionDecision {
                authority: PromotionAuthority::Envelope {
                    envelope_version: "r0.8-default".into(),
                },
                canary: None,
            },
            promoted_at: ts(1_750_000_004_000),
        })
    }

    #[tokio::test]
    async fn candidates_round_trip_keyed_by_path_with_corrupt_tolerance() {
        let root = std::env::temp_dir().join(format!("rusty-learn-test-{}", uuid::Uuid::new_v4()));
        let record = CandidateRecord::new(candidate());
        let scoped = record.candidate.candidate_id.to_string();
        persist_candidate(&root, &scoped, &record).await.unwrap();
        let tenant_scoped = format!("acme/{scoped}");
        persist_candidate(&root, &tenant_scoped, &record)
            .await
            .unwrap();
        std::fs::write(candidates_dir(&root).join("broken.json"), b"{nope").unwrap();

        let loaded = load_candidates(&root);
        assert_eq!(loaded.len(), 2, "corrupt files are skipped, not fatal");
        assert!(loaded.contains_key(&scoped), "default tenant: bare key");
        assert_eq!(
            loaded[&tenant_scoped].candidate.candidate_id, record.candidate.candidate_id,
            "named tenant: the key comes from the path, the record keeps the bare id"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn versions_round_trip_through_hashed_filenames() {
        let root = std::env::temp_dir().join(format!("rusty-learn-test-{}", uuid::Uuid::new_v4()));
        let candidate = candidate();
        let pointer = pointer(&candidate);
        let scoped_surface = format!("acme/{}", candidate.surface().as_str());
        persist_version(&root, &scoped_surface, &pointer)
            .await
            .unwrap();

        // The filename is the key's hash — the raw surface (with its
        // `:` and `/`) appears nowhere in the directory listing.
        let listing: Vec<String> = std::fs::read_dir(versions_dir(&root))
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            listing,
            vec![format!("{}.json", sha256_hex(scoped_surface.as_bytes()))]
        );

        let loaded = load_versions(&root);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[&scoped_surface], pointer);

        // An envelope whose key does not hash back to its filename is
        // corrupt (or a collision) and skipped, not served: a consistent
        // file loads, the same body under a forged name does not.
        let stray = VersionFile {
            key: "memory:user:someone-else".into(),
            pointer: VersionPointer::new(rusty_agent_runtime::learn::SurfaceKey::new(
                "memory:user:someone-else",
            )),
        };
        let stray_bytes = serde_json::to_vec_pretty(&stray).unwrap();
        std::fs::write(
            versions_dir(&root).join(format!("{}.json", sha256_hex(b"memory:user:someone-else"))),
            &stray_bytes,
        )
        .unwrap();
        std::fs::write(
            versions_dir(&root).join(format!("{}.json", sha256_hex(b"forged-name"))),
            &stray_bytes,
        )
        .unwrap();
        let loaded = load_versions(&root);
        assert_eq!(loaded.len(), 2, "the forged-name file is skipped");
        assert!(loaded.contains_key("memory:user:someone-else"));
        let _ = std::fs::remove_dir_all(root);
    }
}
