//! End-to-end test: a graph mixing **local** nodes with **remote** nodes
//! (`RemoteNode` → in-process worker), run by the real `Executor` — including
//! an interrupt → resume round trip across the wire.

use std::sync::Arc;

use rusty_agent_runtime::prelude::*;
use rusty_agent_runtime::remote::RemoteNode;
use rusty_worker::{router, WorkerRegistry};
use serde_json::json;

/// Serve a registry on an ephemeral port; returns the base URL.
async fn start_worker_with(registry: WorkerRegistry) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router(registry))
            .await
            .expect("worker server");
    });
    format!("http://{addr}")
}

/// Start a worker with the test handlers on an ephemeral port; returns the
/// base URL to point `RemoteNode`s at.
async fn start_worker() -> String {
    let registry = WorkerRegistry::new()
        // A normal remote handler: doubles `state.n` into `doubled`.
        .with("doubler", |ctx: NodeContext| async move {
            let n = ctx
                .state()
                .get("n")
                .and_then(|v| v.as_i64())
                .expect("state channel `n` must be set by an upstream node");
            Ok(NodeOutput::update("doubled", json!(n * 2)))
        })
        // A HITL remote handler: interrupts until the run is resumed.
        .with("approval_gate", |ctx: NodeContext| async move {
            match ctx.resume_value() {
                Some(value) => Ok(NodeOutput::update("approved", value.clone())),
                None => Err(ctx.interrupt(json!({
                    "question": "approve deployment?",
                    "node": "approval_gate",
                }))),
            }
        });

    start_worker_with(registry).await
}

/// The mixed local/remote graph used by both tests:
///
/// ```text
/// seed (local) -> double (REMOTE) -> gate (REMOTE, HITL) -> final (local)
/// ```
fn build_graph(worker_url: &str) -> Graph {
    let mut builder = GraphBuilder::new();

    builder.add_node("seed", |_ctx: NodeContext| async {
        Ok(NodeOutput::update("n", json!(21)))
    });
    builder.add_node("double", RemoteNode::new("doubler", worker_url));
    builder.add_node("gate", RemoteNode::new("approval_gate", worker_url));
    builder.add_node("final", |ctx: NodeContext| async move {
        let approved = ctx.state().get("approved").cloned().unwrap_or(json!(false));
        Ok(NodeOutput::update(
            "log",
            json!(format!("finished with approved={approved}")),
        ))
    });

    builder.add_edge("seed", "double");
    builder.add_edge("double", "gate");
    builder.add_edge("gate", "final");
    builder.set_entry_point("seed");
    builder.compile().expect("graph compiles")
}

fn state_spec() -> StateSpec {
    StateSpec::new()
        .channel("n", Reducer::Overwrite)
        .channel("doubled", Reducer::Overwrite)
        .channel("approved", Reducer::Overwrite)
        .channel("log", Reducer::Append)
}

#[tokio::test]
async fn remote_node_executes_end_to_end_alongside_local_nodes() {
    let worker_url = start_worker().await;

    // Liveness probe: the worker reports its protocol + handlers.
    let ok: serde_json::Value = http_get_json(&format!("{worker_url}/ok")).await;
    assert_eq!(ok["status"], json!("ok"));
    assert_eq!(
        ok["protocol_version"],
        json!(rusty_agent_runtime::remote::PROTOCOL_VERSION)
    );
    assert_eq!(ok["nodes"], json!(["approval_gate", "doubler"]));

    // Graph that skips the HITL gate: seed -> double -> final.
    let mut builder = GraphBuilder::new();
    builder.add_node("seed", |_ctx: NodeContext| async {
        Ok(NodeOutput::update("n", json!(21)))
    });
    builder.add_node("double", RemoteNode::new("doubler", &worker_url));
    builder.add_node("final", |ctx: NodeContext| async move {
        let doubled = ctx.state().get("doubled").cloned().unwrap_or(json!(0));
        Ok(NodeOutput::update(
            "log",
            json!(format!("doubled={doubled}")),
        ))
    });
    builder.add_edge("seed", "double");
    builder.add_edge("double", "final");
    builder.set_entry_point("seed");
    let graph = builder.compile().unwrap();

    let outcome = Executor::new()
        .run(
            &graph,
            &state_spec(),
            State::new(),
            RunConfig::new("t-plain"),
        )
        .await
        .expect("run succeeds");

    match outcome {
        ExecutionOutcome::Done(state) => {
            assert_eq!(state.get("n"), Some(&json!(21)));
            // Written by the REMOTE node, merged by the local executor.
            assert_eq!(state.get("doubled"), Some(&json!(42)));
            assert_eq!(state.get("log"), Some(&json!(["doubled=42"])));
        }
        other => panic!("expected Done, got {other:?}"),
    }
}

