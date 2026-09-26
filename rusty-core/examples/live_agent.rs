//! Live demo: a real ReAct agent against any OpenAI-compatible
//! chat-completions endpoint — OpenAI, Ollama, vLLM, LM Studio, ...
//!
//! Unlike `react_agent.rs` (scripted mock model, no network), this example
//! drives [`create_react_agent_streaming`] with a real
//! [`OpenAiCompatibleClient`] and three real tools (`get_current_time`,
//! `calculator`, `word_count`), while pretty-printing the [`GraphEvent`]
//! stream — including live token deltas — as the agent reasons and acts.
//!
//! # Configuration (environment variables)
//!
//! | variable             | default                        | notes                        |
//! |----------------------|--------------------------------|------------------------------|
//! | `RUSTY_BASE_URL`| `http://localhost:11434/v1`    | Ollama's OpenAI shim         |
//! | `RUSTY_API_KEY` | `ollama`                       | any string works for Ollama  |
//! | `RUSTY_MODEL`   | `llama3.1`                     | must support tool calling    |
//!
//! # Run it
//!
//! ```text
//! # Local, free (Ollama):
//! ollama pull llama3.1 && ollama serve
//! cargo run --example live_agent
//!
//! # OpenAI:
//! RUSTY_BASE_URL=https://api.openai.com/v1 \
//! RUSTY_API_KEY=sk-... \
//! RUSTY_MODEL=gpt-4o-mini \
//! cargo run --example live_agent
//! ```
//!
//! If the endpoint is unreachable the demo prints setup instructions and
//! exits with status 0 — it never panics, so it is safe to run in CI.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use rusty_agent_runtime::prelude::*;
use rusty_agent_runtime::react::create_react_agent_streaming;
use serde_json::{json, Value};

const DEFAULT_BASE_URL: &str = "http://localhost:11434/v1";
const DEFAULT_API_KEY: &str = "ollama";
const DEFAULT_MODEL: &str = "llama3.1";

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
        let (date, clock) = unix_to_utc(secs);
        println!("    [tool:get_current_time] -> {date} {clock} UTC");
        Ok(json!({
            "utc": format!("{date} {clock}"),
            "unix_seconds": secs,
        }))
    }
}

/// Convert UNIX seconds to `(YYYY-MM-DD, HH:MM:SS)` in UTC.
/// (Howard Hinnant's civil-from-days algorithm; keeps the example chrono-free.)
fn unix_to_utc(secs: u64) -> (String, String) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let clock = format!("{:02}:{:02}:{:02}", rem / 3600, (rem / 60) % 60, rem % 60);

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // day of era [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // year of era [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of year [0, 365]
    let mp = (5 * doy + 2) / 153; // month index [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (format!("{y:04}-{m:02}-{d:02}"), clock)
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
        // Small models behind OpenAI-compatible shims (Ollama's tool-call
        // emulation in particular) are loose with argument shapes: numbers
        // arrive quoted (`{"a": "128"}`), keys get renamed (`operation`,
        // `lhs`/`rhs`, `x`/`y`). `Value::as_f64() + unwrap_or(0.0)` used to
        // swallow all of that into a silent `0 op 0 = 0`, so coerce
        // defensively — and when coercion still fails, fail the tool call
        // with the raw payload: the `ToolExecutor` turns tool errors into
        // `ERROR:` tool messages the model can see and react to, whereas a
        // printed warning is invisible to it.
        let op = get_any(&args, &["op", "operation", "operator"])
            .and_then(Value::as_str)
            .unwrap_or("add")
            .to_ascii_lowercase();
        let a = get_any(&args, &["a", "lhs", "left", "first_operand", "x"]).and_then(coerce_f64);
        let b = get_any(&args, &["b", "rhs", "right", "second_operand", "y"]).and_then(coerce_f64);
        let (a, b) = match (a, b) {
            (Some(a), Some(b)) => (a, b),
            _ => {
                return Err(RustyError::Tool(format!(
                    "calculator: could not coerce operands to numbers; raw args: {args}"
                )));
            }
        };
        let result = match op.as_str() {
            "add" | "plus" | "sum" => a + b,
            "subtract" | "minus" | "difference" => a - b,
            "multiply" | "times" | "product" => a * b,
            "divide" | "quotient" => {
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

/// Coerce a JSON value to `f64`, accepting real numbers *and* numeric
/// strings (`"128"`, `" 46.5 "`) — Ollama-style tool-call emulation quotes
/// numbers surprisingly often.
fn coerce_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// Fetch the first present key from `args` among `keys` (primary name first,
/// then common aliases small models invent).
fn get_any<'a>(args: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|k| args.get(*k))
}

// ---------------------------------------------------------------------------
// Tool 3: word_count — words / characters / lines of a text.
// ---------------------------------------------------------------------------

struct WordCount;

#[async_trait]
impl Tool for WordCount {
    fn name(&self) -> &str {
        "word_count"
    }
    fn description(&self) -> &str {
        "Counts the words, characters, and lines in a piece of text."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "text": {"type": "string", "description": "The text to analyze."}
            },
            "required": ["text"]
        })
    }
    async fn call(&self, args: Value) -> Result<Value> {
        let text = args.get("text").and_then(Value::as_str).unwrap_or("");
        let stats = json!({
            "words": text.split_whitespace().count(),
            "characters": text.chars().count(),
            "lines": text.lines().count(),
        });
        println!("    [tool:word_count] -> {stats}");
        Ok(stats)
    }
}

