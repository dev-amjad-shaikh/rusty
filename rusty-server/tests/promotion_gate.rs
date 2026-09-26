//! The promotion gate (Astra R07): a version activates only with current
//! evidence — its agent's suites evaluated against *that* version, every
//! case passed — or an admin's recorded word. Through the real paths: a
//! dataset whose case was recorded from the agent, an evaluation pinned to
//! a proposed version, the activation route.
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

/// Follows its charter: a charter that says to call lookup gets a lookup
/// first; any other answers directly.
struct Obedient;
#[async_trait::async_trait]
impl ChatModel for Obedient {
    async fn chat(&self, messages: &[ChatMessage], _t: &[Value]) -> RustyResult<ChatResponse> {
        let charter = messages.iter().filter(|m| m.role == Role::System).filter_map(|m| m.content.clone()).collect::<Vec<_>>().join("\n");
        let looked = messages.iter().any(|m| m.role == Role::Tool);
        let message = if charter.contains("always call lookup") && !looked {
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

fn app() -> (Router, std::path::PathBuf) {
    let store = std::env::temp_dir().join(format!("rusty-server-promotion-{}", uuid::Uuid::new_v4()));
    let mut tools = ToolRegistry::new();
    tools.register(Lookup);
    let graph = create_react_agent(Arc::new(Obedient), tools.clone()).unwrap();
    let spec = StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry.register_with_tools("react_agent", graph, spec, &tools).unwrap();
    (router(registry, ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())), store)
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

async fn version(app: &Router, base: &str, instructions: &str) -> String {
    let (status, made) = call(app, "POST", "/assistants/desk/versions", Some(json!({"base_version_id": base, "name": "Desk", "graph": "react_agent", "config": {"instructions": instructions}}))).await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    made["version_id"].as_str().or_else(|| made["version"]["version_id"].as_str()).unwrap().to_owned()
}

async fn evaluated(app: &Router, name: &str, version: &str, version_id: &str) -> Value {
    let (status, started) = call(app, "POST", &format!("/datasets/{name}/versions/{version}/evaluations"), Some(json!({"assistant_id": "desk", "version_id": version_id}))).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{started}");
    assert_eq!(started["assistant_version_id"], json!(version_id), "the evaluation names the version it runs: {started}");
    for _ in 0..400 {
        let (_, list) = call(app, "GET", &format!("/datasets/{name}/versions/{version}/evaluations"), None).await;
        if let Some(done) = list["evaluations"].as_array().and_then(|e| e.iter().find(|x| x["evaluation_id"] == started["evaluation_id"] && x["status"] != "running")) {
            return done.clone();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the evaluation never finished");
}

#[tokio::test]
async fn a_version_activates_only_with_current_passing_evidence_or_an_admins_recorded_word() {
    let (app, store) = app();
    let (status, made) = call(&app, "POST", "/assistants", Some(json!({"assistant_id": "desk", "name": "Desk", "graph": "react_agent", "config": {"instructions": "answer directly"}}))).await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let v1 = made["active_version_id"].as_str().unwrap().to_owned();

    // A suite recorded from this agent: one real run becomes a case that
    // expects a lookup (the person's expectation, not the run's behavior).
    let (_, thread) = call(&app, "POST", "/threads", Some(json!({"graph": "react_agent"}))).await;
    let thread_id = thread["thread_id"].as_str().unwrap().to_owned();
    let (status, run) = call(&app, "POST", &format!("/threads/{thread_id}/runs/wait"), Some(json!({"input": {"messages": [{"role": "user", "content": "how many?"}]}, "assistant_id": "desk"}))).await;
    assert_eq!(status, StatusCode::OK, "{run}");
    let run_id = run["run_id"].as_str().unwrap().to_owned();
    // The case's input is the run's, exactly as the server kept it.
    let (_, kept) = call(&app, "GET", &format!("/runs/{run_id}"), None).await;
    let (status, dataset) = call(&app, "POST", "/datasets", Some(json!({"name": "desk-counts", "version": "1", "cases": [{
        "id": "counts-via-lookup",
        "input": kept["input"],
        "expect": {"tool_trajectory": [{"name": "lookup"}]},
        "source": {"run_id": run_id, "thread_id": thread_id, "agent_id": "desk", "captured_at": "2026-09-08T10:00:00Z"}
    }]}))).await;
    assert_eq!(status, StatusCode::CREATED, "{dataset}");

    // A skill the proposed version follows: its revision is part of the
    // evidence's dependencies.
    let skill_md = |body: &str| format!("---\nname: count-well\ndescription: How to count and answer.\n---\n\n# Count well\n\n{body}\n");
    let (status, r) = call(&app, "POST", "/skills", Some(json!({"skill_md": skill_md("Look it up, then say the number.")}))).await;
    assert_eq!(status, StatusCode::CREATED, "{r}");
    assert_eq!(r["revision"], json!(1));

    // A proposed version that would pass, and nothing judged it yet.
    let (status, made) = call(&app, "POST", "/assistants/desk/versions", Some(json!({"base_version_id": v1, "name": "Desk", "graph": "react_agent", "config": {"instructions": "always call lookup before answering", "studio_intent": {"skills": ["count-well"]}}}))).await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let v2 = made["version_id"].as_str().or_else(|| made["version"]["version_id"].as_str()).unwrap().to_owned();
    let (status, refused) = call(&app, "POST", &format!("/assistants/desk/versions/{v2}/activate"), Some(json!({"expected_active_version_id": v1}))).await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(refused["error"], "evidence_required");
    assert_eq!(refused["evidence"]["suites"][0]["state"], "missing", "{refused}");
    let (_, evidence) = call(&app, "GET", &format!("/assistants/desk/versions/{v2}/evidence"), None).await;
    assert_eq!(evidence["evidence"]["ok"], false);
    assert_eq!(evidence["evidence"]["suites"][0]["name"], "desk-counts");

    // Evaluate *that* version: the case passes; the gate opens.
    let done = evaluated(&app, "desk-counts", "1", &v2).await;
    assert_eq!(done["passed"], 1, "{done}");
    let (_, evidence) = call(&app, "GET", &format!("/assistants/desk/versions/{v2}/evidence"), None).await;
    assert_eq!(evidence["evidence"]["ok"], true, "{evidence}");
    assert_eq!(evidence["evidence"]["suites"][0]["state"], "passed");
    assert_eq!(done["dependencies"]["skills"]["count-well"], json!(1), "the evaluation kept what it ran under: {done}");

    // The skill it follows changes: the passing evidence is stale, the gate
    // closes again, and says why — until the version is evaluated anew.
    let (status, r) = call(&app, "POST", "/skills", Some(json!({"skill_md": skill_md("Look it up twice, then say the number.")}))).await;
    assert_eq!(status, StatusCode::CREATED, "{r}");
    assert_eq!(r["revision"], json!(2));
    let (_, evidence) = call(&app, "GET", &format!("/assistants/desk/versions/{v2}/evidence"), None).await;
    assert_eq!(evidence["evidence"]["suites"][0]["state"], "stale", "{evidence}");
    assert_eq!(evidence["evidence"]["suites"][0]["stale_because"][0], "skill count-well: revision 1 → 2");
    assert_eq!(evidence["evidence"]["ok"], false);
    let (status, refused) = call(&app, "POST", &format!("/assistants/desk/versions/{v2}/activate"), Some(json!({"expected_active_version_id": v1}))).await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert!(refused["message"].as_str().unwrap().contains("(stale)"), "{refused}");
    let done = evaluated(&app, "desk-counts", "1", &v2).await;
    assert_eq!(done["passed"], 1, "{done}");
    let (_, evidence) = call(&app, "GET", &format!("/assistants/desk/versions/{v2}/evidence"), None).await;
    assert_eq!(evidence["evidence"]["suites"][0]["state"], "passed", "{evidence}");
    let (status, activated) = call(&app, "POST", &format!("/assistants/desk/versions/{v2}/activate"), Some(json!({"expected_active_version_id": v1}))).await;
    assert_eq!(status, StatusCode::OK, "{activated}");
    assert_eq!(activated["activated"], true);
    assert_eq!(activated["evidence"]["ok"], true);

    // A version that would fail: evaluated, failing, refused — until an
    // admin says why, and the word is kept with what it overrode.
    let v3 = version(&app, &v2, "answer directly").await;
    let done = evaluated(&app, "desk-counts", "1", &v3).await;
    assert_eq!(done["passed"], 0, "{done}");
    let (_, evidence) = call(&app, "GET", &format!("/assistants/desk/versions/{v3}/evidence"), None).await;
    assert_eq!(evidence["evidence"]["suites"][0]["state"], "failed", "{evidence}");
    assert_eq!(evidence["evidence"]["suites"][0]["baseline"]["passed"], 1, "the active version's result sits beside it: {evidence}");
    let why = &evidence["evidence"]["suites"][0]["failures"][0];
    assert!(why["said"].as_str().is_some_and(|w| !w.is_empty()), "the gate says why: {evidence}");
    let (status, refused) = call(&app, "POST", &format!("/assistants/desk/versions/{v3}/activate"), Some(json!({"expected_active_version_id": v2}))).await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    let (status, forced) = call(&app, "POST", &format!("/assistants/desk/versions/{v3}/activate"), Some(json!({"expected_active_version_id": v2, "override_reason": "the lookup is down this week; answering from memory is what the desk needs"}))).await;
    assert_eq!(status, StatusCode::OK, "{forced}");
    assert_eq!(forced["activated"], true);
    let (_, evidence) = call(&app, "GET", &format!("/assistants/desk/versions/{v3}/evidence"), None).await;
    let promotion = &evidence["promotions"][0];
    assert_eq!(promotion["version_id"], json!(v3));
    assert!(promotion["override_reason"].as_str().unwrap().contains("lookup is down"), "{evidence}");
    assert_eq!(promotion["evidence"]["ok"], false, "the override is kept with the evidence it overrode");

    let _ = std::fs::remove_dir_all(store);
}

/// The candidate gate: judging a version by hand runs every suite bound to
/// the agent against it, under the candidate budget; and the evidence is on
/// the content — a twin of a judged version (the same configuration under
/// a new id, as Apply-then-Publish makes) passes the gate on that judgment.
#[tokio::test]
async fn a_candidate_is_judged_on_every_suite_under_a_budget_and_its_twin_inherits_the_verdict() {
    let (app, store) = app();
    let (status, made) = call(&app, "POST", "/assistants", Some(json!({"assistant_id": "desk", "name": "Desk", "graph": "react_agent", "config": {"instructions": "answer directly"}}))).await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let v1 = made["active_version_id"].as_str().unwrap().to_owned();
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

    // A candidate nobody evaluated: judged by hand, every bound suite starts
    // against it, capped at the candidate budget.
    let v2 = version(&app, &v1, "always call lookup before answering").await;
    let (status, judged) = call(&app, "POST", &format!("/assistants/desk/versions/{v2}/evidence"), None).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{judged}");
    assert_eq!(judged["started"][0]["dataset"], "desk-counts", "{judged}");
    let evaluation_id = judged["started"][0]["evaluation_id"].as_str().expect("the suite started").to_owned();
    assert_eq!(judged["evidence"]["suites"][0]["state"], "running", "{judged}");
    let mut done = Value::Null;
    for _ in 0..400 {
        let (_, list) = call(&app, "GET", "/datasets/desk-counts/versions/1/evaluations", None).await;
        if let Some(d) = list["evaluations"].as_array().and_then(|e| e.iter().find(|x| x["evaluation_id"] == evaluation_id && x["status"] != "running")) {
            done = d.clone();
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(done["status"], "done", "{done}");
    assert_eq!(done["assistant_version_id"], json!(v2));
    assert_eq!(done["started_by"]["kind"], "candidate", "{done}");
    assert_eq!(done["budget_usd"], json!(1.0), "the candidate budget rides on the evaluation: {done}");
    assert_eq!(done["passed"], 1, "{done}");

    // A twin: the same configuration filed under a new id, as a person's
    // Apply-then-Publish makes. Nothing evaluated it; the judgment of its
    // twin stands for it, and the gate opens without an override.
    let (status, made) = call(&app, "POST", "/assistants/desk/versions", Some(json!({"base_version_id": v1, "name": "Desk", "graph": "react_agent", "config": {"instructions": "always call lookup before answering"}, "metadata": {"studio": {"published_at": "2026-09-23T10:00:00Z"}}}))).await;
    assert_eq!(status, StatusCode::CREATED, "a version with the same content and other metadata is its own: {made}");
    let v3 = made["version"]["version_id"].as_str().unwrap().to_owned();
    assert_ne!(v2, v3);
    let (_, evidence) = call(&app, "GET", &format!("/assistants/desk/versions/{v3}/evidence"), None).await;
    assert_eq!(evidence["evidence"]["suites"][0]["state"], "passed", "{evidence}");
    assert_eq!(evidence["evidence"]["suites"][0]["evaluation_id"], json!(evaluation_id), "the twin's evaluation is the evidence: {evidence}");
    let (status, activated) = call(&app, "POST", &format!("/assistants/desk/versions/{v3}/activate"), Some(json!({"expected_active_version_id": v1}))).await;
    assert_eq!(status, StatusCode::OK, "{activated}");
    assert_eq!(activated["activated"], true);

    // A different charter is no twin: nothing judged it.
    let v4 = version(&app, &v3, "answer directly, then call lookup").await;
    let (_, evidence) = call(&app, "GET", &format!("/assistants/desk/versions/{v4}/evidence"), None).await;
    assert_eq!(evidence["evidence"]["suites"][0]["state"], "missing", "{evidence}");

    let _ = std::fs::remove_dir_all(store);
}


/// The bar is the agent's: each suite's pass rate, set in the version that
/// runs (100% unless a person lowered it). A version passing half the cases
/// fails the default bar, and clears a 50% bar once a version carrying it
/// runs — as long as it passes no fewer than that version.
#[tokio::test]
async fn the_agents_bar_is_a_pass_rate_set_by_the_version_that_runs() {
    let (app, store) = app();
    let (status, made) = call(&app, "POST", "/assistants", Some(json!({"assistant_id": "desk", "name": "Desk", "graph": "react_agent", "config": {"instructions": "answer directly"}}))).await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let v1 = made["active_version_id"].as_str().unwrap().to_owned();
    let (_, thread) = call(&app, "POST", "/threads", Some(json!({"graph": "react_agent"}))).await;
    let thread_id = thread["thread_id"].as_str().unwrap().to_owned();
    let (status, run) = call(&app, "POST", &format!("/threads/{thread_id}/runs/wait"), Some(json!({"input": {"messages": [{"role": "user", "content": "how many?"}]}, "assistant_id": "desk"}))).await;
    assert_eq!(status, StatusCode::OK, "{run}");
    let run_id = run["run_id"].as_str().unwrap().to_owned();
    let (_, kept) = call(&app, "GET", &format!("/runs/{run_id}"), None).await;
    let source = json!({"run_id": run_id, "thread_id": thread_id, "agent_id": "desk", "captured_at": "2026-09-24T10:00:00Z"});
    // Two cases no single charter passes both of: one wants the lookup, one forbids it.
    let (status, dataset) = call(&app, "POST", "/datasets", Some(json!({"name": "desk-half", "version": "1", "cases": [
        {"id": "wants-lookup", "input": kept["input"], "expect": {"tool_trajectory": [{"name": "lookup"}]}, "source": source},
        {"id": "no-lookup", "input": kept["input"], "expect": {"forbid_tools": ["lookup"]}, "source": source}
    ]}))).await;
    assert_eq!(status, StatusCode::CREATED, "{dataset}");

    // The default bar: every case. The lookup version passes one of two and fails, saying why.
    let v2 = version(&app, &v1, "always call lookup before answering").await;
    let done = evaluated(&app, "desk-half", "1", &v2).await;
    assert_eq!(done["passed"], 1, "{done}");
    let (_, evidence) = call(&app, "GET", &format!("/assistants/desk/versions/{v2}/evidence"), None).await;
    assert_eq!(evidence["evidence"]["pass_rate_min"], 1.0, "{evidence}");
    assert_eq!(evidence["evidence"]["suites"][0]["state"], "failed", "{evidence}");
    assert!(evidence["evidence"]["suites"][0]["below"].as_str().unwrap().contains("under the agent's bar of 100%"), "{evidence}");

    // A person lowers the bar to half; it counts once a version carrying it runs.
    let (status, made) = call(&app, "POST", "/assistants/desk/versions", Some(json!({"base_version_id": v1, "name": "Desk", "graph": "react_agent", "config": {"instructions": "answer directly", "studio_intent": {"gate": {"pass_rate": 50, "budget_usd": 0.5}}}}))).await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let v3 = made["version_id"].as_str().or_else(|| made["version"]["version_id"].as_str()).unwrap().to_owned();
    let done = evaluated(&app, "desk-half", "1", &v3).await;
    assert_eq!(done["passed"], 1, "{done}");
    let (status, forced) = call(&app, "POST", &format!("/assistants/desk/versions/{v3}/activate"), Some(json!({"expected_active_version_id": v1, "override_reason": "the bar is changing to half; this version carries it"}))).await;
    assert_eq!(status, StatusCode::OK, "{forced}");
    let (_, evidence) = call(&app, "GET", &format!("/assistants/desk/versions/{v2}/evidence"), None).await;
    assert_eq!(evidence["evidence"]["pass_rate_min"], 0.5, "{evidence}");
    assert_eq!(evidence["evidence"]["suites"][0]["state"], "passed", "half, and as many as the version running now: {evidence}");
    assert_eq!(evidence["evidence"]["ok"], true);
    assert_eq!(evidence["evidence"]["suites"][0]["failures"].as_array().map(|f| f.len()), Some(1), "the failing case is still shown: {evidence}");
    // The candidate cap follows the agent: a judgment started now carries 0.5.
    let (status, judged) = call(&app, "POST", &format!("/assistants/desk/versions/{v2}/evidence"), None).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{judged}");
    let evaluation_id = judged["started"][0]["evaluation_id"].as_str().unwrap().to_owned();
    let (_, list) = call(&app, "GET", "/datasets/desk-half/versions/1/evaluations", None).await;
    let started = list["evaluations"].as_array().unwrap().iter().find(|e| e["evaluation_id"] == json!(evaluation_id)).cloned().unwrap();
    assert_eq!(started["budget_usd"], 0.5, "{started}");

    let _ = std::fs::remove_dir_all(store);
}
