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

const localStorageData = new Map();
const fakeLocalStorage = {
  writes: 0,
  failWrites: 0,
  failReads: 0,
  getItem(key) {
    if (this.failReads > 0) { this.failReads--; throw new Error("blocked"); }
    return localStorageData.has(key) ? localStorageData.get(key) : null;
  },
  setItem(key, value) {
    this.writes++;
    if (this.failWrites > 0) { this.failWrites--; throw new Error("quota"); }
    localStorageData.set(key, String(value));
  },
  removeItem(key) { localStorageData.delete(key); },
  clear() { localStorageData.clear(); },
};
const sandbox = { localStorage: fakeLocalStorage };
vm.createContext(sandbox);
vm.runInContext(src + `
globalThis.__fabric = {
  FABRIC_AGENT_LIMIT, FABRIC_STATUS_LIMIT, FABRIC_STATUS_CONCURRENCY, FABRIC_TRACE_LIMIT,
  FABRIC_ATTEMPT_LIMIT, FABRIC_MEMBER_RENDER_LIMIT, FABRIC_COMPOSER_MEMBER_LIMIT,
  FABRIC_COMPOSER_INPUT_LIMIT, FABRIC_COMPOSER_TOTAL_INPUT_LIMIT, FABRIC_COMPOSER_PREVIEW_LIMIT,
  FABRIC_COMPOSER_CHANNEL_TEXT_LIMIT, FABRIC_COMPOSER_CHANNEL_LIMIT, FABRIC_COMPOSER_CHANNEL_NAME_LIMIT,
  FABRIC_COMPOSER_EFFECTS, FABRIC_COMPOSER_PATTERNS, FABRIC_COMPOSER_RACE_EFFECTS,
  FABRIC_COMPOSER_QUORUM_RESOLVERS,
  FABRIC_RUN_HISTORY_LIMIT, FABRIC_RUN_SCOPE_LIMIT, FABRIC_RUN_GLOBAL_LIMIT, FABRIC_RUN_STORAGE_LIMIT,
  FABRIC_RUN_REFRESH_LIMIT, FABRIC_RUN_REFRESH_CONCURRENCY, FABRIC_FOLLOW_INTERVAL, FABRIC_FOLLOW_MAX_INTERVAL,
  FABRIC_BLUEPRINT_FORMAT, FABRIC_BLUEPRINT_LIMIT, FABRIC_BLUEPRINT_SCOPE_LIMIT,
  FABRIC_BLUEPRINT_GLOBAL_LIMIT, FABRIC_BLUEPRINT_STORAGE_LIMIT, FABRIC_BLUEPRINT_TEXT_LIMIT,
  LS, store, connectionRunScope,
  agentParseJsonWithNumberKinds, fabricObject, fabricRequestCurrent, fabricNormalizeAgents, fabricGroupKey, fabricGroupLabel,
  fabricGroups, fabricNavigationTarget, fabricFocusData, fabricFocusIdentity, fabricRestoreFocus,
  fabricDisclosureState, fabricRestoreDisclosures, fabricCreateScheduler,
  fabricStatusTargets, fabricStatusCoverage, fabricCoverageLabel, fabricCacheStatus, fabricBatchOwnsStatus,
  fabricActivationState, fabricAgentHealth, fabricNextLeaseDelay, fabricAcceptedKinds,
  fabricSummaryHtml, fabricGroupButtonHtml, fabricMailboxLabel, fabricMemberButtonHtml,
  fabricRestartLabel, fabricJsonText, fabricSupervisionHtml, fabricMemberEvidenceHtml,
  fabricTraceModel, fabricTraceHtml, fabricCoordinationConsistency, fabricCarryCoordination, fabricReadCoordinationEvidence,
  fabricCoordinationHtml, fabricErrorHtml, fabricComposerInitial, fabricCarryComposer, fabricComposerGeneratedId,
  fabricComposerMemberSlug, fabricComposerAssignment, fabricComposerEnsure, fabricComposerInput,
  fabricComposerChannels, fabricComposerValidation, fabricComposerPayload, fabricComposerRecordPayload,
  fabricComposerErrorFor, fabricComposerPatternLabel, fabricComposerDecisionBraidHtml,
  fabricComposerHtml, fabricComposerResetApproval, fabricComposerValidateReceipt,
  fabricComposerReviewHtml, fabricComposerSubmitError,
  fabricBlueprintGeneratedId, fabricNormalizeBlueprint, fabricBlueprintFromComposer,
  fabricBlueprintCompatibility, fabricBlueprintApply, fabricEmptyBlueprintEnvelope,
  fabricParseBlueprintEnvelope, fabricPruneBlueprintEnvelope, loadFabricBlueprints,
  saveFabricBlueprints, fabricRememberBlueprint, fabricBlueprintScoreHtml,
  fabricBlueprintDomId, fabricBlueprintCardHtml, fabricBlueprintListHtml, fabricBlueprintFilename, fabricParseBlueprintText,
  fabricBlueprintRevisionSnapshot, fabricBlueprintRevisionName, fabricBlueprintRevisionCandidate, fabricBlueprintRevisionChanges,
  fabricBlueprintRevisionReview, fabricBlueprintRevisionNote, fabricBlueprintRevisionExactHtml, fabricBlueprintEditorHtml,
  fabricBeginBlueprintRevision, fabricSaveCurrentBlueprint,
  fabricRunSafeId, fabricRunTimestamp, fabricRunCount, fabricNormalizeRunRecord,
  fabricRunFromRecord, fabricRunFromPayload, fabricMergeRunHistory, fabricEmptyRunEnvelope,
  fabricParseRunEnvelope, fabricPruneRunEnvelope, loadFabricRunHistory, saveFabricRunHistory,
  fabricRememberRun, fabricMarkRunStale, fabricRunStatusLabel, fabricRunTone,
  fabricRunObservedLabel, fabricRunPulseHtml, fabricVisibleRuns, fabricRunDeskHtml,
  fabricFollowDelay, fabricCoordinationFollowHtml, fabricRunErrorClass,
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
const lead = {
  agent_id: "lead-1",
  team_id: "insight-team",
  manifest: {
    agent_kind: "lead",
    manifest_version: "lead/1.0.0",
    accepts: { coordination_result: {} },
    scopes: ["team"],
  },
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
  const group = { key: "team:insight-team", teamId: "insight-team", members: [research, writer] };
  const allAgents = [research, writer, lead];
  const draft = F.fabricComposerInitial();
  F.fabricComposerEnsure(draft, group);
  check("composer defaults: delegate begins with one pinned team member and explicit non-idempotent effect",
    draft.pattern === "delegate" && draft.selectedIds.length === 1 && draft.selectedIds[0] === research.agent_id &&
    draft.assignments[research.agent_id].kind === "critique" &&
    draft.assignments[research.agent_id].effect === "non_idempotent");
  const freshFanout = F.fabricComposerInitial();
  freshFanout.pattern = "fan_out";
  F.fabricComposerEnsure(freshFanout, group);
  check("composer defaults: a fresh fan-out suggests two members while respecting the render bound",
    freshFanout.selectedIds.length === 2 && F.FABRIC_COMPOSER_MEMBER_LIMIT === 20);
  freshFanout.selectedIds = [research.agent_id];
  F.fabricComposerEnsure(freshFanout, group);
  check("composer roster: a deliberate one-member fan-out remains valid and visibly warned",
    freshFanout.selectedIds.length === 1 &&
    F.fabricComposerValidation(freshFanout, group, allAgents).warnings.some((warning) => warning.includes("one-member fan-out")));
  freshFanout.selectedIds = [];
  F.fabricComposerEnsure(freshFanout, group);
  check("composer roster: removing the last fan-out member leaves an actionable empty-roster error",
    freshFanout.selectedIds.length === 0 &&
    F.fabricComposerValidation(freshFanout, group, allAgents).errors.some((error) => error.field === "roster"));

  const cancelled = F.fabricCarryComposer({ ...freshFanout, submitting: true, request: 7,
    coordinationId: "launch-7", acknowledge: true, assignments: freshFanout.assignments });
  check("composer refresh: an in-flight response is detached honestly without losing its stable retry key",
    !cancelled.submitting && cancelled.coordinationId === "launch-7" && cancelled.request === 7 &&
    cancelled.notice.includes("may still have accepted") && cancelled.notice.includes("launch-7") &&
    !cancelled.acknowledge && cancelled.needsRender);
  const ambiguous = F.fabricComposerInitial();
  ambiguous.coordinationId = "launch-ambiguous-1";
  ambiguous.errorAmbiguous = true;
  ambiguous.acknowledge = true;
  ambiguous.attemptedPayload = { coordination_id: "launch-ambiguous-1", delegate: { delegate: {} } };
  ambiguous.attemptedValidation = { errors: [], warnings: [], selected: [research.agent_id] };
  const carriedAmbiguous = F.fabricCarryComposer(ambiguous);
  check("composer refresh: an ambiguous exact-retry contract stays acknowledged and immutable",
    carriedAmbiguous.errorAmbiguous && carriedAmbiguous.acknowledge &&
    carriedAmbiguous.attemptedPayload === ambiguous.attemptedPayload && carriedAmbiguous.needsRender);
  check("composer identity: browser-generated retry keys are stable server-safe identifiers",
    F.fabricComposerGeneratedId(() => "123e4567-e89b-12d3-a456-426614174000") ===
      "studio-123e4567-e89b-12d3-a456-426614174000" &&
    /^studio-[A-Za-z0-9._-]+$/.test(F.fabricComposerGeneratedId(null, 12345, .25)));

  eq("composer input: plain text stays plain text", F.fabricComposerInput("Summarize the evidence"),
    { value: "Summarize the evidence" });
  eq("composer input: valid JSON becomes typed inline data", F.fabricComposerInput('{"topic":"leases","limit":3}'),
    { value: { topic: "leases", limit: 3 } });
  check("composer input: malformed structured data never silently becomes a string",
    F.fabricComposerInput('{"topic":').error.includes("valid JSON"));
  check("composer input: integers Rust can distinguish but JavaScript cannot represent fail closed",
    Boolean(F.fabricComposerInput('{"limit":9007199254740993}').error));
  check("composer input: UTF-8 byte size and structural depth are bounded before preview or POST",
    F.fabricComposerInput("é".repeat(F.FABRIC_COMPOSER_INPUT_LIMIT)).error.includes("KiB") &&
    F.fabricComposerInput(JSON.stringify({ a: { a: { a: { a: { a: { a: { a: { a: { a: { a: { a: { a: { a: { a: { a: { a: { a: 1 } } } } } } } } } } } } } } } } })).error.includes("nesting"));

  const hostile = { ...research, agent_id: "__proto__" };
  const hostileDraft = F.fabricComposerInitial();
  hostileDraft.selectedIds = [hostile.agent_id];
  F.fabricComposerEnsure(hostileDraft, { members: [hostile] });
  check("composer state: hostile legal identity keys stay own properties on a null-prototype assignment map",
    Object.getPrototypeOf(hostileDraft.assignments) === null &&
    Object.prototype.hasOwnProperty.call(hostileDraft.assignments, "__proto__") &&
    hostileDraft.assignments.__proto__.kind === "critique");

  const changedKind = F.fabricComposerInitial();
  changedKind.selectedIds = [research.agent_id];
  F.fabricComposerEnsure(changedKind, group);
  const narrowedResearch = { ...research, manifest: { ...research.manifest, accepts: { research: {} } } };
  F.fabricComposerEnsure(changedKind, { ...group, members: [narrowedResearch, writer] });
  check("composer registry refresh: a removed accepted kind fails closed instead of silently changing work semantics",
    changedKind.assignments[research.agent_id].kind === "" &&
    F.fabricComposerValidation(changedKind, { ...group, members: [narrowedResearch, writer] }, allAgents)
      .errors.some((error) => error.field === "assignment:researcher-1:kind"));

  const manyMembers = Array.from({ length: F.FABRIC_COMPOSER_MEMBER_LIMIT }, (_, index) => ({
    ...writer, agent_id: `bounded-${index}`, manifest: { ...writer.manifest },
  }));
  const oversizedCombined = F.fabricComposerInitial();
  oversizedCombined.pattern = "fan_out";
  oversizedCombined.selectedIds = manyMembers.map((agent) => agent.agent_id);
  F.fabricComposerEnsure(oversizedCombined, { members: manyMembers });
  for (const id of oversizedCombined.selectedIds) oversizedCombined.assignments[id].input = "x".repeat(7000);
  check("composer input: combined multi-member work is bounded independently of each valid assignment",
    F.fabricComposerValidation(oversizedCombined, { members: manyMembers }, manyMembers)
      .errors.some((error) => error.message.includes("Combined work inputs")));

  const delegate = F.fabricComposerInitial();
  delegate.coordinationId = "launch-delegate-1";
  delegate.delegator = lead.agent_id;
  delegate.parent = "event:brief-ready";
  delegate.selectedIds = [research.agent_id];
  F.fabricComposerEnsure(delegate, group);
  Object.assign(delegate.assignments[research.agent_id], {
    member: "research-role", kind: "research", input: '{"question":"What changed?"}',
    effect: "read_only", deadline: "2099-08-10T12:00:00Z",
  });
  delegate.contextScopes = ["team"];
  delegate.channels = "thread:team-7, kv:briefs\nartifact:source";
  delegate.handoff = true;
  const delegateResult = F.fabricComposerPayload(delegate, group, allAgents, Date.parse("2026-08-09T00:00:00Z"));
  eq("composer contract: delegation pins identity, manifest, accepted kind, effect, input, context, and causality",
    delegateResult.payload, {
      coordination_id: "launch-delegate-1", delegator: "lead-1", parent: "event:brief-ready",
      delegate: {
        delegate: {
          member: "research-role", agent_id: "researcher-1", manifest_version: "researcher/1.4.0",
          kind: "research", input: { kind: "inline", value: { question: "What changed?" } },
          effect: "read_only", deadline: "2099-08-10T12:00:00.000Z",
        },
        context: { scopes: ["team"], channels: ["thread:team-7", "kv:briefs", "artifact:source"] },
        handoff: true,
      },
    });
  delegate.assignments[research.agent_id].effect = "compensatable";
  check("composer effects: compensatable work is selectable and requires its declared rollback path in preflight",
    F.FABRIC_COMPOSER_EFFECTS.includes("compensatable") &&
    F.fabricComposerValidation(delegate, group, allAgents, Date.parse("2026-08-09T00:00:00Z"))
      .warnings.some((warning) => warning.includes("compensation path")));
  delegate.assignments[research.agent_id].effect = "read_only";

  const utf8Bounds = F.fabricComposerInitial();
  utf8Bounds.coordinationId = "é".repeat(129);
  utf8Bounds.parent = "😀".repeat(129);
  utf8Bounds.selectedIds = [research.agent_id];
  F.fabricComposerEnsure(utf8Bounds, group);
  utf8Bounds.assignments[research.agent_id].input = "work";
  const utf8Fields = new Set(F.fabricComposerValidation(utf8Bounds, group, allAgents).errors.map((error) => error.field));
  check("composer identifiers: preflight mirrors Rust UTF-8 byte limits for coordination and causal IDs",
    utf8Fields.has("coordinationId") && utf8Fields.has("parent"));

  const falseHandoff = F.fabricComposerInitial();
  falseHandoff.selectedIds = [research.agent_id];
  F.fabricComposerEnsure(falseHandoff, group);
  falseHandoff.assignments[research.agent_id].input = "work";
  falseHandoff.handoff = true;
  check("composer handoff: a control-plane-only launch cannot promise a delegator handoff record",
    F.fabricComposerValidation(falseHandoff, group, allAgents).errors.some((error) => error.field === "handoff"));
  eq("composer context: bounded comma and line-separated channels preserve explicit order",
    F.fabricComposerChannels("thread:one, kv:briefs\nartifact:source"),
    { value: ["thread:one", "kv:briefs", "artifact:source"] });
  check("composer context: total bytes, item count, and each channel name are independently bounded",
    F.fabricComposerChannels("é".repeat(F.FABRIC_COMPOSER_CHANNEL_TEXT_LIMIT)).error.includes("KiB") &&
    F.fabricComposerChannels(Array.from({ length: F.FABRIC_COMPOSER_CHANNEL_LIMIT + 1 }, (_, index) => `c${index}`).join(",")).error.includes("at most") &&
    F.fabricComposerChannels("x".repeat(F.FABRIC_COMPOSER_CHANNEL_NAME_LIMIT + 1)).error.includes("Each context channel"));

  const attemptedEdit = F.fabricComposerInitial();
  attemptedEdit.coordinationId = "attempted-key";
  attemptedEdit.lastAttemptedId = "attempted-key";
  F.fabricComposerResetApproval(attemptedEdit);
  check("composer retry identity: a semantic edit after an attempt mints a fresh key",
    attemptedEdit.coordinationId.startsWith("studio-") && attemptedEdit.coordinationId !== "attempted-key" && !attemptedEdit.lastAttemptedId);
  attemptedEdit.coordinationId = "operator-key";
  attemptedEdit.lastAttemptedId = "attempted-key";
  F.fabricComposerResetApproval(attemptedEdit, true);
  check("composer retry identity: an explicit operator key edit is preserved",
    attemptedEdit.coordinationId === "operator-key" && !attemptedEdit.lastAttemptedId);

  const fanout = F.fabricComposerInitial();
  fanout.pattern = "fan_out";
  fanout.coordinationId = "launch-fanout-1";
  fanout.selectedIds = [research.agent_id, writer.agent_id];
  fanout.maxInFlight = "1";
  fanout.failurePolicy = "fail_fast";
  F.fabricComposerEnsure(fanout, group);
  Object.assign(fanout.assignments[research.agent_id],
    { member: "research", kind: "research", input: "Find sources", effect: "read_only" });
  Object.assign(fanout.assignments[writer.agent_id],
    { member: "writer", kind: "draft", input: "Draft answer", effect: "idempotent" });
  const fanoutResult = F.fabricComposerPayload(fanout, group, allAgents, Date.parse("2026-08-09T00:00:00Z"));
  check("composer contract: fan-out emits a bounded window, failure policy, and one typed delegation per role",
    fanoutResult.payload.fan_out.members.length === 2 && fanoutResult.payload.fan_out.max_in_flight === 1 &&
    fanoutResult.payload.fan_out.on_member_failure === "fail_fast" &&
    fanoutResult.payload.fan_out.members[1].manifest_version === "writer/2.0.0" &&
    fanoutResult.payload.fan_out.members[1].input.value === "Draft answer");
  fanout.assignments[research.agent_id].input = "x".repeat(F.FABRIC_COMPOSER_INPUT_LIMIT - 4);
  fanout.assignments[writer.agent_id].input = "y".repeat(F.FABRIC_COMPOSER_INPUT_LIMIT - 4);
  check("composer preview: a valid large multi-member contract renders a visibly bounded excerpt",
    F.fabricComposerReviewHtml(fanout, group, allAgents).includes("inspection view truncated"));

  const race = F.fabricComposerInitial();
  race.pattern = "race";
  race.coordinationId = "launch-race-1";
  race.selectedIds = [research.agent_id, writer.agent_id];
  F.fabricComposerEnsure(race, group);
  Object.assign(race.assignments[research.agent_id],
    { member: "research", kind: "research", input: "Find the answer", effect: "read_only" });
  Object.assign(race.assignments[writer.agent_id],
    { member: "writer", kind: "draft", input: "Produce the answer", effect: "idempotent" });
  const raceResult = F.fabricComposerPayload(race, group, allAgents);
  check("composer race: every candidate is pinned and only the first safe completion becomes the contract outcome",
    raceResult.payload.race.candidates.length === 2 &&
    raceResult.payload.race.candidates[0].manifest_version === "researcher/1.4.0" &&
    raceResult.payload.race.candidates[1].effect === "idempotent" &&
    !Object.hasOwn(raceResult.payload.race, "members"));
  race.assignments[writer.agent_id].effect = "compensatable";
  check("composer race: compensatable and non-idempotent candidates fail closed before POST",
    F.fabricComposerValidation(race, group, allAgents).errors.some((error) =>
      error.field === "assignment:writer-1:effect" && error.message.includes("loser can be cancelled")));
  race.assignments[writer.agent_id].effect = "idempotent";
  const raceBraid = F.fabricComposerDecisionBraidHtml(race, [research, { ...writer, agent_id: "<writer>" }]);
  check("composer race: decision braid explains convergence and escapes member identities",
    raceBraid.includes("First safe completion") && raceBraid.includes("2 race candidates") &&
    raceBraid.includes("Losing work is cancel-signalled and must be freely repeatable") &&
    raceBraid.includes("&lt;writer&gt;") && !raceBraid.includes("<writer>"));
  check("composer race: launch preflight names cancellation and discarded-work accounting",
    F.fabricComposerReviewHtml(race, group, allAgents).includes("cancel-signalled") &&
    F.fabricComposerReviewHtml(race, group, allAgents).includes("accounted as waste"));

  const raceReceipt = {
    coordination_id: "launch-race-1", start_event: "coordination:default:launch-race-1:0",
    submitted: [
      { member: "research", task_id: "default--launch-race-1--research" },
      { member: "writer", task_id: "default--launch-race-1--writer" },
    ],
  };
  check("composer receipt: new race binds the exact identity, initial member set, tasks, and start event",
    F.fabricComposerValidateReceipt(raceReceipt, raceResult.payload).ok);
  check("composer receipt: mismatched identities and malformed member-task evidence fail closed",
    !F.fabricComposerValidateReceipt({ ...raceReceipt, coordination_id: "other" }, raceResult.payload).ok &&
    !F.fabricComposerValidateReceipt({ ...raceReceipt, start_event: null }, raceResult.payload).ok &&
    !F.fabricComposerValidateReceipt({ ...raceReceipt, start_event: "coordination:other:launch-race-1:0" }, raceResult.payload).ok &&
    !F.fabricComposerValidateReceipt({ ...raceReceipt, submitted: [
      raceReceipt.submitted[0], { member: "writer", task_id: "default--stale-race--writer" }] }, raceResult.payload).ok &&
    !F.fabricComposerValidateReceipt({ ...raceReceipt, submitted: [raceReceipt.submitted[0], raceReceipt.submitted[0]] }, raceResult.payload).ok &&
    !F.fabricComposerValidateReceipt({ ...raceReceipt, submitted: [{ member: "research", task_id: {} }, raceReceipt.submitted[1]] }, raceResult.payload).ok);
  check("composer identity: control characters fail before they can create unverifiable receipt evidence",
    F.fabricComposerValidation({ ...race, coordinationId: "launch\trace-1" }, group, allAgents).errors
      .some((error) => error.field === "coordinationId" && error.message.includes("control characters")));
  check("composer receipt: deduplication has one exact sparse shape",
    F.fabricComposerValidateReceipt({ coordination_id: "launch-race-1", deduplicated: true }, raceResult.payload).deduplicated &&
    !F.fabricComposerValidateReceipt({ coordination_id: "launch-race-1", deduplicated: false }, raceResult.payload).ok &&
    !F.fabricComposerValidateReceipt({ coordination_id: "launch-race-1", deduplicated: true, submitted: [] }, raceResult.payload).ok);
  check("composer receipt: fan-out receipt matches only its reviewed initial window",
    F.fabricComposerValidateReceipt({ coordination_id: "launch-fanout-1", start_event: "coordination:acme:launch-fanout-1:0",
      submitted: [{ member: "research", task_id: "acme--launch-fanout-1--research" }] }, fanoutResult.payload).ok &&
    !F.fabricComposerValidateReceipt({ coordination_id: "launch-fanout-1", start_event: "coordination:acme:launch-fanout-1:0",
      submitted: [{ member: "writer", task_id: "acme--launch-fanout-1--writer" }] }, fanoutResult.payload).ok &&
    !F.fabricComposerValidateReceipt({ coordination_id: "launch-fanout-1", start_event: "coordination:acme:launch-fanout-1:0",
      submitted: [{ member: "research", task_id: "acme--launch-fanout-1--research" }, { member: "writer", task_id: "acme--launch-fanout-1--writer" }] }, fanoutResult.payload).ok);

  const quorum = F.fabricComposerInitial();
  quorum.pattern = "quorum";
  quorum.coordinationId = "launch-quorum-1";
  quorum.selectedIds = [research.agent_id, writer.agent_id];
  quorum.threshold = "2";
  quorum.resolver = "majority_equal";
  F.fabricComposerEnsure(quorum, group);
  Object.assign(quorum.assignments[research.agent_id],
    { member: "research", kind: "research", input: "Judge the case", effect: "read_only" });
  Object.assign(quorum.assignments[writer.agent_id],
    { member: "writer", kind: "draft", input: "Judge the case", effect: "non_idempotent" });
  const quorumResult = F.fabricComposerPayload(quorum, group, allAgents);
  check("composer quorum: threshold and deterministic resolver use the exact Rust wire shape",
    quorumResult.payload.quorum.members.length === 2 && quorumResult.payload.quorum.threshold === 2 &&
    quorumResult.payload.quorum.resolver.resolver === "majority_equal");
  check("composer quorum: majority guidance matches Rust's structural JSON equality",
    F.fabricComposerDecisionBraidHtml(quorum, [research, writer]).includes("structural JSON majority") &&
    F.fabricComposerReviewHtml(quorum, group, allAgents).includes("object key order does not split a vote"));
  check("composer quorum: even majority thresholds disclose tie evidence without blocking a valid contract",
    quorumResult.validation.warnings.some((warning) => warning.includes("can tie")) &&
    quorumResult.validation.warnings.some((warning) => warning.includes("non-idempotent")));
  quorum.threshold = "3";
  quorum.resolver = "custom";
  const invalidQuorum = F.fabricComposerValidation(quorum, group, allAgents);
  check("composer quorum: out-of-range thresholds and unsupported resolvers fail before POST",
    invalidQuorum.errors.some((error) => error.field === "threshold") &&
    invalidQuorum.errors.some((error) => error.field === "resolver"));
  quorum.threshold = "1";
  quorum.resolver = "first_k";
  const firstK = F.fabricComposerPayload(quorum, group, allAgents);
  check("composer quorum: first-k preserves the threshold and warns that unfinished effects may exist",
    firstK.payload.quorum.resolver.resolver === "first_k" && firstK.payload.quorum.threshold === 1 &&
    firstK.validation.warnings.some((warning) => warning.includes("declared effects may already have happened")));
  check("composer quorum: decision braid exposes the declared acceptance rule",
    F.fabricComposerDecisionBraidHtml(quorum, [research, writer]).includes("1 of 2 accepted") &&
    F.fabricComposerDecisionBraidHtml(quorum, [research, writer]).includes("when 1 result is accepted") &&
    F.fabricComposerDecisionBraidHtml(quorum, [research, writer]).includes("accepted. Return the accepted results in deterministic order") &&
    F.fabricComposerDecisionBraidHtml(quorum, [research, writer]).includes("deterministic order"));

  const existingRace = { coordination_id: "race-existing", contract: { pattern: "race", candidates: raceResult.payload.race.candidates } };
  const existingQuorum = { coordination_id: "quorum-existing", contract: { pattern: "quorum", ...firstK.payload.quorum } };
  check("composer deduplication: durable race and quorum records reconstruct request-shaped contracts",
    F.fabricComposerRecordPayload(existingRace).race.candidates.length === 2 &&
    F.fabricComposerRecordPayload(existingQuorum).quorum.resolver.resolver === "first_k");

  const invalid = F.fabricComposerInitial();
  invalid.pattern = "fan_out";
  invalid.coordinationId = "../reserved";
  invalid.delegator = "writer-1";
  invalid.parent = "x".repeat(513);
  invalid.selectedIds = [research.agent_id, writer.agent_id];
  invalid.maxInFlight = "0";
  invalid.failurePolicy = "unknown";
  F.fabricComposerEnsure(invalid, group);
  Object.assign(invalid.assignments[research.agent_id],
    { member: "outcome", kind: "missing", input: "", effect: "unknown", deadline: "2020-01-01T00:00:00Z" });
  Object.assign(invalid.assignments[writer.agent_id],
    { member: "outcome", kind: "draft", input: "work", effect: "pure" });
  const errors = F.fabricComposerValidation(invalid, group, allAgents, Date.parse("2026-08-09T00:00:00Z")).errors;
  const fields = new Set(errors.map((error) => error.field));
  check("composer validation: unsafe ids, parents, roles, kinds, inputs, effects, deadlines, windows, policies, and recipients fail before POST",
    fields.has("coordinationId") && fields.has("parent") && fields.has("delegator") &&
    fields.has("maxInFlight") && fields.has("failurePolicy") &&
    fields.has("assignment:researcher-1:member") && fields.has("assignment:researcher-1:kind") &&
    fields.has("assignment:researcher-1:input") && fields.has("assignment:researcher-1:effect") &&
    fields.has("assignment:researcher-1:deadline") && errors.length >= 10);

  const widened = F.fabricComposerInitial();
  widened.selectedIds = [writer.agent_id];
  F.fabricComposerEnsure(widened, group);
  widened.assignments[writer.agent_id].input = "write";
  widened.contextScopes = ["private"];
  check("composer validation: delegated context cannot widen the pinned member manifest",
    F.fabricComposerValidation(widened, group, allAgents).errors.some((error) => error.field === "contextScopes"));

  delegate.acknowledge = false;
  const composerHtml = F.fabricComposerHtml(delegate, group, allAgents);
  check("composer markup: coordination patterns, roster, semantic form, typed preview, and explicit approval are present",
    composerHtml.includes('id="fabric-compose-form"') && composerHtml.includes('data-compose-pattern="delegate"') &&
    composerHtml.includes('data-compose-pattern="fan_out"') && composerHtml.includes('data-compose-pattern="race"') &&
    composerHtml.includes('data-compose-pattern="quorum"') && composerHtml.includes('aria-pressed="true"') &&
    composerHtml.includes('data-compose-agent-toggle="researcher-1"') &&
    composerHtml.includes("Review typed coordination contract") && composerHtml.includes("which declared effects") &&
    composerHtml.includes('form="fabric-compose-form"') && composerHtml.includes("Start delegation") &&
    composerHtml.includes("disabled"));
  check("composer responsive semantics: launch review is isolated from the application sidebar element",
    composerHtml.includes('<section class="fabric-compose-review"') &&
    !composerHtml.includes('<aside class="fabric-compose-review"'));
  delegate.acknowledge = true;
  check("composer approval: a valid acknowledged contract enables launch without hiding declared effects",
    !F.fabricComposerReviewHtml(delegate, group, allAgents).match(/type="submit"[^>]*disabled/) &&
    F.fabricComposerReviewHtml(delegate, group, allAgents).includes("read_only"));
  delegate.completed = true;
  delegate.receipt = { coordination_id: "launch-delegate-1", submitted: ["task-1"], deduplicated: false };
  delegate.launchedPayload = delegateResult.payload;
  delegate.launchedValidation = delegateResult.validation;
  delegate.launchedAgents = [research];
  const changedRegistry = { ...group, members: [{ ...research,
    manifest: { ...research.manifest, manifest_version: "researcher/9.0.0", accepts: { future: {} } } }, writer] };
  const completedHtml = F.fabricComposerHtml(delegate, changedRegistry, allAgents);
  check("composer completion: a receipt freezes the launched contract and manifest until Compose another",
    completedHtml.includes("researcher/1.4.0") && !completedHtml.includes("researcher/9.0.0") &&
    completedHtml.includes("launch-delegate-1") && completedHtml.includes("Compose another coordination") &&
    completedHtml.includes('class="fabric-compose-fields" disabled'));
  const existingRecord = {
    coordination_id: "launch-delegate-1", delegator: "lead-1", parent: "event:old",
    contract: { pattern: "delegate", delegate: { member: "old-role", agent_id: "writer-1",
      manifest_version: "writer/2.0.0", kind: "draft", input: { kind: "inline", value: "old work" }, effect: "pure" } },
  };
  eq("composer deduplication: durable records reconstruct the actual request-shaped contract",
    F.fabricComposerRecordPayload(existingRecord), {
      coordination_id: "launch-delegate-1", delegator: "lead-1", parent: "event:old",
      delegate: { delegate: { member: "old-role", agent_id: "writer-1", manifest_version: "writer/2.0.0",
        kind: "draft", input: { kind: "inline", value: "old work" }, effect: "pure" } },
    });
  delegate.receipt.deduplicated = true;
  delegate.launchedPayload = F.fabricComposerRecordPayload(existingRecord);
  delegate.launchedValidation = { errors: [], warnings: [], selected: ["writer-1"] };
  const deduplicatedHtml = F.fabricComposerHtml(delegate, group, allAgents);
  check("composer deduplication: a reused key invalidates the requested form and shows only actual durable evidence",
    deduplicatedHtml.includes("submitted draft was not applied") && deduplicatedHtml.includes("old work") &&
    !deduplicatedHtml.includes("What changed?"));
  const crossPattern = { ...delegate,
    launchedPayload: { coordination_id: "launch-delegate-1", fan_out: { members: [{ agent_id: research.agent_id }] } },
    launchedValidation: { errors: [], warnings: [], selected: [research.agent_id] } };
  const crossPatternReview = F.fabricComposerReviewHtml(crossPattern, group, allAgents);
  check("composer deduplication: summaries describe the actual durable pattern, never the rejected draft pattern",
    crossPatternReview.includes("fan-out coordination") && !crossPatternReview.includes("One delegated handoff"));
  const unverified = { ...delegate,
    launchedPayload: { coordination_id: "launch-delegate-1", existing_contract: "Loading the durable record…" },
    launchedValidation: { errors: [], warnings: [], selected: [] } };
  check("composer deduplication: missing durable evidence is labelled unverified instead of claiming an actual contract",
    F.fabricComposerReviewHtml(unverified, group, allAgents).includes("Existing coordination not verified") &&
    F.fabricComposerHtml(unverified, group, allAgents).includes("could not verify"));
  const freshDraft = F.fabricComposerInitial();
  const freshHtml = F.fabricComposerHtml(freshDraft, group, allAgents);
  check("composer identity: every visible draft starts with a persisted browser retry key before acknowledgement",
    freshDraft.coordinationId.startsWith("studio-") && freshHtml.includes(freshDraft.coordinationId));
  check("composer errors: older servers and conflicts have distinct recovery copy",
    F.fabricComposerSubmitError({ status: 404, body: { raw: "not found" } }).includes("does not expose typed coordination") &&
    F.fabricComposerSubmitError({ status: 409, body: { message: "already exists differently" } }).includes("already exists differently"));
}

const blueprintFixture = {
  format: "rusty.team-blueprint/v1",
  blueprint_id: "bp-insight-review-01",
  name: "Insight review cell",
  team_label: "insight-team",
  created_at: "2026-08-09T06:00:00.000Z",
  updated_at: "2026-08-09T06:00:00.000Z",
  pattern: "fan_out",
  roles: [
    { agent_id: "researcher-1", member: "research", manifest_version: "researcher/1.4.0", kind: "research", effect: "read_only" },
    { agent_id: "writer-1", member: "draft", manifest_version: "writer/2.0.0", kind: "draft", effect: "idempotent" },
  ],
  delegator: "lead-1",
  fan_out: { max_in_flight: 2, on_member_failure: "partial" },
};

{
  const draft = F.fabricComposerInitial();
  draft.pattern = "fan_out";
  draft.initialized = true;
  draft.selectedIds = ["researcher-1", "writer-1"];
  draft.assignments = {
    "researcher-1": { member: "research", kind: "research", input: "classified customer prompt", effect: "read_only", deadline: "2099-01-01T00:00:00Z" },
    "writer-1": { member: "draft", kind: "draft", input: "secret result", effect: "idempotent", deadline: "2099-01-02T00:00:00Z" },
  };
  draft.coordinationId = "live-run-should-not-persist";
  draft.parent = "journal-parent-should-not-persist";
  draft.delegator = "lead-1";
  draft.maxInFlight = "2";
  draft.failurePolicy = "partial";
  draft.acknowledge = true;
  draft.receipt = { submitted: [{ task_id: "secret-task" }] };
  const made = F.fabricBlueprintFromComposer("Insight review cell", draft,
    { teamId: "insight-team", members: [research, writer] }, [research, writer, lead],
    Date.parse("2026-08-09T06:00:00Z"), () => "insight-review-01");
  const raw = JSON.stringify(made.value);
  check("blueprints: composer capture keeps exact reusable role and convergence structure",
    !made.error && made.value.blueprint_id === "bp-insight-review-01" &&
    made.value.roles[0].manifest_version === "researcher/1.4.0" && made.value.fan_out.max_in_flight === 2);
  check("blueprints: task inputs, deadlines, coordination identity, causal parent, approval, and receipts never enter storage",
    !raw.includes("classified") && !raw.includes("secret") && !raw.includes("deadline") &&
    !raw.includes("live-run") && !raw.includes("journal-parent") && !raw.includes("acknowledge") && !raw.includes("receipt"));
}

{
  const normalized = F.fabricNormalizeBlueprint(blueprintFixture);
  check("blueprints: versioned structural manifests normalize without semantic loss",
    !normalized.error && JSON.stringify(normalized.value) === JSON.stringify(blueprintFixture));
  const roleLeak = structuredClone(blueprintFixture);
  roleLeak.roles[0].input = "must not be ignored";
  const runLeak = { ...structuredClone(blueprintFixture), coordination_id: "run-1" };
  check("blueprints: unknown task or run fields fail closed instead of being silently dropped",
    F.fabricNormalizeBlueprint(roleLeak).error.includes("unsupported field") &&
    F.fabricNormalizeBlueprint(runLeak).error.includes("unsupported field"));
  check("blueprints: unsupported formats, unsafe race effects, and impossible quorum thresholds fail closed",
    Boolean(F.fabricNormalizeBlueprint({ ...structuredClone(blueprintFixture), format: "rusty.team-blueprint/v2" }).error) &&
    Boolean(F.fabricNormalizeBlueprint({ ...structuredClone(blueprintFixture), pattern: "race", fan_out: undefined,
      roles: [{ ...blueprintFixture.roles[0], effect: "non_idempotent" }] }).error) &&
    Boolean(F.fabricNormalizeBlueprint({ ...structuredClone(blueprintFixture), pattern: "quorum", fan_out: undefined,
      quorum: { threshold: 3, resolver: "majority_equal" } }).error));
  const stringMaximum = structuredClone(blueprintFixture);
  stringMaximum.fan_out.max_in_flight = "2";
  const stringThreshold = { ...structuredClone(blueprintFixture), pattern: "quorum", fan_out: undefined,
    quorum: { threshold: "2", resolver: "majority_equal" } };
  const numericCreated = { ...structuredClone(blueprintFixture), created_at: Date.parse(blueprintFixture.created_at) };
  const numericUpdated = { ...structuredClone(blueprintFixture), updated_at: Date.parse(blueprintFixture.updated_at) };
  const numericDelegator = { ...structuredClone(blueprintFixture), delegator: 42 };
  const numericRole = structuredClone(blueprintFixture);
  numericRole.roles[0].agent_id = 7;
  const arrayEffect = structuredClone(blueprintFixture);
  arrayEffect.roles[0].effect = ["read_only"];
  const invalidScopes = { ...structuredClone(blueprintFixture), pattern: "delegate", roles: [structuredClone(blueprintFixture.roles[0])],
    context: { scopes: 0, channels: [] }, fan_out: undefined };
  const nullScopes = structuredClone(invalidScopes);
  nullScopes.context.scopes = null;
  const invalidChannels = structuredClone(invalidScopes);
  invalidChannels.context.scopes = [];
  invalidChannels.context.channels = false;
  check("blueprints: strict v1 imports reject coercible policy integers, timestamps, and durable identities",
    Boolean(F.fabricNormalizeBlueprint(stringMaximum).error) && Boolean(F.fabricNormalizeBlueprint(stringThreshold).error) &&
    Boolean(F.fabricNormalizeBlueprint(numericCreated).error) && Boolean(F.fabricNormalizeBlueprint(numericUpdated).error) &&
    Boolean(F.fabricNormalizeBlueprint(numericDelegator).error) && Boolean(F.fabricNormalizeBlueprint(numericRole).error));
  check("blueprints: strict v1 imports reject coercible effect and explicit non-array context fields",
    Boolean(F.fabricNormalizeBlueprint(arrayEffect).error) && Boolean(F.fabricNormalizeBlueprint(invalidScopes).error) &&
    Boolean(F.fabricNormalizeBlueprint(nullScopes).error) && Boolean(F.fabricNormalizeBlueprint(invalidChannels).error));
  const parsed = F.fabricParseBlueprintText(JSON.stringify(blueprintFixture));
  check("blueprints: portable JSON round-trips the exact canonical structure",
    JSON.stringify(parsed) === JSON.stringify(blueprintFixture) && F.fabricBlueprintFilename(parsed.name) === "insight-review-cell.rusty-team.json");
  const exactWire = structuredClone(blueprintFixture);
  exactWire.roles[0].manifest_version = " researcher/1.4.0 ";
  exactWire.roles[0].kind = " research ";
  const exactWireNormalized = F.fabricNormalizeBlueprint(exactWire);
  check("blueprints: manifest pins and accepted-kind keys preserve exact legal wire bytes",
    exactWireNormalized.value.roles[0].manifest_version === " researcher/1.4.0 " &&
    exactWireNormalized.value.roles[0].kind === " research ");
}

{
  const ready = F.fabricBlueprintCompatibility(blueprintFixture, [research, writer, lead]);
  const driftedResearch = { ...research, manifest: { ...research.manifest, manifest_version: "researcher/2.0.0" } };
  const drift = F.fabricBlueprintCompatibility(blueprintFixture, [driftedResearch, writer, lead]);
  const kindRemoved = { ...writer, manifest: { ...writer.manifest, accepts: { critique: {} } } };
  const incompatible = F.fabricBlueprintCompatibility(blueprintFixture, [research, kindRemoved, lead]);
  const missing = F.fabricBlueprintCompatibility(blueprintFixture, [research, lead]);
  const unpinnedResearch = { ...research, manifest: { ...research.manifest, manifest_version: "" } };
  const unpinned = F.fabricBlueprintCompatibility(blueprintFixture, [unpinnedResearch, writer, lead]);
  check("blueprints: exact live roster and manifest pins are ready without invented drift",
    ready.loadable && ready.tone === "ready" && ready.changed.length === 0);
  check("blueprints: changed manifest pins remain reviewable but never look exact",
    drift.loadable && drift.tone === "changed" && drift.changed.length === 1 && drift.message.includes("changed"));
  check("blueprints: missing roles and removed message contracts block composer hydration",
    !missing.loadable && missing.label === "Roles missing" && !incompatible.loadable && incompatible.label === "Contract changed");
  check("blueprints: a live role without a manifest pin cannot become a runnable saved contract",
    !unpinned.loadable && unpinned.label === "Manifest unavailable");
  const scopedDelegate = { ...structuredClone(blueprintFixture), pattern: "delegate", roles: [structuredClone(blueprintFixture.roles[0])],
    context: { scopes: ["team"], channels: [] }, fan_out: undefined };
  const scopedReady = F.fabricBlueprintCompatibility(scopedDelegate, [research, writer, lead]);
  const narrowedResearch = { ...research, manifest: { ...research.manifest, scopes: ["private"] } };
  const scopedBlocked = F.fabricBlueprintCompatibility(scopedDelegate, [narrowedResearch, writer, lead]);
  check("blueprints: a live scope-only registry change immediately moves a saved delegate from ready to blocked",
    scopedReady.loadable && scopedReady.tone === "ready" && !scopedBlocked.loadable && scopedBlocked.label === "Scope changed");
  const applied = F.fabricBlueprintApply(blueprintFixture, { list: [driftedResearch, writer, lead] });
  check("blueprints: hydration starts a fresh unapproved coordination with empty task and deadline fields",
    !applied.error && applied.draft.coordinationId === "" && applied.draft.parent === "" && !applied.draft.acknowledge &&
    applied.draft.assignments["researcher-1"].input === "" && applied.draft.assignments["researcher-1"].deadline === "" &&
    applied.draft.notice.includes("changed manifest pin"));
}

{
  check("blueprint editor: repeated revisions get readable bounded names instead of duplicated suffixes",
    F.fabricBlueprintRevisionName("Insight review cell") === "Insight review cell revision" &&
    F.fabricBlueprintRevisionName("Insight review cell revision") === "Insight review cell revision 2" &&
    F.fabricBlueprintRevisionName("Insight review cell revision 2") === "Insight review cell revision 3" &&
    Buffer.byteLength(F.fabricBlueprintRevisionName("x".repeat(100)), "utf8") <= 80 &&
    Buffer.byteLength(F.fabricBlueprintRevisionName("🦀".repeat(20)), "utf8") <= 80);
  const sourceBefore = JSON.stringify(blueprintFixture);
  const applied = F.fabricBlueprintApply(blueprintFixture, { list: [research, writer, lead] });
  applied.draft.assignments["writer-1"].member = "final-draft";
  applied.draft.assignments["writer-1"].input = "must never enter the blueprint";
  applied.draft.assignments["writer-1"].deadline = "2099-01-01T00:00:00Z";
  applied.draft.maxInFlight = "1";
  const revision = {
    source: structuredClone(blueprintFixture), sourceId: blueprintFixture.blueprint_id,
    name: "Insight review cell v2", previewAt: Date.parse("2026-08-10T00:00:00Z"),
    acknowledged: false, acknowledgedSignature: "", saveError: "",
  };
  const review = F.fabricBlueprintRevisionReview(revision, applied.draft, applied.group,
    [research, writer, lead], revision.previewAt, () => "revision-preview");
  check("blueprint editor: composer changes become a separate structural candidate without mutating the source",
    !review.error && review.candidate.blueprint_id === "bp-revision-preview" &&
    review.candidate.roles[1].member === "final-draft" && review.candidate.fan_out.max_in_flight === 1 &&
    JSON.stringify(blueprintFixture) === sourceBefore && review.candidate.blueprint_id !== blueprintFixture.blueprint_id);
  check("blueprint editor: task text and deadlines remain outside both revision comparison and portable candidate",
    !JSON.stringify(review.candidate).includes("must never") && !JSON.stringify(review.candidate).includes("2099") &&
    !JSON.stringify(F.fabricBlueprintRevisionSnapshot(review.candidate)).includes("deadline"));
  check("blueprint editor: exact review names every changed topology surface",
    review.changes.includes("Blueprint name") && review.changes.includes("Role bindings and contracts") &&
    review.changes.includes("Fan-out policy") && !review.approved);
  revision.acknowledged = true;
  revision.acknowledgedSignature = review.signature;
  const approved = F.fabricBlueprintRevisionReview(revision, applied.draft, applied.group,
    [research, writer, lead], revision.previewAt, () => "revision-preview");
  applied.draft.maxInFlight = "2";
  const drifted = F.fabricBlueprintRevisionReview(revision, applied.draft, applied.group,
    [research, writer, lead], revision.previewAt, () => "revision-preview");
  check("blueprint editor: acknowledgement binds the exact candidate and fails closed after a later edit",
    approved.approved && !drifted.approved && drifted.signature !== revision.acknowledgedSignature);
  const editor = F.fabricBlueprintEditorHtml(revision, applied.draft, applied.group, [research, writer, lead]);
  check("blueprint editor: source and proposed topology are accessible, bounded, and explicit about revision semantics",
    editor.includes("Source · unchanged") && editor.includes("Proposed · page memory") &&
    editor.includes('aria-label="Source blueprint"') && editor.includes('aria-label="Proposed blueprint revision"') &&
    editor.includes("Exact source structure") && editor.includes("Exact proposed structure") &&
    editor.includes("researcher/1.4.0") && editor.includes("does not update durable agents") && !editor.includes("must never enter"));
}

{
  const previousDocument = sandbox.document;
  const previousConfirm = sandbox.confirm;
  const previousFabric = F.store.fabric;
  const previousBlueprints = F.store.fabricBlueprints;
  const previousRevision = F.store.fabricBlueprintRevision;
  let focused = false;
  const nodes = {
    "fabric-blueprints-announcer": { textContent: "" },
    "fabric-blueprint-editor-title": { focus() { focused = true; } },
    "inp-fabric-blueprint-name": {},
  };
  sandbox.document = { getElementById(id) { return nodes[id] || null; } };
  F.store.fabric = null;
  F.store.fabricBlueprintRevision = { sourceId: blueprintFixture.blueprint_id };
  F.fabricSaveCurrentBlueprint();
  check("blueprint editor: toolbar save is actively blocked while the reviewed revision owns the composer",
    focused && nodes["fabric-blueprints-announcer"].textContent.includes("fresh acknowledgement"));

  const openRevision = { sourceId: blueprintFixture.blueprint_id, name: "Edited in progress" };
  let confirmations = 0;
  sandbox.confirm = () => { confirmations += 1; return false; };
  F.store.fabric = {};
  F.store.fabricBlueprints = [structuredClone(blueprintFixture)];
  F.store.fabricBlueprintRevision = openRevision;
  check("blueprint editor: reopening the same source asks before replacing newer draft work",
    !F.fabricBeginBlueprintRevision(blueprintFixture.blueprint_id) && confirmations === 1 &&
    F.store.fabricBlueprintRevision === openRevision);
  sandbox.document = previousDocument;
  sandbox.confirm = previousConfirm;
  F.store.fabric = previousFabric;
  F.store.fabricBlueprints = previousBlueprints;
  F.store.fabricBlueprintRevision = previousRevision;
}

{
  fakeLocalStorage.clear();
  fakeLocalStorage.failReads = 0;
  fakeLocalStorage.failWrites = 0;
  F.store.conn = { baseUrl: "http://blueprints.test", apiKey: "tenant-a" };
  F.store.fabricBlueprints = [structuredClone(blueprintFixture)];
  check("blueprints: a bounded current-scope structure persists", F.saveFabricBlueprints());
  F.store.fabricBlueprints = [];
  F.loadFabricBlueprints();
  check("blueprints: reload restores the exact current connection scope", F.store.fabricBlueprints.length === 1 && F.store.fabricBlueprints[0].blueprint_id === blueprintFixture.blueprint_id);
  F.store.conn = { baseUrl: "http://blueprints.test", apiKey: "tenant-b" };
  F.loadFabricBlueprints();
  check("blueprints: another tenant never inherits saved role bindings", F.store.fabricBlueprints.length === 0);
  F.store.fabricBlueprints = [structuredClone(blueprintFixture)];
  fakeLocalStorage.failWrites = 2;
  check("blueprints: repeated quota failure remains session-only without losing the live structure", !F.saveFabricBlueprints() && F.store.fabricBlueprints.length === 1);
  fakeLocalStorage.failReads = 1;
  F.loadFabricBlueprints();
  check("blueprints: blocked storage reads fail closed with a visible persistence warning", F.store.fabricBlueprints.length === 0 && F.store.fabricBlueprintPersistenceWarning);
  fakeLocalStorage.failReads = 0;
  fakeLocalStorage.failWrites = 0;
}

{
  F.store.conn = { baseUrl: "http://blueprints.test", apiKey: "tenant-limit" };
  F.store.fabricBlueprintPersistenceWarning = false;
  F.store.fabricBlueprints = Array.from({ length: F.FABRIC_BLUEPRINT_LIMIT }, (_, index) => ({
    ...structuredClone(blueprintFixture), blueprint_id: `bp-retained-${String(index).padStart(3, "0")}`,
    name: `Retained ${index}`,
  }));
  const beforeIds = F.store.fabricBlueprints.map((item) => item.blueprint_id);
  const overflow = F.fabricRememberBlueprint({ ...structuredClone(blueprintFixture), blueprint_id: "bp-overflow-001", name: "Overflow" });
  check("blueprints: the authored-item limit refuses a new save instead of silently evicting an older blueprint",
    !overflow.ok && overflow.error.includes("Delete one") &&
    JSON.stringify(F.store.fabricBlueprints.map((item) => item.blueprint_id)) === JSON.stringify(beforeIds));
}

{
  const many = Array.from({ length: F.FABRIC_BLUEPRINT_LIMIT + 5 }, (_, index) => ({
    ...structuredClone(blueprintFixture), blueprint_id: `bp-bounded-${String(index).padStart(3, "0")}`,
    name: `Bounded ${index}`,
  }));
  const envelope = F.fabricEmptyBlueprintEnvelope();
  for (let scope = 0; scope < F.FABRIC_BLUEPRINT_SCOPE_LIMIT + 3; scope++) {
    envelope.scopes[`scope-${scope}`] = { touched_at: scope, blueprints: many };
  }
  const bounded = F.fabricPruneBlueprintEnvelope(envelope, "scope-0", 8192);
  const total = Object.values(bounded.scopes).reduce((count, entry) => count + entry.blueprints.length, 0);
  check("blueprints: scope, per-scope, global, and UTF-8 byte ceilings remain hard bounds",
    Object.keys(bounded.scopes).length <= F.FABRIC_BLUEPRINT_SCOPE_LIMIT &&
    Object.values(bounded.scopes).every((entry) => entry.blueprints.length <= F.FABRIC_BLUEPRINT_LIMIT) &&
    total <= F.FABRIC_BLUEPRINT_GLOBAL_LIMIT && Buffer.byteLength(JSON.stringify(bounded), "utf8") <= 8192);
  const card = F.fabricBlueprintCardHtml(blueprintFixture, [research, writer, lead]);
  const otherCard = F.fabricBlueprintCardHtml({ ...structuredClone(blueprintFixture), blueprint_id: "bp-second-cell-01", name: "Second cell" },
    [research, writer, lead]);
  const spacedIdCard = F.fabricBlueprintCardHtml({ ...structuredClone(blueprintFixture), blueprint_id: "bp-insight review-01" },
    [research, writer, lead]);
  const loneSurrogateId = "bp-\ud800abcd";
  const loneSurrogateCard = F.fabricBlueprintCardHtml({ ...structuredClone(blueprintFixture), blueprint_id: loneSurrogateId },
    [research, writer, lead]);
  const fixtureTitleId = F.fabricBlueprintDomId(blueprintFixture.blueprint_id);
  check("blueprints: the topology score is standalone accessible evidence with separate native actions",
    card.includes('role="img"') && card.includes('aria-label="Insight review cell: 2 roles') &&
    card.includes("Use blueprint") && card.includes("Revise") && card.includes("Export") && card.includes("Delete") &&
    !card.match(/<button[^>]*>[\s\S]*role="img"[\s\S]*<\/button>/));
  check("blueprints: each card and repeated action names expose the blueprint they operate on",
    card.includes(`aria-labelledby="${fixtureTitleId}"`) && card.includes(`id="${fixtureTitleId}"`) &&
    card.includes('aria-label="Use blueprint Insight review cell"') &&
    card.includes('aria-label="Revise Insight review cell"') &&
    card.includes('aria-label="Export Insight review cell"') && card.includes('aria-label="Delete Insight review cell"') &&
    otherCard.includes('aria-label="Use blueprint Second cell"') && !otherCard.includes('aria-label="Use blueprint Insight review cell"'));
  check("blueprints: opaque whitespace in a legal blueprint ID cannot split the card accessible-name reference",
    spacedIdCard.includes(`aria-labelledby="${F.fabricBlueprintDomId("bp-insight review-01")}"`) &&
    spacedIdCard.includes(`id="${F.fabricBlueprintDomId("bp-insight review-01")}"`));
  check("blueprints: a lone-surrogate ID uses a total DOM encoder and cannot break persistent shelf rendering",
    loneSurrogateCard.includes(`aria-labelledby="${F.fabricBlueprintDomId(loneSurrogateId)}"`) &&
    loneSurrogateCard.includes(`id="${F.fabricBlueprintDomId(loneSurrogateId)}"`));
  const failFast = structuredClone(blueprintFixture);
  failFast.fan_out.on_member_failure = "fail_fast";
  const quorumMajority = { ...structuredClone(blueprintFixture), pattern: "quorum", fan_out: undefined,
    quorum: { threshold: 2, resolver: "majority_equal" } };
  const quorumFirst = structuredClone(quorumMajority);
  quorumFirst.quorum.resolver = "first_k";
  const sixRoleScore = { ...structuredClone(blueprintFixture), roles: Array.from({ length: 6 }, (_, index) => ({
    agent_id: `agent-${index}`, member: `member-${index}`, manifest_version: `agent/${index}`, kind: `kind-${index}`, effect: "read_only",
  })) };
  check("blueprints: topology accessible names distinguish every fan-out and quorum convergence policy",
    F.fabricBlueprintScoreHtml(blueprintFixture).includes("failure policy partial") &&
    F.fabricBlueprintScoreHtml(failFast).includes("failure policy fail fast") &&
    F.fabricBlueprintScoreHtml(quorumMajority).includes("resolver majority equal") &&
    F.fabricBlueprintScoreHtml(quorumFirst).includes("resolver first k"));
  check("blueprints: topology accessible names identify bounded role bindings and disclose omitted roles",
    F.fabricBlueprintScoreHtml(blueprintFixture).includes("research bound to researcher-1; draft bound to writer-1") &&
    F.fabricBlueprintScoreHtml(sixRoleScore).includes("member-4 bound to agent-4; and 1 more roles") &&
    !F.fabricBlueprintScoreHtml(sixRoleScore).includes("member-5 bound to agent-5"));
}

{
  const observedAt = Date.parse("2026-08-09T08:00:00Z");
  const activeRecord = {
    coordination_id: "team-run-active", created_at: "2026-08-09T07:55:00Z",
    contract: { pattern: "race" },
    members: [
      { member: "research", task_id: "default--team-run-active--research", submitted: true },
      { member: "write", task_id: "default--team-run-active--write", submitted: true },
    ],
    settled: false, outcome: null,
  };
  const terminalRecord = {
    ...activeRecord, coordination_id: "team-run-done", settled: true,
    outcome: { pattern: "race", status: "completed", members: [
      { member: "research", task_id: "default--team-run-done--research", settlement: "completed" },
      { member: "write", task_id: "default--team-run-done--write", settlement: "cancelled" },
    ] },
  };
  const active = F.fabricRunFromRecord(activeRecord, "attached", observedAt);
  const terminal = F.fabricRunFromRecord(terminalRecord, "launched", observedAt + 1000);
  check("run desk model: an open durable record stays active without inventing partial settlement",
    active.status === "in_progress" && !active.settled && active.member_count === 2 && active.settled_count === 0);
  check("run desk model: terminal progress comes from the server outcome dispositions",
    terminal.status === "completed" && terminal.settled && terminal.member_count === 2 &&
    terminal.settled_count === 2 && terminal.completed_count === 1 && terminal.cancelled_count === 1);
  const deadMemberRecord = { ...terminalRecord, coordination_id: "team-run-dead",
    outcome: { ...terminalRecord.outcome, status: "completed", members: [
      terminalRecord.outcome.members[0],
      { ...terminalRecord.outcome.members[1], settlement: "dead" },
    ] } };
  const deadMemberRun = F.fabricRunFromRecord(deadMemberRecord, "attached", observedAt + 2000);
  check("run desk model: Rust dead-letter settlement is terminal failure evidence, never missing progress",
    deadMemberRun.settled_count === 2 && deadMemberRun.failed_count === 1 &&
    F.fabricRunStatusLabel(deadMemberRun) === "completed with member failures" &&
    F.fabricRunTone(deadMemberRun) === "attention");
  const malicious = F.fabricNormalizeRunRecord({ ...terminal, completed_count: 2, failed_count: 2,
    cancelled_count: 2, payload: { secret: "never retain" }, member_ids: ["private-agent"] });
  check("run desk model: untrusted counters cannot exceed the settled total",
    malicious.completed_count + malicious.failed_count + malicious.cancelled_count === malicious.settled_count);
  check("run desk privacy: normalization retains metadata only",
    !JSON.stringify(malicious).includes("never retain") && !JSON.stringify(malicious).includes("private-agent") &&
    !Object.prototype.hasOwnProperty.call(malicious, "payload"));
  check("run desk identity: path and control characters fail closed while valid opaque ids survive",
    !F.fabricRunSafeId("../other") && !F.fabricRunSafeId("run\tother") && F.fabricRunSafeId("run:@-._9") === "run:@-._9");

  const launch = F.fabricRunFromPayload({ fan_out: { members: [{}, {}, {}] } }, "launch-1", observedAt);
  check("run desk launch: accepted typed payloads create an immediate bounded active receipt",
    launch.pattern === "fan_out" && launch.member_count === 3 && launch.source === "launched" && !launch.settled &&
    F.fabricRunFromPayload({ unknown: {} }, "launch-2", observedAt) === null);
  const merged = F.fabricMergeRunHistory([active], { ...terminal, coordination_id: active.coordination_id, source: "attached" });
  check("run desk merge: authoritative refresh replaces state but preserves started-here provenance",
    merged.length === 1 && merged[0].settled && merged[0].source === "attached");
  const launchedFirst = F.fabricMergeRunHistory([{ ...active, source: "launched" }],
    { ...terminal, coordination_id: active.coordination_id, source: "attached" });
  check("run desk merge: a launch provenance cannot be downgraded by later attachment",
    launchedFirst[0].source === "launched");

  const scopeA = F.connectionRunScope({ baseUrl: "/api", apiKey: "tenant-secret-a" });
  const scopeB = F.connectionRunScope({ baseUrl: "/api", apiKey: "tenant-secret-b" });
  check("run desk isolation: server and tenant scopes are stable, distinct, and never store clear keys",
    scopeA === F.connectionRunScope({ baseUrl: "/api", apiKey: "tenant-secret-a" }) && scopeA !== scopeB &&
    !scopeA.includes("tenant-secret-a"));
  check("run desk persistence: malformed and future envelopes fail closed",
    Object.keys(F.fabricParseRunEnvelope("not-json").scopes).length === 0 &&
    Object.keys(F.fabricParseRunEnvelope(JSON.stringify({ version: 2, scopes: { unsafe: {} } })).scopes).length === 0);

  fakeLocalStorage.clear();
  fakeLocalStorage.writes = 0;
  fakeLocalStorage.failWrites = 0;
  F.store.conn = { baseUrl: "/api", apiKey: "tenant-secret-a" };
  F.store.fabricRunHistory = [{ ...terminal, source: "launched" }];
  check("run desk persistence: sanitized current-scope metadata round-trips",
    F.saveFabricRunHistory() && localStorageData.get(F.LS.fabricRuns).includes("team-run-done"));
  F.store.fabricRunHistory = [];
  F.loadFabricRunHistory();
  check("run desk persistence: reload restores only the current opaque connection scope",
    F.store.fabricRunHistory.length === 1 && F.store.fabricRunHistory[0].coordination_id === "team-run-done");
  F.store.conn = { baseUrl: "/api", apiKey: "tenant-secret-b" };
  F.loadFabricRunHistory();
  check("run desk persistence: another tenant never inherits the prior tenant's remembered runs",
    F.store.fabricRunHistory.length === 0);
  F.store.fabricRunHistory = [terminal];
  fakeLocalStorage.failWrites = 1;
  const writesBeforeRetry = fakeLocalStorage.writes;
  check("run desk persistence: quota pressure compacts once and retries without breaking the run",
    F.saveFabricRunHistory() && fakeLocalStorage.writes === writesBeforeRetry + 2);
  fakeLocalStorage.failWrites = 2;
  check("run desk persistence: repeated quota failure is contained and reported to the caller",
    !F.saveFabricRunHistory() && F.store.fabricRunPersistenceWarning);
  fakeLocalStorage.failWrites = 0;
  fakeLocalStorage.failReads = 1;
  const sessionOnly = { ...active, coordination_id: "session-only" };
  check("run desk persistence: blocked storage reads never turn a confirmed launch into a thrown failure",
    !F.fabricRememberRun(sessionOnly) && F.store.fabricRunPersistenceWarning &&
    F.store.fabricRunHistory.some((run) => run.coordination_id === "session-only"));
  fakeLocalStorage.failReads = 1;
  F.loadFabricRunHistory();
  check("run desk persistence: blocked reload fails closed with a visible session-persistence warning",
    F.store.fabricRunHistory.length === 0 && F.store.fabricRunPersistenceWarning);
  fakeLocalStorage.failReads = 0;
  F.store.fabricRunHistory = [terminal];
  check("run desk persistence: a later successful write clears the persistence warning",
    F.saveFabricRunHistory() && !F.store.fabricRunPersistenceWarning);

  const manyScopes = { version: 1, scopes: Object.create(null) };
  for (let scope = 0; scope < 12; scope++) {
    manyScopes.scopes[`scope-${scope}`] = { touched_at: scope,
      runs: Array.from({ length: 24 }, (_, index) => ({ ...active,
        coordination_id: `scope-${scope}-run-${index}`, observed_at: new Date(observedAt - index).toISOString() })) };
  }
  const pruned = F.fabricPruneRunEnvelope(manyScopes, "scope-11");
  const scopeCount = Object.keys(pruned.scopes).length;
  const runCount = Object.values(pruned.scopes).reduce((sum, entry) => sum + entry.runs.length, 0);
  check("run desk persistence: scope, per-scope, global, and byte budgets remain hard bounds",
    scopeCount <= F.FABRIC_RUN_SCOPE_LIMIT && runCount <= F.FABRIC_RUN_GLOBAL_LIMIT &&
    Object.values(pruned.scopes).every((entry) => entry.runs.length <= F.FABRIC_RUN_HISTORY_LIMIT) &&
    JSON.stringify(pruned).length <= F.FABRIC_RUN_STORAGE_LIMIT);
  const oversizedScope = `server-${"x".repeat(F.FABRIC_RUN_STORAGE_LIMIT + 100)}`;
  const oversizedEnvelope = { version: 1, scopes: { [oversizedScope]: { touched_at: 1, runs: [terminal] } } };
  const oversizedPruned = F.fabricPruneRunEnvelope(oversizedEnvelope, oversizedScope);
  check("run desk persistence: one oversized current scope is omitted rather than violating the byte ceiling",
    JSON.stringify(oversizedPruned).length <= F.FABRIC_RUN_STORAGE_LIMIT &&
    !Object.prototype.hasOwnProperty.call(oversizedPruned.scopes, oversizedScope));
  F.store.conn = { baseUrl: `http://${"x".repeat(F.FABRIC_RUN_STORAGE_LIMIT + 100)}`, apiKey: "tenant" };
  F.store.fabricRunHistory = [terminal];
  check("run desk persistence: an omitted oversized current scope is reported as session-only",
    !F.saveFabricRunHistory() && F.store.fabricRunPersistenceWarning);
  fakeLocalStorage.clear();
  const partialConnection = { baseUrl: `http://${"s".repeat(24000)}`, apiKey: "tenant" };
  const partialScope = F.connectionRunScope(partialConnection);
  F.store.conn = partialConnection;
  F.store.fabricRunHistory = Array.from({ length: F.FABRIC_RUN_HISTORY_LIMIT }, (_, index) => ({
    ...active,
    coordination_id: `partial-${index}-${"y".repeat(180)}`,
    observed_at: new Date(observedAt - index).toISOString(),
  }));
  fakeLocalStorage.failWrites = 1;
  const partialSaved = F.saveFabricRunHistory();
  const partialEnvelope = F.fabricParseRunEnvelope(localStorageData.get(F.LS.fabricRuns));
  const partialCount = partialEnvelope.scopes[partialScope]?.runs.length || 0;
  check("run desk persistence: half-budget compaction reports session-only when any visible rows are omitted",
    !partialSaved && F.store.fabricRunPersistenceWarning && partialCount > 0 &&
    partialCount < F.FABRIC_RUN_HISTORY_LIMIT);
  fakeLocalStorage.failWrites = 0;

  const stale = { ...active, stale: true, error_class: "unavailable" };
  const failedRun = { ...terminal, coordination_id: "team-run-failed", status: "failed" };
  check("run desk filters: lifecycle, attention, and search compose over normalized metadata",
    F.fabricVisibleRuns([active, terminal, stale, failedRun], "race", "active").length === 1 &&
    F.fabricVisibleRuns([active, terminal, stale, failedRun], "refresh unavailable", "attention").length === 1 &&
    F.fabricVisibleRuns([active, terminal, stale, failedRun], "team-run-done", "settled").length === 1);
  const pulse = F.fabricRunPulseHtml(terminal);
  check("run desk pulse rail: exact member settlement is exposed as an accessible progress value",
    pulse.includes('role="progressbar"') && pulse.includes('aria-valuemax="2"') &&
    pulse.includes('aria-valuenow="2"') && pulse.includes("2 of 2 members settled"));
  check("run desk pulse rail: absent member evidence is labelled unknown rather than zero-percent complete",
    F.fabricRunPulseHtml({ ...active, member_count: 0 }).includes('role="img"') &&
    F.fabricRunPulseHtml({ ...active, member_count: 0 }).includes("Member progress unavailable"));
  const escapedDesk = F.fabricRunDeskHtml([{ ...active, coordination_id: "run-&lt;script&gt;" }], "", "all", "", observedAt);
  check("run desk rendering: rows are native controls and hostile identifiers remain escaped",
    escapedDesk.startsWith('<button type="button"') && !escapedDesk.includes("<script>") &&
    escapedDesk.includes("browser") === false);
  check("run desk accessibility: native row buttons receive settlement text through an explicit description",
    escapedDesk.includes('aria-describedby="fabric-run-progress-0"') &&
    escapedDesk.includes('id="fabric-run-progress-0">0 of 2 members settled') &&
    escapedDesk.includes('class="fabric-run-pulse active" aria-hidden="true"'));
  check("run desk follow: only one visible active inspection polls with bounded exponential backoff",
    F.fabricFollowDelay("fabric", true, "visible", { record: activeRecord }, 0) === F.FABRIC_FOLLOW_INTERVAL &&
    F.fabricFollowDelay("fabric", true, "visible", { record: activeRecord }, 99) === F.FABRIC_FOLLOW_MAX_INTERVAL &&
    F.fabricFollowDelay("fabric", true, "hidden", { record: activeRecord }, 0) === null &&
    F.fabricFollowDelay("fabric", true, "visible", { record: terminalRecord }, 0) === null);
  check("run desk follow: stale, paused, and terminal states preserve explicit operator truth",
    F.fabricCoordinationFollowHtml({ record: activeRecord, followError: { status: 503 } }, true, 2).includes("last observed evidence") &&
    F.fabricCoordinationFollowHtml({ record: activeRecord }, false).includes("Live follow paused") &&
    F.fabricCoordinationFollowHtml({ record: terminalRecord }, true).includes("Live follow stopped"));
}

