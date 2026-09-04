//! Integration tests for `rustyness verify-log` (EP-13-S10 AC 2).
//!
//! Fixtures are built in-memory and fed through [`verify_log`] directly,
//! avoiding the need for a running server.

use chrono::Utc;
use rusty_agent_runtime::journal::{Clock, EventDraft, Journal, JournalSnapshot};
use rusty_agent_runtime::record::{Effect, EventStatus, RunEvent, RunEventKind};
use rusty_agent_server::verify::{verify_log, IntegrityFinding};

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
