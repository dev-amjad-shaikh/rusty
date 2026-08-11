#!/usr/bin/env node
/* Focused contracts for Studio's persistent evidence thread. The suite proves
 * cross-workspace identity coherence, honest evidence states, safe routing
 * prerequisites, keyboard behavior, privacy, and responsive markup.
 */
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";
import vm from "node:vm";

const here = path.dirname(fileURLToPath(import.meta.url));
const html = readFileSync(path.join(here, "index.html"), "utf8");
const match = html.match(/<script>([\s\S]*?)<\/script>/);
if (!match) { console.error("FAIL: no Studio script"); process.exit(1); }
const source = match[1].replace(/\ninit\(\);\s*$/, "\n");

const localData = new Map();
let elementMap = new Map();
const sandbox = {
  URL, URLSearchParams, TextEncoder, TextDecoder, CSS: { escape: (value) => String(value) },
  localStorage: {
    getItem: (key) => localData.has(key) ? localData.get(key) : null,
    setItem: (key, value) => localData.set(key, String(value)),
  },
  document: { activeElement: null, getElementById: (id) => elementMap.get(id) || null },
};
vm.createContext(sandbox);
vm.runInContext(source + `
globalThis.__journey = { store, JOURNEY_STAGE_KEYS, journeyRunEntries, journeyRecorderContext,
  journeyQualityEvidence, journeyActiveStage, journeySnapshot, journeyEvidenceLabel,
  journeyHtml, journeyContextHtml, journeyRender, journeyKeyboard, journeyOpenStage };
`, sandbox, { filename: "index.html<script>" });
const J = sandbox.__journey;

let passed = 0, failed = 0;
function check(name, condition, detail = "") {
  if (condition) { passed++; console.log(`ok   ${name}`); }
  else { failed++; console.log(`FAIL ${name}${detail ? ` — ${detail}` : ""}`); }
}
function stage(snapshot, key) { return snapshot.stages.find((item) => item.key === key); }
function connectedState() {
  return {
    conn: { baseUrl: "http://tenant-a", apiKey: "secret-never-render" },
    info: { service: "rusty-agent-server", version: "0.12.0", graphs: [{ name: "react_agent" }] },
    view: "home", selected: null, threads: [], recorder: null,
    agents: { list: [], selected: null }, agentsUnsupported: false,
    agentRuns: Object.create(null), agentRunHistory: Object.create(null),
    memory: null, memoryUnsupported: false, learn: null, learnUnsupported: false,
    schedules: null, schedulesUnsupported: false, automations: null, automationsUnsupported: false,
    tasks: null, tasksUnsupported: false,
  };
}

{
  const snapshot = J.journeySnapshot({ view: "home" });
  check("journey onboarding: disconnected workspaces lead with connection and expose no false stage",
    snapshot.next.key === "connect" && snapshot.stages.every((item) => !item.available));
  check("journey vocabulary: the lifecycle uses one six-stage product language",
    snapshot.stages.map((item) => item.label).join(" → ") === "Shape → Run → Inspect → Evaluate → Govern → Operate");
}

{
  const state = connectedState();
  state.view = "agents";
  state.agents = { selected: "agent-1", list: [{ assistant_id: "agent-1", name: "Research lead", graph: "react_agent", active_version_id: "av-1" }] };
  const snapshot = J.journeySnapshot(state);
  check("journey shape: selected durable agent and active version become the exact context chain",
    snapshot.agentId === "agent-1" && snapshot.versionId === "av-1" && snapshot.title === "Research lead" && stage(snapshot, "shape").complete);
  check("journey progression: a shaped runnable agent advances to Run without fabricating evidence",
    snapshot.next.key === "run" && stage(snapshot, "run").available && !stage(snapshot, "run").complete && snapshot.active === "shape");
}

