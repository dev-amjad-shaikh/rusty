import { useEffect, useMemo, useRef, useState } from "react";
import { useMutation, useQuery } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import {
  cancelExperiment,
  createDataset,
  createExperiment,
  createGate,
  getDatasetCases,
  getExperiment,
  listEvaluationCandidates,
  listDatasets,
  listExperiments,
  listGates,
  type EvalCase,
  type ExperimentRecord,
  type ExperimentSummary,
} from "../../../lib/api/evaluations";
import { connectionScope, jsonEquivalent, StudioApiError } from "../../../lib/api/client";
import { useConnectionStore } from "../../../state/connection";
import type { EvaluationCase } from "../../../state/work";
import styles from "./EvaluationLane.module.css";

const metric = "case_pass_rate";
interface PublishOperation { scope: string; name: string; version: string; cases: EvalCase[]; }
interface ExperimentOperation { scope: string; payload: Parameters<typeof createExperiment>[1]; }
interface GateOperation {
  scope: string;
  payload: Parameters<typeof createGate>[1];
  candidateReport: string;
  baselineReport: string | null;
}

export function EvaluationLane({ cases }: { cases: EvaluationCase[] }) {
  const { connection } = useConnectionStore();
  const scope = connection ? connectionScope(connection) : "disconnected";
  const [datasetName, setDatasetName] = useState("agent-regression");
  const [datasetVersion, setDatasetVersion] = useState(() => new Date().toISOString().slice(0, 10));
  const [selectedDataset, setSelectedDataset] = useState("");
  const [candidateId, setCandidateId] = useState("");
  const [runsPerCase, setRunsPerCase] = useState(3);
  const [maxConcurrency, setMaxConcurrency] = useState(2);
  const [selectedExperiment, setSelectedExperiment] = useState("");
  const [gateName, setGateName] = useState("");
  const [blockedTarget, setBlockedTarget] = useState("");
  const [acknowledged, setAcknowledged] = useState(false);
  const [acknowledgedExperiment, setAcknowledgedExperiment] = useState("");
  const [message, setMessage] = useState("");
  const [error, setError] = useState("");
  const experimentId = useRef(`exp-${crypto.randomUUID()}`);
  const priorScope = useRef(scope);

  useEffect(() => {
    if (priorScope.current === scope) return;
    priorScope.current = scope;
    setSelectedDataset(""); setCandidateId(""); setSelectedExperiment("");
    setGateName(""); setBlockedTarget(""); setAcknowledged(false); setAcknowledgedExperiment(""); setMessage(""); setError("");
    experimentId.current = `exp-${crypto.randomUUID()}`;
  }, [scope]);

  const datasets = useQuery({
    queryKey: [scope, "evaluation-datasets"],
    queryFn: () => listDatasets(connection!), enabled: Boolean(connection),
  });
  const experiments = useQuery({
    queryKey: [scope, "evaluation-experiments"],
    queryFn: () => listExperiments(connection!), enabled: Boolean(connection),
    refetchInterval: (query) => query.state.data?.experiments.some((item) => ["queued", "running"].includes(item.status.phase)) ? 1_000 : false,
  });
  const candidates = useQuery({
    queryKey: [scope, "evaluation-candidates"],
    queryFn: () => listEvaluationCandidates(connection!), enabled: Boolean(connection),
  });
  const gates = useQuery({
    queryKey: [scope, "evaluation-gates"],
    queryFn: () => listGates(connection!), enabled: Boolean(connection),
  });
  const experimentItems = experiments.data?.experiments;
  const activeSummary = experimentItems?.find((item) => item.experiment_id === selectedExperiment)
    ?? experimentItems?.find((item) => item.status.phase === "complete")
    ?? experimentItems?.[0]
    ?? null;
  const experimentDetail = useQuery({
    queryKey: [scope, "evaluation-experiment", activeSummary?.experiment_id],
    queryFn: () => getExperiment(connection!, activeSummary!.experiment_id),
    enabled: Boolean(connection && activeSummary),
    refetchInterval: activeSummary && ["queued", "running"].includes(activeSummary.status.phase) ? 1_000 : false,
  });
  const activeExperiment = experimentDetail.data ?? null;
  const datasetItems = datasets.data?.datasets ?? [];
  const datasetKey = selectedDataset || (datasetItems[0] ? datasetIdentity(datasetItems[0].name, datasetItems[0].version) : "");
  const [selectedName, selectedVersion] = splitDataset(datasetKey);
  const durableCases = useQuery({
    queryKey: [scope, "evaluation-cases", activeExperiment?.dataset_name, activeExperiment?.dataset_version],
    queryFn: () => getDatasetCases(connection!, activeExperiment!.dataset_name, activeExperiment!.dataset_version),
    enabled: Boolean(connection && activeExperiment?.dataset_name && activeExperiment?.dataset_version),
  });

  const publish = useMutation({
    mutationFn: async (operation: PublishOperation) => {
      if (!connection) throw new Error("Connect Rusty first.");
      return createDataset(connection, { name: operation.name, version: operation.version, cases: operation.cases });
    },
    onSuccess: async (record, operation) => {
      if (currentScope() !== operation.scope) return;
      await datasets.refetch();
      if (currentScope() !== operation.scope) return;
      const exact = await getDatasetCases(connection!, record.name, record.version);
      if (currentScope() !== operation.scope) return;
      if (!jsonEquivalent(exact.map(stripServerCapture), operation.cases.map(stripServerCapture))) {
        setError("Rusty returned a dataset version that did not match the reviewed cases.");
        setMessage("");
        return;
      }
      setSelectedDataset(datasetIdentity(record.name, record.version));
      setMessage(`Dataset ${record.name}@${record.version} is ready.`); setError("");
    },
    onError: async (caught, operation) => {
      if (currentScope() !== operation.scope) return;
      if (caught instanceof StudioApiError && caught.mayHaveCommitted) {
        const refreshed = await datasets.refetch();
        if (currentScope() !== operation.scope) return;
        const match = refreshed.data?.datasets.find((item) => item.name === operation.name && item.version === operation.version);
        if (match && match.case_count === operation.cases.length) {
          const exact = await getDatasetCases(connection!, match.name, match.version);
          if (currentScope() !== operation.scope) return;
          if (jsonEquivalent(exact.map(stripServerCapture), operation.cases.map(stripServerCapture))) { setSelectedDataset(datasetIdentity(match.name, match.version)); setMessage("Rusty already has this exact dataset version."); setError(""); return; }
        }
      }
      setError(caught instanceof Error ? caught.message : "The dataset could not be published.");
    },
  });

  const start = useMutation({
    mutationFn: async (operation: ExperimentOperation) => {
      if (!connection) throw new Error("Connect Rusty first.");
      return createExperiment(connection, operation.payload);
    },
    onSuccess: async (record, operation) => {
      if (currentScope() !== operation.scope) return;
      setSelectedExperiment(record.experiment_id); await experiments.refetch();
      if (currentScope() !== operation.scope) return;
      setMessage("Experiment started. Rusty will publish the paired result when every run settles."); setError("");
    },
    onError: async (caught, operation) => {
      if (currentScope() !== operation.scope) return;
      if (caught instanceof StudioApiError && caught.mayHaveCommitted) {
        const refreshed = await experiments.refetch();
        if (currentScope() !== operation.scope) return;
        const match = refreshed.data?.experiments.find((item) => item.experiment_id === operation.payload.experiment_id);
        if (match && experimentMatches(match, operation.payload)) { setSelectedExperiment(match.experiment_id); setMessage("Rusty confirmed the experiment after the response was lost."); setError(""); return; }
        setError("The start receipt was uncertain. Studio kept the same experiment identity; check its durable status before retrying."); return;
      }
      setError(caught instanceof Error ? caught.message : "The experiment could not start.");
    },
  });

  const stop = useMutation({
    mutationFn: (_initiatingScope: string) => cancelExperiment(connection!, activeExperiment!.experiment_id),
    onSuccess: async (_record, initiatingScope) => { if (currentScope() !== initiatingScope) return; await experiments.refetch(); if (currentScope() !== initiatingScope) return; setMessage("Cancellation requested."); setError(""); },
    onError: (caught, initiatingScope) => { if (currentScope() === initiatingScope) setError(caught instanceof Error ? caught.message : "Cancellation could not be confirmed."); },
  });

  const gate = useMutation({
    mutationFn: async (operation: GateOperation) => {
      if (!connection) throw new Error("Connect Rusty first.");
      return createGate(connection, operation.payload);
    },
    onSuccess: async (record, operation) => {
      if (currentScope() !== operation.scope) return;
      if (!gateMatches(record, operation)) {
        setError("Rusty returned a gate that did not match the reviewed experiment and policy.");
        setMessage("");
        return;
      }
      await gates.refetch();
      if (currentScope() !== operation.scope) return;
      setMessage(`Gate ${record.name} saved with a ${record.decision.outcome} decision.`); setError("");
    },
    onError: async (caught, operation) => {
      if (currentScope() !== operation.scope) return;
      if (caught instanceof StudioApiError && caught.mayHaveCommitted) {
        const refreshed = await gates.refetch();
        if (currentScope() !== operation.scope) return;
        const match = refreshed.data?.gates.find((item) => item.name === operation.payload.name);
        if (match && gateMatches(match, operation)) { setMessage(`Rusty confirmed gate ${match.name} after the response was lost.`); setError(""); return; }
      }
      setError(caught instanceof Error ? caught.message : "The release gate could not be saved.");
    },
  });

  const step = activeExperiment?.status.phase === "complete" ? 3 : activeSummary ? 2 : datasetKey ? 1 : 0;
  const sourceByCase = useMemo(() => new Map((durableCases.data ?? []).map((item) => [item.id, item.source])), [durableCases.data]);

  if (!connection) return null;
  return (
    <section className={styles.lab} aria-labelledby="evaluation-lab-heading">
      <header className={styles.labHeader}>
        <div><span className="eyebrow">Regression lab</span><h2 id="evaluation-lab-heading">Prove the next version is better</h2></div>
        <ol className={styles.steps} aria-label="Evaluation progress">
          {["Dataset", "Run", "Compare", "Gate"].map((label, index) => <li key={label} data-state={index < step ? "done" : index === step ? "current" : "next"}><span>{index < step ? "✓" : index + 1}</span>{label}</li>)}
        </ol>
      </header>

      {error && <p className={styles.error} role="alert">{error}</p>}
      {message && !error && <p className={styles.status} role="status">{message}</p>}
      {[datasets, experiments, candidates, gates].some((query) => query.isError) ? <div className={styles.queryError} role="alert"><span>Some evaluation catalogs are unavailable. Refresh them before starting new work.</span><button className="secondary-button" type="button" onClick={() => { datasets.refetch(); experiments.refetch(); candidates.refetch(); gates.refetch(); }}>Retry catalogs</button></div> : null}
      {durableCases.isError ? <div className={styles.queryError} role="alert"><span>Exact source cases are unavailable. Release review stays locked.</span><button className="secondary-button" type="button" onClick={() => durableCases.refetch()}>Retry source cases</button></div> : null}
      {datasets.data?.truncated || gates.data?.truncated ? <p className={styles.catalogBoundary}>Showing the most recent datasets and release gates; exact older records remain addressable.</p> : null}

      <section className={styles.actionRow} aria-labelledby="dataset-step-heading">
        <div><span className="eyebrow">1 · Dataset</span><h3 id="dataset-step-heading">Keep the cases that matter</h3><p>{cases.length} reviewed case{cases.length === 1 ? "" : "s"} in this session.</p></div>
        <div className={styles.inlineFields}>
          <label>Name<input value={datasetName} disabled={publish.isPending} onChange={(event) => setDatasetName(event.target.value)} /></label>
          <label>Version<input value={datasetVersion} disabled={publish.isPending} onChange={(event) => setDatasetVersion(event.target.value)} /></label>
          <button className="primary-button" type="button" disabled={!cases.length || publish.isPending} onClick={() => publish.mutate({ scope, name: datasetName.trim(), version: datasetVersion.trim(), cases: publishCases(cases) })}>{publish.isPending ? "Publishing…" : "Publish dataset"}</button>
        </div>
      </section>

      {datasetItems.length ? <section className={styles.actionRow} aria-labelledby="run-step-heading">
        <div><span className="eyebrow">2 · Run</span><h3 id="run-step-heading">Challenge one exact candidate</h3><p>Baseline and candidate use the same immutable cases.</p></div>
        <div className={styles.runFields}>
          <label>Dataset<select value={datasetKey} disabled={start.isPending} onChange={(event) => { setSelectedDataset(event.target.value); setSelectedExperiment(""); experimentId.current = `exp-${crypto.randomUUID()}`; }}>{datasetItems.map((item) => <option key={datasetIdentity(item.name, item.version)} value={datasetIdentity(item.name, item.version)}>{item.name}@{item.version} · {item.case_count} cases</option>)}</select></label>
          <label>Candidate<select value={candidateId} disabled={start.isPending || candidates.isLoading || candidates.isError} onChange={(event) => { setCandidateId(event.target.value); experimentId.current = `exp-${crypto.randomUUID()}`; }}><option value="">{candidates.isLoading ? "Loading candidates…" : candidates.isError ? "Candidate catalog unavailable" : candidates.data?.length ? "Choose a version" : "No candidates available"}</option>{candidates.data?.map((item) => <option key={item.candidate.candidate_id} value={item.candidate.candidate_id}>{candidateLabel(item.candidate.content)} · {shortId(item.candidate.candidate_id)}</option>)}</select></label>
          <label>Runs / case<input type="number" min="1" max="20" value={runsPerCase} disabled={start.isPending} onChange={(event) => { setRunsPerCase(Number(event.target.value)); experimentId.current = `exp-${crypto.randomUUID()}`; }} /></label>
          <label>Parallel runs<input type="number" min="1" max="16" value={maxConcurrency} disabled={start.isPending} onChange={(event) => { setMaxConcurrency(Number(event.target.value)); experimentId.current = `exp-${crypto.randomUUID()}`; }} /></label>
          <button className="primary-button" type="button" disabled={!candidateId || !selectedName || !selectedVersion || start.isPending || activeSummary?.status.phase === "running"} onClick={() => start.mutate({ scope, payload: experimentPayload(experimentId.current, selectedName, selectedVersion, candidateId.trim(), runsPerCase, maxConcurrency) })}>{start.isPending ? "Starting…" : "Run experiment"}</button>
        </div>
      </section> : null}

      {experimentItems?.length ? <><nav className={styles.experimentTabs} aria-label="Experiments">{experimentWindow(experimentItems, activeSummary).map((item) => <button type="button" key={item.experiment_id} disabled={start.isPending} aria-pressed={activeSummary?.experiment_id === item.experiment_id} onClick={() => { setSelectedExperiment(item.experiment_id); setAcknowledged(false); setAcknowledgedExperiment(""); }}><span>{item.dataset_name}@{item.dataset_version}</span><small>{shortId(item.candidate_id)} · {shortId(item.experiment_id)}</small><b>{statusLabel(item)}</b></button>)}</nav>{experiments.data?.truncated ? <p className={styles.catalogBoundary}>Showing the 200 most recent experiments.</p> : null}</> : null}

      {activeSummary && experimentDetail.isLoading ? <div className={styles.loading} role="status">Loading exact experiment evidence…</div> : null}
      {experimentDetail.isError ? <div className={styles.queryError} role="alert"><span>The selected experiment evidence is unavailable.</span><button className="secondary-button" type="button" onClick={() => experimentDetail.refetch()}>Retry experiment</button></div> : null}
      {activeExperiment && !experimentDetail.isError ? <ExperimentOutcome record={activeExperiment} sourceByCase={sourceByCase} onCancel={() => stop.mutate(scope)} cancelling={stop.isPending} /> : null}

      {activeExperiment?.comparison ? <section className={styles.gate} aria-labelledby="gate-step-heading">
        <header><div><span className="eyebrow">4 · Gate</span><h3 id="gate-step-heading">Protect a release target</h3></div><strong data-outcome={activeExperiment.comparison.regressed ? "block" : "allow"}>{activeExperiment.comparison.regressed ? "Regression found" : "Evidence clears"}</strong></header>
        <div className={styles.gateGrid}>
          <label>Gate name<input value={gateName} disabled={gate.isPending} onChange={(event) => { setGateName(event.target.value); setAcknowledged(false); setAcknowledgedExperiment(""); }} /></label>
          <label>Release target<input value={blockedTarget} disabled={gate.isPending} onChange={(event) => { setBlockedTarget(event.target.value); setAcknowledged(false); setAcknowledgedExperiment(""); }} placeholder="deployment:production" /></label>
        </div>
        <details><summary>Review the complete policy</summary><pre>{JSON.stringify(gatePolicy(gateName.trim(), activeExperiment), null, 2)}</pre></details>
        <label className={styles.ack}><input type="checkbox" checked={acknowledged && acknowledgedExperiment === activeExperiment.experiment_id} disabled={gate.isPending || durableCases.isLoading || durableCases.isError} onChange={(event) => { setAcknowledged(event.target.checked); setAcknowledgedExperiment(event.target.checked ? activeExperiment.experiment_id : ""); }} />I reviewed this policy and the complete experiment evidence it binds.</label>
        <button className="primary-button" type="button" disabled={!acknowledged || !gateName.trim() || !blockedTarget.trim() || acknowledgedExperiment !== activeExperiment.experiment_id || gate.isPending || durableCases.isLoading || durableCases.isError} onClick={() => gate.mutate({ scope, payload: { name: gateName.trim(), blocked_target: blockedTarget.trim(), experiment_id: activeExperiment.experiment_id, policy: gatePolicy(gateName.trim(), activeExperiment), acknowledged: true }, candidateReport: activeExperiment.candidate_report?.name ?? "", baselineReport: activeExperiment.baseline_report?.name ?? null })}>{gate.isPending ? "Saving…" : "Save release gate"}</button>
      </section> : null}
    </section>
  );
}

