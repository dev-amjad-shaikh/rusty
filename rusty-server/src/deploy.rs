//! Deployment persistence (R0.12 Operations Plane, wave 3): the file
//! layout behind the revision / environment / pointer / env-secret store
//! backends, and the environment-secret custody — master keys and the
//! envelope cryptography — the broker's construction verbatim.
//!
//! Two layout names join the reserved set (see [`crate::RESERVED_NAMES`]):
//!
//! - `{store_path}/deployments/` holds the control plane's records:
//!   `revisions/` keeps one JSON file per
//!   [`DeploymentRevision`], named by tenant-scoped content address
//!   (path-keyed tenancy, the `learn/candidates` rule — the address is
//!   path-safe hex, the tenant prefix comes from where the file lives);
//!   `environments/` keeps one JSON file per [`Environment`] record,
//!   same rule (environment names are validated tags — no `/`, no `@`);
//!   `pointers/` keeps one hash-named envelope file per
//!   [`DeploymentPointer`], the `learn/versions` discipline verbatim
//!   (surface keys carry `:` and tenant prefixes, so the filename is the
//!   key's SHA-256 and the body carries the true key). Revisions and
//!   environments are written once and never edited — a changed
//!   declaration is a new record; pointers are rewritten on every move,
//!   so the temp-write-then-rename discipline is what makes a crash
//!   mid-move safe.
//! - `{store_path}/env-secrets/` holds one hash-named envelope file per
//!   environment secret: the metadata record plus the sealed value,
//!   ciphertext only. Plaintext enters the store on neither backend,
//!   ever.
//!
//! ## Key custody
//!
//! The environment-secret master key lives **outside the store
//! abstraction**, exactly as the broker's does
//! (`{store_path}/keys/env-secret-master.{key_id}.secret`, hex, `0600`
//! from the first byte, written once), so the Postgres backend cannot
//! hold what a database must not leak. A distinct key family from the
//! broker's (`esk-` beside `bmk-`): one deployment master key per custody
//! domain keeps the broker's shipping key custody untouched, and an
//! env-secret key rotation never implies a broker re-wrap (or the
//! inverse). Key ids are random, never a hash of the key.
//!
//! ## Cryptography
//!
//! The broker's envelope construction, verbatim: each secret's value is
//! encrypted under a per-secret data key (32 random bytes, minted at
//! set), data keys are wrapped by the master key, both seals are
//! XChaCha20-Poly1305, and the scoped secret id (`{name}@{environment}`)
//! is the associated data — a ciphertext transplanted between scopes
//! fails its tag. Rotation re-seals under a *fresh* data key: the old
//! envelope is replaced whole, so a retired value's data key retires
//! with it. The functions here duplicate the broker's private
//! `seal_new`/`open` rather than generalizing them — the broker's
//! custody path ships untouched, and the construction's authoritative
//! statement stays in `broker.rs`'s module docs.
//!
//! ## The HTTP surface and the run-admission seam
//!
//! The module's second half is the `/deployments` route layer and the
//! `deployment` run-binding: one deployment evidence chain
//! ([`DEPLOYMENT_JOURNAL_RUN_ID`]) every registration, declaration,
//! promotion, rollback, and secret act appends to (hard-fail; denials
//! best-effort), pointer moves committed with their journaled transition
//! in the store's one transaction, and admission binding the revision
//! the environment's pointer serves — journaled into the run's own
//! journal ahead of its events (the `ConfigResolved` precedent, lifted
//! from configuration to deployments).

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use chrono::Utc;
use rusty_agent_runtime::broker::{
    hex_decode, hex_encode, SealedCredential, SEALED_FORMAT_VERSION,
};
use rusty_agent_runtime::deploy::{
    deployment_admission, deployment_surface, pin_set_digest, scoped_secret_name,
    validate_secret_name, DeployError, DeploymentPointer, DeploymentResolved, DeploymentRevision,
    EnvSecretAct, EnvSecretDenial, EnvSecretRecord, EnvSecretRevocation, Environment,
    EnvironmentDeclaration, GateDeclaration, RegistryPin, RevisionContent, RevisionId,
    RevisionPromotion, RevisionRegistration, RevisionRollback, StoredEnvSecret,
};
use rusty_agent_runtime::journal::{Clock, EventDraft, Journal};
use rusty_agent_runtime::learn::{EnvironmentTag, SurfaceKey};
use rusty_agent_runtime::memory::ProvenanceAuthor;
use rusty_agent_runtime::record::{sha256_hex, Effect, PayloadRef, RunEvent, RunEventKind};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::auth::TenantContext;
use crate::error::ApiError;
use crate::routes::{internal_err, AppState};
use crate::server_store::{DeploymentTransition, ServerStore, StoreResult};

use chacha20poly1305::aead::{Aead, KeyInit, OsRng};
use chacha20poly1305::XChaCha20Poly1305;

// --------------------------------------------------------------------- //
// The JSON-file layout
// --------------------------------------------------------------------- //

/// The revision directory under the store root
/// (`{store_path}/deployments/revisions`).
pub(crate) fn revisions_dir(root: &Path) -> PathBuf {
    root.join("deployments").join("revisions")
}

/// The environment-record directory under the store root
/// (`{store_path}/deployments/environments`).
pub(crate) fn environments_dir(root: &Path) -> PathBuf {
    root.join("deployments").join("environments")
}

/// The deployment-pointer directory under the store root
/// (`{store_path}/deployments/pointers`).
pub(crate) fn pointers_dir(root: &Path) -> PathBuf {
    root.join("deployments").join("pointers")
}

/// The env-secret directory under the store root
/// (`{store_path}/env-secrets`).
pub(crate) fn env_secrets_dir(root: &Path) -> PathBuf {
    root.join("env-secrets")
}

/// Recursively collect `*.json` files under `root` (tenant
/// subdirectories hold that tenant's records) — the candidate loader's
/// rule, kept local so this module's layout is self-describing.
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

/// The path-derived scoped key of a record file under `dir`
/// (`{tenant}/{id}` for named tenants, the bare id for the default
/// tenant) — the memory loader's key rule: the record body carries the
/// bare content address, so the key must come from where the file lives.
fn path_scoped_key(dir: &Path, path: &Path) -> Option<String> {
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

/// Persist one JSON record atomically (temp file + rename) under `dir`,
/// named by `scoped_key` — the durability discipline every file record
/// in the server shares (the `learn::persist_candidate` pattern). The
/// key may carry a `{tenant}/` prefix, so the parent directory is
/// created, not just the flat dir.
async fn persist_record(dir: &Path, scoped_key: &str, record: &impl Serialize) -> io::Result<()> {
    tokio::fs::create_dir_all(dir).await?;
    let bytes = serde_json::to_vec_pretty(record)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let path = dir.join(format!("{scoped_key}.json"));
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let tmp = dir.join(format!("{scoped_key}.tmp"));
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, path).await
}

/// Persist one revision atomically under `revisions_dir`. Written once,
/// never edited: a changed declaration is a new content address.
pub(crate) async fn persist_revision(
    root: &Path,
    scoped_id: &str,
    record: &DeploymentRevision,
) -> io::Result<()> {
    persist_record(&revisions_dir(root), scoped_id, record).await
}

/// Load all revisions under `revisions_dir`, keyed by their path-derived
/// scoped id. Files that fail to parse are skipped with a warning (the
/// corrupt-tolerance rule every loader here shares): one bad record must
/// not take the plane down at boot.
pub(crate) fn load_revisions(root: &Path) -> HashMap<String, DeploymentRevision> {
    let dir = revisions_dir(root);
    let mut out = HashMap::new();
    let mut files = Vec::new();
    collect_json_files(&dir, &mut files);
    for path in files {
        let scoped_id = path_scoped_key(&dir, &path);
        let parsed = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<DeploymentRevision>(&raw).ok());
        match (scoped_id, parsed) {
            (Some(id), Some(record)) => {
                out.insert(id, record);
            }
            _ => {
                tracing::warn!(path = %path.display(), "skipping unreadable revision file")
            }
        }
    }
    out
}

/// Persist one environment record atomically under `environments_dir`.
/// Written once, never edited: the declaration an audit reads is the one
/// in force when it was declared.
pub(crate) async fn persist_environment(
    root: &Path,
    scoped_name: &str,
    record: &Environment,
) -> io::Result<()> {
    persist_record(&environments_dir(root), scoped_name, record).await
}

