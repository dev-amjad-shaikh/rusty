//! The R0.10 wave-4 release proof: a learned retry policy walks the whole
//! adaptation loop end to end against the *production* fail path — queue,
//! store, policy registry, journal — with the twin gate as the evaluation
//! half, and returns to a byte-exact static floor when `static-v0` is
//! re-activated.
//!
//! The story (one long test, chapters marked in the body, the
//! `learn_release.rs` convention):
//!
//! 1. **Floor traffic.** Real queue traffic under the static floor: a
//!    scripted worker fails `rate_limited` on attempts 1 and 2 and
//!    completes attempt 3. The journaled decisions record `static-v0`,
//!    and the scheduled delays stay inside the floor's jittered bounds.
//! 2. **The evidence.** The same fault schedule runs in the digital twin:
//!    five recorded fixtures, each rate-limited on its first two attempts.
//!    The twin's journaled retry decisions — outcomes annotated from the
//!    item terminals, the application-code boundary the distiller's
//!    contract documents — distill a per-class schedule: base 100 ms,
//!    budget 5.
//! 3. **The gate.** The distilled parameters become a `policy` candidate,
//!    evaluate through the server's `TwinCandidateEvaluator` (floor arm
//!    vs candidate arm, wall time the target metric, completion parity
//!    enforced per fixture), and promote through the approval envelope.
//!    The registry activates the derived version.
//! 4. **Promoted traffic.** The same workload on the production queue now
//!    resolves the promoted schedule: attempt-1 delay inside [0, 100] ms,
//!    attempt-2 inside [0, 200] ms — bounds the floor's 1 s base cannot
//!    produce on most draws — the journaled decisions name the derived
//!    version, and the drift check reports the version healthy against
//!    its promotion baseline.
//! 5. **The ledger.** The improvement is priced net of telemetry: the
//!    emission path (event construction, draft, journal record) is
//!    measured in-process, the journaled bytes are measured off the wire,
//!    and the per-item twin margin is asserted to exceed the per-item
//!    telemetry charge by three orders of magnitude.
//! 6. **The floor returns.** Activating `static-v0` restores the floor
//!    byte-for-byte: the new traffic's normalized decision events equal
//!    chapter 1's exactly (jitter draws are OS entropy, so byte-exactness
//!    covers journaled decision content and bounds, never the sampled
//!    delays), the delays re-enter the floor's bounds, and the drift
//!    check refuses the floor with `422` — it was never promoted.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use chrono::{DateTime, Utc};
use rusty_agent_runtime::durable::{retry_decision_event, ErrorClass, RetryDecision};
use rusty_agent_runtime::effects::ApprovalToken;
use rusty_agent_runtime::journal::{Clock, EventDraft, Journal, JournalSnapshot};
use rusty_agent_runtime::learn::{
    distill_retry_parameters, promotion_effect_id, Candidate, CandidateContent, EvidenceSpan,
    RetryLearningConfig, TwinCandidateEvaluator,
};
use rusty_agent_runtime::memory::ProvenanceAuthor;
use rusty_agent_runtime::record::{
    derive_policy_version, DecisionFamily, DecisionOutcome, Effect, ExecutorPolicy, RunEventKind,
};
use rusty_agent_runtime::twin::{FaultAnchor, FaultSchedule, InjectedFault, Twin, TwinRunConfig};
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

/// The seed every stochastic surface derives from (twin draws, fault
/// schedule): one number, so the proof reproduces.
const SEED: u64 = 42;

/// The fixtures the twin gate prices.
const FIXTURES: usize = 5;

/// Processing slack (ms) added to the deterministic delay bounds: the
/// bound itself is policy-exact, but the test reads `next_attempt_at`
/// across an in-process HTTP call whose own runtime is not the policy's —
/// and under a parallel `cargo test --workspace`, a scheduling stall
/// between the test's clock read and the server's is real. 500 ms keeps
/// every bound assertion flake-free while staying well inside the floor's
/// own bounds (1 s / 2 s), so a wrong-policy regression still fails.
const SLACK_MS: i64 = 500;

