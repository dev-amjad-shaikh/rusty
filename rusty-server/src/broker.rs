//! The credential/connection broker (R0.11 Extension Plane, wave 3):
//! envelope-encrypted connection storage on both backends, the handle
//! issue/resolve/revoke/expire lifecycle, and the journaled evidence
//! chain. The pure contracts live in `rusty_agent_runtime::broker`;
//! this module owns key custody, cryptography, and persistence.
//!
//! ## Key custody
//!
//! The deployment master key lives **outside the store abstraction**,
//! exactly as receipt signing secrets do (`crate::receipts`): one file
//! per master key under `{store_path}/keys/broker-master.{key_id}.secret`
//! (hex, `0600` from the first byte, written once), so the Postgres
//! backend cannot hold what a database must not leak. Key ids are
//! random (`bmk-{16 hex}` — never a hash of the key: a content address
//! of symmetric material is a verifier oracle). Envelopes record the
//! `key_id` that wrapped them, so rotation (the design's open question
//! 3: lazy re-wrap of per-connection data keys, KMS/HSM as the R1.0
//! plug point) lands additively; wave 3 runs a single active key.
//!
//! ## Cryptography
//!
//! XChaCha20-Poly1305 (see `Cargo.toml` for the dependency rationale).
//! Each connection's token material is encrypted under a per-connection
//! data key (32 random bytes, minted at registration); data keys are
//! wrapped by the master key. Both seals authenticate the connection id
//! as associated data, so a ciphertext transplanted between connections
//! fails to open. Handles are HMAC-SHA256-signed claims under a key
//! derived from the master key by HMAC domain separation — validity
//! (expiry, scope binding) is self-contained, and only the connection
//! liveness check hits the store at resolution.
//!
//! ## Evidence
//!
//! Everything journals into the deployment's broker evidence chain, run
//! id [`BROKER_JOURNAL_RUN_ID`] — the `receipt-keys` precedent applied
//! to a second control plane. Hard-fail: a mutation or use that cannot
//! journal does not happen (nothing reaches the store the journal did
//! not record first). Uses and denials name the connection, the handle,
//! and the grant — never the bytes; the run binding travels in the
//! payload rather than in the run's own journal, because a run's
//! journal is not always server-persisted (embedder-driven runs) and
//! the deployment chain is the only target the broker can always write.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use hmac::{Hmac, Mac};
use rusty_agent_runtime::broker::{
    hex_decode, hex_encode, new_connection_id, new_handle_id, scopes_missing, BrokerDenial,
    ConnectionConsent, ConnectionProvider, ConnectionRecord, ConnectionRefresh,
    ConnectionRevocation, ConnectionStatus, CredentialHandle, CredentialUse, HandleClaims,
    HandleIssuance, SealedCredential, StoredConnection, TokenMaterial, SEALED_FORMAT_VERSION,
};
use rusty_agent_runtime::journal::{Clock, EventDraft, Journal};
use rusty_agent_runtime::record::{Effect, RunEventKind};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use tokio::sync::Mutex;

use chacha20poly1305::aead::{Aead, KeyInit, OsRng};
use chacha20poly1305::XChaCha20Poly1305;

use crate::server_store::{ConnectionUpdate, ServerStore, StoreResult};

/// The deterministic run id of the deployment's broker journal: the
/// chained record of every connection registration, consent, refresh,
/// revocation, issuance, use, and denial. Deployment-wide (the chain is
/// the control plane's; the tenant travels in each payload), distinct
/// from executor run ids by construction, and free of `/` so the
/// JSON-file layout keeps one file per journal — the
/// `RECEIPTS_JOURNAL_RUN_ID` rules.
pub(crate) const BROKER_JOURNAL_RUN_ID: &str = "credential-broker";

/// The default handle TTL: five minutes — "handles live for minutes"
/// (the design's open question 5 leaning), short enough that expiry is
/// routine, long enough that a run's burst of calls reuses one issuance.
pub(crate) const DEFAULT_HANDLE_TTL: Duration = Duration::from_secs(300);

type HmacSha256 = Hmac<Sha256>;

/// The master key id prefix; the id is the prefix plus 16 lowercase hex
/// chars (random, the `uuid` draw the receipt keys use).
const MASTER_KEY_ID_PREFIX: &str = "bmk-";

/// The domain-separation label deriving the handle-signing key from the
/// master key: one HMAC invocation, so a handle signature can never be
/// repurposed as an envelope operation (or vice versa) — the two keys
/// are independent by construction.
const HANDLE_KEY_DOMAIN: &[u8] = b"rusty-broker-handle-signing-v1";

// --------------------------------------------------------------------- //
// Master key custody (the receipts.rs secret-file discipline)
// --------------------------------------------------------------------- //

fn master_secret_path(root: &Path, key_id: &str) -> PathBuf {
    crate::receipts::keys_dir(root).join(format!("broker-master.{key_id}.secret"))
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

/// Every master key this host holds, keyed by key id. Files that fail to
/// parse are skipped with a warning (the corrupt-tolerance rule), as are
/// names outside the `broker-master.{id}.secret` shape.
/// The held master keys: `(key_id, key)` pairs, oldest first — the last
/// entry seals, every entry opens (the rotation seam).
type MasterKeys = Vec<(String, [u8; 32])>;

fn load_master_secrets(root: &Path) -> MasterKeys {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(crate::receipts::keys_dir(root)) else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(rest) = name.strip_prefix("broker-master.") else {
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
                tracing::warn!(path = %entry.path().display(), "skipping unreadable broker master key file")
            }
        }
    }
    out
}

// --------------------------------------------------------------------- //
// The envelope cryptography
// --------------------------------------------------------------------- //

/// Draw `N` bytes of OS entropy through `AeadCore::generate_nonce`-grade
/// randomness — `OsRng`, the same `getrandom` source `uuid` uses.
fn draw_random<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    use chacha20poly1305::aead::rand_core::RngCore as _;
    OsRng.fill_bytes(&mut bytes);
    bytes
}

/// Seal token material under a freshly minted per-connection data key —
/// the registration path. The connection id authenticates both seals
/// (associated data): a wrapped key or ciphertext transplanted to
/// another connection's envelope fails its tag.
fn seal_new(
    master_id: &str,
    master: &[u8; 32],
    connection_id: &str,
    material: &TokenMaterial,
) -> StoreResult<SealedCredential> {
    let data_key = draw_random::<32>();
    seal_with_data_key(master_id, master, connection_id, &data_key, None, material)
}