/// Load all environment records under `environments_dir`, keyed by their
/// path-derived scoped name — the revision loader's rule.
pub(crate) fn load_environments(root: &Path) -> HashMap<String, Environment> {
    let dir = environments_dir(root);
    let mut out = HashMap::new();
    let mut files = Vec::new();
    collect_json_files(&dir, &mut files);
    for path in files {
        let scoped_name = path_scoped_key(&dir, &path);
        let parsed = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<Environment>(&raw).ok());
        match (scoped_name, parsed) {
            (Some(name), Some(record)) => {
                out.insert(name, record);
            }
            _ => {
                tracing::warn!(path = %path.display(), "skipping unreadable environment file")
            }
        }
    }
    out
}

/// The hash-named envelope for keys that are not path-safe (pointer
/// surfaces carry `:` and tenant prefixes; scoped secret names carry
/// `@`): the filename is the key's SHA-256 and the body carries the true
/// key — the version-pointer envelope's rule, verbatim. Loads read the
/// key back out of the envelope rather than reversing the hash, and a
/// file whose envelope key does not hash back to its filename is skipped
/// (corrupt, or a collision) — the fail-closed loader rule every
/// hashed-filename layout here shares.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct KeyedFile<T> {
    /// The tenant-scoped key the file was written under.
    key: String,
    /// The record itself.
    record: T,
}

fn keyed_file_name(scoped_key: &str) -> String {
    sha256_hex(scoped_key.as_bytes())
}

/// Persist one pointer or env-secret atomically, named by its scoped
/// key's hash. Pointers are the most-rewritten files in this layout —
/// the temp+rename discipline is what makes a crash mid-move safe.
async fn persist_keyed<T: Serialize>(dir: &Path, scoped_key: &str, record: &T) -> io::Result<()> {
    tokio::fs::create_dir_all(dir).await?;
    let file = KeyedFile {
        key: scoped_key.to_string(),
        record,
    };
    let bytes = serde_json::to_vec_pretty(&file)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let name = keyed_file_name(scoped_key);
    let tmp = dir.join(format!("{name}.tmp"));
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, dir.join(format!("{name}.json"))).await
}

/// Load every hash-named envelope under `dir`, keyed by the envelope's
/// carried key. The forged-name and corrupt files are skipped with a
/// warning, never served.
fn load_keyed<T: for<'de> Deserialize<'de>>(dir: &Path) -> HashMap<String, T> {
    let mut out = HashMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let parsed = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<KeyedFile<T>>(&raw).ok());
        let matches_name = parsed.as_ref().is_some_and(|file| {
            path.file_stem().and_then(|s| s.to_str()) == Some(&*keyed_file_name(&file.key))
        });
        match (parsed, matches_name) {
            (Some(file), true) => {
                out.insert(file.key, file.record);
            }
            _ => {
                tracing::warn!(path = %path.display(), "skipping unreadable keyed record file")
            }
        }
    }
    out
}

/// Persist one deployment pointer under `pointers_dir`.
pub(crate) async fn persist_pointer(
    root: &Path,
    scoped_surface: &str,
    pointer: &DeploymentPointer,
) -> io::Result<()> {
    persist_keyed(&pointers_dir(root), scoped_surface, pointer).await
}

/// Load all deployment pointers under `pointers_dir`.
pub(crate) fn load_pointers(root: &Path) -> HashMap<String, DeploymentPointer> {
    load_keyed(&pointers_dir(root))
}

/// Persist one stored env-secret under `env_secrets_dir` — the metadata
/// record plus the sealed envelope, ciphertext only.
pub(crate) async fn persist_env_secret(
    root: &Path,
    scoped_key: &str,
    record: &StoredEnvSecret,
) -> io::Result<()> {
    persist_keyed(&env_secrets_dir(root), scoped_key, record).await
}

/// Remove an env-secret's file; `false` when it was not there.
pub(crate) async fn delete_env_secret_file(root: &Path, scoped_key: &str) -> io::Result<bool> {
    let path = env_secrets_dir(root).join(format!("{}.json", keyed_file_name(scoped_key)));
    match tokio::fs::remove_file(&path).await {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Load all stored env-secrets under `env_secrets_dir`.
pub(crate) fn load_env_secrets(root: &Path) -> HashMap<String, StoredEnvSecret> {
    load_keyed(&env_secrets_dir(root))
}

// --------------------------------------------------------------------- //
// Master key custody (the broker's secret-file discipline)
// --------------------------------------------------------------------- //

/// The master key id prefix; the id is the prefix plus 16 lowercase hex
/// chars (random — a content address of symmetric material is a verifier
/// oracle).
const MASTER_KEY_ID_PREFIX: &str = "esk-";

fn master_secret_path(root: &Path, key_id: &str) -> PathBuf {
    crate::receipts::keys_dir(root).join(format!("env-secret-master.{key_id}.secret"))
}

/// Write a freshly generated master key with `0600` permissions from the
/// first byte (`create_new` + mode at creation — there is no window
/// where the file is looser). Written exactly once: rotation mints a new
/// key id, never overwrites.
async fn write_master_secret(root: &Path, key_id: &str, key: &[u8; 32]) -> io::Result<()> {
    let dir = crate::receipts::keys_dir(root);
    tokio::fs::create_dir_all(&dir).await?;
    let path = master_secret_path(root, key_id);
    #[cfg(unix)]
    {
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true).mode(0o600);
        use tokio::io::AsyncWriteExt as _;
        let mut file = options.open(&path).await?;
        file.write_all(hex_encode(key).as_bytes()).await?;
    }
    #[cfg(not(unix))]
    {
        tokio::fs::write(&path, hex_encode(key)).await?;
    }
    Ok(())
}

/// The held master keys: `(key_id, key)` pairs, oldest first — the last
/// entry seals, every entry opens (the rotation seam). Files that fail
/// to parse are skipped with a warning (the corrupt-tolerance rule), as
/// are names outside the `env-secret-master.{id}.secret` shape.
type MasterKeys = Vec<(String, [u8; 32])>;

fn load_master_secrets(root: &Path) -> MasterKeys {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(crate::receipts::keys_dir(root)) else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(rest) = name.strip_prefix("env-secret-master.") else {
            continue;
        };
        let Some(key_id) = rest.strip_suffix(".secret") else {
            continue;
        };
        let parsed = std::fs::read_to_string(entry.path())
            .ok()
            .and_then(|hex| hex_decode(hex.trim()))
            .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok());
        match parsed {
            Some(key) => out.push((key_id.to_owned(), key)),
            None => {
                tracing::warn!(path = %entry.path().display(), "skipping unreadable env-secret master key file")
            }
        }
    }
    out
}

/// The master keys this host holds, minting one on first use — the
/// broker's `masters()` rule: a deployment with no env-secret key yet
/// gets one at the first set, `0600` from the first byte.
pub(crate) async fn master_secrets(root: &Path) -> Result<MasterKeys, String> {
    let held = load_master_secrets(root);
    if !held.is_empty() {
        return Ok(held);
    }
    let key: [u8; 32] = draw_random();
    let key_id = format!(
        "{}{}",
        MASTER_KEY_ID_PREFIX,
        hex_encode(&draw_random::<8>())
    );
    write_master_secret(root, &key_id, &key)
        .await
        .map_err(|e| format!("mint env-secret master key: {e}"))?;
    Ok(vec![(key_id, key)])
}

/// The master key an envelope names (for opening) — `None` when this
/// host does not hold it (a store shared with a host that sealed under
/// another key: the envelope cannot open here, and resolution fails
/// closed).
pub(crate) fn master_for<'k>(keys: &'k MasterKeys, key_id: &str) -> Option<&'k [u8; 32]> {
    keys.iter().find(|(id, _)| id == key_id).map(|(_, key)| key)
}

// --------------------------------------------------------------------- //
// The envelope cryptography (the broker's construction, verbatim)
// --------------------------------------------------------------------- //

/// Draw `N` bytes of OS entropy through `AeadCore::generate_nonce`-grade
/// randomness — `OsRng`, the same `getrandom` source `uuid` uses.
fn draw_random<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    use chacha20poly1305::aead::rand_core::RngCore as _;
    OsRng.fill_bytes(&mut bytes);
    bytes
}

