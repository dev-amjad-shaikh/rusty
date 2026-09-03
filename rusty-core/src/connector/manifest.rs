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
}

impl OperationAuth {
    /// Every template this alternative renders.
    pub(crate) fn templates(&self) -> Vec<&str> {
        match self {
            OperationAuth::Basic { username, password } => vec![username, password],
            OperationAuth::Bearer { token } => vec![token],
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
}

impl ConnectorOperation {
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
    documentation_url: &'a str,
    base_url: &'a str,
    connection_specification: &'a Value,
    operations: &'a [ConnectorOperation],
    check: &'a str,
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
            documentation_url: &self.documentation_url,
            base_url: &self.base_url,
            connection_specification: &self.connection_specification,
            operations: &self.operations,
            check: &self.check,
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
            let name = format!("{}/{}", self.id, operation.name);
            capabilities.push(crate::tool::ToolCapability {
                name,
                description: operation.description.clone(),
                parameters_schema: operation.params_schema.clone(),
                effect: operation.effect.wire_effect(),
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
