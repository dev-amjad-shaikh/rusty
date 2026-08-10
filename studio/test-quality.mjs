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
  QUALITY_DATASET_FORMAT_VERSION, QUALITY_TAG_LIMIT, QUALITY_PREDICATE_LIMIT,
  QUALITY_TOOL_LIMIT, QUALITY_EXPORT_LIMIT, QUALITY_LIBRARY_CASE_LIMIT,
  QUALITY_LIBRARY_BYTES, QUALITY_LIBRARY_LINE_BYTES, QUALITY_LIBRARY_DEPTH_LIMIT,
  QUALITY_LIBRARY_NODE_LIMIT, QUALITY_LIBRARY_NUMBER_BYTES, AGENT_NUMBER_TOKENS,
  QUALITY_GATE_BYTES, QUALITY_GATE_MAP_LIMIT, QUALITY_GATE_TEXT_BYTES,
  QUALITY_GATE_MAP_BYTES, QUALITY_GATE_NUMBER_BYTES,
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
