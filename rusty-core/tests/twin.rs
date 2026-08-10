//! Runtime digital twin integration tests (R0.10 wave 2).
//!
//! The twin's determinism is its whole claim, and the suite is organized to
//! prove it and the four mechanisms:
//!
//! - **Determinism** — same seed + same recorded world ⇒ byte-identical
//!   journals across repeated in-process runs, and across process
//!   invocations via the checked-in golden (`UPDATE_GOLDEN=1` to bless an
//!   intentional change).
//! - **Fault injection** — schedules fire at exactly the declared decision
//!   points and effect boundaries: a rate limit on attempt N, a crash at a
//!   retry decision (the lease-expiry `Unknown` path), a window, a degraded
//!   worker. Fired and declared counts are reported, never silent.
//! - **Schedule randomization** — seeded interleavings preserve the
//!   journaled total order of evidence while per-node latencies follow the
//!   drawn order; every interleaving reproduces exactly from its seed.
//! - **Counterfactual branches** — one changed legal action at one decision
//!   produces a valid `BranchDiff` whose divergence point is the fork;
//!   illegal forks (outside the recomputed legal set — the honest edge) and
//!   forks at decisions the run never reaches are refused.
//! - **Shadow policies** — candidate and floor decisions journal side by
//!   side with roles and true propensities; divergences are reported; the
//!   pair reproduces exactly under re-run.

use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;
use serde_json::{json, Value};

