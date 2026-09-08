//! Server-side turn-stamp minting (EP-07-S12 AC1): every scheduled run
//! carries the provenance stamp the server honestly holds — session =
//! thread, turn = run, main-line traffic — into every node invocation,
//! and a client-chosen non-uuid thread id honestly carries none.

use std::path::PathBuf;

use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::http::{Request, StatusCode};
use rusty_agent_runtime::prelude::*;
use rusty_agent_server::{GraphRegistry, ServerConfig, router};
use serde_json::{Value, json};
use tower::ServiceExt;

/// The probe graph: one node copies the invocation's turn stamp (or its
/// absence) into the `probe` channel, so the wire can read back what the
/// dispatcher actually received.
fn probe_registry() -> GraphRegistry {
    let spec = StateSpec::new().channel("probe", Reducer::Overwrite);
    let mut builder = GraphBuilder::new();
    builder.add_node("probe", |ctx: NodeContext| async move {
        let observed = match ctx
            .config()
            .extra
            .get(rusty_agent_runtime::prelude::TURN_STAMP_KEY)
        {
            Some(value) => {
                let stamp: TurnStamp =
                    serde_json::from_value(value.clone()).expect("a well-formed stamp");
                json!({
                    "present": true,
                    "session_id": stamp.session_id.to_string(),
                    "turn_id": stamp.turn_id.to_string(),
                    "traffic": serde_json::to_value(stamp.traffic).unwrap(),
                    "turn_boundary": serde_json::to_value(stamp.turn_boundary).unwrap(),
                    "component": stamp.issued_by.component,
                })
            }
            None => json!({"present": false}),
        };
        Ok(NodeOutput::update("probe", observed))
    });
    builder.set_entry_point("probe");
    let mut registry = GraphRegistry::new();
    registry.register("probe", builder.compile().unwrap(), spec);
    registry
}

fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!(
        "rusty-server-turn-stamp-test-{}",
        uuid::Uuid::new_v4()
    ))
}

fn app(store: PathBuf) -> Router {
    router(
        probe_registry(),
        ServerConfig::new("127.0.0.1:0".parse().unwrap(), store),
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
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, value)
}

/// Create the thread (server-minted or client-chosen id) and run it once;
/// returns `(thread_id, run_id)`.
async fn run_once(app: &Router, thread_id: Option<&str>) -> (String, String) {
    let mut payload = json!({"graph": "probe"});
    if let Some(id) = thread_id {
        payload["thread_id"] = json!(id);
    }
    let (status, thread) = call(app, "POST", "/threads", Some(payload)).await;
    assert_eq!(status, StatusCode::CREATED, "thread failed: {thread}");
    let thread_id = thread["thread_id"].as_str().unwrap().to_string();
    let (status, run) = call(
        app,
        "POST",
        &format!("/threads/{thread_id}/runs/wait"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "run failed: {run}");
    let run_id = run["run_id"].as_str().unwrap().to_string();
    (thread_id, run_id)
}

/// The probe channel's observed stamp for the thread.
async fn observed(app: &Router, thread_id: &str) -> Value {
    let (status, state) = call(app, "GET", &format!("/threads/{thread_id}/state"), None).await;
    assert_eq!(status, StatusCode::OK, "state failed: {state}");
    state["values"]["probe"].clone()
}

#[tokio::test]
async fn a_scheduled_run_carries_the_minted_stamp() {
    let store = temp_store();
    let server = app(store.clone());
    let (thread_id, run_id) = run_once(&server, None).await;

    let stamp = observed(&server, &thread_id).await;
    assert_eq!(stamp["present"], true, "{stamp}");
    assert_eq!(stamp["session_id"], thread_id);
    assert_eq!(stamp["turn_id"], run_id);
    assert_eq!(stamp["traffic"], json!("main"));
    // The probe node runs at the run's first step: the dispatcher's
    // boundary is the turn start.
    assert_eq!(stamp["turn_boundary"], json!("start"));

    // A second run on the thread is a new turn of the same session.
    let (_, second_run) = {
        let (status, run) = call(
            &server,
            "POST",
            &format!("/threads/{thread_id}/runs/wait"),
            Some(json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "run failed: {run}");
        (
            thread_id.clone(),
            run["run_id"].as_str().unwrap().to_string(),
        )
    };
    let second = observed(&server, &thread_id).await;
    assert_eq!(second["session_id"], thread_id);
    assert_eq!(second["turn_id"], second_run);
    assert_ne!(second["turn_id"], stamp["turn_id"]);
    std::fs::remove_dir_all(store).unwrap();
}

#[tokio::test]
async fn a_client_chosen_non_uuid_thread_id_carries_no_stamp() {
    let store = temp_store();
    let server = app(store.clone());
    let (thread_id, _) = run_once(&server, Some("legacy-1")).await;
    let stamp = observed(&server, &thread_id).await;
    assert_eq!(stamp["present"], false, "{stamp}");
    std::fs::remove_dir_all(store).unwrap();
}
