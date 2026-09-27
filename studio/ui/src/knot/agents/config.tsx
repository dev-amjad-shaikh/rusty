import { useEffect, useMemo, useState, type ReactNode } from "react";
import { useNavigate } from "@tanstack/react-router";
import { useServer } from "../../engine/net/server";
import { compileKnowledgeUnit, knowledgeUnit, removeKnowledgeUnit, type KnowledgeUnit, PROVENANCE_LABEL, createThread, runAndWait, deleteSchedule, deleteWebhook, listKnowledgeSources, listStandingApprovals, listWebhookEvents, webhookUrl, withdrawStandingApproval, listConnectorInstances, listDatasets, listSchedules, listSkills, listWebhooks, llmProviders, serverInfo, type Assistant, type ConnectorInstance, type KnowledgeSourceSummary, type StandingApproval, type WebhookEvent, type DatasetVersion, type LlmProviders, type Schedule, type ServerSkill, type ServerTool, type Webhook } from "../../engine/net/client";
import { cadenceWords } from "../../engine/forms/agentSpec";
import { Badge, OvHead, Switch, openMenu, useOverlay } from "../overlay";
import { connectorOf, riskOf, tokensOf, toolIcon, variablesIn } from "../data";
import type { AgentVariable } from "../../engine/net/client";
import { ImprovementsBlock } from "./improvements";
import { GoalCard, goalOf } from "./goal";
import { VarTextarea } from "./VarTextarea";
import { MemoryDrawer } from "./memoryDrawer";
import { firstSentence, plainName, shortModel } from "./words";
import { DeclaredBlocks } from "./declaredBlocks";
import { AddSourceDrawer, QueryModal } from "../views/Knowledge";
import type { Proposals } from "./proposals";
import { DEFAULT_STEPS, type AgentDraft } from "./useAgent";
import { AddConnectorDrawer, AddToolsDrawer, EvalGoalModal, ModelPickerModal, SkillEditorDrawer, SkillPickerDrawer, TriggerModal, VariableModal } from "../flows/agentFlows";

type Edit = (change: (d: AgentDraft) => AgentDraft) => void;

/** A `.m-card.block`: the head with a tile, titles and actions, a body that folds. */
function Block({ section, icon, tileStyle, title, count, sub, actions, collapsed: startCollapsed, children }: { section: string; icon: string; tileStyle?: React.CSSProperties; title: ReactNode; count?: number; sub: ReactNode; actions?: ReactNode; collapsed?: boolean; children: ReactNode }) {
  const [collapsed, setCollapsed] = useState(!!startCollapsed);
  return (
    <div className={`m-card block${collapsed ? " collapsed" : ""}`} data-block data-section={section}>
      <div className="block-head" onClick={(e) => { if ((e.target as HTMLElement).closest(".head-act button, .m-btn")) return; setCollapsed((c) => !c); }}>
        <div className="block-ic" style={tileStyle}><i className={`ti ${icon}`} /></div>
        <div className="block-titles">
          <div className="block-title">{title}{count !== undefined && <span className="cnt">{count}</span>}</div>
          <div className="block-sub">{sub}</div>
        </div>
        <div className="head-act">{actions}<i className="ti ti-chevron-down chev" /></div>
      </div>
      <div className="block-body">{children}</div>
    </div>
  );
}

/** The config column: every block on the real agent. */
/** The when-to-use hint a skill lends the tools it brings: the skill's own "use for" sentence, so the shortlist ranks a connector's operation by the case it serves. The platform's shared tools (memory, skills, gaps, tasks…) keep their own descriptions — a hint from one skill would misdescribe them for the others. */
function skillToolHint(skill: { name: string; description: string }, tool: string): string | undefined {
  const family = tool.split(".")[0];
  if (!tool.includes(".") || ["memory", "skills", "gaps", "tasks", "agents", "artifacts", "knowledge", "catalog", "web", "connectors", "assignment"].includes(family)) return undefined;
  const first = (skill.description.match(/^[^.!?]{12,240}[.!?]?/) || [skill.description.slice(0, 200)])[0].trim();
  return `for ${skill.name}: ${first}`;
}

