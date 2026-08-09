#!/usr/bin/env node
/* Focused tests for Studio's Connection Hub. The browser bootstrap is
 * stripped; pure persistence, validation, compatibility, and rendering
 * contracts run under vm with isolated browser storage.
 */
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";
import vm from "node:vm";

const here = path.dirname(fileURLToPath(import.meta.url));
const html = readFileSync(path.join(here, "index.html"), "utf8");
const match = html.match(/<script>([\s\S]*?)<\/script>/);
if (!match) { console.error("FAIL: no script block"); process.exit(1); }
const src = match[1].replace(/\ninit\(\);\s*$/, "\n");

function storage(map) {
  return {
    getItem(key) { return map.has(key) ? map.get(key) : null; },
    setItem(key, value) { map.set(key, String(value)); },
    removeItem(key) { map.delete(key); },
  };
}

const localData = new Map();
const sessionData = new Map();
const sandbox = {
  localStorage: storage(localData),
  sessionStorage: storage(sessionData),
  crypto: { getRandomValues(bytes) { bytes[0] = 123456; bytes[1] = 789012; return bytes; } },
  URL,
  AbortController,
  TextEncoder,
  setTimeout() { return 1; },
  clearTimeout() {},
  document: { getElementById() { return null; } },
};
vm.createContext(sandbox);
vm.runInContext(src + `
globalThis.__connection = {
  LS, store, connectionNormalizeBaseUrl, connectionDefaultProfileName,
  connectionLocalStorage, connectionStorageRemove, connectionNormalizeProfile, connectionProfilesResult, connectionParseProfiles,
  loadConnectionProfiles, saveConnectionProfiles, connectionAddPersistenceWarning,
  connectionParseSecrets, connectionStoreSecret, connectionLoadSecret, connectionForgetSecret, connectionPruneSecrets,
  connectionRememberProfile, connectionAcceptVerifiedProfile, loadConn, saveConn, connectionRunScope,
  loadThreads, connectionSemver, connectionInfoContract, connectionFailureMessage,
  connectionCapabilityFromError, connectionCapabilityFromSuccess,
  connectionHandshakeHtml, connectionCompatibilityHtml, connectionProfilesHtml,
  connectionValidateDraft, connectionIdentityChanged, connectionOperation, connectionOperationCurrent, connectionAfterAttempt,
  connectionCompatibilityCurrent, connectionConcealSecret, connectionSetSubmitting,
  connectionRenderChip, createThread, refreshHistory, recLoad,
  recReplay, recCompare, recReplayOperationCurrent, recCompareOperationCurrent, runStream, streamOperationCurrent,
  tasksRequestCurrent, tasksLoad, CONNECTION_CAPABILITIES,
};`, sandbox, { filename: "index.html<script>" });
const C = sandbox.__connection;

let passed = 0, failed = 0;
function check(name, condition, detail = "") {
  if (condition) { passed++; console.log(`ok   ${name}`); }
  else { failed++; console.log(`FAIL ${name}${detail ? ` — ${detail}` : ""}`); }
}

{
  const local = C.connectionNormalizeBaseUrl("http://127.0.0.1:8100///");
  const proxy = C.connectionNormalizeBaseUrl("/api/");
  check("address: local and relative Rusty endpoints normalize without changing their origin",
    local.value === "http://127.0.0.1:8100" && !local.error && proxy.value === "/api" && !proxy.error);
  check("address: embedded credentials, query strings, and unsupported schemes fail before fetch",
    Boolean(C.connectionNormalizeBaseUrl("https://user:secret@example.com").error) &&
    Boolean(C.connectionNormalizeBaseUrl("https://example.com?tenant=a").error) &&
    Boolean(C.connectionNormalizeBaseUrl("file:///tmp/server").error));
  check("address: control characters and oversized values fail closed",
    Boolean(C.connectionNormalizeBaseUrl("http://host\t/path").error) &&
    Boolean(C.connectionNormalizeBaseUrl(`http://host/${"a".repeat(2100)}`).error));
}

