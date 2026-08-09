//! Governed memory integration tests (R0.8 Rusty Learn, wave 1).
//!
//! Four test groups:
//!
//! - **Golden files** — the serialized shapes of `MemoryRecord`,
//!   `ScopeAddress`, `MemoryProvenance`, `ValidityWindow`, `MemoryQuery`,
//!   `ContextBudget`, and `MemoryAssembly` are pinned against checked-in
//!   JSON under `tests/golden/`. Any accidental contract drift fails here.
//!   To bless an intentional contract change, re-run with `UPDATE_GOLDEN=1`
//!   and review the diff. (The new `RunEventKind` variants' wire names are
//!   pinned in `memory_event_kinds.json`; the exhaustive
//!   `run_event_kind.json` list is owned by another test file.)
//! - **Retrieval semantics** — the structured filters, the deterministic
//!   assembly rank, and the token-bounded packing (bytes÷4 estimate with
//!   the declared margin) over the in-memory store.
//! - **The journaled seam** — reads journal `MemoryRead` with the resolved
//!   query and the assembly; equal store state and equal budget produce
//!   byte-equal assemblies.
//! - **Wave-1 exit: replay-serving** — an exact replay of a memory-reading
//!   run serves the journaled assembly byte-identically, proven end to end
//!   through `ExactReplay::run_and_verify`; and the two negative proofs:
//!   a divergent request fails loudly, a live store cannot impersonate the
//!   journaled assembly.

use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{json, Value};

use rusty_agent_runtime::error::RustyError;
use rusty_agent_runtime::executor::{ExecutionOutcome, Executor, RunConfig};
use rusty_agent_runtime::graph::{Graph, GraphBuilder};
use rusty_agent_runtime::journal::{Clock, Journal, JournalSnapshot, RngSource, PARENT_EVENT_KEY};
use rusty_agent_runtime::memory::{
    apply_query, assemble, estimated_tokens, memory_effect_key, BudgetOverflow, ContextBudget,
    InMemoryMemoryStore, MemoryEvidence, MemoryKind, MemoryProvenance, MemoryQuery, MemoryRecord,
    MemoryReplaySource, MemoryScope, MemorySource, MemoryStore, ProvenanceAuthor, ScopeAddress,
    ValidityWindow, DEFAULT_TOKEN_MARGIN_PERCENT, MEMORY_SCHEMA_VERSION, TOKEN_BYTES_PER_ESTIMATE,
};
use rusty_agent_runtime::node::{NodeContext, NodeOutput};
use rusty_agent_runtime::record::{Effect, PayloadRef, RunEventKind};
use rusty_agent_runtime::replay::{ExactReplay, ReplayParams};
use rusty_agent_runtime::state::{Reducer, State, StateSpec};

// ---------- golden-file machinery ----------

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join(name)
}

/// Assert the pretty-printed serialization of `value` equals the golden
/// file's content exactly. `UPDATE_GOLDEN=1` rewrites the file instead —
/// the diff is then the contract change under review.
fn assert_golden(name: &str, value: &impl Serialize) {
    let rendered = format!("{}\n", serde_json::to_string_pretty(value).unwrap());
    let path = golden_path(name);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, &rendered).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing golden file `{}`: {e}", path.display()));
    assert_eq!(
        rendered,
        expected,
        "contract drift in `{}` — if intentional, re-run with UPDATE_GOLDEN=1 \
         and review the diff",
        path.display()
    );
}

// ---------- shared fixtures ----------

fn ts(millis: i64) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp_millis(millis).unwrap()
}

fn full_provenance() -> MemoryProvenance {
    MemoryProvenance {
        author: ProvenanceAuthor::Agent {
            agent_id: "researcher-7".into(),
        },
        evidence: MemoryEvidence {
            run_id: Some("run-abc".into()),
            event_ids: vec!["run-abc:3".into(), "run-abc:4".into()],
            correction_id: Some("correction-9".into()),
            candidate_id: None,
            source_memory_ids: vec!["a".repeat(64), "b".repeat(64)],
        },
        written_at: ts(1_750_000_001_000),
    }
}

