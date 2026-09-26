//! `assignment.create`: an agent hands another agent a durable piece of
//! work from inside a run, as a person would from the studio — the
//! assignment is made in the delegating run's world with the run as its
//! owner, one deeper in the chain of agent-started work; an agent does not
//! assign work to itself; and the chain stops at the limit.
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::error::{Result as RustyResult, RustyError};
use rusty_agent_runtime::llm::{ChatMessage, ChatModel, ChatResponse, ToolCall};
use rusty_agent_runtime::react::{create_react_agent, MESSAGES_CHANNEL};
use rusty_agent_runtime::state::{Reducer, StateSpec};
use rusty_agent_runtime::tool::{ToolRegistry, ToolSource};
use rusty_agent_server::{router, GraphRegistry, PlatformTools, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

/// One script per agent, keyed by the word its charter carries: every
/// agent here shares the one graph, and an assignment's rounds run as the
/// entrusted agent while the delegating run is still on its turn.
type Script = Arc<Mutex<Vec<(&'static str, VecDeque<ChatMessage>)>>>;

struct Scripted(Script);

#[async_trait]
impl ChatModel for Scripted {
    async fn chat(&self, messages: &[ChatMessage], _tools: &[Value]) -> RustyResult<ChatResponse> {
        let charter = messages.iter().filter(|m| m.role == rusty_agent_runtime::llm::Role::System).filter_map(|m| m.content.clone()).collect::<Vec<_>>().join("\n");
        let mut scripts = self.0.lock().unwrap();
        let (_, lines) = scripts.iter_mut().find(|(word, _)| charter.contains(word)).ok_or_else(|| RustyError::Llm(format!("no script for this charter: {charter}")))?;
        let message = lines.pop_front().ok_or_else(|| RustyError::Llm("script exhausted".into()))?;
        // Every call costs the same, so a chain's spend is countable.
        Ok(ChatResponse { message, model: Some("scripted".into()), usage: Some(rusty_agent_runtime::llm::Usage { prompt_tokens: 100, completion_tokens: 50, total_tokens: 150, ..Default::default() }) })
    }
}

fn app() -> (Router, PathBuf, Script) {
    let store = std::env::temp_dir().join(format!("rusty-server-assignment-create-{}", uuid::Uuid::new_v4()));
    let script: Script = Arc::new(Mutex::new(Vec::new()));
    let platform_tools = PlatformTools::new();
    let mut tools = ToolRegistry::new();
    tools.attach(Arc::clone(&platform_tools) as Arc<dyn ToolSource>);
    let graph = create_react_agent(Arc::new(Scripted(Arc::clone(&script))), tools.clone()).unwrap();
    let spec = StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry.register_with_tools("react_agent", graph, spec, &tools).unwrap();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone()).with_platform_tools(platform_tools);
    (router(registry, config), store, script)
}

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(v) => {
            builder = builder.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let response = app.clone().oneshot(builder.body(body).unwrap()).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn lines(script: &Script, word: &'static str, add: Vec<ChatMessage>) {
    let mut scripts = script.lock().unwrap();
    match scripts.iter_mut().find(|(w, _)| *w == word) {
        Some((_, q)) => q.extend(add),
        None => scripts.push((word, VecDeque::from(add))),
    }
}

fn delegation(args: Value) -> Vec<ChatMessage> {
    vec![ChatMessage::assistant_tool_calls(vec![ToolCall::new("c1", "assignment.create", args)]), ChatMessage::assistant("Delegated.")]
}

fn tool_reply(run: &Value) -> Value {
    run["output"][MESSAGES_CHANNEL].as_array().into_iter().flatten().find(|m| m["role"] == "tool").map(|m| serde_json::from_str::<Value>(m["content"].as_str().unwrap_or("null")).unwrap_or(Value::Null)).unwrap_or(Value::Null)
}

/// One run of `assistant`, whose model calls `assignment.create` with `args` and answers.
async fn delegating_run(app: &Router, script: &Script, assistant: &str, word: &'static str, args: Value) -> (Value, Value) {
    lines(script, word, delegation(args));
    run_of(app, assistant).await
}

/// One run of `assistant` on whatever its script holds next.
async fn run_of(app: &Router, assistant: &str) -> (Value, Value) {
    let (_, thread) = call(app, "POST", "/threads", Some(json!({"graph": "react_agent"}))).await;
    let thread_id = thread["thread_id"].as_str().unwrap().to_owned();
    let (status, run) = call(app, "POST", &format!("/threads/{thread_id}/runs/wait"), Some(json!({"assistant_id": assistant, "input": {MESSAGES_CHANNEL: [{"role": "user", "content": "hand it on"}]}}))).await;
    assert_eq!(status, StatusCode::OK, "{run}");
    let reply = tool_reply(&run);
    (run, reply)
}

#[tokio::test]
async fn an_agent_delegates_from_a_run_and_the_chain_stops_at_the_limit() {
    let (app, store, script) = app();
    for (id, name, word) in [("desk", "Desk", "DESK"), ("clerk", "Clerk", "CLERK"), ("runner", "Runner", "RUNNER")] {
        let (status, made) = call(&app, "POST", "/assistants", Some(json!({"assistant_id": id, "name": name, "graph": "react_agent", "config": {"instructions": format!("{word}: do the work"), "studio_intent": {"instructions": format!("{word}: do the work"), "tools": [{"name": "assignment.create"}]}}}))).await;
        assert_eq!(status, StatusCode::CREATED, "{made}");
    }

    // An agent does not assign work to itself.
    // (Named as a model names it — "the Desk agent" is the desk.)
    let (_, reply) = delegating_run(&app, &script, "desk", "DESK", json!({"agent": "the Desk agent", "request": "Do this later."})).await;
    assert_eq!(reply["created"], false, "{reply}");
    assert!(reply["note"].as_str().unwrap().contains("does not assign work to itself"), "{reply}");

    // A name nothing matches: refused with the live agents whose names share a word.
    let (_, reply) = delegating_run(&app, &script, "desk", "DESK", json!({"agent": "the clerk of works", "request": "Do this."})).await;
    assert_eq!(reply["created"], false, "{reply}");
    assert_eq!(reply["candidates"], json!(["Clerk"]), "{reply}");

    // The chain the rounds will run: the clerk's round hands work to the
    // runner, the runner's to the desk, and the desk's round — three deep —
    // may not start more.
    lines(&script, "CLERK", delegation(json!({"agent": "Runner", "request": "Check the runbook for each ticket.", "max_rounds": 1})));
    lines(&script, "RUNNER", delegation(json!({"agent": "Desk", "request": "Close what the runbook resolved.", "max_rounds": 1})));

    // The desk's lines, in order: the delegation that starts the chain, then
    // what its own round three deep will try — queued now, since that round
    // runs as soon as the runner's round delegates.
    lines(&script, "DESK", delegation(json!({"agent": "Clerk", "request": "List every open ticket older than a week and say which owner has the most.", "success": "A list with ticket numbers and one owner named.", "max_rounds": 2})));
    lines(&script, "DESK", delegation(json!({"agent": "Clerk", "request": "One more pass.", "max_rounds": 1})));

    // The desk hands the clerk a piece of work: made, owned by the desk's run, one deep.
    let (run, reply) = run_of(&app, "desk").await;
    assert_eq!(reply["created"], true, "{reply}");
    assert_eq!(reply["agent"], "Clerk");
    assert_eq!(reply["chain_depth"], 1);
    let assignment_id = reply["assignment_id"].as_str().unwrap().to_owned();
    let (status, a) = call(&app, "GET", &format!("/assignments/{assignment_id}"), None).await;
    assert_eq!(status, StatusCode::OK, "{a}");
    assert_eq!(a["assistant_id"], "clerk");
    assert_eq!(a["request"], "List every open ticket older than a week and say which owner has the most.");
    assert_eq!(a["success"], "A list with ticket numbers and one owner named.");
    assert_eq!(a["max_rounds"], 2);
    assert_eq!(a["owner"]["kind"], "agent", "{}", a["owner"]);
    assert_eq!(a["owner"]["agent_id"], "desk");
    assert_eq!(a["owner"]["run_id"], run["run_id"]);
    assert_eq!(a["chain"]["depth"], 1, "{}", a["chain"]);
    assert_eq!(a["chain"]["run_id"], run["run_id"]);
    assert_eq!(a["tags"][0], "delegated-by-agent");

    // The desk's round at depth 3 tries to hand work on and is refused.
    let mut refused = Value::Null;
    for _ in 0..400 {
        let (_, list) = call(&app, "GET", "/assignments", None).await;
        let deepest = list["assignments"].as_array().into_iter().flatten().find(|a| a["chain"]["depth"] == 3).cloned();
        if let Some(run_id) = deepest.as_ref().and_then(|a| a["rounds"][0]["run_id"].as_str()) {
            let (_, r) = call(&app, "GET", &format!("/runs/{run_id}"), None).await;
            if r["status"] == "success" || r["status"] == "error" || r["status"] == "failed" {
                refused = tool_reply(&r);
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let (_, list) = call(&app, "GET", "/assignments", None).await;
    let seen: Vec<Value> = list["assignments"].as_array().into_iter().flatten().map(|a| json!({"agent": a["assistant_id"], "depth": a["chain"]["depth"], "state": a["state"], "reason": a["state_reason"], "round": a["rounds"][0]["status"], "error": a["rounds"][0]["error"], "summary": a["rounds"][0]["summary"]})).collect();
    assert_eq!(refused["created"], false, "the third-deep round may not start more: {refused}; assignments: {seen:?}");
    assert_eq!(refused["chain_depth"], 4, "{refused}");
    assert_eq!(refused["limit"], 3);
    let (_, list) = call(&app, "GET", "/assignments", None).await;
    let depths: Vec<u64> = list["assignments"].as_array().unwrap().iter().filter_map(|a| a["chain"]["depth"].as_u64()).collect();
    assert_eq!(depths.iter().filter(|d| **d == 4).count(), 0, "nothing was made past the limit: {depths:?}");

    let _ = std::fs::remove_dir_all(store);
}

/// The run that delegated is over when the work ends, and an agent has no
/// inbox: the delegating agent is told through its own memory — one note
/// under the assignment's key, the later state superseding the earlier.
#[tokio::test]
async fn the_delegating_agent_is_told_through_its_memory_when_the_work_ends() {
    let (app, store, script) = app();
    for (id, name, word) in [("desk", "Desk", "DESK"), ("clerk", "Clerk", "CLERK"), ("qa", "Incident Q&A", "QA")] {
        let (status, made) = call(&app, "POST", "/assistants", Some(json!({"assistant_id": id, "name": name, "graph": "react_agent", "config": {"instructions": format!("{word}: do the work"), "studio_intent": {"instructions": format!("{word}: do the work"), "tools": [{"name": "assignment.create"}]}}}))).await;
        assert_eq!(status, StatusCode::CREATED, "{made}");
    }
    // A name written the way a model saw it, HTML-escaped, resolves.
    lines(&script, "QA", vec![ChatMessage::assistant("Nothing open."), ChatMessage::assistant("Nothing to record.")]);
    let (_, reply) = delegating_run(&app, &script, "desk", "DESK", json!({"agent": "Incident Q&amp;A agent", "request": "Any open P1?", "max_rounds": 1})).await;
    assert_eq!(reply["created"], true, "{reply}");
    assert_eq!(reply["agent"], "Incident Q&A");
    // The clerk's one round answers without a progress record: its round is
    // spent, so the work waits for a person — and the desk hears that.
    lines(&script, "CLERK", vec![ChatMessage::assistant("3 open tickets older than a week; Ana owns two of them."), ChatMessage::assistant("Nothing to record.")]);
    let (_, reply) = delegating_run(&app, &script, "desk", "DESK", json!({"agent": "Clerk", "request": "Count the open tickets older than a week.", "max_rounds": 1})).await;
    assert_eq!(reply["created"], true, "{reply}");
    let assignment_id = reply["assignment_id"].as_str().unwrap().to_owned();

    let mut state = Value::Null;
    for _ in 0..400 {
        let (_, a) = call(&app, "GET", &format!("/assignments/{assignment_id}"), None).await;
        if a["state"] != "working" {
            state = a;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(state["state"], "waiting", "{}", json!({"state": state["state"], "reason": state["state_reason"], "rounds": state["rounds"]}));
    assert_eq!(state["told"], "delivered", "{}", state["told"]);

    let notes = |list: &Value| -> Vec<Value> {
        list.as_array().into_iter().flatten().filter(|m| m["key"] == format!("assignment:{assignment_id}")).cloned().collect()
    };
    let (status, mine) = call(&app, "POST", "/memory/query", Some(json!({"scope": {"scope": "agent", "id": "desk"}}))).await;
    assert_eq!(status, StatusCode::OK, "{mine}");
    let list = mine.get("records").cloned().unwrap_or_else(|| mine.clone());
    let waiting = notes(&list);
    assert_eq!(waiting.len(), 1, "one note under the assignment's key: {mine}");
    let text = waiting[0]["content"]["value"]["text"].as_str().unwrap_or("").to_owned();
    assert!(text.contains("delegated to Clerk") && text.contains("waits for a person"), "{text}");
    assert_eq!(waiting[0]["provenance"]["author"]["agent_id"], "clerk", "the entrusted agent is the author: {}", waiting[0]);

    // The owner marks it done: the note under the key is the done one now,
    // superseding the earlier; the clerk's finding rides with it.
    let (status, _) = call(&app, "POST", &format!("/assignments/{assignment_id}/done"), None).await;
    assert_eq!(status, StatusCode::OK);
    let (_, mine) = call(&app, "POST", "/memory/query", Some(json!({"scope": {"scope": "agent", "id": "desk"}}))).await;
    let list = mine.get("records").cloned().unwrap_or_else(|| mine.clone());
    let mut all = notes(&list);
    all.sort_by(|x, y| x["created_at"].as_str().cmp(&y["created_at"].as_str()));
    let newest = all.last().unwrap();
    let text = newest["content"]["value"]["text"].as_str().unwrap_or("").to_owned();
    assert!(text.contains("is done") && text.contains("3 open tickets older than a week"), "{text}");
    assert_eq!(newest["supersedes"], waiting[0]["memory_id"], "the done note supersedes the waiting one: {newest}");

    let _ = std::fs::remove_dir_all(store);
}


/// The budget that follows a chain: the delegating agent's cap covers every
/// round and task the chain starts; once spent, a round in the chain waits
/// for a person and nothing more starts from it — while a fresh chain from
/// the same agent starts clean.
#[tokio::test]
async fn a_chain_of_agent_started_work_stops_when_its_token_budget_is_spent() {
    let (app, store, script) = app();
    // The desk caps the work it starts at 200 tokens; the clerk's one round
    // costs 300 (two calls at 150).
    let (status, made) = call(&app, "POST", "/assistants", Some(json!({"assistant_id": "desk", "name": "Desk", "graph": "react_agent", "config": {"instructions": "DESK: do the work", "studio_intent": {"instructions": "DESK: do the work", "tools": [{"name": "assignment.create"}], "chain_max_tokens": 200}}}))).await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let (status, made) = call(&app, "POST", "/assistants", Some(json!({"assistant_id": "clerk", "name": "Clerk", "graph": "react_agent", "config": {"instructions": "CLERK: do the work", "studio_intent": {"instructions": "CLERK: do the work", "tools": [{"name": "assignment.create"}]}}}))).await;
    assert_eq!(status, StatusCode::CREATED, "{made}");

    // The clerk's round: one answer, then the closing turn's answer.
    lines(&script, "CLERK", vec![ChatMessage::assistant("Counted: 3 tickets."), ChatMessage::assistant("Nothing to record.")]);
    let (_, reply) = delegating_run(&app, &script, "desk", "DESK", json!({"agent": "Clerk", "request": "Count the tickets, then hand the runbook check on.", "max_rounds": 2})).await;
    assert_eq!(reply["created"], true, "{reply}");
    assert_eq!(reply["chain_budget"]["max_tokens"], 200, "{reply}");
    assert_eq!(reply["chain_budget"]["spent_tokens"], 0);
    let assignment_id = reply["assignment_id"].as_str().unwrap().to_owned();
    let root = reply["chain_budget"]["root_run_id"].as_str().unwrap().to_owned();

    // Round one ends with no record, its budget of rounds not yet spent: the
    // driver would start round two — but the chain's 300 tokens (the round
    // and its closing turn) passed the cap, so the assignment stops for a
    // person with the words.
    let mut a = Value::Null;
    for _ in 0..400 {
        let (_, got) = call(&app, "GET", &format!("/assignments/{assignment_id}"), None).await;
        if got["state"] == "waiting" || got["state"] == "blocked" || got["state"] == "done" {
            a = got;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(a["state"], "blocked", "{}", json!({"state": a["state"], "reason": a["state_reason"], "rounds": a["rounds"].as_array().map(|r| r.len())}));
    assert!(a["state_reason"].as_str().unwrap_or("").contains("300 of 200 tokens across 2 runs"), "{}", a["state_reason"]);
    assert!(a["state_reason"].as_str().unwrap_or("").contains("token budget is spent"), "{}", a["state_reason"]);
    assert_eq!(a["rounds"].as_array().map(|r| r.len()), Some(1), "round two never started: {}", a["rounds"]);
    assert_eq!(a["chain"]["root_run_id"], root);
    assert_eq!(a["chain"]["max_tokens"], 200);

    // A cap of zero is a cap: this agent may start no work, said so.
    let (status, _) = call(&app, "POST", "/assistants", Some(json!({"assistant_id": "mute", "name": "Mute", "graph": "react_agent", "config": {"instructions": "MUTE: do the work", "studio_intent": {"instructions": "MUTE: do the work", "tools": [{"name": "assignment.create"}], "chain_max_tokens": 0}}}))).await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, refused) = delegating_run(&app, &script, "mute", "MUTE", json!({"agent": "Clerk", "request": "Anything.", "max_rounds": 1})).await;
    assert_eq!(refused["created"], false, "{refused}");
    assert!(refused["note"].as_str().unwrap_or("").contains("may start no work"), "{refused}");
    assert_eq!(refused["chain_budget"]["max_tokens"], 0);

    // Nothing more starts from that chain: the desk's own next delegation
    // from a run inside it is refused — here, the desk asked again from a
    // fresh run starts a fresh chain and is allowed.
    lines(&script, "CLERK", vec![ChatMessage::assistant("Counted again."), ChatMessage::assistant("Nothing to record.")]);
    let (_, again) = delegating_run(&app, &script, "desk", "DESK", json!({"agent": "Clerk", "request": "Count once more.", "max_rounds": 1})).await;
    assert_eq!(again["created"], true, "a fresh run is a fresh chain: {again}");
    assert_ne!(again["chain_budget"]["root_run_id"], json!(root));

    let _ = std::fs::remove_dir_all(store);
}
