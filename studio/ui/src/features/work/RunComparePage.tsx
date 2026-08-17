import { useEffect, useMemo, useState } from "react";
import { Link } from "@tanstack/react-router";
import type { RunEvent } from "../../lib/contracts";
import { type ComparisonRun, useWorkStore } from "../../state/work";
import { PageHeader } from "../../components/PageHeader";
import styles from "./RunComparePage.module.css";

function short(value: string) { return value.length > 18 ? `${value.slice(0, 10)}…${value.slice(-5)}` : value; }
function statusLabel(value: string) { return value[0].toUpperCase() + value.slice(1); }

function totals(item: ComparisonRun) {
  let latency = 0n, latencyObserved = 0, tokens = 0n, tokensObserved = 0, cost = 0, costObserved = 0;
  for (const event of item.evidence.events) {
    if (event.latency_ms !== null) { latency += BigInt(event.latency_ms); latencyObserved += 1; }
    const usage = event.tokens as { total_tokens?: unknown } | null;
    if (typeof usage?.total_tokens === "string" && /^(0|[1-9][0-9]*)$/.test(usage.total_tokens)) { tokens += BigInt(usage.total_tokens); tokensObserved += 1; }
    if (event.cost_usd !== null) { const value = Number(event.cost_usd); if (Number.isFinite(value)) { cost += value; costObserved += 1; } }
  }
  return { events: item.evidence.events.length, failures: item.evidence.events.filter((event) => event.status === "error").length, latency, latencyObserved, tokens, tokensObserved, cost, costObserved };
}

export function RunComparePage() {
  const runs = useWorkStore((state) => state.comparisons);
  const [baselineId, setBaselineId] = useState(runs.at(-2)?.run.run_id ?? runs[0]?.run.run_id ?? "");
  const [candidateId, setCandidateId] = useState(runs.at(-1)?.run.run_id ?? "");
  useEffect(() => {
    if (!runs.some((item) => item.run.run_id === baselineId)) setBaselineId(runs.at(-2)?.run.run_id ?? runs[0]?.run.run_id ?? "");
    if (!runs.some((item) => item.run.run_id === candidateId)) setCandidateId(runs.at(-1)?.run.run_id ?? "");
  }, [baselineId, candidateId, runs]);
  const baseline = runs.find((item) => item.run.run_id === baselineId) ?? null;
  const candidate = runs.find((item) => item.run.run_id === candidateId) ?? null;
  const baselineTotals = useMemo(() => baseline ? totals(baseline) : null, [baseline]);
  const candidateTotals = useMemo(() => candidate ? totals(candidate) : null, [candidate]);

  return <section className="page" aria-labelledby="compare-heading">
    <PageHeader headingId="compare-heading" eyebrow="Work / Compare" title="Compare runs" description="Compare outcomes, evidence coverage, and step trajectories before deciding which run is better." actions={<Link className="secondary-button" to="/work">Back to Work</Link>} />
    {runs.length < 2 ? <div className="empty-state"><span className="eyebrow">Run comparison</span><h2>Complete two runs to compare them</h2><p>Completed runs you open here are available to compare until this session ends.</p><Link className="primary-button" to="/work">Start work</Link></div>
      : baseline && candidate && baselineTotals && candidateTotals ? <div className={styles.workspace}>
        <header className={styles.selectors}><label>Baseline<select value={baselineId} onChange={(event) => setBaselineId(event.target.value)}>{runs.map((item) => <option key={item.run.run_id} value={item.run.run_id}>{item.agentName} · {short(item.run.run_id)}</option>)}</select></label><span aria-hidden="true">→</span><label>Candidate<select value={candidateId} onChange={(event) => setCandidateId(event.target.value)}>{runs.map((item) => <option key={item.run.run_id} value={item.run.run_id}>{item.agentName} · {short(item.run.run_id)}</option>)}</select></label></header>
        <section className={styles.summary} aria-label="Run metric comparison">
          <RunIdentity label="Baseline" item={baseline} />
          <div className={styles.metrics}>
            <ComparisonMetric label="Outcome" baseline={statusLabel(baseline.run.status)} candidate={statusLabel(candidate.run.status)} />
            <ComparisonMetric label="Steps" baseline={String(baselineTotals.events)} candidate={String(candidateTotals.events)} delta={candidateTotals.events - baselineTotals.events} directional={false} />
            <ComparisonMetric label="Failures" baseline={String(baselineTotals.failures)} candidate={String(candidateTotals.failures)} delta={candidateTotals.failures - baselineTotals.failures} lowerIsBetter />
            <ComparisonMetric label="Observed latency" baseline={`${baselineTotals.latency} ms · ${baselineTotals.latencyObserved}/${baselineTotals.events}`} candidate={`${candidateTotals.latency} ms · ${candidateTotals.latencyObserved}/${candidateTotals.events}`} />
            <ComparisonMetric label="Observed tokens" baseline={`${baselineTotals.tokens} · ${baselineTotals.tokensObserved}/${baselineTotals.events}`} candidate={`${candidateTotals.tokens} · ${candidateTotals.tokensObserved}/${candidateTotals.events}`} />
            <ComparisonMetric label="Observed cost" baseline={baselineTotals.costObserved ? `≈ $${baselineTotals.cost.toFixed(4)} · ${baselineTotals.costObserved}/${baselineTotals.events}` : "Unavailable"} candidate={candidateTotals.costObserved ? `≈ $${candidateTotals.cost.toFixed(4)} · ${candidateTotals.costObserved}/${candidateTotals.events}` : "Unavailable"} />
          </div>
          <RunIdentity label="Candidate" item={candidate} />
        </section>
        <Trajectory baseline={baseline.evidence.events} candidate={candidate.evidence.events} />
        <p className={styles.boundary}>Coverage is shown beside partial metrics. Studio does not rank incomplete latency, token, or cost evidence as an improvement.</p>
      </div> : null}
  </section>;
}

