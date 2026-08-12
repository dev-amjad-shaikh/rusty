//! Exact-replay integration tests (R0.5, second work item).
//!
//! Five test groups:
//!
//! - **Exact replay** — a seeded run recorded with `RecordingChatModel` /
//!   `RecordingTool` replays byte-identically (journal and final state) when
//!   the effect implementations are swapped for `ReplayingChatModel` /
//!   `ReplayingTool` wrapping panic-on-call sentinels: the zero-outbound
//!   guarantee is proven by the sentinels never firing.
//! - **Failure modes** — tampered journals are rejected at the boundary; a
//!   graph that diverges from the journaled requests fails loudly; a graph
//!   that stops short leaves recorded effects unserved and fails
//!   verification.
//! - **Interrupts** — an interrupted recorded run replays to the same
//!   suspension, byte-identical journal included.
//! - **Branch diff** — two branches of one recorded history diff into the
//!   expected divergence point, added/removed events, step-level channel
//!   diffs, and per-branch token/cost totals.
//! - **Fixtures** — export → import → replay round-trip, plus the checked-in
//!   example fixture under `tests/fixtures/` replayed end to end. To
//!   regenerate the fixture after an intentional contract change, re-run with
//!   `UPDATE_FIXTURE=1` and review the diff.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};

use rusty_agent_runtime::checkpoint::{Checkpoint, Checkpointer, InMemoryCheckpointer};
use rusty_agent_runtime::error::{Result as RustyResult, RustyError};
use rusty_agent_runtime::executor::{ExecutionOutcome, Executor, RunConfig};
use rusty_agent_runtime::graph::{Graph, GraphBuilder};
use rusty_agent_runtime::journal::{Clock, Journal, JournalSnapshot, RngSource, PARENT_EVENT_KEY};
use rusty_agent_runtime::llm::{ChatMessage, ChatModel, ChatResponse, Usage};
use rusty_agent_runtime::node::{NodeContext, NodeOutput};
use rusty_agent_runtime::record::Effect;
use rusty_agent_runtime::replay::{
    BranchDiff, ExactReplay, LogicalClockParams, RecordingChatModel, RecordingTool, ReplayFixture,
    ReplayOutcome, ReplayParams, ReplaySource, ReplayingChatModel, ReplayingTool,
};
use rusty_agent_runtime::state::{Reducer, State, StateSpec};
use rusty_agent_runtime::tool::Tool;

// ---------- determinism parameters shared by record and replay ----------

const CLOCK_START_MS: u64 = 1_700_000_000_000;
const CLOCK_TICK_MS: u64 = 10;
const RNG_SEED: u64 = 7;
const RUN_ID: &str = "run-replay";
const THREAD_ID: &str = "t-replay";

fn logical_clock() -> Clock {
    Clock::logical(CLOCK_START_MS, CLOCK_TICK_MS)
}

fn clock_params() -> LogicalClockParams {
    LogicalClockParams {
        start_ms: CLOCK_START_MS,
        tick_ms: CLOCK_TICK_MS,
    }
}

// ---------- models and tools: real (mock), replay sentinel, divergent ----------

/// A mock chat model: no network, fixed response and usage. The stand-in for
/// a real provider client when recording.
struct MockModel;

#[async_trait::async_trait]
impl ChatModel for MockModel {
    async fn chat(&self, _messages: &[ChatMessage], _tools: &[Value]) -> RustyResult<ChatResponse> {
        Ok(ChatResponse {
            message: ChatMessage::assistant("pong"),
            model: Some("mock-model-v1".into()),
            usage: Some(Usage {
                prompt_tokens: 12,
                completion_tokens: 3,
                total_tokens: 15,
                cached_tokens: None,
                reasoning_tokens: None,
            }),
        })
    }
}

/// A model that panics if it is ever called. Exact replay must never reach
/// it — the counter makes "never called" assertable rather than implied.
struct PanicModel {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ChatModel for PanicModel {
    async fn chat(&self, _messages: &[ChatMessage], _tools: &[Value]) -> RustyResult<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        panic!("exact replay hit the network: PanicModel was invoked")
    }
}

/// A lookup tool with an honest effect declaration.
struct EchoTool;

