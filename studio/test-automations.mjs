#!/usr/bin/env node
/* Focused contract, rendering, and async-ownership tests for the Studio
 * Automation Desk. The browser bootstrap is stripped and the embedded
 * helpers execute in a dependency-free VM, like the other Studio suites.
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
const sandbox = {};
vm.createContext(sandbox);
vm.runInContext(source + `
globalThis.__automation = {
  store, ApiError, agentParseJsonWithNumberKinds, automationSafeId, automationTimestamp,
  automationTarget, automationTriggerContract, automationEventContract, automationReplayAck, automationReplayCorroborated, automationRunJournalThread, automationListContract,
  automationRenderWindow, automationEventsContract, automationDeadLettersAgree, automationCreateDraft, automationCreateBody, automationCreateReceipt,
  automationLifecycleReceipt, automationEventBadge, automationActionLabel, automationCounter,
  automationErrorHtml, automationRequestCurrent, automationVisibleList, automationSummaryHtml, automationJsonText,
  automationRowHtml, automationEventHtml, automationDetailHtml, automationsLoad,
  automationLoadEvidence, automationCreateSubmit, automationToggle, automationReplaySubmit, automationInspectRun, connectionResetWorkspace,
};`, sandbox, { filename: "index.html<script>" });
const A = sandbox.__automation;

let passed = 0, failed = 0;
function check(name, condition, detail = "") {
  if (condition) { passed++; console.log(`ok   ${name}`); }
  else { failed++; console.log(`FAIL ${name}${detail ? ` — ${detail}` : ""}`); }
}

const baseTrigger = {
  trigger_id: "hook-support",
  name: "Support escalation",
  target: { kind: "assistant", id: "assistant-support" },
  action: "start_run",
  input_template: { messages: [{ role: "user", content: "{{event.issue}}" }] },
  enabled: true,
  secret: "0123456789abcdef0123456789abcdef",
  created_at: "2026-08-10T12:30:45.123456Z",
  events_received: 4,
  runs_fired: 3,
};
const trigger = (extra = {}) => ({ ...baseTrigger, ...extra });
const baseEvent = {
  event_id: "event-001",
  trigger_id: "hook-support",
  payload_hash: "a".repeat(64),
  payload: { issue: "INC-42" },
  action: "start_run",
  status: "executed",
  run_id: "run-001",
  created_at: "2026-08-10T12:31:00Z",
};
const event = (extra = {}) => ({ ...baseEvent, ...extra });

/* Exact trigger wire contract. */
check("trigger contract: assistant start-run binding is accepted", A.automationTriggerContract(baseTrigger)?.target.kind === "assistant");
check("trigger contract: thread message binding is accepted", Boolean(A.automationTriggerContract(trigger({
  target: { kind: "thread", id: "thread-42" }, action: "send_message", debounce_ms: 250,
}))));
check("trigger contract: thread resume binding is accepted", Boolean(A.automationTriggerContract(trigger({
  target: { kind: "thread", id: "thread-42" }, action: "resume_thread",
}))));
check("trigger contract: start-run cannot silently bind a thread", !A.automationTriggerContract(trigger({ target: { kind: "thread", id: "thread-42" } })));
check("trigger contract: thread input cannot silently bind an assistant", !A.automationTriggerContract(trigger({ action: "send_message" })));
check("trigger contract: short signing secret fails closed", !A.automationTriggerContract(trigger({ secret: "too-short" })));
check("trigger contract: signing-secret minimum follows Rust UTF-8 bytes", Boolean(A.automationTriggerContract(trigger({ secret: "🔐🔐🔐🔐" }))) && !A.automationTriggerContract(trigger({ secret: "🔐🔐🔐" })));
check("trigger contract: server-legal long signing secrets remain inspectable", Boolean(A.automationTriggerContract(trigger({ secret: "s".repeat(5000) }))));
check("trigger contract: malformed lifecycle timestamp fails closed", !A.automationTriggerContract(trigger({ created_at: "08/10/2026" })));
check("trigger contract: unsupported future action fails closed", !A.automationTriggerContract(trigger({ action: "shell" })));
check("trigger contract: debounce zero and over-limit fail closed",
  !A.automationTriggerContract(trigger({ debounce_ms: 0 })) && !A.automationTriggerContract(trigger({ debounce_ms: 300001 })));
check("trigger contract: absent debounce is distinct and legal", Boolean(A.automationTriggerContract(baseTrigger)) && !("debounce_ms" in baseTrigger));
check("trigger contract: reserved and path identities fail before DOM or request use",
  !A.automationSafeId("triggers") && !A.automationSafeId("../hook") && !A.automationSafeId("a/b") && A.automationSafeId("hook.42") === "hook.42");

{
  const exact = A.agentParseJsonWithNumberKinds(JSON.stringify(baseTrigger).replace('"events_received":4', '"events_received":18446744073709551615'));
  const checked = A.automationTriggerContract(exact);
  check("trigger contract: legal u64 counters survive JavaScript precision limits exactly",
    checked?.events === 18446744073709551615n && A.automationCounter(exact, "events_received") === "18446744073709551615");
}

