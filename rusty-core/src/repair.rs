//! Typed repair records: the audit stream for every automated repair.
//!
//! Every automated repair — conversational retry, provider backoff, overflow
//! recovery, crash repair, attempt retry, sweep, stuck-turn intervention,
//! invalidation, breaker transition — emits exactly one typed repair record
//! per episode. The record names what was detected, what was done, and how it
//! ended, so self-healing behavior is auditable, evaluable, and improvable —
//! never folklore.
//!
//! Repair records live on a separate audit stream, not in the session log
//! (the closed `RunEventKind` enum is never widened). The session log carries
//! the repair's footprint through existing kinds — synthetic `ToolResult`s,
//! `TurnEnd { Interrupted }`, compaction brackets — and the repair record cites
//! those positions, so the timeline and the audit stream corroborate each
//! other.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Domain types
// ---------------------------------------------------------------------------

/// The component that detected the fault and performed the repair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepairComponent {
    /// The five-stage tool pipeline (validation-failure conversational repair).
    ToolPipeline,
    /// The provider request seam (retry / backoff / overflow).
    ProviderSeam,
    /// The compaction engine (overflow recovery).
    CompactionEngine,
    /// The crash-repair walker (synthetic close + frontier recomputation).
    CrashRepair,
    /// The attempt scheduler (retry chains, resume-safe vs fresh-session).
    AttemptScheduler,
    /// The orphan sweep (heartbeat-lost detection).
    OrphanSweep,
    /// The stuck-turn detector (shielded grace + hard termination).
    StuckTurnDetector,
    /// The dependency invalidator (demotion + revalidation filing).
    DependencyInvalidation,
    /// The circuit breaker (open / half-open / close transitions).
    CircuitBreaker,
}

/// What triggered the repair episode.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "trigger", rename_all = "snake_case")]
pub enum RepairTrigger {
    /// A tool call failed validation and was repaired conversationally.
    ValidationFailure {
        /// The `tool_call_id` of the failing call.
        tool_call_id: String,
    },
    /// A provider request failed with a classified error.
    ProviderError {
        /// The error class (rate-limit, transient, context-overflow, etc.).
        error_class: String,
        /// The provider call row id or event id that failed.
        provider_call_id: String,
    },
    /// The context window overflowed and compaction was attempted.
    ContextOverflow {
        /// The log position of the overflowed model call.
        model_call_position: String,
    },
    /// The process was killed or crashed mid-turn.
    Crash {
        /// The instrumented kill point (e.g. "mid_effect", "after_enqueue").
        kill_point: String,
        /// The last durable checkpoint id, if any.
        last_checkpoint_id: Option<String>,
    },
    /// An attempt's heartbeat was lost.
    HeartbeatLost {
        /// The attempt id that went dark.
        attempt_id: String,
    },
    /// A turn was classified as stuck.
    StuckTurn {
        /// The session id.
        session_id: String,
        /// The phase that was stuck (model_call, tool_execution, etc.).
        phase: String,
        /// The duration in ms before intervention.
        stuck_for_ms: u64,
    },
    /// A dependency's canonical shape changed.
    DependencyChange {
        /// The dependency id (tool name, connector id, setting path).
        dependency_id: String,
        /// The old SHA-256 fingerprint.
        old_fingerprint: String,
        /// The new SHA-256 fingerprint.
        new_fingerprint: String,
    },
    /// A tool or connector crossed the failure-rate threshold.
    FlappingTool {
        /// The tool or connector name.
        target: String,
        /// Failures in the rolling window.
        failure_count: u32,
        /// Total calls in the rolling window.
        total_calls: u32,
    },
}

/// Which rung of the repair ladder handled the episode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepairRung {
    /// Rung 1: in-turn repair (validation retry or provider retry).
    InTurn,
    /// Rung 2: attempt-level repair (retry chain, fresh session).
    Attempt,
    /// Rung 3: subsystem-level repair (compaction, checkpoint-resume).
    Subsystem,
    /// Rung 4: knowledge-level repair (gap-ledger filing).
    Knowledge,
}

