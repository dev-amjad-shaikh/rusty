//! The goals plane suite: id determinism and convergence, the full
//! phase-machine matrix, revision and audit-trail integrity, persistence
//! round-trips and tamper refusal, round-cap accounting with the
//! auto-block attributed to the budget, and the tools' effects, schemas,
//! and provenance discipline.

use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, TimeZone, Utc};
use rusty_agent_runtime::goals::{
    parse_actor, CreateGoalTool, GetGoalTool, Goal, GoalAuditKind, GoalPhase, GoalProvenance,
    GoalStore, UpdateGoalTool, GOALS_FORMAT_VERSION, GOAL_ID_PREFIX, HARNESS_GOALS_PROVENANCE,
    MAX_GOAL_TRAIL_VIEW,
};
use rusty_agent_runtime::journal::Clock;
use rusty_agent_runtime::record::Effect;
use rusty_agent_runtime::tool::Tool;
use serde_json::{json, Value};

/// A fixed instant for every timestamp this suite injects.
fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 2, 9, 8, 0, 0).unwrap()
}

fn t1() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 2, 9, 9, 0, 0).unwrap()
}

fn t2() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 2, 9, 10, 0, 0).unwrap()
}

fn t3() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 2, 9, 11, 0, 0).unwrap()
}

fn temp_goals_path(tag: &str) -> PathBuf {
    std::env::temp_dir()
        .join(format!("rusty-goals-{tag}-{}", uuid::Uuid::new_v4()))
        .join("goals.json")
}

fn operator() -> GoalProvenance {
    GoalProvenance::Operator {
        operator: "ada".to_owned(),
    }
}

fn agent() -> GoalProvenance {
    GoalProvenance::Agent {
        agent: "rusty-1".to_owned(),
    }
}

fn goal(title: &str, description: &str, max_rounds: Option<u64>) -> Goal {
    Goal::new(title, description, operator(), max_rounds, t0()).unwrap()
}

/// Drive a goal into `phase` through legal moves, so matrix tests start
/// from real states rather than constructed ones.
async fn goal_in_phase(store: &GoalStore, goal: Goal, phase: GoalPhase) -> Goal {
    let (stored, inserted) = store.create(goal, "stating the objective").await.unwrap();
    assert!(inserted);
    match phase {
        GoalPhase::Active => stored,
        GoalPhase::Paused => store
            .transition(
                &stored.id,
                GoalPhase::Paused,
                operator(),
                "setting it aside",
                t1(),
            )
            .await
            .unwrap(),
        GoalPhase::Blocked => store
            .transition(
                &stored.id,
                GoalPhase::Blocked,
                operator(),
                "waiting on a dependency",
                t1(),
            )
            .await
            .unwrap(),
        GoalPhase::Complete => store
            .transition(
                &stored.id,
                GoalPhase::Complete,
                operator(),
                "achieved",
                t1(),
            )
            .await
            .unwrap(),
    }
}

// --------------------------------------------------------------------- //
// Identity: content-derived ids, determinism, convergence
// --------------------------------------------------------------------- //

#[test]
fn goal_ids_are_content_derived_and_deterministic() {
    let a = goal("ship the release", "cut, tag, and publish v2", None);
    let b = goal("ship the release", "cut, tag, and publish v2", None);
    assert_eq!(a.id, b.id, "same content, same id, across constructions");
    assert!(a.id.starts_with(GOAL_ID_PREFIX));
    assert_eq!(a.id.len(), GOAL_ID_PREFIX.len() + 64);

    let other_title = goal("ship the hotfix", "cut, tag, and publish v2", None);
    assert_ne!(a.id, other_title.id, "the title is part of the identity");
    let other_description = goal("ship the release", "publish the docs only", None);
    assert_ne!(
        a.id, other_description.id,
        "the description is part of the identity"
    );
}

#[tokio::test]
async fn recreating_a_goal_converges_without_mutating_it() {
    let store = GoalStore::open(temp_goals_path("converge")).await.unwrap();
    let (created, inserted) = store
        .create(
            goal("learn rust", "finish the book and the exercises", None),
            "why not",
        )
        .await
        .unwrap();
    assert!(inserted);
    assert_eq!(store.len(), 1);
    assert_eq!(store.trail().len(), 1);

    // Move it, then re-create the identical goal: the stored state wins.
    store
        .transition(&created.id, GoalPhase::Paused, operator(), "pausing", t1())
        .await
        .unwrap();
    let (again, inserted) = store
        .create(
            goal("learn rust", "finish the book and the exercises", None),
            "why not",
        )
        .await
        .unwrap();
    assert!(!inserted, "a re-creation converges, it does not insert");
    assert_eq!(
        again.phase,
        GoalPhase::Paused,
        "convergence never mutates the stored state"
    );
    assert_eq!(again.revision, 2);
    assert_eq!(store.len(), 1);
    assert_eq!(
        store.trail().len(),
        2,
        "no creation entry for a convergence"
    );
}

