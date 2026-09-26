//! Consolidation gating and high-water mark tests (EP-06-S08 partial).
//!
//! Four test groups:
//!
//! - **Frequency gating** — turn-count threshold, interval threshold,
//!   both thresholds, neither threshold.
//! - **High-water mark lifecycle** — fresh state, advance, persistence
//!   round-trip through `InMemoryMemoryStore`.
//! - **Recall-loop prevention** — exclusion by run id, exclusion by tag,
//!   pass-through for unrelated records.
//! - **Edge cases** — zero min_turns, missing interval, clock rewind.

use chrono::{DateTime, Utc};
use serde_json::json;

use rusty_agent_runtime::memory::{
    exclude_recall_injected, frequency_gate, frequency_gate_reason, load_consolidation_state,
    persist_consolidation_state, ConsolidationCadence, ConsolidationState, InMemoryMemoryStore,
    MemoryEvidence, MemoryKind, MemoryProvenance, MemoryRecord, MemoryScope, MemoryStore,
    ProvenanceAuthor, ScopeAddress, ValidityWindow, CONSOLIDATION_STATE_KEY,
};

fn ts(millis: i64) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp_millis(millis).unwrap()
}

fn provenance() -> MemoryProvenance {
    MemoryProvenance {
        author: ProvenanceAuthor::System,
        evidence: MemoryEvidence::default(),
        written_at: ts(1_000),
    }
}

fn state(turns: u64, last_run_at: DateTime<Utc>) -> ConsolidationState {
    ConsolidationState {
        high_water_mark: 0,
        last_run_at,
        turns_since_last: turns,
    }
}

// ---------- frequency gating ----------

#[test]
fn gate_passes_when_turns_above_threshold() {
    let s = state(10, ts(0));
    let c = ConsolidationCadence {
        min_turns: 5,
        max_interval_ms: None,
    };
    assert!(frequency_gate(&s, &c, ts(1_000)).unwrap());
}

#[test]
fn gate_blocks_when_turns_below_threshold() {
    let s = state(3, ts(0));
    let c = ConsolidationCadence {
        min_turns: 5,
        max_interval_ms: None,
    };
    assert!(!frequency_gate(&s, &c, ts(1_000)).unwrap());
}

#[test]
fn gate_passes_when_interval_elapsed() {
    let s = state(0, ts(0));
    let c = ConsolidationCadence {
        min_turns: 0,
        max_interval_ms: Some(1_000),
    };
    assert!(frequency_gate(&s, &c, ts(1_001)).unwrap());
}

#[test]
fn gate_blocks_when_interval_not_elapsed() {
    let s = state(0, ts(0));
    let c = ConsolidationCadence {
        min_turns: 0,
        max_interval_ms: Some(10_000),
    };
    assert!(!frequency_gate(&s, &c, ts(1_000)).unwrap());
}

#[test]
fn gate_passes_when_both_thresholds_met() {
    let s = state(10, ts(0));
    let c = ConsolidationCadence {
        min_turns: 5,
        max_interval_ms: Some(1_000),
    };
    assert!(frequency_gate(&s, &c, ts(2_000)).unwrap());
}

#[test]
fn gate_blocks_when_turns_ok_but_interval_not() {
    let s = state(10, ts(0));
    let c = ConsolidationCadence {
        min_turns: 5,
        max_interval_ms: Some(10_000),
    };
    assert!(!frequency_gate(&s, &c, ts(1_000)).unwrap());
}

#[test]
fn gate_reason_includes_turns_and_interval() {
    let s = state(3, ts(0));
    let c = ConsolidationCadence {
        min_turns: 5,
        max_interval_ms: Some(10_000),
    };
    let reason = frequency_gate_reason(&s, &c, ts(1_000));
    assert!(reason.contains("turns 3 < 5"), "reason: {reason}");
    assert!(
        reason.contains("elapsed 1000 ms < 10000 ms"),
        "reason: {reason}"
    );
}

#[test]
fn gate_reason_says_pass_when_both_ok() {
    let s = state(10, ts(0));
    let c = ConsolidationCadence {
        min_turns: 5,
        max_interval_ms: Some(1_000),
    };
    let reason = frequency_gate_reason(&s, &c, ts(2_000));
    assert!(reason.contains("would pass"), "reason: {reason}");
}

// ---------- high-water mark lifecycle ----------

#[test]
fn fresh_state_starts_at_zero() {
    let now = ts(5_000);
    let s = ConsolidationState::new(now);
    assert_eq!(s.high_water_mark, 0);
    assert_eq!(s.turns_since_last, 0);
    assert_eq!(s.last_run_at, now);
}

#[test]
fn advance_bumps_mark_and_resets_counter() {
    let mut s = ConsolidationState::new(ts(0));
    s.turns_since_last = 42;
    s.advance(100, ts(10_000));
    assert_eq!(s.high_water_mark, 100);
    assert_eq!(s.turns_since_last, 0);
    assert_eq!(s.last_run_at, ts(10_000));
}

