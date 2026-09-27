//! Demo server: a two-node pipeline graph, a ReAct agent (deterministic
//! local `ChatModel` by default; `RUSTY_LLM_BASE_URL` + `RUSTY_LLM_MODEL`
//! swap in any OpenAI-compatible endpoint), and a long-running `deep-dive`
//! graph that parks in `interrupted` until resumed, served on
//! `127.0.0.1:8100`.
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
    CalculatorTool, KnowledgeDocument, SandboxedDocumentReaderTool,
    TextInspectorTool,
};
use rusty_agent_server::{
    serve, GovernedKnowledgeSearchTool, GraphRegistry, ServerConfig, ShippedPlugin, ShippedSkillPack,
};
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

/// A chat model that prepends a fixed system prompt to every request, unless
/// the conversation already carries a system message. It is how the served
/// Agent Builder blueprint carries its interview behavior on the server side:
/// the studio only opens a normal chat against the `agent_builder` graph, and
/// the persona lives here, not in any client template.
struct WithSystemPrompt {
    inner: Arc<dyn ChatModel>,
    system: String,
}

#[async_trait]
impl ChatModel for WithSystemPrompt {
    async fn chat(&self, messages: &[ChatMessage], tools: &[Value]) -> Result<ChatResponse> {
        if messages.iter().any(|message| message.role == Role::System) {
            return self.inner.chat(messages, tools).await;
        }
        let mut full = Vec::with_capacity(messages.len() + 1);
        full.push(ChatMessage::system(self.system.clone()));
        full.extend_from_slice(messages);
        self.inner.chat(&full, tools).await
    }
}

/// The Agent Builder persona: it interviews the operator about the agent they
/// want, then emits the finished definition as one fenced `agent` block in the
/// studio's canonical agent-markdown schema, which the builder door parses
/// into the same draft the guided form edits. The schema is deliberately flat
/// so a model emits it reliably.
const BUILDER_SYSTEM_PROMPT: &str = r#"You are the Agent Builder inside Rusty Studio. From a short description you immediately design one complete, ready-to-review agent. You BUILD; you do not interrogate.

How to work:
1. On the person's FIRST message, if it describes an agent at all, do NOT ask questions first. Infer every field from what they said — a concise, specific name, who it serves, what it delivers, and sensible guardrails — and output the finished definition right away as ONE fenced code block whose info string is exactly `agent`. Precede the block with a SINGLE short sentence saying what you built and inviting changes (e.g. "Here's a first draft of a support-triage agent — tell me what to adjust."). Put nothing after the block.
2. Only ask a question first when the message is genuinely too vague to build from (a single word, or no task at all) — then ask at most one short question.
3. Whenever the person asks for a change, re-emit the WHOLE updated block with the change applied, preceded by one short sentence. The definition is the deliverable; keep prose to a line.

The block MUST be YAML front matter between two `---` lines, then the agent's responsibility as the markdown body. Use only these keys:

```agent
---
name: <short name>
behavior: react_agent
model:                      # optional; leave blank for the deployment default
audience: <who it serves>
memory: none                # one of: none, read_only, read_write
scopes: []                  # required only when memory is not none; any of: run, agent, user, team, tenant
approval: runtime_policy    # one of: runtime_policy, irreversible, external_effect
output: text                # one of: runtime_default, text, json_object, json_schema
schema:                     # a named schema id, only when output is json_schema
max_steps:                  # optional integer 1..100000
tools: []                   # optional list of "<tool_name> | <effect>" where effect is pure|read_only|idempotent|compensatable|non_idempotent
goals: []                   # optional list, e.g. "Task success rate >= 90 %"
---
One or two sentences describing exactly what this agent is responsible for.
```

Rules: pick `behavior: react_agent` for a conversational agent unless the person clearly needs something else. Never invent tools the person did not ask for — an empty list is fine. Never put credentials, URLs, or secrets in `model`. Keep the responsibility body concrete."#;

/// The served Agent Builder graph: an ordinary ReAct agent over the configured
/// chat model, with no tools (it only converses) and the builder persona
/// prepended by [`WithSystemPrompt`].
fn build_agent_builder_graph() -> Result<(Graph, StateSpec)> {
    let (model, _label) = chat_model();
    let persona = Arc::new(WithSystemPrompt {
        inner: model,
        system: BUILDER_SYSTEM_PROMPT.to_owned(),
    });
    let graph = create_react_agent(persona, ToolRegistry::new())?;
    let spec = StateSpec::new().channel("messages", Reducer::AddMessages);
    Ok((graph, spec))
}

