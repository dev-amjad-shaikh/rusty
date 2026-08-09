//! Demo server: a two-node pipeline graph plus a ReAct agent (scripted
//! `ChatModel` — no network), served on `127.0.0.1:8100`.
//!
//! Every run is journaled by the Flight Recorder: the server attaches a
//! journal to the executor at run start and persists its snapshot at every
//! checkpoint boundary and at completion, so any demo run's evidence can be
//! fetched back over `GET /runs/{run_id}/events`.
//!
//! Run with: `cargo run --example server_demo`
//!
//! Test hooks (defaults unchanged — the interactive demo behaves exactly as
//! before): `RUSTY_DEMO_ADDR` overrides the bind address and
//! `RUSTY_DEMO_STORE` the JSON-file store directory. The crash-recovery
//! release proof (`rusty-server/tests/crash_recovery.rs`) uses both to run
//! this binary as a real process it can SIGKILL mid-effect and restart from
//! the same store.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rusty_agent_runtime::prelude::*;
use rusty_agent_server::{serve, GraphRegistry, ServerConfig};
use serde_json::{json, Value};

/// A scripted model: pops one canned response per call; once the script is
/// exhausted it always answers "done" (so repeated runs keep working).
struct ScriptedModel {
    script: Mutex<VecDeque<ChatMessage>>,
}

#[async_trait]
impl ChatModel for ScriptedModel {
    async fn chat(&self, _messages: &[ChatMessage], _tools: &[Value]) -> Result<ChatResponse> {
        let message = self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| ChatMessage::assistant("done"));
        Ok(ChatResponse {
            message,
            model: Some("scripted".to_string()),
            usage: None,
        })
    }
}

/// Trivial echo tool for the ReAct agent.
struct Echo;

#[async_trait]
impl Tool for Echo {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "Echoes its `text` argument back."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {"text": {"type": "string"}}})
    }
    async fn call(&self, args: Value) -> Result<Value> {
        Ok(args.get("text").cloned().unwrap_or(Value::Null))
    }
}

/// `first -> second`, appending to a `log` channel.
fn build_pipeline_graph() -> Result<(Graph, StateSpec)> {
    let spec = StateSpec::new().channel("log", Reducer::Append);
    let mut builder = GraphBuilder::new();
    builder.add_node("first", |_ctx: NodeContext| async {
        Ok(NodeOutput::update("log", json!("first")))
    });
    builder.add_node("second", |_ctx: NodeContext| async {
        Ok(NodeOutput::update("log", json!("second")))
    });
    builder.set_entry_point("first");
    builder.add_edge("first", "second");
    Ok((builder.compile()?, spec))
}

/// ReAct agent over a scripted model: one tool call, then a final answer.
fn build_react_graph() -> Result<(Graph, StateSpec)> {
    let mut tools = ToolRegistry::new();
    tools.register(Echo);
    let model: Arc<dyn ChatModel> = Arc::new(ScriptedModel {
        script: Mutex::new(VecDeque::from(vec![
            ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                "call_1",
                "echo",
                json!({"text": "pong"}),
            )]),
            ChatMessage::assistant("The echo tool said: pong."),
        ])),
    });
    let graph = create_react_agent(model, tools)?;
    let spec = StateSpec::new().channel("messages", Reducer::AddMessages);
    Ok((graph, spec))
}

#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let (pipeline, pipeline_spec) = build_pipeline_graph()?;
    let (react, react_spec) = build_react_graph()?;

    let mut registry = GraphRegistry::new();
    registry.register("pipeline", pipeline, pipeline_spec);
    registry.register("react_agent", react, react_spec);

    let config = ServerConfig::new(
        std::env::var("RUSTY_DEMO_ADDR")
            .unwrap_or_else(|_| "127.0.0.1:8100".to_string())
            .parse()
            .expect("RUSTY_DEMO_ADDR must be a socket address"),
        std::env::var("RUSTY_DEMO_STORE")
            .unwrap_or_else(|_| "./data/server-demo-checkpoints".to_string()),
    );

    // The menu below is printed with the *actual* address so the test-hook
    // override stays honest when a human runs the demo with it set.
    let base = format!("localhost:{}", config.bind_addr.port());
    println!("\nrusty-server demo on http://{base}\n");
    println!("  (Ctrl-C / SIGTERM drains gracefully: in-flight requests and runs");
    println!("   finish within the grace window, runs resume from their checkpoints)\n");
    println!("  # liveness + registered graphs");
    println!("  curl {base}/ok");
    println!("  curl {base}/info | jq\n");
    println!("  # create a thread (pipeline graph)");
    println!("  THREAD=$(curl -s -X POST {base}/threads \\");
    println!("    -H 'content-type: application/json' \\");
    println!("    -d '{{\"graph\": \"pipeline\"}}' | jq -r .thread_id)\n");
    println!("  # blocking run");
    println!("  curl -s -X POST {base}/threads/$THREAD/runs/wait \\");
    println!("    -H 'content-type: application/json' -d '{{}}' | jq\n");
    println!("  # streaming run (SSE)");
    println!("  curl -N -X POST {base}/threads/$THREAD/runs/stream \\");
    println!("    -H 'content-type: application/json' -d '{{}}'\n");
    println!("  # state + history");
    println!("  curl -s {base}/threads/$THREAD/state | jq");
    println!("  curl -s -X POST {base}/threads/$THREAD/history \\");
    println!("    -H 'content-type: application/json' -d '{{}}' | jq\n");
    println!("  # Flight Recorder: the run's journaled evidence (run_id is in the");
    println!("  # runs/wait terminal JSON, or poll GET /runs/$RUN_ID)");
    println!("  curl -s {base}/runs/$RUN_ID/events | jq");
    println!("  curl -s {base}/runs/$RUN_ID/fixture -o fixture.json  # CI replay bundle\n");
    println!("  # server-side exact replay (verified:true = evidence reproduced),");
    println!("  # and branch diff of two runs' journals");
    println!("  curl -s -X POST {base}/runs/replay \\");
    println!("    -H 'content-type: application/json' -d '{{\"run_id\": \"'$RUN_ID'\"}}' | jq");
    println!("  curl -s '{base}/runs/diff?base='$RUN_ID'&branch='$FORK_RUN_ID'' | jq\n");
    println!("  # ReAct agent (scripted model; no network)");
    println!("  REACT=$(curl -s -X POST {base}/threads \\");
    println!("    -H 'content-type: application/json' \\");
    println!("    -d '{{\"graph\": \"react_agent\"}}' | jq -r .thread_id)");
    println!("  curl -s -X POST {base}/threads/$REACT/runs/wait \\");
    println!("    -H 'content-type: application/json' \\");
    println!("    -d '{{\"input\": {{\"messages\": [{{\"role\": \"user\", \"content\": \"say pong\"}}]}}}}' | jq\n");

    serve(registry, config).await?;
    Ok(())
}