const validInfo = {
  service: "rusty-server", version: "0.9.0", checkpointer: "json_file", server_store: "json_file",
  store_path: "/private/server/path", graphs: [{ name: "pipeline", channels: ["log"] }],
};
{
  const accepted = C.connectionInfoContract(validInfo);
  check("identity: exact Rusty service, semantic version, and behavior catalog are accepted",
    !accepted.error && accepted.info.service === "rusty-server" && accepted.info.graphs[0].name === "pipeline");
  check("identity: unnecessary server filesystem paths never enter the retained client contract",
    !JSON.stringify(accepted.info).includes("private/server/path") && !("store_path" in accepted.info));
  check("identity: wrong services, malformed versions, duplicate graphs, and invalid channels fail closed",
    Boolean(C.connectionInfoContract({ ...validInfo, service: "not-rusty" }).error) &&
    Boolean(C.connectionInfoContract({ ...validInfo, version: 9 }).error) &&
    Boolean(C.connectionInfoContract({ ...validInfo, graphs: [{ name: "x", channels: [] }, { name: "x", channels: [] }] }).error) &&
    Boolean(C.connectionInfoContract({ ...validInfo, graphs: [{ name: "x", channels: "log" }] }).error));
  check("identity: SemVer 2 prerelease/build forms are exact rather than loosely version-shaped",
    C.connectionSemver("1.2.3-rc.1+build.7") && C.connectionSemver("0.0.0") &&
    !C.connectionSemver("01.2.3") && !C.connectionSemver("1.2.3-.") && !C.connectionSemver("1.2.3-01"));
}

const profile = {
  profile_id: "profile_local_1", name: "Local <dev>", base_url: "http://127.0.0.1:8100",
  service: "rusty-server", version: "0.9.0", last_connected_at: "2026-08-09T12:00:00Z", secret_mode: "session",
};
{
  const parsed = C.connectionParseProfiles({ format: "rusty.connection-profiles/v1", profiles: [profile, profile, null] });
  check("profiles: strict normalization deduplicates identity and preserves only non-secret metadata",
    parsed.length === 1 && parsed[0].name === "Local <dev>" && !JSON.stringify(parsed).includes("apiKey"));
  check("profiles: malformed envelopes and unsafe profile identifiers fail closed",
    C.connectionParseProfiles("{").length === 0 && !C.connectionNormalizeProfile({ ...profile, profile_id: "bad id" }));
  check("profiles: oversized persisted envelopes fail closed before parsing",
    C.connectionParseProfiles("x".repeat(32769)).length === 0 &&
    Object.keys(C.connectionParseSecrets("x".repeat(32769))).length === 0);
  check("profiles: corrupt and partially invalid storage carries a visible recovery warning",
    Boolean(C.connectionProfilesResult("{").warning) &&
    Boolean(C.connectionProfilesResult({ format: "rusty.connection-profiles/v1", profiles: [profile, null] }).warning));
  const markup = C.connectionProfilesHtml(parsed, profile.profile_id, profile.profile_id);
  check("profiles: hostile names render as text and the active profile remains explicit",
    markup.includes("Local &lt;dev&gt;") && !markup.includes("Local <dev>") && markup.includes("active"));
}

{
  localData.clear(); sessionData.clear();
  check("secrets: session-only is the default durable boundary",
    C.connectionStoreSecret(profile.profile_id, "session-key", "session") &&
    !JSON.stringify([...localData]).includes("session-key") && JSON.stringify([...sessionData]).includes("session-key"));
  check("secrets: a session key can be recovered only from session storage",
    C.connectionLoadSecret(profile) === "session-key");
  check("secrets: explicit device-local opt-in moves the key out of session storage",
    C.connectionStoreSecret(profile.profile_id, "local-key", "local") &&
    JSON.stringify([...localData]).includes("local-key") && !JSON.stringify([...sessionData]).includes("local-key"));
  const localProfile = { ...profile, secret_mode: "local" };
  check("secrets: locally opted-in profiles recover their key and forgetting scrubs both stores",
    C.connectionLoadSecret(localProfile) === "local-key" && (C.connectionForgetSecret(profile.profile_id), true) &&
    !JSON.stringify([...localData, ...sessionData]).includes("local-key"));
  const normalStorage = sandbox.localStorage;
  sandbox.localStorage = { getItem() { throw new Error("blocked"); }, setItem() { throw new Error("blocked"); }, removeItem() { throw new Error("blocked"); } };
  check("secrets: blocked browser storage is contained and reported without throwing",
    C.connectionStoreSecret(profile.profile_id, "key", "local") === false);
  const trapped = new Map([[C.LS.connectionSecrets, JSON.stringify({ [profile.profile_id]: "old-local-key" })]]);
  sandbox.localStorage = {
    getItem(key) { return trapped.get(key) || null; },
    setItem() { throw new Error("write blocked"); },
    removeItem() { throw new Error("remove blocked"); },
  };
  check("secrets: a failed local cleanup cannot be reported as a successful session downgrade",
    C.connectionForgetSecret(profile.profile_id) === false &&
    C.connectionStoreSecret(profile.profile_id, "new-session-key", "session") === false &&
    trapped.get(C.LS.connectionSecrets).includes("old-local-key"));
  sandbox.localStorage = normalStorage;

  localData.set(C.LS.connectionSecrets, "{");
  C.store.connectionPrivacyWarning = "";
  check("secrets: damaged device-local key storage is visible without exposing its contents",
    C.connectionLoadSecret({ ...profile, secret_mode: "local" }) === "" &&
    C.store.connectionPrivacyWarning.includes("damaged browser key storage"));
  sandbox.localStorage = { getItem() { throw new Error("read blocked"); }, setItem() {}, removeItem() {} };
  C.store.connectionPrivacyWarning = "";
  check("secrets: unreadable device-local key storage leaves a persistent cleanup warning",
    C.connectionLoadSecret({ ...profile, secret_mode: "local" }) === "" &&
    C.store.connectionPrivacyWarning.includes("could not safely read"));
  sandbox.localStorage = normalStorage;
}

