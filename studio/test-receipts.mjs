#!/usr/bin/env node
/* Contract and interaction tests for Studio's signed run-proof desk. */
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";
import vm from "node:vm";

const here = path.dirname(fileURLToPath(import.meta.url));
const html = readFileSync(path.join(here, "index.html"), "utf8");
const match = html.match(/<script>([\s\S]*?)<\/script>/);
if (!match) { console.error("FAIL: no <script> block found"); process.exit(1); }
const src = match[1].replace(/\ninit\(\);\s*$/, "\n");
const sandbox = {};
vm.createContext(sandbox);
vm.runInContext(src + `
globalThis.__proof = {
  runProofHex, runProofText, runProofSame, runProofValidateReceipt, runProofValidateFixture,
  runProofValidateVerified, runProofValidateKeys, runProofReady, runProofOperationCurrent,
  runProofError, runProofHtml, runProofLoadedEvidence, runProofExactJson, runProofVisibleMessage,
  runProofVerify, runProofValidateManifest, runProofManifestHtml, agentParseJsonWithNumberKinds, store,
  RUN_PROOF_RESPONSE_BYTES, RUN_PROOF_LIST_LIMIT, RUN_PROOF_KEY_LIMIT,
};
globalThis.__setProofApi = (fn) => { apiForConnection = fn; };`, sandbox, { filename: "index.html<script>" });
const P = sandbox.__proof;

let passed = 0, failed = 0;
function check(name, condition, detail = "") {
  if (condition) { passed++; console.log(`ok   ${name}`); }
  else { failed++; console.log(`FAIL ${name}${detail ? ` — ${detail}` : ""}`); }
}

const RUN = "run-proof-1";
const HEAD = "a".repeat(64);
const SIGNER = "b".repeat(64);
const receipt = {
  format_version: 1,
  run_id: RUN,
  journal_head: { events: 2, sha256: HEAD },
  manifest_digest: "c".repeat(64),
  manifest: { model: "gpt-pinned", prompts: {}, tool_schemas: {}, capsules: {} },
  capsules: { research: "d".repeat(64) },
  effects: ["e".repeat(64)],
  executor_policy: "policy-1",
  capsule_policies: ["cedar-1"],
  denials: [`${RUN}:1`],
  signer: SIGNER,
  signature: "f".repeat(128),
};
const snapshot = {
  run_id: RUN, thread_id: "thread-1",
  events: [{ id: `${RUN}:0`, run_id: RUN, thread_id: "thread-1", seq: 0 },
    { id: `${RUN}:1`, run_id: RUN, thread_id: "thread-1", seq: 1 }],
  artifacts: {}, artifact_refs: {}, head_hash: HEAD,
};
const recorder = { runId: RUN, requestedRunId: RUN, exactEnvelope: true, events: snapshot.events, complete: true, error: null };
recorder.proofEvidence = P.runProofLoadedEvidence(recorder);
const fixture = { format_version: 1, graph_hash: "1".repeat(64), graph_version: "unversioned",
  journal: snapshot, final_checkpoint: null, metadata: { name: "proof" } };
const verified = {
  run_id: RUN, journal_head: { events: 2, sha256: HEAD },
  manifest_digest: receipt.manifest_digest, capsules: receipt.capsules,
  effect_receipts: 1, executor_policy: "policy-1", capsule_policies: ["cedar-1"],
  denials: [`${RUN}:1`], signer: SIGNER,
};