check("catalog contract: duplicate IDs invalidate the complete snapshot", !A.automationListContract([baseTrigger, baseTrigger]));
check("catalog contract: malformed member invalidates the complete snapshot", !A.automationListContract([baseTrigger, { trigger_id: "broken" }]));
check("catalog contract: bounded valid list is retained", A.automationListContract([baseTrigger]).length === 1);
{
  const largeCatalog = A.automationListContract(Array.from({ length: 501 }, (_, index) => trigger({ trigger_id: `hook-${index}` })));
  const window = A.automationRenderWindow(largeCatalog, "hook-500");
  check("catalog contract: server-legal catalogs stay available under a hard DOM window",
    largeCatalog.length === 501 && window.length === 500 && window.at(-1).trigger_id === "hook-500");
}

/* Exact event/dead-letter evidence. */
check("event contract: executed event binds trigger, action, hash, run, and RFC3339 time", A.automationEventContract(baseEvent, baseTrigger)?.run_id === "run-001");
check("event contract: failed dead letter with error and no run is accepted", Boolean(A.automationEventContract(event({ status: "failed", run_id: null, error: "target unavailable" }), baseTrigger)));
check("event contract: server-legal long failure evidence remains inspectable", Boolean(A.automationEventContract(event({ status: "failed", run_id: null, error: "e".repeat(40000) }), baseTrigger)));
check("event contract: pending and coalesced are distinct evidence", Boolean(A.automationEventContract(event({ status: "pending", run_id: null }), baseTrigger)) && Boolean(A.automationEventContract(event({ status: "coalesced" }), baseTrigger)));
check("event contract: cross-trigger evidence fails closed", !A.automationEventContract(event({ trigger_id: "hook-other" }), baseTrigger));
check("event contract: action drift fails closed", !A.automationEventContract(event({ action: "send_message" }), baseTrigger));
check("event contract: malformed hash/status/timestamp fail closed",
  !A.automationEventContract(event({ payload_hash: "a" }), baseTrigger) &&
  !A.automationEventContract(event({ status: "unknown" }), baseTrigger) &&
  !A.automationEventContract(event({ created_at: "2026-08-10" }), baseTrigger));
check("event contract: impossible status, run, error, and replay-lineage combinations fail closed",
  !A.automationEventContract(event({ status: "executed", run_id: null }), baseTrigger) &&
  !A.automationEventContract(event({ status: "pending", run_id: null, error: "not pending" }), baseTrigger) &&
  !A.automationEventContract(event({ status: "coalesced", replayed_from: "event-old" }), baseTrigger) &&
  !A.automationEventContract(event({ status: "failed", run_id: null, error: null }), baseTrigger) &&
  !A.automationEventContract(event({ status: "failed", run_id: "run-001", error: "failed" }), baseTrigger));
check("event envelope: duplicate event IDs invalidate the complete read", !A.automationEventsContract([baseEvent, baseEvent], baseTrigger));
check("event envelope: server retention cap is enforced", !A.automationEventsContract(Array.from({ length: 257 }, (_, index) => event({ event_id: `event-${index}` })), baseTrigger));
{
  const failed = event({ event_id: "event-failed", status: "failed", run_id: null, error: "target unavailable" });
  check("event envelope: exact full-log/dead-letter agreement is distinguishable from crossed snapshots",
    A.automationDeadLettersAgree([baseEvent, failed], [failed]) &&
    !A.automationDeadLettersAgree([baseEvent, failed], [{ ...failed, payload: { issue: "different" } }]) &&
    !A.automationDeadLettersAgree([baseEvent], [failed]) && !A.automationDeadLettersAgree([baseEvent, failed], []));
}
check("run handoff: every journal event must bind one exact requested run and thread",
  A.automationRunJournalThread({ run_id: "run-001", complete: true, events: [
    { run_id: "run-001", thread_id: "thread-001" }, { run_id: "run-001", thread_id: "thread-001" },
  ] }, "run-001") === "thread-001" &&
  !A.automationRunJournalThread({ run_id: "run-001", complete: true, events: [{ run_id: "other", thread_id: "thread-001" }] }, "run-001") &&
  !A.automationRunJournalThread({ run_id: "run-001", complete: true, events: [{ run_id: "run-001", thread_id: "thread-001" }, { run_id: "run-001", thread_id: "thread-002" }] }, "run-001"));
{
  const ack = { event_id: "event-replay", status: "executed", run_id: "run-replay", replayed_from: "event-001" };
  const stored = event({ event_id: "event-replay", status: "executed", run_id: "run-replay", replayed_from: "event-001" });
  check("replay acknowledgement: compact 202 wire binds a distinct executed event and run", A.automationReplayAck(ack, "event-001")?.event_id === "event-replay");
  check("replay acknowledgement: malformed, failed, or cross-event wires fail closed",
    !A.automationReplayAck({ ...ack, status: "failed" }, "event-001") && !A.automationReplayAck({ ...ack, replayed_from: "other" }, "event-001") && !A.automationReplayAck({ ...ack, payload: {} }, "event-001"));
  check("replay evidence: compact acknowledgement must match the refreshed full durable record",
    A.automationReplayCorroborated([stored], ack, baseTrigger, baseEvent)?.event_id === "event-replay" &&
    !A.automationReplayCorroborated([{ ...stored, run_id: "run-other" }], ack, baseTrigger, baseEvent) &&
    !A.automationReplayCorroborated([{ ...stored, payload: { issue: "different" } }], ack, baseTrigger, baseEvent));
}