/// Seal a secret value under a freshly minted per-secret data key. The
/// scoped secret id (`{name}@{environment}`) authenticates both seals as
/// associated data: a wrapped key or ciphertext transplanted to another
/// scope's envelope fails its tag — scope is part of the secret's
/// identity, enforced by the cryptography, not by convention.
pub(crate) fn seal_env_secret(
    master_id: &str,
    master: &[u8; 32],
    scoped_id: &str,
    plaintext: &[u8],
) -> Result<SealedCredential, String> {
    let data_key = draw_random::<32>();
    let aad = scoped_id.as_bytes();
    let wrap_nonce_bytes = draw_random::<24>();
    let wrapped = XChaCha20Poly1305::new(master.into())
        .encrypt(
            chacha20poly1305::XNonce::from_slice(&wrap_nonce_bytes),
            chacha20poly1305::aead::Payload {
                msg: &data_key,
                aad,
            },
        )
        .map_err(|_| "wrap data key: AEAD failure".to_string())?;
    let nonce_bytes = draw_random::<24>();
    let ciphertext = XChaCha20Poly1305::new((&data_key).into())
        .encrypt(
            chacha20poly1305::XNonce::from_slice(&nonce_bytes),
            chacha20poly1305::aead::Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| "seal secret value: AEAD failure".to_string())?;
    Ok(SealedCredential {
        format_version: SEALED_FORMAT_VERSION,
        key_id: master_id.to_owned(),
        wrapped_data_key: hex_encode(&wrapped),
        wrap_nonce: hex_encode(&wrap_nonce_bytes),
        nonce: hex_encode(&nonce_bytes),
        ciphertext: hex_encode(&ciphertext),
        sealed_at: chrono::Utc::now(),
    })
}

/// Open an envelope: unwrap the data key, decrypt. Any failure — wrong
/// master key, tampered ciphertext, an envelope transplanted across
/// scopes, corrupt hex — is the same refusal: the envelope does not
/// open, and nothing about *why* leaks to the caller.
pub(crate) fn open_env_secret(
    master: &[u8; 32],
    scoped_id: &str,
    envelope: &SealedCredential,
) -> Result<Vec<u8>, String> {
    if envelope.format_version != SEALED_FORMAT_VERSION {
        return Err(format!(
            "envelope format version {} is not {SEALED_FORMAT_VERSION}",
            envelope.format_version
        ));
    }
    let aad = scoped_id.as_bytes();
    let wrapped = hex_decode(&envelope.wrapped_data_key)
        .ok_or_else(|| "corrupt envelope: wrapped key is not hex".to_string())?;
    let wrap_nonce = hex_decode(&envelope.wrap_nonce)
        .ok_or_else(|| "corrupt envelope: wrap nonce is not hex".to_string())?;
    if wrap_nonce.len() != 24 {
        return Err("corrupt envelope: wrap nonce is not 24 bytes".to_string());
    }
    let data_key = XChaCha20Poly1305::new(master.into())
        .decrypt(
            chacha20poly1305::XNonce::from_slice(&wrap_nonce),
            chacha20poly1305::aead::Payload { msg: &wrapped, aad },
        )
        .map_err(|_| "the wrapped data key failed its authentication tag".to_string())?;
    let data_key: [u8; 32] = <[u8; 32]>::try_from(data_key)
        .map_err(|_| "corrupt envelope: data key is not 32 bytes".to_string())?;
    let ciphertext = hex_decode(&envelope.ciphertext)
        .ok_or_else(|| "corrupt envelope: ciphertext is not hex".to_string())?;
    let nonce = hex_decode(&envelope.nonce)
        .ok_or_else(|| "corrupt envelope: nonce is not hex".to_string())?;
    if nonce.len() != 24 {
        return Err("corrupt envelope: nonce is not 24 bytes".to_string());
    }
    XChaCha20Poly1305::new((&data_key).into())
        .decrypt(
            chacha20poly1305::XNonce::from_slice(&nonce),
            chacha20poly1305::aead::Payload {
                msg: &ciphertext,
                aad,
            },
        )
        .map_err(|_| "the sealed secret failed its authentication tag".to_string())
}

// --------------------------------------------------------------------- //
// The deployment evidence chain and the control plane over it
// --------------------------------------------------------------------- //

/// The deployment evidence chain's run id: one journal per deployment
/// (the broker's `BROKER_JOURNAL_RUN_ID` and the artifacts chain's
/// `ARTIFACTS_JOURNAL_RUN_ID` precedent) — every revision registration,
/// environment declaration, promotion, rollback, and secret act appends
/// here, never to a serving run's receipt-covered journal.
pub(crate) const DEPLOYMENT_JOURNAL_RUN_ID: &str = "deployment-control";

/// The deployment control plane: one store handle, one append lock (the
/// `ArtifactRetention` shape, verbatim). The chain lock serializes
/// load → dedupe → append → transition, and the store's own CAS
/// arbitrates the pointer. Lock order is always chain lock, then —
/// inside the store's transition — the pointer lock.
pub(crate) struct DeploymentControl {
    store: Arc<dyn ServerStore>,
    /// Serializes chain appends (the broker's `journal_lock`
    /// discipline): two acts must never load → append → persist
    /// concurrently, or the loser's persist clobbers the winner's event.
    chain_lock: Mutex<()>,
}

/// One transition act the chain records: the journaled payload and the
/// dedupe identity for [`DeploymentControl::transition`].
pub(crate) enum DeploymentAct {
    /// A promotion: `active` moves to the revision.
    Promotion(RevisionPromotion),
    /// A rollback: `active` re-points to the previously serving revision.
    Rollback(RevisionRollback),
}

impl DeploymentAct {
    fn kind(&self) -> RunEventKind {
        match self {
            DeploymentAct::Promotion(_) => RunEventKind::RevisionPromoted,
            DeploymentAct::Rollback(_) => RunEventKind::RevisionRolledBack,
        }
    }

    fn output(&self) -> Result<Value, String> {
        match self {
            DeploymentAct::Promotion(act) => serde_json::to_value(act).map_err(|e| e.to_string()),
            DeploymentAct::Rollback(act) => serde_json::to_value(act).map_err(|e| e.to_string()),
        }
    }

    /// Crash-retry convergence: whether the chain's LAST transition for
    /// this act's (tenant, environment) is semantically this act —
    /// timestamps excluded, so the retry of a move whose journal write
    /// landed but whose pointer move did not completes the move instead
    /// of double-journaling it, and an operator re-issuing an identical
    /// move converges.
    fn is_last_for_environment(&self, events: &[RunEvent]) -> bool {
        for event in events.iter().rev() {
            let Some(PayloadRef::Inline(value)) = &event.output else {
                continue;
            };
            let (same_environment, same_act) = match event.kind {
                RunEventKind::RevisionPromoted => {
                    match serde_json::from_value::<RevisionPromotion>(value.clone()) {
                        Ok(recorded) => {
                            let same_environment = match self {
                                DeploymentAct::Promotion(act) => {
                                    recorded.tenant == act.tenant
                                        && recorded.environment == act.environment
                                }
                                DeploymentAct::Rollback(act) => {
                                    recorded.tenant == act.tenant
                                        && recorded.environment == act.environment
                                }
                            };
                            let same_act = match self {
                                DeploymentAct::Promotion(act) => {
                                    recorded.revision_id == act.revision_id
                                        && recorded.previous == act.previous
                                        && recorded.author == act.author
                                }
                                DeploymentAct::Rollback(_) => false,
                            };
                            (same_environment, same_act)
                        }
                        Err(_) => continue,
                    }
                }
                RunEventKind::RevisionRolledBack => {
                    match serde_json::from_value::<RevisionRollback>(value.clone()) {
                        Ok(recorded) => {
                            let same_environment = match self {
                                DeploymentAct::Promotion(act) => {
                                    recorded.tenant == act.tenant
                                        && recorded.environment == act.environment
                                }
                                DeploymentAct::Rollback(act) => {
                                    recorded.tenant == act.tenant
                                        && recorded.environment == act.environment
                                }
                            };
                            let same_act = match self {
                                DeploymentAct::Promotion(_) => false,
                                DeploymentAct::Rollback(act) => {
                                    recorded.from == act.from
                                        && recorded.to == act.to
                                        && recorded.author == act.author
                                }
                            };
                            (same_environment, same_act)
                        }
                        Err(_) => continue,
                    }
                }
                _ => continue,
            };
            if same_environment {
                return same_act;
            }
        }
        false
    }
}

/// The outcome of [`DeploymentControl::transition`].
pub(crate) enum DeploymentMove {
    /// The journaled transition and the pointer move committed together.
    /// `journaled` is false when the chain already carried this act (a
    /// crash-retry completing the move — the journal persist is an
    /// idempotent re-write of the same snapshot); `event_id` names the
    /// appended event when one was.
    Applied {
        event_id: Option<String>,
        journaled: bool,
    },
    /// The live pointer's `active` is not the expected one — a
    /// concurrent move won the race; carries the serving revision. The
    /// caller retries against the moved pointer or fails typed.
    Conflict(Option<RevisionId>),
}