{
  const normalStorage = sandbox.localStorage;
  Object.defineProperty(sandbox, "localStorage", { configurable: true, get() { throw new Error("property blocked"); } });
  let survived = true;
  try { C.loadConnectionProfiles(); } catch { survived = false; }
  check("storage: a SecurityError while resolving localStorage cannot abort Studio startup",
    survived && C.store.connectionProfiles.length === 0 && C.store.connectionPersistenceWarning.includes("unavailable"));
  Object.defineProperty(sandbox, "localStorage", { configurable: true, writable: true, value: normalStorage });
  localData.set(C.LS.connectionProfiles, "{");
  C.loadConnectionProfiles();
  check("storage: damaged profile data is visible instead of becoming a silent empty list",
    C.store.connectionProfiles.length === 0 && C.store.connectionPersistenceWarning.includes("damaged"));
  localData.set(C.LS.conn, JSON.stringify({ baseUrl: profile.base_url, profileId: profile.profile_id }));
  const recovered = C.loadConn();
  const acceptedRecovery = C.connectionAcceptVerifiedProfile({ profileId: recovered.profileId,
    name: "Recovered", baseUrl: recovered.baseUrl, secretMode: "session" }, validInfo, recovered.preserveProfileStorage);
  check("storage: a successful automatic handshake never overwrites a damaged profile envelope",
    acceptedRecovery.version === validInfo.version && localData.get(C.LS.connectionProfiles) === "{" &&
    C.store.connectionPersistenceWarning.includes("Reconnect and save explicitly"));
  C.connectionAddPersistenceWarning("The active profile could not be remembered for reload.");
  check("storage: pointer failure cannot hide the damaged-profile recovery action",
    C.store.connectionPersistenceWarning.includes("Reconnect and save explicitly") &&
    C.store.connectionPersistenceWarning.includes("could not be remembered for reload"));
}

{
  localData.clear(); sessionData.clear();
  C.store.connectionPersistenceWarning = "";
  C.store.connectionPrivacyWarning = "";
  C.store.connectionProfiles = [profile];
  C.store.conn = { baseUrl: profile.base_url, apiKey: "must-not-persist", profileId: profile.profile_id };
  C.saveConn();
  const saved = localData.get(C.LS.conn);
  check("active connection: browser persistence contains profile identity but never the access key",
    saved.includes(profile.profile_id) && saved.includes(profile.base_url) && !saved.includes("must-not-persist"));

  localData.set(C.LS.conn, JSON.stringify({ baseUrl: profile.base_url, apiKey: "legacy-key", profileId: profile.profile_id }));
  localData.set(C.LS.connectionProfiles, JSON.stringify({ format: "rusty.connection-profiles/v1", profiles: [profile] }));
  C.loadConnectionProfiles();
  C.store.conn = null;
  const startup = C.loadConn();
  check("migration: a legacy key becomes an unverified startup candidate and leaves sanitized metadata",
    C.store.conn === null && startup.apiKey === "legacy-key" && JSON.stringify([...sessionData]).includes("legacy-key") &&
    !localData.get(C.LS.conn).includes("legacy-key"));

  const normalStorage = sandbox.localStorage;
  const blockedData = new Map([[C.LS.conn, JSON.stringify({ baseUrl: profile.base_url, apiKey: "trapped-legacy", profileId: profile.profile_id })]]);
  sandbox.localStorage = {
    getItem(key) { return blockedData.get(key) || null; },
    setItem() { throw new Error("write blocked"); },
    removeItem() { throw new Error("remove blocked"); },
  };
  C.store.connectionProfiles = [profile];
  C.store.connectionPrivacyWarning = "";
  const trappedStartup = C.loadConn();
  check("migration: failed legacy-key scrubbing remains explicit and never becomes active state",
    trappedStartup.apiKey === "trapped-legacy" && C.store.conn === null &&
    blockedData.get(C.LS.conn).includes("trapped-legacy") && C.store.connectionPrivacyWarning.includes("could not remove"));
  sandbox.localStorage = normalStorage;

  localData.set(C.LS.conn, JSON.stringify({ baseUrl: "javascript:alert(1)", apiKey: "unsafe-legacy" }));
  C.store.connectionPrivacyWarning = "";
  check("migration: an invalid active pointer is scrubbed before its legacy key can be ignored",
    C.loadConn() === null && !localData.has(C.LS.conn) && !JSON.stringify([...localData]).includes("unsafe-legacy"));

  const malformedData = new Map([[C.LS.conn, '{"apiKey":"possibly-trapped"']]);
  sandbox.localStorage = {
    getItem(key) { return malformedData.get(key) || null; },
    setItem() { throw new Error("write blocked"); },
    removeItem() { throw new Error("remove blocked"); },
  };
  C.store.connectionPrivacyWarning = "";
  check("migration: an unreadable active pointer warns when potential plaintext cannot be scrubbed",
    C.loadConn() === null && malformedData.has(C.LS.conn) &&
    C.store.connectionPrivacyWarning.includes("may contain a plaintext access key"));
  sandbox.localStorage = normalStorage;
}

