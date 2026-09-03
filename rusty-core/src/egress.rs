//! Layer-7 egress policy: destination × method × path × originating
//! component (EP-11-S03).
//!
//! Core owns the policy vocabulary, the validation contract, and the
//! evaluator — the pure function that decides whether one outbound
//! request is admitted, denied, or audited. The server owns the HTTP
//! interception and the audit-log emission.
//!
//! Design posture: deny-by-default. Every request that does not match
//! an explicit grant is refused with a typed, attributable reason.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// The wire protocol of an egress endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum EgressProtocol {
    /// HTTP/REST.
    Rest,
    /// WebSocket.
    Websocket,
    /// MCP (Model Context Protocol) over stdio or HTTP.
    Mcp,
}

/// Per-endpoint rewrite flags: which placeholder-substitution points
/// are active for this destination.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct EgressRewrite {
    /// Substitute placeholders in request headers.
    #[serde(default)]
    pub header_rewrite: bool,
    /// Substitute placeholders in the request body.
    #[serde(default)]
    pub request_body_rewrite: bool,
    /// Substitute placeholders in WebSocket frames.
    #[serde(default)]
    pub websocket_rewrite: bool,
}

/// One destination endpoint declared in the policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct EgressEndpoint {
    /// Hostname or IP (the preflight-checked value, never a raw user
    /// string — canonicalization is the caller's responsibility).
    pub host: String,
    /// TCP port.
    pub port: u16,
    /// Wire protocol.
    pub protocol: EgressProtocol,
    /// TLS is required for this endpoint.
    #[serde(default)]
    pub tls: bool,
    /// Which rewrite points are active.
    #[serde(default)]
    pub rewrite: EgressRewrite,
    /// Explicit IP pins for this endpoint. When non-empty, DNS
    /// preflight must resolve to one of these addresses; otherwise the
    /// connection is refused (EP-11-S04 AC 3).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_ips: Vec<String>,
    /// When `true`, percent-encoded slashes (`%2F`) in path segments
    /// are accepted. `false` by default — they are refused (EP-11-S04
    /// AC 4).
    #[serde(default)]
    pub allow_encoded_slashes: bool,
}

/// A rule's enforcement mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum EgressRuleMode {
    /// Matching traffic is admitted; a denial is a hard refusal.
    Enforce,
    /// Matching traffic is admitted but an audit record is written
    /// identical in shape to a refusal — progressive rollout without
    /// schema change.
    Audit,
}

/// One rule in an endpoint policy: a grant scoped to method, path
/// pattern, and (for MCP) tool name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct EgressRule {
    /// HTTP methods this rule matches (e.g. `["GET"]`). Empty means
    /// "all methods".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub methods: Vec<String>,
    /// Path glob. `*` matches one segment; `/*` at the end matches any
    /// suffix. `/api/v2/tickets/*` admits `/api/v2/tickets/123` and
    /// refuses `/api/v2/tickets/123/comments`.
    pub path_pattern: String,
    /// Enforcement mode.
    pub mode: EgressRuleMode,
    /// For MCP rules: the tool names this grant covers. `None` means
    /// "all tools on this endpoint".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_names: Option<Vec<String>>,
}

/// The typed refusal when egress is denied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum EgressDenialReason {
    /// No policy covers this destination.
    NoMatchingPolicy,
    /// A policy matched the destination but no rule matched the
    /// method/path/tool.
    NoMatchingRule,
    /// The originating component is not in the policy's `originating`
    /// list.
    ComponentNotGranted,
    /// The rule that matched is in audit mode — this variant is never
    /// used for a hard denial; it exists for symmetry with the audit
    /// record shape.
    AuditMode,
    /// DNS preflight resolved to a private, loopback, or link-local
    /// range not covered by the endpoint's `allowed_ips` pin.
    PreflightFailed,
    /// DNS preflight resolved to an address outside the endpoint's
    /// `allowed_ips` pin set.
    IpNotPinned,
    /// The path contains an encoded slash (`%2F`) and the endpoint
    /// does not set `allow_encoded_slashes`.
    PathNotCanonical,
    /// An HTTP redirect target is off-policy (EP-11-S04 AC 5).
    RedirectOffPolicy,
}

/// The decision the evaluator renders for one request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum EgressDecision {
    /// Admitted: a matching enforce rule covered the request.
    Allow,
    /// Refused: deny-by-default or an explicit policy boundary.
    Deny {
        reason: EgressDenialReason,
        /// The policy name that decided, when one matched.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        policy_name: Option<String>,
    },
    /// Admitted under audit: a matching audit-rule covered the
    /// request. The caller must emit an audit record.
    Audit {
        /// The policy name that decided.
        policy_name: String,
        /// The rule index that matched.
        rule_index: usize,
    },
}

