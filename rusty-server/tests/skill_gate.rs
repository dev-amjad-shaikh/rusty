//! A skill revision reaches its followers only through the gate (Astra
//! R07, the skill half): registering a new revision holds it while the
//! agents that follow the skill keep running the current one; their suites
//! run against the candidate, pinned to it; a promotion needs that
//! evidence — every follower's suite passed — or an admin's recorded word.
//! Through the real paths: registration, the evaluation the gate starts,
//! the promotion route, a restart.
use std::sync::Arc;
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::error::Result as RustyResult;
use rusty_agent_runtime::llm::{ChatMessage, ChatModel, ChatResponse, Role, ToolCall};
use rusty_agent_runtime::react::{create_react_agent, MESSAGES_CHANNEL};
use rusty_agent_runtime::record::Effect;
use rusty_agent_runtime::state::{Reducer, StateSpec};
use rusty_agent_runtime::tool::{Tool, ToolRegistry};
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

/// Follows the skill it is given: a skill that says LOOKUP FIRST gets a
/// lookup first; any other answers from memory.
struct Obedient;
#[async_trait::async_trait]
impl ChatModel for Obedient {
    async fn chat(&self, messages: &[ChatMessage], _t: &[Value]) -> RustyResult<ChatResponse> {
        let system = messages.iter().filter(|m| m.role == Role::System).filter_map(|m| m.content.clone()).collect::<Vec<_>>().join("\n");
        let looked = messages.iter().any(|m| m.role == Role::Tool);
        let message = if system.contains("LOOKUP FIRST") && !looked {
            ChatMessage::assistant_tool_calls(vec![ToolCall::new("c1", "lookup", json!({}))])
        } else {
            ChatMessage::assistant("there are 168")
        };
        Ok(ChatResponse { message, model: Some("obedient".into()), usage: None })
    }
}

struct Lookup;
#[async_trait::async_trait]
impl Tool for Lookup {
    fn name(&self) -> &str {
        "lookup"
    }
    fn description(&self) -> &str {
        "Counts."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, _args: Value) -> RustyResult<Value> {
        Ok(json!({"count": 168}))
    }
}

fn app_at(store: &std::path::Path) -> Router {
    let mut tools = ToolRegistry::new();
    tools.register(Lookup);
    let graph = create_react_agent(Arc::new(Obedient), tools.clone()).unwrap();
    let spec = StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry.register_with_tools("react_agent", graph, spec, &tools).unwrap();
    // Under a context policy: the skills an agent follows are assembled into
    // every model call (without one the react node reads no skills section).
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.to_path_buf())
        .with_context_policy(rusty_agent_runtime::context::ContextPolicy::standard(8192));
    router(registry, config)
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

fn skill_md(body: &str) -> String {
    format!("---\nname: count-well\ndescription: How to count and answer.\n---\n\n# Count well\n\n{body}\n")
}

async fn register(app: &Router, body: &str) -> Value {
    let (status, r) = call(app, "POST", "/skills", Some(json!({"skill_md": skill_md(body)}))).await;
    assert_eq!(status, StatusCode::CREATED, "{r}");
    r
}

