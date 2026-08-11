#!/usr/bin/env node
/* Focused wire-contract, rendering, and async-ownership tests for Studio's
 * Schedule Desk. The page bootstrap is stripped and the embedded helpers run
 * in a dependency-free VM, matching the other Studio suites.
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
const sandbox = { URL, URLSearchParams, CSS: { escape: (value) => String(value) } };
vm.createContext(sandbox);
vm.runInContext(source + `
globalThis.__schedule = {
  store, ApiError, agentParseJsonWithNumberKinds, scheduleTimestamp, scheduleSafeId, scheduleContract,
  scheduleListContract, scheduleCadence, scheduleStableValue, scheduleCreatedRecordMatches,
  scheduleCreateDraft, scheduleGeneratedId, scheduleCreateBody, scheduleCreateReceipt,
  scheduleDeleteReceipt, scheduleErrorHtml, scheduleVisibleList, scheduleRenderWindow,
  scheduleRowHtml, scheduleDetailHtml, scheduleSummaryHtml, scheduleRequestCurrent, scheduleKeyboardMove, scheduleFormEdited,
  navigationParseSearch, navigationBuildUrl, schedulesLoad, scheduleCreateSubmit,
  scheduleRemovalSubmit, connectionResetWorkspace,
};`, sandbox, { filename: "index.html<script>" });
const S = sandbox.__schedule;

let passed = 0, failed = 0;
function check(name, condition, detail = "") {
  if (condition) { passed++; console.log(`ok   ${name}`); }
  else { failed++; console.log(`FAIL ${name}${detail ? ` — ${detail}` : ""}`); }
}

const base = {
  cron_id: "nightly-quality",
  graph: "quality_graph",
  interval_secs: 3600,
  input: { dataset: "nightly" },
  metadata: { source: "studio_schedule_desk" },
  on_run_completed: "keep",
  created_at: "2026-08-10T20:00:00.123456Z",
  last_run_at: null,
  runs_fired: 0,
};
const record = (extra = {}) => ({ ...base, ...extra });
const cronRecord = (extra = {}) => {
  const value = record(extra);
  delete value.interval_secs;
  return value;
};
const createRecord = (extra = {}) => record({ input: S.agentParseJsonWithNumberKinds('{"sequence":18446744073709551615}'), ...extra });

/* Exact CronRecord wire contract. */
check("record contract: interval schedule retains exact cadence and counters",
  S.scheduleContract(base)?.interval === 3600n && S.scheduleContract(base)?.runs === 0n);
check("record contract: schedule identities reject ASCII controls and hidden Unicode formatting",
  !S.scheduleContract(record({ cron_id: "nightly\tquality" })) && !S.scheduleContract(record({ cron_id: "nightly\u202equality" })) &&
  S.scheduleSafeId("nightly-quality") === "nightly-quality");
check("record contract: UTC cron expression is the exclusive cadence form", Boolean(S.scheduleContract(cronRecord({
  cron_expr: "15 2 * * 1", last_run_at: "2026-08-10T21:00:00Z", runs_fired: 1,
}))));
check("record contract: both or neither cadence fields fail closed",
  !S.scheduleContract(record({ cron_expr: "15 2 * * 1" })) &&
  !S.scheduleContract(({ ...base, interval_secs: undefined })));
check("record contract: explicit null interval is not confused with omission",
  !S.scheduleContract(record({ interval_secs: null })));
check("record contract: interval bounds mirror the server",
  !S.scheduleContract(record({ interval_secs: 0 })) &&
  !S.scheduleContract(record({ interval_secs: 31536001 })) &&
  Boolean(S.scheduleContract(record({ interval_secs: 31536000 }))));
check("record contract: input is null or an object, never a scalar",
  Boolean(S.scheduleContract(record({ input: null }))) && !S.scheduleContract(record({ input: [] })) && !S.scheduleContract(record({ input: "state" })));
check("record contract: lifecycle and scheduler timestamps are strict RFC3339",
  !S.scheduleContract(record({ created_at: "08/10/2026" })) &&
  !S.scheduleContract(record({ last_run_at: "2026-08-10" })));
