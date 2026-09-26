//! The estate, backed up while the server runs and restored onto an empty
//! store: what one deployment held — its agents, skills, threads, runs,
//! people — a second one holds after the restore, through the same API;
//! a store that already holds anything is never overwritten.
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::error::Result as RustyResult;
use rusty_agent_runtime::llm::{ChatMessage, ChatModel, ChatResponse};
use rusty_agent_runtime::react::{create_react_agent, MESSAGES_CHANNEL};
use rusty_agent_runtime::state::{Reducer, StateSpec};
use rusty_agent_runtime::tool::ToolRegistry;
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

struct Brief;
#[async_trait::async_trait]
impl ChatModel for Brief {
    async fn chat(&self, _messages: &[ChatMessage], _t: &[Value]) -> RustyResult<ChatResponse> {
        Ok(ChatResponse { message: ChatMessage::assistant("noted"), model: Some("brief".into()), usage: None })
    }
}

fn config(store: &Path) -> ServerConfig {
    ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.to_path_buf())
}

fn app(config: ServerConfig) -> Router {
    let tools = ToolRegistry::new();
    let graph = create_react_agent(Arc::new(Brief), tools.clone()).unwrap();
    let spec = StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry.register_with_tools("react_agent", graph, spec, &tools).unwrap();
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

#[tokio::test]
async fn the_estate_is_backed_up_running_and_restored_onto_an_empty_store() {
    let root = std::env::temp_dir().join(format!("rusty-estate-{}", uuid::Uuid::new_v4()));
    let store_a: PathBuf = root.join("a");
    let backups = root.join("backups");
    let app_a = app(config(&store_a).with_backup_dir(&backups));

    // An estate: an agent, a skill, a conversation with a run.
    let (status, made) = call(&app_a, "POST", "/assistants", Some(json!({"assistant_id": "desk", "name": "Desk", "graph": "react_agent", "config": {"instructions": "be brief"}}))).await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let (status, skill) = call(&app_a, "POST", "/skills", Some(json!({"skill_md": "---\nname: count-well\ndescription: How to count.\n---\n\n# Count well\n\nCount, then say the number.\n"}))).await;
    assert_eq!(status, StatusCode::CREATED, "{skill}");
    let (_, thread) = call(&app_a, "POST", "/threads", Some(json!({"graph": "react_agent"}))).await;
    let thread_id = thread["thread_id"].as_str().unwrap().to_owned();
    let (status, run) = call(&app_a, "POST", &format!("/threads/{thread_id}/runs/wait"), Some(json!({"input": {"messages": [{"role": "user", "content": "how many?"}]}, "assistant_id": "desk"}))).await;
    assert_eq!(status, StatusCode::OK, "{run}");
    let run_id = run["run_id"].as_str().unwrap().to_owned();

    // The estate says what it holds, and has no backups yet.
    let (status, estate) = call(&app_a, "GET", "/estate", None).await;
    assert_eq!(status, StatusCode::OK, "{estate}");
    assert_eq!(estate["counts"]["agents"], json!(1), "{estate}");
    assert_eq!(estate["counts"]["skill_revisions"], json!(1));
    assert_eq!(estate["counts"]["threads"], json!(1));
    assert_eq!(estate["counts"]["runs"], json!(1));
    assert_eq!(estate["backups"], json!([]));
    assert_eq!(estate["restored_from"], Value::Null);

    // A backup, taken while the server runs, with its manifest.
    let (status, backup) = call(&app_a, "POST", "/estate/backups", None).await;
    assert_eq!(status, StatusCode::CREATED, "{backup}");
    let archive = backup["path"].as_str().unwrap().to_owned();
    assert!(archive.starts_with(backups.to_str().unwrap()), "{archive}");
    assert!(backup["bytes"].as_u64().unwrap() > 0);
    assert_eq!(backup["manifest"]["counts"], estate["counts"], "{backup}");
    assert_eq!(backup["manifest"]["by"]["principal_id"], json!("dev"));
    let (_, estate) = call(&app_a, "GET", "/estate", None).await;
    assert_eq!(estate["backups"][0]["path"], json!(archive), "the backup is listed: {estate}");
    assert_eq!(estate["backups"][0]["manifest"]["counts"]["agents"], json!(1));

    // A second deployment on an empty store, restored from it: the same
    // estate through the same API — the agent, the skill, the run, the thread.
    let store_b = root.join("b");
    let app_b = app(config(&store_b).with_restore_from(&archive));
    let (status, estate_b) = call(&app_b, "GET", "/estate", None).await;
    assert_eq!(status, StatusCode::OK, "{estate_b}");
    assert_eq!(estate_b["counts"], estate["counts"], "{estate_b}");
    assert_eq!(estate_b["restored_from"]["archive"], json!(archive), "{estate_b}");
    assert_eq!(estate_b["restore"]["outcome"], json!("restored"), "{estate_b}");
    let (status, desk) = call(&app_b, "GET", "/assistants/desk", None).await;
    assert_eq!(status, StatusCode::OK, "{desk}");
    assert_eq!(desk["name"], json!("Desk"));
    let (status, skill) = call(&app_b, "GET", "/skills/count-well", None).await;
    assert_eq!(status, StatusCode::OK, "{skill}");
    assert_eq!(skill["revision"], json!(1));
    let (status, run) = call(&app_b, "GET", &format!("/runs/{run_id}"), None).await;
    assert_eq!(status, StatusCode::OK, "the run is recalled from its record: {run}");
    assert_eq!(run["status"], json!("success"));
    let (status, thread) = call(&app_b, "GET", &format!("/threads/{thread_id}"), None).await;
    assert_eq!(status, StatusCode::OK, "{thread}");

    // A store that holds anything is never overwritten: the request is
    // recorded as skipped and the estate stands.
    let app_c = app(config(&store_b).with_restore_from(&archive));
    let (_, estate_c) = call(&app_c, "GET", "/estate", None).await;
    assert_eq!(estate_c["counts"]["agents"], json!(1), "{estate_c}");
    let (status, still) = call(&app_c, "GET", "/assistants/desk", None).await;
    assert_eq!(status, StatusCode::OK, "{still}");

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_restore_into_a_store_that_holds_anything_is_skipped_and_says_why() {
    let root = std::env::temp_dir().join(format!("rusty-estate-skip-{}", uuid::Uuid::new_v4()));
    let store = root.join("store");
    std::fs::create_dir_all(store.join("assistants")).unwrap();
    std::fs::write(store.join("assistants").join("desk.json"), b"{}").unwrap();
    let archive = root.join("estate-x.tar.gz");
    std::fs::write(&archive, b"not even an archive").unwrap();
    let outcome = rusty_agent_server::estate::restore_if_asked(&store, Some(&archive)).unwrap();
    match outcome {
        rusty_agent_server::estate::RestoreOutcome::Skipped { reason, .. } => assert!(reason.contains("not empty"), "{reason}"),
        other => panic!("{other:?}"),
    }
    assert!(store.join("assistants").join("desk.json").exists(), "the estate stands");
    let _ = std::fs::remove_dir_all(root);
}
