//! Connector manifests: the declared, content-addressed connector contract.
//!
//! A [`ConnectorManifest`] is *declared* configuration: who the connector
//! is, which provider kind realizes it, which credential slots an instance
//! needs, and a human-facing capability summary. Its identity is the
//! SHA-256 of the canonical serialization of everything except the hash
//! field itself, so re-registering the same bytes is idempotent and
//! registering different bytes under an existing id is a new entry, never
//! a silent overwrite.
//!
//! Validation is strict and fail-closed: unknown provider kinds, missing
//! commands or URLs, malformed ids, and oversized fields are rejected at
//! construction (and again at registry admission), never repaired.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::conn_err;
use crate::error::Result;

/// Maximum length of a connector id (kebab-case).
pub const MAX_CONNECTOR_ID_LEN: usize = 64;

/// Maximum length of a version string.
pub const MAX_VERSION_LEN: usize = 32;

/// Maximum length of the human-facing display name.
pub const MAX_DISPLAY_NAME_LEN: usize = 128;

/// Maximum length of the human-facing description.
pub const MAX_DESCRIPTION_LEN: usize = 4 * 1024;

/// Maximum number of declared capability summary entries.
pub const MAX_DECLARED_CAPABILITIES: usize = 64;

/// Maximum length of one capability summary entry.
pub const MAX_CAPABILITY_SUMMARY_LEN: usize = 256;

/// Maximum number of credential slots a manifest may declare.
pub const MAX_CREDENTIAL_SLOTS: usize = 16;

/// Maximum length of a credential slot name.
pub const MAX_SLOT_NAME_LEN: usize = 64;

/// Maximum length of a credential slot description.
pub const MAX_SLOT_DESCRIPTION_LEN: usize = 256;

/// Maximum number of non-secret config params a manifest may declare.
pub const MAX_CONFIG_PARAMS: usize = 16;

/// Maximum length of a config param description.
pub const MAX_CONFIG_PARAM_DESCRIPTION_LEN: usize = 256;

/// Maximum length of an MCP stdio command path/name.
pub const MAX_COMMAND_LEN: usize = 512;

/// Maximum number of MCP stdio command arguments.
pub const MAX_ARGS: usize = 64;

/// Maximum length of one command argument.
pub const MAX_ARG_LEN: usize = 1024;

/// Maximum number of environment variables the allowlist may pass through.
pub const MAX_ENV_ALLOWLIST: usize = 32;

/// Maximum length of an environment variable name.
pub const MAX_ENV_NAME_LEN: usize = 128;

/// Maximum length of an HTTP search base URL.
pub const MAX_BASE_URL_LEN: usize = 2048;

/// Maximum length of an HTTP header name.
pub const MAX_AUTH_HEADER_LEN: usize = 128;

/// Maximum number of operations an `http-api` manifest may declare.
pub const MAX_OPERATIONS: usize = 64;

/// Maximum length of an operation name (kebab-case).
pub const MAX_OPERATION_NAME_LEN: usize = 64;

/// Maximum length of an operation description. Deliberately tighter than
/// the tool contract's [`crate::tool::MAX_TOOL_DESCRIPTION_BYTES`] so the
/// derived catalog never approaches that ceiling.
pub const MAX_OPERATION_DESCRIPTION_LEN: usize = 1024;

/// Maximum length of an operation path template.
pub const MAX_PATH_TEMPLATE_LEN: usize = 512;

/// Maximum serialized size of one operation's parameter schema.
pub const MAX_OPERATION_SCHEMA_BYTES: usize = 16 * 1024;

/// Maximum size of a GraphQL query template.
pub const MAX_GRAPHQL_TEMPLATE_BYTES: usize = 8 * 1024;

/// Maximum number of default headers an `http-api` spec may declare.
pub const MAX_DEFAULT_HEADERS: usize = 16;

/// Maximum length of a default header value.
pub const MAX_HEADER_VALUE_LEN: usize = 1024;

/// Maximum length of a query-parameter name (operation or auth).
pub const MAX_QUERY_PARAM_NAME_LEN: usize = 128;

/// Maximum length of a response projection pointer.
pub const MAX_PROJECTION_LEN: usize = 256;

/// Provider-wide response ceiling for `http-api` calls; per-operation
/// overrides may tighten but never exceed it.
pub const MAX_HTTP_API_RESPONSE_BYTES: usize = 256 * 1024;

/// Maximum per-operation timeout override, in milliseconds.
pub const MAX_HTTP_API_TIMEOUT_MS: u64 = 60_000;

/// A named credential slot an instance requires.
///
/// The slot is a *name*, never a value: at instantiation the
/// [`super::CredentialBroker`] is asked for `(tenant, slot)` and a missing
/// answer fails the instance with a reason naming this slot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialSlot {
    /// Slot name, e.g. `api_key` (`[a-z][a-z0-9_]*`).
    pub name: String,
    /// Human-facing note on what the credential unlocks. May be empty.
    pub description: String,
}

/// A named non-secret configuration parameter an instance supplies.
///
/// The counterpart of [`CredentialSlot`] for values that are not secrets:
/// instance identity (a ServiceNow subdomain), region, environment. The
/// manifest declares the *name*; the value arrives at instantiation and
/// lives on the instance, never in the content-pinned manifest. An
/// `http-api` [`HttpApiSpec::base_url`] may carry `{param}` placeholders
/// naming these params; the executor substitutes the instance's values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigParam {
    /// Param name, e.g. `instance` (`[a-zA-Z][a-zA-Z0-9_]*` — the
    /// placeholder charset, so the name can appear as `{name}`).
    pub name: String,
    /// Human-facing note on what the value selects. May be empty.
    pub description: String,
}

/// How the connector authenticates search calls, when it does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchAuth {
    /// The HTTP header that carries the credential (an HTTP token:
    /// ASCII letters, digits, or `-`).
    pub header: String,
    /// The credential slot (declared in
    /// [`ConnectorManifest::credential_slots`]) whose secret becomes the
    /// header value, verbatim.
    pub credential_slot: String,
}