/// A record with every populated field exercised (the golden shape), inline
/// content.
fn full_record() -> MemoryRecord {
    MemoryRecord::new(
        MemoryKind::Fact,
        ScopeAddress::new(MemoryScope::User, "user-7"),
        full_provenance(),
        0.95,
        ValidityWindow {
            valid_from: ts(1_750_000_000_000),
            valid_until: Some(ts(1_850_000_000_000)),
        },
        ts(1_750_000_001_000),
        json!({"timezone": "UTC+4", "source": "profile form"}),
    )
    .unwrap()
    .with_key("timezone")
    .with_tags(["profile", "locale"])
    .with_priority(5)
    .with_expires_at(ts(1_900_000_000_000))
    .with_supersedes("c".repeat(64))
}

// ---------- golden files ----------

#[test]
fn golden_memory_record_shape() {
    assert_golden("memory_record.json", &full_record());
}

#[test]
fn golden_memory_scope_shape() {
    // All five scopes in declaration order, each with a concrete id: the
    // taxonomy and the address shape are the contract.
    assert_golden(
        "memory_scope.json",
        &vec![
            ScopeAddress::new(MemoryScope::Run, "run-abc"),
            ScopeAddress::new(MemoryScope::Agent, "researcher-7"),
            ScopeAddress::new(MemoryScope::Team, "team-blue"),
            ScopeAddress::new(MemoryScope::User, "user-7"),
            ScopeAddress::new(MemoryScope::Tenant, "acme"),
        ],
    );
}

#[test]
fn golden_memory_provenance_shape() {
    assert_golden("memory_provenance.json", &full_provenance());
}

#[test]
fn golden_validity_window_shape() {
    assert_golden(
        "validity_window.json",
        &ValidityWindow {
            valid_from: ts(1_750_000_000_000),
            valid_until: Some(ts(1_850_000_000_000)),
        },
    );
}

#[test]
fn golden_memory_query_shape() {
    let query = MemoryQuery {
        scope: Some(ScopeAddress::new(MemoryScope::Agent, "researcher-7")),
        kinds: vec![MemoryKind::Fact, MemoryKind::Preference],
        key: Some("timezone".into()),
        tags: vec!["profile".into()],
        valid_at: Some(ts(1_800_000_000_000)),
        min_confidence: Some(0.5),
        include_expired: true,
        include_superseded: true,
        authored_by: Some(ProvenanceAuthor::Human {
            human_id: "amjad".into(),
        }),
        as_of: Some(ts(1_800_000_000_000)),
    };
    assert_golden("memory_query.json", &query);
}

#[test]
fn golden_context_budget_shape() {
    let budget = ContextBudget::new(512)
        .with_margin_percent(25)
        .with_overflow(BudgetOverflow::Fail);
    assert_golden("context_budget.json", &budget);
}

#[test]
fn golden_memory_assembly_shape() {
    // Two records, one with artifact-referenced content (both payload forms
    // pinned), packed under a budget that fits both — the journaled
    // `MemoryRead` output shape.
    let big = MemoryRecord::new(
        MemoryKind::Summary,
        ScopeAddress::new(MemoryScope::Team, "team-blue"),
        MemoryProvenance {
            author: ProvenanceAuthor::Distiller {
                name: "consolidator-v1".into(),
            },
            evidence: MemoryEvidence {
                source_memory_ids: vec!["d".repeat(64)],
                ..MemoryEvidence::default()
            },
            written_at: ts(1_750_000_002_000),
        },
        0.8,
        ValidityWindow::starting(ts(1_750_000_000_000)),
        ts(1_750_000_002_000),
        json!({"summary": "s".repeat(4096)}),
    )
    .unwrap();
    let first = full_record();
    // Rank: priority 5 leads, then confidence/recency — the small record
    // packs first, the big artifact-referenced one second.
    let budget = ContextBudget::new(4096);
    let assembly = assemble(vec![big, first], &budget).unwrap();
    assert!(!assembly.truncated);
    assert_eq!(assembly.memory_ids.len(), 2);
    assert_golden("memory_assembly.json", &assembly);
}

