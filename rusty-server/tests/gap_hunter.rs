//! The gap backlog as work an agent picks up: `gaps.work_order` reads the
//! queue in priority order, `gaps.resolve` claims a gap the run answered,
//! and the claim settles on the run's verdict — verified closes it with
//! the run as its resolution, anything else puts it back. Through the real
//! paths: the filing route, a scripted agent on the platform's tools, a
//! scripted verifier.
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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

type Script = Arc<Mutex<VecDeque<ChatMessage>>>;

struct Scripted(Script);

#[async_trait]
impl ChatModel for Scripted {
    async fn chat(&self, _messages: &[ChatMessage], _tools: &[Value]) -> RustyResult<ChatResponse> {
        let message = self.0.lock().unwrap().pop_front().ok_or_else(|| RustyError::Llm("script exhausted".into()))?;
        Ok(ChatResponse { message, model: Some("scripted".into()), usage: None })
    }
}

fn app() -> (Router, PathBuf, Script, Script) {
    app_with_judge(true)
}

/// The same app; without a judge no run gets a verdict.
fn app_with_judge(judged: bool) -> (Router, PathBuf, Script, Script) {
    let store = std::env::temp_dir().join(format!("rusty-server-gap-hunter-{}", uuid::Uuid::new_v4()));
    let agent: Script = Arc::new(Mutex::new(VecDeque::new()));
    let judge: Script = Arc::new(Mutex::new(VecDeque::new()));
    let platform_tools = PlatformTools::new();
    let mut tools = ToolRegistry::new();
    tools.attach(Arc::clone(&platform_tools) as Arc<dyn ToolSource>);
    let graph = create_react_agent(Arc::new(Scripted(Arc::clone(&agent))), tools.clone()).unwrap();
    let spec = StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry.register_with_tools("react_agent", graph, spec, &tools).unwrap();
    let mut config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone()).with_platform_tools(platform_tools);
    if judged {
        config = config.with_verifier(Arc::new(Scripted(Arc::clone(&judge))));
    }
    (router(registry, config), store, agent, judge)
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

async fn gap(app: &Router, gap_id: &str) -> Value {
    let (status, v) = call(app, "GET", &format!("/gaps/{gap_id}"), None).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    v
}

