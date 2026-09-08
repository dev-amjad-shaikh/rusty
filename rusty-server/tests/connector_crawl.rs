//! Connector coverage-crawl integration tests (EP-07-S07): a fixture
//! connector serving a knowledge corpus — one exact-signature article,
//! one weak keyword match, one stale article referencing a retired
//! system — crawled through the governed operation, asserting the
//! claim grades and freshness assessments, the receipts every call
//! leaves, the typed refusals, and tenant isolation.
//!
//! Driven in-process via `tower::ServiceExt::oneshot` with the transport
//! override (`ServerConfig::with_connector_transport`) — no sockets.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::http::{Request, StatusCode};
use rusty_agent_runtime::connector::{
    CheckRequest, CheckResponse, ConnectorManifest, ConnectorOperation, ConnectorTransport,
    HttpMethod, OperationAuth, OperationEffect,
};
use rusty_agent_runtime::error::Result as RuntimeResult;
use rusty_agent_server::{GraphRegistry, ServerConfig, router};
use serde_json::{Value, json};
use tower::ServiceExt;

// --------------------------------------------------------------------- //
// Harness (the connector_ingestion.rs pattern)
// --------------------------------------------------------------------- //

fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!("rusty-server-crawl-test-{}", uuid::Uuid::new_v4()))
}

/// The scripted corpus transport: answers the canned body, records
/// every request the plane sends.
#[derive(Debug)]
struct CorpusTransport {
    body: Vec<u8>,
    seen: Mutex<Vec<CheckRequest>>,
}

