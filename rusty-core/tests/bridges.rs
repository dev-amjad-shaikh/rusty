//! R0.9 wave 4 — the client halves of the bridges: the journaled MCP tool
//! (live journaling, replay serving, derived ids) and the A2A durable node
//! (delegation lifecycle, journaled `RemoteCall`, replay serving,
//! cancellation propagation). Server-side bridge coverage lives in
//! `rusty-server/tests/bridges.rs`.
//!
//! The MCP half uses an in-memory `duplex` transport (the `mcp.rs` unit-test
//! convention); the A2A half uses a hand-rolled mock HTTP/1.1 server (the
//! `remote.rs` convention) — no extra dev-dependencies.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use rusty_agent_runtime::mcp::{mcp_tool_effect_id, JournaledMcpTool, McpClient, McpToolInfo};
use rusty_agent_runtime::prelude::*;
use rusty_agent_runtime::replay::tool_call_request;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::task::JoinHandle;

// --------------------------------------------------------------------- //
// Mock MCP server (newline-delimited framing, the stdio convention)
// --------------------------------------------------------------------- //

/// A scripted mock MCP server over an in-memory duplex stream: the full
/// handshake plus the `echo` / `error_tool` tools.
async fn run_mcp_mock(stream: tokio::io::DuplexStream) {
    let (read, mut write) = tokio::io::split(stream);
    let mut reader = BufReader::new(read);
    let mut line = String::new();
    loop {
        line.clear();
        let Ok(n) = reader.read_line(&mut line).await else {
            return;
        };
        if n == 0 {
            return;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let response = match method {
            "notifications/initialized" => None,
            "initialize" => Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "bridges-mock", "version": "0.0.1"},
                }
            })),
            "tools/list" => Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "tools": [
                        {
                            "name": "echo",
                            "description": "Echoes text back.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {"text": {"type": "string"}},
                                "required": ["text"]
                            }
                        },
                        {
                            "name": "error_tool",
                            "description": "Reports a tool-level error.",
                            "inputSchema": {"type": "object"}
                        }
                    ]
                }
            })),
            "tools/call" => {
                let name = msg
                    .pointer("/params/name")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                match name {
                    "echo" => Some(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {"content": [{"type": "text", "text": "hello from echo"}]}
                    })),
                    "error_tool" => Some(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{"type": "text", "text": "invalid widget id"}],
                            "isError": true
                        }
                    })),
                    _ => Some(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {"code": -32601, "message": "unknown tool"}
                    })),
                }
            }
            _ => id.map(|i| {
                json!({
                    "jsonrpc": "2.0",
                    "id": i,
                    "error": {"code": -32601, "message": "method not found"}
                })
            }),
        };
        if let Some(resp) = response {
            let mut bytes = serde_json::to_vec(&resp).expect("serialize");
            bytes.push(b'\n');
            write.write_all(&bytes).await.expect("mock write");
        }
    }
}

/// An initialized client plus the tool infos, over the mock.
async fn initialized_client() -> (McpClient, Vec<McpToolInfo>, JoinHandle<()>) {
    let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);
    let handle = tokio::spawn(run_mcp_mock(server_stream));
    let (read, write) = tokio::io::split(client_stream);
    let client = McpClient::connect(read, write);
    client.initialize().await.expect("initialize");
    let infos = client.list_tools().await.expect("tools/list");
    (client, infos, handle)
}

fn info_of(infos: &[McpToolInfo], name: &str) -> McpToolInfo {
    infos
        .iter()
        .find(|t| t.name == name)
        .unwrap_or_else(|| panic!("tool `{name}`"))
        .clone()
}

fn journal(run_id: &str) -> Journal {
    Journal::new(run_id, "thread-bridges", Clock::System)
}

/// The inline input/output payload of a journaled event.
fn payload(event: &RunEvent, key: fn(&RunEvent) -> &Option<PayloadRef>) -> Value {
    key(event)
        .as_ref()
        .and_then(|p| match p {
            PayloadRef::Inline(v) => Some(v.clone()),
            PayloadRef::Artifact(_) => None,
        })
        .expect("bridge test payloads travel inline")
}

// --------------------------------------------------------------------- //
// JournaledMcpTool — live
// --------------------------------------------------------------------- //

