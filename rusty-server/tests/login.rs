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

use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::http::{Request, StatusCode};
use rusty_agent_runtime::prelude::*;
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
// Manifest registration
// --------------------------------------------------------------------- //

// --------------------------------------------------------------------- //
// People sign in; the first boot creates the administrator.
// --------------------------------------------------------------------- //

fn product_app() -> (Router, PathBuf) {
    let store = temp_store();
    let config =
        ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone()).with_bootstrap_admin(true);
    (router(GraphRegistry::new(), config), store)
}

/// Send a request with a session cookie.
async fn call_with_cookie(
    app: &Router,
    cookie: &str,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value, Option<String>) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("cookie", cookie);
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
    let set_cookie = response
        .headers()
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let bytes: Bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value, set_cookie)
}

async fn bootstrap_password(store: &std::path::Path) -> String {
    // The bootstrap runs right after boot; give it a moment.
    for _ in 0..50 {
        if let Ok(text) = std::fs::read_to_string(store.join("bootstrap-admin.txt")) {
            return text
                .lines()
                .find_map(|l| {
                    l.trim()
                        .strip_prefix("password:")
                        .map(|p| p.trim().to_owned())
                })
                .expect("password line");
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("no bootstrap-admin.txt under {}", store.display());
}

#[tokio::test]
async fn the_first_boot_creates_an_administrator_and_requires_sign_in() {
    let (app, store) = product_app();
    let password = bootstrap_password(&store).await;

    // Nobody is nobody: without a session or a key the door is shut.
    let (status, _) = call(&app, "GET", "/assistants", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // A wrong password is refused in the same words as a wrong name.
    let (status, body, _) = call_with_cookie(
        &app,
        "",
        "POST",
        "/auth/login",
        Some(json!({"username": "admin", "password": "nope-nope-nope"})),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");

    // The handed-over password signs the administrator in; the session
    // rides back as an HttpOnly cookie.
    let (status, me, cookie) = call_with_cookie(
        &app,
        "",
        "POST",
        "/auth/login",
        Some(json!({"username": "admin", "password": password})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{me}");
    assert_eq!(me["principal"]["id"], "admin");
    assert_eq!(me["roles"], json!(["admin"]));
    let cookie = cookie.expect("a session cookie");
    assert!(cookie.contains("HttpOnly"), "{cookie}");
    let session = cookie.split(';').next().unwrap().to_owned();

    // The cookie is who you are from now on.
    let (status, me, _) = call_with_cookie(&app, &session, "GET", "/me", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me["principal"]["id"], "admin");
    let (status, _, _) = call_with_cookie(&app, &session, "GET", "/assistants", None).await;
    assert_eq!(status, StatusCode::OK);

    // The administrator adds a builder, who signs in and is a builder.
    let (status, created, _) = call_with_cookie(&app, &session, "POST", "/users", Some(json!({"username": "bob", "name": "Bob", "roles": ["builder"], "password": "a-long-enough-one"}))).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert!(
        created.get("hash").is_none() && created.get("salt").is_none(),
        "digests never leave: {created}"
    );
    let (status, bob, bob_cookie) = call_with_cookie(
        &app,
        "",
        "POST",
        "/auth/login",
        Some(json!({"username": "bob", "password": "a-long-enough-one"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{bob}");
    let bob_session = bob_cookie.unwrap().split(';').next().unwrap().to_owned();
    let (status, _, _) = call_with_cookie(&app, &bob_session, "GET", "/users", None).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a builder does not manage people"
    );

    // Bob changes his own password; the old one stops working.
    let (status, _, _) = call_with_cookie(
        &app,
        &bob_session,
        "POST",
        "/auth/password",
        Some(json!({"current": "a-long-enough-one", "new": "another-long-one"})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, _) = call_with_cookie(
        &app,
        "",
        "POST",
        "/auth/login",
        Some(json!({"username": "bob", "password": "a-long-enough-one"})),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Signing out ends the session here and in the browser.
    let (status, _, cleared) = call_with_cookie(&app, &session, "POST", "/auth/logout", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(cleared.unwrap().contains("Max-Age=0"));
    let (status, _, _) = call_with_cookie(&app, &session, "GET", "/assistants", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn an_embedded_server_without_bootstrap_stays_open() {
    // The library default: no users, no keys — open, as every existing test
    // relies on. The product entry point is what opts into people.
    let (app, store) = app();
    let (status, me) = call(&app, "GET", "/me", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me["principal"]["id"], "dev");
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn a_session_survives_a_restart_and_a_sign_out_ends_it_everywhere() {
    let (app, store) = product_app();
    let password = bootstrap_password(&store).await;
    let (status, me, cookie) = call_with_cookie(
        &app,
        "",
        "POST",
        "/auth/login",
        Some(json!({"username": "admin", "password": password})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{me}");
    let cookie = cookie
        .expect("a session cookie")
        .split(';')
        .next()
        .unwrap()
        .to_owned();

    // The process restarts: a new server on the same store. The cookie the
    // browser still holds must be a session, not a sign-in screen.
    let again = app_at(store.clone());
    let (status, me, _) = call_with_cookie(&again, &cookie, "GET", "/me", None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "after a restart the session is gone: {me}"
    );
    assert_eq!(me["principal"]["id"], json!("admin"), "{me}");

    // Signing out on the new process ends the session for the old one too:
    // the store forgot it, and the old process's map is a cache.
    let (status, _, _) = call_with_cookie(&again, &cookie, "POST", "/auth/logout", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, _) = call_with_cookie(&again, &cookie, "GET", "/me", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _, _) =
        call_with_cookie(&app_at(store.clone()), &cookie, "GET", "/me", None).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "a third process must not resurrect a closed session"
    );

    let _ = std::fs::remove_dir_all(store);
}

/// A pipeline graph the revocation tests can schedule: one node, no model.
fn probe_registry() -> GraphRegistry {
    let spec = StateSpec::new().channel("done", Reducer::Overwrite);
    let mut builder = GraphBuilder::new();
    builder.add_node("work", |_ctx: NodeContext| async move {
        Ok(NodeOutput::update("done", json!(true)))
    });
    builder.set_entry_point("work");
    let mut registry = GraphRegistry::new();
    registry.register("probe", builder.compile().unwrap(), spec);
    registry
}

fn product_app_with_graph() -> (Router, PathBuf) {
    let store = temp_store();
    let config =
        ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone()).with_bootstrap_admin(true);
    (router(probe_registry(), config), store)
}

/// Astra R02: deleting a user, or revoking their sessions, ends the sessions
/// already issued — on the next request, not at the next sign-in — and a
/// schedule that person created no longer fires.
#[tokio::test]
async fn a_deleted_or_revoked_user_is_signed_out_on_their_next_request_and_their_schedule_stops() {
    let (app, store) = product_app_with_graph();
    let password = bootstrap_password(&store).await;
    let (status, _, admin_cookie) = call_with_cookie(
        &app,
        "",
        "POST",
        "/auth/login",
        Some(json!({"username": "admin", "password": password})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let admin = admin_cookie.unwrap().split(';').next().unwrap().to_owned();
    let (status, _, _) = call_with_cookie(&app, &admin, "POST", "/users", Some(json!({"username": "bob", "name": "Bob", "roles": ["builder"], "password": "a-long-enough-one"}))).await;
    assert_eq!(status, StatusCode::CREATED);
    let sign_in_bob = || async {
        let (status, _, cookie) = call_with_cookie(
            &app,
            "",
            "POST",
            "/auth/login",
            Some(json!({"username": "bob", "password": "a-long-enough-one"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        cookie.unwrap().split(';').next().unwrap().to_owned()
    };

    // Revocation: Bob's session works, an admin revokes, the same session is refused; a fresh sign-in works.
    let bob = sign_in_bob().await;
    let (status, me, _) = call_with_cookie(&app, &bob, "GET", "/me", None).await;
    assert_eq!(status, StatusCode::OK, "{me}");
    let (status, _, _) =
        call_with_cookie(&app, &admin, "POST", "/users/bob/sessions/revoke", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, _) = call_with_cookie(&app, &bob, "GET", "/me", None).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "a revoked session is refused on its next request"
    );
    let bob = sign_in_bob().await;
    let (status, _, _) = call_with_cookie(&app, &bob, "GET", "/me", None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a fresh sign-in after revocation works"
    );

    // A schedule Bob created fires while Bob is a user.
    let (status, thread, _) = call_with_cookie(
        &app,
        &bob,
        "POST",
        "/threads",
        Some(json!({"graph": "probe"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{thread}");
    let (status, cron, _) = call_with_cookie(
        &app,
        &bob,
        "POST",
        "/crons",
        Some(json!({"graph": "probe", "interval_secs": 1, "input": {}})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{cron}");
    let cron_id = cron["cron_id"].as_str().unwrap().to_owned();
    let mut fired_before = 0;
    for _ in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let (_, crons, _) = call_with_cookie(&app, &admin, "GET", "/crons", None).await;
        fired_before = crons
            .as_array()
            .into_iter()
            .flatten()
            .find(|c| c["cron_id"] == json!(cron_id))
            .and_then(|c| c["runs_fired"].as_u64())
            .unwrap_or(0);
        if fired_before >= 1 {
            break;
        }
    }
    assert!(fired_before >= 1, "the schedule fired while Bob was a user");

    // Deletion: Bob's session ends on the next request, and the schedule stops firing.
    let (status, _, _) = call_with_cookie(&app, &admin, "DELETE", "/users/bob", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, _) = call_with_cookie(&app, &bob, "GET", "/me", None).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "a deleted user's session is refused on its next request"
    );
    let (_, crons, _) = call_with_cookie(&app, &admin, "GET", "/crons", None).await;
    let at_deletion = crons
        .as_array()
        .into_iter()
        .flatten()
        .find(|c| c["cron_id"] == json!(cron_id))
        .and_then(|c| c["runs_fired"].as_u64())
        .unwrap_or(0);
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
    let (_, crons, _) = call_with_cookie(&app, &admin, "GET", "/crons", None).await;
    let later = crons
        .as_array()
        .into_iter()
        .flatten()
        .find(|c| c["cron_id"] == json!(cron_id))
        .and_then(|c| c["runs_fired"].as_u64())
        .unwrap_or(0);
    assert_eq!(
        later, at_deletion,
        "a deleted user's schedule fired again: {later} vs {at_deletion}"
    );
    let _ = std::fs::remove_dir_all(store);
}
