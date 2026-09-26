//! The person vault — one key per person, and forgetting is destroying it.
//!
//! What a run keeps of a person — the words they said, the answers the
//! systems gave in their name — lives in records that outlive the run:
//! the accepted-run record and the journal. A backup carries those
//! records off the box. Deleting the records on the box does nothing to
//! the copies. So the records a person's runs write are *sealed* under
//! that person's key, the key stays out of the data archive (it travels
//! in its own, a person-keys archive an operator keeps apart), and
//! forgetting the person destroys the key: every sealed record, on the
//! box or in any data archive, is ciphertext without a key from then on.
//! A tombstone — the principal id, when, by whom, why — is what remains.
//!
//! The key is per person and per tenant, 32 random bytes, kept 0600 under
//! `{store}/keys/person.{tenant}.{principal}.secret`. The cipher is
//! XChaCha20-Poly1305 with the record's kind and id as associated data,
//! so a sealed journal cannot be passed off as another run's.
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use chacha20poly1305::aead::{Aead, KeyInit, OsRng, Payload};
use chacha20poly1305::{AeadCore, XChaCha20Poly1305, XNonce};
use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const CIPHER: &str = "xchacha20poly1305";
const FORMAT_VERSION: u32 = 1;

/// A record sealed for one person: what the file holds in place of the
/// record itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sealed {
    pub sealed_for: SealedFor,
    pub format_version: u32,
    pub cipher: String,
    pub nonce: String,
    pub ciphertext: String,
    pub sealed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealedFor {
    pub tenant: String,
    pub principal: String,
}

/// What remains of a forgotten person.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tombstone {
    pub tenant: String,
    pub principal: String,
    /// The display name at the time, when there was an account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub at: DateTime<Utc>,
    pub by: Value,
    pub reason: String,
    /// The identity provider's issuer and subject for the person, when
    /// the account came from one: what refuses them at the provider too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external: Option<(String, String)>,
}

fn key_stem(tenant: &str, principal: &str) -> String {
    format!("person.{tenant}.{principal}")
}

/// The keys of every person, and the tombstones of everyone forgotten.
pub struct PersonVault {
    keys_dir: PathBuf,
    keys: RwLock<HashMap<String, [u8; 32]>>,
    forgotten: RwLock<HashMap<String, Tombstone>>,
}

impl std::fmt::Debug for PersonVault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PersonVault").field("keys_dir", &self.keys_dir).finish_non_exhaustive()
    }
}

