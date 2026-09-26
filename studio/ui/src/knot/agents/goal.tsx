import { useEffect, useMemo, useState } from "react";
import { useNavigate } from "@tanstack/react-router";
import { goalMeasure, type Assistant, type GoalMeasure, type Run } from "../../engine/net/client";
import { useServer } from "../../engine/net/server";
import { OvHead, useOverlay } from "../overlay";
import type { AgentDraft } from "./useAgent";

/**
 * The goal: one sentence a teammate could verify, and the number that proves
 * it, measured on the agent's own runs over a rolling seven days. Kept on the
 * agent's metadata, so it versions with everything else.
 */
export interface Goal { objective: string; metric: string; target: number; lowerIsBetter?: boolean }

/** What the runs list can measure honestly, per run, as 0/1 or a number. */
const METRICS: { name: string; unit: string; lowerIsBetter?: boolean; counts: (r: Run) => boolean; value: (r: Run) => number }[] = [
  // A run that crashed did not achieve its outcome, so it belongs in the
  // denominator: counting only the runs that earned a verdict let four failed
  // runs sit beside "100% verified". A run still going, or paused and
  // resumable, has not finished and is not counted either way.
  { name: "Outcome verified", unit: "%", counts: (r) => r.status === "success" || r.status === "error" || r.status === "failed", value: (r) => (r.verification?.verdict === "verified" ? 1 : 0) },
  { name: "Resolved without a pause", unit: "%", counts: (r) => r.status !== "pending" && r.status !== "running", value: (r) => (r.status === "interrupted" || r.decision ? 0 : 1) },
  { name: "Runs that finished", unit: "%", counts: (r) => r.status !== "pending" && r.status !== "running", value: (r) => (r.status === "success" ? 1 : 0) },
];
export const goalOf = (draft: AgentDraft): Goal | null => ((draft.metadata.studio as { goal?: Goal } | undefined)?.goal ?? null);

/** A run that counts toward the goal: the agent's own live work. An
 * evaluation replaying a case, or a rehearsal in a world, exercises the
 * agent without serving anyone — a suite that passes is not a goal met, and
 * a candidate judged and dismissed is not a goal missed. */
export const liveRun = (r: Run) => (r.metadata as { channel?: string } | undefined)?.channel !== "evaluation" && !(r.worlds?.length);

/** Measure a goal on runs: the current value over seven days, and one value per day for the trend. */
export function measure(goal: Goal, runs: Run[], assistantId: string) {
  const m = METRICS.find((x) => x.name === goal.metric) ?? METRICS[0];
  const since = Date.now() - 7 * 86_400_000;
  const mine = runs.filter((r) => r.assistant_id === assistantId && liveRun(r) && new Date(r.created_at).getTime() >= since && m.counts(r));
  const pct = (rs: Run[]) => (rs.length ? Math.round((rs.reduce((n, r) => n + m.value(r), 0) / rs.length) * 100) : null);
  const trend: number[] = [];
  for (let i = 6; i >= 0; i--) { const day = new Date(Date.now() - i * 86_400_000).toISOString().slice(0, 10); trend.push(pct(mine.filter((r) => r.created_at.slice(0, 10) === day)) ?? 0); }
  return { current: pct(mine), sample: mine.length, trend, unit: m.unit, lowerIsBetter: !!m.lowerIsBetter };
}