/// Provider configuration for an MCP stdio connector.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpStdioSpec {
    /// The server executable (path or resolved name).
    pub command: String,
    /// Arguments, in order. Order is semantic, so unlike the env allowlist
    /// it is preserved exactly and committed to by the manifest hash.
    pub args: Vec<String>,
    /// Environment variables passed through from the host environment to
    /// the child. The child starts with a scrubbed environment; only these
    /// names cross. Sorted and deduplicated at construction.
    pub env_allowlist: Vec<String>,
}

/// Provider configuration for a bounded HTTP web-search connector.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpSearchSpec {
    /// The search endpoint. `https://` only — a search connector egresses
    /// tenant queries, so plaintext transport is rejected at declaration.
    pub base_url: String,
    /// How calls authenticate, or `None` for an unauthenticated endpoint.
    pub auth: Option<SearchAuth>,
}

/// The HTTP method an operation invokes.
///
/// Serialized in uppercase (`GET`, `POST`, …): the method is part of the
/// declared contract and commits to the manifest hash as the wire spells
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    /// HTTP GET.
    Get,
    /// HTTP POST.
    Post,
    /// HTTP PATCH.
    Patch,
    /// HTTP PUT.
    Put,
    /// HTTP DELETE.
    Delete,
}

impl HttpMethod {
    /// The wire spelling.
    pub fn as_str(&self) -> &'static str {
        match self {
            HttpMethod::Get => "GET",
            HttpMethod::Post => "POST",
            HttpMethod::Patch => "PATCH",
            HttpMethod::Put => "PUT",
            HttpMethod::Delete => "DELETE",
        }
    }

    /// `true` when the method may carry a request body under this plane's
    /// rules (GET and DELETE never do — a body on either is a spec bug).
    pub fn allows_body(&self) -> bool {
        matches!(self, HttpMethod::Post | HttpMethod::Patch | HttpMethod::Put)
    }
}

/// How an `http-api` connector authenticates calls.
///
/// Every style references credential *slots* declared in
/// [`ConnectorManifest::credential_slots`] — never raw secrets. The secret
/// bytes are resolved from the slot at the moment of use and appear only
/// in the outbound auth material, never in errors or logs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "style", rename_all = "snake_case")]
pub enum HttpApiAuth {
    /// `Authorization: Bearer <secret>` from one slot.
    BearerToken {
        /// The slot whose secret becomes the bearer token.
        credential_slot: String,
    },
    /// `Authorization: Basic base64(<user>:<pass>)` from two slots.
    Basic {
        /// The slot whose secret is the username.
        username_slot: String,
        /// The slot whose secret is the password.
        password_slot: String,
    },
    /// `<header>: <secret>` from one slot (e.g. `x-api-key`).
    Header {
        /// The HTTP header carrying the credential.
        header: String,
        /// The slot whose secret becomes the header value, verbatim.
        credential_slot: String,
    },
    /// `?<param>=<secret>` on the query string from one slot.
    QueryParam {
        /// The query parameter carrying the credential.
        param: String,
        /// The slot whose secret becomes the parameter value.
        credential_slot: String,
    },
}

impl HttpApiAuth {
    /// Every credential slot this style references.
    pub fn referenced_slots(&self) -> Vec<&str> {
        match self {
            HttpApiAuth::BearerToken { credential_slot } => vec![credential_slot],
            HttpApiAuth::Basic {
                username_slot,
                password_slot,
            } => vec![username_slot, password_slot],
            HttpApiAuth::Header {
                credential_slot, ..
            } => vec![credential_slot],
            HttpApiAuth::QueryParam {
                credential_slot, ..
            } => vec![credential_slot],
        }
    }
}

/// How an operation builds its request body from the call arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OperationBody {
    /// No request body (the only style GET and DELETE may declare).
    None,
    /// A JSON object assembled from the named parameters. Each named
    /// parameter present in the call arguments becomes one top-level key.
    Json {
        /// The parameter names routed into the body object.
        params: Vec<String>,
    },
    /// A GraphQL POST: `{param}` placeholders in the query template are
    /// substituted with the JSON encoding of the argument value (a string
    /// argument arrives quoted and escaped, so the interpolation cannot
    /// break out of its GraphQL position), and the body is
    /// `{"query": "<interpolated>"}`. Literal braces — which GraphQL
    /// selection sets are full of — are written `{{` and `}}`.
    Graphql {
        /// The query template with `{param}` placeholders.
        query: String,
    },
}

/// The declared effect classification of one operation, mapped
/// one-to-one onto the effect kernel's wire [`crate::record::Effect`].
///
/// The declaration is explicit per operation — never inferred from the
/// method alone — but validation holds the two to an honest contract:
/// GETs are always [`OperationEffect::ReadOnly`], DELETEs always
/// [`OperationEffect::Irreversible`], and `Idempotent` exists only for a
/// POST that declares an idempotency-key header (the key is what makes
/// the claim mean anything at the wire).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationEffect {
    /// Reads the world, writes nothing → [`crate::record::Effect::ReadOnly`].
    ReadOnly,
    /// Safe to retry under the generated idempotency key →
    /// [`crate::record::Effect::Idempotent`].
    Idempotent,
    /// Duplicates on retry but has a logical undo →
    /// [`crate::record::Effect::Compensatable`].
    Compensatable,
    /// No safe repetition and no undo →
    /// [`crate::record::Effect::NonIdempotent`] (the kernel's
    /// *irreversible* rung; see `effects.rs` for the naming reconciliation).
    Irreversible,
}

impl OperationEffect {
    /// The wire-level effect class this declaration maps to.
    pub fn wire_effect(&self) -> crate::record::Effect {
        match self {
            OperationEffect::ReadOnly => crate::record::Effect::ReadOnly,
            OperationEffect::Idempotent => crate::record::Effect::Idempotent,
            OperationEffect::Compensatable => crate::record::Effect::Compensatable,
            OperationEffect::Irreversible => crate::record::Effect::NonIdempotent,
        }
    }
}

/// How an operation shapes the response before handing it to the caller.
///
/// Default is body passthrough under the byte ceiling; `projection` is an
/// optional JSON-pointer field selection (`/data/issues/0/id`, in the
/// shape [`serde_json::Value::pointer`] resolves) applied after parsing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponseExtraction {
    /// JSON-pointer projection into the parsed response, if any.
    pub projection: Option<String>,
    /// Per-operation response byte ceiling, tightening (never exceeding)
    /// [`MAX_HTTP_API_RESPONSE_BYTES`].
    pub max_bytes: Option<usize>,
}

