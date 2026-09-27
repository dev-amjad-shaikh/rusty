//! What a manifest must carry before it is trusted: the things a vendor's
//! API requires that a hand-written manifest forgets, and the things any
//! credentialed connector must do. A placeholder-token probe cannot catch
//! these — Notion checks the token before the version header — so they are
//! known here and refused at registration, whether the manifest is shipped,
//! described, imported from OpenAPI, or built by an agent.
//!
//! Every finding names the operation, the problem and the exact fix.

use serde::{Deserialize, Serialize};

use super::manifest::ConnectorManifest;

/// One thing wrong with a manifest, and how to put it right.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LintFinding {
    /// The operation the finding is about, or `None` for the manifest.
    pub operation: Option<String>,
    pub problem: String,
    pub fix: String,
}

impl std::fmt::Display for LintFinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.operation {
            Some(op) => write!(f, "operation `{op}`: {} — {}", self.problem, self.fix),
            None => write!(f, "{} — {}", self.problem, self.fix),
        }
    }
}

/// A header a vendor's API requires on every request.
struct RequiredHeader {
    name: &'static str,
    example: &'static str,
    why: &'static str,
}

/// What known vendors require. Add a row when a connector to a new vendor
/// fails for a reason a token cannot explain; the row is the memory.
fn required_headers(host: &str) -> &'static [RequiredHeader] {
    match host {
        "api.notion.com" => &[RequiredHeader {
            name: "Notion-Version",
            example: "2022-06-28",
            why: "Notion rejects any request without its version header (400 missing_version) once the token is valid",
        }],
        _ => &[],
    }
}

/// The host of a base URL, lowercased, without a port.
fn host_of(base_url: &str) -> Option<String> {
    let rest = base_url.split("://").nth(1)?;
    let authority = rest.split('/').next()?;
    let host = authority.rsplit('@').next()?.split(':').next()?;
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// Everything wrong with `manifest` that registration must refuse.
pub fn lint(manifest: &ConnectorManifest) -> Vec<LintFinding> {
    let mut findings = Vec::new();
    // 1. The vendor's required headers, on every operation.
    if let Some(host) = host_of(&manifest.base_url) {
        for required in required_headers(&host) {
            for op in &manifest.operations {
                let present = op
                    .headers
                    .iter()
                    .any(|(name, _)| name.eq_ignore_ascii_case(required.name));
                if !present {
                    findings.push(LintFinding {
                        operation: Some(op.name.clone()),
                        problem: format!(
                            "{host} requires the `{}` header on every request; {}",
                            required.name, required.why
                        ),
                        fix: format!(
                            "add [\"{}\", \"{}\"] to the operation's headers",
                            required.name, required.example
                        ),
                    });
                }
            }
        }
    }
    // 2. A credentialed connector's check must send a credential: a check
    //    that never authenticates proves nothing about the token.
    let wants_credentials = manifest
        .connection_specification
        .pointer("/properties/credentials")
        .is_some();
    if wants_credentials {
        if let Some(check) = manifest.operation(&manifest.check) {
            if check.auth.is_empty() {
                findings.push(LintFinding {
                    operation: Some(check.name.clone()),
                    problem: "the connection check sends no credential, so a wrong token would pass".to_owned(),
                    fix: "give the check operation an `auth` entry that uses the connection's credentials".to_owned(),
                });
            }
        }
    }
    findings
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn notion(headers: serde_json::Value) -> ConnectorManifest {
        serde_json::from_value(json!({
            "id": "notion", "version": "1", "display_name": "Notion", "description": "d",
            "documentation_url": "https://developers.notion.com", "base_url": "https://api.notion.com",
            "connection_specification": {"type": "object", "properties": {"credentials": {"type": "object", "properties": {"integration_token": {"type": "string"}}}}},
            "operations": [{
                "name": "check-connection", "description": "who am I", "method": "GET", "path": "/v1/users/me",
                "effect": "read_only", "params_schema": {"type": "object"}, "headers": headers,
                "auth": [{"style": "bearer", "token": "{credentials.integration_token}"}]
            }],
            "check": "check-connection"
        }))
        .unwrap()
    }

    #[test]
    fn a_notion_manifest_without_its_version_header_is_named_with_the_fix() {
        let findings = lint(&notion(json!([])));
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].operation.as_deref(), Some("check-connection"));
        assert!(
            findings[0].problem.contains("Notion-Version"),
            "{}",
            findings[0]
        );
        assert!(findings[0].fix.contains("2022-06-28"), "{}", findings[0]);
        assert!(
            lint(&notion(json!([["notion-version", "2022-06-28"]]))).is_empty(),
            "the header name is case-insensitive"
        );
    }

    #[test]
    fn a_credentialed_check_that_sends_no_credential_is_a_finding() {
        let mut manifest = notion(json!([["Notion-Version", "2022-06-28"]]));
        manifest.operations[0].auth.clear();
        let findings = lint(&manifest);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(findings[0].problem.contains("sends no credential"));
    }

    #[test]
    fn an_unknown_host_has_no_vendor_rule() {
        assert_eq!(
            host_of("https://Api.Example.com:8443/v1/"),
            Some("api.example.com".into())
        );
        assert!(required_headers("api.example.com").is_empty());
    }
}
