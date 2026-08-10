#!/usr/bin/env node
/* Node unit tests for the Flight Recorder timeline helpers embedded in
 * studio/index.html. The <script> block is extracted verbatim, the final
 * browser bootstrap (`init();`) is stripped, and the pure helpers are
 * exercised under `vm` — no browser, no dependencies.
 *
 *   node studio/test-recorder.mjs
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
globalThis.__rec = {
  recSortEvents, recGroups, recLanes, recLaneOf, recCausalChain,
  recKindColor, recKindShort, recPayloadHtml, recDetailHtml, recMarkerHtml,
  recEffectHtml, recFormatUsd, recFormatTokens, REC_KIND_COLORS, REC_EFFECT_INFO,
  recInlineValue, recIssueMessage, recIsSuspensionCheckpoint, recInvestigation,
  recStoryStepHtml, recFinalized, recInvestigationHtml,
  recReplayBannerHtml, recApiErrorBannerHtml, recTotalsHtml,
  recExactUnsigned, recRecordedLatency, recComparisonEvidenceState, recJournalSignals,
  recComparisonMetric, recComparisonLatencyMetric, recComparisonReport, recComparisonMetricHtml, recComparisonReportHtml,
  recReviewGeneratedId, recReviewDraft, recReviewTextValid, recReviewValidate, recReviewPacket,
  recReviewFilename, recReviewHtml, recReviewInput, REC_REVIEW_CRITERIA, REC_REVIEW_VERDICTS, store,
  recCompareRows, recCmpEventHtml, recCompareHtml, recCompareInputsCurrent, AGENT_NUMBER_TOKENS,
};`, sandbox, { filename: "index.html<script>" });

const R = sandbox.__rec;

/* -- fixture: a journaled two-step run in the golden RunEvent wire shape -- */

const RUN = "019157c4-6f1f-7a3b-8c2d-9e4f5a6b7c8d";
const ev = (seq, kind, extra = {}) => ({
  id: `${RUN}:${seq}`, run_id: RUN, thread_id: "thread-42", node_id: null,
  seq, kind, effect: "pure", input: null, output: null,
  latency_ms: null, tokens: null, cost_usd: null, status: "ok",
  parent: null, recorded_at: "2026-08-07T10:00:00Z", ...extra,
});

const journal = [
  ev(0, "resume", { input: { kind: "inline", value: { checkpoint_id: "cp-9" } } }),
  ev(1, "super_step_start", { input: { kind: "inline", value: { activated: ["first"] } } }),
  ev(2, "node_input", { node_id: "first", parent: `${RUN}:1`,
    input: { kind: "inline", value: { log: [] } } }),
  ev(3, "node_output", { node_id: "first", parent: `${RUN}:2`, latency_ms: 4,
    output: { kind: "inline", value: { log: ["first"] } } }),
  ev(4, "routing_decision", { parent: `${RUN}:1`,
    output: { kind: "inline", value: { next: ["second"] } } }),
  ev(5, "checkpoint_written", { parent: `${RUN}:4`,
    output: { kind: "inline", value: { checkpoint_id: "cp-10", step: 1 } } }),
  ev(6, "super_step_end", { parent: `${RUN}:1` }),
  ev(7, "super_step_start", { input: { kind: "inline", value: { activated: ["agent"] } } }),
  ev(8, "node_input", { node_id: "agent", parent: `${RUN}:7` }),
  ev(9, "model_call", { node_id: "agent", effect: "non_idempotent", parent: `${RUN}:8`,
    latency_ms: 137, tokens: { prompt_tokens: 128, completion_tokens: 32, total_tokens: 160 },
    cost_usd: 0.00042,
    input: { kind: "inline", value: { messages: [{ role: "user", content: "ping" }], tools: [] } },
    output: { kind: "artifact", value: { sha256: "9b71d224bd62f3785d96d46ad3ea3d73319bfbc2890caadae2dff72519673ca7", bytes: 8192 } } }),
  ev(10, "tool_call", { node_id: "tools", effect: "non_idempotent", parent: `${RUN}:9`, latency_ms: 11,
    input: { kind: "inline", value: { name: "echo", args: { text: "<b>ping</b>" } } },
    output: { kind: "inline", value: { text: "ping" } } }),
  ev(11, "node_output", { node_id: "agent", parent: `${RUN}:8`, latency_ms: 152 }),
  ev(12, "routing_decision", { parent: `${RUN}:7` }),
  ev(13, "checkpoint_written", { parent: `${RUN}:12`, status: "error",
    output: { kind: "inline", value: { error: "disk full" } } }),
  ev(14, "super_step_end", { parent: `${RUN}:7` }),
  // Defensive case: a partial server may omit every optional field.
  { kind: "interrupt", status: "interrupted" },
];

/* -- tiny assert harness -------------------------------------------------- */

let passed = 0, failed = 0;
function check(name, cond, detail) {
  if (cond) { passed++; console.log(`ok   ${name}`); }
  else { failed++; console.log(`FAIL ${name}${detail ? " — " + detail : ""}`); }
}
function eq(name, got, want) {
  check(name, JSON.stringify(got) === JSON.stringify(want),
    `got ${JSON.stringify(got)}, want ${JSON.stringify(want)}`);
}

/* -- ordering -------------------------------------------------------------- */

{
  const shuffled = [journal[9], journal[0], journal[5], journal[2]];
  eq("recSortEvents orders by seq", R.recSortEvents(shuffled).map((e) => e.seq), [0, 2, 5, 9]);
  const sorted = R.recSortEvents(journal);
  eq("recSortEvents keeps seqless event last (array-order fallback)",
    sorted[sorted.length - 1].kind, "interrupt");
  check("recSortEvents tolerates non-array input", R.recSortEvents(null).length === 0);
}

