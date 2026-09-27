//! The connector manifest: identity, the `connection_specification`
//! schema, and the operation set — validated at declaration, hashed for
//! addressing.
//!
//! Placeholders. `base_url`, operation paths, header values, and auth
//! templates carry `{field}` placeholders; a field is a dot-separated
//! path of schema property names (`{instance}`, `{credentials.token}`).
//! Placeholders in `base_url`, headers, and auth templates resolve
//! against the **config** only; a path placeholder may additionally name
//! one of the operation's own declared params (call arguments — the
//! `{table}` in `/api/now/table/{table}`). Declaration validation checks
//! every placeholder against the schema (walking `properties`, and the
//! `oneOf` variants of a polymorphic sub-form — a placeholder is
//! declared when at least one variant declares it); rendering resolves
//! against the concrete config and fails closed on an absent field.
//!
//! Auth is declared per operation as an ordered list of alternatives
//! ([`OperationAuth`]): the first alternative whose templates fully
//! resolve against the config applies. This is what lets one operation
//! set serve a `oneOf` credential schema — a basic-auth instance renders
//! the `basic` alternative, a token instance renders `bearer` — without
//! per-variant operation declarations. Pure string substitution cannot
//! express `Basic base64(user:pass)`, so the encoding lives in the
//! declaration, not in a template filter language.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::canonical_json_hash;
use super::conn_err;
use crate::error::Result;

/// Maximum connector id length.
pub const MAX_CONNECTOR_ID_LEN: usize = 64;

/// Maximum version string length.
pub const MAX_VERSION_LEN: usize = 32;

/// Maximum display name length.
pub const MAX_DISPLAY_NAME_LEN: usize = 128;

/// Maximum description length.
pub const MAX_DESCRIPTION_LEN: usize = 4 * 1024;

/// Maximum documentation URL length.
pub const MAX_DOC_URL_LEN: usize = 2048;

/// Maximum base URL template length.
pub const MAX_BASE_URL_LEN: usize = 2048;

/// Maximum serialized size of the `connection_specification` schema.
pub const MAX_SPEC_BYTES: usize = 32 * 1024;

/// Maximum declared operations per manifest.
pub const MAX_OPERATIONS: usize = 64;

/// Maximum operation name length.
pub const MAX_OPERATION_NAME_LEN: usize = 64;

/// Maximum operation description length — the tool contract's cap, so a
/// derived catalog entry never exceeds what the executor accepts.
pub const MAX_OPERATION_DESCRIPTION_LEN: usize = crate::tool::MAX_TOOL_DESCRIPTION_BYTES;

/// Maximum path template length.
pub const MAX_PATH_TEMPLATE_LEN: usize = 512;

/// Maximum serialized size of one operation's params schema.
pub const MAX_OPERATION_SCHEMA_BYTES: usize = 16 * 1024;

/// Maximum declared headers per operation.
pub const MAX_HEADERS: usize = 16;

/// Maximum header value template length.
pub const MAX_HEADER_VALUE_LEN: usize = 1024;

/// Maximum auth alternatives per operation.
pub const MAX_AUTH_ALTERNATIVES: usize = 4;

/// The response byte ceiling every operation is bounded by — the
/// declared ceiling and the hard cap.
pub const MAX_RESPONSE_BYTES: usize = 256 * 1024;

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

    /// `true` when the method may carry a request body under this
    /// surface's rules (GET and DELETE never do — a body on either is a
    /// spec bug).
    pub fn allows_body(&self) -> bool {
        matches!(self, HttpMethod::Post | HttpMethod::Patch | HttpMethod::Put)
    }
}

/// The declared effect classification of one operation, mapped
/// one-to-one onto the effect kernel's wire [`crate::record::Effect`].
///
/// The declaration is explicit per operation — never inferred from the
/// method alone — but validation holds the two to an honest contract:
/// GETs are always [`OperationEffect::ReadOnly`] and DELETEs always
/// [`OperationEffect::Irreversible`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationEffect {
    /// Reads the world, writes nothing → [`crate::record::Effect::ReadOnly`].
    ReadOnly,
    /// Safe to retry → [`crate::record::Effect::Idempotent`].
    Idempotent,
    /// Duplicates on retry but has a logical undo →
    /// [`crate::record::Effect::Compensatable`].
    Compensatable,
    /// No safe repetition and no undo →
    /// [`crate::record::Effect::NonIdempotent`] (the kernel's
    /// *irreversible* rung).
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

/// The consent round trip a connector needs before anyone can use it.
///
/// Some systems cannot be configured, only *granted*: no field a person types
/// is a credential, because the credential is issued by the provider after a
/// person approves the request. Slack, Google, Microsoft and Atlassian all
/// work this way, and a `connection_specification` — which renders fields —
/// can no more express that than a form can express a conversation.
///
/// So the manifest declares the round trip: where to send the person, where
/// the code is exchanged, and what to ask for. What comes back is written into
/// the connection as sealed credentials, which is why the operations of such a
/// connector authenticate with an ordinary `{credentials.access_token}` — the
/// grant is a way of *obtaining* a config, not a new kind of call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Authorization {
    /// Where the person is sent to approve, templated over config.
    pub authorize_url: String,
    /// Where the authorization code is exchanged, templated over config.
    pub token_url: String,
    /// What to ask for, in the provider's own separator (usually spaces).
    pub scopes: String,
    /// The template naming the client id (`{credentials.client_id}`).
    pub client_id: String,
    /// The template naming the client secret (`{credentials.client_secret}`).
    pub client_secret: String,
    /// Extra query parameters the provider requires on the authorize URL —
    /// `access_type=offline` and the like. Values are templates too.
    #[serde(default)]
    pub extra_params: std::collections::BTreeMap<String, String>,
}