#[tokio::test]
async fn journaled_mcp_call_is_journaled_in_the_replay_shape() {
    let (client, infos, _mock) = initialized_client().await;
    let journal = journal("run-mcp-live");
    let tool = JournaledMcpTool::new(
        client.clone(),
        info_of(&infos, "echo"),
        "run-mcp-live",
        journal.clone(),
        "run-mcp-live:0",
    );

    let args = json!({"text": "hi"});
    let out = tool.call(args.clone()).await.expect("echo call");
    assert_eq!(out, json!("hello from echo"));

    // The derived key is the call's identity, reported through the Tool
    // surface, and matches the free-function derivation.
    let id = mcp_tool_effect_id("run-mcp-live", "echo", &args);
    assert_eq!(tool.effect_id(&args), id);
    assert_eq!(tool.idempotency_key(&args).as_deref(), Some(id.as_str()));

    let events = journal.events();
    assert_eq!(events.len(), 1, "exactly one ToolCall event: {events:?}");
    let event = &events[0];
    assert_eq!(event.kind, RunEventKind::ToolCall);
    assert_eq!(event.status, EventStatus::Ok);
    assert_eq!(event.parent.as_deref(), Some("run-mcp-live:0"));
    assert_eq!(
        payload(event, |e| &e.input),
        tool_call_request("echo", &args)
    );
    assert_eq!(payload(event, |e| &e.output), json!("hello from echo"));

    client.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn journaled_mcp_failure_is_journaled_with_error_status() {
    let (client, infos, _mock) = initialized_client().await;
    let journal = journal("run-mcp-fail");
    let tool = JournaledMcpTool::new(
        client.clone(),
        info_of(&infos, "error_tool"),
        "run-mcp-fail",
        journal.clone(),
        "run-mcp-fail:0",
    );

    let err = tool.call(json!({})).await.unwrap_err();
    assert!(err.to_string().contains("invalid widget id"), "got: {err}");

    let events = journal.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].status, EventStatus::Error);
    let output = payload(&events[0], |e| &e.output);
    assert!(
        output["error"]
            .as_str()
            .unwrap()
            .contains("invalid widget id"),
        "the journaled error payload: {output}"
    );

    client.shutdown().await.expect("shutdown");
}

// --------------------------------------------------------------------- //
// JournaledMcpTool — replay
// --------------------------------------------------------------------- //

#[tokio::test]
async fn journaled_mcp_replay_serves_the_recorded_response_without_a_client() {
    // Arrange: record a live call into a journal, then shut the transport
    // down entirely — whatever the replay serves cannot have come from the
    // server.
    let (client, infos, _mock) = initialized_client().await;
    let recorded = journal("run-mcp-recorded");
    let live = JournaledMcpTool::new(
        client.clone(),
        info_of(&infos, "echo"),
        "run-mcp-recorded",
        recorded.clone(),
        "run-mcp-recorded:0",
    );
    let args = json!({"text": "recorded"});
    live.call(args.clone()).await.expect("record the call");
    client.shutdown().await.expect("shutdown");

    // Act: replay from the recorded snapshot. The replaying constructor
    // takes no client at all — the "never respawns the stdio server"
    // property is the type, not a promise.
    let snapshot = recorded.snapshot();
    let source = ReplaySource::new(&snapshot);
    let replay_journal = journal("run-mcp-replay");
    let replaying = JournaledMcpTool::replaying(
        info_of(&infos, "echo"),
        "run-mcp-recorded",
        source,
        replay_journal.clone(),
        "run-mcp-replay:0",
    );
    let out = replaying.call(args.clone()).await.expect("served call");
    assert_eq!(out, json!("hello from echo"));

    // The replayed run re-journals the served event (the ReplayingTool
    // precedent): kind, payloads, and status reproduce the recorded event.
    let events = replay_journal.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, RunEventKind::ToolCall);
    assert_eq!(events[0].parent.as_deref(), Some("run-mcp-replay:0"));
    assert_eq!(
        payload(&events[0], |e| &e.input),
        tool_call_request("echo", &args)
    );
    assert_eq!(payload(&events[0], |e| &e.output), json!("hello from echo"));

    // A call the journal does not record is a divergence, not a new
    // outbound call.
    let err = replaying
        .call(json!({"text": "different"}))
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("replay serve failed"),
        "got: {err}"
    );
}

