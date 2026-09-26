//! Integration tests for `rustyness verify-log` (EP-13-S10 AC 2 & AC 5).
//!
//! Fixtures are built in-memory and fed through [`verify_log`] directly,
//! avoiding the need for a running server.

use chrono::Utc;
use rusty_agent_runtime::journal::{
    ArtifactStore, Clock, EventDraft, FileArtifactStore, Journal, JournalSnapshot,
};
use rusty_agent_runtime::record::{
    ArtifactRef, Effect, EventStatus, PayloadRef, RunEvent, RunEventKind,
};
use rusty_agent_server::verify::{verify_log, verify_log_with_store, IntegrityFinding};

fn build_journal(events: &[(RunEventKind, Effect)]) -> JournalSnapshot {
    let journal = Journal::new("run-1", "thread-1", Clock::System);
    for (kind, effect) in events {
        journal.record(EventDraft::new(*kind, *effect));
    }
    journal.snapshot()
}

fn make_event(seq: u64, kind: RunEventKind) -> RunEvent {
    RunEvent {
        id: format!("run-1:{seq}"),
        run_id: "run-1".to_string(),
        thread_id: "thread-1".to_string(),
        node_id: None,
        seq,
        kind,
        effect: Effect::Pure,
        input: None,
        output: None,
        latency_ms: None,
        tokens: None,
        cost_usd: None,
        status: EventStatus::Ok,
        parent: None,
        recorded_at: Utc::now(),
    }
}

#[test]
fn valid_journal_passes() {
    let snapshot = build_journal(&[
        (RunEventKind::SuperStepStart, Effect::Pure),
        (RunEventKind::NodeInput, Effect::Pure),
        (RunEventKind::NodeOutput, Effect::Pure),
        (RunEventKind::SuperStepEnd, Effect::Pure),
    ]);
    let report = verify_log(snapshot);
    assert!(report.passed, "expected pass, got: {:?}", report.findings);
    assert_eq!(report.event_count, 4);
}

#[test]
fn missing_position_detects_gap() {
    let mut snapshot = build_journal(&[
        (RunEventKind::SuperStepStart, Effect::Pure),
        (RunEventKind::SuperStepEnd, Effect::Pure),
    ]);
    // Inject a third event with a gap in seq.
    snapshot
        .events
        .push(make_event(5, RunEventKind::SuperStepStart));
    // Recompute head hash so integrity passes, leaving only the seq gap.
    snapshot.head_hash = rusty_agent_runtime::journal::recompute_head_hash(&snapshot.events)
        .expect("recompute head hash");

    let report = verify_log(snapshot);
    assert!(!report.passed);
    assert!(
        report.findings.iter().any(|f| matches!(
            f,
            IntegrityFinding::MissingPosition {
                index: 2,
                expected: 2,
                found: 5
            }
        )),
        "expected MissingPosition at index 2, got: {:?}",
        report.findings
    );
}

#[test]
fn unpaired_turn_detects_open_super_step() {
    let snapshot = build_journal(&[
        (RunEventKind::SuperStepStart, Effect::Pure),
        (RunEventKind::NodeInput, Effect::Pure),
        (RunEventKind::NodeOutput, Effect::Pure),
        // Missing SuperStepEnd
    ]);
    let report = verify_log(snapshot);
    assert!(!report.passed);
    assert!(
        report.findings.iter().any(|f| matches!(
            f,
            IntegrityFinding::UnpairedTurn { open_kind, .. } if open_kind == "SuperStepStart"
        )),
        "expected UnpairedTurn for SuperStepStart, got: {:?}",
        report.findings
    );
}

#[test]
fn unpaired_turn_detects_open_node_input() {
    let snapshot = build_journal(&[
        (RunEventKind::SuperStepStart, Effect::Pure),
        (RunEventKind::NodeInput, Effect::Pure),
        (RunEventKind::SuperStepEnd, Effect::Pure),
        // Missing NodeOutput
    ]);
    let report = verify_log(snapshot);
    assert!(!report.passed);
    assert!(
        report.findings.iter().any(|f| matches!(
            f,
            IntegrityFinding::UnpairedTurn { open_kind, .. } if open_kind == "NodeInput"
        )),
        "expected UnpairedTurn for NodeInput, got: {:?}",
        report.findings
    );
}

#[test]
fn orphan_close_detects_unmatched_super_step_end() {
    let snapshot = build_journal(&[(RunEventKind::SuperStepEnd, Effect::Pure)]);
    let report = verify_log(snapshot);
    assert!(!report.passed);
    assert!(
        report.findings.iter().any(|f| matches!(
            f,
            IntegrityFinding::UnpairedTurn { open_kind, .. } if open_kind == "SuperStepEnd (orphan close)"
        )),
        "expected orphan close finding, got: {:?}",
        report.findings
    );
}

#[test]
fn integrity_failure_on_corrupted_head_hash() {
    let mut snapshot = build_journal(&[
        (RunEventKind::SuperStepStart, Effect::Pure),
        (RunEventKind::SuperStepEnd, Effect::Pure),
    ]);
    // Corrupt the head hash.
    snapshot.head_hash =
        "0000000000000000000000000000000000000000000000000000000000000000".to_string();
    let report = verify_log(snapshot);
    assert!(!report.passed);
    assert!(
        report
            .findings
            .iter()
            .any(|f| matches!(f, IntegrityFinding::IntegrityFailure { .. })),
        "expected IntegrityFailure, got: {:?}",
        report.findings
    );
}