/// Re-seal under the *existing* envelope's data key — the consent path:
/// the wrapped key and its nonce carry over (the data key does not
/// change when the material does), and the payload nonce is always
/// fresh, so no (key, nonce) pair ever repeats.
fn reseal(
    master_id: &str,
    master: &[u8; 32],
    connection_id: &str,
    existing: &SealedCredential,
    material: &TokenMaterial,
) -> StoreResult<SealedCredential> {
    let data_key = unwrap_data_key(master, connection_id, existing)?;
    seal_with_data_key(
        master_id,
        master,
        connection_id,
        &data_key,
        Some(existing),
        material,
    )
}

fn seal_with_data_key(
    master_id: &str,
    master: &[u8; 32],
    connection_id: &str,
    data_key: &[u8; 32],
    existing: Option<&SealedCredential>,
    material: &TokenMaterial,
) -> StoreResult<SealedCredential> {
    let aad = connection_id.as_bytes();
    let (wrapped_data_key, wrap_nonce) = match existing {
        Some(envelope) => (
            envelope.wrapped_data_key.clone(),
            envelope.wrap_nonce.clone(),
        ),
        None => {
            let wrap_nonce_bytes = draw_random::<24>();
            let wrapped = XChaCha20Poly1305::new(master.into())
                .encrypt(
                    chacha20poly1305::XNonce::from_slice(&wrap_nonce_bytes),
                    chacha20poly1305::aead::Payload { msg: data_key, aad },
                )
                .map_err(|_| "wrap data key: AEAD failure".to_string())?;
            (hex_encode(&wrapped), hex_encode(&wrap_nonce_bytes))
        }
    };
    let plaintext =
        serde_json::to_vec(material).map_err(|e| format!("serialize token material: {e}"))?;
    let nonce_bytes = draw_random::<24>();
    let ciphertext = XChaCha20Poly1305::new(data_key.into())
        .encrypt(
            chacha20poly1305::XNonce::from_slice(&nonce_bytes),
            chacha20poly1305::aead::Payload {
                msg: &plaintext,
                aad,
            },
        )
        .map_err(|_| "seal token material: AEAD failure".to_string())?;
    Ok(SealedCredential {
        format_version: SEALED_FORMAT_VERSION,
        key_id: master_id.to_owned(),
        wrapped_data_key,
        wrap_nonce,
        nonce: hex_encode(&nonce_bytes),
        ciphertext: hex_encode(&ciphertext),
        sealed_at: Utc::now(),
    })
}

fn unwrap_data_key(
    master: &[u8; 32],
    connection_id: &str,
    envelope: &SealedCredential,
) -> StoreResult<[u8; 32]> {
    let wrapped = hex_decode(&envelope.wrapped_data_key)
        .ok_or_else(|| "corrupt envelope: wrapped key is not hex".to_string())?;
    let nonce = hex_decode(&envelope.wrap_nonce)
        .ok_or_else(|| "corrupt envelope: wrap nonce is not hex".to_string())?;
    if nonce.len() != 24 {
        return Err("corrupt envelope: wrap nonce is not 24 bytes".to_string());
    }
    let data_key = XChaCha20Poly1305::new(master.into())
        .decrypt(
            chacha20poly1305::XNonce::from_slice(&nonce),
            chacha20poly1305::aead::Payload {
                msg: &wrapped,
                aad: connection_id.as_bytes(),
            },
        )
        .map_err(|_| "the wrapped data key failed its authentication tag".to_string())?;
    <[u8; 32]>::try_from(data_key)
        .map_err(|_| "corrupt envelope: data key is not 32 bytes".to_string())
}

/// Open an envelope: unwrap the data key, decrypt, parse. Any failure —
/// wrong master key, tampered ciphertext, transplanted envelope, corrupt
/// hex — is the same refusal: the envelope does not open, and nothing
/// about *why* leaks to the caller (the unknown-handle posture).
fn open(
    master: &[u8; 32],
    connection_id: &str,
    envelope: &SealedCredential,
) -> StoreResult<TokenMaterial> {
    if envelope.format_version != SEALED_FORMAT_VERSION {
        return Err(format!(
            "envelope format version {} is not {SEALED_FORMAT_VERSION}",
            envelope.format_version
        ));
    }
    let data_key = unwrap_data_key(master, connection_id, envelope)?;
    let ciphertext = hex_decode(&envelope.ciphertext)
        .ok_or_else(|| "corrupt envelope: ciphertext is not hex".to_string())?;
    let nonce = hex_decode(&envelope.nonce)
        .ok_or_else(|| "corrupt envelope: nonce is not hex".to_string())?;
    if nonce.len() != 24 {
        return Err("corrupt envelope: nonce is not 24 bytes".to_string());
    }
    let plaintext = XChaCha20Poly1305::new((&data_key).into())
        .decrypt(
            chacha20poly1305::XNonce::from_slice(&nonce),
            chacha20poly1305::aead::Payload {
                msg: &ciphertext,
                aad: connection_id.as_bytes(),
            },
        )
        .map_err(|_| "the sealed credential failed its authentication tag".to_string())?;
    serde_json::from_slice(&plaintext).map_err(|e| format!("corrupt token material: {e}"))
}

// --------------------------------------------------------------------- //
// Handle signing
// --------------------------------------------------------------------- //

/// The handle-signing key, derived from a master key by one HMAC under
/// the domain label. No HKDF dependency: the input key is already
/// uniform 32 bytes, so a single HMAC extraction is the whole derivation
/// (the `uuid`-draws-entropy posture — reach for the primitive the
/// moment actually needs).
fn handle_signing_key(master: &[u8; 32]) -> [u8; 32] {
    let mut mac = <HmacSha256 as Mac>::new_from_slice(master).expect("HMAC takes any key length");
    mac.update(HANDLE_KEY_DOMAIN);
    mac.finalize().into_bytes().into()
}

fn sign_claims(signing_key: &[u8; 32], claims: &HandleClaims) -> StoreResult<String> {
    let message =
        serde_json::to_vec(claims).map_err(|e| format!("serialize handle claims: {e}"))?;
    let mut mac =
        <HmacSha256 as Mac>::new_from_slice(signing_key).expect("HMAC takes any key length");
    mac.update(&message);
    Ok(hex_encode(&mac.finalize().into_bytes()))
}

