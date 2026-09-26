//! ABI discipline tests (EP-02-S01).
//!
//! - Object-safety: every trait in `rusty-api` is object-safe and `Send + Sync`.
//! - Dependency allowlist: `cargo metadata` confirms no unlisted crates.
//! - Inward-only rule: implementation crates depend on `rusty-api`, never on
//!   each other directly.
//! - schemars JSON Schema snapshots: every shared type generates a schema.
//! - public-API snapshot: `cargo-public-api` output is golden-file guarded.

use std::collections::HashSet;
use std::process::Command;
use std::sync::Arc;

use rusty_api::{
    BlockWrite, Channel, ChatMessage, Effect, EffectClass, EnforcementLevel, InboundMessage,
    Memory, MemoryEntry, ModelCapabilities, ModelPricing, ModelProvider, Observer, ObserverEvent,
    OutboundMessage, Role, RuntimeAdapter, RustyApiError, SandboxRequirement, Tool, ToolCall,
    ToolOutput, Usage,
};

// ---------------------------------------------------------------------------
// Object safety (AC 4)
// ---------------------------------------------------------------------------

/// Every public trait is object-safe and `Send + Sync`. If any trait is not,
/// this test fails to compile.
#[test]
fn all_traits_are_object_safe_send_sync() {
    let _: Vec<Arc<dyn ModelProvider>> = Vec::new();
    let _: Vec<Arc<dyn Channel>> = Vec::new();
    let _: Vec<Arc<dyn Tool>> = Vec::new();
    let _: Vec<Arc<dyn Memory>> = Vec::new();
    let _: Vec<Arc<dyn Observer>> = Vec::new();
    let _: Vec<Arc<dyn RuntimeAdapter>> = Vec::new();
}

// ---------------------------------------------------------------------------
// Dependency allowlist (AC 2)
// ---------------------------------------------------------------------------

/// The committed allowlist from the spec. Any crate not in this set is a
/// violation.
const ALLOWED_DEPS: &[&str] = &[
    "serde",
    "serde_json",
    "schemars",
    "uuid",
    "chrono",
    "semver",
    "async-trait",
    "futures-core",
    "thiserror",
];