/* -- lanes ----------------------------------------------------------------- */

{
  eq("recLanes: run lane first, nodes in first-appearance order",
    R.recLanes(R.recSortEvents(journal)), ["run", "first", "agent", "tools"]);
  eq("recLaneOf maps missing node_id to the run lane",
    R.recLaneOf({ kind: "interrupt" }), "run");
}

/* -- super-step grouping ---------------------------------------------------- */

{
  const groups = R.recGroups(R.recSortEvents(journal));
  eq("recGroups: prelude + two super-steps", groups.map((g) => g.n), [0, 1, 2]);
  eq("recGroups: prelude holds the resume event",
    groups[0].events.map((e) => e.kind), ["resume"]);
  check("recGroups: super_step_start opens its own group",
    groups[1].events[0].kind === "super_step_start" && groups[1].start.seq === 1);
  eq("recGroups: group 2 spans the agent step",
    groups[2].events.map((e) => e.kind),
    ["super_step_start", "node_input", "model_call", "tool_call",
     "node_output", "routing_decision", "checkpoint_written", "super_step_end", "interrupt"]);
}

/* -- causal chain ----------------------------------------------------------- */

{
  const sorted = R.recSortEvents(journal);
  const chain = R.recCausalChain(sorted, `${RUN}:10`);
  check("causal chain of tool_call walks up to its super-step",
    chain.has(`${RUN}:10`) && chain.has(`${RUN}:9`) && chain.has(`${RUN}:8`) && chain.has(`${RUN}:7`));
  check("causal chain excludes unrelated events", !chain.has(`${RUN}:3`) && !chain.has(`${RUN}:13`));
  const cyclic = [
    { id: "a", parent: "b", kind: "node_input" },
    { id: "b", parent: "a", kind: "node_output" },
  ];
  const cyc = R.recCausalChain(cyclic, "a");
  check("causal chain survives a parent cycle", cyc.has("a") && cyc.has("b") && cyc.size === 2);
  check("causal chain of unknown id is empty", R.recCausalChain(sorted, "nope:0").size === 0);
}

/* -- markers ---------------------------------------------------------------- */

{
  const marker = R.recMarkerHtml(journal[9]);
  check("marker carries data-eid", marker.includes(`data-eid="${RUN}:9"`));
  check("marker is colored by kind", marker.includes(`--kcol:${R.REC_KIND_COLORS.model_call}`));
  const errMarker = R.recMarkerHtml(journal[13]);
  check("error status marks the chip", errMarker.includes("st-error"));
  const seqless = R.recMarkerHtml({ kind: "interrupt", status: "interrupted" });
  check("seqless event renders without crashing",
    seqless.includes("st-interrupted") && seqless.includes("intr"));
  check("unknown kind falls back to the faint color",
    R.recKindColor("future_kind") === "#6b6252");
  check("every golden kind has a color and a short label",
    ["super_step_start","super_step_end","node_input","node_output","model_call","tool_call",
     "remote_call","wasm_call","interrupt","resume","routing_decision","checkpoint_written"]
      .every((k) => R.REC_KIND_COLORS[k] && typeof R.recKindShort(k) === "string" && R.recKindShort(k).length <= 6));
}

/* -- payloads --------------------------------------------------------------- */

{
  const inline = R.recPayloadHtml(journal[10].input);
  check("inline payload renders the value", inline.includes("echo"));
  check("inline payload escapes HTML", inline.includes("&lt;b&gt;") && !inline.includes("<b>"));
  const artifact = R.recPayloadHtml(journal[9].output);
  check("artifact ref shows sha256 and bytes",
    artifact.includes("9b71d224bd62f3785d96d46ad3ea3d73319bfbc2890caadae2dff72519673ca7") &&
    artifact.includes("8192"));
  check("missing payload renders a dash", R.recPayloadHtml(null).includes("—"));
  check("unknown future tag renders raw JSON",
    R.recPayloadHtml({ kind: "external", value: { uri: "s3://x" } }).includes("s3://x"));
}

/* -- detail panel ------------------------------------------------------------- */

{
  const detail = R.recDetailHtml(journal[9]);
  check("detail shows the effect badge", detail.includes("eff-non_idempotent"));
  check("detail shows the effect blurb", detail.includes("never silently retried"));
  check("detail renders the parent as a jump link", detail.includes(`data-eid="${RUN}:8"`));
  check("detail shows tokens", detail.includes("128 prompt + 32 completion = 160 total"));
  check("detail shows cost", detail.includes("$0.00042"));
  check("detail shows latency", detail.includes("137 ms"));
  const noParent = R.recDetailHtml(journal[1]);
  check("parentless event renders a dash", noParent.includes("—"));
  const unknownEff = R.recEffectHtml("future_effect");
  check("unknown effect falls back to a neutral badge", unknownEff.includes("badge pending"));
  check("every frozen effect class has a badge tone",
    ["pure","read_only","idempotent","compensatable","non_idempotent"]
      .every((e) => R.REC_EFFECT_INFO[e] && R.REC_EFFECT_INFO[e].tone === `eff-${e}`));
}

/* -- formatting --------------------------------------------------------------- */

eq("recFormatUsd micro-cost", R.recFormatUsd(0.00042), "$0.00042");
eq("recFormatUsd larger cost", R.recFormatUsd(1.5), "$1.5000");
eq("recFormatUsd missing", R.recFormatUsd(null), "—");
eq("recFormatTokens missing", R.recFormatTokens(null), "—");

/* -- causal investigation story -------------------------------------------- */