/* Exact receipt and fixture boundary. */
{
  const checked = P.runProofValidateReceipt(receipt, RUN);
  const loaded = P.runProofLoadedEvidence({ runId: RUN, events: snapshot.events });
  check("receipt: exact v1 statement is accepted", checked.ok && checked.events === 2n && checked.headHash === HEAD);
  check("receipt: requested run identity is mandatory", !P.runProofValidateReceipt(receipt, "other").ok);
  check("receipt: format versions are not coerced", !P.runProofValidateReceipt({ ...receipt, format_version: "1" }, RUN).ok);
  check("receipt: signature length and lowercase hex are exact", !P.runProofValidateReceipt({ ...receipt, signature: "F".repeat(128) }, RUN).ok);
  check("receipt: carried manifests require their signed digest", !P.runProofValidateReceipt({ ...receipt, manifest_digest: null }, RUN).ok);
  check("receipt: capsule identities are content addresses", !P.runProofValidateReceipt({ ...receipt, capsules: { research: "not-a-digest" } }, RUN).ok);
  check("receipt: effect ledger has a hard item bound", !P.runProofValidateReceipt({ ...receipt, effects: Array(P.RUN_PROOF_LIST_LIMIT + 1).fill("e".repeat(64)) }, RUN).ok);
  check("receipt: explicit null and falsy default collections are malformed", !P.runProofValidateReceipt({ ...receipt, capsules: null }, RUN).ok &&
    !P.runProofValidateReceipt({ ...receipt, effects: false }, RUN).ok && !P.runProofValidateReceipt({ ...receipt, capsule_policies: "" }, RUN).ok);
  check("manifest: explicit null maps are malformed rather than serde omission",
    P.runProofValidateManifest({ prompts: {}, tool_schemas: {}, capsules: {} }) &&
    !P.runProofValidateManifest({ prompts: null }) && !P.runProofValidateManifest({ tool_schemas: false }) &&
    !P.runProofValidateManifest({ capsules: "" }));
  check("manifest: digests and optional identities retain their exact wire types",
    P.runProofValidateManifest({ prompts: { system: "bad" } }).interpreted === false &&
    !P.runProofValidateManifest({ model: 7 }) && P.runProofValidateManifest({ model_params: "bad" }).interpreted === false &&
    !P.runProofValidateManifest({ memory_schema: ["v1"] }));
  const broadManifest = { model: "legal\u0000rust-string", prompts: Object.fromEntries(Array.from({ length: P.RUN_PROOF_KEY_LIMIT + 1 }, (_, index) => [`p${index}`, "not-hex"])) };
  const broadReceipt = { ...receipt, manifest: broadManifest };
  check("manifest: legal broad serde values retain verification but degrade visual interpretation",
    P.runProofValidateReceipt(broadReceipt, RUN).ok && P.runProofValidateManifest(broadManifest).interpreted === false &&
    P.runProofManifestHtml(P.runProofValidateManifest(broadManifest)).includes("Signed, not interpreted"));
  check("manifest: present-empty optional identities never render as absent defaults",
    P.runProofValidateManifest({ model: "" }).interpreted === false &&
    P.runProofValidateManifest({ memory_schema: "" }).interpreted === false &&
    P.runProofValidateManifest({ capsules: { worker: "" } }).interpreted === false);

  const checkedFixture = P.runProofValidateFixture(fixture, RUN, checked, loaded);
  check("fixture: exact run, event count, and head bind the signed statement", checkedFixture.ok && checkedFixture.snapshot === snapshot);
  const reordered = { ...fixture, journal: { ...snapshot,
    events: snapshot.events.map((event) => Object.fromEntries(Object.entries(event).reverse())) } };
  check("fixture: harmless JSON object-key order does not change event identity",
    P.runProofValidateFixture(reordered, RUN, checked, loaded).ok);
  check("fixture: a changed head cannot reach verification", !P.runProofValidateFixture({ ...fixture, journal: { ...snapshot, head_hash: "0".repeat(64) } }, RUN, checked, loaded).ok);
  check("fixture: event-count drift cannot reach verification", !P.runProofValidateFixture({ ...fixture, journal: { ...snapshot, events: snapshot.events.slice(0, 1) } }, RUN, checked, loaded).ok);
  check("fixture: external artifact references fail before a non-portable proof claim", !P.runProofValidateFixture({ ...fixture, journal: { ...snapshot, artifact_refs: { x: {} } } }, RUN, checked, loaded).ok);
  const visibleDrift = { ...fixture, journal: { ...snapshot, events: [{ ...snapshot.events[0], kind: "changed" }, snapshot.events[1]] } };
  check("fixture: same-run same-head responses still bind every visible Recorder event", !P.runProofValidateFixture(visibleDrift, RUN, checked, loaded).ok);
  check("fixture: thread identity is part of the visible evidence binding", !P.runProofValidateFixture({ ...fixture, journal: { ...snapshot, thread_id: "other-thread" } }, RUN, checked, loaded).ok);
  check("fixture: explicit null artifact refs are not serde omission", !P.runProofValidateFixture({ ...fixture, journal: { ...snapshot, artifact_refs: null } }, RUN, checked, loaded).ok);
}

