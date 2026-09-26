//! Conversation-surface integration tests (R0.13 parity wave).
//!
//! The surface derives from real journaled ReAct runs — recorded with the
//! same deterministic seams as the Flight Recorder suites — and every
//! honesty rule is exercised against that derived surface: append/replace
//! semantics, citation validity, revision chaining and recoverability, and
//! the rejection of dishonest replacements.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use rusty_agent_runtime::checkpoint::InMemoryCheckpointer;
use rusty_agent_runtime::error::{Result as RustyResult, RustyError};
use rusty_agent_runtime::executor::{ExecutionOutcome, Executor, RunConfig};
use rusty_agent_runtime::journal::{Clock, Journal, JournalSnapshot, RngSource};
use rusty_agent_runtime::llm::{ChatMessage, ChatModel, ChatResponse, Role, ToolCall};
use rusty_agent_runtime::react::{create_react_agent_with_recording, MESSAGES_CHANNEL};
use rusty_agent_runtime::record::{Effect, RunEventKind};
use rusty_agent_runtime::state::{Reducer, State, StateSpec};
use rusty_agent_runtime::surface::{
    Provenance, Surface, SurfaceEntry, SurfaceEntryKind, SurfaceOp,
};
use rusty_agent_runtime::tool::{Tool, ToolRegistry};

const THREAD_ID: &str = "t-surface";

fn spec() -> StateSpec {
    StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages)
}

fn initial_state() -> State {
    State::from_value(json!({
        MESSAGES_CHANNEL: [serde_json::to_value(ChatMessage::user("say hello")).unwrap()]
    }))
    .unwrap()
}

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

fn tools() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(EchoTool);
    registry
}

/// Record one ReAct run (tool call, then final answer) under the full
/// determinism seam set; return the journal snapshot and the final state.
async fn record_run() -> (JournalSnapshot, State) {
    let checkpointer = Arc::new(InMemoryCheckpointer::new());
    let executor = Executor::with_checkpointer(checkpointer);
    let journal = Journal::new(
        "run-surface-recording",
        THREAD_ID,
        Clock::logical(1_700_000_000_000, 10),
    );
    let model: Arc<dyn ChatModel> = Arc::new(ScriptedModel::react_script());
    let graph = create_react_agent_with_recording(model, tools(), journal.clone()).unwrap();

    let outcome = executor
        .run(
            &graph,
            &spec(),
            initial_state(),
            RunConfig::new(THREAD_ID)
                .with_journal(journal.clone())
                .with_rng(RngSource::seeded(7)),
        )
        .await
        .unwrap();
    match outcome {
        ExecutionOutcome::Done(state) => (journal.snapshot(), state),
        other => panic!("expected Done, got {other:?}"),
    }
}

/// The seqs of a snapshot's events of one kind, in order.
fn seqs_of_kind(snapshot: &JournalSnapshot, kind: RunEventKind) -> Vec<u64> {
    snapshot
        .events
        .iter()
        .filter(|event| event.kind == kind)
        .map(|event| event.seq)
        .collect()
}

#[tokio::test]
async fn derivation_reconstructs_the_journaled_conversation() {
    let (snapshot, state) = record_run().await;
    let surface = Surface::derive(&snapshot).unwrap();

    // The full transcript: user, assistant tool-call request, tool result,
    // assistant final answer — all journal-provenanced.
    let base = surface.base();
    assert_eq!(base.len(), 4);
    let kinds: Vec<SurfaceEntryKind> = base.iter().map(|entry| entry.kind).collect();
    assert_eq!(
        kinds,
        [
            SurfaceEntryKind::User,
            SurfaceEntryKind::Assistant,
            SurfaceEntryKind::ToolResult,
            SurfaceEntryKind::Assistant,
        ]
    );
    assert!(base
        .iter()
        .all(|entry| entry.provenance == Provenance::Journal));
    assert_eq!(
        base[3].message.content.as_deref(),
        Some("the echo said: hello")
    );

    // The derived surface is the run's final messages channel, verbatim.
    let journaled: Vec<ChatMessage> = state.get_as(MESSAGES_CHANNEL).unwrap().unwrap();
    let derived: Vec<ChatMessage> = base.iter().map(|entry| entry.message.clone()).collect();
    assert_eq!(derived, journaled);

    // Citations are real and in range, and the causal attribution lands:
    // assistant turns cite a model call, the tool result cites the tool call.
    let journal_events = surface.journal_events();
    for entry in base {
        assert!(!entry.source_seqs.is_empty());
        assert!(entry.source_seqs.iter().all(|seq| *seq < journal_events));
    }
    let model_calls = seqs_of_kind(&snapshot, RunEventKind::ModelCall);
    let tool_calls = seqs_of_kind(&snapshot, RunEventKind::ToolCall);
    assert_eq!(model_calls.len(), 2);
    assert_eq!(tool_calls.len(), 1);
    assert!(base[1].source_seqs.contains(&model_calls[0]));
    assert!(base[2].source_seqs.contains(&tool_calls[0]));
    assert!(base[3].source_seqs.contains(&model_calls[1]));
}

