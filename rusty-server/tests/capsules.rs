//! The capsule registry integration tests (R0.9 Rusty Capsules, wave 1):
//! the `/capsules/*` surface over the default JSON-file backend —
//! immutable content-addressed manifests, `(name, version)` pin
//! uniqueness, journaled pin resolution (`capsule_resolved` events),
//! tenant isolation, tamper fail-closed, and restart durability.
//! Live-Postgres coverage of the registry semantics is the gated section
//! at the bottom (`RUSTY_TEST_DATABASE_URL`).
//!
//! Driven in-process via `tower::ServiceExt::oneshot` (no sockets), the
//! `policy.rs` convention. Invocation itself (the capability host) is
//! covered in `rusty-agent-runtime`'s `capsule` integration test; these
//! tests cover the registry plane.

use std::collections::BTreeSet;
use std::path::PathBuf;

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::capsule::{
    CapabilityGrant, CapsuleIdentity, CapsuleInterface, CapsuleManifest, ResourceBudget, WORLD_V1,
};
use rusty_agent_runtime::record::{sha256_hex, Effect};
use rusty_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

// --------------------------------------------------------------------- //
// Harness (the policy.rs convention)
// --------------------------------------------------------------------- //

/// Unique temp store root, removed at the end of each test (best effort).
fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!(
        "rusty-server-capsules-test-{}",
        uuid::Uuid::new_v4()
    ))
}

/// The one graph these tests drive: `pipeline` completes, so its run has
/// a persisted journal the resolution events can join.
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

/// An app over `store` with the config customized by `configure`
/// (tenant keys, Postgres).
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

// --------------------------------------------------------------------- //
// Manifest fixtures (built through core, serialized for the wire)
// --------------------------------------------------------------------- //

/// A valid manifest for `name` at `version`. The build digest names the
/// (imaginary) artifact bytes — the registry stores the declaration; the
/// host is what recomputes digests against real bytes.
fn manifest(name: &str, version: &str) -> CapsuleManifest {
    CapsuleManifest {
        identity: CapsuleIdentity {
            name: name.into(),
            description: None,
        },
        version: version.into(),
        build_digest: sha256_hex(format!("{name}@{version}").as_bytes()),
        interface: CapsuleInterface {
            world: WORLD_V1.into(),
            input_schema: None,
            output_schema: None,
        },
        effects: BTreeSet::from([Effect::ReadOnly]),
        capabilities: BTreeSet::from([
            CapabilityGrant::Clock,
            CapabilityGrant::Network {
                hosts: vec!["api.example".into()],
                protocols: vec!["https".into()],
                methods: vec!["GET".into()],
            },
        ]),
        budget: ResourceBudget {
            fuel: Some(10_000_000),
            ..Default::default()
        },
    }
}

/// Register `m`; asserts 201 and returns the capsule id.
async fn register(app: &Router, m: &CapsuleManifest) -> String {
    register_as(app, None, m).await
}

