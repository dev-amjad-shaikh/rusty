//! Knowledge plane integration tests.
//!
//! One coherent suite over the whole module:
//!
//! - **Registration validation** — the fail-closed gate: malformed ids,
//!   empty titles/authors, out-of-range confidence, empty/oversize bodies,
//!   and already-expired TTLs are rejected before a byte is stored.
//! - **Deterministic ingestion** — same input, same chunk ids and content
//!   addresses; Markdown code fences are never split; chunk byte ranges
//!   name exact slices of the normalized body; line-ending normalization
//!   makes the same logical document address identically.
//! - **Content-addressed store** — idempotent puts, address verification,
//!   write-once versions and chunk lists.
//! - **Retrieval** — BM25-lite sanity and determinism, the content-address
//!   tie-break, the optional vector component, and the count/byte ceilings.
//! - **Citations** — every result is a cited chunk; never bare text.
//! - **Corrections** — a superseding version hides the old chunks from
//!   retrieval while the old version stays addressable as evidence.
//! - **Scope isolation** — cross-scope queries return empty, never an
//!   error leak.
//! - **Retention** — dry-run reports, apply-mode purges, and tombstones
//!   that keep old citations resolvable to metadata.

use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};

use rusty_agent_runtime::knowledge::{
    chunk_source, pack_results, rank_lexical, tokenize, ChunkRecord, CitedChunk,
    ContentAddressedStore, InMemoryContentAddressedStore, IngestionConfig, KnowledgeBase,
    KnowledgeSource, LexicalConfig, PurgeReason, QueryLimits, RetentionPolicy, RetrievalWeights,
    ScoredChunk, SourceKind, SourceRegistration, VectorScorer, MAX_SOURCE_BYTES,
};
use rusty_agent_runtime::memory::{MemoryScope, ScopeAddress};
use rusty_agent_runtime::record::sha256_hex;

/// The suite's epoch; every clock is this plus a second offset, so no test
/// reads a wall clock.
fn at(seconds: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(1_700_000_000, 0).unwrap() + Duration::seconds(seconds)
}

fn scope_agent(id: &str) -> ScopeAddress {
    ScopeAddress::new(MemoryScope::Agent, id)
}

fn registration(id: &str, scope: ScopeAddress) -> SourceRegistration {
    SourceRegistration {
        source_id: id.to_owned(),
        scope,
        kind: SourceKind::Text,
        title: format!("Title of {id}"),
        author: "human:curator".to_owned(),
        confidence: 0.9,
        retention: RetentionPolicy::Pinned,
    }
}

fn knowledge_base() -> KnowledgeBase {
    KnowledgeBase::new(Arc::new(InMemoryContentAddressedStore::new()))
}

/// A knowledge base with small chunks, so tests exercise multi-chunk
/// behavior without large bodies.
fn small_chunk_base() -> KnowledgeBase {
    knowledge_base().with_ingestion_config(IngestionConfig {
        target_chunk_bytes: 256,
        overlap_bytes: 32,
    })
}

/// A body of `lines` numbered lines, each ~22 bytes — short enough that
/// the 32-byte test overlap always finds a line boundary to snap to.
fn filler_body(lines: usize) -> String {
    (0..lines)
        .map(|i| format!("line {i:04} filler text\n"))
        .collect()
}

// --------------------------------------------------------------------- //
// Registration validation
// --------------------------------------------------------------------- //