#[async_trait::async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "Echoes its input text."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {"text": {"type": "string"}}})
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, args: Value) -> RustyResult<Value> {
        Ok(json!(args.get("text").cloned().unwrap_or(Value::Null)))
    }
}

/// A tool that panics if it is ever called.
struct PanicTool {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Tool for PanicTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "Echoes its input text."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {"text": {"type": "string"}}})
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, _args: Value) -> RustyResult<Value> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        panic!("exact replay hit the network: PanicTool was invoked")
    }
}

/// The causal parent handed to the current invocation by the executor.
fn parent_event(ctx: &NodeContext) -> String {
    ctx.config()
        .extra
        .get(PARENT_EVENT_KEY)
        .and_then(Value::as_str)
        .expect("executor must set the parent event key")
        .to_owned()
}

// ---------- the graph: same topology in record and replay mode ----------

/// The record-mode graph: `agent` calls the model through a recording
/// wrapper, `tools` calls the echo tool through a recording wrapper. Both
/// journal into `journal`.
fn record_graph(journal: &Journal, prompt: &'static str) -> (Graph, StateSpec) {
    let spec = StateSpec::new().channel("log", Reducer::Append);
    let mut builder = GraphBuilder::new();

    let agent_journal = journal.clone();
    builder.add_node("agent", move |ctx: NodeContext| {
        let journal = agent_journal.clone();
        async move {
            let model = RecordingChatModel::new(Arc::new(MockModel), journal, parent_event(&ctx))
                .node("agent");
            let response = model.chat(&[ChatMessage::user(prompt)], &[]).await?;
            let text = response.message.content.unwrap_or_default();
            Ok(NodeOutput::update("log", json!(format!("agent:{text}"))))
        }
    });

    let tools_journal = journal.clone();
    builder.add_node("tools", move |ctx: NodeContext| {
        let journal = tools_journal.clone();
        async move {
            let tool =
                RecordingTool::new(Arc::new(EchoTool), journal, parent_event(&ctx)).node("tools");
            let result = tool.call(json!({"text": "hello"})).await?;
            Ok(NodeOutput::update(
                "log",
                json!(format!("tools:{}", result.as_str().unwrap_or_default())),
            ))
        }
    });

    builder.set_entry_point("agent");
    builder.add_edge("agent", "tools");
    (builder.compile().unwrap(), spec)
}

/// The replay-mode graph: identical topology and node logic, but the model
/// and tool are replaying wrappers around panic-on-call sentinels.
fn replay_graph(
    journal: &Journal,
    source: &ReplaySource,
    model_calls: Arc<AtomicUsize>,
    tool_calls: Arc<AtomicUsize>,
) -> (Graph, StateSpec) {
    let spec = StateSpec::new().channel("log", Reducer::Append);
    let mut builder = GraphBuilder::new();

    let agent_journal = journal.clone();
    let agent_source = source.clone();
    builder.add_node("agent", move |ctx: NodeContext| {
        let journal = agent_journal.clone();
        let source = agent_source.clone();
        let model_calls = model_calls.clone();
        async move {
            let model = ReplayingChatModel::new(
                Arc::new(PanicModel { calls: model_calls }),
                source,
                journal,
                parent_event(&ctx),
            );
            let response = model.chat(&[ChatMessage::user("ping")], &[]).await?;
            let text = response.message.content.unwrap_or_default();
            Ok(NodeOutput::update("log", json!(format!("agent:{text}"))))
        }
    });

    let tools_journal = journal.clone();
    let tools_source = source.clone();
    builder.add_node("tools", move |ctx: NodeContext| {
        let journal = tools_journal.clone();
        let source = tools_source.clone();
        let tool_calls = tool_calls.clone();
        async move {
            let tool = ReplayingTool::new(
                Arc::new(PanicTool { calls: tool_calls }),
                source,
                journal,
                parent_event(&ctx),
            );
            let result = tool.call(json!({"text": "hello"})).await?;
            Ok(NodeOutput::update(
                "log",
                json!(format!("tools:{}", result.as_str().unwrap_or_default())),
            ))
        }
    });

    builder.set_entry_point("agent");
    builder.add_edge("agent", "tools");
    (builder.compile().unwrap(), spec)
}