#[test]
fn golden_memory_event_kinds_shape() {
    // The two additive R0.8 wave-1 variants' wire names. The exhaustive
    // `run_event_kind.json` list is owned by `tests/agents.rs` (outside this
    // stream's file scope); the names are pinned here so no wire shape
    // lands unpinned.
    assert_golden(
        "memory_event_kinds.json",
        &vec![RunEventKind::MemoryRead, RunEventKind::MemoryWrite],
    );
}

// ---------- contract behavior ----------

#[test]
fn scope_address_and_author_string_forms() {
    assert_eq!(
        ScopeAddress::new(MemoryScope::Agent, "researcher-7").as_address(),
        "agent:researcher-7"
    );
    assert_eq!(
        ScopeAddress::new(MemoryScope::Run, "run-abc").as_address(),
        "run:run-abc"
    );
    assert_eq!(
        ProvenanceAuthor::Agent {
            agent_id: "researcher-7".into()
        }
        .as_id_string(),
        "agent:researcher-7"
    );
    assert_eq!(
        ProvenanceAuthor::Distiller {
            name: "consolidator-v1".into()
        }
        .as_id_string(),
        "distiller:consolidator-v1"
    );
    assert_eq!(ProvenanceAuthor::System.as_id_string(), "system");
    // The derived write key, exactly as the design spells it.
    assert_eq!(
        memory_effect_key(
            &ScopeAddress::new(MemoryScope::Agent, "researcher-7"),
            "m-1"
        ),
        "memory:agent:researcher-7:m-1"
    );
}

#[test]
fn schema_version_matches_the_manifest_pin_vocabulary() {
    // The pin vocabulary RunManifest::with_memory_schema records.
    assert_eq!(MEMORY_SCHEMA_VERSION, "memory-v1");
    assert_eq!(TOKEN_BYTES_PER_ESTIMATE, 4);
    assert_eq!(DEFAULT_TOKEN_MARGIN_PERCENT, 20);
}

#[test]
fn serde_roundtrips_preserve_the_contracts() {
    let record = full_record();
    let back: MemoryRecord =
        serde_json::from_str(&serde_json::to_string(&record).unwrap()).unwrap();
    assert_eq!(record, back);

    let query = MemoryQuery {
        scope: Some(ScopeAddress::new(MemoryScope::User, "user-7")),
        min_confidence: Some(0.5),
        ..MemoryQuery::default()
    };
    let wire = serde_json::to_string(&query).unwrap();
    let back: MemoryQuery = serde_json::from_str(&wire).unwrap();
    assert_eq!(query, back);
    // Sparse stays sparse: unset filters are absent, not null.
    let wire: Value = serde_json::from_str(&wire).unwrap();
    let object = wire.as_object().unwrap();
    assert!(object.contains_key("scope"));
    assert!(object.contains_key("min_confidence"));
    for absent in [
        "kinds",
        "key",
        "tags",
        "valid_at",
        "include_expired",
        "include_superseded",
        "authored_by",
        "as_of",
    ] {
        assert!(!object.contains_key(absent), "`{absent}` must stay absent");
    }
}

#[test]
fn pre_r08_sparse_record_json_still_loads() {
    // The additive-evolution rule, exercised: a record carrying only the
    // mandatory fields (no key/tags/priority/expires/supersedes/embedding)
    // is the minimal wire shape and must keep deserializing.
    let minimal = json!({
        "memory_id": "e".repeat(64),
        "kind": "preference",
        "scope": {"scope": "tenant", "id": "acme"},
        "provenance": {
            "author": {"type": "system"},
            "written_at": "2025-09-21T12:00:00Z",
        },
        "confidence": 1.0,
        "validity": {"valid_from": "2025-09-21T12:00:00Z"},
        "created_at": "2025-09-21T12:00:00Z",
        "content": {"kind": "inline", "value": {"rule": "be brief"}},
    });
    let record: MemoryRecord = serde_json::from_value(minimal).unwrap();
    assert_eq!(record.kind, MemoryKind::Preference);
    assert_eq!(record.scope.scope, MemoryScope::Tenant);
    assert_eq!(record.priority, 0);
    assert!(record.tags.is_empty());
    assert!(record.embedding.is_none());
}

// ---------- retrieval semantics ----------

