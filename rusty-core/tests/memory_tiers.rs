//! Memory-organization integration tests (R0.13 wave 2): tiers, the key
//! grammar and write-gate dedup, consolidation scheduling, the utility
//! index, the two-stage re-rank driver, the learn-gate delta, and the
//! recall measurement that feeds the vector decision.
//!
//! Golden files pin the new wire shapes under `tests/golden/`;
//! `UPDATE_GOLDEN=1` rewrites them — the diff is the contract change under
//! review. The pre-wave `candidate_memory_configuration.json` golden is
//! also round-tripped here to prove the wave's additive `rank` /
//! `maintenance` members leave the existing wire shape byte-identical.

use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{json, Value};

use rusty_agent_runtime::context::{
    ContextPipeline, ContextPolicy, MemorySectionPolicy, SectionKind, SectionManifest,
    CONTEXT_POLICY_SCHEMA_VERSION, MANIFEST_FORMAT_VERSION, MANIFEST_MESSAGE_NAME,
};
use rusty_agent_runtime::effects::ApprovalToken;
use rusty_agent_runtime::journal::{Clock, Journal, JournalSnapshot};
use rusty_agent_runtime::learn::{
    admit_promotion, promotion_effect_id, Candidate, CandidateContent, CandidateEvaluation,
    EvaluationThresholds, EvaluationVerdict, EvidenceSpan, PromotionAuthority, PromotionDecision,
    PromotionEnvelope, PromotionReceipt, ReplaySummary, RollbackReceipt, VersionPointer,
};
use rusty_agent_runtime::memory::{
    apply_query, assemble, consolidation_summary, estimated_tokens, memory_effect_key, plan_forget,
    ContextBudget, InMemoryMemoryStore, JournaledMemory, MemoryKind, MemoryProvenance, MemoryQuery,
    MemoryRecord, MemoryReplaySource, MemoryScope, MemorySource, MemoryStore, ProvenanceAuthor,
    ScopeAddress, ValidityWindow, MEMORY_SCHEMA_VERSION,
};
use rusty_agent_runtime::memory_tiers::{
    build_utility_index, consolidation_due, forgetting_candidates, rederive_section,
    ConsolidationPolicy, ConsolidationTrigger, GateOutcome, HierarchicalKey, KeyGrammar,
    MemoryTier, RankPolicy, RunOutcome, TieredMemoryDriver, TieredMemorySource, UtilityEntry,
    UtilityIndex, UtilityRun, WriteGate, NEUTRAL_SUCCESS_BPS, TIER_MANIFEST_FORMAT_VERSION,
};
use rusty_agent_runtime::record::{EventStatus, PayloadRef, RunEventKind};
use rusty_agent_runtime::error::RustyError;

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

const CLOCK_START_MS: i64 = 1_750_100_000_000;

fn logical_clock() -> Clock {
    Clock::logical(CLOCK_START_MS as u64, 1)
}

fn ts(millis: i64) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp_millis(millis).unwrap()
}

fn provenance(written_at: i64) -> MemoryProvenance {
    MemoryProvenance {
        author: ProvenanceAuthor::Agent {
            agent_id: "researcher-7".into(),
        },
        evidence: Default::default(),
        written_at: ts(written_at),
    }
}

/// A semantic-tier record (user scope, fact kind).
fn semantic_record(
    user: &str,
    key: &str,
    priority: i64,
    confidence: f64,
    clock_ms: i64,
    content: Value,
) -> MemoryRecord {
    MemoryRecord::new(
        MemoryKind::Fact,
        ScopeAddress::new(MemoryScope::User, user),
        provenance(clock_ms),
        confidence,
        ValidityWindow::starting(ts(clock_ms - 1_000)),
        ts(clock_ms),
        content,
    )
    .unwrap()
    .with_key(key)
    .with_priority(priority)
}

fn working_record(run: &str, key: &str, clock_ms: i64, content: Value) -> MemoryRecord {
    MemoryRecord::new(
        MemoryKind::Fact,
        ScopeAddress::new(MemoryScope::Run, run),
        provenance(clock_ms),
        0.9,
        ValidityWindow::starting(ts(clock_ms - 1_000)),
        ts(clock_ms),
        content,
    )
    .unwrap()
    .with_key(key)
}

fn episodic_record(agent: &str, key: &str, priority: i64, clock_ms: i64, content: Value) -> MemoryRecord {
    MemoryRecord::new(
        MemoryKind::Summary,
        ScopeAddress::new(MemoryScope::Agent, agent),
        provenance(clock_ms),
        0.9,
        ValidityWindow::starting(ts(clock_ms - 1_000)),
        ts(clock_ms),
        content,
    )
    .unwrap()
    .with_key(key)
    .with_priority(priority)
}

fn resolve(snapshot: &JournalSnapshot, payload: &PayloadRef) -> Value {
    match payload {
        PayloadRef::Inline(value) => value.clone(),
        PayloadRef::Artifact(reference) => snapshot.artifacts[&reference.sha256].clone(),
    }
}

/// The single journaled MemoryRead output of a journal that ran exactly one
/// tiered assembly.
fn tiered_read_output(snapshot: &JournalSnapshot) -> Value {
    let event = snapshot
        .events
        .iter()
        .find(|event| event.kind == RunEventKind::MemoryRead)
        .expect("one journaled memory read");
    resolve(snapshot, event.output.as_ref().expect("read output"))
}

// ---------- tiers ----------

#[test]
fn tier_classification_is_the_documented_mapping() {
    for kind in [
        MemoryKind::Fact,
        MemoryKind::Preference,
        MemoryKind::Example,
        MemoryKind::Summary,
    ] {
        assert_eq!(
            MemoryTier::classify(MemoryScope::Run, kind),
            MemoryTier::Working,
            "run scope is working memory whatever the kind"
        );
    }
    for scope in [MemoryScope::Agent, MemoryScope::Team] {
        assert_eq!(
            MemoryTier::classify(scope, MemoryKind::Summary),
            MemoryTier::Episodic
        );
        assert_eq!(
            MemoryTier::classify(scope, MemoryKind::Fact),
            MemoryTier::Semantic
        );
    }
    // User/tenant summaries distill what is true, not what happened.
    assert_eq!(
        MemoryTier::classify(MemoryScope::User, MemoryKind::Summary),
        MemoryTier::Semantic
    );
    assert!(MemoryTier::Working < MemoryTier::Episodic);
    assert!(MemoryTier::Episodic < MemoryTier::Semantic);
}

