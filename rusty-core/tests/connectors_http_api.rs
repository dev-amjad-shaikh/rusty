//! HTTP API connector tests: the `http-api` provider kind end to end —
//! manifest validation, catalog derivation, auth styles against a fake
//! transport (with zero secret leakage), path/GraphQL templating,
//! idempotency-key determinism, byte ceilings, error mapping, and
//! lifecycle integration with the existing registry.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use rusty_agent_runtime::connector::manifest::MAX_HTTP_API_RESPONSE_BYTES;
use rusty_agent_runtime::connector::{
    derive_idempotency_key, ConnectorManifest, ConnectorProvider, ConnectorRegistry,
    CredentialHandle, CredentialSlot, HttpApiAuth, HttpApiOperation, HttpApiProvider,
    HttpApiRequest, HttpApiTool, HttpApiTransport, HttpMethod, HttpResponse,
    InMemoryCredentialBroker, LifecycleState, OperationBody, OperationEffect, ProviderKind,
    ResponseExtraction, MAX_HTTP_API_ERROR_BODY_BYTES, MAX_HTTP_API_REQUEST_BYTES,
};
use rusty_agent_runtime::error::Result;
use rusty_agent_runtime::record::Effect;
use rusty_agent_runtime::tool::Tool;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const BASE_URL: &str = "https://api.example.com";
const SECRET: &str = "sekrit-token";

fn slot(name: &str) -> CredentialSlot {
    CredentialSlot {
        name: name.to_owned(),
        description: format!("The {name} credential."),
    }
}

fn bearer_auth() -> HttpApiAuth {
    HttpApiAuth::BearerToken {
        credential_slot: "api_token".to_owned(),
    }
}

/// A bare operation the fixtures customize: no params, no body, no
/// overrides. Callers fill in the fields under test.
fn bare_op(name: &str, method: HttpMethod, path: &str, effect: OperationEffect) -> HttpApiOperation {
    HttpApiOperation {
        name: name.to_owned(),
        description: format!("The {name} operation."),
        method,
        path: path.to_owned(),
        params_schema: json!({"type": "object"}),
        query_params: vec![],
        body: OperationBody::None,
        effect,
        response: ResponseExtraction {
            projection: None,
            max_bytes: None,
        },
        timeout_ms: None,
        idempotency_key_header: None,
    }
}

/// GET `/v1/issues?team=…&limit=…`.
fn list_issues() -> HttpApiOperation {
    let mut op = bare_op("list-issues", HttpMethod::Get, "/v1/issues", OperationEffect::ReadOnly);
    op.params_schema = json!({
        "type": "object",
        "properties": {
            "team": {"type": "string"},
            "limit": {"type": "integer"},
        },
        "required": ["team"],
    });
    op.query_params = vec!["team".to_owned(), "limit".to_owned()];
    op
}

/// GET `/v1/issues/{issue_id}`.
fn get_issue() -> HttpApiOperation {
    let mut op = bare_op(
        "get-issue",
        HttpMethod::Get,
        "/v1/issues/{issue_id}",
        OperationEffect::ReadOnly,
    );
    op.params_schema = json!({
        "type": "object",
        "properties": {"issue_id": {"type": "string"}},
        "required": ["issue_id"],
    });
    op
}

/// Keyed POST `/v1/teams/{team_id}/issues` with a JSON body.
fn create_issue() -> HttpApiOperation {
    let mut op = bare_op(
        "create-issue",
        HttpMethod::Post,
        "/v1/teams/{team_id}/issues",
        OperationEffect::Idempotent,
    );
    op.params_schema = json!({
        "type": "object",
        "properties": {
            "team_id": {"type": "string"},
            "title": {"type": "string"},
            "priority": {"type": "integer"},
        },
        "required": ["team_id", "title"],
    });
    op.body = OperationBody::Json {
        params: vec!["title".to_owned(), "priority".to_owned()],
    };
    op.idempotency_key_header = Some("idempotency-key".to_owned());
    op
}

/// POST `/v1/issues/{issue_id}/comments`, compensatable (no key support).
fn comment_issue() -> HttpApiOperation {
    let mut op = bare_op(
        "comment-issue",
        HttpMethod::Post,
        "/v1/issues/{issue_id}/comments",
        OperationEffect::Compensatable,
    );
    op.params_schema = json!({
        "type": "object",
        "properties": {
            "issue_id": {"type": "string"},
            "text": {"type": "string"},
        },
        "required": ["issue_id", "text"],
    });
    op.body = OperationBody::Json {
        params: vec!["text".to_owned()],
    };
    op
}

/// DELETE `/v1/issues/{issue_id}`.
fn delete_issue() -> HttpApiOperation {
    let mut op = bare_op(
        "delete-issue",
        HttpMethod::Delete,
        "/v1/issues/{issue_id}",
        OperationEffect::Irreversible,
    );
    op.params_schema = json!({
        "type": "object",
        "properties": {"issue_id": {"type": "string"}},
        "required": ["issue_id"],
    });
    op
}

/// POST `/graphql` with an interpolated mutation template, Linear-style.
fn graphql_create() -> HttpApiOperation {
    let mut op = bare_op("graphql-create", HttpMethod::Post, "/graphql", OperationEffect::Compensatable);
    op.params_schema = json!({
        "type": "object",
        "properties": {
            "title": {"type": "string"},
            "priority": {"type": "integer"},
        },
        "required": ["title"],
    });
    op.body = OperationBody::Graphql {
        query: "mutation {{ issueCreate(input: {{ title: {title}, priority: {priority} }}) {{ success }} }}"
            .to_owned(),
    };
    op
}

/// GET `/v1/ping` — a valid health check (parameterless read-only GET).
fn ping() -> HttpApiOperation {
    bare_op("ping", HttpMethod::Get, "/v1/ping", OperationEffect::ReadOnly)
}

/// The standard operation set.
fn all_ops() -> Vec<HttpApiOperation> {
    vec![
        list_issues(),
        get_issue(),
        create_issue(),
        comment_issue(),
        delete_issue(),
        graphql_create(),
        ping(),
    ]
}

fn http_api_manifest(
    id: &str,
    auth: Option<HttpApiAuth>,
    default_headers: Vec<(String, String)>,
    health_check: Option<&str>,
    operations: Vec<HttpApiOperation>,
    slots: Vec<&str>,
) -> Result<ConnectorManifest> {
    ConnectorManifest::new(
        id,
        "1.0.0",
        "Test API",
        "A test HTTP API connector.",
        ProviderKind::HttpApi(rusty_agent_runtime::connector::HttpApiSpec {
            base_url: BASE_URL.to_owned(),
            auth,
            default_headers,
            health_check: health_check.map(str::to_owned),
            operations,
        }),
        vec!["rest api".to_owned()],
        slots.into_iter().map(slot).collect(),
    )
}

/// The standard manifest: bearer auth, all operations, `ping` health check.
fn api_manifest(id: &str) -> ConnectorManifest {
    http_api_manifest(
        id,
        Some(bearer_auth()),
        vec![("x-client".to_owned(), "rusty-test".to_owned())],
        Some("ping"),
        all_ops(),
        vec!["api_token"],
    )
    .expect("valid http-api manifest")
}

