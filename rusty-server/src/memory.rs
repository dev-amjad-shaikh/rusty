//! Governed memory persistence (R0.8 Rusty Learn, wave 1): the file
//! layout behind the memory store backends, plus the content
//! spill/resolve discipline both backends share.
//!
//! Records follow the assistants/agents conventions exactly: one JSON
//! file per record under `{store_path}/memory/` on the default backend
//! (tenant-scoped content addresses live one directory deeper —
//! `memory/{tenant}/{address}.json`), a column-mapped `server_memory`
//! table on Postgres (retrieval filters on real columns; the record
//! itself travels as JSONB).
//!
//! **Content spill.** A record whose content exceeds
//! [`rusty_agent_runtime::journal::INLINE_PAYLOAD_MAX_BYTES`] carries a
//! content-addressed artifact reference instead of the body (core's
//! payload discipline, shared with the journal). The body bytes live in
//! the artifact store shared with journal payloads — `FileArtifactStore`
//! under `{store_path}/memory_artifacts/` on the file backend, core's
//! `PostgresArtifactStore` (`rusty_artifacts` table) on Postgres — and
//! reads **re-inline** them, so served records are always self-contained.
//! Re-inlining is identity-preserving: the content address hashes the
//! content, not its reference form. `memory_artifacts` is a sibling of
//! `memory/`, not a child, so the recursive record loader never mistakes
//! a blob for a record.
//!
//! Tenancy rides the content address the same way it rides agent ids:
//! the store keys records by `{tenant}/{address}` (unprefixed for the
//! default tenant), and the file loader derives that key from the
//! record's *path* — the record itself carries the bare address, because
//! its identity is tenant-neutral content plus provenance.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use rusty_agent_runtime::journal::{ArtifactStore, FileArtifactStore};
use rusty_agent_runtime::memory::MemoryRecord;
use rusty_agent_runtime::record::PayloadRef;
use serde_json::Value;

/// The memory directory under the store root. `memory` is a reserved
/// layout name (see [`crate::RESERVED_NAMES`]): client-chosen thread ids
/// may not claim it.
pub(crate) fn dir(root: &Path) -> PathBuf {
    root.join("memory")
}

/// The artifact directory holding spilled memory bodies under the store
/// root (`memory_artifacts` is likewise reserved). A sibling of
/// [`dir`], deliberately: the recursive record loader walks `memory/`,
/// and blob files must never land inside it.
pub(crate) fn artifacts_dir(root: &Path) -> PathBuf {
    root.join("memory_artifacts")
}

/// The artifact store spilled memory bodies persist through (file
/// backend): content-addressed, integrity-verified on read — the same
/// store the journal uses for oversized payloads, at its own root.
pub(crate) fn artifact_store(root: &Path) -> FileArtifactStore {
    FileArtifactStore::new(artifacts_dir(root))
}

/// Persist one record atomically (temp file + rename) under `dir`,
/// named by `scoped_id` — the durability discipline every file record
/// in the server shares (the `agents::persist_record` pattern): a crash
/// mid-write must never leave a truncated record behind. The id may
/// carry a `{tenant}/` prefix, so the parent directory is created, not
/// just the flat dir.
pub(crate) async fn persist(root: &Path, scoped_id: &str, record: &MemoryRecord) -> io::Result<()> {
    let dir = dir(root);
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

/// Remove one record file (forgetting, R0.8 wave 2): real deletion of
/// derived state. `false` when the file was already gone — a missing file
/// is not an error, because the store's index is the authority on whether
/// the record was held at all. Spilled bodies under `memory_artifacts/`
/// deliberately stay: they are shared, content-addressed blobs (another
/// record may reference the same bytes), and the journal-erasure boundary
/// (design open question 4) classes them with evidence, not derived
/// state.
pub(crate) async fn remove(root: &Path, scoped_id: &str) -> Result<bool, String> {
    let path = dir(root).join(format!("{scoped_id}.json"));
    match tokio::fs::remove_file(&path).await {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("remove memory record {}: {e}", path.display())),
    }
}

/// Recursively collect `*.json` files under `root` (tenant
/// subdirectories hold that tenant's records), mirroring the agents
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

/// Load all records under `dir`, keyed by their **path-derived scoped
/// id** (`{tenant}/{address}` for named tenants, the bare address for
/// the default tenant) — the record body carries only the bare content
/// address, so the key must come from where the file lives. Files that
/// fail to parse are skipped with a warning (the agents loader's
/// corrupt-tolerance rule): one bad record must not take the namespace
/// down at boot.
pub(crate) fn load(root: &Path) -> HashMap<String, MemoryRecord> {
    let dir = dir(root);
    let mut out = HashMap::new();
    let mut files = Vec::new();
    collect_json_files(&dir, &mut files);
    for path in files {
        let scoped_id = path
            .strip_prefix(&dir)
            .ok()
            .map(|relative| relative.with_extension(""))
            .map(|relative| {
                relative
                    .components()
                    .map(|component| component.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/")
            });
        let parsed = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<MemoryRecord>(&raw).ok());
        match (scoped_id, parsed) {
            (Some(id), Some(record)) => {
                out.insert(id, record);
            }
            _ => {
                tracing::warn!(path = %path.display(), "skipping unreadable memory file")
            }
        }
    }
    out
}