fn seeded_record(
    kind: MemoryKind,
    key: Option<&str>,
    confidence: f64,
    created_ms: i64,
) -> MemoryRecord {
    let mut record = MemoryRecord::new(
        kind,
        ScopeAddress::new(MemoryScope::User, "user-7"),
        MemoryProvenance {
            author: ProvenanceAuthor::System,
            evidence: MemoryEvidence::default(),
            written_at: ts(created_ms),
        },
        confidence,
        ValidityWindow {
            valid_from: ts(0),
            valid_until: Some(ts(10_000)),
        },
        ts(created_ms),
        json!({"key": key, "confidence": confidence}),
    )
    .unwrap();
    record.key = key.map(str::to_owned);
    record
}

#[test]
fn structured_filters_each_apply() {
    let fact = seeded_record(MemoryKind::Fact, Some("timezone"), 0.9, 1_000);
    let preference = seeded_record(MemoryKind::Preference, Some("tone"), 0.4, 2_000);
    let example = seeded_record(MemoryKind::Example, None, 0.95, 3_000);
    // A record superseded by `fact`, and an expired one.
    let mut superseded = seeded_record(MemoryKind::Fact, Some("timezone"), 0.1, 500);
    superseded.memory_id = "0".repeat(64);
    let fact = fact.with_supersedes(superseded.memory_id.clone());
    let mut expired = seeded_record(MemoryKind::Fact, Some("old"), 0.99, 100);
    expired.expires_at = Some(ts(5_000));
    let universe = vec![
        fact.clone(),
        preference.clone(),
        example.clone(),
        superseded.clone(),
        expired.clone(),
    ];
    let now = ts(6_000);

    // Kind filter.
    let query = MemoryQuery {
        kinds: vec![MemoryKind::Preference],
        ..MemoryQuery::default()
    };
    assert_eq!(
        apply_query(&universe, &query, now),
        vec![preference.clone()]
    );

    // Key equality + minimum confidence.
    let query = MemoryQuery {
        key: Some("timezone".into()),
        min_confidence: Some(0.5),
        ..MemoryQuery::default()
    };
    assert_eq!(apply_query(&universe, &query, now), vec![fact.clone()]);

    // Superseded records are filtered by default, included on request.
    let query = MemoryQuery {
        key: Some("timezone".into()),
        ..MemoryQuery::default()
    };
    assert_eq!(apply_query(&universe, &query, now), vec![fact.clone()]);
    let query = MemoryQuery {
        key: Some("timezone".into()),
        include_superseded: true,
        ..MemoryQuery::default()
    };
    let mut hits = apply_query(&universe, &query, now);
    hits.sort_by(|a, b| a.memory_id.cmp(&b.memory_id));
    let mut expected = vec![fact.clone(), superseded.clone()];
    expected.sort_by(|a, b| a.memory_id.cmp(&b.memory_id));
    assert_eq!(hits, expected);

    // Expired records are filtered by default (now is past the TTL),
    // included on request.
    let query = MemoryQuery {
        key: Some("old".into()),
        ..MemoryQuery::default()
    };
    assert!(apply_query(&universe, &query, now).is_empty());
    let query = MemoryQuery {
        key: Some("old".into()),
        include_expired: true,
        ..MemoryQuery::default()
    };
    assert_eq!(apply_query(&universe, &query, now), vec![expired.clone()]);

    // Validity-at-time: the windows are [0, 10_000).
    let query = MemoryQuery {
        valid_at: Some(ts(20_000)),
        ..MemoryQuery::default()
    };
    assert!(apply_query(&universe, &query, now).is_empty());
    let query = MemoryQuery {
        valid_at: Some(ts(9_999)),
        kinds: vec![MemoryKind::Example],
        ..MemoryQuery::default()
    };
    assert_eq!(apply_query(&universe, &query, now), vec![example.clone()]);

    // Author filter.
    let query = MemoryQuery {
        authored_by: Some(ProvenanceAuthor::Human {
            human_id: "amjad".into(),
        }),
        ..MemoryQuery::default()
    };
    assert!(apply_query(&universe, &query, now).is_empty());

    // Scope filter excludes other scopes' records.
    let query = MemoryQuery {
        scope: Some(ScopeAddress::new(MemoryScope::Agent, "user-7")),
        ..MemoryQuery::default()
    };
    assert!(apply_query(&universe, &query, now).is_empty());
}