/* Create preflight and exact receipt binding. */
const validDraft = {
  name: "Support escalation", triggerId: "hook-support", action: "start_run", targetId: "assistant-support",
  debounceMs: "250", secret: "0123456789abcdef0123456789abcdef",
  rawTemplate: '{"sequence":18446744073709551615,"issue":"{{event.issue}}"}', acknowledged: true,
};
const parsed = A.automationCreateDraft(validDraft);
check("create preflight: valid reviewed draft produces an assistant target", parsed.value?.target.kind === "assistant" && parsed.value.debounceMs === 250);
check("create preflight: unsafe u64 template token stays exact in request body",
  A.automationCreateBody(parsed.value, "hook-support").includes('"sequence":18446744073709551615'));
check("create preflight: thread actions derive a thread target", A.automationCreateDraft({ ...validDraft, action: "send_message", targetId: "thread-42" }).value?.target.kind === "thread");
check("create preflight: acknowledgement is required", A.automationCreateDraft({ ...validDraft, acknowledged: false }).errors.acknowledged);
check("create preflight: invalid JSON is never downgraded to a string", A.automationCreateDraft({ ...validDraft, rawTemplate: "{" }).errors.rawTemplate);
check("create preflight: byte bounds apply to template and name", A.automationCreateDraft({ ...validDraft, rawTemplate: JSON.stringify("é".repeat(20000)) }).errors.rawTemplate && A.automationCreateDraft({ ...validDraft, name: "é".repeat(65) }).errors.name);
check("create preflight: secret and debounce honor server minima plus the Studio safety ceiling",
  A.automationCreateDraft({ ...validDraft, secret: "short" }).errors.secret &&
  A.automationCreateDraft({ ...validDraft, secret: "🔐🔐🔐🔐" }).value?.secret === "🔐🔐🔐🔐" &&
  A.automationCreateDraft({ ...validDraft, debounceMs: "0" }).errors.debounceMs &&
  A.automationCreateDraft({ ...validDraft, debounceMs: "300001" }).errors.debounceMs);
check("create request: optional fields are omitted rather than sent as coercible nulls", !A.automationCreateBody(A.automationCreateDraft({ ...validDraft, secret: "", debounceMs: "" }).value, "hook-support").includes('"secret"') && !A.automationCreateBody(A.automationCreateDraft({ ...validDraft, secret: "", debounceMs: "" }).value, "hook-support").includes('"debounce_ms"'));

{
  const response = A.agentParseJsonWithNumberKinds(`{"trigger_id":"hook-support","name":"Support escalation","target":{"kind":"assistant","id":"assistant-support"},"action":"start_run","input_template":{"sequence":18446744073709551615,"issue":"{{event.issue}}"},"enabled":true,"secret":"0123456789abcdef0123456789abcdef","debounce_ms":250,"created_at":"2026-08-10T12:30:45Z","events_received":0,"runs_fired":0}`);
  check("create receipt: exact unsafe-number template and reviewed fields are accepted", A.automationCreateReceipt(response, parsed.value, "hook-support")?.trigger_id === "hook-support");
  check("create receipt: active target or template drift fails closed",
    !A.automationCreateReceipt({ ...response, target: { kind: "assistant", id: "other" } }, parsed.value, "hook-support") &&
    !A.automationCreateReceipt({ ...response, input_template: { sequence: 7, issue: "{{event.issue}}" } }, parsed.value, "hook-support"));
  check("create receipt: nonzero evidence cannot masquerade as a newly created binding",
    !A.automationCreateReceipt({ ...response, events_received: 1 }, parsed.value, "hook-support"));
}

check("lifecycle receipt: exact pause preserves immutable binding surfaces", A.automationLifecycleReceipt({ ...baseTrigger, enabled: false }, baseTrigger, false)?.enabled === false);
check("lifecycle receipt: target/template drift fails closed", !A.automationLifecycleReceipt({ ...baseTrigger, enabled: false, target: { kind: "assistant", id: "other" } }, baseTrigger, false));

