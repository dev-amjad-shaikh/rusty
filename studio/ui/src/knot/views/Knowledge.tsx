import { useEffect, useState } from "react";
import { artifactText, correctKnowledgeSource, getKnowledgeSource, knowledgeChunk, knowledgeRetentionApply, knowledgeRetentionPlan, listArtifacts, listKnowledgeSources, listKnowledgeEdits, acceptKnowledgeEdit, declineKnowledgeEdit, type KnowledgeEdit, listKnowledgeConflicts, fileKnowledgeConflict, ruleKnowledgeConflict, type KnowledgeConflict, retireKnowledgeSource, queryKnowledge, registerKnowledgeSource, fetchPageForSource, skillFreshness, type KnowledgeChunkRecord, type KnowledgeScope, type KnowledgeSourceSummary, type KnowledgeTombstone, type RunArtifact, type SkillFreshness } from "../../engine/net/client";
import { useServer } from "../../engine/net/server";
import { Badge, OvHead, useOverlay } from "../overlay";
import { ago, slug } from "../data";
import { renderMarkdown } from "../../engine/text/markdown";

/**
 * Knowledge: the governed sources agents retrieve from — posted once, chunked and indexed on
 * the server, answered with citations — plus what else agents read: references skills learned,
 * artifacts runs filed, memory they wrote.
 */

