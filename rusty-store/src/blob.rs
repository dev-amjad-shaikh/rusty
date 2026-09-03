//! Blob storage seam — content-addressed, tenant-scoped object storage.
//!
//! All large payloads (spilled tool outputs, skill package files, backup
//! archives) flow through this seam rather than PostgreSQL. The seam is
//! deliberately small: [`BlobStore`] defines four operations, [`BlobLocator`]
//! carries the opaque reference, and [`BlobError`] enumerates the failure
//! modes a producer is expected to handle.

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use object_store::local::LocalFileSystem;
use object_store::{ObjectStore, PutPayload};
use sha2::{Digest, Sha256};

/// An opaque handle to a stored blob.
///
/// The locator embeds the tenant prefix, the content hash, and the original
/// byte length so that consumers never need to open the object to know its
/// size. The locator is serialisable and can be carried in event-log records
/// and spill envelopes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BlobLocator {
    /// Tenant-scoped prefix (e.g. `t/acme-corp/`).
    pub prefix: String,
    /// Hex-encoded SHA-256 of the blob content.
    pub sha256: String,
    /// Original byte length.
    pub bytes: u64,
}

impl BlobLocator {
    /// Reconstruct a locator from its fields.
    pub fn new(prefix: impl Into<String>, sha256: impl Into<String>, bytes: u64) -> Self {
        Self {
            prefix: prefix.into(),
            sha256: sha256.into(),
            bytes,
        }
    }

    /// The full object-store key: `{prefix}{sha256}`.
    pub fn key(&self) -> String {
        format!("{}{}", self.prefix, self.sha256)
    }
}

impl fmt::Display for BlobLocator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.prefix.trim_end_matches('/'), self.sha256)
    }
}

/// Errors that can occur when interacting with the blob store.
#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    /// The requested blob does not exist.
    #[error("blob not found: {0}")]
    NotFound(String),
    /// Content hash mismatch — the stored object is corrupt or was truncated.
    #[error("integrity failure for blob {locator}: expected {expected}, got {actual}")]
    Integrity {
        /// The locator that was requested.
        locator: BlobLocator,
        /// Expected hex-encoded SHA-256.
        expected: String,
        /// Actual hex-encoded SHA-256.
        actual: String,
    },
    /// A local I/O error.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// The backend is temporarily unavailable.
    #[error("backend unavailable: {0}")]
    Unavailable(String),
}

/// Storage backend for opaque byte blobs.
///
/// Implementations are expected to be `Send + Sync` so they can be held in
/// an `Arc` and shared across tasks.
#[async_trait::async_trait]
pub trait BlobStore: Send + Sync {
    /// Store `bytes` and return a content-addressed locator.
    ///
    /// The implementation MUST:
    /// - compute the SHA-256 of `bytes`;
    /// - prefix the object key with the tenant scope;
    /// - return a [`BlobLocator`] carrying the hash and length.
    ///
    /// Identical bytes yield identical locators (deduplication).
    async fn put(&self, tenant: &str, bytes: &[u8]) -> Result<BlobLocator, BlobError>;

    /// Retrieve the bytes for `locator`.
    ///
    /// The implementation MUST verify the content hash on read and return
    /// [`BlobError::Integrity`] when the stored object does not match.
    async fn get(&self, locator: &BlobLocator) -> Result<Vec<u8>, BlobError>;

    /// Delete the blob keyed by `locator`.
    ///
    /// Returns `true` if the object existed and was deleted, `false` if it
    /// was already absent. Deleting a shared content-addressed blob is
    /// idempotent and safe — the object remains until the last reference
    /// is dropped (purge logic lives above this seam).
    async fn delete(&self, locator: &BlobLocator) -> Result<bool, BlobError>;

    /// Return `true` if the blob exists.
    async fn exists(&self, locator: &BlobLocator) -> Result<bool, BlobError>;
}

/// A local filesystem-backed blob store using `object_store`.
///
/// This is the reference implementation for development and single-node
/// deployments. Each tenant gets a subdirectory under `base_path`;
/// objects are keyed by SHA-256 inside that subdirectory.
#[derive(Debug, Clone)]
pub struct LocalBlobStore {
    inner: Arc<LocalFileSystem>,
    base_path: PathBuf,
}

impl LocalBlobStore {
    /// Create a new local blob store rooted at `base_path`.
    ///
    /// The directory need not exist; it will be created on first use.
    pub fn new(base_path: impl Into<PathBuf>) -> Result<Self, std::io::Error> {
        let path = base_path.into();
        std::fs::create_dir_all(&path)?;
        // object_store::local::LocalFileSystem::new_with_prefix wants the
        // path to exist and will use it as the root for all operations.
        let inner = LocalFileSystem::new_with_prefix(&path)?;
        Ok(Self {
            inner: Arc::new(inner),
            base_path: path,
        })
    }

    /// Return the base path.
    pub fn base_path(&self) -> &std::path::Path {
        &self.base_path
    }

    fn tenant_prefix(tenant: &str) -> String {
        format!("{}/", tenant)
    }
}

