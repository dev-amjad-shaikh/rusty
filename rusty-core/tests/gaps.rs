//! Gap-ledger integration tests (demand-side learning loop).
//!
//! Four test groups:
//!
//! - **Golden files** — the serialized shapes of `InteractionEvent`,
//!   `GapLedgerEntry`, `GapMutation`, `ClosureCriteria`, and a whole-ledger
//!   snapshot are pinned against checked-in JSON under `tests/golden/`.
//!   Any accidental contract drift fails here. To bless an intentional
//!   contract change, re-run with `UPDATE_GOLDEN=1` and review the diff.
//! - **Filing and reinforcement** — schema validation (an entry without
//!   citations is invalid), content-address convergence (the same source
//!   row ingested twice is one event; the same ignorance filed twice is
//!   one entry), and the reopen rule for demand filed against a closed
//!   gap.
//! - **The status machine, probes, and the frontier** — legal and illegal
//!   transitions, the speculation gate (no hunting unvalidated guesses),
//!   probe promotion and parking, and expiry under the decay clock.
//! - **Closure, the behavioral signal, and rollback** — mechanical
//!   closure against typed criteria, per-intent failure-rate measurement
//!   driving reopens, and byte-exact state restoration through the
//!   mutation chain.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::Serialize;

use rusty_agent_runtime::gaps::{ActorRef, EventSource, GapError};
use rusty_agent_runtime::gaps::{
    AdjacencySource, Citation, CitationKind, ClosureCriteria, ClosureEvidence, GapLedger,
    GapLedgerEntry, GapMutationKind, GapOrigin, GapStatus, GapSubject, InteractionChannel,
    InteractionEvent, InteractionOutcome, JudgeVote, MAX_EMPTY_PROBES, OutcomeAnnotation,
    OutcomeClass, PROBE_BACKOFF_BASE_MILLIS, ResolutionPath,
};

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

fn source() -> EventSource {
    EventSource {
        system: "servicenow".into(),
        stream: "sp_log".into(),
        record_id: "SRL0042".into(),
    }
}

fn actor() -> ActorRef {
    ActorRef {
        role: "employee".into(),
        id: "u-7f3a".into(),
    }
}

fn search_event() -> InteractionEvent {
    InteractionEvent::new(
        source(),
        actor(),
        InteractionChannel::PortalSearch,
        "odyssey login loop after password change",
        ResolutionPath::Abandoned,
        InteractionOutcome::NoResult,
        ts(1_700_000_000_000),
        None,
        vec![],
    )
    .unwrap()
}

fn incident_event() -> InteractionEvent {
    InteractionEvent::new(
        EventSource {
            system: "servicenow".into(),
            stream: "incident".into(),
            record_id: "INC0010001".into(),
        },
        actor(),
        InteractionChannel::Incident,
        "VPN won't connect from home",
        ResolutionPath::HumanResolved,
        InteractionOutcome::Escalated,
        ts(1_700_000_100_000),
        Some(ts(1_700_000_200_000)),
        vec![],
    )
    .unwrap()
}

fn event_citation(event_id: &str) -> Citation {
    Citation::new(CitationKind::InteractionEvent, event_id, None).unwrap()
}

fn intent_subject() -> GapSubject {
    GapSubject::Intent {
        intent_id: "odyssey-login".into(),
    }
}

fn artifact_closure() -> ClosureCriteria {
    ClosureCriteria::ArtifactPromoted {
        candidate_id: "cand-odyssey-skill".into(),
    }
}

// ---------- golden files ----------

#[test]
fn golden_interaction_event() {
    assert_golden("gaps_interaction_event.json", &search_event());
}

#[test]
fn golden_gap_entry() {
    let mut ledger = GapLedger::new();
    let event_id = ledger.record_event(search_event()).unwrap();
    ledger
        .assign_intent(
            &event_id,
            "odyssey-login",
            "miner:induction-1",
            ts(1_700_000_300_000),
        )
        .unwrap();
    let gap_id = ledger
        .file_gap(
            intent_subject(),
            "No reliable guidance for Odyssey login loops after password change",
            vec![event_citation(&event_id)],
            GapOrigin::Induction,
            artifact_closure(),
            380,
            45_000,
            "induction",
            ts(1_700_000_400_000),
        )
        .unwrap();
    assert_golden("gaps_gap_entry.json", ledger.entry(&gap_id).unwrap());
}

#[test]
fn golden_gap_mutation() {
    let mut ledger = GapLedger::new();
    let event_id = ledger.record_event(search_event()).unwrap();
    let gap_id = ledger
        .file_gap(
            intent_subject(),
            "No reliable guidance for Odyssey login loops after password change",
            vec![event_citation(&event_id)],
            GapOrigin::Induction,
            artifact_closure(),
            380,
            45_000,
            "induction",
            ts(1_700_000_400_000),
        )
        .unwrap();
    let chain = ledger.chain(&gap_id).unwrap();
    assert_eq!(chain.len(), 1);
    assert_golden("gaps_gap_mutation.json", &chain[0]);
}

#[test]
fn golden_closure_criteria() {
    let criteria = vec![
        artifact_closure(),
        ClosureCriteria::BlockFilled {
            block_label: "vpn-clients".into(),
        },
        ClosureCriteria::FailureRateBelow {
            threshold_millis: 100,
        },
        ClosureCriteria::BusinessDecisionRequired,
    ];
    assert_golden("gaps_closure_criteria.json", &criteria);
}

