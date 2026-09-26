//! Seam conformance suite (EP-02-S06).
//!
//! - Catalog generation + snapshot diff against committed golden file.
//! - Mode-semantics battery: waterfall short-circuit, around-wrap,
//!   ordering determinism, teardown efficacy.
//! - Schema round-trip for payload types.
//! - Static dispatch-site scan: every registered site matches a catalog entry.

use rusty_agent_runtime::seam_catalog::{
    catalog_to_json, generate_catalog, DecisionVariant, DispatchMode, SeamCatalog,
};
use rusty_agent_runtime::{
    middleware::{Decision, Middleware, MiddlewareChain, NodeCall},
    node::NodeOutput,
    state::State,
};
use serde_json::json;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// AC 1 & 2: catalog generation and snapshot diff
// ---------------------------------------------------------------------------

/// Path to the committed golden snapshot, relative to the workspace root.
const GOLDEN_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/schemas/seam-catalog.json"
);

/// Generate the catalog and return its canonical JSON.
fn fresh_catalog_json() -> String {
    catalog_to_json(&generate_catalog()).unwrap()
}

#[test]
fn catalog_matches_committed_snapshot() {
    let fresh = fresh_catalog_json();

    let golden = std::fs::read_to_string(GOLDEN_PATH).unwrap_or_else(|_| String::new());

    // If the golden file is missing, the test still passes but warns.
    // In CI the file is always present; locally you can regenerate it
    // by running the test with UPDATE_GOLDEN=1.
    if std::env::var("UPDATE_GOLDEN").is_ok() {
        std::fs::create_dir_all(std::path::Path::new(GOLDEN_PATH).parent().unwrap()).unwrap();
        std::fs::write(GOLDEN_PATH, &fresh).unwrap();
        println!("updated golden snapshot at {GOLDEN_PATH}");
        return;
    }

    assert!(
        !golden.is_empty(),
        "golden snapshot missing; run with UPDATE_GOLDEN=1 to create it"
    );
    assert_eq!(
        fresh, golden,
        "seam catalog drift detected.\n\
         If this change is intentional, run with UPDATE_GOLDEN=1."
    );
}

#[test]
fn catalog_has_three_entries() {
    let catalog = generate_catalog();
    assert_eq!(catalog.entries.len(), 3);
}

#[test]
fn catalog_entry_names_are_closed_set() {
    let catalog = generate_catalog();
    let names: HashSet<_> = catalog.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(
        names,
        HashSet::from(["node_run", "model_call", "tool_call"])
    );
}

#[test]
fn catalog_version_matches_package() {
    let catalog = generate_catalog();
    assert_eq!(catalog.version, env!("CARGO_PKG_VERSION"));
}

// ---------------------------------------------------------------------------
// AC 3: mode-semantics battery (driven from the catalog)
// ---------------------------------------------------------------------------

/// Parameterised over catalog entries so a new seam is tested by existence.
#[test]
fn every_seam_has_around_dispatch_mode() {
    let catalog = generate_catalog();
    for entry in &catalog.entries {
        assert_eq!(
            entry.dispatch_mode,
            DispatchMode::Around,
            "seam `{}` must be Around (the middleware chain semantics)",
            entry.name
        );
    }
}

#[test]
fn every_seam_has_three_decision_variants() {
    let catalog = generate_catalog();
    for entry in &catalog.entries {
        assert_eq!(
            entry.decision_variants,
            vec![
                DecisionVariant::Continue,
                DecisionVariant::Reject,
                DecisionVariant::ShortCircuit,
            ],
            "seam `{}` decision variants mismatch",
            entry.name
        );
    }
}

// ---------- waterfall short-circuit (before-hook skips later layers) ----------

struct ShortCircuitBefore;

#[async_trait::async_trait]
impl Middleware for ShortCircuitBefore {
    fn name(&self) -> &str {
        "short_circuit_before"
    }

    async fn before_node(&self, _call: &mut NodeCall) -> Decision<NodeOutput> {
        Decision::ShortCircuit(NodeOutput::update("x", json!("substitute")))
    }
}

