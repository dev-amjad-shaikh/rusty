//! Configuration-registry persistence (R0.11 Extension Plane, wave 1):
//! the file layout behind the artifact store backend.
//!
//! One directory under `{store_path}/registry/` (`registry` is a reserved
//! layout name, see [`crate::RESERVED_NAMES`]):
//!
//! - `artifacts/` holds one JSON file per
//!   [`ArtifactRecord`](rusty_agent_runtime::registry::ArtifactRecord).
//!   Artifact keys are surface keys — they contain `:` (`prompt:system`,
//!   tenant-prefixed surfaces), so the file is named by the key's SHA-256
//!   and the file body is an envelope carrying the true key — the version
//!   pointer layout's rule ([`crate::learn`]) applied to the registry's
//!   one new persisted entity. The record is rewritten on every commit,
//!   so the temp-write-then-rename discipline is what makes a crash
//!   mid-commit safe.
//!
//! Postgres keeps the same entity column-mapped
//! (`server_registry_artifacts`), with the commit append compare-and-
//! swapped inside one transaction, so a crash cannot leave a committed
//! candidate whose artifact history never grew (or the inverse).

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use rusty_agent_runtime::record::sha256_hex;
use rusty_agent_runtime::registry::ArtifactRecord;
use serde::{Deserialize, Serialize};

/// The artifact directory under the store root
/// (`{store_path}/registry/artifacts`).
pub(crate) fn artifacts_dir(root: &Path) -> PathBuf {
    root.join("registry").join("artifacts")
}

/// The artifact file's body: the record plus the scoped surface key it
/// was written under. The key travels in the body because the filename is
/// the key's hash — surface keys are not path-safe, and a one-way
/// filename needs the true key recorded somewhere (the version-pointer
/// envelope's rule, verbatim).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ArtifactFile {
    /// The tenant-scoped surface key (`{tenant}/prompt:system` for named
    /// tenants).
    key: String,
    /// The record itself.
    record: ArtifactRecord,
}

/// The file name for a scoped surface key: its SHA-256 hex — hashing
/// (rather than escaping) keeps every surface inside one fixed-shape,
/// collision-checked namespace; the envelope key check on load is what
/// catches a collision or a forged name.
fn artifact_file_name(scoped_key: &str) -> String {
    sha256_hex(scoped_key.as_bytes())
}

/// Persist one artifact record atomically (temp file + rename), named by
/// the scoped surface key's hash. The record is rewritten on every
/// commit — the most-rewritten file in this layout — so this is the write
/// whose crash-safety the temp+rename discipline buys.
pub(crate) async fn persist_artifact(
    root: &Path,
    scoped_key: &str,
    record: &ArtifactRecord,
) -> io::Result<()> {
    let dir = artifacts_dir(root);
    tokio::fs::create_dir_all(&dir).await?;
    let file = ArtifactFile {
        key: scoped_key.to_string(),
        record: record.clone(),
    };
    let bytes = serde_json::to_vec_pretty(&file)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let name = artifact_file_name(scoped_key);
    let tmp = dir.join(format!("{name}.tmp"));
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, dir.join(format!("{name}.json"))).await
}

/// Recursively collect `*.json` files under `root` — the candidate
/// loader's rule, kept local so this module's layout is self-describing.
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

/// Load all artifact records under `artifacts_dir`, keyed by the scoped
/// surface key carried in each file's envelope. A file whose envelope key
/// does not hash back to its filename is corrupt (or a hash collision)
/// and is skipped with a warning, same as an unparseable file: the
/// registry must never serve a record under a key it was not written
/// under — the version-pointer loader's fail-closed rule.
pub(crate) fn load_artifacts(root: &Path) -> HashMap<String, ArtifactRecord> {
    let dir = artifacts_dir(root);
    let mut out = HashMap::new();
    let mut files = Vec::new();
    collect_json_files(&dir, &mut files);
    for path in files {
        let parsed = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<ArtifactFile>(&raw).ok());
        let matches_name = parsed.as_ref().is_some_and(|file| {
            path.file_stem().and_then(|s| s.to_str()) == Some(&*artifact_file_name(&file.key))
        });
        match (parsed, matches_name) {
            (Some(file), true) => {
                out.insert(file.key, file.record);
            }
            _ => {
                tracing::warn!(path = %path.display(), "skipping unreadable artifact file")
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use rusty_agent_runtime::learn::CandidateKind;
    use rusty_agent_runtime::memory::ProvenanceAuthor;
    use rusty_agent_runtime::registry::ArtifactCommit;

    fn ts(millis: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp_millis(millis).unwrap()
    }

    fn artifact() -> ArtifactRecord {
        let mut record = ArtifactRecord::new(
            CandidateKind::Prompt,
            "system",
            ProvenanceAuthor::Human {
                human_id: "amjad".into(),
            },
            ts(1_760_000_000_000),
        )
        .unwrap();
        record.commits.push(ArtifactCommit {
            candidate_id: rusty_agent_runtime::learn::CandidateId::from("a".repeat(64)),
            committed_at: ts(1_760_000_001_000),
        });
        record
    }

    #[tokio::test]
    async fn artifacts_round_trip_through_hashed_filenames() {
        let root =
            std::env::temp_dir().join(format!("rusty-registry-test-{}", uuid::Uuid::new_v4()));
        let artifact = artifact();
        let scoped = format!("acme/{}", artifact.surface.as_str());
        persist_artifact(&root, &scoped, &artifact).await.unwrap();

        // The filename is the key's hash — the raw surface (with its
        // `:`) appears nowhere in the directory listing.
        let listing: Vec<String> = std::fs::read_dir(artifacts_dir(&root))
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            listing,
            vec![format!("{}.json", sha256_hex(scoped.as_bytes()))]
        );

        let loaded = load_artifacts(&root);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[&scoped], artifact);

        // An envelope whose key does not hash back to its filename is
        // corrupt (or a collision) and skipped, not served — the
        // version-pointer loader's fail-closed rule.
        std::fs::write(
            artifacts_dir(&root).join(format!("{}.json", sha256_hex(b"forged-name"))),
            serde_json::to_vec_pretty(&ArtifactFile {
                key: "prompt:other".into(),
                record: artifact.clone(),
            })
            .unwrap(),
        )
        .unwrap();
        let loaded = load_artifacts(&root);
        assert_eq!(loaded.len(), 1, "the forged-name file is skipped");
        let _ = std::fs::remove_dir_all(root);
    }
}