{
  C.store.connectionProfiles = [];
  for (let i = 0; i < 20; i++) C.connectionRememberProfile({
    profileId: `profile_${String(i).padStart(8, "0")}`, name: `Server ${i}`,
    baseUrl: `http://127.0.0.1:${8100 + i}`, secretMode: "session",
  }, validInfo);
  check("profiles: saved inventory obeys a hard global count bound",
    C.store.connectionProfiles.length === 12 && C.connectionParseProfiles(localData.get(C.LS.connectionProfiles)).length === 12);
  const evictedId = "profile_00000000";
  C.connectionStoreSecret(evictedId, "evicted-local-key", "local");
  C.connectionRememberProfile({
    profileId: "profile_00000020", name: "Server 20", baseUrl: "http://127.0.0.1:8120", secretMode: "session",
  }, validInfo);
  check("profiles: bounded eviction also removes the evicted profile's device-local key",
    !C.store.connectionProfiles.some((item) => item.profile_id === evictedId) &&
    !JSON.stringify([...localData]).includes("evicted-local-key"));
}

{
  check("compatibility: a JSON missing-record response confirms the route contract",
    C.connectionCapabilityFromError({ status: 404, body: { error: "not_found" } }).state === "supported");
  check("compatibility: route-less, authorization, and transport outcomes stay distinct",
    C.connectionCapabilityFromError({ status: 404, body: { raw: "not found" } }).state === "unavailable" &&
    C.connectionCapabilityFromError({ status: 403, body: { error: "forbidden" } }).state === "locked" &&
    C.connectionCapabilityFromError({ status: 0 }).state === "error");
  check("compatibility: a proxy's JSON route-missing fallback cannot become false availability",
    C.connectionCapabilityFromError({ status: 404, body: { error: "route_not_found" } }).state === "unavailable" &&
    C.connectionCapabilityFromError({ status: 404, body: { error: "not_found" } }).state === "supported");
  check("compatibility: a generic successful probe cannot masquerade as a missing-record contract",
    C.connectionCapabilityFromSuccess().state === "unverified" &&
    C.connectionCapabilityFromSuccess().note.includes("not confirmed"));
  check("compatibility: every probe is a read-only missing-record GET against a bounded surface",
    C.CONNECTION_CAPABILITIES.length === 6 && C.CONNECTION_CAPABILITIES.every((item) => item.path.includes("__rusty_studio_capability_probe__")));
}

{
  const capabilities = Object.create(null);
  for (const item of C.CONNECTION_CAPABILITIES) capabilities[item.key] = { state: "supported", note: "Route contract confirmed" };
  const state = { phase: "ready", info: C.connectionInfoContract(validInfo).info, capabilities };
  const markup = C.connectionCompatibilityHtml(state,
    { baseUrl: "http://127.0.0.1:8100", apiKey: "never-render-this" }, profile);
  check("compatibility: identity, persistence, behaviors, and six feature surfaces are legible",
    markup.includes("rusty-server v0.9.0") && markup.includes("json_file checkpoints") &&
    markup.includes("1 registered") && (markup.match(/Route contract confirmed/g) || []).length === 6);
  check("compatibility: access-key presence is disclosed without rendering the credential",
    markup.includes("Access key supplied") && !markup.includes("never-render-this"));
  check("handshake: connecting, failed, and ready phases remain semantically distinct",
    C.connectionHandshakeHtml({ phase: "connecting" }).includes("active") &&
    C.connectionHandshakeHtml({ phase: "failed" }).includes("failed") &&
    (C.connectionHandshakeHtml(state).match(/ready/g) || []).length === 3);
}

