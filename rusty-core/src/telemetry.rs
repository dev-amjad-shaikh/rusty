//! The telemetry ledger: a best-effort operational mirror of Flight Recorder
//! journals.
//!
//! The journal ([`crate::journal`]) is evidence: append-only, hash-chained,
//! never lossy. Telemetry is the operational shadow of that evidence —
//! ordered ledger records derived from journaled events, shaped for
//! dashboards, alerting, and fleet analytics rather than for replay. The
//! asymmetry is deliberate and stated plainly: **telemetry may lose records
//! (a crashed mirror re-mirrors from its cursor; a full sink drops), the
//! journal may not.** Anything an audit must see lives in the journal; the
//! ledger is a derived view that [`TelemetryMirror`] can always rebuild by
//! re-mirroring a snapshot from seq 0 — dedupe on `(run_id, seq)` makes the
//! rebuild idempotent.
//!
//! Three pieces:
//!
//! - [`LedgerRecord`] — one mirrored event with its severity **pre-mapped**
//!   at mirror time ([`severity_of`]), so consumers render urgency instead
//!   of re-deriving it (and never disagree with each other about it).
//! - The **redaction waterfall** ([`Redactor`]) — a chain of redactors the
//!   mirror applies in declared order before a record lands. Each redactor
//!   may transform fields or suppress the whole record; every transformation
//!   leaves a [`RedactionMark`] naming the redactor and the field, never
//!   the removed content — the ledger attests *that* redaction happened, and
//!   the journal remains the only place the unredacted bytes exist.
//! - The backends ([`TelemetryLedger`]): [`InMemoryLedger`] for tests and
//!   ephemeral processes, [`JsonlLedger`] for a single-writer file. Both
//!   dedupe on `(run_id, seq)`, so a retried or resumed mirror converges.
//!
//! The handoff half is [`MirrorCursor`]: a serde-able `(run_id, next_seq)`
//! the caller persists wherever it likes; handing it back to
//! [`TelemetryMirror::mirror_from`] resumes mirroring without re-reading the
//! prefix — and losing it costs a re-scan, never correctness.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{Result, RustyError};
use crate::journal::JournalSnapshot;
use crate::llm::Usage;
use crate::record::{
    sha256_hex, ArtifactRef, Effect, EventStatus, PayloadRef, RunEvent, RunEventKind,
};

/// The current on-disk format version of a [`JsonlLedger`] file.
///
/// Bump only on a breaking change to the ledger envelope; additive record
/// evolution uses serde defaults so previously written ledgers keep loading.
pub const TELEMETRY_FORMAT_VERSION: u32 = 1;

/// The `format` tag of a ledger file's header line — what a reader checks
/// before interpreting any record line.
const LEDGER_FORMAT_TAG: &str = "rusty-telemetry-ledger";

/// The severity of a ledger record, mapped once at mirror time.
///
/// Pre-mapping is the point: a record's urgency is a function of the event
/// it mirrors, computed by the one component that still has the event's full
/// context. Downstream consumers (dashboards, alert routes) order and filter
/// by this field instead of re-implementing — and drifting — the mapping.
///
/// The derive order is the severity ladder (`Ord` compares by declaration
/// order), which is what [`SeverityFloor`] filters with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Mechanical scaffolding an operator almost never reads: super-step
    /// boundaries, node inputs, routing decisions.
    Debug,

    /// A normal recorded fact: model/tool/remote calls, checkpoint writes,
    /// receipts, control-plane lineage.
    Info,

    /// Control flow a human should notice: the run suspended on an
    /// interrupt. Not a failure — a hand-off.
    Notice,

    /// Something was refused or degraded: capability denials, credential
    /// refusals, a connection that needs re-auth, an artifact whose bytes
    /// are gone, a supervision restart.
    Warning,

    /// The recorded operation failed ([`EventStatus::Error`]).
    Error,
}