// ---------- the key grammar ----------

#[test]
fn hierarchical_keys_parse_and_validate() {
    let key = HierarchicalKey::parse("user.timezone").unwrap();
    assert_eq!(key.domain, "user");
    assert_eq!(key.name, "timezone");
    let key = HierarchicalKey::parse("tool.search.quirks").unwrap();
    assert_eq!(key.domain, "tool");
    assert_eq!(key.name, "search.quirks");
    for bad in ["", "noseparator", "User.timezone", "user..timezone", "user.", ".user", "user.tz!"] {
        assert!(
            HierarchicalKey::parse(bad).is_err(),
            "key `{bad}` must fail the grammar"
        );
    }
}

#[test]
fn the_gate_enforces_declared_domains() {
    let grammar = KeyGrammar::declare(["user", "tool"]).unwrap();
    assert!(grammar.validate("user.timezone").is_ok());
    let error = grammar.validate("episode.run_9").unwrap_err();
    assert!(matches!(error, RustyError::InvalidUpdate(_)), "{error}");
    assert!(KeyGrammar::declare(["two.parts"]).is_err());
    assert!(KeyGrammar::declare(["Upper"]).is_err());
}

// ---------- the write gate ----------

fn gated_memory(
    store: Arc<InMemoryMemoryStore>,
    journal: &Journal,
) -> JournaledMemory {
    JournaledMemory::new(journal, MemorySource::Store(store))
}

#[tokio::test]
async fn independent_same_content_writes_converge() {
    let store = Arc::new(InMemoryMemoryStore::new());
    let journal = Journal::new("run-dedup", "t-dedup", logical_clock());
    let memory = gated_memory(store.clone(), &journal);
    let gate = WriteGate::new().with_grammar(KeyGrammar::declare(["user"]).unwrap());
    let now = ts(CLOCK_START_MS);

    let first = semantic_record("user-7", "user.timezone", 0, 0.9, 1_000, json!({"tz": "UTC+4"}));
    let written = gate
        .write(&memory, store.as_ref(), &first, now, None)
        .await
        .unwrap();
    assert_eq!(written.outcome, GateOutcome::Stored);
    assert_eq!(written.memory_id, first.memory_id);

    // An independent submission: same scope, key, and canonical content —
    // but a different provenance, so the content address differs and
    // store-level convergence cannot see it.
    let second = semantic_record("user-7", "user.timezone", 0, 0.9, 2_000, json!({"tz": "UTC+4"}));
    assert_ne!(first.memory_id, second.memory_id);
    let converged = gate
        .write(&memory, store.as_ref(), &second, now, None)
        .await
        .unwrap();
    assert_eq!(converged.outcome, GateOutcome::Converged);
    assert_eq!(converged.memory_id, first.memory_id);
    assert_eq!(store.all().await.unwrap().len(), 1, "one record, not two");

    // The convergence is journaled: the second MemoryWrite names the record
    // the write resolved to, under the converged effect key.
    let events = journal.events();
    let writes: Vec<_> = events
        .iter()
        .filter(|event| event.kind == RunEventKind::MemoryWrite)
        .collect();
    assert_eq!(writes.len(), 2);
    let snapshot = journal.snapshot();
    let output = resolve(&snapshot, writes[1].output.as_ref().unwrap());
    assert_eq!(output["memory_id"], json!(first.memory_id));
    assert_eq!(
        writes[1].input.as_ref().and_then(|p| match p {
            PayloadRef::Inline(v) => Some(v["effect_key"].clone()),
            PayloadRef::Artifact(_) => None,
        }),
        Some(json!(memory_effect_key(&first.scope, &first.memory_id)))
    );
}

#[tokio::test]
async fn same_key_different_content_is_not_dedup() {
    let store = Arc::new(InMemoryMemoryStore::new());
    let journal = Journal::new("run-nodedup", "t-nodedup", logical_clock());
    let memory = gated_memory(store.clone(), &journal);
    let gate = WriteGate::new();
    let now = ts(CLOCK_START_MS);

    let first = semantic_record("user-7", "user.timezone", 0, 0.9, 1_000, json!({"tz": "UTC+4"}));
    gate.write(&memory, store.as_ref(), &first, now, None)
        .await
        .unwrap();
    // Same key, a changed claim: supersession or conflict, never dedup.
    let correction = semantic_record("user-7", "user.timezone", 0, 1.0, 2_000, json!({"tz": "UTC+5"}))
        .with_supersedes(first.memory_id.clone());
    let written = gate
        .write(&memory, store.as_ref(), &correction, now, None)
        .await
        .unwrap();
    assert_eq!(written.outcome, GateOutcome::Stored);
    assert_eq!(store.all().await.unwrap().len(), 2);

    // An expired same-content record does not block a fresh assertion.
    let expired = semantic_record("user-7", "user.language", 0, 0.9, 500, json!({"lang": "en"}))
        .with_expires_at(ts(600));
    gate.write(&memory, store.as_ref(), &expired, now, None)
        .await
        .unwrap();
    let fresh = semantic_record("user-7", "user.language", 0, 0.9, 3_000, json!({"lang": "en"}));
    let written = gate
        .write(&memory, store.as_ref(), &fresh, now, None)
        .await
        .unwrap();
    assert_eq!(written.outcome, GateOutcome::Stored);
}

#[tokio::test]
async fn a_malformed_key_fails_the_gate_without_journaling() {
    let store = Arc::new(InMemoryMemoryStore::new());
    let journal = Journal::new("run-gate", "t-gate", logical_clock());
    let memory = gated_memory(store.clone(), &journal);
    let gate = WriteGate::new().with_grammar(KeyGrammar::declare(["user"]).unwrap());
    let bad = semantic_record("user-7", "undeclared.key", 0, 0.9, 1_000, json!({"x": 1}));
    let error = gate
        .write(&memory, store.as_ref(), &bad, ts(CLOCK_START_MS), None)
        .await
        .unwrap_err();
    assert!(matches!(error, RustyError::InvalidUpdate(_)), "{error}");
    assert!(store.all().await.unwrap().is_empty());
    assert!(journal.events().is_empty(), "a refused write journals nothing");
}

// ---------- consolidation scheduling ----------

