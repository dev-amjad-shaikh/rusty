//! End-to-end demo of the prebuilt ReAct agent (`create_react_agent`).
//!
//! Uses a **scripted** [`ChatModel`] (no network) and two toy tools
//! (`calculator`, `echo`) to walk the full reasoning–acting loop:
//!
//! ```text
//! user ──► agent (LLM: "I need tools") ──► tools (calculator + echo, in parallel)
//!              ▲                                │
//!              └────────── agent (LLM: final answer, sees tool results) ◄──┘
//! ```
//!
//! Run with: `cargo run --example react_agent`

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rusty_agent_runtime::prelude::*;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Mock model: pops one scripted response per call, prints what it observes.
// ---------------------------------------------------------------------------

struct ScriptedModel {
    responses: Mutex<VecDeque<ChatMessage>>,
}

impl ScriptedModel {
    fn new(responses: Vec<ChatMessage>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
        }
    }
}

#[async_trait]
impl ChatModel for ScriptedModel {
    async fn chat(&self, messages: &[ChatMessage], tools: &[Value]) -> Result<ChatResponse> {
        println!(
            "    [mock-llm] chat() called: {} message(s) in context, {} tool schema(s) offered",
            messages.len(),
            tools.len()
        );
        for (i, m) in messages.iter().enumerate() {
            let kind = if m.has_tool_calls() {
                format!("tool_calls x{}", m.tool_calls.len())
            } else {
                m.content.clone().unwrap_or_else(|| "<empty>".into())
            };
            println!("      ctx[{i}] {:?}: {}", m.role, truncate(&kind, 72));
        }
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

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

// ---------------------------------------------------------------------------
// Toy tools.
// ---------------------------------------------------------------------------

struct Calculator;

#[async_trait]
impl Tool for Calculator {
    fn name(&self) -> &str {
        "calculator"
    }
    fn description(&self) -> &str {
        "Basic arithmetic on two numbers."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "op": {"type": "string", "enum": ["add", "subtract", "multiply", "divide"]},
                "a": {"type": "number"},
                "b": {"type": "number"}
            },
            "required": ["op", "a", "b"]
        })
    }
    async fn call(&self, args: Value) -> Result<Value> {
        // Missing or wrongly-typed arguments are tool *errors*, not zeros:
        // defaulting to `0 op 0` would report a confident wrong answer to the
        // model as a successful call, and `ToolExecutor` already turns tool
        // errors into `ERROR:` messages the model can react to.
        let op = args.get("op").and_then(Value::as_str).ok_or_else(|| {
            RustyError::Tool(format!("missing or non-string `op`; raw args: {args}"))
        })?;
        let a = args.get("a").and_then(Value::as_f64).ok_or_else(|| {
            RustyError::Tool(format!("missing or non-numeric `a`; raw args: {args}"))
        })?;
        let b = args.get("b").and_then(Value::as_f64).ok_or_else(|| {
            RustyError::Tool(format!("missing or non-numeric `b`; raw args: {args}"))
        })?;
        let result = match op {
            "add" => a + b,
            "subtract" => a - b,
            "multiply" => a * b,
            "divide" => {
                if b == 0.0 {
                    return Err(RustyError::Tool("division by zero".into()));
                }
                a / b
            }
            other => return Err(RustyError::Tool(format!("unknown op `{other}`"))),
        };
        println!("    [tool:calculator] {a} {op} {b} = {result}");
        Ok(json!(result))
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
        json!({
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"]
        })
    }
    async fn call(&self, args: Value) -> Result<Value> {
        let text = args
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        println!("    [tool:echo] -> \"{text}\"");
        Ok(json!(text))
    }
}

