#!/usr/bin/env node
/* Studio 1.0 product shell: three primary destinations, exception-led
 * operations, and one visual capability map over the existing agent draft.
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
const sandbox = { URL, URLSearchParams, TextEncoder, TextDecoder, setTimeout: () => 1, clearTimeout() {},
  document: { getElementById: () => null, querySelector: () => null } };
vm.createContext(sandbox);
vm.runInContext(source + `
globalThis.__productShell = { store, operationsSnapshot, operationsHtml, operationsNavigate,
  agentCapabilityMapState, navigationParseSearch, navigationRouteForStore };
`, sandbox, { filename: "index.html<script>" });
const S = sandbox.__productShell;

let passed = 0, failed = 0;
function check(name, condition, detail = "") {
  if (condition) { passed++; console.log(`ok   ${name}`); }
  else { failed++; console.log(`FAIL ${name}${detail ? ` — ${detail}` : ""}`); }
}

const primary = html.match(/<section class="studio-nav-primary"[\s\S]*?<\/section>/)?.[0] || "";
check("navigation: the primary shell exposes exactly Agents, Work, and Operations",
  (primary.match(/<button/g) || []).length === 3 && primary.includes(">Agents<") && primary.includes(">Work<") && primary.includes(">Operations<"));
check("navigation: specialist workspaces remain available through deliberate disclosure",
  html.includes('<details class="studio-tool-drawer">') && ["fabric", "memory", "learn", "registry", "automations", "schedules", "tasks"]
    .every((key) => html.includes(`id="btn-${key}-open"`)));
check("navigation: recent work and direct thread creation no longer compete with primary destinations",
  html.includes('<summary id="studio-threads-title">Recent work</summary>') &&
  html.includes('<summary id="studio-behaviors-title">Direct thread</summary>'));

{
  const parsed = S.navigationParseSearch("?studio=operations");
  check("navigation: Operations is a safe managed destination with no embedded evidence identity",
    parsed.route?.view === "operations" && !parsed.error && S.navigationRouteForStore({ view: "operations" }).view === "operations");
}

{
  const snapshot = S.operationsSnapshot({ conn: { baseUrl: "http://local" }, info: { service: "rusty-server" },
    tasks: { list: [{ status: "failed" }, { status: "completed" }], loading: false, error: null },
    automations: { list: [{ trigger_id: "a" }], deadLetter: [{ event_id: "e" }], loading: false, error: null },
    schedules: { list: [{ cron_id: "c" }], loading: false, error: null } });
  const markup = S.operationsHtml(snapshot);
  check("operations: only proven task and delivery failures become attention",
    snapshot.attention === 2 && snapshot.attentionAction === "tasks" && snapshot.failedTasks === 1 && snapshot.deadLetters === 1 && markup.includes("2 items need attention"));
  check("operations: normal tools remain secondary actions instead of status cards",
    snapshot.tools.length === 5 && markup.includes('data-operations-action="tasks"') && !markup.includes('data-operations-action="learn"') && markup.includes("Intervene only when work needs you"));
}

{
  const automationOnly = S.operationsSnapshot({ conn: { baseUrl: "http://local" }, info: { service: "rusty-server" },
    tasks: { list: [], loading: false, error: null },
    automations: { list: [{ trigger_id: "a" }], deadLetter: [{ event_id: "e" }], loading: false, error: null },
    schedules: { list: [], loading: false, error: null } });
  check("operations: the exception action opens the tool that owns the loaded failure",
    automationOnly.attentionAction === "automations" && S.operationsHtml(automationOnly).includes('data-operations-action="automations"'));
  check("operations: loaded catalogs do not overclaim a server-wide all clear",
    S.operationsHtml({ ...automationOnly, attention: 0, attentionAction: "", deadLetters: 0 }).includes("No loaded failure evidence"));
}

{
  const unknown = S.operationsSnapshot({ conn: { baseUrl: "http://local" }, info: { service: "rusty-server" } });
  const disconnected = S.operationsSnapshot({});
  check("operations: unloaded state never becomes a false all-clear",
    !unknown.attention && unknown.known === 0 && S.operationsHtml(unknown).includes("Operational state is not loaded"));
  check("operations: disconnected state offers one connection action",
    !disconnected.connected && S.operationsHtml(disconnected).includes('data-operations-action="connect"'));
}

{
  const empty = S.agentCapabilityMapState({ memoryMode: "none", approval: "runtime_policy", outputMode: "runtime_default" });
  const shaped = S.agentCapabilityMapState({ name: "Researcher", description: "Investigate", model: "provider/model",
    tools: "search | read_only", memoryMode: "read_only", bindingSurfaces: [], outputMode: "json_object",
    approval: "irreversible", budgetTokens: "1000" });
  check("agent map: an untouched draft starts with purpose rather than invented readiness",
    empty.ready === 0 && empty.summary === "Start with its purpose" && Object.values(empty.states).every((value) => !value));
  check("agent map: every visible capability is derived from the existing draft fields",
    shaped.ready === 6 && Object.values(shaped.states).every(Boolean) && shaped.summary === "6 of 6 capabilities shaped");
}

check("agent map: every capability button owns a native labelled destination",
  ["purpose", "knowledge", "tools", "model", "output", "guardrails"].every((key) =>
    html.includes(`data-agent-capability="${key}" data-agent-capability-target=`)) &&
  source.includes('target?.focus({ preventScroll: true })'));
check("agent map: responsive layout becomes a two-column map with a full-width core",
  html.includes(".agent-capability-map { grid-template-columns: repeat(2,minmax(0,1fr)); }") &&
  html.includes(".agent-capability-core { grid-column: 1/-1; grid-row: 1;"));
check("agent map: compact phones receive a deliberate single-column capability flow",
  html.includes("@media (max-width: 420px)") && html.includes(".agent-capability-map { grid-template-columns: 1fr; }"));
check("copy: primary creation language names user decisions rather than runtime implementation history",
  html.includes("Describe the job clearly enough that a teammate can choose the right agent") &&
  html.includes("03 · Capabilities") && html.includes("Choose its core behavior and execution limit") &&
  html.includes("These requirements travel with the agent") && !html.includes('class="agent-intent-state"') &&
  html.includes('id="tasks-title" tabindex="-1" style="margin-top:0">\n            Task queue') && !primary.includes("R0."));

if (failed) { console.error(`\nFAIL: ${failed} failed, ${passed} passed`); process.exit(1); }
console.log(`\nPASS: ${passed} Studio 1.0 product-shell assertions`);