fn episode(agent: &str, n: u64, clock_ms: i64) -> MemoryRecord {
    MemoryRecord::new(
        MemoryKind::Summary,
        ScopeAddress::new(MemoryScope::Agent, agent),
        provenance(clock_ms),
        0.8,
        ValidityWindow::starting(ts(clock_ms - 1_000)),
        ts(clock_ms),
        json!({"episode": n}),
    )
    .unwrap()
    .with_key(format!("episode.run_{n}"))
}

fn episode_policy() -> ConsolidationPolicy {
    ConsolidationPolicy {
        scope: MemoryScope::Agent,
        key_domain: "episode".into(),
        max_records: Some(3),
        max_tokens: None,
        max_age_ms: None,
        distiller: "episode-distiller".into(),
    }
}

#[test]
fn consolidation_fires_on_the_declared_thresholds() {
    let now = ts(10_000);
    let universe: Vec<MemoryRecord> = (0..3).map(|n| episode("agent-1", n, 1_000 + n as i64)).collect();

    // Below the count threshold: not due.
    let mut policy = episode_policy();
    policy.max_records = Some(4);
    assert!(consolidation_due(&policy, &universe, now, 20).unwrap().is_none());

    // At the threshold: due, sources oldest-first.
    let due = consolidation_due(&episode_policy(), &universe, now, 20)
        .unwrap()
        .expect("three records trip the count trigger");
    assert_eq!(due.triggered_by, vec![ConsolidationTrigger::RecordCount]);
    assert_eq!(due.distiller, "episode-distiller");
    let created: Vec<i64> = due
        .sources
        .iter()
        .map(|r| r.created_at.timestamp_millis())
        .collect();
    assert_eq!(created, vec![1_000, 1_001, 1_002]);

    // The token-footprint trigger, independently.
    let mut policy = episode_policy();
    policy.max_records = None;
    policy.max_tokens = Some(1);
    let due = consolidation_due(&policy, &universe, now, 20).unwrap().unwrap();
    assert_eq!(due.triggered_by, vec![ConsolidationTrigger::TokenFootprint]);

    // The age trigger: the oldest record is 9 s old at `now`.
    let mut policy = episode_policy();
    policy.max_records = None;
    policy.max_age_ms = Some(9_000);
    let due = consolidation_due(&policy, &universe, now, 20).unwrap().unwrap();
    assert_eq!(due.triggered_by, vec![ConsolidationTrigger::Age]);
    policy.max_age_ms = Some(9_001);
    assert!(consolidation_due(&policy, &universe, now, 20).unwrap().is_none());

    // Other scopes and domains stay out of the policy's set.
    let off_scope = semantic_record("user-7", "user.timezone", 0, 0.9, 1_000, json!({"tz": "UTC+4"}));
    let due = consolidation_due(&episode_policy(), &[off_scope], now, 20).unwrap();
    assert!(due.is_none());
}

#[test]
fn consolidation_policy_validation_is_loud() {
    let mut policy = episode_policy();
    policy.max_records = None;
    let error = consolidation_due(&policy, &[], ts(10_000), 20).unwrap_err();
    assert!(matches!(error, RustyError::InvalidUpdate(_)), "{error}");

    let mut policy = episode_policy();
    policy.distiller = "  ".into();
    assert!(policy.validate().is_err());

    let mut policy = episode_policy();
    policy.key_domain = "two.parts".into();
    assert!(policy.validate().is_err());
}

#[tokio::test]
async fn a_due_consolidation_supersedes_sources_and_forgetting_walks_the_chain() {
    // The composition the policy feeds: shipped consolidation_summary
    // supersedes the sources in default retrieval, and plan_forget's
    // transitive invalidation takes the summary with its sources.
    let now = ts(10_000);
    let universe: Vec<MemoryRecord> = (0..3).map(|n| episode("agent-1", n, 1_000 + n as i64)).collect();
    let due = consolidation_due(&episode_policy(), &universe, now, 20)
        .unwrap()
        .unwrap();
    let summary = consolidation_summary(
        ScopeAddress::new(MemoryScope::Agent, "agent-1"),
        due.distiller.clone(),
        &due.sources,
        json!({"episodes": [0, 1, 2]}),
        now,
    )
    .unwrap();
    assert_eq!(MemoryTier::of(&summary), MemoryTier::Episodic);

    let mut after = universe.clone();
    after.push(summary.clone());
    let visible = apply_query(&after, &MemoryQuery::default(), now);
    assert_eq!(visible.len(), 1, "the summary supersedes its sources");
    assert_eq!(visible[0].memory_id, summary.memory_id);

    let plan = plan_forget(&after, &[due.sources[0].memory_id.clone()]);
    assert!(
        plan.invalidated.contains(&summary.memory_id),
        "forgetting a source invalidates the dependent summary"
    );

    // Consolidated records are out of the next due evaluation's set.
    let mut policy = episode_policy();
    policy.max_records = Some(1);
    assert!(consolidation_due(&policy, &after, now, 20).unwrap().is_none());
}

// ---------- the utility index ----------

/// Journal one read of `key` over `store` as run `run_id`, returning the
/// snapshot.
async fn journaled_read_run(
    store: &Arc<InMemoryMemoryStore>,
    run_id: &str,
    key: &str,
    clock_ms: i64,
) -> JournalSnapshot {
    let journal = Journal::new(run_id, "t-utility", Clock::logical(clock_ms as u64, 1));
    let memory = JournaledMemory::new(&journal, MemorySource::Store(store.clone()));
    let query = MemoryQuery {
        scope: Some(ScopeAddress::new(MemoryScope::User, "user-7")),
        key: Some(key.to_owned()),
        as_of: Some(ts(clock_ms)),
        ..MemoryQuery::default()
    };
    memory
        .read(&query, &ContextBudget::new(100_000), None)
        .await
        .unwrap();
    journal.snapshot()
}

