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
use rusty_agent_runtime::durable::{
    classify_retry, classify_retry_with_policy, resolve_retry_parameters, resolve_timeout_bound_ms,
    retry_decision_event, timeout_decision_event, timeout_selected_action, ErrorClass,
    LatencyPercentiles, ResolvedRetryParameters,
};
use rusty_agent_runtime::executor::{ExecutionOutcome, Executor, RunConfig};
use rusty_agent_runtime::graph::GraphBuilder;
use rusty_agent_runtime::journal::{Clock, EventDraft, Journal};
use rusty_agent_runtime::node::{NodeContext, NodeOutput};
use rusty_agent_runtime::record::{
    derive_policy_version, DecisionAction, DecisionFamily, Effect, ExecutorPolicy, PayloadRef,
    PolicyVersion, RunEventKind,
};
use rusty_agent_runtime::state::{Reducer, State, StateSpec};
use rusty_agent_runtime::twin::{ParameterizedPolicy, Twin, TwinRunConfig, DEFAULT_TIMEOUT_LADDER};

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

// --------------------------------------------------------------------- //
// R0.10 wave 3: learned retry/timeout parameter contracts and the
// application loop
//
// Four test groups:
//
// - **Golden files** — the serialized shapes of learned (non-floor)
//   retry and timeout policies and of a timeout `DecisionEvent` are
//   pinned under `tests/golden/`. The wave-4 goldens stay untouched:
//   the learned fields are additive and absent from the wire when
//   unset, so the floor's shape is byte-stable.
// - **Envelope validation** — out-of-envelope parameters are rejected
//   by `with_family_parameters` (the promotion gate's parse path), so a
//   malformed candidate can never become an active policy; a hand-built
//   invalid policy that somehow reaches a decision point steers
//   nothing — the resolution falls back to the floor.
// - **The application loop** — `resolve_retry_parameters` /
//   `resolve_timeout_bound_ms` read promoted parameters into decisions
//   (per-class schedules, narrowed budgets, per-callee bounds), the
//   floor deciding when the version names no override; and
//   `ParameterizedPolicy` steers twin runs with the same resolutions.
// - **Revert fidelity** — a floor-parameterized `ParameterizedPolicy`
//   re-executes a recorded run byte-identically to the `StaticFloor`:
//   revert-to-default restores exact static-v0 behavior.
// --------------------------------------------------------------------- //

/// A learned retry policy: the floor's flat schedule plus a per-class
/// entry for `Unknown` (faster recovery, narrowed budget).
fn learned_retry_policy() -> ExecutorPolicy {
    ExecutorPolicy::static_v0()
        .with_family_parameters(
            DecisionFamily::Retry,
            json!({
                "base_delay_ms": 1_000,
                "max_delay_ms": 300_000,
                "max_attempts": 3,
                "per_class": {
                    "unknown": {"base_delay_ms": 250, "max_delay_ms": 30_000, "max_attempts": 2},
                },
            }),
        )
        .unwrap()
}

/// A recorded run for the twin-driven tests: one step with a flaky call
/// (the recording holds an error, which the twin re-observes as
/// `Unknown`), a slow call (10 s completion — the timeout family's
/// target), and a fast one. Synthesized through the journal's own
/// recording path, exactly as a production recording would arrive.
fn recorded_snapshot() -> rusty_agent_runtime::journal::JournalSnapshot {
    let journal = Journal::new(
        "run-learned",
        "thread-learned",
        Clock::logical(1_700_000_000_000, 10),
    );
    let step = journal.record(
        EventDraft::new(RunEventKind::SuperStepStart, Effect::Pure)
            .input(json!({"step": 0, "active_nodes": ["flaky", "slow", "fast"]})),
    );
    journal.record(
        EventDraft::new(RunEventKind::ToolCall, Effect::Idempotent)
            .node("flaky")
            .input(json!({"tool": "flaky", "arguments": {}}))
            .output(json!({"error": "connection reset"}))
            .latency_ms(100)
            .status(rusty_agent_runtime::record::EventStatus::Error)
            .parent(&step),
    );
    journal.record(
        EventDraft::new(RunEventKind::ToolCall, Effect::Idempotent)
            .node("slow")
            .input(json!({"tool": "slow", "arguments": {}}))
            .output(json!({"result": "ok"}))
            .latency_ms(10_000)
            .cost_usd(0.002)
            .parent(&step),
    );
    journal.record(
        EventDraft::new(RunEventKind::ToolCall, Effect::Idempotent)
            .node("fast")
            .input(json!({"tool": "fast", "arguments": {}}))
            .output(json!({"result": "ok"}))
            .latency_ms(50)
            .parent(&step),
    );
    journal.record(
        EventDraft::new(RunEventKind::SuperStepEnd, Effect::Pure)
            .output(json!({"done": true}))
            .parent(&step),
    );
    journal.snapshot()
}

