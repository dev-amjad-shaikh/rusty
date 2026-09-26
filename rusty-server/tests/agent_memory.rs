//! An agent remembers, per person. On one thread a person tells the agent
//! a preference; the agent calls `memory.remember`, which writes to that
//! person's memory and journals the write on the run. On a new thread the
//! same person's run carries the memory into the model's first call under
//! `# Memory`; another person's run carries nothing of it. The deployment's
//! policy has a memory section; the run's memory source narrows the
//! tenant's store to the person and the agent.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use rusty_agent_runtime::context::ContextPolicy;
use rusty_agent_runtime::llm::Role as ChatRole;
use rusty_agent_runtime::prelude::*;
use rusty_agent_runtime::tool::ToolSource;
use rusty_agent_server::{
    GraphRegistry, PlatformTools, Principal, PrincipalKind, Role as AuthRole, ServerConfig, router,
};
use serde_json::{Value, json};
use tower::ServiceExt;

/// Calls `memory.remember` when the person states a preference and no
/// memory shows yet; otherwise answers from what it sees. Keeps every
/// message list it was handed.
struct RememberingModel {
    seen: Arc<Mutex<Vec<Vec<ChatMessage>>>>,
}

#[async_trait]
impl ChatModel for RememberingModel {
    async fn chat(&self, messages: &[ChatMessage], _tools: &[Value]) -> Result<ChatResponse> {
        self.seen.lock().unwrap().push(messages.to_vec());
        let memory = messages
            .iter()
            .find(|m| {
                m.role == ChatRole::System
                    && m.content
                        .as_deref()
                        .is_some_and(|c| c.starts_with("# Memory"))
            })
            .and_then(|m| m.content.clone());
        let last_user = messages
            .iter()
            .rev()
            .find(|m| m.role == ChatRole::User)
            .and_then(|m| m.content.clone())
            .unwrap_or_default();
        let already_remembered = messages.iter().any(|m| m.role == ChatRole::Tool);
        let tool_said = messages
            .iter()
            .rev()
            .find(|m| m.role == ChatRole::Tool)
            .and_then(|m| m.content.clone())
            .unwrap_or_default();
        let message = if let Some((key, text)) = last_user
            .strip_prefix("keep ")
            .and_then(|rest| rest.split_once('|'))
        {
            if already_remembered {
                ChatMessage::assistant(format!("the tool said: {tool_said}"))
            } else {
                ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                    "k1",
                    "memory.remember",
                    json!({"text": text, "key": key}),
                )])
            }
        } else if let Some(line) = last_user.strip_prefix("block: ") {
            if already_remembered {
                ChatMessage::assistant(format!("the tool said: {tool_said}"))
            } else {
                ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                    "b1",
                    "memory.block_edit",
                    json!({"label": "decisions", "action": "add", "content": line}),
                )])
            }
        } else if last_user.contains("prefer") && !already_remembered {
            ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                "c1",
                "memory.remember",
                json!({"text": "I prefer metric units", "kind": "preference", "key": "units"}),
            )])
        } else if let Some(memory) = memory {
            ChatMessage::assistant(format!(
                "I remember: {}",
                memory.lines().skip(1).collect::<Vec<_>>().join(" | ")
            ))
        } else {
            ChatMessage::assistant("I remember nothing about you yet.")
        };
        Ok(ChatResponse {
            message,
            model: Some("remembering-test".into()),
            usage: None,
        })
    }
}

fn person(id: &str) -> Principal {
    Principal {
        id: id.to_owned(),
        name: id.to_owned(),
        kind: PrincipalKind::User,
        roles: vec![AuthRole::Builder],
    }
}