impl DeploymentControl {
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
        match self.store.get_journal(DEPLOYMENT_JOURNAL_RUN_ID).await? {
            Some(snapshot) => Ok(Some(
                Journal::from_snapshot(snapshot, Clock::System).map_err(|e| {
                    format!("the deployment evidence chain failed its integrity check: {e}")
                })?,
            )),
            None => Ok(None),
        }
    }

    /// The chain's events, integrity re-verified (empty until the first
    /// act) — the rollback path's history of record.
    pub(crate) async fn chain_events(&self) -> StoreResult<Vec<RunEvent>> {
        Ok(match self.load_chain().await? {
            Some(journal) => journal.events(),
            None => Vec::new(),
        })
    }

    /// Append a standalone act (registration, declaration, secret acts),
    /// hard-fail: callers treat an `Err` here as "the act did not
    /// happen", and every mutation journals before its store write —
    /// nothing reaches the store the journal did not record first.
    pub(crate) async fn journal_act(
        &self,
        kind: RunEventKind,
        output: Value,
    ) -> StoreResult<String> {
        let _guard = self.chain_lock.lock().await;
        let journal = match self.load_chain().await? {
            Some(journal) => journal,
            None => Journal::new(
                DEPLOYMENT_JOURNAL_RUN_ID,
                DEPLOYMENT_JOURNAL_RUN_ID,
                Clock::System,
            ),
        };
        let event_id = journal.record(EventDraft::new(kind, Effect::Pure).output(output));
        self.store.put_journal(&journal.snapshot()).await?;
        Ok(event_id)
    }

    /// Append a scope denial — best-effort: the typed 403 is the
    /// contract and stands either way (the broker's `journal_denial`
    /// precedent), but the evidence should not be lost without a trace.
    pub(crate) async fn journal_denial(&self, output: Value) {
        let attempt = self
            .journal_act(RunEventKind::EnvSecretDenied, output)
            .await;
        if let Err(e) = attempt {
            tracing::warn!(%e, "env-secret denial could not be journaled");
        }
    }

    /// The transition path: the chain lock held across the dedupe read,
    /// the append, and the store's one-transaction CAS — a crash cannot
    /// leave a journaled move whose pointer never moved (the journal
    /// goes first on the file backend; the pair is one exact transaction
    /// on Postgres), and a crash-retry converges through
    /// [`DeploymentAct::is_last_for_environment`].
    pub(crate) async fn transition(
        &self,
        tenant: &str,
        surface: &str,
        expect: Option<RevisionId>,
        next: &DeploymentPointer,
        act: &DeploymentAct,
    ) -> StoreResult<DeploymentMove> {
        let _guard = self.chain_lock.lock().await;
        let journal = match self.load_chain().await? {
            Some(journal) => journal,
            None => Journal::new(
                DEPLOYMENT_JOURNAL_RUN_ID,
                DEPLOYMENT_JOURNAL_RUN_ID,
                Clock::System,
            ),
        };
        let journaled = !act.is_last_for_environment(&journal.events());
        let event_id = if journaled {
            Some(journal.record(EventDraft::new(act.kind(), Effect::Pure).output(act.output()?)))
        } else {
            None
        };
        match self
            .store
            .transition_deployment(tenant, surface, expect, next, &journal.snapshot())
            .await?
        {
            DeploymentTransition::Applied => Ok(DeploymentMove::Applied {
                event_id,
                journaled,
            }),
            DeploymentTransition::Conflict(live) => Ok(DeploymentMove::Conflict(live)),
        }
    }
}

/// What a rollback of `from` restores (`Some(None)`: the environment
/// returns to serving nothing — the state before its first promotion).
///
/// `from` is the live pointer's `active`, so the replay normally ends
/// there and the target is the stack top. The one exception is the crash
/// window: the transition journals before the pointer moves, so a chain
/// can carry a move the pointer never applied — then the replay ends
/// AHEAD of `from`, and the target re-derives from the stack as it stood
/// when `from` was last installed (a promotion to it, or a rollback
/// landing on it). The rebuilt act then matches the orphaned journal
/// entry, the transition's dedupe converges, and the CAS completes the
/// interrupted move. `None` when `from` never served at all — the route
/// refuses rather than guess.
fn rollback_target(
    events: &[RunEvent],
    tenant: &str,
    environment: &EnvironmentTag,
    from: &RevisionId,
) -> Option<Option<RevisionId>> {
    let mut active: Option<RevisionId> = None;
    let mut stack: Vec<Option<RevisionId>> = Vec::new();
    // The stack as it stood when `from` last became the serving revision.
    let mut installed_stack: Option<Vec<Option<RevisionId>>> = None;
    for event in events {
        let Some(PayloadRef::Inline(value)) = &event.output else {
            continue;
        };
        match event.kind {
            RunEventKind::RevisionPromoted => {
                let Ok(promotion) = serde_json::from_value::<RevisionPromotion>(value.clone())
                else {
                    continue;
                };
                if promotion.tenant == tenant && &promotion.environment == environment {
                    stack.push(promotion.previous);
                    active = Some(promotion.revision_id.clone());
                    if &promotion.revision_id == from {
                        installed_stack = Some(stack.clone());
                    }
                }
            }
            RunEventKind::RevisionRolledBack => {
                let Ok(rollback) = serde_json::from_value::<RevisionRollback>(value.clone()) else {
                    continue;
                };
                if rollback.tenant == tenant && &rollback.environment == environment {
                    let _ = stack.pop();
                    active = rollback.to.clone();
                    if rollback.to.as_ref() == Some(from) {
                        installed_stack = Some(stack.clone());
                    }
                }
            }
            _ => {}
        }
    }
    if active.as_ref() == Some(from) {
        return stack.last().cloned();
    }
    installed_stack.and_then(|stack| stack.last().cloned())
}

// --------------------------------------------------------------------- //
// The run-admission seam
// --------------------------------------------------------------------- //

/// The run payload's deployment declaration (R0.12 wave 3): the
/// environment the run is admitted to. At admission the environment's
/// deployment pointer binds a revision — the active, or the canary when
/// the run's seeded draw admits — and the bound revision's identity
/// checks against the registered graph (name and topology hash), with
/// one `deployment_resolved` event journaled ahead of the run's own
/// events. Absent is the pre-R0.12 behavior, byte-identically: no
/// resolution, no new event.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct DeploymentRunBinding {
    /// The environment the run targets; must be declared and serving.
    pub environment: EnvironmentTag,
}

/// What [`resolve_admission`] produced: the resolution the run journals
/// (and the receipt's walk reads back).
#[derive(Debug, Clone)]
pub(crate) struct DeploymentAdmission {
    /// The journaled resolution: environment, bound revision, pointer
    /// slot, pin-set digest.
    pub resolution: DeploymentResolved,
}

/// Resolve a run's deployment binding against the store: the
/// environment's pointer binds a revision, and the revision checks
/// against the registered graph it will serve.
///
/// Failures are admission failures — the run never starts:
///
/// - `404` when the environment is undeclared, was never promoted into,
///   or serves nothing (a pointer serving nothing binds nothing — never
///   an invented latest).
/// - `422` when the pointer names a revision the store does not hold or
///   the revision fails its own content address (tampered evidence is an
///   admission error, never a journaled resolution), and when the
///   revision no longer describes what would run: a different graph, or
///   a graph whose topology hash drifted from the revision's record (a
///   fresh revision is the journaled way forward — silent drift is not).
pub(crate) async fn resolve_admission(
    store: &Arc<dyn ServerStore>,
    tenant: &str,
    run_id: &str,
    binding: &DeploymentRunBinding,
    graph: &str,
    graph_hash: &str,
) -> Result<DeploymentAdmission, ApiError> {
    let internal = |e: String| ApiError::internal(format!("deployment admission read: {e}"));
    let environment = &binding.environment;
    state_must_declare(store, tenant, environment).await?;
    let surface = deployment_surface(environment);
    let pointer = store
        .get_deployment_pointer(tenant, surface.as_str())
        .await
        .map_err(internal)?
        .ok_or_else(|| {
            ApiError::not_found(format!(
                "environment `{environment}` serves nothing — nothing was ever promoted into \
                 it; an unpromoted environment binds no revision"
            ))
        })?;
    let (revision_id, slot) = deployment_admission(&pointer, run_id).ok_or_else(|| {
        ApiError::not_found(format!(
            "environment `{environment}` serves nothing — its pointer has no active revision \
             and this run's draw did not admit a canary"
        ))
    })?;
    let revision = store
        .get_revision(tenant, revision_id.as_str())
        .await
        .map_err(internal)?
        .ok_or_else(|| {
            ApiError::unprocessable(format!(
                "the deployment pointer for `{environment}` names revision `{}`, which the \
                 store does not hold — corrupt control-plane state; refused, never served",
                revision_id.as_str()
            ))
        })?;
    revision
        .verify_address()
        .map_err(|e| ApiError::unprocessable(e.to_string()))?;
    if revision.content.graph != graph {
        return Err(ApiError::unprocessable(format!(
            "revision `{}` serves graph `{}`; this run targets `{graph}` — a revision binds \
             one graph, and admission will not approximate",
            revision_id.as_str(),
            revision.content.graph
        )));
    }
    if revision.content.graph_hash != graph_hash {
        return Err(ApiError::unprocessable(format!(
            "revision `{}` records topology hash `{}` for graph `{graph}`, but the registered \
             graph hashes to `{graph_hash}` — the build drifted from the revision's record; \
             register a fresh revision, never drift silently",
            revision_id.as_str(),
            revision.content.graph_hash
        )));
    }
    Ok(DeploymentAdmission {
        resolution: DeploymentResolved {
            environment: environment.clone(),
            revision_id,
            pointer: slot,
            pin_set_digest: pin_set_digest(&revision.content.pins),
        },
    })
}

