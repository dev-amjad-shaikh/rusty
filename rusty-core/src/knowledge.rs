//! The knowledge plane: governed sources, deterministic ingestion,
//! content-addressed chunks, hybrid retrieval with citations, corrections,
//! and retention — capability-harness slice #4.
//!
//! Knowledge reuses the governed-memory vocabulary ([`crate::memory`]) but
//! answers a different question. Memory records what an agent *learned*;
//! knowledge stores what an operator *published* — documents and facts a
//! run may retrieve and **cite**. The same governance rules carry over:
//!
//! - [`KnowledgeSource`] — one governed source version: scoped
//!   ([`ScopeAddress`], the memory taxonomy unchanged), attributed (an
//!   author/provenance string is mandatory — a source that cannot name its
//!   origin cannot be audited), confidence as a writer-declared claim in
//!   `(0, 1]`, a retention policy ([`RetentionPolicy`]: TTL or pinned), and
//!   a content hash that is the version's identity. Registration validates
//!   and fails closed on oversize bodies, malformed ids, and out-of-range
//!   claims ([`SourceRegistration`]).
//! - **Ingestion** ([`ingest`]) — a deterministic chunker
//!   ([`ingest::chunk_source`]): same bytes in, same chunks out. Chunks are
//!   byte-bounded with overlap, never split a Markdown code fence, carry
//!   word-count estimates, and are content-addressed (`sha256` over the
//!   normalized chunk bytes). Chunk ids are stable: `{source_id}#{index}`.
//! - **Storage** ([`store`]) — the [`store::ContentAddressedStore`]
//!   contract: idempotent put/get by hash, a source-version → chunks index,
//!   and the reverse chunk → source index citations resolve through.
//!   [`store::InMemoryContentAddressedStore`] is the dev/test
//!   implementation; file/Postgres backends are a server concern behind the
//!   same trait.
//! - **Retrieval** ([`retrieve`]) — hybrid scoring: a BM25-lite lexical
//!   rank (term frequency with document-length normalization) plus an
//!   optional [`retrieve::VectorScorer`] component behind a trait (no
//!   embedding dependencies in core), combined under
//!   [`retrieve::RetrievalWeights`]. Result sets are bounded in count and
//!   bytes ([`retrieve::QueryLimits`]); ranking is a total order with the
//!   content address as the final tie-break.
//! - **Citations** — retrieval returns [`retrieve::CitedChunk`]s, never
//!   bare text: every chunk renders a [`retrieve::Citation`] (source id,
//!   title, chunk id, content address, byte range) so agents attribute what
//!   they quote.
//! - **Corrections** — correcting a source mints a new version (new content
//!   hash) that supersedes the old, mirroring memory's supersession rule:
//!   retrieval never returns superseded chunks; the old version remains
//!   addressable by hash as evidence.
//! - **Retention** — [`base::KnowledgeBase::plan_sweep`] reports what a
//!   sweep *would* purge (dry-run); [`base::KnowledgeBase::apply_sweep`]
//!   executes it: chunks and bodies are removed, and the source id is
//!   tombstoned ([`SourceTombstone`]) so citations in old journals stay
//!   resolvable to metadata. Pinned sources are never swept.
//!
//! Every clock read is caller-injected (`now` parameters): the plane is
//! deterministic end to end, and a journaled query's inputs fully
//! determine its result.
//!
//! The query entry point is [`base::KnowledgeBase::query`]: scope isolation
//! (cross-scope is an empty result, never an error leak), supersession,
//! expiry, and ranking in one call.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{Result, RustyError};
use crate::memory::ScopeAddress;
use crate::record::sha256_hex;

pub mod base;
pub mod ingest;
pub mod retrieve;
pub mod store;

