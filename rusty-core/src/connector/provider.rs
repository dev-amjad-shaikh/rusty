//! Connector providers: the MCP stdio and HTTP search implementations.
//!
//! A [`ConnectorProvider`] turns a validated manifest plus its resolved
//! credentials into a live [`ProviderSession`]; the session answers
//! catalog requests (initial derivation and refresh) and shuts down
//! cleanly. The registry drives both; providers never see tenant state or
//! lifecycle — they connect, list, and die when told.
//!
//! Search is a provider in its own right (the design doc is explicit: web
//! search is a connector, never a hidden network call inside a built-in
//! tool). The HTTP exchange sits behind the [`HttpTransport`] seam, so
//! tests drive a scripted fake and real reqwest wiring is a server-slice
//! concern.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::process::{Child, Command};

use super::credential::CredentialHandle;
use super::manifest::{ConnectorManifest, ProviderKind, SearchAuth};
use super::conn_err;
use crate::error::Result;
use crate::mcp::{McpClient, McpToolInfo};
use crate::record::Effect;
use crate::tool::{Tool, ToolCapability, MAX_TOOL_DESCRIPTION_BYTES, MAX_TOOL_SCHEMA_BYTES};

/// Maximum length of one derived catalog tool name (`<connector>/<tool>`).
const MAX_DERIVED_TOOL_NAME_LEN: usize = 128;

// ---------------------------------------------------------------------------
// Provider contracts
// ---------------------------------------------------------------------------

/// A live connection to one connector instance's provider.
///
/// Sessions are per-instance and owned by the registry entry. `catalog`
/// serves both the initial derivation (right after connect) and every
/// refresh, so a provider whose tool set changes across calls drives
/// catalog generations through the same door.
#[async_trait]
pub trait ProviderSession: std::fmt::Debug + Send + Sync {
    /// The connector id this session serves.
    fn connector_id(&self) -> &str;

    /// Derive the current tool catalog, namespaced `<connector>/<tool>`.
    /// The first call performs any handshake the provider needs.
    async fn catalog(&mut self) -> Result<Vec<ToolCapability>>;

    /// Tear the session down (kill the child, close the transport).
    /// Dropping a session without shutdown is safe — spawned children are
    /// `kill_on_drop` — but shutdown reports teardown failures instead of
    /// swallowing them.
    async fn shutdown(self: Box<Self>) -> Result<()>;
}

/// The provider seam the registry connects instances through.
///
/// `credentials` are the handles resolved for the manifest's declared
/// slots, owned by the registry entry; providers read secrets at the
/// moment of use and store none.
#[async_trait]
pub trait ConnectorProvider: std::fmt::Debug + Send + Sync {
    /// Establish a session for `manifest`. A provider given a manifest of
    /// the wrong kind fails here rather than guessing.
    async fn connect(
        &self,
        manifest: &ConnectorManifest,
        credentials: &[CredentialHandle],
    ) -> Result<Box<dyn ProviderSession>>;
}

/// The provider a manifest gets when the caller does not supply one:
/// [`McpStdioProvider`] for `mcp-stdio`, a manifest-configured
/// [`HttpSearchProvider`] for `http-search`.
pub fn default_provider(manifest: &ConnectorManifest) -> Result<Arc<dyn ConnectorProvider>> {
    match &manifest.provider {
        ProviderKind::McpStdio(_) => Ok(Arc::new(McpStdioProvider)),
        ProviderKind::HttpSearch(_) => Ok(Arc::new(HttpSearchProvider::from_manifest(manifest)?)),
    }
}

// ---------------------------------------------------------------------------
// MCP stdio provider
// ---------------------------------------------------------------------------

/// The MCP stdio provider: spawns the manifest's command as a child
/// process with a scrubbed environment and derives the catalog from the
/// server's `tools/list`.
#[derive(Debug, Default)]
pub struct McpStdioProvider;

