#!/usr/bin/env node
/* Unified Run Session contracts: one thread-bound mission state, truthful
 * execution/decision/proof transitions, exact Recorder and evaluation handoff,
 * async ownership, bounded context, accessible rendering, and responsive UI.
 */
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";
import vm from "node:vm";

const here = path.dirname(fileURLToPath(import.meta.url));
const html = readFileSync(path.join(here, "index.html"), "utf8");
const docs = readFileSync(path.join(here, "..", "docs", "studio.md"), "utf8");
const roadmap = readFileSync(path.join(here, "..", "docs", "studio-experience-roadmap.md"), "utf8");
const match = html.match(/<script>([\s\S]*?)<\/script>/);
if (!match) { console.error("FAIL: no Studio script"); process.exit(1); }
const source = match[1].replace(/\ninit\(\);\s*$/, "\n");

let elements = new Map();
const sandbox = {
  URL, URLSearchParams, TextEncoder, TextDecoder, CSS: { escape: (value) => String(value) },
  setTimeout: () => 1, clearTimeout() {},
  document: { activeElement: null, getElementById: (id) => elements.get(id) || null },
};
vm.createContext(sandbox);
vm.runInContext(source + `
renderMain = () => {};
renderThreads = () => {};
journeyRender = () => {};
registryBindingRender = () => {};
runProofRender = () => {};
qualityRender = () => {};
qualityLibraryRender = () => {};
qualityGateRender = () => {};
qualityReportRender = () => {};
qualityFailureAtlasRender = () => {};
qualityRegressionRender = () => {};
qualityDecisionRender = () => {};
globalThis.__runSession = { store, runSessionRecord, runSessionEnsure, runSessionUpdate,
  runSessionTerminalPhase, runSessionRecorder, runSessionSnapshot, runSessionLaunchBlocked, runSessionBegin, runSessionRender,
  runSessionBackgroundAccepted, runSessionAbandonUncertainty, runSessionAction, threadStageRender, threadStageOpen, threadEvaluateToolRender,
  stopPoll, abortStream, showRunResult, connectionResetWorkspace };
`, sandbox, { filename: "index.html<script>" });
const R = sandbox.__runSession;

let passed = 0, failed = 0;
function check(name, condition, detail = "") {
  if (condition) { passed++; console.log(`ok   ${name}`); }
  else { failed++; console.log(`FAIL ${name}${detail ? ` — ${detail}` : ""}`); }
}
function state(threadId = "thread-1") {
  return {
    view: "thread", selected: threadId, threads: [{ thread_id: threadId, graph: "react_agent" }],
    runSessions: Object.create(null), recorder: null, interruptDecision: null, qualityCase: null,
  };
}
function element(extra = {}) {
  return { textContent: "", innerHTML: "", dataset: {}, disabled: false, focused: 0, scrolled: 0,
    focus() { this.focused += 1; }, scrollIntoView() { this.scrolled += 1; }, ...extra };
}

{
  const s = state();
  s.secretMission = "must-not-enter-session";
  const snapshot = R.runSessionSnapshot(s, "thread-1");
  check("session start: a selected thread begins at one honest Prepare decision",
    snapshot.phase === "ready" && snapshot.steps[0].state === "active" && snapshot.steps.slice(1).every((step) => step.state === "") && snapshot.action.key === "input");
  check("session privacy: the derived state carries no mission input or event payload field",
    !JSON.stringify(snapshot).includes("must-not-enter-session") && !Object.prototype.hasOwnProperty.call(snapshot, "payload"));
}