impl CorpusTransport {
    fn serving(body: Value) -> Self {
        Self {
            body: body.to_string().into_bytes(),
            seen: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait::async_trait]
impl ConnectorTransport for CorpusTransport {
    async fn send(&self, request: CheckRequest) -> RuntimeResult<CheckResponse> {
        self.seen.lock().unwrap().push(request.clone());
        Ok(CheckResponse {
            status: 200,
            body: self.body.clone(),
        })
    }
}

fn app_with(transport: Arc<CorpusTransport>) -> (Router, PathBuf) {
    let store = temp_store();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_connector_transport(transport);
    (router(GraphRegistry::new(), config), store)
}

async fn call_as(
    app: &Router,
    auth: Option<&str>,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(key) = auth {
        builder = builder.header("X-Api-Key", key);
    }
    let body = match body {
        Some(v) => {
            builder = builder.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes: Bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    call_as(app, None, method, uri, body).await
}

fn manifest() -> Value {
    serde_json::to_value(
        ConnectorManifest::new(
            "servicenow",
            "1",
            "ServiceNow",
            "ServiceNow corpus operations.",
            "https://docs.servicenow.com/",
            "https://{instance}.service-now.com",
            json!({
                "type": "object",
                "required": ["instance", "credentials"],
                "properties": {
                    "instance": {"type": "string"},
                    "credentials": {
                        "type": "object",
                        "required": ["token"],
                        "properties": {"token": {"type": "string", "rusty_secret": true}}
                    }
                }
            }),
            vec![
                ConnectorOperation {
                    name: "read-kb".to_owned(),
                    description: "Read the knowledge corpus.".to_owned(),
                    method: HttpMethod::Get,
                    path: "/api/now/kb".to_owned(),
                    effect: OperationEffect::ReadOnly,
                    params_schema: json!({"type": "object"}),
                    headers: Vec::new(),
                    auth: vec![OperationAuth::Bearer {
                        token: "{credentials.token}".to_owned(),
                    }],
                    max_response_bytes: None,
                },
                ConnectorOperation {
                    name: "create-article".to_owned(),
                    description: "Create a knowledge article.".to_owned(),
                    method: HttpMethod::Post,
                    path: "/api/now/table/kb_knowledge".to_owned(),
                    effect: OperationEffect::Compensatable,
                    params_schema: json!({"type": "object"}),
                    headers: Vec::new(),
                    auth: vec![OperationAuth::Bearer {
                        token: "{credentials.token}".to_owned(),
                    }],
                    max_response_bytes: None,
                },
                ConnectorOperation {
                    name: "check-connection".to_owned(),
                    description: "The check operation.".to_owned(),
                    method: HttpMethod::Get,
                    path: "/api/now/table/sys_user?sysparm_limit=1".to_owned(),
                    effect: OperationEffect::ReadOnly,
                    params_schema: json!({"type": "object"}),
                    headers: Vec::new(),
                    auth: vec![OperationAuth::Bearer {
                        token: "{credentials.token}".to_owned(),
                    }],
                    max_response_bytes: None,
                },
            ],
            "check-connection",
        )
        .expect("the fixture manifest validates"),
    )
    .unwrap()
}

/// Register the fixture manifest and instantiate it; returns the
/// instance id.
async fn register_and_instantiate(app: &Router) -> String {
    let (status, _) = call(app, "POST", "/connectors", Some(manifest())).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = call(
        app,
        "POST",
        "/connectors/instances",
        Some(json!({
            "manifest_hash": manifest()["hash"].clone(),
            "config": {
                "instance": "dev123",
                "credentials": {"token": "s3cret-token"}
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["instance_id"].as_str().unwrap().to_owned()
}

/// Record one interaction event, seeding the demand side the crawl
/// joins against.
async fn record_event(app: &Router, record_id: &str, utterance: &str, day: u64) {
    let (status, v) = call(
        app,
        "POST",
        "/gaps/events",
        Some(json!({
            "source": {"system": "servicenow", "stream": "incident", "record_id": record_id},
            "actor": {"role": "employee", "id": "u-1"},
            "channel": "incident",
            "utterance": utterance,
            "resolution_path": "human_resolved",
            "outcome": "resolved",
            "occurred_at": format!("2026-08-{day:02}T09:00:00Z"),
            "resolved_at": format!("2026-08-{day:02}T10:00:00Z"),
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "event ingest failed: {v}");
}

/// Seed the demand corpus: a two-event vpn cluster and a one-event
/// password cluster.
async fn seed_demand(app: &Router) {
    record_event(
        app,
        "INC001",
        "vpn connect home office certificate error",
        1,
    )
    .await;
    record_event(app, "INC002", "vpn connect home office drops hourly", 2).await;
    record_event(app, "INC003", "password reset portal", 3).await;
}

/// The fixture knowledge corpus: one exact-signature vpn article, one
/// weak keyword password article, one stale article on a retired
/// system.
fn knowledge_corpus() -> Value {
    json!({ "result": [
        {
            "sys_id": "KB-1",
            "short_description": "vpn connect home office certificate error troubleshooting",
            "text": "When the vpn still drops hourly after failing to connect from home \
                     office, check the ZTNA certificate error logs.",
            "sys_updated_on": "2026-08-01T00:00:00Z",
            "u_systems": "ztna"
        },
        {
            "sys_id": "KB-2",
            "short_description": "Account help",
            "text": "If you cannot sign in, the password portal can help.",
            "sys_updated_on": "2026-07-20T00:00:00Z"
        },
        {
            "sys_id": "KB-3",
            "short_description": "password reset through legacy erp",
            "text": "File the reset in the legacy erp console.",
            "sys_updated_on": "2024-01-01T00:00:00Z",
            "u_systems": "legacy-erp"
        }
    ]})
}

// --------------------------------------------------------------------- //
// Tests
// --------------------------------------------------------------------- //

#[tokio::test]
async fn crawl_grades_claims_assesses_freshness_and_receipts_every_call() {
    let transport = Arc::new(CorpusTransport::serving(knowledge_corpus()));
    let (app, store) = app_with(transport.clone());
    let instance_id = register_and_instantiate(&app).await;
    seed_demand(&app).await;

    let (status, body) = call(
        &app,
        "POST",
        &format!("/connectors/instances/{instance_id}/crawl"),
        Some(json!({
            "operation": "read-kb",
            "system": "servicenow",
            "kind": "kb_article",
            "retired_systems": ["legacy-erp"]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["artifacts"], json!(3));

    let claims = body["coverage_map"]["claims"].as_array().unwrap();
    let claim_for = |artifact_id: &str| {
        claims
            .iter()
            .find(|claim| claim["artifact"]["id"] == json!(artifact_id))
            .unwrap_or_else(|| panic!("no claim cites {artifact_id}: {claims:?}"))
            .clone()
    };

    // The strong match carries the exact-signature grade; the weak one
    // earns keyword overlap; every claim cites its artifact.
    assert_eq!(claim_for("KB-1")["confidence"], json!("exact_signature"));
    assert_eq!(claim_for("KB-2")["confidence"], json!("keyword_overlap"));

    // The stale article on the retired system is exposed, not trusted.
    let stale = claim_for("KB-3");
    assert_eq!(stale["freshness"]["stale"], json!(true));
    assert_eq!(stale["freshness"]["references_retired_system"], json!(true));

    // One connector call, one receipt — and the audit route reads it
    // back.
    let receipts = body["receipts"].as_array().unwrap();
    assert_eq!(receipts.len(), 1, "{body}");
    let (status, listed) = call(
        &app,
        "GET",
        &format!("/connectors/instances/{instance_id}/receipts"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let listed = listed["receipts"].as_array().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["receipt_id"], receipts[0]);
    assert_eq!(listed[0]["operation"], json!("read-kb"));
    assert_eq!(listed[0]["status"], json!(200));

    // The call carried the opened secret; the persisted instance record
    // never does.
    let seen = transport.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert!(
        seen[0]
            .headers
            .iter()
            .any(|(name, value)| name == "Authorization" && value == "Bearer s3cret-token")
    );
    drop(seen);
    let on_disk = std::fs::read_to_string(
        store
            .join("connectors")
            .join("instances")
            .join(format!("{instance_id}.json")),
    )
    .unwrap();
    assert!(!on_disk.contains("s3cret-token"));

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn crawl_refuses_a_mutating_operation_an_unknown_system_and_an_unknown_kind() {
    let transport = Arc::new(CorpusTransport::serving(knowledge_corpus()));
    let (app, store) = app_with(transport);
    let instance_id = register_and_instantiate(&app).await;
    let uri = format!("/connectors/instances/{instance_id}/crawl");

    let (status, _) = call(
        &app,
        "POST",
        &uri,
        Some(json!({
            "operation": "create-article",
            "system": "servicenow",
            "kind": "kb_article"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, v) = call(
        &app,
        "POST",
        &uri,
        Some(json!({
            "operation": "read-kb",
            "system": "workday",
            "kind": "kb_article"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert_eq!(v["error"], json!("unknown_source_class"));

    let (status, _) = call(
        &app,
        "POST",
        &uri,
        Some(json!({
            "operation": "read-kb",
            "system": "servicenow",
            "kind": "wiki_page"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn an_unmappable_record_fails_the_crawl_and_the_call_is_still_receipted() {
    let mut corpus = knowledge_corpus();
    corpus["result"]
        .as_array_mut()
        .unwrap()
        .push(json!({"short_description": "no identity at all"}));
    let transport = Arc::new(CorpusTransport::serving(corpus));
    let (app, store) = app_with(transport);
    let instance_id = register_and_instantiate(&app).await;
    seed_demand(&app).await;

    let (status, v) = call(
        &app,
        "POST",
        &format!("/connectors/instances/{instance_id}/crawl"),
        Some(json!({
            "operation": "read-kb",
            "system": "servicenow",
            "kind": "kb_article"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert_eq!(v["error"], json!("normalization_failed"));

    // The failed crawl still left its exchange on the audit trail: the
    // call happened, and the receipt says so.
    let (status, listed) = call(
        &app,
        "GET",
        &format!("/connectors/instances/{instance_id}/receipts"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["receipts"].as_array().unwrap().len(), 1);

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn crawl_is_tenant_isolated() {
    let transport = Arc::new(CorpusTransport::serving(knowledge_corpus()));
    let store = temp_store();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_connector_transport(transport)
        .with_tenant_key("acme", "acme-secret")
        .with_tenant_key("globex", "globex-secret");
    let app = router(GraphRegistry::new(), config);

    // acme registers and instantiates; globex cannot crawl it.
    let (status, _) = call_as(
        &app,
        Some("acme-secret"),
        "POST",
        "/connectors",
        Some(manifest()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = call_as(
        &app,
        Some("acme-secret"),
        "POST",
        "/connectors/instances",
        Some(json!({
            "manifest_hash": manifest()["hash"].clone(),
            "config": {
                "instance": "dev123",
                "credentials": {"token": "s3cret-token"}
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let instance_id = body["instance_id"].as_str().unwrap();

    let (status, _) = call_as(
        &app,
        Some("globex-secret"),
        "POST",
        &format!("/connectors/instances/{instance_id}/crawl"),
        Some(json!({
            "operation": "read-kb",
            "system": "servicenow",
            "kind": "kb_article"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let _ = std::fs::remove_dir_all(store);
}
