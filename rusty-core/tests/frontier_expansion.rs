//! Frontier-expansion cycle tests (EP-07-S11): the mastery gate reads the
//! behavioral signal and the retention book; a mastered domain opens
//! bounded speculative entries whose decisions journal their adjacency
//! source and edge; probes promote or park; the parked clock declines and
//! expires; an unmastered domain is a typed refusal with its reasons.

use chrono::{DateTime, Utc};
use rusty_agent_runtime::frontier::{
    DomainMembership, ExpansionPolicy, ExpansionProposal, FrontierError, ProbeObservation,
    assess_domain, run_expansion_cycle,
};
use rusty_agent_runtime::gaps::{
    AdjacencySource, Citation, CitationKind, ClosureCriteria, GapError, GapLedger, GapMutationKind,
    GapOrigin, GapStatus, GapSubject, JudgeVote, OutcomeAnnotation, OutcomeClass,
};
use rusty_agent_runtime::skill_retention::{RetentionBook, RetentionPolicy};

const DAY: i64 = 86_400;

fn t0() -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp(1_750_000_000, 0).unwrap()
}

fn at(base: DateTime<Utc>, secs: i64) -> DateTime<Utc> {
    base + chrono::Duration::seconds(secs)
}

/// The test policy: two decisive outcomes suffice, the failure bar is
/// 200 per mille, skills must sit quiet for a day, and the dials allow
/// two opens and one probe per cycle.
fn policy() -> ExpansionPolicy {
    ExpansionPolicy {
        mastery_failure_threshold_millis: 200,
        mastery_min_outcomes: 2,
        stability_window_secs: DAY as u64,
        max_open_per_cycle: 2,
        max_probes_per_cycle: 1,
    }
}

fn intents() -> Vec<String> {
    vec![
        "intent.password-reset".to_owned(),
        "intent.vpn-drops".to_owned(),
    ]
}

fn skills() -> Vec<String> {
    vec!["reset-password".to_owned()]
}

/// Score `count` accepted turns against an intent — mastery evidence.
fn seed_accepted(ledger: &mut GapLedger, intent: &str, count: u64) {
    for turn in 0..count {
        let annotation = OutcomeAnnotation::from_votes(
            format!("session-1:turn-{turn}"),
            intent,
            vec![JudgeVote {
                judge: "heuristic-v1".to_owned(),
                vote: OutcomeClass::Accepted,
            }],
            at(t0(), turn as i64),
        )
        .unwrap();
        ledger.record_annotation(annotation, "test", t0()).unwrap();
    }
}

/// A mastered ledger + book: both intents pass, the one skill sits quiet.
fn mastered() -> (GapLedger, RetentionBook) {
    let mut ledger = GapLedger::new();
    for intent in intents() {
        seed_accepted(&mut ledger, &intent, 2);
    }
    let mut book = RetentionBook::new();
    book.register_promoted("reset-password", at(t0(), -2 * DAY))
        .unwrap();
    (ledger, book)
}

/// A speculative proposal; the adjacency source and edge id vary, the
/// rest is boilerplate.
fn proposal(statement: &str, adjacency: AdjacencySource, edge: &str) -> ExpansionProposal {
    ExpansionProposal {
        subject: GapSubject::Intent {
            intent_id: format!("intent.{statement}"),
        },
        statement: statement.to_owned(),
        adjacency,
        edge: Citation::new(CitationKind::AdjacencyEdge, edge, Some(edge.to_owned())).unwrap(),
        closure_criteria: ClosureCriteria::FailureRateBelow {
            threshold_millis: 500,
        },
    }
}

fn run(
    ledger: &mut GapLedger,
    book: &RetentionBook,
    proposals: &[ExpansionProposal],
    probes: &[ProbeObservation],
    now: DateTime<Utc>,
) -> Result<rusty_agent_runtime::frontier::ExpansionReport, FrontierError> {
    let domain = DomainMembership {
        name: "endpoint-support",
        intents: &intents(),
        skills: &skills(),
    };
    run_expansion_cycle(
        ledger,
        book,
        &domain,
        proposals,
        probes,
        &policy(),
        "expansion",
        now,
    )
}

