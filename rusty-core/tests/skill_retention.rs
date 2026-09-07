//! Retention-scoring and curator tests (EP-07-S03): turn traffic and
//! clock advance drive a skill through `Promoted → Cold → Archived` on
//! retention alone; loads replenish; pins exempt; restores return; the
//! curator merges near-duplicates into one umbrella with the originals
//! archived, every step a ledger mutation, nothing ever deleted; and a
//! cooled skill leaves the prompt index structurally.

use chrono::{DateTime, Utc};
use rusty_agent_runtime::skill::{SkillMetadata, SkillPromotionStatus};
use rusty_agent_runtime::skill_retention::{
    CuratorPolicy, RETENTION_SCALE_MILLI, RetentionBook, RetentionError, RetentionMutation,
    RetentionPolicy,
};
use rusty_agent_runtime::skills::{
    SkillBinding, SkillCatalogEntry, SkillExclusionReason, SkillSelectionFeatures,
    SkillSelectionPolicy, select_skills,
};

const DAY: i64 = 86_400;

/// A fixed instant plus `secs` seconds.
fn at(base: DateTime<Utc>, secs: i64) -> DateTime<Utc> {
    base + chrono::Duration::seconds(secs)
}

fn t0() -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp(1_750_000_000, 0).unwrap()
}

/// A fast test policy: decay 100/day, cold below 300, archive after one
/// idle hour while cold, replenish 200 per load.
fn policy() -> RetentionPolicy {
    RetentionPolicy {
        cold_threshold_milli: 300,
        idle_archive_secs: 3_600,
        decay_per_idle_day_milli: 100,
        load_replenish_milli: 200,
    }
}

fn metadata(name: &str, description: &str) -> SkillMetadata {
    SkillMetadata {
        name: name.to_owned(),
        description: description.to_owned(),
        revision: 1,
        content_hash: format!("sha256:{name}"),
        license: None,
        allowed_tools: Vec::new(),
        compatibility: None,
        eval_gate: None,
        dependencies: Vec::new(),
    }
}

// --------------------------------------------------------------------- //
// The retention lifecycle
// --------------------------------------------------------------------- //

#[test]
fn idleness_alone_drives_promoted_cold_archived() {
    let policy = policy();
    let mut book = RetentionBook::new();
    book.register_promoted("deploy-web-service", t0()).unwrap();

    // Five idle days: 1000 - 5*100 = 500 — still warm.
    let transitions = book.tick(&policy, at(t0(), 5 * DAY), "retention").unwrap();
    assert!(transitions.is_empty());
    assert_eq!(book.get("deploy-web-service").unwrap().score_milli, 500);
    assert_eq!(
        book.get("deploy-web-service").unwrap().lifecycle,
        SkillPromotionStatus::Promoted
    );

    // Eight idle days: 500 - 3*100 = 200 < 300 — the skill cools, and the
    // crossing journals.
    let transitions = book.tick(&policy, at(t0(), 8 * DAY), "retention").unwrap();
    assert_eq!(transitions.len(), 1);
    assert_eq!(transitions[0].from, SkillPromotionStatus::Promoted);
    assert_eq!(transitions[0].to, SkillPromotionStatus::Cold);
    let record = book.get("deploy-web-service").unwrap();
    assert_eq!(record.lifecycle, SkillPromotionStatus::Cold);
    assert_eq!(record.score_milli, 200);
    assert_eq!(
        record.ledger.last().unwrap().mutation,
        RetentionMutation::CooledToCold { score_milli: 200 }
    );
    assert_eq!(record.ledger.last().unwrap().actor, "retention");

    // One second short of the idle period: still cold.
    let transitions = book
        .tick(&policy, at(t0(), 8 * DAY + 3_599), "retention")
        .unwrap();
    assert!(transitions.is_empty());

    // The idle period elapses: archived, with the idle time journaled.
    let transitions = book
        .tick(&policy, at(t0(), 8 * DAY + 3_600), "retention")
        .unwrap();
    assert_eq!(transitions.len(), 1);
    assert_eq!(transitions[0].to, SkillPromotionStatus::Archived);
    let record = book.get("deploy-web-service").unwrap();
    assert_eq!(record.lifecycle, SkillPromotionStatus::Archived);
    assert_eq!(
        record.ledger.last().unwrap().mutation,
        RetentionMutation::Archived { idle_secs: 3_600 }
    );
    assert_eq!(record.ledger.len(), 2, "one row per discrete act");
}