#[test]
fn mcp_tool_effect_id_is_deterministic_and_input_sensitive() {
    let a = mcp_tool_effect_id("run-1", "echo", &json!({"text": "x"}));
    let b = mcp_tool_effect_id("run-1", "echo", &json!({"text": "x"}));
    assert_eq!(a, b, "same (scope, tool, args) derives the same id");
    assert_ne!(
        a,
        mcp_tool_effect_id("run-1", "echo", &json!({"text": "y"})),
        "arguments are committed"
    );
    assert_ne!(
        a,
        mcp_tool_effect_id("run-2", "echo", &json!({"text": "x"})),
        "the run scope is committed"
    );
    assert_ne!(
        a,
        mcp_tool_effect_id("run-1", "other", &json!({"text": "x"})),
        "the tool name is committed"
    );
}

// --------------------------------------------------------------------- //
// Mock A2A server (hand-rolled HTTP/1.1, the remote.rs convention)
// --------------------------------------------------------------------- //

/// What the mock does with a request: respond with a status + JSON body,
/// keyed off the JSON-RPC method in the body.
struct MockA2a {
    addr: SocketAddr,
    bodies: Arc<StdMutex<Vec<String>>>,
    _handle: JoinHandle<()>,
}

/// Read one HTTP/1.1 request; returns the body.
async fn read_http_request(stream: &mut tokio::net::TcpStream) -> Option<String> {
    let mut buf = vec![0u8; 8192];
    let mut filled = 0usize;
    let (headers_end, content_length) = loop {
        let n = stream.read(&mut buf[filled..]).await.ok()?;
        if n == 0 {
            return None;
        }
        filled += n;
        let text = String::from_utf8_lossy(&buf[..filled]);
        if let Some(pos) = text.find("\r\n\r\n") {
            let headers = &text[..pos];
            let len = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|v| v.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            break (pos + 4, len);
        }
        if filled == buf.len() {
            buf.resize(buf.len() * 2, 0);
        }
    };
    while filled < headers_end + content_length {
        let n = stream.read(&mut buf[filled..]).await.ok()?;
        if n == 0 {
            return None;
        }
        filled += n;
    }
    Some(String::from_utf8_lossy(&buf[headers_end..headers_end + content_length]).to_string())
}

/// Start the mock: `responder` maps each JSON-RPC method to its `result`
/// body (or a closure-computed value).
fn start_a2a_mock<F>(responder: F) -> MockA2a
where
    F: Fn(&str, &Value) -> Value + std::marker::Send + Sync + 'static,
{
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let bodies: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
    let bodies2 = bodies.clone();
    let responder = Arc::new(responder);
    let handle = tokio::spawn(async move {
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                continue;
            };
            let bodies = bodies2.clone();
            let responder = responder.clone();
            tokio::spawn(async move {
                let Some(body) = read_http_request(&mut stream).await else {
                    return;
                };
                bodies.lock().unwrap().push(body.clone());
                let request: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                let method = request.get("method").and_then(Value::as_str).unwrap_or("");
                let params = request.get("params").cloned().unwrap_or(Value::Null);
                let result = responder(method, &params);
                let response = json!({"jsonrpc": "2.0", "id": 1, "result": result});
                let bytes = serde_json::to_vec(&response).unwrap();
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    bytes.len()
                );
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(&bytes).await;
                let _ = stream.flush().await;
            });
        }
    });
    MockA2a {
        addr,
        bodies,
        _handle: handle,
    }
}

/// A node context with the causal parent wired the way the executor wires it.
fn a2a_ctx(state: Value, parent: &str) -> NodeContext {
    NodeContext::new(
        State::from_value(state).unwrap(),
        NodeConfig {
            thread_id: "thread-a2a".into(),
            step: 3,
            resume: None,
            extra: HashMap::from([(PARENT_EVENT_KEY.to_string(), json!(parent))]),
        },
    )
}

fn a2a_task(id: &str, state: &str) -> Value {
    json!({
        "id": id,
        "contextId": "thread-a2a",
        "status": {"state": state, "timestamp": "2026-01-01T00:00:00Z"},
    })
}

// --------------------------------------------------------------------- //
// A2aNode — live lifecycle
// --------------------------------------------------------------------- //

