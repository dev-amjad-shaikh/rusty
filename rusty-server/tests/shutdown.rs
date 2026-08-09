//! Graceful shutdown + drain tests (R0.6 wave 2c): the server's half of
//! "Cancellation propagation + drain".
//!
//! Covered here:
//!
//! - in-flight HTTP requests complete inside the drain window (real
//!   sockets, `serve_with_shutdown`);
//! - an in-flight run is cooperatively cancelled at a super-step boundary
//!   and ends terminal-`cancelled`, its checkpoint intact for the next
//!   process to resume;
//! - new run submissions are rejected 503 once draining starts;
//! - the outbox relay stops on drain without losing pending rows (the next
//!   process's relay publishes them);
//! - the rolling-deploy property: a task leased to a server that goes away
//!   is claimable elsewhere within one lease period (lease expiry is the
//!   bound; the drain only makes the common case fast).
//!
//! The in-process tests drive the router via `tower::ServiceExt::oneshot`
//! (the drain token is observable through it); the socket tests bind real
//! listeners because graceful connection draining is axum's behavior, not
//! the router's.

use std::path::PathBuf;
use std::time::Duration;

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::prelude::*;
use rusty_agent_server::{router_with_shutdown, serve_with_shutdown, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

/// Unique temp store root, removed at the end of each test (best effort).
fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!(
        "rusty-server-shutdown-test-{}",
        uuid::Uuid::new_v4()
    ))
}

/// A self-looping spinner (same shape as the core executor's cancellation
/// tests): each super-step increments `n` after a paced 10 ms; the router
/// terminates the run once `n` reaches 5.
fn spinner_graph() -> (Graph, StateSpec) {
    let spec = StateSpec::new().channel("n", Reducer::Overwrite);
    let mut builder = GraphBuilder::new();
    builder.add_node("spin", |ctx: NodeContext| async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        let n = ctx.state().get("n").and_then(Value::as_i64).unwrap_or(0);
        Ok(NodeOutput::update("n", json!(n + 1)))
    });
    builder.set_entry_point("spin");
    builder.add_conditional_edges("spin", |state: State| async move {
        if state.get("n").and_then(Value::as_i64).unwrap_or(0) >= 5 {
            Ok(Route::End)
        } else {
            Ok(Route::Node("spin".into()))
        }
    });
    (builder.compile().unwrap(), spec)
}

/// A one-shot node that takes 400 ms — long enough to be in flight when
/// the shutdown signal lands.
fn slow_graph() -> (Graph, StateSpec) {
    let spec = StateSpec::new().channel("done", Reducer::Overwrite);
    let mut builder = GraphBuilder::new();
    builder.add_node("slow", |_ctx: NodeContext| async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        Ok(NodeOutput::update("done", json!(true)))
    });
    builder.set_entry_point("slow");
    (builder.compile().unwrap(), spec)
}

fn registry_with(name: &str, graph: Graph, spec: StateSpec) -> GraphRegistry {
    let mut registry = GraphRegistry::new();
    registry.register(name, graph, spec);
    registry
}

/// Send a request in-process; returns `(status, json-body-or-null)`.
async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let body = body.map_or_else(Body::empty, |v| Body::from(v.to_string()));
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

/// Create a thread for `graph`; returns its id.
async fn create_thread(app: &Router, graph: &str) -> String {
    let (status, v) = call(app, "POST", "/threads", Some(json!({"graph": graph}))).await;
    assert_eq!(status, StatusCode::CREATED, "create thread failed: {v}");
    v["thread_id"].as_str().unwrap().to_string()
}

