//! Integration tests for RBAC scope enforcement (EP-11-S10 AC 3).
//!
//! Asserts that unauthorized calls receive [`AdmissionReason::Unauthorized`]
//! carried verbatim, and that the refusal is enumeration-safe: identical
//! responses for existing and nonexistent resources.

use std::path::PathBuf;

use axum::Router;
use axum::body::to_bytes;
use axum::http::{Request, StatusCode};
use rusty_agent_server::{GraphRegistry, ServerConfig, router};
use serde_json::{Value, json};
use tower::ServiceExt;

fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!("rusty-server-scope-test-{}", uuid::Uuid::new_v4()))
}

fn scope_test_app(api_key: &str, scopes: Vec<&str>) -> (Router, PathBuf) {
    let store = temp_store();
    let mut config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone());
    config.api_key = Some(api_key.to_string());
    config.api_key_scopes = std::iter::once((
        api_key.to_string(),
        scopes.iter().map(|s| s.to_string()).collect(),
    ))
    .collect();
    (router(GraphRegistry::new(), config), store)
}

async fn call_raw(
    app: &Router,
    method: &str,
    uri: &str,
    api_key: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(key) = api_key {
        builder = builder.header("x-api-key", key);
    }
    if method == "POST" || method == "PUT" {
        builder = builder.header("content-type", "application/json");
    }
    let response = app
        .clone()
        .oneshot(builder.body(axum::body::Body::empty()).unwrap())
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

#[tokio::test]
async fn missing_api_key_returns_admission_unauthorized() {
    let (app, store) = scope_test_app("s3cret", vec!["system:read"]);

    let (status, body) = call_raw(&app, "GET", "/ok", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["reason"], json!("Unauthorized"));
    assert_eq!(
        body["type"],
        json!("https://rusty.dev/problems/admission-refused")
    );

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn invalid_api_key_returns_admission_unauthorized() {
    let (app, store) = scope_test_app("s3cret", vec!["system:read"]);

    let (status, body) = call_raw(&app, "GET", "/ok", Some("wrong")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["reason"], json!("Unauthorized"));
    assert_eq!(
        body["type"],
        json!("https://rusty.dev/problems/admission-refused")
    );

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn insufficient_scope_returns_admission_unauthorized() {
    // Key has only "system:read"; /threads requires "threads:create".
    let (app, store) = scope_test_app("s3cret", vec!["system:read"]);

    let (status, body) = call_raw(&app, "POST", "/threads", Some("s3cret")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["reason"], json!("Unauthorized"));
    assert_eq!(
        body["type"],
        json!("https://rusty.dev/problems/admission-refused")
    );

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn enumeration_safe_identical_response_for_existing_and_nonexistent() {
    // Key has no scopes at all — every scoped route is forbidden.
    let (app, store) = scope_test_app("s3cret", vec![]);

    // A route that exists but caller lacks scope for.
    let (status_existing, body_existing) = call_raw(&app, "POST", "/threads", Some("s3cret")).await;

    // A route that does not exist (no handler mounted) but is declared
    // in the scope table, so the scope check runs before handler logic.
    let (status_nonexistent, body_nonexistent) =
        call_raw(&app, "POST", "/nonexistent/route", Some("s3cret")).await;

    // Both must be forbidden with the identical AdmissionReason shape.
    // Because the scope table covers both paths, the check happens before
    // any handler can probe resource existence — enumeration-safe.
    assert_eq!(status_existing, StatusCode::FORBIDDEN);
    assert_eq!(status_nonexistent, StatusCode::FORBIDDEN);
    assert_eq!(body_existing, body_nonexistent);
    assert_eq!(body_existing["reason"], json!("Unauthorized"));

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn valid_scope_allows_access() {
    // Key has the exact scope required for POST /threads.
    let (app, store) = scope_test_app("s3cret", vec!["threads:create"]);

    // The call is authorized; the handler will see an empty JSON body and
    // return some error other than 403/401, proving the scope check passed.
    let (status, _body) = call_raw(&app, "POST", "/threads", Some("s3cret")).await;
    assert_ne!(status, StatusCode::FORBIDDEN);
    assert_ne!(status, StatusCode::UNAUTHORIZED);

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn wildcard_scope_grants_access() {
    // Key has a collection-level wildcard scope that matches any 2-segment
    // required scope (e.g. threads:create, system:read).
    let (app, store) = scope_test_app("s3cret", vec!["*:*"]);

    let (status, _body) = call_raw(&app, "POST", "/threads", Some("s3cret")).await;
    assert_ne!(status, StatusCode::FORBIDDEN);
    assert_ne!(status, StatusCode::UNAUTHORIZED);

    let _ = std::fs::remove_dir_all(store);
}
