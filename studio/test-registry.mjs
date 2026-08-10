#!/usr/bin/env node
/* Extension Catalog contract tests. The Studio script is evaluated without
 * bootstrap so the real registry parsing, rendering, and async ownership
 * helpers are exercised instead of copied into fixtures. */
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";
import vm from "node:vm";
import { webcrypto } from "node:crypto";

const here = path.dirname(fileURLToPath(import.meta.url));
const page = readFileSync(path.join(here, "index.html"), "utf8");
const match = page.match(/<script>([\s\S]*?)<\/script>/);
if (!match) throw new Error("Studio script not found");
const src = match[1].replace(/\ninit\(\);\s*$/, "\n");
const nodes = new Map();
const sandbox = { document: { getElementById: (id) => nodes.get(id) || null }, TextDecoder, TextEncoder, URL, URLSearchParams, crypto:webcrypto };
vm.createContext(sandbox);
vm.runInContext(src + `
globalThis.__registry = {
  registryObject, registryText, registryRustTextCompare, registryTimestamp, registryCandidateId,
  registryOwner, registryOwnerText, registryArtifactName, registryCommit,
  registryArtifactContract, registryListContract, registryHistoryContract,
  registryExactValue, registryDiffContract, registryVisible, registryRenderWindow, registryErrorHtml,
  registrySummaryHtml, registryRowHtml, registryCommitHtml, registryDiffHtml,
  registryDetailHtml, registryRender, registryLoad, registryLoadHistory, registryCompare,
  registryBindingEnvironment, registryBindingValidation, registryBindingPrepare,
  registryBindingResolution, registryBindingEvidence, registryBindingRenderWindow,
  registryBindingState, registryBindingAccepted, registryBindingCopy,
  registryBindingArtifactMap, registryBindingOutput, registryBindingRunReceipt, registryBindingStreamMetadata,
  registryBindingSubmitted, registryBindingFailed, registryBindingLoadArtifacts, registryBindingStreamCurrent, registryBindSurface,
  runWait,
  agentParseJsonWithNumberKinds, REGISTRY_FAMILIES, REGISTRY_RENDER_LIMIT,
  REGISTRY_COMMIT_RENDER_LIMIT, REGISTRY_DIFF_RENDER_LIMIT, REGISTRY_ADMISSION_FAMILIES,
  REGISTRY_BINDING_RENDER_LIMIT, REGISTRY_BINDING_AUTHOR_LIMIT, REGISTRY_BACKGROUND_STATUSES, REGISTRY_TERMINAL_STATUSES, store,
};`, sandbox, { filename: "index.html<script>" });

const R = sandbox.__registry;
let passed = 0, failed = 0;
function check(name, condition, detail = "") {
  if (condition) { passed += 1; console.log("ok   " + name); }
  else { failed += 1; console.log("FAIL " + name + (detail ? " — " + detail : "")); }
}

const id1 = "1".repeat(64), id2 = "2".repeat(64), id3 = "3".repeat(64);
const owner = { type: "human", human_id: "release-owner" };
const commit = (candidate_id = id1, committed_at = "2026-08-10T10:00:00Z") => ({ candidate_id, committed_at });
const artifact = (changes = {}) => ({
  surface: "prompt:system", family: "prompt", owner,
  commits: [commit(id1), commit(id2, "2026-08-10T11:00:00Z")],
  created_at: "2026-08-10T09:00:00Z", ...changes,
});
const parsedArtifact = R.registryArtifactContract(artifact());

check("families: all eight shipped registry families are discoverable", R.REGISTRY_FAMILIES.size === 8 && ["prompt", "policy", "memory_set", "tool_permission", "tool_contract", "model_settings", "memory_configuration", "middleware_composition"].every((item) => R.REGISTRY_FAMILIES.has(item)));
check("owner: human, agent, distiller, and system provenance remain attributable",
  R.registryOwnerText({ type: "human", human_id: "h" }) === "human:h" &&
  R.registryOwnerText({ type: "agent", agent_id: "a" }) === "agent:a" &&
  R.registryOwnerText({ type: "distiller", name: "d" }) === "distiller:d" &&
  R.registryOwnerText({ type: "system" }) === "system");
check("owner: strict variants retain server-legal provenance while making invisible identity visible",
  !R.registryOwner({ type: "system", name: "extra" }) && R.registryOwner({ type: "human", human_id: "bad\u202e" }) && R.registryOwnerText({ type:"agent", agent_id:"" }).includes("empty identity"));
check("artifact: exact family surface, owner, timestamps, and immutable commit IDs are accepted", parsedArtifact?.name === "system" && parsedArtifact.commits.length === 2);
check("artifact: declared records may truthfully omit an empty commit sequence", R.registryArtifactContract({ surface: "policy:retry", family: "policy", owner: { type: "system" }, created_at: "2026-08-10T09:00:00Z" })?.commits.length === 0);
check("artifact: explicit null commits are not mistaken for serde omission", !R.registryArtifactContract(artifact({ commits: null })));
check("artifact: mismatched surface prefix, tagged names, paths, and whitespace names fail closed",
  !R.registryArtifactContract(artifact({ surface: "policy:system" })) && !R.registryArtifactContract(artifact({ surface: "prompt:system@prod" })) &&
  !R.registryArtifactContract(artifact({ surface: "prompt:path/name" })) && !R.registryArtifactContract(artifact({ surface: "prompt: name " })));
check("artifact: duplicate content addresses invalidate the immutable commit spine", !R.registryArtifactContract(artifact({ commits: [commit(id1), commit(id1, "2026-08-10T12:00:00Z")] })));
check("artifact: lone surrogates fail before rendering or sort comparison", !R.registryText("bad\ud800id"));
check("artifact: append order does not invent chronological sorting for explicitly reproduced instants",
  R.registryArtifactContract(artifact({ commits: [commit(id1, "2026-08-10T12:00:00Z"), commit(id2, "2026-08-10T11:00:00Z")] }))?.commits.length === 2);
check("catalog: exact sorted unique response-bounded snapshot is retained", R.registryListContract({ artifacts: [artifact()] })?.length === 1);
check("catalog: ordering follows Rust Unicode scalar order instead of JavaScript UTF-16 order",
  R.registryRustTextCompare("prompt:\ue000", "prompt:\ud800\udc00") < 0 &&
  R.registryListContract({ artifacts: [artifact({ surface: "prompt:\ue000", commits: [] }), artifact({ surface: "prompt:\ud800\udc00", commits: [] })] })?.length === 2);