#[test]
fn golden_ledger_snapshot() {
    let mut ledger = GapLedger::new();
    let event_id = ledger.record_event(search_event()).unwrap();
    ledger
        .assign_intent(
            &event_id,
            "odyssey-login",
            "miner:induction-1",
            ts(1_700_000_300_000),
        )
        .unwrap();
    ledger
        .file_gap(
            intent_subject(),
            "No reliable guidance for Odyssey login loops after password change",
            vec![event_citation(&event_id)],
            GapOrigin::Induction,
            artifact_closure(),
            380,
            45_000,
            "induction",
            ts(1_700_000_400_000),
        )
        .unwrap();
    assert_golden("gaps_ledger_snapshot.json", &ledger.to_snapshot().unwrap());
}

// ---------- events ----------

#[test]
fn event_ids_converge_for_the_same_source_row() {
    let mut ledger = GapLedger::new();
    let first = ledger.record_event(search_event()).unwrap();
    let second = ledger.record_event(search_event()).unwrap();
    assert_eq!(first, second, "re-ingesting one row must not double-count");
    assert!(first.starts_with("ie-"));
}

#[test]
fn event_id_collision_with_different_content_is_an_error() {
    let mut ledger = GapLedger::new();
    let event = search_event();
    let id = ledger.record_event(event.clone()).unwrap();
    let mut tampered = event;
    tampered.utterance = "a different utterance entirely".into();
    let result = ledger.record_event(tampered);
    assert!(matches!(result, Err(GapError::EventExists(colliding)) if colliding == id));
}

#[test]
fn intent_assignment_is_versioned() {
    let mut ledger = GapLedger::new();
    let event_id = ledger.record_event(search_event()).unwrap();
    assert_eq!(ledger.current_intent(&event_id), None);
    ledger
        .assign_intent(
            &event_id,
            "odyssey-login",
            "miner:pass-1",
            ts(1_700_000_300_000),
        )
        .unwrap();
    ledger
        .assign_intent(
            &event_id,
            "sso-auth-loops",
            "miner:pass-2",
            ts(1_700_000_500_000),
        )
        .unwrap();
    assert_eq!(ledger.current_intent(&event_id), Some("sso-auth-loops"));
}

#[test]
fn assigning_intent_to_an_unknown_event_fails() {
    let mut ledger = GapLedger::new();
    let result = ledger.assign_intent("ie-missing", "x", "miner", ts(0));
    assert!(matches!(result, Err(GapError::UnknownEvent(_))));
}

// ---------- filing and reinforcement ----------

#[test]
fn filing_without_evidence_is_invalid_by_schema() {
    let mut ledger = GapLedger::new();
    let result = ledger.file_gap(
        intent_subject(),
        "A gap with nothing behind it",
        vec![],
        GapOrigin::Operator,
        artifact_closure(),
        1,
        1,
        "operator:amjad",
        ts(0),
    );
    assert!(matches!(result, Err(GapError::EmptyEvidence)));
}

#[test]
fn filing_an_empty_statement_is_invalid() {
    let mut ledger = GapLedger::new();
    let result = ledger.file_gap(
        intent_subject(),
        "   ",
        vec![event_citation("ie-x")],
        GapOrigin::Operator,
        artifact_closure(),
        1,
        1,
        "operator:amjad",
        ts(0),
    );
    assert!(matches!(result, Err(GapError::EmptyField("statement"))));
}

#[test]
fn repeat_filings_reinforce_one_entry() {
    let mut ledger = GapLedger::new();
    let event_id = ledger.record_event(search_event()).unwrap();
    let mut file = |volume: u64, at: i64| {
        ledger
            .file_gap(
                intent_subject(),
                "No reliable guidance for Odyssey login loops after password change",
                vec![event_citation(&event_id)],
                GapOrigin::Induction,
                artifact_closure(),
                volume,
                1_000,
                "induction",
                ts(at),
            )
            .unwrap()
    };
    let first = file(100, 1_000);
    let second = file(50, 2_000);
    assert_eq!(first, second, "the same ignorance converges on one entry");
    let entry = ledger.entry(&first).unwrap();
    assert_eq!(entry.volume, 150);
    assert_eq!(entry.failure_cost_millis, 2_000);
    assert_eq!(entry.status, GapStatus::Open);
    assert_eq!(
        ledger.chain(&first).unwrap().len(),
        2,
        "filed + one reinforcement"
    );
}

#[test]
fn filing_against_a_closed_gap_reopens_it() {
    let mut ledger = GapLedger::new();
    let event_id = ledger.record_event(search_event()).unwrap();
    let gap_id = ledger
        .file_gap(
            intent_subject(),
            "No reliable guidance for Odyssey login loops after password change",
            vec![event_citation(&event_id)],
            GapOrigin::Induction,
            artifact_closure(),
            380,
            45_000,
            "induction",
            ts(1_000),
        )
        .unwrap();
    ledger
        .evaluate_closure(
            &gap_id,
            &ClosureEvidence::ArtifactPromoted {
                candidate_id: "cand-odyssey-skill".into(),
            },
            "gate",
            ts(2_000),
        )
        .unwrap();
    assert_eq!(ledger.entry(&gap_id).unwrap().status, GapStatus::Closed);

    let same = ledger
        .file_gap(
            intent_subject(),
            "No reliable guidance for Odyssey login loops after password change",
            vec![event_citation(&event_id)],
            GapOrigin::Induction,
            artifact_closure(),
            12,
            45_000,
            "runtime:escalation",
            ts(3_000),
        )
        .unwrap();
    assert_eq!(same, gap_id);
    let entry = ledger.entry(&gap_id).unwrap();
    assert_eq!(
        entry.status,
        GapStatus::Reopened,
        "the ledger never forgets a gap closed on paper but not in practice"
    );
    assert_eq!(entry.resolution, None);
    assert_eq!(entry.volume, 392);
}

