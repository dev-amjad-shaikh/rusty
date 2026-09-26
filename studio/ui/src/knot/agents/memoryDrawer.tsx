import { useEffect, useState, type ReactNode } from "react";
import { acceptMemory, confirmMemory, agentBlockHistory, agentBlocks, forgetMemory, memoryConflicts, memoryUtility, proposedMemory, queryMemory, rollUpMemoryUtility, saveAgentBlock, type AgentBlock, type AgentBlockVersion, type Assistant, type MemoryConflict, type MemoryRecord, type MemoryUtility } from "../../engine/net/client";
import { Badge, OvHead, useOverlay } from "../overlay";
import { ago } from "../data";

/** Pinned notes the platform declares by default, in the words a person uses. */
const PINNED_NAMES: Record<string, string> = { person: "About the person", working: "What it is working on", decisions: "Decisions that stand" };

/** A note's value is text, or an envelope around text; show the sentence. */
function noteText(m: MemoryRecord): string {
  const v = m.content.value ?? m.content;
  if (typeof v === "string") return v;
  if (v && typeof v === "object") {
    const o = v as Record<string, unknown>;
    for (const k of ["text", "claim", "note", "summary", "value", "content"]) if (typeof o[k] === "string") return o[k] as string;
    const only = Object.values(o).filter((x) => typeof x === "string");
    if (only.length === 1) return only[0] as string;
  }
  return JSON.stringify(v);
}

const untrusted = (m: MemoryRecord) => !!m.tags?.includes("origin:untrusted");

/**
 * What an agent remembers, for the person who looks after it: what needs
 * their word first, then the notes in plain sentences, then the pinned notes
 * the agent sees on every run. Keys, confidence, runs and usage are there
 * for whoever opens a note's details — not in everyone's way.
 */
