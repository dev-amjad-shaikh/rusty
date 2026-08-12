#!/usr/bin/env bash
# scripts/load-envelope.sh — build and run the R1.0 capacity-envelope load
# harness (rusty-server/examples/load_envelope.rs).
#
#   ./scripts/load-envelope.sh [--json path]
#   WITH_POSTGRES=1 ./scripts/load-envelope.sh --json target/envelope.json
#
# The default backend is the JSON-file store. WITH_POSTGRES=1 additionally
# runs the Postgres scenarios against a throwaway postgres:17-alpine
# container (127.0.0.1:54317, user/pass/db `rusty`), removed on exit;
# requires docker. Extra cargo features pass through via FEATURES:
#
#   FEATURES="postgres,capsules" ./scripts/load-envelope.sh
#
# Harness sizes pass through via the harness's own env vars
# (LOAD_ENVELOPE_RUNS, LOAD_ENVELOPE_CONCURRENCY, LOAD_ENVELOPE_SSE_STREAMS,
# LOAD_ENVELOPE_TASK_OPS).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"
export PATH="$HOME/.cargo/bin:$PATH"

WITH_POSTGRES="${WITH_POSTGRES:-0}"
FEATURES="${FEATURES:-}"
PG_PORT="${LOAD_ENVELOPE_PG_PORT:-54317}"
CONTAINER="${LOAD_ENVELOPE_PG_CONTAINER:-rusty-load-envelope-pg}"

command -v cargo >/dev/null 2>&1 || { echo "error: cargo not found (install a Rust toolchain via rustup)" >&2; exit 1; }

if [ "$WITH_POSTGRES" = "1" ]; then
  command -v docker >/dev/null 2>&1 || { echo "error: WITH_POSTGRES=1 needs docker" >&2; exit 1; }
  case ",$FEATURES," in
    *",postgres,"*) ;;
    *) FEATURES="${FEATURES:+$FEATURES,}postgres" ;;
  esac
fi

BUILD_ARGS=(-p rusty-agent-server --example load_envelope --release)
if [ -n "$FEATURES" ]; then
  BUILD_ARGS+=(--features "$FEATURES")
fi

echo "Building load_envelope (release${FEATURES:+, features: $FEATURES}) ..."
cargo build "${BUILD_ARGS[@]}"

cleanup() {
  if [ "$WITH_POSTGRES" = "1" ]; then
    docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT INT TERM

if [ "$WITH_POSTGRES" = "1" ]; then
  docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
  echo "Starting throwaway Postgres ($CONTAINER on 127.0.0.1:$PG_PORT) ..."
  docker run -d --name "$CONTAINER" \
    -e POSTGRES_USER=rusty -e POSTGRES_PASSWORD=rusty -e POSTGRES_DB=rusty \
    -p "127.0.0.1:$PG_PORT:5432" postgres:17-alpine >/dev/null

  echo -n "Waiting for Postgres"
  ready=""
  for _ in $(seq 1 60); do
    if docker exec "$CONTAINER" pg_isready -U rusty -d rusty >/dev/null 2>&1; then
      ready=1
      echo " — up."
      break
    fi
    echo -n "."
    sleep 1
  done
  if [ -z "$ready" ]; then
    echo
    echo "error: Postgres did not become ready within 60s" >&2
    exit 1
  fi
  export DATABASE_URL="postgres://rusty:rusty@127.0.0.1:$PG_PORT/rusty"
fi

# Run the built binary directly (not via `cargo run`), matching dev.sh.
"target/release/examples/load_envelope" "$@"