/// The error message of a manifest that must fail construction.
fn manifest_error(result: Result<ConnectorManifest>) -> String {
    result.expect_err("manifest must be rejected").to_string()
}

/// A scripted arbitrary-method transport: replies are queued, requests are
/// captured, and an artificial latency drives the timeout test.
#[derive(Debug, Default)]
struct FakeApiTransport {
    replies: Mutex<VecDeque<HttpResponse>>,
    captured: Mutex<Vec<HttpApiRequest>>,
    latency: Mutex<Duration>,
}

impl FakeApiTransport {
    fn push_json(&self, status: u16, body: Value) {
        self.push(HttpResponse {
            status,
            body: serde_json::to_vec(&body).expect("encode reply"),
        });
    }

    fn push(&self, reply: HttpResponse) {
        self.replies.lock().expect("replies lock").push_back(reply);
    }

    fn set_latency(&self, latency: Duration) {
        *self.latency.lock().expect("latency lock") = latency;
    }

    fn captured(&self) -> Vec<HttpApiRequest> {
        self.captured.lock().expect("captured lock").clone()
    }
}

#[async_trait]
impl HttpApiTransport for FakeApiTransport {
    async fn send(&self, request: HttpApiRequest) -> Result<HttpResponse> {
        let latency = *self.latency.lock().expect("latency lock");
        if !latency.is_zero() {
            tokio::time::sleep(latency).await;
        }
        self.captured
            .lock()
            .expect("captured lock")
            .push(request);
        self.replies
            .lock()
            .expect("replies lock")
            .pop_front()
            .ok_or_else(|| {
                rusty_agent_runtime::error::RustyError::Tool(
                    "fake transport: no scripted reply".to_owned(),
                )
            })
    }
}

fn handle(slot_name: &str, secret: &str) -> CredentialHandle {
    CredentialHandle::new("acme", slot_name, secret).expect("valid handle")
}

fn bearer_credentials() -> Vec<CredentialHandle> {
    vec![handle("api_token", SECRET)]
}

/// The provider over the standard manifest, plus its fake transport.
async fn provider_and_transport() -> (HttpApiProvider, Arc<FakeApiTransport>) {
    let manifest = api_manifest("tickets");
    let provider = HttpApiProvider::from_manifest(&manifest).expect("provider from manifest");
    (provider, Arc::new(FakeApiTransport::default()))
}

fn header<'a>(request: &'a HttpApiRequest, name: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

// ---------------------------------------------------------------------------
// Manifest validation matrix
// ---------------------------------------------------------------------------

#[test]
fn valid_http_api_manifest_constructs_verifies_and_round_trips() {
    let manifest = api_manifest("tickets");
    assert!(manifest.verify_hash());
    assert_eq!(manifest.hash.len(), 64);

    // Serde round-trip preserves the hash (the content is the identity).
    let bytes = serde_json::to_string(&manifest).expect("serialize");
    assert!(bytes.contains("\"kind\":\"http_api\""));
    let back: ConnectorManifest = serde_json::from_str(&bytes).expect("deserialize");
    assert!(back.verify_hash());
    assert_eq!(back.hash, manifest.hash);
}

#[test]
fn http_api_rejects_bad_base_urls() {
    for url in [
        "http://api.example.com",       // plaintext transport
        "https://",                     // no host
        "https://api.example.com?q=1",  // query material belongs to operations
        "https://api.example.com#frag", // no fragments
        "https://api.example.com/x y",  // whitespace
    ] {
        let mut manifest = api_manifest("tickets");
        let ProviderKind::HttpApi(spec) = &mut manifest.provider else {
            unreachable!()
        };
        spec.base_url = url.to_owned();
        let error = ConnectorManifest::new(
            &manifest.id,
            &manifest.version,
            &manifest.display_name,
            &manifest.description,
            manifest.provider.clone(),
            manifest.capabilities.clone(),
            manifest.credential_slots.clone(),
        )
        .expect_err("bad base URL must be rejected")
        .to_string();
        assert!(error.contains("base URL"), "{url}: {error}");
    }
}

#[test]
fn http_api_rejects_bad_auth_declarations() {
    // Every style must reference declared slots.
    let cases: Vec<HttpApiAuth> = vec![
        HttpApiAuth::BearerToken {
            credential_slot: "ghost".to_owned(),
        },
        HttpApiAuth::Header {
            header: "x-api-key".to_owned(),
            credential_slot: "ghost".to_owned(),
        },
        HttpApiAuth::QueryParam {
            param: "apikey".to_owned(),
            credential_slot: "ghost".to_owned(),
        },
        HttpApiAuth::Basic {
            username_slot: "ghost".to_owned(),
            password_slot: "also_ghost".to_owned(),
        },
    ];
    for auth in cases {
        let error = manifest_error(http_api_manifest(
            "tickets",
            Some(auth),
            vec![],
            None,
            all_ops(),
            vec!["api_token"],
        ));
        assert!(error.contains("undeclared credential slot"), "{error}");
    }

    // Basic auth must reference two distinct slots.
    let error = manifest_error(http_api_manifest(
        "tickets",
        Some(HttpApiAuth::Basic {
            username_slot: "api_token".to_owned(),
            password_slot: "api_token".to_owned(),
        }),
        vec![],
        None,
        all_ops(),
        vec!["api_token"],
    ));
    assert!(error.contains("distinct"), "{error}");

    // Header/query-param names are validated.
    let error = manifest_error(http_api_manifest(
        "tickets",
        Some(HttpApiAuth::Header {
            header: "bad header".to_owned(),
            credential_slot: "api_token".to_owned(),
        }),
        vec![],
        None,
        all_ops(),
        vec!["api_token"],
    ));
    assert!(error.contains("not a valid HTTP token"), "{error}");
    let error = manifest_error(http_api_manifest(
        "tickets",
        Some(HttpApiAuth::QueryParam {
            param: "bad param".to_owned(),
            credential_slot: "api_token".to_owned(),
        }),
        vec![],
        None,
        all_ops(),
        vec!["api_token"],
    ));
    assert!(error.contains("URL-unreserved"), "{error}");

    // A default header must not shadow the auth header.
    let error = manifest_error(http_api_manifest(
        "tickets",
        Some(HttpApiAuth::Header {
            header: "X-Api-Key".to_owned(),
            credential_slot: "api_token".to_owned(),
        }),
        vec![("x-api-key".to_owned(), "static".to_owned())],
        None,
        all_ops(),
        vec!["api_token"],
    ));
    assert!(error.contains("collide"), "{error}");
}

#[test]
fn http_api_rejects_bad_default_headers() {
    // Reserved names: auth owns authorization, the provider owns content-type.
    for reserved in ["Authorization", "CONTENT-TYPE"] {
        let error = manifest_error(http_api_manifest(
            "tickets",
            Some(bearer_auth()),
            vec![(reserved.to_owned(), "x".to_owned())],
            None,
            all_ops(),
            vec!["api_token"],
        ));
        assert!(error.contains("reserved"), "{reserved}: {error}");
    }
    // Duplicates are case-insensitive on the wire.
    let error = manifest_error(http_api_manifest(
        "tickets",
        Some(bearer_auth()),
        vec![
            ("X-Trace".to_owned(), "a".to_owned()),
            ("x-trace".to_owned(), "b".to_owned()),
        ],
        None,
        all_ops(),
        vec!["api_token"],
    ));
    assert!(error.contains("twice"), "{error}");
    // Over-cap count.
    let headers = (0..17)
        .map(|i| (format!("x-h{i}"), "v".to_owned()))
        .collect::<Vec<_>>();
    let error = manifest_error(http_api_manifest(
        "tickets",
        Some(bearer_auth()),
        headers,
        None,
        all_ops(),
        vec!["api_token"],
    ));
    assert!(error.contains("default headers"), "{error}");
}

