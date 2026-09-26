//! JSON-file persistence for Flight Recorder journal snapshots.
//!
//! One file per run at `{store_path}/journals/{run_id}.json`, rewritten as
//! the run's journal grows (once per checkpoint boundary and once at run
//! completion). The run id is server-minted (UUID v4), never client-chosen,
//! so no id validation is needed at this layer. Writes are atomic
//! (temp file + rename), mirroring the checkpointer's durability discipline.

use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use rusty_agent_runtime::journal::{Clock, Journal, JournalSnapshot};
use rusty_agent_runtime::record::RunEventKind;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What a listing needs to know about a journaled run without reading the
/// journal: written beside the journal at every persist, read in its
/// place by `GET /runs`. The journal itself is read, and verified, when
/// the run is opened.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JournalHead {
    pub run_id: String,
    pub thread_id: String,
    /// The status the journal proves (`success`, `error`, `interrupted`,
    /// `cancelled`).
    pub status: String,
    /// How many events the journal holds; zero means nothing to serve.
    pub events: usize,
    /// When the earliest event was recorded — the run's start, when no
    /// accepted record says otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_at: Option<DateTime<Utc>>,
    /// When the latest event was recorded — the run's end, for what
    /// reads the newest runs' journals (the recent tool outcomes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_at: Option<DateTime<Utc>>,
    /// The checkpoints the journal names: the ownership proof `GET /runs`
    /// checks against the caller's tenant.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checkpoint_ids: Vec<String>,
    /// What the run was asked — the person's first message, cut to a line —
    /// so a listing can say which run this was without opening it. `None`
    /// on a head written before this was kept (rebuilt on the next
    /// listing); empty when the run began with no message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asked: Option<String>,
    /// The run's declared config (`run_config_declared`), for a run older
    /// than accepted records: its agent and its attribution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declared: Option<Value>,
    /// Whether the journal's hash chain verified when the head was made. A
    /// head made at persist time is of a journal the runtime just wrote;
    /// one made by backfill is of whatever was on disk.
    pub verified: bool,
}

/// The head of `snapshot`.
pub(crate) fn head_of(snapshot: &JournalSnapshot) -> JournalHead {
    let verified = Journal::from_snapshot(snapshot.clone(), Clock::System).is_ok();
    let checkpoint_ids = snapshot
        .events
        .iter()
        .filter(|event| event.kind == RunEventKind::CheckpointWritten)
        .filter_map(|event| crate::replay::resolve(snapshot, event.output.as_ref()))
        .filter_map(|output| output.get("checkpoint_id")?.as_str().map(str::to_owned))
        .collect();
    let declared = snapshot
        .events
        .iter()
        .find(|event| serde_json::to_value(event.kind).ok().and_then(|k| k.as_str().map(str::to_owned)) == Some("run_config_declared".to_owned()))
        .and_then(|event| crate::replay::resolve(snapshot, event.output.as_ref()))
        .filter(Value::is_object);
    JournalHead {
        run_id: snapshot.run_id.clone(),
        thread_id: snapshot.thread_id.clone(),
        status: crate::routes::journal_status(&snapshot.events).to_owned(),
        events: snapshot.events.len(),
        first_at: snapshot.events.iter().map(|event| event.recorded_at).min(),
        last_at: snapshot.events.iter().map(|event| event.recorded_at).max(),
        checkpoint_ids,
        asked: Some(asked_of(snapshot).unwrap_or_default()),
        declared,
        verified,
    }
}

/// How many characters of the ask a head keeps.
const ASKED_CHARS: usize = 200;

/// The person's first message to the run, from the first node's input.
fn asked_of(snapshot: &JournalSnapshot) -> Option<String> {
    let input = snapshot
        .events
        .iter()
        .find(|event| event.kind == RunEventKind::NodeInput)
        .and_then(|event| crate::replay::resolve(snapshot, event.input.as_ref()))?;
    asked_in(&input)
}

/// The first user message in a run's input, as a line.
pub(crate) fn asked_in(input: &Value) -> Option<String> {
    let messages = input.get("messages")?.as_array()?;
    let content = messages
        .iter()
        .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
        .and_then(|m| m.get("content"))?;
    let text = match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join(" "),
        _ => return None,
    };
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.is_empty() {
        return None;
    }
    Some(line.chars().take(ASKED_CHARS).collect())
}

/// The heads directory: `{store_path}/journals/heads/{run_id}.json`.
fn heads_dir(root: &Path) -> PathBuf {
    dir(root).join("heads")
}

/// Write the head as the journal is kept: a person's run's head is sealed
/// under their key, so a store without the key lists the run no more
/// than it can read it (a backup restored after a forget, say).
async fn write_head(root: &Path, head: &JournalHead, vault: Option<&crate::vault::PersonVault>, person: Option<(&str, &str)>) -> io::Result<()> {
    let dir = heads_dir(root);
    tokio::fs::create_dir_all(&dir).await?;
    let bytes = crate::vault::record_bytes(vault, person, "journal-head", &head.run_id, head)?;
    let tmp = dir.join(format!(".{}.{}.tmp", head.run_id, uuid::Uuid::new_v4()));
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, dir.join(format!("{}.json", head.run_id))).await
}

