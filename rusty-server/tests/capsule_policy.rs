//! The capsule authorization plane integration tests (R0.9 Rusty
//! Capsules, wave 2): Cedar policy registration and activation, the
//! admission composition at resolution (policy refusals with journaled
//! denials, overlay narrowing, budget clamp-and-refuse), revocation at
//! the next capability use through the plane's `GrantRecheck` seam,
//! tenant isolation, and — without the `capsules` feature — the typed
//! `503 capsule_policy_unavailable` every new route answers while the
//! wave-1 surface behaves exactly as before.
//!
//! Driven in-process via `tower::ServiceExt::oneshot` (the `capsules.rs`
//! convention). Live-Postgres coverage is the gated section at the
//! bottom (`RUSTY_TEST_DATABASE_URL`).

use std::collections::BTreeSet;
use std::path::PathBuf;

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::capsule::{
    CapabilityGrant, CapsuleIdentity, CapsuleInterface, CapsuleManifest, ResourceBudget, WORLD_V1,
};
#[cfg(feature = "capsules")]
use rusty_agent_runtime::capsule::{CapsuleOverlay, FilesystemMode};
use rusty_agent_runtime::record::{sha256_hex, Effect};
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

// --------------------------------------------------------------------- //
// Harness (the capsules.rs convention)
// --------------------------------------------------------------------- //

/// Unique temp store root, removed at the end of each test (best effort).
fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!(
        "rusty-server-capsule-policy-test-{}",
        uuid::Uuid::new_v4()
    ))
}

/// The one graph these tests drive: `pipeline` completes, so its run has
/// a persisted journal the admission events can join.
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