#[test]
fn runtime_filing_helpers_resolve_subjects() {
    let mut ledger = GapLedger::new();
    let event_id = ledger.record_event(incident_event()).unwrap();
    // No intent assigned: the utterance becomes a question shape.
    let gap_id = ledger
        .file_escalation(
            &event_id,
            "No runbook for home-VPN certificate failures",
            artifact_closure(),
            30_000,
            "runtime:escalation",
            ts(1_000),
        )
        .unwrap();
    let entry = ledger.entry(&gap_id).unwrap();
    assert!(matches!(entry.subject, GapSubject::QuestionShape { .. }));
    assert_eq!(entry.origin, GapOrigin::RuntimeEscalation);

    // Once clustering claims the event, filings land on the intent.
    ledger
        .assign_intent(&event_id, "vpn-remote-access", "miner:pass-1", ts(2_000))
        .unwrap();
    let gap_id = ledger
        .file_correction(
            &event_id,
            "VPN guidance contradicts the current client",
            artifact_closure(),
            30_000,
            "runtime:correction",
            ts(3_000),
        )
        .unwrap();
    assert!(matches!(
        ledger.entry(&gap_id).unwrap().subject,
        GapSubject::Intent { .. }
    ));
    assert_eq!(
        ledger.entry(&gap_id).unwrap().origin,
        GapOrigin::RuntimeCorrection
    );

    // Zero recall files a question-shaped gap with the query as citation.
    let gap_id = ledger
        .file_zero_recall(
            "How do I expense a docking station?",
            artifact_closure(),
            5_000,
            "runtime:zero-recall",
            ts(4_000),
        )
        .unwrap();
    let entry = ledger.entry(&gap_id).unwrap();
    assert_eq!(entry.origin, GapOrigin::ZeroRecall);
    assert!(matches!(entry.subject, GapSubject::QuestionShape { .. }));
}

#[test]
fn question_shapes_normalize_for_matching() {
    let a = GapSubject::question_shape("  The   VPN Question ").unwrap();
    let b = GapSubject::question_shape("the vpn question").unwrap();
    assert_eq!(a, b);
}

// ---------- the status machine ----------

fn filed_ledger() -> (GapLedger, String) {
    let mut ledger = GapLedger::new();
    let event_id = ledger.record_event(search_event()).unwrap();
    let gap_id = ledger
        .file_gap(
            intent_subject(),
            "No reliable guidance for Odyssey login loops after password change",
            vec![event_citation(&event_id)],
            GapOrigin::Induction,
            artifact_closure(),
            380,
            45_000,
            "induction",
            ts(1_000),
        )
        .unwrap();
    (ledger, gap_id)
}

#[test]
fn the_status_machine_admits_the_hunt_lifecycle() {
    let (mut ledger, gap_id) = filed_ledger();
    ledger
        .transition(&gap_id, GapStatus::Hunting, "hunter:1", ts(2_000))
        .unwrap();
    ledger
        .transition(&gap_id, GapStatus::TrialPending, "hunter:1", ts(3_000))
        .unwrap();
    ledger
        .evaluate_closure(
            &gap_id,
            &ClosureEvidence::ArtifactPromoted {
                candidate_id: "cand-odyssey-skill".into(),
            },
            "gate",
            ts(4_000),
        )
        .unwrap();
    let entry = ledger.entry(&gap_id).unwrap();
    assert_eq!(entry.status, GapStatus::Closed);
    assert_eq!(
        entry.resolution.as_deref(),
        Some("candidate:cand-odyssey-skill")
    );
}

#[test]
fn illegal_transitions_are_refused() {
    let (mut ledger, gap_id) = filed_ledger();
    // TrialPending without a hunt.
    let result = ledger.transition(&gap_id, GapStatus::TrialPending, "x", ts(2_000));
    assert!(matches!(
        result,
        Err(GapError::IllegalTransition {
            from: GapStatus::Open,
            to: GapStatus::TrialPending
        })
    ));
    // A bare transition to Closed would be closure without criteria.
    let result = ledger.transition(&gap_id, GapStatus::Closed, "x", ts(2_000));
    assert!(matches!(result, Err(GapError::IllegalTransition { .. })));
    // Parked is the speculative decay state; observed gaps never park.
    let result = ledger.transition(&gap_id, GapStatus::Parked, "x", ts(2_000));
    assert!(matches!(result, Err(GapError::NotSpeculative(_))));
    assert_eq!(ledger.entry(&gap_id).unwrap().status, GapStatus::Open);
}

#[test]
fn unknown_gaps_fail_typed() {
    let (mut ledger, _) = filed_ledger();
    let result = ledger.transition("gap-missing", GapStatus::Hunting, "x", ts(0));
    assert!(matches!(result, Err(GapError::UnknownGap(_))));
}

// ---------- the frontier ----------

fn speculative_ledger() -> (GapLedger, String) {
    let mut ledger = GapLedger::new();
    let edge = Citation::new(
        CitationKind::AdjacencyEdge,
        "cmdb:laptop-fleet:depends-on:ztna-service",
        Some("CMDB dependency edge".into()),
    )
    .unwrap();
    let gap_id = ledger
        .open_speculative(
            GapSubject::Intent {
                intent_id: "ztna-cert-errors".into(),
            },
            "ZTNA certificate error handling is unknown",
            AdjacencySource::Structural,
            edge,
            artifact_closure(),
            "frontier:expansion",
            ts(1_000),
        )
        .unwrap();
    (ledger, gap_id)
}