/// Spill a record's content into `artifacts` when the record carries an
/// artifact reference: the bytes are the canonical serialization of
/// `content` (the value [`MemoryRecord`] minted the address from), and
/// the store recomputes the address on put — a mismatch with the
/// record's reference is corruption, reported, never silently stored.
/// Inline content needs nothing.
pub(crate) async fn spill_content(
    artifacts: &(dyn ArtifactStore + '_),
    record: &MemoryRecord,
    content: &Value,
) -> Result<(), String> {
    let PayloadRef::Artifact(reference) = &record.content else {
        return Ok(());
    };
    let bytes = serde_json::to_vec(content)
        .map_err(|e| format!("serialize memory content for artifact spill: {e}"))?;
    let stored = artifacts
        .put(&bytes)
        .await
        .map_err(|e| format!("persist memory artifact: {e}"))?;
    if stored.sha256 != reference.sha256 {
        return Err(format!(
            "memory artifact address mismatch: the record references {} but the content \
             hashes to {} — the content is not the value the address was minted from",
            reference.sha256, stored.sha256
        ));
    }
    Ok(())
}

/// Re-inline a record's artifact-referenced content, so served records
/// are self-contained: fetch the addressed bytes (integrity-verified by
/// the store — the returned bytes re-hash to the address or the read
/// fails) and replace the reference with the inline body. The content
/// address is unaffected (it hashes the content, not the reference
/// form). Inline content returns unchanged.
pub(crate) async fn resolve_content(
    artifacts: &(dyn ArtifactStore + '_),
    record: &mut MemoryRecord,
) -> Result<(), String> {
    let PayloadRef::Artifact(reference) = &record.content else {
        return Ok(());
    };
    let bytes = artifacts
        .get(&reference.sha256)
        .await
        .map_err(|e| format!("resolve memory artifact {}: {e}", reference.sha256))?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|e| {
        format!(
            "memory artifact {} did not hold a JSON value: {e}",
            reference.sha256
        )
    })?;
    record.content = PayloadRef::Inline(value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use rusty_agent_runtime::memory::{
        MemoryKind, MemoryProvenance, MemoryScope, ProvenanceAuthor, ScopeAddress, ValidityWindow,
    };
    use serde_json::json;

    fn record(content: Value) -> MemoryRecord {
        let provenance = MemoryProvenance {
            author: ProvenanceAuthor::Human {
                human_id: "amjad".into(),
            },
            evidence: Default::default(),
            written_at: Utc::now(),
        };
        MemoryRecord::new(
            MemoryKind::Fact,
            ScopeAddress::new(MemoryScope::User, "user-7"),
            provenance,
            1.0,
            ValidityWindow::starting(Utc::now()),
            Utc::now(),
            content,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn records_round_trip_keyed_by_path_with_corrupt_tolerance() {
        let root = std::env::temp_dir().join(format!("rusty-memory-test-{}", uuid::Uuid::new_v4()));
        let small = record(json!({"timezone": "Asia/Dubai"}));
        persist(&root, &small.memory_id, &small).await.unwrap();
        let tenant_record = record(json!({"team": "qa"}));
        persist(
            &root,
            &format!("acme/{}", tenant_record.memory_id),
            &tenant_record,
        )
        .await
        .unwrap();
        std::fs::write(dir(&root).join("broken.json"), b"{nope").unwrap();

        let loaded = load(&root);
        assert_eq!(loaded.len(), 2, "corrupt files are skipped, not fatal");
        assert!(
            loaded.contains_key(&small.memory_id),
            "default tenant: bare key"
        );
        let scoped = format!("acme/{}", tenant_record.memory_id);
        assert_eq!(
            loaded[&scoped].memory_id, tenant_record.memory_id,
            "named tenant: the key comes from the path, the record keeps the bare address"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn artifact_spill_and_resolve_keep_the_record_self_contained() {
        let root = std::env::temp_dir().join(format!("rusty-memory-test-{}", uuid::Uuid::new_v4()));
        // A body past the inline threshold spills on construction.
        let big = json!({"blob": "x".repeat(9000)});
        let record = record(big.clone());
        assert!(matches!(record.content, PayloadRef::Artifact(_)));

        let artifacts = artifact_store(&root);
        spill_content(&artifacts, &record, &big).await.unwrap();
        let mut resolved = record.clone();
        resolve_content(&artifacts, &mut resolved).await.unwrap();
        assert_eq!(resolved.content, PayloadRef::Inline(big));
        assert_eq!(
            resolved.memory_id, record.memory_id,
            "the address survives resolution"
        );

        // The blob lives under memory_artifacts/, never inside memory/.
        assert!(artifacts_dir(&root).exists());
        assert!(!dir(&root).exists(), "no record was persisted here");
        let _ = std::fs::remove_dir_all(root);
    }
}