pub use base::{KnowledgeBase, PurgeEntry, RetentionPlan, RetentionReceipt};
pub use ingest::{
    chunk_slice, chunk_source, ChunkRecord, IngestionConfig, DEFAULT_OVERLAP_BYTES,
    DEFAULT_TARGET_CHUNK_BYTES,
};
pub use retrieve::{
    pack_results, rank_lexical, tokenize, Citation, CitedChunk, LexicalConfig, QueryLimits,
    RetrievalWeights, ScoredChunk, VectorScorer, DEFAULT_BM25_B, DEFAULT_BM25_K1,
    DEFAULT_MAX_RESULTS, DEFAULT_MAX_RESULT_BYTES, MAX_RESULTS_CEILING, MAX_RESULT_BYTES_CEILING,
};
pub use store::{ContentAddressedStore, InMemoryContentAddressedStore};

/// The knowledge schema version. Bump only on a breaking change to the
/// source/chunk model; additive evolution uses serde defaults so previously
/// written records keep deserializing.
pub const KNOWLEDGE_SCHEMA_VERSION: &str = "knowledge-v1";

/// The maximum source body size registration accepts, in bytes. Oversize
/// bodies fail closed: a source too large to chunk deterministically within
/// bounded memory is rejected at the gate, not truncated.
pub const MAX_SOURCE_BYTES: usize = 1024 * 1024;

/// The maximum source id length, in bytes (the same discipline the built-in
/// knowledge search tool applies to document ids).
pub const MAX_SOURCE_ID_BYTES: usize = 128;

/// The maximum title length, in bytes.
pub const MAX_TITLE_BYTES: usize = 512;

/// The maximum author/provenance string length, in bytes.
pub const MAX_ATTRIBUTION_BYTES: usize = 512;

fn invalid(message: impl Into<String>) -> RustyError {
    // A knowledge write is a state update to the governed store; contract
    // validation failures reuse the invalid-update class rather than
    // growing the error taxonomy for one module — the same reading
    // `crate::memory` takes.
    RustyError::InvalidUpdate(message.into())
}

/// Normalize source text for addressing: CRLF and lone CR line endings
/// become LF. The chunker, the content addresses, and the citation byte
/// ranges all operate on the normalized form, so the same logical document
/// addresses identically regardless of the line-ending convention it
/// arrived with.
pub fn normalize_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
        } else {
            out.push(c);
        }
    }
    out
}

/// What a source is. Closed enum — the chunker matches on it (Markdown gets
/// fence-aware splitting), and unsupported kinds fail at deserialization,
/// never mid-ingestion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    /// Plain text.
    Text,
    /// Markdown; the chunker never splits inside a fenced code block.
    Markdown,
    /// A JSON document, chunked as text (structure-aware chunking is a
    /// later slice; the kind is declared so it can land additively).
    Json,
    /// A CSV document, chunked as text under the same rule as JSON.
    Csv,
}

/// How long a source lives. Closed enum: TTL or pinned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "policy")]
pub enum RetentionPolicy {
    /// The source expires at `expires_at`: retrieval filters it from that
    /// instant on, and the retention sweep may purge it. Expiration is a
    /// filter first and a sweep trigger second — never a silent reaper.
    Ttl {
        /// The expiry instant (exclusive: the source is live strictly
        /// before it).
        expires_at: DateTime<Utc>,
    },
    /// The source is pinned: exempt from expiry and from the sweep.
    Pinned,
}

impl RetentionPolicy {
    /// Whether a source under this policy is live at `now`: pinned sources
    /// are always live; TTL sources are live strictly before their expiry.
    pub fn is_live_at(&self, now: DateTime<Utc>) -> bool {
        match self {
            RetentionPolicy::Ttl { expires_at } => *expires_at > now,
            RetentionPolicy::Pinned => true,
        }
    }
}

