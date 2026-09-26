//! Benchmark: checkpoint serialization + save at increasing state sizes.
//!
//! Covers the three layers of the checkpoint write path at state sizes of
//! 1 KB / 100 KB / 1 MB:
//!
//! - `serialize`: `serde_json::to_vec_pretty(&Checkpoint)` — pure CPU cost,
//!   the same serialization `JsonFileCheckpointer` performs internally;
//! - `in_memory_put`: `InMemoryCheckpointer::put` — mutex + clone-free
//!   move into the store;
//! - `json_file_put`: `JsonFileCheckpointer::put` — serialize + atomic
//!   temp-write + rename + latest-pointer write, pinned to the pre-W4
//!   full-write path (`DeltaPolicy::full_only`) so the published baseline
//!   stays a like-for-like regression guard;
//! - `json_file_get_latest`: `JsonFileCheckpointer::get_latest` — pointer
//!   read + file read + deserialize (the resume-path load cost);
//! - `json_file_put_delta` / `json_file_get_latest_delta` (R0.7 wave 4):
//!   the same writes/reads through the default delta policy — a chain of
//!   checkpoints whose large `blob` channel is unchanged between steps, so
//!   every put after the first persists only the small changed channel.
//!
//! A `DELTA-ACCOUNT` accounting pass (untimed, printed) measures the exit
//! metric timing cannot: total on-disk bytes and wall time for 1000
//! checkpoints of a 1 MB state, full-only vs delta policy.
//!
//! File benches use a dedicated temp root that is cleaned before and after.

mod common;

use common::{state_sized, temp_checkpoint_root, tokio_runtime};
use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion};
use rusty_agent_runtime::checkpoint::DeltaPolicy;
use rusty_agent_runtime::prelude::*;
use serde_json::json;

const SIZES: [usize; 3] = [1_024, 102_400, 1_048_576];

fn make_checkpoint(bytes: usize, step: usize) -> Checkpoint {
    Checkpoint::new(
        "bench-thread",
        step,
        state_sized(bytes),
        vec!["next".into()],
    )
}

fn bench_serialize(c: &mut Criterion) {
    let mut group = c.benchmark_group("checkpoint_serialize");
    for bytes in SIZES {
        group.bench_with_input(
            BenchmarkId::new("state_bytes", bytes),
            &bytes,
            |b, &bytes| {
                let checkpoint = make_checkpoint(bytes, 0);
                b.iter(|| {
                    let out = serde_json::to_vec_pretty(&checkpoint).expect("serializes");
                    criterion::black_box(out)
                });
            },
        );
    }
    group.finish();
}

