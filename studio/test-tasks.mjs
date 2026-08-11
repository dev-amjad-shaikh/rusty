#!/usr/bin/env node
/* Node unit tests for the durable-tasks view helpers embedded in
 * studio/index.html. Same harness as test-recorder.mjs: the <script> block
 * is extracted verbatim, the final browser bootstrap (`init();`) is
 * stripped, and the pure helpers are exercised under `vm` — no browser,
 * no dependencies.
 *
 *   node studio/test-tasks.mjs
 */
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";
import vm from "node:vm";

const here = path.dirname(fileURLToPath(import.meta.url));
const html = readFileSync(path.join(here, "index.html"), "utf8");

const match = html.match(/<script>([\s\S]*?)<\/script>/);
if (!match) { console.error("FAIL: no <script> block found in index.html"); process.exit(1); }
const src = match[1].replace(/\ninit\(\);\s*$/, "\n");
if (/\ninit\(\);/.test(src)) { console.error("FAIL: bootstrap init() was not stripped cleanly"); process.exit(1); }

const sandbox = {};
vm.createContext(sandbox);
vm.runInContext(src + `
globalThis.__tasks = {
  taskBadgeHtml, taskIsTerminal, tasksListPath, taskRowHtml, taskDetailHtml,
  tasksErrorHtml, TASK_STATUS_BADGE,
};`, sandbox, { filename: "index.html<script>" });

const T = sandbox.__tasks;

/* -- fixtures: TaskRecord::wire shapes (rusty-server/src/tasks.rs) ------- */

const base = {
  task_id: "019157c4-6f1f-7a3b-8c2d-9e4f5a6b7c8d",
  kind: "send_email",
  payload: { to: "a@b.c", subject: "hi" },
  pool: "default",
  status: "queued",
  attempt: 0,
  max_attempts: 3,
  error_class: null,
  effect: null,
  last_error: null,
  idempotency_key: "order-42",
  result: null,
  receipt: null,
  run_id: null,
  thread_id: null,
  cancel_requested: false,
  deadline: null,
  lease: null,
  next_attempt_at: null,
  created_at: "2026-08-10T09:00:00Z",
  updated_at: "2026-08-10T09:00:00Z",
};
const task = (extra) => ({ ...base, ...extra });

/* -- tiny assert harness (same shape as test-recorder.mjs) --------------- */

let passed = 0, failed = 0;
function check(name, cond, detail) {
  if (cond) { passed++; console.log(`ok   ${name}`); }
  else { failed++; console.log(`FAIL ${name}${detail ? ` — ${detail}` : ""}`); }
}

/* -- badge tone per status ------------------------------------------------ */

check("badge: queued → pending tone",
  T.taskBadgeHtml("queued").includes('class="badge pending"'));
check("badge: leased → running tone (pulses like a live run)",
  T.taskBadgeHtml("leased").includes('class="badge running"'));
check("badge: failed → error tone",
  T.taskBadgeHtml("failed").includes('class="badge error"'));
check("badge: completed → success tone",
  T.taskBadgeHtml("completed").includes('class="badge success"'));
check("badge: dead → error tone (DLQ is a list of failures)",
  T.taskBadgeHtml("dead").includes('class="badge error"'));
check("badge: cancelled → interrupted tone (control flow, not failure)",
  T.taskBadgeHtml("cancelled").includes('class="badge interrupted"'));
check("badge: label is the real status, not the tone name",
  T.taskBadgeHtml("cancelled").includes(">cancelled</span>"));
check("badge: unknown future status falls back to pending",
  T.taskBadgeHtml("hibernating").includes('class="badge pending"'));

/* -- terminality mirrors TaskRecord::is_terminal -------------------------- */

check("terminal: completed", T.taskIsTerminal(task({ status: "completed" })) === true);
check("terminal: dead", T.taskIsTerminal(task({ status: "dead" })) === true);
check("terminal: cancelled", T.taskIsTerminal(task({ status: "cancelled" })) === true);
check("non-terminal: queued", T.taskIsTerminal(task({ status: "queued" })) === false);
check("non-terminal: leased", T.taskIsTerminal(task({ status: "leased" })) === false);
check("non-terminal: failed with a retry scheduled",
  T.taskIsTerminal(task({ status: "failed", next_attempt_at: "2026-08-10T09:05:00Z" })) === false);