// ------------------------------------------------------------------
// Artifact verification (EP-13-S10 AC 5)
// ------------------------------------------------------------------

#[tokio::test]
async fn artifact_in_snapshot_passes() {
    let mut snapshot = build_journal(&[
        (RunEventKind::SuperStepStart, Effect::Pure),
        (RunEventKind::NodeInput, Effect::Pure),
        (RunEventKind::NodeOutput, Effect::Pure),
        (RunEventKind::SuperStepEnd, Effect::Pure),
    ]);
    // Replace the NodeOutput event (index 2) with one carrying an artifact.
    // The artifact lives in the snapshot's embedded map.
    snapshot.events[2].output = Some(PayloadRef::Artifact(ArtifactRef {
        sha256: "abc123".to_string(),
        bytes: 42,
    }));
    snapshot
        .artifacts
        .insert("abc123".to_string(), serde_json::json!({"data": "hello"}));
    snapshot.head_hash = rusty_agent_runtime::journal::recompute_head_hash(&snapshot.events)
        .expect("recompute head hash");

    let tmp = std::env::temp_dir().join(format!("rusty-verify-test-{}", uuid::Uuid::new_v4()));
    let store = FileArtifactStore::new(&tmp);

    let report = verify_log_with_store(snapshot, Some(&store)).await;
    assert!(
        report.passed,
        "expected pass for embedded artifact, got: {:?}",
        report.findings
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test]
async fn artifact_in_external_store_passes() {
    let mut snapshot = build_journal(&[
        (RunEventKind::SuperStepStart, Effect::Pure),
        (RunEventKind::NodeInput, Effect::Pure),
        (RunEventKind::NodeOutput, Effect::Pure),
        (RunEventKind::SuperStepEnd, Effect::Pure),
    ]);

    let tmp = std::env::temp_dir().join(format!("rusty-verify-test-{}", uuid::Uuid::new_v4()));
    let store = FileArtifactStore::new(&tmp);
    // Seed the store with artifact bytes and use the computed hash.
    let aref = store.put(b"world").await.expect("seed artifact");

    // Replace the NodeOutput event with one referencing the store-resident artifact.
    snapshot.events[2].output = Some(PayloadRef::Artifact(aref));
    snapshot.head_hash = rusty_agent_runtime::journal::recompute_head_hash(&snapshot.events)
        .expect("recompute head hash");

    let report = verify_log_with_store(snapshot, Some(&store)).await;
    assert!(
        report.passed,
        "expected pass for store-resident artifact, got: {:?}",
        report.findings
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test]
async fn missing_artifact_reported_as_dangling_locator() {
    let mut snapshot = build_journal(&[
        (RunEventKind::SuperStepStart, Effect::Pure),
        (RunEventKind::NodeInput, Effect::Pure),
        (RunEventKind::NodeOutput, Effect::Pure),
        (RunEventKind::SuperStepEnd, Effect::Pure),
    ]);

    // Replace the NodeOutput with a reference to a non-existent artifact.
    snapshot.events[2].output = Some(PayloadRef::Artifact(ArtifactRef {
        sha256: "missing789".to_string(),
        bytes: 99,
    }));
    snapshot.head_hash = rusty_agent_runtime::journal::recompute_head_hash(&snapshot.events)
        .expect("recompute head hash");

    let tmp = std::env::temp_dir().join(format!("rusty-verify-test-{}", uuid::Uuid::new_v4()));
    let store = FileArtifactStore::new(&tmp);

    let report = verify_log_with_store(snapshot, Some(&store)).await;
    assert!(!report.passed);
    assert!(
        report.findings.iter().any(|f| matches!(
            f,
            IntegrityFinding::DanglingLocator { locator, event_id }
            if *locator == "missing789" && *event_id == "run-1:2"
        )),
        "expected DanglingLocator for missing artifact, got: {:?}",
        report.findings
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn sync_wrapper_skips_artifact_checks() {
    let mut snapshot = build_journal(&[
        (RunEventKind::SuperStepStart, Effect::Pure),
        (RunEventKind::NodeInput, Effect::Pure),
        (RunEventKind::NodeOutput, Effect::Pure),
        (RunEventKind::SuperStepEnd, Effect::Pure),
    ]);
    // Attach an artifact reference that is neither embedded nor in any store.
    snapshot.events[2].output = Some(PayloadRef::Artifact(ArtifactRef {
        sha256: "ghost000".to_string(),
        bytes: 1,
    }));
    snapshot.head_hash = rusty_agent_runtime::journal::recompute_head_hash(&snapshot.events)
        .expect("recompute head hash");

    // The synchronous verify_log does not take a store, so artifact checks
    // are skipped and the journal should pass.
    let report = verify_log(snapshot);
    assert!(
        report.passed,
        "sync verify_log should skip artifact checks, got: {:?}",
        report.findings
    );
}
