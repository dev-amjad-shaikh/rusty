#!/usr/bin/env bash
# scripts/dev.sh — boot Rusty Server (the server_demo example) and Rusty
# Studio together, locally, no Docker. Ctrl-C stops both.
#
#   ./scripts/dev.sh
#
#   Rusty Server  →  http://127.0.0.1:8100
#   Rusty Studio  →  http://127.0.0.1:8000  (connect with base URL /api)
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"
export PATH="$HOME/.cargo/bin:$PATH"

SERVER_PORT="${RUSTY_SERVER_PORT:-8100}"
STUDIO_PORT="${RUSTY_STUDIO_PORT:-8000}"

command -v cargo >/dev/null 2>&1 || { echo "error: cargo not found (install a Rust toolchain via rustup)" >&2; exit 1; }
command -v python3 >/dev/null 2>&1 || { echo "error: python3 not found" >&2; exit 1; }

echo "Building Rusty Server (rusty-server/examples/server_demo.rs) ..."
cargo build -p rusty-agent-server --example server_demo

# Run the built binary directly (not via `cargo run`) so its PID is the
# process to kill on exit. Workspace builds place examples in the root
# target/ directory.
"target/debug/examples/server_demo" &
SERVER_PID=$!

python3 studio/serve.py --port "$STUDIO_PORT" --target "http://127.0.0.1:$SERVER_PORT" &
STUDIO_PID=$!

cleanup() {
  kill "$SERVER_PID" "$STUDIO_PID" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

echo -n "Waiting for Rusty Server on 127.0.0.1:$SERVER_PORT"
ready=""
for _ in $(seq 1 60); do
  if curl -sf "http://127.0.0.1:$SERVER_PORT/ok" >/dev/null 2>&1; then
    ready=1
    echo " — up."
    break
  fi
  if ! kill -0 "$SERVER_PID" 2>/dev/null; then
    echo
    echo "error: server process exited before becoming ready" >&2
    exit 1
  fi
  echo -n "."
  sleep 1
done
if [ -z "$ready" ]; then
  echo
  echo "error: server did not answer /ok within 60s" >&2
  exit 1
fi

cat <<EOF

  Rusty Server  →  http://127.0.0.1:$SERVER_PORT   (try: curl 127.0.0.1:$SERVER_PORT/info)
  Rusty Studio  →  http://127.0.0.1:$STUDIO_PORT    (connect with base URL /api)

Ctrl-C stops both.
EOF

wait