export function KnowledgeView() {
  const { open } = useOverlay();
  const skills = useServer((s) => s.skills);
  const assistants = useServer((s) => s.assistants);
  const [sources, setSources] = useState<KnowledgeSourceSummary[]>([]);
  const [tombs, setTombs] = useState<KnowledgeTombstone[]>([]);
  const [refs, setRefs] = useState<(SkillFreshness & { skill: string })[]>([]);
  const [artifacts, setArtifacts] = useState<RunArtifact[]>([]);
  const [unavailable, setUnavailable] = useState<string | null>(null);
  const [edits, setEdits] = useState<KnowledgeEdit[]>([]);
  const [conflicts, setConflicts] = useState<KnowledgeConflict[]>([]);
  const [flagging, setFlagging] = useState(false);
  const [showRemoved, setShowRemoved] = useState(false);
  const reload = () => {
    listKnowledgeSources().then((r) => { setSources(r.sources); setTombs(r.tombstones); setUnavailable(null); }).catch((e) => setUnavailable(e instanceof Error ? e.message : "the knowledge store did not answer"));
    listKnowledgeEdits().then((r) => setEdits(r.edits)).catch(() => setEdits([]));
    listKnowledgeConflicts().then((r) => setConflicts(r.conflicts)).catch(() => setConflicts([]));
    listArtifacts().then(setArtifacts).catch(() => {});
  };
  useEffect(reload, []);
  useEffect(() => { Promise.all(skills.map((s) => skillFreshness(s.name).then((f) => (f.learned && f.freshness ? { ...f.freshness, skill: s.name } : null)).catch(() => null))).then((all) => setRefs(all.filter((x): x is SkillFreshness & { skill: string } => !!x))); }, [skills]);
  // Who may look a source up, in a person's words.
  const usedBy = (sc: KnowledgeScope) => (sc.scope === "agent" ? assistants.find((a) => a.assistant_id === sc.id)?.name ?? "One agent" : sc.scope === "tenant" ? "All agents" : "Some agents");
  const what = (s: KnowledgeSourceSummary) => ({ organization: "Our own", vendor: "Vendor", generic: "General guidance" } as Record<string, string>)[s.provenance ?? "organization"] ?? "Our own";
  const status = (s: KnowledgeSourceSummary): [string, "good" | "warn"] => s.retention.policy === "ttl" ? (new Date(s.retention.expires_at).getTime() < Date.now() ? ["Expired", "warn"] : [`Until ${new Date(s.retention.expires_at).toLocaleDateString()}`, "good"]) : ["Ready", "good"];
  // One row per article name: the store keeps every version, the row shows the newest with its count.
  const articles = [...artifacts.reduce((m, a) => { const k = a.name ?? a.artifact_id; const have = m.get(k); if (!have || (a.versions?.length ?? 1) > (have.versions?.length ?? 1)) m.set(k, a); return m; }, new Map<string, RunArtifact>()).values()];
  const ACRONYMS = new Set(["vpn", "mfa", "sso", "it", "hr", "os", "pc", "api", "sla", "ost", "pst", "dns", "vip", "q1", "q2", "q3", "q4"]);
  const title = (name: string) => name.split(/[-_\s]+/).map((w, n) => ACRONYMS.has(w.toLowerCase()) ? w.toUpperCase() : n === 0 ? w.charAt(0).toUpperCase() + w.slice(1) : w).join(" ");
  const byAgents = articles.length + refs.length;
  return (
    <div className="view library active" id="view-knowledge">
      <div className="lib-top"><div className="crumbs"><span>Rustynome</span><i className="ti ti-chevron-right" style={{ color: "var(--ink-300)", fontSize: 15 }} /><b>Knowledge</b></div><div className="sp" /></div>
      <div className="lib-page">
        <div className="lib-hero">
          <div className="lh-ic" style={{ background: "var(--cat-rose-bg)", color: "var(--cat-rose)" }}><i className="ti ti-books" /></div>
          <div className="lh-main"><div className="lib-eyebrow">Library</div><h1 className="lib-title">Knowledge</h1><p className="lib-lead">What your agents look things up in. Add a document or a web page, and agents cite it when they answer.</p></div>
          <div className="lh-act"><button className="m-btn secondary" onClick={() => open("modal", <QueryModal />)}><i className="ti ti-search" /> Try a question</button> <button className="m-btn primary" data-new="knowledge" onClick={() => open("drawer", <AddSourceDrawer onAdded={reload} />)}><i className="ti ti-plus" /> Add source</button></div>
        </div>
        {unavailable && <div className="m-alert"><i className="ti ti-alert-triangle" style={{ color: "var(--bad)", fontSize: 16 }} /><div className="a-body"><div className="a-text">{unavailable}</div></div></div>}
        <div className="lib-stats">
          <Stat v={String(sources.length)} l={sources.length === 1 ? "Source" : "Sources"} />
          <Stat v={String(byAgents)} l="Written by agents" />
        </div>
        {(conflicts.some((c) => c.state === "open") || flagging) && <Conflicts conflicts={conflicts.filter((c) => c.state === "open")} ruled={conflicts.filter((c) => c.state === "ruled").length} sources={sources} flagging={flagging} setFlagging={setFlagging} agentName={(id) => assistants.find((a) => a.assistant_id === id)?.name ?? id} onChanged={reload} />}
        {edits.some((e) => e.state === "waiting") && <SuggestedEdits edits={edits.filter((e) => e.state === "waiting")} agentName={(id) => assistants.find((a) => a.assistant_id === id)?.name ?? id} onChanged={reload} />}
        <div className="lib-table-wrap"><table className="m-table" data-sources>
          <thead><tr><th>Source</th><th>What it is</th><th>Who uses it</th><th>Updated</th><th>Status</th></tr></thead>
          <tbody>
            {sources.map((s) => { const [label, tone] = status(s); return (
              <tr key={s.source_id} className="clickable" onClick={() => open("drawer", <SourceDrawer s={s} onChanged={reload} />)}>
                <td><div className="tl-row-name"><div className="tl-ic" style={{ background: "var(--cat-rose-bg)", color: "var(--cat-rose)" }}><i className="ti ti-file-text" /></div><span style={{ fontWeight: 600 }}>{s.title}</span></div></td>
                <td style={{ color: "var(--ink-600)" }}>{what(s)}</td>
                <td style={{ color: "var(--ink-600)" }}>{usedBy(s.scope)}</td>
                <td style={{ color: "var(--ink-500)" }}>{ago(s.created_at)}</td>
                <td><Badge tone={tone}>{label}</Badge></td>
              </tr>
            ); })}
            {showRemoved && tombs.map((t) => (
              <tr key={t.source_id} data-removed>
                <td><div className="tl-row-name"><div className="tl-ic" style={{ background: "var(--bg-muted)", color: "var(--ink-500)" }}><i className="ti ti-file-off" /></div><span style={{ color: "var(--ink-500)" }}>{t.title}</span></div></td>
                <td /><td style={{ color: "var(--ink-500)" }}>{usedBy(t.scope)}</td><td style={{ color: "var(--ink-500)" }}>{ago(t.purged_at)}</td><td><span className="item-tag">Removed</span></td>
              </tr>
            ))}
            {sources.length === 0 && <tr><td colSpan={5} style={{ color: "var(--ink-500)", textAlign: "center", padding: 24 }}>Nothing here yet. Add a document or a web page.</td></tr>}
          </tbody>
        </table></div>
        <div style={{ marginTop: 10, display: "flex", gap: 8, alignItems: "center" }}>
          {tombs.length > 0 && <button className="m-btn ghost sm" onClick={() => setShowRemoved((v) => !v)}>{showRemoved ? "Hide removed" : `Show removed (${tombs.length})`}</button>}
          <span className="sp" />
          <button className="m-btn ghost sm" data-flow="flag-conflict" disabled={sources.length < 2} onClick={() => setFlagging(true)}><i className="ti ti-arrows-diff" /> Two sources disagree</button>
          <button className="m-btn ghost sm" onClick={() => open("modal", <RetentionModal onApplied={reload} />)}><i className="ti ti-clock-off" /> Clear out expired</button>
        </div>
        {byAgents > 0 && <>
          <div className="cat-label" style={{ marginTop: 26 }}><span>Written by agents</span><span className="ln" /><span className="gc">{byAgents}</span></div>
          <div className="lib-table-wrap"><table className="m-table" data-written>
            <thead><tr><th>Title</th><th>What it is</th><th>Updated</th><th>Status</th></tr></thead>
            <tbody>
              {articles.map((a) => (
                <tr key={`a:${a.name ?? a.artifact_id}`} className="clickable" onClick={() => open("drawer", <ArtifactDrawer a={a} />)}>
                  <td style={{ fontWeight: 600 }}>{title(a.name ?? "Untitled")}</td>
                  <td style={{ color: "var(--ink-600)" }}>Article</td>
                  <td style={{ color: "var(--ink-500)" }}>{a.versions?.length ? ago(a.versions[a.versions.length - 1].committed_at) : "—"}</td>
                  <td><span className="item-tag">{(a.versions?.length ?? 1) > 1 ? `${a.versions!.length} versions` : "First version"}</span></td>
                </tr>
              ))}
              {refs.map((r) => (
                <tr key={`r:${r.skill}:${r.reference}`} className="clickable" onClick={() => open("drawer", <ReferenceDrawer r={r} />)}>
                  <td style={{ fontWeight: 600 }}>{title(r.reference)}</td>
                  <td style={{ color: "var(--ink-600)" }}>Learned for {r.skill}</td>
                  <td style={{ color: "var(--ink-500)" }}>{ago(r.checked_at ?? r.learned_at)}</td>
                  <td><Badge tone={r.stale ? "warn" : "good"}>{r.stale ? "Out of date" : "Current"}</Badge></td>
                </tr>
              ))}
            </tbody>
          </table></div>
        </>}
      </div>
    </div>
  );
}

