//! Integration tests for the L7 egress policy evaluator (EP-11-S03).

use rusty_agent_runtime::egress::{
    EgressDecision, EgressDenialReason, EgressEndpoint, EgressEndpointPolicy, EgressPolicy,
    EgressProtocol, EgressRewrite, EgressRule, EgressRuleMode, PreflightResult, canonicalize_path,
    evaluate_egress, evaluate_redirect, preflight_egress,
};

fn sample_policy() -> EgressPolicy {
    EgressPolicy {
        policies: vec![
            EgressEndpointPolicy {
                name: "crm".into(),
                endpoint: EgressEndpoint {
                    host: "api.crm.example.com".into(),
                    port: 443,
                    protocol: EgressProtocol::Rest,
                    tls: true,
                    rewrite: EgressRewrite::default(),
                    allowed_ips: vec![],
                    allow_encoded_slashes: false,
                },
                rules: vec![
                    EgressRule {
                        methods: vec!["GET".into()],
                        path_pattern: "/api/v2/tickets/*".into(),
                        mode: EgressRuleMode::Enforce,
                        tool_names: None,
                    },
                    EgressRule {
                        methods: vec!["POST".into()],
                        path_pattern: "/api/v2/tickets".into(),
                        mode: EgressRuleMode::Enforce,
                        tool_names: None,
                    },
                ],
                originating: vec!["mcp-client".into()],
            },
            EgressEndpointPolicy {
                name: "payment".into(),
                endpoint: EgressEndpoint {
                    host: "payment.example.com".into(),
                    port: 443,
                    protocol: EgressProtocol::Rest,
                    tls: true,
                    rewrite: EgressRewrite::default(),
                    allowed_ips: vec![],
                    allow_encoded_slashes: false,
                },
                rules: vec![EgressRule {
                    methods: vec![],
                    path_pattern: "/v1/charge".into(),
                    mode: EgressRuleMode::Audit,
                    tool_names: None,
                }],
                originating: vec![],
            },
            EgressEndpointPolicy {
                name: "mcp-bridge".into(),
                endpoint: EgressEndpoint {
                    host: "mcp.example.com".into(),
                    port: 8080,
                    protocol: EgressProtocol::Mcp,
                    tls: false,
                    rewrite: EgressRewrite::default(),
                    allowed_ips: vec![],
                    allow_encoded_slashes: false,
                },
                rules: vec![EgressRule {
                    methods: vec!["POST".into()],
                    path_pattern: "/rpc".into(),
                    mode: EgressRuleMode::Enforce,
                    tool_names: Some(vec!["list_files".into(), "read_file".into()]),
                }],
                originating: vec!["mcp-client".into()],
            },
        ],
    }
}

#[test]
fn deny_by_default_no_matching_policy() {
    let policy = sample_policy();
    let decision = evaluate_egress(
        &policy,
        "unknown.example.com",
        443,
        EgressProtocol::Rest,
        "GET",
        "/",
        None,
        "mcp-client",
    );
    assert_eq!(
        decision,
        EgressDecision::Deny {
            reason: EgressDenialReason::NoMatchingPolicy,
            policy_name: None,
        }
    );
}

#[test]
fn deny_by_default_no_matching_rule() {
    let policy = sample_policy();
    let decision = evaluate_egress(
        &policy,
        "api.crm.example.com",
        443,
        EgressProtocol::Rest,
        "DELETE",
        "/api/v2/tickets/123",
        None,
        "mcp-client",
    );
    assert_eq!(
        decision,
        EgressDecision::Deny {
            reason: EgressDenialReason::NoMatchingRule,
            policy_name: Some("crm".into()),
        }
    );
}

#[test]
fn method_path_matrix_get_admitted() {
    let policy = sample_policy();
    let decision = evaluate_egress(
        &policy,
        "api.crm.example.com",
        443,
        EgressProtocol::Rest,
        "GET",
        "/api/v2/tickets/123",
        None,
        "mcp-client",
    );
    assert_eq!(decision, EgressDecision::Allow);
}