check("record contract: scheduler counters, timestamps, and exact wire fields stay coherent",
  !S.scheduleContract(record({ runs_fired: 1, last_run_at: null })) &&
  !S.scheduleContract(record({ runs_fired: 0, last_run_at: "2026-08-10T21:00:00Z" })) &&
  !S.scheduleContract(record({ unreviewed: true })));
{
  const missingMetadata = { ...base }; delete missingMetadata.metadata;
  check("record contract: completion policy and metadata presence are mandatory wire evidence",
    !S.scheduleContract(record({ on_run_completed: "archive" })) && !S.scheduleContract(missingMetadata));
}
{
  const exact = S.agentParseJsonWithNumberKinds(JSON.stringify(base)
    .replace('"last_run_at":null', '"last_run_at":"2026-08-10T21:00:00Z"')
    .replace('"runs_fired":0', '"runs_fired":18446744073709551615'));
  check("record contract: legal unsafe u64 tokens remain exact before server-bound validation",
    S.scheduleContract(exact)?.runs === 18446744073709551615n);
}
check("catalog contract: one malformed or duplicate record invalidates the complete snapshot",
  !S.scheduleListContract([base, base]) && !S.scheduleListContract([base, { cron_id: "broken" }]));
{
  const list = Array.from({ length: 201 }, (_, index) => record({ cron_id: `schedule-${index}` }));
  const windowed = S.scheduleRenderWindow(list, "schedule-200");
  check("catalog rendering: a selected record outside the first 200 remains operable",
    windowed.length === 200 && windowed.at(-1).cron_id === "schedule-200");
}

/* Authoring and exact create/delete receipts. */
const fields = {
  cronId: "nightly-quality", graph: "quality_graph", mode: "interval",
  interval: "1", unit: "3600", cronExpr: "", rawInput: '{"sequence":18446744073709551615}',
  completion: "keep", acknowledged: true,
};
const draft = S.scheduleCreateDraft(fields, ["quality_graph"]);
check("create preflight: interval units convert to exact server seconds", draft.value?.intervalSecs === 3600);
check("create preflight: reviewed JSON preserves an unsafe u64 token in the request body",
  S.scheduleCreateBody(draft.value, fields.cronId).includes('"sequence":18446744073709551615'));
check("create preflight: acknowledgement and live graph membership are required",
  S.scheduleCreateDraft({ ...fields, acknowledged: false }, ["quality_graph"]).errors.acknowledged &&
  S.scheduleCreateDraft(fields, ["other_graph"]).errors.graph);
check("create preflight: leading zeros, zero, and over-year intervals are rejected",
  S.scheduleCreateDraft({ ...fields, interval: "01" }, ["quality_graph"]).errors.interval &&
  S.scheduleCreateDraft({ ...fields, interval: "0" }, ["quality_graph"]).errors.interval &&
  S.scheduleCreateDraft({ ...fields, interval: "366", unit: "86400" }, ["quality_graph"]).errors.interval);
check("create preflight: cron authoring requires exactly five visible fields",
  S.scheduleCreateDraft({ ...fields, mode: "cron", cronExpr: "15 2 * * 1" }, ["quality_graph"]).value?.cronExpr === "15 2 * * 1" &&
  S.scheduleCreateDraft({ ...fields, mode: "cron", cronExpr: "15 2 * *" }, ["quality_graph"]).errors.cronExpr &&
  S.scheduleCreateDraft({ ...fields, mode: "cron", cronExpr: " 15 2 * * 1" }, ["quality_graph"]).errors.cronExpr &&
  S.scheduleCreateDraft({ ...fields, mode: "cron", cronExpr: "15\t2 * * 1" }, ["quality_graph"]).errors.cronExpr);
check("create preflight: initial state must be an object within the UTF-8 byte ceiling",
  S.scheduleCreateDraft({ ...fields, rawInput: "[]" }, ["quality_graph"]).errors.rawInput &&
  S.scheduleCreateDraft({ ...fields, rawInput: JSON.stringify({ value: "é".repeat(33000) }) }, ["quality_graph"]).errors.rawInput);
