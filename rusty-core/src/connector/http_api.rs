//! The generic HTTP REST/GraphQL API provider.
//!
//! An `http-api` manifest declares a base URL, an auth style (referencing
//! credential *slots*, never raw secrets), and an operations list; each
//! valid operation derives exactly one catalog tool named
//! `<connector-id>/<operation>` with the declared effect classification.
//! This is the foundation the service packs (ServiceNow, Gmail, Slack,
//! Linear, Notion, Google Calendar) are declared against: the pack is a
//! manifest, the machinery here is shared.
//!
//! Execution mirrors the search provider's discipline: the network sits
//! behind the [`HttpApiTransport`] seam (the arbitrary-method sibling of
//! [`super::HttpTransport`], which stays POST-shaped for the search
//! contract), secrets leave their [`CredentialHandle`] only into outbound
//! auth material, response and request byte ceilings are enforced before
//! parsing, and non-2xx statuses map to structured errors with truncated,
//! control-stripped bodies — 401/403 echo no body at all, since an
//! auth-failure page may quote the credential's neighborhood back.
//!
//! Two deliberate minimalities, both declared rather than hidden:
//!
//! - **GraphQL** is a body style, not a provider kind: `{param}`
//!   placeholders in the query template are substituted with the *JSON
//!   encoding* of the argument (a string arrives quoted and escaped, so
//!   interpolation cannot break out of its GraphQL position), and the body
//!   is the standard `{"query": "..."}` POST shape.
//! - **Idempotency** is opt-in per operation: a POST that declares an
//!   idempotency-key header gets a deterministic key derived from
//!   `(scope, operation, canonical args)`, so a retry of the same call
//!   presents the same key and cannot double-create. The scope is the
//!   instance id at the server wiring; token *acquisition* flows (OAuth
//!   dances) are out of scope — slots carry already-usable secrets.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::conn_err;
use super::credential::CredentialHandle;
use super::manifest::{
    ConnectorManifest, HttpApiAuth, HttpApiOperation, HttpMethod, MAX_HTTP_API_RESPONSE_BYTES,
    OperationBody,
};
use super::provider::{
    ConnectorProvider, HttpResponse, MAX_DERIVED_TOOL_NAME_LEN, ProviderSession, provider_kind_name,
};
use crate::error::Result;
use crate::record::sha256_hex;
use crate::tool::{MAX_TOOL_DESCRIPTION_BYTES, MAX_TOOL_SCHEMA_BYTES, Tool, ToolCapability};

/// Default per-call timeout for `http-api` operations.
pub const DEFAULT_HTTP_API_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum size of a built request body, enforced after assembly.
pub const MAX_HTTP_API_REQUEST_BYTES: usize = 64 * 1024;

/// Maximum bytes of a non-2xx response body echoed into an error string.
pub const MAX_HTTP_API_ERROR_BODY_BYTES: usize = 256;

/// Domain-separation prefix for idempotency-key derivation, in the spirit
/// of [`crate::effects::EFFECT_ID_DOMAIN`]: keys are content addresses and
/// the prefix keeps them collision-free against every other digest in the
/// system, with a version handle should the formula ever change.
pub const IDEMPOTENCY_KEY_DOMAIN: &str = "rusty/http-api-idempotency/v1";

/// One outbound arbitrary-method HTTP exchange, as the [`HttpApiTransport`]
/// seam sees it. The sibling of [`super::HttpRequest`] — which stays
/// POST-shaped for the search contract — carrying the method explicitly.
#[derive(Debug, Clone)]
pub struct HttpApiRequest {
    /// The HTTP method.
    pub method: HttpMethod,
    /// The full URL, path template rendered and query string appended.
    pub url: String,
    /// Header name/value pairs. Auth material is already resolved here —
    /// the transport never sees slot names.
    pub headers: Vec<(String, String)>,
    /// The serialized request body (empty for body-less methods).
    pub body: Vec<u8>,
    /// The per-call timeout the transport must enforce.
    pub timeout: Duration,
}

/// The arbitrary-method HTTP seam. Tests drive a scripted fake; the server
/// slice wires reqwest behind the same trait.
#[async_trait]
pub trait HttpApiTransport: std::fmt::Debug + Send + Sync {
    /// Send `request`, honoring `request.method` and `request.timeout`.
    async fn send(&self, request: HttpApiRequest) -> Result<HttpResponse>;
}

