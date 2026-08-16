//! The connector plane: lifecycle-managed providers of tools.
//!
//! A connector is declared as a content-addressed [`ConnectorManifest`]
//! (identity is the SHA-256 of its canonical serialization — the hash *is*
//! the registration key), instantiated per tenant as a
//! [`ConnectorInstance`] whose credentials are injected from a
//! [`CredentialBroker`] seam at creation time, and driven through an
//! explicit lifecycle (`pending → connecting → healthy | degraded | failed`,
//! plus `disabled`). A healthy instance exposes its tools as a derived
//! [`crate::tool::ToolCapability`] catalog pinned by
//! [`CatalogGeneration`]: consumers pin a generation number and content
//! hash, never "latest".
//!
//! Two providers ship in this slice:
//!
//! - [`McpStdioProvider`], which wraps the existing
//!   [`crate::mcp`] stdio client: it spawns the manifest's command with a
//!   scrubbed environment (only the declared env allowlist passes through),
//!   performs the MCP handshake, and namespaces every discovered tool as
//!   `<connector>/<tool>`.
//! - [`HttpSearchProvider`], the bounded web-search contract: a query in,
//!   ranked [`SearchHit`]s out, with byte and count ceilings enforced on
//!   both sides and the HTTP exchange behind the [`HttpTransport`] seam so
//!   tests drive a fake and real wiring is a server concern. Search is a
//!   provider in its own right — never a hidden network call inside a
//!   built-in tool.
//! - [`HttpApiProvider`], the generic REST/GraphQL contract: a manifest
//!   declares a base URL, a slot-referencing auth style, and an operations
//!   list, and each valid operation derives one catalog tool
//!   (`<connector>/<operation>`) with an explicit effect classification.
//!   This is the foundation service packs are declared against.
//!
//! All timestamps are logical: every state transition takes `now_ms` from
//! the caller instead of reading a wall clock, so health sweeps and
//! lifecycle history stay deterministic under replay and test.
//!
//! The persistent stores, credential vaults, and Studio surfaces that pin
//! these contracts live in `rusty-agent-server` / Studio; these are the
//! pure contracts both sides agree on.

use crate::error::RustyError;

pub mod credential;
pub mod http_api;
pub mod instance;
pub mod manifest;
pub mod provider;
pub mod registry;

pub use credential::{CredentialBroker, CredentialHandle, InMemoryCredentialBroker};
pub use http_api::{
    derive_idempotency_key, HttpApiProvider, HttpApiRequest, HttpApiTool, HttpApiTransport,
    DEFAULT_HTTP_API_TIMEOUT, IDEMPOTENCY_KEY_DOMAIN, MAX_HTTP_API_ERROR_BODY_BYTES,
    MAX_HTTP_API_REQUEST_BYTES,
};
pub use instance::{
    CatalogGeneration, CatalogPin, ConnectorInstance, LifecycleState, DEFAULT_DEGRADE_AFTER_FAILURES,
    MAX_INSTANCE_ERROR_BYTES, MAX_TENANT_ID_LEN,
};
pub use manifest::{
    ConnectorManifest, CredentialSlot, HttpApiAuth, HttpApiOperation, HttpApiSpec, HttpMethod,
    HttpSearchSpec, McpStdioSpec, OperationBody, OperationEffect, ProviderKind, ResponseExtraction,
    SearchAuth,
};
pub use provider::{
    default_provider, ConnectorProvider, ConnectorSearchTool, HttpRequest, HttpResponse,
    HttpSearchProvider, HttpTransport, McpSession, McpStdioProvider, ProviderSession, SearchHit,
    SearchRequest, DEFAULT_SEARCH_RESULT_COUNT, DEFAULT_SEARCH_TIMEOUT, MAX_SEARCH_QUERY_BYTES,
    MAX_SEARCH_RESPONSE_BYTES, MAX_SEARCH_RESULT_COUNT,
};
pub use registry::{ConnectorRegistry, SweepOutcome};

/// Build a [`RustyError::Tool`] with a `connector:` context prefix, the
/// same convention `mcp:` uses for the MCP client.
pub(crate) fn conn_err(msg: impl Into<String>) -> RustyError {
    RustyError::Tool(format!("connector: {}", msg.into()))
}

/// SHA-256 of the canonical JSON form of `value` — object keys sorted
/// recursively — the digest convention every connector content address
/// (manifest hashes, catalog generation hashes) shares with the record
/// model's manifest pins.
pub(crate) fn canonical_json_hash(value: &serde_json::Value) -> String {
    // Serializing a `Value` is infallible in practice (its maps always have
    // string keys); see `PayloadRef::content_hash` for the same argument.
    let bytes = serde_json::to_vec(&crate::record::canonicalize_value(value))
        .expect("a serde_json::Value always serializes");
    crate::record::sha256_hex(&bytes)
}
