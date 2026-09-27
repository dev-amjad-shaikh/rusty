//! Durability economics: what checkpoints actually cost, and what they save.
//!
//! Three experiments in one binary, all deterministic and offline (scripted
//! [`ChatModel`], no network):
//!
//! **A. Checkpoint overhead per super-step.** A 200-node linear chain run
//! end-to-end with no checkpointer, an in-memory checkpointer, and a
//! JSON-file checkpointer. The per-step deltas isolate what durability adds
//! to the super-step loop itself, as opposed to the microbenchmarks in
//! `benches/checkpoint.rs` which time `put()` in isolation.
//!
//! **B. RSS vs super-steps.** A self-loop graph runs 300 super-steps with a
//! growing transcript and a file checkpointer attached; the process samples
//! its own RSS at every checkpoint. Answers: does a long-lived run's memory
//! grow unboundedly, and how big does the checkpoint trail get on disk?
//!
//! **C. Resume vs restart.** The same self-loop graph runs 48 model calls,
//! interrupted at step 24. Three paths are measured with byte-exact scripted
//! accounting: a full uninterrupted run, a resume from the step-24
//! checkpoint (what durability pays after the interruption), and a from-
//! scratch restart (what losing the process costs without durability). The
//! model context grows every step, so tail steps are the expensive ones —
//! the savings ratio is the honest version of "resume saves tokens".
//!
//! The JSON summary goes to stdout; progress goes to stderr. Sizes are
//! configurable via `DURABILITY_BENCH_CHAIN`, `DURABILITY_BENCH_REPS`,
//! `DURABILITY_BENCH_RSS_STEPS`, `DURABILITY_BENCH_ECON_STEPS`,
//! `DURABILITY_BENCH_MSG_BYTES`, `DURABILITY_BENCH_INTERRUPT_AT`,
//! `DURABILITY_BENCH_RSS_CHECKPOINTER` (none|memory|file), and
//! `DURABILITY_BENCH_ONLY` (comma-separated subset of `a`, `b-events`,
//! `b-noevents`, `c` — the RSS probes need fresh processes per
//! configuration, because malloc retains a process's RSS peak).
//!
//! Run with: `cargo run --release --example durability_bench`
//! (or `./scripts/durability-bench.sh --json target/durability-bench.json`)

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use rusty_agent_runtime::prelude::*;
use serde::Serialize;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Scripted model: fixed-size responses, byte-exact accounting.
// ---------------------------------------------------------------------------

/// What one path of the economics experiment spent at the model seam. Bytes,
/// not tokens — the scripted model has no tokenizer, so we count the exact
/// serialized context the model was handed plus the exact response bytes.
/// (`bytes / 4` is the usual rough token equivalence; the docs report it as
/// a heuristic, not a measurement.)
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq, Serialize)]
struct ModelSpend {
    calls: u64,
    input_bytes: u64,
    output_bytes: u64,
}

impl ModelSpend {
    fn token_equiv(&self) -> u64 {
        (self.input_bytes + self.output_bytes) / 4
    }
}

/// A scripted [`ChatModel`] that always answers with a fixed-size string and
/// records every call's context size into a shared counter. Deterministic:
/// same transcript in, same bytes out — which is what makes the three paths
/// in experiment C directly comparable.
struct AccountingModel {
    response: String,
    spend: Arc<Mutex<ModelSpend>>,
}

impl AccountingModel {
    fn new(msg_bytes: usize, spend: Arc<Mutex<ModelSpend>>) -> Self {
        // A realistic-ish payload: repetitive but not one repeated byte.
        let unit = "lorem ipsum dolor sit amet, consectetur adipiscing elit. ";
        let mut response = String::new();
        while response.len() < msg_bytes {
            response.push_str(unit);
        }
        response.truncate(msg_bytes);
        Self { response, spend }
    }
}