/// The Tool Builder persona: from a short description it drafts one complete
/// tool definition — a name, the least-powerful effect boundary that fits, and
/// a JSON-Schema parameters block — emitted as one fenced `tool` block the
/// studio parses. Like the Agent Builder, the persona lives here, not in a
/// client template.
const TOOL_BUILDER_PROMPT: &str = r#"You are the Tool Builder inside Rusty Studio. From a short description you immediately design one complete, ready-to-review tool definition. You BUILD; you do not interrogate.

How to work:
1. On the person's FIRST message, if it describes a tool at all, do NOT ask questions first. Infer a concise snake_case name, the effect boundary, and a precise JSON-Schema for the arguments, and output the finished definition right away as ONE fenced code block whose info string is exactly `tool`. Precede it with a SINGLE short sentence saying what you built. Put nothing after the block.
2. Only ask a question first when the message is genuinely too vague to build from.
3. Whenever the person asks for a change, re-emit the WHOLE updated block with the change applied, preceded by one short sentence.

The block MUST be YAML front matter between two `---` lines, then the tool's JSON-Schema arguments as the body. Use only these keys:

```tool
---
name: <snake_case, e.g. create_ticket>
effect: read_only          # one of: pure, read_only, idempotent, compensatable, non_idempotent
description: <one concrete sentence: what the tool does>
---
{
  "type": "object",
  "properties": { "<arg>": { "type": "string", "description": "..." } },
  "required": ["<arg>"]
}
```

Rules: choose the LEAST-powerful effect that fits — `pure`/`read_only` for a read, `idempotent`/`compensatable`/`non_idempotent` for a write. The body MUST be a single valid JSON Schema object describing the tool's arguments. Never invent credentials, URLs, or secrets. Keep the description concrete."#;

