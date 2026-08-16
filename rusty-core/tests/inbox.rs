//! Durable inbox integration tests (R0.13 parity wave).
//!
//! - **Steering** — a message sent mid-run is journaled at settlement and
//!   lands at the next super-step boundary as user-role, model-visible
//!   input.
//! - **Follow-ups** — a queued follow-up turns a would-be-finished turn
//!   into another turn of the same run.
//! - **Injection** — staged context rides the next wake and never causes
//!   one on its own.
//! - **Typed cancellation** — the cause and `keep_inbox` disposition are
//!   journaled, queued messages survive (or are dropped) durably, and the
//!   resume that follows continues from the rewritten checkpoint.
//! - **Exact replay** — an inbox-driven run re-driven through
//!   `Inbox::replaying` reproduces its journal byte-for-byte, cancellation
//!   included.
//! - **Empty means absent** — an attached but untouched inbox leaves the
//!   journal and the checkpoints byte-identical to an inbox-free run.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::{json, Value};

use rusty_agent_runtime::checkpoint::{Checkpointer, InMemoryCheckpointer};
use rusty_agent_runtime::error::{Result as RustyResult, RustyError};
use rusty_agent_runtime::executor::{ExecutionOutcome, Executor, RunConfig};
use rusty_agent_runtime::inbox::{
    CancelCause, ConsumptionPoint, DroppedMessages, Inbox, InboxConsumption, InboxKind,
    InboxMessage, RunCancellation,
};
use rusty_agent_runtime::journal::{Clock, Journal, JournalSnapshot, RngSource};
use rusty_agent_runtime::llm::{ChatMessage, ChatModel, ChatResponse, ToolCall};
use rusty_agent_runtime::react::{
    create_react_agent, create_react_agent_replaying, create_react_agent_with_recording,
    MESSAGES_CHANNEL,
};
use rusty_agent_runtime::record::{RunEvent, RunEventKind};
use rusty_agent_runtime::replay::ExactReplay;
use rusty_agent_runtime::state::{Reducer, State, StateSpec};
use rusty_agent_runtime::tool::{Tool, ToolRegistry};

// ---------- determinism parameters shared by record and replay ----------

const CLOCK_START_MS: u64 = 1_700_000_000_000;
const CLOCK_TICK_MS: u64 = 10;
const RNG_SEED: u64 = 7;

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

fn conversation(state: &State) -> Vec<ChatMessage> {
    state.get_as(MESSAGES_CHANNEL).unwrap().unwrap()
}

// ---------- scripted model with per-call side effects ----------

/// A scripted model: pops one canned response per `chat` call, records the
/// messages it was shown, and runs the side effect registered for that call
/// index (sending into the run's inbox from inside the run — the
/// deterministic way to arrive mid-execution).
struct ScriptedModel {
    script: Mutex<VecDeque<ChatMessage>>,
    seen: Mutex<Vec<Vec<ChatMessage>>>,
    on_call: Mutex<HashMap<usize, Box<dyn Fn() + Send + Sync>>>,
}

impl ScriptedModel {
    fn new(script: Vec<ChatMessage>) -> Self {
        Self {
            script: Mutex::new(script.into()),
            seen: Mutex::new(Vec::new()),
            on_call: Mutex::new(HashMap::new()),
        }
    }

    fn with_effect(self, call: usize, effect: impl Fn() + Send + Sync + 'static) -> Self {
        self.on_call.lock().unwrap().insert(call, Box::new(effect));
        self
    }

    /// The messages the `index`-th call was shown.
    fn seen(&self, index: usize) -> Vec<ChatMessage> {
        self.seen.lock().unwrap()[index].clone()
    }
}

#[async_trait::async_trait]
impl ChatModel for ScriptedModel {
    async fn chat(&self, messages: &[ChatMessage], _tools: &[Value]) -> RustyResult<ChatResponse> {
        let index = {
            let mut seen = self.seen.lock().unwrap();
            seen.push(messages.to_vec());
            seen.len() - 1
        };
        if let Some(effect) = self.on_call.lock().unwrap().remove(&index) {
            effect();
        }
        let message = self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| RustyError::Llm("script exhausted".into()))?;
        Ok(ChatResponse {
            message,
            model: None,
            usage: None,
        })
    }
}

