//! Shared helpers for the rusty-core benchmark suite.
//!
//! These helpers build *real* graphs, states, and checkpoints — the same
//! shapes exercised by the crate's examples and integration tests — so each
//! benchmark measures what its name claims.
//!
//! This module is not a bench target itself (`autobenches = false` in
//! Cargo.toml); it is included via `mod common;` from each bench file.
#![allow(dead_code)]

use rusty_agent_runtime::prelude::*;
use serde_json::{json, Value};

/// A JSON string payload of exactly `bytes` characters.
///
/// The benchmarks size states by their dominant payload: one large string
/// channel. A serde_json string of N chars costs ~N bytes in memory and
/// serializes to ~N+2 bytes of JSON, which keeps "state size" honest and
/// easy to reason about.
pub fn blob(bytes: usize) -> Value {
    Value::String("x".repeat(bytes))
}

/// A state carrying one `blob` string channel of `bytes` size, plus a small
/// `meta` object so the state is not degenerately one-key.
pub fn state_sized(bytes: usize) -> State {
    let mut state = State::new();
    state.insert("meta", json!({"kind": "bench", "payload_bytes": bytes}));
    state.insert("blob", blob(bytes));
    state
}

/// A linear chain graph with `n` nodes: `n0 -> n1 -> ... -> n{n-1}`.
///
/// Each node performs real (if small) work: it reads the previous node's
/// channel value, adds one, and writes its own channel. Node `n0` seeds the
/// chain with 0. Every channel is declared in the returned spec.
///
/// This is deliberately *not* a no-op graph: each node executes a state
/// read + arithmetic + update through the real super-step machinery.
pub fn chain_graph(n: usize) -> (Graph, StateSpec) {
    assert!(n >= 1, "chain must have at least one node");
    let mut spec = StateSpec::new();
    let mut builder = GraphBuilder::new();

    for i in 0..n {
        let channel = format!("c{i}");
        spec.add_channel(channel.clone(), Reducer::Overwrite);

        let prev_channel = (i > 0).then(|| format!("c{}", i - 1));
        builder.add_node(format!("n{i}"), move |ctx: NodeContext| {
            let channel = channel.clone();
            let prev_channel = prev_channel.clone();
            async move {
                let prev = prev_channel
                    .and_then(|p| ctx.state().get(&p).and_then(Value::as_u64))
                    .unwrap_or(0);
                Ok(NodeOutput::update(channel, json!(prev + 1)))
            }
        });

        if i > 0 {
            builder.add_edge(format!("n{}", i - 1), format!("n{i}"));
        }
    }

    builder.set_entry_point("n0");
    let graph = builder.compile().expect("chain graph compiles");
    (graph, spec)
}

/// A static fan-out / fan-in graph with `branches` parallel branches:
///
/// ```text
///        ┌─► b0 ─┐
/// source ┼─► b1 ─┼─► sink
///        └─► ... ┘
/// ```
///
/// `source` seeds the run; every branch node does a deterministic pure
/// computation (checksum of its branch index) and pushes one record onto
/// the `results` channel (`Reducer::Append`, so the concurrent writes are
/// legal); `sink` runs after the barrier and summarizes the merged array.
/// Same shape as `examples/parallel_fanout.rs` but with static edges so the
/// branch count is fixed at build time.
pub fn fanout_graph(branches: usize) -> (Graph, StateSpec) {
    assert!(branches >= 1, "fan-out must have at least one branch");
    let spec = StateSpec::new()
        .channel("seed", Reducer::Overwrite)
        .channel("results", Reducer::Append)
        .channel("summary", Reducer::Overwrite);

    let mut builder = GraphBuilder::new();
    builder.add_node("source", move |_ctx: NodeContext| async move {
        Ok(NodeOutput::update("seed", json!(branches)))
    });

    for i in 0..branches {
        builder.add_node(format!("b{i}"), move |_ctx: NodeContext| async move {
            // Pure, deterministic stand-in for per-branch work.
            let checksum: u64 = (i as u64).wrapping_mul(2_654_435_761) % 1_000_003;
            Ok(NodeOutput::update(
                "results",
                json!({"branch": i, "checksum": checksum}),
            ))
        });
        builder.add_edge("source", format!("b{i}"));
        builder.add_edge(format!("b{i}"), "sink");
    }

    builder.add_node("sink", |ctx: NodeContext| async move {
        let results = ctx
            .state()
            .get("results")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let total: u64 = results
            .iter()
            .filter_map(|r| r.get("checksum").and_then(Value::as_u64))
            .sum();
        Ok(NodeOutput::update(
            "summary",
            json!({"count": results.len(), "total_checksum": total}),
        ))
    });

    builder.set_entry_point("source");
    let graph = builder.compile().expect("fan-out graph compiles");
    (graph, spec)
}

/// A human-in-the-loop graph: `start -> human`, where `human` interrupts
/// until a resume value arrives. Mirrors `tests/interrupt_resume.rs`.
///
/// The initial state passed to the first run may carry a large `blob`
/// channel so the round-trip cost includes checkpointing that state.
pub fn hitl_graph() -> (Graph, StateSpec) {
    let spec = StateSpec::new()
        .channel("blob", Reducer::Overwrite)
        .channel("greeting", Reducer::Overwrite)
        .channel("answer", Reducer::Overwrite);

    let mut builder = GraphBuilder::new();
    builder.add_node("start", |_ctx: NodeContext| async {
        Ok(NodeOutput::update("greeting", json!("hello")))
    });
    builder.add_node("human", |ctx: NodeContext| async move {
        match ctx.resume_value() {
            Some(v) => Ok(NodeOutput::update("answer", v.clone())),
            None => Err(ctx.interrupt(json!({"question": "approve?"}))),
        }
    });
    builder.set_entry_point("start");
    builder.add_edge("start", "human");
    let graph = builder.compile().expect("HITL graph compiles");
    (graph, spec)
}

/// The state spec matching [`state_sized`].
pub fn sized_spec() -> StateSpec {
    StateSpec::new()
        .channel("meta", Reducer::Overwrite)
        .channel("blob", Reducer::Overwrite)
}

/// A multi-threaded tokio runtime for async benches. One runtime per bench
/// binary, created outside the measurement loops so runtime startup is never
/// part of a measurement.
pub fn tokio_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Runtime::new().expect("tokio runtime starts")
}

/// A unique directory under the OS temp dir for file-checkpointer benches.
/// The caller is responsible for cleanup.
pub fn temp_checkpoint_root(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("rusty-bench-checkpoints-{tag}"))
}