#[tokio::test]
async fn state_round_trips_through_store() {
    let store = InMemoryMemoryStore::new();
    let scope = ScopeAddress::new(MemoryScope::Agent, "agent-7");
    let now = ts(5_000);

    // Store a state.
    let mut state = ConsolidationState::new(now);
    state.turns_since_last = 23;
    state.high_water_mark = 47;
    let provenance = MemoryProvenance {
        author: ProvenanceAuthor::System,
        evidence: MemoryEvidence::default(),
        written_at: now,
    };
    let id = persist_consolidation_state(&store, &scope, &state, provenance.clone())
        .await
        .unwrap();
    assert!(!id.is_empty());

    // Load it back.
    let loaded = load_consolidation_state(&store, &scope, now).await.unwrap();
    assert_eq!(loaded.high_water_mark, 47);
    assert_eq!(loaded.turns_since_last, 23);
    assert_eq!(loaded.last_run_at, now);
}

#[tokio::test]
async fn missing_state_returns_fresh() {
    let store = InMemoryMemoryStore::new();
    let scope = ScopeAddress::new(MemoryScope::Agent, "agent-99");
    let now = ts(7_000);

    let loaded = load_consolidation_state(&store, &scope, now).await.unwrap();
    assert_eq!(loaded.high_water_mark, 0);
    assert_eq!(loaded.turns_since_last, 0);
    assert_eq!(loaded.last_run_at, now);
}

#[tokio::test]
async fn state_record_uses_well_known_key() {
    let store = InMemoryMemoryStore::new();
    let scope = ScopeAddress::new(MemoryScope::Agent, "agent-3");
    let now = ts(3_000);

    let state = ConsolidationState::new(now);
    let provenance = MemoryProvenance {
        author: ProvenanceAuthor::System,
        evidence: MemoryEvidence::default(),
        written_at: now,
    };
    persist_consolidation_state(&store, &scope, &state, provenance)
        .await
        .unwrap();

    let all = store.all().await.unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].key.as_deref(), Some(CONSOLIDATION_STATE_KEY));
}

// ---------- recall-loop prevention ----------

#[test]
fn excludes_record_from_injected_run() {
    let mut record = MemoryRecord::new(
        MemoryKind::Fact,
        ScopeAddress::new(MemoryScope::Agent, "a"),
        provenance(),
        1.0,
        ValidityWindow::starting(ts(0)),
        ts(0),
        json!("fact-a"),
    )
    .unwrap();
    record.provenance.evidence.run_id = Some("run-injected".into());

    let filtered = exclude_recall_injected(&[record], &["run-injected".into()]);
    assert!(filtered.is_empty());
}

#[test]
fn excludes_record_with_recall_injected_tag() {
    let mut record = MemoryRecord::new(
        MemoryKind::Fact,
        ScopeAddress::new(MemoryScope::Agent, "a"),
        provenance(),
        1.0,
        ValidityWindow::starting(ts(0)),
        ts(0),
        json!("fact-b"),
    )
    .unwrap();
    record = record.with_tags(["recall_injected"]);

    let filtered = exclude_recall_injected(&[record], &[]);
    assert!(filtered.is_empty());
}

#[test]
fn passes_through_unrelated_records() {
    let record = MemoryRecord::new(
        MemoryKind::Fact,
        ScopeAddress::new(MemoryScope::Agent, "a"),
        provenance(),
        1.0,
        ValidityWindow::starting(ts(0)),
        ts(0),
        json!("fact-c"),
    )
    .unwrap();

    let filtered = exclude_recall_injected(&[record], &["run-x".into()]);
    assert_eq!(filtered.len(), 1);
}

#[test]
fn mixed_slice_filters_correctly() {
    let mut injected = MemoryRecord::new(
        MemoryKind::Fact,
        ScopeAddress::new(MemoryScope::Agent, "a"),
        provenance(),
        1.0,
        ValidityWindow::starting(ts(0)),
        ts(0),
        json!("injected"),
    )
    .unwrap();
    injected.provenance.evidence.run_id = Some("run-1".into());

    let normal = MemoryRecord::new(
        MemoryKind::Fact,
        ScopeAddress::new(MemoryScope::Agent, "a"),
        provenance(),
        1.0,
        ValidityWindow::starting(ts(0)),
        ts(0),
        json!("normal"),
    )
    .unwrap();

    let filtered = exclude_recall_injected(&[injected, normal], &["run-1".into()]);
    assert_eq!(filtered.len(), 1);
    assert_eq!(
        filtered[0].content,
        rusty_agent_runtime::record::PayloadRef::Inline(json!("normal"))
    );
}

// ---------- edge cases ----------

#[test]
fn zero_min_turns_always_passes_if_interval_met() {
    let s = state(0, ts(0));
    let c = ConsolidationCadence {
        min_turns: 0,
        max_interval_ms: Some(1),
    };
    assert!(frequency_gate(&s, &c, ts(2)).unwrap());
}

#[test]
fn gate_ignores_interval_when_unset() {
    let s = state(3, ts(0));
    let c = ConsolidationCadence {
        min_turns: 3,
        max_interval_ms: None,
    };
    assert!(frequency_gate(&s, &c, ts(1)).unwrap());
}
