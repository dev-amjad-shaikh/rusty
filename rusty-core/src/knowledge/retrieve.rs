//! Hybrid retrieval: a BM25-lite lexical rank, an optional vector
//! component behind a trait, and the bounded, cited result set.
//!
//! Retrieval in the knowledge plane is deterministic end to end:
//!
//! - **Lexical scoring** ([`rank_lexical`]) is a BM25-lite — term frequency
//!   with document-length normalization and an inverse-document-frequency
//!   weight — over whitespace/punctuation tokenization. It is a pure
//!   function of the chunk set and the query text; no clocks, no randomness.
//! - **Vector scoring** is the [`VectorScorer`] trait: core ships no
//!   embedding dependencies, so the default knowledge base runs lexical-only
//!   ([`RetrievalWeights`] default `1.0 / 0.0`). An embedding backend plugs
//!   in behind the trait without changing the query path, the same seam the
//!   memory plane reserves with its `embedding` field.
//! - **Ranking is a total order**: score descending, content address
//!   ascending as the final tie-break. Equal store state and equal query
//!   produce byte-equal result sets.
//! - **Results are bounded and cited.** [`QueryLimits`] caps count and
//!   bytes (truncate, never spill); every result is a [`CitedChunk`] — the
//!   text *with* its [`Citation`] — never bare text an agent could quote
//!   without attribution.

use serde::{Deserialize, Serialize};

use crate::error::Result;

use super::{invalid, ChunkRecord};

/// The default BM25 term-frequency saturation (`k1`).
pub const DEFAULT_BM25_K1: f64 = 1.2;

/// The default BM25 document-length normalization (`b`).
pub const DEFAULT_BM25_B: f64 = 0.75;

/// The default maximum number of results a query returns.
pub const DEFAULT_MAX_RESULTS: usize = 20;

/// The hard ceiling on [`QueryLimits::max_results`]: above this, a result
/// set is a dump, not a retrieval.
pub const MAX_RESULTS_CEILING: usize = 100;

/// The default maximum total bytes of chunk text a query returns.
pub const DEFAULT_MAX_RESULT_BYTES: usize = 64 * 1024;

/// The hard ceiling on [`QueryLimits::max_bytes`].
pub const MAX_RESULT_BYTES_CEILING: usize = 1024 * 1024;

/// BM25-lite tuning. The defaults are the standard `k1 = 1.2`, `b = 0.75`;
/// they live on a config so deployments can tune without forking the
/// ranker.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LexicalConfig {
    /// Term-frequency saturation: higher values let repeated terms keep
    /// paying off.
    pub k1: f64,

    /// Document-length normalization in `[0, 1]`: `0` ignores length, `1`
    /// normalizes fully.
    pub b: f64,
}

impl Default for LexicalConfig {
    fn default() -> Self {
        Self {
            k1: DEFAULT_BM25_K1,
            b: DEFAULT_BM25_B,
        }
    }
}

impl LexicalConfig {
    /// The config's validity rules: finite, `k1 > 0`, `b` in `[0, 1]`.
    pub fn validate(&self) -> Result<()> {
        if !self.k1.is_finite() || self.k1 <= 0.0 {
            return Err(invalid(format!(
                "BM25 k1 must be positive and finite, got {}",
                self.k1
            )));
        }
        if !self.b.is_finite() || !(0.0..=1.0).contains(&self.b) {
            return Err(invalid(format!(
                "BM25 b must be in [0, 1], got {}",
                self.b
            )));
        }
        Ok(())
    }
}

/// How the lexical and vector components combine:
/// `score = lexical * lexical_score + vector * vector_score`. The default
/// is lexical-only — core ships no embedding dependencies, so the vector
/// component is opt-in behind [`VectorScorer`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RetrievalWeights {
    /// Weight of the BM25-lite lexical score.
    pub lexical: f64,

    /// Weight of the [`VectorScorer`] score. Must be `0.0` when no scorer
    /// is installed — the query path fails closed otherwise, rather than
    /// silently ranking lexical-only under a hybrid configuration.
    pub vector: f64,
}