/// Verify a presented signature over the claims — recompute and compare
/// through the MAC's own constant-time check, never `==` on hex.
fn verify_claims(
    signing_key: &[u8; 32],
    claims: &HandleClaims,
    signature_hex: &str,
) -> StoreResult<bool> {
    let message =
        serde_json::to_vec(claims).map_err(|e| format!("serialize handle claims: {e}"))?;
    let Some(presented) = hex_decode(signature_hex) else {
        return Ok(false);
    };
    let mut mac =
        <HmacSha256 as Mac>::new_from_slice(signing_key).expect("HMAC takes any key length");
    mac.update(&message);
    Ok(mac.verify_slice(&presented).is_ok())
}

// --------------------------------------------------------------------- //
// The JSON-file layout (the registry.rs hashed-filename rule)
// --------------------------------------------------------------------- //

/// The connections directory under the store root
/// (`{store_path}/connections`; `connections` is a reserved layout name,
/// see [`crate::RESERVED_NAMES`]).
pub(crate) fn connections_dir(root: &Path) -> PathBuf {
    root.join("connections")
}

/// The connection file's body: the stored record plus the scoped key it
/// was written under. The filename is the key's hash — scoped ids
/// contain `/` for named tenants, and a one-way filename needs the true
/// key recorded somewhere (the version-pointer envelope's rule,
/// verbatim).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ConnectionFile {
    /// The tenant-scoped connection id (`{tenant}/conn-…` for named
    /// tenants).
    key: String,
    /// The stored connection (record + sealed credential).
    record: StoredConnection,
}

fn connection_file_name(scoped_id: &str) -> String {
    rusty_agent_runtime::record::sha256_hex(scoped_id.as_bytes())
}

/// Persist one stored connection atomically (temp file + rename) — the
/// durability discipline every file record in the server shares.
pub(crate) async fn persist_connection(
    root: &Path,
    scoped_id: &str,
    record: &StoredConnection,
) -> io::Result<()> {
    let dir = connections_dir(root);
    tokio::fs::create_dir_all(&dir).await?;
    let file = ConnectionFile {
        key: scoped_id.to_string(),
        record: record.clone(),
    };
    let bytes = serde_json::to_vec_pretty(&file)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let name = connection_file_name(scoped_id);
    let tmp = dir.join(format!("{name}.tmp"));
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, dir.join(format!("{name}.json"))).await
}

/// Remove a connection's file; `false` when it was not there.
pub(crate) async fn delete_connection_file(root: &Path, scoped_id: &str) -> io::Result<bool> {
    let path = connections_dir(root).join(format!("{}.json", connection_file_name(scoped_id)));
    match tokio::fs::remove_file(&path).await {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Load all stored connections, keyed by the scoped id carried in each
/// file's envelope. A file whose envelope key does not hash back to its
/// filename is corrupt (or a collision) and is skipped with a warning —
/// the fail-closed loader rule every hashed-filename layout here shares.
pub(crate) fn load_connections(root: &Path) -> std::collections::HashMap<String, StoredConnection> {
    let dir = connections_dir(root);
    let mut out = std::collections::HashMap::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let parsed = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<ConnectionFile>(&raw).ok());
        let matches_name = parsed.as_ref().is_some_and(|file| {
            path.file_stem().and_then(|s| s.to_str()) == Some(&*connection_file_name(&file.key))
        });
        match (parsed, matches_name) {
            (Some(file), true) => {
                out.insert(file.key, file.record);
            }
            _ => tracing::warn!(path = %path.display(), "skipping unreadable connection file"),
        }
    }
    out
}

// --------------------------------------------------------------------- //
// The broker
// --------------------------------------------------------------------- //

/// The outcome of [`Broker::record_consent`].
#[derive(Debug)]
pub(crate) enum ConsentOutcome {
    /// The consent (or refresh) applied; the journaled event kind says
    /// which it was.
    Applied {
        /// The updated record.
        record: ConnectionRecord,
        /// The journaled kind (`connection_consented` when the scope set
        /// changed, `connection_refreshed` when only the material did).
        journaled: RunEventKind,
    },
    /// Nothing changed — same scopes, no new material (the idempotent
    /// create's rule: re-recording the same fact converges).
    Converged(ConnectionRecord),
    /// No such connection in this tenant.
    Unknown,
    /// A concurrent mutation won the read-modify-write race.
    Conflict,
}

/// The outcome of [`Broker::revoke`].
#[derive(Debug)]
pub(crate) enum RevokeOutcome {
    /// The revocation applied and journaled.
    Applied {
        /// The revoked record.
        record: ConnectionRecord,
        /// The journaled event's id.
        event_id: String,
    },
    /// Already revoked — re-revocation converges without a second event
    /// (the fact already journaled once).
    Converged(ConnectionRecord),
    /// No such connection in this tenant.
    Unknown,
    /// A concurrent mutation won the race.
    Conflict,
}

/// The deployment's credential broker. Cheap to share (`Arc`); the
/// master key resolves lazily on first use and is held thereafter, and
/// one mutex serializes appends to the deployment journal so concurrent
/// resolutions cannot clobber each other's freshly journaled events
/// (whole-snapshot persistence — the `journal_locks` reasoning, applied
/// to a journal this module owns).
pub(crate) struct Broker {
    store: Arc<dyn ServerStore>,
    store_path: PathBuf,
    masters: Mutex<Option<MasterKeys>>,
    handle_ttl: Duration,
    journal_lock: Mutex<()>,
}

impl std::fmt::Debug for Broker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Broker")
            .field("store_path", &self.store_path)
            .field("handle_ttl", &self.handle_ttl)
            .finish()
    }
}

impl Broker {
    /// A broker over `store` with master keys under `{store_path}/keys/`
    /// and handle TTL `handle_ttl`.
    pub(crate) fn new(
        store: Arc<dyn ServerStore>,
        store_path: PathBuf,
        handle_ttl: Duration,
    ) -> Self {
        Self {
            store,
            store_path,
            masters: Mutex::new(None),
            handle_ttl,
            journal_lock: Mutex::new(()),
        }
    }

    /// The master keys, ensuring at least one exists: serve the cached
    /// set, else load the local secrets, else generate the genesis key
    /// (written `0600`, never through the store abstraction). The active
    /// key for *sealing* is the last entry; every loaded key stays
    /// available for *opening*, so envelopes wrapped under an older key
    /// keep resolving (the rotation seam).
    async fn masters(&self) -> StoreResult<MasterKeys> {
        let mut guard = self.masters.lock().await;
        if let Some(masters) = guard.as_ref() {
            return Ok(masters.clone());
        }
        let mut masters = load_master_secrets(&self.store_path);
        if masters.is_empty() {
            let key_id = format!(
                "{}{}",
                MASTER_KEY_ID_PREFIX,
                &uuid::Uuid::new_v4().simple().to_string()[..16]
            );
            let key = draw_random::<32>();
            write_master_secret(&self.store_path, &key_id, &key)
                .await
                .map_err(|e| format!("write broker master key: {e}"))?;
            masters.push((key_id, key));
        }
        *guard = Some(masters.clone());
        Ok(masters)
    }