/// One named policy: an endpoint plus the ordered rules and the
/// originating-component grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct EgressEndpointPolicy {
    /// Human-readable policy name (the audit trail names this).
    pub name: String,
    /// The destination endpoint.
    pub endpoint: EgressEndpoint,
    /// Ordered rules: first match wins.
    pub rules: Vec<EgressRule>,
    /// Components permitted to use this grant. Empty means "all
    /// components".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub originating: Vec<String>,
}

/// The full egress policy document: a set of named endpoint policies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct EgressPolicy {
    /// Named endpoint policies. Evaluation iterates in document order;
    /// the first policy whose endpoint matches the destination is the
    /// one whose rules are consulted.
    pub policies: Vec<EgressEndpointPolicy>,
}

impl EgressPolicy {
    /// Validate the policy document. Returns the path of the first
    /// problem found, or `None` when the document is sound.
    pub fn validate(&self) -> Option<String> {
        for (pi, policy) in self.policies.iter().enumerate() {
            if policy.name.is_empty() {
                return Some(format!("policies[{pi}].name is empty"));
            }
            if policy.endpoint.host.is_empty() {
                return Some(format!("policies[{pi}].endpoint.host is empty"));
            }
            if policy.endpoint.port == 0 {
                return Some(format!("policies[{pi}].endpoint.port is zero"));
            }
            for (ri, rule) in policy.rules.iter().enumerate() {
                if !rule.path_pattern.starts_with('/') {
                    return Some(format!(
                        "policies[{pi}].rules[{ri}].path_pattern must start with /"
                    ));
                }
                if let Some(ref tools) = rule.tool_names {
                    if tools.is_empty() {
                        return Some(format!("policies[{pi}].rules[{ri}].tool_names is empty"));
                    }
                    let set: BTreeSet<_> = tools.iter().cloned().collect();
                    if set.len() != tools.len() {
                        return Some(format!(
                            "policies[{pi}].rules[{ri}].tool_names contains duplicates"
                        ));
                    }
                }
            }
            let names: BTreeSet<_> = policy.originating.iter().cloned().collect();
            if names.len() != policy.originating.len() {
                return Some(format!("policies[{pi}].originating contains duplicates"));
            }
            for (ai, ip) in policy.endpoint.allowed_ips.iter().enumerate() {
                if ip.parse::<std::net::IpAddr>().is_err() {
                    return Some(format!(
                        "policies[{pi}].endpoint.allowed_ips[{ai}] is not a valid IP address"
                    ));
                }
            }
        }
        // Policy names must be unique — they are the audit key.
        let policy_names: BTreeSet<_> = self.policies.iter().map(|p| p.name.clone()).collect();
        if policy_names.len() != self.policies.len() {
            return Some("policy names are not unique".into());
        }
        None
    }
}

/// Evaluate one outbound request against the policy.
#[allow(clippy::too_many_arguments)]
///
/// - `host` — the destination host (canonicalized).
/// - `port` — the destination port.
/// - `protocol` — the wire protocol.
/// - `method` — the HTTP method (e.g. `"GET"`).
/// - `path` — the request path (canonicalized, starting with `/`).
/// - `tool_name` — the MCP tool name, when the request is an MCP frame.
/// - `originating_component` — the component identity per
///   `contracts:turn-stamp`.
pub fn evaluate_egress(
    policy: &EgressPolicy,
    host: &str,
    port: u16,
    protocol: EgressProtocol,
    method: &str,
    path: &str,
    tool_name: Option<&str>,
    originating_component: &str,
) -> EgressDecision {
    // Find the first policy whose endpoint matches.
    let matched_policy = policy.policies.iter().find(|p| {
        p.endpoint.host == host && p.endpoint.port == port && p.endpoint.protocol == protocol
    });

    let Some(policy) = matched_policy else {
        return EgressDecision::Deny {
            reason: EgressDenialReason::NoMatchingPolicy,
            policy_name: None,
        };
    };

    // Component check.
    if !policy.originating.is_empty()
        && !policy
            .originating
            .iter()
            .any(|c| c == originating_component)
    {
        return EgressDecision::Deny {
            reason: EgressDenialReason::ComponentNotGranted,
            policy_name: Some(policy.name.clone()),
        };
    }

    // Rule matching: first match wins.
    for (ri, rule) in policy.rules.iter().enumerate() {
        if !rule.methods.is_empty() && !rule.methods.iter().any(|m| m == method) {
            continue;
        }
        if !path_matches(&rule.path_pattern, path) {
            continue;
        }
        if let Some(ref allowed_tools) = rule.tool_names {
            let Some(tn) = tool_name else {
                continue;
            };
            if !allowed_tools.iter().any(|t| t == tn) {
                continue;
            }
        }
        // First matching rule decides.
        return match rule.mode {
            EgressRuleMode::Enforce => EgressDecision::Allow,
            EgressRuleMode::Audit => EgressDecision::Audit {
                policy_name: policy.name.clone(),
                rule_index: ri,
            },
        };
    }

    // Deny-by-default when no rule matched.
    EgressDecision::Deny {
        reason: EgressDenialReason::NoMatchingRule,
        policy_name: Some(policy.name.clone()),
    }
}

