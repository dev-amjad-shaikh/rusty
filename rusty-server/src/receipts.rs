//! The receipt key lifecycle (R0.9 Rusty Capsules, wave 3): first-boot
//! key generation under `{store_path}/keys/`, journaled rotation, and the
//! key history both store backends keep so old receipts verify.
//!
//! The design doc is `docs/capsules-design.md` ("Signed run receipts").
//! v1 is *local signing with local keys*, honestly scoped: one signing
//! identity per server deployment, Ed25519 key material generated on
//! first boot, and rotation as a documented, journaled operation. KMS
//! integration, remote attestation, and transparency-log witnessing are
//! R1.0+ (open question 3); the receipt's canonical form is already the
//! byte string a transparency log would witness.
//!
//! ## Where the material lives
//!
//! - **Secrets** (`{store_path}/keys/{key_id}.secret`, hex, mode `0600`)
//!   stay on the local filesystem and never pass through the store
//!   abstraction — the secret-file functions here are the only code that
//!   touches them, so the Postgres backend cannot hold what a database
//!   must not leak. Permissions are set at creation (`0600` from the
//!   first byte, not retrofitted); an operator is expected to keep the
//!   keys directory backed up, because a lost secret cannot be recovered
//!   — only rotated past.
//! - **Key history** ([`ReceiptKeyRecord`]: key id, public key,
//!   registration and retirement instants) goes through the store trait,
//!   so both backends persist it and a receipt signed by a retired key
//!   keeps verifying. Key ids are content addresses of the public key
//!   ([`rusty_agent_runtime::receipt::derive_key_id`]): two records can
//!   never disagree about which key an id names.
//! - **Rotation** is journaled: every key registration — genesis,
//!   operator rotation, or a host joining a shared store without the
//!   local secret — appends a `signing_key_rotated` event to the
//!   deployment's receipts journal ([`RECEIPTS_JOURNAL_RUN_ID`]), the
//!   supervision-journal precedent applied to the control plane. The
//!   journal is the lineage evidence; the store's key history is the
//!   lookup index verification reads.
//!
//! Multi-host note, stated plainly: a second host booting against a
//! shared store finds no local secret and becomes *its own* signer — a
//! new key is generated, registered, and journaled; old receipts keep
//! verifying because verification resolves keys by id from the history.
//! Fleet-scale key management (shared signing identity, compromise
//! response across N servers) is exactly the R1.0 KMS work the design
//! defers.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use rusty_agent_runtime::journal::{Clock, EventDraft, Journal};
use rusty_agent_runtime::receipt::{RunReceipt, SigningKey, SigningKeyRotation};
use rusty_agent_runtime::record::{Effect, RunEventKind};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::server_store::{ServerStore, StoreResult};

/// The deterministic run id of the deployment's receipts journal: the
/// chained record of every signing-key registration. Distinct from
/// executor run ids (UUIDs) by construction, deployment-wide (keys are
/// not tenant state), and free of `/` so the JSON-file layout keeps one
/// file per journal — the `supervision_journal_run_id` rules.
pub(crate) const RECEIPTS_JOURNAL_RUN_ID: &str = "receipt-keys";

/// One entry in the deployment's signing-key history: the public half of
/// a key that has signed (or still signs) receipts. Immutability by
/// construction: the key id is the public key's content address, so the
/// only legitimate update is annotating `retired_at` at rotation — and a
/// retired key still verifies the receipts it signed, which is the whole
/// point of keeping the history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ReceiptKeyRecord {
    /// The content-addressed key id (sha256 of the public key bytes).
    pub key_id: String,

    /// The public key, lowercase hex.
    pub public_key: String,

    /// When this key was first registered (genesis, rotation, or a new
    /// host joining a shared store).
    pub registered_at: DateTime<Utc>,

    /// When an operator rotation retired the key. Informational — never a
    /// verification input: old receipts must verify against retired keys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retired_at: Option<DateTime<Utc>>,
}

/// The outcome of a rotation, returned by `POST /receipt_keys/rotate`.
#[derive(Debug, Clone)]
pub(crate) struct RotationOutcome {
    /// The key id that signed until now (`None` when rotation doubled as
    /// genesis — a deployment whose first key operation was a rotation).
    pub previous_key_id: Option<String>,
    /// The new key id.
    pub new_key_id: String,
    /// The new public key, lowercase hex.
    pub public_key: String,
    /// The journaled rotation event's id.
    pub event_id: String,
}

// --------------------------------------------------------------------- //
// The JSON-file layout
// --------------------------------------------------------------------- //

/// The keys directory under the store root (`{store_path}/keys`; `keys`
/// is a reserved layout name, see [`crate::RESERVED_NAMES`]).
pub(crate) fn keys_dir(root: &Path) -> PathBuf {
    root.join("keys")
}

/// The receipts directory (`{store_path}/receipts`; `receipts` reserved).
pub(crate) fn receipts_dir(root: &Path) -> PathBuf {
    root.join("receipts")
}

