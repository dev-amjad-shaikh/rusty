//! Benchmark: parallel fan-out / fan-in through the super-step executor.
//!
//! Runs static fan-out graphs (source -> N branch nodes -> sink) with 2 / 8
//! / 32 branches end-to-end via `Executor::run` with no checkpointer. All
//! branch nodes run in the same super-step as parallel tokio tasks; their
//! concurrent writes to the `results` channel merge via `Reducer::Append` at
//! the barrier — the same shape as `examples/parallel_fanout.rs`.

mod common;

use common::{fanout_graph, tokio_runtime};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use rusty_agent_runtime::prelude::*;

fn bench_parallel_fanout(c: &mut Criterion) {
    let rt = tokio_runtime();
    let mut group = c.benchmark_group("parallel_fanout_fanin");

    for branches in [2usize, 8, 32] {
        group.bench_with_input(
            BenchmarkId::new("branches", branches),
            &branches,
            |b, &branches| {
                let (graph, spec) = fanout_graph(branches);
                let executor = Executor::new();
                b.iter(|| {
                    rt.block_on(async {
                        let outcome = executor
                            .run(&graph, &spec, State::new(), RunConfig::new("bench-fanout"))
                            .await
                            .expect("run succeeds");
                        debug_assert!(!outcome.is_interrupted());
                        criterion::black_box(outcome)
                    })
                });
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_parallel_fanout);
criterion_main!(benches);