async fn finished(app: &Router, evaluation_id: &str) -> Value {
    for _ in 0..400 {
        let (_, list) = call(app, "GET", "/datasets/desk-counts/versions/1/evaluations", None).await;
        if let Some(done) = list["evaluations"].as_array().and_then(|e| e.iter().find(|x| x["evaluation_id"] == evaluation_id && x["status"] != "running")) {
            return done.clone();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("evaluation {evaluation_id} never finished");
}

/// The followers' own evaluation — no pin — shows which revision they run.
async fn followers_run(app: &Router) -> Value {
    let (status, started) = call(app, "POST", "/datasets/desk-counts/versions/1/evaluations", Some(json!({"assistant_id": "desk"}))).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{started}");
    finished(app, started["evaluation_id"].as_str().unwrap()).await
}

#[tokio::test]
async fn a_skill_revision_reaches_its_followers_only_through_the_gate() {
    let store = std::env::temp_dir().join(format!("rusty-server-skill-gate-{}", uuid::Uuid::new_v4()));
    let app = app_at(&store);

    // The first revision, before anyone follows it: current at once.
    let r = register(&app, "LOOKUP FIRST, then say the number.").await;
    assert_eq!(r["revision"], json!(1));
    assert_eq!(r["held"], json!(false), "{r}");
    assert_eq!(r["current"], json!(1));

    // An agent follows it; one real run becomes the case its suite expects
    // (a lookup — what the skill says).
    let (status, made) = call(&app, "POST", "/assistants", Some(json!({"assistant_id": "desk", "name": "Desk", "graph": "react_agent", "config": {"instructions": "follow your skills", "studio_intent": {"skills": ["count-well"]}}}))).await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let (_, thread) = call(&app, "POST", "/threads", Some(json!({"graph": "react_agent"}))).await;
    let thread_id = thread["thread_id"].as_str().unwrap().to_owned();
    let (status, run) = call(&app, "POST", &format!("/threads/{thread_id}/runs/wait"), Some(json!({"input": {"messages": [{"role": "user", "content": "how many?"}]}, "assistant_id": "desk"}))).await;
    assert_eq!(status, StatusCode::OK, "{run}");
    let run_id = run["run_id"].as_str().unwrap().to_owned();
    let (_, kept) = call(&app, "GET", &format!("/runs/{run_id}"), None).await;
    let (status, dataset) = call(&app, "POST", "/datasets", Some(json!({"name": "desk-counts", "version": "1", "cases": [{
        "id": "counts-via-lookup",
        "input": kept["input"],
        "expect": {"tool_trajectory": [{"name": "lookup"}]},
        "source": {"run_id": run_id, "thread_id": thread_id, "agent_id": "desk", "captured_at": "2026-09-08T10:00:00Z"}
    }]}))).await;
    assert_eq!(status, StatusCode::CREATED, "{dataset}");
    let (_, receipt) = call(&app, "GET", "/skills/count-well", None).await;
    assert_eq!(receipt["current"], json!(1));
    assert_eq!(receipt["candidate"], Value::Null, "{receipt}");

    // A revision that would break the follower: registered, held — the
    // follower's suite runs against it, pinned; the follower keeps running
    // revision 1 meanwhile.
    let r = register(&app, "Answer from what you remember; the number is usually 168.").await;
    assert_eq!(r["revision"], json!(2));
    assert_eq!(r["held"], json!(true), "{r}");
    assert_eq!(r["current"], json!(1));
    let judged = r["gate"][0]["evaluation_id"].as_str().expect("the gate started the follower's suite").to_owned();
    assert_eq!(r["gate"][0]["assistant"], json!("Desk"));
    let (_, receipt) = call(&app, "GET", "/skills/count-well", None).await;
    assert_eq!(receipt["current"], json!(1), "{receipt}");
    assert_eq!(receipt["latest"], json!(2));
    assert_eq!(receipt["candidate"], json!(2));
    let done = finished(&app, &judged).await;
    assert_eq!(done["passed"], json!(0), "the candidate fails the follower's suite: {done}");
    assert_eq!(done["dependencies"]["skills"]["count-well"], json!(2), "the evaluation ran pinned to the candidate: {done}");
    let plain = followers_run(&app).await;
    assert_eq!(plain["passed"], json!(1), "the follower still runs revision 1: {plain}");
    assert_eq!(plain["dependencies"]["skills"]["count-well"], json!(1), "{plain}");

    // The gate refuses the candidate, and says which follower failed.
    let (status, refused) = call(&app, "POST", "/skills/count-well/promote", Some(json!({}))).await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(refused["error"], json!("evidence_required"));
    assert_eq!(refused["evidence"]["suites"][0]["state"], json!("failed"), "{refused}");
    assert!(refused["message"].as_str().unwrap().contains("Desk on desk-counts (failed)"), "{refused}");
    // And why: the case, and the reason in words.
    let why = &refused["evidence"]["suites"][0]["failures"][0];
    assert!(why["case_id"].is_string(), "{refused}");
    assert!(why["said"].as_str().is_some_and(|w| !w.is_empty()), "the gate says why: {refused}");
    let (_, evidence) = call(&app, "GET", "/skills/count-well/evidence", None).await;
    assert_eq!(evidence["current"], json!(1));
    assert_eq!(evidence["evidence"]["revision"], json!(2));
    assert_eq!(evidence["evidence"]["ok"], json!(false));

    // A revision that holds: held, judged, passing — promoted; the follower
    // runs it from then on.
    let r = register(&app, "LOOKUP FIRST — and say the number plainly.").await;
    assert_eq!(r["revision"], json!(3));
    assert_eq!(r["held"], json!(true));
    assert_eq!(r["current"], json!(1), "the current revision stays while the candidate is judged: {r}");
    let done = finished(&app, r["gate"][0]["evaluation_id"].as_str().unwrap()).await;
    assert_eq!(done["passed"], json!(1), "{done}");
    let (_, evidence) = call(&app, "GET", "/skills/count-well/evidence?revision=3", None).await;
    assert_eq!(evidence["evidence"]["ok"], json!(true), "{evidence}");
    let (status, promoted) = call(&app, "POST", "/skills/count-well/promote", Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK, "{promoted}");
    assert_eq!(promoted["promoted"], json!(true));
    assert_eq!(promoted["current"], json!(3));
    let (_, receipt) = call(&app, "GET", "/skills/count-well", None).await;
    assert_eq!(receipt["current"], json!(3));
    assert_eq!(receipt["candidate"], Value::Null);

    // The current revision survives a restart.
    let app = app_at(&store);
    let (_, receipt) = call(&app, "GET", "/skills/count-well", None).await;
    assert_eq!(receipt["current"], json!(3), "the promotion is kept: {receipt}");
    assert_eq!(receipt["latest"], json!(3));

    // A failing revision past the gate only on an admin's word, kept with
    // the evidence it overrode — and then the follower does run it.
    let r = register(&app, "Say 168 from memory.").await;
    assert_eq!(r["revision"], json!(4));
    assert_eq!(r["held"], json!(true));
    assert_eq!(r["current"], json!(3));
    let done = finished(&app, r["gate"][0]["evaluation_id"].as_str().unwrap()).await;
    assert_eq!(done["passed"], json!(0), "{done}");
    let (status, refused) = call(&app, "POST", "/skills/count-well/promote", Some(json!({"revision": 4}))).await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    let (status, forced) = call(&app, "POST", "/skills/count-well/promote", Some(json!({"revision": 4, "override_reason": "the lookup is down this week; answering from memory is what the desk needs"}))).await;
    assert_eq!(status, StatusCode::OK, "{forced}");
    assert_eq!(forced["current"], json!(4));
    let (_, evidence) = call(&app, "GET", "/skills/count-well/evidence", None).await;
    assert_eq!(evidence["current"], json!(4));
    let promotion = &evidence["promotions"][0];
    assert_eq!(promotion["revision"], json!(4));
    assert!(promotion["override_reason"].as_str().unwrap().contains("lookup is down"), "{evidence}");
    assert_eq!(promotion["evidence"]["ok"], json!(false), "the override is kept with the evidence it overrode");
    let plain = followers_run(&app).await;
    assert_eq!(plain["passed"], json!(0), "the follower runs revision 4 now: {plain}");
    assert_eq!(plain["dependencies"]["skills"]["count-well"], json!(4), "{plain}");

    let _ = std::fs::remove_dir_all(store);
}