fn secret_path(root: &Path, key_id: &str) -> PathBuf {
    keys_dir(root).join(format!("{key_id}.secret"))
}

fn record_path(root: &Path, key_id: &str) -> PathBuf {
    keys_dir(root).join(format!("{key_id}.json"))
}

fn receipt_path(root: &Path, run_id: &str) -> PathBuf {
    receipts_dir(root).join(format!("{run_id}.json"))
}

/// Persist one key-history record atomically (temp file + rename) — the
/// durability discipline every file record in the server shares.
pub(crate) async fn persist_key_record(root: &Path, record: &ReceiptKeyRecord) -> io::Result<()> {
    let dir = keys_dir(root);
    tokio::fs::create_dir_all(&dir).await?;
    let bytes = serde_json::to_vec_pretty(record)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let path = record_path(root, &record.key_id);
    let tmp = dir.join(format!(".{}.tmp", record.key_id));
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, path).await
}

/// Load one key-history record; `None` when the id is unknown.
pub(crate) async fn load_key_record(
    root: &Path,
    key_id: &str,
) -> io::Result<Option<ReceiptKeyRecord>> {
    match tokio::fs::read(record_path(root, key_id)).await {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Load every key-history record under the keys directory. Files that
/// fail to parse are skipped with a warning (the corrupt-tolerance rule
/// every loader here shares).
pub(crate) fn load_key_records(root: &Path) -> Vec<ReceiptKeyRecord> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(keys_dir(root)) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<ReceiptKeyRecord>(&bytes).ok())
        {
            Some(record) => out.push(record),
            None => tracing::warn!(path = %path.display(), "skipping unreadable key record file"),
        }
    }
    out
}

/// Persist a minted receipt, replacing any earlier receipt for the run —
/// the journal-snapshot rule (`journals::persist`): the receipt binds a
/// head, and a run whose journal has advanced gets a fresh receipt.
pub(crate) async fn persist_run_receipt(
    root: &Path,
    run_id: &str,
    receipt: &RunReceipt,
) -> io::Result<()> {
    let dir = receipts_dir(root);
    tokio::fs::create_dir_all(&dir).await?;
    let bytes = serde_json::to_vec_pretty(receipt)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let path = receipt_path(root, run_id);
    let tmp = dir.join(format!(".{run_id}.{}.tmp", uuid::Uuid::new_v4()));
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, path).await
}