/// The concrete action taken.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum RepairAction {
    /// A conversational retry prompt was issued under a tool budget.
    ConversationalRetry { rung: RepairRung },
    /// A provider request was retried with classified backoff.
    ProviderRetry { rung: RepairRung },
    /// Compaction ran to recover from context overflow.
    OverflowCompaction { rung: RepairRung },
    /// The crash-repair walk closed an orphaned turn.
    CrashRepairWalk { rung: RepairRung },
    /// A task attempt was retried (resume-safe or fresh-session).
    AttemptRetry {
        rung: RepairRung,
        /// Whether the retry used the same session (resume-safe) or started fresh.
        resume_safe: bool,
    },
    /// An orphan sweep transitioned a heartbeat-lost attempt to failed.
    OrphanSweep { rung: RepairRung },
    /// A stuck turn was terminated after grace exhaustion.
    StuckTurnIntervention { rung: RepairRung },
    /// A promoted skill was demoted and a revalidation entry filed.
    DependencyInvalidation { rung: RepairRung },
    /// A circuit breaker transitioned state.
    BreakerTransition {
        rung: RepairRung,
        /// The state the breaker moved to.
        to_state: BreakerState,
    },
}

/// Circuit breaker states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BreakerState {
    Open,
    HalfOpen,
    Closed,
}

/// How the repair episode ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepairOutcome {
    /// The repair resolved the fault.
    Repaired,
    /// The repair exhausted its budget and escalated to the next rung.
    Escalated,
    /// The repair failed and the ladder is exhausted.
    Failed,
}

// ---------------------------------------------------------------------------
// Knowledge-level repair filing (EP-10-S09)
// ---------------------------------------------------------------------------

/// The result of classifying a failure cause at the knowledge-repair rung.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KnowledgeCause {
    /// A knowledge-level cause: the agent lacks information or its
    /// instructions diverge from observed reality. Filed as a gap entry.
    Knowledge {
        /// Human-readable statement of what the gap is.
        statement: String,
        /// The subject of the gap (intent or question shape).
        subject: crate::gaps::GapSubject,
        /// Observable closure criteria.
        closure_criteria: crate::gaps::ClosureCriteria,
    },
    /// An environmental cause (provider outage, network, etc.): the ladder
    /// handled it, no gap is filed.
    Environmental,
}

/// Classifies failure signatures into knowledge-level vs environmental causes.
///
/// The classifier is heuristic: repeated patterns that indicate ignorance
/// (unknown tools, unavailable capabilities, identical failures across retries)
/// produce [`KnowledgeCause::Knowledge`]; transient infrastructure failures
/// produce [`KnowledgeCause::Environmental`].
pub struct KnowledgeClassifier;

impl KnowledgeClassifier {
    /// Classify repeated unknown-tool calls.
    ///
    /// A single unknown-tool call may be a typo; repeated calls to the same
    /// plausible-but-absent tool name indicate a knowledge gap.
    pub fn classify_unknown_tool(tool_name: &str, attempt_count: u32) -> KnowledgeCause {
        if attempt_count >= 2 {
            KnowledgeCause::Knowledge {
                statement: format!("Agent repeatedly called unknown tool '{tool_name}'"),
                subject: crate::gaps::GapSubject::question_shape(&format!("use tool {tool_name}"))
                    .unwrap_or_else(|_| crate::gaps::GapSubject::Intent {
                        intent_id: "unknown-tool".to_string(),
                    }),
                closure_criteria: crate::gaps::ClosureCriteria::ArtifactPromoted {
                    candidate_id: format!("skill:tool-knowledge:{tool_name}"),
                },
            }
        } else {
            KnowledgeCause::Environmental
        }
    }

    /// Classify a plan step that names a capability the toolset cannot fulfil.
    pub fn classify_plan_step_unavailable(step: &str) -> KnowledgeCause {
        KnowledgeCause::Knowledge {
            statement: format!("Plan step '{step}' requires a capability not in the toolset"),
            subject: crate::gaps::GapSubject::question_shape(&format!("execute plan step: {step}"))
                .unwrap_or_else(|_| crate::gaps::GapSubject::Intent {
                    intent_id: "unavailable-capability".to_string(),
                }),
            closure_criteria: crate::gaps::ClosureCriteria::ArtifactPromoted {
                candidate_id: format!("skill:plan-capability:{step}"),
            },
        }
    }

