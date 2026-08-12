//! Load harness for the R1.0 "capacity envelope" gate.
//!
//! Boots a real rusty-agent-server in-process on an ephemeral loopback port
//! and drives it over HTTP exactly as a client would, measuring the four
//! surfaces `docs/benchmarks.md` publishes: concurrent blocking runs, SSE
//! fanout, durable-task throughput, and checkpoint writes. The probe graph
//! is four no-op nodes — deliberately without even a scripted `ChatModel`:
//! the envelope describes the server and the executor, and a canned model
//! would add nothing to that picture but a mutex pop. Nothing here ever
//! reaches the network beyond loopback.
//!
//! Deliberately NOT measured:
//!
//! - more than one process (no distribution story, no network variance);
//! - auth/tenancy or TLS overhead (the server runs in open dev mode);
//! - soak behavior — the default sizes finish in about a minute, so tails
//!   past a few thousand samples are out of scope by design.
//!
//! Sizes are env-configurable; defaults match the R1.0 gate:
//!
//! - `LOAD_ENVELOPE_RUNS` (default 1000) — scenario 1's total runs;
//! - `LOAD_ENVELOPE_CONCURRENCY` (default 32) — in-flight runs / workers;
//! - `LOAD_ENVELOPE_SSE_STREAMS` (default 32) — scenario 2's fanout;
//! - `LOAD_ENVELOPE_TASK_OPS` (default 2000) — scenario 3's enqueues, and
//!   the claim+complete cycles that drain them.
//!
//! The default backend is the JSON-file store in a fresh temp dir. With the
//! `postgres` feature compiled in and `DATABASE_URL` set, scenarios 1 and 3
//! repeat against Postgres; without the feature a set `DATABASE_URL` is
//! reported and skipped. `--json <path>` writes the machine-readable report
//! (durations in ms, rates in ops/sec); stdout gets the human-readable one.
//!
//! Run with: `cargo run -p rusty-agent-server --example load_envelope --release`
//! (or `scripts/load-envelope.sh`, which builds and optionally wires a
//! throwaway Postgres container). This is a dev tool: nothing references it
//! from CI test paths, and it cleans up its server and store dirs on exit.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use rusty_agent_runtime::prelude::*;
use rusty_agent_server::{serve_with_shutdown, GraphRegistry, ServerConfig};
use serde_json::{json, Value};

/// The registered name of the probe graph every scenario runs.
const PROBE_GRAPH: &str = "probe";

/// A stream or drain that outlives its bound is counted as an error, never
/// waited on forever — the harness reports and continues.
const SSE_STREAM_TIMEOUT: Duration = Duration::from_secs(30);
/// See [`SSE_STREAM_TIMEOUT`].
const TASK_DRAIN_TIMEOUT: Duration = Duration::from_secs(120);

struct HarnessConfig {
    runs: usize,
    concurrency: usize,
    sse_streams: usize,
    task_ops: usize,
    json_out: Option<PathBuf>,
}

fn env_usize(name: &str, default: usize) -> usize {
    match std::env::var(name) {
        Ok(raw) => match raw.parse::<usize>() {
            Ok(value) if value > 0 => value,
            _ => {
                eprintln!(
                    "warning: ignoring {name}={raw:?} (want a positive integer); using {default}"
                );
                default
            }
        },
        Err(_) => default,
    }
}

fn parse_json_arg() -> Option<PathBuf> {
    let mut out = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--json" => match args.next() {
                Some(path) => out = Some(PathBuf::from(path)),
                None => {
                    eprintln!("error: --json requires a path");
                    std::process::exit(2);
                }
            },
            "-h" | "--help" => {
                println!(
                    "usage: load_envelope [--json <path>]\n\
                     \n\
                     sizes via env: LOAD_ENVELOPE_RUNS, LOAD_ENVELOPE_CONCURRENCY,\n\
                     LOAD_ENVELOPE_SSE_STREAMS, LOAD_ENVELOPE_TASK_OPS\n\
                     postgres pass: build with --features postgres and set DATABASE_URL"
                );
                std::process::exit(0);
            }
            other => {
                eprintln!("error: unknown argument `{other}` (try --help)");
                std::process::exit(2);
            }
        }
    }
    out
}

