//! Executor-policy plane integration tests (R0.8 Rusty Learn, wave 4).
//!
//! Three test groups:
//!
//! - **Golden files** — the serialized shapes of `ExecutorPolicy` (the
//!   static floor and a fully-specified specimen) and of a retry
//!   `DecisionEvent` as built by `retry_decision_event` are pinned against
//!   checked-in JSON under `tests/golden/`. The new `RunEventKind` variant's
//!   wire name is pinned in `policy_event_kinds.json` (the
//!   `learn_event_kinds.json` pattern; the exhaustive `run_event_kind.json`
//!   list is owned by `tests/agents.rs`, outside this stream's file scope).
//!   Any accidental contract drift fails here. To bless an intentional
//!   contract change, re-run with `UPDATE_GOLDEN=1` and review the diff.
//! - **Emission semantics** — the legal set mirrors `classify_retry`'s
//!   gates, the selected action stays inside it, the deterministic v1
//!   propensity is recorded honestly as 1.0, and a journaled
//!   `PolicyDecision` event round-trips the journal's integrity check.
//! - **Admission binding at the executor seam** — a resumed run keeps the
//!   policy version its checkpoint header pins (mid-run promotions do not
//!   leak into in-flight runs), an explicit `RunConfig` pin wins over
//!   inheritance, and an unpinned fresh run records the static floor.

use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::json;

use rusty_agent_runtime::checkpoint::{Checkpointer, InMemoryCheckpointer};
use rusty_agent_runtime::durable::{classify_retry, retry_decision_event, ErrorClass};
use rusty_agent_runtime::executor::{ExecutionOutcome, Executor, RunConfig};
use rusty_agent_runtime::graph::GraphBuilder;
use rusty_agent_runtime::journal::{Clock, EventDraft, Journal};
use rusty_agent_runtime::node::{NodeContext, NodeOutput};
use rusty_agent_runtime::record::{
    derive_policy_version, DecisionAction, DecisionFamily, Effect, ExecutorPolicy, PayloadRef,
    PolicyVersion, RunEventKind,
};
use rusty_agent_runtime::state::{Reducer, State, StateSpec};

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

// ---------- golden pins ----------

#[test]
fn golden_executor_policy_static_v0() {
    assert_golden(
        "executor_policy_static_v0.json",
        &ExecutorPolicy::static_v0(),
    );
}

#[test]
fn golden_executor_policy_specimen() {
    // A fully-specified non-floor policy: every family set, so the pin
    // covers every field the contract carries.
    let specimen = ExecutorPolicy::static_v0()
        .with_family_parameters(
            DecisionFamily::Retry,
            json!({"base_delay_ms": 500, "max_delay_ms": 60_000, "max_attempts": 5}),
        )
        .unwrap()
        .with_family_parameters(
            DecisionFamily::Timeout,
            json!({"default_millis": 30_000, "max_millis": 120_000}),
        )
        .unwrap()
        .with_family_parameters(DecisionFamily::Concurrency, json!({"max_parallel": 8}))
        .unwrap();
    assert_golden("executor_policy.json", &specimen);
}

#[test]
fn golden_policy_decision_event() {
    // The event exactly as the emission seam builds it: the classifier's
    // decision for a retryable timeout on attempt 1, floor policy.
    let decision = classify_retry(Effect::Idempotent, ErrorClass::Timeout, 1, 3, 0.5);
    let event = retry_decision_event(
        "019157c4-6f1f-7a3b-8c2d-9e4f5a6b7c8d",
        "thread-42",
        0,
        Effect::Idempotent,
        ErrorClass::Timeout,
        1,
        3,
        Some(840),
        &decision,
        &PolicyVersion::default(),
        ts(1_750_000_000_000),
    );
    assert_golden("policy_decision_event.json", &event);
}