/// One recorded run of the model/tool graph with the full determinism seam
/// set: attached journal on a logical clock, seeded RNG, in-memory
/// checkpointer. Returns the journal snapshot and the final checkpoint.
async fn record_run(prompt: &'static str) -> (JournalSnapshot, Checkpoint) {
    let checkpointer = Arc::new(InMemoryCheckpointer::new());
    let executor = Executor::with_checkpointer(checkpointer.clone());
    let journal = Journal::new(RUN_ID, THREAD_ID, logical_clock());
    let (graph, spec) = record_graph(&journal, prompt);

    let outcome = executor
        .run(
            &graph,
            &spec,
            State::new(),
            RunConfig::new(THREAD_ID)
                .with_journal(journal.clone())
                .with_rng(RngSource::seeded(RNG_SEED))
                .with_graph_version("replay-fixture-v1"),
        )
        .await
        .unwrap();
    match outcome {
        ExecutionOutcome::Done(state) => {
            assert_eq!(
                state.get("log"),
                Some(&json!(["agent:pong", "tools:hello"]))
            );
        }
        other => panic!("expected Done, got {other:?}"),
    }

    let history = checkpointer.list(THREAD_ID).await.unwrap();
    let final_checkpoint = history.last().expect("the run wrote checkpoints").clone();
    (journal.snapshot(), final_checkpoint)
}

/// Replay a recorded snapshot against panic-on-call sentinels; returns the
/// verified replay outcome plus the sentinel counters.
async fn replay_with_sentinels(
    snapshot: &JournalSnapshot,
) -> (ReplayOutcome, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let replay = ExactReplay::new(snapshot.clone()).unwrap();
    let journal = replay.fresh_journal(logical_clock());
    let model_calls = Arc::new(AtomicUsize::new(0));
    let tool_calls = Arc::new(AtomicUsize::new(0));
    let (graph, spec) = replay_graph(
        &journal,
        &replay.source(),
        model_calls.clone(),
        tool_calls.clone(),
    );
    let params = ReplayParams::new(journal, RngSource::seeded(RNG_SEED))
        .with_checkpointer(Arc::new(InMemoryCheckpointer::new()));
    let outcome = replay
        .run_and_verify(&graph, &spec, State::new(), params)
        .await
        .unwrap();
    (outcome, model_calls, tool_calls)
}

// ---------- exact replay ----------

#[tokio::test]
async fn exact_replay_reproduces_journal_and_state_byte_identically() {
    let (snapshot, final_checkpoint) = record_run("ping").await;

    let (replayed, model_calls, tool_calls) = replay_with_sentinels(&snapshot).await;

    // The zero-outbound guarantee: replaying wrappers never invoked the
    // wrapped implementations — every effect was served from the journal.
    assert_eq!(model_calls.load(Ordering::SeqCst), 0);
    assert_eq!(tool_calls.load(Ordering::SeqCst), 0);

    // Byte-identical evidence (run_and_verify already asserted structural
    // equality; the serialized bytes are the claim this feature makes).
    let recorded_bytes = serde_json::to_string(&snapshot).unwrap();
    let replayed_bytes = serde_json::to_string(&replayed.journal).unwrap();
    assert_eq!(recorded_bytes, replayed_bytes);

    // The replayed outcome matches the recorded final state.
    match &replayed.outcome {
        ExecutionOutcome::Done(state) => {
            assert_eq!(
                state.get("log"),
                Some(&json!(["agent:pong", "tools:hello"]))
            );
            assert_eq!(state, &final_checkpoint.state);
        }
        other => panic!("expected Done, got {other:?}"),
    }
}

