//! Check execution: the setup/edit gate.
//!
//! The manifest's named `check` operation — declaration validation has
//! already pinned it to a parameterless read-only GET — is rendered
//! against the candidate config (base URL, path, headers, and the first
//! auth alternative whose templates resolve) and sent over the
//! [`ConnectorTransport`] seam. Tests drive a scripted fake; the server
//! wires reqwest behind the same trait.
//!
//! The outcome is the Airbyte contract: `{"status": "succeeded"}` or
//! `{"status": "failed", "message": …}`. Every failure — a placeholder
//! that does not resolve, a non-2xx status, a transport error — is a
//! failed outcome with a human-readable message, never a raised error:
//! a check that cannot run *is* a failed check. The message names the
//! failing field when the config is at fault, so the setup form can pin
//! it. Auth material never appears in a message; a 401/403 echoes no
//! response body at all, since an auth-failure page may quote the
//! credential's neighborhood back.

use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;

use super::conn_err;
use super::manifest::{ConnectorManifest, ConnectorOperation, HttpMethod, OperationAuth};
use crate::error::Result;

/// Default per-check timeout.
pub const DEFAULT_CHECK_TIMEOUT: Duration = Duration::from_secs(15);

/// Maximum bytes of a non-2xx response body echoed into a failure
/// message.
pub const CHECK_ERROR_BODY_BYTES: usize = 256;

/// One outbound check exchange, as the transport seam sees it.
#[derive(Debug, Clone)]
pub struct CheckRequest {
    /// The HTTP method (declaration-pinned to GET for the check op).
    pub method: HttpMethod,
    /// The full URL, templates rendered. https only — re-checked after
    /// rendering.
    pub url: String,
    /// Header name/value pairs, auth already resolved — the transport
    /// never sees template syntax.
    pub headers: Vec<(String, String)>,
    /// The timeout the transport must enforce.
    pub timeout: Duration,
    /// The response byte ceiling — the operation's declared ceiling,
    /// clamped to the surface cap.
    pub max_response_bytes: usize,
}

/// The transport's reply.
#[derive(Debug, Clone)]
pub struct CheckResponse {
    /// The HTTP status code.
    pub status: u16,
    /// The response body, already truncated at the operation's ceiling
    /// by the transport.
    pub body: Vec<u8>,
}

/// The HTTP seam check execution drives. Scripted in tests; reqwest in
/// the server slice.
#[async_trait]
pub trait ConnectorTransport: std::fmt::Debug + Send + Sync {
    /// Send `request`, honoring `request.timeout`.
    async fn send(&self, request: CheckRequest) -> Result<CheckResponse>;
}

/// The check verdict, serialized as the wire contract
/// `{"status": "succeeded" | "failed", "message"?}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckOutcome {
    /// The verdict.
    pub status: CheckStatus,
    /// The human-readable failure reason (absent on success).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// The verdict discriminant, lowercase on the wire (the Airbyte
/// contract, lowercased to match this crate's serde conventions).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    /// The config connects and can access what the connector needs.
    Succeeded,
    /// It does not; `message` says why.
    Failed,
}

impl CheckOutcome {
    fn succeeded() -> Self {
        Self {
            status: CheckStatus::Succeeded,
            message: None,
        }
    }

    fn failed(message: impl Into<String>) -> Self {
        Self {
            status: CheckStatus::Failed,
            message: Some(message.into()),
        }
    }
}

