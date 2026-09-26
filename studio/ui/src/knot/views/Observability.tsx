import { useEffect, useMemo, useState } from "react";
import { cancelAssignment, cancelRuntimeAgent, cancelTask, continueAssignment, finishAssignment, getRun, judgeVerifier, listAssignments, listAssistants, listRepairs, listRuns, listRuntimeAgents, listGapsAll, listTasks, pauseAssignment, restartRuntimeAgent, reviewVerdict, runEvents, runPayload, runtimeAgentStatus, sweepGaps, taskMetrics, verifierEvidence, type Assignment, type Gap, type PoolMetrics, type RepairRecord, type Run, type RunEvent, type RuntimeAgent, type Task, type VerifierEvidence, type Assistant } from "../../engine/net/client";
import { useServer } from "../../engine/net/server";
import { Badge, OvHead, useOverlay } from "../overlay";
import { ago, toolIcon } from "../data";
import { argSummary, plainName, resultSummary } from "../agents/words";
import { renderMarkdown } from "../../engine/text/markdown";

type St = "ok" | "escalated" | "error";
// A paused run a person has decided waits on no one: it reads by its decision.
const stOf = (r: Run): St => (r.status === "interrupted" && !r.decision ? "escalated" : r.status === "interrupted" && r.decision?.status === "approved" ? "ok" : r.status === "error" || r.status === "failed" || r.verification?.verdict === "failed" ? "error" : "ok");
const TONE: Record<St, "good" | "warn" | "bad"> = { ok: "good", escalated: "warn", error: "bad" };
/** A run's result in a person's words: what the checker found when it looked, else how the run ended. */
const resultOf = (r: Run, st: St): ["good" | "warn" | "bad" | "info", string] =>
  r.status === "interrupted" && r.decision ? (r.decision.status === "approved" ? ["good", "A person said yes"] : ["warn", "A person said no"]) : r.verification?.verdict === "verified" ? ["good", "Done right"] : r.verification?.verdict === "failed" ? ["bad", "Not done"] : st === "escalated" ? ["warn", "Waiting for a person"] : st === "error" ? ["bad", "Went wrong"] : ["info", "Finished"];
/** A run that carries on a paused one after a person decided. */
const continuedAfterYes = (r: Run) => !!(r.metadata as { approval_of?: string } | undefined)?.approval_of;
const LANE_WORDS: Record<string, string> = { Runs: "Runs", Tasks: "Queued work", Assignments: "Handed off", Fleet: "Workers", Repairs: "Repairs", Gaps: "Open questions" };
/** How a run started, in the order the evidence deserves: a run one agent
 * delegated to another names the lead (it read "Manual" before, because the
 * person behind the lead was stamped on it too); a schedule or a webhook
 * names its channel; then who pressed the button. */
const triggerOf = (r: Run, nameOf?: (id: string) => string) => {
  // The wire folds the run's channel into its metadata: `channel` names it,
  // and a delegated run's `delegated_from` names the agent (`agent_id`) and
  // the run that asked.
  const m = (r.metadata ?? {}) as { trigger?: string; channel?: string; delegated_from?: { agent_id?: string; assistant_id?: string; run_id?: string }; created_by?: { kind?: string; name?: string }; studio?: { objective?: string } };
  const ch = m.channel;
  if (ch === "delegation" || m.trigger === "delegation") { const lead = m.delegated_from?.agent_id ?? m.delegated_from?.assistant_id; return lead ? `Asked by ${nameOf ? nameOf(lead) : "another agent"}` : "Another agent"; }
  if (ch === "schedule" || m.trigger === "schedule") return "Its schedule";
  if (ch === "webhook" || m.trigger === "webhook") return "Another system";
  if (ch === "evaluation" || m.trigger === "evaluation") return "A test";
  const by = m.created_by; if (m.studio?.objective) return "The studio"; if (by?.kind === "agent") return `Asked by ${by.name ?? "another agent"}`; if (by?.kind === "service") return by.name ?? "Another system"; if (by?.kind === "user") return by.name && by.name !== "Developer (open mode)" ? by.name : "A person"; return by?.name ?? "Another system";
};