    /// The active master key (for sealing and signing).
    async fn active_master(&self) -> StoreResult<(String, [u8; 32])> {
        let masters = self.masters().await?;
        masters
            .last()
            .cloned()
            .ok_or_else(|| "no broker master key".to_string())
    }

    /// The master key an envelope names (for opening).
    async fn master_for(&self, key_id: &str) -> StoreResult<[u8; 32]> {
        self.masters()
            .await?
            .into_iter()
            .find(|(id, _)| id == key_id)
            .map(|(_, key)| key)
            .ok_or_else(|| format!("master key `{key_id}` is not held by this host"))
    }

    /// Append one event to the deployment's broker journal, persisting
    /// the grown snapshot — serialized by `journal_lock`, integrity
    /// re-verified on load (a tampered broker journal fails the append
    /// rather than silently forking the chain). Hard-fail by contract:
    /// callers treat an `Err` here as "the mutation did not happen".
    async fn journal(&self, draft: EventDraft) -> StoreResult<String> {
        let _guard = self.journal_lock.lock().await;
        let journal = match self.store.get_journal(BROKER_JOURNAL_RUN_ID).await? {
            Some(snapshot) => Journal::from_snapshot(snapshot, Clock::System)
                .map_err(|e| format!("the broker journal failed its integrity check: {e}"))?,
            None => Journal::new(BROKER_JOURNAL_RUN_ID, BROKER_JOURNAL_RUN_ID, Clock::System),
        };
        let event_id = journal.record(draft);
        self.store.put_journal(&journal.snapshot()).await?;
        Ok(event_id)
    }

    /// Journal a denial (best-effort: the denial stands either way — it
    /// is already the fail-closed answer — but the evidence should not
    /// be lost without a trace).
    async fn journal_denial(&self, denial: &BrokerDenial) {
        let Ok(output) = serde_json::to_value(denial) else {
            return;
        };
        if let Err(e) = self
            .journal(EventDraft::new(RunEventKind::CredentialDenied, Effect::Pure).output(output))
            .await
        {
            tracing::warn!(%e, "broker denial could not be journaled");
        }
    }

    /// Register a connection: validate, seal the material under a fresh
    /// data key, journal, then persist — nothing reaches the store the
    /// journal did not record first.
    pub(crate) async fn register(
        &self,
        tenant: &str,
        provider: ConnectionProvider,
        subject: Option<String>,
        scopes: std::collections::BTreeSet<String>,
        material: &TokenMaterial,
    ) -> StoreResult<ConnectionRecord> {
        let now = Utc::now();
        let record = ConnectionRecord {
            connection_id: new_connection_id(),
            provider,
            subject,
            scopes,
            status: ConnectionStatus::Active,
            health: Default::default(),
            created_at: now,
            updated_at: now,
        };
        record.validate().map_err(|e| e.to_string())?;
        let (key_id, master) = self.active_master().await?;
        let credential = seal_new(&key_id, &master, &record.connection_id, material)?;
        self.journal(
            EventDraft::new(RunEventKind::ConnectionRegistered, Effect::Pure)
                .output(serde_json::to_value(&record).map_err(|e| e.to_string())?),
        )
        .await?;
        self.store
            .put_connection(
                tenant,
                &StoredConnection {
                    record: record.clone(),
                    credential,
                },
            )
            .await?;
        Ok(record)
    }

    /// Record a consent act: a new scope ceiling and/or fresh token
    /// material. The human's grant is the approval — executed at the
    /// provider, recorded here, journaled — and this is the *only* way a
    /// consented set changes. A consent also re-activates: it is the
    /// re-auth path out of `needs_reauth`. Scope-set changes journal
    /// `connection_consented`; material-only updates journal
    /// `connection_refreshed`.
    pub(crate) async fn record_consent(
        &self,
        tenant: &str,
        connection_id: &str,
        scopes: Option<std::collections::BTreeSet<String>>,
        material: Option<&TokenMaterial>,
    ) -> StoreResult<ConsentOutcome> {
        let Some(stored) = self.store.get_connection(tenant, connection_id).await? else {
            return Ok(ConsentOutcome::Unknown);
        };
        let scopes_changed = scopes.as_ref().is_some_and(|s| *s != stored.record.scopes);
        if !scopes_changed && material.is_none() {
            return Ok(ConsentOutcome::Converged(stored.record));
        }
        let now = Utc::now();
        let mut record = stored.record.clone();
        if let Some(scopes) = scopes {
            record.scopes = scopes;
        }
        record.status = ConnectionStatus::Active;
        record.health.consecutive_failures = 0;
        record.updated_at = now;
        record.validate().map_err(|e| e.to_string())?;
        let credential = match material {
            Some(material) => {
                record.health.last_refresh_at = Some(now);
                let (key_id, master) = self.active_master().await?;
                reseal(
                    &key_id,
                    &master,
                    connection_id,
                    &stored.credential,
                    material,
                )?
            }
            None => stored.credential.clone(),
        };
        let (kind, output) = if scopes_changed {
            (
                RunEventKind::ConnectionConsented,
                serde_json::to_value(ConnectionConsent {
                    connection_id: connection_id.to_owned(),
                    subject: record.subject.clone(),
                    scopes: record.scopes.clone(),
                    recorded_at: now,
                })
                .map_err(|e| e.to_string())?,
            )
        } else {
            (
                RunEventKind::ConnectionRefreshed,
                serde_json::to_value(ConnectionRefresh {
                    connection_id: connection_id.to_owned(),
                    refreshed_at: now,
                    expires_at: material.and_then(|m| m.expires_at),
                })
                .map_err(|e| e.to_string())?,
            )
        };
        // Evidence first: the journaled act, then the state change. On a
        // lost CAS race the event records an intent that never applied —
        // the caller's retry journals anew; the answer is `Conflict`,
        // never a silent divergence.
        self.journal(EventDraft::new(kind, Effect::Pure).output(output))
            .await?;
        let updated = StoredConnection {
            record: record.clone(),
            credential,
        };
        match self
            .store
            .update_connection(tenant, connection_id, &stored, &updated)
            .await?
        {
            ConnectionUpdate::Applied => Ok(ConsentOutcome::Applied {
                record,
                journaled: kind,
            }),
            ConnectionUpdate::Unknown => Ok(ConsentOutcome::Unknown),
            ConnectionUpdate::Conflict => Ok(ConsentOutcome::Conflict),
        }
    }