/// Derive the deterministic idempotency key for one operation call:
/// SHA-256 over the newline-joined, domain-prefixed tuple
/// `(domain, scope, operation, canonical-args-hash)`. The scope is the
/// instance identity at the server wiring, so two instances of one
/// connector never share a key, and a retry of the same call under the
/// same instance re-derives exactly the key the first attempt presented.
pub fn derive_idempotency_key(scope: &str, operation: &str, args: &Value) -> String {
    let material = [
        IDEMPOTENCY_KEY_DOMAIN,
        scope,
        operation,
        &super::canonical_json_hash(args),
    ]
    .join("\n");
    sha256_hex(material.as_bytes())
}

/// A validated generic HTTP API provider: the executable truth of an
/// `http-api` manifest.
///
/// Stateless across calls — the transport, the credentials, and the
/// idempotency scope arrive per call, so one provider value serves any
/// tenant the host resolves credentials for. Construction goes through
/// [`HttpApiProvider::from_manifest`] only: validation lives in the
/// manifest, and there is no door that skips it.
#[derive(Clone)]
pub struct HttpApiProvider {
    base_url: String,
    config: BTreeMap<String, String>,
    auth: Option<HttpApiAuth>,
    default_headers: Vec<(String, String)>,
    health_check: Option<String>,
    operations: Vec<HttpApiOperation>,
    timeout: Duration,
    max_response_bytes: usize,
    health_transport: Option<Arc<dyn HttpApiTransport>>,
}

impl std::fmt::Debug for HttpApiProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The transport is deliberately *not* printed: it is stateful (a
        // scripted fake, a connection pool) and its state can hold resolved
        // auth material from prior requests. Debug surfaces show the
        // declared configuration — slot names, never secrets — only.
        f.debug_struct("HttpApiProvider")
            .field("base_url", &self.base_url)
            .field("config", &self.config)
            .field("auth", &self.auth)
            .field("default_headers", &self.default_headers)
            .field("health_check", &self.health_check)
            .field("operations", &self.operations)
            .field("timeout", &self.timeout)
            .field("max_response_bytes", &self.max_response_bytes)
            .field(
                "health_transport",
                &self.health_transport.as_ref().map(|_| "<wired>"),
            )
            .finish()
    }
}

impl HttpApiProvider {
    /// The provider for a validated `http-api` manifest.
    pub fn from_manifest(manifest: &ConnectorManifest) -> Result<Self> {
        match &manifest.provider {
            super::manifest::ProviderKind::HttpApi(spec) => Ok(Self {
                base_url: spec.base_url.clone(),
                config: BTreeMap::new(),
                auth: spec.auth.clone(),
                default_headers: spec.default_headers.clone(),
                health_check: spec.health_check.clone(),
                operations: spec.operations.clone(),
                timeout: DEFAULT_HTTP_API_TIMEOUT,
                max_response_bytes: MAX_HTTP_API_RESPONSE_BYTES,
                health_transport: None,
            }),
            other => Err(conn_err(format!(
                "manifest `{}` is {}; HttpApiProvider cannot serve it",
                manifest.id,
                provider_kind_name(other)
            ))),
        }
    }

    /// Builder-style: the instance's non-secret config values, substituted
    /// into `{param}` placeholders in the base URL at request time. A
    /// manifest with a literal base URL never needs this — the URL is used
    /// byte-identically.
    pub fn with_config(mut self, config: BTreeMap<String, String>) -> Self {
        self.config = config;
        self
    }

    /// Builder-style: override the default per-call timeout (operations
    /// may still tighten it individually via `timeout_ms`).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Builder-style: tighten the provider-wide response ceiling.
    /// Values above [`MAX_HTTP_API_RESPONSE_BYTES`] clamp to it.
    pub fn with_max_response_bytes(mut self, max_bytes: usize) -> Self {
        self.max_response_bytes = max_bytes.clamp(1, MAX_HTTP_API_RESPONSE_BYTES);
        self
    }

    /// Builder-style: the transport `connect` runs the declared
    /// health-check operation over. Without one the health check is
    /// skipped — the registry still derives the catalog and call-time
    /// failures surface per operation, matching the search provider's
    /// "health is reported at call time" stance.
    pub fn with_health_transport(mut self, transport: Arc<dyn HttpApiTransport>) -> Self {
        self.health_transport = Some(transport);
        self
    }