#[test]
fn speculation_cannot_hunt_unvalidated() {
    let (mut ledger, gap_id) = speculative_ledger();
    let result = ledger.transition(&gap_id, GapStatus::Hunting, "hunter:1", ts(2_000));
    assert!(matches!(result, Err(GapError::UnvalidatedSpeculation(_))));
    // And the work order never spends hunting budget on a guess.
    assert!(ledger.work_order().is_empty());
}

#[test]
fn a_demand_probe_validates_and_enqueues() {
    let (mut ledger, gap_id) = speculative_ledger();
    ledger
        .record_probe(&gap_id, 14, false, "probe", ts(2_000))
        .unwrap();
    let entry = ledger.entry(&gap_id).unwrap();
    assert!(entry.observed);
    assert_eq!(entry.volume, 14);
    assert_eq!(
        entry.evidence.len(),
        2,
        "the adjacency edge plus the probe result"
    );
    assert_eq!(
        ledger.work_order().len(),
        1,
        "validated speculation enters the standing work order"
    );
    ledger
        .transition(&gap_id, GapStatus::Hunting, "hunter:1", ts(3_000))
        .unwrap();
    assert!(
        ledger.work_order().is_empty(),
        "a gap under hunt leaves the queue"
    );
}

#[test]
fn an_empty_probe_parks_and_a_later_hit_revives() {
    let (mut ledger, gap_id) = speculative_ledger();
    ledger
        .record_probe(&gap_id, 0, false, "probe", ts(2_000))
        .unwrap();
    assert_eq!(ledger.entry(&gap_id).unwrap().status, GapStatus::Parked);
    assert_eq!(ledger.entry(&gap_id).unwrap().empty_probes, 1);

    ledger
        .record_probe(&gap_id, 7, false, "probe", ts(3_000))
        .unwrap();
    let entry = ledger.entry(&gap_id).unwrap();
    assert_eq!(entry.status, GapStatus::Open);
    assert!(entry.observed);
    assert_eq!(entry.empty_probes, 0);
}

#[test]
fn parked_entries_expire_under_the_decay_clock() {
    let (mut ledger, gap_id) = speculative_ledger();
    for probe in 0..MAX_EMPTY_PROBES {
        ledger
            .record_probe(&gap_id, 0, false, "probe", ts(2_000 + probe as i64 * 1_000))
            .unwrap();
    }
    assert_eq!(
        ledger.entry(&gap_id).unwrap().empty_probes,
        MAX_EMPTY_PROBES
    );
    let expired = ledger.expire_parked(ts(10_000), "decay-clock").unwrap();
    assert_eq!(expired, vec![gap_id.clone()]);
    let entry = ledger.entry(&gap_id).unwrap();
    assert_eq!(entry.status, GapStatus::Closed);
    assert_eq!(entry.resolution.as_deref(), Some("expired:no-demand"));
}

#[test]
fn expiry_respects_the_backoff_schedule() {
    let (mut ledger, gap_id) = speculative_ledger();
    ledger
        .record_probe(&gap_id, 0, false, "probe", ts(2_000))
        .unwrap();
    // One empty probe: the deadline is one base interval after the probe.
    let too_early = ts(2_000 + PROBE_BACKOFF_BASE_MILLIS - 1);
    assert!(
        ledger
            .expire_parked(too_early, "decay-clock")
            .unwrap()
            .is_empty()
    );
    let at_deadline = ts(2_000 + PROBE_BACKOFF_BASE_MILLIS);
    assert_eq!(
        ledger.expire_parked(at_deadline, "decay-clock").unwrap(),
        vec![gap_id]
    );
}

#[test]
fn probes_on_observed_gaps_are_refused() {
    let (mut ledger, gap_id) = filed_ledger();
    let result = ledger.record_probe(&gap_id, 1, false, "probe", ts(2_000));
    assert!(matches!(result, Err(GapError::ProbeOnObserved(_))));
}

// ---------- closure and the behavioral signal ----------

#[test]
fn closure_requires_matching_evidence() {
    let (mut ledger, gap_id) = filed_ledger();
    let result = ledger.evaluate_closure(
        &gap_id,
        &ClosureEvidence::ArtifactPromoted {
            candidate_id: "cand-something-else".into(),
        },
        "gate",
        ts(2_000),
    );
    assert!(matches!(result, Err(GapError::ClosureUnsatisfied { .. })));
    assert_eq!(ledger.entry(&gap_id).unwrap().status, GapStatus::Open);
}

#[test]
fn failure_rate_closure_reads_the_ledgers_own_tallies() {
    let mut ledger = GapLedger::new();
    let event_id = ledger.record_event(search_event()).unwrap();
    let gap_id = ledger
        .file_gap(
            intent_subject(),
            "No reliable guidance for Odyssey login loops after password change",
            vec![event_citation(&event_id)],
            GapOrigin::Induction,
            ClosureCriteria::FailureRateBelow {
                threshold_millis: 100,
            },
            380,
            45_000,
            "induction",
            ts(1_000),
        )
        .unwrap();
    // 9 accepted, 1 corrected: 100 per mille — at the threshold, not below.
    for _ in 0..9 {
        ledger.record_outcome("odyssey-login", OutcomeClass::Accepted);
    }
    ledger.record_outcome("odyssey-login", OutcomeClass::Corrected);
    let result = ledger.evaluate_closure(
        &gap_id,
        &ClosureEvidence::FailureRateMeasured,
        "gate",
        ts(2_000),
    );
    assert!(matches!(result, Err(GapError::ClosureUnsatisfied { .. })));
    // One more accepted: 91 per mille — below.
    ledger.record_outcome("odyssey-login", OutcomeClass::Accepted);
    ledger
        .evaluate_closure(
            &gap_id,
            &ClosureEvidence::FailureRateMeasured,
            "gate",
            ts(3_000),
        )
        .unwrap();
    assert_eq!(ledger.entry(&gap_id).unwrap().status, GapStatus::Closed);
}