#[async_trait]
impl ChatModel for AccountingModel {
    async fn chat(&self, messages: &[ChatMessage], _tools: &[Value]) -> Result<ChatResponse> {
        let input_bytes = serde_json::to_vec(messages)
            .map(|v| v.len() as u64)
            .unwrap_or(0);
        let mut spend = self.spend.lock().unwrap();
        spend.calls += 1;
        spend.input_bytes += input_bytes;
        spend.output_bytes += self.response.len() as u64;
        drop(spend);
        Ok(ChatResponse {
            message: ChatMessage::assistant(self.response.clone()),
            model: Some("accounting-scripted-1".into()),
            usage: None,
        })
    }
}

/// One fixed-size user instruction per step, so the transcript — and thus
/// the model context — grows linearly with step count.
fn user_turn(step: usize, msg_bytes: usize) -> String {
    let unit = format!("step {step}: continue the analysis with the next slice. ");
    let mut text = String::new();
    while text.len() < msg_bytes {
        text.push_str(&unit);
    }
    text.truncate(msg_bytes);
    text
}

// ---------------------------------------------------------------------------
// Self-loop graph: one node, one model call per super-step, interruptible.
// ---------------------------------------------------------------------------

/// `turn` calls the model with the transcript so far and appends the
/// exchange. With `interrupt_at` set, the node suspends the run at that
/// super-step (until resumed, mirroring `human_in_loop`). Termination is a
/// conditional edge: once the transcript holds `total_steps` assistant
/// replies the router returns [`Route::End`]. (A node cannot name `__end__`
/// in a `Command::goto` — END is only reachable as a route.)
///
/// Every 25th step the node samples the process's own RSS into the returned
/// log, so experiment B needs no event channel to build its curve.
fn transcript_graph(
    msg_bytes: usize,
    interrupt_at: Option<usize>,
    total_steps: usize,
    model: Arc<dyn ChatModel>,
) -> (Graph, StateSpec, RssLog) {
    let spec = StateSpec::new().channel("transcript", Reducer::Overwrite);
    let rss_log: RssLog = Arc::new(Mutex::new(Vec::new()));
    // The engine delivers `resume_value` to the first super-step only; a
    // step-counter interrupt would re-fire on every later step. This latch
    // remembers that the resume arrived. (Real graphs gate interrupts on
    // idempotent state instead; the bench uses a step counter, so it needs
    // the latch.)
    let resumed = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let mut builder = GraphBuilder::new();
    let rss_log_for_node = rss_log.clone();
    builder.add_node("turn", move |ctx: NodeContext| {
        let model = model.clone();
        let resumed = resumed.clone();
        let rss_log = rss_log_for_node.clone();
        async move {
            if ctx.step() % 25 == 0 || ctx.step() + 1 >= total_steps {
                if let Some(kb) = self_rss_kb() {
                    rss_log.lock().unwrap().push((ctx.step(), kb));
                }
            }
            if ctx.resume_value().is_some() {
                resumed.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            if !resumed.load(std::sync::atomic::Ordering::SeqCst)
                && interrupt_at.is_some_and(|at| ctx.step() >= at)
            {
                return Err(ctx.interrupt(json!({"parked_at_step": ctx.step()})));
            }
            let transcript: Vec<ChatMessage> =
                ctx.state().get_as("transcript")?.unwrap_or_default();
            let mut response = transcript.clone();
            response.push(ChatMessage::user(user_turn(ctx.step(), msg_bytes)));
            let answer = model.chat(&response, &[]).await?;
            let mut next = response;
            next.push(answer.message);
            Ok(NodeOutput::update(
                "transcript",
                serde_json::to_value(next)?,
            ))
        }
    });
    builder.set_entry_point("turn");
    builder.add_conditional_edges("turn", move |state: State| {
        let done_after = total_steps;
        async move {
            let assistants = state
                .get("transcript")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter(|m| m.get("role").and_then(Value::as_str) == Some("assistant"))
                        .count()
                })
                .unwrap_or(0);
            if assistants >= done_after {
                Ok(Route::End)
            } else {
                Ok(Route::Node("turn".into()))
            }
        }
    });
    let graph = builder.compile().expect("transcript graph compiles");
    (graph, spec, rss_log)
}