{
  const buttons = ["run", "trace", "evaluate"].map((key) => ({ key, disabled: false, selected: "false",
    getAttribute(name) { return name === "data-thread-stage" ? this.key : null; },
    setAttribute(name, value) { if (name === "aria-selected") this.selected = value; } }));
  const nav = { querySelectorAll: () => buttons };
  const run = element(), recorder = element(), trace = element(), evaluate = element(), current = element(), history = element(), select = element({ value: "" });
  const qualityPanels = new Map(["quality-foundry", "quality-dataset", "quality-gate", "quality-report", "quality-failure-atlas", "quality-regression", "quality-decision", "quality-run-compare"].map((id) => [id, element()]));
  elements = new Map([["thread-stage-nav", nav], ["thread-run-panel", run], ["flight-recorder-card", recorder],
    ["thread-trace-core", trace], ["thread-evaluate-core", evaluate], ["thread-state-card", current], ["thread-history-card", history],
    ["sel-evaluate-tool", select], ...qualityPanels]);
  Object.assign(R.store, state(), { threadStage: "run", qualityTool: "case" });
  R.threadStageRender();
  const runOnly = !run.hidden && recorder.hidden && current.hidden && buttons[2].disabled;
  R.store.recorder = { runId: "run-1", exactEnvelope: true, complete: true, error: null,
    events: [{ run_id: "run-1", thread_id: "thread-1" }] };
  R.threadStageOpen("evaluate", false);
  const evaluationOnly = run.hidden && !recorder.hidden && trace.hidden && !evaluate.hidden && !qualityPanels.get("quality-foundry").hidden &&
    [...qualityPanels.entries()].filter(([id]) => id !== "quality-foundry").every(([, panel]) => panel.hidden) && !buttons[2].disabled;
  check("thread focus: Run, Trace, and Evaluate expose one workspace at a time and gate evaluation on exact evidence", runOnly && evaluationOnly);
  buttons[2].focused = 0;
  buttons[2].focus = function () { this.focused += 1; sandbox.document.activeElement = this; };
  buttons[1].focused = 0;
  buttons[1].focus = function () { this.focused += 1; sandbox.document.activeElement = this; };
  sandbox.document.activeElement = buttons[2];
  R.store.recorder.error = "refresh failed";
  R.threadStageRender();
  check("thread focus: losing exact evidence leaves Evaluate for Trace with one enabled tab stop",
    R.store.threadStage === "trace" && buttons[1].selected === "true" && buttons[1].tabIndex === 0 && buttons[1].focused === 1 &&
    buttons[2].disabled && evaluate.hidden && !trace.hidden);
}

{
  const s = state();
  check("session launch ownership: one submission blocks another launch on the same thread",
    R.runSessionBegin("thread-1", "background", s) && !R.runSessionBegin("thread-1", "wait", s) && R.runSessionLaunchBlocked("thread-1", s));
  R.runSessionUpdate("thread-1", { phase: "complete", runId: "run-0", status: "success" }, s);
  check("session launch ownership: a terminal run releases the thread for its next mission",
    R.runSessionBegin("thread-1", "stream", s));
}

{
  const s = state();
  R.runSessionBackgroundAccepted("thread-1", "run-away", "pending", false, s);
  const snapshot = R.runSessionSnapshot(s, "thread-1");
  check("session background ownership: leaving before the receipt preserves the run as recoverable uncertainty",
    snapshot.phase === "uncertain" && snapshot.runId === "run-away" && snapshot.action.key === "trace" && R.runSessionLaunchBlocked("thread-1", s));
}

{
  const s = state();
  R.runSessionUpdate("thread-1", { phase: "submitting", mode: "stream", status: "running" }, s);
  let snapshot = R.runSessionSnapshot(s, "thread-1");
  check("session admission: submitting makes Execute current without inventing a run identity",
    snapshot.phase === "submitting" && snapshot.steps[1].state === "active" && snapshot.runId === "" && snapshot.action.key === "activity");
  R.runSessionUpdate("thread-1", { phase: "running", runId: "run-1", status: "running" }, s);
  snapshot = R.runSessionSnapshot(s, "thread-1");
  check("session execution: accepted live work binds one run and keeps Execute current",
    snapshot.runId === "run-1" && snapshot.visibleStatus === "running" && snapshot.steps[1].detail === "live");
}