/// A valid manifest for `name` at `version`: Clock plus one scoped
/// network grant, fuel-budgeted — the shape every wave-2 check narrows
/// or forbids from.
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
    let (status, v) = call(
        app,
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
// Without the feature: the plane's routes refuse, typed
// --------------------------------------------------------------------- //

#[cfg(not(feature = "capsules"))]
mod without_feature {
    use super::*;

    #[tokio::test]
    async fn plane_routes_answer_503_and_wave1_routes_are_untouched() {
        let (app, store) = app();

        // Every wave-2 route refuses with the typed 503, naming the
        // remedy — fail closed, never a silent skip.
        for (method, uri) in [
            ("POST", "/capsule_policies/versions"),
            ("GET", "/capsule_policies/versions"),
            ("GET", "/capsule_policies/versions/cedar-anything"),
            ("GET", "/capsule_policies/active"),
            ("POST", "/capsule_policies/active"),
            ("POST", "/capsules/overlays"),
            ("GET", "/capsules/overlays"),
            ("GET", "/capsules/overlays/ceiling"),
        ] {
            let (status, v) = call(&app, method, uri, None).await;
            assert_eq!(
                status,
                StatusCode::SERVICE_UNAVAILABLE,
                "{method} {uri} must refuse: {v}"
            );
            assert_eq!(
                v["error"],
                json!("capsule_policy_unavailable"),
                "{method} {uri}"
            );
        }

        // The wave-1 surface behaves exactly as before: register,
        // resolve, and the journaled resolution.
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
        assert_eq!(status, StatusCode::OK, "wave-1 resolve failed: {v}");
        assert_eq!(v["resolutions"][0]["capsule_id"], json!(capsule_id));
        let resolved: Vec<Value> = events_of(&app, &run_id)
            .await
            .into_iter()
            .filter(|event| event["kind"] == json!("capsule_resolved"))
            .collect();
        assert_eq!(resolved.len(), 1, "the wave-1 journal shape is unchanged");

        let _ = std::fs::remove_dir_all(store);
    }
}

// --------------------------------------------------------------------- //
// With the feature: the authorization plane
// --------------------------------------------------------------------- //

#[cfg(feature = "capsules")]
mod with_feature {
    use super::*;
    use rusty_agent_runtime::capsule_host::{CapsuleHost, CapsuleInvocation};
    use rusty_agent_runtime::journal::{Clock, Journal};
    use rusty_agent_runtime::record::{PayloadRef, RunEventKind};
    use rusty_agent_server::capsule_policy::CapsulePolicyPlane;
    use std::sync::Arc;

    /// The permissive policy: admit everything, permit every use and
    /// attach. Activation of this body is what arms the plane.
    pub(crate) const PERMIT_ALL: &str = "permit(principal, action, resource);";

    /// Forbid the network capability outright (decision 2).
    const FORBID_NETWORK: &str = r#"
        permit(principal, action, resource);
        forbid(principal, action == Action::"UseCapability", resource)
            when { context.kind == "network" };"#;

    /// Forbid the clock capability (the revocation test's v2).
    const FORBID_CLOCK: &str = r#"
        permit(principal, action, resource);
        forbid(principal, action == Action::"UseCapability", resource)
            when { context.kind == "clock" };"#;

    /// Forbid any overlay the structural check flags as widening
    /// (decision 3).
    const FORBID_WIDENING_OVERLAYS: &str = r#"
        permit(principal, action, resource);
        forbid(principal, action == Action::"AttachOverlay", resource)
            when { context.widens };"#;

    /// Register `text`; asserts 201 and returns its version.
    async fn register_policy(app: &Router, text: &str) -> String {
        let (status, v) = call(
            app,
            "POST",
            "/capsule_policies/versions",
            Some(json!({"policy_text": text})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "register policy failed: {v}");
        v["version"].as_str().unwrap().to_string()
    }

    /// Move the default tenant's pointer to `version`; asserts 200.
    async fn activate(app: &Router, version: &str) {
        let (status, v) = call(
            app,
            "POST",
            "/capsule_policies/active",
            Some(json!({"version": version})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "activate failed: {v}");
        assert_eq!(v["active"], json!(version));
    }

    /// An overlay ceiling, optionally targeted.
    pub(crate) fn overlay(
        name: &str,
        targets: Option<Vec<String>>,
        capabilities: BTreeSet<CapabilityGrant>,
    ) -> CapsuleOverlay {
        CapsuleOverlay {
            name: name.into(),
            targets,
            capabilities,
            note: None,
        }
    }

    /// Attach `o`; returns the status and body for the caller to assert.
    async fn attach(app: &Router, o: &CapsuleOverlay) -> (StatusCode, Value) {
        call(
            app,
            "POST",
            "/capsules/overlays",
            Some(json!({"overlay": serde_json::to_value(o).unwrap()})),
        )
        .await
    }

    // --- Policy versioning and the active pointer ------------------- //

    #[tokio::test]
    async fn policies_version_converge_conflict_and_activate() {
        let (app, store) = app();

        // The unconfigured posture: no active policy, admission the
        // wave-1 way — the pointer route says so with a 404.
        let (status, v) = call(&app, "GET", "/capsule_policies/active", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "unconfigured posture: {v}");

        // Register → 201 with the content-derived version; the idempotent
        // re-create converges (200, same version).
        let version = register_policy(&app, PERMIT_ALL).await;
        assert!(version.starts_with("cedar-"), "derived version: {version}");
        let (status, v) = call(
            &app,
            "POST",
            "/capsule_policies/versions",
            Some(json!({"policy_text": PERMIT_ALL})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "converge failed: {v}");
        assert_eq!(v["version"], json!(version));

        // Same version naming different text: 409 — a version string is a
        // commitment to one exact policy set.
        let (status, v) = call(
            &app,
            "POST",
            "/capsule_policies/versions",
            Some(json!({"policy_text": FORBID_NETWORK, "version": version})),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "conflict expected: {v}");

        // Text Cedar cannot parse: 422, nothing registered.
        let (status, v) = call(
            &app,
            "POST",
            "/capsule_policies/versions",
            Some(json!({"policy_text": "this is not cedar"})),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "unparseable policy expected 422: {v}"
        );

        // Activating an unregistered version: 422 — a mistyped activation
        // can never silently un-arm the plane.
        let (status, v) = call(
            &app,
            "POST",
            "/capsule_policies/active",
            Some(json!({"version": "cedar-nonexistent"})),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "unregistered activation expected 422: {v}"
        );

        // Activate for real: the pointer serves, the registry reads serve.
        activate(&app, &version).await;
        let (status, v) = call(&app, "GET", "/capsule_policies/active", None).await;
        assert_eq!(status, StatusCode::OK, "active read failed: {v}");
        assert_eq!(v["version"], json!(version));
        let (status, v) = call(
            &app,
            "GET",
            &format!("/capsule_policies/versions/{version}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "get version failed: {v}");
        assert_eq!(v["record"]["policy_text"], json!(PERMIT_ALL));
        let (status, v) = call(&app, "GET", "/capsule_policies/versions", None).await;
        assert_eq!(status, StatusCode::OK, "list failed: {v}");
        assert_eq!(v["policies"].as_array().unwrap().len(), 1);

        // …and the resolution pins the deciding version: the evidence
        // requirement (which version decided each admission).
        register(&app, &manifest("probe", "0.1.0")).await;
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
        assert_eq!(v["resolutions"][0]["policy_version"], json!(version));

        let _ = std::fs::remove_dir_all(store);
    }

    // --- Registration and resolution refusals ----------------------- //

    #[tokio::test]
    async fn forbidden_grants_refuse_at_register_and_resolve() {
        let (app, store) = app();

        // Registered in the unconfigured posture: the wave-1 way, 201.
        let m = manifest("probe", "0.1.0");
        register(&app, &m).await;

        // Arm the plane with a network-forbidding policy.
        let version = register_policy(&app, FORBID_NETWORK).await;
        activate(&app, &version).await;

        // Resolution now refuses (403) and journals one capsule_denied
        // per forbidden grant, pinned to the deciding version.
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
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "policy refusal expected: {v}"
        );
        let denials: Vec<Value> = events_of(&app, &run_id)
            .await
            .into_iter()
            .filter(|event| event["kind"] == json!("capsule_denied"))
            .collect();
        assert_eq!(denials.len(), 1, "one forbidden grant, one denial");
        let out = &denials[0]["output"]["value"];
        assert_eq!(out["capability"], json!("network"));
        assert_eq!(out["policy_version"], json!(version));
        // …and no resolution was journaled for the refused admission.
        let resolved: Vec<Value> = events_of(&app, &run_id)
            .await
            .into_iter()
            .filter(|event| event["kind"] == json!("capsule_resolved"))
            .collect();
        assert!(resolved.is_empty());

        // Registration refuses too: a network-declaring manifest is 403…
        let (status, v) = call(
            &app,
            "POST",
            "/capsules",
            Some(json!({"manifest": serde_json::to_value(manifest("late", "0.1.0")).unwrap()})),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "register refusal expected: {v}"
        );

        // …while declaring fewer grants than policy permits is always
        // fine: clock-only registers 201.
        let mut narrow = manifest("narrow", "0.1.0");
        narrow.capabilities = BTreeSet::from([CapabilityGrant::Clock]);
        register(&app, &narrow).await;

        let _ = std::fs::remove_dir_all(store);
    }

    // --- Overlays: legality at attach, narrowing at resolve ---------- //

    #[tokio::test]
    async fn widening_overlays_refuse_and_narrowing_always_applies() {
        let (app, store) = app();
        let m = manifest("probe", "0.1.0");
        register(&app, &m).await;
        let version = register_policy(&app, FORBID_WIDENING_OVERLAYS).await;
        activate(&app, &version).await;

        // A widening overlay (a host the manifest never declared) is
        // refused at attach: 403, nothing stored.
        let widening = overlay(
            "widen-egress",
            None,
            BTreeSet::from([CapabilityGrant::Network {
                hosts: vec!["evil.example".into()],
                protocols: vec!["https".into()],
                methods: vec!["GET".into()],
            }]),
        );
        let (status, v) = attach(&app, &widening).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "widening attach expected 403: {v}"
        );
        let (status, v) = call(&app, "GET", "/capsules/overlays", None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(v["overlays"].as_array().unwrap().is_empty());

        // A narrowing overlay attaches (201), is readable by name, and
        // narrows the resolution's effective grants to the intersection.
        let narrowing = overlay(
            "clock-only",
            Some(vec!["probe".into()]),
            BTreeSet::from([CapabilityGrant::Clock]),
        );
        let (status, v) = attach(&app, &narrowing).await;
        assert_eq!(status, StatusCode::CREATED, "narrowing attach failed: {v}");
        assert_eq!(v["overlay"]["author"], json!("default"));
        let (status, v) = call(&app, "GET", "/capsules/overlays/clock-only", None).await;
        assert_eq!(status, StatusCode::OK, "get overlay failed: {v}");
        assert_eq!(v["overlay"]["overlay"]["name"], json!("clock-only"));

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
        let resolution = &v["resolutions"][0];
        assert_eq!(resolution["overlays"], json!(["clock-only"]));
        let effective = resolution["effective_grants"].as_array().unwrap();
        assert_eq!(effective.len(), 1);
        assert_eq!(effective[0]["kind"], json!("clock"));
        // The journaled resolution carries the same narrowing.
        let resolved: Vec<Value> = events_of(&app, &run_id)
            .await
            .into_iter()
            .filter(|event| event["kind"] == json!("capsule_resolved"))
            .collect();
        assert_eq!(resolved.len(), 1);
        assert_eq!(
            resolved[0]["output"]["value"]["overlays"],
            json!(["clock-only"])
        );
        drop(app);

        // The double enforcement's arithmetic half: over a *fresh* store
        // with no policy at all (Cedar silent — the unconfigured
        // posture), a hand-attached overlay still cannot widen; the
        // intersection is the enforcement, not the policy.
        let (plain, plain_store) = super::app();
        register(&plain, &manifest("probe", "0.1.0")).await;
        let crafty = overlay(
            "hand-crafted",
            None,
            BTreeSet::from([
                CapabilityGrant::Clock,
                CapabilityGrant::Filesystem {
                    paths: vec!["/etc".into()],
                    mode: FilesystemMode::Read,
                },
            ]),
        );
        let (status, v) = attach(&plain, &crafty).await;
        assert_eq!(status, StatusCode::CREATED, "unconfigured attach: {v}");
        let run_id = run_pipeline(&plain).await;
        let (status, v) = call(
            &plain,
            "POST",
            "/capsules/resolve",
            Some(json!({
                "pins": {"probe": "0.1.0"},
                "run_id": run_id,
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "resolve failed: {v}");
        let effective = v["resolutions"][0]["effective_grants"].as_array().unwrap();
        // The filesystem ceiling grant is beyond the manifest's set —
        // intersection drops it; only the shared clock grant survives.
        assert_eq!(effective.len(), 1);
        assert_eq!(effective[0]["kind"], json!("clock"));

        let _ = std::fs::remove_dir_all(store);
        let _ = std::fs::remove_dir_all(plain_store);
    }

    // --- Budget composition: clamp the local, refuse the accounting -- //

    #[tokio::test]
    async fn budgets_clamp_fuel_and_refuse_cost_overspend() {
        let store = temp_store();
        // The tenant ceiling is the outermost layer.
        let app = app_with(store.clone(), |config| {
            config.with_capsule_budget_ceiling(ResourceBudget {
                fuel: Some(8_000_000),
                max_cost_usd: Some(1.0),
                ..Default::default()
            })
        });
        let mut m = manifest("probe", "0.1.0");
        m.budget.max_cost_usd = Some(0.5);
        register(&app, &m).await;
        let run_id = run_pipeline(&app).await;

        // Fuel clamps field-wise to the tightest enclosing bound (the
        // run's 5M under the ceiling's 8M under the declared 10M), and
        // the clamp is journaled on the resolution.
        let (status, v) = call(
            &app,
            "POST",
            "/capsules/resolve",
            Some(json!({
                "pins": {"probe": "0.1.0"},
                "run_id": run_id,
                "budget": {"fuel": 5_000_000},
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "resolve failed: {v}");
        let clamped = &v["resolutions"][0]["clamped_budget"];
        assert_eq!(clamped["fuel"], json!(5_000_000));
        let resolved: Vec<Value> = events_of(&app, &run_id)
            .await
            .into_iter()
            .filter(|event| event["kind"] == json!("capsule_resolved"))
            .collect();
        assert_eq!(resolved.len(), 1);
        assert_eq!(
            resolved[0]["output"]["value"]["clamped_budget"]["fuel"],
            json!(5_000_000)
        );

        // A manifest declaring more cost than the ceiling is refused
        // outright: 422 — token and cost bounds cannot be retrofitted
        // mid-run, so admission refuses the overspend.
        let mut spendy = manifest("spendy", "0.1.0");
        spendy.budget.max_cost_usd = Some(2.0);
        register(&app, &spendy).await;
        let (status, v) = call(
            &app,
            "POST",
            "/capsules/resolve",
            Some(json!({
                "pins": {"spendy": "0.1.0"},
                "run_id": run_id,
            })),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "cost overspend expected 422: {v}"
        );
        assert!(v["message"].as_str().unwrap().contains("max_cost_usd"));

        let _ = std::fs::remove_dir_all(store);
    }

    // --- Revocation at the next use ---------------------------------- //

    /// The clock guest from core's capsule integration test: imports
    /// `rusty:capsule/clock@0.1.0`'s `now-millis`, calls it once.
    fn clock_guest_wat() -> String {
        let realloc = r#"
    (global $heap (mut i32) (i32.const 1024))
    (func (export "realloc") (param $old i32) (param $old_size i32) (param $align i32) (param $new_size i32) (result i32)
      (local $ptr i32)
      (global.set $heap
        (i32.and
          (i32.add (global.get $heap) (i32.sub (local.get $align) (i32.const 1)))
          (i32.sub (i32.const 0) (local.get $align))))
      (local.set $ptr (global.get $heap))
      (global.set $heap (i32.add (global.get $heap) (local.get $new_size)))
      (local.get $ptr))"#;
        let write_result = r#"
      (i32.store8 (i32.const 512) (local.get $disc))
      (i32.store (i32.const 516) (local.get $ptr))
      (i32.store (i32.const 520) (local.get $len))
      (i32.const 512)"#;
        format!(
            r#"(component
  (import "rusty:capsule/clock@0.1.0" (instance $clock
    (export "now-millis" (func (result u64)))))
  (alias export $clock "now-millis" (func $now))

  (core module $libc
    (memory (export "memory") 1)
    {realloc}
    (data (i32.const 16) "{{\"clock\":true}}"))
  (core instance $libc_i (instantiate $libc))

  (core func $now_lowered (canon lower (func $now)
    (memory (core memory $libc_i "memory"))
    (realloc (core func $libc_i "realloc"))))

  (core module $guest
    (import "libc" "memory" (memory 1))
    (import "clock" "now_millis" (func $now (result i64)))
    (func (export "run") (param $in_ptr i32) (param $in_len i32) (result i32)
      (local $disc i32) (local $ptr i32) (local $len i32)
      (drop (call $now))
      (local.set $disc (i32.const 0))
      (local.set $ptr (i32.const 16))
      (local.set $len (i32.const 14))
      {write_result}))
  (core instance $guest_i (instantiate $guest
    (with "libc" (instance (export "memory" (memory $libc_i "memory"))))
    (with "clock" (instance (export "now_millis" (func $now_lowered))))))

  (func $run (param "input" string) (result (result string (error string)))
    (canon lift (core func $guest_i "run")
      (memory (core memory $libc_i "memory"))
      (realloc (core func $libc_i "realloc"))))
  (export "run" (func $run)))"#
        )
    }

    #[tokio::test]
    async fn revoked_grant_fails_at_its_next_use() {
        let (app, store) = app();

        // The embedder's plane: the server has no invocation route, so a
        // deployment builds its own hosts and plugs the plane's
        // rechecker into them — exercised here directly.
        let plane = Arc::new(CapsulePolicyPlane::new());
        plane.install("default", "v1-permit", PERMIT_ALL).unwrap();

        // A clock capsule admitted under v1: granted, and its first use
        // succeeds against the permissive engine.
        let wat = clock_guest_wat();
        let mut m = manifest("clockwork", "0.1.0");
        m.build_digest = sha256_hex(wat.as_bytes());
        m.capabilities = BTreeSet::from([CapabilityGrant::Clock]);
        let host = CapsuleHost::from_bytes(m, wat.as_bytes())
            .unwrap()
            .with_clock(|| 1_750_000_000_000)
            .with_grant_recheck(plane.rechecker("default"));
        let journal = Journal::new("run-revocation", "thread-revocation", Clock::System);
        host.invoke(CapsuleInvocation::new(json!({})).with_journal(journal.clone(), None))
            .await
            .expect("admitted under a permissive policy, the first use succeeds");

        // Policy moves: v2 forbids the clock. Registration and
        // activation travel the routes; the embedder's plane re-installs
        // (what the activation route's eager refresh does in-process).
        let version = register_policy(&app, FORBID_CLOCK).await;
        activate(&app, &version).await;
        plane.install("default", &version, FORBID_CLOCK).unwrap();

        // The next use is denied — revocation lands without re-admission.
        // The clock import's guest-visible signature has no error
        // channel, so the denial arrives as a trap; the journaled
        // denial below is the evidence, pinned to the *new* version.
        let err = host
            .invoke(CapsuleInvocation::new(json!({})).with_journal(journal.clone(), None))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("trapped"), "got: {err}");
        let denial = journal
            .events()
            .iter()
            .rev()
            .find(|event| event.kind == RunEventKind::CapsuleDenied)
            .expect("the revocation is journaled")
            .output
            .as_ref()
            .and_then(|payload| match payload {
                PayloadRef::Inline(value) => Some(value.clone()),
                PayloadRef::Artifact(_) => None,
            })
            .expect("denial payloads travel inline");
        assert_eq!(denial["capability"], json!("clock"));
        assert_eq!(denial["policy_version"], json!(version));

        let _ = std::fs::remove_dir_all(store);
    }

    // --- Tenant isolation -------------------------------------------- //

    #[tokio::test]
    async fn policies_and_overlays_are_tenant_scoped() {
        let store = temp_store();
        let acme = Some(("x-api-key", "acme-secret"));
        let globex = Some(("x-api-key", "globex-secret"));
        let app = app_with(store.clone(), |config| {
            config
                .with_tenant_key("acme", "acme-secret")
                .with_tenant_key("globex", "globex-secret")
        });

        // Acme registers and activates; globex's plane stays
        // unconfigured — its active read is the 404 posture.
        let (status, v) = call_as(
            &app,
            acme,
            "POST",
            "/capsule_policies/versions",
            Some(json!({"policy_text": PERMIT_ALL})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "acme register failed: {v}");
        let acme_version = v["version"].as_str().unwrap().to_string();
        let (status, v) = call_as(
            &app,
            acme,
            "POST",
            "/capsule_policies/active",
            Some(json!({"version": acme_version})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "acme activate failed: {v}");
        let (status, _) = call_as(&app, globex, "GET", "/capsule_policies/active", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "globex is unconfigured");
        // Cross-tenant version reads are indistinguishable from unknown.
        let (status, _) = call_as(
            &app,
            globex,
            "GET",
            &format!("/capsule_policies/versions/{acme_version}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, v) = call_as(&app, globex, "GET", "/capsule_policies/versions", None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(v["policies"].as_array().unwrap().is_empty());

        // Overlays isolate the same way: acme's ceiling is invisible to
        // globex, and both tenants can claim the same overlay name.
        let ceiling = overlay("ceiling", None, BTreeSet::from([CapabilityGrant::Clock]));
        let (status, v) = call_as(
            &app,
            acme,
            "POST",
            "/capsules/overlays",
            Some(json!({"overlay": serde_json::to_value(&ceiling).unwrap()})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "acme attach failed: {v}");
        assert_eq!(v["overlay"]["author"], json!("acme"));
        let (status, v) = call_as(&app, globex, "GET", "/capsules/overlays", None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(v["overlays"].as_array().unwrap().is_empty());
        let (status, _) = call_as(&app, globex, "GET", "/capsules/overlays/ceiling", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, v) = call_as(
            &app,
            globex,
            "POST",
            "/capsules/overlays",
            Some(json!({"overlay": serde_json::to_value(&ceiling).unwrap()})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "globex attach failed: {v}");
        assert_eq!(v["overlay"]["author"], json!("globex"));

        let _ = std::fs::remove_dir_all(store);
    }

    // --- Durability: the pointer and the ceilings survive a restart -- //

    #[tokio::test]
    async fn plane_state_survives_a_restart() {
        let store = temp_store();
        let first = app_with(store.clone(), |config| config);
        let version = register_policy(&first, PERMIT_ALL).await;
        activate(&first, &version).await;
        let ceiling = overlay("ceiling", None, BTreeSet::from([CapabilityGrant::Clock]));
        let (status, v) = attach(&first, &ceiling).await;
        assert_eq!(status, StatusCode::CREATED, "attach failed: {v}");
        drop(first);

        // A fresh app over the same store serves the settled plane.
        let second = app_with(store.clone(), |config| config);
        let (status, v) = call(&second, "GET", "/capsule_policies/active", None).await;
        assert_eq!(status, StatusCode::OK, "active after restart: {v}");
        assert_eq!(v["version"], json!(version));
        let (status, v) = call(&second, "GET", "/capsules/overlays", None).await;
        assert_eq!(status, StatusCode::OK, "overlays after restart: {v}");
        assert_eq!(v["overlays"].as_array().unwrap().len(), 1);

        let _ = std::fs::remove_dir_all(store);
    }
}

// --------------------------------------------------------------------- //
// Postgres backend (live database required)
//
// Gated on `RUSTY_TEST_DATABASE_URL`; unset skips cleanly. Dedicated
// tenants per run, so repeats against one scratch database never
// interfere; the database itself is throwaway.
// --------------------------------------------------------------------- //

#[cfg(all(feature = "capsules", feature = "postgres"))]
mod postgres {
    use super::with_feature::{overlay, PERMIT_ALL};
    use super::*;

    fn pg_url() -> Option<String> {
        std::env::var("RUSTY_TEST_DATABASE_URL").ok()
    }

    /// The plane on Postgres: policy registration, the active pointer,
    /// and the overlay ceilings survive a restart.
    #[tokio::test]
    async fn postgres_plane_survives_a_restart() {
        let Some(url) = pg_url() else {
            eprintln!("RUSTY_TEST_DATABASE_URL unset; skipping");
            return;
        };
        let tenant = format!("cappolpg-{}", uuid::Uuid::new_v4());
        let auth = Some(("x-api-key", "pg-secret"));
        let build = || {
            app_with(temp_store(), |config| {
                config
                    .with_postgres(url.clone())
                    .with_tenant_key(tenant.clone(), "pg-secret")
            })
        };

        let first = build();
        let (status, v) = call_as(
            &first,
            auth,
            "POST",
            "/capsule_policies/versions",
            Some(json!({"policy_text": PERMIT_ALL})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "pg register failed: {v}");
        let version = v["version"].as_str().unwrap().to_string();
        // The idempotent re-create converges on Postgres too.
        let (status, _) = call_as(
            &first,
            auth,
            "POST",
            "/capsule_policies/versions",
            Some(json!({"policy_text": PERMIT_ALL})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "pg converge failed");
        let (status, v) = call_as(
            &first,
            auth,
            "POST",
            "/capsule_policies/active",
            Some(json!({"version": version})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "pg activate failed: {v}");
        let ceiling = overlay("pg-ceiling", None, BTreeSet::from([CapabilityGrant::Clock]));
        let (status, v) = call_as(
            &first,
            auth,
            "POST",
            "/capsules/overlays",
            Some(json!({"overlay": serde_json::to_value(&ceiling).unwrap()})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "pg attach failed: {v}");
        drop(first);

        // A fresh app over the same database serves the settled plane.
        let second = build();
        let (status, v) = call_as(&second, auth, "GET", "/capsule_policies/active", None).await;
        assert_eq!(status, StatusCode::OK, "pg active after restart: {v}");
        assert_eq!(v["version"], json!(version));
        let (status, v) = call_as(&second, auth, "GET", "/capsules/overlays", None).await;
        assert_eq!(status, StatusCode::OK, "pg overlays after restart: {v}");
        assert_eq!(v["overlays"].as_array().unwrap().len(), 1);
        let (status, v) = call_as(&second, auth, "GET", "/capsule_policies/versions", None).await;
        assert_eq!(status, StatusCode::OK, "pg list after restart: {v}");
        assert_eq!(v["policies"].as_array().unwrap().len(), 1);
    }
}
