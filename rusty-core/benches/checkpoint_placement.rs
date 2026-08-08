//! Benchmark: checkpoint-placement headroom — the R0.5 Flight Recorder
//! "first experiment".
//!
//! Question (`docs/roadmap.md`, R0.5 → R0.10 gate): after the checkpoints a
//! run is *forced* to keep — the boundary after every super-step containing
//! a `NonIdempotent` effect — does any placement freedom remain worth
//! learning, or does the mandatory floor already behave like checkpointing
//! every super-step?
//!
//! Two measurement layers over synthetic super-step chains (50 / 200 / 1000
//! steps, one node per step) whose nodes carry declared [`Effect`] classes:
//! a deterministic mix of mostly `Pure`, 20 % `ReadOnly`, and
//! `NonIdempotent` at 2 % and 10 % densities.
//!
//! - **End-to-end** (`placement_e2e_chain*`): full `Executor::run` against a
//!   [`JsonFileCheckpointer`] behind a bench-local [`PlacementCheckpointer`]
//!   that drops the `put`s its policy does not select. Wall time includes
//!   node execution, Flight Recorder journaling, and per-step checkpoint
//!   minting — the whole R0.5 system as shipped. State size is 10 KB: with
//!   larger states the in-memory journal (which retains every step's input
//!   payload) dominates the run — a separate scaling question from
//!   placement, out of scope here.
//! - **Checkpoint stream** (`placement_stream_*`): the persistence half in
//!   isolation. For each boundary the policy keeps, mint + `put` a real
//!   checkpoint of the given state size (10 KB / 1 MB) through a real
//!   [`JsonFileCheckpointer`]. No executor, no journal: this isolates the
//!   durable-write cost a placement policy actually controls.
//!
//! Policies: `uniform` (every boundary — the executor's current behavior),
//! `terminal_only` (the final boundary only), `mandatory_only` (boundaries
//! after super-steps containing a `NonIdempotent` effect), and
//! `mandatory_periodic_10` (mandatory plus every 10th boundary).
//!
//! Per-run checkpoint counts and bytes written are deterministic, so they
//! come from an accounting pass (one untimed run per configuration, printed
//! with a `PLACEMENT-ACCOUNT` prefix and asserted against the analytic
//! schedule), not from timing inference.

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use common::{state_sized, temp_checkpoint_root, tokio_runtime};
use criterion::measurement::WallTime;
use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkGroup, Criterion};
use rusty_agent_runtime::checkpoint::DeltaPolicy;
use rusty_agent_runtime::prelude::*;
use serde_json::{json, Value};

/// Chain lengths in super-steps (one node per step).
const CHAINS: [usize; 3] = [50, 200, 1000];
/// NonIdempotent densities. The realistic band for agent workloads: a run
/// whose every step touches the world non-idempotently is a script, not an
/// agent.
const DENSITIES: [f64; 2] = [0.02, 0.10];
/// Checkpointed state sizes: small working set and payload-heavy.
const STATE_SIZES: [usize; 2] = [10 * 1024, 1024 * 1024];
/// State size for the end-to-end layer (see module docs).
const E2E_STATE_BYTES: usize = STATE_SIZES[0];
/// ReadOnly share of the effect mix: every fifth step, offset so it never
/// coincides with a NonIdempotent step.
const READ_ONLY_STRIDE: usize = 5;
/// `k` of the mandatory+periodic-k policy.
const PERIODIC_K: usize = 10;

fn non_idempotent_stride(density: f64) -> usize {
    density.recip().round() as usize
}

/// The declared effect of the node running at super-step `step`: NonIdempotent
/// on every `1/density`-th step (evenly spread, deterministic), ReadOnly on
/// every fifth remaining step, Pure otherwise.
fn super_step_effect(density: f64, step: usize) -> Effect {
    if step % non_idempotent_stride(density) == 0 {
        Effect::NonIdempotent
    } else if step % READ_ONLY_STRIDE == READ_ONLY_STRIDE - 1 {
        Effect::ReadOnly
    } else {
        Effect::Pure
    }
}

