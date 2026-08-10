#!/usr/bin/env node
/* Dependency-free contract tests for Studio's human decision boundary. */
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";
import vm from "node:vm";

const here = path.dirname(fileURLToPath(import.meta.url));
const html = readFileSync(path.join(here, "index.html"), "utf8");
const match = html.match(/<script>([\s\S]*?)<\/script>/);
if (!match) { console.error("FAIL: no script block"); process.exit(1); }
const src = match[1].replace(/\ninit\(\);\s*$/, "\n");
if (/\ninit\(\);/.test(src)) { console.error("FAIL: bootstrap was not stripped"); process.exit(1); }

const sandbox = { TextEncoder, TextDecoder };
vm.createContext(sandbox);
vm.runInContext(src + `
globalThis.__interrupt = {
  INTERRUPT_RESPONSE_LIMIT, INTERRUPT_PAYLOAD_LIMIT,
  interruptText, interruptQuestion, interruptApprovalLike, interruptParseResponse,
  interruptResponsePreview, interruptDraft, interruptPayloadText, interruptRenderHtml,
  interruptJsonEqual, interruptBoundaryFromRun, interruptResumePayload,
  interruptFailureUncertain, runOperationResult,
};`, sandbox, { filename: "index.html<script>" });

const I = sandbox.__interrupt;
let passed = 0, failed = 0;
function check(name, condition, detail = "") {
  if (condition) { passed++; console.log(`ok   ${name}`); }
  else { failed++; console.log(`FAIL ${name}${detail ? ` — ${detail}` : ""}`); }
}
function eq(name, got, want) {
  check(name, JSON.stringify(got) === JSON.stringify(want), `got ${JSON.stringify(got)}, want ${JSON.stringify(want)}`);
}

const approvalPayload = { kind: "approval", question: "Approve the external ticket update?", ticket: "OPS-42" };
const approval = I.interruptDraft(approvalPayload, {
  threadId: "thread-42", runId: "run-42", checkpointId: "checkpoint-9", verified: true, returnFocus: "btn-run-wait",
});

check("classification: explicit approval discriminator exposes the bounded convenience response",
  I.interruptApprovalLike(approvalPayload) && approval.approval && approval.mode === "approve");
check("classification: arbitrary interrupts never invent approval semantics",
  !I.interruptApprovalLike({ question: "Provide the missing account number" }) &&
  !I.interruptApprovalLike({ question: "This request was not approved—explain why" }) &&
  !I.interruptApprovalLike({ question: "Approve this?" }) &&
  I.interruptDraft({ question: "Approve this?" }).mode === "json");
check("classification: structured approval and consent kinds are recognized",
  I.interruptApprovalLike({ kind: "approval" }) && I.interruptApprovalLike({ type: "consent" }) &&
  I.interruptApprovalLike({ response_schema: { type: "object", properties: { approved: { type: "boolean" } }, required: ["approved"] } }));
check("question: supported prompt fields are bounded and plain fallback remains useful",
  I.interruptQuestion({ prompt: "Choose a region" }) === "Choose a region" &&
  I.interruptQuestion({ question: "x".repeat(900) }).length === 600 &&
  I.interruptQuestion("pause").includes("explicit response"));

eq("response: approve sends the server integration's exact object", I.interruptParseResponse("approve", "").value, { approved: true });
eq("response: deny changes only the reviewed approval boolean", I.interruptParseResponse("deny", "").value, { approved: false });
eq("response: structured custom JSON preserves arrays and nested values",
  I.interruptParseResponse("json", '{"choice":"later","limits":[1,2]}').value,
  { choice: "later", limits: [1, 2] });
check("response: exact-string mode preserves bytes and never absorbs malformed JSON",
  I.interruptParseResponse("string", "  exact answer  ").value === "  exact answer  " &&
  !I.interruptParseResponse("json", "{broken}").valid);
check("response: JSON null is deliberate while an empty JSON editor is invalid",
  I.interruptParseResponse("json", "null").valid && I.interruptParseResponse("json", "null").value === null &&
  !I.interruptParseResponse("json", "").valid && I.interruptParseResponse("string", "").valid);
check("response: lossy JSON numbers and oversized values fail closed",
  !I.interruptParseResponse("json", '{"unsafe":9007199254740993}').valid &&
  !I.interruptParseResponse("string", "x".repeat(I.INTERRUPT_RESPONSE_LIMIT + 1)).valid);
check("response: preview names the wire type and remains bounded",
  I.interruptResponsePreview(I.interruptParseResponse("json", '{"approved":true}')).startsWith("JSON value\n") &&
  I.interruptResponsePreview({ valid: true, value: "x".repeat(5000), kind: "exact string" }).length < 2050);

