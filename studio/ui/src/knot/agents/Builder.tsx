import { useEffect, useMemo, useState } from "react";
import { useNavigate, useParams } from "@tanstack/react-router";
import { useServer } from "../../engine/net/server";
import { useEngine } from "../../engine/state";
import { createAssistant, listApprovals, setAssistantArchived, type Assistant } from "../../engine/net/client";
import { Badge, openMenu, useOverlay } from "../overlay";
import { COLORS, COLOR_BG, isPlatform, lookOf, slug, statusOf } from "../data";
import { DEFAULT_STEPS, useAgent } from "./useAgent";
import { ConfigColumn } from "./config";
import { TestPanel } from "./TestPanel";
import { useProposals } from "./proposals";
import { IdentityModal, PreviewModal, PublishModal, VersionsDrawer } from "../flows/agentFlows";
import { CreateWizard } from "../flows/wizard";

/** Agents — the builder view: the rail, the workspace, the test panel. */
export function BuilderView() {
  const params = useParams({ strict: false }) as { id?: string };
  const navigate = useNavigate();
  const { open, toast, openWizard } = useOverlay();
  const assistants = useServer((s) => s.assistants);
  const reach = useServer((s) => s.reach);
  const theme = useEngine((s) => s.theme);
  const toggleTheme = useEngine((s) => s.toggleTheme);
  const [filter, setFilter] = useState("");
  const [searching, setSearching] = useState(false);
  // Remove is reversible: removed agents wait here, folded, until someone restores one.
  const [showRemoved, setShowRemoved] = useState(false);

  const live = useMemo(() => assistants.filter((a) => !a.archived_at), [assistants]);
  const removed = useMemo(() => assistants.filter((a) => a.archived_at && !isPlatform(a)), [assistants]);
  async function restore(a: Assistant) {
    try { await setAssistantArchived(a.assistant_id, a.active_version_id ?? "", false); await useServer.getState().refresh(); toast(`${a.name} restored`, "ti-robot"); navigate({ to: "/agents/$id", params: { id: a.assistant_id } }); }
    catch (err) { toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle"); }
  }
  const yours = useMemo(() => live.filter((a) => !isPlatform(a)), [live]);
  const listed = useMemo(() => (yours.length ? yours : live), [yours, live]);
  const selectedId = params.id ?? listed[0]?.assistant_id ?? null;
  const agent = useMemo(() => assistants.find((a) => a.assistant_id === selectedId) ?? null, [assistants, selectedId]);
  useEffect(() => { if (!params.id && listed[0]) navigate({ to: "/agents/$id", params: { id: listed[0].assistant_id }, replace: true }); }, [params.id, listed, navigate]);

  const model = useAgent(agent);
  const proposals = useProposals(agent, (merge) => model.edit((d) => ({ ...d, intent: merge(d.intent) })));
  const { draft, edit, save, savedAt, status, versions, publish, problem } = model;
  useEffect(() => { if (problem) toast(problem, "ti-alert-triangle"); }, [problem]); // eslint-disable-line react-hooks/exhaustive-deps

  const shown = listed.filter((a) => a.name.toLowerCase().includes(filter.toLowerCase()));
  const runs = useServer((s) => s.runs);
  const ran = (a: Assistant) => runs.some((r) => r.assistant_id === a.assistant_id);
  const drafts = shown.filter((a) => (a.assistant_id === agent?.assistant_id ? status : statusOf(a, undefined, ran(a))) === "draft");
  const published = shown.filter((a) => !drafts.includes(a));

  const railLink = (a: Assistant) => {
    const look = lookOf(a);
    const st = a.assistant_id === agent?.assistant_id ? status : statusOf(a, undefined, ran(a));
    return (
      <div key={a.assistant_id} className={`ag-link${a.assistant_id === agent?.assistant_id ? " active" : ""}`} data-agent={a.assistant_id} onClick={() => navigate({ to: "/agents/$id", params: { id: a.assistant_id } })}>
        <div className="ag-tile-sm" style={{ background: COLOR_BG[look.color], color: COLORS[look.color] }}><i className={`ti ${look.icon}`} /></div>
        <div className="nm">{a.name}</div>
        <span className="st" style={{ background: st === "published" ? "var(--good)" : "var(--warn)" }} />
      </div>
    );
  };

  // The label counts up, so "Saved 1s ago" does not sit there for an hour.
  const [clock, setClock] = useState(Date.now());
  useEffect(() => { const t = setInterval(() => setClock(Date.now()), 5000); return () => clearInterval(t); }, []);
  const since = savedAt ? Math.max(1, Math.round((clock - savedAt) / 1000)) : 0;
  const saveWord = save === "saving" ? "Saving…" : save === "unsaved" ? "Unsaved" : save === "failed" ? "Not saved" : savedAt ? `Saved ${since < 60 ? `${since}s` : since < 3600 ? `${Math.round(since / 60)}m` : `${Math.round(since / 3600)}h`} ago` : "Saved";
  const look = agent ? lookOf({ name: draft?.name ?? agent.name, metadata: draft?.metadata ?? agent.metadata }) : null;

  return (
    <div className="view builder-view active" id="view-agents">
      <aside className="rail">
        <div className="rail-head">
          <div className="rail-title">Agents</div>
          <div className="m-btn icon ghost sm" title="Search" data-flow="rail-search" onClick={() => { setSearching((s) => !s); setFilter(""); }}><i className="ti ti-search" /></div>
        </div>
        <div className="rail-search" hidden={!searching}><div className="lib-search"><i className="ti ti-search" /><input placeholder="Filter agents…" value={filter} onChange={(e) => setFilter(e.target.value)} data-rail-filter /></div></div>
        <div className="rail-new">
          <button className="m-btn primary sm block" data-flow="new-agent" onClick={() => openWizard(<CreateWizard />)}><i className="ti ti-plus" /> New agent</button>
        </div>
        <div className="rail-list">
          {drafts.length > 0 && <div className="rl-group-label"><i className="ti ti-chevron-down" style={{ fontSize: 13 }} /> Drafts <span className="gc">{drafts.length}</span></div>}
          {drafts.map(railLink)}
          {published.length > 0 && <div className="rl-group-label"><i className="ti ti-chevron-down" style={{ fontSize: 13 }} /> Published <span className="gc">{published.length}</span></div>}
          {published.map(railLink)}
          {reach === "up" && listed.length === 0 && <div className="thread-empty" style={{ padding: 18 }}><i className="ti ti-robot" />No agent yet. Create one.</div>}
          {removed.length > 0 && <div className="rl-group-label" style={{ cursor: "pointer" }} data-rail-removed onClick={() => setShowRemoved((v) => !v)}><i className={`ti ti-chevron-${showRemoved ? "down" : "right"}`} style={{ fontSize: 13 }} /> Removed <span className="gc">{removed.length}</span></div>}
          {showRemoved && removed.filter((a) => a.name.toLowerCase().includes(filter.toLowerCase())).map((a) => (
            <div key={a.assistant_id} className="ag-link" style={{ opacity: 0.75, cursor: "default", gridTemplateColumns: "1fr auto" }} data-removed-agent={a.assistant_id}>
              <div className="nm" title={a.name} style={{ flex: 1, minWidth: 0, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>{a.name}</div>
              <button className="m-btn ghost sm" style={{ flex: "none", padding: "0 6px" }} onClick={() => void restore(a)}>Restore</button>
            </div>
          ))}
        </div>
        <div className="rail-foot">
          <div className="theme-toggle" id="themeToggle">
            <button data-theme-btn="light" className={theme === "light" ? "on" : ""} title="Light" onClick={() => theme !== "light" && toggleTheme()}><i className="ti ti-sun" /></button>
            <button data-theme-btn="dark" className={theme === "dark" ? "on" : ""} title="Dark" onClick={() => theme !== "dark" && toggleTheme()}><i className="ti ti-moon" /></button>
          </div>
          <span className="sb-foot-meta">{listed.length} agent{listed.length === 1 ? "" : "s"}</span>
        </div>
      </aside>

      <div className="work">
        <div className="work-top">
          <div className="crumbs">
            <span>Agents</span>
            <i className="ti ti-chevron-right sep" />
            <b>{draft?.name ?? agent?.name ?? "—"}</b>
            {agent && (status === "published" ? <Badge tone="good">Published</Badge> : <Badge tone="warn">Draft</Badge>)}
            {agent && !!(agent.metadata as { awaiting_review?: unknown } | undefined)?.awaiting_review && <span title="Spawned by an agent; it answers no one until a person publishes it"><Badge tone="warn">Awaiting review</Badge></span>}
          </div>
          <div className="top-sp" />
          {agent && <div className={`save-state${save === "saving" ? " saving" : ""}`} id="saveState"><span className="d" style={save === "failed" ? { background: "var(--bad-dot)" } : save === "unsaved" ? { background: "var(--warn-dot)" } : undefined} /> <span className="txt">{saveWord}</span></div>}
          <div className="top-actions">
            <button className="m-btn ghost sm" data-flow="versions" disabled={!agent} onClick={() => agent && open("drawer", <VersionsDrawer agent={agent} versions={versions} onRestore={(v) => { model.edit((d) => ({ ...d, name: v.name || d.name, intent: v.config?.studio_intent ?? d.intent, metadata: { ...d.metadata, ...(v.metadata ?? {}) } })); }} />)}><i className="ti ti-history" /> Versions</button>
            <button className="m-btn secondary sm" data-flow="preview" disabled={!agent} onClick={() => agent && draft && open("modal", <PreviewModal agent={agent} draft={draft} />)}><i className="ti ti-eye" /> Preview</button>
            <button className="m-btn icon ghost sm" data-flow="overflow" title="More" disabled={!agent} onClick={(e) => agent && openMenu(e.currentTarget, [{ label: "Duplicate", icon: "ti-copy", run: async () => { if (!draft) return; try { const made = await createAssistant({ name: `${draft.name} copy`, graph: agent.graph, // A copy starts as a draft: the original's published mark does not travel.
                metadata: { ...draft.metadata, description: draft.description, studio: { ...(draft.metadata.studio ?? {}), published_at: undefined } } as never, config: { studio_intent: draft.intent, recursion_limit: draft.recursion_limit ?? DEFAULT_STEPS } }); await useServer.getState().refresh(); toast(`${draft.name} copy created as a draft`, "ti-copy"); navigate({ to: "/agents/$id", params: { id: made.assistant_id } }); } catch (err) { toast(err instanceof Error ? err.message : "the copy was not made", "ti-alert-triangle"); } } }, { label: "Remove", icon: "ti-trash", danger: true, run: async () => { try { const waiting = (await listApprovals("pending").catch(() => [])).filter((p) => p.assistant_id === agent.assistant_id); if (waiting.length) { toast(`${waiting.length} run${waiting.length === 1 ? "" : "s"} of ${agent.name} wait${waiting.length === 1 ? "s" : ""} at the gate — decide them under Notifications first`, "ti-hand-stop"); return; } await setAssistantArchived(agent.assistant_id, agent.active_version_id ?? "", true); toast(`Removed "${agent.name}"`, "ti-trash"); await useServer.getState().refresh(); navigate({ to: "/agents" }); } catch (err) { toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle"); } } }])}><i className="ti ti-dots" /></button>
            <button className="m-btn primary sm" data-flow="publish" disabled={!agent} onClick={() => agent && draft && open("modal", <PublishModal agent={agent} draft={draft} status={status} onPublish={async (reason, note) => { try { await publish(reason, note); toast(`${draft.name} published`, "ti-rocket"); return true; } catch (err) { toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle"); return false; } }} />)}><i className="ti ti-rocket" /> Publish</button>
          </div>
        </div>

        <div className="config">
          <div className="config-inner">
            {agent && draft && look && (
              <div className="hero">
                <div className="hero-tile" data-flow="identity" title="Edit identity" style={{ background: COLOR_BG[look.color], color: COLORS[look.color] }} onClick={() => open("modal", <IdentityModal draft={draft} look={look} onSave={(next) => edit((d) => ({ ...d, ...next }))} />)}>
                  <i className={`ti ${look.icon}`} />
                  <span className="edit-dot"><i className="ti ti-pencil" /></span>
                </div>
                <div className="hero-main">
                  <HeroName name={draft.name} onRename={(name) => edit((d) => ({ ...d, name }))} />
                  <div className="hero-handle">@<b>{slug(draft.name)}</b> · {draft.description || "No description yet."}</div>
                </div>
              </div>
            )}
            {agent && draft ? <ConfigColumn agent={agent} draft={draft} edit={edit} status={status} proposals={proposals} /> : (
              <div className="thread-empty" style={{ padding: 40 }}><i className="ti ti-robot" />{reach === "up" ? "Pick an agent, or create one." : "Reading the server…"}</div>
            )}
          </div>
        </div>

        {agent && draft && <TestPanel agent={agent} draft={draft} look={look!} status={status} proposals={proposals} />}
      </div>
    </div>
  );
}

/** The agent's name, edited in place. An input rather than a contentEditable:
 * React re-renders the hero while a person types, and a contentEditable's text
 * is React's to overwrite — the typing was being discarded before the blur. */
function HeroName({ name, onRename }: { name: string; onRename: (name: string) => void }) {
  const [text, setText] = useState(name);
  const [editing, setEditing] = useState(false);
  useEffect(() => { if (!editing) setText(name); }, [name, editing]);
  const commit = () => { setEditing(false); const next = text.trim(); if (next && next !== name) onRename(next); else setText(name); };
  return (
    <input className="hero-name" value={text} spellCheck={false} aria-label="Agent name" size={Math.max(8, text.length)}
      onFocus={() => setEditing(true)}
      onChange={(e) => setText(e.target.value)}
      onBlur={commit}
      onKeyDown={(e) => { if (e.key === "Enter") { e.preventDefault(); e.currentTarget.blur(); } else if (e.key === "Escape") { setText(name); setEditing(false); e.currentTarget.blur(); } }} />
  );
}