// --------------------------------------------------------------------- //
// The phase machine: the full matrix
// --------------------------------------------------------------------- //

#[tokio::test]
async fn the_full_transition_matrix() {
    let phases = [
        GoalPhase::Active,
        GoalPhase::Paused,
        GoalPhase::Blocked,
        GoalPhase::Complete,
    ];
    let legal = |from: GoalPhase, to: GoalPhase| match from {
        GoalPhase::Active => matches!(
            to,
            GoalPhase::Paused | GoalPhase::Blocked | GoalPhase::Complete
        ),
        GoalPhase::Paused | GoalPhase::Blocked => matches!(to, GoalPhase::Active),
        GoalPhase::Complete => false,
    };
    for from in phases {
        for to in phases {
            let store = GoalStore::open(temp_goals_path("matrix")).await.unwrap();
            let stored = goal_in_phase(
                &store,
                goal("matrix goal", "one cell of the machine", None),
                from,
            )
            .await;
            let outcome = store
                .transition(&stored.id, to, operator(), "matrix probe", t2())
                .await;
            if legal(from, to) {
                let moved = outcome.unwrap_or_else(|e| panic!("{from:?} → {to:?} must pass: {e}"));
                assert_eq!(moved.phase, to);
                assert_eq!(moved.revision, stored.revision + 1);
            } else {
                let error = outcome.unwrap_err().to_string();
                assert!(
                    error.contains(&format!("{from:?}")) && error.contains(&format!("{to:?}")),
                    "an illegal transition names both states, got: {error}"
                );
                assert_eq!(
                    store.get(&stored.id).unwrap().phase,
                    from,
                    "a refused transition changes nothing"
                );
            }
        }
    }
}

#[tokio::test]
async fn complete_is_terminal() {
    let store = GoalStore::open(temp_goals_path("terminal")).await.unwrap();
    let stored = goal_in_phase(
        &store,
        goal("finish the migration", "every tenant moved", None),
        GoalPhase::Complete,
    )
    .await;
    for to in [
        GoalPhase::Active,
        GoalPhase::Paused,
        GoalPhase::Blocked,
        GoalPhase::Complete,
    ] {
        assert!(
            store
                .transition(&stored.id, to, operator(), "reopening", t2())
                .await
                .is_err(),
            "complete stays complete ({to:?} refused)"
        );
    }
}

#[tokio::test]
async fn every_transition_needs_a_reason() {
    let store = GoalStore::open(temp_goals_path("reason")).await.unwrap();
    let (stored, _) = store
        .create(
            goal("document the api", "every public endpoint", None),
            "gaps everywhere",
        )
        .await
        .unwrap();
    for bad in ["", "  ", "has a\nnewline"] {
        assert!(
            store
                .transition(&stored.id, GoalPhase::Paused, operator(), bad, t1())
                .await
                .is_err(),
            "reason `{bad:?}` fails closed"
        );
    }
    assert_eq!(
        store.get(&stored.id).unwrap().phase,
        GoalPhase::Active,
        "refusals left the goal untouched"
    );
}

// --------------------------------------------------------------------- //
// Revisions and the audit trail
// --------------------------------------------------------------------- //

