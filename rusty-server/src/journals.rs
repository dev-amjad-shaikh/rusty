//! JSON-file persistence for Flight Recorder journal snapshots.
//!
//! One file per run at `{store_path}/journals/{run_id}.json`, rewritten as
//! the run's journal grows (once per checkpoint boundary and once at run
//! completion). The run id is server-minted (UUID v4), never client-chosen,
//! so no id validation is needed at this layer. Writes are atomic
//! (temp file + rename), mirroring the checkpointer's durability discipline.

use std::io;
use std::path::{Path, PathBuf};

use rusty_agent_runtime::journal::JournalSnapshot;

/// The journals directory under the store root. `journals` is a reserved
/// layout name (see [`crate::RESERVED_NAMES`]): client-chosen thread ids may
/// not claim it.
pub(crate) fn dir(root: &Path) -> PathBuf {
    root.join("journals")
}

/// Persist `snapshot`, replacing any earlier snapshot of the same run.
pub(crate) async fn persist(root: &Path, snapshot: &JournalSnapshot) -> io::Result<()> {
    let dir = dir(root);
    tokio::fs::create_dir_all(&dir).await?;
    let bytes = serde_json::to_vec_pretty(snapshot)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let path = dir.join(format!("{}.json", snapshot.run_id));
    // Unique temp name per write: two concurrent persists of the same
    // journal (a settlement hook racing a reconcile-on-read, say) must
    // not share a temp file — the loser's rename would find its temp
    // gone and surface ENOENT as a 500. The rename onto `path` stays
    // atomic, so crash-safety is unchanged; last writer wins.
    let tmp = dir.join(format!(".{}.{}.tmp", snapshot.run_id, uuid::Uuid::new_v4()));
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, path).await
}

/// Load the snapshot stored for `run_id`; `None` when none was persisted
/// (a queued run, or one that failed before its first checkpoint boundary).
pub(crate) async fn get(root: &Path, run_id: &str) -> io::Result<Option<JournalSnapshot>> {
    let path = dir(root).join(format!("{run_id}.json"));
    let bytes = match tokio::fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let snapshot = serde_json::from_slice(&bytes)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(Some(snapshot))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusty_agent_runtime::journal::{Clock, EventDraft, Journal};
    use rusty_agent_runtime::record::{Effect, RunEventKind};

    #[tokio::test]
    async fn persist_then_get_round_trips_the_snapshot() {
        let root =
            std::env::temp_dir().join(format!("rusty-journals-test-{}", uuid::Uuid::new_v4()));
        let journal = Journal::new("run-1", "thread-1", Clock::System);
        journal.record(EventDraft::new(RunEventKind::SuperStepStart, Effect::Pure));
        journal.record(
            EventDraft::new(RunEventKind::CheckpointWritten, Effect::Pure).parent("run-1:0"),
        );

        persist(&root, &journal.snapshot()).await.unwrap();
        let loaded = get(&root, "run-1")
            .await
            .unwrap()
            .expect("snapshot persisted");
        assert_eq!(loaded.run_id, "run-1");
        assert_eq!(loaded.events.len(), 2);
        assert_eq!(loaded.head_hash, journal.head_hash());

        // A later snapshot of the same run replaces the earlier file.
        journal.record(EventDraft::new(RunEventKind::SuperStepEnd, Effect::Pure));
        persist(&root, &journal.snapshot()).await.unwrap();
        let loaded = get(&root, "run-1")
            .await
            .unwrap()
            .expect("snapshot persisted");
        assert_eq!(loaded.events.len(), 3);

        assert!(get(&root, "never-written").await.unwrap().is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn concurrent_persists_of_one_journal_never_race_on_the_temp_file() {
        let root =
            std::env::temp_dir().join(format!("rusty-journals-test-{}", uuid::Uuid::new_v4()));
        let journal = Journal::new("run-1", "thread-1", Clock::System);
        journal.record(EventDraft::new(RunEventKind::SuperStepStart, Effect::Pure));
        let snapshot = journal.snapshot();

        // A settlement hook and a reconcile-on-read can persist the same
        // journal in the same instant; a shared temp path made the loser
        // fail with ENOENT. Every writer must succeed; last writer wins.
        let mut writers = tokio::task::JoinSet::new();
        for _ in 0..8 {
            let root = root.clone();
            let snapshot = snapshot.clone();
            writers.spawn(async move { persist(&root, &snapshot).await });
        }
        while let Some(outcome) = writers.join_next().await {
            outcome.expect("writer joined").expect("persist succeeds");
        }
        let loaded = get(&root, "run-1")
            .await
            .unwrap()
            .expect("snapshot persisted");
        assert_eq!(loaded.head_hash, snapshot.head_hash);
        // No temp files survive a completed write.
        let leftovers = std::fs::read_dir(dir(&root))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0);
        let _ = std::fs::remove_dir_all(root);
    }
}
