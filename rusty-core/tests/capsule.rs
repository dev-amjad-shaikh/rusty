//! Rusty Capsules integration tests (R0.9, wave 1).
//!
//! Three test groups:
//!
//! - **Golden files** — the serialized shapes of `CapsuleManifest` (a
//!   full specimen: every grant kind, the full budget, both schemas),
//!   `CapsuleDenial` (scoped), `CapsuleResolution` (with the wave-2
//!   additive fields at `None`), and `CapsuleOverlay` (wave 2) are
//!   pinned against checked-in JSON under `tests/golden/`. The wave's
//!   three new `RunEventKind` wire names are pinned in
//!   `capsule_event_kinds.json`
//!   (the `learn_event_kinds.json` pattern; the exhaustive
//!   `run_event_kind.json` list is owned by `tests/agents.rs`, outside
//!   this wave's file scope). To bless an intentional contract change,
//!   re-run with `UPDATE_GOLDEN=1` and review the diff.
//! - **The contract** — content-address convergence (list order carries
//!   no identity), tamper rejection, and manifest/grant agreement
//!   validation, through the public API.
//! - **The capability host** (feature `wasm`) — the wave's exit criteria,
//!   automated: a no-grant guest cannot perform I/O (the import does not
//!   exist); fuel, memory, and wall-time limits each abort a planted
//!   misbehaving guest; a scoped-grant violation journals a
//!   `CapsuleDenied` naming the absent grant. The reference guests are
//!   hand-written component text (WAT) compiled by wasmtime's `wat`
//!   support — no guest toolchain required (see the module comment on
//!   `guests` below).

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde::Serialize;
use serde_json::json;

use rusty_agent_runtime::capsule::{
    derive_capsule_id, CapabilityGrant, CapsuleDenial, CapsuleId, CapsuleIdentity,
    CapsuleInterface, CapsuleManifest, CapsuleOverlay, CapsuleResolution, FilesystemMode,
    ResourceBudget, WORLD_V1,
};
use rusty_agent_runtime::record::{sha256_hex, CapsuleVersion, Effect, RunEventKind};

// ---------- golden-file machinery ----------

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join(name)
}

/// Assert the pretty-printed serialization of `value` equals the golden
/// file's content exactly. `UPDATE_GOLDEN=1` rewrites the file instead —
/// the diff is then the contract change under review.
fn assert_golden(name: &str, value: &impl Serialize) {
    let rendered = format!("{}\n", serde_json::to_string_pretty(value).unwrap());
    let path = golden_path(name);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, &rendered).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing golden file `{}`: {e}", path.display()));
    assert_eq!(
        rendered,
        expected,
        "contract drift in `{}` — if intentional, re-run with UPDATE_GOLDEN=1 \
         and review the diff",
        path.display()
    );
}

// ---------- golden shapes ----------

/// The full specimen: every grant kind, the full budget, both schemas, a
/// description — one manifest exercising every field the contract
/// declares. Effects are declared consistently with the grants (tool and
/// model imply `NonIdempotent`).
fn specimen_manifest() -> CapsuleManifest {
    CapsuleManifest {
        identity: CapsuleIdentity {
            name: "researcher".into(),
            description: Some("third-party research capsule".into()),
        },
        version: "1.4.0".into(),
        build_digest: sha256_hex(b"reference guest bytes"),
        interface: CapsuleInterface {
            world: WORLD_V1.into(),
            input_schema: Some(json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {"topic": {"type": "string"}},
                "required": ["topic"],
            })),
            output_schema: Some(json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
            })),
        },
        effects: BTreeSet::from([Effect::ReadOnly, Effect::NonIdempotent]),
        capabilities: BTreeSet::from([
            CapabilityGrant::Filesystem {
                paths: vec!["/data/research".into()],
                mode: FilesystemMode::Read,
            },
            CapabilityGrant::Network {
                hosts: vec!["api.example".into()],
                protocols: vec!["https".into()],
                methods: vec!["GET".into()],
            },
            CapabilityGrant::Secret {
                handles: vec!["search-api-key".into()],
            },
            CapabilityGrant::Tool {
                tools: vec!["web_search".into()],
            },
            CapabilityGrant::Model {
                models: vec!["summarizer-v3".into()],
            },
            CapabilityGrant::Clock,
        ]),
        budget: ResourceBudget {
            fuel: Some(5_000_000),
            max_memory_bytes: Some(16 * 1024 * 1024),
            wall_time_ms: Some(30_000),
            max_tokens: Some(50_000),
            max_cost_usd: Some(1.25),
            max_output_bytes: Some(65_536),
        },
    }
}

