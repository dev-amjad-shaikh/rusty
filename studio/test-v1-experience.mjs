#!/usr/bin/env node
/* Studio 1.0 experience acceptance contract.
 *
 * These tests assert the three-destination product shell: Agents, Work, and
 * Operations. They inspect the real rendered structure where practical and
 * fall back to static DOM/CSS evidence only for things that cannot be
 * exercised without a live server or browser event loop.
 */

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";
import vm from "node:vm";

const here = path.dirname(fileURLToPath(import.meta.url));
const html = readFileSync(path.join(here, "index.html"), "utf8");
const scriptMatch = html.match(/<script>([\s\S]*?)<\/script>/);
if (!scriptMatch) { console.error("FAIL: no script block"); process.exit(1); }
const src = scriptMatch[1].replace(/\ninit\(\);\s*$/, "\n");

const localData = new Map();
const sandbox = {
  localStorage: {
    getItem: (key) => (localData.has(key) ? localData.get(key) : null),
    setItem: (key, value) => localData.set(key, String(value)),
  },
  document: {
    getElementById: () => ({ textContent: "", setAttribute() {}, removeAttribute() {} }),
    activeElement: null,
    querySelector: () => null,
    querySelectorAll: () => [],
  },
};
vm.createContext(sandbox);
vm.runInContext(src + `
globalThis.__v1 = { store, homeSnapshot, homeHtml, homePrimaryAction, homeAttentionRoute };
`, sandbox, { filename: "index.html<script>" });
const V = sandbox.__v1;

let passed = 0;
let failed = 0;
function check(name, condition, detail = "") {
  if (condition) { passed++; console.log(`ok   ${name}`); }
  else { failed++; console.log(`FAIL ${name}${detail ? ` — ${detail}` : ""}`); }
}

function connectedState() {
  return {
    conn: { baseUrl: "http://local", apiKey: "tenant-secret" },
    info: { service: "rusty-server", version: "0.12.0", graphs: [{ name: "react_agent" }] },
    agents: { list: [], loading: false, error: null },
    agentRunHistory: Object.create(null),
    threads: [],
    fabric: { list: [], loading: false, error: null },
    fabricRunHistory: [],
    memory: null,
    learn: { records: [], versions: [] },
  };
}