check("create preflight: optional ID can be generated while unsafe IDs fail before transport",
  Boolean(S.scheduleCreateDraft({ ...fields, cronId: "" }, ["quality_graph"]).value) &&
  S.scheduleCreateDraft({ ...fields, cronId: "../schedule" }, ["quality_graph"]).errors.cronId &&
  S.scheduleCreateDraft({ ...fields, cronId: "nightly\tquality" }, ["quality_graph"]).errors.cronId &&
  S.scheduleCreateDraft({ ...fields, cronId: "nightly\u200bquality" }, ["quality_graph"]).errors.cronId &&
  /^schedule-[a-z0-9]+-[a-z0-9]+$/.test(S.scheduleGeneratedId(123456, () => 0.25)));
check("create receipt: exact fresh server record proves the reviewed cadence",
  S.scheduleCreateReceipt(createRecord(), draft.value, fields.cronId)?.cron_id === fields.cronId);
check("create receipt: graph, input, metadata, completion, and fresh counters are all bound",
  !S.scheduleCreateReceipt(createRecord({ graph: "other" }), draft.value, fields.cronId) &&
  !S.scheduleCreateReceipt(createRecord({ input: { sequence: 7 } }), draft.value, fields.cronId) &&
  !S.scheduleCreateReceipt(createRecord({ metadata: {} }), draft.value, fields.cronId) &&
  !S.scheduleCreateReceipt(createRecord({ on_run_completed: "delete" }), draft.value, fields.cronId) &&
  !S.scheduleCreateReceipt(createRecord({ last_run_at: "2026-08-10T21:00:00Z", runs_fired: 1 }), draft.value, fields.cronId));
check("lost-receipt reconciliation: progressed scheduler evidence is accepted only when internally coherent",
  Boolean(S.scheduleCreatedRecordMatches(createRecord({ last_run_at: "2026-08-10T21:00:00Z", runs_fired: 1 }), draft.value, fields.cronId, false)) &&
  !S.scheduleCreatedRecordMatches(createRecord({ last_run_at: null, runs_fired: 1 }), draft.value, fields.cronId, false));
check("delete receipt: exact ID and boolean are required without unreviewed fields",
  Boolean(S.scheduleDeleteReceipt({ cron_id: fields.cronId, deleted: true }, fields.cronId)) &&
  !S.scheduleDeleteReceipt({ cron_id: "other", deleted: true }, fields.cronId) &&
  !S.scheduleDeleteReceipt({ cron_id: fields.cronId, deleted: true, status: "ok" }, fields.cronId));

/* Rendering, privacy, navigation, and responsive invariants. */
check("cadence copy distinguishes human interval from five-field UTC expression",
  S.scheduleCadence(base) === "Every 1 hour" && S.scheduleCadence(cronRecord({ cron_expr: "15 2 * * 1" })) === "15 2 * * 1 · UTC");
check("row rendering: hostile identities are escaped and one native option carries selection",
  !S.scheduleRowHtml(record({ cron_id: '<img src=x onerror="alert(1)">' }), true, true).includes("<img") &&
  S.scheduleRowHtml(base, true, true).includes('role="option"') && S.scheduleRowHtml(base, true, true).includes('aria-selected="true"'));
check("row accessibility: server-legal long and bidi evidence is normalized under a bounded name",
  S.scheduleRowHtml(record({ graph: "model-" + "g".repeat(10000) }), false).length < 7000 &&
  S.scheduleRowHtml(record({ graph: "model\u202e-hidden" }), false).includes("\\u{202e}"));
check("detail truth: cadence evidence does not invent thread, run, success, or effect identity",
  S.scheduleDetailHtml({ removal: null }, base).includes("does not carry the fresh thread or run IDs") &&
  S.scheduleDetailHtml({ removal: null }, base).includes("does not prove that the run was accepted"));
check("detail truth: one-shot removal and explicit operator removal boundaries are visible",
  S.scheduleDetailHtml({ removal: null }, record({ on_run_completed: "delete" })).includes("removes itself") &&
  S.scheduleDetailHtml({ removal: null }, base).includes("Review removal"));
check("error rendering: raw route 404 is a capability gap while hostile messages are escaped and bounded",
  S.scheduleErrorHtml({ status: 404, body: { raw: "not found" }, message: "404" }).includes("does not expose durable schedule routes") &&
  !S.scheduleErrorHtml({ status: 500, message: '<img src=x onerror="alert(1)">' }).includes("<img") &&
  S.scheduleErrorHtml({ status: 500, message: "x".repeat(10000) }).includes("exact preview truncated"));