{
  const s = state();
  R.runSessionUpdate("thread-1", { phase: "complete", runId: "run-1", status: "success" }, s);
  let snapshot = R.runSessionSnapshot(s, "thread-1");
  check("session terminal: a known outcome requests exact trace instead of claiming proof",
    !snapshot.exact && snapshot.steps[3].state === "active" && snapshot.action.key === "trace");
  s.recorder = { runId: "run-1", exactEnvelope: true, complete: true, error: null,
    events: [{ run_id: "run-1", thread_id: "thread-1" }] };
  snapshot = R.runSessionSnapshot(s, "thread-1");
  check("session proof: only a complete exact same-thread journal completes Prove and unlocks evaluation",
    snapshot.exact && snapshot.steps[3].state === "complete" && snapshot.action.key === "evaluate");
  R.runSessionUpdate("thread-1", { phase: "running", runId: "run-2", status: "running" }, s);
  snapshot = R.runSessionSnapshot(s, "thread-1");
  check("session proof: an older same-thread journal cannot complete a newer retained run",
    !snapshot.exact && snapshot.runId === "run-2" && snapshot.phase === "running" && snapshot.action.key === "activity" &&
    snapshot.steps[3].state !== "complete");
  R.runSessionUpdate("thread-1", { phase: "submitting", runId: "", status: "" }, s);
  snapshot = R.runSessionSnapshot(s, "thread-1");
  check("session proof: a fresh submission without an identity never inherits the previous journal",
    !snapshot.exact && snapshot.runId === "" && snapshot.phase === "submitting" && snapshot.action.key === "activity");
  R.runSessionUpdate("thread-1", { phase: "complete", runId: "run-1", status: "success" }, s);
  s.recorder.events[0].thread_id = "thread-crossed";
  snapshot = R.runSessionSnapshot(s, "thread-1");
  check("session isolation: crossed Recorder evidence removes proof and evaluation handoff",
    !snapshot.exact && snapshot.action.key === "trace");
}

{
  const s = state();
  R.runSessionUpdate("thread-1", { phase: "running", runId: "run-1", status: "running" }, s);
  s.interruptDecision = { threadId: "thread-1", runId: "run-1", verified: true };
  const snapshot = R.runSessionSnapshot(s, "thread-1");
  check("session decision: an owned interrupt supersedes generic running state",
    snapshot.phase === "interrupted" && snapshot.decision && snapshot.steps[2].state === "attention" && snapshot.action.key === "decision");
}

{
  const s = state();
  R.runSessionUpdate("thread-1", { phase: "uncertain", runId: "run-1", message: "Known run; terminal response lost." }, s);
  const known = R.runSessionSnapshot(s, "thread-1");
  R.runSessionUpdate("thread-2", { phase: "uncertain", message: "Identity lost; do not repeat." }, s);
  const unknown = R.runSessionSnapshot(s, "thread-2");
  check("session retry safety: a known uncertain run directs exact evidence while an unknown identity never offers a new launch",
    known.action.key === "trace" && known.action.copy.includes("terminal response lost") && unknown.action.key === "activity" && unknown.action.copy.includes("do not repeat"));
}

{
  const s = state("thread-a");
  s.threads.push({ thread_id: "thread-b", graph: "workflow" });
  R.runSessionUpdate("thread-a", { phase: "running", runId: "run-a", status: "running" }, s);
  R.runSessionUpdate("thread-b", { phase: "complete", runId: "run-b", status: "success" }, s);
  check("session ownership: two local threads retain independent bounded mission progress",
    R.runSessionSnapshot(s, "thread-a").runId === "run-a" && R.runSessionSnapshot(s, "thread-b").runId === "run-b");
}

{
  const spine = element(), status = element(), button = element(), abandon = element(), title = element(), copy = element();
  const background = element(), wait = element(), stream = element();
  elements = new Map([
    ["run-session-spine", spine], ["run-session-status", status], ["btn-run-session-next", button], ["btn-run-session-abandon", abandon],
    ["run-session-now-title", title], ["run-session-now-copy", copy], ["btn-run", background], ["btn-run-wait", wait], ["btn-run-stream", stream],
  ]);
  Object.assign(R.store, state());
  R.runSessionUpdate("thread-1", { phase: "running", runId: "run-live", status: "running" });
  R.runSessionRender();
  check("session rendering: every launch mode is disabled while the thread owns live work",
    background.disabled && wait.disabled && stream.disabled);
  R.runSessionUpdate("thread-1", { phase: "uncertain", runId: "run-live", status: "pending" });
  R.runSessionRender();
  check("session rendering: uncertain admission stays locked and exposes one deliberate recovery control",
    background.disabled && wait.disabled && stream.disabled && abandon.hidden === false);
  R.runSessionUpdate("thread-1", { phase: "failed", runId: "run-1", status: "error" });
  const snapshot = R.runSessionRender();
  check("session rendering: the stable live status and next-safe action reflect one derived snapshot",
    snapshot.phase === "failed" && status.dataset.state === "failed" && button.dataset.runSessionAction === "trace" && title.textContent === "Inspect what failed");
  check("session rendering: progress uses a semantic list and bounded identifier-only copy",
    spine.innerHTML.includes('role="listitem"') && spine.innerHTML.includes('data-run-session-step="prove"') && !spine.innerHTML.includes("secret"));
}