#[test]
fn http_api_rejects_bad_operation_shapes() {
    // Zero operations is not a connector.
    let error = manifest_error(http_api_manifest(
        "tickets",
        Some(bearer_auth()),
        vec![],
        None,
        vec![],
        vec!["api_token"],
    ));
    assert!(error.contains("operations"), "{error}");

    // Duplicate operation names.
    let error = manifest_error(http_api_manifest(
        "tickets",
        Some(bearer_auth()),
        vec![],
        None,
        vec![list_issues(), list_issues()],
        vec!["api_token"],
    ));
    assert!(error.contains("twice"), "{error}");

    // Non-kebab operation names.
    let mut op = list_issues();
    op.name = "List_Issues".to_owned();
    let error = manifest_error(http_api_manifest(
        "tickets",
        Some(bearer_auth()),
        vec![],
        None,
        vec![op],
        vec!["api_token"],
    ));
    assert!(error.contains("kebab-case"), "{error}");

    // The derived catalog name must fit the tool-name cap.
    let long_id = "a".repeat(64);
    let mut op = list_issues();
    op.name = "b".repeat(64);
    let error = manifest_error(http_api_manifest(
        &long_id,
        Some(bearer_auth()),
        vec![],
        None,
        vec![op],
        vec!["api_token"],
    ));
    assert!(error.contains("catalog name"), "{error}");

    // Oversized description.
    let mut op = list_issues();
    op.description = "x".repeat(1025);
    let error = manifest_error(http_api_manifest(
        "tickets",
        Some(bearer_auth()),
        vec![],
        None,
        vec![op],
        vec!["api_token"],
    ));
    assert!(error.contains("operation description"), "{error}");
}

#[test]
fn http_api_rejects_bad_paths_and_templates() {
    let cases: Vec<&str> = vec![
        "v1/issues",          // must start with `/`
        "/v1/issues?x=1",     // no query material in the path
        "/v1/issues/{issue",  // unclosed placeholder
        "/v1/issues}/x",      // stray closing brace
        "/v1/issues/{Issue}", // invalid parameter name
        "/v1/is sues",        // whitespace
    ];
    for path in cases {
        let mut op = get_issue();
        op.path = path.to_owned();
        // Keep the schema consistent so the failure is the path itself.
        if path == "/v1/issues/{Issue}" {
            op.params_schema = json!({"type": "object"});
        }
        let error = manifest_error(http_api_manifest(
            "tickets",
            Some(bearer_auth()),
            vec![],
            None,
            vec![op],
            vec!["api_token"],
        ));
        assert!(
            error.contains("path") || error.contains("template") || error.contains("parameter name"),
            "{path}: {error}"
        );
    }
}

#[test]
fn http_api_rejects_schema_and_routing_mismatches() {
    // Schema must be an object.
    let mut op = list_issues();
    op.params_schema = json!(["not", "an", "object"]);
    let error = manifest_error(http_api_manifest(
        "tickets",
        Some(bearer_auth()),
        vec![],
        None,
        vec![op.clone()],
        vec!["api_token"],
    ));
    assert!(error.contains("JSON object"), "{error}");

    // A placeholder the schema does not declare.
    let mut op = get_issue();
    op.params_schema = json!({"type": "object", "properties": {"other": {"type": "string"}}});
    op.query_params = vec!["other".to_owned()];
    let error = manifest_error(http_api_manifest(
        "tickets",
        Some(bearer_auth()),
        vec![],
        None,
        vec![op],
        vec!["api_token"],
    ));
    assert!(error.contains("does not declare"), "{error}");

    // A schema property routed nowhere.
    let mut op = list_issues();
    op.params_schema = json!({
        "type": "object",
        "properties": {
            "team": {"type": "string"},
            "limit": {"type": "integer"},
            "orphan": {"type": "string"},
        },
    });
    let error = manifest_error(http_api_manifest(
        "tickets",
        Some(bearer_auth()),
        vec![],
        None,
        vec![op],
        vec!["api_token"],
    ));
    assert!(error.contains("routed nowhere"), "{error}");

    // One parameter routed to two locations.
    let mut op = create_issue();
    op.query_params = vec!["title".to_owned()];
    let error = manifest_error(http_api_manifest(
        "tickets",
        Some(bearer_auth()),
        vec![],
        None,
        vec![op],
        vec!["api_token"],
    ));
    assert!(error.contains("to both"), "{error}");

    // `required` must name declared properties.
    let mut op = list_issues();
    op.params_schema = json!({
        "type": "object",
        "properties": {"team": {"type": "string"}, "limit": {"type": "integer"}},
        "required": ["ghost"],
    });
    let error = manifest_error(http_api_manifest(
        "tickets",
        Some(bearer_auth()),
        vec![],
        None,
        vec![op],
        vec!["api_token"],
    ));
    assert!(error.contains("requires undeclared property"), "{error}");

    // Oversized schema.
    let mut op = list_issues();
    let padding = "x".repeat(17 * 1024);
    op.params_schema = json!({
        "type": "object",
        "properties": {
            "team": {"type": "string", "description": padding},
            "limit": {"type": "integer"},
        },
    });
    let error = manifest_error(http_api_manifest(
        "tickets",
        Some(bearer_auth()),
        vec![],
        None,
        vec![op],
        vec!["api_token"],
    ));
    assert!(error.contains("exceeds"), "{error}");
}

#[test]
fn http_api_rejects_body_method_mismatches() {
    // No body on GET.
    let mut op = list_issues();
    op.params_schema = json!({
        "type": "object",
        "properties": {"team": {"type": "string"}},
    });
    op.query_params = vec![];
    op.body = OperationBody::Json {
        params: vec!["team".to_owned()],
    };
    let error = manifest_error(http_api_manifest(
        "tickets",
        Some(bearer_auth()),
        vec![],
        None,
        vec![op],
        vec!["api_token"],
    ));
    assert!(error.contains("cannot carry a request body"), "{error}");

    // GraphQL is POST-only.
    let mut op = graphql_create();
    op.method = HttpMethod::Put;
    let error = manifest_error(http_api_manifest(
        "tickets",
        Some(bearer_auth()),
        vec![],
        None,
        vec![op],
        vec!["api_token"],
    ));
    assert!(error.contains("graphql bodies require POST"), "{error}");

    // An empty json body routing is not a body.
    let mut op = create_issue();
    op.body = OperationBody::Json { params: vec![] };
    let error = manifest_error(http_api_manifest(
        "tickets",
        Some(bearer_auth()),
        vec![],
        None,
        vec![op],
        vec!["api_token"],
    ));
    assert!(error.contains("at least one parameter"), "{error}");

    // Oversized GraphQL template.
    let mut op = graphql_create();
    op.body = OperationBody::Graphql {
        query: format!("query {{{{ node(id: {{title}}) {{{{ id }}) }}}} {}", "x".repeat(9 * 1024)),
    };
    let error = manifest_error(http_api_manifest(
        "tickets",
        Some(bearer_auth()),
        vec![],
        None,
        vec![op],
        vec!["api_token"],
    ));
    assert!(error.contains("graphql query template"), "{error}");
}