check("filters: search and completion policy compose without inferring run outcome",
  S.scheduleVisibleList({ list: [base, record({ cron_id: "one-shot", on_run_completed: "delete" })], query: "quality", filter: "keep" }).length === 1 &&
  S.scheduleVisibleList({ list: [base], query: "success", filter: "all" }).length === 0);
check("summary: durable schedules, lifecycle policy, and firing counters remain distinct",
  S.scheduleSummaryHtml({ list: [base, record({ cron_id: "one", on_run_completed: "delete", runs_fired: 2, last_run_at: "2026-08-10T21:00:00Z" })] }).includes("1 / 1</b><span>recurring / one-shot") &&
  S.scheduleSummaryHtml({ list: [base, record({ cron_id: "one", on_run_completed: "delete", runs_fired: 2, last_run_at: "2026-08-10T21:00:00Z" })] }).includes("2</b><span>server firing count"));
check("navigation: exact schedule links reject unknown query keys and round-trip the selected record",
  S.navigationParseSearch("?studio=schedules&schedule=nightly-quality").route?.schedule === "nightly-quality" &&
  !S.navigationParseSearch("?studio=schedules&schedule=nightly-quality&api_key=secret").route &&
  S.navigationBuildUrl({ view: "schedules", schedule: "nightly-quality" }, "https://studio.local/").includes("schedule=nightly-quality"));
check("markup: sidebar, workspace, native form, listbox, acknowledgement, and live announcer are present",
  page.includes('id="btn-schedules-open"') && page.includes('id="schedules-view"') && page.includes('id="schedule-form"') &&
  page.includes('role="listbox" aria-label="Durable schedules"') && page.includes('id="chk-schedule-review"') && page.includes('id="schedule-announcer"') &&
  page.includes('id="schedule-mutation-error" role="alert"') && page.includes("Do not put credentials or secrets here"));
check("responsive: cadence rail, two-pane evidence, composer, and detail stack deliberately",
  page.includes(".schedule-rhythm,.schedule-toolbar,.schedule-form { grid-template-columns:1fr; }") && page.includes(".schedule-layout { grid-template-columns:1fr; }") &&
  page.includes(".schedule-list { grid-template-columns:1fr; max-height:300px; }") && page.includes(".schedule-detail-head { flex-direction:column; }"));
check("connection reset: schedule reads, mutations, catalog, and page-memory form state are invalidated",
  page.includes("store.scheduleRequest += 1") && page.includes("store.scheduleMutationRequest += 1") && page.includes("store.schedules = null") &&
  page.includes('$("schedule-form").reset()'));

/* Real deferred ownership and reconciliation behavior. */
const nodes = new Map();
const field = (value = "") => ({ value, checked: false, disabled: false, hidden: false, textContent: "", innerHTML: "",
  getAttribute: () => null, setAttribute: () => {}, removeAttribute: () => {}, focus: () => {}, reset: () => {} });
for (const [id, value] of Object.entries({
  "inp-schedule-id": "nightly-quality", "sel-schedule-graph": "quality_graph", "inp-schedule-interval": "1",
  "sel-schedule-unit": "3600", "inp-schedule-cron": "", "sel-schedule-completion": "keep",
  "inp-schedule-input": '{"sequence":18446744073709551615}', "chk-schedule-review": "",
  "btn-schedule-submit": "", "schedule-form-error": "", "schedule-compose": "", "btn-schedule-create": "",
  "schedule-form": "", "schedules-side-count": "", "schedule-announcer": "",
})) nodes.set(id, field(value));
nodes.get("chk-schedule-review").checked = true;
nodes.get("schedule-compose").hidden = false;
sandbox.document = {
  getElementById: (id) => nodes.get(id) || null,
  querySelector: (selector) => selector.includes("schedule-mode") ? { value: "interval" } : null,
};
vm.runInContext(`
schedulesRender = () => {};
scheduleUpdateSidebar = () => {};
schedulePopulateGraphs = () => {};
agentGraphNames = () => ["quality_graph"];
toast = () => {};
renderMain = () => {};
navigationReplaceManagedAddress = () => { globalThis.__navCalls = (globalThis.__navCalls || 0) + 1; };
`, sandbox);

