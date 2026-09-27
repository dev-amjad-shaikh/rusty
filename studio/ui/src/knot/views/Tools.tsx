import { useEffect, useMemo, useState } from "react";
import { useNavigate } from "@tanstack/react-router";
import { assistantVersion, createAssistantVersion, listConnectorInstances, serverInfo, type Assistant, type ConnectorInstance, type ServerTool } from "../../engine/net/client";
import { useServer } from "../../engine/net/server";
import { Badge, OvHead, useOverlay } from "../overlay";
import { UsedStack } from "./Skills";
import { connectorOf, isPlatform, riskOf, toolIcon } from "../data";
import { firstSentence, plainName } from "../agents/words";

const TYPE_STYLE: Record<string, [string, string]> = { "Built in": ["var(--bg-muted)", "var(--ink-700)"], Platform: ["var(--brand-soft)", "var(--ink-800)"], Connector: ["var(--bg-tint)", "#fff"] };
/** A tool's type: built in (no dot), the platform's own (dotted, no connection derives it), or a connection's operation. */
const typeOf = (t: ServerTool, derived: Set<string>) => (!t.name.includes(".") ? "Built in" : derived.has(t.name) ? "Connector" : "Platform");

/** Tools: every function agents can call on this server, with who calls it. */
export function ToolsView() {
  const { open } = useOverlay();
  const assistants = useServer((s) => s.assistants);
  const [catalog, setCatalog] = useState<ServerTool[]>([]);
  const [instances, setInstances] = useState<ConnectorInstance[]>([]);
  const [q, setQ] = useState("");
  useEffect(() => { serverInfo().then((i) => setCatalog(i.graphs.flatMap((g) => g.tools))).catch(() => {}); listConnectorInstances().then(setInstances).catch(() => {}); }, []);
  const derived = useMemo(() => new Set(instances.flatMap((i) => i.tools ?? [])), [instances]);
  const users = (name: string) => assistants.filter((a) => !a.archived_at && (a.config?.studio_intent?.tools ?? []).some((t) => t.name === name));
  const shown = catalog.filter((t) => !q || `${t.name} ${t.description}`.toLowerCase().includes(q.toLowerCase())).sort((a, b) => a.name.localeCompare(b.name));
  const cats = new Set(shown.map((t) => connectorOf(t.name) ?? typeOf(t, derived)));
  const inUse = new Set(assistants.filter((a) => !a.archived_at && !isPlatform(a)).flatMap((a) => (a.config?.studio_intent?.tools ?? []).map((t) => t.name)));
  return (
    <div className="view library active" id="view-tools">
      <div className="lib-top"><div className="crumbs"><span>Rusty</span><i className="ti ti-chevron-right" style={{ color: "var(--ink-300)", fontSize: 15 }} /><b>Tools</b></div><div className="sp" /><div className="lib-search" style={{ maxWidth: 240 }}><i className="ti ti-search" /><input placeholder="Search…" data-libsearch value={q} onChange={(e) => setQ(e.target.value)} /></div></div>
      <div className="lib-page">
        <div className="lib-hero">
          <div className="lh-ic" style={{ background: "var(--accent-bg)", color: "var(--accent)" }}><i className="ti ti-tool" /></div>
          <div className="lh-main"><div className="lib-eyebrow">Library</div><h1 className="lib-title">Tools</h1><p className="lib-lead">What agents can do and look up while they work. Anything that changes something asks a person first.</p></div>
          <div className="lh-act"><button className="m-btn primary" data-new="tool" onClick={() => open("drawer", <NewToolDrawer />)}><i className="ti ti-plus" /> New tool</button></div>
        </div>
        <div className="lib-stats">
          <Stat v={String(catalog.length)} l="Tools" />
          <Stat v={String(catalog.filter((t) => inUse.has(t.name)).length)} l="In use by agents" />
        </div>
        <div className="lib-table-wrap"><table className="m-table">
          <thead><tr><th>Tool</th><th>What it does</th><th>System</th><th>Used by</th><th>Kind</th></tr></thead>
          <tbody>
            {shown.map((t) => { const type = typeOf(t, derived); const [bg, fg] = TYPE_STYLE[type]; const risk = riskOf(t.effect); const u = users(t.name); return (
              <tr key={t.name} data-detail={t.name} className="clickable" onClick={() => open("drawer", <ToolDrawer tool={t} type={type} users={u} />)}>
                <td><div className="tl-row-name"><div className="tl-ic" style={{ background: bg, color: fg }}><i className={`ti ${toolIcon(t.name, t.effect)}`} /></div><span title={t.name}>{plainName(t.name)}</span></div></td>
                <td style={{ color: "var(--ink-600)" }} title={t.description}>{firstSentence(t.description, 110)}</td>
                <td><span className="item-tag">{connectorOf(t.name) ? plainName(connectorOf(t.name)!) : "Platform"}</span></td>
                <td><UsedStack agents={u.map((a) => a.name)} /></td>
                <td><Badge tone={risk === "read" ? "good" : risk === "write" ? "warn" : "bad"}>{risk === "read" ? "Looks things up" : risk === "write" ? "Changes things · asks first" : "Can't be undone · asks first"}</Badge></td>
              </tr>
            ); })}
          </tbody>
        </table></div>
        {catalog.length === 0 && <div className="thread-empty" style={{ padding: 40 }}><i className="ti ti-tool" />Reading the catalog…</div>}
      </div>
    </div>
  );
}

