//! Durable pending-run records: the store-backed half of the per-thread
//! run FIFO (the R1.0 "durable pending-run queue" gate).
//!
//! A run the `enqueue` multitask strategy parks behind the active run is
//! accepted work — the client holds its run id — yet it has no checkpoint
//! coverage: it never executed, so there is nothing to resume *from*. The
//! queue entry itself is what must survive a restart. Each queued run is
//! persisted as one JSON file per record under
//! `{store_path}/pending_runs/{run_id}.json` (the Postgres backend maps
//! this to the `server_pending_runs` table), written when the run lands
//! in the FIFO and deleted when the run leaves it — promoted to active,
//! cancelled while queued, or rejected. Boot replays the surviving
//! records back into the FIFO; see [`crate::runs::restore_pending_runs`].
//!
//! Writes are atomic (temp file + rename), mirroring the journals
//! discipline: a crash mid-write leaves the previous record or none,
//! never a torn one.

use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::runs::RunPayload;

/// Everything needed to re-schedule a queued run exactly as accepted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PendingRunRecord {
    /// Server-minted run id (UUID v4) — the client's handle on the run.
    /// Server-minted ids need no validation at this layer (the journals
    /// layout's rule).
    pub run_id: String,
    /// Internal (tenant-scoped) thread id: the scheduling key.
    pub thread_id: String,
    /// External thread id — the only form that may appear on the wire
    /// (SSE frames, terminal JSON).
    pub wire_thread_id: String,
    /// Owning tenant, resolved from the API key at submission; the
    /// restored run's admission re-resolves inside it.
    pub tenant: String,
    /// Registered graph name (the thread's binding at submission).
    pub graph: String,
    /// The accepted run payload, re-scheduled verbatim on restore.
    pub payload: RunPayload,
    /// FIFO position: a process-wide monotonic sequence assigned at
    /// enqueue. Ordering is only ever compared within one thread, but a
    /// single counter keeps the per-thread order unambiguous without a
    /// per-thread watermark that a restart would reset.
    pub seq: u64,
    /// Server acceptance time, carried into the restored run handle's
    /// `created_at` so a restored run keeps its original age.
    pub enqueued_at: DateTime<Utc>,
}

/// The pending-runs directory under the store root. `pending_runs` is a
/// reserved layout name (see [`crate::RESERVED_NAMES`]): client-chosen
/// thread ids may not claim it.
pub(crate) fn dir(root: &Path) -> PathBuf {
    root.join("pending_runs")
}

/// Persist `record`, replacing any earlier record of the same run.
pub(crate) async fn persist(root: &Path, record: &PendingRunRecord) -> io::Result<()> {
    let dir = dir(root);
    tokio::fs::create_dir_all(&dir).await?;
    let bytes = serde_json::to_vec_pretty(record)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let path = dir.join(format!("{}.json", record.run_id));
    // Unique temp name per write (the journals discipline): a persist
    // racing the promotion path's delete-then-rewrite must not share a
    // temp file; the rename onto `path` stays atomic either way.
    let tmp = dir.join(format!(".{}.{}.tmp", record.run_id, uuid::Uuid::new_v4()));
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, path).await
}

/// Delete the record stored for `run_id`; `false` when none existed
/// (already cleared by a racing transition — queue-exit deletes are
/// idempotent by construction).
pub(crate) async fn remove(root: &Path, run_id: &str) -> io::Result<bool> {
    let path = dir(root).join(format!("{run_id}.json"));
    match tokio::fs::remove_file(&path).await {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Load every persisted record in per-thread FIFO order. A file that
/// fails to parse is skipped with a warning rather than failing the
/// listing (the journal listing's rule): one corrupt record must not
/// strand the rest of the queue.
pub(crate) async fn list(root: &Path) -> io::Result<Vec<PendingRunRecord>> {
    let dir = dir(root);
    let mut entries = match tokio::fs::read_dir(&dir).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut records: Vec<PendingRunRecord> = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".json") || name.starts_with('.') {
            continue;
        }
        let bytes = tokio::fs::read(entry.path()).await?;
        match serde_json::from_slice::<PendingRunRecord>(&bytes) {
            Ok(record) => records.push(record),
            Err(e) => tracing::warn!("skipping unparsable pending-run file {name}: {e}"),
        }
    }
    sort(records.as_mut_slice());
    Ok(records)
}

/// Order records so each thread's queue rebuilds in its original enqueue
/// order (`seq` ties break on run id, keeping the order total). Both
/// backends sort through this one function so they answer identically.
pub(crate) fn sort(records: &mut [PendingRunRecord]) {
    records.sort_by(|a, b| (&a.thread_id, a.seq, &a.run_id).cmp(&(&b.thread_id, b.seq, &b.run_id)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Unique temp root under the OS temp dir, removed on drop.
    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            Self(
                std::env::temp_dir()
                    .join(format!("rusty-pending-runs-test-{}", uuid::Uuid::new_v4())),
            )
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn record(run_id: &str, thread_id: &str, seq: u64) -> PendingRunRecord {
        PendingRunRecord {
            run_id: run_id.to_string(),
            thread_id: thread_id.to_string(),
            wire_thread_id: thread_id.to_string(),
            tenant: "default".to_string(),
            graph: "pipeline".to_string(),
            payload: RunPayload {
                input: Some(json!({"seed": seq})),
                ..Default::default()
            },
            seq,
            enqueued_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn persist_then_list_survives_a_restart_in_fifo_order() {
        let tmp = TestDir::new();
        // Interleaved threads and out-of-order writes: the listing orders
        // per thread by sequence regardless.
        persist(&tmp.0, &record("run-b2", "thread-b", 3))
            .await
            .unwrap();
        persist(&tmp.0, &record("run-a1", "thread-a", 1))
            .await
            .unwrap();
        persist(&tmp.0, &record("run-a2", "thread-a", 4))
            .await
            .unwrap();
        persist(&tmp.0, &record("run-b1", "thread-b", 2))
            .await
            .unwrap();

        // A fresh list stands in for a process restart.
        let loaded = list(&tmp.0).await.unwrap();
        let ids: Vec<&str> = loaded.iter().map(|r| r.run_id.as_str()).collect();
        assert_eq!(ids, ["run-a1", "run-a2", "run-b1", "run-b2"]);
        // The payload round-trips verbatim — it is what the restore
        // re-schedules.
        assert_eq!(loaded[0].payload.input, Some(json!({"seed": 1})));
        assert_eq!(loaded[0].graph, "pipeline");
    }

    #[tokio::test]
    async fn remove_is_idempotent() {
        let tmp = TestDir::new();
        persist(&tmp.0, &record("run-1", "thread-1", 1))
            .await
            .unwrap();
        assert!(remove(&tmp.0, "run-1").await.unwrap());
        assert!(!remove(&tmp.0, "run-1").await.unwrap());
        assert!(list(&tmp.0).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn list_skips_unreadable_files() {
        let tmp = TestDir::new();
        let dir = dir(&tmp.0);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("broken.json"), b"not json").unwrap();
        persist(&tmp.0, &record("run-1", "thread-1", 1))
            .await
            .unwrap();
        let loaded = list(&tmp.0).await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].run_id, "run-1");
    }
}
