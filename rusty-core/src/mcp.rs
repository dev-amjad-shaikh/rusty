//! MCP (Model Context Protocol) client support.
//!
//! This module lets Rusty Core agents call tools hosted by **any MCP
//! server**. It provides:
//!
//! - JSON-RPC 2.0 framing types ([`JsonRpcRequest`], [`JsonRpcResponse`],
//!   [`JsonRpcNotification`], [`JsonRpcError`]) with `serde` support.
//! - A transport-generic [`McpClient`] over tokio `AsyncRead`/`AsyncWrite`,
//!   supporting both **newline-delimited JSON** (MCP stdio) and **LSP-style
//!   `Content-Length` headers** ([`Framing`]). Every request carries a
//!   timeout; a background reader task routes responses to their waiters.
//! - [`McpStdioClient::spawn`] to launch an MCP server as a child process
//!   with piped stdin/stdout.
//! - [`McpToolAdapter`], which wraps a single MCP tool as a Rusty Core
//!   [`Tool`], and [`McpClient::into_tools`], which lists the server's tools
//!   and returns them as `Vec<Arc<dyn Tool>>` for direct registration in a
//!   [`crate::tool::ToolRegistry`].
//!
//! All failures map to [`RustyError::Tool`] with an `mcp:` context
//! prefix.
//!
//! ```no_run
//! # async fn demo() -> rusty_agent_runtime::error::Result<()> {
//! use rusty_agent_runtime::mcp::McpStdioClient;
//!
//! let client = McpStdioClient::spawn("npx", &["-y", "@modelcontextprotocol/server-everything"])?;
//! client.initialize().await?;
//! let tools = client.into_tools().await?;
//! // register into a ToolRegistry and hand to a ReAct agent...
//! # let _ = tools;
//! client.shutdown().await?;
//! # Ok(())
//! # }
//! ```

use std::collections::HashMap;
use std::io;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::error::{Result, RustyError};
use crate::tool::Tool;

/// The MCP protocol revision this client requests during `initialize`.
///
/// `2024-11-05` is the most widely implemented revision. The revision the
/// server answers with is **recorded, not validated**: this client performs
/// no compatibility negotiation beyond sending this value.
pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

/// Default per-request timeout.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum size of a single inbound frame (bytes), for both framings.
///
/// The peer is untrusted: without a cap, a hostile or buggy server could
/// declare `Content-Length: 4_000_000_000` (or stream one unterminated line)
/// and turn it into a multi-GiB host allocation. Oversized frames are
/// rejected *before* any length-driven allocation happens.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// Build an [`RustyError::Tool`] with an `mcp:` context prefix.
fn tool_err(msg: impl Into<String>) -> RustyError {
    RustyError::Tool(format!("mcp: {}", msg.into()))
}

// ---------------------------------------------------------------------------
// JSON-RPC 2.0 framing types
// ---------------------------------------------------------------------------

/// A JSON-RPC 2.0 request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    /// Always `"2.0"`.
    pub jsonrpc: String,
    /// Request identifier (this client uses monotonically increasing integers).
    pub id: u64,
    /// Method name, e.g. `"tools/call"`.
    pub method: String,
    /// Structured parameters.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl JsonRpcRequest {
    /// A `"2.0"` request with parameters.
    pub fn new(id: u64, method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_owned(),
            id,
            method: method.into(),
            params: Some(params),
        }
    }
}

/// A JSON-RPC 2.0 notification (no `id`, no response expected).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    /// Always `"2.0"`.
    pub jsonrpc: String,
    /// Method name, e.g. `"notifications/initialized"`.
    pub method: String,
    /// Structured parameters.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl JsonRpcNotification {
    /// A `"2.0"` notification with parameters.
    pub fn new(method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_owned(),
            method: method.into(),
            params: Some(params),
        }
    }
}

/// A JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    /// Error code (e.g. `-32602` invalid params, `-32603` internal error).
    pub code: i64,
    /// Human-readable message.
    pub message: String,
    /// Optional structured data.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl std::fmt::Display for JsonRpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JSON-RPC error {}: {}", self.code, self.message)
    }
}

/// A JSON-RPC 2.0 response. Exactly one of `result` / `error` is present.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    /// Always `"2.0"`.
    pub jsonrpc: String,
    /// Echoed request identifier.
    pub id: Value,
    /// Success payload.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Failure payload.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

// ---------------------------------------------------------------------------
// Wire framing
// ---------------------------------------------------------------------------

/// How JSON-RPC messages are framed on the byte stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Framing {
    /// One JSON object per line — the MCP stdio transport.
    #[default]
    NewlineDelimited,
    /// LSP-style `Content-Length: N\r\n\r\n<body>` headers.
    ContentLength,
}

/// Write one framed JSON message.
async fn write_framed<W>(writer: &mut W, framing: Framing, value: &Value) -> io::Result<()>
where
    W: AsyncWrite + Unpin + ?Sized,
{
    let body =
        serde_json::to_vec(value).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    match framing {
        Framing::NewlineDelimited => {
            writer.write_all(&body).await?;
            writer.write_all(b"\n").await?;
        }
        Framing::ContentLength => {
            let header = format!("Content-Length: {}\r\n\r\n", body.len());
            writer.write_all(header.as_bytes()).await?;
            writer.write_all(&body).await?;
        }
    }
    writer.flush().await
}