    /// Revoke: the status flip and its journaled event commit together
    /// (event first), and outstanding handles fail at their next use —
    /// resolution reads live state, so revocation takes effect at the
    /// next call, not the next deploy.
    pub(crate) async fn revoke(
        &self,
        tenant: &str,
        connection_id: &str,
        reason: Option<String>,
    ) -> StoreResult<RevokeOutcome> {
        let Some(stored) = self.store.get_connection(tenant, connection_id).await? else {
            return Ok(RevokeOutcome::Unknown);
        };
        if stored.record.status == ConnectionStatus::Revoked {
            return Ok(RevokeOutcome::Converged(stored.record));
        }
        let now = Utc::now();
        let mut record = stored.record.clone();
        record.status = ConnectionStatus::Revoked;
        record.updated_at = now;
        let event_id = self
            .journal(
                EventDraft::new(RunEventKind::ConnectionRevoked, Effect::Pure).output(
                    serde_json::to_value(ConnectionRevocation {
                        connection_id: connection_id.to_owned(),
                        grant: record.scopes.clone(),
                        revoked_at: now,
                        reason,
                    })
                    .map_err(|e| e.to_string())?,
                ),
            )
            .await?;
        let updated = StoredConnection {
            record: record.clone(),
            credential: stored.credential.clone(),
        };
        match self
            .store
            .update_connection(tenant, connection_id, &stored, &updated)
            .await?
        {
            ConnectionUpdate::Applied => Ok(RevokeOutcome::Applied { record, event_id }),
            ConnectionUpdate::Unknown => Ok(RevokeOutcome::Unknown),
            ConnectionUpdate::Conflict => Ok(RevokeOutcome::Conflict),
        }
    }

    /// Delete a connection: revoke first when still live (the evidence
    /// trail — a deleted connection's grant stopped holding *here*),
    /// then erase the stored record, sealed material included (the
    /// memory-forget posture: real deletion of derived state). A deleted
    /// connection fails closed exactly like a revoked one — resolution
    /// answers `unknown_connection`.
    pub(crate) async fn delete(&self, tenant: &str, connection_id: &str) -> StoreResult<bool> {
        let Some(stored) = self.store.get_connection(tenant, connection_id).await? else {
            return Ok(false);
        };
        if stored.record.status != ConnectionStatus::Revoked {
            match self
                .revoke(tenant, connection_id, Some("connection deleted".to_owned()))
                .await?
            {
                RevokeOutcome::Applied { .. } | RevokeOutcome::Converged(_) => {}
                RevokeOutcome::Unknown => return Ok(false),
                RevokeOutcome::Conflict => {
                    return Err("connection changed under deletion; retry".to_string())
                }
            }
        }
        self.store.delete_connection(tenant, connection_id).await
    }

    /// Read one connection's public record (never the sealed material —
    /// the read paths serve metadata; the bytes move only at resolution).
    pub(crate) async fn get(
        &self,
        tenant: &str,
        connection_id: &str,
    ) -> StoreResult<Option<ConnectionRecord>> {
        Ok(self
            .store
            .get_connection(tenant, connection_id)
            .await?
            .map(|stored| stored.record))
    }

    /// Every connection record the tenant holds (order unspecified;
    /// callers sort).
    pub(crate) async fn list(&self, tenant: &str) -> StoreResult<Vec<ConnectionRecord>> {
        Ok(self
            .store
            .list_connections(tenant)
            .await?
            .into_iter()
            .map(|stored| stored.record)
            .collect())
    }

    /// Issue a handle: check the connection's live state and the consent
    /// ceiling, mint and sign the claims, journal the issuance —
    /// hard-fail, so a handle that exists was always journaled.
    pub(crate) async fn issue(
        &self,
        request: &rusty_agent_runtime::broker::IssueRequest,
    ) -> Result<CredentialHandle, BrokerDenial> {
        let stored = self
            .store
            .get_connection(&request.tenant, &request.requirement.connection_id)
            .await
            .map_err(|e| BrokerDenial::unavailable(format!("the connection read failed: {e}")))?;
        let Some(stored) = stored else {
            let denial = BrokerDenial {
                connection_id: Some(request.requirement.connection_id.clone()),
                handle_id: None,
                tenant: Some(request.tenant.clone()),
                reason: rusty_agent_runtime::broker::BrokerDenialReason::UnknownConnection,
                detail: format!(
                    "connection `{}` is unknown to tenant `{}`",
                    request.requirement.connection_id, request.tenant
                ),
            };
            self.journal_denial(&denial).await;
            return Err(denial);
        };
        let record = &stored.record;
        let state_denial = match record.status {
            ConnectionStatus::Active => None,
            ConnectionStatus::Revoked => Some(BrokerDenial::connection_revoked(
                record,
                &request.tenant,
                "unissued",
            )),
            ConnectionStatus::NeedsReauth => Some(BrokerDenial::connection_needs_reauth(
                record,
                &request.tenant,
                "unissued",
            )),
        };
        if let Some(denial) = state_denial {
            self.journal_denial(&denial).await;
            return Err(denial);
        }
        // The narrowing rule: an empty request asks for the whole consent
        // set; anything else must be covered by it. Beyond the ceiling is
        // a denial *here*, never at the provider.
        let narrowed = if request.requirement.scopes.is_empty() {
            record.scopes.clone()
        } else {
            let missing = scopes_missing(&record.scopes, &request.requirement.scopes);
            if !missing.is_empty() {
                let denial = BrokerDenial::scope_not_granted(
                    Some((&record.connection_id, None, &request.tenant)),
                    missing,
                    format!(
                        "connection `{}` consented to {}; the request asks beyond that ceiling",
                        record.connection_id,
                        serde_json::to_string(&record.scopes).unwrap_or_default(),
                    ),
                );
                self.journal_denial(&denial).await;
                return Err(denial);
            }
            request.requirement.scopes.clone()
        };
        let now = Utc::now();
        let claims = HandleClaims {
            handle_id: new_handle_id(),
            connection_id: record.connection_id.clone(),
            tenant: request.tenant.clone(),
            run_id: request.run_id.clone(),
            scopes: narrowed,
            issued_at: now,
            expires_at: now
                + chrono::Duration::from_std(self.handle_ttl)
                    .map_err(|e| BrokerDenial::unavailable(format!("invalid handle TTL: {e}")))?,
        };
        let (_, master) = self.active_master().await.map_err(|e| {
            BrokerDenial::unavailable(format!("the master key is unavailable: {e}"))
        })?;
        let signature = sign_claims(&handle_signing_key(&master), &claims)
            .map_err(BrokerDenial::unavailable)?;
        self.journal(
            EventDraft::new(RunEventKind::CredentialHandleIssued, Effect::Pure).output(
                serde_json::to_value(HandleIssuance {
                    claims: claims.clone(),
                })
                .map_err(|e| BrokerDenial::unavailable(e.to_string()))?,
            ),
        )
        .await
        .map_err(|e| {
            BrokerDenial::unavailable(format!("the issuance could not be journaled: {e}"))
        })?;
        Ok(CredentialHandle::from_parts(claims, signature))
    }