/// The served Tool Builder graph: an ordinary ReAct agent over the configured
/// chat model with no tools (it only converses), the tool-builder persona
/// prepended by [`WithSystemPrompt`].
fn build_tool_builder_graph() -> Result<(Graph, StateSpec)> {
    let (model, _label) = chat_model();
    let persona = Arc::new(WithSystemPrompt {
        inner: model,
        system: TOOL_BUILDER_PROMPT.to_owned(),
    });
    let graph = create_react_agent(persona, ToolRegistry::new())?;
    let spec = StateSpec::new().channel("messages", Reducer::AddMessages);
    Ok((graph, spec))
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

/// The chat model behind `react_agent`: the deterministic local harness by
/// default (credential-free), or any OpenAI-compatible endpoint when the
/// operator points `RUSTY_LLM_BASE_URL` + `RUSTY_LLM_MODEL` at one.
/// `RUSTY_LLM_API_KEY` is optional (a local vLLM box needs none), and
/// `RUSTY_LLM_EXTRA_BODY` — a JSON object merged into every request — is
/// the seam for provider extensions, e.g. Qwen3's no-think toggle
/// (`{"chat_template_kwargs": {"enable_thinking": false}}`).
/// The one handle every graph and the verifier hold, filled when the
/// react graph is built.
static MODEL_HANDLE: std::sync::OnceLock<std::sync::Arc<rusty_agent_runtime::llm::SwappableChatModel>> = std::sync::OnceLock::new();

fn chat_model() -> (Arc<dyn ChatModel>, String) {
    let (Ok(base_url), Ok(model)) = (
        std::env::var("RUSTY_LLM_BASE_URL"),
        std::env::var("RUSTY_LLM_MODEL"),
    ) else {
        return (Arc::new(HarnessDemoModel), "local harness model (deterministic, no network)".to_owned());
    };
    let label = format!("{model} via {base_url}");
    let mut client = OpenAiCompatibleClient::new(&base_url, std::env::var("RUSTY_LLM_API_KEY").ok(), model);
    if let Ok(raw) = std::env::var("RUSTY_LLM_EXTRA_BODY") {
        let extra: serde_json::Map<String, Value> = serde_json::from_str(&raw)
            .expect("RUSTY_LLM_EXTRA_BODY must be a JSON object");
        client = client.with_extra_body(extra);
    }
    // What the model costs, per million tokens in and out — operator
    // configuration; with it every journaled model call carries cost_usd
    // and a cost budget on an agent has something to judge. Without it the
    // model is unpriced: tokens are metered, cost is null, never zero.
    //   RUSTY_LLM_PRICE_INPUT_PER_M=0.20 RUSTY_LLM_PRICE_OUTPUT_PER_M=0.60
    if let (Ok(input), Ok(output)) = (
        std::env::var("RUSTY_LLM_PRICE_INPUT_PER_M"),
        std::env::var("RUSTY_LLM_PRICE_OUTPUT_PER_M"),
    ) {
        match (input.trim().parse::<f64>(), output.trim().parse::<f64>()) {
            (Ok(input), Ok(output)) if input >= 0.0 && output >= 0.0 => {
                client = client.with_pricing(rusty_agent_runtime::llm::ModelPricing::new(input, output));
                tracing::info!(input_per_million = input, output_per_million = output, "model priced");
            }
            _ => tracing::warn!("RUSTY_LLM_PRICE_INPUT_PER_M / RUSTY_LLM_PRICE_OUTPUT_PER_M are not numbers; the model stays unpriced"),
        }
    }
    (Arc::new(client), label)
}

/// ReAct agent over the configured chat model and a safe capability pack.
fn build_react_graph(
    connection_tools: std::sync::Arc<rusty_agent_server::ConnectionTools>,
    knowledge_tool: std::sync::Arc<GovernedKnowledgeSearchTool>,
    mcp_tools: std::sync::Arc<rusty_agent_server::McpTools>,
    platform_tools: std::sync::Arc<rusty_agent_server::PlatformTools>,
) -> Result<(Graph, StateSpec, ToolRegistry, String)> {
    let mut tools = ToolRegistry::new();
    tools.register(Echo);
    tools.register(CalculatorTool);
    tools.register(TextInspectorTool);
    tools.register(SandboxedDocumentReaderTool::new(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/demo_documents"),
    )?);
    // The governed knowledge plane, bound at boot; these two documents are
    // what the tool serves until then.
    tools.register_shared(knowledge_tool);
    // Every connection a builder configures becomes tools on this agent —
    // from the store, live, not from anything in this process's environment.
    tools.attach(connection_tools);
    // Every MCP server an administrator registers becomes tools here too.
    tools.attach(mcp_tools);
    // The platform's own doors — the Composer builds agents through them.
    tools.attach(platform_tools);

    let (model, model_label) = chat_model();
    // The graph holds its model behind a handle the providers configuration
    // swaps; the boot model is what the environment says until then.
    let handle = std::sync::Arc::new(rusty_agent_runtime::llm::SwappableChatModel::new(model, model_label.clone()));
    MODEL_HANDLE.get_or_init(|| std::sync::Arc::clone(&handle));
    let model: Arc<dyn ChatModel> = handle;
    let graph = create_react_agent(model, tools.clone())?;
    // The conversation, and the compaction summary the run keeps between
    // steps (revised with the turns since, never re-summarised whole).
    let spec = StateSpec::new()
        .channel("messages", Reducer::AddMessages)
        .channel(rusty_agent_runtime::react::COMPACTION_CHANNEL, Reducer::Overwrite);
    Ok((graph, spec, tools, model_label))
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
                            "username": {"type": "string", "title": "Username", "rusty_secret": true, "rusty_order": 0},
                            "password": {"type": "string", "title": "Password", "rusty_secret": true, "rusty_order": 1}
                        }
                    },
                    {
                        "title": "OAuth token",
                        "type": "object",
                        "required": ["auth", "token"],
                        "additionalProperties": false,
                        "properties": {
                            "auth": {"type": "string", "const": "oauth"},
                            "token": {"type": "string", "title": "Access token", "rusty_secret": true, "rusty_order": 0}
                        }
                    },
                    {
                        "title": "OAuth (password grant)",
                        "type": "object",
                        "required": ["auth", "client_id", "client_secret", "username", "password"],
                        "additionalProperties": false,
                        "properties": {
                            "auth": {"type": "string", "const": "oauth_password"},
                            "client_id": {"type": "string", "title": "OAuth client ID", "rusty_secret": true, "rusty_order": 0},
                            "client_secret": {"type": "string", "title": "OAuth client secret", "rusty_secret": true, "rusty_order": 1},
                            "username": {"type": "string", "title": "Service account", "rusty_order": 2},
                            "password": {"type": "string", "title": "Service account password", "rusty_secret": true, "rusty_order": 3}
                        }
                    },
                    {
                        "title": "OAuth (client credentials)",
                        "type": "object",
                        "required": ["auth", "client_id", "client_secret"],
                        "additionalProperties": false,
                        "properties": {
                            "auth": {"type": "string", "const": "oauth_client_credentials"},
                            "client_id": {"type": "string", "title": "OAuth client ID", "rusty_secret": true, "rusty_order": 0},
                            "client_secret": {"type": "string", "title": "OAuth client secret", "rusty_secret": true, "rusty_order": 1}
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
            reconcile: None,
        }
    };
    ConnectorManifest::new(
        "servicenow",
        "5",
        "ServiceNow",
        "ServiceNow: get, list, count and update records in any table, create incidents and records, and read and order from the service catalog.",
        "https://www.servicenow.com/docs/",
        "https://{instance}.service-now.com",
        spec,
        vec![
            op(
                "get-record",
                HttpMethod::Get,
                "/api/now/table/{table}/{sys_id}?sysparm_display_value=true&sysparm_exclude_reference_link=true",
                OperationEffect::ReadOnly,
                json!({
                    "type": "object",
                    "required": ["table", "sys_id"],
                    "properties": {"table": {"type": "string"}, "sys_id": {"type": "string"}}
                }),
                "Get one record from a ServiceNow table by sys_id — the 32-character id, never a human number. An incident number like INC0010093 is not a sys_id: find that record with list-records (sysparm_query=number=INC0010093, sysparm_limit=1).",
            ),
            op(
                "list-records",
                HttpMethod::Get,
                // Display values, so a reference reads as a name and a choice
                // as its label; no reference links, which are noise to a model.
                "/api/now/table/{table}?sysparm_display_value=true&sysparm_exclude_reference_link=true",
                OperationEffect::ReadOnly,
                json!({
                    "type": "object",
                    "required": ["table"],
                    "properties": {
                        "table": {"type": "string", "description": "The table, e.g. incident, change_request, sys_user."},
                        "sysparm_query": {"type": "string", "description": "An encoded query. A record by its human number: number=INC0010093 (with sysparm_limit 1). Open records about a topic: active=true^short_descriptionLIKEbadge reader (LIKE is a contains match; ^OR joins alternatives: ^short_descriptionLIKEvpn^ORshort_descriptionLIKEremote access). An exact short_description= match misses every rewording — search by two or three distinctive words. Time windows use relative operators, e.g. opened_atRELATIVEGT@hour@ago@24 (opened in the last 24 hours) or sys_created_on>2026-09-01 00:00:00; conditions join with ^; ORDERBYDESCopened_at sorts. javascript: expressions are not evaluated over this API."},
                        "sysparm_fields": {"type": "string", "description": "Comma-separated fields to return, e.g. number,priority,short_description,assigned_to,opened_at. Always name the fields you need; a record has ~90."},
                        "sysparm_limit": {"type": "integer", "description": "At most this many records (default 10 000 — always set one)."},
                        "sysparm_offset": {"type": "integer"}
                    }
                }),
                "List records from a ServiceNow table with an encoded query, chosen fields and a limit. Values come back as display values (names, labels).",
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
            )
            .with_read_back(
                "list-records",
                json!({
                    "table": "incident",
                    "sysparm_query": "short_description=$short_description^ORDERBYDESCsys_created_on",
                    "sysparm_fields": "number,sys_id,short_description,opened_at,state",
                    "sysparm_limit": 3
                }),
            ),
            op(
                "create-record",
                HttpMethod::Post,
                "/api/now/table/{table}",
                OperationEffect::Compensatable,
                json!({
                    "type": "object",
                    "required": ["table", "short_description"],
                    "properties": {
                        "table": {"type": "string", "description": "The table the record goes in, e.g. problem, change_request, sc_request, kb_knowledge. Incidents have create-incident."},
                        "short_description": {"type": "string"},
                        "description": {"type": "string"},
                        "urgency": {"type": "string"},
                        "impact": {"type": "string"},
                        "category": {"type": "string"}
                    },
                    "additionalProperties": {"type": "string", "description": "Any other column of the table, by its field name."}
                }),
                "Create a record in any table: the table's name goes in the path, every other argument is a column of the new record. Answers the created record, its number included.",
            ),
            op(
                "aggregate",
                HttpMethod::Get,
                "/api/now/stats/{table}?sysparm_count=true",
                OperationEffect::ReadOnly,
                json!({
                    "type": "object",
                    "required": ["table"],
                    "properties": {
                        "table": {"type": "string", "description": "The table to count, e.g. incident, cmdb_ci, sc_req_item."},
                        "sysparm_query": {"type": "string", "description": "An encoded query the count is over, e.g. active=true^priority=1."},
                        "sysparm_group_by": {"type": "string", "description": "A field to count by, e.g. priority, category, sys_class_name, state — one count per value."}
                    }
                }),
                "Count records in any table, optionally by a field — how many open incidents by priority, how many CIs by class — without reading the rows. The answer is result.stats.count, or one entry per group under result with its groupby_fields and stats.count.",
            ),
            op(
                "list-catalog-items",
                HttpMethod::Get,
                "/api/sn_sc/servicecatalog/items",
                OperationEffect::ReadOnly,
                json!({
                    "type": "object",
                    "properties": {
                        "sysparm_text": {"type": "string", "description": "Words to search the catalog for, e.g. laptop, VPN access, new hire."},
                        "sysparm_limit": {"type": "integer", "description": "At most this many items (set one; 20 is plenty)."}
                    }
                }),
                "What the service catalog offers: the items a person can request, each with its sys_id, name, short description, category and price. Search by words; then order-catalog-item with the sys_id and its variables.",
            ),
            op(
                "order-catalog-item",
                HttpMethod::Post,
                "/api/sn_sc/servicecatalog/items/{sys_id}/order_now",
                OperationEffect::Compensatable,
                json!({
                    "type": "object",
                    "required": ["sys_id", "sysparm_quantity"],
                    "properties": {
                        "sys_id": {"type": "string", "description": "The catalog item's sys_id from list-catalog-items."},
                        "sysparm_quantity": {"type": "string", "description": "How many, as a string, usually \"1\"."},
                        "variables": {"type": "object", "description": "The item's variables by name, as the catalog form asks for them."}
                    }
                }),
                "Order a catalog item for the caller: submits the request and answers the request number (REQ…) and sys_id. A request is a real ticket — the run pauses for a person's approval before this goes out.",
            ),
            op(
                "update-record",
                HttpMethod::Patch,
                "/api/now/table/{table}/{sys_id}",
                OperationEffect::Compensatable,
                json!({
                    "type": "object",
                    "required": ["table", "sys_id"],
                    "properties": {
                        "table": {"type": "string", "description": "The record's table."},
                        "sys_id": {"type": "string", "description": "The record's 32-character sys_id (find it with list-records; a number like INC0010093 is not a sys_id)."},
                        "work_notes": {"type": "string", "description": "A note for the record's work notes — the usual way to add what was found."},
                        "comments": {"type": "string", "description": "A comment the caller sees."}
                    },
                    "additionalProperties": {"type": "string", "description": "Any other column to set, by its field name — state, assignment_group, priority…"}
                }),
                "Change fields on an existing record — add work notes, set a state or an assignment. Changes a real record: the run pauses for a person's approval before this goes out.",
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


/// The connector library this demo ships — the ServiceNow pack built here
/// and every `catalog/*/manifest.json` — through the server config, so it
/// registers at boot with no caller. A manifest without a hash is sealed
/// at boot; one whose JSON does not parse is named and skipped.
fn shipped_connector_packs() -> Vec<ConnectorManifest> {
    let mut packs = vec![servicenow_pack()];
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../catalog");
    let Ok(entries) = std::fs::read_dir(&root) else { return packs };
    for entry in entries.flatten() {
        let manifest_path = entry.path().join("manifest.json");
        let Ok(text) = std::fs::read_to_string(&manifest_path) else { continue };
        match serde_json::from_str::<ConnectorManifest>(&text) {
            Ok(manifest) => packs.push(manifest),
            Err(error) => eprintln!("connector manifest {} skipped: {error}", manifest_path.display()),
        }
    }
    packs
}

/// The context budget, in estimated tokens: `RUSTY_CONTEXT_BUDGET_TOKENS`,
/// default 32,000 — inside every current model's window, and the operator
/// with a smaller one sets it lower.
fn context_budget() -> u32 {
    std::env::var("RUSTY_CONTEXT_BUDGET_TOKENS")
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok())
        .unwrap_or(32_000)
}

/// The skill library: `catalog/skill-sources.json`, the places a builder can
/// import skills from. Missing or malformed reads as an empty library, logged.
fn skill_library() -> Vec<rusty_agent_server::SkillLibrarySource> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../catalog/skill-sources.json");
    let read = std::fs::read_to_string(&path)
        .map_err(|e| e.to_string())
        .and_then(|text| serde_json::from_str(&text).map_err(|e| e.to_string()));
    match read {
        Ok(sources) => sources,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "skill library not loaded");
            Vec::new()
        }
    }
}