impl Authorization {
    /// Every template this block renders.
    pub(crate) fn templates(&self) -> Vec<&str> {
        let mut all = vec![
            self.authorize_url.as_str(),
            self.token_url.as_str(),
            self.client_id.as_str(),
            self.client_secret.as_str(),
        ];
        all.extend(self.extra_params.values().map(String::as_str));
        all
    }
}

/// One auth alternative an operation may render, as a template over the
/// config. Ordered on the operation: the first alternative whose
/// placeholders all resolve applies.
///
/// Templates name config fields — never raw secrets in the manifest.
/// The secret bytes resolve from the config at the moment of use and
/// appear only in the outbound auth material, never in errors or logs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "style", rename_all = "snake_case")]
pub enum OperationAuth {
    /// `Authorization: Basic base64(<username>:<password>)`.
    Basic {
        /// The username template (e.g. `{credentials.username}`).
        username: String,
        /// The password template (e.g. `{credentials.password}`).
        password: String,
    },
    /// `Authorization: Bearer <token>`.
    Bearer {
        /// The token template (e.g. `{credentials.token}`).
        token: String,
    },
    /// Custom header auth: `name: <value>`.
    Header {
        /// The header name (e.g. `X-API-Key`).
        name: String,
        /// The value template (e.g. `{credentials.api_key}`).
        value_template: String,
    },
    /// Query parameter auth: appended to the URL.
    Query {
        /// The query parameter name (e.g. `api_key`).
        name: String,
        /// The value template (e.g. `{credentials.api_key}`).
        value_template: String,
    },
    /// OAuth2 client credentials flow.
    // Named on the wire the way a connector author would write it. Without
    // this the derived name is `o_auth2_client_credentials`, which nobody
    // authoring a manifest by hand would ever guess.
    #[serde(
        rename = "oauth2_client_credentials",
        alias = "o_auth2_client_credentials"
    )]
    OAuth2ClientCredentials {
        /// The token endpoint URL template (e.g. `{token_url}`).
        token_url: String,
        /// The client id template (e.g. `{credentials.client_id}`).
        client_id_template: String,
        /// The client secret template (e.g. `{credentials.client_secret}`).
        client_secret_template: String,
        /// The scope template, optional (e.g. `{credentials.scope}`).
        scope_template: Option<String>,
    },
}

impl OperationAuth {
    /// Every template this alternative renders.
    pub(crate) fn templates(&self) -> Vec<&str> {
        match self {
            OperationAuth::Basic { username, password } => vec![username, password],
            OperationAuth::Bearer { token } => vec![token],
            OperationAuth::Header { value_template, .. } => vec![value_template],
            OperationAuth::Query { value_template, .. } => vec![value_template],
            OperationAuth::OAuth2ClientCredentials {
                token_url,
                client_id_template,
                client_secret_template,
                scope_template,
            } => {
                let mut v: Vec<&str> = vec![
                    token_url.as_str(),
                    client_id_template.as_str(),
                    client_secret_template.as_str(),
                ];
                if let Some(scope) = scope_template {
                    v.push(scope.as_str());
                }
                v
            }
        }
    }
}

/// One declared HTTP operation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectorOperation {
    /// Operation name, kebab-case; one catalog tool per operation, named
    /// `<connector-id>/<operation>`.
    pub name: String,
    /// Human/model-facing explanation of the action.
    pub description: String,
    /// The HTTP method.
    pub method: HttpMethod,
    /// The path template, appended to the manifest's `base_url`
    /// (`/api/now/table/{table}`).
    pub path: String,
    /// The declared effect classification.
    pub effect: OperationEffect,
    /// The call-arguments JSON Schema (an object schema; `{}` for a
    /// parameterless operation).
    pub params_schema: Value,
    /// Additional headers, values templated from config. Omitted when
    /// empty so equal manifests hash equal.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<(String, String)>,
    /// Ordered auth alternatives; the first that fully resolves against
    /// the config applies. Empty means the operation is unauthenticated.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub auth: Vec<OperationAuth>,
    /// Per-operation response byte ceiling, clamped to
    /// [`MAX_RESPONSE_BYTES`]; absent means the cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_response_bytes: Option<usize>,
    /// How to learn whether this write happened when its answer was lost:
    /// a read-only operation of the same connector and its arguments, a
    /// template over the write's own arguments (`"$short_description"` is
    /// the value the write sent). The tool runs it itself on a lost answer
    /// (see `agent_tool.rs`) — the record read back, or one safe re-send.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reconcile: Option<ReadBack>,
}

/// A write operation's read-back: the operation that finds what the write
/// would have made, and its arguments as a template over the write's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReadBack {
    pub operation: String,
    #[serde(default)]
    pub arguments: Value,
}

impl ConnectorOperation {
    /// Declare how this write checks itself when its answer is lost.
    pub fn with_read_back(mut self, operation: &str, arguments: Value) -> Self {
        self.reconcile = Some(ReadBack {
            operation: operation.to_owned(),
            arguments,
        });
        self
    }

    /// The response byte ceiling in force for this operation.
    pub fn response_ceiling(&self) -> usize {
        self.max_response_bytes
            .unwrap_or(MAX_RESPONSE_BYTES)
            .clamp(1, MAX_RESPONSE_BYTES)
    }

    /// `true` when the operation takes no call arguments — the shape the
    /// `check` operation must have.
    pub fn is_parameterless(&self) -> bool {
        let empty_or_absent = |key: &str| match self.params_schema.get(key) {
            None => true,
            Some(Value::Object(map)) => map.is_empty(),
            Some(Value::Array(list)) => list.is_empty(),
            Some(_) => false,
        };
        empty_or_absent("properties") && empty_or_absent("required")
    }
}