#[test]
fn method_path_matrix_post_refused() {
    let policy = sample_policy();
    let decision = evaluate_egress(
        &policy,
        "api.crm.example.com",
        443,
        EgressProtocol::Rest,
        "POST",
        "/api/v2/tickets/123",
        None,
        "mcp-client",
    );
    assert_eq!(
        decision,
        EgressDecision::Deny {
            reason: EgressDenialReason::NoMatchingRule,
            policy_name: Some("crm".into()),
        }
    );
}

#[test]
fn method_path_matrix_sibling_path_refused() {
    let policy = sample_policy();
    let decision = evaluate_egress(
        &policy,
        "api.crm.example.com",
        443,
        EgressProtocol::Rest,
        "GET",
        "/api/v2/tickets/123/comments",
        None,
        "mcp-client",
    );
    assert_eq!(
        decision,
        EgressDecision::Deny {
            reason: EgressDenialReason::NoMatchingRule,
            policy_name: Some("crm".into()),
        }
    );
}

#[test]
fn audit_mode_returns_audit() {
    let policy = sample_policy();
    let decision = evaluate_egress(
        &policy,
        "payment.example.com",
        443,
        EgressProtocol::Rest,
        "POST",
        "/v1/charge",
        None,
        "any-component",
    );
    assert_eq!(
        decision,
        EgressDecision::Audit {
            policy_name: "payment".into(),
            rule_index: 0,
        }
    );
}

#[test]
fn cross_component_refusal() {
    let policy = sample_policy();
    let decision = evaluate_egress(
        &policy,
        "api.crm.example.com",
        443,
        EgressProtocol::Rest,
        "GET",
        "/api/v2/tickets/123",
        None,
        "code-mode-script",
    );
    assert_eq!(
        decision,
        EgressDecision::Deny {
            reason: EgressDenialReason::ComponentNotGranted,
            policy_name: Some("crm".into()),
        }
    );
}

#[test]
fn mcp_tool_name_admitted() {
    let policy = sample_policy();
    let decision = evaluate_egress(
        &policy,
        "mcp.example.com",
        8080,
        EgressProtocol::Mcp,
        "POST",
        "/rpc",
        Some("read_file"),
        "mcp-client",
    );
    assert_eq!(decision, EgressDecision::Allow);
}

#[test]
fn mcp_tool_name_refused() {
    let policy = sample_policy();
    let decision = evaluate_egress(
        &policy,
        "mcp.example.com",
        8080,
        EgressProtocol::Mcp,
        "POST",
        "/rpc",
        Some("delete_file"),
        "mcp-client",
    );
    assert_eq!(
        decision,
        EgressDecision::Deny {
            reason: EgressDenialReason::NoMatchingRule,
            policy_name: Some("mcp-bridge".into()),
        }
    );
}

#[test]
fn empty_originating_means_all_components() {
    let policy = sample_policy();
    let decision = evaluate_egress(
        &policy,
        "payment.example.com",
        443,
        EgressProtocol::Rest,
        "POST",
        "/v1/charge",
        None,
        "arbitrary-component",
    );
    assert_eq!(
        decision,
        EgressDecision::Audit {
            policy_name: "payment".into(),
            rule_index: 0,
        }
    );
}

