//! Learning-candidate persistence (R0.8 Rusty Learn, wave 3): the file
//! layout behind the candidate and version-pointer store backends.
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