use rusty_agent_runtime::durable::ErrorClass;
use rusty_agent_runtime::journal::{Clock, EventDraft, Journal, JournalSnapshot};
use rusty_agent_runtime::record::{
    DecisionAction, DecisionEvent, DecisionFamily, DecisionRole, Effect, PolicyVersion,
    RunEventKind,
};
use rusty_agent_runtime::replay::{ReplayFixture, SERVABLE_KINDS};
use rusty_agent_runtime::twin::{
    CounterfactualFork, DecisionContext, FaultAnchor, FaultSchedule, InjectedFault, Twin,
    TwinOutcome, TwinPolicy, TwinRunConfig, TWIN_FORK_POLICY_VERSION, TWIN_REPORT_BOUND,
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
/// the diff is the contract change under review.
fn assert_golden(name: &str, value: &impl Serialize) {
    let path = golden_path(name);
    let actual = serde_json::to_string_pretty(value).unwrap() + "\n";
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, &actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing golden file `{}`: {e}", path.display()));
    assert_eq!(
        actual,
        expected,
        "contract drift in `{}` — if intentional, re-run with UPDATE_GOLDEN=1 \
         and review the diff",
        path.display()
    );
}

// ---------- the recorded world ----------

/// A recorded run with two super-steps: step 0 fans out three idempotent
/// tool calls with heterogeneous latencies (the interleaving-sensitive
/// parallel set), step 1 runs one finalizing call. Synthesized through the
/// journal's own recording path, so integrity verification and artifact
/// promotion apply exactly as to a real recording.
fn recorded_snapshot() -> JournalSnapshot {
    let journal = Journal::new(
        "run-twin",
        "thread-twin",
        Clock::logical(1_700_000_000_000, 10),
    );
    let step0 = journal.record(
        EventDraft::new(RunEventKind::SuperStepStart, Effect::Pure)
            .input(json!({"step": 0, "active_nodes": ["fetch-a", "fetch-b", "fetch-c"]})),
    );
    let calls = [
        ("fetch-a", 100, "a"),
        ("fetch-b", 500, "b"),
        ("fetch-c", 1000, "c"),
    ];
    for (node, latency, result) in calls {
        journal.record(
            EventDraft::new(RunEventKind::ToolCall, Effect::Idempotent)
                .node(node)
                .input(json!({"tool": node, "arguments": {"q": result}}))
                .output(json!({"result": result}))
                .latency_ms(latency)
                .cost_usd(0.001)
                .parent(&step0),
        );
    }
    journal.record(
        EventDraft::new(RunEventKind::SuperStepEnd, Effect::Pure)
            .output(json!({"fetched": ["a", "b", "c"]}))
            .parent(&step0),
    );
    let step1 = journal.record(
        EventDraft::new(RunEventKind::SuperStepStart, Effect::Pure)
            .input(json!({"step": 1, "active_nodes": ["finalize"]})),
    );
    journal.record(
        EventDraft::new(RunEventKind::ToolCall, Effect::Idempotent)
            .node("finalize")
            .input(json!({"tool": "finalize", "arguments": {"q": "z"}}))
            .output(json!({"result": "z"}))
            .latency_ms(50)
            .parent(&step1),
    );
    journal.record(
        EventDraft::new(RunEventKind::SuperStepEnd, Effect::Pure)
            .output(json!({"done": true}))
            .parent(&step1),
    );
    journal.snapshot()
}

fn twin() -> Twin {
    Twin::from_snapshot(recorded_snapshot()).unwrap()
}

/// The recorded sequence numbers of the fanned-out tool calls (the fault
/// anchors name effects by recorded seq).
const FETCH_A_SEQ: u64 = 1;
const FETCH_B_SEQ: u64 = 2;

fn baseline_config() -> TwinRunConfig {
    TwinRunConfig::new(42)
}

/// The decision sequence of the first Retry-family decision in an outcome —
/// tests discover decision points from evidence instead of hardcoding the
/// scheduler's numbering.
fn first_retry_decision_seq(outcome: &TwinOutcome) -> u64 {
    outcome
        .decisions
        .iter()
        .find(|d| d.family == DecisionFamily::Retry && d.role == Some(DecisionRole::Acting))
        .map(|d| d.seq)
        .expect("the run contains a retry decision")
}

/// All effect events (servable kinds) in a journal, in seq order.
fn effect_events(snapshot: &JournalSnapshot) -> Vec<&rusty_agent_runtime::record::RunEvent> {
    snapshot
        .events
        .iter()
        .filter(|e| SERVABLE_KINDS.contains(&e.kind))
        .collect()
}

fn resolve(snapshot: &JournalSnapshot, event: &rusty_agent_runtime::record::RunEvent) -> Value {
    match event.output.as_ref().unwrap() {
        rusty_agent_runtime::record::PayloadRef::Inline(value) => value.clone(),
        rusty_agent_runtime::record::PayloadRef::Artifact(reference) => {
            snapshot.artifacts[&reference.sha256].clone()
        }
    }
}

/// Byte-identical journals, asserted field by field: identity, every event,
/// every artifact, and the chained head hash. `JournalSnapshot` has no
/// `PartialEq` (replay's comparisons go through `BranchDiff`'s *logical*
/// equality); the twin's determinism claim is stronger than logical — the
/// whole snapshot, identity fields included, must reproduce.
fn assert_snapshots_identical(a: &JournalSnapshot, b: &JournalSnapshot) {
    assert_eq!(a.run_id, b.run_id);
    assert_eq!(a.thread_id, b.thread_id);
    assert_eq!(a.events, b.events);
    assert_eq!(a.artifacts, b.artifacts);
    assert_eq!(a.head_hash, b.head_hash);
}

// ---------- determinism ----------

#[test]
fn twin_run_is_byte_identical_across_repeated_runs() {
    let twin = twin();
    let config = baseline_config().with_faults(FaultSchedule::new(42).with_injection(
        FaultAnchor::OnAttempt {
            effect_seq: FETCH_B_SEQ,
            attempt: 1,
        },
        InjectedFault::RateLimited {
            retry_after_ms: 250,
        },
    ));
    let first = twin.run(&config).unwrap();
    let second = twin.run(&config).unwrap();
    // Full snapshot equality: events, artifacts, and the chained head hash.
    assert_snapshots_identical(&first.journal, &second.journal);
    assert_eq!(first.decisions, second.decisions);
    assert_eq!(first.report, second.report);
}

#[test]
fn golden_twin_baseline_run() {
    // Cross-process determinism: a fresh `cargo test` process must
    // reproduce these bytes from seed 42 and the fixture alone.
    let outcome = twin().run(&baseline_config()).unwrap();
    assert_golden(
        "twin_baseline_run.json",
        &json!({
            "head_hash": outcome.journal.head_hash,
            "event_count": outcome.journal.events.len(),
            "metrics": outcome.metrics,
            "report": outcome.report,
        }),
    );
}

#[test]
fn golden_fault_schedule_shape() {
    // The schedule is the committed reproducibility artifact; its wire
    // shape is pinned like every other contract.
    let schedule = FaultSchedule::new(7)
        .with_injection(
            FaultAnchor::OnAttempt {
                effect_seq: 3,
                attempt: 2,
            },
            InjectedFault::CalleeTimeout,
        )
        .with_injection(
            FaultAnchor::AtDecision { decision_seq: 5 },
            InjectedFault::WorkerCrash,
        )
        .with_injection(
            FaultAnchor::Window {
                from_seq: 10,
                to_seq: 14,
            },
            InjectedFault::RateLimited {
                retry_after_ms: 1_500,
            },
        )
        .with_injection(
            FaultAnchor::OnWorker {
                worker: "worker-1".to_owned(),
            },
            InjectedFault::ResourceExhausted,
        );
    assert_golden("twin_fault_schedule.json", &schedule);
}

#[test]
fn golden_decision_roles() {
    let roles: Vec<String> = [DecisionRole::Acting, DecisionRole::Shadow]
        .iter()
        .map(|role| {
            serde_json::to_value(role)
                .unwrap()
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_golden("twin_decision_roles.json", &roles);
}

// ---------- mechanism 1: fault injection ----------

#[test]
fn no_fault_twin_reproduces_the_recorded_answers() {
    let recorded = recorded_snapshot();
    let outcome = twin().run(&baseline_config()).unwrap();
    let recorded_effects = effect_events(&recorded);
    let twin_effects = effect_events(&outcome.journal);
    assert_eq!(twin_effects.len(), recorded_effects.len());
    // Outputs and statuses reproduce the recording exactly. Journaled
    // latencies include admission wait: one worker admits one item at a
    // time, so the fan-out serializes in canonical order — fetch-b starts
    // when fetch-a ends (100), fetch-c when fetch-b ends (600); the
    // single-item second step waits for nothing.
    let expected_latencies = [100, 600, 1_600, 50];
    for ((served, original), expected) in twin_effects
        .iter()
        .zip(recorded_effects)
        .zip(expected_latencies)
    {
        assert_eq!(served.status, original.status);
        assert_eq!(served.latency_ms, Some(expected));
        assert_eq!(
            resolve(&outcome.journal, served),
            resolve(&recorded, original)
        );
    }
    assert_eq!(outcome.metrics.items, 4);
    assert_eq!(outcome.metrics.completed, 4);
    assert_eq!(outcome.metrics.attempts, 4);
    assert_eq!(outcome.report.faults_fired, 0);
}

#[test]
fn rate_limit_fires_on_the_declared_attempt_and_floors_the_delay() {
    let outcome = twin()
        .run(
            &baseline_config().with_faults(FaultSchedule::new(42).with_injection(
                FaultAnchor::OnAttempt {
                    effect_seq: FETCH_B_SEQ,
                    attempt: 1,
                },
                InjectedFault::RateLimited {
                    retry_after_ms: 2_000,
                },
            )),
        )
        .unwrap();

    assert_eq!(outcome.report.faults_declared, 1);
    assert_eq!(outcome.report.faults_fired, 1);

    // The faulted attempt is journaled as a rate-limited error; the retry
    // decision observes exactly that class, with the Retry-After floor in
    // the feature snapshot.
    let effects = effect_events(&outcome.journal);
    let faulted = effects
        .iter()
        .find(|e| resolve(&outcome.journal, e) == json!({"error": "rate_limited"}))
        .expect("the faulted attempt is journaled");
    assert_eq!(
        faulted.status,
        rusty_agent_runtime::record::EventStatus::Error
    );

    let retry = outcome
        .decisions
        .iter()
        .find(|d| d.family == DecisionFamily::Retry)
        .expect("the failure produced a retry decision");
    assert_eq!(retry.features["failure_class"], json!("rate_limited"));
    assert_eq!(retry.features["retry_after_ms"], json!(2_000));
    assert_eq!(retry.selected, DecisionAction::Retry { attempt: 2 });
    assert_eq!(
        retry.policy_version,
        PolicyVersion::new(PolicyVersion::STATIC_V0)
    );

    // Attempt 2 is served the recorded answer: the item completes, and the
    // run's other items are untouched.
    assert_eq!(outcome.metrics.completed, 4);
    assert_eq!(outcome.metrics.attempts, 5);
    let served = effects
        .iter()
        .filter(|e| resolve(&outcome.journal, e) == json!({"result": "b"}))
        .count();
    assert_eq!(served, 1);
}

#[test]
fn worker_crash_at_a_decision_point_takes_the_lease_expiry_path() {
    // Discover the retry decision's sequence from evidence, then declare
    // the crash there.
    let rate_limit = FaultSchedule::new(42).with_injection(
        FaultAnchor::OnAttempt {
            effect_seq: FETCH_B_SEQ,
            attempt: 1,
        },
        InjectedFault::RateLimited {
            retry_after_ms: 100,
        },
    );
    let probe = twin()
        .run(&baseline_config().with_faults(rate_limit.clone()))
        .unwrap();
    let decision_seq = first_retry_decision_seq(&probe);

    let outcome = twin()
        .run(&baseline_config().with_faults(rate_limit.with_injection(
            FaultAnchor::AtDecision { decision_seq },
            InjectedFault::WorkerCrash,
        )))
        .unwrap();
    assert_eq!(outcome.report.faults_declared, 2);
    assert_eq!(outcome.report.faults_fired, 2);

    // The crash reclassifies the observation: Unknown (a lost attempt, not
    // the rate limit the world would have reported), discovered at the
    // lease boundary — 300 s of simulated latency, plus whatever admission
    // wait the single lane imposed (fetch-a was ahead of fetch-b).
    let retry = outcome
        .decisions
        .iter()
        .find(|d| d.family == DecisionFamily::Retry)
        .unwrap();
    assert_eq!(retry.features["failure_class"], json!("unknown"));
    let crash = effect_events(&outcome.journal)
        .into_iter()
        .find(|e| resolve(&outcome.journal, e) == json!({"error": "unknown"}))
        .expect("the crashed attempt is journaled");
    assert!(
        crash.latency_ms.unwrap() >= rusty_agent_runtime::durable::MAX_RETRY_DELAY_MS,
        "a crash is discovered at the lease boundary, got {:?}",
        crash.latency_ms
    );
}

#[test]
fn a_window_faults_every_attempt_in_range_and_reports_unfired_anchors() {
    let outcome = twin()
        .run(
            &baseline_config().with_faults(
                FaultSchedule::new(42)
                    .with_injection(
                        FaultAnchor::Window {
                            from_seq: FETCH_A_SEQ,
                            to_seq: FETCH_B_SEQ,
                        },
                        InjectedFault::ResourceExhausted,
                    )
                    // An anchor that never coincides with an attempt: declared,
                    // not fired, and the report shows the difference.
                    .with_injection(
                        FaultAnchor::AtDecision { decision_seq: 999 },
                        InjectedFault::WorkerCrash,
                    ),
            ),
        )
        .unwrap();
    assert_eq!(outcome.report.faults_declared, 2);
    // The window covers both fanned-out items and *every* attempt in
    // range: each fails all three budgeted attempts and dead-letters —
    // six firings. The 999 anchor never coincides with a retry decision
    // and never fires.
    assert_eq!(outcome.report.faults_fired, 6);
    assert_eq!(outcome.metrics.completed, 2);
    assert_eq!(outcome.metrics.dead_lettered, 2);
    let exhausted = effect_events(&outcome.journal)
        .into_iter()
        .filter(|e| resolve(&outcome.journal, e) == json!({"error": "resource_exhausted"}))
        .count();
    assert_eq!(exhausted, 6);
}

#[test]
fn faults_on_non_idempotent_work_hit_the_effect_gate_and_are_flagged() {
    // Model calls are NonIdempotent: a fault there is never silently
    // retried — the effect gate fails the item on the first fault, and the
    // report flags that the gate, not the policy, bounded the evaluation.
    let journal = Journal::new(
        "run-model",
        "thread-model",
        Clock::logical(1_700_000_000_000, 10),
    );
    journal.record(
        EventDraft::new(RunEventKind::ModelCall, Effect::NonIdempotent)
            .node("agent")
            .input(json!({"messages": [], "tools": []}))
            .output(json!({"message": {"role": "assistant", "content": "hi"}, "model": null, "usage": null}))
            .latency_ms(800),
    );
    let twin = Twin::from_snapshot(journal.snapshot()).unwrap();
    let outcome = twin
        .run(
            &baseline_config().with_faults(FaultSchedule::new(42).with_injection(
                FaultAnchor::OnAttempt {
                    effect_seq: 0,
                    attempt: 1,
                },
                InjectedFault::CalleeTimeout,
            )),
        )
        .unwrap();
    assert_eq!(outcome.metrics.failed, 1);
    assert_eq!(outcome.metrics.attempts, 1);
    let retry = outcome
        .decisions
        .iter()
        .find(|d| d.family == DecisionFamily::Retry)
        .unwrap();
    assert_eq!(retry.legal_actions, vec![DecisionAction::Abort]);
    assert_eq!(retry.selected, DecisionAction::Abort);
    assert_eq!(outcome.report.excluded.len(), 1);
    assert!(outcome.report.excluded[0]
        .to_string_case()
        .contains("gated_effect"));
}

/// Test-local helper so the exclusion case reads on one line.
trait CaseName {
    fn to_string_case(&self) -> String;
}

impl CaseName for rusty_agent_runtime::twin::UnevaluableCase {
    fn to_string_case(&self) -> String {
        serde_json::to_value(self).unwrap()["case"]
            .as_str()
            .unwrap()
            .to_owned()
    }
}

// ---------- mechanism 2: schedule randomization ----------

#[test]
fn seeded_interleavings_preserve_evidence_order_and_vary_latency() {
    let twin = twin();
    let canonical = twin.run(&baseline_config()).unwrap();
    let runs = twin
        .run_interleavings(4, &baseline_config().with_seeded_interleaving())
        .unwrap();
    assert_eq!(runs.len(), 4);

    // The journaled total order of evidence is stable: the (kind, node,
    // status) sequence of every interleaving equals the canonical run's.
    let order_of = |snapshot: &JournalSnapshot| {
        snapshot
            .events
            .iter()
            .map(|e| (e.kind, e.node_id.clone(), e.status))
            .collect::<Vec<_>>()
    };
    let canonical_order = order_of(&canonical.journal);
    for run in &runs {
        assert_eq!(order_of(&run.journal), canonical_order);
    }

    // Per-seed reproduction: interleaving run k is a pure function of the
    // base seed and k.
    let reproduced = twin
        .run_interleavings(4, &baseline_config().with_seeded_interleaving())
        .unwrap();
    for (a, b) in runs.iter().zip(&reproduced) {
        assert_snapshots_identical(&a.journal, &b.journal);
    }

    // One worker admits one item at a time, so admission order changes
    // observed latencies: at least two interleavings must differ in their
    // journaled latency vectors. (Deterministic by seed; with three items
    // the drawn permutations of four seeds do not all coincide.)
    let latencies_of = |snapshot: &JournalSnapshot| {
        effect_events(snapshot)
            .iter()
            .map(|e| e.latency_ms)
            .collect::<Vec<_>>()
    };
    let distinct: std::collections::BTreeSet<_> =
        runs.iter().map(|r| latencies_of(&r.journal)).collect();
    assert!(
        distinct.len() > 1,
        "seeded interleavings vary per-node latencies"
    );
    // The interleaving runs' event payloads still match the canonical
    // run's: only timing attributes vary.
    for run in &runs {
        for (a, b) in effect_events(&run.journal)
            .iter()
            .zip(effect_events(&canonical.journal))
        {
            assert_eq!(resolve(&run.journal, a), resolve(&canonical.journal, b));
        }
    }
}

// ---------- mechanism 3: counterfactual branches ----------

#[test]
fn counterfactual_branch_produces_a_valid_branch_diff() {
    // Every attempt of fetch-b rate-limits: the floor retries to its
    // budget and dead-letters. The fork aborts at the first retry
    // decision instead.
    let always_limited = (1..=3u32).fold(FaultSchedule::new(42), |schedule, attempt| {
        schedule.with_injection(
            FaultAnchor::OnAttempt {
                effect_seq: FETCH_B_SEQ,
                attempt,
            },
            InjectedFault::RateLimited {
                retry_after_ms: 100,
            },
        )
    });
    let config = baseline_config().with_faults(always_limited);
    let twin = twin();
    let baseline = twin.run(&config).unwrap();
    assert_eq!(baseline.metrics.dead_lettered, 1);
    let fork_seq = first_retry_decision_seq(&baseline);

    let branch = twin
        .counterfactual(
            &config,
            CounterfactualFork {
                decision_seq: fork_seq,
                action: DecisionAction::Abort,
                then_act_with: None,
            },
        )
        .unwrap();

    // The fork decision is journaled under the twin-fork version, acting.
    let forked = branch
        .outcome
        .decisions
        .iter()
        .find(|d| d.seq == fork_seq && d.role == Some(DecisionRole::Acting))
        .unwrap();
    assert_eq!(forked.selected, DecisionAction::Abort);
    assert_eq!(
        forked.policy_version,
        PolicyVersion::new(TWIN_FORK_POLICY_VERSION)
    );
    assert_eq!(forked.propensity, 1.0);

    // The branch item fails immediately instead of dead-lettering after
    // three attempts; everything before the fork is logically identical.
    assert_eq!(branch.outcome.metrics.failed, 1);
    assert_eq!(branch.outcome.metrics.dead_lettered, 0);
    assert!(branch.outcome.metrics.attempts < baseline.metrics.attempts);

    let diff = &branch.diff;
    assert!(!diff.is_identical());
    // Divergence begins exactly at the fork's PolicyDecision event.
    let fork_event_seq = branch
        .baseline
        .journal
        .events
        .iter()
        .position(|e| {
            e.kind == RunEventKind::PolicyDecision
                && resolve(&branch.baseline.journal, e)["id"]
                    == json!(format!("{}:d{fork_seq}", branch.baseline.report.run_id))
        })
        .map(|i| branch.baseline.journal.events[i].seq);
    assert_eq!(diff.first_divergent_seq, fork_event_seq);
    // The baseline's remaining attempts are the removed work.
    assert!(!diff.removed.is_empty());
    assert!(diff.base_totals.events > diff.branch_totals.events);
    // Step-level evidence: fetch-b's disposition differs at step 0.
    let step0 = diff.step_diffs.iter().find(|s| s.step == 0).unwrap();
    assert!(step0
        .channels
        .iter()
        .any(|c| c.base == Some(json!("dead_lettered")) && c.branch == Some(json!("failed"))));
}

#[test]
fn hybrid_counterfactual_switches_the_acting_policy_after_the_fork() {
    /// Aborts every retryable failure at propensity 1: a deterministic
    /// candidate, so the post-fork evidence is exact.
    #[derive(Debug)]
    struct AbortPolicy;
    impl TwinPolicy for AbortPolicy {
        fn version(&self) -> PolicyVersion {
            PolicyVersion::new("policy-abort-always")
        }
        fn decide(&self, ctx: &DecisionContext<'_>, _draw: f64) -> (DecisionAction, f64) {
            let action = if ctx.family == DecisionFamily::Retry {
                DecisionAction::Abort
            } else {
                ctx.legal_actions.last().cloned().unwrap()
            };
            (action, 1.0)
        }
    }

    // fetch-b's recorded latency (500 ms) exceeds the fork's 100 ms bound:
    // observed as a timeout, then the post-fork policy aborts where the
    // floor would have retried and completed.
    let config = baseline_config();
    let twin = twin();
    let baseline = twin.run(&config).unwrap();
    // The timeout decision for fetch-b's first attempt: the one whose
    // features name the 500 ms recorded latency.
    let fork_seq = baseline
        .decisions
        .iter()
        .find(|d| {
            d.family == DecisionFamily::Timeout && d.features["recorded_latency_ms"] == json!(500)
        })
        .map(|d| d.seq)
        .unwrap();

    let branch = twin
        .counterfactual(
            &config,
            CounterfactualFork {
                decision_seq: fork_seq,
                action: DecisionAction::SetTimeout { millis: 100 },
                then_act_with: Some(Arc::new(AbortPolicy)),
            },
        )
        .unwrap();

    // Before the fork, decisions journal under static-v0; after it, under
    // the candidate — hybrid replay: recorded behavior up to the fork, the
    // new policy afterward.
    let versions: Vec<PolicyVersion> = branch
        .outcome
        .decisions
        .iter()
        .filter(|d| d.role == Some(DecisionRole::Acting) && d.seq > fork_seq)
        .map(|d| d.policy_version.clone())
        .collect();
    assert!(!versions.is_empty());
    assert!(versions
        .iter()
        .all(|v| *v == PolicyVersion::new("policy-abort-always")));
    // The post-fork retry decision aborted the item the floor completed.
    assert_eq!(branch.outcome.metrics.failed, 1);
    assert_eq!(branch.outcome.metrics.completed, 3);
    assert!(!branch.diff.is_identical());
}

#[test]
fn counterfactual_refuses_actions_outside_the_legal_set() {
    let outcome = twin().run(&baseline_config().with_faults(
        FaultSchedule::new(42).with_injection(
            FaultAnchor::OnAttempt {
                effect_seq: FETCH_B_SEQ,
                attempt: 1,
            },
            InjectedFault::RateLimited {
                retry_after_ms: 100,
            },
        ),
    ));
    let outcome = outcome.unwrap();
    let retry_seq = first_retry_decision_seq(&outcome);

    // A SetTimeout action at a retry decision: not a member of the
    // recomputed legal set — refused, and the refusal names the case.
    let error = twin()
        .counterfactual(
            &baseline_config().with_faults(FaultSchedule::new(42).with_injection(
                FaultAnchor::OnAttempt {
                    effect_seq: FETCH_B_SEQ,
                    attempt: 1,
                },
                InjectedFault::RateLimited {
                    retry_after_ms: 100,
                },
            )),
            CounterfactualFork {
                decision_seq: retry_seq,
                action: DecisionAction::SetTimeout { millis: 5_000 },
                then_act_with: None,
            },
        )
        .unwrap_err();
    let message = error.to_string();
    assert!(message.contains("not in the legal set"), "{message}");
    assert!(message.contains("illegal_fork_action"), "{message}");
}

#[test]
fn counterfactual_refuses_a_decision_the_run_never_reaches() {
    let error = twin()
        .counterfactual(
            &baseline_config(),
            CounterfactualFork {
                decision_seq: 9_999,
                action: DecisionAction::Abort,
                then_act_with: None,
            },
        )
        .unwrap_err();
    let message = error.to_string();
    assert!(message.contains("never reaches decision"), "{message}");
    assert!(message.contains("unknown_decision"), "{message}");
}

// ---------- mechanism 4: shadow policies ----------

/// A candidate that retries aggressively (always `Retry` when legal) with
/// an exploration draw: 30 % of the time it explores `Abort`, declaring
/// its true propensity. Deterministic under a seeded run.
#[derive(Debug)]
struct ExploringCandidate;
impl TwinPolicy for ExploringCandidate {
    fn version(&self) -> PolicyVersion {
        PolicyVersion::new("policy-candidate-exploring")
    }
    fn decide(&self, ctx: &DecisionContext<'_>, draw: f64) -> (DecisionAction, f64) {
        if ctx.family != DecisionFamily::Retry {
            // Defer to the floor's stance outside the retry family.
            let action = match ctx.family {
                DecisionFamily::WorkerPlacement => ctx.legal_actions.first().cloned().unwrap(),
                _ => ctx.legal_actions.last().cloned().unwrap(),
            };
            return (action, 1.0);
        }
        let retry = ctx
            .legal_actions
            .iter()
            .find(|a| matches!(a, DecisionAction::Retry { .. }))
            .cloned();
        match (retry, draw < 0.3) {
            (Some(_), true) => (DecisionAction::Abort, 0.3),
            (Some(retry), false) => (retry, 0.7),
            (None, _) => (DecisionAction::Abort, 1.0),
        }
    }
}

#[test]
fn shadow_decisions_journal_with_roles_propensities_and_divergences() {
    // Faults on fetch-b's first two attempts give the run two retry
    // decision points.
    let config = baseline_config()
        .with_shadow(Arc::new(ExploringCandidate))
        .with_faults(
            (1..=2u32).fold(FaultSchedule::new(42), |schedule, attempt| {
                schedule.with_injection(
                    FaultAnchor::OnAttempt {
                        effect_seq: FETCH_B_SEQ,
                        attempt,
                    },
                    InjectedFault::RateLimited {
                        retry_after_ms: 100,
                    },
                )
            }),
        );
    let outcome = twin().run(&config).unwrap();

    // Every decision point journaled exactly one acting and one shadow
    // decision, as PolicyDecision events.
    let acting: Vec<&DecisionEvent> = outcome
        .decisions
        .iter()
        .filter(|d| d.role == Some(DecisionRole::Acting))
        .collect();
    let shadow: Vec<&DecisionEvent> = outcome
        .decisions
        .iter()
        .filter(|d| d.role == Some(DecisionRole::Shadow))
        .collect();
    assert_eq!(acting.len(), shadow.len());
    assert!(acting.len() >= 2);
    for shadow_decision in &shadow {
        assert_eq!(
            shadow_decision.policy_version,
            PolicyVersion::new("policy-candidate-exploring")
        );
        assert!(
            shadow_decision.propensity > 0.0 && shadow_decision.propensity <= 1.0,
            "shadow propensity is truthful: {}",
            shadow_decision.propensity
        );
    }
    // The floor acted throughout (the candidate's Abort never executed).
    for acting_decision in &acting {
        assert_eq!(
            acting_decision.policy_version,
            PolicyVersion::new(PolicyVersion::STATIC_V0)
        );
        assert_eq!(acting_decision.propensity, 1.0);
    }

    // Reported divergences are exactly the decision points where the two
    // selections differ.
    let expected: Vec<u64> = acting
        .iter()
        .zip(&shadow)
        .filter(|(a, s)| a.selected != s.selected)
        .map(|(a, _)| a.seq)
        .collect();
    assert_eq!(outcome.report.shadow_divergences, expected);
    // With seeded exploration at 30 %, the divergences are whatever the
    // seed produced — but they are *evidence*: each names a decision where
    // a counterfactual branch could estimate the candidate's outcome.

    // The shadow pair reproduces exactly under re-run.
    let rerun = twin().run(&config).unwrap();
    assert_snapshots_identical(&outcome.journal, &rerun.journal);
    assert_eq!(outcome.decisions, rerun.decisions);

    // The journaled PolicyDecision payloads carry the role marker.
    let shadow_event = outcome
        .journal
        .events
        .iter()
        .find(|e| {
            e.kind == RunEventKind::PolicyDecision
                && resolve(&outcome.journal, e)["role"] == json!("shadow")
        })
        .expect("a shadow decision is journaled with its role");
    assert_eq!(
        resolve(&outcome.journal, shadow_event)["policy_version"],
        json!("policy-candidate-exploring")
    );
}

#[test]
fn a_policy_violating_the_contract_is_rejected_not_journaled() {
    /// Declares a zero propensity: dishonest evidence.
    #[derive(Debug)]
    struct ZeroPropensity;
    impl TwinPolicy for ZeroPropensity {
        fn version(&self) -> PolicyVersion {
            PolicyVersion::new("policy-zero")
        }
        fn decide(&self, ctx: &DecisionContext<'_>, _draw: f64) -> (DecisionAction, f64) {
            (ctx.legal_actions[0].clone(), 0.0)
        }
    }
    let error = twin()
        .run(&baseline_config().with_acting(Arc::new(ZeroPropensity)))
        .unwrap_err();
    assert!(error.to_string().contains("propensity"), "{error}");
}

// ---------- the honest edge, stated in every report ----------

#[test]
fn every_report_states_the_validity_bound() {
    let twin = twin();
    let baseline = twin.run(&baseline_config()).unwrap();
    assert_eq!(baseline.report.bound, TWIN_REPORT_BOUND);
    assert_eq!(
        baseline.report.evaluable_decisions,
        baseline.report.decisions
    );
    assert!(baseline.report.excluded.is_empty());

    let faulted = twin
        .run(
            &baseline_config().with_faults(FaultSchedule::new(42).with_injection(
                FaultAnchor::OnAttempt {
                    effect_seq: FETCH_B_SEQ,
                    attempt: 1,
                },
                InjectedFault::WorkerCrash,
            )),
        )
        .unwrap();
    assert_eq!(faulted.report.bound, TWIN_REPORT_BOUND);
}

// ---------- fixture interop ----------

#[test]
fn a_recorded_production_run_becomes_a_twin_case_unmodified() {
    // The checked-in exact-replay fixture is the twin's input format: a
    // recorded run becomes a twin case by export, unmodified.
    let json = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("exact_replay_agent_tools.json"),
    )
    .unwrap();
    let fixture = ReplayFixture::import(&json).unwrap();
    let twin = Twin::from_fixture(&fixture).unwrap();
    assert_eq!(twin.world().items().len(), 2);
    assert_eq!(twin.world().steps(), &[0, 1]);

    let outcome = twin.run(&baseline_config()).unwrap();
    assert_eq!(outcome.metrics.completed, 2);
    // The recorded answers reproduce; the model call's recorded latency
    // and payload survive the twin unchanged.
    let effects = effect_events(&outcome.journal);
    let recorded_effects = effect_events(&fixture.journal);
    for (served, original) in effects.iter().zip(recorded_effects) {
        assert_eq!(served.latency_ms, original.latency_ms);
        assert_eq!(
            resolve(&outcome.journal, served),
            resolve(&fixture.journal, original)
        );
    }
    // Determinism holds over the imported fixture too.
    let rerun = twin.run(&baseline_config()).unwrap();
    assert_snapshots_identical(&outcome.journal, &rerun.journal);
}

// ---------- configuration boundaries ----------

#[test]
fn empty_pools_and_sub_minimum_ladders_are_rejected() {
    let mut config = baseline_config();
    config.workers.clear();
    let error = twin().run(&config).unwrap_err();
    assert!(
        error.to_string().contains("worker pool is empty"),
        "{error}"
    );

    let mut config = baseline_config();
    config.timeout_ladder = vec![10, 1_000];
    let error = twin().run(&config).unwrap_err();
    assert!(error.to_string().contains("minimum"), "{error}");
}

// Silence a dead-code lint for the seq constants used across test groups.
#[allow(dead_code)]
const _: (u64, u64) = (FETCH_A_SEQ, FETCH_B_SEQ);

// ErrorClass is part of the asserted vocabulary; referenced here so the
// import is load-bearing even where assertions go through JSON.
#[allow(dead_code)]
fn error_class_vocabulary() {
    let _ = [
        ErrorClass::Unknown,
        ErrorClass::RateLimited,
        ErrorClass::ResourceExhausted,
        ErrorClass::Timeout,
    ];
}