#[tokio::test]
async fn append_and_replace_mutate_the_surface_never_the_journal() {
    let (snapshot, _) = record_run().await;
    let journal_bytes = serde_json::to_string(&snapshot).unwrap();
    let mut surface = Surface::derive(&snapshot).unwrap();

    // A live turn appends.
    let live = surface
        .apply(SurfaceOp::Append {
            entry: SurfaceEntry::live(ChatMessage::user("one more thing")),
        })
        .unwrap();
    assert_eq!(live, 0);
    assert_eq!(surface.entries().len(), 5);

    // The first three turns compact into one summary.
    let summary_revision = surface
        .compact(0, 3, "the user asked for an echo of hello")
        .unwrap();
    assert_eq!(summary_revision, 1);
    assert_eq!(surface.revisions()[1].parent, Some(0));

    let entries = surface.entries();
    assert_eq!(entries.len(), 3);
    let summary = &entries[0];
    assert_eq!(summary.kind, SurfaceEntryKind::Summary);
    assert_eq!(summary.provenance, Provenance::Compaction);
    assert_eq!(summary.message.role, Role::System);

    // The summary cites exactly the union of what it subsumes.
    let mut expected: Vec<u64> = surface.base()[..3]
        .iter()
        .flat_map(|entry| entry.source_seqs.iter().copied())
        .collect();
    expected.sort_unstable();
    expected.dedup();
    assert_eq!(summary.source_seqs, expected);

    // The pre-compaction surface and the mid-history view are recoverable,
    // and the journal itself is untouched.
    assert_eq!(surface.view_at(0).unwrap(), surface.base().to_vec());
    assert_eq!(surface.view_at(1).unwrap().len(), 5);
    assert_eq!(serde_json::to_string(&snapshot).unwrap(), journal_bytes);
}

#[tokio::test]
async fn compacted_surface_projects_the_model_message_list() {
    let (snapshot, _) = record_run().await;
    let mut surface = Surface::derive(&snapshot).unwrap();
    surface.compact(0, 3, "greeting and echo of hello").unwrap();

    // What a model node would consume next: the summary as a system message,
    // then the uncompacted tail verbatim.
    let messages = surface.messages();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role, Role::System);
    assert_eq!(
        messages[0].content.as_deref(),
        Some("greeting and echo of hello")
    );
    assert_eq!(messages[1], surface.base()[3].message);
}

#[tokio::test]
async fn dishonest_replacements_against_a_real_run_are_rejected() {
    let (snapshot, _) = record_run().await;
    let mut surface = Surface::derive(&snapshot).unwrap();

    // Gap leak: the summary drops one of the subsumed entries' seqs.
    let mut leaked: Vec<u64> = surface.base()[..3]
        .iter()
        .flat_map(|entry| entry.source_seqs.iter().copied())
        .collect();
    leaked.sort_unstable();
    leaked.dedup();
    leaked.pop();
    let err = surface
        .apply(SurfaceOp::Replace {
            start: 0,
            end: 3,
            entry: SurfaceEntry::summary("x", leaked),
        })
        .unwrap_err()
        .to_string();
    assert!(err.contains("cite exactly"), "got: {err}");

    // Overlap with reality: citing a seq the span does not subsume is
    // fabrication, even when the seq is real and in range.
    let mut fabricated = surface.base()[0].source_seqs.clone();
    fabricated.push(surface.base()[3].source_seqs[0]);
    let err = surface
        .apply(SurfaceOp::Replace {
            start: 0,
            end: 1,
            entry: SurfaceEntry::summary("x", fabricated),
        })
        .unwrap_err()
        .to_string();
    assert!(err.contains("cite exactly"), "got: {err}");

    // The rejections recorded nothing.
    assert!(surface.revisions().is_empty());
    assert_eq!(surface.entries(), surface.base().to_vec());
}
