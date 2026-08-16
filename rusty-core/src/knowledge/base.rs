//! The knowledge base facade: registration, correction, governed query,
//! and the retention sweep over a [`ContentAddressedStore`].
//!
//! [`KnowledgeBase`] is the one entry point the rest of the runtime (and,
//! later, the server's endpoints and Studio's knowledge workspace) talks
//! to. Every clock read is caller-injected: registration, correction,
//! query, and sweep all take `now`, so a journaled knowledge operation's
//! inputs fully determine its result.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::memory::ScopeAddress;

use super::ingest::{chunk_slice, chunk_source, IngestionConfig};
use super::retrieve::{
    pack_results, rank_lexical, tokenize, Citation, CitedChunk, LexicalConfig, QueryLimits,
    RetrievalWeights, ScoredChunk, VectorScorer,
};
use super::store::ContentAddressedStore;
use super::{
    invalid, KnowledgeSource, PurgeReason, RetentionPolicy, SourceRegistration, SourceTombstone,
};

/// One expired source version a sweep would purge: the dry-run report's
/// line item, metadata only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PurgeEntry {
    /// The source's stable id.
    pub source_id: String,

    /// The expired version's content hash.
    pub source_hash: String,

    /// The expired version's body content address (the purge's content-GC
    /// input — a body shared with a surviving source must survive).
    pub body_hash: String,

    /// The scope the version lives at.
    pub scope: ScopeAddress,

    /// The version's title.
    pub title: String,

    /// The version number in the correction chain.
    pub version: u64,

    /// When the version expired.
    pub expires_at: DateTime<Utc>,

    /// How many chunks the version carries.
    pub chunk_count: u32,

    /// The version's chunk bytes in sum.
    pub chunk_bytes: u64,
}

/// What a sweep at `now` would do, computed over the store **before**
/// anything is deleted: planning is a pure function of store state, so an
/// operator (or a journaled sweep) reads exactly what apply-mode will
/// execute. Entries are sorted by `(source_id, content_hash)` — the plan is
/// deterministic.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RetentionPlan {
    /// The expired versions, sorted.
    pub entries: Vec<PurgeEntry>,

    /// The chunk bytes the purge would remove in sum.
    pub total_chunk_bytes: u64,
}

impl RetentionPlan {
    /// `true` when nothing is purgeable.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// The receipt of an executed sweep: the plan that ran and the tombstones
/// it left. The tombstones are what citations in old journals resolve to
/// after the purge.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RetentionReceipt {
    /// The plan that executed.
    pub plan: RetentionPlan,

    /// The tombstones written, sorted by source id.
    pub tombstones: Vec<SourceTombstone>,
}

/// The governed knowledge base: sources, chunks, retrieval, corrections,
/// and retention over one store backend.
///
/// Cheap to clone (the store and scorer are shared handles), so node
/// closures and tools capture it the way they capture a [`crate::journal::Journal`].
#[derive(Debug, Clone)]
pub struct KnowledgeBase {
    store: Arc<dyn ContentAddressedStore>,
    ingestion: IngestionConfig,
    lexical: LexicalConfig,
    weights: RetrievalWeights,
    vector: Option<Arc<dyn VectorScorer>>,
}

impl KnowledgeBase {
    /// A knowledge base over `store` with the default ingestion, lexical,
    /// and weighting configuration (lexical-only retrieval).
    pub fn new(store: Arc<dyn ContentAddressedStore>) -> Self {
        Self {
            store,
            ingestion: IngestionConfig::default(),
            lexical: LexicalConfig::default(),
            weights: RetrievalWeights::default(),
            vector: None,
        }
    }

    /// Builder-style: override the ingestion tuning.
    pub fn with_ingestion_config(mut self, config: IngestionConfig) -> Self {
        self.ingestion = config;
        self
    }

    /// Builder-style: override the BM25-lite tuning.
    pub fn with_lexical_config(mut self, config: LexicalConfig) -> Self {
        self.lexical = config;
        self
    }

    /// Builder-style: override the hybrid weights.
    pub fn with_weights(mut self, weights: RetrievalWeights) -> Self {
        self.weights = weights;
        self
    }

    /// Builder-style: install the vector half of hybrid retrieval.
    pub fn with_vector_scorer(mut self, scorer: Arc<dyn VectorScorer>) -> Self {
        self.vector = Some(scorer);
        self
    }