#[tokio::test]
async fn registration_fails_closed_on_invalid_inputs() {
    let kb = knowledge_base();
    let now = at(0);
    let body = "a body worth governing";

    // Malformed source ids.
    for bad in ["", "has spaces", "has/slash", &"x".repeat(129)] {
        let reg = registration(bad, scope_agent("a"));
        assert!(
            kb.register_source(reg, body, now).await.is_err(),
            "source id `{bad}` must be rejected"
        );
    }

    // Empty title / author.
    let mut reg = registration("s1", scope_agent("a"));
    reg.title = "   ".to_owned();
    assert!(kb.register_source(reg, body, now).await.is_err());
    let mut reg = registration("s1", scope_agent("a"));
    reg.author = String::new();
    assert!(kb.register_source(reg, body, now).await.is_err());

    // Confidence outside (0, 1].
    for bad in [0.0, -0.5, 1.5, f64::NAN] {
        let mut reg = registration("s1", scope_agent("a"));
        reg.confidence = bad;
        assert!(
            kb.register_source(reg, body, now).await.is_err(),
            "confidence {bad} must be rejected"
        );
    }

    // Empty and oversize bodies.
    let reg = registration("s1", scope_agent("a"));
    assert!(kb.register_source(reg, "", now).await.is_err());
    let reg = registration("s1", scope_agent("a"));
    let oversize = "x".repeat(MAX_SOURCE_BYTES + 1);
    assert!(kb.register_source(reg, &oversize, now).await.is_err());

    // A TTL already expired at registration time.
    let mut reg = registration("s1", scope_agent("a"));
    reg.retention = RetentionPolicy::Ttl { expires_at: at(-1) };
    assert!(kb.register_source(reg, body, now).await.is_err());

    // And nothing was stored through any of it.
    assert!(kb.versions_of("s1").await.unwrap().is_empty());
}

#[tokio::test]
async fn registration_derives_content_address_and_is_idempotent() {
    let kb = knowledge_base();
    let now = at(0);
    let body = "the deterministic chunker rewards steady inputs";
    let source = kb
        .register_source(registration("doc", scope_agent("a")), body, now)
        .await
        .unwrap();
    assert_eq!(source.version, 1);
    assert_eq!(source.supersedes, None);
    assert_eq!(
        source.content_hash,
        rusty_agent_runtime::knowledge::derive_content_hash("doc", body).unwrap(),
        "the content hash identities the source id plus the normalized body"
    );

    // Re-registering the same body converges on the stored version, even
    // with different registration metadata (the hash is the identity).
    let mut again = registration("doc", scope_agent("a"));
    again.title = "a different title, same body".to_owned();
    let stored = kb.register_source(again, body, now).await.unwrap();
    assert_eq!(
        stored, source,
        "idempotent re-registration returns the stored record"
    );

    // A different body under the same id is a correction, not a registration.
    let err = kb
        .register_source(registration("doc", scope_agent("a")), "a changed body", now)
        .await;
    assert!(err.is_err(), "silent overwrite must fail closed");
}

// --------------------------------------------------------------------- //
// Deterministic ingestion
// --------------------------------------------------------------------- //

#[tokio::test]
async fn chunking_is_deterministic_with_stable_ids_and_addresses() {
    let kb = small_chunk_base();
    let body = filler_body(60);
    let source = kb
        .register_source(registration("filler", scope_agent("a")), &body, at(0))
        .await
        .unwrap();
    let config = IngestionConfig {
        target_chunk_bytes: 256,
        overlap_bytes: 32,
    };
    let first = chunk_source(&source, &body, &config).unwrap();
    let second = chunk_source(&source, &body, &config).unwrap();
    assert_eq!(first, second, "same input chunks identically");
    assert!(
        first.len() > 2,
        "the filler body must exercise multi-chunk ingestion"
    );

    for (index, chunk) in first.iter().enumerate() {
        assert_eq!(chunk.chunk_id, format!("filler#{index}"));
        assert_eq!(chunk.chunk_index as usize, index);
        let slice = &body[chunk.byte_start as usize..chunk.byte_end as usize];
        assert_eq!(chunk.bytes, slice.len() as u64);
        assert_eq!(
            chunk.content_address,
            sha256_hex(slice.as_bytes()),
            "the content address is over the exact body slice"
        );
        assert_eq!(chunk.word_count as usize, slice.split_whitespace().count());
    }
    // Overlap: consecutive non-final chunks share bytes (the filler lines
    // are far shorter than the 32-byte overlap, so the snap always finds a
    // line boundary inside it).
    for pair in first.windows(2) {
        assert!(
            pair[1].byte_start < pair[0].byte_end,
            "consecutive chunks overlap: {:?} then {:?}",
            pair[0].chunk_id,
            pair[1].chunk_id
        );
    }
    // The chunks tile the body: every byte is covered by at least one chunk.
    assert_eq!(first.first().unwrap().byte_start, 0);
    assert_eq!(first.last().unwrap().byte_end, body.len() as u64);
}