/// One declared REST/GraphQL operation of an `http-api` connector.
///
/// The operation is the catalog atom: a valid operation derives exactly
/// one [`crate::tool::ToolCapability`] named `<connector-id>/<name>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HttpApiOperation {
    /// Operation name, kebab-case (`[a-z0-9]+(-[a-z0-9]+)*`).
    pub name: String,
    /// Human/model-facing description of the action.
    pub description: String,
    /// The HTTP method.
    pub method: HttpMethod,
    /// Path template relative to the base URL, with `{param}` placeholders
    /// (e.g. `/v1/issues/{issue_id}`). Must start with `/`. Literal braces
    /// are written `{{` and `}}`.
    pub path: String,
    /// JSON-schema object covering *every* parameter the operation routes
    /// (path, query, and body). Validation is closed in both directions:
    /// a placeholder the schema does not declare is rejected, and a schema
    /// property routed nowhere is rejected.
    pub params_schema: Value,
    /// Parameter names sent as query-string pairs.
    pub query_params: Vec<String>,
    /// The request body style.
    pub body: OperationBody,
    /// The declared effect classification.
    pub effect: OperationEffect,
    /// Response shaping.
    pub response: ResponseExtraction,
    /// Per-operation timeout override in milliseconds.
    pub timeout_ms: Option<u64>,
    /// The idempotency-key header a POST operation supports. When
    /// declared, dispatch generates a deterministic key from
    /// `(instance, operation, canonical args)` so retries cannot
    /// double-create. POST only, and required exactly when `effect` is
    /// [`OperationEffect::Idempotent`].
    pub idempotency_key_header: Option<String>,
}

/// Provider configuration for a generic HTTP REST/GraphQL API connector
/// — the foundation service packs (ServiceNow, Gmail, Slack, Linear,
/// Notion, Google Calendar) are declared against.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HttpApiSpec {
    /// The API root (e.g. `https://api.example.com`). `https://` only —
    /// operations egress tenant data and credentials, so plaintext
    /// transport is rejected at declaration. No query string or fragment:
    /// query material belongs to operations and auth styles.
    ///
    /// `{param}` placeholders (e.g. `https://{instance}.service-now.com`)
    /// name the manifest's [`ConnectorManifest::config_params`]; the
    /// instance's non-secret config values substitute in at request time.
    /// A placeholder naming an undeclared param fails declaration, so a
    /// literal-only base URL needs no config at all.
    pub base_url: String,
    /// How calls authenticate, or `None` for a public endpoint.
    pub auth: Option<HttpApiAuth>,
    /// Default headers sent on every call (bounded; `authorization` and
    /// `content-type` are reserved — auth injects the former, the provider
    /// the latter when a body is present). Sorted by name at construction.
    pub default_headers: Vec<(String, String)>,
    /// The operation `connect` runs as a health check, when the host has
    /// wired a transport for it. Must name a parameterless ReadOnly GET.
    pub health_check: Option<String>,
    /// The declared operations.
    pub operations: Vec<HttpApiOperation>,
}
/// The provider kind realizing the connector.
///
/// Serialized with an internal `kind` tag (`mcp_stdio` / `http_search` /
/// `http_api`); an unknown tag fails deserialization, so new provider
/// kinds cannot slip through old binaries unexamined.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProviderKind {
    /// An MCP server spawned as a child process over stdio.
    McpStdio(McpStdioSpec),
    /// A bounded HTTP web-search endpoint.
    HttpSearch(HttpSearchSpec),
    /// A generic HTTP REST/GraphQL API described as an operations list.
    HttpApi(HttpApiSpec),
}

/// The declared, content-addressed connector contract.
///
/// Construct through [`ConnectorManifest::new`], which validates every
/// field and computes [`ConnectorManifest::hash`]. Deserialization bypasses
/// construction, so the admission boundary
/// ([`super::ConnectorRegistry::register_manifest`]) re-validates and
/// re-verifies the hash — a tampered or oversized manifest fails there
/// even if it arrived over a channel that never called `new`.
///
/// `PartialEq` but not `Eq`: `http-api` operations carry a
/// `serde_json::Value` params schema, which is not `Eq`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectorManifest {
    /// Stable connector id, kebab-case (`[a-z0-9]+(-[a-z0-9]+)*`).
    pub id: String,
    /// Manifest version string (opaque; committed to by the hash).
    pub version: String,
    /// Human-facing display name.
    pub display_name: String,
    /// Human-facing description of what the connector provides.
    pub description: String,
    /// The provider kind and its configuration.
    pub provider: ProviderKind,
    /// Declared capability summary (e.g. `"web search"`, `"mcp tools"`).
    /// A declaration for review surfaces; the executable truth is the
    /// derived catalog. Sorted and deduplicated at construction.
    pub capabilities: Vec<String>,
    /// Credential slots an instance requires.
    pub credential_slots: Vec<CredentialSlot>,
    /// Non-secret config params an instance supplies (instance identity,
    /// region, …). Defaults to empty so manifests declared before this
    /// field existed deserialize unchanged; omitted from serialization
    /// when empty, which keeps their content hashes stable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub config_params: Vec<ConfigParam>,
    /// SHA-256 of the canonical serialization of every field above.
    pub hash: String,
}

/// The canonical content view: every field except the hash itself.
#[derive(Serialize)]
struct ManifestContent<'a> {
    id: &'a str,
    version: &'a str,
    display_name: &'a str,
    description: &'a str,
    provider: &'a ProviderKind,
    capabilities: &'a [String],
    credential_slots: &'a [CredentialSlot],
    #[serde(skip_serializing_if = "<[ConfigParam]>::is_empty")]
    config_params: &'a [ConfigParam],
}

