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

/// The provider kind realizing the connector.
///
/// Serialized with an internal `kind` tag (`mcp_stdio` / `http_search`);
/// an unknown tag fails deserialization, so new provider kinds cannot
/// slip through old binaries unexamined.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProviderKind {
    /// An MCP server spawned as a child process over stdio.
    McpStdio(McpStdioSpec),
    /// A bounded HTTP web-search endpoint.
    HttpSearch(HttpSearchSpec),
}

/// The declared, content-addressed connector contract.
///
/// Construct through [`ConnectorManifest::new`], which validates every
/// field and computes [`ConnectorManifest::hash`]. Deserialization bypasses
/// construction, so the admission boundary
/// ([`super::ConnectorRegistry::register_manifest`]) re-validates and
/// re-verifies the hash — a tampered or oversized manifest fails there
/// even if it arrived over a channel that never called `new`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
}

impl ConnectorManifest {
    /// Validate and construct a manifest, computing its content hash.
    ///
    /// `capabilities`, `credential_slots`, and the env allowlist are
    /// canonicalized (sorted/deduplicated) so semantically equal manifests
    /// hash equal regardless of declaration order.
    pub fn new(
        id: impl Into<String>,
        version: impl Into<String>,
        display_name: impl Into<String>,
        description: impl Into<String>,
        mut provider: ProviderKind,
        mut capabilities: Vec<String>,
        mut credential_slots: Vec<CredentialSlot>,
    ) -> Result<Self> {
        capabilities.sort();
        capabilities.dedup();
        credential_slots.sort_by(|left, right| left.name.cmp(&right.name));
        if let ProviderKind::McpStdio(spec) = &mut provider {
            spec.env_allowlist.sort();
            spec.env_allowlist.dedup();
        }
        let mut manifest = Self {
            id: id.into(),
            version: version.into(),
            display_name: display_name.into(),
            description: description.into(),
            provider,
            capabilities,
            credential_slots,
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
        };
        // Serializing this view is infallible: every field is a string, a
        // string vec, or an already-serializable spec struct.
        let value = serde_json::to_value(&content)
            .expect("the manifest content view always serializes");
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
        validate_text_field("display_name", &self.display_name, MAX_DISPLAY_NAME_LEN, false)?;
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
            if self.credential_slots[..index].iter().any(|s| s.name == slot.name) {
                return Err(conn_err(format!(
                    "manifest `{}` declares credential slot `{}` twice",
                    self.id, slot.name
                )));
            }
        }

        match &self.provider {
            ProviderKind::McpStdio(spec) => self.validate_mcp_stdio(spec),
            ProviderKind::HttpSearch(spec) => self.validate_http_search(spec),
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
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_')
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
        if spec.base_url.chars().any(char::is_control) || spec.base_url.contains(char::is_whitespace)
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
        && version.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+' | b'_')
        });
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
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
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
fn validate_text_field(
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