#[tokio::test]
async fn the_audit_trail_journals_every_revision() {
    let store = GoalStore::open(temp_goals_path("trail")).await.unwrap();
    let (created, _) = store
        .create(
            goal(
                "tune the evals",
                "raise the pass rate above the gate",
                Some(9),
            ),
            "the gate keeps failing",
        )
        .await
        .unwrap();
    store
        .transition(
            &created.id,
            GoalPhase::Paused,
            agent(),
            "waiting on data",
            t1(),
        )
        .await
        .unwrap();
    store
        .transition(
            &created.id,
            GoalPhase::Active,
            agent(),
            "data arrived",
            t2(),
        )
        .await
        .unwrap();
    let completed = store
        .transition(
            &created.id,
            GoalPhase::Complete,
            operator(),
            "gate passed",
            t3(),
        )
        .await
        .unwrap();
    assert_eq!(completed.revision, 4);

    let trail = store.trail_for(&created.id);
    assert_eq!(trail.len(), 4, "one entry per revision");

    assert_eq!(trail[0].kind, GoalAuditKind::Created);
    assert_eq!(trail[0].from, None);
    assert_eq!(trail[0].to, GoalPhase::Active);
    assert_eq!(trail[0].revision, 1);
    assert_eq!(trail[0].actor, operator());
    assert_eq!(trail[0].reason, "the gate keeps failing");
    assert_eq!(trail[0].at, t0());

    assert_eq!(trail[1].kind, GoalAuditKind::Transition);
    assert_eq!(trail[1].from, Some(GoalPhase::Active));
    assert_eq!(trail[1].to, GoalPhase::Paused);
    assert_eq!(trail[1].actor, agent());
    assert_eq!(trail[1].at, t1());

    assert_eq!(trail[2].from, Some(GoalPhase::Paused));
    assert_eq!(trail[2].to, GoalPhase::Active);
    assert_eq!(trail[2].revision, 3);

    assert_eq!(trail[3].to, GoalPhase::Complete);
    assert_eq!(trail[3].revision, 4);
    assert_eq!(trail[3].at, t3());

    // The revisions the trail records are exactly the goal's history:
    // strictly increasing by one, one entry each.
    for (index, entry) in trail.iter().enumerate() {
        assert_eq!(entry.revision, index as u64 + 1);
    }
}

// --------------------------------------------------------------------- //
// Persistence: round-trips, format refusal, tamper refusal
// --------------------------------------------------------------------- //

#[tokio::test]
async fn persistence_round_trip_preserves_goals_and_trail() {
    let path = temp_goals_path("round-trip");
    let (created, paused_at, trail_before) = {
        let store = GoalStore::open(&path).await.unwrap();
        let (a, _) = store
            .create(
                goal("write the book", "twelve chapters", Some(3)),
                "it is due",
            )
            .await
            .unwrap();
        let (b, _) = store
            .create(
                goal("run the race", "sub four hours", None),
                "training paid off",
            )
            .await
            .unwrap();
        let paused = store
            .transition(&b.id, GoalPhase::Paused, operator(), "an injury", t1())
            .await
            .unwrap();
        (a.id, paused, store.trail())
    };

    let reopened = GoalStore::open(&path).await.unwrap();
    assert_eq!(reopened.len(), 2);
    assert_eq!(reopened.trail(), trail_before, "the trail round-trips");
    let paused = reopened.get(&paused_at.id).unwrap();
    assert_eq!(paused.phase, GoalPhase::Paused);
    assert_eq!(paused.revision, 2);
    let fresh = reopened.get(&created).unwrap();
    assert_eq!(fresh.rounds_remaining, Some(3));

    // And the reopened store keeps accepting work against real state.
    let resumed = reopened
        .transition(&paused.id, GoalPhase::Active, operator(), "healed", t2())
        .await
        .unwrap();
    assert_eq!(resumed.phase, GoalPhase::Active);
    assert_eq!(resumed.revision, 3);
}

#[tokio::test]
async fn a_newer_format_version_is_refused_loudly() {
    let path = temp_goals_path("version");
    let store = GoalStore::open(&path).await.unwrap();
    store
        .create(
            goal("keep the lights on", "uptime above the slo", None),
            "ops",
        )
        .await
        .unwrap();

    let bytes = std::fs::read(&path).unwrap();
    let mut file: Value = serde_json::from_slice(&bytes).unwrap();
    file["format_version"] = json!(GOALS_FORMAT_VERSION + 1);
    std::fs::write(&path, serde_json::to_vec(&file).unwrap()).unwrap();

    let error = GoalStore::open(&path).await.unwrap_err().to_string();
    assert!(
        error.contains("format version"),
        "the refusal names the version, got: {error}"
    );
}