    /// Classify an instruction that failed identically across retries.
    ///
    /// A failure that repeats with the same signature after rung-1 repair
    /// suggests the instruction itself is wrong, not the execution context.
    pub fn classify_identical_failure_across_retries(
        failure_signature: &str,
        retry_count: u32,
    ) -> KnowledgeCause {
        if retry_count >= 2 {
            KnowledgeCause::Knowledge {
                statement: format!(
                    "Identical failure '{failure_signature}' persisted across {retry_count} retries"
                ),
                subject: crate::gaps::GapSubject::question_shape(&format!(
                    "resolve failure: {failure_signature}"
                ))
                .unwrap_or_else(|_| crate::gaps::GapSubject::Intent {
                    intent_id: "persistent-failure".to_string(),
                }),
                closure_criteria: crate::gaps::ClosureCriteria::FailureRateBelow {
                    threshold_millis: 100, // 10% failure-rate ceiling
                },
            }
        } else {
            KnowledgeCause::Environmental
        }
    }

    /// Classify plan-reality divergence: a skill's instructed step fails
    /// against observed behaviour past the configured repeat threshold.
    pub fn classify_plan_reality_divergence(
        skill_manifest_hash: &str,
        failing_step: &str,
        repeat_count: u32,
    ) -> KnowledgeCause {
        if repeat_count >= 2 {
            KnowledgeCause::Knowledge {
                statement: format!(
                    "Skill {skill_manifest_hash} instructs step '{failing_step}' which contradicts observed behaviour"
                ),
                subject: crate::gaps::GapSubject::question_shape(&format!(
                    "skill divergence: {skill_manifest_hash}"
                ))
                .unwrap_or_else(|_| crate::gaps::GapSubject::Intent {
                    intent_id: "plan-reality-divergence".to_string(),
                }),
                closure_criteria: crate::gaps::ClosureCriteria::ArtifactPromoted {
                    candidate_id: format!("skill:patch:{skill_manifest_hash}"),
                },
            }
        } else {
            KnowledgeCause::Environmental
        }
    }

    /// Provider outage or network partition — environmental, not knowledge.
    pub fn classify_provider_outage() -> KnowledgeCause {
        KnowledgeCause::Environmental
    }

    /// Network error — environmental, not knowledge.
    pub fn classify_network_error() -> KnowledgeCause {
        KnowledgeCause::Environmental
    }
}

/// File a knowledge-level repair gap entry and emit a typed repair record.
///
/// Returns the gap id if a knowledge cause was filed, or `None` if the
/// cause was environmental (nothing to file).
pub fn file_knowledge_repair(
    ledger: &mut crate::gaps::GapLedger,
    repair_ledger: &dyn RepairLedger,
    cause: KnowledgeCause,
    evidence: Vec<crate::gaps::Citation>,
    session_id: Option<String>,
    attempt_id: Option<String>,
    repair_record_chain: Vec<String>,
) -> crate::error::Result<Option<String>> {
    match cause {
        KnowledgeCause::Knowledge {
            statement,
            subject,
            closure_criteria,
        } => {
            let now = Utc::now();
            let mut citations = evidence;
            // Cite the repair-record chain as evidence.
            for record_id in repair_record_chain {
                if let Ok(citation) = crate::gaps::Citation::new(
                    crate::gaps::CitationKind::RunReceipt,
                    record_id,
                    Some("repair record in the escalation chain".to_string()),
                ) {
                    citations.push(citation);
                }
            }
            let gap_id = ledger.file_gap(
                subject,
                statement,
                citations,
                crate::gaps::GapOrigin::RuntimeCorrection,
                closure_criteria,
                1,
                1000, // default failure-cost estimate
                "runtime:knowledge-repair",
                now,
            )?;
            let record = RepairRecordBuilder::new()
                .component(RepairComponent::AttemptScheduler)
                .trigger(RepairTrigger::ProviderError {
                    error_class: "knowledge_gap".to_string(),
                    provider_call_id: gap_id.clone(),
                })
                .action(RepairAction::ProviderRetry {
                    rung: RepairRung::Knowledge,
                })
                .outcome(RepairOutcome::Escalated)
                .start_time(now)
                .end_time(now)
                .session_id(session_id.unwrap_or_default())
                .attempt_id(attempt_id.unwrap_or_default())
                .citation(gap_id.clone())
                .build();
            let _ = repair_ledger.append(record);
            Ok(Some(gap_id))
        }
        KnowledgeCause::Environmental => Ok(None),
    }
}