/* Rendering, privacy, accessibility, and responsive invariants. */
check("row rendering: identity, target, counters, and lifecycle share one native option", A.automationRowHtml(baseTrigger, true).includes('role="option"') && A.automationRowHtml(baseTrigger, true).includes('aria-selected="true"') && A.automationRowHtml(baseTrigger, true).includes("assistant-support"));
check("row rendering: hostile names and IDs are escaped", !A.automationRowHtml(trigger({ name: '<img src=x onerror="alert(1)">', trigger_id: 'hook&quot;' }), false).includes("<img"));
check("event rendering: payload and errors are escaped and visibly bounded", !A.automationEventHtml(event({ payload: { issue: "<script>alert(1)</script>" }, error: "<img>" }), baseTrigger, null).includes("<script>"));
check("event rendering: long server errors disclose their byte-bounded preview",
  A.automationEventHtml(event({ status: "failed", run_id: null, error: "é".repeat(1000) }), baseTrigger, null).includes("error preview truncated"));
{
  const unsafe = A.agentParseJsonWithNumberKinds('{"sequence":18446744073709551615}');
  check("JSON evidence rendering: legal unsafe Rust integers remain exact rather than rounded", A.automationJsonText(unsafe).includes("18446744073709551615") && !A.automationJsonText(unsafe).includes("18446744073709552000"));
  check("JSON evidence rendering: multibyte previews stay within a byte-aware visible boundary",
    A.automationJsonText("é".repeat(10000), 200).includes("exact JSON view truncated") && A.automationJsonText("é".repeat(10000), 200).length < 200);
}
check("event rendering: run handoff and deliberate replay are explicit actions", A.automationEventHtml(baseEvent, baseTrigger, null).includes('data-automation-run="run-001"') && A.automationEventHtml(baseEvent, baseTrigger, null).includes("Review replay"));
check("replay review: exact event/hash/target/action acknowledgement is visible", A.automationEventHtml(baseEvent, baseTrigger, { eventId: "event-001", acknowledged: false, submitting: false, ambiguous: false, error: "" }).includes("payload hash") && A.automationEventHtml(baseEvent, baseTrigger, { eventId: "event-001", acknowledged: false, submitting: false, ambiguous: false, error: "" }).includes("assistant-support"));
check("detail privacy: signing secret is concealed by default", A.automationDetailHtml({ events: [], deadLetter: [], filter: "all", revealed: new Set(), observedAt: "" }, baseTrigger).includes("••••") && !A.automationDetailHtml({ events: [], deadLetter: [], filter: "all", revealed: new Set(), observedAt: "" }, baseTrigger).includes(baseTrigger.secret));
check("detail privacy: explicit reveal exposes the exact page-memory secret", A.automationDetailHtml({ events: [], deadLetter: [], filter: "all", revealed: new Set([baseTrigger.trigger_id]), observedAt: "" }, baseTrigger).includes(baseTrigger.secret));
check("route compatibility: raw 404 is a capability gap while structured errors stay errors", A.automationErrorHtml({ status: 404, body: { raw: "not found" }, message: "404" }).includes("no trigger registry") && !A.automationErrorHtml({ status: 404, body: { error: "not_found" }, message: "missing" }).includes("no trigger registry"));
check("filters: lifecycle and search compose without inferring failures from debounce counters", A.automationVisibleList({ list: [baseTrigger, trigger({ trigger_id: "paused", name: "Paused", enabled: false, events_received: 0, runs_fired: 0 })], query: "support", filter: "enabled" }).length === 1 && A.automationVisibleList({ list: [baseTrigger], query: "", filter: "paused" }).length === 0);
check("summary: server event/action counters and selected dead letters remain distinct", A.automationSummaryHtml({ list: [baseTrigger], selected: "hook-support", deadLetter: [event({ status: "failed" })] }).includes("4 / 3</b><span>events / run actions") && A.automationSummaryHtml({ list: [baseTrigger], selected: "hook-support", deadLetter: [event({ status: "failed" })] }).includes("1</b><span>selected dead letters"));
check("markup: sidebar, workspace, native form, listbox, live announcer, and secret warning are present",
  page.includes('id="btn-automations-open"') && page.includes('id="automations-view"') && page.includes('id="automation-form"') &&
  page.includes('role="listbox" aria-label="Signed webhook automations"') && page.includes('id="automation-announcer"') &&
  page.includes("It stays in page memory and is never saved by Studio"));
check("responsive: signal path, layout, composer, detail, and event evidence collapse deliberately",
  page.includes(".automation-signal-path { grid-template-columns:1fr; }") && page.includes(".automation-layout { grid-template-columns:1fr; }") &&
  page.includes(".automation-toolbar,.automation-form { grid-template-columns:1fr; }") && page.includes(".automation-detail-head { flex-direction:column; }") &&
  page.includes(".automation-event { grid-template-columns:auto minmax(0,1fr); }"));
check("navigation: renderMain owns a distinct automations workspace and init wires every primary action",
  page.includes('const automations = store.view === "automations"') && page.includes('$("automations-view").style.display = automations ? "block" : "none"') &&
  page.includes('$("btn-automations-open").onclick = openAutomations') && page.includes('automationReplaySubmit(form.getAttribute("data-automation-replay-form"))'));
check("connection reset: trigger reads and mutations are invalidated and page-memory secrets are dropped",
  page.includes("store.automationRequest += 1") && page.includes("store.automationMutationRequest += 1") && page.includes("store.automations = null"));

/* Real deferred ownership checks with rendering stubbed. */
const nodes = new Map([
  ["automations-side-count", { textContent: "" }], ["automation-announcer", { textContent: "" }],
]);
sandbox.document = { getElementById: (id) => nodes.get(id) || null };
vm.runInContext(`
automationsRender = () => {};
automationUpdateSidebar = () => {};
toast = () => {};
`, sandbox);