export function GoalCard({ agent, draft, edit }: { agent: Assistant; draft: AgentDraft; edit: (change: (d: AgentDraft) => AgentDraft) => void }) {
  const { open } = useOverlay();
  const navigate = useNavigate();
  const runs = useServer((s) => s.runs);
  const goal = goalOf(draft);
  // The server measures on every run it holds; the list here is bounded, so
  // its measure is the fallback while the server's is on its way.
  const local = useMemo(() => (goal ? measure(goal, runs, agent.assistant_id) : null), [goal, runs, agent.assistant_id]);
  const [served, setServed] = useState<GoalMeasure | null>(null);
  const metric = goal?.metric ?? null;
  useEffect(() => {
    let live = true;
    setServed(null);
    if (metric) goalMeasure(agent.assistant_id, metric).then((m) => { if (live) setServed(m); }).catch(() => {});
    return () => { live = false; };
  }, [metric, agent.assistant_id, runs.length]);
  const m = local && served && served.metric === metric ? { ...local, current: served.current, sample: served.sample, trend: served.trend } : local;
  const editGoal = () => open("modal", <GoalModal goal={goal} onSave={(g) => edit((d) => ({ ...d, metadata: { ...d.metadata, studio: { ...(d.metadata.studio ?? {}), goal: g } } }))} />);
  if (!goal || !m) return (
    <div className="goal-card empty" id="goalCard">
      <div><div className="ge"><i className="ti ti-target-arrow" /> Goal</div><div className="go" style={{ color: "var(--ink-600)", fontWeight: 500 }}>No goal yet. A goal is stated to the agent at the start of every conversation, measured live on its runs, and shown against every publish.</div><div style={{ marginTop: 12 }}><button className="m-btn secondary sm" data-edit-goal onClick={editGoal}><i className="ti ti-plus" /> Set a goal</button></div></div>
    </div>
  );
  const has = m.current !== null;
  const cur = m.current ?? 0;
  const pct = has ? Math.min(100, Math.round((m.lowerIsBetter ? goal.target / Math.max(cur, 0.01) : cur / goal.target) * 100)) : 0;
  const on = has && (m.lowerIsBetter ? cur <= goal.target : cur >= goal.target);
  const max = Math.max(1, ...m.trend);
  return (
    <div className="goal-card" id="goalCard">
      <div>
        <div className="ge"><i className="ti ti-target-arrow" /> Goal <span className={`m-badge ${has ? (on ? "good" : "warn") : ""} sm`}>{has ? (on ? "On track" : "Below target") : "No data yet"}</span><button className="m-btn ghost sm" data-edit-goal onClick={editGoal}><i className="ti ti-pencil" /> Edit</button></div>
        <div className="go">{goal.objective}</div>
        <div className="gm"><span>{goal.metric}</span><div className={`m-progress accent${on ? "" : " warn"}`}><span style={{ width: `${pct}%` }} /></div><span>{has ? `${pct}% of target` : "Waiting for runs"}</span><a href="#" data-goal-evals style={{ color: "var(--accent)", fontWeight: 600, textDecoration: "none", marginLeft: "auto" }} onClick={(e) => { e.preventDefault(); navigate({ to: "/evals" }); }}>Experiments <i className="ti ti-arrow-right" style={{ fontSize: 11 }} /></a></div>
      </div>
      <div className="gv">
        <div className="big">{has ? cur : "—"}<small>{m.unit}</small></div>
        <div className="tg" title="Live runs only: evaluations and rehearsals in a world do not count">target {m.lowerIsBetter ? "≤" : "≥"} {goal.target}{m.unit} · {m.sample} live run{m.sample === 1 ? "" : "s"} · 7 days</div>
        {m.trend.some((v) => v > 0) && <div className="spark">{m.trend.map((v, i) => <div key={i} style={{ height: `${Math.round((v / max) * 100)}%` }} title={`${v}${m.unit}`} />)}</div>}
      </div>
    </div>
  );
}

function GoalModal({ goal, onSave }: { goal: Goal | null; onSave: (g: Goal) => void }) {
  const { close, toast } = useOverlay();
  const [objective, setObjective] = useState(goal?.objective ?? "");
  const [metric, setMetric] = useState(goal?.metric ?? METRICS[0].name);
  const [target, setTarget] = useState(goal?.target ?? 80);
  return (
    <div className="m-modal lg">
      <OvHead icon="ti-target-arrow" title={goal ? "Edit goal" : "Set a goal"} sub="One sentence a teammate could verify, plus the number that proves it." />
      <div className="ov-body">
        <div className="fld"><label className="fld-label">Objective</label><textarea className="m-textarea" data-o value={objective} onChange={(e) => setObjective(e.target.value)} placeholder="Answer every facilities report from the desk's own records, filing once at most." /></div>
        <div className="frow two">
          <div className="fld"><label className="fld-label">Primary metric</label><select className="m-input" data-m value={metric} onChange={(e) => setMetric(e.target.value)}>{METRICS.map((m) => <option key={m.name}>{m.name}</option>)}</select></div>
          <div className="fld"><label className="fld-label">Target</label><input className="m-input" type="number" data-t value={target} onChange={(e) => setTarget(Number(e.target.value))} /></div>
        </div>
        <div className="fld"><label className="fld-label">Measured on</label><div className="opt-list">
          {[["ti-activity", "Live runs", "Every run of this agent, rolling 7 days, from the server's verdicts", true], ["ti-test-pipe", "Suite on publish", "The agent's datasets gate every activation", true], ["ti-user-check", "Human review queue", "Not on this server yet", false]].map(([ic, n, d, on]) => (
            <div key={n as string} className="scope-row"><i className={`ti ${ic}`} style={{ color: "var(--ink-600)" }} /><div className="sb"><div style={{ fontSize: "var(--fs-sm)", fontWeight: 600 }}>{n}</div><div className="sd">{d}</div></div><div className={`m-switch${on ? " on" : ""}`} data-switch style={{ pointerEvents: "none", opacity: on ? 1 : 0.5 }} /></div>
          ))}
        </div></div>
      </div>
      <div className="ov-foot"><div className="sp" /><button className="m-btn ghost" data-close>Cancel</button><button className="m-btn primary" data-save onClick={() => { onSave({ objective: objective.trim() || "Untitled goal", metric, target, lowerIsBetter: METRICS.find((m) => m.name === metric)?.lowerIsBetter }); close(); toast("Goal saved", "ti-target-arrow"); }}><i className="ti ti-check" /> Save goal</button></div>
    </div>
  );
}