/// A content-addressed connector manifest: identity, the
/// `connection_specification` schema, and the operation set.
///
/// Construct through [`ConnectorManifest::new`] — validation and the hash
/// happen there. Deserialized manifests re-verify at registration
/// ([`ConnectorManifest::verify_hash`] plus [`ConnectorManifest::validate`]):
/// a tampered manifest fails there even if it arrived over a channel
/// that never called `new`.
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
    /// Link to the connector's documentation (https only).
    pub documentation_url: String,
    /// The API root, templated from config (`https://{instance}.service-now.com`).
    /// https only — checked on the template at declaration and on the
    /// rendered URL at call time.
    pub base_url: String,
    /// The configuration surface: a JSON Schema draft-07 document, an
    /// object schema at the root. Presentation hints ride the `rusty_*`
    /// extension keys (ignored by validators).
    pub connection_specification: Value,
    /// The declared operations, in canonical (name-sorted) order.
    pub operations: Vec<ConnectorOperation>,
    /// The name of the check operation — a parameterless read-only GET,
    /// executed with the candidate config as the setup/edit gate.
    pub check: String,
    /// The consent round trip, for a connector whose credentials are granted
    /// rather than typed. Absent for every connector that is only configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization: Option<Authorization>,
    /// SHA-256 of the canonical serialization of every field above.
    ///
    /// Derived, never chosen. It is absent from a hand-written manifest —
    /// the door that accepts one computes it — and present on every manifest
    /// the server hands back, where a caller can re-verify it.
    #[serde(default)]
    pub hash: String,
}

/// The canonical content view: every field except the hash itself.
#[derive(Serialize)]
struct ManifestContent<'a> {
    id: &'a str,
    version: &'a str,
    display_name: &'a str,
    description: &'a str,
    documentation_url: &'a str,
    base_url: &'a str,
    connection_specification: &'a Value,
    operations: &'a [ConnectorOperation],
    check: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    authorization: Option<&'a Authorization>,
}