#[tokio::test]
async fn a2a_node_delegates_polls_and_journals_the_remote_call() {
    let polls = Arc::new(AtomicUsize::new(0));
    let polls2 = polls.clone();
    let mock = start_a2a_mock(move |method, params| match method {
        "message/send" => a2a_task("task-1", "submitted"),
        "tasks/get" => {
            assert_eq!(params["id"], json!("task-1"), "polls name the task");
            if polls2.fetch_add(1, Ordering::SeqCst) == 0 {
                a2a_task("task-1", "working")
            } else {
                let mut task = a2a_task("task-1", "completed");
                task["artifacts"] = json!([{
                    "artifactId": "art-1",
                    "name": "answer",
                    "parts": [{"kind": "data", "data": {"answer": 42}}],
                }]);
                task
            }
        }
        other => panic!("unexpected method {other}"),
    });

    let journal = journal("run-a2a-live");
    let node = A2aNode::new("researcher", format!("http://{}", mock.addr))
        .with_poll_interval(std::time::Duration::from_millis(10))
        .with_journal(journal.clone());
    let out = node
        .run(a2a_ctx(json!({"question": "life"}), "run-a2a-live:0"))
        .await
        .expect("delegation completes");

    // The outcome lands on the configured channel with the task's artifacts.
    assert_eq!(out.updates["a2a_outcome"]["task_id"], json!("task-1"));
    assert_eq!(
        out.updates["a2a_outcome"]["artifacts"][0]["parts"][0]["data"],
        json!({"answer": 42})
    );

    // The wire shape: one message/send carrying the derived idempotency
    // handle, then tasks/get polls.
    let bodies = mock.bodies.lock().unwrap();
    let send: Value = serde_json::from_str(&bodies[0]).unwrap();
    assert_eq!(send["method"], json!("message/send"));
    assert_eq!(
        send["params"]["message"]["messageId"],
        json!("a2a-thread-a2a-3-researcher"),
        "the messageId is the derived idempotency handle"
    );
    assert_eq!(send["params"]["message"]["contextId"], json!("thread-a2a"));
    assert_eq!(
        send["params"]["message"]["parts"][0]["data"],
        json!({"question": "life"})
    );
    assert!(bodies[1..]
        .iter()
        .all(|b| serde_json::from_str::<Value>(b).unwrap()["method"] == json!("tasks/get")));

    // The journal holds exactly one RemoteCall: request params in, the
    // terminal task out, parented on the invoking node event.
    let events = journal.events();
    assert_eq!(events.len(), 1, "one RemoteCall event: {events:?}");
    let event = &events[0];
    assert_eq!(event.kind, RunEventKind::RemoteCall);
    assert_eq!(event.status, EventStatus::Ok);
    assert_eq!(event.node_id.as_deref(), Some("researcher"));
    assert_eq!(event.parent.as_deref(), Some("run-a2a-live:0"));
    let input = payload(event, |e| &e.input);
    assert_eq!(
        input["message"]["messageId"],
        json!("a2a-thread-a2a-3-researcher")
    );
    let output = payload(event, |e| &e.output);
    assert_eq!(output["status"]["state"], json!("completed"));
    assert!(
        polls.load(Ordering::SeqCst) >= 2,
        "the task was polled: {bodies:?}"
    );
}

#[tokio::test]
async fn a2a_node_failed_task_is_a_node_error_and_journaled() {
    let mock = start_a2a_mock(|method, _| match method {
        "message/send" => a2a_task("task-9", "submitted"),
        "tasks/get" => {
            let mut task = a2a_task("task-9", "failed");
            task["status"]["message"] = json!({
                "role": "agent",
                "parts": [{"kind": "text", "text": "upstream model unavailable"}],
            });
            task
        }
        other => panic!("unexpected method {other}"),
    });

    let journal = journal("run-a2a-fail");
    let node = A2aNode::new("researcher", format!("http://{}", mock.addr))
        .with_poll_interval(std::time::Duration::from_millis(10))
        .with_journal(journal.clone());
    let err = node
        .run(a2a_ctx(json!({}), "run-a2a-fail:0"))
        .await
        .unwrap_err();
    assert!(matches!(err, RustyError::Node(_)), "got: {err}");
    assert!(
        err.to_string().contains("upstream model unavailable"),
        "got: {err}"
    );

    let events = journal.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].status, EventStatus::Error);
    assert_eq!(
        payload(&events[0], |e| &e.output)["status"]["state"],
        json!("failed")
    );
}

