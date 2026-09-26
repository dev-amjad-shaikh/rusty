//! The context pipeline inside the ReAct node, driven the way the server
//! drives it: a run config carrying a context policy, a journal on the run,
//! the plain `create_react_agent`. Proves the three things that matter:
//!
//! 1. the thread's leading system message — the charter — is the pinned
//!    identity on every call and never reaches the summarizer;
//! 2. the journaled model call *is* the assembled request (manifest message
//!    and all), and the compaction summary is journaled under the pipeline's
//!    own parent;
//! 3. an exact replay of the recording reproduces it byte for byte without
//!    touching a model.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use rusty_agent_runtime::checkpoint::InMemoryCheckpointer;
use rusty_agent_runtime::context::{
    CompactionPolicy, ContextPolicy, SectionPolicy, ToolsSectionPolicy, CONTEXT_PIPELINE_PARENT,
    CONTEXT_POLICY_SCHEMA_VERSION, MANIFEST_MESSAGE_NAME, SUMMARY_MARKER,
};
use rusty_agent_runtime::error::{Result as RustyResult, RustyError};
use rusty_agent_runtime::executor::{ExecutionOutcome, Executor, RunConfig};
use rusty_agent_runtime::journal::{Clock, Journal, JournalSnapshot, RngSource};
use rusty_agent_runtime::llm::{ChatMessage, ChatModel, ChatResponse, Role, ToolCall};
use rusty_agent_runtime::memory::ContextBudget;
use rusty_agent_runtime::react::{create_react_agent, create_react_agent_replaying, AGENT_NODE, MESSAGES_CHANNEL};
use rusty_agent_runtime::record::{Effect, PayloadRef, RunEvent, RunEventKind};
use rusty_agent_runtime::replay::{ExactReplay, ReplayParams};
use rusty_agent_runtime::state::{Reducer, State, StateSpec};
use rusty_agent_runtime::tool::{Tool, ToolRegistry};
use rusty_agent_runtime::tool_select::ToolSelectionOverlay;

const CLOCK_START_MS: u64 = 1_700_000_000_000;
const CLOCK_TICK_MS: u64 = 10;
const RNG_SEED: u64 = 7;
const RUN_ID: &str = "run-react-context";
const THREAD_ID: &str = "t-react-context";
const CHARTER: &str = "CHARTER: you are Echo. Call echo, then answer with what it said.";

fn logical_clock() -> Clock {
    Clock::logical(CLOCK_START_MS, CLOCK_TICK_MS)
}

fn spec() -> StateSpec {
    StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages)
}

/// The thread as the server starts it: the charter journaled as message
/// zero, then the person's turn.
fn initial_state() -> State {
    State::from_value(json!({
        MESSAGES_CHANNEL: [
            serde_json::to_value(ChatMessage::system(CHARTER)).unwrap(),
            serde_json::to_value(ChatMessage::user("say hello")).unwrap(),
        ]
    }))
    .unwrap()
}

/// A policy whose compaction fires on the second call: the trigger is a
/// handful of tokens, two messages stay verbatim.
fn policy() -> ContextPolicy {
    ContextPolicy {
        schema_version: CONTEXT_POLICY_SCHEMA_VERSION.to_owned(),
        budget: ContextBudget::new(4096),
        tokenizer: Default::default(),
        identity: Some(SectionPolicy::new(256)),
        task: None,
        skills: None,
        tools: Some(ToolsSectionPolicy::new(512)),
        memory: None,
        recall: None,
        history: Some(SectionPolicy::new(1024)),
        compaction: Some(CompactionPolicy {
            trigger_tokens: 10,
            keep_recent_messages: 2,
            keep_recent_steps: 0,
            summary_max_tokens: 128,
            prompt: "Summarize the earlier conversation.".to_owned(),
        }),
    }
}

/// A scripted model that keeps every request it was handed.
struct ScriptedModel {
    script: Mutex<VecDeque<ChatMessage>>,
    /// Every `(messages, tools)` pair the model was asked with, in order.
    seen: Arc<Mutex<Vec<SeenCall>>>,
}

type SeenCall = (Vec<ChatMessage>, Vec<Value>);