/// Read one framed JSON message. Returns `Ok(None)` on clean EOF.
///
/// Bounded by [`MAX_FRAME_BYTES`]: an oversized `Content-Length` header or
/// an over-long unterminated line is an `InvalidData` error, never an
/// allocation.
async fn read_framed<R>(reader: &mut BufReader<R>, framing: Framing) -> io::Result<Option<Value>>
where
    R: AsyncRead + Unpin,
{
    match framing {
        Framing::NewlineDelimited => {
            let mut line = String::new();
            loop {
                line.clear();
                // Read through a `take` adapter so a peer that never
                // terminates its line cannot grow the buffer past the cap.
                let n = (&mut *reader)
                    .take(MAX_FRAME_BYTES as u64 + 1)
                    .read_line(&mut line)
                    .await?;
                if n == 0 {
                    return Ok(None); // EOF
                }
                if line.len() > MAX_FRAME_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "newline-delimited frame exceeds the {MAX_FRAME_BYTES}-byte frame cap"
                        ),
                    ));
                }
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue; // tolerate blank lines
                }
                let value = serde_json::from_str(trimmed)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                return Ok(Some(value));
            }
        }
        Framing::ContentLength => {
            let mut content_length: Option<usize> = None;
            let mut line = String::new();
            loop {
                line.clear();
                let n = reader.read_line(&mut line).await?;
                if n == 0 {
                    return Ok(None); // EOF before/inside headers
                }
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    break; // end of headers
                }
                if let Some((name, val)) = trimmed.split_once(':') {
                    if name.trim().eq_ignore_ascii_case("content-length") {
                        content_length = val.trim().parse().ok();
                    }
                }
            }
            let len = content_length.ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "missing Content-Length header")
            })?;
            if len > MAX_FRAME_BYTES {
                // Reject before trusting the peer's length with an allocation.
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Content-Length {len} exceeds the {MAX_FRAME_BYTES}-byte frame cap"),
                ));
            }
            let mut body = vec![0u8; len];
            reader.read_exact(&mut body).await?;
            let value = serde_json::from_slice(&body)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            Ok(Some(value))
        }
    }
}

// ---------------------------------------------------------------------------
// MCP metadata types
// ---------------------------------------------------------------------------

/// The parsed result of the MCP `initialize` handshake.
#[derive(Debug, Clone)]
pub struct InitializeResult {
    /// Protocol revision the server will use.
    pub protocol_version: String,
    /// Server implementation name (`serverInfo.name`).
    pub server_name: String,
    /// Server implementation version (`serverInfo.version`).
    pub server_version: String,
    /// Raw server capabilities object.
    pub capabilities: Value,
}

/// Metadata for one MCP tool, as returned by `tools/list`.
#[derive(Debug, Clone)]
pub struct McpToolInfo {
    /// Tool name.
    pub name: String,
    /// Human/model-facing description (may be empty).
    pub description: String,
    /// JSON Schema for the tool's arguments (`inputSchema`).
    pub input_schema: Value,
}

// ---------------------------------------------------------------------------
// McpClient
// ---------------------------------------------------------------------------

type BoxedWriter = Box<dyn AsyncWrite + Unpin + Send>;
type PendingMap = HashMap<u64, oneshot::Sender<Value>>;

struct ClientInner {
    framing: Framing,
    writer: Arc<Mutex<BoxedWriter>>,
    pending: Arc<Mutex<PendingMap>>,
    next_id: AtomicU64,
    closed: AtomicBool,
    initialized: AtomicBool,
    request_timeout: StdMutex<Duration>,
    reader_handle: StdMutex<Option<JoinHandle<()>>>,
    child: StdMutex<Option<Child>>,
}

impl Drop for ClientInner {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.reader_handle.lock() {
            if let Some(handle) = guard.take() {
                handle.abort();
            }
        }
        if let Ok(mut guard) = self.child.lock() {
            if let Some(mut child) = guard.take() {
                let _ = child.start_kill();
            }
        }
    }
}

/// Background task: reads framed messages from the server and routes them.
///
/// - Responses (`id` + `result`/`error`) are delivered to the matching
///   pending oneshot.
/// - Server-initiated requests (`method` + `id`) get a `-32601` reply, since
///   this client serves no methods.
/// - Notifications (`method`, no `id`) are ignored.
///
/// On EOF or a fatal read error the task drains `pending`, waking all waiters
/// with a "connection closed" error.
async fn reader_loop<R>(
    reader: R,
    framing: Framing,
    pending: Arc<Mutex<PendingMap>>,
    writer: Arc<Mutex<BoxedWriter>>,
) where
    R: AsyncRead + Unpin,
{
    let mut reader = BufReader::new(reader);
    loop {
        match read_framed(&mut reader, framing).await {
            Ok(Some(msg)) => {
                if msg.get("method").is_some() {
                    if let Some(id) = msg.get("id") {
                        let reply = json!({
                            "jsonrpc": "2.0",
                            "id": id.clone(),
                            "error": {"code": -32601, "message": "method not found"},
                        });
                        let mut w = writer.lock().await;
                        if write_framed(&mut **w, framing, &reply).await.is_err() {
                            break;
                        }
                    }
                    // else: notification — ignore.
                } else if let Some(id) = msg.get("id").and_then(Value::as_u64) {
                    let tx = pending.lock().await.remove(&id);
                    if let Some(tx) = tx {
                        let _ = tx.send(msg);
                    }
                }
            }
            Ok(None) => break, // clean EOF
            Err(e) => {
                // Newline framing is self-resyncing: a malformed or oversized
                // frame costs one line, not the whole session. IO errors and
                // Content-Length desync are fatal — the stream can no longer
                // be trusted, so pending waiters are woken below.
                if framing == Framing::NewlineDelimited && e.kind() == io::ErrorKind::InvalidData {
                    tracing::warn!(
                        error = %e,
                        "mcp: dropping malformed frame; resyncing on the next line"
                    );
                    continue;
                }
                tracing::warn!(error = %e, "mcp: reader task terminating on fatal read error");
                break;
            }
        }
    }
    // Wake every waiter: dropping the senders makes the receivers fail.
    pending.lock().await.clear();
}

