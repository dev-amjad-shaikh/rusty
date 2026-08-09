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
  connectionIdentityChanged,
  agentTags, agentSearchItems, agentReadiness, agentReadinessHtml,
  agentCardHtml, agentGraphLabel, agentDefaultInput, agentBuildRunInput,
  agentJourney, agentJourneyHtml, agentBuildCreatePayload,
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

check("connection scope: same server and tenant retains session evidence",
  W.connectionIdentityChanged({ baseUrl: "/api", apiKey: "tenant-a" }, { baseUrl: "/api", apiKey: "tenant-a" }) === false);
check("connection scope: changing server clears session evidence",
  W.connectionIdentityChanged({ baseUrl: "/api", apiKey: "tenant-a" }, { baseUrl: "http://other", apiKey: "tenant-a" }) === true);
check("connection scope: changing tenant key clears session evidence",
  W.connectionIdentityChanged({ baseUrl: "/api", apiKey: "tenant-a" }, { baseUrl: "/api", apiKey: "tenant-b" }) === true);

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
    detail.includes('data-agent-run="research-coordinator"') &&
    detail.includes('data-agent-open-thread="thread-42"') &&
    detail.includes('data-agent-open-run="019157c4-6f1f-7a3b-8c2d-9e4f5a6b7c8d"'));
  check("detail uses a plain-language task instead of raw JSON for conversational agents",
    detail.includes('id="inp-agent-prompt"') && detail.includes("Reply with a short hello.") && !detail.includes("Test input (JSON)"));
  check("detail escapes names and offers the trace as the next step",
    detail.includes("Research &lt;Coordinator&gt;") && detail.includes("Inspect run"));
  check("successful evidence is rendered with success tone",
    W.agentTestResultHtml(run).includes("Run succeeded"));
  check("failed evidence carries its message",
    W.agentTestResultHtml({ status: "error", error: "graph unavailable" }).includes("graph unavailable"));
}

console.log(`\n${passed} passed, ${failed} failed`);
process.exit(failed ? 1 : 0);