    /// The configured API root.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The declared operations, in canonical (name-sorted) order.
    pub fn operations(&self) -> &[HttpApiOperation] {
        &self.operations
    }

    /// One declared operation by name.
    pub fn operation(&self, name: &str) -> Option<&HttpApiOperation> {
        self.operations.iter().find(|op| op.name == name)
    }

    /// Derive the tool catalog: one [`ToolCapability`] per operation,
    /// namespaced `<connector>/<operation>`, the declared params schema
    /// passed through, the declared effect mapped onto the wire taxonomy.
    ///
    /// The manifest validated every field at construction; the contract
    /// caps are re-checked here so a provider built from an unvalidated
    /// (e.g. deserialized) manifest still fails closed rather than
    /// advertising a catalog the runtime executor would reject.
    pub fn catalog(&self, connector_id: &str) -> Result<Vec<ToolCapability>> {
        let mut capabilities = Vec::with_capacity(self.operations.len());
        for operation in &self.operations {
            let name = format!("{connector_id}/{}", operation.name);
            if name.len() > MAX_DERIVED_TOOL_NAME_LEN
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._:-/".contains(&b))
            {
                return Err(conn_err(format!(
                    "http-api operation `{}` maps to invalid catalog name `{name}`",
                    operation.name
                )));
            }
            if operation.description.is_empty()
                || operation.description != operation.description.trim()
                || operation.description.len() > MAX_TOOL_DESCRIPTION_BYTES
                || operation.description.chars().any(char::is_control)
            {
                return Err(conn_err(format!(
                    "http-api operation `{}` description must be non-empty, trimmed, control-free, and at most {MAX_TOOL_DESCRIPTION_BYTES} bytes",
                    operation.name
                )));
            }
            if !operation.params_schema.is_object() {
                return Err(conn_err(format!(
                    "http-api operation `{}` params schema must be a JSON object",
                    operation.name
                )));
            }
            let schema_bytes = serde_json::to_vec(&operation.params_schema).map_err(|e| {
                conn_err(format!(
                    "http-api operation `{}` schema did not serialize: {e}",
                    operation.name
                ))
            })?;
            if schema_bytes.len() > MAX_TOOL_SCHEMA_BYTES {
                return Err(conn_err(format!(
                    "http-api operation `{}` schema exceeds {MAX_TOOL_SCHEMA_BYTES} bytes",
                    operation.name
                )));
            }
            capabilities.push(ToolCapability {
                name,
                description: operation.description.clone(),
                parameters_schema: operation.params_schema.clone(),
                effect: operation.effect.wire_effect(),
            });
        }
        capabilities.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(capabilities)
    }

