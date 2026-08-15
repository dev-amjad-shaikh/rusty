//! Flight Recorder integration tests for the prebuilt ReAct agent (R0.5).
//!
//! - **Recording** — a `create_react_agent_with_recording` run journals
//!   `model_call` / `tool_call` events in the canonical replay-compatible
//!   shapes, each parented to its invocation's node-input event.
//! - **Exact replay** — the recorded run replays byte-identically through
//!   `create_react_agent_replaying` over panic-on-call sentinels (zero
//!   outbound calls), journal and final state included.
//! - **Divergence** — a replayed run that issues a different request fails
//!   loudly instead of improvising.
//! - **Run-owned recording** — plain `create_react_agent` records model and
//!   tool events when the run explicitly attaches a journal. A graph that is
//!   executed without a caller-owned journal keeps the lightweight default.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use rusty_agent_runtime::checkpoint::InMemoryCheckpointer;
use rusty_agent_runtime::error::{Result as RustyResult, RustyError};
use rusty_agent_runtime::executor::{ExecutionOutcome, Executor, RunConfig};
use rusty_agent_runtime::journal::{Clock, Journal, JournalSnapshot, RngSource};
use rusty_agent_runtime::llm::{ChatMessage, ChatModel, ChatResponse, ToolCall};
use rusty_agent_runtime::react::{
    create_react_agent, create_react_agent_replaying, create_react_agent_with_recording,
    AGENT_NODE, MESSAGES_CHANNEL, TOOLS_NODE,
};
use rusty_agent_runtime::record::{Effect, RunEvent, RunEventKind};
use rusty_agent_runtime::replay::{ExactReplay, ReplayParams};
use rusty_agent_runtime::state::{Reducer, State, StateSpec};
use rusty_agent_runtime::tool::{Tool, ToolRegistry};

// ---------- determinism parameters shared by record and replay ----------

const CLOCK_START_MS: u64 = 1_700_000_000_000;
const CLOCK_TICK_MS: u64 = 10;
const RNG_SEED: u64 = 7;
const RUN_ID: &str = "run-react-recording";
const THREAD_ID: &str = "t-react-recording";

fn logical_clock() -> Clock {
    Clock::logical(CLOCK_START_MS, CLOCK_TICK_MS)
}

fn spec() -> StateSpec {
    StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages)
}

fn initial_state() -> State {
    State::from_value(json!({
        MESSAGES_CHANNEL: [serde_json::to_value(ChatMessage::user("say hello")).unwrap()]
    }))
    .unwrap()
}

// ---------- models and tools: scripted (record), panic sentinels (replay) ----------

/// A scripted model: pops one canned response per `chat` call.
struct ScriptedModel {
    script: Mutex<VecDeque<ChatMessage>>,
}

impl ScriptedModel {
    fn react_script() -> Self {
        Self {
            script: Mutex::new(
                vec![
                    ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                        "c1",
                        "echo",
                        json!({"text": "hello"}),
                    )]),
                    ChatMessage::assistant("the echo said: hello"),
                ]
                .into(),
            ),
        }
    }
}