impl ConnectorManifest {
    /// Validate and construct a manifest, computing its content hash.
    ///
    /// `capabilities`, `credential_slots`, and the env allowlist are
    /// canonicalized (sorted/deduplicated) so semantically equal manifests
    /// hash equal regardless of declaration order. Config params attach
    /// through [`ConnectorManifest::new_with_config`] — they must be
    /// present at construction, since an `http-api` base-url placeholder
    /// validates against them.
    pub fn new(
        id: impl Into<String>,
        version: impl Into<String>,
        display_name: impl Into<String>,
        description: impl Into<String>,
        provider: ProviderKind,
        capabilities: Vec<String>,
        credential_slots: Vec<CredentialSlot>,
    ) -> Result<Self> {
        Self::new_with_config(
            id,
            version,
            display_name,
            description,
            provider,
            capabilities,
            credential_slots,
            Vec::new(),
        )
    }

    /// [`ConnectorManifest::new`] with the non-secret config params
    /// declared. Sorted by name during canonicalization; an `http-api`
    /// base-url `{placeholder}` that names none of them fails validation.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_config(
        id: impl Into<String>,
        version: impl Into<String>,
        display_name: impl Into<String>,
        description: impl Into<String>,
        mut provider: ProviderKind,
        mut capabilities: Vec<String>,
        mut credential_slots: Vec<CredentialSlot>,
        mut config_params: Vec<ConfigParam>,
    ) -> Result<Self> {
        capabilities.sort();
        capabilities.dedup();
        credential_slots.sort_by(|left, right| left.name.cmp(&right.name));
        config_params.sort_by(|left, right| left.name.cmp(&right.name));
        match &mut provider {
            ProviderKind::McpStdio(spec) => {
                spec.env_allowlist.sort();
                spec.env_allowlist.dedup();
            }
            ProviderKind::HttpApi(spec) => canonicalize_http_api(spec),
            ProviderKind::HttpSearch(_) => {}
        }
        let mut manifest = Self {
            id: id.into(),
            version: version.into(),
            display_name: display_name.into(),
            description: description.into(),
            provider,
            capabilities,
            credential_slots,
            config_params,
            hash: String::new(),
        };
        manifest.validate()?;
        manifest.hash = manifest.compute_hash();
        Ok(manifest)
    }

    /// The content hash: SHA-256 over the canonical serialization of the
    /// manifest content (everything except `hash`).
    fn compute_hash(&self) -> String {
        let content = ManifestContent {
            id: &self.id,
            version: &self.version,
            display_name: &self.display_name,
            description: &self.description,
            provider: &self.provider,
            capabilities: &self.capabilities,
            credential_slots: &self.credential_slots,
            config_params: &self.config_params,
        };
        // Serializing this view is infallible: every field is a string, a
        // string vec, or an already-serializable spec struct.
        let value =
            serde_json::to_value(&content).expect("the manifest content view always serializes");
        super::canonical_json_hash(&value)
    }

    /// `true` if the stored hash matches a recomputation over the current
    /// content. Registration requires this; a deserialized manifest whose
    /// content was edited after hashing fails here.
    pub fn verify_hash(&self) -> bool {
        !self.hash.is_empty() && self.hash == self.compute_hash()
    }

    /// Strict structural validation. Fails closed on the first violation.
    pub fn validate(&self) -> Result<()> {
        validate_connector_id(&self.id)?;
        validate_version(&self.version)?;
        validate_text_field(
            "display_name",
            &self.display_name,
            MAX_DISPLAY_NAME_LEN,
            false,
        )?;
        validate_text_field("description", &self.description, MAX_DESCRIPTION_LEN, false)?;

        if self.capabilities.len() > MAX_DECLARED_CAPABILITIES {
            return Err(conn_err(format!(
                "manifest `{}` declares {} capabilities, above the {MAX_DECLARED_CAPABILITIES} cap",
                self.id,
                self.capabilities.len()
            )));
        }
        for entry in &self.capabilities {
            validate_text_field("capability", entry, MAX_CAPABILITY_SUMMARY_LEN, false)?;
        }

        if self.credential_slots.len() > MAX_CREDENTIAL_SLOTS {
            return Err(conn_err(format!(
                "manifest `{}` declares {} credential slots, above the {MAX_CREDENTIAL_SLOTS} cap",
                self.id,
                self.credential_slots.len()
            )));
        }
        for (index, slot) in self.credential_slots.iter().enumerate() {
            validate_slot_name(&slot.name)?;
            validate_text_field(
                "credential slot description",
                &slot.description,
                MAX_SLOT_DESCRIPTION_LEN,
                true,
            )?;
            if self.credential_slots[..index]
                .iter()
                .any(|s| s.name == slot.name)
            {
                return Err(conn_err(format!(
                    "manifest `{}` declares credential slot `{}` twice",
                    self.id, slot.name
                )));
            }
        }

        if self.config_params.len() > MAX_CONFIG_PARAMS {
            return Err(conn_err(format!(
                "manifest `{}` declares {} config params, above the {MAX_CONFIG_PARAMS} cap",
                self.id,
                self.config_params.len()
            )));
        }
        for (index, param) in self.config_params.iter().enumerate() {
            // The placeholder charset: a param name must be spellable as
            // `{name}` in a base-url template.
            validate_param_name(&param.name)?;
            validate_text_field(
                "config param description",
                &param.description,
                MAX_CONFIG_PARAM_DESCRIPTION_LEN,
                true,
            )?;
            if self.config_params[..index]
                .iter()
                .any(|p| p.name == param.name)
            {
                return Err(conn_err(format!(
                    "manifest `{}` declares config param `{}` twice",
                    self.id, param.name
                )));
            }
            if self.credential_slots.iter().any(|s| s.name == param.name) {
                return Err(conn_err(format!(
                    "manifest `{}` declares `{}` as both a credential slot and a config param",
                    self.id, param.name
                )));
            }
        }

        match &self.provider {
            ProviderKind::McpStdio(spec) => self.validate_mcp_stdio(spec),
            ProviderKind::HttpSearch(spec) => self.validate_http_search(spec),
            ProviderKind::HttpApi(spec) => self.validate_http_api(spec),
        }
    }

    fn validate_mcp_stdio(&self, spec: &McpStdioSpec) -> Result<()> {
        if spec.command.is_empty()
            || spec.command.len() > MAX_COMMAND_LEN
            || spec.command.chars().any(char::is_control)
        {
            return Err(conn_err(format!(
                "manifest `{}` mcp-stdio command must be non-empty, control-free, and at most {MAX_COMMAND_LEN} bytes",
                self.id
            )));
        }
        if spec.args.len() > MAX_ARGS {
            return Err(conn_err(format!(
                "manifest `{}` declares {} arguments, above the {MAX_ARGS} cap",
                self.id,
                spec.args.len()
            )));
        }
        for arg in &spec.args {
            if arg.len() > MAX_ARG_LEN || arg.chars().any(char::is_control) {
                return Err(conn_err(format!(
                    "manifest `{}` argument must be control-free and at most {MAX_ARG_LEN} bytes",
                    self.id
                )));
            }
        }
        if spec.env_allowlist.len() > MAX_ENV_ALLOWLIST {
            return Err(conn_err(format!(
                "manifest `{}` allowlists {} environment variables, above the {MAX_ENV_ALLOWLIST} cap",
                self.id,
                spec.env_allowlist.len()
            )));
        }
        for name in &spec.env_allowlist {
            let valid = !name.is_empty()
                && name.len() <= MAX_ENV_NAME_LEN
                && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                && !name.as_bytes()[0].is_ascii_digit();
            if !valid {
                return Err(conn_err(format!(
                    "manifest `{}` env allowlist entry `{name}` is not a valid environment variable name",
                    self.id
                )));
            }
        }
        Ok(())
    }

    fn validate_http_search(&self, spec: &HttpSearchSpec) -> Result<()> {
        if !spec.base_url.starts_with("https://") || spec.base_url.len() > MAX_BASE_URL_LEN {
            return Err(conn_err(format!(
                "manifest `{}` http-search base URL must be an `https://` URL of at most {MAX_BASE_URL_LEN} bytes",
                self.id
            )));
        }
        if spec.base_url.chars().any(char::is_control)
            || spec.base_url.contains(char::is_whitespace)
        {
            return Err(conn_err(format!(
                "manifest `{}` http-search base URL must not contain whitespace or control characters",
                self.id
            )));
        }
        if let Some(auth) = &spec.auth {
            let valid_header = !auth.header.is_empty()
                && auth.header.len() <= MAX_AUTH_HEADER_LEN
                && auth
                    .header
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-');
            if !valid_header {
                return Err(conn_err(format!(
                    "manifest `{}` auth header `{}` is not a valid HTTP header name",
                    self.id, auth.header
                )));
            }
            if !self
                .credential_slots
                .iter()
                .any(|slot| slot.name == auth.credential_slot)
            {
                return Err(conn_err(format!(
                    "manifest `{}` auth references undeclared credential slot `{}`",
                    self.id, auth.credential_slot
                )));
            }
        }
        Ok(())
    }

    fn validate_http_api(&self, spec: &HttpApiSpec) -> Result<()> {
        if !spec.base_url.starts_with("https://")
            || spec.base_url.len() == "https://".len()
            || spec.base_url.len() > MAX_BASE_URL_LEN
        {
            return Err(conn_err(format!(
                "manifest `{}` http-api base URL must be an `https://` URL of at most {MAX_BASE_URL_LEN} bytes",
                self.id
            )));
        }
        if spec.base_url.chars().any(char::is_control)
            || spec.base_url.contains(char::is_whitespace)
            || spec.base_url.contains('?')
            || spec.base_url.contains('#')
        {
            return Err(conn_err(format!(
                "manifest `{}` http-api base URL must not contain whitespace, control characters, a query string, or a fragment",
                self.id
            )));
        }
        // Every `{param}` placeholder names a declared config param — an
        // instance can only supply what the manifest declared, so a typo'd
        // placeholder fails here, not at request time.
        for name in extract_placeholders(&spec.base_url)? {
            if !self.config_params.iter().any(|param| param.name == name) {
                return Err(conn_err(format!(
                    "manifest `{}` http-api base URL placeholder `{{{name}}}` names no declared config param",
                    self.id
                )));
            }
        }

        if spec.default_headers.len() > MAX_DEFAULT_HEADERS {
            return Err(conn_err(format!(
                "manifest `{}` declares {} default headers, above the {MAX_DEFAULT_HEADERS} cap",
                self.id,
                spec.default_headers.len()
            )));
        }
        for (index, (name, value)) in spec.default_headers.iter().enumerate() {
            validate_header_name(&self.id, name)?;
            if value.len() > MAX_HEADER_VALUE_LEN || value.chars().any(char::is_control) {
                return Err(conn_err(format!(
                    "manifest `{}` default header `{name}` value must be control-free and at most {MAX_HEADER_VALUE_LEN} bytes",
                    self.id
                )));
            }
            // Header names are case-insensitive on the wire; `authorization`
            // belongs to the auth style and `content-type` to the provider.
            let lowered = name.to_ascii_lowercase();
            if lowered == "authorization" || lowered == "content-type" {
                return Err(conn_err(format!(
                    "manifest `{}` default header `{name}` is reserved (authorization and content-type are provider-managed)",
                    self.id
                )));
            }
            if spec.default_headers[..index]
                .iter()
                .any(|(other, _)| other.eq_ignore_ascii_case(name))
            {
                return Err(conn_err(format!(
                    "manifest `{}` declares default header `{name}` twice",
                    self.id
                )));
            }
        }

        if let Some(auth) = &spec.auth {
            match auth {
                HttpApiAuth::Header { header, .. } => {
                    validate_header_name(&self.id, header)?;
                    if spec
                        .default_headers
                        .iter()
                        .any(|(name, _)| name.eq_ignore_ascii_case(header))
                    {
                        return Err(conn_err(format!(
                            "manifest `{}` default headers collide with the auth header `{header}`",
                            self.id
                        )));
                    }
                }
                HttpApiAuth::QueryParam { param, .. } => {
                    validate_query_param_name(&self.id, param)?;
                }
                HttpApiAuth::Basic {
                    username_slot,
                    password_slot,
                } => {
                    if username_slot == password_slot {
                        return Err(conn_err(format!(
                            "manifest `{}` basic auth must reference two distinct credential slots",
                            self.id
                        )));
                    }
                }
                HttpApiAuth::BearerToken { .. } => {}
            }
            for slot in auth.referenced_slots() {
                if !self.credential_slots.iter().any(|s| s.name == slot) {
                    return Err(conn_err(format!(
                        "manifest `{}` auth references undeclared credential slot `{slot}`",
                        self.id
                    )));
                }
            }
        }

        if spec.operations.is_empty() || spec.operations.len() > MAX_OPERATIONS {
            return Err(conn_err(format!(
                "manifest `{}` must declare 1..={MAX_OPERATIONS} operations, not {}",
                self.id,
                spec.operations.len()
            )));
        }
        for (index, operation) in spec.operations.iter().enumerate() {
            self.validate_http_api_operation(operation)?;
            if spec.operations[..index]
                .iter()
                .any(|other| other.name == operation.name)
            {
                return Err(conn_err(format!(
                    "manifest `{}` declares operation `{}` twice",
                    self.id, operation.name
                )));
            }
        }

        if let Some(health) = &spec.health_check {
            let operation = spec
                .operations
                .iter()
                .find(|op| &op.name == health)
                .ok_or_else(|| {
                    conn_err(format!(
                        "manifest `{}` health check names undeclared operation `{health}`",
                        self.id
                    ))
                })?;
            if operation.method != HttpMethod::Get || operation.effect != OperationEffect::ReadOnly
            {
                return Err(conn_err(format!(
                    "manifest `{}` health check `{health}` must name a read-only GET operation",
                    self.id
                )));
            }
            if !extract_placeholders(&operation.path)?.is_empty() {
                return Err(conn_err(format!(
                    "manifest `{}` health check `{health}` must not take path parameters (connect supplies no arguments)",
                    self.id
                )));
            }
            let has_required = operation
                .params_schema
                .get("required")
                .and_then(Value::as_array)
                .is_some_and(|required| !required.is_empty());
            if has_required {
                return Err(conn_err(format!(
                    "manifest `{}` health check `{health}` must not declare required parameters (connect supplies no arguments)",
                    self.id
                )));
            }
        }
        Ok(())
    }

    fn validate_http_api_operation(&self, operation: &HttpApiOperation) -> Result<()> {
        let segments = operation.name.split('-');
        let valid_name = !operation.name.is_empty()
            && operation.name.len() <= MAX_OPERATION_NAME_LEN
            && segments.clone().all(|segment| {
                !segment.is_empty()
                    && segment
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
            });
        if !valid_name {
            return Err(conn_err(format!(
                "manifest `{}` operation name `{}` must be kebab-case (`[a-z0-9]+(-[a-z0-9]+)*`) of at most {MAX_OPERATION_NAME_LEN} bytes",
                self.id, operation.name
            )));
        }
        if self.id.len() + 1 + operation.name.len() > super::provider::MAX_DERIVED_TOOL_NAME_LEN {
            return Err(conn_err(format!(
                "manifest `{}` operation `{}` derives a catalog name above the {}-byte cap",
                self.id,
                operation.name,
                super::provider::MAX_DERIVED_TOOL_NAME_LEN
            )));
        }
        validate_text_field(
            "operation description",
            &operation.description,
            MAX_OPERATION_DESCRIPTION_LEN,
            false,
        )?;

        if !operation.path.starts_with('/')
            || operation.path.len() > MAX_PATH_TEMPLATE_LEN
            || operation.path.chars().any(char::is_control)
            || operation.path.contains(char::is_whitespace)
            || operation.path.contains('?')
            || operation.path.contains('#')
        {
            return Err(conn_err(format!(
                "manifest `{}` operation `{}` path must start with `/`, carry no query/fragment, be whitespace- and control-free, and be at most {MAX_PATH_TEMPLATE_LEN} bytes",
                self.id, operation.name
            )));
        }
        let path_params = extract_placeholders(&operation.path)?;

        if !operation.params_schema.is_object() {
            return Err(conn_err(format!(
                "manifest `{}` operation `{}` params schema must be a JSON object",
                self.id, operation.name
            )));
        }
        let schema_bytes = serde_json::to_vec(&operation.params_schema).map_err(|e| {
            conn_err(format!(
                "manifest `{}` operation `{}` params schema did not serialize: {e}",
                self.id, operation.name
            ))
        })?;
        if schema_bytes.len() > MAX_OPERATION_SCHEMA_BYTES {
            return Err(conn_err(format!(
                "manifest `{}` operation `{}` params schema exceeds {MAX_OPERATION_SCHEMA_BYTES} bytes",
                self.id, operation.name
            )));
        }
        let properties = match operation.params_schema.get("properties") {
            None => None,
            Some(value) => Some(value.as_object().ok_or_else(|| {
                conn_err(format!(
                    "manifest `{}` operation `{}` schema `properties` must be an object",
                    self.id, operation.name
                ))
            })?),
        };
        let declares = |name: &str| properties.is_some_and(|props| props.contains_key(name));
        if let Some(required) = operation.params_schema.get("required") {
            let entries = required.as_array().ok_or_else(|| {
                conn_err(format!(
                    "manifest `{}` operation `{}` schema `required` must be an array",
                    self.id, operation.name
                ))
            })?;
            for entry in entries {
                let name = entry.as_str().ok_or_else(|| {
                    conn_err(format!(
                        "manifest `{}` operation `{}` schema `required` entries must be strings",
                        self.id, operation.name
                    ))
                })?;
                if !declares(name) {
                    return Err(conn_err(format!(
                        "manifest `{}` operation `{}` schema requires undeclared property `{name}`",
                        self.id, operation.name
                    )));
                }
            }
        }

        // Route every parameter to exactly one location — path, query, or
        // body — and require the schema to declare each. A placeholder the
        // schema does not cover, or a property routed nowhere, both fail.
        let mut routed: std::collections::BTreeMap<&str, &str> = std::collections::BTreeMap::new();
        for name in &path_params {
            route_param(
                &self.id,
                &operation.name,
                properties,
                &mut routed,
                name,
                "path",
            )?;
        }
        for name in &operation.query_params {
            route_param(
                &self.id,
                &operation.name,
                properties,
                &mut routed,
                name,
                "query",
            )?;
        }
        let body_params: Vec<String> = match &operation.body {
            OperationBody::None => Vec::new(),
            OperationBody::Json { params } => {
                if params.is_empty() {
                    return Err(conn_err(format!(
                        "manifest `{}` operation `{}` json body must name at least one parameter",
                        self.id, operation.name
                    )));
                }
                if !operation.method.allows_body() {
                    return Err(conn_err(format!(
                        "manifest `{}` operation `{}` is {} and cannot carry a request body",
                        self.id,
                        operation.name,
                        operation.method.as_str()
                    )));
                }
                params.clone()
            }
            OperationBody::Graphql { query } => {
                if query.is_empty() || query.len() > MAX_GRAPHQL_TEMPLATE_BYTES {
                    return Err(conn_err(format!(
                        "manifest `{}` operation `{}` graphql query template must be non-empty and at most {MAX_GRAPHQL_TEMPLATE_BYTES} bytes",
                        self.id, operation.name
                    )));
                }
                if operation.method != HttpMethod::Post {
                    return Err(conn_err(format!(
                        "manifest `{}` operation `{}` graphql bodies require POST",
                        self.id, operation.name
                    )));
                }
                extract_placeholders(query)?
            }
        };
        for name in &body_params {
            route_param(
                &self.id,
                &operation.name,
                properties,
                &mut routed,
                name,
                "body",
            )?;
        }
        if let Some(props) = properties {
            for name in props.keys() {
                if !routed.contains_key(name.as_str()) {
                    return Err(conn_err(format!(
                        "manifest `{}` operation `{}` schema property `{name}` is routed nowhere (not a path, query, or body parameter)",
                        self.id, operation.name
                    )));
                }
            }
        }

        // Effect/method honesty: GET reads, DELETE is irreversible, and
        // `Idempotent` exists only as a keyed POST — the key header is what
        // the claim means at the wire.
        match (operation.method, operation.effect) {
            (HttpMethod::Get, OperationEffect::ReadOnly) => {}
            (HttpMethod::Get, effect) => {
                return Err(conn_err(format!(
                    "manifest `{}` operation `{}` is GET and must be `read_only`, not `{effect:?}`",
                    self.id, operation.name
                )));
            }
            (HttpMethod::Delete, OperationEffect::Irreversible) => {}
            (HttpMethod::Delete, effect) => {
                return Err(conn_err(format!(
                    "manifest `{}` operation `{}` is DELETE and must be `irreversible`, not `{effect:?}`",
                    self.id, operation.name
                )));
            }
            (_, OperationEffect::ReadOnly) => {
                return Err(conn_err(format!(
                    "manifest `{}` operation `{}` declares `read_only` on a {} — only GET reads",
                    self.id,
                    operation.name,
                    operation.method.as_str()
                )));
            }
            (HttpMethod::Post, OperationEffect::Idempotent) => {}
            (_, OperationEffect::Idempotent) => {
                return Err(conn_err(format!(
                    "manifest `{}` operation `{}` declares `idempotent` on a {} — only a keyed POST can be",
                    self.id,
                    operation.name,
                    operation.method.as_str()
                )));
            }
            _ => {}
        }
        match (&operation.idempotency_key_header, operation.effect) {
            (Some(header), OperationEffect::Idempotent) => {
                validate_header_name(&self.id, header)?;
            }
            (Some(header), _) => {
                return Err(conn_err(format!(
                    "manifest `{}` operation `{}` declares idempotency-key header `{header}` without an `idempotent` effect",
                    self.id, operation.name
                )));
            }
            (None, OperationEffect::Idempotent) => {
                return Err(conn_err(format!(
                    "manifest `{}` operation `{}` is `idempotent` and must declare its idempotency-key header",
                    self.id, operation.name
                )));
            }
            (None, _) => {}
        }

        if let Some(projection) = &operation.response.projection {
            if !projection.starts_with('/')
                || projection.len() > MAX_PROJECTION_LEN
                || projection.chars().any(char::is_control)
                || projection.contains(char::is_whitespace)
            {
                return Err(conn_err(format!(
                    "manifest `{}` operation `{}` projection must be a JSON pointer (`/...`), whitespace- and control-free, at most {MAX_PROJECTION_LEN} bytes",
                    self.id, operation.name
                )));
            }
        }
        if let Some(max_bytes) = operation.response.max_bytes {
            if max_bytes == 0 || max_bytes > MAX_HTTP_API_RESPONSE_BYTES {
                return Err(conn_err(format!(
                    "manifest `{}` operation `{}` response ceiling must be within 1..={MAX_HTTP_API_RESPONSE_BYTES} bytes",
                    self.id, operation.name
                )));
            }
        }
        if let Some(timeout) = operation.timeout_ms {
            if timeout == 0 || timeout > MAX_HTTP_API_TIMEOUT_MS {
                return Err(conn_err(format!(
                    "manifest `{}` operation `{}` timeout must be within 1..={MAX_HTTP_API_TIMEOUT_MS} ms",
                    self.id, operation.name
                )));
            }
        }
        Ok(())
    }
}

