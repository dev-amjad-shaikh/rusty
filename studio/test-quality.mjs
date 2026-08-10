#!/usr/bin/env node
/* Evaluation Case Foundry contract tests. The Studio script is evaluated
 * without bootstrap so the real authoring, exact-number, validation, and
 * canonical JSONL helpers are exercised rather than copied into fixtures. */
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";
import vm from "node:vm";

const here = path.dirname(fileURLToPath(import.meta.url));
const html = readFileSync(path.join(here, "index.html"), "utf8");
const match = html.match(/<script>([\s\S]*?)<\/script>/);
if (!match) throw new Error("Studio script not found");
const src = match[1].replace(/\ninit\(\);\s*$/, "\n");
const sandbox = { document: { getElementById() { return null; } } };
vm.createContext(sandbox);
vm.runInContext(src + `
globalThis.__quality = {
  qualityInlineValue, qualitySafeText, qualitySource, qualityDefaultId, qualityDraft,
  qualityParseTags, qualityParseExpected, qualityValidate, qualityToolJson,
  qualityDatasetJsonl, qualityFilename, qualityRail, qualityHtml, qualityInput,
  qualityInvalidateAcknowledgement, qualityAddPredicate, qualityRemovePredicate,
  qualityClearThreadBoundState,
  QUALITY_DATASET_FORMAT_VERSION, QUALITY_TAG_LIMIT, QUALITY_PREDICATE_LIMIT,
  QUALITY_TOOL_LIMIT, QUALITY_EXPORT_LIMIT, AGENT_NUMBER_TOKENS, runProofCanonicalJson, store,
};`, sandbox, { filename: "index.html<script>" });

const Q = sandbox.__quality;
let passed = 0, failed = 0;
function check(name, condition, detail = "") {
  if (condition) { passed += 1; console.log("ok   " + name); }
  else { failed += 1; console.log("FAIL " + name + (detail ? " — " + detail : "")); }
}
function eq(name, got, expected) {
  check(name, JSON.stringify(got) === JSON.stringify(expected),
    "got " + JSON.stringify(got) + ", expected " + JSON.stringify(expected));
}

const runId = "run-quality-001";
const threadId = "thread-quality";
const event = (seq, kind, extra = {}) => ({
  id: runId + ":" + seq, run_id: runId, thread_id: threadId, node_id: null,
  seq, kind, effect: "pure", input: null, output: null, latency_ms: null,
  tokens: null, cost_usd: null, status: "ok", parent: null,
  recorded_at: "2026-08-10T06:00:00Z", ...extra,
});
const events = [
  event(0, "super_step_start"),
  event(1, "node_input", {
    node_id: "agent",
    input: { kind: "inline", value: { messages: [{ role: "user", content: "Book a quiet room" }], tenant: "acme" } },
  }),
  event(2, "tool_call", {
    node_id: "tools", effect: "read_only",
    input: { kind: "inline", value: { tool: "find_rooms", arguments: { city: "Seattle", guests: 2 } } },
    output: { kind: "inline", value: [{ id: "room-7" }] }, latency_ms: 12, cost_usd: 0.001,
  }),
  event(3, "node_output", { node_id: "agent", latency_ms: 20, cost_usd: 0.002 }),
  event(4, "super_step_end", { output: { kind: "inline", value: { selected: "room-7" } } }),
];
const recorder = {
  runId, requestedRunId: runId, exactEnvelope: true, complete: true,
  events, error: null, proofEvidence: { runId, threadId, events: [], bytes: 1 },
};

{
  const source = Q.qualitySource(recorder);
  check("source: exact finalized journal with inline first-node state is ready", source.ready);
  eq("source: first node state becomes the case input", source.input,
    { messages: [{ role: "user", content: "Book a quiet room" }], tenant: "acme" });
  eq("source: canonical tool calls preserve name and arguments", source.tools,
    [{ name: "find_rooms", arguments: { city: "Seattle", guests: 2 } }]);
  check("source: recorded cost is evidence but not an automatic threshold",
    source.observedCost === 0.003);
}

