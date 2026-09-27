import { useEffect, useMemo, useState } from "react";
import { useNavigate } from "@tanstack/react-router";
import { RunDrawer } from "./Observability";
import { acceptMemory, forgetMemory, listConnectorInstances, listNotices, proposedMemory, type ConnectorInstance, type MemoryRecord, type Notice, type Run } from "../../engine/net/client";
import { OvHead } from "../overlay";
import { useServer } from "../../engine/net/server";
import { useOverlay } from "../overlay";
import { CreateWizard } from "../flows/wizard";
import { NotificationsDrawer } from "../flows/shellFlows";
import { COLORS, COLOR_BG, ago, compact, isPlatform, lookOf, statusOf } from "../data";

/** Home: the workspace at a glance — its agents, what needs a person, what happened. */
export function HomeView() {
  const navigate = useNavigate();
  const { open, openWizard } = useOverlay();
  const assistants = useServer((s) => s.assistants);
  const runs = useServer((s) => s.runs);
  const me = useServer((s) => s.me);
  const [notices, setNotices] = useState<Notice[]>([]);
  const [instances, setInstances] = useState<ConnectorInstance[]>([]);
  // Notes agents proposed about the person signed in: theirs to accept, in one place across agents.
  const [proposed, setProposed] = useState<MemoryRecord[]>([]);
  const loadProposed = () => { proposedMemory().then((r) => setProposed(r.filter((m) => m.scope.scope === "user" && (!me || m.scope.id === me.principal.id)))).catch(() => {}); };
  useEffect(() => { listNotices().then(setNotices).catch(() => {}); listConnectorInstances().then(setInstances).catch(() => {}); loadProposed(); }, [me?.principal.id]); // eslint-disable-line react-hooks/exhaustive-deps

  const agents = useMemo(() => assistants.filter((a) => !a.archived_at && !isPlatform(a)), [assistants]);
  const finished = runs.filter((r) => r.status !== "pending" && r.status !== "running");
  const paused = runs.filter((r) => r.status === "interrupted" && !r.decision);
  const withoutPause = finished.length ? Math.round((finished.filter((r) => r.status !== "interrupted" && !r.decision).length / finished.length) * 100) : null;
  const verified = runs.filter((r) => r.verification?.verdict === "verified" || r.verification?.verdict === "failed");
  const verifiedPct = verified.length ? Math.round((verified.filter((r) => r.verification?.verdict === "verified").length / verified.length) * 100) : null;
  const needsAuth = instances.filter((i) => i.authorization && (i.authorization.kind === "needs_auth" || i.authorization.kind === "expired"));
  const hour = new Date().getHours(), greet = hour < 12 ? "Good morning" : hour < 18 ? "Good afternoon" : "Good evening";
  const who = me?.principal.name.split(" ")[0] ?? "there";

  // Runs per day, fourteen days, from the newest runs the server lists.
  const days = useMemo(() => {
    const out: { key: string; label: string; runs: number }[] = [];
    const today = new Date(); today.setHours(0, 0, 0, 0);
    for (let i = 13; i >= 0; i--) { const d = new Date(today.getTime() - i * 86_400_000); out.push({ key: d.toISOString().slice(0, 10), label: d.toLocaleDateString(undefined, { month: "short", day: "numeric" }), runs: 0 }); }
    for (const r of runs) { const d = out.find((x) => x.key === r.created_at.slice(0, 10)); if (d) d.runs++; }
    return out;
  }, [runs]);
  const max = Math.max(1, ...days.map((d) => d.runs));

  // Activity: the person's notices and the newest runs, as one list.
  const activity = useMemo(() => {
    const items: { run?: Run; agent?: string;  icon: string; text: React.ReactNode; at: string }[] = [];
    for (const n of notices.slice(0, 8)) items.push({ icon: n.about.kind === "approval" ? "ti-hand-stop" : n.about.dataset ? "ti-target-arrow" : n.about.task_id ? "ti-list-check" : n.about.cron_id || n.about.trigger_id ? "ti-bolt" : "ti-bell", text: <><b>{n.title}</b> — {n.text.length > 140 ? `${n.text.slice(0, 140)}…` : n.text}</>, at: n.created_at });
    for (const r of runs.slice(0, 8)) { const a = assistants.find((x) => x.assistant_id === r.assistant_id); items.push({ run: r, agent: a?.name ?? "An agent", icon: r.status === "interrupted" ? "ti-hand-stop" : r.verification?.verdict === "failed" ? "ti-alert-circle" : "ti-player-play", text: <><b>{a?.name ?? "An agent"}</b> {r.status === "interrupted" ? "paused for a decision" : r.verification?.verdict === "verified" ? "achieved its outcome" : r.verification?.verdict === "failed" ? "did not achieve its outcome" : r.status === "success" ? "finished a run" : `run ${r.status}`}{r.asked ? <> · <span style={{ color: "var(--ink-500)" }}>{r.asked.slice(0, 60)}{r.asked.length > 60 ? "…" : ""}</span></> : null}</>, at: r.created_at }); }
    return items.sort((a, b) => (a.at < b.at ? 1 : -1)).slice(0, 8);
  }, [notices, runs, assistants]);

  const runsOf = (id: string) => runs.filter((r) => r.assistant_id === id);
  return (
    <div className="view library active" id="view-home">
      <div className="lib-top"><div className="crumbs"><span>Rusty</span><i className="ti ti-chevron-right" style={{ color: "var(--ink-300)", fontSize: 15 }} /><b>Home</b></div><div className="sp" /></div>
      <div className="lib-page">
        <div className="lib-hero">
          <div className="lh-ic"><i className="ti ti-layout-dashboard" /></div>
          <div className="lh-main"><div className="lib-eyebrow">Rusty</div><h1 className="lib-title">{greet}, {who}</h1><p className="lib-lead">Your {agents.length} agent{agents.length === 1 ? "" : "s"} handled {runs.length} runs recently{withoutPause !== null ? (withoutPause === 100 ? ", none of them needing a person" : `; ${100 - withoutPause}% needed a person`) : ""}.</p></div>
          <div className="lh-act"><button className="m-btn primary" data-flow="new-agent" onClick={() => openWizard(<CreateWizard />)}><i className="ti ti-plus" /> New agent</button></div>
        </div>
        <div className="lib-stats">
          <Stat v={compact(runs.length)} l="Recent runs" />
          <Stat v={withoutPause === null ? "—" : String(withoutPause)} u={withoutPause === null ? "" : "%"} l="Done without a person" />
          <Stat v={verifiedPct === null ? "—" : String(verifiedPct)} u={verifiedPct === null ? "" : "%"} l="Done right" />
          <Stat v={String(paused.length)} l="Waiting for you" />
        </div>
        {needsAuth.length > 0 && <div className="attn" data-attn><i className="ti ti-alert-triangle w" /><div className="ab"><b>{needsAuth[0].connector?.display_name ?? needsAuth[0].instance_id} needs reauthorization</b> — agents using it cannot read until it is fixed.</div><button className="m-btn accent sm" onClick={() => navigate({ to: "/connectors" })}>Fix now</button></div>}
        {proposed.length > 0 && <div className="attn" data-attn="proposed-notes"><i className="ti ti-brain w" /><div className="ab"><b>{proposed.length} note{proposed.length === 1 ? "" : "s"} proposed about you</b> — an agent wants to remember something about you; nothing is recalled until you accept it.</div><button className="m-btn accent sm" data-flow="review-proposed" onClick={() => open("drawer", <ProposedNotesDrawer initial={proposed} agents={assistants} onChange={loadProposed} />)}>Review</button></div>}
        {needsAuth.length === 0 && paused.length > 0 && <div className="attn" data-attn><i className="ti ti-hand-stop w" /><div className="ab"><b>{paused.length} run{paused.length === 1 ? "" : "s"} wait{paused.length === 1 ? "s" : ""} for a decision</b> — an irreversible action is held at the gate until someone decides.</div><button className="m-btn accent sm" onClick={() => open("drawer", <NotificationsDrawer />)}>Decide</button></div>}
        <div className="home-grid">
          <div>
            <div className="h-sec">Agents<span className="sp" /><button className="m-btn ghost sm" data-goto="agents" onClick={() => navigate({ to: "/agents" })}>Open builder <i className="ti ti-arrow-right" /></button></div>
            <div className="home-agents">
              {agents.map((a) => { const look = lookOf(a); const rs = runsOf(a.assistant_id); const st = statusOf(a, undefined, rs.length > 0); return (
                <div key={a.assistant_id} className="acard" data-open-agent={a.assistant_id} onClick={() => navigate({ to: "/agents/$id", params: { id: a.assistant_id } })}>
                  <div className="at"><div className="ag-tile-sm" style={{ background: COLOR_BG[look.color], color: COLORS[look.color] }}><i className={`ti ${look.icon}`} /></div><div className="an">{a.name}</div><span className={`m-badge ${st === "published" ? "good" : "warn"} sm`}><span className="dot" /> {st === "published" ? "Live" : "Draft"}</span></div>
                  <div className="ad">{a.metadata?.description || "No description yet."}</div>
                  <div className="af"><span><i className="ti ti-player-play" style={{ fontSize: 12 }} /> {rs.length} run{rs.length === 1 ? "" : "s"}</span><span className="sp" /><span>{rs[0] ? ago(rs[0].created_at) : ago(a.created_at)}</span></div>
                </div>
              ); })}
              <div className="acard new" data-flow="new-agent" onClick={() => openWizard(<CreateWizard />)}><span><i className="ti ti-plus" /> New agent</span></div>
            </div>
          </div>
          <div>
            <div className="h-sec">Activity</div>
            <div className="act-list">
              {activity.map((a, i) => <div key={i} className={`act${a.run ? " clickable" : ""}`} data-activity-run={a.run?.run_id} title={a.run ? "Open the run: what it did, the verdict, the model calls" : undefined} style={a.run ? { cursor: "pointer" } : undefined} onClick={a.run ? () => open("drawer", <RunDrawer run={a.run!} agent={a.agent ?? "An agent"} />) : undefined}><div className="ai"><i className={`ti ${a.icon}`} /></div><div className="ab">{a.text}<div className="am">{ago(a.at)}</div></div></div>)}
              {activity.length === 0 && <div className="pane-empty"><i className="ti ti-bell" /> Nothing yet.</div>}
            </div>
            <div className="h-sec" style={{ marginTop: 22 }}>Runs · 14 days</div>
            <div className="chart-card">
              <div className="spark">{days.map((d) => <div key={d.key} style={{ height: `${Math.round((d.runs / max) * 100)}%` }} title={String(d.runs)} />)}</div>
              <div className="spark-x">{[0, 3, 6, 9, 12].map((i) => <span key={i}>{days[i].label}</span>)}</div>
            </div>
          </div>
        </div>
      </div>
    </div>
  );
}