#[test]
fn an_unmeasured_intent_cannot_close_on_failure_rate() {
    let mut ledger = GapLedger::new();
    let event_id = ledger.record_event(search_event()).unwrap();
    let gap_id = ledger
        .file_gap(
            intent_subject(),
            "No reliable guidance for Odyssey login loops after password change",
            vec![event_citation(&event_id)],
            GapOrigin::Induction,
            ClosureCriteria::FailureRateBelow {
                threshold_millis: 100,
            },
            380,
            45_000,
            "induction",
            ts(1_000),
        )
        .unwrap();
    let result = ledger.evaluate_closure(
        &gap_id,
        &ClosureEvidence::FailureRateMeasured,
        "gate",
        ts(2_000),
    );
    assert!(matches!(result, Err(GapError::ClosureUnsatisfied { .. })));
}

#[test]
fn a_business_decision_closes_a_blocked_gap() {
    let mut ledger = GapLedger::new();
    let event_id = ledger.record_event(search_event()).unwrap();
    let gap_id = ledger
        .file_gap(
            intent_subject(),
            "The catalog's stated laptop-refresh policy contradicts fulfillment",
            vec![event_citation(&event_id)],
            GapOrigin::Induction,
            ClosureCriteria::BusinessDecisionRequired,
            60,
            90_000,
            "induction",
            ts(1_000),
        )
        .unwrap();
    ledger
        .transition(&gap_id, GapStatus::BlockedOnBusiness, "hunter:1", ts(2_000))
        .unwrap();
    // Mechanical evidence cannot close it — the business must decide.
    let result = ledger.evaluate_closure(
        &gap_id,
        &ClosureEvidence::ArtifactPromoted {
            candidate_id: "cand-anything".into(),
        },
        "gate",
        ts(2_500),
    );
    assert!(matches!(result, Err(GapError::ClosureUnsatisfied { .. })));
    ledger
        .evaluate_closure(
            &gap_id,
            &ClosureEvidence::BusinessDecision {
                decision_ref: "kb-governance:2026-08-25:laptop-refresh".into(),
            },
            "operator:amjad",
            ts(3_000),
        )
        .unwrap();
    let entry = ledger.entry(&gap_id).unwrap();
    assert_eq!(entry.status, GapStatus::Closed);
    assert!(
        entry
            .resolution
            .as_deref()
            .unwrap()
            .starts_with("business-decision:")
    );
}

#[test]
fn the_behavioral_sweep_reopens_closed_gaps_whose_numbers_did_not_move() {
    let mut ledger = GapLedger::new();
    let event_id = ledger.record_event(search_event()).unwrap();
    let gap_id = ledger
        .file_gap(
            intent_subject(),
            "No reliable guidance for Odyssey login loops after password change",
            vec![event_citation(&event_id)],
            GapOrigin::Induction,
            artifact_closure(),
            380,
            45_000,
            "induction",
            ts(1_000),
        )
        .unwrap();
    ledger
        .evaluate_closure(
            &gap_id,
            &ClosureEvidence::ArtifactPromoted {
                candidate_id: "cand-odyssey-skill".into(),
            },
            "gate",
            ts(2_000),
        )
        .unwrap();
    // The promoted skill did not move the numbers: 310 per mille failure.
    for _ in 0..69 {
        ledger.record_outcome("odyssey-login", OutcomeClass::Accepted);
    }
    for _ in 0..20 {
        ledger.record_outcome("odyssey-login", OutcomeClass::Corrected);
    }
    for _ in 0..11 {
        ledger.record_outcome("odyssey-login", OutcomeClass::Redone);
    }
    assert_eq!(ledger.failure_rate_millis("odyssey-login"), Some(310));
    let reopened = ledger
        .sweep_reopens(300, "behavioral-signal", ts(3_000))
        .unwrap();
    assert_eq!(reopened, vec![gap_id.clone()]);
    assert_eq!(ledger.entry(&gap_id).unwrap().status, GapStatus::Reopened);
}

#[test]
fn corrected_and_redone_count_as_failure_accepted_does_not() {
    let mut ledger = GapLedger::new();
    ledger.record_outcome("i", OutcomeClass::Accepted);
    ledger.record_outcome("i", OutcomeClass::Accepted);
    ledger.record_outcome("i", OutcomeClass::Corrected);
    ledger.record_outcome("i", OutcomeClass::Redone);
    assert_eq!(ledger.failure_rate_millis("i"), Some(500));
    assert_eq!(ledger.failure_rate_millis("unscored"), None);
}

// ---------- the work order ----------

