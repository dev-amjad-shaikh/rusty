//! The R0.9 Rusty Capsules release proof (wave 4): the visible-denial
//! story, end to end over HTTP.
//!
//! One A2A context drives three capsule messages — a granted fetch that
//! executes, a scoped violation the host refuses, and a structural
//! refusal the bridge journals at admission — and the proof reads the
//! evidence back through the *native* surface: the context's Flight
//! Recorder events, the signed receipt over them, and caller-side
//! verification of the exported snapshot. The denials are not log lines;
//! they are journaled, attributable, signed events that survive export —
//! and tampering with one fails verification naming the journal head.
//!
//! Gated on the `capsules` feature (the capability host is wasm); the
//! reference guests are hand-written component text compiled by
//! wasmtime's `wat` support, the `rusty-agent-runtime` capsule test
//! convention.

#![cfg(feature = "capsules")]

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::capsule::{
    CapabilityGrant, CapsuleIdentity, CapsuleInterface, CapsuleManifest, ResourceBudget, WORLD_V1,
};
use rusty_agent_runtime::capsule_host::{FetchRequest, FetchResponse, NetworkConnector};
use rusty_agent_runtime::record::{sha256_hex, Effect};
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

// --------------------------------------------------------------------- //
// Harness (the bridges.rs convention)
// --------------------------------------------------------------------- //

/// Unique temp store root, removed at the end of the test (best effort).
fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!(
        "rusty-server-release-test-{}",
        uuid::Uuid::new_v4()
    ))
}