#[test]
fn assembly_rank_and_budget_are_deterministic() {
    // Priority beats confidence beats recency; the content address breaks
    // the final tie.
    let low_priority_high_confidence = seeded_record(MemoryKind::Fact, None, 0.99, 3_000);
    let high_priority = seeded_record(MemoryKind::Fact, None, 0.1, 100).with_priority(10);
    let mid = seeded_record(MemoryKind::Fact, None, 0.5, 2_000);
    let records = vec![
        low_priority_high_confidence.clone(),
        mid.clone(),
        high_priority.clone(),
    ];
    let budget = ContextBudget::new(u32::MAX).with_margin_percent(0);
    let assembly = assemble(records.clone(), &budget).unwrap();
    assert_eq!(
        assembly.memory_ids,
        vec![
            high_priority.memory_id.clone(),
            low_priority_high_confidence.memory_id.clone(),
            mid.memory_id.clone(),
        ]
    );

    // Byte-equal assemblies from equal input, independent of input order.
    let mut reversed = records.clone();
    reversed.reverse();
    let again = assemble(reversed, &budget).unwrap();
    assert_eq!(
        serde_json::to_vec(&assembly).unwrap(),
        serde_json::to_vec(&again).unwrap()
    );

    // A budget packs a prefix and reports the accounting honestly.
    let each = estimated_tokens(high_priority.content_bytes(), 0);
    let budget = ContextBudget::new(each).with_margin_percent(0);
    let packed = assemble(records, &budget).unwrap();
    assert_eq!(packed.memory_ids, vec![high_priority.memory_id.clone()]);
    assert!(packed.truncated);
    assert_eq!(packed.token_accounting.used_tokens, each);
    assert_eq!(packed.token_accounting.budget_tokens, each);
    assert_eq!(packed.token_accounting.bytes_per_token, 4);

    // Fail overflow refuses the prefix.
    let budget = budget.with_overflow(BudgetOverflow::Fail);
    assert!(assemble(
        vec![high_priority, low_priority_high_confidence, mid],
        &budget
    )
    .is_err());
}

// ---------- the journaled seam ----------

const RUN_ID: &str = "memory-run-1";
const THREAD_ID: &str = "memory-thread-1";
const CLOCK_START_MS: u64 = 1_000_000;
const CLOCK_TICK_MS: u64 = 5;
const RNG_SEED: u64 = 42;

fn logical_clock() -> Clock {
    Clock::logical(CLOCK_START_MS, CLOCK_TICK_MS)
}

/// Three user-scope facts in an in-memory store: the live side of the seam.
async fn seeded_store() -> Arc<InMemoryMemoryStore> {
    let store = Arc::new(InMemoryMemoryStore::new());
    for (key, confidence, created_ms) in [
        ("timezone", 0.9, 1_000),
        ("tone", 0.7, 2_000),
        ("units", 0.8, 3_000),
    ] {
        store
            .put(&seeded_record(
                MemoryKind::Fact,
                Some(key),
                confidence,
                created_ms,
            ))
            .await
            .unwrap();
    }
    store
}

fn user_facts_query() -> MemoryQuery {
    MemoryQuery {
        scope: Some(ScopeAddress::new(MemoryScope::User, "user-7")),
        kinds: vec![MemoryKind::Fact],
        min_confidence: Some(0.5),
        ..MemoryQuery::default()
    }
}

