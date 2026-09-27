#!/usr/bin/env bash
# scripts/dev.sh — boot Rusty Server (the server_demo example) and Rusty
# Studio together, locally, no Docker. Ctrl-C stops both.
#
#   ./scripts/dev.sh
#
#   Rusty Server  →  http://127.0.0.1:8100
#   Rusty Studio  →  http://127.0.0.1:4400
#
# Operator configuration (a real model endpoint, keys) lives in
# .env.rusty-local at the repo root — git-ignored, sourced when present:
#
#   RUSTY_LLM_BASE_URL=http://<host>:<port>/v1   # any OpenAI-compatible endpoint
#   RUSTY_LLM_MODEL=<model id>
#   RUSTY_LLM_API_KEY=<key>                      # optional; local boxes need none
#   RUSTY_LLM_EXTRA_BODY='{"chat_template_kwargs":{"enable_thinking":false}}'
#
# Without it the react_agent runs on the deterministic local harness model.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"
export PATH="$HOME/.cargo/bin:$PATH"

SERVER_PORT="${RUSTY_SERVER_PORT:-8100}"
STUDIO_PORT="${RUSTY_STUDIO_PORT:-4400}"

if [ -f .env.rusty-local ]; then
  set -a
  # shellcheck disable=SC1091
  . .env.rusty-local
  set +a
  echo "Loaded .env.rusty-local (react_agent model: ${RUSTY_LLM_MODEL:-harness default})"
fi

command -v cargo >/dev/null 2>&1 || { echo "error: cargo not found (install a Rust toolchain via rustup)" >&2; exit 1; }
command -v python3 >/dev/null 2>&1 || { echo "error: python3 not found" >&2; exit 1; }

echo "Building Rusty Server (rusty-server/examples/server_demo.rs) ..."
cargo build -p rusty-agent-server --example server_demo

# The studio host serves studio/ui/dist; build it when missing or older
# than the newest source file.
if [ ! -f studio/ui/dist/index.html ] || [ -n "$(find studio/ui/src studio/ui/index.html -newer studio/ui/dist/index.html -print -quit 2>/dev/null)" ]; then
  echo "Building Rusty Studio (studio/ui) ..."
  (cd studio/ui && { [ -d node_modules ] || npm ci; } && npm run build)
fi

# Run the built binary directly (not via `cargo run`) so its PID is the
# process to kill on exit. Workspace builds place examples in the root
# target/ directory. The demo reads its bind address from RUSTY_DEMO_ADDR
# (RUSTY_SERVER_PORT above is this script's own knob — without this export
# the server would bind its compiled-in default regardless of the knob).
export RUSTY_DEMO_ADDR="127.0.0.1:$SERVER_PORT"
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
  Rusty Studio  →  http://127.0.0.1:$STUDIO_PORT   (open this one)

Ctrl-C stops both.
EOF

wait