#[tokio::test]
async fn chunking_never_splits_a_markdown_code_fence() {
    let kb = small_chunk_base();
    // A fence block larger than the 256-byte target, starting before the
    // first naive boundary: a fence-blind chunker would split inside it.
    let fence_body = "fn main() {\n".to_owned() + &"    let governed = true;\n".repeat(20) + "}\n";
    let body =
        format!("# Guide\n\nintro paragraph text here\n\n```rust\n{fence_body}```\n\noutro\n");
    let mut reg = registration("guide", scope_agent("a"));
    reg.kind = SourceKind::Markdown;
    let source = kb.register_source(reg, &body, at(0)).await.unwrap();
    let config = IngestionConfig {
        target_chunk_bytes: 256,
        overlap_bytes: 32,
    };
    let chunks = chunk_source(&source, &body, &config).unwrap();
    assert!(chunks.len() >= 2);
    for chunk in &chunks {
        let slice = &body[chunk.byte_start as usize..chunk.byte_end as usize];
        let delimiters = slice
            .lines()
            .filter(|line| line.trim_start().starts_with("```"))
            .count();
        assert_eq!(
            delimiters % 2,
            0,
            "chunk {} leaves a fence dangling:\n{slice}",
            chunk.chunk_id
        );
    }
    // The oversized fence block landed whole in exactly one chunk.
    let fence_chunk = chunks
        .iter()
        .find(|chunk| {
            let slice = &body[chunk.byte_start as usize..chunk.byte_end as usize];
            slice.contains("```rust")
        })
        .expect("the fence lives in some chunk");
    let slice = &body[fence_chunk.byte_start as usize..fence_chunk.byte_end as usize];
    assert!(
        slice.contains("```\n"),
        "the same chunk carries the closing fence"
    );
}

#[tokio::test]
async fn line_endings_normalize_to_identical_addresses() {
    let kb = knowledge_base();
    // One source id, registered first with CRLF bytes and then with LF
    // bytes: both normalize to the same document, so the second
    // registration converges on the first version — same hash, same chunks.
    let crlf = kb
        .register_source(
            registration("doc", scope_agent("a")),
            "alpha\r\nbeta\r\ngamma\r\n",
            at(0),
        )
        .await
        .unwrap();
    let lf = kb
        .register_source(
            registration("doc", scope_agent("a")),
            "alpha\nbeta\ngamma\n",
            at(1),
        )
        .await
        .unwrap();
    assert_eq!(
        crlf.content_hash, lf.content_hash,
        "CRLF and LF forms of one document address identically"
    );
    assert_eq!(kb.versions_of("doc").await.unwrap().len(), 1);
}

#[test]
fn chunker_rejects_a_body_the_source_did_not_register() {
    let source = registration("doc", scope_agent("a"))
        .build("the registered body", at(0))
        .unwrap();
    let err = chunk_source(&source, "some other body", &IngestionConfig::default());
    assert!(err.is_err(), "ingestion cannot chunk unregistered bytes");
    let bad_config = IngestionConfig {
        target_chunk_bytes: 256,
        overlap_bytes: 256,
    };
    assert!(
        bad_config.validate().is_err(),
        "overlap must stay below target"
    );
}

// --------------------------------------------------------------------- //
// The content-addressed store
// --------------------------------------------------------------------- //

#[tokio::test]
async fn content_store_is_idempotent_and_verifies_addresses() {
    let store = InMemoryContentAddressedStore::new();
    let bytes = b"content under test";
    let address = sha256_hex(bytes);

    assert!(store.put_content(&address, bytes).await.unwrap());
    assert!(
        !store.put_content(&address, bytes).await.unwrap(),
        "second put converges"
    );
    assert_eq!(
        store.get_content(&address).await.unwrap().as_deref(),
        Some(bytes.as_slice())
    );
    assert!(store
        .get_content(&sha256_hex(b"absent"))
        .await
        .unwrap()
        .is_none());

    // Bytes under a wrong address fail closed.
    let wrong = sha256_hex(b"other bytes");
    assert!(store.put_content(&wrong, bytes).await.is_err());
}