{
  const errorJournal = journal.slice(0, -1);
  const story = R.recInvestigation(errorJournal, true);
  eq("investigation: error evidence is not promoted to run outcome", story.state, "error");
  eq("investigation: first error event is the causal issue", story.issue.seq, 13);
  eq("investigation: last successful checkpoint precedes the issue", story.recovery.seq, 5);
  eq("investigation: highest repeat risk prefers non-idempotent", story.highestRisk.seq, 9);
  check("investigation: error payload becomes the human cause",
    story.causeDetail === "disk full");
  check("investigation: effect count and warning are explicit",
    story.riskTitle === "2 non-idempotent" && story.riskDetail.includes("must never be repeated silently"));

  const storyHtml = R.recInvestigationHtml(errorJournal, true);
  check("investigation html: accessible labelled story region",
    storyHtml.includes('aria-labelledby="rec-story-title"') &&
    storyHtml.includes('aria-label="Causal investigation summary"'));
  check("investigation html: issue, recovery, and risk link to evidence",
    storyHtml.includes(`data-story-eid="${RUN}:13"`) &&
    storyHtml.includes(`data-story-eid="${RUN}:5"`) &&
    storyHtml.includes(`data-story-eid="${RUN}:9"`));
  check("investigation html: buttons have descriptive accessible names",
    storyHtml.includes('aria-label="First error event: checkpoint_written · seq 13. View evidence"'));
  check("investigation html: a finalized journal can become proposal evidence",
    storyHtml.includes("Use this finalized journal as attributable evidence") && storyHtml.includes("data-propose-run"));
  check("investigation html: an in-flight journal cannot become proposal evidence",
    !R.recInvestigationHtml(errorJournal, false).includes("data-propose-run"));
  check("investigation html: only literal finalized evidence exposes proposal handoff",
    R.recFinalized(true) && !R.recFinalized() && !R.recFinalized(null) && !R.recFinalized("true") &&
    !R.recInvestigationHtml(errorJournal, "true").includes("data-propose-run"));
}

{
  const clean = journal.slice(0, 13);
  const story = R.recInvestigation(clean, true);
  eq("investigation: terminal journal without issue stays outcome-neutral", story.state, "complete");
  check("investigation: clean journal has no causal issue or recovery claim",
    story.issue === null && story.recovery === null && story.causeTitle === "No recorded issue" &&
    story.title === "No issue recorded" && story.summary.includes("not proof of run success"));
  check("investigation: repeat-sensitive effects remain visible on success",
    story.highestRisk.seq === 9 && story.riskTitle === "2 non-idempotent");
}

{
  const partial = R.recInvestigation(journal.slice(0, 7), false);
  eq("investigation: incomplete healthy journal is in flight", partial.state, "running");
  check("investigation: partial copy does not imply a final outcome",
    partial.summary.includes("still arriving") && partial.issue === null && partial.recovery === null);

  const pausedEvents = journal.slice(0, 13).concat([
    { id: "pause:13", seq: 13, kind: "interrupt", status: "interrupted", effect: "pure" },
    ev(14, "checkpoint_written", {
      output: { kind: "inline", value: { checkpoint_id: "cp-suspend", step: 2, suspension: true } },
    }),
  ]);
  const paused = R.recInvestigation(pausedEvents, true);
  eq("investigation: interrupt is distinguished from failure", paused.state, "paused");
  eq("investigation: paused run uses subsequent suspension checkpoint", paused.recovery.seq, 14);
  check("investigation: interruption explains the next operator decision",
    paused.causeDetail.includes("paused for a decision") && paused.summary.includes("resume boundary"));
  check("investigation: only explicit suspension checkpoints prove resume",
    R.recIsSuspensionCheckpoint(pausedEvents[pausedEvents.length - 1]) &&
    !R.recIsSuspensionCheckpoint(journal[5]));

  const unresolvedWithoutCheckpoint = R.recInvestigation([
    ev(0, "interrupt", { status: "interrupted" }),
  ], true);
  check("investigation: pause without suspension evidence does not claim resumability",
    unresolvedWithoutCheckpoint.recovery === null &&
    unresolvedWithoutCheckpoint.recoveryTitle === "No suspension checkpoint" &&
    unresolvedWithoutCheckpoint.summary.includes("Do not assume it can resume"));

  const resumed = R.recInvestigation([
    ev(0, "interrupt", { status: "interrupted" }),
    ev(1, "checkpoint_written", {
      output: { kind: "inline", value: { checkpoint_id: "cp-suspend", suspension: true } },
    }),
    ev(2, "resume"),
  ], true);
  check("investigation: a later resume resolves an earlier interruption",
    resumed.state === "complete" && resumed.issue === null && resumed.recovery === null);

  const containedErrorThenPause = R.recInvestigation([
    ev(0, "tool_call", { status: "error", effect: "idempotent" }),
    ev(1, "interrupt", { status: "interrupted" }),
    ev(2, "checkpoint_written", {
      output: { kind: "inline", value: { checkpoint_id: "cp-after-error", suspension: true } },
    }),
  ], true);
  check("investigation: unresolved pause takes priority over contained error evidence",
    containedErrorThenPause.state === "paused" && containedErrorThenPause.issue.seq === 1 &&
    containedErrorThenPause.recovery.seq === 2 && containedErrorThenPause.relatedError.seq === 0);
  const mixedHtml = R.recInvestigationHtml([
    ev(0, "tool_call", { status: "error", effect: "idempotent" }),
    ev(1, "interrupt", { status: "interrupted" }),
    ev(2, "checkpoint_written", {
      output: { kind: "inline", value: { checkpoint_id: "cp-after-error", suspension: true } },
    }),
  ], true);
  check("investigation: contained error remains separately linked from pause story",
    mixedHtml.includes("Related evidence") && mixedHtml.includes(`data-story-eid="${RUN}:0"`));
}

