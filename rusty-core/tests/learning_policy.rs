//! The blueprint learning policy (EP-08-S07): the floor is the pre-policy
//! behavior, declarations parse with every violation named, budgets enforce
//! exactly and never exceed, and the frontier bridge gives the expansion
//! dials their policy home.

use rusty_agent_runtime::frontier::ExpansionPolicy;
use rusty_agent_runtime::learning::{
    FLOOR_MAX_HUNTS_PER_CYCLE, FLOOR_MAX_PROBES_PER_CYCLE, LearningPolicy, MAX_HUNTS_PER_CYCLE,
};
use rusty_agent_runtime::memory::ConsolidationCadence;
use serde_json::json;

#[test]
fn the_floor_is_the_pre_policy_behavior() {
    let floor = LearningPolicy::floor();
    assert!(floor.review_fork_enabled);
    assert_eq!(floor.consolidation_cadence, None);
    assert_eq!(floor.skill_promotion.eval_suite_ref, None);
    assert_eq!(floor.skill_promotion.approval_scope, None);
    assert_eq!(
        floor.hunting_budget.max_hunts_per_cycle,
        FLOOR_MAX_HUNTS_PER_CYCLE
    );
    assert_eq!(
        floor.hunting_budget.max_probes_per_cycle,
        FLOOR_MAX_PROBES_PER_CYCLE
    );
    assert_eq!(LearningPolicy::default(), floor);
    floor.validate().expect("the floor is valid");
}

#[test]
fn the_hunt_budget_enforces_exactly_and_never_exceeds() {
    let mut policy = LearningPolicy::floor();
    policy.hunting_budget.max_hunts_per_cycle = 2;

    // No caller bound: the declared budget drives.
    assert_eq!(policy.hunt_budget(None), 2);
    // A caller may narrow…
    assert_eq!(policy.hunt_budget(Some(1)), 1);
    // …never exceed, and never silently.
    assert_eq!(policy.hunt_budget(Some(64)), 2);

    // The floor's only bound is the platform ceiling.
    let floor = LearningPolicy::floor();
    assert_eq!(floor.hunt_budget(None), MAX_HUNTS_PER_CYCLE);
    assert_eq!(floor.hunt_budget(Some(3)), 3);
    assert_eq!(floor.hunt_budget(Some(1_000)), MAX_HUNTS_PER_CYCLE);
}

#[test]
fn from_config_reads_the_declaration_or_names_the_floor() {
    // No object, and a null config, both name the floor.
    assert_eq!(LearningPolicy::from_config(&json!({})).unwrap(), None);
    assert_eq!(LearningPolicy::from_config(&json!(null)).unwrap(), None);
    assert_eq!(
        LearningPolicy::from_config(&json!({"recursion_limit": 8})).unwrap(),
        None
    );

    let declared = json!({
        "learning_policy": {
            "review_fork_enabled": false,
            "consolidation_cadence": {"min_turns": 20, "max_interval_ms": 86_400_000},
            "skill_promotion": {"eval_suite_ref": "suite://support-v3", "approval_scope": "ops:learning"},
            "hunting_budget": {"max_hunts_per_cycle": 2, "max_probes_per_cycle": 5},
        }
    });
    let policy = LearningPolicy::from_config(&declared)
        .unwrap()
        .expect("a declaration parses");
    assert!(!policy.review_fork_enabled);
    assert_eq!(
        policy.consolidation_cadence,
        Some(ConsolidationCadence {
            min_turns: 20,
            max_interval_ms: Some(86_400_000),
        })
    );
    assert_eq!(
        policy.skill_promotion.eval_suite_ref.as_deref(),
        Some("suite://support-v3")
    );
    assert_eq!(
        policy.skill_promotion.approval_scope.as_deref(),
        Some("ops:learning")
    );
    assert_eq!(policy.hunting_budget.max_hunts_per_cycle, 2);
    assert_eq!(policy.hunting_budget.max_probes_per_cycle, 5);
}

