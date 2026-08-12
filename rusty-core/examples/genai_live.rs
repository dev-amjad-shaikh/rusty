//! Live demo: a real ReAct agent over [`GenaiChatModel`] — the multi-provider
//! adapter behind the `genai` feature (provider layer, wave 2).
//!
//! Unlike `live_agent.rs` (one OpenAI-compatible endpoint), this demo rides
//! genai's native-protocol routing: the model string alone selects the
//! provider, and API keys resolve from each provider's conventional
//! environment variable.
//!
//! # Configuration (environment variables)
//!
//! | variable      | default        | notes                                   |
//! |---------------|----------------|-----------------------------------------|
//! | `GENAI_MODEL` | `gpt-4o-mini`  | provider-selecting model string         |
//!
//! Model-string routing examples: `gpt-…` → OpenAI (`OPENAI_API_KEY`),
//! `claude-…` → Anthropic (`ANTHROPIC_API_KEY`), `gemini-…` → Gemini
//! (`GEMINI_API_KEY`), `ollama::llama3.1` → a local Ollama (no key).
//!
//! # Run it (manual validation only — never in CI)
//!
//! ```text
//! OPENAI_API_KEY=sk-...  cargo run --example genai_live --features genai
//! ANTHROPIC_API_KEY=...  GENAI_MODEL=claude-haiku-4-5 \
//!   cargo run --example genai_live --features genai
//! ollama pull llama3.1 && ollama serve
//! GENAI_MODEL=ollama::llama3.1 cargo run --example genai_live --features genai
//! ```
//!
//! Requires real API keys (or a local Ollama) and network access; the wave-2
//! gate asks for a manual pass against OpenAI and at least one non-OpenAI
//! provider (Anthropic or Gemini). If the provider is unreachable or the key
//! is missing, the demo prints the classified error and exits 0.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use rusty_agent_runtime::prelude::*;
use rusty_agent_runtime::react::create_react_agent_streaming;
use serde_json::{json, Value};

const DEFAULT_MODEL: &str = "gpt-4o-mini";

// ---------------------------------------------------------------------------
// Tool 1: get_current_time — real wall-clock time, no arguments.
// ---------------------------------------------------------------------------

struct GetCurrentTime;

#[async_trait]
impl Tool for GetCurrentTime {
    fn name(&self) -> &str {
        "get_current_time"
    }
    fn description(&self) -> &str {
        "Returns the current date and time in UTC."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    async fn call(&self, _args: Value) -> Result<Value> {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| RustyError::Tool(format!("system clock before epoch: {e}")))?
            .as_secs();
        println!("    [tool:get_current_time] -> {secs}s since epoch");
        Ok(json!({"unix_seconds": secs}))
    }
}

// ---------------------------------------------------------------------------
// Tool 2: calculator — basic arithmetic on two numbers.
// ---------------------------------------------------------------------------

struct Calculator;

#[async_trait]
impl Tool for Calculator {
    fn name(&self) -> &str {
        "calculator"
    }
    fn description(&self) -> &str {
        "Basic arithmetic on two numbers `a` and `b`."
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
        let op = args.get("op").and_then(Value::as_str).unwrap_or("add");
        let a = args.get("a").and_then(Value::as_f64).unwrap_or(0.0);
        let b = args.get("b").and_then(Value::as_f64).unwrap_or(0.0);
        let result = match op {
            "add" => a + b,
            "subtract" => a - b,
            "multiply" => a * b,
            "divide" if b != 0.0 => a / b,
            "divide" => return Err(RustyError::Tool("division by zero".into())),
            other => return Err(RustyError::Tool(format!("unknown op `{other}`"))),
        };
        println!("    [tool:calculator] {a} {op} {b} = {result}");
        Ok(json!(result))
    }
}

