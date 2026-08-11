#!/usr/bin/env node
/* Focused tests for the Studio Home mission board's evidence model and
 * navigation contract. The browser bootstrap is stripped; only pure helpers
 * are exercised under vm, with static checks for delegated interactions.
 */
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";
import vm from "node:vm";

const here = path.dirname(fileURLToPath(import.meta.url));
const html = readFileSync(path.join(here, "index.html"), "utf8");
const match = html.match(/<script>([\s\S]*?)<\/script>/);
if (!match) { console.error("FAIL: no script block"); process.exit(1); }
const src = match[1].replace(/\ninit\(\);\s*$/, "\n");

const localData = new Map();
const sandbox = { localStorage: {
  getItem: (key) => localData.has(key) ? localData.get(key) : null,
  setItem: (key, value) => localData.set(key, String(value)),
} };
vm.createContext(sandbox);
vm.runInContext(src + `
globalThis.__home = { store, homeTime, homeSnapshot, homePrimaryAction, homeAttentionRoute, homeHtml,
  homeFocusIdentity, homeRestoreFocus, agentsLoad };
`, sandbox, { filename: "index.html<script>" });
const H = sandbox.__home;

let passed = 0;
let failed = 0;
function check(name, condition, detail = "") {
  if (condition) { passed++; console.log(`ok   ${name}`); }
  else { failed++; console.log(`FAIL ${name}${detail ? ` — ${detail}` : ""}`); }
}

function connectedState() {
  return {
    conn: { baseUrl: "http://local", apiKey: "tenant-secret" },
    info: { service: "rusty-server", version: "0.9.0", graphs: [{ name: "react_agent" }] },
    agents: { list: [] }, agentRunHistory: Object.create(null),
    fabric: { list: [] }, fabricRunHistory: [], fabricBlueprints: [],
    threads: [], memory: null, learn: { records: [], versions: [] },
  };
}

{
  const snapshot = H.homeSnapshot({});
  check("home onboarding: disconnected state leads with a local connection", !snapshot.connected &&
    snapshot.next.action === "connect" && snapshot.service === "Not connected");
  const markup = H.homeHtml(snapshot);
  check("home onboarding: the empty mission board leads with the user's first agent task",
    markup.includes("Create your first agent") && markup.includes("Define its job, connect the capabilities it needs") &&
    markup.includes('data-home-action="connect"') && !markup.includes("evidence-led workspace") && !markup.includes("registered behavior"));
}

{
  const state = connectedState();
  state.learn = null;
  const snapshot = H.homeSnapshot(state);
  check("home startup: an unloaded learning workspace remains unknown without blocking confirmed agent guidance",
    snapshot.connected && snapshot.candidate_count === null && !snapshot.candidate_known && snapshot.next.action === "create-agent");
}

{
  const pending = new Map();
  sandbox.__agentApi = (connection) => new Promise((resolve) => pending.set(connection.baseUrl, resolve));
  vm.runInContext("apiForConnection = globalThis.__agentApi", sandbox);
  sandbox.document = { getElementById: () => ({ textContent: "" }) };
  H.store.view = "thread";
  H.store.conn = { baseUrl: "http://server-b", apiKey: "tenant-b" };
  H.store.agents = null;
  const older = H.agentsLoad(true);
  H.store.conn = { baseUrl: "http://server-c", apiKey: "tenant-c" };
  H.store.agents = null;
  const newer = H.agentsLoad(true);
  pending.get("http://server-c")([{ assistant_id: "agent-c" }]);
  await newer;
  pending.get("http://server-b")([{ assistant_id: "agent-b" }]);
  await older;
  check("home isolation: a late assistant response cannot overwrite a newer server and tenant catalog",
    H.store.conn.baseUrl === "http://server-c" && H.store.agents.list.length === 1 &&
    H.store.agents.list[0].assistant_id === "agent-c" && !H.store.agents.loading);
}

{
  const state = connectedState();
  state.agents = null;
  const pending = H.homeSnapshot(state);
  check("home startup: a first-connect pending catalog cannot recommend duplicate creation",
    !pending.assistant_known && pending.assistant_loading && pending.assistant_count === null &&
    pending.next.action === "agents" && H.homeHtml(pending).includes("Check agent catalog"));
}

{
  const state = connectedState();
  state.agents = { list: [{ assistant_id: "retained" }], loading: false, error: { status: 503 } };
  state.fabric = { list: [{ agent_id: "retained", metadata: { team_id: "old" } }], loading: false, error: { status: 503 } };
  state.learn = { records: [{ candidate_id: "old" }], totalCandidates: 1, loading: false, error: { status: 503 } };
  const failed = H.homeSnapshot(state);
  const markup = H.homeHtml(failed);
  check("home truth: failed refreshes present concise unavailable state instead of retained counts",
    !failed.assistant_known && failed.assistant_count === null && !failed.team_known && failed.team_count === null &&
    !failed.candidate_known && failed.candidate_count === null && (markup.match(/Needs refresh/g) || []).length >= 2);
}