impl ConnectorManifest {
    /// Validate and construct a manifest, computing its content hash.
    /// Operations are name-sorted at construction so semantically equal
    /// manifests hash equal regardless of declaration order.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: impl Into<String>,
        version: impl Into<String>,
        display_name: impl Into<String>,
        description: impl Into<String>,
        documentation_url: impl Into<String>,
        base_url: impl Into<String>,
        connection_specification: Value,
        mut operations: Vec<ConnectorOperation>,
        check: impl Into<String>,
    ) -> Result<Self> {
        operations.sort_by(|left, right| left.name.cmp(&right.name));
        let mut manifest = Self {
            id: id.into(),
            version: version.into(),
            display_name: display_name.into(),
            description: description.into(),
            documentation_url: documentation_url.into(),
            base_url: base_url.into(),
            connection_specification,
            operations,
            check: check.into(),
            authorization: None,
            hash: String::new(),
        };
        manifest.validate()?;
        manifest.hash = manifest.compute_hash();
        Ok(manifest)
    }

    /// Declare the consent round trip this connector needs. Separate from
    /// `new` because most connectors have none, and because the hash has to
    /// be recomputed over it — a connector that acquired a grant flow is not
    /// the same connector.
    pub fn with_authorization(mut self, authorization: Authorization) -> Result<Self> {
        self.authorization = Some(authorization);
        self.validate()?;
        self.hash = self.compute_hash();
        Ok(self)
    }

    /// The content hash: SHA-256 over the canonical serialization of the
    /// manifest content (everything except `hash`).
    /// Seal a manifest that was written by hand: validate it, put its
    /// operations in canonical order, and compute the content hash. This is
    /// what a JSON document posted by any client goes through, so a connector
    /// can be authored in a text field and not only in Rust.
    pub fn sealed(self) -> Result<Self> {
        Self::new(
            self.id,
            self.version,
            self.display_name,
            self.description,
            self.documentation_url,
            self.base_url,
            self.connection_specification,
            self.operations,
            self.check,
        )
        .and_then(|manifest| match self.authorization {
            Some(authorization) => manifest.with_authorization(authorization),
            None => Ok(manifest),
        })
    }

    fn compute_hash(&self) -> String {
        let content = ManifestContent {
            id: &self.id,
            version: &self.version,
            display_name: &self.display_name,
            description: &self.description,
            documentation_url: &self.documentation_url,
            base_url: &self.base_url,
            connection_specification: &self.connection_specification,
            operations: &self.operations,
            check: &self.check,
            authorization: self.authorization.as_ref(),
        };
        // Serializing this view is infallible: every field is a string or
        // an already-serializable value.
        let value =
            serde_json::to_value(&content).expect("the manifest content view always serializes");
        canonical_json_hash(&value)
    }

    /// `true` if the stored hash matches a recomputation over the current
    /// content. Registration requires this; a deserialized manifest whose
    /// content was edited after hashing fails here.
    pub fn verify_hash(&self) -> bool {
        !self.hash.is_empty() && self.hash == self.compute_hash()
    }

    /// One declared operation by name.
    pub fn operation(&self, name: &str) -> Option<&ConnectorOperation> {
        self.operations.iter().find(|op| op.name == name)
    }

    /// Strict structural validation. Fails closed on the first violation.
    pub fn validate(&self) -> Result<()> {
        validate_connector_id(&self.id)?;
        validate_text_field("version", &self.version, MAX_VERSION_LEN, false)?;
        validate_text_field(
            "display_name",
            &self.display_name,
            MAX_DISPLAY_NAME_LEN,
            false,
        )?;
        validate_text_field("description", &self.description, MAX_DESCRIPTION_LEN, false)?;
        validate_https_url(
            "documentation_url",
            &self.documentation_url,
            MAX_DOC_URL_LEN,
        )?;
        validate_https_url("base_url", &self.base_url, MAX_BASE_URL_LEN)?;

        let declared = declared_config_paths(&self.connection_specification)
            .map_err(|e| conn_err(format!("manifest `{}`: {e}", self.id)))?;
        // The schema must compile as draft-07 before anything else trusts
        // it — registration compiles it again for instance validation.
        super::config::compile_spec(&self.connection_specification)
            .map_err(|e| conn_err(format!("manifest `{}`: {e}", self.id)))?;
        // …and it must actually constrain. A specification that accepts any
        // object validates every config, including one shaped however a
        // caller found convenient — which is how a connector surface stops
        // being a surface. The spec is the contract every configuring
        // surface reads, so it has to say something.
        validate_spec_constrains(&self.id, &self.connection_specification)?;

        // Every base_url placeholder names a declared config property.
        check_template_placeholders("base_url", &self.base_url, &declared, &[], &self.id)?;

        if self.operations.is_empty() {
            return Err(conn_err(format!(
                "manifest `{}` declares no operations",
                self.id
            )));
        }
        if self.operations.len() > MAX_OPERATIONS {
            return Err(conn_err(format!(
                "manifest `{}` declares {} operations, above the {MAX_OPERATIONS} cap",
                self.id,
                self.operations.len()
            )));
        }
        let mut seen = std::collections::BTreeSet::new();
        for operation in &self.operations {
            validate_operation(operation, &declared, &self.id)?;
            if !seen.insert(&operation.name) {
                return Err(conn_err(format!(
                    "manifest `{}` declares operation `{}` twice",
                    self.id, operation.name
                )));
            }
        }

        // A grant flow is templated over the same config as everything else,
        // and it reaches the network — so its urls are https and its
        // placeholders name declared properties, exactly like base_url.
        if let Some(authorization) = &self.authorization {
            validate_https_url(
                "authorize_url",
                &authorization.authorize_url,
                MAX_BASE_URL_LEN,
            )?;
            validate_https_url("token_url", &authorization.token_url, MAX_BASE_URL_LEN)?;
            if authorization.scopes.trim().is_empty() {
                return Err(conn_err(format!(
                    "manifest `{}` declares a grant flow with no scopes — a consent screen that \
                     asks for nothing grants nothing",
                    self.id
                )));
            }
            for template in authorization.templates() {
                check_template_placeholders("authorization", template, &declared, &[], &self.id)?;
            }
        }

        // The check operation must exist, be parameterless, and be a
        // read-only GET — the shape a setup gate can execute with nothing
        // but the candidate config.
        let check = self.operation(&self.check).ok_or_else(|| {
            conn_err(format!(
                "manifest `{}` names check operation `{}`, which it does not declare",
                self.id, self.check
            ))
        })?;
        if check.method != HttpMethod::Get || check.effect != OperationEffect::ReadOnly {
            return Err(conn_err(format!(
                "manifest `{}` check operation `{}` must be a read-only GET",
                self.id, self.check
            )));
        }
        if !check.is_parameterless() {
            return Err(conn_err(format!(
                "manifest `{}` check operation `{}` must be parameterless",
                self.id, self.check
            )));
        }
        Ok(())
    }

    /// Derive the tool catalog: one [`crate::tool::ToolCapability`] per
    /// operation, namespaced `<connector-id>/<operation>`, the declared
    /// params schema passed through, the declared effect mapped onto the
    /// wire taxonomy. Sorted by name; deterministic for one manifest.
    pub fn derive_catalog(&self) -> Result<Vec<crate::tool::ToolCapability>> {
        let mut capabilities = Vec::with_capacity(self.operations.len().saturating_sub(1));
        for operation in &self.operations {
            if operation.name == self.check {
                continue;
            }
            // The name a builder picks is the name that runs: the same
            // `{connector}.{operation}` the agent registry offers. A dot,
            // because that is what every model provider accepts in a tool
            // name; a slash is not.
            let name = format!("{}.{}", self.id, operation.name);
            capabilities.push(crate::tool::ToolCapability {
                name,
                description: operation.description.clone(),
                parameters_schema: operation.params_schema.clone(),
                effect: operation.effect.wire_effect(),
                reconcile: operation
                    .reconcile
                    .as_ref()
                    .map(|r| format!("{}.{}", self.id, r.operation)),
            });
        }
        capabilities.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(capabilities)
    }
}

// --------------------------------------------------------------------- //

/// Scan a template for `{field}` placeholders, returning each field path
/// (dot-separated segments) in order of appearance. Fails on unbalanced
/// braces or an illegal field name (`[A-Za-z0-9_]` segments joined by
/// `.`) — a template this rejects is a declaration error, never silently
/// literal.
pub fn scan_placeholders(template: &str) -> Result<Vec<String>> {
    let bytes = template.as_bytes();
    let mut fields = Vec::new();
    let mut rest = 0;
    while rest < bytes.len() {
        if bytes[rest] != b'{' {
            rest += 1;
            continue;
        }
        let close = template[rest..]
            .find('}')
            .map(|offset| rest + offset)
            .ok_or_else(|| conn_err(format!("template `{template}` has an unbalanced `{{`")))?;
        let field = &template[rest + 1..close];
        let legal = !field.is_empty()
            && field.split('.').all(|segment| {
                !segment.is_empty()
                    && segment
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_')
            });
        if !legal {
            return Err(conn_err(format!(
                "template `{template}` carries illegal placeholder `{{{field}}}` — field paths are \
                 `[A-Za-z0-9_]` segments joined by `.`"
            )));
        }
        fields.push(field.to_owned());
        rest = close + 1;
    }
    Ok(fields)
}