/// `step_1 -> step_2 -> step_3 -> step_4`, appending to a `log` channel.
/// Four nodes so every run crosses four checkpoint boundaries — the
/// checkpoint scenario counts what those writes cost in aggregate.
fn probe_graph() -> (Graph, StateSpec) {
    let spec = StateSpec::new().channel("log", Reducer::Append);
    let mut builder = GraphBuilder::new();
    for (name, step) in [("step_1", 1), ("step_2", 2), ("step_3", 3), ("step_4", 4)] {
        builder.add_node(name, move |_ctx: NodeContext| async move {
            Ok(NodeOutput::update("log", json!(step)))
        });
    }
    builder.set_entry_point("step_1");
    builder.add_edge("step_1", "step_2");
    builder.add_edge("step_2", "step_3");
    builder.add_edge("step_3", "step_4");
    (builder.compile().expect("probe graph compiles"), spec)
}

/// A running server under test: its base URL, the store dir to clean up,
/// and the shutdown handle.
struct TestServer {
    base: String,
    store_dir: PathBuf,
    shutdown: Arc<tokio::sync::Notify>,
    join: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl TestServer {
    /// Bind an ephemeral port the way the server's own tests do (probe,
    /// release, rebind), boot, and wait for `/ok`.
    async fn spawn(
        store_dir: PathBuf,
        database_url: Option<String>,
    ) -> std::result::Result<Self, Box<dyn std::error::Error>> {
        let probe = std::net::TcpListener::bind("127.0.0.1:0")?;
        let addr = probe.local_addr()?;
        drop(probe);

        let (graph, spec) = probe_graph();
        let mut registry = GraphRegistry::new();
        registry.register(PROBE_GRAPH, graph, spec);

        let config = ServerConfig::new(addr, store_dir.clone());
        #[cfg(feature = "postgres")]
        let config = match database_url {
            Some(url) => config.with_postgres(url),
            None => config,
        };
        #[cfg(not(feature = "postgres"))]
        let _ = database_url;

        let shutdown = Arc::new(tokio::sync::Notify::new());
        let signal = {
            let shutdown = Arc::clone(&shutdown);
            async move { shutdown.notified().await }
        };
        let join = tokio::spawn(serve_with_shutdown(registry, config, signal));

        let base = format!("http://{addr}");
        let probe_client = reqwest::Client::new();
        for _ in 0..200 {
            if probe_client
                .get(format!("{base}/ok"))
                .send()
                .await
                .is_ok_and(|resp| resp.status().is_success())
            {
                return Ok(Self {
                    base,
                    store_dir,
                    shutdown,
                    join,
                });
            }
            if join.is_finished() {
                return Err("server exited before answering /ok".into());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        Err(format!("server at {base} did not answer /ok within 5s").into())
    }

    /// Drain and remove the store dir. A stop problem is reported, never
    /// hidden — but it must not fail a report that already measured.
    async fn stop(self) {
        self.shutdown.notify_one();
        match tokio::time::timeout(Duration::from_secs(10), self.join).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(error))) => eprintln!("warning: server exited with error: {error}"),
            Ok(Err(error)) => eprintln!("warning: server task failed to join: {error}"),
            Err(_) => eprintln!("warning: server did not stop within 10s; abandoning it"),
        }
        let _ = std::fs::remove_dir_all(&self.store_dir);
    }
}

/// Nearest-rank percentile summary over recorded samples (ms). `None` when
/// nothing succeeded — the report prints `n/a` rather than invent a number.
struct Percentiles {
    count: usize,
    mean: f64,
    p50: f64,
    p95: f64,
    p99: f64,
    max: f64,
}

fn percentiles(samples: &[f64]) -> Option<Percentiles> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let n = sorted.len();
    let at = |p: f64| {
        let rank = ((p / 100.0) * n as f64).ceil() as usize;
        sorted[rank.clamp(1, n) - 1]
    };
    Some(Percentiles {
        count: n,
        mean: sorted.iter().sum::<f64>() / n as f64,
        p50: at(50.0),
        p95: at(95.0),
        p99: at(99.0),
        max: sorted[n - 1],
    })
}

fn percentiles_json(samples: &[f64]) -> Value {
    match percentiles(samples) {
        Some(p) => json!({
            "count": p.count,
            "mean": p.mean,
            "p50": p.p50,
            "p95": p.p95,
            "p99": p.p99,
            "max": p.max,
        }),
        None => Value::Null,
    }
}