    /// Register a new source: validate, normalize, chunk, and store.
    ///
    /// Idempotent on content: re-registering a body already present under
    /// `source_id` returns the stored version (registration metadata is
    /// attached at first write — a version's identity is its content hash).
    /// Registering a *different* body under an existing `source_id` fails
    /// closed: changing a source is a correction
    /// ([`KnowledgeBase::correct_source`]), never a silent overwrite.
    pub async fn register_source(
        &self,
        registration: SourceRegistration,
        body: &str,
        now: DateTime<Utc>,
    ) -> Result<KnowledgeSource> {
        let source = registration.build(body, now)?;
        let versions = self.store.versions_of(&source.source_id).await?;
        if let Some(existing) = versions
            .iter()
            .find(|version| version.content_hash == source.content_hash)
        {
            return Ok(existing.clone());
        }
        if !versions.is_empty() {
            return Err(invalid(format!(
                "knowledge source `{}` already exists at version {} — changing a source is a \
                 correction (correct_source), never a silent overwrite",
                source.source_id,
                versions.len()
            )));
        }
        let normalized = super::normalize_text(body);
        self.ingest_and_store(&source, &normalized).await?;
        Ok(source)
    }

    /// Correct a source: mint a new version whose content supersedes the
    /// current latest, mirroring the memory plane's supersession rule.
    ///
    /// The new version inherits the source's scope, kind, title,
    /// confidence, and retention; `author` is the corrector's provenance
    /// string (mandatory — a correction that cannot name its corrector is
    /// indistinguishable from a rewrite). Retrieval stops returning the old
    /// version's chunks immediately; the old version stays addressable by
    /// hash as evidence. Fails closed when the source is unknown, when the
    /// corrected body is byte-identical to the latest version (a correction
    /// that changes nothing is not a correction), or when the corrected
    /// body collides with an earlier version's hash (revert-by-correction
    /// would fork the chain's identity).
    pub async fn correct_source(
        &self,
        source_id: &str,
        author: &str,
        body: &str,
        now: DateTime<Utc>,
    ) -> Result<KnowledgeSource> {
        let versions = self.store.versions_of(source_id).await?;
        let Some(latest) = versions.last() else {
            return Err(invalid(format!(
                "cannot correct unknown knowledge source `{source_id}` — corrections supersede \
                 a registered version, they do not register one"
            )));
        };
        let registration = SourceRegistration {
            source_id: latest.source_id.clone(),
            scope: latest.scope.clone(),
            kind: latest.kind,
            title: latest.title.clone(),
            author: author.to_owned(),
            confidence: latest.confidence,
            retention: latest.retention,
        };
        let mut source = registration.build(body, now)?;
        if source.content_hash == latest.content_hash {
            return Err(invalid(format!(
                "correction of knowledge source `{source_id}` is byte-identical to version {} \
                 — a correction that changes nothing is not a correction",
                latest.version
            )));
        }
        source.version = latest.version + 1;
        source.supersedes = Some(latest.content_hash.clone());
        let normalized = super::normalize_text(body);
        self.ingest_and_store(&source, &normalized).await?;
        Ok(source)
    }

    /// Chunk and store one validated source version. The body bytes are
    /// stored under the pure body hash (shared across identical bodies);
    /// the source record is stored last, so a version becomes visible only
    /// fully ingested.
    async fn ingest_and_store(&self, source: &KnowledgeSource, normalized: &str) -> Result<()> {
        self.store
            .put_content(&source.body_hash, normalized.as_bytes())
            .await?;
        let chunks = chunk_source(source, normalized, &self.ingestion)?;
        for chunk in &chunks {
            self.store
                .put_content(
                    &chunk.content_address,
                    chunk_slice(normalized, chunk).as_bytes(),
                )
                .await?;
        }
        self.store.put_chunks(&chunks).await?;
        self.store.put_source(source).await?;
        Ok(())
    }

