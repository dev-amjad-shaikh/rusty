//! Signed run receipts integration tests (R0.9 Rusty Capsules, wave 3):
//! mint-on-read over the run's reverified journal, caller-driven
//! verification with component-named failures, the key lifecycle
//! (first-boot generation, journaled rotation, the history old receipts
//! verify against), tenant isolation, and restart survival on both store
//! backends.
//!
//! Driven in-process via `tower::ServiceExt::oneshot` (the `capsules.rs`
//! convention). Live-Postgres coverage is the gated section at the
//! bottom (`RUSTY_TEST_DATABASE_URL`).

use std::path::PathBuf;

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

// --------------------------------------------------------------------- //
// Harness (the capsules.rs convention)
// --------------------------------------------------------------------- //

/// Unique temp store root, removed at the end of each test (best effort).
fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!(
        "rusty-server-receipts-test-{}",
        uuid::Uuid::new_v4()
    ))
}

/// The one graph these tests drive: `pipeline` completes, so its run has
/// a persisted journal a receipt can be minted over.
fn registry() -> GraphRegistry {
    use rusty_agent_runtime::prelude::*;

    let spec = StateSpec::new().channel("log", Reducer::Append);
    let mut builder = GraphBuilder::new();
    builder.add_node("first", |_ctx: NodeContext| async {
        Ok(NodeOutput::update("log", json!("first")))
    });
    builder.add_node("second", |_ctx: NodeContext| async {
        Ok(NodeOutput::update("log", json!("second")))
    });
    builder.set_entry_point("first");
    builder.add_edge("first", "second");
    let pipeline = builder.compile().unwrap();

    let mut registry = GraphRegistry::new();
    registry.register("pipeline", pipeline, spec);
    registry
}

/// An app over `store` with the config customized by `configure`.
fn app_with(store: PathBuf, configure: impl FnOnce(ServerConfig) -> ServerConfig) -> Router {
    let config = configure(ServerConfig::new("127.0.0.1:0".parse().unwrap(), store));
    router(registry(), config)
}

