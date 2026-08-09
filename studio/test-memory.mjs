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

const sandbox = { TextDecoder };
vm.createContext(sandbox);
vm.runInContext(src + `
globalThis.__memory = {
  MEMORY_RENDER_LIMIT, MEMORY_SNAPSHOT_LIMIT, MEMORY_CONFLICT_RENDER_LIMIT,
  MEMORY_CORRECTION_TEXT_LIMIT, MEMORY_GOVERNANCE_TAG_LIMIT, MEMORY_CONSOLIDATION_SOURCE_LIMIT,
  MEMORY_GOVERNANCE_RECEIPT_LIMIT, MEMORY_ASSEMBLY_RESULT_LIMIT, MEMORY_ASSEMBLY_RENDER_LIMIT,
  MEMORY_ASSEMBLY_RESPONSE_BYTES,
  memoryRequestCurrent, memoryContentValue, memoryAuthorLabel, memoryPlainPreview,
  memoryBoundedProjection, memoryJsonText, memoryEvidenceId, memorySupersededIds, memoryIntersectIds,
  memoryContext, memoryRecordState, memoryStateLabel, memoryStateHtml,
  memoryScopeLabel, memoryConfidenceLabel, memorySearchText, memoryFilterRecords,
  memoryBuildSearchIndex, memoryBoundedSnapshot, memoryRecordRowHtml,
  memoryRecordAriaLabel, memoryNavigationTarget, memoryNextLifecycleDelay,
  memoryEvidenceSummary, memoryDetailHtml,
  memoryAssemblyDraft, memoryAssemblyUnsigned, memoryAssemblyTimestamp, memoryAssemblyTags,
  memoryAssemblyValidateDraft, memoryAssemblyRecords, memoryAssemblyValidateResult,
  memoryAssemblyPriority, memoryAssemblyInstantNanos, memoryAssemblyRankComparator, memoryAssemblyRanked, memoryAssemblyCompactJson,
  memoryAssemblyContentBytes, memoryAssemblyEstimatedTokens, memoryAssemblyUsedTokens, memoryAssemblySameRecord,
  memoryAssemblyErrorText, memoryAssemblyHardOverflow, memoryAssemblyRecordHtml, memoryAssemblyResultHtml, memoryAssemblyFormHtml,
  apiResponseText,
  memoryCorrectionGeneratedId, memoryCorrectionWireText, memoryCorrectionMemoryId, memoryCorrectionDraft,
  memoryCorrectionParseValue, memoryCorrectionValidateDraft, memoryCorrectionSameValue,
  memoryCorrectionRecordMatch, memoryCorrectionValidateReceipt, memoryCorrectionFindReconciled,
  memoryCorrectionOutcomeText, memoryCorrectionIsExactRetry, memoryCorrectionShouldUnlockFailure,
  memoryCorrectionFailureFocusId,
  memoryScopeEqual, memoryGovernanceTags, memoryConsolidationDraft, memoryGovernanceValidateDraft,
  memoryGovernancePool,
  memoryConsolidationConflictCurrent, memoryGovernanceIdList, memoryConsolidationValidateReceipt,
  memoryConsolidationSourcesHtml, memoryGovernanceCurrent, memoryGovernanceErrorText,
  memoryGovernanceTaskEvidence,
  memoryConsolidationTaskContract, memoryConsolidationTaskFingerprint,
  memoryConsolidationSummaryCheck, memoryConsolidationOutcome, memoryConsolidationSummaryQuery,
  memoryConsolidationFollowDelay, memoryConsolidationFollowErrorText,
  memoryConsolidationSummaryHtml, memoryConsolidationFollowHtml, memoryConsolidationTaskActionHtml,
  memoryConsolidationFollowCurrent,
  setMemoryGovernanceTestState: (draft, conn) => { store.memoryGovernance = draft; store.conn = conn; },
  setMemoryFollowTestState: (follow, request, conn) => {
    store.memoryFollow = follow; store.memoryFollowRequest = request; store.conn = conn;
  },
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
  check("detail: malformed immutable identities never expose a false correction action",
    !detail.includes(`data-memory-correct="${base.memory_id}"`) && detail.includes("Correction unavailable"));
}

{
  const first = {
    ...base, memory_id: "a".repeat(64), priority: 7, confidence: 0.92,
    created_at: "2026-08-09T05:00:00Z", key: "<preferred-tone>",
  };
  const second = {
    ...base, memory_id: "b".repeat(64), priority: 3, confidence: 0.8,
    created_at: "2026-08-08T05:00:00Z", key: "fallback",
  };
  const draft = {
    ...M.memoryAssemblyDraft(first), scopeType: "agent", scopeId: "researcher-7", kind: "fact",
    key: " exact key ", tagsText: "trusted, current, trusted", minConfidence: "0.75",
    authorType: "human", authorId: "amjad", validAt: "2026-08-09T05:00:00Z",
    asOf: "2026-08-09T06:00:00Z", candidatesOnly: true, includeExpired: true,
    includeSuperseded: true, maxTokens: "4096", marginPercent: "20", overflow: "truncate",
  };
  const checked = M.memoryAssemblyValidateDraft(draft);
  check("assembly query: every supported structural filter maps to the exact server contract",
    checked.valid && checked.query.scope.scope === "agent" && checked.query.scope.id === "researcher-7" &&
    checked.query.kinds[0] === "fact" && checked.query.key === " exact key " &&
    JSON.stringify(checked.query.tags) === JSON.stringify(["trusted", "current"]) &&
    checked.query.min_confidence === 0.75 && checked.query.authored_by.human_id === "amjad" &&
    checked.query.valid_at === draft.validAt && checked.query.as_of === draft.asOf &&
    checked.query.candidates_only && checked.query.include_expired && checked.query.include_superseded &&
    !("run_id" in checked.query));
  eq("assembly budget: exact u32 values and overflow policy are preserved", checked.budget,
    { max_tokens: 4096, margin_percent: 20, overflow: "truncate" });
  check("assembly query: visible key and identity whitespace is never silently normalized",
    checked.query.key === " exact key " && M.memoryAssemblyValidateDraft({ ...draft, scopeId: " tenant " }).query.scope.id === " tenant ");
  check("assembly validation: tag normalization is explicit, bounded, and rejects controls",
    JSON.stringify(M.memoryAssemblyTags("a, b, a").tags) === JSON.stringify(["a", "b"]) &&
    Boolean(M.memoryAssemblyTags("a, bad\ttag").error));
  check("assembly validation: strict RFC 3339 instants reject browser-ambiguous values",
    M.memoryAssemblyTimestamp("2026-08-09T06:00:00Z").value === "2026-08-09T06:00:00Z" &&
    Boolean(M.memoryAssemblyTimestamp("2026-08-09 06:00").error) && Boolean(M.memoryAssemblyTimestamp("1786255200").error) &&
    Boolean(M.memoryAssemblyTimestamp("2026-02-30T06:00:00Z").error) && Boolean(M.memoryAssemblyTimestamp("2026-08-09T25:00:00Z").error));
  const nanosNewer = { ...first, memory_id: "f".repeat(64), created_at: "2026-08-09T05:00:00.000000002Z" };
  const nanosOlder = { ...first, memory_id: "0".repeat(64), created_at: "2026-08-09T05:00:00.000000001Z" };
  check("assembly ranking: RFC 3339 nanoseconds retain Rust's exact recency order",
    M.memoryAssemblyInstantNanos(nanosNewer.created_at) === M.memoryAssemblyInstantNanos(nanosOlder.created_at) + 1n &&
    M.memoryAssemblyRankComparator(nanosNewer, nanosOlder) === -1 && M.memoryAssemblyRanked([nanosNewer, nanosOlder]) &&
    !M.memoryAssemblyRanked([nanosOlder, nanosNewer]));
  check("assembly validation: token accounting accepts the u32 boundary but rejects whitespace and fractions",
    M.memoryAssemblyUnsigned("4294967295", 0, 4294967295) === 4294967295 &&
    M.memoryAssemblyUnsigned(" 20", 0, 4294967295) === null && M.memoryAssemblyUnsigned("20.0", 0, 4294967295) === null);
  check("assembly validation: author identity and confidence are fail-closed",
    !M.memoryAssemblyValidateDraft({ ...draft, authorId: "bad\tid", minConfidence: "1.01" }).valid &&
    M.memoryAssemblyValidateDraft({ ...draft, authorType: "system", authorId: "" }).valid);

  const rankedBody = { records: [first, second] };
  const exactUsed = M.memoryAssemblyUsedTokens([first], 20);
  const assemblyBody = {
    memory_ids: [first.memory_id], records: [first],
    token_accounting: { bytes_per_token: 4, margin_percent: 20, budget_tokens: 100, used_tokens: exactUsed },
    truncated: true,
  };
  const proof = M.memoryAssemblyValidateResult(assemblyBody, rankedBody, { max_tokens: 100, margin_percent: 20 });
  check("assembly proof: exact records, rank comparator, accounting, and separate comparison corroborate",
    proof.ok && proof.result.records[0] === first && proof.result.comparisonFirst === second &&
    proof.result.comparisonOmittedCount === 1 && proof.result.comparisonState === "corroborated");
  check("assembly proof: duplicate IDs and altered accounting fail closed while malformed comparison stays ancillary",
    !M.memoryAssemblyValidateResult({ ...assemblyBody, memory_ids: [second.memory_id] }, rankedBody, { max_tokens: 100, margin_percent: 20 }).ok &&
    M.memoryAssemblyValidateResult(assemblyBody, { records: [second, first] }, { max_tokens: 100, margin_percent: 20 }).result.comparisonState === "unavailable" &&
    !M.memoryAssemblyValidateResult({ ...assemblyBody, token_accounting: { ...assemblyBody.token_accounting, margin_percent: 21 } }, rankedBody, { max_tokens: 100, margin_percent: 20 }).ok &&
    M.memoryAssemblyRecords([first, first]) === null);
  check("assembly proof: Rust's four-byte divisor and exact packed cost reject plausible-looking false accounting",
    !M.memoryAssemblyValidateResult({ ...assemblyBody, token_accounting: { ...assemblyBody.token_accounting, bytes_per_token: 999 } }, rankedBody, { max_tokens: 100, margin_percent: 20 }).ok &&
    !M.memoryAssemblyValidateResult({ ...assemblyBody, token_accounting: { ...assemblyBody.token_accounting, used_tokens: 0 } }, rankedBody, { max_tokens: 100, margin_percent: 20 }).ok);
  const rankChanged = { ...first, priority: 6 };
  const changedComparison = M.memoryAssemblyValidateResult(assemblyBody, { records: [rankChanged, second] }, { max_tokens: 100, margin_percent: 20 });
  check("assembly proof: same content ID with changed rank fields is non-atomic drift, never exact omission evidence",
    changedComparison.ok && changedComparison.result.comparisonState === "changed" && !changedComparison.result.comparisonFirst);
  const misorderedAssembly = {
    memory_ids: [second.memory_id, first.memory_id], records: [second, first],
    token_accounting: { bytes_per_token: 4, margin_percent: 20, budget_tokens: 100,
      used_tokens: M.memoryAssemblyUsedTokens([second, first], 20) }, truncated: false,
  };
  check("assembly proof: two consistently misordered responses cannot impersonate Rust's comparator",
    !M.memoryAssemblyValidateResult(misorderedAssembly, { records: [second, first] }, { max_tokens: 100, margin_percent: 20 }).ok);
  const largeResolved = { ...second, memory_id: "d".repeat(64), content: { kind: "inline", value: { blob: "x".repeat(5000) } } };
  const overlayArtifact = { ...second, content: { kind: "artifact", value: { sha256: "c".repeat(64), bytes: 5000 } } };
  check("assembly accounting: base re-inlining and active-overlay artifact references both follow Rust's byte rule",
    M.memoryAssemblyContentBytes(first) === BigInt(new TextEncoder().encode(JSON.stringify(first.content.value)).length) &&
    M.memoryAssemblyContentBytes(largeResolved) > 4096n && M.memoryAssemblyRecords([largeResolved])?.[0] === largeResolved &&
    M.memoryAssemblyContentBytes(overlayArtifact) === 5000n && M.memoryAssemblyRecords([overlayArtifact])?.[0] === overlayArtifact);
  check("assembly proof: unavailable ancillary comparison never hides an exact assembly",
    M.memoryAssemblyValidateResult(assemblyBody, null, { max_tokens: 100, margin_percent: 20 }).ok &&
    M.memoryAssemblyValidateResult(assemblyBody, null, { max_tokens: 100, margin_percent: 20 }).result.comparisonState === "unavailable");
  check("assembly proof: a non-atomic rank change never invalidates exact included evidence or becomes an omission claim",
    M.memoryAssemblyValidateResult({ ...assemblyBody, truncated: false }, rankedBody, { max_tokens: 100, margin_percent: 20 }).result.comparisonState === "changed");
  const resultHtml = M.memoryAssemblyResultHtml({ result: { ...proof.result, asOf: draft.asOf } });
  check("assembly result: accessible budget rail exposes used, reserve, inclusion, and separate comparison evidence",
    resultHtml.includes('role="meter"') && resultHtml.includes(`aria-valuenow="${exactUsed}"`) &&
    resultHtml.includes("Included evidence") && resultHtml.includes("Observed after the stop") &&
    resultHtml.includes("not an atomic omission receipt"));
  check("assembly result: hostile record labels are escaped in text and accessible names",
    !resultHtml.includes("<preferred-tone>") && resultHtml.includes("&lt;preferred-tone&gt;") &&
    resultHtml.includes(`data-memory-assembly-inspect="${first.memory_id}"`));
  check("assembly result: hard overflow is a valid no-partial-context outcome even without comparison",
    M.memoryAssemblyResultHtml({ result: { hardFailure: true, ranked: [], comparisonState: "unavailable", message: "too large" } })
      .includes("Hard budget held") && M.memoryAssemblyResultHtml({ result: { hardFailure: true, ranked: [], comparisonState: "unavailable", message: "too large" } })
      .includes("exact 422 still proves"));
  check("assembly hard overflow: only the exact structured Rust error is classified as a budget hold",
    M.memoryAssemblyHardOverflow({ status: 422, body: { error: "unprocessable", message: "invalid state update: memory assembly exceeds the context budget: record `x` costs 7 tokens" } }) &&
    !M.memoryAssemblyHardOverflow({ status: 422, body: { error: "unprocessable", message: "memory assembly exceeds the context budget: record `x` costs 7 tokens" } }) &&
    !M.memoryAssemblyHardOverflow({ status: 422, body: { error: "unprocessable", message: "some other validation failed" } }) &&
    !M.memoryAssemblyHardOverflow({ status: 422, body: { raw: "unprocessable" } }));
  const formHtml = M.memoryAssemblyFormHtml(draft);
  check("assembly form: read-only boundary, exact filters, and plain-language overflow are visible",
    formHtml.includes("no journal event") && formHtml.includes("Exact scope ID") && formHtml.includes("Expiry evaluated at") &&
    formHtml.includes("Stop and return the ranked prefix") && formHtml.includes("Fail without returning partial context"));
  check("assembly detail handoff: a valid selected record can prefill the exact scope",
    M.memoryDetailHtml(first, M.memoryContext([first], [], now)).includes(`data-memory-assemble-record="${first.memory_id}"`));
}

{
  const response = (parts, declared = null) => {
    const state = { cancelled: false, released: false, index: 0 };
    const body = {
      cancel: async () => { state.cancelled = true; },
      getReader: () => ({
        read: async () => state.index < parts.length ? { done: false, value: new TextEncoder().encode(parts[state.index++]) } : { done: true },
        cancel: async () => { state.cancelled = true; },
        releaseLock: () => { state.released = true; },
      }),
    };
    return [{ headers: { get: (name) => name === "content-length" ? declared : null }, body }, state];
  };
  const [small, smallState] = response(["{\"records\":", "[]}"]);
  const [streamOverflow, streamState] = response(["1234", "5678"]);
  const [declaredOverflow, declaredState] = response([], "9");
  const smallText = await M.apiResponseText(small, 32);
  let streamError = null, declaredError = null;
  try { await M.apiResponseText(streamOverflow, 7); } catch (error) { streamError = error; }
  try { await M.apiResponseText(declaredOverflow, 8); } catch (error) { declaredError = error; }
  check("assembly response containment: streamed bytes stop at the ceiling before JSON parsing",
    smallText === '{"records":[]}' && smallState.released && streamState.cancelled && streamState.released &&
    streamError?.body?.error === "response_too_large" && declaredState.cancelled && declaredError?.body?.error === "response_too_large");
}

{
  const target = {
    ...base,
    memory_id: "1".repeat(64),
    key: "response_tone",
    content: { kind: "inline", value: { tone: "concise" } },
  };
  const cryptography = { randomUUID: () => "11111111-2222-4333-8444-555555555555" };
  const draft = M.memoryCorrectionDraft(target, cryptography);
  check("correction: secure stable identity is minted once into the draft",
    draft.correctionId === "correction-11111111-2222-4333-8444-555555555555" &&
    M.memoryCorrectionGeneratedId(cryptography) === draft.correctionId);
  check("correction: structured source content opens as exact JSON",
    draft.mode === "json" && JSON.parse(draft.correctedText).tone === "concise" && draft.targetSnapshot === target);
  check("correction: content-addressed targets expose the correction action",
    M.memoryCorrectionMemoryId(target.memory_id) &&
    M.memoryDetailHtml(target, M.memoryContext([target], [], now)).includes(`data-memory-correct="${target.memory_id}"`));
  draft.author = "amjad";
  draft.correctedText = '{"tone":"direct","weight":2}';
  draft.acknowledged = true;
  const validated = M.memoryCorrectionValidateDraft(draft);
  check("correction: valid draft produces the flattened governed wire contract",
    validated.valid && validated.payload.correction_id === draft.correctionId &&
    validated.payload.target.type === "memory" && validated.payload.target.memory_id === target.memory_id &&
    validated.payload.scope.scope === "user" && validated.payload.scope.id === "user-7" &&
    validated.payload.corrected.tone === "direct" && !Object.hasOwn(validated.payload, "rationale"));
  check("correction: wider destination is described as candidacy",
    M.memoryCorrectionOutcomeText({ created: true, candidate: true, reconciled: false }).includes("pending memory candidate"));
  check("correction: outcome names supersession only when the confirmed record proves it",
    M.memoryCorrectionOutcomeText({ created: true, candidate: false, record: { supersedes: target.memory_id } }).includes("replaced same-scope record") &&
    M.memoryCorrectionOutcomeText({ created: true, candidate: false, record: {} }).includes("No same-scope record was replaced"));

  const correctedRecord = {
    memory_id: "2".repeat(64), kind: "fact", scope: validated.payload.scope, key: target.key,
    confidence: 1, candidacy: "pending", supersedes: target.memory_id,
    content: { kind: "inline", value: validated.payload.corrected },
    provenance: {
      author: { type: "human", human_id: validated.payload.author },
      evidence: { correction_id: validated.payload.correction_id },
    },
  };
  const response = {
    correction_id: validated.payload.correction_id,
    attribution: `human:${validated.payload.author} via correction:${validated.payload.correction_id}`,
    candidate: true, memory_id: correctedRecord.memory_id, created: true,
    record: correctedRecord, superseded: target.memory_id, example_id: null,
  };
  check("correction: exact receipt binds identity, attribution, content, scope, candidacy, and supersession",
    M.memoryCorrectionValidateReceipt(response, validated.payload, target).ok);
  check("correction: malformed successful receipt fails closed",
    !M.memoryCorrectionValidateReceipt({ ...response, attribution: "human:someone-else" }, validated.payload, target).ok &&
    !M.memoryCorrectionValidateReceipt({ ...response, record: { ...correctedRecord, content: { kind: "inline", value: "wrong" } } }, validated.payload, target).ok);
  const unkeyedTarget = { ...target, memory_id: "3".repeat(64) };
  delete unkeyedTarget.key;
  const unkeyedRecord = { ...correctedRecord, memory_id: "4".repeat(64) };
  delete unkeyedRecord.key;
  delete unkeyedRecord.supersedes;
  check("correction: receipt matching preserves exact unkeyed shape and numeric confidence",
    M.memoryCorrectionRecordMatch(unkeyedRecord, validated.payload, unkeyedTarget) &&
    !M.memoryCorrectionRecordMatch({ ...unkeyedRecord, key: "unexpected" }, validated.payload, unkeyedTarget) &&
    !M.memoryCorrectionRecordMatch({ ...unkeyedRecord, confidence: "1" }, validated.payload, unkeyedTarget));
  check("correction: supersession evidence is absent or a content address, never a truthy shortcut",
    !M.memoryCorrectionRecordMatch({ ...unkeyedRecord, supersedes: "" }, validated.payload, unkeyedTarget) &&
    !M.memoryCorrectionRecordMatch({ ...unkeyedRecord, supersedes: null }, validated.payload, unkeyedTarget) &&
    !M.memoryCorrectionRecordMatch({ ...unkeyedRecord, supersedes: "not-a-memory" }, validated.payload, unkeyedTarget));
  check("correction: hostile legal object keys remain exact without mutating canonical prototypes",
    M.memoryCorrectionSameValue(JSON.parse('{"__proto__":{"approved":true}}'), JSON.parse('{"__proto__":{"approved":true}}')) &&
    !M.memoryCorrectionSameValue(JSON.parse('{"__proto__":{"approved":true}}'), JSON.parse('{"__proto__":{"approved":false}}')));
  check("correction: uncertain response reconciles by correction provenance and exact payload",
    M.memoryCorrectionFindReconciled([correctedRecord], validated.payload, target).status === "confirmed");
  check("correction: reused identity with different content is a collision, never an idempotent retry",
    M.memoryCorrectionFindReconciled([{ ...correctedRecord, content: { kind: "inline", value: "other" } }], validated.payload, target).status === "collision");

  const runPayload = { ...validated.payload, scope: { scope: "run", id: "run-42" } };
  const runRecord = { ...correctedRecord, scope: runPayload.scope };
  delete runRecord.candidacy;
  check("correction: run-scoped receipt requires adopted memory without candidacy",
    M.memoryCorrectionRecordMatch(runRecord, runPayload, target) &&
    !M.memoryCorrectionRecordMatch({ ...runRecord, candidacy: "pending" }, runPayload, target) &&
    !M.memoryCorrectionRecordMatch({ ...runRecord, candidacy: false }, runPayload, target) &&
    !M.memoryCorrectionRecordMatch({ ...runRecord, candidacy: null }, runPayload, target));
  check("correction: run-scoped drafts bind the same finalized run as the write journal",
    M.memoryCorrectionValidateDraft({ ...draft, scopeType: "run", scopeId: "run-42" }).payload.run_id === "run-42");
}

{
  const blank = { correctionId: "correction-1", targetId: "1".repeat(64), author: "", scopeType: "user",
    scopeId: "user-7", mode: "text", correctedText: "fixed", acknowledged: false };
  const result = M.memoryCorrectionValidateDraft(blank);
  check("correction validation: attribution and deliberate acknowledgement are mandatory",
    !result.valid && result.errors.author && result.errors.acknowledged);
  check("correction validation: content is UTF-8 byte bounded",
    M.memoryCorrectionParseValue("text", "😀".repeat(9000)).error.includes("32 KiB"));
  check("correction validation: lossy browser numbers and negative zero fail closed",
    M.memoryCorrectionParseValue("json", '{"amount":9007199254740993}').error.includes("browser-safe") &&
    M.memoryCorrectionParseValue("json", '{"amount":-0}').error.includes("negative zero"));
  check("correction validation: empty text is deliberate only through JSON",
    Boolean(M.memoryCorrectionParseValue("text", "").error) &&
    M.memoryCorrectionParseValue("json", '""').error === "");
  check("correction validation: control characters cannot enter human or scope identities",
    !M.memoryCorrectionWireText("amjad\noperator") && !M.memoryCorrectionWireText("tenant\u0000a"));
  check("correction retry: frozen ambiguous payload is recognized exactly",
    M.memoryCorrectionIsExactRetry({ locked: true, attemptedPayload: { correction_id: "correction-1" } }) &&
    !M.memoryCorrectionIsExactRetry({ locked: false, attemptedPayload: { correction_id: "correction-1" } }));
  check("correction retry: a later 4xx never unlocks an exact retry",
    M.memoryCorrectionShouldUnlockFailure(false, 422) &&
    !M.memoryCorrectionShouldUnlockFailure(true, 422) &&
    !M.memoryCorrectionShouldUnlockFailure(false, 500));
  check("correction focus: editable and locked failures have deterministic focus targets",
    M.memoryCorrectionFailureFocusId({ locked: false }) === "memory-correction-error" &&
    M.memoryCorrectionFailureFocusId({ locked: true }) === "memory-correction-lock");
}

{
  const sourceA = { ...base, memory_id: "a".repeat(64), scope: { scope: "user", id: "user-7" } };
  const sourceB = { ...conflictPeer, memory_id: "b".repeat(64), scope: { scope: "user", id: "user-7" } };
  const conflict = { scope: sourceA.scope, key: "response_tone", memory_ids: [sourceB.memory_id, sourceA.memory_id] };
  const draft = M.memoryConsolidationDraft(conflict, [sourceA, sourceB]);
  check("consolidation: conflict evidence opens as one sorted, exact-scope source set",
    draft.kind === "consolidate" && draft.sourceIds[0] === sourceA.memory_id &&
    draft.sourceIds[1] === sourceB.memory_id && M.memoryScopeEqual(draft.scope, sourceA.scope));
  const evidenceHtml = M.memoryConsolidationSourcesHtml(draft);
  check("consolidation evidence: every source shows full identity, contradictory content, provenance, and scope",
    evidenceHtml.includes(sourceA.memory_id) && evidenceHtml.includes(sourceB.memory_id) &&
    evidenceHtml.includes("concise &lt;trusted&gt;") && evidenceHtml.includes("detailed") &&
    evidenceHtml.includes("human:amjad") && evidenceHtml.includes("user:user-7") &&
    evidenceHtml.includes('aria-labelledby="memory-governance-source-title-0"'));
  check("consolidation evidence: hostile content is escaped and each source projection is bounded",
    !M.memoryConsolidationSourcesHtml({ sourceSnapshots: [{ ...sourceA, content: { kind: "inline", value: "<script>bad</script>" } }] }).includes("<script>") &&
    M.memoryConsolidationSourcesHtml({ sourceSnapshots: [{ ...sourceA, content: { kind: "inline", value: "x".repeat(5000) } }] }).includes("inspection view truncated"));
  check("consolidation drift: only the exact refreshed key, scope, and source set remains actionable",
    M.memoryConsolidationConflictCurrent(draft, [{ ...conflict, memory_ids: [...conflict.memory_ids].reverse() }]) &&
    !M.memoryConsolidationConflictCurrent(draft, [{ ...conflict, key: "other" }]) &&
    !M.memoryConsolidationConflictCurrent(draft, [{ ...conflict, scope: { scope: "user", id: "other" } }]) &&
    !M.memoryConsolidationConflictCurrent(draft, [{ ...conflict, memory_ids: [sourceA.memory_id] }]) &&
    !M.memoryConsolidationConflictCurrent(draft, null));
  draft.distiller = "operator-distiller";
  draft.tagsText = "voice, reviewed, voice";
  draft.priority = "3";
  draft.pool = "memory-workers";
  draft.acknowledged = true;
  const validated = M.memoryGovernanceValidateDraft(draft);
  check("consolidation: reviewed fields produce the exact durable task request",
    validated.valid && validated.path === "/memory/consolidate" &&
    validated.payload.distiller === "operator-distiller" && validated.payload.key === "response_tone" &&
    JSON.stringify(validated.payload.memory_ids) === JSON.stringify([sourceA.memory_id, sourceB.memory_id]) &&
    JSON.stringify(validated.payload.tags) === JSON.stringify(["voice", "reviewed"]) &&
    validated.payload.priority === 3 && validated.payload.pool === "memory-workers");
  check("consolidation: tags are bounded, deduplicated, and reject controls",
    M.memoryGovernanceTags("alpha, beta, alpha").tags.length === 2 &&
    Boolean(M.memoryGovernanceTags("safe, bad\nvalue").error) &&
    Boolean(M.memoryGovernanceTags(Array.from({ length: M.MEMORY_GOVERNANCE_TAG_LIMIT + 1 }, (_, index) => `t${index}`).join(",")).error));
  const task = {
    task_id: "tenant--task-consolidation-1", kind: "memory_consolidation", status: "queued", pool: "memory-workers",
    idempotency_key: `memory_consolidation:user:user-7:${"c".repeat(64)}`,
    run_id: null, thread_id: null, parent: null, recipient: null, effect: null, worker_version: null,
    payload: { scope: validated.payload.scope, memory_ids: validated.payload.memory_ids,
      distiller: validated.payload.distiller, key: validated.payload.key, tags: validated.payload.tags,
      priority: validated.payload.priority, written_at: "2026-08-09T08:00:00Z", run_id: null, parent: null },
  };
  const summaryId = "d".repeat(64);
  const governedSummary = {
    memory_id: summaryId, kind: "summary", scope: { ...task.payload.scope }, key: task.payload.key,
    tags: [...task.payload.tags], priority: task.payload.priority,
    provenance: {
      author: { type: "distiller", name: task.payload.distiller },
      evidence: { source_memory_ids: [...task.payload.memory_ids] },
      written_at: task.payload.written_at,
    },
    confidence: 0.7, validity: { valid_from: "2026-08-08T10:00:00Z" },
    created_at: task.payload.written_at,
    content: { kind: "inline", value: { tone: "concise <reviewed>" } },
  };
  const completedTask = { ...task, status: "completed", result: { memory_id: summaryId } };
  const contract = M.memoryConsolidationTaskContract(task);
  check("consolidation follow: durable task normalizes one exact immutable outcome contract",
    contract.ok && contract.taskId === task.task_id && contract.sourceIds[0] === sourceA.memory_id &&
    contract.distiller === task.payload.distiller && contract.key === task.payload.key &&
    contract.priority === 3 && contract.pool === "memory-workers" &&
    M.memoryConsolidationTaskFingerprint(task) === M.memoryConsolidationTaskFingerprint({ ...task, status: "leased" }));
  check("consolidation follow: summary query is scope- and kind-bounded without hiding policy mismatches",
    JSON.stringify(M.memoryConsolidationSummaryQuery(contract)) === JSON.stringify({
      scope: task.payload.scope, kinds: ["summary"], include_expired: true, include_superseded: true,
    }));
  check("consolidation follow: exact summary binds source set, scope, attribution, time, and reviewed policy",
    M.memoryConsolidationSummaryCheck(contract, governedSummary).match &&
    M.memoryConsolidationOutcome(completedTask, [governedSummary]).state === "proven" &&
    M.memoryConsolidationOutcome(completedTask, [governedSummary]).summary.memory_id === summaryId);
  check("consolidation follow: summary proof and task settlement remain independent evidence",
    M.memoryConsolidationOutcome({ ...completedTask, status: "dead" }, [governedSummary]).state === "proven" &&
    M.memoryConsolidationOutcome({ ...task, status: "queued" }, []).state === "waiting" &&
    M.memoryConsolidationOutcome(completedTask, []).label.includes("without summary proof"));
  check("consolidation follow: policy drift cannot masquerade as the reviewed governed summary",
    M.memoryConsolidationOutcome(completedTask, [{ ...governedSummary, key: "other" }]).state === "attention" &&
    M.memoryConsolidationOutcome(completedTask, [{ ...governedSummary, key: "other" }]).differences.includes("summary key") &&
    M.memoryConsolidationOutcome(completedTask, [{ ...governedSummary,
      provenance: { ...governedSummary.provenance, author: { type: "distiller", name: "other" } } }]).state === "attention" &&
    M.memoryConsolidationOutcome(completedTask, [{ ...governedSummary, created_at: "2026-08-09T08:00:01Z" }]).state === "attention");
  check("consolidation follow: contradictory result ids and duplicate exact summaries fail closed",
    M.memoryConsolidationOutcome({ ...completedTask, result: { memory_id: "e".repeat(64) } }, [governedSummary]).state === "attention" &&
    M.memoryConsolidationOutcome(completedTask, [governedSummary, { ...governedSummary, memory_id: "f".repeat(64) }]).label.includes("Multiple") &&
    M.memoryConsolidationOutcome(completedTask, [governedSummary], true).label.includes("incomplete"));
  check("consolidation follow: malformed task contracts never gain a Memory handoff",
    !M.memoryConsolidationTaskContract({ ...task, payload: { ...task.payload, tags: "reviewed" } }).ok &&
    !M.memoryConsolidationTaskContract({ ...task, payload: { ...task.payload, memory_ids: [sourceA.memory_id, sourceA.memory_id] } }).ok &&
    !M.memoryConsolidationTaskContract({ ...task, task_id: 42 }).ok &&
    !M.memoryConsolidationTaskContract({ ...task, payload: { ...task.payload, scope: { scope: "user", id: 7 } } }).ok &&
    !M.memoryConsolidationTaskContract({ ...task, payload: { ...task.payload, written_at: 12 } }).ok &&
    M.memoryConsolidationTaskActionHtml({ ...task, kind: "ordinary" }) === "" &&
    M.memoryConsolidationTaskActionHtml(task).includes("Follow memory outcome"));
  const followHtml = M.memoryConsolidationFollowHtml({ taskId: task.task_id, task: completedTask, contract,
    outcome: M.memoryConsolidationOutcome(completedTask, [governedSummary]), auto: true, refreshing: false,
    failures: 0, error: "", lastChecked: now });
  check("consolidation follow: accessible evidence path exposes task, summary, source identities, and escaped content",
    followHtml.includes('aria-label="Consolidation evidence path"') && followHtml.includes("Governed summary proven") &&
    followHtml.includes(summaryId) && followHtml.includes(sourceA.memory_id) &&
    followHtml.includes("concise &lt;reviewed&gt;") && !followHtml.includes("<reviewed>") &&
    followHtml.includes('data-memory-follow-summary'));
  check("consolidation follow: live polling is visible-workspace and nonterminal only",
    M.memoryConsolidationFollowDelay("memory", "visible", { auto: true, refreshing: false,
      failures: 0, outcome: { state: "waiting" } }) === 1500 &&
    M.memoryConsolidationFollowDelay("memory", "visible", { auto: true, refreshing: false,
      failures: 9, outcome: { state: "waiting" } }) === 20000 &&
    M.memoryConsolidationFollowDelay("tasks", "visible", { auto: true, refreshing: false,
      failures: 0, outcome: { state: "waiting" } }) === null &&
    M.memoryConsolidationFollowDelay("memory", "hidden", { auto: true, refreshing: false,
      failures: 0, outcome: { state: "waiting" } }) === null &&
    M.memoryConsolidationFollowDelay("memory", "visible", { auto: true, refreshing: false,
      failures: 0, outcome: { state: "proven" } }) === null);
  const followState = { taskId: task.task_id };
  M.setMemoryFollowTestState(followState, 7, { baseUrl: "http://tenant-b", apiKey: "b" });
  check("consolidation follow: late refreshes are owned by task state, request generation, and tenant",
    M.memoryConsolidationFollowCurrent(followState, 7, { baseUrl: "http://tenant-b", apiKey: "b" }) &&
    !M.memoryConsolidationFollowCurrent({ taskId: task.task_id }, 7, { baseUrl: "http://tenant-b", apiKey: "b" }) &&
    !M.memoryConsolidationFollowCurrent(followState, 6, { baseUrl: "http://tenant-b", apiKey: "b" }) &&
    !M.memoryConsolidationFollowCurrent(followState, 7, { baseUrl: "http://tenant-a", apiKey: "a" }));
  check("consolidation follow: route absence, missing evidence, and transport failure remain distinct",
    M.memoryConsolidationFollowErrorText({ status: 404, body: { raw: "missing" } }).includes("does not expose") &&
    M.memoryConsolidationFollowErrorText({ status: 404, body: { error: "not_found" } }).includes("no longer exists") &&
    M.memoryConsolidationFollowErrorText(new Error("offline")) === "offline");
  const response = { task_id: task.task_id, kind: "memory_consolidation", deduplicated: false };
  check("consolidation receipt: enqueue identity is corroborated against the durable task payload",
    M.memoryConsolidationValidateReceipt(response, validated.payload, task).ok);
  check("consolidation receipt: stale task or source evidence fails closed",
    !M.memoryConsolidationValidateReceipt({ ...response, task_id: "other-task" }, validated.payload, task).ok &&
    !M.memoryConsolidationValidateReceipt(response, validated.payload,
      { ...task, payload: { ...task.payload, memory_ids: [sourceA.memory_id] } }).ok &&
    !M.memoryConsolidationValidateReceipt(response, validated.payload,
      { ...task, payload: { ...task.payload, scope: { scope: "user", id: "other" } } }).ok);
  const collision = M.memoryConsolidationValidateReceipt(
    { ...response, deduplicated: true }, validated.payload,
    { ...task, pool: "existing-pool", payload: { ...task.payload, distiller: "existing-distiller" } });
  check("consolidation receipt: source-set policy collision is definitive and preserves the existing task handoff",
    !collision.ok && collision.collision && collision.taskId === task.task_id &&
    collision.differences.includes("distiller") && collision.differences.includes("queue pool") &&
    M.memoryGovernanceTaskEvidence({ collision }) === collision &&
    M.memoryGovernanceTaskEvidence({ receipt: { taskId: "confirmed" }, collision }).taskId === "confirmed");
  check("consolidation receipt: unsafe identities, malformed status, and unbounded source lists are rejected",
    !M.memoryConsolidationValidateReceipt({ ...response, task_id: "bad\ntask" }, validated.payload, task).ok &&
    !M.memoryConsolidationValidateReceipt(response, validated.payload, { ...task, status: "mystery" }).ok &&
    M.memoryGovernanceIdList(Array(M.MEMORY_GOVERNANCE_RECEIPT_LIMIT + 1).fill(sourceA.memory_id)) === null);
  check("consolidation receipt: unreviewed task routing, effect, version, and idempotency linkage fail closed",
    !M.memoryConsolidationValidateReceipt(response, validated.payload, { ...task, run_id: "run-other" }).ok &&
    !M.memoryConsolidationValidateReceipt(response, validated.payload, { ...task, thread_id: "thread-other" }).ok &&
    !M.memoryConsolidationValidateReceipt(response, validated.payload, { ...task, recipient: "worker-x" }).ok &&
    !M.memoryConsolidationValidateReceipt(response, validated.payload, { ...task, effect: "idempotent" }).ok &&
    !M.memoryConsolidationValidateReceipt(response, validated.payload, { ...task, worker_version: "v2" }).ok &&
    !M.memoryConsolidationValidateReceipt(response, validated.payload, { ...task, idempotency_key: `memory_consolidation:user:other:${"c".repeat(64)}` }).ok);
  check("consolidation validation: distiller and deliberate queued-not-resolved acknowledgement are mandatory",
    !M.memoryGovernanceValidateDraft({ ...draft, distiller: "", acknowledged: false }).valid);
  check("consolidation validation: queue pool mirrors the server's 128-byte ASCII grammar",
    M.memoryGovernancePool("") && M.memoryGovernancePool("memory-workers.v1") &&
    M.memoryGovernancePool("a".repeat(128)) && !M.memoryGovernancePool("a".repeat(129)) &&
    !M.memoryGovernancePool("memory workers") && !M.memoryGovernancePool("mémory") &&
    !M.memoryGovernanceValidateDraft({ ...draft, pool: "memory workers" }).valid);
  const spacedLabels = M.memoryGovernanceValidateDraft({ ...draft, distiller: " operator ", key: " response_tone " });
  check("consolidation validation: visible label whitespace is preserved exactly while pool and numeric whitespace fail closed",
    spacedLabels.valid && spacedLabels.payload.distiller === " operator " && spacedLabels.payload.key === " response_tone " &&
    !M.memoryGovernanceValidateDraft({ ...draft, pool: " memory-workers" }).valid &&
    !M.memoryGovernanceValidateDraft({ ...draft, priority: " 3" }).valid &&
    !M.memoryGovernanceValidateDraft({ ...draft, priority: "-0" }).valid);
  check("consolidation validation: run scopes and cross-scope evidence are rejected before enqueue",
    (() => { try { M.memoryConsolidationDraft({ ...conflict, scope: { scope: "run", id: "run-1" } }, [sourceA, sourceB]); return false; } catch { return true; } })() &&
    (() => { try { M.memoryConsolidationDraft(conflict, [sourceA, { ...sourceB, scope: { scope: "team", id: "team-1" } }]); return false; } catch { return true; } })());
  const manyIds = Array.from({ length: M.MEMORY_CONSOLIDATION_SOURCE_LIMIT + 1 }, (_, index) =>
    index.toString(16).padStart(64, "0"));
  check("consolidation validation: an exact evidence review never exceeds the bounded source dossier",
    (() => { try {
      M.memoryConsolidationDraft({ ...conflict, memory_ids: manyIds }, manyIds.map((memory_id) => ({ ...sourceA, memory_id })));
      return false;
    } catch { return true; } })());
  check("consolidation compatibility: missing routes and missing source records stay distinct",
    M.memoryGovernanceErrorText({ status: 404, body: { raw: "missing" } }).includes("does not expose") &&
    M.memoryGovernanceErrorText({ status: 404, body: { error: "not_found" } }).includes("source record"));
  draft.generation = 4;
  M.setMemoryGovernanceTestState(draft, { baseUrl: "http://tenant-b", apiKey: "b" });
  check("consolidation isolation: only the current draft generation and tenant can accept task evidence",
    M.memoryGovernanceCurrent(draft, 4, { baseUrl: "http://tenant-b", apiKey: "b" }) &&
    !M.memoryGovernanceCurrent(draft, 3, { baseUrl: "http://tenant-b", apiKey: "b" }) &&
    !M.memoryGovernanceCurrent(draft, 4, { baseUrl: "http://tenant-a", apiKey: "a" }));
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
check("assembly markup: the lab is labelled, expandable, status-aware, and available from selected evidence",
  html.includes('id="btn-memory-assemble" aria-expanded="false" aria-controls="memory-assembly"') &&
  html.includes('id="memory-assembly" aria-labelledby="memory-assembly-title" hidden') &&
  html.includes('id="memory-assembly-result" tabindex="-1" aria-live="polite"') &&
  html.includes("data-memory-assemble-record"));
check("assembly isolation: connection changes discard previews and late responses cannot cross tenants",
  html.includes("store.memoryAssemblyRequest += 1") && html.includes("store.memoryAssembly = null") &&
  html.includes("memoryRequestCurrent(requestId, requestConnection, store.memoryAssemblyRequest, store.conn)") &&
  html.includes("store.memoryAssembly !== draft"));
check("assembly retrieval: expiry is pinned and exact assembly is separated from a non-atomic live rank comparison",
  html.includes('const resolvedQuery = { ...checked.query, as_of: checked.query.as_of || new Date().toISOString() }') &&
  html.includes('apiForConnection(requestConnection, "POST", "/memory/query", budgetedPayload, MEMORY_ASSEMBLY_RESPONSE_BYTES)') &&
  html.includes('apiForConnection(requestConnection, "POST", "/memory/query", resolvedQuery, MEMORY_ASSEMBLY_RESPONSE_BYTES)') &&
  html.includes("memoryAssemblyValidateResult(assemblyResult.value, ranked ? rankedResult.value : null, checked.budget)"));
check("assembly containment: each response is byte-bounded while an ancillary failure preserves exact evidence",
  M.MEMORY_ASSEMBLY_RESPONSE_BYTES === 8 * 1024 * 1024 && html.includes("async function apiResponseText(res, maxBytes = 0)") &&
  html.includes("received > maxBytes") && html.includes('comparisonState: ranked ? "available" : "unavailable"'));
check("assembly privacy: preview requests deliberately omit run linkage and browser persistence",
  html.includes("Read-only preview · no run ID · no journal event · no browser persistence") &&
  !html.includes("memoryAssemblyPersistence") && !html.includes("localStorage.setItem(\"ags:memory-assembly"));
check("assembly interaction: edited parameters invalidate old evidence and selected results hand off to the bounded ledger",
  html.includes("function memoryAssemblyMarkEdited()") && html.includes("Parameters changed") &&
  html.includes("state.assemblyInjected = new Set") && html.includes("slice(0, MEMORY_SNAPSHOT_LIMIT)") &&
  html.includes("context preview"));
check("assembly focus: submitting and terminal rerenders use a stable result target and close returns to its origin",
  html.includes("memoryAssemblyCaptureFocus()") && html.includes("memoryAssemblyRestoreFocus(focus)") &&
  html.includes('target.area === "assembly"') && html.includes("if (next && next.disabled)") &&
  html.includes("memoryAssemblyFocusOutcome(draft)") && html.includes('draft.returnFocus.type === "record"'));
check("assembly responsive: loom, controls, evidence rows, and budget facts collapse for narrow screens",
  html.includes(".memory-assembly-body { grid-template-columns: 1fr; }") &&
  html.includes(".memory-assembly-fields, .memory-assembly-checks { grid-template-columns: 1fr; }") &&
  html.includes(".memory-budget-facts { grid-template-columns: 1fr; }") &&
  html.includes(".memory-assembly-record { grid-template-columns: 28px minmax(0, 1fr); }"));
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
check("correction lifecycle: connection changes discard drafts and stale writes cannot cross tenants",
  html.includes("store.memoryCorrection = null") && html.includes("memoryCorrectionCurrent(draft, generation, connection)") &&
  html.includes("connectionIdentityChanged(connection, store.conn)"));
check("correction lifecycle: ambiguous writes reconcile before exposing exact retry",
  html.includes("memoryCorrectionReconcile(draft, payload, generation, connection)") &&
  html.includes("Retry only this exact locked correction") && html.includes("draft.attemptedPayload = JSON.parse(JSON.stringify(payload))"));
check("correction lifecycle: identity collisions remove the retry path",
  html.includes('draft.retryAllowed = reconciled.status !== "collision"') &&
  html.includes("draft.locked && draft.retryAllowed !== true"));
check("correction lifecycle: run scope requires a finalized journal before adoption",
  html.includes("feed.complete !== true") && html.includes("Choose a completed run or a candidacy scope"));
check("correction accessibility: workspace is labelled, busy-aware, and keeps validation focus local",
  html.includes('id="memory-correction" aria-labelledby="memory-correction-title" hidden') &&
  html.includes('panel.setAttribute("aria-busy"') && html.includes('role="group" aria-label="Corrected content format"') &&
  html.includes('role="alert"') && html.includes('tabindex="-1"') && html.includes("setSelectionRange") &&
  html.includes('$("memory-correction-title")?.focus({ preventScroll: true })') &&
  html.includes("memoryCorrectionFocusFailure(draft)") && !html.includes('inp-memory-correction-rationale'));
check("correction interaction: attribution and destination reviews update without replacing the focused editor",
  html.includes('id="memory-correction-review-author"') && html.includes('id="memory-correction-review-destination"') &&
  html.includes('review.textContent = author ? `human:${author}`') &&
  html.includes('destination.textContent = `${draft.scopeType}:${draft.scopeId || "?"}`'));
check("correction interaction: editing clears stale assertive and field errors without rerendering the form",
  html.includes('querySelector(".memory-correction-error")?.remove()') &&
  html.includes('$(`memory-correction-${field}-error`)?.remove()') &&
  html.includes('event.target.removeAttribute("aria-invalid")'));
check("correction responsive: memory splice and editor collapse to one column",
  html.includes(".memory-splice { grid-template-columns: 1fr; }") &&
  html.includes(".memory-correction-body { grid-template-columns: 1fr; }") &&
  html.includes(".memory-correction-fields { grid-template-columns: 1fr; }"));
check("consolidation capability: actions appear only with a confirmed conflict contract",
  M.memoryDetailHtml({ ...base, memory_id: "a".repeat(64) },
    M.memoryContext([], [{ memory_ids: ["a".repeat(64)] }], now, { operationsAvailable: true }))
    .includes("data-memory-consolidate-record") &&
  !M.memoryDetailHtml({ ...base, memory_id: "a".repeat(64) },
    M.memoryContext([], [{ memory_ids: ["a".repeat(64)] }], now)).includes("data-memory-consolidate-record"));
check("consolidation accessibility: conflict cards are labelled articles with separate evidence and planning actions",
  M.memoryConflictsHtml(conflicts, null).includes('<article class="memory-conflict-card" aria-labelledby="memory-conflict-title-0"') &&
  M.memoryConflictsHtml(conflicts, null).includes('data-memory-consolidate="0"') &&
  M.memoryConflictsHtml(conflicts, null).includes('aria-label="Plan consolidation for response_tone"') &&
  M.memoryConflictsHtml([{ ...conflicts[0], key: '&quot; <hostile>' }], null).includes('Plan consolidation for &amp;quot; &lt;hostile&gt;') &&
  !M.memoryConflictsHtml(conflicts, null).includes('<button class="memory-conflict-card"'));
check("consolidation bounds: oversized source sets remain reviewable but expose no false launch affordance",
  M.memoryConflictsHtml([{ ...conflicts[0], memory_ids: Array(M.MEMORY_CONSOLIDATION_SOURCE_LIMIT + 1).fill("a".repeat(64)) }], null)
    .includes("Review outside Studio") &&
  M.memoryConflictsHtml([{ ...conflicts[0], memory_ids: Array(M.MEMORY_CONSOLIDATION_SOURCE_LIMIT + 1).fill("a".repeat(64)) }], null)
    .includes("disabled title="));
check("consolidation truth: the consequence braid and receipt never equate queued work with resolution",
  html.includes("A queue receipt proves durable work, not a resolved memory conflict") &&
  html.includes("Studio does not call the conflict resolved from a task receipt alone") &&
  html.includes("the enqueue receipt alone changes no memory") &&
  !html.includes("completed summary names these sources"));
check("consolidation lifecycle: ambiguous enqueue evidence locks one exact deduplicated retry",
  html.includes("draft.attemptedPayload = JSON.parse(JSON.stringify(payload))") &&
  html.includes("Retry only this exact locked source set") && html.includes("deduplicates it by scope and sorted sources") &&
  html.includes('apiForConnection(connection, "GET", `/tasks/${encodeURIComponent(taskId)}`)'));
check("consolidation collision: a source set with different durable policy never enters a futile retry loop",
  html.includes("Source set already owned") && html.includes("The reviewed policy was not accepted") &&
  html.includes("Inspect existing task") && html.includes("draft.retryAllowed = false") &&
  html.includes("draft.collision = checked"));
check("consolidation drift: refresh and first submit both block stale conflict evidence while exact retry stays locked",
  html.includes("memoryConsolidationConflictCurrent(governance, state.conflicts)") &&
  html.includes("memoryConsolidationConflictCurrent(draft, store.memory && store.memory.conflicts)") &&
  html.includes("The current conflict evidence no longer matches this reviewed source set") &&
  html.includes('if (exactRetry) {\n    payload = draft.attemptedPayload;\n  } else {\n    if (!memoryConsolidationConflictCurrent'));
check("consolidation lifecycle: connection changes clear drafts and stale task evidence is rejected",
  html.includes("store.memoryGovernance = null") && html.includes("memoryGovernanceCurrent(draft, generation, connection)") &&
  html.includes("connectionIdentityChanged(connection, store.conn)"));
check("consolidation accessibility: validation, busy state, announcement, and durable task handoff are explicit",
  html.includes('id="memory-governance" aria-labelledby="memory-governance-title" hidden') &&
  html.includes('panel.setAttribute("aria-busy"') && html.includes('id="memory-announcer" role="status"') &&
  html.includes('aria-label="Consolidation consequence"') && html.includes("memoryGovernanceOpenTask") &&
  html.includes('${invalid("acknowledged")}'));
check("consolidation evidence accessibility: the complete bounded dossier precedes acknowledgement",
  html.includes('aria-labelledby="memory-governance-evidence-title"') &&
  html.includes("Full identities remain visible") &&
  html.indexOf("memoryConsolidationSourcesHtml(draft)") < html.indexOf("chk-memory-governance-ack"));
check("consolidation responsive: consequence, form, and review collapse without hiding primary actions",
  html.includes(".memory-consequence { grid-template-columns: 1fr; }") &&
  html.includes(".memory-governance-body { grid-template-columns: 1fr; }") &&
  html.includes(".memory-governance-fields { grid-template-columns: 1fr; }"));
check("consolidation follow lifecycle: accepted receipts and discovered durable tasks share one evidence workspace",
  html.includes("memoryConsolidationFollowStart(receipt.task)") &&
  html.includes('data-memory-follow-task-id=') &&
  html.includes("memoryConsolidationFollowOpen(task)") &&
  html.includes('id="memory-follow" aria-labelledby="memory-follow-title" hidden'));
check("consolidation follow reads: task contract, scoped summaries, and result record are independently corroborated",
  html.includes('apiForConnection(connection, "GET", `/tasks/${encodeURIComponent(follow.taskId)}`)') &&
  html.includes('apiForConnection(connection, "POST", "/memory/query", memoryConsolidationSummaryQuery(follow.contract))') &&
  html.includes('apiForConnection(connection, "GET", `/memory/${encodeURIComponent(resultId)}`)') &&
  html.includes("memoryConsolidationTaskFingerprint(task) !== follow.fingerprint"));
check("consolidation follow isolation: connection reset and request ownership stop prior-tenant refreshes",
  html.includes("store.memoryFollowRequest += 1") && html.includes("store.memoryFollow = null") &&
  html.includes("memoryConsolidationFollowCurrent(follow, request, connection)") &&
  html.includes("connectionIdentityChanged(connection, store.conn)"));
check("consolidation follow accessibility: live state, refresh, task, summary, and close actions stay labelled",
  html.includes('role="list" aria-label="Consolidation evidence path"') &&
  html.includes('id="chk-memory-follow-auto"') && html.includes('aria-disabled="${value.refreshing}"') &&
  html.includes('id="memory-follow-error"') && html.includes('data-memory-follow-summary'));
check("consolidation follow responsive: evidence path and summary metadata collapse to one column",
  html.includes(".memory-follow-path { grid-template-columns: 1fr; }") &&
  html.includes(".memory-follow-joint { transform: rotate(90deg); }") &&
  html.includes(".memory-follow-summary dl { grid-template-columns: 1fr;"));
check("responsive shell: mobile navigation leaves the workspace in the first viewport",
  html.includes("max-height: 34vh; overflow-y: auto") && html.includes("@media (max-width: 1120px)"));
check("accessibility: small memory metadata uses the higher-contrast dim token",
  !html.match(/\.memory-(?:summary span|toolbar label|scope|proof-step span|kv \.k)[^{]*\{[^}]*text-faint/));
check("accessibility: conflict explanation uses the reviewed AA color",
  html.includes(".memory-conflict-detail p { margin: 4px 0 8px; color: #aaa08f;"));

console.log(`\n${passed} passed, ${failed} failed`);
if (failed) process.exit(1);
