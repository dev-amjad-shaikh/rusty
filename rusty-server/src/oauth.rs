//! A reqwest-backed OAuth 2.0 token endpoint: the concrete
//! [`OAuthProvider`] a deployment plugs for the flows that need no human
//! consent screen — resource-owner password and client-credentials
//! (ServiceNow's `/oauth_token.do` is the reference shape for both).
//!
//! The transport discipline: secrets leave the call only as form fields
//! on the outbound request,
//! the response body is read under a byte ceiling while streaming, and
//! non-2xx statuses classify under the retry taxonomy — the RFC 6749
//! terminal refusals (`invalid_grant`, `invalid_client`, a malformed
//! request the same bytes will always lose) are `permanent`, everything
//! operational (429, 5xx, transport) is transient. Error details are
//! bounded and control-stripped; request material is never echoed into a
//! failure.

use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use rusty_agent_runtime::broker::{
    ConnectionProvider, ConnectionRecord, OAuthFailure, OAuthProvider, PasswordGrant, TokenGrant,
    TokenMaterial,
};
use rusty_agent_runtime::durable::ErrorClass;

/// Per-call timeout for a token-endpoint exchange.
const TOKEN_ENDPOINT_TIMEOUT: Duration = Duration::from_secs(15);

/// Maximum bytes of a token-endpoint response body. Token responses are
/// small JSON documents; the ceiling keeps a hostile or misconfigured
/// endpoint from turning one exchange into an unbounded allocation.
const MAX_TOKEN_RESPONSE_BYTES: usize = 16 * 1024;

/// Maximum bytes of a provider error echoed into a failure's detail.
const MAX_ERROR_DETAIL_BYTES: usize = 256;

/// The reqwest-backed token endpoint. Stateless across calls: every grant
/// arrives with the material it presents, so one provider serves every
/// connection in the deployment.
pub struct ReqwestOAuthProvider {
    client: reqwest::Client,
}

impl std::fmt::Debug for ReqwestOAuthProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The client's connection pool can hold resolved auth material
        // from prior requests; only the configuration surface prints.
        f.debug_struct("ReqwestOAuthProvider").finish()
    }
}