/// Find the first endpoint policy that matches the given destination.
/// Returns the endpoint policy so the caller can run DNS preflight against it.
pub fn find_endpoint_policy<'a>(
    policy: &'a EgressPolicy,
    host: &str,
    port: u16,
    protocol: EgressProtocol,
) -> Option<&'a EgressEndpointPolicy> {
    policy.policies.iter().find(|p| {
        p.endpoint.host == host && p.endpoint.port == port && p.endpoint.protocol == protocol
    })
}
///
/// Rules:
/// - `*` matches exactly one non-empty path segment (no `/`).
/// - All other characters match literally.
fn path_matches(pattern: &str, path: &str) -> bool {
    let pat_parts: Vec<&str> = pattern.split('/').collect();
    let path_parts: Vec<&str> = path.split('/').collect();

    if pat_parts.len() != path_parts.len() {
        return false;
    }
    pat_parts
        .iter()
        .zip(path_parts.iter())
        .all(|(p, s)| segment_matches(p, s))
}

fn segment_matches(pattern: &str, segment: &str) -> bool {
    if pattern == "*" {
        !segment.is_empty()
    } else {
        pattern == segment
    }
}

/// Canonicalize a path: percent-decode, collapse duplicate slashes,
/// remove dot-segments, and refuse encoded slashes unless the endpoint
/// permits them (EP-11-S04 AC 4).
///
/// Returns `Ok(canonical_path)` on success, or `Err` with the reason
/// when the path is not canonical.
pub fn canonicalize_path(
    path: &str,
    allow_encoded_slashes: bool,
) -> Result<String, EgressDenialReason> {
    // Refuse encoded slashes early unless permitted.
    if !allow_encoded_slashes && path.to_ascii_lowercase().contains("%2f") {
        return Err(EgressDenialReason::PathNotCanonical);
    }

    // Percent-decode.
    let decoded = percent_decode(path);

    // Split into segments and process dot-segments.
    let mut segments: Vec<String> = Vec::new();
    for seg in decoded.split('/') {
        if seg.is_empty() {
            continue;
        }
        if seg == "." {
            continue;
        }
        if seg == ".." {
            segments.pop();
            continue;
        }
        segments.push(seg.to_string());
    }

    // Rebuild with leading slash.
    let mut out = String::new();
    out.push('/');
    for (i, seg) in segments.iter().enumerate() {
        if i > 0 {
            out.push('/');
        }
        out.push_str(seg);
    }
    Ok(out)
}