S.scheduleFormEdited(nodes.get("inp-schedule-input"));
check("review integrity: changing a semantic field invalidates the prior exact acknowledgement",
  nodes.get("chk-schedule-review").checked === false && nodes.get("schedule-announcer").textContent.includes("acknowledge the exact schedule again"));
nodes.get("chk-schedule-review").checked = true;

{
  const pending = new Map();
  sandbox.__scheduleLoadApi = (connection) => new Promise((resolve) => pending.set(connection.baseUrl, resolve));
  vm.runInContext("apiForConnection = globalThis.__scheduleLoadApi", sandbox);
  S.store.connectionEpoch = 1; S.store.conn = { baseUrl: "http://tenant-a", apiKey: "a" }; S.store.schedules = null;
  const oldLoad = S.schedulesLoad(true);
  S.store.connectionEpoch = 2; S.store.conn = { baseUrl: "http://tenant-b", apiKey: "b" }; S.store.schedules = null;
  const newLoad = S.schedulesLoad(true);
  pending.get("http://tenant-b")([record({ cron_id: "tenant-b" })]); await newLoad;
  pending.get("http://tenant-a")([record({ cron_id: "tenant-a" })]); await oldLoad;
  check("async isolation: late tenant-A catalog cannot overwrite tenant B",
    S.store.schedules.list.length === 1 && S.store.schedules.list[0].cron_id === "tenant-b");
}

{
  let reads = 0;
  sandbox.__scheduleReconcileApi = async (connection, method) => {
    if (method === "POST") return { status: 201, body: { cron_id: "nightly-quality", created: true } };
    reads++; return [createRecord()];
  };
  vm.runInContext("apiForConnection = globalThis.__scheduleReconcileApi", sandbox);
  sandbox.__navCalls = 0;
  S.store.connectionEpoch = 3; S.store.conn = { baseUrl: "http://tenant-b", apiKey: "b" }; S.store.view = "schedules";
  S.store.schedules = { list: [], selected: "", query: "", filter: "all", removal: null };
  await S.scheduleCreateSubmit();
  check("create reconciliation: malformed 201 changes catalog truth only after exact durable GET corroboration",
    reads === 1 && S.store.schedules.list[0]?.cron_id === "nightly-quality" && S.store.schedules.selected === "nightly-quality",
    `reads=${reads} list=${S.store.schedules.list.map((item) => item.cron_id).join(",")} selected=${S.store.schedules.selected} error=${nodes.get("schedule-form-error").textContent}`);
  check("create reconciliation: matching GET state is present without fabricated creation provenance",
    nodes.get("schedule-announcer").textContent.includes("cannot prove this request created it") && sandbox.__navCalls === 1);
}

{
  let reads = 0;
  sandbox.__scheduleWrongStatusApi = async (connection, method) => {
    if (method === "POST") return { status: 200, body: createRecord() };
    reads++; return [createRecord()];
  };
  vm.runInContext("apiForConnection = globalThis.__scheduleWrongStatusApi", sandbox);
  nodes.get("chk-schedule-review").checked = true;
  nodes.get("inp-schedule-input").value = '{"sequence":18446744073709551615}';
  nodes.get("schedule-announcer").textContent = "";
  S.store.connectionEpoch = 4; S.store.conn = { baseUrl: "http://tenant-b", apiKey: "b" }; S.store.view = "schedules";
  S.store.schedules = { list: [], selected: "", query: "", filter: "all", removal: null };
  await S.scheduleCreateSubmit();
  check("create provenance: an exact-looking 200 body is reconciled as presence and never called a fresh creation",
    reads === 1 && S.store.schedules.list[0]?.cron_id === base.cron_id &&
    nodes.get("schedule-announcer").textContent.includes("cannot prove this request created it"),
    `reads=${reads} list=${S.store.schedules.list.map((item) => item.cron_id).join(",")} announce=${nodes.get("schedule-announcer").textContent}`);
}