#[async_trait::async_trait]
impl ChatModel for ScriptedModel {
    async fn chat(&self, messages: &[ChatMessage], tools: &[Value]) -> RustyResult<ChatResponse> {
        self.seen.lock().unwrap().push((messages.to_vec(), tools.to_vec()));
        let message = self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| RustyError::Llm("script exhausted".into()))?;
        Ok(ChatResponse {
            message,
            model: Some("scripted-context-1".into()),
            usage: None,
        })
    }
}

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

struct EchoTool {
    panic: Option<Arc<AtomicUsize>>,
}

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
        if let Some(calls) = &self.panic {
            calls.fetch_add(1, Ordering::SeqCst);
            panic!("exact replay hit a tool");
        }
        Ok(json!(args.get("text").cloned().unwrap_or(Value::Null)))
    }
}

fn tools(panic: Option<Arc<AtomicUsize>>) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(EchoTool { panic });
    registry
}

type Seen = Arc<Mutex<Vec<(Vec<ChatMessage>, Vec<Value>)>>>;

/// Record a run the server's way: plain react agent, journal on the config,
/// the policy on the config. Call order on the scripted model: the first
/// agent call, then the compaction summarizer, then the second agent call.
async fn record_run() -> (JournalSnapshot, State, Seen) {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let model: Arc<dyn ChatModel> = Arc::new(ScriptedModel {
        script: Mutex::new(
            vec![
                ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                    "c1",
                    "echo",
                    json!({"text": "hello"}),
                )]),
                ChatMessage::assistant("Earlier: the user asked to say hello; echo was called."),
                ChatMessage::assistant("the echo said: hello"),
            ]
            .into(),
        ),
        seen: Arc::clone(&seen),
    });
    let journal = Journal::new(RUN_ID, THREAD_ID, logical_clock());
    let graph = create_react_agent(model, tools(None)).unwrap();
    let executor = Executor::with_checkpointer(Arc::new(InMemoryCheckpointer::new()));
    let outcome = executor
        .run(
            &graph,
            &spec(),
            initial_state(),
            RunConfig::new(THREAD_ID)
                .with_journal(journal.clone())
                .with_rng(RngSource::seeded(RNG_SEED))
                .with_context_policy(&policy()),
        )
        .await
        .unwrap();
    match outcome {
        ExecutionOutcome::Done(state) => (journal.snapshot(), state, seen),
        other => panic!("expected Done, got {other:?}"),
    }
}

fn inline_input(event: &RunEvent) -> Value {
    match event.input.as_ref() {
        Some(PayloadRef::Inline(value)) => value.clone(),
        other => panic!("expected an inline request payload, got {other:?}"),
    }
}

fn system_texts(messages: &[ChatMessage]) -> Vec<String> {
    messages
        .iter()
        .filter(|m| m.role == Role::System)
        .map(|m| m.content.clone().unwrap_or_default())
        .collect()
}