#[test]
fn loads_replenish_and_hold_a_skill_warm() {
    let policy = policy();
    let mut book = RetentionBook::new();
    book.register_promoted("deploy-web-service", t0()).unwrap();

    // Sixty days of traffic: a load every other day keeps the score at
    // full — the skill never cools.
    for day in (2..=60).step_by(2) {
        let now = at(t0(), day * DAY);
        book.tick(&policy, now, "retention").unwrap();
        book.record_load("deploy-web-service", now, &policy)
            .unwrap();
    }
    let record = book.get("deploy-web-service").unwrap();
    assert_eq!(record.lifecycle, SkillPromotionStatus::Promoted);
    assert_eq!(record.score_milli, RETENTION_SCALE_MILLI);
    assert!(record.ledger.is_empty(), "loads never journal");
}

#[test]
fn a_load_while_cold_replenishes_but_never_transitions() {
    let policy = policy();
    let mut book = RetentionBook::new();
    book.register_promoted("deploy-web-service", t0()).unwrap();
    book.tick(&policy, at(t0(), 8 * DAY), "retention").unwrap();
    assert_eq!(
        book.get("deploy-web-service").unwrap().lifecycle,
        SkillPromotionStatus::Cold
    );

    // The state machine admits no Cold → Promoted edge: traffic
    // replenishes the score (and defers the archive clock), the state
    // stays Cold until an operator restores it.
    let score = book
        .record_load("deploy-web-service", at(t0(), 8 * DAY + 600), &policy)
        .unwrap();
    assert_eq!(score, 400);
    let record = book.get("deploy-web-service").unwrap();
    assert_eq!(record.lifecycle, SkillPromotionStatus::Cold);

    // The load moved the idle base: the archive fires one idle period
    // after the load, not after the cold entry.
    let transitions = book
        .tick(&policy, at(t0(), 8 * DAY + 3_599), "retention")
        .unwrap();
    assert!(transitions.is_empty(), "the load deferred the archive");
    let transitions = book
        .tick(&policy, at(t0(), 8 * DAY + 600 + 3_600), "retention")
        .unwrap();
    assert_eq!(transitions.len(), 1);
    assert_eq!(transitions[0].to, SkillPromotionStatus::Archived);
}

#[test]
fn tick_is_cadence_invariant() {
    let policy = policy();
    // Decay accounting is exactly cadence-invariant: one five-day tick
    // and five one-day ticks charge the same idleness.
    let mut one_tick = RetentionBook::new();
    one_tick
        .register_promoted("deploy-web-service", t0())
        .unwrap();
    one_tick
        .tick(&policy, at(t0(), 5 * DAY), "retention")
        .unwrap();

    let mut five_ticks = RetentionBook::new();
    five_ticks
        .register_promoted("deploy-web-service", t0())
        .unwrap();
    for day in 1..=5 {
        five_ticks
            .tick(&policy, at(t0(), day * DAY), "retention")
            .unwrap();
    }
    assert_eq!(
        one_tick.get("deploy-web-service").unwrap().score_milli,
        five_ticks.get("deploy-web-service").unwrap().score_milli,
    );

    // A repeated tick at the same instant is a no-op: the charged
    // interval is accounted.
    let transitions = one_tick
        .tick(&policy, at(t0(), 5 * DAY), "retention")
        .unwrap();
    assert!(transitions.is_empty());
    assert_eq!(
        one_tick.get("deploy-web-service").unwrap().score_milli,
        500,
        "the second tick charged nothing"
    );
}

#[test]
fn pinned_skills_are_retention_exempt() {
    let policy = policy();
    let mut book = RetentionBook::new();
    book.register_promoted("deploy-web-service", t0()).unwrap();
    book.pin("deploy-web-service", "amjad", t0()).unwrap();

    let transitions = book
        .tick(&policy, at(t0(), 365 * DAY), "retention")
        .unwrap();
    assert!(transitions.is_empty());
    let record = book.get("deploy-web-service").unwrap();
    assert_eq!(record.lifecycle, SkillPromotionStatus::Promoted);
    assert_eq!(record.score_milli, RETENTION_SCALE_MILLI);
    assert_eq!(
        record
            .ledger
            .iter()
            .map(|row| &row.mutation)
            .collect::<Vec<_>>(),
        vec![&RetentionMutation::Pinned]
    );

    // Unpin rebases the decay clock: the year under the pin is never
    // back-charged.
    book.unpin("deploy-web-service", "amjad", at(t0(), 365 * DAY))
        .unwrap();
    book.tick(&policy, at(t0(), 366 * DAY), "retention")
        .unwrap();
    assert_eq!(
        book.get("deploy-web-service").unwrap().score_milli,
        RETENTION_SCALE_MILLI - 100,
        "one idle day since the unpin, not a year"
    );
}