fn percent_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = hex_value(bytes[i + 1]);
            let lo = hex_value(bytes[i + 2]);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as char);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Result of DNS preflight for one endpoint (EP-11-S04 AC 1–3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum PreflightResult {
    /// The hostname resolved to an allowed address.
    Allowed {
        /// The pinned IP address the connection must use.
        ip: String,
    },
    /// The hostname resolved to a disallowed address.
    Denied {
        /// The typed refusal reason.
        reason: EgressDenialReason,
        /// Human-readable detail for logs.
        detail: String,
    },
}

/// Run DNS preflight for a destination against its endpoint policy.
///
/// `resolver` is an injected function `hostname -> Vec<String>` of
/// resolved IP addresses so tests can use a fixture DNS server.
pub fn preflight_egress(endpoint: &EgressEndpoint, resolved_ips: &[String]) -> PreflightResult {
    if resolved_ips.is_empty() {
        return PreflightResult::Denied {
            reason: EgressDenialReason::PreflightFailed,
            detail: format!("{} resolved to no addresses", endpoint.host),
        };
    }

    for ip_str in resolved_ips {
        let Ok(ip) = ip_str.parse::<std::net::IpAddr>() else {
            return PreflightResult::Denied {
                reason: EgressDenialReason::PreflightFailed,
                detail: format!("{} resolved to invalid IP {}", endpoint.host, ip_str),
            };
        };

        if is_private_or_loopback_or_link_local(&ip) {
            // Private / loopback / link-local requires an explicit pin.
            if endpoint.allowed_ips.is_empty() {
                return PreflightResult::Denied {
                    reason: EgressDenialReason::PreflightFailed,
                    detail: format!(
                        "{} resolved to private/loopback/link-local {} with no allowed_ips pin",
                        endpoint.host, ip_str
                    ),
                };
            }
            if !endpoint.allowed_ips.iter().any(|p| p == ip_str) {
                return PreflightResult::Denied {
                    reason: EgressDenialReason::IpNotPinned,
                    detail: format!(
                        "{} resolved to {} which is outside the allowed_ips pin set",
                        endpoint.host, ip_str
                    ),
                };
            }
        }

        // If allowed_ips is non-empty, every resolved address must be in the set.
        if !endpoint.allowed_ips.is_empty() && !endpoint.allowed_ips.iter().any(|p| p == ip_str) {
            return PreflightResult::Denied {
                reason: EgressDenialReason::IpNotPinned,
                detail: format!(
                    "{} resolved to {} which is outside the allowed_ips pin set",
                    endpoint.host, ip_str
                ),
            };
        }
    }

    // All resolved addresses passed checks; pin the first one.
    PreflightResult::Allowed {
        ip: resolved_ips[0].clone(),
    }
}

fn is_private_or_loopback_or_link_local(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_multicast()
                || v4.is_broadcast()
                || v4.is_documentation()
        }
        std::net::IpAddr::V6(v6) => v6.is_loopback() || v6.is_multicast() || is_v6_link_local(v6),
    }
}

fn is_v6_link_local(v6: &std::net::Ipv6Addr) -> bool {
    // fe80::/10
    let segs = v6.segments();
    (segs[0] & 0xffc0) == 0xfe80
}

