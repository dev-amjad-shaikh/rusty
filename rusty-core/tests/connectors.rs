//! Connector surface tests: manifest declaration validation, placeholder
//! declaration + substitution, schema validation with field-path errors,
//! secret extraction, check semantics, and catalog derivation.

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::Utc;
use rusty_agent_runtime::connector::check::render_operation_request;
use rusty_agent_runtime::connector::config::without_secrets;
use rusty_agent_runtime::connector::{
    execute_check, extract_secrets, insert_masked_secrets, insert_opened_secrets, render_template,
    scan_placeholders, validate_config, CheckOutcome, CheckRequest, CheckResponse, CheckStatus,
    ConnectorInstance, ConnectorManifest, ConnectorOperation, ConnectorTransport, HttpMethod,
    OperationAuth, OperationEffect,
};
use rusty_agent_runtime::error::Result as RuntimeResult;
use serde_json::{json, Value};

/// The ServiceNow-shaped fixture: `instance` + `credentials` oneOf
/// basic/oauth, three Table API operations, parameterless GET check.
fn demo_spec() -> Value {
    json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "title": "ServiceNow Connection Spec",
        "type": "object",
        "required": ["instance", "credentials"],
        "additionalProperties": false,
        "properties": {
            "instance": {
                "type": "string",
                "pattern": "^[a-z0-9-]+$",
                "rusty_pattern_descriptor": "your-instance.service-now.com",
                "rusty_order": 0
            },
            "credentials": {
                "type": "object",
                "rusty_order": 1,
                "rusty_group": "auth",
                "oneOf": [
                    {
                        "title": "Basic",
                        "type": "object",
                        "required": ["auth", "username", "password"],
                        "additionalProperties": false,
                        "properties": {
                            "auth": {"type": "string", "const": "basic"},
                            "username": {"type": "string", "rusty_secret": true},
                            "password": {"type": "string", "rusty_secret": true}
                        }
                    },
                    {
                        "title": "OAuth token",
                        "type": "object",
                        "required": ["auth", "token"],
                        "additionalProperties": false,
                        "properties": {
                            "auth": {"type": "string", "const": "oauth"},
                            "token": {"type": "string", "rusty_secret": true}
                        }
                    }
                ]
            }
        }
    })
}

fn op(
    name: &str,
    method: HttpMethod,
    path: &str,
    effect: OperationEffect,
    params: Value,
) -> ConnectorOperation {
    ConnectorOperation {
        name: name.to_owned(),
        description: format!("The {name} operation."),
        method,
        path: path.to_owned(),
        effect,
        params_schema: params,
        headers: Vec::new(),
        auth: vec![
            OperationAuth::Basic {
                username: "{credentials.username}".to_owned(),
                password: "{credentials.password}".to_owned(),
            },
            OperationAuth::Bearer {
                token: "{credentials.token}".to_owned(),
            },
        ],
        max_response_bytes: None,
    }
}

fn demo_manifest() -> ConnectorManifest {
    let check = op(
        "check-connection",
        HttpMethod::Get,
        "/api/now/table/sys_user?sysparm_limit=1",
        OperationEffect::ReadOnly,
        json!({"type": "object"}),
    );
    let get_record = op(
        "get-record",
        HttpMethod::Get,
        "/api/now/table/{table}/{sys_id}",
        OperationEffect::ReadOnly,
        json!({
            "type": "object",
            "required": ["table", "sys_id"],
            "properties": {"table": {"type": "string"}, "sys_id": {"type": "string"}}
        }),
    );
    let create = op(
        "create-incident",
        HttpMethod::Post,
        "/api/now/table/incident",
        OperationEffect::Compensatable,
        json!({
            "type": "object",
            "required": ["short_description"],
            "properties": {"short_description": {"type": "string"}}
        }),
    );
    ConnectorManifest::new(
        "servicenow",
        "1",
        "ServiceNow",
        "ServiceNow Table API operations.",
        "https://docs.servicenow.com/",
        "https://{instance}.service-now.com",
        demo_spec(),
        vec![get_record, create, check],
        "check-connection",
    )
    .expect("the demo manifest validates")
}

fn basic_config() -> Value {
    json!({
        "instance": "dev123",
        "credentials": {"auth": "basic", "username": "admin", "password": "s3cret"}
    })
}

