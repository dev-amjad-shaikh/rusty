#!/usr/bin/env node
/* Focused contracts and async ownership tests for Studio's non-secret
 * workspace and evidence links. The browser bootstrap is removed; helpers
 * execute inside the same dependency-free VM used by the other suites.
 */
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";
import vm from "node:vm";

const here = path.dirname(fileURLToPath(import.meta.url));
const page = readFileSync(path.join(here, "index.html"), "utf8");
const match = page.match(/<script>([\s\S]*?)<\/script>/);
if (!match) { console.error("FAIL: no Studio script"); process.exit(1); }
const source = match[1].replace(/\ninit\(\);\s*$/, "\n");

const elements = new Map();
const element = (id) => {
  if (!elements.has(id)) elements.set(id, {
    id, value: "", textContent: "", style: {}, dataset: {}, hidden: false, focused: 0,
    focus() { this.focused += 1; }, scrollIntoView() {},
  });
  return elements.get(id);
};
const sandbox = {
  URL, URLSearchParams,
  document: { getElementById: element, activeElement: null },
  location: new URL("http://127.0.0.1:8000/studio/index.html"),
  history: { replaceState() {} },
  navigator: {},
};
vm.createContext(sandbox);
vm.runInContext(source + `
globalThis.__navigation = {
  store, navigationSafeId, navigationParseSearch, navigationBuildUrl,
  navigationRouteForStore, navigationOpenSharedRun, navigationCancelPending,
  navigationFinish, navigationRouteMessage, navigationPrepareLocation,
  navigationCopyCurrent, navigationApplyPending, navigationReconcileRecorderRoute,
  automationRunJournalThread,
};`, sandbox, { filename: "index.html<script>" });
const N = sandbox.__navigation;

let passed = 0, failed = 0;
function check(name, condition, detail = "") {
  if (condition) { passed++; console.log(`ok   ${name}`); }
  else { failed++; console.log(`FAIL ${name}${detail ? ` — ${detail}` : ""}`); }
}

check("route contract: a workspace-only link is accepted",
  N.navigationParseSearch("?studio=memory").route?.view === "memory");
check("route contract: exact agent, thread/run, and automation targets are accepted",
  N.navigationParseSearch("?studio=agents&agent=assistant-1").route?.agent === "assistant-1" &&
  N.navigationParseSearch("?studio=thread&thread=thread-1&run=run-1").route?.run === "run-1" &&
  N.navigationParseSearch("?studio=automations&automation=hook-1").route?.automation === "hook-1");
check("route contract: unsupported workspaces and cross-workspace targets fail closed",
  !N.navigationParseSearch("?studio=fleet").route &&
  !N.navigationParseSearch("?studio=memory&agent=assistant-1").route &&
  !N.navigationParseSearch("?studio=agents&run=run-1").route);
check("route contract: repeated fields and target-free thread links fail closed",
  !N.navigationParseSearch("?studio=agents&agent=a&agent=b").route &&
  !N.navigationParseSearch("?studio=thread").route);
check("route contract: controls, bidi spoofing, and oversized identities fail closed",
  !N.navigationSafeId("run\t1") && !N.navigationSafeId("run\u202e1") &&
  !N.navigationSafeId("run\u200b1") && !N.navigationSafeId("x".repeat(257)));
check("route contract: unrelated pages do not become a Studio deep link",
  N.navigationParseSearch("?q=agent").route === null && N.navigationParseSearch("?q=agent").error === "");
check("route contract: unknown query fields fail the identifier-only boundary",
  !N.navigationParseSearch("?studio=thread&run=run-1&api_key=secret&server=https%3A%2F%2Fexample.test").route &&
  N.navigationParseSearch("?studio=thread&run=run-1&payload=hidden").managed === true);

{
  let replaced = "";
  sandbox.location.href = "http://127.0.0.1:8000/studio/index.html?studio=thread&run=run-1&api_key=secret#payload";
  sandbox.history.replaceState = (_state, _title, url) => { replaced = url; };
  const parsed = N.navigationPrepareLocation();
  check("privacy: a blocked Studio route immediately scrubs its complete query and fragment",
    !parsed.route && parsed.error.includes("unsupported") &&
    replaced === "http://127.0.0.1:8000/studio/index.html" && !replaced.includes("secret"));
}

{
  const url = N.navigationBuildUrl({ view: "thread", thread: "thread-1", run: "run-1" },
    "https://user:password@example.test/studio?api_key=secret&foo=bar#payload");
  check("privacy: generated links contain only the bounded Studio route and evidence identities",
    url === "https://example.test/studio?studio=thread&thread=thread-1&run=run-1" &&
    !url.includes("api_key") && !url.includes("secret") && !url.includes("payload"));
}