{
  check("compatibility: route-less older server is explained as a capability gap",
    F.fabricErrorHtml(404, { raw: "not found" }, "Durable inventory").includes("needs an Agent Fabric server"));
  check("errors: unknown coordination and invalid identifiers have distinct recovery copy",
    F.fabricErrorHtml(404, { message: "coordination missing" }, "Coordination").includes("was not found") &&
    F.fabricErrorHtml(400, { message: "invalid id" }, "Coordination").includes("not valid"));
  check("run desk errors: only structured coordination-missing 404s become not-found evidence",
    F.fabricRunErrorClass({ status: 404, body: { message: "coordination missing" } }) === "not_found" &&
    F.fabricRunErrorClass({ status: 404, body: { raw: "route not found" } }) === "unavailable" &&
    F.fabricRunErrorClass({ status: 503, body: { message: "down" } }) === "unavailable");
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
  let focusedRun = null;
  const fakeRun = {
    getAttribute(name) { return name === "data-fabric-run" ? "coord-a" : null; },
    focus(options) { focusedRun = options; },
  };
  const fakeRunContainer = { querySelectorAll(selector) { return selector === "[data-fabric-run]" ? [fakeRun] : []; } };
  sandbox.document = { getElementById(id) { return id === "fabric-run-list" ? fakeRunContainer : null; } };
  const runIdentity = F.fabricFocusIdentity(fakeRun);
  F.fabricRestoreFocus(runIdentity);
  check("focus continuity: a replaced Team Run Desk row regains focus after live reconciliation",
    runIdentity.kind === "run" && runIdentity.value === "coord-a" && focusedRun.preventScroll);
  let fallbackFocused = false;
  sandbox.document = { getElementById(id) {
    if (id === "fabric-run-list") return { querySelectorAll() { return []; } };
    if (id === "inp-fabric-run-search") return { focus(options) { fallbackFocused = options.preventScroll; } };
    return null;
  } };
  F.fabricRestoreFocus({ kind: "run", value: "settled-and-filtered" });
  check("focus continuity: a run leaving the active filter moves focus to stable Run Desk search",
    fallbackFocused);
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
check("run desk markup: browser recall and server truth are explicitly distinguished",
  html.includes('id="fabric-run-desk-title">Team run desk') &&
  html.includes("Every refresh asks Rusty for current state") &&
  html.includes("remembered list is not server-side discovery") &&
  html.includes("browser recall · server truth"));
check("run desk accessibility: search, lifecycle, follow, refresh, list, and quiet status surfaces are labelled",
  html.includes('id="inp-fabric-run-search" type="search"') &&
  html.includes('id="sel-fabric-run-state"') && html.includes('id="chk-fabric-follow"') &&
  html.includes('id="btn-fabric-runs-refresh" type="button"') &&
  html.includes('id="fabric-run-desk" aria-labelledby="fabric-run-desk-title" aria-busy="false"') &&
  html.includes('$("fabric-run-desk").setAttribute("aria-busy"') &&
  html.includes('id="fabric-run-list"') && !html.includes('id="fabric-run-list" aria-live=') &&
  html.includes('id="fabric-run-announcer" role="status" aria-live="polite"'));
check("blueprint markup: reusable topology is a labelled Studio surface with explicit privacy boundaries",
  html.includes('class="card fabric-blueprints" aria-labelledby="fabric-blueprints-title"') &&
  html.includes('id="fabric-blueprints-title">Team blueprints</h2>') &&
  html.includes("Task instructions, deadlines, coordination IDs, parents, results, and receipts are never saved") &&
  html.includes('id="fabric-blueprints-status" role="status" aria-live="polite"') &&
  html.includes('id="fabric-blueprints-announcer" role="status" aria-live="polite"'));
check("blueprint lifecycle: save, revise, import, export, delete, and exact composer hydration use native controls",
  html.includes('$("btn-fabric-blueprint-save").onclick = fabricSaveCurrentBlueprint;') &&
  html.includes('$("btn-fabric-blueprint-import").onclick') &&
  html.includes('$("inp-fabric-blueprint-import").addEventListener("change"') &&
  html.includes('$("fabric-blueprints-list").addEventListener("click"') &&
  html.includes("fabricUseBlueprint(load.getAttribute") && html.includes("fabricBeginBlueprintRevision(revise.getAttribute") &&
  html.includes("fabricExportBlueprint(exportButton.getAttribute") &&
  html.includes("fabricDeleteBlueprint(remove.getAttribute") &&
  html.includes("draft.submitting || draft.completed || draft.errorAmbiguous"));
check("blueprint isolation: connection changes clear old bindings before loading the new opaque scope",
  html.includes("store.fabricBlueprints = [];") && html.includes("store.fabricBlueprintRevision = null;") &&
  html.includes("store.fabricBlueprintRenderKey = \"\";") &&
  html.includes("loadFabricBlueprints();") && html.includes("connectionRunScope(store.conn)"));
check("blueprint editor lifecycle: one delegated form creates a reviewed new ID and keeps the source immutable",
  html.includes('id="fabric-blueprint-editor" hidden') &&
  html.includes('$("fabric-blueprint-editor").addEventListener("submit"') &&
  html.includes("fabricSaveBlueprintRevision();") &&
  html.includes("source: structuredClone(blueprint)") &&
  html.includes("saved as a separate blueprint revision"));
check("blueprint editor lifecycle: toolbar saves cannot bypass review and reopening any source requires confirmation",
  html.includes("save.disabled = Boolean(store.fabricBlueprintRevision)") &&
  html.includes("Finish or cancel the open revision before saving another blueprint") &&
  html.includes("Discard the open blueprint revision and start again from this source?") &&
  !html.includes("store.fabricBlueprintRevision.sourceId !== blueprintId"));
check("blueprint editor accessibility: source, candidate, change list, boundary, and fresh acknowledgement stay named",
  html.includes('aria-label="Source blueprint"') && html.includes('aria-label="Proposed blueprint revision"') &&
  html.includes('aria-label="Reviewed topology changes"') &&
  html.includes('id="fabric-blueprint-revision-boundary"') &&
  html.includes("data-blueprint-revision-ack"));
check("blueprint editor lifecycle: visible copy distinguishes the page-memory draft from bounded saved storage",
  html.includes("The open draft lives only in this page and is lost on reload or connection change") &&
  html.includes("a saved revision follows the bounded browser-scoped blueprint storage"));
check("blueprint focus: list replacement restores the exact action or falls back to the stable name field",
  html.includes('element.getAttribute("data-fabric-blueprint-focus")') &&
  html.includes('blueprint ? "fabric-blueprints-list"') &&
  html.includes('if (blueprint && (!restored || restored.disabled)) $("inp-fabric-blueprint-name")'));
check("blueprint drift rendering: manifest versions, accepted kinds, team membership, and scopes invalidate the card snapshot",
  html.includes("fabricAcceptedKinds(agent),") &&
  html.includes("Array.isArray(fabricObject(agent.manifest).scopes) ? agent.manifest.scopes.map(String) : []"));
check("accessibility: team rerenders stay quiet while concise selected-state changes use a dedicated announcer",
  html.includes('id="fabric-team-announcer" role="status" aria-live="polite"') &&
  html.includes('<section class="card fabric-team-card" id="fabric-team">') &&
  !html.includes('id="fabric-team" aria-live='));
check("markup: the observatory exposes all four runtime-backed coordination patterns",
  html.includes('id="fabric-compose-title" tabindex="-1">Coordinate this team') &&
  html.includes('data-compose-pattern="delegate"') && html.includes('data-compose-pattern="fan_out"') &&
  html.includes('data-compose-pattern="race"') && html.includes('data-compose-pattern="quorum"') &&
  !html.match(/id="fabric-[^"]*"[^>]*>[^<]*(restart|cancel)/i));
check("composer lifecycle: delegated event wiring, generation guards, edit invalidation, and result investigation are explicit",
  html.includes('$("fabric-compose-body").addEventListener("submit"') &&
  html.includes('fabricRequestCurrent(generation, connection, store.fabricRequest, store.conn)') &&
  html.includes('if (field !== "acknowledge") fabricComposerResetApproval(draft, field === "coordinationId");') &&
  html.includes('await fabricLoadCoordination(draft.coordinationId, {'));
check("run desk lifecycle: accepted launches are recalled before evidence loading and every operator control is wired",
  html.includes("if (launchedRun) fabricRememberRun(launchedRun);") &&
  html.includes('$("inp-fabric-run-search").addEventListener("input"') &&
  html.includes('$("sel-fabric-run-state").addEventListener("change"') &&
  html.includes('$("btn-fabric-runs-refresh").onclick = fabricRefreshRememberedRuns;') &&
  html.includes('$("chk-fabric-follow").addEventListener("change"') &&
  html.includes('$("fabric-run-list").addEventListener("click"') &&
  html.includes('document.addEventListener("visibilitychange"'));
check("run desk networking: recall refresh and live follow remain bounded and connection-generation guarded",
  html.includes("const targets = store.fabricRunHistory.slice(0, FABRIC_RUN_REFRESH_LIMIT);") &&
  html.includes("const scheduler = fabricCreateScheduler(FABRIC_RUN_REFRESH_CONCURRENCY);") &&
  html.includes("fabricRequestCurrent(request, connection, store.fabricRunRequest, store.conn)") &&
  html.includes("connectionIdentityChanged(connection, store.conn)"));
check("composer lifecycle: launch and ambiguous failure fully rerender to lock every editor and refresh control",
  html.includes('if (refresh) refresh.disabled = Boolean(state.loading || draft.submitting);') &&
  html.includes('draft.submitting = true;') &&
  html.includes('draft.notice = null;\n  fabricRenderComposer(true);') &&
  html.includes('draft.errorAmbiguous = ![400, 404, 409, 422].includes(Number(error.status));') &&
  html.includes(': null;\n    fabricRenderComposer(true);'));
check("composer lifecycle: registry refresh preserves only exact ambiguous approval and rerenders all evidence",
  html.includes("acknowledge: Boolean(previous.errorAmbiguous && previous.attemptedPayload), needsRender: true") &&
  html.includes("store.fabric.composer.needsRender = true;") &&
  html.includes("Team composition needs a loaded durable-agent registry."));
check("composer focus: blocked team navigation and Compose another return focus to the retained or new draft",
  html.includes('fabricFocusData($("fabric-groups"), "data-fabric-group", state.selectedGroup);') &&
  html.includes('fabricFocusData($("fabric-compose-body"), "data-fabric-focus", "compose-id");') &&
  html.includes('if (fabricSelectGroup(next, false)) fabricFocusData'));
check("composer accessibility: frequent preflight changes stay quiet while submissions use one dedicated announcer",
  html.includes('id="fabric-compose-announcer" role="status" aria-live="polite"') &&
  html.includes('id="fabric-compose-review"') && !html.includes('id="fabric-compose-review" aria-live=') &&
  html.includes('id="fabric-compose-error" role="alert" tabindex="-1"') &&
  html.includes('$("fabric-compose-title")?.focus({ preventScroll: true })'));
check("responsive: team layout and causal evidence stack at narrow widths",
  html.includes(".fabric-members { grid-template-columns: 1fr; }") &&
  html.includes(".fabric-coordination-head { flex-direction: column; }") &&
  html.includes(".fabric-trace-event { grid-template-columns: 1fr;") &&
  html.includes(".fabric-compose-grid { grid-template-columns: 1fr;") &&
  html.includes(".fabric-decision-braid { grid-template-columns: 1fr;"));
check("run desk responsive: toolbar, rows, and the pulse rail collapse without hiding progress",
  html.includes(".fabric-run-toolbar { grid-template-columns: 1fr;") &&
  html.includes(".fabric-run-row { grid-template-columns: 1fr auto;") &&
  html.includes(".fabric-run-pulse { grid-column: 1 / -1;") &&
  html.includes(".fabric-run-state { display: grid;") &&
  !html.includes(".fabric-run-state { display: none;"));
check("blueprint responsive: toolbar, topology cards, and actions stack without hiding readiness",
  html.includes(".fabric-blueprints-toolbar { grid-template-columns: 1fr;") &&
  html.includes(".fabric-blueprint-card { grid-template-columns: 1fr;") &&
  html.includes(".fabric-blueprint-score { grid-template-columns:") &&
  html.includes(".fabric-blueprint-card-actions { grid-column: auto;") &&
  html.includes(".fabric-blueprint-compare { grid-template-columns: 1fr;") &&
  html.includes(".fabric-blueprint-revision-arrow { min-height: 18px; transform: rotate(90deg);"));
check("accessibility: essential TeamTrace sequence and depth use the AA text token",
  html.includes(".fabric-trace-event small { color: var(--text-dim);") &&
  !html.includes(".fabric-trace-event small { color: var(--text-faint);"));

if (failed) {
  console.error(`\nFAIL: ${failed} failed, ${passed} passed`);
  process.exit(1);
}
console.log(`\nPASS: ${passed} Agent Fabric / TeamTrace assertions`);
