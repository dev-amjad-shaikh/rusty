//! Read-backs for any connector: the writes a version leaves to guess after
//! a lost answer are named, a read-back is proposed from the connector's
//! own reads, and adopting it is a new version the connection moves to
//! without a check — a read-back changes no request the check would make.
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::connector::{ConnectorManifest, ConnectorOperation, HttpMethod, OperationEffect};
use rusty_agent_runtime::error::Result as RustyResult;
use rusty_agent_runtime::llm::{ChatMessage, ChatModel, ChatResponse};
use rusty_agent_runtime::react::{create_react_agent, MESSAGES_CHANNEL};
use rusty_agent_runtime::state::{Reducer, StateSpec};
use rusty_agent_runtime::tool::ToolRegistry;
use rusty_agent_server::{router, ConnectionTools, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

struct Quiet;

#[async_trait::async_trait]
impl ChatModel for Quiet {
    async fn chat(&self, _m: &[ChatMessage], _t: &[Value]) -> RustyResult<ChatResponse> {
        Ok(ChatResponse { message: ChatMessage::assistant("noted"), model: Some("quiet".into()), usage: None })
    }
}

fn app(store: &std::path::Path) -> Router {
    let connection_tools = ConnectionTools::new();
    let mut tools = ToolRegistry::new();
    tools.attach(Arc::clone(&connection_tools) as Arc<dyn rusty_agent_runtime::tool::ToolSource>);
    let graph = create_react_agent(Arc::new(Quiet), tools.clone()).unwrap();
    let spec = StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry.register_with_tools("react_agent", graph, spec, &tools).unwrap();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.to_path_buf()).with_connection_tools(connection_tools);
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

/// A table API as an OpenAPI import leaves it: a check read, a filtering
/// read and a write on the same path, and no read-back anywhere.
fn imported_manifest() -> ConnectorManifest {
    let spec = json!({"$schema": "http://json-schema.org/draft-07/schema#", "type": "object", "required": ["instance"], "properties": {"instance": {"type": "string"}}, "additionalProperties": false});
    let op = |name: &str, method: HttpMethod, path: &str, effect: OperationEffect, params: Value| ConnectorOperation {
        name: name.to_owned(),
        description: format!("The {name} operation."),
        method,
        path: path.to_owned(),
        effect,
        params_schema: params,
        headers: Vec::new(),
        auth: Vec::new(),
        max_response_bytes: None,
        reconcile: None,
    };
    ConnectorManifest::new(
        "ticketing-import",
        "1",
        "Ticketing Import",
        "A ticketing API read from its published description.",
        "https://example.invalid/docs",
        "https://{instance}.example.invalid",
        spec,
        vec![
            op("check-connection", HttpMethod::Get, "/api/now/table/sys_user_group", OperationEffect::ReadOnly, json!({"type": "object"})),
            op("list-records", HttpMethod::Get, "/api/now/table/{table}", OperationEffect::ReadOnly, json!({"type": "object", "required": ["table"], "properties": {"table": {"type": "string"}, "sysparm_query": {"type": "string"}, "sysparm_limit": {"type": "integer"}}})),
            op("create-record", HttpMethod::Post, "/api/now/table/{table}", OperationEffect::Compensatable, json!({"type": "object", "required": ["table", "short_description"], "properties": {"table": {"type": "string"}, "short_description": {"type": "string"}, "description": {"type": "string"}}})),
        ],
        "check-connection",
    )
    .unwrap()
}

#[tokio::test]
async fn a_write_without_a_read_back_is_named_a_read_back_is_proposed_and_adopting_it_moves_the_connection() {
    let store = std::env::temp_dir().join(format!("rusty-read-backs-{}", uuid::Uuid::new_v4()));
    let app = app(&store);
    let (status, receipt) = call(&app, "POST", "/connectors", Some(serde_json::to_value(imported_manifest()).unwrap())).await;
    assert_eq!(status, StatusCode::CREATED, "{receipt}");
    let hash = receipt["hash"].as_str().unwrap().to_owned();
    let (status, instance) = call(&app, "POST", "/connectors/instances", Some(json!({"manifest_hash": hash, "config": {"instance": "dev-twin"}}))).await;
    assert_eq!(status, StatusCode::CREATED, "{instance}");
    let instance_id = instance["instance_id"].as_str().unwrap().to_owned();

    // The write would guess; its read-back is proposed from the read on
    // its own path, bound to the write's short description and table.
    let (status, proposed) = call(&app, "GET", &format!("/connectors/{hash}/read-backs"), None).await;
    assert_eq!(status, StatusCode::OK, "{proposed}");
    assert_eq!(proposed["version"], "1");
    assert_eq!(proposed["declared"], json!([]));
    assert_eq!(proposed["unproposable"], json!([]));
    let proposals = proposed["proposals"].as_array().unwrap();
    assert_eq!(proposals.len(), 1, "{proposed}");
    assert_eq!(proposals[0]["write"], "create-record");
    assert_eq!(proposals[0]["operation"], "list-records");
    assert_eq!(proposals[0]["arguments"], json!({"sysparm_query": "short_description=$short_description", "table": "$table", "sysparm_limit": 5}));

    // A read-back for a read the connector lacks is refused.
    let (status, refused) = call(&app, "POST", &format!("/connectors/{hash}/read-backs"), Some(json!({"writes": ["create-record"], "adopt": [{"operation": "find-record", "arguments": {}}]}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");

    // Adopting the proposal is version 2 in the library, and the
    // connection moves to it — no check, the configuration is the one
    // already proven.
    let (status, adopted) = call(&app, "POST", &format!("/connectors/{hash}/read-backs"), Some(json!({"writes": ["create-record"], "adopt": [proposals[0].clone()], "instance_id": instance_id}))).await;
    assert_eq!(status, StatusCode::CREATED, "{adopted}");
    assert_eq!(adopted["version"], "2");
    assert_eq!(adopted["moved"], json!(instance_id));
    let new_hash = adopted["hash"].as_str().unwrap().to_owned();
    assert_ne!(new_hash, hash);
    let (status, moved) = call(&app, "GET", &format!("/connectors/instances/{instance_id}/catalog"), None).await;
    assert_eq!(status, StatusCode::OK, "{moved}");
    let create = moved["tools"].as_array().unwrap().iter().find(|t| t["name"] == "ticketing-import.create-record").expect("the write is a tool");
    assert_eq!(create["reconcile"], "ticketing-import.list-records", "{create}");
    // Nothing is left to propose on the new version; the read-back is declared.
    let (_, after) = call(&app, "GET", &format!("/connectors/{new_hash}/read-backs"), None).await;
    assert_eq!(after["proposals"], json!([]));
    assert_eq!(after["declared"], json!([{"write": "create-record", "operation": "list-records"}]));

    let _ = std::fs::remove_dir_all(store);
}

/// Two connections of one connector keep the catalog whole: each gets its
/// tools under a name the catalog accepts, and `/info` lists them all.
#[tokio::test]
async fn two_connections_of_one_connector_name_their_tools_apart_and_the_catalog_stays_whole() {
    let store = std::env::temp_dir().join(format!("rusty-two-connections-{}", uuid::Uuid::new_v4()));
    let app = app(&store);
    let (status, receipt) = call(&app, "POST", "/connectors", Some(serde_json::to_value(imported_manifest()).unwrap())).await;
    assert_eq!(status, StatusCode::CREATED, "{receipt}");
    let hash = receipt["hash"].as_str().unwrap().to_owned();
    for instance in ["dev-one", "dev-two"] {
        let (status, made) = call(&app, "POST", "/connectors/instances", Some(json!({"manifest_hash": hash, "config": {"instance": instance}}))).await;
        assert_eq!(status, StatusCode::CREATED, "{made}");
    }
    let (status, info) = call(&app, "GET", "/info", None).await;
    assert_eq!(status, StatusCode::OK);
    let graph = info["graphs"].as_array().unwrap().iter().find(|g| g["name"] == "react_agent").expect("the graph");
    let names: Vec<&str> = graph["tools"].as_array().unwrap().iter().filter_map(|t| t["name"].as_str()).collect();
    let creates: Vec<&&str> = names.iter().filter(|n| n.starts_with("ticketing-import-") && n.ends_with(".create-record")).collect();
    assert_eq!(creates.len(), 2, "one create tool per connection, named apart: {names:?}");
    assert!(names.iter().all(|n| !n.contains('@')), "{names:?}");
    let _ = std::fs::remove_dir_all(store);
}