#[tokio::test]
async fn source_versions_and_chunk_lists_are_write_once() {
    let store = InMemoryContentAddressedStore::new();
    let source = registration("doc", scope_agent("a"))
        .build("versioned body text", at(0))
        .unwrap();
    assert!(store.put_source(&source).await.unwrap());
    assert!(
        !store.put_source(&source).await.unwrap(),
        "identical re-put converges"
    );

    let mut impostor = source.clone();
    impostor.title = "same hash, different metadata".to_owned();
    assert!(
        store.put_source(&impostor).await.is_err(),
        "a different record under an occupied hash fails"
    );

    let chunks = chunk_source(&source, "versioned body text", &IngestionConfig::default()).unwrap();
    store.put_chunks(&chunks).await.unwrap();
    store.put_chunks(&chunks).await.unwrap();
    let mut drifted = chunks.clone();
    drifted[0].word_count += 1;
    assert!(
        store.put_chunks(&drifted).await.is_err(),
        "chunk lists are write-once"
    );

    assert_eq!(store.chunks_of(&source.content_hash).await.unwrap(), chunks);
    let reverse = store
        .source_of_chunk(&chunks[0].content_address)
        .await
        .unwrap();
    assert_eq!(reverse.as_deref(), Some(source.content_hash.as_str()));
}

// --------------------------------------------------------------------- //
// Retrieval: ranking, determinism, citations, ceilings
// --------------------------------------------------------------------- //

#[test]
fn lexical_ranking_is_sane_deterministic_and_tie_broken() {
    let source = registration("doc", scope_agent("a"))
        .build("placeholder", at(0))
        .unwrap();
    // Three chunks: one rich in the query term, one mentioning it, one
    // without it.
    let texts = [
        "rust rust rust: the borrow checker keeps rust code honest",
        "a note about rust and little else at all",
        "entirely unrelated prose about gardening and rain",
    ];
    let chunks: Vec<ChunkRecord> = texts
        .iter()
        .enumerate()
        .map(|(index, text)| ChunkRecord {
            chunk_id: format!("doc#{index}"),
            source_id: "doc".to_owned(),
            source_hash: source.content_hash.clone(),
            chunk_index: index as u32,
            byte_start: 0,
            byte_end: text.len() as u64,
            content_address: sha256_hex(text.as_bytes()),
            bytes: text.len() as u64,
            word_count: text.split_whitespace().count() as u32,
        })
        .collect();
    let corpus: Vec<ScoredChunk<'_>> = chunks
        .iter()
        .zip(texts.iter())
        .map(|(chunk, text)| ScoredChunk { chunk, text })
        .collect();
    let lexical = LexicalConfig::default();

    let ranked = rank_lexical(&corpus, "rust", &lexical).unwrap();
    assert_eq!(
        ranked.len(),
        2,
        "the termless chunk scores zero and drops out"
    );
    assert_eq!(
        ranked[0].0, 0,
        "term frequency with length normalization ranks the rich chunk first"
    );
    assert!(ranked[0].1 > ranked[1].1);

    // Determinism: repeated ranking of the same corpus is byte-identical.
    let again = rank_lexical(&corpus, "rust", &lexical).unwrap();
    assert_eq!(ranked, again);

    // The tie-break is the content address: equal scores order by address.
    let tie_texts = ["apple banana", "apple cherry"];
    let tie_chunks: Vec<ChunkRecord> = tie_texts
        .iter()
        .enumerate()
        .map(|(index, text)| ChunkRecord {
            content_address: sha256_hex(text.as_bytes()),
            chunk_index: index as u32,
            ..chunks[index].clone()
        })
        .collect();
    let tie_corpus: Vec<ScoredChunk<'_>> = tie_chunks
        .iter()
        .zip(tie_texts.iter())
        .map(|(chunk, text)| ScoredChunk { chunk, text })
        .collect();
    let tied = rank_lexical(&tie_corpus, "apple", &lexical).unwrap();
    assert_eq!(tied.len(), 2);
    assert_eq!(
        tied[0].1.total_cmp(&tied[1].1),
        std::cmp::Ordering::Equal,
        "equal-length single-mention chunks tie"
    );
    let addresses: Vec<&str> = tied
        .iter()
        .map(|(index, _)| tie_chunks[*index].content_address.as_str())
        .collect();
    assert!(
        addresses[0] < addresses[1],
        "ties break by content address ascending"
    );
}