impl McpStdioProvider {
    /// Handshake (if needed) with `client`, list its tools, and map them
    /// into a namespaced, validated [`ToolCapability`] catalog.
    ///
    /// Exposed separately from the spawn path so any `McpClient`
    /// transport — including the in-memory duplex fakes the MCP tests
    /// use — drives the same derivation.
    pub async fn catalog_from_client(
        connector_id: &str,
        client: &McpClient,
    ) -> Result<Vec<ToolCapability>> {
        if !client.is_initialized() {
            client.initialize().await?;
        }
        let infos = client.list_tools().await?;
        map_mcp_tools(connector_id, infos)
    }
}

#[async_trait]
impl ConnectorProvider for McpStdioProvider {
    async fn connect(
        &self,
        manifest: &ConnectorManifest,
        _credentials: &[CredentialHandle],
    ) -> Result<Box<dyn ProviderSession>> {
        let spec = match &manifest.provider {
            ProviderKind::McpStdio(spec) => spec,
            ProviderKind::HttpSearch(_) => {
                return Err(conn_err(format!(
                    "manifest `{}` is http-search; McpStdioProvider cannot serve it",
                    manifest.id
                )))
            }
        };
        let (client, child) = spawn_stdio(spec)?;
        Ok(Box::new(McpSession {
            connector_id: manifest.id.clone(),
            client,
            child: Some(child),
        }))
    }
}

/// Spawn `spec.command` with the declared environment allowlist.
///
/// The child starts from an *empty* environment: only allowlisted names
/// cross from the host, so a manifest declares the entire env surface its
/// server may observe. The returned client uses newline-delimited framing
/// per the MCP stdio transport; the child is `kill_on_drop` and is also
/// killed explicitly on session shutdown.
fn spawn_stdio(spec: &super::manifest::McpStdioSpec) -> Result<(McpClient, Child)> {
    let mut command = Command::new(&spec.command);
    command
        .args(&spec.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .env_clear();
    for name in &spec.env_allowlist {
        if let Ok(value) = std::env::var(name) {
            command.env(name, value);
        }
    }
    let mut child = command
        .spawn()
        .map_err(|e| conn_err(format!("failed to spawn `{}`: {e}", spec.command)))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| conn_err("child stdout was not piped"))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| conn_err("child stdin was not piped"))?;
    Ok((McpClient::connect(stdout, stdin), child))
}

/// Map discovered MCP tools into the derived catalog, fail-closed.
///
/// Every tool is namespaced `<connector>/<tool>` and held to the same
/// contract [`ToolRegistry::capabilities`](crate::tool::ToolRegistry::capabilities)
/// enforces (name charset extended by the namespace separator `/`,
/// non-empty trimmed control-free description, bounded object schema): a
/// server advertising an invalid tool fails the whole derivation — and
/// therefore the connection — rather than advertising a catalog the
/// runtime executor would reject.
///
/// MCP calls are arbitrary remote work, so every derived capability is
/// [`Effect::NonIdempotent`], the `McpToolAdapter` precedent.
fn map_mcp_tools(connector_id: &str, infos: Vec<McpToolInfo>) -> Result<Vec<ToolCapability>> {
    let mut capabilities = Vec::with_capacity(infos.len());
    for info in infos {
        let name = format!("{connector_id}/{}", info.name);
        if name.len() > MAX_DERIVED_TOOL_NAME_LEN
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._:-/".contains(&b))
        {
            return Err(conn_err(format!(
                "MCP tool `{}` maps to invalid catalog name `{name}`",
                info.name
            )));
        }
        let description = info.description;
        if description.is_empty()
            || description != description.trim()
            || description.len() > MAX_TOOL_DESCRIPTION_BYTES
            || description.chars().any(char::is_control)
        {
            return Err(conn_err(format!(
                "MCP tool `{}` description must be non-empty, trimmed, control-free, and at most {MAX_TOOL_DESCRIPTION_BYTES} bytes",
                info.name
            )));
        }
        if !info.input_schema.is_object() {
            return Err(conn_err(format!(
                "MCP tool `{}` input schema must be a JSON object",
                info.name
            )));
        }
        let schema_bytes = serde_json::to_vec(&info.input_schema)
            .map_err(|e| conn_err(format!("MCP tool `{}` schema did not serialize: {e}", info.name)))?;
        if schema_bytes.len() > MAX_TOOL_SCHEMA_BYTES {
            return Err(conn_err(format!(
                "MCP tool `{}` schema exceeds {MAX_TOOL_SCHEMA_BYTES} bytes",
                info.name
            )));
        }
        capabilities.push(ToolCapability {
            name,
            description,
            parameters_schema: info.input_schema,
            effect: Effect::NonIdempotent,
        });
    }
    capabilities.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(capabilities)
}