// --------------------------------------------------------------------- //
// Harness (the policy.rs / learn_release.rs conventions)
// --------------------------------------------------------------------- //

/// Unique temp store root, removed at the end of the test (best effort).
fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!(
        "rusty-server-adaptation-release-test-{}",
        uuid::Uuid::new_v4()
    ))
}

/// A fixed timestamp, so candidates minted here are deterministic.
fn ts(millis: i64) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp_millis(millis).unwrap()
}

/// The distiller identity stamped on candidates and twin evaluations.
fn distiller() -> ProvenanceAuthor {
    ProvenanceAuthor::Distiller {
        name: "release-proof".into(),
    }
}

/// Send a request; returns `(status, json-body-or-null)`.
async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(v) => {
            builder = builder.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes: Bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

/// The one graph this proof drives: two nodes, completes — run-linked
/// tasks need a completed run's journal and admission checkpoint.
fn registry() -> GraphRegistry {
    use rusty_agent_runtime::prelude::*;

    let spec = StateSpec::new().channel("log", Reducer::Append);
    let mut builder = GraphBuilder::new();
    builder.add_node("first", |_ctx: NodeContext| async {
        Ok(NodeOutput::update("log", json!("first")))
    });
    builder.add_node("second", |_ctx: NodeContext| async {
        Ok(NodeOutput::update("log", json!("second")))
    });
    builder.set_entry_point("first");
    builder.add_edge("first", "second");
    let pipeline = builder.compile().unwrap();

    let mut registry = GraphRegistry::new();
    registry.register("pipeline", pipeline, spec);
    registry
}

/// Create a thread bound to `graph`; returns the thread id.
async fn create_thread(app: &Router, graph: &str) -> String {
    let (status, v) = call(app, "POST", "/threads", Some(json!({"graph": graph}))).await;
    assert_eq!(status, StatusCode::CREATED, "thread failed: {v}");
    v["thread_id"].as_str().unwrap().to_string()
}

/// Run the thread to a terminal state; returns the run body.
async fn run_wait(app: &Router, thread_id: &str, body: Value) -> Value {
    let (status, v) = call(
        app,
        "POST",
        &format!("/threads/{thread_id}/runs/wait"),
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "run failed: {v}");
    v
}

/// Create a thread, run `pipeline` to completion; returns the run id.
async fn run_pipeline(app: &Router) -> String {
    let thread_id = create_thread(app, "pipeline").await;
    let run = run_wait(app, &thread_id, json!({})).await;
    assert_eq!(run["status"], json!("success"), "pipeline failed: {run}");
    run["run_id"].as_str().unwrap().to_string()
}

/// The thread's latest checkpoint ref (`GET /threads/{id}/state`).
async fn checkpoint_of(app: &Router, thread_id: &str) -> Value {
    let (status, v) = call(app, "GET", &format!("/threads/{thread_id}/state"), None).await;
    assert_eq!(status, StatusCode::OK, "state failed: {v}");
    v["checkpoint"].clone()
}

/// The run's journaled events (Flight Recorder).
async fn events_of(app: &Router, run_id: &str) -> Vec<Value> {
    let (status, v) = call(app, "GET", &format!("/runs/{run_id}/events"), None).await;
    assert_eq!(status, StatusCode::OK, "events failed: {v}");
    v["events"].as_array().unwrap().clone()
}

/// The run's journaled `policy_decision` payloads (the inline
/// DecisionEvent values).
async fn decisions_of(app: &Router, run_id: &str) -> Vec<Value> {
    events_of(app, run_id)
        .await
        .into_iter()
        .filter(|event| event["kind"] == json!("policy_decision"))
        .map(|event| event["output"]["value"].clone())
        .collect()
}

/// Enqueue a run-linked `search` task (the fixture's callee), idempotent,
/// the floor's attempt budget; returns its task id.
async fn enqueue(app: &Router, run_id: &str) -> String {
    let (status, v) = call(
        app,
        "POST",
        "/tasks",
        Some(json!({
            "kind": "search",
            "payload": {"tool": "search", "arguments": {}},
            "effect": "idempotent",
            "max_attempts": 3,
            "run_id": run_id,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "enqueue failed: {v}");
    v["task_id"].as_str().unwrap().to_string()
}

/// Claim, polling until a task falls due (up to ~3 s): the retry schedule
/// is the point of the test, but the wall clock the schedule runs on is
/// not deterministic under a parallel test load.
async fn poll_claim(app: &Router, worker: &str) -> Value {
    for _ in 0..30 {
        let (status, v) = call(
            app,
            "POST",
            "/tasks/claim",
            Some(json!({"worker_id": worker, "lease_ms": 30_000})),
        )
        .await;
        if status == StatusCode::OK {
            return v["task"].clone();
        }
        assert_eq!(status, StatusCode::NO_CONTENT, "claim failed: {v}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("task never became claimable");
}

/// Fail the leased task `rate_limited`; asserts 200 and returns the
/// settlement body (`{requeued, next_attempt_at, dead, …}`).
async fn fail_rate_limited(app: &Router, task_id: &str, worker: &str) -> Value {
    let (status, v) = call(
        app,
        "POST",
        &format!("/tasks/{task_id}/fail"),
        Some(json!({"worker_id": worker, "error_class": "rate_limited",
                    "message": "HTTP 429", "retryable": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "fail failed: {v}");
    v
}

/// Complete the leased task; asserts 200.
async fn complete_task(app: &Router, task_id: &str, worker: &str) {
    let (status, v) = call(
        app,
        "POST",
        &format!("/tasks/{task_id}/complete"),
        Some(json!({"worker_id": worker, "result": {"ok": true}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "complete failed: {v}");
}

/// Drive one run-linked task through the scripted fault schedule — fail
/// `rate_limited` on attempts 1 and 2, complete attempt 3 — sleeping
/// exactly until each scheduled retry falls due. Returns the per-failure
/// scheduling delays (ms between the fail call and `next_attempt_at`).
async fn drive_task(app: &Router, run_id: &str, worker: &str) -> Vec<i64> {
    let task_id = enqueue(app, run_id).await;
    poll_claim(app, worker).await;
    let mut delays = Vec::new();
    for _ in 0..2 {
        let before = Utc::now();
        let settled = fail_rate_limited(app, &task_id, worker).await;
        assert_eq!(settled["requeued"], json!(true), "attempts remain");
        assert_eq!(settled["dead"], json!(false));
        let next = DateTime::parse_from_rfc3339(settled["next_attempt_at"].as_str().unwrap())
            .unwrap()
            .with_timezone(&Utc);
        delays.push((next - before).num_milliseconds());
        let wait = (next - Utc::now()).num_milliseconds().max(0) + 5;
        tokio::time::sleep(Duration::from_millis(wait as u64)).await;
        poll_claim(app, worker).await;
    }
    complete_task(app, &task_id, worker).await;
    delays
}

/// Decision events with the volatile fields stripped (id, run/thread
/// linkage, decision instant): the byte-exact comparison surface for the
/// floor's return. The sampled delay is never journaled — server jitter
/// draws from OS entropy — so nothing below depends on it.
fn normalized(decisions: &[Value]) -> Vec<Value> {
    decisions
        .iter()
        .map(|decision| {
            let mut decision = decision.clone();
            let object = decision.as_object_mut().unwrap();
            for field in ["id", "run_id", "thread_id", "decided_at"] {
                object.remove(field);
            }
            decision
        })
        .collect()
}

/// The twin-gate fixture (the wave-3 pattern): one recorded 100 ms `search`
/// completion the fault schedule rate-limits on its first two attempts.
fn rate_limited_snapshot(index: usize) -> JournalSnapshot {
    let journal = Journal::new(
        format!("run-rate-limited-{index}"),
        format!("thread-rate-limited-{index}"),
        Clock::logical(1_700_000_000_000, 10),
    );
    let step = journal.record(
        EventDraft::new(RunEventKind::SuperStepStart, Effect::Pure)
            .input(json!({"step": 0, "active_nodes": ["search"]})),
    );
    journal.record(
        EventDraft::new(RunEventKind::ToolCall, Effect::Idempotent)
            .node("search")
            .input(json!({"tool": "search", "arguments": {}}))
            .output(json!({"result": "ok"}))
            .latency_ms(100)
            .cost_usd(0.001)
            .parent(&step),
    );
    journal.record(
        EventDraft::new(RunEventKind::SuperStepEnd, Effect::Pure)
            .output(json!({"done": true}))
            .parent(&step),
    );
    journal.snapshot()
}

/// The fault schedule: the recorded `search` call is rate-limited (50 ms
/// retry-after) on its first two attempts; attempt 3 lands in the recorded
/// world and completes.
fn fault_schedule(fixtures: &[JournalSnapshot]) -> FaultSchedule {
    let effect_seq = fixtures[0]
        .events
        .iter()
        .find(|event| event.kind == RunEventKind::ToolCall)
        .map(|event| event.seq)
        .expect("the fixture records the call");
    FaultSchedule::new(SEED)
        .with_injection(
            FaultAnchor::OnAttempt {
                effect_seq,
                attempt: 1,
            },
            InjectedFault::RateLimited { retry_after_ms: 50 },
        )
        .with_injection(
            FaultAnchor::OnAttempt {
                effect_seq,
                attempt: 2,
            },
            InjectedFault::RateLimited { retry_after_ms: 50 },
        )
}

// --------------------------------------------------------------------- //
// The proof
// --------------------------------------------------------------------- //

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_adaptation_release_proves_itself_end_to_end() {
    let fixtures: Vec<JournalSnapshot> = (0..FIXTURES).map(rate_limited_snapshot).collect();
    let faults = fault_schedule(&fixtures);
    let evaluator = TwinCandidateEvaluator::new(SEED, distiller()).with_faults(faults.clone());
    let store = temp_store();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_candidate_evaluator(Arc::new(evaluator));
    let app = router(registry(), config);

    // ---- Chapter 1: floor traffic ----------------------------------- //
    // The same workload the twin prices, driven through the production
    // queue under the static floor. The journaled decisions are the
    // byte-exactness reference chapter 6 compares against.
    let floor_run = run_pipeline(&app).await;
    let floor_delays = drive_task(&app, &floor_run, "w-floor").await;
    // The floor's jittered bounds: attempt k draws from
    // [0, 1000 * 2^(k-1)].
    assert!(
        (0..=1_000 + SLACK_MS).contains(&floor_delays[0]),
        "floor attempt-1 delay {} outside its bound",
        floor_delays[0]
    );
    assert!(
        (0..=2_000 + SLACK_MS).contains(&floor_delays[1]),
        "floor attempt-2 delay {} outside its bound",
        floor_delays[1]
    );
    let floor_decisions = decisions_of(&app, &floor_run).await;
    assert_eq!(floor_decisions.len(), 2, "two failures, two decisions");
    for (i, decision) in floor_decisions.iter().enumerate() {
        assert_eq!(decision["policy_version"], json!("static-v0"));
        assert_eq!(decision["family"], json!("retry"));
        assert_eq!(decision["features"]["failure_class"], json!("rate_limited"));
        assert_eq!(decision["features"]["attempt"], json!(i + 1));
        assert_eq!(decision["features"]["max_attempts"], json!(3));
    }

    // ---- Chapter 2: twin evidence → distilled parameters ------------- //
    // Each fixture runs in the twin under the floor with the fault
    // schedule. The twin journals its retry decisions with `outcome: None`
    // — the re-attempt has not happened at decision time — so the item
    // terminals annotate them: the decision that sent each item to its
    // completing attempt recovered; the earlier one carries no outcome
    // (the distiller reads it as not-yet-recovered, p = 0.5).
    let mut evidence = Vec::new();
    for fixture in &fixtures {
        let twin = Twin::from_snapshot(fixture.clone()).expect("the fixture loads");
        let outcome = twin
            .run(&TwinRunConfig::new(SEED).with_faults(faults.clone()))
            .expect("the twin re-executes the fixture");
        assert_eq!(
            outcome.metrics.completed, outcome.metrics.items,
            "attempt 3 completes in the recorded world"
        );
        let mut decisions = outcome.decisions;
        let last_retry = decisions
            .iter_mut()
            .filter(|decision| decision.family == DecisionFamily::Retry)
            .last()
            .expect("the fault schedule forces two retry decisions");
        last_retry.outcome = Some(DecisionOutcome::Success);
        evidence.extend(decisions);
    }
    let params = distill_retry_parameters(&evidence, &RetryLearningConfig::default());
    // The flat schedule stays the floor's; the class the evidence spoke
    // for earns the grid's shortest base and a wider budget.
    assert_eq!(params.base_delay_ms, 1_000);
    assert_eq!(params.max_delay_ms, 300_000);
    assert_eq!(params.max_attempts, 3);
    let entry = params
        .per_class
        .as_ref()
        .and_then(|table| table.get(&ErrorClass::RateLimited))
        .expect("p = 0.5 clears the 2 s margin");
    assert_eq!(entry.base_delay_ms, 100);
    assert_eq!(entry.max_attempts, 5);
    assert_eq!(entry.max_delay_ms, 30_000);

    // ---- Chapter 3: candidate → twin gate → promotion ---------------- //
    let candidate = Candidate::new(
        CandidateContent::Policy {
            family: DecisionFamily::Retry,
            parameters: serde_json::to_value(&params).unwrap(),
        },
        distiller(),
        EvidenceSpan {
            run_ids: vec![floor_run.clone()],
            ..EvidenceSpan::default()
        },
        ts(1_750_000_020_000),
    )
    .unwrap();
    let (status, v) = call(
        &app,
        "POST",
        "/learn/candidates",
        Some(json!({
            "candidate": serde_json::to_value(&candidate).unwrap(),
            "run_id": floor_run,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create failed: {v}");
    let candidate_id = v["candidate_id"].as_str().unwrap().to_string();

    let (status, v) = call(
        &app,
        "POST",
        &format!("/learn/candidates/{candidate_id}/evaluate"),
        Some(json!({
            "request": {
                "dataset_version": "adaptation-v1",
                "target_metric": "wall_time_ms",
                "thresholds": {"max_pass_rate_drop": 0.05, "max_latency_p95_ratio": 1.25},
                "replay_evidence": fixtures,
            },
            "run_id": floor_run,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "evaluate failed: {v}");
    assert_eq!(v["status"], json!("evaluated"));
    let evaluation = &v["evaluation"];
    assert_eq!(evaluation["replay"]["matched"], json!(FIXTURES));
    assert!(
        evaluation["replay"]["divergences"]
            .as_array()
            .is_none_or(|d| d.is_empty()),
        "every fixture re-executes"
    );
    assert_eq!(
        evaluation["verdict"]["regressed"],
        json!(false),
        "completion parity — both arms recover on attempt 3"
    );
    let delta = evaluation["verdict"]["delta"]
        .as_f64()
        .expect("the twin prices wall time");
    assert!(
        delta > 0.0,
        "the shorter backoff waits less at identical completion: delta {delta}"
    );
    // The reports carry the aggregate the drift baseline is derived from.
    let baseline_aggregate = evaluation["baseline_report"]["aggregate"].clone();
    let candidate_aggregate = evaluation["candidate_report"]["aggregate"].clone();
    assert!(baseline_aggregate.get("completion_rate").is_some());
    assert!(candidate_aggregate.get("completion_rate").is_some());

    // Policy promotions are approval-ruled under the default envelope.
    let token = ApprovalToken::approve(promotion_effect_id(&candidate), "ops:amjad");
    let (status, v) = call(
        &app,
        "POST",
        &format!("/learn/candidates/{candidate_id}/promote"),
        Some(json!({
            "run_id": floor_run,
            "approval": serde_json::to_value(&token).unwrap(),
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "approved promote failed: {v}");

    let promoted_policy = ExecutorPolicy::static_v0()
        .with_family_parameters(
            DecisionFamily::Retry,
            serde_json::to_value(&params).unwrap(),
        )
        .unwrap();
    let promoted_version = derive_policy_version(&promoted_policy).unwrap();
    let (status, v) = call(&app, "GET", "/policy/active", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["version"], json!(promoted_version.as_str()));

    // ---- Chapter 4: promoted traffic on the production fail path ----- //
    // New traffic binds the promoted version at admission; the fail path
    // resolves its per-class schedule. Attempt 1's delay lands inside
    // [0, 100] and attempt 2's inside [0, 200] — bounds the floor's 1 s
    // base cannot produce on most of its draws, resolved by the same
    // `classify_retry_with_policy` the twin priced. Completion parity
    // holds: attempt 3 completes, exactly as the floor's traffic did.
    let promoted_thread = create_thread(&app, "pipeline").await;
    let promoted_run = run_wait(&app, &promoted_thread, json!({})).await;
    assert_eq!(promoted_run["status"], json!("success"));
    let promoted_run = promoted_run["run_id"].as_str().unwrap().to_string();
    assert_eq!(
        checkpoint_of(&app, &promoted_thread).await["policy_version"],
        json!(promoted_version.as_str()),
        "admission pins the promoted version"
    );
    let promoted_delays = drive_task(&app, &promoted_run, "w-promoted").await;
    assert!(
        (0..=100 + SLACK_MS).contains(&promoted_delays[0]),
        "promoted attempt-1 delay {} outside the learned [0, 100] bound",
        promoted_delays[0]
    );
    assert!(
        (0..=200 + SLACK_MS).contains(&promoted_delays[1]),
        "promoted attempt-2 delay {} outside the learned [0, 200] bound",
        promoted_delays[1]
    );
    let promoted_decisions = decisions_of(&app, &promoted_run).await;
    assert_eq!(promoted_decisions.len(), 2);
    for (i, decision) in promoted_decisions.iter().enumerate() {
        assert_eq!(
            decision["policy_version"],
            json!(promoted_version.as_str()),
            "the decision names the acting version"
        );
        assert_eq!(decision["features"]["failure_class"], json!("rate_limited"));
        assert_eq!(decision["features"]["attempt"], json!(i + 1));
        // The learned budget (5) narrows to the task's declared budget (3)
        // — the journaled features record the boundary the classifier
        // actually enforced.
        assert_eq!(decision["features"]["max_attempts"], json!(3));
    }
    // The drift check reads the version's production evidence against its
    // promotion baseline: healthy (and below the minimum-evidence bar,
    // which itself declares nothing).
    let (status, v) = call(&app, "GET", "/policy/drift", None).await;
    assert_eq!(status, StatusCode::OK, "drift check failed: {v}");
    assert_eq!(v["report"]["version"], json!(promoted_version.as_str()));
    assert_eq!(v["report"]["drifted"], json!(false));
    assert_eq!(v["report"]["decisions"], json!(2));

    // ---- Chapter 5: the ledger — improvement net of telemetry -------- //
    // Bytes: the journaled decision events as they actually landed.
    let decision_bytes: usize = promoted_decisions
        .iter()
        .map(|decision| serde_json::to_vec(decision).unwrap().len())
        .sum();
    let bytes_per_decision = decision_bytes / promoted_decisions.len();
    // Wall time: the emission path (event construction, serialization,
    // draft, journal record — the work `try_journal_policy_decision` adds
    // to a settlement, minus the store IO a settlement already pays),
    // measured in-process over 10k iterations. The measurement repeats
    // and keeps the best run: scheduling interference on a shared runner
    // can only add time, so the minimum is the honest estimate of the
    // path's cost and the only estimator the ratio below can trust.
    let retry = RetryDecision::Retry { after_ms: 100 };
    let iterations = 10_000u64;
    let attempts = 5;
    let mut per_decision_ns = u128::MAX;
    for _ in 0..attempts {
        let overhead_journal = Journal::new("run-overhead", "thread-overhead", Clock::System);
        let started = Instant::now();
        let mut parent: Option<String> = None;
        for seq in 0..iterations {
            let event = retry_decision_event(
                "run-overhead",
                "thread-overhead",
                seq,
                Effect::Idempotent,
                ErrorClass::RateLimited,
                1,
                3,
                None,
                &retry,
                &promoted_version,
                Utc::now(),
            );
            let draft = EventDraft::new(RunEventKind::PolicyDecision, Effect::Pure)
                .output(serde_json::to_value(&event).unwrap());
            let draft = match &parent {
                Some(parent) => draft.parent(parent.clone()),
                None => draft,
            };
            parent = Some(overhead_journal.record(draft));
        }
        per_decision_ns = per_decision_ns.min(started.elapsed().as_nanos() / iterations as u128);
    }
    // The charge: two retry decisions per item. The margin: the twin's
    // aggregate wall-time delta, per fixture.
    let overhead_per_item_ns = per_decision_ns * 2;
    let margin_per_item_ns = (delta / FIXTURES as f64 * 1_000_000.0) as u128;
    assert!(
        margin_per_item_ns > overhead_per_item_ns * 1_000,
        "the measured margin ({margin_per_item_ns} ns/item) must dwarf the telemetry charge \
         ({overhead_per_item_ns} ns/item) by three orders of magnitude"
    );
    eprintln!(
        "adaptation-release measurements:\n\
         \x20 twin mean_wall_time_ms per item: floor {} / candidate {} (delta {:.1} ms over {FIXTURES} fixtures)\n\
         \x20 telemetry: {per_decision_ns} ns/decision, {bytes_per_decision} bytes/decision \
         ({overhead_per_item_ns} ns/item charged vs {margin_per_item_ns} ns/item margin)\n\
         \x20 floor aggregate: {baseline_aggregate}\n\
         \x20 candidate aggregate: {candidate_aggregate}\n\
         \x20 floor delays (ms): {floor_delays:?} / promoted delays (ms): {promoted_delays:?}",
        baseline_aggregate["mean_wall_time_ms"].as_f64().unwrap_or(0.0),
        candidate_aggregate["mean_wall_time_ms"].as_f64().unwrap_or(0.0),
        delta,
    );

    // ---- Chapter 6: the floor returns, byte-exact -------------------- //
    let (status, v) = call(
        &app,
        "POST",
        "/policy/activations",
        Some(json!({"version": "static-v0"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "floor activation failed: {v}");
    let (status, v) = call(&app, "GET", "/policy/active", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["version"], json!("static-v0"));

    let returned_thread = create_thread(&app, "pipeline").await;
    let returned_run = run_wait(&app, &returned_thread, json!({})).await;
    assert_eq!(returned_run["status"], json!("success"));
    let returned_run = returned_run["run_id"].as_str().unwrap().to_string();
    assert_eq!(
        checkpoint_of(&app, &returned_thread).await["policy_version"],
        json!("static-v0")
    );
    let returned_delays = drive_task(&app, &returned_run, "w-returned").await;
    assert!(
        (0..=1_000 + SLACK_MS).contains(&returned_delays[0]),
        "the floor's attempt-1 bound is back in force (delay {})",
        returned_delays[0]
    );
    assert!(
        (0..=2_000 + SLACK_MS).contains(&returned_delays[1]),
        "the floor's attempt-2 bound is back in force (delay {})",
        returned_delays[1]
    );
    let returned_decisions = decisions_of(&app, &returned_run).await;
    assert_eq!(returned_decisions.len(), 2);
    assert_eq!(
        normalized(&returned_decisions),
        normalized(&floor_decisions),
        "the floor's return is byte-exact: same legal sets, selections, features, and version \
         as chapter 1 (jitter draws excluded — they are never journaled)"
    );
    // The floor was never promoted: the drift check refuses it, and says
    // why, instead of inventing a baseline.
    let (status, v) = call(&app, "GET", "/policy/drift", None).await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "expected 422: {v}"
    );

    let _ = std::fs::remove_dir_all(store);
}