#[tokio::test]
async fn journaled_read_records_request_and_assembly() {
    let store = seeded_store().await;
    let journal = Journal::new(RUN_ID, THREAD_ID, logical_clock());
    let memory = journal.memory(MemorySource::Store(store));
    let budget = ContextBudget::new(1_000);
    let assembly = memory
        .read(&user_facts_query(), &budget, Some("memory-run-1:1".into()))
        .await
        .unwrap();

    // The filtered set, ranked by confidence-then-recency here (no
    // priorities): units (0.8), timezone (0.9)? — confidence descends
    // first, so timezone (0.9) leads.
    assert_eq!(assembly.records.len(), 3);
    assert_eq!(
        assembly.records[0].key.as_deref(),
        Some("timezone"),
        "rank: confidence descends before recency"
    );

    let events = journal.events();
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event.kind, RunEventKind::MemoryRead);
    assert_eq!(event.effect, Effect::ReadOnly);
    assert_eq!(event.parent.as_deref(), Some("memory-run-1:1"));

    // The journaled request is the resolved query (as_of stamped through
    // the run's logical clock) plus the budget.
    let Some(PayloadRef::Inline(input)) = &event.input else {
        panic!("read request travels inline")
    };
    let expected_as_of = ts(CLOCK_START_MS as i64); // first clock read
    assert_eq!(
        input["query"]["as_of"],
        serde_json::to_value(expected_as_of).unwrap()
    );
    assert_eq!(input["budget"]["max_tokens"].as_u64().unwrap(), 1_000);
    // The journaled output is the assembly itself — ids and order.
    let Some(PayloadRef::Inline(output)) = &event.output else {
        panic!("assembly travels inline")
    };
    assert_eq!(
        output["memory_ids"],
        serde_json::to_value(&assembly.memory_ids).unwrap()
    );
    assert_eq!(
        output["token_accounting"]["bytes_per_token"]
            .as_u64()
            .unwrap(),
        4
    );
}

#[tokio::test]
async fn equal_state_and_budget_give_byte_equal_assemblies() {
    // The determinism property the design requires, stated as a test: two
    // reads over equal store state under equal budgets produce byte-equal
    // assemblies (timestamps live on the events, not the assembly).
    let store_a = seeded_store().await;
    let store_b = seeded_store().await;
    let journal_a = Journal::new(RUN_ID, THREAD_ID, logical_clock());
    let journal_b = Journal::new(RUN_ID, THREAD_ID, logical_clock());
    let memory_a = journal_a.memory(MemorySource::Store(store_a));
    let memory_b = journal_b.memory(MemorySource::Store(store_b));
    let budget = ContextBudget::new(1_000);
    let a = memory_a
        .read(&user_facts_query(), &budget, None)
        .await
        .unwrap();
    let b = memory_b
        .read(&user_facts_query(), &budget, None)
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_vec(&a).unwrap(),
        serde_json::to_vec(&b).unwrap()
    );
}

// ---------- wave-1 exit: replay-serving ----------

/// Build the one-node memory-reading graph. The node reads its causal
/// parent from `PARENT_EVENT_KEY`, performs the journaled read through the
/// captured handle, and writes the assembly's ids into state — so the run's
/// *behavior* depends on the served content, not just its evidence.
fn memory_graph(source: MemorySource, journal: &Journal) -> (Graph, StateSpec) {
    let spec = StateSpec::new().channel("memory_ids", Reducer::Overwrite);
    let memory = journal.memory(source);
    let mut builder = GraphBuilder::new();
    builder.add_node("reader", move |ctx: NodeContext| {
        let memory = memory.clone();
        async move {
            let parent = ctx
                .config()
                .extra
                .get(PARENT_EVENT_KEY)
                .and_then(Value::as_str)
                .map(str::to_owned);
            let assembly = memory
                .read(&user_facts_query(), &ContextBudget::new(1_000), parent)
                .await?;
            Ok(NodeOutput::update("memory_ids", json!(assembly.memory_ids)))
        }
    });
    builder.set_entry_point("reader");
    (builder.compile().unwrap(), spec)
}

async fn record_memory_run() -> JournalSnapshot {
    let store = seeded_store().await;
    let journal = Journal::new(RUN_ID, THREAD_ID, logical_clock());
    let (graph, spec) = memory_graph(MemorySource::Store(store), &journal);
    let outcome = Executor::new()
        .run(
            &graph,
            &spec,
            State::new(),
            RunConfig::new(THREAD_ID)
                .with_journal(journal.clone())
                .with_rng(RngSource::seeded(RNG_SEED)),
        )
        .await
        .unwrap();
    match outcome {
        ExecutionOutcome::Done(state) => {
            assert_eq!(
                state
                    .get("memory_ids")
                    .and_then(Value::as_array)
                    .map(Vec::len),
                Some(3),
                "the recorded run's behavior consumed the assembly"
            );
        }
        other => panic!("expected Done, got {other:?}"),
    }
    journal.snapshot()
}