#[test]
fn golden_learned_retry_policy() {
    // The learned shape: the flat schedule plus a per-class table. New
    // golden file — the wave-4 pins are untouched.
    assert_golden(
        "executor_policy_learned_retry.json",
        &learned_retry_policy(),
    );
}

#[test]
fn golden_learned_timeout_policy() {
    let policy = ExecutorPolicy::static_v0()
        .with_family_parameters(
            DecisionFamily::Timeout,
            json!({
                "max_millis": 300_000,
                "per_callee": {"search": 5_000, "embed": 1_000},
            }),
        )
        .unwrap();
    assert_golden("executor_policy_learned_timeout.json", &policy);
}

#[test]
fn golden_timeout_decision_event() {
    // The timeout family's emission point, decided with a promoted
    // per-callee bound honored by the smallest covering rung.
    let policy = ExecutorPolicy::static_v0()
        .with_family_parameters(
            DecisionFamily::Timeout,
            json!({"max_millis": 300_000, "per_callee": {"search": 5_000}}),
        )
        .unwrap();
    let version = derive_policy_version(&policy).unwrap();
    let percentiles = LatencyPercentiles {
        p50_ms: 820,
        p95_ms: 4_100,
        p99_ms: 7_300,
        samples: 64,
    };
    let event = timeout_decision_event(
        "019157c4-6f1f-7a3b-8c2d-9e4f5a6b7c8d",
        "thread-42",
        2,
        Some("search"),
        Effect::Idempotent,
        1,
        Some(&percentiles),
        Some(5_000),
        &DEFAULT_TIMEOUT_LADDER,
        &version,
        ts(1_750_000_000_000),
    );
    assert_golden("timeout_decision_event.json", &event);
}

// ---------- envelope validation ----------

#[test]
fn out_of_envelope_parameters_are_rejected_at_the_gate() {
    let base = ExecutorPolicy::static_v0();

    // Backoff: zero base, base above cap, cap past the envelope, budget
    // past the envelope.
    for parameters in [
        json!({"base_delay_ms": 0, "max_delay_ms": 1_000, "max_attempts": 3}),
        json!({"base_delay_ms": 5_000, "max_delay_ms": 1_000, "max_attempts": 3}),
        json!({"base_delay_ms": 1_000, "max_delay_ms": 7_200_000, "max_attempts": 3}),
        json!({"base_delay_ms": 1_000, "max_delay_ms": 300_000, "max_attempts": 42}),
    ] {
        let err = base
            .with_family_parameters(DecisionFamily::Retry, parameters)
            .unwrap_err();
        assert!(err.to_string().contains("outside the envelope"), "{err}");
    }
    // One bad per-class entry condemns the whole set.
    let err = base
        .with_family_parameters(
            DecisionFamily::Retry,
            json!({
                "base_delay_ms": 1_000,
                "max_delay_ms": 300_000,
                "max_attempts": 3,
                "per_class": {"timeout": {"base_delay_ms": 500, "max_delay_ms": 100, "max_attempts": 2}},
            }),
        )
        .unwrap_err();
    assert!(err.to_string().contains("outside the envelope"), "{err}");

    // Timeout: below the minimum rung, default above the ceiling, a
    // per-callee entry above the ceiling.
    for parameters in [
        json!({"default_millis": 50}),
        json!({"default_millis": 200_000, "max_millis": 100_000}),
        json!({"max_millis": 10_000, "per_callee": {"search": 30_000}}),
    ] {
        let err = base
            .with_family_parameters(DecisionFamily::Timeout, parameters)
            .unwrap_err();
        assert!(err.to_string().contains("outside the envelope"), "{err}");
    }

    // Concurrency: zero admits no work at all.
    let err = base
        .with_family_parameters(DecisionFamily::Concurrency, json!({"max_parallel": 0}))
        .unwrap_err();
    assert!(err.to_string().contains("outside the envelope"), "{err}");
}