impl Default for RetrievalWeights {
    fn default() -> Self {
        Self {
            lexical: 1.0,
            vector: 0.0,
        }
    }
}

impl RetrievalWeights {
    /// The weights' validity rules: finite, non-negative, and not both
    /// zero (a zero-sum weighting ranks everything at zero — retrieval
    /// that cannot rank is a configuration bug, not an empty result).
    pub fn validate(&self) -> Result<()> {
        for (name, weight) in [("lexical", self.lexical), ("vector", self.vector)] {
            if !weight.is_finite() || weight < 0.0 {
                return Err(invalid(format!(
                    "retrieval weight `{name}` must be non-negative and finite, got {weight}"
                )));
            }
        }
        if self.lexical == 0.0 && self.vector == 0.0 {
            return Err(invalid(
                "retrieval weights cannot both be zero — a zero-sum weighting ranks everything \
                 at zero, and retrieval that cannot rank is a configuration bug",
            ));
        }
        Ok(())
    }
}

/// The optional vector half of hybrid retrieval. Implementations compute
/// embedding similarity between the query and one chunk; core ships none
/// (no embedding dependencies), so this trait is the seam a server-side
/// embedding backend plugs into.
///
/// Determinism is the implementor's contract: the same
/// `(query, chunk_text)` pair must always yield the same score, or the
/// ranking's total order stops being a function of store state.
pub trait VectorScorer: Send + Sync + std::fmt::Debug {
    /// Score `chunk_text` against `query`; `None` means no opinion (treated
    /// as `0.0` in the weighted sum, so a scorer with a cache miss does not
    /// distort ranking).
    fn score(&self, query: &str, chunk_text: &str, chunk: &ChunkRecord) -> Option<f64>;
}

/// The ceilings of one query's result set. Both bounds truncate: packing
/// walks the ranked list and stops at the first result that would exceed a
/// ceiling, the same overflow rule the memory plane's assembly takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryLimits {
    /// At most this many results (1..=[`MAX_RESULTS_CEILING`]).
    pub max_results: usize,

    /// The results' chunk texts sum to at most this many bytes
    /// (1..=[`MAX_RESULT_BYTES_CEILING`]).
    pub max_bytes: usize,
}

impl Default for QueryLimits {
    fn default() -> Self {
        Self {
            max_results: DEFAULT_MAX_RESULTS,
            max_bytes: DEFAULT_MAX_RESULT_BYTES,
        }
    }
}

impl QueryLimits {
    /// The limits' validity rules: both ceilings within their envelopes.
    pub fn validate(&self) -> Result<()> {
        if self.max_results == 0 || self.max_results > MAX_RESULTS_CEILING {
            return Err(invalid(format!(
                "query max_results {} is outside 1..={MAX_RESULTS_CEILING}",
                self.max_results
            )));
        }
        if self.max_bytes == 0 || self.max_bytes > MAX_RESULT_BYTES_CEILING {
            return Err(invalid(format!(
                "query max_bytes {} is outside 1..={MAX_RESULT_BYTES_CEILING}",
                self.max_bytes
            )));
        }
        Ok(())
    }
}

/// The attribution every retrieved chunk renders: enough for an agent (or
/// an auditor walking a journal) to name exactly what was quoted — the
/// source, its title, the chunk, the content address, and the byte range
/// inside the source version's normalized body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Citation {
    /// The source's stable id.
    pub source_id: String,

    /// The source version's content hash — the version the citation
    /// resolves against, addressable as evidence even after supersession.
    pub source_hash: String,

    /// The source's human-facing title.
    pub title: String,

    /// The chunk's stable id (`{source_id}#{index}`).
    pub chunk_id: String,

    /// The chunk's position in its version.
    pub chunk_index: u32,

    /// The chunk's content address.
    pub content_address: String,

    /// Inclusive byte offset into the source version's normalized body.
    pub byte_start: u64,

    /// Exclusive byte offset into the source version's normalized body.
    pub byte_end: u64,
}

