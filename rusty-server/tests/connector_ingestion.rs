//! Connector ingestion integration tests (EP-07-S05): a fixture
//! connector driven over a synthetic ServiceNow-class corpus containing
//! every channel and every failure shape, asserting row counts per
//! `ResolutionPath`/`InteractionOutcome`, receipt presence on every
//! connector call, idempotency across a repeated window, the
//! all-or-nothing normalization rule, and tenant isolation.
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
// Harness
// --------------------------------------------------------------------- //

fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!("rusty-server-ingest-test-{}", uuid::Uuid::new_v4()))
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

fn multi_tenant_app_with(transport: Arc<CorpusTransport>) -> (Router, PathBuf) {
    let store = temp_store();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_connector_transport(transport)
        .with_tenant_key("acme", "acme-secret")
        .with_tenant_key("globex", "globex-secret");
    (router(GraphRegistry::new(), config), store)
}

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    call_as(app, None, method, uri, body).await
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

fn corpus_record(stream: &str, sys_id: &str, extra: Value) -> Value {
    let mut record = json!({
        "stream": stream,
        "sys_id": sys_id,
        "caller_id": "u-1",
        "occurred_at": "2026-01-05T12:00:00Z",
    });
    record
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    record
}

/// The synthetic corpus: all six channels, with the failure shapes the
/// demand schema must preserve — a zero-result search, a no-click
/// search, an abandoned chat, a cancelled request, a reopened incident,
/// a multi-reassignment incident, an escalated case, an escalation.
fn corpus() -> Value {
    json!({ "result": [
        corpus_record("sp_log", "SP-1", json!({"query": "expense policy", "result_count": 0, "clicked": false})),
        corpus_record("sp_log", "SP-2", json!({"query": "parental leave", "result_count": 6, "clicked": false})),
        corpus_record("sp_log", "SP-3", json!({"query": "vpn setup", "result_count": 3, "clicked": true})),
        corpus_record("sys_cs_conversation", "CH-1", json!({"question": "reset my password", "state": "deflected"})),
        corpus_record("sys_cs_conversation", "CH-2", json!({"question": "where is my invoice", "state": "abandoned"})),
        corpus_record("sc_request", "REQ-1", json!({"short_description": "new laptop", "state": "closed_complete"})),
        corpus_record("sc_request", "REQ-2", json!({"short_description": "office move", "state": "cancelled"})),
        corpus_record("incident", "INC-1", json!({"short_description": "vpn is down", "state": "resolved"})),
        corpus_record("incident", "INC-2", json!({"short_description": "vpn down again", "state": "resolved", "reopen_count": 2})),
        corpus_record("incident", "INC-3", json!({"short_description": "bounced between queues", "state": "closed", "reassignment_count": 3})),
        corpus_record("sn_customerservice_case", "CS-1", json!({"short_description": "billing question", "state": "closed"})),
        corpus_record("sn_customerservice_case", "CS-2", json!({"short_description": "complaint", "state": "escalated"})),
        corpus_record("sys_escalation", "ESC-1", json!({"reason": "tier 2 needed", "state": "resolved"})),
    ]})
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
                    name: "read-corpus".to_owned(),
                    description: "Read the interaction corpus.".to_owned(),
                    method: HttpMethod::Get,
                    path: "/api/now/corpus/{window}".to_owned(),
                    effect: OperationEffect::ReadOnly,
                    params_schema: json!({
                        "type": "object",
                        "required": ["window"],
                        "properties": {"window": {"type": "string"}}
                    }),
                    headers: Vec::new(),
                    auth: vec![OperationAuth::Bearer {
                        token: "{credentials.token}".to_owned(),
                    }],
                    max_response_bytes: None,
                },
                ConnectorOperation {
                    name: "create-incident".to_owned(),
                    description: "Create an incident.".to_owned(),
                    method: HttpMethod::Post,
                    path: "/api/now/table/incident".to_owned(),
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

// --------------------------------------------------------------------- //
// Tests
// --------------------------------------------------------------------- //

#[tokio::test]
async fn ingestion_files_every_failure_shape_and_receipts_every_call() {
    let transport = Arc::new(CorpusTransport::serving(corpus()));
    let (app, store) = app_with(transport.clone());
    let instance_id = register_and_instantiate(&app).await;

    let (status, body) = call(
        &app,
        "POST",
        &format!("/connectors/instances/{instance_id}/ingest"),
        Some(json!({
            "operation": "read-corpus",
            "system": "servicenow",
            "params": {"window": "2026-W01"}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ingested"], json!(13));
    assert_eq!(body["duplicates"], json!(0));

    // Row counts per outcome: the failure classes landed as themselves.
    let by_outcome = &body["by_outcome"];
    assert_eq!(by_outcome["no_result"], json!(1));
    assert_eq!(by_outcome["no_click"], json!(1));
    assert_eq!(by_outcome["reopened"], json!(1));
    assert_eq!(
        by_outcome["escalated"],
        json!(5),
        "abandoned chat, cancelled request, multi-reassignment, escalated case, escalation: {by_outcome}"
    );
    assert_eq!(by_outcome["resolved"], json!(5));
    let by_path = &body["by_resolution_path"];
    assert_eq!(by_path["unresolved"], json!(3));
    assert_eq!(by_path["self_service"], json!(1));
    assert_eq!(by_path["deflected"], json!(1));
    assert_eq!(by_path["abandoned"], json!(1));
    // The reopened incident resolved through a human, so `reopened` the
    // outcome still counts under `human_resolved` the path.
    assert_eq!(by_path["human_resolved"], json!(7));

    // One connector call, one receipt — referenced by the answer and
    // readable back from the audit route.
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
    assert_eq!(listed[0]["operation"], json!("read-corpus"));
    assert_eq!(listed[0]["status"], json!(200));

    // The call went out with the opened secret in the auth header and
    // the call argument rendered into the path — and the persisted
    // instance record holds no plaintext credential.
    let seen = transport.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert!(seen[0].url.contains("/api/now/corpus/2026-W01"));
    assert!(
        seen[0]
            .headers
            .iter()
            .any(|(name, value)| name == "Authorization" && value == "Bearer s3cret-token")
    );
    drop(seen);
    let stored = std::fs::read_to_string(
        store
            .join("connectors")
            .join("instances")
            .join(format!("{instance_id}.json")),
    )
    .expect("the instance record exists");
    assert!(!stored.contains("s3cret-token"), "{stored}");

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn a_repeated_window_converges_with_zero_new_rows() {
    let transport = Arc::new(CorpusTransport::serving(corpus()));
    let (app, store) = app_with(transport);
    let instance_id = register_and_instantiate(&app).await;

    for (expected_created, expected_dupes) in [(13, 0), (0, 13)] {
        let (status, body) = call(
            &app,
            "POST",
            &format!("/connectors/instances/{instance_id}/ingest"),
            Some(json!({
                "operation": "read-corpus",
                "system": "servicenow",
                "params": {"window": "2026-W01"}
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["ingested"], json!(expected_created));
        assert_eq!(body["duplicates"], json!(expected_dupes));
    }
    // Two windows, two calls, two receipts.
    let (_, listed) = call(
        &app,
        "GET",
        &format!("/connectors/instances/{instance_id}/receipts"),
        None,
    )
    .await;
    assert_eq!(listed["receipts"].as_array().unwrap().len(), 2);

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn an_unmappable_record_fails_the_batch_and_files_nothing() {
    let mut bad = corpus();
    bad["result"].as_array_mut().unwrap().push(corpus_record(
        "incident",
        "INC-BAD",
        json!({"short_description": "mystery", "state": "awaiting_vendor"}),
    ));
    let transport = Arc::new(CorpusTransport::serving(bad));
    let (app, store) = app_with(transport);
    let instance_id = register_and_instantiate(&app).await;

    let (status, body) = call(
        &app,
        "POST",
        &format!("/connectors/instances/{instance_id}/ingest"),
        Some(json!({
            "operation": "read-corpus",
            "system": "servicenow",
            "params": {"window": "2026-W02"}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"], json!("normalization_failed"));
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("awaiting_vendor")
    );

    // Nothing filed: a subsequent good window creates every row.
    let (app2, _) = {
        // Same store, same app — the corpus transport now serves good data.
        (app, ())
    };
    let good_transport = Arc::new(CorpusTransport::serving(corpus()));
    let (app_good, store_good) = app_with(good_transport);
    // Prove all-or-nothing on the original store: re-ingest there with a
    // good corpus by registering a second app against the same store.
    drop(store_good);
    let transport = Arc::new(CorpusTransport::serving(corpus()));
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_connector_transport(transport);
    let app_same_store = router(GraphRegistry::new(), config);
    let _ = app2;
    let _ = app_good;
    let (status, body) = call(
        &app_same_store,
        "POST",
        &format!("/connectors/instances/{instance_id}/ingest"),
        Some(json!({
            "operation": "read-corpus",
            "system": "servicenow",
            "params": {"window": "2026-W02"}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["ingested"],
        json!(13),
        "the failed batch filed nothing, so every row is new"
    );

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn ingestion_refuses_a_mutating_operation_and_an_unknown_source_class() {
    let transport = Arc::new(CorpusTransport::serving(corpus()));
    let (app, store) = app_with(transport);
    let instance_id = register_and_instantiate(&app).await;

    // A write operation is not an ingestion path.
    let (status, body) = call(
        &app,
        "POST",
        &format!("/connectors/instances/{instance_id}/ingest"),
        Some(json!({
            "operation": "create-incident",
            "system": "servicenow"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body["message"].as_str().unwrap().contains("effect"));

    // A source class with no mapping is a typed refusal.
    let (status, body) = call(
        &app,
        "POST",
        &format!("/connectors/instances/{instance_id}/ingest"),
        Some(json!({
            "operation": "read-corpus",
            "system": "zendesk"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"], json!("unknown_source_class"));

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn ingestion_is_tenant_isolated() {
    let transport = Arc::new(CorpusTransport::serving(corpus()));
    let (app, store) = multi_tenant_app_with(transport);

    // Register + instantiate as acme.
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
    let instance_id = body["instance_id"].as_str().unwrap().to_owned();

    // Globex sees nothing: the ingest route answers 404, never 403.
    let (status, _) = call_as(
        &app,
        Some("globex-secret"),
        "POST",
        &format!("/connectors/instances/{instance_id}/ingest"),
        Some(json!({
            "operation": "read-corpus",
            "system": "servicenow"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Acme ingests; globex's receipt and ledger views stay empty.
    let (status, body) = call_as(
        &app,
        Some("acme-secret"),
        "POST",
        &format!("/connectors/instances/{instance_id}/ingest"),
        Some(json!({
            "operation": "read-corpus",
            "system": "servicenow",
            "params": {"window": "2026-W01"}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ingested"], json!(13));

    let (status, listed) = call_as(
        &app,
        Some("globex-secret"),
        "GET",
        &format!("/connectors/instances/{instance_id}/receipts"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{listed}");

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn an_untrusted_corpus_files_its_filings_as_untrusted_derived() {
    let transport = Arc::new(CorpusTransport::serving(corpus()));
    let (app, store) = app_with(transport);
    let instance_id = register_and_instantiate(&app).await;

    // Ingest the corpus marked as third-party content.
    let (status, body) = call(
        &app,
        "POST",
        &format!("/connectors/instances/{instance_id}/ingest"),
        Some(json!({
            "operation": "read-corpus",
            "system": "servicenow",
            "params": {"window": "2026-W01"},
            "origin_class": "untrusted"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ingested"], json!(13));
    let event_id = body["event_ids"][0].as_str().unwrap().to_owned();

    // A runtime filing citing one of those events lands as
    // untrusted_derived, whatever surface it came in through.
    let (status, filed) = call(
        &app,
        "POST",
        "/gaps/file/escalation",
        Some(json!({
            "event_id": event_id,
            "statement": "vendor feed escalated this",
            "closure_criteria": {"block_filled": {"block_label": "vendor guidance"}}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{filed}");
    let gap_id = filed["gap_id"].as_str().unwrap();

    // The governance read: the origin filter isolates exactly the
    // untrusted-derived entries.
    let (status, work) = call(&app, "GET", "/gaps?origin=untrusted_derived", None).await;
    assert_eq!(status, StatusCode::OK);
    let rows = work["work_order"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{work}");
    assert_eq!(rows[0]["gap_id"], json!(gap_id));
    assert_eq!(rows[0]["origin"], json!("untrusted_derived"));

    let (status, work) = call(&app, "GET", "/gaps?origin=runtime_escalation", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(work["work_order"].as_array().unwrap().is_empty());

    // A typo'd filter is a typed 400 — never a silent empty list reading
    // as "no untrusted-derived gaps".
    let (status, _) = call(&app, "GET", "/gaps?origin=untrusted", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let _ = std::fs::remove_dir_all(store);
}
