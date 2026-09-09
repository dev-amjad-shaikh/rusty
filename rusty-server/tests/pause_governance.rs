//! End-to-end governed pause governance (EP-03-S11): a run whose node
//! raises a governed interrupt parks as `paused`, its obligations committed
//! through the executor's pause sink into the server store — where the
//! approvals queue, the expiry sweep, and the cancel route observe them.
//! Driven in-process over the real HTTP surface via `tower::ServiceExt`;
//! the only stub is the graph itself.

use std::path::PathBuf;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use rusty_agent_runtime::prelude::*;
use rusty_agent_server::{GraphRegistry, ServerConfig, router};
use serde_json::{Value, json};
use tower::ServiceExt;

/// `gate` suspends governed until a resume value arrives.
fn gated_graph() -> (Graph, StateSpec) {
    let spec = StateSpec::new().channel("answer", Reducer::Overwrite);
    let mut builder = GraphBuilder::new();
    builder.add_node("gate", |ctx: NodeContext| async move {
        match ctx.resume_value() {
            Some(v) => Ok(NodeOutput::update("answer", v.clone())),
            None => Err(ctx.interrupt_governed(
                json!({"question": "approve the refund?"}),
                vec![RunObligation::open(
                    rusty_agent_runtime::record::ObligationKind::Approval {
                        scope: "ops".into(),
                        sticky_allowed: true,
                    },
                )
                .with_tool_call("tc-refund")],
            )),
        }
    });
    builder.set_entry_point("gate");
    (builder.compile().expect("graph compiles"), spec)
}

fn app_with_ttl(ttl: std::time::Duration) -> Router {
    let store_dir = std::env::temp_dir().join(format!(
        "rusty-pause-governance-test-{}",
        uuid::Uuid::new_v4()
    ));
    let (graph, spec) = gated_graph();
    let mut registry = GraphRegistry::new();
    registry.register("gated", graph, spec);
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store_dir)
        .with_default_obligation_ttl(ttl);
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
    let response = app
        .clone()
        .oneshot(builder.body(body).unwrap())
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

/// Create a thread and drive one run to its governed pause; returns the
/// thread id and the paused run id.
async fn pause_a_run(app: &Router) -> (String, String) {
    let (status, v) = call(app, "POST", "/threads", Some(json!({"graph": "gated"}))).await;
    assert_eq!(status, StatusCode::CREATED, "thread creation failed: {v}");
    let thread = v["thread_id"].as_str().unwrap().to_string();
    let (status, v) = call(
        app,
        "POST",
        &format!("/threads/{thread}/runs/wait"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "run/wait failed: {v}");
    assert_eq!(v["status"], json!("paused"), "the governed run parks: {v}");
    assert_eq!(v["open_obligations"], json!(1));
    let run_id = v["run_id"].as_str().unwrap().to_string();
    (thread, run_id)
}

#[tokio::test]
async fn governed_pause_registers_obligations_end_to_end() {
    let app = app_with_ttl(std::time::Duration::from_secs(3600));
    let (thread, run_id) = pause_a_run(&app).await;

    // The run row parked as paused, visible over the wire.
    let (status, v) = call(&app, "GET", &format!("/runs/{run_id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["status"], json!("paused"));

    // The approvals queue answers from the committed obligation rows: one
    // open approval with the deployment's default TTL stamped at creation.
    let (status, v) = call(&app, "GET", &format!("/approvals?run_id={run_id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    let items = v["items"].as_array().expect("approvals list");
    assert_eq!(items.len(), 1, "the pause committed one obligation: {v}");
    assert_eq!(items[0]["status"], json!("open"));
    assert_eq!(items[0]["kind"]["kind"], json!("approval"));
    let stamped = items[0]["expires_at"]
        .as_str()
        .expect("the deployment default TTL stamps expires_at at creation");
    let stamped = chrono::DateTime::parse_from_rfc3339(stamped).expect("RFC 3339 expiry");
    let remaining = stamped.with_timezone(&chrono::Utc) - chrono::Utc::now();
    assert!(
        remaining > chrono::Duration::minutes(55) && remaining <= chrono::Duration::minutes(60),
        "the stamped expiry is one hour out, give or take the test's own runtime: {remaining}"
    );

    // Resume is an ordinary invocation: the answer lands, the run completes.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/runs/wait"),
        Some(json!({"command": {"resume": {"approved": true}}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "resume failed: {v}");
    assert_eq!(v["status"], json!("success"), "resume completes: {v}");
    assert_eq!(v["output"]["answer"], json!({"approved": true}));
}

#[tokio::test]
async fn expiry_sweep_observes_committed_obligations() {
    // A zero TTL: the obligation is due the moment it commits.
    let app = app_with_ttl(std::time::Duration::ZERO);
    let (_thread, run_id) = pause_a_run(&app).await;

    let (status, v) = call(&app, "POST", "/approvals/sweep", None).await;
    assert_eq!(status, StatusCode::OK, "sweep failed: {v}");
    assert_eq!(v["expired"], json!(1));
    assert_eq!(v["affected_runs"], json!([run_id]));

    let (status, v) = call(&app, "GET", &format!("/approvals?run_id={run_id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["items"][0]["status"], json!("expired"));
}

#[tokio::test]
async fn cancelling_a_paused_run_expires_its_obligations() {
    let app = app_with_ttl(std::time::Duration::from_secs(3600));
    let (_thread, run_id) = pause_a_run(&app).await;

    let (status, v) = call(&app, "POST", &format!("/runs/{run_id}/cancel"), None).await;
    assert_eq!(status, StatusCode::OK, "cancel failed: {v}");
    assert_eq!(v["obligations_expired"], json!(true));

    let (status, v) = call(&app, "GET", &format!("/approvals?run_id={run_id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["items"][0]["status"], json!("expired"));
    let (status, v) = call(&app, "GET", &format!("/runs/{run_id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["status"], json!("cancelled"));
}