#[tokio::test]
async fn the_charter_is_pinned_the_history_compacts_and_the_journal_holds_the_assembly() {
    let (snapshot, state, seen) = record_run().await;
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 3, "agent, summarizer, agent");

    // Call 1: the charter leads as the identity, once; the manifest rides
    // along; the echo schema is the tools argument.
    let (first, first_tools) = &seen[0];
    assert_eq!(first[0].role, Role::System);
    assert_eq!(first[0].content.as_deref(), Some(CHARTER));
    assert_eq!(
        system_texts(first).iter().filter(|t| t.as_str() == CHARTER).count(),
        1,
        "the charter must not appear in both the identity and the history"
    );
    assert!(
        first.iter().any(|m| m.name.as_deref() == Some(MANIFEST_MESSAGE_NAME)),
        "the assembled request carries the section manifest"
    );
    assert!(first.iter().any(|m| m.content.as_deref() == Some("say hello")));
    assert_eq!(first_tools.len(), 1);

    // Call 2 is the summarizer: it sees the history after the charter and
    // never the charter itself.
    let (summarize, _) = &seen[1];
    assert_eq!(summarize[0].content.as_deref(), Some("Summarize the earlier conversation."));
    let rendered = summarize[1].content.clone().unwrap_or_default();
    assert!(rendered.contains("say hello"), "{rendered}");
    assert!(!rendered.contains("CHARTER"), "the charter reached the summarizer: {rendered}");

    // Call 3: the charter still leads; the older history is the marked
    // summary; the two most recent messages are verbatim.
    let (third, _) = &seen[2];
    assert_eq!(third[0].content.as_deref(), Some(CHARTER));
    let summary = third
        .iter()
        .find(|m| m.content.as_deref().is_some_and(|c| c.starts_with(SUMMARY_MARKER)))
        .expect("a generated summary replaced the oldest history");
    assert!(summary.content.as_deref().unwrap().contains("echo was called"));
    assert!(third.iter().any(|m| !m.tool_calls.is_empty()), "the tool call stayed verbatim");
    assert!(third.iter().any(|m| m.role == Role::Tool), "the tool result stayed verbatim");

    // The journal: three model calls — two under the agent's node inputs,
    // one under the pipeline's parent — and the agent's journaled request is
    // the assembled one.
    let model_calls: Vec<&RunEvent> = snapshot
        .events
        .iter()
        .filter(|e| e.kind == RunEventKind::ModelCall)
        .collect();
    assert_eq!(model_calls.len(), 3, "{:?}", model_calls.iter().map(|e| &e.parent).collect::<Vec<_>>());
    let agent_calls: Vec<&&RunEvent> = model_calls
        .iter()
        .filter(|e| e.node_id.as_deref() == Some(AGENT_NODE))
        .collect();
    assert_eq!(agent_calls.len(), 2);
    let journaled = inline_input(agent_calls[0]);
    let names: Vec<&str> = journaled["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m.get("name").and_then(Value::as_str))
        .collect();
    assert!(names.contains(&MANIFEST_MESSAGE_NAME), "journaled request is the assembled one: {names:?}");
    let compaction = model_calls
        .iter()
        .find(|e| e.parent.as_deref() == Some(CONTEXT_PIPELINE_PARENT))
        .expect("the compaction call is journaled under the pipeline's parent");
    assert!(inline_input(compaction)["messages"][1]["content"]
        .as_str()
        .unwrap()
        .contains("say hello"));

    // The thread itself is untouched: verbatim history, charter first.
    let messages: Vec<ChatMessage> = state.get_as(MESSAGES_CHANNEL).unwrap().unwrap();
    assert_eq!(messages[0].content.as_deref(), Some(CHARTER));
    assert_eq!(messages.len(), 5);
    assert_eq!(messages[4].content.as_deref(), Some("the echo said: hello"));
    assert!(!messages.iter().any(|m| m.content.as_deref().is_some_and(|c| c.starts_with(SUMMARY_MARKER))));
}

#[tokio::test]
async fn an_assembled_recording_replays_exactly_without_a_model() {
    let (snapshot, recorded_state, _) = record_run().await;

    let replay = ExactReplay::new(snapshot.clone()).unwrap();
    let journal = replay.fresh_journal(logical_clock());
    let model_calls = Arc::new(AtomicUsize::new(0));
    let tool_calls = Arc::new(AtomicUsize::new(0));
    let model: Arc<dyn ChatModel> = Arc::new(PanicModel {
        calls: Arc::clone(&model_calls),
    });
    let graph = create_react_agent_replaying(
        model,
        tools(Some(Arc::clone(&tool_calls))),
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

    assert_eq!(model_calls.load(Ordering::SeqCst), 0);
    assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        serde_json::to_string(&snapshot).unwrap(),
        serde_json::to_string(&replayed.journal).unwrap()
    );
    match &replayed.outcome {
        ExecutionOutcome::Done(state) => assert_eq!(state, &recorded_state),
        other => panic!("expected Done, got {other:?}"),
    }
}

/// The builder's overlay for the echo tool: a when-to-use note.
fn overlays() -> std::collections::BTreeMap<String, ToolSelectionOverlay> {
    let mut overlays = std::collections::BTreeMap::new();
    overlays.insert(
        "echo".to_owned(),
        ToolSelectionOverlay {
            when_to_use: Some("when the person wants their own words back".to_owned()),
            ..Default::default()
        },
    );
    overlays
}