/* 1. Three primary destinations only: Agents, Work, Operations.
 * The workspace nav must present exactly those three as equal top-level
 * destinations. Mission control and internal specialist tools are not
 * primary destinations. */
{
  const nav = html.match(/<nav class="studio-nav"[\s\S]*?<\/nav>/)?.[0] || "";
  const topButtons = [...nav.matchAll(/<button[^>]*class="studio-nav-button[^"]*"[^>]*>([\s\S]*?)<\/button>/g)];
  const labels = topButtons.map((m) => {
    const b = m[1].match(/<b>([^<]*)<\/b>/)?.[1] || "";
    return b.trim();
  }).filter(Boolean);

  check("v1 nav: exactly three primary destination labels",
    labels.length === 3 && labels.includes("Agents") && labels.includes("Work") && labels.includes("Operations"),
    `found: ${labels.join(", ") || "none"}`);
  check("v1 nav: Mission control is not an equal top-level destination",
    !labels.includes("Mission control"), `found: ${labels.join(", ")}`);
  check("v1 nav: no specialist tool is promoted to equal top-level",
    !["Teams", "Memory", "Learning", "Configuration", "Automations", "Schedules", "Task queue"].some((l) => labels.includes(l)),
    `found: ${labels.join(", ")}`);
}

/* 2. Internal specialist tools remain accessible through contextual handoffs
 * or progressive disclosure, not equal top-level navigation. */
{
  const nav = html.match(/<nav class="studio-nav"[\s\S]*?<\/nav>/)?.[0] || "";
  const topButtons = [...nav.matchAll(/<button[^>]*class="studio-nav-button[^"]*"[^>]*>([\s\S]*?)<\/button>/g)];
  const topLabels = topButtons.map((m) => m[1].match(/<b>([^<]*)<\/b>/)?.[1].trim());
  const specialist = ["Teams", "Memory", "Learning", "Configuration", "Automations", "Schedules", "Task queue"];
  check("v1 disclosure: specialist tools are not top-level nav buttons",
    specialist.every((l) => !topLabels.includes(l)));

  // Teams / fabric, Memory, Learning, Registry, Automations, Schedules, Tasks
  // must still exist as views or panels so they can be reached contextually.
  const views = ["agents-view", "fabric-view", "memory-view", "learn-view", "registry-view",
                 "automations-view", "schedules-view", "tasks-view", "thread-view"];
  check("v1 disclosure: specialist workspaces remain in the product",
    views.every((id) => html.includes(`id="${id}"`)),
    `missing one of: ${views.filter((id) => !html.includes(`id="${id}"`)).join(", ")}`);
}

/* 3. A disconnected first visit presents one clear action, without loading
 * dashboards or platform-status commentary. */
{
  const disconnected = V.homeSnapshot({});
  check("v1 disconnected: one clear primary action",
    disconnected.next && disconnected.next.action === "connect" && disconnected.next.label);
  const markup = V.homeHtml(disconnected);
  check("v1 disconnected: the rendered view exposes exactly one call-to-action",
    (markup.match(/data-home-action="connect"/g) || []).length === 1);
  check("v1 disconnected: no dashboard or status commentary",
    !markup.includes("Mission control") && !markup.includes("Dashboard") &&
    !markup.includes("platform") && !markup.includes("server version") &&
    !markup.includes("Loading the Studio mission board"));
  check("v1 disconnected: no release-number commentary",
    !/v\d+\.\d+/.test(markup) && !markup.includes("0.12.0"));
}

/* 4. Agent creation reads as one visual capability system: purpose, model,
 * knowledge, tools, output, and guardrails. */
{
  const createPanel = html.match(/id="agent-create-panel"[\s\S]*?<\/form>/)?.[0] || "";
  const sections = [...createPanel.matchAll(/class="agent-form-section"[\s\S]*?<b>([^<]+)<\/b>/g)]
    .map((m) => m[1].trim());
  const cards = [...createPanel.matchAll(/class="agent-intent-card"[\s\S]*?<b[^>]*>([^<]+)<\/b>/g)]
    .map((m) => m[1].trim());

  check("v1 agent create: purpose section leads the capability system",
    createPanel.includes("01") && sections.some((s) => /purpose|responsible/i.test(s)));
  check("v1 agent create: capability cards surface model, tools, memory/guardrail, output",
    ["Model", "Tools", "Memory", "Guardrail", "Output contract"].every((label) =>
      cards.includes(label)),
    `cards: ${cards.join(", ")}`);
  // Primary labels = section headings, card titles, form labels, and main buttons.
  // Field-help text may remain technical; it is not a primary label.
  const primaryCreate = createPanel
    .replace(/<span class="field-help"[\s\S]*?<\/span>/g, "")
    .replace(/<span class="agent-intent-state"[\s\S]*?<\/span>/g, "");
  check("v1 agent create: no implementation-endpoint language in primary labels",
    !primaryCreate.includes("/assistants") && !primaryCreate.includes("/api") &&
    !primaryCreate.includes("graph binding") && !primaryCreate.includes("Rusty server"));
}

/* 5. Work is one continuous journey from objective → run → trace → evaluation,
 * preserving exact run/thread ownership. */
{
  const threadView = html.match(/id="thread-view"[\s\S]*?<\/div>\s*(?:<div id=")/)?.[0] ||
                     html.match(/id="thread-view"[\s\S]*?<\/section>\s*<\/div>/)?.[0] || "";
  if (!threadView) {
    check("v1 work: thread view exists", html.includes('id="thread-view"'));
  }
  const stageLabels = [...threadView.matchAll(/data-thread-stage="([^"]+)"[^>]*>([^<]+)<\/button>/g)]
    .map((m) => ({ stage: m[1], label: m[2].trim() }));
  check("v1 work: thread stage tabs are Run, Trace, Evaluate",
    stageLabels.length === 3 &&
    stageLabels.some((s) => s.stage === "run" && s.label === "Run") &&
    stageLabels.some((s) => s.stage === "trace" && s.label === "Trace") &&
    stageLabels.some((s) => s.stage === "evaluate" && s.label === "Evaluate"),
    `found: ${JSON.stringify(stageLabels)}`);

  // Exact run/thread ownership must be visible in the thread identity bar.
  check("v1 work: thread identity exposes exact thread id",
    html.includes('id="th-id"') && html.includes('id="th-graph"'));
  check("v1 work: run session preserves selected thread in store",
    src.includes("store.selected") && src.includes("thread_id"));
}

/* 6. Operations prioritizes exceptions requiring attention; schedules,
 * automations, queues, memory governance, and lifecycle controls are
 * secondary tools. */
{
  const tasksView = html.match(/id="tasks-view"[\s\S]*?<\/div>\s*<div id="automations-view"/)?.[0] || "";
  const hasAttentionFilter = tasksView.includes("attention") || tasksView.includes("Needs attention") ||
                             html.includes('value="attention"');
  check("v1 operations: tasks view surfaces an attention-first filter",
    hasAttentionFilter);

  const opsSpecialists = ["Automations", "Schedules", "Task queue", "Memory governance"];
  const nav = html.match(/<nav class="studio-nav"[\s\S]*?<\/nav>/)?.[0] || "";
  const topLabels = [...nav.matchAll(/<button[^>]*class="studio-nav-button[^"]*"[^>]*>[\s\S]*?<b>([^<]*)<\/b>/g)]
    .map((m) => m[1].trim());
  check("v1 operations: operational tools are not equal top-level destinations",
    !opsSpecialists.some((l) => topLabels.includes(l)));
}

/* 7. Visible primary UI contains no release-number commentary, implementation
 * history, endpoint language, or internal contract narration. */
{
  const header = html.match(/<header>[\s\S]*?<\/header>/)?.[0] || "";
  const nav = html.match(/<nav class="studio-nav"[\s\S]*?<\/nav>/)?.[0] || "";
  const primary = header + nav;
  check("v1 language: no release numbers in primary chrome",
    !/v\d+\.\d+(\.\d+)?/.test(primary) && !primary.includes("0.12.0"));
  check("v1 language: no endpoint paths in primary chrome",
    !primary.includes("/api") && !primary.includes("/assistants") &&
    !primary.includes("/agents") && !primary.includes("/tasks"));
  check("v1 language: no internal contract narration in primary chrome",
    !primary.includes("server truth") && !primary.includes("browser recall") &&
    !primary.includes("durable work") && !primary.includes("content-addressed"));
  check("v1 language: no implementation-history commentary in primary chrome",
    !primary.includes("currently") && !primary.includes("does not") &&
    !primary.includes("at most") && !primary.includes("not stored"));
}

/* 8. Technical evidence and exact manifests remain available through
 * deliberate details/review surfaces. */
{
  check("v1 evidence: exact manifest editor is behind an advanced disclosure",
    html.includes('class="agent-advanced"') && html.includes('id="inp-agent-manifest"'));
  check("v1 evidence: run proof / receipt review surface exists",
    html.includes('id="run-proof"') || html.includes("runProofHtml") || html.includes("runProofRender"));
  check("v1 evidence: configuration review panel sits beside the agent form",
    html.includes('class="agent-review-stack"') && html.includes('id="agent-contract-preview"'));
}

/* 9. Keyboard focus, accessible names, reduced motion, and 390px mobile layout
 * remain usable with no horizontal overflow. */
{
  check("v1 a11y: main views have focusable headings",
    html.includes('id="agents-title" tabindex="-1"') &&
    html.includes('id="home-title" tabindex="-1"'));
  check("v1 a11y: navigation buttons are real buttons with accessible names",
    html.includes('aria-label="Studio workspaces"') &&
    html.includes('class="sr-only"'));
  check("v1 a11y: reduced-motion media query exists",
    html.includes("@media (prefers-reduced-motion: reduce)"));
  check("v1 mobile: layout collapses to single column at narrow widths",
    html.includes("grid-template-columns: 1fr") && html.includes("@media (max-width: 680px)"));
  check("v1 mobile: no fixed min-width on primary chrome that would cause overflow",
    !html.includes("min-width: 390px") && !html.includes("width: 400px"));
}

/* 10. Existing correctness, receipt, tenant-isolation, retry-safety, and
 * evidence-binding guarantees are not weakened. */
{
  check("v1 guarantees: tenant isolation still requires api key",
    html.includes('X-Api-Key') && src.includes("apiKey"));
  check("v1 guarantees: run receipt validation remains in the product",
    src.includes("runProofValidateReceipt") || html.includes('id="run-proof"'));
  check("v1 guarantees: connection identity checks still guard deferred callbacks",
    src.includes("connectionOperationIdentityCurrent") && src.includes("connectionIdentityChanged"));
  check("v1 guarantees: agent manifest editor rejects unknown top-level fields",
    html.includes("Unknown top-level fields are rejected"));
}

if (failed) {
  console.error(`\nFAIL: ${failed} failed, ${passed} passed`);
  process.exit(1);
}
console.log(`\nPASS: ${passed} Studio 1.0 experience assertions`);
