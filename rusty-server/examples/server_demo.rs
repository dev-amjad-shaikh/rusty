//! Demo server: a two-node pipeline graph, a ReAct agent (deterministic
//! local `ChatModel` — no network), and a long-running `deep-dive` graph
//! that parks in `interrupted` until resumed, served on `127.0.0.1:8100`.
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
//! the same store. `RUSTY_DEMO_STAGE_DELAY_MS` overrides the `deep-dive`
//! stage delay (default 75 000 ms) so automated proofs don't wait minutes.

use std::sync::Arc;

use async_trait::async_trait;
use rusty_agent_runtime::connector::{
    ConnectorManifest, ConnectorOperation, HttpMethod, OperationAuth, OperationEffect,
};
use rusty_agent_runtime::prelude::*;
use rusty_agent_runtime::tool::builtins::{
    CalculatorTool, KnowledgeDocument, KnowledgeSearchTool, SandboxedDocumentReaderTool,
    TextInspectorTool,
};
use rusty_agent_server::{serve, GraphRegistry, ServerConfig};
use serde_json::{json, Value};

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

/// `gather -> analyze -> report`: a long-running three-stage graph over a
/// `log` channel, built so Studio's Command Center has real Working and
/// Needs-you evidence. `gather` and `analyze` each sleep (async, so the
/// executor stays responsive) between a start and a done marker; `report`
/// raises an interrupt and parks the run in `interrupted` until it is
/// resumed with `command.resume`, then appends the published marker. Every
/// stage boundary is a super-step barrier, so the store checkpoints between
/// stages and a crash/restart resumes from the last completed stage.
fn build_deep_dive_graph() -> Result<(Graph, StateSpec)> {
    let spec = StateSpec::new().channel("log", Reducer::Append);
    let mut builder = GraphBuilder::new();

    // One output per node: updates merge at the super-step barrier, and an
    // array update extends an `Append` channel in order, so the start
    // marker lands ahead of the done marker in the log.
    builder.add_node("gather", |_ctx: NodeContext| async {
        tokio::time::sleep(stage_delay()).await;
        Ok(NodeOutput::update(
            "log",
            json!(["gather: started", "gather: done"]),
        ))
    });
    builder.add_node("analyze", |_ctx: NodeContext| async {
        tokio::time::sleep(stage_delay()).await;
        Ok(NodeOutput::update(
            "log",
            json!(["analyze: started", "analyze: done"]),
        ))
    });
    builder.add_node("report", |ctx: NodeContext| async move {
        if ctx.resume_value().is_none() {
            return Err(ctx.interrupt(json!({
                "question": "Publish the deep-dive findings?",
                "stage": "report"
            })));
        }
        Ok(NodeOutput::update("log", json!("report: published")))
    });

    builder.set_entry_point("gather");
    builder.add_edge("gather", "analyze");
    builder.add_edge("analyze", "report");
    Ok((builder.compile()?, spec))
}