#[test]
fn http_api_enforces_effect_method_honesty() {
    // GET is read-only by definition.
    let mut op = list_issues();
    op.effect = OperationEffect::Compensatable;
    let error = manifest_error(http_api_manifest(
        "tickets",
        Some(bearer_auth()),
        vec![],
        None,
        vec![op],
        vec!["api_token"],
    ));
    assert!(error.contains("must be `read_only`"), "{error}");

    // DELETE is irreversible.
    let mut op = delete_issue();
    op.effect = OperationEffect::Compensatable;
    let error = manifest_error(http_api_manifest(
        "tickets",
        Some(bearer_auth()),
        vec![],
        None,
        vec![op],
        vec!["api_token"],
    ));
    assert!(error.contains("must be `irreversible`"), "{error}");

    // Only GET reads.
    let mut op = comment_issue();
    op.effect = OperationEffect::ReadOnly;
    let error = manifest_error(http_api_manifest(
        "tickets",
        Some(bearer_auth()),
        vec![],
        None,
        vec![op],
        vec!["api_token"],
    ));
    assert!(error.contains("only GET reads"), "{error}");

    // Idempotent exists only as a keyed POST.
    let mut op = comment_issue();
    op.method = HttpMethod::Patch;
    op.effect = OperationEffect::Idempotent;
    op.idempotency_key_header = Some("idempotency-key".to_owned());
    let error = manifest_error(http_api_manifest(
        "tickets",
        Some(bearer_auth()),
        vec![],
        None,
        vec![op],
        vec!["api_token"],
    ));
    assert!(error.contains("only a keyed POST"), "{error}");

    // An idempotent POST must declare its key header.
    let mut op = create_issue();
    op.idempotency_key_header = None;
    let error = manifest_error(http_api_manifest(
        "tickets",
        Some(bearer_auth()),
        vec![],
        None,
        vec![op.clone()],
        vec!["api_token"],
    ));
    assert!(error.contains("must declare its idempotency-key header"), "{error}");

    // A key header without the idempotent effect is a lie in the other
    // direction.
    let mut op = create_issue();
    op.effect = OperationEffect::Compensatable;
    let error = manifest_error(http_api_manifest(
        "tickets",
        Some(bearer_auth()),
        vec![],
        None,
        vec![op],
        vec!["api_token"],
    ));
    assert!(error.contains("without an `idempotent` effect"), "{error}");
}

#[test]
fn http_api_rejects_bad_response_and_timeout_overrides() {
    // Projections are JSON pointers.
    let mut op = get_issue();
    op.response.projection = Some("data.user".to_owned());
    let error = manifest_error(http_api_manifest(
        "tickets",
        Some(bearer_auth()),
        vec![],
        None,
        vec![op],
        vec!["api_token"],
    ));
    assert!(error.contains("projection"), "{error}");

    // Response ceilings tighten, never exceed.
    for max_bytes in [0, MAX_HTTP_API_RESPONSE_BYTES + 1] {
        let mut op = get_issue();
        op.response.max_bytes = Some(max_bytes);
        let error = manifest_error(http_api_manifest(
            "tickets",
            Some(bearer_auth()),
            vec![],
            None,
            vec![op],
            vec!["api_token"],
        ));
        assert!(error.contains("response ceiling"), "{max_bytes}: {error}");
    }

    // Timeouts are bounded.
    for timeout in [0, 60_001] {
        let mut op = get_issue();
        op.timeout_ms = Some(timeout);
        let error = manifest_error(http_api_manifest(
            "tickets",
            Some(bearer_auth()),
            vec![],
            None,
            vec![op],
            vec!["api_token"],
        ));
        assert!(error.contains("timeout"), "{timeout}: {error}");
    }
}

#[test]
fn http_api_validates_health_check_declarations() {
    // Unknown operation.
    let error = manifest_error(http_api_manifest(
        "tickets",
        Some(bearer_auth()),
        vec![],
        Some("ghost"),
        all_ops(),
        vec!["api_token"],
    ));
    assert!(error.contains("undeclared operation"), "{error}");

    // A mutating operation is not a health check.
    let error = manifest_error(http_api_manifest(
        "tickets",
        Some(bearer_auth()),
        vec![],
        Some("create-issue"),
        all_ops(),
        vec!["api_token"],
    ));
    assert!(error.contains("read-only GET"), "{error}");

    // A parameterized path is not a health check (connect supplies no
    // arguments).
    let error = manifest_error(http_api_manifest(
        "tickets",
        Some(bearer_auth()),
        vec![],
        Some("get-issue"),
        all_ops(),
        vec!["api_token"],
    ));
    assert!(error.contains("path parameters"), "{error}");

    // Required parameters are equally impossible at connect time.
    let error = manifest_error(http_api_manifest(
        "tickets",
        Some(bearer_auth()),
        vec![],
        Some("list-issues"),
        all_ops(),
        vec!["api_token"],
    ));
    assert!(error.contains("required parameters"), "{error}");
}

#[test]
fn http_api_manifest_hashes_are_canonical_and_content_sensitive() {
    let left = http_api_manifest(
        "tickets",
        Some(bearer_auth()),
        vec![
            ("x-b".to_owned(), "2".to_owned()),
            ("x-a".to_owned(), "1".to_owned()),
        ],
        Some("ping"),
        all_ops(),
        vec!["api_token"],
    )
    .expect("valid");

    // Operation and header order are canonicalized away.
    let mut reversed = all_ops();
    reversed.reverse();
    let right = http_api_manifest(
        "tickets",
        Some(bearer_auth()),
        vec![
            ("x-a".to_owned(), "1".to_owned()),
            ("x-b".to_owned(), "2".to_owned()),
        ],
        Some("ping"),
        reversed,
        vec!["api_token"],
    )
    .expect("valid");
    assert_eq!(left.hash, right.hash, "canonical content hashes equal");

    // Any content change is a new identity.
    let mut changed = all_ops();
    changed[0].description = "Lists issues, differently.".to_owned();
    let other = http_api_manifest(
        "tickets",
        Some(bearer_auth()),
        vec![
            ("x-a".to_owned(), "1".to_owned()),
            ("x-b".to_owned(), "2".to_owned()),
        ],
        Some("ping"),
        changed,
        vec!["api_token"],
    )
    .expect("valid");
    assert_ne!(left.hash, other.hash);

    // A deserialized manifest edited after hashing fails verification and
    // registration.
    let mut tampered = left.clone();
    tampered.display_name = "Tampered".to_owned();
    assert!(!tampered.verify_hash());
    let mut registry = ConnectorRegistry::new();
    let provider = HttpApiProvider::from_manifest(&left).expect("provider");
    let error = registry
        .register_manifest(tampered, Arc::new(provider))
        .expect_err("tampered manifest must be rejected")
        .to_string();
    assert!(error.contains("hash does not match"), "{error}");
}