impl Default for ReqwestOAuthProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl ReqwestOAuthProvider {
    /// A provider over a fresh connection pool.
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }

    /// POST one grant to `token_url` and classify the answer. `fields`
    /// carries the form pairs — grant type, client credentials, and the
    /// presented secret — which appear only on the outbound wire.
    async fn grant(
        &self,
        token_url: &str,
        fields: Vec<(&str, &str)>,
    ) -> std::result::Result<TokenGrant, OAuthFailure> {
        let exchange = self
            .client
            .post(token_url)
            .form(&fields)
            .timeout(TOKEN_ENDPOINT_TIMEOUT)
            .send();
        let response = match tokio::time::timeout(TOKEN_ENDPOINT_TIMEOUT, exchange).await {
            Err(_) => {
                return Err(OAuthFailure::transient(
                    ErrorClass::Timeout,
                    format!("the token endpoint did not answer within {TOKEN_ENDPOINT_TIMEOUT:?}"),
                ));
            }
            Ok(Err(e)) => {
                return Err(OAuthFailure::transient(
                    ErrorClass::Transient,
                    format!("the token endpoint exchange failed: {e}"),
                ));
            }
            Ok(Ok(response)) => response,
        };
        let status = response.status().as_u16();

        use futures::StreamExt;
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| {
                OAuthFailure::transient(
                    ErrorClass::Transient,
                    format!("the token endpoint response broke mid-stream: {e}"),
                )
            })?;
            if body.len() + chunk.len() > MAX_TOKEN_RESPONSE_BYTES {
                return Err(OAuthFailure::transient(
                    ErrorClass::DependencyFailure,
                    format!(
                        "the token endpoint response exceeds the {MAX_TOKEN_RESPONSE_BYTES}-byte ceiling"
                    ),
                ));
            }
            body.extend_from_slice(&chunk);
        }

        if !(200..=299).contains(&status) {
            return Err(classify_refusal(status, &body));
        }

        let value: serde_json::Value = serde_json::from_slice(&body).map_err(|e| {
            OAuthFailure::transient(
                ErrorClass::DependencyFailure,
                format!("the token endpoint answered non-JSON success: {e}"),
            )
        })?;
        let access_token = value
            .get("access_token")
            .and_then(|t| t.as_str())
            .filter(|t| !t.is_empty())
            .ok_or_else(|| {
                OAuthFailure::transient(
                    ErrorClass::DependencyFailure,
                    "the token endpoint's success answer carries no access token".to_owned(),
                )
            })?;
        let refresh_token = value
            .get("refresh_token")
            .and_then(|t| t.as_str())
            .filter(|t| !t.is_empty())
            .map(str::to_owned);
        let expires_at = parse_expires_in(&value).map(|seconds| Utc::now() + seconds);
        Ok(TokenGrant {
            access_token: access_token.to_owned(),
            refresh_token,
            expires_at,
        })
    }

    /// The refresh half for one connection, dispatching on its provider
    /// kind. The broker's `is_oauth` gate keeps static kinds off this
    /// path; the defensive arm fails closed rather than panicking on a
    /// caller that bypassed it.
    async fn refresh_dispatch(
        &self,
        connection: &ConnectionRecord,
        material: &TokenMaterial,
    ) -> std::result::Result<TokenGrant, OAuthFailure> {
        let token_url = material.token_url.as_deref().ok_or_else(|| {
            OAuthFailure::permanent(format!(
                "connection `{}` carries no token endpoint — re-register it through a grant \
                 path that records one",
                connection.connection_id
            ))
        })?;
        match connection.provider {
            ConnectionProvider::Oauth2Password => {
                // The password grant's design: the sealed password is the
                // durable material, so a refused refresh token re-mints
                // from the password instead of flipping to needs_reauth.
                // A *wrong password* is the terminal refusal that stands.
                if let Some(refresh_token) = &material.refresh_token {
                    let presented = self
                        .grant(
                            token_url,
                            vec![
                                ("grant_type", "refresh_token"),
                                ("refresh_token", refresh_token),
                                ("client_id", material.client_id.as_deref().unwrap_or("")),
                                (
                                    "client_secret",
                                    material.client_secret.as_deref().unwrap_or(""),
                                ),
                            ],
                        )
                        .await;
                    match presented {
                        Err(failure) if failure.permanent && material.password.is_some() => {}
                        other => return other,
                    }
                }
                let (username, password) = match (&material.username, &material.password) {
                    (Some(username), Some(password)) => (username, password),
                    _ => {
                        return Err(OAuthFailure::permanent(format!(
                            "connection `{}` carries no password material to re-present",
                            connection.connection_id
                        )));
                    }
                };
                self.grant(
                    token_url,
                    vec![
                        ("grant_type", "password"),
                        ("client_id", material.client_id.as_deref().unwrap_or("")),
                        (
                            "client_secret",
                            material.client_secret.as_deref().unwrap_or(""),
                        ),
                        ("username", username),
                        ("password", password),
                    ],
                )
                .await
            }
            ConnectionProvider::Oauth2ClientCredentials => {
                let client_secret = material.client_secret.as_deref().ok_or_else(|| {
                    OAuthFailure::permanent(format!(
                        "connection `{}` carries no client secret to re-present",
                        connection.connection_id
                    ))
                })?;
                self.grant(
                    token_url,
                    vec![
                        ("grant_type", "client_credentials"),
                        ("client_id", material.client_id.as_deref().unwrap_or("")),
                        ("client_secret", client_secret),
                    ],
                )
                .await
            }
            ConnectionProvider::Oauth2AuthorizationCode => {
                let refresh_token = material.refresh_token.as_deref().ok_or_else(|| {
                    OAuthFailure::permanent(format!(
                        "connection `{}` carries no refresh token",
                        connection.connection_id
                    ))
                })?;
                self.grant(
                    token_url,
                    vec![
                        ("grant_type", "refresh_token"),
                        ("refresh_token", refresh_token),
                        ("client_id", material.client_id.as_deref().unwrap_or("")),
                        (
                            "client_secret",
                            material.client_secret.as_deref().unwrap_or(""),
                        ),
                    ],
                )
                .await
            }
            other => Err(OAuthFailure::permanent(format!(
                "connection `{}` is {other:?} — not an OAuth flow this provider refreshes",
                connection.connection_id
            ))),
        }
    }
}