/// An MCP provider session over a spawned child or an injected client.
pub struct McpSession {
    connector_id: String,
    client: McpClient,
    child: Option<Child>,
}

impl McpSession {
    /// A session over an already-connected client (any transport).
    ///
    /// The registry's spawn path produces sessions through
    /// [`ConnectorProvider::connect`]; this constructor is the seam for
    /// transports that are not child processes — and for tests driving a
    /// scripted in-memory server.
    pub fn from_client(connector_id: impl Into<String>, client: McpClient) -> Self {
        Self {
            connector_id: connector_id.into(),
            client,
            child: None,
        }
    }
}

impl std::fmt::Debug for McpSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpSession")
            .field("connector_id", &self.connector_id)
            .field("initialized", &self.client.is_initialized())
            .field("child", &self.child.as_ref().map(|c| c.id()))
            .finish()
    }
}

#[async_trait]
impl ProviderSession for McpSession {
    fn connector_id(&self) -> &str {
        &self.connector_id
    }

    async fn catalog(&mut self) -> Result<Vec<ToolCapability>> {
        McpStdioProvider::catalog_from_client(&self.connector_id, &self.client).await
    }

    async fn shutdown(self: Box<Self>) -> Result<()> {
        let this = *self;
        if let Some(mut child) = this.child {
            let _ = child.start_kill();
        }
        this.client.shutdown().await
    }
}

// ---------------------------------------------------------------------------
// HTTP search provider
// ---------------------------------------------------------------------------

/// Maximum size of a search query, in bytes.
pub const MAX_SEARCH_QUERY_BYTES: usize = 1024;

/// Maximum number of results one search call may return.
pub const MAX_SEARCH_RESULT_COUNT: usize = 20;

/// Default result count when the caller does not ask for one.
pub const DEFAULT_SEARCH_RESULT_COUNT: usize = 5;

/// Maximum size of a search response body, in bytes. Enforced before any
/// length-driven allocation or parsing.
pub const MAX_SEARCH_RESPONSE_BYTES: usize = 256 * 1024;

/// Per-field byte ceilings on one returned hit.
pub const MAX_SEARCH_TITLE_BYTES: usize = 512;
/// Per-field byte ceilings on one returned hit.
pub const MAX_SEARCH_URL_BYTES: usize = 2048;
/// Per-field byte ceilings on one returned hit.
pub const MAX_SEARCH_SNIPPET_BYTES: usize = 2048;

/// Default per-call timeout.
pub const DEFAULT_SEARCH_TIMEOUT: Duration = Duration::from_secs(10);

/// One outbound HTTP exchange, as the [`HttpTransport`] seam sees it.
#[derive(Debug, Clone)]
pub struct HttpRequest {
    /// The full URL to POST.
    pub url: String,
    /// Header name/value pairs (the auth header, when configured, is
    /// already resolved here — the transport never sees the slot name).
    pub headers: Vec<(String, String)>,
    /// The serialized JSON request body.
    pub body: Vec<u8>,
    /// The per-call timeout the transport must enforce.
    pub timeout: Duration,
}

/// One inbound HTTP reply.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    /// The status code.
    pub status: u16,
    /// The raw body bytes (bounded by [`MAX_SEARCH_RESPONSE_BYTES`] on
    /// the provider side, before parsing).
    pub body: Vec<u8>,
}