// ---------------------------------------------------------------------------
// Catalog derivation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_api_catalog_is_namespaced_sorted_and_effect_honest() {
    let manifest = api_manifest("tickets");
    let provider = HttpApiProvider::from_manifest(&manifest).expect("provider");
    let catalog = provider.catalog("tickets").expect("catalog");

    let names: Vec<&str> = catalog.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "tickets/comment-issue",
            "tickets/create-issue",
            "tickets/delete-issue",
            "tickets/get-issue",
            "tickets/graphql-create",
            "tickets/list-issues",
            "tickets/ping",
        ],
        "namespaced and sorted"
    );

    let by_name = |name: &str| catalog.iter().find(|c| c.name == name).expect(name);
    assert_eq!(by_name("tickets/list-issues").effect, Effect::ReadOnly);
    assert_eq!(by_name("tickets/get-issue").effect, Effect::ReadOnly);
    assert_eq!(by_name("tickets/create-issue").effect, Effect::Idempotent);
    assert_eq!(by_name("tickets/comment-issue").effect, Effect::Compensatable);
    assert_eq!(by_name("tickets/graphql-create").effect, Effect::Compensatable);
    assert_eq!(by_name("tickets/delete-issue").effect, Effect::NonIdempotent);

    // The params schema passes through untouched.
    assert_eq!(
        by_name("tickets/create-issue").parameters_schema,
        create_issue().params_schema
    );
    assert_eq!(
        by_name("tickets/list-issues").description,
        "The list-issues operation."
    );

    // The session serves the same derivation, so registry generations pin
    // exactly what dispatch executes.
    let mut session = provider
        .connect(&manifest, &bearer_credentials())
        .await
        .expect("connect");
    let served = session.catalog().await.expect("session catalog");
    assert_eq!(served, catalog);
    session.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// Auth styles and secret hygiene
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_api_bearer_auth_injects_header_and_never_leaks() {
    let (provider, transport) = provider_and_transport().await;
    transport.push_json(200, json!({"issues": []}));
    let result = provider
        .execute(
            transport.as_ref(),
            &bearer_credentials(),
            "inst-000001",
            "list-issues",
            &json!({"team": "eng"}),
        )
        .await
        .expect("execute");

    assert_eq!(result, json!({"issues": []}));
    let captured = transport.captured();
    assert_eq!(captured.len(), 1);
    let request = &captured[0];
    assert_eq!(request.method, HttpMethod::Get);
    assert_eq!(request.url, "https://api.example.com/v1/issues?team=eng");
    assert_eq!(header(request, "authorization"), Some("Bearer sekrit-token"));
    assert_eq!(header(request, "x-client"), Some("rusty-test"));
    assert!(header(request, "content-type").is_none(), "no body, no content-type");
    assert!(request.body.is_empty());

    // The secret appears in the outbound header and nowhere else: not in
    // the provider's Debug, not in error strings.
    let debug = format!("{provider:?}");
    assert!(!debug.contains(SECRET), "provider Debug leaked: {debug}");

    // An auth failure echoes no body at all (it may quote the credential's
    // neighborhood back).
    transport.push_json(
        401,
        json!({"error": format!("token {SECRET} is invalid")}),
    );
    let error = provider
        .execute(
            transport.as_ref(),
            &bearer_credentials(),
            "inst-000001",
            "list-issues",
            &json!({"team": "eng"}),
        )
        .await
        .expect_err("401 must fail")
        .to_string();
    assert!(error.contains("rejected"), "{error}");
    assert!(!error.contains(SECRET), "error leaked the secret: {error}");
}

#[tokio::test]
async fn http_api_basic_and_header_and_query_auth_styles() {
    // Basic: base64(user:pass) in the authorization header.
    let manifest = http_api_manifest(
        "basic-api",
        Some(HttpApiAuth::Basic {
            username_slot: "username".to_owned(),
            password_slot: "password".to_owned(),
        }),
        vec![],
        None,
        vec![ping()],
        vec!["username", "password"],
    )
    .expect("valid");
    let provider = HttpApiProvider::from_manifest(&manifest).expect("provider");
    let transport = FakeApiTransport::default();
    transport.push_json(200, json!({"ok": true}));
    provider
        .execute(
            &transport,
            &[handle("username", "ada"), handle("password", "p@ss")],
            "inst-000001",
            "ping",
            &json!({}),
        )
        .await
        .expect("execute");
    let captured = transport.captured();
    // base64("ada:p@ss") — verified against RFC 4648.
    assert_eq!(
        header(&captured[0], "authorization"),
        Some("Basic YWRhOnBAc3M=")
    );

    // Header style: the secret is the header value, verbatim.
    let manifest = http_api_manifest(
        "header-api",
        Some(HttpApiAuth::Header {
            header: "X-Api-Key".to_owned(),
            credential_slot: "api_token".to_owned(),
        }),
        vec![],
        None,
        vec![ping()],
        vec!["api_token"],
    )
    .expect("valid");
    let provider = HttpApiProvider::from_manifest(&manifest).expect("provider");
    let transport = FakeApiTransport::default();
    transport.push_json(200, json!({"ok": true}));
    provider
        .execute(
            &transport,
            &bearer_credentials(),
            "inst-000001",
            "ping",
            &json!({}),
        )
        .await
        .expect("execute");
    let captured = transport.captured();
    assert_eq!(header(&captured[0], "x-api-key"), Some(SECRET));
    assert!(header(&captured[0], "authorization").is_none());

    // Query-param style: the secret joins the query string, encoded.
    let manifest = http_api_manifest(
        "query-api",
        Some(HttpApiAuth::QueryParam {
            param: "apikey".to_owned(),
            credential_slot: "api_token".to_owned(),
        }),
        vec![],
        None,
        vec![list_issues()],
        vec!["api_token"],
    )
    .expect("valid");
    let provider = HttpApiProvider::from_manifest(&manifest).expect("provider");
    let transport = FakeApiTransport::default();
    transport.push_json(200, json!({"issues": []}));
    provider
        .execute(
            &transport,
            &[handle("api_token", "key with/slash")],
            "inst-000001",
            "list-issues",
            &json!({"team": "eng"}),
        )
        .await
        .expect("execute");
    let captured = transport.captured();
    assert_eq!(
        captured[0].url,
        "https://api.example.com/v1/issues?team=eng&apikey=key%20with%2Fslash"
    );
    assert!(header(&captured[0], "authorization").is_none());
}

#[tokio::test]
async fn http_api_connect_fails_closed_on_unresolved_slots() {
    let manifest = api_manifest("tickets");
    let provider = HttpApiProvider::from_manifest(&manifest).expect("provider");
    let error = provider
        .connect(&manifest, &[])
        .await
        .expect_err("missing credential must fail connect")
        .to_string();
    assert!(error.contains("credential slot `api_token`"), "{error}");
    assert!(!error.contains(SECRET));

    // A provider never serves the wrong manifest kind.
    let search = ConnectorManifest::new(
        "web-search",
        "1.0.0",
        "Search",
        "A search connector.",
        ProviderKind::HttpSearch(rusty_agent_runtime::connector::HttpSearchSpec {
            base_url: "https://search.example.com".to_owned(),
            auth: None,
        }),
        vec![],
        vec![],
    )
    .expect("valid search manifest");
    let error = provider
        .connect(&search, &[])
        .await
        .expect_err("wrong kind must fail")
        .to_string();
    assert!(error.contains("http-search"), "{error}");
}