/// One typed repair record. Immutable once created.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepairRecord {
    /// Unique record id (`rr-{uuidv4}`).
    pub record_id: String,
    /// The component that performed the repair.
    pub component: RepairComponent,
    /// What triggered the repair.
    pub trigger: RepairTrigger,
    /// The action taken (rung + operation).
    pub action: RepairAction,
    /// How the episode ended.
    pub outcome: RepairOutcome,
    /// When the episode started.
    pub start_time: DateTime<Utc>,
    /// When the episode finished.
    pub end_time: DateTime<Utc>,
    /// The session the repair concerned, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The attempt the repair concerned, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<String>,
    /// Citations into the evidence: log positions, attempt ids, provider call
    /// ids, dependency-change record ids.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub citations: Vec<String>,
    /// For retry episodes: the total number of internal attempts (one record
    /// carries the count, not N records).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_count: Option<u32>,
}

impl RepairRecord {
    /// The wall-clock duration of the episode.
    pub fn duration_ms(&self) -> i64 {
        (self.end_time - self.start_time).num_milliseconds()
    }
}

// ---------------------------------------------------------------------------
// Query filters
// ---------------------------------------------------------------------------

/// Filter criteria for repair-record queries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepairQuery {
    /// Include only records from these components (empty = all).
    pub components: Vec<RepairComponent>,
    /// Include only records with these trigger classes (empty = all).
    /// Trigger classes are matched by the variant name of [`RepairTrigger`].
    pub trigger_classes: Vec<String>,
    /// Include only records with these outcomes (empty = all).
    pub outcomes: Vec<RepairOutcome>,
    /// Include only records whose start_time is >= this.
    pub from: Option<DateTime<Utc>>,
    /// Include only records whose start_time < this.
    pub until: Option<DateTime<Utc>>,
    /// Include only records concerning this session.
    pub session_id: Option<String>,
    /// Include only records concerning this attempt.
    pub attempt_id: Option<String>,
}

impl RepairQuery {
    /// Match one record against the query. All declared criteria must hold.
    pub fn matches(&self, record: &RepairRecord) -> bool {
        if !self.components.is_empty() && !self.components.contains(&record.component) {
            return false;
        }
        if !self.trigger_classes.is_empty() {
            let class = trigger_class_name(&record.trigger);
            if !self.trigger_classes.iter().any(|c| c == &class) {
                return false;
            }
        }
        if !self.outcomes.is_empty() && !self.outcomes.contains(&record.outcome) {
            return false;
        }
        if let Some(from) = self.from {
            if record.start_time < from {
                return false;
            }
        }
        if let Some(until) = self.until {
            if record.start_time >= until {
                return false;
            }
        }
        if let Some(ref sid) = self.session_id {
            if record.session_id.as_ref() != Some(sid) {
                return false;
            }
        }
        if let Some(ref aid) = self.attempt_id {
            if record.attempt_id.as_ref() != Some(aid) {
                return false;
            }
        }
        true
    }
}