#[tokio::test]
async fn retrieval_returns_cited_chunks_with_complete_attribution() {
    let kb = small_chunk_base();
    let body = filler_body(60);
    let source = kb
        .register_source(registration("manual", scope_agent("a")), &body, at(0))
        .await
        .unwrap();
    let results = kb
        .query(
            &scope_agent("a"),
            "filler text",
            &QueryLimits::default(),
            at(1),
        )
        .await
        .unwrap();
    assert!(!results.is_empty());
    for result in &results {
        let citation = &result.citation;
        assert_eq!(citation.source_id, "manual");
        assert_eq!(citation.source_hash, source.content_hash);
        assert_eq!(citation.title, "Title of manual");
        assert!(citation.chunk_id.starts_with("manual#"));
        // The byte range names exactly the returned text inside the
        // normalized body.
        let slice = &body[citation.byte_start as usize..citation.byte_end as usize];
        assert_eq!(result.text, slice);
        assert_eq!(
            citation.content_address,
            sha256_hex(slice.as_bytes()),
            "the citation's address resolves the cited bytes"
        );
        assert!(result.score > 0.0);
    }
    // The whole result set is deterministically ordered: re-querying ranks
    // byte-identically.
    let again = kb
        .query(
            &scope_agent("a"),
            "filler text",
            &QueryLimits::default(),
            at(1),
        )
        .await
        .unwrap();
    assert_eq!(results, again);
}

#[tokio::test]
async fn query_ceilings_truncate_count_and_bytes() {
    let kb = small_chunk_base();
    let body = filler_body(60); // many ~256-byte chunks, all mentioning the terms
    kb.register_source(registration("bulk", scope_agent("a")), &body, at(0))
        .await
        .unwrap();
    let unlimited = kb
        .query(
            &scope_agent("a"),
            "filler text",
            &QueryLimits::default(),
            at(1),
        )
        .await
        .unwrap();
    assert!(
        unlimited.len() > 3,
        "the corpus must exceed the test ceilings"
    );

    let by_count = kb
        .query(
            &scope_agent("a"),
            "filler text",
            &QueryLimits {
                max_results: 2,
                max_bytes: DEFAULT_MAX_RESULT_BYTES_FOR_TEST,
            },
            at(1),
        )
        .await
        .unwrap();
    assert_eq!(by_count.len(), 2, "the count ceiling truncates");
    assert_eq!(by_count, unlimited[..2], "truncation keeps rank order");

    let by_bytes = kb
        .query(
            &scope_agent("a"),
            "filler text",
            &QueryLimits {
                max_results: 100,
                max_bytes: 300,
            },
            at(1),
        )
        .await
        .unwrap();
    let total: usize = by_bytes.iter().map(|r| r.text.len()).sum();
    assert!(total <= 300, "the byte ceiling holds");
    assert_eq!(
        by_bytes.len(),
        1,
        "one ~256-byte chunk fits 300 bytes, two do not"
    );

    // Invalid ceilings fail closed.
    for limits in [
        QueryLimits {
            max_results: 0,
            max_bytes: 1024,
        },
        QueryLimits {
            max_results: 1,
            max_bytes: 0,
        },
    ] {
        assert!(kb
            .query(&scope_agent("a"), "governed", &limits, at(1))
            .await
            .is_err());
    }
    // A punctuation-only query cannot rank and fails closed.
    assert!(kb
        .query(&scope_agent("a"), "… …", &QueryLimits::default(), at(1))
        .await
        .is_err());
}

const DEFAULT_MAX_RESULT_BYTES_FOR_TEST: usize = 64 * 1024;

/// A stub scorer for the hybrid seam: scores chunks containing "zebra".
#[derive(Debug)]
struct ZebraScorer;

impl VectorScorer for ZebraScorer {
    fn score(&self, _query: &str, chunk_text: &str, _chunk: &ChunkRecord) -> Option<f64> {
        Some(if chunk_text.contains("zebra") {
            10.0
        } else {
            0.0
        })
    }
}

