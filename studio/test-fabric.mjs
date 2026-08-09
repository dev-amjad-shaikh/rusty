#!/usr/bin/env node
/* Node unit tests for the pure Agent Fabric / TeamTrace helpers embedded in
 * studio/index.html. Browser bootstrap is removed and wire-to-view behavior
 * is exercised dependency-free under vm.
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
globalThis.__fabric = {
  FABRIC_AGENT_LIMIT, FABRIC_STATUS_LIMIT, FABRIC_STATUS_CONCURRENCY, FABRIC_TRACE_LIMIT,
  FABRIC_ATTEMPT_LIMIT, FABRIC_MEMBER_RENDER_LIMIT,
  agentParseJsonWithNumberKinds, fabricObject, fabricRequestCurrent, fabricNormalizeAgents, fabricGroupKey, fabricGroupLabel,
  fabricGroups, fabricNavigationTarget, fabricFocusData, fabricFocusIdentity,
  fabricDisclosureState, fabricRestoreDisclosures, fabricCreateScheduler,
  fabricStatusTargets, fabricStatusCoverage, fabricCoverageLabel, fabricCacheStatus, fabricBatchOwnsStatus,
  fabricActivationState, fabricAgentHealth, fabricNextLeaseDelay, fabricAcceptedKinds,
  fabricSummaryHtml, fabricGroupButtonHtml, fabricMailboxLabel, fabricMemberButtonHtml,
  fabricRestartLabel, fabricJsonText, fabricSupervisionHtml, fabricMemberEvidenceHtml,
  fabricTraceModel, fabricTraceHtml, fabricCoordinationConsistency, fabricCarryCoordination, fabricReadCoordinationEvidence,
  fabricCoordinationHtml, fabricErrorHtml,
};`, sandbox, { filename: "index.html<script>" });

const F = sandbox.__fabric;
const research = {
  agent_id: "researcher-1",
  team_id: "insight-team",
  manifest: {
    agent_kind: "researcher",
    manifest_version: "researcher/1.4.0",
    accepts: { research: { schema: {} }, critique: { schema: {} } },
    scopes: ["private", "team"],
    supervision: { restart: "permanent", intensity: 2, period_ms: 60000, supervisor: "lead-1" },
  },
  metadata: { display_name: "Research <Lead>" },
  created_at: "2026-08-09T00:00:00Z",
};
const writer = {
  agent_id: "writer-1",
  team_id: "insight-team",
  manifest: { agent_kind: "writer", manifest_version: "writer/2.0.0", accepts: { draft: {} } },
  created_at: "2026-08-09T00:01:00Z",
};
const solo = {
  agent_id: "solo-1",
  manifest: { agent_kind: "reviewer", manifest_version: "reviewer/1.0.0", accepts: {} },
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

{
  const normalized = F.fabricNormalizeAgents([null, {}, research, { ...writer, team_id: "  insight-team  " }, solo], 2);
  check("inventory: malformed records are discarded and render count is bounded",
    normalized.list.length === 2 && normalized.total === 3 && normalized.omitted === 1);
  check("inventory: identifiers and team labels are normalized",
    normalized.list[1].agent_id === "writer-1" && normalized.list[1].team_id === "insight-team");
  check("inventory: source records are not mutated", writer.team_id === "insight-team");
  eq("inventory: non-arrays fail closed", F.fabricNormalizeAgents({}, 10), { list: [], total: 0, omitted: 0 });
  const metadata = F.fabricNormalizeAgents([
    { ...solo, agent_id: "scalar", metadata: "catalog-note" },
    { ...solo, agent_id: "array", metadata: ["a", 2] },
    { ...solo, agent_id: "null", metadata: null },
  ], 10).list;
  check("inventory: arbitrary legal metadata stays byte-semantically visible in raw evidence",
    metadata[0].metadata === "catalog-note" && Array.isArray(metadata[1].metadata) &&
    metadata[1].metadata[1] === 2 && metadata[2].metadata === null);
  const unsafeWire = F.agentParseJsonWithNumberKinds(
    '[{"agent_id":"unsafe","manifest":{"budget":{"max_tokens":9007199254740993}},"metadata":9007199254740993}]');
  const unsafeAgent = F.fabricNormalizeAgents(unsafeWire, 1).list[0];
  const unsafeRaw = F.fabricJsonText(unsafeAgent);
  check("inventory: unsafe Rust integer lexemes survive normalization and raw evidence rendering",
    (unsafeRaw.match(/9007199254740993/g) || []).length === 2 && !unsafeRaw.includes("9007199254740992"));
  check("raw evidence: an unsafe number without a retained wire token fails closed with a visible warning",
    F.fabricJsonText({ max_tokens: 9007199254740992 }).startsWith("WARNING: One or more numeric values"));
}

{
  const groups = F.fabricGroups([solo, writer, research]);
  eq("grouping: declared labels sort before the explicit ungrouped bucket",
    groups.map((group) => [group.key, group.members.map((agent) => agent.agent_id)]),
    [["team:insight-team", ["writer-1", "researcher-1"]], ["ungrouped", ["solo-1"]]]);
  check("grouping: ungrouped is never presented as a server team definition",
    F.fabricGroupLabel(groups[1]) === "Ungrouped" && F.fabricGroupKey(null) === "ungrouped");
  const button = F.fabricGroupButtonHtml(groups[0], true);
  check("grouping: accessible selection and registry-only language are rendered",
    button.includes('role="option"') && button.includes('aria-selected="true"') &&
    button.includes('tabindex="0"') && button.includes("team label") && button.includes("2 members"));
}

{
  const summary = F.fabricSummaryHtml([research, writer, solo]);
  check("summary: uses registry facts rather than inventing live global health",
    summary.includes(">3</b><span>Retained identities") &&
    summary.includes(">1</b><span>Declared team labels") &&
    summary.includes(">1</b><span>Supervised manifests") &&
    summary.includes(">3</b><span>Accepted message kinds"));
}

{
  const observedAt = Date.parse("2026-08-09T06:00:00Z");
  const active = { activation: { owner: "host-1", fencing: 3, lease_expires_at: "2026-08-09T07:00:00Z" }, mailbox: { queued: 0, in_flight: 1, dead: 0 } };
  const waiting = { activation: null, mailbox: { queued: 3, in_flight: 0, dead: 0 } };
  const dead = { activation: { owner: "host-1", lease_expires_at: "broken" }, mailbox: { queued: 0, in_flight: 0, dead: 2 } };
  const expired = { activation: { owner: "host-old", lease_expires_at: "2026-08-09T05:00:00Z" }, mailbox: {} };
  const malformed = { activation: { owner: "host-?", lease_expires_at: "not-a-time" }, mailbox: {} };
  check("health: dead letters outrank even an invalid activation lease", F.fabricAgentHealth(dead, null, observedAt).tone === "attention");
  check("health: active lease and queued work remain distinguishable",
    F.fabricAgentHealth(active, null, observedAt).tone === "active" && F.fabricAgentHealth(waiting, null, observedAt).tone === "waiting");
  check("health: expired and malformed activation records never look active",
    F.fabricActivationState(expired, observedAt).state === "expired" &&
    F.fabricAgentHealth(expired, null, observedAt).label === "activation lease expired" &&
    F.fabricActivationState(malformed, observedAt).state === "unknown" &&
    F.fabricAgentHealth(malformed, null, observedAt).tone === "unknown");
  check("health: missing activation evidence differs from an observed null lease",
    F.fabricActivationState(null, observedAt).state === "unknown" &&
    F.fabricActivationState({ activation: null }, observedAt).state === "none");
  check("health: known empty mailbox is idle but missing or failed evidence is unknown",
    F.fabricAgentHealth({ activation: null, mailbox: {} }, null, observedAt).tone === "idle" &&
    F.fabricAgentHealth(null, null, observedAt).tone === "unknown" && F.fabricAgentHealth({}, { message: "failed" }, observedAt).tone === "unknown");
  check("health: observed live leases schedule one honest expiry transition",
    F.fabricNextLeaseDelay({ active }, observedAt) === 3600025 && F.fabricNextLeaseDelay({ expired }, observedAt) === null);
  check("health: mailbox label does not turn missing evidence into zero",
    F.fabricMailboxLabel(null) === "not loaded" && F.fabricMailboxLabel(waiting).startsWith("3 queued"));
  const row = F.fabricMemberButtonHtml(research, true, dead, null);
  check("member row: identity, manifest pin, mailbox evidence, and screen-reader health travel together",
    row.includes("researcher-1") && row.includes("researcher/1.4.0") &&
    row.includes("2 dead") && row.includes("2 dead-lettered") && row.includes('aria-current="true"') &&
    row.includes('tabindex="0"'));
}

{
  const members = Array.from({ length: 40 }, (_, index) => ({ agent_id: `member-${index}` }));
  const group = { members };
  const targets = F.fabricStatusTargets(group, "member-35", F.FABRIC_STATUS_LIMIT);
  check("status coverage: the selected member owns one bounded status slot even outside the leading registry window",
    targets.length === 30 && targets[0].agent_id === "member-35" &&
    targets.some((agent) => agent.agent_id === "member-0") && !targets.some((agent) => agent.agent_id === "member-35" && agent !== targets[0]));
  const statuses = { "member-35": { mailbox: {} }, "member-0": { mailbox: {} } };
  const errors = { "member-1": { message: "unavailable" } };
  const coverage = F.fabricStatusCoverage(group, "member-35", statuses, errors);
  check("status coverage: loaded, failed, pending, and deliberately unrequested members are honest",
    coverage.loaded === 2 && coverage.failed === 1 && coverage.pending === 27 && coverage.notRequested === 10 &&
    F.fabricCoverageLabel(coverage).includes("2 loaded") && F.fabricCoverageLabel(coverage).includes("1 unavailable") &&
    F.fabricCoverageLabel(coverage).includes("27 pending") && F.fabricCoverageLabel(coverage).includes("10 not requested"));

  let generation = 1;
  let activeRequests = 0;
  let maximumRequests = 0;
  const started = [];
  const scheduler = F.fabricCreateScheduler(F.FABRIC_STATUS_CONCURRENCY);
  const scheduleBatch = (batch, count) => Array.from({ length: count }, (_, index) => scheduler.run(async () => {
    activeRequests++;
    maximumRequests = Math.max(maximumRequests, activeRequests);
    started.push(`${batch}:${index}`);
    await Promise.resolve();
    activeRequests--;
    return `${batch}:${index}`;
  }, () => generation === batch));
  const first = scheduleBatch(1, 12);
  generation = 2;
  const second = scheduleBatch(2, 12);
  generation = 3;
  const third = scheduleBatch(3, 12);
  const results = await Promise.all([...first, ...second, ...third]);
  check("status loading: one global scheduler caps rapid selection and group refresh traffic",
    maximumRequests === F.FABRIC_STATUS_CONCURRENCY && results.length === 36);
  check("status loading: stale queued generations are cancelled before they reach the server",
    started.filter((value) => value.startsWith("1:")).length === F.FABRIC_STATUS_CONCURRENCY &&
    !started.some((value) => value.startsWith("2:")) && started.filter((value) => value.startsWith("3:")).length === 12 &&
    results.filter((value) => value && value.skipped).length === 20);

  const cache = { statuses: {}, statusErrors: {} };
  F.fabricCacheStatus(cache, "member-1", { mailbox: { queued: 9 } }, null);
  F.fabricCacheStatus(cache, "member-1", null, { message: "refresh failed" });
  check("status cache: a failed refresh removes old gauges instead of pairing them with an unknown-health error",
    !("member-1" in cache.statuses) && cache.statusErrors["member-1"].message === "refresh failed");
  F.fabricCacheStatus(cache, "member-1", { mailbox: { queued: 1 } }, null);
  check("status cache: a later success clears the failure", cache.statuses["member-1"].mailbox.queued === 1 && !("member-1" in cache.statusErrors));
  check("status ownership: a member selected during an older batch is reserved for its detail request",
    F.fabricBatchOwnsStatus("member-7", "member-1") && !F.fabricBatchOwnsStatus("member-7", "member-7"));
}

{
  const status = {
    activation: { owner: "host-7", fencing: 4, lease_expires_at: "2099-08-09T01:00:00Z" },
    mailbox: { queued: 2, in_flight: 1, dead: 1 },
  };
  const supervision = {
    policy: research.manifest.supervision,
    escalated: true,
    deadline_breached: false,
    suppressed_failures: 3,
    attempts: [{ ordinal: 1, trigger: "turn_failed", message: "provider timeout", task_id: "task-1" }],
    journal_run_id: "agent-supervision:default:researcher-1",
    events: [],
  };
  const detail = F.fabricMemberEvidenceHtml(research, {
    agentId: research.agent_id, loading: false, status, statusError: null,
    supervision, supervisionError: null,
  });
  check("member evidence: declared identity, activation, mailbox, capability, and scope are legible",
    detail.includes("insight-team") && detail.includes("host-7 · fence 4") &&
    detail.includes("critique, research") && detail.includes("private, team") && detail.includes("Dead letter"));
  check("member evidence: escalation, policy, attempt, and raw proof remain attributable",
    detail.includes("escalated") && detail.includes("permanent") && detail.includes("provider timeout") &&
    detail.includes("Raw supervision evidence excerpt"));
  const failedStatus = F.fabricMemberEvidenceHtml(research, {
    agentId: research.agent_id, loading: false, status: null,
    statusError: { message: "tenant changed" }, supervision: null,
    supervisionError: { message: "unavailable" },
  });
  check("member evidence: independent endpoint failures stay explicit instead of becoming zeros",
    failedStatus.includes("tenant changed") && failedStatus.includes("Supervision evidence unavailable") &&
    failedStatus.includes("Activation evidence not loaded") && !failedStatus.includes("No current host lease") &&
    !failedStatus.includes(">0</b><span>Queued"));
}

const connectedTrace = {
  coordination_id: "coord-1",
  connected: true,
  trace: {
    run_ids: ["coordination:default:coord-1"],
    roots: ["coordination:default:coord-1:0"],
    nodes: [
      { event_id: "coordination:default:coord-1:0", run_id: "coordination:default:coord-1", seq: 0, kind: "coordination_start", depth: 0 },
      { event_id: "coordination:default:coord-1:1", run_id: "coordination:default:coord-1", seq: 1, kind: "mailbox_send", parent: "coordination:default:coord-1:0", depth: 1 },
      { event_id: "coordination:default:coord-1:2", run_id: "coordination:default:coord-1", seq: 2, kind: "mailbox_receive", parent: "coordination:default:coord-1:1", depth: 2 },
    ],
  },
};

{
  const model = F.fabricTraceModel(connectedTrace);
  check("TeamTrace: server connectivity is accepted only with one root and every depth present",
    model.connected && model.roots.length === 1 && model.nodes.every((node) => node.depth !== null));
  const missingDepth = structuredClone(connectedTrace);
  delete missingDepth.trace.nodes[2].depth;
  check("TeamTrace: a contradictory connected flag fails closed",
    !F.fabricTraceModel(missingDepth).connected);
  const multipleRoots = structuredClone(connectedTrace);
  multipleRoots.trace.roots.push("detached:0");
  check("TeamTrace: detached roots are incomplete evidence", !F.fabricTraceModel(multipleRoots).connected);
  const bounded = F.fabricTraceModel({ connected: true, trace: {
    roots: ["r:0"], run_ids: ["r"],
    nodes: Array.from({ length: 4 }, (_, index) => ({ event_id: `r:${index}`, run_id: "r", seq: index, kind: "x", depth: 99 })),
  } }, 2);
  check("TeamTrace: render count and untrusted visual indentation are bounded",
    bounded.nodes.length === 2 && bounded.omitted === 2 && bounded.nodes[0].depth === 20);
  const hiddenInvalid = { connected: true, trace: {
    roots: ["r:0"], run_ids: ["r"],
    nodes: [
      { event_id: "r:0", run_id: "r", seq: 0, kind: "start", depth: 0 },
      { event_id: "r:1", run_id: "r", seq: 1, kind: "future" },
    ],
  } };
  check("TeamTrace: an invalid node beyond the render cap still invalidates the whole evidence claim",
    !F.fabricTraceModel(hiddenInvalid, 1).connected && F.fabricTraceModel(hiddenInvalid, 1).unreachable === 1);
  const crossJournal = { connected: true, trace: {
    roots: ["coord:0"], run_ids: ["coord", "member-a", "member-z"],
    nodes: [
      { event_id: "coord:0", run_id: "coord", seq: 0, kind: "coordination_start", depth: 0 },
      { event_id: "member-a:0", run_id: "member-a", seq: 0, kind: "node_output", parent: "member-z:0", depth: 2 },
      { event_id: "coord:1", run_id: "coord", seq: 1, kind: "mailbox_send", parent: "coord:0", depth: 1 },
      { event_id: "member-z:0", run_id: "member-z", seq: 0, kind: "node_input", parent: "coord:1", depth: 2 },
    ],
  } };
  const causal = F.fabricTraceModel(crossJournal);
  check("TeamTrace: cross-journal source order becomes bounded causal parent-to-child preorder",
    causal.nodes.map((node) => node.eventId).join(",") === "coord:0,coord:1,member-z:0,member-a:0");
  const causalHtml = F.fabricTraceHtml(crossJournal);
  check("TeamTrace: every row exposes its journal and explicit causal parent",
    causalHtml.includes("member-z") && causalHtml.includes("caused by coord:1") &&
    causalHtml.includes("caused by member-z:0"));
  const deepNodes = Array.from({ length: 12000 }, (_, index) => ({
    event_id: `deep:${index}`, run_id: "deep", seq: index, kind: "node_input",
    ...(index ? { parent: `deep:${index - 1}` } : {}), depth: index,
  }));
  const deep = F.fabricTraceModel({ connected: true, trace: {
    roots: ["deep:0"], run_ids: ["deep"], nodes: deepNodes,
  } }, 3);
  check("TeamTrace: deep valid chains are traversed iteratively without overflowing and remain render-bounded",
    deep.connected && deep.nodes.length === 3 && deep.omitted === 11997 && deep.nodes[2].eventId === "deep:2");
  const html = F.fabricTraceHtml(connectedTrace);
  check("TeamTrace: connected causal braid identifies journals, roots, events, sequence, and depth",
    html.includes("Connected causal evidence") && html.includes("1 root") &&
    html.includes("mailbox send") && html.includes("caused by") && html.includes("seq 1") && html.includes("depth 2"));
  const unsafe = structuredClone(connectedTrace);
  unsafe.connected = false;
  unsafe.trace.nodes[1].kind = "<future_event>";
  const warning = F.fabricTraceHtml(unsafe);
  check("TeamTrace: disconnected evidence warns and future kinds are escaped",
    warning.includes("Incomplete causal evidence") && warning.includes("do not treat this as one trustworthy tree") &&
    warning.includes("&lt;future event&gt;") && !warning.includes("<future event>"));
}

{
  const record = {
    coordination_id: "coord-1",
    contract: { pattern: "fan_out", max_in_flight: 2 },
    members: [
      { member: "research", agent_id: "researcher-1", submitted: true, disposition: { settlement: "completed" } },
      { member: "write", agent_id: "writer-1", submitted: true, disposition: null },
    ],
    settled: false,
    outcome: null,
    updated_at: "2026-08-09T01:02:03Z",
  };
  const view = F.fabricCoordinationHtml(record, connectedTrace, null);
  check("coordination: typed contract, open state, members, dispositions, and trace are joined",
    view.includes("fan out") && view.includes("in progress") && view.includes(">2</b>") &&
    view.includes("research · completed") && view.includes("write · submitted") && view.includes("Connected causal evidence"));
  const partial = F.fabricCoordinationHtml(record, null, { message: "journal integrity failed" });
  check("coordination: a trace failure does not erase the durable contract", partial.includes("fan out") && partial.includes("journal integrity failed"));
  const oversizedMembers = [null, ...Array.from({ length: F.FABRIC_MEMBER_RENDER_LIMIT + 5 }, (_, index) => ({
    member: `member-${index}`, submitted: true, disposition: index % 2 ? null : { settlement: "completed" },
  }))];
  const oversized = F.fabricCoordinationHtml({ ...record, members: oversizedMembers }, null, null);
  check("coordination: malformed and oversized member evidence degrades safely under a hard DOM bound",
    oversized.includes("unknown member · not submitted") &&
    (oversized.match(/class="fabric-disposition"/g) || []).length === F.FABRIC_MEMBER_RENDER_LIMIT &&
    oversized.includes("6 additional member dispositions were not rendered"));
  check("coordination: bounded raw excerpts never claim to be exact evidence",
    oversized.includes("Raw coordination contract and outcome excerpt") && !oversized.includes("Exact"));
  check("coordination: open record and open trace from the same id reconcile",
    F.fabricCoordinationConsistency("coord-1", record, connectedTrace).consistent);
  const settledTrace = structuredClone(connectedTrace);
  settledTrace.trace.nodes.push({ event_id: "coordination:default:coord-1:3", run_id: "coordination:default:coord-1", seq: 3,
    kind: "coordination_end", parent: "coordination:default:coord-1:2", depth: 3 });
  check("coordination: terminal record and terminal TeamTrace reconcile",
    F.fabricCoordinationConsistency("coord-1", { ...record, settled: true }, settledTrace).consistent);
  check("coordination: cross-id and settlement revision mismatches remain explicit",
    !F.fabricCoordinationConsistency("coord-2", record, connectedTrace).consistent &&
    F.fabricCoordinationConsistency("coord-1", { ...record, settled: true }, connectedTrace).warning.includes("different revisions"));

  const settledRecord = { ...record, settled: true };
  const readSequence = (values) => {
    let index = 0;
    return async () => {
      const value = values[index++];
      if (value instanceof Error) throw value;
      return structuredClone(value);
    };
  };
  const reconciled = await F.fabricReadCoordinationEvidence("coord-1",
    readSequence([connectedTrace, settledTrace]), readSequence([settledRecord, settledRecord]), () => true);
  check("coordination reads: one successful ordered retry reconciles a crossed settlement revision",
    !reconciled.stale && !reconciled.consistencyWarning && reconciled.record.settled &&
    reconciled.trace.trace.nodes.at(-1).kind === "coordination_end");

  const traceRetryFailed = await F.fabricReadCoordinationEvidence("coord-1",
    readSequence([connectedTrace, new Error("trace refresh failed")]), readSequence([settledRecord, settledRecord]), () => true);
  check("coordination reads: a failed trace retry preserves the original trace and names the failed endpoint",
    traceRetryFailed.trace.trace.nodes.length === connectedTrace.trace.nodes.length && !traceRetryFailed.traceError &&
    traceRetryFailed.consistencyWarning.includes("TeamTrace: trace refresh failed") &&
    traceRetryFailed.consistencyWarning.includes("different revisions"));

  const recordRetryFailed = await F.fabricReadCoordinationEvidence("coord-1",
    readSequence([connectedTrace, settledTrace]), readSequence([settledRecord, new Error("record refresh failed")]), () => true);
  check("coordination reads: a failed record retry keeps the refreshed trace and never masquerades as a trace failure",
    recordRetryFailed.trace.trace.nodes.at(-1).kind === "coordination_end" && !recordRetryFailed.traceError &&
    recordRetryFailed.consistencyWarning.includes("coordination record: record refresh failed") &&
    !recordRetryFailed.consistencyWarning.includes("TeamTrace:"));

  let currentChecks = 0;
  let traceReads = 0;
  let recordReads = 0;
  const staleBetweenRetries = await F.fabricReadCoordinationEvidence("coord-1",
    async () => { traceReads++; return structuredClone(traceReads === 1 ? connectedTrace : settledTrace); },
    async () => { recordReads++; return structuredClone(settledRecord); },
    () => ++currentChecks < 3);
  check("coordination reads: a tenant or request change between retry reads cancels before the record endpoint",
    staleBetweenRetries.stale && traceReads === 2 && recordReads === 1);

  const cancelledRefresh = F.fabricCarryCoordination({ id: "coord-1", loading: true,
    record: null, trace: null, error: null });
  check("coordination refresh: an inventory generation change cancels loading and leaves an immediate retry path",
    cancelledRefresh.id === "coord-1" && !cancelledRefresh.loading && !cancelledRefresh.record &&
    cancelledRefresh.notice.includes("cancelled") &&
    html.includes("const coordination = fabricCarryCoordination(previous.coordination)") &&
    html.includes('if (coordination.notice) $("fabric-announcer").textContent = coordination.notice'));
  const retainedEvidence = F.fabricCarryCoordination({ id: "coord-1", loading: false,
    record, trace: connectedTrace, consistencyWarning: null, error: null });
  check("coordination refresh: settled evidence is preserved when no coordination read is in flight",
    retainedEvidence.record.coordination_id === "coord-1" && retainedEvidence.trace.connected && !retainedEvidence.notice);
}

{
  check("compatibility: route-less older server is explained as a capability gap",
    F.fabricErrorHtml(404, { raw: "not found" }, "Durable inventory").includes("needs an Agent Fabric server"));
  check("errors: unknown coordination and invalid identifiers have distinct recovery copy",
    F.fabricErrorHtml(404, { message: "coordination missing" }, "Coordination").includes("was not found") &&
    F.fabricErrorHtml(400, { message: "invalid id" }, "Coordination").includes("not valid"));
  check("raw evidence: large values are visibly bounded", F.fabricJsonText({ value: "x".repeat(500) }, 100).includes("truncated"));
}

{
  eq("keyboard: arrows, home, and end remain inside the current list", [
    F.fabricNavigationTarget(["a", "b", "c"], "b", "ArrowDown"),
    F.fabricNavigationTarget(["a", "b", "c"], "b", "ArrowUp"),
    F.fabricNavigationTarget(["a", "b", "c"], "c", "ArrowDown"),
    F.fabricNavigationTarget(["a", "b", "c"], "b", "Home"),
    F.fabricNavigationTarget(["a", "b", "c"], "b", "End"),
  ], ["c", "a", "c", "a", "c"]);
  const tenantA = { baseUrl: "/api", apiKey: "tenant-a" };
  check("isolation: evidence is current only for the captured server, tenant, and generation",
    F.fabricRequestCurrent(2, tenantA, 2, { ...tenantA }) &&
    !F.fabricRequestCurrent(2, tenantA, 3, tenantA) &&
    !F.fabricRequestCurrent(2, tenantA, 2, { baseUrl: "/api", apiKey: "tenant-b" }));

  let focused = null;
  const fakeOptions = ["member-a", "member-b"].map((id) => ({
    getAttribute(name) { return name === "data-fabric-agent" ? id : null; },
    focus(options) { focused = { id, options }; },
  }));
  const fakeContainer = { querySelectorAll(selector) { return selector === "[data-fabric-agent]" ? fakeOptions : []; } };
  const identity = F.fabricFocusIdentity(fakeOptions[1]);
  F.fabricFocusData(fakeContainer, "data-fabric-agent", identity.value);
  check("focus continuity: a replaced roving option can be resolved and focused by stable identity",
    identity.kind === "agent" && identity.value === "member-b" && focused.id === "member-b" && focused.options.preventScroll);
  check("focus continuity: every Team Observatory render captures and restores the active option",
    html.includes("const focusIdentity = fabricFocusIdentity(document.activeElement)") &&
    html.includes("fabricRestoreFocus(focusIdentity);"));

  const priorDetails = [
    { open: true, getAttribute(name) { return name === "data-fabric-disclosure" ? "member-manifest" : null; } },
    { open: false, getAttribute(name) { return name === "data-fabric-disclosure" ? "trace-raw" : null; } },
  ];
  const nextDetails = [
    { open: false, getAttribute(name) { return name === "data-fabric-disclosure" ? "member-manifest" : null; } },
    { open: false, getAttribute(name) { return name === "data-fabric-disclosure" ? "trace-raw" : null; } },
  ];
  const priorContainer = { querySelectorAll(selector) { return selector.includes("[open]") ? priorDetails.filter((item) => item.open) : priorDetails; } };
  const nextContainer = { querySelectorAll() { return nextDetails; } };
  const disclosures = F.fabricDisclosureState(priorContainer);
  F.fabricRestoreDisclosures(nextContainer, disclosures);
  check("disclosure continuity: asynchronous evidence refresh preserves the user's open raw panel",
    disclosures.length === 1 && disclosures[0] === "member-manifest" && nextDetails[0].open && !nextDetails[1].open);

  let focusedControl = false;
  const summary = {
    getAttribute(name) { return name === "data-fabric-focus" ? "member-manifest" : null; },
    focus() { focusedControl = true; },
  };
  const controlIdentity = F.fabricFocusIdentity(summary);
  F.fabricFocusData({ querySelectorAll() { return [summary]; } }, "data-fabric-focus", controlIdentity.value);
  check("focus continuity: an open disclosure summary also survives an asynchronous rerender",
    controlIdentity.kind === "control" && focusedControl);
}

check("markup: team observatory is a first-class sidebar workspace with a labelled heading",
  html.includes('id="btn-fabric-open"') && html.includes('id="fabric-view"') &&
  html.includes('aria-labelledby="fabric-title"') && html.includes('id="fabric-title" tabindex="-1"'));
check("markup: assistant configuration and durable runtime identity are explicitly distinguished",
  html.includes("Workbench assistant") && html.includes("Durable agent identity") &&
  html.includes("mailbox-addressed runtime identity"));
check("markup: inventory, team labels, member evidence, and coordination form have semantic controls",
  html.includes('id="fabric-groups" role="listbox"') &&
  html.includes('id="fabric-coordination-form"') && html.includes('id="fabric-announcer" role="status"'));
check("accessibility: team rerenders stay quiet while concise selected-state changes use a dedicated announcer",
  html.includes('id="fabric-team-announcer" role="status" aria-live="polite"') &&
  html.includes('<section class="card fabric-team-card" id="fabric-team">') &&
  !html.includes('id="fabric-team" aria-live='));
check("markup: read-only observatory never offers restart, cancel, or coordination submission actions",
  !html.match(/id="fabric-[^"]*"[^>]*>[^<]*(restart|cancel|delegate|fan.?out|race|quorum)/i));
check("responsive: team layout and causal evidence stack at narrow widths",
  html.includes(".fabric-members { grid-template-columns: 1fr; }") &&
  html.includes(".fabric-coordination-head { flex-direction: column; }") &&
  html.includes(".fabric-trace-event { grid-template-columns: 1fr;"));
check("accessibility: essential TeamTrace sequence and depth use the AA text token",
  html.includes(".fabric-trace-event small { color: var(--text-dim);") &&
  !html.includes(".fabric-trace-event small { color: var(--text-faint);"));

if (failed) {
  console.error(`\nFAIL: ${failed} failed, ${passed} passed`);
  process.exit(1);
}
console.log(`\nPASS: ${passed} Agent Fabric / TeamTrace assertions`);