#[tokio::test]
async fn interrupt_resume_round_trip_across_the_wire() {
    let worker_url = start_worker().await;
    let graph = build_graph(&worker_url);
    let spec = state_spec();
    let checkpointer = Arc::new(InMemoryCheckpointer::new());

    // ---- Run 1: seed -> double -> gate INTERRUPTS (remotely) ----
    let executor = Executor::with_checkpointer(checkpointer.clone());
    let outcome = executor
        .run(&graph, &spec, State::new(), RunConfig::new("t-hitl"))
        .await
        .expect("run succeeds (suspends)");

    let checkpoint_id = match outcome {
        ExecutionOutcome::Interrupted {
            value,
            state,
            checkpoint_id,
        } => {
            // The interrupt payload came back across the wire from the worker.
            assert_eq!(
                value,
                json!({"question": "approve deployment?", "node": "approval_gate"})
            );
            // Work up to the suspension point survived: the remote `double`
            // node's update was merged before the gate ran.
            assert_eq!(state.get("n"), Some(&json!(21)));
            assert_eq!(state.get("doubled"), Some(&json!(42)));
            assert!(!checkpoint_id.is_empty());
            checkpoint_id
        }
        other => panic!("expected Interrupted, got {other:?}"),
    };

    // The suspension point was persisted and schedules the gate node.
    let stored = checkpointer
        .get_latest("t-hitl")
        .await
        .unwrap()
        .expect("checkpoint exists");
    assert_eq!(stored.id, checkpoint_id);
    assert_eq!(stored.next_nodes, vec!["gate".to_string()]);

    // ---- Run 2: resume; the value crosses the wire into the gate ----
    let executor = Executor::with_checkpointer(checkpointer);
    let outcome = executor
        .run(
            &graph,
            &spec,
            State::new(),
            RunConfig::new("t-hitl").with_resume(json!(true)),
        )
        .await
        .expect("resume succeeds");

    match outcome {
        ExecutionOutcome::Done(state) => {
            // The gate re-ran remotely with config.resume = true and wrote
            // `approved`; `final` then ran locally.
            assert_eq!(state.get("approved"), Some(&json!(true)));
            assert_eq!(state.get("doubled"), Some(&json!(42)));
            assert_eq!(
                state.get("log"),
                Some(&json!(["finished with approved=true"]))
            );
        }
        other => panic!("expected Done after resume, got {other:?}"),
    }
}

/// Minimal GET helper (the worker crate intentionally has no HTTP client
/// dependency): one raw HTTP/1.1 request over a tokio TCP stream.
async fn http_get_json(url: &str) -> serde_json::Value {
    let (_status, body) = http_request("GET", url, None).await;
    body
}

/// Minimal POST helper returning the status code alongside the JSON body —
/// the contract tests below assert on both.
async fn http_post_json(url: &str, body: &serde_json::Value) -> (u16, serde_json::Value) {
    http_request("POST", url, Some(body)).await
}

/// One raw HTTP/1.1 request over a tokio TCP stream. The worker crate
/// intentionally has no HTTP client dependency, so tests hand-roll the
/// framing instead of pulling one in.
async fn http_request(
    method: &str,
    url: &str,
    body: Option<&serde_json::Value>,
) -> (u16, serde_json::Value) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let authority = url
        .strip_prefix("http://")
        .expect("test worker URLs are plain http");
    let (host, path) = match authority.split_once('/') {
        Some((host, path)) => (host, format!("/{path}")),
        None => (authority, "/".to_string()),
    };
    let mut stream = tokio::net::TcpStream::connect(host)
        .await
        .expect("connect to worker");
    let request = match body {
        Some(body) => {
            let payload = serde_json::to_string(body).unwrap();
            format!(
                "{method} {path} HTTP/1.1\r\nhost: {host}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
                payload.len()
            )
        }
        None => format!("{method} {path} HTTP/1.1\r\nhost: {host}\r\nconnection: close\r\n\r\n"),
    };
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let raw = String::from_utf8(raw).unwrap();
    let (head, body) = raw
        .split_once("\r\n\r\n")
        .expect("HTTP response has a header/body split");
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .expect("status line carries a code")
        .parse()
        .expect("status code is numeric");
    let body = serde_json::from_str(body).expect("valid JSON response body");
    (status, body)
}