/// The plugins under `catalog/plugins/*` — each directory's files by path,
/// `plugin.json` at its root — offered in the library, installed by a person.
fn shipped_plugins() -> Vec<ShippedPlugin> {
    fn walk(base: &std::path::Path, dir: &std::path::Path, out: &mut std::collections::BTreeMap<String, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(base, &path, out);
            } else if let Ok(bytes) = std::fs::read(&path) {
                let rel = path.strip_prefix(base).unwrap_or(&path).to_string_lossy().replace('\\', "/");
                out.insert(rel, bytes);
            }
        }
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../catalog/plugins");
    let Ok(entries) = std::fs::read_dir(&root) else { return Vec::new() };
    let mut packs = Vec::new();
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.join("plugin.json").is_file() {
            continue;
        }
        let mut files = std::collections::BTreeMap::new();
        walk(&dir, &dir, &mut files);
        packs.push(ShippedPlugin { files });
    }
    packs
}

/// The skill packs under `catalog/skills/*` — each one's `SKILL.md` and
/// `references/` members — shipped through the server config so they
/// register at boot with no caller.
fn shipped_skill_packs() -> Vec<ShippedSkillPack> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../catalog/skills");
    let Ok(entries) = std::fs::read_dir(&root) else { return Vec::new() };
    let mut packs = Vec::new();
    for entry in entries.flatten() {
        let pack = entry.path();
        let Ok(skill_md) = std::fs::read(pack.join("SKILL.md")) else { continue };
        let mut files: std::collections::BTreeMap<String, Vec<u8>> = std::collections::BTreeMap::new();
        files.insert("SKILL.md".to_owned(), skill_md);
        let refs_dir = pack.join("references");
        if refs_dir.is_dir() {
            let mut references = std::collections::BTreeMap::new();
            collect_reference_members(&refs_dir, &refs_dir, &mut references);
            for (path, text) in references {
                files.insert(format!("references/{path}"), text.into_bytes());
            }
        }
        packs.push(ShippedSkillPack { files, author: "rusty-demo".to_owned() });
    }
    packs
}

