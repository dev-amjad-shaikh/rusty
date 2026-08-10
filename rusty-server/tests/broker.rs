//! The credential/connection broker integration tests (R0.11 Extension
//! Plane, wave 3): the `/connections` surface and the `/broker/journal`
//! evidence chain over the default JSON-file backend — registration
//! (validation, tenant isolation), consent as the only scope-widening
//! path (journaled, converging), revocation and erase, health, restart
//! durability, ciphertext-at-rest, and the deployment journal's contents.
//! Live-Postgres coverage of the exit criterion "a dump holds no
//! plaintext credential" is the gated section at the bottom
//! (`RUSTY_TEST_DATABASE_URL`).
//!
//! Driven in-process via `tower::ServiceExt::oneshot` (no sockets), the
//! `registry.rs` convention. Handle issue/resolve and the typed denial
//! classes are in-process seams (tools and capsule hosts mediate against
//! the broker, not HTTP), so they are proven in the module's unit tests;
//! this file proves the HTTP custody and lifecycle surface end to end.

use std::path::PathBuf;

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

/// The plaintext credential every registration here seals. Distinctive,
/// so a scan for it is a scan for a leak: it may appear in exactly one
/// place — the request body that carried it — and nowhere the server
/// writes or serves.
const MARKER: &str = "sk-live-MARKER-9f2e";

// --------------------------------------------------------------------- //
// Harness (the registry.rs shapes)
// --------------------------------------------------------------------- //

/// Unique temp store root, removed at the end of each test (best effort).
fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!("rusty-server-broker-test-{}", uuid::Uuid::new_v4()))
}

/// An app over `store` with the config customized by `configure` (tenant
/// keys, the Postgres backend). No graphs: the broker surface stands on
/// its own.
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

/// A register payload carrying the marker credential.
fn register_payload() -> Value {
    json!({
        "provider": "oauth2_authorization_code",
        "subject": "user-7",
        "scopes": ["repo", "repo:status"],
        "token": {
            "access_token": MARKER,
            "refresh_token": "rt-refresh-1",
        },
    })
}

/// Register one connection; returns the served record.
async fn register(app: &Router, auth: Option<(&str, &str)>) -> Value {
    let (status, v) = call_as(app, auth, "POST", "/connections", Some(register_payload())).await;
    assert_eq!(status, StatusCode::CREATED, "register failed: {v}");
    let record = &v["connection"];
    assert!(record["connection_id"]
        .as_str()
        .unwrap()
        .starts_with("conn-"));
    assert_eq!(record["status"], "active");
    record.clone()
}

// --------------------------------------------------------------------- //
// Registration, reads, validation
// --------------------------------------------------------------------- //