#[test]
fn the_work_order_ranks_actionable_gaps_by_priority() {
    let mut ledger = GapLedger::new();
    let event_id = ledger.record_event(search_event()).unwrap();
    let mut file = |intent: &str, volume: u64, cost: u64, at: i64| {
        ledger
            .file_gap(
                GapSubject::Intent {
                    intent_id: intent.into(),
                },
                format!("gap statement for {intent}"),
                vec![event_citation(&event_id)],
                GapOrigin::Induction,
                artifact_closure(),
                volume,
                cost,
                "induction",
                ts(at),
            )
            .unwrap()
    };
    let low = file("low-priority", 10, 1_000, 1_000);
    let high = file("high-priority", 380, 45_000, 2_000);
    let mid = file("mid-priority", 100, 10_000, 3_000);
    let order: Vec<&str> = ledger
        .work_order()
        .iter()
        .map(|entry| entry.gap_id.as_str())
        .collect();
    assert_eq!(order, vec![high.as_str(), mid.as_str(), low.as_str()]);

    // Hunting entries leave the queue.
    ledger
        .transition(&high, GapStatus::Hunting, "hunter:1", ts(4_000))
        .unwrap();
    let order: Vec<&str> = ledger
        .work_order()
        .iter()
        .map(|entry| entry.gap_id.as_str())
        .collect();
    assert_eq!(order, vec![mid.as_str(), low.as_str()]);
}

// ---------- rollback ----------

#[test]
fn rollback_restores_the_exact_prior_state() {
    let (mut ledger, gap_id) = filed_ledger();
    let after_filing: GapLedgerEntry = ledger.entry(&gap_id).unwrap().clone();

    ledger
        .file_gap(
            intent_subject(),
            "No reliable guidance for Odyssey login loops after password change",
            vec![event_citation("ie-second")],
            GapOrigin::Induction,
            artifact_closure(),
            50,
            1_000,
            "runtime:escalation",
            ts(2_000),
        )
        .unwrap();
    ledger
        .transition(&gap_id, GapStatus::Hunting, "hunter:1", ts(3_000))
        .unwrap();
    let filed_mutation = ledger.chain(&gap_id).unwrap()[0].mutation_id.clone();
    assert_ne!(
        serde_json::to_value(ledger.entry(&gap_id).unwrap()).unwrap(),
        serde_json::to_value(&after_filing).unwrap(),
        "reinforcement and the hunt changed the entry"
    );

    ledger
        .rollback(&gap_id, &filed_mutation, "operator:amjad", ts(4_000))
        .unwrap();
    assert_eq!(
        serde_json::to_value(ledger.entry(&gap_id).unwrap()).unwrap(),
        serde_json::to_value(&after_filing).unwrap(),
        "the restore is the state that was, not a reconstruction"
    );
    // The chain keeps the full history — rollback is additive, never
    // destructive.
    let chain = ledger.chain(&gap_id).unwrap();
    assert_eq!(chain.len(), 4);
    assert!(matches!(
        chain.last().unwrap().kind,
        GapMutationKind::RolledBack { .. }
    ));
}

#[test]
fn rollback_to_an_unknown_mutation_fails() {
    let (mut ledger, gap_id) = filed_ledger();
    let result = ledger.rollback(&gap_id, "gm-missing", "operator:amjad", ts(2_000));
    assert!(matches!(result, Err(GapError::UnknownMutation { .. })));
}

#[test]
fn mutation_chains_are_hash_linked() {
    let (mut ledger, gap_id) = filed_ledger();
    ledger
        .transition(&gap_id, GapStatus::Hunting, "hunter:1", ts(2_000))
        .unwrap();
    let chain = ledger.chain(&gap_id).unwrap();
    assert_eq!(chain[0].previous, None);
    assert_eq!(
        chain[1].previous.as_deref(),
        Some(chain[0].mutation_id.as_str())
    );
}

// ---------- snapshots ----------

#[test]
fn snapshots_roundtrip_byte_exact() {
    let (mut ledger, gap_id) = filed_ledger();
    ledger
        .transition(&gap_id, GapStatus::Hunting, "hunter:1", ts(2_000))
        .unwrap();
    ledger.record_outcome("odyssey-login", OutcomeClass::Corrected);
    let snapshot = ledger.to_snapshot().unwrap();
    let restored = GapLedger::from_snapshot(snapshot.clone()).unwrap();
    assert_eq!(restored.to_snapshot().unwrap(), snapshot);
    assert_eq!(
        restored.entry(&gap_id).unwrap().status,
        GapStatus::Hunting,
        "the projection rides the snapshot; the chain is the record"
    );
}

#[test]
fn unknown_snapshot_versions_fail_closed() {
    let ledger = GapLedger::new();
    let mut snapshot = ledger.to_snapshot().unwrap();
    snapshot["format_version"] = serde_json::json!(99);
    let result = GapLedger::from_snapshot(snapshot);
    assert!(matches!(result, Err(GapError::UnsupportedFormat(99))));
}

// ---------- evidence attachment (the hunting loop's documentation) ----------

#[test]
fn add_evidence_attaches_without_touching_demand_tallies() {
    let (mut ledger, gap_id) = filed_ledger();
    let before = ledger.entry(&gap_id).unwrap().clone();
    let citation = Citation::new(
        CitationKind::CoverageEdge,
        "deliverable:hunt-report-7",
        Some("the proposed fix contradicts the SSO rollout plan".into()),
    )
    .unwrap();
    ledger
        .add_evidence(&gap_id, vec![citation.clone()], "hunt:blocked", ts(2_000))
        .unwrap();
    let after = ledger.entry(&gap_id).unwrap();
    assert_eq!(after.volume, before.volume, "evidence adds no volume");
    assert_eq!(
        after.failure_cost_millis, before.failure_cost_millis,
        "evidence adds no failure cost"
    );
    assert!(
        after.evidence.contains(&citation),
        "the citation lands on the entry"
    );
    assert!(
        matches!(
            ledger.chain(&gap_id).unwrap().last().unwrap().kind,
            GapMutationKind::Reinforced {
                added_volume: 0,
                added_failure_cost_millis: 0,
                ..
            }
        ),
        "the chain records a zero-volume reinforcement"
    );
}

