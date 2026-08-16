//! The knowledge store contract: content-addressed bytes, governed source
//! versions, and the chunk indexes citations resolve through.
//!
//! [`ContentAddressedStore`] is the seam the server slices swap behind: the
//! in-memory implementation here is the dev/test reference (the same role
//! `InMemoryMemoryStore` plays for the memory plane); file and Postgres
//! backends implement the same trait over their own layouts. The contract
//! deliberately knows nothing about tenants — tenant isolation is the
//! server's `{tenant}/` id-namespacing applied around this contract, not
//! inside it.
//!
//! Three storage disciplines meet here:
//!
//! - **Content addressing.** Bytes are stored under their `sha256`;
//!   `put_content` verifies the address against the bytes (a mismatch fails
//!   closed — storing bytes under a wrong address would poison every
//!   citation that resolves through it) and is idempotent (the same bytes
//!   under the same address converge).
//! - **Immutability.** Source versions and chunk lists are write-once,
//!   keyed by content hash; re-putting identical data converges, and
//!   putting *different* data under a key that exists fails — a version is
//!   an immutable fact, and a key that would change meaning under it is a
//!   correctness bug, not an update.
//! - **Tombstoned deletion.** Purge removes content, versions, and chunk
//!   indexes but leaves [`SourceTombstone`]s, so citations in old journals
//!   stay resolvable to metadata.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

use async_trait::async_trait;

use crate::error::Result;
use crate::record::sha256_hex;

use super::{invalid, ChunkRecord, KnowledgeSource, SourceTombstone};

/// The knowledge store contract. See the module docs for the disciplines;
/// every method documents its idempotency.
#[async_trait]
pub trait ContentAddressedStore: Send + Sync + std::fmt::Debug {
    /// Store `bytes` under `address`. Idempotent: the same bytes under the
    /// same address yield `false` (already present). Fails closed when
    /// `address` is not the `sha256` of `bytes`.
    async fn put_content(&self, address: &str, bytes: &[u8]) -> Result<bool>;

    /// Fetch stored bytes by content address (`None` when absent).
    async fn get_content(&self, address: &str) -> Result<Option<Vec<u8>>>;

    /// Remove content by address (`false` when absent). The sweep calls
    /// this only after [`ContentAddressedStore::content_in_use`] reports no
    /// surviving chunk references the address.
    async fn remove_content(&self, address: &str) -> Result<bool>;

    /// Store one source version under its content hash. Idempotent for an
    /// identical record; fails when a *different* record already occupies
    /// the hash — a version is immutable, and the hash is its identity.
    async fn put_source(&self, source: &KnowledgeSource) -> Result<bool>;

    /// Fetch one source version by content hash (`None` when absent).
    /// Superseded versions stay addressable here — they are evidence.
    async fn get_source(&self, content_hash: &str) -> Result<Option<KnowledgeSource>>;

    /// Every source version in the store (the query universe).
    async fn all_sources(&self) -> Result<Vec<KnowledgeSource>>;

    /// Remove one source version by content hash (`false` when absent).
    /// The sweep's deletion path; callers wanting addressability kept use
    /// supersession, not removal.
    async fn remove_source(&self, content_hash: &str) -> Result<bool>;

    /// Store the chunk list of one source version. Write-once:
    /// re-putting an identical list converges; a different list under an
    /// existing version hash fails.
    async fn put_chunks(&self, chunks: &[ChunkRecord]) -> Result<()>;

    /// The chunk list of one source version, in chunk order (empty when
    /// the version is unknown or purged).
    async fn chunks_of(&self, source_hash: &str) -> Result<Vec<ChunkRecord>>;

    /// Remove one version's chunk list (`false` when absent), including
    /// the reverse-index entries.
    async fn remove_chunks(&self, source_hash: &str) -> Result<bool>;