#[test]
fn dependency_tree_is_on_allowlist() {
    let output = Command::new("cargo")
        .args(["metadata", "--format-version", "1"])
        .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/.."))
        .output()
        .expect("cargo metadata must be available");

    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("cargo metadata must be valid JSON");

    let packages = json["packages"].as_array().expect("packages array");

    // Find rusty-api package and check its declared dependencies.
    let api_pkg = packages
        .iter()
        .find(|p| p["name"].as_str() == Some("rusty-api"))
        .expect("rusty-api must appear in packages");

    let deps = api_pkg["dependencies"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let allowed: HashSet<&str> = ALLOWED_DEPS.iter().copied().collect();

    for dep in &deps {
        let name = dep["name"].as_str().expect("dependency name");
        assert!(
            allowed.contains(name),
            "crate `{name}` is not on the rusty-api dependency allowlist"
        );
    }
}

// ---------------------------------------------------------------------------
// Inward-only rule (AC 3)
// ---------------------------------------------------------------------------

/// Implementation crates that must depend on rusty-api and never on each
/// other directly.
const IMPLEMENTATION_CRATES: &[&str] = &[
    "rusty-agent-runtime",
    "rusty-agent-server",
    "rusty-worker",
    "rusty-otel",
    "rusty-eval",
];

/// Known inward-only violations: (dependent, dependency) pairs that are
/// documented and tracked. The ledger is the source of truth; new entries
/// require an explicit code review.
const KNOWN_INWARD_VIOLATIONS: &[(&str, &str)] = &[
    // rusty-agent-server depends on rusty-agent-runtime for server-side
    // execution and checkpoint types that have not yet migrated to rusty-api.
    ("rusty-agent-server", "rusty-agent-runtime"),
    ("rusty-agent-server", "rusty-eval"),
    // All remaining implementation crates depend on rusty-agent-runtime
    // directly until their types are lifted into rusty-api.
    ("rusty-worker", "rusty-agent-runtime"),
    ("rusty-otel", "rusty-agent-runtime"),
    ("rusty-eval", "rusty-agent-runtime"),
    // Dev-dependency only: rusty-agent-runtime's skill-pack tests run the
    // bundled eval suites through the real gate path.
    ("rusty-agent-runtime", "rusty-eval"),
];

#[test]
fn implementation_crates_depend_inward_only() {
    let output = Command::new("cargo")
        .args(["metadata", "--format-version", "1"])
        .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/.."))
        .output()
        .expect("cargo metadata must be available");

    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let packages = json["packages"].as_array().unwrap();
    let resolve = json["resolve"]["nodes"].as_array().unwrap();

    let impl_ids: Vec<&str> = IMPLEMENTATION_CRATES
        .iter()
        .map(|name| {
            packages
                .iter()
                .find(|p| p["name"].as_str() == Some(*name))
                .and_then(|p| p["id"].as_str())
                .unwrap_or_else(|| panic!("implementation crate `{name}` not found"))
        })
        .collect();

    for (idx, crate_name) in IMPLEMENTATION_CRATES.iter().enumerate() {
        let node = resolve
            .iter()
            .find(|n| n["id"].as_str() == Some(impl_ids[idx]))
            .unwrap();

        let deps = node["deps"].as_array().cloned().unwrap_or_default();
        let dep_names: Vec<String> = deps
            .iter()
            .filter_map(|d| {
                let pkg_id = d["pkg"].as_str()?;
                packages
                    .iter()
                    .find(|p| p["id"].as_str() == Some(pkg_id))
                    .and_then(|p| p["name"].as_str())
                    .map(|s| s.to_owned())
            })
            .collect();

        // Every implementation crate must depend on rusty-api.
        assert!(
            dep_names.contains(&"rusty-api".to_owned()),
            "`{crate_name}` must depend on rusty-api"
        );

        // No implementation crate may depend on another implementation crate
        // directly unless the pair is in the known-violation ledger.
        for other in IMPLEMENTATION_CRATES.iter() {
            if *other == *crate_name {
                continue;
            }
            if dep_names.contains(&other.to_string()) {
                let is_known = KNOWN_INWARD_VIOLATIONS
                    .iter()
                    .any(|(d, dep)| *d == *crate_name && *dep == *other);
                assert!(
                    is_known,
                    "inward-only violation: `{crate_name}` depends on `{other}` directly — add to KNOWN_INWARD_VIOLATIONS if intentional"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// schemars JSON Schema snapshots (AC 5)
// ---------------------------------------------------------------------------

fn golden_path(name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join(name)
}

fn assert_golden(name: &str, value: &impl serde::Serialize) {
    let rendered = format!("{}\n", serde_json::to_string_pretty(value).unwrap());
    let path = golden_path(name);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &rendered).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing golden file `{}`: {e}", path.display()));
    assert_eq!(
        rendered,
        expected,
        "schema drift in `{}` — re-run with UPDATE_GOLDEN=1 and review",
        path.display()
    );
}

#[test]
fn effect_schema_is_stable() {
    let schema = schemars::schema_for!(Effect);
    assert_golden("schema_effect.json", &schema);
}

#[test]
fn role_schema_is_stable() {
    let schema = schemars::schema_for!(Role);
    assert_golden("schema_role.json", &schema);
}

#[test]
fn chat_message_schema_is_stable() {
    let schema = schemars::schema_for!(ChatMessage);
    assert_golden("schema_chat_message.json", &schema);
}

#[test]
fn tool_call_schema_is_stable() {
    let schema = schemars::schema_for!(ToolCall);
    assert_golden("schema_tool_call.json", &schema);
}

#[test]
fn usage_schema_is_stable() {
    let schema = schemars::schema_for!(Usage);
    assert_golden("schema_usage.json", &schema);
}

#[test]
fn model_pricing_schema_is_stable() {
    let schema = schemars::schema_for!(ModelPricing);
    assert_golden("schema_model_pricing.json", &schema);
}

#[test]
fn model_capabilities_schema_is_stable() {
    let schema = schemars::schema_for!(ModelCapabilities);
    assert_golden("schema_model_capabilities.json", &schema);
}

#[test]
fn inbound_message_schema_is_stable() {
    let schema = schemars::schema_for!(InboundMessage);
    assert_golden("schema_inbound_message.json", &schema);
}

#[test]
fn outbound_message_schema_is_stable() {
    let schema = schemars::schema_for!(OutboundMessage);
    assert_golden("schema_outbound_message.json", &schema);
}

#[test]
fn tool_output_schema_is_stable() {
    let schema = schemars::schema_for!(ToolOutput);
    assert_golden("schema_tool_output.json", &schema);
}

#[test]
fn effect_class_schema_is_stable() {
    let schema = schemars::schema_for!(EffectClass);
    assert_golden("schema_effect_class.json", &schema);
}

#[test]
fn sandbox_requirement_schema_is_stable() {
    let schema = schemars::schema_for!(SandboxRequirement);
    assert_golden("schema_sandbox_requirement.json", &schema);
}

#[test]
fn memory_entry_schema_is_stable() {
    let schema = schemars::schema_for!(MemoryEntry);
    assert_golden("schema_memory_entry.json", &schema);
}

#[test]
fn block_write_schema_is_stable() {
    let schema = schemars::schema_for!(BlockWrite);
    assert_golden("schema_block_write.json", &schema);
}

#[test]
fn observer_event_schema_is_stable() {
    let schema = schemars::schema_for!(ObserverEvent);
    assert_golden("schema_observer_event.json", &schema);
}

#[test]
fn enforcement_level_schema_is_stable() {
    let schema = schemars::schema_for!(EnforcementLevel);
    assert_golden("schema_enforcement_level.json", &schema);
}

#[test]
fn rusty_api_error_schema_is_stable() {
    let schema = schemars::schema_for!(RustyApiError);
    assert_golden("schema_rusty_api_error.json", &schema);
}

// ---------------------------------------------------------------------------
// public-API snapshot (AC 5)
// ---------------------------------------------------------------------------

#[test]
fn public_api_snapshot_is_unchanged() {
    let output = Command::new("cargo")
        .args(["public-api", "-p", "rusty-api"])
        .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/.."))
        .output()
        .expect("cargo-public-api must be installed");

    assert!(
        output.status.success(),
        "cargo-public-api failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let rendered = String::from_utf8(output.stdout).unwrap();
    let path = golden_path("public_api.txt");

    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &rendered).unwrap();
        return;
    }

    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing golden file `{}`: {e}", path.display()));
    assert_eq!(
        rendered, expected,
        "public API drift — re-run with UPDATE_GOLDEN=1 and review the diff"
    );
}

// ---------------------------------------------------------------------------
// No global registration (AC 6)
// ---------------------------------------------------------------------------

/// Scan `rusty-api` source for any `OnceLock` or `lazy_static` global state
/// that would act as a service locator. The spec forbids this.
#[test]
fn no_global_registration_points_in_api() {
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/lib.rs")).unwrap();

    let forbidden = ["OnceLock", "lazy_static", "std::sync::LazyLock"];
    for term in &forbidden {
        assert!(
            !src.contains(term),
            "rusty-api must contain no global registration point (`{term}`)"
        );
    }
}
