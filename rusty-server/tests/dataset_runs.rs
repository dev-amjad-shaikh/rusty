//! Running a dataset version against an agent: every case becomes a real
//! run of the agent on a fresh thread, judged against the case's
//! expectation; the verdicts are stored and read back as the cases finish.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::prelude::*;
use rusty_agent_runtime::tool::builtins::CalculatorTool;
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

/// Calls the calculator once per run, then answers.
struct CalculatingModel {
    calls: AtomicUsize,
}

#[async_trait]
impl ChatModel for CalculatingModel {
    async fn chat(&self, messages: &[ChatMessage], _tools: &[Value]) -> Result<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let answered_tool = messages.iter().any(|m| m.role == Role::Tool);
        let message = if answered_tool {
            ChatMessage::assistant("42")
        } else {
            ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                "call-1",
                "calculator",
                json!({"operation": "multiply", "left": 6, "right": 7}),
            )])
        };
        Ok(ChatResponse {
            message,
            model: Some("calc-test".into()),
            usage: None,
        })
    }
}

/// Never answers: a run on it is still running whenever the server dies.
struct HangingModel;

#[async_trait]
impl ChatModel for HangingModel {
    async fn chat(&self, _messages: &[ChatMessage], _tools: &[Value]) -> Result<ChatResponse> {
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        Ok(ChatResponse {
            message: ChatMessage::assistant("never"),
            model: None,
            usage: None,
        })
    }
}

fn test_app() -> (Router, PathBuf) {
    let store = std::env::temp_dir().join(format!("rusty-dataset-runs-{}", uuid::Uuid::new_v4()));
    (test_app_at(store.clone(), false), store)
}

/// A judge that reads the rubric it was given: a rubric naming "42" scores
/// a reply that says 42; any other rubric scores zero and says why.
struct RubricJudge;

#[async_trait]
impl ChatModel for RubricJudge {
    async fn chat(&self, messages: &[ChatMessage], _tools: &[Value]) -> Result<ChatResponse> {
        let system = messages
            .iter()
            .find(|m| m.role == Role::System)
            .and_then(|m| m.content.clone())
            .unwrap_or_default();
        let user = messages
            .iter()
            .find(|m| m.role == Role::User)
            .and_then(|m| m.content.clone())
            .unwrap_or_default();
        let (score, rationale) = if system.contains("42") && user.contains("42") {
            (1.0, "The reply gives 42 as the rubric asks.")
        } else {
            (0.0, "The reply does not do what the rubric asks.")
        };
        Ok(ChatResponse {
            message: ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                "j-1",
                "submit_judgment",
                json!({"score": score, "rationale": rationale}),
            )]),
            model: Some("rubric-judge".into()),
            usage: None,
        })
    }
}