{
  const input = element(), feed = element(), decision = element(), interruptCard = element(), quality = element(), foundry = element();
  elements = new Map([
    ["inp-payload", input], ["feed", feed], ["interrupt-title", decision], ["interrupt-card", interruptCard],
    ["quality-foundry-title", quality], ["quality-foundry", foundry],
  ]);
  Object.assign(R.store, state());
  check("session handoff: Review mission moves focus to the exact JSON input", await R.runSessionAction("input") && input.focused === 1);
  check("session handoff: Follow activity moves focus and scroll to the stable live region", await R.runSessionAction("activity") && feed.focused === 1 && feed.scrolled === 1);
  R.store.interruptDecision = { threadId: "thread-1", runId: "run-1" };
  check("session handoff: Review pause moves focus to the owned decision surface", await R.runSessionAction("decision") && decision.focused === 1 && interruptCard.scrolled === 1);
  R.store.interruptDecision = null;
  R.store.recorder = { runId: "run-1", exactEnvelope: true, complete: true, error: null, events: [{ run_id: "run-1", thread_id: "thread-1" }] };
  check("session handoff: exact journal opens evaluation without rerendering away its evidence", await R.runSessionAction("evaluate") && R.store.journeyPhase === "evaluate" && quality.focused === 1 && foundry.scrolled === 1);
}

{
  const flight = element(), card = element();
  elements = new Map([["flight-recorder-title", flight], ["flight-recorder-card", card]]);
  Object.assign(R.store, state());
  R.runSessionUpdate("thread-1", { phase: "complete", runId: "run-deferred", status: "success" });
  sandbox.__resolveTrace = null;
  vm.runInContext(`recLoad = async () => { await new Promise((resolve) => { globalThis.__resolveTrace = resolve; }); store.recLoadRequest += 1; };`, sandbox);
  const pending = R.runSessionAction("trace");
  await Promise.resolve();
  R.store.view = "home";
  sandbox.__resolveTrace();
  const opened = await pending;
  check("session async ownership: a deferred trace load cannot focus after the user leaves the thread workspace",
    !opened && flight.focused === 0 && card.scrolled === 0);
}

{
  Object.assign(R.store, state());
  R.runSessionUpdate("thread-1", { phase: "running", mode: "background", runId: "run-poll", status: "running" });
  R.store.pollSession = { threadId: "thread-1", runId: "run-poll" };
  R.stopPoll();
  const poll = R.runSessionSnapshot(R.store, "thread-1");
  R.runSessionUpdate("thread-1", { phase: "running", mode: "stream", runId: "run-stream", status: "running" });
  R.store.streamSession = { threadId: "thread-1", runId: "run-stream" };
  R.store.streamAbort = { abort() {} };
  R.abortStream();
  const stream = R.runSessionSnapshot(R.store, "thread-1");
  check("session ownership loss: stopping poll or live transport preserves known identity and marks the outcome uncertain",
    poll.phase === "uncertain" && poll.runId === "run-poll" && stream.phase === "uncertain" && stream.runId === "run-stream");
}

check("session lifecycle: changing connection clears every page-memory run session",
  source.includes("store.runSessions = Object.create(null);"));

check("markup: Run Session makes live execution primary while retaining advanced background and wait modes",
  html.includes('id="run-session"') && html.includes('id="btn-run-stream" type="button">Start live run') && html.includes("Other execution modes and delivery controls") && html.includes('id="btn-run-session-next"'));
