//! The run artifact plane's server surface (R0.12 Operations Plane,
//! wave 1): the `/artifacts` routes, and the file layout the JSON-file
//! backend persists through.
//!
//! The layout under `{store_path}/artifacts/` (`artifacts` is a reserved
//! layout name, see [`crate::RESERVED_NAMES`]):
//!
//! - `records/` holds one JSON file per
//!   [`RunArtifact`](rusty_agent_runtime::artifact::RunArtifact), named
//!   by tenant-scoped content address — path-keyed tenancy, the
//!   `learn/candidates` rule: the address is path-safe hex, the record
//!   body carries the bare address, and the tenant prefix comes from
//!   where the file lives. Records are written once at commit, never
//!   edited; a later version (Wave 2) is a new address and a new file.
//! - `names/` holds the name index: one hash-named pointer file per
//!   tenant-scoped artifact name, the file body an envelope carrying the
//!   true key — the registry-artifact rule, because artifact names carry
//!   the same punctuation surface keys do. Wave 1 writes a name once;
//!   version accumulation (Wave 2) re-points the envelope at the new
//!   head.
//! - `blobs/` is the byte store — a [`FileArtifactStore`](rusty_agent_runtime::journal::FileArtifactStore)
//!   sibling of the records, so the recursive record loader never picks
//!   up a blob (the `memory_artifacts` discipline). Bytes are *not*
//!   tenant-namespaced: content addressing makes byte storage global,
//!   and the tenant-scoped metadata layer is the only path that lists
//!   or resolves, so a shared address grants no cross-tenant read path.
//!   On Postgres the same bytes land in core's `rusty_artifacts` table
//!   through the same [`ArtifactStore`](rusty_agent_runtime::journal::ArtifactStore)
//!   trait — the plane stores bytes through the trait and nothing else.
//!
//! # The commit discipline
//!
//! Both commit paths — the SDK-declared output (`/artifacts/commits`)
//! and the journaled spill (`/artifacts/spills`) — run the same
//! sequence, and the order is the design's fail-closed rule:
//!
//! 1. **Convergence pre-check.** An identical re-commit (same tenant,
//!    same address, same name) answers `200` without journaling again or
//!    rewriting anything — the commit is idempotent the way the byte
//!    store's re-put is. Same bytes under a different name, or a name
//!    already pointing at different bytes, is a `409`: one object
//!    carries one logical name, and version accumulation is Wave 2.
//! 2. **The journal first.** One `ArtifactCommitted` event appends to
//!    the producing run's persisted journal (ownership and integrity
//!    checked on load), hard-fail: a commit that cannot journal its
//!    event does not persist the record. Nothing reaches the store the
//!    journal did not record first.
//! 3. **Bytes through the trait.** `ArtifactStore::put` dedupes by
//!    construction. If the record write then fails, the bytes sit
//!    orphaned — content-addressed, unlisted, eventually swept: a
//!    storage cost, never an evidence lie.
//! 4. **The record.** Insert-only on the tenant-scoped address.
//!
//! The read side fails closed in the other direction: a read that fails
//! integrity is refused as corruption (`422 artifact_corrupt`), never
//! silently served; a live record whose bytes are gone answers the
//! typed miss `410 artifact_unavailable` — distinct from `404`, because
//! the difference between "no such artifact" and "the record exists,
//! the bytes do not" is exactly what a retention audit needs.
//!
//! One caveat inherits from the memory journaler's documented gap:
//! appending to a *live* run's persisted journal races the executor's
//! own checkpoint-boundary flushes (whole-snapshot persistence), so a
//! commit mid-run can have its event overwritten by the run it belongs
//! to. The record and the bytes stand either way; the in-executor
//! commit seam — the runtime journaling `ArtifactCommitted` at
//! production time — is the follow-up that closes it, exactly the seam
//! the receipt journaler already names.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Path as AxumPath, Query};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use chrono::{DateTime, Utc};
use rusty_agent_runtime::artifact::{
    commit_artifact, ArtifactCommitment, ArtifactError, ArtifactLineage, CommitDeclaration,
    MediaKind, RetentionPolicy, RunArtifact,
};
use rusty_agent_runtime::broker::hex_decode;
use rusty_agent_runtime::journal::{Clock, EventDraft, FileArtifactStore, Journal};
use rusty_agent_runtime::record::{sha256_hex, ArtifactRef, Effect, PayloadRef, RunEventKind};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::TenantContext;
use crate::error::ApiError;
use crate::routes::{internal_err, AppState};
use crate::server_store::RunArtifactWrite;