#[tokio::test]
async fn replay_serves_artifact_backed_payloads() {
    // A response above the inline threshold is content-addressed at record
    // time; replay must resolve it and re-journal it identically.
    struct BigModel;
    #[async_trait::async_trait]
    impl ChatModel for BigModel {
        async fn chat(
            &self,
            _messages: &[ChatMessage],
            _tools: &[Value],
        ) -> RustyResult<ChatResponse> {
            Ok(ChatResponse {
                message: ChatMessage::assistant("x".repeat(9000)),
                model: None,
                usage: None,
            })
        }
    }

    let spec = StateSpec::new().channel("log", Reducer::Append);
    let mut builder = GraphBuilder::new();
    let record_journal = Journal::new(RUN_ID, THREAD_ID, logical_clock());
    let j = record_journal.clone();
    builder.add_node("agent", move |ctx: NodeContext| {
        let journal = j.clone();
        async move {
            let model = RecordingChatModel::new(Arc::new(BigModel), journal, parent_event(&ctx))
                .node("agent");
            let response = model.chat(&[ChatMessage::user("ping")], &[]).await?;
            let size = response.message.content.unwrap_or_default().len();
            Ok(NodeOutput::update("log", json!(size)))
        }
    });
    builder.set_entry_point("agent");
    let graph = builder.compile().unwrap();

    Executor::new()
        .run(
            &graph,
            &spec,
            State::new(),
            RunConfig::new(THREAD_ID)
                .with_journal(record_journal.clone())
                .with_rng(RngSource::seeded(RNG_SEED)),
        )
        .await
        .unwrap();
    let snapshot = record_journal.snapshot();
    assert!(
        !snapshot.artifacts.is_empty(),
        "response must be artifact-backed"
    );

    // Replay with a sentinel: the artifact-backed response is served whole.
    let replay = ExactReplay::new(snapshot.clone()).unwrap();
    let journal = replay.fresh_journal(logical_clock());
    let source = replay.source();
    let mut builder = GraphBuilder::new();
    let j = journal.clone();
    builder.add_node("agent", move |ctx: NodeContext| {
        let journal = j.clone();
        let source = source.clone();
        async move {
            let model = ReplayingChatModel::new(
                Arc::new(PanicModel {
                    calls: Arc::new(AtomicUsize::new(0)),
                }),
                source,
                journal,
                parent_event(&ctx),
            );
            let response = model.chat(&[ChatMessage::user("ping")], &[]).await?;
            let size = response.message.content.unwrap_or_default().len();
            Ok(NodeOutput::update("log", json!(size)))
        }
    });
    builder.set_entry_point("agent");
    let graph = builder.compile().unwrap();

    let replayed = replay
        .run_and_verify(
            &graph,
            &spec,
            State::new(),
            ReplayParams::new(journal, RngSource::seeded(RNG_SEED)),
        )
        .await
        .unwrap();
    match &replayed.outcome {
        ExecutionOutcome::Done(state) => assert_eq!(state.get("log"), Some(&json!([9000]))),
        other => panic!("expected Done, got {other:?}"),
    }
    assert_eq!(
        serde_json::to_string(&snapshot).unwrap(),
        serde_json::to_string(&replayed.journal).unwrap()
    );
}

// ---------- failure modes ----------

#[tokio::test]
async fn divergent_graph_fails_loudly_with_sequence_and_hashes() {
    // Record with prompt "ping", then replay against a graph whose node
    // issues a different request — the run diverged from its evidence.
    let (snapshot, _) = record_run("ping").await;

    let replay = ExactReplay::new(snapshot).unwrap();
    let journal = replay.fresh_journal(logical_clock());
    let source = replay.source();
    let spec = StateSpec::new().channel("log", Reducer::Append);
    let mut builder = GraphBuilder::new();
    let j = journal.clone();
    builder.add_node("agent", move |ctx: NodeContext| {
        let journal = j.clone();
        let source = source.clone();
        async move {
            let model = ReplayingChatModel::new(
                Arc::new(PanicModel {
                    calls: Arc::new(AtomicUsize::new(0)),
                }),
                source,
                journal,
                parent_event(&ctx),
            );
            // The divergent request: not what the journal recorded.
            let response = model
                .chat(&[ChatMessage::user("PING-DIVERGED")], &[])
                .await?;
            let text = response.message.content.unwrap_or_default();
            Ok(NodeOutput::update("log", json!(format!("agent:{text}"))))
        }
    });
    builder.add_node("tools", |_ctx: NodeContext| async {
        Ok(NodeOutput::empty())
    });
    builder.set_entry_point("agent");
    builder.add_edge("agent", "tools");
    let graph = builder.compile().unwrap();

    let error = replay
        .run(
            &graph,
            &spec,
            State::new(),
            ReplayParams::new(journal, RngSource::seeded(RNG_SEED))
                .with_checkpointer(Arc::new(InMemoryCheckpointer::new())),
        )
        .await
        .unwrap_err();
    let message = error.to_string();
    assert!(
        matches!(error, RustyError::Node(_)),
        "node wraps: {message}"
    );
    assert!(message.contains("divergence"), "got: {message}");
    assert!(message.contains("seq 2"), "got: {message}");
}