export function ConfigColumn({ agent, draft, edit, status, proposals }: { agent: Assistant; draft: AgentDraft; edit: Edit; status: "draft" | "published"; proposals: Proposals }) {
  const navigate = useNavigate();
  const { open, toast } = useOverlay();
  const intent = draft.intent;

  // What the server holds, read once per agent.
  const [providers, setProviders] = useState<LlmProviders | null>(null);
  const [catalog, setCatalog] = useState<ServerTool[]>([]);
  // A skill's tools, and the model's finer settings, shown on demand.
  const [advanced, setAdvanced] = useState(false);
  const [addPin, setAddPin] = useState(false);
  const [openSkill, setOpenSkill] = useState<string | null>(null);
  const [context, setContext] = useState<{ budget_tokens: number; keep_recent_messages?: number | null; memory: boolean } | null>(null);
  const [skills, setSkills] = useState<ServerSkill[]>([]);
  const [instances, setInstances] = useState<ConnectorInstance[]>([]);
  const [schedules, setSchedules] = useState<Schedule[]>([]);
  const [webhooks, setWebhooks] = useState<Webhook[]>([]);
  const [datasets, setDatasets] = useState<DatasetVersion[]>([]);
  const [standing, setStanding] = useState<StandingApproval[]>([]);
  const [knowledge, setKnowledge] = useState<KnowledgeSourceSummary[]>([]);
  const assistantsAll = useServer((s) => s.assistants);
  const [improving, setImproving] = useState(false);
  const [suggestion, setSuggestion] = useState<{ text: string; addition: string } | null>(null);
  /** The Coach reads the charter and answers with one addition; nothing is filed — Apply edits the draft. */
  async function improve() {
    const coach = assistantsAll.find((a) => a.name === "Coach" && !a.archived_at);
    if (!coach) { toast("There is no Coach on this server to ask", "ti-alert-triangle"); return; }
    setImproving(true); setSuggestion(null);
    try {
      const thread = await createThread(coach.graph);
      const r = await runAndWait(thread.thread_id, coach.assistant_id, [{ role: "user", content: `Read this charter for the agent ${draft.name} and propose ONE addition that would make its replies more grounded or its handoffs clearer. Do not file a version and do not call agents.revise. Answer in exactly two parts separated by a line "---": first, one sentence on why; second, the exact text to add to the charter.\n\nCHARTER:\n${charter}` }]);
      const reply = [...(r.output?.messages ?? [])].reverse().find((m) => m.role === "assistant" && m.content && !(m.tool_calls?.length))?.content ?? "";
      const [why, ...rest] = reply.split(/\n-{3,}\n/);
      const addition = rest.join("\n").trim() || why.trim();
      if (!addition) { toast("The Coach had nothing to add", "ti-wand"); return; }
      setSuggestion({ text: rest.length ? why.trim() : "Suggested addition", addition });
    } catch (err) { toast(err instanceof Error ? err.message : "the Coach could not be reached", "ti-alert-triangle"); }
    finally { setImproving(false); }
  }
  const [unit, setUnit] = useState<Awaited<ReturnType<typeof knowledgeUnit>> | null>(null);
  const reloadKnowledge = () => { void listKnowledgeSources().then((r) => setKnowledge(r.sources.filter((k) => k.scope.scope === "tenant" || (k.scope.scope === "agent" && k.scope.id === agent.assistant_id)))).catch(() => {}); void knowledgeUnit(agent.assistant_id).then(setUnit).catch(() => setUnit(null)); };
  const reload = () => {
    void listSchedules().then(setSchedules).catch(() => {});
    void listWebhooks().then(setWebhooks).catch(() => {});
    void listConnectorInstances().then(setInstances).catch(() => {});
    void listDatasets().then((d) => setDatasets(d)).catch(() => {});
    reloadKnowledge();
    void listStandingApprovals().then((l) => setStanding(l.filter((x) => !x.assistant_id || x.assistant_id === agent.assistant_id))).catch(() => {});
  };
  useEffect(() => {
    void llmProviders().then(setProviders).catch(() => setProviders(null));
    void serverInfo().then((i) => { setCatalog(i.graphs.find((g) => g.name === agent.graph)?.tools ?? []); setContext(i.context ?? null); }).catch(() => {});
    void listSkills().then(setSkills).catch(() => {});
    reload();
  }, [agent.assistant_id]); // eslint-disable-line react-hooks/exhaustive-deps

  const tools = intent.tools ?? [];
  const toolNames = tools.map((t) => t.name);
  const hasSearchTool = toolNames.includes("search_knowledge");
  const catalogByName = useMemo(() => new Map(catalog.map((t) => [t.name, t])), [catalog]);
  const [off, setOff] = useState<Set<string>>(new Set());
  const setTools = (next: { name: string; when?: string }[]) => edit((d) => ({ ...d, intent: { ...d.intent, tools: next } }));
  // A drawer stays open across several adds, so it must not close over the
  // list it saw when it opened — each add reads the working copy as it is now.
  const addTools = (added: { name: string; when?: string }[]) =>
    edit((d) => { const have = new Set((d.intent.tools ?? []).map((t) => t.name)); return { ...d, intent: { ...d.intent, tools: [...(d.intent.tools ?? []), ...added.filter((a) => !have.has(a.name))] } }; });

  const mySkills = (intent.skills ?? []).map((name) => skills.find((s) => s.name === name) ?? { name, description: "", revision: 0, content_hash: "", allowed_tools: [] as string[] });
  const myConnectors = instances.filter((i) => (i.tools ?? []).some((t) => toolNames.includes(t)) || (i.agents ?? []).some((a) => a.assistant_id === agent.assistant_id));
  const mySchedules = schedules.filter((s) => s.assistant_id === agent.assistant_id);
  const myWebhooks = webhooks.filter((w) => w.target.kind === "assistant" && w.target.id === agent.assistant_id);
  const myDatasets = datasets.filter((d) => d.agent_id === agent.assistant_id);
  // The agent may name a provider by id or by model; otherwise it runs on the deployment's primary.
  const named = intent.model ? providers?.providers.find((p) => p.id === intent.model || p.model === intent.model) ?? null : null;
  const primary = named ?? providers?.providers.find((p) => p.id === providers.primary) ?? null;
  // Its own fallback when it names one; the deployment's otherwise.
  const ownFallback = intent.fallback_model ? providers?.providers.find((p) => p.id === intent.fallback_model) ?? null : null;
  const fallback = ownFallback ?? providers?.providers.find((p) => p.id === providers.fallback) ?? null;
  const charter = intent.instructions ?? "";
  // Variables: the ones declared (with a source and a test value), and the
  // ones the charter or a skill names without a declaration yet.
  const declared = intent.variables ?? [];
  const skillVars = useMemo(() => [...new Set(skills.filter((k) => (intent.skills ?? []).includes(k.name)).flatMap((k) => variablesIn(k.description)))], [skills, intent.skills]);
  const used = [...new Set([...variablesIn(charter), ...skillVars])];
  const vars = [...new Set([...declared.map((v) => v.name), ...used])];
  const undeclared = used.filter((n) => !declared.some((v) => v.name === n));
  const setVariables = (fn: (list: AgentVariable[]) => AgentVariable[]) => edit((d) => ({ ...d, intent: { ...d.intent, variables: fn(d.intent.variables ?? []) } }));
  const upsertVariable = (v: AgentVariable) => setVariables((list) => list.some((x) => x.name === v.name) ? list.map((x) => (x.name === v.name ? v : x)) : [...list, v]);
  const addVariable = (initial?: Partial<AgentVariable>) => open("modal", <VariableModal initial={initial} taken={declared.map((v) => v.name)} onSave={(v) => { upsertVariable(v); toast(`{{${v.name}}} ${initial?.name ? "saved" : "added — use it in the prompt"}`, "ti-variable"); }} />);
  const [instrTab, setInstrTab] = useState<"edit" | "vars">("edit");
  const budget = intent.budget ?? {};
  const bounded = !!(budget.max_tokens || budget.max_cost_usd);

  // Readiness: what the agent has, from what the server holds.
  const checks: [string, string, boolean][] = [
    ["Identity", "add a name and description", draft.name.trim().length > 0 && draft.description.trim().length > 0],
    ["Goal", "set a goal", goalOf(draft) !== null],
    ["Instructions", "write instructions", charter.trim().length > 40],
    ["Tools", "enable at least one tool", toolNames.some((n) => !off.has(n))],
    ["Skills", "compose a skill", (intent.skills ?? []).length > 0],
    ["Connectors healthy", "reauthorize a connector", !myConnectors.some((c) => c.authorization && (c.authorization.kind === "needs_auth" || c.authorization.kind === "expired"))],
    ["Triggers", "add a trigger", mySchedules.length + myWebhooks.filter((w) => w.enabled).length > 0],
    ["Evaluation goal", "add an evaluation goal", myDatasets.length > 0],
    ["Deployment", "publish", status === "published"],
  ];
  const ok = checks.filter((c) => c[2]).length;
  const miss = checks.filter((c) => !c[2]).map((c) => c[1]);
  const missText = miss.length ? (miss.length === 1 ? miss[0] : `${miss.slice(0, -1).join(", ")} and ${miss[miss.length - 1]}`) : "";

  return (
    <>
      <GoalCard agent={agent} draft={draft} edit={edit} />

      <div className="readiness" id="readiness" data-flow="readiness" onClick={() => open("modal", <ReadinessModal checks={checks} />)}>
        <div className="block-ic" style={{ background: "var(--accent-bg)", color: "var(--accent)", width: 34, height: 34 }}><i className="ti ti-progress-check" /></div>
        <div className="rl">
          <div className="rt">Agent readiness — {ok} of {checks.length} steps</div>
          <div className="rs">{miss.length ? `${missText[0].toUpperCase()}${missText.slice(1)} to publish.` : "Everything is in place. Publish when ready."}</div>
        </div>
        <div style={{ width: 120 }}><div className="m-progress accent"><span className="bar" style={{ width: `${Math.round((ok / checks.length) * 100)}%` }} /></div></div>
        <i className="ti ti-chevron-right" style={{ color: "var(--ink-400)" }} />
      </div>

      {/* ① MODEL & BEHAVIOR */}
      <ImprovementsBlock proposals={proposals} policy={intent.promotion ?? "person"} setPolicy={(p) => edit((d) => ({ ...d, intent: { ...d.intent, promotion: p } }))} gate={intent.gate} setGate={(g) => edit((d) => ({ ...d, intent: { ...d.intent, gate: g } }))} />

      <Block section="model" icon="ti-cpu" tileStyle={{ background: "var(--brand-soft)", color: "var(--ink-800)" }} title="Model & behavior" sub="Which AI model it thinks with">
        <div className="frow two">
          <div>
            <div className="f-mini-label">Model</div>
            <div className="model-pick" data-flow="model" data-slot="primary" onClick={() => providers && open("modal", <ModelPickerModal slot="primary" providers={providers} current={intent.model ?? providers.primary ?? null} own={!!intent.model} onPick={(id) => { edit((d) => ({ ...d, intent: { ...d.intent, model: id ?? undefined } })); toast(id ? `${providers.providers.find((p) => p.id === id)?.name ?? id} selected` : "Follows the deployment default", "ti-cpu"); }} />)}>
              <div className="model-logo" style={{ background: "var(--accent)" }}><i className="ti ti-sparkles" /></div>
              <div title={primary?.model}><div className="mn">{primary ? primary.name : providers ? "No model yet" : "Reading…"}</div><div className="mp">{primary ? `${shortModel(primary.model)} · ${named ? "chosen for this agent" : "the workspace default"}` : providers ? "add one under AI models" : ""}</div></div>
              <i className="ti ti-selector" style={{ marginLeft: "auto", color: "var(--ink-400)" }} />
            </div>
          </div>
          <div>
            <div className="f-mini-label">If it does not answer</div>
            <div className="model-pick" data-flow="model" data-slot="fallback" onClick={() => providers && open("modal", <ModelPickerModal slot="fallback" providers={providers} current={intent.fallback_model ?? providers.fallback ?? null} own={!!intent.fallback_model} onPick={(id) => { edit((d) => ({ ...d, intent: { ...d.intent, fallback_model: id ?? undefined } })); toast(id ? `${providers.providers.find((p) => p.id === id)?.name ?? id} is this agent's fallback` : "Follows the deployment's fallback", "ti-circle-dashed"); }} />)}>
              <div className="model-logo" style={{ background: "var(--cat-blue)" }}><i className="ti ti-circle-dashed" /></div>
              <div title={fallback?.model}><div className="mn">{fallback ? fallback.name : providers ? "None" : "Reading…"}</div><div className="mp">{fallback ? `${shortModel(fallback.model)} · ${ownFallback ? "chosen for this agent" : "the workspace default"}` : providers ? "pick one if the first may not answer" : ""}</div></div>
              <i className="ti ti-selector" style={{ marginLeft: "auto", color: "var(--ink-400)" }} />
            </div>
          </div>
        </div>
        <button className="m-btn ghost sm" style={{ marginTop: 12 }} data-flow="model-advanced" onClick={() => setAdvanced((v) => !v)}>{advanced ? "Hide advanced settings" : "Advanced settings"}</button>
        {advanced && <div className="frow two" style={{ marginTop: 12 }}>
          <div>
            <div className="f-mini-label">How much it keeps in view <span className="item-tag" style={{ marginLeft: "auto" }}>{context ? `${context.budget_tokens.toLocaleString()} tokens` : "raw"}</span></div>
            <div className="param"><input type="range" className="m-slider" min={4000} max={200000} step={1000} value={intent.context?.budget_tokens ?? context?.budget_tokens ?? 32000} onChange={(e) => edit((d) => ({ ...d, intent: { ...d.intent, context: { ...(d.intent.context ?? {}), budget_tokens: Number(e.target.value) } } }))} /><span className="pv">{(Number(intent.context?.budget_tokens ?? context?.budget_tokens ?? 32000) / 1000).toFixed(0)}k</span></div>
          </div>
          <div>
            <div className="f-mini-label">Steps before it pauses for you</div>
            <div className="param"><input type="range" className="m-slider" min={1} max={200} value={draft.recursion_limit ?? DEFAULT_STEPS} onChange={(e) => edit((d) => ({ ...d, recursion_limit: Number(e.target.value) }))} /><span className="pv">{draft.recursion_limit ?? DEFAULT_STEPS}</span></div>
            <div className="f-mini-label">Spend limit for work it hands to other agents</div>
            <div className="param"><input type="number" className="m-input" min={0} step={10000} style={{ width: 140 }} value={intent.chain_max_tokens ?? 400000} onChange={(e) => edit((d) => { const v = e.target.value.trim(); const { chain_max_tokens: _c, ...rest } = d.intent; return { ...d, intent: v === "" ? rest : { ...rest, chain_max_tokens: Math.max(0, Number(v) || 0) } }; })} /><span className="pv">tokens · empty uses the workspace's limit</span></div>
            <div className="f-mini-label">Creativity</div>
            <div className="param"><input type="number" className="m-input" min={0} max={2} step={0.1} style={{ width: 100 }} placeholder="default" data-flow="temperature" value={intent.temperature ?? ""} onChange={(e) => edit((d) => { const v = e.target.value.trim(); const { temperature: _t, ...rest } = d.intent; const n = Number(v); return { ...d, intent: v === "" || !Number.isFinite(n) ? rest : { ...rest, temperature: Math.min(2, Math.max(0, n)) } }; })} /><span className="pv">0 steady – 2 varied · empty uses the model's default</span></div>
          </div>
        </div>}
      </Block>

      {/* ② INSTRUCTIONS */}
      <Block section="instructions" icon="ti-file-text" tileStyle={{ background: "var(--cat-blue-bg)", color: "var(--cat-blue)" }} title="Instructions" sub="The system prompt — who the agent is and how it works">
        <div className="instr-tabs">
          <div className="m-seg">
            <button className={instrTab === "edit" ? "on" : ""} data-instr="edit" onClick={() => setInstrTab("edit")}>Edit</button>
            <button className={instrTab === "vars" ? "on" : ""} data-instr="vars" onClick={() => setInstrTab("vars")}>Variables</button>
          </div>
          <button className="m-btn ghost sm" style={{ marginLeft: "auto" }} data-flow="improve" onClick={() => void improve()}><i className="ti ti-wand" /> Improve with AI</button>
        </div>
        <div hidden={instrTab !== "edit"}>
          <VarTextarea className="instr-editor" spellCheck={false} value={charter} names={vars} onValueChange={(v) => edit((d) => ({ ...d, intent: { ...d.intent, instructions: v } }))} style={{ width: "100%", resize: "vertical", border: "none", outline: "none", background: "transparent", font: "inherit", color: "inherit", minHeight: 200 }} placeholder="You are …" />
          {improving && <div className="ai-suggest"><span className="m-spin" style={{ width: 18, height: 18, borderWidth: 2 }} /><div className="as-body"><div className="as-title">Improving instructions…</div><div className="as-text">The Coach is reading the charter.</div></div></div>}
          {suggestion && <div className="ai-suggest"><i className="ti ti-wand as-ic" /><div className="as-body"><div className="as-title">Suggested addition</div><div className="as-text">{suggestion.text}{suggestion.text !== suggestion.addition ? <><br /><span style={{ fontStyle: "italic" }}>“{suggestion.addition}”</span></> : null}</div><div className="as-act"><button className="m-btn accent sm" data-apply onClick={() => { edit((d) => ({ ...d, intent: { ...d.intent, instructions: `${(d.intent.instructions ?? "").trimEnd()}\n\n${suggestion.addition}` } })); setSuggestion(null); toast("Instructions updated", "ti-wand"); }}><i className="ti ti-check" /> Apply</button><button className="m-btn ghost sm" data-dismiss onClick={() => setSuggestion(null)}>Dismiss</button></div></div></div>}
          <div className="instr-foot">
            <span className="tk"><i className="ti ti-square-rounded-letter-t" style={{ fontSize: 13, verticalAlign: -2 }} /> {tokensOf(charter)} tokens</span>
            <span className="tk">·</span>
            <span className="tk">{vars.length} variable{vars.length === 1 ? "" : "s"}</span>
            <div className="var-chips">{vars.map((v) => <span key={v} className="m-chip"><i className="ti ti-variable" style={{ fontSize: 13, color: "var(--accent)" }} /> {v}</span>)}</div>
          </div>
        </div>
        <div hidden={instrTab !== "vars"}>
          <div className="vars-panel">
            <div className="vars-intro"><i className="ti ti-info-circle" /><div>Variables are placeholders like <code style={{ fontFamily: "var(--font-mono)" }}>{"{{company}}"}</code> in the prompt. They're filled in fresh on <b>every run</b> — from the trigger payload or the message — so one agent serves many customers without editing the prompt.</div></div>
            {declared.map((v) => (
              <div key={v.name} className="vrow" data-var={v.name}>
                <div className="vk"><span className="vtok">{`{{${v.name}}}`}</span></div>
                <div className="vmeta">
                  <div className="vdesc">{v.description || "No description"}{!used.includes(v.name) && <span className="m-badge" style={{ marginLeft: 8 }}>not used yet</span>}</div>
                  <div className="vsrc"><i className={`ti ${v.source === "setting" ? "ti-settings" : "ti-bolt"}`} style={{ fontSize: 12 }} /> Source: {v.source === "setting" ? <>Agent setting · <span className="mono">{v.value || "no value"}</span></> : <>Trigger payload{v.path ? <> · <span className="mono">{v.path}</span></> : " · no field yet"}</>}</div>
                </div>
                <div className="vtest"><div className="vlbl">TEST VALUE</div><input className="m-input" value={v.test_value ?? ""} placeholder={v.source === "setting" ? v.value || "" : ""} onChange={(e) => upsertVariable({ ...v, test_value: e.target.value })} /></div>
                <button className="m-btn ghost sm icon" title="More" onClick={(e) => openMenu(e.currentTarget, [
                  { icon: "ti-pencil", label: "Edit", run: () => addVariable(v) },
                  { icon: "ti-trash", label: "Remove", danger: true, run: () => { setVariables((list) => list.filter((x) => x.name !== v.name)); toast(`{{${v.name}}} removed`, "ti-trash"); } },
                ])}><i className="ti ti-dots" /></button>
              </div>
            ))}
            {undeclared.map((n) => (
              <div key={n} className="vrow" data-var={n}>
                <div className="vk"><span className="vtok">{`{{${n}}}`}</span></div>
                <div className="vmeta"><div className="vdesc">Named in the prompt, no value yet</div><div className="vsrc"><i className="ti ti-alert-circle" style={{ fontSize: 12, color: "var(--warn)" }} /> Runs see the placeholder as written until a source is set</div></div>
                <button className="m-btn ghost sm" onClick={() => addVariable({ name: n })}>Set source</button>
              </div>
            ))}
            {vars.length === 0 && <div className="m-hint">Type <code style={{ fontFamily: "var(--font-mono)" }}>{"{{name}}"}</code> in the prompt, or add one here.</div>}
            <div className="add-row" style={{ marginTop: 14 }} data-add-var onClick={() => addVariable()}><i className="ti ti-plus" /> Add variable</div>
          </div>
        </div>
      </Block>

      {/* ③ SKILLS */}
      <Block section="skills" icon="ti-puzzle" tileStyle={{ background: "var(--cat-plum-bg)", color: "var(--cat-plum)" }} title="Skills" count={mySkills.length} sub="Ways of working the agent follows, step by step"
        actions={<button className="m-btn ghost sm" onClick={(e) => openMenu(e.currentTarget, [
          { label: "From the library", icon: "ti-library", run: () => open("drawer", <SkillPickerDrawer skills={skills} catalog={catalog} have={draft.intent.skills ?? []} onAdd={(s, tools) => { edit((d) => ({ ...d, intent: { ...d.intent, skills: [...new Set([...(d.intent.skills ?? []), s.name])] } })); if (tools.length) addTools(tools.map((name) => ({ name, ...(skillToolHint(s, name) ? { when: skillToolHint(s, name) } : {}) }))); }} />) },
          { label: "Compose a skill", icon: "ti-pencil-plus", run: () => open("drawer", <SkillEditorDrawer isNew agent={agent} tools={toolNames} onSaved={(name) => { edit((d) => ({ ...d, intent: { ...d.intent, skills: [...new Set([...(d.intent.skills ?? []), name])] } })); void listSkills().then(setSkills); }} />) },
        ])}><i className="ti ti-plus" /> Add</button>}>
        {mySkills.map((s) => (
          <div key={s.name} className="skill">
            <div className="skill-top" onClick={() => open("drawer", <SkillEditorDrawer agent={agent} tools={toolNames} skill={s} onSaved={() => void listSkills().then(setSkills)} onRemove={() => edit((d) => ({ ...d, intent: { ...d.intent, skills: (d.intent.skills ?? []).filter((k) => k !== s.name) } }))} />)}>
              <div className="item-ic" style={{ background: "var(--cat-plum-bg)", color: "var(--cat-plum)" }}><i className="ti ti-route" /></div>
              <div className="item-body">
                <div className="item-name" title={s.name}>{plainName(s.name)}</div>
                <div className="item-desc" title={s.description}>{s.description ? firstSentence(s.description) : "No description."}</div>
              </div>
              <div className="item-act">
                {(s.allowed_tools ?? []).length > 0 && <button className="m-btn ghost sm" data-flow="skill-tools" onClick={(e) => { e.stopPropagation(); setOpenSkill(openSkill === s.name ? null : s.name); }}>{openSkill === s.name ? "Hide tools" : `Uses ${s.allowed_tools!.length} tool${s.allowed_tools!.length === 1 ? "" : "s"}`}</button>}
                <button className="m-btn icon ghost sm" title="More" onClick={(e) => { e.stopPropagation(); openMenu(e.currentTarget, [
                  { label: "Edit", icon: "ti-pencil", run: () => open("drawer", <SkillEditorDrawer agent={agent} tools={toolNames} skill={s} onSaved={() => void listSkills().then(setSkills)} />) },
                  { label: "Open in library", icon: "ti-arrow-up-right", run: () => navigate({ to: "/skills" }) },
                  "-",
                  { label: "Remove from agent", icon: "ti-trash", danger: true, run: () => { edit((d) => ({ ...d, intent: { ...d.intent, skills: (d.intent.skills ?? []).filter((k) => k !== s.name) } })); toast(`Removed "${s.name}" from ${draft.name}`, "ti-trash"); } },
                ]); }}><i className="ti ti-dots" /></button>
              </div>
            </div>
            {openSkill === s.name && (s.allowed_tools ?? []).length > 0 && (
              <div className="skill-tools">{s.allowed_tools?.map((t) => <span key={t} className="m-chip" title={t}><i className={`ti ${toolIcon(t)}`} style={{ fontSize: 12 }} /> {plainName(t)}</span>)}</div>
            )}
          </div>
        ))}
        <div className="add-row" onClick={() => open("drawer", <SkillPickerDrawer skills={skills} catalog={catalog} have={draft.intent.skills ?? []} onAdd={(s, tools) => { edit((d) => ({ ...d, intent: { ...d.intent, skills: [...new Set([...(d.intent.skills ?? []), s.name])] } })); if (tools.length) addTools(tools.map((name) => ({ name, ...(skillToolHint(s, name) ? { when: skillToolHint(s, name) } : {}) }))); }} />)}><i className="ti ti-plus" /> Add from the library</div>
      </Block>

      {/* ④ TOOLS */}
      <Block section="tools" icon="ti-tool" tileStyle={{ background: "var(--accent-bg)", color: "var(--accent)" }} title="Tools" count={toolNames.filter((n) => !off.has(n)).length} sub="What the agent can do and look up while it works"
        actions={<button className="m-btn ghost sm" onClick={() => open("drawer", <AddToolsDrawer catalog={catalog} instances={instances} have={toolNames} onAdd={addTools} />)}><i className="ti ti-plus" /> Add</button>}>
        {tools.map((t) => {
          const cat = catalogByName.get(t.name);
          const risk = cat ? riskOf(cat.effect) : "read";
          return (
            <div key={t.name} className="item" data-tool={t.name}>
              <div className="item-ic" style={{ background: "var(--bg-muted)", color: "var(--ink-700)" }}><i className={`ti ${toolIcon(t.name, cat?.effect)}`} /></div>
              <div className="item-body">
                <div className="item-name" title={t.name}>{plainName(t.name)}{!cat && <span className="item-tag" style={{ marginLeft: 8, color: "var(--bad)", borderColor: "var(--bad-line)" }}>not available</span>}</div>
                <div className="item-desc" title={t.when || cat?.description || ""}>{firstSentence(t.when || cat?.description || "")}</div>
                {risk !== "read" && <div className="item-meta"><span className="item-tag" style={risk === "destructive" ? { color: "var(--bad)", borderColor: "var(--bad-line)" } : undefined}>{risk === "destructive" ? "Can't be undone — asks first" : "Changes things — asks first"}</span></div>}
              </div>
              <div className="item-act"><Switch on={!off.has(t.name)} onChange={(on) => { setOff((o) => { const n = new Set(o); if (on) n.delete(t.name); else n.add(t.name); return n; }); if (!on) setTools(tools.filter((x) => x.name !== t.name)); }} title={off.has(t.name) ? "Off — removed from the agent" : "On"} /></div>
            </div>
          );
        })}
        <div className="add-row" onClick={() => open("drawer", <AddToolsDrawer catalog={catalog} instances={instances} have={toolNames} onAdd={addTools} />)}><i className="ti ti-plus" /> Add tool from catalog</div>
      </Block>

      {/* ⑤ CONNECTORS */}
      <Block section="connectors" icon="ti-plug-connected" tileStyle={{ background: "var(--cat-teal-bg)", color: "var(--cat-teal)" }} title="Connectors" count={myConnectors.length} sub="The systems it works in"
        actions={<button className="m-btn ghost sm" onClick={() => open("drawer", <AddConnectorDrawer onConnected={reload} />)}><i className="ti ti-plus" /> Connect</button>}>
        {myConnectors.map((c) => {
          const granted = !c.authorization || c.authorization.kind === "connected" || c.authorization.kind === "not_required";
          return (
            <div key={c.instance_id} className="item" data-connector={c.instance_id}>
              <div className="item-ic logo" style={{ background: "var(--bg-tint)", color: "var(--on-tint)" }}><i className="ti ti-plug-connected" /></div>
              <div className="item-body">
                <div className="item-name">{c.connector?.display_name ?? c.instance_id} {granted ? <Badge tone="good">Connected</Badge> : <Badge tone="warn">Reauthorize</Badge>}</div>
                <div className="item-desc">Uses {(c.tools ?? []).filter((t) => toolNames.includes(t)).length} of its {(c.tools ?? []).length} action{(c.tools ?? []).length === 1 ? "" : "s"}</div>
              </div>
              <div className="item-act"><button className={`m-btn ${granted ? "secondary" : "accent"} sm`} onClick={() => navigate({ to: "/connectors" })}>{granted ? "Manage" : "Fix"}</button></div>
            </div>
          );
        })}
        <div className="add-row" onClick={() => open("drawer", <AddConnectorDrawer onConnected={reload} />)}><i className="ti ti-plug" /> Browse connectors</div>
      </Block>

      {/* ⑥ MEMORY */}
      <Block section="memory" icon="ti-brain" tileStyle={{ background: "var(--cat-orange-bg)", color: "var(--cat-orange)" }} title="Memory" sub="What it remembers between conversations"
        actions={<button className="m-btn ghost sm" data-view-memory onClick={() => open("drawer", <MemoryDrawer agent={agent} declared={intent.memory?.blocks ?? []} />)}><i className="ti ti-eye" /> View memory</button>}>
        <div className="mem-grid">
          <div className="mem-card on">
            <div className="mem-top">
              <div className="mem-ic" style={{ background: "var(--accent-bg)", color: "var(--accent)" }}><i className="ti ti-history-toggle" /></div>
              <div style={{ flex: 1 }}><div className="mem-name">Remembers the conversation</div></div>
              <Switch on title="Always on" />
            </div>
            <div className="mem-desc">Keeps the last {(intent.context?.keep_recent_messages ?? context?.keep_recent_messages ?? 8)} turns in view and sums up the older ones.</div>
          </div>
          <div className={`mem-card${intent.memory?.access !== "none" ? " on" : ""}`}>
            <div className="mem-top">
              <div className="mem-ic" style={{ background: "var(--cat-orange-bg)", color: "var(--cat-orange)" }}><i className="ti ti-database-cog" /></div>
              <div style={{ flex: 1 }}><div className="mem-name">Remembers what it learns</div></div>
              <Switch on={intent.memory?.access !== "none"} title={intent.memory?.access === "none" ? "Off: it starts every conversation fresh" : "On"} onChange={(on) => edit((d) => ({ ...d, intent: { ...d.intent, memory: { ...(d.intent.memory ?? {}), access: on ? "read_write" : "none" } } }))} />
            </div>
            <div className="mem-desc">Keeps what it learns and what people tell it for next time — what a person says stays with that person.</div>
          </div>
        </div>
        {intent.memory?.access !== "none" && (
          <div className="mem-card on" style={{ marginTop: 12 }}>
            <div className="mem-top">
              <div className="mem-ic" style={{ background: "var(--cat-plum-bg)", color: "var(--cat-plum)" }}><i className="ti ti-pin" /></div>
              <div style={{ flex: 1 }}><div className="mem-name">Pinned notes</div></div>
              <button className="m-btn ghost sm" data-flow="add-pinned" onClick={() => setAddPin((v) => !v)}>{addPin ? "Done" : "Add a pinned note"}</button>
            </div>
            <div className="mem-desc">Always in front of it, and kept up to date by it: {["About the person", "What it is working on", "Decisions that stand", ...(intent.memory?.blocks ?? []).map((b) => b.label)].join(" · ")}.</div>
            {addPin && <DeclaredBlocks intent={intent} edit={edit} />}
          </div>
        )}
      </Block>

      {/* ⑦ KNOWLEDGE */}
      <Block section="knowledge" icon="ti-books" tileStyle={{ background: "var(--cat-rose-bg)", color: "var(--cat-rose)" }} title="Knowledge" count={knowledge.length} sub="What it looks things up in"
        actions={<><button className="m-btn ghost sm" onClick={() => open("modal", <QueryModal scope={{ scope: "agent", id: agent.assistant_id }} />)}><i className="ti ti-search" /> Ask</button><button className="m-btn ghost sm" onClick={() => open("drawer", <AddSourceDrawer scope={{ scope: "agent", id: agent.assistant_id }} onAdded={reloadKnowledge} />)}><i className="ti ti-plus" /> Add</button></>}>
        {knowledge.map((k) => (
          <div key={k.source_id} className="item" style={{ cursor: "pointer" }} onClick={() => navigate({ to: "/knowledge" })}>
            <div className="item-ic" style={{ background: "var(--cat-rose-bg)", color: "var(--cat-rose)" }}><i className="ti ti-file-text" /></div>
            <div className="item-body"><div className="item-name">{k.title}</div><div className="item-desc">{({ organization: "Our own", vendor: "Vendor documentation", generic: "General guidance" } as Record<string, string>)[k.provenance ?? "organization"] ?? "Our own"} · {k.scope.scope === "agent" ? "only this agent" : "all agents"}{k.retention.policy === "ttl" ? ` · until ${new Date(k.retention.expires_at).toLocaleDateString()}` : ""}</div></div>
          </div>
        ))}
        {!hasSearchTool && knowledge.length > 0 && <div className="m-alert" style={{ marginTop: 10 }}><i className="ti ti-alert-triangle" style={{ color: "var(--warn)", fontSize: 16 }} /><div className="a-body"><div className="a-title">It cannot look these up yet</div><div className="a-text">Give it the <b>Search knowledge</b> tool under Tools.</div></div></div>}
        <div className="add-row" onClick={() => open("drawer", <AddSourceDrawer scope={{ scope: "agent", id: agent.assistant_id }} onAdded={reloadKnowledge} />)}><i className="ti ti-upload" /> Add a source for this agent</div>
        <div className="item" data-knowledge-unit style={{ marginTop: 10 }}>
          <div className="item-ic" style={{ background: "var(--cat-plum-bg)", color: "var(--cat-plum)" }}><i className="ti ti-stack-2" /></div>
          <div className="item-body"><div className="item-name">Always carry these sources</div><div className="item-desc">{unit?.unit ? `${unit.unit.sources.length} source${unit.unit.sources.length === 1 ? "" : "s"} in front of it in every conversation${unit.stale?.length ? ` · out of date: ${unit.stale.map((s) => s.title).join(", ")}` : ""}${unit.contested?.length ? ` · disputed: ${unit.contested.map((c) => `"${c.claim}"`).join(", ")}` : ""}` : "Pick sources it should have in front of it every time, not only when it searches."}</div></div>
          <div className="item-act">{unit?.unit && <button className="m-btn ghost sm" data-flow="unit-remove" onClick={() => void removeKnowledgeUnit(agent.assistant_id).then(() => { toast("It no longer carries those sources in every conversation", "ti-trash"); reloadKnowledge(); }).catch((err) => toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle"))}>Remove</button>}<button className="m-btn secondary sm" data-flow="unit-compile" disabled={knowledge.length === 0} onClick={() => open("modal", <CompileUnitModal agentId={agent.assistant_id} sources={knowledge} current={unit?.unit ?? null} onCompiled={reloadKnowledge} />)}>{unit?.unit ? "Update" : "Choose sources"}</button></div>
        </div>
      </Block>

      {/* ⑧ TRIGGERS */}
      <Block section="triggers" icon="ti-bolt" tileStyle={{ background: "var(--cat-amber-bg)", color: "var(--cat-amber)" }} title="Triggers" count={mySchedules.length + myWebhooks.length + (intent.pool ? 1 : 0)} sub="How runs start — events, schedules, or webhooks" collapsed
        actions={<button className="m-btn ghost sm" onClick={() => open("modal", <TriggerModal agent={agent} onAdded={reload} onPool={(pool) => edit((d) => ({ ...d, intent: { ...d.intent, pool } }))} />)}><i className="ti ti-plus" /> Add</button>}>
        {mySchedules.map((s) => (
          <div key={s.cron_id} className="item">
            <div className="item-ic" style={{ background: "var(--cat-amber-bg)", color: "var(--cat-amber)" }}><i className="ti ti-clock-hour-4" /></div>
            <div className="item-body"><div className="item-name">Schedule · {cadenceWords({ interval_secs: s.interval_secs, cron_expr: s.cron_expr })}</div><div className="item-desc">{s.input?.messages?.[0]?.content ?? "Runs with no message"} · fired {s.runs_fired}×{s.max_runs ? ` of ${s.max_runs}` : ""}{s.max_tokens ? ` · spent ${(s.tokens_spent ?? 0).toLocaleString()} of ${s.max_tokens.toLocaleString()} tokens` : s.tokens_spent ? ` · spent ${s.tokens_spent.toLocaleString()} tokens` : ""}{s.stalled ? <span style={{ color: "var(--bad)" }}> · stopped: {s.stalled}</span> : null}</div></div>
            <div className="item-act"><Switch on onChange={async () => { await deleteSchedule(s.cron_id).catch(() => {}); reload(); toast("Schedule removed", "ti-bolt"); }} title="Off removes the schedule" /></div>
          </div>
        ))}
        {myWebhooks.map((w) => (
          <div key={w.trigger_id} className="item" style={{ cursor: "pointer" }} onClick={() => open("drawer", <WebhookDrawer w={w} />)}>
            <div className="item-ic" style={{ background: "var(--cat-plum-bg)", color: "var(--cat-plum)" }}><i className="ti ti-webhook" /></div>
            <div className="item-body"><div className="item-name">{w.name}</div><div className="item-desc"><span className="mono">POST {webhookUrl(w.trigger_id).replace(/^https?:\/\/[^/]+/, "")}</span> · HMAC-signed{w.world_name ? ` · in ${w.world_name}` : ""}</div></div>
            <div className="item-act"><button className="m-btn ghost sm icon" title="Copy URL" onClick={(e) => { e.stopPropagation(); void navigator.clipboard?.writeText(webhookUrl(w.trigger_id)); toast("Webhook URL copied", "ti-copy"); }}><i className="ti ti-copy" /></button><Switch on={w.enabled} onChange={async () => { await deleteWebhook(w.trigger_id).catch(() => {}); reload(); toast("Webhook removed", "ti-bolt"); }} title="Off removes the webhook" /></div>
          </div>
        ))}
        {intent.pool && (
          <div className="item">
            <div className="item-ic" style={{ background: "var(--cat-teal-bg)", color: "var(--cat-teal)" }}><i className="ti ti-list-check" /></div>
            <div className="item-body"><div className="item-name">Task queue · <span className="mono">{intent.pool}</span></div><div className="item-desc">The server claims tasks from this pool one at a time and runs the agent with each task's payload as the message.</div></div>
            <div className="item-act"><Switch on onChange={() => edit((d) => ({ ...d, intent: { ...d.intent, pool: undefined } }))} title="Off leaves the pool" /></div>
          </div>
        )}
        {mySchedules.length + myWebhooks.length + (intent.pool ? 1 : 0) === 0 && <div className="add-row" onClick={() => open("modal", <TriggerModal agent={agent} onAdded={reload} onPool={(pool) => edit((d) => ({ ...d, intent: { ...d.intent, pool } }))} />)}><i className="ti ti-bolt" /> Add a trigger</div>}
      </Block>

      {/* ⑨ EVALUATION */}
      <Block section="evaluation" icon="ti-target-arrow" title="Evaluation" count={myDatasets.length} sub="How you’ll know the agent is working before and after publish" collapsed
        actions={<button className="m-btn ghost sm" onClick={() => open("modal", <EvalGoalModal agent={agent} onSaved={reload} />)}><i className="ti ti-plus" /> Add goal</button>}>
        {myDatasets.map((d) => (
          <div key={d.name} className="item" onClick={() => navigate({ to: "/evals", search: { dataset: d.name } })} style={{ cursor: "pointer" }}>
            <div className="item-ic"><i className="ti ti-target-arrow" /></div>
            <div className="item-body"><div className="item-name">{d.name} <span className="m-badge sm">v{d.version}</span></div><div className="item-desc">{d.case_count} cases · runs on every publish</div></div>
            <div className="item-act"><button className="m-btn secondary sm"><i className="ti ti-player-play" /> Open</button></div>
          </div>
        ))}
        <div className="add-row" onClick={() => open("modal", <EvalGoalModal agent={agent} onSaved={reload} />)}><i className="ti ti-target-arrow" /> Set an evaluation goal</div>
      </Block>

      {/* ⑩ GUARDRAILS */}
      <Block section="guardrails" icon="ti-shield-check" tileStyle={{ background: "var(--bad-bg)", color: "var(--bad)" }} title="Guardrails" sub="Limits and safety checks applied to every run" collapsed>
        <div className="guard">
          <div className="guard-ic"><i className="ti ti-hand-stop" /></div>
          <div className="guard-body"><div className="guard-name">Approval before irreversible actions</div><div className="guard-desc">A write that cannot be undone pauses for a person; a standing approval can cover a repeated call.</div></div>
          <Switch on title="The platform's gate; always on" />
        </div>
        {standing.map((sa) => (
          <div key={sa.id} className="item">
            <div className="item-ic" style={{ background: "var(--good-bg)", color: "var(--good)" }}><i className="ti ti-shield-check" /></div>
            <div className="item-body"><div className="item-name">Standing approval · <span className="mono">{sa.tool}</span></div><div className="item-desc">by {sa.by?.name ?? "a person"} · until {new Date(sa.until).toLocaleString()} · used {sa.uses}×{sa.live === false ? " · expired" : ""}</div></div>
            <div className="item-act"><button className="m-btn ghost sm" onClick={async () => { try { await withdrawStandingApproval(sa.id); setStanding((l) => l.filter((x) => x.id !== sa.id)); toast("Standing approval withdrawn", "ti-shield-off"); } catch (err) { toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle"); } }}>Withdraw</button></div>
          </div>
        ))}
        <div className="guard">
          <div className="guard-ic"><i className="ti ti-currency-dollar" /></div>
          <div className="guard-body"><div className="guard-name">Run budget</div><div className="guard-desc">{bounded ? `Hard cap of ${budget.max_tokens ? `${Number(budget.max_tokens).toLocaleString()} tokens` : ""}${budget.max_tokens && budget.max_cost_usd ? " and " : ""}${budget.max_cost_usd ? `$${budget.max_cost_usd}` : ""} per run before it halts.` : "No cap — a run may spend what it needs."}</div></div>
          <Switch on={bounded} onChange={(on) => edit((d) => ({ ...d, intent: { ...d.intent, budget: on ? { max_tokens: 20000 } : {} } }))} />
        </div>
        <div className="guard">
          <div className="guard-ic"><i className="ti ti-eye-off" /></div>
          <div className="guard-body"><div className="guard-name">Secrets never in context</div><div className="guard-desc">Credentials are sealed in the store and never reach the model or the journal.</div></div>
          <Switch on title="The platform's rule; always on" />
        </div>
        <div className="guard">
          <div className="guard-ic"><i className="ti ti-message-report" /></div>
          <div className="guard-body"><div className="guard-name">Outcome verification</div><div className="guard-desc">Every finished run is judged against its own evidence; a false claim is sent back once.</div></div>
          <Switch on title="The deployment's verifier; always on" />
        </div>
      </Block>
    </>
  );
}

function ReadinessModal({ checks }: { checks: [string, string, boolean][] }) {
  return (
    <div className="m-modal">
      <div className="ov-head"><div className="oh-ic"><i className="ti ti-progress-check" /></div><div className="oh-titles"><div className="ov-title">Readiness checklist</div><div className="ov-sub">What the agent needs before it can go live.</div></div><div className="ov-close" data-close><i className="ti ti-x" /></div></div>
      <div className="ov-body ready-list">
        {checks.map((c) => (
          <div key={c[0]} className="chk"><div className={`chk-mark ${c[2] ? "ok" : "miss"}`}><i className={`ti ${c[2] ? "ti-check" : "ti-alert-triangle"}`} /></div><div className="chk-body"><div className="chk-name">{c[0]}</div><div className="chk-desc">{c[2] ? "Done" : c[1][0].toUpperCase() + c[1].slice(1)}</div></div></div>
        ))}
      </div>
      <div className="ov-foot"><div className="sp" /><button className="m-btn secondary" data-close>Close</button></div>
    </div>
  );
}

/** A webhook's door: the URL a sender posts to, its secret, and what arrived. */
function WebhookDrawer({ w }: { w: Webhook }) {
  const { toast } = useOverlay();
  const [events, setEvents] = useState<WebhookEvent[]>([]);
  useEffect(() => { listWebhookEvents(w.trigger_id).then(setEvents).catch(() => {}); }, [w.trigger_id]);
  return (
    <div className="m-drawer">
      <OvHead icon="ti-webhook" bg="var(--cat-plum-bg)" fg="var(--cat-plum)" title={w.name} sub={`${w.action.replace("_", " ")} · ${w.enabled ? "enabled" : "disabled"}${w.world_name ? ` · in ${w.world_name}` : ""}`} />
      <div className="ov-body">
        <div className="fld"><label className="fld-label">URL</label><div className="copy-row"><span>{webhookUrl(w.trigger_id)}</span><i className="ti ti-copy" onClick={() => { void navigator.clipboard?.writeText(webhookUrl(w.trigger_id)); toast("Copied"); }} /></div></div>
        <div className="fld"><label className="fld-label">Signing secret <span className="opt">— the sender signs the body with it (HMAC-SHA256)</span></label><div className="copy-row"><span>{w.secret}</span><i className="ti ti-copy" onClick={() => { void navigator.clipboard?.writeText(w.secret); toast("Copied"); }} /></div></div>
        <div className="fld"><label className="fld-label">Example</label><div className="pre">{`curl ${webhookUrl(w.trigger_id)} \\\n  -H "Content-Type: application/json" \\\n  -H "X-Signature: sha256=$(printf '%s' "$BODY" | openssl dgst -sha256 -hmac "$SECRET" | cut -d' ' -f2)" \\\n  -d "$BODY"`}</div></div>
        <div className="cat-label"><span>Events</span><span className="ln" /></div>
        {events.slice(0, 20).map((e) => <div key={e.event_id} className="scope-row"><div className="sb"><div className="mono" style={{ fontFamily: "var(--font-mono)", fontSize: "var(--fs-xs)" }}>{e.event_id.slice(0, 12)} · {e.status}</div><div className="sd">{e.run_id ? `run ${e.run_id.slice(0, 12)}` : e.error ?? "no run"}</div></div></div>)}
        {events.length === 0 && <div className="m-hint">Nothing has arrived yet.</div>}
      </div>
      <div className="ov-foot"><div className="sp" /><button className="m-btn secondary" data-close>Done</button></div>
    </div>
  );
}


/** Compile the agent's knowledge unit: pick the sources; the server ranks them by provenance, bounds the text, and refuses while one is contested. */
function CompileUnitModal({ agentId, sources, current, onCompiled }: { agentId: string; sources: KnowledgeSourceSummary[]; current: KnowledgeUnit | null; onCompiled: () => void }) {
  const { close, toast } = useOverlay();
  const [picked, setPicked] = useState<string[]>(current ? current.sources.map((s) => s.source_id) : sources.map((s) => s.source_id));
  const [limit, setLimit] = useState<string>(String(current?.char_limit ?? 12000));
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);
  const [preview, setPreview] = useState<string | null>(current?.text ?? null);
  const order = (p?: string) => (p === "vendor" ? 1 : p === "generic" ? 2 : 0);
  const sorted = [...sources].sort((a, b) => order(a.provenance) - order(b.provenance) || a.title.localeCompare(b.title));
  async function compile() {
    setBusy(true); setProblem(null);
    try { const r = await compileKnowledgeUnit(agentId, picked, Number(limit) || undefined); setPreview(r.unit.text); toast(`Compiled v${r.unit.version} — ${r.chars.toLocaleString()} chars from ${r.unit.sources.length} source${r.unit.sources.length === 1 ? "" : "s"}`, "ti-stack-2"); onCompiled(); }
    catch (err) { setProblem(err instanceof Error ? err.message : "the server refused"); }
    finally { setBusy(false); }
  }
  return (
    <div className="m-modal lg" data-compile-unit>
      <OvHead icon="ti-stack-2" bg="var(--cat-plum-bg)" fg="var(--cat-plum)" title="Compile the knowledge unit" sub="The chosen sources' text, ranked by provenance — our own first, then vendor documentation, then generic guidance — carried in every run's first turn. Refused while a chosen source is contested." />
      <div className="ov-body">
        {sorted.map((s) => <label key={s.source_id} className="scope-row" style={{ display: "flex", gap: 8, alignItems: "center", cursor: "pointer" }} data-unit-source={s.source_id}><input type="checkbox" checked={picked.includes(s.source_id)} onChange={(e) => setPicked((p) => (e.target.checked ? [...p, s.source_id] : p.filter((x) => x !== s.source_id)))} /><span style={{ fontWeight: 600 }}>{s.title}</span><span className="item-tag">{PROVENANCE_LABEL[s.provenance ?? "organization"]}</span><span style={{ color: "var(--ink-500)", fontSize: 12 }}>v{s.version} · {s.chunk_count} chunks</span></label>)}
        <div className="fld" style={{ marginTop: 10 }}><label className="fld-label">At most, in characters</label><input className="m-input" style={{ width: 120 }} value={limit} onChange={(e) => setLimit(e.target.value.replace(/[^0-9]/g, ""))} /></div>
        {problem && <div className="m-alert" data-unit-problem><i className="ti ti-alert-triangle" style={{ color: "var(--bad)", fontSize: 16 }} /><div className="a-body"><div className="a-text">{problem}</div></div></div>}
        {preview && <div className="pre" style={{ marginTop: 10, whiteSpace: "pre-wrap", maxHeight: 300, overflow: "auto", fontSize: 12 }} data-unit-preview>{preview}</div>}
      </div>
      <div className="ov-foot"><button className="m-btn ghost" onClick={close}>Close</button><button className="m-btn primary" data-flow="unit-compile-go" disabled={busy || picked.length === 0} onClick={() => void compile()}>{busy ? "Compiling…" : "Compile"}</button></div>
    </div>
  );
}