/// Kebab-case connector ids: `[a-z0-9]+(-[a-z0-9]+)*`, bounded.
fn validate_connector_id(id: &str) -> Result<()> {
    let segments = id.split('-');
    let valid = !id.is_empty()
        && id.len() <= MAX_CONNECTOR_ID_LEN
        && segments.clone().all(|segment| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        });
    if !valid {
        return Err(conn_err(format!(
            "connector id `{id}` must be kebab-case (`[a-z0-9]+(-[a-z0-9]+)*`) of at most {MAX_CONNECTOR_ID_LEN} bytes"
        )));
    }
    Ok(())
}

/// Version strings: opaque but bounded and printable.
fn validate_version(version: &str) -> Result<()> {
    let valid = !version.is_empty()
        && version.len() <= MAX_VERSION_LEN
        && version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+' | b'_'));
    if !valid {
        return Err(conn_err(format!(
            "version `{version}` must be 1..={MAX_VERSION_LEN} ASCII letters, digits, `.`, `-`, `+`, or `_`"
        )));
    }
    Ok(())
}

/// Credential slot names: `[a-z][a-z0-9_]*`, bounded.
fn validate_slot_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.len() <= MAX_SLOT_NAME_LEN
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        && name.as_bytes()[0].is_ascii_lowercase();
    if !valid {
        return Err(conn_err(format!(
            "credential slot name `{name}` must match `[a-z][a-z0-9_]*` and be at most {MAX_SLOT_NAME_LEN} bytes"
        )));
    }
    Ok(())
}