#[tokio::test]
async fn the_utility_index_counts_successful_and_failed_uses() {
    let store = Arc::new(InMemoryMemoryStore::new());
    let good = semantic_record("user-7", "user.good", 0, 0.9, 1_000, json!({"v": "good"}));
    let bad = semantic_record("user-7", "user.bad", 0, 0.9, 2_000, json!({"v": "bad"}));
    let mixed = semantic_record("user-7", "user.mixed", 0, 0.9, 3_000, json!({"v": "mixed"}));
    for record in [&good, &bad, &mixed] {
        store.put(record).await.unwrap();
    }

    let mut runs: Vec<(JournalSnapshot, RunOutcome)> = Vec::new();
    for n in 0..3u8 {
        let snapshot = journaled_read_run(&store, &format!("good-{n}"), "user.good", 10_000).await;
        runs.push((snapshot, RunOutcome { status: EventStatus::Ok, score_bps: Some(9_000) }));
    }
    for n in 0..2u8 {
        let snapshot = journaled_read_run(&store, &format!("bad-{n}"), "user.bad", 20_000).await;
        runs.push((snapshot, RunOutcome { status: EventStatus::Error, score_bps: None }));
    }
    // A graded run below the success bar counts as a failure even when the
    // terminal status is Ok — it completed, poorly.
    let snapshot = journaled_read_run(&store, "bad-graded", "user.bad", 30_000).await;
    runs.push((snapshot, RunOutcome { status: EventStatus::Ok, score_bps: Some(5_500) }));
    // An interrupted run is not terminal evidence either way.
    let snapshot = journaled_read_run(&store, "mixed-interrupted", "user.mixed", 40_000).await;
    runs.push((snapshot, RunOutcome { status: EventStatus::Interrupted, score_bps: None }));

    let utility_runs: Vec<UtilityRun> = runs
        .iter()
        .map(|(snapshot, outcome)| UtilityRun { snapshot, outcome: *outcome })
        .collect();
    let index = build_utility_index(&utility_runs, Some(6_000), ts(50_000)).unwrap();

    assert_eq!(
        index.entries[&good.memory_id],
        UtilityEntry { successful_uses: 3, failed_uses: 0 }
    );
    assert_eq!(
        index.entries[&bad.memory_id],
        UtilityEntry { successful_uses: 0, failed_uses: 3 }
    );
    assert!(!index.entries.contains_key(&mixed.memory_id));
    assert_eq!(index.success_bps(&good.memory_id), 4 * 10_000 / 5);
    assert_eq!(index.success_bps(&bad.memory_id), 10_000 / 5);
    assert_eq!(index.success_bps("unobserved"), NEUTRAL_SUCCESS_BPS);
}

#[tokio::test]
async fn the_index_rebuilds_from_journals_byte_identically() {
    let store = Arc::new(InMemoryMemoryStore::new());
    let record = semantic_record("user-7", "user.good", 0, 0.9, 1_000, json!({"v": "good"}));
    store.put(&record).await.unwrap();
    let snapshot = journaled_read_run(&store, "run-1", "user.good", 10_000).await;
    let runs = [UtilityRun {
        snapshot: &snapshot,
        outcome: RunOutcome { status: EventStatus::Ok, score_bps: Some(9_000) },
    }];
    let first = build_utility_index(&runs, Some(6_000), ts(50_000)).unwrap();
    let second = build_utility_index(&runs, Some(6_000), ts(50_000)).unwrap();
    assert_eq!(
        serde_json::to_vec(&first).unwrap(),
        serde_json::to_vec(&second).unwrap()
    );
    // …and a rebuild over a serialized-and-reparsed journal agrees.
    let reparsed: JournalSnapshot =
        serde_json::from_str(&serde_json::to_string(&snapshot).unwrap()).unwrap();
    let runs = [UtilityRun {
        snapshot: &reparsed,
        outcome: RunOutcome { status: EventStatus::Ok, score_bps: Some(9_000) },
    }];
    let rebuilt = build_utility_index(&runs, Some(6_000), ts(50_000)).unwrap();
    assert_eq!(
        serde_json::to_vec(&first).unwrap(),
        serde_json::to_vec(&rebuilt).unwrap()
    );
}

#[test]
fn forgetting_candidates_are_expired_and_never_useful() {
    let now = ts(10_000);
    let expired_useless = semantic_record("user-7", "user.old", 0, 0.9, 500, json!({"v": 1}))
        .with_expires_at(ts(600));
    let expired_useful = semantic_record("user-7", "user.kept", 0, 0.9, 500, json!({"v": 2}))
        .with_expires_at(ts(600));
    let live_useless = semantic_record("user-7", "user.live", 0, 0.9, 500, json!({"v": 3}));
    let universe = vec![expired_useless.clone(), expired_useful.clone(), live_useless];
    let index = UtilityIndex {
        stamp: now,
        entries: [(expired_useful.memory_id.clone(), UtilityEntry {
            successful_uses: 2,
            failed_uses: 0,
        })]
        .into_iter()
        .collect(),
    };
    let candidates = forgetting_candidates(&index, &universe, now);
    assert_eq!(candidates, vec![expired_useless.memory_id.clone()]);
}

// ---------- the two-stage driver ----------

fn tiered_store() -> (Arc<InMemoryMemoryStore>, Vec<MemoryRecord>) {
    // Base rank order: semantic-high (priority 9), episodic (5),
    // semantic-low (1), working (0). Tier order inverts it.
    let records = vec![
        working_record("run-1", "run.scratch", 1_000, json!({"note": "partial draft"})),
        episodic_record("agent-1", "episode.run_8", 5, 2_000, json!({"episode": "run-8 summary"})),
        semantic_record("user-7", "user.timezone", 9, 0.9, 3_000, json!({"tz": "UTC+4"})),
        semantic_record("user-7", "user.language", 1, 0.8, 4_000, json!({"lang": "en-US"})),
    ];
    let store = Arc::new(InMemoryMemoryStore::new());
    (store, records)
}

fn tiered_query() -> MemoryQuery {
    MemoryQuery {
        as_of: Some(ts(CLOCK_START_MS)),
        ..MemoryQuery::default()
    }
}

#[tokio::test]
async fn the_floor_driver_is_the_shipped_rank_within_tiers() {
    let (store, records) = tiered_store();
    for record in &records {
        store.put(record).await.unwrap();
    }
    let journal = Journal::new("run-floor", "t-floor", logical_clock());
    let driver = TieredMemoryDriver::floor(ts(CLOCK_START_MS));
    let section = driver
        .assemble_section(
            &journal,
            &TieredMemorySource::Store(store.clone()),
            &tiered_query(),
            &ContextBudget::new(100_000),
            None,
        )
        .await
        .unwrap();

    // Tier-major: working → episodic → semantic; within the semantic tier
    // the shipped rank (priority 9 before 1).
    let ids = section.memory_ids();
    assert_eq!(
        ids,
        vec![
            records[0].memory_id.clone(),
            records[1].memory_id.clone(),
            records[2].memory_id.clone(),
            records[3].memory_id.clone(),
        ]
    );
    // The zero-weight floor within a tier is exactly the shipped rank:
    // single-tier input, floor driver, and assemble() agree byte-for-byte.
    let semantic_only: Vec<MemoryRecord> = vec![records[2].clone(), records[3].clone()];
    let shipped = assemble(semantic_only, &ContextBudget::new(100_000)).unwrap();
    let journal = Journal::new("run-floor-1t", "t-floor-1t", logical_clock());
    let section = driver
        .assemble_section(
            &journal,
            &TieredMemorySource::Store(store),
            &MemoryQuery {
                scope: Some(ScopeAddress::new(MemoryScope::User, "user-7")),
                as_of: Some(ts(CLOCK_START_MS)),
                ..MemoryQuery::default()
            },
            &ContextBudget::new(100_000),
            None,
        )
        .await
        .unwrap();
    assert_eq!(section.memory_ids(), shipped.memory_ids);
}