// --------------------------------------------------------------------- //
// File layout (the JSON-file backend)
// --------------------------------------------------------------------- //

/// The records directory under the store root
/// (`{store_path}/artifacts/records`).
pub(crate) fn records_dir(root: &Path) -> PathBuf {
    root.join("artifacts").join("records")
}

/// The name-index directory (`{store_path}/artifacts/names`).
pub(crate) fn names_dir(root: &Path) -> PathBuf {
    root.join("artifacts").join("names")
}

/// The byte store spilled run-artifact blobs live in (a sibling of the
/// records dir, so the recursive record loader never picks up a blob —
/// the `memory_artifacts` discipline).
pub(crate) fn blob_store(root: &Path) -> FileArtifactStore {
    FileArtifactStore::new(root.join("artifacts").join("blobs"))
}

/// Persist one record atomically (temp file + rename) under
/// `records_dir`, named by `scoped_id` — the durability discipline every
/// file record in the server shares. The id may carry a `{tenant}/`
/// prefix, so the parent directory is created, not just the flat dir.
pub(crate) async fn persist_record(
    root: &Path,
    scoped_id: &str,
    record: &RunArtifact,
) -> io::Result<()> {
    let dir = records_dir(root);
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
/// subdirectories hold that tenant's records), mirroring the learn
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
/// (`{tenant}/{address}` for named tenants, the bare address for the
/// default tenant) — the learn loader's key rule.
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

/// Load all records under `records_dir`, keyed by their path-derived
/// scoped content address. Unparseable files are skipped with a warning
/// (one bad record must not take the namespace down at boot), and a
/// record whose `artifact_id` does not match the address its filename
/// claims is skipped the same way — the registry loader's fail-closed
/// rule: the plane must never serve a record under an address it was
/// not written under.
pub(crate) fn load_records(root: &Path) -> HashMap<String, RunArtifact> {
    let dir = records_dir(root);
    let mut out = HashMap::new();
    let mut files = Vec::new();
    collect_json_files(&dir, &mut files);
    for path in files {
        let Some(scoped_id) = path_scoped_id(&dir, &path) else {
            continue;
        };
        let parsed = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<RunArtifact>(&raw).ok());
        let bare = scoped_id.rsplit('/').next().unwrap_or(&scoped_id);
        match parsed {
            Some(record) if record.artifact_id == bare => {
                out.insert(scoped_id, record);
            }
            _ => {
                tracing::warn!(path = %path.display(), "skipping unreadable artifact record file")
            }
        }
    }
    out
}

/// The name-index file's body: the address the name currently points
/// at, plus the scoped name it was written under. The key travels in
/// the body because the filename is the key's hash — artifact names are
/// not path-safe, and a one-way filename needs the true key recorded
/// somewhere (the registry-artifact envelope's rule, verbatim).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct NameFile {
    /// The tenant-scoped artifact name (`{tenant}/weekly-report` for
    /// named tenants).
    key: String,
    /// The content address the name resolves to.
    artifact_id: String,
}

/// The name file's name for a scoped artifact name: its SHA-256 hex —
/// hashing (rather than escaping) keeps every name inside one
/// fixed-shape, collision-checked namespace; the envelope key check on
/// load is what catches a collision or a forged name.
fn name_file_name(scoped_name: &str) -> String {
    sha256_hex(scoped_name.as_bytes())
}

/// Persist one name pointer atomically (temp file + rename), named by
/// the scoped name's hash.
pub(crate) async fn persist_name(
    root: &Path,
    scoped_name: &str,
    artifact_id: &str,
) -> io::Result<()> {
    let dir = names_dir(root);
    tokio::fs::create_dir_all(&dir).await?;
    let file = NameFile {
        key: scoped_name.to_string(),
        artifact_id: artifact_id.to_string(),
    };
    let bytes = serde_json::to_vec_pretty(&file)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let name = name_file_name(scoped_name);
    let tmp = dir.join(format!("{name}.tmp"));
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, dir.join(format!("{name}.json"))).await
}

