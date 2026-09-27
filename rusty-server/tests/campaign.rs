//! The campaign report: a family's variants are ordinary suite cases
//! tagged `campaign:Fn`, a repetition is an ordinary evaluation, and the
//! counts come from the evaluation record and the case run's journal —
//! duplicate consequential calls, refused calls, interventions — rendered
//! as JSON and as the Markdown the campaign note records.
use std::sync::Arc;
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::error::{Result as RustyResult, RustyError};
use rusty_agent_runtime::llm::{ChatMessage, ChatModel, ChatResponse, Role, ToolCall};
use rusty_agent_runtime::react::{create_react_agent, MESSAGES_CHANNEL};
use rusty_agent_runtime::record::Effect;
use rusty_agent_runtime::state::{Reducer, StateSpec};
use rusty_agent_runtime::tool::{Tool, ToolFailure, ToolRegistry};
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

/// Told "post twice" it posts the same notice twice; told "open the vault"
/// it tries the locked tool; then it answers.
struct Scripted;
#[async_trait::async_trait]
impl ChatModel for Scripted {
    async fn chat(&self, messages: &[ChatMessage], _t: &[Value]) -> RustyResult<ChatResponse> {
        let asked = messages
            .iter()
            .filter(|m| m.role == Role::User)
            .filter_map(|m| m.content.clone())
            .collect::<Vec<_>>()
            .join(" ");
        let tool_turns = messages.iter().filter(|m| m.role == Role::Tool).count();
        let message = if asked.contains("post twice") && tool_turns == 0 {
            ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                "c1",
                "post",
                json!({"text": "hello"}),
            )])
        } else if asked.contains("post twice") && tool_turns == 1 {
            ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                "c2",
                "post",
                json!({"text": "hello"}),
            )])
        } else if asked.contains("open the vault") && tool_turns == 0 {
            ChatMessage::assistant_tool_calls(vec![ToolCall::new("c3", "vault", json!({}))])
        } else {
            ChatMessage::assistant("done")
        };
        Ok(ChatResponse {
            message,
            model: Some("scripted".into()),
            usage: None,
        })
    }
}

struct Post;
#[async_trait::async_trait]
impl Tool for Post {
    fn name(&self) -> &str {
        "post"
    }
    fn description(&self) -> &str {
        "Posts a notice."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn effect(&self) -> Effect {
        Effect::NonIdempotent
    }
    async fn call(&self, _args: Value) -> RustyResult<Value> {
        Ok(json!({"posted": true}))
    }
}

struct Vault;
#[async_trait::async_trait]
impl Tool for Vault {
    fn name(&self) -> &str {
        "vault"
    }
    fn description(&self) -> &str {
        "A locked door."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, _args: Value) -> RustyResult<Value> {
        Err(RustyError::Tool(
            serde_json::to_string(&ToolFailure::new(
                "denied",
                "vault",
                "the vault refuses this key",
                true,
                false,
                "ask for access",
            ))
            .unwrap(),
        ))
    }
}

fn app() -> (Router, std::path::PathBuf) {
    let store = std::env::temp_dir().join(format!("rusty-campaign-{}", uuid::Uuid::new_v4()));
    let mut tools = ToolRegistry::new();
    tools.register(Post);
    tools.register(Vault);
    let graph = create_react_agent(Arc::new(Scripted), tools.clone()).unwrap();
    let spec = StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry
        .register_with_tools("react_agent", graph, spec, &tools)
        .unwrap();
    (
        router(
            registry,
            ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone()),
        ),
        store,
    )
}

async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value, String) {
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
    let text = String::from_utf8_lossy(&bytes).into_owned();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        text,
    )
}