/// Render one operation's request against a config: base URL, path,
/// declared headers, and the first auth alternative whose templates all
/// resolve. Fails closed on the first unresolvable template. Rendered
/// URL and header values reject CR/LF — a rendered value must not
/// smuggle a second header or a request-line break.
pub fn render_operation_request(
    manifest: &ConnectorManifest,
    operation: &ConnectorOperation,
    config: &serde_json::Value,
) -> Result<CheckRequest> {
    let base = super::manifest::render_template(&manifest.base_url, config)?;
    let path = super::manifest::render_template(&operation.path, config)?;
    let url = format!("{}{}", base.trim_end_matches('/'), path);
    if !url.starts_with("https://") {
        return Err(conn_err(format!(
            "rendered URL for operation `{}` is not https",
            operation.name
        )));
    }
    if url.contains(['\r', '\n']) {
        return Err(conn_err(format!(
            "rendered URL for operation `{}` contains a newline",
            operation.name
        )));
    }
    let mut headers = Vec::with_capacity(operation.headers.len() + 1);
    for (name, template) in &operation.headers {
        let value = super::manifest::render_template(template, config)?;
        if value.contains(['\r', '\n']) {
            return Err(conn_err(format!(
                "rendered header `{name}` contains a newline"
            )));
        }
        headers.push((name.clone(), value));
    }
    // First fully-resolvable alternative wins; an operation with
    // alternatives that none resolve fails closed — the config's variant
    // does not match the operation's auth declaration.
    if !operation.auth.is_empty() {
        let mut rendered = None;
        for alternative in &operation.auth {
            match render_auth(alternative, config) {
                Ok(header) => {
                    rendered = Some(header);
                    break;
                }
                Err(_) => continue,
            }
        }
        let Some(header) = rendered else {
            return Err(conn_err(format!(
                "operation `{}` declares auth, but no alternative's placeholders resolve \
                 against this config",
                operation.name
            )));
        };
        headers.push(("Authorization".to_owned(), header));
    }
    Ok(CheckRequest {
        method: operation.method,
        url,
        headers,
        timeout: DEFAULT_CHECK_TIMEOUT,
        max_response_bytes: operation.response_ceiling(),
    })
}

fn render_auth(auth: &OperationAuth, config: &serde_json::Value) -> Result<String> {
    match auth {
        OperationAuth::Basic { username, password } => {
            let username = super::manifest::render_template(username, config)?;
            let password = super::manifest::render_template(password, config)?;
            Ok(format!(
                "Basic {}",
                base64_encode(format!("{username}:{password}").as_bytes())
            ))
        }
        OperationAuth::Bearer { token } => {
            let token = super::manifest::render_template(token, config)?;
            if token.contains(['\r', '\n']) {
                return Err(conn_err("rendered bearer token contains a newline"));
            }
            Ok(format!("Bearer {token}"))
        }
    }
}

/// Run the manifest's check operation with a candidate config. Never
/// raises: render failures, non-2xx statuses, and transport errors all
/// land in the [`CheckOutcome::message`].
pub async fn execute_check(
    manifest: &ConnectorManifest,
    config: &serde_json::Value,
    transport: &dyn ConnectorTransport,
) -> CheckOutcome {
    let Some(operation) = manifest.operation(&manifest.check) else {
        // Unreachable for a validated manifest — the check operation's
        // existence is declaration-checked — but a deserialized manifest
        // that skipped validation fails closed here.
        return CheckOutcome::failed(format!(
            "manifest `{}` declares no check operation named `{}`",
            manifest.id, manifest.check
        ));
    };
    let request = match render_operation_request(manifest, operation, config) {
        Ok(request) => request,
        Err(error) => return CheckOutcome::failed(error.to_string()),
    };
    let response = match transport.send(request).await {
        Ok(response) => response,
        Err(error) => return CheckOutcome::failed(format!("the check request failed: {error}")),
    };
    if (200..300).contains(&response.status) {
        return CheckOutcome::succeeded();
    }
    // Auth refusals echo no body: an auth-failure page may quote the
    // credential's neighborhood back.
    if response.status == 401 || response.status == 403 {
        return CheckOutcome::failed(format!(
            "the instance refused the configured credentials (HTTP {})",
            response.status
        ));
    }
    let excerpt = sanitize_excerpt(&response.body, CHECK_ERROR_BODY_BYTES);
    if excerpt.is_empty() {
        CheckOutcome::failed(format!("HTTP {}", response.status))
    } else {
        CheckOutcome::failed(format!("HTTP {}: {excerpt}", response.status))
    }
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

/// A bounded, control-stripped excerpt of a response body for failure
/// messages: lossy UTF-8, control characters flattened to spaces (a
/// message must not smuggle terminal escapes), truncated at a char
/// boundary with an explicit marker.
fn sanitize_excerpt(body: &[u8], max_bytes: usize) -> String {
    let text: String = String::from_utf8_lossy(body)
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if text.len() <= max_bytes {
        return text;
    }
    const MARKER: &str = "…[truncated]";
    let mut end = max_bytes.saturating_sub(MARKER.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{MARKER}", &text[..end])
}
