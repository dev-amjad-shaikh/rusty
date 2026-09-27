//! Two people on one platform: a person's runs and conversations are
//! theirs. A builder sees the runs that acted for them and the runs that
//! acted for nobody in particular (a service's); another person's run and
//! thread are absent — 404, never 403 — on every read path the studio
//! uses, and a run on another person's thread is refused the same way. An
//! administrator acts for anyone; an auditor reads everyone's.
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::error::Result as RustyResult;
use rusty_agent_runtime::llm::{ChatMessage, ChatModel, ChatResponse};
use rusty_agent_runtime::react::{create_react_agent, MESSAGES_CHANNEL};
use rusty_agent_runtime::state::{Reducer, StateSpec};
use rusty_agent_runtime::tool::ToolRegistry;
use rusty_agent_server::{router, GraphRegistry, Principal, PrincipalKind, Role, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

struct Brief;
#[async_trait::async_trait]
impl ChatModel for Brief {
    async fn chat(&self, _messages: &[ChatMessage], _t: &[Value]) -> RustyResult<ChatResponse> {
        Ok(ChatResponse {
            message: ChatMessage::assistant("noted"),
            model: Some("brief".into()),
            usage: None,
        })
    }
}

fn person(id: &str, role: Role) -> Principal {
    Principal {
        id: id.to_owned(),
        name: id.to_owned(),
        kind: PrincipalKind::User,
        roles: vec![role],
    }
}

fn app() -> (Router, PathBuf) {
    let store = std::env::temp_dir().join(format!("rusty-two-people-{}", uuid::Uuid::new_v4()));
    let tools = ToolRegistry::new();
    let graph = create_react_agent(Arc::new(Brief), tools.clone()).unwrap();
    let spec = StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry
        .register_with_tools("react_agent", graph, spec, &tools)
        .unwrap();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_principal("default", person("ada", Role::Admin), "ada-key")
        .with_principal("default", person("bob", Role::Builder), "bob-key")
        .with_principal("default", person("ana", Role::Builder), "ana-key")
        .with_principal("default", person("otto", Role::Operator), "otto-key")
        .with_principal("default", person("aud", Role::Auditor), "aud-key")
        .with_principal(
            "default",
            Principal {
                id: "svc".to_owned(),
                name: "nightly".to_owned(),
                kind: PrincipalKind::Service,
                roles: vec![Role::Builder],
            },
            "svc-key",
        );
    (router(registry, config), store)
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

/// One conversation as `key`: a thread and a finished run on it.
async fn converse(app: &Router, key: &str, words: &str) -> (String, String) {
    let (status, thread) = call(
        app,
        key,
        "POST",
        "/threads",
        Some(json!({"graph": "react_agent"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{thread}");
    let thread_id = thread["thread_id"].as_str().unwrap().to_owned();
    let (status, run) = call(
        app,
        key,
        "POST",
        &format!("/threads/{thread_id}/runs/wait"),
        Some(json!({"input": {"messages": [{"role": "user", "content": words}]}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{run}");
    (thread_id, run["run_id"].as_str().unwrap().to_owned())
}

async fn listed(app: &Router, key: &str) -> Vec<String> {
    let (status, runs) = call(app, key, "GET", "/runs", None).await;
    assert_eq!(status, StatusCode::OK, "{runs}");
    runs.as_array()
        .unwrap()
        .iter()
        .map(|r| r["run_id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn a_persons_runs_and_conversations_are_theirs() {
    let (app, store) = app();
    let (bob_thread, bob_run) =
        converse(&app, "bob-key", "remember that I prefer metric units").await;
    let (_, ana_run) = converse(&app, "ana-key", "remember that I prefer imperial units").await;
    let (_, svc_run) = converse(&app, "svc-key", "the nightly sweep").await;

    // The thread carries whose it is, stamped by the server.
    let (_, thread) = call(
        &app,
        "bob-key",
        "GET",
        &format!("/threads/{bob_thread}"),
        None,
    )
    .await;
    assert_eq!(
        thread["metadata"]["created_by"]["principal_id"],
        json!("bob"),
        "{thread}"
    );

    // Each person lists their own runs and the service's — not each other's.
    let bobs = listed(&app, "bob-key").await;
    assert!(
        bobs.contains(&bob_run) && bobs.contains(&svc_run) && !bobs.contains(&ana_run),
        "{bobs:?}"
    );
    let anas = listed(&app, "ana-key").await;
    assert!(
        anas.contains(&ana_run) && anas.contains(&svc_run) && !anas.contains(&bob_run),
        "{anas:?}"
    );

    // Another person's run is absent on every read path — the run, its
    // events, its receipt, its fixture — and so is their thread; a run on
    // it is refused the same way, so nobody continues another's
    // conversation. 404, never 403: nothing to probe.
    for path in [
        format!("/runs/{bob_run}"),
        format!("/runs/{bob_run}/events"),
        format!("/runs/{bob_run}/receipt"),
        format!("/runs/{bob_run}/fixture"),
        format!("/threads/{bob_thread}"),
    ] {
        let (status, body) = call(&app, "ana-key", "GET", &path, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}: {body}");
    }
    let (status, refused) = call(
        &app,
        "ana-key",
        "POST",
        &format!("/threads/{bob_thread}/runs/wait"),
        Some(json!({"input": {"messages": [{"role": "user", "content": "what do I prefer?"}]}})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{refused}");
    // Bob reads his own, every path.
    for path in [
        format!("/runs/{bob_run}"),
        format!("/runs/{bob_run}/events"),
        format!("/runs/{bob_run}/receipt"),
        format!("/threads/{bob_thread}"),
    ] {
        let (status, body) = call(&app, "bob-key", "GET", &path, None).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
    }
    // The service's run is everyone's.
    let (status, _) = call(&app, "ana-key", "GET", &format!("/runs/{svc_run}"), None).await;
    assert_eq!(status, StatusCode::OK);

    // An operator runs what exists and approves what waits, but another
    // person's conversation is not theirs to read either.
    let (status, _) = call(&app, "otto-key", "GET", &format!("/runs/{bob_run}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(!listed(&app, "otto-key").await.contains(&bob_run));

    // An administrator acts for anyone; an auditor reads everyone's.
    for key in ["ada-key", "aud-key"] {
        let all = listed(&app, key).await;
        assert!(
            all.contains(&bob_run) && all.contains(&ana_run) && all.contains(&svc_run),
            "{key}: {all:?}"
        );
        let (status, run) = call(&app, key, "GET", &format!("/runs/{bob_run}"), None).await;
        assert_eq!(status, StatusCode::OK, "{key}: {run}");
        assert_eq!(run["metadata"]["created_by"]["principal_id"], json!("bob"));
        let (status, _) = call(&app, key, "GET", &format!("/threads/{bob_thread}"), None).await;
        assert_eq!(status, StatusCode::OK, "{key}");
    }
    // The service key stays tenant-wide, as every key was before people existed.
    assert!(listed(&app, "svc-key").await.contains(&bob_run));

    // The rule survives the process: a run the server no longer holds is
    // recalled from its record, with the same answer for each person.
    let (app, _) = (
        router(
            {
                let tools = ToolRegistry::new();
                let graph = create_react_agent(Arc::new(Brief), tools.clone()).unwrap();
                let spec = StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages);
                let mut registry = GraphRegistry::new();
                registry
                    .register_with_tools("react_agent", graph, spec, &tools)
                    .unwrap();
                registry
            },
            ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
                .with_principal("default", person("bob", Role::Builder), "bob-key")
                .with_principal("default", person("ana", Role::Builder), "ana-key"),
        ),
        store.clone(),
    );
    let (status, _) = call(&app, "bob-key", "GET", &format!("/runs/{bob_run}"), None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "bob recalls his own run after a restart"
    );
    let (status, _) = call(&app, "ana-key", "GET", &format!("/runs/{bob_run}"), None).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "ana still cannot see bob's after a restart"
    );
    assert!(!listed(&app, "ana-key").await.contains(&bob_run));
    assert!(listed(&app, "bob-key").await.contains(&bob_run));

    let _ = std::fs::remove_dir_all(store);
}
