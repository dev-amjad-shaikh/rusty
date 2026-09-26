//! Durable accepted-run records: what the server accepted, for as long as
//! the run's identity matters.
//!
//! A run's checkpoints and journal survive a restart; the run's *acceptance*
//! — the exact payload the server took, which agent it was of, when — lived
//! only in the in-process [`crate::runs::RunManager`], so after a restart a
//! finished run answered 404 on `GET /runs/{id}` and could not become an
//! evaluation case (a case binds to the run's exact accepted input, which
//! the journal does not hold: its first `node_input` is the reduced thread
//! state). Every admitted run now leaves one JSON file under
//! `{store_path}/accepted_runs/{run_id}.json` (the Postgres backend maps
//! this to `server_accepted_runs`), written at admission and never deleted:
//! the record is the run's identity after this process is gone.
//!
//! Writes are atomic (temp file + rename), the journals discipline.

use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::runs::RunPayload;

/// Exactly what the server accepted for one run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AcceptedRunRecord {
    /// Server-minted run id (UUID v4).
    pub run_id: String,
    /// Internal (tenant-scoped) thread id — ownership is checked against it.
    pub thread_id: String,
    /// External thread id, the wire form.
    pub wire_thread_id: String,
    /// Owning tenant, resolved at submission.
    pub tenant: String,
    /// Registered graph name (the thread's binding at submission).
    pub graph: String,
    /// The accepted run payload, verbatim: input, assistant, metadata, config.
    pub payload: RunPayload,
    /// Server acceptance time.
    pub accepted_at: DateTime<Utc>,
}

/// The accepted-runs directory under the store root. `accepted_runs` is a
/// reserved layout name (see [`crate::RESERVED_NAMES`]).
pub(crate) fn dir(root: &Path) -> PathBuf {
    root.join("accepted_runs")
}

/// Persist `record`, replacing any earlier record of the same run.
/// The person a run acts for — its subject, or whom it was started for,
/// or who started it, when that is a user.
pub(crate) fn person_of(record: &AcceptedRunRecord) -> Option<(String, String)> {
    crate::routes::run_person(record.payload.metadata.as_ref()).map(|p| (record.tenant.clone(), p.to_owned()))
}

/// Keep the record; a person's run is sealed under their key.
pub(crate) async fn persist(root: &Path, record: &AcceptedRunRecord, vault: Option<&crate::vault::PersonVault>, person: Option<(&str, &str)>) -> io::Result<()> {
    let dir = dir(root);
    tokio::fs::create_dir_all(&dir).await?;
    let bytes = crate::vault::record_bytes(vault, person, "run", &record.run_id, record)?;
    let path = dir.join(format!("{}.json", record.run_id));
    let tmp = dir.join(format!(".{}.{}.tmp", record.run_id, uuid::Uuid::new_v4()));
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, path).await
}

/// The record stored for `run_id`; `None` when the run was accepted by a
/// server from before records were kept, or never.
/// The record, or `None` when there is none — or when it is sealed for a
/// person whose key this store does not hold.
pub(crate) async fn load(root: &Path, run_id: &str, vault: Option<&crate::vault::PersonVault>) -> io::Result<Option<AcceptedRunRecord>> {
    let path = dir(root).join(format!("{run_id}.json"));
    let bytes = match tokio::fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    crate::vault::record_from_bytes(vault, "run", run_id, &bytes)
}

/// Every readable record.
pub(crate) async fn list(root: &Path, vault: Option<&crate::vault::PersonVault>) -> io::Result<Vec<AcceptedRunRecord>> {
    let mut entries = match tokio::fs::read_dir(dir(root)).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".json") || name.starts_with('.') {
            continue;
        }
        let bytes = tokio::fs::read(entry.path()).await?;
        if let Ok(Some(record)) = crate::vault::record_from_bytes::<AcceptedRunRecord>(vault, "run", name.trim_end_matches(".json"), &bytes) {
            out.push(record);
        }
    }
    Ok(out)
}

/// Remove the record file; `false` when there was none.
pub(crate) async fn remove(root: &Path, run_id: &str) -> io::Result<bool> {
    match tokio::fs::remove_file(dir(root).join(format!("{run_id}.json"))).await {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn a_record_round_trips_and_a_missing_one_is_none() {
        let root = std::env::temp_dir().join(format!("rusty-accepted-{}", uuid::Uuid::new_v4()));
        let payload: RunPayload = serde_json::from_value(json!({
            "assistant_id": "agent-1",
            "input": {"messages": [{"role": "user", "content": "hello"}]},
            "metadata": {"created_by": {"kind": "user", "principal_id": "bob"}}
        }))
        .unwrap();
        let record = AcceptedRunRecord {
            run_id: "run-1".into(),
            thread_id: "tenant:thread-1".into(),
            wire_thread_id: "thread-1".into(),
            tenant: "tenant".into(),
            graph: "react_agent".into(),
            payload,
            accepted_at: Utc::now(),
        };
        persist(&root, &record, None, None).await.unwrap();
        let back = load(&root, "run-1", None).await.unwrap().expect("stored");
        assert_eq!(back.run_id, "run-1");
        assert_eq!(back.payload.assistant_id.as_deref(), Some("agent-1"));
        assert_eq!(
            back.payload.input,
            Some(json!({"messages": [{"role": "user", "content": "hello"}]}))
        );
        assert!(load(&root, "run-2", None).await.unwrap().is_none());
        let _ = std::fs::remove_dir_all(root);
    }
}