/// One governed source version: scoped, attributed, content-addressed,
/// superseding.
///
/// The identity split is deliberate: `source_id` is the stable name a
/// correction chain shares, while `content_hash` identifies one immutable
/// version of it. A changed source is a new version — there is no in-place
/// update anywhere in the model, the same rule the memory plane's records
/// follow.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeSource {
    /// The stable source name, shared by every version in a correction
    /// chain (tenant-relative, like memory scope ids).
    pub source_id: String,

    /// Whose knowledge it is (the memory scope taxonomy, unchanged).
    pub scope: ScopeAddress,

    /// What the source is.
    pub kind: SourceKind,

    /// The human-facing title citations render.
    pub title: String,

    /// Who published this version: a provenance string in the memory
    /// plane's vocabulary (`human:{id}` / `agent:{id}` / `system`).
    /// Mandatory — a source that cannot name its origin cannot be audited.
    pub author: String,

    /// The writer-declared confidence in `(0, 1]`. Confidence is a claim,
    /// not a measurement; nothing in the runtime computes it.
    pub confidence: f64,

    /// When this version was registered. Caller-injected (determinism).
    pub created_at: DateTime<Utc>,

    /// TTL or pinned.
    pub retention: RetentionPolicy,

    /// The version's identity: `sha256` over `{source_id, body hash}` —
    /// see [`derive_content_hash`]. The source id is part of the identity
    /// so two sources with byte-identical bodies remain distinct versions;
    /// a correction changes the body, so it mints a new hash. Chunk
    /// addressing and supersession both key on it.
    pub content_hash: String,

    /// The body's own content address: `sha256` over the normalized body
    /// bytes. The body is stored under this pure address, so sources with
    /// byte-identical bodies share the stored bytes even though their
    /// versions stay distinct.
    pub body_hash: String,

    /// The normalized body size in bytes.
    pub content_bytes: u64,

    /// The position in the correction chain: `1` for the first version.
    pub version: u64,

    /// The version this one replaces, when it does. Supersession is a
    /// chain of immutable versions; the superseded version is retained as
    /// evidence but filtered from retrieval.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
}

impl KnowledgeSource {
    /// Whether this version is eligible for retrieval at `now`: live under
    /// its retention policy. Supersession is a property of the version set,
    /// not of one record, so it is applied by the query path
    /// ([`store::ContentAddressedStore::versions_of`] plus the superseded
    /// set), not here.
    pub fn is_live_at(&self, now: DateTime<Utc>) -> bool {
        self.retention.is_live_at(now)
    }
}

/// The validated inputs of a source registration. Construction is the gate:
/// every rule the contract enforces runs here, and failures are
/// [`RustyError::InvalidUpdate`] — fail-closed, before any byte is stored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceRegistration {
    /// The stable source name.
    pub source_id: String,

    /// Whose knowledge it is.
    pub scope: ScopeAddress,

    /// What the source is.
    pub kind: SourceKind,

    /// The human-facing title.
    pub title: String,

    /// The publisher's provenance string. Mandatory.
    pub author: String,

    /// The writer-declared confidence in `(0, 1]`.
    pub confidence: f64,

    /// TTL or pinned.
    pub retention: RetentionPolicy,
}

