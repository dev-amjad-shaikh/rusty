// Boots Studio with its local runtime in one command.
//
// Studio is the local companion of one Rusty backend. `npm run dev` starts the
// demo server from this repository (reusing one that is already answering on
// 127.0.0.1:8100), waits until it proves itself via /info, then starts Vite.
// Ctrl-C stops both; a backend Studio did not spawn is left running.

import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import path from "node:path";

const uiRoot = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
const repoRoot = path.resolve(uiRoot, "..", "..");
const ORIGIN = "http://127.0.0.1:8100";

async function backendAnswers() {
  try {
    const response = await fetch(`${ORIGIN}/info`, { signal: AbortSignal.timeout(1_500) });
    return response.ok;
  } catch {
    return false;
  }
}

async function waitForBackend() {
  process.stdout.write("waiting for the local runtime on 127.0.0.1:8100 (the first cargo build can take a few minutes)…\n");
  for (;;) {
    if (await backendAnswers()) return;
    await new Promise((resolve) => setTimeout(resolve, 2_000));
  }
}

const children = [];
function shutdown(signal) {
  for (const child of children.splice(0)) {
    if (!child.killed) child.kill(signal);
  }
}
for (const signal of ["SIGINT", "SIGTERM"]) process.on(signal, () => { shutdown(signal); process.exit(130); });

let backend = null;
if (await backendAnswers()) {
  process.stdout.write("local runtime already answering on 127.0.0.1:8100 — reusing it\n");
} else {
  backend = spawn("cargo", ["run", "-p", "rusty-agent-server", "--example", "server_demo"], {
    cwd: repoRoot,
    env: { ...process.env, RUSTC_WRAPPER: process.env.RUSTC_WRAPPER ?? "sccache" },
    stdio: ["ignore", "inherit", "inherit"],
  });
  backend.on("exit", (code) => {
    if (code !== null && code !== 0) {
      process.stderr.write(`rusty-server exited with code ${code}\n`);
      shutdown("SIGTERM");
      process.exit(code);
    }
  });
  children.push(backend);
  await waitForBackend();
  process.stdout.write("local runtime is ready\n");
}

const viteBin = path.join(uiRoot, "node_modules", ".bin", "vite");
const vite = spawn(viteBin, process.argv.slice(2), { cwd: uiRoot, stdio: "inherit" });
children.push(vite);
vite.on("exit", (code, signal) => {
  // Only the backend Studio spawned is stopped; a reused external one stays up.
  if (backend && !backend.killed) backend.kill("SIGTERM");
  if (signal) process.kill(process.pid, signal);
  else process.exit(code ?? 0);
});
