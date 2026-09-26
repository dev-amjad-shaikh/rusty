//! The verifier's own suite: a person says whether a run's verdict was
//! right; judging the verifier runs every reviewed run's kept transcript
//! past the judge again and counts where it agrees with the person.
//! Through the real paths: a verified run, its review, the judging.
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
use rusty_agent_runtime::record::Effect;
use rusty_agent_runtime::state::{Reducer, StateSpec};
use rusty_agent_runtime::tool::{Tool, ToolRegistry};
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
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

struct Echo;

#[async_trait]
impl Tool for Echo {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "Echoes its input text."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {"text": {"type": "string"}}})
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, args: Value) -> RustyResult<Value> {
        Ok(args.get("text").cloned().unwrap_or(Value::Null))
    }
}

fn app() -> (Router, PathBuf, Script, Script) {
    let store = std::env::temp_dir().join(format!("rusty-server-verifier-suite-{}", uuid::Uuid::new_v4()));
    let agent: Script = Arc::new(Mutex::new(VecDeque::new()));
    let judge: Script = Arc::new(Mutex::new(VecDeque::new()));
    let mut tools = ToolRegistry::new();
    tools.register(Echo);
    let graph = create_react_agent(Arc::new(Scripted(Arc::clone(&agent))), tools.clone()).unwrap();
    let spec = StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry.register_with_tools("react", graph, spec, &tools).unwrap();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone()).with_verifier(Arc::new(Scripted(Arc::clone(&judge))));
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

fn judge_line(verdict: &str) -> ChatMessage {
    ChatMessage::assistant(format!("{{\"verdict\": \"{verdict}\", \"reason\": \"scripted\"}}"))
}

/// One run: echo, then a reply; the judge says `verdict`.
async fn one_run(app: &Router, agent: &Script, judge: &Script, verdict: &str) -> Value {
    run_as(app, agent, judge, verdict, None).await
}

/// The same, as `assistant` when one is named.
async fn run_as(app: &Router, agent: &Script, judge: &Script, verdict: &str, assistant: Option<&str>) -> Value {
    // A failed verdict sends the model back for one repair turn, and the
    // judge reads that turn too: both scripts carry the extra line.
    *agent.lock().unwrap() = VecDeque::from(vec![
        ChatMessage::assistant_tool_calls(vec![ToolCall::new("c1", "echo", json!({"text": "hello"}))]),
        ChatMessage::assistant("The echo said: hello."),
        ChatMessage::assistant("The echo said: hello."),
    ]);
    *judge.lock().unwrap() = VecDeque::from(vec![judge_line(verdict), judge_line(verdict)]);
    let (_, thread) = call(app, "POST", "/threads", Some(json!({"graph": "react"}))).await;
    let thread_id = thread["thread_id"].as_str().unwrap().to_owned();
    let mut body = json!({"input": {MESSAGES_CHANNEL: [
        {"role": "system", "content": "CHARTER: call echo, then report what it said."},
        {"role": "user", "content": "say hello"}
    ]}});
    if let Some(assistant) = assistant {
        body["assistant_id"] = json!(assistant);
    }
    let (status, run) = call(app, "POST", &format!("/threads/{thread_id}/runs/wait"), Some(body)).await;
    assert_eq!(status, StatusCode::OK, "{run}");
    assert_eq!(run["verification"]["verdict"], verdict, "{run}");
    run
}

