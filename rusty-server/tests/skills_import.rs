//! `GET /skills/library` serves what the deployment configured, and
//! `POST /skills/import` refuses what it cannot read before it touches the
//! network. The successful path — a real archive over egress — is driven
//! through the studio against GitHub, not simulated here.

use std::path::PathBuf;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_server::{router, GraphRegistry, ServerConfig, SkillLibrarySource};
use serde_json::{json, Value};
use tower::ServiceExt;

fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!("rusty-server-skills-import-{}", uuid::Uuid::new_v4()))
}

fn app() -> (Router, PathBuf) {
    let store = temp_store();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_skill_library(vec![SkillLibrarySource {
            id: "acme".to_owned(),
            name: "Acme skills".to_owned(),
            url: "https://github.com/acme/skills".to_owned(),
            description: "What Acme publishes.".to_owned(),
            publisher: "Acme".to_owned(),
            license: Some("MIT".to_owned()),
            subpath: Some("skills".to_owned()),
        }]);
    (router(GraphRegistry::new(), config), store)
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
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

#[tokio::test]
async fn the_library_is_what_the_deployment_configured() {
    let (app, store) = app();
    let (status, body) = call(&app, "GET", "/skills/library", None).await;
    assert_eq!(status, StatusCode::OK);
    let sources = body["sources"].as_array().unwrap();
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0]["id"], "acme");
    assert_eq!(sources[0]["url"], "https://github.com/acme/skills");
    assert_eq!(sources[0]["subpath"], "skills");
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn an_unreadable_url_is_refused_before_any_fetch() {
    let (app, store) = app();
    let (status, body) = call(
        &app,
        "POST",
        "/skills/import",
        Some(json!({ "url": "http://github.com/acme/skills" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body["message"].as_str().unwrap().contains("https://"));

    let (status, body) = call(
        &app,
        "POST",
        "/skills/import",
        Some(json!({ "url": "https://example.com/some/page" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body["message"].as_str().unwrap().contains("GitHub repository URL"));

    let (status, body) = call(
        &app,
        "POST",
        "/skills/import",
        Some(json!({ "url": "https://github.com/acme/skills", "ref": "../main" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body["message"].as_str().unwrap().contains("not a git ref"));
    let _ = std::fs::remove_dir_all(store);
}
