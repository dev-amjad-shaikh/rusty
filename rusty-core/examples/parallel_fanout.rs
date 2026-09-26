//! Example: dynamic fan-out / fan-in with the `Send` API (map-reduce).
//!
//! This is the LangGraph `Send` pattern expressed in Rusty Core:
//!
//! ```text
//!                    ┌──────────────────┐
//!                    │ generate_topics  │  emits N items at runtime
//!                    └────────┬─────────┘
//!                             │ conditional router → Route::Send([...])
//!                             │ one Send per item (dynamic fan-out)
//!              ┌──────────────┼──────────────┬──────────────┐
//!              ▼              ▼              ▼              ▼
//!        process_item   process_item   process_item   process_item   ← one
//!              │              │              │              │          super-step,
//!              └──────────────┴──────┬───────┴──────────────┘          parallel
//!                                    │ fan-in via Reducer::Append
//!                             ┌──────▼──────┐
//!                             │  summarize  │  runs after the barrier
//!                             └─────────────┘
//! ```
//!
//! Key ideas demonstrated:
//!
//! 1. **Dynamic routing** — the number of parallel branches is not known at
//!    graph-build time. A conditional router inspects the post-barrier state
//!    and returns [`Route::Send`] with one [`Send`] per item.
//! 2. **Scoped input state** — each `Send` carries a small JSON object that
//!    is merged into the shared state snapshot *for that invocation only*, so
//!    each `process_item` invocation sees its own `item` (see the `Send`
//!    doc comment in `src/graph.rs`).
//! 3. **Safe fan-in** — all `process_item` invocations write to the same
//!    `results` channel in the same super-step. That is only legal because
//!    `results` uses [`Reducer::Append`] (multi-write). A `LastValue`-style
//!    channel would fail with `InvalidUpdate` — the classic parallel-graph
//!    bug class this engine is designed to catch.
//! 4. **Barrier semantics** — `summarize` runs in the *next* super-step, so
//!    it observes the fully merged `results` array, never a partial one.
//!
//! Everything is pure computation (no network, no LLM); each node prints a
//! trace line so the super-step structure is visible in the output.
//!
//! Run with: `cargo run --example parallel_fanout`

use rusty_agent_runtime::prelude::*;
use serde_json::{json, Value};

#[tokio::main]
async fn main() -> Result<()> {
    println!("=== parallel_fanout: dynamic map-reduce via Route::Send ===\n");

    // ---------------------------------------------------------------------
    // State schema. Every channel must be declared; undeclared writes are
    // rejected at the barrier with `RustyError::InvalidUpdate`.
    // ---------------------------------------------------------------------
    let spec = StateSpec::new()
        // Written exactly once per super-step (by `generate_topics`), so the
        // default LastValue semantics are fine.
        .channel("topics", Reducer::Overwrite)
        // Scoped per-`Send` input: each `process_item` invocation receives
        // its own `item` merged over the shared snapshot.
        .channel("item", Reducer::Overwrite)
        // THE fan-in channel: N parallel `process_item` invocations each
        // push one record in the same super-step. Append (list-concat)
        // semantics make those concurrent writes legal.
        .channel("results", Reducer::Append)
        // Single writer (`summarize`) → LastValue semantics.
        .channel("summary", Reducer::Overwrite);

    // ---------------------------------------------------------------------
    // Graph construction.
    // ---------------------------------------------------------------------
    let mut builder = GraphBuilder::new();

    // MAP phase, step 0: emit the work items. In a real agent this might be
    // "generate N research sub-questions"; here it is a fixed list of
    // purely computational topics.
    builder.add_node("generate_topics", |_ctx: NodeContext| async move {
        let topics = vec![
            "super-step scheduling",
            "channel reducers",
            "checkpoint persistence",
            "interrupt/resume",
        ];
        println!("[generate_topics] emitting {} topics", topics.len());
        Ok(NodeOutput::update("topics", json!(topics)))
    });

    // MAP phase, step 1: one invocation per topic (activated by the Sends
    // below). Reads its scoped `item`, does a deterministic pure
    // computation, and appends one record to `results`.
    builder.add_node("process_item", |ctx: NodeContext| async move {
        let topic = ctx
            .state()
            .get("item")
            .and_then(Value::as_str)
            .unwrap_or("<missing item>")
            .to_owned();

        // Pure, deterministic stand-in for real per-item work.
        let checksum: u32 = topic.bytes().map(u32::from).sum();
        let record = json!({
            "topic": topic,
            "chars": topic.chars().count(),
            "checksum": checksum,
        });

        println!(
            "[process_item] (step {}) processed {:?} -> checksum {}",
            ctx.step(),
            topic,
            checksum
        );
        // A single (non-array) update value is pushed as one element by
        // Reducer::Append; sibling invocations' pushes concatenate.
        Ok(NodeOutput::update("results", record))
    });

    // REDUCE phase, step 2: runs once, after the barrier, with all mapped
    // records merged into `results`.
    builder.add_node("summarize", |ctx: NodeContext| async move {
        let results = ctx
            .state()
            .get("results")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let total_checksum: u64 = results
            .iter()
            .filter_map(|r| r.get("checksum").and_then(Value::as_u64))
            .sum();
        let summary = format!(
            "fan-in complete: {} results merged, total checksum {}",
            results.len(),
            total_checksum
        );
        println!("[summarize] {summary}");
        Ok(NodeOutput::update("summary", json!(summary)))
    });

    builder.set_entry_point("generate_topics");

    // Dynamic fan-out: read the topics produced by `generate_topics` and
    // return ONE `Send` PER TOPIC. Each Send activates `process_item` once
    // with `{"item": <topic>}` as its scoped input state.
    builder.add_conditional_edges("generate_topics", |state| async move {
        let topics = state
            .get("topics")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        println!(
            "[router] fanning out {} Sends to `process_item`",
            topics.len()
        );
        let sends = topics
            .iter()
            .map(|t| Send::new("process_item", json!({ "item": t })))
            .collect();
        Ok(Route::Send(sends))
    });

    // Fan-in edge: after all `process_item` invocations of the super-step
    // complete and their writes merge at the barrier, `summarize` runs once.
    builder.add_edge("process_item", "summarize");

    // compile() validates structure up front (entry point, edge endpoints).
    let graph = builder.compile()?;

    // ---------------------------------------------------------------------
    // Run. No persistence needed for this example, so a plain executor.
    // ---------------------------------------------------------------------
    let outcome = Executor::new()
        .run(&graph, &spec, State::new(), RunConfig::new("fanout-demo"))
        .await?;

    match outcome {
        ExecutionOutcome::Done(state) => {
            println!("\n=== run finished (Done) ===");
            println!(
                "final summary: {}",
                state.get("summary").cloned().unwrap_or(Value::Null)
            );
            println!(
                "results channel: {}",
                serde_json::to_string_pretty(state.get("results").unwrap_or(&Value::Null))?
            );
        }
        interrupted => {
            // This graph never interrupts; print defensively.
            println!("unexpected outcome; state: {:?}", interrupted.state());
        }
    }

    Ok(())
}