/// Record a run on the governed tools path: the same policy, plus the
/// builder's overlays on the config. One call, no tool use, no compaction.
async fn record_governed_run() -> (JournalSnapshot, State, Seen) {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let model: Arc<dyn ChatModel> = Arc::new(ScriptedModel {
        script: Mutex::new(vec![ChatMessage::assistant("hello")].into()),
        seen: Arc::clone(&seen),
    });
    let journal = Journal::new(RUN_ID, THREAD_ID, logical_clock());
    let graph = create_react_agent(model, tools(None)).unwrap();
    let executor = Executor::with_checkpointer(Arc::new(InMemoryCheckpointer::new()));
    let outcome = executor
        .run(
            &graph,
            &spec(),
            initial_state(),
            RunConfig::new(THREAD_ID)
                .with_journal(journal.clone())
                .with_rng(RngSource::seeded(RNG_SEED))
                .with_context_policy(&policy())
                .with_tool_overlays(&overlays()),
        )
        .await
        .unwrap();
    match outcome {
        ExecutionOutcome::Done(state) => (journal.snapshot(), state, seen),
        other => panic!("expected Done, got {other:?}"),
    }
}

#[tokio::test]
async fn the_builders_when_to_use_note_reaches_the_schema_and_the_ranking_is_journaled() {
    let (snapshot, _, seen) = record_governed_run().await;
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    let (_, tools_seen) = &seen[0];
    assert_eq!(tools_seen.len(), 1);
    let description = tools_seen[0]["function"]["description"].as_str().unwrap();
    assert_eq!(
        description,
        "Echoes its input text. When to use: when the person wants their own words back"
    );

    // The journaled request is the assembled one, and its manifest message
    // carries the tools section's selection outcome: echo ranked, selected.
    let agent_call = snapshot
        .events
        .iter()
        .find(|e| e.kind == RunEventKind::ModelCall && e.node_id.as_deref() == Some(AGENT_NODE))
        .expect("the agent's model call is journaled");
    let journaled = inline_input(agent_call);
    let manifest_text = journaled["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m.get("name").and_then(Value::as_str) == Some(MANIFEST_MESSAGE_NAME))
        .and_then(|m| m.get("content").and_then(Value::as_str))
        .expect("the manifest message rides in the journaled request")
        .to_owned();
    assert!(manifest_text.contains("shortlist"), "{manifest_text}");
    assert!(manifest_text.contains("\"selected\""), "{manifest_text}");
    assert!(manifest_text.contains("\"name\":\"echo\""), "{manifest_text}");

    // The declaration carries the overlays, so a replay re-declares them.
    let declared = snapshot
        .events
        .iter()
        .find(|e| e.kind == RunEventKind::RunConfigDeclared)
        .expect("a run with overlays declares its config");
    let output = match declared.output.as_ref() {
        Some(PayloadRef::Inline(value)) => value.clone(),
        other => panic!("expected an inline declaration, got {other:?}"),
    };
    assert_eq!(
        output["tool_overlays"]["echo"]["when_to_use"].as_str(),
        Some("when the person wants their own words back")
    );
}

#[tokio::test]
async fn a_governed_recording_replays_exactly_without_a_model() {
    let (snapshot, recorded_state, _) = record_governed_run().await;
    let replay = ExactReplay::new(snapshot.clone()).unwrap();
    let journal = replay.fresh_journal(logical_clock());
    let model_calls = Arc::new(AtomicUsize::new(0));
    let model: Arc<dyn ChatModel> = Arc::new(PanicModel {
        calls: Arc::clone(&model_calls),
    });
    let graph = create_react_agent_replaying(model, tools(None), replay.source(), journal.clone()).unwrap();
    let params = ReplayParams::new(journal, RngSource::seeded(RNG_SEED))
        .with_checkpointer(Arc::new(InMemoryCheckpointer::new()));
    let replayed = replay
        .run_and_verify(&graph, &spec(), initial_state(), params)
        .await
        .unwrap();
    assert_eq!(model_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        serde_json::to_string(&snapshot).unwrap(),
        serde_json::to_string(&replayed.journal).unwrap()
    );
    match &replayed.outcome {
        ExecutionOutcome::Done(state) => assert_eq!(state, &recorded_state),
        other => panic!("expected Done, got {other:?}"),
    }
}
