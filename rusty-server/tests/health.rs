//! Health endpoint integration tests (EP-10-S10).
//!
//! Driven in-process via `tower::ServiceExt::oneshot` (no sockets).

use std::path::PathBuf;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::prelude::*;
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
use serde_json::json;
use tower::ServiceExt;

fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!("rusty-server-health-test-{}", uuid::Uuid::new_v4()))
}

fn test_app() -> (Router, PathBuf) {
    let store = temp_store();
    let spec = StateSpec::new().channel("log", Reducer::Append);
    let mut builder = GraphBuilder::new();
    builder.add_node("only", |_ctx: NodeContext| async {
        Ok(NodeOutput::update("log", json!("only")))
    });
    builder.set_entry_point("only");
    let (graph, spec) = (builder.compile().unwrap(), spec);
    let mut registry = GraphRegistry::new();
    registry.register("only", graph, spec);
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone());
    (router(registry, config), store)
}

#[tokio::test]
async fn health_returns_200_with_components() {
    let (app, _store) = test_app();

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(
        json["status"], "up",
        "overall status should be up on a fresh server"
    );

    let components = json["components"]
        .as_array()
        .expect("components should be an array");
    assert!(
        !components.is_empty(),
        "components array should not be empty"
    );

    let names: Vec<&str> = components
        .iter()
        .map(|c| c["component"].as_str().unwrap())
        .collect();

    // Every component in the report must itself report up on a fresh server.
    for c in components {
        assert_eq!(
            c["status"], "up",
            "component {} should be up",
            c["component"]
        );
    }

    // Deterministic ordering (alphabetical).
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "components should be sorted alphabetically");

    // Expected components for the current surface.
    let expected = vec![
        "artifact_retention",
        "broker",
        "checkpointer",
        "connectors",
        "deployment",
        "evaluation_state",
        "knowledge",
        "receipt_keyring",
        "skills",
        "store",
    ];
    for name in &expected {
        assert!(
            names.contains(name),
            "expected component {name} to be present"
        );
    }
}