/// Register `m` with an optional auth header; asserts 201 and returns
/// the capsule id.
async fn register_as(app: &Router, auth: Option<(&str, &str)>, m: &CapsuleManifest) -> String {
    let (status, v) = call_as(
        app,
        auth,
        "POST",
        "/capsules",
        Some(json!({"manifest": serde_json::to_value(m).unwrap()})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "register failed: {v}");
    v["capsule_id"].as_str().unwrap().to_string()
}

/// Create a thread, run `pipeline` to completion; returns the run id.
async fn run_pipeline(app: &Router) -> String {
    let (status, v) = call(app, "POST", "/threads", Some(json!({"graph": "pipeline"}))).await;
    assert_eq!(status, StatusCode::CREATED, "thread failed: {v}");
    let thread_id = v["thread_id"].as_str().unwrap().to_string();
    let (status, v) = call(
        app,
        "POST",
        &format!("/threads/{thread_id}/runs/wait"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "run failed: {v}");
    assert_eq!(v["status"], json!("success"), "pipeline failed: {v}");
    v["run_id"].as_str().unwrap().to_string()
}

/// The run's journaled events (Flight Recorder).
async fn events_of(app: &Router, run_id: &str) -> Vec<Value> {
    let (status, v) = call(app, "GET", &format!("/runs/{run_id}/events"), None).await;
    assert_eq!(status, StatusCode::OK, "events failed: {v}");
    v["events"].as_array().unwrap().clone()
}

// --------------------------------------------------------------------- //
// The registry
// --------------------------------------------------------------------- //

#[tokio::test]
async fn register_converges_conflicts_and_validates() {
    let (app, store) = app();
    let m = manifest("probe", "0.1.0");

    // Create → 201 with the derived content address.
    let capsule_id = register(&app, &m).await;
    assert_eq!(capsule_id, m.capsule_id().unwrap().as_str());

    // Re-registering the identical manifest converges (the idempotent
    // create): 200, same address, original registration instant kept.
    let (status, v) = call(
        &app,
        "POST",
        "/capsules",
        Some(json!({"manifest": serde_json::to_value(&m).unwrap()})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "converge failed: {v}");
    assert_eq!(v["capsule_id"].as_str().unwrap(), capsule_id);

    // A changed declaration under the same (name, version) pin: the pin
    // is claimed — 409, registry immutability.
    let mut changed = m.clone();
    changed.identity.description = Some("a different declaration".into());
    let (status, v) = call(
        &app,
        "POST",
        "/capsules",
        Some(json!({"manifest": serde_json::to_value(&changed).unwrap()})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "pin conflict expected: {v}");

    // …while the same change under a *new* version registers fine.
    let mut v2 = changed.clone();
    v2.version = "0.2.0".into();
    v2.build_digest = sha256_hex(b"probe@0.2.0");
    register(&app, &v2).await;

    // An invalid manifest (no declared effects) fails validation: 422,
    // nothing stored.
    let mut invalid = manifest("invalid", "0.1.0");
    invalid.effects = BTreeSet::new();
    let (status, v) = call(
        &app,
        "POST",
        "/capsules",
        Some(json!({"manifest": serde_json::to_value(&invalid).unwrap()})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "422 expected: {v}"
    );
    let (status, _) = call(&app, "GET", "/capsules", None).await;
    assert_eq!(status, StatusCode::OK);

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn list_and_get_are_tenant_scoped() {
    let store = temp_store();
    let acme = Some(("x-api-key", "acme-secret"));
    let globex = Some(("x-api-key", "globex-secret"));
    let app = app_with(store.clone(), |config| {
        config
            .with_tenant_key("acme", "acme-secret")
            .with_tenant_key("globex", "globex-secret")
    });

    let acme_id = register_as(&app, acme, &manifest("acme-probe", "0.1.0")).await;
    let globex_id = register_as(&app, globex, &manifest("globex-probe", "0.1.0")).await;

    // Each tenant lists exactly its own manifests.
    let (status, v) = call_as(&app, acme, "GET", "/capsules", None).await;
    assert_eq!(status, StatusCode::OK, "acme list failed: {v}");
    let listed = v["capsules"].as_array().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["capsule_id"].as_str().unwrap(), acme_id);
    let (status, v) = call_as(&app, globex, "GET", "/capsules", None).await;
    assert_eq!(status, StatusCode::OK, "globex list failed: {v}");
    let listed = v["capsules"].as_array().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["capsule_id"].as_str().unwrap(), globex_id);

    // Cross-tenant reads are indistinguishable from unknown ids: 404.
    let (status, _) = call_as(&app, globex, "GET", &format!("/capsules/{acme_id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call_as(&app, acme, "GET", &format!("/capsules/{globex_id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Own-tenant reads serve the full record.
    let (status, v) = call_as(&app, acme, "GET", &format!("/capsules/{acme_id}"), None).await;
    assert_eq!(status, StatusCode::OK, "acme get failed: {v}");
    assert_eq!(
        v["record"]["manifest"]["identity"]["name"],
        json!("acme-probe")
    );

    // Cross-tenant *pins* are equally invisible: the two tenants can
    // claim the same (name, version) pin independently.
    register_as(&app, acme, &manifest("shared", "1.0.0")).await;
    register_as(&app, globex, &manifest("shared", "1.0.0")).await;

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn resolve_journals_one_resolution_per_pin() {
    let (app, store) = app();
    let m = manifest("probe", "0.1.0");
    let capsule_id = register(&app, &m).await;
    let run_id = run_pipeline(&app).await;

    let (status, v) = call(
        &app,
        "POST",
        "/capsules/resolve",
        Some(json!({
            "pins": {"probe": "0.1.0"},
            "run_id": run_id,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "resolve failed: {v}");
    let resolutions = v["resolutions"].as_array().unwrap();
    assert_eq!(resolutions.len(), 1);
    assert_eq!(resolutions[0]["name"], json!("probe"));
    assert_eq!(resolutions[0]["version"], json!("0.1.0"));
    assert_eq!(resolutions[0]["capsule_id"], json!(capsule_id));
    assert_eq!(resolutions[0]["build_digest"], json!(m.build_digest));

    // The resolution is journaled into the resolving run: one
    // `capsule_resolved` event naming the version and the content
    // address — the link from the checkpoint's version-string pin to
    // the address the host will admit.
    let resolved: Vec<Value> = events_of(&app, &run_id)
        .await
        .into_iter()
        .filter(|event| event["kind"] == json!("capsule_resolved"))
        .collect();
    assert_eq!(resolved.len(), 1, "one pin, one resolution event");
    assert_eq!(resolved[0]["effect"], json!("read_only"));
    // Payloads carry the adjacent-tagged payload-ref envelope; the
    // CapsuleResolution is the inline value.
    let out = &resolved[0]["output"]["value"];
    assert_eq!(out["name"], json!("probe"));
    assert_eq!(out["version"], json!("0.1.0"));
    assert_eq!(out["capsule_id"], json!(capsule_id));

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn resolve_unknown_pin_and_unresolvable_run_fail() {
    let (app, store) = app();
    register(&app, &manifest("probe", "0.1.0")).await;
    let run_id = run_pipeline(&app).await;

    // A pin the registry never heard of: 404 naming the pin.
    let (status, v) = call(
        &app,
        "POST",
        "/capsules/resolve",
        Some(json!({
            "pins": {"ghost": "9.9.9"},
            "run_id": run_id,
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "unknown pin expected 404: {v}"
    );

    // A run that does not resolve: the journaled resolution is
    // hard-fail, so the request fails (404) rather than resolving
    // unattributably.
    let (status, v) = call(
        &app,
        "POST",
        "/capsules/resolve",
        Some(json!({
            "pins": {"probe": "0.1.0"},
            "run_id": "run-does-not-exist",
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "unresolvable run expected 404: {v}"
    );

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn tampered_record_fails_closed_at_resolution() {
    let (app, store) = app();
    let m = manifest("probe", "0.1.0");
    let capsule_id = register(&app, &m).await;
    let run_id = run_pipeline(&app).await;

    // Corrupt the on-disk record: the stored manifest no longer matches
    // the address it is filed under.
    let path = store
        .join("capsules/manifests")
        .join(format!("{capsule_id}.json"));
    let mut stored: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    stored["manifest"]["identity"]["description"] = json!("hand-edited after admission");
    std::fs::write(&path, stored.to_string()).unwrap();
    drop(app);

    // A fresh app over the tampered store resolves nothing: the
    // re-derived address no longer matches the key — 422, fail closed.
    let app = app_with(store.clone(), |config| config);
    let (status, v) = call(
        &app,
        "POST",
        "/capsules/resolve",
        Some(json!({
            "pins": {"probe": "0.1.0"},
            "run_id": run_id,
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "tampered record must fail closed: {v}"
    );
    // And no resolution evidence was journaled for the refused pin.
    let resolved: Vec<Value> = events_of(&app, &run_id)
        .await
        .into_iter()
        .filter(|event| event["kind"] == json!("capsule_resolved"))
        .collect();
    assert!(resolved.is_empty());

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn registry_survives_a_restart() {
    let store = temp_store();
    let first = app_with(store.clone(), |config| config);
    let m = manifest("probe", "0.1.0");
    let capsule_id = register(&first, &m).await;
    register(&first, &manifest("probe", "0.2.0")).await;
    drop(first);

    // A fresh app over the same store serves the settled registry.
    let second = app_with(store.clone(), |config| config);
    let (status, v) = call(&second, "GET", &format!("/capsules/{capsule_id}"), None).await;
    assert_eq!(status, StatusCode::OK, "get after restart: {v}");
    assert_eq!(v["record"]["manifest"]["version"], json!("0.1.0"));
    let (status, v) = call(&second, "GET", "/capsules", None).await;
    assert_eq!(status, StatusCode::OK, "list after restart: {v}");
    assert_eq!(v["capsules"].as_array().unwrap().len(), 2);

    // …and convergence still holds: re-registering the same manifest
    // against the reloaded index is the idempotent create, not a
    // duplicate.
    let (status, v) = call(
        &second,
        "POST",
        "/capsules",
        Some(json!({"manifest": serde_json::to_value(&m).unwrap()})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "converge after restart: {v}");
    assert_eq!(v["capsule_id"].as_str().unwrap(), capsule_id);

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Postgres backend (live database required)
//
// Gated on `RUSTY_TEST_DATABASE_URL`; unset skips cleanly so the suite is
// green without a database. Every test uses a dedicated tenant, so
// repeated runs against one scratch database never interfere; the
// database itself is throwaway.
// --------------------------------------------------------------------- //

#[cfg(feature = "postgres")]
mod postgres {
    use super::*;

    fn pg_url() -> Option<String> {
        std::env::var("RUSTY_TEST_DATABASE_URL").ok()
    }

    /// The wave-1 registry on Postgres: register, converge, pin-conflict,
    /// journaled resolution — surviving a restart.
    #[tokio::test]
    async fn postgres_registry_and_resolution_survive_a_restart() {
        let Some(url) = pg_url() else {
            eprintln!("RUSTY_TEST_DATABASE_URL unset; skipping");
            return;
        };
        let tenant = format!("capsulespg-{}", uuid::Uuid::new_v4());
        let auth = Some(("x-api-key", "pg-secret"));
        let build = || {
            app_with(temp_store(), |config| {
                config
                    .with_postgres(url.clone())
                    .with_tenant_key(tenant.clone(), "pg-secret")
            })
        };

        let m = manifest("pg-probe", "0.1.0");
        let first = build();
        let capsule_id = register_as(&first, auth, &m).await;

        // Converge (idempotent create) and the pin conflict both map.
        let (status, v) = call_as(
            &first,
            auth,
            "POST",
            "/capsules",
            Some(json!({"manifest": serde_json::to_value(&m).unwrap()})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "pg converge failed: {v}");
        let mut changed = m.clone();
        changed.identity.description = Some("different declaration".into());
        let (status, v) = call_as(
            &first,
            auth,
            "POST",
            "/capsules",
            Some(json!({"manifest": serde_json::to_value(&changed).unwrap()})),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "pg pin conflict expected: {v}"
        );

        // A journaled resolution needs a completed run in this tenant.
        let (status, v) = call_as(
            &first,
            auth,
            "POST",
            "/threads",
            Some(json!({"graph": "pipeline"})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "pg thread failed: {v}");
        let thread_id = v["thread_id"].as_str().unwrap().to_string();
        let (status, v) = call_as(
            &first,
            auth,
            "POST",
            &format!("/threads/{thread_id}/runs/wait"),
            Some(json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "pg run failed: {v}");
        let run_id = v["run_id"].as_str().unwrap().to_string();
        let (status, v) = call_as(
            &first,
            auth,
            "POST",
            "/capsules/resolve",
            Some(json!({
                "pins": {"pg-probe": "0.1.0"},
                "run_id": run_id,
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "pg resolve failed: {v}");
        assert_eq!(
            v["resolutions"][0]["capsule_id"].as_str().unwrap(),
            capsule_id
        );
        drop(first);

        // A fresh app over the same database serves the settled registry.
        let second = build();
        let (status, v) = call_as(&second, auth, "GET", "/capsules", None).await;
        assert_eq!(status, StatusCode::OK, "pg list after restart: {v}");
        let listed = v["capsules"].as_array().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["capsule_id"].as_str().unwrap(), capsule_id);
        let (status, v) = call_as(
            &second,
            auth,
            "GET",
            &format!("/capsules/{capsule_id}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "pg get after restart: {v}");
        assert_eq!(
            v["record"]["manifest"]["identity"]["name"],
            json!("pg-probe")
        );
    }
}