/// The string class name of a trigger (the serde tag value).
fn trigger_class_name(trigger: &RepairTrigger) -> String {
    // The tag values are snake_case variant names.
    match trigger {
        RepairTrigger::ValidationFailure { .. } => "validation_failure".to_owned(),
        RepairTrigger::ProviderError { .. } => "provider_error".to_owned(),
        RepairTrigger::ContextOverflow { .. } => "context_overflow".to_owned(),
        RepairTrigger::Crash { .. } => "crash".to_owned(),
        RepairTrigger::HeartbeatLost { .. } => "heartbeat_lost".to_owned(),
        RepairTrigger::StuckTurn { .. } => "stuck_turn".to_owned(),
        RepairTrigger::DependencyChange { .. } => "dependency_change".to_owned(),
        RepairTrigger::FlappingTool { .. } => "flapping_tool".to_owned(),
    }
}

// ---------------------------------------------------------------------------
// Ledger trait
// ---------------------------------------------------------------------------

/// The repair-record sink/ledger: append-only, queryable.
pub trait RepairLedger: Send + Sync {
    /// Append a record. Returns the record id on success.
    fn append(&self, record: RepairRecord) -> crate::error::Result<String>;

    /// Query records matching the filter, newest first.
    fn query(&self, filter: &RepairQuery) -> crate::error::Result<Vec<RepairRecord>>;

    /// Total number of records held.
    fn len(&self) -> crate::error::Result<usize>;

    /// True when no records are held.
    fn is_empty(&self) -> crate::error::Result<bool> {
        Ok(self.len()? == 0)
    }
}

// ---------------------------------------------------------------------------
// In-memory ledger (for testing and single-node deployments)
// ---------------------------------------------------------------------------

/// An in-memory, append-only repair ledger. Not durable: records are lost on
/// process restart. Suitable for testing and for deployments that replay the
/// audit stream from a persistent journal.
#[derive(Debug, Default)]
pub struct InMemoryRepairLedger {
    records: Mutex<Vec<RepairRecord>>,
}

impl InMemoryRepairLedger {
    /// Create an empty ledger.
    pub fn new() -> Self {
        Self {
            records: Mutex::new(Vec::new()),
        }
    }
}

impl RepairLedger for InMemoryRepairLedger {
    fn append(&self, record: RepairRecord) -> crate::error::Result<String> {
        let id = record.record_id.clone();
        let mut guard = self.records.lock().unwrap();
        guard.push(record);
        Ok(id)
    }

    fn query(&self, filter: &RepairQuery) -> crate::error::Result<Vec<RepairRecord>> {
        let guard = self.records.lock().unwrap();
        let mut matched: Vec<RepairRecord> = guard
            .iter()
            .filter(|r| filter.matches(r))
            .cloned()
            .collect();
        // Newest first.
        matched.sort_by_key(|a| std::cmp::Reverse(a.start_time));
        Ok(matched)
    }

    fn len(&self) -> crate::error::Result<usize> {
        Ok(self.records.lock().unwrap().len())
    }
}

// ---------------------------------------------------------------------------
// File-backed ledger (durable, suitable for single-node deployments)
// ---------------------------------------------------------------------------

use std::path::{Path, PathBuf};

/// A file-backed, append-only repair ledger. Each record is written as one
/// JSON file under `{root}/repairs/{record_id}.json`. Not high-throughput:
/// every append takes an exclusive lock and opens the directory listing.
/// Suitable for the audit stream, which is low-volume and append-heavy.
#[derive(Debug)]
pub struct FileRepairLedger {
    root: PathBuf,
    lock: Mutex<()>,
}

impl FileRepairLedger {
    /// Open (or create) a ledger at `root`. Records live under
    /// `{root}/repairs/`. The directory is created on first append.
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
            lock: Mutex::new(()),
        }
    }

    fn repairs_dir(&self) -> PathBuf {
        self.root.join("repairs")
    }

    fn path_for(&self, record_id: &str) -> PathBuf {
        self.repairs_dir().join(format!("{record_id}.json"))
    }
}

impl RepairLedger for FileRepairLedger {
    fn append(&self, record: RepairRecord) -> crate::error::Result<String> {
        let _guard = self.lock.lock().unwrap();
        let dir = self.repairs_dir();
        std::fs::create_dir_all(&dir)
            .map_err(|e| crate::error::RustyError::Tool(format!("create repairs dir: {e}")))?;
        let path = self.path_for(&record.record_id);
        let raw = serde_json::to_vec_pretty(&record)
            .map_err(|e| crate::error::RustyError::Tool(format!("serialize repair record: {e}")))?;
        std::fs::write(&path, raw)
            .map_err(|e| crate::error::RustyError::Tool(format!("write repair record: {e}")))?;
        Ok(record.record_id.clone())
    }