/// Load the name index under `names_dir`: scoped name → content
/// address. A file whose envelope key does not hash back to its
/// filename is corrupt (or a collision) and is skipped with a warning,
/// same as an unparseable file — the registry loader's fail-closed
/// rule.
pub(crate) fn load_names(root: &Path) -> HashMap<String, String> {
    let dir = names_dir(root);
    let mut out = HashMap::new();
    let mut files = Vec::new();
    collect_json_files(&dir, &mut files);
    for path in files {
        let parsed = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<NameFile>(&raw).ok());
        let matches_name = parsed.as_ref().is_some_and(|file| {
            path.file_stem().and_then(|s| s.to_str()) == Some(&*name_file_name(&file.key))
        });
        match (parsed, matches_name) {
            (Some(file), true) => {
                out.insert(file.key, file.artifact_id);
            }
            _ => {
                tracing::warn!(path = %path.display(), "skipping unreadable artifact name file")
            }
        }
    }
    out
}

// --------------------------------------------------------------------- //
// The commit paths
// --------------------------------------------------------------------- //

/// The lineage a commit declares. The effect id is the caller's
/// declaration — `derive_effect_id` is deterministic, so the producing
/// node computes its own id and the server records it; re-derivation at
/// audit (from the journaled scope, kind, input hash, and key) is where
/// a fabricated claim fails, not at commit.
#[derive(Debug, Deserialize)]
pub(crate) struct LineagePayload {
    /// The run that produced the artifact. The commitment journals into
    /// this run's journal, so the run must exist and belong to the
    /// calling tenant.
    run_id: String,
    /// The producing effect's deterministic id (64 lowercase hex).
    effect_id: String,
    /// The journal event id whose output carried the reference.
    event_id: String,
}

/// Map an artifact refusal to its HTTP status: naming-rule and
/// address-rule violations are `422` (the request is well-formed; the
/// contract refuses it).
fn artifact_error(error: &ArtifactError) -> ApiError {
    ApiError::unprocessable(error.to_string())
}

/// `true` for 64 lowercase hex characters — the shape both content
/// addresses and effect ids mint. A declared effect id outside the rule
/// is a malformed lineage, refused `422` before anything journals.
fn is_lower_hex_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

fn parse_lineage(payload: LineagePayload) -> Result<ArtifactLineage, ApiError> {
    if !is_lower_hex_digest(&payload.effect_id) {
        return Err(ApiError::unprocessable(format!(
            "effect id `{}` is not 64 lowercase hex characters — lineage anchors to a \
             derived effect id; anything else cannot re-derive at audit",
            payload.effect_id
        )));
    }
    Ok(ArtifactLineage {
        run_id: payload.run_id,
        // Validated above as a bare lowercase digest; `EffectId` is a
        // transparent newtype over exactly this string.
        effect_id: serde_json::from_value(Value::String(payload.effect_id))
            .map_err(|e| ApiError::unprocessable(format!("effect id does not parse: {e}")))?,
        event_id: payload.event_id,
    })
}

/// Journal the commitment into the producing run's persisted journal —
/// hard-fail, the candidate-lifecycle discipline: ownership proof first
/// (the journal's thread must resolve in this tenant — journaling into
/// another tenant's run would leak evidence across the isolation
/// boundary), integrity re-check on load, append, persist. `404` when
/// the run does not resolve in this tenant; `422` when the journal
/// fails its integrity check; `500` on a persistence failure. Returns
/// the journaled event's id.
async fn journal_commitment(
    state: &AppState,
    tenant: &TenantContext,
    run_id: &str,
    commitment: &ArtifactCommitment,
) -> Result<String, ApiError> {
    let snapshot = state
        .server_store
        .get_journal(run_id)
        .await
        .map_err(internal_err)?
        .ok_or_else(|| {
            ApiError::not_found(format!(
                "run `{run_id}` has no persisted journal — a commit journals into the \
                 producing run; an unknown run commits nothing"
            ))
        })?;
    let internal_thread_id = tenant.scope(&snapshot.thread_id);
    let owned = state
        .server_store
        .get_thread(&internal_thread_id)
        .await
        .map_err(internal_err)?
        .is_some();
    if !owned {
        return Err(ApiError::not_found(format!(
            "run `{run_id}` does not resolve in this tenant"
        )));
    }
    let journal = Journal::from_snapshot(snapshot, Clock::System).map_err(|e| {
        ApiError::unprocessable(format!(
            "run `{run_id}`'s journal failed its integrity check: {e} — the commitment \
             cannot join a chain that does not verify"
        ))
    })?;
    let parent = journal.events().last().map(|event| event.id.clone());
    let output = serde_json::to_value(commitment)
        .map_err(|e| ApiError::internal(format!("serialize commitment: {e}")))?;
    let mut draft = EventDraft::new(RunEventKind::ArtifactCommitted, Effect::Pure).output(output);
    if let Some(parent) = parent {
        draft = draft.parent(parent);
    }
    let event_id = journal.record(draft);
    state
        .server_store
        .put_journal(&journal.snapshot())
        .await
        .map_err(internal_err)?;
    Ok(event_id)
}