fn take(counter: &Arc<AtomicUsize>) -> usize {
    counter.load(Ordering::Relaxed)
}

/// Scenario 1's outcome: every completed run's `runs/wait` latency plus the
/// thread ids (scenario 4 counts their checkpoints).
struct ConcurrentRuns {
    backend: &'static str,
    requested: usize,
    concurrency: usize,
    wall_ms: f64,
    latencies_ms: Vec<f64>,
    errors: usize,
    thread_ids: Vec<String>,
}

/// Scenario 1: `requested` blocking runs, `concurrency` in flight, each on a
/// fresh thread (the per-thread run cap would otherwise serialize them).
/// The latency sample is the `runs/wait` call itself; thread creation is
/// counted in wall time (hence in runs/sec) but not in per-run latency.
async fn scenario_concurrent_runs(
    client: &reqwest::Client,
    base: &str,
    backend: &'static str,
    requested: usize,
    concurrency: usize,
) -> ConcurrentRuns {
    let latencies = Arc::new(Mutex::new(Vec::with_capacity(requested)));
    let thread_ids = Arc::new(Mutex::new(Vec::with_capacity(requested)));
    let errors = Arc::new(AtomicUsize::new(0));

    let wall = Instant::now();
    futures::stream::iter(0..requested)
        .map(|_| {
            let latencies = Arc::clone(&latencies);
            let thread_ids = Arc::clone(&thread_ids);
            let errors = Arc::clone(&errors);
            async move {
                let thread = client
                    .post(format!("{base}/threads"))
                    .json(&json!({"graph": PROBE_GRAPH}))
                    .send()
                    .await;
                let thread_id = match thread {
                    Ok(resp) if resp.status().is_success() => match resp.json::<Value>().await {
                        Ok(body) => body["thread_id"].as_str().map(str::to_owned),
                        Err(_) => None,
                    },
                    _ => None,
                };
                let Some(thread_id) = thread_id else {
                    errors.fetch_add(1, Ordering::Relaxed);
                    return;
                };
                let started = Instant::now();
                let run = client
                    .post(format!("{base}/threads/{thread_id}/runs/wait"))
                    .json(&json!({}))
                    .send()
                    .await;
                let elapsed = started.elapsed();
                let ok = match run {
                    Ok(resp) if resp.status().is_success() => match resp.json::<Value>().await {
                        Ok(body) => body["status"] == json!("success"),
                        Err(_) => false,
                    },
                    _ => false,
                };
                if ok {
                    latencies
                        .lock()
                        .unwrap()
                        .push(elapsed.as_secs_f64() * 1000.0);
                    thread_ids.lock().unwrap().push(thread_id);
                } else {
                    errors.fetch_add(1, Ordering::Relaxed);
                }
            }
        })
        .buffer_unordered(concurrency)
        .collect::<Vec<()>>()
        .await;
    let wall_ms = wall.elapsed().as_secs_f64() * 1000.0;

    let latencies_ms = std::mem::take(&mut *latencies.lock().unwrap());
    let thread_ids = std::mem::take(&mut *thread_ids.lock().unwrap());
    ConcurrentRuns {
        backend,
        requested,
        concurrency,
        wall_ms,
        latencies_ms,
        errors: take(&errors),
        thread_ids,
    }
}

impl ConcurrentRuns {
    fn runs_per_sec(&self) -> f64 {
        self.latencies_ms.len() as f64 / (self.wall_ms / 1000.0)
    }

    fn print(&self) {
        println!(
            "[{}] concurrent runs — {} requested, {} in flight",
            self.backend, self.requested, self.concurrency
        );
        println!(
            "  {} runs in {:.2}s ({:.1} runs/sec), errors: {}",
            self.latencies_ms.len(),
            self.wall_ms / 1000.0,
            self.runs_per_sec(),
            self.errors
        );
        match percentiles(&self.latencies_ms) {
            Some(p) => println!(
                "  run latency ms: p50 {:.1}  p95 {:.1}  p99 {:.1}  (mean {:.1}, max {:.1})",
                p.p50, p.p95, p.p99, p.mean, p.max
            ),
            None => println!("  run latency ms: n/a (no successful run)"),
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "scenario": "concurrent_runs",
            "backend": self.backend,
            "requested": self.requested,
            "concurrency": self.concurrency,
            "completed": self.latencies_ms.len(),
            "errors": self.errors,
            "wall_ms": self.wall_ms,
            "runs_per_sec": self.runs_per_sec(),
            "latency_ms": percentiles_json(&self.latencies_ms),
        })
    }
}