/// A transport-generic MCP client over a tokio byte stream.
///
/// Cheap to clone (all state is shared); clones see the same connection.
/// Use [`McpClient::connect`] for an arbitrary transport or
/// [`McpStdioClient::spawn`] for a child-process stdio server.
///
/// Lifecycle: `connect` → [`initialize`](McpClient::initialize) →
/// [`list_tools`](McpClient::list_tools) / [`call_tool`](McpClient::call_tool)
/// / [`into_tools`](McpClient::into_tools) → [`shutdown`](McpClient::shutdown).
#[derive(Clone)]
pub struct McpClient {
    inner: Arc<ClientInner>,
}

impl McpClient {
    /// Connect over an arbitrary transport using newline-delimited framing
    /// (the MCP stdio convention).
    pub fn connect<R, W>(reader: R, writer: W) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        Self::connect_with_framing(reader, writer, Framing::NewlineDelimited)
    }

    /// Connect over an arbitrary transport with explicit framing.
    pub fn connect_with_framing<R, W>(reader: R, writer: W, framing: Framing) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        Self::connect_inner(reader, writer, framing, None)
    }

    fn connect_inner<R, W>(reader: R, writer: W, framing: Framing, child: Option<Child>) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let pending: Arc<Mutex<PendingMap>> = Arc::new(Mutex::new(HashMap::new()));
        let writer: Arc<Mutex<BoxedWriter>> = Arc::new(Mutex::new(Box::new(writer)));
        let handle = tokio::spawn(reader_loop(
            reader,
            framing,
            Arc::clone(&pending),
            Arc::clone(&writer),
        ));
        Self {
            inner: Arc::new(ClientInner {
                framing,
                writer,
                pending,
                next_id: AtomicU64::new(1),
                closed: AtomicBool::new(false),
                initialized: AtomicBool::new(false),
                request_timeout: StdMutex::new(DEFAULT_REQUEST_TIMEOUT),
                reader_handle: StdMutex::new(Some(handle)),
                child: StdMutex::new(child),
            }),
        }
    }

    /// The wire framing in use.
    pub fn framing(&self) -> Framing {
        self.inner.framing
    }

    /// `true` once [`initialize`](McpClient::initialize) has completed.
    pub fn is_initialized(&self) -> bool {
        self.inner.initialized.load(Ordering::Relaxed)
    }

    /// Override the per-request timeout (default: 30 s).
    pub fn set_request_timeout(&self, timeout: Duration) {
        if let Ok(mut guard) = self.inner.request_timeout.lock() {
            *guard = timeout;
        }
    }

    fn request_timeout(&self) -> Duration {
        self.inner
            .request_timeout
            .lock()
            .map(|d| *d)
            .unwrap_or(DEFAULT_REQUEST_TIMEOUT)
    }

    fn ensure_open(&self) -> Result<()> {
        if self.inner.closed.load(Ordering::Relaxed) {
            return Err(tool_err("client is shut down"));
        }
        Ok(())
    }

    fn ensure_initialized(&self) -> Result<()> {
        if !self.is_initialized() {
            return Err(tool_err(
                "client is not initialized; call `initialize()` first",
            ));
        }
        Ok(())
    }

    /// Send a request and await its response, with timeout and JSON-RPC
    /// error mapping.
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.ensure_open()?;
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let request = JsonRpcRequest::new(id, method, params);
        let encoded = serde_json::to_value(&request)
            .map_err(|e| tool_err(format!("failed to encode `{method}` request: {e}")))?;

        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().await.insert(id, tx);

        {
            let mut w = self.inner.writer.lock().await;
            if let Err(e) = write_framed(&mut **w, self.inner.framing, &encoded).await {
                self.inner.pending.lock().await.remove(&id);
                return Err(tool_err(format!("failed to send `{method}` request: {e}")));
            }
        }

        let raw = match timeout(self.request_timeout(), rx).await {
            Ok(Ok(value)) => value,
            Ok(Err(_)) => {
                return Err(tool_err(format!(
                    "connection closed while awaiting `{method}` response"
                )));
            }
            Err(_) => {
                self.inner.pending.lock().await.remove(&id);
                return Err(tool_err(format!(
                    "`{method}` request timed out after {:?}",
                    self.request_timeout()
                )));
            }
        };

        let response: JsonRpcResponse = serde_json::from_value(raw)
            .map_err(|e| tool_err(format!("malformed response to `{method}`: {e}")))?;
        if let Some(error) = response.error {
            return Err(tool_err(format!(
                "`{method}` failed (code {}): {}",
                error.code, error.message
            )));
        }
        response.result.ok_or_else(|| {
            tool_err(format!(
                "`{method}` response carried neither result nor error"
            ))
        })
    }

    /// Send a notification (no response expected).
    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.ensure_open()?;
        let notification = JsonRpcNotification::new(method, params);
        let encoded = serde_json::to_value(&notification)
            .map_err(|e| tool_err(format!("failed to encode `{method}` notification: {e}")))?;
        let mut w = self.inner.writer.lock().await;
        write_framed(&mut **w, self.inner.framing, &encoded)
            .await
            .map_err(|e| tool_err(format!("failed to send `{method}` notification: {e}")))
    }

    /// Perform the MCP `initialize` handshake: negotiate the protocol
    /// revision, advertise `clientInfo`/capabilities, then send the
    /// `notifications/initialized` notification.
    pub async fn initialize(&self) -> Result<InitializeResult> {
        let params = json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {
                "name": "rusty-core",
                "version": env!("CARGO_PKG_VERSION"),
            },
        });
        let result = self.request("initialize", params).await?;

        let protocol_version = result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned();
        let server_info = result.get("serverInfo");
        let server_name = server_info
            .and_then(|i| i.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned();
        let server_version = server_info
            .and_then(|i| i.get("version"))
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned();
        let capabilities = result.get("capabilities").cloned().unwrap_or(Value::Null);

        self.notify("notifications/initialized", json!({})).await?;
        self.inner.initialized.store(true, Ordering::Relaxed);

        Ok(InitializeResult {
            protocol_version,
            server_name,
            server_version,
            capabilities,
        })
    }

    /// List the server's tools (`tools/list`).
    pub async fn list_tools(&self) -> Result<Vec<McpToolInfo>> {
        self.ensure_initialized()?;
        let result = self.request("tools/list", json!({})).await?;
        let tools = result
            .get("tools")
            .and_then(Value::as_array)
            .ok_or_else(|| tool_err("`tools/list` result missing `tools` array"))?;
        tools
            .iter()
            .map(|t| {
                let name = t
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| tool_err("`tools/list` entry missing `name`"))?;
                Ok(McpToolInfo {
                    name: name.to_owned(),
                    description: t
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                    input_schema: match t.get("inputSchema") {
                        Some(schema) => schema.clone(),
                        None => {
                            // The spec requires inputSchema; synthesize a
                            // permissive schema rather than drop the tool,
                            // but make the fabrication visible.
                            tracing::warn!(
                                tool = name,
                                "mcp: `tools/list` entry omitted `inputSchema`; \
                                 defaulting to a permissive object schema"
                            );
                            json!({"type": "object"})
                        }
                    },
                })
            })
            .collect()
    }

    /// Call a tool (`tools/call`).
    ///
    /// On success returns the concatenated `text` content items as a
    /// [`Value::String`] (the common case), or the raw result object if the
    /// server returned no text content. A result with `isError: true` maps to
    /// [`RustyError::Tool`].
    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value> {
        self.ensure_initialized()?;
        let result = self
            .request("tools/call", json!({"name": name, "arguments": arguments}))
            .await?;

        let text = extract_text_content(&result);
        let is_error = result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if is_error {
            let detail = text.unwrap_or_else(|| result.to_string());
            return Err(tool_err(format!("tool `{name}` reported error: {detail}")));
        }
        match text {
            Some(t) => Ok(Value::String(t)),
            None => Ok(result),
        }
    }

    /// List the server's tools and wrap each as a Rusty Core [`Tool`],
    /// ready for [`crate::tool::ToolRegistry::register_shared`].
    pub async fn into_tools(&self) -> Result<Vec<Arc<dyn Tool>>> {
        let infos = self.list_tools().await?;
        Ok(infos
            .into_iter()
            .map(|info| Arc::new(McpToolAdapter::new(self.clone(), info)) as Arc<dyn Tool>)
            .collect())
    }

    /// Cleanly shut the client down: stop the reader task, fail pending
    /// requests, and kill the child process (for stdio clients). Idempotent.
    pub async fn shutdown(&self) -> Result<()> {
        self.inner.closed.store(true, Ordering::Relaxed);
        if let Ok(mut guard) = self.inner.reader_handle.lock() {
            if let Some(handle) = guard.take() {
                handle.abort();
            }
        }
        if let Ok(mut guard) = self.inner.child.lock() {
            if let Some(mut child) = guard.take() {
                let _ = child.start_kill();
            }
        }
        self.inner.pending.lock().await.clear();
        Ok(())
    }
}