/// The shared tail of both commit paths: convergence pre-check, journal,
/// bytes, record — in the fail-closed order the module docs state.
async fn commit_shared(
    state: &AppState,
    tenant: &TenantContext,
    bytes: &[u8],
    declaration: CommitDeclaration,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let (record, commitment) = commit_artifact(declaration).map_err(|e| artifact_error(&e))?;

    // The convergence pre-check, before anything journals: an identical
    // re-commit is the same fact and must not journal a second event;
    // same bytes under a different name — or a name already pointing at
    // different bytes — is a conflict, not a version (Wave 2 grows the
    // sequence). Advisory only: the store re-checks at the write.
    if let Some(existing) = state
        .server_store
        .get_run_artifact(tenant.tenant(), &record.artifact_id)
        .await
        .map_err(internal_err)?
    {
        if existing.name == record.name {
            return Ok((
                StatusCode::OK,
                Json(json!({
                    "artifact_id": existing.artifact_id,
                    "created": false,
                    "artifact": existing,
                })),
            ));
        }
        return Err(ApiError::conflict(format!(
            "artifact `{}` is already committed under name `{:?}` — one object carries one \
             logical name; the bytes are shared, the metadata is not",
            existing.artifact_id, existing.name
        )));
    }
    if let Some(name) = &record.name {
        if let Some(other) = state
            .server_store
            .get_run_artifact_by_name(tenant.tenant(), name)
            .await
            .map_err(internal_err)?
        {
            return Err(ApiError::conflict(format!(
                "name `{name}` already points at artifact `{}` — version accumulation is \
                 Wave 2; a distinct output needs a distinct name until then",
                other.artifact_id
            )));
        }
    }

    // The journal first: a commit that cannot journal its event does
    // not persist the record.
    let event_id = journal_commitment(state, tenant, &record.lineage.run_id, &commitment).await?;

    // Bytes through the trait, then the record. The store's `put`
    // re-mints the address from the bytes it stored — a disagreement
    // with the declared address means the byte path is lying, and the
    // commit aborts before the record exists. Orphaned bytes on a
    // record-write failure are the documented storage cost.
    let stored = state
        .server_store
        .put_run_artifact_bytes(bytes)
        .await
        .map_err(internal_err)?;
    if stored.sha256 != record.artifact_id {
        return Err(ApiError::internal(format!(
            "byte store minted address `{}` for bytes the commit declared as `{}` — the \
             commit aborts; the orphaned bytes are unlisted and eventually swept",
            stored.sha256, record.artifact_id
        )));
    }
    match state
        .server_store
        .put_run_artifact(tenant.tenant(), &record)
        .await
        .map_err(internal_err)?
    {
        RunArtifactWrite::Created => Ok((
            StatusCode::CREATED,
            Json(json!({
                "artifact_id": record.artifact_id,
                "created": true,
                "artifact": record,
                "commitment": commitment,
                "journal_event_id": event_id,
            })),
        )),
        RunArtifactWrite::Converged(existing) => Ok((
            StatusCode::OK,
            Json(json!({
                "artifact_id": existing.artifact_id,
                "created": false,
                "artifact": existing,
            })),
        )),
        RunArtifactWrite::NameTaken(other) => Err(ApiError::conflict(format!(
            "name `{:?}` already points at artifact `{other}`",
            record.name
        ))),
        RunArtifactWrite::Conflict(existing) => Err(ApiError::conflict(format!(
            "artifact `{}` is already committed under name `{:?}`",
            existing.artifact_id, existing.name
        ))),
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct CommitArtifactPayload {
    /// The output bytes, hex-encoded (the broker envelope's codec — the
    /// dependency-free byte-on-JSON convention this codebase already
    /// chose; arbitrary bytes cannot ride a JSON string raw).
    bytes_hex: String,
    /// The logical name versions accumulate under, when named.
    #[serde(default)]
    name: Option<String>,
    /// The media class.
    media_kind: MediaKind,
    /// The producer-declared media type string, when asserted.
    #[serde(default)]
    media_type: Option<String>,
    /// The declared retention (default `receipt_bound`).
    #[serde(default)]
    retention: Option<RetentionPolicy>,
    /// The producing run, effect, and journal event.
    lineage: LineagePayload,
    /// When the commit happened (default: now). Explicit for callers
    /// reproducing a recorded history; the instant is metadata, never
    /// identity.
    #[serde(default)]
    committed_at: Option<DateTime<Utc>>,
}

/// `POST /artifacts/commits` — the SDK-declared commit path: a node (or
/// an operator replaying one) declares an output's bytes and lineage →
/// `201 {artifact_id, created, artifact, commitment, journal_event_id}`;
/// `200` + `created: false` on an identical re-commit; `404` when the
/// producing run does not resolve in this tenant; `409` on a name or
/// identity conflict; `422` on a malformed name, address, or lineage.
pub(crate) async fn commit_run_artifact(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(payload): Json<CommitArtifactPayload>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let bytes = hex_decode(&payload.bytes_hex).ok_or_else(|| {
        ApiError::bad_request(
            "`bytes_hex` is not valid hex — bytes ride the wire hex-encoded (the broker \
             envelope's codec)"
                .to_owned(),
        )
    })?;
    let reference = ArtifactRef {
        sha256: sha256_hex(&bytes),
        bytes: bytes.len() as u64,
    };
    let declaration = CommitDeclaration {
        reference,
        name: payload.name,
        media_kind: payload.media_kind,
        media_type: payload.media_type,
        lineage: parse_lineage(payload.lineage)?,
        retention: payload.retention.unwrap_or_default(),
        committed_at: payload.committed_at.unwrap_or_else(Utc::now),
    };
    commit_shared(&state, &tenant, &bytes, declaration).await
}

#[derive(Debug, Deserialize)]
pub(crate) struct CommitSpillPayload {
    /// The run whose journal holds the spilled output.
    run_id: String,
    /// The event id whose output is the journaled
    /// [`PayloadRef::Artifact`] to commit.
    event_id: String,
    /// The producing effect's deterministic id (64 lowercase hex).
    effect_id: String,
    /// The logical name versions accumulate under, when named.
    #[serde(default)]
    name: Option<String>,
    /// The media class.
    media_kind: MediaKind,
    /// The producer-declared media type string, when asserted.
    #[serde(default)]
    media_type: Option<String>,
    /// The declared retention (default `receipt_bound`).
    #[serde(default)]
    retention: Option<RetentionPolicy>,
    /// When the commit happened (default: now).
    #[serde(default)]
    committed_at: Option<DateTime<Utc>>,
}

/// `POST /artifacts/spills` — the journaled-spill commit path: commit
/// the bytes behind a journaled [`PayloadRef::Artifact`] whose
/// producing node opted in. The bytes come from the run's own journal
/// (the canonical serialization the artifact map holds), so what the
/// plane commits is exactly what the run produced — never a second
/// upload that could drift from the evidence. Statuses are the declared
/// path's, plus `422` when the event does not exist, its output is not
/// an artifact reference, or its bytes are absent from the snapshot.
pub(crate) async fn commit_spilled_artifact(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(payload): Json<CommitSpillPayload>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let snapshot = state
        .server_store
        .get_journal(&payload.run_id)
        .await
        .map_err(internal_err)?
        .ok_or_else(|| {
            ApiError::not_found(format!(
                "run `{}` has no persisted journal — a spill commit reads the producing \
                 run's journal; an unknown run spills nothing",
                payload.run_id
            ))
        })?;
    let internal_thread_id = tenant.scope(&snapshot.thread_id);
    let owned = state
        .server_store
        .get_thread(&internal_thread_id)
        .await
        .map_err(internal_err)?
        .is_some();
    if !owned {
        return Err(ApiError::not_found(format!(
            "run `{}` does not resolve in this tenant",
            payload.run_id
        )));
    }
    let event = snapshot
        .events
        .iter()
        .find(|event| event.id == payload.event_id)
        .ok_or_else(|| {
            ApiError::unprocessable(format!(
                "run `{}` has no event `{}` — a spill commit names the event whose output \
                 carried the reference",
                payload.run_id, payload.event_id
            ))
        })?;
    let reference = match event.output.as_ref() {
        Some(PayloadRef::Artifact(reference)) => reference.clone(),
        _ => {
            return Err(ApiError::unprocessable(format!(
                "event `{}`'s output is not an artifact reference — only a spilled \
                 (content-addressed) output commits through this path",
                payload.event_id
            )))
        }
    };
    let value = snapshot.artifacts.get(&reference.sha256).ok_or_else(|| {
        ApiError::unprocessable(format!(
            "event `{}` references artifact `{}`, whose bytes are absent from the journal \
             snapshot — a truncated snapshot cannot source a commit",
            payload.event_id, reference.sha256
        ))
    })?;
    // The canonical serialization the artifact map was keyed on (the
    // `snapshot_externalized` rule): the committed bytes re-hash to the
    // reference by construction.
    let bytes = serde_json::to_vec(value)
        .map_err(|e| ApiError::internal(format!("serialize spilled payload: {e}")))?;
    let declaration = CommitDeclaration {
        reference,
        name: payload.name,
        media_kind: payload.media_kind,
        media_type: payload.media_type,
        lineage: parse_lineage(LineagePayload {
            run_id: payload.run_id,
            effect_id: payload.effect_id,
            event_id: payload.event_id,
        })?,
        retention: payload.retention.unwrap_or_default(),
        committed_at: payload.committed_at.unwrap_or_else(Utc::now),
    };
    commit_shared(&state, &tenant, &bytes, declaration).await
}

// --------------------------------------------------------------------- //
// The read surface
// --------------------------------------------------------------------- //

/// `GET /artifacts/{artifact_id}` — fetch one record (`404`
/// unknown/cross-tenant — the two are indistinguishable by design).
pub(crate) async fn get_run_artifact(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath(artifact_id): AxumPath<String>,
) -> Result<Json<RunArtifact>, ApiError> {
    state
        .server_store
        .get_run_artifact(tenant.tenant(), &artifact_id)
        .await
        .map_err(internal_err)?
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("artifact `{artifact_id}` not found")))
}