/// Map a journaled event to its ledger severity — the single mapping every
/// consumer shares.
///
/// Status beats kind: a failed event is [`Severity::Error`] whatever it
/// recorded. Refusals and denials are [`Severity::Warning`] — nothing
/// executed, but the refusal is exactly what an operator watches for (the
/// journal's own "the event is the evidence that nothing happened"
/// discipline, lifted to operations). Interrupts are [`Severity::Notice`]:
/// control flow, not failure. The executor's scaffolding kinds are
/// [`Severity::Debug`]; everything else is a routine [`Severity::Info`].
pub fn severity_of(event: &RunEvent) -> Severity {
    if event.status == EventStatus::Error {
        return Severity::Error;
    }
    if event.status == EventStatus::Interrupted || event.kind == RunEventKind::Interrupt {
        return Severity::Notice;
    }
    match event.kind {
        RunEventKind::CapsuleDenied
        | RunEventKind::CredentialDenied
        | RunEventKind::EnvSecretDenied
        | RunEventKind::ShadowEffectRefused
        | RunEventKind::ArtifactUnavailable
        | RunEventKind::ConnectionNeedsReauth
        | RunEventKind::SupervisionEvent => Severity::Warning,
        RunEventKind::SuperStepStart
        | RunEventKind::SuperStepEnd
        | RunEventKind::NodeInput
        | RunEventKind::RoutingDecision => Severity::Debug,
        _ => Severity::Info,
    }
}

/// What a redactor did to one field. The audit half of the waterfall: the
/// mark names the redactor, the field, and the action — never the removed
/// content. Content an operator may not see in telemetry stays readable in
/// exactly one place: the journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedactionMark {
    /// The redactor that acted ([`Redactor::name`]).
    pub redactor: String,

    /// The record field it acted on (`input`, `output`, ...).
    pub field: String,

    /// What it did.
    pub action: RedactionAction,
}

/// The kind of redaction a [`RedactionMark`] attests to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RedactionAction {
    /// The field was transformed in place (e.g. payload bytes replaced by
    /// their content hash).
    Transformed,
    /// The field's content was dropped outright.
    Cleared,
}

/// One mirrored journal event: the telemetry ledger's atomic record.
///
/// Carries the event's identifying and analytic fields verbatim (ids, kind,
/// effect class, latency, usage, cost, causal parent) plus the two things
/// only the mirror can supply: the pre-mapped [`Severity`] and the
/// [`RedactionMark`]s the waterfall applied.
///
/// Payloads travel as the event's own [`PayloadRef`]s. An `Artifact`
/// reference in a ledger record is *metadata* — content address plus byte
/// count — and intentionally dangling: the ledger never holds payload bytes
/// it did not inline, and the reference is what a follow-up journal query
/// resolves. Redactors that must keep payload bytes out of the ledger
/// transform or clear the inline half ([`PayloadHasher`], [`PayloadDrop`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LedgerRecord {
    /// The run the mirrored event belongs to.
    pub run_id: String,

    /// The thread (session) the run belongs to.
    pub thread_id: String,

    /// The mirrored event's journal sequence number. `(run_id, seq)` is the
    /// dedupe key: a record is stored at most once per key.
    pub seq: u64,

    /// What the journaled event recorded.
    pub kind: RunEventKind,

    /// The declared effect classification of whatever produced the event.
    pub effect: Effect,

    /// The node the event is about, where applicable.
    pub node_id: Option<String>,

    /// How the recorded operation ended.
    pub status: EventStatus,

    /// The mirror-mapped severity (see [`severity_of`]).
    pub severity: Severity,

    /// Input payload, post-waterfall.
    pub input: Option<PayloadRef>,

    /// Output payload, post-waterfall.
    pub output: Option<PayloadRef>,

    /// Recorded latency in milliseconds, when measured.
    pub latency_ms: Option<u64>,

    /// Token usage, when the provider reported it.
    pub tokens: Option<Usage>,

    /// Journaled monetary cost in USD, when recorded.
    pub cost_usd: Option<f64>,

    /// The mirrored event's causal parent id, when it has one.
    pub parent: Option<String>,

    /// When the event was recorded (the run's clock).
    pub recorded_at: DateTime<Utc>,

    /// The redactions the waterfall applied to this record, in application
    /// order. Empty when no redactor touched it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub redactions: Vec<RedactionMark>,
}

impl LedgerRecord {
    /// Mirror one journaled event into a record, pre-waterfall.
    fn from_event(event: &RunEvent) -> Self {
        Self {
            run_id: event.run_id.clone(),
            thread_id: event.thread_id.clone(),
            seq: event.seq,
            kind: event.kind,
            effect: event.effect,
            node_id: event.node_id.clone(),
            status: event.status,
            severity: severity_of(event),
            input: event.input.clone(),
            output: event.output.clone(),
            latency_ms: event.latency_ms,
            tokens: event.tokens,
            cost_usd: event.cost_usd,
            parent: event.parent.clone(),
            recorded_at: event.recorded_at,
            redactions: Vec::new(),
        }
    }
}