/** One tool: what the model reads, its parameters, its policy, who uses it — and a door onto an agent. */
function ToolDrawer({ tool, type, users }: { tool: ServerTool; type: string; users: Assistant[] }) {
  const { close, toast } = useOverlay();
  const navigate = useNavigate();
  const assistants = useServer((s) => s.assistants);
  const candidates = assistants.filter((a) => !a.archived_at && !isPlatform(a) && !users.some((u) => u.assistant_id === a.assistant_id));
  const [target, setTarget] = useState(candidates[0]?.assistant_id ?? "");
  const [busy, setBusy] = useState(false);
  const risk = riskOf(tool.effect);
  const [bg, fg] = TYPE_STYLE[type];
  /** Add to an agent: a new version above the one that serves, with the tool on its list — a draft until published. */
  async function addTo() {
    const a = assistants.find((x) => x.assistant_id === target); if (!a) return;
    setBusy(true);
    try {
      const active = a.active_version_id ? await assistantVersion(a.assistant_id, a.active_version_id) : null;
      const intent = active?.config?.studio_intent ?? a.config?.studio_intent ?? {};
      await createAssistantVersion(a.assistant_id, { base_version_id: a.active_version_id ?? "", name: active?.name ?? a.name, graph: a.graph, metadata: active?.metadata ?? a.metadata, config: { ...(active?.config ?? a.config ?? {}), studio_intent: { ...intent, tools: [...(intent.tools ?? []), { name: tool.name }] } } });
      await useServer.getState().refresh();
      toast(`${tool.name} added to ${a.name} — publish to run it`, "ti-tool"); close(); navigate({ to: "/agents/$id", params: { id: a.assistant_id } });
    } catch (err) { toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle"); }
    finally { setBusy(false); }
  }
  return (
    <div className="m-drawer" style={{ width: "min(560px,100%)" }}>
      <OvHead icon={toolIcon(tool.name, tool.effect)} bg={bg} fg={fg} title={plainName(tool.name)} sub={`${tool.name} · ${riskOf(tool.effect) === "read" ? "looks things up" : riskOf(tool.effect) === "write" ? "changes things — asks a person first" : "can delete — asks a person first"}`} />
      <div className="ov-body">
        <div className="fld"><label className="fld-label">What the agent is told about it</label><div className="m-textarea" style={{ minHeight: 0 }}>{tool.description}</div></div>
        <div className="fld"><label className="fld-label">Parameters</label><div className="pre">{JSON.stringify(tool.parameters_schema ?? { type: "object", properties: {} }, null, 2)}</div></div>
        <div className="cat-label"><span>Policy</span><span className="ln" /></div>
        {[["Requires human approval", risk !== "read", risk === "read" ? "reads never pause a run" : "the gate holds the run before this call"], ["Journalled with arguments", true, "every call lands in the run's events"], ["Verified after the run", true, "the outcome verifier reads its result"]].map(([n, on, d]) => <div key={n as string} className="set-row"><div className="sk"><div className="n">{n}</div><div className="d">{d}</div></div><div className={`m-switch${on ? " on" : ""}`} data-switch style={{ pointerEvents: "none" }} /></div>)}
        <div className="cat-label" style={{ marginTop: 22 }}><span>Used by</span><span className="ln" /></div>
        <div className="kv"><span className="k">Agents</span><span className="v"><UsedStack agents={users.map((a) => a.name)} /></span>{users.map((a) => <span key={a.assistant_id} className="k" style={{ gridColumn: "1 / -1" }}><a href="#" onClick={(e) => { e.preventDefault(); close(); navigate({ to: "/agents/$id", params: { id: a.assistant_id } }); }} style={{ color: "var(--ink-900)", fontWeight: 500 }}>{a.name}</a></span>)}</div>
      </div>
      <div className="ov-foot">
        {candidates.length > 0 && <><select className="m-select" style={{ width: 200 }} value={target} onChange={(e) => setTarget(e.target.value)}>{candidates.map((a) => <option key={a.assistant_id} value={a.assistant_id}>{a.name}</option>)}</select><button className="m-btn ghost sm" data-add disabled={busy || !target} onClick={() => void addTo()}><i className="ti ti-plus" /> Add to agent</button></>}
        <div className="sp" /><button className="m-btn secondary" data-close>Done</button>
      </div>
    </div>
  );
}

/** A new tool is a connection's operation: the library has no hand-written tools. */
function NewToolDrawer() {
  const navigate = useNavigate();
  const { close } = useOverlay();
  return (
    <div className="m-drawer">
      <OvHead icon="ti-tool" bg="var(--accent-bg)" fg="var(--accent)" title="New tool" sub="Tools come from what the server can reach." />
      <div className="ov-body">
        {[["ti-plug-connected", "From a connection", "Connect a service; every operation its manifest names becomes a tool with an effect class.", "/connectors"], ["ti-api", "From an OpenAPI document", "Import a spec under Connectors → Browse all → Custom protocol; each operation becomes a tool.", "/connectors"], ["ti-puzzle", "As a skill", "A procedure over existing tools is a skill, not a tool.", "/skills"]].map(([ic, n, d, to]) => (
          <div key={n} className="trig-opt" onClick={() => { close(); navigate({ to: to as "/connectors" | "/skills" }); }}><div className="to-ic"><i className={`ti ${ic}`} /></div><div><div className="to-name">{n}</div><div className="to-desc">{d}</div></div><i className="ti ti-chevron-right to-go" /></div>
        ))}
      </div>
    </div>
  );
}

function Stat({ v, u, l }: { v: string; u?: string; l: string }) {
  return <div className="lib-stat"><div className="ls-v">{v}{u && <span className="u">{u}</span>}</div><div className="ls-l">{l}</div></div>;
}
