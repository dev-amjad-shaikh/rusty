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
let fetchHandler = null;
const fetchCalls = [];
const uiElements = new Map();
const sandbox = {
  async fetch(url, options) {
    fetchCalls.push({ url, options });
    if (fetchHandler) return fetchHandler(url, options);
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
  agentArchivedAt, agentLifecycleState, agentLifecycleItems, agentCardHtml, agentGraphLabel, agentDefaultInput, agentBuildRunInput,
  agentRunTone, agentErrorCategory, agentNormalizeRunRecord, agentMergeRunHistory,
  agentDurationLabel, agentRunTimeLabel, agentRunHistoryHtml,
  agentJourney, agentJourneyHtml, agentCopyableValue, agentCopyableMap, agentCopyDraft, agentVersionDraft, agentCopySourceSnapshot,
  agentSensitiveKey, agentSensitiveValueKeys, agentRedactedValue, agentCopyManifestText, agentCopyContextHtml,
  agentUtf8Length, agentClientIdError, agentValidateDraft, agentManifestScan, agentPortableManifest,
  agentIntentInspectConfig, agentIntentDraft, agentIntentBuild, agentIntentToolsText, agentIntentSelectedScopeOrder,
  agentManifestText, agentParseManifestText, agentManifestFilename, agentManifestDraft,
  agentParseJsonWithNumberKinds, agentValidateManifestNumbers, agentValidateRuntimeLimitLexemes,
  agentRuntimeLimitValue, agentRuntimeLimitRoundTrips, agentStoredNumbersRoundTrip, agentManifestActionError,
  agentReadManifestFile,
  agentConfigurationEvidence, agentConfigurationEvidenceHtml, agentImportContextHtml,
  agentApplyDraftValidation, agentRevalidateEditedField, agentSetShortcutLocks,
  agentCopyIdentityConflict, agentBuildCreatePayload,
  agentChangeFingerprint, agentChangeEqual, agentChangeExcerpt, agentChangeAdvanced,
  agentChangeIntentView, agentChangeReviewView, agentChangeReview, agentChangeReviewHtml,
  agentVersionIdValid, agentVersionAssistant, agentVersionEnvelope, agentVersionExact,
  agentVersionCreateReceipt, agentVersionDraftReviewable, agentVersionActivationReceipt, agentVersionManifestEvidence,
  agentLifecycleSnapshotMatches, agentLifecycleReceipt, agentLifecycleManifestEvidence, agentLifecycleDeskHtml,
  agentVersionDeskHtml, agentVersionContextHtml, agentInvalidateVersionsForSelection,
  agentVersionsLoad, agentVersionReview, agentVersionActivate, agentSelectAssistant, agentSetLifecycleFilter, homeNavigate,
  agentInvalidateLifecycleForSelection, agentLifecycleOpen, agentLifecycleApply,
  agentErrorHtml, agentTestResultHtml, agentRunAnnouncement, agentDetailHtml,
  agentOpenCreate, agentOpenVersion, agentCreate, store,
};`, sandbox, { filename: "index.html<script>" });

const W = sandbox.__workbench;
const info = { graphs: [{ name: "pipeline" }, { name: "react_agent" }] };
const VERSION1 = `av-${"1".repeat(64)}`;
const agent = {
  assistant_id: "research-coordinator",
  name: "Research <Coordinator>",
  graph: "react_agent",
  config: { recursion_limit: 12 },
  metadata: { description: "Collect and synthesize evidence", tags: ["research", "production"] },
  created_at: "2026-08-09T12:00:00Z",
  active_version_id: VERSION1,
  version_count: 1,
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
  const archived = { ...agent, assistant_id: "archived-scout", archived_at: "2026-08-10T04:00:00Z" };
  eq("lifecycle shelf: active filter excludes archived records", W.agentLifecycleItems([agent, archived], "active").map((item) => item.assistant_id), [agent.assistant_id]);
  eq("lifecycle shelf: archived and all filters preserve exact records", [
    W.agentLifecycleItems([agent, archived], "archived").map((item) => item.assistant_id),
    W.agentLifecycleItems([agent, archived], "all").map((item) => item.assistant_id),
  ], [[archived.assistant_id], [agent.assistant_id, archived.assistant_id]]);
  const malformed = { ...agent, assistant_id: "malformed-lifecycle", archived_at: "yesterday" };
  const permissiveDate = { ...agent, assistant_id: "permissive-date", archived_at: "08/10/2026" };
  check("lifecycle shelf: malformed and non-RFC3339 timestamps become unavailable, never active",
    W.agentLifecycleState(malformed) === "unknown" && W.agentLifecycleState(permissiveDate) === "unknown" &&
    W.agentLifecycleItems([malformed, permissiveDate], "active").length === 0 &&
    W.agentLifecycleItems([malformed, permissiveDate], "archived").length === 0 &&
    W.agentLifecycleItems([malformed, permissiveDate], "all").length === 2);
  const unavailableReadiness = W.agentReadiness(malformed, info, null);
  check("lifecycle shelf: unavailable evidence fails run readiness with a visible diagnosis",
    unavailableReadiness.steps[1].label === "Lifecycle unavailable" && !unavailableReadiness.steps[1].ready &&
    W.agentCardHtml(malformed, unavailableReadiness).includes("Lifecycle unavailable"));
  const archivedReadiness = W.agentReadiness(archived, info, { status: "success" });
  check("lifecycle shelf: archive blocks runnable readiness without erasing prior test evidence",
    archivedReadiness.steps[1].label === "Archived" && !archivedReadiness.steps[1].ready && archivedReadiness.steps[2].ready);
  const card = W.agentCardHtml(archived, archivedReadiness);
  check("lifecycle shelf: archived catalog identity is visible and hostile content stays escaped",
    card.includes("Archived") && card.includes("archived-scout") && card.includes("&lt;Coordinator&gt;"));
}

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
  const valid = W.agentValidateDraft({
    name: "Research", graph: "react_agent", assistantId: "research-v2", recursionLimit: "20",
  }, ["react_agent", "pipeline"]);
  check("configuration validation: a registered behavior and valid runtime fields pass", valid.valid);
  const missing = W.agentValidateDraft({ name: "", graph: "missing", assistantId: "../agent", recursionLimit: "1.5" }, ["react_agent"]);
  check("configuration validation: all actionable field errors are retained",
    !missing.valid && missing.first === "name" && missing.errors.graph.includes("not registered") &&
    missing.errors.assistantId.includes("path separators") && missing.errors.recursionLimit.includes("whole number"));
  check("configuration validation: optional generated identity is accepted",
    W.agentClientIdError("") === "" && W.agentClientIdError("...").includes("only dots") &&
    W.agentClientIdError("assistants").includes("reserved"));
  check("configuration validation: identifier bounds mirror server UTF-8 bytes, not JavaScript code units",
    W.agentUtf8Length("é") === 2 && W.agentClientIdError("é".repeat(129)).includes("UTF-8 bytes"));
  check("configuration validation: unsafe integer limits are rejected", !W.agentValidateDraft({
    name: "A", graph: "react_agent", recursionLimit: String(Number.MAX_SAFE_INTEGER + 1),
  }, ["react_agent"]).valid);
  check("configuration validation: zero mirrors the server's unsigned runtime limit",
    W.agentValidateDraft({ name: "A", graph: "react_agent", recursionLimit: "0" }, ["react_agent"]).valid);
  check("configuration validation: an explicit empty behavior registry rejects every graph",
    W.agentValidateDraft({ name: "A", graph: "react_agent" }, []).errors.graph.includes("no registered behaviors"));
  check("configuration validation: whitespace identity is rejected rather than silently replaced",
    W.agentValidateDraft({ name: "A", graph: "react_agent", assistantId: "   " }, ["react_agent"]).errors.assistantId.includes("whitespace"));
}

{
  const fields = {
    model: "openai/gpt-5",
    tools: "search | read_only\npublish | non_idempotent",
    memoryMode: "read_write",
    scopes: ["agent", "user"],
    approval: "irreversible",
    outputMode: "json_schema",
    outputSchema: "registry/report-v2",
    budgetTokens: "18446744073709551615",
    budgetCost: "125.500001",
    budgetLatency: "90000",
  };
  const built = W.agentIntentBuild(fields);
  check("visual intent: typed model, tool, memory, and approval surfaces form one versioned contract",
    built.valid && built.intent.format === "rusty.agent-intent/v2" && built.intent.model === "openai/gpt-5" &&
    built.intent.tools[1].effect === "non_idempotent" && built.intent.memory.scopes.join(",") === "agent,user" &&
    built.intent.approval === "irreversible" && built.intent.output.mode === "json_schema" &&
    built.intent.output.schema === "registry/report-v2" &&
    built.intent.budget.max_tokens === "18446744073709551615" && built.intent.budget.max_cost_usd === "125.500001");
  check("visual intent: the untouched canvas does not invent a stored requirement",
    W.agentIntentBuild({ model: "", tools: "", memoryMode: "none", scopes: [], approval: "runtime_policy" }).empty);
  check("visual intent: tool contracts use a canonical exact effect grammar", (() => {
    const malformed = [
      "search|read_only", "search | unknown", "search | read_only\nsearch | pure", "bad tool | pure",
    ];
    return malformed.every((tools) => !W.agentIntentBuild({ ...fields, tools }).valid);
  })());
  check("visual intent: tool text is byte-bounded before line parsing",
    W.agentIntentBuild({ ...fields, tools: "x".repeat(4097) }).errors.tools.includes("4 KiB"));
  check("visual intent: memory scopes cannot imply access the operator did not choose",
    W.agentIntentBuild({ ...fields, memoryMode: "none" }).errors.memoryMode.includes("read access"));
  check("visual intent: declared memory access requires an explicit allowed scope",
    W.agentIntentBuild({ ...fields, memoryMode: "read_only", scopes: [] }).errors.memoryMode.includes("at least one") &&
    W.agentIntentDraft({ studio_intent: { format: "rusty.agent-intent/v1", model: "model", tools: [],
      memory: { access: "read_write", scopes: [] }, approval: "runtime_policy" } }).intentLocked);
  check("visual intent: model identities reject hidden controls and invisible normalization",
    !W.agentIntentBuild({ ...fields, model: " model " }).valid &&
    !W.agentIntentBuild({ ...fields, model: "model\tname" }).valid &&
    !W.agentIntentBuild({ ...fields, model: "model\u202Ename" }).valid &&
    !W.agentIntentBuild({ ...fields, model: "model\u200Bname" }).valid);
  check("visual intent: URL credentials and token-shaped model values fail before persistence",
    !W.agentIntentBuild({ ...fields, model: "https://user:password@host/model" }).valid &&
    !W.agentIntentBuild({ ...fields, model: "sk-1234567890abcdef" }).valid);
  check("visual intent: ordinary registry names with security-adjacent prefixes remain usable",
    W.agentIntentBuild({ ...fields, model: "api-speech-preview" }).valid &&
    W.agentIntentBuild({ ...fields, model: "token-embedding-model" }).valid);
  check("visual intent: output schema binding is mode-bound and credential-safe",
    !W.agentIntentBuild({ ...fields, outputMode: "text", outputSchema: "registry/report-v2" }).valid &&
    !W.agentIntentBuild({ ...fields, outputSchema: "https://user:password@host/schema" }).valid &&
    !W.agentIntentBuild({ ...fields, outputSchema: "schema\u202Ename" }).valid &&
    W.agentIntentBuild({ ...fields, outputSchema: "registry/report-v3" }).valid);
  check("visual intent: exact budget strings reject rounding, exponent, leading-zero, and u64 overflow shapes",
    !W.agentIntentBuild({ ...fields, budgetTokens: "1e3" }).valid &&
    !W.agentIntentBuild({ ...fields, budgetTokens: "01" }).valid &&
    !W.agentIntentBuild({ ...fields, budgetLatency: "18446744073709551616" }).valid &&
    !W.agentIntentBuild({ ...fields, budgetCost: "1.0000001" }).valid &&
    W.agentIntentBuild({ ...fields, budgetTokens: "0", budgetCost: "0", budgetLatency: "0" }).valid);
  check("visual intent: output-only and zero-budget-only requirements are not erased as untouched defaults",
    !W.agentIntentBuild({ model: "", tools: "", memoryMode: "none", scopes: [], approval: "runtime_policy",
      outputMode: "text", outputSchema: "", budgetTokens: "", budgetCost: "", budgetLatency: "" }).empty &&
    !W.agentIntentBuild({ model: "", tools: "", memoryMode: "none", scopes: [], approval: "runtime_policy",
      outputMode: "runtime_default", outputSchema: "", budgetTokens: "0", budgetCost: "", budgetLatency: "" }).empty);
  check("visual intent: imported invisible or credential-shaped model bindings lock instead of rendering a shortcut", (() => {
    const base = { format: "rusty.agent-intent/v1", tools: [], memory: { access: "none", scopes: [] }, approval: "irreversible" };
    return ["model\u202Ename", "model\u200Bname", "https://user:password@host/model", "sk-1234567890abcdef"]
      .every((model) => W.agentIntentDraft({ studio_intent: { ...base, model } }).intentLocked);
  })());
  const config = { recursion_limit: 9, studio_intent: built.intent };
  const inspected = W.agentIntentInspectConfig(config);
  const draft = W.agentIntentDraft(config);
  check("visual intent: stored v2 contracts hydrate every guided surface exactly",
    inspected.valid && inspected.present && draft.model === fields.model && draft.tools === fields.tools &&
    draft.memoryMode === fields.memoryMode && draft.scopes.join(",") === "agent,user" && draft.approval === fields.approval &&
    draft.outputMode === fields.outputMode && draft.outputSchema === fields.outputSchema &&
    draft.budgetTokens === fields.budgetTokens && draft.budgetCost === fields.budgetCost && draft.budgetLatency === fields.budgetLatency);
  check("visual intent: valid v1 intent hydrates safe defaults and upgrades only when rebuilt", (() => {
    const legacy = { format: "rusty.agent-intent/v1", model: "legacy/model", tools: [],
      memory: { access: "none", scopes: [] }, approval: "irreversible" };
    const legacyDraft = W.agentIntentDraft({ studio_intent: legacy });
    const upgraded = W.agentIntentBuild(legacyDraft);
    return !legacyDraft.intentLocked && legacyDraft.outputMode === "runtime_default" && legacyDraft.budgetTokens === "" &&
      upgraded.valid && upgraded.intent.format === "rusty.agent-intent/v2";
  })());
  check("visual intent: malformed v2 output and budget envelopes lock instead of being normalized", (() => {
    const base = { format: "rusty.agent-intent/v2", model: "model", tools: [],
      memory: { access: "none", scopes: [] }, approval: "runtime_policy",
      output: { mode: "text", schema: "" }, budget: { max_tokens: "", max_cost_usd: "", max_latency_ms: "" } };
    return [
      { ...base, output: { mode: "json_schema", schema: "" } },
      { ...base, budget: { max_tokens: 10, max_cost_usd: "", max_latency_ms: "" } },
      { ...base, budget: { max_tokens: "", max_cost_usd: "", max_latency_ms: "", extra: "" } },
    ].every((studio_intent) => W.agentIntentDraft({ studio_intent }).intentLocked);
  })());
  check("visual intent: imported scope order survives fixed checkbox display order",
    W.agentIntentSelectedScopeOrder(["tenant", "run"], ["run", "tenant"]).join(",") === "tenant,run");
  const payload = W.agentBuildCreatePayload({
    name: "Builder", graph: "react_agent", assistantId: "", recursionLimit: "9", description: "", tags: "", ...fields,
  }, null);
  eq("visual intent: assistant creation stores the reviewed intent under config without changing runtime fields",
    payload.config, config);
  const opaque = { recursion_limit: 4, studio_intent: { format: "vendor.intent/v2", token: "keep-exact" } };
  const opaqueDraft = W.agentIntentDraft(opaque);
  const preserved = W.agentBuildCreatePayload({
    name: "Opaque", graph: "react_agent", assistantId: "", recursionLimit: "4", description: "", tags: "",
    ...opaqueDraft,
  }, { config: opaque });
  check("visual intent: unknown stored contracts lock the shortcut and remain byte-for-byte structural values",
    opaqueDraft.intentLocked && JSON.stringify(preserved.config.studio_intent) === JSON.stringify(opaque.studio_intent));
  const explicitEmpty = { format: "rusty.agent-intent/v1", model: "", tools: [], memory: { access: "none", scopes: [] }, approval: "runtime_policy" };
  check("visual intent: a present no-op envelope is locked and preserved rather than silently deleted", (() => {
    const locked = W.agentIntentDraft({ studio_intent: explicitEmpty });
    const kept = W.agentBuildCreatePayload({ name: "Empty", graph: "react_agent", recursionLimit: "", description: "", tags: "", ...locked },
      { config: { studio_intent: explicitEmpty } });
    return locked.intentLocked && JSON.stringify(kept.config.studio_intent) === JSON.stringify(explicitEmpty);
  })());
  check("visual intent: exact non-DOM scope and tool order survives export, import, rebuild, and duplicate", (() => {
    const sourceFields = { ...fields, tools: "publish | non_idempotent\nsearch | read_only", scopes: ["tenant", "run"] };
    const original = W.agentBuildCreatePayload({ name: "Portable", graph: "react_agent", recursionLimit: "8", description: "", tags: "", ...sourceFields }, null);
    const manifest = W.agentPortableManifest({ ...original, assistant_id: "portable" });
    const parsed = W.agentParseManifestText(W.agentManifestText(manifest, false), ["react_agent"]);
    const importedDraft = W.agentManifestDraft(parsed);
    const imported = W.agentBuildCreatePayload({ ...importedDraft, description: "", tags: "" }, parsed);
    const copyDraft = W.agentCopyDraft({ ...original, assistant_id: "portable" });
    const copied = W.agentBuildCreatePayload({ ...copyDraft, name: "Copy", description: "", tags: "" }, { ...original, assistant_id: "portable" });
    return JSON.stringify(imported.config.studio_intent) === JSON.stringify(original.config.studio_intent) &&
      JSON.stringify(copied.config.studio_intent) === JSON.stringify(original.config.studio_intent);
  })());
}

{
  const intent = W.agentIntentBuild({
    model: "openai/gpt-5", tools: "search | read_only\npublish | non_idempotent",
    memoryMode: "read_write", scopes: ["agent", "user"], approval: "irreversible",
  }).intent;
  const source = {
    assistant_id: "evidence-scout", name: "Evidence scout", graph: "react_agent",
    config: { recursion_limit: 12, studio_intent: intent, runtime: { mode: "careful", retries: 2 } },
    metadata: { description: "Collect defensible evidence", tags: ["research", "production"], owner: "quality" },
  };
  const copy = W.agentCopyDraft(source);
  const draft = W.agentBuildCreatePayload({
    ...copy, name: "Evidence scout · canary", assistantId: "evidence-scout-canary", model: "openai/gpt-5.1",
  }, source);
  const review = W.agentChangeReview(source, draft);
  check("configuration change review: a copy binds exact source and draft surfaces",
    review.changed === 3 && review.unchanged === 11 && review.review === 0 &&
    review.rows.find((row) => row.key === "name").state === "changed" &&
    review.rows.find((row) => row.key === "assistantId").state === "changed" &&
    review.rows.find((row) => row.key === "model").state === "changed" &&
    review.rows.find((row) => row.key === "output").state === "unchanged" &&
    review.rows.find((row) => row.key === "budget").state === "unchanged" &&
    review.rows.find((row) => row.key === "advancedConfig").state === "unchanged");
  check("configuration change review: comparison does not mutate the source record",
    source.name === "Evidence scout" && source.assistant_id === "evidence-scout" &&
    source.config.studio_intent.model === "openai/gpt-5");
  check("configuration change review: rebuilding v1 visibly discloses both v2 default envelopes", (() => {
    const legacy = { assistant_id: "legacy", name: "Legacy", graph: "react_agent", config: { studio_intent: {
      format: "rusty.agent-intent/v1", model: "legacy/model", tools: [], memory: { access: "none", scopes: [] }, approval: "irreversible",
    } } };
    const rebuilt = W.agentBuildCreatePayload({ ...W.agentVersionDraft(legacy), description: "", tags: "" }, legacy);
    const review = W.agentChangeReview(legacy, rebuilt);
    const rendered = W.agentChangeReviewHtml(legacy, rebuilt, "version");
    return review.rows.find((row) => row.key === "output").state === "changed" &&
      review.rows.find((row) => row.key === "budget").state === "changed" &&
      rendered.includes("Runtime default · implicit v1") && rendered.includes("Runtime default · explicit v2") &&
      rendered.includes("Runtime defaults · implicit v1") && rendered.includes("Runtime defaults · explicit v2");
  })());
  check("configuration change review: structural object order is not a false change",
    W.agentChangeEqual({ alpha: 1, nested: { one: true, two: false } },
      { nested: { two: false, one: true }, alpha: 1 }) === true);
  check("configuration change review: exact raw integer tokens remain distinguishable", (() => {
    const left = W.agentParseJsonWithNumberKinds('{"value":9007199254740992}');
    const same = W.agentParseJsonWithNumberKinds('{"value":9007199254740992}');
    const adjacent = W.agentParseJsonWithNumberKinds('{"value":9007199254740993}');
    return W.agentChangeEqual(W.agentChangeAdvanced(left, []).raw, W.agentChangeAdvanced(same, []).raw) === true &&
      W.agentChangeEqual(W.agentChangeAdvanced(left, []).raw, W.agentChangeAdvanced(adjacent, []).raw) === false;
  })());
  check("configuration change review: lossless server number provenance is not a false change", (() => {
    const parsedSource = W.agentParseJsonWithNumberKinds(
      '{"assistant_id":"source","name":"Source","graph":"pipeline","config":{"recursion_limit":12}}');
    const plainDraft = { assistant_id: "copy", name: "Source", graph: "pipeline", config: { recursion_limit: 12 } };
    return W.agentChangeReview(parsedSource, plainDraft).rows
      .find((row) => row.key === "recursionLimit").state === "unchanged";
  })());
  const htmlReview = W.agentChangeReviewHtml(
    { ...source, name: 'Source <script>alert("x")</script>' }, draft, "copy");
  check("configuration change review: hostile values are escaped and lifecycle truth stays explicit",
    htmlReview.includes("&lt;script&gt;") && !htmlReview.includes('<script>alert("x")</script>') &&
    htmlReview.includes("separate assistant") && htmlReview.includes("not a new active version") &&
    htmlReview.includes('role="list"') && htmlReview.includes('role="listitem"'));
  check("configuration change review: long visible values are bounded without weakening exact comparison",
    W.agentChangeExcerpt("x".repeat(400)).length < 220 && W.agentChangeExcerpt("x".repeat(400)).includes("[excerpt]") &&
    W.agentChangeEqual("x".repeat(400), "x".repeat(399) + "y") === false);
  const opaqueIntent = { format: "vendor.intent/v2", credential: "must-not-render" };
  const opaqueSource = { assistant_id: "opaque", name: "Opaque", graph: "pipeline", config: { studio_intent: opaqueIntent } };
  const opaqueDraft = W.agentBuildCreatePayload({
    ...W.agentCopyDraft(opaqueSource), name: "Opaque copy", assistantId: "opaque-copy",
  }, opaqueSource);
  const opaqueHtml = W.agentChangeReviewHtml(opaqueSource, opaqueDraft, "copy");
  check("configuration change review: opaque intent stays comparable without exposing stored contents",
    W.agentChangeReview(opaqueSource, opaqueDraft).rows.find((row) => row.key === "model").state === "unchanged" &&
    opaqueHtml.includes("Opaque intent · exact manifest") && !opaqueHtml.includes("must-not-render"));
  const tooDeep = {};
  let cursor = tooDeep;
  for (let index = 0; index < 18; index++) { cursor.next = {}; cursor = cursor.next; }
  const bounded = W.agentChangeReview(
    { assistant_id: "deep", name: "Deep", graph: "pipeline", config: { extension: tooDeep } },
    { assistant_id: "deep-copy", name: "Deep", graph: "pipeline", config: { extension: tooDeep } });
  check("configuration change review: over-bound structures degrade to explicit review instead of throwing",
    bounded.rows.find((row) => row.key === "advancedConfig").state === "review");
}

{
  const v2 = `av-${"2".repeat(64)}`;
  const assistantId = "research-coordinator";
  const active = { ...agent, version_count: 2 };
  const envelope = {
    assistant_id: assistantId,
    active_version_id: VERSION1,
    assistant: active,
    versions: [
      { version_id: v2, parent_version_id: VERSION1, graph: "pipeline", created_at: "2026-08-10T01:00:00Z", active: false },
      { version_id: VERSION1, graph: "react_agent", created_at: "2026-08-09T12:00:00Z", active: true },
    ],
  };
  const loaded = W.agentVersionEnvelope(envelope, assistantId);
  check("assistant versions: exact bounded history binds one active version and its catalog snapshot",
    loaded && loaded.activeVersionId === VERSION1 && loaded.versions.length === 2 &&
    loaded.assistant.version_count === 2 && loaded.versions[0].parent_version_id === VERSION1);
  check("assistant versions: malformed identity, duplicate, active, parent, and boolean evidence fail closed",
    W.agentVersionEnvelope({ ...envelope, active_version_id: "av-bad" }, assistantId) === null &&
    W.agentVersionEnvelope({ ...envelope, versions: [envelope.versions[0], envelope.versions[0]] }, assistantId) === null &&
    W.agentVersionEnvelope({ ...envelope, versions: envelope.versions.map((item) => ({ ...item, active: false })) }, assistantId) === null &&
    W.agentVersionEnvelope({ ...envelope, versions: [{ ...envelope.versions[0], parent_version_id: `av-${"3".repeat(64)}` }, envelope.versions[1]] }, assistantId) === null &&
    W.agentVersionEnvelope({ ...envelope, versions: [{ ...envelope.versions[0], active: "false" }, envelope.versions[1]] }, assistantId) === null);
  check("assistant versions: IDs require the full lowercase content address",
    W.agentVersionIdValid(VERSION1) && !W.agentVersionIdValid(`av-${"A".repeat(64)}`) &&
    !W.agentVersionIdValid(`av-${"1".repeat(63)}`) && !W.agentVersionIdValid("v1"));
  const versionDraft = W.agentVersionDraft(active);
  check("assistant versions: a version draft preserves identity and name instead of becoming a copy",
    versionDraft.assistantId === assistantId && versionDraft.name === active.name &&
    W.agentCopyDraft(active).assistantId === "" && W.agentCopyDraft(active).name.startsWith("Copy of "));

  const exactWire = W.agentParseJsonWithNumberKinds(`{
    "assistant_id":"research-coordinator",
    "active_version_id":"${VERSION1}",
    "version":{
      "version_id":"${v2}","parent_version_id":"${VERSION1}",
      "name":"Research candidate","graph":"pipeline",
      "config":{"recursion_limit":9007199254740992,"model":"candidate"},
      "metadata":{"owner":"quality"},"created_at":"2026-08-10T01:00:00Z","active":false
    }
  }`);
  const candidate = W.agentVersionExact(exactWire, assistantId, envelope.versions[0], VERSION1);
  check("assistant versions: exact version reads preserve unsafe Rust integers",
    candidate && candidate.version_id === v2 && W.agentStoredNumbersRoundTrip(candidate.config));
  const draft = { name: candidate.name, graph: candidate.graph, config: candidate.config, metadata: candidate.metadata };
  const createReceipt = W.agentVersionCreateReceipt({
    assistant_id: assistantId, active_version_id: VERSION1, created: true, version: candidate,
  }, assistantId, VERSION1, draft);
  check("assistant versions: creation receipt binds parent, draft content, inactive state, and idempotency bit",
    createReceipt && createReceipt.created && createReceipt.version.version_id === v2 &&
    W.agentVersionCreateReceipt({ assistant_id: assistantId, active_version_id: VERSION1, created: true, version: { ...candidate, active: true } }, assistantId, VERSION1, draft) === null &&
    W.agentVersionCreateReceipt({ assistant_id: assistantId, active_version_id: v2, created: true, version: candidate }, assistantId, VERSION1, draft) === null &&
    W.agentVersionCreateReceipt({ assistant_id: assistantId, active_version_id: VERSION1, created: true, version: candidate }, assistantId, VERSION1,
      { ...draft, metadata: { owner: "other" } }) === null);
  const activatedAssistant = {
    ...active, name: candidate.name, graph: candidate.graph, config: candidate.config,
    metadata: candidate.metadata, active_version_id: v2,
  };
  const activation = W.agentVersionActivationReceipt({ activated: true, assistant: activatedAssistant }, assistantId, candidate, 2);
  check("assistant versions: activation receipt binds the exact reviewed snapshot and serving pointer",
    activation && activation.assistant.active_version_id === v2 &&
    W.agentVersionActivationReceipt({ activated: true, assistant: { ...activatedAssistant, graph: "react_agent" } }, assistantId, candidate, 2) === null);
  const manifests = W.agentVersionManifestEvidence(active, { assistant_id: assistantId, ...candidate });
  const oversized = { ...candidate, config: Object.fromEntries(Array.from({ length: 2001 }, (_, index) => [`field_${index}`, index])) };
  check("assistant versions: activation requires complete bounded current and selected manifests",
    manifests.ready && manifests.html.includes("Review complete current manifest") &&
    manifests.html.includes("Review complete selected manifest") && manifests.html.includes('&quot;owner&quot;: &quot;quality&quot;') &&
    !W.agentVersionManifestEvidence(active, { assistant_id: assistantId, ...oversized }).ready);
  const whitespaceIdentity = W.agentVersionManifestEvidence(
    { ...active, name: "  Stored exactly  ", graph: "pipeline " },
    { assistant_id: assistantId, ...candidate, name: "  Candidate exactly  ", graph: "canary " },
  );
  check("assistant versions: activation evidence preserves stored identity bytes without portable-manifest trimming",
    whitespaceIdentity.ready && whitespaceIdentity.html.includes("  Stored exactly  ") &&
    whitespaceIdentity.html.includes("pipeline ") && whitespaceIdentity.html.includes("  Candidate exactly  "));
  check("assistant versions: an over-bound draft is blocked before a mutation it cannot receipt-check",
    W.agentVersionDraftReviewable(draft) &&
    !W.agentVersionDraftReviewable({ ...draft, config: oversized.config }));
  W.store.agentVersions = { assistantId, activeVersionId: VERSION1, loading: true, reviewing: v2, submitting: true };
  const generation = W.store.agentVersionRequest;
  check("assistant versions: selecting another assistant invalidates every stale busy operation",
    W.agentInvalidateVersionsForSelection("another-assistant") && W.store.agentVersions === null &&
    W.store.agentVersionRequest === generation + 1 && !W.agentInvalidateVersionsForSelection("another-assistant"));

  W.store.info = info;
  const desk = W.agentVersionDeskHtml(active, {
    ...loaded, loading: false, error: null, reviewing: "", submitting: false, pendingActivation: candidate,
  });
  check("assistant versions: lineage rail exposes full IDs, one active marker, and deliberate activation review",
    desk.includes(VERSION1) && desk.includes(v2) && desk.includes("Active") &&
    desk.includes("Review activation") && desk.includes("Make this version active") &&
    desk.includes("Current serving → selected version") && desk.includes('aria-describedby="agent-version-row-0"'));
  W.store.info = { graphs: [{ name: "react_agent" }] };
  const unavailableDesk = W.agentVersionDeskHtml(active, {
    ...loaded, loading: false, error: null, reviewing: "", submitting: false, pendingActivation: candidate,
  });
  check("assistant versions: activation is disabled when the historical behavior is no longer registered",
    unavailableDesk.includes("not registered on this server") &&
    unavailableDesk.includes('data-agent-version-activate=') && unavailableDesk.includes(" disabled"));
  W.store.info = info;
  const hostileDesk = W.agentVersionDeskHtml(active, {
    ...loaded, versions: [{ ...loaded.versions[0], graph: '<img src=x onerror="alert(1)">' }, loaded.versions[1]],
    loading: false, error: null, reviewing: "", submitting: false, pendingActivation: null,
  });
  check("assistant versions: hostile lineage labels are escaped",
    hostileDesk.includes("&lt;img") && !hostileDesk.includes("<img src=x"));
  const versionContext = W.agentVersionContextHtml(active);
  check("assistant versions: draft copy explains the non-serving boundary",
    versionContext.includes("immutable draft") && versionContext.includes("Unchanged until activation") &&
    versionContext.includes(VERSION1));

  const archived = { ...active, archived_at: "2026-08-10T04:00:00Z" };
  check("assistant lifecycle: exact review snapshot binds identity, active version, body, and lifecycle",
    W.agentLifecycleSnapshotMatches({ ...active }, active) &&
    !W.agentLifecycleSnapshotMatches({ ...active, metadata: { owner: "other" } }, active) &&
    !W.agentLifecycleSnapshotMatches(archived, active));
  const archiveReceipt = W.agentLifecycleReceipt({ changed: true, lifecycle: "archived", assistant: archived }, assistantId, active, "archive");
  const restoreReceipt = W.agentLifecycleReceipt({ changed: true, lifecycle: "active", assistant: active }, assistantId, archived, "restore");
  check("assistant lifecycle: receipts bind the reviewed version and requested terminal state",
    archiveReceipt && restoreReceipt && archiveReceipt.assistant.archived_at &&
    W.agentLifecycleReceipt({ changed: true, lifecycle: "active", assistant: archived }, assistantId, active, "archive") === null &&
    W.agentLifecycleReceipt({ changed: true, lifecycle: "archived", assistant: { ...archived, active_version_id: v2 } }, assistantId, active, "archive") === null);
  check("assistant lifecycle: receipts reject permissive and malformed archive timestamps",
    W.agentLifecycleReceipt({ changed: true, lifecycle: "archived", assistant: { ...active, archived_at: "0" } }, assistantId, active, "archive") === null &&
    W.agentLifecycleReceipt({ changed: true, lifecycle: "archived", assistant: { ...active, archived_at: "2026-02-30T04:00:00Z" } }, assistantId, active, "archive") === null);
  const lifecycleEvidence = W.agentLifecycleManifestEvidence(active);
  check("assistant lifecycle: decision desk carries the complete exact manifest within bounds",
    lifecycleEvidence.ready && lifecycleEvidence.html.includes("Review complete active manifest") &&
    lifecycleEvidence.html.includes('&quot;description&quot;: &quot;Collect and synthesize evidence&quot;'));
  const lifecycleDesk = W.agentLifecycleDeskHtml(active, {
    assistantId, activeVersionId: VERSION1, action: "archive", loading: false,
    snapshot: active, error: null, submitting: false,
  });
  check("assistant lifecycle: archive review states retention, serving effect, and deliberate action",
    lifecycleDesk.includes("New runs will stop at admission") && lifecycleDesk.includes("Versions retained") &&
    lifecycleDesk.includes("Archive agent") && lifecycleDesk.includes(VERSION1) && lifecycleDesk.includes('id="agent-lifecycle-title"'));
  const restoreDesk = W.agentLifecycleDeskHtml(archived, {
    assistantId, activeVersionId: VERSION1, action: "restore", loading: false,
    snapshot: archived, error: null, submitting: false,
  });
  check("assistant lifecycle: restore review exposes archived evidence and reversibility",
    restoreDesk.includes("Restore review") && restoreDesk.includes("Restore agent") && restoreDesk.includes("archived"));
}

{
  const portable = W.agentPortableManifest(agent);
  check("manifest export: carries an explicit version and durable identity",
    portable.format === "rusty.assistant/v1" && portable.assistant_id === "research-coordinator");
  eq("manifest export: preserves exact config and metadata", { config: portable.config, metadata: portable.metadata },
    { config: agent.config, metadata: agent.metadata });
  const raw = W.agentManifestText(portable, false);
  const parsed = W.agentParseManifestText(raw, ["react_agent", "pipeline"]);
  eq("manifest import: exported JSON round-trips without data loss", parsed, portable);
  check("manifest import: non-object config and metadata remain legal exact values", (() => {
    const unusual = { format: "rusty.assistant/v1", name: "Opaque", graph: "pipeline", config: ["x"], metadata: "catalog" };
    const roundTrip = W.agentParseManifestText(JSON.stringify(unusual), ["pipeline"]);
    return Array.isArray(roundTrip.config) && roundTrip.metadata === "catalog";
  })());
  check("manifest import: unsupported recursion values are preserved without a false runtime claim", (() => {
    const unusual = { format: "rusty.assistant/v1", name: "Opaque", graph: "pipeline", config: { recursion_limit: "five" } };
    const roundTrip = W.agentParseManifestText(JSON.stringify(unusual), ["pipeline"]);
    const draft = W.agentManifestDraft(roundTrip);
    const payload = W.agentBuildCreatePayload({ ...draft, description: "", tags: "" }, roundTrip);
    const evidence = W.agentConfigurationEvidence(payload, ["pipeline"]);
    return draft.recursionLimit === "" && payload.config.recursion_limit === "five" &&
      evidence.runtimeLimit === "server default" && evidence.unsupportedRuntimeLimit;
  })());
  check("manifest import: unusual descriptive shapes are preserved instead of stringified", (() => {
    const unusual = { format: "rusty.assistant/v1", name: "Opaque", graph: "pipeline",
      metadata: { description: { localized: "Evidence" }, tags: [{ id: "research" }], owner: "quality" } };
    const draft = W.agentManifestDraft(unusual);
    const payload = W.agentBuildCreatePayload({ ...draft, recursionLimit: "" }, unusual);
    const evidence = W.agentConfigurationEvidence(payload, ["pipeline"]);
    return draft.description === "" && draft.tags === "" &&
      JSON.stringify(payload.metadata.description) === '{"localized":"Evidence"}' &&
      JSON.stringify(payload.metadata.tags) === '[{"id":"research"}]' &&
      evidence.unsupportedDescription && evidence.unsupportedTags;
  })());
  check("manifest import: untouched legacy tags retain their exact stored type", (() => {
    const legacy = { format: "rusty.assistant/v1", name: "Legacy", graph: "pipeline", metadata: { tags: "research,production" } };
    const draft = W.agentManifestDraft(legacy);
    const payload = W.agentBuildCreatePayload({ ...draft, recursionLimit: "", description: "" }, legacy);
    return draft.tags === "research, production" && payload.metadata.tags === "research,production";
  })());
  check("manifest import: unknown top-level fields are rejected instead of dropped", (() => {
    try { W.agentParseManifestText(JSON.stringify({ ...portable, model: "ignored" }), ["react_agent"]); return false; }
    catch (error) { return error.message.includes("Unknown top-level field") && error.message.includes("Nothing was imported"); }
  })());
  check("manifest import: unknown formats are rejected", (() => {
    try { W.agentParseManifestText(JSON.stringify({ ...portable, format: "rusty.assistant/v2" }), ["react_agent"]); return false; }
    catch (error) { return error.message.includes("rusty.assistant/v1"); }
  })());
  check("manifest import: values that JavaScript would mutate are rejected", (() => {
    const values = ["1e400", "9007199254740993", "-0", "9007199254740993.0"];
    return values.every((value) => {
      try {
        W.agentParseManifestText(`{"format":"rusty.assistant/v1","name":"A","graph":"react_agent","config":{"value":${value}}}`, ["react_agent"]);
        return false;
      } catch (error) { return /finite range|browser-safe range|negative zero|preserved exactly/.test(error.message); }
    });
  })());
  check("manifest import: alternate numeric tokens that would be rewritten are rejected", (() => {
    try {
      W.agentParseManifestText('{"format":"rusty.assistant/v1","name":"A","graph":"react_agent","config":{"timeout":1e3}}', ["react_agent"]);
      return false;
    } catch (error) { return error.message.includes("preserved exactly"); }
  })());
  check("manifest import: float and exponent step limits cannot change server semantics", (() => {
    return ["12.0", "1.2e1"].every((value) => {
      try {
        W.agentParseManifestText(`{"format":"rusty.assistant/v1","name":"A","graph":"react_agent","config":{"recursion_limit":${value}}}`, ["react_agent"]);
        return false;
      } catch (error) { return error.message.includes("unsigned integer JSON token"); }
    });
  })());
  check("manifest import: unavailable graphs are rejected before server mutation", (() => {
    try { W.agentParseManifestText(raw, ["pipeline"]); return false; }
    catch (error) { return error.message.includes("not registered"); }
  })());
  check("manifest import: oversized text is bounded", (() => {
    try { W.agentParseManifestText(" ".repeat(65537), ["react_agent"]); return false; }
    catch (error) { return error.message.includes("64 KiB"); }
  })());
  check("manifest import: multibyte text uses the documented byte bound", (() => {
    try { W.agentParseManifestText("é".repeat(32769), ["react_agent"]); return false; }
    catch (error) { return error.message.includes("64 KiB"); }
  })());
  check("manifest file import: declared and actual size are both bounded", await (async () => {
    const good = await W.agentReadManifestFile({ size: 10, async text() { return "{}"; } });
    let declared = false;
    let actual = false;
    try { await W.agentReadManifestFile({ size: 65537, async text() { return "{}"; } }); }
    catch (error) { declared = error.message.includes("64 KiB"); }
    try { await W.agentReadManifestFile({ size: 1, async text() { return "x".repeat(65537); } }); }
    catch (error) { actual = error.message.includes("64 KiB"); }
    return good === "{}" && declared && actual && await W.agentReadManifestFile(null) === null;
  })());
  check("manifest import: deep and high-cardinality values are bounded", (() => {
    let deep = {};
    let cursor = deep;
    for (let index = 0; index < 18; index++) { cursor.next = {}; cursor = cursor.next; }
    try { W.agentManifestScan(deep); return false; }
    catch (depthError) {
      try { W.agentManifestScan({ values: Array(2100).fill(0) }); return false; }
      catch (nodeError) { return depthError.message.includes("nesting") && nodeError.message.includes("values"); }
    }
  })());
  check("manifest export: filenames are portable and bounded",
    W.agentManifestFilename(" Résumé / Research Agent ") === "resume-research-agent.rusty-agent.json" &&
    W.agentManifestFilename("!") === "agent.rusty-agent.json");
  const secrets = W.agentManifestScan({ config: { provider: { api_key: "private" } }, metadata: { owner: "safe" } });
  check("manifest preview: secret-looking paths are located without exposing their value",
    secrets.sensitivePaths.includes("$.config.provider.api_key") &&
    !W.agentManifestText(portable, true).includes("private-provider-token"));
  const headerTuple = { config: { headers: [{ name: "Authorization", value: "Bearer private" }] } };
  check("manifest preview: credential header tuples redact their sibling value and trigger review",
    W.agentManifestScan(headerTuple).sensitivePaths.includes("$.config.headers[0].value") &&
    W.agentManifestText(headerTuple, true).includes('"value": "[hidden]"') &&
    !W.agentManifestText(headerTuple, true).includes("Bearer private"));
  const casedHeaderTuple = { config: { headers: [{ Name: "Authorization", Value: "Bearer private" }] } };
  check("manifest preview: credential header tuple property names are case-insensitive",
    W.agentManifestScan(casedHeaderTuple).sensitivePaths.includes("$.config.headers[0].Value") &&
    W.agentManifestText(casedHeaderTuple, true).includes('"Value": "[hidden]"') &&
    !W.agentManifestText(casedHeaderTuple, true).includes("Bearer private"));
}

{
  const configured = {
    ...agent,
    config: { recursion_limit: 12, temperature: 0.2, runtime: { mode: "careful" } },
    metadata: { description: "Collect evidence", tags: ["research"], owner: "quality" },
  };
  const evidence = W.agentConfigurationEvidence(configured, ["react_agent"]);
  check("configuration evidence: separates enforced runtime values from preserved fields",
    evidence.graphAvailable && evidence.runtimeLimit === "12 steps" && evidence.preservedFields === 3);
  const unavailable = W.agentConfigurationEvidence(configured, ["pipeline"]);
  check("configuration evidence: unavailable behavior is explicit", unavailable.graphAvailable === false);
  check("configuration evidence: an empty server registry is not treated as a wildcard",
    W.agentConfigurationEvidence(configured, []).graphAvailable === false);
  const zeroLimit = W.agentConfigurationEvidence({ ...configured, config: { recursion_limit: 0 } }, ["react_agent"]);
  const lockedLimit = W.agentConfigurationEvidence({ ...configured, config: { recursion_limit: 9007199254740992 } }, ["react_agent"]);
  check("configuration evidence: server-valid zero and large u64 limits are reported as applied",
    zeroLimit.runtimeLimit === "0 steps" && !zeroLimit.unsupportedRuntimeLimit &&
    lockedLimit.runtimeLimit === "9007199254740992 steps" && lockedLimit.lockedRuntimeLimit);
  check("configuration evidence: raw server number kinds mirror serde_json as_u64", (() => {
    const integer = W.agentParseJsonWithNumberKinds('{"config":{"recursion_limit":12}}');
    const float = W.agentParseJsonWithNumberKinds('{"config":{"recursion_limit":12.0}}');
    const exponent = W.agentParseJsonWithNumberKinds('{"config":{"recursion_limit":1.2e1}}');
    const max = W.agentParseJsonWithNumberKinds('{"config":{"recursion_limit":18446744073709551615}}');
    const over = W.agentParseJsonWithNumberKinds('{"config":{"recursion_limit":18446744073709551616}}');
    return W.agentRuntimeLimitValue(integer.config) === 12 &&
      W.agentRuntimeLimitValue(float.config) === null && W.agentRuntimeLimitValue(exponent.config) === null &&
      W.agentRuntimeLimitValue(max.config) === "18446744073709551615" &&
      W.agentRuntimeLimitValue(over.config) === null && !W.agentRuntimeLimitRoundTrips(max.config) &&
      W.agentCopyContextHtml({ name: "Float", graph: "pipeline", config: float.config }).includes("Step limit<b>runtime default") &&
      W.agentCopyContextHtml({ name: "Integer", graph: "pipeline", config: integer.config }).includes("Step limit<b>12");
  })());
  check("configuration portability: lossy server number tokens fail closed", (() => {
    const float = W.agentParseJsonWithNumberKinds('{"name":"Legacy","graph":"pipeline","config":{"recursion_limit":12.0}}');
    try { W.agentPortableManifest(float); return false; }
    catch (exportError) {
      try { W.agentBuildCreatePayload({ name: "Copy", graph: "pipeline" }, float); return false; }
      catch (copyError) { return exportError.message.includes("number token") && copyError.message.includes("number token"); }
    }
  })());
  check("configuration portability: exact unsafe integers export and re-import consistently", (() => {
    const source = W.agentParseJsonWithNumberKinds(
      '{"assistant_id":"exact-unsafe","name":"Exact unsafe","graph":"pipeline","config":{"recursion_limit":9007199254740992,"custom_budget":9007199254740992}}');
    const portable = W.agentPortableManifest(source);
    const imported = W.agentParseManifestText(W.agentManifestText(portable, false), ["pipeline"]);
    return imported.config.recursion_limit === 9007199254740992 &&
      imported.config.custom_budget === 9007199254740992 && W.agentStoredNumbersRoundTrip(imported.config);
  })());
  check("configuration portability: lossy arbitrary config and metadata numbers are blocked", (() => {
    const sources = [
      W.agentParseJsonWithNumberKinds(
        '{"assistant_id":"lossy-config","name":"Lossy config","graph":"pipeline","config":{"custom_budget":9007199254740993}}'),
      W.agentParseJsonWithNumberKinds(
        '{"assistant_id":"lossy-metadata","name":"Lossy metadata","graph":"pipeline","metadata":{"score":12.0}}'),
    ];
    return sources.every((source) => {
      const evidence = W.agentConfigurationEvidence(source, ["pipeline"]);
      const evidenceHtml = W.agentConfigurationEvidenceHtml(source, ["pipeline"], "lossy-contract");
      if (!evidence.portableError.includes("number token") || !evidenceHtml.includes("number token")) return false;
      try { W.agentPortableManifest(source); return false; }
      catch (exportError) {
        try { W.agentBuildCreatePayload({ name: "Copy", graph: "pipeline" }, source); return false; }
        catch (copyError) { return exportError.message.includes("number token") && copyError.message.includes("number token"); }
      }
    });
  })());
  const unusual = W.agentConfigurationEvidence({ name: "Opaque", graph: "pipeline", config: [], metadata: "x" }, ["pipeline"]);
  check("configuration evidence: non-object stored shapes are disclosed",
    unusual.configShape === "non-object" && unusual.metadataShape === "non-object" && unusual.preservedFields === 2);
  const evidenceHtml = W.agentConfigurationEvidenceHtml({
    ...configured, name: "Research <unsafe>", config: { ...configured.config, api_key: "private" },
  }, ["react_agent"], "contract-test");
  check("configuration evidence: rail carries all three truthful boundaries",
    evidenceHtml.includes("Runs with") && evidenceHtml.includes("Describes") && evidenceHtml.includes("Preserves") &&
    evidenceHtml.includes("Stored is not the same as executed"));
  check("configuration evidence: output escapes identity and redacts secret-looking values",
    evidenceHtml.includes("Research &lt;unsafe&gt;") && !evidenceHtml.includes("private") && evidenceHtml.includes("secret-looking path"));
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
  check("copy payload: editing a server integer replaces stale token provenance everywhere", (() => {
    const apiSource = W.agentParseJsonWithNumberKinds(
      '{"assistant_id":"api-12","name":"API","graph":"react_agent","config":{"recursion_limit":12,"temperature":0.2}}');
    const apiDraft = W.agentCopyDraft(apiSource);
    const edited = W.agentBuildCreatePayload({ ...apiDraft, name: "API 18", recursionLimit: "18", description: "", tags: "" }, apiSource);
    const portable = W.agentPortableManifest(edited);
    const evidence = W.agentConfigurationEvidence(edited, ["react_agent"]);
    return edited.config.recursion_limit === 18 && portable.config.recursion_limit === 18 &&
      evidence.runtimeLimit === "18 steps" && W.agentRuntimeLimitRoundTrips(edited.config);
  })());
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
  check("manifest synchronization: raw edits block actions while guided-only edits do not",
    W.agentManifestActionError(true).includes("Apply the edited JSON") && W.agentManifestActionError(false) === "");
}

{
  function element(value = "") {
    return {
      value, disabled: false, style: {}, className: "", textContent: "", focused: false,
      focus() { this.focused = true; },
      getAttribute(name) { return this[name] ?? null; },
      hasAttribute(name) { return this[name] !== undefined; },
      setAttribute(name, next) { this[name] = next; },
      removeAttribute(name) { delete this[name]; },
    };
  }
  uiElements.clear();
  for (const [id, value] of Object.entries({
    "inp-agent-name": "Copy draft", "sel-agent-graph": "react_agent",
    "inp-agent-id": agent.assistant_id, "inp-agent-limit": "12",
    "inp-agent-description": "Collect evidence", "inp-agent-tags": "research",
    "inp-agent-model": "", "inp-agent-tools": "", "sel-agent-memory-mode": "none",
    "sel-agent-approval": "runtime_policy", "sel-agent-output-mode": "runtime_default", "inp-agent-output-schema": "",
    "inp-agent-budget-tokens": "", "inp-agent-budget-cost": "", "inp-agent-budget-latency": "",
    "btn-agent-create": "", "agent-form-error": "", "agent-intent-lock": "", toast: "",
  })) { const next = element(value); next.id = id; uiElements.set(id, next); }
  uiElements.set("agent-create-form", { querySelectorAll() { return []; } });
  W.store.info = info;
  const legacyFloat = W.agentParseJsonWithNumberKinds(
    '{"assistant_id":"legacy-float","name":"Legacy float","graph":"pipeline","config":{"recursion_limit":12.0}}');
  const legacySnapshot = W.agentCopySourceSnapshot(legacyFloat);
  const callsBeforeBlockedOpen = fetchCalls.length;
  check("copy-open interaction: raw number provenance survives the source snapshot",
    !W.agentRuntimeLimitRoundTrips(legacySnapshot.config) &&
    W.agentConfigurationEvidence(legacySnapshot, ["pipeline"]).unsupportedRuntimeLimit);
  check("copy-open interaction: a behavior-changing legacy copy is blocked visibly before mutation",
    W.agentOpenCreate(legacyFloat) === false && fetchCalls.length === callsBeforeBlockedOpen &&
    uiElements.get("toast").textContent.includes("cannot be copied") && !W.store.agentCreateOpen);
  W.store.agentCopySource = JSON.parse(JSON.stringify(agent));
  W.store.agentManifestSource = JSON.parse(JSON.stringify(agent));
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
  const toolField = uiElements.get("inp-agent-tools");
  toolField["aria-describedby"] = "agent-tools-help agent-intent-truth agent-intent-lock";
  const tooManyTools = Array.from({ length: 17 }, (_, index) => `tool${index} | pure`).join("\n");
  toolField.value = tooManyTools;
  W.agentApplyDraftValidation(W.agentValidateDraft({
    name: "Draft", graph: "react_agent", model: "", tools: tooManyTools, memoryMode: "none", scopes: [], approval: "runtime_policy",
  }, ["react_agent"]), true);
  const retainedError = uiElements.get("agent-form-error").textContent;
  toolField.value = "bad";
  check("visual intent interaction: a different invalid edit updates its announced validation state",
    toolField["aria-invalid"] === "true" && W.agentRevalidateEditedField(toolField) === false &&
    retainedError.includes("no more than") && uiElements.get("agent-form-error").textContent.includes("canonical") &&
    uiElements.get("agent-form-error").textContent !== retainedError && toolField["aria-describedby"].includes("agent-form-error"));
  toolField.value = "search | read_only";
  check("visual intent interaction: correcting the validated field clears stale invalid state atomically",
    W.agentRevalidateEditedField(toolField) && toolField["aria-invalid"] === undefined &&
    uiElements.get("agent-form-error").textContent === "" && !toolField["aria-describedby"].includes("agent-form-error") &&
    toolField["aria-describedby"].includes("agent-tools-help"));
  const nameField = uiElements.get("inp-agent-name");
  nameField.value = "";
  toolField.value = "bad";
  W.agentApplyDraftValidation(W.agentValidateDraft({
    name: "", graph: "react_agent", model: "", tools: "bad", memoryMode: "none", scopes: [], approval: "runtime_policy",
  }, ["react_agent"]), true);
  check("visual intent interaction: shared validation describes only the first invalid control",
    nameField["aria-invalid"] === "true" && nameField["aria-describedby"].includes("agent-form-error") &&
    toolField["aria-invalid"] === "true" && !toolField["aria-describedby"].includes("agent-form-error") &&
    uiElements.get("agent-form-error").textContent.includes("Enter an agent name"));
  nameField.value = "Recovered draft";
  check("visual intent interaction: correcting the first error advances its announcement to the next control",
    W.agentRevalidateEditedField(nameField) && nameField["aria-invalid"] === undefined &&
    !String(nameField["aria-describedby"] || "").includes("agent-form-error") &&
    toolField["aria-invalid"] === "true" && toolField["aria-describedby"].includes("agent-form-error") &&
    uiElements.get("agent-form-error").textContent.includes("canonical"));
  toolField.value = "search | read_only";
  check("visual intent interaction: correcting the final error clears the shared announcement",
    W.agentRevalidateEditedField(toolField) && toolField["aria-invalid"] === undefined &&
    !toolField["aria-describedby"].includes("agent-form-error") &&
    uiElements.get("agent-form-error").textContent === "");
  const budgetField = uiElements.get("inp-agent-budget-tokens");
  budgetField.value = "01";
  W.agentApplyDraftValidation(W.agentValidateDraft({
    name: "Draft", graph: "react_agent", model: "", tools: "", memoryMode: "none", scopes: [], approval: "runtime_policy",
    outputMode: "runtime_default", outputSchema: "", budgetTokens: "01", budgetCost: "", budgetLatency: "",
  }, ["react_agent"]), true);
  budgetField.value = "18446744073709551615";
  check("visual intent interaction: correcting an exact budget clears its associated error without rounding",
    budgetField["aria-invalid"] === "true" && W.agentRevalidateEditedField(budgetField) &&
    budgetField["aria-invalid"] === undefined && uiElements.get("agent-form-error").textContent === "");
  const outputModeField = uiElements.get("sel-agent-output-mode");
  const outputSchemaField = uiElements.get("inp-agent-output-schema");
  outputModeField.value = "json_schema";
  outputSchemaField.value = "";
  W.agentApplyDraftValidation(W.agentValidateDraft({
    name: "Draft", graph: "react_agent", model: "", tools: "", memoryMode: "none", scopes: [], approval: "runtime_policy",
    outputMode: "json_schema", outputSchema: "", budgetTokens: "", budgetCost: "", budgetLatency: "",
  }, ["react_agent"]), true);
  outputModeField.value = "text";
  check("visual intent interaction: changing output mode clears a resolved dependent schema error atomically",
    outputSchemaField["aria-invalid"] === "true" && W.agentRevalidateEditedField(outputModeField) &&
    outputSchemaField["aria-invalid"] === undefined && uiElements.get("agent-form-error").textContent === "");
  W.agentSetShortcutLocks({ config: { studio_intent: { format: "vendor/v2" } } });
  check("visual intent interaction: unknown stored contracts expose a visible described lock reason",
    uiElements.get("inp-agent-model").disabled && !uiElements.get("agent-intent-lock").hidden &&
    uiElements.get("agent-intent-lock").textContent.includes("exact JSON editor"));
  W.agentSetShortcutLocks(null);
  check("visual intent interaction: a fresh draft clears the lock note and re-enables controls",
    !uiElements.get("inp-agent-model").disabled && uiElements.get("agent-intent-lock").hidden &&
    uiElements.get("agent-intent-lock").textContent === "");
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
  check("detail exposes immutable version identity and lifecycle entry points",
    detail.includes(VERSION1) && detail.includes('data-agent-versions="research-coordinator"') &&
    detail.includes('data-agent-version-create="research-coordinator"') && detail.includes(">Version history</button>"));
  const archivedAgent = { ...agent, archived_at: "2026-08-10T04:00:00Z" };
  const archivedDetail = W.agentDetailHtml(archivedAgent,
    W.agentReadiness(archivedAgent, info, run), run, history, info);
  check("detail keeps archived evidence inspectable while disabling new work and offering restore",
    archivedDetail.includes('data-agent-lifecycle="restore"') && archivedDetail.includes(">Restore</button>") &&
    archivedDetail.includes("Restore it before starting new work") &&
    archivedDetail.includes('data-agent-run="research-coordinator" disabled') && archivedDetail.includes("Recent runs"));
  check("detail makes configuration portable and explains its runtime contract",
    detail.includes('data-agent-export="research-coordinator"') && detail.includes(">Export manifest</button>") &&
    detail.includes('aria-labelledby="agent-config-summary-heading"') &&
    detail.includes('id="agent-config-summary-heading">Configuration contract') &&
    detail.includes("Runs with") && detail.includes("Preserves"));
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
  check("configuration workshop exposes bounded import, draft export, and an explicit apply step",
    html.includes('id="inp-agent-import" type="file" accept="application/json,.json" hidden') &&
    html.includes('id="btn-agent-export-draft"') && html.includes('id="inp-agent-manifest"') &&
    html.includes('id="btn-agent-manifest-apply"') && html.includes("Unknown top-level fields are rejected"));
  check("configuration workshop preserves server number provenance at the copy-open boundary",
    html.includes("store.agentCopySource = agentCopySourceSnapshot(source)") &&
    html.includes("This agent cannot be copied without changing a stored JSON number token."));
  check("configuration workshop labels descriptive fields honestly and announces validation",
    html.includes("The current runtime does not inject it as instructions") &&
    html.includes("Guided fields changed · refresh JSON before editing it") &&
    html.includes("JSON edited · apply or refresh before creating or exporting") &&
    html.includes("Discard unapplied JSON edits") &&
    html.includes('id="agent-form-error" role="alert"') &&
    html.includes('id="agent-manifest-status" role="status"') &&
    html.includes('id="agent-contract-preview" aria-labelledby="agent-contract-title"'));
  check("visual intent canvas exposes model, tools, memory, approval, output, and budgets as labelled native controls",
    html.includes('id="agent-intent-model-title"') && html.includes('id="inp-agent-model"') &&
    html.includes('id="inp-agent-tools" maxlength="4096"') && html.includes('id="sel-agent-memory-mode"') &&
    html.includes('data-agent-scope="tenant"') && html.includes('id="sel-agent-approval"') &&
    html.includes('id="sel-agent-output-mode"') && html.includes('id="inp-agent-output-schema"') &&
    html.includes('id="inp-agent-budget-tokens"') && html.includes('id="inp-agent-budget-cost"') &&
    html.includes('id="inp-agent-budget-latency"'));
  check("visual intent canvas never claims portable requirements are already runtime-enforced",
    html.includes("Today it does not bind or enforce model, tool, memory, approval, output, or budget providers") &&
    html.includes("A declared requirement, not a browser-side permission grant") &&
    html.includes("Use an identifier or registry reference, not a URL or credential"));
  check("visual intent canvas collapses the wiring bench into one mobile column",
    html.includes(".agent-intent-grid { grid-template-columns: 1fr; }") &&
    html.includes(".agent-intent-shell::before, .agent-intent-card::before, .agent-intent-card::after { display: none; }"));
  check("visual intent validation associates the focused field with the announced error",
    html.includes('describedBy.add("agent-form-error")') && html.includes('id="agent-form-error" role="alert"'));
  check("configuration change review has a labelled source-bound decision surface",
    html.includes('class="agent-review-stack" aria-label="Configuration review"') &&
    html.includes('id="agent-change-review" aria-labelledby="agent-change-title" hidden') &&
    html.includes('class="agent-change-list" role="list"') &&
    html.includes("store.agentCopySource || store.agentManifestSource"));
  check("configuration change review stacks before mobile text becomes unreadable",
    html.includes(".agent-change-row { grid-template-columns: 1fr; gap: 5px; }") &&
    html.includes(".agent-change-values { grid-template-columns: 1fr; }") &&
    html.includes(".agent-review-stack { position: static; }"));
  check("assistant version rail is labelled, responsive, and announced outside rerendered detail",
    html.includes('id="agent-version-announcer" role="status" aria-live="polite" aria-atomic="true"') &&
    html.includes('id="agent-version-title" tabindex="-1"') &&
    html.includes(".agent-version-head { flex-direction: column; }") &&
    html.includes(".agent-version-item { grid-template-columns: 1fr; }") &&
    html.includes('data-agent-version-activate="'));
  check("assistant lifecycle shelf is filtered, announced, and mobile-safe",
    html.includes('id="sel-agent-lifecycle" aria-label="Agent lifecycle"') &&
    html.includes('id="agent-lifecycle-announcer" role="status" aria-live="polite"') &&
    html.includes('action === "unavailable-agents"') && html.includes("Open All to inspect without enabling new work") &&
    html.includes(".agent-toolbar input { flex-basis: 100%; min-width: 100%; }") &&
    html.includes(".agent-lifecycle-facts { grid-template-columns: 1fr; gap: 2px; }"));
  check("configuration workshop carries responsive and reduced-motion quality hooks",
    html.includes(".agent-workshop { grid-template-columns: 1fr;") &&
    html.includes('@media (prefers-reduced-motion: reduce)'));
  check("copy outcomes are announced through an atomic live region",
    html.includes('id="toast" role="status" aria-live="polite" aria-atomic="true"'));
}

{
  vm.runInContext(`
    globalThis.__versionToasts = [];
    agentsRender = () => {};
    openAgents = () => {};
    toast = (message, tone) => globalThis.__versionToasts.push({ message, tone });
  `, sandbox);
  uiElements.clear();
  uiElements.set("agent-version-title", { focus() {} });
  uiElements.set("agent-version-announcer", { textContent: "" });
  const assistantId = "async-a";
  const otherId = "async-b";
  const v1 = `av-${"a".repeat(64)}`;
  const v2 = `av-${"b".repeat(64)}`;
  const current = {
    assistant_id: assistantId, name: "Async A", graph: "pipeline", config: { model: "stable" },
    metadata: null, created_at: "2026-08-10T00:00:00Z", active_version_id: v1, version_count: 2,
  };
  const other = { ...current, assistant_id: otherId, name: "Async B", active_version_id: `av-${"c".repeat(64)}`, version_count: 1 };
  const summary = { version_id: v2, parent_version_id: v1, graph: "pipeline", created_at: "2026-08-10T01:00:00Z", active: false };
  const candidate = { ...summary, name: "Async A next", config: { model: "next" }, metadata: null };
  const baseState = () => ({
    assistantId, activeVersionId: v1, assistant: current,
    versions: [summary, { version_id: v1, parent_version_id: null, graph: "pipeline", created_at: current.created_at, active: true }],
    loading: false, error: null, reviewing: "", pendingActivation: null, submitting: false,
  });
  W.store.conn = { baseUrl: "/api", apiKey: "tenant-a" };
  W.store.connectionEpoch = 7;
  W.store.info = info;
  W.store.agents = { list: [current, other], selected: assistantId, error: null };
  W.store.agentVersions = baseState();
  fetchFailure = null;
  let release;
  fetchHandler = () => new Promise((resolve) => { release = resolve; });
  const pendingReview = W.agentVersionReview(assistantId, v2);
  await Promise.resolve();
  W.homeNavigate("agent-run", "", otherId);
  release({
    ok: true, status: 200,
    async text() { return JSON.stringify({ assistant_id: assistantId, active_version_id: v1, version: candidate }); },
  });
  await pendingReview;
  check("assistant versions: deferred exact review cannot return into another selected agent",
    W.store.agents.selected === otherId && W.store.agentVersions === null &&
    sandbox.__versionToasts.length === 0 && uiElements.get("agent-version-announcer").textContent === "");

  W.store.agents.selected = assistantId;
  W.store.agentVersions = { ...baseState(), pendingActivation: candidate };
  release = null;
  const pendingActivation = W.agentVersionActivate(assistantId, v2);
  await Promise.resolve();
  W.homeNavigate("agent-run", "", otherId);
  release({
    ok: true, status: 200,
    async text() {
      return JSON.stringify({
        activated: true,
        assistant: { ...current, name: candidate.name, config: candidate.config, active_version_id: v2 },
      });
    },
  });
  await pendingActivation;
  check("assistant versions: deferred activation cannot mutate catalog or announce after selected-agent ownership changes",
    W.store.agents.selected === otherId && W.store.agents.list[0].active_version_id === v1 &&
    W.store.agentVersions === null && sandbox.__versionToasts.length === 0);

  uiElements.set("agent-lifecycle-title", { focus() {} });
  uiElements.set("agent-lifecycle-announcer", { textContent: "" });
  uiElements.set("sel-agent-lifecycle", { value: "active" });
  uiElements.set("agent-side-count", { textContent: "" });
  uiElements.set("agent-detail", { querySelector() { return { focus() {} }; } });
  sandbox.__versionToasts.length = 0;
  W.store.agents = { list: [current, other], selected: assistantId, error: null };
  W.store.agentLifecycleReview = null;
  release = null;
  fetchHandler = () => new Promise((resolve) => { release = resolve; });
  const pendingLifecycleReview = W.agentLifecycleOpen(assistantId, "archive");
  await Promise.resolve();
  W.homeNavigate("agent-run", "", otherId);
  release({ ok: true, status: 200, async text() { return JSON.stringify(current); } });
  await pendingLifecycleReview;
  check("assistant lifecycle: deferred exact review cannot return into another selected agent",
    W.store.agents.selected === otherId && W.store.agentLifecycleReview === null &&
    sandbox.__versionToasts.length === 0 && uiElements.get("agent-lifecycle-announcer").textContent === "");

  W.store.agents.selected = assistantId;
  W.store.agentLifecycleReview = null;
  fetchCalls.length = 0;
  fetchHandler = async (url, options) => {
    if (options.method === "GET") return { ok: true, status: 200, async text() { return JSON.stringify(current); } };
    return { ok: true, status: 200, async text() { return JSON.stringify({
      changed: true, lifecycle: "archived", assistant: { ...current, archived_at: "2026-08-10T04:00:00Z" },
    }); } };
  };
  check("assistant lifecycle: exact review opens from a corroborated server snapshot",
    await W.agentLifecycleOpen(assistantId, "archive") && W.store.agentLifecycleReview.snapshot.active_version_id === v1);
  check("assistant lifecycle: successful archive sends only the reviewed serving guard and moves to the retained shelf",
    await W.agentLifecycleApply() && W.store.agents.list[0].archived_at === "2026-08-10T04:00:00Z" &&
    W.store.agentLifecycleFilter === "archived" &&
    JSON.parse(fetchCalls.at(-1).options.body).expected_active_version_id === v1 &&
    Object.keys(JSON.parse(fetchCalls.at(-1).options.body)).length === 1);

  const archivedCurrent = { ...current, archived_at: "2026-08-10T04:00:00Z" };
  W.store.view = "agents";
  W.store.agents = { list: [current, other], selected: assistantId, error: null };
  W.store.agentLifecycleFilter = "active";
  fetchHandler = async (url, options) => {
    if (options.method === "GET") return { ok: true, status: 200, async text() { return JSON.stringify(current); } };
    return new Promise((resolve) => { release = resolve; });
  };
  await W.agentLifecycleOpen(assistantId, "archive");
  const successAnnouncement = uiElements.get("agent-lifecycle-announcer").textContent;
  const successToasts = sandbox.__versionToasts.length;
  release = null;
  const pendingHiddenSuccess = W.agentLifecycleApply();
  await Promise.resolve();
  W.store.view = "home";
  release({ ok: true, status: 200, async text() { return JSON.stringify({
    changed: true, lifecycle: "archived", assistant: archivedCurrent,
  }); } });
  check("assistant lifecycle: a deferred success updates catalog truth without focusing or announcing inside a workspace the user left",
    await pendingHiddenSuccess === true && W.store.view === "home" && W.store.agentLifecycleReview === null &&
    W.agentLifecycleState(W.store.agents.list[0]) === "archived" &&
    !W.agentReadiness(W.store.agents.list[0], info, null).steps[1].ready && sandbox.__versionToasts.length === successToasts &&
    uiElements.get("agent-lifecycle-announcer").textContent === successAnnouncement);

  W.store.view = "agents";
  W.store.conn = { baseUrl: "/api", apiKey: "tenant-a" };
  W.store.connectionEpoch = 7;
  W.store.agents = { list: [current, other], selected: assistantId, error: null };
  fetchHandler = async (url, options) => {
    if (options.method === "GET") return { ok: true, status: 200, async text() { return JSON.stringify(current); } };
    return new Promise((resolve) => { release = resolve; });
  };
  await W.agentLifecycleOpen(assistantId, "archive");
  release = null;
  const pendingTenantReceipt = W.agentLifecycleApply();
  await Promise.resolve();
  const tenantBRecord = { ...current, name: "Tenant B same ID", config: { model: "tenant-b" } };
  W.store.conn = { baseUrl: "/api", apiKey: "tenant-b" };
  W.store.connectionEpoch = 8;
  W.store.agentLifecycleRequest += 1;
  W.store.agentLifecycleReview = null;
  W.store.agents = { list: [tenantBRecord, other], selected: assistantId, error: null };
  release({ ok: true, status: 200, async text() { return JSON.stringify({
    changed: true, lifecycle: "archived", assistant: archivedCurrent,
  }); } });
  check("assistant lifecycle: a deferred same-ID receipt can never cross a connection or tenant boundary",
    await pendingTenantReceipt === false && W.store.agents.list[0].name === "Tenant B same ID" &&
    W.store.agents.list[0].config.model === "tenant-b" && W.agentLifecycleState(W.store.agents.list[0]) === "active");

  W.store.view = "agents";
  W.store.conn = { baseUrl: "/api", apiKey: "tenant-a" };
  W.store.connectionEpoch = 9;
  W.store.agents = { list: [current, other], selected: assistantId, error: null };
  W.store.agentLifecycleFilter = "active";
  let restoredFilterFocus = 0;
  const lifecycleFilter = { id: "sel-agent-lifecycle", value: "active", focus() { restoredFilterFocus += 1; } };
  uiElements.set("sel-agent-lifecycle", lifecycleFilter);
  uiElements.set("agents-view", { contains(element) { return element === lifecycleFilter; }, querySelectorAll() { return []; } });
  sandbox.document.activeElement = lifecycleFilter;
  vm.runInContext("globalThis.__agentRenderCount = 0; agentsRender = () => { globalThis.__agentRenderCount += 1; };", sandbox);
  fetchHandler = async (url, options) => {
    if (options.method === "GET") return { ok: true, status: 200, async text() { return JSON.stringify(current); } };
    return new Promise((resolve) => { release = resolve; });
  };
  await W.agentLifecycleOpen(assistantId, "archive");
  release = null;
  const pendingVisibleSuccess = W.agentLifecycleApply();
  await Promise.resolve();
  W.agentSetLifecycleFilter("all", false);
  lifecycleFilter.value = "all";
  W.agentSelectAssistant(otherId);
  const rendersBeforeReceipt = sandbox.__agentRenderCount;
  release({ ok: true, status: 200, async text() { return JSON.stringify({
    changed: true, lifecycle: "archived", assistant: archivedCurrent,
  }); } });
  check("assistant lifecycle: a stale-view success refreshes visible truth without taking newer filter, selection, or focus",
    await pendingVisibleSuccess === true && W.store.agentLifecycleFilter === "all" &&
    W.store.agents.selected === otherId && W.agentLifecycleState(W.store.agents.list[0]) === "archived" &&
    sandbox.__agentRenderCount === rendersBeforeReceipt + 1 && restoredFilterFocus === 1);
  sandbox.document.activeElement = null;

  W.store.view = "agents";
  W.store.agents = { list: [current, other], selected: assistantId, error: null };
  fetchHandler = async (url, options) => {
    if (options.method === "GET") return { ok: true, status: 200, async text() {
      return JSON.stringify(url.endsWith(`/assistants/${assistantId}`) ? current : [archivedCurrent, other]);
    } };
    return new Promise((resolve, reject) => { release = () => reject(new TypeError("receipt connection closed")); });
  };
  await W.agentLifecycleOpen(assistantId, "archive");
  const failureAnnouncement = uiElements.get("agent-lifecycle-announcer").textContent;
  const failureToasts = sandbox.__versionToasts.length;
  release = null;
  const pendingHiddenFailure = W.agentLifecycleApply();
  await Promise.resolve();
  W.store.view = "home";
  release();
  check("assistant lifecycle: a deferred lost receipt silently reconciles catalog truth without announcing after workspace navigation",
    await pendingHiddenFailure === false && W.store.view === "home" && W.store.agentLifecycleReview === null &&
    W.agentLifecycleState(W.store.agents.list[0]) === "archived" &&
    sandbox.__versionToasts.length === failureToasts &&
    uiElements.get("agent-lifecycle-announcer").textContent === failureAnnouncement);

  W.store.view = "agents";
  W.store.agents = { list: [archivedCurrent, other], selected: assistantId, error: null };
  W.store.agentLifecycleFilter = "archived";
  fetchHandler = async (url, options) => {
    if (options.method === "POST") throw new TypeError("connection closed before the receipt arrived");
    if (url.endsWith(`/assistants/${assistantId}`)) {
      return { ok: true, status: 200, async text() { return JSON.stringify(archivedCurrent); } };
    }
    return { ok: true, status: 200, async text() { return JSON.stringify([current, other]); } };
  };
  check("assistant lifecycle: restore review opens from the retained archived snapshot",
    await W.agentLifecycleOpen(assistantId, "restore"));
  check("assistant lifecycle: a lost receipt reconciles authoritative state without retaining a stale review",
    await W.agentLifecycleApply() && W.store.agentLifecycleReview === null &&
    W.store.agents.selected === assistantId && W.store.agentLifecycleFilter === "active" &&
    !W.store.agents.list[0].archived_at &&
    uiElements.get("agent-lifecycle-announcer").textContent.includes("confirmed after refresh"));

  W.store.agents = { list: [archivedCurrent, other], selected: assistantId, error: null };
  W.store.agentLifecycleFilter = "archived";
  uiElements.get("sel-agent-lifecycle").value = "archived";
  release = null;
  fetchHandler = async (url, options) => {
    if (options.method === "POST") throw new TypeError("connection closed before the receipt arrived");
    if (url.endsWith(`/assistants/${assistantId}`)) {
      return { ok: true, status: 200, async text() { return JSON.stringify(archivedCurrent); } };
    }
    return new Promise((resolve) => { release = resolve; });
  };
  await W.agentLifecycleOpen(assistantId, "restore");
  const announcementBeforeRecovery = uiElements.get("agent-lifecycle-announcer").textContent;
  const toastCountBeforeRecovery = sandbox.__versionToasts.length;
  const pendingRecovery = W.agentLifecycleApply();
  for (let turn = 0; turn < 6 && !release; turn += 1) await Promise.resolve();
  W.agentSetLifecycleFilter("active", false);
  release({ ok: true, status: 200, async text() { return JSON.stringify([current, other]); } });
  check("assistant lifecycle: deferred lost-receipt reconciliation cannot override a newer filter and selection",
    await pendingRecovery === false && W.store.agents.selected === otherId &&
    W.store.agentLifecycleFilter === "active" && W.store.agentLifecycleReview === null &&
    uiElements.get("agent-lifecycle-announcer").textContent === announcementBeforeRecovery &&
    sandbox.__versionToasts.length === toastCountBeforeRecovery);
  fetchHandler = null;
}

console.log(`\n${passed} passed, ${failed} failed`);
process.exit(failed ? 1 : 0);