// --------------------------------------------------------------------- //
// A2aNode — replay
// --------------------------------------------------------------------- //

#[tokio::test]
async fn a2a_node_replay_serves_the_recorded_outcome() {
    // Arrange: record a live delegation.
    let mock = start_a2a_mock(|method, _| match method {
        "message/send" => a2a_task("task-1", "submitted"),
        "tasks/get" => {
            let mut task = a2a_task("task-1", "completed");
            task["artifacts"] = json!([{
                "artifactId": "art-1",
                "parts": [{"kind": "data", "data": {"answer": 42}}],
            }]);
            task
        }
        other => panic!("unexpected method {other}"),
    });
    let recorded = journal("run-a2a-recorded");
    let live = A2aNode::new("researcher", format!("http://{}", mock.addr))
        .with_poll_interval(std::time::Duration::from_millis(10))
        .with_journal(recorded.clone());
    live.run(a2a_ctx(json!({"question": "life"}), "run-a2a-recorded:0"))
        .await
        .expect("record the delegation");
    drop(mock);

    // Act: replay with no reachable server at all — the replaying node
    // holds no HTTP client, so the address is decorative.
    let snapshot = recorded.snapshot();
    let source = ReplaySource::new(&snapshot);
    let replay_journal = journal("run-a2a-replay");
    let replaying =
        A2aNode::new("researcher", "http://127.0.0.1:1").replaying(source, replay_journal.clone());
    let out = replaying
        .run(a2a_ctx(json!({"question": "life"}), "run-a2a-replay:0"))
        .await
        .expect("served outcome");
    assert_eq!(out.updates["a2a_outcome"]["task_id"], json!("task-1"));
    assert_eq!(
        out.updates["a2a_outcome"]["artifacts"][0]["parts"][0]["data"],
        json!({"answer": 42})
    );

    let events = replay_journal.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, RunEventKind::RemoteCall);
    assert_eq!(events[0].parent.as_deref(), Some("run-a2a-replay:0"));

    // A divergent request is refused, not served.
    let source = ReplaySource::new(&snapshot);
    let diverging = A2aNode::new("researcher", "http://127.0.0.1:1")
        .replaying(source, journal("run-a2a-diverge"));
    let err = diverging
        .run(a2a_ctx(
            json!({"question": "something else"}),
            "run-a2a-diverge:0",
        ))
        .await
        .unwrap_err();
    assert!(matches!(err, RustyError::Replay(_)), "got: {err}");
}

// --------------------------------------------------------------------- //
// A2aNode — cancellation
// --------------------------------------------------------------------- //

#[tokio::test]
async fn a2a_node_cancellation_cancels_the_remote_task() {
    let mock = start_a2a_mock(|method, _| match method {
        "message/send" => a2a_task("task-7", "submitted"),
        "tasks/get" => a2a_task("task-7", "working"), // never terminal
        "tasks/cancel" => a2a_task("task-7", "canceled"),
        other => panic!("unexpected method {other}"),
    });

    let journal = journal("run-a2a-cancel");
    let token = tokio_util::sync::CancellationToken::new();
    let node = A2aNode::new("researcher", format!("http://{}", mock.addr))
        .with_poll_interval(std::time::Duration::from_millis(50))
        .with_journal(journal.clone())
        .with_cancellation(token.clone());

    let run = tokio::spawn(async move { node.run(a2a_ctx(json!({}), "run-a2a-cancel:0")).await });
    // Let the delegation land, then cancel mid-poll.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    token.cancel();
    let err = run.await.expect("join").unwrap_err();
    assert!(matches!(err, RustyError::Cancelled(_)), "got: {err}");

    // The remote task was sent tasks/cancel, and the canceled outcome is
    // journaled as evidence of where the delegation stopped.
    let bodies = mock.bodies.lock().unwrap();
    assert!(
        bodies
            .iter()
            .any(|b| serde_json::from_str::<Value>(b).unwrap()["method"] == json!("tasks/cancel")),
        "tasks/cancel reached the agent: {bodies:?}"
    );
    let events = journal.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].status, EventStatus::Error);
    assert_eq!(
        payload(&events[0], |e| &e.output)["status"]["state"],
        json!("canceled")
    );
}
