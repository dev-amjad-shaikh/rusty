//! Connector surface integration tests: the `/connectors/*` HTTP surface
//! over the default JSON-file backend — manifest registration (content
//! addressing, declaration validation), instantiation with schema
//! validation (the 422 field-path contract) and broker-sealed secrets,
//! the check gate (pre-save and live-instance), the derived catalog,
//! restart replay, and tenant isolation (404-never-403).
//!
//! Driven in-process via `tower::ServiceExt::oneshot` (no sockets), the
//! `knowledge.rs` convention. The check-gate failure test points at an
//! unresolvable instance name, so it is network-independent: DNS failure
//! and connection refused both land in the same `failed` verdict.

use std::path::PathBuf;

use axum::body::{Body, Bytes, to_bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::connector::{
    ConnectorManifest, ConnectorOperation, HttpMethod, OperationAuth, OperationEffect,
};
use rusty_agent_server::{GraphRegistry, ServerConfig, router};
use serde_json::{Value, json};
use tower::ServiceExt;

// --------------------------------------------------------------------- //
// Harness
// --------------------------------------------------------------------- //

/// Unique temp store root, removed at the end of each test (best effort).
fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!(
        "rusty-server-connectors-test-{}",
        uuid::Uuid::new_v4()
    ))
}

/// Open-mode (single `default` tenant) app over a fresh store.
fn app() -> (Router, PathBuf) {
    let store = temp_store();
    (app_at(store.clone()), store)
}

/// Open-mode app over a given store root (restart tests build it twice).
fn app_at(store: PathBuf) -> Router {
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store);
    router(GraphRegistry::new(), config)
}

/// Two-tenant app for the isolation tests.
fn multi_tenant_app() -> (Router, PathBuf) {
    let store = temp_store();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_tenant_key("acme", "acme-secret")
        .with_tenant_key("globex", "globex-secret");
    (router(GraphRegistry::new(), config), store)
}

/// Send a request; returns `(status, json-body-or-null)`.
async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    call_as(app, None, method, uri, body).await
}

/// Send a request with an optional auth header.
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
    let response = app.clone().oneshot(builder.body(body).unwrap()).await.unwrap();
    let status = response.status();
    let bytes: Bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

/// The ServiceNow-shaped fixture manifest, built through core so the
/// hash is real.
fn demo_manifest() -> ConnectorManifest {
    let spec = json!({
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
    });
    let auth = vec![
        OperationAuth::Basic {
            username: "{credentials.username}".to_owned(),
            password: "{credentials.password}".to_owned(),
        },
        OperationAuth::Bearer {
            token: "{credentials.token}".to_owned(),
        },
    ];
    let op = |name: &str, method: HttpMethod, path: &str, effect: OperationEffect, params: Value| {
        ConnectorOperation {
            name: name.to_owned(),
            description: format!("The {name} operation."),
            method,
            path: path.to_owned(),
            effect,
            params_schema: params,
            headers: Vec::new(),
            auth: auth.clone(),
            max_response_bytes: None,
        }
    };
    ConnectorManifest::new(
        "servicenow",
        "1",
        "ServiceNow",
        "ServiceNow Table API operations.",
        "https://docs.servicenow.com/",
        "https://{instance}.service-now.com",
        spec,
        vec![
            op(
                "get-record",
                HttpMethod::Get,
                "/api/now/table/{table}/{sys_id}",
                OperationEffect::ReadOnly,
                json!({
                    "type": "object",
                    "required": ["table", "sys_id"],
                    "properties": {"table": {"type": "string"}, "sys_id": {"type": "string"}}
                }),
            ),
            op(
                "create-incident",
                HttpMethod::Post,
                "/api/now/table/incident",
                OperationEffect::Compensatable,
                json!({
                    "type": "object",
                    "required": ["short_description"],
                    "properties": {"short_description": {"type": "string"}}
                }),
            ),
            op(
                "check-connection",
                HttpMethod::Get,
                "/api/now/table/sys_user?sysparm_limit=1",
                OperationEffect::ReadOnly,
                json!({"type": "object"}),
            ),
        ],
        "check-connection",
    )
    .expect("the demo manifest validates")
}

