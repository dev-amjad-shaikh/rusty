//! Transactional outbox (R0.6 wave 2b): state change and task submission in
//! one transaction, with a relay publishing into the queue at-least-once.
//!
//! The split-brain this kills: a run writes state (a checkpoint) and submits
//! a task, crashing between the two — state saved, task lost (or the
//! reverse). The outbox makes the pair atomic on Postgres: the outbox row is
//! written *with* the checkpoint in one transaction, and the background
//! relay ([`spawn_relay`]) publishes pending rows into the task queue.
//! Publish is idempotent — the relay inserts through the same
//! idempotency-key dedupe as `POST /tasks` — so a relay that dies and
//! restarts can neither lose nor double a row.
//!
//! The JSON-file backend shares the API but not the atomicity: one file per
//! record cannot transact across checkpoint and queue. It writes the outbox
//! row *first* and then the checkpoint, so a crash can leave a task whose
//! state never landed (which the recipient must tolerate — the task id and
//! idempotency key still correlate it) but never a checkpoint whose task is
//! silently gone. Cross-record atomicity is Postgres-only; see
//! `docs/durable-work-design.md`.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::server_store::ServerStore;
use crate::tasks::TaskRecord;

/// How often the relay polls for unpublished outbox rows. Short enough that
/// outbox-enqueued tasks feel promptly submitted; long enough that an idle
/// server issues one cheap indexed query per second.
pub(crate) const DEFAULT_RELAY_INTERVAL: Duration = Duration::from_millis(250);

/// Rows published per relay pass. Bounded so a backlog drains in
/// predictable-size transactions rather than one unbounded one; the next
/// tick continues where this one stopped.
pub(crate) const RELAY_BATCH_LIMIT: usize = 100;

/// One outbox row: a task waiting for the relay to publish it into the
/// queue. Rows are 1:1 with tasks (`outbox_id == task.task_id`), so the
/// row's lifecycle is exactly "pending until published".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct OutboxRecord {
    /// The row id — the enclosed task's id. Equality is what makes
    /// re-writing the same outbox row (a retried checkpoint+enqueue) a
    /// no-op rather than a second pending copy.
    pub outbox_id: String,
    /// Owning tenant, mirrored from the task (the relay publishes across
    /// tenants; the record keeps the scope for inspection and tests).
    pub tenant: String,
    /// The task to publish, as constructed at enqueue time (status
    /// `queued`, attempt 0).
    pub task: TaskRecord,
    /// When the relay published the row (`None` = pending).
    #[serde(default)]
    pub published_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

impl OutboxRecord {
    /// A pending row wrapping `task`.
    pub(crate) fn new(task: TaskRecord, now: DateTime<Utc>) -> Self {
        Self {
            outbox_id: task.task_id.clone(),
            tenant: task.tenant.clone(),
            task,
            published_at: None,
            created_at: now,
        }
    }
}

// --------------------------------------------------------------------- //
// JSON-file persistence (`{store_path}/outbox/{outbox_id}.json`)
// --------------------------------------------------------------------- //

/// The outbox directory under the store root. `outbox` is a reserved layout
/// name (see [`crate::RESERVED_NAMES`]): client-chosen thread ids may not
/// claim it.
pub(crate) fn dir(root: &Path) -> PathBuf {
    root.join("outbox")
}

/// Persist one outbox record (create or overwrite — the relay rewrites the
/// row when it publishes), atomically: temp file + rename, mirroring the
/// task store's durability discipline.
pub(crate) async fn persist(root: &Path, record: &OutboxRecord) -> io::Result<()> {
    let dir = dir(root);
    tokio::fs::create_dir_all(&dir).await?;
    let bytes = serde_json::to_vec_pretty(record)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let path = dir.join(format!("{}.json", record.outbox_id));
    let tmp = dir.join(format!("{}.tmp", record.outbox_id));
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, path).await
}

