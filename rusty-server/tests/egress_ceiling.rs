//! R06 — egress as a ceiling the operator owns. A connection to a host the
//! ceiling does not admit is refused at creation, naming the host; an
//! administrator allows it (exactly, or as a wildcard) and the same request
//! succeeds; the ceiling cannot be narrowed below a connection in use; a
//! kept ceiling outlives a restart and beats the environment's list; an
//! unbounded deployment says it is open and closes to the hosts in use.
use std::path::PathBuf;

use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::http::{Request, StatusCode};
use rusty_agent_runtime::connector::{
    ConnectorManifest, ConnectorOperation, HttpMethod, OperationAuth, OperationEffect,
};
use rusty_agent_runtime::egress::{
    EgressEndpoint, EgressEndpointPolicy, EgressPolicy, EgressProtocol, EgressRewrite, EgressRule,
    EgressRuleMode,
};
use rusty_agent_server::{GraphRegistry, ServerConfig, router};
use serde_json::{Value, json};
use tower::ServiceExt;

fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!(
        "rusty-server-egress-ceiling-test-{}",
        uuid::Uuid::new_v4()
    ))
}

fn allow(hosts: &[&str]) -> EgressPolicy {
    EgressPolicy {
        policies: hosts
            .iter()
            .map(|host| EgressEndpointPolicy {
                name: format!("operator:{host}"),
                endpoint: EgressEndpoint {
                    host: (*host).to_owned(),
                    port: 443,
                    protocol: EgressProtocol::Rest,
                    tls: true,
                    rewrite: EgressRewrite::default(),
                    allowed_ips: vec![],
                    allow_encoded_slashes: false,
                },
                rules: vec![EgressRule {
                    methods: vec![],
                    path_pattern: "/**".into(),
                    mode: EgressRuleMode::Enforce,
                    tool_names: None,
                }],
                originating: vec![],
            })
            .collect(),
    }
}

/// Open-mode app over `store`, booted with the environment's allow-list
/// (`None` = no `RUSTY_EGRESS_ALLOW`).
fn app_at(store: &std::path::Path, env: Option<&[&str]>) -> Router {
    let mut config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.to_path_buf());
    if let Some(hosts) = env {
        config = config.with_egress_policy(allow(hosts));
    }
    router(GraphRegistry::new(), config)
}

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
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

/// The ceiling once boot has kept it (the seed runs right after the router
/// is built; a test that edits it must not race the seed).
async fn settled(app: &Router) -> Value {
    for _ in 0..200 {
        let (status, ceiling) = call(app, "GET", "/egress/ceiling", None).await;
        assert_eq!(status, StatusCode::OK);
        if !ceiling["updated_at"].is_null() {
            return ceiling;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("the egress ceiling was never kept at boot");
}

fn demo_manifest() -> ConnectorManifest {
    let spec = json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "required": ["instance", "credentials"],
        "additionalProperties": false,
        "properties": {
            "instance": {"type": "string", "pattern": "^[a-z0-9-]+$", "rusty_order": 0},
            "credentials": {
                "type": "object", "rusty_order": 1, "required": ["auth", "username", "password"], "additionalProperties": false,
                "properties": {
                    "auth": {"type": "string", "const": "basic"},
                    "username": {"type": "string", "rusty_secret": true},
                    "password": {"type": "string", "rusty_secret": true}
                }
            }
        }
    });
    let auth = vec![OperationAuth::Basic {
        username: "{credentials.username}".to_owned(),
        password: "{credentials.password}".to_owned(),
    }];
    ConnectorManifest::new(
        "servicenow",
        "1",
        "ServiceNow",
        "ServiceNow Table API operations.",
        "https://docs.servicenow.com/",
        "https://{instance}.service-now.com",
        spec,
        vec![ConnectorOperation {
            name: "check-connection".to_owned(),
            description: "One row of sys_user.".to_owned(),
            method: HttpMethod::Get,
            path: "/api/now/table/sys_user?sysparm_limit=1".to_owned(),
            effect: OperationEffect::ReadOnly,
            params_schema: json!({"type": "object"}),
            headers: Vec::new(),
            auth,
            max_response_bytes: None,
            reconcile: None,
        }],
        "check-connection",
    )
    .expect("the demo manifest validates")
}

fn basic_config(instance: &str) -> Value {
    json!({"instance": instance, "credentials": {"auth": "basic", "username": "admin", "password": "s3cret-marker"}})
}