#[test]
fn golden_capsule_manifest_shape() {
    assert_golden("capsule_manifest.json", &specimen_manifest());
}

#[test]
fn golden_capsule_event_kinds_shape() {
    // The wave's new RunEventKind wire names, in declaration order (they
    // append after `policy_decision` — the same additive evolution rule
    // every variant since R0.6's `effect_receipt` followed).
    assert_golden(
        "capsule_event_kinds.json",
        &vec![
            RunEventKind::CapsuleResolved,
            RunEventKind::CapsuleCall,
            RunEventKind::CapsuleDenied,
        ],
    );
}

#[test]
fn golden_capsule_denial_shape() {
    // The scoped denial: granted `api.example`, attempted `evil.example`
    // — the absent grant names the missing scope.
    let denial = CapsuleDenial::scoped(
        CapsuleId::from("ab".repeat(32)),
        CapabilityGrant::Network {
            hosts: vec!["evil.example".into()],
            protocols: vec!["https".into()],
            methods: vec!["GET".into()],
        },
        "fetch GET https://evil.example/probe names no granted network scope",
    );
    assert_golden("capsule_denial.json", &denial);
}

#[test]
fn golden_capsule_resolution_shape() {
    // The wave-2 additive fields ride along as `None` (serde skips them)
    // — the wire shape a wave-1 journal entry already carries.
    let resolution = CapsuleResolution {
        name: "researcher".into(),
        version: CapsuleVersion::new("1.4.0"),
        capsule_id: CapsuleId::from("cd".repeat(32)),
        build_digest: "ef".repeat(32),
        policy_version: None,
        overlays: None,
        effective_grants: None,
        clamped_budget: None,
    };
    assert_golden("capsule_resolution.json", &resolution);
}

#[test]
fn golden_capsule_overlay_shape() {
    // The tenant overlay (R0.9 wave 2): a named ceiling over one capsule
    // name, with the operator's note.
    let overlay = CapsuleOverlay {
        name: "research-ceiling".into(),
        targets: Some(vec!["researcher".into()]),
        capabilities: BTreeSet::from([
            CapabilityGrant::Network {
                hosts: vec!["api.example".into()],
                protocols: vec!["https".into()],
                methods: vec!["GET".into()],
            },
            CapabilityGrant::Clock,
        ]),
        note: Some("egress narrowed to the research API".into()),
    };
    assert_golden("capsule_overlay.json", &overlay);
}

// ---------- contract behavior ----------

#[test]
fn content_address_converges_and_tampering_fails_it() {
    let id = derive_capsule_id(&specimen_manifest()).unwrap();
    assert_eq!(id, specimen_manifest().capsule_id().unwrap());
    // Rebuilding the same declaration converges on the same id.
    assert_eq!(derive_capsule_id(&specimen_manifest()).unwrap(), id);
    // A tampered manifest fails its own address.
    let mut tampered = specimen_manifest();
    tampered.capabilities.insert(CapabilityGrant::Network {
        hosts: vec!["attacker.example".into()],
        protocols: vec!["https".into()],
        methods: vec!["GET".into()],
    });
    assert_ne!(derive_capsule_id(&tampered).unwrap(), id);
}