/// Extract concatenated `text` content items from a `tools/call` result.
fn extract_text_content(result: &Value) -> Option<String> {
    let content = result.get("content")?.as_array()?;
    let texts: Vec<&str> = content
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|item| item.get("text").and_then(Value::as_str))
        .collect();
    if texts.is_empty() {
        None
    } else {
        Some(texts.join("\n"))
    }
}

// ---------------------------------------------------------------------------
// Stdio transport
// ---------------------------------------------------------------------------

/// Factory for MCP clients backed by a child-process stdio transport.
pub struct McpStdioClient;

impl McpStdioClient {
    /// Spawn `command args...` as a child process with piped stdin/stdout
    /// (stderr is discarded) and return an [`McpClient`] connected to it
    /// using newline-delimited framing, per the MCP stdio transport.
    ///
    /// The child is killed on [`McpClient::shutdown`] and when the last
    /// client handle drops (`kill_on_drop`).
    pub fn spawn<S: AsRef<str>>(command: S, args: &[S]) -> Result<McpClient> {
        let mut cmd = Command::new(command.as_ref());
        cmd.args(args.iter().map(AsRef::as_ref))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .map_err(|e| tool_err(format!("failed to spawn `{}`: {e}", command.as_ref())))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| tool_err("child stdout was not piped"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| tool_err("child stdin was not piped"))?;
        Ok(McpClient::connect_inner(
            stdout,
            stdin,
            Framing::NewlineDelimited,
            Some(child),
        ))
    }
}

// ---------------------------------------------------------------------------
// In-process MCP bridge
// ---------------------------------------------------------------------------

use crate::record::Effect;
use crate::tool::ToolRegistry;

/// Error raised when a tool is refused by the in-process MCP bridge at mount
/// time because its effect class is not in the allowed set.
#[derive(Debug, Clone, PartialEq)]
pub struct InProcessMountError {
    pub tool_name: String,
    pub effect: Effect,
    pub allowed: Vec<Effect>,
}

