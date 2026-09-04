//! Threads: durable records binding a conversation id to a registered graph.
//!
//! A thread is `{graph, metadata}` plus its owning tenant (design doc §8,
//! open question 2 — per-thread binding). Records are persisted as one JSON
//! file per thread under `{store_path}/threads/{thread_id}.json` and reloaded
//! when the router is built. Persistence is what makes the checkpoint
//! durability story reachable through the API: checkpoints are keyed by
//! thread id, so a restart that forgot the thread records would 404 every
//! pre-restart thread while its checkpoints sat orphaned on disk.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::auth::scope_id;

/// One thread: a conversation/session bound to one registered graph at
/// creation time.
///
/// `thread_id` is always the external id clients see on the wire; the map
/// key and file name use the **internal** id (`{tenant}/{thread_id}`, the
/// default tenant unprefixed — see [`crate::auth`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ThreadRecord {
    pub thread_id: String,
    /// Owning tenant (resolved from the API key at creation time).
    pub tenant: String,
    pub graph: String,
    #[serde(default)]
    pub metadata: Value,
    /// Parent thread id when this thread was forked from another.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<String>,
    /// Number of checkpoints copied from the parent at fork time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed_length: Option<usize>,
    pub created_at: DateTime<Utc>,
}

/// The internal id a record is keyed and filed under.
pub(crate) fn internal_id(record: &ThreadRecord) -> String {
    scope_id(&record.tenant, &record.thread_id)
}

/// The on-disk directory holding one JSON file per thread.
pub(crate) fn dir(store_root: &Path) -> PathBuf {
    store_root.join("threads")
}

/// Load all persisted threads, skipping (with a warning) any file that
/// fails to parse. Tenant-scoped threads live one directory deeper
/// (`threads/{tenant}/{thread_id}.json`), so the walk is recursive.
pub(crate) fn load(store_root: &Path) -> HashMap<String, ThreadRecord> {
    let mut out = HashMap::new();
    let mut files = Vec::new();
    collect_json_files(&dir(store_root), &mut files);
    for path in files {
        let parsed = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<ThreadRecord>(&raw).ok());
        match parsed {
            Some(record) => {
                out.insert(internal_id(&record), record);
            }
            None => {
                tracing::warn!(path = %path.display(), "skipping unreadable thread file")
            }
        }
    }
    out
}

/// Recursively collect `*.json` files under `root` (tenant subdirectories
/// hold that tenant's records).
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

/// Persist one thread record under its internal id. The id may carry a
/// `{tenant}/` prefix, so the parent directory is created, not just the
/// flat threads dir.
pub(crate) async fn persist(
    store_root: &Path,
    internal_id: &str,
    record: &ThreadRecord,
) -> std::io::Result<()> {
    let path = dir(store_root).join(format!("{internal_id}.json"));
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let raw = serde_json::to_vec_pretty(record).expect("thread serialization is infallible");
    tokio::fs::write(path, raw).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Unique temp root under the OS temp dir, removed on drop.
    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!("rusty-threads-test-{}", uuid::Uuid::new_v4())))
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn persist_then_load_survives_a_restart() {
        let tmp = TestDir::new();
        let default_record = ThreadRecord {
            thread_id: "t-1".to_string(),
            tenant: "default".to_string(),
            graph: "pipeline".to_string(),
            metadata: json!({"origin": "test"}),
            forked_from: None,
            seed_length: None,
            created_at: Utc::now(),
        };
        let tenant_record = ThreadRecord {
            thread_id: "t-2".to_string(),
            tenant: "acme".to_string(),
            graph: "pipeline".to_string(),
            metadata: Value::Null,
            forked_from: None,
            seed_length: None,
            created_at: Utc::now(),
        };
        persist(&tmp.0, &internal_id(&default_record), &default_record)
            .await
            .unwrap();
        persist(&tmp.0, &internal_id(&tenant_record), &tenant_record)
            .await
            .unwrap();

        // A fresh load stands in for a process restart.
        let loaded = load(&tmp.0);
        assert_eq!(loaded.len(), 2);
        // The default tenant stays unprefixed; named tenants are keyed by
        // their internal id so isolation survives the reload.
        assert_eq!(loaded["t-1"].graph, "pipeline");
        assert_eq!(loaded["t-1"].metadata, json!({"origin": "test"}));
        assert_eq!(loaded["acme/t-2"].tenant, "acme");
    }

    #[tokio::test]
    async fn load_skips_unreadable_files() {
        let tmp = TestDir::new();
        let threads_dir = dir(&tmp.0);
        std::fs::create_dir_all(&threads_dir).unwrap();
        std::fs::write(threads_dir.join("broken.json"), b"not json").unwrap();
        assert!(load(&tmp.0).is_empty());
    }

    #[tokio::test]
    async fn fork_lineage_round_trips() {
        let tmp = TestDir::new();
        let fork_record = ThreadRecord {
            thread_id: "fork-1".to_string(),
            tenant: "default".to_string(),
            graph: "pipeline".to_string(),
            metadata: Value::Null,
            forked_from: Some("parent-1".to_string()),
            seed_length: Some(3),
            created_at: Utc::now(),
        };
        persist(&tmp.0, &internal_id(&fork_record), &fork_record)
            .await
            .unwrap();

        let loaded = load(&tmp.0);
        let rec = &loaded["fork-1"];
        assert_eq!(rec.forked_from.as_deref(), Some("parent-1"));
        assert_eq!(rec.seed_length, Some(3));
    }
}