{
  check("source: partial journals fail closed",
    !Q.qualitySource({ ...recorder, complete: false }).ready);
  check("source: request/receipt identity mismatch fails closed",
    !Q.qualitySource({ ...recorder, requestedRunId: "other" }).ready);
  check("source: missing proof evidence fails closed",
    !Q.qualitySource({ ...recorder, proofEvidence: null }).ready);
  const artifact = events.map((item) => ({ ...item }));
  artifact[1] = { ...artifact[1], input: { kind: "artifact", value: { sha256: "a".repeat(64), bytes: 4 } } };
  check("source: artifact-backed initial state is not silently treated as an input",
    !Q.qualitySource({ ...recorder, events: artifact }).ready);
  const malformed = events.map((item) => ({ ...item }));
  malformed[2] = { ...malformed[2], input: { kind: "inline", value: { name: "legacy", args: {} } } };
  const degraded = Q.qualitySource({ ...recorder, events: malformed });
  check("source: noncanonical tool evidence degrades trajectory only",
    degraded.ready && degraded.tools === null);

  const unordered = [events[4], events[2], events[1], events[0], events[3]];
  const reordered = Q.qualitySource({ ...recorder, events: unordered });
  check("source: exact u64 sequence determines evidence order, not response array order",
    reordered.ready && reordered.input.tenant === "acme" && reordered.tools[0].name === "find_rooms");
  const missingSeq = events.map((item) => ({ ...item }));
  delete missingSeq[2].seq;
  check("source: missing exact sequence fails closed",
    !Q.qualitySource({ ...recorder, events: missingSeq }).ready);
  const duplicateSeq = events.map((item) => ({ ...item }));
  duplicateSeq[3].seq = 2;
  duplicateSeq[3].id = runId + ":2";
  check("source: duplicate exact sequence fails closed",
    !Q.qualitySource({ ...recorder, events: duplicateSeq }).ready);

  const high = event(9007199254740992, "node_output");
  Object.defineProperty(high, Q.AGENT_NUMBER_TOKENS, {
    value: { seq: "9007199254740993" }, enumerable: false,
  });
  high.id = runId + ":9007199254740993";
  const highSource = Q.qualitySource({ ...recorder, events: [...events, high] });
  check("source: legal u64 sequence above browser-safe range stays exact", highSource.ready);

  const manyTools = [...events];
  for (let index = 0; index <= Q.QUALITY_TOOL_LIMIT; index += 1) {
    const seq = 10 + index;
    manyTools.push(event(seq, "tool_call", {
      input: { kind: "inline", value: { tool: "lookup", arguments: { index } } },
    }));
  }
  const bounded = Q.qualitySource({ ...recorder, events: manyTools });
  check("source: an oversized complete tool trajectory disables the shortcut without partial inference",
    bounded.ready && bounded.tools === null && bounded.toolEvents === Q.QUALITY_TOOL_LIMIT + 2);

  const escapedToolEvents = events.map((item) => ({ ...item }));
  escapedToolEvents[2] = { ...escapedToolEvents[2], input: { kind: "inline", value: {
    tool: '"'.repeat(256), arguments: { payload: "x".repeat(65000) },
  } } };
  const escapedBound = Q.qualitySource({ ...recorder, events: escapedToolEvents });
  check("source: trajectory byte ceiling includes JSON escaping and framing",
    escapedBound.ready && escapedBound.tools === null);
}

{
  const source = Q.qualitySource(recorder);
  const draft = Q.qualityDraft(source);
  check("draft: observed tool behavior is not asserted by default", draft.useTools === false);
  check("draft: source run is retained as a bounded free-form tag",
    draft.tags.includes("source-run:" + runId));
  check("draft: stable case identity is filesystem-neutral",
    draft.caseId === "regression-run-quality-001");
  check("draft: no cost or latency gate is invented",
    draft.maxCost === "" && draft.maxLatency === "");

  draft.acknowledged = true;
  Q.store.qualityCase = draft;
  Q.qualityInput({ target: {
    type: "text", value: "changed-dataset",
    getAttribute(name) { return name === "data-quality-field" ? "dataset" : null; },
    closest() { return null; },
  } });
  check("draft: editing a reviewed field invalidates acknowledgement",
    draft.dataset === "changed-dataset" && draft.acknowledged === false);
  draft.acknowledged = true;
  check("draft: adding an expectation requires fresh acknowledgement",
    Q.qualityAddPredicate(draft) && draft.acknowledged === false && draft.predicates.length === 1);
  draft.acknowledged = true;
  check("draft: removing an expectation requires fresh acknowledgement",
    Q.qualityRemovePredicate(draft, 0) && draft.acknowledged === false && draft.predicates.length === 0);
}