/// One stage of the redaction waterfall.
///
/// Redactors run in declared order, each seeing the record as the previous
/// stage left it. A stage transforms fields in place — pushing a
/// [`RedactionMark`] for every field it touches — or suppresses the whole
/// record by returning `true`. The mark is mandatory by convention, not by
/// type: a redactor that transforms without marking lies to the audit, and
/// the built-ins model the honest shape.
pub trait Redactor: Send + Sync {
    /// The redactor's name, recorded on every [`RedactionMark`] it leaves.
    fn name(&self) -> &str;

    /// Transform `record` in place, returning `true` to suppress it
    /// entirely (it never lands; the mirror's report counts the
    /// suppression).
    fn redact(&self, record: &mut LedgerRecord) -> bool;
}

/// A redactor from a closure — the escape hatch for site-specific policy
/// without a newtype per policy.
#[derive(Debug, Clone)]
pub struct RedactorFn<F> {
    name: String,
    f: F,
}

/// Wrap `f` as a named [`Redactor`].
pub fn redactor_fn<F>(name: impl Into<String>, f: F) -> RedactorFn<F>
where
    F: Fn(&mut LedgerRecord) -> bool + Send + Sync,
{
    RedactorFn {
        name: name.into(),
        f,
    }
}

impl<F> Redactor for RedactorFn<F>
where
    F: Fn(&mut LedgerRecord) -> bool + Send + Sync,
{
    fn name(&self) -> &str {
        &self.name
    }

    fn redact(&self, record: &mut LedgerRecord) -> bool {
        (self.f)(record)
    }
}

/// The payload-hashing redactor: every inline payload is replaced by its
/// content address ([`PayloadRef::Artifact`]) before the record lands, so
/// the ledger carries where the bytes live and how big they are, never the
/// bytes. The standard first stage of any waterfall over journals that may
/// hold prompts, tool arguments, or responses.
#[derive(Debug, Clone, Copy, Default)]
pub struct PayloadHasher;

impl Redactor for PayloadHasher {
    fn name(&self) -> &str {
        "payload-hasher"
    }

    fn redact(&self, record: &mut LedgerRecord) -> bool {
        for (field, slot) in [("input", &mut record.input), ("output", &mut record.output)] {
            let Some(PayloadRef::Inline(value)) = slot else {
                continue;
            };
            // The same canonical-content hash `PayloadRef::content_hash`
            // computes, plus the serialized size — an `ArtifactRef` is
            // exactly address + size.
            let bytes = serde_json::to_vec(value).unwrap_or_default();
            *slot = Some(PayloadRef::Artifact(ArtifactRef {
                sha256: sha256_hex(&bytes),
                bytes: bytes.len() as u64,
            }));
            record.redactions.push(RedactionMark {
                redactor: self.name().to_owned(),
                field: field.to_owned(),
                action: RedactionAction::Transformed,
            });
        }
        false
    }
}

/// The payload-dropping redactor: inline payloads are removed outright. For
/// ledgers where even a content address says too much; the record keeps its
/// shape, the payload fields land empty, and the marks attest to the drop.
#[derive(Debug, Clone, Copy, Default)]
pub struct PayloadDrop;

impl Redactor for PayloadDrop {
    fn name(&self) -> &str {
        "payload-drop"
    }

    fn redact(&self, record: &mut LedgerRecord) -> bool {
        for (field, slot) in [("input", &mut record.input), ("output", &mut record.output)] {
            if slot.is_some() {
                *slot = None;
                record.redactions.push(RedactionMark {
                    redactor: self.name().to_owned(),
                    field: field.to_owned(),
                    action: RedactionAction::Cleared,
                });
            }
        }
        false
    }
}

/// The severity gate: suppress every record below `floor`. Volume control
/// implemented as a redactor so the suppression is a declared pipeline stage
/// — visible in the mirror's report — rather than a sink's silent filter.
#[derive(Debug, Clone, Copy)]
pub struct SeverityFloor {
    floor: Severity,
}

impl SeverityFloor {
    /// A gate suppressing records with severity below `floor`.
    pub fn new(floor: Severity) -> Self {
        Self { floor }
    }
}

impl Redactor for SeverityFloor {
    fn name(&self) -> &str {
        "severity-floor"
    }

    fn redact(&self, record: &mut LedgerRecord) -> bool {
        record.severity < self.floor
    }
}