#[async_trait::async_trait]
impl ChatModel for ScriptedModel {
    async fn chat(&self, _messages: &[ChatMessage], _tools: &[Value]) -> RustyResult<ChatResponse> {
        let message = self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| RustyError::Llm("script exhausted".into()))?;
        Ok(ChatResponse {
            message,
            model: Some("scripted-react-1".into()),
            usage: None,
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

/// A tool that panics if it is ever called. Identity (name, description,
/// schema, effect class) is identical to `EchoTool`'s: tool schemas feed
/// the model-call request hash, so the replay registry must match the
/// recorded one byte-for-byte.
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

fn tools() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(EchoTool);
    registry
}

fn sentinel_tools(calls: Arc<AtomicUsize>) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(PanicTool { calls });
    registry
}

// ---------- record ----------

/// Record one ReAct run (scripted model + real echo tool) with the full
/// determinism seam set: attached journal on a logical clock, seeded RNG,
/// in-memory checkpointer. Returns the journal snapshot and the final state.
async fn record_run() -> (JournalSnapshot, State) {
    let checkpointer = Arc::new(InMemoryCheckpointer::new());
    let executor = Executor::with_checkpointer(checkpointer.clone());
    let journal = Journal::new(RUN_ID, THREAD_ID, logical_clock());
    let model: Arc<dyn ChatModel> = Arc::new(ScriptedModel::react_script());
    let graph = create_react_agent_with_recording(model, tools(), journal.clone()).unwrap();

    let outcome = executor
        .run(
            &graph,
            &spec(),
            initial_state(),
            RunConfig::new(THREAD_ID)
                .with_journal(journal.clone())
                .with_rng(RngSource::seeded(RNG_SEED)),
        )
        .await
        .unwrap();
    match outcome {
        ExecutionOutcome::Done(state) => (journal.snapshot(), state),
        other => panic!("expected Done, got {other:?}"),
    }
}

/// The journaled events of one kind, in sequence order.
fn events_of_kind(snapshot: &JournalSnapshot, kind: RunEventKind) -> Vec<&RunEvent> {
    snapshot
        .events
        .iter()
        .filter(|event| event.kind == kind)
        .collect()
}

/// The id of the (single) node-input event for `node` matching `index`
/// (0-based among that node's invocations).
fn node_input_id(snapshot: &JournalSnapshot, node: &str, index: usize) -> String {
    snapshot
        .events
        .iter()
        .filter(|event| {
            event.kind == RunEventKind::NodeInput && event.node_id.as_deref() == Some(node)
        })
        .nth(index)
        .unwrap_or_else(|| panic!("no node-input event #{index} for `{node}`"))
        .id
        .clone()
}

/// An event's inline input payload. The payloads recorded in these tests
/// are far below the inline threshold, so an artifact reference (or a
/// missing payload) is a test failure, not a lookup miss.
fn inline_input(event: &RunEvent) -> Value {
    match event.input.as_ref() {
        Some(rusty_agent_runtime::record::PayloadRef::Inline(value)) => value.clone(),
        other => panic!("expected inline request payload, got {other:?}"),
    }
}

#[tokio::test]
async fn recording_journals_canonical_model_and_tool_events() {
    let (snapshot, state) = record_run().await;

    // Two model calls (tool request, then final answer), one tool call.
    let model_calls = events_of_kind(&snapshot, RunEventKind::ModelCall);
    let tool_calls = events_of_kind(&snapshot, RunEventKind::ToolCall);
    assert_eq!(model_calls.len(), 2);
    assert_eq!(tool_calls.len(), 1);

    // Canonical request shapes, carried by the right node, with the
    // declared effect classes.
    for (iteration, call) in model_calls.iter().enumerate() {
        assert_eq!(call.node_id.as_deref(), Some(AGENT_NODE));
        assert_eq!(call.effect, Effect::NonIdempotent);
        let request = inline_input(call);
        assert!(request.get("messages").is_some_and(Value::is_array));
        assert!(request.get("tools").is_some_and(Value::is_array));
        // Iteration N's model call is parented to iteration N's agent input.
        assert_eq!(
            call.parent.as_deref(),
            Some(node_input_id(&snapshot, AGENT_NODE, iteration).as_str())
        );
    }

    let tool_call = tool_calls[0];
    assert_eq!(tool_call.node_id.as_deref(), Some(TOOLS_NODE));
    assert_eq!(tool_call.effect, Effect::ReadOnly);
    assert_eq!(
        inline_input(tool_call),
        json!({"tool": "echo", "arguments": {"text": "hello"}})
    );
    // The tool call hangs off the tools node's invocation.
    assert_eq!(
        tool_call.parent.as_deref(),
        Some(node_input_id(&snapshot, TOOLS_NODE, 0).as_str())
    );

    // The run completed the loop: the final transcript ends with the answer.
    let messages: Vec<ChatMessage> = state.get_as(MESSAGES_CHANNEL).unwrap().unwrap();
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[3].content.as_deref(), Some("the echo said: hello"));
}

// ---------- exact replay ----------