/** The notes agents proposed about the person, across agents: accept (recalled from now on) or decline (forgotten). */
function ProposedNotesDrawer({ initial, agents, onChange }: { initial: MemoryRecord[]; agents: { assistant_id: string; name: string }[]; onChange: () => void }) {
  const { toast } = useOverlay();
  const [notes, setNotes] = useState(initial);
  const text = (m: MemoryRecord) => { const v = (m.content as { value?: unknown }).value ?? m.content; if (typeof v === "string") return v; const o = (v ?? {}) as Record<string, unknown>; return typeof o.text === "string" ? o.text : JSON.stringify(v); };
  const by = (m: MemoryRecord) => { const a = m.provenance.author as { type?: string; agent_id?: string; name?: string }; if (a.type === "agent") return agents.find((x) => x.assistant_id === a.agent_id)?.name ?? a.agent_id ?? "an agent"; if (a.type === "distiller") return "the post-run review"; return a.type ?? "someone"; };
  const done = (id: string) => { setNotes((l) => l.filter((x) => x.memory_id !== id)); onChange(); };
  const accept = async (m: MemoryRecord) => { try { await acceptMemory(m.memory_id); toast("Accepted — recalled from now on", "ti-check"); done(m.memory_id); } catch (err) { toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle"); } };
  const decline = async (m: MemoryRecord) => { try { await forgetMemory(m.memory_id); toast("Declined — forgotten", "ti-trash"); done(m.memory_id); } catch (err) { toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle"); } };
  return (
    <div className="m-drawer" style={{ width: "min(560px,100%)" }}>
      <OvHead icon="ti-brain" bg="var(--cat-orange-bg)" fg="var(--cat-orange)" title="Notes proposed about you" sub={`${notes.length} waiting · nothing is recalled until you accept it`} />
      <div style={{ padding: "14px 22px" }}>
        {notes.map((m) => (
          <div key={m.memory_id} className="mem-entry" data-kind={m.kind} data-proposed-note={m.memory_id}>
            <div className="me-ic"><i className={`ti ${m.kind === "preference" ? "ti-adjustments" : "ti-bulb"}`} /></div>
            <div className="mb"><div className="mt">{text(m)}</div><div className="mm"><span className="item-tag">{m.kind}</span><span>proposed by {by(m)}</span><span>{ago(m.created_at)}</span></div></div>
            <span style={{ display: "flex", gap: 4 }}><button className="m-btn primary sm" data-flow="accept-memory" onClick={() => void accept(m)}>Accept</button><button className="m-btn ghost sm" data-flow="decline-memory" onClick={() => void decline(m)}>Decline</button></span>
          </div>
        ))}
        {notes.length === 0 && <div className="pane-empty"><i className="ti ti-brain" /> Nothing waiting — every note about you has been accepted or declined.</div>}
      </div>
    </div>
  );
}

function Stat({ v, u, l }: { v: string; u?: string; l: string }) {
  return <div className="lib-stat"><div className="ls-v">{v}{u && <span className="u">{u}</span>}</div><div className="ls-l">{l}</div></div>;
}
