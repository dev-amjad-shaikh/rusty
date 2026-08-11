//! The run artifact plane's server surface (R0.12 Operations Plane,
//! waves 1 and 2): the `/artifacts` routes, the file layout the
//! JSON-file backend persists through, and the retention plane
//! (versions, previews, releases, the sweeper, and the deployment's
//! artifact evidence chain).
//!
//! # Wave 2: versions, previews, retention
//!
//! - **Version accumulation.** A commit under a taken name no longer
//!   answers `409`: [`append_artifact_version`] builds the new head
//!   record (the prior sequence plus the new entry — old records are
//!   never edited, so every version keeps serving by address), the
//!   commit journals into the producing run exactly as a fresh commit
//!   does, and the store's compare-and-swap
//!   ([`ServerStore::put_run_artifact_version`]) refuses a fork — a
//!   concurrent version commit moves the head and the loser retries.
//! - **Previews** (`GET /artifacts/{id}/preview`) derive on read through
//!   [`derive_preview`], never stored — a stored preview would be a
//!   second, divergent account of the same bytes. Kinds the
//!   dependency-free derivations cannot cover answer an honest `empty`.
//! - **Retention** is enforced by the sweeper ([`ArtifactRetention`])
//!   and the release act (`POST /artifacts/{id}/release`), both over one
//!   deployment evidence chain ([`ARTIFACTS_JOURNAL_RUN_ID`] — the
//!   `credential-broker` precedent). Every retention act journals there:
//!   releases, prune intentions (journaled *before* any byte moves), and
//!   typed misses. Deliberately never the producing run's journal — a
//!   signed receipt covers that chain, and a retention act must not
//!   rewrite witnessed evidence.
//! - **Receipt coverage** is verified with
//!   [`verify_receipt_prefix`](rusty_agent_runtime::receipt::verify_receipt_prefix):
//!   a receipt pins the addresses its covered events name however much
//!   the journal grew since the mint. Coverage the sweeper cannot verify
//!   (a missing journal, an unknown signer key, a failed verification)
//!   pins *everything* that pass — fail closed, never prune what a
//!   receipt may cover.
//! - **The typed miss** (`410 artifact_unavailable`) is journaled on
//!   first observation per (tenant, address), best-effort: the typed
//!   answer is the contract and stands either way (the broker's
//!   `journal_denial` precedent), but the evidence should not be lost
//!   without a trace. This is the exit clause's "exact replay fails
//!   closed": the byte read *is* the replay's byte source.
//!
//! # The file layout
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
//!    store's re-put is. Same bytes under a different name is a `409`:
//!    one object carries one logical name. A name already pointing at
//!    different bytes takes the version path (wave 2): the new head
//!    record joins the name's sequence and the name re-points.
//! 2. **The journal first.** One `ArtifactCommitted` event appends to
//!    the producing run's persisted journal (ownership and integrity
//!    checked on load), hard-fail: a commit that cannot journal its
//!    event does not persist the record. Nothing reaches the store the
//!    journal did not record first.
//! 3. **Bytes through the trait.** `ArtifactStore::put` dedupes by
//!    construction. If the record write then fails, the bytes sit
//!    orphaned — content-addressed, unlisted, eventually swept: a
//!    storage cost, never an evidence lie.
//! 4. **The record.** Insert-only on the tenant-scoped address — the
//!    fresh commit inserts; the version commit CAS-appends against the
//!    head it was built from, the name index re-pointing in the same
//!    mutation on both backends.
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

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path as AxumPath, Query};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use chrono::{DateTime, Utc};
use rusty_agent_runtime::artifact::{
    append_artifact_version, commit_artifact, derive_preview, ArtifactCommitment, ArtifactError,
    ArtifactLineage, ArtifactPrune, ArtifactRelease, ArtifactUnavailability, CommitDeclaration,
    MediaKind, PruneCause, RetentionPolicy, RunArtifact, UnavailabilitySurface,
};
use rusty_agent_runtime::broker::hex_decode;
use rusty_agent_runtime::journal::{Clock, EventDraft, FileArtifactStore, Journal};
use rusty_agent_runtime::receipt::{verify_receipt_prefix, PublicKey};
use rusty_agent_runtime::record::{
    sha256_hex, ArtifactRef, Effect, PayloadRef, RunEvent, RunEventKind,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::auth::TenantContext;
use crate::error::ApiError;
use crate::routes::{internal_err, AppState};
use crate::server_store::{RunArtifactVersionWrite, RunArtifactWrite, ServerStore, StoreResult};

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
    // The convergence pre-checks, before anything journals or is built:
    // an identical re-commit is the same fact and must not journal a
    // second event; same bytes under a different name is a conflict,
    // never a version. Advisory only: the store re-checks at the write.
    if let Some(existing) = state
        .server_store
        .get_run_artifact(tenant.tenant(), &declaration.reference.sha256)
        .await
        .map_err(internal_err)?
    {
        if existing.name == declaration.name {
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

    // Version accumulation (wave 2): a taken name grows the sequence —
    // the new head record is built from the head it will be committed
    // against, journals exactly as a fresh commit does, and lands through
    // the store's compare-and-swap, so two concurrent version commits
    // cannot fork the sequence: the loser reads `HeadMoved` and retries.
    if let Some(name) = declaration.name.clone() {
        if let Some(head) = state
            .server_store
            .get_run_artifact_by_name(tenant.tenant(), &name)
            .await
            .map_err(internal_err)?
        {
            let (record, commitment) =
                append_artifact_version(&head, declaration).map_err(|e| artifact_error(&e))?;
            let event_id =
                journal_commitment(state, tenant, &record.lineage.run_id, &commitment).await?;
            let stored = state
                .server_store
                .put_run_artifact_bytes(bytes)
                .await
                .map_err(internal_err)?;
            if stored.sha256 != record.artifact_id {
                return Err(ApiError::internal(format!(
                    "byte store minted address `{}` for bytes the commit declared as `{}` — \
                     the commit aborts; the orphaned bytes are unlisted and eventually swept",
                    stored.sha256, record.artifact_id
                )));
            }
            return match state
                .server_store
                .put_run_artifact_version(tenant.tenant(), &head.artifact_id, &record)
                .await
                .map_err(internal_err)?
            {
                RunArtifactVersionWrite::Versioned => Ok((
                    StatusCode::CREATED,
                    Json(json!({
                        "artifact_id": record.artifact_id,
                        "created": true,
                        "artifact": record,
                        "commitment": commitment,
                        "journal_event_id": event_id,
                    })),
                )),
                RunArtifactVersionWrite::HeadMoved(live) => Err(ApiError::conflict(format!(
                    "name `{name}`'s head moved to artifact `{live}` while this version \
                     committed — a concurrent commit won; re-read the name and retry against \
                     the new head (nothing was written)"
                ))),
                RunArtifactVersionWrite::NameUnknown => Err(ApiError::conflict(format!(
                    "name `{name}` stopped resolving while this version committed — re-read \
                     the name and retry (nothing was written)"
                ))),
            };
        }
    }

    // The fresh-commit path: build, journal, bytes, record — the same
    // fail-closed order, the store's insert-only write re-checking the
    // convergence the pre-checks advised.
    let (record, commitment) = commit_artifact(declaration).map_err(|e| artifact_error(&e))?;
    let event_id = journal_commitment(state, tenant, &record.lineage.run_id, &commitment).await?;
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

/// The fail-closed byte read every byte-backed surface shares (wave 2):
/// bytes that do not re-hash to their address are corruption (`422
/// artifact_corrupt`), never a served object; a live record whose bytes
/// are gone journals the typed miss (best-effort — the `410` is the
/// contract and stands either way) and answers `410
/// artifact_unavailable`, distinct from `404` because a retention audit
/// reads exactly that difference. `surface` records *which* read
/// observed the miss.
async fn read_bytes_fail_closed(
    state: &AppState,
    tenant: &TenantContext,
    record: &RunArtifact,
    surface: UnavailabilitySurface,
) -> Result<Vec<u8>, ApiError> {
    match state
        .server_store
        .get_run_artifact_bytes(&record.artifact_id)
        .await
    {
        Ok(bytes) => Ok(bytes),
        Err(e) => {
            if let Some(detail) = e.strip_prefix("artifact corrupt:") {
                Err(ApiError::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "artifact_corrupt",
                    format!(
                        "artifact `{}` failed its integrity check:{detail} — the stored \
                         bytes are corrupt; refusing to serve",
                        record.artifact_id
                    ),
                ))
            } else if let Some(detail) = e.strip_prefix("artifact unavailable:") {
                // Journal the miss before answering (best-effort — the
                // typed `410` is the contract and stands either way;
                // `journal_miss` converges on the first observation).
                state
                    .artifact_retention
                    .journal_miss(tenant.tenant(), record, surface)
                    .await;
                Err(ApiError::new(
                    StatusCode::GONE,
                    "artifact_unavailable",
                    format!(
                        "artifact `{}` is a live record whose bytes are not in the \
                         store:{detail}",
                        record.artifact_id
                    ),
                ))
            } else {
                Err(internal_err(e))
            }
        }
    }
}

/// `GET /artifacts/{artifact_id}/bytes` — the bytes behind a live
/// record, integrity-verified on read by the byte store's contract.
/// Fails closed (`422 artifact_corrupt` / `410 artifact_unavailable`),
/// and this read *is* an exact replay's byte source: the journaled,
/// typed miss is what a replay of the producing run fails closed on.
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
    let bytes =
        read_bytes_fail_closed(&state, &tenant, &record, UnavailabilitySurface::Bytes).await?;
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

// --------------------------------------------------------------------- //
// The retention plane (wave 2): the deployment evidence chain, the
// sweeper, and the release act
// --------------------------------------------------------------------- //

/// The deterministic run id of the deployment's artifact evidence chain:
/// the chained record of every retention act — releases, prune
/// intentions, and typed misses. Distinct from executor run ids (UUIDs)
/// by construction, deployment-wide (retention enforcement is a
/// deployment duty, the `list_all_connections` rule), and free of `/` so
/// the JSON-file layout keeps one file per journal — the
/// [`crate::broker::BROKER_JOURNAL_RUN_ID`] precedent, and the same
/// reason the acts journal here rather than on the producing run's
/// chain: a signed receipt covers that chain, and a retention act must
/// not rewrite witnessed evidence.
pub(crate) const ARTIFACTS_JOURNAL_RUN_ID: &str = "run-artifacts";

/// What the chain's events currently attest — the sweeper's and the
/// release act's read of the evidence.
#[derive(Debug, Default)]
struct ChainState {
    /// `(tenant, artifact_id)` → the event id of the first journaled
    /// release (releases converge: a repeat act reads the first).
    released: HashMap<(String, String), String>,
    /// Addresses with a journaled prune intention (a retried prune does
    /// not re-journal).
    prune_intents: HashSet<String>,
    /// `(tenant, artifact_id)` pairs whose miss is already journaled (a
    /// repeat read of the same hole does not re-journal).
    misses: HashSet<(String, String)>,
}

/// Parse the chain's events into its current state. An act payload that
/// will not parse is skipped with a warning — the chain's own integrity
/// is verified on load; a malformed payload is a bug's trace, and the
/// fail-closed side (an unjournaled release cannot converge, an
/// unjournaled intent re-journals) is the safe one.
fn parse_chain(events: &[RunEvent]) -> ChainState {
    let mut state = ChainState::default();
    for event in events {
        let value = match &event.output {
            Some(PayloadRef::Inline(value)) => value.clone(),
            _ => {
                tracing::warn!(event_id = %event.id, "artifact chain event's payload is not inline; skipped");
                continue;
            }
        };
        match event.kind {
            RunEventKind::ArtifactRetentionReleased => {
                match serde_json::from_value::<ArtifactRelease>(value) {
                    Ok(release) => {
                        state
                            .released
                            .entry((release.tenant, release.artifact_id))
                            .or_insert_with(|| event.id.clone());
                    }
                    Err(e) => {
                        tracing::warn!(event_id = %event.id, %e, "artifact chain carries an unparseable release event")
                    }
                }
            }
            RunEventKind::ArtifactPruned => match serde_json::from_value::<ArtifactPrune>(value) {
                Ok(prune) => {
                    state.prune_intents.insert(prune.artifact_id);
                }
                Err(e) => {
                    tracing::warn!(event_id = %event.id, %e, "artifact chain carries an unparseable prune event")
                }
            },
            RunEventKind::ArtifactUnavailable => {
                match serde_json::from_value::<ArtifactUnavailability>(value) {
                    Ok(miss) => {
                        state.misses.insert((miss.tenant, miss.artifact_id));
                    }
                    Err(e) => {
                        tracing::warn!(event_id = %event.id, %e, "artifact chain carries an unparseable miss event")
                    }
                }
            }
            _ => {}
        }
    }
    state
}

/// The receipt-coverage read: which addresses a verified signed receipt
/// still pins, and which receipts could not be verified this pass.
#[derive(Debug, Default)]
struct Coverage {
    /// Content addresses named by the covered events of at least one
    /// verified receipt.
    covered: HashSet<String>,
    /// Run ids whose coverage could not be verified (a missing journal,
    /// an unknown signer key, a failed prefix verification, or an
    /// unparseable covered commitment). Non-empty pins *everything* the
    /// pass evaluates — coverage the sweeper cannot check is coverage it
    /// must assume.
    unverifiable: Vec<String>,
}

/// One record's protection under the current evidence.
enum Protection {
    /// The record still protects its address.
    Protected,
    /// The record's retention has lapsed; the cause is part of the
    /// evidence a prune journals.
    Prunable(PruneCause),
}

/// Evaluate one record against the chain, the coverage, and the clock.
/// The release check comes first by design: the release is the *only*
/// path that prunes an address a live signed receipt covers or a
/// `pinned` policy holds — it is a governance act with a name on it,
/// never housekeeping.
fn evaluate_record(
    tenant: &str,
    record: &RunArtifact,
    chain: &ChainState,
    coverage: &Coverage,
    now: DateTime<Utc>,
) -> Protection {
    if chain
        .released
        .contains_key(&(tenant.to_owned(), record.artifact_id.clone()))
    {
        return Protection::Prunable(PruneCause::Released);
    }
    let pin_all = !coverage.unverifiable.is_empty();
    match record.retention {
        RetentionPolicy::Pinned => Protection::Protected,
        RetentionPolicy::Days { days } => {
            let deadline = record.created_at + chrono::Duration::days(i64::from(days));
            if deadline > now || pin_all || coverage.covered.contains(&record.artifact_id) {
                Protection::Protected
            } else {
                Protection::Prunable(PruneCause::Expired)
            }
        }
        RetentionPolicy::ReceiptBound => {
            if pin_all || coverage.covered.contains(&record.artifact_id) {
                Protection::Protected
            } else {
                Protection::Prunable(PruneCause::Unbound)
            }
        }
    }
}

/// Evaluate one address across *every* tenant's records naming it (byte
/// storage is global by content addressing, so pruning is honest only as
/// a cross-tenant decision): `Some(cause)` when every record is prunable
/// — the cause precedence is `released` over `expired` over `unbound`,
/// because the audit reads *which* rule fired — `None` when any record
/// still protects the bytes.
fn evaluate_address(
    records: &[(String, RunArtifact)],
    chain: &ChainState,
    coverage: &Coverage,
    now: DateTime<Utc>,
) -> Option<PruneCause> {
    let mut saw_released = false;
    let mut saw_expired = false;
    let mut saw_prunable = false;
    for (tenant, record) in records {
        match evaluate_record(tenant, record, chain, coverage, now) {
            Protection::Protected => return None,
            Protection::Prunable(cause) => {
                saw_prunable = true;
                saw_released |= cause == PruneCause::Released;
                saw_expired |= cause == PruneCause::Expired;
            }
        }
    }
    if !saw_prunable {
        return None;
    }
    Some(if saw_released {
        PruneCause::Released
    } else if saw_expired {
        PruneCause::Expired
    } else {
        PruneCause::Unbound
    })
}

/// One sweep pass's account — the answer of `POST /artifacts/sweep` and
/// the sweeper's own log line.
#[derive(Debug, Default, Clone, Serialize)]
pub(crate) struct ArtifactSweepReport {
    /// Distinct content addresses evaluated.
    pub scanned: usize,
    /// Artifact records evaluated (across every tenant).
    pub records: usize,
    /// Addresses whose bytes this pass deleted.
    pub pruned: usize,
    /// Addresses with a journaled intention whose bytes were already
    /// gone (a converged retry).
    pub already_gone: usize,
    /// Addresses at least one record still protects.
    pub protected: usize,
    /// Delete failures (retried next pass; the intention is already
    /// journaled, so the retry does not re-journal).
    pub failed: usize,
    /// Run ids whose receipt coverage could not be verified this pass —
    /// non-empty pinned every address the pass evaluated.
    pub unverifiable_receipts: Vec<String>,
}

/// The outcome of a release act (`POST /artifacts/{id}/release`).
#[derive(Debug, Clone)]
pub(crate) struct ReleaseOutcome {
    /// The released content address.
    pub artifact_id: String,
    /// `true` when the act converged on an already-journaled release
    /// (the repeat carries the first act's event id).
    pub converged: bool,
    /// Whether *this call* deleted the bytes (the release's prune tail;
    /// a protected cross-tenant record or a failed delete answers
    /// `false`, and the sweeper converges the rest).
    pub pruned: bool,
    /// The chain event id of the release (the first act's, when
    /// converged).
    pub journal_event_id: String,
}

/// The artifact retention plane: the sweeper and the release act over
/// the server store, with every retention act journaled onto the
/// deployment evidence chain. Mirrors the broker's shape — one store
/// handle, one append lock — and its concurrency rule: the chain lock
/// serializes load → append → persist, and the store's own CASes
/// arbitrate the records.
pub(crate) struct ArtifactRetention {
    store: Arc<dyn ServerStore>,
    /// Serializes chain appends (the broker's `journal_lock`
    /// discipline): two acts must never load → append → persist
    /// concurrently, or the loser's persist clobbers the winner's event.
    chain_lock: Mutex<()>,
}

impl ArtifactRetention {
    pub(crate) fn new(store: Arc<dyn ServerStore>) -> Self {
        Self {
            store,
            chain_lock: Mutex::new(()),
        }
    }

    /// Load the evidence chain, integrity re-verified (`None` when no
    /// act has journaled yet). A tampered chain fails the load — the
    /// caller's act refuses rather than fork a witnessed chain.
    async fn load_chain(&self) -> StoreResult<Option<Journal>> {
        match self.store.get_journal(ARTIFACTS_JOURNAL_RUN_ID).await? {
            Some(snapshot) => Ok(Some(
                Journal::from_snapshot(snapshot, Clock::System).map_err(|e| {
                    format!("the artifact evidence chain failed its integrity check: {e}")
                })?,
            )),
            None => Ok(None),
        }
    }

    /// The chain's current parsed state.
    async fn chain_state(&self) -> StoreResult<ChainState> {
        let chain = self.load_chain().await?;
        let events = chain
            .as_ref()
            .map(|journal| journal.events())
            .unwrap_or_default();
        Ok(parse_chain(&events))
    }

    /// Append one act to the chain, hard-fail: callers treat an `Err`
    /// here as "the act did not happen", and the prune path journals its
    /// intention before any byte moves, so a failed append means no
    /// deletion.
    async fn journal_act(&self, draft: EventDraft) -> StoreResult<String> {
        let _guard = self.chain_lock.lock().await;
        let journal = match self.load_chain().await? {
            Some(journal) => journal,
            None => Journal::new(
                ARTIFACTS_JOURNAL_RUN_ID,
                ARTIFACTS_JOURNAL_RUN_ID,
                Clock::System,
            ),
        };
        let event_id = journal.record(draft);
        self.store.put_journal(&journal.snapshot()).await?;
        Ok(event_id)
    }

    /// Journal a typed miss (best-effort: the `410` is the contract and
    /// stands either way — the broker's `journal_denial` precedent — but
    /// the evidence should not be lost without a trace). Converged: only
    /// the first observed miss per (tenant, address) appends; later
    /// reads of the same hole re-answer `410` without re-journaling.
    pub(crate) async fn journal_miss(
        &self,
        tenant: &str,
        record: &RunArtifact,
        surface: UnavailabilitySurface,
    ) {
        let attempt = async {
            let _guard = self.chain_lock.lock().await;
            let chain = self.load_chain().await?;
            let events = chain
                .as_ref()
                .map(|journal| journal.events())
                .unwrap_or_default();
            let state = parse_chain(&events);
            if state
                .misses
                .contains(&(tenant.to_owned(), record.artifact_id.clone()))
            {
                return Ok::<_, String>(());
            }
            let miss = ArtifactUnavailability {
                artifact_id: record.artifact_id.clone(),
                tenant: tenant.to_owned(),
                name: record.name.clone(),
                surface,
                observed_at: Utc::now(),
            };
            let output = serde_json::to_value(&miss).map_err(|e| e.to_string())?;
            let journal = chain.unwrap_or_else(|| {
                Journal::new(
                    ARTIFACTS_JOURNAL_RUN_ID,
                    ARTIFACTS_JOURNAL_RUN_ID,
                    Clock::System,
                )
            });
            journal.record(
                EventDraft::new(RunEventKind::ArtifactUnavailable, Effect::Pure).output(output),
            );
            self.store.put_journal(&journal.snapshot()).await?;
            Ok(())
        }
        .await;
        if let Err(e) = attempt {
            tracing::warn!(artifact_id = %record.artifact_id, %e, "artifact miss could not be journaled");
        }
    }

    /// The coverage scan: every minted receipt, prefix-verified against
    /// its run's current journal, contributes the addresses its covered
    /// `ArtifactCommitted` events name. Anything that cannot be verified
    /// — a missing journal, a signer key absent from the history, a
    /// failed [`verify_receipt_prefix`], or a covered commitment that
    /// will not parse — lands in `unverifiable`, which pins *every*
    /// address the pass evaluates: coverage the sweeper cannot check is
    /// coverage it must assume.
    async fn receipt_coverage(&self) -> StoreResult<Coverage> {
        let mut coverage = Coverage::default();
        let receipts = self.store.list_all_run_receipts().await?;
        if receipts.is_empty() {
            return Ok(coverage);
        }
        let keys = self.store.list_receipt_keys().await?;
        for (run_id, receipt) in receipts {
            let verified = async {
                let snapshot = self
                    .store
                    .get_journal(&run_id)
                    .await?
                    .ok_or_else(|| format!("run `{run_id}` has no persisted journal"))?;
                let key = keys
                    .iter()
                    .find(|key| key.key_id == receipt.signer)
                    .ok_or_else(|| {
                        format!("signer key `{}` is not in the key history", receipt.signer)
                    })?;
                let public = PublicKey::from_hex(&key.public_key).map_err(|e| e.to_string())?;
                verify_receipt_prefix(&snapshot, &receipt, &public)
                    .map_err(|e| format!("prefix verification failed: {e}"))?;
                Ok::<_, String>(snapshot)
            }
            .await;
            let snapshot = match verified {
                Ok(snapshot) => snapshot,
                Err(e) => {
                    tracing::warn!(run_id = %run_id, %e, "receipt coverage unverifiable; pinning every address this pass");
                    coverage.unverifiable.push(run_id);
                    continue;
                }
            };
            let covered_count = (receipt.journal_head.events as usize).min(snapshot.events.len());
            for event in &snapshot.events[..covered_count] {
                if !matches!(event.kind, RunEventKind::ArtifactCommitted) {
                    continue;
                }
                let value = match &event.output {
                    Some(PayloadRef::Inline(value)) => Some(value.clone()),
                    Some(PayloadRef::Artifact(reference)) => {
                        snapshot.artifacts.get(&reference.sha256).cloned()
                    }
                    None => None,
                };
                let parsed = value
                    .and_then(|value| serde_json::from_value::<ArtifactCommitment>(value).ok());
                match parsed {
                    Some(commitment) => {
                        coverage.covered.insert(commitment.artifact_id);
                    }
                    None => {
                        tracing::warn!(run_id = %run_id, event_id = %event.id, "covered commitment will not parse; pinning every address this pass");
                        coverage.unverifiable.push(run_id.clone());
                        break;
                    }
                }
            }
        }
        Ok(coverage)
    }

    /// Journal the prune intention unless the chain already carries one
    /// for this address — the journaled-before-delete rule: a crash
    /// after this point leaves the intention auditable and the bytes
    /// recoverable.
    async fn journal_prune_intent(
        &self,
        artifact_id: &str,
        name: Option<String>,
        cause: PruneCause,
        chain: &ChainState,
        now: DateTime<Utc>,
    ) -> StoreResult<()> {
        if chain.prune_intents.contains(artifact_id) {
            return Ok(());
        }
        let prune = ArtifactPrune {
            artifact_id: artifact_id.to_owned(),
            name,
            cause,
            swept_at: now,
        };
        let output = serde_json::to_value(&prune).map_err(|e| e.to_string())?;
        self.journal_act(
            EventDraft::new(RunEventKind::ArtifactPruned, Effect::Pure).output(output),
        )
        .await?;
        Ok(())
    }

    /// The release's prune tail: re-evaluate the address across *every*
    /// tenant's records with the release included, and prune when no
    /// record protects it anymore. Best-effort — the release stands
    /// journaled either way; a failed intention or delete logs and
    /// leaves the convergence to the sweeper. Returns whether this call
    /// deleted the bytes.
    async fn prune_if_unprotected(&self, artifact_id: &str) -> StoreResult<bool> {
        let chain = self.chain_state().await?;
        let coverage = self.receipt_coverage().await?;
        let records: Vec<(String, RunArtifact)> = self
            .store
            .list_all_run_artifacts()
            .await?
            .into_iter()
            .filter(|(_, record)| record.artifact_id == artifact_id)
            .collect();
        if records.is_empty() {
            return Ok(false);
        }
        let now = Utc::now();
        let Some(cause) = evaluate_address(&records, &chain, &coverage, now) else {
            return Ok(false);
        };
        let name = records.iter().find_map(|(_, record)| record.name.clone());
        if let Err(e) = self
            .journal_prune_intent(artifact_id, name, cause, &chain, now)
            .await
        {
            tracing::warn!(artifact_id = %artifact_id, %e, "release-time prune could not journal its intention; the sweeper will converge it");
            return Ok(false);
        }
        match self.store.delete_run_artifact_bytes(artifact_id).await {
            Ok(deleted) => Ok(deleted),
            Err(e) => {
                tracing::warn!(artifact_id = %artifact_id, %e, "release-time prune could not delete the bytes; the sweeper will converge it");
                Ok(false)
            }
        }
    }

    /// The retention-release act: journal `ArtifactRetentionReleased`
    /// (converging on an existing release for the same tenant and
    /// address — a repeat act carries the first act's event id), then
    /// run the prune tail. `None` when the address does not resolve in
    /// this tenant (unknown and cross-tenant are indistinguishable).
    /// Journaling hard-fails: a release that cannot join the chain did
    /// not happen.
    pub(crate) async fn release(
        &self,
        tenant: &str,
        artifact_id: &str,
        released_by: String,
        reason: Option<String>,
    ) -> StoreResult<Option<ReleaseOutcome>> {
        let Some(record) = self.store.get_run_artifact(tenant, artifact_id).await? else {
            return Ok(None);
        };
        let (event_id, converged) = {
            let _guard = self.chain_lock.lock().await;
            let chain = self.load_chain().await?;
            let events = chain
                .as_ref()
                .map(|journal| journal.events())
                .unwrap_or_default();
            let state = parse_chain(&events);
            if let Some(existing) = state
                .released
                .get(&(tenant.to_owned(), artifact_id.to_owned()))
            {
                (existing.clone(), true)
            } else {
                let release = ArtifactRelease {
                    artifact_id: artifact_id.to_owned(),
                    tenant: tenant.to_owned(),
                    name: record.name.clone(),
                    released_by,
                    reason,
                    released_at: Utc::now(),
                };
                let output = serde_json::to_value(&release).map_err(|e| e.to_string())?;
                let journal = chain.unwrap_or_else(|| {
                    Journal::new(
                        ARTIFACTS_JOURNAL_RUN_ID,
                        ARTIFACTS_JOURNAL_RUN_ID,
                        Clock::System,
                    )
                });
                let event_id = journal.record(
                    EventDraft::new(RunEventKind::ArtifactRetentionReleased, Effect::Pure)
                        .output(output),
                );
                self.store.put_journal(&journal.snapshot()).await?;
                (event_id, false)
            }
        };
        let pruned = self.prune_if_unprotected(artifact_id).await?;
        Ok(Some(ReleaseOutcome {
            artifact_id: artifact_id.to_owned(),
            converged,
            pruned,
            journal_event_id: event_id,
        }))
    }

    /// One sweep pass: evaluate every artifact address in the deployment
    /// against the chain, the receipt coverage, and the clock; prune the
    /// addresses no record protects. Deterministic order (addresses
    /// sorted) so a pass is reproducible; the intention journals before
    /// any byte moves, and a pass that cannot journal hard-fails — a
    /// deletion nothing recorded is the one outcome the plane refuses.
    /// A failed delete counts and retries next pass (the journaled
    /// intention is not re-journaled). Bytes are only ever candidates
    /// when at least one run-artifact record names them — memory and
    /// journal-spill blobs are never this plane's to sweep.
    pub(crate) async fn sweep_once(&self, now: DateTime<Utc>) -> StoreResult<ArtifactSweepReport> {
        let mut report = ArtifactSweepReport::default();
        let chain = self.chain_state().await?;
        let coverage = self.receipt_coverage().await?;
        report.unverifiable_receipts = coverage.unverifiable.clone();
        if !coverage.unverifiable.is_empty() {
            tracing::warn!(runs = ?coverage.unverifiable, "artifact sweep: unverifiable receipt coverage pins every address this pass");
        }
        let mut by_address: HashMap<String, Vec<(String, RunArtifact)>> = HashMap::new();
        for (tenant, record) in self.store.list_all_run_artifacts().await? {
            report.records += 1;
            by_address
                .entry(record.artifact_id.clone())
                .or_default()
                .push((tenant, record));
        }
        let mut addresses: Vec<&String> = by_address.keys().collect();
        addresses.sort();
        for address in addresses {
            let records = &by_address[address];
            report.scanned += 1;
            let Some(cause) = evaluate_address(records, &chain, &coverage, now) else {
                report.protected += 1;
                continue;
            };
            let name = records.iter().find_map(|(_, record)| record.name.clone());
            // Hard-fail the pass when the intention cannot journal —
            // before any byte moves.
            self.journal_prune_intent(address, name, cause, &chain, now)
                .await?;
            match self.store.delete_run_artifact_bytes(address).await {
                Ok(true) => report.pruned += 1,
                Ok(false) => report.already_gone += 1,
                Err(e) => {
                    tracing::warn!(artifact_id = %address, %e, "artifact sweep could not delete the bytes; will retry next pass");
                    report.failed += 1;
                }
            }
        }
        Ok(report)
    }
}

/// Spawn the background retention sweeper: [`ArtifactRetention::sweep_once`]
/// on `interval`, with the broker sweeper's drain semantics — a pass in
/// flight when shutdown starts completes, and a delayed tick delays
/// rather than bursting. Off is safe: `POST /artifacts/sweep` and the
/// release act's prune tail run the same evaluation, so a stopped
/// sweeper degrades to operator-triggered passes, never to unprotected
/// pruning.
pub(crate) fn spawn_sweeper(
    retention: Arc<ArtifactRetention>,
    interval: Duration,
    shutdown: tokio_util::sync::CancellationToken,
) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                _ = shutdown.cancelled() => {
                    tracing::info!("artifact retention sweeper shutting down; due prunes run on the next process or the operator-triggered pass");
                    break;
                }
            }
            match retention.sweep_once(Utc::now()).await {
                Ok(report) if report.pruned > 0 || report.failed > 0 || report.already_gone > 0 => {
                    tracing::info!(?report, "artifact retention sweep completed");
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(%error, "artifact retention sweep failed; will retry");
                }
            }
        }
    });
}

