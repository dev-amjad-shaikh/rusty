//! Example: human-in-the-loop with interrupt/resume + durable checkpoints.
//!
//! A minimal approval pipeline:
//!
//! ```text
//!   draft ──► approve ──► publish
//!                │
//!                └─ calls NodeContext::interrupt(payload) on the first run,
//!                   suspending the graph until a human decision arrives
//! ```
//!
//! The two-phase protocol (mirrors LangGraph's `interrupt` / `Command(resume=...)`):
//!
//! **Phase 1 — suspend.** The `approve` node has no human decision yet, so
//! it returns `Err(ctx.interrupt(review_payload))`. The executor unwinds the
//! in-flight super-step (transactional: the step's writes are discarded),
//! persists a checkpoint for the thread via [`JsonFileCheckpointer`], and
//! returns [`ExecutionOutcome::Interrupted`] carrying the review payload and
//! the checkpoint id. The payload is what a UI would render to the human.
//!
//! **Phase 2 — resume.** The run is continued with the SAME `thread_id` and
//! `RunConfig::resume` set to the human's decision. The executor restores the
//! latest checkpoint and **re-executes the interrupted node from its start**
//! — this is why node logic must be idempotent. This time
//! `ctx.resume_value()` is `Some`, so `approve` records the decision and the
//! pipeline flows on to `publish`, ending in [`ExecutionOutcome::Done`].
//!
//! Persistence is real: checkpoints land as JSON files under
//! `target/examples-checkpoints/human-in-loop/<thread_id>/`, so the two
//! phases could equally well be separate process invocations (that is the
//! point of a file-backed checkpointer).
//!
//! Run with: `cargo run --example human_in_loop`

use std::path::PathBuf;
use std::sync::Arc;

use rusty_agent_runtime::prelude::*;
use serde_json::{json, Value};

#[tokio::main]
async fn main() -> Result<()> {
    println!("=== human_in_loop: interrupt/resume with JsonFileCheckpointer ===\n");

    // ---------------------------------------------------------------------
    // State schema. Each channel has exactly one writer node in this linear
    // pipeline, so LastValue (Overwrite) semantics are correct everywhere.
    // ---------------------------------------------------------------------
    let spec = StateSpec::new()
        .channel("draft", Reducer::Overwrite)
        .channel("approval", Reducer::Overwrite)
        .channel("published", Reducer::Overwrite);

    // ---------------------------------------------------------------------
    // Graph: draft -> approve -> publish (all static edges).
    // ---------------------------------------------------------------------
    let mut builder = GraphBuilder::new();

    builder.add_node("draft", |_ctx: NodeContext| async move {
        let draft = "Rusty makes cyclic, resumable agent graphs safe in Rust.";
        println!("[draft] wrote draft: {draft:?}");
        Ok(NodeOutput::update("draft", json!(draft)))
    });

    // The resumable node. Note the canonical pattern: check
    // `ctx.resume_value()` FIRST; only interrupt when there is no decision.
    // On resume this node re-runs from the top, so everything before the
    // interrupt must be idempotent (here: nothing but a log line).
    builder.add_node("approve", |ctx: NodeContext| async move {
        match ctx.resume_value() {
            // Phase 2: a human decision was supplied via RunConfig::resume.
            Some(decision) => {
                println!("[approve] resumed with human decision: {decision}");
                Ok(NodeOutput::update("approval", decision.clone()))
            }
            // Phase 1: no decision yet — suspend the whole run. The payload
            // is surfaced to the caller in ExecutionOutcome::Interrupted.
            None => {
                let draft = ctx.state().get("draft").cloned().unwrap_or(Value::Null);
                println!("[approve] no decision available — interrupting for human review");
                Err(ctx.interrupt(json!({
                    "kind": "approval_request",
                    "prompt": "Approve this draft for publication?",
                    "draft": draft,
                })))
            }
        }
    });

    builder.add_node("publish", |ctx: NodeContext| async move {
        let draft = ctx.state().get("draft").cloned().unwrap_or(Value::Null);
        let approved = ctx
            .state()
            .get("approval")
            .and_then(|a| a.get("approved"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let published = json!({
            "draft": draft,
            "approved": approved,
            "status": if approved { "published" } else { "rejected" },
        });
        println!("[publish] {}", published["status"]);
        Ok(NodeOutput::update("published", published))
    });

    builder.set_entry_point("draft");
    builder.add_edge("draft", "approve");
    builder.add_edge("approve", "publish");
    let graph = builder.compile()?;

    // ---------------------------------------------------------------------
    // Executor with durable, file-backed checkpoints under target/.
    // ---------------------------------------------------------------------
    let checkpoint_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/examples-checkpoints/human-in-loop");
    println!("checkpoint dir: {}\n", checkpoint_dir.display());
    let executor = Executor::with_checkpointer(Arc::new(JsonFileCheckpointer::new(checkpoint_dir)));

    // The thread id is the resume handle: it namespaces the checkpoints and
    // MUST be identical across the two phases.
    let thread_id = "hitl-demo-thread";

    // ---------------------------------------------------------------------
    // PHASE 1: initial run — expected to suspend at `approve`.
    // ---------------------------------------------------------------------
    println!("--- PHASE 1: initial run (expect interrupt) ---");
    let outcome = executor
        .run(&graph, &spec, State::new(), RunConfig::new(thread_id))
        .await?;

    match outcome {
        ExecutionOutcome::Interrupted {
            value,
            checkpoint_id,
            ..
        } => {
            println!("[phase 1] run suspended at `approve`");
            println!("[phase 1] review payload surfaced to the human: {value}");
            println!("[phase 1] durable checkpoint id: {checkpoint_id}\n");
        }
        ExecutionOutcome::Done(_) => {
            println!("[phase 1] UNEXPECTED: run completed without interrupting\n");
        }
    }

    // ---------------------------------------------------------------------
    // PHASE 2: resume with the human's approval decision. Same thread_id!
    // The executor restores the checkpoint, re-runs `approve` with
    // ctx.resume_value() == Some(decision), and flows on to `publish`.
    // ---------------------------------------------------------------------
    println!("--- PHASE 2: resume with human approval ---");
    let human_decision = json!({
        "approved": true,
        "reviewer": "alice",
        "comment": "ship it",
    });
    let outcome = executor
        .run(
            &graph,
            &spec,
            State::new(),
            RunConfig::new(thread_id).with_resume(human_decision.clone()),
        )
        .await?;

    match outcome {
        ExecutionOutcome::Done(state) => {
            println!("[phase 2] run completed after resume");
            println!(
                "published channel: {}",
                serde_json::to_string_pretty(state.get("published").unwrap_or(&Value::Null))?
            );
        }
        ExecutionOutcome::Interrupted { .. } => {
            println!("[phase 2] UNEXPECTED: run interrupted again");
        }
    }

    Ok(())
}