/// Re-evaluate an HTTP redirect target against the full policy (EP-11-S04
/// AC 5). Returns the decision for the redirect as if it were a fresh
/// request.
#[allow(clippy::too_many_arguments)]
pub fn evaluate_redirect(
    policy: &EgressPolicy,
    redirect_host: &str,
    redirect_port: u16,
    redirect_protocol: EgressProtocol,
    redirect_method: &str,
    redirect_path: &str,
    tool_name: Option<&str>,
    originating_component: &str,
) -> EgressDecision {
    evaluate_egress(
        policy,
        redirect_host,
        redirect_port,
        redirect_protocol,
        redirect_method,
        redirect_path,
        tool_name,
        originating_component,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_globs() {
        assert!(path_matches("/api/v2/tickets/*", "/api/v2/tickets/123"));
        assert!(!path_matches(
            "/api/v2/tickets/*",
            "/api/v2/tickets/123/comments"
        ));
        assert!(path_matches(
            "/api/v2/tickets/*/comments",
            "/api/v2/tickets/123/comments"
        ));
        assert!(!path_matches(
            "/api/v2/tickets/*/comments",
            "/api/v2/tickets/123"
        ));
        assert!(path_matches("/health", "/health"));
        assert!(!path_matches("/health", "/healthz"));
    }

    #[test]
    fn empty_policy_validates() {
        let policy = EgressPolicy { policies: vec![] };
        assert!(policy.validate().is_none());
    }

    #[test]
    fn duplicate_policy_names_invalid() {
        let policy = EgressPolicy {
            policies: vec![
                EgressEndpointPolicy {
                    name: "same".into(),
                    endpoint: EgressEndpoint {
                        host: "a.example.com".into(),
                        port: 443,
                        protocol: EgressProtocol::Rest,
                        tls: true,
                        rewrite: EgressRewrite::default(),
                        allowed_ips: vec![],
                        allow_encoded_slashes: false,
                    },
                    rules: vec![],
                    originating: vec![],
                },
                EgressEndpointPolicy {
                    name: "same".into(),
                    endpoint: EgressEndpoint {
                        host: "b.example.com".into(),
                        port: 443,
                        protocol: EgressProtocol::Rest,
                        tls: true,
                        rewrite: EgressRewrite::default(),
                        allowed_ips: vec![],
                        allow_encoded_slashes: false,
                    },
                    rules: vec![],
                    originating: vec![],
                },
            ],
        };
        assert_eq!(
            policy.validate(),
            Some("policy names are not unique".into())
        );
    }

    #[test]
    fn bad_path_pattern_fails_validation() {
        let policy = EgressPolicy {
            policies: vec![EgressEndpointPolicy {
                name: "crm".into(),
                endpoint: EgressEndpoint {
                    host: "api.example.com".into(),
                    port: 443,
                    protocol: EgressProtocol::Rest,
                    tls: true,
                    rewrite: EgressRewrite::default(),
                    allowed_ips: vec![],
                    allow_encoded_slashes: false,
                },
                rules: vec![EgressRule {
                    methods: vec!["GET".into()],
                    path_pattern: "api/v2".into(), // missing leading /
                    mode: EgressRuleMode::Enforce,
                    tool_names: None,
                }],
                originating: vec![],
            }],
        };
        assert!(
            policy
                .validate()
                .unwrap()
                .contains("path_pattern must start with")
        );
    }

    // -----------------------------------------------------------------
    // Schema generation and validation (EP-11-S03 AC 1)
    // -----------------------------------------------------------------

    /// The golden snapshot path: the published schema generated from the Rust
    /// types. A diff-guard test below fails when the schema drifts.
    const GOLDEN_PATH: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/schemas/egress-policy.json"
    );

    #[test]
    fn schema_matches_golden() {
        let schema = schemars::schema_for!(EgressPolicy);
        let generated: serde_json::Value =
            serde_json::to_value(&schema).expect("schema serializes");
        let golden = std::fs::read_to_string(GOLDEN_PATH).unwrap_or_else(|e| {
            panic!("read golden {GOLDEN_PATH}: {e}\nRun the regenerate test to create it.")
        });
        let expected: serde_json::Value = serde_json::from_str(&golden)
            .unwrap_or_else(|e| panic!("parse golden {GOLDEN_PATH}: {e}"));

        if generated != expected {
            let pretty = serde_json::to_string_pretty(&generated).expect("pretty");
            panic!(
                "egress policy schema drift detected.\n\
                 Run `schema_regenerate_golden` to update the golden.\n\
                 Generated:\n{pretty}"
            );
        }
    }

    #[test]
    fn schema_regenerate_golden() {
        // Writes the golden only on explicit request: an unconditional write
        // races `schema_matches_golden` under parallel test execution and
        // would silently bless drift in CI.
        if std::env::var_os("UPDATE_GOLDEN").is_none() {
            return;
        }
        let schema = schemars::schema_for!(EgressPolicy);
        let pretty = serde_json::to_string_pretty(&schema).expect("pretty");
        let dir = std::path::Path::new(GOLDEN_PATH).parent().unwrap();
        std::fs::create_dir_all(dir).expect("create schema dir");
        std::fs::write(GOLDEN_PATH, pretty).expect("write golden");
    }

    #[test]
    fn schema_validation_corpus() {
        let golden = std::fs::read_to_string(GOLDEN_PATH)
            .unwrap_or_else(|e| panic!("read golden {GOLDEN_PATH}: {e}"));
        let schema_value: serde_json::Value =
            serde_json::from_str(&golden).unwrap_or_else(|e| panic!("parse golden: {e}"));
        let validator = jsonschema::draft7::new(&schema_value)
            .unwrap_or_else(|e| panic!("compile schema: {e}"));

        // Valid: minimal policy.
        let valid: serde_json::Value = serde_json::json!({ "policies": [] });
        assert!(validator.is_valid(&valid), "empty policy should validate");

        // Valid: full sample policy.
        let valid = serde_json::json!({
            "policies": [
                {
                    "name": "crm",
                    "endpoint": {
                        "host": "api.crm.example.com",
                        "port": 443,
                        "protocol": "rest",
                        "tls": true,
                        "rewrite": { "header_rewrite": true }
                    },
                    "rules": [
                        {
                            "methods": ["GET"],
                            "path_pattern": "/api/v2/tickets/*",
                            "mode": "enforce"
                        }
                    ],
                    "originating": ["mcp-client"]
                }
            ]
        });
        assert!(
            validator.is_valid(&valid),
            "full sample policy should validate"
        );

        // Invalid: missing required field `name`.
        let invalid = serde_json::json!({
            "policies": [{
                "endpoint": {
                    "host": "x",
                    "port": 443,
                    "protocol": "rest"
                },
                "rules": []
            }]
        });
        assert!(
            !validator.is_valid(&invalid),
            "missing name should fail schema"
        );

        // Invalid: wrong protocol enum.
        let invalid = serde_json::json!({
            "policies": [{
                "name": "bad",
                "endpoint": {
                    "host": "x",
                    "port": 443,
                    "protocol": "ftp"
                },
                "rules": []
            }]
        });
        assert!(
            !validator.is_valid(&invalid),
            "wrong protocol should fail schema"
        );
    }
}