fn bench_in_memory_put(c: &mut Criterion) {
    let rt = tokio_runtime();
    let mut group = c.benchmark_group("checkpoint_in_memory_put");
    for bytes in SIZES {
        group.bench_with_input(
            BenchmarkId::new("state_bytes", bytes),
            &bytes,
            |b, &bytes| {
                let store = InMemoryCheckpointer::new();
                b.iter_batched(
                    || make_checkpoint(bytes, 0),
                    |checkpoint| {
                        rt.block_on(async {
                            store.put(checkpoint).await.expect("put succeeds");
                        })
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }
    group.finish();
}

fn bench_json_file(c: &mut Criterion) {
    let rt = tokio_runtime();
    let root = temp_checkpoint_root("json-file");
    let _ = std::fs::remove_dir_all(&root);

    let mut group = c.benchmark_group("checkpoint_json_file_put");
    for bytes in SIZES {
        group.bench_with_input(
            BenchmarkId::new("state_bytes", bytes),
            &bytes,
            |b, &bytes| {
                // W4: full-only policy pins the pre-wave-4 write path. Under
                // the default delta policy these identical-content puts
                // would degrade to empty deltas and stop measuring the
                // full-write cost this group publishes.
                let store =
                    JsonFileCheckpointer::with_delta_policy(root.clone(), DeltaPolicy::full_only());
                b.iter_batched(
                    // Fresh checkpoint per iteration: ids are unique by
                    // construction, so puts never collide on disk.
                    || make_checkpoint(bytes, 0),
                    |checkpoint| {
                        rt.block_on(async {
                            store.put(checkpoint).await.expect("put succeeds");
                        })
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }
    group.finish();

    // Load path: one checkpoint pre-saved per size; each iteration reads and
    // deserializes it (the pointer fast path, as on resume). Full-only
    // policy here too: the comparison is against the pre-W4 single-file
    // load, not the chain fold measured below.
    let mut group = c.benchmark_group("checkpoint_json_file_get_latest");
    for bytes in SIZES {
        group.bench_with_input(
            BenchmarkId::new("state_bytes", bytes),
            &bytes,
            |b, &bytes| {
                let store =
                    JsonFileCheckpointer::with_delta_policy(root.clone(), DeltaPolicy::full_only());
                let thread = format!("load-{bytes}");
                rt.block_on(async {
                    store
                        .put(Checkpoint::new(
                            thread.clone(),
                            0,
                            state_sized(bytes),
                            vec!["next".into()],
                        ))
                        .await
                        .expect("seed put succeeds");
                });
                b.iter(|| {
                    rt.block_on(async {
                        let cp = store
                            .get_latest(&thread)
                            .await
                            .expect("get_latest succeeds")
                            .expect("checkpoint exists");
                        criterion::black_box(cp)
                    })
                });
            },
        );
    }
    group.finish();

    let _ = std::fs::remove_dir_all(&root);
}

/// R0.7 wave 4: the delta write path. One long chain per size; every step
/// rewrites only the small `meta` channel while the large `blob` channel is
/// byte-identical, so puts after the first persist a small delta (the chain
/// re-anchors to a full write every `DeltaPolicy::max_chain_len` steps —
/// realistic steady state, not a best case).
fn bench_json_file_delta(c: &mut Criterion) {
    let rt = tokio_runtime();
    let root = temp_checkpoint_root("json-file-delta");
    let _ = std::fs::remove_dir_all(&root);

    let mut group = c.benchmark_group("checkpoint_json_file_put_delta");
    for bytes in SIZES {
        group.bench_with_input(
            BenchmarkId::new("state_bytes", bytes),
            &bytes,
            |b, &bytes| {
                let store = JsonFileCheckpointer::new(root.clone());
                let thread = format!("delta-put-{bytes}");
                let step = std::cell::Cell::new(0usize);
                b.iter(|| {
                    let n = step.get();
                    step.set(n + 1);
                    let mut state = state_sized(bytes);
                    state.insert("meta", json!({"kind": "bench", "step": n}));
                    let checkpoint = Checkpoint::new(thread.clone(), n, state, vec!["next".into()]);
                    rt.block_on(async {
                        store.put(checkpoint).await.expect("put succeeds");
                    })
                });
            },
        );
    }
    group.finish();

    // Resume-path cost of a delta chain at its bounded worst: the head sits
    // `max_chain_len - 1` deltas above its full base, so every get_latest
    // reads + folds the whole chain.
    let mut group = c.benchmark_group("checkpoint_json_file_get_latest_delta");
    for bytes in SIZES {
        group.bench_with_input(
            BenchmarkId::new("state_bytes", bytes),
            &bytes,
            |b, &bytes| {
                let store = JsonFileCheckpointer::new(root.clone());
                let thread = format!("delta-load-{bytes}");
                let chain = DeltaPolicy::default().max_chain_len;
                rt.block_on(async {
                    for n in 0..chain {
                        let mut state = state_sized(bytes);
                        state.insert("meta", json!({"kind": "bench", "step": n}));
                        store
                            .put(Checkpoint::new(
                                thread.clone(),
                                n,
                                state,
                                vec!["next".into()],
                            ))
                            .await
                            .expect("seed put succeeds");
                    }
                });
                b.iter(|| {
                    rt.block_on(async {
                        let cp = store
                            .get_latest(&thread)
                            .await
                            .expect("get_latest succeeds")
                            .expect("checkpoint exists");
                        criterion::black_box(cp)
                    })
                });
            },
        );
    }
    group.finish();

    let _ = std::fs::remove_dir_all(&root);
}

/// Total bytes under `dir`, recursively (checkpoint files + latest pointers).
fn dir_bytes(dir: &std::path::Path) -> u64 {
    let mut total = 0;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                total += dir_bytes(&path);
            } else {
                total += entry.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    total
}

/// R0.7 wave 4 exit metric, untimed by Criterion on purpose: 1000
/// checkpoints of a 1 MB state (blob constant, `meta` rewritten per step),
/// full-only vs delta policy. Prints total on-disk bytes and wall time per
/// configuration with a `DELTA-ACCOUNT` prefix and asserts the delta total
/// is at least 4x smaller — the expected ratio is far above that (~30x:
/// 32 full anchors + 968 small deltas vs 1000 full writes).
fn delta_accounting(_c: &mut Criterion) {
    const STEPS: usize = 1000;
    const STATE_BYTES: usize = 1_048_576;
    let rt = tokio_runtime();
    let root = temp_checkpoint_root("delta-account");
    let _ = std::fs::remove_dir_all(&root);

    println!("# delta accounting — 1000 checkpoints x 1MB state, one untimed run per policy");
    let mut totals = Vec::new();
    for (label, policy) in [
        ("full-only", DeltaPolicy::full_only()),
        ("delta", DeltaPolicy::default()),
    ] {
        let thread = format!("account-{label}");
        let store = JsonFileCheckpointer::with_delta_policy(root.clone(), policy);
        let start = std::time::Instant::now();
        rt.block_on(async {
            for n in 0..STEPS {
                let mut state = state_sized(STATE_BYTES);
                state.insert("meta", json!({"kind": "bench", "step": n}));
                store
                    .put(Checkpoint::new(
                        thread.clone(),
                        n,
                        state,
                        vec!["next".into()],
                    ))
                    .await
                    .expect("put succeeds");
            }
        });
        let wall = start.elapsed();
        let bytes = dir_bytes(&root.join(&thread));
        println!(
            "DELTA-ACCOUNT policy={label} steps={STEPS} state_bytes={STATE_BYTES} \
             dir_bytes={bytes} wall_ms={:.0}",
            wall.as_secs_f64() * 1_000.0
        );
        totals.push((label, bytes));
    }
    let full = totals[0].1;
    let delta = totals[1].1;
    assert!(
        delta * 4 < full,
        "delta policy must cut on-disk bytes at least 4x (full={full}, delta={delta})"
    );

    let _ = std::fs::remove_dir_all(&root);
}

criterion_group!(
    benches,
    bench_serialize,
    bench_in_memory_put,
    bench_json_file,
    bench_json_file_delta,
    delta_accounting
);
criterion_main!(benches);