    fn query(&self, filter: &RepairQuery) -> crate::error::Result<Vec<RepairRecord>> {
        let _guard = self.lock.lock().unwrap();
        let dir = self.repairs_dir();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Ok(Vec::new());
        };

        let mut matched = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let raw = match std::fs::read_to_string(&path) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let record: RepairRecord = match serde_json::from_str(&raw) {
                Ok(r) => r,
                Err(_) => continue,
            };
            if filter.matches(&record) {
                matched.push(record);
            }
        }
        // Newest first.
        matched.sort_by_key(|a| std::cmp::Reverse(a.start_time));
        Ok(matched)
    }

    fn len(&self) -> crate::error::Result<usize> {
        let _guard = self.lock.lock().unwrap();
        let dir = self.repairs_dir();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Ok(0);
        };
        let count = entries
            .flatten()
            .filter(|e| e.path().extension().and_then(|ext| ext.to_str()) == Some("json"))
            .count();
        Ok(count)
    }
}

// ---------------------------------------------------------------------------
// Buffered sink with best-effort retry and drop metric
// ---------------------------------------------------------------------------

/// Metrics exposed by the buffered repair sink.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RepairSinkMetrics {
    /// Records successfully flushed to the inner ledger.
    pub flushed: u64,
    /// Records dropped after buffer exhaustion.
    pub dropped: u64,
    /// Current records in the retry buffer.
    pub buffered: usize,
}

/// A repair-record sink that buffers records and retries them best-effort
/// against an inner ledger. If the inner ledger fails, the record is queued
/// for retry; if the buffer is full, the record is dropped and the drop
/// counter increments.
///
/// A background flush is **not** provided: callers must call `try_flush`
/// (or let a scheduled task call it) to drain the buffer. This keeps the
/// sink free of async runtime dependencies.
pub struct BufferedRepairSink {
    inner: Arc<dyn RepairLedger>,
    buffer: Mutex<VecDeque<RepairRecord>>,
    max_buffer: usize,
    metrics: Mutex<RepairSinkMetrics>,
}

impl std::fmt::Debug for BufferedRepairSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BufferedRepairSink")
            .field("max_buffer", &self.max_buffer)
            .field("buffered", &self.buffer.lock().unwrap().len())
            .field("metrics", &self.metrics.lock().unwrap())
            .finish_non_exhaustive()
    }
}

impl BufferedRepairSink {
    /// Wrap `inner` with a bounded retry buffer of `max_buffer` records.
    pub fn new(inner: Arc<dyn RepairLedger>, max_buffer: usize) -> Self {
        Self {
            inner,
            buffer: Mutex::new(VecDeque::with_capacity(max_buffer)),
            max_buffer,
            metrics: Mutex::new(RepairSinkMetrics::default()),
        }
    }

    /// Emit a record. On inner-ledger failure, queues for retry; on buffer
    /// full, drops and increments the drop counter. Never blocks the caller.
    pub fn emit(&self, record: RepairRecord) {
        if self.inner.append(record.clone()).is_err() {
            let mut buf = self.buffer.lock().unwrap();
            if buf.len() < self.max_buffer {
                buf.push_back(record);
            } else {
                let mut m = self.metrics.lock().unwrap();
                m.dropped += 1;
            }
        } else {
            let mut m = self.metrics.lock().unwrap();
            m.flushed += 1;
        }
    }

    /// Attempt to drain the retry buffer into the inner ledger.
    /// Returns how many records were flushed in this call.
    pub fn try_flush(&self) -> usize {
        let mut flushed = 0usize;
        loop {
            let maybe_record = {
                let mut buf = self.buffer.lock().unwrap();
                buf.pop_front()
            };
            match maybe_record {
                Some(record) => {
                    if self.inner.append(record.clone()).is_err() {
                        // Put it back at the front and stop — if the ledger is
                        // still down, the rest will likely fail too.
                        let mut buf = self.buffer.lock().unwrap();
                        buf.push_front(record);
                        break;
                    } else {
                        flushed += 1;
                        let mut m = self.metrics.lock().unwrap();
                        m.flushed += 1;
                        m.buffered = m.buffered.saturating_sub(1);
                    }
                }
                None => break,
            }
        }
        flushed
    }

