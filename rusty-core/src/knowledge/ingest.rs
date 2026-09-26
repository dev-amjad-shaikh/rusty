//! Deterministic ingestion: the chunker that turns a governed source body
//! into content-addressed chunks.
//!
//! The contract is **same bytes in, same chunks out**: the chunker is a
//! pure function of the normalized body, the source's kind, and
//! [`IngestionConfig`]. Chunks are exact slices of the normalized body, so
//! a chunk's content address is `sha256` over the slice and its byte range
//! in the citation points into the very text the source registered.
//!
//! Two structural rules:
//!
//! - **Byte-bounded with overlap.** A chunk closes once it reaches
//!   [`IngestionConfig::target_chunk_bytes`]; the next chunk then backs up
//!   by up to [`IngestionConfig::overlap_bytes`], snapped to a line
//!   boundary, so a sentence straddling a boundary is whole in at least one
//!   chunk.
//! - **Fence-aware for Markdown.** A chunk never closes inside a fenced
//!   code block: a fence that would straddle the boundary pulls the whole
//!   block into the current chunk (an unterminated fence runs to the end of
//!   the document). Fence state is a property of each *line*, computed
//!   document-wide before chunking, so overlapping chunk starts cannot
//!   disagree about whether a line sits inside a fence.

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::record::sha256_hex;

use super::{derive_content_hash, invalid, KnowledgeSource, SourceKind};

/// The default chunk target, in bytes: about 1 KiB of normalized text per
/// chunk.
pub const DEFAULT_TARGET_CHUNK_BYTES: usize = 1024;

/// The default overlap between consecutive chunks, in bytes.
pub const DEFAULT_OVERLAP_BYTES: usize = 128;

/// The smallest accepted chunk target: below this, chunks fragment into
/// line-noise and retrieval pays more in bookkeeping than it gains in
/// precision.
pub const MIN_TARGET_CHUNK_BYTES: usize = 256;

/// The largest accepted chunk target, in bytes.
pub const MAX_TARGET_CHUNK_BYTES: usize = 64 * 1024;

/// Chunker tuning. Both constants are caller-declared (per knowledge base,
/// not per source) so an operator can trade chunk granularity against
/// retrieval bookkeeping without touching the chunker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestionConfig {
    /// The size a chunk closes at, in normalized-text bytes. A fence-aware
    /// chunk may exceed it (a code block is never split); every other chunk
    /// closes at the first line boundary at or past the target.
    pub target_chunk_bytes: usize,

    /// How far the next chunk backs up into the previous one, in bytes,
    /// snapped to a line boundary. Must be strictly below
    /// `target_chunk_bytes` — that inequality is what makes the chunk walk
    /// provably advancing.
    pub overlap_bytes: usize,
}

impl Default for IngestionConfig {
    fn default() -> Self {
        Self {
            target_chunk_bytes: DEFAULT_TARGET_CHUNK_BYTES,
            overlap_bytes: DEFAULT_OVERLAP_BYTES,
        }
    }
}

impl IngestionConfig {
    /// The configuration's validity rules: the target is bounded, and the
    /// overlap is strictly smaller (see the field docs for why the strict
    /// inequality is load-bearing).
    pub fn validate(&self) -> Result<()> {
        if self.target_chunk_bytes < MIN_TARGET_CHUNK_BYTES
            || self.target_chunk_bytes > MAX_TARGET_CHUNK_BYTES
        {
            return Err(invalid(format!(
                "ingestion target chunk size {} is outside \
                 {MIN_TARGET_CHUNK_BYTES}..={MAX_TARGET_CHUNK_BYTES} bytes",
                self.target_chunk_bytes
            )));
        }
        if self.overlap_bytes >= self.target_chunk_bytes {
            return Err(invalid(format!(
                "ingestion overlap {} must be strictly below the target chunk size {} — the \
                 strict inequality is what makes the chunk walk provably advancing",
                self.overlap_bytes, self.target_chunk_bytes
            )));
        }
        Ok(())
    }
}

/// One chunk of one source version. The chunk is metadata; its bytes live
/// in the content-addressed store under `content_address` (and are an exact
/// slice `[byte_start, byte_end)` of the normalized body, so both forms
/// agree by construction).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkRecord {
    /// The stable chunk id: `{source_id}#{index}`. Stable across
    /// re-ingestion of identical bytes; two versions of one source share
    /// the id space, which is unambiguous because retrieval only ever sees
    /// one version of a source (the live one).
    pub chunk_id: String,

    /// The source the chunk belongs to (its stable id).
    pub source_id: String,

    /// The source *version* the chunk belongs to (its content hash) — the
    /// key of the store's source → chunks index.
    pub source_hash: String,

    /// The chunk's position in the version's chunk sequence.
    pub chunk_index: u32,

    /// Inclusive byte offset into the normalized source body.
    pub byte_start: u64,

    /// Exclusive byte offset into the normalized source body.
    pub byte_end: u64,

    /// The content address: `sha256` over the chunk's bytes. Chunk bytes
    /// are an exact body slice, so this is also the address of the slice.
    pub content_address: String,

    /// The chunk's size in bytes (`byte_end - byte_start`).
    pub bytes: u64,

    /// The token-ish estimate: a word count. Honest and deterministic; a
    /// provider-precise tokenizer plugs in behind the same field later, the
    /// same reading the memory plane's estimated-token accounting takes.
    pub word_count: u32,
}

