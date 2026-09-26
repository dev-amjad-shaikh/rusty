//! What belongs to a person is theirs: `user`-scoped memory and a
//! connection with a subject are bound to the signed-in person on every
//! path — write, read, query, forget; register, list, get, consent,
//! revoke, delete. An administrator acts for anyone; a service key (an
//! integration, nobody in particular) stays tenant-wide. Another person's
//! record reads as absent, the way a record outside the tenant does.

use std::path::PathBuf;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_server::{Principal, PrincipalKind, Role};
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

fn person(id: &str, role: Role) -> Principal {
    Principal { id: id.to_owned(), name: id.to_owned(), kind: PrincipalKind::User, roles: vec![role] }
}

fn app() -> (Router, PathBuf) {
    let store = std::env::temp_dir().join(format!("rusty-person-bound-{}", uuid::Uuid::new_v4()));
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_principal("default", person("ada", Role::Admin), "ada-key")
        .with_principal("default", person("bob", Role::Builder), "bob-key")
        .with_principal("default", person("cy", Role::Builder), "cy-key")
        .with_principal(
            "default",
            Principal { id: "svc".to_owned(), name: "integration".to_owned(), kind: PrincipalKind::Service, roles: vec![Role::Builder] },
            "svc-key",
        );
    (router(GraphRegistry::new(), config), store)
}

async fn call(app: &Router, key: &str, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri).header("X-Api-Key", key);
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
    let value = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap_or(Value::Null) };
    (status, value)
}

fn memory(user: &str, fact: &str) -> Value {
    json!({
        "kind": "fact",
        "scope": {"scope": "user", "id": user},
        "content": {"prefers": fact},
        "author": {"type": "human", "human_id": user},
    })
}

#[tokio::test]
async fn a_persons_memory_is_theirs_on_every_path() {
    let (app, store) = app();
    // Bob writes his own; writing as Cy is refused in words.
    let (status, wrote) = call(&app, "bob-key", "POST", "/memory", Some(memory("bob", "short answers"))).await;
    assert_eq!(status, StatusCode::CREATED, "{wrote}");
    let bobs = wrote["memory_id"].as_str().unwrap().to_owned();
    // Debug: what Bob sees right after writing.
    let (status, body) = call(&app, "bob-key", "POST", "/memory", Some(memory("cy", "long answers"))).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(body["message"].as_str().unwrap().contains("not yours (you are `bob`)"), "{body}");

    // Cy cannot name Bob's scope, cannot read Bob's record, and does not
    // see it in an unscoped query; Cy's own query works.
    let (status, body) = call(&app, "cy-key", "POST", "/memory/query", Some(json!({"scope": {"scope": "user", "id": "bob"}}))).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body) = call(&app, "cy-key", "GET", &format!("/memory/{bobs}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, body) = call(&app, "cy-key", "POST", "/memory/query", Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["records"].as_array().unwrap().is_empty(), "Bob's memory leaked to Cy: {body}");
    let (status, body) = call(&app, "cy-key", "POST", "/memory/forget", Some(json!({"memory_id": bobs, "reason": "erasure_request"}))).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, body) = call(&app, "cy-key", "POST", "/memory/forget_scope", Some(json!({"scope": {"scope": "user", "id": "bob"}, "reason": "erasure_request"}))).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // Bob sees his own, unscoped and scoped; the administrator sees it too.
    let (status, body) = call(&app, "bob-key", "POST", "/memory/query", Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["records"].as_array().unwrap().len(), 1, "{body}");
    let (status, body) = call(&app, "ada-key", "GET", &format!("/memory/{bobs}"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = call(&app, "ada-key", "POST", "/memory", Some(memory("cy", "long answers"))).await;
    assert_eq!(status, StatusCode::CREATED, "an administrator writes for anyone");
    // A service key is nobody in particular: tenant-wide, as before.
    let (status, body) = call(&app, "svc-key", "POST", "/memory/query", Some(json!({"scope": {"scope": "user", "id": "bob"}}))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["records"].as_array().unwrap().len(), 1, "{body}");
    let _ = std::fs::remove_dir_all(store);
}

fn connection(subject: Option<&str>) -> Value {
    let mut v = json!({
        "provider": "oauth2_authorization_code",
        "scopes": ["repo"],
        "token": {"access_token": "tok-1", "refresh_token": "rt-1"},
    });
    if let Some(subject) = subject {
        v["subject"] = json!(subject);
    }
    v
}

#[tokio::test]
async fn a_connection_with_a_subject_is_that_persons() {
    let (app, store) = app();
    let (status, body) = call(&app, "bob-key", "POST", "/connections", Some(connection(Some("cy")))).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(body["message"].as_str().unwrap().contains("subject `cy` is not you"), "{body}");
    let (status, body) = call(&app, "bob-key", "POST", "/connections", Some(connection(Some("bob")))).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let bobs = body["connection"]["connection_id"].as_str().unwrap().to_owned();
    let (status, body) = call(&app, "bob-key", "POST", "/connections", Some(connection(None))).await;
    assert_eq!(status, StatusCode::CREATED, "a service-level connection has no subject: {body}");
    let shared = body["connection"]["connection_id"].as_str().unwrap().to_owned();

    // Cy sees the shared one, not Bob's; Bob's reads as absent on every path.
    let (status, body) = call(&app, "cy-key", "GET", "/connections", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let ids: Vec<&str> = body["connections"].as_array().unwrap().iter().map(|c| c["connection_id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec![shared.as_str()], "{body}");
    for (method, path, payload) in [
        ("GET", format!("/connections/{bobs}"), None),
        ("GET", format!("/connections/{bobs}/health"), None),
        ("POST", format!("/connections/{bobs}/consent"), Some(json!({"scopes": ["repo", "gist"]}))),
    ] {
        let (status, body) = call(&app, "cy-key", method, &path, payload).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}: {body}");
    }
    // Revoke and delete are an administrator's by the role table; a builder
    // is refused at the door, before any record is looked at.
    for (method, path, payload) in [
        ("POST", format!("/connections/{bobs}/revoke"), Some(json!({"reason": "user_revoked"}))),
        ("DELETE", format!("/connections/{bobs}"), None),
    ] {
        let (status, body) = call(&app, "cy-key", method, &path, payload).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}: {body}");
    }
    // Bob's is still there, for Bob and for the administrator.
    let (status, _) = call(&app, "bob-key", "GET", &format!("/connections/{bobs}"), None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = call(&app, "ada-key", "GET", "/connections", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["connections"].as_array().unwrap().len(), 2, "{body}");
    let (status, _) = call(&app, "ada-key", "POST", "/connections", Some(connection(Some("cy")))).await;
    assert_eq!(status, StatusCode::CREATED, "an administrator registers for anyone");
    let _ = std::fs::remove_dir_all(store);
}