#[tokio::test]
async fn hybrid_vector_component_reorders_and_fails_closed_when_uninstalled() {
    let kb = small_chunk_base();
    // Two ~300-byte lines, so the chunker splits between them: the first
    // carries the query's terms, the second carries none — only the vector
    // component can rank it.
    let line_a = format!("query terms{}\n", " alpha".repeat(50));
    let line_b = format!("zebra{}\n", " omega".repeat(50));
    let body = format!("{line_a}{line_b}");
    kb.register_source(registration("zoo", scope_agent("a")), &body, at(0))
        .await
        .unwrap();

    // A vector weight with no scorer installed fails closed.
    let hybrid_weights = RetrievalWeights {
        lexical: 1.0,
        vector: 0.5,
    };
    let uninstalled = kb.clone().with_weights(hybrid_weights);
    assert!(
        uninstalled
            .query(
                &scope_agent("a"),
                "query terms",
                &QueryLimits::default(),
                at(1)
            )
            .await
            .is_err(),
        "hybrid weights without a scorer fail closed"
    );

    // With the scorer installed, the vector component outranks the lexical
    // one: the zebra chunk never matched lexically but leads the hybrid.
    let hybrid = kb
        .with_weights(hybrid_weights)
        .with_vector_scorer(Arc::new(ZebraScorer));
    let results = hybrid
        .query(
            &scope_agent("a"),
            "query terms",
            &QueryLimits::default(),
            at(1),
        )
        .await
        .unwrap();
    assert_eq!(results.len(), 2);
    assert!(
        results[0].text.contains("zebra"),
        "the vector component reorders: {:?}",
        results.iter().map(|r| &r.text).collect::<Vec<_>>()
    );
    assert!(results[0].score > results[1].score);
}

#[test]
fn pack_results_truncates_like_the_memory_assembly() {
    let cited = |text: &str| CitedChunk {
        citation: rusty_agent_runtime::knowledge::Citation {
            source_id: "s".to_owned(),
            source_hash: "h".to_owned(),
            title: "t".to_owned(),
            chunk_id: "s#0".to_owned(),
            chunk_index: 0,
            content_address: sha256_hex(text.as_bytes()),
            byte_start: 0,
            byte_end: text.len() as u64,
        },
        text: text.to_owned(),
        score: 1.0,
        word_count: 1,
    };
    let ranked = vec![cited("aaaa"), cited("bbbb"), cited("cccc")];
    let packed = pack_results(
        ranked,
        &QueryLimits {
            max_results: 10,
            max_bytes: 9,
        },
    );
    assert_eq!(
        packed.len(),
        2,
        "packing stops at the first result that would spill"
    );
    assert!(tokenize("Hello, WORLD! hello") == vec!["hello", "world", "hello"]);
}

// --------------------------------------------------------------------- //
// Corrections and supersession
// --------------------------------------------------------------------- //

#[tokio::test]
async fn corrections_supersede_old_chunks_but_preserve_evidence() {
    let kb = knowledge_base();
    let scope = scope_agent("a");
    let v1 = kb
        .register_source(
            registration("policy", scope.clone()),
            "the apple policy stands",
            at(0),
        )
        .await
        .unwrap();
    let v1_chunks = {
        let results = kb
            .query(&scope, "apple policy", &QueryLimits::default(), at(1))
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].citation.source_hash, v1.content_hash);
        results
    };

    let v2 = kb
        .correct_source("policy", "human:editor", "the banana policy stands", at(2))
        .await
        .unwrap();
    assert_eq!(v2.version, 2);
    assert_eq!(v2.supersedes.as_deref(), Some(v1.content_hash.as_str()));
    assert_eq!(
        v2.author, "human:editor",
        "the correction carries the corrector"
    );
    assert_ne!(
        v2.content_hash, v1.content_hash,
        "a correction is a new version"
    );

    // Retrieval never returns superseded chunks: the term unique to the
    // old version finds nothing, and a term shared by both versions is
    // served exclusively by the new one.
    assert!(
        kb.query(&scope, "apple", &QueryLimits::default(), at(3))
            .await
            .unwrap()
            .is_empty(),
        "the superseded version stops serving"
    );
    let shared = kb
        .query(&scope, "policy", &QueryLimits::default(), at(3))
        .await
        .unwrap();
    assert!(
        shared
            .iter()
            .all(|hit| hit.citation.source_hash == v2.content_hash),
        "shared terms are served by the live version only"
    );
    let results = kb
        .query(&scope, "banana policy", &QueryLimits::default(), at(3))
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].citation.source_hash, v2.content_hash);

    // The old version remains addressable by hash — evidence, not retrieval.
    let stored_v1 = kb.get_source(&v1.content_hash).await.unwrap().unwrap();
    assert_eq!(stored_v1.version, 1);
    let chain = kb.versions_of("policy").await.unwrap();
    assert_eq!(chain.len(), 2);
    let old_text = kb
        .chunk_content(&v1_chunks[0].citation.content_address)
        .await
        .unwrap();
    assert_eq!(old_text.as_deref(), Some("the apple policy stands"));

    // Discipline failures fail closed.
    assert!(
        kb.correct_source("unknown", "human:editor", "body", at(3))
            .await
            .is_err(),
        "correcting an unknown source fails"
    );
    assert!(
        kb.correct_source("policy", "human:editor", "the banana policy stands", at(3))
            .await
            .is_err(),
        "a byte-identical correction is not a correction"
    );
}

