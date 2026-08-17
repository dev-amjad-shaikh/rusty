//! The connector plane integration tests: the `/connectors` surface over
//! the default JSON-file backend — manifest publishing (validation,
//! hash-pinning, idempotence), instantiation with the vault credential
//! bridge (slot → connection bindings, denials naming slots never
//! material), the connect/catalog/health lifecycle with generation pins,
//! disable/enable guards, the plane-wide sweep, tenant
//! indistinguishability, restart durability, and the redaction audit
//! (no answer ever carries credential material).
//!
//! Driven in-process via `tower::ServiceExt::oneshot` (no sockets), the
//! `broker.rs` convention. Live MCP sessions are proven against a
//! scripted fake server (`/bin/sh` answering newline-delimited JSON-RPC);
//! the reqwest transport's own loopback coverage is in the module's unit
//! tests (`src/connectors.rs`).

use std::path::PathBuf;

use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::http::{Request, StatusCode};
use rusty_agent_server::{GraphRegistry, ServerConfig, router};
use serde_json::{Value, json};
use tower::ServiceExt;

/// The plaintext credential the vault-bridge tests register. Distinctive,
/// so a scan for it is a scan for a leak: it may appear in the request
/// body that carried it, and nowhere the server serves (the `broker.rs`
/// marker rule).
const MARKER: &str = "sk-live-MARKER-9f2e";

/// The scripted fake MCP server: newline-delimited JSON-RPC answering
/// `initialize` and `tools/list` (one `echo` tool). Notification lines
/// (`notifications/initialized`) carry no id and are skipped; anything
/// else unanswered is ignored, so the loop also tolerates pings. One
/// line, control-free, and under the manifest's 1024-byte argument cap —
/// the manifest validation rules.
const FAKE_MCP_SCRIPT: &str = r#"while IFS= read -r l; do id=$(printf '%s' "$l" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p'); [ -z "$id" ] && continue; case "$l" in *tools/list*) printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"tools\":[{\"name\":\"echo\",\"description\":\"Echoes text.\",\"inputSchema\":{\"type\":\"object\"}}]}}" ;; *initialize*) printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"protocolVersion\":\"2025-03-26\",\"capabilities\":{},\"serverInfo\":{\"name\":\"fake\",\"version\":\"0.1\"}}}" ;; esac; done"#;

// --------------------------------------------------------------------- //
// Harness (the broker.rs shapes)
// --------------------------------------------------------------------- //

/// Unique temp store root, removed at the end of each test (best effort).
fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!(
        "rusty-server-connectors-test-{}",
        uuid::Uuid::new_v4()
    ))
}

/// An app over `store` with the config customized by `configure`. No
/// graphs: the connector surface stands on its own.
fn app_with(store: PathBuf, configure: impl FnOnce(ServerConfig) -> ServerConfig) -> Router {
    let config = configure(ServerConfig::new("127.0.0.1:0".parse().unwrap(), store));
    router(GraphRegistry::new(), config)
}

/// Open-mode (single `default` tenant) app over a fresh store.
fn app() -> (Router, PathBuf) {
    let store = temp_store();
    (app_with(store.clone(), |config| config), store)
}

/// A two-tenant app: acme and globex, each with its own key.
fn two_tenant_app() -> (Router, PathBuf) {
    let store = temp_store();
    let app = app_with(store.clone(), |config| {
        config
            .with_tenant_key("acme", "acme-key")
            .with_tenant_key("globex", "globex-key")
    });
    (app, store)
}

/// Send a request; returns `(status, json-body-or-null)`.
async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    call_as(app, None, method, uri, body).await
}