function RunIdentity({ label, item }: { label: string; item: ComparisonRun }) {
  return <article className={styles.identity}><span className="eyebrow">{label}</span><h2>{item.agentName}</h2><p>{item.objective || "Objective unavailable"}</p><code>{item.run.run_id}</code><Link to="/work/$threadId/runs/$runId/trace" params={{ threadId: item.run.thread_id, runId: item.run.run_id }}>Open trace</Link></article>;
}

function ComparisonMetric({ label, baseline, candidate, delta, lowerIsBetter = false, directional = true }: { label: string; baseline: string; candidate: string; delta?: number; lowerIsBetter?: boolean; directional?: boolean }) {
  const tone = !directional || delta === undefined || delta === 0 ? "" : (lowerIsBetter ? delta < 0 : delta > 0) ? styles.better : styles.worse;
  return <div className={styles.metric}><span>{label}</span><b>{baseline}</b><i className={tone}>{delta === undefined ? "→" : delta > 0 ? `+${delta}` : String(delta)}</i><b>{candidate}</b></div>;
}

function Trajectory({ baseline, candidate }: { baseline: RunEvent[]; candidate: RunEvent[] }) {
  const count = Math.min(60, Math.max(baseline.length, candidate.length));
  return <section className={styles.trajectory} aria-labelledby="trajectory-heading"><header><div><span className="eyebrow">Step trajectory</span><h2 id="trajectory-heading">Execution, aligned by sequence</h2></div><span>{Math.max(baseline.length, candidate.length) > 60 ? "First 60 steps shown" : `${count} aligned step${count === 1 ? "" : "s"}`}</span></header><div className={styles.table} role="table" aria-label="Aligned run steps"><div role="row" className={styles.tableHead}><span role="columnheader">Seq</span><span role="columnheader">Baseline</span><span role="columnheader">Candidate</span></div>{Array.from({ length: count }, (_, index) => <div role="row" key={index}><code role="cell">{index}</code><Step event={baseline[index]} /><Step event={candidate[index]} /></div>)}</div></section>;
}
function Step({ event }: { event?: RunEvent }) { return <span role="cell" className={!event ? styles.missing : event.status === "error" ? styles.failed : ""}>{event ? `${event.node_id ? `${event.node_id} · ` : ""}${event.kind.replaceAll("_", " ")}` : "No step"}</span>; }