/* The typed verifier summary must repeat every signed component Studio shows. */
{
  const checked = P.runProofValidateReceipt(receipt, RUN);
  check("verification: exact typed summary is accepted", P.runProofValidateVerified(verified, checked).ok);
  const emptyReceipt = { ...receipt };
  delete emptyReceipt.effects; delete emptyReceipt.capsules; delete emptyReceipt.capsule_policies; delete emptyReceipt.denials;
  const emptyChecked = P.runProofValidateReceipt(emptyReceipt, RUN);
  const emptyVerified = { ...verified, effect_receipts: 0, capsules: {}, capsule_policies: [], denials: [] };
  check("verification: serde-omitted empty ledgers retain exact zero semantics", emptyChecked.ok && P.runProofValidateVerified(emptyVerified, emptyChecked).ok);
  check("verification: effect count must equal the signed ledger", !P.runProofValidateVerified({ ...verified, effect_receipts: 0 }, checked).ok);
  check("verification: policy evidence cannot drift", !P.runProofValidateVerified({ ...verified, executor_policy: "other" }, checked).ok);
  check("verification: capsule map cannot drift", !P.runProofValidateVerified({ ...verified, capsules: {} }, checked).ok);
  check("verification: denial order cannot drift", !P.runProofValidateVerified({ ...verified, denials: [] }, checked).ok);
  check("verification: signer identity cannot drift", !P.runProofValidateVerified({ ...verified, signer: "9".repeat(64) }, checked).ok);
  check("verification: explicit null default collections are malformed", !P.runProofValidateVerified({ ...verified, capsules: null }, checked).ok);
}

/* Raw Rust integer lexemes survive the exact verifier request serializer. */
{
  const unsafe = P.agentParseJsonWithNumberKinds(`{"snapshot":{"run_id":"${RUN}","thread_id":"thread-1","events":[{"run_id":"${RUN}","thread_id":"thread-1","seq":9007199254740993}],"artifacts":{},"head_hash":"${HEAD}"},"receipt":${JSON.stringify(receipt)}}`);
  const exact = P.runProofExactJson(unsafe);
  check("exact request: legal u64 values do not round through JavaScript", exact.includes('"seq":9007199254740993') && !exact.includes('"seq":9007199254740992'));
}

/* Public key history is corroborating metadata, never secret material. */
{
  const record = { key_id: SIGNER, public_key: "9".repeat(64), registered_at: "2026-08-09T00:00:00Z" };
  const active = P.runProofValidateKeys({ active: SIGNER, keys: [record] }, SIGNER);
  check("key history: active signer is distinguished", active && active.state === "active" && active.record.key_id === SIGNER);
  const successor = { ...record, key_id: "8".repeat(64), public_key: "7".repeat(64) };
  const retired = P.runProofValidateKeys({ active: successor.key_id, keys: [{ ...record, retired_at: "2026-08-10T00:00:00Z" }, successor] }, SIGNER);
  check("key history: retired signer remains recognized", retired && retired.state === "retired");
  const historical = P.runProofValidateKeys({ active: successor.key_id, keys: [record, successor] }, SIGNER);
  check("key history: non-active unretired signers are historical, not falsely retired", historical && historical.state === "historical");
  check("key history: missing signer never becomes corroborated", P.runProofValidateKeys({ active: "8".repeat(64), keys: [] }, SIGNER) === null);
  check("key history: active identity must be present in the catalog", P.runProofValidateKeys({ active: "8".repeat(64), keys: [record] }, SIGNER) === null);
  check("key history: malformed timestamps fail closed", P.runProofValidateKeys({ active: SIGNER, keys: [{ ...record, registered_at: "sometime" }] }, SIGNER) === null);
}

