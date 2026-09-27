//! Reading an OpenAPI document into a connector: the writes that would
//! guess after a lost answer read back through the document's own reads
//! when it has one that filters by the write's key, and the draft says
//! which writes still guess.

use std::path::PathBuf;

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

fn app() -> (Router, PathBuf) {
    let store = std::env::temp_dir().join(format!("rusty-openapi-import-{}", uuid::Uuid::new_v4()));
    (
        router(
            GraphRegistry::new(),
            ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone()),
        ),
        store,
    )
}

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(value) => {
            builder = builder.header("content-type", "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes: Bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

#[tokio::test]
async fn an_imported_write_reads_back_through_the_documents_own_read() {
    let (app, store) = app();
    let spec = json!({
        "openapi": "3.0.0",
        "info": {"title": "Facilities", "version": "1"},
        "paths": {
            "/tickets": {
                "get": {"operationId": "listTickets", "summary": "List tickets", "parameters": [
                    {"name": "title", "in": "query", "schema": {"type": "string"}},
                    {"name": "limit", "in": "query", "schema": {"type": "integer"}}
                ]},
                "post": {"operationId": "createTicket", "summary": "File a ticket", "requestBody": {"content": {"application/json": {"schema": {"type": "object", "required": ["title"], "properties": {"title": {"type": "string"}, "room": {"type": "string"}}}}}}}
            },
            "/notes": {
                "post": {"operationId": "addNote", "summary": "Add a note", "requestBody": {"content": {"application/json": {"schema": {"type": "object", "properties": {"body": {"type": "string"}}}}}}}
            }
        }
    });
    let (status, draft) = call(&app, "POST", "/connectors/openapi", Some(json!({
        "id": "facilities", "version": "1", "display_name": "Facilities", "description": "The facilities desk.",
        "documentation_url": "https://facilities.example.internal/docs", "base_url": "https://facilities.example.internal", "auth": "bearer", "spec": spec,
    }))).await;
    assert_eq!(status, StatusCode::OK, "{draft}");
    let ops = draft["manifest"]["operations"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let create = ops
        .iter()
        .find(|op| op["method"] == json!("POST") && op["path"] == json!("/tickets"))
        .cloned()
        .expect("the write was imported");
    // The check is derived from the same path; the read-back is the document's own listing.
    let read = ops
        .iter()
        .find(|op| {
            op["method"] == json!("GET")
                && op["path"] == json!("/tickets")
                && op["name"] != json!("check-connection")
        })
        .cloned()
        .expect("the read was imported");
    assert_eq!(
        create["reconcile"]["operation"], read["name"],
        "the write reads back through the document's own read: {create}"
    );
    assert_eq!(
        create["reconcile"]["arguments"]["title"],
        json!("$title"),
        "bound to the write's natural key: {create}"
    );
    assert_eq!(
        create["reconcile"]["arguments"]["limit"],
        json!(5),
        "the read's limit held small: {create}"
    );
    let adopted = draft["read_backs"]["adopted"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(adopted.len(), 1, "{draft}");
    assert_eq!(adopted[0]["write"], create["name"]);
    let note = ops
        .iter()
        .find(|op| op["path"] == json!("/notes"))
        .cloned()
        .expect("the note write");
    assert!(
        note["reconcile"].is_null(),
        "a write with no read on its path keeps guessing: {note}"
    );
    assert_eq!(
        draft["read_backs"]["still_guessing"],
        json!([note["name"]]),
        "and the draft says so: {draft}"
    );
    // The draft registers as it is: the read-back rides with the write.
    let (status, receipt) =
        call(&app, "POST", "/connectors", Some(draft["manifest"].clone())).await;
    assert_eq!(status, StatusCode::CREATED, "{receipt}");
    let _ = std::fs::remove_dir_all(store);
}

/// A large public API: more operations than one connector holds. The draft
/// answers with the list to choose from, a builder picks, and the chosen
/// ones become the connector — needing no sign-in when the API needs none.
#[tokio::test]
async fn a_large_api_is_chosen_from_and_a_public_one_asks_for_nothing() {
    let (app, store) = app();
    let mut paths = serde_json::Map::new();
    paths.insert(
        "/alerts".into(),
        json!({"get": {"operationId": "alerts", "summary": "Active weather alerts."}}),
    );
    for n in 0..70 {
        paths.insert(format!("/stations/{n}/{{id}}"), json!({"get": {"operationId": format!("station{n}"), "summary": "One station.", "parameters": [{"name": "id", "in": "path", "required": true, "schema": {"type": "string"}}]}}));
    }
    let spec =
        json!({"openapi": "3.0.0", "info": {"title": "Weather", "version": "1"}, "paths": paths});
    let draft_of = |ops: Option<Vec<&str>>| {
        let mut body = json!({"id": "weather", "display_name": "Weather", "description": "Weather.", "documentation_url": "https://weather.example/openapi.json", "base_url": "https://weather.example", "auth": "none", "spec": spec.clone()});
        if let Some(ops) = ops {
            body["operations"] = json!(ops);
        }
        body
    };
    let (status, draft) = call(&app, "POST", "/connectors/openapi", Some(draft_of(None))).await;
    assert_eq!(status, StatusCode::OK, "{draft}");
    assert!(
        draft["manifest"].is_null(),
        "71 operations are more than one connector holds: {draft}"
    );
    assert_eq!(draft["choose"]["cap"], json!(63), "{draft}");
    assert_eq!(
        draft["available"].as_array().map(Vec::len),
        Some(71),
        "{draft}"
    );
    // The builder chooses two; the check is derived from the parameterless read.
    let (status, draft) = call(
        &app,
        "POST",
        "/connectors/openapi",
        Some(draft_of(Some(vec!["alerts", "station3"]))),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{draft}");
    let names: Vec<&str> = draft["manifest"]["operations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|op| op["name"].as_str().unwrap())
        .collect();
    assert_eq!(names.len(), 3, "{names:?}");
    assert!(
        names.contains(&"check-connection")
            && names.contains(&"alerts")
            && names.contains(&"station3"),
        "{names:?}"
    );
    assert!(
        draft["manifest"]["connection_specification"]["required"].is_null(),
        "a public API asks for nothing: {draft}"
    );
    let (status, receipt) =
        call(&app, "POST", "/connectors", Some(draft["manifest"].clone())).await;
    assert_eq!(status, StatusCode::CREATED, "{receipt}");
    let _ = std::fs::remove_dir_all(store);
}
