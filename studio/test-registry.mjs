#!/usr/bin/env node
/* Extension Catalog contract tests. The Studio script is evaluated without
 * bootstrap so the real registry parsing, rendering, and async ownership
 * helpers are exercised instead of copied into fixtures. */
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";
import vm from "node:vm";

const here = path.dirname(fileURLToPath(import.meta.url));
const page = readFileSync(path.join(here, "index.html"), "utf8");
const match = page.match(/<script>([\s\S]*?)<\/script>/);
if (!match) throw new Error("Studio script not found");
const src = match[1].replace(/\ninit\(\);\s*$/, "\n");
const nodes = new Map();
const sandbox = { document: { getElementById: (id) => nodes.get(id) || null }, TextDecoder, TextEncoder, URL, URLSearchParams };
vm.createContext(sandbox);
vm.runInContext(src + `
globalThis.__registry = {
  registryObject, registryText, registryRustTextCompare, registryTimestamp, registryCandidateId,
  registryOwner, registryOwnerText, registryArtifactName, registryCommit,
  registryArtifactContract, registryListContract, registryHistoryContract,
  registryExactValue, registryDiffContract, registryVisible, registryRenderWindow, registryErrorHtml,
  registrySummaryHtml, registryRowHtml, registryCommitHtml, registryDiffHtml,
  registryDetailHtml, registryRender, registryLoad, registryLoadHistory, registryCompare,
  agentParseJsonWithNumberKinds, REGISTRY_FAMILIES, REGISTRY_RENDER_LIMIT,
  REGISTRY_COMMIT_RENDER_LIMIT, REGISTRY_DIFF_RENDER_LIMIT, store,
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
check("truth boundary: visible UI never turns catalog membership into serving or run evidence", page.includes("Catalog presence does not prove an environment or run used a version") && page.includes("Admission is separate"));
check("markup: sidebar, workspace, native filters, listbox, stable status, and exact diff surface exist", page.includes('id="btn-registry-open"') && page.includes('id="registry-view"') && page.includes('role="listbox" aria-label="Extension artifacts"') && page.includes('id="registry-announcer"') && page.includes('data-registry-compare'));
check("responsive: catalog surfaces collapse while the real structural headers remain scrollable", page.includes(".registry-layout { grid-template-columns:1fr; }") && page.includes(".registry-signal { grid-template-columns:1fr; }") && page.includes(".registry-toolbar,.registry-compare-controls { grid-template-columns:1fr; }") && page.includes(".registry-facts { grid-template-columns:1fr; }") && page.includes(".registry-structural { min-width:620px; }") && page.includes('scope="col"'));
check("connection reset and navigation: registry requests, page state, shared route, and primary action are owned", page.includes("store.registryRequest += 1") && page.includes("store.registry = null") && page.includes('const registry = store.view === "registry"') && page.includes("registry: openRegistry") && page.includes('$("btn-registry-open").onclick = openRegistry'));

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