#[tokio::test]
async fn a_tampered_goal_fails_closed() {
    let path = temp_goals_path("tamper");
    let store = GoalStore::open(&path).await.unwrap();
    store
        .create(
            goal("audit the vendors", "every third party reviewed", None),
            "policy",
        )
        .await
        .unwrap();

    let bytes = std::fs::read(&path).unwrap();
    let mut file: Value = serde_json::from_slice(&bytes).unwrap();
    file["goals"][0]["title"] = json!("audit none of the vendors");
    std::fs::write(&path, serde_json::to_vec(&file).unwrap()).unwrap();

    let error = GoalStore::open(&path).await.unwrap_err().to_string();
    assert!(
        error.contains("does not match its contents"),
        "a content/id mismatch fails closed, got: {error}"
    );
}

#[tokio::test]
async fn an_orphaned_trail_entry_fails_closed() {
    let path = temp_goals_path("orphan");
    let store = GoalStore::open(&path).await.unwrap();
    let (created, _) = store
        .create(
            goal("map the estate", "every system catalogued", None),
            "unknowns abound",
        )
        .await
        .unwrap();

    let bytes = std::fs::read(&path).unwrap();
    let mut file: Value = serde_json::from_slice(&bytes).unwrap();
    let mut entry = file["trail"][0].clone();
    entry["goal_id"] = json!(format!("{created_id}-forged", created_id = created.id));
    file["trail"].as_array_mut().unwrap().push(entry);
    std::fs::write(&path, serde_json::to_vec(&file).unwrap()).unwrap();

    let error = GoalStore::open(&path).await.unwrap_err().to_string();
    assert!(
        error.contains("does not hold"),
        "a trail entry for a missing goal fails closed, got: {error}"
    );
}

// --------------------------------------------------------------------- //
// The round cap
// --------------------------------------------------------------------- //

#[tokio::test]
async fn the_round_cap_auto_blocks_at_zero() {
    let store = GoalStore::open(temp_goals_path("cap")).await.unwrap();
    let (created, _) = store
        .create(
            goal("index the corpus", "every document searchable", Some(2)),
            "recall matters",
        )
        .await
        .unwrap();

    let after_one = store
        .record_round(&created.id, agent(), "indexed the first shard", t1())
        .await
        .unwrap();
    assert_eq!(after_one.phase, GoalPhase::Active);
    assert_eq!(after_one.rounds_remaining, Some(1));
    assert_eq!(after_one.revision, 2);

    let after_two = store
        .record_round(&created.id, agent(), "indexed the second shard", t2())
        .await
        .unwrap();
    assert_eq!(
        after_two.phase,
        GoalPhase::Blocked,
        "the spent budget blocks the goal in the same call"
    );
    assert_eq!(after_two.rounds_remaining, Some(0));
    assert_eq!(
        after_two.revision, 4,
        "the spend and the auto-block both count"
    );

    let trail = store.trail_for(&created.id);
    assert_eq!(trail.len(), 4);
    let auto_block = &trail[3];
    assert_eq!(auto_block.kind, GoalAuditKind::AutoBlock);
    assert_eq!(auto_block.from, Some(GoalPhase::Active));
    assert_eq!(auto_block.to, GoalPhase::Blocked);
    assert_eq!(
        auto_block.actor.label(),
        HARNESS_GOALS_PROVENANCE,
        "the auto-block is attributed to the harness, never the caller"
    );
    assert!(
        auto_block.reason.contains('2'),
        "the cap is named as the reason: {}",
        auto_block.reason
    );
}

#[tokio::test]
async fn a_spent_budget_stays_spent() {
    let store = GoalStore::open(temp_goals_path("spent")).await.unwrap();
    let (created, _) = store
        .create(
            goal("clear the queue", "zero messages pending", Some(1)),
            "backlog is risk",
        )
        .await
        .unwrap();
    store
        .record_round(&created.id, agent(), "drained most of it", t1())
        .await
        .unwrap();

    // Blocked goals cannot spend; resuming does not mint new budget.
    assert!(store
        .record_round(&created.id, agent(), "one more", t2())
        .await
        .is_err());
    let resumed = store
        .transition(
            &created.id,
            GoalPhase::Active,
            operator(),
            "retry the drain",
            t2(),
        )
        .await
        .unwrap();
    let error = store
        .record_round(&resumed.id, agent(), "one more", t3())
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("spent its round cap of 1"),
        "a spent cap refuses further rounds, got: {error}"
    );
}