/// A model that panics if it is ever called: exact replay must serve every
/// call from the journal.
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

/// The lookup tool every test registers.
struct EchoTool;

#[async_trait::async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "Echoes its input."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {"text": {"type": "string"}}})
    }
    async fn call(&self, args: Value) -> RustyResult<Value> {
        Ok(json!(args.get("text").cloned().unwrap_or(Value::Null)))
    }
}

/// An echo tool that steers the run from inside the tools node — a
/// mid-run arrival with deterministic timing.
struct SteeringTool {
    inbox: Inbox,
    content: String,
}

#[async_trait::async_trait]
impl Tool for SteeringTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "Echoes its input and steers the run."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {"text": {"type": "string"}}})
    }
    async fn call(&self, args: Value) -> RustyResult<Value> {
        self.inbox.steer(json!(self.content))?;
        Ok(json!(args.get("text").cloned().unwrap_or(Value::Null)))
    }
}

/// A tool that panics if called: replay must serve the recorded result. The
/// identity (name, description, schema) is carried for the model-call
/// request hash, so it must mirror the recorded run's tool exactly.
struct PanicTool {
    calls: Arc<AtomicUsize>,
    description: &'static str,
}

#[async_trait::async_trait]
impl Tool for PanicTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        self.description
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {"text": {"type": "string"}}})
    }
    async fn call(&self, _args: Value) -> RustyResult<Value> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        panic!("exact replay executed a tool: PanicTool was invoked")
    }
}

fn echo_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(EchoTool);
    registry
}

fn tool_call_script() -> ChatMessage {
    ChatMessage::assistant_tool_calls(vec![ToolCall::new("c1", "echo", json!({"text": "hello"}))])
}

fn journal_of_kinds(snapshot: &JournalSnapshot, kind: RunEventKind) -> Vec<RunEvent> {
    snapshot
        .events
        .iter()
        .filter(|event| event.kind == kind)
        .cloned()
        .collect()
}

fn output_payload(event: &RunEvent) -> Value {
    match event.output.as_ref().expect("event carries an output") {
        rusty_agent_runtime::record::PayloadRef::Inline(value) => value.clone(),
        other => panic!("expected an inline payload, got {other:?}"),
    }
}

// ---------- steering ----------

#[tokio::test]
async fn steering_lands_at_the_next_step_boundary_journaled_and_model_visible() {
    let inbox = Inbox::new();
    let model = Arc::new(ScriptedModel::new(vec![
        tool_call_script(),
        ChatMessage::assistant("adjusted answer"),
    ]));
    let mut tools = ToolRegistry::new();
    tools.register(SteeringTool {
        inbox: inbox.clone(),
        content: "actually, use metric units".to_owned(),
    });
    let graph = create_react_agent(model.clone(), tools).unwrap();
    let journal = Journal::new("run-inbox-steer", "t-inbox-steer", Clock::System);

    let outcome = Executor::new()
        .run(
            &graph,
            &spec(),
            initial_state(),
            RunConfig::new("t-inbox-steer")
                .with_journal(journal.clone())
                .with_inbox(inbox),
        )
        .await
        .unwrap();
    assert!(matches!(outcome, ExecutionOutcome::Done(_)));

    // The model's second call observed the steering as user-role input,
    // after the tool result.
    let seen = model.seen(1);
    let steering = seen
        .iter()
        .find(|message| message.content.as_deref() == Some("actually, use metric units"))
        .expect("the steering message reached the model");
    assert_eq!(steering.role, rusty_agent_runtime::llm::Role::User);

    // And it joined the persisted conversation ahead of the final answer.
    let messages = conversation(outcome.state());
    let position = messages
        .iter()
        .position(|message| message.content.as_deref() == Some("actually, use metric units"))
        .expect("the steering message joined the channel");
    assert_eq!(messages.len(), 5);
    assert_eq!(position, 3, "user, assistant, tool, steering, answer");

    // The journal fixes the timing: intake at settlement, consumption at
    // the step-2 boundary (the agent's next call after the tools step).
    let snapshot = journal.snapshot();
    let intakes = journal_of_kinds(&snapshot, RunEventKind::InboxIntake);
    assert_eq!(intakes.len(), 1);
    let message: InboxMessage = serde_json::from_value(output_payload(&intakes[0])).unwrap();
    assert_eq!(message.kind, InboxKind::Steering);
    assert_eq!(message.sender, "user");
    assert_eq!(message.seq, 0);

    let consumptions = journal_of_kinds(&snapshot, RunEventKind::InboxConsumed);
    assert_eq!(consumptions.len(), 1);
    let consumption: InboxConsumption =
        serde_json::from_value(output_payload(&consumptions[0])).unwrap();
    assert_eq!(consumption.point, ConsumptionPoint::StepBoundary);
    assert_eq!(consumption.step, 2);
    assert_eq!(consumption.messages, vec![message]);
}

