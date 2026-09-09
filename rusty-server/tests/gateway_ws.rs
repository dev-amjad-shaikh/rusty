//! The gateway WebSocket transport (EP-04-S01): schema validation before
//! dispatch, snapshot-on-connect with per-connection monotonic sequencing,
//! the client-driven re-snapshot, named run-boundary events, and tenant
//! isolation — driven as a real WS client over a real socket.
//!
//! Maps to the story's test verification:
//!
//! - `frame_validation_rejects_unknown`: frames with a missing `method`,
//!   an unknown `frame` tag, and an oversized payload each earn a typed
//!   `invalid_frame` refusal, and nothing reaches a method handler.
//! - `snapshot_then_monotonic_seq`: connect → snapshot (seq 1) → ≥100
//!   events with strictly increasing `seq`; the run's boundary events
//!   arrive named and in order (`run_admitted` → `run_started` →
//!   `run_ended`).
//! - the AC3 half: a `resnapshot` request is answered and followed by a
//!   fresh snapshot whose `seq` restarts at 1.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use rusty_agent_runtime::prelude::*;
use rusty_agent_server::{serve_with_shutdown, GraphRegistry, ServerConfig};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

/// Unique temp store root, removed at the end of each test (best effort).
fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!(
        "rusty-server-gateway-ws-test-{}",
        uuid::Uuid::new_v4()
    ))
}

/// A paced spinner: each super-step increments `n` after 10 ms, ending at
/// `steps` — the frame source for sequencing tests (each step emits an
/// `updates` and a `values` frame, so 60 steps produce ~120 run frames).
fn spinner_graph(steps: i64) -> (Graph, StateSpec) {
    let spec = StateSpec::new().channel("n", Reducer::Overwrite);
    let mut builder = GraphBuilder::new();
    builder.add_node("spin", |ctx: NodeContext| async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        let n = ctx.state().get("n").and_then(Value::as_i64).unwrap_or(0);
        Ok(NodeOutput::update("n", json!(n + 1)))
    });
    builder.set_entry_point("spin");
    builder.add_conditional_edges("spin", move |state: State| async move {
        if state.get("n").and_then(Value::as_i64).unwrap_or(0) >= steps {
            Ok(Route::End)
        } else {
            Ok(Route::Node("spin".into()))
        }
    });
    (builder.compile().unwrap(), spec)
}

/// Bind the server on a free loopback port and spawn it. The returned
/// handle aborts the server at test end.
async fn spawn_server(
    registry: GraphRegistry,
    mut config: ServerConfig,
) -> (SocketAddr, tokio::task::JoinHandle<std::io::Result<()>>) {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);
    config.bind_addr = addr;
    let server = tokio::spawn(serve_with_shutdown(
        registry,
        config,
        std::future::pending::<()>(),
    ));
    // Give the listener a moment to bind.
    tokio::time::sleep(Duration::from_millis(100)).await;
    (addr, server)
}

fn open_config(store: PathBuf) -> ServerConfig {
    // The address is replaced by `spawn_server`'s probe; open (dev) mode
    // on loopback is the documented posture for these tests.
    ServerConfig::new("127.0.0.1:0".parse().unwrap(), store)
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Connect to `/gateway/ws`, optionally with an API key, and consume the
/// snapshot-on-connect frame, which every protocol conversation opens
/// with (AC2). Returns the socket and the snapshot payload.
async fn connect(addr: SocketAddr, key: Option<&str>) -> (Ws, Value) {
    let mut request = format!("ws://{addr}/gateway/ws")
        .into_client_request()
        .unwrap();
    if let Some(key) = key {
        request
            .headers_mut()
            .insert("x-api-key", key.parse().unwrap());
    }
    let (mut ws, _response) = tokio_tungstenite::connect_async(request)
        .await
        .expect("WS upgrade succeeds");
    let snapshot = recv_frame(&mut ws, Duration::from_secs(5)).await;
    assert_eq!(snapshot["frame"], json!("event"), "{snapshot}");
    assert_eq!(snapshot["name"], json!("snapshot"), "{snapshot}");
    assert_eq!(snapshot["seq"], json!(1), "{snapshot}");
    assert!(snapshot["payload"]["sessions"].is_array(), "{snapshot}");
    assert!(snapshot["payload"]["runs"].is_array(), "{snapshot}");
    assert!(snapshot["payload"]["obligations"].is_array(), "{snapshot}");
    (ws, snapshot)
}

/// Receive the next text frame as JSON, skipping anything else.
async fn recv_frame(ws: &mut Ws, timeout: Duration) -> Value {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!remaining.is_zero(), "timed out waiting for a frame");
        let message = tokio::time::timeout(remaining, ws.next())
            .await
            .expect("frame arrives within the timeout")
            .expect("the socket stays open")
            .expect("a well-formed WS message");
        if let Message::Text(text) = message {
            return serde_json::from_str(&text).expect("gateway frames are JSON");
        }
    }
}