/// The registry: the trivial graph named `a2a` the bridge binds context
/// thread records to (the fixture endpoint resolves evidence through the
/// thread record's graph binding, so the name must be registered — the
/// `A2A_THREAD_GRAPH` convention).
fn registry() -> GraphRegistry {
    use rusty_agent_runtime::prelude::*;

    let spec = StateSpec::new().channel("log", Reducer::Append);
    let mut builder = GraphBuilder::new();
    builder.add_node("noop", |_ctx: NodeContext| async {
        Ok(NodeOutput::update("log", json!("noop")))
    });
    builder.set_entry_point("noop");
    let graph = builder.compile().unwrap();

    let mut registry = GraphRegistry::new();
    registry.register("a2a", graph, spec);
    registry
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

/// The JSON-RPC envelope for one A2A request.
fn rpc(id: u64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

/// Poll `tasks/get` until the task settles (the bridge executor is
/// asynchronous by design).
async fn wait_terminal(app: &Router, task_id: &str) -> Value {
    for _ in 0..400 {
        let (_s, v) = call(
            app,
            "POST",
            "/a2a",
            Some(rpc(0, "tasks/get", json!({ "id": task_id }))),
        )
        .await;
        let state = v["result"]["status"]["state"].as_str().unwrap_or("");
        if matches!(state, "completed" | "failed" | "canceled") {
            return v["result"].clone();
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("task `{task_id}` did not settle in time");
}

// --------------------------------------------------------------------- //
// The reference guests + manifests (the core capsule test convention)
// --------------------------------------------------------------------- //

/// Escape a string for a WAT string literal.
fn wat_escape(raw: &str) -> String {
    raw.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The canonical-ABI bump `realloc`: aligns the heap pointer, hands out
/// the next `new_size` bytes, never frees. Short-lived guests leak
/// freely; the store is dropped at invocation end.
const REALLOC: &str = r#"
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

/// Write `result<string, string>` at `OUT` (512): discriminant u8 at +0,
/// payload (ptr, len) at +4/+8 (the canonical-ABI variant layout), then
/// answer `OUT`.
const WRITE_RESULT: &str = r#"
      (i32.store8 (i32.const 512) (local.get $disc))
      (i32.store (i32.const 516) (local.get $ptr))
      (i32.store (i32.const 520) (local.get $len))
      (i32.const 512)"#;

/// The probe component: imports `rusty:capsule/net@0.1.0`'s `fetch`,
/// calls it once with static arguments (`GET https://{host}/probe`, no
/// body), and forwards the host's `result<string, string>` as its own.
/// This is the guest that performs I/O when granted — and the guest
/// whose out-of-scope attempt the host must refuse in-band.
fn probe_guest_wat(host: &str) -> String {
    let host_len = host.len();
    format!(
        r#"(component
  (import "rusty:capsule/net@0.1.0" (instance $net
    (export "fetch" (func (param "protocol" string) (param "host" string) (param "method" string) (param "path" string) (param "body" (option string)) (result (result string (error string)))))))
  (alias export $net "fetch" (func $fetch))

  (core module $libc
    (memory (export "memory") 1)
    {REALLOC}
    (data (i32.const 16) "https")
    (data (i32.const 32) "{host}")
    (data (i32.const 96) "GET")
    (data (i32.const 112) "/probe"))
  (core instance $libc_i (instantiate $libc))

  (core func $fetch_lowered (canon lower (func $fetch)
    (memory (core memory $libc_i "memory"))
    (realloc (core func $libc_i "realloc"))))

  (core module $guest
    (import "libc" "memory" (memory 1))
    (import "net" "fetch" (func $fetch (param i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32)))
    (func (export "run") (param $in_ptr i32) (param $in_len i32) (result i32)
      (local $disc i32) (local $ptr i32) (local $len i32)
      ;; fetch("https", host, "GET", "/probe", none) with retptr 512 (a
      ;; lowered import's over-limit results arrive through a parameter).
      (call $fetch
        (i32.const 16) (i32.const 5)
        (i32.const 32) (i32.const {host_len})
        (i32.const 96) (i32.const 3)
        (i32.const 112) (i32.const 6)
        (i32.const 0) (i32.const 0) (i32.const 0)
        (i32.const 512))
      ;; Forward the host's result as this guest's own: read the retptr
      ;; slots into locals, then re-emit them (WRITE_RESULT targets 512).
      (local.set $disc (i32.load8_u (i32.const 512)))
      (local.set $ptr (i32.load (i32.const 516)))
      (local.set $len (i32.load (i32.const 520)))
      {WRITE_RESULT}))
  (core instance $guest_i (instantiate $guest
    (with "libc" (instance (export "memory" (memory $libc_i "memory"))))
    (with "net" (instance (export "fetch" (func $fetch_lowered))))))

  (func $run (param "input" string) (result (result string (error string)))
    (canon lift (core func $guest_i "run")
      (memory (core memory $libc_i "memory"))
      (realloc (core func $libc_i "realloc"))))
  (export "run" (func $run)))"#,
        host = wat_escape(host),
    )
}

/// A manifest admitted for `wat` under `name@version` with one network
/// grant. The build digest is over the artifact bytes the host compiles —
/// the WAT text itself (the blob upload must name exactly these bytes).
fn manifest_for(name: &str, version: &str, wat: &str, grant: CapabilityGrant) -> CapsuleManifest {
    CapsuleManifest {
        identity: CapsuleIdentity {
            name: name.into(),
            description: None,
        },
        version: version.into(),
        build_digest: sha256_hex(wat.as_bytes()),
        interface: CapsuleInterface {
            world: WORLD_V1.into(),
            input_schema: None,
            output_schema: None,
        },
        effects: BTreeSet::from([Effect::ReadOnly]),
        capabilities: BTreeSet::from([grant]),
        budget: ResourceBudget {
            fuel: Some(10_000_000),
            ..Default::default()
        },
    }
}

/// A network grant for one exact call shape.
fn network_grant(host: &str) -> CapabilityGrant {
    CapabilityGrant::Network {
        hosts: vec![host.into()],
        protocols: vec!["https".into()],
        methods: vec!["GET".into()],
    }
}

/// The scripted egress: counts calls and answers a fixed body. The proof
/// asserts it ran exactly once across three capsule messages — both
/// denials happen before anything executes.
#[derive(Debug)]
struct ScriptedConnector {
    calls: AtomicUsize,
    body: String,
}

impl NetworkConnector for ScriptedConnector {
    fn fetch(&self, _request: &FetchRequest) -> Result<FetchResponse, String> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(FetchResponse {
            status: 200,
            body: self.body.clone(),
        })
    }
}

// --------------------------------------------------------------------- //
// The release proof
// --------------------------------------------------------------------- //

#[tokio::test]
async fn visible_denial_release_proof() {
    let store = temp_store();
    let connector = Arc::new(ScriptedConnector {
        calls: AtomicUsize::new(0),
        body: r#"{"answer":42}"#.into(),
    });
    let app = router(
        registry(),
        ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
            .with_capsule_connector(connector.clone()),
    );

    // --- Chapter 1: the registry holds two capsules -------------------- //
    // `probe-granted` fetches the host its manifest grants;
    // `probe-ungranted` fetches a host outside its grant. Same WAT
    // shape, different target — the difference is entirely in what the
    // grant permits.
    let granted_wat = probe_guest_wat("api.example.com");
    let ungranted_wat = probe_guest_wat("evil.example.com");
    let capsules = [
        ("probe-granted", &granted_wat),
        ("probe-ungranted", &ungranted_wat),
    ];
    for (name, wat) in &capsules {
        let manifest = manifest_for(name, "1.0.0", wat, network_grant("api.example.com"));
        let (status, v) = call(
            &app,
            "POST",
            "/capsules",
            Some(json!({ "manifest": serde_json::to_value(manifest).unwrap() })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "register {name} failed: {v}");
        let capsule_id = v["capsule_id"].as_str().unwrap();

        let request = Request::builder()
            .method("PUT")
            .uri(format!("/capsules/{capsule_id}/blob"))
            .body(Body::from(wat.as_bytes().to_vec()))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::CREATED,
            "blob upload for {name} failed"
        );
    }

    // --- Chapter 2: one A2A context, three capsule messages ------------ //
    let send_capsule = async |name: &str, message_id: &str, input: Value| {
        let (_s, v) = call(
            &app,
            "POST",
            "/a2a",
            Some(rpc(
                1,
                "message/send",
                json!({
                    "message": {
                        "role": "user",
                        "messageId": message_id,
                        "contextId": "release-proof",
                        "parts": [{
                            "kind": "data",
                            "data": {
                                "capsule": { "name": name, "version": "1.0.0" },
                                "input": input,
                            },
                        }],
                    },
                }),
            )),
        )
        .await;
        let task_id = v["result"]["id"]
            .as_str()
            .unwrap_or_else(|| panic!("message/send for {message_id} failed: {v}"))
            .to_string();
        wait_terminal(&app, &task_id).await
    };

    // (a) The granted fetch executes.
    let granted = send_capsule("probe-granted", "msg-granted", json!({})).await;
    assert_eq!(
        granted["status"]["state"],
        json!("completed"),
        "the granted capsule completes: {granted}"
    );
    assert_eq!(
        granted["artifacts"][0]["parts"][0]["data"],
        json!({"answer": 42}),
        "the connector's body is the task's artifact: {granted}"
    );

    // (b) The scoped violation is refused by the host.
    let scoped = send_capsule("probe-ungranted", "msg-scoped", json!({})).await;
    assert_eq!(
        scoped["status"]["state"],
        json!("failed"),
        "the out-of-scope capsule fails: {scoped}"
    );

    // (c) The structural refusal is journaled at admission — the caller
    // requires `filesystem`, which the v1 world does not import and the
    // manifest does not grant.
    let structural = send_capsule(
        "probe-granted",
        "msg-structural",
        json!({ "requires": ["filesystem"] }),
    )
    .await;
    assert_eq!(
        structural["status"]["state"],
        json!("failed"),
        "the filesystem requirement is refused: {structural}"
    );

    // Both refusals happened before egress: one fetch across three
    // capsule executions.
    assert_eq!(
        connector.calls.load(Ordering::Relaxed),
        1,
        "only the granted fetch ever reached the connector"
    );

    // --- Chapter 3: the evidence is on the native surface -------------- //
    // The context id maps to one Flight Recorder journal, readable
    // through `GET /runs/{id}/events` like any run's.
    let (status, v) = call(&app, "GET", "/runs/a2a-default-release-proof/events", None).await;
    assert_eq!(status, StatusCode::OK, "events failed: {v}");
    let events = v["events"].as_array().unwrap();
    let ids: Vec<&str> = events.iter().filter_map(|e| e["id"].as_str()).collect();
    let capsule_events: Vec<&Value> = events
        .iter()
        .filter(|e| {
            matches!(
                e["kind"].as_str(),
                Some("capsule_call") | Some("capsule_denied") | Some("wasm_call")
            )
        })
        .collect();
    assert!(
        capsule_events.len() >= 5,
        "three invocations leave their call/denial evidence: {v}"
    );

    // The granted fetch: one capsule_call naming host and operation.
    let calls: Vec<&&Value> = capsule_events
        .iter()
        .filter(|e| e["kind"] == json!("capsule_call"))
        .collect();
    assert_eq!(calls.len(), 1, "exactly one granted fetch executed: {v}");
    let call_payload = &calls[0]["output"]["value"];
    assert_eq!(call_payload["capability"], json!("network"));
    assert_eq!(call_payload["operation"], json!("fetch"));
    assert_eq!(call_payload["request"]["host"], json!("api.example.com"));

    // Two denials, attributable and distinct: the scoped violation names
    // the absent scope (`evil.example.com`); the structural refusal
    // names the capability class with the empty-scope absent grant.
    let denials: Vec<&&Value> = capsule_events
        .iter()
        .filter(|e| e["kind"] == json!("capsule_denied"))
        .collect();
    assert_eq!(denials.len(), 2, "exactly two denials: {v}");
    let scoped_denial = denials
        .iter()
        .find(|e| e["output"]["value"]["capability"] == json!("network"))
        .expect("a scoped network denial");
    assert_eq!(
        scoped_denial["output"]["value"]["absent_grant"]["hosts"],
        json!(["evil.example.com"]),
        "the scoped denial names the missing scope: {v}"
    );
    let structural_denial = denials
        .iter()
        .find(|e| e["output"]["value"]["capability"] == json!("filesystem"))
        .expect("a structural filesystem denial");
    assert_eq!(
        structural_denial["output"]["value"]["absent_grant"]["kind"],
        json!("filesystem"),
        "{v}"
    );
    assert_eq!(
        structural_denial["output"]["value"]["absent_grant"]["paths"],
        json!([]),
        "no grant at any scope existed: {v}"
    );

    // The causal chain: capability events and their invocation event are
    // siblings sharing the invocation's parent (the host journals them
    // from the same anchor), so the first invocation's two events are the
    // chain's parentless roots; every later capsule event parents to an
    // earlier event in the same journal — invocations chain across tasks.
    let mut parentless = 0;
    for (index, event) in capsule_events.iter().enumerate() {
        match event["parent"].as_str() {
            Some(parent) => assert!(
                ids[..ids
                    .iter()
                    .position(|id| *id == event["id"].as_str().unwrap())
                    .unwrap()]
                    .contains(&parent),
                "capsule event {index} parents to an earlier event: {event}"
            ),
            None => parentless += 1,
        }
    }
    assert_eq!(
        parentless, 2,
        "the first invocation's call and invocation events are the roots: {v}"
    );

    // --- Chapter 4: the receipt signs the denials ---------------------- //
    let (status, receipt) =
        call(&app, "GET", "/runs/a2a-default-release-proof/receipt", None).await;
    assert_eq!(status, StatusCode::OK, "receipt failed: {receipt}");
    let denial_ids: Vec<&str> = denials.iter().map(|e| e["id"].as_str().unwrap()).collect();
    let receipt_denials: Vec<&str> = receipt["denials"]
        .as_array()
        .expect("the receipt carries a denials ledger")
        .iter()
        .filter_map(|d| d.as_str())
        .collect();
    for id in &denial_ids {
        assert!(
            receipt_denials.contains(id),
            "the receipt's denials ledger names {id}: {receipt}"
        );
    }

    // --- Chapter 5: exported evidence verifies; tampering fails -------- //
    let (status, fixture) =
        call(&app, "GET", "/runs/a2a-default-release-proof/fixture", None).await;
    assert_eq!(status, StatusCode::OK, "fixture failed: {fixture}");
    let snapshot = fixture["journal"].clone();

    let (status, v) = call(
        &app,
        "POST",
        "/receipts/verify",
        Some(json!({ "snapshot": snapshot, "receipt": receipt })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "verification failed: {v}");

    // Flip one detail string inside a denial event's payload: the
    // exported evidence no longer matches the signed head, and the
    // failure names it.
    let mut tampered = snapshot.clone();
    let index = tampered["events"]
        .as_array()
        .unwrap()
        .iter()
        .position(|e| e["kind"] == json!("capsule_denied"))
        .unwrap();
    tampered["events"][index]["output"]["value"]["detail"] = json!("a benign-looking edit");
    let (status, v) = call(
        &app,
        "POST",
        "/receipts/verify",
        Some(json!({ "snapshot": tampered, "receipt": receipt })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "tampering must fail verification: {v}"
    );
    assert_eq!(v["error"], json!("receipt_verification_failed"), "{v}");
    assert!(
        v["message"].as_str().unwrap().starts_with("journal_head:"),
        "the failure names the journal head: {v}"
    );

    drop(app);
    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Postgres backend (live database required)
//
// Gated on `RUSTY_TEST_DATABASE_URL`; unset skips cleanly (the
// capsules.rs convention). The blob SQL (`BYTEA` insert / read-back) and
// the task claim/settle the executor drives are the wave-4 code paths
// this exercises on the second backend — the file backend's release
// proof above covers the semantics.
// --------------------------------------------------------------------- //

#[cfg(feature = "postgres")]
mod postgres {
    use super::*;

    fn pg_url() -> Option<String> {
        std::env::var("RUSTY_TEST_DATABASE_URL").ok()
    }

    /// The granted-fetch half of the proof on Postgres: register, upload
    /// the blob, execute through the bridge, and read the journaled use
    /// back through the native events endpoint.
    #[tokio::test]
    async fn postgres_granted_fetch_executes_and_journals() {
        let Some(url) = pg_url() else {
            eprintln!("RUSTY_TEST_DATABASE_URL unset; skipping");
            return;
        };
        let tenant = format!("releasepg-{}", uuid::Uuid::new_v4());
        let connector = Arc::new(ScriptedConnector {
            calls: AtomicUsize::new(0),
            body: r#"{"answer":42}"#.into(),
        });
        let app = router(
            registry(),
            ServerConfig::new("127.0.0.1:0".parse().unwrap(), temp_store())
                .with_postgres(url)
                .with_tenant_key(tenant.clone(), "pg-secret")
                .with_capsule_connector(connector.clone()),
        );
        let auth = Some(("x-api-key", "pg-secret"));

        let wat = probe_guest_wat("api.example.com");
        let manifest = manifest_for("pg-probe", "1.0.0", &wat, network_grant("api.example.com"));
        let (status, v) = call_as(
            &app,
            auth,
            "POST",
            "/capsules",
            Some(json!({ "manifest": serde_json::to_value(manifest).unwrap() })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "pg register failed: {v}");
        let capsule_id = v["capsule_id"].as_str().unwrap();

        let mut builder = Request::builder()
            .method("PUT")
            .uri(format!("/capsules/{capsule_id}/blob"));
        if let Some((k, v)) = auth {
            builder = builder.header(k, v);
        }
        let response = app
            .clone()
            .oneshot(builder.body(Body::from(wat.as_bytes().to_vec())).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED, "pg blob failed");

        let (_s, v) = call_as(
            &app,
            auth,
            "POST",
            "/a2a",
            Some(rpc(
                1,
                "message/send",
                json!({
                    "message": {
                        "role": "user",
                        "messageId": "pg-msg",
                        "contextId": "pg-proof",
                        "parts": [{
                            "kind": "data",
                            "data": {
                                "capsule": { "name": "pg-probe", "version": "1.0.0" },
                                "input": {},
                            },
                        }],
                    },
                }),
            )),
        )
        .await;
        let task_id = v["result"]["id"]
            .as_str()
            .unwrap_or_else(|| panic!("pg message/send failed: {v}"))
            .to_string();

        // Poll to terminal as this tenant.
        let mut terminal = Value::Null;
        for _ in 0..400 {
            let (_s, v) = call_as(
                &app,
                auth,
                "POST",
                "/a2a",
                Some(rpc(0, "tasks/get", json!({ "id": task_id }))),
            )
            .await;
            let state = v["result"]["status"]["state"].as_str().unwrap_or("");
            if matches!(state, "completed" | "failed" | "canceled") {
                terminal = v["result"].clone();
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert_eq!(
            terminal["status"]["state"],
            json!("completed"),
            "the granted capsule completes on Postgres: {terminal}"
        );
        assert_eq!(connector.calls.load(Ordering::Relaxed), 1);

        // The journaled use reads back through the native surface — the
        // run id embeds the tenant (`a2a-{tenant}-{contextId}`).
        let (status, v) = call_as(
            &app,
            auth,
            "GET",
            &format!("/runs/a2a-{tenant}-pg-proof/events"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "pg events failed: {v}");
        let has_call = v["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == json!("capsule_call"));
        assert!(has_call, "the granted fetch is journaled on Postgres: {v}");
    }
}