// ---------- follow-ups ----------

#[tokio::test]
async fn followups_extend_a_turn_instead_of_idling() {
    let inbox = Inbox::new();
    let sender = inbox.clone();
    let model = Arc::new(
        ScriptedModel::new(vec![
            ChatMessage::assistant("first answer"),
            ChatMessage::assistant("second answer"),
        ])
        .with_effect(0, move || {
            sender.followup(json!("and one more thing")).unwrap();
        }),
    );
    let graph = create_react_agent(model.clone(), echo_registry()).unwrap();
    let journal = Journal::new("run-inbox-followup", "t-inbox-followup", Clock::System);

    let outcome = Executor::new()
        .run(
            &graph,
            &spec(),
            initial_state(),
            RunConfig::new("t-inbox-followup")
                .with_journal(journal.clone())
                .with_inbox(inbox.clone()),
        )
        .await
        .unwrap();

    // The run continued past the first answer: the follow-up re-activated
    // the agent with the full history plus the new user message.
    let messages = conversation(outcome.state());
    let texts: Vec<&str> = messages
        .iter()
        .filter_map(|message| message.content.as_deref())
        .collect();
    assert_eq!(
        texts,
        [
            "say hello",
            "first answer",
            "and one more thing",
            "second answer"
        ]
    );

    // The extension turn's model call saw the follow-up as user-role input.
    let seen = model.seen(1);
    assert!(seen
        .iter()
        .any(|message| message.content.as_deref() == Some("and one more thing")));

    // The journal shows the turn extension: one intake, one consumption at
    // the turn-end check, and no mid-turn consumption.
    let snapshot = journal.snapshot();
    assert_eq!(
        journal_of_kinds(&snapshot, RunEventKind::InboxIntake).len(),
        1
    );
    let consumptions = journal_of_kinds(&snapshot, RunEventKind::InboxConsumed);
    assert_eq!(consumptions.len(), 1);
    let consumption: InboxConsumption =
        serde_json::from_value(output_payload(&consumptions[0])).unwrap();
    assert_eq!(consumption.point, ConsumptionPoint::TurnExtension);
    assert_eq!(consumption.messages.len(), 1);
    assert_eq!(consumption.messages[0].kind, InboxKind::FollowUp);
    assert!(inbox.is_empty(), "the extension consumed the queue");
}

// ---------- staged injection ----------

#[tokio::test]
async fn injected_context_stages_until_the_loop_next_wakes() {
    let inbox = Inbox::new();
    let injector = inbox.clone();
    let follower = inbox.clone();
    let model = Arc::new(
        ScriptedModel::new(vec![
            tool_call_script(),
            ChatMessage::assistant("done"),
            ChatMessage::assistant("answer with context"),
        ])
        .with_effect(0, move || {
            injector
                .inject(json!("context: the user is in Lisbon"))
                .unwrap();
        })
        .with_effect(1, move || {
            follower.followup(json!("where am I?")).unwrap();
        }),
    );
    let graph = create_react_agent(model.clone(), echo_registry()).unwrap();
    let journal = Journal::new("run-inbox-inject", "t-inbox-inject", Clock::System);

    let outcome = Executor::new()
        .run(
            &graph,
            &spec(),
            initial_state(),
            RunConfig::new("t-inbox-inject")
                .with_journal(journal.clone())
                .with_inbox(inbox),
        )
        .await
        .unwrap();
    assert!(matches!(outcome, ExecutionOutcome::Done(_)));

    // The injection alone did not wake the loop: the second model call (the
    // step boundary right after it settled) ran without it.
    let mid_turn = model.seen(1);
    assert!(
        !mid_turn
            .iter()
            .any(|message| message.content.as_deref() == Some("context: the user is in Lisbon")),
        "staged context must not appear at an ordinary step boundary"
    );

    // The follow-up's turn extension was the wake: the third call carries
    // both the follow-up and the staged context.
    let woken = model.seen(2);
    assert!(woken
        .iter()
        .any(|message| message.content.as_deref() == Some("where am I?")));
    assert!(woken
        .iter()
        .any(|message| message.content.as_deref() == Some("context: the user is in Lisbon")));

    // One consumption event, at the turn extension, holding both messages.
    let snapshot = journal.snapshot();
    let consumptions = journal_of_kinds(&snapshot, RunEventKind::InboxConsumed);
    assert_eq!(consumptions.len(), 1);
    let consumption: InboxConsumption =
        serde_json::from_value(output_payload(&consumptions[0])).unwrap();
    assert_eq!(consumption.point, ConsumptionPoint::TurnExtension);
    assert_eq!(consumption.messages.len(), 2);
    assert_eq!(consumption.messages[0].kind, InboxKind::FollowUp);
    assert_eq!(consumption.messages[1].kind, InboxKind::Injected);
}