fn judged_app() -> (Router, PathBuf) {
    let store = std::env::temp_dir().join(format!("rusty-dataset-runs-{}", uuid::Uuid::new_v4()));
    let mut tools = ToolRegistry::new();
    tools.register(CalculatorTool);
    let graph = create_react_agent(
        Arc::new(CalculatingModel {
            calls: AtomicUsize::new(0),
        }),
        tools.clone(),
    )
    .unwrap();
    let spec = StateSpec::new().channel("messages", Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry
        .register_with_tools("capable", graph, spec, &tools)
        .unwrap();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_verifier(Arc::new(RubricJudge));
    (router(registry, config), store)
}

/// The app over a store root; `hanging` swaps in a model that never answers.
fn test_app_at(store: PathBuf, hanging: bool) -> Router {
    let mut tools = ToolRegistry::new();
    tools.register(CalculatorTool);
    let model: Arc<dyn ChatModel> = if hanging {
        Arc::new(HangingModel)
    } else {
        Arc::new(CalculatingModel {
            calls: AtomicUsize::new(0),
        })
    };
    let graph = create_react_agent(model, tools.clone()).unwrap();
    let spec = StateSpec::new().channel("messages", Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry
        .register_with_tools("capable", graph, spec, &tools)
        .unwrap();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store);
    router(registry, config)
}

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(value) => {
            builder = builder.header("content-type", "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes: Bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

fn input() -> Value {
    json!({"messages": [{"role": "user", "content": "what is 6 times 7?"}]})
}

#[tokio::test]
async fn a_dataset_runs_against_an_agent_as_real_runs_and_every_case_is_judged() {
    let (app, store) = test_app();
    let (status, v) = call(
        &app,
        "POST",
        "/assistants",
        Some(json!({
            "assistant_id": "calc",
            "name": "Calc",
            "graph": "capable",
            "config": {"studio_intent": {"instructions": "Multiply, then answer.", "tools": [{"name": "calculator"}]}},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{v}");

    // A recorded run is the source of the cases.
    let (_, v) = call(&app, "POST", "/threads", Some(json!({"graph": "capable"}))).await;
    let thread = v["thread_id"].as_str().unwrap().to_string();
    let (status, terminal) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/runs/wait"),
        Some(json!({"assistant_id": "calc", "input": input()})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{terminal}");
    let run_id = terminal["run_id"].as_str().unwrap().to_string();
    let (_, detail) = call(&app, "GET", &format!("/runs/{run_id}"), None).await;
    let accepted = detail["input"].clone();
    let source = json!({"run_id": run_id, "thread_id": thread, "agent_id": "calc", "captured_at": "2026-01-01T00:00:00Z"});
    let (status, v) = call(
        &app,
        "POST",
        "/datasets",
        Some(json!({
            "name": "calc",
            "version": "v1",
            "cases": [
                {"id": "multiplies", "input": accepted, "expect": {"tool_trajectory": [{"name": "calculator"}]}, "tags": ["math"], "source": source},
                {"id": "no-tools", "input": accepted, "expect": {"forbid_tools": ["calculator"]}, "tags": [], "source": source}
            ]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{v}");

    // Nothing has run yet.
    let (status, v) = call(&app, "GET", "/datasets/calc/versions/v1/evaluations", None).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["evaluations"], json!([]));

    // An unknown agent is refused; a real one starts at once.
    let (status, _) = call(
        &app,
        "POST",
        "/datasets/calc/versions/v1/evaluations",
        Some(json!({"assistant_id": "nobody"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, started) = call(
        &app,
        "POST",
        "/datasets/calc/versions/v1/evaluations",
        Some(json!({"assistant_id": "calc"})),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{started}");
    assert_eq!(started["status"], json!("running"));
    assert_eq!(started["total"], json!(2));
    let evaluation_id = started["evaluation_id"].as_str().unwrap().to_string();

    // The cases finish one by one; the listing shows the verdicts.
    let mut finished = Value::Null;
    for _ in 0..200 {
        let (_, v) = call(&app, "GET", "/datasets/calc/versions/v1/evaluations", None).await;
        let ev = v["evaluations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["evaluation_id"] == json!(evaluation_id))
            .cloned()
            .expect("listed");
        if ev["status"] == json!("done") {
            finished = ev;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(
        finished["status"],
        json!("done"),
        "the evaluation finished: {finished}"
    );
    assert_eq!(finished["passed"], json!(1), "{finished}");
    assert_eq!(finished["total"], json!(2));
    let cases = finished["cases"].as_array().unwrap();
    let multiplies = cases
        .iter()
        .find(|c| c["case_id"] == json!("multiplies"))
        .unwrap();
    assert_eq!(multiplies["passed"], json!(true), "{multiplies}");
    assert_eq!(multiplies["status"], json!("done"));
    assert_eq!(multiplies["tool_calls"], json!(["calculator"]));
    let no_tools = cases
        .iter()
        .find(|c| c["case_id"] == json!("no-tools"))
        .unwrap();
    assert_eq!(no_tools["passed"], json!(false), "{no_tools}");
    let failed = no_tools["assertions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["passed"] == json!(false))
        .expect("a failed assertion explains it");
    assert!(
        failed["detail"]
            .as_str()
            .is_some_and(|d| d.contains("calculator")),
        "{failed}"
    );

    // Each case was a real run of the agent, in Observe like any other.
    let case_run = multiplies["run_id"]
        .as_str()
        .expect("a real run")
        .to_string();
    let (status, run) = call(&app, "GET", &format!("/runs/{case_run}"), None).await;
    assert_eq!(status, StatusCode::OK, "{run}");
    assert_eq!(run["assistant_id"], json!("calc"));
    assert_eq!(run["metadata"]["channel"], json!("evaluation"));
    assert_eq!(run["metadata"]["case_id"], json!("multiplies"));
    // The case replays as the person who recorded it: the source run's
    // attribution rides on the evaluation run.
    let (_, source_run) = call(&app, "GET", &format!("/runs/{run_id}"), None).await;
    let recorded_by = source_run["metadata"]["created_by"].clone();
    assert!(!recorded_by.is_null(), "{source_run}");
    assert_eq!(run["metadata"]["on_behalf_of"], recorded_by, "{run}");

    let _ = std::fs::remove_dir_all(store);
}

/// A sweep tells the administrators — here the open-mode developer —
/// about a suite that did not pass in full: which agent, which dataset,
/// how many of how many, the first reason; a suite that passed says
/// nothing.
#[tokio::test]
async fn a_sweep_tells_the_administrators_about_a_suite_that_did_not_pass() {
    let (app, store) = test_app();
    let (_, _) = call(&app, "POST", "/assistants", Some(json!({"assistant_id": "calc", "name": "Calc", "graph": "capable", "config": {"studio_intent": {"instructions": "Multiply, then answer.", "tools": [{"name": "calculator"}]}}}))).await;
    let (_, v) = call(&app, "POST", "/threads", Some(json!({"graph": "capable"}))).await;
    let thread = v["thread_id"].as_str().unwrap().to_string();
    let (_, terminal) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/runs/wait"),
        Some(json!({"assistant_id": "calc", "input": input()})),
    )
    .await;
    let run_id = terminal["run_id"].as_str().unwrap().to_string();
    let (_, detail) = call(&app, "GET", &format!("/runs/{run_id}"), None).await;
    let accepted = detail["input"].clone();
    let source = json!({"run_id": run_id, "thread_id": thread, "agent_id": "calc", "captured_at": "2026-01-01T00:00:00Z"});
    // One suite that passes in full, one that does not.
    let (status, _) = call(&app, "POST", "/datasets", Some(json!({"name": "calc-good", "version": "v1", "cases": [
        {"id": "multiplies", "input": accepted, "expect": {"tool_trajectory": [{"name": "calculator"}]}, "tags": [], "source": source}
    ]}))).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(&app, "POST", "/datasets", Some(json!({"name": "calc-mixed", "version": "v1", "cases": [
        {"id": "multiplies", "input": accepted, "expect": {"tool_trajectory": [{"name": "calculator"}]}, "tags": [], "source": source},
        {"id": "no-tools", "input": accepted, "expect": {"forbid_tools": ["calculator"]}, "tags": [], "source": source}
    ]}))).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, swept) = call(&app, "POST", "/datasets/sweep", None).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{swept}");
    assert_eq!(
        swept["started"].as_array().map(Vec::len),
        Some(2),
        "{swept}"
    );
    let mut told = Value::Null;
    for _ in 0..300 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let (_, notices) = call(&app, "GET", "/notices", None).await;
        if let Some(n) = notices["notices"]
            .as_array()
            .and_then(|a| a.iter().find(|n| n["about"]["kind"] == json!("sweep")))
        {
            told = n.clone();
            break;
        }
    }
    assert!(!told.is_null(), "the administrator is told");
    assert_eq!(
        told["about"]["dataset"],
        json!("calc-mixed"),
        "the mixed suite, not the good one: {told}"
    );
    assert_eq!(
        told["title"],
        json!("Sweep: Calc's suite calc-mixed — 1 of 2 passed"),
        "{told}"
    );
    assert_eq!(
        told["about"]["regressed"],
        json!(false),
        "no earlier run to regress from: {told}"
    );
    assert!(
        told["text"]
            .as_str()
            .unwrap_or("")
            .contains("open the suite under Evals"),
        "{told}"
    );
    // Give the good suite a moment more; it says nothing.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let (_, notices) = call(&app, "GET", "/notices", None).await;
    let sweeps: Vec<&Value> = notices["notices"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|n| n["about"]["kind"] == json!("sweep"))
        .collect();
    assert_eq!(sweeps.len(), 1, "one suite told of, once: {notices}");
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn an_evaluation_puts_the_version_under_test_in_place_of_the_charter_the_run_heard() {
    let (app, store) = test_app();
    let (status, created) = call(
        &app,
        "POST",
        "/assistants",
        Some(json!({"assistant_id": "calc", "name": "Calc", "graph": "capable", "config": {"studio_intent": {"instructions": "Multiply, then answer.", "tools": [{"name": "calculator"}]}}})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");

    // The recorded run heard the first charter as its leading system message,
    // and the accepted input a case is bound to carries it.
    let (_, v) = call(&app, "POST", "/threads", Some(json!({"graph": "capable"}))).await;
    let thread = v["thread_id"].as_str().unwrap().to_string();
    let (status, terminal) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/runs/wait"),
        Some(json!({"assistant_id": "calc", "input": input()})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{terminal}");
    let run_id = terminal["run_id"].as_str().unwrap().to_string();
    let (_, detail) = call(&app, "GET", &format!("/runs/{run_id}"), None).await;
    let accepted = detail["input"].clone();
    assert_eq!(
        accepted["messages"][0]["role"],
        json!("system"),
        "{accepted}"
    );
    assert_eq!(
        accepted["messages"][0]["content"],
        json!("Multiply, then answer.")
    );
    let source = json!({"run_id": run_id, "thread_id": thread, "agent_id": "calc", "captured_at": "2026-01-01T00:00:00Z"});
    let (status, v) = call(
        &app,
        "POST",
        "/datasets",
        Some(json!({"name": "calc", "version": "v1", "cases": [{"id": "multiplies", "input": accepted, "expect": {"tool_trajectory": [{"name": "calculator"}]}, "tags": [], "source": source}]})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{v}");

    // The charter changes. Evaluating the new version must run the case
    // under the new charter, not replay the one the source run heard.
    let base = created["active_version_id"].as_str().unwrap().to_string();
    let (status, v) = call(
        &app,
        "POST",
        "/assistants/calc/versions",
        Some(json!({"base_version_id": base, "name": "Calc", "graph": "capable", "config": {"studio_intent": {"instructions": "Multiply carefully, then answer in one line.", "tools": [{"name": "calculator"}]}}})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    let new_version = v["version"]["version_id"].as_str().unwrap().to_string();
    // The promotion gate: the new version cannot go live until its suite
    // judged it — so the evaluation runs pinned to the version under test,
    // and it is that version's charter the cases hear.
    let (status, refused) = call(
        &app,
        "POST",
        &format!("/assistants/calc/versions/{new_version}/activate"),
        Some(json!({"expected_active_version_id": base})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(refused["error"], "evidence_required");

    let (status, started) = call(
        &app,
        "POST",
        "/datasets/calc/versions/v1/evaluations",
        Some(json!({"assistant_id": "calc", "version_id": new_version})),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{started}");
    assert_eq!(
        started["assistant_version_id"],
        json!(new_version),
        "{started}"
    );
    let mut finished = started;
    for _ in 0..400 {
        let (_, v) = call(&app, "GET", "/datasets/calc/versions/v1/evaluations", None).await;
        finished = v["evaluations"][0].clone();
        if finished["status"] != json!("running") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(finished["status"], json!("done"), "{finished}");
    let case_thread = finished["cases"][0]["thread_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, state) = call(&app, "GET", &format!("/threads/{case_thread}/state"), None).await;
    assert_eq!(status, StatusCode::OK, "{state}");
    let messages = state["values"]["messages"].as_array().unwrap();
    assert_eq!(messages[0]["role"], json!("system"), "{state}");
    assert_eq!(
        messages[0]["content"],
        json!("Multiply carefully, then answer in one line."),
        "{state}"
    );
    assert!(
        !messages
            .iter()
            .any(|m| m["content"] == json!("Multiply, then answer.")),
        "the charter the source run heard must not be replayed: {state}"
    );
    assert_eq!(messages[1]["role"], json!("user"), "{state}");

    // With that evidence, the version goes live.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/assistants/calc/versions/{new_version}/activate"),
        Some(json!({"expected_active_version_id": base})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["evidence"]["ok"], true, "{v}");

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn a_new_skill_revision_runs_the_suites_of_the_agents_that_follow_it() {
    let (app, store) = test_app();
    let skill_md = |body: &str| {
        format!("---\nname: multiply-well\ndescription: How to multiply and answer.\n---\n\n# Multiply well\n\n{body}\n")
    };

    // Revision 1, before anyone follows it: nothing to gate.
    let (status, r) = call(
        &app,
        "POST",
        "/skills",
        Some(json!({"skill_md": skill_md("Use the calculator.")})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{r}");
    assert_eq!(r["revision"], json!(1));
    assert_eq!(r["gate"], json!([]), "{r}");

    // An agent follows it and has a suite recorded from its own run.
    let (status, v) = call(
        &app,
        "POST",
        "/assistants",
        Some(json!({"assistant_id": "calc", "name": "Calc", "graph": "capable", "config": {"studio_intent": {"instructions": "Multiply, then answer.", "tools": [{"name": "calculator"}], "skills": ["multiply-well"]}}})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    let (_, v) = call(&app, "POST", "/threads", Some(json!({"graph": "capable"}))).await;
    let thread = v["thread_id"].as_str().unwrap().to_string();
    let (status, terminal) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/runs/wait"),
        Some(json!({"assistant_id": "calc", "input": input()})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{terminal}");
    let run_id = terminal["run_id"].as_str().unwrap().to_string();
    let (_, detail) = call(&app, "GET", &format!("/runs/{run_id}"), None).await;
    let source = json!({"run_id": run_id, "thread_id": thread, "agent_id": "calc", "captured_at": "2026-01-01T00:00:00Z"});
    let (status, v) = call(
        &app,
        "POST",
        "/datasets",
        Some(json!({"name": "calc", "version": "v1", "cases": [{"id": "multiplies", "input": detail["input"], "expect": {"tool_trajectory": [{"name": "calculator"}]}, "tags": [], "source": source}]})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{v}");

    // A second version of the suite: the gate runs the newest one only.
    let (status, v) = call(
        &app,
        "POST",
        "/datasets",
        Some(json!({"name": "calc", "version": "v2", "cases": [
            {"id": "multiplies", "input": detail["input"], "expect": {"tool_trajectory": [{"name": "calculator"}]}, "tags": [], "source": source},
            {"id": "multiplies-again", "input": detail["input"], "expect": {"tool_trajectory": [{"name": "calculator"}]}, "tags": ["twice"], "source": source}
        ]})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{v}");

    // Revision 2: the follower's suite starts on its own, and the receipt says so.
    let (status, r) = call(
        &app,
        "POST",
        "/skills",
        Some(json!({"skill_md": skill_md("Use the calculator, then answer in one line.")})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{r}");
    assert_eq!(r["revision"], json!(2));
    let gate = r["gate"].as_array().expect("a gate list");
    assert_eq!(gate.len(), 1, "one suite, its newest version: {r}");
    assert_eq!(gate[0]["assistant_id"], json!("calc"));
    assert_eq!(gate[0]["dataset"], json!("calc"));
    assert_eq!(gate[0]["version"], json!("v2"));
    assert_eq!(gate[0]["cases"], json!(2));
    let evaluation_id = gate[0]["evaluation_id"]
        .as_str()
        .expect("an evaluation started")
        .to_string();

    let mut finished = Value::Null;
    for _ in 0..400 {
        let (_, v) = call(&app, "GET", "/datasets/calc/versions/v2/evaluations", None).await;
        finished = v["evaluations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["evaluation_id"] == json!(evaluation_id))
            .cloned()
            .unwrap_or(Value::Null);
        if finished["status"] != json!("running") && !finished.is_null() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(finished["status"], json!("done"), "{finished}");
    assert_eq!(finished["passed"], json!(2), "{finished}");
    assert_eq!(finished["started_by"]["kind"], json!("skill"), "{finished}");
    assert_eq!(finished["started_by"]["revision"], json!(2), "{finished}");

    // The same text again is the same revision: nothing runs.
    let (status, r) = call(
        &app,
        "POST",
        "/skills",
        Some(json!({"skill_md": skill_md("Use the calculator, then answer in one line.")})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{r}");
    assert_eq!(r["already_registered"], json!(true));
    assert!(r.get("gate").is_none(), "{r}");

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn an_evaluation_orphaned_by_a_restart_is_closed_as_an_error_not_left_running() {
    let store = std::env::temp_dir().join(format!("rusty-dataset-runs-{}", uuid::Uuid::new_v4()));
    // A normal server makes the dataset from a real run…
    let app = test_app_at(store.clone(), false);
    let (status, v) = call(&app, "POST", "/assistants", Some(json!({"assistant_id": "calc", "name": "Calc", "graph": "capable", "config": {"studio_intent": {"instructions": "Answer.", "tools": [{"name": "calculator"}]}}}))).await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    let (_, v) = call(&app, "POST", "/threads", Some(json!({"graph": "capable"}))).await;
    let thread = v["thread_id"].as_str().unwrap().to_string();
    let (_, terminal) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/runs/wait"),
        Some(json!({"assistant_id": "calc", "input": input()})),
    )
    .await;
    let run_id = terminal["run_id"].as_str().unwrap().to_string();
    let (_, detail) = call(&app, "GET", &format!("/runs/{run_id}"), None).await;
    let (status, v) = call(&app, "POST", "/datasets", Some(json!({"name": "calc", "version": "v1", "cases": [
        {"id": "multiplies", "input": detail["input"], "expect": {}, "tags": [], "source": {"run_id": run_id, "thread_id": thread, "agent_id": "calc", "captured_at": "2026-01-01T00:00:00Z"}}
    ]}))).await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    drop(app);

    // …a server whose model never answers starts running it, then dies.
    let app = test_app_at(store.clone(), true);
    let (status, started) = call(
        &app,
        "POST",
        "/datasets/calc/versions/v1/evaluations",
        Some(json!({"assistant_id": "calc"})),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{started}");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let (_, v) = call(&app, "GET", "/datasets/calc/versions/v1/evaluations", None).await;
    assert_eq!(
        v["evaluations"][0]["status"],
        json!("running"),
        "still running on the process that owns it: {v}"
    );
    drop(app);

    // The next server finds the record and closes it honestly.
    let app = test_app_at(store.clone(), false);
    let (status, v) = call(&app, "GET", "/datasets/calc/versions/v1/evaluations", None).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let ev = &v["evaluations"][0];
    assert_eq!(ev["evaluation_id"], started["evaluation_id"]);
    assert_eq!(ev["status"], json!("error"), "{ev}");
    assert!(ev["error"].as_str().unwrap().contains("restarted"), "{ev}");
    assert!(ev["finished_at"].is_string());
    // Closed once: a second read is the same record, and a fresh evaluation
    // on this server runs to completion as usual.
    let (_, again) = call(&app, "GET", "/datasets/calc/versions/v1/evaluations", None).await;
    assert_eq!(again["evaluations"][0], *ev);
    let (status, fresh) = call(
        &app,
        "POST",
        "/datasets/calc/versions/v1/evaluations",
        Some(json!({"assistant_id": "calc"})),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{fresh}");
    let mut done = false;
    for _ in 0..200 {
        let (_, v) = call(&app, "GET", "/datasets/calc/versions/v1/evaluations", None).await;
        if v["evaluations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["evaluation_id"] == fresh["evaluation_id"] && e["status"] == json!("done"))
        {
            done = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(done, "a fresh evaluation on the new server finishes");
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn a_case_with_a_rubric_is_judged_by_the_servers_model_over_the_reply() {
    let (app, store) = judged_app();
    let (status, v) = call(&app, "POST", "/assistants", Some(json!({"assistant_id": "calc", "name": "Calc", "graph": "capable", "config": {"studio_intent": {"instructions": "Answer.", "tools": [{"name": "calculator"}]}}}))).await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    let (_, v) = call(&app, "POST", "/threads", Some(json!({"graph": "capable"}))).await;
    let thread = v["thread_id"].as_str().unwrap().to_string();
    let (_, terminal) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/runs/wait"),
        Some(json!({"assistant_id": "calc", "input": input()})),
    )
    .await;
    let run_id = terminal["run_id"].as_str().unwrap().to_string();
    let (_, detail) = call(&app, "GET", &format!("/runs/{run_id}"), None).await;
    let source = json!({"run_id": run_id, "thread_id": thread, "agent_id": "calc", "captured_at": "2026-01-01T00:00:00Z"});
    let (status, v) = call(&app, "POST", "/datasets", Some(json!({"name": "judged", "version": "v1", "cases": [
        {"id": "says-42", "input": detail["input"], "expect": {"rubric": "The reply must state 42."}, "tags": [], "source": source},
        {"id": "apologises", "input": detail["input"], "expect": {"rubric": "The reply must apologise."}, "tags": [], "source": source}
    ]}))).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "the rubric is part of the case: {v}"
    );
    let (_, cases) = call(&app, "GET", "/datasets/judged/versions/v1/cases", None).await;
    assert_eq!(
        cases["cases"][0]["expect"]["rubric"],
        json!("The reply must state 42.")
    );

    let (status, started) = call(
        &app,
        "POST",
        "/datasets/judged/versions/v1/evaluations",
        Some(json!({"assistant_id": "calc"})),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{started}");
    let mut finished = Value::Null;
    for _ in 0..200 {
        let (_, v) = call(
            &app,
            "GET",
            "/datasets/judged/versions/v1/evaluations",
            None,
        )
        .await;
        let ev = v["evaluations"][0].clone();
        if ev["status"] == json!("done") {
            finished = ev;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(finished["status"], json!("done"), "{finished}");
    let cases = finished["cases"].as_array().unwrap();
    let says = cases
        .iter()
        .find(|c| c["case_id"] == json!("says-42"))
        .unwrap();
    assert_eq!(says["passed"], json!(true), "{says}");
    assert_eq!(says["judge"]["passed"], json!(true));
    assert_eq!(
        says["judge"]["rationale"],
        json!("The reply gives 42 as the rubric asks.")
    );
    let apologises = cases
        .iter()
        .find(|c| c["case_id"] == json!("apologises"))
        .unwrap();
    assert_eq!(
        apologises["passed"],
        json!(false),
        "the judge's no is the case's no: {apologises}"
    );
    assert_eq!(apologises["judge"]["score"], json!(0.0));
    assert_eq!(
        apologises["judge"]["rationale"],
        json!("The reply does not do what the rubric asks.")
    );
    assert_eq!(finished["passed"], json!(1));
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn a_rubric_on_a_server_without_a_judge_fails_the_case_and_says_so() {
    let (app, store) = test_app();
    let (_, _) = call(&app, "POST", "/assistants", Some(json!({"assistant_id": "calc", "name": "Calc", "graph": "capable", "config": {"studio_intent": {"instructions": "Answer.", "tools": [{"name": "calculator"}]}}}))).await;
    let (_, v) = call(&app, "POST", "/threads", Some(json!({"graph": "capable"}))).await;
    let thread = v["thread_id"].as_str().unwrap().to_string();
    let (_, terminal) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/runs/wait"),
        Some(json!({"assistant_id": "calc", "input": input()})),
    )
    .await;
    let run_id = terminal["run_id"].as_str().unwrap().to_string();
    let (_, detail) = call(&app, "GET", &format!("/runs/{run_id}"), None).await;
    let (status, v) = call(&app, "POST", "/datasets", Some(json!({"name": "unjudged", "version": "v1", "cases": [
        {"id": "says-42", "input": detail["input"], "expect": {"rubric": "The reply must state 42."}, "tags": [], "source": {"run_id": run_id, "thread_id": thread, "agent_id": "calc", "captured_at": "2026-01-01T00:00:00Z"}}
    ]}))).await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    let (_, _) = call(
        &app,
        "POST",
        "/datasets/unjudged/versions/v1/evaluations",
        Some(json!({"assistant_id": "calc"})),
    )
    .await;
    let mut finished = Value::Null;
    for _ in 0..200 {
        let (_, v) = call(
            &app,
            "GET",
            "/datasets/unjudged/versions/v1/evaluations",
            None,
        )
        .await;
        if v["evaluations"][0]["status"] == json!("done") {
            finished = v["evaluations"][0].clone();
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let case = &finished["cases"][0];
    assert_eq!(case["passed"], json!(false), "{case}");
    assert!(
        case["judge"]["rationale"]
            .as_str()
            .unwrap()
            .contains("no judge model"),
        "{case}"
    );
    let _ = std::fs::remove_dir_all(store);
}