/// `Ok(Some)` for a head this store can read; `Ok(None)` when there is
/// none, or it is sealed for a key not held here (the journal reads the
/// same way, so nothing is backfilled for it).
async fn read_head(root: &Path, run_id: &str, vault: Option<&crate::vault::PersonVault>) -> io::Result<Option<Option<JournalHead>>> {
    match tokio::fs::read(heads_dir(root).join(format!("{run_id}.json"))).await {
        Ok(bytes) => Ok(Some(crate::vault::record_from_bytes(vault, "journal-head", run_id, &bytes).unwrap_or(None))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// The journals directory under the store root. `journals` is a reserved
/// layout name (see [`crate::RESERVED_NAMES`]): client-chosen thread ids may
/// not claim it.
pub(crate) fn dir(root: &Path) -> PathBuf {
    root.join("journals")
}

/// Persist `snapshot`, replacing any earlier snapshot of the same run.
/// Keep `snapshot`; a person's run is sealed under their key (see
/// [`crate::vault`]), nobody's is plain.
pub(crate) async fn persist(root: &Path, snapshot: &JournalSnapshot, vault: Option<&crate::vault::PersonVault>, person: Option<(&str, &str)>) -> io::Result<()> {
    let dir = dir(root);
    tokio::fs::create_dir_all(&dir).await?;
    let bytes = crate::vault::record_bytes(vault, person, "journal", &snapshot.run_id, snapshot)?;
    let path = dir.join(format!("{}.json", snapshot.run_id));
    // Unique temp name per write: two concurrent persists of the same
    // journal (a settlement hook racing a reconcile-on-read, say) must
    // not share a temp file — the loser's rename would find its temp
    // gone and surface ENOENT as a 500. The rename onto `path` stays
    // atomic, so crash-safety is unchanged; last writer wins.
    let tmp = dir.join(format!(".{}.{}.tmp", snapshot.run_id, uuid::Uuid::new_v4()));
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, path).await?;
    // The head after the journal: a journal without a head is backfilled
    // at the next listing; a head without a journal never exists.
    write_head(root, &head_of(snapshot), vault, person).await
}

/// Load the snapshot stored for `run_id`; `None` when none was persisted
/// (a queued run, or one that failed before its first checkpoint boundary).
/// The journal, or `None` when there is none — or when it is sealed for
/// a person whose key this store does not hold.
pub(crate) async fn get(root: &Path, run_id: &str, vault: Option<&crate::vault::PersonVault>) -> io::Result<Option<JournalSnapshot>> {
    let path = dir(root).join(format!("{run_id}.json"));
    let bytes = match tokio::fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    crate::vault::record_from_bytes(vault, "journal", run_id, &bytes)
}

/// Remove the journal file and its head; `false` when there was none.
pub(crate) async fn remove(root: &Path, run_id: &str) -> io::Result<bool> {
    let _ = tokio::fs::remove_file(heads_dir(root).join(format!("{run_id}.json"))).await;
    match tokio::fs::remove_file(dir(root).join(format!("{run_id}.json"))).await {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Every journaled run's head, in run-id order, without reading a journal
/// that already has one. A journal without a head — written before heads
/// were kept — is read once, and its head written, so the next listing
/// reads none. A sealed journal whose key is not here has no head and is
/// absent, as it is from the listing.
pub(crate) async fn list_heads(root: &Path, vault: Option<&crate::vault::PersonVault>) -> io::Result<Vec<JournalHead>> {
    let dir = dir(root);
    let mut entries = match tokio::fs::read_dir(&dir).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut heads: Vec<JournalHead> = Vec::new();
    let mut backfilled = 0usize;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".json") || name.starts_with('.') {
            continue;
        }
        let run_id = name.trim_end_matches(".json");
        match read_head(root, run_id, vault).await? {
            // A head from before the ask was kept is rebuilt once, below.
            Some(Some(head)) if head.asked.is_some() => {
                heads.push(head);
                continue;
            }
            Some(Some(_)) => {}
            // Sealed for a key not here: absent, as its journal is.
            Some(None) => continue,
            None => {}
        }
        let bytes = tokio::fs::read(entry.path()).await?;
        match crate::vault::record_from_bytes::<JournalSnapshot>(vault, "journal", run_id, &bytes) {
            Ok(Some(snapshot)) => {
                let head = head_of(&snapshot);
                // Sealed as the journal was, for the person it names.
                let sealed_for = serde_json::from_slice::<Value>(&bytes).ok().and_then(|v| v.get("sealed_for").cloned());
                let person = sealed_for.as_ref().and_then(|p| Some((p.get("tenant")?.as_str()?.to_owned(), p.get("principal")?.as_str()?.to_owned())));
                write_head(root, &head, vault, person.as_ref().map(|(t, p)| (t.as_str(), p.as_str()))).await?;
                backfilled += 1;
                heads.push(head);
            }
            Ok(None) => {}
            Err(e) => tracing::warn!("skipping unparsable journal file {name}: {e}"),
        }
    }
    if backfilled > 0 {
        tracing::info!(backfilled, "journal heads written for journals that had none");
    }
    heads.sort_by(|a, b| a.run_id.cmp(&b.run_id));
    Ok(heads)
}

/// Load every persisted snapshot, in file-name (run-id) order. A file
/// that fails to parse is skipped with a warning rather than failing the
/// listing — the health board reads journals to *derive* state, and one
/// corrupt file must not blind it to everything else (the corrupt file's
/// own read path still surfaces the error).
pub(crate) async fn list(root: &Path, vault: Option<&crate::vault::PersonVault>) -> io::Result<Vec<JournalSnapshot>> {
    let dir = dir(root);
    let mut entries = match tokio::fs::read_dir(&dir).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut snapshots: Vec<JournalSnapshot> = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".json") || name.starts_with('.') {
            continue;
        }
        let bytes = tokio::fs::read(entry.path()).await?;
        let run_id = name.trim_end_matches(".json");
        match crate::vault::record_from_bytes::<JournalSnapshot>(vault, "journal", run_id, &bytes) {
            Ok(Some(snapshot)) => snapshots.push(snapshot),
            // Sealed for a person whose key is not here: absent, by design.
            Ok(None) => {}
            Err(e) => tracing::warn!("skipping unparsable journal file {name}: {e}"),
        }
    }
    snapshots.sort_by(|a, b| a.run_id.cmp(&b.run_id));
    Ok(snapshots)
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

        persist(&root, &journal.snapshot(), None, None).await.unwrap();
        let loaded = get(&root, "run-1", None)
            .await
            .unwrap()
            .expect("snapshot persisted");
        assert_eq!(loaded.run_id, "run-1");
        assert_eq!(loaded.events.len(), 2);
        assert_eq!(loaded.head_hash, journal.head_hash());

        // A later snapshot of the same run replaces the earlier file.
        journal.record(EventDraft::new(RunEventKind::SuperStepEnd, Effect::Pure));
        persist(&root, &journal.snapshot(), None, None).await.unwrap();
        let loaded = get(&root, "run-1", None)
            .await
            .unwrap()
            .expect("snapshot persisted");
        assert_eq!(loaded.events.len(), 3);

        assert!(get(&root, "never-written", None).await.unwrap().is_none());
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
            writers.spawn(async move { persist(&root, &snapshot, None, None).await });
        }
        while let Some(outcome) = writers.join_next().await {
            outcome.expect("writer joined").expect("persist succeeds");
        }
        let loaded = get(&root, "run-1", None)
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

    /// A listing reads heads, not journals: a persist writes the head; a
    /// journal from before heads gets one at the first listing and is not
    /// read again; a removed journal takes its head with it.
    #[tokio::test]
    async fn the_listing_reads_heads_and_backfills_the_journals_that_have_none() {
        let root =
            std::env::temp_dir().join(format!("rusty-journals-test-{}", uuid::Uuid::new_v4()));
        let journal = Journal::new("run-new", "thread-1", Clock::System);
        journal.record(EventDraft::new(RunEventKind::SuperStepStart, Effect::Pure));
        journal.record(EventDraft::new(RunEventKind::CheckpointWritten, Effect::Pure).parent("run-new:0").output(serde_json::json!({"checkpoint_id": "cp-1"})));
        persist(&root, &journal.snapshot(), None, None).await.unwrap();
        assert!(read_head(&root, "run-new", None).await.unwrap().is_some(), "a persist writes the head");

        // An older journal, written before heads: the file alone.
        let old = Journal::new("run-old", "thread-1", Clock::System);
        old.record(EventDraft::new(RunEventKind::SuperStepStart, Effect::Pure));
        let bytes = serde_json::to_vec(&old.snapshot()).unwrap();
        std::fs::write(dir(&root).join("run-old.json"), bytes).unwrap();
        assert!(read_head(&root, "run-old", None).await.unwrap().is_none());

        let heads = list_heads(&root, None).await.unwrap();
        assert_eq!(heads.iter().map(|h| h.run_id.as_str()).collect::<Vec<_>>(), vec!["run-new", "run-old"]);
        let new = heads.iter().find(|h| h.run_id == "run-new").unwrap();
        assert_eq!(new.status, "success");
        assert_eq!(new.events, 2);
        assert_eq!(new.checkpoint_ids, vec!["cp-1"]);
        assert!(new.verified && new.first_at.is_some());
        assert!(read_head(&root, "run-old", None).await.unwrap().is_some(), "the first listing backfills the head");

        // The journal files are the listing; a head alone lists nothing.
        std::fs::remove_file(dir(&root).join("run-old.json")).unwrap();
        assert!(list_heads(&root, None).await.unwrap().iter().all(|h| h.run_id != "run-old"), "no journal, no listing");
        assert!(remove(&root, "run-new").await.unwrap());
        assert!(read_head(&root, "run-new", None).await.unwrap().is_none(), "a removed journal takes its head");
        let _ = std::fs::remove_dir_all(root);
    }
}