// ---------- typed cancellation ----------

/// Drive a run that cancels itself from inside the first model call, with a
/// follow-up already queued. Returns the error, the journal, the inbox, and
/// the checkpointer for the resume assertions.
async fn cancel_run(
    cause: CancelCause,
    keep_inbox: bool,
) -> (RustyError, Journal, Inbox, Arc<InMemoryCheckpointer>) {
    let inbox = Inbox::new();
    inbox.followup(json!("still there?")).unwrap();
    let canceller = inbox.clone();
    let model = Arc::new(
        ScriptedModel::new(vec![
            tool_call_script(),
            ChatMessage::assistant("resumed answer"),
            ChatMessage::assistant("follow-up answer"),
        ])
        .with_effect(0, move || canceller.cancel(cause, keep_inbox)),
    );
    let graph = create_react_agent(model, echo_registry()).unwrap();
    let journal = Journal::new("run-inbox-cancel", "t-inbox-cancel", Clock::System);
    let checkpointer = Arc::new(InMemoryCheckpointer::new());

    let error = Executor::with_checkpointer(checkpointer.clone())
        .run(
            &graph,
            &spec(),
            initial_state(),
            RunConfig::new("t-inbox-cancel")
                .with_journal(journal.clone())
                .with_inbox(inbox.clone()),
        )
        .await
        .expect_err("the run cancels at the next super-step boundary");
    (error, journal, inbox, checkpointer)
}

#[tokio::test]
async fn cancel_journals_the_cause_and_keep_inbox_preserves_the_queue() {
    let (error, journal, inbox, checkpointer) = cancel_run(CancelCause::User, true).await;

    // The typed cause travels in the (still control-flow, not failure)
    // cancellation error.
    let message = error.to_string();
    assert!(matches!(error, RustyError::Cancelled(_)), "got: {message}");
    assert!(message.contains("user"), "got: {message}");

    // The journaled cancellation names the cause, the disposition, and an
    // honest (zero) drop accounting.
    let snapshot = journal.snapshot();
    let cancellations = journal_of_kinds(&snapshot, RunEventKind::RunCancelled);
    assert_eq!(cancellations.len(), 1);
    let cancellation: RunCancellation =
        serde_json::from_value(output_payload(&cancellations[0])).unwrap();
    assert_eq!(cancellation.cause, CancelCause::User);
    assert!(cancellation.keep_inbox);
    assert_eq!(cancellation.dropped, DroppedMessages::default());

    // The queued follow-up survived in the live inbox and in the rewritten
    // boundary checkpoint the resume reads.
    assert_eq!(inbox.len(), 1);
    let checkpoint = checkpointer
        .get_latest("t-inbox-cancel")
        .await
        .unwrap()
        .expect("the cancellation rewrote the boundary checkpoint");
    let stamp = checkpoint
        .header
        .inbox
        .expect("inbox run stamps its queues");
    assert_eq!(stamp.followups.len(), 1);
    assert_eq!(
        stamp.followups[0].content,
        json!("still there?"),
        "keep_inbox preserved the follow-up across the cancellation"
    );
}