    /// The governed query: scope isolation, supersession, expiry, hybrid
    /// ranking, and bounded cited results in one call.
    ///
    /// Scope isolation is a filter, not an error channel: a scope with no
    /// matching live sources answers with an empty result — cross-scope
    /// reads return nothing and leak nothing. Superseded and expired
    /// versions are filtered before ranking; purged versions have no chunks
    /// left to rank. Fails closed on invalid limits, on a hybrid weighting
    /// with no scorer installed, and on a query with no searchable terms
    /// (a punctuation-only query cannot rank).
    pub async fn query(
        &self,
        scope: &ScopeAddress,
        text: &str,
        limits: &QueryLimits,
        now: DateTime<Utc>,
    ) -> Result<Vec<CitedChunk>> {
        limits.validate()?;
        self.weights.validate()?;
        if self.weights.vector > 0.0 && self.vector.is_none() {
            return Err(invalid(
                "retrieval weights declare a vector component but no VectorScorer is installed \
                 — fail closed rather than silently rank lexical-only under a hybrid \
                 configuration",
            ));
        }
        if tokenize(text).is_empty() {
            return Err(invalid(
                "knowledge query carries no searchable terms — a query that cannot rank is a \
                 caller bug, not an empty result",
            ));
        }

        let sources = self.store.all_sources().await?;
        let superseded: HashSet<&str> = sources
            .iter()
            .filter_map(|source| source.supersedes.as_deref())
            .collect();
        let live: Vec<&KnowledgeSource> = sources
            .iter()
            .filter(|source| {
                &source.scope == scope
                    && !superseded.contains(source.content_hash.as_str())
                    && source.is_live_at(now)
            })
            .collect();

        // Resolve chunk text from the content-addressed store. Chunk bytes
        // are UTF-8 by construction (they are slices of a validated UTF-8
        // body); anything else is store corruption and fails loudly.
        let mut records = Vec::new();
        let mut texts: Vec<String> = Vec::new();
        for source in &live {
            for chunk in self.store.chunks_of(&source.content_hash).await? {
                let bytes = self
                    .store
                    .get_content(&chunk.content_address)
                    .await?
                    .ok_or_else(|| {
                        invalid(format!(
                            "chunk {} of source `{}` is indexed but its content is missing \
                                 from the store — the index and the content store disagree",
                            chunk.chunk_id, source.source_id
                        ))
                    })?;
                let text = String::from_utf8(bytes).map_err(|_| {
                    invalid(format!(
                        "chunk {} of source `{}` is not valid UTF-8 — store corruption",
                        chunk.chunk_id, source.source_id
                    ))
                })?;
                records.push(chunk);
                texts.push(text);
            }
        }
        let corpus: Vec<ScoredChunk<'_>> = records
            .iter()
            .zip(texts.iter())
            .map(|(chunk, text)| ScoredChunk { chunk, text })
            .collect();

        let ranked = rank_lexical(&corpus, text, &self.lexical)?;
        let mut lexical_scores = vec![0.0_f64; corpus.len()];
        for (index, score) in ranked {
            lexical_scores[index] = score;
        }
        let titles: BTreeMap<&str, &str> = live
            .iter()
            .map(|source| (source.content_hash.as_str(), source.title.as_str()))
            .collect();
        // Hybrid combination over the whole live corpus: the vector
        // component has recall beyond lexical matches (a chunk sharing no
        // terms with the query can still rank on embedding similarity), so
        // scoring runs corpus-wide and the combined score filters.
        let mut combined: Vec<(usize, f64)> = (0..corpus.len())
            .filter_map(|index| {
                let vector_score = match &self.vector {
                    Some(scorer) if self.weights.vector > 0.0 => scorer
                        .score(text, corpus[index].text, corpus[index].chunk)
                        .unwrap_or(0.0),
                    _ => 0.0,
                };
                let score = self.weights.lexical * lexical_scores[index]
                    + self.weights.vector * vector_score;
                (score > 0.0).then_some((index, score))
            })
            .collect();
        // The final sort runs after combining — the same total order:
        // score descending, content address ascending.
        combined.sort_by(|(a_idx, a_score), (b_idx, b_score)| {
            b_score.total_cmp(a_score).then_with(|| {
                corpus[*a_idx]
                    .chunk
                    .content_address
                    .cmp(&corpus[*b_idx].chunk.content_address)
            })
        });
        let results = combined
            .into_iter()
            .map(|(index, score)| {
                let chunk = corpus[index].chunk;
                CitedChunk {
                    citation: Citation {
                        source_id: chunk.source_id.clone(),
                        source_hash: chunk.source_hash.clone(),
                        title: titles
                            .get(chunk.source_hash.as_str())
                            .copied()
                            .unwrap_or_default()
                            .to_owned(),
                        chunk_id: chunk.chunk_id.clone(),
                        chunk_index: chunk.chunk_index,
                        content_address: chunk.content_address.clone(),
                        byte_start: chunk.byte_start,
                        byte_end: chunk.byte_end,
                    },
                    text: corpus[index].text.to_owned(),
                    score,
                    word_count: chunk.word_count,
                }
            })
            .collect();
        Ok(pack_results(results, limits))
    }