{
  let reads = 0;
  sandbox.__scheduleEmptySuccessApi = async (connection, method) => {
    if (method === "POST") return { status: 204, body: null };
    reads++; return [createRecord()];
  };
  vm.runInContext("apiForConnection = globalThis.__scheduleEmptySuccessApi", sandbox);
  nodes.get("chk-schedule-review").checked = true;
  nodes.get("inp-schedule-input").value = '{"sequence":18446744073709551615}';
  nodes.get("schedule-announcer").textContent = "";
  S.store.connectionEpoch = 5; S.store.conn = { baseUrl: "http://tenant-b", apiKey: "b" }; S.store.view = "schedules";
  S.store.schedules = { list: [], selected: "", query: "", filter: "all", removal: null };
  await S.scheduleCreateSubmit();
  check("create retry safety: an empty successful response still reconciles the stable ID before retry is offered",
    reads === 1 && S.store.schedules.list[0]?.cron_id === base.cron_id &&
    nodes.get("schedule-announcer").textContent.includes("cannot prove this request created it"));
}

{
  let reads = 0;
  sandbox.__scheduleConflictApi = async (connection, method) => {
    if (method === "POST") throw new S.ApiError(409, { error: "conflict", message: "already exists" });
    reads++; return [base];
  };
  vm.runInContext("apiForConnection = globalThis.__scheduleConflictApi", sandbox);
  S.store.connectionEpoch = 6; S.store.conn = { baseUrl: "http://tenant-b", apiKey: "b" }; S.store.view = "schedules";
  S.store.schedules = { list: [], selected: "", query: "", filter: "all", removal: null };
  const created = await S.scheduleCreateSubmit();
  check("create collision: deterministic 409 never claims a pre-existing matching ID as this operation",
    created === false && reads === 0 && S.store.schedules.list.length === 0);
}

{
  sandbox.__scheduleLongCreateError = async () => { throw new S.ApiError(400, { error: "bad_request", message: "é".repeat(5000) }); };
  vm.runInContext("apiForConnection = globalThis.__scheduleLongCreateError", sandbox);
  nodes.get("chk-schedule-review").checked = true;
  S.store.connectionEpoch = 6; S.store.conn = { baseUrl: "http://tenant-b", apiKey: "b" }; S.store.view = "schedules";
  S.store.schedules = { list: [], selected: "", query: "", filter: "all", removal: null };
  await S.scheduleCreateSubmit();
  check("mutation bounds: hostile create errors stay inside the disclosed alert preview",
    nodes.get("schedule-form-error").textContent.includes("exact preview truncated") && nodes.get("schedule-form-error").textContent.length < 5000);
}

{
  let resolveDelete;
  const newer = record({ cron_id: "newer-choice" });
  sandbox.__scheduleDeleteApi = (connection, method) => method === "DELETE"
    ? new Promise((resolve) => { resolveDelete = resolve; }) : Promise.resolve([newer]);
  vm.runInContext("apiForConnection = globalThis.__scheduleDeleteApi", sandbox);
  sandbox.__navCalls = 0;
  S.store.connectionEpoch = 7; S.store.conn = { baseUrl: "http://tenant-b", apiKey: "b" }; S.store.view = "schedules";
  const original = { list: [base], selected: base.cron_id, query: "", filter: "all",
    removal: { cronId: base.cron_id, snapshot: S.scheduleStableValue(base), acknowledged: true, submitting: false, ambiguous: false, error: "" } };
  S.store.schedules = original;
  const removing = S.scheduleRemovalSubmit();
  S.store.schedules = { list: [base, newer], selected: newer.cron_id, query: "newer", filter: "all", removal: null };
  resolveDelete({ cron_id: base.cron_id, deleted: true }); await removing;
  check("async removal: exact same-tenant receipt updates current catalog without taking newer selection/filter",
    !S.store.schedules.list.some((item) => item.cron_id === base.cron_id) &&
    S.store.schedules.selected === newer.cron_id && S.store.schedules.query === "newer" && sandbox.__navCalls === 1);
}

{
  sandbox.__scheduleLongDeleteError = async (connection, method) => {
    if (method === "DELETE") throw new S.ApiError(403, { error: "forbidden", message: "é".repeat(5000) });
    return [base];
  };
  vm.runInContext("apiForConnection = globalThis.__scheduleLongDeleteError", sandbox);
  S.store.connectionEpoch = 7; S.store.conn = { baseUrl: "http://tenant-b", apiKey: "b" }; S.store.view = "schedules";
  S.store.schedules = { list: [base], selected: base.cron_id, query: "", filter: "all", mutationError: "",
    removal: { cronId: base.cron_id, snapshot: S.scheduleStableValue(base), acknowledged: true, submitting: false, ambiguous: false, error: "" } };
  const removed = await S.scheduleRemovalSubmit();
  check("mutation bounds: hostile delete errors stay inside the global alert preview",
    removed === false && S.store.schedules.mutationError.includes("exact preview truncated") && S.store.schedules.mutationError.length < 5000);
}

