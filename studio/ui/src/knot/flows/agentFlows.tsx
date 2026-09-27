import { useEffect, useMemo, useState } from "react";
import { type AgentVariable, activateAssistantVersion, assistantVersion, listDatasets, llmProviders, checkSkillFreshness, learnSkill, promoteSkill, skillEvidence, createAssistant, createConnectorInstance, createDataset, createSchedule, createThread, createWebhook, getRun, connectorFromOpenApi, listConnectorInstances, listConnectorManifests, listRuns, serverInfo, registerConnectorManifest, registerSkill, removeSkill, runDraft, skillBody, type Assistant, type AssistantVersion, type ConnectorInstance, type ConnectorManifest, type LlmProviders, type ServerSkill, type ServerTool, type SkillEvidence, type GenericAuth, type OfferedOperation } from "../../engine/net/client";
import { cadenceOf } from "../../engine/forms/agentSpec";
import { composeSkillMd } from "../../engine/forms/skillDraft";
import { useServer } from "../../engine/net/server";
import { Badge, OvHead, openMenu, useOverlay } from "../overlay";
import { COLORS, COLOR_BG, ICONS, ago, riskOf, slug, toolIcon, variablesIn, type Look } from "../data";
import { VarTextarea } from "../agents/VarTextarea";
import { versionNote } from "../agents/versionNote";
import type { AgentDraft } from "../agents/useAgent";
import { renderMarkdown } from "../../engine/text/markdown";
import { McpMountDrawer, PackagesTab } from "./connectorFlows";
import { createRevision, declareEnvironment, listEnvironments, promoteRevision, type DeploymentEnvironment, type DeploymentPointer } from "../../engine/net/client";
import { firstSentence, plainName } from "../agents/words";
import { emptySkillForm, skillFormOf, writeSkillBody, type SkillForm } from "../agents/skillForm";
import { SIGN_IN_WORDS, signInOf, type SignIn } from "../../engine/forms/openapiSignIn";
import { buildConfig, fieldsOf, missingRequired, pathKey, readSpec, type Schema, type SpecNode } from "../../engine/forms/spec";

/* ───────── New agent ───────── */
const TEMPLATES: { icon: string; color: string; name: string; desc: string; instr: string }[] = [
  { icon: "ti-square-plus", color: "blue", name: "Blank agent", desc: "Start from nothing and wire it up yourself.", instr: "" },
  { icon: "ti-headset", color: "teal", name: "Support agent", desc: "Triage tickets, draft replies, escalate when unsure.", instr: "You are the first responder for inbound tickets.\n\nFor every ticket:\n1. Read what is already open before acting.\n2. Answer from what the system holds; never invent.\n3. Keep replies short and say what happens next.\n4. When unsure, say so and hand off." },
  { icon: "ti-chart-pie", color: "plum", name: "Data analyst", desc: "Query systems and answer questions with numbers.", instr: "You are a data analyst. Read the records, count what you read, and answer with numbers you can trace to rows. State the range and filters you applied." },
  { icon: "ti-mail-fast", color: "amber", name: "Outbound SDR", desc: "Research leads and draft personalized outreach.", instr: "You are an SDR. Research the lead, find one relevant hook, and draft a concise first-touch email. Never fabricate facts about the prospect." },
  { icon: "ti-code", color: "rose", name: "Coding agent", desc: "Read repos, reproduce bugs, open pull requests.", instr: "You are a coding agent. Reproduce the reported bug, find the root cause, and propose a minimal fix with a clear description." },
  { icon: "ti-search", color: "blue", name: "Research assistant", desc: "Search sources and synthesize cited briefs.", instr: "You are a research assistant. Read reputable sources, cross-check claims, and produce a concise brief with citations." },
];