check("catalog: duplicate surfaces and unsorted records fail closed without inventing a Rust protocol count cap",
  !R.registryListContract({ artifacts: [artifact(), artifact()] }) &&
  !R.registryListContract({ artifacts: [artifact({ surface: "prompt:z" }), artifact({ surface: "prompt:a" })] }) &&
  R.registryListContract({ artifacts: Array.from({ length: R.REGISTRY_RENDER_LIMIT + 1 }, (_, index) => artifact({ surface: `prompt:${String(index).padStart(4, "0")}`, commits: [] })) })?.length === R.REGISTRY_RENDER_LIMIT + 1);

const history = { surface: "prompt:system", family: "prompt", owner, commits: [
  { ...commit(id1), author: { type: "human", human_id: "alice" }, status: "evaluated" },
  { ...commit(id2, "2026-08-10T11:00:00Z"), author: null, status: null },
] };
check("history: joined author and lifecycle bind the selected artifact's exact spine", R.registryHistoryContract(history, parsedArtifact)?.[0].status === "evaluated");
check("history: missing candidate join remains explicit null evidence", R.registryHistoryContract(history, parsedArtifact)?.[1].author === null);
check("history: cross-surface, reordered, or malformed lifecycle evidence fails closed",
  !R.registryHistoryContract({ ...history, surface: "prompt:other" }, parsedArtifact) &&
  !R.registryHistoryContract({ ...history, commits: history.commits.slice().reverse() }, parsedArtifact) &&
  !R.registryHistoryContract({ ...history, commits: [{ ...history.commits[0], status: "serving" }, history.commits[1]] }, parsedArtifact) &&
  !R.registryHistoryContract({ ...history, commits: [{ ...history.commits[0], author:null }, history.commits[1]] }, parsedArtifact) &&
  !R.registryHistoryContract({ ...history, commits: [history.commits[0], { ...history.commits[1], author:{ type:"system" } }] }, parsedArtifact));
check("history: a concurrent append reconciles only after the earlier exact spine remains a prefix",
  R.registryHistoryContract({ ...history, commits: [...history.commits, { ...commit(id3, "2026-08-10T12:00:00Z"), author: { type:"system" }, status:"created" }] }, parsedArtifact)?.length === 3 &&
  !R.registryHistoryContract({ ...history, commits: [history.commits[0], { ...commit(id3), author: { type:"system" }, status:"created" }, history.commits[1]] }, parsedArtifact));

const textDiff = { surface: "prompt:system", from: id1, to: id2, diff: { view: "text", lines: [
  { op: "context", line: "system" }, { op: "removed", line: "old <rule>" }, { op: "added", line: "new & safe" },
] } };
check("diff: prompt line view binds exact surface and candidate pair", R.registryDiffContract(textDiff, parsedArtifact, id1, id2)?.lines.length === 3);
check("diff: a legal empty prompt line remains exact", R.registryDiffContract({ ...textDiff, diff:{ view:"text", lines:[{ op:"context", line:"" }] } }, parsedArtifact, id1, id2)?.lines[0].line === "");
const policyArtifact = R.registryArtifactContract(artifact({ surface:"policy:system", family:"policy" }));
check("diff: family binds prompt to text and every JSON family to structural evidence",
  !R.registryDiffContract({ ...textDiff, diff:{ view:"structural", added:[], removed:[], changed:[] } }, parsedArtifact, id1, id2) &&
  !R.registryDiffContract({ ...textDiff, surface:"policy:system" }, policyArtifact, id1, id2));
check("diff: an unreviewed pair or unsupported operation fails closed", !R.registryDiffContract(textDiff, parsedArtifact, id2, id1) && !R.registryDiffContract({ ...textDiff, diff: { view: "text", lines: [{ op: "moved", line: "x" }] } }, parsedArtifact, id1, id2));
{
  const structural = R.agentParseJsonWithNumberKinds(`{"surface":"policy:system","from":"${id1}","to":"${id2}","diff":{"view":"structural","added":[{"path":"/limit","value":18446744073709551615}],"removed":[],"changed":[{"path":"/temperature","from":0.1,"to":0.2}]}}`);
  const parsed = R.registryDiffContract(structural, policyArtifact, id1, id2);
  check("diff: structural leaves preserve legal unsafe Rust JSON integers exactly", parsed && parsed.added[0].exact === "18446744073709551615");
  check("diff rendering: unsafe number remains exact rather than rounded", R.registryDiffHtml({ from: id1, to: id2, diff: parsed }).includes("18446744073709551615") && !R.registryDiffHtml({ from: id1, to: id2, diff: parsed }).includes("18446744073709552000"));
}
check("diff: strict structural leaf shapes fail closed", !R.registryDiffContract({ surface: "policy:system", from: id1, to: id2, diff: { view: "structural", added: [{ path: "/x" }], removed: [], changed: [] } }, policyArtifact, id1, id2));
check("diff: structural paths are unique and disjoint while legal array traversal keeps numeric index order",
  R.registryDiffContract({ surface:"policy:system", from:id1, to:id2, diff:{ view:"structural", added:Array.from({length:11}, (_, index) => ({path:`/${index}`,value:index})), removed:[], changed:[] } }, policyArtifact, id1, id2)?.added.length === 11 &&
  !R.registryDiffContract({ surface:"policy:system", from:id1, to:id2, diff:{ view:"structural", added:[{path:"/a",value:1}], removed:[{path:"/a",value:2}], changed:[] } }, policyArtifact, id1, id2));
const otherArtifact = R.registryArtifactContract(artifact({ surface:"prompt:other", commits:[commit(id3)] }));
check("filters: search and family compose across every catalog candidate, not only selected history", R.registryVisible({ artifacts: [parsedArtifact, otherArtifact], history: { surface: parsedArtifact.surface, commits: R.registryHistoryContract(history, parsedArtifact) } }, id3.slice(0, 12), "prompt")[0]?.surface === "prompt:other" && R.registryVisible({ artifacts: [parsedArtifact] }, "release-owner", "policy").length === 0);