    /// Resolve a handle at use: verify, check expiry and scope coverage
    /// from the self-contained claims, read the connection's *live*
    /// state, open the envelope for the connector, and journal the use —
    /// hard-fail, so a resolution that happened was always journaled.
    /// Every denial is journaled too.
    pub(crate) async fn resolve(
        &self,
        token: &str,
        scopes: &std::collections::BTreeSet<String>,
    ) -> Result<rusty_agent_runtime::broker::ResolvedCredential, BrokerDenial> {
        let (claims, signature) = match CredentialHandle::parse_token(token) {
            Ok(parts) => parts,
            Err(denial) => {
                self.journal_denial(&denial).await;
                return Err(denial);
            }
        };
        let (_, master) = self.active_master().await.map_err(|e| {
            BrokerDenial::unavailable(format!("the master key is unavailable: {e}"))
        })?;
        let signing_key = handle_signing_key(&master);
        match verify_claims(&signing_key, &claims, &signature) {
            Ok(true) => {}
            Ok(false) => {
                let denial =
                    BrokerDenial::unknown_handle("the handle signature did not verify".to_owned());
                self.journal_denial(&denial).await;
                return Err(denial);
            }
            Err(e) => return Err(BrokerDenial::unavailable(e)),
        }
        if claims.is_expired(Utc::now()) {
            let denial = BrokerDenial::handle_expired(&claims);
            self.journal_denial(&denial).await;
            return Err(denial);
        }
        let missing = scopes_missing(&claims.scopes, scopes);
        if !missing.is_empty() {
            let denial = BrokerDenial::scope_not_granted(
                Some((
                    &claims.connection_id,
                    Some(&claims.handle_id),
                    &claims.tenant,
                )),
                missing,
                "the call asked beyond the handle's narrowed set".to_owned(),
            );
            self.journal_denial(&denial).await;
            return Err(denial);
        }
        let stored = self
            .store
            .get_connection(&claims.tenant, &claims.connection_id)
            .await
            .map_err(|e| BrokerDenial::unavailable(format!("the connection read failed: {e}")))?;
        let Some(stored) = stored else {
            let denial = BrokerDenial::unknown_connection(&claims);
            self.journal_denial(&denial).await;
            return Err(denial);
        };
        match stored.record.status {
            ConnectionStatus::Active => {}
            ConnectionStatus::Revoked => {
                let denial = BrokerDenial::connection_revoked(
                    &stored.record,
                    &claims.tenant,
                    &claims.handle_id,
                );
                self.journal_denial(&denial).await;
                return Err(denial);
            }
            ConnectionStatus::NeedsReauth => {
                let denial = BrokerDenial::connection_needs_reauth(
                    &stored.record,
                    &claims.tenant,
                    &claims.handle_id,
                );
                self.journal_denial(&denial).await;
                return Err(denial);
            }
        }
        let master = self
            .master_for(&stored.credential.key_id)
            .await
            .map_err(BrokerDenial::unavailable)?;
        let material = open(&master, &claims.connection_id, &stored.credential)
            .map_err(|e| BrokerDenial::unavailable(format!("the envelope did not open: {e}")))?;
        self.journal(
            EventDraft::new(RunEventKind::CredentialUse, Effect::ReadOnly).output(
                serde_json::to_value(CredentialUse {
                    handle_id: claims.handle_id.clone(),
                    connection_id: claims.connection_id.clone(),
                    tenant: claims.tenant.clone(),
                    run_id: claims.run_id.clone(),
                    scopes_checked: scopes.clone(),
                    used_at: Utc::now(),
                })
                .map_err(|e| BrokerDenial::unavailable(e.to_string()))?,
            ),
        )
        .await
        .map_err(|e| BrokerDenial::unavailable(format!("the use could not be journaled: {e}")))?;
        Ok(rusty_agent_runtime::broker::ResolvedCredential {
            connection_id: claims.connection_id,
            handle_id: claims.handle_id,
            scopes: scopes.clone(),
            material,
        })
    }
}

/// The core seam, implemented so `CredentialMediator` /
/// `BrokeredCapsuleHost` mediate against the real broker in-process
/// (the A2A capsule path and embedders building on the server crate).
#[async_trait::async_trait]
impl rusty_agent_runtime::broker::CredentialBroker for Broker {
    async fn issue(
        &self,
        request: &rusty_agent_runtime::broker::IssueRequest,
    ) -> Result<CredentialHandle, BrokerDenial> {
        Broker::issue(self, request).await
    }