{
  const state = {
    view: "thread", selected: "thread-1",
    recorder: { runId: "run-1", exactEnvelope: true, complete: true,
      events: [{ run_id: "run-1", thread_id: "thread-1" }] },
  };
  const exact = N.navigationRouteForStore(state);
  state.recorder.events[0].thread_id = "thread-other";
  const mismatched = N.navigationRouteForStore(state);
  check("route evidence: a run is shared only when its exact journal binds the selected thread",
    exact.thread === "thread-1" && exact.run === "run-1" &&
    mismatched.thread === "thread-1" && !("run" in mismatched));
}

check("route evidence: selected agent and automation identities survive route construction",
  N.navigationRouteForStore({ view: "agents", agents: { selected: "assistant-1" } }).agent === "assistant-1" &&
  N.navigationRouteForStore({ view: "automations", automations: { selected: "hook-1" } }).automation === "hook-1");

{
  N.store.conn = { baseUrl: "http://tenant-a", apiKey: "key-a" };
  N.store.connectionEpoch = 4;
  N.store.threads = [];
  const route = { view: "thread", run: "run-1" };
  N.store.navigationPending = route;
  sandbox.__navigationApi = async () => ({ run_id: "run-1", complete: true,
    events: [{ run_id: "run-1", thread_id: "thread-1" }] });
  sandbox.__navigationRecLoad = async (runId) => {
    N.store.recorder = { runId, exactEnvelope: true, complete: true,
      events: [{ run_id: runId, thread_id: "thread-1" }] };
  };
  vm.runInContext(`
apiForConnection = globalThis.__navigationApi;
recLoad = globalThis.__navigationRecLoad;
saveThreads = () => {};
renderThreads = () => {};
renderMain = () => {};
toast = () => {};
`, sandbox);
  const opened = await N.navigationOpenSharedRun(route);
  check("run handoff: server journal proves one thread before Recorder opens",
    opened && N.store.selected === "thread-1" && N.store.view === "thread" &&
    N.store.threads[0].metadata.source === "shared_run_evidence" && N.store.navigationPending === null);
}

for (const [label, recorder] of [
  ["failed", { runId: "run-second", requestedRunId: "run-second", exactEnvelope: false, events: [], complete: false, error: "not found" }],
  ["crossed", { runId: "run-other", requestedRunId: "run-second", exactEnvelope: true, complete: true,
    events: [{ run_id: "run-other", thread_id: "thread-other" }], error: null }],
]) {
  N.store.conn = { baseUrl: "http://tenant-a", apiKey: "key-a" };
  N.store.connectionEpoch += 1;
  N.store.threads = [];
  const route = { view: "thread", run: "run-second" };
  N.store.navigationPending = route;
  sandbox.__navigationApi = async () => ({ run_id: "run-second", complete: true,
    events: [{ run_id: "run-second", thread_id: "thread-second" }] });
  sandbox.__navigationRecLoad = async () => { N.store.recorder = recorder; };
  vm.runInContext("apiForConnection = globalThis.__navigationApi; recLoad = globalThis.__navigationRecLoad", sandbox);
  const opened = await N.navigationOpenSharedRun(route);
  check(`run handoff integrity: a ${label} Recorder reread cannot earn exact-open status`,
    opened === false && N.store.navigationPending === route &&
    N.store.navigationError.includes("corroborate the same exact run and thread"));
}

{
  N.store.conn = { baseUrl: "http://tenant-a", apiKey: "key-a" };
  N.store.connectionEpoch = 8;
  N.store.threads = [];
  N.store.selected = "thread-before-isolation";
  const route = { view: "thread", run: "run-stale" };
  N.store.navigationPending = route;
  let resolve;
  sandbox.__navigationApi = () => new Promise((done) => { resolve = done; });
  vm.runInContext("apiForConnection = globalThis.__navigationApi", sandbox);
  const pending = N.navigationOpenSharedRun(route);
  N.store.conn = { baseUrl: "http://tenant-b", apiKey: "key-b" };
  N.store.connectionEpoch += 1;
  resolve({ run_id: "run-stale", complete: true, events: [{ run_id: "run-stale", thread_id: "thread-a" }] });
  const opened = await pending;
  check("run handoff isolation: a late tenant response cannot navigate the newer connection",
    opened === false && N.store.selected === "thread-before-isolation" && !N.store.threads.some((item) => item.thread_id === "thread-a"));
}

{
  N.store.navigationPending = { view: "thread", run: "run-pending" };
  const before = N.store.navigationRequest;
  const cancelled = N.navigationCancelPending("agents", "assistant-new");
  check("run handoff ownership: choosing another workspace cancels the pending shared route",
    cancelled && N.store.navigationPending === null && N.store.navigationRequest === before + 1);
}

{
  N.store.navigationPending = { view: "agents", agent: "assistant-pending" };
  const cancelled = N.navigationCancelPending("agents");
  check("route ownership: explicitly reopening a workspace abandons its pending exact target",
    cancelled && N.store.navigationPending === null);
}

{
  const dialog = element("connection-dialog");
  const target = element("flight-recorder-title");
  dialog.open = true; target.focused = 0;
  N.store.navigationPending = { view: "thread", run: "run-focus" };
  N.navigationFinish("Opened shared run", "flight-recorder-title");
  check("focus: a connected deep link never moves focus behind the open Connection Hub",
    target.focused === 0 && N.store.navigationFocusTarget === "flight-recorder-title" &&
    source.includes("const target = store.navigationFocusTarget") && source.includes("if (target && $(target))"));
  dialog.open = false;
}

