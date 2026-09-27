import { useEffect, useMemo, useState } from "react";
import { useNavigate } from "@tanstack/react-router";
import { createAssistant, createSchedule, listConnectorInstances, llmProviders, serverInfo, type Assistant, type ConnectorInstance, type LlmProviders, type ServerTool } from "../../engine/net/client";
import { useServer } from "../../engine/net/server";
import { useOverlay } from "../overlay";
import { COLORS, COLOR_BG, ICONS, riskOf, slug, toolIcon } from "../data";
import { AddToolsDrawer } from "./agentFlows";
import { DEFAULT_STEPS } from "../agents/useAgent";
import { firstSentence, plainName, shortModel } from "../agents/words";

/**
 * The create wizard the design puts behind New agent: six steps — Template ·
 * Identity · Goal · Instructions · Model & tools · Review — then the agent
 * exists on the server with all of it, and the builder opens on it.
 */
const STEPS = ["Template", "Identity", "Goal", "Instructions", "Model & tools", "Review"];
/** Templates: a look, a name, a line, a charter, and the tools this server has for the job. */
/** A cadence a template ships with: the wizard creates the schedule with the agent, capped from day one. */
interface TplSchedule { interval_secs: number; every: string; max_runs: number; max_tokens: number; input: string }
/** A template may start with more reasoning steps than the default: research and code read many things before they answer. */
const TEMPLATES: Record<string, { icon: string; color: string; name: string; desc: string; instr: string; tools: string[]; goal: string; schedule?: TplSchedule; steps?: number }> = {
  "Blank agent": { icon: "ti-square-plus", color: "blue", name: "New agent", desc: "A fresh agent. Describe what it should do.", instr: "You are a helpful assistant. Describe the task you handle and the steps you take.", tools: [], goal: "" },
  "Support agent": { icon: "ti-headset", color: "teal", name: "Support Triage", desc: "Routes inbound tickets, drafts replies, and escalates when unsure.", instr: "You are the first responder for inbound customer tickets.\n\nFor every ticket:\n1. Read what is already open before acting.\n2. Search the knowledge base before answering; answer only from what it says.\n3. Draft a warm, concise reply under 120 words.\n4. If confidence is low or the person is angry, say so and hand off.", tools: ["search_knowledge"], goal: "Resolve 70% of tier-1 tickets without a person, with every reply grounded in the knowledge base." },
  "Data analyst": { icon: "ti-chart-pie", color: "plum", name: "Revenue Analyst", desc: "Reads the records and answers questions with numbers you can trace.", instr: "You are a data analyst. Read the records, count what you read, and answer with numbers you can trace to rows. State the range and filters you applied; never estimate.", tools: ["calculator"], goal: "Answer 80% of questions correctly on the first try, with the rows behind every number." , steps: 40 },
  "Outbound SDR": { icon: "ti-mail-fast", color: "amber", name: "Outbound SDR", desc: "Researches leads and drafts personalized outreach.", instr: "You are an SDR. Research the lead from what the systems hold, find one relevant hook, and draft a concise first-touch email. Never invent facts about the prospect.", tools: [], goal: "Get 70% of drafted emails approved by reps without edits." },
  "Coding agent": { icon: "ti-code", color: "rose", name: "Bug Reproducer", desc: "Reads repos, reproduces bugs, proposes fixes.", instr: "You are a coding agent. Reproduce the reported bug from the repository, find the root cause, and propose a minimal fix with a clear description.", tools: ["read_document"], goal: "Reproduce 60% of reported bugs with logs attached." , steps: 60 },
  "Gap hunter": { icon: "ti-target-arrow", color: "teal", name: "Gap Hunter", desc: "Works the gap backlog on a schedule: answers what it can from knowledge and closes it.", instr: "You work the platform's gap backlog — questions agents could not answer, filed from their runs. Every run, with no one telling you what to do:\n1. Read the backlog once with gaps.work_order. Never read it again in the same run.\n2. Skip every gap that needs a tool, a connector or a person's decision: it is not yours to close.\n3. For each gap left, try once: one search_knowledge with the gap's own words, one knowledge.read if a hit looks like the answer, one memory.recall. If that does not answer it, it is not answerable from what you have now — move on. Do not search again in other words.\n4. When a gap is answered, write the answer in full with the source you cited, then close it with gaps.resolve and its gap_id.\n5. End with a short report: which gaps you closed, which you left and why. If none could be answered, say so in one line.", tools: ["gaps.work_order", "gaps.resolve", "search_knowledge", "knowledge.read", "memory.recall"], goal: "Close half the answerable gaps within a week of their filing.", schedule: { interval_secs: 86400, every: "every day", max_runs: 30, max_tokens: 600000, input: "Work the gap backlog." } },
  "Research assistant": { icon: "ti-search", color: "blue", name: "Research Assistant", desc: "Searches sources and writes cited briefs.", instr: "You are a research assistant. Read the sources available to you, cross-check claims, and produce a concise brief where every claim carries a citation.", tools: ["search_knowledge", "read_document"], goal: "Deliver briefs where 95% of claims carry a checkable citation." , steps: 60 },
};
const TPL_ORDER = Object.keys(TEMPLATES);
const GOAL_METRICS = ["Outcome verified", "Resolved without a pause", "Runs that finished"];

