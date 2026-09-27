#!/usr/bin/env bash
# scripts/durability-bench.sh — build and run the durability-economics
# harness (rusty-core/examples/durability_bench.rs).
#
#   ./scripts/durability-bench.sh [--json path]
#
# Three experiments in one run, all offline and deterministic (scripted
# ChatModel, no network):
#
#   A. checkpoint overhead per super-step — 200-node chain, no vs in-memory
#      vs JSON-file checkpointer;
#   B. RSS vs super-steps — 300-step self-loop with a file checkpointer, the
#      process sampling its own RSS at every checkpoint;
#   C. resume vs restart — 48 model calls interrupted at step 24; byte-exact
#      accounting of what resume pays after the interruption vs what a from-
#      scratch restart pays without durability.
#
# Sizes are tunable via the harness's env vars (DURABILITY_BENCH_CHAIN,
# DURABILITY_BENCH_REPS, DURABILITY_BENCH_RSS_STEPS,
# DURABILITY_BENCH_ECON_STEPS, DURABILITY_BENCH_MSG_BYTES,
# DURABILITY_BENCH_INTERRUPT_AT, DURABILITY_BENCH_RSS_CHECKPOINTER, and
# DURABILITY_BENCH_ONLY to run a subset — `a`, `b-events`, `b-noevents`,
# `c` — which the RSS probes need for a fair fresh-process comparison).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"
export PATH="$HOME/.cargo/bin:$PATH"

command -v cargo >/dev/null 2>&1 || { echo "error: cargo not found (install a Rust toolchain via rustup)" >&2; exit 1; }

echo "Building durability_bench (release) ..."
cargo build -p rusty-agent-runtime --example durability_bench --release

# Run the built binary directly (not via `cargo run`), matching dev.sh.
"target/release/examples/durability_bench" "$@"