#[tokio::test]
async fn utility_rerank_changes_selection_through_the_over_fetched_pool() {
    // Two semantic records, one packable slot: the base rank packs the
    // high-priority one; the utility-weighted re-rank packs the one with
    // the successful history — selection, not just order, because the
    // over-fetch put it in the pool.
    let winner = semantic_record("user-7", "user.quiet", 0, 0.7, 1_000, json!({"v": "x"}));
    let loud = semantic_record("user-7", "user.loud", 9, 0.95, 2_000, json!({"v": "y"}));
    let store = Arc::new(InMemoryMemoryStore::new());
    store.put(&winner).await.unwrap();
    store.put(&loud).await.unwrap();

    let cost = estimated_tokens(winner.content_bytes(), 20);
    let budget = ContextBudget::new(cost); // exactly one record fits
    let utility = UtilityIndex {
        stamp: ts(50_000),
        entries: [
            (winner.memory_id.clone(), UtilityEntry { successful_uses: 5, failed_uses: 0 }),
            (loud.memory_id.clone(), UtilityEntry { successful_uses: 0, failed_uses: 5 }),
        ]
        .into_iter()
        .collect(),
    };
    let rank = RankPolicy { utility_weight: 4, over_fetch_percent: 200 };
    let driver = TieredMemoryDriver::new(rank, utility).unwrap();
    let journal = Journal::new("run-rerank", "t-rerank", logical_clock());
    let query = MemoryQuery {
        scope: Some(ScopeAddress::new(MemoryScope::User, "user-7")),
        as_of: Some(ts(CLOCK_START_MS)),
        ..MemoryQuery::default()
    };
    let section = driver
        .assemble_section(&journal, &TieredMemorySource::Store(store.clone()), &query, &budget, None)
        .await
        .unwrap();

    assert_eq!(section.memory_ids(), vec![winner.memory_id.clone()]);
    assert!(section.truncated, "the pool held both; the budget kept one");
    // The journaled manifest pins the weights, the snapshot stamp, and the
    // over-fetched pool (base-rank order: loud first).
    assert_eq!(section.manifest.format, TIER_MANIFEST_FORMAT_VERSION);
    assert_eq!(section.manifest.rank, rank);
    assert_eq!(section.manifest.utility_stamp, ts(50_000));
    assert_eq!(
        section.manifest.over_fetch_ids,
        vec![loud.memory_id.clone(), winner.memory_id.clone()]
    );
    let output = tiered_read_output(&journal.snapshot());
    assert_eq!(output["section_manifest"]["rank"]["utility_weight"], json!(4));

    // The floor, same inputs: the shipped rank's pick.
    let journal = Journal::new("run-rerank-floor", "t-rerank-floor", logical_clock());
    let floor = TieredMemoryDriver::floor(ts(CLOCK_START_MS));
    let section = floor
        .assemble_section(
            &journal,
            &TieredMemorySource::Store(store),
            &query,
            &budget,
            None,
        )
        .await
        .unwrap();
    assert_eq!(section.memory_ids(), vec![loud.memory_id.clone()]);
}

#[tokio::test]
async fn replay_serves_the_tiered_read_byte_identically_and_rederivation_agrees() {
    let (store, records) = tiered_store();
    for record in &records {
        store.put(record).await.unwrap();
    }
    let utility = UtilityIndex {
        stamp: ts(50_000),
        entries: [(
            records[3].memory_id.clone(),
            UtilityEntry { successful_uses: 4, failed_uses: 0 },
        )]
        .into_iter()
        .collect(),
    };
    let rank = RankPolicy { utility_weight: 2, over_fetch_percent: 150 };
    let driver = TieredMemoryDriver::new(rank, utility.clone()).unwrap();

    let journal = Journal::new("run-live", "t-live", logical_clock());
    let recorded = driver
        .assemble_section(
            &journal,
            &TieredMemorySource::Store(store.clone()),
            &tiered_query(),
            &ContextBudget::new(100_000),
            None,
        )
        .await
        .unwrap();
    let snapshot = journal.snapshot();

    // Exact replay: served byte-identically, re-journaled, source exhausted.
    let replay_journal = Journal::new("run-replay", "t-replay", logical_clock());
    let source = MemoryReplaySource::new(&snapshot);
    let replayed = driver
        .assemble_section(
            &replay_journal,
            &TieredMemorySource::Replay(source.clone()),
            &tiered_query(),
            &ContextBudget::new(100_000),
            None,
        )
        .await
        .unwrap();
    assert!(source.is_exhausted());
    assert_eq!(
        serde_json::to_vec(&recorded).unwrap(),
        serde_json::to_vec(&replayed).unwrap(),
        "replay serves the tiered section byte-identically"
    );
    assert_eq!(
        serde_json::to_vec(&tiered_read_output(&snapshot)).unwrap(),
        serde_json::to_vec(&tiered_read_output(&replay_journal.snapshot())).unwrap(),
        "the re-journaled read reproduces the recorded evidence"
    );

    // Re-derivation from the journaled pins: the over-fetched pool
    // re-resolves from the store by content address, and re-running the
    // two stages reproduces the packed section byte-for-byte.
    let mut pool: Vec<MemoryRecord> = Vec::new();
    for id in &recorded.manifest.over_fetch_ids {
        pool.push(
            store
                .get(id)
                .await
                .expect("store read")
                .expect("pool record lives"),
        );
    }
    let rederived = rederive_section(
        &driver,
        pool,
        &ContextBudget::new(100_000),
        &recorded.manifest,
    )
    .unwrap();
    assert_eq!(
        serde_json::to_vec(&recorded).unwrap(),
        serde_json::to_vec(&rederived).unwrap()
    );

    // A replay under different pins is divergence, loudly.
    let other = TieredMemoryDriver::new(
        RankPolicy { utility_weight: 9, over_fetch_percent: 150 },
        utility,
    )
    .unwrap();
    let replay_journal = Journal::new("run-diverged", "t-diverged", logical_clock());
    let error = other
        .assemble_section(
            &replay_journal,
            &TieredMemorySource::Replay(MemoryReplaySource::new(&snapshot)),
            &tiered_query(),
            &ContextBudget::new(100_000),
            None,
        )
        .await
        .unwrap_err();
    assert!(matches!(error, RustyError::Replay(_)), "{error}");
}