/// A checkpoint-placement policy: given the boundary after super-step `step`,
/// whether a checkpoint is written there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Placement {
    /// Every super-step boundary — the executor's current behavior.
    Uniform,
    /// Only the final boundary of the run: nothing to resume from until the
    /// run has finished. The durability floor.
    TerminalOnly,
    /// Only boundaries after super-steps containing a NonIdempotent effect —
    /// the floor exact replay imposes.
    MandatoryOnly,
    /// Mandatory boundaries plus every `PERIODIC_K`-th boundary.
    MandatoryPeriodic,
}

impl Placement {
    const ALL: [Placement; 4] = [
        Placement::Uniform,
        Placement::TerminalOnly,
        Placement::MandatoryOnly,
        Placement::MandatoryPeriodic,
    ];

    fn name(self) -> &'static str {
        match self {
            Placement::Uniform => "uniform",
            Placement::TerminalOnly => "terminal_only",
            Placement::MandatoryOnly => "mandatory_only",
            Placement::MandatoryPeriodic => "mandatory_periodic_10",
        }
    }

    fn keeps(self, step: usize, chain_len: usize, effect: Effect) -> bool {
        match self {
            Placement::Uniform => true,
            Placement::TerminalOnly => step + 1 == chain_len,
            Placement::MandatoryOnly => effect == Effect::NonIdempotent,
            Placement::MandatoryPeriodic => {
                effect == Effect::NonIdempotent || (step + 1) % PERIODIC_K == 0
            }
        }
    }
}

/// The boundary indices a policy keeps for a chain. Deterministic, so counts
/// and bytes are asserted against this schedule in the accounting pass.
fn kept_steps(chain_len: usize, density: f64, policy: Placement) -> Vec<usize> {
    (0..chain_len)
        .filter(|&step| policy.keeps(step, chain_len, super_step_effect(density, step)))
        .collect()
}

fn density_label(density: f64) -> String {
    format!("{}pct", (density * 100.0).round() as u32)
}

fn size_label(bytes: usize) -> String {
    if bytes >= 1024 * 1024 {
        format!("{}MB", bytes / (1024 * 1024))
    } else {
        format!("{}KB", bytes / 1024)
    }
}

/// A chain node with an explicitly declared [`Effect`] class (the closure
/// blanket impl always declares `Pure`).
struct DeclaredEffect<F> {
    effect: Effect,
    f: F,
}

#[async_trait]
impl<F, Fut> Node for DeclaredEffect<F>
where
    F: Fn(NodeContext) -> Fut + std::marker::Send + Sync,
    Fut: std::future::Future<Output = Result<NodeOutput>> + std::marker::Send,
{
    fn effect(&self) -> Effect {
        self.effect
    }

    async fn run(&self, ctx: NodeContext) -> Result<NodeOutput> {
        (self.f)(ctx).await
    }
}

/// A linear chain of `chain_len` nodes (one super-step each) whose node at
/// step `s` declares `super_step_effect(density, s)`. Node bodies match
/// `common::chain_graph`: read the previous channel, increment, write own
/// channel — real work through the real super-step machinery. The `blob`
/// channel carries the sized state payload through every checkpoint.
fn effect_chain(chain_len: usize, density: f64) -> (Graph, StateSpec) {
    let mut spec = StateSpec::new()
        .channel("meta", Reducer::Overwrite)
        .channel("blob", Reducer::Overwrite);
    let mut builder = GraphBuilder::new();

    for i in 0..chain_len {
        let channel = format!("c{i}");
        spec.add_channel(channel.clone(), Reducer::Overwrite);
        let prev_channel = (i > 0).then(|| format!("c{}", i - 1));
        let node = DeclaredEffect {
            effect: super_step_effect(density, i),
            f: move |ctx: NodeContext| {
                let channel = channel.clone();
                let prev_channel = prev_channel.clone();
                async move {
                    let prev = prev_channel
                        .and_then(|p| ctx.state().get(&p).and_then(Value::as_u64))
                        .unwrap_or(0);
                    Ok(NodeOutput::update(channel, json!(prev + 1)))
                }
            },
        };
        builder.add_node(format!("n{i}"), node);
        if i > 0 {
            builder.add_edge(format!("n{}", i - 1), format!("n{i}"));
        }
    }

    builder.set_entry_point("n0");
    let graph = builder.compile().expect("effect chain compiles");
    (graph, spec)
}