#[test]
fn validation_rejects_out_of_taxonomy_declarations() {
    // A writing grant under a read-only effect declaration.
    let mut m = specimen_manifest();
    m.effects = BTreeSet::from([Effect::ReadOnly]);
    let err = m.validate().unwrap_err();
    assert!(err.to_string().contains("must agree"), "got: {err}");
    // An unknown world era.
    let mut m = specimen_manifest();
    m.interface.world = "rusty:capsule/world@0.2.0".into();
    let err = m.validate().unwrap_err();
    assert!(
        err.to_string().contains("unsupported WIT world"),
        "got: {err}"
    );
    // An empty scope list is a grant that permits nothing — refused.
    let mut m = specimen_manifest();
    m.capabilities.insert(CapabilityGrant::Secret {
        handles: Vec::new(),
    });
    assert!(m.validate().is_err());
}

// ---------- the capability host (feature `wasm`) ----------

#[cfg(feature = "wasm")]
mod host {
    //! The reference guests are hand-written component text (WAT),
    //! compiled by wasmtime's built-in `wat` support — no guest toolchain
    //! (`wit-bindgen`, `cargo-component`) required to build or test this
    //! wave, per the design's "no guest authoring toolchain" stance. The
    //! canonical-ABI glue (string lowering/lifting, the result retptr
    //! convention, the bump `realloc`) is written out by hand, which is
    //! exactly the discipline a generated guest applies: shared linear
    //! memory, static arguments in data segments, results forwarded by
    //! pointer. Each guest is a *planted behavior*: one probes granted and
    //! ungranted network hosts, one reads the clock, and three are the
    //! planted misbehaviors (fuel exhaustion loop, memory bomb, wall-time
    //! spinner) the exit criteria abort.

    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use rusty_agent_runtime::capsule_host::{
        CapsuleHost, CapsuleInvocation, FetchRequest, FetchResponse, NetworkConnector,
    };
    use rusty_agent_runtime::journal::{Clock, Journal};
    use rusty_agent_runtime::record::{EventStatus, PayloadRef};
    use serde_json::Value;

    /// Escape a string for a WAT string literal (the `wasm_node` test
    /// convention).
    fn wat_escape(raw: &str) -> String {
        raw.replace('\\', "\\\\").replace('"', "\\\"")
    }

    /// The canonical-ABI bump `realloc`: aligns the heap pointer, hands
    /// out the next `new_size` bytes, never frees. Short-lived guests leak
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
    /// answer `OUT`. A *lifted* export whose results exceed the flat limit
    /// returns one pointer into its own memory — the retptr-as-parameter
    /// convention applies to lowered imports, not to exports.
    const WRITE_RESULT: &str = r#"
      (i32.store8 (i32.const 512) (local.get $disc))
      (i32.store (i32.const 516) (local.get $ptr))
      (i32.store (i32.const 520) (local.get $len))
      (i32.const 512)"#;