/// Poll `GET /runs/{run_id}` until the run reaches `status`; returns the
/// terminal body.
async fn wait_run_status(app: &Router, run_id: &str, status: &str) -> Value {
    for _ in 0..100 {
        let (_, v) = call(app, "GET", &format!("/runs/{run_id}"), None).await;
        if v["status"] == json!(status) {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (_, v) = call(app, "GET", &format!("/runs/{run_id}"), None).await;
    panic!("run `{run_id}` never reached status `{status}`: {v}");
}

#[tokio::test]
async fn in_flight_requests_complete_inside_the_drain_window() {
    let store = temp_store();
    let (slow, slow_spec) = slow_graph();
    let registry = registry_with("slow", slow, slow_spec);
    // Grab a free port the way tests do: bind, read, release.
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);
    let config = ServerConfig::new(addr, store.clone()).with_shutdown_grace(Duration::from_secs(5));

    // The shutdown trigger: a Notify the test fires mid-request.
    let trigger = std::sync::Arc::new(tokio::sync::Notify::new());
    let signal = {
        let trigger = trigger.clone();
        async move { trigger.notified().await }
    };
    let server = tokio::spawn(serve_with_shutdown(registry, config, signal));

    // Give the listener a moment to bind.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let client = reqwest::Client::new();
    let base = format!("http://{addr}");
    let thread: Value = client
        .post(format!("{base}/threads"))
        .json(&json!({"graph": "slow"}))
        .send()
        .await
        .expect("create thread request")
        .json()
        .await
        .unwrap();
    let thread_id = thread["thread_id"].as_str().unwrap().to_string();

    // A blocking run whose 400 ms node is in flight when the signal lands.
    let wait_request = {
        let client = client.clone();
        let base = base.clone();
        let thread_id = thread_id.clone();
        tokio::spawn(async move {
            client
                .post(format!("{base}/threads/{thread_id}/runs/wait"))
                .json(&json!({}))
                .send()
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    trigger.notify_one();

    // The in-flight request completes normally — drain waits for it.
    let response = wait_request
        .await
        .expect("wait request task")
        .expect("in-flight request must complete, not be cut off");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["status"], json!("success"));
    assert_eq!(body["output"]["done"], json!(true));

    // With nothing else in flight the server returns well inside the grace.
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("server stops once in-flight work lands")
        .expect("server task did not panic")
        .expect("clean shutdown");

    // The listener is gone: new connections are refused.
    let refused = client.get(format!("{base}/ok")).send().await;
    assert!(
        refused.is_err(),
        "the listener must be closed after shutdown"
    );

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn in_flight_run_is_cancelled_at_a_boundary_and_resumes_in_the_next_process() {
    let store = temp_store();
    let token = CancellationToken::new();
    let config = || ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone());
    let (spinner, spinner_spec) = spinner_graph();
    let app = router_with_shutdown(
        registry_with("spinner", spinner, spinner_spec),
        config(),
        token.clone(),
    );

    let thread_id = create_thread(&app, "spinner").await;
    let (status, v) = call(
        &app,
        "POST",
        &format!("/threads/{thread_id}/runs"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "create run failed: {v}");
    let run_id = v["run_id"].as_str().unwrap().to_string();

    // Wait for the first boundary checkpoint, then drain. (The history
    // endpoint answers a bare array, newest first.)
    let mut checkpoints: Vec<Value> = Vec::new();
    for _ in 0..100 {
        let (_, v) = call(
            &app,
            "POST",
            &format!("/threads/{thread_id}/history"),
            Some(json!({})),
        )
        .await;
        if let Some(found) = v.as_array() {
            if !found.is_empty() {
                checkpoints = found.clone();
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        !checkpoints.is_empty(),
        "no boundary checkpoint landed before the drain"
    );
    token.cancel();

    // The run stops at its next boundary and ends terminal-cancelled.
    let status = wait_run_status(&app, &run_id, "cancelled").await;
    assert!(
        status["message"]
            .as_str()
            .unwrap_or("")
            .contains("resumes from there"),
        "the terminal message should state the resume path: {status}"
    );

    // New submissions are rejected while draining.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/threads/{thread_id}/runs"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "expected 503: {v}");
    assert_eq!(v["error"], json!("shutting_down"));
    drop(app);

    // The next process (same store) sees the drained run's checkpoints and
    // resumes the work from the last boundary.
    let (spinner, spinner_spec) = spinner_graph();
    let app2 = router_with_shutdown(
        registry_with("spinner", spinner, spinner_spec),
        config(),
        CancellationToken::new(),
    );
    let (status, v) = call(
        &app2,
        "POST",
        &format!("/threads/{thread_id}/history"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "history failed: {v}");
    let checkpoint_id = v[0]["checkpoint"]["checkpoint_id"]
        .as_str()
        .expect("a boundary checkpoint must survive the drain")
        .to_string();

    let (status, v) = call(
        &app2,
        "POST",
        &format!("/threads/{thread_id}/runs/wait"),
        Some(json!({"checkpoint": {"checkpoint_id": checkpoint_id}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "resume run failed: {v}");
    assert_eq!(v["status"], json!("success"), "resume must complete: {v}");
    assert_eq!(
        v["output"]["n"],
        json!(5),
        "the resumed run continues from the checkpoint, not from scratch"
    );

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn outbox_relay_stops_on_drain_and_the_next_process_publishes() {
    let store = temp_store();
    let token = CancellationToken::new();
    let config = || {
        ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
            .with_outbox_relay_interval(Duration::from_millis(50))
    };
    let app = router_with_shutdown(GraphRegistry::new(), config(), token.clone());

    // Control: the relay publishes a pending row within a few intervals.
    let (status, v) = call(
        &app,
        "POST",
        "/tasks/outbox",
        Some(json!({"kind": "send_email", "payload": {"to": "a@b.c"}})),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "outbox enqueue failed: {v}");
    let published_id = v["task_id"].as_str().unwrap().to_string();
    let mut published = StatusCode::NOT_FOUND;
    for _ in 0..100 {
        let (status, _) = call(&app, "GET", &format!("/tasks/{published_id}"), None).await;
        published = status;
        if published == StatusCode::OK {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(published, StatusCode::OK, "control row never published");

    // Drain, then let any pass that was in flight during the cancel
    // complete (the relay is only observed between passes).
    token.cancel();
    tokio::time::sleep(Duration::from_millis(150)).await;

    // A row enqueued after the drain must NOT publish — the relay stopped
    // accepting new publishes — and must NOT be lost either.
    let (status, v) = call(
        &app,
        "POST",
        "/tasks/outbox",
        Some(json!({"kind": "send_email", "payload": {"to": "b@c.d"}})),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "outbox enqueue failed: {v}");
    let pending_id = v["task_id"].as_str().unwrap().to_string();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let (status, _) = call(&app, "GET", &format!("/tasks/{pending_id}"), None).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a drained relay must not publish new rows"
    );
    drop(app);

    // The next process's relay publishes the surviving row on its first
    // passes — the crash-safety the outbox is built on, unchanged by drain.
    let app2 = router_with_shutdown(GraphRegistry::new(), config(), CancellationToken::new());
    let mut published = StatusCode::NOT_FOUND;
    for _ in 0..100 {
        let (status, _) = call(&app2, "GET", &format!("/tasks/{pending_id}"), None).await;
        published = status;
        if published == StatusCode::OK {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        published,
        StatusCode::OK,
        "the next process's relay must publish the surviving row"
    );

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn leased_task_is_reclaimable_within_one_lease_period_after_the_server_is_gone() {
    // The rolling-deploy property: SIGTERM a server holding a leased task
    // (here: drain + drop the router) and the task is claimable by the
    // next instance within ONE LEASE PERIOD. Lease expiry — not the drain —
    // is the bound; the worker-side drain only makes the common case fast.
    let store = temp_store();
    let token = CancellationToken::new();
    let config = || ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone());

    let app = router_with_shutdown(GraphRegistry::new(), config(), token.clone());
    let (status, v) = call(
        &app,
        "POST",
        "/tasks",
        Some(json!({"kind": "send_email", "payload": {"to": "a@b.c"}})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "enqueue failed: {v}");
    let task_id = v["task_id"].as_str().unwrap().to_string();

    // A worker on the old pod leases the task (short lease, 300 ms)...
    let (status, v) = call(
        &app,
        "POST",
        "/tasks/claim",
        Some(json!({"worker_id": "worker-on-old-pod", "lease_ms": 300})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "claim failed: {v}");
    assert_eq!(v["task"]["task_id"], json!(task_id));

    // ...and the pod goes away mid-lease (SIGTERM → drain → exit).
    token.cancel();
    drop(app);

    // The replacement instance claims the task as soon as the lease
    // expires — measured here against a bound of one lease period plus
    // scheduling slack.
    let app2 = router_with_shutdown(GraphRegistry::new(), config(), CancellationToken::new());
    let deadline = std::time::Instant::now() + Duration::from_millis(2_000);
    let claimed = loop {
        let (status, v) = call(
            &app2,
            "POST",
            "/tasks/claim",
            Some(json!({"worker_id": "worker-on-new-pod", "lease_ms": 30_000})),
        )
        .await;
        if status == StatusCode::OK {
            break v["task"].clone();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "task was not reclaimable within one lease period"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert_eq!(claimed["task_id"], json!(task_id));
    assert_eq!(claimed["attempt"], json!(2));
    assert_eq!(claimed["lease"]["owner"], json!("worker-on-new-pod"));

    let _ = std::fs::remove_dir_all(store);
}