/// The wave-1 exit property, end to end: an exact replay of a
/// memory-reading run serves the journaled assembly byte-identically.
#[tokio::test]
async fn exact_replay_serves_the_journaled_assembly_byte_identically() {
    let snapshot = record_memory_run().await;
    assert_eq!(
        snapshot
            .events
            .iter()
            .filter(|e| e.kind == RunEventKind::MemoryRead)
            .count(),
        1,
        "the recorded run journaled exactly one memory read"
    );

    let replay = ExactReplay::new(snapshot.clone()).unwrap();
    let journal = replay.fresh_journal(logical_clock());
    let source = MemoryReplaySource::new(replay.snapshot());
    let (graph, spec) = memory_graph(MemorySource::Replay(source.clone()), &journal);
    let outcome = replay
        .run_and_verify(
            &graph,
            &spec,
            State::new(),
            ReplayParams::new(journal, RngSource::seeded(RNG_SEED)),
        )
        .await
        .unwrap();

    // Every recorded memory read was served — the store was never queried
    // (the replay source is the only answer the run got).
    assert!(source.is_exhausted());

    // Byte-identical evidence (run_and_verify asserted structural equality;
    // the serialized bytes are the claim this wave makes).
    assert_eq!(
        serde_json::to_string(&snapshot).unwrap(),
        serde_json::to_string(&outcome.journal).unwrap()
    );

    // The served assembly specifically: byte-identical output payloads on
    // the two MemoryRead events.
    let assembly_of = |snapshot: &JournalSnapshot| {
        snapshot
            .events
            .iter()
            .find(|e| e.kind == RunEventKind::MemoryRead)
            .map(|e| serde_json::to_vec(&e.output).unwrap())
            .unwrap()
    };
    assert_eq!(assembly_of(&snapshot), assembly_of(&outcome.journal));

    // And the replayed run's behavior consumed the same content.
    match &outcome.outcome {
        ExecutionOutcome::Done(state) => {
            assert_eq!(
                state
                    .get("memory_ids")
                    .and_then(Value::as_array)
                    .map(Vec::len),
                Some(3)
            );
        }
        other => panic!("expected Done, got {other:?}"),
    }
}

/// The divergence half of the property: a replayed run asking a different
/// question fails loudly instead of improvising an answer.
#[tokio::test]
async fn replay_refuses_a_divergent_request() {
    let snapshot = record_memory_run().await;
    let journal = Journal::new(RUN_ID, THREAD_ID, logical_clock());
    let source = MemoryReplaySource::new(&snapshot);
    let memory = journal.memory(MemorySource::Replay(source));
    // A different query (higher confidence floor) hashes differently.
    let divergent = MemoryQuery {
        min_confidence: Some(0.99),
        ..user_facts_query()
    };
    let error = memory
        .read(&divergent, &ContextBudget::new(1_000), None)
        .await
        .unwrap_err();
    assert!(matches!(error, RustyError::Replay(_)));
    assert!(error.to_string().contains("divergence"));
}

/// A live store cannot impersonate the journaled assembly: replaying
/// against an empty store produces different evidence, and verification
/// catches it.
#[tokio::test]
async fn replay_verification_catches_a_re_queried_store() {
    let snapshot = record_memory_run().await;
    let replay = ExactReplay::new(snapshot.clone()).unwrap();
    let journal = replay.fresh_journal(logical_clock());
    // The replayed run reads from a live (empty) store instead of the
    // journal — the request matches, the assembly does not.
    let (graph, spec) = memory_graph(
        MemorySource::Store(Arc::new(InMemoryMemoryStore::new())),
        &journal,
    );
    let outcome = replay
        .run(
            &graph,
            &spec,
            State::new(),
            ReplayParams::new(journal, RngSource::seeded(RNG_SEED)),
        )
        .await
        .unwrap();
    let error = replay.verify(&outcome.journal).unwrap_err();
    assert!(matches!(error, RustyError::Replay(_)));
    assert!(error.to_string().contains("event mismatch"));
}