async fn status_settles(app: &Router, gap_id: &str, want: &str) -> Value {
    for _ in 0..200 {
        let g = gap(app, gap_id).await;
        if g["entry"]["status"] == want {
            return g["entry"].clone();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("gap {gap_id} never reached `{want}`: {}", gap(app, gap_id).await);
}

/// One run of the hunter: it reads the work order, claims `gap_id`, and
/// answers; the judge says `verdict`.
async fn hunt(app: &Router, agent: &Script, judge: &Script, gap_id: &str, verdict: &str) -> Value {
    *agent.lock().unwrap() = VecDeque::from(vec![
        ChatMessage::assistant_tool_calls(vec![ToolCall::new("c1", "gaps.work_order", json!({"limit": 3}))]),
        ChatMessage::assistant_tool_calls(vec![ToolCall::new("c2", "gaps.resolve", json!({"gap_id": gap_id, "how": "the VPN runbook, section 3"}))]),
        ChatMessage::assistant("Intermittent VPN drops: set the client's MTU to 1380 and disable IPv6 on the adapter; the runbook's section 3 says so."),
    ]);
    *judge.lock().unwrap() = VecDeque::from(vec![ChatMessage::assistant(format!("{{\"verdict\": \"{verdict}\", \"reason\": \"scripted\"}}"))]);
    let (_, thread) = call(app, "POST", "/threads", Some(json!({"graph": "react_agent"}))).await;
    let thread_id = thread["thread_id"].as_str().unwrap().to_owned();
    let (status, run) = call(
        app,
        "POST",
        &format!("/threads/{thread_id}/runs/wait"),
        Some(json!({"input": {MESSAGES_CHANNEL: [
            {"role": "system", "content": "CHARTER: read the gap backlog, answer one you can, resolve it."},
            {"role": "user", "content": "work the backlog"}
        ]}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{run}");
    assert_eq!(run["status"], "success", "{run}");
    run
}

#[tokio::test]
async fn a_hunter_reads_the_work_order_claims_a_gap_and_the_verdict_settles_it() {
    let (app, store, agent, judge) = app();
    // A gap filed on a question nobody could answer (an operator's filing;
    // an agent's own goes through gaps.file, the same ledger).
    let (status, filed) = call(&app, "POST", "/gaps/file", Some(json!({
        "subject": {"question_shape": {"text": "how do i stop the vpn dropping every few minutes"}},
        "statement": "No source says how intermittent VPN drops are fixed",
        "evidence": [{"kind": "run_receipt", "id": "run-seed", "note": "the run that needed it"}],
        "origin": "operator",
        "closure_criteria": {"failure_rate_below": {"threshold_millis": 50}},
        "volume": 4
    }))).await;
    assert_eq!(status, StatusCode::CREATED, "{filed}");
    let gap_id = filed["gap_id"].as_str().unwrap().to_owned();

    // The work order the hunter reads: the gap, open, with what closes it.
    let run = hunt(&app, &agent, &judge, &gap_id, "unverified").await;
    let messages = run["output"][MESSAGES_CHANNEL].as_array().unwrap();
    let tool_replies: Vec<Value> = messages.iter().filter(|m| m["role"] == "tool").map(|m| serde_json::from_str(m["content"].as_str().unwrap_or("null")).unwrap_or(Value::Null)).collect();
    assert_eq!(tool_replies[0]["open"], 1, "{}", tool_replies[0]);
    assert_eq!(tool_replies[0]["gaps"][0]["gap_id"], json!(gap_id));
    assert_eq!(tool_replies[0]["gaps"][0]["question"], "how do i stop the vpn dropping every few minutes");
    assert!(tool_replies[0]["gaps"][0]["closes_when"].as_str().unwrap().contains("gaps.resolve"), "{}", tool_replies[0]);
    assert_eq!(tool_replies[1]["claimed"], true, "{}", tool_replies[1]);
    // The judge did not confirm the answer: the claim is released, the gap is back in the queue.
    let back = status_settles(&app, &gap_id, "open").await;
    assert!(back["resolution"].is_null(), "{back}");

    // The same hunt, verified: the gap closes with the run as its resolution.
    let run = hunt(&app, &agent, &judge, &gap_id, "verified").await;
    assert_eq!(run["verification"]["verdict"], "verified", "{run}");
    let run_id = run["run_id"].as_str().unwrap();
    let closed = status_settles(&app, &gap_id, "closed").await;
    assert_eq!(closed["resolution"], json!(format!("run:{run_id}:verified")), "{closed}");

    // Once closed, the work order no longer offers it — and claiming it is refused.
    *agent.lock().unwrap() = VecDeque::from(vec![
        ChatMessage::assistant_tool_calls(vec![ToolCall::new("c1", "gaps.work_order", json!({}))]),
        ChatMessage::assistant_tool_calls(vec![ToolCall::new("c2", "gaps.resolve", json!({"gap_id": gap_id, "how": "again"}))]),
        ChatMessage::assistant("Nothing left to do."),
    ]);
    *judge.lock().unwrap() = VecDeque::from(vec![ChatMessage::assistant("{\"verdict\": \"verified\", \"reason\": \"scripted\"}")]);
    let (_, thread) = call(&app, "POST", "/threads", Some(json!({"graph": "react_agent"}))).await;
    let thread_id = thread["thread_id"].as_str().unwrap().to_owned();
    let (_, run) = call(&app, "POST", &format!("/threads/{thread_id}/runs/wait"), Some(json!({"input": {MESSAGES_CHANNEL: [{"role": "user", "content": "work the backlog"}]}}))).await;
    let messages = run["output"][MESSAGES_CHANNEL].as_array().unwrap();
    let tool_replies: Vec<Value> = messages.iter().filter(|m| m["role"] == "tool").map(|m| serde_json::from_str(m["content"].as_str().unwrap_or("null")).unwrap_or(Value::Null)).collect();
    assert_eq!(tool_replies[0]["open"], 0, "{}", tool_replies[0]);
    assert_eq!(tool_replies[1]["claimed"], false, "{}", tool_replies[1]);

    let _ = std::fs::remove_dir_all(store);
}

/// A claim whose run ended without a verdict does not hold the gap
/// forever: the sweep releases it back to the queue.
#[tokio::test]
async fn a_claim_whose_run_got_no_verdict_is_released_by_the_sweep() {
    let (app, store, agent, _judge) = app_with_judge(false);
    let (status, filed) = call(&app, "POST", "/gaps/file", Some(json!({
        "subject": {"question_shape": {"text": "how do i stop the vpn dropping"}},
        "statement": "No source says how intermittent VPN drops are fixed",
        "evidence": [{"kind": "run_receipt", "id": "run-seed", "note": "the run that needed it"}],
        "origin": "operator",
        "closure_criteria": {"failure_rate_below": {"threshold_millis": 50}},
        "volume": 2
    }))).await;
    assert_eq!(status, StatusCode::CREATED, "{filed}");
    let gap_id = filed["gap_id"].as_str().unwrap().to_owned();
    *agent.lock().unwrap() = VecDeque::from(vec![
        ChatMessage::assistant_tool_calls(vec![ToolCall::new("c1", "gaps.resolve", json!({"gap_id": gap_id, "how": "the runbook"}))]),
        ChatMessage::assistant("Set the MTU to 1380."),
    ]);
    let (_, thread) = call(&app, "POST", "/threads", Some(json!({"graph": "react_agent"}))).await;
    let thread_id = thread["thread_id"].as_str().unwrap().to_owned();
    let (status, run) = call(&app, "POST", &format!("/threads/{thread_id}/runs/wait"), Some(json!({"input": {MESSAGES_CHANNEL: [{"role": "user", "content": "work the backlog"}]}}))).await;
    assert_eq!(status, StatusCode::OK, "{run}");
    assert!(run.get("verification").is_none() || run["verification"].is_null(), "no judge, no verdict: {run}");
    // Claimed, and nothing settles it: trial pending, out of the queue.
    let g = gap(&app, &gap_id).await;
    assert_eq!(g["entry"]["status"], "trial_pending", "{}", g["entry"]);
    let (_, listed) = call(&app, "GET", "/gaps", None).await;
    assert!(listed["work_order"].as_array().unwrap().is_empty(), "{listed}");
    assert_eq!(listed["claimed"][0]["gap_id"], json!(gap_id), "the lane shows it as claimed: {listed}");
    // The sweep: the run ended without a verdict, so the claim is released.
    let (status, swept) = call(&app, "POST", "/gaps/sweep", Some(json!({"threshold_millis": 50}))).await;
    assert_eq!(status, StatusCode::OK, "{swept}");
    assert_eq!(swept["claims"][0]["gap_id"], json!(gap_id), "{swept}");
    assert_eq!(swept["claims"][0]["released"], "its run ended without a verdict");
    let g = gap(&app, &gap_id).await;
    assert_eq!(g["entry"]["status"], "open", "{}", g["entry"]);
    let (_, listed) = call(&app, "GET", "/gaps", None).await;
    assert_eq!(listed["work_order"][0]["gap_id"], json!(gap_id), "back in the queue: {listed}");
    // The listing says who filed each gap, so the backlog can be narrowed
    // to one filer's gaps.
    assert!(listed["work_order"][0]["filer"].as_str().is_some_and(|f| !f.is_empty()), "the filer rides the row: {listed}");
    // Once, not twice: the claim is spent.
    let (_, swept) = call(&app, "POST", "/gaps/sweep", Some(json!({"threshold_millis": 50}))).await;
    assert!(swept["claims"].as_array().unwrap().is_empty(), "{swept}");
    let _ = std::fs::remove_dir_all(store);
}

/// The brief a review starts from: `agents.read` carries the agent's goal
/// measured on its live runs of the week, and the gaps it filed that stay
/// open — so the Coach reads the shortfall and what the agent could not do
/// before it names a cause.
#[tokio::test]
async fn agents_read_carries_the_goal_shortfall_and_the_gaps_the_agent_filed() {
    let (app, store, agent, judge) = app();
    let (status, made) = call(&app, "POST", "/assistants", Some(json!({
        "assistant_id": "desk", "name": "Desk", "graph": "react_agent",
        "config": {"instructions": "DESK: help people", "studio_intent": {"instructions": "DESK: help people", "tools": [{"name": "gaps.file"}]}},
        "metadata": {"studio": {"goal": {"objective": "Every issue answered from a source", "metric": "Runs that finished", "target": 90}}}
    }))).await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let (status, made) = call(&app, "POST", "/assistants", Some(json!({
        "assistant_id": "coach", "name": "Coach", "graph": "react_agent",
        "config": {"instructions": "COACH: review agents", "studio_intent": {"instructions": "COACH: review agents", "tools": [{"name": "agents.read"}]}}
    }))).await;
    assert_eq!(status, StatusCode::CREATED, "{made}");

    // The desk's one live run finishes, and files what it could not answer.
    *agent.lock().unwrap() = VecDeque::from(vec![
        ChatMessage::assistant_tool_calls(vec![ToolCall::new("g1", "gaps.file", json!({"question": "Can I expense my home internet?", "missing": "a stipend policy source that says who qualifies"}))]),
        ChatMessage::assistant("I have no stipend policy to answer from; filed the gap."),
    ]);
    *judge.lock().unwrap() = VecDeque::from(vec![ChatMessage::assistant("{\"verdict\": \"verified\", \"reason\": \"scripted\"}")]);
    let (_, thread) = call(&app, "POST", "/threads", Some(json!({"graph": "react_agent"}))).await;
    let thread_id = thread["thread_id"].as_str().unwrap().to_owned();
    let (status, run) = call(&app, "POST", &format!("/threads/{thread_id}/runs/wait"), Some(json!({"assistant_id": "desk", "input": {MESSAGES_CHANNEL: [{"role": "user", "content": "Can I expense my home internet?"}]}}))).await;
    assert_eq!(status, StatusCode::OK, "{run}");
    assert_eq!(run["status"], "success", "{run}");

    // The coach reads the desk: the goal measured, the gap listed.
    *agent.lock().unwrap() = VecDeque::from(vec![
        ChatMessage::assistant_tool_calls(vec![ToolCall::new("r1", "agents.read", json!({"agent": "Desk"}))]),
        ChatMessage::assistant("Read."),
    ]);
    *judge.lock().unwrap() = VecDeque::from(vec![ChatMessage::assistant("{\"verdict\": \"verified\", \"reason\": \"scripted\"}")]);
    let (_, thread) = call(&app, "POST", "/threads", Some(json!({"graph": "react_agent"}))).await;
    let thread_id = thread["thread_id"].as_str().unwrap().to_owned();
    let (status, run) = call(&app, "POST", &format!("/threads/{thread_id}/runs/wait"), Some(json!({"assistant_id": "coach", "input": {MESSAGES_CHANNEL: [{"role": "user", "content": "Review the Desk."}]}}))).await;
    assert_eq!(status, StatusCode::OK, "{run}");
    let read: Value = run["output"][MESSAGES_CHANNEL]
        .as_array()
        .into_iter()
        .flatten()
        .find(|m| m["role"] == "tool")
        .and_then(|m| serde_json::from_str(m["content"].as_str().unwrap_or("null")).ok())
        .unwrap_or(Value::Null);
    assert_eq!(read["found"], true, "{read}");
    let goal = &read["goal"];
    assert_eq!(goal["metric"], "Runs that finished", "{goal}");
    assert_eq!(goal["target"], 90.0);
    assert_eq!(goal["sample"], 1, "the desk's one live run counted: {goal}");
    assert_eq!(goal["current"], 100.0, "{goal}");
    assert_eq!(goal["on_target"], true, "{goal}");
    assert_eq!(goal["shortfall"], 0.0, "{goal}");
    let gaps = &read["gaps_filed"];
    assert_eq!(gaps["open"], 1, "{gaps}");
    assert_eq!(gaps["listed"][0]["question"], "can i expense my home internet?", "{gaps}");
    assert_eq!(gaps["listed"][0]["missing"], "a stipend policy source that says who qualifies");
    assert_eq!(gaps["listed"][0]["askers"], 1);

    // The route the Goal card reads gives the same number, with the trend.
    let (status, measured) = call(&app, "GET", "/assistants/desk/goal?metric=Runs%20that%20finished", None).await;
    assert_eq!(status, StatusCode::OK, "{measured}");
    assert_eq!(measured["current"], 100.0, "{measured}");
    assert_eq!(measured["sample"], 1);
    assert_eq!(measured["trend"].as_array().map(|t| t.len()), Some(7));
    assert_eq!(measured["trend"][6], 100.0, "today's bar: {measured}");
    assert_eq!(measured["goal"]["target"], 90);

    // An agent without a goal reads `null`, not a made-up one; the coach
    // filed no gap, so its own list is empty.
    *agent.lock().unwrap() = VecDeque::from(vec![
        ChatMessage::assistant_tool_calls(vec![ToolCall::new("r2", "agents.read", json!({"agent": "Coach"}))]),
        ChatMessage::assistant("Read."),
    ]);
    *judge.lock().unwrap() = VecDeque::from(vec![ChatMessage::assistant("{\"verdict\": \"verified\", \"reason\": \"scripted\"}")]);
    let (_, thread) = call(&app, "POST", "/threads", Some(json!({"graph": "react_agent"}))).await;
    let thread_id = thread["thread_id"].as_str().unwrap().to_owned();
    let (_, run) = call(&app, "POST", &format!("/threads/{thread_id}/runs/wait"), Some(json!({"assistant_id": "coach", "input": {MESSAGES_CHANNEL: [{"role": "user", "content": "Review yourself."}]}}))).await;
    let read: Value = run["output"][MESSAGES_CHANNEL].as_array().into_iter().flatten().find(|m| m["role"] == "tool").and_then(|m| serde_json::from_str(m["content"].as_str().unwrap_or("null")).ok()).unwrap_or(Value::Null);
    assert!(read["goal"].is_null(), "{}", read["goal"]);
    assert_eq!(read["gaps_filed"]["open"], 0, "{}", read["gaps_filed"]);

    let _ = std::fs::remove_dir_all(store);
}


/// A gap that names a tool the platform has but the filing agent lacks
/// becomes a proposal on that agent — gated, a person approves — and stays
/// open until a version carrying the tool is the one that runs; the gap
/// closes for the agent, not for the platform having the tool.
#[tokio::test]
async fn a_gap_naming_a_tool_the_agent_lacks_proposes_it_and_closes_when_the_agent_has_it() {
    let (app, store, agent, judge) = app();
    let (status, made) = call(&app, "POST", "/assistants", Some(json!({
        "assistant_id": "desk", "name": "Desk", "graph": "react_agent",
        "config": {"instructions": "DESK: help people", "studio_intent": {"instructions": "DESK: help people", "tools": [{"name": "gaps.file"}]}}
    }))).await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let active = made["active_version_id"].as_str().unwrap_or_default().to_owned();

    *agent.lock().unwrap() = VecDeque::from(vec![
        ChatMessage::assistant_tool_calls(vec![ToolCall::new("g1", "gaps.file", json!({"question": "Which other agents exist on this platform?", "missing": "an agents.directory or agents.list tool that names the live agents — nothing lists them"}))]),
        ChatMessage::assistant("I cannot list agents; filed the gap."),
    ]);
    *judge.lock().unwrap() = VecDeque::from(vec![ChatMessage::assistant("{\"verdict\": \"verified\", \"reason\": \"scripted\"}")]);
    let (_, thread) = call(&app, "POST", "/threads", Some(json!({"graph": "react_agent"}))).await;
    let thread_id = thread["thread_id"].as_str().unwrap().to_owned();
    let (status, run) = call(&app, "POST", &format!("/threads/{thread_id}/runs/wait"), Some(json!({"assistant_id": "desk", "input": {MESSAGES_CHANNEL: [{"role": "user", "content": "Which other agents exist?"}]}}))).await;
    assert_eq!(status, StatusCode::OK, "{run}");
    let filed: Value = run["output"][MESSAGES_CHANNEL].as_array().into_iter().flatten().find(|m| m["role"] == "tool").and_then(|m| serde_json::from_str(m["content"].as_str().unwrap_or("null")).ok()).unwrap_or(Value::Null);
    let gap_id = filed["gap_id"].as_str().unwrap_or_else(|| panic!("the gap was filed: {filed}")).to_owned();

    // The sweep: the platform has agents.list, the desk does not — proposed, not closed.
    let (status, swept) = call(&app, "POST", "/gaps/sweep", Some(json!({"threshold_millis": 50}))).await;
    assert_eq!(status, StatusCode::OK, "{swept}");
    assert!(swept["closed_on_capability"].as_array().is_some_and(|c| c.is_empty()), "the gap is not the platform's to close: {swept}");
    let proposed = &swept["proposed_on_capability"][0];
    assert_eq!(proposed["gap_id"], gap_id, "{swept}");
    assert_eq!(proposed["assistant_id"], "desk");
    assert_eq!(proposed["tools"], json!(["agents.list"]));
    let version_id = proposed["version_id"].as_str().unwrap().to_owned();
    let (_, g) = call(&app, "GET", &format!("/gaps/{gap_id}"), None).await;
    assert_eq!(g["entry"]["status"], "open", "{g}");

    // The proposal on the agent: the tool added, the gap named as proposer, the reason in the agent's words.
    let (status, ver) = call(&app, "GET", &format!("/assistants/desk/versions/{version_id}"), None).await;
    assert_eq!(status, StatusCode::OK, "{ver}");
    let ver = if ver.get("version").is_some() { ver["version"].clone() } else { ver };
    let names: Vec<&str> = ver["config"]["studio_intent"]["tools"].as_array().unwrap_or_else(|| panic!("the version's tools: {ver}")).iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(names.contains(&"agents.list") && names.contains(&"gaps.file"), "{names:?}");
    assert_eq!(ver["metadata"]["proposed_by"]["kind"], "gap");
    assert_eq!(ver["metadata"]["proposed_by"]["gap_id"], gap_id);
    assert!(ver["metadata"]["proposed_by"]["reason"].as_str().unwrap().contains("which other agents exist on this platform?"), "{}", ver["metadata"]);

    // Swept again: nothing twice.
    let (_, swept) = call(&app, "POST", "/gaps/sweep", Some(json!({"threshold_millis": 50}))).await;
    assert!(swept["proposed_on_capability"].as_array().is_some_and(|p| p.is_empty()), "one proposal per gap: {swept}");

    // A person activates it: the desk can call the tool now, so the gap closes.
    let (status, act) = call(&app, "POST", &format!("/assistants/desk/versions/{version_id}/activate"), Some(json!({"expected_active_version_id": active, "override_reason": "walked in the test"}))).await;
    assert_eq!(status, StatusCode::OK, "{act}");
    let (_, g) = call(&app, "GET", &format!("/gaps/{gap_id}"), None).await;
    assert_eq!(g["entry"]["status"], "closed", "{g}");
    assert_eq!(g["entry"]["resolution"], "capability:agents.list", "{g}");

    let _ = std::fs::remove_dir_all(store);
}


/// An agent that finds a source wrong proposes the corrected body; the
/// source is unchanged until a person accepts — which mints the superseding
/// version on the agent's proposal — or declines with a reason. One open
/// suggestion per source from an agent at a time.
#[tokio::test]
async fn an_agent_proposes_a_knowledge_edit_and_a_person_decides() {
    let (app, store, agent, judge) = app();
    let (status, made) = call(&app, "POST", "/knowledge/sources", Some(json!({"source_id": "hours", "kind": "text", "title": "Badge office hours", "author": "human:curator", "body": "the badge office opens at ten and closes at four"}))).await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let (status, made) = call(&app, "POST", "/assistants", Some(json!({"assistant_id": "desk", "name": "Desk", "graph": "react_agent", "config": {"instructions": "DESK: help people", "studio_intent": {"instructions": "DESK: help people", "tools": [{"name": "knowledge.propose_edit"}]}}}))).await;
    assert_eq!(status, StatusCode::CREATED, "{made}");

    let propose = |why: &str| vec![
        ChatMessage::assistant_tool_calls(vec![ToolCall::new("p1", "knowledge.propose_edit", json!({"source_id": "hours", "body": "the badge office opens at nine and closes at five", "why": why}))]),
        ChatMessage::assistant("Proposed a correction to the badge office hours."),
    ];
    *agent.lock().unwrap() = VecDeque::from(propose("the facilities ticket FAC0001 says nine to five since March"));
    *judge.lock().unwrap() = VecDeque::from(vec![ChatMessage::assistant("{\"verdict\": \"verified\", \"reason\": \"scripted\"}")]);
    let (_, thread) = call(&app, "POST", "/threads", Some(json!({"graph": "react_agent"}))).await;
    let thread_id = thread["thread_id"].as_str().unwrap().to_owned();
    let (status, run) = call(&app, "POST", &format!("/threads/{thread_id}/runs/wait"), Some(json!({"assistant_id": "desk", "input": {MESSAGES_CHANNEL: [{"role": "user", "content": "When does the badge office open?"}]}}))).await;
    assert_eq!(status, StatusCode::OK, "{run}");
    let reply: Value = run["output"][MESSAGES_CHANNEL].as_array().into_iter().flatten().find(|m| m["role"] == "tool").and_then(|m| serde_json::from_str(m["content"].as_str().unwrap_or("null")).ok()).unwrap_or(Value::Null);
    assert_eq!(reply["proposed"], true, "{reply}");
    let edit_id = reply["edit_id"].as_str().unwrap().to_owned();

    // The source is unchanged; the suggestion waits.
    let (_, listed) = call(&app, "GET", "/knowledge/sources", None).await;
    let source = listed["sources"].as_array().unwrap().iter().find(|s| s["source_id"] == "hours").cloned().unwrap();
    assert_eq!(source["version"], 1, "{source}");
    let (_, edits) = call(&app, "GET", "/knowledge/edits", None).await;
    assert_eq!(edits["waiting"], 1, "{edits}");
    assert_eq!(edits["edits"][0]["proposed_by"]["agent_id"], "desk");

    // A second proposal from the same agent while one waits is refused.
    *agent.lock().unwrap() = VecDeque::from(propose("again"));
    *judge.lock().unwrap() = VecDeque::from(vec![ChatMessage::assistant("{\"verdict\": \"verified\", \"reason\": \"scripted\"}")]);
    let (_, thread) = call(&app, "POST", "/threads", Some(json!({"graph": "react_agent"}))).await;
    let thread_id = thread["thread_id"].as_str().unwrap().to_owned();
    let (_, run) = call(&app, "POST", &format!("/threads/{thread_id}/runs/wait"), Some(json!({"assistant_id": "desk", "input": {MESSAGES_CHANNEL: [{"role": "user", "content": "again"}]}}))).await;
    let again: Value = run["output"][MESSAGES_CHANNEL].as_array().into_iter().flatten().find(|m| m["role"] == "tool").and_then(|m| serde_json::from_str(m["content"].as_str().unwrap_or("null")).ok()).unwrap_or(Value::Null);
    assert_eq!(again["proposed"], false, "{again}");
    assert_eq!(again["edit_id"], edit_id);

    // Accepted: version 2 serves, minted in the person's name on the proposal.
    let (status, accepted) = call(&app, "POST", &format!("/knowledge/edits/{edit_id}/accept"), None).await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    assert_eq!(accepted["version"], 2);
    let (_, listed) = call(&app, "GET", "/knowledge/sources", None).await;
    let source = listed["sources"].as_array().unwrap().iter().find(|s| s["source_id"] == "hours").cloned().unwrap();
    assert_eq!(source["version"], 2, "{source}");
    assert!(source["author"].as_str().unwrap_or("").contains("accepting agent:desk"), "{source}");
    let (status, twice) = call(&app, "POST", &format!("/knowledge/edits/{edit_id}/accept"), None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{twice}");

    // A new proposal declined with a reason stays on the record as declined.
    *agent.lock().unwrap() = VecDeque::from(vec![
        ChatMessage::assistant_tool_calls(vec![ToolCall::new("p3", "knowledge.propose_edit", json!({"source_id": "hours", "body": "the badge office opens at eight", "why": "a guess"}))]),
        ChatMessage::assistant("Proposed."),
    ]);
    *judge.lock().unwrap() = VecDeque::from(vec![ChatMessage::assistant("{\"verdict\": \"verified\", \"reason\": \"scripted\"}")]);
    let (_, thread) = call(&app, "POST", "/threads", Some(json!({"graph": "react_agent"}))).await;
    let thread_id = thread["thread_id"].as_str().unwrap().to_owned();
    let (_, run) = call(&app, "POST", &format!("/threads/{thread_id}/runs/wait"), Some(json!({"assistant_id": "desk", "input": {MESSAGES_CHANNEL: [{"role": "user", "content": "once more"}]}}))).await;
    let third: Value = run["output"][MESSAGES_CHANNEL].as_array().into_iter().flatten().find(|m| m["role"] == "tool").and_then(|m| serde_json::from_str(m["content"].as_str().unwrap_or("null")).ok()).unwrap_or(Value::Null);
    let third_id = third["edit_id"].as_str().unwrap().to_owned();
    let (status, _) = call(&app, "POST", &format!("/knowledge/edits/{third_id}/decline"), Some(json!({"reason": ""}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, declined) = call(&app, "POST", &format!("/knowledge/edits/{third_id}/decline"), Some(json!({"reason": "a guess is not a source"}))).await;
    assert_eq!(status, StatusCode::OK, "{declined}");
    assert_eq!(declined["edit"]["state"], "declined");
    assert_eq!(declined["edit"]["reason"], "a guess is not a source");
    let (_, listed) = call(&app, "GET", "/knowledge/sources", None).await;
    let source = listed["sources"].as_array().unwrap().iter().find(|s| s["source_id"] == "hours").cloned().unwrap();
    assert_eq!(source["version"], 2, "a decline changes nothing: {source}");

    let _ = std::fs::remove_dir_all(store);
}

/// An agent that finds two sources disagreeing flags the conflict; the
/// same pair flagged again answers the open conflict, not a second one.
#[tokio::test]
async fn an_agent_flags_two_sources_that_disagree() {
    let (app, store, agent, judge) = app();
    for (id, title, body) in [("hours", "Badge office hours", "the badge office opens at ten"), ("poster", "Lobby poster", "the badge office opens at nine")] {
        let (status, made) = call(&app, "POST", "/knowledge/sources", Some(json!({"source_id": id, "kind": "text", "title": title, "author": "human:curator", "body": body}))).await;
        assert_eq!(status, StatusCode::CREATED, "{made}");
    }
    let (status, made) = call(&app, "POST", "/assistants", Some(json!({"assistant_id": "desk", "name": "Desk", "graph": "react_agent", "config": {"instructions": "DESK: help people", "studio_intent": {"instructions": "DESK: help people", "tools": [{"name": "knowledge.flag_conflict"}]}}}))).await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let flag = || vec![
        ChatMessage::assistant_tool_calls(vec![ToolCall::new("f1", "knowledge.flag_conflict", json!({"source_a": "hours", "source_b": "poster", "claim": "when the badge office opens", "a_says": "ten", "b_says": "nine", "why": "a person asked"}))]),
        ChatMessage::assistant("The sources disagree; I flagged it."),
    ];
    let mut ids = Vec::new();
    for _ in 0..2 {
        *agent.lock().unwrap() = VecDeque::from(flag());
        *judge.lock().unwrap() = VecDeque::from(vec![ChatMessage::assistant("{\"verdict\": \"verified\", \"reason\": \"scripted\"}")]);
        let (_, thread) = call(&app, "POST", "/threads", Some(json!({"graph": "react_agent"}))).await;
        let thread_id = thread["thread_id"].as_str().unwrap().to_owned();
        let (status, run) = call(&app, "POST", &format!("/threads/{thread_id}/runs/wait"), Some(json!({"assistant_id": "desk", "input": {MESSAGES_CHANNEL: [{"role": "user", "content": "When does the badge office open?"}]}}))).await;
        assert_eq!(status, StatusCode::OK, "{run}");
        let reply: Value = run["output"][MESSAGES_CHANNEL].as_array().into_iter().flatten().find(|m| m["role"] == "tool").and_then(|m| serde_json::from_str(m["content"].as_str().unwrap_or("null")).ok()).unwrap_or(Value::Null);
        ids.push((reply["flagged"].clone(), reply["conflict_id"].as_str().unwrap_or("").to_owned()));
    }
    assert_eq!(ids[0].0, json!(true), "{ids:?}");
    assert_eq!(ids[1].0, json!(false), "the pair is already flagged: {ids:?}");
    assert_eq!(ids[0].1, ids[1].1);
    let (_, listed) = call(&app, "GET", "/knowledge/conflicts", None).await;
    assert_eq!(listed["open"], 1, "{listed}");
    assert_eq!(listed["conflicts"][0]["filed_by"]["agent_id"], "desk");
    let _ = std::fs::remove_dir_all(store);
}