{
  const field = (value = "") => ({ value, checked: false, disabled: false, hidden: false, textContent: "",
    getAttribute: () => null, setAttribute: () => {}, removeAttribute: () => {}, focus: () => {} });
  const formNodes = {
    "inp-automation-name": field("Deferred create"), "inp-automation-id": field("hook-deferred"),
    "sel-automation-action": field("start_run"), "inp-automation-target": field("assistant-support"),
    "inp-automation-debounce": field(""), "inp-automation-secret": field("0123456789abcdef0123456789abcdef"),
    "inp-automation-template": field('{"event":"{{event}}"}'), "chk-automation-review": field(),
    "btn-automation-submit": field(), "automation-form-error": field(),
  };
  formNodes["chk-automation-review"].checked = true;
  for (const [id, node] of Object.entries(formNodes)) nodes.set(id, node);
  let resolveCreate;
  sandbox.__createApi = () => new Promise((resolve) => { resolveCreate = resolve; });
  vm.runInContext("apiForConnection = globalThis.__createApi", sandbox);
  A.store.connectionEpoch = 1;
  A.store.conn = { baseUrl: "http://tenant-a", apiKey: "a" };
  A.store.view = "automations";
  const newer = trigger({ trigger_id: "hook-newer", name: "Newer selection" });
  const newerEvent = event({ event_id: "event-newer" });
  A.store.automations = { list: [baseTrigger, newer], selected: baseTrigger.trigger_id, events: [baseEvent],
    deadLetter: [], query: "", filter: "all", eventMode: "events", revealed: new Set(), replay: null };
  const creating = A.automationCreateSubmit();
  A.store.automations.selected = newer.trigger_id;
  A.store.automations.events = [newerEvent];
  resolveCreate({ trigger_id: "hook-deferred", name: "Deferred create", target: { kind: "assistant", id: "assistant-support" },
    action: "start_run", input_template: { event: "{{event}}" }, enabled: true,
    secret: "0123456789abcdef0123456789abcdef", created_at: "2026-08-10T13:00:00Z", events_received: 0, runs_fired: 0 });
  await creating;
  check("async create: a late exact receipt updates catalog truth without taking a newer selection or its evidence",
    A.store.automations.list.some((item) => item.trigger_id === "hook-deferred") &&
    A.store.automations.selected === newer.trigger_id && A.store.automations.events[0].event_id === "event-newer" &&
    formNodes["inp-automation-name"].value === "Deferred create");
}

{
  nodes.get("inp-automation-name").value = "Reconciled create";
  nodes.get("inp-automation-id").value = "hook-reconciled";
  let createReads = 0;
  sandbox.__createReconcileApi = async (connection, method) => {
    if (method === "POST") return { trigger_id: "hook-reconciled", created: true };
    createReads++;
    return { trigger_id: "hook-reconciled", name: "Reconciled create", target: { kind: "assistant", id: "assistant-support" },
      action: "start_run", input_template: { event: "{{event}}" }, enabled: true,
      secret: "0123456789abcdef0123456789abcdef", created_at: "2026-08-10T13:01:00Z", events_received: 0, runs_fired: 0 };
  };
  vm.runInContext("apiForConnection = globalThis.__createReconcileApi", sandbox);
  A.store.connectionEpoch = 2;
  A.store.conn = { baseUrl: "http://tenant-a", apiKey: "a" };
  A.store.view = "automations";
  A.store.automations = { list: [baseTrigger], selected: baseTrigger.trigger_id, events: [baseEvent],
    deadLetter: [], query: "", filter: "all", eventMode: "events", revealed: new Set(), replay: null };
  const creating = A.automationCreateSubmit();
  A.store.view = "home";
  await creating;
  check("create receipt: a malformed success response is reconciled through the exact stable ID before catalog truth changes",
    createReads === 1 && A.store.automations.list.some((item) => item.trigger_id === "hook-reconciled") &&
    A.store.automations.events[0].event_id === "event-001");
}

{
  const pending = new Map();
  sandbox.__automationApi = (connection, method, route) => new Promise((resolve) => pending.set(connection.baseUrl + route, resolve));
  vm.runInContext("apiForConnection = globalThis.__automationApi", sandbox);
  A.store.connectionEpoch = 1;
  A.store.conn = { baseUrl: "http://tenant-a", apiKey: "a" };
  A.store.automations = null;
  const oldLoad = A.automationsLoad(true);
  A.store.connectionEpoch = 2;
  A.store.conn = { baseUrl: "http://tenant-b", apiKey: "b" };
  A.store.automations = null;
  const newLoad = A.automationsLoad(true);
  pending.get("http://tenant-b/triggers")([trigger({ trigger_id: "hook-b", name: "B" })]);
  await newLoad;
  pending.get("http://tenant-a/triggers")([trigger({ trigger_id: "hook-a", name: "A" })]);
  await oldLoad;
  check("async isolation: late trigger catalog from tenant A cannot overwrite tenant B",
    A.store.conn.baseUrl === "http://tenant-b" && A.store.automations.list.length === 1 && A.store.automations.list[0].trigger_id === "hook-b");
}