/// Render a template against a config object: every `{field}` placeholder
/// resolves to the config value at that dot path. Scalars render as
/// themselves; a missing field or a structured value (object, array,
/// null) is an error naming the placeholder — the caller maps it to a
/// failed check or a 422, never a half-rendered request.
///
/// No percent-encoding happens here: a config value's legal alphabet is
/// the schema's own `pattern` constraint (the declaration's job), and
/// double-encoding a value the schema already constrained would corrupt
/// it. Header rendering additionally rejects CR/LF in the rendered value
/// (see [`super::check`]).
pub fn render_template(template: &str, config: &Value) -> Result<String> {
    let mut rendered = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let mut rest = 0;
    while rest < bytes.len() {
        if bytes[rest] != b'{' {
            // Copy the literal run up to the next placeholder.
            let next = template[rest..]
                .find('{')
                .map(|o| rest + o)
                .unwrap_or(bytes.len());
            rendered.push_str(&template[rest..next]);
            rest = next;
            continue;
        }
        let close = template[rest..]
            .find('}')
            .map(|o| rest + o)
            .ok_or_else(|| conn_err(format!("template `{template}` has an unbalanced `{{`")))?;
        let field = &template[rest + 1..close];
        let mut value = config;
        for segment in field.split('.') {
            value = value.get(segment).ok_or_else(|| {
                conn_err(format!(
                    "placeholder `{{{field}}}` does not resolve against this config"
                ))
            })?;
        }
        match value {
            Value::String(text) => rendered.push_str(text),
            Value::Number(number) => rendered.push_str(&number.to_string()),
            Value::Bool(flag) => rendered.push_str(if *flag { "true" } else { "false" }),
            _ => {
                return Err(conn_err(format!(
                    "placeholder `{{{field}}}` resolves to a structured value — only scalars render"
                )));
            }
        }
        rest = close + 1;
    }
    Ok(rendered)
}

/// The config-property paths a schema declares, dot-joined, walking
/// `properties` recursively and unioning `oneOf` variants (a path is
/// declared when at least one variant declares it — the variant picker
/// decides at config time which branch exists). Errors when the schema
/// is not an object at the root or exceeds the serialized cap.
fn declared_config_paths(
    spec: &Value,
) -> std::result::Result<std::collections::BTreeSet<String>, String> {
    if !spec.is_object() {
        return Err(
            "connection_specification must be a JSON object (a draft-07 schema)".to_owned(),
        );
    }
    let bytes = serde_json::to_vec(spec)
        .map_err(|e| format!("connection_specification did not serialize: {e}"))?;
    if bytes.len() > MAX_SPEC_BYTES {
        return Err(format!(
            "connection_specification is {} bytes, above the {MAX_SPEC_BYTES} cap",
            bytes.len()
        ));
    }
    if spec.get("type") != Some(&Value::String("object".to_owned())) {
        return Err("connection_specification must be an object schema at the root".to_owned());
    }
    let mut paths = std::collections::BTreeSet::new();
    collect_property_paths(spec, "", &mut paths);
    Ok(paths)
}

/// Walk one schema's `properties` (and each `oneOf` variant's),
/// recording dot-joined property paths.
fn collect_property_paths(
    schema: &Value,
    prefix: &str,
    paths: &mut std::collections::BTreeSet<String>,
) {
    let walk = |schema: &Value, paths: &mut std::collections::BTreeSet<String>| {
        if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
            for (name, subschema) in properties {
                let path = format!("{prefix}{name}");
                paths.insert(path.clone());
                collect_property_paths(subschema, &format!("{path}."), paths);
            }
        }
    };
    walk(schema, paths);
    if let Some(variants) = schema.get("oneOf").and_then(Value::as_array) {
        for variant in variants {
            walk(variant, paths);
        }
    }
}

/// Check one template's placeholders against the declared set, erroring
/// on the first undeclared name. `extra` holds the additional legal
/// names a path template gets from the operation's own params.
fn check_template_placeholders(
    what: &str,
    template: &str,
    declared: &std::collections::BTreeSet<String>,
    extra: &[String],
    manifest_id: &str,
) -> Result<()> {
    for field in scan_placeholders(template)? {
        if !declared.contains(&field) && !extra.iter().any(|name| name == &field) {
            return Err(conn_err(format!(
                "manifest `{manifest_id}` {what} placeholder `{{{field}}}` names no declared \
                 schema property"
            )));
        }
    }
    Ok(())
}