#[test]
fn invalid_policy_at_a_decision_point_steers_nothing() {
    // A policy built by hand (bypassing the gate) with an out-of-envelope
    // schedule: the resolution falls back to the floor, for retry…
    let mut invalid = ExecutorPolicy::static_v0();
    invalid.retry.base_delay_ms = 9_000;
    invalid.retry.max_delay_ms = 100; // base > cap: invalid
    assert!(invalid.retry.validate().is_err());
    let resolved = resolve_retry_parameters(&invalid, ErrorClass::Transient, 5);
    assert_eq!(resolved, ResolvedRetryParameters::floor(5));
    // …and for timeout.
    let mut invalid_timeout = ExecutorPolicy::static_v0();
    invalid_timeout.timeout.default_millis = Some(10); // below the minimum rung
    assert_eq!(
        resolve_timeout_bound_ms(&invalid_timeout, Some("search")),
        None
    );
}

// ---------- the application loop ----------

#[test]
fn resolve_retry_parameters_narrows_budgets_and_falls_back_to_the_floor() {
    // The floor resolves to the task's own budget and the module
    // constants — byte-identical to pre-learning behavior.
    let floor = ExecutorPolicy::static_v0();
    assert_eq!(
        resolve_retry_parameters(&floor, ErrorClass::Unknown, 5),
        ResolvedRetryParameters::floor(5)
    );

    // A per-class entry drives its class (narrowing the task's budget)
    // and leaves the others to the flat schedule.
    let learned = learned_retry_policy();
    let resolved = resolve_retry_parameters(&learned, ErrorClass::Unknown, 5);
    assert_eq!(resolved.base_delay_ms, 250);
    assert_eq!(resolved.max_delay_ms, 30_000);
    assert_eq!(resolved.max_attempts, 2, "min(learned 2, task 5)");
    let resolved = resolve_retry_parameters(&learned, ErrorClass::Unknown, 1);
    assert_eq!(
        resolved.max_attempts, 1,
        "a learned budget never widens the task's"
    );
    // A class without an entry: the flat schedule is the floor's here, so
    // the resolution is the floor's.
    assert_eq!(
        resolve_retry_parameters(&learned, ErrorClass::Transient, 4),
        ResolvedRetryParameters::floor(4)
    );

    // A flat learned schedule applies when no per-class entry exists, and
    // narrows the same way.
    let flat = ExecutorPolicy::static_v0()
        .with_family_parameters(
            DecisionFamily::Retry,
            json!({"base_delay_ms": 500, "max_delay_ms": 60_000, "max_attempts": 2}),
        )
        .unwrap();
    let resolved = resolve_retry_parameters(&flat, ErrorClass::Timeout, 3);
    assert_eq!(resolved.base_delay_ms, 500);
    assert_eq!(resolved.max_attempts, 2);
}

#[test]
fn classify_retry_with_policy_matches_the_floor_byte_for_byte() {
    // Property: for every input the classifier can see, deciding through
    // the floor's resolution IS `classify_retry` — the R0.5/R0.8
    // contracts do not move.
    let effects = [
        Effect::Pure,
        Effect::ReadOnly,
        Effect::Idempotent,
        Effect::NonIdempotent,
        Effect::Compensatable,
    ];
    let classes = [
        ErrorClass::Transient,
        ErrorClass::RateLimited,
        ErrorClass::Timeout,
        ErrorClass::InvalidInput,
        ErrorClass::DependencyFailure,
        ErrorClass::ResourceExhausted,
        ErrorClass::Cancelled,
        ErrorClass::Unknown,
    ];
    for effect in effects {
        for class in classes {
            for (attempt, max_attempts) in [(1, 3), (2, 3), (3, 3), (1, 0), (7, 5)] {
                for uniform in [0.0, 0.42, 0.999] {
                    assert_eq!(
                        classify_retry(effect, class, attempt, max_attempts, uniform),
                        classify_retry_with_policy(
                            effect,
                            class,
                            attempt,
                            &ResolvedRetryParameters::floor(max_attempts),
                            uniform,
                        ),
                        "{effect:?}/{class:?} attempt {attempt}/{max_attempts} @ {uniform}"
                    );
                }
            }
        }
    }
}