#[test]
fn a_mastered_domain_opens_bounded_entries_with_their_source_and_edge() {
    let (mut ledger, book) = mastered();
    let proposals = vec![
        proposal(
            "ztna-vpn",
            AdjacencySource::Structural,
            "cmdb:laptop-fleet:depends-on:ztna",
        ),
        proposal(
            "conf-room-av",
            AdjacencySource::Statistical,
            "cooc:laptop+av:0.14",
        ),
        proposal(
            "printer-fleet",
            AdjacencySource::ModelPrior,
            "prior:endpoint-adjacent",
        ),
    ];
    let report = run(&mut ledger, &book, &proposals, &[], t0()).unwrap();

    // The dial holds at two; the third waits, named by its statement.
    assert_eq!(report.opened.len(), 2);
    assert_eq!(report.deferred, vec!["printer-fleet".to_owned()]);

    // Every decision row carries its source and its specific edge.
    let first = &report.opened[0];
    let entry = ledger.entry(first).unwrap();
    assert_eq!(
        entry.origin,
        GapOrigin::Speculative {
            adjacency: AdjacencySource::Structural
        }
    );
    assert_eq!(entry.evidence.len(), 1);
    assert_eq!(entry.evidence[0].kind, CitationKind::AdjacencyEdge);
    assert_eq!(entry.evidence[0].id, "cmdb:laptop-fleet:depends-on:ztna");
    let second = &report.opened[1];
    assert_eq!(
        ledger.entry(second).unwrap().origin,
        GapOrigin::Speculative {
            adjacency: AdjacencySource::Statistical
        }
    );
}

#[test]
fn an_unvalidated_speculative_entry_cannot_hunt() {
    let (mut ledger, book) = mastered();
    let proposals = vec![proposal(
        "ztna-vpn",
        AdjacencySource::Structural,
        "cmdb:laptop-fleet:depends-on:ztna",
    )];
    let report = run(&mut ledger, &book, &proposals, &[], t0()).unwrap();

    let err = ledger
        .transition(&report.opened[0], GapStatus::Hunting, "hunter", t0())
        .expect_err("speculation cannot hunt unvalidated");
    assert!(
        matches!(err, GapError::UnvalidatedSpeculation(_)),
        "the refusal is typed: {err}"
    );
}

#[test]
fn a_confirmed_probe_promotes_with_its_citation() {
    let (mut ledger, book) = mastered();
    let proposals = vec![proposal(
        "ztna-vpn",
        AdjacencySource::Structural,
        "cmdb:laptop-fleet:depends-on:ztna",
    )];
    let report = run(&mut ledger, &book, &proposals, &[], t0()).unwrap();
    let gap_id = report.opened[0].clone();

    // A later cycle runs the demand probe: fourteen matching tickets.
    let probes = vec![ProbeObservation {
        gap_id: gap_id.clone(),
        demand_hits: 14,
        supply_covered: false,
    }];
    let report = run(&mut ledger, &book, &[], &probes, at(t0(), DAY)).unwrap();
    assert_eq!(report.probed, vec![gap_id.clone()]);

    // Promoted to observed with the probe as citation — it may hunt now.
    let entry = ledger.entry(&gap_id).unwrap();
    assert!(entry.observed);
    assert!(
        entry
            .evidence
            .iter()
            .any(|c| c.kind == CitationKind::ProbeResult),
        "the probe result is a citation"
    );
    ledger
        .transition(&gap_id, GapStatus::Hunting, "hunter", at(t0(), DAY))
        .expect("a validated entry enters the queue");
}