struct SseFanout {
    backend: &'static str,
    requested: usize,
    completed: usize,
    ttfes_ms: Vec<f64>,
    errors: usize,
}

enum StreamOutcome {
    Completed { ttfe_ms: f64 },
    Failed,
}

/// One `runs/stream` attachment on a fresh thread, read to its `end` frame.
/// Time-to-first-event is measured at the first body chunk — an exact read
/// for this graph (its first frame lands immediately), one a keep-alive
/// comment (15s cadence) would only inflate on a stalled run.
async fn one_stream(client: &reqwest::Client, base: &str) -> StreamOutcome {
    let thread = client
        .post(format!("{base}/threads"))
        .json(&json!({"graph": PROBE_GRAPH}))
        .send()
        .await;
    let thread_id = match thread {
        Ok(resp) if resp.status().is_success() => match resp.json::<Value>().await {
            Ok(body) => body["thread_id"].as_str().map(str::to_owned),
            Err(_) => None,
        },
        _ => None,
    };
    let Some(thread_id) = thread_id else {
        return StreamOutcome::Failed;
    };

    let started = Instant::now();
    let resp = client
        .post(format!("{base}/threads/{thread_id}/runs/stream"))
        .json(&json!({}))
        .send()
        .await;
    let Ok(resp) = resp else {
        return StreamOutcome::Failed;
    };
    if !resp.status().is_success() {
        return StreamOutcome::Failed;
    }

    let mut ttfe = None;
    let mut body = String::new();
    let mut chunks = resp.bytes_stream();
    while let Some(chunk) = chunks.next().await {
        let Ok(bytes) = chunk else {
            return StreamOutcome::Failed;
        };
        if ttfe.is_none() {
            ttfe = Some(started.elapsed());
        }
        body.push_str(&String::from_utf8_lossy(&bytes));
        if body.contains("event: end") {
            return match ttfe {
                Some(ttfe) => StreamOutcome::Completed {
                    ttfe_ms: ttfe.as_secs_f64() * 1000.0,
                },
                None => StreamOutcome::Failed,
            };
        }
    }
    // The body closed without an `end` frame — not a completed stream.
    StreamOutcome::Failed
}

