#!/usr/bin/env node
/* Mission Brief contracts: bounded page-memory authoring, exact agent/source
 * ownership, prepare-only thread creation, ambiguous receipt recovery, and
 * deliberate transfer into the existing local run surface.
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

const localData = new Map();
const elements = new Map();
const payload = { value: "", focused: 0, focus() { this.focused += 1; } };
elements.set("inp-payload", payload);
elements.set("studio-journey-announcer", { textContent: "" });
const sandbox = {
  URL, URLSearchParams, TextEncoder, TextDecoder, CSS: { escape: (value) => String(value) },
  confirm: () => true,
  setTimeout: () => 1, clearTimeout() {},
  localStorage: {
    getItem: (key) => localData.has(key) ? localData.get(key) : null,
    setItem: (key, value) => { localData.set(key, String(value)); },
  },
  document: { activeElement: null, getElementById: (id) => elements.get(id) || null },
};
vm.createContext(sandbox);
vm.runInContext(source + `
globalThis.__api = async () => { throw new Error("API stub not installed"); };
globalThis.__registryLoad = async () => true;
globalThis.__toasts = [];
apiForConnection = (...args) => globalThis.__api(...args);
registryLoad = (...args) => globalThis.__registryLoad(...args);
journeyRender = () => journeySnapshot(store);
renderThreads = () => {};
renderMain = () => {};
agentsRender = () => {};
registryBindingRender = () => {};
navigationReplaceManagedAddress = () => true;
toast = (message, kind) => globalThis.__toasts.push({ message, kind });
globalThis.__brief = { store, missionBriefRunnableAgents, missionBriefWindow, missionBriefEnsure,
  missionBriefValidation, missionBriefHtml, missionBriefEdit, missionBriefSubmit,
  missionBriefOpenPrepared, missionBriefAttachUncertain, missionBriefAbandonUncertain,
  missionBriefThreadReceipt, journeySnapshot, journeyMissionDossier, connectionResetWorkspace };
`, sandbox, { filename: "index.html<script>" });
const B = sandbox.__brief;

let passed = 0, failed = 0;
function check(name, condition, detail = "") {
  if (condition) { passed++; console.log(`ok   ${name}`); }
  else { failed++; console.log(`FAIL ${name}${detail ? ` — ${detail}` : ""}`); }
}
function agent(id = "agent-1", graph = "react_agent", extra = {}) {
  return { assistant_id: id, name: `Agent ${id}`, graph, active_version_id: `av-${id}`, config: null, metadata: null, ...extra };
}
function reset(agents = [agent()]) {
  Object.assign(B.store, {
    conn: { baseUrl: "http://tenant-a", apiKey: "secret-never-render" }, connectionEpoch: 7,
    info: { service: "rusty-agent-server", version: "0.12.0", graphs: [{ name: "react_agent" }, { name: "workflow" }] },
    view: "home", selected: null, threads: [], recorder: null, qualityCase: null,
    agents: { list: agents, selected: agents[0]?.assistant_id || null }, agentsUnsupported: false,
    agentRuns: Object.create(null), agentRunHistory: Object.create(null),
    registry: null, registryBindings: Object.create(null), missionBrief: null, missionBriefRequest: 0,
  });
  payload.value = ""; payload.focused = 0; sandbox.__toasts.length = 0;
  sandbox.__api = async () => { throw new Error("API stub not installed"); };
  sandbox.__registryLoad = async () => true;
  return B.missionBriefEnsure({ agentId: agents[0]?.assistant_id || null }, B.store);
}

{
  const archived = agent("archived", "react_agent", { archived_at: "2026-08-10T10:00:00Z" });
  const removed = agent("removed", "missing_graph");
  const draft = reset([agent(), archived, removed]);
  check("brief eligibility: only active agents backed by a live graph can own a mission",
    B.missionBriefRunnableAgents(B.store).map((item) => item.assistant_id).join() === "agent-1" && draft.agentId === "agent-1");
  draft.objective = "Investigate the customer issue and return cited evidence.";
  const valid = B.missionBriefValidation(draft, B.store);
  check("brief input: a conversational mission becomes the exact react-agent input envelope",
    valid.error === "" && valid.input.messages[0].content === draft.objective);
  draft.objective = " ";
  check("brief input: empty outcomes fail before any thread mutation",
    B.missionBriefValidation(draft, B.store).field === "objective");
}

{
  reset();
  B.store.view = "agents";
  B.store.selected = "old-thread";
  B.store.threads = [{ thread_id: "old-thread", graph: "react_agent", metadata: { assistant_id: "other-agent" } }];
  B.store.agentRunHistory["other-agent"] = [{ run_id: "old-run", thread_id: "old-thread", graph: "react_agent", status: "success" }];
  B.store.recorder = { runId: "old-run", exactEnvelope: true, complete: true, error: null,
    events: [{ run_id: "old-run", thread_id: "old-thread" }] };
  const snapshot = B.journeySnapshot(B.store), dossier = B.journeyMissionDossier(snapshot, B.store);
  check("brief context: an unrelated remembered thread and journal cannot eclipse the selected agent's Shape-to-Run decision",
    snapshot.agentId === "agent-1" && !snapshot.threadId && !snapshot.runId && !snapshot.exactRecorder && dossier.key === "run");
}

{
  reset();
  B.store.view = "agents";
  B.store.selected = "conflicted-thread";
  B.store.threads = [{ thread_id: "conflicted-thread", graph: "react_agent", metadata: { assistant_id: "agent-1" } }];
  B.store.agentRunHistory["other-agent"] = [{ run_id: "other-run", thread_id: "conflicted-thread", graph: "react_agent", status: "success" }];
  B.store.recorder = { runId: "other-run", exactEnvelope: true, complete: true, error: null,
    events: [{ run_id: "other-run", thread_id: "conflicted-thread" }] };
  const snapshot = B.journeySnapshot(B.store);
  check("brief context: conflicting thread metadata and run ownership fail closed instead of binding another agent's evidence",
    snapshot.agentId === "agent-1" && !snapshot.threadId && !snapshot.runId && !snapshot.exactRecorder);
}

{
  const agents = Array.from({ length: 150 }, (_, index) => agent(`agent-${index}`));
  reset(agents);
  const shown = B.missionBriefWindow(agents, "agent-149");
  check("brief bounds: the chooser is capped while retaining an exact selected agent outside the leading window",
    shown.length === 120 && shown.some((item) => item.assistant_id === "agent-149"));
}

{
  const draft = reset([agent("workflow-1", "workflow")]);
  draft.objective = '{"goal":"ship"}';
  check("brief structured input: non-chat graphs require and preserve an object",
    B.missionBriefValidation(draft, B.store).input.goal === "ship");
  draft.objective = "ship it";
  check("brief structured input: invalid JSON remains a field error instead of reaching the server",
    B.missionBriefValidation(draft, B.store).field === "objective");
  draft.objective = '{"count":9007199254740993}';
  check("brief structured input: a JSON integer the browser would round is rejected before transfer",
    B.missionBriefValidation(draft, B.store).error.includes("cannot transfer exactly"));
  draft.objective = '{"value":"\\ud800"}';
  check("brief structured input: serde-incompatible lone surrogates fail before transfer",
    B.missionBriefValidation(draft, B.store).error.includes("Unicode scalar"));
}

{
  const draft = reset([agent("workflow-errors", "workflow")]);
  draft.error = "Describe the outcome this mission should produce.";
  draft.errorField = "objective";
  const alert = { textContent: draft.error, remove() {} };
  elements.set("mission-brief-error", alert);
  const attrs = new Map();
  const target = { value: "{", matches: (selector) => selector === "[data-mission-objective]",
    setAttribute: (key, value) => attrs.set(key, value), removeAttribute: (key) => attrs.delete(key) };
  B.missionBriefEdit(target);
  check("brief validation: invalid-to-different-invalid edits update the visible associated diagnosis",
    draft.errorField === "objective" && alert.textContent.includes("not valid JSON"));
  elements.delete("mission-brief-error");
}

{
  const metadata = { source: "studio_mission_brief", assistant_id: "agent-1", assistant_version_id: "av-agent-1", governed_preset: false };
  const receipt = { thread_id: "thread-1", tenant: "default", graph: "react_agent", metadata, created_at: "2026-08-10T20:00:00Z" };
  check("brief receipt: exact Rust ThreadRecord shape is required and extra local-storage payloads are rejected",
    Boolean(B.missionBriefThreadReceipt(receipt, "thread-1", "react_agent", metadata)) &&
    !B.missionBriefThreadReceipt(({ ...receipt, tenant: undefined }), "thread-1", "react_agent", metadata) &&
    !B.missionBriefThreadReceipt(({ ...receipt, extra: "not-on-the-wire" }), "thread-1", "react_agent", metadata));
  check("brief receipt: tenant identity mirrors Rust's bounded ASCII grammar and reserved-layout refusal",
    ["", "bad/tenant", "ténant", "threads", "x".repeat(65)].every((tenant) =>
      !B.missionBriefThreadReceipt(({ ...receipt, tenant }), "thread-1", "react_agent", metadata)) &&
    Boolean(B.missionBriefThreadReceipt(({ ...receipt, tenant: "acme.prod_1" }), "thread-1", "react_agent", metadata)));
}

{
  const draft = reset(); draft.objective = "Prepare a concise launch brief.";
  const calls = [];
  sandbox.__api = async (_connection, method, route, body, maxResponseBytes, _text, includeStatus) => {
    calls.push({ method, route, body, maxResponseBytes, includeStatus });
    return { status: 201, body: { thread_id: body.thread_id, tenant: "default", graph: body.graph, metadata: body.metadata, created_at: "2026-08-10T20:00:00Z" } };
  };
  const prepared = await B.missionBriefSubmit();
  check("brief handoff: an exact HTTP 201 receipt creates one thread and never starts a run",
    prepared && calls.length === 1 && calls[0].method === "POST" && calls[0].route === "/threads" && calls[0].maxResponseBytes > 0 && calls[0].includeStatus === true &&
    !calls.some((call) => call.route.includes("/runs")) && B.store.threads.length === 1);
  check("brief handoff: the existing local run form receives the exact assistant/input payload",
    B.store.view === "thread" && B.store.selected === B.store.threads[0].thread_id && JSON.parse(payload.value).input.messages[0].content === draft.objective && payload.focused === 1);
  const handedOff = B.journeySnapshot(B.store);
  check("brief continuity: the prepared thread metadata keeps its exact agent/version context before a run exists",
    handedOff.agentId === "agent-1" && handedOff.versionId === "av-agent-1" && handedOff.threadId === B.store.selected && !handedOff.runId);
  check("brief privacy: the durable thread metadata carries identity and provenance, never the objective",
    calls[0].body.metadata.source === "studio_mission_brief" && !JSON.stringify(calls[0].body.metadata).includes("launch brief"));
}

{
  const governed = agent("governed", "react_agent", { config: { studio_intent: {
    format: "rusty.agent-intent/v3", model: "model-x", tools: [], memory: { access: "none", scopes: [] },
    approval: "runtime_policy", output: { mode: "runtime_default", schema: "" },
    budget: { max_tokens: "", max_cost_usd: "", max_latency_ms: "" },
    binding: { environment: "prod", surfaces: ["prompt:system"] },
  } } });
  const draft = reset([governed]); draft.objective = "Use the reviewed system prompt.";
  let registryReads = 0;
  sandbox.__registryLoad = async () => {
    registryReads += 1;
    B.store.registry = { loading: false, error: null, artifacts: [
      { surface: "prompt:system", family: "prompt", name: "system", commits: [{ candidate_id: "cand-1" }] },
    ] };
    return true;
  };
  sandbox.__api = async (_connection, _method, _route, body) => ({ status: 201,
    body: { thread_id: body.thread_id, tenant: "default", graph: body.graph, metadata: body.metadata, created_at: "2026-08-10T20:00:00Z" } });
  const prepared = await B.missionBriefSubmit();
  const binding = B.store.registryBindings[B.store.selected];
  check("brief governance: a stored preset forces one fresh live catalog read before thread creation",
    prepared && registryReads === 1 && binding && binding.environment === "prod" && binding.surfaces.join() === "prompt:system");
  check("brief governance: the transferred planner remains unacknowledged and cannot silently admit a run",
    binding.acknowledged === false && binding.lastSubmission === null);
}

{
  const governed = agent("governed-drift", "react_agent", { config: { studio_intent: {
    format: "rusty.agent-intent/v3", model: "model-x", tools: [], memory: { access: "none", scopes: [] },
    approval: "runtime_policy", output: { mode: "runtime_default", schema: "" },
    budget: { max_tokens: "", max_cost_usd: "", max_latency_ms: "" },
    binding: { environment: "prod", surfaces: ["prompt:system"] },
  } } });
  const draft = reset([governed]); draft.objective = "Prepare from current source truth.";
  let releaseCatalog;
  sandbox.__registryLoad = () => new Promise((resolve) => { releaseCatalog = resolve; });
  const pending = B.missionBriefSubmit();
  B.store.agents.list = [{ ...governed, active_version_id: "av-new-version" }];
  releaseCatalog(true);
  const prepared = await pending;
  check("brief ownership: governed source drift releases busy state and requires review instead of stranding the action",
    !prepared && draft.submitting === false && draft.error.includes("agent changed"));
}

{
  const draft = reset(); draft.objective = "Do work exactly once.";
  let calls = 0;
  sandbox.__api = async (_connection, _method, _route, body) => {
    calls += 1;
    return { status: 201, body: { thread_id: body.thread_id, tenant: "default", graph: "wrong_graph", metadata: body.metadata, created_at: "2026-08-10T20:00:00Z" } };
  };
  const prepared = await B.missionBriefSubmit();
  const retried = await B.missionBriefSubmit();
  check("brief retry safety: a malformed successful receipt locks the stable ID instead of fabricating proof",
    !prepared && !retried && calls === 1 && draft.uncertainThreadId.startsWith("studio-run-") && draft.error.includes("cannot prove"));
  check("brief retry safety: the recovery copy states no run started and exposes deliberate attach/abandon actions",
    B.missionBriefHtml({ agentId: "agent-1" }, B.store).includes("No run was started") &&
    B.missionBriefHtml({ agentId: "agent-1" }, B.store).includes("data-mission-attach") &&
    B.missionBriefHtml({ agentId: "agent-1" }, B.store).includes("data-mission-abandon"));
}

{
  const draft = reset(); draft.objective = "Prepare, but respect my newer navigation.";
  let resolve;
  sandbox.__api = (_connection, _method, _route, body) => new Promise((done) => {
    resolve = () => done({ status: 201, body: { thread_id: body.thread_id, tenant: "default", graph: body.graph, metadata: body.metadata, created_at: "2026-08-10T20:00:00Z" } });
  });
  const pending = B.missionBriefSubmit();
  B.store.view = "tasks";
  resolve();
  const prepared = await pending;
  check("brief ownership: a late exact receipt updates thread truth without taking over a newer workspace",
    prepared && B.store.view === "tasks" && B.store.selected === null && B.store.threads.length === 1 && draft.preparedThreadId === B.store.threads[0].thread_id && payload.value === "");
  const opened = B.missionBriefOpenPrepared(draft, false);
  check("brief continuation: the operator can deliberately transfer a proven prepared thread later",
    opened && B.store.view === "thread" && JSON.parse(payload.value).input.messages[0].content.includes("newer navigation"));
}

{
  const draft = reset(); draft.objective = "Keep this private.";
  const rendered = B.missionBriefHtml({ agentId: "agent-1" }, B.store);
  check("brief lifecycle: visible copy names page-memory loss and explicitly excludes URL and thread metadata",
    rendered.includes("Page memory") && rendered.includes("not placed in the URL or thread metadata") && rendered.includes("reloading discards the draft"));
  check("brief lifecycle: connection reset invalidates the draft and its pending generation",
    /function connectionResetWorkspace\(\) \{\s*store\.connectionEpoch \+= 1;\s*store\.missionBriefRequest \+= 1;\s*store\.missionBrief = null;/.test(source));
}

check("brief accessibility: labels, stable field errors, busy state, and a prepare-only action are present",
  html.includes('data-mission-brief-form aria-busy=') && html.includes('aria-describedby="mission-brief-error"') &&
  html.includes("Prepare mission — no run starts") && html.includes('data-mission-focus="objective"') &&
  html.includes('aria-disabled="${draft.submitting || locked || !agents.length}"${locked || !agents.length ? " disabled" : ""}'));
check("brief responsive: the two-field composer deliberately collapses to one column with full-width actions",
  html.includes(".mission-brief-form { grid-template-columns: 1fr;") &&
  html.includes(".mission-brief-boundary button, .mission-brief-prepared button { width: 100%; }"));
check("brief provider boundary: visible copy warns that an eventual run can reach configured providers and evidence",
  html.includes("Running can send it to configured models/tools and journal evidence") && html.includes("never use this field as a credential store"));
check("brief docs: product and roadmap describe prepare-only continuity and the page-memory privacy boundary",
  docs.includes("Mission Brief") && docs.includes("never starts the run") && docs.includes("thread metadata") &&
  roadmap.includes("Mission Brief") && /existing\s+local run surface/.test(roadmap));

if (failed) {
  console.error(`\nFAIL: ${failed} failed, ${passed} passed`);
  process.exit(1);
}
console.log(`\nPASS: ${passed} Studio Mission Brief assertions`);