/** Observability: every run the server lists, across agents. */
export function ObservabilityView() {
  const { open } = useOverlay();
  // The shared list holds the newest twenty-five runs, which is a day's worth
  // on a busy box — charted "per day" it drew one bar. The view reads the
  // newest hundred for itself and says so.
  const shared = useServer((s) => s.runs);
  const [fetched, setFetched] = useState<Run[] | null>(null);
  useEffect(() => { listRuns(100).then(setFetched).catch(() => setFetched(null)); }, [shared.length]);
  const runs = fetched ?? shared;
  const assistants = useServer((s) => s.assistants);
  const nameOf = (id: string) => assistants.find((a) => a.assistant_id === id)?.name ?? id.slice(0, 8);
  const [lane, setLane] = useState<"Runs" | "Tasks" | "Assignments" | "Fleet" | "Repairs" | "Gaps">("Runs");
  const [filter, setFilter] = useState<"All" | "Escalated" | "Errors">("All");
  const [q, setQ] = useState("");
  const name = (r: Run) => assistants.find((a) => a.assistant_id === r.assistant_id)?.name ?? "—";
  const shown = runs.filter((r) => filter === "All" || (filter === "Errors" ? stOf(r) === "error" : stOf(r) === "escalated")).filter((r) => !q || `${name(r)} ${r.asked ?? ""} ${r.run_id}`.toLowerCase().includes(q.toLowerCase()));
  const days = useMemo(() => {
    const out: { key: string; label: string; runs: number; errors: number }[] = [];
    const today = new Date(); today.setHours(0, 0, 0, 0);
    for (let i = 13; i >= 0; i--) { const d = new Date(today.getTime() - i * 86_400_000); out.push({ key: d.toISOString().slice(0, 10), label: d.toLocaleDateString(undefined, { month: "short", day: "numeric" }), runs: 0, errors: 0 }); }
    for (const r of runs) { const d = out.find((x) => x.key === r.created_at.slice(0, 10)); if (d) { d.runs++; if (stOf(r) === "error") d.errors++; } }
    return out;
  }, [runs]);
  const bars = (vals: number[], err?: boolean[]) => { const max = Math.max(1, ...vals); return <div className="spark">{vals.map((v, i) => <div key={i} style={{ height: `${Math.round((v / max) * 100)}%` }} title={String(v)} className={err?.[i] ? "err" : undefined} />)}</div>; };
  const axis = <div className="spark-x">{[0, 3, 6, 9, 12].map((i) => <span key={i}>{days[i].label}</span>)}</div>;
  const escal = runs.length ? Math.round((runs.filter((r) => stOf(r) === "escalated").length / runs.length) * 100) : 0;
  const verified = runs.filter((r) => r.verification?.verdict === "verified" || r.verification?.verdict === "failed");
  return (
    <div className="view library active" id="view-analytics">
      <div className="lib-top"><div className="crumbs"><span>Rustynome</span><i className="ti ti-chevron-right" style={{ color: "var(--ink-300)", fontSize: 15 }} /><b>Activity</b></div><div className="sp" /></div>
      <div className="lib-page">
        <div className="lib-hero">
          <div className="lh-ic" style={{ background: "var(--cat-blue-bg)", color: "var(--cat-blue)" }}><i className="ti ti-chart-dots-3" /></div>
          <div className="lh-main"><div className="lib-eyebrow">Monitor</div><h1 className="lib-title">Activity</h1><p className="lib-lead">How your agents are doing: what they got done, where they are waiting for someone, and what went wrong.</p></div>
        </div>
        <div className="lib-stats">
          <Stat v={String(runs.length)} l="Recent runs" />
          <Stat v={verified.length ? String(Math.round((verified.filter((r) => r.verification?.verdict === "verified").length / verified.length) * 100)) : "—"} u={verified.length ? "%" : ""} l="Done right" />
          <Stat v={String(runs.filter((r) => stOf(r) === "error").length)} l="Didn't get it done" />
          <Stat v={String(escal)} u="%" l="Waiting for a person" />
        </div>
        <div className="two-col" style={{ marginBottom: 26 }}>
          <div className="chart-card"><div className="ch"><span className="t">Runs per day · newest {runs.length}</span><span className="v">{days.reduce((n, d) => n + d.runs, 0)}</span></div>{bars(days.map((d) => d.runs))}{axis}</div>
          <div className="chart-card"><div className="ch"><span className="t">Errors per day</span><span className="v" style={{ color: "var(--bad)" }}>{days.reduce((n, d) => n + d.errors, 0)}</span></div>{bars(days.map((d) => d.errors), days.map((d) => d.errors > 0))}{axis}</div>
        </div>
        <div className="tbl-filter"><div className="m-seg">{(["Runs", "Tasks", "Assignments", "Fleet", "Repairs", "Gaps"] as const).map((l) => <button key={l} className={lane === l ? "on" : ""} onClick={() => setLane(l)}>{LANE_WORDS[l]}</button>)}</div>{lane === "Runs" && <div className="m-seg" data-trf style={{ marginLeft: 8 }}>{(["All", "Escalated", "Errors"] as const).map((f) => <button key={f} className={filter === f ? "on" : ""} onClick={() => setFilter(f)}>{f === "Escalated" ? "Waiting" : f === "Errors" ? "Went wrong" : "All"}</button>)}</div>}<span className="sp" />{lane === "Runs" && <div className="lib-search" style={{ maxWidth: 260 }}><i className="ti ti-search" /><input placeholder="Search runs…" data-libsearch value={q} onChange={(e) => setQ(e.target.value)} /></div>}</div>
        {lane === "Runs" && <VerifierCard />}
        {lane === "Runs" && <div className="lib-table-wrap"><table className="m-table">
          <thead><tr><th>Agent</th><th>Asked</th><th>Started by</th><th>Result</th><th>When</th></tr></thead>
          <tbody data-trb>
            {shown.map((r) => { const st = stOf(r); return (
              <tr key={r.run_id} className="clickable" onClick={() => open("drawer", <RunDrawer run={r} agent={name(r)} />)}>
                <td style={{ fontWeight: 600 }}>{name(r)}</td>
                <td style={{ color: "var(--ink-600)" }}>{r.asked ? (r.asked.length > 70 ? `${r.asked.slice(0, 70)}…` : r.asked) : <span style={{ color: "var(--ink-500)" }}>{continuedAfterYes(r) ? "Continued after a person's yes" : "—"}</span>}</td>
                <td><span className="item-tag">{triggerOf(r, nameOf)}</span></td>
                <td>{(() => { const [tone, words] = resultOf(r, st); return <Badge tone={tone}>{words}</Badge>; })()}</td>
                <td style={{ color: "var(--ink-500)" }}>{ago(r.created_at)}</td>
              </tr>
            ); })}
            {shown.length === 0 && <tr><td colSpan={5} style={{ color: "var(--ink-500)", textAlign: "center", padding: 24 }}>No run matches.</td></tr>}
          </tbody>
        </table></div>}
        {lane === "Tasks" && <TasksLane />}
        {lane === "Assignments" && <AssignmentsLane />}
        {lane === "Fleet" && <FleetLane />}
        {lane === "Repairs" && <RepairsLane />}
        {lane === "Gaps" && <GapsLane openRun={(id) => { const run = runs.find((r) => r.run_id === id); if (run) open("drawer", <RunDrawer run={run} agent={name(run)} />); }} />}
      </div>
    </div>
  );
}

/** One run in full: its numbers, the trace from the journal, what was asked, what came back. */
/** A journaled payload kept by reference (a large tool result): `{kind: "artifact", value: {sha256, …}}`. */
function isArtifactRef(x: unknown): boolean {
  return !!x && typeof x === "object" && (x as { kind?: string }).kind === "artifact" && typeof (x as { value?: { sha256?: unknown } }).value?.sha256 === "string";
}