#[test]
fn restore_returns_an_archived_skill_to_full_score() {
    let policy = policy();
    let mut book = RetentionBook::new();
    book.register_promoted("deploy-web-service", t0()).unwrap();
    book.tick(&policy, at(t0(), 8 * DAY), "retention").unwrap();
    book.tick(&policy, at(t0(), 8 * DAY + 3_600), "retention")
        .unwrap();
    assert_eq!(
        book.get("deploy-web-service").unwrap().lifecycle,
        SkillPromotionStatus::Archived
    );

    book.restore("deploy-web-service", "amjad", at(t0(), 9 * DAY))
        .unwrap();
    let record = book.get("deploy-web-service").unwrap();
    assert_eq!(record.lifecycle, SkillPromotionStatus::Promoted);
    assert_eq!(record.score_milli, RETENTION_SCALE_MILLI);
    assert_eq!(
        record.ledger.last().unwrap().mutation,
        RetentionMutation::Restored
    );

    // Refused transitions are typed errors, never silent no-ops.
    assert!(matches!(
        book.restore("deploy-web-service", "amjad", at(t0(), 9 * DAY)),
        Err(RetentionError::InvalidTransition { .. })
    ));
}

#[test]
fn the_book_refuses_what_it_cannot_honestly_answer() {
    let policy = policy();
    let mut book = RetentionBook::new();
    assert!(matches!(
        book.record_load("ghost", t0(), &policy),
        Err(RetentionError::UntrackedSkill(_))
    ));
    assert!(matches!(
        book.pin("ghost", "amjad", t0()),
        Err(RetentionError::UntrackedSkill(_))
    ));
    book.register_promoted("deploy-web-service", t0()).unwrap();
    assert!(matches!(
        book.register_promoted("deploy-web-service", t0()),
        Err(RetentionError::AlreadyTracked(_))
    ));

    // An archived skill cannot be loaded back into view — the operator's
    // restore is the only way back.
    book.tick(&policy, at(t0(), 8 * DAY), "retention").unwrap();
    book.tick(&policy, at(t0(), 8 * DAY + 3_600), "retention")
        .unwrap();
    assert!(matches!(
        book.record_load("deploy-web-service", at(t0(), 9 * DAY), &policy),
        Err(RetentionError::InvalidTransition { .. })
    ));
}

// --------------------------------------------------------------------- //
// The curator pass
// --------------------------------------------------------------------- //

/// The near-duplicate fixture: three renderings of one capability (the
/// token sets overlap at or above the threshold) plus one distinct skill.
fn fixture_library() -> Vec<SkillMetadata> {
    vec![
        metadata("deploy-web-service", "Deploy the web service to production"),
        metadata(
            "deploy-web-services",
            "Deploy the web service to production",
        ),
        metadata("web-service-deploy", "Deploy the web service to production"),
        metadata(
            "refund-policy-lookup",
            "Look up the refund policy for a customer",
        ),
    ]
}

fn fixture_book() -> RetentionBook {
    let mut book = RetentionBook::new();
    for name in [
        "deploy-web-service",
        "deploy-web-services",
        "web-service-deploy",
        "refund-policy-lookup",
    ] {
        book.register_promoted(name, t0()).unwrap();
    }
    book
}

#[test]
fn curator_consolidates_near_duplicates_into_one_umbrella() {
    let mut book = fixture_book();
    let report = rusty_agent_runtime::skill_retention::curator_pass(
        &mut book,
        &fixture_library(),
        &CuratorPolicy::default(),
        at(t0(), DAY),
        "curator",
    );

    assert_eq!(report.skills_examined, 4);
    assert_eq!(report.consolidations.len(), 1);
    let outcome = &report.consolidations[0];
    // Equal scores break ties by ascending name.
    assert_eq!(outcome.umbrella, "deploy-web-service");
    assert_eq!(
        outcome.merged,
        vec![
            "deploy-web-services".to_owned(),
            "web-service-deploy".to_owned()
        ]
    );

    // Every step journaled: the originals archive with the umbrella
    // named; the umbrella records what it absorbed.
    for name in &outcome.merged {
        let record = book.get(name).unwrap();
        assert_eq!(record.lifecycle, SkillPromotionStatus::Archived);
        assert_eq!(
            record.ledger.last().unwrap().mutation,
            RetentionMutation::ConsolidatedIntoUmbrella {
                umbrella: outcome.umbrella.clone()
            }
        );
    }
    let umbrella = book.get(&outcome.umbrella).unwrap();
    assert_eq!(umbrella.lifecycle, SkillPromotionStatus::Promoted);
    assert_eq!(
        umbrella.ledger.last().unwrap().mutation,
        RetentionMutation::DesignatedUmbrella {
            merged: outcome.merged.clone()
        }
    );

    // The distinct skill is untouched — and nothing anywhere was deleted.
    let distinct = book.get("refund-policy-lookup").unwrap();
    assert_eq!(distinct.lifecycle, SkillPromotionStatus::Promoted);
    assert!(distinct.ledger.is_empty());
    assert_eq!(book.len(), 4, "consolidation archives; it never deletes");

    // A second pass consolidates nothing: archived originals are
    // invisible to the curator.
    let second = rusty_agent_runtime::skill_retention::curator_pass(
        &mut book,
        &fixture_library(),
        &CuratorPolicy::default(),
        at(t0(), 2 * DAY),
        "curator",
    );
    assert!(second.consolidations.is_empty());
    assert_eq!(second.skills_examined, 2);
}