/// `(step, rss_kb)` pairs sampled by the node itself.
type RssLog = Arc<Mutex<Vec<(usize, u64)>>>;

fn initial_transcript() -> State {
    let mut state = State::new();
    state.insert("transcript", json!(Vec::<ChatMessage>::new()));
    state
}

async fn run_steps(
    graph: &Graph,
    spec: &StateSpec,
    executor: &Executor,
    thread_id: &str,
    resume: Option<Value>,
) -> Result<ExecutionOutcome> {
    let mut config = RunConfig::new(thread_id).with_max_steps(100_000);
    if let Some(value) = resume {
        config = config.with_resume(value);
    }
    executor
        .run(graph, spec, initial_transcript(), config)
        .await
}

// ---------------------------------------------------------------------------
// Experiment A: checkpoint overhead per super-step (200-node chain).
// ---------------------------------------------------------------------------

/// Minimal copy of `benches/common::chain_graph` (benches aren't importable
/// from examples): node `n_i` reads `c_{i-1}`, writes `c_i = prev + 1`.
fn chain_graph(n: usize) -> (Graph, StateSpec) {
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

fn checkpoint_overhead(chain_nodes: usize, reps: usize, root: &std::path::Path) -> Value {
    let (graph, spec) = chain_graph(chain_nodes);
    let state = State::new();

    // Each rep gets a fresh file-checkpointer dir so renames never contend.
    let time = |rep: usize, file: bool| {
        let executor = if file {
            let dir = root.join(format!("overhead-{rep}"));
            Executor::with_checkpointer(Arc::new(JsonFileCheckpointer::new(dir)))
        } else {
            Executor::new()
        };
        futures_executor(&executor, &graph, &spec, &state, &format!("chain-{rep}"))
    };

    let mut plain_us = Vec::new();
    let mut mem_us = Vec::new();
    let mut file_us = Vec::new();
    for rep in 0..reps {
        // Warm the file backend's directory once outside the measurement.
        let _ = std::fs::create_dir_all(root.join(format!("overhead-{rep}")));
        plain_us.push(time(rep, false));
        mem_us.push(time(rep, false)); // placeholder replaced below
                                       // In-memory checkpointer run.
        let executor = Executor::with_checkpointer(Arc::new(InMemoryCheckpointer::new()));
        let start = Instant::now();
        futures_executor(
            &executor,
            &graph,
            &spec,
            &state,
            &format!("chain-mem-{rep}"),
        );
        mem_us[rep] = start.elapsed().as_micros() as u64;
        file_us.push(time(rep, true));
    }

    let per_step =
        |v: &[u64]| -> f64 { (*v.iter().min().expect("non-empty") as f64) / chain_nodes as f64 };
    let plain = per_step(&plain_us);
    let mem = per_step(&mem_us);
    let file = per_step(&file_us);
    json!({
        "chain_nodes": chain_nodes,
        "reps": reps,
        "unit": "us_per_super_step (min over reps)",
        "no_checkpoint": plain,
        "in_memory": mem,
        "json_file": file,
        "in_memory_overhead": mem - plain,
        "json_file_overhead": file - plain,
    })
}

/// Run the executor on a current-thread runtime (benchmarks in this repo use
/// a multi-threaded runtime, but the chain is sequential; a current-thread
/// runtime keeps the measurement free of scheduler noise).
fn futures_executor(
    executor: &Executor,
    graph: &Graph,
    spec: &StateSpec,
    state: &State,
    thread_id: &str,
) -> u64 {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime builds");
    let start = Instant::now();
    runtime
        .block_on(executor.run(graph, spec, state.clone(), RunConfig::new(thread_id)))
        .expect("chain run completes");
    start.elapsed().as_micros() as u64
}

// ---------------------------------------------------------------------------
// Experiment B: RSS vs super-steps (self-sampling at each checkpoint).
// ---------------------------------------------------------------------------

/// This process's own resident set size in KiB, via `ps` (works on macOS and
/// Linux with identical flags). `None` if the platform surprises us.
fn self_rss_kb() -> Option<u64> {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()?;
    String::from_utf8(out.stdout)
        .ok()?
        .trim()
        .split_ascii_whitespace()
        .next()?
        .parse()
        .ok()
}

fn dir_size_bytes(path: &std::path::Path) -> u64 {
    let mut total = 0;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                total += dir_size_bytes(&p);
            } else if let Ok(meta) = entry.metadata() {
                total += meta.len();
            }
        }
    }
    total
}