export function NewAgentModal({ onCreated }: { onCreated: (a: Assistant) => void }) {
  const { close, toast } = useOverlay();
  const [sel, setSel] = useState(1);
  const [busy, setBusy] = useState(false);
  async function create() {
    const t = TEMPLATES[sel];
    setBusy(true);
    try {
      const name = t.name === "Blank agent" ? "New agent" : t.name;
      const made = await createAssistant({ name, graph: "react_agent", metadata: { description: t.desc, studio: { color: t.color, icon: t.icon } } as never, config: { studio_intent: { instructions: t.instr, tools: [] } } });
      await useServer.getState().refresh();
      close(); toast(`Created "${made.name}" — draft ready`, "ti-robot"); onCreated(made);
    } catch (err) { toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle"); }
    finally { setBusy(false); }
  }
  return (
    <div className="m-modal lg">
      <OvHead icon="ti-robot" bg="var(--cat-teal-bg)" fg="var(--cat-teal)" title="Create a new agent" sub="Start from a template or a blank canvas." />
      <div className="ov-body"><div className="tmpl-grid">
        {TEMPLATES.map((t, i) => (
          <div key={t.name} className={`tmpl-card${i === sel ? " sel" : ""}`} onClick={() => setSel(i)}>
            <div className="tmpl-ic" style={{ background: COLOR_BG[t.color], color: COLORS[t.color] }}><i className={`ti ${t.icon}`} /></div>
            <div className="tmpl-name">{t.name}</div>
            <div className="tmpl-desc">{t.desc}</div>
          </div>
        ))}
      </div></div>
      <div className="ov-foot"><div className="sp" /><button className="m-btn ghost" data-close>Cancel</button><button className="m-btn primary" disabled={busy} onClick={() => void create()}><i className="ti ti-arrow-right" /> Create agent</button></div>
    </div>
  );
}

/* ───────── Identity ───────── */
export function IdentityModal({ draft, look, onSave }: { draft: AgentDraft; look: Look; onSave: (next: Partial<AgentDraft>) => void }) {
  const { close, toast } = useOverlay();
  const [name, setName] = useState(draft.name);
  const [desc, setDesc] = useState(draft.description);
  const [color, setColor] = useState(look.color);
  const [icon, setIcon] = useState(look.icon);
  return (
    <div className="m-modal lg">
      <OvHead icon="ti-id-badge" title="Agent identity" sub="Name, handle, description and how it looks across the workspace." />
      <div className="ov-body">
        <div style={{ display: "flex", gap: 18, alignItems: "flex-start" }}>
          <div className="wz-preview-tile" style={{ background: COLOR_BG[color], color: COLORS[color] }}><i className={`ti ${icon}`} /></div>
          <div style={{ flex: 1 }}>
            <div className="fld"><label className="fld-label">Name</label><input className="m-input" value={name} onChange={(e) => setName(e.target.value)} /></div>
            <div className="fld"><label className="fld-label">Handle</label><div className="m-input-group"><span style={{ color: "var(--ink-400)" }}>@</span><input value={slug(name)} readOnly /></div></div>
          </div>
        </div>
        <div className="fld"><label className="fld-label">Description</label><textarea className="m-textarea" value={desc} onChange={(e) => setDesc(e.target.value)} /></div>
        <div className="fld"><label className="fld-label">Color</label><div className="swatch-row">{Object.keys(COLORS).map((c) => <div key={c} className={`swatch${c === color ? " sel" : ""}`} style={{ background: COLORS[c] }} onClick={() => setColor(c)} />)}</div></div>
        <div className="fld"><label className="fld-label">Icon</label><div className="icon-row">{ICONS.map((i) => <div key={i} className={`icon-pick${i === icon ? " sel" : ""}`} onClick={() => setIcon(i)}><i className={`ti ${i}`} /></div>)}</div></div>
      </div>
      <div className="ov-foot"><div className="sp" /><button className="m-btn ghost" data-close>Cancel</button><button className="m-btn primary" onClick={() => { onSave({ name: name.trim() || draft.name, description: desc.trim(), metadata: { ...draft.metadata, studio: { color, icon } } }); close(); toast("Identity updated", "ti-id-badge"); }}><i className="ti ti-check" /> Save</button></div>
    </div>
  );
}

/* ───────── Model picker ───────── */
/** Add or edit one `{{variable}}` — the design's Add variable modal: name,
 * description, where the value comes from, and a value for the Test panel. */
export function VariableModal({ initial, taken, onSave }: { initial?: Partial<AgentVariable>; taken: string[]; onSave: (v: AgentVariable) => void }) {
  const { close } = useOverlay();
  const [name, setName] = useState(initial?.name ?? "");
  const [desc, setDesc] = useState(initial?.description ?? "");
  const [source, setSource] = useState<AgentVariable["source"]>(initial?.source ?? "trigger");
  const [value, setValue] = useState(initial?.value ?? "");
  const [path, setPath] = useState(initial?.path ?? "");
  const [test, setTest] = useState(initial?.test_value ?? "");
  const clean = name.trim().replace(/[^a-z0-9_.]/gi, "_").toLowerCase();
  const clash = !initial?.name && taken.includes(clean);
  const SOURCES: [AgentVariable["source"], string, string, string][] = [
    ["setting", "ti-settings", "Agent setting", "A fixed value set in this builder"],
    ["trigger", "ti-bolt", "Trigger payload", "Read from the event that started the run"],
  ];
  return (
    <div className="m-modal">
      <OvHead icon="ti-variable" bg="var(--accent-bg)" fg="var(--accent)" title={initial?.name ? "Edit variable" : "Add variable"} sub="Filled in fresh on every run." />
      <div className="ov-body">
        <div className="fld"><label className="fld-label">Name</label><div className="m-input-group"><span style={{ color: "var(--ink-400)", fontFamily: "var(--font-mono)" }}>{"{{"}</span><input value={name} onChange={(e) => setName(e.target.value)} placeholder="customer_tier" autoFocus /><span style={{ color: "var(--ink-400)", fontFamily: "var(--font-mono)" }}>{"}}"}</span></div>{clash && <div className="m-hint" style={{ color: "var(--bad)" }}>{`{{${clean}}}`} already exists.</div>}</div>
        <div className="fld"><label className="fld-label">Description</label><input className="m-input" value={desc} onChange={(e) => setDesc(e.target.value)} placeholder="What this value represents" /></div>
        <div className="fld"><label className="fld-label">Source</label><div className="opt-list">
          {SOURCES.map(([k, ic, n, d]) => <div key={k} className={`trig-opt${source === k ? " sel" : ""}`} data-src={n} onClick={() => setSource(k)}><div className="to-ic"><i className={`ti ${ic}`} /></div><div><div className="to-name">{n}</div><div className="to-desc">{d}</div></div></div>)}
        </div></div>
        {source === "setting" && <div className="fld"><label className="fld-label">Value</label><input className="m-input" value={value} onChange={(e) => setValue(e.target.value)} placeholder="The value every run reads" /></div>}
        {source === "trigger" && <div className="fld"><label className="fld-label">Event field</label><input className="m-input mono" value={path} onChange={(e) => setPath(e.target.value)} placeholder="event.ticket.org" /><div className="m-hint">A dotted path into the webhook's event, from <span className="mono">event</span>.</div></div>}
        <div className="fld"><label className="fld-label">Test value</label><input className="m-input" value={test} onChange={(e) => setTest(e.target.value)} placeholder="Used only in the Test panel" /></div>
      </div>
      <div className="ov-foot"><div className="sp" /><button className="m-btn ghost" onClick={close}>Cancel</button><button className="m-btn primary" disabled={!clean || clash} data-save onClick={() => { onSave({ name: clean, description: desc.trim() || undefined, source, value: source === "setting" ? value : undefined, path: source === "trigger" ? path.trim() || undefined : undefined, test_value: test.trim() || undefined }); close(); }}>{initial?.name ? "Save" : "Add variable"}</button></div>
    </div>
  );
}

export function ModelPickerModal({ slot, providers, current, own, onPick }: { slot: "primary" | "fallback"; providers: LlmProviders; current: string | null; /** Whether the agent names its own provider; false when it follows the deployment. */ own: boolean; onPick: (id: string | null) => void }) {
  const { close } = useOverlay();
  const deployment = slot === "fallback" ? providers.fallback : providers.primary;
  const deploymentProvider = providers.providers.find((p) => p.id === deployment);
  return (
    <div className="m-modal">
      <OvHead icon="ti-cpu" bg="var(--brand-soft)" fg="var(--ink-800)" title={slot === "fallback" ? "Fallback model" : "Choose a model"} sub={slot === "fallback" ? "Used when the primary is rate-limited or down." : "The reasoning engine for this agent."} />
      <div className="ov-body" style={{ paddingTop: 8 }}>
        <div className={`mdl-row${!own ? " sel" : ""}`} onClick={() => { onPick(null); close(); }} data-flow="model-default">
          <div className="mdl-logo" style={{ background: "var(--ink-200)", color: "var(--ink-700)" }}><i className="ti ti-building" /></div>
          <div className="mdl-body"><div className="mdl-name">The workspace default</div><div className="mdl-meta">{deploymentProvider ? `${deploymentProvider.name} · ${deploymentProvider.model.split("/").pop()} — changes when the workspace changes it` : slot === "fallback" ? "the workspace has none set to step in" : "the workspace has none set to answer first"}</div></div>
          {!own && <i className="ti ti-check mdl-check" />}
        </div>
        <div className="cat-label"><span>Configured providers</span><span className="ln" /></div>
        {providers.providers.map((p) => (
          <div key={p.id} className={`mdl-row${own && p.id === current ? " sel" : ""}`} onClick={() => { onPick(p.id); close(); }}>
            <div className="mdl-logo" style={{ background: "var(--accent)" }}><i className="ti ti-sparkles" /></div>
            <div className="mdl-body"><div className="mdl-name" title={p.model}>{p.name}</div><div className="mdl-meta">{p.model.split("/").pop()}{p.id === providers.primary ? " · the workspace default" : ""}{p.has_key ? "" : " · no key"}</div></div>
            <div className="mdl-cost">{p.price_input_per_m != null ? `$${p.price_input_per_m} / 1M` : "—"}</div>
            {own && p.id === current && <i className="ti ti-check mdl-check" />}
          </div>
        ))}
        {providers.providers.length === 0 && <div className="thread-empty">No provider configured — add one under AI models.</div>}
      </div>
    </div>
  );
}

/* ───────── Versions ───────── */
export function VersionsDrawer({ agent, versions, onRestore }: { agent: Assistant; versions: AssistantVersion[]; onRestore: (v: AssistantVersion) => void }) {
  const { close, toast } = useOverlay();
  const sorted = [...versions].sort((x, y) => y.created_at.localeCompare(x.created_at));
  // The listing carries no config: each version is read in full so its row
  // can say what it changed against its parent, and who saved it.
  const [full, setFull] = useState<Record<string, AssistantVersion>>({});
  useEffect(() => {
    let alive = true;
    Promise.all(versions.map((v) => assistantVersion(agent.assistant_id, v.version_id).then((r) => [v.version_id, r] as const).catch(() => null)))
      .then((rows) => { if (alive) setFull(Object.fromEntries(rows.filter((r): r is readonly [string, AssistantVersion] => !!r))); });
    return () => { alive = false; };
  }, [agent.assistant_id, versions]);
  const [providers, setProviders] = useState<LlmProviders | null>(null);
  useEffect(() => { llmProviders().then(setProviders).catch(() => {}); }, []);
  const modelName = (id: string) => providers?.providers.find((p) => p.id === id)?.name ?? id;
  // Each row says what changed since the save before it, in time — a changelog.
  const noteOf = (v: AssistantVersion, i: number) => { const me = full[v.version_id]; if (!me) return null; const before = sorted[i + 1]; const parent = before ? full[before.version_id] ?? null : null; if (before && !parent) return null; return versionNote(me, parent, modelName); };
  return (
    <div className="m-drawer">
      <OvHead icon="ti-history" bg="var(--brand-soft)" fg="var(--ink-800)" title="Version history" sub="Every save is a version; Restore brings one back as the working copy." />
      <div className="ov-body">
        {sorted.map((v, i) => (
          <div key={v.version_id} className="ver-row">
            <div className="ver-rail"><div className={`ver-dot${v.active ? " cur" : ""}`} />{i < sorted.length - 1 && <div className="ver-line" />}</div>
            <div className="ver-body">
              <div className="ver-top"><span className="ver-tag">v{sorted.length - i}</span>{v.active ? <span className="m-badge accent sm">Current</span> : <span className="ver-restore" onClick={() => { const me = full[v.version_id]; if (!me) { toast("Still reading that version…", "ti-history"); return; } onRestore(me); toast(`v${sorted.length - i} restored to the working copy — Publish makes it run`, "ti-history"); close(); }}>Restore</span>}</div>
              <div className="ver-msg">{noteOf(v, i) ?? "Reading…"}{v.declined ? ` · declined: ${v.declined.reason}` : ""}</div>
              <div className="ver-meta"><i className="ti ti-user" style={{ fontSize: 12 }} /> {((full[v.version_id] ?? v).metadata as { created_by?: { name?: string }; proposed_by?: { name?: string } } | undefined)?.created_by?.name ?? ((full[v.version_id] ?? v).metadata as { proposed_by?: { name?: string } } | undefined)?.proposed_by?.name ?? "—"} <span>·</span> {ago(v.created_at)}</div>
            </div>
          </div>
        ))}
        {sorted.length === 0 && <div className="thread-empty"><i className="ti ti-history" />No version yet.</div>}
      </div>
    </div>
  );
}

/* ───────── Publish ───────── */
export function PublishModal({ agent, draft, status, onPublish }: { agent: Assistant; draft: AgentDraft; status: "draft" | "published"; onPublish: (reason?: string, note?: string) => Promise<boolean> }) {
  const { toast } = useOverlay();
  const me = useServer((s) => s.me);
  const [done, setDone] = useState(false);
  const [busy, setBusy] = useState(false);
  const [reason, setReason] = useState("");
  const [note, setNote] = useState("");
  const [needsReason, setNeedsReason] = useState(false);
  // The Evals page promises that every publish is gated on the agent's suites.
  // An agent with no suite has nothing to gate it, so it published freely and
  // silently; now it needs the same written reason the held gate asks for.
  const [noSuite, setNoSuite] = useState(false);
  useEffect(() => { listDatasets().then((d) => { const none = !d.some((x) => x.agent_id === agent.assistant_id); setNoSuite(none); if (none) setNeedsReason(true); }).catch(() => {}); }, [agent.assistant_id]);
  const [envs, setEnvs] = useState<DeploymentEnvironment[] | null>(null);
  const [target, setTarget] = useState<string>("");
  const [newEnv, setNewEnv] = useState("");
  const [pointer, setPointer] = useState<DeploymentPointer | null>(null);
  const [deployNote, setDeployNote] = useState<string | null>(null);
  useEffect(() => { listEnvironments().then((e) => { setEnvs(e); }).catch(() => setEnvs([])); }, []);
  const author = { type: "human" as const, human_id: me?.principal.id ?? "studio" };
  const tools = (draft.intent.tools ?? []).length;
  const checks: [string, string, string, string?][] = [
    [draft.intent.instructions && draft.intent.instructions.trim().length > 40 ? "ok" : "miss", "Instructions defined", `~${Math.round((draft.intent.instructions ?? "").length / 4)} tokens`],
    [tools ? "ok" : "miss", "Tools configured", `${tools} tool${tools === 1 ? "" : "s"}`],
    ["ok", "Guardrails active", "approval gate · verifier · sealed secrets"],
    [status === "draft" ? "miss" : "ok", "Newest version live", status === "draft" ? "This publish activates it" : "Already running"],
    [target ? "ok" : "miss", "Deployment target", target ? `Promote a revision into ${target}` : "Optional: activation alone serves the API, schedules and the queue", target ? undefined : "Set below"],
  ];
  /** Activate the newest version; then, when an environment is chosen, freeze a revision binding this agent and promote it there. */
  async function publish() {
    setBusy(true);
    try {
      const ok = await onPublish(needsReason ? reason : undefined, note.trim() || undefined);
      if (!ok) { setNeedsReason(true); return; }
      if (target) {
        try {
          const rev = await createRevision({ graph: agent.graph, assistant: agent.assistant_id, source_environment: target, author });
          const r = await promoteRevision(target, { revision_id: rev.revision.revision_id, author });
          setPointer(r.pointer); setDeployNote(r.applied ? `Revision ${rev.revision.revision_id.slice(0, 12)} now serves ${target}.` : `${target} already served that revision.`);
        } catch (err) { setDeployNote(`Activated, but ${target} did not take it: ${err instanceof Error ? err.message : "the server refused"}`); }
      }
      setDone(true);
    } finally { setBusy(false); }
  }
  async function declare() {
    const name = newEnv.trim(); if (!name) return;
    try { const r = await declareEnvironment({ name, author }); setEnvs((e) => [...(e ?? []), r.environment]); setTarget(name); setNewEnv(""); toast(`${name} declared`, "ti-world"); }
    catch (err) { toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle"); }
  }
  return (
    <div className="m-modal lg">
      <OvHead icon="ti-rocket" bg="var(--accent-bg)" fg="var(--accent)" title={`Publish ${draft.name}`} sub="Review the pre-flight checklist, then choose a target." />
      {!done ? (
        <>
          <div className="ov-body" data-stage="review">
            {checks.map((c) => <div key={c[1]} className="chk"><div className={`chk-mark ${c[0]}`}><i className={`ti ${c[0] === "ok" ? "ti-check" : "ti-alert-triangle"}`} /></div><div className="chk-body"><div className="chk-name">{c[1]}</div><div className="chk-desc">{c[2]}</div></div>{c[3] && <span className="chk-act">{c[3]}</span>}</div>)}
            <div className="fld" style={{ marginTop: 18 }}><label className="fld-label">Deployment target</label>
              <div className={`deploy-opt${target === "" ? " sel" : ""}`} data-dep="none" onClick={() => setTarget("")}><div className="do-ic" style={{ background: "var(--bg-muted)", color: "var(--ink-700)" }}><i className="ti ti-api" /></div><div style={{ flex: 1 }}><div className="chk-name">Activate only</div><div className="chk-desc">The newest version serves the API, schedules, webhooks and the queue</div></div><div className="m-box radio" style={target === "" ? { borderColor: "var(--accent)" } : undefined} /></div>
              {(envs ?? []).map((e) => <div key={e.name} className={`deploy-opt${target === e.name ? " sel" : ""}`} data-dep={e.name} onClick={() => setTarget(e.name)}><div className="do-ic" style={{ background: /prod/i.test(e.name) ? "var(--good-bg)" : "var(--warn-bg)", color: /prod/i.test(e.name) ? "var(--good)" : "var(--warn)" }}><i className={`ti ${/prod/i.test(e.name) ? "ti-world" : "ti-test-pipe"}`} /></div><div style={{ flex: 1 }}><div className="chk-name">{e.name}</div><div className="chk-desc">{e.gate ? `gated by ${e.gate.policy} on ${e.gate.dataset_version}` : "no gate"}{e.approval_required ? " · approval required" : ""}</div></div><div className="m-box radio" style={target === e.name ? { borderColor: "var(--accent)" } : undefined} /></div>)}
              {envs && envs.length === 0 && <div style={{ display: "flex", gap: 8, marginTop: 8 }}><input className="m-input" placeholder="Declare an environment: staging, prod…" value={newEnv} onChange={(e) => setNewEnv(e.target.value)} /><button className="m-btn secondary sm" disabled={!newEnv.trim()} onClick={() => void declare()}><i className="ti ti-plus" /> Declare</button></div>}
            </div>
            <div className="fld" style={{ marginTop: 14 }}><label className="fld-label">Note for this version <span className="opt">— optional; kept on the version, read in Versions</span></label><input className="m-input" data-flow="version-note" placeholder="What changed and why, in your words" value={note} onChange={(e) => setNote(e.target.value)} /></div>
            {needsReason && <div className="fld" style={{ marginTop: 14 }}><label className="fld-label">{noSuite ? "This agent has no evaluation suite — a reason to publish without one" : "The gate held this version — a reason to publish anyway"}</label><input className="m-input" value={reason} onChange={(e) => setReason(e.target.value)} placeholder="Why it should run before its suite passes" /></div>}
          </div>
          <div className="ov-foot"><div className="sp" /><button className="m-btn ghost" data-close>Cancel</button><button className="m-btn primary" disabled={busy || (needsReason && !reason.trim())} onClick={() => void publish()}><i className="ti ti-rocket" /> {target ? `Publish to ${target}` : "Publish"}</button></div>
        </>
      ) : (
        <>
          <div className="ov-body"><div className="pub-done"><div className="burst"><i className="ti ti-check" /></div><div className="pd-t">Published</div><div className="pd-c">{draft.name} runs the newest version now.{deployNote ? ` ${deployNote}` : " Schedules, webhooks and the queue route to it."}</div><div className="pub-url"><i className="ti ti-link" style={{ color: "var(--ink-500)" }} /> {pointer ? `${pointer.surface} → ${pointer.active?.slice(0, 12) ?? "—"}` : `/assistants/${agent.assistant_id.slice(0, 8)}`}</div></div></div>
          <div className="ov-foot"><div className="sp" /><button className="m-btn primary" data-close>Done</button></div>
        </>
      )}
    </div>
  );
}

/* ───────── Preview: the agent as a person meets it ───────── */
export function PreviewModal({ agent, draft }: { agent: Assistant; draft: AgentDraft }) {
  const [thread, setThread] = useState<string | null>(null);
  const [msgs, setMsgs] = useState<{ who: "user" | "agent"; text: string }[]>([{ who: "agent", text: `Hi! I’m ${draft.name}. ${draft.description.split(".")[0] || "How can I help?"}` }]);
  const [input, setInput] = useState("");
  const [busy, setBusy] = useState(false);
  async function send() {
    const t = input.trim(); if (!t || busy) return;
    setInput(""); setBusy(true); setMsgs((m) => [...m, { who: "user", text: t }]);
    try {
      const th = thread ?? (await createThread(agent.graph)).thread_id; if (!thread) setThread(th);
      // The draft runs as a bare graph here, so its model and every variable's
      // value (test value, else the setting) ride along with the charter.
      const variables = Object.fromEntries((draft.intent.variables ?? []).flatMap((v) => { const value = v.test_value?.trim() || v.value?.trim(); return value ? [[v.name, value]] : []; }));
      const r = await runDraft(th, { instructions: draft.intent.instructions ?? "", tools: draft.intent.tools ?? [], skills: draft.intent.skills ?? [], model: draft.intent.model ?? null, fallback_model: draft.intent.fallback_model ?? null, variables }, [{ role: "user", content: t }], !thread);
      const reply = [...(r.output?.messages ?? [])].reverse().find((m) => m.role === "assistant" && !(m.tool_calls?.length) && m.content)?.content ?? (r.status === "interrupted" ? "(paused for a decision)" : "(no reply)");
      setMsgs((m) => [...m, { who: "agent", text: reply }]);
    } catch (err) { setMsgs((m) => [...m, { who: "agent", text: err instanceof Error ? err.message : "the run could not be started" }]); }
    finally { setBusy(false); }
  }
  return (
    <div className="m-modal xl">
      <OvHead icon="ti-eye" title={`Preview — ${draft.name}`} sub="What an end user sees. Runs against the current draft." />
      <div className="ov-body"><div className="pv-wrap">
        <div className="pv-widget">
          <div className="pv-head"><div className="ag-tile-sm" style={{ background: "var(--bg-tint)", color: "#fff" }}><i className="ti ti-robot" /></div><div><div className="n">{draft.name}</div><div className="s"><span style={{ width: 6, height: 6, borderRadius: "50%", background: "var(--good)", display: "inline-block" }} /> Online</div></div></div>
          <div className="pv-thread">{msgs.map((m, i) => <div key={i} className={`msg ${m.who}`}><div className="bub">{m.who === "agent" ? <div className="md">{renderMarkdown(m.text)}</div> : m.text}</div></div>)}{busy && <div className="msg agent"><div className="thinking"><span /><span /><span /></div></div>}</div>
          <div className="pv-foot"><div className="composer"><textarea rows={1} placeholder="Type a message…" value={input} onChange={(e) => setInput(e.target.value)} onKeyDown={(e) => { if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); void send(); } }} /><button className="composer-send" onClick={() => void send()}><i className="ti ti-arrow-up" /></button></div></div>
        </div>
        <div className="pv-side">
          <div className="fld"><label className="fld-label">Run it by API</label><div className="pv-code">{`POST /threads/{thread}/runs/wait\n{"assistant_id": "${agent.assistant_id}",\n "input": {"messages": [{"role":"user","content":"…"}]}}`}</div></div>
          <div className="fld"><label className="fld-label">Channels</label>{[["ti-clock-hour-4", "Schedule"], ["ti-webhook", "Webhook"], ["ti-list-check", "Task queue"], ["ti-api", "REST API"]].map((c) => <div key={c[1]} className="scope-row"><i className={`ti ${c[0]}`} style={{ color: "var(--ink-600)" }} /><div className="sb"><div style={{ fontSize: "var(--fs-sm)", fontWeight: 500 }}>{c[1]}</div></div></div>)}</div>
        </div>
      </div></div>
      <div className="ov-foot"><div className="sp" /><button className="m-btn secondary" data-close>Close</button></div>
    </div>
  );
}

/* ───────── Skill editor drawer (760px): preview / source, live tool detection ───────── */
/** Tools a procedure names: in backticks, or bare `connector.operation` names in the prose. */
const toolsIn = (src: string, known?: (name: string) => boolean) => {
  const quoted = [...src.matchAll(/`([a-z][a-z0-9_.@-]{2,})`/g)].map((m) => m[1]);
  // A bare `connector.operation` in prose counts only when the platform knows
  // it — otherwise a hostname (en.wikipedia.org) reads as a tool.
  const bare = [...src.matchAll(/(?<![\w`/])([a-z0-9][a-z0-9-]*(?:@[a-z0-9]+)?\.[a-z][a-z0-9_-]+)(?![\w`])/g)]
    .map((m) => m[1])
    .filter((n) => (known ? known(n) : false));
  return [...new Set([...quoted, ...bare])];
};

/** `tools`: the agent's tools as the builder has them now — the working copy, not the published version. */
export function SkillEditorDrawer({ agent, tools, skill, isNew, onSaved, onRemove }: { agent?: Assistant; tools?: string[]; skill?: ServerSkill; isNew?: boolean; onSaved: (name: string) => void; onRemove?: () => void }) {
  const { open, close, toast } = useOverlay();
  // A builder fills in a form; the document stays one click away for a skill written as one.
  const [mode, setMode] = useState<"form" | "preview" | "source">(isNew ? "form" : "preview");
  const [name, setName] = useState(skill?.name ?? "");
  const [desc, setDesc] = useState(skill?.description ?? "");
  // A new skill starts from the agent's own tools, already linked: the builder writes the steps, not the markup.
  const ownTools = tools ?? (agent?.config?.studio_intent?.tools ?? []).map((t) => t.name);
  const [form, setForm] = useState<SkillForm | null>(isNew ? emptySkillForm(ownTools) : null);
  const [body, setBody] = useState("");
  const [dirty, setDirty] = useState(false);
  // Taking the skill out of the library asks once, in place; a refusal is said there too.
  const [removing, setRemoving] = useState<{ refused?: string } | null>(null);
  // Every tool on the platform, with what it does: the library form finds a tool by either.
  const [catalog, setCatalog] = useState<Map<string, string>>(new Map());
  const agentTools = new Set(ownTools);
  useEffect(() => {
    if (skill && !isNew) skillBody(skill.name).then((b) => { setBody(b.body); const f = skillFormOf(b.body); if (f) { setForm(f); setMode("form"); } }).catch(() => setBody("Could not read the procedure."));
    serverInfo().then((i) => setCatalog(new Map(i.graphs.flatMap((g) => g.tools.map((t) => [t.name, t.description ?? ""] as [string, string]))))).catch(() => {});
  }, [skill?.name, isNew]);
  const text = mode === "form" && form ? writeSkillBody(form) : body;
  const refs = [...new Set([...(skill?.allowed_tools ?? []), ...toolsIn(text, (n) => catalog.has(n) || agentTools.has(n))])];
  const status = (t: string): [string, string] => (agentTools.has(t) ? ["good", "On this agent"] : catalog.has(t) ? ["info", "Available"] : /^[a-z0-9-]+\./.test(t) ? ["warn", "Not connected yet"] : ["bad", "Unknown"]);
  // What a person needs to see: the tools the skill really uses. A backticked word that is no tool is not listed.
  const shownRefs = refs.filter((t) => status(t)[0] !== "bad");
  async function save() {
    if (!slug(name)) { toast("Give the skill a name first", "ti-alert-triangle"); return; }
    if (!desc.trim()) { toast("Say when to use it — the agent reads that to decide", "ti-alert-triangle"); return; }
    if (mode === "form" && form && !form.steps.some((x) => x.trim())) { toast("Add at least one step", "ti-alert-triangle"); return; }
    try {
      const md = composeSkillMd({ name: slug(name), description: desc.trim(), license: skill?.license ?? "", evalGate: skill?.eval_gate ?? "", tools: refs.filter((t) => catalog.has(t)), body: text });
      const r = await registerSkill(md);
      toast(isNew ? `Skill "${r.name}" created` : `Skill saved as v${r.revision}`, "ti-puzzle");
      onSaved(r.name); close();
    } catch (err) { toast(err instanceof Error ? err.message : "the server refused the skill", "ti-alert-triangle"); }
  }
  const wrap = (w: string) => setBody((b) => `${b}${w}${w}`);
  const edited = (f: SkillForm) => { setForm(f); setDirty(true); };
  function show(next: "form" | "preview" | "source") {
    if (next === "form") {
      const f = skillFormOf(body);
      if (!f) { toast("This skill is written as a document with parts the form has no place for — edit it as a document", "ti-file-text"); return; }
      setForm(f);
    } else if (mode === "form" && form) setBody(writeSkillBody(form));
    setMode(next);
  }
  return (
    <div className="m-drawer sk-drawer">
      <div className="sk-top">
        <button className="ov-close" data-close title="Close"><i className="ti ti-chevrons-right" /></button>
        <div className="sk-top-title"><span className="sk-kicker">{isNew ? "New skill" : "Skill"}</span><span className="sk-slug" title={slug(name)} style={{ fontFamily: "inherit" }}>{name ? plainName(slug(name)) : "—"}</span></div>
        <div className="sp" />
        {!isNew && onRemove && <button className="m-btn ghost sm icon" title="Remove from agent" onClick={() => { onRemove(); close(); toast(`Removed ${skill?.name}`, "ti-trash"); }}><i className="ti ti-trash" /></button>}
        {dirty && <span className="save-state"><span className="d" style={{ background: "var(--warn-dot)" }} /> Unsaved</span>}
        {!isNew && skill && <button className="m-btn ghost sm icon" title="More" onClick={(e) => openMenu(e.currentTarget, [
          { label: "Learn from the system", icon: "ti-book-2", run: async () => { try { const r = await learnSkill(skill.name); toast(`Learned ${r.reference} · ${r.rows} rows from ${r.reads} reads · revision ${r.revision}`, "ti-book-2"); onSaved(skill.name); } catch (err) { toast(err instanceof Error ? err.message : "the skill has no reads to run", "ti-alert-triangle"); } } },
          { label: "Check freshness", icon: "ti-refresh", run: async () => { try { const r = await checkSkillFreshness(skill.name); toast(!r.learned ? "Nothing learned yet" : r.freshness.stale ? `Stale: ${r.freshness.because.join("; ")}` : "Current — the system has not moved", r.freshness?.stale ? "ti-alert-triangle" : "ti-circle-check"); } catch (err) { toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle"); } } },
          "-",
          { label: "Evidence & promotion", icon: "ti-shield-check", run: () => open("drawer", <SkillEvidenceDrawer name={skill.name} />) },
          "-",
          { label: "Remove from library", icon: "ti-trash", danger: true, run: () => setRemoving({}) },
        ])}><i className="ti ti-dots" /></button>}
        <button className="m-btn primary sm" onClick={() => void save()}><i className="ti ti-check" /> {isNew ? "Create skill" : "Save changes"}</button>
      </div>
      <div className="sk-bar">
        <div className="sk-fmt" style={{ visibility: mode === "source" ? "visible" : "hidden" }}>
          <button title="Bold" onClick={() => wrap("**")}><i className="ti ti-bold" /></button><button title="Italic" onClick={() => wrap("*")}><i className="ti ti-italic" /></button><button title="Strikethrough" onClick={() => wrap("~~")}><i className="ti ti-strikethrough" /></button>
          <span className="sep" /><button title="Heading" onClick={() => setBody((b) => `${b}\n## `)}><i className="ti ti-h-2" /></button><button title="Sub-heading" onClick={() => setBody((b) => `${b}\n### `)}><i className="ti ti-h-3" /></button>
          <span className="sep" /><button title="Bullet list" onClick={() => setBody((b) => `${b}\n- `)}><i className="ti ti-list" /></button><button title="Numbered list" onClick={() => setBody((b) => `${b}\n1. `)}><i className="ti ti-list-numbers" /></button>
          <span className="sep" /><button title="Inline tool" onClick={() => wrap("`")}><i className="ti ti-code" /></button><button title="Code block" onClick={() => setBody((b) => `${b}\n\`\`\`\n\n\`\`\`\n`)}><i className="ti ti-source-code" /></button>
        </div>
        <div className="sp" />
        <div className="m-seg sk-mode"><button className={mode === "form" ? "on" : ""} onClick={() => show("form")} data-sk-mode="form"><i className="ti ti-forms" /> Form</button><button className={mode === "preview" ? "on" : ""} onClick={() => show("preview")} data-sk-mode="preview"><i className="ti ti-file-text" /> Document</button><button className={mode === "source" ? "on" : ""} onClick={() => show("source")} data-sk-mode="source"><i className="ti ti-code" /> Markdown</button></div>
      </div>
      {removing && skill && (
        <div className="m-alert" data-sk-remove style={{ margin: "10px 20px 0" }}>
          <i className="ti ti-trash" style={{ color: "var(--bad)", fontSize: 16 }} />
          <div className="a-body">
            <div className="a-title">{removing.refused ? "It stays in the library" : `Remove ${plainName(skill.name)} from the library?`}</div>
            <div className="a-text">{removing.refused ?? "No agent can add it after this. Runs that used it keep their record."}</div>
          </div>
          {!removing.refused && <button className="m-btn danger sm" onClick={async () => { try { await removeSkill(skill.name); toast(`Removed ${plainName(skill.name)} from the library`, "ti-trash"); onSaved(skill.name); close(); } catch (err) { setRemoving({ refused: err instanceof Error ? err.message : "the server refused" }); } }}>Remove</button>}
          <button className="m-btn ghost sm" onClick={() => setRemoving(null)}>{removing.refused ? "OK" : "Keep it"}</button>
        </div>
      )}
      <div className="ov-body sk-body">
        <div className="m-field"><label className="m-label">Name</label><input className="m-input" value={isNew ? name : plainName(name)} title={isNew ? undefined : name} onChange={(e) => { setName(e.target.value); setDirty(true); }} placeholder="e.g. Weekly notice" readOnly={!isNew} /></div>
        <div className="m-field" style={{ marginTop: 12 }}><label className="m-label">When to use it <span className="opt">— the agent reads this to decide</span></label><textarea className="m-textarea" rows={3} value={desc} onChange={(e) => { setDesc(e.target.value); setDirty(true); }} placeholder='Use when the user says "…", asks for …, or when …' /></div>
        {mode === "form" && form && <SkillFormFields form={form} onChange={edited} choices={agent || tools ? ownTools : [...catalog.keys()].sort()} describe={(t) => catalog.get(t) ?? ""} forAgent={!!(agent || tools)} status={status} />}
        {mode !== "form" && <div className="sk-tools">
          {shownRefs.length > 0 && (
            <>
              <div className="f-mini-label" style={{ marginBottom: 6 }}>Tools it uses <span className="caption" style={{ fontWeight: 400 }}>{shownRefs.length}</span></div>
              <div className="sk-tool-chips">{shownRefs.map((t) => { const [k, l] = status(t); return <span key={t} className="sk-tool" data-k={k} title={t}><span className="dot" /><span>{plainName(t)}</span><span className="st">{l}</span></span>; })}</div>
            </>
          )}
        </div>}
        {mode !== "form" && <div className="sk-doc" onDoubleClick={() => mode === "preview" && setMode("source")}>
          {mode === "preview"
            ? <div className="md">{renderMarkdown(body, { code: (t) => (/^[a-z][a-z0-9_.@-]{2,}$/.test(t) && (catalog.has(t) || agentTools.has(t) || /^[a-z0-9-]+\./.test(t)) ? <span className="sk-tool" data-k={status(t)[0]} title={status(t)[1]}><span className="dot" /><span>{plainName(t)}</span></span> : null) })}</div>
            : <VarTextarea className="sk-src" spellCheck={false} value={body} names={[...variablesIn(body), ...(agent ? variablesIn(agent.config?.studio_intent?.instructions ?? "") : [])]} onValueChange={(v) => { setBody(v); setDirty(true); }} style={{ minHeight: Math.max(320, body.split("\n").length * 22) }} />}
        </div>}
      </div>
    </div>
  );
}

/** The skill as a builder writes it: plain fields, the agent's tools picked rather than typed. */
/** `choices`: the agent's tools on an agent page; the whole catalog in the library, found by name. */
function SkillFormFields({ form, onChange, choices, describe, forAgent, status }: { form: SkillForm; onChange: (f: SkillForm) => void; choices: string[]; describe: (t: string) => string; forAgent: boolean; status: (t: string) => [string, string] }) {
  const [q, setQ] = useState("");
  const set = (patch: Partial<SkillForm>) => onChange({ ...form, ...patch });
  const picked = new Map(form.tools.map((t) => [t.name, t]));
  const all = [...new Set([...form.tools.map((t) => t.name), ...choices])];
  const long = all.length > 8;
  const matches = q ? all.filter((t) => `${t} ${plainName(t)} ${describe(t)}`.toLowerCase().includes(q.toLowerCase())) : all;
  const candidates = all.filter((t) => picked.has(t) || matches.includes(t)).slice(0, long && !q ? Math.max(8, form.tools.length) : undefined);
  const toggle = (name: string) => set({ tools: picked.has(name) ? form.tools.filter((t) => t.name !== name) : [...form.tools, { name, when: "" }] });
  const list = (key: "steps" | "never", items: string[], placeholder: (n: number) => string, add: string, numbered: boolean) => (
    <div className="skf-list">
      {items.map((v, n) => (
        <div key={n} className="skf-item">
          <span className="skf-mark">{numbered ? `${n + 1}.` : "•"}</span>
          <input className="m-input" value={v} placeholder={placeholder(n)} onChange={(e) => set({ [key]: items.map((x, i) => (i === n ? e.target.value : x)) })} onKeyDown={(e) => { if (e.key === "Enter") { e.preventDefault(); const list = e.currentTarget.closest(".skf-list"); set({ [key]: [...items.slice(0, n + 1), "", ...items.slice(n + 1)] }); setTimeout(() => list?.querySelectorAll<HTMLInputElement>("input")[n + 1]?.focus(), 0); } }} />
          {(items.length > 1 || !numbered) && <button className="m-btn ghost sm icon" title="Remove" onClick={() => set({ [key]: items.filter((_, i) => i !== n) })}><i className="ti ti-x" /></button>}
        </div>
      ))}
      <button className="skf-add" onClick={(e) => { const list = e.currentTarget.closest(".skf-list"); set({ [key]: [...items, ""] }); setTimeout(() => list?.querySelectorAll<HTMLInputElement>("input")[items.length]?.focus(), 0); }}><i className="ti ti-plus" /> {add}</button>
    </div>
  );
  return (
    <div className="skf" data-skill-form>
      <div className="m-field"><label className="m-label">What it produces</label><textarea className="m-textarea" rows={2} value={form.purpose} onChange={(e) => set({ purpose: e.target.value })} placeholder="e.g. A short notice on the board, in the words the person asked for." /></div>
      <div className="m-field"><label className="m-label">Tools it uses</label>
        {all.length === 0 && <div className="skf-empty">{forAgent ? "This agent has no tools yet — add one under Tools, then pick it here." : "Reading the tools…"}</div>}
        {long && <input className="m-input sm" style={{ marginBottom: 6 }} value={q} onChange={(e) => setQ(e.target.value)} placeholder={`Find a tool — ${all.length} to choose from`} />}
        {q && matches.length === 0 && <div className="skf-empty">No tool does “{q}” yet.</div>}
        <div className="skf-tools">{candidates.map((t) => { const on = picked.get(t); const [k, l] = status(t); return (
          <div key={t} className={`skf-tool${on ? " on" : ""}`}>
            <label className="skf-tool-h"><input type="checkbox" checked={!!on} onChange={() => toggle(t)} /><span title={describe(t) || t}>{plainName(t)}</span>{(k === "warn" || k === "bad") && <span className="sk-tool" data-k={k}><span className="dot" /><span className="st">{l}</span></span>}</label>
            {on && <input className="m-input sm" value={on.when} placeholder="When it uses it (optional)" onChange={(e) => set({ tools: form.tools.map((x) => (x.name === t ? { ...x, when: e.target.value } : x)) })} />}
          </div>); })}</div>
      </div>
      <div className="m-field"><label className="m-label">Steps</label>{list("steps", form.steps, (n) => (n === 0 ? "What to do first" : "What to do next"), "Add a step", true)}</div>
      <div className="m-field"><label className="m-label">Never <span className="opt">— optional</span></label>{list("never", form.never, () => "Something the agent must never do", "Add a rule", false)}</div>
      <div className="m-field"><label className="m-label">When done</label><input className="m-input" value={form.done} onChange={(e) => set({ done: e.target.value })} placeholder="What the agent gives back, e.g. says what was posted" /></div>
    </div>
  );
}

/* ───────── Add tools drawer (520px): catalog | from connectors ───────── */
/** Attach skills that already exist on the platform — a plugin's, a colleague's, the agent's own from
 * an earlier version. Each skill names the tools it relies on: the ones the platform has are added to
 * the agent with it; the ones that need a connection that is not there are named, so the builder
 * knows what the skill will file as a gap until that connector is connected. */
export function SkillPickerDrawer({ skills, catalog, have, onAdd }: { skills: ServerSkill[]; catalog: ServerTool[]; have: string[]; onAdd: (skill: ServerSkill, tools: string[]) => void }) {
  const { toast } = useOverlay();
  const [q, setQ] = useState("");
  const [added, setAdded] = useState<Set<string>>(new Set(have));
  const known = new Set(catalog.map((t) => t.name));
  const visible = skills.filter((s) => !q || `${s.name} ${plainName(s.name)} ${s.description}`.toLowerCase().includes(q.toLowerCase()));
  return (
    <div className="m-drawer">
      <OvHead icon="ti-puzzle" bg="var(--cat-plum-bg)" fg="var(--cat-plum)" title="Add skills from the library" sub="A skill brings the tools it relies on; the ones that need a connection you have not made are named." />
      <div className="ov-body" style={{ paddingTop: 14 }}>
        <div className="ov-search"><i className="ti ti-search" /><input placeholder="Search skills…" value={q} onChange={(e) => setQ(e.target.value)} /></div>
        {visible.length === 0 && <div className="m-hint">No skill matches.</div>}
        {visible.map((s) => {
          const tools = s.allowed_tools ?? [];
          const present = tools.filter((t) => known.has(t));
          const missing = tools.filter((t) => !known.has(t));
          const connectors = [...new Set(missing.filter((t) => t.includes(".")).map((t) => t.split(".")[0]))];
          return (
            <div key={s.name} className="item">
              <div className="item-ic" style={{ background: "var(--cat-plum-bg)", color: "var(--cat-plum)" }}><i className="ti ti-route" /></div>
              <div className="item-body">
                <div className="item-name" title={s.name}>{plainName(s.name)}</div>
                <div className="item-desc">{s.description}</div>
                {tools.length > 0 && <div className="mono-meta" style={{ marginTop: 4 }}>{present.length} of {tools.length} tools ready{connectors.length ? ` · needs ${connectors.map(plainName).join(", ")} connected` : ""}</div>}
              </div>
              <button className={`cat-add${added.has(s.name) ? " added" : ""}`} title={added.has(s.name) ? "Already on the agent" : "Add to the agent"} onClick={() => {
                if (added.has(s.name)) return;
                setAdded((a) => new Set(a).add(s.name));
                onAdd(s, present);
                toast(connectors.length ? `${plainName(s.name)} added · ${present.length} tools with it; ${connectors.map(plainName).join(", ")} not connected` : `${plainName(s.name)} added${present.length ? ` · ${present.length} tools with it` : ""}`);
              }}><i className={`ti ${added.has(s.name) ? "ti-check" : "ti-plus"}`} /></button>
            </div>
          );
        })}
      </div>
    </div>
  );
}

export function AddToolsDrawer({ catalog, instances, have, onAdd }: { catalog: ServerTool[]; instances: ConnectorInstance[]; have: string[]; onAdd: (tools: { name: string; when?: string }[]) => void }) {
  const { close, toast } = useOverlay();
  const [tab, setTab] = useState<"lib" | "disc">("lib");
  const [q, setQ] = useState("");
  const [added, setAdded] = useState<Set<string>>(new Set(have));
  // Every connection is a source; what it offers comes from its manifest's operations when nothing is derived yet.
  const sources = instances;
  const [manifests, setManifestsForDiscovery] = useState<ConnectorManifest[]>([]);
  useEffect(() => { listConnectorManifests().then(setManifestsForDiscovery).catch(() => {}); }, []);
  // Built-ins carry no dot; the platform's own tools (agents.ask, catalog.*) do, but no connection derives them.
  const derived = new Set(sources.flatMap((i) => i.tools ?? []));
  const groups: Record<string, ServerTool[]> = { "Built in": catalog.filter((t) => !t.name.includes(".")), Platform: catalog.filter((t) => t.name.includes(".") && !derived.has(t.name)) };
  const [src, setSrc] = useState(0);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [intent, setIntent] = useState("");
  const [areas, setAreas] = useState<Set<string>>(new Set());
  const source = sources[src];
  const ops = useMemo(() => {
    if (!source) return [] as { name: string; desc: string; risk: "read" | "write" | "destructive"; area: string }[];
    const derived = (source.tools ?? []).map((name) => { const t = catalog.find((c) => c.name === name); return { name, desc: t?.description ?? "", risk: t ? riskOf(t.effect) : ("read" as const), area: name.split(".").pop()?.split("-").pop() ?? "" }; });
    if (derived.length) return derived;
    const m = manifests.find((x) => x.hash === source.manifest_hash);
    const id = source.connector?.id ?? m?.id ?? "connector";
    return (m?.operations ?? []).map((op) => ({ name: `${id}.${op.name}`, desc: op.description, risk: riskOf(op.effect), area: op.name.split("-").pop() ?? "" }));
  }, [source, catalog, manifests]);
  useEffect(() => { setSelected(new Set(ops.filter((o) => o.risk === "read" && !added.has(o.name)).map((o) => o.name))); }, [ops]); // eslint-disable-line react-hooks/exhaustive-deps
  const words = intent.toLowerCase().split(/\W+/).filter((w) => w.length > 3);
  const visible = ops.filter((o) => (!areas.size || areas.has(o.area)) && (!words.length || words.some((w) => `${o.name} ${o.desc}`.toLowerCase().includes(w))));
  const RISK = { read: ["Looks things up", "var(--good-dot)"], write: ["Changes things · asks first", "var(--warn-dot)"], destructive: ["Can't be undone · asks first", "var(--bad-dot)"] } as const;
  return (
    <div className="m-drawer">
      <OvHead icon="ti-tool" bg="var(--bg-muted)" fg="var(--ink-700)" title="Add tools" sub="Pick what the agent can do — from the catalog, or from a system you have connected." />
      <div style={{ padding: "14px 22px 0" }}><div className="m-seg conn-tabs" style={{ display: "flex" }}><button className={tab === "lib" ? "on" : ""} style={{ flex: 1 }} onClick={() => setTab("lib")}><i className="ti ti-library" /> Catalog</button><button className={tab === "disc" ? "on" : ""} style={{ flex: 1 }} onClick={() => setTab("disc")}><i className="ti ti-radar-2" /> From connectors</button></div></div>
      {tab === "lib" && (
        <div className="ov-body" style={{ paddingTop: 14 }}>
          <div className="ov-search"><i className="ti ti-search" /><input placeholder="Search tools…" value={q} onChange={(e) => setQ(e.target.value)} /></div>
          {Object.entries(groups).map(([g, list]) => (
            <div key={g}>
              <div className="cat-label"><span>{g}</span><span className="ln" /></div>
              {list.filter((t) => !q || `${t.name} ${t.description}`.toLowerCase().includes(q.toLowerCase())).map((t) => (
                <div key={t.name} className="item">
                  <div className="item-ic" style={{ background: "var(--bg-muted)", color: "var(--ink-700)" }}><i className={`ti ${toolIcon(t.name, t.effect)}`} /></div>
                  <div className="item-body"><div className="item-name" title={t.name}>{plainName(t.name)}</div><div className="item-desc" title={t.description}>{firstSentence(t.description)}</div></div>
                  <button className={`cat-add${added.has(t.name) ? " added" : ""}`} onClick={() => { if (added.has(t.name)) return; setAdded((a) => new Set(a).add(t.name)); onAdd([{ name: t.name }]); toast(`${plainName(t.name)} added`); }}><i className={`ti ${added.has(t.name) ? "ti-check" : "ti-plus"}`} /></button>
                </div>
              ))}
            </div>
          ))}
        </div>
      )}
      {tab === "disc" && (
        <div className="ov-body" style={{ paddingTop: 14 }}>
          <div className="f-mini-label">System</div><div className="m-hint" style={{ margin: "-4px 0 8px" }}>The agent uses the connection you already set up — nothing to sign in again.</div>
          <div className="src-list">
            {sources.map((s, i) => (
              <div key={s.instance_id} className={`src-opt${i === src ? " sel" : ""}`} onClick={() => setSrc(i)}>
                <div className="item-ic logo" style={{ background: "var(--bg-tint)", color: "#fff" }}><i className="ti ti-plug-connected" /></div>
                <div className="item-body"><div className="item-name">{s.connector?.display_name ?? s.instance_id}</div><div className="item-desc">{(() => { const n = (s.tools ?? []).length || (manifests.find((x) => x.hash === s.manifest_hash)?.operations.length ?? 0); return `${n} action${n === 1 ? "" : "s"}`; })()}</div></div>
                {s.authorization && (s.authorization.kind === "needs_auth" || s.authorization.kind === "expired") ? <Badge tone="warn">Needs authorization</Badge> : <Badge tone="good">Connected</Badge>}
              </div>
            ))}
            {sources.length === 0 && <div className="thread-empty">No connected service exposes operations yet. Connect one first.</div>}
          </div>
          {source && (
            <div className="disc-state">
              <div className="disc-sum"><i className="ti ti-check" style={{ color: "var(--good-dot)" }} /> <b>{ops.length}</b> action{ops.length === 1 ? "" : "s"} in {source.connector?.display_name ?? "this system"}. Ones that only look things up are ticked; ones that change things stay off until you tick them.</div>
              <div className="f-mini-label" style={{ marginTop: 14 }}>What will the agent use it for? <span className="caption" style={{ fontWeight: 400 }}>optional — narrows the list</span></div>
              <div className="ov-search" style={{ marginBottom: 8 }}><i className="ti ti-target-arrow" /><input placeholder="e.g. triage tickets and answer from the knowledge base" value={intent} onChange={(e) => setIntent(e.target.value)} /></div>
              <div className="area-chips">{[...new Set(ops.map((o) => o.area))].map((a) => <button key={a} className={`auth-opt${areas.has(a) ? " on" : ""}`} onClick={() => setAreas((s) => { const n = new Set(s); if (n.has(a)) n.delete(a); else n.add(a); return n; })}>{a}</button>)}</div>
              <div className="ops-head"><span className="f-mini-label" style={{ margin: 0 }}>Actions <span className="caption" style={{ fontWeight: 400 }}>{visible.length === ops.length ? ops.length : `${visible.length} of ${ops.length}`}</span></span><span className="sp" /><button className="m-btn ghost sm" onClick={() => setSelected(new Set(visible.filter((o) => o.risk === "read" && !added.has(o.name)).map((o) => o.name)))}>Reads only</button><button className="m-btn ghost sm" onClick={() => setSelected(new Set(visible.filter((o) => !added.has(o.name)).map((o) => o.name)))}>All</button><button className="m-btn ghost sm" onClick={() => setSelected(new Set())}>None</button></div>
              <div className="ops-list">
                {visible.map((o) => { const has = added.has(o.name); const r = RISK[o.risk]; return (
                  <label key={o.name} className={`op-row${has ? " have" : ""}`}>
                    <input type="checkbox" checked={selected.has(o.name)} disabled={has} onChange={(e) => setSelected((s) => { const n = new Set(s); if (e.target.checked) n.add(o.name); else n.delete(o.name); return n; })} /><span className="m-box"><svg viewBox="0 0 12 12" fill="none" stroke="currentColor" strokeWidth="2"><path d="M2 6l3 3 5-6" /></svg></span>
                    <div className="op-body"><div className="op-name" title={o.name}>{plainName(o.name)}{has && <span className="item-tag"> already added</span>}</div><div className="op-desc">{o.desc}</div></div>
                    <span className="op-risk" style={{ color: r[1] }}><span className="dot" style={{ background: r[1] }} />{r[0]}</span>
                  </label>
                ); })}
              </div>
            </div>
          )}
        </div>
      )}
      <div className="ov-foot"><span className="caption" /><div className="sp" />
        <button className="m-btn secondary" data-close>{tab === "disc" ? "Cancel" : "Done"}</button>
        {tab === "disc" && <button className="m-btn primary" disabled={selected.size === 0} onClick={() => { const list = [...selected].map((name) => ({ name })); onAdd(list); close(); toast(`${list.length} action${list.length === 1 ? "" : "s"} from ${source?.connector?.display_name ?? "the system"} added`, "ti-plug-connected"); }}><i className="ti ti-plus" /> Add {selected.size} action{selected.size === 1 ? "" : "s"}</button>}
      </div>
    </div>
  );
}

/* ───────── Add connector drawer (520px): library | custom protocol ───────── */
const AUTH = {
  oauth: { label: "OAuth 2.0", icon: "ti-shield-lock", hint: "Sign in with the provider. Tokens rotate automatically." },
  token: { label: "API key", icon: "ti-key", hint: "Paste a key or personal access token. Stored sealed." },
  conn: { label: "Connection string", icon: "ti-plug", hint: "Host, credentials and database. Read-only role recommended." },
  none: { label: "No auth", icon: "ti-lock-open", hint: "Public endpoint. Nothing is stored." },
} as const;
type Auth = keyof typeof AUTH;
export const authLabel = (m: ConnectorManifest) => AUTH[authOf(m)];
const authOf = (m: ConnectorManifest): Auth => {
  const spec = JSON.stringify(m.connection_specification ?? {}).toLowerCase();
  if (m.authorization) return "oauth";
  if (/token|api_key|apikey|password|secret/.test(spec)) return "token";
  if (/host|dsn|database/.test(spec)) return "conn";
  return "none";
};

export function AddConnectorDrawer({ onConnected }: { onConnected: () => void }) {
  const { open, close, toast } = useOverlay();
  const [tab, setTab] = useState<"lib" | "custom" | "packages">("lib");
  const [manifests, setManifests] = useState<ConnectorManifest[]>([]);
  const [instances, setInstances] = useState<ConnectorInstance[]>([]);
  const [q, setQ] = useState("");
  useEffect(() => {
    void listConnectorManifests().then(setManifests).catch(() => {});
    listConnectorInstances().then(setInstances).catch(() => {});
  }, []);
  const connected = new Set(instances.map((i) => i.manifest_hash));
  const [proto, setProto] = useState<"mcp" | "rest" | "db" | "webhook">("rest");
  const [name, setName] = useState("");
  const [url, setUrl] = useState("");
  const [busy, setBusy] = useState(false);
  // A REST service in two steps a person follows: read its API description, then choose what agents may do with it.
  const [read, setRead] = useState<{ spec: unknown; available: OfferedOperation[]; cap: number; signIn: SignIn } | null>(null);
  const [chosen, setChosen] = useState<Set<string>>(new Set());
  const [opQ, setOpQ] = useState("");
  const draftInput = (spec: unknown, signIn: SignIn, operations?: string[]) => ({ id: slug(name) || "custom-api", display_name: name.trim() || "Custom API", description: `${name.trim() || "Custom API"} over its published API.`, documentation_url: url, base_url: (() => { try { return new URL(url).origin; } catch { return ""; } })(), auth: signIn.auth, ...(signIn.name ? { auth_name: signIn.name } : {}), ...(operations ? { operations } : {}), spec });
  async function readDescription() {
    // The studio reads the description from the address (JSON, else text for the server to parse).
    const raw = await fetch(url).then((r) => { if (!r.ok) throw new Error(`that address answered ${r.status} — check it points at the API description`); return r.text(); }, () => { throw new Error("that site does not let the studio read its description — download it and host it where the studio can reach it"); });
    let spec: unknown; try { spec = JSON.parse(raw); } catch { spec = raw; }
    const signIn = signInOf(spec);
    const drafted = await connectorFromOpenApi(draftInput(spec, signIn));
    const available = drafted.available ?? drafted.manifest?.operations.filter((o) => o.name !== "check-connection").map((o) => ({ name: o.name, description: o.description, method: o.method, path: o.path, effect: o.effect })) ?? [];
    const cap = drafted.choose?.cap ?? available.length;
    setRead({ spec, available, cap, signIn });
    // Everything it offers, when that fits in one connector; otherwise the builder picks.
    setChosen(new Set(available.length <= cap ? available.map((o) => o.name) : []));
  }
  async function saveChosen() {
    if (!read) return;
    const drafted = await connectorFromOpenApi(draftInput(read.spec, read.signIn, [...chosen]));
    if (!drafted.manifest) throw new Error(`pick at most ${read.cap}`);
    const { hash } = await registerConnectorManifest(drafted.manifest);
    const all = await listConnectorManifests(); setManifests(all);
    const m = all.find((x) => x.hash === hash);
    toast(`${name.trim() || "The service"} added · ${chosen.size} action${chosen.size === 1 ? "" : "s"}`, "ti-plug-connected");
    if (m) open("modal", <CredentialModal manifest={m} onDone={onConnected} />); else close();
  }
  async function testAndSave() {
    setBusy(true);
    try {
      if (proto === "rest") {
        if (read) await saveChosen(); else await readDescription();
      } else if (proto === "mcp") {
        open("drawer", <McpMountDrawer onMounted={onConnected} />);
      } else {
        setTab("lib"); toast(proto === "db" ? "Databases connect through a library manifest (Ledger, for one) — pick it here." : "Outbound webhooks are a trigger's answer, not a connection: add one under the agent's Triggers.", "ti-info-circle");
      }
    } catch (err) { toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle"); }
    finally { setBusy(false); }
  }
  return (
    <div className="m-drawer">
      <OvHead icon="ti-plug-connected" bg="var(--bg-muted)" fg="var(--ink-700)" title="Add connector" sub="Pick a service from the library, or wire up anything that speaks a standard protocol." />
      <div style={{ padding: "14px 22px 0" }}><div className="m-seg conn-tabs" style={{ display: "flex" }}><button className={tab === "lib" ? "on" : ""} style={{ flex: 1 }} onClick={() => setTab("lib")}><i className="ti ti-library" /> Library</button><button className={tab === "custom" ? "on" : ""} style={{ flex: 1 }} onClick={() => setTab("custom")}><i className="ti ti-code" /> Custom protocol</button><button className={tab === "packages" ? "on" : ""} style={{ flex: 1 }} onClick={() => setTab("packages")}><i className="ti ti-package" /> Packages</button></div></div>
      {tab === "packages" && <PackagesTab onChanged={onConnected} />}
      {tab === "lib" && (
        <div className="ov-body" style={{ paddingTop: 14 }}>
          <div className="ov-search"><i className="ti ti-search" /><input placeholder={`Search ${manifests.length} connectors…`} value={q} onChange={(e) => setQ(e.target.value)} /></div>
          <div className="cat-label"><span>Library</span><span className="ln" /></div>
          {manifests.filter((m) => !q || `${m.display_name} ${m.description}`.toLowerCase().includes(q.toLowerCase())).map((m) => { const a = AUTH[authOf(m)]; const is = connected.has(m.hash); return (
            <div key={m.hash} className="item">
              <div className="item-ic logo" style={{ background: "var(--bg-tint)", color: "#fff" }}><i className="ti ti-plug-connected" /></div>
              <div className="item-body"><div className="item-name">{m.display_name}</div><div className="item-desc">{m.description}</div><div className="item-meta"><span className="item-tag auth-tag"><i className={`ti ${a.icon}`} /> {a.label}</span><span className="item-tag">{m.operations.length} ops</span></div></div>
              {is ? <Badge tone="good">Connected</Badge> : <button className="m-btn secondary sm" onClick={() => open("modal", <CredentialModal manifest={m} onDone={() => { onConnected(); }} />)}>Connect</button>}
            </div>
          ); })}
          {manifests.length === 0 && <div className="thread-empty">The library is empty; describe a system under Custom protocol.</div>}
        </div>
      )}
      {tab === "custom" && (
        <div className="ov-body" style={{ paddingTop: 14 }}>
          {!read && <><div className="f-mini-label">Protocol</div>
          <div className="proto-list">
            {([["mcp", "ti-server-2", "MCP server", "Remote MCP endpoint. Tools are discovered automatically."], ["rest", "ti-api", "REST / OpenAPI", "Import an OpenAPI spec. Each operation becomes a tool."], ["db", "ti-database", "Database", "Postgres, MySQL, Snowflake or BigQuery. Tables become read tools."], ["webhook", "ti-webhook", "Webhook", "Outbound POST to a URL you own."]] as const).map(([id, ic, nm, ds]) => (
              <div key={id} className={`proto-opt${proto === id ? " sel" : ""}`} onClick={() => setProto(id)}><div className="item-ic pt-ic"><i className={`ti ${ic}`} /></div><div className="item-body"><div className="item-name">{nm}</div><div className="item-desc">{ds}</div></div><i className="ti ti-circle-check proto-check" /></div>
            ))}
          </div></>}
          <div className="frow" style={{ marginTop: read ? 0 : 18 }}>
            <div className="m-field"><label className="m-label">Name</label><input className="m-input" placeholder="e.g. Internal billing API" value={name} onChange={(e) => setName(e.target.value)} /></div>
            {proto === "rest" && !read && <div className="m-field"><label className="m-label">Address of its API description</label><input className="m-input" placeholder="https://api.example.com/openapi.json" value={url} onChange={(e) => setUrl(e.target.value)} style={{ fontFamily: "var(--font-mono)", fontSize: 12 }} /></div>}
            {proto === "rest" && !read && <div className="m-hint">Most services publish one (an OpenAPI file). Next you choose what agents may do with it.</div>}
            {proto === "rest" && read && <ChooseOperations read={read} chosen={chosen} setChosen={setChosen} q={opQ} setQ={setOpQ} signIn={read.signIn} setSignIn={(signIn) => setRead({ ...read, signIn })} />}
            {proto === "mcp" && <div className="m-hint">Continue to launch the server, see its tools and name each one's effect class before mounting.</div>}
            {proto === "db" && <div className="m-hint">A database is a library connector with a connection string (Ledger is one); pick it under Library.</div>}
            {proto === "webhook" && <div className="m-hint">An outbound POST is what a trigger answers with; add a webhook under the agent's Triggers.</div>}
          </div>
        </div>
      )}
      <div className="ov-foot">{!read && <span className="caption">{tab === "custom" ? "Credentials are sealed at rest and never shown again." : "Connected services are shared across all agents in this workspace."}</span>}<div className="sp" />
        <button className="m-btn secondary" data-close>{tab === "custom" ? "Cancel" : "Done"}</button>
        {tab === "custom" && proto === "rest" && read && <button className="m-btn ghost" onClick={() => setRead(null)}>Back</button>}
        {tab === "custom" && <button className="m-btn primary" disabled={busy || (proto === "rest" && (!url || (!!read && (chosen.size === 0 || chosen.size > read.cap))))} onClick={() => void testAndSave()}><i className="ti ti-plug-connected" /> {busy ? (read ? "Saving…" : "Reading…") : proto === "mcp" ? "Continue" : proto === "rest" ? (read ? `Add ${chosen.size} action${chosen.size === 1 ? "" : "s"}` : "Read it") : "Test & save"}</button>}
      </div>
    </div>
  );
}

/** What agents may do with a service, chosen in words: each operation by name, what it does, and whether it changes anything. */
function ChooseOperations({ read, chosen, setChosen, q, setQ, signIn, setSignIn }: { read: { available: OfferedOperation[]; cap: number }; chosen: Set<string>; setChosen: (s: Set<string>) => void; q: string; setQ: (q: string) => void; signIn: SignIn; setSignIn: (s: SignIn) => void }) {
  const RISK = { read: ["Looks things up", "good"], write: ["Changes things · asks first", "warn"], destructive: ["Can't be undone · asks first", "bad"] } as const;
  const shown = read.available.filter((o) => !q || `${o.name} ${plainName(o.name)} ${o.description}`.toLowerCase().includes(q.toLowerCase()));
  const toggle = (n: string) => { const next = new Set(chosen); if (next.has(n)) next.delete(n); else next.add(n); setChosen(next); };
  const over = chosen.size > read.cap;
  return (
    <div className="m-field" data-choose-ops>
      <label className="m-label">How it signs in</label>
      <select className="m-select" value={signIn.auth} onChange={(e) => setSignIn({ auth: e.target.value as GenericAuth, ...(signIn.name ? { name: signIn.name } : {}) })}>
        {(Object.keys(SIGN_IN_WORDS) as GenericAuth[]).map((k) => <option key={k} value={k}>{SIGN_IN_WORDS[k]}</option>)}
      </select>
      <div className="m-hint">Read from its description{signIn.auth === "none" ? " — nothing to paste." : `${signIn.name ? ` (it names ${signIn.name})` : ""}; you add it when you connect.`}</div>
      <label className="m-label" style={{ marginTop: 14 }}>What agents may do with it <span className="opt">— {chosen.size} chosen{read.available.length > read.cap ? `, up to ${read.cap}` : ""}</span></label>
      {read.available.length > 8 && <input className="m-input sm" style={{ marginBottom: 6 }} value={q} onChange={(e) => setQ(e.target.value)} placeholder={`Find an action — ${read.available.length} offered`} />}
      {over && <div className="m-hint" style={{ color: "var(--bad)" }}>One connection holds {read.cap} actions — untick {chosen.size - read.cap}.</div>}
      <div className="skf-tools" style={{ maxHeight: 340, overflow: "auto" }}>
        {shown.map((o) => { const [label, k] = RISK[riskOf(o.effect)]; return (
          <label key={o.name} className={`skf-tool skf-tool-h${chosen.has(o.name) ? " on" : ""}`} title={`${o.method} ${o.path}`} style={{ flexDirection: "row", alignItems: "flex-start" }}>
            <input type="checkbox" checked={chosen.has(o.name)} onChange={() => toggle(o.name)} style={{ marginTop: 3 }} />
            <span style={{ display: "flex", flexDirection: "column", gap: 2, flex: 1 }}><span>{plainName(o.name)}</span>{o.description && <span style={{ fontWeight: 400, fontSize: 12.5, color: "var(--ink-500)" }}>{firstSentence(o.description, 120)}</span>}</span>
            {k !== "good" && <Badge tone={k}>{label}</Badge>}
          </label>); })}
        {shown.length === 0 && <div className="skf-empty">No action matches “{q}”.</div>}
      </div>
    </div>
  );
}

/** Connect a library connector with a credential: the fields its manifest names. */
export function CredentialModal({ manifest, onDone }: { manifest: ConnectorManifest; onDone: () => void }) {
  const { close, toast } = useOverlay();
  // The form is the connector's own connection specification, read by the one spec engine:
  // groups (a credentials object), secrets and choices render as the connector declares them.
  const nodes = useMemo(() => readSpec((manifest.connection_specification ?? {}) as Schema), [manifest.hash]);
  const [values, setValues] = useState<Record<string, string>>({});
  const [chosen, setChosen] = useState<Record<string, number>>({});
  const [busy, setBusy] = useState(false);
  const a = AUTH[authOf(manifest)];
  const missing = missingRequired(nodes, values, chosen);
  async function connect() {
    if (missing.length) { toast(`Fill in ${missing.join(", ")}`, "ti-alert-triangle"); return; }
    setBusy(true);
    try { await createConnectorInstance(manifest.hash, buildConfig(nodes, values, chosen)); toast(`${manifest.display_name} connected`, "ti-plug-connected"); onDone(); close(); }
    catch (err) { toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle"); }
    finally { setBusy(false); }
  }
  return (
    <div className="m-modal">
      <OvHead icon="ti-plug-connected" bg="var(--bg-tint)" fg="#fff" logo title={`Connect ${manifest.display_name}`} sub={manifest.description} />
      <div className="ov-body">
        <div className="auth-method"><i className={`ti ${a.icon}`} /><div><b>{a.label}</b><span>{a.hint}</span></div></div>
        <div className="frow" style={{ marginTop: 16 }}>
          <SpecFields nodes={nodes} values={values} setValue={(k, v) => setValues((x) => ({ ...x, [k]: v }))} chosen={chosen} choose={(k, i) => setChosen((c) => ({ ...c, [k]: i }))} />
          {fieldsOf(nodes).length === 0 && <div className="m-hint">This connector needs no credential.</div>}
        </div>
      </div>
      <div className="ov-foot"><div className="sp" /><button className="m-btn ghost" data-close>Cancel</button><button className="m-btn primary" disabled={busy} onClick={() => void connect()}><i className="ti ti-plug-connected" /> {busy ? "Testing…" : "Test & connect"}</button></div>
    </div>
  );
}

/** A connection specification as fields: a group by its title, a choice as a picker, a secret never echoed. */
function SpecFields({ nodes, values, setValue, chosen, choose }: { nodes: SpecNode[]; values: Record<string, string>; setValue: (key: string, value: string) => void; chosen: Record<string, number>; choose: (key: string, index: number) => void }) {
  return <>{nodes.map((node) => {
    if (node.kind === "field") {
      const f = node.field; const k = pathKey(f.path);
      return (
        <div key={k} className="m-field"><label className="m-label">{f.title}{f.required ? " *" : ""}</label>
          {f.kind === "select" ? <select className="m-select" value={values[k] ?? ""} onChange={(e) => setValue(k, e.target.value)}><option value="">—</option>{(f.options ?? []).map((o) => <option key={o} value={o}>{o}</option>)}</select>
            : f.kind === "boolean" ? <select className="m-select" value={values[k] ?? ""} onChange={(e) => setValue(k, e.target.value)}><option value="">—</option><option value="true">Yes</option><option value="false">No</option></select>
            : <input className="m-input" type={f.kind === "secret" ? "password" : f.kind === "number" ? "number" : "text"} placeholder={f.placeholder ?? ""} value={values[k] ?? ""} onChange={(e) => setValue(k, e.target.value)} autoComplete="off" />}
          {f.hint && <div className="m-hint">{f.hint}</div>}
        </div>
      );
    }
    if (node.kind === "group") return <SpecFields key={pathKey(node.path)} nodes={node.nodes} values={values} setValue={setValue} chosen={chosen} choose={choose} />;
    const k = pathKey(node.path); const at = chosen[k] ?? 0;
    return (
      <div key={k} className="m-field"><label className="m-label">{node.title}</label>
        <select className="m-select" value={at} onChange={(e) => choose(k, Number(e.target.value))}>{node.variants.map((v, i) => <option key={v.title} value={i}>{v.title}</option>)}</select>
        {node.variants[at] && <SpecFields nodes={node.variants[at].nodes} values={values} setValue={setValue} chosen={chosen} choose={choose} />}
      </div>
    );
  })}</>;
}

/* ───────── Triggers: schedule or webhook ───────── */
export function TriggerModal({ agent, onAdded, onPool }: { agent: Assistant; onAdded: () => void; onPool?: (pool: string) => void }) {
  const { close, toast } = useOverlay();
  const [kind, setKind] = useState<"schedule" | "webhook" | "pool" | null>(null);
  const [pool, setPool] = useState(slug(agent.name));
  const [cron, setCron] = useState("0 9 * * *");
  const [maxRuns, setMaxRuns] = useState("");
  const [maxTokens, setMaxTokens] = useState("");
  const [message, setMessage] = useState("");
  const [name, setName] = useState("Custom webhook");
  const [busy, setBusy] = useState(false);
  async function save() {
    setBusy(true);
    try {
      if (kind === "pool") { onPool?.(pool.trim()); toast(`Pool ${pool.trim()} set — publish to start claiming`, "ti-list-check"); onAdded(); close(); return; }
      if (kind === "schedule") { const c = cadenceOf(cron) ?? { cron_expr: cron }; await createSchedule({ assistant_id: agent.assistant_id, ...c, input: { messages: [{ role: "user", content: message || "Do your job." }] }, ...(Number(maxRuns) > 0 ? { max_runs: Math.round(Number(maxRuns)) } : {}), ...(Number(maxTokens) > 0 ? { max_tokens: Math.round(Number(maxTokens)) } : {}) }); toast("Schedule added", "ti-bolt"); }
      else { await createWebhook({ name, target: { kind: "assistant", id: agent.assistant_id }, action: "start_run", input_template: { messages: [{ role: "user", content: "{{body}}" }] }, enabled: true }); toast(`"${name}" trigger added`, "ti-bolt"); }
      onAdded(); close();
    } catch (err) { toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle"); }
    finally { setBusy(false); }
  }
  if (!kind) return (
    <div className="m-modal lg">
      <OvHead icon="ti-bolt" title="Add a trigger" sub="Choose what starts a run of this agent." />
      <div className="ov-body">
        {([["schedule", "ti-clock-hour-4", "Schedule", "Run on a cron schedule (e.g. every hour)."], ["webhook", "ti-webhook", "Webhook", "Run on a signed POST to a generated URL."]] as const).map(([k, ic, n, d]) => (
          <div key={k} className="trig-opt" onClick={() => setKind(k)}><div className="to-ic"><i className={`ti ${ic}`} /></div><div><div className="to-name">{n}</div><div className="to-desc">{d}</div></div><i className="ti ti-chevron-right to-go" /></div>
        ))}
        {onPool && <div className="trig-opt" onClick={() => setKind("pool")}><div className="to-ic"><i className="ti ti-list-check" /></div><div><div className="to-name">Task queue</div><div className="to-desc">Run once per task the server claims from a pool.</div></div><i className="ti ti-chevron-right to-go" /></div>}
        <div className="trig-opt" onClick={() => { close(); toast("Runs start from the API with POST /threads/{thread}/runs", "ti-api"); }}><div className="to-ic"><i className="ti ti-hand-click" /></div><div><div className="to-name">Manual / API</div><div className="to-desc">Run on demand from the API or the test panel.</div></div><i className="ti ti-chevron-right to-go" /></div>
      </div>
    </div>
  );
  return (
    <div className="m-modal lg">
      <OvHead icon={kind === "schedule" ? "ti-clock-hour-4" : kind === "pool" ? "ti-list-check" : "ti-webhook"} title={kind === "schedule" ? "Schedule" : kind === "pool" ? "Task queue" : "Webhook"} sub={kind === "schedule" ? "Run on a cron schedule." : kind === "pool" ? "The server claims tasks from the pool and runs the agent with each." : "Run on a signed POST."} />
      <div className="ov-body">
        {kind === "pool" ? (
          <div className="fld"><label className="fld-label">Pool</label><input className="m-input" value={pool} onChange={(e) => setPool(e.target.value)} style={{ fontFamily: "var(--font-mono)" }} /><div className="m-hint" style={{ marginTop: 8 }}>Tasks land with <span className="mono">POST /tasks</span> naming this pool; each becomes one run.</div></div>
        ) : kind === "schedule" ? (
          <>
            <div className="fld"><label className="fld-label">Frequency</label><div className="cron-row">{[["Every hour", "0 * * * *"], ["Daily 9:00", "0 9 * * *"], ["Weekdays 8:00", "0 8 * * 1-5"], ["Every 15 min", "*/15 * * * *"]].map(([l, c]) => <span key={c} className={`preset${cron === c ? " on" : ""}`} onClick={() => setCron(c)}>{l}</span>)}</div><input className="m-input" value={cron} onChange={(e) => setCron(e.target.value)} style={{ fontFamily: "var(--font-mono)" }} /></div>
            <div className="fld"><label className="fld-label">Each run is asked</label><input className="m-input" value={message} onChange={(e) => setMessage(e.target.value)} placeholder="Read what is open and brief the desk." /></div>
            <div className="fld"><label className="fld-label">At most</label><input className="m-input" type="number" min={1} value={maxRuns} onChange={(e) => setMaxRuns(e.target.value)} placeholder="runs, ever — leave empty for no cap" style={{ maxWidth: 260 }} /><div className="m-hint">A budget that follows the schedule: once it has fired this many times it stops and its row says so.</div></div>
            <div className="fld"><label className="fld-label">Spend at most</label><input className="m-input" type="number" min={1} step={1000} value={maxTokens} onChange={(e) => setMaxTokens(e.target.value)} placeholder="tokens, all its runs together — leave empty for no cap" style={{ maxWidth: 320 }} /><div className="m-hint">The spend that follows the schedule: what its runs have used adds up as each one ends, and past this it stops and says so.</div></div>
          </>
        ) : (
          <div className="fld"><label className="fld-label">Name</label><input className="m-input" value={name} onChange={(e) => setName(e.target.value)} /><div className="m-hint" style={{ marginTop: 8 }}>The endpoint and its signing secret are shown once the webhook exists, on the Triggers block.</div></div>
        )}
      </div>
      <div className="ov-foot"><button className="m-btn ghost" onClick={() => setKind(null)}><i className="ti ti-arrow-left" /> Back</button><div className="sp" /><button className="m-btn primary" disabled={busy} onClick={() => void save()}><i className="ti ti-check" /> Add trigger</button></div>
    </div>
  );
}

/* ───────── Evaluation goal: a dataset from a run of this agent ───────── */
export function EvalGoalModal({ agent, onSaved }: { agent: Assistant; onSaved: () => void }) {
  const { close, toast } = useOverlay();
  type Listed = { run_id: string; asked?: string | null; assistant_id?: string; status: string; created_at?: string | null; metadata?: unknown };
  const [all, setAll] = useState<Listed[]>([]);
  // A run that carried on after a person's yes is the second half of one
  // conversation: it is offered as the person's question, and the case is cut
  // from the run that asked it — the one whose input is theirs.
  const originOf = (r: Listed) => { const of = (r.metadata as { approval_of?: string } | undefined)?.approval_of; return (of && all.find((x) => x.run_id === of)) || r; };
  const runs = all.filter((x) => x.status === "success");
  const [runsLoading, setRunsLoading] = useState(true);
  const [pick, setPick] = useState<string>("");
  const [name, setName] = useState(slug(agent.name));
  const [rubric, setRubric] = useState("");
  const [busy, setBusy] = useState(false);
  // An agent that carries memory changes what the right steps are between one
  // run and the next — it learns. Pinning the trajectory from one run then
  // fails it for improving, so such a suite judges the reply by default.
  const carriesMemory = (agent.config?.studio_intent?.tools ?? []).some((t) => t.name.startsWith("memory.")) || !!agent.config?.studio_intent?.memory?.access;
  const [checks, setChecks] = useState<"reply" | "reply-and-steps">(carriesMemory ? "reply" : "reply-and-steps");
  const [problem, setProblem] = useState<string | null>(null);
  useEffect(() => { listRuns(50, agent.assistant_id).then((r) => setAll(r as Listed[])).catch(() => {}).finally(() => setRunsLoading(false)); }, [agent.assistant_id]);
  async function save() {
    if (!pick) return;
    setBusy(true);
    try {
      const picked = runs.find((r) => r.run_id === pick);
      const sourceId = picked ? originOf(picked).run_id : pick;
      const run = await getRun(sourceId);
      // The path it took is the whole conversation, which the finished run holds.
      const finished = sourceId === pick ? run : await getRun(pick);
      const asked = run.output?.messages?.find((m) => m.role === "user")?.content ?? finished.output?.messages?.find((m) => m.role === "user")?.content ?? "";
      // The case replays the run, so it carries the run's own input — the
      // charter the server prepended included. Rebuilding it from the reply's
      // messages loses that, and the server refuses a case that does not match
      // the run it was cut from.
      const input = run.input && Array.isArray((run.input as { messages?: unknown }).messages)
        ? (run.input as { messages: { role: string; content: string }[] })
        : { messages: [{ role: "user", content: asked }] };
      const calls = [...new Set((finished.output?.messages ?? []).flatMap((m) => (m.tool_calls ?? []).map((c) => c.function?.name ?? "")).filter(Boolean))];
      await createDataset({ name, version: new Date().toISOString().slice(0, 10), cases: [{ id: slug(asked).slice(0, 40) || "case-1", source: { run_id: sourceId, thread_id: run.thread_id, agent_id: agent.assistant_id, captured_at: new Date().toISOString() }, input, expect: { ...(checks === "reply-and-steps" && calls.length ? { tool_trajectory: calls.filter((c) => !/create|post|send|file/.test(c)).map((c) => ({ name: c })) } : {}), ...(rubric ? { rubric } : {}) } }] });
      toast("Evaluation goal saved", "ti-target-arrow"); onSaved(); close();
    } catch (err) { const why = err instanceof Error ? err.message : "the server refused"; setProblem(why); toast(why, "ti-alert-triangle"); }
    finally { setBusy(false); }
  }
  return (
    <div className="m-modal lg">
      <OvHead icon="ti-target-arrow" title="Set an evaluation goal" sub="A suite from a good run of this agent; every version is judged against it before it runs." />
      <div className="ov-body">
        <div className="fld"><label className="fld-label">A run that went well</label><div className="opt-list">
          {runs.slice(0, 6).map((r) => <div key={r.run_id} className={`trig-opt${pick === r.run_id ? " sel" : ""}`} onClick={() => setPick(r.run_id)}><div><div className="to-name">{originOf(r).asked || r.asked || "A run without a question"}</div><div className="to-desc" title={r.run_id}>{originOf(r) !== r ? "Finished after a person's yes · " : ""}{r.created_at ? ago(r.created_at) : ""}</div></div></div>)}
          {runsLoading && runs.length === 0 && <div className="m-hint"><span className="m-spin" style={{ width: 12, height: 12, borderWidth: 2, verticalAlign: -2, marginRight: 6 }} />Reading the agent's runs…</div>}
          {!runsLoading && runs.length === 0 && <div className="m-hint">No finished run yet — test the agent first.</div>}
        </div></div>
        {problem && <div className="m-alert"><i className="ti ti-alert-triangle" style={{ color: "var(--bad)", fontSize: 16 }} /><div className="a-body"><div className="a-text">{problem}</div></div></div>}
        <div className="fld"><label className="fld-label">What the suite checks</label><div className="opt-list">
          {([["reply", "ti-message-check", "The reply", "A judge reads the answer against the rule below. Use this when the agent carries memory — what it does next changes as it learns."], ["reply-and-steps", "ti-route", "The reply and the steps", "Also pins the tools this run called, in order. Use this when the path itself is the contract."]] as const).map(([k, ic, n, d]) => (
            <div key={k} className={`trig-opt${checks === k ? " sel" : ""}`} data-checks={k} onClick={() => setChecks(k)}><div className="to-ic"><i className={`ti ${ic}`} /></div><div><div className="to-name">{n}{carriesMemory && k === "reply" ? " · recommended" : ""}</div><div className="to-desc">{d}</div></div></div>
          ))}
        </div>{carriesMemory && <div className="m-hint">This agent calls memory tools, so its own steps change run to run.</div>}</div>
        <div className="frow two"><div className="fld"><label className="fld-label">Dataset</label><input className="m-input" value={name} onChange={(e) => setName(e.target.value)} /></div><div className="fld"><label className="fld-label">What a good reply must do</label><input className="m-input" value={rubric} onChange={(e) => setRubric(e.target.value)} placeholder="in words; the judge reads it" /></div></div>
      </div>
      <div className="ov-foot"><div className="sp" /><button className="m-btn ghost" data-close>Cancel</button><button className="m-btn primary" disabled={busy || !pick} onClick={() => void save()}><i className="ti ti-check" /> Save goal</button></div>
    </div>
  );
}


/** A skill's evidence: the revision its followers run, the newest, each follower's suite verdict, and the promotion gate. */
export function SkillEvidenceDrawer({ name }: { name: string }) {
  const { close, toast } = useOverlay();
  const [ev, setEv] = useState<SkillEvidence | null>(null);
  const [reason, setReason] = useState("");
  const [busy, setBusy] = useState(false);
  useEffect(() => { skillEvidence(name).then(setEv).catch(() => setEv(null)); }, [name]);
  const behind = ev ? ev.latest > ev.current : false;
  async function promote(override?: string) {
    setBusy(true);
    try { const r = await promoteSkill(name, undefined, override); toast(r.promoted ? `${name} promoted to revision ${r.current}` : `Held at revision ${r.current} — the gate did not pass`, r.promoted ? "ti-shield-check" : "ti-alert-triangle"); if (r.promoted) { void useServer.getState().refresh(); close(); } }
    catch (err) { toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle"); }
    finally { setBusy(false); }
  }
  return (
    <div className="m-drawer">
      <OvHead icon="ti-shield-check" bg="var(--cat-plum-bg)" fg="var(--cat-plum)" title={`${name} · evidence`} sub={ev ? `Followers run revision ${ev.current}${behind ? ` · revision ${ev.latest} waits at the gate` : " · newest"}` : "Reading…"} />
      <div className="ov-body">
        {ev && (
          <>
            <div className="kv"><span className="k">Runs now</span><span className="v">revision {ev.current}</span><span className="k">Newest</span><span className="v">revision {ev.latest}</span><span className="k">Gate</span><span className="v">{ev.evidence.unevaluated ? "no suite has run on the newest revision" : ev.evidence.ok ? "every follower's suite passed" : "a follower's suite did not pass"}</span></div>
            <div className="cat-label"><span>Followers' suites</span><span className="ln" /></div>
            {ev.evidence.suites.map((s) => <div key={`${s.assistant_id}:${s.dataset}`} className="scope-row"><div className="sb"><div style={{ fontSize: "var(--fs-sm)", fontWeight: 600 }}>{s.assistant}</div><div className="sd">{s.dataset} v{s.version} · {s.cases} cases{s.evaluated_at ? ` · ${ago(s.evaluated_at)}` : ""}</div></div><Badge tone={s.state === "passed" ? "good" : s.state === "failed" ? "bad" : s.state === "running" ? "info" : "warn"}>{s.state === "passed" || s.state === "failed" ? `${s.passed}/${s.total} ${s.state}` : s.state}</Badge></div>)}
            {ev.evidence.suites.length === 0 && <div className="m-hint">No follower has a suite; the gate has nothing to hold.</div>}
            {ev.promotions.length > 0 && <><div className="cat-label" style={{ marginTop: 22 }}><span>Promotions</span><span className="ln" /></div>{ev.promotions.slice(0, 6).map((p) => <div key={p.revision} className="scope-row"><div className="sb"><div style={{ fontSize: "var(--fs-sm)", fontWeight: 600 }}>revision {p.revision}</div><div className="sd">{ago(p.at)}{p.override_reason ? ` · overrode the gate: ${p.override_reason}` : ""}</div></div></div>)}</>}
            {behind && !ev.evidence.ok && <div className="fld" style={{ marginTop: 14 }}><label className="fld-label">Promote anyway — a reason the record keeps</label><input className="m-input" value={reason} onChange={(e) => setReason(e.target.value)} placeholder="Why revision {latest} should run before its suites pass" /></div>}
          </>
        )}
      </div>
      <div className="ov-foot"><div className="sp" /><button className="m-btn secondary" data-close>Close</button>{ev && behind && <button className="m-btn primary" disabled={busy || (!ev.evidence.ok && !reason.trim())} onClick={() => void promote(ev.evidence.ok ? undefined : reason.trim())}><i className="ti ti-shield-check" /> Promote revision {ev.latest}</button>}</div>
    </div>
  );
}