#[test]
fn an_empty_probe_parks_and_the_clock_declines_and_expires() {
    let (mut ledger, book) = mastered();
    let proposals = vec![proposal(
        "conf-room-av",
        AdjacencySource::Statistical,
        "cooc:laptop+av:0.01",
    )];
    let report = run(&mut ledger, &book, &proposals, &[], t0()).unwrap();
    let gap_id = report.opened[0].clone();

    // A near-empty stream parks the entry; the schedule reads as data.
    ledger
        .record_probe(&gap_id, 0, false, "probe", at(t0(), DAY))
        .unwrap();
    let entry = ledger.entry(&gap_id).unwrap();
    assert_eq!(entry.status, GapStatus::Parked);
    let schedule = entry.probe_schedule().unwrap();
    assert_eq!(schedule.empty_probes, 1);
    assert_eq!(
        schedule.next_probe_at,
        at(t0(), 2 * DAY),
        "grace doubles from one day"
    );
    assert_eq!(
        schedule.expires_at,
        at(t0(), 8 * DAY),
        "1 + 2 + 4 days of remaining grace"
    );

    // Not due before the deadline; due at it.
    assert!(ledger.probes_due(at(t0(), 2 * DAY - 1)).is_empty());
    assert_eq!(ledger.probes_due(at(t0(), 2 * DAY)), vec![gap_id.clone()]);

    // Misses land on schedule: each grace doubles, and the fourth empty
    // probe exhausts the clock — expiry is certain by expires_at.
    ledger
        .record_probe(&gap_id, 0, false, "probe", at(t0(), 2 * DAY))
        .unwrap();
    let schedule = ledger.entry(&gap_id).unwrap().probe_schedule().unwrap();
    assert_eq!(schedule.next_probe_at, at(t0(), 4 * DAY));
    ledger
        .record_probe(&gap_id, 0, false, "probe", at(t0(), 4 * DAY))
        .unwrap();
    ledger
        .record_probe(&gap_id, 0, false, "probe", at(t0(), 8 * DAY))
        .unwrap();
    let expired = ledger.expire_parked(at(t0(), 8 * DAY), "expiry").unwrap();
    assert_eq!(expired, vec![gap_id.clone()]);
    let entry = ledger.entry(&gap_id).unwrap();
    assert_eq!(entry.status, GapStatus::Closed);
    assert_eq!(entry.resolution.as_deref(), Some("expired:no-demand"));
}

#[test]
fn the_probe_dial_bounds_the_cycle_and_names_the_deferred() {
    let (mut ledger, book) = mastered();
    let proposals = vec![
        proposal("ztna-vpn", AdjacencySource::Structural, "cmdb:a"),
        proposal("conf-room-av", AdjacencySource::Statistical, "cooc:b"),
    ];
    let report = run(&mut ledger, &book, &proposals, &[], t0()).unwrap();

    // One probe per cycle: the second observation defers, named.
    let probes: Vec<ProbeObservation> = report
        .opened
        .iter()
        .map(|gap_id| ProbeObservation {
            gap_id: gap_id.clone(),
            demand_hits: 3,
            supply_covered: true,
        })
        .collect();
    let report = run(&mut ledger, &book, &[], &probes, at(t0(), DAY)).unwrap();
    assert_eq!(report.probed.len(), 1);
    assert_eq!(report.deferred, vec![probes[1].gap_id.clone()]);
    assert!(ledger.entry(&report.probed[0]).unwrap().observed);
    assert!(
        !ledger.entry(&probes[1].gap_id).unwrap().observed,
        "the deferred probe never recorded"
    );
}

#[test]
fn an_intent_above_the_failure_threshold_blocks_expansion() {
    let (mut ledger, book) = mastered();
    // One intent turns bad: two corrections against it — 500 per mille.
    for turn in 10..12 {
        let annotation = OutcomeAnnotation::from_votes(
            format!("session-2:turn-{turn}"),
            "intent.vpn-drops",
            vec![JudgeVote {
                judge: "heuristic-v1".to_owned(),
                vote: OutcomeClass::Corrected,
            }],
            at(t0(), turn),
        )
        .unwrap();
        ledger.record_annotation(annotation, "test", t0()).unwrap();
    }

    let err = run(&mut ledger, &book, &[], &[], t0()).expect_err("unmastered refuses");
    match err {
        FrontierError::DomainNotMastered { domain, reasons } => {
            assert_eq!(domain, "endpoint-support");
            assert!(
                reasons
                    .iter()
                    .any(|r| r.contains("intent.vpn-drops") && r.contains("500")),
                "the reason names the intent and its rate: {reasons:?}"
            );
        }
        other => panic!("expected DomainNotMastered, got {other}"),
    }
}