#[async_trait]
impl OAuthProvider for ReqwestOAuthProvider {
    async fn exchange_code(
        &self,
        _code: &str,
        _scopes: &std::collections::BTreeSet<String>,
    ) -> std::result::Result<TokenGrant, OAuthFailure> {
        Err(OAuthFailure::permanent(
            "this deployment's token endpoint speaks the password and client-credentials \
             grants — an authorization-code exchange needs a provider wired for that flow",
        ))
    }

    async fn exchange_password(
        &self,
        grant: &PasswordGrant,
    ) -> std::result::Result<TokenGrant, OAuthFailure> {
        self.grant(
            &grant.token_url,
            vec![
                ("grant_type", "password"),
                ("client_id", &grant.client_id),
                ("client_secret", &grant.client_secret),
                ("username", &grant.username),
                ("password", &grant.password),
            ],
        )
        .await
    }

    async fn refresh(
        &self,
        connection: &ConnectionRecord,
        material: &TokenMaterial,
    ) -> std::result::Result<TokenGrant, OAuthFailure> {
        self.refresh_dispatch(connection, material).await
    }
}

/// Read `expires_in` (seconds, number or string — providers spell it both
/// ways) into a duration. Absent or unparsable means no declared expiry:
/// the refresh lifecycle then never preempts, matching the broker's
/// "no declared expiry, no horizon" rule.
fn parse_expires_in(value: &serde_json::Value) -> Option<chrono::Duration> {
    let seconds = value
        .get("expires_in")
        .and_then(|e| e.as_i64().or_else(|| e.as_str()?.parse().ok()))?;
    if seconds <= 0 {
        return None;
    }
    Some(chrono::Duration::seconds(seconds))
}

/// Classify a non-2xx answer. The terminal set is RFC 6749's: 400 carries
/// `invalid_grant` / `invalid_client` / `invalid_request` — the same bytes
/// will fail the same way on every attempt — and 401/403 are the client
/// authentication refusals. The detail quotes the provider's `error` and
/// `error_description` (bounded, control-stripped), never the request.
fn classify_refusal(status: u16, body: &[u8]) -> OAuthFailure {
    let detail = error_detail(body);
    match status {
        400 | 401 | 403 => OAuthFailure::permanent(detail),
        429 => OAuthFailure::transient(ErrorClass::RateLimited, detail),
        500..=599 => OAuthFailure::transient(ErrorClass::DependencyFailure, detail),
        _ => OAuthFailure::transient(ErrorClass::Transient, detail),
    }
}