check("rendering: listbox row carries one accessible selected artifact identity", R.registryRowHtml(parsedArtifact, true).includes('role="option"') && R.registryRowHtml(parsedArtifact, true).includes('aria-selected="true"'));
check("rendering: hostile artifact and line content is escaped", !R.registryRowHtml({ ...parsedArtifact, name: "<img onerror=alert(1)>" }, false).includes("<img") && !R.registryDiffHtml({ from: id1, to: id2, diff: R.registryDiffContract(textDiff, parsedArtifact, id1, id2) }).includes("<rule>"));
check("rendering: null joins never fabricate author or serving state", R.registryCommitHtml(R.registryHistoryContract(history, parsedArtifact)[1], 1, 2).includes("join unavailable") && R.registryCommitHtml(R.registryHistoryContract(history, parsedArtifact)[1], 1, 2).includes("author unavailable"));
check("rendering: declared-only and one-commit artifacts disclose why comparison is unavailable", R.registryDetailHtml({ history: { surface: "policy:retry", commits: [] } }, R.registryArtifactContract({ surface: "policy:retry", family: "policy", owner: { type: "system" }, created_at: "2026-08-10T09:00:00Z" })).includes("no committed versions") && R.registryDetailHtml({ history: { surface: parsedArtifact.surface, commits: [R.registryHistoryContract(history, parsedArtifact)[0]] } }, parsedArtifact).includes("second immutable commit"));
{
  const commits = Array.from({ length:R.REGISTRY_COMMIT_RENDER_LIMIT + 5 }, (_, index) => commit(index.toString(16).padStart(64, "0"), `2026-08-10T${String(Math.floor(index / 60) % 24).padStart(2, "0")}:${String(index % 60).padStart(2, "0")}:00Z`));
  const largeArtifact = R.registryArtifactContract(artifact({ commits }));
  const joined = commits.map((item) => ({ ...item, author:{ type:"system" }, status:"created" }));
  const html = R.registryDetailHtml({ history:{ surface:largeArtifact.surface, commits:joined, appended:0 }, from:joined.at(-2).candidate_id, to:joined.at(-1).candidate_id }, largeArtifact);
  check("rendering: complete legal lineage is validated while the DOM keeps a disclosed hard window", (html.match(/class="registry-commit/g) || []).length === R.REGISTRY_COMMIT_RENDER_LIMIT && html.includes(`latest ${R.REGISTRY_COMMIT_RENDER_LIMIT} of ${commits.length}`));
}
check("rendering: oversized server errors are visibly byte-bounded", R.registryErrorHtml({ message:"é".repeat(5000) }).includes("error preview truncated") && R.registryErrorHtml({ message:"é".repeat(5000) }).length < 2500);
{
  const hostileOwner = { type:"human", human_id:"\u202e" + "é".repeat(5000) };
  const hostileArtifact = R.registryArtifactContract(artifact({ owner:hostileOwner }));
  const row = R.registryRowHtml(hostileArtifact, false), error = R.registryErrorHtml({ message:"bad\u202e" + "é".repeat(5000) });
  const hostileDiff = R.registryDiffContract({ surface:"policy:system", from:id1, to:id2, diff:{ view:"structural", added:[{ path:"/\u202e" + "é".repeat(5000), value:"\u202e" + "é".repeat(5000) }], removed:[], changed:[] } }, policyArtifact, id1, id2);
  const diffHtml = R.registryDiffHtml({ from:id1, to:id2, diff:hostileDiff });
  check("rendering: legal control-heavy owner, error, path, and value evidence is explicit and byte-bounded", !row.includes("\u202e") && !error.includes("\u202e") && !diffHtml.includes("\u202e") && row.includes("exact preview truncated") && error.includes("error preview truncated") && diffHtml.includes("exact preview truncated"));
}
{
  const control = R.registryRowHtml(R.registryArtifactContract(artifact({ owner:{ type:"human", human_id:"\u202e" } })), false);
  const literal = R.registryRowHtml(R.registryArtifactContract(artifact({ owner:{ type:"human", human_id:"\\u{202e}" } })), false);
  check("rendering: visible control encoding is injective against literal escape-shaped identity text", control !== literal && control.includes("\\u{202e}") && literal.includes("\\\\u{202e}"));
}
{
  const many = Array.from({ length:R.REGISTRY_RENDER_LIMIT + 1 }, (_, index) => R.registryArtifactContract(artifact({ surface:`prompt:${String(index).padStart(4,"0")}`, commits:[] })));
  const shown = R.registryRenderWindow(many, many.at(-1).surface);
  check("rendering: the bounded catalog window retains an exact selected artifact beyond the leading rows", shown.length === R.REGISTRY_RENDER_LIMIT && shown.at(-1).surface === many.at(-1).surface);
}
const toolArtifact = R.registryArtifactContract(artifact({ surface:"tool_contract:search", family:"tool_contract", commits:[commit(id1)] }));
const modelArtifact = R.registryArtifactContract(artifact({ surface:"model_settings:chat", family:"model_settings", commits:[commit(id2)] }));
const modelArtifact2 = R.registryArtifactContract(artifact({ surface:"model_settings:backup", family:"model_settings", commits:[commit(id3)] }));
check("binding: only the three manifest-pinnable registry families are offered", R.REGISTRY_ADMISSION_FAMILIES.size === 3 && ["prompt","tool_contract","model_settings"].every((family) => R.REGISTRY_ADMISSION_FAMILIES.has(family)) && !R.REGISTRY_ADMISSION_FAMILIES.has("policy"));
{
  const tagChecks = [R.registryBindingEnvironment("") === "", R.registryBindingEnvironment("prod") === "prod", R.registryBindingEnvironment("é".repeat(32)) !== null,
    R.registryBindingEnvironment("é".repeat(33)) === null, R.registryBindingEnvironment("bad tag") === null, R.registryBindingEnvironment("bad@tag") === null,
    R.registryBindingEnvironment("bad/tag") === null, R.registryBindingEnvironment("bad\u2028tag") === null, R.registryBindingEnvironment("bad\u0000tag") === null];
  check("binding: environment tags mirror the Rust UTF-8 byte and separator grammar", tagChecks.every(Boolean), JSON.stringify(tagChecks));
}
{
  R.store.registry = { artifacts:[parsedArtifact, toolArtifact, modelArtifact, modelArtifact2], loading:false, error:null };
  const state = { enabled:true, environment:"prod", surfaces:[parsedArtifact.surface, toolArtifact.surface, modelArtifact.surface], acknowledged:true };
  R.store.registryBindings = { thread:state }; R.store.threads = [{ thread_id:"thread", graph:"pipeline" }]; R.store.selected = "thread";
  const checked = R.registryBindingValidation(state, true);
  check("binding: exact declaration order and optional environment become the server run contract", JSON.stringify(checked.binding) === JSON.stringify({ artifacts:[{family:"prompt",name:"system"},{family:"tool_contract",name:"search"},{family:"model_settings",name:"chat"}], environment:"prod" }));
  check("binding: fresh acknowledgement and one singular model slot fail closed",
    !R.registryBindingValidation({ ...state, acknowledged:false }, true).binding &&
    !R.registryBindingValidation({ ...state, surfaces:[modelArtifact.surface, modelArtifact2.surface] }, false).binding);
  check("binding: catalog drift, declared-only artifacts, and unsupported families fail closed",
    !R.registryBindingValidation({ ...state, surfaces:["prompt:missing"] }, false).binding &&
    !R.registryBindingValidation({ ...state, surfaces:["policy:system"] }, false).binding &&
    !R.registryBindingValidation({ ...state, surfaces:[R.registryArtifactContract({ surface:"prompt:empty", family:"prompt", owner:{type:"system"}, created_at:"2026-08-10T09:00:00Z" })?.surface] }, false).binding);
  const visiblePayload = { input:{ value:1 } }, prepared = R.registryBindingPrepare(visiblePayload);
  check("binding: reviewed plan is injected without mutating the visible run JSON", prepared && !Object.prototype.hasOwnProperty.call(visiblePayload, "registry") && prepared.payload !== visiblePayload && prepared.payload.registry.environment === "prod" && prepared.payload.input.value === 1 && prepared.plan.surfaces[2] === "model_settings:chat");
  state.error = "";
  check("binding: visual and raw registry declarations can never silently overwrite one another", R.registryBindingPrepare({ registry:{ artifacts:[] } }) === null && state.error.includes("already contains a registry field"));
}
{
  const explicit = R.registryBindingResolution({ surface:"model_settings:chat", tag:"prod", candidate_id:id2, pointer:"canary", digest:id3, model:"gpt-5" }, "model_settings:chat", "prod");
  check("binding evidence: exact surface, tag, candidate, pointer, digest, and model resolve", explicit?.pointer === "canary" && explicit.model === "gpt-5");
  check("binding evidence: malformed pointer, crossed environment, missing model, and stray model fail closed",
    !R.registryBindingResolution({ surface:"model_settings:chat", tag:"prod", candidate_id:id2, pointer:"shadow", digest:id3, model:"gpt-5" }, "model_settings:chat", "prod") &&
    !R.registryBindingResolution({ surface:"model_settings:chat", tag:"dev", candidate_id:id2, pointer:"active", digest:id3, model:"gpt-5" }, "model_settings:chat", "prod") &&
    !R.registryBindingResolution({ surface:"model_settings:chat", tag:"prod", candidate_id:id2, pointer:"active", digest:id3 }, "model_settings:chat", "prod") &&
    !R.registryBindingResolution({ surface:"prompt:system", tag:"prod", candidate_id:id1, pointer:"active", digest:id2, model:"stray" }, "prompt:system", "prod"));
}
{
  const bindingState = { lastSubmission:{ runId:"run-bound", threadId:"thread-bound", binding:{ environment:"prod", artifacts:[{family:"prompt",name:"system"},{family:"tool_contract",name:"search"}] }, surfaces:["prompt:system","tool_contract:search"] } };
  const events = [
    { id:"run-bound:0", run_id:"run-bound", thread_id:"thread-bound", seq:0, kind:"config_resolved", effect:"read_only", status:"ok", parent:null, output:{ kind:"inline", value:{ surface:"prompt:system", tag:"prod", candidate_id:id1, pointer:"active", digest:id2 } } },
    { id:"run-bound:1", run_id:"run-bound", thread_id:"thread-bound", seq:1, kind:"config_resolved", effect:"read_only", status:"ok", parent:"run-bound:0", output:{ kind:"inline", value:{ surface:"tool_contract:search", tag:"prod", candidate_id:id2, pointer:"canary", digest:id3 } } },
    { id:"run-bound:2", run_id:"run-bound", thread_id:"thread-bound", seq:2, kind:"super_step_start", effect:"pure", status:"ok", parent:"run-bound:1", output:{ kind:"inline", value:{} } },
  ];
  const verified = R.registryBindingEvidence({ runId:"run-bound", exactEnvelope:true, complete:true, events }, bindingState);
  check("binding evidence: complete journal proves the ordered admission chain", verified?.state === "verified" && verified.proof.length === 2 && verified.proof[1].pointer === "canary");
  check("binding evidence: partial, reordered, extra, or crossed resolution evidence never verifies",
    R.registryBindingEvidence({ runId:"run-bound", exactEnvelope:true, complete:false, events }, bindingState).state === "awaiting" &&
    R.registryBindingEvidence({ runId:"run-bound", exactEnvelope:true, complete:true, events:[events[1],events[0],events[2]] }, bindingState).state === "invalid" &&
    R.registryBindingEvidence({ runId:"run-bound", exactEnvelope:true, complete:true, events:[events[0],events[1],{...events[1],id:"run-bound:2",seq:2,parent:"run-bound:1"},events[2]] }, bindingState).state === "invalid" &&
    R.registryBindingEvidence({ runId:"run-bound", exactEnvelope:true, complete:true, events:[events[0],{...events[1],output:{kind:"inline",value:{...events[1].output.value,surface:"tool_contract:other"}}},events[2]] }, bindingState).state === "invalid" &&
    R.registryBindingEvidence({ runId:"run-bound", exactEnvelope:true, complete:true, events:[events[0],{...events[1],output:events[1].output.value},events[2]] }, bindingState).state === "invalid" &&
    R.registryBindingEvidence({ runId:"run-bound", exactEnvelope:true, complete:true, events:[events[0],{...events[1],run_id:"run-crossed"},events[2]] }, bindingState).state === "invalid" &&
    R.registryBindingEvidence({ runId:"run-bound", exactEnvelope:true, complete:true, events:[events[0],{...events[1],thread_id:"thread-crossed"},events[2]] }, bindingState).state === "invalid" &&
    R.registryBindingEvidence({ runId:"run-bound", exactEnvelope:true, complete:true, events:[events[0],{...events[1],id:"run-bound:2",seq:2,parent:"run-bound:0"},events[2]] }, bindingState).state === "invalid");
  const artifactResolution = events[1].output.value;
  const artifactCanonical = JSON.stringify({surface:"tool_contract:search",tag:"prod",candidate_id:id2,pointer:"canary",digest:id3});
  const artifactSha = Array.from(new Uint8Array(await webcrypto.subtle.digest("SHA-256", new TextEncoder().encode(artifactCanonical))), (byte) => byte.toString(16).padStart(2,"0")).join("");
  const artifactBytes = new TextEncoder().encode(artifactCanonical).length;
  const artifactEvents = [events[0], {...events[1],output:{kind:"artifact",value:{sha256:artifactSha,bytes:artifactBytes}}}, events[2]];
  const fixture = {format_version:1,journal:{run_id:"run-bound",thread_id:"thread-bound",events:artifactEvents,artifacts:{[artifactSha]:artifactResolution},artifact_refs:{}}};
  const retained = await R.registryBindingArtifactMap(fixture, bindingState.lastSubmission, artifactEvents, webcrypto);
  check("binding evidence: a verified portable journal resolves only legal hash- and byte-bound config evidence", retained && Object.keys(retained).length === 1 && R.registryBindingEvidence({ runId:"run-bound", exactEnvelope:true, complete:true, events:artifactEvents, admissionArtifacts:retained }, bindingState).state === "verified" && R.registryBindingEvidence({ runId:"run-bound", exactEnvelope:true, complete:true, events:artifactEvents, admissionArtifactsError:"fixture unavailable" }, bindingState).state === "awaiting");
  check("binding evidence: crossed fixtures, missing hashes, byte drift, and content-address drift fail closed",
    !await R.registryBindingArtifactMap({...fixture,journal:{...fixture.journal,run_id:"run-crossed"}}, bindingState.lastSubmission, artifactEvents, webcrypto) &&
    !await R.registryBindingArtifactMap({...fixture,journal:{...fixture.journal,thread_id:"thread-crossed"}}, bindingState.lastSubmission, artifactEvents, webcrypto) &&
    !await R.registryBindingArtifactMap({...fixture,journal:{...fixture.journal,events:[events[0],events[2]]}}, bindingState.lastSubmission, artifactEvents, webcrypto) &&
    !await R.registryBindingArtifactMap({...fixture,journal:{...fixture.journal,artifacts:{}}}, bindingState.lastSubmission, artifactEvents, webcrypto) &&
    !await R.registryBindingArtifactMap({...fixture,journal:{...fixture.journal,artifacts:{[artifactSha]:{...artifactResolution,tag:"drift"}}}}, bindingState.lastSubmission, artifactEvents, webcrypto) &&
    !await R.registryBindingArtifactMap(fixture, bindingState.lastSubmission, [events[0],{...artifactEvents[1],output:{kind:"artifact",value:{sha256:artifactSha,bytes:1}}},events[2]], webcrypto));
  let resolveFixture;
  sandbox.__registryApi = () => new Promise((resolve) => { resolveFixture = resolve; });
  Object.assign(sandbox, { __artifactFixture:fixture, __artifactEvents:artifactEvents, __artifactSubmission:bindingState.lastSubmission });
  vm.runInContext(`apiForConnection=globalThis.__registryApi; store.conn={baseUrl:"http://artifact-tenant",apiKey:"artifact"}; store.connectionEpoch=18;
    store.threads=[{thread_id:"thread-bound",graph:"pipeline"},{thread_id:"thread-other",graph:"pipeline"}]; store.selected="thread-bound";
    store.registryBindings={"thread-bound":{enabled:true,environment:"prod",surfaces:[],acknowledged:false,error:"",errorField:"",lastSubmission:globalThis.__artifactSubmission,submissionPending:false,uncertainSubmission:""}};
    store.recorder={runId:"run-bound",exactEnvelope:true,complete:true,events:globalThis.__artifactEvents,admissionArtifacts:null,admissionArtifactsLoading:false,admissionArtifactsError:""}; store.recLoadRequest+=1;`, sandbox);
  const artifactPending = vm.runInContext("registryBindingLoadArtifacts()", sandbox);
  vm.runInContext(`store.selected="thread-other";`, sandbox); resolveFixture(fixture); await artifactPending;
  check("binding evidence: a stale portable-evidence read always releases the captured recorder busy state", vm.runInContext("store.recorder.admissionArtifactsLoading === false && store.recorder.admissionArtifacts === null", sandbox));
  let artifactFocuses = 0;
  nodes.set("registry-binding-artifact-status", { focus() { artifactFocuses += 1; } });
  nodes.set("registry-binding-announcer", { textContent:"" });
  sandbox.__registryApi = async () => fixture;
  vm.runInContext(`apiForConnection=globalThis.__registryApi; store.selected="thread-bound"; store.recLoadRequest+=1;`, sandbox);
  const loadedArtifacts = await vm.runInContext("registryBindingLoadArtifacts()", sandbox);
  check("binding evidence focus: successful portable verification lands on the stable verified status", loadedArtifacts === true && artifactFocuses >= 2 && vm.runInContext("store.recorder.admissionArtifactsLoading === false && !!store.recorder.admissionArtifacts", sandbox));
  const focusBeforeFailure = artifactFocuses;
  sandbox.__registryApi = async () => ({...fixture,journal:{...fixture.journal,artifacts:{}}});
  vm.runInContext(`apiForConnection=globalThis.__registryApi; store.recorder.admissionArtifacts=null; store.recLoadRequest+=1;`, sandbox);
  const failedArtifacts = await vm.runInContext("registryBindingLoadArtifacts()", sandbox);
  check("binding evidence focus: failed portable verification lands on the same retryable status", failedArtifacts === false && artifactFocuses >= focusBeforeFailure + 2 && vm.runInContext("store.recorder.admissionArtifactsLoading === false && store.recorder.admissionArtifactsError.includes('could not verify')", sandbox));
  const defaultState = { lastSubmission:{ runId:"run-default", threadId:"thread-bound", binding:{ artifacts:[{family:"prompt",name:"system"}] }, surfaces:["prompt:system"] } };
  const defaultEvidence = R.registryBindingEvidence({ runId:"run-default", exactEnvelope:true, complete:true, events:[{...events[0],id:"run-default:0",run_id:"run-default",output:{kind:"inline",value:{...events[0].output.value,tag:"staging"}}}] }, defaultState);
  check("binding evidence: an omitted tag reports the deployment-selected environment without inventing it before admission", defaultEvidence?.state === "verified" && defaultEvidence.environment === "staging");
}
{
  const many = Array.from({length:R.REGISTRY_BINDING_RENDER_LIMIT + 2}, (_,index) => R.registryArtifactContract(artifact({surface:`prompt:bind-${String(index).padStart(3,"0")}`,commits:[commit(index.toString(16).padStart(64,"0"))]})));
  const shown = R.registryBindingRenderWindow(many, [many.at(-2).surface,many.at(-1).surface]);
  check("binding rendering: bounded options retain every selected artifact beyond the leading window", shown.length === R.REGISTRY_BINDING_RENDER_LIMIT && shown.at(-2).surface === many.at(-2).surface && shown.at(-1).surface === many.at(-1).surface);
  const selected = many.slice(0, R.REGISTRY_BINDING_AUTHOR_LIMIT).map((item) => item.surface);
  const overbound = {enabled:true,environment:"prod",surfaces:[...selected,many[R.REGISTRY_BINDING_AUTHOR_LIMIT].surface],acknowledged:true,submissionPending:false,uncertainSubmission:""};
  sandbox.__bindingMany = many; sandbox.__bindingSelected = selected;
  vm.runInContext(`store.threads=[{thread_id:"thread-cap",graph:"pipeline"}]; store.selected="thread-cap"; store.registry={artifacts:globalThis.__bindingMany,loading:false,error:null};
    store.registryBindings={"thread-cap":{enabled:true,environment:"prod",surfaces:[...globalThis.__bindingSelected],acknowledged:false,error:"",errorField:"",lastSubmission:null,submissionPending:false,uncertainSubmission:""}}; toast=()=>{};`, sandbox);
  const rejected = R.registryBindSurface(many[R.REGISTRY_BINDING_AUTHOR_LIMIT].surface);
  check("binding authoring: validation and Catalog handoff both stop at the fully reviewable 120-artifact ceiling", R.registryBindingValidation(overbound, true).error.includes("at most 120") && rejected === false && vm.runInContext('store.registryBindings["thread-cap"].surfaces.length', sandbox) === R.REGISTRY_BINDING_AUTHOR_LIMIT);
}
check("truth boundary: visible UI never turns catalog membership into serving or run evidence", page.includes("Catalog presence does not prove an environment or run used a version") && page.includes("Admission is separate"));
check("binding markup: one accessible page-memory planner spans environment, admission, and journal proof", page.includes('id="registry-binding-card"') && page.includes("Bind governed configuration into the next run") && page.includes("only the run journal proves the resolved versions") && page.includes("cleared on connection change, thread removal, or reload"));
check("binding integration: background, wait, and stream all pass through the same exact planner", page.includes("withRunOpts(prepared.payload)") && page.includes("withRunOpts({ ...prepared.payload })") && page.includes("JSON.stringify(prepared.payload)") && page.includes("registryBindingStreamMetadata(frame.data") && page.includes("registryBindingAccepted(runId, prepared.plan)"));
check("binding receipts: background/wait enforce their phase and thread while stream accepts one exact metadata identity", R.registryBindingRunReceipt({run_id:"run",thread_id:"thread",status:"pending"},"thread",R.REGISTRY_BACKGROUND_STATUSES) === "run" && !R.registryBindingRunReceipt({run_id:"run",thread_id:"thread",status:"success"},"thread",R.REGISTRY_BACKGROUND_STATUSES) && R.registryBindingRunReceipt({run_id:"run",thread_id:"thread",status:"success"},"thread",R.REGISTRY_TERMINAL_STATUSES) === "run" && !R.registryBindingRunReceipt({run_id:"run",thread_id:"other",status:"success"},"thread",R.REGISTRY_TERMINAL_STATUSES) && R.registryBindingStreamMetadata({run_id:"run",thread_id:"thread"},"thread",null) === "run" && !R.registryBindingStreamMetadata({run_id:"run",thread_id:"other"},"thread",null) && !R.registryBindingStreamMetadata({run_id:"run-2",thread_id:"thread"},"thread","run"));
check("binding artifact privacy: portable evidence is an explicit action and only verified referenced values survive", page.includes("Load portable evidence") && page.includes("retains only configuration-resolution values in page memory") && page.includes("retained[reference.sha256] = value"));
check("binding uncertainty: pending or ambiguous work blocks even an unbound retry until deliberate abandonment", page.indexOf("if (state.submissionPending)") < page.indexOf("if (!state.enabled)") && page.includes("if (state.submissionPending || state.uncertainSubmission)") && page.includes("Abandon uncertainty and plan another run") && page.includes("stream_identity_missing"));
check("binding identity: hostile legal environment tags are displayed through an injective visible encoding", page.includes('registryEvidencePreview(state.environment || "deployment default")') && page.includes('registryEvidencePreview(state.environment || "deployment default")}`'));
check("binding async ownership: a catalog load may update shared truth but cannot focus a different thread's planner", page.includes("const operation = connectionOperation(currentThread()?.thread_id || null)") && page.includes("await registryLoad(true);\n    if (!connectionOperationCurrent(operation)) return;"));
check("binding responsive: the admission braid, controls, options, and proof stack at mobile width", page.includes(".runtime-binding-signal { grid-template-columns:1fr; }") && page.includes(".runtime-binding-options { grid-template-columns:1fr;") && page.includes(".runtime-binding-proof { grid-template-columns:1fr; }"));
check("markup: sidebar, workspace, native filters, listbox, stable status, and exact diff surface exist", page.includes('id="btn-registry-open"') && page.includes('id="registry-view"') && page.includes('role="listbox" aria-label="Extension artifacts"') && page.includes('id="registry-announcer"') && page.includes('data-registry-compare'));
check("responsive: catalog surfaces collapse while the real structural headers remain scrollable", page.includes(".registry-layout { grid-template-columns:1fr; }") && page.includes(".registry-signal { grid-template-columns:1fr; }") && page.includes(".registry-toolbar,.registry-compare-controls { grid-template-columns:1fr; }") && page.includes(".registry-facts { grid-template-columns:1fr; }") && page.includes(".registry-structural { min-width:620px; }") && page.includes('scope="col"'));
check("connection reset and navigation: registry requests, page state, shared route, and primary action are owned", page.includes("store.registryRequest += 1") && page.includes("store.registry = null") && page.includes('const registry = store.view === "registry"') && page.includes("registry: openRegistry") && page.includes('$("btn-registry-open").onclick = openRegistry'));

/* A real direct-run flow proves the planner is not only wired by source text:
 * the reviewed declaration reaches the POST byte shape and the accepted run
 * becomes the exact evidence owner. */
{
  nodes.set("inp-payload", { value:"{}" });
  nodes.set("th-runstatus", { innerHTML:"" });
  sandbox.__runBody = null;
  sandbox.__registryApi = async (_connection, method, path, body) => {
    if (method === "POST" && path === "/threads/thread-run/runs/wait") sandbox.__runBody = body;
    return { run_id:"run-direct", thread_id:"thread-run", status:"success", result:{} };
  };
  vm.runInContext(`apiForConnection = globalThis.__registryApi; showRunResult = () => {}; refreshState = () => {}; refreshHistory = () => {}; recAutoLoad = () => {}; toast = () => {}; registryBindingRender = () => {};
    store.conn = { baseUrl:"http://tenant-run", apiKey:"run" }; store.connectionEpoch = 21;
    store.threads = [{ thread_id:"thread-run", graph:"pipeline" }]; store.selected = "thread-run";
    store.registry = { artifacts:[globalThis.__runArtifact], loading:false, error:null };
    store.registryBindings = { "thread-run":{ enabled:true, environment:"prod", surfaces:["prompt:system"], acknowledged:true, error:"", errorField:"", lastSubmission:null } };`, Object.assign(sandbox, { __runArtifact:parsedArtifact }));
  await vm.runInContext("runWait(undefined, false, false)", sandbox);
  const accepted = vm.runInContext('store.registryBindings["thread-run"]', sandbox);
  check("binding direct run: reviewed declaration reaches the exact POST and accepted run owns later proof", JSON.stringify(sandbox.__runBody) === JSON.stringify({registry:{artifacts:[{family:"prompt",name:"system"}],environment:"prod"}}) && accepted.lastSubmission?.runId === "run-direct" && accepted.lastSubmission?.threadId === "thread-run" && accepted.acknowledged === false && accepted.submissionPending === false);
  const oneShotPlan = {binding:{artifacts:[{family:"prompt",name:"system"}],environment:"prod"},surfaces:["prompt:system"],threadId:"thread-run",state:accepted};
  accepted.enabled = true; accepted.acknowledged = true;
  R.registryBindingSubmitted(oneShotPlan); accepted.enabled = false;
  const pendingBlocked = R.registryBindingPrepare({}) === null;
  R.registryBindingFailed({status:504,message:"wait timed out"}, oneShotPlan);
  const uncertainBlocked = R.registryBindingPrepare({}) === null && accepted.uncertainSubmission.includes("may have accepted");
  accepted.uncertainSubmission = ""; accepted.submissionPending = false; accepted.enabled = true; accepted.acknowledged = true;
  R.registryBindingSubmitted(oneShotPlan); R.registryBindingFailed({status:422,message:"rejected"}, oneShotPlan);
  check("binding direct run: one-shot acknowledgement locks pending and ambiguous retries but releases a confirmed rejection", pendingBlocked && uncertainBlocked && !accepted.submissionPending && !accepted.uncertainSubmission && accepted.error === "rejected");

  let resolveBound;
  sandbox.__registryApi = () => new Promise((resolve) => { resolveBound = resolve; });
  vm.runInContext(`apiForConnection = globalThis.__registryApi;
    store.threads = [{thread_id:"thread-run",graph:"pipeline"},{thread_id:"thread-new",graph:"pipeline"}]; store.selected="thread-run";
    store.registryBindings["thread-run"] = { enabled:true, environment:"prod", surfaces:["prompt:system"], acknowledged:true, error:"", errorField:"", lastSubmission:null, submissionPending:false, uncertainSubmission:"" };
    store.registryBindings["thread-new"] = { enabled:false, environment:"", surfaces:[], acknowledged:false, error:"", errorField:"", lastSubmission:null, submissionPending:false, uncertainSubmission:"" };`, sandbox);
  const deferredSuccess = vm.runInContext("runWait(undefined, false, false)", sandbox);
  vm.runInContext(`store.selected="thread-new";`, sandbox);
  resolveBound({run_id:"run-deferred",thread_id:"thread-run",status:"success",result:{}}); await deferredSuccess;
  const deferredAccepted = vm.runInContext('store.registryBindings["thread-run"]', sandbox);
  check("binding async run: exact success settles the initiating thread without taking over a newer selection", deferredAccepted.lastSubmission?.runId === "run-deferred" && !deferredAccepted.submissionPending && vm.runInContext('store.selected === "thread-new" && store.registryBindings["thread-new"].lastSubmission === null', sandbox));

  let rejectBound;
  sandbox.__registryApi = () => new Promise((_resolve, reject) => { rejectBound = reject; });
  vm.runInContext(`apiForConnection = globalThis.__registryApi; store.selected="thread-run";
    Object.assign(store.registryBindings["thread-run"], { acknowledged:true, lastSubmission:null, submissionPending:false, uncertainSubmission:"", error:"" });`, sandbox);
  const deferredFailure = vm.runInContext("runWait(undefined, false, false)", sandbox);
  vm.runInContext(`store.selected="thread-new";`, sandbox);
  rejectBound({status:504,message:"wait timed out"}); await deferredFailure;
  const deferredUncertain = vm.runInContext('store.registryBindings["thread-run"]', sandbox);
  check("binding async run: ambiguous failure unlocks busy state only into the initiating thread's uncertainty gate", !deferredUncertain.submissionPending && deferredUncertain.uncertainSubmission.includes("may have accepted") && vm.runInContext('store.selected === "thread-new" && !store.registryBindings["thread-new"].uncertainSubmission', sandbox));

  vm.runInContext(`store.selected="thread-run"; Object.assign(store.registryBindings["thread-run"], { acknowledged:true, submissionPending:false, uncertainSubmission:"", error:"" });`, sandbox);
  const streamPlan = vm.runInContext(`registryBindingPrepare({}).plan`, sandbox);
  R.registryBindingSubmitted(streamPlan);
  sandbox.__streamController = {};
  vm.runInContext(`store.streamAbort=globalThis.__streamController; store.selected="thread-new";`, sandbox);
  const streamOwned = vm.runInContext(`registryBindingStreamCurrent(globalThis.__streamController, {epoch:store.connectionEpoch,connection:{...store.conn},threadId:"thread-run"}, globalThis.__streamPlan)`, Object.assign(sandbox, {__streamPlan:streamPlan}));
  check("binding async stream: losing the initiating thread before metadata cancels ownership into a retry lock", streamOwned === false && !streamPlan.state.submissionPending && streamPlan.state.uncertainSubmission.includes("may have accepted"));
}

/* Real deferred connection and selection ownership checks. */
nodes.set("registry-side-count", { textContent: "" });
vm.runInContext(`registryRender = () => {}; toast = () => {};`, sandbox);
const listEnvelope = { artifacts: [artifact()] };
{
  sandbox.__registryApi = async () => listEnvelope;
  vm.runInContext(`apiForConnection = globalThis.__registryApi; store.conn = { baseUrl:"http://tenant-ready", apiKey:"ready" }; store.connectionEpoch = 7; store.registry = null;`, sandbox);
  await vm.runInContext("registryLoad(true)", sandbox);
  check("async success: an owned catalog response leaves loading and retains exact records", vm.runInContext("store.registry && !store.registry.loading && store.registry.artifacts.length", sandbox) === 1);
}
{
  let resolveA;
  sandbox.__registryApi = () => new Promise((resolve) => { resolveA = resolve; });
  vm.runInContext(`apiForConnection = globalThis.__registryApi; store.conn = { baseUrl:"http://tenant-a", apiKey:"a" }; store.connectionEpoch = 1; store.registry = null;`, sandbox);
  const pending = vm.runInContext("registryLoad(true)", sandbox);
  vm.runInContext(`store.conn = { baseUrl:"http://tenant-b", apiKey:"b" }; store.connectionEpoch = 2; store.registry = null;`, sandbox);
  resolveA(listEnvelope); await pending;
  check("async isolation: late catalog from tenant A cannot enter tenant B", vm.runInContext("store.registry", sandbox) === null);
}
{
  let resolveHistory;
  sandbox.__registryApi = () => new Promise((resolve) => { resolveHistory = resolve; });
  sandbox.__registryArtifacts = [parsedArtifact, R.registryArtifactContract(artifact({ surface: "prompt:other", commits: [] }))];
  vm.runInContext(`apiForConnection = globalThis.__registryApi; store.conn = { baseUrl:"http://tenant-b", apiKey:"b" }; store.connectionEpoch = 2; store.registry = { artifacts:globalThis.__registryArtifacts, selected:"prompt:system", history:null, detailLoading:false, diff:null };`, sandbox);
  const pending = vm.runInContext(`registryLoadHistory("prompt:system", false, false)`, sandbox);
  vm.runInContext(`store.registry.selected = "prompt:other"; store.registryRequest += 1;`, sandbox);
  resolveHistory(history); await pending;
  check("async selection: late history cannot replace a newer artifact choice", vm.runInContext("store.registry.history", sandbox) === null);
}
{
  const appended = { ...history, commits:[...history.commits, { ...commit(id3, "2026-08-10T12:00:00Z"), author:{ type:"system" }, status:"created" }] };
  sandbox.__registryApi = async () => appended;
  sandbox.__registryArtifacts = [R.registryArtifactContract(artifact())];
  vm.runInContext(`apiForConnection = globalThis.__registryApi; store.view="registry"; store.registryRequest += 1; store.registry = { artifacts:globalThis.__registryArtifacts, selected:"prompt:system", history:null, detailLoading:false, diff:null };`, sandbox);
  await vm.runInContext(`registryLoadHistory("prompt:system", false, false)`, sandbox);
  check("async snapshots: a concurrent append reconciles catalog truth and discloses the suffix", vm.runInContext("store.registry.artifacts[0].commits.length === 3 && store.registry.history.appended === 1", sandbox));
}
{
  let resolveDiff;
  const policy = R.registryArtifactContract(artifact({ surface:"policy:system", family:"policy", commits:[commit(id1), commit(id2), commit(id3)] }));
  const joined = [
    { ...commit(id1), author:{ type:"system" }, status:"created" },
    { ...commit(id2, "2026-08-10T11:00:00Z"), author:{ type:"system" }, status:"evaluated" },
    { ...commit(id3, "2026-08-10T12:00:00Z"), author:{ type:"system" }, status:"promoted" },
  ];
  nodes.set("sel-registry-from", { value:id1 }); nodes.set("sel-registry-to", { value:id2 });
  nodes.set("registry-statusline", { focus() {} }); nodes.set("registry-announcer", { textContent:"" });
  sandbox.__registryApi = () => new Promise((resolve) => { resolveDiff = resolve; });
  sandbox.__registryArtifacts = [policy]; sandbox.__registryHistory = joined;
  vm.runInContext(`apiForConnection = globalThis.__registryApi; store.view="registry"; store.registryRequest += 1; store.registry = { artifacts:globalThis.__registryArtifacts, selected:"policy:system", history:{surface:"policy:system",commits:globalThis.__registryHistory}, from:"${id1}", to:"${id2}", detailLoading:false, diff:null, diffLoading:false };`, sandbox);
  const pending = vm.runInContext("registryCompare()", sandbox);
  vm.runInContext(`store.registry.from="${id3}"; store.registry.diffLoading=false;`, sandbox);
  resolveDiff({ surface:"policy:system", from:id1, to:id2, diff:{ view:"structural", added:[], removed:[], changed:[] } }); await pending;
  check("async comparison: a deferred old pair cannot render beneath newer controls", vm.runInContext("store.registry.diff", sandbox) === null);
}
{
  let resolveHistory, focuses = 0;
  nodes.set("registry-statusline", { focus() { focuses += 1; } }); nodes.set("registry-detail-title", { focus() { focuses += 1; } });
  sandbox.__registryApi = () => new Promise((resolve) => { resolveHistory = resolve; }); sandbox.__registryArtifacts = [parsedArtifact];
  vm.runInContext(`apiForConnection = globalThis.__registryApi; store.view="registry"; store.registryRequest += 1; store.registry = { artifacts:globalThis.__registryArtifacts, selected:"prompt:system", history:null, detailLoading:false, diff:null };`, sandbox);
  const pending = vm.runInContext(`registryLoadHistory("prompt:system", true, true)`, sandbox);
  vm.runInContext(`store.view="home";`, sandbox); resolveHistory(history); await pending;
  check("async navigation: background history may refresh truth but cannot focus a workspace the user left", focuses === 1 && vm.runInContext("store.registry.history.surface", sandbox) === "prompt:system");
}
{
  const many = Array.from({ length:R.REGISTRY_RENDER_LIMIT + 1 }, (_, index) => artifact({ surface:`prompt:${String(index).padStart(4,"0")}`, commits:[] }));
  const selected = many.at(-1);
  for (const [id, node] of [
    ["registry-summary", { innerHTML:"" }], ["registry-statusline", { textContent:"" }], ["btn-registry-refresh", { disabled:false }],
    ["registry-workspace", { setAttribute() {} }], ["registry-list", { innerHTML:"", querySelector() { return null; } }],
    ["registry-detail", { innerHTML:"", querySelector() { return null; } }], ["inp-registry-search", { value:"" }], ["sel-registry-family", { value:"" }],
  ]) nodes.set(id, node);
  sandbox.__registryList = { artifacts:many }; sandbox.__registrySelected = selected.surface;
  sandbox.__registryApi = async (_connection, _method, route) => route.endsWith("/commits") ? { surface:selected.surface, family:selected.family, owner:selected.owner, commits:[] } : sandbox.__registryList;
  vm.runInContext(`registryRender = globalThis.__registry.registryRender; apiForConnection = globalThis.__registryApi; store.view="registry"; store.registryRequest += 1; store.registry = { artifacts:[], selected:globalThis.__registrySelected, loading:false, history:null, diff:null };`, sandbox);
  await vm.runInContext("registryLoad(true)", sandbox); await new Promise((resolve) => setImmediate(resolve));
  check("async refresh: a selected artifact beyond the leading DOM window remains owned and cannot strand loading", vm.runInContext("store.registry.selected === globalThis.__registrySelected && store.registry.detailLoading === false && store.registry.history.surface === globalThis.__registrySelected", sandbox), vm.runInContext("JSON.stringify({selected:store.registry.selected, loading:store.registry.detailLoading, history:store.registry.history, error:store.registry.error && store.registry.error.message, expected:globalThis.__registrySelected})", sandbox));
  vm.runInContext(`registryRender = () => {};`, sandbox);
}

console.log(`\n${passed} passed, ${failed} failed`);
if (failed) process.exit(1);