export function MemoryDrawer({ agent, declared = [] }: { agent: Assistant; /** Pinned notes the working copy declares beyond the published agent's. */ declared?: { label: string; description?: string; char_limit?: number }[] }) {
  const { toast } = useOverlay();
  const [tab, setTab] = useState<"Notes" | "Pinned">("Notes");
  const [all, setAll] = useState<MemoryRecord[]>([]);
  const [proposed, setProposed] = useState<MemoryRecord[]>([]);
  const [conflicts, setConflicts] = useState<MemoryConflict[]>([]);
  const [utility, setUtility] = useState<(MemoryUtility & { rolling?: boolean }) | null>(null);
  const [q, setQ] = useState("");
  const [aboutPeople, setAboutPeople] = useState(false);
  const [open, setOpen] = useState<string | null>(null);

  const mine = (m: MemoryRecord) => m.scope.id === agent.assistant_id || m.provenance.author.agent_id === agent.assistant_id;
  const load = () => {
    // Pinned notes live on their own tab, not twice.
    queryMemory().then((r) => setAll(r.filter((m) => mine(m) && m.scope.scope !== "run" && !m.tags?.includes("block")))).catch(() => {});
    proposedMemory().then((r) => setProposed(r.filter(mine))).catch(() => {});
    memoryConflicts().then(setConflicts).catch(() => {});
    memoryUtility().then(setUtility).catch(() => setUtility(null));
  };
  useEffect(load, [agent.assistant_id]); // eslint-disable-line react-hooks/exhaustive-deps

  /** Who a note came from, in a person's words. */
  const from = (m: MemoryRecord) => {
    const a = m.provenance.author;
    const about = m.scope.scope === "user" ? ` · about ${m.scope.id}` : "";
    if (a.type === "human") return `Said by ${a.name ?? a.human_id ?? "a person"}${about}`;
    if (a.type === "distiller") return (a.name === "review" ? "Kept after reviewing a run" : "A summary of earlier notes") + about;
    if (a.type === "agent" && a.agent_id && a.agent_id !== agent.assistant_id) return `Reported by another agent${about}`;
    if (a.type === "agent") return `Learned by ${agent.name}${about}`;
    return `Kept by the platform${about}`;
  };

  // What needs a person's word, in one place.
  const myConflicts = conflicts
    .map((c) => ({ c, recs: c.memory_ids.map((id) => all.find((m) => m.memory_id === id)).filter((m): m is MemoryRecord => !!m) }))
    .filter((x) => x.recs.length > 1);
  const toCheck = all.filter(untrusted);
  const waiting = proposed.length + toCheck.length + myConflicts.length;
  const inConflict = new Set(myConflicts.flatMap((x) => x.recs.map((m) => m.memory_id)));
  const notes = all
    .filter((m) => !untrusted(m) && !inConflict.has(m.memory_id))
    .filter((m) => !aboutPeople || m.scope.scope === "user")
    .filter((m) => !q || noteText(m).toLowerCase().includes(q.toLowerCase()));

  const act = async (work: Promise<unknown>, done: string, icon = "ti-check") => {
    try { await work; toast(done, icon); load(); } catch (err) { toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle"); }
  };
  const recount = async () => {
    try {
      const r = await rollUpMemoryUtility();
      toast(r.started ? "Counting how often each note helped — this takes a minute" : "Already counting", "ti-refresh");
    } catch (err) { toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle"); }
  };

  const details = (m: MemoryRecord) => {
    const use = utility?.entries[m.memory_id];
    return (
      <div className="mm" style={{ marginTop: 6, flexWrap: "wrap" }} data-memory-details>
        {m.key && <span>key {m.key}</span>}
        <span>confidence {m.confidence.toFixed(2)}</span>
        {m.provenance.evidence?.run_id && <span>learned in run {m.provenance.evidence.run_id.slice(0, 8)}</span>}
        {use && <span>used in {use.successful_uses + use.failed_uses} runs, helped {use.successful_uses}</span>}
        <span>{m.kind} · {m.scope.scope}</span>
      </div>
    );
  };

  const note = (m: MemoryRecord, actions: ReactNode) => (
    <div key={m.memory_id} className="mem-entry" data-kind={m.kind}>
      <div className="me-ic"><i className={`ti ${m.scope.scope === "user" ? "ti-user" : m.kind === "summary" ? "ti-history-toggle" : "ti-bulb"}`} /></div>
      <div className="mb">
        <div className="mt">{noteText(m)}</div>
        <div className="mm">
          <span>{from(m)} · {ago(m.created_at)}</span>
          {utility?.sweep?.decayed?.includes(m.memory_id) && <Badge tone="bad">fading</Badge>}
          <span className="sp" />
          <button className="m-btn ghost sm" data-flow="memory-details" onClick={() => setOpen(open === m.memory_id ? null : m.memory_id)}>{open === m.memory_id ? "Hide details" : "Details"}</button>
        </div>
        {open === m.memory_id && details(m)}
      </div>
      {actions}
    </div>
  );
  const forgetButton = (m: MemoryRecord) => <button className="m-btn ghost sm icon" title="Forget this note" onClick={() => void act(forgetMemory(m.memory_id), "Forgotten", "ti-trash")}><i className="ti ti-trash" /></button>;

  return (
    <div className="m-drawer" style={{ width: "min(600px,100%)" }}>
      <OvHead icon="ti-brain" bg="var(--cat-orange-bg)" fg="var(--cat-orange)" title={`What ${agent.name} remembers`} sub={`${all.length} note${all.length === 1 ? "" : "s"}${waiting ? ` · ${waiting} need${waiting === 1 ? "s" : ""} you` : ""}`} />
      <div style={{ padding: "14px 22px 0" }}><div className="m-seg">{(["Notes", "Pinned"] as const).map((t) => <button key={t} className={tab === t ? "on" : ""} onClick={() => setTab(t)}>{t === "Notes" ? "Notes" : "Pinned notes"}</button>)}</div></div>
      <div className="ov-body" data-pane>
        {tab === "Notes" && (
          <>
            {waiting > 0 && (
              <div className="m-card" style={{ padding: 12, marginBottom: 14 }} data-needs-you>
                <div className="f-mini-label">Needs you</div>
                {proposed.map((m) => note(m, <span style={{ display: "flex", gap: 4 }}><button className="m-btn primary sm" data-flow="accept-memory" onClick={() => void act(acceptMemory(m.memory_id), "Kept — it is used from now on")}>Keep</button><button className="m-btn ghost sm" data-flow="decline-memory" onClick={() => void act(forgetMemory(m.memory_id), "Dropped", "ti-trash")}>Drop</button></span>))}
                {toCheck.map((m) => (
                  <div key={m.memory_id} data-flow="memory-untrusted">
                    <div style={{ fontSize: 12, color: "var(--ink-600)", margin: "6px 0 2px" }}>Learned from a web page — check it before the agent relies on it</div>
                    {note(m, <span style={{ display: "flex", gap: 4 }}><button className="m-btn primary sm" data-flow="memory-confirm" onClick={() => void act(confirmMemory(m.memory_id), "Confirmed — the agent now treats it as checked")}>It's right</button>{forgetButton(m)}</span>)}
                  </div>
                ))}
                {myConflicts.map(({ recs }, i) => (
                  <div key={i} data-memory-conflict>
                    <div style={{ fontSize: 12, color: "var(--ink-600)", margin: "6px 0 2px" }}>These disagree — keep the right one</div>
                    {recs.map((m) => note(m, <button className="m-btn ghost sm" data-flow="keep-this" onClick={() => void act(Promise.all(recs.filter((o) => o.memory_id !== m.memory_id).map((o) => forgetMemory(o.memory_id))), "Kept — the other is forgotten")}>Keep this</button>))}
                  </div>
                ))}
              </div>
            )}
            <div className="ov-search" style={{ marginBottom: 10 }}><i className="ti ti-search" /><input placeholder="Search what it remembers…" value={q} onChange={(e) => setQ(e.target.value)} /></div>
            <div className="cron-row" style={{ marginBottom: 8 }}>{([["All", false], ["About people", true]] as const).map(([label, v]) => <span key={label} className={`preset${aboutPeople === v ? " on" : ""}`} onClick={() => setAboutPeople(v)}>{label}</span>)}</div>
            <div>
              {notes.map((m) => note(m, forgetButton(m)))}
              {notes.length === 0 && <div className="pane-empty"><i className="ti ti-brain" /> {all.length ? "Nothing matches." : `${agent.name} has not remembered anything yet.`}</div>}
            </div>
          </>
        )}
        {tab === "Pinned" && <PinnedNotes agent={agent} declared={declared} onSaved={load} />}
      </div>
      <div className="ov-foot">{tab === "Notes" && <button className="m-btn ghost sm" data-flow="roll-up" disabled={!!utility?.rolling} title="Count how often each note helped a run" onClick={() => void recount()}><i className="ti ti-refresh" /> Recount usage</button>}<div className="sp" /><button className="m-btn secondary" data-close>Done</button></div>
    </div>
  );
}

/** The notes the agent sees at the start of every run, edited in place. */
function PinnedNotes({ agent, declared, onSaved }: { agent: Assistant; declared: { label: string; description?: string; char_limit?: number }[]; onSaved: () => void }) {
  const { toast } = useOverlay();
  const [blocks, setBlocks] = useState<AgentBlock[]>([]);
  const [drafts, setDrafts] = useState<Record<string, string>>({});
  const [history, setHistory] = useState<Record<string, AgentBlockVersion[] | null>>({});
  const who = (author: string) => author === "system" ? "the platform" : author.startsWith("human:") ? author.slice(6) : author.startsWith("agent:") ? agent.name : author;
  const load = () => agentBlocks(agent.assistant_id).then((list) => {
    const b = [...list, ...declared.filter((x) => !list.some((s) => s.label === x.label)).map((x) => ({ label: x.label, description: x.description ?? "", char_limit: x.char_limit ?? 1500, text: "", declared: false }))];
    setBlocks(b); setDrafts(Object.fromEntries(b.map((x) => [x.label, x.text])));
  }).catch(() => {});
  useEffect(() => { void load(); }, [agent.assistant_id]); // eslint-disable-line react-hooks/exhaustive-deps
  const toggleHistory = (label: string) => {
    if (history[label] !== undefined) { setHistory((h) => { const n = { ...h }; delete n[label]; return n; }); return; }
    setHistory((h) => ({ ...h, [label]: null }));
    agentBlockHistory(agent.assistant_id, label).then((v) => setHistory((h) => ({ ...h, [label]: v }))).catch(() => setHistory((h) => ({ ...h, [label]: [] })));
  };
  return (
    <div>
      <div className="m-hint" style={{ marginBottom: 12 }}>{agent.name} sees these at the start of every run and keeps them up to date. One note per line.</div>
      {blocks.map((b) => {
        const draft = drafts[b.label] ?? "";
        const chars = draft.split("\n").map((l) => l.trim()).filter(Boolean).join("\n").length;
        const dirty = draft !== b.text;
        const nearLimit = chars > b.char_limit * 0.8;
        return (
          <div key={b.label} className="mem-entry" data-kind="block" style={{ display: "block" }}>
            <div style={{ display: "flex", alignItems: "baseline", gap: 8 }}>
              <span style={{ fontWeight: 600 }}>{PINNED_NAMES[b.label] ?? b.label}</span>
              {b.declared === false && <span className="item-tag" title="Only in the draft until it is published">draft</span>}
              <span className="sp" />
              {nearLimit && <span style={{ color: chars > b.char_limit ? "var(--bad)" : "var(--ink-500)", fontSize: 12 }}>{chars > b.char_limit ? "Too long — shorten it" : "Nearly full"}</span>}
            </div>
            <textarea value={draft} rows={Math.min(8, Math.max(3, draft.split("\n").length + 1))} style={{ width: "100%", marginTop: 8, fontSize: 13, fontFamily: "inherit" }} placeholder={b.description || "Nothing yet"} onChange={(e) => setDrafts((x) => ({ ...x, [b.label]: e.target.value }))} />
            <div style={{ display: "flex", gap: 8, marginTop: 6, alignItems: "center" }}>
              <span style={{ color: "var(--ink-500)", fontSize: 12 }}>{b.updated_at ? `Updated ${ago(b.updated_at)}` : "Empty"}</span>
              <span className="sp" />
              <button className="m-btn ghost sm" data-flow="block-history" onClick={() => toggleHistory(b.label)}>{history[b.label] !== undefined ? "Hide earlier versions" : "Earlier versions"}</button>
              {dirty && <button className="m-btn ghost sm" onClick={() => setDrafts((x) => ({ ...x, [b.label]: b.text }))}>Discard</button>}
              <button className="m-btn sm" disabled={!dirty || chars > b.char_limit} onClick={() => saveAgentBlock(agent.assistant_id, b.label, draft, b.declared === false ? b.char_limit : undefined).then(() => { toast("Saved — the agent sees it from its next run", "ti-check"); void load(); onSaved(); }).catch((err) => toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle"))}>Save</button>
            </div>
            {history[b.label] !== undefined && (
              <div style={{ marginTop: 8, borderTop: "1px solid var(--line)", paddingTop: 8 }} data-block-history={b.label}>
                {history[b.label] === null && <div style={{ color: "var(--ink-500)", fontSize: 12 }}>Reading…</div>}
                {history[b.label]?.length === 0 && <div style={{ color: "var(--ink-500)", fontSize: 12 }}>No earlier versions.</div>}
                {history[b.label]?.map((v) => (
                  <div key={v.version} style={{ display: "flex", gap: 8, alignItems: "center", padding: "4px 0", fontSize: 12 }} data-block-version={v.version}>
                    <span style={{ color: "var(--ink-500)", minWidth: 90 }}>{ago(v.written_at)}</span><span>by {who(v.author)}</span>
                    <span className="sp" />
                    {v.current ? <span className="item-tag">current</span> : <button className="m-btn ghost sm" data-flow="block-restore" title="Put this version back in the editor; Save keeps it" onClick={() => setDrafts((x) => ({ ...x, [b.label]: v.text }))}>Use this</button>}
                  </div>
                ))}
              </div>
            )}
          </div>
        );
      })}
      {blocks.length === 0 && <div className="pane-empty"><i className="ti ti-pin" /> No pinned notes.</div>}
    </div>
  );
}