{
  check("draft: default profile names are derived only after a valid address",
    C.connectionValidateDraft({ name: "", baseUrl: "http://localhost:8100", apiKey: "", secretMode: "session" }).value.name === "localhost:8100" &&
    Boolean(C.connectionValidateDraft({ name: "Local", baseUrl: "bad", apiKey: "", secretMode: "session" }).error));
  check("switching: failed candidates preserve the active connection and identity changes remain explicit",
    C.connectionAfterAttempt(profile, { baseUrl: "http://other" }, false) === profile &&
    C.connectionIdentityChanged({ baseUrl: "/api", apiKey: "a" }, { baseUrl: "/api", apiKey: "b" }));
  C.store.connectionCompatibilityRequest = 8;
  C.store.conn = { baseUrl: "/api", apiKey: "tenant-b" };
  check("switching: capability evidence is generation- and tenant-bound",
    C.connectionCompatibilityCurrent(8, { baseUrl: "/api", apiKey: "tenant-b" }) &&
    !C.connectionCompatibilityCurrent(7, { baseUrl: "/api", apiKey: "tenant-b" }) &&
    !C.connectionCompatibilityCurrent(8, { baseUrl: "/api", apiKey: "tenant-a" }));
  check("threads: local thread history is isolated by server and access boundary",
    C.connectionRunScope({ baseUrl: "/api", apiKey: "tenant-a" }) !==
    C.connectionRunScope({ baseUrl: "/api", apiKey: "tenant-b" }));
  C.store.info = null;
  const failedStartup = C.connectionCompatibilityHtml({ phase: "failed", error: "offline" },
    { baseUrl: "/api", apiKey: "tenant-b" }, profile);
  check("switching: an unverified startup candidate is never described as an active workspace",
    !failedStartup.includes("still using"));

  localData.clear();
  C.store.conn = { baseUrl: "/api", apiKey: "tenant-b" };
  localData.set(C.LS.threads, JSON.stringify({ "/api": [{ thread_id: "legacy-thread", graph: "pipeline" }] }));
  localData.set(C.LS.sel, JSON.stringify({ "/api": "legacy-thread" }));
  C.loadThreads();
  check("threads: legacy server-only recall never crosses into an authenticated access boundary",
    C.store.threads.length === 0 && C.store.selected === null);
}

{
  const originalDocument = sandbox.document;
  const input = { type: "text" };
  const button = { disabled: true, textContent: "", attrs: Object.create(null), setAttribute(name, value) { this.attrs[name] = value; } };
  const elements = { "inp-key": input, "btn-key-reveal": button, "btn-connect": button };
  sandbox.document = { getElementById(id) { return elements[id] || null; } };
  C.connectionConcealSecret();
  C.store.conn = null; C.store.info = null;
  C.connectionSetSubmitting(true);
  C.connectionSetSubmitting(false);
  check("interaction: closing or switching profiles reconceals a previously revealed key",
    input.type === "password" && button.attrs["aria-pressed"] === "false");
  check("interaction: cancelling an in-flight connection always restores the connect control",
    button.disabled === false && button.textContent === "Connect and save");
  sandbox.document = originalDocument;
}

{
  const originalDocument = sandbox.document;
  const classes = new Set();
  const chip = { classList: { toggle(name, on) { if (on) classes.add(name); else classes.delete(name); } } };
  const warning = { hidden: true, textContent: "" };
  const announcer = { textContent: "" };
  const nodes = {
    "btn-connection-open": chip, "conn-profile": { textContent: "" }, "conn-status": { textContent: "" },
    "conn-warning": warning, "connection-global-announcer": announcer,
  };
  sandbox.document = { getElementById(id) { return nodes[id] || null; } };
  C.store.connectionProfiles = [profile];
  C.store.conn = { baseUrl: profile.base_url, apiKey: "legacy", profileId: profile.profile_id };
  C.store.info = C.connectionInfoContract(validInfo).info;
  C.store.connectionPersistenceWarning = "";
  C.store.connectionPrivacyWarning = "A legacy plaintext key may remain.";
  C.connectionRenderChip();
  check("privacy: a successful reconnect cannot hide failed legacy-key cleanup behind the closed dialog",
    classes.has("connected") && classes.has("warning") && !warning.hidden &&
    warning.textContent === "Security cleanup needed" && announcer.textContent.includes("legacy plaintext key"));
  C.store.connectionPrivacyWarning = "";
  C.store.connectionPersistenceWarning = "The reconnect pointer could not be removed.";
  C.connectionRenderChip();
  check("disconnect: reconnect-pointer persistence failure remains visible outside the closed dialog",
    classes.has("warning") && warning.textContent === "Browser storage warning" &&
    announcer.textContent.includes("reconnect pointer"));
  sandbox.document = originalDocument;
}