/// The declared-environment gate several routes share (`404` when the
/// environment is not declared).
async fn state_must_declare(
    store: &Arc<dyn ServerStore>,
    tenant: &str,
    environment: &EnvironmentTag,
) -> Result<Environment, ApiError> {
    store
        .get_environment(tenant, environment.as_str())
        .await
        .map_err(internal_err)?
        .ok_or_else(|| {
            ApiError::not_found(format!(
                "environment `{environment}` is not declared — declare it before deploying to \
                 it"
            ))
        })
}

// --------------------------------------------------------------------- //
// The `/deployments` routes
// --------------------------------------------------------------------- //

/// Map a deployment-plane refusal to its HTTP status: every refusal is a
/// contract outcome (`422`), except an unaddressable payload, which is a
/// server-side bug surfaced honestly (`500`).
fn deploy_err(error: &DeployError) -> ApiError {
    match error {
        DeployError::UnaddressableContent(_) => ApiError::internal(error.to_string()),
        _ => ApiError::unprocessable(error.to_string()),
    }
}

/// `POST /deployments/revisions` body: the graph (registered — its
/// current topology hash is what the revision records), the optional
/// assistant binding, the environment the pin set resolves from, the
/// registry surfaces to pin, and the mandatory author.
#[derive(Deserialize)]
pub(crate) struct CreateRevisionPayload {
    graph: String,
    #[serde(default)]
    assistant: Option<String>,
    source_environment: EnvironmentTag,
    #[serde(default)]
    surfaces: Vec<String>,
    author: ProvenanceAuthor,
}

/// `POST /deployments/revisions` — register a revision. The server
/// computes the graph's topology hash from the registered graph (never
/// caller-declared — a hash the server did not compute is a claim, not a
/// fact); each named surface pins the candidate its `surface@{source}`
/// pointer serves as ACTIVE (a revision freezes what verifiably serves —
/// canary slots bind runs, not revisions). `201` on registration; an
/// identical re-declaration converges `200 {created: false}` without
/// journaling (content addressing makes it the same declaration).
pub(crate) async fn create_revision(
    State(state): State<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(payload): Json<CreateRevisionPayload>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let tenant_id = tenant.tenant();
    let (graph, _spec) = state.registry.get(&payload.graph).ok_or_else(|| {
        ApiError::not_found(format!(
            "graph `{}` is not registered — a revision exists to name what serves",
            payload.graph
        ))
    })?;
    let graph_hash = graph.topology_hash();
    if let Some(assistant) = &payload.assistant {
        state
            .server_store
            .get_assistant(&tenant.scope(assistant))
            .await
            .map_err(internal_err)?
            .ok_or_else(|| {
                ApiError::not_found(format!(
                    "assistant `{assistant}` not found — a revision binds only what exists"
                ))
            })?;
    }
    state_must_declare(&state.server_store, tenant_id, &payload.source_environment).await?;

    // Freeze the pin set from the source environment's active pointers.
    let mut pins = Vec::with_capacity(payload.surfaces.len());
    for surface in &payload.surfaces {
        let surface = SurfaceKey::new(surface.clone());
        let target = surface.tagged(&payload.source_environment);
        let pointer = state
            .server_store
            .get_version_pointer(tenant_id, target.as_str())
            .await
            .map_err(internal_err)?
            .ok_or_else(|| {
                ApiError::unprocessable(format!(
                    "surface `{target}` has no version pointer — nothing was ever promoted \
                     for `{}`; an unresolvable pin is no pin",
                    payload.source_environment
                ))
            })?;
        let candidate_id = pointer.active.clone().ok_or_else(|| {
            ApiError::unprocessable(format!(
                "surface `{target}` serves nothing in `{}` — its pointer has no active \
                 version to freeze",
                payload.source_environment
            ))
        })?;
        pins.push(RegistryPin {
            surface,
            candidate_id,
        });
    }

    let revision = DeploymentRevision::new(
        RevisionContent {
            graph: payload.graph.clone(),
            graph_hash,
            assistant: payload.assistant.clone(),
            source_environment: payload.source_environment.clone(),
            pins,
        },
        payload.author.clone(),
        Utc::now(),
    )
    .map_err(|e| deploy_err(&e))?;

    // An identical re-registration converges without journaling — the
    // content address makes it the same declaration.
    if let Some(existing) = state
        .server_store
        .get_revision(tenant_id, revision.revision_id.as_str())
        .await
        .map_err(internal_err)?
    {
        return Ok((
            StatusCode::OK,
            Json(json!({ "created": false, "revision": existing })),
        ));
    }
    let registration = RevisionRegistration {
        tenant: tenant_id.to_owned(),
        revision: revision.clone(),
    };
    let output = serde_json::to_value(&registration).map_err(internal_err)?;
    state
        .deployment
        .journal_act(RunEventKind::RevisionRegistered, output)
        .await
        .map_err(internal_err)?;
    let created = state
        .server_store
        .put_revision(tenant_id, &revision)
        .await
        .map_err(internal_err)?;
    Ok((
        if created {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        },
        Json(json!({ "created": created, "revision": revision })),
    ))
}

/// `GET /deployments/revisions` — the tenant's revisions, sorted by id.
pub(crate) async fn list_revisions(
    State(state): State<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
) -> Result<Json<Value>, ApiError> {
    let mut revisions = state
        .server_store
        .list_revisions(tenant.tenant())
        .await
        .map_err(internal_err)?;
    revisions.sort_by(|a, b| a.revision_id.as_str().cmp(b.revision_id.as_str()));
    Ok(Json(json!({ "revisions": revisions })))
}