// --------------------------------------------------------------------- //
// Declaration validation
// --------------------------------------------------------------------- //

#[test]
fn manifest_constructs_and_hashes_deterministically() {
    let manifest = demo_manifest();
    assert_eq!(manifest.hash.len(), 64);
    assert!(manifest.verify_hash());
    let again = demo_manifest();
    assert_eq!(manifest.hash, again.hash, "content addressing converges");
}

#[test]
fn manifest_rejects_http_base_url() {
    let mut manifest = demo_manifest();
    manifest.base_url = "http://{instance}.service-now.com".to_owned();
    let err = manifest.validate().unwrap_err().to_string();
    assert!(err.contains("https"), "unexpected error: {err}");
}

#[test]
fn manifest_rejects_http_documentation_url() {
    let mut manifest = demo_manifest();
    manifest.documentation_url = "http://example.com/docs".to_owned();
    let err = manifest.validate().unwrap_err().to_string();
    assert!(err.contains("https"), "unexpected error: {err}");
}

#[test]
fn manifest_rejects_undeclared_base_url_placeholder() {
    let mut manifest = demo_manifest();
    manifest.base_url = "https://{tenant_domain}.service-now.com".to_owned();
    let err = manifest.validate().unwrap_err().to_string();
    assert!(
        err.contains("{tenant_domain}") && err.contains("no declared schema property"),
        "unexpected error: {err}"
    );
}

#[test]
fn manifest_rejects_undeclared_auth_placeholder() {
    let mut manifest = demo_manifest();
    manifest.operations[0].auth = vec![OperationAuth::Bearer {
        token: "{credentials.api_key}".to_owned(),
    }];
    let err = manifest.validate().unwrap_err().to_string();
    assert!(
        err.contains("{credentials.api_key}"),
        "unexpected error: {err}"
    );
}

#[test]
fn manifest_rejects_undeclared_path_placeholder() {
    let mut manifest = demo_manifest();
    manifest.operations[0].path = "/api/now/table/{nope}".to_owned();
    let err = manifest.validate().unwrap_err().to_string();
    assert!(err.contains("{nope}"), "unexpected error: {err}");
}

#[test]
fn path_placeholders_may_name_operation_params() {
    // `{table}` and `{sys_id}` are not config properties; they are the
    // operation's declared params, and declaration accepts them.
    let manifest = demo_manifest();
    let get = manifest.operation("get-record").expect("declared");
    assert!(get.path.contains("{table}"));
}

#[test]
fn check_operation_must_exist() {
    let mut manifest = demo_manifest();
    manifest.check = "nope".to_owned();
    let err = manifest.validate().unwrap_err().to_string();
    assert!(err.contains("does not declare"), "unexpected error: {err}");
}

#[test]
fn check_operation_must_be_a_read_only_get() {
    let mut manifest = demo_manifest();
    manifest.check = "create-incident".to_owned();
    let err = manifest.validate().unwrap_err().to_string();
    assert!(err.contains("read-only GET"), "unexpected error: {err}");
}

#[test]
fn check_operation_must_be_parameterless() {
    let mut manifest = demo_manifest();
    manifest.check = "get-record".to_owned();
    let err = manifest.validate().unwrap_err().to_string();
    assert!(err.contains("parameterless"), "unexpected error: {err}");
}

#[test]
fn get_operations_must_declare_read_only_effect() {
    let mut manifest = demo_manifest();
    let get = manifest
        .operations
        .iter_mut()
        .find(|op| op.name == "get-record")
        .expect("declared");
    get.effect = OperationEffect::Compensatable;
    let err = manifest.validate().unwrap_err().to_string();
    assert!(err.contains("read_only"), "unexpected error: {err}");
}

// --------------------------------------------------------------------- //
// Placeholders
// --------------------------------------------------------------------- //

#[test]
fn scan_placeholders_finds_dot_paths() {
    let fields = scan_placeholders("https://{instance}.example.com/{credentials.token}").unwrap();
    assert_eq!(fields, vec!["instance", "credentials.token"]);
}

#[test]
fn scan_placeholders_rejects_unbalanced_and_illegal() {
    assert!(scan_placeholders("{instance").is_err());
    assert!(scan_placeholders("{}").is_err());
    assert!(scan_placeholders("{a b}").is_err());
}