fn validate_operation(
    operation: &ConnectorOperation,
    declared: &std::collections::BTreeSet<String>,
    manifest_id: &str,
) -> Result<()> {
    validate_operation_name(&operation.name, manifest_id)?;
    validate_text_field(
        "operation description",
        &operation.description,
        MAX_OPERATION_DESCRIPTION_LEN,
        false,
    )
    .map_err(|e| {
        conn_err(format!(
            "manifest `{manifest_id}` operation `{}`: {e}",
            operation.name
        ))
    })?;
    if operation.description != operation.description.trim() {
        return Err(conn_err(format!(
            "manifest `{manifest_id}` operation `{}` description must be trimmed",
            operation.name
        )));
    }
    if !operation.path.starts_with('/') || operation.path.len() > MAX_PATH_TEMPLATE_LEN {
        return Err(conn_err(format!(
            "manifest `{manifest_id}` operation `{}` path must start with `/` and be at most \
             {MAX_PATH_TEMPLATE_LEN} bytes",
            operation.name
        )));
    }
    // Method/effect honesty: the declaration may not claim a GET writes
    // or a DELETE is safe to retry.
    match (operation.method, operation.effect) {
        (HttpMethod::Get, OperationEffect::ReadOnly) => {}
        (HttpMethod::Get, _) => {
            return Err(conn_err(format!(
                "manifest `{manifest_id}` operation `{}` is a GET and must declare \
                 `read_only` effect",
                operation.name
            )));
        }
        (HttpMethod::Delete, OperationEffect::Irreversible) => {}
        (HttpMethod::Delete, _) => {
            return Err(conn_err(format!(
                "manifest `{manifest_id}` operation `{}` is a DELETE and must declare \
                 `irreversible` effect",
                operation.name
            )));
        }
        _ => {}
    }
    if !operation.params_schema.is_object() {
        return Err(conn_err(format!(
            "manifest `{manifest_id}` operation `{}` params schema must be a JSON object",
            operation.name
        )));
    }
    let schema_bytes = serde_json::to_vec(&operation.params_schema).map_err(|e| {
        conn_err(format!(
            "manifest `{manifest_id}` operation `{}` params schema did not serialize: {e}",
            operation.name
        ))
    })?;
    if schema_bytes.len() > MAX_OPERATION_SCHEMA_BYTES {
        return Err(conn_err(format!(
            "manifest `{manifest_id}` operation `{}` params schema exceeds \
             {MAX_OPERATION_SCHEMA_BYTES} bytes",
            operation.name
        )));
    }
    // The extra names a path template may use: the operation's own
    // declared params (call arguments at execution time).
    let params: Vec<String> = operation
        .params_schema
        .get("properties")
        .and_then(Value::as_object)
        .map(|properties| properties.keys().cloned().collect())
        .unwrap_or_default();
    check_template_placeholders("path", &operation.path, declared, &params, manifest_id)?;
    if operation.headers.len() > MAX_HEADERS {
        return Err(conn_err(format!(
            "manifest `{manifest_id}` operation `{}` declares {} headers, above the {MAX_HEADERS} \
             cap",
            operation.name,
            operation.headers.len()
        )));
    }
    for (name, value) in &operation.headers {
        let legal_name = !name.is_empty()
            && name.len() <= 128
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b));
        if !legal_name {
            return Err(conn_err(format!(
                "manifest `{manifest_id}` operation `{}` declares illegal header name `{name}`",
                operation.name
            )));
        }
        if value.len() > MAX_HEADER_VALUE_LEN {
            return Err(conn_err(format!(
                "manifest `{manifest_id}` operation `{}` header `{name}` value template exceeds \
                 {MAX_HEADER_VALUE_LEN} bytes",
                operation.name
            )));
        }
        check_template_placeholders("header", value, declared, &[], manifest_id)?;
    }
    if operation.auth.len() > MAX_AUTH_ALTERNATIVES {
        return Err(conn_err(format!(
            "manifest `{manifest_id}` operation `{}` declares {} auth alternatives, above the \
             {MAX_AUTH_ALTERNATIVES} cap",
            operation.name,
            operation.auth.len()
        )));
    }
    for alternative in &operation.auth {
        for template in alternative.templates() {
            check_template_placeholders("auth", template, declared, &[], manifest_id)?;
        }
    }
    if let Some(ceiling) = operation.max_response_bytes {
        if ceiling == 0 {
            return Err(conn_err(format!(
                "manifest `{manifest_id}` operation `{}` response ceiling must be positive",
                operation.name
            )));
        }
    }
    Ok(())
}

/// A connection specification has to constrain what it accepts.
///
/// Two properties make the difference between a contract and a formality: an
/// object is closed (`additionalProperties: false`, so an undeclared field is
/// a rejection rather than a silent extra), and something is required (so an
/// empty object is not a valid configuration). Without both, every config
/// validates, no surface can tell an operator what a connector needs, and a
/// caller can carry any shape it found convenient in an undeclared corner.
///
/// The rule holds all the way down: a spec whose root is closed but whose
/// `credentials` object is open is exactly the hole it is meant to close.
fn validate_spec_constrains(id: &str, spec: &Value) -> Result<()> {
    check_object_schema(id, "connection_specification", spec)
}

fn check_object_schema(id: &str, at: &str, schema: &Value) -> Result<()> {
    // A choice of shapes delegates to the shapes: each branch is a config a
    // caller may send, so each one carries the obligation.
    for key in ["oneOf", "anyOf"] {
        if let Some(branches) = schema.get(key).and_then(Value::as_array) {
            if branches.is_empty() {
                return Err(spec_err(id, at, format!("`{key}` lists no variants")));
            }
            for (i, branch) in branches.iter().enumerate() {
                check_object_schema(id, &format!("{at}.{key}[{i}]"), branch)?;
            }
            return Ok(());
        }
    }

    match schema.get("additionalProperties") {
        // A closed object: an undeclared field is refused. It must also ask
        // for something, or an empty object configures the connector.
        Some(Value::Bool(false)) => {
            let declares = schema
                .get("properties")
                .and_then(Value::as_object)
                .is_some_and(|properties| !properties.is_empty());
            let requires = schema
                .get("required")
                .and_then(Value::as_array)
                .is_some_and(|required| !required.is_empty());
            // A closed object with no properties is already exact: the only
            // config it accepts is the empty one, which is the honest shape
            // for a connector that needs no configuration.
            if declares && !requires {
                return Err(spec_err(
                    id,
                    at,
                    "declares properties but requires none of them — an \
                     operator could save a connection that configures nothing"
                        .to_string(),
                ));
            }
        }
        // A typed map — the values are constrained even though the keys are
        // open, which is a contract an operator can read.
        Some(Value::Object(_)) => {}
        _ => {
            return Err(spec_err(
                id,
                at,
                "must set `additionalProperties: false` (or a schema for its \
                 values) — an open object accepts a config shaped however its \
                 caller preferred"
                    .to_string(),
            ));
        }
    }

    // Every declared object property carries the same obligation.
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        for (name, property) in properties {
            if is_object_schema(property) {
                check_object_schema(id, &format!("{at}.{name}"), property)?;
            }
        }
    }
    Ok(())
}

/// True when the schema describes an object — either by saying so or by
/// carrying the keywords only an object schema has.
fn is_object_schema(schema: &Value) -> bool {
    schema.get("type").and_then(Value::as_str) == Some("object")
        || schema.get("properties").is_some()
        || schema.get("oneOf").is_some()
        || schema.get("anyOf").is_some()
}