/// `GET /artifacts/{artifact_id}/bytes` — the bytes behind a live
/// record, integrity-verified on read by the byte store's contract.
/// Fails closed: bytes that do not re-hash to their address are
/// corruption (`422 artifact_corrupt`), never a served object; a live
/// record whose bytes are gone answers the typed miss (`410
/// artifact_unavailable`) — distinct from `404`, because a retention
/// audit reads exactly that difference.
pub(crate) async fn get_run_artifact_bytes(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath(artifact_id): AxumPath<String>,
) -> Result<Response, ApiError> {
    let record = state
        .server_store
        .get_run_artifact(tenant.tenant(), &artifact_id)
        .await
        .map_err(internal_err)?
        .ok_or_else(|| ApiError::not_found(format!("artifact `{artifact_id}` not found")))?;
    let bytes = state
        .server_store
        .get_run_artifact_bytes(&record.artifact_id)
        .await
        .map_err(|e| {
            if let Some(detail) = e.strip_prefix("artifact corrupt:") {
                ApiError::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "artifact_corrupt",
                    format!(
                        "artifact `{}` failed its integrity check:{detail} — the stored \
                         bytes are corrupt; refusing to serve",
                        record.artifact_id
                    ),
                )
            } else if let Some(detail) = e.strip_prefix("artifact unavailable:") {
                ApiError::new(
                    StatusCode::GONE,
                    "artifact_unavailable",
                    format!(
                        "artifact `{}` is a live record whose bytes are not in the \
                         store:{detail}",
                        record.artifact_id
                    ),
                )
            } else {
                internal_err(e)
            }
        })?;
    let content_type = record
        .media_type
        .clone()
        .unwrap_or_else(|| "application/octet-stream".to_owned());
    Ok(([(axum::http::header::CONTENT_TYPE, content_type)], bytes).into_response())
}