// ---------------------------------------------------------------------------
// The demo.
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    println!("=== Rusty Core: LIVE ReAct agent over the genai adapter ===\n");

    // 1. The model string selects the provider through genai's routing.
    let model = std::env::var("GENAI_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
    println!("model    : {model}");
    println!("api keys : resolved from the provider's conventional env var");
    println!("           (OPENAI_API_KEY / ANTHROPIC_API_KEY / GEMINI_API_KEY, ...)\n");

    // 2. The multi-provider model: one adapter, whichever provider the model
    //    string routes to.
    let model: Arc<dyn ChatModel> = Arc::new(GenaiChatModel::new(&model));

    // 3. Two real tools.
    let mut registry = ToolRegistry::new();
    registry.register(GetCurrentTime);
    registry.register(Calculator);

    // 4. The event channel carries both executor events and live token deltas.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<GraphEvent>(64);

    // 5. The prebuilt ReAct graph, streaming variant.
    let graph = create_react_agent_streaming(model, registry, tx.clone())?;
    println!(
        "graph compiled: {} nodes, entry point `{}`\n",
        graph.node_count(),
        graph.entry_point()
    );

    // 6. A question that needs both tools.
    let spec = StateSpec::new().channel("messages", Reducer::AddMessages);
    let question = "What is the current UNIX time in seconds? Then multiply 128 by 46.";
    let mut initial = State::new();
    initial.insert(
        "messages",
        json!([serde_json::to_value(ChatMessage::user(question))?]),
    );
    println!("user: {question}\n");
    println!("--- live event stream ---");

    // 7. Pretty-print the GraphEvent stream as the loop runs.
    let tracer = tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            match event {
                GraphEvent::SuperStep { step, active_nodes } => {
                    println!("[step {step}] active: {}", active_nodes.join(", "));
                }
                GraphEvent::NodeStart { node, step } => {
                    println!("  |- {node} start (step {step})");
                }
                GraphEvent::NodeEnd { node, step } => {
                    println!("  |- {node} end   (step {step})");
                }
                GraphEvent::Token { node, delta } => {
                    print!("  |- {node} token: {delta}");
                }
                GraphEvent::StateUpdate { step, updates } => {
                    println!(
                        "  |- state merge (step {step}): channels [{}]",
                        updates.keys().cloned().collect::<Vec<_>>().join(", ")
                    );
                }
                GraphEvent::CheckpointSaved {
                    checkpoint_id,
                    step,
                } => {
                    println!("  |- checkpoint {checkpoint_id} (step {step})");
                }
            }
        }
    });

    // 8. Run — and treat a failed run as a configuration hint, not a crash:
    //    this example is manual validation and never runs in CI, but a
    //    missing key should still exit 0 with a readable message.
    let config = RunConfig::new("genai-live-demo")
        .with_max_steps(12)
        .with_event_tx(tx);
    let outcome = match Executor::new().run(&graph, &spec, initial, config).await {
        Ok(outcome) => outcome,
        Err(e) => {
            drop(tracer);
            println!("\n--- could not complete the run ---");
            println!("error: {e}");
            println!("class: {}", e.llm_class());
            println!("\nCheck that GENAI_MODEL routes to the provider you intended and");
            println!("that its API-key environment variable is set (see the header docs).");
            return Ok(());
        }
    };
    drop(tracer);

    // 9. Print the final answer.
    println!("\n--- final answer ---");
    match &outcome {
        ExecutionOutcome::Done(state) => {
            let messages: Vec<ChatMessage> =
                state.get_as("messages")?.expect("messages channel present");
            match messages
                .iter()
                .rev()
                .find(|m| m.role == Role::Assistant && !m.has_tool_calls())
            {
                Some(m) => println!("{}", m.content.as_deref().unwrap_or("<empty>")),
                None => println!("<no final assistant answer>"),
            }
        }
        ExecutionOutcome::Interrupted { value, .. } => {
            println!("run interrupted with payload: {value}");
        }
    }

    Ok(())
}