#[tokio::test]
async fn uncapped_and_inactive_goals_cannot_spend_rounds() {
    let store = GoalStore::open(temp_goals_path("uncapped")).await.unwrap();
    let (uncapped, _) = store
        .create(goal("stay curious", "keep reading", None), "growth")
        .await
        .unwrap();
    let error = store
        .record_round(&uncapped.id, agent(), "read a paper", t1())
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("no round cap"), "got: {error}");

    let (capped, _) = store
        .create(
            goal("close the books", "reconcile the quarter", Some(4)),
            "audit season",
        )
        .await
        .unwrap();
    store
        .transition(
            &capped.id,
            GoalPhase::Paused,
            operator(),
            "waiting on ledger",
            t1(),
        )
        .await
        .unwrap();
    let error = store
        .record_round(&capped.id, agent(), "work a paused goal", t2())
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("not active"),
        "only an active goal spends, got: {error}"
    );
}

#[test]
fn a_zero_cap_is_refused_at_construction() {
    assert!(
        Goal::new("impossible", "no budget at all", operator(), Some(0), t0()).is_err(),
        "a cap of zero budgets no work and fails closed"
    );
}

// --------------------------------------------------------------------- //
// Provenance
// --------------------------------------------------------------------- //

#[test]
fn the_provenance_vocabulary_is_closed() {
    assert_eq!(parse_actor("operator:ada").unwrap().label(), "operator:ada");
    assert_eq!(
        parse_actor("harness:goals").unwrap().label(),
        "harness:goals"
    );
    assert_eq!(
        parse_actor("agent:rusty-1").unwrap().label(),
        "agent:rusty-1"
    );
    for bad in ["system:root", "ada", "operator:", ":ada", "operator: "] {
        assert!(
            parse_actor(bad).is_err(),
            "actor `{bad}` is outside the vocabulary and fails closed"
        );
    }
}

// --------------------------------------------------------------------- //
// Serde vocabulary
// --------------------------------------------------------------------- //

#[test]
fn phases_round_trip_through_the_closed_vocabulary() {
    for (phase, wire) in [
        (GoalPhase::Active, "active"),
        (GoalPhase::Paused, "paused"),
        (GoalPhase::Blocked, "blocked"),
        (GoalPhase::Complete, "complete"),
    ] {
        assert_eq!(serde_json::to_value(phase).unwrap(), json!(wire));
        assert_eq!(
            serde_json::from_value::<GoalPhase>(json!(wire)).unwrap(),
            phase
        );
    }
    assert!(
        serde_json::from_value::<GoalPhase>(json!("on_hold")).is_err(),
        "an unknown phase fails at deserialization"
    );
}

// --------------------------------------------------------------------- //
// The tools
// --------------------------------------------------------------------- //

#[tokio::test]
async fn tool_effects_and_schemas_are_honest_and_closed() {
    let store = Arc::new(GoalStore::open(temp_goals_path("schema")).await.unwrap());
    let create = CreateGoalTool::new(Arc::clone(&store), Clock::logical(0, 1));
    let get = GetGoalTool::new(Arc::clone(&store));
    let update = UpdateGoalTool::new(store, Clock::logical(0, 1));

    assert_eq!(create.name(), "create_goal");
    assert_eq!(create.effect(), Effect::Idempotent);
    assert_eq!(get.name(), "get_goal");
    assert_eq!(get.effect(), Effect::ReadOnly);
    assert_eq!(update.name(), "update_goal");
    assert_eq!(update.effect(), Effect::Compensatable);

    for tool in [&create as &dyn Tool, &get, &update] {
        let schema = tool.parameters_schema();
        assert_eq!(
            schema["additionalProperties"],
            json!(false),
            "{} takes a closed schema",
            tool.name()
        );
    }
    assert_eq!(
        update.parameters_schema()["properties"]["action"]["enum"],
        json!(["pause", "resume", "block", "complete", "record_round"]),
        "the action vocabulary is closed"
    );
}

#[tokio::test]
async fn create_goal_is_idempotent_under_its_content_id() {
    let store = Arc::new(
        GoalStore::open(temp_goals_path("tool-create"))
            .await
            .unwrap(),
    );
    let create = CreateGoalTool::new(
        Arc::clone(&store),
        Clock::logical(1_700_000_000_000, 60_000),
    );
    let args = json!({
        "title": "stabilize the pipeline",
        "description": "no flaky tests on main",
        "actor": "operator:ada",
        "reason": "flakes erode trust",
        "max_rounds": 5
    });
    let first = create.call(args.clone()).await.unwrap();
    assert_eq!(first["inserted"], json!(true));
    assert_eq!(first["phase"], json!("active"));
    assert_eq!(first["revision"], json!(1));
    assert_eq!(first["rounds_remaining"], json!(5));
    assert_eq!(first["provenance"], json!("operator:ada"));

    let second = create.call(args).await.unwrap();
    assert_eq!(second["inserted"], json!(false));
    assert_eq!(second["id"], first["id"], "same content, same goal");
    assert_eq!(store.len(), 1);
}