/// Receive frames until `stop` matches one, returning everything seen.
async fn recv_until(ws: &mut Ws, timeout: Duration, stop: impl Fn(&Value) -> bool) -> Vec<Value> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut frames = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "stop condition never arrived; saw {frames:?}"
        );
        let frame = recv_frame(ws, remaining).await;
        let done = stop(&frame);
        frames.push(frame);
        if done {
            return frames;
        }
    }
}

async fn send_frame(ws: &mut Ws, frame: Value) {
    ws.send(Message::Text(serde_json::to_string(&frame).unwrap().into()))
        .await
        .expect("the socket accepts the frame");
}

#[tokio::test]
async fn frame_validation_rejects_unknown() {
    let store = temp_store();
    let (graph, spec) = spinner_graph(1);
    let mut registry = GraphRegistry::new();
    registry.register("spin", graph, spec);
    let (addr, server) = spawn_server(registry, open_config(store.clone())).await;
    let client = reqwest::Client::new();
    let base = format!("http://{addr}");
    let thread: Value = client
        .post(format!("{base}/threads"))
        .json(&json!({"graph": "spin"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let thread_id = thread["thread_id"].as_str().unwrap().to_string();

    let (mut ws, _snapshot) = connect(addr, None).await;

    // A `request` missing `method`.
    send_frame(&mut ws, json!({"frame": "request", "id": 1, "params": {}})).await;
    let refusal = recv_frame(&mut ws, Duration::from_secs(5)).await;
    assert_eq!(refusal["frame"], json!("response"), "{refusal}");
    assert_eq!(refusal["id"], json!(1), "{refusal}");
    assert_eq!(
        refusal["err"]["error"]["error"],
        json!("invalid_frame"),
        "{refusal}"
    );
    assert!(
        refusal["err"]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("method"),
        "{refusal}"
    );

    // An unknown `frame` tag.
    send_frame(&mut ws, json!({"frame": "mystery", "id": 2})).await;
    let refusal = recv_frame(&mut ws, Duration::from_secs(5)).await;
    assert_eq!(
        refusal["err"]["error"]["error"],
        json!("invalid_frame"),
        "{refusal}"
    );

    // An oversized payload (beyond the 256 KiB protocol ceiling).
    let pad = "x".repeat(300_000);
    send_frame(
        &mut ws,
        json!({
            "frame": "request",
            "id": 3,
            "method": "submit_turn",
            "params": {"thread_id": thread_id, "input": {"pad": pad}},
        }),
    )
    .await;
    let refusal = recv_frame(&mut ws, Duration::from_secs(5)).await;
    assert_eq!(
        refusal["err"]["error"]["error"],
        json!("invalid_frame"),
        "{refusal}"
    );
    assert!(
        refusal["err"]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("byte"),
        "{refusal}"
    );

    // A client-sent event frame: events flow server-to-client only.
    send_frame(
        &mut ws,
        json!({"frame": "event", "seq": 1, "name": "x", "payload": {}}),
    )
    .await;
    let refusal = recv_frame(&mut ws, Duration::from_secs(5)).await;
    assert_eq!(
        refusal["err"]["error"]["error"],
        json!("unexpected_frame"),
        "{refusal}"
    );

    // A valid frame naming no method the gateway serves.
    send_frame(
        &mut ws,
        json!({"frame": "request", "id": 5, "method": "explode", "params": {}}),
    )
    .await;
    let refusal = recv_frame(&mut ws, Duration::from_secs(5)).await;
    assert_eq!(refusal["id"], json!(5), "{refusal}");
    assert_eq!(
        refusal["err"]["error"]["error"],
        json!("unknown_method"),
        "{refusal}"
    );

    // Nothing invalid reached a handler: the oversized submit_turn never
    // scheduled, so the tenant holds no runs at all.
    let runs: Value = client
        .get(format!("{base}/runs"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(runs.as_array().map(Vec::len), Some(0), "{runs}");

    server.abort();
    let _ = std::fs::remove_dir_all(&store);
}

#[tokio::test]
async fn snapshot_then_monotonic_seq() {
    let store = temp_store();
    let (graph, spec) = spinner_graph(60);
    let mut registry = GraphRegistry::new();
    registry.register("spin", graph, spec);
    let (addr, server) = spawn_server(registry, open_config(store.clone())).await;
    let client = reqwest::Client::new();
    let base = format!("http://{addr}");
    let thread: Value = client
        .post(format!("{base}/threads"))
        .json(&json!({"graph": "spin"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let thread_id = thread["thread_id"].as_str().unwrap().to_string();

    let (mut ws, _snapshot) = connect(addr, None).await;

    // Submit the turn over the gateway protocol itself.
    send_frame(
        &mut ws,
        json!({
            "frame": "request",
            "id": 1,
            "method": "submit_turn",
            "params": {"thread_id": thread_id, "input": {}},
            "idempotency_key": "ws-turn-1",
        }),
    )
    .await;

    // Follow the run to its named end boundary.
    let frames = recv_until(&mut ws, Duration::from_secs(20), |frame| {
        frame["frame"] == json!("event") && frame["name"] == json!("run_ended")
    })
    .await;

    // The request was answered with the scheduled run.
    let response = frames
        .iter()
        .find(|f| f["frame"] == json!("response"))
        .expect("submit_turn answers a response frame");
    assert_eq!(response["id"], json!(1));
    let run_id = response["ok"]["result"]["run_id"]
        .as_str()
        .expect("the run id answers in the result")
        .to_string();
    assert!(
        matches!(
            response["ok"]["result"]["status"].as_str(),
            Some("running") | Some("pending")
        ),
        "{response}"
    );

    // Every event carries a strictly increasing per-connection seq.
    let seqs: Vec<u64> = frames
        .iter()
        .filter(|f| f["frame"] == json!("event"))
        .map(|f| f["seq"].as_u64().unwrap())
        .collect();
    assert!(
        seqs.windows(2).all(|w| w[1] > w[0]),
        "seq is strictly increasing: {seqs:?}"
    );
    assert!(
        seqs.len() >= 100,
        "a 60-step run streams well past 100 events, got {}",
        seqs.len()
    );

    // The named run boundaries arrive in order, never inferred (AC4), and
    // the admission verdict carries the contract's reason vocabulary.
    let names: Vec<&str> = frames
        .iter()
        .filter(|f| f["frame"] == json!("event"))
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    let admitted = names.iter().position(|n| *n == "run_admitted").unwrap();
    let started = names.iter().position(|n| *n == "run_started").unwrap();
    let ended = names.iter().position(|n| *n == "run_ended").unwrap();
    assert!(admitted < started && started < ended, "{names:?}");
    let boundary = |name: &str| {
        frames
            .iter()
            .find(|f| f["name"] == json!(name))
            .cloned()
            .unwrap()
    };
    assert_eq!(boundary("run_admitted")["payload"]["run_id"], json!(run_id));
    assert_eq!(
        boundary("run_admitted")["payload"]["reason"],
        json!("queued")
    );
    assert_eq!(boundary("run_started")["payload"]["run_id"], json!(run_id));
    assert_eq!(boundary("run_ended")["payload"]["run_id"], json!(run_id));
    assert_eq!(boundary("run_ended")["payload"]["status"], json!("success"));

    // Incremental run traffic rides the same connection, teed off the
    // run's frame sink — the SSE stream's event source.
    let run_frames: Vec<&Value> = frames
        .iter()
        .filter(|f| f["name"] == json!("run_frame"))
        .collect();
    assert!(!run_frames.is_empty(), "{names:?}");
    assert!(
        run_frames
            .iter()
            .all(|f| f["payload"]["run_id"] == json!(run_id)
                && f["payload"]["run_seq"].as_u64().is_some()),
        "run frames carry their run identity and per-run sequence"
    );

    server.abort();
    let _ = std::fs::remove_dir_all(&store);
}

#[tokio::test]
async fn resnapshot_restarts_sequencing() {
    let store = temp_store();
    let (graph, spec) = spinner_graph(1);
    let mut registry = GraphRegistry::new();
    registry.register("spin", graph, spec);
    let (addr, server) = spawn_server(registry, open_config(store.clone())).await;
    let (mut ws, first) = connect(addr, None).await;
    assert_eq!(first["payload"]["kind"], json!("snapshot"), "{first}");

    // AC3: the client requests a re-snapshot; the gateway answers, then
    // serves a fresh snapshot with sequencing restarted.
    send_frame(
        &mut ws,
        json!({"frame": "request", "id": 9, "method": "resnapshot", "params": {}}),
    )
    .await;
    let response = recv_frame(&mut ws, Duration::from_secs(5)).await;
    assert_eq!(response["frame"], json!("response"), "{response}");
    assert_eq!(response["id"], json!(9), "{response}");
    assert_eq!(
        response["ok"]["result"]["resnapshotted"],
        json!(true),
        "{response}"
    );

    let snapshot = recv_frame(&mut ws, Duration::from_secs(5)).await;
    assert_eq!(snapshot["frame"], json!("event"), "{snapshot}");
    assert_eq!(snapshot["name"], json!("snapshot"), "{snapshot}");
    assert_eq!(
        snapshot["seq"],
        json!(1),
        "the re-snapshot restarts the connection's sequencing: {snapshot}"
    );

    // ...and the stream continues monotonically from the restart.
    server.abort();
    let _ = std::fs::remove_dir_all(&store);
}

#[tokio::test]
async fn tenant_streams_are_isolated() {
    let store = temp_store();
    let (graph, spec) = spinner_graph(3);
    let mut registry = GraphRegistry::new();
    registry.register("spin", graph, spec);
    let config = open_config(store.clone())
        .with_tenant_key("acme", "k-acme")
        .with_tenant_key("globex", "k-globex");
    let (addr, server) = spawn_server(registry, config).await;

    // Without a key the upgrade is refused before any protocol traffic.
    let anonymous = format!("ws://{addr}/gateway/ws")
        .into_client_request()
        .unwrap();
    assert!(tokio_tungstenite::connect_async(anonymous).await.is_err());

    // globex's connection sees its own (empty) snapshot, then silence
    // while acme runs a full turn.
    let (mut globex_ws, snapshot) = connect(addr, Some("k-globex")).await;
    assert_eq!(snapshot["payload"]["runs"], json!([]), "{snapshot}");

    let client = reqwest::Client::new();
    let base = format!("http://{addr}");
    let thread: Value = client
        .post(format!("{base}/threads"))
        .header("x-api-key", "k-acme")
        .json(&json!({"graph": "spin"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let thread_id = thread["thread_id"].as_str().unwrap().to_string();
    let run: Value = client
        .post(format!("{base}/threads/{thread_id}/runs/wait"))
        .header("x-api-key", "k-acme")
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(run["status"], json!("success"), "{run}");

    // acme's whole turn — admitted, started, every frame, ended — is
    // invisible on globex's socket: any frame at all within the window is
    // a leak (nothing globex did generates one).
    let leaked = tokio::time::timeout(Duration::from_millis(800), async {
        loop {
            match globex_ws.next().await {
                Some(Ok(Message::Text(text))) => return Some(text.to_string()),
                // The socket ending mid-window is a leak of a different
                // kind; keep watching otherwise (pings, pongs).
                Some(Ok(_)) => {}
                other => return Some(format!("socket ended: {other:?}")),
            }
        }
    })
    .await
    .unwrap_or(None);
    assert!(
        leaked.is_none(),
        "no cross-tenant traffic reaches the socket: {leaked:?}"
    );

    server.abort();
    let _ = std::fs::remove_dir_all(&store);
}