#[test]
fn render_template_substitutes_nested_scalars() {
    let config = basic_config();
    let url = render_template("https://{instance}.service-now.com", &config).unwrap();
    assert_eq!(url, "https://dev123.service-now.com");
    let who = render_template("{credentials.username}", &config).unwrap();
    assert_eq!(who, "admin");
}

#[test]
fn render_template_fails_closed_on_absent_or_structured() {
    let config = basic_config();
    let err = render_template("{credentials.token}", &config)
        .unwrap_err()
        .to_string();
    assert!(err.contains("{credentials.token}"), "unexpected: {err}");
    let err = render_template("{credentials}", &config)
        .unwrap_err()
        .to_string();
    assert!(err.contains("structured"), "unexpected: {err}");
}

// --------------------------------------------------------------------- //
// Schema validation — the 422 field-path contract
// --------------------------------------------------------------------- //

#[test]
fn valid_configs_pass_both_variants() {
    validate_config(&demo_spec(), &basic_config()).expect("basic validates");
    let oauth = json!({
        "instance": "dev123",
        "credentials": {"auth": "oauth", "token": "tok"}
    });
    validate_config(&demo_spec(), &oauth).expect("oauth validates");
}

#[test]
fn missing_required_names_the_dotted_path() {
    let config = json!({
        "instance": "dev123",
        "credentials": {"auth": "basic", "password": "s3cret"}
    });
    let err = validate_config(&demo_spec(), &config).unwrap_err();
    assert_eq!(err, "credentials.username: required property missing");
}

#[test]
fn unknown_property_names_the_field() {
    let config = json!({
        "instance": "dev123",
        "credentials": {"auth": "basic", "username": "a", "password": "b"},
        "region": "us-east"
    });
    let err = validate_config(&demo_spec(), &config).unwrap_err();
    assert_eq!(err, "region: unknown property");
}

#[test]
fn wrong_type_names_the_field() {
    let config = json!({
        "instance": 42,
        "credentials": {"auth": "basic", "username": "a", "password": "b"}
    });
    let err = validate_config(&demo_spec(), &config).unwrap_err();
    assert!(
        err.starts_with("instance: ") && err.contains("type"),
        "unexpected: {err}"
    );
}

#[test]
fn pattern_violation_names_the_field() {
    let config = json!({
        "instance": "Dev_123!",
        "credentials": {"auth": "basic", "username": "a", "password": "b"}
    });
    let err = validate_config(&demo_spec(), &config).unwrap_err();
    assert!(err.starts_with("instance: "), "unexpected: {err}");
}

#[test]
fn ambiguous_credentials_fail_both_variants() {
    // No discriminator const — matches neither oneOf branch.
    let config = json!({
        "instance": "dev123",
        "credentials": {"username": "a", "password": "b"}
    });
    let err = validate_config(&demo_spec(), &config).unwrap_err();
    assert!(err.starts_with("credentials: "), "unexpected: {err}");
}

// --------------------------------------------------------------------- //
// Secret extraction
// --------------------------------------------------------------------- //

#[test]
fn secret_extraction_walks_the_matching_variant() {
    let spec = demo_spec();
    let extracted = extract_secrets(&spec, &basic_config());
    let paths: Vec<&str> = extracted.iter().map(|(path, _)| path.as_str()).collect();
    assert_eq!(paths, vec!["credentials.password", "credentials.username"]);

    let oauth = json!({
        "instance": "dev123",
        "credentials": {"auth": "oauth", "token": "tok"}
    });
    let extracted = extract_secrets(&spec, &oauth);
    let paths: Vec<&str> = extracted.iter().map(|(path, _)| path.as_str()).collect();
    assert_eq!(paths, vec!["credentials.token"]);
}

#[test]
fn without_secrets_strips_and_reinsertion_restores() {
    let spec = demo_spec();
    let config = basic_config();
    let extracted = extract_secrets(&spec, &config);
    let stripped = without_secrets(config.clone(), &extracted);
    // `credentials` keeps only the discriminator; no secret survives.
    assert_eq!(stripped["credentials"], json!({"auth": "basic"}));
    let stripped_bytes = serde_json::to_string(&stripped).unwrap();
    assert!(!stripped_bytes.contains("s3cret"));
    assert!(!stripped_bytes.contains("admin"));

    let restored = insert_opened_secrets(stripped, &extracted);
    assert_eq!(restored, config);
}

