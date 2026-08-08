//! Benchmark: reducer merge cost at increasing state sizes.
//!
//! Measures `StateSpec::apply_single` — the real barrier merge path
//! (channel validation + reducer application), not raw `Reducer::apply` —
//! for the three core reducers:
//!
//! - `Overwrite` with a large replacement value (expected ~flat: the update
//!   is moved, not merged);
//! - `Append` pushing one element onto an existing array of N elements
//!   (pre-W4: O(N) clone per write; W4: in-place when the state's channel
//!   is uniquely owned, as in these benches);
//! - `DeepMerge` merging a K-key object into an existing K-key object
//!   (same unique-ownership in-place path);
//! - `Append` with a *shared* channel (`reducer_append_shared`, R0.7 wave
//!   4): a snapshot clone is alive during the merge, so copy-on-write must
//!   clone the channel before appending — the honest cost of merging into
//!   a state the executor's pre-step snapshot still references.

mod common;

use std::collections::HashMap;

use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion};
use rusty_agent_runtime::prelude::*;
use serde_json::{json, Map, Value};

fn updates(pairs: &[(&str, Value)]) -> HashMap<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

/// A JSON object with `keys` keys (`k0..kN`, small nested values).
fn object_with_keys(keys: usize) -> Value {
    let mut map = Map::new();
    for i in 0..keys {
        map.insert(format!("k{i}"), json!({"v": i, "nested": {"x": i}}));
    }
    Value::Object(map)
}

fn bench_overwrite(c: &mut Criterion) {
    let mut group = c.benchmark_group("reducer_overwrite");
    let spec = StateSpec::new().channel("blob", Reducer::Overwrite);
    for bytes in [1_024usize, 102_400, 1_048_576] {
        group.bench_with_input(
            BenchmarkId::new("value_bytes", bytes),
            &bytes,
            |b, &bytes| {
                b.iter_batched(
                    || {
                        let mut state = State::new();
                        state.insert("blob", common::blob(bytes));
                        (state, updates(&[("blob", common::blob(bytes))]))
                    },
                    |(mut state, updates)| {
                        spec.apply_single(&mut state, "node", updates)
                            .expect("merge succeeds");
                        criterion::black_box(state)
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }
    group.finish();
}

fn bench_append(c: &mut Criterion) {
    let mut group = c.benchmark_group("reducer_append");
    let spec = StateSpec::new().channel("items", Reducer::Append);
    for len in [10usize, 100, 1_000, 10_000] {
        group.bench_with_input(BenchmarkId::new("existing_len", len), &len, |b, &len| {
            b.iter_batched(
                || {
                    let mut state = State::new();
                    let arr: Vec<Value> = (0..len).map(|i| json!({"i": i})).collect();
                    state.insert("items", Value::Array(arr));
                    (state, updates(&[("items", json!({"i": len}))]))
                },
                |(mut state, updates)| {
                    spec.apply_single(&mut state, "node", updates)
                        .expect("merge succeeds");
                    criterion::black_box(state)
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

fn bench_deep_merge(c: &mut Criterion) {
    let mut group = c.benchmark_group("reducer_deep_merge");
    let spec = StateSpec::new().channel("cfg", Reducer::DeepMerge);
    for keys in [100usize, 1_000, 10_000] {
        group.bench_with_input(
            BenchmarkId::new("existing_keys", keys),
            &keys,
            |b, &keys| {
                b.iter_batched(
                    || {
                        let mut state = State::new();
                        state.insert("cfg", object_with_keys(keys));
                        // Update touches 10% of the keys (overlap → recursion).
                        (state, updates(&[("cfg", object_with_keys(keys / 10))]))
                    },
                    |(mut state, updates)| {
                        spec.apply_single(&mut state, "node", updates)
                            .expect("merge succeeds");
                        criterion::black_box(state)
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }
    group.finish();
}

/// R0.7 wave 4: `Append` against a channel the pre-step snapshot still
/// shares. The merge must copy the channel (copy-on-write) before
/// appending — unlike `reducer_append`, where the state uniquely owns the
/// channel and the W4 in-place path applies. This is the durable-run cost:
/// the executor's snapshot always shares the channels being merged.
fn bench_append_shared(c: &mut Criterion) {
    let mut group = c.benchmark_group("reducer_append_shared");
    let spec = StateSpec::new().channel("items", Reducer::Append);
    for len in [10usize, 100, 1_000, 10_000] {
        group.bench_with_input(BenchmarkId::new("existing_len", len), &len, |b, &len| {
            b.iter_batched(
                || {
                    let mut state = State::new();
                    let arr: Vec<Value> = (0..len).map(|i| json!({"i": i})).collect();
                    state.insert("items", Value::Array(arr));
                    // A live snapshot holds a second Arc on the channel, so
                    // the merge cannot mutate in place.
                    let snapshot = state.clone();
                    (state, snapshot, updates(&[("items", json!({"i": len}))]))
                },
                |(mut state, snapshot, updates)| {
                    spec.apply_single(&mut state, "node", updates)
                        .expect("merge succeeds");
                    criterion::black_box((state, snapshot))
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_overwrite,
    bench_append,
    bench_deep_merge,
    bench_append_shared
);
criterion_main!(benches);