impl std::fmt::Display for InProcessMountError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "in-process MCP bridge refused tool `{}`: effect {:?} not in allowed set {:?}",
            self.tool_name, self.effect, self.allowed
        )
    }
}

impl std::error::Error for InProcessMountError {}

/// An in-process MCP server that exposes native Rusty [`Tool`]s over a
/// memory-based transport.
///
/// The bridge creates a paired [`McpClient`] and server task: from the
/// client's perspective the tools look exactly like an external MCP server,
/// but no serialization to bytes occurs and the server dispatches directly
/// to the native [`Tool::call`] implementation.
///
/// # Mount-time validation
///
/// By default only [`Effect::Pure`] and [`Effect::ReadOnly`] tools are
/// accepted. Use [`InProcessMcpBridge::with_allowed_effects`] to override.
/// Mounting refuses any tool whose effect is outside the allowed set with a
/// typed [`InProcessMountError`] (AC 3).
///
/// # Example
///
/// ```ignore
/// use rusty_agent_runtime::mcp::InProcessMcpBridge;
/// use rusty_agent_runtime::tool::ToolRegistry;
/// use std::sync::Arc;
///
/// let bridge = InProcessMcpBridge::new(Arc::new(registry)).unwrap();
/// let client = bridge.client();
/// let tools = client.into_tools().await.unwrap();
/// ```
#[derive(Clone, Debug)]
pub struct InProcessMcpBridge {
    registry: Arc<ToolRegistry>,
    allowed_effects: Vec<Effect>,
    server_name: String,
}

impl InProcessMcpBridge {
    /// Create a new bridge over `registry`.
    ///
    /// Defaults to accepting only [`Effect::Pure`] and [`Effect::ReadOnly`].
    /// Returns an error on the first tool whose effect is not allowed.
    pub fn new(registry: Arc<ToolRegistry>) -> Self {
        Self {
            registry,
            allowed_effects: vec![Effect::Pure, Effect::ReadOnly],
            server_name: "rusty-in-process".to_owned(),
        }
    }

    /// Set the effect classes allowed for in-process mounting.
    pub fn with_allowed_effects(mut self, effects: Vec<Effect>) -> Self {
        self.allowed_effects = effects;
        self
    }

    /// Set the server name reported in `initialize`.
    pub fn with_server_name(mut self, name: impl Into<String>) -> Self {
        self.server_name = name.into();
        self
    }

    /// Create an [`McpClient`] connected to this bridge over an in-memory
    /// transport.
    ///
    /// The returned client is initialized and ready for `list_tools` /
    /// `call_tool`. The server task runs in the background until the client
    /// (and all clones) drop.
    pub fn client(&self) -> std::result::Result<McpClient, InProcessMountError> {
        self.validate()?;
        let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);
        let (client_read, client_write) = tokio::io::split(client_stream);
        let (server_read, server_write) = tokio::io::split(server_stream);

        let registry = Arc::clone(&self.registry);
        let server_name = self.server_name.clone();
        tokio::spawn(async move {
            let _ = server_loop(registry, server_read, server_write, server_name).await;
        });

        let client = McpClient::connect(client_read, client_write);
        Ok(client)
    }

    fn validate(&self) -> std::result::Result<(), InProcessMountError> {
        for tool in self.registry.tools() {
            if !self.allowed_effects.contains(&tool.effect()) {
                return Err(InProcessMountError {
                    tool_name: tool.name().to_owned(),
                    effect: tool.effect(),
                    allowed: self.allowed_effects.clone(),
                });
            }
        }
        Ok(())
    }
}

/// Background server loop: reads JSON-RPC requests from `reader`, dispatches
/// to native tools, and writes responses to `writer`.
async fn server_loop<R, W>(
    registry: Arc<ToolRegistry>,
    reader: R,
    mut writer: W,
    server_name: String,
) -> io::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let mut reader = BufReader::new(reader);
    let framing = Framing::NewlineDelimited;

    loop {
        let request = match read_framed(&mut reader, framing).await? {
            Some(v) => v,
            None => break, // EOF
        };

        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();

        let response = match method.as_str() {
            "initialize" => {
                let result = json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "serverInfo": { "name": server_name, "version": env!("CARGO_PKG_VERSION") },
                    "capabilities": {}
                });
                json_response(&id, result)
            }
            "notifications/initialized" => {
                // No response for notifications
                continue;
            }
            "tools/list" => {
                let tools: Vec<Value> = registry
                    .tools()
                    .map(|tool| {
                        json!({
                            "name": tool.name(),
                            "description": tool.description(),
                            "inputSchema": tool.parameters_schema(),
                        })
                    })
                    .collect();
                json_response(&id, json!({ "tools": tools }))
            }
            "tools/call" => {
                let params = request.get("params").cloned().unwrap_or(Value::Null);
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);

                match registry.get(&name) {
                    Some(tool) => match tool.call(arguments).await {
                        Ok(value) => {
                            let text = match value {
                                Value::String(s) => s,
                                other => other.to_string(),
                            };
                            json_response(
                                &id,
                                json!({
                                    "content": [{ "type": "text", "text": text }],
                                    "isError": false,
                                }),
                            )
                        }
                        Err(e) => {
                            let msg = e.to_string();
                            json_response(
                                &id,
                                json!({
                                    "content": [{ "type": "text", "text": msg }],
                                    "isError": true,
                                }),
                            )
                        }
                    },
                    None => json_response(
                        &id,
                        json!({
                            "content": [{
                                "type": "text",
                                "text": format!("unknown tool: {name}")
                            }],
                            "isError": true,
                        }),
                    ),
                }
            }
            _ => json_error(&id, -32601, format!("method not found: {method}")),
        };

        write_framed(&mut writer, framing, &response).await?;
    }

    Ok(())
}