{
  const state = connectedState();
  state.view = "thread"; state.selected = "thread-1";
  state.threads = [{ thread_id: "thread-1", graph: "react_agent" }];
  state.agents = { selected: "agent-other", list: [
    { assistant_id: "agent-other", name: "Other", graph: "react_agent" },
    { assistant_id: "agent-owner", name: "Owner", graph: "react_agent", active_version_id: "av-owner" },
  ] };
  state.agentRunHistory["agent-owner"] = [{ run_id: "run-1", thread_id: "thread-1", graph: "react_agent", status: "error", started_at: "2026-08-10T10:00:00Z" }];
  const snapshot = J.journeySnapshot(state);
  check("journey coherence: thread evidence resolves its run owner instead of carrying an unrelated selected agent",
    snapshot.agentId === "agent-owner" && snapshot.versionId === "av-owner" && snapshot.runId === "run-1" && snapshot.threadId === "thread-1");
  check("journey truth: browser-recalled failure makes Run attention and Inspect available, not proved",
    stage(snapshot, "run").complete && stage(snapshot, "run").attention && stage(snapshot, "inspect").available && !stage(snapshot, "inspect").complete);
}

{
  const state = connectedState();
  state.view = "thread"; state.selected = "thread-empty";
  state.threads = [{ thread_id: "thread-empty", graph: "react_agent" }];
  state.agents = { selected: "agent-other", list: [{ assistant_id: "agent-other", graph: "react_agent" }] };
  state.agentRunHistory["agent-other"] = [{ run_id: "run-other", thread_id: "thread-other", graph: "react_agent", status: "success" }];
  const snapshot = J.journeySnapshot(state);
  check("journey isolation: a selected thread never inherits the selected agent's unrelated run",
    snapshot.threadId === "thread-empty" && !snapshot.agentId && !snapshot.runId && !stage(snapshot, "inspect").available);
  state.recorder = { runId: "run-exact", exactEnvelope: true, complete: true, error: null,
    events: [{ run_id: "run-exact", thread_id: "thread-empty" }] };
  const exact = J.journeySnapshot(state);
  check("journey isolation: exact Recorder identity wins even when browser history has no matching run",
    exact.threadId === "thread-empty" && exact.runId === "run-exact" && !exact.agentId && exact.exactRecorder && stage(exact, "inspect").complete);
}

{
  const state = connectedState();
  state.view = "thread"; state.selected = "thread-1"; state.journeyPhase = "evaluate";
  state.threads = [{ thread_id: "thread-1", graph: "react_agent" }];
  state.agentRunHistory["agent-1"] = [{ run_id: "run-1", thread_id: "thread-1", graph: "react_agent", status: "success" }];
  state.recorder = { runId: "run-1", exactEnvelope: true, complete: true, error: null,
    events: [{ run_id: "run-1", thread_id: "thread-1" }, { run_id: "run-1", thread_id: "thread-1" }] };
  const snapshot = J.journeySnapshot(state);
  check("journey inspect: only a complete exact same-thread journal proves inspection",
    snapshot.exactRecorder && stage(snapshot, "inspect").complete && stage(snapshot, "evaluate").available);
  check("journey focus: a deliberate evaluation handoff remains the current stage inside the shared Run workspace",
    snapshot.active === "evaluate" && stage(snapshot, "evaluate").current);
  state.recorder.events[1].thread_id = "thread-crossed";
  const crossed = J.journeySnapshot(state);
  check("journey integrity: crossed inner event identity removes exact Inspect and Evaluate claims",
    !crossed.exactRecorder && !stage(crossed, "inspect").complete && !stage(crossed, "evaluate").available);
}