// --------------------------------------------------------------------- //
// The wave-2 routes
// --------------------------------------------------------------------- //

/// `GET /artifacts/{artifact_id}/preview` — the derived preview of a
/// live record's bytes (`{artifact_id, preview}`). Never stored: the
/// answer is a pure function of the bytes, so it can never drift from
/// them; kinds the dependency-free derivations cannot cover answer an
/// honest `empty` with its reason. The same fail-closed mapping as the
/// byte read: `404` unknown/cross-tenant, `410 artifact_unavailable`
/// (journaled on the `preview` surface) for a live record whose bytes
/// are gone, `422 artifact_corrupt` for bytes that fail integrity.
pub(crate) async fn get_run_artifact_preview(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath(artifact_id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let record = state
        .server_store
        .get_run_artifact(tenant.tenant(), &artifact_id)
        .await
        .map_err(internal_err)?
        .ok_or_else(|| ApiError::not_found(format!("artifact `{artifact_id}` not found")))?;
    let bytes =
        read_bytes_fail_closed(&state, &tenant, &record, UnavailabilitySurface::Preview).await?;
    let preview = derive_preview(record.media_kind, &bytes);
    Ok(Json(json!({
        "artifact_id": record.artifact_id,
        "preview": preview,
    })))
}

#[derive(Debug, Deserialize)]
pub(crate) struct ReleaseArtifactPayload {
    /// The operator identity releasing the pin (`human:{id}`, the
    /// registry commit discipline) — a release shortens evidence
    /// retention, so the act carries a name.
    released_by: String,
    /// The operator's stated reason, when given.
    #[serde(default)]
    reason: Option<String>,
}

/// `POST /artifacts/{artifact_id}/release` — the retention-release act:
/// journal the release onto the deployment evidence chain, then prune
/// the address when no record (in any tenant) protects it anymore →
/// `200 {artifact_id, released, converged, pruned, journal_event_id}`;
/// `404` unknown/cross-tenant; `422` when the operator identity is
/// empty. This is the *only* path that prunes an address a live signed
/// receipt covers or a `pinned` policy holds.
pub(crate) async fn release_run_artifact(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath(artifact_id): AxumPath<String>,
    Json(payload): Json<ReleaseArtifactPayload>,
) -> Result<Json<Value>, ApiError> {
    if payload.released_by.trim().is_empty() {
        return Err(ApiError::unprocessable(
            "`released_by` is empty — a release is a governance act with a name on it".to_owned(),
        ));
    }
    let outcome = state
        .artifact_retention
        .release(
            tenant.tenant(),
            &artifact_id,
            payload.released_by,
            payload.reason,
        )
        .await
        .map_err(internal_err)?
        .ok_or_else(|| ApiError::not_found(format!("artifact `{artifact_id}` not found")))?;
    Ok(Json(json!({
        "artifact_id": outcome.artifact_id,
        "released": true,
        "converged": outcome.converged,
        "pruned": outcome.pruned,
        "journal_event_id": outcome.journal_event_id,
    })))
}

/// `POST /artifacts/sweep` — the operator-triggered sweep pass: the same
/// evaluation the interval sweeper runs, on demand → `200` with the
/// pass's [`ArtifactSweepReport`]. The pass is deterministic for a given
/// store state (addresses evaluate in sorted order) so tests and audits
/// reproduce it exactly; the interval sweeper is off by default
/// ([`ServerConfig::artifact_sweep_interval`](crate::ServerConfig::artifact_sweep_interval)),
/// and this route is the always-available enforcement path.
pub(crate) async fn sweep_artifacts(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Extension(_tenant): Extension<TenantContext>,
) -> Result<Json<ArtifactSweepReport>, ApiError> {
    let report = state
        .artifact_retention
        .sweep_once(Utc::now())
        .await
        .map_err(internal_err)?;
    Ok(Json(report))
}

/// `GET /artifacts/journal` — the deployment's artifact evidence chain
/// (`{run_id, events, complete: false}`): every release, prune
/// intention, and typed miss, in chain order. Integrity re-verified on
/// read — a tampered chain is refused (`422`), never served.
/// `complete: false` is the honest bound: the chain is deployment-wide
/// and grows with every act, so this is a full read of *now*, not a
/// paginated history contract.
pub(crate) async fn get_artifacts_journal(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Extension(_tenant): Extension<TenantContext>,
) -> Result<Json<Value>, ApiError> {
    let Some(snapshot) = state
        .server_store
        .get_journal(ARTIFACTS_JOURNAL_RUN_ID)
        .await
        .map_err(internal_err)?
    else {
        return Ok(Json(json!({
            "run_id": ARTIFACTS_JOURNAL_RUN_ID,
            "events": [],
            "complete": false,
        })));
    };
    let journal = Journal::from_snapshot(snapshot, Clock::System).map_err(|e| {
        ApiError::unprocessable(format!(
            "the artifact evidence chain failed its integrity check: {e} — refusing to serve \
             a chain that does not verify"
        ))
    })?;
    Ok(Json(json!({
        "run_id": ARTIFACTS_JOURNAL_RUN_ID,
        "events": journal.events(),
        "complete": false,
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