// ---------------------------------------------------------------------------
// Execution: templating, bodies, GraphQL
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_api_renders_paths_and_queries_fail_closed() {
    let (provider, transport) = provider_and_transport().await;

    // Path placeholders take percent-encoded scalars; query params encode.
    transport.push_json(200, json!({"id": "i1"}));
    provider
        .execute(
            transport.as_ref(),
            &bearer_credentials(),
            "inst-000001",
            "get-issue",
            &json!({"issue_id": "ISS UE/1"}),
        )
        .await
        .expect("execute");
    let captured = transport.captured();
    assert_eq!(
        captured[0].url,
        "https://api.example.com/v1/issues/ISS%20UE%2F1"
    );

    // A structured value has no honest path rendering.
    let error = provider
        .execute(
            transport.as_ref(),
            &bearer_credentials(),
            "inst-000001",
            "get-issue",
            &json!({"issue_id": {"nested": true}}),
        )
        .await
        .expect_err("structured path param must fail")
        .to_string();
    assert!(error.contains("string, number, or boolean"), "{error}");

    // A missing path argument is an error naming the parameter.
    let error = provider
        .execute(
            transport.as_ref(),
            &bearer_credentials(),
            "inst-000001",
            "get-issue",
            &json!({}),
        )
        .await
        .expect_err("missing argument must fail")
        .to_string();
    assert!(error.contains("`issue_id`"), "{error}");

    // Unknown arguments are rejected, not silently dropped.
    let error = provider
        .execute(
            transport.as_ref(),
            &bearer_credentials(),
            "inst-000001",
            "list-issues",
            &json!({"team": "eng", "tem": "typo"}),
        )
        .await
        .expect_err("unexpected argument must fail")
        .to_string();
    assert!(error.contains("unexpected argument `tem`"), "{error}");

    // Missing required arguments are rejected before any network call.
    let before = transport.captured().len();
    let error = provider
        .execute(
            transport.as_ref(),
            &bearer_credentials(),
            "inst-000001",
            "list-issues",
            &json!({}),
        )
        .await
        .expect_err("missing required must fail")
        .to_string();
    assert!(error.contains("missing required argument `team`"), "{error}");
    assert_eq!(transport.captured().len(), before, "no request went out");
}

#[tokio::test]
async fn http_api_json_body_contains_only_routed_params() {
    let (provider, transport) = provider_and_transport().await;
    transport.push_json(201, json!({"id": "i1"}));
    provider
        .execute(
            transport.as_ref(),
            &bearer_credentials(),
            "inst-000001",
            "create-issue",
            &json!({"team_id": "t1", "title": "Hello", "priority": 3}),
        )
        .await
        .expect("execute");
    let captured = transport.captured();
    let request = &captured[0];
    assert_eq!(request.method, HttpMethod::Post);
    assert_eq!(request.url, "https://api.example.com/v1/teams/t1/issues");
    assert_eq!(header(request, "content-type"), Some("application/json"));
    // team_id is a path parameter: it must not leak into the body.
    let body: Value = serde_json::from_slice(&request.body).expect("body is JSON");
    assert_eq!(body, json!({"title": "Hello", "priority": 3}));
}

#[tokio::test]
async fn http_api_graphql_interpolates_with_json_escaping() {
    let (provider, transport) = provider_and_transport().await;
    transport.push_json(200, json!({"data": {"issueCreate": {"success": true}}}));
    let result = provider
        .execute(
            transport.as_ref(),
            &bearer_credentials(),
            "inst-000001",
            "graphql-create",
            &json!({"title": "He said \"hi\"", "priority": 2}),
        )
        .await
        .expect("execute");
    assert_eq!(result, json!({"data": {"issueCreate": {"success": true}}}));

    let captured = transport.captured();
    let body: Value = serde_json::from_slice(&captured[0].body).expect("body is JSON");
    // The string argument arrived quoted and escaped — interpolation cannot
    // break out of its GraphQL position — and the number arrived bare.
    assert_eq!(
        body,
        json!({
            "query": "mutation { issueCreate(input: { title: \"He said \\\"hi\\\"\", priority: 2 }) { success } }"
        })
    );

    // An absent optional parameter fails honestly rather than rendering a
    // hole.
    let error = provider
        .execute(
            transport.as_ref(),
            &bearer_credentials(),
            "inst-000001",
            "graphql-create",
            &json!({"title": "No priority"}),
        )
        .await
        .expect_err("missing template argument must fail")
        .to_string();
    assert!(error.contains("`priority`"), "{error}");
}

// ---------------------------------------------------------------------------
// Idempotency
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_api_idempotency_keys_are_deterministic_and_scoped() {
    let args = json!({"team_id": "t1", "title": "Hello", "priority": 3});

    // Deterministic, and insensitive to JSON key order (canonical args).
    let shuffled = json!({"priority": 3, "title": "Hello", "team_id": "t1"});
    assert_eq!(
        derive_idempotency_key("inst-000001", "create-issue", &args),
        derive_idempotency_key("inst-000001", "create-issue", &shuffled)
    );

    // Every tuple component is load-bearing.
    let base = derive_idempotency_key("inst-000001", "create-issue", &args);
    assert_ne!(base, derive_idempotency_key("inst-000002", "create-issue", &args));
    assert_ne!(base, derive_idempotency_key("inst-000001", "comment-issue", &args));
    assert_ne!(
        base,
        derive_idempotency_key("inst-000001", "create-issue", &json!({"team_id": "t1", "title": "Other"}))
    );

    // Dispatch sends the derived key, and a retry of the same call sends
    // the same one — no double-create.
    let (provider, transport) = provider_and_transport().await;
    for _ in 0..2 {
        transport.push_json(201, json!({"id": "i1"}));
        provider
            .execute(
                transport.as_ref(),
                &bearer_credentials(),
                "inst-000001",
                "create-issue",
                &args,
            )
            .await
            .expect("execute");
    }
    let captured = transport.captured();
    let first = header(&captured[0], "idempotency-key").expect("key header").to_owned();
    let second = header(&captured[1], "idempotency-key").expect("key header").to_owned();
    assert_eq!(first, second, "retries present the same key");
    assert_eq!(first, base, "dispatch uses the documented derivation");
    assert_eq!(first.len(), 64, "keys are hex SHA-256 digests");

    // The Tool contract agrees: the key the admission boundary sees is the
    // key dispatch sends, and unkeyed operations answer None.
    let tool = HttpApiTool::new(
        "tickets",
        provider.clone(),
        "create-issue",
        Arc::new(FakeApiTransport::default()),
        bearer_credentials(),
        "inst-000001",
    )
    .expect("tool");
    assert_eq!(tool.idempotency_key(&args).as_deref(), Some(base.as_str()));
    assert_eq!(tool.effect(), Effect::Idempotent);

    let comment = HttpApiTool::new(
        "tickets",
        provider,
        "comment-issue",
        Arc::new(FakeApiTransport::default()),
        bearer_credentials(),
        "inst-000001",
    )
    .expect("tool");
    assert_eq!(comment.idempotency_key(&json!({"issue_id": "i1", "text": "hi"})), None);
    assert_eq!(comment.effect(), Effect::Compensatable);
}