/// Approve whatever waits — a person's part of a consequential write.
async fn approve_all(app: &Router) -> usize {
    let (_, list, _) = call(app, "GET", "/approvals", None).await;
    let pending: Vec<String> = list
        .get("approvals")
        .and_then(Value::as_array)
        .or_else(|| list.as_array())
        .into_iter()
        .flatten()
        .filter(|a| {
            a.get("status")
                .and_then(Value::as_str)
                .is_none_or(|s| s == "pending")
        })
        .filter_map(|a| a.get("run_id").and_then(Value::as_str).map(str::to_owned))
        .collect();
    for run_id in &pending {
        let (status, decided, _) = call(
            app,
            "POST",
            &format!("/approvals/{run_id}/decide"),
            Some(json!({"decision": "approve"})),
        )
        .await;
        assert!(status.is_success(), "{decided}");
    }
    pending.len()
}

/// A run that finishes, its approvals decided along the way — following
/// the run each decision continues it in.
async fn finished_run(app: &Router, run_id: &str) -> Value {
    let mut current = run_id.to_owned();
    for _ in 0..600 {
        approve_all(app).await;
        let (_, run, _) = call(app, "GET", &format!("/runs/{current}"), None).await;
        if matches!(
            run["status"].as_str(),
            Some("success" | "error" | "cancelled")
        ) {
            return run;
        }
        if run["status"] == "interrupted" {
            let (_, list, _) = call(app, "GET", "/approvals", None).await;
            if let Some(next) = list["approvals"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|a| a["run_id"] == current && a["status"] == "approved")
                .and_then(|a| a["resumed_run_id"].as_str())
            {
                current = next.to_owned();
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let (_, run, _) = call(app, "GET", &format!("/runs/{run_id}"), None).await;
    let (_, approvals, _) = call(app, "GET", "/approvals", None).await;
    let (_, events, _) = call(app, "GET", &format!("/runs/{run_id}/events"), None).await;
    let kinds: Vec<String> = events["events"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| format!("{}:{}", e["kind"], e["status"]))
        .collect();
    panic!(
        "run {run_id} never finished: status {} approvals {} events {:?}",
        run["status"], approvals, kinds
    );
}

async fn converse(app: &Router, words: &str) -> (String, String) {
    let (_, thread, _) = call(
        app,
        "POST",
        "/threads",
        Some(json!({"graph": "react_agent"})),
    )
    .await;
    let thread_id = thread["thread_id"].as_str().unwrap().to_owned();
    let (status, run, _) = call(app, "POST", &format!("/threads/{thread_id}/runs"), Some(json!({"input": {"messages": [{"role": "user", "content": words}]}, "assistant_id": "desk"}))).await;
    assert!(status.is_success(), "{run}");
    let run_id = run["run_id"].as_str().unwrap().to_owned();
    let done = finished_run(app, &run_id).await;
    assert_eq!(done["status"], json!("success"), "{done}");
    (thread_id, run_id)
}

#[tokio::test]
async fn the_campaign_counts_a_familys_variants_from_their_runs() {
    let (app, store) = app();
    let (status, made, _) = call(&app, "POST", "/assistants", Some(json!({"assistant_id": "desk", "name": "Desk", "graph": "react_agent", "config": {"instructions": "do as asked"}}))).await;
    assert_eq!(status, StatusCode::CREATED, "{made}");

    // Two variants recorded from real runs, each tagged with its family.
    let (t1, r1) = converse(&app, "post twice").await;
    let (t2, r2) = converse(&app, "open the vault").await;
    let (_, kept1, _) = call(&app, "GET", &format!("/runs/{r1}"), None).await;
    let (_, kept2, _) = call(&app, "GET", &format!("/runs/{r2}"), None).await;
    let (status, dataset, _) = call(&app, "POST", "/datasets", Some(json!({"name": "campaign-desk", "version": "1", "cases": [
        {"id": "twice", "input": kept1["input"], "tags": ["campaign:F8"], "expect": {"tool_trajectory": [{"name": "post"}]},
         "source": {"run_id": r1, "thread_id": t1, "agent_id": "desk", "captured_at": "2026-09-09T10:00:00Z"}},
        {"id": "vault", "input": kept2["input"], "tags": ["F3"], "expect": {"tool_trajectory": [{"name": "vault"}]},
         "source": {"run_id": r2, "thread_id": t2, "agent_id": "desk", "captured_at": "2026-09-09T10:00:00Z"}}
    ]}))).await;
    assert_eq!(status, StatusCode::CREATED, "{dataset}");

    // Before any repetition: the variants are listed, nothing counted.
    let (status, report, _) = call(&app, "GET", "/campaign", None).await;
    assert_eq!(status, StatusCode::OK, "{report}");
    let family = |id: &str| {
        report["families"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["id"] == id)
            .cloned()
            .unwrap()
    };
    assert_eq!(family("F8")["totals"]["variants"], json!(1));
    assert_eq!(family("F8")["totals"]["repetitions"], json!(0));
    assert_eq!(family("F1")["totals"]["variants"], json!(0));
    assert!(family("F1")["needs"].as_str().unwrap().contains("conflict"));

    // One repetition: the suite evaluated.
    let (status, started, _) = call(
        &app,
        "POST",
        "/datasets/campaign-desk/versions/1/evaluations",
        Some(json!({"assistant_id": "desk"})),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{started}");
    for _ in 0..600 {
        approve_all(&app).await;
        let (_, list, _) = call(
            &app,
            "GET",
            "/datasets/campaign-desk/versions/1/evaluations",
            None,
        )
        .await;
        if list["evaluations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["evaluation_id"] == started["evaluation_id"] && e["status"] != "running")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let (_, report, _) = call(&app, "GET", "/campaign", None).await;
    let f8 = report["families"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == "F8")
        .unwrap();
    assert_eq!(f8["totals"]["repetitions"], json!(1), "{f8}");
    // The loop refused the identical second write before it was placed
    // (Story 40's repeat rule), so the campaign counts one placed call, one
    // approval a person decided, and no duplicate effect — the count the
    // family exists to keep at zero.
    assert_eq!(f8["totals"]["duplicate_effects"], json!(0), "{f8}");
    assert_eq!(f8["totals"]["verified"], json!(1));
    assert_eq!(
        f8["variants"][0]["repetitions"][0]["tool_calls"],
        json!(1),
        "{f8}"
    );
    assert_eq!(
        f8["totals"]["interventions"],
        json!(1),
        "one approval decided by a person: {f8}"
    );
    // The case's evidence followed the run the approval continued it in.
    let (_, list, _) = call(
        &app,
        "GET",
        "/datasets/campaign-desk/versions/1/evaluations",
        None,
    )
    .await;
    let case = list["evaluations"][0]["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["case_id"] == "twice")
        .cloned()
        .unwrap();
    assert_eq!(case["status"], json!("done"), "{case}");
    assert_eq!(
        case["resumed_run_ids"].as_array().map(Vec::len),
        Some(1),
        "{case}"
    );
    assert_eq!(case["tool_calls"], json!(["post"]));
    let f3 = report["families"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == "F3")
        .unwrap();
    assert_eq!(f3["totals"]["unauthorized_attempts"], json!(1), "{f3}");
    assert_eq!(f3["totals"]["unauthorized_refused"], json!(1));
    assert_eq!(f3["totals"]["duplicate_effects"], json!(0));
    assert!(f3["totals"]["tokens"].is_number());
    assert_eq!(
        report["conditions"]["server_version"],
        json!(env!("CARGO_PKG_VERSION"))
    );

    // The record, as Markdown.
    let (status, _, text) = call(&app, "GET", "/campaign?format=markdown", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(text.starts_with("# Campaign run"), "{text}");
    assert!(text.contains("## F8 —"), "{text}");
    assert!(text.contains("| campaign-desk@1 `twice` | 1 ("), "{text}");
    assert!(
        text.contains("| 0 | 1 |") || text.contains("| 0 (0) | 0 | 1 |"),
        "duplicates 0, interventions 1 on the row: {text}"
    );
    assert!(
        text.contains("## F1 —") && text.contains("Could not run: no variant recorded yet"),
        "{text}"
    );

    let _ = std::fs::remove_dir_all(store);
}