function ExperimentOutcome({ record, sourceByCase, onCancel, cancelling }: { record: ExperimentRecord; sourceByCase: Map<string, EvalCase["source"]>; onCancel: () => void; cancelling: boolean }) {
  const [casePage, setCasePage] = useState(0);
  const caseWindow = comparisonWindow(record.comparison?.case_deltas ?? [], casePage);
  useEffect(() => setCasePage(0), [record.experiment_id]);
  useEffect(() => {
    if (casePage >= caseWindow.pageCount) setCasePage(Math.max(0, caseWindow.pageCount - 1));
  }, [casePage, caseWindow.pageCount]);
  if (["queued", "running"].includes(record.status.phase)) {
    const complete = record.status.phase === "running" ? record.status.completed_runs : 0;
    const total = record.status.phase === "running" ? record.status.total_runs : record.config.runs_per_case;
    return <section className={styles.running} aria-live="polite"><div><span className="eyebrow">Experiment running</span><h3>{complete ? `${complete} of ${total} runs complete` : `Preparing ${total} paired runs`}</h3><progress value={complete} max={Math.max(total, 1)} /></div><button className="secondary-button" type="button" disabled={cancelling} onClick={onCancel}>{cancelling ? "Stopping…" : "Stop experiment"}</button></section>;
  }
  if (record.status.phase === "failed" || record.status.phase === "cancelled") return <section className={styles.settled}><span className="eyebrow">Experiment stopped</span><h3>{record.status.phase === "failed" ? record.status.reason : "Cancelled before completion"}</h3></section>;
  if (!record.baseline_report || !record.candidate_report || !record.comparison) return null;
  return <section className={styles.results} aria-labelledby="comparison-heading">
    <header><div><span className="eyebrow">3 · Compare</span><h3 id="comparison-heading">Paired outcomes</h3></div><div className={styles.score}><span>Baseline <b>{percent(record.baseline_report.summary.case_pass_rate)}</b></span><i aria-hidden="true">→</i><span>Candidate <b>{percent(record.candidate_report.summary.case_pass_rate)}</b></span></div></header>
    <div className={styles.resultMetrics}><Metric label="Pass-rate change" value={signed(record.candidate_report.summary.case_pass_rate - record.baseline_report.summary.case_pass_rate)} /><Metric label="p95 latency" value={`${record.baseline_report.summary.latency_ms.p95} → ${record.candidate_report.summary.latency_ms.p95} ms`} /><Metric label="Cost" value={`$${record.baseline_report.summary.total_cost_usd.toFixed(4)} → $${record.candidate_report.summary.total_cost_usd.toFixed(4)}`} /><Metric label="Verdict" value={record.comparison.regressed ? "Regression" : "Clear"} danger={record.comparison.regressed} /></div>
    <div className={styles.caseTable} role="table" aria-label="Paired result by case"><div className={styles.caseHead} role="row"><span role="columnheader">Case</span><span role="columnheader">Baseline</span><span role="columnheader">Candidate</span><span role="columnheader">Change</span><span role="columnheader">Evidence</span></div>{caseWindow.items.map((item) => { const source = sourceByCase.get(item.case_id); return <div className={styles.caseRow} role="row" key={item.case_id}><b role="cell">{item.case_id}</b><span role="cell">{rate(item.baseline_pass_rate)}</span><span role="cell">{rate(item.candidate_pass_rate)}</span><span role="cell" data-change={item.change}>{item.change}</span><span role="cell">{source ? <Link to="/work/$threadId/runs/$runId/trace" params={{ threadId: source.thread_id, runId: source.run_id }}>Open source run</Link> : "Unavailable"}</span></div>; })}</div>
    {caseWindow.pageCount > 1 ? <nav className={styles.casePager} aria-label="Comparison cases">
      <button className="secondary-button" type="button" disabled={casePage === 0} onClick={() => setCasePage((page) => page - 1)}>Previous cases</button>
      <span>{caseWindow.start + 1}–{caseWindow.end} of {record.comparison.case_deltas.length}</span>
      <button className="secondary-button" type="button" disabled={casePage + 1 >= caseWindow.pageCount} onClick={() => setCasePage((page) => page + 1)}>Next cases</button>
    </nav> : null}
  </section>;
}