{
  sandbox.__scheduleAutoDeleteRefresh = async () => [record({ cron_id: "still-present" })];
  vm.runInContext("apiForConnection = globalThis.__scheduleAutoDeleteRefresh", sandbox);
  sandbox.__navCalls = 0;
  S.store.connectionEpoch = 8; S.store.conn = { baseUrl: "http://tenant-b", apiKey: "b" }; S.store.view = "schedules";
  S.store.schedules = { list: [base], selected: base.cron_id, query: "", filter: "all", removal: null, mutationError: "" };
  await S.schedulesLoad(true);
  check("managed URL: authoritative refresh clears an auto-removed selected target from the shared address",
    S.store.schedules.selected === "" && sandbox.__navCalls === 1);
}

{
  const replacement = record({ created_at: "2026-08-10T22:00:00Z", graph: "replacement_graph" });
  sandbox.__scheduleRecreatedApi = async (connection, method) => method === "DELETE"
    ? { cron_id: base.cron_id, deleted: true } : [replacement];
  vm.runInContext("apiForConnection = globalThis.__scheduleRecreatedApi", sandbox);
  S.store.connectionEpoch = 9; S.store.conn = { baseUrl: "http://tenant-b", apiKey: "b" }; S.store.view = "schedules";
  S.store.schedules = { list: [base], selected: base.cron_id, query: "", filter: "all", mutationError: "",
    removal: { cronId: base.cron_id, snapshot: S.scheduleStableValue(base), acknowledged: true, submitting: false, ambiguous: false, error: "" } };
  const removed = await S.scheduleRemovalSubmit();
  check("removal reconciliation: a recreated same-ID schedule remains visible and future firings are not declared stopped",
    removed === false && S.store.schedules.list[0].graph === "replacement_graph" &&
    S.store.schedules.mutationError.includes("newer durable schedule") && S.store.schedules.selected === base.cron_id);
}

{
  let resolveDelete;
  sandbox.__scheduleCrossTenantDelete = () => new Promise((resolve) => { resolveDelete = resolve; });
  vm.runInContext("apiForConnection = globalThis.__scheduleCrossTenantDelete", sandbox);
  S.store.connectionEpoch = 10; S.store.conn = { baseUrl: "http://tenant-a", apiKey: "a" }; S.store.view = "schedules";
  S.store.schedules = { list: [base], selected: base.cron_id, query: "", filter: "all",
    removal: { cronId: base.cron_id, snapshot: S.scheduleStableValue(base), acknowledged: true, submitting: false, ambiguous: false, error: "" } };
  const removing = S.scheduleRemovalSubmit();
  S.store.connectionEpoch = 11; S.store.conn = { baseUrl: "http://tenant-b", apiKey: "b" };
  S.store.schedules = { list: [base], selected: base.cron_id, query: "", filter: "all", removal: null };
  resolveDelete({ cron_id: base.cron_id, deleted: true }); await removing;
  check("async removal: late tenant-A receipt cannot delete a same-ID tenant-B schedule", S.store.schedules.list.length === 1);
}

{
  let focused = "";
  const oldRows = [base, record({ cron_id: "second" })].map((item) => ({ getAttribute: () => item.cron_id }));
  const replacement = { focus: () => { focused = "second"; } };
  nodes.set("schedule-list", { querySelectorAll: () => oldRows, querySelector: (selector) => selector.includes("second") ? replacement : null });
  S.store.view = "schedules";
  S.store.schedules = { list: [base, record({ cron_id: "second" })], selected: base.cron_id, query: "", filter: "all", removal: null };
  check("listbox keyboard: End selects and focuses the replacement DOM row after rerender",
    S.scheduleKeyboardMove(oldRows[0], "End") && S.store.schedules.selected === "second" && focused === "second");
}

console.log(`\n${passed} passed, ${failed} failed`);
if (failed) process.exit(1);