#[tokio::test]
async fn register_then_get_and_list() {
    let (app, store) = app();

    let record = register(&app, None).await;
    assert_eq!(record["provider"], "oauth2_authorization_code");
    assert_eq!(record["subject"], "user-7");
    assert_eq!(record["scopes"], json!(["repo", "repo:status"]));
    assert_eq!(record["health"]["consecutive_failures"], 0);
    let connection_id = record["connection_id"].as_str().unwrap();

    let (status, v) = call(&app, "GET", &format!("/connections/{connection_id}"), None).await;
    assert_eq!(status, StatusCode::OK, "get failed: {v}");
    assert_eq!(v["connection"]["connection_id"], connection_id);

    let (status, v) = call(&app, "GET", "/connections", None).await;
    assert_eq!(status, StatusCode::OK, "list failed: {v}");
    let connections = v["connections"].as_array().unwrap();
    assert_eq!(connections.len(), 1);
    assert_eq!(connections[0]["connection_id"], connection_id);

    // Nothing the server served carries the credential — the record is
    // metadata by construction, and the material is not in any answer.
    let served = serde_json::to_string(&json!([record, v])).unwrap();
    assert!(
        !served.contains(MARKER),
        "a served answer leaked the credential"
    );

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn register_validation_refuses_bad_input_before_the_store() {
    let (app, store) = app();

    // An empty access token could never resolve — a client mistake, 422.
    let (status, v) = call(
        &app,
        "POST",
        "/connections",
        Some(json!({
            "provider": "api_key",
            "scopes": ["repo"],
            "token": {"access_token": ""},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "got: {v}");

    // Free-text rules (no `/`, the tenant separator) — 422, not 500.
    let (status, v) = call(
        &app,
        "POST",
        "/connections",
        Some(json!({
            "provider": "api_key",
            "scopes": ["repo/admin"],
            "token": {"access_token": MARKER},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "got: {v}");

    // An unknown provider kind fails deserialization — axum's 422, still
    // never reaching the store.
    let (status, _v) = call(
        &app,
        "POST",
        "/connections",
        Some(json!({
            "provider": "carrier_pigeon",
            "scopes": ["repo"],
            "token": {"access_token": MARKER},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // Nothing landed: the list stays empty.
    let (status, v) = call(&app, "GET", "/connections", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["connections"].as_array().unwrap().len(), 0);

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn connections_are_tenant_scoped() {
    let (app, store) = two_tenant_app();
    let acme = Some(("x-api-key", "acme-key"));
    let globex = Some(("x-api-key", "globex-key"));

    let record = register(&app, acme).await;
    let connection_id = record["connection_id"].as_str().unwrap();

    // Every verb answers 404 across the boundary — unknown and
    // cross-tenant are indistinguishable.
    let (status, _) = call_as(
        &app,
        globex,
        "GET",
        &format!("/connections/{connection_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, v) = call_as(&app, globex, "GET", "/connections", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["connections"].as_array().unwrap().len(), 0);
    let (status, _) = call_as(
        &app,
        globex,
        "POST",
        &format!("/connections/{connection_id}/consent"),
        Some(json!({"scopes": ["repo"]})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call_as(
        &app,
        globex,
        "POST",
        &format!("/connections/{connection_id}/revoke"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call_as(
        &app,
        globex,
        "DELETE",
        &format!("/connections/{connection_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call_as(
        &app,
        globex,
        "GET",
        &format!("/connections/{connection_id}/health"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The owning tenant reads it throughout.
    let (status, _) = call_as(
        &app,
        acme,
        "GET",
        &format!("/connections/{connection_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Consent — the only scope-widening path
// --------------------------------------------------------------------- //

#[tokio::test]
async fn consent_widens_only_by_journaled_act_and_converges() {
    let (app, store) = app();
    let record = register(&app, None).await;
    let connection_id = record["connection_id"].as_str().unwrap();
    let consent_uri = format!("/connections/{connection_id}/consent");

    // A scope-set change journals `connection_consented`.
    let (status, v) = call(
        &app,
        "POST",
        &consent_uri,
        Some(json!({"scopes": ["repo", "repo:status", "workflow"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "consent failed: {v}");
    assert_eq!(v["journaled"], "connection_consented");
    assert_eq!(
        v["connection"]["scopes"],
        json!(["repo", "repo:status", "workflow"])
    );

    // Re-recording the same fact converges — no second event.
    let (status, v) = call(
        &app,
        "POST",
        &consent_uri,
        Some(json!({"scopes": ["repo", "repo:status", "workflow"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "re-consent failed: {v}");
    assert!(v["journaled"].is_null());

    // Material-only is a refresh: `connection_refreshed`, and the health
    // record notes the refresh instant.
    let (status, v) = call(
        &app,
        "POST",
        &consent_uri,
        Some(json!({"token": {"access_token": "sk-live-MARKER-rotated", "refresh_token": "rt-2"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "refresh failed: {v}");
    assert_eq!(v["journaled"], "connection_refreshed");
    assert!(v["connection"]["health"]["last_refresh_at"].is_string());
    assert_eq!(
        v["connection"]["scopes"],
        json!(["repo", "repo:status", "workflow"])
    );

    // An empty act is a client mistake.
    let (status, _) = call(&app, "POST", &consent_uri, Some(json!({}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // Unknown ids 404.
    let (status, _) = call(
        &app,
        "POST",
        "/connections/conn-00000000000000000000000000000000/consent",
        Some(json!({"scopes": ["repo"]})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Revocation, erase, health
// --------------------------------------------------------------------- //

#[tokio::test]
async fn revoke_converges_and_delete_erases() {
    let (app, store) = app();
    let record = register(&app, None).await;
    let connection_id = record["connection_id"].as_str().unwrap();
    let revoke_uri = format!("/connections/{connection_id}/revoke");

    // The revocation applies and journals; the answer carries the
    // event's id and the flipped record.
    let (status, v) = call(
        &app,
        "POST",
        &revoke_uri,
        Some(json!({"reason": "user asked"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "revoke failed: {v}");
    assert!(v["event_id"].is_string());
    assert_eq!(v["connection"]["status"], "revoked");

    // Re-revocation converges: 200, no second event.
    let (status, v) = call(&app, "POST", &revoke_uri, Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK, "re-revoke failed: {v}");
    assert!(v["event_id"].is_null());
    assert_eq!(v["connection"]["status"], "revoked");

    // Erase: 200 once, 404 thereafter, and the record is gone.
    let (status, v) = call(
        &app,
        "DELETE",
        &format!("/connections/{connection_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "delete failed: {v}");
    assert_eq!(v["deleted"], true);
    let (status, _) = call(&app, "GET", &format!("/connections/{connection_id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(
        &app,
        "DELETE",
        &format!("/connections/{connection_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn delete_revokes_a_live_connection_first() {
    let (app, store) = app();
    let record = register(&app, None).await;
    let connection_id = record["connection_id"].as_str().unwrap();

    // Deleting a still-active connection revokes it first — the evidence
    // trail says the grant stopped holding here before the bytes went.
    let (status, v) = call(
        &app,
        "DELETE",
        &format!("/connections/{connection_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "delete failed: {v}");
    assert_eq!(v["deleted"], true);

    let (status, v) = call(&app, "GET", "/broker/journal", None).await;
    assert_eq!(status, StatusCode::OK);
    let kinds: Vec<&str> = v["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["connection_registered", "connection_revoked"]);

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn health_reports_the_lifecycle() {
    let (app, store) = app();
    let record = register(&app, None).await;
    let connection_id = record["connection_id"].as_str().unwrap();
    let health_uri = format!("/connections/{connection_id}/health");

    let (status, v) = call(&app, "GET", &health_uri, None).await;
    assert_eq!(status, StatusCode::OK, "health failed: {v}");
    assert_eq!(v["connection_id"], connection_id);
    assert_eq!(v["status"], "active");
    assert_eq!(v["health"]["consecutive_failures"], 0);

    let (status, _) = call(
        &app,
        "POST",
        &format!("/connections/{connection_id}/revoke"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, v) = call(&app, "GET", &health_uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["status"], "revoked");

    let (status, _) = call(
        &app,
        "GET",
        "/connections/conn-00000000000000000000000000000000/health",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Durability and evidence
// --------------------------------------------------------------------- //

#[tokio::test]
async fn connections_survive_a_restart() {
    let store = temp_store();
    let first = app_with(store.clone(), |config| config);
    let record = register(&first, None).await;
    let connection_id = record["connection_id"].as_str().unwrap().to_string();
    // Consent once so the reloaded record has to carry the widened set.
    let (status, _) = call(
        &first,
        "POST",
        &format!("/connections/{connection_id}/consent"),
        Some(json!({"scopes": ["repo", "repo:status", "workflow"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    drop(first);

    // A fresh process (a fresh store index over the same root) serves
    // the settled record — the master key reloads from the keys dir.
    let second = app_with(store.clone(), |config| config);
    let (status, v) = call(
        &second,
        "GET",
        &format!("/connections/{connection_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "post-restart get failed: {v}");
    assert_eq!(
        v["connection"]["scopes"],
        json!(["repo", "repo:status", "workflow"])
    );

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn broker_journal_records_the_chain_without_the_bytes() {
    let (app, store) = app();
    let record = register(&app, None).await;
    let connection_id = record["connection_id"].as_str().unwrap();
    let (status, _) = call(
        &app,
        "POST",
        &format!("/connections/{connection_id}/consent"),
        Some(json!({"scopes": ["repo", "workflow"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(
        &app,
        "POST",
        &format!("/connections/{connection_id}/revoke"),
        Some(json!({"reason": "offboarding"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, v) = call(&app, "GET", "/broker/journal", None).await;
    assert_eq!(status, StatusCode::OK, "journal failed: {v}");
    assert_eq!(v["run_id"], "credential-broker");
    let kinds: Vec<&str> = v["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        [
            "connection_registered",
            "connection_consented",
            "connection_revoked"
        ]
    );

    // The chained evidence names the grant — never the credential.
    let journal_text = serde_json::to_string(&v).unwrap();
    assert!(
        !journal_text.contains(MARKER),
        "the journal leaked the credential"
    );
    assert!(journal_text.contains(connection_id));

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn the_file_store_holds_ciphertext_only() {
    let (app, store) = app();
    let _record = register(&app, None).await;

    // Walk everything the server wrote — connection files, the broker
    // journal, the keys dir — and scan for the plaintext. The sealed
    // envelope and the journal may name the connection; the bytes may
    // appear nowhere.
    let mut scanned = 0usize;
    let mut stack = vec![store.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let bytes = std::fs::read(&path).unwrap();
                scanned += 1;
                assert!(
                    !bytes
                        .windows(MARKER.len())
                        .any(|window| window == MARKER.as_bytes()),
                    "plaintext credential found in {}",
                    path.display()
                );
            }
        }
    }
    assert!(
        scanned >= 3,
        "expected connection + journal + key files, scanned {scanned}"
    );

    // The connection file itself is ciphertext-shaped: hex fields, a key
    // id — and the record rides along in the clear (metadata by design).
    let connections_dir = store.join("connections");
    let file = std::fs::read_dir(&connections_dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let body: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    let credential = &body["record"]["credential"];
    assert_eq!(credential["format_version"], 1);
    assert!(credential["key_id"].as_str().unwrap().starts_with("bmk-"));
    for field in ["wrapped_data_key", "wrap_nonce", "ciphertext", "nonce"] {
        let hex = credential[field].as_str().unwrap();
        assert!(!hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit()));
    }

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Postgres backend (live database required)
//
// Gated on `RUSTY_TEST_DATABASE_URL`; unset skips cleanly so the suite is
// green without a database. Every run uses a dedicated tenant, so
// repeated runs against one scratch database never interfere; the
// database itself is throwaway.
// --------------------------------------------------------------------- //

#[cfg(feature = "postgres")]
mod postgres {
    use super::*;
    use sqlx::Row;

    fn pg_url() -> Option<String> {
        std::env::var("RUSTY_TEST_DATABASE_URL").ok()
    }

    /// The wave's exit criterion on Postgres: the raw row — a database
    /// dump, byte for byte — holds no plaintext credential. Register
    /// through the HTTP surface, then read the table directly: the
    /// payload is the sealed envelope, the projected columns are
    /// metadata, and the marker appears nowhere.
    #[tokio::test]
    async fn postgres_dump_contains_no_plaintext_credential() {
        let Some(url) = pg_url() else {
            eprintln!("RUSTY_TEST_DATABASE_URL unset; skipping");
            return;
        };
        let tenant = format!("brokerpg-{}", uuid::Uuid::new_v4());
        let auth = Some(("x-api-key", "pg-secret"));
        let build = || {
            app_with(temp_store(), |config| {
                config
                    .with_postgres(url.clone())
                    .with_tenant_key(tenant.clone(), "pg-secret")
            })
        };

        let first = build();
        let record = register(&first, auth).await;
        let connection_id = record["connection_id"].as_str().unwrap().to_string();

        let pool = sqlx::PgPool::connect(&url).await.unwrap();
        let rows = sqlx::query(
            "SELECT payload::text AS payload, provider, status FROM server_connections WHERE tenant = $1",
        )
        .bind(&tenant)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 1);
        let payload: String = rows[0].get("payload");
        assert!(
            !payload.contains(MARKER),
            "the Postgres row holds the plaintext credential"
        );
        // The envelope's shape, at the row level: sealed, keyed, hex.
        let parsed: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(parsed["credential"]["format_version"], 1);
        assert_eq!(
            rows[0].get::<String, _>("provider"),
            "oauth2_authorization_code"
        );
        assert_eq!(rows[0].get::<String, _>("status"), "active");

        // The lifecycle works identically over Postgres: revoke flips the
        // projected column and converges on repeat; a reconnect (the
        // restart) serves the settled record.
        let (status, v) = call_as(
            &first,
            auth,
            "POST",
            &format!("/connections/{connection_id}/revoke"),
            Some(json!({"reason": "pg check"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "pg revoke failed: {v}");
        assert!(v["event_id"].is_string());
        let (status, v) = call_as(
            &first,
            auth,
            "POST",
            &format!("/connections/{connection_id}/revoke"),
            Some(json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "pg re-revoke failed: {v}");
        assert!(v["event_id"].is_null());
        let status_col: String =
            sqlx::query("SELECT status FROM server_connections WHERE tenant = $1")
                .bind(&tenant)
                .fetch_one(&pool)
                .await
                .unwrap()
                .get("status");
        assert_eq!(status_col, "revoked");
        drop(first);

        let second = build();
        let (status, v) = call_as(
            &second,
            auth,
            "GET",
            &format!("/connections/{connection_id}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "pg post-reconnect get failed: {v}");
        assert_eq!(v["connection"]["status"], "revoked");

        // Erase removes the row outright — real deletion, sealed
        // material included.
        let (status, v) = call_as(
            &second,
            auth,
            "DELETE",
            &format!("/connections/{connection_id}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "pg delete failed: {v}");
        assert_eq!(v["deleted"], true);
        let remaining: i64 =
            sqlx::query("SELECT count(*) AS n FROM server_connections WHERE tenant = $1")
                .bind(&tenant)
                .fetch_one(&pool)
                .await
                .unwrap()
                .get("n");
        assert_eq!(remaining, 0);
    }
}