#[tokio::test]
async fn create_goal_refuses_an_actor_outside_the_vocabulary() {
    let store = Arc::new(
        GoalStore::open(temp_goals_path("tool-actor"))
            .await
            .unwrap(),
    );
    let create = CreateGoalTool::new(store.clone(), Clock::logical(0, 1));
    let error = create
        .call(json!({
            "title": "sneak in",
            "description": "an unattributable goal",
            "actor": "system:root",
            "reason": "no origin"
        }))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("provenance vocabulary"), "got: {error}");
    assert!(store.is_empty(), "nothing was written");
}

#[tokio::test]
async fn get_goal_reads_and_bounds_the_trail() {
    let store = Arc::new(GoalStore::open(temp_goals_path("tool-get")).await.unwrap());
    let (created, _) = store
        .create(
            goal("map the coast", "every harbor charted", Some(200)),
            "storms coming",
        )
        .await
        .unwrap();
    for index in 0..150u64 {
        store
            .record_round(
                &created.id,
                agent(),
                format!("charted harbor {index}"),
                t1(),
            )
            .await
            .unwrap();
    }

    let get = GetGoalTool::new(Arc::clone(&store));
    let view = get
        .call(json!({"goal_id": created.id, "include_trail": true}))
        .await
        .unwrap();
    assert_eq!(view["trail_total"], json!(151));
    let trail = view["trail"].as_array().unwrap();
    assert_eq!(
        trail.len(),
        MAX_GOAL_TRAIL_VIEW,
        "the rendered trail is bounded"
    );
    assert_eq!(
        trail[0]["revision"],
        json!(151),
        "the view shows the most recent entries first"
    );

    let error = get
        .call(json!({"goal_id": "goal-does-not-exist"}))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("unknown goal"), "got: {error}");
}

#[tokio::test]
async fn update_goal_moves_and_journals() {
    let store = Arc::new(
        GoalStore::open(temp_goals_path("tool-update"))
            .await
            .unwrap(),
    );
    let create = CreateGoalTool::new(Arc::clone(&store), Clock::logical(0, 1000));
    let created = create
        .call(json!({
            "title": "provision the fleet",
            "description": "every node enrolled",
            "actor": "operator:ada",
            "reason": "capacity",
            "max_rounds": 1
        }))
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap();

    let update = UpdateGoalTool::new(Arc::clone(&store), Clock::logical(0, 1000));
    let paused = update
        .call(json!({
            "goal_id": id,
            "action": "pause",
            "actor": "operator:ada",
            "reason": "waiting on hardware"
        }))
        .await
        .unwrap();
    assert_eq!(paused["phase"], json!("paused"));
    assert_eq!(paused["revision"], json!(2));

    let resumed = update
        .call(json!({
            "goal_id": id,
            "action": "resume",
            "actor": "agent:rusty-1",
            "reason": "hardware landed"
        }))
        .await
        .unwrap();
    assert_eq!(resumed["phase"], json!("active"));

    // The last round auto-blocks, attributed to the harness.
    let spent = update
        .call(json!({
            "goal_id": id,
            "action": "record_round",
            "actor": "agent:rusty-1",
            "reason": "enrolled the first batch"
        }))
        .await
        .unwrap();
    assert_eq!(spent["phase"], json!("blocked"));
    assert_eq!(spent["rounds_remaining"], json!(0));

    let trail = store.trail_for(id);
    assert_eq!(trail.last().unwrap().kind, GoalAuditKind::AutoBlock);

    // An unknown action fails closed naming the vocabulary.
    let error = update
        .call(json!({
            "goal_id": id,
            "action": "reopen",
            "actor": "operator:ada",
            "reason": "not a verb"
        }))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("unknown action"), "got: {error}");

    // And an illegal move names both states.
    let error = update
        .call(json!({
            "goal_id": id,
            "action": "complete",
            "actor": "operator:ada",
            "reason": "declare victory anyway"
        }))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("Blocked") && error.contains("Complete"),
        "got: {error}"
    );
}