{
  const state = connectedState();
  state.agents.list = [{ assistant_id: "assistant-1", graph: "pipeline" }];
  state.agentRunHistory["assistant-1"] = [{ record_id: "run-1", status: "success", started_at: "2026-08-09T12:00:00Z" }];
  state.learn.records = [{ candidate_id: "candidate-1", status: "created" }];
  const markup = H.homeHtml(H.homeSnapshot(state));
  check("home truth: candidate evidence never turns unloaded memory into a false zero",
    markup.includes(">—</b><span>Memory</span><small>Not loaded</small>") && !markup.includes(">0</b><span>Memory</span>"));
}

{
  const state = connectedState();
  const empty = H.homeSnapshot(state);
  check("home next action: a connected empty server asks for one useful agent", empty.next.action === "create-agent");
  state.agents.list = [{ assistant_id: "assistant-1", graph: "react_agent" }];
  const shaped = H.homeSnapshot(state);
  check("home next action: a shaped system advances to a real task", shaped.next.action === "agents" && shaped.assistant_count === 1);
}

{
  const state = connectedState();
  state.agents.list = [{ assistant_id: "unavailable", graph: "react_agent", archived_at: "08/10/2026" }];
  const unavailable = H.homeSnapshot(state);
  const markup = H.homeHtml(unavailable);
  check("home lifecycle: an unclassifiable record is discoverable and never becomes a create-first recommendation",
    unavailable.assistant_count === 0 && unavailable.assistant_archived_count === 0 &&
    unavailable.assistant_unavailable_count === 1 && unavailable.next.action === "unavailable-agents" &&
    markup.includes("Review unavailable agents") && markup.includes("0 archived · 1 unavailable"));
}

{
  const state = connectedState();
  state.agents.list = [{ assistant_id: "assistant-1", graph: "react_agent" }];
  state.agentRunHistory["assistant-1"] = [{
    record_id: "agent-run", run_id: "agent-run", thread_id: "thread-1", graph: "react_agent",
    status: "success", started_at: "2026-08-09T10:00:00Z", finished_at: "2026-08-09T10:01:00Z",
    prompt: "must never appear", result: { secret: "must never appear" },
  }];
  state.fabricRunHistory = [{ coordination_id: "team-latest", pattern: "quorum", status: "failed",
    member_count: 3, settled_count: 3, observed_at: "2026-08-09T11:00:00Z", stale: false }];
  const before = JSON.stringify(state);
  const snapshot = H.homeSnapshot(state);
  const markup = H.homeHtml(snapshot);
  check("home recency: agent and team recall share one deterministic latest-work decision",
    snapshot.run_count === 2 && snapshot.latest.family === "team" && snapshot.latest.coordination_id === "team-latest");
  check("home attention: failed team evidence takes priority over routine continuation",
    snapshot.attention_count === 1 && snapshot.attention.coordination_id === "team-latest" &&
    snapshot.next.action === "attention" && snapshot.next.label === "Investigate attention");
  check("home privacy: prompts, results, and connection credentials never enter rendered Home evidence",
    !markup.includes("must never appear") && !markup.includes("tenant-secret") &&
    !JSON.stringify(snapshot).includes("must never appear") && !JSON.stringify(snapshot).includes("tenant-secret"));
  check("home purity: summary construction does not mutate source evidence", JSON.stringify(state) === before);
}

{
  const state = connectedState();
  state.agents.list = [{ assistant_id: "assistant-1", graph: "pipeline" }];
  state.agentRunHistory["assistant-1"] = [
    { record_id: "healthy-latest", status: "success", started_at: "2026-08-09T12:00:00Z" },
    { record_id: "failed-earlier", run_id: "failed-run", thread_id: "failed-thread", graph: "pipeline",
      status: "error", started_at: "2026-08-09T11:00:00Z" },
  ];
  const snapshot = H.homeSnapshot(state);
  const route = H.homeAttentionRoute(snapshot.attention);
  check("home attention: the recommended target is the evidence needing review, not merely the latest run",
    snapshot.latest.record_id === "healthy-latest" && snapshot.attention.record_id === "failed-earlier" &&
    snapshot.next.action === "attention" && route.action === "thread-run" && route.id === "failed-thread" &&
    route.run_id === "failed-run" && route.graph === "pipeline");
}