#[test]
fn promoted_parameters_steer_the_retry_decision() {
    let learned = learned_retry_policy();
    // The learned Unknown schedule: base 250, cap 30 s, budget 2 — the
    // delay draws from the learned schedule (bounded by base at the first
    // retry), and the budget dead-letters one attempt earlier than the
    // floor's three.
    let resolved = resolve_retry_parameters(&learned, ErrorClass::Unknown, 3);
    match classify_retry_with_policy(Effect::Idempotent, ErrorClass::Unknown, 1, &resolved, 1.0) {
        rusty_agent_runtime::durable::RetryDecision::Retry { after_ms } => {
            assert_eq!(after_ms, 250, "uniform 1.0 draws the learned base");
        }
        other => panic!("expected Retry, got {other:?}"),
    }
    assert_eq!(
        classify_retry_with_policy(Effect::Idempotent, ErrorClass::Unknown, 2, &resolved, 0.5),
        rusty_agent_runtime::durable::RetryDecision::Dead,
        "the learned budget (2) dead-letters where the floor (3) would retry"
    );
    // The gates never move: a non-repeatable effect fails immediately
    // under any parameters.
    assert_eq!(
        classify_retry_with_policy(
            Effect::NonIdempotent,
            ErrorClass::Unknown,
            1,
            &resolved,
            0.5
        ),
        rusty_agent_runtime::durable::RetryDecision::Fail
    );
}

#[test]
fn resolve_timeout_bound_ms_reads_the_policy_and_fails_closed() {
    // The floor: no bound in force.
    let floor = ExecutorPolicy::static_v0();
    assert_eq!(resolve_timeout_bound_ms(&floor, Some("search")), None);

    let learned = ExecutorPolicy::static_v0()
        .with_family_parameters(
            DecisionFamily::Timeout,
            json!({
                "default_millis": 30_000,
                "max_millis": 60_000,
                "per_callee": {"search": 5_000},
            }),
        )
        .unwrap();
    // A callee with an entry resolves to it; others resolve to the
    // default; a hand-built out-of-ceiling value clamps defensively.
    assert_eq!(
        resolve_timeout_bound_ms(&learned, Some("search")),
        Some(5_000)
    );
    assert_eq!(
        resolve_timeout_bound_ms(&learned, Some("embed")),
        Some(30_000)
    );
    assert_eq!(resolve_timeout_bound_ms(&learned, None), Some(30_000));
    let mut clamped = ExecutorPolicy::static_v0();
    clamped.timeout.default_millis = Some(45_000);
    clamped.timeout.max_millis = Some(60_000);
    clamped.timeout.per_callee = Some(std::collections::BTreeMap::from([(
        "search".to_owned(),
        90_000,
    )]));
    // 90 s exceeds the declared ceiling — validation rejects it, so the
    // resolution fails closed to no bound at all.
    assert_eq!(resolve_timeout_bound_ms(&clamped, Some("search")), None);
}

#[test]
fn timeout_selected_action_covers_the_bound_and_the_floor_takes_the_top_rung() {
    let ladder = DEFAULT_TIMEOUT_LADDER;
    // The smallest covering rung honors the bound; rounding down would
    // silently tighten it.
    assert_eq!(
        timeout_selected_action(Some(4_999), &ladder),
        DecisionAction::SetTimeout { millis: 5_000 }
    );
    assert_eq!(
        timeout_selected_action(Some(5_000), &ladder),
        DecisionAction::SetTimeout { millis: 5_000 }
    );
    // No bound in force: the top rung — the floor's stance, modeled
    // exactly as Wave 1 modeled it.
    assert_eq!(
        timeout_selected_action(None, &ladder),
        DecisionAction::SetTimeout { millis: 300_000 }
    );
}