#[test]
fn add_evidence_validates_like_a_filing() {
    let (mut ledger, gap_id) = filed_ledger();
    let citation = Citation::new(CitationKind::CoverageEdge, "deliverable:x", None).unwrap();
    assert!(matches!(
        ledger.add_evidence(&gap_id, vec![], "hunt:blocked", ts(2_000)),
        Err(GapError::EmptyEvidence)
    ));
    assert!(matches!(
        ledger.add_evidence("gap-missing", vec![citation], "hunt:blocked", ts(2_000)),
        Err(GapError::UnknownGap(_))
    ));
}

#[test]
fn entries_scans_every_entry() {
    let (mut ledger, gap_id) = filed_ledger();
    let second = ledger
        .file_gap(
            GapSubject::question_shape("vpn-clients").unwrap(),
            "No answer for VPN client compatibility questions",
            vec![event_citation("evt-other")],
            GapOrigin::Induction,
            ClosureCriteria::BlockFilled {
                block_label: "vpn-clients".into(),
            },
            12,
            3_000,
            "induction",
            ts(1_500),
        )
        .unwrap();
    let ids: Vec<&str> = ledger
        .entries()
        .map(|entry| entry.gap_id.as_str())
        .collect();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&gap_id.as_str()));
    assert!(ids.contains(&second.as_str()));
}

// ---------- outcome annotations (the behavioral signal's evidence) ----------

/// Build judge votes from `(judge, verdict)` pairs.
fn votes(pairs: &[(&str, OutcomeClass)]) -> Vec<JudgeVote> {
    pairs
        .iter()
        .map(|(judge, vote)| JudgeVote {
            judge: (*judge).into(),
            vote: *vote,
        })
        .collect()
}

/// A scored-turn fixture against the odyssey intent.
fn annotation(turn_ref: &str, pairs: &[(&str, OutcomeClass)], millis: i64) -> OutcomeAnnotation {
    OutcomeAnnotation::from_votes(turn_ref, "odyssey-login", votes(pairs), ts(millis)).unwrap()
}

#[test]
fn golden_outcome_annotation() {
    let annotation = annotation(
        "session-9:turn-4",
        &[
            ("judge:gpt-4o", OutcomeClass::Corrected),
            ("judge:claude", OutcomeClass::Corrected),
            ("judge:heuristic", OutcomeClass::Accepted),
        ],
        1_700_001_000_000,
    );
    assert_golden("gaps_outcome_annotation.json", &annotation);
}

#[test]
fn the_majority_vote_decides_and_every_vote_is_recorded() {
    let annotation = annotation(
        "session-9:turn-4",
        &[
            ("judge:a", OutcomeClass::Corrected),
            ("judge:b", OutcomeClass::Corrected),
            ("judge:c", OutcomeClass::Accepted),
        ],
        1_000,
    );
    assert_eq!(annotation.outcome, OutcomeClass::Corrected);
    assert_eq!(annotation.judge_votes.len(), 3, "every sample is kept");
    assert!(annotation.annotation_id.starts_with("oa-"));

    let mut ledger = GapLedger::new();
    let recorded = ledger
        .record_annotation(annotation, "scorer:turns", ts(2_000))
        .unwrap();
    assert!(recorded.closed_gap_ids.is_empty());
    let tally = ledger.tally("odyssey-login").unwrap();
    assert_eq!(tally.corrected, 1);
    assert_eq!(tally.total(), 1);
}

#[test]
fn a_split_jury_abstains_neutral_and_dilutes_nothing() {
    let tied = annotation(
        "session-9:turn-5",
        &[
            ("judge:a", OutcomeClass::Accepted),
            ("judge:b", OutcomeClass::Corrected),
        ],
        1_000,
    );
    assert_eq!(
        tied.outcome,
        OutcomeClass::Neutral,
        "a tie for the lead is no verdict"
    );

    let mut ledger = GapLedger::new();
    ledger
        .record_annotation(tied, "scorer:turns", ts(2_000))
        .unwrap();
    let tally = ledger.tally("odyssey-login").unwrap();
    assert_eq!(tally.neutral, 1);
    assert_eq!(tally.total(), 0, "neutral turns are not decisive");
    assert_eq!(
        ledger.failure_rate_millis("odyssey-login"),
        None,
        "an intent with only neutral turns is unmeasured, not passing"
    );
}

#[test]
fn scoring_without_votes_fails() {
    let result =
        OutcomeAnnotation::from_votes("session-9:turn-1", "odyssey-login", vec![], ts(1_000));
    assert!(matches!(result, Err(GapError::EmptyVotes)));
}