/// What a ledger append did with a record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendOutcome {
    /// The record landed.
    Stored,
    /// A record with the same `(run_id, seq)` was already stored; the
    /// append was dropped. Dedupe is what makes re-mirroring after a cursor
    /// loss a re-scan, never a double-count.
    Duplicate,
}

/// The ledger backend seam.
///
/// Delivery is **best-effort** at every implementation: an append that
/// fails returns an error the mirror propagates, but nothing retries on the
/// record's behalf, and no backend fsyncs per record. Durability lives in
/// the journal; the ledger is rebuildable from it.
#[async_trait]
pub trait TelemetryLedger: Send + Sync {
    /// Append one record. Implementations dedupe on `(run_id, seq)`: a
    /// repeat append is a [`AppendOutcome::Duplicate`], never an error and
    /// never a second copy.
    async fn append(&self, record: LedgerRecord) -> Result<AppendOutcome>;

    /// Records for one run with `seq` greater than `after` (`None` reads
    /// from the start), in sequence order, at most `limit`.
    async fn read(
        &self,
        run_id: &str,
        after: Option<u64>,
        limit: usize,
    ) -> Result<Vec<LedgerRecord>>;

    /// The highest sequence number stored for a run, when any — the resume
    /// hint a mirror can rebuild a [`MirrorCursor`] from.
    async fn cursor(&self, run_id: &str) -> Result<Option<u64>>;
}

/// In-memory [`TelemetryLedger`]: the dev/test backend, lost on restart —
/// which costs nothing, because the journal outlives it and the mirror
/// rebuilds.
#[derive(Debug, Default, Clone)]
pub struct InMemoryLedger {
    // (run_id, seq) -> record; the BTreeMap key IS the dedupe index and the
    // per-run ordering at once.
    records: Arc<Mutex<BTreeMap<(String, u64), LedgerRecord>>>,
}

impl InMemoryLedger {
    /// An empty ledger. Clones share the same records.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<(String, u64), LedgerRecord>> {
        // Poison means a writer panicked mid-append; the map is plain data
        // and stays coherent, so recovering is safe (the journal's own lock
        // discipline).
        self.records.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[async_trait]
impl TelemetryLedger for InMemoryLedger {
    async fn append(&self, record: LedgerRecord) -> Result<AppendOutcome> {
        let key = (record.run_id.clone(), record.seq);
        let mut records = self.lock();
        if records.contains_key(&key) {
            return Ok(AppendOutcome::Duplicate);
        }
        records.insert(key, record);
        Ok(AppendOutcome::Stored)
    }

    async fn read(
        &self,
        run_id: &str,
        after: Option<u64>,
        limit: usize,
    ) -> Result<Vec<LedgerRecord>> {
        let from = after.map_or(0, |seq| seq.saturating_add(1));
        Ok(self
            .lock()
            .range((run_id.to_owned(), from)..)
            .take_while(|((run, _), _)| run == run_id)
            .take(limit)
            .map(|(_, record)| record.clone())
            .collect())
    }