#[test]
fn masked_serving_marks_secrets_without_values() {
    use rusty_agent_runtime::broker::SealedCredential;
    let mut sealed = BTreeMap::new();
    let envelope = |path: &str| SealedCredential {
        format_version: 1,
        key_id: "bmk-test".to_owned(),
        wrapped_data_key: "00".to_owned(),
        wrap_nonce: "00".to_owned(),
        nonce: "00".to_owned(),
        ciphertext: format!("ciphertext-of-{path}"),
        sealed_at: Utc::now(),
    };
    sealed.insert(
        "credentials.username".to_owned(),
        envelope("credentials.username"),
    );
    let served = insert_masked_secrets(json!({"instance": "dev123"}), &sealed);
    assert_eq!(
        served,
        json!({"instance": "dev123", "credentials": {"username": {"rusty_secret": true}}})
    );
    assert!(!serde_json::to_string(&served)
        .unwrap()
        .contains("ciphertext-of"));
}

// --------------------------------------------------------------------- //
// Check execution
// --------------------------------------------------------------------- //

/// A scripted transport: records the request, answers the queued status.
#[derive(Debug, Default)]
struct FakeTransport {
    seen: Mutex<Option<CheckRequest>>,
    status: u16,
    body: Vec<u8>,
    fail: Option<String>,
}

#[async_trait]
impl ConnectorTransport for FakeTransport {
    async fn send(&self, request: CheckRequest) -> RuntimeResult<CheckResponse> {
        *self.seen.lock().unwrap() = Some(request.clone());
        if let Some(fail) = &self.fail {
            return Err(rusty_agent_runtime::error::RustyError::Tool(fail.clone()));
        }
        Ok(CheckResponse {
            status: self.status,
            body: self.body.clone(),
        })
    }
}

#[tokio::test]
async fn check_success_on_2xx() {
    let transport = FakeTransport {
        status: 200,
        ..FakeTransport::default()
    };
    let outcome = execute_check(&demo_manifest(), &basic_config(), &transport).await;
    assert_eq!(
        outcome,
        CheckOutcome {
            status: CheckStatus::Succeeded,
            message: None
        }
    );
    let seen = transport.seen.lock().unwrap().clone().expect("sent");
    assert_eq!(
        seen.url,
        "https://dev123.service-now.com/api/now/table/sys_user?sysparm_limit=1"
    );
    assert_eq!(seen.method.as_str(), "GET");
    // The basic alternative rendered: base64("admin:s3cret").
    assert!(seen
        .headers
        .iter()
        .any(|(name, value)| name == "Authorization" && value.starts_with("Basic ")));
}

#[tokio::test]
async fn check_renders_bearer_for_the_oauth_variant() {
    let transport = FakeTransport {
        status: 200,
        ..FakeTransport::default()
    };
    let config = json!({
        "instance": "dev123",
        "credentials": {"auth": "oauth", "token": "tok-1"}
    });
    let outcome = execute_check(&demo_manifest(), &config, &transport).await;
    assert_eq!(outcome.status, CheckStatus::Succeeded);
    let seen = transport.seen.lock().unwrap().clone().expect("sent");
    assert!(seen
        .headers
        .iter()
        .any(|(name, value)| name == "Authorization" && value == "Bearer tok-1"));
}

#[tokio::test]
async fn check_failure_on_non_2xx_carries_status_and_excerpt() {
    let transport = FakeTransport {
        status: 500,
        body: b"upstream exploded".to_vec(),
        ..FakeTransport::default()
    };
    let outcome = execute_check(&demo_manifest(), &basic_config(), &transport).await;
    assert_eq!(outcome.status, CheckStatus::Failed);
    assert_eq!(
        outcome.message.as_deref(),
        Some("HTTP 500: upstream exploded")
    );
}

#[tokio::test]
async fn check_failure_on_auth_refusal_echoes_no_body() {
    let transport = FakeTransport {
        status: 401,
        body: b"wrong password s3cret neighborhood".to_vec(),
        ..FakeTransport::default()
    };
    let outcome = execute_check(&demo_manifest(), &basic_config(), &transport).await;
    assert_eq!(outcome.status, CheckStatus::Failed);
    let message = outcome.message.expect("a failure message");
    assert!(message.contains("401"), "unexpected: {message}");
    assert!(!message.contains("neighborhood"), "body leaked: {message}");
}