fn json_response(id: &Value, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
}

fn json_error(id: &Value, code: i64, message: String) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}

// ---------------------------------------------------------------------------
// Tool adapter
// ---------------------------------------------------------------------------

/// Wraps one MCP tool as a Rusty Core [`Tool`].
///
/// `name` / `description` / `parameters_schema` come from the server's
/// `tools/list` metadata; [`Tool::call`] issues `tools/call` and extracts the
/// text content.
pub struct McpToolAdapter {
    client: McpClient,
    info: McpToolInfo,
}

impl McpToolAdapter {
    /// An adapter for `info` that dispatches through `client`.
    pub fn new(client: McpClient, info: McpToolInfo) -> Self {
        Self { client, info }
    }
}

#[async_trait]
impl Tool for McpToolAdapter {
    fn name(&self) -> &str {
        &self.info.name
    }

    fn description(&self) -> &str {
        &self.info.description
    }

    fn parameters_schema(&self) -> Value {
        self.info.input_schema.clone()
    }

    async fn call(&self, args: Value) -> Result<Value> {
        self.client.call_tool(&self.info.name, args).await
    }
}

// ---------------------------------------------------------------------------
// Journaled MCP tools (R0.9 Rusty Capsules, wave 4)
// ---------------------------------------------------------------------------

/// The deterministic identity of one journaled MCP tool call.
///
/// Derived — never minted — over `(scope, tool name, canonical arguments)`
/// through the shared [`crate::effects::derive_effect_id`] construction,
/// hashing the canonical [`crate::replay::tool_call_request`] payload so
/// the id commits to exactly what the journal records. The key doubles as
/// the replay lookup identity: a recovering run re-derives the id of the
/// call it was about to issue, and [`JournaledMcpTool::replaying`] serves
/// the journaled response instead of re-issuing the call.
pub fn mcp_tool_effect_id(
    scope: &str,
    tool_name: &str,
    arguments: &Value,
) -> crate::effects::EffectId {
    let request = crate::replay::tool_call_request(tool_name, arguments);
    let input_hash = crate::record::PayloadRef::inline(request)
        .content_hash()
        .expect("a serde_json::Value always serializes");
    crate::effects::derive_effect_id(scope, &format!("mcp_tool:{tool_name}"), &input_hash, None)
}

/// An MCP tool that leaves evidence (R0.9 wave 4): the durable half of the
/// MCP client bridge.
///
/// Live ([`JournaledMcpTool::new`]), every call is dispatched through the
/// wrapped [`McpClient`] and journaled as a
/// [`RunEventKind::ToolCall`](crate::record::RunEventKind::ToolCall)
/// event in the canonical [`crate::replay::tool_call_request`] shape — the
/// same shape [`crate::replay::RecordingTool`] writes, so journals produced
/// here are exact-replay servable. The call's identity is
/// [`mcp_tool_effect_id`], reported through [`Tool::idempotency_key`].
///
/// Replaying ([`JournaledMcpTool::replaying`]), the tool holds **no client
/// at all** — not a disconnected one, none — so a replayed run cannot
/// respawn the stdio server (or reopen any transport) by construction; the
/// type makes the zero-outbound guarantee rather than promising it. Calls
/// are served from the recorded journal's [`crate::replay::ReplaySource`]
/// by canonical request hash and re-journaled into the replay run, the
/// [`crate::replay::ReplayingTool`] precedent.
///
/// The wrapper changes the evidence posture, not the effect class: an MCP
/// call is arbitrary remote work, so [`Tool::effect`] stays the adapter's
/// [`crate::record::Effect::NonIdempotent`] default.
pub struct JournaledMcpTool {
    info: McpToolInfo,
    /// The live transport; `None` in replay mode (see the type docs).
    client: Option<McpClient>,
    /// The recorded journal's serving cursor; `Some` in replay mode.
    source: Option<crate::replay::ReplaySource>,
    journal: crate::journal::Journal,
    parent: String,
    scope: String,
}

impl JournaledMcpTool {
    /// The live half: dispatch `info`'s tool through `client`, journaling
    /// every call into `journal` under causal parent `parent` (the invoking
    /// node's node-input event id, delivered via
    /// [`crate::journal::PARENT_EVENT_KEY`]). `scope` names the run scope
    /// the effect id derives under — conventionally the run id.
    pub fn new(
        client: McpClient,
        info: McpToolInfo,
        scope: impl Into<String>,
        journal: crate::journal::Journal,
        parent: impl Into<String>,
    ) -> Self {
        Self {
            info,
            client: Some(client),
            source: None,
            journal,
            parent: parent.into(),
            scope: scope.into(),
        }
    }

    /// The replaying half: serve calls from `source` (built over the
    /// recorded run's verified snapshot), re-journaling into the replay
    /// run's `journal` under `parent`. No client is taken — a replayed run
    /// never respawns the server.
    pub fn replaying(
        info: McpToolInfo,
        scope: impl Into<String>,
        source: crate::replay::ReplaySource,
        journal: crate::journal::Journal,
        parent: impl Into<String>,
    ) -> Self {
        Self {
            info,
            client: None,
            source: Some(source),
            journal,
            parent: parent.into(),
            scope: scope.into(),
        }
    }

    /// This call's derived identity ([`mcp_tool_effect_id`]).
    pub fn effect_id(&self, arguments: &Value) -> crate::effects::EffectId {
        mcp_tool_effect_id(&self.scope, &self.info.name, arguments)
    }
}

#[async_trait]
impl Tool for JournaledMcpTool {
    fn name(&self) -> &str {
        &self.info.name
    }