#[tokio::test]
async fn rank_policy_validation_rejects_under_fetch() {
    let error = TieredMemoryDriver::new(
        RankPolicy { utility_weight: 0, over_fetch_percent: 50 },
        UtilityIndex { stamp: ts(0), entries: Default::default() },
    )
    .unwrap_err();
    assert!(matches!(error, RustyError::InvalidUpdate(_)), "{error}");
}

// ---------- end to end through the W1 context pipeline ----------

#[tokio::test]
async fn a_tiered_memory_section_flows_through_the_context_pipeline() {
    // The driver assembles and journals the tiered section; the pipeline's
    // journaled read is then served the driver's evidence through the
    // shipped replay seam, and packs the tier-ordered records under its own
    // section manifest — context.rs untouched.
    let (store, records) = tiered_store();
    for record in &records {
        store.put(record).await.unwrap();
    }
    let rank = RankPolicy { utility_weight: 4, over_fetch_percent: 200 };
    let driver = TieredMemoryDriver::new(
        rank,
        UtilityIndex { stamp: ts(50_000), entries: Default::default() },
    )
    .unwrap();
    let budget = ContextBudget::new(512);
    let driver_journal = Journal::new("run-driver", "t-driver", logical_clock());
    let section = driver
        .assemble_section(
            &driver_journal,
            &TieredMemorySource::Store(store),
            &tiered_query(),
            &budget,
            None,
        )
        .await
        .unwrap();
    // Tier-major assembly: working first although the base rank puts it last.
    assert_eq!(section.memory_ids()[0], records[0].memory_id);
    let snapshot = driver_journal.snapshot();

    let policy = ContextPolicy {
        schema_version: CONTEXT_POLICY_SCHEMA_VERSION.to_owned(),
        budget: ContextBudget::new(100_000),
        tokenizer: Default::default(),
        identity: None,
        task: None,
        skills: None,
        tools: None,
        memory: Some(MemorySectionPolicy {
            budget_tokens: 512,
            overflow: None,
            query: tiered_query(),
        }),
        history: None,
        compaction: None,
    };
    let pipeline = ContextPipeline::new(policy).unwrap();
    let run_journal = Journal::new("run-pipeline", "t-pipeline", logical_clock());
    let memory = JournaledMemory::new(
        &run_journal,
        MemorySource::Replay(MemoryReplaySource::new(&snapshot)),
    );
    let assembly = pipeline
        .assemble(&Default::default(), Some(&memory))
        .await
        .unwrap();

    let memory_section = assembly
        .manifest
        .sections
        .iter()
        .find(|s| s.kind == SectionKind::Memory)
        .expect("memory section report");
    assert_eq!(
        memory_section.ids,
        section.memory_ids(),
        "the pipeline packs the driver's tier-ordered section"
    );
    // The manifest message rides inside the assembled input and parses.
    let manifest_message = assembly
        .messages
        .iter()
        .find(|m| m.name.as_deref() == Some(MANIFEST_MESSAGE_NAME))
        .expect("the manifest message");
    let text = manifest_message.content.as_deref().unwrap();
    assert!(text.starts_with(MANIFEST_FORMAT_VERSION));
    let manifest: SectionManifest =
        serde_json::from_str(text.split_once('\n').unwrap().1).unwrap();
    assert_eq!(manifest.sections.len(), 1);
    // The weights and snapshot stamp are pinned in the journaled read the
    // pipeline consumed.
    let output = tiered_read_output(&snapshot);
    assert_eq!(output["section_manifest"]["rank"], serde_json::to_value(rank).unwrap());
    assert_eq!(
        output["section_manifest"]["utility_stamp"],
        json!("1970-01-01T00:00:50Z")
    );
}

// ---------- the learn gate: rank + maintenance through promotion ----------

fn memory_config_candidate(utility_weight: u32, created_ms: i64) -> Candidate {
    Candidate::new(
        CandidateContent::MemoryConfiguration {
            name: "recall".into(),
            budget: ContextBudget::new(4096),
            default_filters: MemoryQuery::default(),
            schema_version: MEMORY_SCHEMA_VERSION.to_owned(),
            rank: Some(RankPolicy { utility_weight, over_fetch_percent: 200 }),
            maintenance: vec![episode_policy()],
        },
        ProvenanceAuthor::Distiller {
            name: "utility-distiller".into(),
        },
        EvidenceSpan {
            run_ids: vec!["run-abc".into()],
            ..EvidenceSpan::default()
        },
        ts(created_ms),
    )
    .unwrap()
}

fn evaluation(candidate: &Candidate) -> CandidateEvaluation {
    CandidateEvaluation {
        candidate_id: candidate.candidate_id.clone(),
        dataset_version: "recall-v1".into(),
        replay: ReplaySummary {
            fixture_ids: vec!["run-abc".into()],
            matched: 1,
            divergences: Vec::new(),
        },
        baseline_report: json!({
            "format_version": 1,
            "name": "recall@recall-v1",
            "dataset_version": "recall-v1",
            "summary": {"run_pass_rate": 0.5},
        }),
        candidate_report: json!({
            "format_version": 1,
            "name": "recall@recall-v1",
            "dataset_version": "recall-v1",
            "summary": {"run_pass_rate": 0.75},
        }),
        verdict: EvaluationVerdict {
            regressed: false,
            target_metric: "run_pass_rate".into(),
            baseline: Some(0.5),
            candidate: Some(0.75),
            delta: Some(0.25),
        },
        thresholds: EvaluationThresholds::default(),
        evaluated_by: ProvenanceAuthor::Distiller {
            name: "utility-distiller".into(),
        },
        evaluated_at: ts(1_750_100_003_000),
    }
}