/** Conflicts: two sources disagree on a claim. Until a person rules, every search hit from either source is marked contested and agents state neither side as fact; the ruling names the source that stands. */
function Conflicts({ conflicts, ruled, sources, flagging, setFlagging, agentName, onChanged }: { conflicts: KnowledgeConflict[]; ruled: number; sources: KnowledgeSourceSummary[]; flagging: boolean; setFlagging: (v: boolean) => void; agentName: (id: string) => string; onChanged: () => void }) {
  const { toast } = useOverlay();
  const say = (err: unknown) => toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle");
  const [notes, setNotes] = useState<Record<string, string>>({});
  const [f, setF] = useState({ source_a: "", source_b: "", claim: "", a_says: "", b_says: "", why: "" });
  const by = (c: KnowledgeConflict) => c.filed_by.kind === "agent" ? agentName(c.filed_by.agent_id ?? "") : String(c.filed_by.name ?? c.filed_by.principal_id ?? "a person");
  const rule = (c: KnowledgeConflict, stands: string, title: string) => ruleKnowledgeConflict(c.conflict_id, stands, notes[c.conflict_id]).then(() => { toast(`Ruled — ${title} stands`, "ti-gavel"); onChanged(); }).catch(say);
  const file = () => fileKnowledgeConflict(f).then((r) => { toast(r.created ? "Flagged — agents state neither side as fact until you rule" : "That pair is already flagged", "ti-arrows-diff"); setFlagging(false); setF({ source_a: "", source_b: "", claim: "", a_says: "", b_says: "", why: "" }); onChanged(); }).catch(say);
  return (
    <div className="m-card" style={{ padding: 14, marginBottom: 14 }} data-conflicts>
      <div className="cat-label"><span>Conflicts</span><span className="ln" /><span className="gc">{conflicts.length}</span></div>
      <div className="m-hint" style={{ marginBottom: 8 }}>Two sources disagree. Until you rule, every search hit from either one is marked contested and agents state neither side as fact. Rule which stands; the other's hits then say it was overruled on this claim.{ruled > 0 ? ` ${ruled} ruled before.` : ""}</div>
      {conflicts.map((c) => (
        <div key={c.conflict_id} className="scope-row" data-conflict={c.conflict_id} style={{ display: "block" }}>
          <div style={{ display: "flex", alignItems: "baseline", gap: 8 }}><span style={{ fontWeight: 600 }}>{c.claim}</span><span className="item-tag">{by(c)}</span><span style={{ color: "var(--ink-500)", fontSize: 12 }}>{ago(c.filed_at)}</span></div>
          <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: 10, marginTop: 6 }}>
            <div className="m-hint" data-side="a"><b>{c.title_a}</b> says: {c.a_says}</div>
            <div className="m-hint" data-side="b"><b>{c.title_b}</b> says: {c.b_says}</div>
          </div>
          {c.why && <div className="m-hint" style={{ marginTop: 4 }}><b>Found:</b> {c.why}</div>}
          <div style={{ display: "flex", gap: 8, marginTop: 8, alignItems: "center" }}>
            <input className="m-input" placeholder="Why — optional; agents read it with the ruling" value={notes[c.conflict_id] ?? ""} onChange={(e) => setNotes((n) => ({ ...n, [c.conflict_id]: e.target.value }))} data-flow="ruling-note" />
            <button className="m-btn secondary sm" data-flow="rule-a" onClick={() => void rule(c, c.source_a, c.title_a)}>{c.title_a} stands</button>
            <button className="m-btn secondary sm" data-flow="rule-b" onClick={() => void rule(c, c.source_b, c.title_b)}>{c.title_b} stands</button>
          </div>
        </div>
      ))}
      {flagging && (
        <div className="scope-row" style={{ display: "block" }} data-flag-form>
          <div className="frow two"><div className="fld"><label className="fld-label">One source</label><select className="m-input" value={f.source_a} onChange={(e) => setF({ ...f, source_a: e.target.value })}><option value="">Pick…</option>{sources.map((s) => <option key={s.source_id} value={s.source_id}>{s.title}</option>)}</select></div><div className="fld"><label className="fld-label">The other</label><select className="m-input" value={f.source_b} onChange={(e) => setF({ ...f, source_b: e.target.value })}><option value="">Pick…</option>{sources.filter((s) => s.source_id !== f.source_a).map((s) => <option key={s.source_id} value={s.source_id}>{s.title}</option>)}</select></div></div>
          <div className="fld"><label className="fld-label">What they disagree about</label><input className="m-input" value={f.claim} onChange={(e) => setF({ ...f, claim: e.target.value })} placeholder="the first step when the VPN keeps dropping" /></div>
          <div className="frow two"><div className="fld"><label className="fld-label">The first says</label><input className="m-input" value={f.a_says} onChange={(e) => setF({ ...f, a_says: e.target.value })} /></div><div className="fld"><label className="fld-label">The other says</label><input className="m-input" value={f.b_says} onChange={(e) => setF({ ...f, b_says: e.target.value })} /></div></div>
          <div style={{ display: "flex", gap: 8, marginTop: 8 }}><button className="m-btn primary sm" disabled={!f.source_a || !f.source_b || !f.claim.trim() || !f.a_says.trim() || !f.b_says.trim()} onClick={() => void file()}>Flag it</button><button className="m-btn ghost sm" onClick={() => setFlagging(false)}>Cancel</button></div>
        </div>
      )}
    </div>
  );
}