/// The HTTP seam. Tests drive a scripted fake; the server slice wires
/// reqwest behind the same trait.
#[async_trait]
pub trait HttpTransport: std::fmt::Debug + Send + Sync {
    /// POST `request.body` to `request.url` with the given headers,
    /// honoring `request.timeout`.
    async fn post(&self, request: HttpRequest) -> Result<HttpResponse>;
}

/// A validated search call: query in, ranked results out.
///
/// Construction validates the query (non-empty, trimmed, control-free,
/// bounded); `max_results` is clamped to `1..=MAX_SEARCH_RESULT_COUNT` by
/// construction, so provider code never re-checks its own inputs.
#[derive(Debug, Clone)]
pub struct SearchRequest {
    query: String,
    max_results: usize,
}

impl SearchRequest {
    /// A search for `query` returning the default number of results.
    pub fn new(query: impl Into<String>) -> Result<Self> {
        let query = query.into();
        if query.is_empty()
            || query != query.trim()
            || query.len() > MAX_SEARCH_QUERY_BYTES
            || query.chars().any(char::is_control)
        {
            return Err(conn_err(format!(
                "search query must be non-empty, trimmed, control-free, and at most {MAX_SEARCH_QUERY_BYTES} bytes"
            )));
        }
        Ok(Self {
            query,
            max_results: DEFAULT_SEARCH_RESULT_COUNT,
        })
    }

    /// Ask for `max_results` results, bounded by
    /// `1..=MAX_SEARCH_RESULT_COUNT`.
    pub fn with_max_results(mut self, max_results: usize) -> Result<Self> {
        if max_results == 0 || max_results > MAX_SEARCH_RESULT_COUNT {
            return Err(conn_err(format!(
                "max_results must be within 1..={MAX_SEARCH_RESULT_COUNT}"
            )));
        }
        self.max_results = max_results;
        Ok(self)
    }

    /// The validated query.
    pub fn query(&self) -> &str {
        &self.query
    }

    /// The validated result ceiling.
    pub fn max_results(&self) -> usize {
        self.max_results
    }
}

/// One ranked search result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchHit {
    /// The result title.
    pub title: String,
    /// The result URL.
    pub url: String,
    /// The result excerpt.
    pub snippet: String,
}

/// The bounded web-search connector contract.
///
/// Holds the endpoint configuration from the manifest; stateless across
/// calls — the transport and the credential arrive per call, so the same
/// provider value serves any tenant the host resolves credentials for.
#[derive(Debug, Clone)]
pub struct HttpSearchProvider {
    base_url: String,
    auth: Option<SearchAuth>,
    timeout: Duration,
}

impl HttpSearchProvider {
    /// A provider over `base_url` with the default timeout. The URL must
    /// be `https://` — the same rule manifests enforce.
    pub fn new(base_url: impl Into<String>, auth: Option<SearchAuth>) -> Result<Self> {
        let base_url = base_url.into();
        if !base_url.starts_with("https://") {
            return Err(conn_err(format!(
                "search base URL `{base_url}` must use `https://`"
            )));
        }
        Ok(Self {
            base_url,
            auth,
            timeout: DEFAULT_SEARCH_TIMEOUT,
        })
    }

    /// The provider for a validated `http-search` manifest.
    pub fn from_manifest(manifest: &ConnectorManifest) -> Result<Self> {
        match &manifest.provider {
            ProviderKind::HttpSearch(spec) => {
                Self::new(spec.base_url.clone(), spec.auth.clone())
            }
            ProviderKind::McpStdio(_) => Err(conn_err(format!(
                "manifest `{}` is mcp-stdio; HttpSearchProvider cannot serve it",
                manifest.id
            ))),
        }
    }

    /// Builder-style: override the per-call timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The configured endpoint.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The declared tool name this provider's catalog advertises:
    /// `<connector_id>/search`.
    pub fn tool_name(connector_id: &str) -> String {
        search_contract(connector_id).0
    }