    /// Execute one declared operation.
    ///
    /// The pipeline, in order, fail-closed at every step: `args` must be
    /// an object covering exactly the schema's declared properties
    /// (unknown arguments and missing required ones are errors); the path
    /// template renders with percent-encoded scalar values (a structured
    /// value in a path parameter is an error); query and body assemble
    /// from their routed parameters; auth resolves from `credentials` into
    /// header or query material; a declared idempotency-key header gets
    /// its deterministic key from `(scope, operation, canonical args)`;
    /// the transport sends under the operation's timeout; and the reply
    /// passes the byte ceiling, the status mapping, and the optional
    /// JSON-pointer projection before reaching the caller.
    ///
    /// `scope` is the idempotency-key scope — the instance id at the
    /// server wiring. Secrets appear only in the outbound request: never
    /// in errors, never in logs.
    pub async fn execute(
        &self,
        transport: &dyn HttpApiTransport,
        credentials: &[CredentialHandle],
        scope: &str,
        operation: &str,
        args: &Value,
    ) -> Result<Value> {
        let op = self.operation(operation).ok_or_else(|| {
            conn_err(format!("http-api connector has no operation `{operation}`"))
        })?;
        let object = args.as_object().ok_or_else(|| {
            conn_err(format!(
                "operation `{operation}` arguments must be a JSON object"
            ))
        })?;
        let properties = op
            .params_schema
            .get("properties")
            .and_then(Value::as_object);
        for key in object.keys() {
            if !properties.is_some_and(|props| props.contains_key(key)) {
                return Err(conn_err(format!(
                    "operation `{operation}` got unexpected argument `{key}` (not declared in its params schema)"
                )));
            }
        }
        if let Some(required) = op.params_schema.get("required").and_then(Value::as_array) {
            for entry in required {
                if let Some(name) = entry.as_str() {
                    if !object.contains_key(name) {
                        return Err(conn_err(format!(
                            "operation `{operation}` is missing required argument `{name}`"
                        )));
                    }
                }
            }
        }

        // Path: placeholders take percent-encoded scalars only.
        let path = render_template(&op.path, operation, object, |name, value| {
            scalar_string(value).map(|s| percent_encode(&s)).ok_or_else(|| {
                conn_err(format!(
                    "operation `{operation}` path parameter `{name}` must be a string, number, or boolean"
                ))
            })
        })?;

        // Query: routed parameters plus the query-param auth style.
        let mut query: Vec<(String, String)> = Vec::new();
        for name in &op.query_params {
            if let Some(value) = object.get(name) {
                let encoded = scalar_string(value).map(|s| percent_encode(&s)).ok_or_else(|| {
                    conn_err(format!(
                        "operation `{operation}` query parameter `{name}` must be a string, number, or boolean"
                    ))
                })?;
                query.push((name.clone(), encoded));
            }
        }
        if let Some(HttpApiAuth::QueryParam {
            param,
            credential_slot,
        }) = &self.auth
        {
            let handle = resolve_slot(credentials, credential_slot)?;
            query.push((param.clone(), percent_encode(handle.secret())));
        }

        // Body: assembled JSON object, or an interpolated GraphQL query.
        let body = match &op.body {
            OperationBody::None => Vec::new(),
            OperationBody::Json { params } => {
                let mut map = serde_json::Map::new();
                for name in params {
                    if let Some(value) = object.get(name) {
                        map.insert(name.clone(), value.clone());
                    }
                }
                let bytes = serde_json::to_vec(&Value::Object(map)).map_err(|e| {
                    conn_err(format!(
                        "operation `{operation}` body did not serialize: {e}"
                    ))
                })?;
                if bytes.len() > MAX_HTTP_API_REQUEST_BYTES {
                    return Err(conn_err(format!(
                        "operation `{operation}` body of {} bytes exceeds the {MAX_HTTP_API_REQUEST_BYTES}-byte ceiling",
                        bytes.len()
                    )));
                }
                bytes
            }
            OperationBody::Graphql { query } => {
                let interpolated = render_template(query, operation, object, |name, value| {
                    // The JSON encoding of the value is the escaping: a
                    // string arrives quoted, so the interpolation cannot
                    // break out of its GraphQL position.
                    serde_json::to_string(value).map_err(|e| {
                        conn_err(format!(
                            "operation `{operation}` graphql parameter `{name}` did not encode: {e}"
                        ))
                    })
                })?;
                let bytes = serde_json::to_vec(&json!({ "query": interpolated })).map_err(|e| {
                    conn_err(format!(
                        "operation `{operation}` graphql body did not serialize: {e}"
                    ))
                })?;
                if bytes.len() > MAX_HTTP_API_REQUEST_BYTES {
                    return Err(conn_err(format!(
                        "operation `{operation}` body of {} bytes exceeds the {MAX_HTTP_API_REQUEST_BYTES}-byte ceiling",
                        bytes.len()
                    )));
                }
                bytes
            }
        };

        let mut headers = self.default_headers.clone();
        if !body.is_empty() {
            headers.push(("content-type".to_owned(), "application/json".to_owned()));
        }
        match &self.auth {
            Some(HttpApiAuth::BearerToken { credential_slot }) => {
                let handle = resolve_slot(credentials, credential_slot)?;
                headers.push((
                    "authorization".to_owned(),
                    format!("Bearer {}", handle.secret()),
                ));
            }
            Some(HttpApiAuth::Basic {
                username_slot,
                password_slot,
            }) => {
                let username = resolve_slot(credentials, username_slot)?;
                let password = resolve_slot(credentials, password_slot)?;
                headers.push((
                    "authorization".to_owned(),
                    format!(
                        "Basic {}",
                        base64_encode(
                            format!("{}:{}", username.secret(), password.secret()).as_bytes()
                        )
                    ),
                ));
            }
            Some(HttpApiAuth::Header {
                header,
                credential_slot,
            }) => {
                let handle = resolve_slot(credentials, credential_slot)?;
                headers.push((header.clone(), handle.secret().to_owned()));
            }
            Some(HttpApiAuth::QueryParam { .. }) | None => {}
        }
        if let Some(header) = &op.idempotency_key_header {
            headers.push((
                header.clone(),
                derive_idempotency_key(scope, &op.name, args),
            ));
        }

        let mut url = format!(
            "{}{}",
            resolve_base_url(&self.base_url, &self.config)?.trim_end_matches('/'),
            path
        );
        if !query.is_empty() {
            let pairs = query
                .iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect::<Vec<_>>()
                .join("&");
            url.push('?');
            url.push_str(&pairs);
        }

        let timeout = op
            .timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(self.timeout);
        let exchange = transport.send(HttpApiRequest {
            method: op.method,
            url,
            headers,
            body,
            timeout,
        });
        let reply = tokio::time::timeout(timeout, exchange)
            .await
            .map_err(|_| {
                conn_err(format!(
                    "operation `{operation}` timed out after {timeout:?}"
                ))
            })??;

        if !(200..=299).contains(&reply.status) {
            return Err(conn_err(match reply.status {
                // Status only: an auth-failure body may quote the request —
                // including the credential's neighborhood — back at us.
                401 | 403 => format!(
                    "operation `{operation}` was rejected by the endpoint (status {}): check the resolved credential",
                    reply.status
                ),
                status => format!(
                    "operation `{operation}` returned status {status}: {}",
                    sanitize_excerpt(&reply.body, MAX_HTTP_API_ERROR_BODY_BYTES)
                ),
            }));
        }
        let ceiling = op
            .response
            .max_bytes
            .unwrap_or(self.max_response_bytes)
            .min(self.max_response_bytes);
        if reply.body.len() > ceiling {
            return Err(conn_err(format!(
                "operation `{operation}` response of {} bytes exceeds the {ceiling}-byte ceiling",
                reply.body.len()
            )));
        }

        let value: Value = match serde_json::from_slice(&reply.body) {
            Ok(value) => value,
            Err(error) => {
                if op.response.projection.is_some() {
                    return Err(conn_err(format!(
                        "operation `{operation}` response was not JSON, so its projection cannot resolve: {error}"
                    )));
                }
                // Passthrough: a non-JSON body is still the answer, as text.
                return Ok(Value::String(
                    String::from_utf8_lossy(&reply.body).into_owned(),
                ));
            }
        };
        match &op.response.projection {
            None => Ok(value),
            Some(pointer) => value.pointer(pointer).cloned().ok_or_else(|| {
                conn_err(format!(
                    "operation `{operation}` projection `{pointer}` did not resolve in the response"
                ))
            }),
        }
    }
}