{
  const failedWithoutCheckpoint = [ev(0, "tool_call", {
    status: "error", effect: "idempotent",
    output: { kind: "inline", value: { message: "provider unavailable" } },
  })];
  const story = R.recInvestigation(failedWithoutCheckpoint, true);
  check("investigation: missing recovery evidence is honest",
    story.recovery === null && story.recoveryTitle === "No prior checkpoint" &&
    story.recoveryDetail.includes("operator judgment"));
  eq("investigation: safe effects do not become repeat risk", story.riskTitle, "No repeat-sensitive events");

  const unknownCheckpoint = [
    ev(0, "checkpoint_written", { status: undefined }),
    ev(1, "node_output", { status: "error" }),
  ];
  check("investigation: unknown checkpoint status is not called safe",
    R.recInvestigation(unknownCheckpoint, true).recovery === null);

  const futureEffect = R.recInvestigation([ev(0, "remote_call", { effect: "future_write" })], true);
  check("investigation: future effect classes remain unknown, never safe",
    futureEffect.unclassified.length === 1 && futureEffect.highestRisk.seq === 0 &&
    futureEffect.riskTitle === "1 unclassified" && futureEffect.riskDetail.includes("Do not infer"));
  const missingEffect = R.recInvestigation([{ id: "partial:0", seq: 0, kind: "tool_call", status: "ok" }], true);
  check("investigation: missing effect classes remain unknown, never safe",
    missingEffect.unclassified.length === 1 && missingEffect.riskTitle === "1 unclassified");

  const mixedRisk = R.recInvestigation([
    ev(0, "tool_call", { effect: "non_idempotent" }),
    ev(1, "remote_call", { effect: "compensatable" }),
    ev(2, "wasm_call", { effect: "future_write" }),
  ], true);
  check("investigation: mixed risk summary discloses every classification",
    mixedRisk.riskTitle === "1 non-idempotent · 1 compensatable · 1 unclassified" &&
    mixedRisk.riskDetail.includes("never be repeated silently") &&
    mixedRisk.riskDetail.includes("declared compensation path") && mixedRisk.riskDetail.includes("Do not infer safety"));
}

{
  const hostile = [ev(0, "tool_call", {
    status: "error", node_id: "<img src=x>",
    output: { kind: "inline", value: { error: "<script>alert(1)</script>" } },
  })];
  const html = R.recInvestigationHtml(hostile, true);
  check("investigation html: journal evidence is escaped",
    html.includes("&lt;img src=x&gt;") && html.includes("&lt;script&gt;alert(1)&lt;/script&gt;") &&
    !html.includes("<img src=x>") && !html.includes("<script>alert(1)</script>"));
  check("investigation helpers tolerate empty input",
    R.recInvestigation(null, false).state === "running" && R.recInvestigation([], true).state === "complete");
}

check("investigation markup: updates are announced politely",
  html.includes('<div id="rec-investigation" aria-live="polite"></div>'));
check("investigation interaction: evidence buttons share delegated selection",
  html.includes('$("rec-investigation").addEventListener("click", recClick)') &&
  html.includes('e.target.closest("[data-story-eid]")'));
check("investigation interaction: proposal handoff carries only the exact completed recorder run",
  html.includes('e.target.closest("[data-propose-run]")') &&
  html.includes("complete: recFinalized(res.complete)") &&
  html.includes("store.recorder && store.recorder.complete ? store.recorder.runId") &&
  html.includes("learnFoundryOpen(runId)"));
check("investigation responsive: story and evidence panel stack before phone width",
  html.includes('.rec-story-spine { grid-template-columns: repeat(2, minmax(0, 1fr)); }') &&
  html.includes('.rec-split { flex-direction: column; }') &&
  html.includes('.rec-story-spine { grid-template-columns: 1fr; }'));