/// Scenario 2: `streams` concurrent SSE attachments, each verified to
/// terminate with an `end` frame.
async fn scenario_sse_fanout(
    client: &reqwest::Client,
    base: &str,
    backend: &'static str,
    streams: usize,
) -> SseFanout {
    let ttfes = Arc::new(Mutex::new(Vec::with_capacity(streams)));
    let completed = Arc::new(AtomicUsize::new(0));
    let errors = Arc::new(AtomicUsize::new(0));

    futures::stream::iter(0..streams)
        .map(|_| {
            let ttfes = Arc::clone(&ttfes);
            let completed = Arc::clone(&completed);
            let errors = Arc::clone(&errors);
            async move {
                match tokio::time::timeout(SSE_STREAM_TIMEOUT, one_stream(client, base)).await {
                    Ok(StreamOutcome::Completed { ttfe_ms }) => {
                        completed.fetch_add(1, Ordering::Relaxed);
                        ttfes.lock().unwrap().push(ttfe_ms);
                    }
                    // `Err(_)` is the stream timeout; `Ok(Failed)` a stream
                    // that errored or closed short — both land here.
                    _ => {
                        errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        })
        .buffer_unordered(streams)
        .collect::<Vec<()>>()
        .await;

    let ttfes_ms = std::mem::take(&mut *ttfes.lock().unwrap());
    SseFanout {
        backend,
        requested: streams,
        completed: take(&completed),
        ttfes_ms,
        errors: take(&errors),
    }
}

impl SseFanout {
    fn print(&self) {
        println!(
            "[{}] SSE fanout — {} concurrent streams on runs/stream",
            self.backend, self.requested
        );
        println!(
            "  {}/{} streams ended with an `end` frame ({:.1}%), errors: {}",
            self.completed,
            self.requested,
            100.0 * self.completed as f64 / self.requested as f64,
            self.errors
        );
        match percentiles(&self.ttfes_ms) {
            Some(p) => println!(
                "  time-to-first-event ms: p50 {:.1}  p95 {:.1}  (mean {:.1}, max {:.1})",
                p.p50, p.p95, p.mean, p.max
            ),
            None => println!("  time-to-first-event ms: n/a (no completed stream)"),
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "scenario": "sse_fanout",
            "backend": self.backend,
            "requested": self.requested,
            "completed": self.completed,
            "completed_fraction": self.completed as f64 / self.requested as f64,
            "errors": self.errors,
            "ttfe_ms": percentiles_json(&self.ttfes_ms),
        })
    }
}

struct TaskQueue {
    backend: &'static str,
    requested: usize,
    enqueued: usize,
    enqueue_wall_ms: f64,
    cycles: usize,
    drain_wall_ms: f64,
    errors: usize,
}

/// Scenario 3: `ops` enqueues followed by a claim+complete drain of the
/// same count, each phase timed on its own wall clock. A cycle is one claim
/// plus one complete — two HTTP calls, reported as cycles/sec.
async fn scenario_task_queue(
    client: &reqwest::Client,
    base: &str,
    backend: &'static str,
    ops: usize,
    concurrency: usize,
) -> TaskQueue {
    let errors = Arc::new(AtomicUsize::new(0));

    let enqueued = Arc::new(AtomicUsize::new(0));
    let enqueue_wall = Instant::now();
    futures::stream::iter(0..ops)
        .map(|seq| {
            let enqueued = Arc::clone(&enqueued);
            let errors = Arc::clone(&errors);
            async move {
                let res = client
                    .post(format!("{base}/tasks"))
                    .json(&json!({"kind": "load_probe", "payload": {"seq": seq}}))
                    .send()
                    .await;
                match res {
                    Ok(resp) if resp.status() == reqwest::StatusCode::CREATED => {
                        enqueued.fetch_add(1, Ordering::Relaxed);
                    }
                    _ => {
                        errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        })
        .buffer_unordered(concurrency)
        .collect::<Vec<()>>()
        .await;
    let enqueue_wall_ms = enqueue_wall.elapsed().as_secs_f64() * 1000.0;

    let target = take(&enqueued);
    let cycles = Arc::new(AtomicUsize::new(0));
    let drain_wall = Instant::now();
    let drain = futures::stream::iter(0..concurrency)
        .map(|worker| {
            let cycles = Arc::clone(&cycles);
            let errors = Arc::clone(&errors);
            async move {
                let worker_id = format!("load-worker-{worker}");
                loop {
                    if take(&cycles) >= target {
                        break;
                    }
                    // Persistent failure (server gone) must not spin workers
                    // until the drain timeout; a healthy run never gets near
                    // this ceiling.
                    if take(&errors) > 100 {
                        break;
                    }
                    let claim = client
                        .post(format!("{base}/tasks/claim"))
                        .json(&json!({"worker_id": worker_id, "lease_ms": 30_000}))
                        .send()
                        .await;
                    let resp = match claim {
                        Ok(resp) => resp,
                        Err(_) => {
                            errors.fetch_add(1, Ordering::Relaxed);
                            break;
                        }
                    };
                    if resp.status() == reqwest::StatusCode::NO_CONTENT {
                        break; // queue drained
                    }
                    if !resp.status().is_success() {
                        errors.fetch_add(1, Ordering::Relaxed);
                        break;
                    }
                    let task_id = match resp.json::<Value>().await {
                        Ok(body) => body["task"]["task_id"].as_str().map(str::to_owned),
                        Err(_) => None,
                    };
                    let Some(task_id) = task_id else {
                        errors.fetch_add(1, Ordering::Relaxed);
                        continue;
                    };
                    let settle = client
                        .post(format!("{base}/tasks/{task_id}/complete"))
                        .json(&json!({"worker_id": worker_id, "result": Value::Null}))
                        .send()
                        .await;
                    match settle {
                        Ok(resp) if resp.status().is_success() => {
                            cycles.fetch_add(1, Ordering::Relaxed);
                        }
                        // A lost lease leaves the task leased; its 30s
                        // visibility timeout reclaims it, and the drain
                        // target accounting below stays honest about the
                        // shortfall.
                        _ => {
                            errors.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }
        })
        .buffer_unordered(concurrency)
        .collect::<Vec<()>>();
    let drain_timed_out = tokio::time::timeout(TASK_DRAIN_TIMEOUT, drain)
        .await
        .is_err();
    let drain_wall_ms = drain_wall.elapsed().as_secs_f64() * 1000.0;
    if drain_timed_out {
        let shortfall = target.saturating_sub(take(&cycles));
        eprintln!(
            "warning: task drain hit its {TASK_DRAIN_TIMEOUT:?} bound; {shortfall} tasks unsettled"
        );
        errors.fetch_add(shortfall, Ordering::Relaxed);
    }

    TaskQueue {
        backend,
        requested: ops,
        enqueued: target,
        enqueue_wall_ms,
        cycles: take(&cycles),
        drain_wall_ms,
        errors: take(&errors),
    }
}

impl TaskQueue {
    fn enqueue_per_sec(&self) -> f64 {
        self.enqueued as f64 / (self.enqueue_wall_ms / 1000.0)
    }

    fn cycles_per_sec(&self) -> f64 {
        self.cycles as f64 / (self.drain_wall_ms / 1000.0)
    }

    fn print(&self) {
        println!(
            "[{}] task queue — enqueue + claim/complete over {} tasks",
            self.backend, self.requested
        );
        println!(
            "  enqueue: {} ops in {:.2}s ({:.1} ops/sec)",
            self.enqueued,
            self.enqueue_wall_ms / 1000.0,
            self.enqueue_per_sec()
        );
        println!(
            "  claim+complete: {} cycles in {:.2}s ({:.1} cycles/sec), errors: {}",
            self.cycles,
            self.drain_wall_ms / 1000.0,
            self.cycles_per_sec(),
            self.errors
        );
        if self.cycles < self.enqueued {
            println!(
                "  drain incomplete: {} of {} tasks settled (see errors above)",
                self.cycles, self.enqueued
            );
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "scenario": "task_queue",
            "backend": self.backend,
            "requested": self.requested,
            "enqueued": self.enqueued,
            "enqueue_wall_ms": self.enqueue_wall_ms,
            "enqueue_ops_per_sec": self.enqueue_per_sec(),
            "cycles": self.cycles,
            "drain_wall_ms": self.drain_wall_ms,
            "cycles_per_sec": self.cycles_per_sec(),
            "drain_incomplete": self.cycles < self.enqueued,
            "errors": self.errors,
        })
    }
}

struct CheckpointWrites {
    threads: usize,
    checkpoints: usize,
    window_ms: f64,
    errors: usize,
}

/// Scenario 4: checkpoint writes, derived from the file-backed concurrent
/// runs. Each completed run's history names the checkpoints the server
/// wrote for it; the rate divides that total by scenario 1's wall window.
/// Per-write latency is deliberately not reported — a checkpoint write is
/// not separately timeable over the wire, so only the aggregate is honest.
async fn scenario_checkpoint_writes(
    client: &reqwest::Client,
    base: &str,
    runs: &ConcurrentRuns,
) -> CheckpointWrites {
    let checkpoints = Arc::new(AtomicUsize::new(0));
    let errors = Arc::new(AtomicUsize::new(0));

    futures::stream::iter(&runs.thread_ids)
        .map(|thread_id| {
            let checkpoints = Arc::clone(&checkpoints);
            let errors = Arc::clone(&errors);
            async move {
                let res = client
                    .post(format!("{base}/threads/{thread_id}/history"))
                    .json(&json!({}))
                    .send()
                    .await;
                match res {
                    Ok(resp) if resp.status().is_success() => match resp.json::<Value>().await {
                        Ok(body) => {
                            let count = body.as_array().map_or(0, Vec::len);
                            checkpoints.fetch_add(count, Ordering::Relaxed);
                        }
                        Err(_) => {
                            errors.fetch_add(1, Ordering::Relaxed);
                        }
                    },
                    _ => {
                        errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        })
        .buffer_unordered(32)
        .collect::<Vec<()>>()
        .await;

    CheckpointWrites {
        threads: runs.thread_ids.len(),
        checkpoints: take(&checkpoints),
        window_ms: runs.wall_ms,
        errors: take(&errors),
    }
}

impl CheckpointWrites {
    fn per_sec(&self) -> f64 {
        self.checkpoints as f64 / (self.window_ms / 1000.0)
    }

    fn print(&self) {
        println!("[file] checkpoint writes — derived from the concurrent-runs window");
        println!(
            "  {} checkpoints across {} threads in a {:.2}s window ({:.1} checkpoints/sec), errors: {}",
            self.checkpoints,
            self.threads,
            self.window_ms / 1000.0,
            self.per_sec(),
            self.errors
        );
        println!(
            "  per-write latency not reported: a checkpoint write is not separately\n  timeable over the wire; the rate above is the honest aggregate."
        );
    }

    fn to_json(&self) -> Value {
        json!({
            "scenario": "checkpoint_writes",
            "backend": "file",
            "threads": self.threads,
            "checkpoints": self.checkpoints,
            "window_ms": self.window_ms,
            "checkpoints_per_sec": self.per_sec(),
            "errors": self.errors,
            "note": "aggregate rate over the concurrent-runs window; per-write latency is not separately measurable over the wire",
        })
    }
}

#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let config = HarnessConfig {
        runs: env_usize("LOAD_ENVELOPE_RUNS", 1000),
        concurrency: env_usize("LOAD_ENVELOPE_CONCURRENCY", 32),
        sse_streams: env_usize("LOAD_ENVELOPE_SSE_STREAMS", 32),
        task_ops: env_usize("LOAD_ENVELOPE_TASK_OPS", 2000),
        json_out: parse_json_arg(),
    };

    println!("load_envelope — rusty-agent-server capacity harness");
    println!(
        "  sizes: runs={} concurrency={} sse_streams={} task_ops={}",
        config.runs, config.concurrency, config.sse_streams, config.task_ops
    );

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?;
    let started_at = chrono::Utc::now().to_rfc3339();
    let mut scenarios: Vec<Value> = Vec::new();

    // File backend (always).
    let store = std::env::temp_dir().join(format!("rusty-load-envelope-{}", uuid::Uuid::new_v4()));
    let server = TestServer::spawn(store, None).await?;
    let runs = scenario_concurrent_runs(
        &client,
        &server.base,
        "file",
        config.runs,
        config.concurrency,
    )
    .await;
    runs.print();
    scenarios.push(runs.to_json());
    let sse = scenario_sse_fanout(&client, &server.base, "file", config.sse_streams).await;
    sse.print();
    scenarios.push(sse.to_json());
    let tasks = scenario_task_queue(
        &client,
        &server.base,
        "file",
        config.task_ops,
        config.concurrency,
    )
    .await;
    tasks.print();
    scenarios.push(tasks.to_json());
    let checkpoints = scenario_checkpoint_writes(&client, &server.base, &runs).await;
    checkpoints.print();
    scenarios.push(checkpoints.to_json());
    server.stop().await;

    // Postgres pass (feature-gated, opt-in via DATABASE_URL): scenarios 1
    // and 3 only — SSE fanout measures the live path, which does not change
    // backend, and scenario 4 is defined against the file store.
    #[cfg(feature = "postgres")]
    if let Ok(url) = std::env::var("DATABASE_URL") {
        let store =
            std::env::temp_dir().join(format!("rusty-load-envelope-pg-{}", uuid::Uuid::new_v4()));
        let server = TestServer::spawn(store, Some(url)).await?;
        let runs = scenario_concurrent_runs(
            &client,
            &server.base,
            "postgres",
            config.runs,
            config.concurrency,
        )
        .await;
        runs.print();
        scenarios.push(runs.to_json());
        let tasks = scenario_task_queue(
            &client,
            &server.base,
            "postgres",
            config.task_ops,
            config.concurrency,
        )
        .await;
        tasks.print();
        scenarios.push(tasks.to_json());
        server.stop().await;
    }

    #[cfg(not(feature = "postgres"))]
    if std::env::var("DATABASE_URL").is_ok() {
        println!(
            "note: DATABASE_URL is set but this binary was built without the `postgres`\nfeature; skipping the Postgres pass (rebuild with --features postgres)"
        );
    }

    let report = json!({
        "harness": "load_envelope",
        "started_at": started_at,
        "units": {"durations": "ms", "rates": "ops/sec"},
        "config": {
            "runs": config.runs,
            "concurrency": config.concurrency,
            "sse_streams": config.sse_streams,
            "task_ops": config.task_ops,
        },
        "scenarios": scenarios,
    });

    if let Some(path) = &config.json_out {
        std::fs::write(path, serde_json::to_string_pretty(&report)?)?;
        println!("json report written to {}", path.display());
    }
    Ok(())
}