#[derive(Debug, Deserialize)]
pub(crate) struct ListArtifactsQuery {
    /// Restrict to one logical name.
    name: Option<String>,
    /// Restrict to one media kind (`file`, `image`, `audio`, `data`).
    media_kind: Option<String>,
    /// Restrict to artifacts one run produced (the lineage join).
    run_id: Option<String>,
}

/// `GET /artifacts?name=&media_kind=&run_id=` — the tenant's artifacts,
/// optionally filtered, sorted by content address for a deterministic
/// listing.
pub(crate) async fn list_run_artifacts(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Query(query): Query<ListArtifactsQuery>,
) -> Result<Json<Value>, ApiError> {
    let media_kind = query
        .media_kind
        .as_deref()
        .map(|kind| {
            serde_json::from_value::<MediaKind>(Value::String(kind.to_owned())).map_err(|_| {
                ApiError::bad_request(format!(
                    "unknown media kind `{kind}` — expected one of `file`, `image`, \
                     `audio`, `data`"
                ))
            })
        })
        .transpose()?;
    let mut records = state
        .server_store
        .list_run_artifacts(tenant.tenant())
        .await
        .map_err(internal_err)?;
    if let Some(name) = &query.name {
        records.retain(|record| record.name.as_deref() == Some(name.as_str()));
    }
    if let Some(kind) = media_kind {
        records.retain(|record| record.media_kind == kind);
    }
    if let Some(run_id) = &query.run_id {
        records.retain(|record| record.lineage.run_id == *run_id);
    }
    records.sort_by(|a, b| a.artifact_id.cmp(&b.artifact_id));
    Ok(Json(json!({ "artifacts": records })))
}

