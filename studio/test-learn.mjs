#!/usr/bin/env node
/* Node regression tests for the governed-learning helpers embedded in
 * Studio's zero-build index. The browser bootstrap is stripped and pure
 * wire/state/view helpers run dependency-free under vm.
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
globalThis.__learn = {
  LEARN_SNAPSHOT_LIMIT, LEARN_RENDER_LIMIT, LEARN_VERSION_LIMIT, LEARN_ID_LIMIT, LEARN_SURFACE_LIMIT,
  LEARN_TEXT_LIMIT, LEARN_PREVIEW_LIMIT,
  LEARN_REPLAY_LIMIT, LEARN_REPLAY_TEXT_LIMIT,
  learnObject, learnText, learnNormalizeRecord, learnNormalizePointer,
  learnKind, learnKindLabel, learnStatusLabel, learnScopeAddressKey, learnScopeAddress, learnSurfaceKey, learnSurface,
  learnCandidateTitle, learnCandidateId, learnSearchText, learnBuildSearchIndex,
  learnFilterRecords, learnPointerFor, learnServingState, learnStatusHtml,
  learnCandidateRowHtml, learnSummaryHtml, learnRailHtml, learnChangeHtml,
  learnEvidenceCounts, learnVerdictHtml, learnEvaluationHtml, learnServingHtml,
  learnVersionsHtml, learnDraftFor, learnValidateLink, learnReplayRunIds, learnEvaluatePayload,
  learnPromotePayload, learnRollbackPayload, learnActionValidation,
  learnFieldError, learnErrorListHtml, learnActionHtml, learnDetailHtml,
  learnErrorHtml, learnPromotionRequiredEffect, learnActionPath,
  learnRefusalMessage, learnCandidateSnapshot, learnVersionSnapshot,
  learnConflictNeedsRefresh, learnFinalizedRunEvidence, learnReplayEvidenceJournal, learnTransitionMessage,
};`, sandbox, { filename: "index.html<script>" });

const L = sandbox.__learn;
const prompt = {
  candidate: {
    candidate_id: "a".repeat(64),
    content: { kind: "prompt", name: "system", prompt: "Be careful <always>." },
    distilled_by: { type: "distiller", name: "correction-loop" },
    evidence: { run_ids: ["run-1"], correction_ids: ["correction-1"], memory_ids: ["memory-1"] },
    created_at: "2026-08-09T01:00:00Z",
  },
  status: "created",
};
const evaluation = {
  candidate_id: "b".repeat(64), dataset_version: "support-v3",
  replay: { fixture_ids: ["run-a", "run-b"], matched: 2 },
  baseline_report: { format_version: 1, name: "support@support-v3", dataset_version: "support-v3", summary: { run_pass_rate: 0.5 } },
  candidate_report: { format_version: 1, name: "support@support-v3", dataset_version: "support-v3", summary: { run_pass_rate: 0.9 } },
  verdict: { regressed: false, target_metric: "run_pass_rate", baseline: 0.5, candidate: 0.9, delta: 0.4 },
  thresholds: { max_pass_rate_drop: 0.05, max_latency_p95_ratio: 1.25 },
  evaluated_by: { type: "human", human_id: "reviewer-1" }, evaluated_at: "2026-08-09T02:00:00Z",
};
const memory = {
  candidate: {
    candidate_id: "b".repeat(64),
    content: { kind: "memory_set", scope: { scope: "agent", id: "support-1" },
      adds: [{ memory_id: "mem-1", content: { kind: "inline", value: "warm" } }], supersedes: ["mem-old"] },
    distilled_by: { type: "agent", agent_id: "distiller-1" }, evidence: { run_ids: ["run-a", "run-b"] },
    created_at: "2026-08-09T02:00:00Z",
  },
  status: "evaluated", evaluation,
};
const policy = {
  candidate: {
    candidate_id: "c".repeat(64), content: { kind: "policy", family: "retry", parameters: { max_attempts: 4 } },
    distilled_by: { type: "system" }, created_at: "2026-08-09T03:00:00Z",
  },
  status: "promoted", evaluation: { ...evaluation, candidate_id: "c".repeat(64) },
  promotion: { candidate_id: "c".repeat(64), surface: "policy:retry", promoted_at: "2026-08-09T04:00:00Z",
    decision: { authority: { authority: "envelope", envelope_version: "r0.8-default" } } },
};
const tool = {
  candidate: {
    candidate_id: "d".repeat(64), content: { kind: "tool_permission", tool: "billing", direction: "narrow" },
    distilled_by: { type: "human", human_id: "ops-1" }, created_at: "2026-08-09T04:00:00Z",
  },
  status: "rolled_back", evaluation: { ...evaluation, candidate_id: "d".repeat(64) },
  promotion: { candidate_id: "d".repeat(64), surface: "tool:billing", previous: "e".repeat(64),
    promoted_at: "2026-08-09T05:00:00Z", decision: { authority: { authority: "human" } } },
  rollback: { from: "d".repeat(64), surface: "tool:billing", to: "e".repeat(64), cause: "drift", rolled_back_at: "2026-08-09T06:00:00Z" },
};
const records = [prompt, memory, policy, tool];
const versions = [
  { surface: "policy:retry", active: policy.candidate.candidate_id, canary: null },
  { surface: "memory:agent:support-1", active: null, canary: { candidate_id: memory.candidate.candidate_id, fraction: 0.1 } },
];

let passed = 0, failed = 0;
function check(name, condition, detail) {
  if (condition) { passed++; console.log(`ok   ${name}`); }
  else { failed++; console.log(`FAIL ${name}${detail ? ` — ${detail}` : ""}`); }
}
function eq(name, got, want) {
  check(name, JSON.stringify(got) === JSON.stringify(want), `got ${JSON.stringify(got)}, want ${JSON.stringify(want)}`);
}

check("bounds: learning snapshots, rendering, pointers, identifiers, text, and previews are explicit",
  L.LEARN_SNAPSHOT_LIMIT === 200 && L.LEARN_RENDER_LIMIT === 100 && L.LEARN_VERSION_LIMIT === 200 &&
  L.LEARN_ID_LIMIT === 256 && L.LEARN_SURFACE_LIMIT === 4096 && L.LEARN_TEXT_LIMIT === 4096 && L.LEARN_PREVIEW_LIMIT === 32768 &&
  L.LEARN_REPLAY_LIMIT === 8 && L.LEARN_REPLAY_TEXT_LIMIT === 2048);
check("normalization: valid immutable records retain their lifecycle", L.learnNormalizeRecord(prompt).status === "created");
check("normalization: source candidate records are not mutated", L.learnNormalizeRecord(prompt) !== prompt && prompt.candidate.content.prompt.includes("<always>"));
check("normalization: records without bounded identity fail closed", !L.learnNormalizeRecord({ ...prompt, candidate: { ...prompt.candidate, candidate_id: "x".repeat(257) } }));
check("normalization: candidate identity is exactly lowercase SHA-256", !L.learnNormalizeRecord({ ...prompt, candidate: { ...prompt.candidate, candidate_id: "A".repeat(64) } }) && !L.learnNormalizeRecord({ ...prompt, candidate: { ...prompt.candidate, candidate_id: "a".repeat(63) } }));
check("normalization: future lifecycle states fail closed before actions", !L.learnNormalizeRecord({ ...prompt, status: "silently_applied" }));
check("normalization: every candidate kind must satisfy its exact wire shape",
  !L.learnNormalizeRecord({ ...prompt, candidate: { ...prompt.candidate, content: { kind: "prompt", name: "system" } } }) &&
  !L.learnNormalizeRecord({ ...policy, candidate: { ...policy.candidate, content: { kind: "policy", family: "invented", parameters: {} } } }) &&
  !L.learnNormalizeRecord({ ...memory, candidate: { ...memory.candidate, content: { ...memory.candidate.content, scope: { scope: "global", id: "x" } } } }) &&
  !L.learnNormalizeRecord({ ...tool, candidate: { ...tool.candidate, content: { ...tool.candidate.content, direction: "anything" } } }));
check("normalization: lifecycle receipts must describe the exact candidate and surface",
  !L.learnNormalizeRecord({ ...policy, promotion: { ...policy.promotion, candidate_id: "9".repeat(64) } }) &&
  !L.learnNormalizeRecord({ ...tool, rollback: { ...tool.rollback, surface: "tool:other" } }) &&
  !L.learnNormalizeRecord({ ...memory, evaluation: { ...memory.evaluation, replay: {} } }));
check("normalization: replay evidence must be non-vacuous, typed, and internally consistent",
  !L.learnNormalizeRecord({ ...memory, evaluation: { ...memory.evaluation, replay: { fixture_ids: [], matched: 0 } } }) &&
  !L.learnNormalizeRecord({ ...memory, evaluation: { ...memory.evaluation, replay: { fixture_ids: ["run-a"], matched: 0 } } }) &&
  !L.learnNormalizeRecord({ ...memory, evaluation: { ...memory.evaluation, replay: { fixture_ids: ["run-a"], matched: 0, divergences: [{ fixture_id: "run-b", detail: "mismatch" }] } } }));
check("normalization: verdict, thresholds, reports, evaluator, and timestamp are required evidence",
  !L.learnNormalizeRecord({ ...memory, evaluation: { ...memory.evaluation, verdict: { foo: 1 } } }) &&
  !L.learnNormalizeRecord({ ...memory, evaluation: { ...memory.evaluation, thresholds: { max_pass_rate_drop: 0.05, max_latency_p95_ratio: "1.25" } } }) &&
  !L.learnNormalizeRecord({ ...memory, evaluation: { ...memory.evaluation, baseline_report: { name: "support", summary: {} } } }) &&
  !L.learnNormalizeRecord({ ...memory, evaluation: { ...memory.evaluation, evaluated_by: { type: "human", human_id: "" } } }) &&
  !L.learnNormalizeRecord({ ...memory, evaluation: { ...memory.evaluation, evaluated_at: "not-a-time" } }));
check("normalization: unknown candidate kinds fail closed before actions", !L.learnNormalizeRecord({ ...prompt, candidate: { ...prompt.candidate, content: { kind: "root_access" } } }));
check("normalization: lifecycle status cannot outrun or contradict its receipts", !L.learnNormalizeRecord({ ...prompt, status: "evaluated" }) && !L.learnNormalizeRecord({ ...memory, status: "created" }) && !L.learnNormalizeRecord({ ...policy, status: "rolled_back" }));
eq("pointer normalization: active and valid canary survive", L.learnNormalizePointer(versions[1]), versions[1]);
check("pointer normalization: invalid canary bindings fail the whole pointer closed", !L.learnNormalizePointer({ surface: "x", canary: { candidate_id: "b".repeat(64), fraction: 2 } }));
check("pointer normalization: control characters are rejected", !L.learnNormalizePointer({ surface: "policy:\nretry" }));
check("pointer normalization: malformed active and canary identities fail the whole pointer closed", !L.learnNormalizePointer({ surface: "policy:retry", active: "bad\nid" }) && !L.learnNormalizePointer({ surface: "policy:retry", canary: { candidate_id: "bad\nid", fraction: 0.1 } }));

eq("labels: all candidate kinds have plain names", records.map((record) => L.learnKindLabel(L.learnKind(record))),
  ["Prompt", "Memory set", "Policy", "Tool permission"]);
eq("labels: lifecycle names describe the next decision", records.map((record) => L.learnStatusLabel(record.status)),
  ["Needs evaluation", "Ready for gate", "Serving", "Rolled back"]);
eq("surfaces: wire content maps to the exact version-pointer namespace", records.map(L.learnSurface),
  ["prompt:system", "memory:agent:support-1", "policy:retry", "tool:billing"]);
eq("titles: each proposal is recognizable without its digest", records.map(L.learnCandidateTitle),
  ["system", "agent:support-1 memory", "retry policy", "billing · narrow"]);
check("titles: hostile server labels are bounded before entering the DOM", L.learnCandidateTitle({ ...prompt, candidate: { ...prompt.candidate, content: { ...prompt.candidate.content, name: "x".repeat(10000) } } }).length === 160);
check("identity: candidate digest remains fully available", L.learnCandidateId(prompt) === "a".repeat(64));
const longSurfaceRecord = { ...prompt, candidate: { ...prompt.candidate, content: { ...prompt.candidate.content, name: "n".repeat(300) } } };
const longSurfacePointer = { surface: `prompt:${"n".repeat(300)}`, active: prompt.candidate.candidate_id, canary: null };
check("surfaces: canonical identity is never display-truncated during pointer matching", L.learnSurface(longSurfaceRecord).length < L.learnSurfaceKey(longSurfaceRecord).length && L.learnPointerFor(longSurfaceRecord, [longSurfacePointer]) === longSurfacePointer);

const search = L.learnBuildSearchIndex(records);
check("search: indexes surface, distiller, evidence, and dataset", search.get(prompt.candidate.candidate_id).includes("correction-loop") && search.get(memory.candidate.candidate_id).includes("support-v3"));
eq("filters: kind and lifecycle compose", L.learnFilterRecords(records, { kind: "memory_set", status: "evaluated" }, search).map(L.learnCandidateId), [memory.candidate.candidate_id]);
check("filters: evidence ids are searchable", L.learnFilterRecords(records, { search: "correction-1" }, search)[0] === prompt);
check("filters: non-matches are excluded", L.learnFilterRecords(records, { search: "not-present" }, search).length === 0);

check("serving: active pointer proves full-traffic service", L.learnServingState(policy, versions[0]).mode === "active");
check("serving: canary pointer proves bounded service", L.learnServingState(memory, versions[1]).mode === "canary" && L.learnServingState(memory, versions[1]).label.includes("10%"));
check("serving: a promoted record without its pointer is a visible mismatch", L.learnServingState(policy, null).mode === "mismatch");
check("serving: failed pointer reads remain unknown, never inactive", L.learnServingState(policy, null, false).mode === "unknown");
check("serving: no pointer means the static version serves", L.learnServingState(prompt, null).label.includes("Static"));
check("pointer lookup: surface, not array position, binds evidence", L.learnPointerFor(policy, versions) === versions[0]);

check("row: title, surface, stage, and attribution travel together", L.learnCandidateRowHtml(prompt).includes("prompt:system") && L.learnCandidateRowHtml(prompt).includes("correction-loop") && L.learnCandidateRowHtml(prompt).includes("Needs evaluation"));
check("row: proposal text is not leaked into the scan list", !L.learnCandidateRowHtml(prompt).includes("Be careful"));
check("row: untrusted labels are escaped", L.learnCandidateRowHtml({ ...prompt, candidate: { ...prompt.candidate, content: { ...prompt.candidate.content, name: "<script>" } } }).includes("&lt;script&gt;"));
check("summary: retained candidates, action queues, and serving surfaces are honest", L.learnSummaryHtml(records, versions, { candidates: 9, versions: 7 }).includes("<b>9</b>") && L.learnSummaryHtml(records, versions, { candidates: 9, versions: 7 }).includes("<b>7</b>"));

check("rail: created candidates expose observation as the current boundary", L.learnRailHtml(prompt, { mode: "inactive" }).match(/learn-stage done current[^>]*><b>Observed/));
check("rail: promoted serving candidates expose recovery without a second current stage", L.learnRailHtml(policy, { mode: "active" }).includes('learn-stage done"><b>Recoverable') && (L.learnRailHtml(policy, { mode: "active" }).match(/ current/g) || []).length === 1);
check("rail: rolled-back candidates preserve the complete evidence path", (L.learnRailHtml(tool, { mode: "inactive" }).match(/learn-stage done/g) || []).length === 4);
check("rail: exactly one semantic current step is exposed", (L.learnRailHtml(policy, { mode: "active" }).match(/aria-current="step"/g) || []).length === 1 && L.learnRailHtml(policy, { mode: "active" }).startsWith('<ol'));
check("change: prompt content is escaped and visible only in the dossier", L.learnChangeHtml(prompt).includes("Be careful &lt;always&gt;"));
check("change: policy parameters remain inspectable", L.learnChangeHtml(policy).includes("max_attempts"));
check("change: memory additions and supersessions are counted", L.learnChangeHtml(memory).includes("1 immutable record") && L.learnChangeHtml(memory).includes("1 record"));
check("change: tool permission direction is explicit", L.learnChangeHtml(tool).includes("narrow"));
eq("evidence: source spans remain attributable by kind", L.learnEvidenceCounts(prompt.candidate), { runs: 1, corrections: 1, memories: 1 });

check("verdict: missing evaluation never implies readiness", L.learnVerdictHtml(null).includes("No evaluation recorded"));
check("verdict: clean movement is explicit", L.learnVerdictHtml(evaluation).includes("No threshold regression") && L.learnVerdictHtml(evaluation).includes("+0.4"));
check("verdict: regressions override positive-looking movement", L.learnVerdictHtml({ ...evaluation, verdict: { ...evaluation.verdict, regressed: true } }).includes("Regression detected"));
check("evaluation: replay coverage, thresholds, and evaluator are visible", L.learnEvaluationHtml(memory).includes("2/2 matched") && L.learnEvaluationHtml(memory).includes("latency ratio") && L.learnEvaluationHtml(memory).includes("reviewer-1"));
check("serving panel: full traffic and canary are independently disclosed", L.learnServingHtml(memory, versions[1], true).includes("Canary on 10%") && L.learnServingHtml(memory, versions[1], true).includes("static version"));
check("versions: active and canary pointers render without inventing deployments", L.learnVersionsHtml(versions).includes("policy:retry") && L.learnVersionsHtml(versions).includes("canary · 10%"));
check("versions: endpoint failure blocks with a visible evidence error", L.learnVersionsHtml([], "offline").includes("Serving versions unavailable") && L.learnVersionsHtml([], "offline").includes("offline"));
check("candidate envelopes: malformed and invalid retained records fail closed", Boolean(L.learnCandidateSnapshot({}).error) && Boolean(L.learnCandidateSnapshot({ candidates: [{ nope: true }] }).error));
const largeCandidateInbox = Array.from({ length: 201 }, (_, index) => ({ ...prompt, candidate: { ...prompt.candidate, candidate_id: index.toString(16).padStart(64, "0") } }));
check("candidate envelopes: a valid oversized inbox remains an explicit bounded snapshot", L.learnCandidateSnapshot({ candidates: largeCandidateInbox }).records.length === L.LEARN_SNAPSHOT_LIMIT && L.learnCandidateSnapshot({ candidates: largeCandidateInbox }).truncated);
check("version envelopes: malformed, invalid, and unbounded pointer sets make serving state unavailable", Boolean(L.learnVersionSnapshot({}).error) && Boolean(L.learnVersionSnapshot({ versions: [{ surface: "policy:retry", active: "invalid" }] }).error) && Boolean(L.learnVersionSnapshot({ versions: Array.from({ length: 201 }, () => versions[0]) }).error));
check("evidence envelopes: duplicate candidate identities and serving surfaces fail closed", Boolean(L.learnCandidateSnapshot({ candidates: [prompt, prompt] }).error) && Boolean(L.learnVersionSnapshot({ versions: [versions[0], versions[0]] }).error));

const draftState = { drafts: Object.create(null) };
const evaluateDraft = L.learnDraftFor(draftState, prompt, "evaluate");
check("drafts: action state is candidate-scoped and defaults from recorded evidence", evaluateDraft.runId === "run-1" && evaluateDraft.replayRuns === "run-1" && evaluateDraft.dataset === "support-v1" && !evaluateDraft.acknowledge);
check("drafts: hostile legal candidate keys stay own properties", Object.getPrototypeOf(draftState.drafts) === null);
check("drafts: malformed evidence identifiers never prefill an action", L.learnDraftFor({ drafts: Object.create(null) }, { ...prompt, candidate: { ...prompt.candidate, evidence: { run_ids: ["run\nunsafe", "x".repeat(300)] } } }, "evaluate").runId === "");
check("evaluation payload: acknowledgement is mandatory", !L.learnEvaluatePayload(evaluateDraft).payload && L.learnEvaluatePayload(evaluateDraft).errors.some((error) => error.field === "acknowledge"));
evaluateDraft.acknowledge = true;
check("evaluation payload: dataset, metric, thresholds, empty replay claim, and journal link are exact", JSON.stringify(L.learnEvaluatePayload(evaluateDraft).payload) === JSON.stringify({ request: { dataset_version: "support-v1", target_metric: "run_pass_rate", thresholds: { max_pass_rate_drop: 0.05, max_latency_p95_ratio: 1.25 }, replay_evidence: [] }, run_id: "run-1" }));
check("evaluation payload: invalid thresholds fail closed", !L.learnEvaluatePayload({ ...evaluateDraft, passDrop: "-1" }).payload && !L.learnEvaluatePayload({ ...evaluateDraft, latencyRatio: "Infinity" }).payload);
eq("replay evidence: run identifiers are trimmed, deduplicated, and ordered", L.learnReplayRunIds(" run-a, run-b\nrun-a ").ids, ["run-a", "run-b"]);
check("replay evidence: an empty set cannot satisfy cleanliness vacuously", !L.learnEvaluatePayload({ ...evaluateDraft, replayRuns: "" }).payload && L.learnEvaluatePayload({ ...evaluateDraft, replayRuns: "" }).errors.some((error) => error.field === "replayRuns"));
check("replay evidence: fixture fan-out and text are independently bounded", L.learnReplayRunIds(Array.from({ length: 9 }, (_, i) => `run-${i}`).join(",")).error.includes("At most 8") && L.learnReplayRunIds("x".repeat(2049)).error.includes("2 KiB"));
check("replay evidence: evaluation plans retain exact fixture identities for preflight loading", JSON.stringify(L.learnEvaluatePayload(evaluateDraft).replayRunIds) === JSON.stringify(["run-1"]));
check("replay evidence: exact finalized event status admits its matching journal", L.learnReplayEvidenceJournal({ journal: { run_id: "run-1" } }, { run_id: "run-1", events: [], complete: true }, "run-1").journal.run_id === "run-1");
check("replay evidence: finalized status is independently checked before fixture retrieval", !L.learnFinalizedRunEvidence({ run_id: "run-1", events: [], complete: true }, "run-1").error && Boolean(L.learnFinalizedRunEvidence({ run_id: "run-1", events: [], complete: false }, "run-1").error));
check("replay evidence: active, mismatched, and malformed run status fail before evaluation", Boolean(L.learnReplayEvidenceJournal({ journal: { run_id: "run-1" } }, { run_id: "run-1", events: [], complete: false }, "run-1").error) && Boolean(L.learnReplayEvidenceJournal({ journal: { run_id: "run-2" } }, { run_id: "run-1", events: [], complete: true }, "run-1").error) && Boolean(L.learnReplayEvidenceJournal({ journal: { run_id: "run-1" } }, { run_id: "run-1", complete: true }, "run-1").error));
check("journal link: UTF-8 byte bounds and control characters fail closed", !L.learnEvaluatePayload({ ...evaluateDraft, runId: "é".repeat(200) }).payload && !L.learnEvaluatePayload({ ...evaluateDraft, runId: "run\n1" }).payload);

const promoteDraft = { runId: "run-1", parent: "event-1", approvedBy: "", requiredEffectId: "", acknowledge: true };
eq("promotion payload: envelope check starts without forging approval", L.learnPromotePayload(promoteDraft).payload, { run_id: "run-1", parent: "event-1" });
check("promotion payload: scoped approval requires attribution", !L.learnPromotePayload({ ...promoteDraft, requiredEffectId: "e".repeat(64) }).payload);
eq("promotion payload: exact server scope and approver form one non-transferable token", L.learnPromotePayload({ ...promoteDraft, requiredEffectId: "e".repeat(64), approvedBy: "ops:amjad" }).payload,
  { run_id: "run-1", parent: "event-1", approval: { effect_id: "e".repeat(64), approved_by: "ops:amjad" } });
check("promotion payload: malformed effect scopes fail closed", !L.learnPromotePayload({ ...promoteDraft, requiredEffectId: "not-a-digest", approvedBy: "ops" }).payload);
check("promotion refusal: only the server's explicit candidate scope is extracted", L.learnPromotionRequiredEffect({ body: { message: `present an approval token scoped to effect id ${"f".repeat(64)} — exact` } }) === "f".repeat(64) && !L.learnPromotionRequiredEffect({ body: { message: "approval required" } }));

const rollbackDraft = { runId: "run-1", parent: "", cause: "Regression in run-9", acknowledge: true };
eq("rollback payload: cause and journal link are exact", L.learnRollbackPayload(rollbackDraft).payload, { run_id: "run-1", cause: "Regression in run-9" });
check("rollback payload: blank or oversized causes fail closed", !L.learnRollbackPayload({ ...rollbackDraft, cause: " " }).payload && !L.learnRollbackPayload({ ...rollbackDraft, cause: "x".repeat(4097) }).payload);
check("routes: candidate ids are encoded and actions stay on frozen lifecycle endpoints", L.learnActionPath("candidate/unsafe", "promote") === "/learn/candidates/candidate%2Funsafe/promote");

const baseState = { versions, versionsError: null, drafts: Object.create(null), action: null, notice: null };
check("actions: created candidates offer only evaluation", L.learnActionHtml(baseState, prompt, null).includes('data-learn-action="evaluate"') && !L.learnActionHtml(baseState, prompt, null).includes('data-learn-action="promote"'));
check("actions: evaluated candidates can either ask the deployment envelope or re-evaluate", L.learnActionHtml(baseState, memory, versions[1]).includes('data-learn-action="promote"') && L.learnActionHtml(baseState, memory, versions[1]).includes('data-learn-action="evaluate"') && L.learnActionHtml(baseState, memory, versions[1]).includes("Evaluate again"));
check("actions: rollback exists only while the exact candidate serves", L.learnActionHtml(baseState, policy, versions[0]).includes('data-learn-action="rollback"') && !L.learnActionHtml(baseState, { ...policy, candidate: { ...policy.candidate, candidate_id: "9".repeat(64) } }, versions[0]).includes('data-learn-action="rollback"'));
check("actions: missing pointer evidence blocks promotion", L.learnActionHtml({ ...baseState, versionsError: "offline" }, memory, null).includes("Serving evidence required"));
check("actions: rolled-back candidates cannot be silently reused", L.learnActionHtml(baseState, tool, null).includes("new candidate"));
check("detail: immutable identity, evidence rail, serving state, recovery, action, and raw proof stay together", L.learnDetailHtml(baseState, policy).includes("Candidate lifecycle") && L.learnDetailHtml(baseState, policy).includes("Serving pointer") && L.learnDetailHtml(baseState, policy).includes("Recovery receipt") && L.learnDetailHtml(baseState, policy).includes("bounded immutable candidate record"));
check("detail: raw evidence is bounded", L.learnDetailHtml(baseState, { ...prompt, candidate: { ...prompt.candidate, content: { ...prompt.candidate.content, prompt: "x".repeat(100000) } } }).length < 50000);

check("compatibility: route-less servers get a capability explanation", L.learnErrorHtml(404, { raw: "not found" }).includes("does not expose"));
check("errors: real server messages are escaped", L.learnErrorHtml(500, { message: "failed <unsafe>" }).includes("&lt;unsafe&gt;"));
check("errors: evaluator setup failures become an operator action without leaking server internals", L.learnRefusalMessage({ body: { message: "no candidate evaluator is configured (`ServerConfig::with_candidate_evaluator`)" } }).includes("Configure a candidate evaluator") && !L.learnRefusalMessage({ body: { message: "no candidate evaluator is configured (`ServerConfig::with_candidate_evaluator`)" } }).includes("ServerConfig"));
check("errors: unfamiliar server refusals remain visible but bounded", L.learnRefusalMessage({ body: { message: "x".repeat(10000) } }).length === L.LEARN_TEXT_LIMIT);
check("conflicts: only the no-evaluator capability refusal stays on the current snapshot", !L.learnConflictNeedsRefresh({ status: 409, body: { message: "no candidate evaluator is configured" } }) && L.learnConflictNeedsRefresh({ status: 409, body: { message: "a concurrent transition won" } }));
check("refresh outcomes: confirmed receipts never claim an authoritative pointer after a failed version read", L.learnTransitionMessage("promote", "confirmed", { candidates: true, versions: false }).includes("could not be verified") && !L.learnTransitionMessage("promote", "confirmed", { candidates: true, versions: false }).includes("authoritative"));
check("refresh outcomes: uncertain and conflict paths disclose incomplete rereads", L.learnTransitionMessage("evaluate", "ambiguous", { candidates: false, versions: false }).includes("could not fully re-read") && L.learnTransitionMessage("rollback", "conflict", { candidates: true, versions: false }, "settled elsewhere").includes("could not fully refresh"));
check("refresh outcomes: detail notices use a neutral heading when refresh is incomplete", L.learnDetailHtml({ ...baseState, notice: "Refresh failed." }, policy).includes("Transition status.") && !L.learnDetailHtml({ ...baseState, notice: "Refresh failed." }, policy).includes("Durable state refreshed."));
check("markup: Learning is a first-class sidebar workspace with a labelled heading", html.includes('id="btn-learn-open"') && html.includes('id="learn-view"') && html.includes('aria-labelledby="learn-title"'));
check("markup: the evidence rail names the real governance path", html.includes("Immutable proposal") && html.includes("Journaled evidence gate") && html.includes("Reversible serving pointer"));
check("accessibility: candidate list is a labelled listbox with a dedicated action announcer", html.includes('id="learn-list" role="listbox"') && html.includes('id="learn-announcer" role="status" aria-live="polite"'));
check("interaction: search is debounced and candidate selection is keyboard operable", html.includes("store.learnSearchTimer = setTimeout") && html.includes('["ArrowDown", "ArrowUp", "ArrowLeft", "ArrowRight", "Home", "End"]'));
check("interaction: candidate switching locks while a governed action is in flight", html.includes('${state.action ? " disabled" : ""} aria-label="Inspect'));
check("lifecycle: server and tenant changes invalidate candidate reads and actions", html.includes("store.learnRequest += 1;") && html.includes("learnRequestCurrent(request, connection)"));
check("lifecycle: refreshed receipts cannot attach to a replaced connection or stale load", html.includes("connection.baseUrl !== store.conn.baseUrl") && html.includes("learnObject(refresh).stale"));
check("lifecycle: uncertain receipts force an authoritative reread", html.includes("The transition receipt was not confirmed") && html.includes("await learnLoad(true);"));
check("lifecycle: confirmed conflicts refresh settled state while no-evaluator stays actionable", html.includes("learnConflictNeedsRefresh(error)") && html.includes('learnTransitionMessage(action, "conflict"'));
const eventRead = html.indexOf('const eventFeed = await apiForConnection(connection, "GET", `/runs/${encodeURIComponent(runId)}/events`)');
const fixtureRead = html.indexOf('const fixture = await apiForConnection(connection, "GET", `/runs/${encodeURIComponent(runId)}/fixture`)');
check("lifecycle: finalized status is read before the exact fixture and before posting journals", eventRead >= 0 && fixtureRead > eventRead && html.includes("learnReplayEvidenceJournal(fixture, eventFeed, runId)") && html.includes("result.payload.request.replay_evidence = evidence"));
check("lifecycle: malformed pointer evidence blocks unsafe promotion and rollback", html.includes("learnVersionSnapshot") && html.includes("pointer-changing actions are blocked"));
check("responsive: learning layout, toolbar, dossier, and action forms collapse intentionally", html.includes(".learn-layout { grid-template-columns: 1fr; }") && html.includes(".learn-toolbar, .learn-dossier, .learn-action-grid { grid-template-columns: 1fr; }"));
check("design: candidate lifecycle uses text and shape, not color alone", html.includes('.learn-status::before') && html.includes("learnStatusLabel(status)"));

if (failed) {
  console.error(`\nFAIL: ${failed} failed, ${passed} passed`);
  process.exit(1);
}
console.log(`\nPASS: ${passed} governed-learning assertions`);
