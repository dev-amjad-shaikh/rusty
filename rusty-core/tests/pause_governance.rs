//! Governed pause commit tests (EP-03-S11, executor side).
//!
//! A governed interrupt registers its obligations and a pause envelope
//! through the executor's `PauseSink` at the suspension point, with the
//! deployment's default TTL stamped at creation. These tests cover the
//! commit itself, TTL stamping and its non-retroactivity, the loud failure
//! modes (no checkpointer, malformed payload), and the longevity AC: a
//! pause resumed after ninety days and a version upgrade completes
//! identically to a same-day resume.

use std::sync::{Arc, Mutex};

use chrono::Duration;
use rusty_agent_runtime::journal::Clock;
use rusty_agent_runtime::pause::{GOVERNED_PAUSE_KEY, PauseCommit, PauseSink};
use rusty_agent_runtime::prelude::*;
use rusty_agent_runtime::record::{
    ObligationKind, ObligationStatus, RunObligation, ToolIdentityKey, rebind_tool_identities,
};
use serde_json::json;

/// A `PauseSink` that captures every commit instead of persisting it.
#[derive(Clone, Default)]
struct CaptureSink {
    commits: Arc<Mutex<Vec<PauseCommit>>>,
}

impl CaptureSink {
    fn commits(&self) -> Vec<PauseCommit> {
        self.commits
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

#[async_trait::async_trait]
impl PauseSink for CaptureSink {
    async fn commit_pause(
        &self,
        commit: PauseCommit,
    ) -> rusty_agent_runtime::error::Result<()> {
        self.commits
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(commit);
        Ok(())
    }
}

fn approval_obligation(tool_call_id: &str) -> RunObligation {
    RunObligation::open(ObligationKind::Approval {
        scope: "ops".into(),
        sticky_allowed: true,
    })
    .with_tool_call(tool_call_id)
}

/// A one-node graph that suspends governed on the first pass and consumes
/// the resume value on the second.
fn governed_graph(obligations: Vec<RunObligation>) -> (Graph, StateSpec) {
    let spec = StateSpec::new().channel("answer", Reducer::Overwrite);
    let mut builder = GraphBuilder::new();
    builder.add_node("gate", move |ctx: NodeContext| {
        let obligations = obligations.clone();
        async move {
            match ctx.resume_value() {
                Some(v) => Ok(NodeOutput::update("answer", v.clone())),
                None => Err(ctx.interrupt_governed(json!({"question": "approve?"}), obligations)),
            }
        }
    });
    builder.set_entry_point("gate");
    (builder.compile().expect("graph compiles"), spec)
}

#[tokio::test]
async fn governed_pause_commits_obligations_and_envelope() {
    let obligations = vec![
        approval_obligation("tc-1"),
        approval_obligation("tc-2").for_member_run("member-7"),
    ];
    let (graph, spec) = governed_graph(obligations.clone());
    let checkpointer = Arc::new(InMemoryCheckpointer::new());
    let sink = CaptureSink::default();
    let executor = Executor::with_checkpointer(checkpointer).with_pause_sink(Arc::new(sink.clone()));

    let outcome = executor
        .run(
            &graph,
            &spec,
            State::new(),
            RunConfig::new("t-gov").with_run_id("run-1"),
        )
        .await
        .expect("governed interrupt still surfaces as Ok(Interrupted)");

    // The caller sees the clean payload, never the governance wrapper.
    let checkpoint_id = match &outcome {
        ExecutionOutcome::Interrupted {
            value,
            checkpoint_id,
            ..
        } => {
            assert_eq!(value, &json!({"question": "approve?"}));
            assert!(value.get(GOVERNED_PAUSE_KEY).is_none());
            checkpoint_id.clone()
        }
        other => panic!("expected Interrupted, got {other:?}"),
    };

    // One commit: obligations plus the envelope that embeds them.
    let commits = sink.commits();
    assert_eq!(commits.len(), 1);
    let envelope = &commits[0].envelope;
    assert_eq!(envelope.run_id, "run-1");
    assert_eq!(envelope.session_id, "t-gov");
    assert_eq!(envelope.checkpoint_id, checkpoint_id);
    assert_eq!(envelope.obligations.len(), 2);
    assert_eq!(envelope.obligations[0].tool_call_id.as_deref(), Some("tc-1"));
    assert_eq!(
        envelope.obligations[1].member_run_id.as_deref(),
        Some("member-7")
    );
    // Committed obligations are Open by construction, whatever the node
    // claimed, and carry no expiry when the deployment sets no default.
    assert!(
        envelope
            .obligations
            .iter()
            .all(|o| o.status == ObligationStatus::Open && o.expires_at.is_none())
    );
    assert_eq!(commits[0].payload, json!({"question": "approve?"}));
}

#[tokio::test]
async fn governed_pause_carries_tool_identities_for_rebinding() {
    let tool_keys = vec![ToolIdentityKey {
        agent_path: "main".into(),
        qualified_tool_name: "refund".into(),
    }];
    let spec = StateSpec::new().channel("answer", Reducer::Overwrite);
    let mut builder = GraphBuilder::new();
    builder.add_node("gate", move |ctx: NodeContext| {
        let tool_keys = tool_keys.clone();
        async move {
            match ctx.resume_value() {
                Some(v) => Ok(NodeOutput::update("answer", v.clone())),
                None => Err(ctx.interrupt(rusty_agent_runtime::pause::governed_interrupt(
                    json!({"question": "approve refund?"}),
                    vec![approval_obligation("tc-9")],
                    tool_keys,
                ))),
            }
        }
    });
    builder.set_entry_point("gate");
    let graph = builder.compile().expect("graph compiles");

    let sink = CaptureSink::default();
    let executor = Executor::with_checkpointer(Arc::new(InMemoryCheckpointer::new()))
        .with_pause_sink(Arc::new(sink.clone()));
    let outcome = executor
        .run(&graph, &spec, State::new(), RunConfig::new("t-tools"))
        .await
        .expect("governed interrupt surfaces");

    assert!(outcome.is_interrupted());
    let commits = sink.commits();
    assert_eq!(commits.len(), 1);
    assert_eq!(commits[0].envelope.tool_identities.len(), 1);
    // The committed identity rebinds against the live toolset it names.
    let rebound = rebind_tool_identities(&commits[0].envelope, &["refund".to_string()]);
    assert!(
        matches!(
            rebound,
            rusty_agent_runtime::record::ToolRebindingResult::Bound { .. }
        ),
        "the envelope's tool identities must rebind: {rebound:?}"
    );
}

#[tokio::test]
async fn plain_interrupt_commits_nothing() {
    let spec = StateSpec::new().channel("answer", Reducer::Overwrite);
    let mut builder = GraphBuilder::new();
    builder.add_node("gate", |ctx: NodeContext| async move {
        match ctx.resume_value() {
            Some(v) => Ok(NodeOutput::update("answer", v.clone())),
            None => Err(ctx.interrupt(json!({"question": "approve?"}))),
        }
    });
    builder.set_entry_point("gate");
    let graph = builder.compile().expect("graph compiles");

    let sink = CaptureSink::default();
    let executor = Executor::with_checkpointer(Arc::new(InMemoryCheckpointer::new()))
        .with_pause_sink(Arc::new(sink.clone()));
    let outcome = executor
        .run(&graph, &spec, State::new(), RunConfig::new("t-plain"))
        .await
        .expect("plain interrupt surfaces as before");

    assert!(outcome.is_interrupted());
    assert!(
        sink.commits().is_empty(),
        "an ungoverned interrupt stays in-process: no obligations, no envelope"
    );
}

#[tokio::test]
async fn governed_pause_without_checkpointer_fails_loudly() {
    let (graph, spec) = governed_graph(vec![approval_obligation("tc-1")]);
    let sink = CaptureSink::default();
    let executor = Executor::new().with_pause_sink(Arc::new(sink.clone()));

    let err = executor
        .run(&graph, &spec, State::new(), RunConfig::new("t-nodurable"))
        .await
        .expect_err("a governed pause that cannot resume must not register");
    assert!(
        err.to_string().contains("checkpointer"),
        "the failure names the missing durability: {err}"
    );
    assert!(
        sink.commits().is_empty(),
        "no commit may escape a pause that cannot resume"
    );
}

#[tokio::test]
async fn malformed_governed_payload_fails_loudly() {
    let spec = StateSpec::new().channel("answer", Reducer::Overwrite);
    let mut builder = GraphBuilder::new();
    builder.add_node("gate", |ctx: NodeContext| async move {
        Err::<NodeOutput, _>(ctx.interrupt(json!({
            GOVERNED_PAUSE_KEY: {"obligations": "not-a-list"}
        })))
    });
    builder.set_entry_point("gate");
    let graph = builder.compile().expect("graph compiles");

    let sink = CaptureSink::default();
    let executor = Executor::with_checkpointer(Arc::new(InMemoryCheckpointer::new()))
        .with_pause_sink(Arc::new(sink.clone()));
    let err = executor
        .run(&graph, &spec, State::new(), RunConfig::new("t-malformed"))
        .await
        .expect_err("a malformed governed payload fails, never degrades");
    assert!(
        err.to_string().contains("malformed"),
        "the failure names the malformed payload: {err}"
    );
    assert!(sink.commits().is_empty());
}

#[tokio::test]
async fn default_ttl_stamped_at_commit_and_never_retroactive() {
    // First pause under a 30-day deployment default.
    let (graph, spec) = governed_graph(vec![approval_obligation("tc-1")]);
    let sink = CaptureSink::default();
    let executor = Executor::with_checkpointer(Arc::new(InMemoryCheckpointer::new()))
        .with_pause_sink(Arc::new(sink.clone()));
    let outcome = executor
        .run(
            &graph,
            &spec,
            State::new(),
            RunConfig::new("t-ttl-a").with_default_obligation_ttl(Duration::days(30)),
        )
        .await
        .expect("first pause commits");
    assert!(outcome.is_interrupted());

    let first = sink.commits();
    assert_eq!(first.len(), 1);
    let first_obligation = &first[0].envelope.obligations[0];
    let stamped = first_obligation
        .expires_at
        .expect("the default TTL stamps obligations that declare no expiry");
    assert_eq!(
        stamped - first[0].envelope.created_at,
        Duration::days(30),
        "expires_at is the commit-time clock reading plus the default"
    );

    // The deployment changes its default to 10 days. A second pause stamps
    // the new default; the first commit's rows are untouched.
    let outcome = executor
        .run(
            &graph,
            &spec,
            State::new(),
            RunConfig::new("t-ttl-b").with_default_obligation_ttl(Duration::days(10)),
        )
        .await
        .expect("second pause commits");
    assert!(outcome.is_interrupted());

    let commits = sink.commits();
    assert_eq!(commits.len(), 2);
    let second_obligation = &commits[1].envelope.obligations[0];
    assert_eq!(
        second_obligation.expires_at.expect("stamped") - commits[1].envelope.created_at,
        Duration::days(10),
        "pauses after the change pick up the new default"
    );
    assert_eq!(
        commits[0].envelope.obligations[0].expires_at,
        Some(stamped),
        "the earlier row is never rewritten by the later policy"
    );
}

#[tokio::test]
async fn explicit_expiry_survives_the_default() {
    let explicit = chrono::Utc::now() + Duration::days(7);
    let obligation = approval_obligation("tc-1").with_expiry(explicit);
    let (graph, spec) = governed_graph(vec![obligation]);
    let sink = CaptureSink::default();
    let executor = Executor::with_checkpointer(Arc::new(InMemoryCheckpointer::new()))
        .with_pause_sink(Arc::new(sink.clone()));

    let outcome = executor
        .run(
            &graph,
            &spec,
            State::new(),
            RunConfig::new("t-explicit").with_default_obligation_ttl(Duration::days(30)),
        )
        .await
        .expect("pause commits");
    assert!(outcome.is_interrupted());

    let commits = sink.commits();
    assert_eq!(
        commits[0].envelope.obligations[0].expires_at,
        Some(explicit),
        "an explicit expiry is never overwritten by the deployment default"
    );
}

/// AC 1: a run paused with open obligations and no `expires_at` resumes
/// identically after ninety days of clock time and a platform version
/// upgrade (a fresh executor over the same store, the "next-version
/// binary") — same rebinding, same post-resume transcript as a same-day
/// resume.
#[tokio::test]
async fn ninety_day_resume_across_version_upgrade() {
    const DAY_MS: u64 = 86_400_000;
    let t0: u64 = 1_700_000_000_000;

    let obligations = vec![approval_obligation("tc-refund")];
    let (graph, spec) = governed_graph(obligations);
    let checkpointer: Arc<dyn Checkpointer> = Arc::new(InMemoryCheckpointer::new());
    let sink = CaptureSink::default();

    // Pause under the original binary, on the test clock.
    let executor_v1 = Executor::with_checkpointer(Arc::clone(&checkpointer))
        .with_pause_sink(Arc::new(sink.clone()));
    let paused = executor_v1
        .run(
            &graph,
            &spec,
            State::new(),
            RunConfig::new("t-longevity")
                .with_run_id("run-longevity")
                .with_clock(Clock::logical(t0, 1_000)),
        )
        .await
        .expect("the pause commits");
    assert!(paused.is_interrupted());

    let envelope = &sink.commits()[0].envelope;
    assert_eq!(envelope.run_id, "run-longevity");
    assert!(
        envelope
            .obligations
            .iter()
            .all(|o| o.status == ObligationStatus::Open && o.expires_at.is_none()),
        "AC 1's pause: open obligations, no expires_at"
    );
    let paused_at = envelope.created_at;

    // Ninety days pass; the platform upgrades one version. The envelope's
    // schema version passes the new binary's floor check, and its tool
    // identities rebind the same way they would have on day one.
    let executor_v2 = Executor::with_checkpointer(checkpointer);
    envelope
        .check_version()
        .expect("a compatible envelope version survives the upgrade");

    let resumed = executor_v2
        .run(
            &graph,
            &spec,
            State::new(),
            RunConfig::new("t-longevity")
                .with_resume(json!({"approved": true}))
                .with_clock(Clock::logical(t0 + 90 * DAY_MS, 1_000)),
        )
        .await
        .expect("a ninety-day-old pause resumes");
    let (longevity_state, longevity_trail) = match &resumed {
        ExecutionOutcome::Done(state) => (state.clone(), event_trail(&executor_v2)),
        other => panic!("expected Done after the ninety-day resume, got {other:?}"),
    };
    let upgraded_at = executor_v2
        .journal()
        .expect("the run journaled")
        .events()
        .first()
        .expect("a resumed run opens with its Resume event")
        .recorded_at;
    let paused_ms = paused_at.timestamp_millis() as u64;
    let upgraded_ms = upgraded_at.timestamp_millis() as u64;
    assert!(
        paused_ms >= t0 && paused_ms < t0 + DAY_MS,
        "the pause committed on day zero of the test clock"
    );
    assert!(
        upgraded_ms >= t0 + 90 * DAY_MS && upgraded_ms < t0 + 91 * DAY_MS,
        "the resume ran ninety days later on the test clock"
    );

    // The control: the same graph paused and resumed same-day, on the same
    // clock parameters, must produce the identical post-resume transcript.
    let control_executor = Executor::with_checkpointer(Arc::new(InMemoryCheckpointer::new()))
        .with_pause_sink(Arc::new(CaptureSink::default()));
    let paused = control_executor
        .run(
            &graph,
            &spec,
            State::new(),
            RunConfig::new("t-control").with_clock(Clock::logical(t0, 1_000)),
        )
        .await
        .expect("control pauses");
    assert!(paused.is_interrupted());
    let control = control_executor
        .run(
            &graph,
            &spec,
            State::new(),
            RunConfig::new("t-control")
                .with_resume(json!({"approved": true}))
                .with_clock(Clock::logical(t0 + DAY_MS, 1_000)),
        )
        .await
        .expect("control resumes same-day");
    let (control_state, control_trail) = match &control {
        ExecutionOutcome::Done(state) => (state.clone(), event_trail(&control_executor)),
        other => panic!("expected Done for the control, got {other:?}"),
    };

    assert_eq!(
        longevity_state.to_value(),
        control_state.to_value(),
        "the ninety-day resume lands the same state as the same-day resume"
    );
    assert_eq!(
        longevity_trail, control_trail,
        "the post-resume transcript is identical to the control's"
    );
}

/// The resume run's evidence as a comparable trail: event kind, node, and
/// status — ids and timestamps excluded (they legitimately differ between
/// runs and clocks).
fn event_trail(executor: &Executor) -> Vec<String> {
    executor
        .journal()
        .expect("the run journaled")
        .events()
        .iter()
        .map(|e| format!("{:?}/{:?}/{:?}", e.kind, e.node_id, e.status))
        .collect()
}