#[tokio::test]
async fn replay_that_stops_short_fails_verification() {
    // The replayed graph answers the model call but never makes the tool
    // call: one recorded effect remains unserved.
    let (snapshot, _) = record_run("ping").await;

    let replay = ExactReplay::new(snapshot).unwrap();
    let journal = replay.fresh_journal(logical_clock());
    let source = replay.source();
    let spec = StateSpec::new().channel("log", Reducer::Append);
    let mut builder = GraphBuilder::new();
    let j = journal.clone();
    builder.add_node("agent", move |ctx: NodeContext| {
        let journal = j.clone();
        let source = source.clone();
        async move {
            let model = ReplayingChatModel::new(
                Arc::new(PanicModel {
                    calls: Arc::new(AtomicUsize::new(0)),
                }),
                source,
                journal,
                parent_event(&ctx),
            );
            let response = model.chat(&[ChatMessage::user("ping")], &[]).await?;
            let text = response.message.content.unwrap_or_default();
            Ok(NodeOutput::update("log", json!(format!("agent:{text}"))))
        }
    });
    // `tools` makes no tool call.
    builder.add_node("tools", |_ctx: NodeContext| async {
        Ok(NodeOutput::update("log", json!("tools:skipped")))
    });
    builder.set_entry_point("agent");
    builder.add_edge("agent", "tools");
    let graph = builder.compile().unwrap();

    let error = replay
        .run_and_verify(
            &graph,
            &spec,
            State::new(),
            ReplayParams::new(journal, RngSource::seeded(RNG_SEED))
                .with_checkpointer(Arc::new(InMemoryCheckpointer::new())),
        )
        .await
        .unwrap_err();
    let message = error.to_string();
    assert!(matches!(error, RustyError::Replay(_)), "got: {message}");
    assert!(message.contains("unserved"), "got: {message}");
}

#[tokio::test]
async fn tampered_journal_is_rejected_before_replay() {
    let (snapshot, _) = record_run("ping").await;

    // Flip a recorded response.
    let mut tampered = snapshot.clone();
    let event = tampered
        .events
        .iter_mut()
        .find(|e| matches!(e.kind, rusty_agent_runtime::record::RunEventKind::ModelCall))
        .unwrap();
    event.output = Some(rusty_agent_runtime::record::PayloadRef::inline(json!({
        "message": {"role": "assistant", "content": "forged"},
        "model": null,
        "usage": null,
    })));
    let error = ExactReplay::new(tampered).unwrap_err();
    assert!(matches!(
        error,
        RustyError::Serialization(_) | RustyError::Replay(_)
    ));
}

// ---------- interrupts ----------

