import { useEffect, useMemo, useState } from "react";
import { importSkills, skillLibrary, type ServerSkill, type SkillImportReport, type SkillLibrarySource } from "../../engine/net/client";
import { OvHead } from "../overlay";
import { useServer } from "../../engine/net/server";
import { useOverlay } from "../overlay";
import { SkillEditorDrawer } from "../flows/agentFlows";
import { COLORS, COLOR_BG, compact, connectorOf, isPlatform } from "../data";
import { firstSentence, plainName } from "../agents/words";

const PALETTE = ["plum", "teal", "blue", "amber", "rose", "orange"];
const hash = (s: string) => [...s].reduce((h, c) => (h * 31 + c.charCodeAt(0)) >>> 0, 7);
/** A skill's category: the system its tools speak to, else the platform's own, else none. */
const categoryOf = (s: ServerSkill) => { const c = (s.allowed_tools ?? []).map(connectorOf).find((x) => x); return c ? plainName(c) : (s.allowed_tools ?? []).length ? "Platform" : "Steps only"; };

/** Skills: every procedure on the server, grouped by the system it works, with who follows it. */
export function SkillsView() {
  const { open } = useOverlay();
  const skills = useServer((s) => s.skills);
  const assistants = useServer((s) => s.assistants);
  const runs = useServer((s) => s.runs);
  const [q, setQ] = useState("");
  const followers = (name: string) => assistants.filter((a) => !a.archived_at && (a.config?.studio_intent?.skills ?? []).includes(name));
  const runsOf = (name: string) => { const ids = new Set(followers(name).map((a) => a.assistant_id)); return runs.filter((r) => r.assistant_id && ids.has(r.assistant_id)).length; };
  const shown = skills.filter((s) => !q || `${s.name} ${s.description}`.toLowerCase().includes(q.toLowerCase()));
  const cats = useMemo(() => { const m = new Map<string, ServerSkill[]>(); for (const s of shown) { const c = categoryOf(s); m.set(c, [...(m.get(c) ?? []), s]); } return [...m.entries()].sort((a, b) => b[1].length - a[1].length); }, [shown]);
  const using = new Set(assistants.filter((a) => !a.archived_at && !isPlatform(a) && (a.config?.studio_intent?.skills ?? []).length).map((a) => a.assistant_id));
  const openSkill = (s?: ServerSkill) => open("drawer", <SkillEditorDrawer skill={s} isNew={!s} onSaved={() => void useServer.getState().refresh()} />);
  return (
    <div className="view library active" id="view-skills">
      <div className="lib-top"><div className="crumbs"><span>Rusty</span><i className="ti ti-chevron-right" style={{ color: "var(--ink-300)", fontSize: 15 }} /><b>Skills</b></div><div className="sp" /><div className="lib-search" style={{ maxWidth: 240 }}><i className="ti ti-search" /><input placeholder="Search…" data-libsearch value={q} onChange={(e) => setQ(e.target.value)} /></div></div>
      <div className="lib-page">
        <div className="lib-hero">
          <div className="lh-ic" style={{ background: "var(--cat-plum-bg)", color: "var(--cat-plum)" }}><i className="ti ti-puzzle" /></div>
          <div className="lh-main"><div className="lib-eyebrow">Library</div><h1 className="lib-title">Skills</h1><p className="lib-lead">Reusable procedures that bundle a method with the tools it needs. Compose once, attach to any agent.</p></div>
          <div className="lh-act"><button className="m-btn secondary" onClick={() => open("drawer", <SkillLibraryDrawer onImported={() => void useServer.getState().refresh()} />)}><i className="ti ti-library" /> Browse library</button> <button className="m-btn primary" data-new="skill" onClick={() => openSkill()}><i className="ti ti-plus" /> New skill</button></div>
        </div>
        <div className="lib-stats">
          <Stat v={String(skills.length)} l="Total skills" />
          <Stat v={String(using.size)} l="Agents using skills" />
          <Stat v={compact(runs.filter((r) => r.assistant_id && using.has(r.assistant_id)).length)} l="Runs through skills (newest listed)" />
          <Stat v={String(cats.length)} l="Categories" />
        </div>
        {cats.map(([c, list]) => (
          <div key={c}>
            <div className="lib-cat"><span>{c}</span><span className="ln" /><span className="gc">{list.length}</span></div>
            <div className="lib-grid">
              {list.map((s) => { const color = PALETTE[hash(s.name) % PALETTE.length]; const f = followers(s.name); return (
                <div key={s.name} className="lcard" data-detail={s.name} onClick={() => openSkill(s)}>
                  <div className="lcard-top"><div className="lcard-ic" style={{ background: COLOR_BG[color], color: COLORS[color] }}><i className="ti ti-puzzle" /></div><div><div className="lcard-title" title={s.name}>{plainName(s.name)}</div>{f.length > 0 && <div className="lcard-sub">used by {f.length} agent{f.length === 1 ? "" : "s"}</div>}</div></div>
                  <div className="lcard-desc" title={s.description}>{firstSentence(s.description ?? "", 160)}</div>
                  <div className="lcard-foot"><span className="mu"><i className="ti ti-player-play" style={{ fontSize: 13 }} /> {runsOf(s.name)} runs</span>{(s.allowed_tools ?? []).length > 0 && <span className="mu" style={{ marginLeft: 10 }}><i className="ti ti-tool" style={{ fontSize: 13 }} /> {(s.allowed_tools ?? []).length} tools</span>}<span className="sp" /><UsedStack agents={f.map((a) => a.name)} /></div>
                </div>
              ); })}
            </div>
          </div>
        ))}
        {skills.length === 0 && <div className="thread-empty" style={{ padding: 40 }}><i className="ti ti-puzzle" />No skill yet. Compose one — any agent can follow it.</div>}
      </div>
    </div>
  );
}

