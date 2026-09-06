//! InteractionStore conformance (EP-07-S05): the contract every
//! interaction-event store must keep, exercised against the gap ledger's
//! event half — the store of record the ingestion route files into.
//!
//! The three properties under test:
//!
//! - **Append-only**: an event's fields never change after recording;
//!   re-recording the same source row converges on the same id, and a
//!   colliding id with different content is a typed error, never an
//!   overwrite.
//! - **Intent reassignment is history**: reassignment appends an
//!   `IntentAssignment` row; the current intent is the newest row; no
//!   other field of the event is ever touched.
//! - **Durability of the shape**: a snapshot round-trip preserves events
//!   and the full assignment history byte-for-byte.

use chrono::{TimeZone, Utc};
use serde_json::json;

use rusty_agent_runtime::gaps::{
    ActorRef, EventSource, GapError, GapLedger, InteractionChannel, InteractionEvent,
    InteractionOutcome, ResolutionPath,
};

fn at(secs: u32) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 5, 12, 0, secs).unwrap()
}

fn event(system: &str, stream: &str, record_id: &str, utterance: &str) -> InteractionEvent {
    InteractionEvent::new(
        EventSource {
            system: system.to_owned(),
            stream: stream.to_owned(),
            record_id: record_id.to_owned(),
        },
        ActorRef {
            role: "employee".to_owned(),
            id: "u-1".to_owned(),
        },
        InteractionChannel::Incident,
        utterance.to_owned(),
        ResolutionPath::HumanResolved,
        InteractionOutcome::Resolved,
        at(0),
        None,
        Vec::new(),
    )
    .expect("the fixture event is valid")
}

#[test]
fn recording_is_append_only_and_converges_on_source_identity() {
    let mut ledger = GapLedger::new();
    let first = event("servicenow", "incident", "INC001", "vpn is down");
    let id = ledger.record_event(first.clone()).unwrap();

    // A second ingestion run over the same source window re-derives the
    // same content address and converges — no duplicate row.
    let rerun = event("servicenow", "incident", "INC001", "vpn is down");
    assert_eq!(ledger.record_event(rerun).unwrap(), id);
    assert_eq!(ledger.events().count(), 1);

    // A colliding id with different content is a typed error, never an
    // overwrite: the stored row still says what it first said.
    let mut tampered = event("servicenow", "incident", "INC001", "vpn is down");
    tampered.utterance = "rewritten after the fact".to_owned();
    tampered.event_id = id.clone();
    assert!(matches!(
        ledger.record_event(tampered),
        Err(GapError::EventExists(ref colliding)) if *colliding == id
    ));
    assert_eq!(ledger.event(&id).unwrap().utterance, "vpn is down");
}

#[test]
fn intent_reassignment_appends_history_and_reflects_the_newest_row() {
    let mut ledger = GapLedger::new();
    let id = ledger
        .record_event(event("servicenow", "incident", "INC002", "laptop broken"))
        .unwrap();

    ledger
        .assign_intent(&id, "hardware", "induction", at(1))
        .unwrap();
    ledger
        .assign_intent(&id, "vpn-issues", "induction", at(2))
        .unwrap();

    // The current intent is the newest assignment; the earlier row is
    // preserved — reassignment is history, not mutation.
    assert_eq!(ledger.current_intent(&id), Some("vpn-issues"));

    // Reassignment touches nothing else about the event.
    assert_eq!(ledger.event(&id).unwrap().utterance, "laptop broken");

    // Reassigning an event that was never recorded is a typed error.
    assert!(matches!(
        ledger.assign_intent("ie-unknown", "x", "induction", at(3)),
        Err(GapError::UnknownEvent(ref missing)) if missing == "ie-unknown"
    ));
}

#[test]
fn snapshot_round_trip_preserves_events_and_assignment_history() {
    let mut ledger = GapLedger::new();
    let id = ledger
        .record_event(event(
            "servicenow",
            "incident",
            "INC003",
            "reset my password",
        ))
        .unwrap();
    ledger
        .assign_intent(&id, "account-access", "induction", at(1))
        .unwrap();
    ledger
        .assign_intent(&id, "password-reset", "induction", at(2))
        .unwrap();

    let snapshot = ledger.to_snapshot().unwrap();
    let restored = GapLedger::from_snapshot(snapshot).unwrap();

    assert_eq!(restored.events().count(), 1);
    assert_eq!(
        restored.event(&id).unwrap(),
        ledger.event(&id).unwrap(),
        "the event survives the round-trip byte-for-byte"
    );
    assert_eq!(restored.current_intent(&id), Some("password-reset"));

    // Reassignment continues to append on the restored store.
    let mut restored = restored;
    restored
        .assign_intent(&id, "iam", "induction", at(3))
        .unwrap();
    assert_eq!(restored.current_intent(&id), Some("iam"));
}

#[test]
fn the_failure_classes_survive_as_first_class_rows() {
    // The corpus's non-negotiable ingestion rule: zero-result searches,
    // no-click searches, reopened incidents, and escalations land as
    // their own ResolutionPath/InteractionOutcome values — the
    // conformance fixture proves no failure class is dropped or coerced.
    let mut ledger = GapLedger::new();
    for (record_id, path, outcome) in [
        (
            "SP1",
            ResolutionPath::Unresolved,
            InteractionOutcome::NoResult,
        ),
        (
            "SP2",
            ResolutionPath::Unresolved,
            InteractionOutcome::NoClick,
        ),
        (
            "INC1",
            ResolutionPath::HumanResolved,
            InteractionOutcome::Reopened,
        ),
        (
            "ESC1",
            ResolutionPath::HumanResolved,
            InteractionOutcome::Escalated,
        ),
    ] {
        let event = InteractionEvent::new(
            EventSource {
                system: "servicenow".to_owned(),
                stream: "fixture".to_owned(),
                record_id: record_id.to_owned(),
            },
            ActorRef {
                role: "employee".to_owned(),
                id: "u-1".to_owned(),
            },
            InteractionChannel::PortalSearch,
            format!("utterance for {record_id}"),
            path,
            outcome,
            at(0),
            None,
            Vec::new(),
        )
        .unwrap();
        let id = ledger.record_event(event).unwrap();
        let stored = ledger.event(&id).unwrap();
        assert_eq!(stored.resolution_path, path);
        assert_eq!(stored.outcome, outcome);
    }
    assert_eq!(ledger.events().count(), 4);
}

#[test]
fn wire_shape_keeps_the_schema_names() {
    // The append-only contract on the wire: the event carries its
    // content address, and the assignment is its own record type.
    let event = event("servicenow", "incident", "INC004", "printer on 3F");
    let value = serde_json::to_value(&event).unwrap();
    assert!(value["event_id"].as_str().unwrap().starts_with("ie-"));
    assert_eq!(value["resolution_path"], json!("human_resolved"));
    assert_eq!(value["outcome"], json!("resolved"));
}
