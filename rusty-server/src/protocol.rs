//! The schema-defined gateway protocol (EP-04-S01): frame envelopes, the
//! connect handshake, and the hello-ok contract.
//!
//! This module is the wire's single definition. Every type derives
//! `JsonSchema`; the golden schemas under `tests/golden/` are generated
//! from these types and feed the TypeScript codegen in `sdks/wire`, so
//! server validators and client types can never drift apart silently —
//! a schema change fails the drift test until the goldens are updated,
//! and the goldens fail the wire package's freshness test until the
//! clients are regenerated.
//!
//! Envelope discipline: every frame is a closed object (unknown fields
//! are refused, not ignored) inside one discriminated union keyed by
//! `type`. Requests pair with responses by `id`; events carry a
//! monotonic per-connection `seq` so a client can detect a gap and
//! re-snapshot instead of trusting a stream it cannot prove complete.
//! The connection lifecycle (who serves these frames, over which
//! transport) is the adapter waves' concern — the contract lands first.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The protocol generation this server speaks. Clients send the range
/// they understand in [`ConnectParams`]; the server answers with the one
/// generation it chose in [`HelloOk::protocol`].
pub const PROTOCOL_VERSION: u32 = 1;

/// The client's opening message: the protocol range it speaks, who it
/// is, and how it authenticates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConnectParams {
    /// Oldest protocol generation the client still understands.
    pub min_protocol: u32,
    /// Newest protocol generation the client understands.
    pub max_protocol: u32,
    /// Who is connecting.
    pub client: ClientIdentity,
    /// Scopes the client asks for; the grant comes back in
    /// [`HelloOk::auth`] and may be narrower.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes: Option<Vec<String>>,
    /// Credentials. Absent on an open-mode server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<ConnectAuth>,
}

/// The connecting client's self-description.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ClientIdentity {
    /// Stable client identifier (e.g. `studio`, `sdk-ts`).
    pub id: String,
    /// Client build version.
    pub version: String,
    /// Platform the client runs on (e.g. `web`, `node`, `python`).
    pub platform: String,
    /// Human-readable name, when the client has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

/// Connect credentials. Mirrors the HTTP surface's `X-Api-Key` scheme —
/// the gateway grants what the same key would be granted over REST.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConnectAuth {
    /// API key, when the server is not in open mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

/// The server's acceptance of a connect: the chosen protocol, what this
/// server offers, the initial state snapshot, the granted authority, and
/// the budgets the connection must respect.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HelloOk {
    /// The protocol generation the server chose from the client's range.
    pub protocol: u32,
    /// Which server answered.
    pub server: ServerIdentity,
    /// What this server can do, as data: the methods a request may name
    /// and the events this connection may observe.
    pub features: Features,
    /// The initial state: everything after this arrives as sequenced
    /// events, so snapshot + gap-free `seq` is a complete view.
    pub snapshot: ConnectSnapshot,
    /// The authority this connection actually holds (never assumed from
    /// what was requested).
    pub auth: GrantedAuth,
    /// Budgets the connection must respect.
    pub policy: PolicyBudgets,
}

/// The answering server's identity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ServerIdentity {
    /// Server build version.
    pub version: String,
    /// This connection's server-minted id; correlates gateway log lines
    /// with client reports.
    pub conn_id: String,
}

/// The server's capability listing, as data a client can branch on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Features {
    /// Request methods this server serves.
    pub methods: Vec<String>,
    /// Event kinds this connection may observe.
    pub events: Vec<String>,
}

/// The state a connection starts from. Deliberately the same truth the
/// REST surface serves at `/info`: the registered graphs. Everything
/// richer (threads, runs) stays request/response until the sequenced
/// event catalog carries it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConnectSnapshot {
    /// Version of the state this snapshot describes; event frames carry
    /// the `seq` that continues from it.
    pub state_version: u64,
    /// The graphs registered on this server.
    pub graphs: Vec<GraphSummary>,
}

/// One registered graph, as the snapshot lists it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GraphSummary {
    /// Graph name (`POST /threads` binds to it).
    pub name: String,
    /// Declared state channels.
    pub channels: Vec<String>,
}

/// What the connection is allowed to do — the grant, not the request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GrantedAuth {
    /// The role the credentials resolved to.
    pub role: String,
    /// The scopes actually granted.
    pub scopes: Vec<String>,
}

/// Connection budgets. Declared, not discovered: a client that knows the
/// ceilings validates before sending instead of learning them from a
/// dropped connection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PolicyBudgets {
    /// Largest frame the server accepts, in bytes.
    pub max_payload_bytes: u64,
    /// Most unacknowledged bytes the server buffers before it closes the
    /// connection as stalled.
    pub max_buffered_bytes: u64,
    /// Heartbeat interval; a client that misses several ticks should
    /// reconnect and re-snapshot.
    pub tick_interval_ms: u64,
}

/// The structured error carried by failed responses and refused
/// connects.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ErrorShape {
    /// Stable machine-readable code.
    pub code: String,
    /// Human-readable description.
    pub message: String,
    /// Error-specific detail payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    /// Whether retrying the same frame can succeed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
    /// Server-suggested floor before a retry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
}

/// A client request; the named `method`'s own schema governs `params`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RequestFrame {
    /// Client-minted id the response echoes.
    pub id: String,
    /// Method name, from [`Features::methods`].
    pub method: String,
    /// Method arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// The server's answer to one request, paired by `id`. Exactly one of
/// `payload` (when `ok`) and `error` (when not) is meaningful.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResponseFrame {
    /// The request id this answers.
    pub id: String,
    /// Whether the request succeeded.
    pub ok: bool,
    /// The result, when `ok`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
    /// The failure, when not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorShape>,
}

/// A server-pushed event. `seq` is monotonic per connection: a skip
/// means frames were lost and the client must re-snapshot rather than
/// render a stream it cannot prove complete.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EventFrame {
    /// Event kind, from [`Features::events`].
    pub event: String,
    /// Event payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
    /// Monotonic per-connection sequence number.
    pub seq: u64,
}

/// Every gateway frame, as one closed union keyed by `type`. A frame
/// that parses is one of exactly these shapes; a frame that does not is
/// refused, never partially read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum GatewayFrame {
    /// Client → server request.
    Req(RequestFrame),
    /// Server → client answer.
    Res(ResponseFrame),
    /// Server → client sequenced event.
    Event(EventFrame),
    /// Server → client connect acceptance.
    HelloOk(HelloOk),
}