/* Rendering stays useful before, during, after, and outside verification. */
{
  check("readiness: only literal finalized non-empty evidence is eligible", P.runProofReady(recorder) &&
    !P.runProofReady({ ...recorder, complete: "true" }) && !P.runProofReady({ ...recorder, events: [] }) &&
    !P.runProofReady({ ...recorder, exactEnvelope: false }));
  const oversizedRecorder = { ...recorder, events: [{ ...snapshot.events[0], output: "x".repeat(P.RUN_PROOF_RESPONSE_BYTES) }] };
  oversizedRecorder.proofEvidence = P.runProofLoadedEvidence(oversizedRecorder);
  check("readiness: an oversized visible journal cannot expose the proof action",
    oversizedRecorder.proofEvidence === null && !P.runProofReady(oversizedRecorder) &&
    P.runProofHtml(oversizedRecorder, null).includes("exceeds the 8 MiB inspection boundary"));
  const readyHtml = P.runProofHtml(recorder, null);
  check("ready UI: action names minting and verification", readyHtml.includes("Mint &amp; verify signed proof") || readyHtml.includes("Mint & verify signed proof"));
  check("ready UI: mint-on-read persistence is disclosed before action", readyHtml.includes("First use persists a receipt") && readyHtml.includes('aria-describedby="run-proof-mint-note"'));
  check("ready UI: key rotation is not exposed", !readyHtml.includes("rotate") && !readyHtml.includes("/receipt_keys/rotate"));
  const checked = P.runProofValidateReceipt(receipt, RUN);
  const doneHtml = P.runProofHtml(recorder, { phase: "verified", step: "done", receipt, receiptCheck: checked,
    verified, keyInfo: { state: "retired", record: { key_id: SIGNER } } });
  check("verified UI: all four proof links and retired-key truth are visible", ["Journal head", "Runtime contract", "Effect ledger", "Deployment signer", "retired key · still verifiable"].every((text) => doneHtml.includes(text)));
  check("verified UI: exact trust boundary rejects model-quality and remote-attestation inference", doneHtml.includes("model answer quality") && doneHtml.includes("remote/KMS transparency attestation"));
  const fullManifest = P.runProofValidateManifest({ model: "provider/model-2026-08-09", model_params: "1".repeat(64),
    prompts: { '<system>': "2".repeat(64), reviewer: "2".repeat(8) + "f".repeat(52) + "2".repeat(4) },
    tool_schemas: { search: "4".repeat(64) }, memory_schema: "memory-v1",
    capsules: { research: "capsule-v3" }, future_pin: { digest: "5".repeat(64) } });
  const manifestHtml = P.runProofManifestHtml(fullManifest);
  check("runtime contract: five signed surfaces become one visual bill of materials",
    ["5 / 5 surfaces carry pins", "provider/model-2026-08-09", "2 content pins", "1 schema pin", "memory-v1", "capsule-v3", "1".repeat(64)].every((text) => manifestHtml.includes(text)));
  check("runtime contract: collision-shaped digests remain visually distinguishable in full",
    manifestHtml.includes("2".repeat(64)) && manifestHtml.includes("2".repeat(8) + "f".repeat(52) + "2".repeat(4)));
  check("runtime contract: hostile names are escaped and unknown signed fields stay explicit",
    manifestHtml.includes("&lt;system&gt;") && !manifestHtml.includes("<system>") &&
    manifestHtml.includes("future_pin") && manifestHtml.includes("not interpreted"));
  const partialModel = P.runProofManifestHtml(P.runProofValidateManifest({ model: "floating-alias" }));
  check("runtime contract: partial and absent pins never become defaults",
    partialModel.includes("parameter set unpinned") && partialModel.includes("1 / 5 surfaces carry pins") &&
    P.runProofManifestHtml(null).includes("0 / 5 surfaces carry pins"));
  const hostile = P.runProofHtml(recorder, { phase: "error", error: '<img src=x onerror="boom">', receiptReceived: true });
  check("error UI: server and client messages are escaped", hostile.includes("&lt;img") && !hostile.includes("<img"));
  check("empty UI: partial journals never expose a mint control", !P.runProofHtml({ ...recorder, complete: false }, null).includes("data-run-proof-action"));
}