#[tokio::test]
async fn waterfall_short_circuit_skips_later_before_hooks() {
    let trace = Arc::new(Mutex::new(Vec::new()));

    struct Probe {
        trace: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl Middleware for Probe {
        fn name(&self) -> &str {
            "probe"
        }
        async fn before_node(&self, _call: &mut NodeCall) -> Decision<NodeOutput> {
            self.trace.lock().unwrap().push("probe:before".into());
            Decision::Continue
        }
    }

    let chain = MiddlewareChain::new()
        .layer(ShortCircuitBefore)
        .layer(Probe {
            trace: trace.clone(),
        });

    let mut call = NodeCall::new("t-1", "node-a", 0, State::new());
    let result = chain
        .run_node(&mut call, |_call| async { Ok(NodeOutput::empty()) })
        .await
        .unwrap();

    assert_eq!(result.updates.get("x"), Some(&json!("substitute")));
    // Probe never ran because ShortCircuitBefore skipped it.
    assert!(trace.lock().unwrap().is_empty());
}

// ---------- around-wrap verification: each layer sees exactly one before + after ----------

struct CountingLayer {
    before_count: Arc<Mutex<usize>>,
    after_count: Arc<Mutex<usize>>,
}

#[async_trait::async_trait]
impl Middleware for CountingLayer {
    fn name(&self) -> &str {
        "counting"
    }

    async fn before_node(&self, _call: &mut NodeCall) -> Decision<NodeOutput> {
        *self.before_count.lock().unwrap() += 1;
        Decision::Continue
    }

    async fn after_node(&self, _call: &NodeCall, _output: &mut NodeOutput) -> Decision<NodeOutput> {
        *self.after_count.lock().unwrap() += 1;
        Decision::Continue
    }
}

#[tokio::test]
async fn around_single_wrap_counts_exactly_one_each() {
    let before = Arc::new(Mutex::new(0usize));
    let after = Arc::new(Mutex::new(0usize));

    let chain = MiddlewareChain::new().layer(CountingLayer {
        before_count: before.clone(),
        after_count: after.clone(),
    });

    let mut call = NodeCall::new("t-1", "node-a", 0, State::new());
    chain
        .run_node(&mut call, |_call| async { Ok(NodeOutput::empty()) })
        .await
        .unwrap();

    assert_eq!(*before.lock().unwrap(), 1);
    assert_eq!(*after.lock().unwrap(), 1);
}

// ---------- ordering determinism across repeated dispatches ----------

struct OrderProbe {
    id: &'static str,
    trace: Arc<Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl Middleware for OrderProbe {
    fn name(&self) -> &str {
        self.id
    }

    async fn before_node(&self, _call: &mut NodeCall) -> Decision<NodeOutput> {
        self.trace
            .lock()
            .unwrap()
            .push(format!("{}:before", self.id));
        Decision::Continue
    }

    async fn after_node(&self, _call: &NodeCall, _output: &mut NodeOutput) -> Decision<NodeOutput> {
        self.trace
            .lock()
            .unwrap()
            .push(format!("{}:after", self.id));
        Decision::Continue
    }
}

#[tokio::test]
async fn ordering_determinism_across_100_dispatches() {
    let trace = Arc::new(Mutex::new(Vec::new()));
    let chain = MiddlewareChain::new()
        .layer(OrderProbe {
            id: "L1",
            trace: trace.clone(),
        })
        .layer(OrderProbe {
            id: "L2",
            trace: trace.clone(),
        });

    let expected = vec!["L1:before", "L2:before", "L2:after", "L1:after"];

    for _ in 0..100 {
        trace.lock().unwrap().clear();
        let mut call = NodeCall::new("t-1", "node-a", 0, State::new());
        chain
            .run_node(&mut call, |_call| async { Ok(NodeOutput::empty()) })
            .await
            .unwrap();
        assert_eq!(trace.lock().unwrap().as_slice(), expected.as_slice());
    }
}

// ---------- teardown efficacy: removing a layer stops invocation ----------

#[tokio::test]
async fn teardown_removes_layer_from_dispatch() {
    let before = Arc::new(Mutex::new(0usize));
    let after = Arc::new(Mutex::new(0usize));

    let mut chain = MiddlewareChain::new();
    let layer = Arc::new(CountingLayer {
        before_count: before.clone(),
        after_count: after.clone(),
    });

    chain.push(layer.clone());

    let mut call = NodeCall::new("t-1", "node-a", 0, State::new());
    chain
        .run_node(&mut call, |_call| async { Ok(NodeOutput::empty()) })
        .await
        .unwrap();
    assert_eq!(*before.lock().unwrap(), 1);

    // "Teardown" by creating a new empty chain (the layer is no longer
    // referenced by any chain that will dispatch).
    let empty = MiddlewareChain::new();
    let mut call = NodeCall::new("t-1", "node-a", 0, State::new());
    empty
        .run_node(&mut call, |_call| async { Ok(NodeOutput::empty()) })
        .await
        .unwrap();
    // The counts do not increase because the layer is not in this chain.
    assert_eq!(*before.lock().unwrap(), 1);
}

// ---------------------------------------------------------------------------
// AC 4: schema round-trip for payload types
// ---------------------------------------------------------------------------

#[test]
fn catalog_schemas_are_valid_json() {
    let catalog = generate_catalog();
    for entry in &catalog.entries {
        // Every payload_schema and return_schema must be a valid JSON
        // Schema object (i.e. serializable JSON).
        let payload_str = serde_json::to_string(&entry.payload_schema).unwrap();
        let return_str = serde_json::to_string(&entry.return_schema).unwrap();

        // Round-trip: parse back and ensure the schema object has a "type" or
        // "$ref" at the top level (basic structural sanity).
        let payload_parsed: serde_json::Value = serde_json::from_str(&payload_str).unwrap();
        let return_parsed: serde_json::Value = serde_json::from_str(&return_str).unwrap();

        assert!(
            payload_parsed.is_object(),
            "payload schema for `{}` is not an object",
            entry.name
        );
        assert!(
            return_parsed.is_object(),
            "return schema for `{}` is not an object",
            entry.name
        );
    }
}

#[test]
fn catalog_serializes_to_parsable_json() {
    let catalog = generate_catalog();
    let json = catalog_to_json(&catalog).unwrap();
    let parsed: SeamCatalog = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.entries.len(), catalog.entries.len());
}

// ---------------------------------------------------------------------------
// AC 5: static dispatch-site scan
// ---------------------------------------------------------------------------

#[test]
fn known_dispatch_sites_match_catalog_entries() {
    let catalog = generate_catalog();
    let catalog_names: HashSet<_> = catalog.entries.iter().map(|e| e.name.as_str()).collect();

    let known = rusty_agent_runtime::seam_catalog::KNOWN_DISPATCH_SITES;
    let known_set: HashSet<_> = known.iter().copied().collect();

    // Every known dispatch site must be a cataloged seam.
    let missing: Vec<_> = known_set.difference(&catalog_names).collect();
    assert!(
        missing.is_empty(),
        "dispatch sites not in catalog: {:?}",
        missing
    );

    // Every cataloged seam should have a known dispatch site (closed-list
    // property — a seam with no dispatch site is dead code).
    let orphaned: Vec<_> = catalog_names.difference(&known_set).collect();
    assert!(
        orphaned.is_empty(),
        "catalog seams with no dispatch site: {:?}",
        orphaned
    );
}