/** Who uses it: the first letters of up to four agents, then the count. */
export function UsedStack({ agents }: { agents: string[] }) {
  if (!agents.length) return <span style={{ color: "var(--ink-400)" }}>Unused</span>;
  return <><span className="used-stack">{agents.slice(0, 4).map((n) => <span key={n} className="ua" style={{ background: COLORS[PALETTE[hash(n) % PALETTE.length]] }} title={n}>{n.slice(0, 1).toUpperCase()}</span>)}</span><span>{agents.length} agent{agents.length > 1 ? "s" : ""}</span></>;
}

function Stat({ v, u, l }: { v: string; u?: string; l: string }) {
  return <div className="lib-stat"><div className="ls-v">{v}{u && <span className="u">{u}</span>}</div><div className="ls-l">{l}</div></div>;
}

/** The server's skill library: published sources of SKILL.md folders, imported whole. */
function SkillLibraryDrawer({ onImported }: { onImported: () => void }) {
  const { toast } = useOverlay();
  const [sources, setSources] = useState<SkillLibrarySource[]>([]);
  const [url, setUrl] = useState("");
  const [busy, setBusy] = useState<string | null>(null);
  const [report, setReport] = useState<SkillImportReport | null>(null);
  useEffect(() => { skillLibrary().then(setSources).catch(() => {}); }, []);
  async function doImport(u: string, subpath?: string | null) {
    setBusy(u);
    try { const r = await importSkills({ url: u, ...(subpath ? { subpath } : {}) }); setReport(r); toast(`${r.imported.length} of ${r.found} skills imported`, "ti-download"); onImported(); }
    catch (err) { toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle"); }
    finally { setBusy(null); }
  }
  return (
    <div className="m-drawer">
      <OvHead icon="ti-library" bg="var(--cat-plum-bg)" fg="var(--cat-plum)" title="Skill library" sub="Published skill packs; each SKILL.md becomes a skill any agent can follow." />
      <div className="ov-body">
        {sources.map((s) => (
          <div key={s.id} className="item">
            <div className="item-ic" style={{ background: "var(--cat-plum-bg)", color: "var(--cat-plum)" }}><i className="ti ti-puzzle" /></div>
            <div className="item-body"><div className="item-name">{s.name}</div><div className="item-desc">{s.description}</div><div className="item-meta"><span className="item-tag">{s.publisher}</span>{s.license && <span className="item-tag">{s.license}</span>}</div></div>
            <button className="m-btn secondary sm" disabled={busy === s.url} onClick={() => void doImport(s.url, s.subpath)}>{busy === s.url ? "Importing…" : "Import"}</button>
          </div>
        ))}
        {sources.length === 0 && <div className="m-hint">The library names no sources on this server.</div>}
        <div className="cat-label" style={{ marginTop: 18 }}><span>From a repository</span><span className="ln" /></div>
        <div style={{ display: "flex", gap: 8 }}><input className="m-input" placeholder="https://github.com/org/skills" value={url} onChange={(e) => setUrl(e.target.value)} style={{ fontFamily: "var(--font-mono)", fontSize: 12 }} /><button className="m-btn secondary sm" disabled={!url.trim() || !!busy} onClick={() => void doImport(url.trim())}><i className="ti ti-download" /> Import</button></div>
        {report && <><div className="cat-label" style={{ marginTop: 18 }}><span>Last import</span><span className="ln" /></div><div className="kv"><span className="k">Source</span><span className="v mono">{report.source}</span><span className="k">Found</span><span className="v">{report.found}</span><span className="k">Imported</span><span className="v">{report.imported.map((i) => `${i.name} r${i.revision}`).join(", ") || "none"}</span>{report.skipped.length > 0 && <><span className="k">Skipped</span><span className="v">{report.skipped.map((x) => `${x.path}: ${x.reason}`).join("; ")}</span></>}</div></>}
      </div>
      <div className="ov-foot"><div className="sp" /><button className="m-btn secondary" data-close>Done</button></div>
    </div>
  );
}