/// Bench-local checkpointer that persists only the boundaries its [`Placement`]
/// policy selects; kept writes delegate to a real [`JsonFileCheckpointer`].
/// Dropped boundaries still cost the executor a checkpoint mint (state clone)
/// and a journaled `CheckpointWritten` event — that is today's executor, so
/// measured savings are conservative.
struct PlacementCheckpointer {
    inner: JsonFileCheckpointer,
    policy: Placement,
    chain_len: usize,
    density: f64,
    kept: Arc<AtomicU64>,
}

impl PlacementCheckpointer {
    fn new(dir: &std::path::Path, chain_len: usize, density: f64, policy: Placement) -> Self {
        Self {
            // W4: pin the pre-wave-4 full-write path. This bench re-puts
            // identical states per step, which the default delta policy
            // would encode as empty deltas — that would measure the delta
            // fast path, not the R0.5 placement experiment this file
            // publishes numbers for.
            inner: JsonFileCheckpointer::with_delta_policy(dir, DeltaPolicy::full_only()),
            policy,
            chain_len,
            density,
            kept: Arc::new(AtomicU64::new(0)),
        }
    }

    fn kept_count(&self) -> u64 {
        self.kept.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl Checkpointer for PlacementCheckpointer {
    async fn put(&self, checkpoint: Checkpoint) -> Result<()> {
        let effect = super_step_effect(self.density, checkpoint.step);
        if self.policy.keeps(checkpoint.step, self.chain_len, effect) {
            self.kept.fetch_add(1, Ordering::Relaxed);
            self.inner.put(checkpoint).await
        } else {
            Ok(())
        }
    }

    async fn get_latest(&self, thread_id: &str) -> Result<Option<Checkpoint>> {
        self.inner.get_latest(thread_id).await
    }

    async fn list(&self, thread_id: &str) -> Result<Vec<Checkpoint>> {
        self.inner.list(thread_id).await
    }
}

/// Criterion budget per group, scaled to iteration cost so the whole bench
/// completes in minutes rather than hours.
fn tune(group: &mut BenchmarkGroup<'_, WallTime>, chain_len: usize, state_bytes: usize) {
    let samples = match (chain_len, state_bytes) {
        (50, s) if s == E2E_STATE_BYTES => 30,
        (200, s) if s == E2E_STATE_BYTES => 20,
        (_, s) if s == E2E_STATE_BYTES => 10,
        (50, _) => 20,
        _ => 10,
    };
    group
        .sample_size(samples)
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(2));
}

/// End-to-end layer: full `Executor::run` wall time per policy, 10 KB state.
fn bench_end_to_end(c: &mut Criterion) {
    let rt = tokio_runtime();
    let root = temp_checkpoint_root("placement-e2e");
    let _ = std::fs::remove_dir_all(&root);

    for &chain_len in &CHAINS {
        let mut group = c.benchmark_group(format!("placement_e2e_chain{chain_len}"));
        tune(&mut group, chain_len, E2E_STATE_BYTES);
        for &density in &DENSITIES {
            let (graph, spec) = effect_chain(chain_len, density);
            for &policy in &Placement::ALL {
                let id = format!("{}@{}", policy.name(), density_label(density));
                group.bench_function(id, |b| {
                    let store = Arc::new(PlacementCheckpointer::new(
                        &root, chain_len, density, policy,
                    ));
                    let executor = Executor::with_checkpointer(store);
                    let thread = format!(
                        "e2e-{chain_len}-{}-{}",
                        density_label(density),
                        policy.name()
                    );
                    b.iter(|| {
                        let outcome = rt
                            .block_on(executor.run(
                                &graph,
                                &spec,
                                state_sized(E2E_STATE_BYTES),
                                RunConfig::new(thread.clone()).with_max_steps(chain_len + 16),
                            ))
                            .expect("run completes");
                        criterion::black_box(outcome);
                    });
                });
            }
        }
        group.finish();
    }

    let _ = std::fs::remove_dir_all(&root);
}

/// Checkpoint-stream layer: mint + durable `put` for exactly the boundaries
/// each policy keeps, no executor. 10 KB and 1 MB states.
fn bench_checkpoint_stream(c: &mut Criterion) {
    let rt = tokio_runtime();
    let root = temp_checkpoint_root("placement-stream");
    let _ = std::fs::remove_dir_all(&root);

    for &state_bytes in &STATE_SIZES {
        for &chain_len in &CHAINS {
            let mut group = c.benchmark_group(format!(
                "placement_stream_{}_chain{chain_len}",
                size_label(state_bytes)
            ));
            tune(&mut group, chain_len, state_bytes);
            for &density in &DENSITIES {
                for &policy in &Placement::ALL {
                    let id = format!("{}@{}", policy.name(), density_label(density));
                    group.bench_function(id, |b| {
                        let thread = format!(
                            "stream-{state_bytes}-{chain_len}-{}-{}",
                            density_label(density),
                            policy.name()
                        );
                        let dir = root.join(&thread);
                        b.iter_batched(
                            // Fresh directory per iteration so timed passes
                            // never rewrite existing files; cleanup sits
                            // outside the measured region.
                            || {
                                let _ = std::fs::remove_dir_all(&dir);
                            },
                            |_| {
                                rt.block_on(async {
                                    // W4: full-only policy — see
                                    // `PlacementCheckpointer::new` above.
                                    let store = JsonFileCheckpointer::with_delta_policy(
                                        root.clone(),
                                        DeltaPolicy::full_only(),
                                    );
                                    for step in kept_steps(chain_len, density, policy) {
                                        store
                                            .put(Checkpoint::new(
                                                thread.clone(),
                                                step,
                                                state_sized(state_bytes),
                                                vec!["next".to_owned()],
                                            ))
                                            .await
                                            .expect("put succeeds");
                                    }
                                });
                            },
                            BatchSize::PerIteration,
                        );
                    });
                }
            }
            group.finish();
        }
    }

    let _ = std::fs::remove_dir_all(&root);
}

/// One untimed pass per configuration for the metrics timing cannot deliver:
/// per-run checkpoint count and bytes written, plus (end-to-end) proof that
/// the declared effect classes reach the journal. Counts are asserted against
/// the analytic placement schedule.
fn accounting(_c: &mut Criterion) {
    let rt = tokio_runtime();

    let stream_root = temp_checkpoint_root("placement-account-stream");
    let _ = std::fs::remove_dir_all(&stream_root);
    println!("# placement accounting — one untimed run per configuration");
    for &state_bytes in &STATE_SIZES {
        for &chain_len in &CHAINS {
            for &density in &DENSITIES {
                for &policy in &Placement::ALL {
                    let thread = format!(
                        "account-{state_bytes}-{chain_len}-{}-{}",
                        density_label(density),
                        policy.name()
                    );
                    let dir = stream_root.join(&thread);
                    let _ = std::fs::remove_dir_all(&dir);
                    let expected = kept_steps(chain_len, density, policy);
                    rt.block_on(async {
                        // W4: full-only policy — see
                        // `PlacementCheckpointer::new` above.
                        let store = JsonFileCheckpointer::with_delta_policy(
                            stream_root.clone(),
                            DeltaPolicy::full_only(),
                        );
                        for &step in &expected {
                            store
                                .put(Checkpoint::new(
                                    thread.clone(),
                                    step,
                                    state_sized(state_bytes),
                                    vec!["next".to_owned()],
                                ))
                                .await
                                .expect("put succeeds");
                        }
                    });
                    let mut files = 0usize;
                    let mut bytes = 0u64;
                    for entry in std::fs::read_dir(&dir).expect("thread dir exists") {
                        let entry = entry.expect("dir entry readable");
                        if entry.path().extension().is_some_and(|ext| ext == "json") {
                            files += 1;
                            bytes += entry.metadata().expect("file metadata").len();
                        }
                    }
                    assert_eq!(
                        files,
                        expected.len(),
                        "persisted checkpoints match the placement schedule"
                    );
                    println!(
                        "PLACEMENT-ACCOUNT layer=stream state={} chain={} density={} policy={} checkpoints={} bytes={}",
                        size_label(state_bytes),
                        chain_len,
                        density_label(density),
                        policy.name(),
                        files,
                        bytes
                    );
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(&stream_root);

    // End-to-end: one real run per configuration. The policy filter must keep
    // exactly the scheduled boundaries, and the journal must show one
    // NonIdempotent node output per NonIdempotent step — the mandatory set the
    // policies are defined over actually exists in the evidence.
    let e2e_root = temp_checkpoint_root("placement-account-e2e");
    let _ = std::fs::remove_dir_all(&e2e_root);
    for &chain_len in &CHAINS {
        for &density in &DENSITIES {
            let (graph, spec) = effect_chain(chain_len, density);
            let expected_ni = (0..chain_len)
                .filter(|&s| super_step_effect(density, s) == Effect::NonIdempotent)
                .count();
            for &policy in &Placement::ALL {
                let store = Arc::new(PlacementCheckpointer::new(
                    &e2e_root, chain_len, density, policy,
                ));
                let executor = Executor::with_checkpointer(store.clone());
                let thread = format!(
                    "account-e2e-{chain_len}-{}-{}",
                    density_label(density),
                    policy.name()
                );
                let outcome = rt
                    .block_on(executor.run(
                        &graph,
                        &spec,
                        state_sized(E2E_STATE_BYTES),
                        RunConfig::new(thread.clone()).with_max_steps(chain_len + 16),
                    ))
                    .expect("run completes");
                assert!(
                    matches!(outcome, ExecutionOutcome::Done(_)),
                    "chain runs to completion"
                );
                let expected = kept_steps(chain_len, density, policy);
                assert_eq!(
                    store.kept_count() as usize,
                    expected.len(),
                    "policy filter kept exactly the scheduled boundaries"
                );
                let journal = executor.journal().expect("journal published");
                let ni_outputs = journal
                    .events()
                    .iter()
                    .filter(|e| {
                        e.kind == RunEventKind::NodeOutput && e.effect == Effect::NonIdempotent
                    })
                    .count();
                assert_eq!(
                    ni_outputs, expected_ni,
                    "journal carries the declared NonIdempotent effects"
                );
                println!(
                    "PLACEMENT-ACCOUNT layer=e2e state={} chain={} density={} policy={} checkpoints={} non_idempotent_node_outputs={}",
                    size_label(E2E_STATE_BYTES),
                    chain_len,
                    density_label(density),
                    policy.name(),
                    store.kept_count(),
                    ni_outputs
                );
            }
        }
    }
    let _ = std::fs::remove_dir_all(&e2e_root);
}

criterion_group!(
    benches,
    accounting,
    bench_end_to_end,
    bench_checkpoint_stream
);
criterion_main!(benches);