/// The `deep-dive` stage delay: 75 seconds by default so runs read as
/// genuinely long-running on the board; `RUSTY_DEMO_STAGE_DELAY_MS`
/// shortens it for automated proofs.
fn stage_delay() -> std::time::Duration {
    std::env::var("RUSTY_DEMO_STAGE_DELAY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(std::time::Duration::from_millis)
        .unwrap_or_else(|| std::time::Duration::from_secs(75))
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

/// The ServiceNow demo pack, instance-agnostic per
/// `docs/connector-surface-design.md`: the manifest pins
/// `https://{instance}.service-now.com` and a draft-07
/// `connection_specification` — `instance` (pattern-constrained
/// subdomain) plus a `credentials` oneOf (basic: username + password,
/// both `rusty_secret`; or an OAuth token) — with Table API operations
/// (get-record, list-records, create-incident) and a parameterless
/// read-only check (`GET /api/now/table/sys_user?sysparm_limit=1`).
/// The operator's instance and credentials arrive with the config at
/// instantiation, never in the content-pinned manifest.
fn servicenow_pack() -> ConnectorManifest {
    let spec = json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "title": "ServiceNow Connection Spec",
        "type": "object",
        "required": ["instance", "credentials"],
        "additionalProperties": false,
        "properties": {
            "instance": {
                "type": "string",
                "title": "Instance",
                "pattern": "^[a-z0-9-]+$",
                "rusty_pattern_descriptor": "your-instance.service-now.com",
                "rusty_order": 0
            },
            "credentials": {
                "type": "object",
                "title": "Authentication",
                "rusty_order": 1,
                "rusty_group": "auth",
                "oneOf": [
                    {
                        "title": "Basic",
                        "type": "object",
                        "required": ["auth", "username", "password"],
                        "additionalProperties": false,
                        "properties": {
                            "auth": {"type": "string", "const": "basic"},
                            "username": {"type": "string", "title": "Username", "rusty_secret": true},
                            "password": {"type": "string", "title": "Password", "rusty_secret": true}
                        }
                    },
                    {
                        "title": "OAuth token",
                        "type": "object",
                        "required": ["auth", "token"],
                        "additionalProperties": false,
                        "properties": {
                            "auth": {"type": "string", "const": "oauth"},
                            "token": {"type": "string", "title": "Access token", "rusty_secret": true}
                        }
                    }
                ]
            }
        }
    });
    let auth = vec![
        OperationAuth::Basic {
            username: "{credentials.username}".to_owned(),
            password: "{credentials.password}".to_owned(),
        },
        OperationAuth::Bearer {
            token: "{credentials.token}".to_owned(),
        },
    ];
    let op = |name: &str,
              method: HttpMethod,
              path: &str,
              effect: OperationEffect,
              params: Value,
              description: &str| {
        ConnectorOperation {
            name: name.to_owned(),
            description: description.to_owned(),
            method,
            path: path.to_owned(),
            effect,
            params_schema: params,
            headers: Vec::new(),
            auth: auth.clone(),
            max_response_bytes: None,
        }
    };
    ConnectorManifest::new(
        "servicenow",
        "1",
        "ServiceNow",
        "ServiceNow Table API: get and list records in any table, and create incidents.",
        "https://www.servicenow.com/docs/",
        "https://{instance}.service-now.com",
        spec,
        vec![
            op(
                "get-record",
                HttpMethod::Get,
                "/api/now/table/{table}/{sys_id}",
                OperationEffect::ReadOnly,
                json!({
                    "type": "object",
                    "required": ["table", "sys_id"],
                    "properties": {"table": {"type": "string"}, "sys_id": {"type": "string"}}
                }),
                "Get one record from a ServiceNow table by sys_id.",
            ),
            op(
                "list-records",
                HttpMethod::Get,
                "/api/now/table/{table}",
                OperationEffect::ReadOnly,
                json!({
                    "type": "object",
                    "required": ["table"],
                    "properties": {
                        "table": {"type": "string"},
                        "sysparm_query": {"type": "string"},
                        "sysparm_fields": {"type": "string"},
                        "sysparm_limit": {"type": "integer"},
                        "sysparm_offset": {"type": "integer"}
                    }
                }),
                "List records from a ServiceNow table, with sysparm filtering and pagination.",
            ),
            op(
                "create-incident",
                HttpMethod::Post,
                "/api/now/table/incident",
                OperationEffect::Compensatable,
                json!({
                    "type": "object",
                    "required": ["short_description"],
                    "properties": {
                        "short_description": {"type": "string"},
                        "description": {"type": "string"},
                        "urgency": {"type": "string"},
                        "impact": {"type": "string"}
                    }
                }),
                "Create an incident in ServiceNow.",
            ),
            op(
                "check-connection",
                HttpMethod::Get,
                "/api/now/table/sys_user?sysparm_limit=1",
                OperationEffect::ReadOnly,
                json!({"type": "object"}),
                "Verify connectivity and credentials by reading one sys_user row.",
            ),
        ],
        "check-connection",
    )
    .expect("the ServiceNow demo pack validates")
}

