//! Benchmark: interrupt/resume round-trip.
//!
//! Measures one full human-in-the-loop cycle through the real executor and
//! checkpointer:
//!
//! 1. Phase 1 run — `start` completes, `human` interrupts, the in-flight
//!    super-step is unwound, and a checkpoint is persisted;
//! 2. Phase 2 resume — the executor loads the latest checkpoint, re-runs
//!    `human` with the resume value, and finishes.
//!
//! Run with `InMemoryCheckpointer` at two carried-state sizes: empty (pure
//! protocol overhead) and a 100 KB `blob` channel (protocol + checkpoint
//! write/read of a realistic payload). Each iteration gets a fresh
//! checkpointer via batched setup, so iterations are independent.

mod common;

use std::sync::Arc;

use common::{hitl_graph, state_sized, tokio_runtime};
use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion};
use rusty_agent_runtime::prelude::*;
use serde_json::json;

fn bench_interrupt_resume(c: &mut Criterion) {
    let rt = tokio_runtime();
    let mut group = c.benchmark_group("interrupt_resume_roundtrip");

    for (label, initial) in [
        ("empty_state", State::new()),
        ("blob_100kb", state_sized(102_400)),
    ] {
        group.bench_with_input(
            BenchmarkId::new("carried_state", label),
            &initial,
            |b, initial| {
                let (graph, spec) = hitl_graph();
                b.iter_batched(
                    // Fresh store + executor per iteration keeps threads
                    // isolated (checkpoint ids are per-thread unique).
                    || Executor::with_checkpointer(Arc::new(InMemoryCheckpointer::new())),
                    |executor| {
                        rt.block_on(async {
                            // Phase 1: suspend at `human`.
                            let outcome = executor
                                .run(&graph, &spec, initial.clone(), RunConfig::new("bench-hitl"))
                                .await
                                .expect("phase 1 run succeeds");
                            debug_assert!(outcome.is_interrupted());

                            // Phase 2: resume with the human's decision.
                            let outcome = executor
                                .run(
                                    &graph,
                                    &spec,
                                    State::new(), // ignored: checkpoint wins
                                    RunConfig::new("bench-hitl")
                                        .with_resume(json!({"approved": true})),
                                )
                                .await
                                .expect("phase 2 resume succeeds");
                            debug_assert!(!outcome.is_interrupted());
                            criterion::black_box(outcome)
                        })
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_interrupt_resume);
criterion_main!(benches);