#[test]
fn golden_policy_event_kinds() {
    // The wave's new RunEventKind wire name (the `learn_event_kinds.json`
    // pattern: each wave pins its own additions).
    let kinds: Vec<String> = [RunEventKind::PolicyDecision]
        .iter()
        .map(|kind| {
            serde_json::to_value(kind)
                .unwrap()
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_golden("policy_event_kinds.json", &kinds);
}

// ---------- emission semantics ----------

#[test]
fn retry_decision_event_marks_the_policy_that_decided() {
    // A promoted (non-floor) policy version is what learning evidence needs:
    // the event names the decider, and the version derives from content.
    let promoted = ExecutorPolicy::static_v0()
        .with_family_parameters(
            DecisionFamily::Retry,
            json!({"base_delay_ms": 500, "max_delay_ms": 60_000, "max_attempts": 5}),
        )
        .unwrap();
    let version = derive_policy_version(&promoted).unwrap();
    let decision = classify_retry(
        Effect::Idempotent,
        ErrorClass::DependencyFailure,
        2,
        promoted.retry.max_attempts,
        0.25,
    );
    let event = retry_decision_event(
        "run-1",
        "thread-1",
        1,
        Effect::Idempotent,
        ErrorClass::DependencyFailure,
        2,
        promoted.retry.max_attempts,
        None,
        &decision,
        &version,
        ts(1_750_000_000_000),
    );
    assert_eq!(event.policy_version, version);
    assert_eq!(event.selected, DecisionAction::Retry { attempt: 3 });
    assert_eq!(event.propensity, 1.0);
}

#[test]
fn journaled_policy_decision_round_trips_the_integrity_check() {
    // The DecisionEvent travels as a `PolicyDecision` event's output
    // payload; a journal rebuilt from the snapshot must agree with itself.
    let journal = Journal::new("run-1", "thread-1", Clock::System);
    let decision = classify_retry(Effect::Idempotent, ErrorClass::Timeout, 1, 3, 0.5);
    let event = retry_decision_event(
        "run-1",
        "thread-1",
        0,
        Effect::Idempotent,
        ErrorClass::Timeout,
        1,
        3,
        Some(840),
        &decision,
        &PolicyVersion::default(),
        ts(1_750_000_000_000),
    );
    let event_id = journal.record(
        EventDraft::new(RunEventKind::PolicyDecision, Effect::Pure)
            .output(serde_json::to_value(&event).unwrap()),
    );
    assert_eq!(event_id, "run-1:0");

    let snapshot = journal.snapshot();
    let rebuilt = Journal::from_snapshot(snapshot, Clock::System).unwrap();
    let recorded = &rebuilt.events()[0];
    assert_eq!(recorded.kind, RunEventKind::PolicyDecision);
    assert_eq!(recorded.effect, Effect::Pure);
    let inline = match recorded.output.as_ref().expect("decision payload") {
        PayloadRef::Inline(value) => value,
        other => panic!("a decision payload fits inline: {other:?}"),
    };
    assert_eq!(inline["family"], json!("retry"));
    assert_eq!(inline["propensity"], json!(1.0));
    assert_eq!(inline["policy_version"], json!("static-v0"));
}

// ---------- admission binding at the executor seam ----------

/// A gate node that interrupts on first contact and writes the resume value
/// when resumed; the tail gives the resumed run a second super-step so a
/// boundary checkpoint is minted after the resume.
fn gate_graph() -> (rusty_agent_runtime::graph::Graph, StateSpec) {
    let spec = StateSpec::new().channel("answer", Reducer::Overwrite);
    let mut builder = GraphBuilder::new();
    builder.add_node("gate", |ctx: NodeContext| async move {
        match ctx.resume_value() {
            Some(v) => Ok(NodeOutput::update("answer", v.clone())),
            None => Err(ctx.interrupt(json!({"question": "approve?"}))),
        }
    });
    builder.add_node("tail", |ctx: NodeContext| async move {
        let answer = ctx.state().get("answer").cloned().unwrap_or_default();
        Ok(NodeOutput::update("answer", answer))
    });
    builder.set_entry_point("gate");
    builder.add_edge("gate", "tail");
    (builder.compile().unwrap(), spec)
}

#[tokio::test]
async fn resumed_run_keeps_the_pinned_policy_version() {
    let (graph, spec) = gate_graph();
    let checkpointer = Arc::new(InMemoryCheckpointer::new());
    let executor = Executor::with_checkpointer(checkpointer.clone());
    let pinned = PolicyVersion::new("policy-aaa");

    // First run: pinned to version A, suspends at the gate.
    let outcome = executor
        .run(
            &graph,
            &spec,
            State::new(),
            RunConfig::new("t-bound").with_policy_version(pinned.clone()),
        )
        .await
        .unwrap();
    assert!(outcome.is_interrupted(), "expected suspension: {outcome:?}");

    // Resume WITHOUT a policy pin: the run must keep version A — a mid-run
    // promotion (to anything else) does not leak into in-flight runs.
    let outcome = executor
        .run(
            &graph,
            &spec,
            State::new(),
            RunConfig::new("t-bound").with_resume(json!(true)),
        )
        .await
        .unwrap();
    assert!(matches!(outcome, ExecutionOutcome::Done(_)));

    let checkpoints = checkpointer.list("t-bound").await.unwrap();
    assert!(
        checkpoints.len() >= 2,
        "suspension + post-resume boundary checkpoints: {}",
        checkpoints.len()
    );
    for checkpoint in &checkpoints {
        assert_eq!(
            checkpoint.header.policy_version, pinned,
            "every checkpoint of the run — including those minted after the \
             unpinned resume — keeps the bound version"
        );
    }
}

#[tokio::test]
async fn explicit_pin_on_resume_wins_over_inheritance() {
    let (graph, spec) = gate_graph();
    let checkpointer = Arc::new(InMemoryCheckpointer::new());
    let executor = Executor::with_checkpointer(checkpointer.clone());

    let outcome = executor
        .run(
            &graph,
            &spec,
            State::new(),
            RunConfig::new("t-repinned").with_policy_version(PolicyVersion::new("policy-aaa")),
        )
        .await
        .unwrap();
    assert!(outcome.is_interrupted(), "expected suspension: {outcome:?}");

    // An explicit pin on the resume is a deliberate operator act (e.g.
    // re-driving under the floor); it wins over the checkpoint's pin.
    let outcome = executor
        .run(
            &graph,
            &spec,
            State::new(),
            RunConfig::new("t-repinned")
                .with_resume(json!(true))
                .with_policy_version(PolicyVersion::default()),
        )
        .await
        .unwrap();
    assert!(matches!(outcome, ExecutionOutcome::Done(_)));

    let checkpoints = checkpointer.list("t-repinned").await.unwrap();
    let latest = checkpoints.last().unwrap();
    assert_eq!(
        latest.header.policy_version,
        PolicyVersion::default(),
        "the explicit floor pin wins over the inherited version"
    );
    assert_eq!(
        checkpoints[0].header.policy_version,
        PolicyVersion::new("policy-aaa"),
        "the suspension checkpoint keeps the version it was written under"
    );
}

#[tokio::test]
async fn unpinned_fresh_run_records_the_static_floor() {
    let (graph, spec) = gate_graph();
    let checkpointer = Arc::new(InMemoryCheckpointer::new());
    let executor = Executor::with_checkpointer(checkpointer.clone());

    let outcome = executor
        .run(&graph, &spec, State::new(), RunConfig::new("t-floor"))
        .await
        .unwrap();
    assert!(outcome.is_interrupted(), "expected suspension: {outcome:?}");

    let stored = checkpointer.get_latest("t-floor").await.unwrap().unwrap();
    assert_eq!(stored.header.policy_version, PolicyVersion::default());
}