function Metric({ label, value, danger = false }: { label: string; value: string; danger?: boolean }) { return <span data-danger={danger || undefined}><small>{label}</small><b>{value}</b></span>; }
export function datasetIdentity(name: string, version: string) { return JSON.stringify([name, version]); }
export function splitDataset(value: string) { try { const parsed = JSON.parse(value); return Array.isArray(parsed) && parsed.length === 2 && parsed.every((item) => typeof item === "string") ? parsed : ["", ""]; } catch { return ["", ""]; } }
function stripServerCapture(item: EvalCase) { return { ...item, source: { ...item.source, captured_at: "server" } }; }
function publishCases(cases: EvaluationCase[]): EvalCase[] { return cases.map((item) => ({ id: item.caseId, input: { objective: item.objective }, expect: { state: [{ pointer: item.pointer, expected: item.expected }] }, tags: ["studio", item.agentName], source: { run_id: item.runId, thread_id: item.threadId, agent_id: item.agentId, captured_at: item.createdAt } })); }
function experimentPayload(experimentId: string, datasetName: string, datasetVersion: string, candidateId: string, runsPerCase: number, maxConcurrency: number) { return { experiment_id: experimentId, candidate_id: candidateId, dataset_name: datasetName, dataset_version: datasetVersion, runs_per_case: runsPerCase, max_concurrency: maxConcurrency, target_metric: metric, thresholds: { max_pass_rate_drop: .05, max_latency_p95_ratio: 1.25 } }; }
function experimentMatches(item: ExperimentSummary, payload: Parameters<typeof createExperiment>[1]) { return item.experiment_id === payload.experiment_id && item.dataset_name === payload.dataset_name && item.dataset_version === payload.dataset_version && item.candidate_id === payload.candidate_id && jsonEquivalent(item.config, { runs_per_case: payload.runs_per_case, max_concurrency: payload.max_concurrency, target_metric: payload.target_metric, thresholds: payload.thresholds }); }
function gateMatches(item: Awaited<ReturnType<typeof createGate>>, operation: GateOperation) { return item.name === operation.payload.name && item.experiment_id === operation.payload.experiment_id && item.blocked_target === operation.payload.blocked_target && jsonEquivalent(item.policy, operation.payload.policy) && item.decision.policy === operation.payload.name && item.decision.candidate === operation.candidateReport && item.decision.baseline === operation.baselineReport; }
function gatePolicy(name: string, experiment: ExperimentRecord) { return { format_version: 1, name, minimum_runs: experiment.config.runs_per_case, minimum_run_pass_rate: .95, minimum_case_pass_rate: .95, minimum_assertion_pass_rates: {}, minimum_tag_pass_rates: {}, maximum_total_cost_usd: null, maximum_cost_ratio: null, maximum_regressions: 0, forbid_removed_cases: true, comparison_thresholds: experiment.config.thresholds }; }
function statusLabel(item: ExperimentSummary) { if (item.status.phase === "running") return `${item.status.completed_runs}/${item.status.total_runs}`; return item.status.phase; }
function experimentWindow(items: ExperimentSummary[], active: ExperimentSummary | null) {
  const leading = items.slice(0, 8);
  if (!active || leading.some((item) => item.experiment_id === active.experiment_id)) return leading;
  return [...leading.slice(0, 7), active];
}
export function comparisonWindow<T>(items: T[], page: number, pageSize = 50) {
  const pageCount = Math.max(1, Math.ceil(items.length / pageSize));
  const safePage = Math.min(Math.max(0, page), pageCount - 1);
  const start = safePage * pageSize;
  const end = Math.min(start + pageSize, items.length);
  return { items: items.slice(start, end), start, end, pageCount };
}
function percent(value: number) { return `${Math.round(value * 100)}%`; }
function rate(value: number | null) { return value === null ? "—" : percent(value); }
function signed(value: number) { const points = value * 100; return `${points > 0 ? "+" : ""}${points.toFixed(1)} pts`; }
function candidateLabel(content: Record<string, unknown>) { const subject = [content.name, content.tool, content.family].find((value) => typeof value === "string"); return `${String(content.kind).replaceAll("_", " ")}${subject ? ` · ${subject}` : ""}`; }
function shortId(value: string) { return `${value.slice(0, 8)}…${value.slice(-4)}`; }
function currentScope() { const current = useConnectionStore.getState().connection; return current ? connectionScope(current) : "disconnected"; }