{
  const pending = new Map();
  sandbox.__evidenceApi = (connection, method, route) => new Promise((resolve) => pending.set(route, resolve));
  vm.runInContext("apiForConnection = globalThis.__evidenceApi", sandbox);
  A.store.connectionEpoch = 3;
  A.store.conn = { baseUrl: "http://tenant-b", apiKey: "b" };
  const first = trigger({ trigger_id: "hook-first", name: "First" });
  const second = trigger({ trigger_id: "hook-second", name: "Second" });
  A.store.automations = { list: [first, second], selected: "", events: [], deadLetter: [], revealed: new Set(), filter: "all", query: "" };
  const oldEvidence = A.automationLoadEvidence("hook-first", true);
  const freshEvidence = A.automationLoadEvidence("hook-second", true);
  pending.get("/triggers/hook-second")({ ...second, events_received: 1, runs_fired: 1 });
  pending.get("/triggers/hook-second/events")([event({ trigger_id: "hook-second", event_id: "event-second" })]);
  pending.get("/triggers/hook-second/dead-letter")([]);
  await freshEvidence;
  pending.get("/triggers/hook-first")({ ...first, events_received: 9, runs_fired: 9 });
  pending.get("/triggers/hook-first/events")([event({ trigger_id: "hook-first", event_id: "event-first" })]);
  pending.get("/triggers/hook-first/dead-letter")([]);
  await oldEvidence;
  check("async selection: late evidence for the prior trigger cannot replace the selected trigger",
    A.store.automations.selected === "hook-second" && A.store.automations.events[0].event_id === "event-second" &&
    A.store.automations.list.find((item) => item.trigger_id === "hook-second").events_received === 1);
}

{
  const pending = new Map();
  sandbox.__crossedEvidenceApi = (connection, method, route) => new Promise((resolve) => pending.set(route, resolve));
  vm.runInContext("apiForConnection = globalThis.__crossedEvidenceApi", sandbox);
  A.store.connectionEpoch = 4;
  A.store.conn = { baseUrl: "http://tenant-b", apiKey: "b" };
  A.store.view = "automations";
  A.store.automations = { list: [baseTrigger], selected: baseTrigger.trigger_id, events: [], deadLetter: [],
    query: "", filter: "all", eventMode: "events", revealed: new Set(), replay: null };
  const loading = A.automationLoadEvidence(baseTrigger.trigger_id, true);
  pending.get("/triggers/hook-support")({ ...baseTrigger, events_received: 5, runs_fired: 3 });
  pending.get("/triggers/hook-support/events")([baseEvent]);
  await Promise.resolve();
  const justFailed = event({ event_id: "event-just-failed", status: "failed", run_id: null, error: "target unavailable" });
  pending.get("/triggers/hook-support/dead-letter")([justFailed]);
  const loaded = await loading;
  check("async evidence: a failure landing between independent endpoint reads is retained as explicit snapshot drift",
    loaded === true && A.store.automations.events[0].event_id === "event-001" &&
    A.store.automations.deadLetter[0].event_id === "event-just-failed" && A.store.automations.evidenceDrift === true);
}

{
  const pending = new Map();
  sandbox.__reverseCrossedEvidenceApi = (connection, method, route) => new Promise((resolve) => pending.set(route, resolve));
  vm.runInContext("apiForConnection = globalThis.__reverseCrossedEvidenceApi", sandbox);
  A.store.connectionEpoch = 5;
  A.store.conn = { baseUrl: "http://tenant-b", apiKey: "b" };
  A.store.view = "automations";
  A.store.automations = { list: [baseTrigger], selected: baseTrigger.trigger_id, events: [], deadLetter: [],
    query: "", filter: "all", eventMode: "events", evidenceDrift: true, revealed: new Set(), replay: null };
  const loading = A.automationLoadEvidence(baseTrigger.trigger_id, true);
  check("async evidence: a prior trigger's drift warning clears while a new evidence read is pending", A.store.automations.evidenceDrift === false);
  pending.get("/triggers/hook-support")({ ...baseTrigger, events_received: 5, runs_fired: 3 });
  pending.get("/triggers/hook-support/dead-letter")([]);
  await Promise.resolve();
  const justFailed = event({ event_id: "event-reverse-failed", status: "failed", run_id: null, error: "target unavailable" });
  pending.get("/triggers/hook-support/events")([justFailed, baseEvent]);
  const loaded = await loading;
  check("async evidence: reverse endpoint skew is also retained and disclosed as snapshot drift",
    loaded === true && A.store.automations.events[0].event_id === "event-reverse-failed" &&
    A.store.automations.deadLetter.length === 0 && A.store.automations.evidenceDrift === true);
}