/* Generation + tenant + selected-run ownership are one gate. */
{
  P.store.conn = { baseUrl: "http://127.0.0.1:8000", apiKey: "tenant-a" };
  P.store.connectionEpoch = 7;
  P.store.threads = [{ thread_id: "thread-1" }];
  P.store.selected = "thread-1";
  P.store.view = "thread";
  const ownedRecorder = { runId: RUN, requestedRunId: RUN, exactEnvelope: true, events: snapshot.events, complete: true, error: null };
  ownedRecorder.proofEvidence = P.runProofLoadedEvidence(ownedRecorder);
  P.store.recorder = ownedRecorder;
  P.store.runProofRequest = 4;
  const operation = { epoch: 7, connection: { ...P.store.conn }, threadId: "thread-1" };
  check("ownership: exact request, tenant, thread, run, and Recorder object remain current", P.runProofOperationCurrent(4, operation, RUN, ownedRecorder));
  P.store.recorder = { ...P.store.recorder, runId: "new-run" };
  check("ownership: a changed loaded run cancels late proof evidence", !P.runProofOperationCurrent(4, operation, RUN, ownedRecorder));
  P.store.recorder = ownedRecorder;
  P.store.connectionEpoch = 8;
  check("ownership: a connection epoch change cancels late proof evidence", !P.runProofOperationCurrent(4, operation, RUN, ownedRecorder));
}

/* Exercise the real async operation, including exact unsafe-u64 transport and stale completion ownership. */
{
  const panel = { contains: () => false, setAttribute: () => {} };
  const body = { innerHTML: "", 
    querySelector: () => ({ focus: () => {} }) };
  const announcer = { textContent: "" };
  const visualStatus = { textContent: "Proof status changed." };
  sandbox.document = { activeElement: null,
    getElementById: (id) => id === "run-proof" ? panel : id === "run-proof-body" ? body :
      id === "run-proof-announcer" ? announcer : id === "run-proof-status" ? visualStatus : null };
  const unsafeFixture = P.agentParseJsonWithNumberKinds(`{"format_version":1,"graph_hash":"${"1".repeat(64)}","graph_version":"unversioned","journal":{"run_id":"${RUN}","thread_id":"thread-1","events":[{"id":"${RUN}:9007199254740993","run_id":"${RUN}","thread_id":"thread-1","seq":9007199254740993}],"artifacts":{},"artifact_refs":{},"head_hash":"${HEAD}"},"final_checkpoint":null,"metadata":{"name":"proof"}}`);
  const unsafeReceipt = { ...receipt, journal_head: { events: 1, sha256: HEAD } };
  const unsafeVerified = { ...verified, journal_head: { events: 1, sha256: HEAD } };
  const unsafeRecorder = { runId: RUN, requestedRunId: RUN, exactEnvelope: true,
    events: unsafeFixture.journal.events, complete: true, error: null };
  unsafeRecorder.proofEvidence = P.runProofLoadedEvidence(unsafeRecorder);
  const keyCatalog = { active: SIGNER, keys: [{ key_id: SIGNER, public_key: "7".repeat(64),
    registered_at: "2026-08-09T00:00:00Z", retired_at: null }] };
  const calls = [];
  sandbox.__setProofApi(async (...args) => {
    calls.push(args);
    if (args[2].endsWith("/receipt")) return unsafeReceipt;
    if (args[2].endsWith("/fixture")) return unsafeFixture;
    if (args[2] === "/receipts/verify") return unsafeVerified;
    if (args[2] === "/receipt_keys") return keyCatalog;
    throw new Error(`unexpected path ${args[2]}`);
  });
  P.store.conn = { baseUrl: "http://proof.test", apiKey: "tenant-a" };
  P.store.connectionEpoch = 20;
  P.store.threads = [{ thread_id: "thread-1" }]; P.store.selected = "thread-1"; P.store.view = "thread";
  P.store.recorder = unsafeRecorder; P.store.runProof = null;
  await P.runProofVerify();
  const posted = calls.find((args) => args[2] === "/receipts/verify");
  check("async operation: receipt → fixture → verifier → keys reaches verified state", calls.map((args) => args[2]).join("|") ===
    `/runs/${RUN}/receipt|/runs/${RUN}/fixture|/receipts/verify|/receipt_keys` && P.store.runProof?.phase === "verified" &&
    announcer === sandbox.document.getElementById("run-proof-announcer") && announcer.textContent.includes("Signed proof verified"));
  check("async operation: verifier receives the exact unsafe-u64 fixture token", posted && posted[5].includes('"seq":9007199254740993') &&
    !posted[5].includes('"seq":9007199254740992'));

  let releaseReceipt;
  const delayedReceipt = new Promise((resolve) => { releaseReceipt = resolve; });
  let staleCalls = 0;
  sandbox.__setProofApi(async () => { staleCalls += 1; return delayedReceipt; });
  P.store.connectionEpoch = 21; P.store.recorder = recorder; P.store.runProof = null;
  const staleOperation = P.runProofVerify();
  await Promise.resolve();
  const replacement = { phase: "ready", marker: "new workspace" };
  P.store.connectionEpoch = 22; P.store.recorder = { ...recorder }; P.store.runProof = replacement;
  releaseReceipt(receipt);
  await staleOperation;
  check("async operation: a stale deferred response cannot mutate replacement proof state",
    staleCalls === 1 && P.store.runProof === replacement);
}