impl PersonVault {
    /// The vault under `store_root`, with every key and tombstone it holds.
    pub fn new(store_root: &Path) -> Self {
        let keys_dir = crate::receipts::keys_dir(store_root);
        let mut keys = HashMap::new();
        let mut forgotten = HashMap::new();
        if let Ok(entries) = std::fs::read_dir(&keys_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let Some(rest) = name.strip_prefix("person.") else { continue };
                if let Some(stem) = rest.strip_suffix(".secret") {
                    match std::fs::read(entry.path()).ok().and_then(|bytes| parse_key(&bytes)) {
                        Some(key) => {
                            keys.insert(format!("person.{stem}"), key);
                        }
                        None => tracing::warn!(path = %entry.path().display(), "skipping an unreadable person key file"),
                    }
                } else if let Some(stem) = rest.strip_suffix(".forgotten.json") {
                    if let Some(tombstone) = std::fs::read(entry.path()).ok().and_then(|b| serde_json::from_slice::<Tombstone>(&b).ok()) {
                        forgotten.insert(format!("person.{stem}"), tombstone);
                    }
                }
            }
        }
        Self { keys_dir, keys: RwLock::new(keys), forgotten: RwLock::new(forgotten) }
    }

    /// The files a person-keys archive carries: every live key.
    pub fn key_files(&self) -> Vec<PathBuf> {
        self.keys.read().map(|k| k.keys().map(|stem| self.keys_dir.join(format!("{stem}.secret"))).collect()).unwrap_or_default()
    }

    /// `true` for a path under `keys/` that is a person's key.
    pub fn is_key_file(path: &Path) -> bool {
        path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("person.") && n.ends_with(".secret"))
    }

    pub fn is_forgotten(&self, tenant: &str, principal: &str) -> bool {
        self.forgotten.read().map(|f| f.contains_key(&key_stem(tenant, principal))).unwrap_or(false)
    }

    /// Everyone forgotten, oldest first.
    pub fn forgotten(&self) -> Vec<Tombstone> {
        let mut out: Vec<Tombstone> = self.forgotten.read().map(|f| f.values().cloned().collect()).unwrap_or_default();
        out.sort_by_key(|a| a.at);
        out
    }

    fn key_for(&self, tenant: &str, principal: &str, create: bool) -> io::Result<Option<[u8; 32]>> {
        let stem = key_stem(tenant, principal);
        if let Some(key) = self.keys.read().ok().and_then(|k| k.get(&stem).copied()) {
            return Ok(Some(key));
        }
        if !create || self.is_forgotten(tenant, principal) {
            return Ok(None);
        }
        let mut guard = self.keys.write().map_err(|_| io::Error::other("person keys poisoned"))?;
        if let Some(key) = guard.get(&stem) {
            return Ok(Some(*key));
        }
        let mut key = [0u8; 32];
        use chacha20poly1305::aead::rand_core::RngCore;
        OsRng.fill_bytes(&mut key);
        std::fs::create_dir_all(&self.keys_dir)?;
        let path = self.keys_dir.join(format!("{stem}.secret"));
        let tmp = self.keys_dir.join(format!(".{stem}.{}.tmp", uuid::Uuid::new_v4()));
        std::fs::write(&tmp, hex(&key))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, &path)?;
        guard.insert(stem, key);
        tracing::info!(%tenant, %principal, "person key made");
        Ok(Some(key))
    }

    /// Seal `plaintext` for the person; `aad` names the record (kind and
    /// id) so the ciphertext cannot stand in for another record's. A
    /// forgotten person gets no new key: the seal is refused.
    pub fn seal(&self, tenant: &str, principal: &str, aad: &str, plaintext: &[u8]) -> io::Result<Sealed> {
        let key = self
            .key_for(tenant, principal, true)?
            .ok_or_else(|| io::Error::other(format!("`{principal}` was forgotten; nothing more is kept in their name")))?;
        let cipher = XChaCha20Poly1305::new((&key).into());
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let ciphertext = cipher
            .encrypt(&nonce, Payload { msg: plaintext, aad: aad.as_bytes() })
            .map_err(|_| io::Error::other("sealing failed"))?;
        Ok(Sealed {
            sealed_for: SealedFor { tenant: tenant.to_owned(), principal: principal.to_owned() },
            format_version: FORMAT_VERSION,
            cipher: CIPHER.to_owned(),
            nonce: hex(nonce.as_slice()),
            ciphertext: hex(&ciphertext),
            sealed_at: Utc::now(),
        })
    }

    /// The plaintext, or `None` when the person's key is gone — forgotten,
    /// or kept apart from this store.
    pub fn open(&self, sealed: &Sealed, aad: &str) -> io::Result<Option<Vec<u8>>> {
        let Some(key) = self.key_for(&sealed.sealed_for.tenant, &sealed.sealed_for.principal, false)? else {
            return Ok(None);
        };
        if sealed.cipher != CIPHER {
            return Err(io::Error::other(format!("sealed with `{}`, which this vault does not open", sealed.cipher)));
        }
        let nonce_bytes = unhex(&sealed.nonce).ok_or_else(|| io::Error::other("sealed nonce is not hex"))?;
        let ciphertext = unhex(&sealed.ciphertext).ok_or_else(|| io::Error::other("sealed ciphertext is not hex"))?;
        let nonce = XNonce::from_slice(&nonce_bytes);
        let cipher = XChaCha20Poly1305::new((&key).into());
        cipher
            .decrypt(nonce, Payload { msg: &ciphertext, aad: aad.as_bytes() })
            .map(Some)
            .map_err(|_| io::Error::other("the sealed record does not open: wrong key, or the record was moved"))
    }

    /// Forget: the key is destroyed and a tombstone takes its place.
    pub fn destroy(&self, tenant: &str, principal: &str, name: Option<String>, by: Value, reason: &str, external: Option<(String, String)>) -> io::Result<Tombstone> {
        let stem = key_stem(tenant, principal);
        let tombstone = Tombstone { tenant: tenant.to_owned(), principal: principal.to_owned(), name, at: Utc::now(), by, reason: reason.to_owned(), external };
        std::fs::create_dir_all(&self.keys_dir)?;
        let marker = self.keys_dir.join(format!("{stem}.forgotten.json"));
        std::fs::write(&marker, serde_json::to_vec_pretty(&tombstone)?)?;
        let path = self.keys_dir.join(format!("{stem}.secret"));
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        if let Ok(mut keys) = self.keys.write() {
            if let Some(mut key) = keys.remove(&stem) {
                key.iter_mut().for_each(|b| *b = 0);
            }
        }
        if let Ok(mut forgotten) = self.forgotten.write() {
            forgotten.insert(stem, tombstone.clone());
        }
        tracing::info!(%tenant, %principal, "person key destroyed");
        Ok(tombstone)
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    let text = text.trim();
    if text.len() % 2 != 0 {
        return None;
    }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok()).collect()
}