/// `GET /deployments/revisions/{revision_id}` — one revision (`404` for
/// unknown or cross-tenant ids).
pub(crate) async fn get_revision(
    State(state): State<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath(revision_id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let revision = state
        .server_store
        .get_revision(tenant.tenant(), &revision_id)
        .await
        .map_err(internal_err)?
        .ok_or_else(|| ApiError::not_found(format!("revision `{revision_id}` not found")))?;
    Ok(Json(json!({ "revision": revision })))
}

/// `POST /deployments/environments` body: the name, the optional gate
/// declaration (recorded this wave, enforced in wave 4), the approval
/// flag (same), and the mandatory author.
#[derive(Deserialize)]
pub(crate) struct DeclareEnvironmentPayload {
    name: String,
    #[serde(default)]
    gate: Option<GateDeclaration>,
    #[serde(default)]
    approval_required: bool,
    author: ProvenanceAuthor,
}

/// `POST /deployments/environments` — declare an environment. `201` on
/// declaration; an identical re-declaration (same name, gate, and
/// approval rule) converges `200 {created: false}`; a different rule
/// under the same name conflicts `409` — declarations are immutable, so
/// the declaration an audit reads is the one in force.
pub(crate) async fn declare_environment(
    State(state): State<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(payload): Json<DeclareEnvironmentPayload>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let tenant_id = tenant.tenant();
    let tag = EnvironmentTag::new(payload.name.clone()).map_err(|e| {
        ApiError::bad_request(format!("invalid environment name `{}`: {e}", payload.name))
    })?;
    if let Some(existing) = state
        .server_store
        .get_environment(tenant_id, tag.as_str())
        .await
        .map_err(internal_err)?
    {
        if existing.gate == payload.gate && existing.approval_required == payload.approval_required
        {
            return Ok((
                StatusCode::OK,
                Json(json!({ "created": false, "environment": existing })),
            ));
        }
        return Err(ApiError::conflict(format!(
            "environment `{}` is already declared with a different rule — declarations are \
             immutable; a changed declaration is a new environment, not an edit",
            payload.name
        )));
    }
    let environment = Environment {
        name: tag,
        gate: payload.gate,
        approval_required: payload.approval_required,
        created_by: payload.author,
        created_at: Utc::now(),
    };
    let declaration = EnvironmentDeclaration {
        tenant: tenant_id.to_owned(),
        environment: environment.clone(),
    };
    let output = serde_json::to_value(&declaration).map_err(internal_err)?;
    state
        .deployment
        .journal_act(RunEventKind::EnvironmentDeclared, output)
        .await
        .map_err(internal_err)?;
    let created = state
        .server_store
        .put_environment(tenant_id, &environment)
        .await
        .map_err(internal_err)?;
    Ok((
        if created {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        },
        Json(json!({ "created": created, "environment": environment })),
    ))
}

/// `GET /deployments/environments` — the tenant's environments, sorted
/// by name.
pub(crate) async fn list_environments(
    State(state): State<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
) -> Result<Json<Value>, ApiError> {
    let mut environments = state
        .server_store
        .list_environments(tenant.tenant())
        .await
        .map_err(internal_err)?;
    environments.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
    Ok(Json(json!({ "environments": environments })))
}

/// `GET /deployments/environments/{name}` — one environment (`404` for
/// unknown or cross-tenant names).
pub(crate) async fn get_environment(
    State(state): State<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let environment = state
        .server_store
        .get_environment(tenant.tenant(), &name)
        .await
        .map_err(internal_err)?
        .ok_or_else(|| ApiError::not_found(format!("environment `{name}` is not declared")))?;
    Ok(Json(json!({ "environment": environment })))
}

/// `POST /deployments/environments/{name}/promote` body: the revision to
/// serve and the mandatory author. The gate and approval declarations on
/// the environment are recorded, not enforced — enforcement wires in
/// wave 4; what this wave guarantees is the move itself: journaled, CAS,
/// byte-exact.
#[derive(Deserialize)]
pub(crate) struct PromotePayload {
    revision_id: String,
    author: ProvenanceAuthor,
}

/// `POST /deployments/environments/{name}/rollback` body: the mandatory
/// author and the cause (the operator's note, the incident id — a
/// rollback without a stated cause is indistinguishable from a fat
/// finger).
#[derive(Deserialize)]
pub(crate) struct RollbackPayload {
    author: ProvenanceAuthor,
    cause: String,
}

/// The promote/rollback handler core: read the pointer, rebuild the act
/// against fresh chain events, commit through the chain-locked CAS; on a
/// lost race, rebuild against the moved pointer exactly once before
/// answering the typed conflict. `build` yields the act, the pointer
/// after it, and the act's desired end state (the convergence check: a
/// re-issued move whose serving state already holds answers `200
/// {applied: false}` without journaling).
async fn move_pointer(
    state: &AppState,
    tenant_id: &str,
    name: &str,
    tag: &EnvironmentTag,
    build: impl Fn(
        Option<&DeploymentPointer>,
        &[RunEvent],
    ) -> Result<(DeploymentAct, DeploymentPointer, Option<RevisionId>), ApiError>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let surface = deployment_surface(tag);
    for attempt in 0..2 {
        let pointer = state
            .server_store
            .get_deployment_pointer(tenant_id, surface.as_str())
            .await
            .map_err(internal_err)?;
        let current = pointer.as_ref().and_then(|p| p.active.clone());
        // Fresh events per attempt: a lost race means the winner's act
        // joined the chain, and the rebuild must see it.
        let events = state
            .deployment
            .chain_events()
            .await
            .map_err(internal_err)?;
        let (act, next, desired) = build(pointer.as_ref(), &events)?;
        if current == desired {
            // State-converged: the asked-for serving state already holds
            // — no journal noise for a re-issued move.
            let pointer = pointer.unwrap_or_else(|| DeploymentPointer::new(surface.clone()));
            return Ok((
                StatusCode::OK,
                Json(json!({ "applied": false, "journaled": false, "pointer": pointer })),
            ));
        }
        let outcome = state
            .deployment
            .transition(tenant_id, surface.as_str(), current, &next, &act)
            .await
            .map_err(internal_err)?;
        match outcome {
            DeploymentMove::Applied {
                event_id,
                journaled,
            } => {
                return Ok((
                    if journaled {
                        StatusCode::CREATED
                    } else {
                        StatusCode::OK
                    },
                    Json(json!({
                        "applied": true,
                        "journaled": journaled,
                        "event_id": event_id,
                        "pointer": next,
                    })),
                ));
            }
            DeploymentMove::Conflict(live) => {
                if attempt == 1 {
                    let serving = live
                        .as_ref()
                        .map(|id| id.as_str().to_owned())
                        .unwrap_or_else(|| "nothing".to_owned());
                    return Err(ApiError::conflict(format!(
                        "environment `{name}` moved under this act — the live pointer serves \
                         `{serving}`; re-read and retry (never a lost move)"
                    )));
                }
            }
        }
    }
    unreachable!("the conflict arm returns on the second attempt")
}

/// `POST /deployments/environments/{name}/promote` — move the
/// environment's pointer to a registered revision. `201` with the moved
/// pointer; a re-issued or state-converged promotion answers `200
/// {applied: false}`; a lost race retries once, then `409`.
pub(crate) async fn promote_revision(
    State(state): State<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath(name): AxumPath<String>,
    Json(payload): Json<PromotePayload>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let tenant_id = tenant.tenant();
    let tag = EnvironmentTag::new(name.clone())
        .map_err(|e| ApiError::bad_request(format!("invalid environment name `{name}`: {e}")))?;
    state_must_declare(&state.server_store, tenant_id, &tag).await?;
    let revision = state
        .server_store
        .get_revision(tenant_id, &payload.revision_id)
        .await
        .map_err(internal_err)?
        .ok_or_else(|| {
            ApiError::not_found(format!("revision `{}` not found", payload.revision_id))
        })?;
    revision
        .verify_address()
        .map_err(|e| ApiError::unprocessable(e.to_string()))?;
    let surface = deployment_surface(&tag);
    move_pointer(&state, tenant_id, &name, &tag, |pointer, _events| {
        let base = pointer
            .cloned()
            .unwrap_or_else(|| DeploymentPointer::new(surface.clone()));
        let promotion = RevisionPromotion {
            tenant: tenant_id.to_owned(),
            environment: tag.clone(),
            revision_id: revision.revision_id.clone(),
            previous: base.active.clone(),
            author: payload.author.clone(),
            promoted_at: Utc::now(),
        };
        let next = base.promoted(&promotion);
        let desired = Some(revision.revision_id.clone());
        Ok((DeploymentAct::Promotion(promotion), next, desired))
    })
    .await
}

/// `POST /deployments/environments/{name}/rollback` — re-point the
/// environment to the revision that served before, byte-exact: the
/// target re-derives from the chain's transition history (the immutable
/// revision that served, never a reconstruction). `201` with the moved
/// pointer (`200` on a converged re-issue); `409` when nothing serves or
/// the requested rollback has no history to restore.
pub(crate) async fn rollback_revision(
    State(state): State<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath(name): AxumPath<String>,
    Json(payload): Json<RollbackPayload>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let tenant_id = tenant.tenant();
    let tag = EnvironmentTag::new(name.clone())
        .map_err(|e| ApiError::bad_request(format!("invalid environment name `{name}`: {e}")))?;
    state_must_declare(&state.server_store, tenant_id, &tag).await?;
    if payload.cause.trim().is_empty() {
        return Err(ApiError::bad_request(
            "a rollback names its cause — the operator's note, the incident id".to_owned(),
        ));
    }
    let surface = deployment_surface(&tag);
    move_pointer(&state, tenant_id, &name, &tag, |pointer, events| {
        let base = pointer
            .cloned()
            .unwrap_or_else(|| DeploymentPointer::new(surface.clone()));
        let Some(from) = base.active.clone() else {
            return Err(ApiError::conflict(format!(
                "environment `{name}` serves nothing — there is no serving revision to roll \
                 back"
            )));
        };
        let Some(to) = rollback_target(events, tenant_id, &tag, &from) else {
            return Err(ApiError::conflict(format!(
                "revision `{}` never served in `{name}` per the deployment chain — there is \
                 no journaled history to restore to",
                from.as_str()
            )));
        };
        let rollback = RevisionRollback {
            tenant: tenant_id.to_owned(),
            environment: tag.clone(),
            from,
            to: to.clone(),
            cause: payload.cause.clone(),
            author: payload.author.clone(),
            rolled_back_at: Utc::now(),
        };
        let next = base.rolled_back(&rollback);
        Ok((DeploymentAct::Rollback(rollback), next, to))
    })
    .await
}

/// `GET /deployments/environments/{name}/pointer` — the environment's
/// serving picture (`404` when nothing was ever promoted into it).
pub(crate) async fn get_environment_pointer(
    State(state): State<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let tenant_id = tenant.tenant();
    let tag = EnvironmentTag::new(name.clone())
        .map_err(|e| ApiError::bad_request(format!("invalid environment name `{name}`: {e}")))?;
    state_must_declare(&state.server_store, tenant_id, &tag).await?;
    let surface = deployment_surface(&tag);
    let pointer = state
        .server_store
        .get_deployment_pointer(tenant_id, surface.as_str())
        .await
        .map_err(internal_err)?
        .ok_or_else(|| {
            ApiError::not_found(format!(
                "environment `{name}` serves nothing — nothing was ever promoted into it"
            ))
        })?;
    Ok(Json(json!({ "pointer": pointer })))
}

/// `GET /deployments/journal` — the deployment evidence chain, integrity
/// re-verified on read (a chain that does not verify is refused, never
/// served as fact — the receipts-journal precedent). Empty until the
/// first control-plane act.
pub(crate) async fn get_deployment_journal(
    State(state): State<Arc<AppState>>,
    Extension(_tenant): Extension<TenantContext>,
) -> Result<Json<Value>, ApiError> {
    let Some(snapshot) = state
        .server_store
        .get_journal(DEPLOYMENT_JOURNAL_RUN_ID)
        .await
        .map_err(internal_err)?
    else {
        return Ok(Json(json!({
            "run_id": DEPLOYMENT_JOURNAL_RUN_ID,
            "events": [],
            "complete": false,
        })));
    };
    let journal = Journal::from_snapshot(snapshot, Clock::System).map_err(|e| {
        ApiError::unprocessable(format!(
            "the deployment evidence chain failed its integrity check: {e} — refusing to \
             serve a chain that does not verify"
        ))
    })?;
    Ok(Json(json!({
        "run_id": DEPLOYMENT_JOURNAL_RUN_ID,
        "events": journal.events(),
        // The chain never completes — it grows with every control-plane act.
        "complete": false,
    })))
}

/// `PUT /deployments/secrets` body: the name, the environment scope, the
/// value (any JSON — sealed as its canonical bytes), and the mandatory
/// author. The value crosses this boundary exactly once — request to
/// seal — and the store only ever holds the envelope.
#[derive(Deserialize)]
pub(crate) struct SetSecretPayload {
    name: String,
    environment: EnvironmentTag,
    value: Value,
    author: ProvenanceAuthor,
}

/// `PUT /deployments/secrets` — set or rotate an environment secret.
/// `201` on creation, `200` on rotation (replacement beneath the stable
/// scoped name; `rotated_at` marks it). The act journals before the
/// store write — nothing reaches the store the journal did not record
/// first.
pub(crate) async fn set_env_secret(
    State(state): State<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(payload): Json<SetSecretPayload>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let tenant_id = tenant.tenant();
    validate_secret_name(&payload.name).map_err(|e| deploy_err(&e))?;
    state_must_declare(&state.server_store, tenant_id, &payload.environment).await?;
    let existing = state
        .server_store
        .get_env_secret(tenant_id, &payload.name, payload.environment.as_str())
        .await
        .map_err(internal_err)?;
    let now = Utc::now();
    let record = EnvSecretRecord {
        name: payload.name.clone(),
        environment: payload.environment.clone(),
        set_by: payload.author.clone(),
        created_at: existing
            .as_ref()
            .map(|s| s.record.created_at)
            .unwrap_or(now),
        rotated_at: existing.as_ref().map(|_| Some(now)).unwrap_or(None),
    };
    // Seal before anything writes: the plaintext's whole life is this
    // handler — request body, one seal, drop.
    let plaintext = serde_json::to_vec(&payload.value).map_err(internal_err)?;
    let scoped = scoped_secret_name(&payload.name, &payload.environment);
    let keys = master_secrets(&state.config.store_path)
        .await
        .map_err(internal_err)?;
    let (key_id, master) = keys
        .last()
        .ok_or_else(|| ApiError::internal("no env-secret master key after mint".to_owned()))?;
    let envelope = seal_env_secret(key_id, master, &scoped, &plaintext).map_err(internal_err)?;
    let stored = StoredEnvSecret {
        record: record.clone(),
        envelope,
    };
    let act = EnvSecretAct {
        tenant: tenant_id.to_owned(),
        record: record.clone(),
    };
    let output = serde_json::to_value(&act).map_err(internal_err)?;
    state
        .deployment
        .journal_act(RunEventKind::EnvSecretSet, output)
        .await
        .map_err(internal_err)?;
    let created = state
        .server_store
        .set_env_secret(tenant_id, &stored)
        .await
        .map_err(internal_err)?;
    Ok((
        if created {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        },
        Json(json!({ "created": created, "record": record })),
    ))
}

/// `GET /deployments/secrets` query: the optional environment filter.
#[derive(Deserialize)]
pub(crate) struct ListSecretsQuery {
    #[serde(default)]
    environment: Option<EnvironmentTag>,
}

/// `GET /deployments/secrets?environment=` — the tenant's secret
/// metadata (never the envelopes — a listing is an audit view), sorted
/// by (environment, name).
pub(crate) async fn list_env_secrets(
    State(state): State<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Query(query): Query<ListSecretsQuery>,
) -> Result<Json<Value>, ApiError> {
    let mut records = state
        .server_store
        .list_env_secrets(tenant.tenant())
        .await
        .map_err(internal_err)?;
    if let Some(environment) = &query.environment {
        records.retain(|record| &record.environment == environment);
    }
    records.sort_by(|a, b| {
        (a.environment.as_str(), a.name.as_str()).cmp(&(b.environment.as_str(), b.name.as_str()))
    });
    Ok(Json(json!({ "secrets": records })))
}

/// `POST /deployments/secrets/resolve` body: the name, the environment
/// scope requested, and the environment the requester HOLDS (the
/// admission environment the resolution runs under). The tenant is the
/// HTTP trust boundary; the authoritative run-binding seam — the run's
/// journaled admission environment answering for the holder — is
/// in-process; this route carries the holder explicitly so the scope
/// check is exercised by what the caller declares, audited by what the
/// chain records.
#[derive(Deserialize)]
pub(crate) struct ResolveSecretPayload {
    name: String,
    environment: EnvironmentTag,
    holder: EnvironmentTag,
}

/// `POST /deployments/secrets/resolve` — resolve a secret at use, inside
/// its declared scope. A cross-environment request fails closed: `403`
/// typed (`environment_scope_denied`) and journaled (best-effort — the
/// broker's denial discipline); an unheld master key fails closed `500`
/// (a store shared with a host that sealed elsewhere cannot open here).
pub(crate) async fn resolve_env_secret(
    State(state): State<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(payload): Json<ResolveSecretPayload>,
) -> Result<Json<Value>, ApiError> {
    let tenant_id = tenant.tenant();
    if payload.holder != payload.environment {
        let denial = EnvSecretDenial {
            tenant: tenant_id.to_owned(),
            name: payload.name.clone(),
            requested_environment: payload.environment.clone(),
            held_environment: payload.holder.clone(),
            denied_at: Utc::now(),
        };
        if let Ok(output) = serde_json::to_value(&denial) {
            state.deployment.journal_denial(output).await;
        }
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "environment_scope_denied",
            format!(
                "secret `{}` is scoped to `{}`; the holder admits `{}` — a value never \
                 crosses its environment's boundary",
                payload.name, payload.environment, payload.holder,
            ),
        ));
    }
    let scoped = scoped_secret_name(&payload.name, &payload.environment);
    let stored = state
        .server_store
        .get_env_secret(tenant_id, &payload.name, payload.environment.as_str())
        .await
        .map_err(internal_err)?
        .ok_or_else(|| ApiError::not_found(format!("secret `{scoped}` not found")))?;
    let keys = master_secrets(&state.config.store_path)
        .await
        .map_err(internal_err)?;
    let Some(master) = master_for(&keys, &stored.envelope.key_id) else {
        return Err(ApiError::internal(format!(
            "secret `{scoped}` was sealed under key `{}`, which this host does not hold — \
             failing closed",
            stored.envelope.key_id
        )));
    };
    let plaintext = open_env_secret(master, &scoped, &stored.envelope)
        .map_err(|e| ApiError::internal(format!("secret `{scoped}` failed to open: {e}")))?;
    let value: Value = serde_json::from_slice(&plaintext).map_err(internal_err)?;
    Ok(Json(json!({
        "name": payload.name,
        "environment": payload.environment,
        "value": value,
    })))
}