#[tokio::test]
async fn cancel_without_keep_inbox_drops_the_queue_durably() {
    let (error, journal, inbox, checkpointer) = cancel_run(CancelCause::Disposed, false).await;
    assert!(matches!(error, RustyError::Cancelled(_)));
    assert!(error.to_string().contains("disposed"));

    // The journal accounts for exactly what was dropped.
    let snapshot = journal.snapshot();
    let cancellations = journal_of_kinds(&snapshot, RunEventKind::RunCancelled);
    let cancellation: RunCancellation =
        serde_json::from_value(output_payload(&cancellations[0])).unwrap();
    assert_eq!(cancellation.cause, CancelCause::Disposed);
    assert!(!cancellation.keep_inbox);
    assert_eq!(
        cancellation.dropped,
        DroppedMessages {
            steering: 0,
            followups: 1,
            staged: 0,
            pending: 0,
        }
    );

    // The drop is durable: the rewritten checkpoint's stamp has no
    // follow-up to restore, and the live inbox is empty.
    assert!(inbox.is_empty());
    let checkpoint = checkpointer
        .get_latest("t-inbox-cancel")
        .await
        .unwrap()
        .unwrap();
    let stamp = checkpoint.header.inbox.unwrap();
    assert!(stamp.followups.is_empty());
}

#[tokio::test]
async fn resume_after_keep_inbox_cancellation_delivers_the_preserved_queue() {
    let (_error, _journal, inbox, checkpointer) = cancel_run(CancelCause::User, true).await;

    // Resume with the same inbox handle (in-process continuity): the
    // preserved follow-up extends the resumed run's first finished turn.
    let model = Arc::new(ScriptedModel::new(vec![
        ChatMessage::assistant("resumed answer"),
        ChatMessage::assistant("follow-up answer"),
    ]));
    let graph = create_react_agent(model.clone(), echo_registry()).unwrap();
    let outcome = Executor::with_checkpointer(checkpointer)
        .run(
            &graph,
            &spec(),
            State::new(),
            RunConfig::new("t-inbox-cancel")
                .with_resume(Value::Null)
                .with_inbox(inbox),
        )
        .await
        .unwrap();

    let messages = conversation(outcome.state());
    let texts: Vec<&str> = messages
        .iter()
        .filter_map(|message| message.content.as_deref())
        .collect();
    assert_eq!(
        texts,
        [
            "say hello",
            "hello", // the tool result of the preserved tool call
            "resumed answer",
            "still there?",
            "follow-up answer",
        ]
    );
    let last_call = model.seen(1);
    assert!(last_call
        .iter()
        .any(|message| message.content.as_deref() == Some("still there?")));
}

// ---------- exact replay ----------

/// Record an inbox-driven run under the determinism seams: steering from
/// inside the tools node, a follow-up from inside the second model call.
async fn record_inbox_run() -> (JournalSnapshot, State) {
    let inbox = Inbox::new();
    let follower = inbox.clone();
    let model = Arc::new(
        ScriptedModel::new(vec![
            tool_call_script(),
            ChatMessage::assistant("first answer"),
            ChatMessage::assistant("second answer"),
        ])
        .with_effect(1, move || {
            follower.followup(json!("and one more thing")).unwrap();
        }),
    );
    let mut tools = ToolRegistry::new();
    tools.register(SteeringTool {
        inbox: inbox.clone(),
        content: "actually, use metric units".to_owned(),
    });
    let journal = Journal::new("run-inbox-replay", "t-inbox-replay", logical_clock());
    let graph = create_react_agent_with_recording(model, tools, journal.clone()).unwrap();

    let outcome = Executor::with_checkpointer(Arc::new(InMemoryCheckpointer::new()))
        .run(
            &graph,
            &spec(),
            initial_state(),
            RunConfig::new("t-inbox-replay")
                .with_clock(logical_clock())
                .with_rng(RngSource::seeded(RNG_SEED))
                .with_journal(journal.clone())
                .with_inbox(inbox),
        )
        .await
        .unwrap();
    match outcome {
        ExecutionOutcome::Done(state) => (journal.snapshot(), state),
        other => panic!("expected Done, got {other:?}"),
    }
}