#[test]
fn a_partial_declaration_floors_what_it_does_not_name() {
    let declared = json!({
        "learning_policy": {"hunting_budget": {"max_hunts_per_cycle": 3, "max_probes_per_cycle": 7}}
    });
    let policy = LearningPolicy::from_config(&declared).unwrap().unwrap();
    assert!(policy.review_fork_enabled);
    assert_eq!(policy.consolidation_cadence, None);
    assert_eq!(policy.skill_promotion.eval_suite_ref, None);
    assert_eq!(policy.hunting_budget.max_hunts_per_cycle, 3);
    assert_eq!(policy.hunting_budget.max_probes_per_cycle, 7);
}

#[test]
fn unknown_fields_and_bad_shapes_are_refused() {
    let typo = json!({"learning_policy": {"hunting_budjet": {}}});
    let error = LearningPolicy::from_config(&typo).unwrap_err();
    assert!(
        error.violations[0].contains("learning_policy does not parse"),
        "the parse failure is named: {error}"
    );

    let wrong_type = json!({"learning_policy": {"review_fork_enabled": "no"}});
    assert!(LearningPolicy::from_config(&wrong_type).is_err());
}

#[test]
fn validation_names_every_violation_in_one_report() {
    let declared = json!({
        "learning_policy": {
            "consolidation_cadence": {"min_turns": 0, "max_interval_ms": 0},
            "skill_promotion": {"eval_suite_ref": "", "approval_scope": ""},
            "hunting_budget": {"max_hunts_per_cycle": 0, "max_probes_per_cycle": 0},
        }
    });
    let error = LearningPolicy::from_config(&declared).unwrap_err();
    let report = error.to_string();
    for expected in [
        "max_hunts_per_cycle must be at least 1",
        "max_probes_per_cycle must be at least 1",
        "eval_suite_ref must not be empty",
        "approval_scope must not be empty",
        "min_turns must be at least 1",
        "max_interval_ms must be at least 1",
    ] {
        assert!(
            report.contains(expected),
            "missing `{expected}` in: {report}"
        );
    }
}

#[test]
fn a_budget_above_the_platform_ceiling_is_a_configuration_error() {
    let declared = json!({
        "learning_policy": {"hunting_budget": {"max_hunts_per_cycle": MAX_HUNTS_PER_CYCLE + 1, "max_probes_per_cycle": 4}}
    });
    let error = LearningPolicy::from_config(&declared).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("max_hunts_per_cycle must be at most")
    );
    // The ceiling itself is admissible.
    let at_ceiling = json!({
        "learning_policy": {"hunting_budget": {"max_hunts_per_cycle": MAX_HUNTS_PER_CYCLE, "max_probes_per_cycle": 4}}
    });
    assert!(LearningPolicy::from_config(&at_ceiling).is_ok());
}

#[test]
fn the_declaration_round_trips_byte_for_byte() {
    let declared = json!({
        "learning_policy": {
            "review_fork_enabled": false,
            "consolidation_cadence": {"min_turns": 5},
            "skill_promotion": {"approval_scope": "ops:learning"},
            "hunting_budget": {"max_hunts_per_cycle": 2, "max_probes_per_cycle": 5},
        }
    });
    let policy = LearningPolicy::from_config(&declared).unwrap().unwrap();
    let serialized = serde_json::to_value(&policy).unwrap();
    let reparsed: LearningPolicy = serde_json::from_value(serialized).unwrap();
    assert_eq!(policy, reparsed);
}

#[test]
fn the_frontier_bridge_gives_the_probe_dial_its_policy_home() {
    let mut policy = LearningPolicy::floor();
    policy.hunting_budget.max_probes_per_cycle = 3;

    let dials = ExpansionPolicy::from_learning_policy(&policy);
    assert_eq!(dials.max_probes_per_cycle, 3);
    dials.validate().expect("the bridged dials stay honest");

    // The dials the policy has no vocabulary for keep their own values.
    let defaults = ExpansionPolicy::default();
    assert_eq!(
        dials.mastery_failure_threshold_millis,
        defaults.mastery_failure_threshold_millis
    );
    assert_eq!(dials.mastery_min_outcomes, defaults.mastery_min_outcomes);
    assert_eq!(dials.stability_window_secs, defaults.stability_window_secs);
    assert_eq!(dials.max_open_per_cycle, defaults.max_open_per_cycle);

    // The floor bridges to exactly the pre-policy frontier behavior.
    assert_eq!(
        ExpansionPolicy::from_learning_policy(&LearningPolicy::floor()),
        ExpansionPolicy::default()
    );
}