#[test]
fn re_scoring_converges_and_collisions_fail() {
    let pairs = [
        ("judge:a", OutcomeClass::Accepted),
        ("judge:b", OutcomeClass::Corrected),
    ];
    let first = annotation("session-9:turn-4", &pairs, 1_000);
    // The same sample set in a different submission order addresses the
    // same annotation.
    let reordered = annotation("session-9:turn-4", &[pairs[1], pairs[0]], 1_000);
    assert_eq!(first.annotation_id, reordered.annotation_id);

    let mut ledger = GapLedger::new();
    ledger
        .record_annotation(first.clone(), "scorer:turns", ts(2_000))
        .unwrap();
    let again = ledger
        .record_annotation(reordered, "scorer:turns", ts(3_000))
        .unwrap();
    assert_eq!(again.annotation_id, first.annotation_id);
    assert_eq!(ledger.outcome_curve("odyssey-login").len(), 1);

    // A different score colliding on the id is a typed error — but the
    // address covers every defining field, so a genuinely different
    // score is a genuinely different id. Tamper instead: same id,
    // different content.
    let mut tampered = first.clone();
    tampered.outcome = OutcomeClass::Accepted;
    let result = ledger.record_annotation(tampered, "scorer:turns", ts(4_000));
    assert!(matches!(
        result,
        Err(GapError::AnnotationExists(id)) if id == first.annotation_id
    ));
}

#[test]
fn the_curve_is_per_intent_and_time_ordered() {
    let mut ledger = GapLedger::new();
    for (turn, millis) in [("t-3", 3_000), ("t-1", 1_000), ("t-2", 2_000)] {
        let scored = annotation(
            &format!("session-9:{turn}"),
            &[("judge:a", OutcomeClass::Accepted)],
            millis,
        );
        ledger
            .record_annotation(scored, "scorer:turns", ts(millis))
            .unwrap();
    }
    let other = OutcomeAnnotation::from_votes(
        "session-9:t-9",
        "vpn-clients",
        votes(&[("judge:a", OutcomeClass::Redone)]),
        ts(4_000),
    )
    .unwrap();
    ledger
        .record_annotation(other, "scorer:turns", ts(4_000))
        .unwrap();

    let curve = ledger.outcome_curve("odyssey-login");
    let turns: Vec<&str> = curve.iter().map(|a| a.turn_ref.as_str()).collect();
    assert_eq!(
        turns,
        vec!["session-9:t-1", "session-9:t-2", "session-9:t-3"]
    );
    assert_eq!(ledger.outcome_curve("vpn-clients").len(), 1);
}

#[test]
fn a_measurement_below_threshold_closes_without_bookkeeping() {
    let mut ledger = GapLedger::new();
    let event_id = ledger.record_event(search_event()).unwrap();
    let gap_id = ledger
        .file_gap(
            intent_subject(),
            "Odyssey login guidance fails too often",
            vec![event_citation(&event_id)],
            GapOrigin::Induction,
            ClosureCriteria::FailureRateBelow {
                threshold_millis: 500,
            },
            380,
            45_000,
            "induction",
            ts(1_000),
        )
        .unwrap();

    // One correction: rate 1000 per mille, at the threshold — unsatisfied
    // is not an error, the measurement just has not moved enough.
    let first = ledger
        .record_annotation(
            annotation(
                "session-9:t-1",
                &[("judge:a", OutcomeClass::Corrected)],
                2_000,
            ),
            "scorer:turns",
            ts(2_000),
        )
        .unwrap();
    assert!(first.closed_gap_ids.is_empty());
    assert_eq!(ledger.entry(&gap_id).unwrap().status, GapStatus::Open);

    // Two accepted turns: the rate drops to 333 per mille and the
    // second recording closes the entry in the same mutation.
    let mut closed = Vec::new();
    for (turn, millis) in [("t-2", 3_000), ("t-3", 4_000)] {
        let recorded = ledger
            .record_annotation(
                annotation(
                    &format!("session-9:{turn}"),
                    &[("judge:a", OutcomeClass::Accepted)],
                    millis,
                ),
                "scorer:turns",
                ts(millis),
            )
            .unwrap();
        closed.extend(recorded.closed_gap_ids);
    }
    assert_eq!(closed, vec![gap_id.clone()]);
    let entry = ledger.entry(&gap_id).unwrap();
    assert_eq!(entry.status, GapStatus::Closed);
    assert_eq!(
        entry.resolution.as_deref(),
        Some("failure-rate:333:below:500")
    );
}

#[test]
fn annotations_ride_the_snapshot() {
    let mut ledger = GapLedger::new();
    ledger
        .record_annotation(
            annotation(
                "session-9:t-1",
                &[("judge:a", OutcomeClass::Corrected)],
                1_000,
            ),
            "scorer:turns",
            ts(1_000),
        )
        .unwrap();
    let snapshot = ledger.to_snapshot().unwrap();
    let restored = GapLedger::from_snapshot(snapshot.clone()).unwrap();
    assert_eq!(restored.to_snapshot().unwrap(), snapshot);
    assert_eq!(restored.outcome_curve("odyssey-login").len(), 1);
    assert_eq!(
        restored.tally("odyssey-login").unwrap().corrected,
        1,
        "the projection rides the snapshot; the annotations are the record"
    );
}

#[test]
fn snapshots_written_before_annotations_still_load() {
    // A v1 snapshot without the annotations key (and a tally without
    // the neutral field) loads with both defaulted.
    let mut ledger = GapLedger::new();
    ledger.record_outcome("odyssey-login", OutcomeClass::Corrected);
    let mut snapshot = ledger.to_snapshot().unwrap();
    snapshot.as_object_mut().unwrap().remove("annotations");
    snapshot["tallies"]["odyssey-login"]
        .as_object_mut()
        .unwrap()
        .remove("neutral");
    let restored = GapLedger::from_snapshot(snapshot).unwrap();
    assert_eq!(restored.outcome_curve("odyssey-login").len(), 0);
    assert_eq!(restored.tally("odyssey-login").unwrap().neutral, 0);
    assert_eq!(restored.failure_rate_millis("odyssey-login"), Some(1000));
}