#[tokio::test]
async fn interrupted_run_replays_to_the_same_suspension() {
    let build_gate = || {
        let spec = StateSpec::new().channel("answer", Reducer::Overwrite);
        let mut builder = GraphBuilder::new();
        builder.add_node("gate", |ctx: NodeContext| async move {
            match ctx.resume_value() {
                Some(v) => Ok(NodeOutput::update("answer", v.clone())),
                None => Err(ctx.interrupt(json!({"question": "approve?"}))),
            }
        });
        builder.set_entry_point("gate");
        (builder.compile().unwrap(), spec)
    };

    // Record the suspension.
    let (graph, spec) = build_gate();
    let checkpointer = Arc::new(InMemoryCheckpointer::new());
    let executor = Executor::with_checkpointer(checkpointer);
    let journal = Journal::new(RUN_ID, THREAD_ID, logical_clock());
    let outcome = executor
        .run(
            &graph,
            &spec,
            State::new(),
            RunConfig::new(THREAD_ID)
                .with_journal(journal.clone())
                .with_rng(RngSource::seeded(RNG_SEED)),
        )
        .await
        .unwrap();
    assert!(outcome.is_interrupted());
    let snapshot = journal.snapshot();

    // Replay: the gate re-derives the same interrupt; the whole journal —
    // including the suspension checkpoint write — reproduces byte-for-byte.
    let (graph, spec) = build_gate();
    let replay = ExactReplay::new(snapshot.clone()).unwrap();
    let replayed = replay
        .run_and_verify(
            &graph,
            &spec,
            State::new(),
            ReplayParams::new(
                replay.fresh_journal(logical_clock()),
                RngSource::seeded(RNG_SEED),
            )
            .with_checkpointer(Arc::new(InMemoryCheckpointer::new())),
        )
        .await
        .unwrap();

    match &replayed.outcome {
        ExecutionOutcome::Interrupted {
            value,
            checkpoint_id,
            ..
        } => {
            assert_eq!(value, &json!({"question": "approve?"}));
            // The suspension checkpoint id was minted from the seeded RNG:
            // same seed, same id.
            let recorded_id = checkpointer_id_of(&snapshot);
            assert_eq!(checkpoint_id, &recorded_id);
        }
        other => panic!("expected Interrupted, got {other:?}"),
    }
    assert_eq!(
        serde_json::to_string(&snapshot).unwrap(),
        serde_json::to_string(&replayed.journal).unwrap()
    );
}

/// The checkpoint id journaled in the snapshot's checkpoint-written event.
fn checkpointer_id_of(snapshot: &JournalSnapshot) -> String {
    snapshot
        .events
        .iter()
        .find(|e| {
            matches!(
                e.kind,
                rusty_agent_runtime::record::RunEventKind::CheckpointWritten
            )
        })
        .and_then(|e| e.output.as_ref())
        .and_then(|payload| match payload {
            rusty_agent_runtime::record::PayloadRef::Inline(value) => {
                value.get("checkpoint_id")?.as_str().map(str::to_owned)
            }
            rusty_agent_runtime::record::PayloadRef::Artifact(_) => None,
        })
        .expect("recorded run journaled a checkpoint id")
}

// ---------- branch diff ----------

#[tokio::test]
async fn branch_diff_between_two_continuations_of_one_history() {
    // Base: the recorded model/tool run. Branch: an exact replay (identical
    // evidence, different run identity fields) — the diff must be empty.
    let (snapshot, _) = record_run("ping").await;
    let (replayed, _, _) = replay_with_sentinels(&snapshot).await;

    let diff = BranchDiff::between(&snapshot, &replayed.journal);
    assert!(diff.is_identical());
    assert!(diff.added.is_empty() && diff.removed.is_empty());
    assert!(diff.step_diffs.is_empty());
    assert_eq!(diff.base_totals.tokens.total_tokens, 15);
    assert_eq!(diff.branch_totals.tokens.total_tokens, 15);

    // A branch that continued differently: recorded with a different prompt,
    // so its model call diverges at the same sequence position.
    let (other, _) = record_run("different-prompt").await;
    let diff = BranchDiff::between(&snapshot, &other);
    assert!(!diff.is_identical());
    // Both runs share the super-step/node-input prefix; the model call is
    // the first logically-different event.
    let seq = diff.first_divergent_seq.unwrap();
    assert_eq!(
        snapshot.events[seq as usize].kind,
        rusty_agent_runtime::record::RunEventKind::ModelCall
    );
    assert_eq!(
        diff.removed.len() as u64,
        snapshot.events.len() as u64 - seq
    );
    assert_eq!(diff.added.len() as u64, other.events.len() as u64 - seq);
    // The step-0 barrier merged the same pre-model state in both branches;
    // step 1's `log` differs (different node-output text downstream is
    // identical here — the prompts only change the model *request*, so the
    // merged channel values agree and no step diff is expected).
    assert!(diff.step_diffs.is_empty());
}

// ---------- fixtures ----------

