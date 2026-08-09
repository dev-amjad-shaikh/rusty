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

const sandbox = {};
vm.createContext(sandbox);
vm.runInContext(src + `
globalThis.__workbench = {
  agentTags, agentSearchItems, agentReadiness, agentReadinessHtml,
  agentCardHtml, agentDefaultInput, agentBuildCreatePayload,
  agentErrorHtml, agentTestResultHtml, agentDetailHtml,
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
  W.agentDefaultInput("react_agent").messages[0].content === "say pong");

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

check("route-missing error explains the required server capability",
  W.agentErrorHtml(404, null).includes("no <code>/assistants</code> routes"));
check("server error message is escaped",
  W.agentErrorHtml(500, { message: "store <offline>" }).includes("&lt;offline&gt;"));

{
  const run = { status: "success", run_id: "019157c4-6f1f-7a3b-8c2d-9e4f5a6b7c8d", thread_id: "thread-42", result: { status: "success" } };
  const detail = W.agentDetailHtml(agent, W.agentReadiness(agent, info, run), run);
  check("detail exposes durable identity and configuration",
    detail.includes("research-coordinator") && detail.includes("12") && detail.includes("production"));
  check("detail provides real run and open-thread actions",
    detail.includes('data-agent-run="research-coordinator"') && detail.includes('data-agent-open-thread="thread-42"'));
  check("detail escapes names and pre-fills test input", detail.includes("Research &lt;Coordinator&gt;") && detail.includes("say pong"));
  check("successful evidence is rendered with success tone",
    W.agentTestResultHtml(run).includes('badge success'));
  check("failed evidence carries its message",
    W.agentTestResultHtml({ status: "error", error: "graph unavailable" }).includes("graph unavailable"));
}

console.log(`\n${passed} passed, ${failed} failed`);
process.exit(failed ? 1 : 0);