/// `GET /artifacts/names/{name}` — the record a logical name currently
/// resolves to (`404` unknown/cross-tenant).
pub(crate) async fn get_run_artifact_named(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<RunArtifact>, ApiError> {
    state
        .server_store
        .get_run_artifact_by_name(tenant.tenant(), &name)
        .await
        .map_err(internal_err)?
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("artifact name `{name}` not found")))
}

/// `GET /artifacts/names/{name}/versions` — the name's version
/// sequence, oldest first. Wave 1 commits write the base entry; the
/// route's shape is the Wave-2 one so accumulation lands without a
/// route change.
pub(crate) async fn list_run_artifact_versions(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let record = state
        .server_store
        .get_run_artifact_by_name(tenant.tenant(), &name)
        .await
        .map_err(internal_err)?
        .ok_or_else(|| ApiError::not_found(format!("artifact name `{name}` not found")))?;
    Ok(Json(json!({
        "name": name,
        "current": record.artifact_id,
        "versions": record.versions,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusty_agent_runtime::artifact::ArtifactVersion;

    fn ts(millis: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp_millis(millis).unwrap()
    }

    fn record(address: &str, name: Option<&str>) -> RunArtifact {
        RunArtifact {
            artifact_id: address.to_owned(),
            name: name.map(str::to_owned),
            media_kind: MediaKind::File,
            media_type: None,
            lineage: ArtifactLineage {
                run_id: "run-1".into(),
                effect_id: serde_json::from_value(Value::String("e".repeat(64))).unwrap(),
                event_id: "run-1:3".into(),
            },
            versions: name
                .map(|_| ArtifactVersion {
                    sha256: address.to_owned(),
                    bytes: 12,
                    committed_at: ts(1_760_000_000_000),
                })
                .into_iter()
                .collect(),
            retention: RetentionPolicy::default(),
            created_at: ts(1_760_000_000_000),
        }
    }

    #[tokio::test]
    async fn records_and_names_round_trip_through_the_layout() {
        let root =
            std::env::temp_dir().join(format!("rusty-artifacts-test-{}", uuid::Uuid::new_v4()));
        let named = record(&"a".repeat(64), Some("weekly-report"));
        let unnamed = record(&"b".repeat(64), None);
        persist_record(&root, &format!("acme/{}", named.artifact_id), &named)
            .await
            .unwrap();
        persist_record(&root, &unnamed.artifact_id, &unnamed)
            .await
            .unwrap();
        persist_name(&root, "acme/weekly-report", &named.artifact_id)
            .await
            .unwrap();

        let records = load_records(&root);
        assert_eq!(records.len(), 2);
        assert_eq!(records[&format!("acme/{}", named.artifact_id)], named);
        assert_eq!(records[&unnamed.artifact_id], unnamed);

        let names = load_names(&root);
        assert_eq!(names["acme/weekly-report"], named.artifact_id);

        // A record whose body disagrees with its filename's address is
        // skipped, not served — the fail-closed load rule.
        persist_record(&root, &format!("acme/{}", "c".repeat(64)), &named)
            .await
            .unwrap();
        assert_eq!(load_records(&root).len(), 2);
        let _ = std::fs::remove_dir_all(root);
    }
}