fn parse_key(bytes: &[u8]) -> Option<[u8; 32]> {
    let text = std::str::from_utf8(bytes).ok()?;
    let raw = unhex(text)?;
    raw.try_into().ok()
}

// ── Records on disk: plain when nobody's, sealed when a person's ─────────────

/// The bytes a file holds for `record`: the record itself, or — when it is
/// a person's — its sealed form.
pub fn record_bytes<T: Serialize>(vault: Option<&PersonVault>, person: Option<(&str, &str)>, kind: &str, id: &str, record: &T) -> io::Result<Vec<u8>> {
    let plain = serde_json::to_vec_pretty(record).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    match (vault, person) {
        (Some(vault), Some((tenant, principal))) => {
            let sealed = vault.seal(tenant, principal, &format!("{kind}:{id}"), &plain)?;
            serde_json::to_vec_pretty(&sealed).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
        }
        _ => Ok(plain),
    }
}

/// What a file's bytes hold: the record, or `None` when it is sealed for
/// a person whose key this store no longer has.
pub fn record_from_bytes<T: DeserializeOwned>(vault: Option<&PersonVault>, kind: &str, id: &str, bytes: &[u8]) -> io::Result<Option<T>> {
    let value: Value = serde_json::from_slice(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if value.get("sealed_for").is_some() {
        let sealed: Sealed = serde_json::from_value(value).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let Some(vault) = vault else {
            return Ok(None);
        };
        let Some(plain) = vault.open(&sealed, &format!("{kind}:{id}"))? else {
            tracing::debug!(kind, id, principal = %sealed.sealed_for.principal, "a sealed record has no key here; it reads as absent");
            return Ok(None);
        };
        return serde_json::from_slice(&plain).map(Some).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e));
    }
    serde_json::from_value(value).map(Some).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// `true` when the bytes are a sealed record (whoever's).
pub fn is_sealed(bytes: &[u8]) -> bool {
    serde_json::from_slice::<Value>(bytes).ok().is_some_and(|v| v.get("sealed_for").is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn vault() -> (PersonVault, PathBuf) {
        let root = std::env::temp_dir().join(format!("rusty-vault-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        (PersonVault::new(&root), root)
    }

    #[test]
    fn a_record_sealed_for_a_person_opens_with_their_key_and_not_after_the_key_is_destroyed() {
        let (vault, root) = vault();
        let record = json!({"run_id": "r1", "input": "Ana's words"});
        let bytes = record_bytes(Some(&vault), Some(("default", "ana")), "journal", "r1", &record).unwrap();
        assert!(is_sealed(&bytes));
        assert!(!String::from_utf8_lossy(&bytes).contains("Ana's words"));
        let back: Option<Value> = record_from_bytes(Some(&vault), "journal", "r1", &bytes).unwrap();
        assert_eq!(back, Some(record.clone()));
        // Another record's id does not open it.
        assert!(record_from_bytes::<Value>(Some(&vault), "journal", "r2", &bytes).is_err());
        // The key is on disk, 0600; a fresh vault over the same root opens it.
        let again = PersonVault::new(&root);
        assert_eq!(record_from_bytes::<Value>(Some(&again), "journal", "r1", &bytes).unwrap(), Some(record.clone()));
        // Destroyed: the record reads as absent, here and in any copy; the
        // tombstone remains; nothing new is sealed in their name.
        let tombstone = vault.destroy("default", "ana", Some("Ana".into()), json!({"principal_id": "ada"}), "left the company", None).unwrap();
        assert_eq!(tombstone.principal, "ana");
        assert!(vault.is_forgotten("default", "ana"));
        assert_eq!(record_from_bytes::<Value>(Some(&vault), "journal", "r1", &bytes).unwrap(), None);
        let later = PersonVault::new(&root);
        assert!(later.is_forgotten("default", "ana"));
        assert_eq!(record_from_bytes::<Value>(Some(&later), "journal", "r1", &bytes).unwrap(), None);
        assert!(vault.seal("default", "ana", "journal:r3", b"more").is_err());
        assert_eq!(vault.forgotten().len(), 1);
        // Nobody's record is plain and reads without a vault.
        let plain = record_bytes::<Value>(Some(&vault), None, "journal", "r9", &record).unwrap();
        assert!(!is_sealed(&plain));
        assert_eq!(record_from_bytes::<Value>(None, "journal", "r9", &plain).unwrap(), Some(record));
        let _ = std::fs::remove_dir_all(root);
    }
}