// --------------------------------------------------------------------- //
// Scope isolation
// --------------------------------------------------------------------- //

#[tokio::test]
async fn scope_isolation_returns_empty_never_an_error_leak() {
    let kb = knowledge_base();
    kb.register_source(
        registration("secret", scope_agent("agent-1")),
        "the scoped fact about otters",
        at(0),
    )
    .await
    .unwrap();

    for foreign in [
        scope_agent("agent-2"),
        ScopeAddress::new(MemoryScope::User, "agent-1"),
        ScopeAddress::new(MemoryScope::Tenant, "acme"),
    ] {
        let results = kb
            .query(&foreign, "otters", &QueryLimits::default(), at(1))
            .await
            .unwrap();
        assert!(
            results.is_empty(),
            "cross-scope reads return empty for {}",
            foreign.as_address()
        );
    }
    let own = kb
        .query(
            &scope_agent("agent-1"),
            "otters",
            &QueryLimits::default(),
            at(1),
        )
        .await
        .unwrap();
    assert_eq!(own.len(), 1);
}

// --------------------------------------------------------------------- //
// Retention
// --------------------------------------------------------------------- //

#[tokio::test]
async fn retention_dry_run_reports_then_apply_purges_with_tombstones() {
    let kb = small_chunk_base();
    let scope = scope_agent("a");

    let mut expiring = registration("ephemeral", scope.clone());
    expiring.retention = RetentionPolicy::Ttl {
        expires_at: at(100),
    };
    let expired_source = kb
        .register_source(expiring, &filler_body(40), at(0))
        .await
        .unwrap();
    // A distinct body, so the pinned source shares no content addresses
    // with the expiring one (shared-content survival has its own test).
    let pinned_body: String = (0..30)
        .map(|i| format!("pinned record {i:04} stays put\n"))
        .collect();
    let pinned = kb
        .register_source(registration("pinned", scope.clone()), &pinned_body, at(0))
        .await
        .unwrap();

    // Capture a live citation before expiry — its content address is what
    // the purge must make unresolvable (the tombstone keeps the metadata).
    let live_hits = kb
        .query(&scope, "filler text", &QueryLimits::default(), at(50))
        .await
        .unwrap();
    let ephemeral_chunk_address = live_hits
        .iter()
        .find(|hit| hit.citation.source_id == "ephemeral")
        .map(|hit| hit.citation.content_address.clone())
        .expect("the expiring source serves before expiry");

    // Before expiry the plan is empty; expiry is a retrieval filter first.
    assert!(kb.plan_sweep(at(50)).await.unwrap().is_empty());
    assert!(!live_hits.is_empty());

    // Past expiry the source stops serving immediately — the sweep is an
    // operator action on top of the filter, not the filter itself.
    assert!(
        kb.query(&scope, "filler text", &QueryLimits::default(), at(150))
            .await
            .unwrap()
            .is_empty(),
        "expired sources are filtered from retrieval before any sweep"
    );

    // Dry-run: the plan names exactly the expired version, with accounting.
    let plan = kb.plan_sweep(at(150)).await.unwrap();
    assert_eq!(plan.entries.len(), 1);
    let entry = &plan.entries[0];
    assert_eq!(entry.source_id, "ephemeral");
    assert_eq!(entry.source_hash, expired_source.content_hash);
    assert_eq!(entry.expires_at, at(100));
    assert!(entry.chunk_count > 0);
    assert_eq!(plan.total_chunk_bytes, entry.chunk_bytes);
    // Dry-run changes nothing.
    assert!(kb
        .get_source(&expired_source.content_hash)
        .await
        .unwrap()
        .is_some());

    // Apply: the plan executes exactly; the pinned source is untouched.
    let receipt = kb.apply_sweep(at(150)).await.unwrap();
    assert_eq!(
        receipt.plan, plan,
        "apply executes the dry-run plan byte-identically"
    );
    assert_eq!(receipt.tombstones.len(), 1);
    let tombstone = &receipt.tombstones[0];
    assert_eq!(tombstone.source_id, "ephemeral");
    assert_eq!(
        tombstone.purged_hashes,
        vec![expired_source.content_hash.clone()]
    );
    assert_eq!(tombstone.reason, PurgeReason::Expired);
    assert_eq!(tombstone.purged_at, at(150));
    assert_eq!(tombstone.scope, scope);
    assert_eq!(tombstone.title, "Title of ephemeral");

    // Content, chunks, and the source record are gone; the tombstone keeps
    // old citations resolvable to metadata.
    assert!(kb
        .get_source(&expired_source.content_hash)
        .await
        .unwrap()
        .is_none());
    assert!(
        kb.chunk_content(&expired_source.body_hash)
            .await
            .unwrap()
            .is_none(),
        "the purged body is gone"
    );
    assert!(
        kb.chunk_content(&ephemeral_chunk_address)
            .await
            .unwrap()
            .is_none(),
        "the pre-sweep citation's content address no longer resolves to bytes"
    );
    assert_eq!(
        kb.tombstone("ephemeral").await.unwrap().as_ref(),
        Some(tombstone),
        "the tombstone persists for citation resolution"
    );
    assert!(kb.tombstone("pinned").await.unwrap().is_none());
    assert!(kb.get_source(&pinned.content_hash).await.unwrap().is_some());

    // A second sweep is a no-op: purging is idempotent and the first
    // tombstone stays the evidence.
    let second = kb.apply_sweep(at(200)).await.unwrap();
    assert!(second.plan.is_empty());
    assert!(second.tombstones.is_empty());
    assert_eq!(
        kb.tombstone("ephemeral").await.unwrap().as_ref(),
        Some(tombstone),
        "the earliest tombstone remains the receipt"
    );
}

