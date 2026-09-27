//! MCP servers Rusty connects to: an operator-gated, admin-only surface
//! that spawns a stdio server, declares each tool's effect, and mounts the
//! result as `{server}.{tool}` on the live tool source.
//!
//! The server under test is a twenty-line Python MCP server written to a
//! temp file — real stdio, real JSON-RPC, no network. Skipped (with a
//! note) where `python3` is not on the PATH.

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::tool::ToolSource;
use rusty_agent_server::{router, GraphRegistry, McpTools, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

const TOY_SERVER: &str = r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    m = json.loads(line)
    method, rid = m.get("method"), m.get("id")
    if method == "initialize":
        res = {"protocolVersion": m["params"].get("protocolVersion", "2024-11-05"), "capabilities": {"tools": {}}, "serverInfo": {"name": "toy", "version": "0.1"}}
    elif method == "notifications/initialized":
        continue
    elif method == "tools/list":
        res = {"tools": [
            {"name": "echo", "description": "Echoes text back.", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}, "annotations": {"readOnlyHint": True}},
            {"name": "wipe", "description": "", "inputSchema": {"type": "object"}}
        ]}
    elif method == "tools/call":
        res = {"content": [{"type": "text", "text": "echo:" + m["params"]["arguments"].get("text", "")}]}
    else:
        if rid is None:
            continue
        sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": rid, "error": {"code": -32601, "message": "unknown"}}) + "\n"); sys.stdout.flush(); continue
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": rid, "result": res}) + "\n"); sys.stdout.flush()
"#;

fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!("rusty-server-mcp-servers-{}", uuid::Uuid::new_v4()))
}

fn python() -> Option<String> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join("python3"))
            .find(|candidate| candidate.is_file())
            .map(|p| p.display().to_string())
    })
}

fn app(enabled: bool) -> (Router, Arc<McpTools>, PathBuf) {
    let store = temp_store();
    let cell = McpTools::new();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_mcp_tools(Arc::clone(&cell))
        .with_mcp_stdio(enabled);
    (router(GraphRegistry::new(), config), cell, store)
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

#[tokio::test]
async fn a_deployment_that_did_not_opt_in_spawns_nothing() {
    let (app, _cell, store) = app(false);
    let (status, body) = call(&app, "GET", "/mcp/servers", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["enabled"], false);
    assert_eq!(body["servers"].as_array().unwrap().len(), 0);

    let (status, body) = call(
        &app,
        "POST",
        "/mcp/servers/probe",
        Some(json!({ "command": "true" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"], "mcp_disabled");
    assert!(body["message"]
        .as_str()
        .unwrap()
        .contains("RUSTY_MCP_STDIO=1"));

    let (status, _) = call(
        &app,
        "POST",
        "/mcp/servers",
        Some(json!({ "name": "x", "command": "true" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn a_server_saves_only_after_it_answers_and_mounts_under_declared_effects() {
    let Some(python) = python() else {
        eprintln!("python3 not on PATH — skipping the stdio round trip");
        return;
    };
    let (app, cell, store) = app(true);
    std::fs::create_dir_all(&store).unwrap();
    let script = store.join("toy_mcp.py");
    std::fs::write(&script, TOY_SERVER).unwrap();
    let script = script.display().to_string();

    // A name that is not a tool prefix, and a command that does not exist.
    let (status, body) = call(
        &app,
        "POST",
        "/mcp/servers",
        Some(json!({ "name": "Toy", "command": &python })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = call(
        &app,
        "POST",
        "/mcp/servers",
        Some(json!({ "name": "toy", "command": "/definitely/not/a/binary" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(body["error"], "mcp_failed");
    let (_, listed) = call(&app, "GET", "/mcp/servers", None).await;
    assert_eq!(
        listed["servers"].as_array().unwrap().len(),
        0,
        "a failed mount stores nothing"
    );

    // Probe: the server's annotations suggest; nothing is stored.
    let (status, probed) = call(
        &app,
        "POST",
        "/mcp/servers/probe",
        Some(json!({ "command": &python, "args": [&script] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{probed}");
    assert_eq!(probed["server"]["name"], "toy");
    let tools = probed["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0]["name"], "echo");
    assert_eq!(tools[0]["suggested_effect"], "read_only");
    assert_eq!(tools[1]["suggested_effect"], "non_idempotent");
    assert!(cell.tools().is_empty(), "a probe mounts nothing");

    // Register, declaring echo read-only and leaving wipe undeclared.
    let (status, created) = call(
        &app,
        "POST",
        "/mcp/servers",
        Some(json!({
            "name": "toy",
            "command": &python,
            "args": [&script],
            "env": [{ "name": "TOY_TOKEN", "value": "s3cret", "secret": true }, { "name": "TOY_MODE", "value": "test" }],
            "tool_effects": { "echo": "read_only" }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().unwrap().to_owned();
    assert!(id.starts_with("mcp-"));
    assert_eq!(created["status"]["state"], "mounted");
    assert_eq!(created["status"]["server"], "toy 0.1");
    let mounted = created["status"]["tools"].as_array().unwrap();
    assert_eq!(mounted[0]["name"], "toy.echo");
    assert_eq!(mounted[0]["effect"], "read_only");
    assert_eq!(mounted[1]["name"], "toy.wipe");
    assert_eq!(
        mounted[1]["effect"], "non_idempotent",
        "undeclared is a write"
    );
    assert_eq!(mounted[1]["description"], "wipe on the toy MCP server.");
    // Secrets are served as names only.
    let env = created["env"].as_array().unwrap();
    let token = env.iter().find(|e| e["name"] == "TOY_TOKEN").unwrap();
    assert_eq!(token["secret"], true);
    assert!(token.get("value").is_none(), "{token}");
    let stored =
        std::fs::read_to_string(store.join("mcp-servers").join(format!("{id}.json"))).unwrap();
    assert!(
        !stored.contains("s3cret"),
        "the record holds ciphertext only"
    );

    // The live source carries the tools; calling one reaches the process.
    let live = cell.tools();
    let names: Vec<&str> = live.iter().map(|t| t.name()).collect();
    assert_eq!(names, vec!["toy.echo", "toy.wipe"]);
    let echo = live.iter().find(|t| t.name() == "toy.echo").unwrap();
    assert_eq!(echo.effect(), rusty_agent_runtime::record::Effect::ReadOnly);
    let answer = echo.call(json!({ "text": "hi" })).await.unwrap();
    assert_eq!(answer, json!("echo:hi"));

    // A second server may not take the same name.
    let (status, _) = call(
        &app,
        "POST",
        "/mcp/servers",
        Some(json!({ "name": "toy", "command": &python, "args": [&script] })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Listing shows it mounted; deleting stops it and empties the source.
    let (_, listed) = call(&app, "GET", "/mcp/servers", None).await;
    assert_eq!(listed["servers"][0]["status"]["state"], "mounted");
    let (status, _) = call(&app, "DELETE", &format!("/mcp/servers/{id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(cell.tools().is_empty());
    let (_, listed) = call(&app, "GET", "/mcp/servers", None).await;
    assert_eq!(listed["servers"].as_array().unwrap().len(), 0);
    let _ = std::fs::remove_dir_all(store);
}