#[tokio::test]
async fn exact_replay_of_recorded_react_run_is_byte_identical() {
    let (snapshot, recorded_state) = record_run().await;

    let replay = ExactReplay::new(snapshot.clone()).unwrap();
    let journal = replay.fresh_journal(logical_clock());
    let model_calls = Arc::new(AtomicUsize::new(0));
    let tool_calls = Arc::new(AtomicUsize::new(0));
    let model: Arc<dyn ChatModel> = Arc::new(PanicModel {
        calls: model_calls.clone(),
    });
    let graph = create_react_agent_replaying(
        model,
        sentinel_tools(tool_calls.clone()),
        replay.source(),
        journal.clone(),
    )
    .unwrap();
    let params = ReplayParams::new(journal, RngSource::seeded(RNG_SEED))
        .with_checkpointer(Arc::new(InMemoryCheckpointer::new()));
    let replayed = replay
        .run_and_verify(&graph, &spec(), initial_state(), params)
        .await
        .unwrap();

    // The zero-outbound guarantee: replaying wrappers never invoked the
    // wrapped implementations — every effect was served from the journal.
    assert_eq!(model_calls.load(Ordering::SeqCst), 0);
    assert_eq!(tool_calls.load(Ordering::SeqCst), 0);

    // Byte-identical evidence (run_and_verify already asserted structural
    // equality; the serialized bytes are the claim this feature makes).
    assert_eq!(
        serde_json::to_string(&snapshot).unwrap(),
        serde_json::to_string(&replayed.journal).unwrap()
    );

    // The replayed outcome matches the recorded final state.
    match &replayed.outcome {
        ExecutionOutcome::Done(state) => assert_eq!(state, &recorded_state),
        other => panic!("expected Done, got {other:?}"),
    }
}

#[tokio::test]
async fn replay_diverging_from_the_recording_fails_loudly() {
    let (snapshot, _) = record_run().await;

    let replay = ExactReplay::new(snapshot).unwrap();
    let journal = replay.fresh_journal(logical_clock());
    let model: Arc<dyn ChatModel> = Arc::new(PanicModel {
        calls: Arc::new(AtomicUsize::new(0)),
    });
    let graph = create_react_agent_replaying(
        model,
        sentinel_tools(Arc::new(AtomicUsize::new(0))),
        replay.source(),
        journal.clone(),
    )
    .unwrap();

    // A different user question: the replayed run's first model request
    // diverges from the journaled one.
    let diverged_initial = State::from_value(json!({
        MESSAGES_CHANNEL: [serde_json::to_value(ChatMessage::user("DIVERGED")).unwrap()]
    }))
    .unwrap();
    let error = replay
        .run(
            &graph,
            &spec(),
            diverged_initial,
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
}

// ---------- run-owned recording ----------

/// A server-style run owns its journal at admission time, after the reusable
/// graph has already been built. Plain ReAct agents therefore inherit that
/// journal at the model/tool boundaries and emit the same canonical effects
/// as an explicitly recording graph.
#[tokio::test]
async fn plain_react_agent_records_effects_into_an_attached_run_journal() {
    let checkpointer = Arc::new(InMemoryCheckpointer::new());
    let executor = Executor::with_checkpointer(checkpointer.clone());
    let journal = Journal::new(RUN_ID, THREAD_ID, logical_clock());
    let model: Arc<dyn ChatModel> = Arc::new(ScriptedModel::react_script());
    let graph = create_react_agent(model, tools()).unwrap();

    let outcome = executor
        .run(
            &graph,
            &spec(),
            initial_state(),
            RunConfig::new(THREAD_ID)
                .with_journal(journal.clone())
                .with_rng(RngSource::seeded(RNG_SEED)),
        )
        .await
        .unwrap();

    let snapshot = journal.snapshot();
    let model_calls = events_of_kind(&snapshot, RunEventKind::ModelCall);
    let tool_calls = events_of_kind(&snapshot, RunEventKind::ToolCall);
    assert_eq!(model_calls.len(), 2);
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(
        tool_calls[0].parent.as_deref(),
        Some(node_input_id(&snapshot, TOOLS_NODE, 0).as_str())
    );

    // Same conversation as the recording run produces.
    match outcome {
        ExecutionOutcome::Done(state) => {
            let messages: Vec<ChatMessage> = state.get_as(MESSAGES_CHANNEL).unwrap().unwrap();
            assert_eq!(messages.len(), 4);
            assert_eq!(messages[3].content.as_deref(), Some("the echo said: hello"));
        }
        other => panic!("expected Done, got {other:?}"),
    }
}