#[test]
fn timeout_decision_event_records_the_acting_policy_and_closed_legal_set() {
    let policy = ExecutorPolicy::static_v0()
        .with_family_parameters(
            DecisionFamily::Timeout,
            json!({"max_millis": 300_000, "per_callee": {"search": 5_000}}),
        )
        .unwrap();
    let version = derive_policy_version(&policy).unwrap();
    let event = timeout_decision_event(
        "run-9",
        "thread-2",
        3,
        Some("search"),
        Effect::Idempotent,
        2,
        None,
        Some(5_000),
        &DEFAULT_TIMEOUT_LADDER,
        &version,
        ts(1_750_000_000_000),
    );
    assert_eq!(event.id, "run-9:d3");
    assert_eq!(event.family, DecisionFamily::Timeout);
    assert_eq!(event.selected, DecisionAction::SetTimeout { millis: 5_000 });
    assert!(event.legal_actions.contains(&event.selected));
    assert_eq!(event.propensity, 1.0, "deterministic acting policy");
    assert_eq!(event.policy_version, version);
    assert!(
        event.outcome.is_none(),
        "the bound's outcome is the operation's own evidence"
    );
    assert_eq!(event.features.get("callee"), Some(&json!("search")));
    assert_eq!(event.features.get("bound_ms"), Some(&json!(5_000)));

    // The floor's decision: top rung selected, no bound on the wire.
    let floor_event = timeout_decision_event(
        "run-9",
        "thread-2",
        4,
        Some("embed"),
        Effect::Idempotent,
        1,
        None,
        None,
        &DEFAULT_TIMEOUT_LADDER,
        &PolicyVersion::default(),
        ts(1_750_000_000_000),
    );
    assert_eq!(
        floor_event.selected,
        DecisionAction::SetTimeout { millis: 300_000 }
    );
    assert_eq!(
        floor_event.features.get("bound_ms"),
        Some(&serde_json::Value::Null)
    );
}

// ---------- the application loop in the twin ----------

#[test]
fn parameterized_policy_steers_twin_retry_and_timeout_decisions() {
    let twin = Twin::from_snapshot(recorded_snapshot()).unwrap();
    let seed = 42;

    // The floor: the flaky call retries to the budget and dead-letters;
    // the slow call completes unbounded.
    let baseline = twin.run(&TwinRunConfig::new(seed)).unwrap();
    assert_eq!(baseline.metrics.completed, 2, "slow + fast complete");
    assert_eq!(
        baseline.metrics.dead_lettered, 1,
        "flaky exhausts the budget"
    );
    assert_eq!(baseline.metrics.attempts, 5, "3 flaky + 1 slow + 1 fast");

    // A promoted policy: Unknown aborts after one attempt (the learned
    // permanent-failure stance), and `slow` runs under a 5 s bound.
    let policy = learned_retry_policy()
        .with_family_parameters(
            DecisionFamily::Retry,
            json!({
                "base_delay_ms": 1_000,
                "max_delay_ms": 300_000,
                "max_attempts": 3,
                "per_class": {
                    "unknown": {"base_delay_ms": 250, "max_delay_ms": 30_000, "max_attempts": 1},
                },
            }),
        )
        .unwrap()
        .with_family_parameters(
            DecisionFamily::Timeout,
            json!({"max_millis": 300_000, "per_callee": {"slow": 5_000}}),
        )
        .unwrap();
    let version = derive_policy_version(&policy).unwrap();
    let acting = ParameterizedPolicy::new(policy, version.clone()).unwrap();
    let learned = twin
        .run(&TwinRunConfig::new(seed).with_acting(std::sync::Arc::new(acting)))
        .unwrap();

    // The promoted parameters steered the decisions: flaky failed fast
    // (one attempt, no retries), and slow's 10 s completion was truncated
    // at the 5 s bound on every attempt — observed as Timeout, retried to
    // the budget, dead-lettered. The candidate loses on completion here,
    // which is exactly what the twin gate (tests/learn.rs) refuses to
    // promote.
    assert_eq!(learned.metrics.completed, 1, "only fast completes");
    assert_eq!(learned.metrics.failed, 1, "flaky aborts after one attempt");
    assert_eq!(
        learned.metrics.dead_lettered, 1,
        "slow truncates at the bound every attempt"
    );
    assert_eq!(learned.metrics.attempts, 5, "1 flaky + 3 slow + 1 fast");

    // Every decision journals the acting policy's derived version with
    // the acting role and the deterministic propensity — the evidence
    // contract does not regress under learned parameters.
    let acting_decisions: Vec<_> = learned
        .decisions
        .iter()
        .filter(|d| d.role == Some(rusty_agent_runtime::record::DecisionRole::Acting))
        .collect();
    assert!(!acting_decisions.is_empty());
    for decision in &acting_decisions {
        assert_eq!(decision.policy_version, version);
        assert_eq!(decision.propensity, 1.0);
        assert!(decision.legal_actions.contains(&decision.selected));
    }
    // The timeout family's decisions name the bound the policy declared.
    let timeout_selections: Vec<_> = acting_decisions
        .iter()
        .filter(|d| d.family == DecisionFamily::Timeout)
        .map(|d| d.selected.clone())
        .collect();
    assert!(timeout_selections.contains(&DecisionAction::SetTimeout { millis: 5_000 }));
    assert!(timeout_selections.contains(&DecisionAction::SetTimeout { millis: 300_000 }));
}