    /// Execute one bounded search.
    ///
    /// Ceilings are enforced on both sides: the request body is built from
    /// a validated [`SearchRequest`], and the response is rejected before
    /// parsing when it exceeds [`MAX_SEARCH_RESPONSE_BYTES`], fails
    /// per-field caps on any hit, or is not the `{"results": [...]}`
    /// shape. Over-long ranked lists are truncated to the requested
    /// count — the tail of a ranking is droppable, malformed bytes are
    /// not.
    ///
    /// When the provider declares auth, `credential` must be the handle
    /// for the configured slot; its secret becomes the header value
    /// verbatim and appears nowhere else — not in errors, not in logs.
    pub async fn search(
        &self,
        transport: &dyn HttpTransport,
        credential: Option<&CredentialHandle>,
        request: &SearchRequest,
    ) -> Result<Vec<SearchHit>> {
        let mut headers = vec![("content-type".to_owned(), "application/json".to_owned())];
        if let Some(auth) = &self.auth {
            let handle = credential.ok_or_else(|| {
                conn_err(format!(
                    "search endpoint requires credential slot `{}`",
                    auth.credential_slot
                ))
            })?;
            if handle.slot() != auth.credential_slot {
                return Err(conn_err(format!(
                    "credential slot `{}` does not match the configured slot `{}`",
                    handle.slot(),
                    auth.credential_slot
                )));
            }
            headers.push((auth.header.clone(), handle.secret().to_owned()));
        }

        let body = serde_json::to_vec(&json!({
            "query": request.query(),
            "max_results": request.max_results(),
        }))
        .map_err(|e| conn_err(format!("failed to encode search request: {e}")))?;

        let exchange = transport.post(HttpRequest {
            url: self.base_url.clone(),
            headers,
            body,
            timeout: self.timeout,
        });
        let reply = tokio::time::timeout(self.timeout, exchange)
            .await
            .map_err(|_| conn_err(format!("search request timed out after {:?}", self.timeout)))??;

        if !(200..=299).contains(&reply.status) {
            // Status only: the body is provider-controlled text and may
            // quote the request — including the credential header's
            // neighborhood — back at us.
            return Err(conn_err(match reply.status {
                401 | 403 => {
                    format!("search endpoint rejected the credential (status {})", reply.status)
                }
                status => format!("search endpoint returned status {status}"),
            }));
        }
        if reply.body.len() > MAX_SEARCH_RESPONSE_BYTES {
            return Err(conn_err(format!(
                "search response of {} bytes exceeds the {MAX_SEARCH_RESPONSE_BYTES}-byte ceiling",
                reply.body.len()
            )));
        }

        let value: Value = serde_json::from_slice(&reply.body)
            .map_err(|e| conn_err(format!("search response was not JSON: {e}")))?;
        let results = value
            .get("results")
            .and_then(Value::as_array)
            .ok_or_else(|| conn_err("search response is missing the `results` array"))?;

        let mut hits = Vec::with_capacity(results.len().min(request.max_results()));
        for item in results {
            let title = bounded_field(item, "title", MAX_SEARCH_TITLE_BYTES)?;
            let url = bounded_field(item, "url", MAX_SEARCH_URL_BYTES)?;
            let snippet = bounded_field(item, "snippet", MAX_SEARCH_SNIPPET_BYTES)?;
            hits.push(SearchHit {
                title,
                url,
                snippet,
            });
        }
        hits.truncate(request.max_results());
        Ok(hits)
    }
}

/// Extract one bounded string field from a search result item.
fn bounded_field(item: &Value, field: &str, max_bytes: usize) -> Result<String> {
    let value = item
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| conn_err(format!("search result is missing `{field}`")))?;
    if value.len() > max_bytes {
        return Err(conn_err(format!(
            "search result `{field}` exceeds the {max_bytes}-byte ceiling"
        )));
    }
    Ok(value.to_owned())
}

#[async_trait]
impl ConnectorProvider for HttpSearchProvider {
    async fn connect(
        &self,
        manifest: &ConnectorManifest,
        _credentials: &[CredentialHandle],
    ) -> Result<Box<dyn ProviderSession>> {
        match &manifest.provider {
            ProviderKind::HttpSearch(_) => Ok(Box::new(HttpSearchSession {
                connector_id: manifest.id.clone(),
                provider: self.clone(),
            })),
            ProviderKind::McpStdio(_) => Err(conn_err(format!(
                "manifest `{}` is mcp-stdio; HttpSearchProvider cannot serve it",
                manifest.id
            ))),
        }
    }
}

