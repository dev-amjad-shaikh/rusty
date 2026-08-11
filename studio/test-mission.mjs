#!/usr/bin/env node
/* Focused contracts for Studio's progressive mission dossier. The suite proves
 * one-decision guidance, privacy-safe evidence, supported destination routing,
 * focus continuity, bounded rendering, and deliberate mobile behavior.
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

let elementMap = new Map();
const sandbox = {
  URL, URLSearchParams, TextEncoder, TextDecoder, CSS: { escape: (value) => String(value) },
  localStorage: { getItem: () => null, setItem() {} },
  document: { activeElement: null, getElementById: (id) => elementMap.get(id) || null },
};
vm.createContext(sandbox);
vm.runInContext(source + `
globalThis.__mission = { store, JOURNEY_STAGE_KEYS, journeySnapshot, journeyMissionDestination,
  journeyMissionDossier, journeyMissionHtml, journeyRender, journeyOpenStage };
`, sandbox, { filename: "index.html<script>" });
const M = sandbox.__mission;

let passed = 0, failed = 0;
function check(name, condition, detail = "") {
  if (condition) { passed++; console.log(`ok   ${name}`); }
  else { failed++; console.log(`FAIL ${name}${detail ? ` — ${detail}` : ""}`); }
}
function connectedState() {
  return {
    conn: { baseUrl: "http://tenant-a", apiKey: "secret-never-render" },
    info: { service: "rusty-agent-server", version: "0.12.0", graphs: [{ name: "react_agent" }] },
    view: "home", selected: null, threads: [], recorder: null, qualityCase: null,
    agents: { list: [], selected: null }, agentsUnsupported: false,
    agentRuns: Object.create(null), agentRunHistory: Object.create(null),
    memory: null, memoryUnsupported: false, learn: null, learnUnsupported: false,
    schedules: null, schedulesUnsupported: false, automations: null, automationsUnsupported: false,
    tasks: null, tasksUnsupported: false,
  };
}

{
  const state = { view: "home" };
  const dossier = M.journeyMissionDossier(M.journeySnapshot(state), state);
  check("mission onboarding: an unverified system yields one connection decision",
    dossier.key === "connect" && dossier.question.includes("Rusty system") && dossier.destination === "Connection Hub" &&
    dossier.gap.includes("server identity") && dossier.checkpoints === 0);
  check("mission onboarding: no identity is invented before connection",
    dossier.evidence.length === 0 && M.journeyMissionHtml(dossier).includes("No mission identity has been selected"));
}

{
  const state = connectedState();
  state.view = "agents";
  state.agents = { selected: "agent-1", list: [{ assistant_id: "agent-1", name: "Research lead", graph: "react_agent", active_version_id: "av-1" }] };
  const dossier = M.journeyMissionDossier(M.journeySnapshot(state), state);
  check("mission progression: an exact shaped agent produces the Run decision rather than another dashboard",
    dossier.key === "run" && dossier.destination === "Agent Workbench" && dossier.actionLabel === "Continue in Agent Workbench");
  check("mission Run truth: a selected runnable agent names the missing run proof and the real next task",
    dossier.gap.includes("real run and thread") && dossier.safeMove.includes("selected agent") && !dossier.safeMove.includes("Choose a runnable agent"));
  check("mission evidence: only bounded identity and verified status enter the dossier",
    JSON.stringify(dossier).includes("agent-1") && JSON.stringify(dossier).includes("av-1") && !JSON.stringify(dossier).includes("secret-never-render"));
}

{
  const state = connectedState();
  state.view = "thread"; state.selected = "thread-1";
  state.threads = [{ thread_id: "thread-1", graph: "react_agent" }];
  state.agentRunHistory["agent-1"] = [{ run_id: "run-1", thread_id: "thread-1", graph: "react_agent", status: "success" }];
  state.recorder = { runId: "run-1", exactEnvelope: true, complete: true, error: null,
    events: [{ run_id: "run-1", thread_id: "thread-1", input: { prompt: "never copy me" } }] };
  const snapshot = M.journeySnapshot(state);
  const dossier = M.journeyMissionDossier(snapshot, state);
  const rendered = M.journeyMissionHtml(dossier);
  check("mission progression: an exact journal advances to one evaluation decision",
    dossier.key === "evaluate" && dossier.destination === "Case Foundry" && dossier.gap.includes("frozen reviewed case"));
  check("mission privacy: journal payloads and prompts never enter derived state or markup",
    !JSON.stringify(dossier).includes("never copy me") && !rendered.includes("never copy me"));
  check("mission rendering: the primary handoff reuses the exact journey action contract",
    rendered.includes('data-mission-primary data-journey-next="evaluate"') && rendered.includes("Continue in Case Foundry"));
}

{
  const state = connectedState();
  state.memoryUnsupported = true;
  state.learn = { loading: false, error: null, records: [{ candidate_id: "candidate-1" }] };
  state.schedulesUnsupported = true; state.automationsUnsupported = false; state.tasksUnsupported = false;
  state.automations = { loading: false, error: null, list: [{ trigger_id: "trigger-1" }] };
  const snapshot = M.journeySnapshot(state);
  check("mission routing: governance names the supported evidence workspace",
    M.journeyMissionDestination("govern", snapshot, state) === "Learning Inbox");
  check("mission routing: operations names a supported loaded destination",
    M.journeyMissionDestination("operate", snapshot, state) === "Automation Desk");
}

{
  const state = connectedState();
  state.view = "automations"; state.schedulesUnsupported = false; state.automationsUnsupported = false;
  state.tasksUnsupported = false; state.schedules = null; state.automations = null; state.tasks = null;
  Object.assign(M.store, state);
  const automationTitle = { focused: 0, focus() { this.focused += 1; }, scrollIntoView() {} };
  elementMap = new Map([["automations-title", automationTitle]]);
  const snapshot = M.journeySnapshot(M.store);
  const dossier = M.journeyMissionDossier(snapshot, M.store);
  const opened = await M.journeyOpenStage("operate");
  check("mission routing: an already-open supported desk owns both the visible destination and the action",
    dossier.destination === "Automation Desk" && opened && M.store.view === "automations" && automationTitle.focused === 1);
}

{
  const stages = M.JOURNEY_STAGE_KEYS.map((key) => ({ key, complete: true, available: true, detail: "loaded" }));
  const snapshot = { connected: true, stages, next: { key: "home" }, active: "operate", agentId: "agent-1",
    versionId: "av-1", threadId: "thread-1", runId: "run-1", exactRecorder: true, qualityEvidence: true,
    candidateCount: 1 };
  const dossier = M.journeyMissionDossier(snapshot, connectedState());
  check("mission completion: six checkpoints resolve to review instead of restarting Shape",
    dossier.key === "home" && dossier.checkpoints === 6 && dossier.destination === "Mission board");
}

{
  const state = connectedState();
  state.agents = { selected: "<agent&1>", list: [{ assistant_id: "<agent&1>", name: "Lead", graph: "react_agent" }] };
  const rendered = M.journeyMissionHtml(M.journeyMissionDossier(M.journeySnapshot(state), state));
  check("mission rendering: hostile legal identity text is escaped",
    rendered.includes("&lt;agent&amp;1&gt;") && !rendered.includes("<agent&1>"));
}

{
  let focused = "";
  const oldMission = { hasAttribute: (name) => name === "data-mission-primary", getAttribute: () => null };
  const missionButton = { focus() { focused = "mission"; } };
  const nextButton = { focus() { focused = "next"; } };
  const stageButton = { focus() { focused = "stage"; } };
  const panel = { contains: (value) => value === oldMission };
  const rail = { innerHTML: "", querySelectorAll: () => [], querySelector: () => stageButton };
  const next = { innerHTML: "", querySelector: () => nextButton };
  const missionBody = { innerHTML: "", querySelector: () => missionButton };
  elementMap = new Map([
    ["studio-journey", panel], ["studio-journey-rail", rail], ["studio-journey-context", { innerHTML: "" }],
    ["studio-journey-next", next], ["studio-journey-label", { textContent: "" }],
    ["studio-mission-body", missionBody], ["studio-mission-progress", { textContent: "" }],
  ]);
  sandbox.document.activeElement = oldMission;
  Object.assign(M.store, connectedState());
  M.store.view = "agents";
  const snapshot = M.journeyRender();
  check("mission focus: evidence refresh restores the dossier action rather than jumping to the header",
    snapshot && focused === "mission" && missionBody.innerHTML.includes("studio-mission-decision"));
  sandbox.document.activeElement = null;
}

check("mission markup: the progressive dossier is a native, collapsed-on-entry region beneath the evidence rail",
  html.indexOf('id="studio-journey-rail"') < html.indexOf('id="studio-mission"') &&
  html.includes('<details class="studio-mission" id="studio-mission">') &&
  html.includes('id="studio-mission-body" aria-labelledby="studio-mission-label"'));
check("mission lifecycle: rendering updates only the dossier body so the operator's expanded state is preserved",
  html.includes('missionBody.innerHTML = journeyMissionHtml(dossier)') && !html.includes('$("studio-mission").innerHTML'));
check("mission responsive: the dossier becomes one decision column with a full-width primary handoff",
  html.includes('.studio-mission-body { grid-template-columns: 1fr;') &&
  html.includes('.studio-mission-action button { width: 100%; }'));
check("mission calmness: the primary next-step view omits implementation and approval commentary",
  !html.includes("the destination remains authoritative") && !html.includes("this summary never approves a change") &&
  !html.includes("One decision at a time. The dossier carries bounded identity"));
check("mission documentation: product and roadmap name the same derived, collapsible evidence boundary",
  docs.includes("Progressive mission dossier") && docs.includes("at most five bounded identity/status facts") &&
  roadmap.includes("progressive mission dossier") && roadmap.includes("retains no separate mission record"));

if (failed) {
  console.error(`\nFAIL: ${failed} failed, ${passed} passed`);
  process.exit(1);
}
console.log(`\nPASS: ${passed} Studio mission dossier assertions`);