async fn register_demo(app: &Router) -> String {
    let (status, receipt) = call(
        app,
        "POST",
        "/connectors",
        Some(serde_json::to_value(demo_manifest()).unwrap()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{receipt}");
    receipt["hash"].as_str().unwrap().to_owned()
}

async fn connect(app: &Router, hash: &str, instance: &str) -> (StatusCode, Value) {
    call(
        app,
        "POST",
        "/connectors/instances",
        Some(json!({"manifest_hash": hash, "config": basic_config(instance)})),
    )
    .await
}

fn hosts_of(ceiling: &Value) -> Vec<&str> {
    ceiling["hosts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["host"].as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn a_connection_outside_the_ceiling_is_refused_until_an_administrator_allows_its_host() {
    let store = temp_store();
    let app = app_at(&store, Some(&["docs.example.com"]));
    let booted = settled(&app).await;
    assert_eq!(
        booted["open"], false,
        "an allow-list boots the ceiling closed"
    );
    assert_eq!(hosts_of(&booted), vec!["docs.example.com"]);
    assert_eq!(booted["hosts"][0]["added_by"]["kind"], "environment");
    let hash = register_demo(&app).await;

    let (status, refusal) = connect(&app, &hash, "dev12345").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refusal}");
    assert_eq!(refusal["error"], "egress_outside_ceiling");
    assert_eq!(
        refusal["host"], "dev12345.service-now.com",
        "the refusal names the host to allow"
    );
    // The pre-save check says the same, before it would fail on egress.
    let (status, refusal) = call(
        &app,
        "POST",
        "/connectors/check",
        Some(json!({"manifest_hash": hash, "config": basic_config("dev12345")})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refusal}");
    assert_eq!(refusal["error"], "egress_outside_ceiling");
    assert_eq!(refusal["host"], "dev12345.service-now.com");
    assert!(
        refusal["message"]
            .as_str()
            .unwrap()
            .contains("Sites agents may reach"),
        "{refusal}"
    );

    // The administrator allows every instance of the system at once.
    let (status, ceiling) = call(
        &app,
        "PUT",
        "/egress/ceiling",
        Some(json!({"open": false, "hosts": [{"host": "docs.example.com"}, {"host": " *.Service-Now.com ", "note": "our ServiceNow instances"}]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{ceiling}");
    assert_eq!(
        hosts_of(&ceiling),
        vec!["docs.example.com", "*.service-now.com"]
    );
    assert_eq!(
        ceiling["hosts"][0]["added_by"]["kind"], "environment",
        "a kept host keeps who allowed it"
    );
    assert_ne!(
        ceiling["hosts"][1]["added_by"]["kind"], "environment",
        "a new host is stamped with the administrator"
    );
    assert_eq!(ceiling["hosts"][1]["note"], "our ServiceNow instances");
    assert!(!ceiling["updated_by"].is_null());

    let (status, instance) = connect(&app, &hash, "dev12345").await;
    assert_eq!(status, StatusCode::CREATED, "{instance}");
    let (_, ceiling) = call(&app, "GET", "/egress/ceiling", None).await;
    assert_eq!(
        ceiling["hosts"][1]["used_by"][0]["name"], "ServiceNow",
        "the wildcard shows the connections under it"
    );
    assert_eq!(ceiling["unlisted"].as_array().unwrap().len(), 0);

    // Not below a connection in use: the ceiling names the connection.
    let (status, refusal) = call(
        &app,
        "PUT",
        "/egress/ceiling",
        Some(json!({"open": false, "hosts": [{"host": "docs.example.com"}]})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refusal}");
    assert_eq!(refusal["error"], "ceiling_below_connections");
    assert_eq!(refusal["host"], "dev12345.service-now.com");
    assert_eq!(refusal["connection"], "ServiceNow");
    let (_, ceiling) = call(&app, "GET", "/egress/ceiling", None).await;
    assert_eq!(
        hosts_of(&ceiling).len(),
        2,
        "a refused edit changes nothing"
    );

    // A host is typed as a host name.
    let (status, refusal) = call(
        &app,
        "PUT",
        "/egress/ceiling",
        Some(json!({"open": false, "hosts": [{"host": "https://docs.example.com/x"}]})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(refusal["error"], "invalid_host");

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn a_kept_ceiling_outlives_a_restart_and_beats_the_environment() {
    let store = temp_store();
    let app = app_at(&store, Some(&["docs.example.com"]));
    settled(&app).await;
    let (status, _) = call(&app, "PUT", "/egress/ceiling", Some(json!({"open": false, "hosts": [{"host": "docs.example.com"}, {"host": "api.example.com"}]}))).await;
    assert_eq!(status, StatusCode::OK);
    drop(app);

    // Restart with a different environment list: the kept ceiling wins.
    let app = app_at(&store, Some(&["other.example.com"]));
    let ceiling = settled(&app).await;
    assert_eq!(
        hosts_of(&ceiling),
        vec!["docs.example.com", "api.example.com"]
    );
    assert!(!ceiling["updated_by"].is_null());

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn an_open_deployment_says_so_and_closes_to_the_hosts_in_use() {
    let store = temp_store();
    let app = app_at(&store, None);
    let booted = settled(&app).await;
    assert_eq!(
        booted["open"], true,
        "no allow-list: the deployment is open, and says so"
    );
    let hash = register_demo(&app).await;
    let (status, _) = connect(&app, &hash, "dev1").await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "open admits any host a connection names"
    );

    let (_, ceiling) = call(&app, "GET", "/egress/ceiling", None).await;
    assert_eq!(
        ceiling["unlisted"][0]["host"], "dev1.service-now.com",
        "what is in use but not listed is shown"
    );
    assert_eq!(
        ceiling["unlisted"][0]["connections"][0]["name"],
        "ServiceNow"
    );

    // Close to the hosts in use.
    let (status, ceiling) = call(
        &app,
        "PUT",
        "/egress/ceiling",
        Some(json!({"open": false, "hosts": [{"host": "dev1.service-now.com"}]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{ceiling}");
    assert_eq!(ceiling["open"], false);
    let (status, refusal) = connect(&app, &hash, "dev2").await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "closed: another host is refused — {refusal}"
    );
    assert_eq!(refusal["host"], "dev2.service-now.com");

    // Closing to nothing while a connection is in use is refused.
    let (status, refusal) = call(
        &app,
        "PUT",
        "/egress/ceiling",
        Some(json!({"open": false, "hosts": []})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refusal}");
    assert_eq!(refusal["connection"], "ServiceNow");

    let _ = std::fs::remove_dir_all(store);
}