/* Error families stay operationally distinct. */
check("errors: route-less older servers are capability gaps", P.runProofError("receipt", { status: 404, body: { raw: "missing" } }).includes("does not expose"));
check("errors: structured receipt 404 stays access-bound unknown", P.runProofError("receipt", { status: 404, body: { error: "not_found" } }).includes("access boundary"));
check("errors: pre-journal conflict is not a signature rejection", P.runProofError("receipt", { status: 409, body: {} }).includes("no persisted journal"));
check("errors: verification mismatch preserves the named component", P.runProofError("verify", { status: 422, body: { error: "receipt_verification_failed", message: "journal_head mismatch" } }) === "journal_head mismatch");
check("errors: visible server text is bounded and names truncation", (() => { const text = P.runProofError("verify", { status: 422, body: { error: "receipt_verification_failed", message: "x".repeat(9000) } }); return text.length < 2200 && text.includes("message truncated by Studio"); })());

/* Source-level integration assertions protect the deliberate request order. */
{
  const receiptAt = src.indexOf("/receipt`, undefined, RUN_PROOF_RESPONSE_BYTES");
  const fixtureAt = src.indexOf("/fixture`, undefined, RUN_PROOF_RESPONSE_BYTES", receiptAt);
  const verifyAt = src.indexOf('"POST", "/receipts/verify"', fixtureAt);
  const keysAt = src.indexOf('"GET", "/receipt_keys"', verifyAt);
  check("integration: receipt → fixture → verifier → key lineage is sequential", receiptAt > 0 && fixtureAt > receiptAt && verifyAt > fixtureAt && keysAt > verifyAt);
  check("integration: every proof response is bounded at 8 MiB", P.RUN_PROOF_RESPONSE_BYTES === 8 * 1024 * 1024 &&
    (src.match(/RUN_PROOF_RESPONSE_BYTES/g) || []).length === 7);
  check("integration: verifier POST uses the exact raw-number serializer", src.includes("exactBodyText ||") &&
    src.includes("undefined, RUN_PROOF_RESPONSE_BYTES, exactRequest"));
  check("integration: click delegation invokes the proof operation", src.includes('[data-run-proof-action]")) runProofVerify()'));
  check("interaction: rerenders preserve owned keyboard focus on a stable labelled target", src.includes("const ownedFocus = panel.contains(document.activeElement)") &&
    src.includes('focusSelector || "#run-proof-title"'));
  check("integration: workspace and journal changes invalidate proof generations", (src.match(/store\.runProofRequest \+= 1/g) || []).length >= 3);
  check("responsive: the proof chain stacks deliberately on narrow screens", html.includes(".run-proof-chain { grid-template-columns: 1fr; }") && html.includes(".run-proof-meta { grid-template-columns: repeat(2"));
  check("responsive: the runtime bill of materials becomes one readable mobile column",
    html.includes(".run-contract-grid { grid-template-columns: 1fr; }") && html.includes(".run-contract-head { flex-direction: column; }"));
  check("accessibility: only the narrow status region is live", !html.includes('id="run-proof" aria-labelledby="run-proof-title" aria-live') &&
    html.includes('id="run-proof-announcer" role="status" aria-live="polite" aria-atomic="true"') &&
    !P.runProofHtml(recorder, { phase: "ready" }).includes('aria-live='));
}

if (failed) {
  console.error(`\n${passed} passed, ${failed} failed`);
  process.exit(1);
}
console.log(`\n${passed} passed, 0 failed`);
