import { useEffect, useMemo, useState } from "react";
import { useNavigate } from "@tanstack/react-router";
import { conformanceChecks, datasetCases, datasetEvaluations, listConformanceRuns, listConformanceSuites, listDatasets, listExperiments, listGates, runDataset, sweepDatasets, type DatasetEvaluation, type DatasetVersion, type EvalCase, type ExperimentSummary, type GateRecord } from "../../engine/net/client";
import { useServer } from "../../engine/net/server";
import { Badge, OvHead, useOverlay } from "../overlay";
import { ago, isPlatform } from "../data";
import { plainName } from "../agents/words";

type Ev = DatasetEvaluation & { dataset: string };
const resultOf = (e: DatasetEvaluation): ["good" | "warn" | "bad" | "info", string] => (e.status === "running" ? ["info", "Running"] : e.status === "error" ? ["bad", "Error"] : e.passed === e.total ? ["good", "Pass"] : e.passed === 0 ? ["bad", "Fail"] : ["warn", "Partial"]);

/** Evals: datasets are the ground truth; an evaluation runs an agent version against one and judges every case. */
export function EvalsView() {
  const { open, toast } = useOverlay();
  const assistants = useServer((s) => s.assistants);
  const [datasets, setDatasets] = useState<(DatasetVersion & { versions: number })[]>([]);
  const [evals, setEvals] = useState<Ev[]>([]);
  const [busy, setBusy] = useState(false);
  const [engineering, setEngineering] = useState(false);
  const [experiments, setExperiments] = useState<ExperimentSummary[]>([]);
  const [gates, setGates] = useState<GateRecord[]>([]);
  const [conf, setConf] = useState<{ suites: number; runs: { run_id: string; suite_name: string; suite_version: string; target: string; status: string; created_at: string }[]; passing: boolean | null }>({ suites: 0, runs: [], passing: null });
  useEffect(() => { listExperiments().then((r) => setExperiments(r.experiments)).catch(() => {}); listGates().then((r) => setGates(r.gates)).catch(() => {}); Promise.all([listConformanceSuites().catch(() => ({ suites: [] })), listConformanceRuns().catch(() => ({ runs: [] })), conformanceChecks().catch(() => null)]).then(([s, r, c]) => setConf({ suites: s.suites.length, runs: r.runs, passing: c ? c.passing : null })); }, []);
  const reload = async () => {
    // One card per dataset: its newest version; the versions count rides on the sub line.
    const all = await listDatasets().catch(() => [] as DatasetVersion[]);
    const newest = new Map<string, DatasetVersion & { versions: number }>();
    for (const d of all) { const cur = newest.get(d.name); if (!cur) newest.set(d.name, { ...d, versions: 1 }); else { cur.versions++; if (d.created_at > cur.created_at) newest.set(d.name, { ...d, versions: cur.versions }); } }
    const ds = [...newest.values()].sort((a, b) => (a.created_at < b.created_at ? 1 : -1));
    setDatasets(ds);
    const evs = await Promise.all(ds.map((d) => datasetEvaluations(d.name, d.version).then((e) => e.map((x) => ({ ...x, dataset: d.name }))).catch(() => [] as Ev[])));
    setEvals(evs.flat().sort((a, b) => (a.started_at < b.started_at ? 1 : -1)));
  };
  useEffect(() => { void reload(); }, []);
  const agentName = (id?: string) => assistants.find((a) => a.assistant_id === id)?.name ?? (id ? id.slice(0, 8) : "—");
  const recent = evals.filter((e) => Date.now() - new Date(e.started_at).getTime() < 7 * 86_400_000);
  const passing = evals.filter((e) => e.status === "done" && e.passed === e.total).length, done = evals.filter((e) => e.status === "done").length;
  function run(d: DatasetVersion) { open("modal", <RunEvalModal d={d} onStarted={(e) => setEvals((l) => [{ ...e, dataset: d.name }, ...l])} />); }
  async function sweep() {
    setBusy(true);
    try { const started = await sweepDatasets(); toast(`${started.filter((s) => s.evaluation_id).length} suite${started.length === 1 ? "" : "s"} started`, "ti-player-play"); setTimeout(() => void reload(), 1500); }
    catch (err) { toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle"); }
    finally { setBusy(false); }
  }
  return (
    <div className="view library active" id="view-evals">
      <div className="lib-top"><div className="crumbs"><span>Rusty</span><i className="ti ti-chevron-right" style={{ color: "var(--ink-300)", fontSize: 15 }} /><b>Tests</b></div><div className="sp" /></div>
      <div className="lib-page">
        <div className="lib-hero">
          <div className="lh-ic"><i className="ti ti-test-pipe" /></div>
          <div className="lh-main"><div className="lib-eyebrow">Quality</div><h1 className="lib-title">Tests</h1><p className="lib-lead">Checks recorded from real runs. A new version of an agent must pass them before it goes live.</p></div>
          <div className="lh-act"><button className="m-btn secondary" data-new-ds onClick={() => open("modal", <NewDatasetModal />)}><i className="ti ti-plus" /> New test set</button> <button className="m-btn primary" data-new-exp disabled={busy || datasets.length === 0} onClick={() => void sweep()}><i className="ti ti-player-play" /> Run all tests</button></div>
        </div>
        <div className="lib-stats">
          <Stat v={String(datasets.length)} l="Test sets" />
          <Stat v={String(datasets.reduce((n, d) => n + d.case_count, 0))} l="Test cases" />
          <Stat v={String(passing)} u={` of ${done}`} l="Passed this week" />
        </div>
        <div className="lib-cat"><span>Test sets</span><span className="ln" /><span className="gc">{datasets.length}</span></div>
        <div className="lib-grid">
          {datasets.map((d) => (
            <div key={`${d.name}@${d.version}`} className="lcard" onClick={() => open("drawer", <DatasetDrawer d={d} agent={agentName(d.agent_id ?? undefined)} onRun={() => void run(d)} />)}>
              <div className="lcard-top"><div className="lcard-ic"><i className="ti ti-table" /></div><div style={{ flex: 1 }}><div className="lcard-title" title={d.name}>{plainName(d.name)}</div><div className="lcard-sub">{d.case_count} case{d.case_count === 1 ? "" : "s"} · recorded {ago(d.created_at)}</div></div></div>
              <div className="lcard-foot"><span className="mu"><i className="ti ti-robot" style={{ fontSize: 13 }} /> {agentName(d.agent_id ?? undefined)}</span><span className="sp" /><button className="m-btn secondary sm" data-run-ds disabled={busy} onClick={(e) => { e.stopPropagation(); void run(d); }}><i className="ti ti-player-play" /> Run tests</button></div>
            </div>
          ))}
          {datasets.length === 0 && <div className="thread-empty" style={{ gridColumn: "1 / -1", padding: 30 }}><i className="ti ti-table" />No tests yet. Turn a good run into one from the agent's Observe tab.</div>}
        </div>
        <div className="lib-cat"><span>Recent results</span><span className="ln" /><span className="gc">{evals.length}</span></div>
        <div className="lib-table-wrap"><table className="m-table">
          <thead><tr><th>Agent</th><th>Test set</th><th>Result</th><th>When</th></tr></thead>
          <tbody>
            {evals.slice(0, 40).map((e) => { const r = resultOf(e); return (
              <tr key={e.evaluation_id} className="clickable" onClick={() => open("drawer", <EvaluationDrawer e={e} agent={agentName(e.assistant_id)} />)}>
                <td style={{ fontWeight: 600 }}>{agentName(e.assistant_id)}</td>
                <td style={{ color: "var(--ink-600)" }}>{plainName(e.dataset)}</td>
                <td><Badge tone={r[0]}>{r[1]}</Badge> <span style={{ color: "var(--ink-500)", fontSize: 12 }}>passed {e.passed} of {e.total}</span></td>
                <td style={{ color: "var(--ink-500)" }}>{ago(e.started_at)}</td>
              </tr>
            ); })}
            {evals.length === 0 && <tr><td colSpan={4} style={{ color: "var(--ink-500)", textAlign: "center", padding: 24 }}>No results yet.</td></tr>}
          </tbody>
        </table></div>
        {(experiments.length > 0 || gates.length > 0 || conf.suites > 0 || conf.runs.length > 0) && <button className="m-btn ghost sm" style={{ marginTop: 18 }} data-flow="evals-engineering" onClick={() => setEngineering((v) => !v)}>{engineering ? "Hide engineering details" : "Engineering details: experiments, publish checks, conformance"}</button>}
        {engineering && (experiments.length > 0 || gates.length > 0) && (
          <>
            <div className="lib-cat"><span>Experiments</span><span className="ln" /><span className="gc">{experiments.length}</span></div>
            <div className="lib-table-wrap"><table className="m-table"><thead><tr><th>Experiment</th><th>Candidate</th><th>Dataset</th><th>Metric</th><th>Status</th><th>When</th></tr></thead><tbody>
              {experiments.slice(0, 20).map((x) => <tr key={x.experiment_id}><td><span className="mono" style={{ fontFamily: "var(--font-mono)", fontWeight: 600 }}>{x.experiment_id.slice(0, 12)}</span></td><td style={{ fontFamily: "var(--font-mono)", fontSize: "var(--fs-xs)" }}>{x.candidate_id.slice(0, 12)}</td><td style={{ color: "var(--ink-600)" }}>{x.dataset_name} · v{x.dataset_version}</td><td><span className="item-tag">{x.config.target_metric}</span></td><td><Badge tone={x.status.phase === "complete" ? "good" : x.status.phase === "failed" ? "bad" : x.status.phase === "cancelled" ? "warn" : "info"}>{x.status.phase}{x.status.phase === "running" ? ` ${x.status.completed_runs}/${x.status.total_runs}` : ""}</Badge></td><td style={{ fontFamily: "var(--font-mono)", fontSize: "var(--fs-xs)", color: "var(--ink-500)" }}>{ago(x.created_at)}</td></tr>)}
              {experiments.length === 0 && <tr><td colSpan={6} style={{ color: "var(--ink-500)", textAlign: "center", padding: 18 }}>No experiment yet — a learn candidate against its baseline over a dataset.</td></tr>}
            </tbody></table></div>
            <div className="lib-cat"><span>Gates</span><span className="ln" /><span className="gc">{gates.length}</span></div>
            <div className="lib-table-wrap"><table className="m-table"><thead><tr><th>Gate</th><th>Blocks</th><th>Experiment</th><th>Dataset</th><th>When</th></tr></thead><tbody>
              {gates.slice(0, 20).map((g) => <tr key={g.name}><td style={{ fontWeight: 600 }}>{g.name}</td><td><span className="item-tag">{g.blocked_target}</span></td><td style={{ fontFamily: "var(--font-mono)", fontSize: "var(--fs-xs)" }}>{g.experiment_id.slice(0, 12)}</td><td style={{ color: "var(--ink-600)" }}>{g.dataset_name} · v{g.dataset_version}</td><td style={{ fontFamily: "var(--font-mono)", fontSize: "var(--fs-xs)", color: "var(--ink-500)" }}>{ago(g.created_at)}</td></tr>)}
              {gates.length === 0 && <tr><td colSpan={5} style={{ color: "var(--ink-500)", textAlign: "center", padding: 18 }}>No gate frozen yet.</td></tr>}
            </tbody></table></div>
          </>
        )}
        {engineering && (conf.suites > 0 || conf.runs.length > 0) && (
          <>
            <div className="lib-cat"><span>Conformance</span><span className="ln" /><span className="gc">{conf.suites} suite{conf.suites === 1 ? "" : "s"}{conf.passing !== null ? ` · ${conf.passing ? "passing" : "not passing"}` : ""}</span></div>
            <div className="lib-table-wrap"><table className="m-table"><thead><tr><th>Run</th><th>Suite</th><th>Target</th><th>Status</th><th>When</th></tr></thead><tbody>
              {conf.runs.slice(0, 20).map((r) => <tr key={r.run_id}><td><span className="mono" style={{ fontFamily: "var(--font-mono)", fontWeight: 600 }}>{r.run_id.slice(0, 12)}</span></td><td>{r.suite_name} · v{r.suite_version}</td><td style={{ color: "var(--ink-600)" }}>{r.target}</td><td><Badge tone={r.status === "passed" ? "good" : r.status === "failed" ? "bad" : "info"}>{r.status}</Badge></td><td style={{ fontFamily: "var(--font-mono)", fontSize: "var(--fs-xs)", color: "var(--ink-500)" }}>{ago(r.created_at)}</td></tr>)}
            </tbody></table></div>
          </>
        )}
      </div>
    </div>
  );
}

/** Run a dataset against an agent — its own when it is still here, else one you pick; the server's refusal stays in view. */
function RunEvalModal({ d, onStarted }: { d: DatasetVersion; onStarted: (e: DatasetEvaluation) => void }) {
  const { close, toast } = useOverlay();
  const assistants = useServer((s) => s.assistants);
  const live = assistants.filter((a) => !a.archived_at && !isPlatform(a));
  const own = assistants.find((a) => a.assistant_id === d.agent_id);
  const [target, setTarget] = useState(own && !own.archived_at ? own.assistant_id : live[0]?.assistant_id ?? "");
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);
  return (
    <div className="m-modal">
      <OvHead icon="ti-player-play" title={`Run ${d.name}`} sub={`${d.case_count} case${d.case_count === 1 ? "" : "s"} · v${d.version}${own ? ` · cut from ${own.name}${own.archived_at ? " (archived)" : ""}` : ""}`} />
      <div className="ov-body">
        <div className="fld"><label className="fld-label">Against</label><select className="m-input" value={target} onChange={(e) => setTarget(e.target.value)}>{live.map((a) => <option key={a.assistant_id} value={a.assistant_id}>{a.name}</option>)}</select></div>
        {own?.archived_at && <div className="m-hint">The agent these cases came from is archived; another agent needs the same tools to pass them.</div>}
        {problem && <div className="m-alert"><i className="ti ti-alert-triangle" style={{ color: "var(--bad)", fontSize: 16 }} /><div className="a-body"><div className="a-text">{problem}</div></div></div>}
      </div>
      <div className="ov-foot"><div className="sp" /><button className="m-btn ghost" data-close>Cancel</button><button className="m-btn primary" disabled={busy || !target} onClick={async () => { setBusy(true); setProblem(null); try { const e = await runDataset(d.name, d.version, target); onStarted(e); toast(`Evaluation started · ${e.total} case${e.total === 1 ? "" : "s"}`, "ti-player-play"); close(); } catch (err) { setProblem(err instanceof Error ? err.message : "the server refused"); } finally { setBusy(false); } }}><i className="ti ti-player-play" /> Run</button></div>
    </div>
  );
}

function DatasetDrawer({ d, agent, onRun }: { d: DatasetVersion; agent: string; onRun: () => void }) {
  const { close } = useOverlay();
  const navigate = useNavigate();
  const [cases, setCases] = useState<EvalCase[]>([]);
  useEffect(() => { datasetCases(d.name, d.version).then(setCases).catch(() => {}); }, [d.name, d.version]);
  const asked = (c: EvalCase) => { const m = (c.input as { messages?: { content?: string }[] } | null)?.messages; return m?.[0]?.content ?? JSON.stringify(c.input).slice(0, 80); };
  return (
    <div className="m-drawer" style={{ width: "min(640px,100%)" }}>
      <OvHead icon="ti-table" title={d.name} sub={`${d.case_count} cases · v${d.version} · ${agent}`} />
      <div className="ov-body">
        <div className="kv"><span className="k">Agent</span><span className="v">{agent}</span><span className="k">Digest</span><span className="v mono">{d.digest}</span><span className="k">Source</span><span className="v">Recorded runs, each with what a good run must do</span><span className="k">Gate</span><span className="v">Runs against every new version before it is activated</span></div>
        <div className="cat-label"><span>Cases</span><span className="ln" /></div>
        <div className="lib-table-wrap"><table className="m-table"><thead><tr><th>Asked</th><th>Must call</th><th>Rubric</th></tr></thead><tbody>
          {cases.slice(0, 12).map((c) => <tr key={c.id}><td style={{ fontSize: "var(--fs-xs)" }}>{asked(c)}</td><td>{(c.expect?.tool_trajectory ?? []).map((t) => <span key={t.name} className="item-tag" style={{ marginRight: 4 }}>{t.name}</span>)}</td><td style={{ fontSize: "var(--fs-xs)", color: "var(--ink-600)" }}>{c.expect?.rubric ?? "—"}</td></tr>)}
        </tbody></table></div>
        {cases.length > 12 && <div style={{ fontSize: "var(--fs-xs)", color: "var(--ink-500)", marginTop: 8 }}>Showing 12 of {cases.length}</div>}
        <div className="cat-label" style={{ marginTop: 22 }}><span>Add cases</span><span className="ln" /></div>
        <div style={{ display: "flex", gap: 8, flexWrap: "wrap" }}><button className="m-btn secondary sm" onClick={() => { close(); navigate({ to: "/agents" }); }}><i className="ti ti-timeline" /> From a run, in the builder's Evaluation block</button></div>
      </div>
      <div className="ov-foot"><div className="sp" /><button className="m-btn secondary" data-close>Close</button><button className="m-btn primary" data-run onClick={() => { onRun(); close(); }}><i className="ti ti-player-play" /> Run evaluation</button></div>
    </div>
  );
}

function EvaluationDrawer({ e, agent }: { e: Ev; agent: string }) {
  const r = resultOf(e);
  const judged = e.cases.filter((c) => c.judge);
  return (
    <div className="m-drawer" style={{ width: "min(640px,100%)" }}>
      <OvHead icon="ti-flask" title={`${e.evaluation_id.slice(0, 8)} · ${agent}`} sub={`${e.dataset} · v${e.version} · ${ago(e.started_at)}${e.finished_at ? ` · ${Math.round((new Date(e.finished_at).getTime() - new Date(e.started_at).getTime()) / 1000)}s` : ""}`} />
      <div className="ov-body">
        <div className="lib-stats" style={{ gridTemplateColumns: `repeat(${judged.length ? 3 : 2}, 1fr)`, marginBottom: 18 }}>
          <Stat v={String(e.total ? Math.round((e.passed / e.total) * 100) : 0)} u="%" l="Passed" /><Stat v={`${e.passed}/${e.total}`} l="Cases" />{judged.length > 0 && <Stat v={String(Math.round((judged.filter((c) => c.judge!.passed).length / judged.length) * 100))} u="%" l="Judge agreed" />}
        </div>
        <Badge tone={r[0]} sm={false}>{r[1]}{e.error ? ` · ${e.error}` : e.status === "done" ? (e.passed === e.total ? " · every case held" : ` · ${e.total - e.passed} case${e.total - e.passed === 1 ? "" : "s"} did not`) : ""}</Badge>
        <div className="cat-label" style={{ marginTop: 22 }}><span>Per-case results</span><span className="ln" /></div>
        <div className="lib-table-wrap"><table className="m-table"><thead><tr><th>Case</th><th>Assertions</th><th>Judge</th><th className="num">Result</th></tr></thead><tbody>
          {e.cases.map((c) => <tr key={c.case_id} className="clickable"><td style={{ fontSize: "var(--fs-xs)" }}>{c.case_id}</td><td style={{ fontSize: "var(--fs-xs)" }}>{c.assertions.map((a, i) => <span key={i} style={{ color: a.passed ? "inherit" : "var(--bad)", marginRight: 6 }}>{a.assertion} {a.passed ? "✓" : "✗"}</span>)}</td><td style={{ fontSize: "var(--fs-xs)", color: c.judge && !c.judge.passed ? "var(--bad)" : "inherit" }}>{c.judge ? `${c.judge.score.toFixed(2)} · ${c.judge.rationale.slice(0, 80)}` : "—"}</td><td className="num"><Badge tone={c.passed ? "good" : "bad"}>{c.passed ? "pass" : "fail"}</Badge></td></tr>)}
        </tbody></table></div>
      </div>
      <div className="ov-foot"><div className="sp" /><button className="m-btn secondary" data-close>Close</button></div>
    </div>
  );
}

/** A dataset is cut from a run: the door is the agent's Evaluation block. */
function NewDatasetModal() {
  const { close } = useOverlay();
  const navigate = useNavigate();
  return (
    <div className="m-modal">
      <OvHead icon="ti-table" title="New dataset" sub="A dataset is cut from a run that went well." />
      <div className="ov-body"><div className="trig-opt" onClick={() => { close(); navigate({ to: "/agents" }); }}><div className="to-ic"><i className="ti ti-target-arrow" /></div><div><div className="to-name">From a good run</div><div className="to-desc">Open the agent, Evaluation → Set an evaluation goal, pick the run.</div></div><i className="ti ti-chevron-right to-go" /></div></div>
      <div className="ov-foot"><div className="sp" /><button className="m-btn secondary" data-close>Close</button></div>
    </div>
  );
}

function Stat({ v, u, l }: { v: string; u?: string; l: string }) {
  return <div className="lib-stat"><div className="ls-v">{v}{u && <span className="u">{u}</span>}</div><div className="ls-l">{l}</div></div>;
}
