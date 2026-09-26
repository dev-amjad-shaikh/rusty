//! Benchmark: sequential node execution through the super-step executor.
//!
//! Runs linear chain graphs (10 / 50 / 100 nodes) end-to-end via
//! `Executor::run` with no checkpointer. Each node does real work — reads the
//! previous channel, increments, writes its own channel — so the measurement
//! is engine overhead (super-step loop, snapshot, barrier merge, routing)
//! plus genuine node execution, not a no-op graph.

mod common;

use common::{chain_graph, tokio_runtime};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use rusty_agent_runtime::prelude::*;

fn bench_sequential_execution(c: &mut Criterion) {
    let rt = tokio_runtime();
    let mut group = c.benchmark_group("sequential_chain_execution");

    for nodes in [10usize, 50, 100] {
        group.bench_with_input(BenchmarkId::new("nodes", nodes), &nodes, |b, &nodes| {
            let (graph, spec) = chain_graph(nodes);
            let executor = Executor::new();
            b.iter(|| {
                rt.block_on(async {
                    let outcome = executor
                        .run(&graph, &spec, State::new(), RunConfig::new("bench-seq"))
                        .await
                        .expect("run succeeds");
                    debug_assert!(!outcome.is_interrupted());
                    criterion::black_box(outcome)
                })
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_sequential_execution);
criterion_main!(benches);