    fn description(&self) -> &str {
        &self.info.description
    }

    fn parameters_schema(&self) -> Value {
        self.info.input_schema.clone()
    }

    fn idempotency_key(&self, args: &Value) -> Option<String> {
        Some(self.effect_id(args).as_str().to_string())
    }

    async fn call(&self, args: Value) -> Result<Value> {
        let request = crate::replay::tool_call_request(&self.info.name, &args);
        match (&self.client, &self.source) {
            (Some(client), None) => {
                let result = client.call_tool(&self.info.name, args).await;
                // The journaled shape is RecordingTool's exactly: success
                // carries the verbatim response; failure carries
                // `{"error": msg}` under EventStatus::Error — so either way
                // the event is servable by exact replay.
                let mut draft = crate::journal::EventDraft::new(
                    crate::record::RunEventKind::ToolCall,
                    <Self as Tool>::effect(self),
                )
                .input(request)
                .parent(self.parent.clone());
                match &result {
                    Ok(value) => {
                        draft = draft.output(value.clone());
                    }
                    Err(error) => {
                        draft = draft
                            .output(json!({ "error": error.to_string() }))
                            .status(crate::record::EventStatus::Error);
                    }
                }
                self.journal.record(draft);
                result
            }
            (None, Some(source)) => {
                let served = source
                    .serve(crate::record::RunEventKind::ToolCall, &request)
                    .map_err(|e| tool_err(format!("replay serve failed: {e}")))?;
                served.rejournal(&self.journal, self.parent.clone());
                if served.event.status == crate::record::EventStatus::Error {
                    let message = served
                        .output
                        .as_ref()
                        .and_then(|value| value.get("error"))
                        .and_then(Value::as_str)
                        .unwrap_or("recorded tool call failed");
                    return Err(tool_err(message));
                }
                Ok(served.output.unwrap_or(Value::Null))
            }
            // The two constructors set exactly one half; this arm is
            // unreachable by construction.
            _ => Err(tool_err("journaled MCP tool is neither live nor replaying")),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolRegistry;
    use tokio::io::{DuplexStream, duplex};

    /// A scripted mock MCP server speaking the full handshake.
    async fn run_mock_server(stream: DuplexStream, framing: Framing) {
        let (read, mut write) = tokio::io::split(stream);
        let mut reader = BufReader::new(read);
        while let Ok(Some(msg)) = read_framed(&mut reader, framing).await {
            let id = msg.get("id").cloned();
            let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
            let response = match method {
                "notifications/initialized" => None,
                "initialize" => Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": MCP_PROTOCOL_VERSION,
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "mock-mcp", "version": "0.0.1"},
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
                                "name": "fail_rpc",
                                "description": "Fails at the JSON-RPC layer.",
                                "inputSchema": {"type": "object"}
                            },
                            {
                                "name": "error_tool",
                                "description": "Reports a tool-level error.",
                                "inputSchema": {"type": "object"}
                            },
                            {
                                "name": "slow",
                                "description": "Responds slowly.",
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
                        "fail_rpc" => Some(json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": {"code": -32602, "message": "invalid params: missing `widget_id`"}
                        })),
                        "slow" => {
                            tokio::time::sleep(Duration::from_millis(500)).await;
                            Some(json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": {"content": [{"type": "text", "text": "too late"}]}
                            }))
                        }
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
                write_framed(&mut write, framing, &resp)
                    .await
                    .expect("mock server write");
            }
        }
    }

    /// A client connected to a scripted mock server over an in-memory
    /// full-duplex transport.
    fn client_and_mock(framing: Framing) -> (McpClient, JoinHandle<()>) {
        let (client_stream, server_stream) = duplex(64 * 1024);
        let handle = tokio::spawn(run_mock_server(server_stream, framing));
        let (read, write) = tokio::io::split(client_stream);
        (
            McpClient::connect_with_framing(read, write, framing),
            handle,
        )
    }

    async fn initialized_client(framing: Framing) -> (McpClient, JoinHandle<()>) {
        let (client, handle) = client_and_mock(framing);
        client.initialize().await.expect("initialize");
        (client, handle)
    }

    #[tokio::test]
    async fn initialize_handshake_returns_server_info() {
        let (client, _mock) = client_and_mock(Framing::NewlineDelimited);
        assert!(!client.is_initialized());
        let info = client.initialize().await.expect("initialize");
        assert_eq!(info.protocol_version, MCP_PROTOCOL_VERSION);
        assert_eq!(info.server_name, "mock-mcp");
        assert_eq!(info.server_version, "0.0.1");
        assert!(info.capabilities.get("tools").is_some());
        assert!(client.is_initialized());
        client.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn requests_before_initialize_are_rejected() {
        let (client, _mock) = client_and_mock(Framing::NewlineDelimited);
        let err = client.list_tools().await.unwrap_err();
        assert!(matches!(err, RustyError::Tool(_)));
        assert!(err.to_string().contains("initialize"));
    }

    #[tokio::test]
    async fn tools_list_parses_metadata() {
        let (client, _mock) = initialized_client(Framing::NewlineDelimited).await;
        let tools = client.list_tools().await.expect("tools/list");
        assert_eq!(tools.len(), 4);
        let echo = tools.iter().find(|t| t.name == "echo").expect("echo tool");
        assert_eq!(echo.description, "Echoes text back.");
        assert_eq!(echo.input_schema["type"], json!("object"));
        assert!(echo.input_schema["properties"]["text"].is_object());
        assert_eq!(echo.input_schema["required"], json!(["text"]));
    }

    #[tokio::test]
    async fn tools_call_extracts_text_content() {
        let (client, _mock) = initialized_client(Framing::NewlineDelimited).await;
        let value = client
            .call_tool("echo", json!({"text": "hi"}))
            .await
            .expect("tools/call");
        assert_eq!(value, json!("hello from echo"));
    }

    #[tokio::test]
    async fn json_rpc_and_tool_errors_map_to_tool_variant() {
        let (client, _mock) = initialized_client(Framing::NewlineDelimited).await;

        // JSON-RPC-level error.
        let err = client.call_tool("fail_rpc", json!({})).await.unwrap_err();
        match err {
            RustyError::Tool(msg) => {
                assert!(msg.contains("-32602"), "got: {msg}");
                assert!(msg.contains("invalid params"), "got: {msg}");
            }
            other => panic!("expected Tool error, got: {other}"),
        }

        // Tool-level error (`isError: true`).
        let err = client.call_tool("error_tool", json!({})).await.unwrap_err();
        assert!(matches!(err, RustyError::Tool(_)));
        assert!(err.to_string().contains("invalid widget id"));
    }

    #[tokio::test]
    async fn request_timeout_aborts_pending_call() {
        let (client, _mock) = client_and_mock(Framing::NewlineDelimited);
        client.set_request_timeout(Duration::from_millis(50));
        client.initialize().await.expect("initialize");
        let err = client.call_tool("slow", json!({})).await.unwrap_err();
        assert!(matches!(err, RustyError::Tool(_)));
        assert!(err.to_string().contains("timed out"), "got: {err}");
    }

    #[tokio::test]
    async fn into_tools_produces_registry_ready_adapters() {
        let (client, _mock) = initialized_client(Framing::NewlineDelimited).await;
        let tools = client.into_tools().await.expect("into_tools");
        assert_eq!(tools.len(), 4);

        let mut registry = ToolRegistry::new();
        for tool in tools {
            registry.register_shared(tool);
        }
        assert!(registry.contains("echo"));
        assert!(registry.contains("fail_rpc"));

        let echo = registry.get("echo").expect("echo registered");
        assert_eq!(echo.name(), "echo");
        assert_eq!(echo.description(), "Echoes text back.");
        assert!(echo.parameters_schema()["properties"]["text"].is_object());

        let out = echo
            .call(json!({"text": "yo"}))
            .await
            .expect("adapter call");
        assert_eq!(out, json!("hello from echo"));

        // Registry schemas remain OpenAI-shaped with the MCP tool inside.
        let schemas = registry.schemas();
        assert!(
            schemas
                .iter()
                .any(|s| s["function"]["name"] == json!("echo"))
        );
    }

    #[tokio::test]
    async fn content_length_framing_roundtrip() {
        let (client, _mock) = initialized_client(Framing::ContentLength).await;
        assert_eq!(client.framing(), Framing::ContentLength);
        let value = client
            .call_tool("echo", json!({"text": "framed"}))
            .await
            .expect("tools/call over Content-Length framing");
        assert_eq!(value, json!("hello from echo"));
        client.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn content_length_cap_rejects_giant_frame_before_allocating() {
        // A peer-declared 4 GB frame must be rejected, not allocated.
        let bytes = b"Content-Length: 4000000000\r\n\r\n";
        let mut reader = BufReader::new(&bytes[..]);
        let err = read_framed(&mut reader, Framing::ContentLength)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("frame cap"), "got: {err}");
    }

    #[tokio::test]
    async fn newline_cap_bounds_unterminated_lines() {
        // No newline anywhere: the buffer must stop at the cap.
        let bytes = vec![b'a'; MAX_FRAME_BYTES + 8];
        let mut reader = BufReader::new(&bytes[..]);
        let err = read_framed(&mut reader, Framing::NewlineDelimited)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("frame cap"), "got: {err}");
    }

    #[tokio::test]
    async fn malformed_newline_frame_is_dropped_and_session_survives() {
        let (client_stream, mut server) = duplex(64 * 1024);
        let server_task = tokio::spawn(async move {
            // A garbage line first: must not brick the client session.
            server
                .write_all(b"this is not json at all\n")
                .await
                .expect("write garbage");
            let (read, mut write) = tokio::io::split(server);
            let mut reader = BufReader::new(read);
            // Then a normal initialize handshake.
            let msg = read_framed(&mut reader, Framing::NewlineDelimited)
                .await
                .expect("read initialize")
                .expect("initialize frame");
            let id = msg.get("id").cloned();
            write_framed(
                &mut write,
                Framing::NewlineDelimited,
                &json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": MCP_PROTOCOL_VERSION,
                        "capabilities": {},
                        "serverInfo": {"name": "resync-mock", "version": "0"},
                    }
                }),
            )
            .await
            .expect("write initialize result");
            // Drain the initialized notification so the pipe stays open.
            let _ = read_framed(&mut reader, Framing::NewlineDelimited).await;
        });

        let (read, write) = tokio::io::split(client_stream);
        let client = McpClient::connect(read, write);
        let info = client
            .initialize()
            .await
            .expect("initialize must survive a leading malformed frame");
        assert_eq!(info.server_name, "resync-mock");
        client.shutdown().await.expect("shutdown");
        server_task.await.expect("server task");
    }

    #[tokio::test]
    async fn stdio_spawn_of_missing_command_errors() {
        let args: [&str; 0] = [];
        let result = McpStdioClient::spawn("rusty-no-such-mcp-server-zzz", &args);
        match result {
            Err(err) => {
                assert!(matches!(err, RustyError::Tool(_)));
                assert!(err.to_string().contains("failed to spawn"));
            }
            Ok(_) => panic!("spawning a nonexistent command should fail"),
        }
    }
}