/// An HTTP search session. Stateless — the catalog is declarative (one
/// `search` tool), so health is reported at call time and `catalog` is a
/// constant derivation. Teardown has nothing to close.
#[derive(Debug)]
struct HttpSearchSession {
    connector_id: String,
    provider: HttpSearchProvider,
}

#[async_trait]
impl ProviderSession for HttpSearchSession {
    fn connector_id(&self) -> &str {
        &self.connector_id
    }

    async fn catalog(&mut self) -> Result<Vec<ToolCapability>> {
        Ok(vec![search_capability(&self.connector_id)])
    }

    async fn shutdown(self: Box<Self>) -> Result<()> {
        let _ = self.provider;
        Ok(())
    }
}

/// The single source of the search tool's advertised surface: name,
/// description, and schema are built once here so the catalog capability
/// and the executable [`ConnectorSearchTool`] cannot drift apart.
fn search_contract(connector_id: &str) -> (String, String, Value) {
    (
        format!("{connector_id}/search"),
        format!(
            "Bounded web search via the `{connector_id}` connector. \
             Returns ranked results as title, URL, and snippet."
        ),
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "The search query.",
                },
                "max_results": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_SEARCH_RESULT_COUNT,
                    "description": "Maximum number of ranked results to return.",
                },
            },
            "required": ["query"],
            "additionalProperties": false,
        }),
    )
}

/// The catalog capability every `http-search` connector advertises.
/// Search reads the world and writes nothing, so it is
/// [`Effect::ReadOnly`].
fn search_capability(connector_id: &str) -> ToolCapability {
    let (name, description, schema) = search_contract(connector_id);
    ToolCapability {
        name,
        description,
        parameters_schema: schema,
        effect: Effect::ReadOnly,
    }
}

/// A search connector as an executable [`Tool`].
///
/// The tool is a thin, explicit delegate: it validates arguments into a
/// [`SearchRequest`] and calls [`HttpSearchProvider::search`]. The network
/// call lives in the provider, never hidden inside tool logic.
pub struct ConnectorSearchTool {
    provider: HttpSearchProvider,
    transport: Arc<dyn HttpTransport>,
    credential: Option<CredentialHandle>,
    name: String,
    description: String,
    schema: Value,
}

impl ConnectorSearchTool {
    /// The `<connector_id>/search` tool over `transport`, holding the
    /// resolved credential handle (when the provider declares auth).
    pub fn new(
        connector_id: &str,
        provider: HttpSearchProvider,
        transport: Arc<dyn HttpTransport>,
        credential: Option<CredentialHandle>,
    ) -> Self {
        let (name, description, schema) = search_contract(connector_id);
        Self {
            provider,
            transport,
            credential,
            name,
            description,
            schema,
        }
    }
}

impl std::fmt::Debug for ConnectorSearchTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectorSearchTool")
            .field("name", &self.name)
            .field("provider", &self.provider)
            .field("transport", &self.transport)
            .field("credential", &self.credential)
            .finish()
    }
}

#[async_trait]
impl Tool for ConnectorSearchTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> Value {
        self.schema.clone()
    }

    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }

    async fn call(&self, args: Value) -> Result<Value> {
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .ok_or_else(|| conn_err(format!("tool `{}` requires a string `query`", self.name)))?;
        let mut request = SearchRequest::new(query)?;
        if let Some(max_results) = args.get("max_results") {
            let count = max_results.as_u64().ok_or_else(|| {
                conn_err(format!("tool `{}` `max_results` must be a positive integer", self.name))
            })?;
            request = request.with_max_results(count as usize)?;
        }
        let hits = self
            .provider
            .search(
                self.transport.as_ref(),
                self.credential.as_ref(),
                &request,
            )
            .await?;
        Ok(json!({ "results": hits }))
    }
}
