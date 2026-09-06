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

// ---------- untrusted-derived filing (EP-07-S09 AC5) ----------

#[test]
fn filings_citing_untrusted_events_land_as_untrusted_derived() {
    use rusty_agent_runtime::gaps::{ClosureCriteria, GapOrigin, OriginClass};

    let mut ledger = GapLedger::new();
    let untrusted = event("vendor-mail", "email", "EM-1", "vpn keeps dropping")
        .with_origin_class(OriginClass::Untrusted);
    let trusted = event("servicenow", "incident", "INC-1", "vpn keeps dropping");
    let untrusted_id = ledger.record_event(untrusted).unwrap();
    let trusted_id = ledger.record_event(trusted).unwrap();

    let criteria = || ClosureCriteria::BlockFilled {
        block_label: "vpn guidance".to_string(),
    };

    // Same surface, same statement shape — the content's trust class,
    // not the filing surface, decides the recorded origin.
    let from_untrusted = ledger
        .file_correction(
            &untrusted_id,
            "vendor doc was wrong",
            criteria(),
            0,
            "test",
            at(50),
        )
        .unwrap();
    let from_trusted = ledger
        .file_correction(
            &trusted_id,
            "vendor doc was wrong",
            criteria(),
            0,
            "test",
            at(51),
        )
        .unwrap();

    let untrusted_entry = ledger.entry(&from_untrusted).unwrap();
    assert_eq!(untrusted_entry.origin, GapOrigin::UntrustedDerived);
    let trusted_entry = ledger.entry(&from_trusted).unwrap();
    assert_eq!(trusted_entry.origin, GapOrigin::RuntimeCorrection);

    // Escalation filings follow the same rule.
    let escalated = ledger
        .file_escalation(
            &untrusted_id,
            "vendor doc escalated",
            criteria(),
            0,
            "test",
            at(52),
        )
        .unwrap();
    assert_eq!(
        ledger.entry(&escalated).unwrap().origin,
        GapOrigin::UntrustedDerived
    );
}

#[test]
fn origin_class_rides_outside_the_content_address() {
    use rusty_agent_runtime::gaps::OriginClass;

    let plain = event("servicenow", "incident", "INC-9", "vpn down");
    let marked = event("servicenow", "incident", "INC-9", "vpn down")
        .with_origin_class(OriginClass::Untrusted);
    assert_eq!(
        plain.event_id, marked.event_id,
        "trust classifies the evidence; it is not the evidence"
    );

    // Wire back-compat: a pre-EP-07-S09 event without the field
    // deserializes trusted, and a trusted event never grows the field.
    let wire = serde_json::to_value(&plain).unwrap();
    assert!(wire.get("origin_class").is_none());
    let decoded: InteractionEvent = serde_json::from_value(wire).unwrap();
    assert_eq!(decoded.origin_class, OriginClass::Trusted);

    let marked_wire = serde_json::to_value(&marked).unwrap();
    assert_eq!(marked_wire["origin_class"], json!("untrusted"));
}
