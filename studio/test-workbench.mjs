#!/usr/bin/env node
/* Node unit tests for the pure Agent Workbench helpers embedded in
 * studio/index.html. The browser bootstrap is removed and the helpers run
 * dependency-free under vm, matching the recorder and task test harnesses.
 *
 *   node studio/test-workbench.mjs
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

const localData = new Map();
let quotaFailures = 0;
let storageWrites = 0;
let fetchFailure = null;
const fetchCalls = [];
const uiElements = new Map();
const sandbox = {
  async fetch(url, options) {
    fetchCalls.push({ url, options });
    if (fetchFailure) {
      return {
        ok: false, status: fetchFailure.status,
        async text() { return JSON.stringify(fetchFailure.body); },
      };
    }
    return { ok: true, status: 200, async text() { return '{"service":"rusty"}'; } };
  },
  document: { getElementById(id) { return uiElements.get(id) || null; } },
  setTimeout() { return 1; },
  clearTimeout() {},
  localStorage: {
    getItem(key) { return localData.has(key) ? localData.get(key) : null; },
    setItem(key, value) {
      storageWrites++;
      if (quotaFailures > 0) { quotaFailures--; throw new Error("quota exceeded"); }
      localData.set(key, String(value));
    },
  },
};
vm.createContext(sandbox);
vm.runInContext(src + `
globalThis.__workbench = {
  connectionIdentityChanged, connectionAfterAttempt, apiForConnection,
  connectionRunScope,
  agentParseRunEnvelope, agentPruneRunEnvelope, loadAgentRunHistory, saveAgentRunHistory,
  agentTags, agentSearchItems, agentReadiness, agentReadinessHtml,
  agentCardHtml, agentGraphLabel, agentDefaultInput, agentBuildRunInput,
  agentRunTone, agentErrorCategory, agentNormalizeRunRecord, agentMergeRunHistory,
  agentDurationLabel, agentRunTimeLabel, agentRunHistoryHtml,
  agentJourney, agentJourneyHtml, agentCopyableValue, agentCopyableMap, agentCopyDraft,
  agentSensitiveKey, agentRedactedValue, agentCopyManifestText, agentCopyContextHtml,
  agentCopyIdentityConflict, agentBuildCreatePayload,
  agentErrorHtml, agentTestResultHtml, agentRunAnnouncement, agentDetailHtml,
  agentCreate, store,
};`, sandbox, { filename: "index.html<script>" });

const W = sandbox.__workbench;
const info = { graphs: [{ name: "pipeline" }, { name: "react_agent" }] };
const agent = {
  assistant_id: "research-coordinator",
  name: "Research <Coordinator>",
  graph: "react_agent",
  config: { recursion_limit: 12 },
  metadata: { description: "Collect and synthesize evidence", tags: ["research", "production"] },
  created_at: "2026-08-09T12:00:00Z",
};

let passed = 0, failed = 0;
function check(name, condition, detail) {
  if (condition) { passed++; console.log(`ok   ${name}`); }
  else { failed++; console.log(`FAIL ${name}${detail ? ` — ${detail}` : ""}`); }
}
function eq(name, got, want) {
  check(name, JSON.stringify(got) === JSON.stringify(want),
    `got ${JSON.stringify(got)}, want ${JSON.stringify(want)}`);
}

check("connection scope: same server and tenant retains session evidence",
  W.connectionIdentityChanged({ baseUrl: "/api", apiKey: "tenant-a" }, { baseUrl: "/api", apiKey: "tenant-a" }) === false);
check("connection scope: changing server clears session evidence",
  W.connectionIdentityChanged({ baseUrl: "/api", apiKey: "tenant-a" }, { baseUrl: "http://other", apiKey: "tenant-a" }) === true);
check("connection scope: changing tenant key clears session evidence",
  W.connectionIdentityChanged({ baseUrl: "/api", apiKey: "tenant-a" }, { baseUrl: "/api", apiKey: "tenant-b" }) === true);
{
  const tenantA = { baseUrl: "/api", apiKey: "tenant-a" };
  const tenantB = { baseUrl: "/api", apiKey: "tenant-b" };
  const afterFailure = W.connectionAfterAttempt(tenantA, tenantB, false);
  const afterRetry = W.connectionAfterAttempt(afterFailure, tenantB, true);
  check("connection scope: failed tenant switch rolls back before retry",
    afterFailure === tenantA && W.connectionIdentityChanged(afterFailure, afterRetry));
  W.store.conn = tenantA;
  await W.apiForConnection(tenantB, "GET", "/info");
  check("connection scope: candidate validation does not mutate the active tenant",
    W.store.conn === tenantA && fetchCalls.at(-1).url === "/api/info" &&
    fetchCalls.at(-1).options.headers["X-Api-Key"] === "tenant-b");
}
check("run ledger scope: same connection identity is stable",
  W.connectionRunScope({ baseUrl: "/api", apiKey: "tenant-a" }) ===
  W.connectionRunScope({ baseUrl: "/api", apiKey: "tenant-a" }));
check("run ledger scope: tenant keys do not share history",
  W.connectionRunScope({ baseUrl: "/api", apiKey: "tenant-a" }) !==
  W.connectionRunScope({ baseUrl: "/api", apiKey: "tenant-b" }));
check("run ledger scope: API key is not copied into the storage namespace",
  !W.connectionRunScope({ baseUrl: "/api", apiKey: "secret-tenant-key" }).includes("secret-tenant-key"));

{
  check("run ledger storage: malformed JSON becomes an empty envelope",
    Object.keys(W.agentParseRunEnvelope("{broken").scopes).length === 0);
  check("run ledger storage: wrong top-level types become an empty envelope",
    Object.keys(W.agentParseRunEnvelope("[]").scopes).length === 0 &&
    Object.keys(W.agentParseRunEnvelope('"wrong"').scopes).length === 0);
  const envelope = { version: 1, scopes: {} };
  for (let scope = 0; scope < 10; scope++) {
    const assistants = {};
    for (let assistant = 0; assistant < 12; assistant++) {
      assistants[`agent-${scope}-${assistant}`] = {
        touched_at: scope * 100 + assistant,
        runs: [{ record_id: `run-${scope}-${assistant}`, status: "success" }],
      };
    }
    envelope.scopes[`scope-${scope}`] = { touched_at: scope, assistants };
  }
  const bounded = W.agentPruneRunEnvelope(envelope, "scope-0", "agent-0-0", 196608);
  const assistantCount = Object.values(bounded.scopes)
    .reduce((total, scope) => total + Object.keys(scope.assistants).length, 0);
  check("run ledger storage: scope and global assistant counts are bounded",
    Object.keys(bounded.scopes).length <= 8 && assistantCount <= 80 && bounded.scopes["scope-0"]);
}

{
  W.store.conn = { baseUrl: "/api", apiKey: "tenant-storage" };
  W.store.agentRunHistory = { agent: [{ record_id: "run-storage", status: "success" }] };
  for (const corrupt of ["{broken", "[]", '"wrong"']) {
    localData.set("ags:agentRuns", corrupt);
    check(`run ledger storage: a corrupt ${corrupt[0]} value self-recovers on save`,
      W.saveAgentRunHistory("agent") && W.agentParseRunEnvelope(localData.get("ags:agentRuns")).version === 1);
  }
  quotaFailures = 1;
  storageWrites = 0;
  check("run ledger storage: quota failure prunes and retries once",
    W.saveAgentRunHistory("agent") && storageWrites === 2);
}

eq("tags: array is preserved", W.agentTags(agent), ["research", "production"]);
eq("tags: comma-separated legacy value is normalized",
  W.agentTags({ metadata: { tags: "research, production" } }), ["research", "production"]);
eq("tags: missing metadata is defensive", W.agentTags({}), []);

check("search: name is case-insensitive", W.agentSearchItems([agent], "COORD").length === 1);
check("search: graph is searchable", W.agentSearchItems([agent], "react_agent").length === 1);
check("search: id is searchable", W.agentSearchItems([agent], "research-co").length === 1);
check("search: tags are searchable", W.agentSearchItems([agent], "production").length === 1);
check("search: non-match is excluded", W.agentSearchItems([agent], "billing").length === 0);

{
  const readiness = W.agentReadiness(agent, info, null);
  eq("readiness: durable + registered graph gives two steps", readiness.steps.map((s) => s.ready), [true, true, false]);
  check("readiness: score is 2/3 before a test", readiness.ready === 2 && readiness.total === 3);
  const tested = W.agentReadiness(agent, info, { status: "success" });
  check("readiness: successful runtime evidence completes the rail", tested.ready === 3);
  const failed = W.agentReadiness(agent, info, { status: "error" });
  check("readiness: failed test does not mark tested", failed.ready === 2);
  const missingGraph = W.agentReadiness({ ...agent, graph: "missing" }, info, null);
  check("readiness: unregistered graph is not runnable", missingGraph.steps[1].ready === false);
}

{
  const rail = W.agentReadinessHtml(W.agentReadiness(agent, info, null), true);
  check("readiness rail has three semantic labels",
    rail.includes("Defined") && rail.includes("Runnable") && rail.includes("Tested"));
  check("readiness rail marks only ready segments", (rail.match(/readiness-step ready/g) || []).length === 2);
}

{
  const card = W.agentCardHtml(agent, W.agentReadiness(agent, info, null));
  check("catalog card carries name, graph, id, and description",
    card.includes("Research") && card.includes("react_agent") && card.includes("research-coordinator") && card.includes("synthesize"));
  check("catalog card escapes the agent name", card.includes("&lt;Coordinator&gt;") && !card.includes("<Coordinator>"));
}

eq("default input: pipeline is empty object", W.agentDefaultInput("pipeline"), {});
check("default input: ReAct is immediately runnable",
  W.agentDefaultInput("react_agent").messages[0].content === "Reply with a short hello.");
check("graph labels: built-in behaviors are understandable",
  W.agentGraphLabel("react_agent") === "Conversational agent" && W.agentGraphLabel("pipeline") === "Workflow pipeline");

{
  eq("run input: conversational task becomes a user message",
    W.agentBuildRunInput(agent, "  Find three sources.  "),
    { messages: [{ role: "user", content: "Find three sources." }] });
  check("run input: conversational task cannot be empty", (() => {
    try { W.agentBuildRunInput(agent, "  "); return false; }
    catch (error) { return error.message === "Enter a task for the agent."; }
  })());
  eq("run input: workflow data remains structured",
    W.agentBuildRunInput({ graph: "pipeline" }, '{"topic":"rust"}'), { topic: "rust" });
  for (const value of ["[]", "null", '"topic"', "7"]) {
    check(`run input: workflow rejects non-object ${value}`, (() => {
      try { W.agentBuildRunInput({ graph: "pipeline" }, value); return false; }
      catch (error) { return error.message === "Input data must be a JSON object."; }
    })());
  }
  check("run input: malformed workflow data has a plain error", (() => {
    try { W.agentBuildRunInput({ graph: "pipeline" }, "{"); return false; }
    catch (error) { return error.message.includes("Input data is not valid JSON"); }
  })());
}

{
  eq("journey: empty workbench starts at create",
    W.agentJourney(null, null).map((step) => step.state), ["active", "pending", "pending"]);
  eq("journey: selected agent advances to run",
    W.agentJourney(agent, null).map((step) => step.state), ["complete", "active", "pending"]);
  eq("journey: successful run exposes inspection",
    W.agentJourney(agent, { status: "success", thread_id: "thread-42" }).map((step) => step.state),
    ["complete", "complete", "active"]);
  eq("journey: failed run is visibly actionable without inventing a trace",
    W.agentJourney(agent, { status: "error" }).map((step) => step.state),
    ["complete", "attention", "pending"]);
  eq("journey: opening creation always returns focus to create",
    W.agentJourney(agent, { status: "success", thread_id: "thread-42" }, true).map((step) => step.state),
    ["active", "pending", "pending"]);
  check("journey: a thread is inspectable without claiming recorder availability",
    W.agentJourney(agent, { status: "success", thread_id: "thread-42" })[2].detail === "Run ready to inspect");
  const journey = W.agentJourneyHtml({ ...agent, name: "Research <Coordinator>" }, null);
  check("journey: labels the real sequence and escapes names",
    journey.includes("Create") && journey.includes("Run") && journey.includes("Inspect") && journey.includes("&lt;Coordinator&gt;"));
}

{
  const payload = W.agentBuildCreatePayload({
    name: "  Research coordinator ", graph: "react_agent", assistantId: "research-1",
    recursionLimit: "14", description: "  Evidence work ", tags: "research, production, research",
  });
  check("create payload trims identity fields", payload.name === "Research coordinator" && payload.assistant_id === "research-1");
  eq("create payload maps the runtime recursion limit", payload.config, { recursion_limit: 14 });
  eq("create payload normalizes metadata", payload.metadata,
    { description: "Evidence work", tags: ["research", "production", "research"] });
  const minimal = W.agentBuildCreatePayload({ name: "A", graph: "pipeline" });
  check("create payload omits empty optional objects", !("config" in minimal) && !("metadata" in minimal) && !("assistant_id" in minimal));
}

{
  const source = {
    ...agent,
    name: "Research <Coordinator>",
    config: { recursion_limit: 12, temperature: 0.2, runtime: { mode: "careful" } },
    metadata: { description: "Collect evidence", tags: ["research", "production"], owner: "quality" },
  };
  const before = JSON.stringify(source);
  const draft = W.agentCopyDraft(source);
  check("copy draft: makes the new identity explicit and preserves editable fields",
    draft.name === "Copy of Research <Coordinator>" && draft.assistantId === "" &&
    draft.graph === "react_agent" && draft.recursionLimit === "12" &&
    draft.description === "Collect evidence" && draft.tags === "research, production");
  const copied = W.agentBuildCreatePayload({
    ...draft, name: "Research coordinator v2", recursionLimit: "18",
    description: "Review evidence", tags: "research, review",
  }, source);
  check("copy payload: carries unknown stored configuration without mutating the source",
    copied.config.temperature === 0.2 && copied.config.runtime.mode === "careful" &&
    copied.config.recursion_limit === 18 && copied.metadata.owner === "quality" &&
    copied.metadata.description === "Review evidence" && JSON.stringify(source) === before);
  eq("copy payload: known metadata changes are deliberate",
    copied.metadata.tags, ["research", "review"]);
  const cleared = W.agentBuildCreatePayload({ ...draft, recursionLimit: "", description: "", tags: "" }, source);
  check("copy payload: clearing known fields keeps unrelated configuration",
    !("recursion_limit" in cleared.config) && cleared.config.temperature === 0.2 &&
    !("description" in cleared.metadata) && !("tags" in cleared.metadata) && cleared.metadata.owner === "quality");
  check("copy payload: source identity is never inherited",
    !("assistant_id" in copied) && W.agentCopyIdentityConflict(source, { assistant_id: source.assistant_id }));
  check("copy payload: a generated or distinct identity is accepted",
    !W.agentCopyIdentityConflict(source, {}) &&
    !W.agentCopyIdentityConflict(source, { assistant_id: "research-coordinator-v2" }));
  const receipt = W.agentCopyContextHtml({ ...source, config: { ...source.config, credential: "must-not-render" } });
  check("copy receipt: explains isolation, escapes identity, and does not expose stored values",
    receipt.includes("Source agent → new draft") && receipt.includes("Research &lt;Coordinator&gt;") &&
    receipt.includes("research-coordinator") && receipt.includes("source agent") &&
    receipt.includes("stay unchanged") && receipt.includes("Review carried manifest") &&
    receipt.includes("[hidden]") && !receipt.includes("must-not-render"));
  const manifest = W.agentCopyManifestText({ config: { api_key: "private", model: "local" }, metadata: { owner: "quality" } });
  check("copy receipt: advanced manifest is readable and redacts sensitive-looking values",
    manifest.includes('"model": "local"') && manifest.includes('"owner": "quality"') &&
    manifest.includes('"api_key": "[hidden]"') && !manifest.includes("private"));
  const aliases = W.agentCopyManifestText({
    config: {
      auth: "Basic private", headers: { authentication: "Bearer private", authorization: "Bearer private", cookie: "session=private" },
      private_key: "private",
    },
    metadata: { connection_string: "private", session_id: "private", public_label: "visible" },
  });
  check("copy receipt: nested and common credential aliases are redacted",
    !aliases.includes("Bearer private") && !aliases.includes("session=private") &&
    !aliases.includes('"private"') && aliases.includes('"public_label": "visible"') &&
    W.agentSensitiveKey("auth") && W.agentSensitiveKey("authentication") &&
    W.agentSensitiveKey("set-cookie") && W.agentSensitiveKey("accessKey"));
  const deep = { a: { b: { c: { d: { e: { f: { g: { h: "hidden" } } } } } } } };
  check("copy receipt: deeply nested and oversized values are bounded",
    W.agentRedactedValue(deep).a.b.c.d.e.f.g.h === "[depth limit]" &&
    W.agentCopyManifestText({ config: { note: "x".repeat(2000) } }, 512).endsWith("… preview truncated"));
  check("copy helpers: the editable map view remains defensive for non-object values",
    Object.keys(W.agentCopyableMap([])).length === 0 && Object.keys(W.agentCopyableMap("bad")).length === 0);
  const arbitrary = { ...source, config: ["model", { retries: 2 }], metadata: "opaque metadata" };
  const arbitraryBefore = JSON.stringify(arbitrary);
  const arbitraryCopy = W.agentBuildCreatePayload({
    name: "Exact JSON copy", graph: "react_agent", assistantId: "",
    recursionLimit: "", description: "", tags: "",
  }, arbitrary);
  eq("copy payload: array config is preserved exactly", arbitraryCopy.config, arbitrary.config);
  eq("copy payload: scalar metadata is preserved exactly", arbitraryCopy.metadata, arbitrary.metadata);
  check("copy payload: arbitrary JSON round-trip does not mutate its source",
    JSON.stringify(arbitrary) === arbitraryBefore &&
    W.agentCopyManifestText(arbitrary).includes("opaque metadata"));
}

{
  function element(value = "") {
    return {
      value, disabled: false, style: {}, className: "", textContent: "", focused: false,
      focus() { this.focused = true; },
      removeAttribute(name) { delete this[name]; },
    };
  }
  uiElements.clear();
  for (const [id, value] of Object.entries({
    "inp-agent-name": "Copy draft", "sel-agent-graph": "react_agent",
    "inp-agent-id": agent.assistant_id, "inp-agent-limit": "12",
    "inp-agent-description": "Collect evidence", "inp-agent-tags": "research",
    "btn-agent-create": "", toast: "",
  })) uiElements.set(id, element(value));
  W.store.agentCopySource = JSON.parse(JSON.stringify(agent));
  const callsBeforeConflict = fetchCalls.length;
  await W.agentCreate();
  check("copy interaction: source identity is rejected before any POST",
    fetchCalls.length === callsBeforeConflict && uiElements.get("inp-agent-id").focused &&
    uiElements.get("toast").textContent.includes("source agent stays unchanged"));
  uiElements.get("inp-agent-id").value = "research-copy";
  uiElements.get("inp-agent-id").focused = false;
  fetchFailure = { status: 409, body: { message: "assistant already exists" } };
  await W.agentCreate();
  fetchFailure = null;
  check("copy interaction: server failure preserves the draft and source context",
    fetchCalls.length === callsBeforeConflict + 1 && W.store.agentCopySource.assistant_id === agent.assistant_id &&
    uiElements.get("inp-agent-name").value === "Copy draft" &&
    uiElements.get("inp-agent-id").value === "research-copy" &&
    uiElements.get("btn-agent-create").disabled === false &&
    uiElements.get("toast").textContent === "assistant already exists");
}

check("route-missing error explains the required server capability",
  W.agentErrorHtml(404, null).includes("no <code>/assistants</code> routes"));
check("server error message is escaped",
  W.agentErrorHtml(500, { message: "store <offline>" }).includes("&lt;offline&gt;"));

{
  const record = W.agentNormalizeRunRecord({
    record_id: "attempt-1", run_id: "run-1", thread_id: "thread-1", graph: "react_agent",
    status: "success", started_at: "2026-08-09T12:00:00Z", finished_at: "2026-08-09T12:00:02Z",
    duration_ms: 2345, error: "provider leaked a private task", error_class: "tool_error",
    prompt: "private task", result: { output: "private answer" },
  });
  check("run ledger: stores identity, status, graph, and timing",
    record.run_id === "run-1" && record.thread_id === "thread-1" && record.graph === "react_agent" && record.duration_ms === 2345);
  check("run ledger: strips prompts and result payloads",
    !("prompt" in record) && !("result" in record) && !("input" in record) && !("error" in record));
  check("run ledger: invalid records without an identity are discarded",
    W.agentNormalizeRunRecord({ status: "success" }) === null);
  check("run ledger: corrupt timestamps become unavailable instead of throwing",
    W.agentNormalizeRunRecord({ record_id: "bad-time", started_at: "yesterdayish" }).started_at === null);
  check("run ledger: only a stable error class survives sanitization",
    record.error_class === "tool_error" && !JSON.stringify(record).includes("private task"));
  check("run ledger: an unknown secret-bearing server error is classified, not persisted",
    W.agentErrorCategory({ status: 500, body: { error: "sk-live-secret", message: "private task" } }) === "http_500");
  check("run ledger: a safe HTTP category survives history normalization",
    W.agentNormalizeRunRecord({ record_id: "http", error_class: "http_500" }).error_class === "http_500");
  check("run ledger: network failures receive a stable category",
    W.agentErrorCategory({ status: 0, body: { message: "private host" } }) === "network_error");
}

{
  const old = {
    record_id: "run-old", run_id: "run-old", status: "error",
    started_at: "2026-08-09T11:00:00Z", duration_ms: 1000,
  };
  const current = {
    record_id: "run-current", run_id: "run-current", status: "success",
    started_at: "2026-08-09T12:00:00Z", duration_ms: 2000,
  };
  eq("run ledger: newest run is placed first", W.agentMergeRunHistory([old], current, 12).map((run) => run.run_id),
    ["run-current", "run-old"]);
  const updated = { ...current, status: "interrupted", duration_ms: 3000 };
  const merged = W.agentMergeRunHistory([current, old], updated, 12);
  check("run ledger: the same run updates instead of duplicating",
    merged.length === 2 && merged[0].status === "interrupted" && merged[0].duration_ms === 3000);
  check("run ledger: history is bounded",
    W.agentMergeRunHistory([current, old], { ...current, run_id: "run-new", record_id: "run-new" }, 2).length === 2);
}

check("run ledger: sub-second duration is readable", W.agentDurationLabel(250) === "<1s");
check("run ledger: seconds duration is readable", W.agentDurationLabel(2345) === "2.3s");
check("run ledger: minute duration is readable", W.agentDurationLabel(65000) === "1m 5s");
check("run ledger: invalid duration is explicit", W.agentDurationLabel(null) === "duration unavailable");
check("run ledger: invalid time is explicit", W.agentRunTimeLabel("invalid") === "time unavailable");

{
  const empty = W.agentRunHistoryHtml([]);
  check("run ledger: empty state invites the first real task", empty.includes("No runs from this browser yet"));
  const history = W.agentRunHistoryHtml([{
    record_id: "run-1", run_id: "run-1", thread_id: "thread-1", graph: "react_agent",
    status: "error", started_at: "2026-08-09T12:00:00Z", duration_ms: 1200,
    error: "private provider detail", error_class: "tool_error", result: { output: "must not render" },
  }]);
  check("run ledger: row exposes status, run, thread, and inspection",
    history.includes("error") && history.includes("run-1") && history.includes("thread-1") && history.includes(">Inspect</button>"));
  check("run ledger: inspection carries the graph needed to reattach a forgotten local thread",
    history.includes('data-agent-open-graph="react_agent"'));
  check("run ledger: only the safe error class renders and result payload is absent",
    history.includes("Tool error") && !history.includes("private provider detail") && !history.includes("must not render"));
  const failedStart = W.agentRunHistoryHtml([{
    record_id: "attempt-1", status: "error", started_at: "2026-08-09T12:00:00Z", error_class: "network_error",
  }]);
  check("run ledger: an attempt without a thread does not offer a false Inspect action",
    failedStart.includes("run did not start") && !failedStart.includes(">Inspect</button>"));
  const unattachable = W.agentRunHistoryHtml([{
    record_id: "run-no-graph", run_id: "run-no-graph", thread_id: "thread-no-graph", status: "success",
  }]);
  check("run ledger: a corrupt record without graph context does not offer a no-op Inspect action",
    !unattachable.includes(">Inspect</button>"));
}

{
  const run = { status: "success", run_id: "019157c4-6f1f-7a3b-8c2d-9e4f5a6b7c8d", thread_id: "thread-42", result: { status: "success" } };
  const history = [{ ...run, record_id: run.run_id, graph: agent.graph, started_at: "2026-08-09T12:00:00Z", duration_ms: 850 }];
  const detail = W.agentDetailHtml(agent, W.agentReadiness(agent, info, run), run, history);
  check("detail exposes durable identity and configuration",
    detail.includes("research-coordinator") && detail.includes("12") && detail.includes("production"));
  check("detail provides real run and open-thread actions",
    detail.includes('data-agent-run="research-coordinator"') &&
    detail.includes('data-agent-open-thread="thread-42"') &&
    detail.includes('data-agent-open-run="019157c4-6f1f-7a3b-8c2d-9e4f5a6b7c8d"'));
  check("detail offers a source-safe copy action",
    detail.includes('data-agent-copy="research-coordinator"') && detail.includes(">Copy agent</button>"));
  check("detail uses a plain-language task instead of raw JSON for conversational agents",
    detail.includes('id="inp-agent-prompt"') && detail.includes("Reply with a short hello.") && !detail.includes("Test input (JSON)"));
  check("detail escapes names and offers the trace as the next step",
    detail.includes("Research &lt;Coordinator&gt;") && detail.includes("Inspect latest"));
  check("detail includes the recent-run evidence ledger and privacy boundary",
    detail.includes("Recent runs") && detail.includes("Prompts and outputs are not stored here") && detail.includes("1/12"));
  check("successful evidence is rendered with success tone",
    W.agentTestResultHtml(run).includes("Run succeeded"));
  check("failed evidence carries its message",
    W.agentTestResultHtml({ status: "error", error: "graph unavailable" }).includes("graph unavailable"));
  check("interrupted evidence is not mislabeled as an error",
    W.agentTestResultHtml({ status: "interrupted", run_id: "run-2" }).includes("Run interrupted"));
  check("cancelled evidence is control flow, not a pending or generic error state",
    W.agentRunTone("cancelled") === "interrupted" &&
    W.agentTestResultHtml({ status: "cancelled", run_id: "run-3" }).includes("Run cancelled"));
  check("wire error results retain a safe diagnosis without retaining the raw detail", (() => {
    const wire = { status: "error", error: "llm_error", message: "provider secret" };
    const saved = W.agentNormalizeRunRecord({ record_id: "run-wire", status: wire.status,
      error: wire.message, error_class: W.agentErrorCategory(wire) });
    return saved.error_class === "llm_error" && !JSON.stringify(saved).includes("provider secret");
  })());
  check("the real internal panic wire category remains diagnosable",
    W.agentErrorCategory({ status: "error", error: "internal_panic", message: "private panic" }) === "internal_panic");
  check("run status changes use a persistent announcement and inspection has a focus target",
    html.includes('id="agent-run-announcer" role="status" aria-live="polite"') &&
    html.indexOf('id="agent-run-announcer"') < html.indexOf('id="agent-detail"') &&
    W.agentRunAnnouncement({ status: "success", run_id: "run-42" }).includes("Run succeeded") &&
    W.agentRunAnnouncement({ status: "running" }).includes("Run started") &&
    html.includes('id="flight-recorder-title" tabindex="-1"'));
  check("copy form uses native submission and an explicitly labelled review region",
    html.includes('id="agent-create-panel" role="region" aria-labelledby="agent-create-title"') &&
    html.includes('id="agent-copy-context" role="note" hidden') &&
    html.includes('id="agent-create-form"') && html.includes('type="submit" class="primary" id="btn-agent-create"'));
  check("copy outcomes are announced through an atomic live region",
    html.includes('id="toast" role="status" aria-live="polite" aria-atomic="true"'));
}

console.log(`\n${passed} passed, ${failed} failed`);
process.exit(failed ? 1 : 0);