/// The human-facing detail of a refused exchange: the provider's
/// `error[: description]` when the body is OAuth-shaped JSON, else a
/// bounded, control-stripped excerpt.
fn error_detail(body: &[u8]) -> String {
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) {
        let error = value.get("error").and_then(|e| e.as_str());
        let description = value.get("error_description").and_then(|d| d.as_str());
        if let Some(error) = error {
            let mut detail = match description {
                Some(description) if !description.is_empty() => {
                    format!("{error}: {description}")
                }
                _ => error.to_owned(),
            };
            detail = detail
                .chars()
                .map(|c| if c.is_control() { ' ' } else { c })
                .collect();
            if detail.len() > MAX_ERROR_DETAIL_BYTES {
                detail.truncate(MAX_ERROR_DETAIL_BYTES);
            }
            return format!("the token endpoint refused: {detail}");
        }
    }
    let text: String = String::from_utf8_lossy(body)
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let mut excerpt = text.trim().to_owned();
    if excerpt.len() > MAX_ERROR_DETAIL_BYTES {
        excerpt.truncate(MAX_ERROR_DETAIL_BYTES);
    }
    if excerpt.is_empty() {
        "the token endpoint refused with no detail".to_owned()
    } else {
        format!("the token endpoint answered: {excerpt}")
    }
}

// --------------------------------------------------------------------- //
// Tests
// --------------------------------------------------------------------- //

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---------- classify_refusal ----------

    #[test]
    fn refusal_classifies_the_terminal_set_permanent() {
        // RFC 6749's terminal refusals ride 400; 401/403 are the client
        // authentication refusals. All three flip the connection to
        // needs_reauth — the same bytes fail the same way on a retry.
        let body = br#"{"error": "invalid_grant", "error_description": "bad credentials"}"#;
        for status in [400u16, 401, 403] {
            let failure = classify_refusal(status, body);
            assert!(failure.permanent, "status {status} must be terminal");
            assert_eq!(failure.class, ErrorClass::InvalidInput);
            assert_eq!(
                failure.detail,
                "the token endpoint refused: invalid_grant: bad credentials"
            );
        }
    }

    #[test]
    fn refusal_classifies_operational_answers_transient() {
        let cases: [(u16, ErrorClass); 5] = [
            (429, ErrorClass::RateLimited),
            (500, ErrorClass::DependencyFailure),
            (503, ErrorClass::DependencyFailure),
            (404, ErrorClass::Transient),
            (418, ErrorClass::Transient),
        ];
        for (status, class) in cases {
            let failure = classify_refusal(status, b"");
            assert!(!failure.permanent, "status {status} must stay retryable");
            assert_eq!(failure.class, class, "status {status}");
            assert_eq!(failure.detail, "the token endpoint refused with no detail");
        }
    }

    #[test]
    fn refusal_strips_control_bytes_from_the_detail() {
        let failure = classify_refusal(503, b"bad\x00gateway\x07");
        assert!(failure.detail.starts_with("the token endpoint answered: "));
        assert!(failure.detail.chars().all(|c| !c.is_control()));
    }

    #[test]
    fn refusal_bounds_the_detail_at_the_byte_ceiling() {
        let body = format!(
            r#"{{"error": "{}", "error_description": ""}}"#,
            "x".repeat(MAX_ERROR_DETAIL_BYTES * 2)
        );
        let failure = classify_refusal(400, body.as_bytes());
        assert!(
            failure.detail.len() <= "the token endpoint refused: ".len() + MAX_ERROR_DETAIL_BYTES
        );
    }

    // ---------- parse_expires_in ----------

    #[test]
    fn expires_in_reads_numbers_and_numeric_strings() {
        // Providers spell it both ways; both mean the same duration.
        for body in [json!({"expires_in": 3600}), json!({"expires_in": "3600"})] {
            assert_eq!(
                parse_expires_in(&body),
                Some(chrono::Duration::seconds(3600)),
                "{body}"
            );
        }
    }

    #[test]
    fn expires_in_treats_absent_and_unparsable_as_no_declared_expiry() {
        let cases = [
            json!({}),
            json!({"expires_in": null}),
            json!({"expires_in": 0}),
            json!({"expires_in": -5}),
            json!({"expires_in": "never"}),
            json!({"expires_in": 3600.5}),
            json!({"expires_in": true}),
        ];
        for body in cases {
            assert_eq!(parse_expires_in(&body), None, "{body}");
        }
    }
}