// ---------------------------------------------------------------------------
// Ceilings, timeouts, error mapping, projection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_api_enforces_byte_ceilings_and_timeouts() {
    let (provider, transport) = provider_and_transport().await;

    // Response over the provider ceiling.
    let big = "x".repeat(MAX_HTTP_API_RESPONSE_BYTES + 1);
    transport.push_json(200, json!({"blob": big}));
    let error = provider
        .execute(
            transport.as_ref(),
            &bearer_credentials(),
            "inst-000001",
            "list-issues",
            &json!({"team": "eng"}),
        )
        .await
        .expect_err("oversized response must fail")
        .to_string();
    assert!(error.contains("byte ceiling"), "{error}");

    // A per-operation ceiling tightens further.
    let manifest = api_manifest("tickets");
    let mut tight = manifest;
    let ProviderKind::HttpApi(spec) = &mut tight.provider else {
        unreachable!()
    };
    let op = spec
        .operations
        .iter_mut()
        .find(|o| o.name == "get-issue")
        .expect("op");
    op.response.max_bytes = Some(16);
    let tight = ConnectorManifest::new(
        tight.id.clone(),
        tight.version.clone(),
        tight.display_name.clone(),
        tight.description.clone(),
        tight.provider.clone(),
        tight.capabilities.clone(),
        tight.credential_slots.clone(),
    )
    .expect("still valid");
    let provider = HttpApiProvider::from_manifest(&tight).expect("provider");
    let transport = FakeApiTransport::default();
    transport.push_json(200, json!({"id": "i1", "title": "this is far too long"}));
    let error = provider
        .execute(
            &transport,
            &bearer_credentials(),
            "inst-000001",
            "get-issue",
            &json!({"issue_id": "i1"}),
        )
        .await
        .expect_err("tight ceiling must fail")
        .to_string();
    assert!(error.contains("16-byte ceiling"), "{error}");

    // The request body ceiling holds on the way out.
    let (provider, transport) = provider_and_transport().await;
    let huge = "x".repeat(MAX_HTTP_API_REQUEST_BYTES);
    let error = provider
        .execute(
            transport.as_ref(),
            &bearer_credentials(),
            "inst-000001",
            "comment-issue",
            &json!({"issue_id": "i1", "text": huge}),
        )
        .await
        .expect_err("oversized request body must fail")
        .to_string();
    assert!(error.contains("exceeds"), "{error}");
    assert!(transport.captured().is_empty(), "nothing went out");

    // Per-operation timeouts are enforced around the transport.
    let manifest = api_manifest("tickets");
    let mut slow = manifest;
    let ProviderKind::HttpApi(spec) = &mut slow.provider else {
        unreachable!()
    };
    spec.operations
        .iter_mut()
        .find(|o| o.name == "ping")
        .expect("op")
        .timeout_ms = Some(50);
    let slow = ConnectorManifest::new(
        slow.id.clone(),
        slow.version.clone(),
        slow.display_name.clone(),
        slow.description.clone(),
        slow.provider.clone(),
        slow.capabilities.clone(),
        slow.credential_slots.clone(),
    )
    .expect("still valid");
    let provider = HttpApiProvider::from_manifest(&slow).expect("provider");
    let transport = FakeApiTransport::default();
    transport.set_latency(Duration::from_millis(500));
    transport.push_json(200, json!({"ok": true}));
    let error = provider
        .execute(
            &transport,
            &bearer_credentials(),
            "inst-000001",
            "ping",
            &json!({}),
        )
        .await
        .expect_err("slow reply must time out")
        .to_string();
    assert!(error.contains("timed out"), "{error}");
}

#[tokio::test]
async fn http_api_maps_errors_with_truncated_sanitized_bodies() {
    let (provider, transport) = provider_and_transport().await;

    // A 500 echoes a bounded, control-stripped excerpt of the body.
    let long_body = format!("boom\x1b[31m {}", "y".repeat(1024));
    transport.push(HttpResponse {
        status: 500,
        body: long_body.into_bytes(),
    });
    let error = provider
        .execute(
            transport.as_ref(),
            &bearer_credentials(),
            "inst-000001",
            "get-issue",
            &json!({"issue_id": "i1"}),
        )
        .await
        .expect_err("500 must fail")
        .to_string();
    assert!(error.contains("status 500"), "{error}");
    assert!(error.contains("boom"), "{error}");
    assert!(!error.contains('\x1b'), "control bytes stripped: {error}");
    assert!(
        error.len() <= 512 + MAX_HTTP_API_ERROR_BODY_BYTES + 64,
        "bounded excerpt: {} bytes",
        error.len()
    );
    assert!(error.contains("[truncated]"), "{error}");

    // A transport failure propagates as a tool error.
    let error = provider
        .execute(
            transport.as_ref(),
            &bearer_credentials(),
            "inst-000001",
            "get-issue",
            &json!({"issue_id": "i1"}),
        )
        .await
        .expect_err("unscripted call must fail")
        .to_string();
    assert!(error.contains("no scripted reply"), "{error}");
}

#[tokio::test]
async fn http_api_response_projection_and_passthrough() {
    // Projected operation: `/data/user/name` out of a nested reply.
    let mut op = get_issue();
    op.name = "get-user-name".to_owned();
    op.path = "/v1/users/{issue_id}".to_owned();
    op.response.projection = Some("/data/user/name".to_owned());
    let manifest = http_api_manifest(
        "tickets",
        Some(bearer_auth()),
        vec![],
        None,
        vec![op],
        vec!["api_token"],
    )
    .expect("valid");
    let provider = HttpApiProvider::from_manifest(&manifest).expect("provider");
    let transport = FakeApiTransport::default();

    transport.push_json(200, json!({"data": {"user": {"name": "Ada", "id": 7}}}));
    let result = provider
        .execute(
            &transport,
            &bearer_credentials(),
            "inst-000001",
            "get-user-name",
            &json!({"issue_id": "u1"}),
        )
        .await
        .expect("execute");
    assert_eq!(result, json!("Ada"));

    // A pointer that resolves nowhere is an error, not a null.
    transport.push_json(200, json!({"data": {"user": {}}}));
    let error = provider
        .execute(
            &transport,
            &bearer_credentials(),
            "inst-000001",
            "get-user-name",
            &json!({"issue_id": "u1"}),
        )
        .await
        .expect_err("unresolved projection must fail")
        .to_string();
    assert!(error.contains("did not resolve"), "{error}");

    // Passthrough: a non-JSON body is still the answer, as text.
    let (provider, transport) = provider_and_transport().await;
    transport.push(HttpResponse {
        status: 200,
        body: b"plain text answer".to_vec(),
    });
    let result = provider
        .execute(
            transport.as_ref(),
            &bearer_credentials(),
            "inst-000001",
            "ping",
            &json!({}),
        )
        .await
        .expect("execute");
    assert_eq!(result, json!("plain text answer"));

    // But a projected operation needs JSON to point into.
    transport.push(HttpResponse {
        status: 200,
        body: b"not json".to_vec(),
    });
    let provider = HttpApiProvider::from_manifest(
        &http_api_manifest(
            "tickets",
            Some(bearer_auth()),
            vec![],
            None,
            vec![{
                let mut op = get_issue();
                op.response.projection = Some("/id".to_owned());
                op
            }],
            vec!["api_token"],
        )
        .expect("valid"),
    )
    .expect("provider");
    let error = provider
        .execute(
            transport.as_ref(),
            &bearer_credentials(),
            "inst-000001",
            "get-issue",
            &json!({"issue_id": "i1"}),
        )
        .await
        .expect_err("projection over non-JSON must fail")
        .to_string();
    assert!(error.contains("was not JSON"), "{error}");
}