fn rss_vs_steps(steps: usize, msg_bytes: usize, events: bool, root: &std::path::Path) -> Value {
    let spend = Arc::new(Mutex::new(ModelSpend::default()));
    let model: Arc<dyn ChatModel> = Arc::new(AccountingModel::new(msg_bytes, spend));
    let (graph, spec, rss_log) = transcript_graph(msg_bytes, None, steps, model);

    let dir = root.join(if events { "rss-events" } else { "rss-noevents" });
    // DURABILITY_BENCH_RSS_CHECKPOINTER=none|memory|file (default file)
    // attributes RSS growth across the persistence path.
    let backend =
        std::env::var("DURABILITY_BENCH_RSS_CHECKPOINTER").unwrap_or_else(|_| "file".to_owned());
    let executor = match backend.as_str() {
        "none" => Executor::new(),
        "memory" => Executor::with_checkpointer(Arc::new(InMemoryCheckpointer::new())),
        _ => Executor::with_checkpointer(Arc::new(JsonFileCheckpointer::new(dir.clone()))),
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime builds");
    runtime.block_on(async move {
        // The event channel is the thing under test here: experiment B runs
        // once with and once without it to separate event-traffic memory
        // from the rest of the curve. The node samples its own RSS either
        // way, through the log it was handed at build time.
        let mut config = RunConfig::new("rss-probe").with_max_steps(100_000);
        if events {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<GraphEvent>(64);
            let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
            config = config.with_event_tx(tx);
            executor
                .run(&graph, &spec, initial_transcript(), config)
                .await
                .expect("rss probe run completes");
            drain.await.expect("event drain finishes");
        } else {
            executor
                .run(&graph, &spec, initial_transcript(), config)
                .await
                .expect("rss probe run completes");
        }
    });

    let samples: Vec<(usize, u64)> = rss_log.lock().unwrap().clone();
    let first = samples.first().copied().unwrap_or((0, 0));
    let last = samples.last().copied().unwrap_or((0, 0));
    json!({
        "steps": steps,
        "msg_bytes": msg_bytes,
        "events": events,
        "checkpointer_backend": backend,
        "samples": samples,
        "rss_first_kb": first.1,
        "rss_last_kb": last.1,
        "rss_delta_kb": last.1 as i64 - first.1 as i64,
        "checkpoint_dir_bytes": dir_size_bytes(&dir),
    })
}

// ---------------------------------------------------------------------------
// Experiment C: resume vs restart, byte-exact.
// ---------------------------------------------------------------------------

fn resume_economics(
    steps: usize,
    interrupt_at: usize,
    msg_bytes: usize,
    root: &std::path::Path,
) -> Value {
    // Path 1 — full uninterrupted run.
    let full_spend = Arc::new(Mutex::new(ModelSpend::default()));
    let full_model: Arc<dyn ChatModel> =
        Arc::new(AccountingModel::new(msg_bytes, full_spend.clone()));
    let (full_graph, full_spec, _log) = transcript_graph(msg_bytes, None, steps, full_model);
    let executor = Executor::new();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime builds");
    runtime
        .block_on(run_steps(
            &full_graph,
            &full_spec,
            &executor,
            "econ-full",
            None,
        ))
        .expect("full run completes");

    // Path 2 — interrupt at the halfway super-step, then resume the thread.
    // Phase 1 and phase 2 are measured separately: what matters is phase 2
    // (what durability pays after the interruption). The interrupted step
    // re-runs on resume — that duplicate model call is real and stays in the
    // phase-2 count.
    let p1_spend = Arc::new(Mutex::new(ModelSpend::default()));
    let p1_model: Arc<dyn ChatModel> = Arc::new(AccountingModel::new(msg_bytes, p1_spend.clone()));
    let (resume_graph, resume_spec, _log) =
        transcript_graph(msg_bytes, Some(interrupt_at), steps, p1_model);
    let dir = root.join("econ-resume");
    let durable = Executor::with_checkpointer(Arc::new(JsonFileCheckpointer::new(dir)));
    let outcome = runtime
        .block_on(run_steps(
            &resume_graph,
            &resume_spec,
            &durable,
            "econ-resume",
            None,
        ))
        .expect("interrupt run executes");
    assert!(
        matches!(outcome, ExecutionOutcome::Interrupted { .. }),
        "the run parks at step {interrupt_at}"
    );
    let p1 = *p1_spend.lock().unwrap();

    let p2_spend = Arc::new(Mutex::new(ModelSpend::default()));
    let p2_model: Arc<dyn ChatModel> = Arc::new(AccountingModel::new(msg_bytes, p2_spend.clone()));
    let (resume_graph, resume_spec, _log) =
        transcript_graph(msg_bytes, Some(interrupt_at), steps, p2_model);
    let outcome = runtime
        .block_on(run_steps(
            &resume_graph,
            &resume_spec,
            &durable,
            "econ-resume",
            Some(json!({"continue": true})),
        ))
        .expect("resume run executes");
    assert!(
        matches!(outcome, ExecutionOutcome::Done(_)),
        "the resumed run completes, got: {outcome:?}"
    );
    let p2 = *p2_spend.lock().unwrap();

    // Path 3 — the counterfactual: the process died at the halfway point and
    // there is no checkpoint, so the run restarts from scratch. The model is
    // deterministic, so this must reproduce the full run byte-for-byte —
    // assert that instead of assuming it.
    let restart_spend = Arc::new(Mutex::new(ModelSpend::default()));
    let restart_model: Arc<dyn ChatModel> =
        Arc::new(AccountingModel::new(msg_bytes, restart_spend.clone()));
    let (restart_graph, restart_spec, _log) =
        transcript_graph(msg_bytes, None, steps, restart_model);
    let executor = Executor::new();
    runtime
        .block_on(run_steps(
            &restart_graph,
            &restart_spec,
            &executor,
            "econ-restart",
            None,
        ))
        .expect("restart run completes");
    let restart = *restart_spend.lock().unwrap();
    assert_eq!(
        restart,
        *full_spend.lock().unwrap(),
        "the scripted model is deterministic: restart must equal full"
    );

    // Sanity: the interrupt fires *before* step `interrupt_at`'s model call.
    // Node-visible steps are 0-based, so phase 1 makes exactly
    // `interrupt_at` calls, and the resumed run re-runs the parked step plus
    // the remainder — `steps - interrupt_at` calls.
    let full = *full_spend.lock().unwrap();
    assert_eq!(
        p1.calls, interrupt_at as u64,
        "phase 1 interrupts at step {interrupt_at}"
    );
    assert_eq!(
        p2.calls,
        (steps - interrupt_at) as u64,
        "phase 2 re-runs the parked step plus the remainder"
    );

    // Savings: model spend after the crash, resume vs restart.
    let after_crash_restart = restart.input_bytes + restart.output_bytes;
    let after_crash_resume = p2.input_bytes + p2.output_bytes;
    let savings_pct = 100.0 * (1.0 - after_crash_resume as f64 / after_crash_restart as f64);

    json!({
        "steps": steps,
        "interrupt_at": interrupt_at,
        "msg_bytes": msg_bytes,
        "full_run": full,
        "resume_phase1_pre_interrupt": p1,
        "resume_phase2_post_interrupt": p2,
        "restart_from_scratch": restart,
        "token_equiv_heuristic_bytes_div4": {
            "full_run": full.token_equiv(),
            "resume_post_interrupt": p2.token_equiv(),
            "restart_from_scratch": restart.token_equiv(),
        },
        "after_crash_restart_bytes": after_crash_restart,
        "after_crash_resume_bytes": after_crash_resume,
        "savings_pct": savings_pct,
    })
}

// ---------------------------------------------------------------------------
// Main: run all three experiments, print a JSON summary on stdout.
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    // `--json <path>` additionally writes the summary to a file (stdout
    // always carries it too, matching load_envelope's contract).
    let mut json_path: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--json" {
            json_path = args.next().map(PathBuf::from);
        }
    }

    let chain_nodes = env_usize("DURABILITY_BENCH_CHAIN", 200);
    let reps = env_usize("DURABILITY_BENCH_REPS", 3);
    let rss_steps = env_usize("DURABILITY_BENCH_RSS_STEPS", 300);
    let econ_steps = env_usize("DURABILITY_BENCH_ECON_STEPS", 48);
    let interrupt_at = env_usize("DURABILITY_BENCH_INTERRUPT_AT", econ_steps / 2);
    assert!(
        interrupt_at < econ_steps,
        "DURABILITY_BENCH_INTERRUPT_AT must be < DURABILITY_BENCH_ECON_STEPS"
    );
    let msg_bytes = env_usize("DURABILITY_BENCH_MSG_BYTES", 256);

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/durability-bench");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("bench root created");

    // DURABILITY_BENCH_ONLY=comma,separated,subset restricts which
    // experiments run (`a`, `b-events`, `b-noevents`, `c`). The two B probes
    // each measure RSS deltas inside one process, so a fair comparison needs
    // fresh processes per probe — run them separately with ONLY set.
    let only: Option<Vec<String>> = std::env::var("DURABILITY_BENCH_ONLY")
        .ok()
        .map(|v| v.split(',').map(|s| s.trim().to_owned()).collect());
    let selected = |name: &str| {
        only.as_ref()
            .map(|l| l.contains(&name.to_owned()))
            .unwrap_or(true)
    };

    let mut summary = json!({
        "generated_by": "rusty-core/examples/durability_bench.rs",
        "crate": env!("CARGO_PKG_NAME"),
        "crate_version": env!("CARGO_PKG_VERSION"),
        "config": {
            "chain_nodes": chain_nodes,
            "reps": reps,
            "rss_steps": rss_steps,
            "econ_steps": econ_steps,
            "interrupt_at": interrupt_at,
            "msg_bytes": msg_bytes,
        },
    });

    if selected("a") {
        eprintln!("A. checkpoint overhead: {chain_nodes}-node chain, {reps} reps …");
        summary["checkpoint_overhead"] = checkpoint_overhead(chain_nodes, reps, &root);
    }
    // Experiment B runs twice: with the GraphEvent channel attached and
    // without. The node samples its own RSS either way, so the difference
    // isolates event-traffic memory from the rest of the curve.
    if selected("b-events") {
        eprintln!(
            "B. rss vs steps: {rss_steps} super-steps, {msg_bytes}-byte messages, events on …"
        );
        summary["rss_vs_steps_events"] = rss_vs_steps(rss_steps, msg_bytes, true, &root);
    }
    if selected("b-noevents") {
        eprintln!(
            "B. rss vs steps: {rss_steps} super-steps, {msg_bytes}-byte messages, events off …"
        );
        summary["rss_vs_steps_no_events"] = rss_vs_steps(rss_steps, msg_bytes, false, &root);
    }
    if selected("c") {
        eprintln!("C. resume vs restart: {econ_steps} steps, interrupt at {interrupt_at} …");
        summary["resume_economics"] = resume_economics(econ_steps, interrupt_at, msg_bytes, &root);
    }
    let rendered = serde_json::to_string_pretty(&summary)?;
    if let Some(path) = json_path {
        std::fs::write(&path, &rendered)
            .map_err(|e| RustyError::Checkpoint(format!("cannot write {}: {e}", path.display())))?;
        eprintln!("summary written to {}", path.display());
    }
    println!("{rendered}");

    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}