    async fn cursor(&self, run_id: &str) -> Result<Option<u64>> {
        Ok(self
            .lock()
            .range((run_id.to_owned(), 0)..)
            .rev()
            .find_map(|((run, seq), _)| (run == run_id).then_some(*seq)))
    }
}

/// Map a ledger IO failure into the module's error convention — the same
/// `Serialization`-over-`io` shape the journal's artifact store uses.
fn ledger_io_error(context: String, e: std::io::Error) -> RustyError {
    RustyError::Serialization(serde_json::Error::io(std::io::Error::new(
        e.kind(),
        format!("{context}: {e}"),
    )))
}

/// JSONL-file [`TelemetryLedger`]: one line per record behind a
/// format-versioned header line, for single-writer ops processes.
///
/// The first line of the file is the header
/// (`{"format": "rusty-telemetry-ledger", "format_version": N}`), written
/// when the file is created and checked on every open: a ledger whose header
/// names a newer format version than this build supports is **refused** with
/// the found version, the supported version, and the upgrade direction —
/// the bytes are never reinterpreted (the checkpoint store's
/// `ensure_supported_format` discipline, applied to the ledger).
///
/// Appends are atomic per line under an in-process write lock and deduped
/// against an in-memory index rebuilt by scanning the file at open. The
/// documented limits are the file backend's own: one writer process per
/// file, and a whole-file scan at open — a ledger is an ops-side mirror,
/// not the system of record, so neither bound is a durability concern.
#[derive(Debug, Clone)]
pub struct JsonlLedger {
    path: PathBuf,
    // The dedupe index, rebuilt from the file at open and extended on every
    // stored append. A std mutex: only ever held for map operations, never
    // across an `.await`.
    seen: Arc<Mutex<HashSet<(String, u64)>>>,
    // Serializes appends. A tokio mutex because the guard lives across the
    // write's `.await`s — a std guard would make the future `!Send` (the
    // checkpointer's put-lock discipline).
    write_lock: Arc<tokio::sync::Mutex<()>>,
}

impl JsonlLedger {
    /// Open (creating if needed) the ledger at `path`, verifying the header
    /// line's format version when the file already has one.
    pub async fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let ledger = Self {
            path,
            seen: Arc::new(Mutex::new(HashSet::new())),
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
        };
        match tokio::fs::read_to_string(&ledger.path).await {
            Ok(contents) if !contents.is_empty() => ledger.load(&contents)?,
            Ok(_) => {
                // An existing but empty file (a crash between create and the
                // header write): stamp the header and start clean.
                ledger.write_line(&ledger.header_line()).await?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if let Some(parent) = ledger.path.parent() {
                    tokio::fs::create_dir_all(parent)
                        .await
                        .map_err(|e| ledger_io_error("create ledger directory".into(), e))?;
                }
                ledger.write_line(&ledger.header_line()).await?;
            }
            Err(e) => return Err(ledger_io_error("open ledger".into(), e)),
        }
        Ok(ledger)
    }

    /// The path the ledger appends to.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    fn lock(&self) -> MutexGuard<'_, HashSet<(String, u64)>> {
        self.seen.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn header_line(&self) -> String {
        serde_json::json!({
            "format": LEDGER_FORMAT_TAG,
            "format_version": TELEMETRY_FORMAT_VERSION,
        })
        .to_string()
    }

    /// Append one line (record or header) under the write lock. Best-effort
    /// by contract: no fsync, no rotation — the journal is the durable copy.
    async fn write_line(&self, line: &str) -> Result<()> {
        use tokio::io::AsyncWriteExt as _;
        let _guard = self.write_lock.lock().await;
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await
            .map_err(|e| ledger_io_error("open ledger for append".into(), e))?;
        file.write_all(line.as_bytes())
            .await
            .map_err(|e| ledger_io_error("append to ledger".into(), e))?;
        file.write_all(b"\n")
            .await
            .map_err(|e| ledger_io_error("append to ledger".into(), e))
    }

    /// Load an existing file: verify the header's format version, then index
    /// every parseable record line for dedupe. A corrupt record line is
    /// skipped with a warning (the forgiving-scan discipline); a format
    /// refusal is never skipped.
    fn load(&self, contents: &str) -> Result<()> {
        let mut lines = contents.lines();
        let Some(header) = lines.next().filter(|line| !line.is_empty()) else {
            return Err(ledger_io_error(
                format!("open ledger `{}`", self.path.display()),
                std::io::Error::new(std::io::ErrorKind::InvalidData, "missing header line"),
            ));
        };
        let header: serde_json::Value = serde_json::from_str(header).map_err(|e| {
            ledger_io_error(
                format!("ledger `{}` has an unreadable header line", self.path.display()),
                std::io::Error::new(std::io::ErrorKind::InvalidData, e),
            )
        })?;
        let found = header
            .get("format_version")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as u32;
        let tag = header.get("format").and_then(serde_json::Value::as_str);
        if tag != Some(LEDGER_FORMAT_TAG) || found > TELEMETRY_FORMAT_VERSION {
            return Err(ledger_io_error(
                format!("open ledger `{}`", self.path.display()),
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "ledger format version {found} is not supported by this build, which \
                         supports version {TELEMETRY_FORMAT_VERSION} — upgrade the runtime to \
                         read this ledger; the bytes are never reinterpreted"
                    ),
                ),
            ));
        }
        let mut seen = self.lock();
        for line in lines {
            match serde_json::from_str::<LedgerRecord>(line) {
                Ok(record) => {
                    seen.insert((record.run_id, record.seq));
                }
                Err(e) => {
                    tracing::warn!(
                        path = %self.path.display(),
                        error = %e,
                        "skipping unparseable ledger line during open"
                    );
                }
            }
        }
        Ok(())
    }
}