    /// A pure-compute component: no imports, a static `ok` output. The
    /// empty-capability manifest's guest — nothing regresses from ABI v0's
    /// world.
    fn pure_guest_wat(output_json: &str) -> String {
        let escaped = wat_escape(output_json);
        let len = output_json.len();
        format!(
            r#"(component
  (core module $m
    (memory (export "memory") 1)
    {REALLOC}
    (func (export "run") (param $in_ptr i32) (param $in_len i32) (result i32)
      (local $disc i32) (local $ptr i32) (local $len i32)
      (local.set $disc (i32.const 0))
      (local.set $ptr (i32.const 16))
      (local.set $len (i32.const {len}))
      {WRITE_RESULT})
    (data (i32.const 16) "{escaped}"))
  (core instance $i (instantiate $m))
  (func $run (param "input" string) (result (result string (error string)))
    (canon lift (core func $i "run")
      (memory (core memory $i "memory"))
      (realloc (core func $i "realloc"))))
  (export "run" (func $run)))"#
        )
    }

    /// The probe component: imports `rusty:capsule/net@0.1.0`'s `fetch`,
    /// calls it once with static arguments (`GET https://{host}/probe`,
    /// no body), and forwards the host's `result<string, string>` as its
    /// own. This is the guest that performs I/O when granted — and the
    /// guest that *cannot* when the import is never linked.
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

    /// The clock component: imports `rusty:capsule/clock@0.1.0`'s
    /// `now-millis`, calls it once, returns a static `ok`.
    fn clock_guest_wat() -> String {
        format!(
            r#"(component
  (import "rusty:capsule/clock@0.1.0" (instance $clock
    (export "now-millis" (func (result u64)))))
  (alias export $clock "now-millis" (func $now))

  (core module $libc
    (memory (export "memory") 1)
    {REALLOC}
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
      {WRITE_RESULT}))
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

    /// Planted misbehavior: the fuel-exhaustion loop / wall-time spinner
    /// (which budget kills it depends on the invocation's bounds).
    fn loop_guest_wat() -> String {
        format!(
            r#"(component
  (core module $m
    (memory (export "memory") 1)
    {REALLOC}
    (func (export "run") (param $in_ptr i32) (param $in_len i32) (result i32)
      (loop $forever (br $forever))
      unreachable))
  (core instance $i (instantiate $m))
  (func $run (param "input" string) (result (result string (error string)))
    (canon lift (core func $i "run")
      (memory (core memory $i "memory"))
      (realloc (core func $i "realloc"))))
  (export "run" (func $run)))"#
        )
    }

    /// Planted misbehavior: the memory bomb. `memory.grow` by 100 pages;
    /// a denied grow returns -1 and the guest traps — so a *successful*
    /// invocation proves the cap was not enforced, and a trap proves it
    /// was.
    fn memory_bomb_wat() -> String {
        format!(
            r#"(component
  (core module $m
    (memory (export "memory") 1)
    {REALLOC}
    (func (export "run") (param $in_ptr i32) (param $in_len i32) (result i32)
      (local $disc i32) (local $ptr i32) (local $len i32)
      (if (i32.eq (memory.grow (i32.const 100)) (i32.const -1))
        (then unreachable))
      (local.set $disc (i32.const 0))
      (local.set $ptr (i32.const 16))
      (local.set $len (i32.const 13))
      {WRITE_RESULT})
    (data (i32.const 16) "{{\"grew\":true}}"))
  (core instance $i (instantiate $m))
  (func $run (param "input" string) (result (result string (error string)))
    (canon lift (core func $i "run")
      (memory (core memory $i "memory"))
      (realloc (core func $i "realloc"))))
  (export "run" (func $run)))"#
        )
    }

    /// A manifest admitted for `wat` with the given effects, grants, and
    /// budget. The build digest is over the artifact bytes the host
    /// compiles — the WAT text itself.
    fn manifest_for(
        wat: &str,
        effects: &[Effect],
        capabilities: BTreeSet<CapabilityGrant>,
        budget: ResourceBudget,
    ) -> CapsuleManifest {
        CapsuleManifest {
            identity: CapsuleIdentity {
                name: "probe".into(),
                description: None,
            },
            version: "0.1.0".into(),
            build_digest: sha256_hex(wat.as_bytes()),
            interface: CapsuleInterface {
                world: WORLD_V1.into(),
                input_schema: None,
                output_schema: None,
            },
            effects: effects.iter().copied().collect(),
            capabilities,
            budget,
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

    /// The scripted egress: counts calls and answers a fixed body. The
    /// scoped-violation test asserts it is *never called* — a denial must
    /// happen before anything executes.
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

    fn journal() -> Journal {
        Journal::new("run-capsule", "thread-capsule", Clock::System)
    }

    /// The inline output payload of the first journaled event of `kind`.
    fn event_payload(journal: &Journal, kind: RunEventKind) -> Value {
        let events = journal.events();
        events
            .iter()
            .find(|event| event.kind == kind)
            .unwrap_or_else(|| panic!("expected a journaled {kind:?} event"))
            .output
            .as_ref()
            .and_then(|payload| match payload {
                PayloadRef::Inline(value) => Some(value.clone()),
                PayloadRef::Artifact(_) => None,
            })
            .expect("capability payloads travel inline")
    }

    // --- Exit criterion 1: no-grant guests cannot perform I/O -------- //

    #[tokio::test]
    async fn no_grant_guest_cannot_perform_io() {
        let wat = probe_guest_wat("api.example");
        let manifest = manifest_for(
            &wat,
            &[Effect::Pure],
            BTreeSet::new(),
            ResourceBudget {
                fuel: Some(10_000_000),
                ..Default::default()
            },
        );
        let host = CapsuleHost::from_bytes(manifest, wat.as_bytes()).unwrap();
        let journal = journal();

        let err = host
            .invoke(
                CapsuleInvocation::new(json!({"probe": true})).with_journal(journal.clone(), None),
            )
            .await
            .unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("structural denial") && message.contains("does not exist"),
            "the import does not exist for this guest: {message}"
        );

        // The denial is journaled: capability network, absent grant with
        // empty scope (no grant at any scope existed).
        let payload = event_payload(&journal, RunEventKind::CapsuleDenied);
        assert_eq!(payload["capability"], json!("network"));
        assert_eq!(payload["absent_grant"]["kind"], json!("network"));
        assert_eq!(payload["absent_grant"]["hosts"], json!([]));
    }

    #[tokio::test]
    async fn pure_compute_guest_with_no_grants_runs() {
        let wat = pure_guest_wat(r#"{"result":"ok"}"#);
        let manifest = manifest_for(
            &wat,
            &[Effect::Pure],
            BTreeSet::new(),
            ResourceBudget {
                fuel: Some(10_000_000),
                ..Default::default()
            },
        );
        let host = CapsuleHost::from_bytes(manifest, wat.as_bytes()).unwrap();
        let journal = journal();

        let outcome = host
            .invoke(CapsuleInvocation::new(json!({})).with_journal(journal.clone(), None))
            .await
            .unwrap();
        assert_eq!(outcome.output, json!({"result": "ok"}));
        assert!(outcome.uses.is_empty());
        // The invocation itself is journaled as a WASM call naming the
        // capsule id.
        let payload = event_payload(&journal, RunEventKind::WasmCall);
        assert_eq!(payload["capsule_id"], json!(host.capsule_id().as_str()));
        assert_eq!(payload["output"], json!({"result": "ok"}));
    }

    // --- Exit criterion 2: resource limits abort planted misbehaviors - //

    #[tokio::test]
    async fn fuel_limit_aborts_planted_loop() {
        let wat = loop_guest_wat();
        let manifest = manifest_for(
            &wat,
            &[Effect::Pure],
            BTreeSet::new(),
            ResourceBudget {
                fuel: Some(1_000_000),
                ..Default::default()
            },
        );
        let host = CapsuleHost::from_bytes(manifest, wat.as_bytes()).unwrap();
        let journal = journal();

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            host.invoke(CapsuleInvocation::new(json!({})).with_journal(journal.clone(), None)),
        )
        .await
        .expect("the fuel budget must kill the loop, not hang");
        let err = result.unwrap_err();
        assert!(err.to_string().contains("trapped"), "got: {err}");
        // The breach is journaled, attributed to the budget that bit.
        let payload = event_payload(&journal, RunEventKind::WasmCall);
        assert_eq!(payload["budget_breach"], json!("fuel"));
    }

    #[tokio::test]
    async fn memory_limit_aborts_planted_bomb() {
        let wat = memory_bomb_wat();
        let manifest = manifest_for(
            &wat,
            &[Effect::Pure],
            BTreeSet::new(),
            ResourceBudget {
                fuel: Some(10_000_000),
                // One 64 KiB page: the initial page fits, the 100-page grow
                // is denied and the planted guest traps on the denial.
                max_memory_bytes: Some(64 * 1024),
                ..Default::default()
            },
        );
        let host = CapsuleHost::from_bytes(manifest, wat.as_bytes()).unwrap();

        let err = host
            .invoke(CapsuleInvocation::new(json!({})))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("trapped"), "got: {err}");
    }

    #[tokio::test]
    async fn wall_time_limit_aborts_planted_spinner() {
        let wat = loop_guest_wat();
        let manifest = manifest_for(
            &wat,
            &[Effect::Pure],
            BTreeSet::new(),
            ResourceBudget {
                // Fuel deliberately unbounded: only the wall-time budget can
                // stop this guest, and epoch interruption is its enforcement
                // arm.
                wall_time_ms: Some(150),
                ..Default::default()
            },
        );
        let host = CapsuleHost::from_bytes(manifest, wat.as_bytes()).unwrap();
        let journal = journal();

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            host.invoke(CapsuleInvocation::new(json!({})).with_journal(journal.clone(), None)),
        )
        .await
        .expect("the wall-time budget must preempt the spinner, not hang");
        let err = result.unwrap_err();
        assert!(err.to_string().contains("trapped"), "got: {err}");
        let payload = event_payload(&journal, RunEventKind::WasmCall);
        assert_eq!(payload["budget_breach"], json!("wall_time"));
    }

    // --- Exit criterion 3: scoped violations journal CapsuleDenied ---- //

    #[tokio::test]
    async fn scoped_grant_violation_journals_denial_naming_the_absent_grant() {
        let wat = probe_guest_wat("evil.example");
        let manifest = manifest_for(
            &wat,
            &[Effect::ReadOnly],
            BTreeSet::from([network_grant("api.example")]),
            ResourceBudget {
                fuel: Some(10_000_000),
                ..Default::default()
            },
        );
        let connector = Arc::new(ScriptedConnector {
            calls: AtomicUsize::new(0),
            body: r#"{"answer":42}"#.into(),
        });
        let host = CapsuleHost::from_bytes(manifest, wat.as_bytes())
            .unwrap()
            .with_connector(connector.clone());
        let journal = journal();

        let err = host
            .invoke(CapsuleInvocation::new(json!({})).with_journal(journal.clone(), None))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("denied"),
            "the guest surfaces the in-band denial: {err}"
        );
        // The connector never executed: the denial precedes any socket.
        assert_eq!(connector.calls.load(Ordering::Relaxed), 0);

        // The denial is journaled and names the absent grant: `network`
        // scoped to the attempted host.
        let payload = event_payload(&journal, RunEventKind::CapsuleDenied);
        assert_eq!(payload["capability"], json!("network"));
        assert_eq!(payload["absent_grant"]["kind"], json!("network"));
        assert_eq!(payload["absent_grant"]["hosts"], json!(["evil.example"]));
        assert_eq!(payload["absent_grant"]["methods"], json!(["GET"]));
        // The event itself records that nothing executed.
        let events = journal.events();
        let event = events
            .iter()
            .find(|event| event.kind == RunEventKind::CapsuleDenied)
            .unwrap();
        assert_eq!(event.effect, Effect::Pure);
        assert_eq!(event.status, EventStatus::Ok);
    }

    #[tokio::test]
    async fn granted_fetch_succeeds_and_journals_the_use() {
        let wat = probe_guest_wat("api.example");
        let manifest = manifest_for(
            &wat,
            &[Effect::ReadOnly],
            BTreeSet::from([network_grant("api.example")]),
            ResourceBudget {
                fuel: Some(10_000_000),
                ..Default::default()
            },
        );
        let connector = Arc::new(ScriptedConnector {
            calls: AtomicUsize::new(0),
            body: r#"{"answer":42}"#.into(),
        });
        let host = CapsuleHost::from_bytes(manifest, wat.as_bytes())
            .unwrap()
            .with_connector(connector.clone());
        let journal = journal();

        let outcome = host
            .invoke(CapsuleInvocation::new(json!({})).with_journal(journal.clone(), None))
            .await
            .unwrap();
        assert_eq!(outcome.output, json!({"answer": 42}));
        assert_eq!(connector.calls.load(Ordering::Relaxed), 1);
        assert_eq!(outcome.uses.len(), 1);
        assert!(outcome.denials.is_empty());

        // Every use is journaled: the granted fetch, with its matched
        // scope and the connector's response.
        let payload = event_payload(&journal, RunEventKind::CapsuleCall);
        assert_eq!(payload["capability"], json!("network"));
        assert_eq!(payload["operation"], json!("fetch"));
        assert_eq!(payload["request"]["host"], json!("api.example"));
        assert_eq!(payload["response"]["status"], json!(200));
        let events = journal.events();
        let event = events
            .iter()
            .find(|event| event.kind == RunEventKind::CapsuleCall)
            .unwrap();
        assert_eq!(event.effect, Effect::ReadOnly);
    }

    #[tokio::test]
    async fn clock_grant_links_the_import_and_journals_the_use() {
        let wat = clock_guest_wat();
        let manifest = manifest_for(
            &wat,
            &[Effect::ReadOnly],
            BTreeSet::from([CapabilityGrant::Clock]),
            ResourceBudget {
                fuel: Some(10_000_000),
                ..Default::default()
            },
        );
        let host = CapsuleHost::from_bytes(manifest, wat.as_bytes())
            .unwrap()
            .with_clock(|| 1_750_000_000_000);
        let journal = journal();

        let outcome = host
            .invoke(CapsuleInvocation::new(json!({})).with_journal(journal.clone(), None))
            .await
            .unwrap();
        assert_eq!(outcome.output, json!({"clock": true}));
        let payload = event_payload(&journal, RunEventKind::CapsuleCall);
        assert_eq!(payload["capability"], json!("clock"));
        assert_eq!(payload["response"]["millis"], json!(1_750_000_000_000_u64));
    }

    #[tokio::test]
    async fn ungranted_clock_import_is_a_structural_denial() {
        let wat = clock_guest_wat();
        let manifest = manifest_for(
            &wat,
            &[Effect::Pure],
            BTreeSet::new(),
            ResourceBudget {
                fuel: Some(10_000_000),
                ..Default::default()
            },
        );
        let host = CapsuleHost::from_bytes(manifest, wat.as_bytes()).unwrap();
        let journal = journal();

        let err = host
            .invoke(CapsuleInvocation::new(json!({})).with_journal(journal.clone(), None))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("structural denial"), "got: {err}");
        let payload = event_payload(&journal, RunEventKind::CapsuleDenied);
        assert_eq!(payload["capability"], json!("clock"));
        assert_eq!(payload["absent_grant"]["kind"], json!("clock"));
    }

    // --- Admission and the output gate ------------------------------ //

    #[tokio::test]
    async fn build_digest_mismatch_fails_admission() {
        let wat = pure_guest_wat(r#"{"result":"ok"}"#);
        let mut manifest = manifest_for(
            &wat,
            &[Effect::Pure],
            BTreeSet::new(),
            ResourceBudget::default(),
        );
        manifest.build_digest = sha256_hex(b"different bytes");
        let err = CapsuleHost::from_bytes(manifest, wat.as_bytes()).unwrap_err();
        assert!(
            err.to_string().contains("build digest mismatch"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn output_gate_enforces_size_budget_and_well_formed_json() {
        // Over max_output_bytes.
        let big = format!("{{\"pad\":\"{}\"}}", "x".repeat(2048));
        let wat = pure_guest_wat(&big);
        let manifest = manifest_for(
            &wat,
            &[Effect::Pure],
            BTreeSet::new(),
            ResourceBudget {
                fuel: Some(10_000_000),
                max_output_bytes: Some(1024),
                ..Default::default()
            },
        );
        let host = CapsuleHost::from_bytes(manifest, wat.as_bytes()).unwrap();
        let err = host
            .invoke(CapsuleInvocation::new(json!({})))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("max_output_bytes"), "got: {err}");

        // Not JSON at all.
        let wat = pure_guest_wat("not json");
        let manifest = manifest_for(
            &wat,
            &[Effect::Pure],
            BTreeSet::new(),
            ResourceBudget {
                fuel: Some(10_000_000),
                ..Default::default()
            },
        );
        let host = CapsuleHost::from_bytes(manifest, wat.as_bytes()).unwrap();
        let err = host
            .invoke(CapsuleInvocation::new(json!({})))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not valid JSON"), "got: {err}");
    }
}
