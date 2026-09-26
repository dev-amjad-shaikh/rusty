//! End-to-end demo of `rusty-otel`: the executor's span taxonomy
//! exported through the OpenTelemetry layer.
//!
//! Runs two workloads against a single global subscriber:
//!
//! 1. a minimal 2-node pipeline graph (`fetch` → `summarize`), and
//! 2. the prebuilt ReAct agent with a scripted mock [`ChatModel`]
//!    (no network), walking the full reason → act → reason loop.
//!
//! What you see depends on whether an OTLP endpoint is configured:
//!
//! ```sh
//! # Local only — pretty span logs on stderr:
//! cargo run --example otel_demo
//!
//! # With the collector from docker-compose.yml running — logs + spans in Jaeger:
//! OTEL_DEMO_ENDPOINT=http://localhost:4318/v1/traces cargo run --example otel_demo
//! ```
//!
//! Then open http://localhost:16686 and pick the `rusty-otel-demo`
//! service. Every `rusty.run` trace fans out into `rusty.super_step`
//! → `rusty.node` spans with `thread_id`, `step`, and `node` attributes.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rusty_agent_runtime::prelude::*;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Mock model: pops one scripted response per call (no network).
// ---------------------------------------------------------------------------

struct ScriptedModel {
    responses: Mutex<VecDeque<ChatMessage>>,
}

#[async_trait]
impl ChatModel for ScriptedModel {
    async fn chat(&self, _messages: &[ChatMessage], _tools: &[Value]) -> Result<ChatResponse> {
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

// ---------------------------------------------------------------------------
// One toy tool for the ReAct loop.
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
                "op": {"type": "string", "enum": ["add", "multiply"]},
                "a": {"type": "number"},
                "b": {"type": "number"}
            },
            "required": ["op", "a", "b"]
        })
    }
    async fn call(&self, args: Value) -> Result<Value> {
        let op = args.get("op").and_then(Value::as_str).unwrap_or("add");
        let a = args.get("a").and_then(Value::as_f64).unwrap_or(0.0);
        let b = args.get("b").and_then(Value::as_f64).unwrap_or(0.0);
        let result = match op {
            "add" => a + b,
            "multiply" => a * b,
            other => return Err(RustyError::Tool(format!("unknown op `{other}`"))),
        };
        Ok(json!(result))
    }
}

// ---------------------------------------------------------------------------
// Demo 1: a minimal 2-node pipeline.
// ---------------------------------------------------------------------------

async fn run_pipeline() -> Result<()> {
    let spec = StateSpec::new()
        .channel("raw", Reducer::Overwrite)
        .channel("summary", Reducer::Overwrite);

    let mut builder = GraphBuilder::new();
    builder.add_node("fetch", |_ctx: NodeContext| async move {
        Ok(NodeOutput::update("raw", json!("17 * 3")))
    });
    builder.add_node("summarize", |ctx: NodeContext| async move {
        let raw: String = ctx
            .state()
            .get_as("raw")?
            .unwrap_or_else(|| "<empty>".into());
        Ok(NodeOutput::update(
            "summary",
            json!(format!("computed: {raw}")),
        ))
    });
    builder.set_entry_point("fetch");
    builder.add_edge("fetch", "summarize");
    let graph = builder.compile()?;

    let outcome = Executor::new()
        .run(&graph, &spec, State::new(), RunConfig::new("pipeline-demo"))
        .await?;

    match outcome {
        ExecutionOutcome::Done(state) => {
            let summary: String = state.get_as("summary")?.unwrap_or_default();
            println!("[pipeline] final summary channel: {summary}");
        }
        ExecutionOutcome::Interrupted { .. } => println!("[pipeline] interrupted (unexpected)"),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Demo 2: prebuilt ReAct agent with the scripted model.
// ---------------------------------------------------------------------------

async fn run_react() -> Result<()> {
    let mut registry = ToolRegistry::new();
    registry.register(Calculator);

    let model: Arc<dyn ChatModel> = Arc::new(ScriptedModel {
        responses: Mutex::new(
            vec![
                ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                    "call_1",
                    "calculator",
                    json!({"op": "multiply", "a": 17, "b": 3}),
                )]),
                ChatMessage::assistant("17 * 3 = 51."),
            ]
            .into(),
        ),
    });

    let graph = create_react_agent(model, registry)?;
    let spec = StateSpec::new().channel("messages", Reducer::AddMessages);

    let mut initial = State::new();
    initial.insert(
        "messages",
        json!([serde_json::to_value(ChatMessage::user("What is 17 * 3?"))?]),
    );

    let outcome = Executor::new()
        .run(
            &graph,
            &spec,
            initial,
            RunConfig::new("react-demo").with_max_steps(10),
        )
        .await?;

    if let ExecutionOutcome::Done(state) = outcome {
        let messages: Vec<ChatMessage> = state.get_as("messages")?.expect("messages channel");
        let final_answer = messages
            .iter()
            .rev()
            .find(|m| matches!(m.role, Role::Assistant) && !m.has_tool_calls())
            .and_then(|m| m.content.clone())
            .unwrap_or_default();
        println!("[react] final answer: {final_answer}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    // OTEL_DEMO_ENDPOINT unset → fmt-only local logging; set → also OTLP spans.
    let endpoint = std::env::var("OTEL_DEMO_ENDPOINT").ok();
    let mut guard = rusty_otel::init(rusty_otel::OTelConfig {
        service_name: "rusty-otel-demo".into(),
        otlp_endpoint: endpoint.clone(),
        log_filter: None,
    })
    .expect("failed to initialize tracing");

    match &endpoint {
        Some(url) => {
            println!("tracing: stderr logs + OTLP spans -> {url}");
            println!(
                "view traces: open http://localhost:16686 (Jaeger), service `rusty-otel-demo`"
            );
        }
        None => println!("tracing: stderr logs only (set OTEL_DEMO_ENDPOINT for OTLP export)"),
    }

    println!("\n--- demo 1: 2-node pipeline graph ---");
    run_pipeline().await?;

    println!("\n--- demo 2: prebuilt ReAct agent (scripted mock model) ---");
    run_react().await?;

    // Flush any buffered OTLP spans before the process exits.
    guard.shutdown();
    println!("\ndone. tracer provider shut down (spans flushed).");
    Ok(())
}