/// Capture the recorded run as a fixture.
async fn capture_fixture() -> ReplayFixture {
    let (snapshot, final_checkpoint) = record_run("ping").await;
    let (graph, _) = record_graph(&Journal::new(RUN_ID, THREAD_ID, logical_clock()), "ping");
    ReplayFixture::capture(
        "exact-replay-agent-tools",
        &graph,
        "replay-fixture-v1",
        snapshot,
        Some(final_checkpoint),
        Some(clock_params()),
        Some(RNG_SEED),
    )
}

/// Replay a fixture with panic-on-call sentinels through `replay_in_ci`.
async fn replay_fixture_in_ci(
    fixture: &ReplayFixture,
) -> (ReplayOutcome, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let replay = fixture.exact_replay().unwrap();
    let params = fixture.replay_params(&replay).unwrap();
    let model_calls = Arc::new(AtomicUsize::new(0));
    let tool_calls = Arc::new(AtomicUsize::new(0));
    let (graph, spec) = replay_graph(
        &params.journal,
        &replay.source(),
        model_calls.clone(),
        tool_calls.clone(),
    );
    let outcome = fixture
        .replay_in_ci(replay, &graph, &spec, State::new(), params)
        .await
        .unwrap();
    (outcome, model_calls, tool_calls)
}

#[tokio::test]
async fn fixture_export_import_replay_roundtrip() {
    let fixture = capture_fixture().await;
    let wire = fixture.export().unwrap();
    let imported = ReplayFixture::import(&wire).unwrap();
    assert_eq!(imported.export().unwrap(), wire);

    let (outcome, model_calls, tool_calls) = replay_fixture_in_ci(&imported).await;
    assert_eq!(model_calls.load(Ordering::SeqCst), 0);
    assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
    match &outcome.outcome {
        ExecutionOutcome::Done(state) => {
            assert_eq!(
                state.get("log"),
                Some(&json!(["agent:pong", "tools:hello"]))
            );
        }
        other => panic!("expected Done, got {other:?}"),
    }
}

#[tokio::test]
async fn fixture_rejects_a_different_topology() {
    let fixture = capture_fixture().await;
    let replay = fixture.exact_replay().unwrap();
    let params = fixture.replay_params(&replay).unwrap();

    // Same node names but a different edge shape: the topology hash differs.
    let spec = StateSpec::new().channel("log", Reducer::Append);
    let mut builder = GraphBuilder::new();
    builder.add_node("agent", |_ctx: NodeContext| async {
        Ok(NodeOutput::empty())
    });
    builder.add_node("tools", |_ctx: NodeContext| async {
        Ok(NodeOutput::empty())
    });
    builder.set_entry_point("tools");
    builder.add_edge("tools", "agent");
    let wrong_graph = builder.compile().unwrap();

    let error = fixture
        .replay_in_ci(replay, &wrong_graph, &spec, State::new(), params)
        .await
        .unwrap_err();
    assert!(matches!(error, RustyError::Replay(_)));
    assert!(error.to_string().contains("topology"), "got: {error}");
}

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("exact_replay_agent_tools.json")
}

/// The checked-in example fixture replays end to end with panic-on-call
/// sentinels. `UPDATE_FIXTURE=1` regenerates the file after an intentional
/// contract change — the diff is then the change under review.
#[tokio::test]
async fn checked_in_fixture_replays_in_ci() {
    if std::env::var_os("UPDATE_FIXTURE").is_some() {
        let fixture = capture_fixture().await;
        let path = fixture_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("{}\n", fixture.export().unwrap())).unwrap();
        return;
    }

    let path = fixture_path();
    let json = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing fixture `{}`: {e}", path.display()));
    let fixture = ReplayFixture::import(&json).unwrap();
    assert_eq!(fixture.metadata.name, "exact-replay-agent-tools");
    assert_eq!(fixture.metadata.clock, Some(clock_params()));
    assert_eq!(fixture.metadata.rng_seed, Some(RNG_SEED));

    let (outcome, model_calls, tool_calls) = replay_fixture_in_ci(&fixture).await;
    assert_eq!(model_calls.load(Ordering::SeqCst), 0);
    assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
    match &outcome.outcome {
        ExecutionOutcome::Done(state) => {
            assert_eq!(
                state.get("log"),
                Some(&json!(["agent:pong", "tools:hello"]))
            );
        }
        other => panic!("expected Done, got {other:?}"),
    }
}