check("shell: navigation presents Agents, Work, and Operations while specialist tools stay secondary",
  html.includes('class="studio-nav-primary" aria-label="Primary destinations"') &&
  html.includes('id="btn-agents-open"') && html.includes('id="btn-home-open"') && html.includes('id="btn-operations-open"') &&
  html.includes('<details class="studio-tool-drawer">') && html.includes('<b>Task queue</b></button>') &&
  !html.includes("The R0.6 task queue is tenant-wide") && !html.includes("no list-threads endpoint"));
check("shell: primary chrome uses task language instead of implementation and privacy narration",
  html.includes('id="btn-copy-view-link" type="button" aria-describedby="view-link-status">Copy link</button>') &&
  html.includes('<small class="sr-only" id="view-link-status">Copy a link to this workspace</small>') &&
  !html.includes("Workspace only · no server address, key, prompt, or payload") &&
  !html.includes("Connection hub · ignition desk"));
check("shell: quick starts present human behaviors and recent work instead of graph channels and raw IDs",
  source.includes('const graphLabel = agentGraphLabel(g.name)') && source.includes('btn.textContent = "Start"') &&
  source.includes('const graphLabel = agentGraphLabel(t.graph)') && source.includes('escapeHtml(shortId(t.thread_id))') &&
  !source.includes('`<div class="channels">channels:'));
check("shell: workspace navigation exposes one current destination without adding a second router",
  source.includes("function studioNavRender()") && source.includes('button.setAttribute("aria-current", "page")') && source.includes("studioNavRender();"));
check("progressive disclosure: implementation boundaries stay available without occupying the primary task flow",
  html.includes('<summary>Execution and data details</summary>') && html.includes('<summary>Evidence details</summary>') &&
  html.includes('<details class="studio-mission" id="studio-mission">'));
check("workspace focus: the thread shows Run, Trace, or Evaluate and one evaluation tool at a time",
  html.includes('id="thread-stage-nav" role="tablist" aria-label="Current task workspace"') && html.includes('id="sel-evaluate-tool"') &&
  source.includes("function threadStageRender") && source.includes("function threadEvaluateToolRender") &&
  docs.includes("shows only one primary") && roadmap.includes("never all three at"));
check("integration: verified non-interrupt streams render the adjacent latest outcome",
  source.includes('endData.status !== "interrupted" && !reportOutcome) showRunResult(endData)'));
check("markup: the four real runtime stations are rendered through one labelled semantic progress list",
  html.includes('id="run-session-spine" role="list" aria-label="Run session progress"') && source.includes('{ key: "prepare"') && source.includes('{ key: "prove"'));
check("accessibility: stable status, mission input description, next action, and focusable live activity are present",
  html.includes('id="run-session-status" role="status" aria-live="polite" aria-atomic="true"') && html.includes('aria-describedby="run-session-input-boundary"') && html.includes('id="feed" tabindex="-1"'));
check("accessibility: workspace tabs have owned panels, roving focus, and keyboard navigation",
  html.includes('role="tab" aria-controls="thread-run-panel"') && html.includes('role="tabpanel" aria-labelledby="thread-stage-run"') &&
  source.includes("function threadStageKeyboard") && source.includes('addEventListener("keydown", threadStageKeyboard)'));
check("responsive: cockpit, progress spine, live evidence, launch modes, and mobile actions collapse deliberately",
  html.includes(".run-session-grid { grid-template-columns: 1fr; }") && html.includes(".run-session-live { grid-template-columns: 1fr; gap: 0; }") &&
  html.includes(".run-session-spine { grid-template-columns: repeat(2,minmax(0,1fr));") && html.includes(".run-session-advanced .row { align-items: stretch; flex-direction: column; }") &&
  html.includes(".thread-stage-nav { display: flex; width: 100%; }") && html.includes(".thread-evaluate-nav { align-items: stretch; flex-direction: column; }"));
check("documentation: Run Session states orchestration, privacy, retry, interruption, exact trace, and evaluation boundaries",
  docs.includes("Unified Run Session") && docs.includes("does not create a second execution authority") && docs.includes("payloads or event bodies") &&
  roadmap.includes("unified **Run Session**") && roadmap.includes("Prepare → Execute → Resolve → Prove"));

console.log(`\n${passed} passed, ${failed} failed`);
if (failed) process.exit(1);