/// Load all persisted outbox rows, skipping (with a warning) any file that
/// fails to parse — the same corrupt-record tolerance as the task store.
pub(crate) fn load(root: &Path) -> HashMap<String, OutboxRecord> {
    let mut out = HashMap::new();
    let Ok(entries) = std::fs::read_dir(dir(root)) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let parsed = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<OutboxRecord>(&raw).ok());
        match parsed {
            Some(record) => {
                out.insert(record.outbox_id.clone(), record);
            }
            None => {
                tracing::warn!(path = %path.display(), "skipping unreadable outbox file")
            }
        }
    }
    out
}

// --------------------------------------------------------------------- //
// The relay
// --------------------------------------------------------------------- //

/// Spawn the outbox relay: a background task publishing pending outbox rows
/// into the task queue every `interval`, until `shutdown` is cancelled (the
/// same detached-spawn discipline as the cron scheduler — the relay lives
/// as long as the server process).
///
/// Crash-safety lives in the store, not here: each row's publish (task
/// insert + mark-published) is one atomic store operation, and the task
/// insert dedupes on the idempotency key, so a relay killed at any point
/// leaves only work a restart will redo — never lost, never doubled. A
/// failed poll is logged and retried on the next tick: in a durable system
/// the store coming back is the normal case.
///
/// Drain semantics (R0.6 wave 2c): the token is only observed *between*
/// passes, so a pass in flight when shutdown starts always completes — an
/// aborted pass would be safe (publish is idempotent) but needlessly
/// wasteful. Rows still pending when the relay stops stay pending; the
/// next process's relay publishes them on its first pass.
pub(crate) fn spawn_relay(
    store: Arc<dyn ServerStore>,
    interval: Duration,
    shutdown: tokio_util::sync::CancellationToken,
) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        // A delayed tick must not burst into a publish storm: the relay is
        // a background pump, not a quota to catch up on.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                _ = shutdown.cancelled() => {
                    tracing::info!("outbox relay shutting down; pending rows publish on the next process");
                    break;
                }
            }
            match store
                .outbox_publish_pending(RELAY_BATCH_LIMIT, Utc::now())
                .await
            {
                Ok(published) if !published.is_empty() => {
                    tracing::info!(count = published.len(), "outbox relay published tasks");
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(%error, "outbox relay poll failed; will retry");
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::{NewTask, StatusCategory, DEFAULT_MAX_ATTEMPTS, DEFAULT_POOL};

    fn task(id: &str) -> TaskRecord {
        TaskRecord::new(
            NewTask {
                task_id: id.to_string(),
                tenant: "acme".to_string(),
                kind: "send_email".to_string(),
                payload: serde_json::json!({"to": "a@b.c"}),
                pool: DEFAULT_POOL.to_string(),
                recipient: None,
                max_attempts: DEFAULT_MAX_ATTEMPTS,
                idempotency_key: None,
                effect: None,
                run_id: None,
                thread_id: None,
                deadline: None,
                worker_version: None,
                parent: None,
                parent_task_id: None,
                stage: 0,
                status_category: StatusCategory::Todo,
            },
            Utc::now(),
        )
    }

    #[tokio::test]
    async fn outbox_file_round_trip_and_corrupt_tolerance() {
        let root = std::env::temp_dir().join(format!("rusty-outbox-test-{}", uuid::Uuid::new_v4()));
        let pending = OutboxRecord::new(task("t-1"), Utc::now());
        persist(&root, &pending).await.unwrap();
        let mut published = OutboxRecord::new(task("t-2"), Utc::now());
        published.published_at = Some(Utc::now());
        persist(&root, &published).await.unwrap();
        // A corrupt file must not take the outbox down at boot.
        std::fs::write(dir(&root).join("broken.json"), "{ not json").unwrap();

        let loaded = load(&root);
        assert_eq!(loaded.len(), 2);
        assert!(loaded["t-1"].published_at.is_none());
        assert!(loaded["t-2"].published_at.is_some());
        assert_eq!(loaded["t-1"].task.task_id, "t-1");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn outbox_id_mirrors_the_task_id() {
        let record = OutboxRecord::new(task("t-9"), Utc::now());
        assert_eq!(record.outbox_id, "t-9");
        assert_eq!(record.tenant, "acme");
        assert!(record.published_at.is_none());
    }
}