/// Seed the ServiceNow pack into the demo's connector surface, so
/// Studio's Connectors page has a schema-driven connector to walk
/// through on first boot. Registration is idempotent by content hash: a
/// restart against a store that already holds the pack converges with
/// `registered: false`, and nothing here blocks serving.
///
/// Instantiate it with `POST /connectors/instances` —
/// `config: {"instance": "<subdomain>", "credentials": {"auth": "basic",
/// "username": …, "password": …}}`; the secrets seal through the
/// broker before anything persists.
async fn seed_servicenow_pack(addr: std::net::SocketAddr) -> std::result::Result<(), String> {
    let manifest = servicenow_pack();
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();

    // The listener comes up inside `serve`; poll `/ok` briefly rather
    // than racing the bind. A demo that never comes up gives up with a
    // warning instead of hanging the runtime on a stray task.
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
        .post(format!("{base}/connectors"))
        .json(&manifest)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!("POST /connectors answered {status}: {body}"));
    }
    Ok(())
}

#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let (pipeline, pipeline_spec) = build_pipeline_graph()?;
    let (react, react_spec, react_tools) = build_react_graph()?;
    let (deep_dive, deep_dive_spec) = build_deep_dive_graph()?;

    let mut registry = GraphRegistry::new();
    registry.register("pipeline", pipeline, pipeline_spec);
    registry.register_with_tools("react_agent", react, react_spec, &react_tools)?;
    registry.register("deep-dive", deep_dive, deep_dive_spec);

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

    // Seed the connector surface once the listener is up (see the
    // function's contract): the ServiceNow pack registers by content
    // hash, idempotently.
    {
        let seed_addr = config.bind_addr;
        tokio::spawn(async move {
            if let Err(error) = seed_servicenow_pack(seed_addr).await {
                tracing::warn!(error = %error, "ServiceNow demo pack seeding skipped");
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
    println!("  # ReAct agent (deterministic local model; no network)");
    println!("  REACT=$(curl -s -X POST {base}/threads \\");
    println!("    -H 'content-type: application/json' \\");
    println!("    -d '{{\"graph\": \"react_agent\"}}' | jq -r .thread_id)");
    println!("  curl -s -X POST {base}/threads/$REACT/runs/wait \\");
    println!("    -H 'content-type: application/json' \\");
    println!(
        "    -d '{{\"input\": {{\"messages\": [{{\"role\": \"user\", \"content\": \"say pong\"}}]}}}}' | jq\n"
    );
    println!("  # connector surface: the ServiceNow Table API pack is seeded");
    println!("  # (instance-agnostic: config supplies the subdomain + credentials)");
    println!("  curl -s {base}/connectors | jq '.manifests[].id'\n");
    println!("  # deep-dive: long-running stages, then parks in `interrupted` at the");
    println!("  # report stage until resumed (Working / Needs-you evidence)");
    println!("  DEEP=$(curl -s -X POST {base}/threads \\");
    println!("    -H 'content-type: application/json' \\");
    println!("    -d '{{\"graph\": \"deep-dive\"}}' | jq -r .thread_id)");
    println!("  RUN=$(curl -s -X POST {base}/threads/$DEEP/runs \\");
    println!("    -H 'content-type: application/json' -d '{{}}' | jq -r .run_id)");
    println!("  curl -s {base}/runs/$RUN | jq .status   # running, then interrupted");
    println!("  curl -s -X POST {base}/threads/$DEEP/runs/wait \\");
    println!("    -H 'content-type: application/json' \\");
    println!("    -d '{{\"command\": {{\"resume\": {{\"publish\": true}}}}}}' | jq .status\n");

    serve(registry, config).await?;
    Ok(())
}
