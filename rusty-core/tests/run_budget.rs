//! The spend guard: a run declaring a budget stops at the super-step
//! boundary where its journaled model calls cross it, with the words that
//! say which bound and how much; a run under budget is untouched; the
//! declaration carries the budget.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use rusty_agent_runtime::checkpoint::InMemoryCheckpointer;
use rusty_agent_runtime::error::{Result as RustyResult, RustyError};
use rusty_agent_runtime::executor::{ExecutionOutcome, Executor, RunConfig};
use rusty_agent_runtime::journal::{Clock, Journal, RngSource};
use rusty_agent_runtime::llm::{ChatMessage, ChatModel, ChatResponse, ToolCall, Usage};
use rusty_agent_runtime::meter::RunBudget;
use rusty_agent_runtime::react::{create_react_agent, MESSAGES_CHANNEL};
use rusty_agent_runtime::record::{Effect, PayloadRef, RunEventKind};
use rusty_agent_runtime::state::{Reducer, State, StateSpec};
use rusty_agent_runtime::tool::{Tool, ToolRegistry};

/// A scripted model whose every call reports 60 tokens of usage.
struct MeteredModel {
    script: Mutex<VecDeque<ChatMessage>>,
}

#[async_trait::async_trait]
impl ChatModel for MeteredModel {
    async fn chat(&self, _messages: &[ChatMessage], _tools: &[Value]) -> RustyResult<ChatResponse> {
        let message = self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| RustyError::Llm("script exhausted".into()))?;
        Ok(ChatResponse {
            message,
            model: Some("metered-1".into()),
            usage: Some(Usage {
                prompt_tokens: 50,
                completion_tokens: 10,
                total_tokens: 60,
                ..Usage::default()
            }),
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

fn spec() -> StateSpec {
    StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages)
}

fn initial_state() -> State {
    State::from_value(json!({
        MESSAGES_CHANNEL: [serde_json::to_value(ChatMessage::user("say hello twice")).unwrap()]
    }))
    .unwrap()
}

/// Two model calls of 60 tokens each: a tool call, then the answer.
fn model() -> Arc<dyn ChatModel> {
    Arc::new(MeteredModel {
        script: Mutex::new(
            vec![
                ChatMessage::assistant_tool_calls(vec![ToolCall::new("c1", "echo", json!({"text": "hello"}))]),
                ChatMessage::assistant("the echo said: hello"),
            ]
            .into(),
        ),
    })
}

async fn run_with(budget: Option<RunBudget>) -> (RustyResult<ExecutionOutcome>, Journal) {
    let journal = Journal::new("run-budget", "t-budget", Clock::logical(1_700_000_000_000, 10));
    let mut tools = ToolRegistry::new();
    tools.register(EchoTool);
    let graph = create_react_agent(model(), tools).unwrap();
    let executor = Executor::with_checkpointer(Arc::new(InMemoryCheckpointer::new()));
    let mut config = RunConfig::new("t-budget")
        .with_journal(journal.clone())
        .with_rng(RngSource::seeded(7));
    if let Some(budget) = budget {
        config = config.with_budget(budget);
    }
    (executor.run(&graph, &spec(), initial_state(), config).await, journal)
}

#[tokio::test]
async fn a_run_stops_at_the_boundary_where_its_tokens_cross_the_budget() {
    // 100 tokens: the first model call (60) fits, the second brings the run
    // to 120 — the step that made it is the one the run stops after.
    let (outcome, journal) = run_with(Some(RunBudget { max_tokens: Some(100), max_cost_usd: None })).await;
    let error = match outcome {
        Err(error) => error,
        Ok(other) => panic!("expected the budget to stop the run, got {other:?}"),
    };
    assert!(matches!(error, RustyError::Budget(_)), "{error}");
    let text = error.to_string();
    assert!(text.contains("120 tokens spent against a limit of 100"), "{text}");
    assert!(text.contains("the run stops here"), "{text}");

    // The declaration carries the budget; the model calls are journaled.
    let snapshot = journal.snapshot();
    let declared = snapshot
        .events
        .iter()
        .find(|e| e.kind == RunEventKind::RunConfigDeclared)
        .expect("a run with a budget declares it");
    let output = match declared.output.as_ref() {
        Some(PayloadRef::Inline(value)) => value.clone(),
        other => panic!("expected an inline declaration, got {other:?}"),
    };
    assert_eq!(output["budget"]["max_tokens"], 100);
    assert_eq!(
        snapshot.events.iter().filter(|e| e.kind == RunEventKind::ModelCall).count(),
        2
    );
}

#[tokio::test]
async fn a_run_under_budget_finishes_and_an_unpriced_model_never_trips_a_cost_bound() {
    let (outcome, _) = run_with(Some(RunBudget { max_tokens: Some(1000), max_cost_usd: Some(0.0001) })).await;
    match outcome {
        Ok(ExecutionOutcome::Done(state)) => {
            let messages: Vec<ChatMessage> = state.get_as(MESSAGES_CHANNEL).unwrap().unwrap();
            assert_eq!(messages.last().unwrap().content.as_deref(), Some("the echo said: hello"));
        }
        other => panic!("expected Done, got {other:?}"),
    }
    // No budget at all: byte-identical prior behavior, no declaration.
    let (outcome, journal) = run_with(None).await;
    assert!(matches!(outcome, Ok(ExecutionOutcome::Done(_))));
    assert!(!journal
        .snapshot()
        .events
        .iter()
        .any(|e| e.kind == RunEventKind::RunConfigDeclared));
}
