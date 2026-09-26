import { useEffect, useMemo, useRef, useState } from "react";
import { createThread, decideApproval, getRun, listApprovals, listWorlds, runAndWait, runEvents, threadState, type Approval, type Assistant, type ChatMessage, type Run, type RunEvent, type RunResult, type World } from "../../engine/net/client";
import { useServer } from "../../engine/net/server";
import { worldsForAgent } from "../../engine/worlds";
import { useOverlay } from "../overlay";
import { cancelRun, listRuns, llmProviders, resumeRun, type Halt } from "../../engine/net/client";
import { ago, toolIcon, type Look } from "../data";
import type { AgentDraft } from "./useAgent";
import type { Proposals } from "./proposals";
import { KIND_ICON } from "./improvements";
import { renderMarkdown } from "../../engine/text/markdown";
import { argSummary, plainName, resultSummary } from "./words";

type Mode = "build" | "test" | "observe";
const MODE_KEY = "rusty.panelMode";

/** The side panel: Build (the Coach proposes reviewable versions), Test (the playground), Observe (this agent's runs and their events). */
/** The working copy's context numbers as the server takes them: positive numbers only, strings from the form parsed. */
function draftContext(c: { budget_tokens?: number | string; keep_recent_messages?: number | string } | undefined): { budget_tokens?: number; keep_recent_messages?: number } | undefined {
  if (!c) return undefined;
  const num = (v: number | string | undefined) => { const n = typeof v === "string" ? Number(v.replace(/,/g, "")) : v; return typeof n === "number" && Number.isFinite(n) && n > 0 ? Math.round(n) : undefined; };
  const out = { ...(num(c.budget_tokens) ? { budget_tokens: num(c.budget_tokens) } : {}), ...(num(c.keep_recent_messages) ? { keep_recent_messages: num(c.keep_recent_messages) } : {}) };
  return Object.keys(out).length ? out : undefined;
}

/** The plan a turn wrote: the last `plan` call's steps, as the loop reads them. */
function planOf(calls: { function?: { name?: string; arguments?: string } }[]): { text: string; status: string; note?: string }[] | undefined {
  const call = [...calls].reverse().find((c) => c.function?.name === "plan");
  if (!call) return undefined;
  try {
    const args = JSON.parse(call.function?.arguments ?? "{}") as { steps?: { text?: string; status?: string; note?: string }[] };
    const steps = (args.steps ?? []).filter((x) => x.text?.trim()).map((x) => ({ text: String(x.text).trim(), status: ["doing", "done", "skipped"].includes(String(x.status ?? "").toLowerCase()) ? String(x.status).toLowerCase() : "todo", note: x.note?.trim() || undefined }));
    return steps.length ? steps : undefined;
  } catch { return undefined; }
}