    /// The reverse index: which source version a chunk content address
    /// belongs to (`None` when unknown or purged).
    async fn source_of_chunk(&self, content_address: &str) -> Result<Option<String>>;

    /// Record a purge tombstone. Write-once per source id: a re-purge of a
    /// tombstoned source converges on the first tombstone (purging is
    /// idempotent; the earliest receipt is the evidence).
    async fn put_tombstone(&self, tombstone: &SourceTombstone) -> Result<()>;

    /// The tombstone for a purged source id (`None` while the source is
    /// alive or unknown) — the lookup that keeps old citations resolvable
    /// to metadata.
    async fn tombstone_for(&self, source_id: &str) -> Result<Option<SourceTombstone>>;

    /// Every tombstone in the store (the audit view of purges).
    async fn all_tombstones(&self) -> Result<Vec<SourceTombstone>>;

    /// Every version of one source, ordered by version number. The default
    /// implementation scans [`ContentAddressedStore::all_sources`];
    /// backends with indexes override the scan, never the semantics.
    async fn versions_of(&self, source_id: &str) -> Result<Vec<KnowledgeSource>> {
        let mut versions: Vec<KnowledgeSource> = self
            .all_sources()
            .await?
            .into_iter()
            .filter(|source| source.source_id == source_id)
            .collect();
        versions.sort_by_key(|source| source.version);
        Ok(versions)
    }