#[tokio::test]
async fn replay_reproduces_an_inbox_driven_run_exactly() {
    let (snapshot, recorded_state) = record_inbox_run().await;
    // Sanity: the recording exercised every inbox event kind.
    for kind in [RunEventKind::InboxIntake, RunEventKind::InboxConsumed] {
        assert!(
            !journal_of_kinds(&snapshot, kind).is_empty(),
            "recording must exercise {kind:?}"
        );
    }

    let replay = ExactReplay::new(snapshot.clone()).unwrap();
    let journal = replay.fresh_journal(logical_clock());
    let model_calls = Arc::new(AtomicUsize::new(0));
    let tool_calls = Arc::new(AtomicUsize::new(0));
    let model: Arc<dyn ChatModel> = Arc::new(PanicModel {
        calls: model_calls.clone(),
    });
    let mut tools = ToolRegistry::new();
    tools.register(PanicTool {
        calls: tool_calls.clone(),
        description: "Echoes its input and steers the run.",
    });
    let graph =
        create_react_agent_replaying(model, tools, replay.source(), journal.clone()).unwrap();
    // The replay inbox re-delivers the recorded mutations at the seqs the
    // journal attests; the panic sentinels prove nothing re-executed.
    let inbox = Inbox::replaying(&snapshot).unwrap();

    let outcome = Executor::with_checkpointer(Arc::new(InMemoryCheckpointer::new()))
        .run(
            &graph,
            &spec(),
            initial_state(),
            RunConfig::new("t-inbox-replay")
                .with_clock(logical_clock())
                .with_rng(RngSource::seeded(RNG_SEED))
                .with_journal(journal.clone())
                .with_inbox(inbox),
        )
        .await
        .unwrap();
    replay.verify(&journal.snapshot()).unwrap();

    assert_eq!(model_calls.load(Ordering::SeqCst), 0);
    assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
    // Byte-identical evidence, not just structural agreement.
    assert_eq!(
        serde_json::to_string(&snapshot).unwrap(),
        serde_json::to_string(&journal.snapshot()).unwrap()
    );
    match &outcome {
        ExecutionOutcome::Done(state) => assert_eq!(state, &recorded_state),
        other => panic!("expected Done, got {other:?}"),
    }
}

#[tokio::test]
async fn replay_reproduces_an_inbox_cancellation_exactly() {
    // Record: the first model call latches a typed cancellation; the run
    // stops at the next boundary and journals the cause.
    let inbox = Inbox::new();
    inbox.steer(json!("too late to steer")).unwrap();
    let canceller = inbox.clone();
    let model = Arc::new(
        ScriptedModel::new(vec![tool_call_script()])
            .with_effect(0, move || canceller.cancel(CancelCause::Hook, true)),
    );
    let journal = Journal::new(
        "run-inbox-cancel-replay",
        "t-inbox-cancel-replay",
        logical_clock(),
    );
    let graph = create_react_agent_with_recording(model, echo_registry(), journal.clone()).unwrap();
    let recorded_error = Executor::with_checkpointer(Arc::new(InMemoryCheckpointer::new()))
        .run(
            &graph,
            &spec(),
            initial_state(),
            RunConfig::new("t-inbox-cancel-replay")
                .with_clock(logical_clock())
                .with_rng(RngSource::seeded(RNG_SEED))
                .with_journal(journal.clone())
                .with_inbox(inbox),
        )
        .await
        .expect_err("the recorded run cancels");
    let snapshot = journal.snapshot();

    // Replay: the scheduled cancellation fires at the same journal position
    // and the evidence matches byte-for-byte.
    let replay = ExactReplay::new(snapshot.clone()).unwrap();
    let replay_journal = replay.fresh_journal(logical_clock());
    let model_calls = Arc::new(AtomicUsize::new(0));
    let model: Arc<dyn ChatModel> = Arc::new(PanicModel {
        calls: model_calls.clone(),
    });
    let graph = create_react_agent_replaying(
        model,
        echo_registry(),
        replay.source(),
        replay_journal.clone(),
    )
    .unwrap();
    let replay_inbox = Inbox::replaying(&snapshot).unwrap();
    let replayed_error = Executor::with_checkpointer(Arc::new(InMemoryCheckpointer::new()))
        .run(
            &graph,
            &spec(),
            initial_state(),
            RunConfig::new("t-inbox-cancel-replay")
                .with_clock(logical_clock())
                .with_rng(RngSource::seeded(RNG_SEED))
                .with_journal(replay_journal.clone())
                .with_inbox(replay_inbox),
        )
        .await
        .expect_err("the replayed run cancels at the same point");

    assert_eq!(model_calls.load(Ordering::SeqCst), 0);
    assert_eq!(recorded_error.to_string(), replayed_error.to_string());
    replay.verify(&replay_journal.snapshot()).unwrap();
    assert_eq!(
        serde_json::to_string(&snapshot).unwrap(),
        serde_json::to_string(&replay_journal.snapshot()).unwrap()
    );
}