interface Wz { tpl: string; name: string; handle: string; desc: string; color: string; icon: string; instr: string; model: string; tools: { name: string; when?: string }[]; goal: string; metric: string; target: number; /** The cadence the template ships with, when the builder keeps it. */ schedule?: TplSchedule; keepSchedule?: boolean; /** Reasoning steps a run may take before it pauses for a person to carry on. */ steps: number }

export function CreateWizard() {
  const { closeWizard, open, toast } = useOverlay();
  const navigate = useNavigate();
  const [step, setStep] = useState(0);
  const [wz, setWz] = useState<Wz>(() => fromTemplate("Support agent"));
  const [providers, setProviders] = useState<LlmProviders | null>(null);
  const [catalog, setCatalog] = useState<ServerTool[]>([]);
  const [instances, setInstances] = useState<ConnectorInstance[]>([]);
  const [busy, setBusy] = useState(false);
  // Read again whenever the model step opens: a provider changed under an
  // open wizard left its list stale.
  useEffect(() => { if (step === 4) llmProviders().then((p) => { setProviders(p); setWz((w) => ({ ...w, model: w.model && p.providers.some((x) => x.id === w.model) ? w.model : p.primary || "" })); }).catch(() => {}); }, [step]);
  useEffect(() => { llmProviders().then((p) => { setProviders(p); setWz((w) => ({ ...w, model: w.model || p.primary || "" })); }).catch(() => {}); serverInfo().then((i) => setCatalog(i.graphs.flatMap((g) => g.tools))).catch(() => {}); listConnectorInstances().then(setInstances).catch(() => {}); }, []);
  const set = (patch: Partial<Wz>) => setWz((w) => ({ ...w, ...patch }));
  const unit = wz.metric === "Cost per run" ? "¢" : "%";
  /** The starter set: the template's tools and any picked from the catalog, as the catalog describes them. */
  const starter = useMemo(() => { const base = catalog.filter((t) => !t.name.includes(".")); const chosen = wz.tools.map((t) => catalog.find((c) => c.name === t.name) ?? { name: t.name, description: "", effect: "read_only" }); return [...base, ...chosen.filter((c) => !base.some((b) => b.name === c.name))]; }, [catalog, wz.tools]);
  const has = (n: string) => wz.tools.some((t) => t.name === n);
  const toggle = (n: string) => set({ tools: has(n) ? wz.tools.filter((t) => t.name !== n) : [...wz.tools, { name: n }] });

  async function finish() {
    setBusy(true);
    try {
      const made = await createAssistant({
        name: wz.name.trim() || "New agent", graph: "react_agent",
        metadata: { description: wz.desc.trim(), studio: { color: wz.color, icon: wz.icon, ...(wz.goal.trim() ? { goal: { objective: wz.goal.trim(), metric: wz.metric, target: Number(wz.target) } } : {}) } } as never,
        // The builder's step limit from the first save, so a new agent never
        // silently takes the graph's own ceiling.
        config: { studio_intent: { instructions: wz.instr, tools: wz.tools, ...(wz.model && wz.model !== providers?.primary ? { model: wz.model } : {}) }, recursion_limit: wz.steps },
      });
      // The template's cadence, capped from day one: the agent works on its own.
      if (wz.schedule && wz.keepSchedule) {
        await createSchedule({ assistant_id: made.assistant_id, interval_secs: wz.schedule.interval_secs, input: { messages: [{ role: "user", content: wz.schedule.input }] }, max_runs: wz.schedule.max_runs, max_tokens: wz.schedule.max_tokens });
      }
      await useServer.getState().refresh();
      closeWizard(); toast(`Agent "${made.name}" created${wz.schedule && wz.keepSchedule ? ` — it runs ${wz.schedule.every}` : ""}`, "ti-robot");
      navigate({ to: "/agents/$id", params: { id: made.assistant_id } });
    } catch (err) { toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle"); }
    finally { setBusy(false); }
  }

  return (
    <div className="wz">
      <div className="wz-rail">
        <div className="wz-brand"><div className="bm"><i className="ti ti-robot" style={{ fontSize: 18 }} /></div><div><div className="bt">New agent</div><div className="bs">Rusty</div></div></div>
        <div className="wz-steps">{STEPS.map((s, i) => <div key={s} className={`wz-step${i === step ? " active" : ""}${i < step ? " done" : ""}`} data-step={i} onClick={() => { if (i <= step) setStep(i); }}><div className="sn">{i < step ? <i className="ti ti-check" /> : i + 1}</div><div><div className="sl">{s}</div></div></div>)}</div>
        <div className="wz-rail-foot">Step <span data-curstep>{step + 1}</span> of {STEPS.length}</div>
      </div>
      <div className="wz-main">
        <div className="wz-scroll"><div className="wz-body" data-wzbody>
          {step === 0 && (
            <>
              <div className="wz-eyebrow">Step 1 · Start</div><h2 className="wz-h">Pick a starting point</h2><p className="wz-sub">Templates pre-fill instructions, tools and a look. You can change everything later.</p>
              <div className="tmpl-grid">{TPL_ORDER.map((n) => { const t = TEMPLATES[n]; return <div key={n} className={`tmpl-card${n === wz.tpl ? " sel" : ""}`} data-tpl={n} onClick={() => setWz((w) => (w.tpl === n ? w : { ...fromTemplate(n), model: w.model }))}><div className="tmpl-ic" style={{ background: COLOR_BG[t.color], color: COLORS[t.color] }}><i className={`ti ${t.icon}`} /></div><div className="tmpl-name">{n}</div><div className="tmpl-desc">{t.desc}</div></div>; })}</div>
            </>
          )}
          {step === 1 && (
            <>
              <div className="wz-eyebrow">Step 2 · Identity</div><h2 className="wz-h">Name your agent</h2><p className="wz-sub">This is how it shows up across the workspace.</p>
              <div style={{ display: "flex", gap: 20, alignItems: "flex-start", marginBottom: 24 }}>
                <div className="wz-preview-tile" data-prev-tile style={{ background: COLOR_BG[wz.color], color: COLORS[wz.color] }}><i className={`ti ${wz.icon}`} /></div>
                <div style={{ flex: 1 }}><div className="fld"><label className="fld-label">Name</label><input className="m-input" data-name value={wz.name} onChange={(e) => set({ name: e.target.value, handle: slug(e.target.value) })} /></div><div className="fld" style={{ marginBottom: 0 }}><label className="fld-label">Handle</label><div className="m-input-group"><span style={{ color: "var(--ink-400)" }}>@</span><input data-handle value={wz.handle} onChange={(e) => set({ handle: slug(e.target.value) })} /></div></div></div>
              </div>
              <div className="fld"><label className="fld-label">Description</label><textarea className="m-textarea" data-desc placeholder="One line on what this agent does" value={wz.desc} onChange={(e) => set({ desc: e.target.value })} /></div>
              <div className="fld"><label className="fld-label">Color</label><div className="swatch-row">{Object.keys(COLORS).map((c) => <div key={c} className={`swatch-pick${c === wz.color ? " sel" : ""}`} data-color={c} onClick={() => set({ color: c })}><div className={`swatch${c === wz.color ? " sel" : ""}`} style={{ background: COLORS[c] }}>{c === wz.color && <i className="ti ti-check" style={{ color: "var(--on-brand, #fff)", fontSize: 16 }} />}</div><span className="swatch-name">{c}</span></div>)}</div>
                <div className="m-hint">The theme keeps every category in stone; the name is what tells them apart across the workspace.</div></div>
              <div className="fld"><label className="fld-label">Icon</label><div className="icon-row">{ICONS.map((ic) => <div key={ic} className={`icon-pick${ic === wz.icon ? " sel" : ""}`} data-icon={ic} onClick={() => set({ icon: ic })}><i className={`ti ${ic}`} /></div>)}</div></div>
            </>
          )}
          {step === 2 && (
            <>
              <div className="wz-eyebrow">Step 3 · Goal</div><h2 className="wz-h">What does success look like?</h2><p className="wz-sub">The goal is shown on the agent, measured on its runs, and shown against every publish.</p>
              <div className="fld"><label className="fld-label">Objective</label><textarea className="m-textarea" data-goal placeholder="One sentence a teammate could verify" value={wz.goal} onChange={(e) => set({ goal: e.target.value })} /></div>
              <div className="frow two"><div className="fld"><label className="fld-label">Primary metric</label><select className="m-input" data-metric value={wz.metric} onChange={(e) => set({ metric: e.target.value, target: 70 })}>{GOAL_METRICS.map((m) => <option key={m}>{m}</option>)}</select></div><div className="fld"><label className="fld-label">Target</label><div className="m-input-group"><input data-target type="number" value={wz.target} onChange={(e) => set({ target: Number(e.target.value) })} /><span style={{ color: "var(--ink-400)" }}>{unit}</span></div></div></div>
              <div className="m-alert info"><div className="a-ic"><i className="ti ti-info-circle" /></div><div className="a-body"><div className="a-title">Measured two ways</div><div className="a-text">Live: on the agent's runs over a rolling seven days, from the server's verdicts. On publish: the agent's suites gate every activation.</div></div></div>
            </>
          )}
          {step === 3 && (
            <>
              <div className="wz-eyebrow">Step 4 · Instructions</div><h2 className="wz-h">How should it behave?</h2><p className="wz-sub">The system prompt. Use {"{{variables}}"} for anything that changes per run.</p>
              <textarea className="instr-editor" data-instr spellCheck={false} style={{ minHeight: 220, width: "100%", resize: "vertical" }} value={wz.instr} onChange={(e) => set({ instr: e.target.value })} />
              <div className="instr-foot" style={{ marginTop: 12 }}><span className="tk"><i className="ti ti-square-rounded-letter-t" style={{ fontSize: 13, verticalAlign: -2 }} /> <span data-tok>~{Math.max(20, Math.round(wz.instr.length / 4))}</span> tokens</span></div>
            </>
          )}
          {step === 4 && (
            <>
              <div className="wz-eyebrow">Step 5 · Capabilities</div><h2 className="wz-h">Model &amp; starter tools</h2><p className="wz-sub">Choose the reasoning engine and the tools it can call. Add more anytime.</p>
              <div className="fld"><label className="fld-label">Model</label>
                {(providers?.providers ?? []).map((p) => <div key={p.id} className={`mdl-row${p.id === wz.model ? " sel" : ""}`} data-model={p.id} onClick={() => set({ model: p.id })}><div className="mdl-logo" style={{ background: "var(--accent)" }}><i className="ti ti-sparkles" /></div><div className="mdl-body"><div className="mdl-name" title={p.model}>{p.name}</div><div className="mdl-meta">{shortModel(p.model)}{p.id === providers?.primary ? " · the workspace default" : ""}</div></div>{p.id === wz.model && <i className="ti ti-check mdl-check" />}</div>)}
                {providers && providers.providers.length === 0 && <div className="m-hint">No provider configured — add one under AI models.</div>}
              </div>
              <div className="fld"><label className="fld-label">Tools <span className="opt">— {wz.tools.length} selected</span></label>
                {starter.map((t) => { const risk = riskOf(t.effect); return <div key={t.name} className="wz-tool"><div className="wt-ic" style={{ background: risk === "read" ? "var(--cat-blue-bg)" : "var(--cat-rose-bg)", color: risk === "read" ? "var(--cat-blue)" : "var(--cat-rose)" }}><i className={`ti ${toolIcon(t.name, t.effect)}`} /></div><div className="wt-b"><div className="wt-n" title={t.name}>{plainName(t.name)}</div><div className="wt-d" title={t.description}>{firstSentence(t.description)}</div></div><div className={`m-switch${has(t.name) ? " on" : ""}`} data-tool={t.name} onClick={() => toggle(t.name)} /></div>; })}
                <div className="add-row" data-wz-more onClick={() => open("drawer", <AddToolsDrawer catalog={catalog} instances={instances} have={wz.tools.map((t) => t.name)} onAdd={(added) => setWz((w) => ({ ...w, tools: [...w.tools, ...added.filter((a) => !w.tools.some((t) => t.name === a.name))] }))} />)}><i className="ti ti-plus" /> Browse the tool catalog</div>
              </div>
            </>
          )}
          {step === 5 && (
            <>
              <div className="wz-eyebrow">Step 6 · Review</div><h2 className="wz-h">Ready to create</h2><p className="wz-sub">Confirm the setup. You'll land in the builder to fine-tune.</p>
              <div className="rev-grid">
                <div className="rev-row"><div className="rev-k"><i className="ti ti-robot" style={{ fontSize: 15 }} /> Agent</div><div className="rev-v" style={{ display: "flex", alignItems: "center", gap: 11 }}><div className="ag-tile-sm" style={{ width: 30, height: 30, background: COLOR_BG[wz.color], color: COLORS[wz.color] }}><i className={`ti ${wz.icon}`} /></div><div><b>{wz.name}</b> <span className="mono" style={{ color: "var(--ink-500)" }}>@{wz.handle}</span><div style={{ color: "var(--ink-600)", fontWeight: 400 }}>{wz.desc}</div></div></div></div>
                {wz.schedule && <div className="rev-row" data-rev-schedule><div className="rev-k"><i className="ti ti-calendar-repeat" style={{ fontSize: 15 }} /> Schedule</div><div className="rev-v"><label style={{ display: "flex", gap: 8, alignItems: "center", cursor: "pointer" }}><input type="checkbox" checked={!!wz.keepSchedule} onChange={(e) => set({ keepSchedule: e.target.checked })} data-flow="keep-schedule" />Runs {wz.schedule.every} on its own, up to {wz.schedule.max_runs} times</label><div style={{ color: "var(--ink-600)", fontSize: "var(--fs-xs)", marginTop: 4 }}>Change or remove it on the agent's Triggers card.</div></div></div>}
                <div className="rev-row"><div className="rev-k"><i className="ti ti-target-arrow" style={{ fontSize: 15 }} /> Goal</div><div className="rev-v">{wz.goal.trim() ? <>{wz.goal}<div style={{ color: "var(--ink-600)", fontSize: "var(--fs-xs)", marginTop: 4 }}>{wz.metric} ≥ {wz.target}{unit}</div></> : <span style={{ color: "var(--ink-400)" }}>No goal set</span>}</div></div>
                <div className="rev-row"><div className="rev-k"><i className="ti ti-cpu" style={{ fontSize: 15 }} /> Model</div><div className="rev-v" title={providers?.providers.find((p) => p.id === (wz.model || providers?.primary))?.model}>{providers?.providers.find((p) => p.id === wz.model)?.name ?? `${providers?.providers.find((p) => p.id === providers?.primary)?.name ?? "The workspace model"} (the default)`}</div></div>
                <div className="rev-row"><div className="rev-k"><i className="ti ti-tool" style={{ fontSize: 15 }} /> Tools</div><div className="rev-v">{wz.tools.length ? wz.tools.map((t) => <span key={t.name} className="m-chip" style={{ margin: "0 6px 6px 0" }} title={t.name}>{plainName(t.name)}</span>) : <span style={{ color: "var(--ink-400)" }}>None yet</span>}</div></div>
                <div className="rev-row"><div className="rev-k"><i className="ti ti-file-text" style={{ fontSize: 15 }} /> Instructions</div><div className="rev-v" style={{ color: "var(--ink-600)", fontSize: "var(--fs-sm)", lineHeight: 1.6, maxHeight: 96, overflow: "hidden", whiteSpace: "pre-wrap" }}>{wz.instr.slice(0, 280)}{wz.instr.length > 280 ? "…" : ""}</div></div>
              </div>
            </>
          )}
        </div></div>
        <div className="wz-foot">
          <button className="m-btn ghost" data-back style={{ visibility: step === 0 ? "hidden" : "visible" }} onClick={() => setStep((s) => Math.max(0, s - 1))}><i className="ti ti-arrow-left" /> Back</button>
          <span className="sp" /><span className="prog" data-prog>Step {step + 1} / {STEPS.length}</span>
          <button className={`m-btn primary${step === STEPS.length - 1 ? " accent" : ""}`} data-next disabled={busy || (step === 1 && !wz.name.trim())} onClick={() => (step < STEPS.length - 1 ? setStep((s) => s + 1) : void finish())}>{step === STEPS.length - 1 ? <><i className="ti ti-rocket" /> Create agent</> : <>Continue <i className="ti ti-arrow-right" /></>}</button>
        </div>
      </div>
      <div className="wz-close" data-wzclose onClick={closeWizard}><i className="ti ti-x" /></div>
    </div>
  );
}

function fromTemplate(n: string): Wz {
  const t = TEMPLATES[n];
  return { tpl: n, name: t.name, handle: slug(t.name), desc: t.desc, color: t.color, icon: t.icon, instr: t.instr, model: "", tools: t.tools.map((name) => ({ name })), goal: t.goal, metric: GOAL_METRICS[0], target: 70, steps: t.steps ?? DEFAULT_STEPS, ...(t.schedule ? { schedule: t.schedule, keepSchedule: true } : {}) };
}

export type { Assistant };
