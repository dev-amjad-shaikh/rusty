//! The connector surface: schema-driven configuration (Airbyte model).
//!
//! One JSON Schema document per connector *is* the entire configuration
//! surface — validation, form rendering, field ordering, secret masking,
//! and conditional sub-forms all derive from it (the design:
//! `docs/connector-surface-design.md`; the research basis:
//! `research/airbyte-connector-configuration-model.md`). A connector is
//! declared as a content-addressed [`ConnectorManifest`] — identity is
//! the SHA-256 of its canonical serialization, and the manifest is
//! instance-agnostic by construction: it carries the
//! `connection_specification` schema and the operation set, never an
//! instance's identity or credentials.
//!
//! An instance pairs a manifest hash with one config object, validated
//! against the schema before anything persists. Fields flagged
//! `rusty_secret: true` are extracted from the config before persistence
//! and sealed through the credential broker; the stored record holds the
//! non-secret config plus sealed envelopes, so secrets are config-shaped
//! data, not a parallel credential model.
//!
//! Operations are bounded HTTP calls whose `base_url`, path segments,
//! headers, and auth templates carry `{field}` placeholders resolved
//! against the config at call time; every placeholder must name a
//! declared schema property, checked at declaration. The named `check`
//! operation — a parameterless read-only GET — is the setup/edit gate:
//! it executes with the candidate config and answers
//! [`CheckOutcome`], and check-success is the precondition the instance
//! assumes (the Airbyte invariant).
//!
//! Presentation hints live in a clean extension namespace on the schema,
//! ignored by validators: `rusty_secret` (mask + seal), `rusty_order`,
//! `rusty_group`, `rusty_hidden`, `rusty_pattern_descriptor`.
//! Polymorphism idiom: `oneOf` + `const` discriminator (auth variants).
//!
//! The persistent stores, sealing, and HTTP surface live in
//! `rusty-agent-server`; these are the pure contracts both sides agree
//! on.

use crate::error::RustyError;

pub mod check;
pub mod config;
pub mod curation;
pub mod instance;
pub mod manifest;
pub mod openapi;

pub use check::{
    execute_check, render_operation_request, CheckOutcome, CheckRequest, CheckResponse,
    CheckStatus, ConnectorTransport, CHECK_ERROR_BODY_BYTES, DEFAULT_CHECK_TIMEOUT,
};
pub use config::{
    compile_spec, extract_secrets, insert_masked_secrets, insert_opened_secrets, validate_config,
    without_secrets, SECRET_FLAG,
};
pub use curation::{curate, CuratedConnector, CuratedOperation, CurationRule};
pub use instance::{ConnectorInstance, INSTANCE_ID_PREFIX, MAX_INSTANCE_ID_LEN};
pub use manifest::{
    render_template, scan_placeholders, ConnectorManifest, ConnectorOperation, HttpMethod,
    OperationAuth, OperationEffect,
};
pub use openapi::{diff_imports, import_openapi, OpenApiImport, UnmappedOperation};

/// Build a [`RustyError::Tool`] with a `connector:` context prefix, the
/// same convention `mcp:` uses for the MCP client.
pub(crate) fn conn_err(msg: impl Into<String>) -> RustyError {
    RustyError::Tool(format!("connector: {}", msg.into()))
}

/// SHA-256 of the canonical JSON form of `value` — object keys sorted
/// recursively — the digest convention the connector content address
/// (manifest hash) shares with the record model's manifest pins.
pub(crate) fn canonical_json_hash(value: &serde_json::Value) -> String {
    // Serializing a `Value` is infallible in practice (its maps always
    // have string keys); see `PayloadRef::content_hash` for the same
    // argument.
    let bytes = serde_json::to_vec(&crate::record::canonicalize_value(value))
        .expect("a serde_json::Value always serializes");
    crate::record::sha256_hex(&bytes)
}