#[tokio::test]
async fn check_failure_on_transport_error() {
    let transport = FakeTransport {
        fail: Some("dns: no such host".to_owned()),
        ..FakeTransport::default()
    };
    let outcome = execute_check(&demo_manifest(), &basic_config(), &transport).await;
    assert_eq!(outcome.status, CheckStatus::Failed);
    assert!(
        outcome.message.unwrap().contains("dns: no such host"),
        "transport error surfaces in the message"
    );
}

#[tokio::test]
async fn check_never_raises_on_render_failure() {
    // A config whose variant matches no auth alternative: the manifest
    // declares basic + bearer, this config carries neither field.
    let config = json!({
        "instance": "dev123",
        "credentials": {"auth": "oauth", "token": "tok"}
    });
    validate_config(&demo_spec(), &config).expect("valid");
    // Drop the token after validation to force a render failure.
    let config = json!({
        "instance": "dev123",
        "credentials": {"auth": "oauth"}
    });
    let outcome = execute_check(&demo_manifest(), &config, &FakeTransport::default()).await;
    assert_eq!(outcome.status, CheckStatus::Failed);
    assert!(outcome.message.is_some());
}

#[test]
fn rendered_request_rejects_newline_in_header_values() {
    let mut manifest = demo_manifest();
    manifest.operations[0].headers = vec![("x-trace".to_owned(), "{instance}".to_owned())];
    let op = manifest.operations[0].clone();
    let config = json!({
        "instance": "dev123\r\nx-injected: yes",
        "credentials": {"auth": "basic", "username": "a", "password": "b"}
    });
    let err = render_operation_request(&manifest, &op, &config).unwrap_err();
    assert!(err.to_string().contains("newline"), "unexpected: {err}");
}

// --------------------------------------------------------------------- //
// Catalog derivation and the instance record
// --------------------------------------------------------------------- //

#[test]
fn catalog_derives_one_tool_per_operation() {
    let manifest = demo_manifest();
    let catalog = manifest.derive_catalog().expect("catalog derives");
    let names: Vec<&str> = catalog.iter().map(|cap| cap.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "servicenow/check-connection",
            "servicenow/create-incident",
            "servicenow/get-record"
        ]
    );
    let get = catalog
        .iter()
        .find(|cap| cap.name == "servicenow/get-record")
        .expect("derived");
    assert_eq!(get.effect, rusty_agent_runtime::record::Effect::ReadOnly);
    assert!(get.parameters_schema["properties"]["table"].is_object());
    let create = catalog
        .iter()
        .find(|cap| cap.name == "servicenow/create-incident")
        .expect("derived");
    assert_eq!(
        create.effect,
        rusty_agent_runtime::record::Effect::Compensatable
    );
}

#[test]
fn instance_record_validates_shape() {
    let record = ConnectorInstance::new(
        "inst-0123456789abcdef",
        &demo_manifest().hash,
        json!({"instance": "dev123"}),
        BTreeMap::new(),
        Utc::now(),
    )
    .expect("constructs");
    assert_eq!(record.manifest_hash.len(), 64);

    assert!(ConnectorInstance::new(
        "bad id!",
        &demo_manifest().hash,
        json!({}),
        BTreeMap::new(),
        Utc::now()
    )
    .is_err());
    assert!(ConnectorInstance::new(
        "inst-1",
        "not-a-hash",
        json!({}),
        BTreeMap::new(),
        Utc::now()
    )
    .is_err());
}

// --------------------------------------------------------------------- //
// Auth variant validation and rendering
// --------------------------------------------------------------------- //