// ---------------------------------------------------------------------------
// HTTP-layer contract tests: the branches that keep `RemoteNode`'s retry
// semantics honest — 4xx is fatal client-side, application outcomes always
// arrive as 200 + a one-payload body.
// ---------------------------------------------------------------------------

/// A task body valid except where a test patches it; built from
/// `probe_body()` so the shape tracks the protocol.
fn task_body(node: &str) -> serde_json::Value {
    let mut body = rusty_worker::probe_body();
    body["node"] = json!(node);
    body
}

#[tokio::test]
async fn protocol_version_mismatch_is_400_with_error_body() {
    let worker_url = start_worker().await;

    let mut body = task_body("doubler");
    body["protocol_version"] = json!(rusty_agent_runtime::remote::PROTOCOL_VERSION + 1);
    let (status, response) = http_post_json(&format!("{worker_url}/execute"), &body).await;

    assert_eq!(status, 400);
    let error = response["error"].as_str().expect("error payload");
    assert!(
        error.contains("unsupported protocol_version"),
        "unexpected error body: {error}"
    );
    assert!(response.get("output").is_none() && response.get("interrupt").is_none());
}

#[tokio::test]
async fn unknown_handler_is_200_with_error_body() {
    let worker_url = start_worker().await;

    let (status, response) =
        http_post_json(&format!("{worker_url}/execute"), &task_body("ghost")).await;

    // Unknown handler is an application outcome, not a transport failure —
    // 200 so the client never retries it.
    assert_eq!(status, 200);
    let error = response["error"].as_str().expect("error payload");
    assert!(
        error.contains("no handler registered for node `ghost`"),
        "unexpected error body: {error}"
    );
    // The registered-name list is sorted for deterministic logs.
    assert!(
        error.contains("[\"approval_gate\", \"doubler\"]"),
        "unexpected registered list: {error}"
    );
}

#[tokio::test]
async fn handler_error_is_200_with_error_body() {
    let registry = WorkerRegistry::new().with("failer", |_ctx: NodeContext| async {
        Err(RustyError::Tool("backend exploded".into()))
    });
    let worker_url = start_worker_with(registry).await;

    let (status, response) =
        http_post_json(&format!("{worker_url}/execute"), &task_body("failer")).await;

    assert_eq!(status, 200);
    let error = response["error"].as_str().expect("error payload");
    assert!(
        error.contains("backend exploded"),
        "unexpected error body: {error}"
    );
    assert!(response.get("output").is_none() && response.get("interrupt").is_none());
}

#[tokio::test]
async fn handler_panic_is_caught_and_returned_as_error_body() {
    let registry = WorkerRegistry::new()
        .with("panicker", |_ctx: NodeContext| async {
            panic!("kaboom");
        })
        .with("alive", |_ctx: NodeContext| async {
            Ok(NodeOutput::update("ok", json!(true)))
        });
    let worker_url = start_worker_with(registry).await;

    let (status, response) =
        http_post_json(&format!("{worker_url}/execute"), &task_body("panicker")).await;

    // The panic must NOT drop the connection: that would read as a
    // transport failure client-side and be retried, replaying node logic.
    assert_eq!(status, 200);
    let error = response["error"].as_str().expect("error payload");
    assert!(
        error.contains("panicked") && error.contains("kaboom"),
        "unexpected error body: {error}"
    );

    // The worker survives and keeps serving other handlers.
    let (status, response) =
        http_post_json(&format!("{worker_url}/execute"), &task_body("alive")).await;
    assert_eq!(status, 200);
    assert_eq!(response["output"]["updates"]["ok"], json!(true));
}