check("terminal: failed outright (no retry scheduled)",
  T.taskIsTerminal(task({ status: "failed" })) === true);

/* -- list path ------------------------------------------------------------ */

check("list path: no filter → bare /tasks", T.tasksListPath("") === "/tasks");
check("list path: status filter → query string",
  T.tasksListPath("dead") === "/tasks?status=dead");

/* -- list row -------------------------------------------------------------- */

{
  const html = T.taskRowHtml(task({ status: "failed", attempt: 2, next_attempt_at: "2026-08-10T09:05:00Z" }));
  check("row: kind headline", html.includes('>send_email</span>'));
  check("row: attempt counter", html.includes("attempt 2/3"));
  check("row: retry schedule shown for a retryable failure",
    html.includes("retry at 2026-08-10T09:05:00Z"));
  check("row: pool", html.includes("pool default"));
  check("row: full task id present", html.includes(base.task_id));
}
check("row: no retry schedule for a plain queued task",
  !T.taskRowHtml(base).includes("retry at"));
check("row: kind is HTML-escaped",
  !T.taskRowHtml(task({ kind: "<img src=x onerror=alert(1)>" })).includes("<img"));

/* -- detail card ------------------------------------------------------------ */

{
  const leased = task({
    status: "leased",
    attempt: 1,
    lease: { owner: "worker-7", expires_at: "2026-08-10T09:01:00Z" },
    cancel_requested: true,
    effect: "idempotent",
  });
  const html = T.taskDetailHtml(leased);
  check("detail: envelope fields (idempotency key, pool)",
    html.includes("order-42") && html.includes("default"));
  check("detail: lease owner + expiry while leased",
    html.includes("worker-7") && html.includes("2026-08-10T09:01:00Z"));
  check("detail: cancel_requested note explains the heartbeat hint",
    html.includes("cancellation requested") && html.includes("next"));
  check("detail: declared effect rendered as a badge",
    html.includes("eff-idempotent"));
  check("detail: payload pretty-printed", html.includes("&quot;to&quot;"));
  check("detail: cancel enabled for a non-terminal task",
    !html.includes("disabled"));
}
{
  const dead = task({
    status: "dead",
    attempt: 3,
    error_class: "dependency_failure",
    last_error: "smtp: connection refused",
  });
  const html = T.taskDetailHtml(dead);
  check("detail: DLQ triage shows error class + last error",
    html.includes("dependency_failure") && html.includes("smtp: connection refused"));
  check("detail: cancel disabled with the terminal reason",
    html.includes("disabled") && html.includes("409"));
}
{
  const done = task({
    status: "completed",
    attempt: 1,
    result: { message_id: "m-1" },
    receipt: { provider: "sendgrid", provider_id: "sg-99", idempotency_key: "order-42", task_id: base.task_id },
  });
  const html = T.taskDetailHtml(done);
  check("detail: completed task shows the result", html.includes("message_id"));
  check("detail: effect receipt fields shown",
    html.includes("sendgrid") && html.includes("sg-99"));
}
check("detail: queued task renders no lease section",
  !T.taskDetailHtml(base).includes(">lease</div>"));
check("detail: payload is HTML-escaped",
  !T.taskDetailHtml(task({ payload: { x: "<script>alert(1)</script>" } })).includes("<script>alert"));
check("detail: missing fields render defensively (partial record)",
  T.taskDetailHtml({ task_id: "t-1", kind: "k", status: "queued", payload: {} }).includes("t-1"));

/* -- error notes ------------------------------------------------------------ */

check("error: unavailable task support gives one user-facing recovery direction",
  T.tasksErrorHtml(404, null).includes("Task queue is unavailable on this server") &&
  !T.tasksErrorHtml(404, null).includes("R0."));
check("error: JSON failure → server message verbatim",
  T.tasksErrorHtml(500, { error: "internal", message: "store offline" }).includes("store offline"));

/* ------------------------------------------------------------------------- */

console.log(`\n${passed} passed, ${failed} failed`);
process.exit(failed ? 1 : 0);