{
  const state = connectedState();
  state.view = "thread"; state.selected = "thread-1"; state.threads = [{ thread_id: "thread-1", graph: "react_agent" }];
  state.recorder = { runId: "run-1", exactEnvelope: true, complete: true, error: null,
    events: [{ run_id: "run-1", thread_id: "thread-1" }] };
  state.qualityCase = { phase: "locked", source: { recorder: state.recorder, runId: "run-1", threadId: "thread-1",
    input: { secret: "must-not-enter-rail" } } };
  state.memory = { loading: false, error: null, totalRecords: 4 };
  state.learn = { loading: false, error: null, records: [{ candidate_id: "candidate-1" }] };
  state.schedules = { loading: false, error: null, list: [{ cron_id: "daily" }] };
  state.automations = { loading: false, error: null, list: [] };
  state.tasks = { loading: false, error: null, list: [{ task_id: "task-1" }] };
  const snapshot = J.journeySnapshot(state);
  check("journey evidence: evaluation, governance, and operations use truthful retained boundaries",
    stage(snapshot, "evaluate").complete && stage(snapshot, "govern").complete && stage(snapshot, "operate").complete &&
    stage(snapshot, "operate").detail.includes("2 records · 3/3 desks loaded"));
  check("journey privacy: prompts, results, credentials, and quality inputs never enter the derived rail",
    !JSON.stringify(snapshot).includes("must-not-enter-rail") && !JSON.stringify(snapshot).includes("secret-never-render"));
}

{
  const state = connectedState();
  state.view = "thread"; state.selected = "thread-1";
  state.threads = [{ thread_id: "thread-1", graph: "react_agent" }];
  state.recorder = { runId: "run-1", exactEnvelope: true, complete: true, error: null,
    events: [{ run_id: "run-1", thread_id: "thread-1" }] };
  state.qualityDataset = { acknowledged: true, cases: [{ id: "unrelated" }] };
  state.qualityReport = { consistent: true, invalid: false };
  state.qualityCase = { phase: "locked", source: { recorder: {}, runId: "run-other", threadId: "thread-other" } };
  const snapshot = J.journeySnapshot(state);
  check("journey evaluation: unrelated page-memory quality artifacts never complete the current run",
    !snapshot.qualityEvidence && !stage(snapshot, "evaluate").complete && stage(snapshot, "evaluate").available);
}

{
  const state = connectedState();
  state.view = "agents";
  state.agents = { selected: "<agent&1>", list: [{ assistant_id: "<agent&1>", name: "<Lead&One>", graph: "react_agent" }] };
  const snapshot = J.journeySnapshot(state);
  const context = J.journeyContextHtml(snapshot), rail = J.journeyHtml(snapshot);
  check("journey rendering: hostile legal identities are escaped in the visible exact context",
    context.includes("&lt;agent&amp;1&gt;") && !context.includes("<agent&1>"));
  check("journey accessibility: native stages expose current position and disable unavailable destinations",
    rail.includes('aria-current="step"') && rail.includes('data-journey-stage="inspect" disabled'));
}

{
  const payload = { focused: 0, focus() { this.focused += 1; } };
  const quality = { focused: 0, focus() { this.focused += 1; }, scrollIntoView() {} };
  const announcer = { textContent: "" };
  elementMap = new Map([["inp-payload", payload], ["quality-foundry-title", quality], ["studio-journey-announcer", announcer]]);
  Object.assign(J.store, connectedState(), {
    view: "thread", selected: "thread-1", threads: [{ thread_id: "thread-1", graph: "react_agent" }],
    agents: { selected: "agent-1", list: [{ assistant_id: "agent-1", graph: "react_agent" }] },
  });
  J.store.agentRunHistory["agent-1"] = [{ run_id: "run-1", thread_id: "thread-1", graph: "react_agent", status: "success" }];
  let renderOptions = null;
  sandbox.__captureRender = (options) => { renderOptions = options || {}; };
  vm.runInContext("renderThreads = () => {}; renderMain = (options) => globalThis.__captureRender(options);", sandbox);
  const ran = await J.journeyOpenStage("run");
  check("journey routing: Run preserves the exact selected thread and hands focus to its real task input",
    ran && J.store.view === "thread" && J.store.selected === "thread-1" && J.store.journeyPhase === "run" && payload.focused === 1);
  J.store.recorder = { runId: "run-1", exactEnvelope: true, complete: true, error: null,
    events: [{ run_id: "run-1", thread_id: "thread-1" }] };
  renderOptions = null;
  const evaluated = await J.journeyOpenStage("evaluate");
  check("journey routing: Evaluate preserves exact same-thread evidence and focuses the first quality decision surface",
    evaluated && J.store.journeyPhase === "evaluate" && renderOptions === null &&
    J.store.recorder.runId === "run-1" && quality.focused === 1);
  J.store.recorder = null; J.store.agentRunHistory = Object.create(null); J.store.selected = null;
  const blocked = await J.journeyOpenStage("inspect");
  check("journey routing: an unavailable Inspect stage neither navigates nor fails silently",
    !blocked && announcer.textContent.includes("Inspect is not available yet"));
}