    /// Current metrics snapshot.
    pub fn metrics(&self) -> RepairSinkMetrics {
        let mut m = self.metrics.lock().unwrap();
        m.buffered = self.buffer.lock().unwrap().len();
        *m
    }

    /// Query the inner ledger directly (bypassing the buffer).
    pub fn query(&self, filter: &RepairQuery) -> crate::error::Result<Vec<RepairRecord>> {
        self.inner.query(filter)
    }
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// A convenient builder for [`RepairRecord`].
#[derive(Debug, Clone, Default)]
pub struct RepairRecordBuilder {
    record_id: Option<String>,
    component: Option<RepairComponent>,
    trigger: Option<RepairTrigger>,
    action: Option<RepairAction>,
    outcome: Option<RepairOutcome>,
    start_time: Option<DateTime<Utc>>,
    end_time: Option<DateTime<Utc>>,
    session_id: Option<String>,
    attempt_id: Option<String>,
    citations: Vec<String>,
    attempt_count: Option<u32>,
}

impl RepairRecordBuilder {
    /// Start building.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the record id. If omitted, a UUID is generated at build time.
    pub fn record_id(mut self, id: impl Into<String>) -> Self {
        self.record_id = Some(id.into());
        self
    }

    /// Set the issuing component.
    pub fn component(mut self, component: RepairComponent) -> Self {
        self.component = Some(component);
        self
    }

    /// Set the trigger.
    pub fn trigger(mut self, trigger: RepairTrigger) -> Self {
        self.trigger = Some(trigger);
        self
    }

    /// Set the action.
    pub fn action(mut self, action: RepairAction) -> Self {
        self.action = Some(action);
        self
    }

    /// Set the outcome.
    pub fn outcome(mut self, outcome: RepairOutcome) -> Self {
        self.outcome = Some(outcome);
        self
    }

    /// Set start time.
    pub fn start_time(mut self, t: DateTime<Utc>) -> Self {
        self.start_time = Some(t);
        self
    }

    /// Set end time.
    pub fn end_time(mut self, t: DateTime<Utc>) -> Self {
        self.end_time = Some(t);
        self
    }

    /// Set session id.
    pub fn session_id(mut self, id: impl Into<String>) -> Self {
        self.session_id = Some(id.into());
        self
    }

    /// Set attempt id.
    pub fn attempt_id(mut self, id: impl Into<String>) -> Self {
        self.attempt_id = Some(id.into());
        self
    }

    /// Add a citation.
    pub fn citation(mut self, citation: impl Into<String>) -> Self {
        self.citations.push(citation.into());
        self
    }

    /// Set the attempt count (for retry episodes).
    pub fn attempt_count(mut self, count: u32) -> Self {
        self.attempt_count = Some(count);
        self
    }

    /// Build the record. Panics on missing required fields (ids are
    /// auto-generated, but component/trigger/action/outcome/times are
    /// mandatory).
    pub fn build(self) -> RepairRecord {
        RepairRecord {
            record_id: self
                .record_id
                .unwrap_or_else(|| format!("rr-{}", uuid::Uuid::new_v4())),
            component: self.component.expect("RepairRecord component is required"),
            trigger: self.trigger.expect("RepairRecord trigger is required"),
            action: self.action.expect("RepairRecord action is required"),
            outcome: self.outcome.expect("RepairRecord outcome is required"),
            start_time: self
                .start_time
                .expect("RepairRecord start_time is required"),
            end_time: self.end_time.expect("RepairRecord end_time is required"),
            session_id: self.session_id,
            attempt_id: self.attempt_id,
            citations: self.citations,
            attempt_count: self.attempt_count,
        }
    }
}