{
  let lifecycleReads = 0;
  sandbox.__toggleReconcileApi = async (connection, method) => {
    if (method === "PATCH") return { trigger_id: "hook-support", enabled: false };
    lifecycleReads++;
    return { ...baseTrigger, enabled: false };
  };
  vm.runInContext("apiForConnection = globalThis.__toggleReconcileApi", sandbox);
  A.store.connectionEpoch = 6;
  A.store.conn = { baseUrl: "http://tenant-b", apiKey: "b" };
  A.store.view = "automations";
  A.store.automations = { list: [baseTrigger], selected: baseTrigger.trigger_id, events: [baseEvent], deadLetter: [], revealed: new Set() };
  await A.automationToggle(baseTrigger.trigger_id);
  check("lifecycle receipt: a malformed PATCH response is reconciled from authoritative trigger state before success",
    lifecycleReads === 1 && A.store.automations.list[0].enabled === false);
}

{
  let resolvePatch;
  sandbox.__toggleApi = () => new Promise((resolve) => { resolvePatch = resolve; });
  vm.runInContext("apiForConnection = globalThis.__toggleApi", sandbox);
  A.store.connectionEpoch = 4;
  A.store.conn = { baseUrl: "http://tenant-a", apiKey: "a" };
  A.store.automations = { list: [baseTrigger], selected: baseTrigger.trigger_id, events: [], deadLetter: [], revealed: new Set() };
  const toggling = A.automationToggle(baseTrigger.trigger_id);
  A.store.connectionEpoch = 5;
  A.store.conn = { baseUrl: "http://tenant-b", apiKey: "b" };
  A.store.automations = { list: [baseTrigger], selected: baseTrigger.trigger_id, events: [], deadLetter: [], revealed: new Set() };
  resolvePatch({ ...baseTrigger, enabled: false });
  await toggling;
  check("async lifecycle: late pause receipt cannot cross a connection or tenant boundary", A.store.automations.list[0].enabled === true);
}

{
  sandbox.__replayMalformedApi = async () => ({ event_id: "event-replay", status: "executed", replayed_from: "event-001" });
  vm.runInContext("apiForConnection = globalThis.__replayMalformedApi", sandbox);
  A.store.connectionEpoch = 6;
  A.store.conn = { baseUrl: "http://tenant-b", apiKey: "b" };
  A.store.view = "automations";
  const review = { eventId: "event-001", acknowledged: true, submitting: false, ambiguous: false, error: "" };
  A.store.automations = { list: [baseTrigger], selected: baseTrigger.trigger_id, events: [baseEvent], deadLetter: [], eventMode: "events", revealed: new Set(), replay: review };
  await A.automationReplaySubmit("event-001");
  check("replay safety: an untrusted 202 acknowledgement locks the non-idempotent action",
    review.ambiguous === true && review.submitting === false && review.error.includes("may have executed"));
}

{
  sandbox.__replayTimeoutApi = async () => { throw new A.ApiError(408, { error: "request_timeout", message: "response timed out" }); };
  vm.runInContext("apiForConnection = globalThis.__replayTimeoutApi", sandbox);
  A.store.connectionEpoch = 7;
  A.store.conn = { baseUrl: "http://tenant-b", apiKey: "b" };
  A.store.view = "automations";
  const review = { eventId: "event-001", acknowledged: true, submitting: false, ambiguous: false, error: "" };
  A.store.automations = { list: [baseTrigger], selected: baseTrigger.trigger_id, events: [baseEvent], deadLetter: [], eventMode: "events", revealed: new Set(), replay: review };
  await A.automationReplaySubmit("event-001");
  check("replay safety: HTTP 408 remains locked until durable lineage is refreshed",
    review.ambiguous === true && review.submitting === false && review.error.includes("outcome is uncertain"));
}

{
  const replayed = event({ event_id: "event-replay", run_id: "run-replay", replayed_from: "event-001" });
  sandbox.__replaySuccessApi = async (connection, method, route) => {
    if (method === "POST") return { event_id: "event-replay", status: "executed", run_id: "run-replay", replayed_from: "event-001" };
    if (route === "/triggers/hook-support") return { ...baseTrigger, events_received: 5, runs_fired: 4 };
    if (route.endsWith("/events")) return [replayed, baseEvent];
    if (route.endsWith("/dead-letter")) return [];
    throw new Error(`unexpected route ${route}`);
  };
  vm.runInContext("apiForConnection = globalThis.__replaySuccessApi", sandbox);
  A.store.connectionEpoch = 8;
  A.store.conn = { baseUrl: "http://tenant-b", apiKey: "b" };
  A.store.view = "automations";
  const review = { eventId: "event-001", acknowledged: true, submitting: false, ambiguous: false, error: "" };
  A.store.automations = { list: [baseTrigger], selected: baseTrigger.trigger_id, events: [baseEvent], deadLetter: [], eventMode: "events", revealed: new Set(), replay: review };
  await A.automationReplaySubmit("event-001");
  check("replay success: compact acknowledgement is corroborated from the refreshed full record before success",
    A.store.automations.replay === null && A.store.automations.events[0].event_id === "event-replay" && A.store.automations.list[0].runs_fired === 4);
}