{
  const state = connectedState();
  state.view = "agents";
  state.info.graphs = [{ name: "registered" }];
  state.agents = { selected: "agent-stale", list: [{ assistant_id: "agent-stale", name: "Stale", graph: "removed" }] };
  const snapshot = J.journeySnapshot(state);
  check("journey readiness: a removed graph cannot be presented as ready for real work",
    !stage(snapshot, "run").available && stage(snapshot, "run").detail === "select a runnable agent");
}

{
  let opened = "";
  sandbox.__journeyOpened = (value) => { opened = value; };
  vm.runInContext(`
    openMemory = () => globalThis.__journeyOpened("memory");
    openLearn = () => globalThis.__journeyOpened("learn");
    openSchedules = () => globalThis.__journeyOpened("schedules");
    openAutomations = () => globalThis.__journeyOpened("automations");
    openTasks = () => globalThis.__journeyOpened("tasks");
  `, sandbox);
  Object.assign(J.store, connectedState(), { memoryUnsupported: true, learnUnsupported: false,
    schedulesUnsupported: false, automationsUnsupported: true, tasksUnsupported: true });
  await J.journeyOpenStage("govern");
  const governed = opened;
  opened = "";
  await J.journeyOpenStage("operate");
  const fromHome = opened;
  opened = "";
  J.store.view = "tasks";
  await J.journeyOpenStage("operate");
  check("journey routing: governance and operations always choose a supported destination",
    governed === "learn" && fromHome === "schedules" && opened === "schedules");
}

{
  const quality = { focused: 0, focus() { this.focused += 1; }, scrollIntoView() {} };
  elementMap = new Map([["quality-foundry-title", quality], ["studio-journey-announcer", { textContent: "" }]]);
  Object.assign(J.store, connectedState(), { view: "agents", selected: null,
    recorder: { runId: "run-deferred", exactEnvelope: true, complete: true, error: null,
      events: [{ run_id: "run-deferred", thread_id: "thread-deferred" }] } });
  J.store.agentRunHistory["agent-1"] = [{ run_id: "run-deferred", thread_id: "thread-deferred", graph: "react_agent", status: "success" }];
  vm.runInContext(`
    agentOpenThread = async (threadId, runId, graph, options) => {
      store.selected = threadId; store.view = "thread";
      await new Promise((resolve) => { globalThis.__resolveJourneyOpen = resolve; });
      return options.current();
    };
  `, sandbox);
  const pending = J.journeyOpenStage("evaluate");
  await Promise.resolve();
  J.store.view = "home";
  sandbox.__resolveJourneyOpen();
  const completed = await pending;
  check("journey ownership: a deferred evidence handoff cannot focus after the user leaves",
    !completed && quality.focused === 0 && J.store.view === "home");
}