// ---------- empty means absent ----------

#[tokio::test]
async fn an_untouched_inbox_is_byte_identical_to_no_inbox() {
    async fn drive(run_id: &str, thread: &str, inbox: Option<Inbox>) -> (String, String) {
        let model = Arc::new(ScriptedModel::new(vec![
            tool_call_script(),
            ChatMessage::assistant("the echo said: hello"),
        ]));
        let graph = create_react_agent(model, echo_registry()).unwrap();
        let journal = Journal::new(run_id, thread, logical_clock());
        let checkpointer = Arc::new(InMemoryCheckpointer::new());
        let mut config = RunConfig::new(thread)
            .with_clock(logical_clock())
            .with_rng(RngSource::seeded(RNG_SEED))
            .with_journal(journal.clone());
        if let Some(inbox) = inbox {
            config = config.with_inbox(inbox);
        }
        let outcome = Executor::with_checkpointer(checkpointer.clone())
            .run(&graph, &spec(), initial_state(), config)
            .await
            .unwrap();
        assert!(matches!(outcome, ExecutionOutcome::Done(_)));
        let checkpoint = checkpointer.get_latest(thread).await.unwrap().unwrap();
        (
            serde_json::to_string(&journal.snapshot()).unwrap(),
            serde_json::to_string(&checkpoint).unwrap(),
        )
    }

    let (without_journal, without_checkpoint) = drive("run-plain", "t-plain", None).await;
    let (with_journal, with_checkpoint) = drive("run-plain", "t-plain", Some(Inbox::new())).await;

    assert_eq!(
        without_journal, with_journal,
        "an attached but untouched inbox journals nothing"
    );
    assert_eq!(
        without_checkpoint, with_checkpoint,
        "an untouched inbox stamps nothing into checkpoints"
    );
}

// ---------- golden wire shapes ----------

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

#[test]
fn golden_inbox_event_kinds() {
    // The wave's appended variants, in declaration order — appended only,
    // never renamed or reordered, so pre-wave journals keep deserializing.
    assert_golden(
        "inbox_event_kinds.json",
        &vec![
            RunEventKind::InboxIntake,
            RunEventKind::InboxConsumed,
            RunEventKind::RunCancelled,
        ],
    );
}

#[test]
fn golden_inbox_message_shape() {
    assert_golden(
        "inbox_message.json",
        &InboxMessage {
            seq: 7,
            kind: InboxKind::Steering,
            sender: "parent:run-41".into(),
            content: json!("prefer metric units"),
        },
    );
}

#[test]
fn golden_inbox_consumption_shape() {
    assert_golden(
        "inbox_consumption.json",
        &InboxConsumption {
            point: ConsumptionPoint::TurnExtension,
            step: 4,
            messages: vec![
                InboxMessage {
                    seq: 3,
                    kind: InboxKind::FollowUp,
                    sender: "user".into(),
                    content: json!("and one more thing"),
                },
                InboxMessage {
                    seq: 1,
                    kind: InboxKind::Injected,
                    sender: "hook:locale".into(),
                    content: json!({"timezone": "Europe/Lisbon"}),
                },
            ],
        },
    );
}

#[test]
fn golden_run_cancellation_shape() {
    // The closed cause vocabulary, one entry per variant, and the drop
    // accounting of a clearing cancellation.
    assert_golden(
        "cancel_cause.json",
        &vec![
            CancelCause::User,
            CancelCause::Parent,
            CancelCause::Hook,
            CancelCause::Disposed,
        ],
    );
    assert_golden(
        "run_cancellation.json",
        &RunCancellation {
            cause: CancelCause::Hook,
            keep_inbox: false,
            dropped: DroppedMessages {
                steering: 2,
                followups: 1,
                staged: 1,
                pending: 0,
            },
        },
    );
}