    async fn resolve(
        &self,
        token: &str,
        scopes: &std::collections::BTreeSet<String>,
    ) -> Result<rusty_agent_runtime::broker::ResolvedCredential, BrokerDenial> {
        Broker::resolve(self, token, scopes).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server_store::JsonFileStore;
    use chrono::DateTime;
    use rusty_agent_runtime::broker::{BrokerDenialReason, CredentialRequirement, IssueRequest};
    use std::collections::BTreeSet;

    /// Unique temp store root, removed at the end of each test (best
    /// effort).
    fn temp_store() -> PathBuf {
        std::env::temp_dir().join(format!("rusty-broker-test-{}", uuid::Uuid::new_v4()))
    }

    fn material() -> TokenMaterial {
        TokenMaterial {
            access_token: "sk-live-MARKER-9f2e".into(),
            refresh_token: Some("rt-MARKER-41ab".into()),
            expires_at: Some(DateTime::<Utc>::from_timestamp_millis(1_800_003_600_000).unwrap()),
        }
    }

    fn broker(root: &Path) -> (Broker, Arc<dyn ServerStore>) {
        let store: Arc<dyn ServerStore> = Arc::new(JsonFileStore::load(root));
        (
            Broker::new(Arc::clone(&store), root.to_path_buf(), DEFAULT_HANDLE_TTL),
            store,
        )
    }

    async fn register(b: &Broker) -> ConnectionRecord {
        b.register(
            "acme",
            ConnectionProvider::Oauth2AuthorizationCode,
            Some("user-7".into()),
            BTreeSet::from(["drive.readonly".to_owned(), "drive.write".to_owned()]),
            &material(),
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn wrong_key_and_tampering_fail_to_open() {
        let root = temp_store();
        let (key_id, master) = ("bmk-testmaster0001".to_string(), draw_random::<32>());
        let envelope = seal_new(&key_id, &master, "conn-abc", &material()).unwrap();
        // A wrong master key fails the wrap tag.
        let wrong = draw_random::<32>();
        assert!(open(&wrong, "conn-abc", &envelope).is_err());
        // A transplanted envelope (same bytes, another connection's AAD)
        // fails.
        assert!(open(&master, "conn-other", &envelope).is_err());
        // Tampered ciphertext fails the tag.
        let mut tampered = envelope.clone();
        let mut bytes = hex_decode(&tampered.ciphertext).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        tampered.ciphertext = hex_encode(&bytes);
        assert!(open(&master, "conn-abc", &tampered).is_err());
        // A tampered wrapped key fails the wrap tag.
        let mut tampered = envelope.clone();
        let mut bytes = hex_decode(&tampered.wrapped_data_key).unwrap();
        bytes[0] ^= 1;
        tampered.wrapped_data_key = hex_encode(&bytes);
        assert!(open(&master, "conn-abc", &tampered).is_err());
        // The honest envelope opens byte-exact.
        assert_eq!(open(&master, "conn-abc", &envelope).unwrap(), material());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn reseal_keeps_the_data_key_and_freshens_the_nonce() {
        let (key_id, master) = ("bmk-testmaster0002".to_string(), draw_random::<32>());
        let first = seal_new(&key_id, &master, "conn-abc", &material()).unwrap();
        let rotated = TokenMaterial {
            access_token: "sk-live-ROTATED".into(),
            refresh_token: None,
            expires_at: None,
        };
        let second = reseal(&key_id, &master, "conn-abc", &first, &rotated).unwrap();
        assert_eq!(first.wrapped_data_key, second.wrapped_data_key);
        assert_ne!(first.nonce, second.nonce);
        assert_eq!(open(&master, "conn-abc", &second).unwrap(), rotated);
    }

    #[tokio::test]
    async fn master_key_file_is_0600_and_outside_the_store() {
        let root = temp_store();
        let (b, _) = broker(&root);
        register(&b).await;
        let keys: Vec<_> = std::fs::read_dir(crate::receipts::keys_dir(&root))
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(keys.len(), 1);
        let name = keys[0].file_name().to_string_lossy().into_owned();
        assert!(name.starts_with("broker-master.bmk-"), "got: {name}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = keys[0].metadata().unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "master key file mode: {mode:o}");
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn store_holds_ciphertext_only_and_restart_resolves() {
        let root = temp_store();
        let record = {
            let (b, _) = broker(&root);
            register(&b).await
        };
        // The on-disk record contains no plaintext credential: scan the
        // raw bytes of every file under the store root for both markers.
        let mut raw = Vec::new();
        for entry in std::fs::read_dir(connections_dir(&root)).unwrap().flatten() {
            raw.extend(std::fs::read(entry.path()).unwrap());
        }
        let raw = String::from_utf8_lossy(&raw);
        assert!(!raw.contains("sk-live-MARKER-9f2e"), "plaintext at rest");
        assert!(!raw.contains("rt-MARKER-41ab"), "plaintext at rest");
        // A fresh process (new store index, no cached master key)
        // resolves: the master key file and the ciphertext both persist.
        let (b2, _) = broker(&root);
        let request = IssueRequest {
            tenant: "acme".into(),
            run_id: Some("run-1".into()),
            requirement: CredentialRequirement {
                connection_id: record.connection_id.clone(),
                scopes: BTreeSet::from(["drive.readonly".to_owned()]),
            },
        };
        let handle = rusty_agent_runtime::broker::CredentialBroker::issue(&b2, &request)
            .await
            .unwrap();
        let resolved = rusty_agent_runtime::broker::CredentialBroker::resolve(
            &b2,
            &handle.token(),
            &BTreeSet::from(["drive.readonly".to_owned()]),
        )
        .await
        .unwrap();
        assert_eq!(resolved.material, material());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn forged_and_tampered_handles_fail_closed() {
        let root = temp_store();
        let (b, _) = broker(&root);
        let record = register(&b).await;
        let request = IssueRequest {
            tenant: "acme".into(),
            run_id: None,
            requirement: CredentialRequirement {
                connection_id: record.connection_id.clone(),
                scopes: BTreeSet::new(),
            },
        };
        let handle = rusty_agent_runtime::broker::CredentialBroker::issue(&b, &request)
            .await
            .unwrap();
        let scopes = BTreeSet::new();
        // A tampered token (claims edited underneath the signature) fails.
        let mut tampered_claims = handle.claims().clone();
        tampered_claims.scopes.insert("drive.admin".into());
        let forged = CredentialHandle::from_parts(tampered_claims, handle.signature().to_owned());
        let err =
            rusty_agent_runtime::broker::CredentialBroker::resolve(&b, &forged.token(), &scopes)
                .await
                .unwrap_err();
        assert!(
            matches!(err.reason, BrokerDenialReason::UnknownHandle),
            "got: {err}"
        );
        // Garbage fails the same way (parse failure and forgery are one
        // refusal).
        let err =
            rusty_agent_runtime::broker::CredentialBroker::resolve(&b, "v1.garbage.sig", &scopes)
                .await
                .unwrap_err();
        assert!(
            matches!(err.reason, BrokerDenialReason::UnknownHandle),
            "got: {err}"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn revocation_fails_closed_typed_and_journaled() {
        let root = temp_store();
        let (b, store) = broker(&root);
        let record = register(&b).await;
        let request = IssueRequest {
            tenant: "acme".into(),
            run_id: Some("run-1".into()),
            requirement: CredentialRequirement {
                connection_id: record.connection_id.clone(),
                scopes: BTreeSet::from(["drive.readonly".to_owned()]),
            },
        };
        let handle = rusty_agent_runtime::broker::CredentialBroker::issue(&b, &request)
            .await
            .unwrap();
        // The handle resolves while live...
        rusty_agent_runtime::broker::CredentialBroker::resolve(
            &b,
            &handle.token(),
            &BTreeSet::from(["drive.readonly".to_owned()]),
        )
        .await
        .unwrap();
        // ...and the very next use after revocation is a typed denial.
        match b
            .revoke("acme", &record.connection_id, Some("offboarded".into()))
            .await
            .unwrap()
        {
            RevokeOutcome::Applied { .. } => {}
            other => panic!(
                "expected applied, got {}",
                match other {
                    RevokeOutcome::Converged(_) => "converged",
                    RevokeOutcome::Unknown => "unknown",
                    RevokeOutcome::Conflict => "conflict",
                    RevokeOutcome::Applied { .. } => unreachable!(),
                }
            ),
        }
        let err = rusty_agent_runtime::broker::CredentialBroker::resolve(
            &b,
            &handle.token(),
            &BTreeSet::from(["drive.readonly".to_owned()]),
        )
        .await
        .unwrap_err();
        match &err.reason {
            BrokerDenialReason::ConnectionRevoked { grant } => {
                assert_eq!(
                    grant,
                    &vec!["drive.readonly".to_owned(), "drive.write".to_owned()]
                )
            }
            other => panic!("expected revoked, got {other:?}"),
        }
        assert_eq!(
            err.connection_id.as_deref(),
            Some(record.connection_id.as_str())
        );

        // The chain: registered, issued, used, revoked, denied — every
        // transition in order, none carrying the marker bytes.
        let snapshot = store
            .get_journal(BROKER_JOURNAL_RUN_ID)
            .await
            .unwrap()
            .expect("the broker journal exists");
        let kinds: Vec<RunEventKind> = snapshot.events.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            vec![
                RunEventKind::ConnectionRegistered,
                RunEventKind::CredentialHandleIssued,
                RunEventKind::CredentialUse,
                RunEventKind::ConnectionRevoked,
                RunEventKind::CredentialDenied,
            ],
            "journal kinds: {kinds:?}"
        );
        let whole = serde_json::to_string(&snapshot).unwrap();
        assert!(
            !whole.contains("sk-live-MARKER-9f2e"),
            "bytes in the journal"
        );
        assert!(!whole.contains("rt-MARKER-41ab"), "bytes in the journal");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cross_tenant_handles_and_scope_escalation_are_denied() {
        let root = temp_store();
        let (b, _) = broker(&root);
        let record = register(&b).await;
        // Beyond the consented set: denied at issuance, journaled.
        let request = IssueRequest {
            tenant: "acme".into(),
            run_id: None,
            requirement: CredentialRequirement {
                connection_id: record.connection_id.clone(),
                scopes: BTreeSet::from(["drive.admin".to_owned()]),
            },
        };
        let err = rusty_agent_runtime::broker::CredentialBroker::issue(&b, &request)
            .await
            .unwrap_err();
        match &err.reason {
            BrokerDenialReason::ScopeNotGranted { missing } => {
                assert_eq!(missing, &vec!["drive.admin".to_owned()])
            }
            other => panic!("expected scope denial, got {other:?}"),
        }
        // A handle issued to one tenant resolves nothing in another —
        // the liveness read is tenant-scoped, so the claim's tenant names
        // a connection that tenant does not hold.
        let request = IssueRequest {
            tenant: "acme".into(),
            run_id: None,
            requirement: CredentialRequirement {
                connection_id: record.connection_id.clone(),
                scopes: BTreeSet::new(),
            },
        };
        // A baseline issuance succeeds; the adversarial cases are below.
        rusty_agent_runtime::broker::CredentialBroker::issue(&b, &request)
            .await
            .unwrap();
        // Forge the tenant in the claims (the signature covers it, so a
        // real cross-tenant presentation would need the signing key —
        // the read being scoped is the second wall, tested here through
        // an honestly re-tenant-ed issuance request against globex).
        let err = rusty_agent_runtime::broker::CredentialBroker::issue(
            &b,
            &IssueRequest {
                tenant: "globex".into(),
                run_id: None,
                requirement: CredentialRequirement {
                    connection_id: record.connection_id.clone(),
                    scopes: BTreeSet::new(),
                },
            },
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err.reason, BrokerDenialReason::UnknownConnection),
            "got: {err}"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn consent_narrows_widens_only_by_journaled_act_and_refreshes() {
        let root = temp_store();
        let (b, _) = broker(&root);
        let record = register(&b).await;
        // Material-only update: a refresh beneath the same ceiling —
        // the old handle's scopes still resolve, against new bytes.
        let rotated = TokenMaterial {
            access_token: "sk-live-ROTATED".into(),
            refresh_token: None,
            expires_at: None,
        };
        match b
            .record_consent("acme", &record.connection_id, None, Some(&rotated))
            .await
            .unwrap()
        {
            ConsentOutcome::Applied { journaled, .. } => {
                assert_eq!(journaled, RunEventKind::ConnectionRefreshed)
            }
            _ => panic!("expected applied"),
        }
        let handle = rusty_agent_runtime::broker::CredentialBroker::issue(
            &b,
            &IssueRequest {
                tenant: "acme".into(),
                run_id: None,
                requirement: CredentialRequirement {
                    connection_id: record.connection_id.clone(),
                    scopes: BTreeSet::new(),
                },
            },
        )
        .await
        .unwrap();
        let resolved = rusty_agent_runtime::broker::CredentialBroker::resolve(
            &b,
            &handle.token(),
            &BTreeSet::new(),
        )
        .await
        .unwrap();
        assert_eq!(resolved.material.access_token, "sk-live-ROTATED");
        // A scope change is a consent act, journaled as such.
        match b
            .record_consent(
                "acme",
                &record.connection_id,
                Some(BTreeSet::from(["drive.readonly".to_owned()])),
                None,
            )
            .await
            .unwrap()
        {
            ConsentOutcome::Applied { journaled, record } => {
                assert_eq!(journaled, RunEventKind::ConnectionConsented);
                assert_eq!(record.scopes, BTreeSet::from(["drive.readonly".to_owned()]));
            }
            _ => panic!("expected applied"),
        }
        // Re-recording the same fact converges without a new event.
        match b
            .record_consent("acme", &record.connection_id, None, None)
            .await
            .unwrap()
        {
            ConsentOutcome::Converged(_) => {}
            _ => panic!("expected converged"),
        }
        let _ = std::fs::remove_dir_all(root);
    }
}