/// Shared rule for human-facing text: trimmed, control-free, bounded.
/// `allow_empty` distinguishes notes (may be empty) from primary fields.
/// `pub(crate)` so the composer plane validates drafted tool definitions
/// against exactly this rule instead of restating it.
pub(crate) fn validate_text_field(
    field: &str,
    value: &str,
    max_bytes: usize,
    allow_empty: bool,
) -> Result<()> {
    if (!allow_empty && value.is_empty())
        || value != value.trim()
        || value.len() > max_bytes
        || value.chars().any(char::is_control)
    {
        return Err(conn_err(format!(
            "{field} must be {}trimmed, control-free, and at most {max_bytes} bytes",
            if allow_empty { "" } else { "non-empty, " }
        )));
    }
    Ok(())
}

/// Canonicalize an `http-api` spec so semantically equal manifests hash
/// equal: operations sort by name, parameter lists and default headers
/// sort and deduplicate. Called by [`ConnectorManifest::new`] before
/// validation, so duplicate declarations still fail validation on the
/// deduplicated view only when they survive it — exact duplicates of a
/// query parameter are semantically one entry, and collapse.
fn canonicalize_http_api(spec: &mut HttpApiSpec) {
    spec.default_headers
        .sort_by(|left, right| left.0.cmp(&right.0));
    spec.operations
        .sort_by(|left, right| left.name.cmp(&right.name));
    for operation in &mut spec.operations {
        operation.query_params.sort();
        operation.query_params.dedup();
        if let OperationBody::Json { params } = &mut operation.body {
            params.sort();
            params.dedup();
        }
    }
}

