//! The capsule registry (R0.9 Rusty Capsules, wave 1): the server-side
//! record, the JSON-file layout, and the pin-resolution rule.
//!
//! The registry stores immutable capsule manifests under their content
//! address — the golden-pinned contract half of the plane
//! (`CapsuleManifest`, `derive_capsule_id`, the capability taxonomy)
//! lives in `rusty_agent_runtime::capsule`; this module is the operator
//! face that maps `(identity, version)` pins onto those addresses. The
//! discipline is the policy plane's ([`crate::policy`]):
//!
//! - **Registry record** ([`CapsuleRecord`]) — one immutable manifest
//!   plus its registration instant. Immutability is enforced at the write
//!   seam ([`crate::server_store::ServerStore::put_capsule`]): the same id
//!   naming the same manifest converges (idempotent create); a `(name,
//!   version)` pin already taken by a different id conflicts — a version
//!   pin is a commitment to one exact declaration.
//! - **The file layout** — one directory under `{store_path}/capsules/`
//!   (`capsules` is a reserved layout name, see
//!   [`crate::RESERVED_NAMES`]), one JSON file per record, temp-write-
//!   then-rename, path-keyed tenancy (`{tenant}/{capsule_id}.json` for
//!   named tenants, bare ids for the default tenant). Postgres keeps the
//!   same entity column-mapped (`server_capsules`), with the pin
//!   uniqueness the file backend checks by scan enforced as a UNIQUE
//!   index on `(tenant, identity, version)`.
//! - **Resolution** — routes resolve a run's capsule pins against the
//!   registry and journal one `CapsuleResolved` event per pin; the
//!   resolution re-derives the stored manifest's content address before
//!   answering, so a tampered store record fails closed (`422`) rather
//!   than resolving a pin to bytes that were never admitted.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use rusty_agent_runtime::capsule::{CapsuleId, CapsuleManifest};
use serde::{Deserialize, Serialize};

/// One immutable capsule manifest in the registry. Immutability is
/// enforced at the write seam
/// ([`crate::server_store::ServerStore::put_capsule`]): the same content
/// address naming the same manifest converges (the idempotent create),
/// and a `(name, version)` pin is claimed exactly once — re-registration
/// of a changed declaration under an already-claimed pin conflicts, so a
/// version string stays a commitment to one exact manifest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct CapsuleRecord {
    /// The manifest's content address — the registry's key.
    pub capsule_id: CapsuleId,

    /// The admitted manifest (validated at registration: the store holds
    /// only well-formed, well-addressed declarations).
    pub manifest: CapsuleManifest,

    /// When the manifest was first registered (a converged
    /// re-registration keeps the original instant — the record is
    /// immutable).
    pub registered_at: DateTime<Utc>,
}

/// The result of a capsule registration
/// ([`ServerStore::put_capsule`](crate::server_store::ServerStore::put_capsule)):
/// created, converged (the id already names exactly this manifest — the
/// idempotent create), or conflicted (the id names a different manifest
/// — a content-address collision — or the `(name, version)` pin is
/// already claimed by a different id). Registry immutability refuses
/// both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CapsuleWrite {
    /// The id is new; the manifest is stored.
    Created,
    /// The id already named exactly this manifest; nothing changed.
    Converged,
    /// The id names a different manifest, or the `(name, version)` pin
    /// is claimed by another id. Refused — immutability is what makes a
    /// version pin a commitment.
    Conflict,
}

// --------------------------------------------------------------------- //
// The JSON-file layout
// --------------------------------------------------------------------- //

/// The registry directory under the store root
/// (`{store_path}/capsules/manifests`).
pub(crate) fn manifests_dir(root: &Path) -> PathBuf {
    root.join("capsules").join("manifests")
}

/// Persist one capsule record atomically (temp file + rename) under
/// `manifests_dir`, named by `scoped_id` (`{tenant}/{capsule_id}` —
/// capsule ids are 64-hex digests, already path-safe) — the durability
/// discipline every file record in the server shares (the
/// `learn::persist_candidate` pattern).
pub(crate) async fn persist_capsule(
    root: &Path,
    scoped_id: &str,
    record: &CapsuleRecord,
) -> io::Result<()> {
    let dir = manifests_dir(root);
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
/// subdirectories hold that tenant's records) — the loader walk every
/// layout here shares.
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
/// (`{tenant}/{capsule_id}` for named tenants, the bare id for the
/// default tenant) — the path-keyed tenancy rule every layout here
/// shares: the record body carries the bare content address, so the
/// tenancy must come from where the file lives.
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

/// Load all capsule records under `manifests_dir`, keyed by their
/// path-derived scoped id. Files that fail to parse are skipped with a
/// warning (the corrupt-tolerance rule every loader here shares): one
/// bad record must not take the registry down at boot — and resolution
/// re-derives addresses, so a hand-edited record that *does* parse still
/// fails closed at resolve time.
pub(crate) fn load_capsules(root: &Path) -> HashMap<String, CapsuleRecord> {
    let dir = manifests_dir(root);
    let mut out = HashMap::new();
    let mut files = Vec::new();
    collect_json_files(&dir, &mut files);
    for path in files {
        let scoped_id = path_scoped_id(&dir, &path);
        let parsed = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<CapsuleRecord>(&raw).ok());
        match (scoped_id, parsed) {
            (Some(id), Some(record)) => {
                out.insert(id, record);
            }
            _ => {
                tracing::warn!(path = %path.display(), "skipping unreadable capsule file")
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusty_agent_runtime::capsule::{
        CapabilityGrant, CapsuleIdentity, CapsuleInterface, ResourceBudget, WORLD_V1,
    };
    use rusty_agent_runtime::record::{sha256_hex, Effect};
    use std::collections::BTreeSet;

    fn ts(millis: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp_millis(millis).unwrap()
    }

    fn record(name: &str) -> CapsuleRecord {
        let manifest = CapsuleManifest {
            identity: CapsuleIdentity {
                name: name.into(),
                description: None,
            },
            version: "0.1.0".into(),
            build_digest: sha256_hex(name.as_bytes()),
            interface: CapsuleInterface {
                world: WORLD_V1.into(),
                input_schema: None,
                output_schema: None,
            },
            effects: BTreeSet::from([Effect::Pure]),
            capabilities: BTreeSet::from([CapabilityGrant::Clock]),
            budget: ResourceBudget::default(),
        };
        CapsuleRecord {
            capsule_id: manifest.capsule_id().unwrap(),
            manifest,
            registered_at: ts(1_000),
        }
    }

    #[tokio::test]
    async fn layout_round_trips_and_skips_corrupt_files() {
        let root =
            std::env::temp_dir().join(format!("rusty-capsules-test-{}", uuid::Uuid::new_v4()));
        let a = record("probe-a");
        let b = record("probe-b");
        persist_capsule(&root, a.capsule_id.as_str(), &a)
            .await
            .unwrap();
        let scoped_b = crate::auth::scope_id("acme", b.capsule_id.as_str());
        persist_capsule(&root, &scoped_b, &b).await.unwrap();
        std::fs::create_dir_all(manifests_dir(&root)).unwrap();
        std::fs::write(manifests_dir(&root).join("broken.json"), b"{nope").unwrap();

        let loaded = load_capsules(&root);
        assert_eq!(loaded.len(), 2, "corrupt files are skipped, not fatal");
        assert!(
            loaded.contains_key(a.capsule_id.as_str()),
            "default tenant: bare key"
        );
        assert!(loaded.contains_key(&scoped_b));
        let _ = std::fs::remove_dir_all(root);
    }
}