{
  const originalStorage = sandbox.localStorage;
  Object.defineProperty(sandbox, "localStorage", { configurable: true, get() { throw new Error("property blocked"); } });
  C.store.conn = null;
  check("disconnect: an unavailable storage property cannot falsely confirm reconnect-pointer removal",
    C.connectionStorageRemove(null, C.LS.conn) === false && C.saveConn() === false &&
    html.includes("It may try this profile again after reload"));
  Object.defineProperty(sandbox, "localStorage", { configurable: true, writable: true, value: originalStorage });
}

{
  const originalDocument = sandbox.document;
  const originalFetch = sandbox.fetch;
  const nodes = {
    "sel-task-status": { value: "" }, "tasks-statusline": { textContent: "" },
    "tasks-body": { innerHTML: "" }, "tasks-detail": { innerHTML: "", style: {} },
  };
  sandbox.document = { getElementById(id) { return nodes[id] || null; } };
  const pending = new Map();
  sandbox.fetch = (url) => new Promise((resolve) => pending.set(String(url), resolve));
  const response = (body) => ({ ok: true, status: 200, text: async () => JSON.stringify(body) });
  C.store.taskRequest = 0;
  C.store.conn = { baseUrl: "http://tenant-a", apiKey: "a" };
  const first = C.tasksLoad(true);
  C.store.conn = { baseUrl: "http://tenant-b", apiKey: "b" };
  const second = C.tasksLoad(true);
  pending.get("http://tenant-b/tasks")(response([]));
  await second;
  pending.get("http://tenant-a/tasks")(response([{ task_id: "stale-a" }]));
  const staleApplied = await first;
  check("switching: a late task response from the previous tenant cannot overwrite the active catalog",
    staleApplied === false && C.store.tasks.list.length === 0 && C.store.conn.baseUrl === "http://tenant-b");
  sandbox.document = originalDocument;
  sandbox.fetch = originalFetch;
}

{
  const originalDocument = sandbox.document;
  const originalFetch = sandbox.fetch;
  const response = (body) => ({ ok: true, status: 200, text: async () => JSON.stringify(body) });
  let resolveFetch;
  sandbox.fetch = () => new Promise((resolve) => { resolveFetch = resolve; });

  C.store.connectionEpoch = 20;
  C.store.conn = { baseUrl: "http://tenant-a", apiKey: "a" };
  C.store.threads = [];
  const creating = C.createThread("pipeline");
  C.store.connectionEpoch += 1;
  C.store.conn = { baseUrl: "http://tenant-b", apiKey: "b" };
  resolveFetch(response({ thread_id: "thread-from-a", graph: "pipeline" }));
  const created = await creating;
  check("switching: a late create-thread receipt cannot enter the next tenant's local scope",
    created === null && C.store.threads.length === 0);

  const historyNode = { innerHTML: "" };
  sandbox.document = { getElementById(id) { return id === "history-list" ? historyNode : null; }, createElement() { return {}; } };
  C.store.connectionEpoch = 30;
  C.store.conn = { baseUrl: "http://tenant-a", apiKey: "a" };
  C.store.threads = [{ thread_id: "thread-a", graph: "pipeline" }];
  C.store.selected = "thread-a";
  C.store.history = ["keep-b"];
  const history = C.refreshHistory();
  C.store.connectionEpoch += 1;
  C.store.conn = { baseUrl: "http://tenant-b", apiKey: "b" };
  C.store.threads = [{ thread_id: "thread-b", graph: "pipeline" }];
  C.store.selected = "thread-b";
  resolveFetch(response([{ checkpoint: { checkpoint_id: "from-a" } }]));
  await history;
  check("switching: late checkpoint history cannot repopulate a new tenant workspace",
    C.store.history.length === 1 && C.store.history[0] === "keep-b");

  const recorderNodes = { "rec-statusline": { textContent: "" } };
  sandbox.document = { getElementById(id) { return recorderNodes[id] || null; } };
  C.store.connectionEpoch = 40;
  C.store.conn = { baseUrl: "http://tenant-a", apiKey: "a" };
  C.store.threads = [{ thread_id: "thread-a", graph: "pipeline" }];
  C.store.selected = "thread-a";
  C.store.recorder = null;
  const recording = C.recLoad("run-a", true);
  C.store.connectionEpoch += 1;
  C.store.conn = { baseUrl: "http://tenant-b", apiKey: "b" };
  resolveFetch(response({ run_id: "run-a", complete: true, events: [] }));
  await recording;
  check("switching: a late recorder response cannot restore prior-tenant evidence",
    C.store.recorder === null);

  sandbox.document = originalDocument;
  sandbox.fetch = originalFetch;
}