{
  Q.store.recorder = recorder;
  const draft = Q.qualityDraft(Q.qualitySource(recorder));
  let validation = Q.qualityValidate(draft);
  check("validation: explicit evidence acknowledgement is required",
    !validation.ok && Boolean(validation.errors.acknowledged));
  draft.acknowledged = true;
  validation = Q.qualityValidate(draft);
  check("validation: a reviewed input-only case is legal", validation.ok);
  draft.dataset = "bad\ttitle";
  check("validation: hidden controls fail before export",
    Boolean(Q.qualityValidate(draft).errors.dataset));
  draft.dataset = "support-regression";
  draft.tags = Array.from({ length: Q.QUALITY_TAG_LIMIT + 1 }, (_, i) => "t" + i).join(",");
  check("validation: tag cardinality is bounded",
    Boolean(Q.qualityValidate(draft).errors.tags));
  draft.tags = "regression";
  draft.predicates = [{ pointer: "not-a-pointer", expected: "true" }];
  check("validation: state paths use RFC 6901 shape",
    Boolean(Q.qualityValidate(draft).errors["predicate-0"]));
  draft.predicates = [{ pointer: "/answer", expected: "9007199254740993" }];
  check("validation: browser-lossy expected integers fail closed",
    Boolean(Q.qualityValidate(draft).errors["predicate-0"]));
  draft.predicates = [{ pointer: "/answer", expected: "\"room-7\"" }];
  check("validation: exact JSON expectation passes", Q.qualityValidate(draft).ok);
  const replacement = { ...recorder };
  Q.store.recorder = replacement;
  check("validation: recorder object replacement invalidates the frozen draft",
    Boolean(Q.qualityValidate(draft).errors.source));
  Q.store.recorder = recorder;
}

{
  const source = Q.qualitySource(recorder);
  const draft = Q.qualityDraft(source);
  draft.dataset = "hotel-quality";
  draft.version = "2.1.0";
  draft.caseId = "quiet-room";
  draft.tags = "smoke,booking";
  draft.useTools = true;
  draft.forbidTools = "delete_booking";
  draft.maxCost = "0.01";
  draft.maxLatency = "200";
  draft.predicates = [{ pointer: "/selected", expected: "\"room-7\"" }];
  draft.acknowledged = true;
  Q.store.recorder = recorder;
  const jsonl = Q.qualityDatasetJsonl(draft);
  const lines = jsonl.trimEnd().split("\n");
  eq("JSONL: export has one header and one case", lines.length, 2);
  const header = JSON.parse(lines[0]), item = JSON.parse(lines[1]);
  eq("JSONL: header matches rusty-eval format v1",
    header, { kind: "header", format_version: 1, name: "hotel-quality", version: "2.1.0" });
  check("JSONL: case binds frozen input rather than event output",
    item.kind === "case" && item.id === "quiet-room" &&
    item.input.messages[0].content === "Book a quiet room" && item.input.selected === undefined);
  eq("JSONL: observed tool is deliberate exact whole-argument matcher",
    item.expect.tool_trajectory,
    [{ name: "find_rooms", args: { "": { city: "Seattle", guests: 2 } } }]);
  eq("JSONL: final-state predicate and explicit safety limits round-trip",
    item.expect.state, [{ pointer: "/selected", expected: "room-7" }]);
  check("JSONL: optional gates are emitted only after review",
    item.expect.max_cost_usd === 0.01 && item.expect.max_latency_ms === 200 &&
    item.expect.forbid_tools[0] === "delete_booking");
  eq("JSONL: tag order remains author-controlled", item.tags, ["smoke", "booking"]);
  check("JSONL: canonical file has a trailing newline", jsonl.endsWith("\n"));
}