/** Suggested edits: an agent found a source wrong while working and proposed the corrected body; a person accepts — the superseding version is minted on the agent's proposal — or declines with a reason the agent reads next time. */
function SuggestedEdits({ edits, agentName, onChanged }: { edits: KnowledgeEdit[]; agentName: (id: string) => string; onChanged: () => void }) {
  const { toast } = useOverlay();
  const [openId, setOpenId] = useState<string | null>(null);
  const [declining, setDeclining] = useState<string | null>(null);
  const [reason, setReason] = useState("");
  const say = (err: unknown) => toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle");
  return (
    <div className="m-card" style={{ padding: 14, marginBottom: 14 }} data-suggested-edits>
      <div className="cat-label"><span>Suggested edits</span><span className="ln" /><span className="gc">{edits.length}</span></div>
      <div className="m-hint" style={{ marginBottom: 8 }}>An agent found a source wrong while working and proposed what it should say. Accept mints the superseding version in your name on its proposal; decline with a reason it reads next time. Nothing changes until you decide.</div>
      {edits.map((e) => (
        <div key={e.edit_id} className="scope-row" data-edit={e.edit_id} style={{ display: "block" }}>
          <div style={{ display: "flex", alignItems: "baseline", gap: 8 }}><span style={{ fontWeight: 600 }}>{e.title}</span><span className="item-tag">{agentName(e.proposed_by.agent_id ?? "")}</span><span style={{ color: "var(--ink-500)", fontSize: 12 }}>{ago(e.proposed_at)}</span><span className="sp" /><button className="m-btn ghost sm" onClick={() => setOpenId(openId === e.edit_id ? null : e.edit_id)}>{openId === e.edit_id ? "Hide the proposal" : "Show the proposal"}</button><button className="m-btn primary sm" data-flow="accept-edit" onClick={() => acceptKnowledgeEdit(e.edit_id).then((r) => { toast(`${e.title} corrected — v${r.version} serves now, on ${agentName(e.proposed_by.agent_id ?? "")}'s proposal`, "ti-check"); onChanged(); }).catch(say)}><i className="ti ti-check" /> Accept</button><button className="m-btn ghost sm" onClick={() => setDeclining(declining === e.edit_id ? null : e.edit_id)}>Decline</button></div>
          <div className="m-hint" style={{ marginTop: 4 }}><b>Why:</b> {e.why}</div>
          {openId === e.edit_id && <div className="pre" style={{ marginTop: 8, whiteSpace: "pre-wrap", maxHeight: 320, overflow: "auto", fontSize: 12 }}>{e.body}</div>}
          {declining === e.edit_id && <div style={{ display: "flex", gap: 8, marginTop: 8 }}><input className="m-input" placeholder="Why not — the agent reads this next time" value={reason} onChange={(ev) => setReason(ev.target.value)} data-flow="decline-reason" /><button className="m-btn secondary sm" disabled={!reason.trim()} onClick={() => declineKnowledgeEdit(e.edit_id, reason.trim()).then(() => { toast("Declined — the reason stays on the record", "ti-x"); setDeclining(null); setReason(""); onChanged(); }).catch(say)}>Decline with this reason</button></div>}
        </div>
      ))}
    </div>
  );
}