{
  const state = connectedState();
  for (let assistant = 0; assistant < 100; assistant++) {
    state.agentRunHistory[`assistant-${assistant}`] = Array.from({ length: 20 }, (_, run) => ({
      record_id: `run-${assistant}-${run}`, status: "success", started_at: "2026-08-09T10:00:00Z",
    }));
  }
  state.fabricRunHistory = Array.from({ length: 40 }, (_, run) => ({
    coordination_id: `coord-${run}`, status: "completed", observed_at: "2026-08-09T10:00:00Z",
  }));
  check("home bounds: adversarial recalled history stays within the established browser evidence limits",
    H.homeSnapshot(state).run_count === 80 * 12 + 24);
}

{
  const state = connectedState();
  state.agents.list = [{ assistant_id: "assistant-1", graph: "pipeline" }];
  state.agentRunHistory["assistant-1"] = [{ record_id: "rec-1", run_id: "<run&1>", graph: "pipeline",
    status: "success", started_at: "2026-08-09T12:00:00Z" }];
  state.learn.records = [{ candidate_id: "candidate-1", status: "created" }];
  const snapshot = H.homeSnapshot(state);
  const markup = H.homeHtml(snapshot);
  check("home governance: a healthy run with candidates leads into governed review",
    snapshot.next.action === "learn" && snapshot.candidate_count === 1);
  check("home escaping: hostile remembered identifiers remain text, never markup",
    markup.includes("&lt;run&amp;1&gt;") && !markup.includes("<run&1>"));
  check("home continuation: agent evidence preserves owner routing and an optional thread action",
    markup.includes('data-home-action="agent-run"') && markup.includes('data-home-owner="assistant-1"'));
}

{
  const unknown = connectedState();
  const unknownSnapshot = H.homeSnapshot(unknown);
  unknown.memory = { loading: false, error: null, totalRecords: 0, totalConflicts: 0 };
  const knownSnapshot = H.homeSnapshot(unknown);
  check("home truth: memory not loaded is distinct from a server-confirmed empty ledger",
    !unknownSnapshot.memory_known && unknownSnapshot.memory_records === null &&
    knownSnapshot.memory_known && knownSnapshot.memory_records === 0 && knownSnapshot.memory_conflicts === 0);
  check("home truth: unloaded and confirmed-empty memory remain visibly distinct without implementation narration",
    H.homeHtml(unknownSnapshot).includes(">—</b><span>Memory</span><small>Not loaded</small>") &&
    H.homeHtml(knownSnapshot).includes(">0</b><span>Memory</span><small>0 conflicts</small>") &&
    !H.homeHtml(unknownSnapshot).includes("server truth"));
}

check("home markup: Home is the default labelled workspace with a persistent sidebar return",
  html.includes('view: "home"') && html.includes('id="btn-home-open"') &&
  html.includes('id="home-view" aria-labelledby="home-title"'));
check("home interaction: one delegated handler routes every mission-board action",
  html.includes('$("home-view").addEventListener("click"') &&
  html.includes('target.closest("[data-home-action]")') && html.includes("homeNavigate(target.getAttribute"));

{
  const attributes = { "data-home-action": "agents", "data-home-id": "", "data-home-owner": "" };
  const oldButton = { getAttribute: (name) => attributes[name] || null };
  oldButton.closest = () => oldButton;
  const oldView = { contains: (item) => item === oldButton, querySelectorAll: () => [oldButton] };
  sandbox.document = { activeElement: oldButton };
  const identity = H.homeFocusIdentity(oldView);
  let focused = "";
  const replacement = { getAttribute: oldButton.getAttribute, focus: () => { focused = "replacement"; } };
  const title = { focus: () => { focused = "title"; } };
  H.homeRestoreFocus({ querySelectorAll: () => [replacement], querySelector: () => title }, identity);
  const replaced = focused === "replacement";
  focused = "";
  H.homeRestoreFocus({ querySelectorAll: () => [], querySelector: () => title }, identity);
  check("home focus: async replacement restores the same action and disappearing actions fall back to Home",
    replaced && focused === "title");
}
check("home responsive: the primary mission and system actions collapse cleanly",
  html.includes(".home-hero { grid-template-columns: 1fr;") &&
  html.includes(".home-signals { grid-template-columns: 1fr;"));
check("home accessibility: Mission control is focusable and the quiet navigation exposes its current workspace",
  html.includes('id="home-title" tabindex="-1"') && html.includes('$("home-title")?.focus') &&
  html.includes('button.setAttribute("aria-current", "page")'));
check("home accessibility: every Home destination receives a visible labelled focus target",
  html.includes('id="agents-title" tabindex="-1"') && html.includes('$("agents-title").focus') &&
  html.includes('id="tasks-title" tabindex="-1"') && html.includes('$("tasks-title").focus') &&
  html.includes('id="thread-head" tabindex="-1"') && html.includes('$("thread-head")?.focus'));

if (failed) {
  console.error(`\nFAIL: ${failed} failed, ${passed} passed`);
  process.exit(1);
}
console.log(`\nPASS: ${passed} Studio Home assertions`);
