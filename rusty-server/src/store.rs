//! Cross-thread key-value store, JSON-file-backed under the store root.
//!
//! Items are namespaced: `PUT /store/{namespace}/{key}` writes an arbitrary
//! JSON value, persisted as `{store_path}/store/{namespace}/{key}.json`
//! with `{namespace, key, value, created_at, updated_at}` inside. There is
//! no in-memory index — reads, lists, and deletes go straight to the file
//! system, so the store survives restarts by construction. Namespace and
//! key segments are restricted to `[A-Za-z0-9._-]` (1–128 chars) to keep
//! the mapping to paths unambiguous.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::ApiError;

/// One stored item as persisted on disk and returned over the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct StoreItem {
    pub namespace: String,
    pub key: String,
    pub value: Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Reject segments that could escape the store root or collide on disk:
/// anything outside `[A-Za-z0-9._-]` (1–128 chars), or all-dots segments
/// (`.`, `..`, `…`) that would resolve as parent-directory components.
pub(crate) fn validate_segment(kind: &str, segment: &str) -> Result<(), ApiError> {
    let ok = !segment.is_empty()
        && segment.len() <= 128
        && segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        && !segment.chars().all(|c| c == '.');
    if ok {
        Ok(())
    } else {
        Err(ApiError::bad_request(format!(
            "invalid {kind} `{segment}` (allowed: [A-Za-z0-9._-], 1..=128 chars)"
        )))
    }
}

fn namespace_dir(store_root: &Path, namespace: &str) -> PathBuf {
    store_root.join("store").join(namespace)
}

fn item_path(store_root: &Path, namespace: &str, key: &str) -> PathBuf {
    namespace_dir(store_root, namespace).join(format!("{key}.json"))
}

fn lock_path(store_root: &Path, namespace: &str, key: &str) -> PathBuf {
    namespace_dir(store_root, namespace).join(format!(".{key}.lock"))
}

async fn acquire_item_lock(
    store_root: &Path,
    namespace: &str,
    key: &str,
) -> std::io::Result<PathBuf> {
    let dir = namespace_dir(store_root, namespace);
    tokio::fs::create_dir_all(&dir).await?;
    let path = lock_path(store_root, namespace, key);
    for _ in 0..2_000 {
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await
        {
            Ok(_) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale = tokio::fs::metadata(&path)
                    .await
                    .ok()
                    .and_then(|metadata| metadata.modified().ok())
                    .and_then(|modified| modified.elapsed().ok())
                    .is_some_and(|elapsed| elapsed > std::time::Duration::from_secs(30));
                if stale {
                    let _ = tokio::fs::remove_file(&path).await;
                    continue;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!("timed out acquiring store lock for {namespace}/{key}"),
    ))
}

async fn release_item_lock(path: PathBuf) -> std::io::Result<()> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

async fn write_item(store_root: &Path, item: &StoreItem) -> std::io::Result<()> {
    let raw = serde_json::to_vec_pretty(item).expect("store item serialization is infallible");
    let path = item_path(store_root, &item.namespace, &item.key);
    let temporary = path.with_extension(format!("json.tmp-{}", uuid::Uuid::new_v4()));
    tokio::fs::write(&temporary, raw).await?;
    tokio::fs::rename(temporary, path).await
}

/// Read one item (`None` when absent). A corrupt item file reads as absent
/// — but loudly, matching `list`'s warn-and-skip behavior, since silent
/// corruption would make a later `put` answer `201` and reset `created_at`.
pub(crate) async fn get(
    store_root: &Path,
    namespace: &str,
    key: &str,
) -> std::io::Result<Option<StoreItem>> {
    let path = item_path(store_root, namespace, key);
    let raw = match tokio::fs::read(&path).await {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    match serde_json::from_slice(&raw) {
        Ok(item) => Ok(Some(item)),
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "skipping corrupt store item");
            Ok(None)
        }
    }
}

/// Insert or replace one item. Returns the record plus `true` when the key
/// was newly created (creation time is preserved on overwrite).
pub(crate) async fn put(
    store_root: &Path,
    namespace: &str,
    key: &str,
    value: Value,
) -> std::io::Result<(StoreItem, bool)> {
    let existing = get(store_root, namespace, key).await?;
    let now = Utc::now();
    let item = StoreItem {
        namespace: namespace.to_string(),
        key: key.to_string(),
        value,
        created_at: existing.as_ref().map(|i| i.created_at).unwrap_or(now),
        updated_at: now,
    };
    let created = existing.is_none();
    let dir = namespace_dir(store_root, namespace);
    tokio::fs::create_dir_all(&dir).await?;
    let raw = serde_json::to_vec_pretty(&item).expect("store item serialization is infallible");
    tokio::fs::write(item_path(store_root, namespace, key), raw).await?;
    Ok((item, created))
}

/// Atomically insert one item when the key is absent. This operation is
/// process-safe for the file backend and is used by durable ownership claims.
pub(crate) async fn create(
    store_root: &Path,
    namespace: &str,
    key: &str,
    value: Value,
) -> std::io::Result<Option<StoreItem>> {
    let lock = acquire_item_lock(store_root, namespace, key).await?;
    let result = async {
        if get(store_root, namespace, key).await?.is_some() {
            return Ok(None);
        }
        let now = Utc::now();
        let item = StoreItem {
            namespace: namespace.to_string(),
            key: key.to_string(),
            value,
            created_at: now,
            updated_at: now,
        };
        write_item(store_root, &item).await?;
        Ok(Some(item))
    }
    .await;
    let release = release_item_lock(lock).await;
    match (result, release) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), _) | (_, Err(error)) => Err(error),
    }
}

/// Atomically replace one item only when its revision is still current.
pub(crate) async fn compare_and_swap(
    store_root: &Path,
    namespace: &str,
    key: &str,
    expected_updated_at: DateTime<Utc>,
    value: Value,
) -> std::io::Result<Option<StoreItem>> {
    let lock = acquire_item_lock(store_root, namespace, key).await?;
    let result = async {
        let Some(existing) = get(store_root, namespace, key).await? else {
            return Ok(None);
        };
        if existing.updated_at != expected_updated_at {
            return Ok(None);
        }
        let item = StoreItem {
            namespace: namespace.to_string(),
            key: key.to_string(),
            value,
            created_at: existing.created_at,
            updated_at: Utc::now(),
        };
        write_item(store_root, &item).await?;
        Ok(Some(item))
    }
    .await;
    let release = release_item_lock(lock).await;
    match (result, release) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), _) | (_, Err(error)) => Err(error),
    }
}

/// Delete one item. Returns `true` when it existed.
pub(crate) async fn delete(store_root: &Path, namespace: &str, key: &str) -> std::io::Result<bool> {
    match tokio::fs::remove_file(item_path(store_root, namespace, key)).await {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// List all items in one namespace, sorted by key. A missing namespace
/// lists as empty; unreadable entries are skipped with a warning.
pub(crate) async fn list(store_root: &Path, namespace: &str) -> std::io::Result<Vec<StoreItem>> {
    let dir = namespace_dir(store_root, namespace);
    let mut entries = match tokio::fs::read_dir(&dir).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut items = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match tokio::fs::read(&path)
            .await
            .ok()
            .and_then(|raw| serde_json::from_slice::<StoreItem>(&raw).ok())
        {
            Some(item) => items.push(item),
            None => tracing::warn!(path = %path.display(), "skipping unreadable store item"),
        }
    }
    items.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(items)
}