check("draft: evidence identity and focus origin are frozen for this selected thread",
  approval.threadId === "thread-42" && approval.runId === "run-42" &&
  approval.checkpointId === "checkpoint-9" && approval.verified && approval.returnFocus === "btn-run-wait");

{
  const terminal = { run_id: "run-42", thread_id: "thread-42", status: "interrupted",
    checkpoint_id: "checkpoint-9", interrupt: { question: "Approve?", nested: { b: 2, a: 1 } } };
  const evidence = I.interruptBoundaryFromRun(terminal, {
    threadId: "thread-42", runId: "run-42", interrupt: { nested: { a: 1, b: 2 }, question: "Approve?" },
  });
  check("boundary: terminal record binds exact run, thread, request, and checkpoint evidence",
    evidence.verified && evidence.checkpointId === "checkpoint-9" && I.interruptJsonEqual(terminal.interrupt, { nested: { a: 1, b: 2 }, question: "Approve?" }));
  check("boundary: stale run, moved thread, changed request, and missing checkpoint all fail closed",
    !I.interruptBoundaryFromRun({ ...terminal, run_id: "other" }, { runId: "run-42" }).verified &&
    !I.interruptBoundaryFromRun({ ...terminal, thread_id: "other" }, { threadId: "thread-42" }).verified &&
    !I.interruptBoundaryFromRun(terminal, { interrupt: { question: "Changed" } }).verified &&
    !I.interruptBoundaryFromRun({ ...terminal, checkpoint_id: "" }).verified);
  eq("resume: reviewed value is pinned to the frozen suspension checkpoint",
    I.interruptResumePayload({ verified: true, checkpointId: "checkpoint-9" }, { approved: true }),
    { command: { resume: { approved: true } }, checkpoint: { checkpoint_id: "checkpoint-9" } });
  check("resume: unverified evidence cannot produce a wire payload",
    I.interruptResumePayload({ verified: false, checkpointId: "checkpoint-9" }, true) === null);
}

check("retry: deterministic client rejection stays retryable while transport and server uncertainty lock",
  !I.interruptFailureUncertain({ status: 400 }) && !I.interruptFailureUncertain({ status: 404 }) &&
  !I.interruptFailureUncertain({ status: 409 }) && !I.interruptFailureUncertain({ status: 422 }) &&
  !I.interruptFailureUncertain({ status: 503, body: { error: "shutting_down" } }) &&
  I.interruptFailureUncertain({ status: 0 }) && I.interruptFailureUncertain({ status: 408 }) &&
  I.interruptFailureUncertain({ status: 429 }) && I.interruptFailureUncertain({ status: 503, body: { error: "internal_error" } }));
check("payload: hostile content is bounded before entering the evidence view",
  I.interruptPayloadText({ value: "x".repeat(20000) }).length < I.INTERRUPT_PAYLOAD_LIMIT + 200 &&
  I.interruptPayloadText({ value: "x".repeat(20000) }).includes("truncated"));

{
  const rendered = I.interruptRenderHtml({ ...approval });
  check("render: decision gate exposes request, suspension, response, and re-execution consequence",
    rendered.includes('aria-label="Interrupt and resume path"') && rendered.includes("Suspension checkpoint") &&
    rendered.includes("active step will re-execute") && rendered.includes("active siblings execute again"));
  check("render: approval choices and exact outgoing value are perceivable before resume",
    rendered.includes('data-interrupt-mode="approve"') && rendered.includes('data-interrupt-mode="deny"') &&
    rendered.includes("Value Rusty will receive as command.resume") && rendered.includes('&quot;approved&quot;: true'));
  check("render: transport actions say what happens and the panel is labelled",
    rendered.includes("Resume with live events") && rendered.includes("Resume and wait") &&
    rendered.includes('id="interrupt-title" tabindex="-1"'));
  const hostile = I.interruptRenderHtml({ ...approval, question: "<img src=x onerror=1>", payload: { html: "<script>bad()</script>" } });
  check("render: request copy and raw payload are escaped", !hostile.includes("<img") && !hostile.includes("<script>"));
}

{
  const custom = I.interruptDraft({ question: "Provide account" }, {
    threadId: "thread-1", runId: "run-1", checkpointId: "cp-1", verified: true,
  });
  const rendered = I.interruptRenderHtml(custom);
  check("render: generic interrupts offer explicit JSON and string shapes instead of false approve/deny semantics",
    rendered.includes('data-interrupt-mode="json"') && rendered.includes('data-interrupt-mode="string"') &&
    !rendered.includes('data-interrupt-mode="approve"'));
  const invalid = I.interruptRenderHtml({ ...custom, error: "Review response", fieldError: "Enter a response" });
  check("accessibility: custom validation is programmatically connected and assertive",
    invalid.includes('aria-invalid="true" aria-describedby="interrupt-custom-error"') &&
    invalid.includes('id="interrupt-error" role="alert" tabindex="-1"'));
  const locked = I.interruptRenderHtml({ ...custom, locked: true, error: "Receipt unknown" });
  check("uncertain outcome: reviewed decision locks without a second resume action",
    locked.includes("Refresh state and history") && !locked.includes("Resume and wait") &&
    locked.includes("Resume outcome is uncertain"));
}

