//! The approval loop: a run pauses before an irreversible effect, a person
//! decides, and the decision resumes the run — approving executes exactly the
//! call asked about, denying puts the refusal in the model's hands.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::error::{Result as RustyResult, RustyError};
use rusty_agent_runtime::llm::{ChatMessage, ChatModel, ChatResponse, Role, ToolCall};
use rusty_agent_runtime::react::{create_react_agent, DENIED_NOTICE, MESSAGES_CHANNEL};
use rusty_agent_runtime::record::Effect;
use rusty_agent_runtime::state::{Reducer, StateSpec};
use rusty_agent_runtime::tool::{Tool, ToolRegistry};
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

type Seen = Arc<Mutex<Vec<Vec<ChatMessage>>>>;

struct ScriptedModel {
    script: Mutex<VecDeque<ChatMessage>>,
    seen: Seen,
}

#[async_trait::async_trait]
impl ChatModel for ScriptedModel {
    async fn chat(&self, messages: &[ChatMessage], _tools: &[Value]) -> RustyResult<ChatResponse> {
        self.seen.lock().unwrap().push(messages.to_vec());
        let message = self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| RustyError::Llm("script exhausted".into()))?;
        Ok(ChatResponse {
            message,
            model: Some("scripted".into()),
            usage: None,
        })
    }
}

/// An irreversible action: posting. Counts how often it really ran.
struct Post(Arc<AtomicUsize>);

#[async_trait::async_trait]
impl Tool for Post {
    fn name(&self) -> &str {
        "post"
    }
    fn description(&self) -> &str {
        "Posts a message somewhere it cannot be unposted."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {"text": {"type": "string"}}})
    }
    fn effect(&self) -> Effect {
        Effect::NonIdempotent
    }
    async fn call(&self, args: Value) -> RustyResult<Value> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(json!({"posted": args.get("text").cloned().unwrap_or(Value::Null)}))
    }
}