#[test]
fn header_auth_validates_and_renders() {
    let spec = json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "required": ["api_key"],
        "properties": {
            "api_key": {"type": "string", "rusty_secret": true}
        }
    });
    let op = ConnectorOperation {
        name: "test".to_owned(),
        description: "test".to_owned(),
        method: HttpMethod::Get,
        path: "/test".to_owned(),
        effect: OperationEffect::ReadOnly,
        params_schema: json!({"type": "object"}),
        headers: Vec::new(),
        auth: vec![OperationAuth::Header {
            name: "X-API-Key".to_owned(),
            value_template: "{api_key}".to_owned(),
        }],
        max_response_bytes: None,
    };
    let manifest = ConnectorManifest::new(
        "test-conn",
        "1",
        "Test",
        "Test",
        "https://docs.example.com",
        "https://api.example.com",
        spec,
        vec![op],
        "test",
    )
    .expect("manifest validates");

    let config = json!({"api_key": "secret-123"});
    let req = render_operation_request(&manifest, &manifest.operations[0], &config).unwrap();
    assert!(req
        .headers
        .iter()
        .any(|(k, v)| k == "X-API-Key" && v == "secret-123"));
}

#[test]
fn query_auth_appends_to_url() {
    let spec = json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "required": ["token"],
        "properties": {
            "token": {"type": "string", "rusty_secret": true}
        }
    });
    let op = ConnectorOperation {
        name: "test".to_owned(),
        description: "test".to_owned(),
        method: HttpMethod::Get,
        path: "/test".to_owned(),
        effect: OperationEffect::ReadOnly,
        params_schema: json!({"type": "object"}),
        headers: Vec::new(),
        auth: vec![OperationAuth::Query {
            name: "token".to_owned(),
            value_template: "{token}".to_owned(),
        }],
        max_response_bytes: None,
    };
    let manifest = ConnectorManifest::new(
        "test-conn",
        "1",
        "Test",
        "Test",
        "https://docs.example.com",
        "https://api.example.com",
        spec,
        vec![op],
        "test",
    )
    .expect("manifest validates");

    let config = json!({"token": "tok-1"});
    let req = render_operation_request(&manifest, &manifest.operations[0], &config).unwrap();
    assert!(req.url.contains("?token=tok-1"), "url was: {}", req.url);
}

#[test]
fn oauth2_client_credentials_declares_and_validates() {
    let spec = json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "required": ["client_id", "client_secret", "token_url"],
        "properties": {
            "client_id": {"type": "string"},
            "client_secret": {"type": "string", "rusty_secret": true},
            "token_url": {"type": "string"},
            "scope": {"type": "string"}
        }
    });
    let op = ConnectorOperation {
        name: "test".to_owned(),
        description: "test".to_owned(),
        method: HttpMethod::Get,
        path: "/test".to_owned(),
        effect: OperationEffect::ReadOnly,
        params_schema: json!({"type": "object"}),
        headers: Vec::new(),
        auth: vec![OperationAuth::OAuth2ClientCredentials {
            token_url: "{token_url}".to_owned(),
            client_id_template: "{client_id}".to_owned(),
            client_secret_template: "{client_secret}".to_owned(),
            scope_template: Some("{scope}".to_owned()),
        }],
        max_response_bytes: None,
    };
    let manifest = ConnectorManifest::new(
        "test-conn",
        "1",
        "Test",
        "Test",
        "https://docs.example.com",
        "https://api.example.com",
        spec,
        vec![op],
        "test",
    )
    .expect("manifest validates");

    // Validation should succeed because all placeholders name declared schema properties.
    assert!(manifest.verify_hash());
}

#[test]
fn oauth2_missing_placeholder_fails_validation() {
    let spec = json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "required": ["client_id"],
        "properties": {
            "client_id": {"type": "string"}
        }
    });
    let op = ConnectorOperation {
        name: "test".to_owned(),
        description: "test".to_owned(),
        method: HttpMethod::Get,
        path: "/test".to_owned(),
        effect: OperationEffect::ReadOnly,
        params_schema: json!({"type": "object"}),
        headers: Vec::new(),
        auth: vec![OperationAuth::OAuth2ClientCredentials {
            token_url: "{token_url}".to_owned(),
            client_id_template: "{client_id}".to_owned(),
            client_secret_template: "{client_secret}".to_owned(),
            scope_template: None,
        }],
        max_response_bytes: None,
    };
    let err = ConnectorManifest::new(
        "test-conn",
        "1",
        "Test",
        "Test",
        "https://docs.example.com",
        "https://api.example.com",
        spec,
        vec![op],
        "test",
    )
    .unwrap_err();
    assert!(err.to_string().contains("{token_url}") || err.to_string().contains("{client_secret}"));
}