fn spec_err(id: &str, at: &str, why: String) -> crate::error::RustyError {
    conn_err(format!("manifest `{id}`: {at} {why}"))
}

fn validate_connector_id(id: &str) -> Result<()> {
    let legal = !id.is_empty()
        && id.len() <= MAX_CONNECTOR_ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !id.starts_with('-')
        && !id.ends_with('-')
        && !id.contains("--");
    if legal {
        return Ok(());
    }
    Err(conn_err(format!(
        "connector id `{id}` must be kebab-case (`[a-z0-9]+(-[a-z0-9]+)*`), at most \
         {MAX_CONNECTOR_ID_LEN} bytes"
    )))
}

fn validate_operation_name(name: &str, manifest_id: &str) -> Result<()> {
    let legal = !name.is_empty()
        && name.len() <= MAX_OPERATION_NAME_LEN
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if legal {
        return Ok(());
    }
    Err(conn_err(format!(
        "manifest `{manifest_id}` operation name `{name}` must be kebab-case, at most \
         {MAX_OPERATION_NAME_LEN} bytes"
    )))
}

/// https-only, on the template: a URL that does not begin `https://`
/// fails at declaration, before any config exists to render it.
fn validate_https_url(what: &str, value: &str, max_len: usize) -> Result<()> {
    if value.len() > max_len {
        return Err(conn_err(format!("{what} exceeds {max_len} bytes")));
    }
    if !value.starts_with("https://") {
        return Err(conn_err(format!(
            "{what} `{value}` must be https — plaintext endpoints are not declarable"
        )));
    }
    Ok(())
}

/// Non-empty, control-free, within the cap — the text-field discipline
/// the registry's other contracts share.
fn validate_text_field(what: &str, value: &str, max_len: usize, allow_empty: bool) -> Result<()> {
    if !allow_empty && value.is_empty() {
        return Err(conn_err(format!("{what} must not be empty")));
    }
    if value.len() > max_len {
        return Err(conn_err(format!("{what} exceeds {max_len} bytes")));
    }
    if value.chars().any(char::is_control) {
        return Err(conn_err(format!("{what} contains control characters")));
    }
    Ok(())
}

// ── Read-backs proposed from the manifest itself ────────────────────────────

/// A read-back the manifest's own operations make possible for one of its
/// writes: the read that would find what the write made, and how its
/// arguments follow from the write's. Proposed, never applied — a builder
/// adopts it, and the adoption is a new version of the connector.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReadBackProposal {
    /// The write operation the read-back is for.
    pub write: String,
    /// The read operation that finds what the write made.
    pub operation: String,
    /// The read's arguments as a template over the write's (`$field`).
    pub arguments: Value,
    /// How the proposal was made, in words a builder can check.
    pub why: String,
}

/// Parameters a read takes that filter by a record's own words, most
/// specific first: a query language, then a plain field.
const FILTER_PARAMS: [&str; 12] = [
    "sysparm_query",
    "query",
    "q",
    "search",
    "filter",
    "where",
    "short_description",
    "subject",
    "title",
    "summary",
    "name",
    "key",
];

/// The write's fields a record is found by again, most specific first.
const NATURAL_KEYS: [&str; 8] = [
    "short_description",
    "subject",
    "title",
    "summary",
    "name",
    "key",
    "description",
    "text",
];

/// Parameters that bound how much a read answers.
const LIMIT_PARAMS: [&str; 4] = ["sysparm_limit", "limit", "per_page", "max_results"];

