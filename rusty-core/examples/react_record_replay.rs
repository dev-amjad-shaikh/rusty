//! Flight Recorder demo for the prebuilt ReAct agent: record a run, then
//! replay it exactly — zero outbound calls, byte-identical evidence.
//!
//! Phase 1 records a `create_react_agent_with_recording` run (scripted mock
//! model + a real echo tool, no network) under the determinism seams
//! (logical clock, seeded RNG), so every model/tool call lands in the run's
//! journal in the canonical replay-compatible shapes.
//!
//! Phase 2 replays the recorded journal through
//! `create_react_agent_replaying` over **panic-on-call sentinels**: every
//! model/tool call is answered from the journal instead of executed, and
//! `ExactReplay::run_and_verify` proves the replayed journal reproduces the
//! recorded one event-for-event, head hash included.
//!
//! Run with: `cargo run --example react_record_replay`

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rusty_agent_runtime::prelude::*;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Determinism parameters: shared by the record and replay phases, which is
// what makes the replayed journal byte-identical to the recorded one.
// ---------------------------------------------------------------------------

const CLOCK_START_MS: u64 = 1_700_000_000_000;
const CLOCK_TICK_MS: u64 = 10;
const RNG_SEED: u64 = 7;

// ---------------------------------------------------------------------------
// Models and tools: scripted (record) / panic-on-call sentinels (replay).
// ---------------------------------------------------------------------------

struct ScriptedModel {
    responses: Mutex<VecDeque<ChatMessage>>,
}

#[async_trait]
impl ChatModel for ScriptedModel {
    async fn chat(&self, _messages: &[ChatMessage], _tools: &[Value]) -> Result<ChatResponse> {
        println!("    [mock-llm] chat() called (record mode: this runs for real)");
        let message = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| RustyError::Llm("script exhausted".into()))?;
        Ok(ChatResponse {
            message,
            model: Some("scripted-mock-1".into()),
            usage: None,
        })
    }
}

/// A model that panics if invoked: exact replay must never reach it.
struct PanicModel {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl ChatModel for PanicModel {
    async fn chat(&self, _messages: &[ChatMessage], _tools: &[Value]) -> Result<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        panic!("replay hit the network: the model was invoked")
    }
}

struct Echo;

#[async_trait]
impl Tool for Echo {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "Echoes back the given text."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {"text": {"type": "string"}}})
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, args: Value) -> Result<Value> {
        let text = args.get("text").and_then(Value::as_str).unwrap_or("");
        println!("    [tool:echo] -> \"{text}\" (record mode: this runs for real)");
        Ok(json!(text))
    }
}

/// A tool that panics if invoked. Identity matches `Echo` byte-for-byte:
/// tool schemas feed the model-call request hash, so the replay registry
/// must offer the same names, descriptions, and parameter schemas.
struct PanicTool {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for PanicTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "Echoes back the given text."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {"text": {"type": "string"}}})
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, _args: Value) -> Result<Value> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        panic!("replay hit the network: the tool was invoked")
    }
}

// ---------------------------------------------------------------------------
// The demo.
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    println!("=== Rusty Core: ReAct record -> exact replay ===\n");

    let spec = StateSpec::new().channel("messages", Reducer::AddMessages);
    let initial = State::from_value(json!({
        "messages": [serde_json::to_value(ChatMessage::user("echo 'hello' back to me"))?]
    }))?;

    // -- Phase 1: record ----------------------------------------------------
    println!("--- phase 1: recording the run ---");
    let journal = Journal::new(
        "run-react-demo",
        "react-record-replay",
        Clock::logical(CLOCK_START_MS, CLOCK_TICK_MS),
    );
    let model: Arc<dyn ChatModel> = Arc::new(ScriptedModel {
        responses: Mutex::new(
            vec![
                ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                    "call_1",
                    "echo",
                    json!({"text": "hello"}),
                )]),
                ChatMessage::assistant("The echo said: hello."),
            ]
            .into(),
        ),
    });
    let mut registry = ToolRegistry::new();
    registry.register(Echo);
    let graph = create_react_agent_with_recording(model, registry, journal.clone())?;

    let recorded = Executor::new()
        .run(
            &graph,
            &spec,
            initial.clone(),
            RunConfig::new("react-record-replay")
                .with_journal(journal.clone())
                .with_rng(RngSource::seeded(RNG_SEED)),
        )
        .await?;
    let recorded_state = recorded.state().clone();
    let snapshot = journal.snapshot();
    println!(
        "recorded {} event(s); head hash {}…",
        snapshot.events.len(),
        &snapshot.head_hash[..16]
    );
    for event in &snapshot.events {
        if matches!(event.kind, RunEventKind::ModelCall | RunEventKind::ToolCall) {
            println!(
                "  seq {:>2} {:?} node={:?} parent={:?} effect={:?}",
                event.seq, event.kind, event.node_id, event.parent, event.effect
            );
        }
    }

    // -- Phase 2: exact replay ----------------------------------------------
    println!("\n--- phase 2: exact replay (sentinels must never fire) ---");
    let replay = ExactReplay::new(snapshot.clone())?;
    let replay_journal = replay.fresh_journal(Clock::logical(CLOCK_START_MS, CLOCK_TICK_MS));
    let model_calls = Arc::new(AtomicUsize::new(0));
    let tool_calls = Arc::new(AtomicUsize::new(0));
    let sentinel_model: Arc<dyn ChatModel> = Arc::new(PanicModel {
        calls: model_calls.clone(),
    });
    let mut sentinel_registry = ToolRegistry::new();
    sentinel_registry.register(PanicTool {
        calls: tool_calls.clone(),
    });
    let replay_graph = create_react_agent_replaying(
        sentinel_model,
        sentinel_registry,
        replay.source(),
        replay_journal.clone(),
    )?;

    let replayed = replay
        .run_and_verify(
            &replay_graph,
            &spec,
            initial,
            ReplayParams::new(replay_journal, RngSource::seeded(RNG_SEED)),
        )
        .await?;

    println!(
        "sentinel invocations: model={}, tool={} (zero outbound calls)",
        model_calls.load(Ordering::SeqCst),
        tool_calls.load(Ordering::SeqCst)
    );
    println!(
        "replayed {} event(s); journals byte-identical: {}",
        replayed.journal.events.len(),
        serde_json::to_vec(&snapshot)? == serde_json::to_vec(&replayed.journal)?
    );
    let replayed_state = replayed.outcome.state().clone();
    let replayed_messages: Vec<ChatMessage> = replayed_state
        .get_as("messages")?
        .expect("messages channel");
    println!(
        "final states identical: {} ({} messages; last: {:?})",
        replayed_state == recorded_state,
        replayed_messages.len(),
        replayed_messages.last().and_then(|m| m.content.as_deref())
    );
    Ok(())
}
