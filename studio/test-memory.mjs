#!/usr/bin/env node
/* Node unit tests for the governed-memory helpers embedded in index.html.
 * The final browser bootstrap is stripped and pure wire-to-view functions
 * are exercised dependency-free under vm.
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
globalThis.__memory = {
  MEMORY_RENDER_LIMIT, MEMORY_SNAPSHOT_LIMIT, MEMORY_CONFLICT_RENDER_LIMIT,
  memoryRequestCurrent, memoryContentValue, memoryAuthorLabel, memoryPlainPreview,
  memoryBoundedProjection, memoryJsonText, memoryEvidenceId, memorySupersededIds, memoryIntersectIds,
  memoryContext, memoryRecordState, memoryStateLabel, memoryStateHtml,
  memoryScopeLabel, memoryConfidenceLabel, memorySearchText, memoryFilterRecords,
  memoryBuildSearchIndex, memoryBoundedSnapshot, memoryRecordRowHtml,
  memoryRecordAriaLabel, memoryNavigationTarget, memoryNextLifecycleDelay,
  memoryEvidenceSummary, memoryDetailHtml,
  memorySummaryHtml, memoryConflictsHtml, memoryErrorHtml,
};`, sandbox, { filename: "index.html<script>" });

const M = sandbox.__memory;
const now = "2026-08-09T06:00:00Z";
const base = {
  memory_id: "mem-active-0001",
  kind: "preference",
  scope: { scope: "user", id: "user-7" },
  key: "response_tone",
  tags: ["support", "voice"],
  priority: 4,
  provenance: {
    author: { type: "human", human_id: "amjad" },
    evidence: { correction_id: "correction-42", run_id: "run-evidence-42", event_ids: ["event-1"] },
    written_at: "2026-08-08T10:00:00Z",
  },
  confidence: 1,
  validity: { valid_from: "2026-08-08T10:00:00Z" },
  created_at: "2026-08-08T10:00:00Z",
  content: { kind: "inline", value: { tone: "concise <trusted>" } },
};
const candidate = {
  ...base,
  memory_id: "mem-candidate-0002",
  kind: "fact",
  scope: { scope: "agent", id: "researcher-7" },
  key: "market",
  candidacy: "pending",
  provenance: {
    author: { type: "distiller", name: "correction-loop" },
    evidence: { candidate_id: "candidate-99", source_memory_ids: [base.memory_id] },
    written_at: "2026-08-08T11:00:00Z",
  },
  confidence: 0.82,
  content: { kind: "inline", value: "enterprise" },
};
const expired = {
  ...base,
  memory_id: "mem-expired-0003",
  key: "old_timezone",
  expires_at: "2026-08-01T00:00:00Z",
  provenance: { ...base.provenance, author: { type: "agent", agent_id: "support-1" } },
};
const old = { ...base, memory_id: "mem-old-0004", key: "timezone", content: { kind: "inline", value: "UTC" } };
const replacement = {
  ...base,
  memory_id: "mem-new-0005",
  key: "timezone",
  supersedes: old.memory_id,
  content: { kind: "inline", value: "UTC+4" },
  provenance: { ...base.provenance, author: { type: "system" }, evidence: {} },
};
const conflictPeer = {
  ...base,
  memory_id: "mem-conflict-0006",
  key: base.key,
  confidence: 0.7,
  content: { kind: "inline", value: { tone: "detailed" } },
};
const summarySource = { ...base, memory_id: "mem-source-0007", key: "project_notes" };
const summaryRecord = {
  ...base,
  memory_id: "mem-summary-0008",
  kind: "summary",
  key: "project_summary",
  provenance: { ...base.provenance, evidence: { source_memory_ids: [summarySource.memory_id] } },
  content: { kind: "inline", value: "distilled project context" },
};
const records = [base, candidate, expired, old, replacement, conflictPeer, summarySource, summaryRecord];
const conflicts = [{
  scope: base.scope,
  key: base.key,
  memory_ids: [base.memory_id, conflictPeer.memory_id],
  overlap: { valid_from: "2026-08-08T10:00:00Z" },
}];
const context = M.memoryContext(records, conflicts, now);

let passed = 0, failed = 0;
function check(name, condition, detail) {
  if (condition) { passed++; console.log(`ok   ${name}`); }
  else { failed++; console.log(`FAIL ${name}${detail ? ` — ${detail}` : ""}`); }
}
function eq(name, got, want) {
  check(name, JSON.stringify(got) === JSON.stringify(want),
    `got ${JSON.stringify(got)}, want ${JSON.stringify(want)}`);
}

eq("content: inline payload unwraps", M.memoryContentValue(base), { tone: "concise <trusted>" });
eq("content: artifact payload remains inspectable",
  M.memoryContentValue({ content: { kind: "artifact", sha256: "abc", bytes: 9001 } }),
  { artifact: "abc", bytes: 9001 });
check("content: preview is bounded", M.memoryPlainPreview("x".repeat(300), 60).length === 60);
check("content: detail JSON is bounded and says so",
  M.memoryJsonText({ payload: "x".repeat(1000) }, 240).length < 300 &&
  M.memoryJsonText({ payload: "x".repeat(1000) }, 240).includes("inspection view truncated"));
check("content: cyclic values fail closed without traversing forever",
  M.memoryJsonText(globalThis, 240).includes("circular reference") ||
  M.memoryJsonText(globalThis, 240).includes("inspection view truncated"));
check("content: wide objects stop at the inspection field budget",
  M.memoryJsonText(Object.fromEntries(Array.from({ length: 500 }, (_, index) => [`field-${index}`, index])), 400)
    .includes("inspection view truncated"));
check("evidence: identifiers remain attributable but bounded",
  M.memoryEvidenceId("run-42") === "run-42" && M.memoryEvidenceId("x".repeat(100)).length === 80);

check("author: every frozen provenance author is readable",
  M.memoryAuthorLabel({ type: "human", human_id: "h-1" }) === "human:h-1" &&
  M.memoryAuthorLabel({ type: "agent", agent_id: "a-1" }) === "agent:a-1" &&
  M.memoryAuthorLabel({ type: "distiller", name: "d-1" }) === "distiller:d-1" &&
  M.memoryAuthorLabel({ type: "system" }) === "system");
check("author: future and malformed authors degrade without throwing",
  M.memoryAuthorLabel({ type: "reviewer" }) === "reviewer" && M.memoryAuthorLabel(null) === "unknown author");

{
  let currentRequest = 1;
  let currentConnection = { baseUrl: "http://tenant-a", apiKey: "a" };
  const capturedConnection = { ...currentConnection };
  const delayedA = Promise.resolve().then(() =>
    M.memoryRequestCurrent(1, capturedConnection, currentRequest, currentConnection));
  currentRequest = 2;
  currentConnection = { baseUrl: "http://tenant-b", apiKey: "b" };
  check("isolation: delayed tenant A response is stale after switching to tenant B", !(await delayedA));
  check("isolation: current response is accepted only for its captured connection",
    M.memoryRequestCurrent(2, currentConnection, currentRequest, currentConnection));
}

check("context: supersession is derived from immutable replacement links",
  context.superseded.has(old.memory_id) && !context.superseded.has(replacement.memory_id));
check("context: consolidation summaries supersede every named source like Rust core",
  context.superseded.has(summarySource.memory_id) && !context.superseded.has(summaryRecord.memory_id));
check("context: conflicts index both records", context.conflictMap.get(base.memory_id).length === 1 &&
  context.conflictMap.get(conflictPeer.memory_id).length === 1);
check("lifecycle: active record", M.memoryRecordState(base, context) === "active");
check("lifecycle: pending candidate", M.memoryRecordState(candidate, context) === "candidate");
check("lifecycle: expired by TTL", M.memoryRecordState(expired, context) === "expired");
check("lifecycle: expiry wins over stale candidate metadata",
  M.memoryRecordState({ ...candidate, expires_at: "2026-08-01T00:00:00Z" }, context) === "expired");
check("lifecycle: immutable predecessor is superseded", M.memoryRecordState(old, context) === "superseded");
check("lifecycle: consolidated source is superseded", M.memoryRecordState(summarySource, context) === "superseded");
check("lifecycle: future validity is explicit",
  M.memoryRecordState({ ...base, validity: { valid_from: "2026-08-10T00:00:00Z" } }, context) === "scheduled");
check("lifecycle: ended validity is explicit",
  M.memoryRecordState({ ...base, validity: { valid_from: "2026-08-01T00:00:00Z", valid_until: "2026-08-08T00:00:00Z" } }, context) === "historical");
check("lifecycle: labels stay plain-language", M.memoryStateLabel("candidate") === "pending candidate");
check("lifecycle: nearest future boundary schedules a just-after transition refresh",
  M.memoryNextLifecycleDelay([
    { validity: { valid_from: "2026-08-09T06:00:01Z" } },
    { expires_at: "2026-08-09T06:00:03Z" },
  ], Date.parse(now)) === 1025 && M.memoryNextLifecycleDelay([base], Date.parse(now)) === null);

eq("filters: scope and kind compose",
  M.memoryFilterRecords(records, { scope: "agent", kind: "fact" }, context).map((r) => r.memory_id),
  [candidate.memory_id]);
eq("filters: lifecycle isolates expired records",
  M.memoryFilterRecords(records, { state: "expired" }, context).map((r) => r.memory_id),
  [expired.memory_id]);
check("filters: search spans content, author, scope, tag, key, and id",
  M.memoryFilterRecords(records, { search: "enterprise" }, context)[0].memory_id === candidate.memory_id &&
  M.memoryFilterRecords(records, { search: "correction-loop" }, context)[0].memory_id === candidate.memory_id &&
  M.memoryFilterRecords(records, { search: "support" }, context).some((r) => r.memory_id === base.memory_id) &&
  M.memoryFilterRecords(records, { search: "mem-new" }, context)[0].memory_id === replacement.memory_id);
eq("filters: conflict review restricts the ledger without losing order",
  M.memoryFilterRecords(records, { reviewIds: conflicts[0].memory_ids }, context).map((r) => r.memory_id),
  [base.memory_id, conflictPeer.memory_id]);
check("filters: malformed records are excluded", M.memoryFilterRecords([null, {}, base], {}, context).length === 1);
{
  const indexed = M.memoryBuildSearchIndex(records);
  const indexedContext = M.memoryContext(records, conflicts, now, { searchIndex: indexed });
  check("filters: bounded search index is precomputed once per loaded snapshot",
    indexed.size === records.length &&
    M.memoryFilterRecords(records, { search: "distilled project" }, indexedContext)[0].memory_id === summaryRecord.memory_id);
}
{
  const large = Array.from({ length: M.MEMORY_SNAPSHOT_LIMIT + 2 }, (_, index) => ({
    ...base, memory_id: `large-${String(index).padStart(4, "0")}`, key: `key-${index}`,
  }));
  const farId = large[large.length - 1].memory_id;
  const bounded = M.memoryBoundedSnapshot(large, [{ memory_ids: [large[0].memory_id, farId] }],
    M.MEMORY_SNAPSHOT_LIMIT, M.MEMORY_CONFLICT_RENDER_LIMIT);
  check("rendering: audit snapshot is bounded but retains conflict peers for review",
    bounded.length === M.MEMORY_SNAPSHOT_LIMIT + 1 && bounded.some((record) => record.memory_id === farId));
  const boundedIds = M.memoryIntersectIds(new Set([large[0].memory_id, farId, "outside-snapshot"]), bounded);
  check("rendering: retained supersession state is intersected with the bounded snapshot",
    boundedIds.size === 2 && !boundedIds.has("outside-snapshot"));
}

{
  const row = M.memoryRecordRowHtml(base, context);
  check("row: carries key, kind, scope, confidence, lifecycle, and conflict",
    row.includes("response_tone") && row.includes("preference") && row.includes("user:user-7") &&
    row.includes("100% declared confidence") && row.includes("active") && row.includes("conflict"));
  check("row: content is HTML escaped", !row.includes("<trusted>") && row.includes("&lt;trusted&gt;"));
  check("row: duplicate keys remain distinguishable to assistive technology",
    M.memoryRecordAriaLabel(base, context).includes("response_tone, active now, user:user-7") &&
    M.memoryRecordAriaLabel(base, context).includes("mem-acti…0001"));
}

check("evidence: correction, run, and journal events remain attributable",
  M.memoryEvidenceSummary(base).includes("correction correction-42") &&
  M.memoryEvidenceSummary(base).includes("run run-evidence-42") &&
  M.memoryEvidenceSummary(base).includes("1 journal event(s)"));
check("evidence: direct records say that no derivation IDs exist",
  M.memoryEvidenceSummary(replacement) === "Directly stated; no derivation IDs");

{
  const detail = M.memoryDetailHtml(base, context);
  check("detail: provenance spine connects record, author, and evidence",
    detail.includes('aria-label="Memory provenance"') && detail.includes("Authored by") &&
    detail.includes("human:amjad") && detail.includes("correction correction-42"));
  check("detail: immutable lifecycle fields and raw record remain inspectable",
    detail.includes("Memory ID") && detail.includes("Valid from") && detail.includes("Supersedes") &&
    detail.includes("Raw immutable record"));
  check("detail: conflicts explain non-resolution and link both records",
    detail.includes("does not silently choose a winner") && detail.includes(`data-memory-select="${base.memory_id}"`) &&
    detail.includes(`data-memory-select="${conflictPeer.memory_id}"`));
  check("detail: remembered content is escaped", !detail.includes("<trusted>") && detail.includes("&lt;trusted&gt;"));
}

{
  const summary = M.memorySummaryHtml(records, conflicts, context);
  check("summary: retained, active, candidate, conflict, and scope counts are honest",
    summary.includes(">8</b><span>Retained records") && summary.includes(">4</b><span>Active now") &&
    summary.includes(">1</b><span>Pending candidates") && summary.includes(">1</b><span>Conflicts across 1 scope"));
  check("summary: conflict capability failure is unknown, never a false zero",
    M.memorySummaryHtml(records, null, context).includes("Conflict status unavailable"));
  check("summary: capped ledgers distinguish snapshot counts from retained totals",
    M.memorySummaryHtml(records, conflicts, context, { totalRecords: 4000, snapshotTruncated: true })
      .includes(">4000</b><span>Retained records") &&
    M.memorySummaryHtml(records, conflicts, context, { totalRecords: 4000, snapshotTruncated: true })
      .includes("Active in snapshot"));
  check("summary: bounded conflict state keeps the server total honest",
    M.memorySummaryHtml(records, conflicts, context, { totalConflicts: 900 })
      .includes(">900</b><span>Conflicts · first 1 reviewable"));
}

{
  const inbox = M.memoryConflictsHtml(conflicts, null);
  check("conflict inbox: offers an accessible evidence review action",
    inbox.includes("Conflict inbox") && inbox.includes('data-memory-conflict="0"') &&
    inbox.includes('aria-label="Review conflict for response_tone"'));
  check("conflict inbox: empty state explains structural detection",
    M.memoryConflictsHtml([], null).includes("same key and overlapping validity"));
  check("conflict inbox: partial capability failure preserves an honest note",
    M.memoryConflictsHtml([], "Conflict detection unavailable").includes("Conflict detection unavailable"));
  check("conflict inbox: bounded review discloses the full server total",
    M.memoryConflictsHtml(conflicts, null, 900).includes("first 1 of 900 conflicts"));
}

check("compatibility: route-missing explains the required memory contract",
  M.memoryErrorHtml(404, { raw: "not found" }).includes("POST /memory/query"));
check("compatibility: real server errors are escaped",
  !M.memoryErrorHtml(500, { message: "<script>alert(1)</script>" }).includes("<script>"));
check("rendering: production-shaped lists, snapshots, and conflict inboxes are explicitly bounded",
  M.MEMORY_RENDER_LIMIT === 200 && M.MEMORY_SNAPSHOT_LIMIT === 1000 && M.MEMORY_CONFLICT_RENDER_LIMIT === 50);
check("rendering: runtime stores a bounded conflict slice plus the honest total",
  html.includes("totalConflicts = rawConflicts.length") &&
  html.includes("rawConflicts.slice(0, MEMORY_CONFLICT_RENDER_LIMIT)") &&
  html.includes("memoryIntersectIds(memorySupersededIds(rawRecords), records)"));
eq("keyboard: roving selection supports arrows and boundaries",
  [
    M.memoryNavigationTarget(["a", "b", "c"], "b", "ArrowDown"),
    M.memoryNavigationTarget(["a", "b", "c"], "b", "ArrowUp"),
    M.memoryNavigationTarget(["a", "b", "c"], "b", "Home"),
    M.memoryNavigationTarget(["a", "b", "c"], "b", "End"),
    M.memoryNavigationTarget(["a", "b", "c"], "c", "ArrowDown"),
  ], ["c", "a", "a", "c", "c"]);
check("markup: memory has sidebar entry, labelled workspace, filters, and live status",
  html.includes('id="btn-memory-open"') && html.includes('id="memory-view"') &&
  html.includes('id="memory-title" tabindex="-1"') && html.includes('role="search" aria-label="Filter memory records"') &&
  html.includes('id="memory-statusline" role="status" aria-live="polite"') &&
  html.includes('role="listbox" aria-label="Memory records"') &&
  html.includes("Content search reads the first 2,000 characters per record."));
check("interaction: rerendered memory selection restores a meaningful focus target",
  html.includes("querySelector('[aria-selected=\"true\"]')") &&
  html.includes("selected.focus({ preventScroll: true })"));
check("interaction: memory list uses roving option focus instead of 200 tab stops",
  html.includes('item.setAttribute("role", "option")') &&
  html.includes('item.setAttribute("tabindex", selected ? "0" : "-1")') &&
  html.includes('["ArrowDown", "ArrowUp", "Home", "End"]'));
check("interaction: lifecycle labels schedule a refresh at the next temporal boundary",
  html.includes("memoryScheduleLifecycleRefresh(state.records, nowMs)") &&
  html.includes("if (store.view === \"memory\" && store.memory"));
check("interaction: timed lifecycle rerenders preserve focus inside replaced memory controls",
  html.includes("const focus = memoryCaptureDynamicFocus()") &&
  html.includes("memoryRestoreDynamicFocus(focus)") &&
  html.includes("querySelector('[aria-selected=\"true\"]') || $(\"inp-memory-search\")") &&
  html.includes('$("memory-detail-title") || $("inp-memory-search")'));
check("responsive shell: mobile navigation leaves the workspace in the first viewport",
  html.includes("max-height: 34vh; overflow-y: auto") && html.includes("@media (max-width: 1120px)"));
check("accessibility: small memory metadata uses the higher-contrast dim token",
  !html.match(/\.memory-(?:summary span|toolbar label|scope|proof-step span|kv \.k)[^{]*\{[^}]*text-faint/));
check("accessibility: conflict explanation uses the reviewed AA color",
  html.includes(".memory-conflict-detail p { margin: 4px 0 8px; color: #aaa08f;"));

console.log(`\n${passed} passed, ${failed} failed`);
if (failed) process.exit(1);