/// Chunk one registered source's normalized body under `config`.
///
/// Pure: the same `(source, body, config)` triple always yields the same
/// chunk list, including ids and content addresses. Fails closed when
/// `config` is invalid, when `body` is not the source's normalized body
/// (its `sha256` must equal [`KnowledgeSource::content_hash`] — ingestion
/// cannot chunk bytes the source did not register), or when the body is
/// empty.
pub fn chunk_source(
    source: &KnowledgeSource,
    body: &str,
    config: &IngestionConfig,
) -> Result<Vec<ChunkRecord>> {
    config.validate()?;
    if body.is_empty() {
        return Err(invalid(format!(
            "knowledge source `{}` has an empty body — there is nothing to chunk",
            source.source_id
        )));
    }
    let body_hash = derive_content_hash(&source.source_id, body)?;
    if body_hash != source.content_hash {
        return Err(invalid(format!(
            "ingestion body does not match source `{}` version {} — ingestion cannot chunk \
             bytes the source did not register",
            source.content_hash, source.source_id
        )));
    }

    // Line index: (byte offset of the line's start, byte length including
    // the trailing newline when present). The sentinel at `body.len()`
    // makes end offsets uniform.
    let mut line_offsets: Vec<usize> = Vec::new();
    let mut offset = 0usize;
    for line in body.split_inclusive('\n') {
        line_offsets.push(offset);
        offset += line.len();
    }
    line_offsets.push(body.len());

    // Fence state per line, computed document-wide: `in_fence[i]` is true
    // when line `i` starts inside a fenced block (strictly between the
    // opener and its closer, or on the closer itself), and `open_after[i]`
    // is the state once line `i` is consumed. A chunk boundary before line
    // `i` is clean iff `!in_fence[i]`; a chunk may close after line `i`
    // iff `!open_after[i]`. Both chunk *ends* and overlapped chunk *starts*
    // respect clean boundaries, so every chunk carries balanced fences.
    let line_count = line_offsets.len() - 1;
    let fence_aware = source.kind == SourceKind::Markdown;
    let mut in_fence: Vec<bool> = Vec::with_capacity(line_count);
    let mut open_after: Vec<bool> = Vec::with_capacity(line_count);
    let mut fence_open = false;
    for i in 0..line_count {
        in_fence.push(fence_open);
        let line = &body[line_offsets[i]..line_offsets[i + 1]];
        let is_delimiter = fence_aware && line.trim_start().starts_with("```");
        if is_delimiter {
            fence_open = !fence_open;
        }
        open_after.push(fence_open);
    }

    let mut chunks: Vec<ChunkRecord> = Vec::new();
    let mut start_line = 0usize;
    while start_line < line_count {
        let mut end_line = start_line;
        // Advance to the first closeable boundary: at or past the target
        // and not inside a fence. The final line always closes.
        while end_line + 1 < line_count {
            let covered = line_offsets[end_line + 1] - line_offsets[start_line];
            if covered >= config.target_chunk_bytes && !open_after[end_line] {
                break;
            }
            end_line += 1;
        }
        let byte_start = line_offsets[start_line] as u64;
        let byte_end = line_offsets[end_line + 1] as u64;
        let text = &body[byte_start as usize..byte_end as usize];
        let chunk_index = chunks.len() as u32;
        chunks.push(ChunkRecord {
            chunk_id: format!("{}#{chunk_index}", source.source_id),
            source_id: source.source_id.clone(),
            source_hash: source.content_hash.clone(),
            chunk_index,
            byte_start,
            byte_end,
            content_address: sha256_hex(text.as_bytes()),
            bytes: byte_end - byte_start,
            word_count: text.split_whitespace().count() as u32,
        });
        if end_line + 1 == line_count {
            break;
        }
        // Overlap: back up to the earliest line start within
        // `overlap_bytes` of the boundary. A single line longer than the
        // overlap yields no overlap (next start = the boundary). The start
        // must be fence-clean too: backing into a fenced block would begin
        // a chunk mid-fence, so the start advances past the block (at most
        // to the boundary itself — overlap shrinks, correctness doesn't).
        // Progress is guaranteed: every non-final chunk covers at least
        // `target_chunk_bytes` > `overlap_bytes`, so the next start is
        // strictly ahead of `start_line`.
        let boundary = line_offsets[end_line + 1];
        let mut next_start = end_line + 1;
        while next_start > start_line && boundary - line_offsets[next_start] < config.overlap_bytes
        {
            next_start -= 1;
        }
        if boundary - line_offsets[next_start] > config.overlap_bytes {
            next_start += 1;
        }
        while next_start < end_line + 1 && in_fence[next_start] {
            next_start += 1;
        }
        start_line = next_start.max(start_line + 1).min(end_line + 1);
    }
    Ok(chunks)
}

/// The chunk's bytes as a slice of the normalized body — the exact text
/// [`ChunkRecord::content_address`] hashes and [`ChunkRecord::byte_start`]
/// ..[`ChunkRecord::byte_end`] names.
pub fn chunk_slice<'a>(normalized_body: &'a str, chunk: &ChunkRecord) -> &'a str {
    &normalized_body[chunk.byte_start as usize..chunk.byte_end as usize]
}