#[tokio::test]
async fn sweep_preserves_content_shared_with_surviving_versions() {
    let kb = small_chunk_base();
    // Two sources whose bodies coincide for the expiring one and the pinned
    // one: the shared chunk content address must survive the sweep.
    let shared = filler_body(20);
    let mut expiring = registration("shared-exp", scope_agent("a"));
    expiring.retention = RetentionPolicy::Ttl {
        expires_at: at(100),
    };
    let exp = kb.register_source(expiring, &shared, at(0)).await.unwrap();
    let keep = kb
        .register_source(
            registration("shared-keep", scope_agent("a")),
            &shared,
            at(0),
        )
        .await
        .unwrap();

    kb.apply_sweep(at(150)).await.unwrap();
    // The pinned source's identical body is a different version record, so
    // its chunks — the same content addresses — must still resolve.
    assert!(kb.get_source(&keep.content_hash).await.unwrap().is_some());
    assert!(kb.get_source(&exp.content_hash).await.unwrap().is_none());
    assert!(
        kb.chunk_content(&keep.body_hash).await.unwrap().is_some(),
        "an address dies only with its last reference"
    );
    let results = kb
        .query(
            &scope_agent("a"),
            "filler text",
            &QueryLimits::default(),
            at(150),
        )
        .await
        .unwrap();
    assert!(
        results
            .iter()
            .all(|hit| hit.citation.source_id == "shared-keep"),
        "only the surviving source serves"
    );
}

// --------------------------------------------------------------------- //
// Construction
// --------------------------------------------------------------------- //

#[test]
fn knowledge_source_record_is_serde_round_trippable() {
    let source: KnowledgeSource = registration("doc", scope_agent("a"))
        .build("round trip body", at(0))
        .unwrap();
    let json = serde_json::to_string(&source).unwrap();
    let back: KnowledgeSource = serde_json::from_str(&json).unwrap();
    assert_eq!(source, back);
    // Supersession is sparse on the wire: absent while unset.
    assert!(!json.contains("supersedes"));
}