// ---------------------------------------------------------------------------
// The demo.
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    println!("=== Rusty Core: LIVE ReAct agent demo (real LLM endpoint) ===\n");

    // 1. Endpoint configuration from the environment.
    let base_url = std::env::var("RUSTY_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
    let api_key = std::env::var("RUSTY_API_KEY").unwrap_or_else(|_| DEFAULT_API_KEY.to_string());
    let model = std::env::var("RUSTY_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());

    println!("endpoint : {base_url}");
    println!("model    : {model}\n");

    // 2. A real OpenAI-compatible client. Timeouts keep the demo snappy when
    //    the endpoint is down or hung (connection refused fails fast anyway).
    //    `expect` is deliberate: a builder failure here means the demo is
    //    misconfigured and should die loudly, not silently drop its timeouts.
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(120))
        .build()
        .expect("reqwest client builder with only timeouts must build");
    let client =
        OpenAiCompatibleClient::new(&base_url, Some(api_key), &model).with_http_client(http);

    // 3. Three real tools.
    let mut registry = ToolRegistry::new();
    registry.register(GetCurrentTime);
    registry.register(Calculator);
    registry.register(WordCount);

    // 4. The event channel: the graph's agent node forwards token deltas
    //    into it, and the executor emits its own events into the same sink.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<GraphEvent>(64);

    // 5. The prebuilt ReAct graph, streaming variant: agent ⇄ tools over the
    //    `messages` channel, with token deltas forwarded as GraphEvent::Token.
    let model: Arc<dyn ChatModel> = Arc::new(client);
    let graph = create_react_agent_streaming(model, registry, tx.clone())?;
    println!(
        "graph compiled: {} nodes, entry point `{}`\n",
        graph.node_count(),
        graph.entry_point()
    );

    // 6. Seed the conversation with a question that needs all three tools.
    let spec = StateSpec::new().channel("messages", Reducer::AddMessages);
    let question = "What time is it right now in UTC? Then multiply 128 by 46, \
                    and count the words in 'the quick brown fox jumps over the lazy dog'.";
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
                    println!("[step {step}] ▶ active: {}", active_nodes.join(", "));
                }
                GraphEvent::NodeStart { node, step } => {
                    println!("  ├─ {node} ▶ start (step {step})");
                }
                GraphEvent::NodeEnd { node, step } => {
                    println!("  ├─ {node} ✔ end   (step {step})");
                }
                GraphEvent::Token { node, delta } => {
                    print!("  ├─ {node} ⚡ token: {delta}");
                }
                GraphEvent::StateUpdate { step, updates } => {
                    println!(
                        "  ├─ state merge (step {step}): channels [{}]",
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

    // 8. Run — and treat a failed run as a friendly setup hint, not a crash.
    let config = RunConfig::new("live-demo")
        .with_max_steps(12)
        .with_event_tx(tx);
    let outcome = match Executor::new().run(&graph, &spec, initial, config).await {
        Ok(outcome) => outcome,
        Err(e) => {
            drop(tracer);
            print_setup_instructions(&base_url, &e);
            return Ok(()); // never panic: safe for CI without an LLM running
        }
    };
    drop(tracer); // the trace task ends once the sender is dropped

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

/// Friendly troubleshooting block when the LLM endpoint cannot be reached.
fn print_setup_instructions(base_url: &str, error: &RustyError) {
    println!("\n--- could not complete the run ---");
    println!("error: {error}\n");
    println!("No OpenAI-compatible endpoint answered at `{base_url}`.");
    println!("To see this demo in action, start one:\n");
    println!("  Option A — Ollama (local, free):");
    println!("    1. install from https://ollama.com");
    println!("    2. ollama pull llama3.1");
    println!("    3. ollama serve        # listens on http://localhost:11434/v1");
    println!("    4. cargo run --example live_agent\n");
    println!("  Option B — OpenAI:");
    println!("    RUSTY_BASE_URL=https://api.openai.com/v1 \\");
    println!("    RUSTY_API_KEY=sk-... \\");
    println!("    RUSTY_MODEL=gpt-4o-mini \\");
    println!("    cargo run --example live_agent\n");
    println!("  Option C — vLLM / LM Studio:");
    println!("    point RUSTY_BASE_URL at the server's /v1 path and set");
    println!("    RUSTY_MODEL to the served model name.\n");
    println!("(exiting 0 so CI stays green without a live model)");
}

// ---------------------------------------------------------------------------
// Tests for the calculator's defensive argument coercion (the 2026-08-05
// live-transcript bug: Ollama quoted numeric args and `as_f64()` swallowed
// them into a silent `0 op 0`).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coerce_f64_accepts_numbers_and_numeric_strings() {
        assert_eq!(coerce_f64(&json!(128)), Some(128.0));
        assert_eq!(coerce_f64(&json!(46.5)), Some(46.5));
        assert_eq!(coerce_f64(&json!("128")), Some(128.0));
        assert_eq!(coerce_f64(&json!(" 46.5 ")), Some(46.5));
        assert_eq!(coerce_f64(&json!("-3")), Some(-3.0));
        assert_eq!(coerce_f64(&json!("abc")), None);
        assert_eq!(coerce_f64(&json!("")), None);
        assert_eq!(coerce_f64(&json!(true)), None);
        assert_eq!(coerce_f64(&Value::Null), None);
        assert_eq!(coerce_f64(&json!({"nested": 1})), None);
    }

    #[test]
    fn get_any_falls_back_to_alias_keys() {
        let args = json!({"operation": "multiply", "lhs": 6, "rhs": 7});
        assert_eq!(
            get_any(&args, &["op", "operation", "operator"]).and_then(Value::as_str),
            Some("multiply")
        );
        assert_eq!(
            get_any(&args, &["a", "lhs", "left", "first_operand", "x"]).and_then(coerce_f64),
            Some(6.0)
        );
        assert_eq!(
            get_any(&args, &["b", "rhs", "right", "second_operand", "y"]).and_then(coerce_f64),
            Some(7.0)
        );
        // Primary key wins when both primary and alias are present.
        let both = json!({"a": 1, "x": 2});
        assert_eq!(
            get_any(&both, &["a", "lhs", "left", "first_operand", "x"]).and_then(coerce_f64),
            Some(1.0)
        );
        assert!(get_any(&json!({}), &["a", "x"]).is_none());
    }

    #[tokio::test]
    async fn calculator_handles_ollama_quoted_number_args() {
        // The exact shape observed in the wild: op as string, operands quoted.
        let out = Calculator
            .call(json!({"op": "multiply", "a": "128", "b": "46"}))
            .await
            .unwrap();
        assert_eq!(out, json!(5888.0));
    }

    #[tokio::test]
    async fn calculator_handles_alias_keys_and_mixed_types() {
        let out = Calculator
            .call(json!({"operation": "add", "lhs": "1.5", "rhs": 2}))
            .await
            .unwrap();
        assert_eq!(out, json!(3.5));
    }

    #[tokio::test]
    async fn calculator_still_rejects_unknown_ops_and_divide_by_zero() {
        assert!(Calculator
            .call(json!({"op": "modulo", "a": 1, "b": 2}))
            .await
            .is_err());
        assert!(Calculator
            .call(json!({"op": "divide", "a": 1, "b": 0}))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn calculator_errors_with_raw_args_when_operands_do_not_coerce() {
        // Non-coercible operands are a tool *error* the model can see (the
        // ToolExecutor surfaces it as an ERROR: tool message), never a
        // confident `0 op 0` result.
        let err = Calculator
            .call(json!({"op": "add", "a": "abc", "b": 2}))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("could not coerce"), "got: {msg}");
        assert!(msg.contains("abc"), "raw args must be embedded, got: {msg}");

        // Missing operand: same treatment.
        let err = Calculator
            .call(json!({"op": "add", "a": 1}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("could not coerce"), "got: {err}");
    }
}