fn string_properties(schema: &Value) -> Vec<String> {
    schema
        .get("properties")
        .and_then(Value::as_object)
        .map(|props| {
            props
                .iter()
                .filter(|(_, v)| {
                    v.get("type")
                        .is_none_or(|t| t == "string" || t == "integer" || t == "number")
                })
                .map(|(k, _)| k.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// The write's path with a trailing `/{param}` removed: the collection an
/// update or delete addresses one member of.
fn collection_of(path: &str) -> Option<&str> {
    let trimmed = path.trim_end_matches('/');
    let (head, last) = trimmed.rsplit_once('/')?;
    (last.starts_with('{') && last.ends_with('}') && !head.is_empty()).then_some(head)
}

impl ConnectorManifest {
    /// The writes that would guess after a lost answer: every write with
    /// no read-back declared.
    pub fn writes_without_read_back(&self) -> Vec<&ConnectorOperation> {
        self.operations
            .iter()
            .filter(|op| op.method != HttpMethod::Get && op.reconcile.is_none())
            .collect()
    }

    /// Read-backs the manifest's own reads make possible, one per write
    /// that has none: a read on the same path (or the collection a member
    /// path belongs to) that takes a filter, bound to the write's most
    /// specific natural key; parameters the two share by name bound alike;
    /// a limit, when the read takes one, held to a few records. A write
    /// with no such read gets no proposal — nothing is invented.
    pub fn propose_read_backs(&self) -> Vec<ReadBackProposal> {
        let reads: Vec<&ConnectorOperation> = self
            .operations
            .iter()
            .filter(|op| op.method == HttpMethod::Get)
            .collect();
        let mut proposals = Vec::new();
        for write in self.writes_without_read_back() {
            let write_fields = string_properties(&write.params_schema);
            let Some(key) = NATURAL_KEYS
                .iter()
                .find(|k| write_fields.iter().any(|f| f == *k))
            else {
                continue;
            };
            let paths: Vec<&str> = std::iter::once(write.path.trim_end_matches('/'))
                .chain(collection_of(&write.path))
                .collect();
            let candidate = reads
                .iter()
                .filter(|r| paths.contains(&r.path.trim_end_matches('/')))
                .find_map(|read| {
                    let read_params = string_properties(&read.params_schema);
                    let filter = FILTER_PARAMS
                        .iter()
                        .find(|f| read_params.iter().any(|p| p == *f))?;
                    Some((read, read_params, *filter))
                });
            let Some((read, read_params, filter)) = candidate else {
                continue;
            };
            let mut arguments = serde_json::Map::new();
            // A query-language parameter takes `field=value`; a plain field
            // parameter takes the value.
            let query_language = matches!(
                filter,
                "sysparm_query" | "query" | "q" | "search" | "filter" | "where"
            );
            arguments.insert(
                filter.to_owned(),
                Value::String(if query_language {
                    format!("{key}=${key}")
                } else {
                    format!("${key}")
                }),
            );
            for shared in read_params
                .iter()
                .filter(|p| p.as_str() != filter && write_fields.contains(p))
            {
                arguments.insert(shared.clone(), Value::String(format!("${shared}")));
            }
            if let Some(limit) = LIMIT_PARAMS
                .iter()
                .find(|l| read_params.iter().any(|p| p == *l))
            {
                arguments.insert((*limit).to_owned(), Value::from(5));
            }
            proposals.push(ReadBackProposal {
                write: write.name.clone(),
                operation: read.name.clone(),
                arguments: Value::Object(arguments),
                why: format!("`{}` reads {} and takes `{filter}`, so what `{}` made can be found again by its `{key}`", read.name, read.path, write.name),
            });
        }
        proposals
    }
}

#[cfg(test)]
mod read_back_tests {
    use super::*;
    use serde_json::json;

    fn op(name: &str, method: HttpMethod, path: &str, properties: Value) -> ConnectorOperation {
        ConnectorOperation {
            name: name.to_owned(),
            description: name.to_owned(),
            method,
            path: path.to_owned(),
            effect: if method == HttpMethod::Get {
                OperationEffect::ReadOnly
            } else {
                OperationEffect::Compensatable
            },
            params_schema: json!({"type": "object", "properties": properties}),
            headers: Vec::new(),
            auth: Vec::new(),
            max_response_bytes: None,
            reconcile: None,
        }
    }

    fn manifest(operations: Vec<ConnectorOperation>) -> ConnectorManifest {
        ConnectorManifest {
            id: "t".into(),
            version: "1".into(),
            display_name: "T".into(),
            description: String::new(),
            documentation_url: String::new(),
            base_url: "https://t.example".into(),
            connection_specification: json!({}),
            operations,
            check: "list".into(),
            authorization: None,
            hash: String::new(),
        }
    }

    #[test]
    fn a_table_api_write_is_read_back_by_the_query_read_on_its_path() {
        let m = manifest(vec![
            op(
                "list",
                HttpMethod::Get,
                "/api/now/table/{table}",
                json!({"table": {"type": "string"}, "sysparm_query": {"type": "string"}, "sysparm_limit": {"type": "integer"}, "sysparm_fields": {"type": "string"}}),
            ),
            op(
                "create",
                HttpMethod::Post,
                "/api/now/table/{table}",
                json!({"table": {"type": "string"}, "short_description": {"type": "string"}, "caller_id": {"type": "string"}}),
            ),
        ]);
        let proposals = m.propose_read_backs();
        assert_eq!(proposals.len(), 1, "{proposals:?}");
        let p = &proposals[0];
        assert_eq!((p.write.as_str(), p.operation.as_str()), ("create", "list"));
        assert_eq!(
            p.arguments,
            json!({"sysparm_query": "short_description=$short_description", "table": "$table", "sysparm_limit": 5})
        );
        assert!(p.why.contains("short_description"), "{}", p.why);
    }

    #[test]
    fn a_member_update_is_read_back_through_its_collection_and_a_plain_field_binds_directly() {
        let m = manifest(vec![
            op(
                "list-items",
                HttpMethod::Get,
                "/items",
                json!({"name": {"type": "string"}, "limit": {"type": "integer"}}),
            ),
            op(
                "update-item",
                HttpMethod::Patch,
                "/items/{id}",
                json!({"id": {"type": "string"}, "name": {"type": "string"}}),
            ),
        ]);
        let p = &m.propose_read_backs()[0];
        assert_eq!(p.operation, "list-items");
        assert_eq!(p.arguments, json!({"name": "$name", "limit": 5}));
    }

    #[test]
    fn nothing_is_proposed_without_a_filtering_read_or_a_natural_key_and_declared_writes_are_left_alone(
    ) {
        let no_filter = manifest(vec![
            op(
                "list",
                HttpMethod::Get,
                "/things",
                json!({"page": {"type": "integer"}}),
            ),
            op(
                "create",
                HttpMethod::Post,
                "/things",
                json!({"title": {"type": "string"}}),
            ),
        ]);
        assert!(no_filter.propose_read_backs().is_empty());
        assert_eq!(no_filter.writes_without_read_back().len(), 1);
        let no_key = manifest(vec![
            op(
                "list",
                HttpMethod::Get,
                "/things",
                json!({"q": {"type": "string"}}),
            ),
            op(
                "create",
                HttpMethod::Post,
                "/things",
                json!({"payload": {"type": "object"}}),
            ),
        ]);
        assert!(no_key.propose_read_backs().is_empty());
        let mut declared = manifest(vec![
            op(
                "list",
                HttpMethod::Get,
                "/things",
                json!({"q": {"type": "string"}}),
            ),
            op(
                "create",
                HttpMethod::Post,
                "/things",
                json!({"title": {"type": "string"}}),
            ),
        ]);
        declared.operations[1] = declared.operations[1]
            .clone()
            .with_read_back("list", json!({"q": "$title"}));
        assert!(declared.propose_read_backs().is_empty());
        assert!(declared.writes_without_read_back().is_empty());
    }
}