export function TestPanel({ agent, draft, status, proposals }: { agent: Assistant; draft: AgentDraft; look: Look; status: "draft" | "published"; proposals: Proposals }) {
  const { toast } = useOverlay();
  const [mode, setMode] = useState<Mode>(() => { try { const m = localStorage.getItem(MODE_KEY); return m === "build" || m === "observe" ? m : "test"; } catch { return "test"; } });
  const [unseen, setUnseen] = useState(false);
  const [obsTick, setObsTick] = useState(0);
  useEffect(() => { const on = (e: Event) => { const m = (e as CustomEvent<Mode>).detail; if (m === "build" || m === "test" || m === "observe") switchMode(m); }; window.addEventListener("rusty:panel", on); return () => window.removeEventListener("rusty:panel", on); }, []); // eslint-disable-line react-hooks/exhaustive-deps
  const switchMode = (m: Mode) => { setMode(m); if (m === "observe") setUnseen(false); try { localStorage.setItem(MODE_KEY, m); } catch { /* per-viewer convenience only */ } };

  // ── Test: a real conversation with the agent ──
  const [thread, setThread] = useState<string | null>(null);
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [input, setInput] = useState("");
  const [running, setRunning] = useState(false);
  // One step's raw call and result, shown on demand.
  const [rawStep, setRawStep] = useState<string | null>(null);
  /** What the run is doing while it runs: the tools it has called so far, read
   * from the journal. A seven-minute run showed three dots and nothing else. */
  const [live, setLive] = useState<{ secs: number; steps: string[]; runId?: string }>({ secs: 0, steps: [] });
  const [stopping, setStopping] = useState(false);
  const [last, setLast] = useState<RunResult | null>(null);
  const [failure, setFailure] = useState<string | null>(null);
  const [pending, setPending] = useState<Approval | null>(null);
  const [halt, setHalt] = useState<Halt | null>(null);
  const [resuming, setResuming] = useState(false);
  const [deciding, setDeciding] = useState(false);
  const [worlds, setWorlds] = useState<World[]>([]);
  /** The thread as the server holds it — the record; the run's output when the thread cannot be read. */
  async function readThread(th: string, fallback?: ChatMessage[]) {
    try { const st = await threadState(th); const m = st.values?.messages; if (m && m.length) { setMessages(m); return; } } catch { /* the run's own output below */ }
    if (fallback && fallback.length) setMessages(fallback);
  }
  /** Decide the paused call here; the run resumes, and the thread is read back once it settles. */
  /** Carry a halted run on from its checkpoint, with more room when asked. */
  async function carryOn(maxSteps?: number) {
    if (!thread) return;
    setResuming(true);
    try {
      const r = await resumeRun(thread, agent.assistant_id, maxSteps);
      setLast(r);
      setHalt((r.interrupt?.["rusty.halted"] as Halt | undefined) ?? null);
      await readThread(thread);
      toast(r.status === "success" ? "It carried on and finished" : `It carried on — ${r.status}`, "ti-player-play");
    } catch (err) {
      toast(err instanceof Error ? err.message : "the run could not be carried on", "ti-alert-triangle");
    } finally { setResuming(false); }
  }

  /** Follow a decided run to its continuation and show how it ended. */
  async function follow(pausedRunId: string, resumedId: string | null) {
    if (!thread) return;
    let resumed = resumedId;
    for (let i = 0; i < 20 && !resumed; i++) { await new Promise((r) => setTimeout(r, 1000)); resumed = (await getRun(pausedRunId).catch(() => null))?.decision?.resumed_run_id ?? null; }
    if (resumed) { for (let i = 0; i < 120; i++) { const r = await getRun(resumed).catch(() => null); if (r && r.status !== "pending" && r.status !== "running") { setLast({ run_id: r.run_id, status: r.status, output: r.output, verification: r.verification } as RunResult); if (r.status === "interrupted") { const p = (await listApprovals("pending").catch(() => [] as Approval[])).find((a) => a.run_id === r.run_id) ?? null; setPending(p); } break; } await new Promise((res) => setTimeout(res, 1000)); } }
    await readThread(thread);
    await useServer.getState().refresh();
    setObsTick((n) => n + 1);
  }
  async function decide(decision: "approve" | "deny") {
    if (!pending || !thread) return;
    setDeciding(true); setFailure(null);
    try {
      const d = await decideApproval(pending.run_id, decision);
      const paused = pending.run_id;
      setPending(null); setRunning(true);
      await follow(paused, d.resumed_run_id ?? null);
    } catch (err) { setFailure(err instanceof Error ? err.message : "the decision was not taken"); }
    finally { setDeciding(false); setRunning(false); }
  }
  // A decision taken elsewhere — in Notifications, by someone else — is
  // noticed here too: the panel stops waiting and shows how the run went on.
  useEffect(() => {
    if (!pending || deciding) return;
    const paused = pending.run_id;
    const t = setInterval(() => {
      void getRun(paused).then((r) => {
        if (!r?.decision) return;
        clearInterval(t);
        setPending(null); setRunning(true);
        void follow(paused, r.decision.resumed_run_id ?? null).finally(() => setRunning(false));
      }).catch(() => {});
    }, 3000);
    return () => clearInterval(t);
  }, [pending?.run_id, deciding]); // eslint-disable-line react-hooks/exhaustive-deps
  const [world, setWorld] = useState("");
  const endRef = useRef<HTMLDivElement>(null);
  const recent = useServer((s) => s.runs);
  const toolNames = (draft.intent.tools ?? []).map((t) => t.name);
  useEffect(() => { listWorlds().then(setWorlds).catch(() => setWorlds([])); }, []);
  const fitting = useMemo(() => worldsForAgent(toolNames, worlds), [toolNames.join("|"), worlds]); // eslint-disable-line react-hooks/exhaustive-deps
  useEffect(() => { setWorld((w) => (w && fitting.some((f) => f.name === w) ? w : fitting[0]?.name ?? "")); }, [fitting]);
  useEffect(() => { setThread(null); setMessages([]); setLast(null); setFailure(null); setPending(null); }, [agent.assistant_id]);
  useEffect(() => { endRef.current?.scrollIntoView({ block: "end" }); }, [messages, running]);
  const presets = useMemo(() => [...new Set(recent.filter((r) => r.assistant_id === agent.assistant_id && r.asked).map((r) => r.asked!))].slice(0, 3), [recent, agent.assistant_id]);

  async function send(text: string) {
    const t = text.trim();
    if (!t || running) return;
    setInput(""); setFailure(null); setRunning(true);
    let watch: number | undefined;
    const stopWatch = () => { if (watch !== undefined) window.clearInterval(watch); setLive({ secs: 0, steps: [] }); };
    const turn: ChatMessage = { role: "user", content: t };
    setMessages((m) => [...m, turn]);
    try {
      const th = thread ?? (await createThread(agent.graph)).thread_id;
      if (!thread) setThread(th);
      // Watch the thread's newest run while the turn is in flight, so the
      // pane shows the tools as they are called rather than three dots.
      const began = Date.now();
      watch = window.setInterval(() => {
        setLive((l) => ({ ...l, secs: Math.round((Date.now() - began) / 1000) }));
        void listRuns(1, agent.assistant_id)
          .then(async ([r]) => {
            // Only this turn's run: the newest listed run may still be the
            // last one, whose steps would be somebody else's story.
            if (!r || new Date(r.created_at).getTime() < began - 2000) return;
            const ev = await runEvents(r.run_id).catch(() => ({ events: [] as RunEvent[] }));
            const steps = ev.events.filter((e) => e.kind === "tool_call").map((e) => ((e.input as { value?: { tool?: string } } | undefined)?.value?.tool ?? "a tool"));
            setLive((l) => ({ ...l, steps, runId: r.run_id }));
          })
          .catch(() => {});
      }, 3000);

      // A draft runs with the working copy's charter, tools and skills; a published agent runs as it is.
      // The Test panel fills every variable from its test value (or the setting), draft or published.
      const variables = Object.fromEntries((draft.intent.variables ?? []).flatMap((v) => { const value = v.test_value?.trim() || v.value?.trim(); return value ? [[v.name, value]] : []; }));
      const overrides = status === "draft" ? { instructions: draft.intent.instructions ?? "", tools: draft.intent.tools ?? [], skills: draft.intent.skills ?? [], model: draft.intent.model ?? null, fallback_model: draft.intent.fallback_model ?? null, variables, memory_access: (draft.intent.memory?.access === "none" ? "none" : "read_write") as "none" | "read_write", ...(draftContext(draft.intent.context) ? { context: draftContext(draft.intent.context) } : {}), ...(draft.recursion_limit ? { recursion_limit: draft.recursion_limit } : {}), ...(draft.intent.chain_max_tokens != null ? { chain_max_tokens: draft.intent.chain_max_tokens } : {}), ...(draft.intent.temperature != null ? { temperature: draft.intent.temperature } : {}), ...(draft.intent.memory?.blocks?.length ? { memory_blocks_declared: draft.intent.memory.blocks } : {}) } : { variables };
      const result = await runAndWait(th, agent.assistant_id, [turn], world || undefined, overrides);
      await readThread(th, result.output?.messages);
      setLast(result);
      // A ceiling is not an approval: the run stopped with its work and a
      // person carries it on, so it is told apart here and offered a resume.
      setHalt((result.interrupt?.["rusty.halted"] as Halt | undefined) ?? null);
      if (result.status === "interrupted" && !result.interrupt?.["rusty.halted"]) { const p = (await listApprovals("pending").catch(() => [] as Approval[])).find((a) => a.run_id === result.run_id) ?? null; setPending(p); } else setPending(null);
      await useServer.getState().refresh();
      setObsTick((n) => n + 1);
      if (mode !== "observe") setUnseen(true);
    } catch (err) {
      setFailure(err instanceof Error ? err.message : "the run could not be started");
    } finally { stopWatch(); setRunning(false); setStopping(false); }
  }

  const shown = useMemo(() => {
    const out: { kind: "user" | "trace" | "agent"; text?: string; steps?: { name: string; what: string; detail: string; ok: boolean; raw: string }[]; plan?: { text: string; status: string; note?: string }[] }[] = [];
    const results = new Map<string, string>();
    for (const m of messages) if (m.role === "tool" && m.tool_call_id) results.set(m.tool_call_id, m.content ?? "");
    for (const m of messages) {
      if (m.role === "user") out.push({ kind: "user", text: m.content ?? "" });
      else if (m.role === "assistant" && m.tool_calls?.length) {
        out.push({ kind: "trace", plan: planOf(m.tool_calls), steps: m.tool_calls.map((c) => { const res = results.get(c.id) ?? ""; const tool = c.function?.name ?? "tool"; return { name: tool, what: argSummary(c.function?.arguments ?? ""), detail: resultSummary(res), ok: !/^(ERROR|DENIED|REPEATED)/.test(res), raw: `${tool}(${c.function?.arguments ?? ""})\n\n${res}` }; }) });
      } else if (m.role === "assistant" && m.content) out.push({ kind: "agent", text: m.content });
    }
    return out;
  }, [messages]);

  // ── Build: the Coach's proposals and a request in words ──
  const [buildInput, setBuildInput] = useState("");
  const buildEnd = useRef<HTMLDivElement>(null);
  useEffect(() => { if (mode === "build") buildEnd.current?.scrollIntoView({ block: "end" }); }, [proposals.said, proposals.asking, mode]);
  async function request(text: string) {
    const t = text.trim(); if (!t || proposals.asking) return;
    setBuildInput("");
    await proposals.ask(t);
  }
  async function applyAll() {
    for (const p of proposals.open) { try { await proposals.apply(p); } catch (err) { toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle"); return; } }
    toast(`${proposals.open.length} proposal${proposals.open.length === 1 ? "" : "s"} applied`, "ti-check");
  }
  const flash = (section: string) => { const sec = document.querySelector(`[data-section="${section}"]`) as HTMLElement | null; if (!sec) return; sec.classList.add("flash"); setTimeout(() => sec.classList.remove("flash"), 1200); document.querySelector(".config")?.scrollTo({ top: sec.offsetTop - 80, behavior: "smooth" }); };

  const reset = () => {
    if (mode === "test") { setThread(null); setMessages([]); setLast(null); setFailure(null); setPending(null); toast("New test session", "ti-refresh"); }
    else if (mode === "build") { void proposals.reload(); toast("Build thread reset", "ti-refresh"); }
    else { setObsTick((n) => n + 1); toast("Observe reloaded", "ti-refresh"); }
  };
  const [modelWord, setModelWord] = useState(draft.intent.model ?? "model");
  useEffect(() => { llmProviders().then((p) => { const named = draft.intent.model ? p.providers.find((x) => x.id === draft.intent.model || x.model === draft.intent.model) : null; const use = named ?? p.providers.find((x) => x.id === p.primary); setModelWord((use?.model ?? draft.intent.model ?? "model").split("/").pop()!.toLowerCase()); }).catch(() => {}); }, [draft.intent.model]);

  return (
    <div className="test" data-mode={mode}>
      <div className="test-head">
        <div className="m-seg panel-modes">
          <button className={mode === "build" ? "on" : ""} data-pmode="build" onClick={() => switchMode("build")}><i className="ti ti-sparkles" /> Build</button>
          <button className={mode === "test" ? "on" : ""} data-pmode="test" onClick={() => switchMode("test")}><i className="ti ti-player-play" /> Test</button>
          <button className={mode === "observe" ? "on" : ""} data-pmode="observe" onClick={() => switchMode("observe")}><i className="ti ti-activity" /> Observe <span className={`pm-dot${unseen ? " live" : ""}`} /></button>
        </div>
        <div className="sp" />
        <span className="ts" data-pane-meta="test" hidden={mode !== "test"}>{last ? last.run_id.slice(0, 12) : `${modelWord} · ${status}${world ? ` · ${world}` : ""}`}</span>
        <span className="ts" data-pane-meta="build" hidden={mode !== "build"}>v{agent.version_count} · {proposals.open.length} pending</span>
        <span className="ts" data-pane-meta="observe" hidden={mode !== "observe"}>live</span>
        <button className="m-btn ghost sm icon" title="Reset" data-pane-reset onClick={reset}><i className="ti ti-refresh" /></button>
      </div>

      {/* BUILD: the Coach proposes; a person approves */}
      <div className="pane" data-pane="build" hidden={mode !== "build"}>
        <div className="thread">
          <div className="pane-intro">
            <div className="pi-ic"><i className="ti ti-sparkles" /></div>
            <div className="pi-t">Build with a co-pilot</div>
            <div className="pi-s">Describe a change and the Coach files it against this agent as a reviewable version — instructions, tools, skills or guardrails.</div>
          </div>
          <div className="msg agent"><div className="who">Coach</div><div className="bub">{proposals.coach ? (proposals.open.length ? `${proposals.open.length} version${proposals.open.length === 1 ? "" : "s"} filed for ${draft.name} wait for review below. Apply them here, or tell me what else to change.` : `Nothing is waiting for review on ${draft.name}. Tell me what to change, or ask me to re-scan its runs.`) : "There is no Coach on this server; proposals arrive only as versions someone files."}</div></div>
          {proposals.said.map((m, i) => <div key={i} className={`msg ${m.who === "user" ? "user" : "agent"}`}>{m.who === "coach" && <div className="who">Coach</div>}<div className="bub">{m.text}</div></div>)}
          {proposals.asking && <CoachWorking />}
          <div className="proposals" data-proposals>
            {proposals.list.length === 0 && <div className="pane-empty"><i className="ti ti-circle-check" /> Nothing pending. Ask for a change below.</div>}
            {proposals.list.map((p) => (
              <div key={p.version.version_id} className={`proposal${p.state === "applied" ? " applied" : ""}`} hidden={p.state === "dismissed"}>
                <div className="pp-head"><div className="pp-ic"><i className={`ti ${KIND_ICON[p.section] ?? "ti-file-text"}`} /></div><div className="pp-body"><div className="pp-t">{p.title}</div><div className="pp-d">{p.desc}</div></div>{p.evidence[0] && <span className="m-badge good sm"><span className="dot" /> {p.evidence[0]}</span>}</div>
                {p.diff.length > 0 && <div className="pp-diff">{p.diff.map((d, n) => <div key={n} className={`dl ${d.kind}`}>{d.kind === "del" ? "- " : "+ "}{d.text}</div>)}</div>}
                <div className="pp-foot">
                  <span className="item-tag"><i className={`ti ${p.src === "scan" ? "ti-radar-2" : "ti-sparkles"}`} /> {p.src === "scan" ? "From run analysis" : "From your request"}</span><span className="item-tag">{p.section}</span><span className="sp" />
                  {p.state === "applied" ? <span className="item-tag" style={{ color: "var(--good)", borderColor: "var(--good-line)" }}><i className="ti ti-check" /> Applied to {p.section}</span> : (
                    <>
                      <button className="m-btn ghost sm" data-pp="dismiss" disabled={proposals.busy === p.version.version_id} onClick={() => proposals.dismiss(p, "Dismissed in Build").catch((err) => toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle"))}>Dismiss</button>
                      <button className="m-btn primary sm" data-pp="apply" disabled={proposals.busy === p.version.version_id} onClick={() => proposals.apply(p).then(() => flash(p.section)).catch((err) => toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle"))}><i className="ti ti-check" /> Apply</button>
                    </>
                  )}
                </div>
              </div>
            ))}
          </div>
          <div ref={buildEnd} />
        </div>
        <div className="test-foot">
          <div className="test-presets">
            <span className="preset" data-build="improve" onClick={() => proposals.open.length ? void applyAll() : void proposals.rescan()}>{proposals.open.length ? "Apply all suggestions" : "Re-scan runs"}</span>
            <span className="preset" data-build="tone" onClick={() => void request("Make replies more concise")}>Make replies more concise</span>
            <span className="preset" data-build="guard" onClick={() => void request("Never promise what a tool did not confirm; say so and hand off instead")}>Add a guardrail</span>
          </div>
          <div className="composer">
            <textarea rows={1} placeholder="Tell the Coach what to change…" data-build-input value={buildInput} onChange={(e) => setBuildInput(e.target.value)} onKeyDown={(e) => { if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); void request(buildInput); } }} />
            <button className="composer-send" data-build-send title="Send" disabled={proposals.asking} onClick={() => void request(buildInput)}><i className="ti ti-arrow-up" /></button>
          </div>
        </div>
      </div>

      {/* TEST: the playground */}
      <div className="pane" data-pane="test" hidden={mode !== "test"}>
        <div className="thread" id="thread">
          {shown.length === 0 && !running && <div className="thread-empty"><i className="ti ti-flask" />Send a message or pick a preset to test <b>{draft.name}</b>.</div>}
          {shown.map((s, i) => s.kind === "user" ? (
            <div key={i} className="msg user"><div className="who">You</div><div className="bub">{s.text}</div></div>
          ) : s.kind === "trace" ? (
            <div key={i} className="msg agent"><div className="who">{draft.name}</div>
              {s.plan && <div className="plan-block"><div className="plan-head"><i className="ti ti-list-check" /> Plan · {s.plan.filter((p) => p.status === "done").length} of {s.plan.length} done</div>{s.plan.map((p, j) => <div key={j} className={`plan-step ${p.status}`}><i className={`ti ${p.status === "done" ? "ti-circle-check-filled" : p.status === "skipped" ? "ti-circle-minus" : p.status === "doing" ? "ti-loader-2" : "ti-circle"}`} /> <span>{p.text}</span>{p.note && <span className="plan-note"> — {p.note}</span>}</div>)}</div>}
              <div className="trace">{s.steps?.map((st, j) => (
                <div key={j} className="trace-step" style={{ cursor: "pointer", flexWrap: "wrap" }} data-flow="trace-step" title="Show the call and its result as the agent saw them" onClick={() => setRawStep(rawStep === `${i}:${j}` ? null : `${i}:${j}`)}>
                  <div className="trace-ic" style={{ background: "var(--bg-muted)", color: "var(--ink-600)" }}><i className={`ti ${toolIcon(st.name)}`} /></div>
                  <div className="trace-main"><div className="trace-name" style={{ fontFamily: "inherit" }}>{plainName(st.name)}{st.what && <span style={{ color: "var(--ink-500)", fontWeight: 400 }}> · {st.what}</span>}</div><div className="trace-detail" style={{ fontFamily: "inherit" }}>{st.detail}</div></div>
                  {st.ok ? <i className="ti ti-circle-check-filled trace-check" /> : <i className="ti ti-alert-circle" style={{ color: "var(--bad)" }} />}
                  {rawStep === `${i}:${j}` && <pre style={{ flexBasis: "100%", whiteSpace: "pre-wrap", fontSize: 11, maxHeight: 220, overflow: "auto", margin: "6px 0 0" }}>{st.raw}</pre>}
                </div>
              ))}</div>
            </div>
          ) : (
            <div key={i} className="msg agent"><div className="who">{draft.name}</div><div className="bub md">{renderMarkdown(s.text ?? "")}</div></div>
          ))}
          {running && (
            <div className="msg agent">
              <div className="who">{draft.name}</div>
              {!live.runId ? <div className="thinking"><span /><span /><span /></div> : (
                <div className="bub" style={{ color: "var(--ink-600)" }}>
                  <div style={{ marginBottom: 4, display: "flex", alignItems: "center", gap: 10 }}>
                    <span>{live.steps.length === 0 ? "Thinking" : `${live.steps.length} step${live.steps.length === 1 ? "" : "s"} so far`} · {Math.floor(live.secs / 60)}:{String(live.secs % 60).padStart(2, "0")}</span>
                    {live.runId && <button className="m-btn ghost sm" disabled={stopping} title="The run ends at its next step; its work is kept" onClick={() => { setStopping(true); void cancelRun(live.runId!).then((r) => toast(r.run === "stopping" ? "Stopping — it ends at its next step" : r.note ?? "nothing to stop", "ti-player-stop")).catch((err) => toast(err instanceof Error ? err.message : "the stop was refused", "ti-alert-triangle")); }}><i className="ti ti-player-stop" /> {stopping ? "Stopping…" : "Stop"}</button>}
                  </div>
                  <div style={{ display: "flex", flexWrap: "wrap", gap: 4 }}>{live.steps.slice(-8).map((t, i) => <span key={i} className="m-chip" style={{ fontSize: 11 }} title={t}>{/^[a-z0-9_.:-]+$/i.test(t) ? plainName(t) : t}</span>)}</div>
                </div>
              )}
            </div>
          )}
          {failure && <div className="m-alert" style={{ marginTop: 6 }}><i className="ti ti-alert-triangle" style={{ color: "var(--bad)", fontSize: 16 }} /><div className="a-body"><div className="a-text">{failure}</div></div></div>}
          {pending && !running && (
            <div className="msg agent"><div className="who">{draft.name} · waiting</div>
              <div className="trace">{pending.requests.map((r) => <div key={r.call_id} className="trace-step"><div className="trace-ic" style={{ background: "var(--warn-bg)", color: "var(--warn)" }}><i className="ti ti-hand-stop" /></div><div className="trace-main"><div className="trace-name" style={{ fontFamily: "inherit" }} title={JSON.stringify(r.arguments ?? {})}>{plainName(r.tool)}{argSummary(JSON.stringify(r.arguments ?? {})) && <span style={{ color: "var(--ink-500)", fontWeight: 400 }}> · {argSummary(JSON.stringify(r.arguments ?? {}))}</span>}</div><div className="trace-detail" style={{ fontFamily: "inherit" }}>This changes something outside — it waits for your yes</div></div></div>)}</div>
              <div style={{ display: "flex", gap: 6, marginTop: 8 }}><button className="m-btn ghost sm" disabled={deciding} onClick={() => void decide("deny")}>Deny</button><button className="m-btn primary sm" disabled={deciding} onClick={() => void decide("approve")}><i className="ti ti-check" /> Approve</button></div>
            </div>
          )}
          {halt && !running && (
            <div className="m-alert" style={{ marginTop: 6 }}>
              <i className="ti ti-player-pause" style={{ color: "var(--warn)", fontSize: 16 }} />
              <div className="a-body">
                <div className="a-title">{halt.reason === "budget_ceiling" ? "Stopped at its spend limit" : `Stopped at its step limit${halt.limit ? ` of ${halt.limit}` : ""}`}</div>
                <div className="a-text">It kept the work it had done{halt.crossed ? ` — ${halt.crossed}` : ""}. Carry it on, with more room when the task needs it.</div>
                <div className="as-act" style={{ marginTop: 8 }}>
                  <button className="m-btn accent sm" disabled={resuming} onClick={() => void carryOn(halt.limit ? halt.limit * 2 : undefined)}>{resuming ? "Carrying on…" : "Resume with more room"}</button>
                  <button className="m-btn ghost sm" disabled={resuming} onClick={() => void carryOn()}>Resume as it is</button>
                </div>
              </div>
            </div>
          )}
          {last?.status === "cancelled" && !running && <div className="m-alert" style={{ marginTop: 6 }}><i className="ti ti-player-stop" style={{ color: "var(--ink-600)", fontSize: 16 }} /><div className="a-body"><div className="a-title">Stopped</div><div className="a-text">It ended at the step it was on. What it did so far is kept, and the next message carries on from there.</div></div></div>}
          {last?.status === "interrupted" && !halt && !pending && !running && <div className="m-alert" style={{ marginTop: 6 }}><i className="ti ti-hand-stop" style={{ color: "var(--warn)", fontSize: 16 }} /><div className="a-body"><div className="a-title">Paused before an irreversible action</div><div className="a-text">Decide it under Notifications; the run continues from there.</div></div></div>}
          <div ref={endRef} />
        </div>
        <div className="test-foot">
          <div className="test-presets">
            {fitting.length > 0 && (
              <select className="preset" value={world} onChange={(e) => setWorld(e.target.value)} aria-label="World" style={{ appearance: "none" }}>
                <option value="">Live systems</option>
                {fitting.map((w) => <option key={w.world_id} value={w.name}>Stand-in · {w.name}</option>)}
              </select>
            )}
            {presets.map((p) => <span key={p} className="preset" data-preset={p} onClick={() => void send(p)}>{p.length > 28 ? `${p.slice(0, 28)}…` : p}</span>)}
          </div>
          <div className="composer">
            <textarea id="composerInput" rows={1} placeholder={`Message ${draft.name}…`} value={input} onChange={(e) => setInput(e.target.value)} onKeyDown={(e) => { if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); void send(input); } }} />
            <button className="composer-send" id="sendBtn" onClick={() => void send(input)} disabled={running}><i className="ti ti-arrow-up" /></button>
          </div>
        </div>
      </div>

      {/* OBSERVE: this agent's runs and their events */}
      <ObservePane agent={agent} active={mode === "observe"} tick={obsTick} onReplay={(text) => { switchMode("test"); setInput(text); }} onFix={(text) => { switchMode("build"); void request(text); }} />
    </div>
  );
}

interface Trace { run: Run; status: "ok" | "warn" | "bad" | "live"; latency: number; tokens: number; cost: number | null; steps: { name: string; detail: string; ms: number; bad: boolean }[]; events: RunEvent[] }
type Log = { ts: string; lvl: "info" | "warn" | "error"; src: string; msg: string; at: string };
const STAT = { ok: ["good", "Success"], warn: ["warn", "Escalated"], bad: ["bad", "Failed"], live: ["info", "Running"] } as const;

function ObservePane({ agent, active, tick, onReplay, onFix }: { agent: Assistant; active: boolean; tick: number; onReplay: (text: string) => void; onFix: (text: string) => void }) {
  // This agent's runs come from the server, not from the shared list: that
  // list holds the newest runs across the whole deployment, so a quiet agent
  // disappears behind a busy one as the fleet grows.
  const [runs, setRuns] = useState<Run[]>([]);
  const [tab, setTab] = useState<"traces" | "logs">("traces");
  const [env, setEnv] = useState<"playground" | "production">("playground");
  const [traces, setTraces] = useState<Trace[]>([]);
  const [openId, setOpenId] = useState<string | null>(null);
  const [lvl, setLvl] = useState<"all" | "info" | "warn" | "error">("all");
  const [loading, setLoading] = useState(false);
  // Playground = runs in a stand-in world or started from the studio; Production = the rest (schedules, webhooks, the queue).
  const mine = useMemo(() => runs.filter((r) => { const studio = (r.metadata?.created_by as { kind?: string } | undefined)?.kind === "user" || !!r.worlds?.length; return env === "playground" ? studio : !studio; }).slice(0, 8), [runs, agent.assistant_id, env]);
  useEffect(() => {
    if (!active) return;
    let live = true;
    void listRuns(50, agent.assistant_id).then((r) => { if (live) setRuns(r); }).catch(() => {});
    return () => { live = false; };
  }, [active, agent.assistant_id, tick]);
  useEffect(() => {
    if (!active) return;
    let live = true; setLoading(true);
    Promise.all(mine.map(async (run) => {
      const [full, ev] = await Promise.all([getRun(run.run_id).catch(() => null), runEvents(run.run_id).catch(() => ({ events: [] as RunEvent[] }))]);
      const events = ev.events;
      const first = events[0]?.recorded_at, lastAt = events[events.length - 1]?.recorded_at;
      const latency = first && lastAt ? Math.max(0, new Date(lastAt).getTime() - new Date(first).getTime()) : events.reduce((n, e) => n + (e.latency_ms ?? 0), 0);
      // Steps are the journal's tool calls: the tool named on the input, the answer on the output, the pause where the gate held.
      const val = (x: unknown) => (x && typeof x === "object" && "value" in (x as object) ? (x as { value: unknown }).value : x);
      const steps = events.filter((e) => e.node_id === "tools" && (e.kind === "tool_call" || e.kind === "interrupt")).map((e) => {
        const inp = val(e.input) as { tool?: string; arguments?: unknown } | null;
        const out = val(e.output);
        const text = out == null ? "" : typeof out === "string" ? out : JSON.stringify(out);
        return e.kind === "interrupt"
          ? { name: inp?.tool ?? "gate", detail: "paused for a decision before an irreversible action", ms: e.latency_ms ?? 0, bad: false }
          : { name: inp?.tool ?? "tool", detail: text ? text.replace(/\s+/g, " ").slice(0, 80) : JSON.stringify(inp?.arguments ?? "").slice(0, 80), ms: e.latency_ms ?? 0, bad: e.status !== "ok" || /^"?(ERROR|DENIED)/.test(text) };
      });
      const verdict = full?.verification?.verdict ?? run.verification?.verdict;
      // A run that has not finished reads as running, not as a success: the
      // trace list showed a seven-minute run as "ok" while it was still going.
      const status: Trace["status"] = run.status === "running" || run.status === "pending" ? "live" : run.status === "interrupted" ? "warn" : run.status === "error" || run.status === "failed" || verdict === "failed" ? "bad" : "ok";
      const tokens = full?.usage ? full.usage.prompt_tokens + full.usage.completion_tokens : 0;
      const cost = (full as { spend?: { cost_usd?: number | null } } | null)?.spend?.cost_usd ?? null;
      return { run, status, latency, tokens, cost, steps, events } as Trace;
    })).then((t) => { if (live) setTraces(t); }).finally(() => { if (live) setLoading(false); });
    return () => { live = false; };
  }, [active, tick, mine.map((r) => r.run_id).join("|")]); // eslint-disable-line react-hooks/exhaustive-deps

  const logs: Log[] = useMemo(() => traces.flatMap((t) => t.events.map((e) => {
    const lvl: Log["lvl"] = e.status === "error" || e.status === "failed" ? "error" : e.status === "interrupted" || e.status === "denied" || e.effect === "non_idempotent" ? "warn" : "info";
    const d = new Date(e.recorded_at);
    const src = e.node_id === "agent" ? "model" : e.node_id === "tools" ? "tool" : /routing|memory/.test(e.kind) ? "router" : "run";
    return { ts: `${[d.getHours(), d.getMinutes(), d.getSeconds()].map((n) => String(n).padStart(2, "0")).join(":")}.${String(d.getMilliseconds()).padStart(3, "0")}`, lvl, src, msg: `${e.kind}${e.status ? ` · ${e.status}` : ""}${e.latency_ms != null ? ` · ${e.latency_ms}ms` : ""}${e.cost_usd != null ? ` · $${e.cost_usd.toFixed(4)}` : ""} · ${t.run.run_id.slice(0, 12)}`, at: e.recorded_at };
  })).sort((a, b) => (a.at < b.at ? 1 : -1)), [traces]);
  const sorted = [...traces.map((t) => t.latency)].sort((a, b) => a - b);
  const p50 = sorted.length ? sorted[Math.floor(sorted.length / 2)] : 0;
  const errors = traces.filter((t) => t.status === "bad").length;
  const cost = traces.reduce((n, t) => n + (t.cost ?? 0), 0);
  const kpi = (l: string, v: string, k?: string) => <div className={`obs-kpi${k ? ` ${k}` : ""}`}><span className="k">{l}</span><span className="v mono">{v}</span></div>;
  return (
    <div className="pane" data-pane="observe" hidden={!active}>
      <div className="obs-bar">
        <div className="m-seg sm"><button className={tab === "traces" ? "on" : ""} data-obs="traces" onClick={() => setTab("traces")}>Traces</button><button className={tab === "logs" ? "on" : ""} data-obs="logs" onClick={() => setTab("logs")}>Logs</button></div>
        <div className="sp" />
        <select className="m-select xs" data-obs-env value={env} onChange={(e) => setEnv(e.target.value as typeof env)}><option value="playground">Playground</option><option value="production">Production</option></select>
      </div>
      <div className="thread obs-list" data-obs-pane="traces" hidden={tab !== "traces"}>
        {traces.length === 0 && !loading && <div className="pane-empty"><i className="ti ti-activity" /> No runs yet. Send a message in Test.</div>}
        {loading && traces.length === 0 && <div className="pane-empty"><span className="m-spin" style={{ width: 14, height: 14, borderWidth: 2 }} /> Reading runs…</div>}
        {traces.length > 0 && <div className="obs-kpis">{kpi("Runs", String(traces.length))}{kpi("p50 latency", `${(p50 / 1000).toFixed(1)}s`)}{kpi("Errors", String(errors), errors ? "bad" : undefined)}{kpi("Cost", traces.some((t) => t.cost != null) ? `$${cost.toFixed(4)}` : "—")}</div>}
        {traces.map((t) => { const st = STAT[t.status]; const isOpen = openId === t.run.run_id; return (
          <div key={t.run.run_id} className={`run${isOpen ? " open" : ""}`} data-run={t.run.run_id}>
            <div className="run-head" onClick={() => setOpenId(isOpen ? null : t.run.run_id)}><span className="dot" style={{ background: `var(--${st[0]}-dot)` }} /><span className="mono">{t.run.run_id.slice(0, 12)}</span><span className="run-in">{t.run.asked ?? "—"}</span><span className="mono dim">{ago(t.run.created_at)}</span><i className="ti ti-chevron-down chev" /></div>
            <div className="run-body" hidden={!isOpen}>
              <div className="run-meta"><span className={`m-badge ${st[0]} sm`}><span className="dot" /> {st[1]}</span><span className="mono">{(t.latency / 1000).toFixed(2)}s</span><span className="mono">{t.tokens.toLocaleString()} tok</span><span className="mono">{t.cost != null ? `$${t.cost.toFixed(4)}` : "—"}</span></div>
              <div className="steps">{t.steps.map((s, i) => <div key={i} className={`step${s.bad ? " bad" : ""}`}><div className="step-bar" style={{ width: `${t.latency ? Math.max(4, Math.round((s.ms / t.latency) * 100)) : 4}%` }} /><span className="mono step-n">{s.name}</span><span className="step-d">{s.detail}</span><span className="mono dim">{s.ms ? `${s.ms}ms` : ""}</span></div>)}{t.steps.length === 0 && <div className="step"><span className="step-d">No tool step — the model answered directly.</span></div>}</div>
              <div className="run-act"><button className="m-btn ghost sm" data-replay onClick={() => onReplay(t.run.asked ?? "")}><i className="ti ti-player-play" /> Replay in Test</button><button className="m-btn ghost sm" data-fix onClick={() => onFix(`Fix the failure in run ${t.run.run_id.slice(0, 12)}: ${t.steps[t.steps.length - 1]?.detail ?? t.run.verification?.reason ?? "it did not achieve the outcome"}`)}><i className="ti ti-sparkles" /> Fix in Build</button></div>
            </div>
          </div>
        ); })}
      </div>
      <div className="thread obs-list" data-obs-pane="logs" hidden={tab !== "logs"}>
        <div className="log-filter">{(["all", "info", "warn", "error"] as const).map((l) => <button key={l} className={`auth-opt${lvl === l ? " on" : ""}`} data-lvl={l} onClick={() => setLvl(l)}>{l}</button>)}</div>
        <div className="log-list">
          {logs.filter((l) => lvl === "all" || l.lvl === lvl).map((l, i) => <div key={i} className={`log ${l.lvl}`}><span className="mono ts">{l.ts}</span><span className="lvl">{l.lvl}</span><span className="mono src">{l.src}</span><span className="msg-t">{l.msg}</span></div>)}
          {logs.length === 0 && <div className="pane-empty"><i className="ti ti-list" /> No events yet.</div>}
        </div>
      </div>
    </div>
  );
}

/** The Coach reads the agent and its runs, then files — a minute or more
 * on a reasoning model. The wait says so, with the clock, instead of three dots. */
function CoachWorking() {
  const [secs, setSecs] = useState(0);
  useEffect(() => { const t = setInterval(() => setSecs((n) => n + 1), 1000); return () => clearInterval(t); }, []);
  const stage = secs < 15 ? "reading the agent and its runs" : secs < 60 ? "working out the change" : "still working — the steps are in Observe";
  return <div className="msg agent"><div className="who">Coach</div><div className="bub" style={{ color: "var(--ink-500)" }}><span className="m-spin" style={{ width: 14, height: 14, borderWidth: 2, verticalAlign: -2, marginRight: 8 }} />{stage} · {Math.floor(secs / 60)}:{String(secs % 60).padStart(2, "0")}</div></div>;
}