    /// Whether any surviving chunk list references `address`. The sweep
    /// consults this before removing content bytes: identical chunks can
    /// appear in several versions of a source (and across sources), so a
    /// content address dies only with its last reference. The default
    /// implementation scans every chunk list; backends override with a
    /// refcount or an index query.
    async fn content_in_use(&self, address: &str) -> Result<bool> {
        for source in self.all_sources().await? {
            if self
                .chunks_of(&source.content_hash)
                .await?
                .iter()
                .any(|chunk| chunk.content_address == address)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// The state of [`InMemoryContentAddressedStore`], behind one lock so the
/// indexes never observe each other mid-write.
#[derive(Debug, Default)]
struct StoreInner {
    /// Content address → bytes.
    content: BTreeMap<String, Vec<u8>>,
    /// Version content hash → source record.
    sources: BTreeMap<String, KnowledgeSource>,
    /// Version content hash → chunk list (source → chunks index).
    chunks: BTreeMap<String, Vec<ChunkRecord>>,
    /// Chunk content address → version content hash (chunk → source
    /// reverse index).
    chunk_reverse: BTreeMap<String, String>,
    /// Source id → tombstone.
    tombstones: BTreeMap<String, SourceTombstone>,
}

/// In-memory [`ContentAddressedStore`] (dev and test): plain locked maps.
/// No persistence, no tenant concept — the honest reference for the
/// contract's semantics.
#[derive(Debug, Default)]
pub struct InMemoryContentAddressedStore {
    inner: Mutex<StoreInner>,
}

impl InMemoryContentAddressedStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, StoreInner> {
        // Poison means a store path panicked mid-write; the maps are plain
        // data and stay coherent, so recovering is safe (the same reading
        // the memory plane's in-memory store takes).
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[async_trait]
impl ContentAddressedStore for InMemoryContentAddressedStore {
    async fn put_content(&self, address: &str, bytes: &[u8]) -> Result<bool> {
        let actual = sha256_hex(bytes);
        if actual != address {
            return Err(invalid(format!(
                "content address mismatch: {address} was declared but the bytes hash to \
                 {actual} — storing bytes under a wrong address would poison every citation \
                 that resolves through it"
            )));
        }
        let mut inner = self.lock();
        if inner.content.contains_key(address) {
            return Ok(false);
        }
        inner.content.insert(address.to_owned(), bytes.to_vec());
        Ok(true)
    }

    async fn get_content(&self, address: &str) -> Result<Option<Vec<u8>>> {
        Ok(self.lock().content.get(address).cloned())
    }

    async fn remove_content(&self, address: &str) -> Result<bool> {
        Ok(self.lock().content.remove(address).is_some())
    }

    async fn put_source(&self, source: &KnowledgeSource) -> Result<bool> {
        let mut inner = self.lock();
        match inner.sources.get(&source.content_hash) {
            Some(existing) if existing == source => Ok(false),
            Some(_) => Err(invalid(format!(
                "a different source record already occupies content hash {} — a version is \
                 immutable, and its hash is its identity",
                source.content_hash
            ))),
            None => {
                inner
                    .sources
                    .insert(source.content_hash.clone(), source.clone());
                Ok(true)
            }
        }
    }

    async fn get_source(&self, content_hash: &str) -> Result<Option<KnowledgeSource>> {
        Ok(self.lock().sources.get(content_hash).cloned())
    }

    async fn all_sources(&self) -> Result<Vec<KnowledgeSource>> {
        Ok(self.lock().sources.values().cloned().collect())
    }

    async fn remove_source(&self, content_hash: &str) -> Result<bool> {
        Ok(self.lock().sources.remove(content_hash).is_some())
    }

    async fn put_chunks(&self, chunks: &[ChunkRecord]) -> Result<()> {
        let Some(first) = chunks.first() else {
            return Ok(());
        };
        let source_hash = first.source_hash.clone();
        if chunks.iter().any(|chunk| chunk.source_hash != source_hash) {
            return Err(invalid(
                "one put_chunks call must carry exactly one source version's chunks",
            ));
        }
        let mut inner = self.lock();
        if let Some(existing) = inner.chunks.get(&source_hash) {
            if existing == chunks {
                return Ok(());
            }
            return Err(invalid(format!(
                "different chunks already occupy source version {source_hash} — a version's \
                 chunk list is write-once"
            )));
        }
        for chunk in chunks {
            inner
                .chunk_reverse
                .insert(chunk.content_address.clone(), source_hash.clone());
        }
        inner.chunks.insert(source_hash, chunks.to_vec());
        Ok(())
    }

    async fn chunks_of(&self, source_hash: &str) -> Result<Vec<ChunkRecord>> {
        Ok(self.lock().chunks.get(source_hash).cloned().unwrap_or_default())
    }

    async fn remove_chunks(&self, source_hash: &str) -> Result<bool> {
        let mut inner = self.lock();
        let Some(chunks) = inner.chunks.remove(source_hash) else {
            return Ok(false);
        };
        for chunk in chunks {
            // The reverse map keys on the content address, so a chunk
            // shared across versions names exactly one owner; hand the
            // entry to a surviving version, or drop it when the removed
            // version was the last reference.
            let owner = inner
                .chunks
                .iter()
                .find(|(_, list)| {
                    list.iter()
                        .any(|c| c.content_address == chunk.content_address)
                })
                .map(|(hash, _)| hash.clone());
            match owner {
                Some(hash) => {
                    inner.chunk_reverse.insert(chunk.content_address.clone(), hash);
                }
                None => {
                    inner.chunk_reverse.remove(&chunk.content_address);
                }
            }
        }
        Ok(true)
    }

    async fn source_of_chunk(&self, content_address: &str) -> Result<Option<String>> {
        Ok(self.lock().chunk_reverse.get(content_address).cloned())
    }

    async fn put_tombstone(&self, tombstone: &SourceTombstone) -> Result<()> {
        let mut inner = self.lock();
        inner
            .tombstones
            .entry(tombstone.source_id.clone())
            .or_insert_with(|| tombstone.clone());
        Ok(())
    }

    async fn tombstone_for(&self, source_id: &str) -> Result<Option<SourceTombstone>> {
        Ok(self.lock().tombstones.get(source_id).cloned())
    }

    async fn all_tombstones(&self) -> Result<Vec<SourceTombstone>> {
        Ok(self.lock().tombstones.values().cloned().collect())
    }
}