{
  const failedReplay = event({ event_id: "event-replay-failed", status: "failed", run_id: null,
    error: "target unavailable", replayed_from: "event-001" });
  sandbox.__replayFailedApi = async (connection, method, route) => {
    if (method === "POST") throw new A.ApiError(502, { error: "action_failed", message: "trigger action failed" });
    if (route === "/triggers/hook-support") return { ...baseTrigger, events_received: 4, runs_fired: 3 };
    if (route.endsWith("/events")) return [failedReplay, baseEvent];
    if (route.endsWith("/dead-letter")) return [failedReplay];
    throw new Error(`unexpected route ${route}`);
  };
  vm.runInContext("apiForConnection = globalThis.__replayFailedApi", sandbox);
  A.store.connectionEpoch = 9;
  A.store.conn = { baseUrl: "http://tenant-b", apiKey: "b" };
  A.store.view = "automations";
  const review = { eventId: "event-001", acknowledged: true, submitting: false, ambiguous: false, error: "" };
  A.store.automations = { list: [baseTrigger], selected: baseTrigger.trigger_id, events: [baseEvent], deadLetter: [], eventMode: "events", revealed: new Set(), replay: review };
  await A.automationReplaySubmit("event-001");
  check("replay failure: exact action_failed response is corroborated as a new dead letter rather than mislabeled uncertain",
    A.store.automations.replay === null && A.store.automations.deadLetter[0].event_id === "event-replay-failed");
}

{
  sandbox.__replayApi = async () => { throw new A.ApiError(0, { error: "network", message: "connection closed" }); };
  vm.runInContext("apiForConnection = globalThis.__replayApi", sandbox);
  A.store.connectionEpoch = 10;
  A.store.conn = { baseUrl: "http://tenant-b", apiKey: "b" };
  const review = { eventId: "event-001", acknowledged: true, submitting: false, ambiguous: false, error: "" };
  A.store.automations = { list: [baseTrigger], selected: baseTrigger.trigger_id, events: [baseEvent], deadLetter: [], revealed: new Set(), replay: review };
  await A.automationReplaySubmit("event-001");
  check("replay safety: transport uncertainty locks repetition and directs evidence refresh",
    review.ambiguous === true && review.submitting === false && review.error.includes("look for a new record whose replayed_from matches"));
}

{
  let resolveJournal;
  sandbox.__handoffDeferredApi = () => new Promise((resolve) => { resolveJournal = resolve; });
  vm.runInContext("apiForConnection = globalThis.__handoffDeferredApi", sandbox);
  A.store.connectionEpoch = 11;
  A.store.conn = { baseUrl: "http://tenant-b", apiKey: "b" };
  A.store.view = "automations";
  A.store.threads = [];
  const second = trigger({ trigger_id: "hook-second", name: "Second" });
  const runEvent = event({ event_id: "event-handoff", run_id: "run-handoff" });
  A.store.automations = { list: [baseTrigger, second], selected: baseTrigger.trigger_id, events: [runEvent],
    deadLetter: [], eventMode: "events", revealed: new Set(), replay: null };
  const handoff = A.automationInspectRun("run-handoff", "event-handoff");
  A.store.automations.selected = second.trigger_id;
  A.store.automationRequest += 1;
  resolveJournal({ run_id: "run-handoff", complete: true, events: [{ run_id: "run-handoff", thread_id: "thread-handoff" }] });
  await handoff;
  check("run handoff ownership: a deferred journal cannot navigate after automation selection changes",
    A.store.view === "automations" && A.store.automations.selected === second.trigger_id && A.store.threads.length === 0);
}

{
  let resolveRecorder, recorderStarted;
  const started = new Promise((resolve) => { recorderStarted = resolve; });
  let recorderFocuses = 0;
  nodes.set("flight-recorder-title", { focus: () => { recorderFocuses++; } });
  nodes.set("flight-recorder-card", { scrollIntoView: () => { recorderFocuses++; } });
  sandbox.__handoffApi = async () => ({ run_id: "run-handoff", complete: true,
    events: [{ run_id: "run-handoff", thread_id: "thread-handoff" }] });
  sandbox.__deferredRecorder = () => { recorderStarted(); return new Promise((resolve) => { resolveRecorder = resolve; }); };
  vm.runInContext(`
apiForConnection = globalThis.__handoffApi;
recLoad = globalThis.__deferredRecorder;
saveThreads = () => {};
renderThreads = () => {};
renderMain = () => {};
`, sandbox);
  A.store.connectionEpoch = 12;
  A.store.conn = { baseUrl: "http://tenant-b", apiKey: "b" };
  A.store.view = "automations";
  A.store.threads = [];
  const runEvent = event({ event_id: "event-handoff", run_id: "run-handoff" });
  A.store.automations = { list: [baseTrigger], selected: baseTrigger.trigger_id, events: [runEvent],
    deadLetter: [], eventMode: "events", revealed: new Set(), replay: null };
  const handoff = A.automationInspectRun("run-handoff", "event-handoff");
  await started;
  A.store.view = "home";
  A.store.selected = "";
  A.store.recorder = { requestedRunId: "run-handoff" };
  resolveRecorder();
  await handoff;
  check("run handoff ownership: leaving the destination while Recorder loads suppresses stale focus and scroll",
    A.store.view === "home" && recorderFocuses === 0);
}

console.log(`\n${passed} passed, ${failed} failed`);
if (failed) process.exit(1);
