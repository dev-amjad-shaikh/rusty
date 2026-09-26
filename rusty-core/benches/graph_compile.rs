//! Benchmark: `GraphBuilder::compile()` at increasing graph sizes.
//!
//! Measures structural validation + graph assembly for linear chains of
//! 10 / 100 / 1000 nodes (n-1 static edges). The builder (node registration
//! and edge wiring) is constructed in setup — only `compile()` is measured.

mod common;

use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion};
use rusty_agent_runtime::prelude::*;
use serde_json::{json, Value};

/// Build an *uncompiled* chain builder of `n` nodes — the same wiring as
/// `common::chain_graph` but without calling `compile()`, since compile
/// consumes the builder and each iteration needs a fresh one.
fn uncompiled_chain(n: usize) -> GraphBuilder {
    let mut builder = GraphBuilder::new();
    for i in 0..n {
        let channel = format!("c{i}");
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
    builder
}

fn bench_graph_compile(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph_compile");
    for nodes in [10usize, 100, 1000] {
        group.bench_with_input(BenchmarkId::new("chain", nodes), &nodes, |b, &nodes| {
            b.iter_batched(
                || uncompiled_chain(nodes),
                |builder| builder.compile().expect("compiles"),
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

criterion_group!(benches, bench_graph_compile);
criterion_main!(benches);