export function RunDrawer({ run, agent }: { run: Run; agent: string }) {
  const [events, setEvents] = useState<RunEvent[]>([]);
  const [rawStep, setRawStep] = useState<number | null>(null);
  const [details, setDetails] = useState(false);
  const [full, setFull] = useState<Awaited<ReturnType<typeof getRun>> | null>(null);
  const loadRun = () => { runEvents(run.run_id).then((e) => setEvents(e.events)).catch(() => {}); getRun(run.run_id).then(setFull).catch(() => {}); };
  useEffect(loadRun, [run.run_id]); // eslint-disable-line react-hooks/exhaustive-deps
  // A running run's page follows it: the progress record and the trace grow until it ends.
  const live = (full?.status ?? run.status) === "running" || (full?.status ?? run.status) === "pending";
  useEffect(() => { if (!live) return; const t = setInterval(loadRun, 4000); return () => clearInterval(t); }, [live]); // eslint-disable-line react-hooks/exhaustive-deps
  const progress = full?.progress ?? null;
  const st = stOf(full ? { ...run, status: full.status, verification: full.verification ?? run.verification } : run);
  const first = events[0]?.recorded_at, last = events[events.length - 1]?.recorded_at;
  const latency = first && last ? Math.max(0, new Date(last).getTime() - new Date(first).getTime()) : 0;
  const val = (x: unknown) => (x && typeof x === "object" && "value" in (x as object) ? (x as { value: unknown }).value : x);
  // A large tool result is journaled by reference: fetch its bytes, so the
  // trace row shows what the tool answered rather than a hash.
  const [fetched, setFetched] = useState<Record<string, unknown>>({});
  useEffect(() => {
    const refs = events.filter((e) => e.node_id === "tools" && e.kind === "tool_call" && isArtifactRef(e.output)).map((e) => (e.output as { value: { sha256: string } }).value.sha256).filter((sha) => !(sha in fetched));
    if (refs.length === 0) return;
    Promise.all(refs.map((sha) => runPayload(run.run_id, sha).then((p) => [sha, p] as const).catch(() => [sha, null] as const))).then((pairs) => setFetched((f) => ({ ...f, ...Object.fromEntries(pairs) })));
  }, [events]); // eslint-disable-line react-hooks/exhaustive-deps
  const steps = events.filter((e) => e.node_id === "tools" && (e.kind === "tool_call" || e.kind === "interrupt")).map((e) => { const inp = val(e.input) as { tool?: string; arguments?: unknown } | null; const out = isArtifactRef(e.output) ? fetched[(e.output as { value: { sha256: string } }).value.sha256] ?? null : val(e.output); const text = out == null ? "" : typeof out === "string" ? out : JSON.stringify(out); const args = JSON.stringify(inp?.arguments ?? {}); return { name: inp?.tool ?? (e.kind === "interrupt" ? "gate" : "tool"), what: argSummary(args), detail: e.kind === "interrupt" ? "waited for a person's decision" : resultSummary(text), raw: `${inp?.tool ?? ""}(${args})\n\n${text}`, ms: e.latency_ms ?? 0, ok: e.status === "ok" && !/^"?(ERROR|DENIED)/.test(text), paused: e.kind === "interrupt" }; });
  const tokens = full?.usage ? full.usage.prompt_tokens + full.usage.completion_tokens : null;
  const cost = (full as { spend?: { cost_usd?: number | null } } | null)?.spend?.cost_usd ?? null;
  const reply = [...(full?.output?.messages ?? [])].reverse().find((m) => m.role === "assistant" && m.content && !(m.tool_calls?.length))?.content ?? null;
  return (
    <div className="m-drawer" style={{ width: "min(560px,100%)" }}>
      <OvHead icon="ti-timeline" title={`${agent} · run`} sub={`${triggerOf(run, (id) => useServer.getState().assistants.find((a) => a.assistant_id === id)?.name ?? id.slice(0, 8))} · ${ago(run.created_at)}`} />
      <div className="ov-body">
        <div className="cat-label"><span>Asked</span><span className="ln" /></div>
        <div style={{ whiteSpace: "pre-wrap", fontSize: 14 }}>{run.asked ?? (continuedAfterYes(run) ? "Continued after a person's yes to what it was doing" : "—")}</div>
        {reply && <><div className="cat-label" style={{ marginTop: 18 }}><span>Answered</span><span className="ln" /></div><div className="rn-md" style={{ fontSize: 14 }}>{renderMarkdown(reply)}</div></>}
        <div className="cat-label" style={{ marginTop: 18 }}><span>Result</span><span className="ln" /></div>
        <div style={{ display: "flex", gap: 8, alignItems: "baseline", flexWrap: "wrap" }}>{(() => { const [tone, words] = resultOf(full ? { ...run, status: full.status, verification: full.verification ?? run.verification } : run, st); return <Badge tone={tone}>{words}</Badge>; })()}{run.verification?.reason && <span className="m-hint" style={{ margin: 0 }}>The checker: {run.verification.reason}</span>}</div>
        {run.verification && <VerdictReviewRow run={run} />}
        {live && progress && (progress.plan || progress.tool_calls > 0) && <>
          <div className="cat-label"><span>Progress</span><span className="ln" /></div>
          <div className="m-hint" data-run-progress>{live ? <><span className="m-spin" style={{ width: 12, height: 12, borderWidth: 2, marginRight: 6 }} />Working — </> : ""}{progress.tool_calls} tool call{progress.tool_calls === 1 ? "" : "s"}{progress.last_tool ? ` · last ${progress.last_tool}` : ""}{progress.plan ? ` · plan ${progress.plan.done} of ${progress.plan.steps.length} done` : ""}{progress.updated_at ? ` · ${ago(progress.updated_at)}` : ""}</div>
          {progress.plan && <div className="trace" style={{ marginBottom: 14 }}>{progress.plan.steps.map((s, i) => <div key={i} className="trace-step" data-plan-step={s.status}><div className="trace-ic"><i className={`ti ${s.status === "done" ? "ti-check" : s.status === "doing" ? "ti-player-play" : s.status === "skipped" ? "ti-arrow-forward" : "ti-circle"}`} /></div><div className="trace-main"><div className="trace-name">{s.text}</div>{s.note && <div className="trace-detail">{s.note}</div>}</div><span className="item-tag">{s.status}</span></div>)}</div>}
        </>}
        <div className="cat-label" style={{ marginTop: 18 }}><span>What it did</span><span className="ln" /></div>
        <div className="trace">{steps.map((s, i) => <div key={i} className="trace-step" style={{ cursor: "pointer", flexWrap: "wrap" }} onClick={() => setRawStep(rawStep === i ? null : i)} title="Show the call and its result as the agent saw them"><div className="trace-ic"><i className={`ti ${s.paused ? "ti-hand-stop" : toolIcon(s.name)}`} /></div><div className="trace-main"><div className="trace-name" style={{ fontFamily: "inherit" }}>{s.paused ? "Waited for a person" : plainName(s.name)}{s.what && <span style={{ color: "var(--ink-500)", fontWeight: 400 }}> · {s.what}</span>}</div><div className="trace-detail" style={{ fontFamily: "inherit" }}>{s.detail}</div></div>{s.paused ? <i className="ti ti-hand-stop" style={{ color: "var(--warn)" }} /> : s.ok ? <i className="ti ti-circle-check-filled trace-check" /> : <i className="ti ti-circle-x-filled" style={{ color: "var(--bad)", fontSize: 15 }} />}{rawStep === i && <pre style={{ flexBasis: "100%", whiteSpace: "pre-wrap", fontSize: 11, maxHeight: 220, overflow: "auto", margin: "6px 0 0" }}>{s.raw}</pre>}</div>)}{steps.length === 0 && <div className="m-hint">It answered without using a tool.</div>}</div>
        <div className="m-hint" style={{ marginTop: 14 }}>Took {(latency / 1000).toFixed(1)}s{tokens != null ? ` · ${tokens.toLocaleString()} tokens` : ""}{cost != null ? ` · $${cost.toFixed(3)}` : ""}</div>
        <button className="m-btn ghost sm" style={{ marginTop: 8 }} data-flow="run-details" onClick={() => setDetails((v) => !v)}>{details ? "Hide details" : "Details: model calls and run ids"}</button>
        {details && <>
        <ModelCalls run={run.run_id} events={events} />
        <div className="cat-label" style={{ marginTop: 22 }}><span>Metadata</span><span className="ln" /></div>
        <div className="kv"><span className="k">Run ID</span><span className="v mono">{run.run_id}</span><span className="k">Thread</span><span className="v mono">{run.thread_id}</span><span className="k">Graph</span><span className="v">{run.graph}</span>{run.worlds?.length ? <><span className="k">World</span><span className="v">{run.worlds.map((w) => w.name).join(", ")}</span></> : null}{run.connections?.length ? <><span className="k">Connections</span><span className="v">{run.connections.map((c) => c.name).join(", ")}</span></> : null}</div>
        </>}
      </div>
      <div className="ov-foot"><div className="sp" /><button className="m-btn secondary" data-close>Close</button></div>
    </div>
  );
}

/** The prompt is the evidence: every model call of the run with what it cost, and the assembled request on demand — the sections, the summary that replaced the older turns, the notice when it was clipped. A request too large to ride inline is fetched by its content address. */
function ModelCalls({ run, events }: { run: string; events: RunEvent[] }) {
  const [open, setOpen] = useState<Record<number, string | null>>({});
  const calls = events.filter((e) => e.kind === "model_call");
  if (calls.length === 0) return null;
  const isArtifact = (x: unknown) => !!x && typeof x === "object" && (x as { kind?: string }).kind === "artifact";
  const render = (payload: unknown) => {
    const messages = (payload as { messages?: { role?: string; content?: string | null; tool_calls?: { function?: { name?: string } }[] }[] } | null)?.messages ?? (Array.isArray(payload) ? (payload as never[]) : null);
    if (!messages) return JSON.stringify(payload, null, 1).slice(0, 6000);
    return messages.map((m) => `▸ ${(m.role ?? "?").toUpperCase()}${m.tool_calls?.length ? ` → ${m.tool_calls.map((c) => c.function?.name ?? "tool").join(", ")}` : ""}: ${(m.content ?? "").length > 3000 ? `${(m.content ?? "").slice(0, 2600)} … ${(m.content ?? "").slice(-300)}` : m.content ?? ""}`).join("\n\n");
  };
  const show = async (e: RunEvent) => {
    if (open[e.seq] !== undefined) { setOpen((o) => { const n = { ...o }; delete n[e.seq]; return n; }); return; }
    const input = e.input as { kind?: string; value?: unknown } | undefined;
    try {
      const payload = isArtifact(input) ? await runPayload(run, (input!.value as { sha256: string }).sha256) : input?.value;
      setOpen((o) => ({ ...o, [e.seq]: render(payload) }));
    } catch (err) { setOpen((o) => ({ ...o, [e.seq]: err instanceof Error ? err.message : "the server refused" })); }
  };
  const tokensOf = (e: RunEvent) => (e.tokens && typeof e.tokens === "object" ? (e.tokens as { prompt_tokens?: number }).prompt_tokens ?? null : null);
  // The model that answered, as the provider reported it: a fallback shows as a different name from the other calls.
  const modelOf = (e: RunEvent) => { const o = e.output as { kind?: string; value?: { model?: unknown } } | undefined; return o && o.kind !== "artifact" && typeof o.value?.model === "string" ? o.value.model : null; };
  const models = new Set(calls.map(modelOf).filter((m): m is string => !!m));
  const first = modelOf(calls[0]);
  const bytesOf = (e: RunEvent) => { const i = e.input as { kind?: string; value?: { bytes?: number } } | undefined; return isArtifact(i) ? i?.value?.bytes ?? null : null; };
  return (
    <>
      <div className="cat-label" style={{ marginTop: 22 }}><span>Model calls</span><span className="ln" /></div>
      <div className="trace">{calls.map((e, i) => {
        const compaction = /pipeline/.test(String((e as { parent?: unknown }).parent ?? ""));
        const p = tokensOf(e), b = bytesOf(e);
        return <div key={e.seq} className="trace-step" style={{ display: "block" }}>
          <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
            <span style={{ fontWeight: 600 }}>{compaction ? "summary of the older turns" : `call ${i + 1}`}</span>
            <span style={{ color: "var(--ink-500)", fontSize: 12 }}>{p != null ? `${p.toLocaleString()} prompt tokens` : ""}{b != null ? ` · ${(b / 1024).toFixed(1)} KB request` : ""}{modelOf(e) ? <> · answered by <span style={{ fontFamily: "var(--font-mono)" }}>{modelOf(e)}</span>{models.size > 1 && modelOf(e) !== first ? <span style={{ color: "var(--warn)", marginLeft: 6 }} title="A different model from the run's other calls: the primary did not answer and the fallback did">· fell back</span> : null}</> : null}</span>
            <span className="sp" />
            <button className="m-btn ghost sm" onClick={() => show(e)}>{open[e.seq] !== undefined ? "Hide the prompt" : "Show the prompt"}</button>
          </div>
          {open[e.seq] !== undefined && <div className="pre" style={{ marginTop: 8, whiteSpace: "pre-wrap", maxHeight: 420, overflow: "auto", fontSize: 12 }}>{open[e.seq]}</div>}
        </div>;
      })}</div>
    </>
  );
}

function Stat({ v, u, l }: { v: string; u?: string; l: string }) {
  return <div className="lib-stat"><div className="ls-v">{v}{u && <span className="u">{u}</span>}</div><div className="ls-l">{l}</div></div>;
}

/** The gap backlog: what agents lacked, in priority order — what closes each, how many it affects, the run that needed it. A sweep closes what the platform's tools now satisfy. */
function GapsLane({ openRun }: { openRun: (runId: string) => void }) {
  const { toast } = useOverlay();
  const [gapsAll, setGapsAll] = useState<Gap[]>([]);
  const [claimedAll, setClaimedAll] = useState<Gap[]>([]);
  const [closedAll, setClosedAll] = useState<Gap[]>([]);
  const [showClosed, setShowClosed] = useState(false);
  // The agents, to name a gap's filer and to narrow the backlog to one
  // agent's gaps: what one agent still lacks is the question its builder asks.
  const [agents, setAgents] = useState<Assistant[]>([]);
  const [filer, setFiler] = useState<string>("");
  const load = () => { listGapsAll().then((r) => { setGapsAll(r.work_order); setClaimedAll(r.claimed); setClosedAll(r.closed); }).catch(() => {}); };
  useEffect(() => { load(); listAssistants().then(setAgents).catch(() => {}); }, []);
  const agentOf = (g: Gap): Assistant | null => { const f = g.filer ?? ""; return f ? agents.find((a) => a.assistant_id === f || f.endsWith(`:${a.assistant_id}`)) ?? null : null; };
  const filers = useMemo(() => { const seen = new Map<string, string>(); for (const g of [...gapsAll, ...claimedAll, ...closedAll]) { const a = agentOf(g); if (a && !seen.has(a.assistant_id)) seen.set(a.assistant_id, a.name); } return [...seen.entries()].sort((x, y) => x[1].localeCompare(y[1])); }, [gapsAll, claimedAll, closedAll, agents]);
  const mine = (g: Gap) => !filer || agentOf(g)?.assistant_id === filer;
  const gaps = gapsAll.filter(mine);
  const claimed = claimedAll.filter(mine);
  const closed = closedAll.filter(mine);
  const closesWhen = (g: Gap) => {
    if (g.closes_on_tools.length) return `${g.closes_on_tools.join(", ")} becomes available`;
    const c = g.closure_criteria;
    if (typeof c === "string") return c === "business_decision_required" ? "the business decides" : c;
    const kind = Object.keys(c)[0];
    if (kind === "business_decision_required") return "the business decides";
    if (kind === "block_filled") return `the ${(c[kind] as { block_label: string }).block_label} block is filled`;
    if (kind === "artifact_promoted") return "the candidate is promoted";
    if (g.status === "trial_pending") return "a run claimed it — its verdict decides";
    return "an agent answers it and the verifier confirms";
  };
  const asked = (g: Gap) => g.subject.question_shape?.text?.replace(/^"|"$/g, "") ?? g.subject.intent?.intent_id ?? "";
  const runOf = (g: Gap) => g.evidence.find((e) => e.kind === "run_receipt")?.id ?? null;
  const sweep = () => sweepGaps().then((r) => { const released = (r.claims ?? []).filter((c) => c.released).length; const settled = (r.claims ?? []).filter((c) => c.settled).length; const parts = [r.closed_on_capability.length ? `${r.closed_on_capability.length} closed — the tool arrived` : "", released ? `${released} claim${released === 1 ? "" : "s"} released back to the queue` : "", settled ? `${settled} claim${settled === 1 ? "" : "s"} settled on a late verdict` : "", (r.recall_answered ?? []).length ? `${r.recall_answered!.length} memory miss${r.recall_answered!.length === 1 ? "" : "es"} answered since` : ""].filter(Boolean); toast(parts.length ? parts.join(" · ") : "Nothing to close yet", "ti-check"); load(); }).catch((err) => toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle"));
  return (
    <>
      <div style={{ display: "flex", alignItems: "center", gap: 8, marginBottom: 12 }}><span className="m-hint">Filed by agents when a tool or a fact was missing. A gap that names a tool closes by itself when that tool is connected; any other closes when an agent working the backlog (gaps.work_order) answers it and the run is verified.</span><span className="sp" />{filers.length > 0 && <select className="m-select sm" data-flow="gaps-filer" value={filer} onChange={(e) => setFiler(e.target.value)} title="Only the gaps one agent filed"><option value="">Every agent</option>{filers.map(([id, name]) => <option key={id} value={id}>{name}</option>)}</select>}<button className="m-btn ghost sm" onClick={sweep}><i className="ti ti-refresh" /> Sweep</button></div>
      <div className="lib-table-wrap"><table className="m-table"><thead><tr><th>What was missing</th><th>Asked</th><th>Closes when</th><th>Filed by</th><th className="num">Affects</th><th>Filed</th><th></th></tr></thead><tbody>
        {gaps.slice(0, 60).map((g) => { const run = runOf(g); return <tr key={g.gap_id}><td style={{ maxWidth: 360 }}>{g.statement.length > 160 ? `${g.statement.slice(0, 160)}…` : g.statement}</td><td style={{ color: "var(--ink-600)", maxWidth: 220 }}>{asked(g).length > 90 ? `${asked(g).slice(0, 90)}…` : asked(g)}</td><td>{g.closes_on_tools.length ? <span className="mono" style={{ fontFamily: "var(--font-mono)", fontSize: 12 }}>{closesWhen(g)}</span> : <span style={{ color: "var(--ink-500)" }}>{closesWhen(g)}</span>}</td><td>{agentOf(g) ? <a href={`/agents/${agentOf(g)!.assistant_id}`} title={g.origin.replace(/_/g, " ")}>{agentOf(g)!.name}</a> : <Badge tone={g.origin === "agent_declared" ? "info" : g.origin === "platform" ? "accent" : "warn"}>{g.origin === "agent_declared" ? "an agent" : g.origin === "platform" ? "the platform" : g.origin.replace(/_/g, " ")}</Badge>}</td><td className="num">{g.volume}</td><td style={{ color: "var(--ink-500)" }}>{ago(g.filed_at)}</td><td>{run && <button className="m-btn ghost sm" onClick={() => openRun(run)}>The run</button>}</td></tr>; })}
        {gaps.length === 0 && <tr><td colSpan={7} style={{ color: "var(--ink-500)", textAlign: "center", padding: 24 }}>{filer ? "No open gap from this agent." : "No open gap — every agent had what it needed."}</td></tr>}
      </tbody></table></div>
      {claimed.length > 0 && <>
        <div className="cat-label" style={{ marginTop: 18 }}><span>Claimed — waiting on a run's verdict</span><span className="ln" /></div>
        <div className="lib-table-wrap"><table className="m-table"><thead><tr><th>What was missing</th><th>Asked</th><th>State</th><th>Since</th></tr></thead><tbody>
          {claimed.slice(0, 20).map((g) => <tr key={g.gap_id}><td style={{ maxWidth: 360 }}>{g.statement.length > 120 ? `${g.statement.slice(0, 120)}…` : g.statement}</td><td style={{ color: "var(--ink-600)", maxWidth: 220 }}>{asked(g).slice(0, 80)}</td><td><Badge tone="info">{g.status.replace(/_/g, " ")}</Badge></td><td style={{ fontFamily: "var(--font-mono)", fontSize: "var(--fs-xs)", color: "var(--ink-500)" }}>{ago(g.updated_at ?? g.filed_at)}</td></tr>)}
        </tbody></table></div>
      </>}
      <div className="cat-label" style={{ marginTop: 18, cursor: "pointer" }} onClick={() => setShowClosed((v) => !v)}><span>Closed <span className="cnt">{closed.length}</span> {showClosed ? "▾" : "▸"}</span><span className="ln" /></div>
      {showClosed && <div className="lib-table-wrap"><table className="m-table"><thead><tr><th>What was missing</th><th>Asked</th><th>Closed by</th><th>When</th></tr></thead><tbody>
        {closed.map((g) => { const res = g.resolution ?? ""; const run = res.startsWith("run:") ? res.split(":")[1] : null; return <tr key={g.gap_id}><td style={{ maxWidth: 360 }}>{g.statement.length > 120 ? `${g.statement.slice(0, 120)}…` : g.statement}</td><td style={{ color: "var(--ink-600)", maxWidth: 220 }}>{asked(g).slice(0, 80)}</td><td>{run ? <a href="#" onClick={(e) => { e.preventDefault(); openRun(run); }}>a verified run</a> : res.startsWith("capability:") ? `the tool ${res.slice("capability:".length)} arrived` : res.startsWith("block:") ? "memory answered it later" : res || "—"}</td><td style={{ fontFamily: "var(--font-mono)", fontSize: "var(--fs-xs)", color: "var(--ink-500)" }}>{ago(g.updated_at ?? g.filed_at)}</td></tr>; })}
        {closed.length === 0 && <tr><td colSpan={4} style={{ color: "var(--ink-500)", textAlign: "center", padding: 16 }}>Nothing closed yet.</td></tr>}
      </tbody></table></div>}
    </>
  );
}

/** The durable task queue: pools and their depth, every task with its state, cancel. */
function TasksLane() {
  const { toast } = useOverlay();
  const [tasks, setTasks] = useState<Task[]>([]);
  const [pools, setPools] = useState<PoolMetrics[]>([]);
  const load = () => { listTasks().then(setTasks).catch(() => {}); taskMetrics().then((m) => setPools(m.pools)).catch(() => {}); };
  useEffect(load, []);
  const tone = (t: Task["status"]) => (t === "completed" ? "good" : t === "failed" || t === "dead" ? "bad" : t === "leased" ? "info" : t === "cancelled" ? "warn" : "warn");
  return (
    <>
      {pools.length > 0 && <div className="lib-stats" style={{ gridTemplateColumns: `repeat(${Math.min(4, pools.length)}, 1fr)`, marginBottom: 16 }}>{pools.slice(0, 4).map((p) => <div key={p.pool} className="lib-stat"><div className="ls-v">{p.queue_depth}<span className="u">queued</span></div><div className="ls-l">{p.pool} · {p.leased} leased{p.concurrency_limit ? ` of ${p.concurrency_limit}` : ""}</div></div>)}</div>}
      <div className="lib-table-wrap"><table className="m-table"><thead><tr><th>Task</th><th>Pool</th><th>Kind</th><th>Status</th><th className="num">Attempt</th><th>Run</th><th></th></tr></thead><tbody>
        {tasks.slice(0, 60).map((t) => <tr key={t.task_id}><td><span className="mono" style={{ fontFamily: "var(--font-mono)", fontWeight: 600 }}>{t.task_id.slice(0, 12)}</span></td><td><span className="item-tag">{t.pool}</span></td><td style={{ color: "var(--ink-600)" }}>{t.kind}{t.waiting_on ? ` · waiting on ${t.waiting_on.agent}` : ""}</td><td><Badge tone={tone(t.status)}>{t.status}</Badge>{t.last_error && <span style={{ fontSize: "var(--fs-xs)", color: "var(--bad)", marginLeft: 6 }}>{t.last_error.slice(0, 60)}</span>}</td><td className="num" style={{ fontFamily: "var(--font-mono)", fontSize: "var(--fs-xs)" }}>{t.attempt}/{t.max_attempts}</td><td style={{ fontFamily: "var(--font-mono)", fontSize: "var(--fs-xs)", color: "var(--ink-500)" }}>{t.run_id?.slice(0, 12) ?? "—"}</td><td>{(t.status === "queued" || t.status === "leased") && <button className="m-btn ghost sm" onClick={async () => { try { await cancelTask(t.task_id); toast("Task cancelled"); load(); } catch (err) { toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle"); } }}>Cancel</button>}</td></tr>)}
        {tasks.length === 0 && <tr><td colSpan={7} style={{ color: "var(--ink-500)", textAlign: "center", padding: 24 }}>The queue is empty.</td></tr>}
      </tbody></table></div>
    </>
  );
}

/** Assignments: outcomes delegated to an agent, worked in rounds, steered by a person. */
function AssignmentsLane() {
  const { toast } = useOverlay();
  const agents = useServer((s) => s.assistants);
  const agentName = (o: { agent_id?: string; principal_id: string }) => agents.find((x) => x.assistant_id === (o.agent_id ?? o.principal_id))?.name ?? "an agent";
  const [list, setList] = useState<Assignment[]>([]);
  const load = () => { listAssignments().then(setList).catch(() => {}); };
  useEffect(load, []);
  const say = (err: unknown) => toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle");
  const tone = (s: Assignment["state"]) => (s === "done" ? "good" : s === "blocked" || s === "cancelled" ? "bad" : s === "waiting" || s === "paused" ? "warn" : "info");
  return (
    <div className="lib-table-wrap"><table className="m-table"><thead><tr><th>Agent</th><th>Asked</th><th>From</th><th>State</th><th className="num">Rounds</th><th>World</th><th></th></tr></thead><tbody>
      {list.slice(0, 40).map((a) => <tr key={a.assignment_id}><td style={{ fontWeight: 600 }}>{a.assistant_name}</td><td style={{ color: "var(--ink-600)" }}>{a.request.length > 80 ? `${a.request.slice(0, 80)}…` : a.request}</td><td style={{ color: "var(--ink-600)", fontSize: "var(--fs-xs)" }} title={a.owner.kind === "agent" ? `Delegated by an agent from run ${"run_id" in a.owner ? a.owner.run_id ?? "" : ""}${a.chain ? ` · ${a.chain.depth} deep in agent-started work` : ""}` : undefined}>{a.owner.kind === "agent" ? <><i className="ti ti-robot" style={{ fontSize: 11 }} /> {agentName(a.owner)}{a.chain ? ` · depth ${a.chain.depth}` : ""}{a.told && a.told !== "none" && <span title="Told through its memory: the outcome is a note under this assignment's key, recalled at the start of its next run and by the words that name the work" style={{ marginLeft: 6, color: "var(--ink-500)" }}>· told</span>}{a.chain_spend && <span title={`The chain of work this agent started has spent ${a.chain_spend.spent_tokens.toLocaleString()} of its ${a.chain_spend.max_tokens.toLocaleString()}-token cap across ${a.chain_spend.runs} run${a.chain_spend.runs === 1 ? "" : "s"}${a.chain_spend.over ? " — spent: nothing more starts from it until a person raises the cap on the delegating agent" : ""}`} style={{ marginLeft: 6, color: a.chain_spend.over ? "var(--bad)" : "var(--ink-500)" }}>· {Math.round(a.chain_spend.spent_tokens / 1000)}k of {Math.round(a.chain_spend.max_tokens / 1000)}k{a.chain_spend.over ? " spent" : ""}</span>}</> : a.owner.name}</td><td><Badge tone={tone(a.state)}>{a.state}</Badge>{a.state_reason && <span style={{ fontSize: "var(--fs-xs)", color: "var(--ink-500)", marginLeft: 6 }}>{a.state_reason.slice(0, 60)}</span>}</td><td className="num" style={{ fontFamily: "var(--font-mono)", fontSize: "var(--fs-xs)" }}>{a.rounds_used}/{a.max_rounds}</td><td style={{ fontSize: "var(--fs-xs)" }}>{a.world_name ?? "live"}</td><td style={{ whiteSpace: "nowrap" }}>
        {a.state === "working" && <button className="m-btn ghost sm" onClick={() => pauseAssignment(a.assignment_id).then(load).catch(say)}>Pause</button>}
        {(a.state === "paused" || a.state === "waiting" || a.state === "blocked") && <button className="m-btn ghost sm" onClick={() => continueAssignment(a.assignment_id).then(load).catch(say)}>Continue</button>}
        {a.state !== "done" && a.state !== "cancelled" && <><button className="m-btn ghost sm" onClick={() => finishAssignment(a.assignment_id).then(load).catch(say)}>Done</button><button className="m-btn ghost sm" onClick={() => cancelAssignment(a.assignment_id).then(load).catch(say)}>Cancel</button></>}
      </td></tr>)}
      {list.length === 0 && <tr><td colSpan={7} style={{ color: "var(--ink-500)", textAlign: "center", padding: 24 }}>No assignment yet.</td></tr>}
    </tbody></table></div>
  );
}

/** The runtime fleet: live agent instances, their status, restart and cancel. */
function FleetLane() {
  const { toast } = useOverlay();
  const [agents, setAgents] = useState<RuntimeAgent[]>([]);
  const [status, setStatus] = useState<Record<string, Record<string, unknown>>>({});
  const load = () => { listRuntimeAgents().then((l) => { setAgents(l); l.slice(0, 30).forEach((a) => runtimeAgentStatus(a.agent_id).then((s) => setStatus((x) => ({ ...x, [a.agent_id]: s }))).catch(() => {})); }).catch(() => {}); };
  useEffect(load, []);
  const say = (err: unknown) => toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle");
  const word = (s?: Record<string, unknown>) => (s ? String(s.state ?? s.status ?? s.phase ?? Object.keys(s)[0] ?? "—") : "…");
  return (
    <div className="lib-table-wrap"><table className="m-table"><thead><tr><th>Instance</th><th>Manifest</th><th>Team</th><th>Status</th><th>Since</th><th></th></tr></thead><tbody>
      {agents.slice(0, 60).map((a) => <tr key={a.agent_id}><td><span className="mono" style={{ fontFamily: "var(--font-mono)", fontWeight: 600 }}>{a.agent_id.slice(0, 16)}</span></td><td>{String(a.manifest?.name ?? "—")}</td><td style={{ fontSize: "var(--fs-xs)" }}>{a.team_id ?? "—"}</td><td><Badge tone="info">{word(status[a.agent_id])}</Badge></td><td style={{ fontFamily: "var(--font-mono)", fontSize: "var(--fs-xs)", color: "var(--ink-500)" }}>{ago(a.created_at)}</td><td style={{ whiteSpace: "nowrap" }}><button className="m-btn ghost sm" onClick={() => restartRuntimeAgent(a.agent_id).then(() => { toast("Restart journaled"); load(); }).catch(say)}>Restart</button><button className="m-btn ghost sm" onClick={() => cancelRuntimeAgent(a.agent_id).then(() => { toast("Cancelled"); load(); }).catch(say)}>Cancel</button></td></tr>)}
      {agents.length === 0 && <tr><td colSpan={6} style={{ color: "var(--ink-500)", textAlign: "center", padding: 24 }}>No live instance — the fleet runs durable agent teams; the builder's agents run as assistants.</td></tr>}
    </tbody></table></div>
  );
}

/** What healed itself: the typed repair audit stream. Its fields are typed records, read for the word that names them. */
const wordOf = (x: unknown): string => { if (x == null) return "—"; if (typeof x === "string") return x; if (typeof x === "object") { const o = x as Record<string, unknown>; for (const k of ["trigger", "action", "outcome", "kind", "type", "status"]) if (typeof o[k] === "string") return o[k] as string; } return JSON.stringify(x).slice(0, 60); };
const reasonOf = (x: unknown): string | null => (x && typeof x === "object" && typeof (x as { reason?: unknown }).reason === "string" ? (x as { reason: string }).reason : null);
function RepairsLane() {
  const [list, setList] = useState<RepairRecord[]>([]);
  useEffect(() => { listRepairs(100).then((l) => setList(Array.isArray(l) ? l : [])).catch(() => {}); }, []);
  return (
    <div className="lib-table-wrap"><table className="m-table"><thead><tr><th>Component</th><th>Trigger</th><th>Action</th><th>Outcome</th><th>When</th></tr></thead><tbody>
      {list.map((r) => { const out = wordOf(r.outcome); return <tr key={r.record_id}><td style={{ fontWeight: 600 }}>{wordOf(r.component)}</td><td style={{ color: "var(--ink-600)" }} title={reasonOf(r.trigger) ?? undefined}>{wordOf(r.trigger).replace(/_/g, " ")}{reasonOf(r.trigger) && <div style={{ fontSize: "var(--fs-xs)", color: "var(--ink-500)" }}>{reasonOf(r.trigger)!.slice(0, 110)}{reasonOf(r.trigger)!.length > 110 ? "…" : ""}</div>}</td><td>{wordOf(r.action).replace(/_/g, " ")}{r.attempt_count ? ` ×${r.attempt_count}` : ""}</td><td><Badge tone={/ok|success|repaired|recovered|achieved/i.test(out) ? "good" : /fail|gave up|dead|not/i.test(out) ? "bad" : "info"}>{out.replace(/_/g, " ")}</Badge></td><td style={{ fontFamily: "var(--font-mono)", fontSize: "var(--fs-xs)", color: "var(--ink-500)" }}>{ago(r.start_time)}</td></tr>; })}
      {list.length === 0 && <tr><td colSpan={5} style={{ color: "var(--ink-500)", textAlign: "center", padding: 24 }}>Nothing has needed repair.</td></tr>}
    </tbody></table></div>
  );
}

/** Was the verdict right? A person's word becomes a case in the verifier's own suite. */
function VerdictReviewRow({ run }: { run: Run }) {
  const { toast } = useOverlay();
  const [said, setSaid] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const then = run.verification?.verdict ?? "";
  const other = then === "verified" ? "failed" : "verified";
  async function say(agree: boolean) {
    setBusy(true);
    try {
      const r = await reviewVerdict(run.run_id, agree, agree ? undefined : (other as "verified" | "failed"));
      setSaid(agree ? `Thanks — the checker learns from it (${r.reviews} reviewed)` : `Noted — the checker learns from it (${r.reviews} reviewed)`);
    } catch (err) { toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle"); }
    finally { setBusy(false); }
  }
  if (then !== "verified" && then !== "failed") return null;
  return (
    <div className="verdict-review">
      {said ? <span className="m-hint"><i className="ti ti-check" /> {said}</span> : <>
        <span className="m-hint">Did the checker get this right?</span>
        <button className="m-btn ghost sm" data-verdict-right disabled={busy} onClick={() => void say(true)}><i className="ti ti-thumb-up" /> Yes</button>
        <button className="m-btn ghost sm" data-verdict-wrong disabled={busy} onClick={() => void say(false)} title={other === "verified" ? "It did get it done" : "It did not get it done"}><i className="ti ti-thumb-down" /> No — {other === "verified" ? "it did get it done" : "it did not get it done"}</button>
      </>}
    </div>
  );
}

/** The verifier's own evidence: how many verdicts people reviewed, and how often the judge agrees with them when judged again. */
export function VerifierCard() {
  const { toast } = useOverlay();
  const [ev, setEv] = useState<VerifierEvidence | null>(null);
  const load = () => { verifierEvidence().then(setEv).catch(() => {}); };
  useEffect(load, []);
  const running = ev?.latest?.status === "running";
  useEffect(() => { if (!running) return; const t = setInterval(load, 4000); return () => clearInterval(t); }, [running]);
  if (!ev) return null;
  const latest = ev.latest;
  const judgedN = latest ? (latest.judged ?? latest.cases.filter((c) => !c.skipped).length) : 0;
  const pct = latest && judgedN ? Math.round((latest.agreed / judgedN) * 100) : null;
  return (
    <div className="verifier-card">
      <div className="verifier-head"><i className="ti ti-gavel" /> <b>The checker</b>
        <span className="m-hint">{ev.reviews === 0 ? "It checks every finished run. Open a run and say whether it got it right." : `It checks every finished run; people reviewed ${ev.reviews} of its calls and disagreed with ${ev.disagreed}`}</span>
        <span className="sp" />
        <button className="m-btn ghost sm" data-judge-verifier disabled={ev.reviews === 0 || running} onClick={() => judgeVerifier().then(() => load()).catch((err) => toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle"))}>{running ? <><span className="m-spin" style={{ width: 12, height: 12, borderWidth: 2 }} /> Judging…</> : <><i className="ti ti-refresh" /> Check it again</>}</button>
      </div>
      {latest && <div className="verifier-body">
        <div className="m-hint">{latest.status === "running" ? `Judging ${latest.cases.length} of ${latest.total}…` : `Agrees with people on ${latest.agreed} of ${judgedN}${pct !== null ? ` (${pct}%)` : ""}${latest.cases.length > judgedN ? `, ${latest.cases.length - judgedN} it could not decide` : ""} · checked ${ago(latest.finished_at ?? latest.started_at)}`}{pct !== null && latest.status === "done" && pct < 90 && <span style={{ color: "var(--warn)" }}> · below the 90% needed to apply changes on its own</span>}</div>
        {latest.cases.filter((c) => !c.agrees).slice(0, 5).map((c) => <div key={c.run_id} className={`m-hint${c.skipped ? "" : " verifier-miss"}`}><i className={`ti ${c.skipped ? "ti-minus" : "ti-x"}`} /> {c.asked.slice(0, 80) || c.run_id.slice(0, 8)}: person said {c.person}, {c.skipped ? `not judged — ${c.skipped.slice(0, 120)}` : `the judge says ${c.now} — ${c.now_reason.slice(0, 140)}`}</div>)}
      </div>}
    </div>
  );
}