// ---------------------------------------------------------------------------
// The demo.
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    println!("=== Rusty Core: prebuilt ReAct agent demo ===\n");

    // 1. Tools.
    let mut registry = ToolRegistry::new();
    registry.register(Calculator);
    registry.register(Echo);

    // 2. Scripted model: first pass requests both tools, second pass answers.
    //    (A real model would decide this from the conversation; the script
    //    just demonstrates the graph wiring without any network access.)
    let model: Arc<dyn ChatModel> = Arc::new(ScriptedModel::new(vec![
        ChatMessage::assistant_tool_calls(vec![
            ToolCall::new(
                "call_1",
                "calculator",
                json!({"op": "add", "a": 17, "b": 25}),
            ),
            ToolCall::new("call_2", "echo", json!({"text": "Bonjour, ReAct!"})),
        ]),
        ChatMessage::assistant(
            "17 + 25 = 42, and echo replied: \"Bonjour, ReAct!\". Both tools worked.",
        ),
    ]));

    // 3. The prebuilt graph: agent ⇄ tools over the `messages` channel.
    let graph = create_react_agent(model, registry)?;
    println!(
        "graph compiled: {} nodes, entry point `{}`\n",
        graph.node_count(),
        graph.entry_point()
    );

    // 4. State: one channel, `add_messages` semantics; seed the user question.
    let spec = StateSpec::new().channel("messages", Reducer::AddMessages);
    let question = "What is 17 + 25? Also, echo 'Bonjour, ReAct!' back to me.";
    let mut initial = State::new();
    initial.insert(
        "messages",
        json!([serde_json::to_value(ChatMessage::user(question))?]),
    );
    println!("user: {question}\n");

    // 5. Stream executor events as the loop trace.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<GraphEvent>(64);
    let tracer = tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            match event {
                GraphEvent::SuperStep { step, active_nodes } => {
                    println!("[step {step}] active: {}", active_nodes.join(", "));
                }
                GraphEvent::NodeStart { node, step } => println!("  ├─ {node} start (step {step})"),
                GraphEvent::NodeEnd { node, step } => println!("  ├─ {node} end   (step {step})"),
                GraphEvent::Token { node, delta } => {
                    // Never fires in this example: `create_react_agent` is
                    // non-streaming. See `create_react_agent_streaming`
                    // (used by live_agent) for Token events.
                    print!("  ├─ {node} token: {delta}");
                }
                GraphEvent::StateUpdate { step, updates } => {
                    println!(
                        "  ├─ barrier merge (step {step}): channels [{}]",
                        updates.keys().cloned().collect::<Vec<_>>().join(", ")
                    );
                }
                GraphEvent::CheckpointSaved {
                    checkpoint_id,
                    step,
                } => {
                    println!("  └─ checkpoint {checkpoint_id} (step {step})");
                }
            }
        }
    });

    // 6. Run.
    let config = RunConfig::new("react-demo")
        .with_max_steps(10)
        .with_event_tx(tx);
    let outcome = Executor::new().run(&graph, &spec, initial, config).await?;
    drop(tracer); // the trace task ends once the sender is dropped

    // 7. Print the final transcript.
    match &outcome {
        ExecutionOutcome::Done(state) => {
            println!("\n=== run finished: final transcript ===");
            let messages: Vec<ChatMessage> =
                state.get_as("messages")?.expect("messages channel present");
            for m in &messages {
                match m.role {
                    Role::User => println!("user      : {}", m.content.as_deref().unwrap_or("")),
                    Role::Assistant if m.has_tool_calls() => {
                        for call in &m.tool_calls {
                            println!(
                                "assistant : → tool_call {}({}) [{}]",
                                call.name, call.arguments, call.id
                            );
                        }
                    }
                    Role::Assistant => {
                        println!("assistant : {}", m.content.as_deref().unwrap_or(""))
                    }
                    Role::Tool => println!(
                        "tool      : [{}] {}",
                        m.tool_call_id.as_deref().unwrap_or("?"),
                        m.content.as_deref().unwrap_or("")
                    ),
                    Role::System => println!("system    : {}", m.content.as_deref().unwrap_or("")),
                }
            }
        }
        ExecutionOutcome::Interrupted { value, .. } => {
            println!("run interrupted with payload: {value}");
        }
    }

    Ok(())
}