{
  const originalDocument = sandbox.document;
  const originalFetch = sandbox.fetch;
  const response = (body) => ({ ok: true, status: 200, text: async () => JSON.stringify(body) });
  const pending = [];
  sandbox.fetch = (url) => new Promise((resolve) => pending.push({ url: String(url), resolve }));
  const replayButton = { disabled: false };
  const compareButton = { disabled: false };
  const inputs = {
    "inp-cmp-base": { value: "run-base" }, "inp-cmp-branch": { value: "run-branch" },
    "btn-rec-replay": replayButton, "btn-rec-compare": compareButton,
    "rec-replay-banner": { innerHTML: "" }, "rec-compare-view": { innerHTML: "" },
  };
  sandbox.document = { getElementById(id) { return inputs[id] || null; } };

  C.store.connectionEpoch = 50;
  C.store.conn = { baseUrl: "http://tenant-a", apiKey: "a" };
  C.store.threads = [{ thread_id: "thread-a", graph: "pipeline" }];
  C.store.selected = "thread-a";
  C.store.recorder = { runId: "run-a" };
  const oldReplay = C.recReplay();
  C.store.connectionEpoch += 1;
  C.store.conn = { baseUrl: "http://tenant-b", apiKey: "b" };
  C.store.threads = [{ thread_id: "thread-b", graph: "pipeline" }];
  C.store.selected = "thread-b";
  C.store.recorder = { runId: "run-b" };
  const newReplay = C.recReplay();
  pending.find((item) => item.url === "http://tenant-a/runs/replay").resolve(response({ verified: true }));
  await oldReplay;
  const oldReplayKeptOwnership = replayButton.disabled;
  pending.find((item) => item.url === "http://tenant-b/runs/replay").resolve(response({ verified: true }));
  await newReplay;
  check("switching: an old replay completion cannot re-enable a newer tenant's recorder action",
    oldReplayKeptOwnership && replayButton.disabled === false);

  C.store.connectionEpoch = 60;
  C.store.conn = { baseUrl: "http://tenant-a", apiKey: "a" };
  C.store.threads = [{ thread_id: "thread-a", graph: "pipeline" }];
  C.store.selected = "thread-a";
  const oldCompare = C.recCompare();
  C.store.connectionEpoch += 1;
  C.store.conn = { baseUrl: "http://tenant-b", apiKey: "b" };
  C.store.threads = [{ thread_id: "thread-b", graph: "pipeline" }];
  C.store.selected = "thread-b";
  const newCompare = C.recCompare();
  pending.find((item) => item.url.startsWith("http://tenant-a/runs/diff")).resolve(response({ added: [], removed: [] }));
  await oldCompare;
  const oldCompareKeptOwnership = compareButton.disabled;
  pending.find((item) => item.url.startsWith("http://tenant-b/runs/diff")).resolve(response({ added: [], removed: [] }));
  let newEventReads = [];
  for (let spin = 0; spin < 10 && newEventReads.length < 2; spin++) {
    await Promise.resolve();
    newEventReads = pending.filter((item) => item.url.startsWith("http://tenant-b/runs/") && item.url.endsWith("/events"));
  }
  newEventReads.forEach((item) => item.resolve(response({ events: [] })));
  await newCompare;
  check("switching: an old comparison completion cannot re-enable a newer tenant's diff action",
    oldCompareKeptOwnership && compareButton.disabled === false);

  const controllerA = {}, controllerB = {};
  C.store.connectionEpoch = 70;
  C.store.conn = { baseUrl: "http://tenant-b", apiKey: "b" };
  const streamOperation = C.connectionOperation("thread-b");
  C.store.streamAbort = controllerB;
  const staleControllerRejected = !C.streamOperationCurrent(controllerA, streamOperation);
  const ownedControllerAccepted = C.streamOperationCurrent(controllerB, streamOperation);
  C.store.connectionEpoch += 1;
  check("switching: a late stream reader failure is owned by both its controller and connection epoch",
    staleControllerRejected && ownedControllerAccepted && !C.streamOperationCurrent(controllerB, streamOperation));

  const streamEvents = [];
  const streamFeed = {
    innerHTML: "", scrollTop: 0, scrollHeight: 0,
    querySelector() { return null; },
    appendChild(node) { streamEvents.push(node); },
  };
  const streamNodes = {
    "sm-values": { checked: false }, "sm-updates": { checked: false }, "sm-messages": { checked: false },
    "sel-multitask": { value: "" }, "feed": streamFeed, "th-runstatus": { innerHTML: "" },
  };
  sandbox.document = {
    getElementById(id) { return streamNodes[id] || null; },
    createElement() { return { className: "", innerHTML: "" }; },
  };
  let resolveOldErrorBody;
  sandbox.fetch = (url) => {
    if (String(url).startsWith("http://tenant-a/")) {
      return Promise.resolve({ ok: false, status: 500, body: null,
        json: () => new Promise((resolve) => { resolveOldErrorBody = resolve; }) });
    }
    return new Promise(() => {});
  };
  C.store.streamAbort = null;
  C.store.connectionEpoch = 80;
  C.store.conn = { baseUrl: "http://tenant-a", apiKey: "a" };
  C.store.threads = [{ thread_id: "thread-a", graph: "pipeline" }];
  C.store.selected = "thread-a";
  const oldErrorStream = C.runStream({ input: {} });
  for (let spin = 0; spin < 10 && !resolveOldErrorBody; spin++) await Promise.resolve();
  C.store.connectionEpoch += 1;
  C.store.conn = { baseUrl: "http://tenant-b", apiKey: "b" };
  C.store.threads = [{ thread_id: "thread-b", graph: "pipeline" }];
  C.store.selected = "thread-b";
  C.runStream({ input: {} });
  streamEvents.length = 0;
  resolveOldErrorBody({ error: "old-tenant", message: "must not cross" });
  await oldErrorStream;
  check("switching: a deferred old-server HTTP error body cannot enter the new tenant's stream feed",
    !streamEvents.some((node) => node.className === "evt error" || node.innerHTML.includes("old-tenant")));
  if (C.store.streamAbort && typeof C.store.streamAbort.abort === "function") C.store.streamAbort.abort();
  C.store.streamAbort = null;

  sandbox.document = originalDocument;
  sandbox.fetch = originalFetch;
}