#[test]
fn validation_duplicate_policy_names() {
    let policy = EgressPolicy {
        policies: vec![
            EgressEndpointPolicy {
                name: "dup".into(),
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
                name: "dup".into(),
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
    assert!(policy.validate().unwrap().contains("not unique"));
}

#[test]
fn validation_empty_host() {
    let policy = EgressPolicy {
        policies: vec![EgressEndpointPolicy {
            name: "bad".into(),
            endpoint: EgressEndpoint {
                host: "".into(),
                port: 443,
                protocol: EgressProtocol::Rest,
                tls: true,
                rewrite: EgressRewrite::default(),
                allowed_ips: vec![],
                allow_encoded_slashes: false,
            },
            rules: vec![],
            originating: vec![],
        }],
    };
    assert!(policy.validate().unwrap().contains("host is empty"));
}

#[test]
fn validation_zero_port() {
    let policy = EgressPolicy {
        policies: vec![EgressEndpointPolicy {
            name: "bad".into(),
            endpoint: EgressEndpoint {
                host: "example.com".into(),
                port: 0,
                protocol: EgressProtocol::Rest,
                tls: true,
                rewrite: EgressRewrite::default(),
                allowed_ips: vec![],
                allow_encoded_slashes: false,
            },
            rules: vec![],
            originating: vec![],
        }],
    };
    assert!(policy.validate().unwrap().contains("port is zero"));
}

#[test]
fn validation_missing_leading_slash() {
    let policy = EgressPolicy {
        policies: vec![EgressEndpointPolicy {
            name: "bad".into(),
            endpoint: EgressEndpoint {
                host: "example.com".into(),
                port: 443,
                protocol: EgressProtocol::Rest,
                tls: true,
                rewrite: EgressRewrite::default(),
                allowed_ips: vec![],
                allow_encoded_slashes: false,
            },
            rules: vec![EgressRule {
                methods: vec![],
                path_pattern: "api/v2".into(),
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

#[test]
fn validation_empty_tool_names() {
    let policy = EgressPolicy {
        policies: vec![EgressEndpointPolicy {
            name: "bad".into(),
            endpoint: EgressEndpoint {
                host: "example.com".into(),
                port: 443,
                protocol: EgressProtocol::Rest,
                tls: true,
                rewrite: EgressRewrite::default(),
                allowed_ips: vec![],
                allow_encoded_slashes: false,
            },
            rules: vec![EgressRule {
                methods: vec![],
                path_pattern: "/api".into(),
                mode: EgressRuleMode::Enforce,
                tool_names: Some(vec![]),
            }],
            originating: vec![],
        }],
    };
    assert!(policy.validate().unwrap().contains("tool_names is empty"));
}

#[test]
fn validation_duplicate_originating() {
    let policy = EgressPolicy {
        policies: vec![EgressEndpointPolicy {
            name: "bad".into(),
            endpoint: EgressEndpoint {
                host: "example.com".into(),
                port: 443,
                protocol: EgressProtocol::Rest,
                tls: true,
                rewrite: EgressRewrite::default(),
                allowed_ips: vec![],
                allow_encoded_slashes: false,
            },
            rules: vec![],
            originating: vec!["a".into(), "a".into()],
        }],
    };
    assert!(
        policy
            .validate()
            .unwrap()
            .contains("originating contains duplicates")
    );
}

#[test]
fn canonicalize_path_basic() {
    assert_eq!(
        canonicalize_path("/api/v2/tickets/123", false).unwrap(),
        "/api/v2/tickets/123"
    );
}

#[test]
fn canonicalize_path_collapse_slashes() {
    assert_eq!(
        canonicalize_path("/api//v2///tickets", false).unwrap(),
        "/api/v2/tickets"
    );
}

#[test]
fn canonicalize_path_remove_dot_segments() {
    assert_eq!(
        canonicalize_path("/api/v2/../health", false).unwrap(),
        "/api/health"
    );
    assert_eq!(
        canonicalize_path("/api/./v2/tickets", false).unwrap(),
        "/api/v2/tickets"
    );
}

#[test]
fn canonicalize_path_percent_decode() {
    assert_eq!(
        canonicalize_path("/api/hello%20world", false).unwrap(),
        "/api/hello world"
    );
}

#[test]
fn canonicalize_path_encoded_slash_refused() {
    assert_eq!(
        canonicalize_path("/api/%2Fsecret", false),
        Err(EgressDenialReason::PathNotCanonical)
    );
}

#[test]
fn canonicalize_path_encoded_slash_allowed() {
    assert_eq!(
        canonicalize_path("/api/%2Fsecret", true).unwrap(),
        "/api/secret"
    );
}

#[test]
fn preflight_private_range_refused_without_pin() {
    let endpoint = EgressEndpoint {
        host: "internal.example.com".into(),
        port: 443,
        protocol: EgressProtocol::Rest,
        tls: true,
        rewrite: EgressRewrite::default(),
        allowed_ips: vec![],
        allow_encoded_slashes: false,
    };
    let result = preflight_egress(&endpoint, &["192.168.1.1".into()]);
    assert_eq!(
        result,
        PreflightResult::Denied {
            reason: EgressDenialReason::PreflightFailed,
            detail: "internal.example.com resolved to private/loopback/link-local 192.168.1.1 with no allowed_ips pin".into(),
        }
    );
}

#[test]
fn preflight_private_range_allowed_with_pin() {
    let endpoint = EgressEndpoint {
        host: "internal.example.com".into(),
        port: 443,
        protocol: EgressProtocol::Rest,
        tls: true,
        rewrite: EgressRewrite::default(),
        allowed_ips: vec!["192.168.1.1".into()],
        allow_encoded_slashes: false,
    };
    let result = preflight_egress(&endpoint, &["192.168.1.1".into()]);
    assert_eq!(
        result,
        PreflightResult::Allowed {
            ip: "192.168.1.1".into(),
        }
    );
}

#[test]
fn preflight_public_range_allowed_without_pin() {
    let endpoint = EgressEndpoint {
        host: "api.example.com".into(),
        port: 443,
        protocol: EgressProtocol::Rest,
        tls: true,
        rewrite: EgressRewrite::default(),
        allowed_ips: vec![],
        allow_encoded_slashes: false,
    };
    let result = preflight_egress(&endpoint, &["93.184.216.34".into()]);
    assert_eq!(
        result,
        PreflightResult::Allowed {
            ip: "93.184.216.34".into(),
        }
    );
}

#[test]
fn preflight_ip_outside_pin_refused() {
    let endpoint = EgressEndpoint {
        host: "api.example.com".into(),
        port: 443,
        protocol: EgressProtocol::Rest,
        tls: true,
        rewrite: EgressRewrite::default(),
        allowed_ips: vec!["93.184.216.34".into()],
        allow_encoded_slashes: false,
    };
    let result = preflight_egress(&endpoint, &["1.2.3.4".into()]);
    assert_eq!(
        result,
        PreflightResult::Denied {
            reason: EgressDenialReason::IpNotPinned,
            detail: "api.example.com resolved to 1.2.3.4 which is outside the allowed_ips pin set"
                .into(),
        }
    );
}

#[test]
fn preflight_loopback_refused() {
    let endpoint = EgressEndpoint {
        host: "localhost".into(),
        port: 8080,
        protocol: EgressProtocol::Rest,
        tls: false,
        rewrite: EgressRewrite::default(),
        allowed_ips: vec![],
        allow_encoded_slashes: false,
    };
    let result = preflight_egress(&endpoint, &["127.0.0.1".into()]);
    assert_eq!(
        result,
        PreflightResult::Denied {
            reason: EgressDenialReason::PreflightFailed,
            detail: "localhost resolved to private/loopback/link-local 127.0.0.1 with no allowed_ips pin".into(),
        }
    );
}

#[test]
fn evaluate_redirect_uses_full_policy() {
    let policy = sample_policy();
    let decision = evaluate_redirect(
        &policy,
        "unknown.example.com",
        443,
        EgressProtocol::Rest,
        "GET",
        "/",
        None,
        "mcp-client",
    );
    assert_eq!(
        decision,
        EgressDecision::Deny {
            reason: EgressDenialReason::NoMatchingPolicy,
            policy_name: None,
        }
    );
}

#[test]
fn evaluate_redirect_admits_policied_target() {
    let policy = sample_policy();
    let decision = evaluate_redirect(
        &policy,
        "api.crm.example.com",
        443,
        EgressProtocol::Rest,
        "GET",
        "/api/v2/tickets/123",
        None,
        "mcp-client",
    );
    assert_eq!(decision, EgressDecision::Allow);
}

#[test]
fn validation_bad_allowed_ip() {
    let policy = EgressPolicy {
        policies: vec![EgressEndpointPolicy {
            name: "bad".into(),
            endpoint: EgressEndpoint {
                host: "example.com".into(),
                port: 443,
                protocol: EgressProtocol::Rest,
                tls: true,
                rewrite: EgressRewrite::default(),
                allowed_ips: vec!["not-an-ip".into()],
                allow_encoded_slashes: false,
            },
            rules: vec![],
            originating: vec![],
        }],
    };
    assert!(
        policy
            .validate()
            .unwrap()
            .contains("not a valid IP address")
    );
}