#[async_trait]
impl ConnectorProvider for HttpApiProvider {
    async fn connect(
        &self,
        manifest: &ConnectorManifest,
        credentials: &[CredentialHandle],
        config: &BTreeMap<String, String>,
    ) -> Result<Box<dyn ProviderSession>> {
        match &manifest.provider {
            super::manifest::ProviderKind::HttpApi(_) => {}
            other => {
                return Err(conn_err(format!(
                    "manifest `{}` is {}; HttpApiProvider cannot serve it",
                    manifest.id,
                    provider_kind_name(other)
                )));
            }
        }
        // The provider is per-manifest and shared; the config is
        // per-instance. The session — and the health check below — run on
        // a configured clone so two instances of one manifest resolve
        // their own base URLs.
        let configured = self.clone().with_config(config.clone());
        // Fail closed on slot resolution: the registry already fails
        // pending instances with unresolved slots, but a provider reached
        // directly must not discover a missing credential at first call.
        if let Some(auth) = &configured.auth {
            for slot in auth.referenced_slots() {
                resolve_slot(credentials, slot)?;
            }
        }
        // The health check runs only when the host wired a transport for
        // it; validation guarantees the named operation is a parameterless
        // read-only GET.
        if let (Some(operation), Some(transport)) =
            (&configured.health_check, &self.health_transport)
        {
            configured
                .execute(
                    transport.as_ref(),
                    credentials,
                    "connect",
                    operation,
                    &json!({}),
                )
                .await
                .map_err(|e| conn_err(format!("health check `{operation}` failed: {e}")))?;
        }
        Ok(Box::new(HttpApiSession {
            connector_id: manifest.id.clone(),
            provider: configured,
        }))
    }
}

/// An `http-api` session. The catalog is declarative (one tool per
/// declared operation), so `catalog` is a constant derivation and teardown
/// has nothing to close.
#[derive(Debug)]
struct HttpApiSession {
    connector_id: String,
    provider: HttpApiProvider,
}