impl SourceRegistration {
    /// Validate the registration against `body` at `now` and build the
    /// first version of the source. Fails closed on: a malformed or
    /// overlong `source_id`, an empty or overlong title, an empty or
    /// overlong author string, a confidence outside `(0, 1]` (a value
    /// outside the interval is not a claim at all), an empty or oversize
    /// body, and a TTL already expired at `now` (registering a dead source
    /// is a caller bug, not a storage event).
    pub fn build(self, body: &str, now: DateTime<Utc>) -> Result<KnowledgeSource> {
        validate_source_id(&self.source_id)?;
        if self.title.trim().is_empty() || self.title.len() > MAX_TITLE_BYTES {
            return Err(invalid(format!(
                "knowledge source `{}` needs a non-empty title of at most {MAX_TITLE_BYTES} bytes",
                self.source_id
            )));
        }
        if self.author.trim().is_empty() || self.author.len() > MAX_ATTRIBUTION_BYTES {
            return Err(invalid(format!(
                "knowledge source `{}` needs a non-empty author of at most {MAX_ATTRIBUTION_BYTES} \
                 bytes — a source that cannot name its origin cannot be audited",
                self.source_id
            )));
        }
        if !(self.confidence > 0.0 && self.confidence <= 1.0) {
            return Err(invalid(format!(
                "knowledge source `{}` confidence must be in (0, 1], got {} — confidence is a \
                 writer-declared claim, and a value outside the interval is not a claim at all",
                self.source_id, self.confidence
            )));
        }
        if let RetentionPolicy::Ttl { expires_at } = self.retention {
            if expires_at <= now {
                return Err(invalid(format!(
                    "knowledge source `{}` expires at {expires_at}, already past at {now} — \
                     registering a dead source is a caller bug, not a storage event",
                    self.source_id
                )));
            }
        }
        if body.is_empty() {
            return Err(invalid(format!(
                "knowledge source `{}` has an empty body — there is nothing to govern",
                self.source_id
            )));
        }
        let normalized = normalize_text(body);
        if normalized.len() > MAX_SOURCE_BYTES {
            return Err(invalid(format!(
                "knowledge source `{}` body is {} bytes, above the {MAX_SOURCE_BYTES} byte \
                 ceiling — oversize sources fail closed at registration, they are not truncated",
                self.source_id,
                normalized.len()
            )));
        }
        let content_hash = derive_content_hash(&self.source_id, &normalized)?;
        Ok(KnowledgeSource {
            source_id: self.source_id,
            scope: self.scope,
            kind: self.kind,
            title: self.title,
            author: self.author,
            confidence: self.confidence,
            created_at: now,
            retention: self.retention,
            content_hash,
            body_hash: sha256_hex(normalized.as_bytes()),
            content_bytes: normalized.len() as u64,
            version: 1,
            supersedes: None,
        })
    }
}

/// The content address of a source version: `sha256` over the canonical
/// serialization of `{source_id, body_hash}`, where `body_hash` is the
/// `sha256` of the normalized body. The source id is part of the identity
/// — two sources with byte-identical bodies are distinct versions (they
/// answer differently to an audit), while a correction, which changes the
/// body under one id, mints a new hash. The same hashing discipline the
/// memory plane applies to record identity.
pub fn derive_content_hash(source_id: &str, normalized_body: &str) -> Result<String> {
    let identity = serde_json::json!({
        "source_id": source_id,
        "body": sha256_hex(normalized_body.as_bytes()),
    });
    Ok(sha256_hex(&serde_json::to_vec(&identity)?))
}

/// Source id validation: non-empty, bounded, and restricted to the same
/// character set the built-in knowledge search tool accepts
/// (ASCII alphanumerics plus `.` `_` `:` `-`), so ids stay safe in effect
/// keys, paths, and URLs.
pub fn validate_source_id(source_id: &str) -> Result<()> {
    if source_id.is_empty()
        || source_id.len() > MAX_SOURCE_ID_BYTES
        || !source_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte))
    {
        return Err(invalid(format!(
            "knowledge source id `{source_id}` is invalid — ids are 1..={MAX_SOURCE_ID_BYTES} \
             bytes of ASCII alphanumerics plus `.` `_` `:` `-`"
        )));
    }
    Ok(())
}

/// Why a source was purged. Closed enum, carried on the tombstone: the
/// reason is what distinguishes a reaper's expiry from a later erasure
/// request in the audit trail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PurgeReason {
    /// The source's TTL lapsed and the retention sweep purged it.
    Expired,
}

/// The receipt a purge leaves behind. **Metadata by construction**: the
/// source id, scope, title, the purged versions' content hashes, the
/// reason, and when. There is no content field — a tombstone that *could*
/// carry the purged bytes would be one careless serializer away from
/// defeating the retention it receipts, the same rule the memory plane's
/// forget tombstone follows.
///
/// The tombstone is what keeps citations in old journals resolvable: a
/// citation names the source id and content address, and after a purge
/// both resolve to this metadata instead of failing outright.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceTombstone {
    /// The purged source's stable id.
    pub source_id: String,

    /// The scope it lived at.
    pub scope: ScopeAddress,

    /// The title citations rendered.
    pub title: String,

    /// The content hashes of every purged version, sorted.
    pub purged_hashes: Vec<String>,

    /// Why it was purged.
    pub reason: PurgeReason,

    /// When the purge executed (the sweep's injected clock).
    pub purged_at: DateTime<Utc>,
}