check("markup: the header exposes one active-connection control and an accessible native dialog",
  html.includes('id="btn-connection-open"') && html.includes('<dialog class="connection-dialog"') &&
  html.includes('aria-labelledby="connection-title"') && html.includes('id="connection-form" novalidate'));
check("markup: durable key storage is an explicit warned opt-in, never the default",
  html.includes('value="session" aria-describedby="connection-session-key-note" checked') && html.includes('value="local"') &&
  html.includes("Stores the key as readable browser data") &&
  html.includes('value="local" aria-describedby="connection-local-key-warning"') &&
  html.includes('id="connection-local-key-warning"'));
check("interaction: Home, profile selection, native submit, reveal, disconnect, and forget are wired",
  html.includes('if (action === "connect") { connectionOpen();') &&
  html.includes('$("connection-form").addEventListener("submit"') &&
  html.includes('$("btn-profile-forget").onclick = connectionForgetSelected') &&
  html.includes('$("btn-disconnect").onclick = connectionDisconnect') &&
  html.includes('$("btn-key-reveal").onclick'));
check("switching: workspace reset cancels poll, stream, task, recorder, comparison, and capability evidence",
  html.includes("store.taskRequest += 1;") && html.includes("stopPoll();") && html.includes("abortStream();") &&
  html.includes("store.recorder = null;") && html.includes("store.compare = null;") && html.includes("store.recUnsupported = false;") &&
  html.includes("if (!tasksRequestCurrent(request, connection)) return false;"));
check("responsive: Connection Hub collapses its profiles, handshake, and form for mobile",
  html.includes(".connection-body { display: block;") &&
  html.includes(".connection-handshake { grid-template-columns: 1fr;") &&
  html.includes(".connection-form { grid-template-columns: 1fr;"));

if (failed) {
  console.error(`\nFAIL: ${failed} failed, ${passed} passed`);
  process.exit(1);
}
console.log(`\nPASS: ${passed} Studio Connection Hub assertions`);