check("investigation accessibility: small labels use the higher-contrast dim token",
  /\.rec-story-label\s*\{[\s\S]*?color: var\(--text-dim\)/.test(html));
check("investigation accessibility: story navigation focuses a labelled evidence region",
  html.includes('detail.setAttribute("role", "region")') &&
  html.includes('detail.setAttribute("aria-label"') && html.includes('detail.focus({ preventScroll: true })'));
check("investigation accessibility: stacked evidence is scrolled into view",
  html.includes('detail.scrollIntoView({ block: "nearest", behavior: "auto" })'));

/* -- replay banner (POST /runs/replay) --------------------------------------
 * Response shape: {run_id, verified, expected_events, actual_events,
 * first_divergence}. Fixture-shaped JSON — the server wave has not landed in
 * this workspace, so these fixtures are the verification stand-in. */

{
  const ok = R.recReplayBannerHtml({
    run_id: RUN, verified: true, expected_events: 16, actual_events: 16, first_divergence: null,
  });
  check("replay banner verified: ok tone + count", ok.includes("rec-banner ok") && ok.includes("16"));
  check("replay banner verified: no jump link", !ok.includes("data-jump-seq"));

  const bad = R.recReplayBannerHtml({
    run_id: RUN, verified: false, expected_events: 16, actual_events: 11, first_divergence: 9,
  });
  check("replay banner mismatch: err tone + both counts",
    bad.includes("rec-banner err") && bad.includes("16") && bad.includes("11"));
  check("replay banner mismatch: divergence is a jump link", bad.includes('data-jump-seq="9"'));

  const noDiv = R.recReplayBannerHtml({ verified: false, expected_events: 3, actual_events: 3 });
  check("replay banner mismatch without divergence: no jump link", !noDiv.includes("data-jump-seq"));

  const partial = R.recReplayBannerHtml({});
  check("replay banner tolerates an empty/partial response",
    partial.includes("rec-banner err") && partial.includes("?"));
}

/* -- replay/diff error mapping (404 / 409 / 422 / route-missing / other) ----- */

{
  const noRoute = R.recApiErrorBannerHtml("POST /runs/replay", 404, null);
  check("non-JSON 404 is the route-missing note",
    noRoute.includes("rec-banner warn") && noRoute.includes("POST /runs/replay"));

  const unknown = R.recApiErrorBannerHtml("POST /runs/replay", 404,
    { error: "not_found", message: "run `abc` not found" });
  check("JSON 404 is the unknown-run note",
    unknown.includes("Unknown run (404)") && unknown.includes("run `abc` not found") && !unknown.includes("route yet"));

  const noJournal = R.recApiErrorBannerHtml("GET /runs/diff", 409,
    { error: "conflict", message: "run `abc` has no persisted journal" });
  check("409 is the no-persisted-journal note",
    noJournal.includes("No persisted journal (409)") && noJournal.includes("no persisted journal"));

  const noGraph = R.recApiErrorBannerHtml("POST /runs/replay", 422,
    { error: "unprocessable", message: "graph `react_agent` is not registered" });
  check("422 is the graph-not-registered note",
    noGraph.includes("Graph not registered (422)") && noGraph.includes("not registered"));

  const boom = R.recApiErrorBannerHtml("POST /runs/replay", 500, { error: "internal_error", message: "boom" });
  check("other statuses render the message verbatim", boom.includes("boom"));

  const net = R.recApiErrorBannerHtml("GET /runs/diff", 0, { error: "network", message: "connection refused" });
  check("network failure renders its message", net.includes("connection refused"));

  const escaped = R.recApiErrorBannerHtml("POST /runs/replay", 404,
    { error: "not_found", message: "run `<script>` not found" });
  check("error messages are HTML-escaped", escaped.includes("&lt;script&gt;") && !escaped.includes("<script>`"));
}

/* -- run comparison: row alignment (BranchDiff shape from rusty-core replay.rs) */

const cev = (seq, kind, extra = {}) => ({
  id: `cmp:${seq}`, seq, kind, node_id: null, status: "ok", ...extra,
});
const CMP_BASE = [
  cev(0, "super_step_start"), cev(1, "node_input", { node_id: "first" }),
  cev(2, "node_output", { node_id: "first" }), cev(3, "super_step_end"),
  cev(4, "super_step_start"), cev(5, "model_call", { node_id: "agent" }),
];
const CMP_BRANCH = [
  cev(0, "super_step_start"), cev(1, "node_input", { node_id: "first" }),
  cev(2, "node_output", { node_id: "first" }), cev(3, "super_step_end"),
  cev(4, "super_step_start"),               // same seq as base, different evidence past it
  cev(6, "tool_call", { node_id: "tools" }), // seq 6 exists only on the branch
];
const CMP_DIFF = {
  first_divergent_seq: 4,
  added: [CMP_BRANCH[4], CMP_BRANCH[5]],
  removed: [CMP_BASE[4], CMP_BASE[5]],
  step_diffs: [],
  base_totals: { events: 6, tokens: { prompt_tokens: 128, completion_tokens: 32, total_tokens: 160 }, cost_usd: 0.00042 },
  branch_totals: { events: 6, tokens: { prompt_tokens: 64, completion_tokens: 16, total_tokens: 80 }, cost_usd: 0.00021 },
};

{
  const unsafe = {};
  Object.defineProperty(unsafe, R.AGENT_NUMBER_TOKENS, {
    value: { events: "18446744073709551615" }, enumerable: false,
  });
  check("comparison numbers: safe and raw u64 values remain exact",
    R.recExactUnsigned({ events: 6 }, "events") === 6n &&
    R.recExactUnsigned(unsafe, "events") === 18446744073709551615n &&
    R.recExactUnsigned({ events: 1.5 }, "events") === null);
}

{
  const completeBase = { run_id: "base", complete: true, events: CMP_BASE };
  const completeBranch = { run_id: "candidate", complete: true, events: CMP_BRANCH };
  eq("comparison evidence: finalized journals and matching atomic totals are exact",
    R.recComparisonEvidenceState(CMP_DIFF, completeBase, completeBranch, "base", "candidate").state, "finalized");
  eq("comparison evidence: matching unfinished journals stay live rather than final",
    R.recComparisonEvidenceState(CMP_DIFF, { ...completeBase, complete: false }, completeBranch, "base", "candidate").state, "live");
  eq("comparison evidence: a revision advance cannot masquerade as the atomic diff",
    R.recComparisonEvidenceState(CMP_DIFF, { ...completeBase, events: CMP_BASE.concat(cev(7, "resume")) }, completeBranch, "base", "candidate").state, "mismatch");
  eq("comparison evidence: unavailable timelines preserve divergence-only truth",
    R.recComparisonEvidenceState(CMP_DIFF, null, completeBranch, "base", "candidate").state, "partial");
  check("comparison evidence: response identities and boolean completeness are exact-contract gates",
    R.recComparisonEvidenceState(CMP_DIFF, { ...completeBase, run_id: "other" }, completeBranch, "base", "candidate").reason === "invalid_envelope" &&
    R.recComparisonEvidenceState(CMP_DIFF, completeBase, { ...completeBranch, complete: "true" }, "base", "candidate").usableEvents === false);
}

{
  const base = CMP_BASE.map((event, index) => ({ ...event, effect: index === 5 ? "non_idempotent" : "pure", latency_ms: index + 1 }));
  const branch = CMP_BRANCH.map((event, index) => ({ ...event, effect: index === 5 ? "read_only" : "pure", latency_ms: index + 2,
    status: index === 5 ? "error" : "ok" }));
  const diff = {
    ...CMP_DIFF,
    step_diffs: [{ step: 2, channels: [{ channel: "answer", base: "a", branch: "b" }, { channel: "audit<log>", base: null, branch: [] }] }],
  };
  const state = { state: "finalized", label: "finalized evidence", usableEvents: true };
  const report = R.recComparisonReport("base-full", "candidate-full", base, branch, diff, state);
  check("comparison report: resource deltas are descriptive, never a quality verdict",
    report.metrics[0].delta === "80 tokens less" && report.metrics[1].tone === "less" &&
    report.metrics[2].delta === "same recorded value" && report.metrics[3].delta.includes("latency more"));
  check("comparison report: structural and repeat-risk signals remain attributable",
    report.divergence === 4n && report.changedSteps === 1 && report.changedChannels === 2 &&
    report.baseSignals.repeatSensitive === 1 && report.branchSignals.errors === 1);
  const reportHtml = R.recComparisonReportHtml(report);
  check("comparison report html: decision balance names both runs and withholds an automatic winner",
    reportHtml.includes("base-full") && reportHtml.includes("candidate-full") &&
    reportHtml.includes("No automatic winner") && reportHtml.includes("not proof of a better answer"));
  check("comparison report html: metric values expose baseline/candidate semantics to assistive tech",
    reportHtml.includes('<span class="sr-only">Baseline: </span>') &&
    reportHtml.includes('<span class="sr-only">Candidate: </span>'));
  check("comparison report html: hostile channel identities are escaped inside bounded structural evidence",
    reportHtml.includes("audit&lt;log&gt;") && !reportHtml.includes("audit<log>"));
}

{
  const partial = R.recComparisonReport("base", "candidate", CMP_BASE, CMP_BRANCH, CMP_DIFF,
    { state: "partial", label: "divergence only", usableEvents: false });
  check("comparison report: partial timelines suppress latency and operational claims",
    partial.metrics[3].tone === "unknown" && partial.baseSignals === null && partial.branchSignals === null &&
    R.recComparisonReportHtml(partial).includes("Full journal signals unavailable"));
  const signals = R.recJournalSignals([
    { effect: "future_effect", status: "ok" }, { effect: "compensatable", status: "interrupted", kind: "interrupt" },
  ]);
  check("comparison signals: unknown effects and one interrupt event stay distinct",
    signals.unknownEffects === 1 && signals.repeatSensitive === 1 && signals.interruptions === 1);
  const partialLatency = R.recRecordedLatency([{ latency_ms: 4 }, { latency_ms: null }]);
  const fullLatency = R.recRecordedLatency([{ latency_ms: 4 }, { latency_ms: 6 }]);
  check("comparison latency: optional samples retain explicit observed-event coverage",
    partialLatency.total === 4n && partialLatency.observed === 1 && partialLatency.events === 2 && !partialLatency.complete &&
    fullLatency.total === 10n && fullLatency.complete);
  check("comparison latency: partial coverage stays neutral rather than creating a favorable rank",
    R.recComparisonLatencyMetric([{ latency_ms: 1 }, {}], [{ latency_ms: 9 }, {}], true).delta === "partial samples · not ranked");
}

{
  const state = { state: "finalized", label: "finalized evidence", usableEvents: true };
  const report = R.recComparisonReport("base-review", "candidate-review", CMP_BASE, CMP_BRANCH, CMP_DIFF, state);
  const review = R.recReviewDraft(report, "review-fixed");
  check("human review: draft binds exact run identities and a fixed three-part rubric",
    review.baseId === "base-review" && review.branchId === "candidate-review" &&
    R.REC_REVIEW_CRITERIA.map((criterion) => criterion.key).join(",") === "task_outcome,correctness,safety");
  const empty = R.recReviewValidate(review, report, true);
  check("human review: incomplete judgments fail every required decision family",
    !empty.ok && empty.errors.reviewer && empty.errors.verdict && empty.errors.acknowledged &&
    empty.errors["score-correctness-baseline"]);
  review.reviewer = "A. Reviewer";
  review.notes = "Reviewer pasted <secret-event-payload> here.";
  review.verdict = "candidate_preferred";
  review.acknowledged = true;
  for (const criterion of R.REC_REVIEW_CRITERIA) review.scores[criterion.key] = { baseline: "3", candidate: "5" };
  check("human review: an exact completed rubric is recordable", R.recReviewValidate(review, report, true).ok);
  check("human review: stale inputs and non-final evidence cannot be recorded",
    !R.recReviewValidate(review, report, false).ok &&
    !R.recReviewValidate(review, { ...report, evidenceState: { state: "live", usableEvents: true } }, true).ok);
  review.locked = true;
  review.recordedAt = "2026-08-10T00:30:00.000Z";
  const packet = R.recReviewPacket(review, report);
  check("human review packet: exact pair, explicit verdict, fixed scores, and honest boundary survive export",
    packet.format === "rusty.studio.run_review" && packet.version === 1 &&
    packet.run_pair.baseline_run_id === "base-review" && packet.run_pair.candidate_run_id === "candidate-review" &&
    packet.verdict === "candidate_preferred" && packet.rubric.length === 3 &&
    packet.boundary.storage === "page_memory" && packet.boundary.durable === false &&
    packet.boundary.promotion_gate === false && packet.boundary.lost_on_reload_or_thread_switch === true &&
    packet.boundary.raw_event_payloads_automatically_included === false && packet.boundary.reviewer_notes_exported_verbatim === true);
  check("human review packet: no journal payload is automatic, while reviewer notes remain exact and visibly warned",
    packet.notes === "Reviewer pasted <secret-event-payload> here." && packet.evidence.metrics.length === 4 &&
    packet.evidence.first_divergent_seq === "4");
  check("human review packet: export filename cannot inherit hostile path syntax",
    R.recReviewFilename({ reviewId: "../../bad review" }) === "rusty-run-review--..-bad-review.json");
}

{
  const state = { state: "finalized", label: "finalized evidence", usableEvents: true };
  const report = R.recComparisonReport("base", "candidate", CMP_BASE, CMP_BRANCH, CMP_DIFF, state);
  const review = R.recReviewDraft(report, "review-html");
  review.reviewer = '\"><img src=x onerror=alert(1)>';
  review.notes = "<script>alert(2)</script>";
  review.errors = { reviewer: "Reviewer <required>", acknowledged: "Acknowledge" };
  const draftHtml = R.recReviewHtml(report, review);
  check("human review html: hostile reviewer, notes, and errors are escaped",
    !draftHtml.includes("<img") && !draftHtml.includes("<script") && !draftHtml.includes("Reviewer <required>") &&
    draftHtml.includes("&lt;img") && draftHtml.includes("Reviewer &lt;required&gt;"));
  check("human review html: every score and repeated verdict action has a distinct accessible identity",
    draftHtml.includes("Baseline Task outcome score") && draftHtml.includes("Candidate Safety score") &&
    draftHtml.includes('name="cmp-review-verdict"') && draftHtml.includes("Baseline preferred") &&
    draftHtml.includes("Inconclusive"));
  check("human review html: notes disclose exact export while page-memory lifetime stays explicit",
    draftHtml.includes("Exported exactly as entered") && draftHtml.includes("Do not paste secrets") &&
    draftHtml.includes("lost on reload or thread switch"));
  check("human review html: acknowledgement errors are programmatically attached",
    draftHtml.includes('aria-invalid="true" aria-describedby="cmp-review-error-acknowledged"') &&
    draftHtml.includes('id="cmp-review-error-acknowledged"'));
  R.store.compare = { review };
  R.recReviewInput({ target: {
    value: "Corrected reviewer", checked: false,
    getAttribute: (name) => name === "data-rec-review-field" ? "reviewer" : null,
    hasAttribute: () => false,
  } });
  check("human review interaction: editing preserves the validated error and its association until resubmit",
    review.reviewer === "Corrected reviewer" && review.errors.reviewer === "Reviewer <required>" &&
    R.recReviewHtml(report, review).includes('aria-describedby="cmp-review-error-reviewer"'));
  review.reviewer = "Reviewer";
  review.notes = "Reviewed.";
  review.verdict = "tie";
  review.acknowledged = true;
  review.locked = true;
  review.recordedAt = "2026-08-10T00:30:00.000Z";
  review.errors = {};
  for (const criterion of R.REC_REVIEW_CRITERIA) review.scores[criterion.key] = { baseline: "4", candidate: "4" };
  const lockedHtml = R.recReviewHtml(report, review);
  check("human review html: locked docket keeps edit and bounded export controls explicit",
    lockedHtml.includes("Tie") && lockedHtml.includes("Edit review") && lockedHtml.includes("Export review packet") &&
    lockedHtml.includes("page memory only") && lockedHtml.includes("notes export exactly as entered"));
  const unavailable = R.recReviewHtml({ ...report, evidenceState: { state: "partial", usableEvents: false } }, review);
  check("human review html: partial evidence cannot expose a record action",
    unavailable.includes("Review is unavailable") && !unavailable.includes("Record page-memory verdict"));
}

{
  check("human review validation: visible exact values are never silently trimmed or control-normalized",
    !R.recReviewTextValid("reviewer\t", 128) && R.recReviewTextValid("line one\nline two", 2048, true));
  const generated = R.recReviewGeneratedId(null, 1000, 0.5);
  check("human review identity: bounded fallback remains filename-safe", /^review-[a-z0-9]+-[a-z0-9]+$/.test(generated));
}

{
  const rows = R.recCompareRows(CMP_BASE, CMP_BRANCH, CMP_DIFF);
  eq("compare rows: one row per seq in the union",
    rows.map((r) => r.seq), [0, 1, 2, 3, 4, 5, 6]);
  eq("compare rows: identical prefix is dimmed",
    rows.slice(0, 4).map((r) => r.cls), ["same", "same", "same", "same"]);
  eq("compare rows: shared seq at the fork is divergent", rows[4].cls, "divergent");
  eq("compare rows: base-only seq is removed", rows[5].cls, "removed");
  eq("compare rows: branch-only seq is added", rows[6].cls, "added");
  check("compare rows: removed row carries only the base event",
    rows[5].base && rows[5].base.kind === "model_call" && rows[5].branch === null);
  check("compare rows: added row carries only the branch event",
    rows[6].branch && rows[6].branch.kind === "tool_call" && rows[6].base === null);

  const hugeA = cev(Number("9007199254740992"), "model_call");
  const hugeB = cev(Number("9007199254740993"), "tool_call");
  Object.defineProperty(hugeA, R.AGENT_NUMBER_TOKENS, { value: { seq: "9007199254740992" }, enumerable: false });
  Object.defineProperty(hugeB, R.AGENT_NUMBER_TOKENS, { value: { seq: "9007199254740993" }, enumerable: false });
  const hugeDiff = { first_divergent_seq: Number("9007199254740992") };
  Object.defineProperty(hugeDiff, R.AGENT_NUMBER_TOKENS, { value: { first_divergent_seq: "9007199254740992" }, enumerable: false });
  const hugeRows = R.recCompareRows([hugeB, hugeA], [hugeA], hugeDiff);
  check("compare rows: adjacent legal u64 sequences remain distinct and ordered",
    hugeRows.length === 2 && hugeRows[0].seq === 9007199254740992n && hugeRows[1].seq === 9007199254740993n &&
    hugeRows[1].cls === "removed" && R.recSortEvents([hugeB, hugeA])[0] === hugeA);
}

{
  const identical = R.recCompareRows(CMP_BASE, CMP_BASE, { first_divergent_seq: null });
  check("compare rows: identical branches are all 'same'",
    identical.every((r) => r.cls === "same"));
}

{
  // Partial diff: no first_divergent_seq — divergence derived from presence.
  const rows = R.recCompareRows(CMP_BASE, CMP_BRANCH, {});
  eq("partial diff: presence-derived classes",
    rows.map((r) => r.cls), ["same", "same", "same", "same", "same", "removed", "added"]);
  const noDiff = R.recCompareRows(CMP_BASE, CMP_BRANCH, null);
  eq("null diff: same presence-derived classes",
    noDiff.map((r) => r.cls), ["same", "same", "same", "same", "same", "removed", "added"]);
  const seqless = R.recCompareRows([cev(0, "resume"), { kind: "interrupt" }], [cev(0, "resume")], null);
  check("seqless events do not crash and align by position",
    seqless.length === 2 && seqless[1].cls === "removed" && seqless[1].base.kind === "interrupt");
}

{
  const html = R.recCompareHtml("base-run-0001", "branch-run-0002", CMP_BASE, CMP_BRANCH, CMP_DIFF);
  check("compare html: both run ids in the header",
    html.includes("base-run-0001") && html.includes("branch-run-0002"));
  check("compare html: per-branch totals from base_totals/branch_totals",
    html.includes("128 prompt + 32 completion = 160 total") && html.includes("$0.00042") &&
    html.includes("64 prompt + 16 completion = 80 total") && html.includes("$0.00021"));
  check("compare html: divergence is marked once at seq 4",
    html.includes("first divergence — seq 4") && html.indexOf("first divergence") === html.lastIndexOf("first divergence"));
  check("compare html: removed and added tags", html.includes(">removed</span>") && html.includes(">added</span>"));
  check("compare html: prefix cells are dimmed", html.includes('cmp-side same'));
  check("compare html: human report precedes technical rows behind a disclosure",
    html.indexOf('class="cmp-report"') < html.indexOf('class="cmp-evidence"') &&
    html.includes("Inspect aligned event evidence"));
  check("compare html: aligned evidence exposes table, row, header, and cell semantics",
    html.includes('role="table"') && html.includes('role="row"') &&
    html.includes('role="columnheader"') && html.includes('role="cell"'));

  const sameHtml = R.recCompareHtml("a", "b", CMP_BASE, CMP_BASE, { first_divergent_seq: null });
  check("compare html: identical branches say so", sameHtml.includes("logically identical"));

  const xssBase = [cev(0, "node_input", { node_id: "<img src=x onerror=alert(1)>" })];
  const xssHtml = R.recCompareHtml("a", "b", xssBase, [], { first_divergent_seq: 0 });
  check("compare html escapes node ids", !xssHtml.includes("<img") && xssHtml.includes("&lt;img"));

  const hostileSeq = cev(0, "model_call");
  hostileSeq.seq = '\" onmouseover=alert(1) x="';
  const hostileSeqHtml = R.recCmpEventHtml(hostileSeq, "divergent");
  const exactSeq = cev(Number("9007199254740993"), "model_call");
  Object.defineProperty(exactSeq, R.AGENT_NUMBER_TOKENS, { value: { seq: "9007199254740993" }, enumerable: false });
  check("compare event: sequence tooltip rejects attribute injection and preserves raw u64 evidence",
    !hostileSeqHtml.includes("onmouseover") && hostileSeqHtml.includes("seq ?") &&
    R.recCmpEventHtml(exactSeq, "same").includes("seq 9007199254740993"));

  const noTotals = R.recCompareHtml("a", "b", CMP_BASE, CMP_BRANCH, {});
  check("compare html: missing totals degrade to a note", noTotals.includes("totals unavailable"));
}

{
  const totals = R.recTotalsHtml(CMP_DIFF.base_totals);
  check("totals line: events + tokens + cost",
    totals.includes("6 event(s)") && totals.includes("160 total") && totals.includes("$0.00042"));
  eq("totals line: missing value", R.recTotalsHtml(undefined).includes("unavailable"), true);
}

check("comparison interaction: semantic run labels, atomic revision reconciliation, and stable report focus are wired",
  html.includes("Baseline run<input id=\"inp-cmp-base\"") &&
  html.includes("Candidate run<input id=\"inp-cmp-branch\"") &&
  html.includes("recComparisonEvidenceState(diff, baseResponse, branchResponse, baseId, branchId)") &&
  html.includes("recCompareInputsCurrent(baseId, branchId)") &&
  html.includes('$("cmp-report-title")?.focus({ preventScroll: true })'));
check("comparison responsive: decision balance, metrics, and findings collapse deliberately",
  html.includes(".cmp-balance { grid-template-columns: 1fr; }") &&
  html.includes(".cmp-metric-grid { grid-template-columns: repeat(2, minmax(0, 1fr)); }") &&
  html.includes(".cmp-report-grid { grid-template-columns: 1fr; }") &&
  html.includes(".cmp-review-meta, .cmp-review-locked { grid-template-columns: 1fr; }") &&
  html.includes(".cmp-review-rubric-row { grid-template-columns: minmax(0,1fr) minmax(0,1fr); }") &&
  html.includes(".cmp-review-rubric-row .cmp-review-criterion { grid-column: 1 / -1; grid-row: 1; }"));
check("human review interaction: delegated editing, explicit record, edit, and export actions are wired",
  html.includes('$("rec-compare-view").addEventListener("input", recReviewInput)') &&
  html.includes('event.target.id !== "cmp-review-form"') &&
  html.includes('event.target.closest("[data-rec-review-edit]")') &&
  html.includes('event.target.closest("[data-rec-review-export]")'));
check("human review lifecycle: every new comparison creates a fresh page-memory exact-pair docket",
  html.includes("review: recReviewDraft(report)") && html.includes("recCompareInputsCurrent(value.baseId, value.branchId)") &&
  !html.includes("localStorage.setItem(\"ags:run-review"));

console.log(`\n${passed} passed, ${failed} failed`);
process.exit(failed ? 1 : 0);