/// Extract the `{param}` placeholder names from a path or GraphQL
/// template, in order of appearance. `{{` and `}}` are literal braces
/// (GraphQL selection sets are full of them); every other brace must form
/// a well-formed placeholder. Fail-closed: a stray or unclosed brace, a
/// nested placeholder, or an invalid parameter name is an error, never a
/// silently literal brace.
pub(crate) fn extract_placeholders(template: &str) -> Result<Vec<String>> {
    let mut names = Vec::new();
    let bytes = template.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'{' if bytes.get(index + 1) == Some(&b'{') => index += 2,
            b'}' if bytes.get(index + 1) == Some(&b'}') => index += 2,
            b'{' => {
                let close = template[index + 1..]
                    .find('}')
                    .map(|offset| index + 1 + offset)
                    .ok_or_else(|| conn_err("template has an unclosed `{` placeholder"))?;
                let name = &template[index + 1..close];
                if name.contains('{') {
                    return Err(conn_err("template placeholders cannot nest `{`"));
                }
                validate_param_name(name)?;
                names.push(name.to_owned());
                index = close + 1;
            }
            b'}' => {
                return Err(conn_err(
                    "template has a stray `}` (write `}}` for a literal brace)",
                ));
            }
            _ => index += 1,
        }
    }
    Ok(names)
}