fn app(script: Vec<ChatMessage>) -> (Router, PathBuf, Seen, Arc<AtomicUsize>) {
    let store =
        std::env::temp_dir().join(format!("rusty-server-approvals-{}", uuid::Uuid::new_v4()));
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let posted = Arc::new(AtomicUsize::new(0));
    let model: Arc<dyn ChatModel> = Arc::new(ScriptedModel {
        script: Mutex::new(script.into()),
        seen: Arc::clone(&seen),
    });
    let mut tools = ToolRegistry::new();
    tools.register(Post(Arc::clone(&posted)));
    let graph = create_react_agent(model, tools.clone()).unwrap();
    let spec = StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry
        .register_with_tools("react", graph, spec, &tools)
        .unwrap();
    (
        router(
            registry,
            ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone()),
        ),
        store,
        seen,
        posted,
    )
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
    let response = app
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn wait_terminal(app: &Router, run_id: &str) -> Value {
    for _ in 0..100 {
        let (_, run) = call(app, "GET", &format!("/runs/{run_id}"), None).await;
        if matches!(
            run["status"].as_str(),
            Some("success") | Some("error") | Some("interrupted")
        ) {
            return run;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("run {run_id} never reached a terminal state");
}

fn post_call(id: &str) -> ChatMessage {
    ChatMessage::assistant_tool_calls(vec![ToolCall::new(id, "post", json!({"text": "hello"}))])
}

#[tokio::test]
async fn a_run_pauses_before_an_irreversible_effect_and_approving_resumes_it() {
    let (app, store, _seen, posted) =
        app(vec![post_call("c1"), ChatMessage::assistant("posted it")]);
    let (status, thread) = call(&app, "POST", "/threads", Some(json!({"graph": "react"}))).await;
    assert_eq!(status, StatusCode::CREATED, "{thread}");
    let thread_id = thread["thread_id"].as_str().unwrap().to_owned();

    // The run stops before the post, and says what it wants to do.
    let (_, run) = call(
        &app,
        "POST",
        &format!("/threads/{thread_id}/runs/wait"),
        Some(json!({ "input": { MESSAGES_CHANNEL: [{"role": "user", "content": "post hello"}] } })),
    )
    .await;
    assert_eq!(run["status"], "interrupted", "{run}");
    assert_eq!(run["interrupt"]["kind"], "approval", "{run}");
    assert_eq!(run["interrupt"]["requests"][0]["tool"], "post");
    assert_eq!(
        posted.load(Ordering::SeqCst),
        0,
        "nothing ran before the decision"
    );
    let run_id = run["run_id"].as_str().unwrap().to_owned();

    // A person's own conversation needs no telling: they are in it.
    let (_, told) = call(&app, "GET", "/notices", None).await;
    assert!(
        told["notices"]
            .as_array()
            .map(|n| n
                .iter()
                .all(|x| x["key"] != json!(format!("approval:{run_id}"))))
            .unwrap_or(true),
        "{told}"
    );
    // It is on the list of things needing a decision, with what it asked.
    let (status, list) = call(&app, "GET", "/approvals?status=pending", None).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let pending = list["approvals"].as_array().unwrap();
    assert_eq!(pending.len(), 1, "{list}");
    assert_eq!(pending[0]["run_id"], json!(run_id));
    assert_eq!(pending[0]["requests"][0]["arguments"]["text"], "hello");

    // Approving resumes the run; the post runs exactly once, and the model
    // sees its result and finishes.
    let (status, decided) = call(
        &app,
        "POST",
        &format!("/approvals/{run_id}/decide"),
        Some(json!({"decision": "approve"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{decided}");
    assert_eq!(decided["status"], "approved");
    assert!(decided["decided_by"].is_object(), "{decided}");
    let resumed = decided["resumed_run_id"].as_str().unwrap().to_owned();
    let run = wait_terminal(&app, &resumed).await;
    assert_eq!(run["status"], "success", "{run}");
    assert_eq!(
        posted.load(Ordering::SeqCst),
        1,
        "the approved call ran once"
    );

    // A decision is made once.
    let (status, again) = call(
        &app,
        "POST",
        &format!("/approvals/{run_id}/decide"),
        Some(json!({"decision": "deny"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{again}");
    let (_, list) = call(&app, "GET", "/approvals?status=pending", None).await;
    assert!(list["approvals"].as_array().unwrap().is_empty(), "{list}");

    let _ = std::fs::remove_dir_all(store);
}

/// A decision that stands: approved once with `standing_hours`, the same
/// call from the same agent runs without asking until it expires or is
/// withdrawn; a call that differs in an argument asks.
#[tokio::test]
async fn a_standing_approval_decides_the_same_call_without_asking() {
    let other = ChatMessage::assistant_tool_calls(vec![ToolCall::new(
        "c3",
        "post",
        json!({"text": "goodbye"}),
    )]);
    let (app, store, _seen, posted) = app(vec![
        post_call("c1"),
        ChatMessage::assistant("posted it"),
        post_call("c2"),
        ChatMessage::assistant("posted again"),
        other,
        ChatMessage::assistant("posted goodbye"),
        post_call("c4"),
        ChatMessage::assistant("posted once more"),
    ]);
    async fn pause(app: &Router) -> String {
        let (_, thread) = call(app, "POST", "/threads", Some(json!({"graph": "react"}))).await;
        let thread_id = thread["thread_id"].as_str().unwrap().to_owned();
        let (_, run) = call(
            app,
            "POST",
            &format!("/threads/{thread_id}/runs/wait"),
            Some(json!({ "input": { MESSAGES_CHANNEL: [{"role": "user", "content": "post"}] } })),
        )
        .await;
        assert_eq!(run["status"], "interrupted", "{run}");
        run["run_id"].as_str().unwrap().to_owned()
    }
    // The first pause: approved, and the decision stands for a day.
    let first = pause(&app).await;
    let (status, decided) = call(
        &app,
        "POST",
        &format!("/approvals/{first}/decide"),
        Some(json!({"decision": "approve", "standing_hours": 24})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{decided}");
    assert_eq!(decided["status"], "approved");
    assert_eq!(
        decided["standing"][0]["tool"],
        json!("post"),
        "the decision stands, per request: {decided}"
    );
    assert_eq!(decided["standing"][0]["arguments"]["text"], json!("hello"));
    let standing_id = decided["standing"][0]["id"].as_str().unwrap().to_owned();
    wait_terminal(&app, decided["resumed_run_id"].as_str().unwrap()).await;
    assert_eq!(posted.load(Ordering::SeqCst), 1);

    // The same call again: the pause is decided on the spot, under the
    // person's name, and the Inbox never sees it.
    let second = pause(&app).await;
    let mut decided_by = Value::Null;
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let (_, list) = call(&app, "GET", "/approvals?status=approved", None).await;
        if let Some(a) = list["approvals"]
            .as_array()
            .and_then(|a| a.iter().find(|a| a["run_id"] == json!(second)))
        {
            decided_by = a["decided_by"].clone();
            wait_terminal(&app, a["resumed_run_id"].as_str().unwrap()).await;
            break;
        }
    }
    assert_eq!(
        decided_by["kind"],
        json!("standing"),
        "approved by the standing approval: {decided_by}"
    );
    assert_eq!(decided_by["standing_id"], json!(standing_id));
    assert_eq!(posted.load(Ordering::SeqCst), 2, "the standing call ran");
    let (_, listed) = call(&app, "GET", "/approvals/standing", None).await;
    assert_eq!(listed["standing"][0]["uses"], json!(1), "{listed}");
    assert_eq!(listed["standing"][0]["live"], json!(true));

    // A call that differs in an argument asks.
    let third = pause(&app).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    let (_, list) = call(&app, "GET", "/approvals?status=pending", None).await;
    assert_eq!(
        list["approvals"][0]["run_id"],
        json!(third),
        "goodbye is not hello: {list}"
    );
    let (_, denied) = call(
        &app,
        "POST",
        &format!("/approvals/{third}/decide"),
        Some(json!({"decision": "deny"})),
    )
    .await;
    // The denied run continues (the model hears the refusal); let it end
    // before the next run, so the script is read in order.
    wait_terminal(&app, denied["resumed_run_id"].as_str().unwrap()).await;

    // Withdrawn, the next such call asks again.
    let (status, gone) = call(
        &app,
        "DELETE",
        &format!("/approvals/standing/{standing_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{gone}");
    let fourth = pause(&app).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    let (_, list) = call(&app, "GET", "/approvals?status=pending", None).await;
    assert_eq!(
        list["approvals"][0]["run_id"],
        json!(fourth),
        "asks again: {list}"
    );
    assert_eq!(posted.load(Ordering::SeqCst), 2);
    // A denial does not stand; a day-count out of range is refused.
    let (status, refused) = call(
        &app,
        "POST",
        &format!("/approvals/{fourth}/decide"),
        Some(json!({"decision": "deny", "standing_hours": 2})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    let (status, refused) = call(
        &app,
        "POST",
        &format!("/approvals/{fourth}/decide"),
        Some(json!({"decision": "approve", "standing_hours": 9999})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn denying_puts_the_refusal_in_the_models_hands_and_nothing_runs() {
    let (app, store, seen, posted) = app(vec![
        post_call("c1"),
        ChatMessage::assistant("understood — not posted"),
    ]);
    let (_, thread) = call(&app, "POST", "/threads", Some(json!({"graph": "react"}))).await;
    let thread_id = thread["thread_id"].as_str().unwrap().to_owned();
    let (_, run) = call(
        &app,
        "POST",
        &format!("/threads/{thread_id}/runs/wait"),
        Some(json!({ "input": { MESSAGES_CHANNEL: [{"role": "user", "content": "post hello"}] } })),
    )
    .await;
    assert_eq!(run["status"], "interrupted", "{run}");
    let run_id = run["run_id"].as_str().unwrap().to_owned();

    let (status, decided) = call(
        &app,
        "POST",
        &format!("/approvals/{run_id}/decide"),
        Some(json!({"decision": "deny", "reason": "not to that channel"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{decided}");
    assert_eq!(decided["status"], "denied");
    let resumed = decided["resumed_run_id"].as_str().unwrap().to_owned();
    let run = wait_terminal(&app, &resumed).await;
    assert_eq!(run["status"], "success", "{run}");
    assert_eq!(posted.load(Ordering::SeqCst), 0, "a denied call never runs");

    // What the model was told: the refusal, by whom, and why.
    let last = seen
        .lock()
        .unwrap()
        .last()
        .cloned()
        .expect("the model was called after the decision");
    let tool_message = last
        .iter()
        .rev()
        .find(|m| m.role == Role::Tool)
        .expect("a tool message");
    let text = tool_message.content.clone().unwrap_or_default();
    assert!(text.starts_with(DENIED_NOTICE), "{text}");
    assert!(text.contains("not to that channel"), "{text}");

    let _ = std::fs::remove_dir_all(store);
}

/// The declared counterpart of a run, from its journal's config declaration.
async fn declared_counterpart(app: &Router, run_id: &str) -> Value {
    let (_, events) = call(app, "GET", &format!("/runs/{run_id}/events"), None).await;
    events["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == json!("run_config_declared"))
        .and_then(|e| e["output"]["value"]["counterpart"].as_object().cloned())
        .map(Value::Object)
        .unwrap_or(Value::Null)
}

#[tokio::test]
async fn a_resumed_run_keeps_the_paused_runs_channel_and_person() {
    // A schedule's run: nobody's conversation, before and after the decision.
    let (app, store, _seen, posted) = app(vec![post_call("c1"), ChatMessage::assistant("posted")]);
    // The schedule fires the run through the real scheduler: a client may
    // not say "a schedule fired this" (that is authority the server sets).
    let (status, cron) = call(
        &app,
        "POST",
        "/crons",
        Some(json!({ "graph": "react", "interval_secs": 1, "input": { MESSAGES_CHANNEL: [{"role": "user", "content": "post hello"}] } })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{cron}");
    let cron_id = cron["cron_id"].as_str().unwrap().to_owned();
    let mut paused: Option<Value> = None;
    for _ in 0..80 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let (_, runs) = call(&app, "GET", "/runs", None).await;
        let hit = runs
            .as_array()
            .into_iter()
            .flatten()
            .find(|r| {
                r["metadata"]["cron_id"] == json!(cron_id) && r["status"] == json!("interrupted")
            })
            .cloned();
        if hit.is_some() {
            paused = hit;
            break;
        }
    }
    let _ = call(&app, "DELETE", &format!("/crons/{cron_id}"), None).await;
    let run = paused.unwrap_or_else(|| panic!("the schedule never fired a run that paused"));
    assert_eq!(run["status"], "interrupted");
    let run_id = run["run_id"].as_str().unwrap().to_owned();
    assert_eq!(
        declared_counterpart(&app, &run_id).await,
        json!({"nobody": true}),
        "a schedule's run is nobody's"
    );
    // Nobody was sitting in it: the person who set the schedule going is
    // told, once, that it waits on them.
    let (status, told) = call(&app, "GET", "/notices", None).await;
    assert_eq!(status, StatusCode::OK, "{told}");
    let notices = told["notices"].as_array().cloned().unwrap_or_default();
    let mine = notices
        .iter()
        .find(|n| n["key"] == json!(format!("approval:{run_id}")))
        .unwrap_or_else(|| panic!("the schedule's creator is told: {told}"));
    assert_eq!(mine["about"]["kind"], json!("approval"));
    assert_eq!(mine["about"]["channel"], json!("schedule"));
    assert!(
        mine["title"]
            .as_str()
            .unwrap()
            .contains("paused before post"),
        "{mine}"
    );
    assert!(
        mine["text"]
            .as_str()
            .unwrap()
            .contains("nobody is sitting in"),
        "{mine}"
    );

    let (status, decided) = call(
        &app,
        "POST",
        &format!("/approvals/{run_id}/decide"),
        Some(json!({"decision": "approve"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{decided}");
    let resumed = decided["resumed_run_id"].as_str().unwrap().to_owned();
    let done = wait_terminal(&app, &resumed).await;
    assert_eq!(done["status"], "success", "{done}");
    assert_eq!(posted.load(Ordering::SeqCst), 1);
    // The resumed run carries the schedule's identity and stays nobody's.
    assert_eq!(done["metadata"]["channel"], json!("schedule"), "{done}");
    assert_eq!(done["metadata"]["cron_id"], json!(cron_id));
    assert_eq!(done["metadata"]["approval_of"], json!(run_id));
    assert_eq!(
        declared_counterpart(&app, &resumed).await,
        json!({"nobody": true}),
        "the decider is not the counterpart"
    );
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn a_resumed_run_stays_on_behalf_of_whoever_started_it() {
    let (app, store, _seen, _posted) = app(vec![post_call("c1"), ChatMessage::assistant("posted")]);
    let (_, t) = call(&app, "POST", "/threads", Some(json!({"graph": "react"}))).await;
    let thread_id = t["thread_id"].as_str().unwrap().to_owned();
    let (_, run) = call(
        &app,
        "POST",
        &format!("/threads/{thread_id}/runs/wait"),
        Some(json!({ "input": { MESSAGES_CHANNEL: [{"role": "user", "content": "post hello"}] } })),
    )
    .await;
    let run_id = run["run_id"].as_str().unwrap().to_owned();
    let (_, original) = call(&app, "GET", &format!("/runs/{run_id}"), None).await;
    let started_by = original["metadata"]["created_by"].clone();
    assert!(started_by.is_object(), "{original}");
    let (_, decided) = call(
        &app,
        "POST",
        &format!("/approvals/{run_id}/decide"),
        Some(json!({"decision": "approve"})),
    )
    .await;
    let resumed = decided["resumed_run_id"].as_str().unwrap().to_owned();
    let done = wait_terminal(&app, &resumed).await;
    // Whoever decided, the resumed run is on behalf of the person who started it.
    assert_eq!(done["metadata"]["on_behalf_of"], started_by, "{done}");
    assert_eq!(
        declared_counterpart(&app, &resumed).await,
        declared_counterpart(&app, &run_id).await
    );
    let _ = std::fs::remove_dir_all(store);
}

/// What a run declared it acted in and under, from its journal.
async fn declared(app: &Router, run_id: &str) -> Value {
    let (_, events) = call(app, "GET", &format!("/runs/{run_id}/events"), None).await;
    events["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == json!("run_config_declared"))
        .map(|e| e["output"]["value"].clone())
        .unwrap_or(Value::Null)
}

/// A run paused under a configuration continues under it after the
/// decision: the allowlist here, and by the same carry the world a run was
/// put in (proven with a live world in `tests/worlds.rs` and the handover),
/// so an approved effect lands where the run was put, never in the live
/// system a world stands in for.
#[tokio::test]
async fn a_resumed_run_continues_under_the_configuration_it_paused_under() {
    let (app, store, _seen, posted) = app(vec![post_call("c1"), ChatMessage::assistant("posted")]);
    let (_, thread) = call(&app, "POST", "/threads", Some(json!({"graph": "react"}))).await;
    let thread_id = thread["thread_id"].as_str().unwrap().to_owned();
    let (_, run) = call(
        &app,
        "POST",
        &format!("/threads/{thread_id}/runs/wait"),
        Some(json!({ "input": { MESSAGES_CHANNEL: [{"role": "user", "content": "post hello"}] }, "config": { "tool_allowlist": ["post"] } })),
    )
    .await;
    assert_eq!(run["status"], "interrupted", "{run}");
    let run_id = run["run_id"].as_str().unwrap().to_owned();
    let paused = declared(&app, &run_id).await;
    assert_eq!(paused["tool_allowlist"], json!(["post"]), "{paused}");

    let (status, decided) = call(
        &app,
        "POST",
        &format!("/approvals/{run_id}/decide"),
        Some(json!({"decision": "approve"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{decided}");
    let resumed = decided["resumed_run_id"].as_str().unwrap().to_owned();
    let done = wait_terminal(&app, &resumed).await;
    assert_eq!(done["status"], "success", "{done}");
    assert_eq!(posted.load(Ordering::SeqCst), 1);
    let continued = declared(&app, &resumed).await;
    assert_eq!(
        continued["tool_allowlist"],
        json!(["post"]),
        "the resumed run continues under the paused run's configuration: {continued}"
    );
    let _ = std::fs::remove_dir_all(store);
}

/// The person deciding sees who asks and where the effect lands: the agent
/// by name, and the world the run was put in — by name, with the host it
/// stands in for. A run in no world says nothing of one.
#[tokio::test]
async fn an_approval_names_the_agent_and_the_world_the_effect_lands_in() {
    let (app, store, _seen, _posted) = app(vec![
        post_call("c1"),
        post_call("c2"),
        ChatMessage::assistant("posted"),
    ]);
    let (status, made) = call(
        &app,
        "POST",
        "/assistants",
        Some(json!({"assistant_id": "poster", "name": "Board Poster", "graph": "react"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let (status, world) = call(&app, "POST", "/worlds", Some(json!({"name": "board-twin", "connector": "board", "stands_for": "board.example.test", "dialect": "ledger-api"}))).await;
    assert_eq!(status, StatusCode::CREATED, "{world}");
    let world_id = world["world_id"].as_str().unwrap().to_owned();

    let (_, thread) = call(&app, "POST", "/threads", Some(json!({"graph": "react"}))).await;
    let thread_id = thread["thread_id"].as_str().unwrap().to_owned();
    let (_, run) = call(
        &app,
        "POST",
        &format!("/threads/{thread_id}/runs/wait"),
        Some(json!({ "assistant_id": "poster", "input": { MESSAGES_CHANNEL: [{"role": "user", "content": "post hello"}] }, "config": { "world": "board-twin" } })),
    )
    .await;
    assert_eq!(run["status"], "interrupted", "{run}");
    let run_id = run["run_id"].as_str().unwrap().to_owned();

    let (_, list) = call(&app, "GET", "/approvals?status=pending", None).await;
    let pending = list["approvals"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["run_id"] == json!(run_id))
        .cloned()
        .expect("the paused run");
    assert_eq!(pending["agent_name"], json!("Board Poster"), "{pending}");
    assert_eq!(pending["world"]["name"], json!("board-twin"), "{pending}");
    assert_eq!(pending["world"]["world_id"], json!(world_id));
    assert_eq!(pending["world"]["stands_for"], json!("board.example.test"));

    // The world removed while the decision waits: the card says so.
    let (status, _) = call(&app, "DELETE", &format!("/worlds/{world_id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    let (_, list) = call(&app, "GET", "/approvals?status=pending", None).await;
    let pending = list["approvals"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["run_id"] == json!(run_id))
        .cloned()
        .unwrap();
    assert_eq!(pending["world"]["gone"], json!(true), "{pending}");

    // A run in no world: no world said.
    let (_, thread) = call(&app, "POST", "/threads", Some(json!({"graph": "react"}))).await;
    let thread_id = thread["thread_id"].as_str().unwrap().to_owned();
    let (_, run) = call(
        &app,
        "POST",
        &format!("/threads/{thread_id}/runs/wait"),
        Some(json!({ "input": { MESSAGES_CHANNEL: [{"role": "user", "content": "post hello"}] } })),
    )
    .await;
    assert_eq!(run["status"], "interrupted", "{run}");
    let (_, list) = call(&app, "GET", "/approvals?status=pending", None).await;
    let live = list["approvals"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["run_id"] == run["run_id"])
        .cloned()
        .unwrap();
    assert!(live.get("world").is_none(), "{live}");
    let _ = std::fs::remove_dir_all(store);
}

/// A queued task's run pauses at the gate: the person who queued it is
/// the one told, and the run is attributed to them.
#[tokio::test]
async fn a_queued_tasks_paused_run_tells_the_person_who_queued_it() {
    let (app, store, _seen, posted) = app(vec![post_call("c1"), ChatMessage::assistant("posted")]);
    let (status, made) = call(&app, "POST", "/assistants", Some(json!({"assistant_id": "poster", "name": "Board Poster", "graph": "react", "config": {"studio_intent": {"instructions": "Post what you are told.", "tools": [{"name": "post"}], "pool": "posts"}}}))).await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let (status, queued) = call(
        &app,
        "POST",
        "/tasks",
        Some(json!({"kind": "post", "pool": "posts", "payload": {"message": "post hello"}})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{queued}");
    let task_id = queued["task_id"].as_str().unwrap().to_owned();
    let (_, kept) = call(&app, "GET", &format!("/tasks/{task_id}"), None).await;
    assert!(
        kept["payload"]["enqueued_by"]["principal_id"].is_string(),
        "who queued it rides with the work: {kept}"
    );

    let mut paused: Option<Value> = None;
    // The pool worker claims on its own poll; give it a minute.
    for _ in 0..600 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let (_, runs) = call(&app, "GET", "/runs", None).await;
        let hit = runs
            .as_array()
            .into_iter()
            .flatten()
            .find(|r| {
                r["metadata"]["channel"] == json!("pool") && r["status"] == json!("interrupted")
            })
            .cloned();
        if hit.is_some() {
            paused = hit;
            break;
        }
    }
    let (_, task_now) = call(&app, "GET", &format!("/tasks/{task_id}"), None).await;
    let (_, runs_now) = call(&app, "GET", "/runs", None).await;
    let run = paused.unwrap_or_else(|| {
        panic!(
            "the pool never worked the task into a pause; task: {task_now}; runs: {}",
            runs_now
                .as_array()
                .map(|r| r
                    .iter()
                    .map(|x| format!(
                        "{}:{}:{}",
                        x["run_id"].as_str().unwrap_or("?").get(..8).unwrap_or("?"),
                        x["status"],
                        x["metadata"]["channel"]
                    ))
                    .collect::<Vec<_>>()
                    .join(", "))
                .unwrap_or_default()
        )
    });
    let run_id = run["run_id"].as_str().unwrap().to_owned();
    assert!(
        run["metadata"]["created_by"]["principal_id"].is_string(),
        "the run is attributed to the person who queued it: {run}"
    );
    let (_, told) = call(&app, "GET", "/notices", None).await;
    let mine = told["notices"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|n| n["key"] == json!(format!("approval:{run_id}")))
        .cloned()
        .unwrap_or_else(|| panic!("the person who queued it is told: {told}"));
    assert_eq!(mine["about"]["channel"], json!("pool"), "{mine}");
    assert!(
        mine["title"]
            .as_str()
            .unwrap()
            .contains("Board Poster paused before post"),
        "{mine}"
    );
    assert_eq!(posted.load(Ordering::SeqCst), 0);
    // The task waits, leased, for the decision — it is neither failed nor
    // retried — and settles on the run the decision continued.
    assert_eq!(task_now["status"], json!("leased"), "{task_now}");
    let (status, decided) = call(
        &app,
        "POST",
        &format!("/approvals/{run_id}/decide"),
        Some(json!({"decision": "approve"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{decided}");
    let resumed = decided["resumed_run_id"].as_str().unwrap().to_owned();
    let mut settled: Option<Value> = None;
    for _ in 0..300 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let (_, t) = call(&app, "GET", &format!("/tasks/{task_id}"), None).await;
        if t["status"] == json!("completed") || t["status"] == json!("failed") {
            settled = Some(t);
            break;
        }
    }
    let done = settled.expect("the task settles once decided");
    assert_eq!(done["status"], json!("completed"), "{done}");
    assert_eq!(
        done["result"]["run_id"],
        json!(resumed),
        "settled on the run the decision continued: {done}"
    );
    assert_eq!(done["result"]["first_run_id"], json!(run_id));
    assert!(
        done["result"]["reply"]
            .as_str()
            .unwrap_or("")
            .contains("posted"),
        "{done}"
    );
    assert_eq!(
        posted.load(Ordering::SeqCst),
        1,
        "the approved post ran once"
    );
    let _ = std::fs::remove_dir_all(store);
}