#[async_trait]
impl TelemetryLedger for JsonlLedger {
    async fn append(&self, record: LedgerRecord) -> Result<AppendOutcome> {
        use tokio::io::AsyncWriteExt as _;
        let key = (record.run_id.clone(), record.seq);
        let line = serde_json::to_string(&record)?;
        // Check-and-write under the one lock: the dedupe check outside it
        // would race a concurrent append of the same record into a
        // duplicate line.
        let _guard = self.write_lock.lock().await;
        if self.lock().contains(&key) {
            return Ok(AppendOutcome::Duplicate);
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await
            .map_err(|e| ledger_io_error("open ledger for append".into(), e))?;
        file.write_all(line.as_bytes())
            .await
            .map_err(|e| ledger_io_error("append to ledger".into(), e))?;
        file.write_all(b"\n")
            .await
            .map_err(|e| ledger_io_error("append to ledger".into(), e))?;
        self.lock().insert(key);
        Ok(AppendOutcome::Stored)
    }

    async fn read(
        &self,
        run_id: &str,
        after: Option<u64>,
        limit: usize,
    ) -> Result<Vec<LedgerRecord>> {
        // Reads re-walk the file: the ledger's consistency story is "the
        // file is the truth, the index is a write-side dedupe cache", and a
        // read path that served only cached records would hide records
        // appended before this process opened the file.
        let from = after.map_or(0, |seq| seq.saturating_add(1));
        let contents = match tokio::fs::read_to_string(&self.path).await {
            Ok(contents) => contents,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(ledger_io_error("read ledger".into(), e)),
        };
        let mut records = Vec::new();
        for line in contents.lines().skip(1) {
            let Ok(record) = serde_json::from_str::<LedgerRecord>(line) else {
                continue;
            };
            if record.run_id == run_id && record.seq >= from {
                records.push(record);
            }
        }
        records.sort_by_key(|record| record.seq);
        records.truncate(limit);
        Ok(records)
    }

    async fn cursor(&self, run_id: &str) -> Result<Option<u64>> {
        Ok(self
            .lock()
            .iter()
            .filter(|(run, _)| run == run_id)
            .map(|(_, seq)| *seq)
            .max())
    }
}

/// The mirror's resumable position in one run's journal: the next sequence
/// number to mirror. Serde-able so the caller can persist it anywhere; losing
/// it costs a re-scan (dedupe makes re-mirroring safe), never correctness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MirrorCursor {
    /// The run being mirrored.
    pub run_id: String,

    /// The next journal sequence number to mirror.
    pub next_seq: u64,
}

impl MirrorCursor {
    /// A cursor at the start of `run_id`'s journal.
    pub fn start(run_id: impl Into<String>) -> Self {
        Self {
            run_id: run_id.into(),
            next_seq: 0,
        }
    }
}

/// What one mirror pass did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirrorReport {
    /// The run mirrored.
    pub run_id: String,

    /// Records that landed.
    pub mirrored: u64,

    /// Records the ledger already held (dedupe hits — expected whenever a
    /// pass overlaps a previous one).
    pub duplicates: u64,

    /// Records the waterfall suppressed.
    pub suppressed: u64,

    /// Records that landed carrying at least one [`RedactionMark`].
    pub redacted: u64,

    /// The handoff cursor: where the next pass resumes.
    pub cursor: MirrorCursor,
}

/// The journal → ledger mirror: maps events to [`LedgerRecord`]s, runs them
/// through the redaction waterfall in declared order, and appends the
/// survivors.
#[derive(Clone)]
pub struct TelemetryMirror {
    ledger: Arc<dyn TelemetryLedger>,
    redactors: Vec<Arc<dyn Redactor>>,
}

impl TelemetryMirror {
    /// A mirror appending to `ledger` with an empty waterfall (records land
    /// exactly as journaled, payloads included).
    pub fn new(ledger: Arc<dyn TelemetryLedger>) -> Self {
        Self {
            ledger,
            redactors: Vec::new(),
        }
    }

    /// Append a redactor to the waterfall. Stages run in declaration order.
    pub fn with_redactor(mut self, redactor: Arc<dyn Redactor>) -> Self {
        self.redactors.push(redactor);
        self
    }

    /// Mirror the whole snapshot — [`TelemetryMirror::mirror_from`] from the
    /// journal's start. Idempotent: ledger-side dedupe absorbs whatever a
    /// previous pass already stored.
    pub async fn mirror(&self, snapshot: &JournalSnapshot) -> Result<MirrorReport> {
        self.mirror_from(snapshot, MirrorCursor::start(snapshot.run_id.clone()))
            .await
    }