#[test]
fn memory_config_rank_and_maintenance_promote_and_roll_back_byte_exactly() {
    // The wave's gate proof: a `memory_config` candidate carrying `rank`
    // and `maintenance` passes the envelope (approval-ruled, like every
    // registry kind), and the promote A → B → rollback walk restores A
    // byte-exactly — candidates are content-addressed and immutable, so
    // the pointer's `previous` IS the version that served.
    let a = memory_config_candidate(4, 1_750_100_002_000);
    let b = memory_config_candidate(8, 1_750_100_002_500);
    assert_ne!(a.candidate_id, b.candidate_id);
    assert_eq!(a.surface().as_str(), "memory_config:recall");

    // The gate holds the family at approval; a scoped token admits.
    let envelope = PromotionEnvelope::r08_default();
    let refusal = admit_promotion(&envelope, &a, Some(&evaluation(&a)), None)
        .expect_err("no token, no promotion");
    assert!(matches!(
        refusal,
        rusty_agent_runtime::learn::LearnError::Refused(
            rusty_agent_runtime::learn::PromotionRefusal::RequiresApproval { .. }
        )
    ));
    let approval = ApprovalToken::approve(promotion_effect_id(&a), "ops:test");
    let decision = admit_promotion(&envelope, &a, Some(&evaluation(&a)), Some(&approval))
        .expect("a scoped token admits");
    match decision.authority {
        PromotionAuthority::Approval { approved_by } => assert_eq!(approved_by, "ops:test"),
        other => panic!("expected approval authority, got {other:?}"),
    }

    let mut store = std::collections::HashMap::new();
    store.insert(a.candidate_id.to_string(), serde_json::to_vec(&a).unwrap());
    store.insert(b.candidate_id.to_string(), serde_json::to_vec(&b).unwrap());

    let promote_a = PromotionReceipt {
        candidate_id: a.candidate_id.clone(),
        surface: a.surface(),
        previous: None,
        decision: PromotionDecision {
            authority: PromotionAuthority::Approval {
                approved_by: "ops:test".into(),
            },
            canary: None,
        },
        promoted_at: ts(1_750_100_004_000),
    };
    let promote_b = PromotionReceipt {
        candidate_id: b.candidate_id.clone(),
        surface: a.surface(),
        previous: Some(a.candidate_id.clone()),
        decision: promote_a.decision.clone(),
        promoted_at: ts(1_750_100_005_000),
    };
    let pointer = VersionPointer::new(a.surface())
        .promoted(&promote_a)
        .promoted(&promote_b);
    assert_eq!(pointer.active, Some(b.candidate_id.clone()));

    let rolled = pointer.rolled_back(&RollbackReceipt {
        surface: a.surface(),
        from: b.candidate_id.clone(),
        to: promote_b.previous.clone(),
        cause: "recall regression on the replay set".into(),
        rolled_back_at: ts(1_750_100_006_000),
    });
    let restored_id = rolled.active.expect("rollback re-points to A");
    assert_eq!(restored_id, a.candidate_id);

    // Re-resolution by id returns the exact bytes that served before B.
    let redistilled = memory_config_candidate(4, 1_750_100_002_000);
    assert_eq!(redistilled.candidate_id, restored_id);
    let restored_bytes = store.get(restored_id.as_str()).expect("the restored id resolves");
    assert_eq!(*restored_bytes, serde_json::to_vec(&redistilled).unwrap());
    let restored: Candidate = serde_json::from_slice(restored_bytes).unwrap();
    restored.verify_address().unwrap();
}

// ---------- goldens ----------

#[test]
fn golden_extended_memory_configuration_candidate_shape() {
    assert_golden(
        "candidate_memory_configuration_maintenance.json",
        &memory_config_candidate(4, 1_750_100_002_000),
    );
}

#[test]
fn pre_wave_memory_configuration_wire_shape_is_byte_identical() {
    // The additivity proof at the wire level: the pre-wave golden parses
    // under the extended contract (the new members default to absent) and
    // re-serializes to the same bytes.
    let path = golden_path("candidate_memory_configuration.json");
    let expected = std::fs::read_to_string(&path).unwrap();
    let candidate: Candidate = serde_json::from_str(&expected).unwrap();
    candidate.verify_address().unwrap();
    let rendered = format!("{}\n", serde_json::to_string_pretty(&candidate).unwrap());
    assert_eq!(
        rendered, expected,
        "the pre-wave memory_config golden must round-trip byte-identically"
    );
}

#[tokio::test]
async fn golden_utility_index_shape() {
    let store = Arc::new(InMemoryMemoryStore::new());
    let record = semantic_record("user-7", "user.good", 0, 0.9, 1_000, json!({"v": "good"}));
    store.put(&record).await.unwrap();
    let snapshot = journaled_read_run(&store, "run-1", "user.good", 10_000).await;
    let runs = [UtilityRun {
        snapshot: &snapshot,
        outcome: RunOutcome { status: EventStatus::Ok, score_bps: Some(9_000) },
    }];
    let index = build_utility_index(&runs, Some(6_000), ts(50_000)).unwrap();
    assert_golden("utility_index.json", &index);
}

#[tokio::test]
async fn golden_tiered_memory_read_output_shape() {
    let (store, records) = tiered_store();
    for record in &records {
        store.put(record).await.unwrap();
    }
    let driver = TieredMemoryDriver::new(
        RankPolicy { utility_weight: 4, over_fetch_percent: 200 },
        UtilityIndex { stamp: ts(50_000), entries: Default::default() },
    )
    .unwrap();
    let journal = Journal::new("run-golden", "t-golden", logical_clock());
    driver
        .assemble_section(
            &journal,
            &TieredMemorySource::Store(store),
            &tiered_query(),
            &ContextBudget::new(512),
            None,
        )
        .await
        .unwrap();
    assert_golden(
        "tiered_memory_read_output.json",
        &tiered_read_output(&journal.snapshot()),
    );
}

// ---------- the recall measurement (feeds the vector decision) ----------