{
  let focused = "";
  const oldNext = { getAttribute: () => null, hasAttribute: (name) => name === "data-journey-next" };
  const nextButton = { focus() { focused = "next"; } };
  const stageButton = { focus() { focused = "stage"; } };
  const oldStage = { getAttribute: (name) => name === "data-journey-stage" ? "inspect" : null,
    hasAttribute: () => false };
  const panel = { contains: (value) => value === oldNext || value === oldStage };
  const rail = { innerHTML: "", querySelectorAll: () => [], querySelector: (selector) => selector.includes("inspect") ? null : stageButton };
  const context = { innerHTML: "" }, next = { innerHTML: "", querySelector: () => nextButton }, label = { textContent: "" };
  elementMap = new Map([["studio-journey", panel], ["studio-journey-rail", rail], ["studio-journey-context", context],
    ["studio-journey-next", next], ["studio-journey-label", label]]);
  sandbox.document.activeElement = oldNext;
  Object.assign(J.store, connectedState());
  J.store.view = "agents";
  J.journeyRender();
  const nextPreserved = focused === "next";
  focused = ""; sandbox.document.activeElement = oldStage;
  J.journeyRender();
  check("journey focus: rerenders retain next-action focus and fall back when a stage becomes unavailable",
    nextPreserved && focused === "next");
  sandbox.document.activeElement = null;
}

{
  const state = connectedState();
  for (let assistant = 0; assistant < 200; assistant++) {
    state.agentRunHistory[`agent-${assistant}`] = Array.from({ length: 40 }, (_, run) => ({
      run_id: `run-${assistant}-${run}`, thread_id: `thread-${assistant}-${run}`, status: "success",
    }));
  }
  check("journey bounds: adversarial browser history is capped before every persistent-rail render",
    J.journeyRunEntries(state).length === 80 * 12);
}

{
  const focused = [];
  const buttons = ["shape", "run", "govern"].map((key) => ({
    key, focus() { focused.push(key); }, closest() { return this; },
  }));
  elementMap = new Map([["studio-journey-rail", { querySelectorAll: () => buttons }]]);
  const event = { target: buttons[1], key: "ArrowRight", prevented: false, preventDefault() { this.prevented = true; } };
  const moved = J.journeyKeyboard(event);
  const homeEvent = { target: buttons[2], key: "Home", preventDefault() {} };
  J.journeyKeyboard(homeEvent);
  check("journey keyboard: arrow navigation moves through only enabled stages and Home returns to the first",
    moved && event.prevented && focused.join(",") === "govern,shape");
}

check("journey markup: the lifecycle rail is available inside active workspaces with one stable announcer",
  html.indexOf('id="studio-journey"') < html.indexOf('id="home-view"') &&
  html.includes('id="studio-journey-announcer" role="status" aria-live="polite" aria-atomic="true"') &&
  html.includes('panel.hidden = store.view === "home" || store.view === "thread"'));
check("journey interaction: one delegated action path owns stage and next-safe-move routing",
  html.includes('target.closest("[data-journey-stage],[data-journey-next]")') &&
  html.includes('$("studio-journey-rail").addEventListener("keydown", journeyKeyboard)'));
check("journey responsive: mobile uses a deliberate three-by-two rail and stacked identity context",
  html.includes('.studio-journey-rail { grid-template-columns: repeat(3,minmax(0,1fr));') &&
  html.includes('.studio-journey-context { grid-column: 1 / -1; grid-row: 2; }'));
check("journey singularity: Mission control and the focused thread flow omit duplicate lifecycle rails",
  !html.includes('class="home-evidence-rail"') && html.includes('store.view === "home" || store.view === "thread"'));
check("journey calmness: implementation-boundary commentary is absent from the primary workspace",
  !html.includes("Identity-only page context") && !html.includes("the rail never upgrades browser recall into server truth"));

if (failed) {
  console.error(`\nFAIL: ${failed} failed, ${passed} passed`);
  process.exit(1);
}
console.log(`\nPASS: ${passed} Studio journey assertions`);