    /// Mirror the snapshot's events from `cursor` onward, returning the
    /// report and the next handoff cursor. A cursor naming a different run
    /// is an error, not a silent restart — resuming the wrong journal would
    /// interleave two runs' evidence under one cursor.
    pub async fn mirror_from(
        &self,
        snapshot: &JournalSnapshot,
        cursor: MirrorCursor,
    ) -> Result<MirrorReport> {
        if cursor.run_id != snapshot.run_id {
            return Err(RustyError::InvalidUpdate(format!(
                "mirror cursor names run `{}` but the snapshot is run `{}`; \
                 a cursor only ever resumes its own journal",
                cursor.run_id, snapshot.run_id
            )));
        }
        let mut report = MirrorReport {
            run_id: snapshot.run_id.clone(),
            mirrored: 0,
            duplicates: 0,
            suppressed: 0,
            redacted: 0,
            cursor: cursor.clone(),
        };
        for event in &snapshot.events {
            if event.seq < cursor.next_seq {
                continue;
            }
            let mut record = LedgerRecord::from_event(event);
            if self.redactors.iter().any(|stage| stage.redact(&mut record)) {
                report.suppressed += 1;
            } else {
                if !record.redactions.is_empty() {
                    report.redacted += 1;
                }
                match self.ledger.append(record).await? {
                    AppendOutcome::Stored => report.mirrored += 1,
                    AppendOutcome::Duplicate => report.duplicates += 1,
                }
            }
            report.cursor.next_seq = event.seq + 1;
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::{Clock, EventDraft, Journal};
    use serde_json::json;

    fn journaled_snapshot() -> JournalSnapshot {
        let journal = Journal::new("run-1", "thread-1", Clock::System);
        let step = journal.record(EventDraft::new(RunEventKind::SuperStepStart, Effect::Pure));
        let input = journal.record(
            EventDraft::new(RunEventKind::NodeInput, Effect::Pure)
                .node("agent")
                .parent(step),
        );
        journal.record(
            EventDraft::new(RunEventKind::ModelCall, Effect::NonIdempotent)
                .node("agent")
                .parent(input)
                .input(json!({"messages": [{"role": "user", "content": "secret prompt"}]}))
                .output(json!({"message": "hi", "model": "mock", "usage": null}))
                .tokens(Usage {
                    prompt_tokens: 10,
                    completion_tokens: 4,
                    total_tokens: 14,
                    ..Usage::default()
                }),
        );
        journal.record(
            EventDraft::new(RunEventKind::ToolCall, Effect::NonIdempotent)
                .status(EventStatus::Error)
                .output(json!({"error": "boom"})),
        );
        journal.snapshot()
    }

    #[tokio::test]
    async fn mirror_maps_events_and_premaps_severity() {
        let ledger = Arc::new(InMemoryLedger::new());
        let mirror = TelemetryMirror::new(ledger.clone());
        let report = mirror.mirror(&journaled_snapshot()).await.unwrap();
        assert_eq!(report.mirrored, 4);
        assert_eq!(report.cursor.next_seq, 4);

        let records = ledger.read("run-1", None, 100).await.unwrap();
        assert_eq!(records.len(), 4);
        assert_eq!(records[0].severity, Severity::Debug); // super-step start
        assert_eq!(records[2].severity, Severity::Info); // model call
        assert_eq!(records[3].severity, Severity::Error); // failed tool call
        assert!(records.iter().all(|r| r.redactions.is_empty()));
    }

    #[tokio::test]
    async fn mirror_is_idempotent_and_the_cursor_resumes() {
        let ledger = Arc::new(InMemoryLedger::new());
        let mirror = TelemetryMirror::new(ledger.clone());
        let snapshot = journaled_snapshot();

        let first = mirror.mirror(&snapshot).await.unwrap();
        assert_eq!(first.mirrored, 4);
        // A full re-mirror is all duplicates — a lost cursor costs a
        // re-scan, never a double-count.
        let second = mirror.mirror(&snapshot).await.unwrap();
        assert_eq!(second.duplicates, 4);
        assert_eq!(second.mirrored, 0);
        // A resumed pass over the same snapshot mirrors nothing new.
        let resumed = mirror.mirror_from(&snapshot, first.cursor).await.unwrap();
        assert_eq!(resumed.mirrored, 0);
        assert_eq!(resumed.duplicates, 0);
        assert_eq!(ledger.cursor("run-1").await.unwrap(), Some(3));
    }

    #[tokio::test]
    async fn cursor_of_another_run_is_refused() {
        let ledger = Arc::new(InMemoryLedger::new());
        let mirror = TelemetryMirror::new(ledger);
        let err = mirror
            .mirror_from(&journaled_snapshot(), MirrorCursor::start("run-2"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("run-2"));
    }

    #[tokio::test]
    async fn waterfall_hashes_payloads_and_attests() {
        let ledger = Arc::new(InMemoryLedger::new());
        let mirror =
            TelemetryMirror::new(ledger.clone()).with_redactor(Arc::new(PayloadHasher));
        let report = mirror.mirror(&journaled_snapshot()).await.unwrap();
        assert_eq!(report.redacted, 2); // the model call (in+out) and the tool error

        let records = ledger.read("run-1", None, 100).await.unwrap();
        let model_call = &records[2];
        // The prompt bytes never landed: what landed is the content address.
        let Some(PayloadRef::Artifact(reference)) = &model_call.input else {
            panic!("expected a hashed input payload");
        };
        assert_eq!(reference.sha256.len(), 64);
        assert!(model_call
            .redactions
            .iter()
            .any(|mark| mark.field == "input" && mark.action == RedactionAction::Transformed));
        // The ledger never mentions what was removed — only that removal happened.
        let serialized = serde_json::to_string(&records).unwrap();
        assert!(!serialized.contains("secret prompt"));
    }

    #[tokio::test]
    async fn waterfall_suppresses_in_declared_order() {
        let ledger = Arc::new(InMemoryLedger::new());
        let mirror = TelemetryMirror::new(ledger.clone())
            .with_redactor(Arc::new(PayloadDrop))
            .with_redactor(Arc::new(SeverityFloor::new(Severity::Notice)));
        let report = mirror.mirror(&journaled_snapshot()).await.unwrap();
        // Only the errored tool call survives the floor; the drop stage ran
        // first, so even it lands without payloads.
        assert_eq!(report.suppressed, 3);
        assert_eq!(report.mirrored, 1);
        let records = ledger.read("run-1", None, 100).await.unwrap();
        assert_eq!(records.len(), 1);
        assert!(records[0].output.is_none());
        assert!(records[0]
            .redactions
            .iter()
            .any(|mark| mark.action == RedactionAction::Cleared));
    }

    #[tokio::test]
    async fn jsonl_ledger_round_trips_and_dedupes() {
        let dir = std::env::temp_dir().join(format!("rusty-telemetry-test-{}", uuid::Uuid::new_v4()));
        let path = dir.join("ledger.jsonl");
        let ledger = JsonlLedger::open(&path).await.unwrap();
        let mirror = TelemetryMirror::new(Arc::new(ledger.clone()));
        let report = mirror.mirror(&journaled_snapshot()).await.unwrap();
        assert_eq!(report.mirrored, 4);

        // A fresh process over the same file: dedupe index rebuilt, records
        // readable, re-mirror is all duplicates.
        let reopened = JsonlLedger::open(&path).await.unwrap();
        let records = reopened.read("run-1", None, 100).await.unwrap();
        assert_eq!(records.len(), 4);
        assert_eq!(reopened.cursor("run-1").await.unwrap(), Some(3));
        let mirror = TelemetryMirror::new(Arc::new(reopened));
        let again = mirror.mirror(&journaled_snapshot()).await.unwrap();
        assert_eq!(again.duplicates, 4);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn jsonl_ledger_refuses_a_newer_format_version() {
        let dir = std::env::temp_dir().join(format!("rusty-telemetry-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ledger.jsonl");
        std::fs::write(
            &path,
            "{\"format\": \"rusty-telemetry-ledger\", \"format_version\": 99}\n",
        )
        .unwrap();
        let err = JsonlLedger::open(&path).await.unwrap_err();
        let message = err.to_string();
        assert!(message.contains("99"), "names the found version: {message}");
        assert!(
            message.contains(&TELEMETRY_FORMAT_VERSION.to_string()),
            "names the supported version: {message}"
        );
        assert!(
            message.contains("upgrade the runtime"),
            "names the upgrade direction: {message}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn severity_ladder_orders_by_declaration() {
        assert!(Severity::Debug < Severity::Info);
        assert!(Severity::Notice < Severity::Warning);
        assert!(Severity::Warning < Severity::Error);
    }
}