#[test]
fn parameterized_policy_decides_shadow_only_families_exactly_as_the_floor() {
    // Placement and concurrency have no parameter contract — a registered
    // policy cannot carry their parameters, so a `ParameterizedPolicy`
    // decides them with the floor's stance. A retry-only learned policy
    // that keeps the floor's attempt budget (only the schedule moves)
    // must leave the other families' decisions byte-identical.
    let twin = Twin::from_snapshot(recorded_snapshot()).unwrap();
    let seed = 7;
    let baseline = twin.run(&TwinRunConfig::new(seed)).unwrap();
    let policy = ExecutorPolicy::static_v0()
        .with_family_parameters(
            DecisionFamily::Retry,
            json!({"base_delay_ms": 500, "max_delay_ms": 60_000, "max_attempts": 3}),
        )
        .unwrap();
    let version = derive_policy_version(&policy).unwrap();
    let acting = ParameterizedPolicy::new(policy, version).unwrap();
    let learned = twin
        .run(&TwinRunConfig::new(seed).with_acting(std::sync::Arc::new(acting)))
        .unwrap();

    let placements = |outcome: &rusty_agent_runtime::twin::TwinOutcome| {
        outcome
            .decisions
            .iter()
            .filter(|d| {
                matches!(
                    d.family,
                    DecisionFamily::WorkerPlacement | DecisionFamily::Concurrency
                )
            })
            .map(|d| d.selected.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(placements(&baseline), placements(&learned));
    // Same family coverage, same counts — only retry decisions moved.
    assert_eq!(
        baseline
            .decisions
            .iter()
            .filter(|d| d.family == DecisionFamily::WorkerPlacement)
            .count(),
        learned
            .decisions
            .iter()
            .filter(|d| d.family == DecisionFamily::WorkerPlacement)
            .count()
    );
}

#[test]
fn parameterized_policy_construction_refuses_an_invalid_policy() {
    let mut invalid = ExecutorPolicy::static_v0();
    invalid.retry.max_attempts = 99;
    let err = ParameterizedPolicy::new(invalid, PolicyVersion::new("policy-bad")).unwrap_err();
    assert!(err.to_string().contains("outside the envelope"), "{err}");
}

// ---------- revert fidelity ----------

#[test]
fn floor_parameterized_policy_re_executes_byte_identically_to_the_static_floor() {
    // Revert-to-default restores exact static-v0 behavior: a
    // `ParameterizedPolicy` carrying the floor's parameters under the
    // floor's name reproduces the `StaticFloor`'s journal byte for byte —
    // same seed, same draws, same decisions, same head.
    let twin = Twin::from_snapshot(recorded_snapshot()).unwrap();
    let seed = 1_234;
    let baseline = twin.run(&TwinRunConfig::new(seed)).unwrap();
    let floor =
        ParameterizedPolicy::new(ExecutorPolicy::static_v0(), PolicyVersion::default()).unwrap();
    let reverted = twin
        .run(&TwinRunConfig::new(seed).with_acting(std::sync::Arc::new(floor)))
        .unwrap();
    assert_eq!(
        serde_json::to_string(&baseline.journal).unwrap(),
        serde_json::to_string(&reverted.journal).unwrap(),
        "the floor, parameterized or built-in, is one behavior"
    );
    assert_eq!(baseline.metrics, reverted.metrics);
}

#[test]
fn is_static_floor_compares_by_value() {
    assert!(ExecutorPolicy::static_v0().is_static_floor());
    assert!(!learned_retry_policy().is_static_floor());
    // A registered body identical to the floor IS the floor's behavior.
    let floor_clone = ExecutorPolicy::static_v0()
        .with_family_parameters(
            DecisionFamily::Retry,
            json!({"base_delay_ms": 1_000, "max_delay_ms": 300_000, "max_attempts": 3}),
        )
        .unwrap();
    assert!(floor_clone.is_static_floor());
}