fn test_app() -> (Router, PathBuf, Arc<Mutex<Vec<Vec<ChatMessage>>>>) {
    let store = std::env::temp_dir().join(format!("rusty-agent-memory-{}", uuid::Uuid::new_v4()));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let platform_tools = PlatformTools::new();
    let mut tools = ToolRegistry::new();
    tools.attach(Arc::clone(&platform_tools) as Arc<dyn ToolSource>);
    let graph = create_react_agent(
        Arc::new(RememberingModel {
            seen: Arc::clone(&seen),
        }),
        tools.clone(),
    )
    .unwrap();
    let spec = StateSpec::new().channel("messages", Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry
        .register_with_tools("react_agent", graph, spec, &tools)
        .unwrap();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_platform_tools(platform_tools)
        .with_context_policy(ContextPolicy::standard(8192).with_memory_section(512))
        .with_principal("default", person("bob"), "bob-key")
        .with_principal("default", person("cy"), "cy-key");
    (router(registry, config), store, seen)
}

async fn call(
    app: &Router,
    key: &str,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("X-Api-Key", key);
    let body = match body {
        Some(v) => {
            builder = builder.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

async fn run(app: &Router, key: &str, text: &str) -> Value {
    let (status, thread) = call(
        app,
        key,
        "POST",
        "/threads",
        Some(json!({"graph": "react_agent"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{thread}");
    let thread_id = thread["thread_id"].as_str().unwrap();
    let (status, terminal) = call(
        app,
        key,
        "POST",
        &format!("/threads/{thread_id}/runs/wait"),
        Some(json!({"input": {"messages": [{"role": "user", "content": text}]}, "assistant_id": "recall"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{terminal}");
    assert_eq!(terminal["status"], "success", "{terminal}");
    terminal
}

/// A run a schedule fired, through the real scheduler: a cron on the agent
/// with the text as its standing input, the first firing awaited, the cron
/// removed. The run's record is returned, the shape `GET /runs/{id}` gives.
async fn fire_scheduled(app: &Router, key: &str, text: &str) -> Value {
    let (status, cron) = call(
        app,
        key,
        "POST",
        "/crons",
        Some(json!({
            "assistant_id": "recall", "interval_secs": 1,
            "input": {"messages": [{"role": "user", "content": text}]},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{cron}");
    let cron_id = cron["cron_id"].as_str().unwrap().to_owned();
    let mut fired: Option<Value> = None;
    for _ in 0..80 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let (_, runs) = call(app, key, "GET", "/runs", None).await;
        let hit = runs
            .as_array()
            .into_iter()
            .flatten()
            .find(|r| r["metadata"]["cron_id"] == json!(cron_id) && r["status"] == json!("success"))
            .cloned();
        if hit.is_some() {
            fired = hit;
            break;
        }
    }
    let _ = call(app, key, "DELETE", &format!("/crons/{cron_id}"), None).await;
    let fired = fired.unwrap_or_else(|| panic!("the schedule never fired a finished run"));
    let (_, run) = call(
        app,
        key,
        "GET",
        &format!("/runs/{}", fired["run_id"].as_str().unwrap()),
        None,
    )
    .await;
    run
}

fn last_assistant(terminal: &Value) -> String {
    terminal["output"]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|m| m["role"] == "assistant")
        .and_then(|m| m["content"].as_str())
        .unwrap_or("")
        .to_owned()
}

#[tokio::test]
async fn an_agent_remembers_for_the_person_and_only_for_them() {
    let (app, store, seen) = test_app();
    // The agent, named so the run is of it; memory.remember is one of its tools.
    let (status, body) = call(&app, "bob-key", "POST", "/assistants", Some(json!({
        "assistant_id": "recall", "name": "Recall", "graph": "react_agent",
        "config": {"studio_intent": {"instructions": "Remember what people tell you about themselves.", "tools": [{"name": "memory.remember"}]}},
    }))).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // Bob states a preference: the agent remembers it, for Bob.
    let first = run(&app, "bob-key", "I prefer metric units, please.").await;
    let tool_result = first["output"]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .expect("the tool ran");
    let said = tool_result["content"].as_str().unwrap();
    assert!(said.contains("user:bob"), "{said}");
    let run_id = first["run_id"].as_str().unwrap();
    let (_, events) = call(
        &app,
        "bob-key",
        "GET",
        &format!("/runs/{run_id}/events"),
        None,
    )
    .await;
    let kinds: Vec<&str> = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["kind"].as_str())
        .collect();
    assert!(kinds.contains(&"memory_write"), "{kinds:?}");
    assert!(kinds.contains(&"memory_read"), "{kinds:?}");
    // A note about a person is proposed to them, not kept outright: it is
    // in Bob's memory as a pending candidate the agent wrote, held back
    // from recall until Bob accepts it.
    assert!(said.contains("waits_for"), "{said}");
    let (status, mine) = call(
        &app,
        "bob-key",
        "POST",
        "/memory/query",
        Some(json!({"scope": {"scope": "user", "id": "bob"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{mine}");
    assert!(
        mine["records"].as_array().unwrap().is_empty(),
        "a proposed note is not recalled: {mine}"
    );
    let (_, waiting) = call(
        &app,
        "bob-key",
        "POST",
        "/memory/query",
        Some(json!({"scope": {"scope": "user", "id": "bob"}, "candidates_only": true})),
    )
    .await;
    let records = waiting["records"].as_array().unwrap();
    assert_eq!(records.len(), 1, "{waiting}");
    assert_eq!(
        records[0]["provenance"]["author"]["agent_id"], "recall",
        "{waiting}"
    );
    assert_eq!(records[0]["kind"], "preference");
    assert_eq!(records[0]["candidacy"], "pending");
    let proposal = records[0]["memory_id"].as_str().unwrap().to_owned();

    // Saying it again stores nothing twice: the tool answers with the same id.
    let again = run(&app, "bob-key", "I prefer metric units, as I said.").await;
    let tool_result = again["output"]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .expect("the tool ran again");
    let said = tool_result["content"].as_str().unwrap();
    assert!(
        said.contains("\"new\":false") || said.contains("\"new\": false"),
        "{said}"
    );
    assert!(said.contains("already remembered"), "{said}");
    let (_, waiting) = call(
        &app,
        "bob-key",
        "POST",
        "/memory/query",
        Some(json!({"scope": {"scope": "user", "id": "bob"}, "candidates_only": true})),
    )
    .await;
    assert_eq!(
        waiting["records"].as_array().unwrap().len(),
        1,
        "still one proposal: {waiting}"
    );

    // Before Bob accepts, a new thread carries nothing of it.
    let early = run(&app, "bob-key", "What do you know about me?").await;
    assert_eq!(
        last_assistant(&early),
        "I remember nothing about you yet.",
        "{early}"
    );

    // Only Bob accepts a note about Bob.
    let (status, refused) = call(
        &app,
        "cy-key",
        "POST",
        &format!("/memory/{proposal}/accept"),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "Cy cannot see Bob's note: {refused}"
    );
    let (status, accepted) = call(
        &app,
        "bob-key",
        "POST",
        &format!("/memory/{proposal}/accept"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    assert_eq!(accepted["proposal"], json!(proposal));
    assert_eq!(accepted["proposed_by"], json!("agent:recall"));
    assert_eq!(
        accepted["record"]["provenance"]["author"]["human_id"],
        json!("bob"),
        "{accepted}"
    );
    assert_eq!(accepted["record"]["supersedes"], json!(proposal));
    let (status, twice) = call(
        &app,
        "bob-key",
        "POST",
        &format!("/memory/{proposal}/accept"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "accepted once: {twice}");
    let (_, mine) = call(
        &app,
        "bob-key",
        "POST",
        "/memory/query",
        Some(json!({"scope": {"scope": "user", "id": "bob"}})),
    )
    .await;
    assert_eq!(
        mine["records"].as_array().unwrap().len(),
        1,
        "the accepted note is the one kept: {mine}"
    );
    assert_eq!(
        mine["records"][0]["provenance"]["author"]["human_id"],
        json!("bob")
    );

    // A new thread, the same person: the model's first call carries it.
    let second = run(&app, "bob-key", "What do you know about me?").await;
    assert!(last_assistant(&second).contains("metric"), "{second}");
    {
        let calls = seen.lock().unwrap();
        let last_call = calls.last().unwrap();
        let memory = last_call
            .iter()
            .find(|m| {
                m.role == ChatRole::System
                    && m.content
                        .as_deref()
                        .is_some_and(|c| c.starts_with("# Memory"))
            })
            .expect("a # Memory section");
        assert!(
            memory
                .content
                .as_deref()
                .unwrap()
                .contains("I prefer metric units"),
            "{:?}",
            memory.content
        );
    }

    // Another person, a new thread: nothing of Bob's.
    let other = run(&app, "cy-key", "What do you know about me?").await;
    assert_eq!(
        last_assistant(&other),
        "I remember nothing about you yet.",
        "{other}"
    );
    let calls = seen.lock().unwrap();
    let cy_call = calls.last().unwrap();
    assert!(
        !cy_call
            .iter()
            .any(|m| m.content.as_deref().is_some_and(|c| c.contains("metric"))),
        "Bob's memory reached Cy's run"
    );
    let _ = std::fs::remove_dir_all(store);
}

/// A proposed note the person declines is forgotten: the forget plans over
/// the whole namespace, proposals included, so a note nobody accepted
/// cannot linger unseen.
#[tokio::test]
async fn a_declined_proposal_is_forgotten() {
    let (app, store, _seen) = test_app();
    let (status, body) = call(&app, "bob-key", "POST", "/assistants", Some(json!({
        "assistant_id": "recall", "name": "Recall", "graph": "react_agent",
        "config": {"studio_intent": {"instructions": "Remember what people tell you about themselves.", "tools": [{"name": "memory.remember"}]}},
    }))).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let _ = run(&app, "bob-key", "I prefer metric units, please.").await;
    let (_, waiting) = call(
        &app,
        "bob-key",
        "POST",
        "/memory/query",
        Some(json!({"scope": {"scope": "user", "id": "bob"}, "candidates_only": true})),
    )
    .await;
    let proposal = waiting["records"][0]["memory_id"]
        .as_str()
        .expect("a proposal waits")
        .to_owned();
    let (status, forgotten) = call(
        &app,
        "bob-key",
        "POST",
        "/memory/forget",
        Some(json!({"memory_id": proposal, "reason": "erasure_request"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{forgotten}");
    assert_eq!(forgotten["forgotten"], json!([proposal]), "{forgotten}");
    let (_, waiting) = call(
        &app,
        "bob-key",
        "POST",
        "/memory/query",
        Some(json!({"scope": {"scope": "user", "id": "bob"}, "candidates_only": true})),
    )
    .await;
    assert!(
        waiting["records"].as_array().unwrap().is_empty(),
        "declined, gone: {waiting}"
    );
    let (status, _) = call(
        &app,
        "bob-key",
        "POST",
        &format!("/memory/{proposal}/accept"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "nothing left to accept");
    let _ = std::fs::remove_dir_all(store);
}

/// A run fired by a schedule remembers for the agent, not for the person
/// who created the schedule — and a person's own thread does not see what
/// the agent learned on its schedule as if it were about them.
#[tokio::test]
async fn a_scheduled_run_remembers_for_the_agent_not_for_the_scheduler() {
    let (app, store, _seen) = test_app();
    let (status, body) = call(&app, "bob-key", "POST", "/assistants", Some(json!({
        "assistant_id": "recall", "name": "Recall", "graph": "react_agent",
        "config": {"studio_intent": {"instructions": "Remember.", "tools": [{"name": "memory.remember"}]}},
    }))).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    // A schedule Bob created fires the run: the server's own admission
    // says so (a client may not say it — see the last test in this file).
    let terminal = fire_scheduled(&app, "bob-key", "I prefer metric units, please.").await;
    let tool_result = terminal["output"]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .expect("the tool ran");
    let said = tool_result["content"].as_str().unwrap();
    assert!(
        said.contains("agent:recall"),
        "a scheduled run remembers for the agent: {said}"
    );
    assert!(!said.contains("user:bob"), "{said}");
    let (status, bobs) = call(
        &app,
        "bob-key",
        "POST",
        "/memory/query",
        Some(json!({"scope": {"scope": "user", "id": "bob"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{bobs}");
    assert!(
        bobs["records"].as_array().unwrap().is_empty(),
        "nothing landed in Bob's own memory: {bobs}"
    );
    let (status, agents) = call(
        &app,
        "bob-key",
        "POST",
        "/memory/query",
        Some(json!({"scope": {"scope": "agent", "id": "recall"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{agents}");
    assert_eq!(agents["records"].as_array().unwrap().len(), 1, "{agents}");
    let _ = std::fs::remove_dir_all(store);
}

/// Remembers under a key when told to, and answers "did I post …?" by
/// asking `memory.recall` for that key, echoing the fact it gets back.
struct RecallingModel;

#[async_trait]
impl ChatModel for RecallingModel {
    async fn chat(&self, messages: &[ChatMessage], _tools: &[Value]) -> Result<ChatResponse> {
        let last_user = messages
            .iter()
            .rev()
            .find(|m| m.role == ChatRole::User)
            .and_then(|m| m.content.clone())
            .unwrap_or_default();
        let tool_result = messages
            .iter()
            .rev()
            .find(|m| m.role == ChatRole::Tool)
            .and_then(|m| m.content.clone());
        let message = match tool_result {
            Some(result) => ChatMessage::assistant(format!("memory says: {result}")),
            None if last_user.starts_with("note") => {
                ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                    "c1",
                    "memory.remember",
                    json!({"text": "posted INC0010093", "kind": "fact", "key": "posted:INC0010093"}),
                )])
            }
            None if last_user.starts_with("which of") => {
                let numbers: Vec<String> = last_user
                    .split_whitespace()
                    .filter(|w| w.starts_with("INC"))
                    .map(|w| {
                        w.trim_end_matches(|c: char| !c.is_ascii_alphanumeric())
                            .to_owned()
                    })
                    .collect();
                ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                    "c3",
                    "memory.recall",
                    json!({"contains": numbers}),
                )])
            }
            None => {
                let number = last_user
                    .split_whitespace()
                    .find(|w| w.starts_with("INC"))
                    .unwrap_or("INC?")
                    .trim_end_matches('?');
                ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                    "c2",
                    "memory.recall",
                    json!({"key": format!("posted:{number}")}),
                )])
            }
        };
        Ok(ChatResponse {
            message,
            model: Some("recalling-test".into()),
            usage: None,
        })
    }
}

fn recalling_app() -> (Router, PathBuf) {
    let store = std::env::temp_dir().join(format!("rusty-agent-memory-{}", uuid::Uuid::new_v4()));
    let platform_tools = PlatformTools::new();
    let mut tools = ToolRegistry::new();
    tools.attach(Arc::clone(&platform_tools) as Arc<dyn ToolSource>);
    let graph = create_react_agent(Arc::new(RecallingModel), tools.clone()).unwrap();
    let spec = StateSpec::new().channel("messages", Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry
        .register_with_tools("react_agent", graph, spec, &tools)
        .unwrap();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_platform_tools(platform_tools)
        .with_context_policy(ContextPolicy::standard(8192).with_memory_section(512))
        .with_principal("default", person("bob"), "bob-key");
    (router(registry, config), store)
}

#[tokio::test]
async fn an_agent_recalls_by_key_what_it_remembered_a_fact_not_a_paragraph() {
    let (app, store) = recalling_app();
    let (status, body) = call(&app, "bob-key", "POST", "/assistants", Some(json!({
        "assistant_id": "recall", "name": "Recall", "graph": "react_agent",
        "config": {"studio_intent": {"instructions": "Remember and recall.", "tools": [{"name": "memory.remember"}, {"name": "memory.recall"}]}},
    }))).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    // Each scheduled run goes through the real scheduler.
    let fire = |text: &'static str| fire_scheduled(&app, "bob-key", text);
    // A scheduled run remembers under a key, for the agent.
    let noted = fire("note that INC0010093 was posted").await;
    assert!(last_assistant(&noted).contains("agent:recall"), "{noted}");
    // The next scheduled run asks by key and gets the fact.
    let asked = fire("did I post INC0010093?").await;
    let said = last_assistant(&asked);
    assert!(said.contains("\"found\":true"), "{said}");
    assert!(said.contains("posted:INC0010093"), "{said}");
    assert!(said.contains("agent:recall"), "{said}");
    // A key never remembered is a plain no.
    let unknown = fire("did I post INC0010099?").await;
    assert!(
        last_assistant(&unknown).contains("\"found\":false"),
        "{unknown}"
    );
    // A whole list in one call: one answer per item, in order.
    let listed = fire("which of INC0010093, INC0010099 did I post?").await;
    let said = last_assistant(&listed);
    assert!(said.contains("\"answers\""), "{said}");
    let start = said
        .find("memory says: ")
        .map(|i| i + "memory says: ".len())
        .unwrap_or(0);
    let parsed: Value = serde_json::from_str(&said[start..]).expect("the answer is JSON");
    assert_eq!(parsed["found"], json!(true));
    assert_eq!(parsed["answers"][0]["found"], json!(true), "{parsed}");
    assert_eq!(parsed["answers"][1]["found"], json!(false), "{parsed}");
    // A person's run with the agent reads the agent's notes too.
    let mine = run(&app, "bob-key", "did I post INC0010093?").await;
    assert!(last_assistant(&mine).contains("\"found\":true"), "{mine}");
    let _ = std::fs::remove_dir_all(store);
}

/// Astra R01: a client's metadata may name nobody. Bob, signed in, starts
/// runs whose metadata names Cy — as `on_behalf_of`, as `created_by`, as a
/// delegation — and every one is refused with the key named; a plain run
/// carries the server's execution block, actor and subject both Bob.
#[tokio::test]
async fn a_run_acts_for_the_person_who_started_it_and_for_nobody_their_metadata_names() {
    let (app, store, _seen) = test_app();
    let (status, body) = call(&app, "bob-key", "POST", "/assistants", Some(json!({
        "assistant_id": "recall", "name": "Recall", "graph": "react_agent",
        "config": {"studio_intent": {"instructions": "Remember.", "tools": [{"name": "memory.remember"}]}},
    }))).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, thread) = call(
        &app,
        "bob-key",
        "POST",
        "/threads",
        Some(json!({"graph": "react_agent"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{thread}");
    let thread_id = thread["thread_id"].as_str().unwrap().to_owned();
    let cy = json!({"principal_id": "cy", "name": "cy", "kind": "user"});
    for (key, value) in [
        ("on_behalf_of", cy.clone()),
        ("created_by", cy.clone()),
        ("acting_for", json!("cy")),
        ("counterpart", json!("cy")),
        (
            "delegation",
            json!({"depth": 1, "from_run": "x", "from_agent": "y"}),
        ),
        (
            "execution",
            json!({"tenant": "default", "actor": cy, "subject": cy, "via": "http"}),
        ),
        ("channel", json!("schedule")),
    ] {
        for uri in [
            format!("/threads/{thread_id}/runs"),
            format!("/threads/{thread_id}/runs/wait"),
        ] {
            let (status, body) = call(
                &app,
                "bob-key",
                "POST",
                &uri,
                Some(json!({"input": {"messages": [{"role": "user", "content": "hi"}]}, "assistant_id": "recall", "metadata": {key: value.clone(), "team": "qa"}})),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{key} via {uri}: {body}");
            let said = body.to_string();
            assert!(said.contains(&format!("`{key}`")), "{key}: {said}");
        }
    }
    // A plain run: the server's word on who, for whom, through which path.
    let (status, terminal) = call(
        &app,
        "bob-key",
        "POST",
        &format!("/threads/{thread_id}/runs/wait"),
        Some(json!({"input": {"messages": [{"role": "user", "content": "remember that I like tea"}]}, "assistant_id": "recall", "metadata": {"team": "qa"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{terminal}");
    let run_id = terminal["run_id"].as_str().unwrap();
    let (_, run) = call(&app, "bob-key", "GET", &format!("/runs/{run_id}"), None).await;
    assert_eq!(
        run["metadata"]["created_by"]["principal_id"],
        json!("bob"),
        "{run}"
    );
    assert_eq!(
        run["metadata"]["execution"]["actor"]["principal_id"],
        json!("bob"),
        "{run}"
    );
    assert_eq!(
        run["metadata"]["execution"]["subject"]["principal_id"],
        json!("bob"),
        "{run}"
    );
    assert_eq!(run["metadata"]["execution"]["via"], json!("http"), "{run}");
    assert_eq!(
        run["metadata"]["execution"]["tenant"],
        json!("default"),
        "{run}"
    );
    assert_eq!(run["metadata"]["team"], json!("qa"), "{run}");
    let _ = std::fs::remove_dir_all(store);
}

/// An agent asked to keep a credential in a block is refused by the tool
/// itself, naming the kind, and the block stays unwritten.
#[tokio::test]
async fn the_block_tool_refuses_a_credential() {
    let (app, store, _seen) = test_app();
    let (status, made) = call(&app, "bob-key", "POST", "/assistants", Some(json!({
        "assistant_id": "desk", "name": "Desk", "graph": "react_agent",
        "config": {"studio_intent": {"instructions": "Help.", "tools": [{"name": "memory.block_edit"}]}}
    }))).await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let (_, thread) = call(
        &app,
        "bob-key",
        "POST",
        "/threads",
        Some(json!({"graph": "react_agent"})),
    )
    .await;
    let thread_id = thread["thread_id"].as_str().unwrap();
    let (status, terminal) = call(&app, "bob-key", "POST", &format!("/threads/{thread_id}/runs/wait"), Some(json!({
        "assistant_id": "desk", "input": {"messages": [{"role": "user", "content": "block: Printer resets use the admin password: Winter2026x"}]}
    }))).await;
    assert_eq!(status, StatusCode::OK, "{terminal}");
    let reply = terminal.to_string();
    assert!(
        reply.contains("not written") && reply.contains("a password"),
        "{reply}"
    );
    let (_, listed) = call(
        &app,
        "bob-key",
        "GET",
        "/assistants/desk/memory/blocks",
        None,
    )
    .await;
    let decisions = listed["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["label"] == "decisions")
        .cloned()
        .unwrap_or(Value::Null);
    assert!(
        !decisions["text"]
            .as_str()
            .unwrap_or("")
            .contains("Winter2026x"),
        "{listed}"
    );
    let _ = std::fs::remove_dir_all(store);
}

/// A block the working copy declares — not yet on the published agent —
/// renders into a run that carries the declaration, its text can be set
/// before publishing (the limit rides with the write), and the listing
/// shows it as the working copy's.
#[tokio::test]
async fn a_working_copy_declares_a_block_before_publishing() {
    let (app, store, seen) = test_app();
    let (status, made) = call(&app, "bob-key", "POST", "/assistants", Some(json!({
        "assistant_id": "desk", "name": "Desk", "graph": "react_agent",
        "config": {"studio_intent": {"instructions": "Help.", "tools": [{"name": "memory.block_edit"}]}}
    }))).await;
    assert_eq!(status, StatusCode::CREATED, "{made}");

    // The published agent declares no `runbook` block: a write without its limit is refused.
    let (status, refused) = call(
        &app,
        "bob-key",
        "PUT",
        "/assistants/desk/memory/blocks/runbook",
        Some(json!({"text": "Reboot first."})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{refused}");
    // The working copy's declaration rides with the write.
    let (status, written) = call(
        &app,
        "bob-key",
        "PUT",
        "/assistants/desk/memory/blocks/runbook",
        Some(json!({"text": "Reboot first.\nThen check the cable.", "char_limit": 300})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{written}");
    assert_eq!(written["limit"], 300);
    let (status, too_long) = call(
        &app,
        "bob-key",
        "PUT",
        "/assistants/desk/memory/blocks/runbook",
        Some(json!({"text": "x".repeat(301), "char_limit": 300})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{too_long}");
    // A block carrying a credential is refused, naming the kind, never the value.
    let (status, secret) = call(
        &app,
        "bob-key",
        "PUT",
        "/assistants/desk/memory/blocks/runbook",
        Some(json!({"text": "Reboot first.\nThe admin password: Winter2026x", "char_limit": 300})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{secret}");
    assert!(
        secret.to_string().contains("a password") && !secret.to_string().contains("Winter2026x"),
        "{secret}"
    );

    // Every version the block has had, newest first, each with its author.
    let (status, again) = call(&app, "bob-key", "PUT", "/assistants/desk/memory/blocks/runbook", Some(json!({"text": "Reboot first.\nThen check the cable.\nThen call the desk.", "char_limit": 300}))).await;
    assert_eq!(status, StatusCode::OK, "{again}");
    let (status, history) = call(
        &app,
        "bob-key",
        "GET",
        "/assistants/desk/memory/blocks/runbook/history",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{history}");
    let versions = history["versions"].as_array().unwrap();
    assert_eq!(versions.len(), 2, "{history}");
    assert_eq!(
        versions[0]["text"],
        "Reboot first.\nThen check the cable.\nThen call the desk."
    );
    assert_eq!(versions[0]["current"], true);
    assert_eq!(versions[0]["lines"], 3);
    assert_eq!(versions[1]["text"], "Reboot first.\nThen check the cable.");
    assert_eq!(versions[1]["current"], false);
    assert_eq!(
        versions[0]["supersedes"], versions[1]["version"],
        "the newer supersedes the older"
    );
    assert_eq!(versions[0]["author"], "human:bob");

    // The listing shows it as the working copy's, beside the platform's three.
    let (_, listed) = call(
        &app,
        "bob-key",
        "GET",
        "/assistants/desk/memory/blocks",
        None,
    )
    .await;
    let runbook = listed["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["label"] == "runbook")
        .cloned()
        .unwrap_or(Value::Null);
    assert_eq!(runbook["declared"], false, "{listed}");
    assert_eq!(
        runbook["text"],
        "Reboot first.\nThen check the cable.\nThen call the desk."
    );

    // A run that carries the declaration renders the block; one that does not, does not.
    let (_, thread) = call(
        &app,
        "bob-key",
        "POST",
        "/threads",
        Some(json!({"graph": "react_agent"})),
    )
    .await;
    let thread_id = thread["thread_id"].as_str().unwrap().to_owned();
    let (status, run) = call(&app, "bob-key", "POST", &format!("/threads/{thread_id}/runs/wait"), Some(json!({"assistant_id": "desk", "config": {"memory_blocks_declared": [{"label": "runbook", "description": "The steps for a stuck laptop.", "char_limit": 300}]}, "input": {"messages": [{"role": "user", "content": "hi"}]}}))).await;
    assert_eq!(status, StatusCode::OK, "{run}");
    let prompt = seen.lock().unwrap().last().cloned().unwrap_or_default();
    let text: String = prompt
        .iter()
        .filter_map(|m| m.content.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("## runbook — The steps for a stuck laptop.")
            && text.contains("Reboot first."),
        "the working copy's block rendered: {text}"
    );
    let (_, thread) = call(
        &app,
        "bob-key",
        "POST",
        "/threads",
        Some(json!({"graph": "react_agent"})),
    )
    .await;
    let thread_id = thread["thread_id"].as_str().unwrap().to_owned();
    let (_, _run) = call(&app, "bob-key", "POST", &format!("/threads/{thread_id}/runs/wait"), Some(json!({"assistant_id": "desk", "input": {"messages": [{"role": "user", "content": "hi again"}]}}))).await;
    let prompt = seen.lock().unwrap().last().cloned().unwrap_or_default();
    let text: String = prompt
        .iter()
        .filter_map(|m| m.content.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !text.contains("## runbook"),
        "the published agent declares no runbook: {text}"
    );

    let _ = std::fs::remove_dir_all(store);
}

/// A roll-up asked for by hand answers at once and works in the
/// background; the index says it is rolling until it ends, then carries
/// its new stamp.
#[tokio::test]
async fn an_on_demand_roll_up_answers_at_once_and_runs_in_the_background() {
    let (app, store, _seen) = test_app();
    let (status, started) = call(&app, "bob-key", "POST", "/memory/utility/roll-up", None).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{started}");
    assert_eq!(started["started"], json!(true), "{started}");
    assert_eq!(started["rolling"], json!(true), "{started}");
    let mut done = None;
    for _ in 0..50 {
        let (status, index) = call(&app, "bob-key", "GET", "/memory/utility", None).await;
        assert_eq!(status, StatusCode::OK, "{index}");
        if index["rolling"] == json!(false) {
            done = Some(index);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let index = done.expect("the roll-up ends");
    assert!(index["stamp"].as_str().is_some(), "{index}");
    let _ = std::fs::remove_dir_all(store);
}

/// A person vouches for a note the runtime marked as learned from outside
/// content: a record in their name without the mark supersedes it. Only a
/// marked note is confirmed, once, and a note about a person by that person.
#[tokio::test]
async fn a_person_confirms_a_note_learned_from_outside() {
    let (app, store, _seen) = test_app();
    let (status, written) = call(
        &app,
        "bob-key",
        "POST",
        "/memory",
        Some(json!({
            "kind": "fact", "scope": {"scope": "user", "id": "bob"},
            "content": {"text": "The vendor page says rebuild the OST first."},
            "author": {"type": "agent", "agent_id": "desk"}, "confidence": 0.5,
            "tags": ["origin:untrusted", "trigger:ost"]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{written}");
    let marked = written["memory_id"].as_str().unwrap().to_owned();

    // Someone else cannot vouch for a note about bob.
    let (status, refused) = call(
        &app,
        "cy-key",
        "POST",
        &format!("/memory/{marked}/confirm"),
        None,
    )
    .await;
    assert!(
        status == StatusCode::FORBIDDEN || status == StatusCode::NOT_FOUND,
        "{status} {refused}"
    );

    let (status, confirmed) = call(
        &app,
        "bob-key",
        "POST",
        &format!("/memory/{marked}/confirm"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{confirmed}");
    assert_eq!(confirmed["supersedes"], marked.as_str());
    let tags = confirmed["record"]["tags"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        !tags.iter().any(|t| t == "origin:untrusted") && tags.iter().any(|t| t == "trigger:ost"),
        "{confirmed}"
    );
    assert_eq!(confirmed["record"]["confidence"], 1.0, "{confirmed}");
    assert_eq!(
        confirmed["record"]["provenance"]["author"]["type"], "human",
        "{confirmed}"
    );

    // Once only; and a note without the mark has nothing to confirm.
    let (status, again) = call(
        &app,
        "bob-key",
        "POST",
        &format!("/memory/{marked}/confirm"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{again}");
    let fresh = confirmed["confirmed"].as_str().unwrap();
    let (status, plain) = call(
        &app,
        "bob-key",
        "POST",
        &format!("/memory/{fresh}/confirm"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{plain}");
    let _ = std::fs::remove_dir_all(store);
}

/// Two spellings of one subject's key land on one canonical key, so the
/// later note supersedes the earlier instead of standing beside it.
#[tokio::test]
async fn two_spellings_of_a_key_supersede_each_other() {
    let (app, store, _seen) = test_app();
    let (status, made) = call(&app, "bob-key", "POST", "/assistants", Some(json!({
        "assistant_id": "desk", "name": "Desk", "graph": "react_agent",
        "config": {"studio_intent": {"instructions": "Help.", "tools": [{"name": "memory.remember"}]}}
    }))).await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let mut replies = Vec::new();
    for message in [
        "keep Outlook-Sync:Vendor first|Rebuild the OST first.",
        "keep outlook_sync.vendor-first|Check the mailbox size first.",
    ] {
        let (_, thread) = call(
            &app,
            "bob-key",
            "POST",
            "/threads",
            Some(json!({"graph": "react_agent"})),
        )
        .await;
        let thread_id = thread["thread_id"].as_str().unwrap().to_owned();
        let (status, terminal) = call(&app, "bob-key", "POST", &format!("/threads/{thread_id}/runs/wait"), Some(json!({
            "assistant_id": "desk", "input": {"messages": [{"role": "user", "content": message}]}
        }))).await;
        assert_eq!(status, StatusCode::OK, "{terminal}");
        replies.push(terminal.to_string());
    }
    assert!(
        replies[0].contains("outlook_sync.vendor_first") && replies[0].contains("key_as_given"),
        "{}",
        replies[0]
    );
    assert!(
        replies[1].contains("supersedes"),
        "the second spelling supersedes the first: {}",
        replies[1]
    );
    let _ = std::fs::remove_dir_all(store);
}