#[test]
fn curator_is_deterministic() {
    let mut first = fixture_book();
    let mut second = fixture_book();
    let catalog = fixture_library();
    let a = rusty_agent_runtime::skill_retention::curator_pass(
        &mut first,
        &catalog,
        &CuratorPolicy::default(),
        at(t0(), DAY),
        "curator",
    );
    let b = rusty_agent_runtime::skill_retention::curator_pass(
        &mut second,
        &catalog,
        &CuratorPolicy::default(),
        at(t0(), DAY),
        "curator",
    );
    assert_eq!(a, b);
    assert_eq!(first, second);
}

#[test]
fn the_operators_pin_outranks_the_curator() {
    let mut book = fixture_book();
    // Pin one near-duplicate: it becomes the umbrella (the operator's
    // word outranks the ascending-name tiebreak) and is never merged
    // away.
    book.pin("web-service-deploy", "amjad", t0()).unwrap();
    let report = rusty_agent_runtime::skill_retention::curator_pass(
        &mut book,
        &fixture_library(),
        &CuratorPolicy::default(),
        at(t0(), DAY),
        "curator",
    );
    let outcome = &report.consolidations[0];
    assert_eq!(outcome.umbrella, "web-service-deploy");
    assert!(book.get("web-service-deploy").unwrap().pinned);
    assert_eq!(
        book.get("web-service-deploy").unwrap().lifecycle,
        SkillPromotionStatus::Promoted
    );
    for name in &outcome.merged {
        assert_eq!(
            book.get(name).unwrap().lifecycle,
            SkillPromotionStatus::Archived
        );
    }
}

#[test]
fn a_fully_pinned_cluster_consolidates_nothing() {
    let mut book = RetentionBook::new();
    book.register_promoted("deploy-web-service", t0()).unwrap();
    book.register_promoted("deploy-web-services", t0()).unwrap();
    book.pin("deploy-web-service", "amjad", t0()).unwrap();
    book.pin("deploy-web-services", "amjad", t0()).unwrap();
    let catalog = vec![
        metadata("deploy-web-service", "Deploy the web service to production"),
        metadata(
            "deploy-web-services",
            "Deploy the web service to production",
        ),
    ];
    let report = rusty_agent_runtime::skill_retention::curator_pass(
        &mut book,
        &catalog,
        &CuratorPolicy::default(),
        at(t0(), DAY),
        "curator",
    );
    assert!(
        report.consolidations.is_empty(),
        "two pins means the operator wants both"
    );
}

// --------------------------------------------------------------------- //
// The prompt-index coupling
// --------------------------------------------------------------------- //

#[test]
fn cold_and_archived_skills_leave_the_prompt_index() {
    let entry = |name: &str, lifecycle: Option<SkillPromotionStatus>| SkillCatalogEntry {
        metadata: metadata(name, "a skill"),
        binding: SkillBinding::default(),
        lifecycle,
    };
    let catalog = vec![
        entry("warm", None),
        entry("promoted", Some(SkillPromotionStatus::Promoted)),
        entry("cold", Some(SkillPromotionStatus::Cold)),
        entry("archived", Some(SkillPromotionStatus::Archived)),
    ];
    let selection = select_skills(
        &SkillSelectionFeatures::default(),
        &catalog,
        &SkillSelectionPolicy::default(),
    );

    let names: Vec<&str> = selection
        .selected
        .iter()
        .map(|ranked| ranked.name.as_str())
        .collect();
    assert_eq!(names, vec!["promoted", "warm"], "score ties break by name");

    assert_eq!(selection.excluded.len(), 2);
    for excluded in &selection.excluded {
        match &excluded.reason {
            SkillExclusionReason::LifecycleGated { status } => {
                assert!(
                    matches!(
                        status,
                        SkillPromotionStatus::Cold | SkillPromotionStatus::Archived
                    ),
                    "the exclusion names its status"
                );
            }
            other => panic!("expected a lifecycle exclusion, got {other:?}"),
        }
    }
}