    /// The retention sweep's dry-run: what a sweep at `now` *would* purge —
    /// every expired, unpinned version with its chunk accounting, sorted,
    /// computed before anything is deleted.
    pub async fn plan_sweep(&self, now: DateTime<Utc>) -> Result<RetentionPlan> {
        let mut entries = Vec::new();
        for source in self.store.all_sources().await? {
            let RetentionPolicy::Ttl { expires_at } = source.retention else {
                continue;
            };
            if source.is_live_at(now) {
                continue;
            }
            let chunks = self.store.chunks_of(&source.content_hash).await?;
            entries.push(PurgeEntry {
                source_id: source.source_id.clone(),
                source_hash: source.content_hash.clone(),
                body_hash: source.body_hash.clone(),
                scope: source.scope.clone(),
                title: source.title.clone(),
                version: source.version,
                expires_at,
                chunk_count: chunks.len() as u32,
                chunk_bytes: chunks.iter().map(|chunk| chunk.bytes).sum(),
            });
        }
        entries.sort_by(|a, b| {
            a.source_id
                .cmp(&b.source_id)
                .then_with(|| a.source_hash.cmp(&b.source_hash))
        });
        let total_chunk_bytes = entries.iter().map(|entry| entry.chunk_bytes).sum();
        Ok(RetentionPlan {
            entries,
            total_chunk_bytes,
        })
    }

    /// The retention sweep's apply mode: execute [`KnowledgeBase::plan_sweep`]
    /// exactly. Purging removes the expired versions' chunks, bodies, and
    /// source records — content addresses die only with their last
    /// reference, so chunks shared across versions survive — and tombstones
    /// each purged source id ([`SourceTombstone`], metadata by
    /// construction), so citations in old journals stay resolvable.
    pub async fn apply_sweep(&self, now: DateTime<Utc>) -> Result<RetentionReceipt> {
        let plan = self.plan_sweep(now).await?;

        // Remove chunk indexes and source records first, so the
        // content-in-use checks below see only surviving references.
        let mut candidate_addresses: Vec<String> = Vec::new();
        for entry in &plan.entries {
            for chunk in self.store.chunks_of(&entry.source_hash).await? {
                candidate_addresses.push(chunk.content_address);
            }
            self.store.remove_chunks(&entry.source_hash).await?;
            self.store.remove_source(&entry.source_hash).await?;
            candidate_addresses.push(entry.body_hash.clone());
        }
        // Content GC: an address dies with its last reference — a surviving
        // version's chunk list, or a surviving source's body (a chunk slice
        // can coincide with a whole body, and identical bodies share one
        // body address across sources).
        let surviving_bodies: HashSet<String> = self
            .store
            .all_sources()
            .await?
            .into_iter()
            .map(|source| source.body_hash)
            .collect();
        for address in candidate_addresses {
            if surviving_bodies.contains(&address) || self.store.content_in_use(&address).await? {
                continue;
            }
            self.store.remove_content(&address).await?;
        }

        // One tombstone per purged source id (a source whose versions all
        // expired purges them together), metadata by construction.
        let mut by_source: BTreeMap<String, Vec<&PurgeEntry>> = BTreeMap::new();
        for entry in &plan.entries {
            by_source
                .entry(entry.source_id.clone())
                .or_default()
                .push(entry);
        }
        let mut tombstones = Vec::new();
        for (source_id, entries) in by_source {
            let latest = entries
                .iter()
                .max_by_key(|entry| entry.version)
                .expect("grouped entries are non-empty");
            let mut purged_hashes: Vec<String> = entries
                .iter()
                .map(|entry| entry.source_hash.clone())
                .collect();
            purged_hashes.sort();
            let tombstone = SourceTombstone {
                source_id,
                scope: latest.scope.clone(),
                title: latest.title.clone(),
                purged_hashes,
                reason: PurgeReason::Expired,
                purged_at: now,
            };
            self.store.put_tombstone(&tombstone).await?;
            tombstones.push(tombstone);
        }
        Ok(RetentionReceipt { plan, tombstones })
    }

    /// Fetch one source version by content hash — superseded versions
    /// included, because they are evidence.
    pub async fn get_source(&self, content_hash: &str) -> Result<Option<KnowledgeSource>> {
        self.store.get_source(content_hash).await
    }

    /// Fetch chunk text by content address — the resolution path a citation
    /// in an old journal walks while the content lives.
    pub async fn chunk_content(&self, content_address: &str) -> Result<Option<String>> {
        let Some(bytes) = self.store.get_content(content_address).await? else {
            return Ok(None);
        };
        String::from_utf8(bytes).map(Some).map_err(|_| {
            invalid(format!(
                "content at {content_address} is not valid UTF-8 — store corruption"
            ))
        })
    }

    /// The tombstone a purged source id left behind (`None` while the
    /// source is alive or was never registered).
    pub async fn tombstone(&self, source_id: &str) -> Result<Option<SourceTombstone>> {
        self.store.tombstone_for(source_id).await
    }

    /// Every version of one source, ordered by version number — the
    /// correction chain, evidence included.
    pub async fn versions_of(&self, source_id: &str) -> Result<Vec<KnowledgeSource>> {
        self.store.versions_of(source_id).await
    }
}