/// The synthetic study published in `docs/benchmarks.md`: planted utility
/// signal over journaled evidence, then recall of the known-relevant set
/// under the zero-weight floor versus a utility-weighted rank. The numbers
/// in the doc are pinned by these assertions.
#[tokio::test]
async fn utility_rerank_beats_the_zero_weight_floor_on_recorded_evidence() {
    const DOMAINS: [&str; 3] = ["billing", "travel", "prefs"];
    const RELEVANT_PER_DOMAIN: u64 = 8;
    const STALE_PER_DOMAIN: u64 = 2;
    const PACKABLE: u32 = 6; // records the held-out section budget fits

    let store = Arc::new(InMemoryMemoryStore::new());
    // Uniform content, so every record costs the same estimate and the
    // budget arithmetic is exact; provenance keeps identities distinct.
    let content = || json!({"v": "x".repeat(48)});
    let mut relevant: Vec<MemoryRecord> = Vec::new();
    let mut stale: Vec<MemoryRecord> = Vec::new();
    let mut clock: i64 = 1_000;
    for domain in DOMAINS {
        for i in 0..RELEVANT_PER_DOMAIN {
            clock += 1;
            let record = semantic_record(
                "user-7",
                &format!("{domain}.fact.{i}"),
                0,
                0.7,
                clock,
                content(),
            )
            .with_tags([domain]);
            relevant.push(record);
        }
        for i in 0..STALE_PER_DOMAIN {
            clock += 1;
            let record = semantic_record(
                "user-7",
                &format!("{domain}.stale.{i}"),
                10,
                0.95,
                clock,
                content(),
            )
            .with_tags([domain]);
            stale.push(record);
        }
    }
    for record in relevant.iter().chain(stale.iter()) {
        store.put(record).await.unwrap();
    }

    // Historical evidence: each relevant record appears in 12 successful
    // graded runs and one failed run; each stale record in 10 failed runs
    // and one Ok run graded below the 6000-bps success bar.
    let mut runs: Vec<(JournalSnapshot, RunOutcome)> = Vec::new();
    let mut planned: Vec<(String, RunOutcome)> = Vec::new();
    for record in &relevant {
        let key = record.key.clone().unwrap();
        for _ in 0..12 {
            planned.push((key.clone(), RunOutcome { status: EventStatus::Ok, score_bps: Some(8_000) }));
        }
        planned.push((key.clone(), RunOutcome { status: EventStatus::Error, score_bps: None }));
    }
    for record in &stale {
        let key = record.key.clone().unwrap();
        for _ in 0..10 {
            planned.push((key.clone(), RunOutcome { status: EventStatus::Error, score_bps: None }));
        }
        planned.push((key.clone(), RunOutcome { status: EventStatus::Ok, score_bps: Some(5_500) }));
    }
    for (n, (key, outcome)) in planned.into_iter().enumerate() {
        // Each synthetic run journals one read of its key.
        let snapshot =
            journaled_read_run(&store, &format!("hist-{n}"), &key, 100_000 + n as i64).await;
        runs.push((snapshot, outcome));
    }
    let utility_runs: Vec<UtilityRun> = runs
        .iter()
        .map(|(snapshot, outcome)| UtilityRun { snapshot, outcome: *outcome })
        .collect();
    let stamp = ts(500_000);
    let index = build_utility_index(&utility_runs, Some(6_000), stamp).unwrap();

    // The planted signal, read back: relevant 8666 bps, stale 769 bps.
    let relevant_bps = index.success_bps(&relevant[0].memory_id);
    let stale_bps = index.success_bps(&stale[0].memory_id);
    assert_eq!(relevant_bps, 13 * 10_000 / 15);
    assert_eq!(stale_bps, 10_000 / 13);

    let cost = estimated_tokens(relevant[0].content_bytes(), 20);
    let budget = ContextBudget::new(PACKABLE * cost);
    let floor = TieredMemoryDriver::floor(stamp);
    let weighted = TieredMemoryDriver::new(
        RankPolicy { utility_weight: 4, over_fetch_percent: 200 },
        index.clone(),
    )
    .unwrap();

    let mut floor_hits = 0usize;
    let mut weighted_hits = 0usize;
    let mut floor_used = 0u32;
    let mut weighted_used = 0u32;
    for (d, domain) in DOMAINS.iter().enumerate() {
        let query = MemoryQuery {
            scope: Some(ScopeAddress::new(MemoryScope::User, "user-7")),
            tags: vec![domain.to_string()],
            as_of: Some(ts(600_000)),
            ..MemoryQuery::default()
        };
        let relevant_ids: std::collections::BTreeSet<&str> = relevant
            [d * RELEVANT_PER_DOMAIN as usize..(d + 1) * RELEVANT_PER_DOMAIN as usize]
            .iter()
            .map(|r| r.memory_id.as_str())
            .collect();
        for (driver, hits, used) in [
            (&floor, &mut floor_hits, &mut floor_used),
            (&weighted, &mut weighted_hits, &mut weighted_used),
        ] {
            let journal = Journal::new(
                format!("held-out-{domain}-{}", driver.rank().utility_weight),
                "t-recall",
                logical_clock(),
            );
            let section = driver
                .assemble_section(
                    &journal,
                    &TieredMemorySource::Store(store.clone()),
                    &query,
                    &budget,
                    None,
                )
                .await
                .unwrap();
            *hits += section
                .memory_ids()
                .iter()
                .filter(|id| relevant_ids.contains(id.as_str()))
                .count();
            *used += section.token_accounting.used_tokens;
        }
    }
    let total_relevant = (RELEVANT_PER_DOMAIN as usize) * DOMAINS.len();
    let floor_recall_bps = floor_hits * 10_000 / total_relevant;
    let weighted_recall_bps = weighted_hits * 10_000 / total_relevant;
    println!(
        "recall measurement: relevant signal {relevant_bps} bps vs stale {stale_bps} bps; \
         floor recall {floor_hits}/{total_relevant} ({floor_recall_bps} bps), \
         weighted recall {weighted_hits}/{total_relevant} ({weighted_recall_bps} bps); \
         tokens used: floor {floor_used}, weighted {weighted_used}"
    );

    // The pinned measurement (docs/benchmarks.md carries these numbers):
    // the floor packs the two high-priority stale records plus four
    // relevant ones per domain; the weighted re-rank packs six relevant.
    assert_eq!(floor_hits, 4 * DOMAINS.len());
    assert_eq!(weighted_hits, 6 * DOMAINS.len());
    assert_eq!(floor_recall_bps, 5_000);
    assert_eq!(weighted_recall_bps, 7_500);
    assert!(
        weighted_recall_bps > floor_recall_bps,
        "utility re-ranking must beat the zero-weight floor"
    );
    assert_eq!(floor_used, weighted_used, "non-inferior cost: same budget spent");
}