#[async_trait::async_trait]
impl BlobStore for LocalBlobStore {
    async fn put(&self, tenant: &str, bytes: &[u8]) -> Result<BlobLocator, BlobError> {
        let hash = Sha256::digest(bytes);
        let sha256 = hex::encode(hash);
        let prefix = Self::tenant_prefix(tenant);
        let key = format!("{}{}", prefix, sha256);

        let payload = PutPayload::from(bytes.to_vec());
        self.inner
            .put(&key.into(), payload)
            .await
            .map_err(|e| BlobError::Unavailable(format!("put failed: {e}")))?;

        Ok(BlobLocator {
            prefix,
            sha256,
            bytes: bytes.len() as u64,
        })
    }

    async fn get(&self, locator: &BlobLocator) -> Result<Vec<u8>, BlobError> {
        let key = locator.key();
        let result = self.inner.get(&key.into()).await.map_err(|e| match e {
            object_store::Error::NotFound { .. } => BlobError::NotFound(locator.to_string()),
            _ => BlobError::Unavailable(format!("get failed: {e}")),
        })?;

        let bytes = result
            .bytes()
            .await
            .map_err(|e| BlobError::Unavailable(format!("read failed: {e}")))?;

        let computed = Sha256::digest(&bytes);
        let actual = hex::encode(computed);
        if actual != locator.sha256 {
            return Err(BlobError::Integrity {
                locator: locator.clone(),
                expected: locator.sha256.clone(),
                actual,
            });
        }

        Ok(bytes.to_vec())
    }

    async fn delete(&self, locator: &BlobLocator) -> Result<bool, BlobError> {
        let key = locator.key();
        match self.inner.delete(&key.into()).await {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(e) => Err(BlobError::Unavailable(format!("delete failed: {e}"))),
        }
    }

    async fn exists(&self, locator: &BlobLocator) -> Result<bool, BlobError> {
        let key = locator.key();
        match self.inner.head(&key.into()).await {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(e) => Err(BlobError::Unavailable(format!("head failed: {e}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_blob_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalBlobStore::new(dir.path()).unwrap();
        let bytes = b"hello blob world";

        let locator = store.put("acme", bytes.as_slice()).await.unwrap();
        assert_eq!(locator.bytes, bytes.len() as u64);
        assert_eq!(locator.sha256.len(), 64);
        assert!(locator.prefix.starts_with("acme"));

        let got = store.get(&locator).await.unwrap();
        assert_eq!(got, bytes.as_slice());
    }

    #[tokio::test]
    async fn local_blob_integrity_check() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalBlobStore::new(dir.path()).unwrap();
        let bytes = b"integrity test";

        let locator = store.put("acme", bytes.as_slice()).await.unwrap();

        // Corrupt the file on disk.
        let corrupt_path = dir.path().join(&locator.prefix).join(&locator.sha256);
        std::fs::write(&corrupt_path, b"corrupted").unwrap();

        let err = store.get(&locator).await.unwrap_err();
        assert!(
            matches!(err, BlobError::Integrity { .. }),
            "expected integrity error, got {err:?}"
        );
    }

    #[tokio::test]
    async fn local_blob_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalBlobStore::new(dir.path()).unwrap();
        let locator = BlobLocator::new("acme", "a".repeat(64), 0);

        let err = store.get(&locator).await.unwrap_err();
        assert!(matches!(err, BlobError::NotFound(..)));
    }

    #[tokio::test]
    async fn local_blob_dedup() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalBlobStore::new(dir.path()).unwrap();
        let bytes = b"deduplicated content";

        let loc1 = store.put("acme", bytes.as_slice()).await.unwrap();
        let loc2 = store.put("acme", bytes.as_slice()).await.unwrap();
        assert_eq!(loc1.sha256, loc2.sha256);
    }

    #[tokio::test]
    async fn local_blob_tenant_isolation() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalBlobStore::new(dir.path()).unwrap();
        let bytes = b"tenant secret";

        let loc_a = store.put("tenant-a", bytes.as_slice()).await.unwrap();
        let loc_b = store.put("tenant-b", bytes.as_slice()).await.unwrap();

        // Same content, different prefixes.
        assert_eq!(loc_a.sha256, loc_b.sha256);
        assert_ne!(loc_a.prefix, loc_b.prefix);

        // tenant-a cannot read tenant-b's blob by swapping prefixes
        // (the key would be wrong).
        let swapped = BlobLocator::new("tenant-b", &loc_a.sha256, loc_a.bytes);
        let err = store.get(&swapped).await.unwrap_err();
        assert!(matches!(err, BlobError::NotFound(..)));
    }

    #[tokio::test]
    async fn local_blob_delete_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalBlobStore::new(dir.path()).unwrap();
        let bytes = b"delete me";

        let locator = store.put("acme", bytes.as_slice()).await.unwrap();
        assert!(store.exists(&locator).await.unwrap());

        assert!(store.delete(&locator).await.unwrap());
        assert!(!store.exists(&locator).await.unwrap());

        // Second delete is idempotent.
        assert!(!store.delete(&locator).await.unwrap());
    }
}