/// `DELETE /deployments/secrets/{environment}/{name}` body: the
/// mandatory author (revocation is an attributable act).
#[derive(Deserialize)]
pub(crate) struct RevokeSecretPayload {
    author: ProvenanceAuthor,
}

/// `DELETE /deployments/secrets/{environment}/{name}` — revoke by
/// deletion, sealed envelope included. The revocation journals before
/// the delete — the tombstone is the evidence the scope once held a
/// value. `204`; `404` for unknown or cross-tenant scoped names.
pub(crate) async fn revoke_env_secret(
    State(state): State<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath((environment, name)): AxumPath<(String, String)>,
    Json(payload): Json<RevokeSecretPayload>,
) -> Result<StatusCode, ApiError> {
    let tenant_id = tenant.tenant();
    let tag = EnvironmentTag::new(environment.clone()).map_err(|e| {
        ApiError::bad_request(format!("invalid environment name `{environment}`: {e}"))
    })?;
    state
        .server_store
        .get_env_secret(tenant_id, &name, tag.as_str())
        .await
        .map_err(internal_err)?
        .ok_or_else(|| {
            ApiError::not_found(format!(
                "secret `{}` not found",
                scoped_secret_name(&name, &tag)
            ))
        })?;
    let revocation = EnvSecretRevocation {
        tenant: tenant_id.to_owned(),
        name: name.clone(),
        environment: tag.clone(),
        revoked_by: payload.author,
        revoked_at: Utc::now(),
    };
    let output = serde_json::to_value(&revocation).map_err(internal_err)?;
    state
        .deployment
        .journal_act(RunEventKind::EnvSecretRevoked, output)
        .await
        .map_err(internal_err)?;
    state
        .server_store
        .delete_env_secret(tenant_id, &name, tag.as_str())
        .await
        .map_err(internal_err)?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use rusty_agent_runtime::deploy::{EnvSecretRecord, RevisionContent};
    use rusty_agent_runtime::learn::EnvironmentTag;
    use rusty_agent_runtime::memory::ProvenanceAuthor;

    fn ts(millis: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp_millis(millis).unwrap()
    }

    fn tag(name: &str) -> EnvironmentTag {
        EnvironmentTag::new(name).unwrap()
    }

    fn temp_root() -> PathBuf {
        std::env::temp_dir().join(format!("rusty-deploy-test-{}", uuid::Uuid::new_v4()))
    }

    fn revision() -> DeploymentRevision {
        DeploymentRevision::new(
            RevisionContent {
                graph: "pipeline".into(),
                graph_hash: "a".repeat(64),
                assistant: None,
                source_environment: tag("staging"),
                pins: Vec::new(),
            },
            ProvenanceAuthor::Human {
                human_id: "amjad".into(),
            },
            ts(1_760_000_000_000),
        )
        .unwrap()
    }

    fn environment() -> Environment {
        Environment {
            name: tag("staging"),
            gate: None,
            approval_required: false,
            created_by: ProvenanceAuthor::Human {
                human_id: "amjad".into(),
            },
            created_at: ts(1_760_000_000_000),
        }
    }

    #[tokio::test]
    async fn revisions_and_environments_round_trip_with_corrupt_tolerance() {
        let root = temp_root();
        let revision = revision();
        let scoped = revision.revision_id.to_string();
        persist_revision(&root, &scoped, &revision).await.unwrap();
        let tenant_scoped = format!("acme/{scoped}");
        persist_revision(&root, &tenant_scoped, &revision)
            .await
            .unwrap();
        std::fs::write(revisions_dir(&root).join("broken.json"), b"{nope").unwrap();
        let loaded = load_revisions(&root);
        assert_eq!(loaded.len(), 2, "corrupt files are skipped, not fatal");
        assert!(loaded.contains_key(&scoped), "default tenant: bare key");
        assert_eq!(
            loaded[&tenant_scoped].revision_id, revision.revision_id,
            "named tenant: the key comes from the path"
        );

        let environment = environment();
        let scoped_name = format!("acme/{}", environment.name.as_str());
        persist_environment(&root, environment.name.as_str(), &environment)
            .await
            .unwrap();
        persist_environment(&root, &scoped_name, &environment)
            .await
            .unwrap();
        let loaded = load_environments(&root);
        assert_eq!(loaded.len(), 2);
        assert!(loaded.contains_key(environment.name.as_str()));
        assert!(loaded.contains_key(&scoped_name));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn pointers_and_secrets_round_trip_through_hashed_filenames() {
        let root = temp_root();
        let pointer = DeploymentPointer::new(rusty_agent_runtime::learn::SurfaceKey::new(
            "deployment:staging",
        ));
        let scoped_surface = "acme/deployment:staging";
        persist_pointer(&root, scoped_surface, &pointer)
            .await
            .unwrap();

        // The filename is the key's hash — the raw surface (with its
        // `:` and `/`) appears nowhere in the directory listing.
        let listing: Vec<String> = std::fs::read_dir(pointers_dir(&root))
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            listing,
            vec![format!("{}.json", sha256_hex(scoped_surface.as_bytes()))]
        );
        let loaded = load_pointers(&root);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[scoped_surface], pointer);

        // An envelope whose key does not hash back to its filename is
        // corrupt (or a collision) and skipped, not served.
        let forged = pointers_dir(&root).join(format!("{}.json", sha256_hex(b"forged")));
        std::fs::write(
            forged,
            serde_json::to_vec_pretty(&KeyedFile {
                key: "acme/deployment:prod".to_string(),
                record: pointer.clone(),
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            load_pointers(&root).len(),
            1,
            "the forged-name file is skipped"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn secrets_seal_open_and_fail_closed_across_scopes() {
        let root = temp_root();
        let keys = master_secrets(&root).await.unwrap();
        assert_eq!(keys.len(), 1, "first use mints one key");
        let (key_id, master) = &keys[0];

        // The master key file is 0600 and outside the store layout.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(master_secret_path(&root, key_id))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        let scoped = "database-url@staging";
        let value = b"postgres://staging-db:5432/app";
        let envelope = seal_env_secret(key_id, master, scoped, value).unwrap();
        assert_eq!(open_env_secret(master, scoped, &envelope).unwrap(), value);

        // A ciphertext transplanted across scopes fails its tag — scope
        // is identity, enforced by the associated data.
        assert!(open_env_secret(master, "database-url@prod", &envelope).is_err());
        // A wrong master key fails the same way — nothing about why leaks.
        let wrong: [u8; 32] = draw_random();
        assert!(open_env_secret(&wrong, scoped, &envelope).is_err());
        // A tampered ciphertext fails its tag.
        let mut tampered = envelope.clone();
        tampered.ciphertext = hex_encode(b"forged");
        assert!(open_env_secret(master, scoped, &tampered).is_err());
        // A master key this host does not hold is a fail-closed miss.
        assert!(master_for(&keys, "esk-doesnotexist").is_none());

        // The persisted envelope file holds ciphertext only — the raw
        // bytes on disk carry no plaintext fragment.
        let stored = StoredEnvSecret {
            record: EnvSecretRecord {
                name: "database-url".into(),
                environment: tag("staging"),
                set_by: ProvenanceAuthor::Human {
                    human_id: "amjad".into(),
                },
                created_at: ts(1_760_000_000_000),
                rotated_at: None,
            },
            envelope,
        };
        persist_env_secret(&root, scoped, &stored).await.unwrap();
        let raw =
            std::fs::read(env_secrets_dir(&root).join(format!("{}.json", keyed_file_name(scoped))))
                .unwrap();
        let needle = b"staging-db";
        assert!(
            !raw.windows(needle.len()).any(|w| w == needle),
            "the store holds ciphertext only — a raw read finds no plaintext"
        );
        let loaded = load_env_secrets(&root);
        assert_eq!(loaded.len(), 1);
        let reopened = open_env_secret(master, scoped, &loaded[scoped].envelope).unwrap();
        assert_eq!(reopened, value, "a restart resolves byte-exact");
        let _ = std::fs::remove_dir_all(root);
    }
}