/// Open-mode (single `default` tenant) app over a fresh store.
fn app() -> (Router, PathBuf) {
    let store = temp_store();
    (app_with(store.clone(), |config| config), store)
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

/// Create a thread, run `pipeline` to completion; returns the run id.
async fn run_pipeline(app: &Router) -> String {
    run_pipeline_as(app, None).await
}

/// The tenant-scoped twin of [`run_pipeline`].
async fn run_pipeline_as(app: &Router, auth: Option<(&str, &str)>) -> String {
    let (status, v) = call_as(
        app,
        auth,
        "POST",
        "/threads",
        Some(json!({"graph": "pipeline"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "thread failed: {v}");
    let thread_id = v["thread_id"].as_str().unwrap().to_string();
    let (status, v) = call_as(
        app,
        auth,
        "POST",
        &format!("/threads/{thread_id}/runs/wait"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "run failed: {v}");
    assert_eq!(v["status"], json!("success"), "pipeline failed: {v}");
    v["run_id"].as_str().unwrap().to_string()
}

/// The run's exported journal snapshot (from the portable fixture — the
/// same export a CI verifier would carry).
async fn snapshot_of(app: &Router, run_id: &str) -> Value {
    let (status, v) = call(app, "GET", &format!("/runs/{run_id}/fixture"), None).await;
    assert_eq!(status, StatusCode::OK, "fixture failed: {v}");
    v["journal"].clone()
}

/// Mint (or serve) the run's receipt; asserts 200.
async fn receipt_of(app: &Router, run_id: &str) -> Value {
    let (status, v) = call(app, "GET", &format!("/runs/{run_id}/receipt"), None).await;
    assert_eq!(status, StatusCode::OK, "receipt failed: {v}");
    v
}

// --------------------------------------------------------------------- //
// Exit criterion 1: a receipt verifies against the exported snapshot
// --------------------------------------------------------------------- //

#[tokio::test]
async fn receipt_mints_and_verifies_against_the_exported_snapshot() {
    let (app, store) = app();
    let run_id = run_pipeline(&app).await;

    let receipt = receipt_of(&app, &run_id).await;
    assert_eq!(receipt["run_id"], json!(run_id));
    assert_eq!(receipt["format_version"], json!(1));
    assert_eq!(receipt["signer"].as_str().unwrap().len(), 64);
    assert_eq!(receipt["signature"].as_str().unwrap().len(), 128);
    // The pipeline's journal ran under the static policy floor, read back
    // from its checkpoint header.
    assert_eq!(receipt["executor_policy"], json!("static-v0"));

    // A second GET serves the stored mint verbatim — the head stands.
    let again = receipt_of(&app, &run_id).await;
    assert_eq!(receipt, again);

    // Verification against the exported snapshot answers the typed
    // summary, not a bare boolean.
    let snapshot = snapshot_of(&app, &run_id).await;
    let (status, v) = call(
        &app,
        "POST",
        "/receipts/verify",
        Some(json!({"snapshot": snapshot, "receipt": receipt})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "verify failed: {v}");
    assert_eq!(v["run_id"], json!(run_id));
    assert_eq!(
        v["journal_head"]["sha256"],
        receipt["journal_head"]["sha256"]
    );
    assert_eq!(v["effect_receipts"], json!(0));
    assert_eq!(v["signer"], receipt["signer"]);

    // The first receipt request generated the key: one history record,
    // and the genesis rotation journaled.
    let (status, v) = call(&app, "GET", "/receipt_keys", None).await;
    assert_eq!(status, StatusCode::OK, "keys failed: {v}");
    assert_eq!(v["keys"].as_array().unwrap().len(), 1);
    assert_eq!(v["active"], v["keys"][0]["key_id"]);
    assert_eq!(v["active"], receipt["signer"]);

    drop(app);
    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Exit criterion 2: one flipped byte fails verification, naming the head
// --------------------------------------------------------------------- //

#[tokio::test]
async fn one_flipped_byte_in_a_journaled_event_fails_verification_naming_the_head() {
    let (app, store) = app();
    let run_id = run_pipeline(&app).await;
    let receipt = receipt_of(&app, &run_id).await;
    let snapshot = snapshot_of(&app, &run_id).await;
    let event_count = snapshot["events"].as_array().unwrap().len();

    for index in 0..event_count {
        let mut tampered = snapshot.clone();
        tampered["events"][index]["latency_ms"] = json!(1);
        let (status, v) = call(
            &app,
            "POST",
            "/receipts/verify",
            Some(json!({"snapshot": tampered, "receipt": receipt})),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "event {index}: tampering must fail verification: {v}"
        );
        assert_eq!(
            v["error"],
            json!("receipt_verification_failed"),
            "event {index}"
        );
        assert!(
            v["message"].as_str().unwrap().starts_with("journal_head:"),
            "event {index}: the failure names the journal head: {v}"
        );
    }

    drop(app);
    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Exit criterion 3: rotation is journaled; old receipts still verify
// --------------------------------------------------------------------- //

#[tokio::test]
async fn rotation_is_journaled_and_old_receipts_still_verify_against_the_history() {
    let (app, store) = app();
    let run_id = run_pipeline(&app).await;
    let receipt = receipt_of(&app, &run_id).await;
    let snapshot = snapshot_of(&app, &run_id).await;
    let old_key_id = receipt["signer"].as_str().unwrap().to_string();

    // Rotate: the new key id, its public half, and the journaled event.
    let (status, v) = call(&app, "POST", "/receipt_keys/rotate", Some(json!({}))).await;
    assert_eq!(status, StatusCode::CREATED, "rotate failed: {v}");
    assert_eq!(v["previous_key_id"], json!(old_key_id));
    let new_key_id = v["key_id"].as_str().unwrap().to_string();
    assert_ne!(new_key_id, old_key_id);
    let event_id = v["event_id"].as_str().unwrap().to_string();

    // The rotation is journaled: genesis (previous absent) then the
    // rotation naming both key ids, in one chained journal.
    let (status, v) = call(&app, "GET", "/receipt_keys/journal", None).await;
    assert_eq!(status, StatusCode::OK, "journal failed: {v}");
    let events = v["events"].as_array().unwrap();
    assert_eq!(events.len(), 2, "genesis + rotation: {v}");
    assert_eq!(events[0]["kind"], json!("signing_key_rotated"));
    assert!(events[0]["output"]["value"]
        .get("previous_key_id")
        .is_none());
    assert_eq!(events[1]["kind"], json!("signing_key_rotated"));
    assert_eq!(events[1]["id"], json!(event_id));
    assert_eq!(
        events[1]["output"]["value"]["previous_key_id"],
        json!(old_key_id)
    );
    assert_eq!(
        events[1]["output"]["value"]["new_key_id"],
        json!(new_key_id)
    );

    // The history holds both keys; the old one is retired — and still
    // verifies the receipt it signed.
    let (status, v) = call(&app, "GET", "/receipt_keys", None).await;
    assert_eq!(status, StatusCode::OK, "keys failed: {v}");
    assert_eq!(v["keys"].as_array().unwrap().len(), 2);
    assert_eq!(v["active"], json!(new_key_id));
    assert!(
        v["keys"][0]["retired_at"].is_string(),
        "old key retired: {v}"
    );

    let (status, v) = call(
        &app,
        "POST",
        "/receipts/verify",
        Some(json!({
            "snapshot": snapshot,
            "receipt": receipt,
            "key_id": old_key_id,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "old receipt must still verify: {v}");

    // Offering the new key against the old receipt is a definitive signer
    // mismatch — key ids are content addresses.
    let (status, v) = call(
        &app,
        "POST",
        "/receipts/verify",
        Some(json!({
            "snapshot": snapshot_of(&app, &run_id).await,
            "receipt": receipt,
            "key_id": new_key_id,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert!(
        v["message"].as_str().unwrap().starts_with("signer_key_id:"),
        "{v}"
    );

    // New receipts sign with the new key.
    let second_run = run_pipeline(&app).await;
    let second_receipt = receipt_of(&app, &second_run).await;
    assert_eq!(second_receipt["signer"], json!(new_key_id));

    drop(app);
    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Component-named verification failures
// --------------------------------------------------------------------- //

#[tokio::test]
async fn tampered_ledgers_fail_naming_the_component() {
    let (app, store) = app();
    let run_id = run_pipeline(&app).await;
    let receipt = receipt_of(&app, &run_id).await;
    let snapshot = snapshot_of(&app, &run_id).await;

    let verify = |receipt: Value| {
        let app = app.clone();
        let snapshot = snapshot.clone();
        async move {
            call(
                &app,
                "POST",
                "/receipts/verify",
                Some(json!({"snapshot": snapshot, "receipt": receipt})),
            )
            .await
        }
    };

    // The pipeline journaled no effects or denials, so an added entry is
    // the tamper: the recomputed ledger is shorter than the claimed one.
    let mut forged = receipt.clone();
    forged["effects"] = json!(["0".repeat(64)]);
    let (status, v) = verify(forged).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert!(
        v["message"].as_str().unwrap().starts_with("effect_ledger:"),
        "{v}"
    );

    let mut forged = receipt.clone();
    forged["denials"] = json!([format!("{run_id}:99")]);
    let (status, v) = verify(forged).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert!(
        v["message"]
            .as_str()
            .unwrap()
            .starts_with("denials_ledger:"),
        "{v}"
    );

    // A forged manifest commitment fails by name.
    let mut forged = receipt.clone();
    forged["manifest_digest"] = json!("0".repeat(64));
    let (status, v) = verify(forged).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert!(
        v["message"]
            .as_str()
            .unwrap()
            .starts_with("manifest_digest:"),
        "{v}"
    );

    // A broken signature fails by name.
    let mut forged = receipt.clone();
    forged["signature"] = json!(receipt["signature"].as_str().unwrap().replacen('a', "b", 1));
    let (status, v) = verify(forged).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert!(
        v["message"].as_str().unwrap().starts_with("signature:"),
        "{v}"
    );

    // An unknown key id is a 404 — distinct from a verification failure.
    let (status, v) = call(
        &app,
        "POST",
        "/receipts/verify",
        Some(json!({
            "snapshot": snapshot_of(&app, &run_id).await,
            "receipt": receipt,
            "key_id": "0".repeat(64),
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{v}");

    drop(app);
    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Tenant isolation
// --------------------------------------------------------------------- //

#[tokio::test]
async fn receipts_are_tenant_isolated() {
    let store = temp_store();
    let app = app_with(store.clone(), |config| {
        config
            .with_tenant_key("alice", "alice-secret")
            .with_tenant_key("bob", "bob-secret")
    });
    let alice = Some(("x-api-key", "alice-secret"));
    let bob = Some(("x-api-key", "bob-secret"));

    let run_id = run_pipeline_as(&app, alice).await;

    // Bob cannot see Alice's run's receipt — cross-tenant is 404, never
    // 403 (the `GET /runs/{id}` rule).
    let (status, v) = call_as(&app, bob, "GET", &format!("/runs/{run_id}/receipt"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{v}");

    // Alice's own receipt mints fine, and the deployment key lifecycle —
    // not tenant state — is visible to both tenants.
    let (status, v) = call_as(&app, alice, "GET", &format!("/runs/{run_id}/receipt"), None).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let (status, v) = call_as(&app, bob, "GET", "/receipt_keys", None).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["keys"].as_array().unwrap().len(), 1);

    drop(app);
    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Restart survival (JSON-file backend)
// --------------------------------------------------------------------- //

/// Keys and minted receipts survive a restart: the secret file and the
/// key history are re-read, the stored receipt is served verbatim, and a
/// receipt signed before the restart still verifies.
#[tokio::test]
async fn keys_and_receipts_survive_a_restart() {
    let store = temp_store();
    let build = || app_with(store.clone(), |config| config);

    let first = build();
    let run_id = run_pipeline(&first).await;
    let receipt = receipt_of(&first, &run_id).await;
    let (status, v) = call(&first, "POST", "/receipt_keys/rotate", Some(json!({}))).await;
    assert_eq!(status, StatusCode::CREATED, "rotate failed: {v}");
    let rotated_key_id = v["key_id"].as_str().unwrap().to_string();
    drop(first);

    let second = build();
    // The stored receipt is served verbatim — the journal's head stands.
    let served = receipt_of(&second, &run_id).await;
    assert_eq!(served, receipt);

    // The history survived: both keys, the rotated one active.
    let (status, v) = call(&second, "GET", "/receipt_keys", None).await;
    assert_eq!(status, StatusCode::OK, "keys failed: {v}");
    assert_eq!(v["keys"].as_array().unwrap().len(), 2);
    assert_eq!(v["active"], json!(rotated_key_id));

    // The pre-restart receipt — signed by the retired key — still
    // verifies against the history.
    let (status, v) = call(
        &second,
        "POST",
        "/receipts/verify",
        Some(json!({
            "snapshot": snapshot_of(&second, &run_id).await,
            "receipt": receipt,
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "restart must not strand receipts: {v}"
    );

    // New mints after the restart sign with the rotated key.
    let new_run = run_pipeline(&second).await;
    let new_receipt = receipt_of(&second, &new_run).await;
    assert_eq!(new_receipt["signer"], json!(rotated_key_id));

    drop(second);
    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Postgres backend (live database required)
//
// Gated on `RUSTY_TEST_DATABASE_URL`; unset skips cleanly. The store path
// is shared across the two builds (the secret files live there — local
// signing with local keys); the database itself is throwaway.
// --------------------------------------------------------------------- //

#[cfg(feature = "postgres")]
mod postgres {
    use super::*;

    fn pg_url() -> Option<String> {
        std::env::var("RUSTY_TEST_DATABASE_URL").ok()
    }

    /// Keys and minted receipts survive a restart on Postgres: the key
    /// history lives in `server_receipt_keys`, the minted receipt in
    /// `server_run_receipts`, and the secret files on the shared store
    /// path — so the second boot signs with the same rotated key and the
    /// pre-restart receipt still verifies.
    #[tokio::test]
    async fn postgres_keys_and_receipts_survive_a_restart() {
        let Some(url) = pg_url() else {
            eprintln!("RUSTY_TEST_DATABASE_URL unset; skipping");
            return;
        };
        let store = temp_store();
        let build = || app_with(store.clone(), |config| config.with_postgres(url.clone()));

        let first = build();
        // The key history is deployment-wide and the scratch database is
        // shared across invocations, so assertions are relative to the
        // history as this run found it.
        let (status, v) = call(&first, "GET", "/receipt_keys", None).await;
        assert_eq!(status, StatusCode::OK, "pg keys baseline failed: {v}");
        let history_before = v["keys"].as_array().unwrap().len();

        let run_id = run_pipeline(&first).await;
        let receipt = receipt_of(&first, &run_id).await;
        let signer = receipt["signer"].as_str().unwrap().to_string();
        let (status, v) = call(&first, "POST", "/receipt_keys/rotate", Some(json!({}))).await;
        assert_eq!(status, StatusCode::CREATED, "pg rotate failed: {v}");
        let rotated_key_id = v["key_id"].as_str().unwrap().to_string();
        drop(first);

        let second = build();
        let served = receipt_of(&second, &run_id).await;
        assert_eq!(served, receipt, "pg: the stored receipt is served verbatim");

        let (status, v) = call(&second, "GET", "/receipt_keys", None).await;
        assert_eq!(status, StatusCode::OK, "pg keys failed: {v}");
        // This run added exactly two records: the key that signed the
        // receipt and the rotated successor.
        let keys = v["keys"].as_array().unwrap();
        assert_eq!(keys.len(), history_before + 2, "pg keys: {v}");
        for key_id in [&signer, &rotated_key_id] {
            assert!(
                keys.iter().any(|record| &record["key_id"] == key_id),
                "pg history must contain {key_id}: {v}"
            );
        }
        assert_eq!(v["active"], json!(rotated_key_id));

        // The pre-restart receipt verifies against the persisted history,
        // with the signer resolved by default from the receipt itself.
        let (status, v) = call(
            &second,
            "POST",
            "/receipts/verify",
            Some(json!({
                "snapshot": snapshot_of(&second, &run_id).await,
                "receipt": receipt,
            })),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "pg: restart must not strand receipts: {v}"
        );
        assert_eq!(v["signer"], json!(signer));

        let new_run = run_pipeline(&second).await;
        let new_receipt = receipt_of(&second, &new_run).await;
        assert_eq!(new_receipt["signer"], json!(rotated_key_id));

        drop(second);
        let _ = std::fs::remove_dir_all(store);
    }
}