/// A retrieved chunk with its citation and score — the only form retrieval
/// returns. There is deliberately no bare-text result anywhere in the
/// plane.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CitedChunk {
    /// The attribution.
    pub citation: Citation,

    /// The chunk's text (an exact slice of the source version's normalized
    /// body).
    pub text: String,

    /// The hybrid score the rank produced.
    pub score: f64,

    /// The chunk's word-count estimate, carried so consumers can budget
    /// without re-tokenizing.
    pub word_count: u32,
}

/// A chunk presented to the ranker: its record plus its text (resolved by
/// the caller from the content-addressed store).
#[derive(Debug, Clone, Copy)]
pub struct ScoredChunk<'a> {
    /// The chunk's metadata.
    pub chunk: &'a ChunkRecord,

    /// The chunk's text.
    pub text: &'a str,
}

/// Tokenize for lexical scoring: lowercase runs of ASCII alphanumerics.
/// Deterministic and dependency-free; a language-aware tokenizer is a
/// later refinement behind the same call sites.
pub fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|term| !term.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Rank `corpus` against `query` with the BM25-lite lexical score.
///
/// Returns `(corpus index, score)` pairs with score > 0, sorted by score
/// descending with the content address as the final tie-break — a total
/// order, so equal corpus state and equal query text rank byte-identically.
/// An empty term set (a query of pure punctuation) ranks nothing; the
/// query path turns that into a fail-closed error.
pub fn rank_lexical(
    corpus: &[ScoredChunk<'_>],
    query: &str,
    config: &LexicalConfig,
) -> Result<Vec<(usize, f64)>> {
    config.validate()?;
    let mut terms = tokenize(query);
    terms.sort();
    terms.dedup();
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    let n = corpus.len();
    if n == 0 {
        return Ok(Vec::new());
    }
    let tokenized: Vec<Vec<String>> = corpus.iter().map(|c| tokenize(c.text)).collect();
    let total_len: usize = tokenized.iter().map(Vec::len).sum();
    let avg_len = total_len as f64 / n as f64;
    // Document frequency per term. BTreeMap iteration is only consumed via
    // lookups; the score sum iterates the sorted query terms, so the float
    // accumulation order is fixed.
    let mut scores: Vec<f64> = vec![0.0; n];
    for term in &terms {
        let df = tokenized
            .iter()
            .filter(|doc| doc.iter().any(|t| t == term))
            .count() as f64;
        let idf = (1.0 + (n as f64 - df + 0.5) / (df + 0.5)).ln();
        for (doc, doc_terms) in tokenized.iter().enumerate() {
            let tf = doc_terms.iter().filter(|t| t.as_str() == term).count() as f64;
            if tf == 0.0 {
                continue;
            }
            let doc_len = doc_terms.len() as f64;
            let norm = 1.0 - config.b + config.b * doc_len / avg_len.max(1.0);
            scores[doc] += idf * (tf * (config.k1 + 1.0)) / (tf + config.k1 * norm);
        }
    }
    let mut ranked: Vec<(usize, f64)> = scores
        .into_iter()
        .enumerate()
        .filter(|(_, score)| *score > 0.0)
        .collect();
    ranked.sort_by(|(a_idx, a_score), (b_idx, b_score)| {
        b_score
            .total_cmp(a_score)
            .then_with(|| {
                corpus[*a_idx]
                    .chunk
                    .content_address
                    .cmp(&corpus[*b_idx].chunk.content_address)
            })
    });
    Ok(ranked)
}

/// Pack a ranked result list under `limits`, truncating at the first
/// result that would exceed either ceiling (the memory plane's
/// truncate-overflow rule: a bounded result set carries what fit, ranked).
pub fn pack_results(ranked: Vec<CitedChunk>, limits: &QueryLimits) -> Vec<CitedChunk> {
    let mut packed = Vec::new();
    let mut used_bytes = 0usize;
    for result in ranked {
        if packed.len() >= limits.max_results {
            break;
        }
        let cost = result.text.len();
        if used_bytes.saturating_add(cost) > limits.max_bytes {
            break;
        }
        used_bytes += cost;
        packed.push(result);
    }
    packed
}