/// Every UTF-8 reference member under a pack's `references/` directory, keyed by
/// its path beneath that directory — the shape `POST /skills` expects.
fn collect_reference_members(
    base: &std::path::Path,
    dir: &std::path::Path,
    out: &mut std::collections::BTreeMap<String, String>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_reference_members(base, &path, out);
        } else if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(relative) = path.strip_prefix(base) {
                out.insert(relative.to_string_lossy().replace('\\', "/"), text);
            }
        }
    }
}

#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let (pipeline, pipeline_spec) = build_pipeline_graph()?;
    let connection_tools = rusty_agent_server::ConnectionTools::new();
    let knowledge_tool = GovernedKnowledgeSearchTool::late_bound(vec![
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
    ])?;
    let mcp_tools = rusty_agent_server::McpTools::new();
    let platform_tools = rusty_agent_server::PlatformTools::new();
    let (react, react_spec, react_tools, react_model) =
        build_react_graph(
            std::sync::Arc::clone(&connection_tools),
            std::sync::Arc::clone(&knowledge_tool),
            std::sync::Arc::clone(&mcp_tools),
            std::sync::Arc::clone(&platform_tools),
        )?;
    let (deep_dive, deep_dive_spec) = build_deep_dive_graph()?;
    let (agent_builder, agent_builder_spec) = build_agent_builder_graph()?;
    let (tool_builder, tool_builder_spec) = build_tool_builder_graph()?;

    let mut registry = GraphRegistry::new();
    registry.register("pipeline", pipeline, pipeline_spec);
    registry.register_with_tools("react_agent", react, react_spec, &react_tools)?;
    registry.register("deep-dive", deep_dive, deep_dive_spec);
    registry.register("agent_builder", agent_builder, agent_builder_spec);
    registry.register("tool_builder", tool_builder, tool_builder_spec);

    // The egress ceiling is one cell: the server enforces it on connections
    // and fetches, the OAuth provider on token exchanges.
    let egress_ceiling = rusty_agent_server::egress_ceiling::SharedCeiling::default();
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
        rusty_agent_server::oauth::ReqwestOAuthProvider::under(Arc::clone(&egress_ceiling)),
    ))
    .with_egress_ceiling(Arc::clone(&egress_ceiling))
    .with_connection_tools(connection_tools)
    .with_knowledge_tool(knowledge_tool)
    // MCP servers an administrator registers are processes this server
    // spawns; the operator says whether that is allowed here.
    .with_mcp_tools(mcp_tools)
    // The Composer's doors; the server seeds the Composer at boot.
    .with_platform_tools(platform_tools)
    .with_mcp_stdio(std::env::var("RUSTY_MCP_STDIO").map(|v| v == "1").unwrap_or(false))
    .with_restore_from_opt(std::env::var("RUSTY_RESTORE_FROM").ok().filter(|v| !v.is_empty()))
    .with_backup_dir_opt(std::env::var("RUSTY_BACKUP_DIR").ok().filter(|v| !v.is_empty()))
    // A product has people. The first boot creates the administrator and
    // hands its password over in a file next to the store. RUSTY_OPEN=1 is
    // the explicit way to run without sign-in — a laptop, a test harness —
    // and a production boot refuses it regardless.
    .with_bootstrap_admin(std::env::var("RUSTY_OPEN").map(|v| v != "1").unwrap_or(true))
    // Where a builder can import skills from. Pointers, not content —
    // nothing is fetched until someone asks.
    .with_skill_library(skill_library())
    .with_connector_packs(shipped_connector_packs())
    // How long agents.ask waits for the asked agent (its run, and a
    // person's decision when it pauses): RUSTY_ASK_WAIT_SECS, 150 by default.
    .with_ask_wait(std::time::Duration::from_secs(
        std::env::var("RUSTY_ASK_WAIT_SECS").ok().and_then(|s| s.trim().parse::<u64>().ok()).filter(|n| (5..=3600).contains(n)).unwrap_or(150),
    ))
    // A nightly sweep of every suite: RUSTY_SWEEP_AT=HH:MM (UTC). Unset, the
    // sweep is the button on Evals.
    .with_sweep_at_opt(std::env::var("RUSTY_SWEEP_AT").ok().and_then(|t| {
        let (h, m) = t.trim().split_once(':')?;
        Some((h.parse::<u32>().ok()?, m.parse::<u32>().ok()?))
    }))
    .with_skill_packs(shipped_skill_packs())
    .with_plugin_packs(shipped_plugins());
    // Every model call is assembled: the charter pinned, the history
    // compacted, the tools budgeted. RUSTY_CONTEXT_BUDGET_TOKENS sizes it to
    // the model's window (estimated tokens); 0 turns assembly off.
    let config = match context_budget() {
        0 => config,
        // …and every run reads what it remembers — about the person it is
        // for, and what the agent itself has learned — into a # Memory
        // section, at an eighth of the budget.
        budget => config.with_context_policy(
            rusty_agent_runtime::context::ContextPolicy::standard(budget)
                .with_memory_section((budget / 8).clamp(512, 4_096))
                // …and lane-one recall: the five notes that bear most on the
                // turn's message, after the history, zero model calls.
                .with_recall(1_200, 5),
        ),
    };
    // A run ends when the model stops; whether it achieved the outcome is
    // asked of a judge — the same model — unless RUSTY_VERIFY_OUTCOMES=0.
    let config = if std::env::var("RUSTY_VERIFY_OUTCOMES").map(|v| v == "0").unwrap_or(false) {
        config
    } else {
        config.with_verifier(match MODEL_HANDLE.get() {
            Some(handle) => std::sync::Arc::clone(handle) as Arc<dyn ChatModel>,
            None => chat_model().0,
        })
    };
    // Hosts the operator allows beyond the connections that exist:
    //   RUSTY_EGRESS_ALLOW="api.example.com,hooks.example.net"
    // Egress is on regardless — a host with no policy is denied, and private
    // addresses are refused at preflight — so this only ever widens.
    // The providers a person configures swap what is behind the handle.
    let config = match MODEL_HANDLE.get() {
        Some(handle) => config.with_model_handle(std::sync::Arc::clone(handle)),
        None => config,
    };
    let config = match std::env::var("RUSTY_EGRESS_ALLOW") {
        Ok(list) if !list.trim().is_empty() => {
            use rusty_agent_runtime::egress::*;
            let policies = list
                .split(',')
                .map(str::trim)
                .filter(|h| !h.is_empty())
                .map(|host| EgressEndpointPolicy {
                    name: format!("operator:{host}"),
                    endpoint: EgressEndpoint {
                        host: host.to_ascii_lowercase(),
                        port: 443,
                        protocol: EgressProtocol::Rest,
                        tls: true,
                        rewrite: EgressRewrite::default(),
                        allowed_ips: Vec::new(),
                        allow_encoded_slashes: false,
                    },
                    rules: vec![EgressRule {
                        methods: Vec::new(),
                        path_pattern: "/**".to_owned(),
                        mode: EgressRuleMode::Enforce,
                        tool_names: None,
                    }],
                    originating: Vec::new(),
                })
                .collect();
            config.with_egress_policy(EgressPolicy { policies })
        }
        _ => config,
    };
    // Named principals, when the deployment declares them:
    //   RUSTY_PRINCIPALS="amjad:user:admin:KEY1;nightly-kb:service:operator+builder:KEY2"
    // Absent, the server runs open — every caller is the developer principal —
    // which is what a laptop wants and what production refuses to boot as.
    let config = match std::env::var("RUSTY_PRINCIPALS") {
        Ok(spec) if !spec.trim().is_empty() => {
            let mut config = config;
            for entry in spec.split(';').map(str::trim).filter(|e| !e.is_empty()) {
                let parts: Vec<&str> = entry.splitn(4, ':').collect();
                let [id, kind, roles, key] = parts.as_slice() else {
                    panic!("RUSTY_PRINCIPALS entry `{entry}` is not id:kind:roles:key");
                };
                let kind = match *kind {
                    "user" => rusty_agent_server::PrincipalKind::User,
                    "service" => rusty_agent_server::PrincipalKind::Service,
                    other => panic!("RUSTY_PRINCIPALS: unknown kind `{other}` (user|service)"),
                };
                let roles: Vec<_> = roles
                    .split('+')
                    .map(|r| rusty_agent_server::Role::parse(r).unwrap_or_else(|| panic!("RUSTY_PRINCIPALS: unknown role `{r}`")))
                    .collect();
                config = config.with_principal(
                    "default",
                    rusty_agent_server::Principal { id: id.to_string(), name: id.to_string(), kind, roles },
                    key.to_string(),
                );
            }
            config
        }
        _ => config,
    };

    // The menu below is printed with the *actual* address so the test-hook
    // override stays honest when a human runs the demo with it set.
    let base = format!("localhost:{}", config.bind_addr.port());
    println!("\nrusty-server demo on http://{base}");
    println!("  react_agent model: {react_model}");
    if config.bootstrap_admin {
        println!(
            "\n  first boot seeds an administrator — sign in before the curl menu below:"
        );
        println!("    password file: <store>/bootstrap-admin.txt (next to the checkpoint store)");
        println!("  curl -s -c /tmp/rusty-cookies.txt -X POST http://{base}/auth/login \\");
        println!("    -H 'content-type: application/json' \\");
        println!("    -d '{{\"username\": \"admin\", \"password\": \"<from the file>\"}}'");
        println!("  then pass -b /tmp/rusty-cookies.txt with every request.\n");
    }
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
    println!("  # ReAct agent (model named above; RUSTY_LLM_BASE_URL + RUSTY_LLM_MODEL");
    println!("  # point it at any OpenAI-compatible endpoint)");
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