/** Register a source: a body the server chunks and indexes, scoped to the workspace or one agent. */
export function AddSourceDrawer({ onAdded, scope }: { onAdded: () => void; scope?: KnowledgeScope }) {
  const { close, toast } = useOverlay();
  const me = useServer((s) => s.me);
  const assistants = useServer((s) => s.assistants);
  const [from, setFrom] = useState<"paste" | "web">("paste");
  const [title, setTitle] = useState("");
  const [kind, setKind] = useState<KnowledgeSourceSummary["kind"]>("markdown");
  const [provenance, setProvenance] = useState<NonNullable<KnowledgeSourceSummary["provenance"]>>("organization");
  const [body, setBody] = useState("");
  const [ttl, setTtl] = useState("");
  const [more, setMore] = useState(false);
  const [url, setUrl] = useState("");
  const [fetching, setFetching] = useState(false);
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);
  // Opened from an agent, the source can be that agent's alone; from the library it is for every agent.
  const forAgent = scope?.scope === "agent" ? scope.id : null;
  const [onlyAgent, setOnlyAgent] = useState(false);
  async function fetchPage() {
    setFetching(true); setProblem(null);
    try {
      // Read through the egress ceiling and shown here to review before it is added.
      const page = await fetchPageForSource(url.trim());
      setBody(`Source: ${page.url} (fetched ${new Date().toISOString().slice(0, 10)})\n\n${page.text}`);
      if (!title.trim()) setTitle((page.title ?? page.url).slice(0, 120));
      setKind("text");
      if (provenance === "organization") setProvenance("vendor");
    } catch (err) { setProblem(err instanceof Error ? err.message : "the page could not be read"); }
    finally { setFetching(false); }
  }
  async function read(file: File) { const t = await file.text(); setBody(t); if (!title) setTitle(file.name.replace(/\.[^.]+$/, "")); const ext = file.name.split(".").pop()?.toLowerCase(); setKind(ext === "md" ? "markdown" : ext === "json" ? "json" : ext === "csv" ? "csv" : "text"); }
  async function save() {
    setBusy(true);
    try {
      const agentOnly = forAgent && onlyAgent;
      const sourceId = `${slug(title).slice(0, 80) || "source"}${agentOnly ? `--${forAgent.slice(0, 8)}` : ""}`;
      const r = await registerKnowledgeSource({ source_id: sourceId, kind, title: title.trim(), author: `human:${me?.principal.id ?? "studio"}`, body, provenance, ...(ttl ? { retention: { policy: "ttl", expires_at: new Date(ttl).toISOString() } } : {}), ...(agentOnly ? { scope: { scope: "agent", id: forAgent } } : {}) });
      toast(r.created ? `${title.trim()} added — agents can find it now` : "That text is already in the library", "ti-books"); onAdded(); close();
    } catch (err) { setProblem(err instanceof Error ? err.message : "the server refused"); }
    finally { setBusy(false); }
  }
  const choice = (v: typeof provenance, label: string) => <span key={v} className={`preset${provenance === v ? " on" : ""}`} data-flow={`provenance-${v}`} onClick={() => setProvenance(v)}>{label}</span>;
  return (
    <div className="m-drawer" style={{ width: "min(640px,100%)" }}>
      <OvHead icon="ti-books" bg="var(--cat-rose-bg)" fg="var(--cat-rose)" title="Add a source" sub="Agents cite it when they answer." />
      <div className="ov-body">
        <div className="m-seg" style={{ marginBottom: 14 }}><button className={from === "paste" ? "on" : ""} onClick={() => setFrom("paste")}>Paste or upload</button><button className={from === "web" ? "on" : ""} data-flow="source-from-web" onClick={() => setFrom("web")}>From a web page</button></div>
        {from === "web" && <div className="fld"><label className="fld-label">Web page address</label><div style={{ display: "flex", gap: 8 }}><input className="m-input" placeholder="https://learn.microsoft.com/…" value={url} onChange={(e) => setUrl(e.target.value)} data-flow="source-url" /><button className="m-btn secondary sm" data-flow="source-fetch" disabled={fetching || !url.trim().startsWith("https://")} onClick={() => void fetchPage()}>{fetching ? "Reading…" : "Read page"}</button></div></div>}
        {problem && <div className="m-alert"><i className="ti ti-alert-triangle" style={{ color: "var(--bad)", fontSize: 16 }} /><div className="a-body"><div className="a-text">{problem}</div></div></div>}
        {(from === "paste" || body) && <div className="fld"><label className="fld-label">{from === "web" ? "What the page says — check it before adding" : "Text"}</label><textarea className="m-textarea" rows={12} value={body} onChange={(e) => setBody(e.target.value)} style={{ fontFamily: "inherit" }} placeholder={from === "paste" ? "Paste the text here" : undefined} />{from === "paste" && <input type="file" accept=".md,.txt,.json,.csv,text/*,application/json" style={{ marginTop: 8 }} onChange={(e) => { const f = e.target.files?.[0]; if (f) void read(f); }} />}</div>}
        <div className="fld"><label className="fld-label">Title</label><input className="m-input" value={title} onChange={(e) => setTitle(e.target.value)} placeholder="Facilities desk handbook" /></div>
        <div className="fld"><label className="fld-label">What is it?</label><div className="cron-row" data-flow="source-provenance">{choice("organization", "Our own — policy, runbook, record")}{choice("vendor", "Vendor documentation")}{choice("generic", "General guidance")}</div><div className="m-hint">When sources disagree, your own comes first.</div></div>
        {forAgent && <label style={{ display: "flex", gap: 8, alignItems: "center", fontSize: 13, marginBottom: 10 }}><input type="checkbox" checked={onlyAgent} onChange={(e) => setOnlyAgent(e.target.checked)} />Only for {assistants.find((x) => x.assistant_id === forAgent)?.name ?? "this agent"}</label>}
        <button className="m-btn ghost sm" onClick={() => setMore((v) => !v)}>{more ? "Fewer options" : "More options"}</button>
        {more && <div className="fld" style={{ marginTop: 8 }}><label className="fld-label">Remove it on <span className="opt">— optional</span></label><input className="m-input" type="date" value={ttl} onChange={(e) => setTtl(e.target.value)} style={{ width: 180 }} /></div>}
      </div>
      <div className="ov-foot"><div className="sp" /><button className="m-btn ghost" data-close>Cancel</button><button className="m-btn primary" data-flow="source-add" disabled={busy || !title.trim() || !body.trim()} onClick={() => void save()}><i className="ti ti-check" /> Add source</button></div>
    </div>
  );
}