/// Load the receipt stored for `run_id`; `None` when none was minted.
pub(crate) async fn load_run_receipt(root: &Path, run_id: &str) -> io::Result<Option<RunReceipt>> {
    match tokio::fs::read(receipt_path(root, run_id)).await {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

// --------------------------------------------------------------------- //
// Secret files — local only, never through the store abstraction
// --------------------------------------------------------------------- //

/// Write a newly generated secret key with `0600` permissions from the
/// first byte (`create_new` + mode at creation, not a chmod after the
/// fact — there is no window where the file is looser). A secret file is
/// written exactly once: rotation mints a new key id, never overwrites.
async fn write_secret(root: &Path, key_id: &str, signing_key: &SigningKey) -> io::Result<()> {
    let dir = keys_dir(root);
    tokio::fs::create_dir_all(&dir).await?;
    let path = secret_path(root, key_id);
    #[cfg(unix)]
    {
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true).mode(0o600);
        use tokio::io::AsyncWriteExt as _;
        let mut file = options.open(&path).await?;
        file.write_all(signing_key.to_hex().as_bytes()).await?;
    }
    #[cfg(not(unix))]
    {
        tokio::fs::write(&path, signing_key.to_hex()).await?;
    }
    Ok(())
}

/// Read the local secret for `key_id`; `None` when this host does not
/// hold it (a peer host generated it against a shared store).
fn read_secret(root: &Path, key_id: &str) -> Option<SigningKey> {
    let hex = std::fs::read_to_string(secret_path(root, key_id)).ok()?;
    SigningKey::from_hex(hex.trim())
}

// --------------------------------------------------------------------- //
// The rotation journal
// --------------------------------------------------------------------- //

/// Append a `signing_key_rotated` event to the deployment's receipts
/// journal, persisting the grown snapshot. Returns the event id.
///
/// The journal is rebuilt from its persisted snapshot through
/// [`Journal::from_snapshot`] — the same integrity check every journal
/// read here runs — so a tampered receipts journal fails the append
/// rather than silently forking the lineage chain.
pub(crate) async fn journal_key_rotation(
    store: &Arc<dyn ServerStore>,
    rotation: &SigningKeyRotation,
) -> StoreResult<String> {
    let journal = match store.get_journal(RECEIPTS_JOURNAL_RUN_ID).await? {
        Some(snapshot) => Journal::from_snapshot(snapshot, Clock::System)
            .map_err(|e| format!("the receipts journal failed its integrity check: {e}"))?,
        None => Journal::new(
            RECEIPTS_JOURNAL_RUN_ID,
            RECEIPTS_JOURNAL_RUN_ID,
            Clock::System,
        ),
    };
    let event_id = journal.record(
        EventDraft::new(RunEventKind::SigningKeyRotated, Effect::Pure)
            .output(serde_json::to_value(rotation).map_err(|e| e.to_string())?),
    );
    store.put_journal(&journal.snapshot()).await?;
    Ok(event_id)
}

// --------------------------------------------------------------------- //
// The keyring
// ---------------------------------------------------------------------//

/// The deployment's signing key, resolved lazily on first use and held
/// for signing thereafter.
///
/// Lazy for the same reason the Postgres checkpointer is
/// (`build_backends` stays synchronous): the first receipt operation —
/// not server boot — pays for generation or secret loading. The mutex
/// serializes ensure-and-rotate within the process, so two concurrent
/// first uses cannot generate two genesis keys.
pub(crate) struct SigningKeyring {
    store: Arc<dyn ServerStore>,
    store_path: PathBuf,
    active: Mutex<Option<SigningKey>>,
}

impl SigningKeyring {
    /// A keyring over `store` (the key history) with secrets under
    /// `{store_path}/keys/`.
    pub(crate) fn new(store: Arc<dyn ServerStore>, store_path: PathBuf) -> Self {
        Self {
            store,
            store_path,
            active: Mutex::new(None),
        }
    }

    /// The active signing key, ensuring it exists first.
    pub(crate) async fn active_key(&self) -> StoreResult<SigningKey> {
        let mut guard = self.active.lock().await;
        self.ensure_locked(&mut guard).await
    }

    /// Ensure the active key: serve the cached key, else adopt the newest
    /// non-retired history record whose secret this host holds, else
    /// generate — genesis (empty history), or a new signer for a host
    /// that joined a shared store without the secret. Every generation is
    /// registered in the history and journaled (the lineage contract:
    /// "which key signed what, from when" is a chained fact, never a
    /// registry note).
    async fn ensure_locked(&self, guard: &mut Option<SigningKey>) -> StoreResult<SigningKey> {
        if let Some(key) = guard {
            return Ok(key.clone());
        }
        let mut records = self.store.list_receipt_keys().await?;
        records.sort_by_key(|record| record.registered_at);
        let active_records: Vec<&ReceiptKeyRecord> = records
            .iter()
            .filter(|record| record.retired_at.is_none())
            .collect();
        for record in active_records.iter().rev() {
            if let Some(key) = read_secret(&self.store_path, &record.key_id) {
                *guard = Some(key.clone());
                return Ok(key);
            }
        }
        let key = SigningKey::generate();
        let key_id = key.key_id();
        write_secret(&self.store_path, &key_id, &key)
            .await
            .map_err(|e| format!("write signing key secret: {e}"))?;
        let record = ReceiptKeyRecord {
            key_id: key_id.clone(),
            public_key: key.public_key().to_hex(),
            registered_at: Utc::now(),
            retired_at: None,
        };
        self.store.put_receipt_key(&record).await?;
        // Genesis has no predecessor; a host joining a shared store
        // succeeds the newest active record it cannot sign with.
        let previous_key_id = active_records.last().map(|record| record.key_id.clone());
        journal_key_rotation(
            &self.store,
            &SigningKeyRotation {
                previous_key_id,
                new_key_id: key_id,
                public_key: record.public_key,
                rotated_at: record.registered_at,
            },
        )
        .await?;
        *guard = Some(key.clone());
        Ok(key)
    }

    /// Rotate: generate a successor, retire the current key in the
    /// history, journal the new key id, and sign with the successor from
    /// here on. The retired key's secret stays on disk (verification of
    /// old receipts needs only the public history, but an operator
    /// auditing locally may want it); retiring is an annotation, never a
    /// deletion — old receipts must keep verifying.
    pub(crate) async fn rotate(&self) -> StoreResult<RotationOutcome> {
        let mut guard = self.active.lock().await;
        let current = self.ensure_locked(&mut guard).await?;
        let previous_key_id = current.key_id();
        let now = Utc::now();
        if let Some(mut record) = self.store.get_receipt_key(&previous_key_id).await? {
            record.retired_at = Some(now);
            self.store.put_receipt_key(&record).await?;
        }
        let successor = SigningKey::generate();
        let new_key_id = successor.key_id();
        write_secret(&self.store_path, &new_key_id, &successor)
            .await
            .map_err(|e| format!("write signing key secret: {e}"))?;
        let record = ReceiptKeyRecord {
            key_id: new_key_id.clone(),
            public_key: successor.public_key().to_hex(),
            registered_at: now,
            retired_at: None,
        };
        self.store.put_receipt_key(&record).await?;
        let event_id = journal_key_rotation(
            &self.store,
            &SigningKeyRotation {
                previous_key_id: Some(previous_key_id.clone()),
                new_key_id: new_key_id.clone(),
                public_key: record.public_key.clone(),
                rotated_at: now,
            },
        )
        .await?;
        *guard = Some(successor);
        Ok(RotationOutcome {
            previous_key_id: Some(previous_key_id),
            new_key_id,
            public_key: record.public_key,
            event_id,
        })
    }
}