// ---------------------------------------------------------------------------
// Tool dispatch and registry lifecycle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_api_tool_executes_through_the_provider_contract() {
    let (provider, transport) = provider_and_transport().await;
    let tool = HttpApiTool::new(
        "tickets",
        provider,
        "get-issue",
        transport.clone(),
        bearer_credentials(),
        "inst-000001",
    )
    .expect("tool");

    assert_eq!(tool.name(), "tickets/get-issue");
    assert_eq!(tool.description(), "The get-issue operation.");
    assert_eq!(tool.effect(), Effect::ReadOnly);
    assert!(tool.parameters_schema().is_object());

    transport.push_json(200, json!({"id": "i1", "title": "Hello"}));
    let result = tool.call(json!({"issue_id": "i1"})).await.expect("call");
    assert_eq!(result, json!({"id": "i1", "title": "Hello"}));
    let captured = transport.captured();
    assert_eq!(captured[0].method, HttpMethod::Get);
    assert_eq!(captured[0].url, "https://api.example.com/v1/issues/i1");

    // The tool's Debug redacts credentials by construction.
    let debug = format!("{tool:?}");
    assert!(!debug.contains(SECRET), "tool Debug leaked: {debug}");
    assert!(debug.contains("[redacted]"), "{debug}");

    // Unknown operations fail at construction.
    let error = HttpApiTool::new(
        "tickets",
        HttpApiProvider::from_manifest(&api_manifest("tickets")).expect("provider"),
        "ghost-op",
        Arc::new(FakeApiTransport::default()),
        vec![],
        "inst-000001",
    )
    .expect_err("unknown operation must fail")
    .to_string();
    assert!(error.contains("no operation `ghost-op`"), "{error}");
}

#[tokio::test]
async fn http_api_health_check_runs_at_connect() {
    let manifest = api_manifest("tickets");
    let transport = Arc::new(FakeApiTransport::default());
    transport.push_json(200, json!({"ok": true}));
    let provider = HttpApiProvider::from_manifest(&manifest)
        .expect("provider")
        .with_health_transport(transport.clone());

    let mut registry = ConnectorRegistry::new();
    let hash = registry
        .register_manifest(manifest, Arc::new(provider))
        .expect("register");
    let mut broker = InMemoryCredentialBroker::new();
    broker.insert("acme", "api_token", SECRET);
    let instance_id = registry
        .instantiate(&hash, "acme", &broker)
        .expect("instantiate");
    registry.connect(&instance_id, 1_000).await.expect("connect");

    let instance = registry.instance(&instance_id).expect("instance");
    assert_eq!(instance.state(), &LifecycleState::Healthy);
    let captured = transport.captured();
    assert_eq!(captured.len(), 1, "the health check ran once");
    assert_eq!(captured[0].method, HttpMethod::Get);
    assert_eq!(captured[0].url, "https://api.example.com/v1/ping");
    assert_eq!(
        header(&captured[0], "authorization"),
        Some("Bearer sekrit-token")
    );

    // A failing health check lands the instance in `failed` with the
    // bounded reason — and without the secret.
    let manifest = api_manifest("tickets");
    let transport = Arc::new(FakeApiTransport::default());
    transport.push_json(503, json!({"error": "down"}));
    let provider = HttpApiProvider::from_manifest(&manifest)
        .expect("provider")
        .with_health_transport(transport);
    let mut registry = ConnectorRegistry::new();
    let hash = registry
        .register_manifest(manifest, Arc::new(provider))
        .expect("register");
    let instance_id = registry
        .instantiate(&hash, "acme", &broker)
        .expect("instantiate");
    registry.connect(&instance_id, 1_000).await.expect("connect");
    let instance = registry.instance(&instance_id).expect("instance");
    match instance.state() {
        LifecycleState::Failed { reason } => {
            assert!(reason.contains("health check `ping` failed"), "{reason}");
            assert!(reason.contains("status 503"), "{reason}");
            assert!(!reason.contains(SECRET), "{reason}");
        }
        other => panic!("expected failed, got {other:?}"),
    }
}

#[tokio::test]
async fn http_api_integrates_with_registry_lifecycle_and_generations() {
    let mut registry = ConnectorRegistry::new();
    let manifest = api_manifest("tickets");
    // The default provider wires no health transport: the catalog is
    // declarative and health is reported at call time.
    let hash = registry
        .register_manifest_with_default(manifest)
        .expect("register");

    // An unresolved slot produces a `failed` instance naming the slot.
    let broker = InMemoryCredentialBroker::new();
    let broken = registry
        .instantiate(&hash, "globex", &broker)
        .expect("instantiate");
    match registry.instance(&broken).expect("instance").state() {
        LifecycleState::Failed { reason } => {
            assert!(reason.contains("api_token"), "{reason}");
            assert!(reason.contains("globex"), "{reason}");
        }
        other => panic!("expected failed, got {other:?}"),
    }

    // With the credential resolved, connect derives and pins the catalog.
    let mut broker = InMemoryCredentialBroker::new();
    broker.insert("acme", "api_token", SECRET);
    let instance_id = registry
        .instantiate(&hash, "acme", &broker)
        .expect("instantiate");
    registry.connect(&instance_id, 1_000).await.expect("connect");
    let instance = registry.instance(&instance_id).expect("instance");
    assert_eq!(instance.state(), &LifecycleState::Healthy);
    let catalog = instance.catalog().expect("catalog");
    assert_eq!(catalog.generation, 1);
    assert_eq!(catalog.tools.len(), 7);
    assert!(
        catalog.tools.iter().any(|t| t.name == "tickets/graphql-create"),
        "graphql operation advertised"
    );
    let pin = registry.catalog_pin(&instance_id).expect("pin");
    assert!(registry
        .instance(&instance_id)
        .expect("instance")
        .verify_pin(&pin));

    // The declarative catalog never changes bytes, so the sweep holds the
    // generation steady.
    let outcomes = registry.health_sweep(2_000).await;
    assert_eq!(outcomes.len(), 1);
    assert!(!outcomes[0].catalog_bumped);
    assert_eq!(outcomes[0].current, LifecycleState::Healthy);

    // Disable shuts the session down; enable returns to pending for a
    // fresh connect.
    registry.disable(&instance_id).await.expect("disable");
    assert_eq!(
        registry.instance(&instance_id).expect("instance").state(),
        &LifecycleState::Disabled
    );
    registry.enable(&instance_id).expect("enable");
    assert_eq!(
        registry.instance(&instance_id).expect("instance").state(),
        &LifecycleState::Pending
    );
    registry.connect(&instance_id, 3_000).await.expect("reconnect");
    assert_eq!(
        registry.instance(&instance_id).expect("instance").state(),
        &LifecycleState::Healthy
    );
}