{
  const unsafeInput = { count: 18446744073709552000 };
  Object.defineProperty(unsafeInput, Q.AGENT_NUMBER_TOKENS, {
    value: { count: "18446744073709551615" }, enumerable: false,
  });
  const unsafeEvents = events.map((item) => ({ ...item }));
  unsafeEvents[1] = { ...unsafeEvents[1], input: { kind: "inline", value: unsafeInput } };
  const unsafeRecorder = { ...recorder, events: unsafeEvents };
  Q.store.recorder = unsafeRecorder;
  const draft = Q.qualityDraft(Q.qualitySource(unsafeRecorder));
  draft.acknowledged = true;
  const jsonl = Q.qualityDatasetJsonl(draft);
  check("exact numbers: legal u64 input token is exported without rounding",
    jsonl.includes('"count":18446744073709551615') &&
    !jsonl.includes('"count":18446744073709552000'));
}

{
  Q.store.recorder = recorder;
  const draft = Q.qualityDraft(Q.qualitySource(recorder));
  draft.dataset = '<img src=x onerror="alert(1)">';
  draft.acknowledged = true;
  const rendered = Q.qualityHtml(recorder, draft);
  check("rendering: hostile dataset identity is escaped",
    rendered.includes("&lt;img") && !rendered.includes('<img src=x'));
  check("rendering: boundary distinguishes export from execution and approval",
    rendered.includes("does not persist a dataset") &&
    rendered.includes("execute an experiment") && rendered.includes("release gate"));
  check("rendering: tool evidence is visible but explicitly selectable",
    rendered.includes("find_rooms") && rendered.includes("ordered subsequence") &&
    rendered.includes("extra calls before, between, or after"));
  check("rendering: acknowledgement names the observed-versus-correct boundary",
    rendered.includes("Observed behavior is not treated as correct"));
}

{
  const sensitiveTail = "x".repeat(13000) + "TAIL-SECRET-MUST-BE-REVIEWED";
  const largeEvents = events.map((item) => ({ ...item }));
  largeEvents[1] = { ...largeEvents[1], input: { kind: "inline", value: { prompt: sensitiveTail } } };
  const largeRecorder = { ...recorder, events: largeEvents };
  const draft = Q.qualityDraft(Q.qualitySource(largeRecorder));
  const rendered = Q.qualityHtml(largeRecorder, draft);
  check("rendering: full exportable input is visible, including a sensitive tail beyond 12 KiB",
    rendered.includes("TAIL-SECRET-MUST-BE-REVIEWED") && rendered.includes("portable-data risk"));
}

{
  Q.store.qualityCase = { retained: true };
  Q.qualityClearThreadBoundState();
  check("lifecycle: losing the selected thread clears the page-memory case", Q.store.qualityCase === null);
}

check("filename: bounded portable dataset identity",
  Q.qualityFilename({ dataset: "../../Hotel Quality", version: "2.1 rc" }) ===
  "hotel-quality@2.1-rc.jsonl");
check("markup: foundry has a labelled title and stable live announcer",
  html.includes('id="quality-foundry" aria-labelledby="quality-foundry-title"') &&
  html.includes('id="quality-foundry-announcer" role="status"'));
check("interaction: delegated input, change, click, and submit are wired",
  html.includes('addEventListener("input", qualityInput)') &&
  html.includes('addEventListener("change", qualityInput)') &&
  html.includes('addEventListener("click", qualityClick)') &&
  html.includes("qualitySubmit(event)"));
check("lifecycle: connection and thread resets discard page-memory cases",
  (html.match(/store\.qualityCase = null;/g) || []).length >= 4);
check("responsive: fields, source evidence, predicates, and rail collapse on narrow screens",
  html.includes(".quality-fields, .quality-source, .quality-expect-grid { grid-template-columns:1fr; }") &&
  html.includes(".quality-predicate { grid-template-columns:1fr; }") &&
  html.includes(".quality-rail { grid-template-columns:1fr; }"));

if (failed) {
  console.error("\n" + failed + " failed, " + passed + " passed");
  process.exit(1);
}
console.log("\n" + passed + " passed, 0 failed");