#[async_trait]
impl ProviderSession for HttpApiSession {
    fn connector_id(&self) -> &str {
        &self.connector_id
    }

    async fn catalog(&mut self) -> Result<Vec<ToolCapability>> {
        self.provider.catalog(&self.connector_id)
    }

    async fn shutdown(self: Box<Self>) -> Result<()> {
        let _ = self.provider;
        Ok(())
    }
}

/// One declared `http-api` operation as an executable [`Tool`].
///
/// A thin, explicit delegate in the [`super::ConnectorSearchTool`]
/// pattern: the tool owns the operation's advertised surface and the
/// idempotency-key contract, and the network call lives in the provider.
/// `scope` is the idempotency scope — the instance id at the server
/// wiring — so the key a retry presents is derived from exactly the
/// identity the first attempt used.
pub struct HttpApiTool {
    provider: HttpApiProvider,
    operation: String,
    transport: Arc<dyn HttpApiTransport>,
    credentials: Vec<CredentialHandle>,
    scope: String,
    name: String,
    description: String,
    schema: Value,
    effect: crate::record::Effect,
}

impl HttpApiTool {
    /// The `<connector>/<operation>` tool over `transport`, holding the
    /// resolved credential handles (when the provider declares auth).
    pub fn new(
        connector_id: &str,
        provider: HttpApiProvider,
        operation: &str,
        transport: Arc<dyn HttpApiTransport>,
        credentials: Vec<CredentialHandle>,
        scope: impl Into<String>,
    ) -> Result<Self> {
        let op = provider.operation(operation).ok_or_else(|| {
            conn_err(format!(
                "http-api connector `{connector_id}` has no operation `{operation}`"
            ))
        })?;
        // Clone the advertised surface up front so `provider` can move
        // into the struct without an outstanding borrow.
        let operation_name = op.name.clone();
        let description = op.description.clone();
        let schema = op.params_schema.clone();
        let effect = op.effect.wire_effect();
        Ok(Self {
            provider,
            operation: operation_name,
            transport,
            credentials,
            scope: scope.into(),
            name: format!("{connector_id}/{operation}"),
            description,
            schema,
            effect,
        })
    }
}

impl std::fmt::Debug for HttpApiTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // As with the provider, the transport's state (which can hold
        // resolved auth material from prior requests) is never printed.
        f.debug_struct("HttpApiTool")
            .field("name", &self.name)
            .field("operation", &self.operation)
            .field("scope", &self.scope)
            .field("provider", &self.provider)
            .field("transport", &"<wired>")
            .field("credentials", &self.credentials)
            .finish()
    }
}

#[async_trait]
impl Tool for HttpApiTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> Value {
        self.schema.clone()
    }

    fn effect(&self) -> crate::record::Effect {
        self.effect
    }

    fn idempotency_key(&self, args: &Value) -> Option<String> {
        // Only a keyed POST answers here: the admission boundary rejects an
        // `Idempotent` call with no key, and the key it sees must be the
        // one dispatch sends — both come from the same derivation.
        self.provider
            .operation(&self.operation)
            .and_then(|op| op.idempotency_key_header.as_ref())
            .map(|_| derive_idempotency_key(&self.scope, &self.operation, args))
    }

    async fn call(&self, args: Value) -> Result<Value> {
        self.provider
            .execute(
                self.transport.as_ref(),
                &self.credentials,
                &self.scope,
                &self.operation,
                &args,
            )
            .await
    }
}