async fn judged(app: &Router) -> Value {
    for _ in 0..200 {
        let (_, ev) = call(app, "GET", "/verifier/evidence", None).await;
        if ev["latest"]["status"] == "done" {
            return ev;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the judging never finished");
}

#[tokio::test]
async fn a_persons_word_on_a_verdict_is_a_case_and_the_judge_is_measured_against_it() {
    let (app, store, agent, judge) = app();
    // Nothing reviewed yet: nothing to judge, and the evidence says so.
    let (_, ev) = call(&app, "GET", "/verifier/evidence", None).await;
    assert_eq!(ev["reviews"], 0, "{ev}");
    let (status, refused) = call(&app, "POST", "/verifier/evaluations", None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");

    // Two runs: one the judge verified, one it failed. The served verdict
    // carries no transcript; the kept one does.
    let good = one_run(&app, &agent, &judge, "verified").await;
    let bad = one_run(&app, &agent, &judge, "failed").await;
    assert!(good["verification"].get("transcript").is_none(), "the served verdict leaves the transcript out: {}", good["verification"]);

    // A person agrees with the first and calls the second wrong.
    let (status, r) = call(&app, "POST", &format!("/runs/{}/verdict/review", good["run_id"].as_str().unwrap()), Some(json!({"agree": true}))).await;
    assert_eq!(status, StatusCode::OK, "{r}");
    assert_eq!(r["review"]["verdict_right"], "verified");
    let (status, r) = call(&app, "POST", &format!("/runs/{}/verdict/review", bad["run_id"].as_str().unwrap()), Some(json!({"agree": false, "note": "the echo did say hello"}))).await;
    assert_eq!(status, StatusCode::OK, "{r}");
    assert_eq!(r["review"]["verdict_right"], "verified", "wrong defaults to the other verdict: {r}");
    assert_eq!(r["reviews"], 2);
    let (status, r) = call(&app, "POST", "/runs/no-such-run/verdict/review", Some(json!({"agree": true}))).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{r}");

    // Judged again with a judge that verifies everything: it agrees with
    // the person on both — the second verdict was the judge's slip.
    *judge.lock().unwrap() = VecDeque::from(vec![judge_line("verified"), judge_line("verified")]);
    let (status, started) = call(&app, "POST", "/verifier/evaluations", None).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{started}");
    assert_eq!(started["total"], 2);
    let ev = judged(&app).await;
    assert_eq!(ev["latest"]["agreed"], 2, "{}", ev["latest"]);
    assert_eq!(ev["latest"]["cases"][0]["asked"], "say hello", "{}", ev["latest"]["cases"][0]);
    assert_eq!(ev["reviews"], 2);
    assert_eq!(ev["disagreed"], 1);

    // And with a judge that fails everything: it agrees on neither, and
    // each case says what the person said and what the judge says now.
    *judge.lock().unwrap() = VecDeque::from(vec![judge_line("failed"), judge_line("failed")]);
    let (status, _) = call(&app, "POST", "/verifier/evaluations", None).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let ev = judged(&app).await;
    assert_eq!(ev["latest"]["agreed"], 0, "{}", ev["latest"]);
    assert_eq!(ev["latest"]["judged"], 2);
    let case = &ev["latest"]["cases"][0];
    assert_eq!(case["person"], "verified");
    assert_eq!(case["now"], "failed");
    assert_eq!(case["agrees"], false);

    // A judge that gives no verdict (nothing it could parse) is not a
    // disagreement: the case is skipped and left out of the ratio.
    *judge.lock().unwrap() = VecDeque::from(vec![ChatMessage::assistant("I cannot say."), judge_line("verified")]);
    let (status, _) = call(&app, "POST", "/verifier/evaluations", None).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let ev = judged(&app).await;
    assert_eq!(ev["latest"]["judged"], 1, "{}", ev["latest"]);
    assert_eq!(ev["latest"]["agreed"], 1);
    assert!(ev["latest"]["cases"][0]["skipped"].as_str().is_some_and(|s| s.contains("no verdict")), "{}", ev["latest"]["cases"][0]);

    let _ = std::fs::remove_dir_all(store);
}

async fn review(app: &Router, id: String) {
    let (status, r) = call(app, "POST", &format!("/runs/{id}/verdict/review"), Some(json!({"agree": true}))).await;
    assert_eq!(status, StatusCode::OK, "{r}");
}

/// Judge `version_id` by hand and wait for its evaluation: the case's run
/// takes the desk's script, and the judge's verdict on it.
async fn judge_candidate(app: &Router, agent: &Script, judge: &Script, version_id: String) {
    *agent.lock().unwrap() = VecDeque::from(vec![
        ChatMessage::assistant_tool_calls(vec![ToolCall::new("c1", "echo", json!({"text": "hello"}))]),
        ChatMessage::assistant("The echo said: hello."),
    ]);
    *judge.lock().unwrap() = VecDeque::from(vec![judge_line("verified")]);
    let (status, judged) = call(app, "POST", &format!("/assistants/desk/versions/{version_id}/evidence"), None).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{judged}");
    for _ in 0..400 {
        let (_, list) = call(app, "GET", "/datasets/desk-echo/versions/1/evaluations", None).await;
        if list["evaluations"].as_array().is_some_and(|e| e.iter().any(|x| x["assistant_version_id"] == version_id && x["status"] == "done")) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the candidate's evaluation never finished");
}

/// Under the `auto` policy a candidate every suite passed activates by
/// itself — only once the verifier's evidence stands: five verdicts
/// reviewed and the judge agreeing with people on nine in ten.
#[tokio::test]
async fn a_candidate_auto_applies_only_when_the_gate_passes_and_the_verifier_stands() {
    let (app, store, agent, judge) = app();
    let (status, made) = call(&app, "POST", "/assistants", Some(json!({"assistant_id": "desk", "name": "Desk", "graph": "react", "config": {"instructions": "CHARTER: call echo, then report what it said.", "studio_intent": {"promotion": "auto"}}}))).await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let v1 = made["active_version_id"].as_str().unwrap().to_owned();

    // A suite recorded from the desk: one run becomes a case that expects the echo.
    let run = run_as(&app, &agent, &judge, "verified", Some("desk")).await;
    let (_, kept) = call(&app, "GET", &format!("/runs/{}", run["run_id"].as_str().unwrap()), None).await;
    let (status, dataset) = call(&app, "POST", "/datasets", Some(json!({"name": "desk-echo", "version": "1", "cases": [{
        "id": "echo-first",
        "input": kept["input"],
        "expect": {"tool_trajectory": [{"name": "echo"}]},
        "source": {"run_id": run["run_id"], "thread_id": run["thread_id"], "agent_id": "desk", "captured_at": "2026-09-23T10:00:00Z"}
    }]}))).await;
    assert_eq!(status, StatusCode::CREATED, "{dataset}");

    // The verifier's evidence does not stand yet (one review short of five):
    // a candidate the gate passes waits for a person.
    review(&app, run["run_id"].as_str().unwrap().to_owned()).await;
    for _ in 0..3 {
        let r = one_run(&app, &agent, &judge, "verified").await;
        review(&app, r["run_id"].as_str().unwrap().to_owned()).await;
    }
    let (_, ev) = call(&app, "GET", "/verifier/evidence", None).await;
    assert_eq!(ev["floor"]["stands"], false, "{}", ev["floor"]);
    assert_eq!(ev["floor"]["reviews"], 4);

    let (status, made) = call(&app, "POST", "/assistants/desk/versions", Some(json!({"base_version_id": v1, "name": "Desk", "graph": "react", "config": {"instructions": "CHARTER: call echo, then report what it said, briefly.", "studio_intent": {"promotion": "auto"}}}))).await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let v2 = made["version"]["version_id"].as_str().unwrap().to_owned();
    judge_candidate(&app, &agent, &judge, v2.clone()).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let (_, a) = call(&app, "GET", "/assistants/desk", None).await;
    assert_eq!(a["active_version_id"], json!(v1), "held for a person: the verifier has four reviews, not five: {a}");
    let (_, evidence) = call(&app, "GET", &format!("/assistants/desk/versions/{v2}/evidence"), None).await;
    assert_eq!(evidence["evidence"]["ok"], true, "the gate itself passed: {evidence}");

    // The fifth review, and a judging that agrees with everyone: the floor stands.
    let r = one_run(&app, &agent, &judge, "verified").await;
    review(&app, r["run_id"].as_str().unwrap().to_owned()).await;
    *judge.lock().unwrap() = VecDeque::from((0..5).map(|_| judge_line("verified")).collect::<Vec<_>>());
    let (status, _) = call(&app, "POST", "/verifier/evaluations", None).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let ev = judged(&app).await;
    assert_eq!(ev["floor"]["stands"], true, "{}", ev["floor"]);

    // The next candidate the gate passes activates by itself, recorded by the gate.
    let (status, made) = call(&app, "POST", "/assistants/desk/versions", Some(json!({"base_version_id": v1, "name": "Desk", "graph": "react", "config": {"instructions": "CHARTER: call echo, then report what it said, in one line.", "studio_intent": {"promotion": "auto"}}}))).await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let v3 = made["version"]["version_id"].as_str().unwrap().to_owned();
    judge_candidate(&app, &agent, &judge, v3.clone()).await;
    let mut active = Value::Null;
    for _ in 0..100 {
        let (_, a) = call(&app, "GET", "/assistants/desk", None).await;
        active = a["active_version_id"].clone();
        if active == json!(v3) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(active, json!(v3), "the candidate gate activated it");
    let (_, evidence) = call(&app, "GET", &format!("/assistants/desk/versions/{v3}/evidence"), None).await;
    assert_eq!(evidence["promotions"][0]["by"]["principal_id"], "candidate-gate", "{}", evidence["promotions"]);
    assert_eq!(evidence["promotions"][0]["by"]["floor"]["stands"], true);

    let _ = std::fs::remove_dir_all(store);
}