{
  let resolveClipboard;
  let replaced = "";
  sandbox.navigator.clipboard = { writeText: () => new Promise((resolve) => { resolveClipboard = resolve; }) };
  sandbox.history.replaceState = (_state, _title, url) => { replaced = url; };
  sandbox.location.href = "http://127.0.0.1:8000/studio/index.html?studio=agents&agent=assistant-a";
  N.store.navigationPending = null;
  N.store.view = "agents";
  N.store.agents = { selected: "assistant-a", list: [], loading: false };
  const copying = N.navigationCopyCurrent();
  N.store.view = "memory";
  resolveClipboard();
  await copying;
  check("clipboard ownership: a delayed copy cannot restore the prior workspace URL",
    replaced === "" && element("view-link-status").textContent.includes("workspace changed"));
}

{
  let resolveClipboard;
  let replaced = "";
  sandbox.navigator.clipboard = { writeText: () => new Promise((resolve) => { resolveClipboard = resolve; }) };
  sandbox.history.replaceState = (_state, _title, url) => { replaced = url; };
  N.store.navigationPending = null;
  N.store.view = "agents";
  N.store.agents = { selected: "assistant-same", list: [], loading: false };
  N.store.conn = { baseUrl: "http://tenant-a", apiKey: "key-a" };
  N.store.connectionEpoch = 30;
  const copying = N.navigationCopyCurrent();
  N.store.conn = { baseUrl: "http://tenant-b", apiKey: "key-b" };
  N.store.connectionEpoch += 1;
  resolveClipboard();
  await copying;
  check("clipboard isolation: a same-ID tenant switch invalidates delayed completion",
    replaced === "" && element("view-link-status").textContent.includes("workspace changed"));
}

{
  const route = { view: "thread", thread: "thread-recovered", run: "run-recovered" };
  N.store.navigationPending = route;
  N.store.threads = [];
  N.store.recorder = { runId: "run-recovered", requestedRunId: "run-recovered", exactEnvelope: true,
    complete: true, error: null, events: [{ run_id: "run-recovered", thread_id: "thread-recovered" }] };
  vm.runInContext("saveThreads = () => {}; renderThreads = () => {}; renderMain = () => {};", sandbox);
  const recovered = N.navigationReconcileRecorderRoute();
  check("run handoff recovery: an exact manual Recorder refresh completes a blocked shared route",
    recovered && N.store.navigationPending === null && N.store.navigationError === "" &&
    N.store.selected === "thread-recovered" && N.store.threads[0].thread_id === "thread-recovered");
}

{
  const route = { view: "agents", agent: "assistant-archived" };
  N.store.conn = { baseUrl: "http://tenant-a", apiKey: "key-a" };
  N.store.agents = { selected: null, loading: false, list: [{
    assistant_id: "assistant-archived", name: "retained", graph: "react_agent",
    archived_at: "2026-08-10T00:00:00Z", config: {}, metadata: {},
  }] };
  N.store.navigationPending = route;
  vm.runInContext("renderMain = () => {}; agentsRender = () => {};", sandbox);
  const opened = await N.navigationApplyPending({ focus: false });
  check("agent target: retained evidence synchronizes the visible lifecycle filter",
    opened && N.store.agents.selected === "assistant-archived" &&
    N.store.agentLifecycleFilter === "all" && element("sel-agent-lifecycle").value === "all");
}

check("markup: global copy action states the content-free privacy boundary",
  page.includes('id="btn-copy-view-link"') && page.includes('aria-describedby="view-link-status"') &&
  page.includes("no server address, key, prompt, or payload") && page.includes('id="view-link-announcer"'));
{
  const panel = element("view-link-message");
  N.navigationRouteMessage("Connect to the intended Rusty server to open this run evidence.");
  const visible = !panel.hidden && panel.textContent.includes("Connect to the intended Rusty server");
  N.navigationRouteMessage();
  check("route status: blocked shared evidence stays visibly actionable in the workspace",
    visible && panel.hidden && page.includes('id="view-link-message" class="view-link-message" role="note"') &&
    !page.includes('id="view-link-message" class="view-link-message" role="status"'));
}
check("responsive: the evidence-link control remains available at narrow width",
  page.includes(".view-link-control button { padding: 6px 8px; font-size: 0; }") &&
  page.includes(".view-link-control button::before"));
check("bootstrap: startup, popstate, copy, and post-connect reconciliation are wired",
  source.includes("navigationPrepareLocation();") &&
  source.includes('window.addEventListener("popstate"') &&
  source.includes('$("btn-copy-view-link").onclick = navigationCopyCurrent') &&
  source.includes("navigationApplyPending({ focus: false });"));

console.log(`\n${passed} passed, ${failed} failed`);
if (failed) process.exit(1);
