//! CORS integration tests: `router()` layers a permissive `CorsLayer`, so
//! browser clients (e.g. the Studio) can call the API cross-origin. Asserts
//! that an OPTIONS preflight is answered with CORS headers *without* hitting
//! the API-key middleware, and that ordinary responses carry
//! `access-control-allow-origin`. Driven in-process via
//! `tower::ServiceExt::oneshot` (no sockets).

use std::path::PathBuf;

use axum::body::{to_bytes, Body};
use axum::http::{header, Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::prelude::*;
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
use serde_json::json;
use tower::ServiceExt;

/// Unique temp store root, removed at the end of each test (best effort).
fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!("rusty-server-cors-test-{}", uuid::Uuid::new_v4()))
}

fn test_app() -> (Router, PathBuf) {
    let store = temp_store();
    let spec = StateSpec::new().channel("log", Reducer::Append);
    let mut builder = GraphBuilder::new();
    builder.add_node("only", |_ctx: NodeContext| async {
        Ok(NodeOutput::update("log", json!("only")))
    });
    builder.set_entry_point("only");
    let mut registry = GraphRegistry::new();
    registry.register("pipeline", builder.compile().unwrap(), spec);
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone());
    (router(registry, config), store)
}

#[tokio::test]
async fn options_preflight_gets_cors_headers() {
    let (app, store) = test_app();
    let response = app
        .oneshot(
            Request::builder()
                .method("OPTIONS")
                .uri("/threads")
                .header(header::ORIGIN, "http://localhost:8000")
                .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
                .header(
                    header::ACCESS_CONTROL_REQUEST_HEADERS,
                    "content-type,x-api-key",
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "preflight should succeed"
    );
    let headers = response.headers();
    assert_eq!(
        headers.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).unwrap(),
        "*",
        "permissive layer should allow any origin"
    );
    assert!(
        headers.contains_key(header::ACCESS_CONTROL_ALLOW_METHODS),
        "preflight should echo allowed methods"
    );
    assert!(
        headers.contains_key(header::ACCESS_CONTROL_ALLOW_HEADERS),
        "preflight should echo allowed headers"
    );
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn ordinary_responses_carry_allow_origin() {
    let (app, store) = test_app();
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/ok")
                .header(header::ORIGIN, "null") // file:// pages send Origin: null
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .unwrap(),
        "*"
    );
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert!(!bytes.is_empty(), "/ok should still answer normally");
    let _ = std::fs::remove_dir_all(store);
}