/// Resolve a base-url template against an instance's non-secret config:
/// every `{param}` placeholder substitutes the config value verbatim
/// (`{{`/`}}` are literal braces, as everywhere in this plane). A template
/// without placeholders returns byte-identically. The result is held to
/// the declaration's own rule — `https://`, no query string, no fragment,
/// no whitespace or control characters — so a config value that would
/// smuggle URL structure fails here, at both instantiation (the server's
/// 422) and request time (fail-closed).
pub fn resolve_base_url(template: &str, config: &BTreeMap<String, String>) -> Result<String> {
    let mut out = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'{' if bytes.get(index + 1) == Some(&b'{') => {
                out.push('{');
                index += 2;
            }
            b'}' if bytes.get(index + 1) == Some(&b'}') => {
                out.push('}');
                index += 2;
            }
            b'{' => {
                let close = template[index + 1..]
                    .find('}')
                    .map(|offset| index + 1 + offset)
                    .expect("manifest validation rejects unclosed placeholders");
                let name = &template[index + 1..close];
                let value = config.get(name).ok_or_else(|| {
                    conn_err(format!(
                        "config param `{name}` has no value; the instance cannot resolve the base URL"
                    ))
                })?;
                out.push_str(value);
                index = close + 1;
            }
            _ => {
                let ch = template[index..]
                    .chars()
                    .next()
                    .expect("index is in bounds");
                out.push(ch);
                index += ch.len_utf8();
            }
        }
    }
    if !out.starts_with("https://")
        || out.len() == "https://".len()
        || out.chars().any(char::is_control)
        || out.contains(char::is_whitespace)
        || out.contains('?')
        || out.contains('#')
    {
        return Err(conn_err(format!(
            "resolved base URL `{out}` must be an `https://` URL without whitespace, control characters, a query string, or a fragment"
        )));
    }
    Ok(out)
}

/// The credential handle for `slot`, or a fail-closed error naming the
/// slot — never the secret.
fn resolve_slot<'a>(
    credentials: &'a [CredentialHandle],
    slot: &str,
) -> Result<&'a CredentialHandle> {
    credentials
        .iter()
        .find(|handle| handle.slot() == slot)
        .ok_or_else(|| conn_err(format!("credential slot `{slot}` is not resolved")))
}

/// Render a `{param}` template, substituting each placeholder through
/// `substitute` and collapsing the `{{` / `}}` literal-brace escapes. The
/// template was validated at manifest construction, so the brace structure
/// is sound; a missing argument is the only failure mode left.
fn render_template(
    template: &str,
    operation: &str,
    args: &serde_json::Map<String, Value>,
    substitute: impl Fn(&str, &Value) -> Result<String>,
) -> Result<String> {
    let mut out = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'{' if bytes.get(index + 1) == Some(&b'{') => {
                out.push('{');
                index += 2;
            }
            b'}' if bytes.get(index + 1) == Some(&b'}') => {
                out.push('}');
                index += 2;
            }
            b'{' => {
                let close = template[index + 1..]
                    .find('}')
                    .map(|offset| index + 1 + offset)
                    .expect("manifest validation rejects unclosed placeholders");
                let name = &template[index + 1..close];
                let value = args.get(name).ok_or_else(|| {
                    conn_err(format!(
                        "operation `{operation}` requires argument `{name}`"
                    ))
                })?;
                out.push_str(&substitute(name, value)?);
                index = close + 1;
            }
            _ => {
                let ch = template[index..]
                    .chars()
                    .next()
                    .expect("index is in bounds");
                out.push(ch);
                index += ch.len_utf8();
            }
        }
    }
    Ok(out)
}

/// The string form of a scalar JSON value, or `None` for objects, arrays,
/// and null — structured values have no honest path or query rendering.
fn scalar_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(_) | Value::Bool(_) => Some(value.to_string()),
        _ => None,
    }
}

/// Percent-encode for path segments and query values: URL-unreserved
/// characters (`[A-Za-z0-9-._~]`) pass through, everything else becomes
/// `%XX` of its UTF-8 bytes.
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push_str(&format!("{byte:02X}"));
        }
    }
    out
}

/// RFC 4648 base64 with padding — the Basic-auth encoding, written out
/// here because the crate takes no new dependencies.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = u32::from(chunk[0]);
        let second = u32::from(*chunk.get(1).unwrap_or(&0));
        let third = u32::from(*chunk.get(2).unwrap_or(&0));
        let packed = first << 16 | second << 8 | third;
        out.push(ALPHABET[(packed >> 18 & 63) as usize] as char);
        out.push(ALPHABET[(packed >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(packed >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(packed & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// A bounded, control-stripped excerpt of a response body for error
/// strings: lossy UTF-8, control characters flattened to spaces (an error
/// string must not smuggle terminal escapes), truncated at a char boundary
/// with an explicit marker.
fn sanitize_excerpt(body: &[u8], max_bytes: usize) -> String {
    let text: String = String::from_utf8_lossy(body)
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if text.len() <= max_bytes {
        return text;
    }
    const MARKER: &str = "…[truncated]";
    let budget = max_bytes - MARKER.len();
    let mut end = budget;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut excerpt = text[..end].to_owned();
    excerpt.push_str(MARKER);
    excerpt
}
