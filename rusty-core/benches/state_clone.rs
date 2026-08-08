//! Benchmark: full-state cloning cost at increasing state sizes.
//!
//! The executor hands every node an immutable state snapshot, and
//! checkpoints serialize the full state — so deep-cloning `State` is on the
//! hot path of every super-step. These benches pin down where that cost
//! becomes visible: 1 KB / 100 KB / 1 MB / 10 MB payload states.
//!
//! Two operations are measured:
//!
//! - `state_clone`: `State::clone()` — a deep clone of the underlying
//!   `serde_json::Map<String, Value>`;
//! - `state_serde_roundtrip`: `serde_json::to_string` + `from_str` — the
//!   full serialize/parse cycle a durable checkpoint pays in both
//!   directions;
//! - `superstep_snapshot_clones` (R0.7 wave 4): the executor's per-step
//!   fan-out — one pre-step snapshot, one immutable clone per concurrent
//!   node (4), one clone for the checkpoint mint. Channel-granularity
//!   copy-on-write turns each clone into an Arc bump, so this group shows
//!   the win `state_clone` (a single isolated clone) cannot.
//!
//! Note: a 10 MB payload means a single ~10 MB JSON string; string clones
//! are memcpy-fast, so this is a *lower bound* — a structurally deep 10 MB
//! state (many small values) clones slower per byte.

mod common;

use common::state_sized;
use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput};
use rusty_agent_runtime::prelude::State;

const SIZES: [usize; 4] = [1_024, 102_400, 1_048_576, 10_485_760];

fn bench_state_clone(c: &mut Criterion) {
    let mut group = c.benchmark_group("state_clone");
    for bytes in SIZES {
        group.throughput(Throughput::Bytes(bytes as u64));
        group.bench_with_input(
            BenchmarkId::new("payload_bytes", bytes),
            &bytes,
            |b, &bytes| {
                let state = state_sized(bytes);
                b.iter(|| criterion::black_box(state.clone()));
            },
        );
    }
    group.finish();
}

fn bench_state_serde_roundtrip(c: &mut Criterion) {
    let mut group = c.benchmark_group("state_serde_roundtrip");
    for bytes in SIZES {
        group.throughput(Throughput::Bytes(bytes as u64));
        group.bench_with_input(
            BenchmarkId::new("payload_bytes", bytes),
            &bytes,
            |b, &bytes| {
                b.iter_batched(
                    || state_sized(bytes),
                    |state| {
                        let text = serde_json::to_string(&state).expect("serializes");
                        let back: State = serde_json::from_str(&text).expect("parses");
                        criterion::black_box(back)
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }
    group.finish();
}

fn bench_superstep_snapshot_clones(c: &mut Criterion) {
    let mut group = c.benchmark_group("superstep_snapshot_clones");
    for bytes in [1_048_576usize, 10_485_760] {
        group.throughput(Throughput::Bytes(bytes as u64));
        group.bench_with_input(
            BenchmarkId::new("payload_bytes", bytes),
            &bytes,
            |b, &bytes| {
                let state = state_sized(bytes);
                b.iter(|| {
                    // Executor super-step fan-out: the pre-step snapshot plus
                    // one immutable clone per concurrent node and one for the
                    // checkpoint mint. W4 makes each clone an Arc bump;
                    // pre-W4 each was a full deep copy of the payload.
                    let snapshot = state.clone();
                    let node_clones: Vec<State> = (0..4).map(|_| snapshot.clone()).collect();
                    let checkpoint_copy = snapshot.clone();
                    criterion::black_box((node_clones, checkpoint_copy))
                });
            },
        );
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_state_clone,
    bench_state_serde_roundtrip,
    bench_superstep_snapshot_clones
);
criterion_main!(benches);