fn basic_config(instance: &str) -> Value {
    json!({
        "instance": instance,
        "credentials": {"auth": "basic", "username": "admin", "password": "s3cret-marker"}
    })
}

/// Register the demo manifest; returns its content hash.
async fn register_demo(app: &Router, auth: Option<&str>) -> String {
    let (status, receipt) = call_as(
        app,
        auth,
        "POST",
        "/connectors",
        Some(serde_json::to_value(demo_manifest()).unwrap()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{receipt}");
    receipt["hash"].as_str().unwrap().to_owned()
}

// --------------------------------------------------------------------- //
// Manifest registration
// --------------------------------------------------------------------- //

#[tokio::test]
async fn register_manifest_validates_and_converges() {
    let (app, store) = app();
    let hash = register_demo(&app, None).await;

    // Identical re-registration converges.
    let (status, again) = call(
        &app,
        "POST",
        "/connectors",
        Some(serde_json::to_value(demo_manifest()).unwrap()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["registered"], false);
    assert_eq!(again["hash"], hash);

    // The listing serves the manifest, schema and all.
    let (status, list) = call(&app, "GET", "/connectors", None).await;
    assert_eq!(status, StatusCode::OK);
    let manifests = list["manifests"].as_array().unwrap();
    assert_eq!(manifests.len(), 1);
    assert_eq!(manifests[0]["id"], "servicenow");
    assert!(manifests[0]["connection_specification"]["properties"]["instance"].is_object());

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn register_manifest_rejects_a_tampered_hash() {
    let (app, store) = app();
    let mut body = serde_json::to_value(demo_manifest()).unwrap();
    body["display_name"] = json!("ServiceNow (edited after hashing)");
    let (status, err) = call(&app, "POST", "/connectors", Some(body)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    assert!(err["message"].as_str().unwrap().contains("hash"));

    let mut body = serde_json::to_value(demo_manifest()).unwrap();
    body["base_url"] = json!("http://{instance}.service-now.com");
    let (status, err) = call(&app, "POST", "/connectors", Some(body)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    assert!(err["message"].as_str().unwrap().contains("https"));

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Instantiation — the 422 field-path contract and secret sealing
// --------------------------------------------------------------------- //

#[tokio::test]
async fn instantiate_validates_and_names_the_failing_path() {
    let (app, store) = app();
    let hash = register_demo(&app, None).await;

    // Missing a required secret field inside the picked variant.
    let (status, err) = call(
        &app,
        "POST",
        "/connectors/instances",
        Some(json!({
            "manifest_hash": hash,
            "config": {"instance": "dev123", "credentials": {"auth": "basic", "password": "x"}}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{err}");
    assert_eq!(err["error"], "invalid_config");
    assert_eq!(
        err["message"],
        "credentials.username: required property missing"
    );

    // Unknown top-level property.
    let (status, err) = call(
        &app,
        "POST",
        "/connectors/instances",
        Some(json!({
            "manifest_hash": hash,
            "config": {
                "instance": "dev123",
                "credentials": {"auth": "basic", "username": "a", "password": "b"},
                "region": "us-east"
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{err}");
    assert_eq!(err["message"], "region: unknown property");

    // Wrong type.
    let (status, err) = call(
        &app,
        "POST",
        "/connectors/instances",
        Some(json!({
            "manifest_hash": hash,
            "config": {"instance": 42, "credentials": {"auth": "basic", "username": "a", "password": "b"}}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{err}");
    assert!(
        err["message"].as_str().unwrap().starts_with("instance: "),
        "{err}"
    );

    // Unknown manifest is a 404, never a validation error.
    let (status, _) = call(
        &app,
        "POST",
        "/connectors/instances",
        Some(json!({"manifest_hash": "0".repeat(64), "config": basic_config("dev123")})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn instantiate_seals_secrets_and_serves_them_masked() {
    let (app, store) = app();
    let hash = register_demo(&app, None).await;

    let (status, instance) = call(
        &app,
        "POST",
        "/connectors/instances",
        Some(json!({"manifest_hash": hash, "config": basic_config("dev123")})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{instance}");
    let instance_id = instance["instance_id"].as_str().unwrap().to_owned();
    assert!(instance_id.starts_with("inst-"));
    // Non-secret config renders; secrets are markers, never values.
    assert_eq!(instance["config"]["instance"], "dev123");
    assert_eq!(instance["config"]["credentials"]["auth"], "basic");
    assert_eq!(
        instance["config"]["credentials"]["username"],
        json!({"rusty_secret": true})
    );
    assert_eq!(
        instance["config"]["credentials"]["password"],
        json!({"rusty_secret": true})
    );

    // The listing serves the same masked shape.
    let (status, list) = call(&app, "GET", "/connectors/instances", None).await;
    assert_eq!(status, StatusCode::OK);
    let instances = list["instances"].as_array().unwrap();
    assert_eq!(instances.len(), 1);
    assert_eq!(instances[0]["instance_id"], instance_id);

    // The sealed plaintext is nowhere under the store: scan every byte
    // of every connector record for the marker secrets.
    let mut scanned = Vec::new();
    fn scan(dir: &std::path::Path, out: &mut Vec<Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                scan(&path, out);
            } else {
                out.push(std::fs::read(&path).unwrap());
            }
        }
    }
    scan(&store.join("connectors"), &mut scanned);
    for bytes in &scanned {
        let text = String::from_utf8_lossy(bytes);
        assert!(!text.contains("s3cret-marker"), "plaintext secret persisted");
        assert!(!text.contains("\"admin\""), "plaintext secret persisted");
    }

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// The check gate
// --------------------------------------------------------------------- //

#[tokio::test]
async fn check_gate_pre_save_and_live_instance() {
    let (app, store) = app();
    let hash = register_demo(&app, None).await;

    // Pre-save: an unresolvable instance name fails with a message.
    // Network-independent: DNS failure and refused connections are both
    // transport errors.
    let (status, outcome) = call(
        &app,
        "POST",
        "/connectors/check",
        Some(json!({"manifest_hash": hash, "config": basic_config("no-such-host-zzz")})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{outcome}");
    assert_eq!(outcome["status"], "failed");
    assert!(outcome["message"].as_str().unwrap().len() > 4);

    // Pre-save with an invalid config is the 422 contract, not a verdict.
    let (status, err) = call(
        &app,
        "POST",
        "/connectors/check",
        Some(json!({
            "manifest_hash": hash,
            "config": {"instance": "dev123", "credentials": {"auth": "basic", "password": "x"}}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{err}");
    assert_eq!(
        err["message"],
        "credentials.username: required property missing"
    );

    // A live instance checks from its stored config — sealed secrets
    // open host-side for this call only. The demo instance name does not
    // resolve, so the verdict fails with a transport message (proving
    // the sealed config rendered into a real request).
    let (status, instance) = call(
        &app,
        "POST",
        "/connectors/instances",
        Some(json!({"manifest_hash": hash, "config": basic_config("no-such-host-zzz")})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{instance}");
    let instance_id = instance["instance_id"].as_str().unwrap();
    let (status, outcome) = call(
        &app,
        "POST",
        "/connectors/check",
        Some(json!({"instance_id": instance_id})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{outcome}");
    assert_eq!(outcome["status"], "failed");
    assert!(outcome["message"].as_str().is_some());

    // Malformed bodies: both forms, and neither.
    let (status, _) = call(
        &app,
        "POST",
        "/connectors/check",
        Some(json!({"instance_id": instance_id, "manifest_hash": hash, "config": basic_config("x")})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = call(&app, "POST", "/connectors/check", Some(json!({}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Catalog
// --------------------------------------------------------------------- //

#[tokio::test]
async fn catalog_derives_one_tool_per_operation() {
    let (app, store) = app();
    let hash = register_demo(&app, None).await;
    let (status, instance) = call(
        &app,
        "POST",
        "/connectors/instances",
        Some(json!({"manifest_hash": hash, "config": basic_config("dev123")})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{instance}");
    let instance_id = instance["instance_id"].as_str().unwrap();

    let (status, catalog) = call(
        &app,
        "GET",
        &format!("/connectors/instances/{instance_id}/catalog"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{catalog}");
    let tools = catalog["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        vec![
            "servicenow/check-connection",
            "servicenow/create-incident",
            "servicenow/get-record"
        ]
    );
    assert_eq!(catalog["manifest_hash"], hash);

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Restart replay and tenant isolation
// --------------------------------------------------------------------- //

#[tokio::test]
async fn restart_replays_manifests_and_instances_byte_exact() {
    let store = temp_store();
    let app = app_at(store.clone());
    let hash = register_demo(&app, None).await;
    let (status, instance) = call(
        &app,
        "POST",
        "/connectors/instances",
        Some(json!({"manifest_hash": hash, "config": basic_config("dev123")})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{instance}");
    let instance_id = instance["instance_id"].as_str().unwrap().to_owned();

    // Restart: a fresh app over the same store.
    let app = app_at(store.clone());
    let (status, list) = call(&app, "GET", "/connectors", None).await;
    assert_eq!(status, StatusCode::OK);
    let manifests = list["manifests"].as_array().unwrap();
    assert_eq!(manifests.len(), 1);
    assert_eq!(manifests[0]["hash"], hash);
    assert_eq!(
        serde_json::to_value(demo_manifest()).unwrap(),
        manifests[0].clone(),
        "the manifest replays byte-exactly"
    );

    let (status, list) = call(&app, "GET", "/connectors/instances", None).await;
    assert_eq!(status, StatusCode::OK);
    let instances = list["instances"].as_array().unwrap();
    assert_eq!(instances.len(), 1);
    assert_eq!(
        serde_json::to_string(&instances[0]).unwrap(),
        serde_json::to_string(&instance).unwrap(),
        "the served instance replays byte-exactly"
    );

    // The catalog still derives after the restart.
    let (status, catalog) = call(
        &app,
        "GET",
        &format!("/connectors/instances/{instance_id}/catalog"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{catalog}");
    assert_eq!(catalog["tools"].as_array().unwrap().len(), 3);

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn tenant_isolation_404_never_403() {
    let (app, store) = multi_tenant_app();
    let hash = register_demo(&app, Some("acme-secret")).await;
    let (status, instance) = call_as(
        &app,
        Some("acme-secret"),
        "POST",
        "/connectors/instances",
        Some(json!({"manifest_hash": hash, "config": basic_config("dev123")})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{instance}");
    let instance_id = instance["instance_id"].as_str().unwrap();

    // Globex sees no manifests, cannot instantiate against acme's hash,
    // and cannot read acme's instance catalog.
    let (status, list) = call_as(&app, Some("globex-secret"), "GET", "/connectors", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["manifests"].as_array().unwrap().len(), 0);

    let (status, _) = call_as(
        &app,
        Some("globex-secret"),
        "POST",
        "/connectors/instances",
        Some(json!({"manifest_hash": hash, "config": basic_config("dev123")})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, list) =
        call_as(&app, Some("globex-secret"), "GET", "/connectors/instances", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["instances"].as_array().unwrap().len(), 0);

    let (status, _) = call_as(
        &app,
        Some("globex-secret"),
        "GET",
        &format!("/connectors/instances/{instance_id}/catalog"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call_as(
        &app,
        Some("globex-secret"),
        "POST",
        "/connectors/check",
        Some(json!({"instance_id": instance_id})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let _ = std::fs::remove_dir_all(store);
}
