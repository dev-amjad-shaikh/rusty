//! Demo server: a two-node pipeline graph plus a ReAct agent (deterministic
//! local `ChatModel` — no network), served on `127.0.0.1:8100`.
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

use std::sync::Arc;

use async_trait::async_trait;
use rusty_agent_runtime::connector::packs;
use rusty_agent_runtime::prelude::*;
use rusty_agent_runtime::tool::builtins::{
    CalculatorTool, KnowledgeDocument, KnowledgeSearchTool, SandboxedDocumentReaderTool,
    TextInspectorTool,
};
use rusty_agent_server::{GraphRegistry, ServerConfig, serve};
use serde_json::{Value, json};

/// A deterministic local model that exercises the complete tool pipeline on
/// every new thread. It keeps the demo credential-free while producing real
/// model-call and tool-call evidence for Studio.
struct HarnessDemoModel;

#[async_trait]
impl ChatModel for HarnessDemoModel {
    async fn chat(&self, messages: &[ChatMessage], _tools: &[Value]) -> Result<ChatResponse> {
        let message = if messages.iter().any(|message| message.role == Role::Tool) {
            ChatMessage::assistant(
                "The local capability pack completed its calculation, text inspection, knowledge search, document read, and echo calls.",
            )
        } else {
            ChatMessage::assistant_tool_calls(vec![
                ToolCall::new("call_echo", "echo", json!({"text": "pong"})),
                ToolCall::new(
                    "call_calculator",
                    "calculator",
                    json!({"operation": "multiply", "left": 7, "right": 6}),
                ),
                ToolCall::new(
                    "call_inspect",
                    "inspect_text",
                    json!({"text": "Rusty records exact tool evidence."}),
                ),
                ToolCall::new(
                    "call_search",
                    "search_knowledge",
                    json!({"query": "Rusty tool evidence", "limit": 2}),
                ),
                ToolCall::new(
                    "call_document",
                    "read_document",
                    json!({"path": "capability-pack.md"}),
                ),
            ])
        };
        Ok(ChatResponse {
            message,
            model: Some("rusty-harness-demo".to_string()),
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
    fn effect(&self) -> Effect {
        Effect::Pure
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

/// ReAct agent over a deterministic local model and a safe capability pack.
fn build_react_graph() -> Result<(Graph, StateSpec, ToolRegistry)> {
    let mut tools = ToolRegistry::new();
    tools.register(Echo);
    tools.register(CalculatorTool);
    tools.register(TextInspectorTool);
    tools.register(SandboxedDocumentReaderTool::new(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/demo_documents"),
    )?);
    tools.register(KnowledgeSearchTool::new(vec![
        KnowledgeDocument {
            id: "runtime".into(),
            title: "Rusty runtime".into(),
            text: "Rusty executes typed tools through an effect-aware registry and records every call in the Flight Recorder.".into(),
        },
        KnowledgeDocument {
            id: "studio".into(),
            title: "Rusty Studio".into(),
            text: "Studio creates versioned agents, starts work, and hands exact run evidence to Trace and Evaluate.".into(),
        },
    ])?);
    let model: Arc<dyn ChatModel> = Arc::new(HarnessDemoModel);
    let graph = create_react_agent(model, tools.clone())?;
    let spec = StateSpec::new().channel("messages", Reducer::AddMessages);
    Ok((graph, spec, tools))
}

/// Seed the ServiceNow service-pack manifest into the demo's connector
/// plane, so Studio's Connectors page has a basic-auth connector to walk
/// through on first boot. Registration is idempotent by content hash: a
/// restart against a store that already holds the pack converges with
/// `already_registered: true`, and nothing here blocks serving.
///
/// The pack is instance-agnostic: it declares an `instance` config param
/// and pins `https://{instance}.service-now.com`. The operator names the
/// real instance at instantiation — Studio's instantiate journey asks for
/// it next to the credentials, or `POST /connectors/instances` takes it as
/// `config: {"instance": "<subdomain>"}`.
async fn seed_servicenow_manifest(addr: std::net::SocketAddr) -> std::result::Result<(), String> {
    let manifest = packs::servicenow().map_err(|e| e.to_string())?;
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();

    // The listener comes up inside `serve`; poll `/ok` briefly rather than
    // racing the bind. A demo that never comes up gives up with a warning
    // instead of hanging the runtime on a stray task.
    let mut ready = false;
    for _ in 0..120 {
        match client.get(format!("{base}/ok")).send().await {
            Ok(response) if response.status().is_success() => {
                ready = true;
                break;
            }
            _ => tokio::time::sleep(std::time::Duration::from_millis(500)).await,
        }
    }
    if !ready {
        return Err("the demo server never answered /ok".to_owned());
    }

    let response = client
        .post(format!("{base}/connectors/manifests"))
        .json(&manifest)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!(
            "POST /connectors/manifests answered {status}: {body}"
        ));
    }
    Ok(())
}

#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let (pipeline, pipeline_spec) = build_pipeline_graph()?;
    let (react, react_spec, react_tools) = build_react_graph()?;

    let mut registry = GraphRegistry::new();
    registry.register("pipeline", pipeline, pipeline_spec);
    registry.register_with_tools("react_agent", react, react_spec, &react_tools)?;

    let config = ServerConfig::new(
        std::env::var("RUSTY_DEMO_ADDR")
            .unwrap_or_else(|_| "127.0.0.1:8100".to_string())
            .parse()
            .expect("RUSTY_DEMO_ADDR must be a socket address"),
        std::env::var("RUSTY_DEMO_STORE")
            .unwrap_or_else(|_| "./data/server-demo-checkpoints".to_string()),
    )
    // The demo deployment speaks the consent-free OAuth flows (password,
    // client-credentials) against the token endpoint each connection
    // records — ServiceNow's `/oauth_token.do` is the reference shape.
    .with_oauth_provider(Arc::new(
        rusty_agent_server::oauth::ReqwestOAuthProvider::new(),
    ));

    // Seed the connector plane once the listener is up (see the function's
    // docs): Studio's credential walkthrough needs the ServiceNow pack on a
    // fresh store, and the POST is idempotent by content hash on an old one.
    {
        let seed_addr = config.bind_addr;
        tokio::spawn(async move {
            if let Err(error) = seed_servicenow_manifest(seed_addr).await {
                tracing::warn!(error = %error, "ServiceNow demo manifest seeding skipped");
            }
        });
    }

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
    println!("  # connector plane: the ServiceNow Table API pack manifest is seeded");
    println!("  # on boot, instance-agnostic; instantiation supplies the subdomain");
    println!("  # as config: {{\"instance\": \"<subdomain>\"}}");
    println!("  curl -s {base}/connectors/manifests | jq\n");
    println!("  # ReAct agent (deterministic local model; no network)");
    println!("  REACT=$(curl -s -X POST {base}/threads \\");
    println!("    -H 'content-type: application/json' \\");
    println!("    -d '{{\"graph\": \"react_agent\"}}' | jq -r .thread_id)");
    println!("  curl -s -X POST {base}/threads/$REACT/runs/wait \\");
    println!("    -H 'content-type: application/json' \\");
    println!(
        "    -d '{{\"input\": {{\"messages\": [{{\"role\": \"user\", \"content\": \"say pong\"}}]}}}}' | jq\n"
    );

    serve(registry, config).await?;
    Ok(())
}
