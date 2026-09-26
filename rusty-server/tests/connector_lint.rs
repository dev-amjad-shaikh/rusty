//! A manifest that lacks what its vendor requires is refused at
//! registration with the exact fix, and every shipped pack passes the same
//! lint — the guard that keeps a hand-written manifest from shipping with
//! a request the vendor will reject.

use std::path::PathBuf;

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::connector::{lint_manifest, ConnectorManifest};
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

fn app() -> (Router, PathBuf) {
    let store = std::env::temp_dir().join(format!("rusty-connector-lint-{}", uuid::Uuid::new_v4()));
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone());
    (router(GraphRegistry::new(), config), store)
}

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(value) => { builder = builder.header("content-type", "application/json"); Body::from(value.to_string()) }
        None => Body::empty(),
    };
    let response = app.clone().oneshot(builder.body(body).unwrap()).await.unwrap();
    let status = response.status();
    let bytes: Bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap_or(Value::Null) };
    (status, value)
}

fn notion(headers: Value) -> Value {
    json!({
        "id": "notion-test", "version": "1", "display_name": "Notion", "description": "d",
        "documentation_url": "https://developers.notion.com", "base_url": "https://api.notion.com",
        "connection_specification": {"type": "object", "additionalProperties": false, "required": ["credentials"], "properties": {"credentials": {"type": "object", "additionalProperties": false, "required": ["integration_token"], "properties": {"integration_token": {"type": "string", "rusty_secret": true}}}}},
        "operations": [{
            "name": "check-connection", "description": "who am I", "method": "GET", "path": "/v1/users/me",
            "effect": "read_only", "params_schema": {"type": "object"}, "headers": headers,
            "auth": [{"style": "bearer", "token": "{credentials.integration_token}"}], "max_response_bytes": null
        }],
        "check": "check-connection"
    })
}

#[tokio::test]
async fn a_manifest_missing_what_its_vendor_requires_is_refused_with_the_fix() {
    let (app, store) = app();
    let (status, v) = call(&app, "POST", "/connectors", Some(notion(json!([])))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    let message = v.to_string();
    assert!(message.contains("Notion-Version"), "{message}");
    assert!(message.contains("2022-06-28"), "the fix is spelled out: {message}");
    let (status, v) = call(&app, "POST", "/connectors", Some(notion(json!([["Notion-Version", "2022-06-28"]])))).await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    let _ = std::fs::remove_dir_all(store);
}

#[test]
fn every_shipped_pack_passes_the_lint() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../catalog");
    let mut seen = 0;
    for entry in std::fs::read_dir(&root).unwrap().flatten() {
        let path = entry.path().join("manifest.json");
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let manifest: ConnectorManifest = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let findings = lint_manifest(&manifest);
        assert!(findings.is_empty(), "{}: {}", path.display(), findings.iter().map(ToString::to_string).collect::<Vec<_>>().join("; "));
        seen += 1;
    }
    assert!(seen >= 5, "the catalog ships packs: {seen}");
}