#[test]
fn an_undermeasured_intent_blocks_expansion() {
    let (mut ledger, book) = mastered();
    // A third intent belongs to the domain but has never been scored.
    let mut domain_intents = intents();
    domain_intents.push("intent.fresh-topic".to_owned());
    let domain = DomainMembership {
        name: "endpoint-support",
        intents: &domain_intents,
        skills: &skills(),
    };
    let err = run_expansion_cycle(
        &mut ledger,
        &book,
        &domain,
        &[],
        &[],
        &policy(),
        "expansion",
        t0(),
    )
    .expect_err("an unmeasured intent is not a passing intent");
    assert!(
        matches!(err, FrontierError::DomainNotMastered { ref reasons, .. }
            if reasons.iter().any(|r| r.contains("intent.fresh-topic"))),
        "the reason names the unmeasured intent: {err}"
    );
}

#[test]
fn a_churning_skill_blocks_expansion() {
    let (mut ledger, mut book) = mastered();
    // An outcome penalty lands within the stability window — churn.
    let retention = RetentionPolicy::default();
    book.apply_outcome_penalty(
        "reset-password",
        "intent.password-reset",
        &retention,
        t0(),
        "judge",
    )
    .unwrap();

    let err = run(&mut ledger, &book, &[], &[], t0()).expect_err("churn refuses");
    assert!(
        matches!(err, FrontierError::DomainNotMastered { ref reasons, .. }
            if reasons.iter().any(|r| r.contains("reset-password"))),
        "the reason names the churning skill: {err}"
    );

    // Once the window passes with no fresh churn, the gate lifts.
    let report = run(&mut ledger, &book, &[], &[], at(t0(), DAY + 1));
    assert!(
        report.is_ok(),
        "a quiet window restores mastery: {report:?}"
    );
}

#[test]
fn a_pin_is_governance_not_churn() {
    let (ledger, mut book) = mastered();
    book.pin("reset-password", "operator", t0()).unwrap();
    let domain = DomainMembership {
        name: "endpoint-support",
        intents: &intents(),
        skills: &skills(),
    };
    let report = assess_domain(
        &domain,
        &|intent| ledger.outcome_tally(intent).cloned(),
        &book,
        &policy(),
        t0(),
    );
    assert!(
        report.mastered,
        "a pin holds, it does not churn: {:?}",
        report.reasons
    );
}

#[test]
fn a_zeroed_dial_is_a_configuration_error() {
    let (mut ledger, book) = mastered();
    let mut broken = policy();
    broken.max_open_per_cycle = 0;
    let domain = DomainMembership {
        name: "endpoint-support",
        intents: &intents(),
        skills: &skills(),
    };
    let err = run_expansion_cycle(
        &mut ledger,
        &book,
        &domain,
        &[],
        &[],
        &broken,
        "expansion",
        t0(),
    )
    .expect_err("a cycle configured to do nothing is an error");
    assert!(
        matches!(err, FrontierError::InvalidPolicy(_)),
        "named as misconfiguration: {err}"
    );
}

#[test]
fn every_decision_journals_its_source_and_edge() {
    let (mut ledger, book) = mastered();
    let proposals = vec![proposal(
        "ztna-vpn",
        AdjacencySource::Structural,
        "cmdb:laptop-fleet:depends-on:ztna",
    )];
    let report = run(&mut ledger, &book, &proposals, &[], t0()).unwrap();

    // The mutation chain is the answer to "why did the agent decide
    // laptops imply VPN": the filing names the adjacency and the edge.
    let chain = ledger.chain(&report.opened[0]).unwrap();
    let filed = chain
        .iter()
        .find_map(|row| match &row.kind {
            GapMutationKind::Filed {
                origin, evidence, ..
            } => Some((origin, evidence)),
            _ => None,
        })
        .expect("the chain opens with the filing");
    assert_eq!(
        filed.0,
        &GapOrigin::Speculative {
            adjacency: AdjacencySource::Structural
        },
        "the source is on the record"
    );
    assert_eq!(filed.1[0].id, "cmdb:laptop-fleet:depends-on:ztna");
}