/// Route one operation parameter to its location, fail-closed: the name
/// must be a valid parameter name, the schema must declare it, and no
/// other location may already carry it.
fn route_param<'a>(
    manifest_id: &str,
    operation_name: &str,
    properties: Option<&serde_json::Map<String, Value>>,
    routed: &mut std::collections::BTreeMap<&'a str, &'a str>,
    name: &'a str,
    location: &'a str,
) -> Result<()> {
    validate_param_name(name)?;
    if !properties.is_some_and(|props| props.contains_key(name)) {
        return Err(conn_err(format!(
            "manifest `{manifest_id}` operation `{operation_name}` routes `{location}` parameter `{name}`, which the params schema does not declare"
        )));
    }
    if let Some(previous) = routed.insert(name, location) {
        return Err(conn_err(format!(
            "manifest `{manifest_id}` operation `{operation_name}` routes parameter `{name}` to both `{previous}` and `{location}`"
        )));
    }
    Ok(())
}

/// Operation parameter names: `[a-zA-Z][a-zA-Z0-9_]*`, bounded. The
/// charset's job is identifier safety across path templates, query
/// strings, JSON bodies, and GraphQL placeholders — not casing policy:
/// real APIs spell camelCase (`maxResults`, `timeMin`, `addLabelIds`) and
/// a manifest must declare the wire's spelling exactly. (Credential slot
/// names stay `[a-z][a-z0-9_]*` — those are the host's keys, not the
/// wire's.) Config param names share this rule: a `{name}` placeholder is
/// how the name appears in a base-url template.
pub(crate) fn validate_param_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.len() <= MAX_SLOT_NAME_LEN
        && name
            .bytes()
            .all(|b| b.is_ascii_alphabetic() || b.is_ascii_digit() || b == b'_')
        && name.as_bytes()[0].is_ascii_alphabetic();
    if !valid {
        return Err(conn_err(format!(
            "parameter name `{name}` must match `[a-zA-Z][a-zA-Z0-9_]*` and be at most {MAX_SLOT_NAME_LEN} bytes"
        )));
    }
    Ok(())
}

/// HTTP header names: RFC 9110 tokens — ASCII letters, digits, or one of
/// the symbol characters in `!#$%&'*+-.^_\`|~` — bounded.
fn validate_header_name(manifest_id: &str, name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.len() <= MAX_AUTH_HEADER_LEN
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b));
    if !valid {
        return Err(conn_err(format!(
            "manifest `{manifest_id}` header name `{name}` is not a valid HTTP token of at most {MAX_AUTH_HEADER_LEN} bytes"
        )));
    }
    Ok(())
}

/// Query-parameter names: URL-unreserved ASCII (`[A-Za-z0-9-._~]`), so the
/// name crosses into the query string without encoding ambiguity.
fn validate_query_param_name(manifest_id: &str, name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.len() <= MAX_QUERY_PARAM_NAME_LEN
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'));
    if !valid {
        return Err(conn_err(format!(
            "manifest `{manifest_id}` query parameter `{name}` must use URL-unreserved characters and be at most {MAX_QUERY_PARAM_NAME_LEN} bytes"
        )));
    }
    Ok(())
}
