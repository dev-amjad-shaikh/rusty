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
const sandbox = { document: { getElementById() { return null; } }, TextDecoder };
vm.createContext(sandbox);
vm.runInContext(src + `
globalThis.__quality = {
  qualityInlineValue, qualitySafeText, qualitySource, qualityDefaultId, qualityDraft,
  qualityParseTags, qualityParseExpected, qualityValidate, qualityToolJson,
  qualityDatasetJsonl, qualityFilename, qualityRail, qualityHtml, qualityInput,
  qualityInvalidateAcknowledgement, qualityAddPredicate, qualityRemovePredicate,
  qualityClearThreadBoundState, qualityLibraryNumberCheck, qualityLibraryParseJson,
  qualityLibraryCanonicalValue, qualityLibraryRustF64, qualityLibraryDecode, qualityLibraryParse,
  qualityLibraryJsonl, qualityLibraryMerge, qualityLibraryStats, qualityLibraryHtml,
  qualityGateDraft, qualityGateU64, qualityGateF64, qualityGateMap,
  qualityGateCanonical, qualityGateValidate, qualityGateParse, qualityGateHtml,
  qualityGateFilename, qualityGateApplyField, qualityGateOperationCurrent,
  qualityReportParse, qualityReportCases, qualityReportExcerpt,
  qualityReportHtml, qualityReportOperationCurrent, qualityReportFile, qualityReportControl, qualityReportClick,
  qualityRegressionDraft, qualityRegressionConfig, qualityRegressionLogTail,
  qualityRegressionCompute, qualityRegressionRefresh, qualityRegressionHtml,
  qualityRegressionOperationCurrent, qualityRegressionFile, qualityRegressionControl, qualityRegressionClick,
  QUALITY_DATASET_FORMAT_VERSION, QUALITY_TAG_LIMIT, QUALITY_PREDICATE_LIMIT,
  QUALITY_TOOL_LIMIT, QUALITY_EXPORT_LIMIT, QUALITY_LIBRARY_CASE_LIMIT,
  QUALITY_LIBRARY_BYTES, QUALITY_LIBRARY_LINE_BYTES, QUALITY_LIBRARY_DEPTH_LIMIT,
  QUALITY_LIBRARY_NODE_LIMIT, QUALITY_LIBRARY_NUMBER_BYTES, AGENT_NUMBER_TOKENS,
  QUALITY_GATE_BYTES, QUALITY_GATE_MAP_LIMIT, QUALITY_GATE_TEXT_BYTES,
  QUALITY_GATE_MAP_BYTES, QUALITY_GATE_NUMBER_BYTES,
  QUALITY_REPORT_BYTES, QUALITY_REPORT_CASE_LIMIT, QUALITY_REPORT_RUN_LIMIT,
  QUALITY_REPORT_ASSERTION_LIMIT, QUALITY_REPORT_VALUE_BYTES,
  QUALITY_REGRESSION_PAIR_WINDOW,
  runProofCanonicalJson, store,
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

const goldenJsonl = readFileSync(path.join(here, "..", "rusty-eval", "tests", "golden", "math_tools_v1.jsonl"), "utf8");

{
  const dataset = Q.qualityLibraryParse(goldenJsonl);
  check("dataset workbench: Rust golden format-v1 dataset parses",
    dataset.name === "math-tools" && dataset.version === "1.0.0" && dataset.cases.length === 2);
  check("dataset workbench: canonical output matches Rust golden field order exactly",
    Q.qualityLibraryJsonl(dataset) === goldenJsonl);
  eq("dataset workbench: summary counts cases, tags, and expectation surfaces",
    Q.qualityLibraryStats(dataset), { cases: 2, tags: 2, expectations: 6, bytes: Buffer.byteLength(goldenJsonl) });
}

{
  const unsafe = '{"kind":"header","format_version":1,"name":"exact","version":"1"}\n' +
    '{"kind":"case","id":"u64","input":{"count":18446744073709551615},"expect":{"max_latency_ms":18446744073709551615}}\n';
  const dataset = Q.qualityLibraryParse(unsafe);
  check("dataset workbench: legal unsafe u64 tokens survive import and canonical export",
    dataset.jsonl.includes('"count":18446744073709551615') &&
    dataset.jsonl.includes('"max_latency_ms":18446744073709551615'));
  const normalized = Q.qualityLibraryParse('{"kind":"header","format_version":1,"name":"float","version":"1"}\n' +
    '{"kind":"case","id":"f","input":{"fixed":0.000001,"scientific":1e16},"expect":{"max_cost_usd":1,"max_latency_ms":null}}\n');
  check("dataset workbench: serde f64 forms and nullable options match Rust canonical output",
    normalized.jsonl.includes('"fixed":1e-6') && normalized.jsonl.includes('"scientific":1e+16') &&
    normalized.jsonl.includes('"max_cost_usd":1.0') && !normalized.jsonl.includes("max_latency_ms"));
  check("dataset workbench: Rust f64 threshold forms are explicit",
    Q.qualityLibraryRustF64(0.00001) === "0.00001" && Q.qualityLibraryRustF64(0.000001) === "1e-6" &&
    Q.qualityLibraryRustF64(1e15) === "1000000000000000.0" && Q.qualityLibraryRustF64(1e16) === "1e+16");
  const negativeZero = Q.qualityLibraryParseJson('{"value":-0}');
  check("dataset workbench: serde_json Value preserves negative zero as a float",
    Q.qualityLibraryCanonicalValue(negativeZero) === '{"value":-0.0}');
  const overflowValue = Q.qualityLibraryParseJson('{"value":18446744073709551616}');
  check("dataset workbench: integers outside i64/u64 fall back to serde_json finite-float form",
    Q.qualityLibraryCanonicalValue(overflowValue) === '{"value":1.8446744073709552e+19}');
}

{
  const missingHeader = '{"kind":"case","id":"a","input":{},"expect":{}}\n';
  const duplicate = '{"kind":"header","format_version":1,"name":"d","version":"1"}\n' +
    '{"kind":"case","id":"a","input":{},"expect":{}}\n' +
    '{"kind":"case","id":"a","input":{},"expect":{}}\n';
  for (const [name, value] of [["case before header", missingHeader], ["duplicate case identity", duplicate]]) {
    let rejected = false;
    try { Q.qualityLibraryParse(value); } catch { rejected = true; }
    check(`dataset workbench: ${name} is rejected atomically`, rejected);
  }
  const malformedDefaults = [
    '{"kind":"case","id":"a","input":{},"expect":null}',
    '{"kind":"case","id":"a","input":{},"expect":{"tool_trajectory":null}}',
    '{"kind":"case","id":"a","input":{},"expect":{},"tags":null}',
  ];
  for (const item of malformedDefaults) {
    let rejected = false;
    try { Q.qualityLibraryParse('{"kind":"header","format_version":1,"name":"d","version":"1"}\n' + item + '\n'); }
    catch { rejected = true; }
    check("dataset workbench: explicit malformed serde-default field is not treated as omitted", rejected);
  }
  const nameOnly = Q.qualityLibraryParse('{"kind":"header","format_version":1,"name":"d","version":"1"}\n' +
    '{"kind":"case","id":"named","input":{},"expect":{"tool_trajectory":[{"name":"search"}]}}\n');
  check("dataset workbench: omitted tool arguments follow Rust default and stay omitted canonically",
    nameOnly.jsonl.includes('{"name":"search"}') && !nameOnly.jsonl.includes('"args"'));
  for (const token of ["-0", "1.0", "1e0", "18446744073709551616"]) {
    let rejected = false;
    try { Q.qualityLibraryParse('{"kind":"header","format_version":1,"name":"d","version":"1"}\n' +
      '{"kind":"case","id":"u64","input":{},"expect":{"max_latency_ms":' + token + '}}\n'); }
    catch { rejected = true; }
    check(`dataset workbench: typed u64 token ${token} follows Rust grammar and range`, rejected);
  }
  for (const item of [
    '{"kind":"case","id":"a","id":"b","input":{},"expect":{}}',
    '{"kind":"case","id":"a","input":{},"expect":{"state":[],"state":[]}}',
  ]) {
    let rejected = false;
    try { Q.qualityLibraryParse('{"kind":"header","format_version":1,"name":"d","version":"1"}\n' + item + '\n'); }
    catch { rejected = true; }
    check("dataset workbench: duplicate JSON fields fail instead of becoming last-write-wins", rejected);
  }
}

{
  const supplementary = String.fromCodePoint(0x10000), bmp = String.fromCodePoint(0xe000);
  const ordered = Q.qualityLibraryParse('{"kind":"header","format_version":1,"name":"unicode","version":"1"}\n' +
    '{"kind":"case","id":"order","input":{"' + supplementary + '":1,"' + bmp + '":2},"expect":{}}\n');
  const caseLine = ordered.jsonl.split("\n")[1];
  check("dataset workbench: map keys use Rust Unicode-scalar order rather than UTF-16 order",
    caseLine.indexOf('"' + bmp + '"') < caseLine.indexOf('"' + supplementary + '"'));

  let surrogateRejected = false;
  try { Q.qualityLibraryParse('{"kind":"header","format_version":1,"name":"u","version":"1"}\n' +
    '{"kind":"case","id":"bad","input":{"value":"\\ud800"},"expect":{}}\n'); }
  catch { surrogateRejected = true; }
  check("dataset workbench: lone surrogate escapes fail before portable export", surrogateRejected);

  const deep = "[".repeat(Q.QUALITY_LIBRARY_DEPTH_LIMIT + 2) + "0" + "]".repeat(Q.QUALITY_LIBRARY_DEPTH_LIMIT + 2);
  let depthRejected = false;
  try { Q.qualityLibraryParse('{"kind":"header","format_version":1,"name":"d","version":"1"}\n' +
    '{"kind":"case","id":"deep","input":' + deep + ',"expect":{}}\n'); }
  catch { depthRejected = true; }
  check("dataset workbench: deep JSON fails inside the explicit serde-safe review boundary", depthRejected);
}

{
  const many = Array.from({ length: 12000 }, (_, index) => String(index % 10)).join(",");
  const parsed = Q.qualityLibraryParseJson('{"values":[' + many + ']}');
  check("dataset workbench: high-cardinality numeric JSON uses one bounded parser pass",
    parsed.values.length === 12000 && Q.qualityLibraryCanonicalValue(parsed).startsWith('{"values":['));
  let giantNumber = false;
  try { Q.qualityLibraryParseJson('{"value":' + "1".repeat(Q.QUALITY_LIBRARY_NUMBER_BYTES + 1) + '}'); }
  catch { giantNumber = true; }
  check("dataset workbench: a single hostile numeric token is byte bounded before BigInt conversion", giantNumber);
  let invalidUtf8 = false;
  try { Q.qualityLibraryDecode(Uint8Array.from([0x7b, 0xff, 0x7d])); } catch { invalidUtf8 = true; }
  check("dataset workbench: malformed UTF-8 is rejected instead of replacement-decoded", invalidUtf8);
  const bom = Q.qualityLibraryDecode(Uint8Array.from([0xef, 0xbb, 0xbf, 0x7b, 0x7d]));
  check("dataset workbench: a UTF-8 BOM remains visible to the Rust-compatible parser", bom.startsWith("\ufeff"));
}

{
  const first = Q.qualityLibraryParse(goldenJsonl);
  first.acknowledged = true;
  const exactDuplicate = Q.qualityLibraryParse(goldenJsonl);
  const merged = Q.qualityLibraryMerge(first, exactDuplicate);
  check("dataset workbench: exact duplicate imports are deduplicated and invalidate acknowledgement",
    merged.added === 0 && merged.deduplicated === 2 && !merged.dataset.acknowledged && merged.dataset.cases.length === 2);

  const addedJsonl = '{"kind":"header","format_version":1,"name":"math-tools","version":"1.0.0"}\n' +
    '{"kind":"case","id":"subtract","input":{"n":3},"expect":{},"tags":["math"]}\n';
  const added = Q.qualityLibraryMerge(first, Q.qualityLibraryParse(addedJsonl));
  eq("dataset workbench: same-identity additions preserve ledger order",
    added.dataset.cases.map((item) => item.id), ["add-two-numbers", "mul-then-add", "subtract"]);

  const conflictJsonl = '{"kind":"header","format_version":1,"name":"math-tools","version":"1.0.0"}\n' +
    '{"kind":"case","id":"add-two-numbers","input":{"changed":true},"expect":{}}\n';
  let conflicted = false;
  try { Q.qualityLibraryMerge(first, Q.qualityLibraryParse(conflictJsonl)); } catch { conflicted = true; }
  check("dataset workbench: conflicting case identity rejects without mutating the ledger",
    conflicted && first.cases.length === 2 && first.cases[0].input.includes("messages"));

  const otherIdentity = goldenJsonl.replace('"name":"math-tools"', '"name":"other"');
  let identityRejected = false;
  try { Q.qualityLibraryMerge(first, Q.qualityLibraryParse(otherIdentity)); } catch { identityRejected = true; }
  check("dataset workbench: mixed dataset identities require an explicit clear", identityRejected);
}

{
  const hostile = Q.qualityLibraryParse('{"kind":"header","format_version":1,"name":"safe","version":"1"}\n' +
    '{"kind":"case","id":"<img src=x onerror=alert(1)>","input":{"prompt":"secret"},"expect":{}}\n');
  const rendered = Q.qualityLibraryHtml(hostile);
  check("dataset workbench: hostile case identity is escaped in ledger and exact review",
    rendered.includes("&lt;img") && !rendered.includes("<img src=x"));
  check("dataset workbench: export requires fresh acknowledgement and warns about portable inputs",
    rendered.includes("Download dataset") && rendered.includes("disabled") &&
    rendered.includes("personal data, or secrets"));
}

{
  const header = '{"kind":"header","format_version":1,"name":"bounded","version":"1"}\n';
  const cases = Array.from({ length: Q.QUALITY_LIBRARY_CASE_LIMIT + 1 }, (_, index) =>
    '{"kind":"case","id":"c-' + index + '","input":{},"expect":{}}').join("\n") + "\n";
  let tooMany = false;
  try { Q.qualityLibraryParse(header + cases); } catch { tooMany = true; }
  check("dataset workbench: case cardinality is bounded before DOM assembly", tooMany);
  let tooLarge = false;
  try { Q.qualityLibraryParse("x".repeat(Q.QUALITY_LIBRARY_BYTES + 1)); } catch { tooLarge = true; }
  check("dataset workbench: imported files are byte bounded", tooLarge);
}

const strictGate = Q.qualityGateDraft();
const strictGateValidation = Q.qualityGateValidate(strictGate);

{
  check("release gate: strict starting point is a valid comparison-aware policy",
    strictGateValidation.ok && strictGateValidation.policy.minimumRuns === "1" &&
    strictGateValidation.policy.maximumRegressions === "0" &&
    strictGateValidation.policy.forbidRemovedCases === true);
  check("release gate: typed f64 values serialize exactly like serde_json",
    strictGateValidation.json.includes('"minimum_run_pass_rate": 1.0') &&
    strictGateValidation.json.includes('"max_pass_rate_drop": 0.05') &&
    strictGateValidation.json.includes('"max_latency_p95_ratio": 1.25'));
  const imported = Q.qualityGateParse(strictGateValidation.json);
  check("release gate: canonical format-v1 policy imports and re-exports byte-identically",
    Q.qualityGateValidate(imported).json === strictGateValidation.json && !imported.acknowledged);
}

{
  const draft = Q.qualityGateDraft();
  draft.assertionRates = '{"safe":1,"grounded":0.95}';
  draft.tagRates = '{"smoke":1}';
  draft.maximumTotalCostUsd = "0.01";
  draft.maximumCostRatio = "1.1";
  const validated = Q.qualityGateValidate(draft);
  check("release gate: candidate and comparison requirements remain distinct and complete",
    validated.ok && validated.json.includes('"grounded": 0.95') &&
    validated.json.includes('"safe": 1.0') && validated.json.includes('"smoke": 1.0') &&
    validated.json.includes('"maximum_cost_ratio": 1.1'));
  check("release gate: BTreeMap keys use Rust Unicode-scalar order",
    (() => {
      const supplementary = String.fromCodePoint(0x10000), bmp = String.fromCodePoint(0xe000);
      draft.assertionRates = '{"' + supplementary + '":1,"' + bmp + '":1}';
      const json = Q.qualityGateValidate(draft).json;
      return json.indexOf('"' + bmp + '"') < json.indexOf('"' + supplementary + '"');
    })());
}

{
  const empty = Q.qualityGateDraft();
  empty.minimumRuns = ""; empty.minimumRunPassRate = ""; empty.minimumCasePassRate = "";
  empty.maximumRegressions = ""; empty.forbidRemovedCases = false;
  check("release gate: thresholds alone never masquerade as an executable check",
    !Q.qualityGateValidate(empty).ok && Boolean(Q.qualityGateValidate(empty).errors.checks));
  for (const [field, value] of [["minimumRuns", "0"], ["minimumRunPassRate", "1.01"],
    ["maximumTotalCostUsd", "-0.01"], ["maxLatencyP95Ratio", "NaN"]]) {
    const invalid = Q.qualityGateDraft(); invalid[field] = value;
    check(`release gate: invalid ${field} fails closed`, !Q.qualityGateValidate(invalid).ok);
  }
  const repeated = Q.qualityGateDraft(); repeated.assertionRates = '{"safe":1,"safe":0.9}';
  check("release gate: duplicate named floors fail instead of silently overwriting",
    Boolean(Q.qualityGateValidate(repeated).errors.assertionRates));
}

{
  const exactKeys = Q.qualityGateDraft();
  exactKeys.assertionRates = '{" pass ":1,"a=b":0.9}';
  const validated = Q.qualityGateValidate(exactKeys);
  const roundTrip = Q.qualityGateValidate(Q.qualityGateParse(validated.json));
  check("release gate: legal whitespace and equals signs in Rust map keys round-trip exactly",
    validated.ok && validated.json.includes('" pass ": 1.0') &&
    validated.json.includes('"a=b": 0.9') && roundTrip.json === validated.json);
}

{
  const nullable = JSON.parse(strictGateValidation.json);
  nullable.minimum_runs = null;
  nullable.minimum_run_pass_rate = null;
  nullable.maximum_regressions = null;
  nullable.forbid_removed_cases = false;
  nullable.minimum_case_pass_rate = 0.9;
  const parsed = Q.qualityGateParse(JSON.stringify(nullable));
  const json = Q.qualityGateValidate(parsed).json;
  check("release gate: Rust Option nulls remain omitted checks, not coercible zeros",
    parsed.minimumRuns === "" && parsed.maximumRegressions === "" &&
    json.includes('"minimum_runs": null') && json.includes('"minimum_case_pass_rate": 0.9'));

  const unsafe = strictGateValidation.json.replace('"minimum_runs": 1',
    '"minimum_runs": 18446744073709551615');
  const unsafeParsed = Q.qualityGateParse(unsafe);
  check("release gate: legal unsafe u64 policy limits survive browser import exactly",
    Q.qualityGateValidate(unsafeParsed).json.includes('"minimum_runs": 18446744073709551615'));
  for (const token of ["-0", "1.0", "1e0", "18446744073709551616"]) {
    let rejected = false;
    try { Q.qualityGateParse(strictGateValidation.json.replace('"minimum_runs": 1', '"minimum_runs": ' + token)); }
    catch { rejected = true; }
    check(`release gate: typed u64 token ${token} follows Rust grammar and range`, rejected);
  }
}

{
  const adversarial = [
    strictGateValidation.json.replace('"name": "production",', '"name": "production",\n  "future": true,'),
    strictGateValidation.json.replace('  "minimum_runs": 1,\n', ""),
    strictGateValidation.json.replace('"name": "production"', '"name": "a", "name": "b"'),
    strictGateValidation.json.replace('"forbid_removed_cases": true', '"forbid_removed_cases": "true"'),
    strictGateValidation.json.replace('"max_pass_rate_drop": 0.05,', '"max_pass_rate_drop": 0.05,\n    "future": 1,'),
  ];
  for (const value of adversarial) {
    let rejected = false;
    try { Q.qualityGateParse(value); } catch { rejected = true; }
    check("release gate: unknown, missing, duplicate, and wrong-type policy fields fail closed", rejected);
  }
}

{
  const hostile = Q.qualityGateDraft();
  hostile.name = '<img src=x onerror="alert(1)">';
  const rendered = Q.qualityGateHtml(hostile);
  check("release gate: hostile policy identity is escaped in fields and exact preview",
    rendered.includes("&lt;img") && !rendered.includes("<img src=x"));
  check("release gate: download requires fresh review and states the non-approval boundary",
    rendered.includes("Download reviewed policy") && rendered.includes("disabled") &&
    rendered.includes("not an experiment result, release approval, or promotion"));

  hostile.acknowledged = true;
  Q.store.qualityGateRequest = 0;
  Q.qualityGateApplyField(hostile, "minimumRuns", "20", false, "text");
  check("release gate: every semantic edit invalidates acknowledgement and pending import ownership",
    hostile.minimumRuns === "20" && hostile.acknowledged === false && Q.store.qualityGateRequest === 1);
  Q.store.view = "thread"; Q.store.selected = "thread-a"; Q.store.qualityGateRequest = 7;
  check("release gate: deferred file import is bound to the initiating workspace",
    Q.qualityGateOperationCurrent(7, "thread", "thread-a") &&
    !Q.qualityGateOperationCurrent(7, "thread", "thread-b"));
}

{
  const bounded = Q.qualityGateDraft();
  bounded.assertionRates = "{" + Array.from({ length: Q.QUALITY_GATE_MAP_LIMIT + 1 }, (_, i) => `"a-${i}":1`).join(",") + "}";
  check("release gate: named-floor cardinality is bounded before DOM or export assembly",
    !Q.qualityGateValidate(bounded).ok);
  const hugeMap = Q.qualityGateDraft(); hugeMap.assertionRates = " ".repeat(Q.QUALITY_GATE_MAP_BYTES + 1);
  check("release gate: raw map editor bytes are bounded before parsing", !Q.qualityGateValidate(hugeMap).ok);
  const hugeNumber = Q.qualityGateDraft(); hugeNumber.maximumTotalCostUsd = "1".repeat(Q.QUALITY_GATE_NUMBER_BYTES + 1);
  check("release gate: raw numeric tokens are bounded before conversion", !Q.qualityGateValidate(hugeNumber).ok);
  let oversized = false;
  try { Q.qualityGateParse("x".repeat(Q.QUALITY_GATE_BYTES + 1)); } catch { oversized = true; }
  check("release gate: imported policy bytes are bounded before parsing", oversized);
}

const experimentReportValue = {
  format_version: 1,
  name: "support-regression@1.4.0",
  dataset_name: "support-regression",
  dataset_version: "1.4.0",
  runs_per_case: 2,
  max_concurrency: 2,
  cases: [
    {
      case_id: "case-alpha", tags: ["smoke", "billing"], pass_rate: 0.5,
      runs: [
        { repetition: 0, status: { status: "done" }, passed: true,
          assertions: [{ assertion: "state:/selected", passed: true,
            expected: { selected: "room-7" }, observed: { selected: "room-7" } }],
          judge: { score: 0.9, passed: true, rationale: "Resolved the request without unsupported claims." },
          tool_calls: 1, latency_ms: 100, cost_usd: 0.01, total_tokens: 10 },
        { repetition: 1, status: { status: "failed", error: "provider unavailable" }, passed: false,
          assertions: [{ assertion: "state:/selected", passed: false,
            expected: { selected: "room-7" }, observed: null, detail: "No terminal state was available." }],
          tool_calls: 0, latency_ms: 200, cost_usd: 0.02, total_tokens: 20 },
      ],
    },
    {
      case_id: "case-beta", pass_rate: 0.5,
      runs: [
        { repetition: 0, status: { status: "done" }, passed: true,
          assertions: [{ assertion: "state:/selected", passed: true,
            expected: { selected: "room-7" }, observed: { selected: "room-7" } }],
          tool_calls: 1, latency_ms: 300, cost_usd: 0.03, total_tokens: 30 },
        { repetition: 1, status: { status: "interrupted" }, passed: false,
          assertions: [{ assertion: "state:/selected", passed: false,
            expected: { selected: "room-7" }, observed: { waiting: true } }],
          judge: { score: 0.2, passed: false, rationale: "The run stopped before a final answer." },
          tool_calls: 1, latency_ms: 400, cost_usd: 0.04, total_tokens: 40 },
      ],
    },
  ],
  summary: {
    cases: 2, runs: 4, runs_passed: 2, run_pass_rate: 0.5, case_pass_rate: 0.5,
    assertions: [{ assertion: "state:/selected", passed: 2, total: 4, rate: 0.5 }],
    latency_ms: { min: 100, p50: 200, p95: 400, max: 400, mean: 250.0 },
    total_cost_usd: 0.1, total_tokens: 100,
  },
};
const experimentReportText = JSON.stringify(experimentReportValue);

{
  const report = Q.qualityReportParse(experimentReportText);
  check("experiment report: format-v1 evidence and recomputed aggregates reconcile", report.consistent);
  check("experiment report: serde defaults preserve omitted tags and judge",
    report.cases[1].tags.length === 0 && report.cases[1].runs[0].judge === null);
  check("experiment report: failing, interrupted, and judged slices use run evidence",
    (report.filter = "failing", Q.qualityReportCases(report).length === 2) &&
    (report.filter = "interrupted", Q.qualityReportCases(report)[0].id === "case-beta") &&
    (report.filter = "judged", Q.qualityReportCases(report).length === 2));
  report.filter = "all"; report.search = "billing";
  check("experiment report: search is bounded to case identities and tags",
    Q.qualityReportCases(report).length === 1 && Q.qualityReportCases(report)[0].id === "case-alpha");
  report.search = "";
  const rendered = Q.qualityReportHtml(report);
  check("experiment report: ledger renders carried totals and exact evidence surfaces",
    rendered.includes("50%") && rendered.includes("p95 latency") && rendered.includes("Expected") && rendered.includes("Observed"));
  check("experiment report: artifact attribution names experiment, dataset, repetitions, and concurrency",
    rendered.includes("support-regression@1.4.0") && rendered.includes("support-regression@1.4.0</b>") &&
    rendered.includes("Repetitions") && rendered.includes("Max concurrency"));
  check("experiment report: case and repetition actions retain native button semantics",
    rendered.includes('role="list" aria-label="Experiment cases"') &&
    rendered.includes('role="listitem"><button class="quality-report-case"') &&
    rendered.includes('role="listitem"><button class="small"'));
  report.filter = "interrupted"; report.search = "missing";
  check("experiment report: an empty slice keeps a labelled case-evidence region",
    Q.qualityReportHtml(report).includes('id="quality-report-case-title" tabindex="-1">Case evidence'));
}

{
  const driftValue = structuredClone(experimentReportValue);
  driftValue.summary.runs_passed = 4;
  driftValue.summary.run_pass_rate = 1;
  driftValue.cases[0].runs[1].passed = true;
  const drift = Q.qualityReportParse(JSON.stringify(driftValue));
  check("experiment report: serde-valid aggregate drift remains inspectable but never reconciled",
    !drift.consistent && drift.issues.some((issue) => issue.includes("passed-run")) &&
    Q.qualityReportHtml(drift).includes("reconciliation required"));

  const broadValue = structuredClone(experimentReportValue);
  broadValue.cases[0].runs[0].judge.score = 2;
  const broad = Q.qualityReportParse(JSON.stringify(broadValue));
  check("experiment report: broad serde f64 values are accepted as carried evidence and flagged",
    !broad.consistent && broad.issues.some((issue) => issue.includes("judge score is outside")));

  const toleratedValue = structuredClone(experimentReportValue);
  toleratedValue.summary.run_pass_rate = 0.5 + 5e-13;
  check("experiment report: reconciliation uses rusty-eval's finite 1e-12 float tolerance",
    Q.qualityReportParse(JSON.stringify(toleratedValue)).consistent);
  toleratedValue.summary.run_pass_rate = 0.5 + 2e-12;
  check("experiment report: float drift beyond rusty-eval's tolerance remains visible",
    !Q.qualityReportParse(JSON.stringify(toleratedValue)).consistent);

  const repetitionsValue = structuredClone(experimentReportValue);
  repetitionsValue.cases[0].runs[0].repetition = 1;
  repetitionsValue.cases[0].runs[1].repetition = 2;
  const repetitions = Q.qualityReportParse(JSON.stringify(repetitionsValue));
  check("experiment report: reconciliation requires the exact zero-based repetition set",
    !repetitions.consistent && repetitions.issues.some((issue) => issue.includes("exact 0–1 repetition set")));

  const emptyEvidence = structuredClone(experimentReportValue);
  emptyEvidence.name = "  "; emptyEvidence.dataset_name = ""; emptyEvidence.dataset_version = "\t";
  emptyEvidence.cases[0].case_id = " ";
  emptyEvidence.cases[0].runs[0].assertions[0].assertion = "";
  emptyEvidence.cases[0].runs[0].judge.rationale = "  ";
  const empty = Q.qualityReportParse(JSON.stringify(emptyEvidence));
  check("experiment report: reconciliation mirrors rusty-eval non-empty evidence requirements",
    !empty.consistent && empty.issues.some((issue) => issue.includes("experiment name is empty")) &&
    empty.issues.some((issue) => issue.includes("dataset name is empty")) &&
    empty.issues.some((issue) => issue.includes("dataset version is empty")) &&
    empty.issues.some((issue) => issue.includes("case id is empty")) &&
    empty.issues.some((issue) => issue.includes("empty assertion name")) &&
    empty.issues.some((issue) => issue.includes("judge rationale is empty")));
}

{
  const duplicateValue = structuredClone(experimentReportValue);
  duplicateValue.cases = [structuredClone(experimentReportValue.cases[0]), structuredClone(experimentReportValue.cases[0])];
  duplicateValue.cases[1].runs[0].assertions[0].observed = { selected: "second-copy" };
  duplicateValue.cases[1].runs[1].repetition = 0;
  const report = Q.qualityReportParse(JSON.stringify(duplicateValue));
  Q.store.qualityReport = report;
  Q.qualityReportClick({ target: { closest(selector) {
    if (selector === "[data-quality-report-case]") return { getAttribute() { return "1"; } };
    return null;
  } } });
  check("experiment report: duplicate serde-valid case IDs retain distinct selection identity",
    report.selectedCase === "1" && Q.qualityReportHtml(report).includes("second-copy"));
  report.selectedRun = "1";
  check("experiment report: duplicate repetition numbers retain distinct evidence identity",
    Q.qualityReportHtml(report).includes("provider unavailable"));
}

{
  const exactText = experimentReportText
    .replace('"total_tokens":10', '"total_tokens":9007199254740993')
    .replace('"total_tokens":100', '"total_tokens":9007199254741083');
  const exact = Q.qualityReportParse(exactText);
  check("experiment report: u64 evidence above browser-safe range stays exact",
    exact.cases[0].runs[0].totalTokens === "9007199254740993" &&
    exact.summary.totalTokens === "9007199254741083" && exact.consistent);

  const extraValue = structuredClone(experimentReportValue);
  extraValue.provider_receipt = { deployment: "external" };
  const extra = Q.qualityReportParse(JSON.stringify(extraValue));
  check("experiment report: serde-compatible extra fields are disclosed, not interpreted",
    extra.extras.includes("report.provider_receipt") &&
    Q.qualityReportHtml(extra).includes("remain uninterpreted"));
}

{
  for (const [name, text] of [
    ["wrong version", experimentReportText.replace('"format_version":1', '"format_version":2')],
    ["duplicate typed key", experimentReportText.replace('"name":"support-regression@1.4.0"', '"name":"first","name":"second"')],
    ["fractional u64", experimentReportText.replace('"runs_per_case":2', '"runs_per_case":2.0')],
    ["malformed status", experimentReportText.replace('{"status":"failed","error":"provider unavailable"}', '{"status":"failed"}')],
  ]) {
    let rejected = false;
    try { Q.qualityReportParse(text); } catch { rejected = true; }
    check(`experiment report: ${name} fails closed`, rejected);
  }
  let oversized = false;
  try { Q.qualityReportParse(" ".repeat(Q.QUALITY_REPORT_BYTES + 1)); } catch { oversized = true; }
  check("experiment report: raw import bytes are bounded before parsing", oversized);
  const excerpt = Q.qualityReportExcerpt('"' + "🔥".repeat(3000) + '"');
  check("experiment report: visible JSON excerpts use a truthful UTF-8 byte boundary",
    excerpt.truncated && new TextEncoder().encode(excerpt.text).length <= 8192);
}

{
  const hostile = structuredClone(experimentReportValue);
  hostile.cases[0].case_id = '<img src=x onerror="alert(1)">\u202Etxt';
  hostile.cases[0].tags = ["<script>unsafe()</script>"];
  const report = Q.qualityReportParse(JSON.stringify(hostile));
  const rendered = Q.qualityReportHtml(report);
  check("experiment report: hostile report identities and tags are escaped",
    !rendered.includes("<img src=x") && !rendered.includes("<script>unsafe") &&
    rendered.includes("&lt;img") && rendered.includes("&lt;script&gt;") &&
    rendered.includes("\\u{202E}") && !rendered.includes("\u202E"));
}

{
  const nodes = new Map([
    ["quality-report-body", { innerHTML: "", querySelector() { return { focus() {}, matches() { return false; } }; } }],
    ["quality-report-mark", { textContent: "" }],
    ["quality-report-announcer", { textContent: "" }],
  ]);
  sandbox.document.getElementById = (id) => nodes.get(id) || null;
  Q.store.view = "thread"; Q.store.selected = "thread-a"; Q.store.connectionEpoch = 9;
  const prior = Q.qualityReportParse(experimentReportText); Q.store.qualityReport = prior;
  let release;
  const pending = new Promise((resolve) => { release = resolve; });
  const operation = Q.qualityReportFile({ target: { value: "chosen", files: [{ size: 8, arrayBuffer: () => pending }] } });
  Q.store.selected = "thread-b";
  release(new TextEncoder().encode(experimentReportText).buffer);
  await operation;
  check("experiment report: deferred file reads cannot cross the initiating thread workspace",
    Q.store.qualityReport === prior);

  Q.store.selected = "thread-a";
  await Q.qualityReportFile({ target: { value: "chosen", files: [{ size: 8,
    arrayBuffer: async () => new TextEncoder().encode("{bad").buffer }] } });
  check("experiment report: a rejected replacement preserves the already-inspectable report",
    Q.store.qualityReport === prior && prior.error.includes("Report not accepted"));

  let restoredSelection = null;
  const searchNode = { value: "bilXling", focus() {}, matches(selector) { return selector === 'input[type="search"]'; },
    setSelectionRange(start, end) { restoredSelection = [start, end]; } };
  nodes.get("quality-report-body").querySelector = (selector) => selector === "[data-quality-report-search]" ? searchNode : null;
  Q.qualityReportControl({ isComposing: true, target: { value: "請求", selectionStart: 1, selectionEnd: 1,
    matches(selector) { return selector === "[data-quality-report-search]"; } } });
  check("experiment report: IME composition updates search state without replacing the active input",
    Q.store.qualityReport.search === "請求" && restoredSelection === null);
  Q.qualityReportControl({ target: { value: "bilXling", selectionStart: 4, selectionEnd: 4,
    matches(selector) { return selector === "[data-quality-report-search]"; } } });
  check("experiment report: mid-string search edits preserve the exact caret selection",
    restoredSelection?.[0] === 4 && restoredSelection?.[1] === 4);
  sandbox.document.getElementById = () => null;
}

function matchedReport(name, outcomes, runsPerCase) {
  if (!outcomes.length || outcomes.length % runsPerCase) throw new Error("fixture outcomes must fill complete cases");
  const cases = [];
  for (let offset = 0; offset < outcomes.length; offset += runsPerCase) {
    const slice = outcomes.slice(offset, offset + runsPerCase);
    cases.push({
      case_id: `case-${offset / runsPerCase}`, pass_rate: slice.filter(Boolean).length / slice.length,
      runs: slice.map((passed, repetition) => ({
        repetition, status: passed ? { status: "done" } : { status: "failed", error: "fixture failure" },
        passed, assertions: [], tool_calls: 0, latency_ms: 1, cost_usd: 0, total_tokens: 0,
      })),
    });
  }
  const passed = outcomes.filter(Boolean).length;
  return Q.qualityReportParse(JSON.stringify({
    format_version: 1, name, dataset_name: "matched-support", dataset_version: "v1",
    runs_per_case: runsPerCase, max_concurrency: 1, cases,
    summary: { cases: cases.length, runs: outcomes.length, runs_passed: passed,
      run_pass_rate: passed / outcomes.length,
      case_pass_rate: cases.reduce((sum, item) => sum + item.pass_rate, 0) / cases.length,
      assertions: [], latency_ms: { min: 1, p50: 1, p95: 1, max: 1, mean: 1.0 },
      total_cost_usd: 0, total_tokens: 0 },
  }));
}

{
  const baseline = matchedReport("baseline", Array(30).fill(true), 30);
  const candidate = matchedReport("candidate", Array(30).fill(false), 30);
  const draft = { ...Q.qualityRegressionDraft(), baseline, candidate };
  const result = Q.qualityRegressionCompute(draft);
  check("matched regression: exact paired loss meets practical and statistical thresholds",
    result.total === 30 && result.regressions === 30 && result.improvements === 0 &&
    result.passRateDrop === 1 && result.effectThresholdMet && result.significanceThresholdMet &&
    result.decision === "regression" && Math.abs(result.pValue - 2 ** -30) < 1e-18);
  draft.minimumPairs = "31";
  const insufficient = Q.qualityRegressionCompute(draft);
  check("matched regression: underpowered evidence has no p-value or regression claim",
    insufficient.decision === "insufficient_evidence" && insufficient.pValue === null && !insufficient.significanceThresholdMet);
}

{
  const baselineOutcomes = Array.from({ length: 40 }, (_, index) => index % 2 === 0);
  const candidateOutcomes = baselineOutcomes.map((passed) => !passed);
  const balanced = Q.qualityRegressionCompute({ ...Q.qualityRegressionDraft(), minimumPairs: "1",
    baseline: matchedReport("balanced-base", baselineOutcomes, 40),
    candidate: matchedReport("balanced-candidate", candidateOutcomes, 40) });
  check("matched regression: balanced discordance is not mislabeled as regression",
    balanced.regressions === 20 && balanced.improvements === 20 && balanced.passRateDrop === 0 &&
    balanced.pValue > 0.5 && balanced.decision === "no_regression");

  const baseline = [...Array(6).fill(true), ...Array(94).fill(false)];
  const candidate = [true, ...Array(99).fill(false)];
  const threshold = Q.qualityRegressionCompute({ ...Q.qualityRegressionDraft(), minimumPairs: "1",
    baseline: matchedReport("effect-base", baseline, 50), candidate: matchedReport("effect-candidate", candidate, 50) });
  check("matched regression: Rust golden exact tail and inclusive five-point effect agree",
    threshold.regressions === 5 && threshold.improvements === 0 && threshold.passRateDrop === 0.05 &&
    Math.abs(threshold.pValue - 0.03125) < 1e-14 && threshold.decision === "regression");
  const stricter = Q.qualityRegressionCompute({ ...Q.qualityRegressionDraft(), minimumPairs: "1", minimumDrop: "0.051",
    baseline: matchedReport("effect-base", baseline, 50), candidate: matchedReport("effect-candidate", candidate, 50) });
  check("matched regression: significance alone cannot satisfy the practical-effect policy",
    stricter.significanceThresholdMet && !stricter.effectThresholdMet && stricter.decision === "no_regression");
}

{
  const baseline = matchedReport("large-base", Array(1100).fill(true), 55);
  const candidate = matchedReport("large-candidate", Array(1100).fill(false), 55);
  const result = Q.qualityRegressionCompute({ ...Q.qualityRegressionDraft(), significance: "1e-320", baseline, candidate });
  check("matched regression: subnormal exact tail stays nonzero and compares in log space",
    result.pValue === Number.MIN_VALUE && result.significanceThresholdMet && result.decision === "regression");
  const rendered = Q.qualityRegressionHtml({ ...Q.qualityRegressionDraft(), baseline, candidate, result, filter: "regression", error: "" });
  check("matched regression: the complete computation uses a bounded visible pair window",
    rendered.includes(`Showing ${Q.QUALITY_REGRESSION_PAIR_WINDOW} of 1100 matched runs`) &&
    (rendered.match(/role="listitem"/g) || []).length === Q.QUALITY_REGRESSION_PAIR_WINDOW);
}

{
  const baseline = matchedReport("baseline", [true, false, true, false], 2);
  const candidate = matchedReport("candidate", [true, true, false, false], 2);
  const draft = { ...Q.qualityRegressionDraft(), minimumPairs: "1", baseline, candidate };
  Q.qualityRegressionRefresh(draft);
  const rendered = Q.qualityRegressionHtml(draft);
  check("matched regression: outcome matrix and ledger expose all four exact pair classes",
    draft.result.bothPassed === 1 && draft.result.regressions === 1 && draft.result.improvements === 1 && draft.result.bothFailed === 1 &&
    rendered.includes('<table class="quality-regression-matrix">') && rendered.includes("candidate regressed") && rendered.includes("candidate improved"));
  check("matched regression: evidence names both artifacts and keeps the release boundary explicit",
    rendered.includes("baseline") && rendered.includes("candidate") && rendered.includes("not a durable") && rendered.includes("Release Gate decision"));
  check("matched regression: matrix actions carry specific accessible names and pressed state",
    rendered.includes('aria-label="candidate regressed: 1 matched runs"') && rendered.includes('aria-pressed="false"'));

  const hostileBase = matchedReport("<baseline>", [true], 1);
  hostileBase.name = '<img src=x onerror="alert(1)">';
  const hostileCandidate = matchedReport("candidate", [false], 1);
  hostileCandidate.cases[0].id = '<script>bad()</script>\u202E';
  const hostileDraft = { ...Q.qualityRegressionDraft(), minimumPairs: "1", baseline: hostileBase, candidate: hostileCandidate };
  hostileBase.cases[0].id = hostileCandidate.cases[0].id;
  Q.qualityRegressionRefresh(hostileDraft);
  const hostileHtml = Q.qualityRegressionHtml(hostileDraft);
  check("matched regression: hostile artifact and pair identities are escaped and controls exposed visibly",
    !hostileHtml.includes("<img src=x") && !hostileHtml.includes("<script>bad") && !hostileHtml.includes("\u202E") &&
    hostileHtml.includes("&lt;img") && hostileHtml.includes("&lt;script&gt;") && hostileHtml.includes("\\u{202E}"));
}

{
  const baseline = matchedReport("baseline", [true, false], 2);
  const differentDataset = matchedReport("candidate", [true, false], 2);
  differentDataset.datasetVersion = "v2";
  let mismatch = "";
  try { Q.qualityRegressionCompute({ ...Q.qualityRegressionDraft(), baseline, candidate: differentDataset }); }
  catch (error) { mismatch = error.message; }
  check("matched regression: dataset drift fails before pairing", mismatch.includes("same dataset"));

  const missing = matchedReport("candidate", [true, false], 2); missing.cases[0].id = "different-case";
  let missingError = "";
  try { Q.qualityRegressionCompute({ ...Q.qualityRegressionDraft(), baseline, candidate: missing }); }
  catch (error) { missingError = error.message; }
  check("matched regression: case-key drift cannot be silently discarded", missingError.includes("missing matched"));

  const driftValue = structuredClone(experimentReportValue); driftValue.summary.runs_passed = 4;
  const drift = Q.qualityReportParse(JSON.stringify(driftValue));
  let driftError = "";
  try { Q.qualityRegressionCompute({ ...Q.qualityRegressionDraft(), baseline: drift, candidate: drift }); }
  catch (error) { driftError = error.message; }
  check("matched regression: internally unreconciled inputs fail closed", driftError.includes("reconciliation issues"));

  for (const [field, value] of [["significance", "0"], ["significance", "1"], ["minimumDrop", "1.1"], ["minimumPairs", "0"], ["minimumPairs", "1.0"], ["minimumPairs", " 30"]]) {
    let rejected = false;
    try { Q.qualityRegressionConfig({ ...Q.qualityRegressionDraft(), [field]: value }); } catch { rejected = true; }
    check(`matched regression: invalid ${field} token ${value} fails closed`, rejected);
  }
  const invalidDraft = { ...Q.qualityRegressionDraft(), baseline, candidate: baseline, significance: "0" };
  Q.qualityRegressionRefresh(invalidDraft);
  const invalidHtml = Q.qualityRegressionHtml(invalidDraft);
  check("matched regression: policy errors identify and describe the exact invalid control",
    invalidDraft.errorField === "significance" && invalidHtml.includes('data-quality-regression-field="significance" value="0" inputmode="decimal" aria-invalid="true"') &&
    invalidHtml.includes('aria-describedby="quality-regression-policy-help quality-regression-error"'));

  const emptyInvalidDraft = { ...Q.qualityRegressionDraft(), significance: "0" };
  Q.qualityRegressionRefresh(emptyInvalidDraft);
  check("matched regression: policy validation remains active before reports are loaded",
    emptyInvalidDraft.errorField === "significance" && Q.qualityRegressionHtml(emptyInvalidDraft).includes('aria-invalid="true"'));
}

{
  const nodes = new Map([
    ["quality-regression-body", { innerHTML: "", querySelector() { return { focus() {}, matches() { return false; } }; } }],
    ["quality-regression-mark", { textContent: "" }],
    ["quality-regression-announcer", { textContent: "" }],
  ]);
  sandbox.document.getElementById = (id) => nodes.get(id) || null;
  Q.store.view = "thread"; Q.store.selected = "thread-a"; Q.store.connectionEpoch = 12;
  const prior = matchedReport("prior", [true], 1);
  Q.store.qualityRegression = { ...Q.qualityRegressionDraft(), baseline: prior };
  let release;
  const pending = new Promise((resolve) => { release = resolve; });
  const operation = Q.qualityRegressionFile({ target: { value: "chosen", getAttribute() { return "candidate"; },
    files: [{ size: 8, arrayBuffer: () => pending }] } });
  Q.store.selected = "thread-b";
  release(new TextEncoder().encode(experimentReportText).buffer);
  await operation;
  check("matched regression: deferred report reads cannot cross the initiating thread workspace",
    Q.store.qualityRegression.baseline === prior && Q.store.qualityRegression.candidate === null);

  Q.store.selected = "thread-a";
  const explorerChoice = matchedReport("explorer-choice", [true], 1); Q.store.qualityReport = explorerChoice;
  let releaseOlder;
  const olderRead = new Promise((resolve) => { releaseOlder = resolve; });
  const olderOperation = Q.qualityRegressionFile({ target: { value: "chosen", getAttribute() { return "candidate"; },
    files: [{ size: 8, arrayBuffer: () => olderRead }] } });
  Q.qualityRegressionClick({ target: { closest(selector) {
    return selector === "[data-quality-regression-use]" ? { getAttribute() { return "candidate"; } } : null;
  } } });
  releaseOlder(new TextEncoder().encode(experimentReportText).buffer);
  await olderOperation;
  check("matched regression: a newer Explorer choice owns its slot over a delayed file read",
    Q.store.qualityRegression.candidate?.name === "explorer-choice");

  const hostileAnnouncement = structuredClone(experimentReportValue); hostileAnnouncement.name = "bad\u202Ename";
  await Q.qualityRegressionFile({ target: { value: "chosen", getAttribute() { return "baseline"; },
    files: [{ size: 8, arrayBuffer: async () => new TextEncoder().encode(JSON.stringify(hostileAnnouncement)).buffer }] } });
  check("matched regression: live import announcements expose bidi controls visibly",
    !nodes.get("quality-regression-announcer").textContent.includes("\u202E") &&
    nodes.get("quality-regression-announcer").textContent.includes("\\u{202E}"));

  Q.store.qualityRegression.significance = "0";
  Q.qualityRegressionRefresh(Q.store.qualityRegression);
  await Q.qualityRegressionFile({ target: { value: "chosen", getAttribute() { return "candidate"; },
    files: [{ size: Q.QUALITY_REPORT_BYTES + 1, arrayBuffer: async () => new ArrayBuffer(0) }] } });
  const sourceFailureHtml = Q.qualityRegressionHtml(Q.store.qualityRegression);
  check("matched regression: source and policy failures retain separate truthful associations",
    Q.store.qualityRegression.errorField === "significance" && sourceFailureHtml.includes('id="quality-regression-source-error"') &&
    sourceFailureHtml.includes("exceeds the 2 MiB import boundary") && sourceFailureHtml.includes('aria-invalid="true"') &&
    sourceFailureHtml.includes('aria-describedby="quality-regression-policy-help quality-regression-error"'));

  const baseline = matchedReport("control-base", [true], 1), candidate = matchedReport("control-candidate", [false], 1);
  Q.store.selected = "thread-a";
  Q.store.qualityRegression = { ...Q.qualityRegressionDraft(), minimumPairs: "1", baseline, candidate };
  Q.qualityRegressionRefresh(Q.store.qualityRegression);
  let restoredSelection = null, focusCount = 0;
  const inputNode = { value: "0.01", focus() { focusCount += 1; }, matches(selector) { return selector === "input"; },
    setSelectionRange(start, end) { restoredSelection = [start, end]; } };
  nodes.get("quality-regression-body").querySelector = () => inputNode;
  Q.qualityRegressionControl({ type: "input", isComposing: true, target: { value: "0.0", selectionStart: 3, selectionEnd: 3,
    getAttribute() { return "significance"; } } });
  check("matched regression: IME composition does not replace the active policy input", focusCount === 0 && restoredSelection === null);
  Q.qualityRegressionControl({ type: "input", target: { value: "0.01", selectionStart: 4, selectionEnd: 4,
    getAttribute() { return "significance"; } } });
  check("matched regression: semantic policy edits recompute immediately and preserve caret",
    Q.store.qualityRegression.result?.config.significance === 0.01 && restoredSelection?.[0] === 4 && restoredSelection?.[1] === 4);
  sandbox.document.getElementById = () => null;
}

check("experiment report markup: labelled read-only boundary and one stable live announcer",
  html.includes('id="quality-report" aria-labelledby="quality-report-title"') &&
  html.includes("Studio did not execute this experiment") &&
  html.includes('id="quality-report-announcer" role="status"'));
check("experiment report interaction: import, filters, cases, runs, and clear are delegated",
  html.includes('matches("[data-quality-report-file]")') &&
  html.includes('addEventListener("input", qualityReportControl)') &&
  html.includes('addEventListener("compositionend", qualityReportControl)') &&
  html.includes('addEventListener("click", qualityReportClick)'));
check("experiment report lifecycle: connection reset clears evidence and invalidates pending reads",
  html.includes("store.qualityReport = null;") && html.includes("store.qualityReportRequest += 1;"));
check("experiment report responsive: summary, index, run, and assertion evidence collapse on narrow screens",
  html.includes(".quality-report-stats { grid-template-columns:repeat(2,minmax(0,1fr)); }") &&
  html.includes(".quality-report-toolbar, .quality-report-layout { grid-template-columns:1fr; }") &&
  html.includes(".quality-report-integrity { grid-template-columns:1fr; }") &&
  html.includes(".quality-report-run-head { display:flex; flex-wrap:wrap;") &&
  html.includes(".quality-report-evidence { grid-template-columns:1fr; }") &&
  html.includes(".quality-report-identity { grid-template-columns:repeat(2,minmax(0,1fr)); }") &&
    html.includes(".quality-report-assertion p,.quality-report-judge,.quality-report-detail .note { overflow-wrap:anywhere;"));
check("matched regression markup: labelled evidence boundary and one stable live announcer",
  html.includes('id="quality-regression" aria-labelledby="quality-regression-title"') &&
  html.includes("Studio exposes evidence; it does not run experiments, approve a release, or apply a gate policy") &&
  html.includes('id="quality-regression-announcer" role="status"'));
check("matched regression interaction: exact imports, policy edits, matrix filters, and current report handoff are delegated",
  html.includes('matches("[data-quality-regression-file]")') &&
  html.includes('addEventListener("input", qualityRegressionControl)') &&
  html.includes('addEventListener("compositionend", qualityRegressionControl)') &&
  html.includes('addEventListener("click", qualityRegressionClick)') &&
  html.includes('data-quality-regression-use="${side}"'));
check("matched regression lifecycle: connection reset clears both reports and invalidates pending reads",
  html.includes("store.qualityRegression = null;") && html.includes("store.qualityRegressionRequest += 1;") &&
  html.includes("store.qualityRegressionSideRequest.baseline += 1;") && html.includes("store.qualityRegressionSideRequest.candidate += 1;"));
check("matched regression accessibility: matrix is a native table and policy help is associated",
  html.includes('<table class="quality-regression-matrix">') &&
  html.includes('scope="col"') && html.includes('scope="row"') &&
  html.includes('aria-describedby="quality-regression-policy-help${invalid ? " quality-regression-error" : ""}"'));
check("matched regression responsive: sources, policy, verdict, ledger, and metrics collapse on narrow screens",
  html.includes(".quality-regression-sources,.quality-regression-config,.quality-regression-verdict,.quality-regression-grid { grid-template-columns:1fr; }") &&
  html.includes(".quality-regression-metrics { grid-template-columns:repeat(2,minmax(0,1fr)); }") &&
  html.includes(".quality-regression-pair { grid-template-columns:minmax(0,1fr) auto; }"));

check("release gate filename: portable and purpose-specific",
  Q.qualityGateFilename("../../Production Approval") === "production-approval.gate.json");
check("release gate markup: labelled evidence-contract surface and stable live announcer",
  html.includes('id="quality-gate" aria-labelledby="quality-gate-title"') &&
  html.includes('id="quality-gate-announcer" role="status"') &&
  html.includes("Release evidence contract · connection-bound page memory") &&
  html.includes("a connection change or page reload discards it unless downloaded"));
check("release gate interaction: edit, import, reset, and download are delegated",
  html.includes('addEventListener("input", qualityGateInput)') &&
  html.includes('matches("[data-quality-gate-file]")') &&
  html.includes("qualityGateClick(event)") &&
  html.includes('addEventListener("submit", (event) => event.preventDefault())'));
check("release gate lifecycle: connection reset discards policy and invalidates pending file reads",
  html.includes("store.qualityGate = null;") && html.includes("store.qualityGateRequest += 1;"));
check("release gate accessibility: field errors are programmatically associated",
  html.includes('aria-describedby="quality-gate-error-') &&
  html.includes('field.setAttribute("aria-invalid", "true")'));
check("release gate responsive: evidence columns stack on narrow screens",
  html.includes(".quality-gate-contract, .quality-gate-preview, .quality-gate-fields { grid-template-columns:1fr; }"));

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
check("markup: dataset workbench is labelled, page-memory scoped, and has one live announcer",
  html.includes('id="quality-dataset" aria-labelledby="quality-dataset-title"') &&
  html.includes("Portable quality library · page memory") &&
  html.includes('id="quality-dataset-announcer" role="status"'));
check("interaction: dataset import, selection, acknowledgement, and actions are delegated",
  html.includes('matches("[data-quality-dataset-file]")') &&
  html.includes("qualityLibraryChange(event)") &&
  html.includes('addEventListener("click", qualityLibraryClick)'));
check("accessibility: hidden file inputs have a visible focus ring and final removal has a stable fallback",
  html.includes(".quality-file-button:focus-within") &&
  html.includes("dataset.jsonl = qualityLibraryJsonl(dataset);\n    qualityLibraryRender(dataset.cases.length ? '[data-quality-dataset-select]' : '[data-quality-dataset-file]')"));
check("download: assembled datasets inherit their exact reviewed name and version",
  html.includes("qualityFilename({ dataset: dataset.name, version: dataset.version })"));
check("lifecycle: connection reset discards dataset content and invalidates pending file reads",
  html.includes("store.qualityDataset = null;") && html.includes("store.qualityDatasetRequest += 1;"));
check("responsive: dataset ledger collapses to one column on narrow screens",
  html.includes(".quality-dataset-layout { grid-template-columns:1fr; }") &&
  html.includes(".quality-dataset-stats { grid-template-columns:repeat(2,minmax(0,1fr)); }"));

if (failed) {
  console.error("\n" + failed + " failed, " + passed + " passed");
  process.exit(1);
}
console.log("\n" + passed + " passed, 0 failed");