/// Send a request with an optional auth header.
async fn call_as(
    app: &Router,
    auth: Option<(&str, &str)>,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some((k, v)) = auth {
        builder = builder.header(k, v);
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

// --------------------------------------------------------------------- //
// Manifests and fixtures
// --------------------------------------------------------------------- //

/// The MCP manifest over the scripted fake server. `env_allowlist`
/// carries `PATH` because the child starts with a scrubbed environment
/// and the script's `sed` must resolve.
fn mcp_manifest() -> Value {
    json!({
        "id": "test-conn",
        "version": "1.0.0",
        "display_name": "Test Connector",
        "description": "A scripted MCP stdio connector for tests.",
        "provider": {
            "kind": "mcp_stdio",
            "command": "/bin/sh",
            "args": ["-c", FAKE_MCP_SCRIPT],
            "env_allowlist": ["PATH"],
        },
        "capabilities": ["mcp tools"],
        "credential_slots": [],
    })
}

/// The bounded HTTP search manifest with one `api_key` credential slot.
/// The endpoint is never called — the search catalog is declarative and
/// connect performs no network I/O.
fn search_manifest() -> Value {
    json!({
        "id": "web-search",
        "version": "1.0.0",
        "display_name": "Web Search",
        "description": "A bounded HTTP search connector.",
        "provider": {
            "kind": "http_search",
            "base_url": "https://search.example.test/query",
            "auth": {"header": "x-search-key", "credential_slot": "api_key"},
        },
        "capabilities": ["web search"],
        "credential_slots": [{"name": "api_key", "description": "The search API key."}],
    })
}

/// Publish a manifest; returns the receipt.
async fn publish(app: &Router, auth: Option<(&str, &str)>, manifest: Value) -> Value {
    let (status, v) = call_as(app, auth, "POST", "/connectors/manifests", Some(manifest)).await;
    assert_eq!(status, StatusCode::CREATED, "publish failed: {v}");
    v["receipt"].clone()
}

/// Register one connection holding the marker credential; returns its id.
async fn register_connection(app: &Router, auth: Option<(&str, &str)>) -> String {
    let (status, v) = call_as(
        app,
        auth,
        "POST",
        "/connections",
        Some(json!({
            "provider": "oauth2_authorization_code",
            "subject": "user-7",
            "scopes": ["search"],
            "token": {"access_token": MARKER},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "register failed: {v}");
    // The registration answer is metadata only — never the material.
    assert!(!v.to_string().contains(MARKER));
    v["connection"]["connection_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// Instantiate a manifest; returns `(status, body)`.
async fn instantiate(
    app: &Router,
    auth: Option<(&str, &str)>,
    manifest_hash: &str,
    credentials: Value,
) -> (StatusCode, Value) {
    call_as(
        app,
        auth,
        "POST",
        "/connectors/instances",
        Some(json!({
            "manifest_hash": manifest_hash,
            "credentials": credentials,
        })),
    )
    .await
}

/// Publish the MCP manifest and instantiate it (no credential slots);
/// returns the instance id.
async fn mcp_instance(app: &Router) -> String {
    let receipt = publish(app, None, mcp_manifest()).await;
    let (status, v) = instantiate(
        app,
        None,
        receipt["manifest_hash"].as_str().unwrap(),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "instantiate failed: {v}");
    v["instance"]["instance_id"].as_str().unwrap().to_owned()
}

/// Connect an instance; asserts the healthy landing and returns the body.
async fn connect_healthy(app: &Router, instance_id: &str) -> Value {
    let (status, v) = call(
        app,
        "POST",
        &format!("/connectors/instances/{instance_id}/connect"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "connect failed: {v}");
    assert_eq!(v["instance"]["state"], "healthy");
    v
}

// --------------------------------------------------------------------- //
// Manifests
// --------------------------------------------------------------------- //

#[tokio::test]
async fn manifests_publish_validate_and_converge() {
    let (app, store) = app();

    let receipt = publish(&app, None, mcp_manifest()).await;
    assert_eq!(receipt["id"], "test-conn");
    assert_eq!(receipt["version"], "1.0.0");
    assert_eq!(receipt["already_registered"], false);
    let hash = receipt["manifest_hash"].as_str().unwrap().to_owned();
    assert!(!hash.is_empty());

    // Idempotent by content: the same bytes converge.
    let again = publish(&app, None, mcp_manifest()).await;
    assert_eq!(again["already_registered"], true);
    assert_eq!(again["manifest_hash"], hash);

    let (status, v) = call(&app, "GET", "/connectors/manifests", None).await;
    assert_eq!(status, StatusCode::OK, "list failed: {v}");
    let manifests = v["manifests"].as_array().unwrap();
    assert_eq!(manifests.len(), 1);
    assert_eq!(manifests[0]["id"], "test-conn");
    assert_eq!(manifests[0]["hash"], hash);

    // Validation refuses a malformed connector id.
    let mut bad = mcp_manifest();
    bad["id"] = json!("Bad ID!!");
    let (status, v) = call(&app, "POST", "/connectors/manifests", Some(bad)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "body: {v}");

    // A declared hash that disagrees with the content is a 422, not an
    // override.
    let mut pinned = mcp_manifest();
    pinned["id"] = json!("other-conn");
    pinned["hash"] = json!("bogus");
    let (status, v) = call(&app, "POST", "/connectors/manifests", Some(pinned)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "body: {v}");
    assert!(v["message"].as_str().unwrap().contains("does not match"));

    // Search endpoints are https-only at declaration.
    let mut plaintext = search_manifest();
    plaintext["provider"]["base_url"] = json!("http://search.example.test/query");
    let (status, v) = call(&app, "POST", "/connectors/manifests", Some(plaintext)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "body: {v}");

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Instantiation and the vault bridge
// --------------------------------------------------------------------- //

#[tokio::test]
async fn instantiation_bridges_credential_slots_through_the_vault() {
    let (app, store) = app();
    let receipt = publish(&app, None, search_manifest()).await;
    let hash = receipt["manifest_hash"].as_str().unwrap().to_owned();
    let connection_id = register_connection(&app, None).await;

    // An unbound declared slot is a 422 naming the slot — never a silent
    // default.
    let (status, v) = instantiate(&app, None, &hash, json!({})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "body: {v}");
    assert!(v["message"].as_str().unwrap().contains("api_key"));

    // An unknown manifest hash is a 404.
    let (status, v) = instantiate(&app, None, &"ab".repeat(32), json!({})).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "body: {v}");

    // A slot bound to a connection the vault refuses is a 422 naming the
    // slot and the connection, never the material.
    let (status, v) = instantiate(&app, None, &hash, json!({"api_key": "conn-missing"})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "body: {v}");
    let message = v["message"].as_str().unwrap();
    assert!(message.contains("api_key"), "message: {message}");
    assert!(message.contains("conn-missing"), "message: {message}");

    // The happy path: slot → connection binding, instance pending.
    let (status, v) = instantiate(&app, None, &hash, json!({"api_key": connection_id})).await;
    assert_eq!(status, StatusCode::CREATED, "instantiate failed: {v}");
    let instance = &v["instance"];
    assert_eq!(instance["state"], "pending");
    assert_eq!(instance["connector_id"], "web-search");
    assert_eq!(instance["credential_slots"], json!(["api_key"]));
    assert!(instance["catalog_generation"].is_null());

    let (status, v) = call(&app, "GET", "/connectors/instances", None).await;
    assert_eq!(status, StatusCode::OK, "list failed: {v}");
    assert_eq!(v["instances"].as_array().unwrap().len(), 1);

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Connect, catalog, health
// --------------------------------------------------------------------- //

#[tokio::test]
async fn connect_serves_a_generation_pinned_catalog() {
    let (app, store) = app();
    let instance_id = mcp_instance(&app).await;

    // A pending instance has served no catalog yet.
    let (status, v) = call(
        &app,
        "GET",
        &format!("/connectors/instances/{instance_id}/catalog"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "body: {v}");

    let connected = connect_healthy(&app, &instance_id).await;
    assert_eq!(connected["instance"]["catalog_generation"], 1);

    let (status, v) = call(
        &app,
        "GET",
        &format!("/connectors/instances/{instance_id}/catalog"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "catalog failed: {v}");
    let catalog = &v["catalog"];
    assert_eq!(catalog["generation"], 1);
    assert!(!catalog["hash"].as_str().unwrap().is_empty());
    let tools = catalog["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "test-conn/echo");

    // A matching pin serves; a stale pin is a 409 naming the live
    // generation.
    let (status, v) = call(
        &app,
        "GET",
        &format!("/connectors/instances/{instance_id}/catalog?generation=1"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "pinned catalog failed: {v}");
    let (status, v) = call(
        &app,
        "GET",
        &format!("/connectors/instances/{instance_id}/catalog?generation=9"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {v}");
    assert!(v["message"].as_str().unwrap().contains("live generation 1"));

    // Already healthy: a second connect is a guard violation, not a
    // reconnect.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/connectors/instances/{instance_id}/connect"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {v}");

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn connect_failure_lands_failed_and_still_answers_200() {
    let (app, store) = app();
    let mut sick = mcp_manifest();
    sick["id"] = json!("sick-conn");
    sick["provider"]["args"] = json!(["-c", "exit 1"]);
    let receipt = publish(&app, None, sick).await;
    let (status, v) = instantiate(
        &app,
        None,
        receipt["manifest_hash"].as_str().unwrap(),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "instantiate failed: {v}");
    let instance_id = v["instance"]["instance_id"].as_str().unwrap().to_owned();

    // The provider fails (the child exits before answering): the
    // lifecycle is the answer — 200 with `failed`, not a 5xx.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/connectors/instances/{instance_id}/connect"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "connect failed: {v}");
    assert_eq!(v["instance"]["state"], "failed");
    assert!(v["instance"]["state_reason"].is_string());

    // `failed` is connectable: the retry path is connect, not
    // re-instantiation.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/connectors/instances/{instance_id}/connect"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "reconnect failed: {v}");
    assert_eq!(v["instance"]["state"], "failed");

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn health_disable_enable_guard_the_lifecycle() {
    let (app, store) = app();
    let instance_id = mcp_instance(&app).await;

    // Health checks do not apply to a pending instance.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/connectors/instances/{instance_id}/health"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {v}");

    connect_healthy(&app, &instance_id).await;

    // On-demand health: the check re-derives the catalog from the live
    // session; unchanged bytes leave the generation untouched.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/connectors/instances/{instance_id}/health"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "health failed: {v}");
    assert_eq!(v["outcome"]["previous_state"]["state"], "healthy");
    assert_eq!(v["outcome"]["current_state"]["state"], "healthy");
    assert_eq!(v["outcome"]["catalog_bumped"], false);
    assert_eq!(v["instance"]["catalog_generation"], 1);
    assert!(v["instance"]["last_health_check_ms"].is_number());

    // Disable parks the instance; connect and health refuse it.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/connectors/instances/{instance_id}/disable"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "disable failed: {v}");
    assert_eq!(v["instance"]["state"], "disabled");
    let (status, v) = call(
        &app,
        "POST",
        &format!("/connectors/instances/{instance_id}/disable"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {v}");
    let (status, v) = call(
        &app,
        "POST",
        &format!("/connectors/instances/{instance_id}/connect"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {v}");
    let (status, v) = call(
        &app,
        "POST",
        &format!("/connectors/instances/{instance_id}/health"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {v}");

    // Enable returns it to pending; only enable applies to disabled.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/connectors/instances/{instance_id}/enable"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "enable failed: {v}");
    assert_eq!(v["instance"]["state"], "pending");
    let (status, v) = call(
        &app,
        "POST",
        &format!("/connectors/instances/{instance_id}/enable"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {v}");

    // Reconnect re-derives the same catalog: the generation holds.
    let connected = connect_healthy(&app, &instance_id).await;
    assert_eq!(connected["instance"]["catalog_generation"], 1);

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Sweep
// --------------------------------------------------------------------- //

#[tokio::test]
async fn sweep_rechecks_and_reports_only_the_callers_tenant() {
    let (app, store) = two_tenant_app();
    let acme = Some(("x-api-key", "acme-key"));
    let globex = Some(("x-api-key", "globex-key"));

    let receipt = publish(&app, acme, mcp_manifest()).await;
    let (status, v) = instantiate(
        &app,
        acme,
        receipt["manifest_hash"].as_str().unwrap(),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "instantiate failed: {v}");
    let instance_id = v["instance"]["instance_id"].as_str().unwrap().to_owned();
    let (status, v) = call_as(
        &app,
        acme,
        "POST",
        &format!("/connectors/instances/{instance_id}/connect"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "connect failed: {v}");

    // The sweep is plane-wide; the report is the caller's tenant only.
    let (status, v) = call_as(&app, globex, "POST", "/connectors/sweep", None).await;
    assert_eq!(status, StatusCode::OK, "sweep failed: {v}");
    assert_eq!(v["outcomes"], json!([]));

    let (status, v) = call_as(&app, acme, "POST", "/connectors/sweep", None).await;
    assert_eq!(status, StatusCode::OK, "sweep failed: {v}");
    let outcomes = v["outcomes"].as_array().unwrap();
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0]["instance_id"], instance_id);
    assert_eq!(outcomes[0]["current_state"]["state"], "healthy");

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Tenant isolation
// --------------------------------------------------------------------- //

#[tokio::test]
async fn cross_tenant_instances_are_indistinguishable_from_unknown() {
    let (app, store) = two_tenant_app();
    let acme = Some(("x-api-key", "acme-key"));
    let globex = Some(("x-api-key", "globex-key"));

    let receipt = publish(&app, acme, mcp_manifest()).await;
    let hash = receipt["manifest_hash"].as_str().unwrap().to_owned();
    let (status, v) = instantiate(&app, acme, &hash, json!({})).await;
    assert_eq!(status, StatusCode::CREATED, "instantiate failed: {v}");
    let instance_id = v["instance"]["instance_id"].as_str().unwrap().to_owned();
    let (status, _) = call_as(
        &app,
        acme,
        "POST",
        &format!("/connectors/instances/{instance_id}/connect"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Globex never sees acme's manifests, cannot instantiate acme's hash,
    // and every per-instance verb on acme's id is a 404 — never a 403.
    let (status, v) = call_as(&app, globex, "GET", "/connectors/manifests", None).await;
    assert_eq!(status, StatusCode::OK, "list failed: {v}");
    assert_eq!(v["manifests"], json!([]));
    let (status, v) = call_as(&app, globex, "GET", "/connectors/instances", None).await;
    assert_eq!(status, StatusCode::OK, "list failed: {v}");
    assert_eq!(v["instances"], json!([]));
    let (status, _) = instantiate(&app, globex, &hash, json!({})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    for (method, suffix) in [
        ("GET", "catalog"),
        ("POST", "connect"),
        ("POST", "health"),
        ("POST", "disable"),
        ("POST", "enable"),
    ] {
        let (status, v) = call_as(
            &app,
            globex,
            method,
            &format!("/connectors/instances/{instance_id}/{suffix}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {suffix}: {v}");
    }

    // Acme's view is untouched.
    let (status, v) = call_as(&app, acme, "GET", "/connectors/instances", None).await;
    assert_eq!(status, StatusCode::OK, "list failed: {v}");
    assert_eq!(v["instances"].as_array().unwrap().len(), 1);

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Restart durability
// --------------------------------------------------------------------- //

#[tokio::test]
async fn manifests_and_instances_restore_pending_across_a_restart() {
    let store = temp_store();
    let first = app_with(store.clone(), |config| config);
    let receipt = publish(&first, None, mcp_manifest()).await;
    let hash = receipt["manifest_hash"].as_str().unwrap().to_owned();
    let (status, v) = instantiate(&first, None, &hash, json!({})).await;
    assert_eq!(status, StatusCode::CREATED, "instantiate failed: {v}");
    let instance_id = v["instance"]["instance_id"].as_str().unwrap().to_owned();
    let connected = connect_healthy(&first, &instance_id).await;
    assert_eq!(connected["instance"]["catalog_generation"], 1);
    let served_hash = connected["instance"]["catalog_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    drop(first);

    // A fresh process over the same root: the manifest re-registers, the
    // instance restores `pending` (sessions do not survive boot), and the
    // durable record still holds the served catalog generation.
    let second = app_with(store.clone(), |config| config);
    let (status, v) = call(&second, "GET", "/connectors/manifests", None).await;
    assert_eq!(status, StatusCode::OK, "post-restart list failed: {v}");
    assert_eq!(v["manifests"].as_array().unwrap().len(), 1);

    let (status, v) = call(&second, "GET", "/connectors/instances", None).await;
    assert_eq!(status, StatusCode::OK, "post-restart list failed: {v}");
    let instances = v["instances"].as_array().unwrap();
    assert_eq!(instances.len(), 1);
    assert_eq!(instances[0]["instance_id"], instance_id);
    assert_eq!(instances[0]["state"], "pending");
    assert_eq!(instances[0]["catalog_generation"], 1);
    assert_eq!(instances[0]["catalog_hash"], served_hash);

    // The durable record answers a pinned catalog read even before the
    // instance reconnects.
    let (status, v) = call(
        &second,
        "GET",
        &format!("/connectors/instances/{instance_id}/catalog?generation=1"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "post-restart catalog failed: {v}");
    assert_eq!(v["catalog"]["tools"][0]["name"], "test-conn/echo");

    // Reconnect re-spawns the provider; unchanged bytes keep the
    // generation — the durable chain is monotone across restarts.
    let reconnected = connect_healthy(&second, &instance_id).await;
    assert_eq!(reconnected["instance"]["catalog_generation"], 1);
    assert_eq!(reconnected["instance"]["catalog_hash"], served_hash);

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// The redaction audit
// --------------------------------------------------------------------- //

#[tokio::test]
async fn no_connector_answer_carries_credential_material() {
    let (app, store) = app();
    let receipt = publish(&app, None, search_manifest()).await;
    let connection_id = register_connection(&app, None).await;
    let (status, v) = instantiate(
        &app,
        None,
        receipt["manifest_hash"].as_str().unwrap(),
        json!({"api_key": connection_id}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "instantiate failed: {v}");
    let instance_id = v["instance"]["instance_id"].as_str().unwrap().to_owned();

    // Collect every answer the plane serves for this tenant, healthy
    // session included (the search catalog is declarative; connect makes
    // no network call).
    let mut served = vec![receipt, v];
    let (status, v) = call(
        &app,
        "POST",
        &format!("/connectors/instances/{instance_id}/connect"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "connect failed: {v}");
    served.push(v);
    for (method, uri) in [
        ("GET", "/connectors/manifests".to_owned()),
        ("GET", "/connectors/instances".to_owned()),
        (
            "GET",
            format!("/connectors/instances/{instance_id}/catalog"),
        ),
        (
            "POST",
            format!("/connectors/instances/{instance_id}/health"),
        ),
        ("POST", "/connectors/sweep".to_owned()),
    ] {
        let (status, v) = call(&app, method, &uri, None).await;
        assert_eq!(status, StatusCode::OK, "{method} {uri} failed: {v}");
        served.push(v);
    }

    let everything = serde_json::to_string(&served).unwrap();
    assert!(
        !everything.contains(MARKER),
        "a connector answer carried the credential material"
    );

    // The store root holds the slot → connection-id binding, never the
    // material (the file backend's half of the audit).
    let mut files = Vec::new();
    fn walk(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else {
                out.push(path);
            }
        }
    }
    walk(&store.join("connectors"), &mut files);
    assert!(!files.is_empty());
    for file in files {
        let raw = std::fs::read_to_string(&file).unwrap();
        assert!(
            !raw.contains(MARKER),
            "{} carried the credential material",
            file.display()
        );
    }

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Instance config params
// --------------------------------------------------------------------- //

/// An `http-api` manifest templating its base URL on the `instance`
/// config param — the ServiceNow pack's shape, trimmed to one operation.
fn tenant_api_manifest() -> Value {
    json!({
        "id": "tenant-api",
        "version": "1.0.0",
        "display_name": "Tenant API",
        "description": "An http-api connector whose base URL templates the instance subdomain.",
        "provider": {
            "kind": "http_api",
            "base_url": "https://{instance}.example.test",
            "auth": {"style": "basic", "username_slot": "username", "password_slot": "password"},
            "default_headers": [],
            "health_check": null,
            "operations": [{
                "name": "ping",
                "description": "Ping the tenant API.",
                "method": "GET",
                "path": "/v1/ping",
                "params_schema": {"type": "object"},
                "query_params": [],
                "body": {"type": "none"},
                "effect": "read_only",
                "response": {"projection": null, "max_bytes": null},
                "timeout_ms": null,
                "idempotency_key_header": null,
            }],
        },
        "capabilities": ["tenant api"],
        "credential_slots": [
            {"name": "username", "description": "The user name."},
            {"name": "password", "description": "The password."},
        ],
        "config_params": [{"name": "instance", "description": "The instance subdomain."}],
    })
}

/// Instantiate with credentials and config; returns `(status, body)`.
async fn instantiate_full(
    app: &Router,
    manifest_hash: &str,
    credentials: Value,
    config: Value,
) -> (StatusCode, Value) {
    call(
        app,
        "POST",
        "/connectors/instances",
        Some(json!({
            "manifest_hash": manifest_hash,
            "credentials": credentials,
            "config": config,
        })),
    )
    .await
}

#[tokio::test]
async fn instantiate_validates_config_against_the_manifest() {
    let (app, store) = app();
    let receipt = publish(&app, None, tenant_api_manifest()).await;
    let hash = receipt["manifest_hash"].as_str().unwrap().to_owned();
    // Both basic-auth slots may bind the same connection — the bridge
    // resolves each slot through its own id, and one id is fine here.
    let connection_id = register_connection(&app, None).await;
    let credentials = json!({"username": connection_id, "password": connection_id});

    // A missing declared param is a 422 naming it — before any vault work.
    let (status, v) = instantiate_full(&app, &hash, credentials.clone(), json!({})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "body: {v}");
    assert!(v.to_string().contains("config param `instance`"), "{v}");

    // An undeclared key is a 422 naming it.
    let (status, v) = instantiate_full(
        &app,
        &hash,
        credentials.clone(),
        json!({"instance": "dev123", "region": "eu"}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "body: {v}");
    assert!(v.to_string().contains("config key `region`"), "{v}");

    // Empty, oversized, and URL-structure-smuggling values all fail.
    for bad in [
        json!({"instance": ""}),
        json!({"instance": "x".repeat(2049)}),
        json!({"instance": "dev123?debug=1"}),
        json!({"instance": "dev123#frag"}),
    ] {
        let (status, v) = instantiate_full(&app, &hash, credentials.clone(), bad).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "body: {v}");
    }

    // The exact declared set instantiates; the view carries the config
    // (non-secret) and no credential material.
    let (status, v) =
        instantiate_full(&app, &hash, credentials, json!({"instance": "dev123"})).await;
    assert_eq!(status, StatusCode::CREATED, "instantiate failed: {v}");
    assert_eq!(v["instance"]["config"]["instance"], "dev123");
    assert!(!v.to_string().contains(MARKER));

    // A manifest declaring no config params rejects an unexpected key.
    let receipt = publish(&app, None, search_manifest()).await;
    let hash = receipt["manifest_hash"].as_str().unwrap().to_owned();
    let (status, v) = instantiate_full(&app, &hash, json!({}), json!({"instance": "dev123"})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "body: {v}");

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn instance_config_persists_and_replays_across_a_restart() {
    let store = temp_store();
    let first = app_with(store.clone(), |config| config);
    let receipt = publish(&first, None, tenant_api_manifest()).await;
    let hash = receipt["manifest_hash"].as_str().unwrap().to_owned();
    let connection_id = register_connection(&first, None).await;
    let credentials = json!({"username": connection_id, "password": connection_id});
    let (status, v) =
        instantiate_full(&first, &hash, credentials, json!({"instance": "dev123"})).await;
    assert_eq!(status, StatusCode::CREATED, "instantiate failed: {v}");
    let instance_id = v["instance"]["instance_id"].as_str().unwrap().to_owned();
    drop(first);

    // A fresh process over the same root replays the config exactly: the
    // restored instance carries it and reconnects against the substituted
    // base URL (the pack declares no health check, so connect is the
    // declarative catalog derivation — no network).
    let second = app_with(store.clone(), |config| config);
    let (status, v) = call(&second, "GET", "/connectors/instances", None).await;
    assert_eq!(status, StatusCode::OK, "post-restart list failed: {v}");
    let instances = v["instances"].as_array().unwrap();
    assert_eq!(instances.len(), 1);
    assert_eq!(instances[0]["instance_id"], instance_id);
    assert_eq!(instances[0]["config"]["instance"], "dev123");
    assert_eq!(instances[0]["state"], "pending");

    let connected = connect_healthy(&second, &instance_id).await;
    assert_eq!(connected["instance"]["config"]["instance"], "dev123");
    assert_eq!(connected["instance"]["catalog_generation"], 1);

    let _ = std::fs::remove_dir_all(store);
}