{
  const unverified = I.interruptDraft({ question: "Provide account" }, {
    threadId: "thread-1", runId: "run-1", boundaryError: "Evidence mismatch",
  });
  const rendered = I.interruptRenderHtml(unverified);
  check("boundary: unverified evidence disables response controls and offers corroboration",
    rendered.includes("Suspension boundary not verified") && rendered.includes("Refresh suspension boundary") &&
    !rendered.includes("Resume and wait") && rendered.includes("disabled"));
  const noRun = I.interruptRenderHtml({ ...unverified, runId: "", boundaryError: "Missing run identity" });
  check("boundary: a stream without run identity exposes no dead corroboration action",
    noRun.includes("supplied no run identity") && !noRun.includes("data-interrupt-refresh-boundary"));
}

check("markup: the workspace is a labelled hidden region, not an unlabeled raw textarea",
  html.includes('class="card interrupt-decision" id="interrupt-card" aria-labelledby="interrupt-title" hidden') &&
  !html.includes("Interrupted — human in the loop") && !html.includes("Resume value (JSON if it parses"));
check("integration: wait receipts validate terminal evidence and stream receipts corroborate metadata run identity",
  html.includes("interruptBoundaryFromRun(res") &&
  html.includes("const boundaryDraft = shown ? store.interruptDecision : null") &&
  html.includes("boundaryDraft.submitting = true") &&
  html.includes('apiForConnection(connection, "GET", `/runs/${encodeURIComponent(runId)}`)') &&
  !html.includes("runId: endData.run_id") && !html.includes("checkpointId: endData.checkpoint_id"));
check("integration: exact resume payload carries the frozen checkpoint and run controls cannot replace an open decision",
  html.includes("interruptResumePayload(draft, response.value)") &&
  html.includes('checkpoint: { checkpoint_id: draft.checkpointId }') &&
  html.includes("runWait(payload, true, false)") && html.includes("runStream(payload, true, false)") &&
  html.includes('"btn-run-stream", "btn-replay"') && html.includes("!cp || Boolean(store.interruptDecision)") &&
  !html.includes('event.target.closest("[data-interrupt-close]")'));
check("stream: a received terminal frame wins over a later reader close and provisional evidence owns corroboration",
  html.includes("if (!endData) {") && !html.includes("if (streamError || !endData)") &&
  html.includes("store.interruptDecision !== boundaryDraft"));
check("integration: resume keeps the panel until a terminal result and locks an uncertain outcome",
  html.includes("if (store.interruptDecision !== draft) return;") &&
  html.includes("Studio will not submit it twice") && html.includes("draft.locked = interruptFailureUncertain") &&
  html.includes("const outcome = wait") && html.includes("showRunResult(outcome.result"));
check("isolation: connection and thread switches clear session-only decisions",
  html.includes("store.interruptDecision = null") && html.includes("hideInterrupt(false)") &&
  html.includes("currentThread()?.thread_id !== draft.threadId"));
check("focus: open, validation, uncertainty, thread transitions, and success have stable targets",
  html.includes('$("interrupt-title")?.focus') && html.includes('$("inp-resume")?.focus') &&
  html.includes('$("interrupt-error")?.focus') && html.includes('$("run-result")?.focus') &&
  html.includes("draft.returnFocus") && html.includes('querySelector(`[data-interrupt-mode="${draft.mode}"]`)'));
check("refresh: suspension corroboration announces busy state and cannot be submitted twice",
  html.includes('${draft.submitting ? "disabled" : ""}>${draft.submitting ? "Corroborating…"') &&
  html.includes('$("interrupt-title")?.focus({ preventScroll: true });'));
check("privacy: only the browser decision draft is session-only and no local persistence is introduced",
  !html.includes("ags:interrupt") && !html.includes("interruptLocalStorage") &&
  html.includes("browser review is session-only"));
check("responsive: evidence gate and response workspace collapse deliberately",
  html.includes(".interrupt-body { grid-template-columns: 1fr; }") &&
  html.includes(".interrupt-path { grid-template-columns: 1fr; }") &&
  html.includes(".interrupt-choice-set { grid-template-columns: 1fr; }"));
check("runtime: delegated controls preserve keyboard submit semantics",
  html.includes('$("interrupt-card").addEventListener("submit"') &&
  html.includes('event.submitter?.getAttribute("data-interrupt-resume")'));

console.log(`\n${passed} passed, ${failed} failed`);
if (failed) process.exit(1);