function SourceDrawer({ s, onChanged }: { s: KnowledgeSourceSummary; onChanged: () => void }) {
  const { close, toast } = useOverlay();
  const me = useServer((st) => st.me);
  const assistants = useServer((st) => st.assistants);
  const [chunks, setChunks] = useState<KnowledgeChunkRecord[]>([]);
  const [versions, setVersions] = useState<number | null>(null);
  const [text, setText] = useState<string | null>(null);
  const [removing, setRemoving] = useState(false);
  const [editing, setEditing] = useState(false);
  const [details, setDetails] = useState(false);
  const [body, setBody] = useState("");
  useEffect(() => { getKnowledgeSource(s.source_id).then((r) => { setChunks(r.chunks ?? []); setVersions(r.versions ?? null); }).catch(() => {}); }, [s.source_id]);
  // The document as a person reads it: its passages in order, not their ids.
  // Passages overlap so a search hit keeps its context; joined here by their
  // byte ranges, each passage adds only what the one before did not cover.
  useEffect(() => {
    if (!chunks.length) return;
    const shown = [...chunks].sort((a, b) => a.byte_start - b.byte_start).slice(0, 30);
    Promise.all(shown.map((c) => knowledgeChunk(s.source_id, c.chunk_index).then((r) => r.text).catch(() => ""))).then((parts) => {
      const enc = new TextEncoder(); const dec = new TextDecoder();
      let covered = 0; let out = "";
      shown.forEach((c, n) => {
        const bytes = enc.encode(parts[n]);
        const skip = Math.max(0, Math.min(bytes.length, covered - c.byte_start));
        out += dec.decode(bytes.slice(skip));
        covered = Math.max(covered, c.byte_start + bytes.length);
      });
      setText(out + (chunks.length > 30 ? "\n\n…" : ""));
    });
  }, [chunks, s.source_id]);
  const what = ({ organization: "Our own", vendor: "Vendor documentation", generic: "General guidance" } as Record<string, string>)[s.provenance ?? "organization"] ?? "Our own";
  const who = s.scope.scope === "agent" ? `Only ${assistants.find((a) => a.assistant_id === s.scope.id)?.name ?? "one agent"}` : "All agents";
  async function save() {
    try { await correctKnowledgeSource(s.source_id, { author: `human:${me?.principal.id ?? "studio"}`, body }); toast("Saved — agents use the new text; answers that cited the old one still resolve", "ti-books"); onChanged(); close(); }
    catch (err) { toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle"); }
  }
  return (
    <div className="m-drawer" style={{ width: "min(640px,100%)" }}>
      <OvHead icon="ti-file-text" bg="var(--cat-rose-bg)" fg="var(--cat-rose)" title={s.title} sub={`${what} · ${who} · updated ${ago(s.created_at)}`} />
      <div className="ov-body">
        {!editing && <div className="rn-md" style={{ fontSize: 14, lineHeight: 1.55 }} data-source-text>{text === null ? "Reading…" : s.kind === "markdown" ? renderMarkdown(text) : <div style={{ whiteSpace: "pre-wrap" }}>{text}</div>}</div>}
        {editing && <div className="fld"><label className="fld-label">Text</label><textarea className="m-textarea" rows={16} value={body} onChange={(e) => setBody(e.target.value)} style={{ fontFamily: "inherit" }} /></div>}
        {!editing && <div style={{ marginTop: 16 }}><button className="m-btn ghost sm" data-flow="source-details" onClick={() => setDetails((v) => !v)}>{details ? "Hide details" : "Details"}</button>
          {details && <div className="kv" style={{ marginTop: 8 }} data-source-provenance><span className="k">Added by</span><span className="v">{s.author.replace(/^human:/, "")}</span><span className="k">Version</span><span className="v">{s.version}{versions && versions > 1 ? ` of ${versions}` : ""}</span><span className="k">Passages</span><span className="v">{s.chunk_count}</span><span className="k">Kept</span><span className="v">{s.retention.policy === "ttl" ? `until ${new Date(s.retention.expires_at).toLocaleDateString()}` : "until removed"}</span><span className="k">Confidence</span><span className="v">{s.confidence.toFixed(2)}</span><span className="k">Content</span><span className="v mono">{s.content_hash.slice(0, 16)}</span></div>}
        </div>}
      </div>
      <div className="ov-foot">{editing
        ? <><button className="m-btn ghost sm" onClick={() => setEditing(false)}>Cancel</button><div className="sp" /><button className="m-btn primary" disabled={!body.trim()} onClick={() => void save()}><i className="ti ti-check" /> Save</button></>
        : <><button className="m-btn ghost sm" onClick={() => { setBody(text ?? ""); setEditing(true); }}><i className="ti ti-pencil" /> Edit</button><button className={`m-btn ${removing ? "danger" : "ghost"} sm`} data-flow="retire-source" onClick={() => { if (!removing) { setRemoving(true); return; } void retireKnowledgeSource(s.source_id).then(() => { toast(`${s.title} removed — agents no longer find it`, "ti-trash"); onChanged(); close(); }).catch((err) => toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle")); }}><i className="ti ti-trash" /> {removing ? "Remove for good" : "Remove"}</button><div className="sp" /><button className="m-btn secondary" data-close>Close</button></>}</div>
    </div>
  );
}

/** Ask the store what an agent would get: cited chunks, scored. */
export function QueryModal({ scope: fixed }: { scope?: KnowledgeScope }) {
  const assistants = useServer((s) => s.assistants);
  const me = useServer((s) => s.me);
  const [as, setAs] = useState<string>(fixed ? `${fixed.scope}:${fixed.id}` : "tenant");
  const scope: KnowledgeScope | undefined = fixed ?? (as === "tenant" ? undefined : { scope: as.split(":")[0], id: as.split(":")[1] });
  const [q, setQ] = useState("");
  const [res, setRes] = useState<{ citation: { title: string; chunk_id: string }; text: string; score: number }[] | null>(null);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  async function ask() { if (!q.trim()) return; setBusy(true); setErr(null); try { const r = await queryKnowledge(q.trim(), scope, { max_results: 8, max_bytes: 32768 }); setRes(r.results); } catch (e) { setErr(e instanceof Error ? e.message : "the store did not answer"); } finally { setBusy(false); } }
  return (
    <div className="m-modal lg">
      <OvHead icon="ti-search" bg="var(--cat-rose-bg)" fg="var(--cat-rose)" title="Ask the store" sub="The same retrieval an agent's search_knowledge call gets, with the citations it would carry." />
      <div className="ov-body">
        {!fixed && <div className="fld"><label className="fld-label">Retrieve as</label><select className="m-input" value={as} onChange={(e) => setAs(e.target.value)}><option value="tenant">The workspace ({me?.tenant ?? "tenant"}) — shared sources only</option>{assistants.filter((a) => !a.archived_at).map((a) => <option key={a.assistant_id} value={`agent:${a.assistant_id}`}>{a.name} — its own sources</option>)}</select></div>}
        <div className="composer"><textarea rows={1} placeholder="What does the desk do with a jammed vending machine?" value={q} onChange={(e) => setQ(e.target.value)} onKeyDown={(e) => { if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); void ask(); } }} /><button className="composer-send" disabled={busy} onClick={() => void ask()}><i className="ti ti-arrow-up" /></button></div>
        {err && <div className="m-hint" style={{ color: "var(--bad)", marginTop: 10 }}>{err}</div>}
        {res && <div style={{ marginTop: 14 }}>{res.length === 0 && <div className="pane-empty"><i className="ti ti-search-off" /> Nothing scored for that.</div>}{res.map((r, i) => <div key={i} className="m-card" style={{ padding: 12, marginBottom: 8 }}><div className="f-mini-label">{r.citation.title} · <span className="mono">{r.citation.chunk_id}</span> · score {r.score.toFixed(2)}</div><div style={{ fontSize: "var(--fs-sm)", whiteSpace: "pre-wrap" }}>{r.text.slice(0, 600)}{r.text.length > 600 ? "…" : ""}</div></div>)}</div>}
      </div>
      <div className="ov-foot"><div className="sp" /><button className="m-btn secondary" data-close>Close</button></div>
    </div>
  );
}

function RetentionModal({ onApplied }: { onApplied: () => void }) {
  const { close, toast } = useOverlay();
  const [plan, setPlan] = useState<Awaited<ReturnType<typeof knowledgeRetentionPlan>> | null>(null);
  useEffect(() => { knowledgeRetentionPlan().then(setPlan).catch(() => setPlan({ entries: [], total_chunk_bytes: 0 })); }, []);
  return (
    <div className="m-modal lg">
      <OvHead icon="ti-clock-off" title="Retention sweep" sub="What a sweep now would purge: expired sources leave a metadata-only tombstone." />
      <div className="ov-body">
        {!plan && <div className="thread-empty">Planning…</div>}
        {plan && plan.entries.length === 0 && <div className="pane-empty"><i className="ti ti-circle-check" /> Nothing has expired.</div>}
        {plan?.entries.map((e) => <div key={e.source_id} className="scope-row"><div className="sb"><div style={{ fontSize: "var(--fs-sm)", fontWeight: 600 }}>{e.title}</div><div className="sd">v{e.version} · expired {ago(e.expires_at)} · {e.chunk_count} chunks · {(e.chunk_bytes / 1024).toFixed(0)} KB</div></div></div>)}
      </div>
      <div className="ov-foot"><div className="sp" /><button className="m-btn ghost" data-close>Cancel</button><button className="m-btn danger" disabled={!plan || plan.entries.length === 0} onClick={async () => { try { const r = await knowledgeRetentionApply(); toast(`${r.tombstones.length} source${r.tombstones.length === 1 ? "" : "s"} purged`, "ti-clock-off"); onApplied(); close(); } catch (err) { toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle"); } }}>Purge {plan?.entries.length ?? 0}</button></div>
    </div>
  );
}

function ReferenceDrawer({ r }: { r: SkillFreshness & { skill: string } }) {
  return (
    <div className="m-drawer">
      <OvHead icon="ti-book-2" bg="var(--cat-plum-bg)" fg="var(--cat-plum)" title={r.reference} sub={`Learned by ${r.skill} · revision ${r.revision}`} />
      <div className="ov-body">
        <div className="kv"><span className="k">Status</span><span className="v"><Badge tone={r.stale ? "warn" : "good"}>{r.stale ? "Stale" : "Current"}</Badge></span><span className="k">Learned</span><span className="v">{ago(r.learned_at)}</span><span className="k">Checked</span><span className="v">{r.checked_at ? ago(r.checked_at) : "not yet"}</span></div>
        {r.because.length > 0 && <><div className="cat-label"><span>What moved</span><span className="ln" /></div>{r.because.map((b, i) => <div key={i} className="m-hint">{b}</div>)}</>}
        <div className="cat-label" style={{ marginTop: 22 }}><span>Read from</span><span className="ln" /></div>
        {r.reads.map((x, i) => <div key={i} className="scope-row"><div className="sb"><div style={{ fontSize: "var(--fs-sm)", fontWeight: 600 }}>{x.title}</div><div className="sd"><span className="mono">{x.tool}</span> · {x.records} records{x.newest ? ` · newest ${x.newest}` : ""}</div></div></div>)}
      </div>
      <div className="ov-foot"><div className="sp" /><button className="m-btn secondary" data-close>Close</button></div>
    </div>
  );
}

function ArtifactDrawer({ a }: { a: RunArtifact }) {
  const [text, setText] = useState<string | null>(null);
  useEffect(() => { artifactText(a.artifact_id).then((t) => setText(t.slice(0, 4000))).catch(() => setText(null)); }, [a.artifact_id]);
  return (
    <div className="m-drawer" style={{ width: "min(640px,100%)" }}>
      <OvHead icon="ti-file-text" bg="var(--cat-amber-bg)" fg="var(--cat-amber)" title={a.name ?? a.artifact_id} sub={`${a.media_kind}${a.media_type ? ` · ${a.media_type}` : ""} · filed by run ${a.lineage.run_id.slice(0, 12)}`} />
      <div className="ov-body">
        <div className="kv"><span className="k">Versions</span><span className="v">{a.versions?.length ?? 1}</span><span className="k">Bytes</span><span className="v mono">{a.versions?.[a.versions.length - 1]?.bytes ?? "—"}</span><span className="k">Address</span><span className="v mono">{a.artifact_id}</span></div>
        <div className="cat-label"><span>Contents</span><span className="ln" /></div>
        <div className="pre">{text ?? "Not readable as text."}</div>
      </div>
      <div className="ov-foot"><div className="sp" /><button className="m-btn secondary" data-close>Close</button></div>
    </div>
  );
}

function Stat({ v, u, l }: { v: string; u?: string; l: string }) {
  return <div className="lib-stat"><div className="ls-v">{v}{u && <span className="u">{u}</span>}</div><div className="ls-l">{l}</div></div>;
}
