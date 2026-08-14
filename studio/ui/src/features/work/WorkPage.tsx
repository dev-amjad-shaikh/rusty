import { type FormEvent, useEffect, useMemo, useRef, useState } from "react";
import { useMutation, useQueries, useQuery } from "@tanstack/react-query";
import { parse as parseLossless, stringify as stringifyLossless } from "lossless-json";
import { Link, useLocation, useNavigate, useParams } from "@tanstack/react-router";
import { connectionScope, createThread, getRun, getRunEvidence, listAssistants, mutationScope, startRun, StudioApiError } from "../../lib/api/client";
import type { Assistant, RunEvent, RunEvidence, RunSnapshot } from "../../lib/contracts";
import { useConnectionStore } from "../../state/connection";
import { type EvaluationCase, evaluationDatasetJsonl, useWorkStore } from "../../state/work";
import { durableConnectionScope, readRecentWork, rememberRecentWork, type RecentWorkIdentity } from "../../state/recentWork";
import { bytePreview } from "../../lib/text";
import { ArtifactTray } from "./artifacts/ArtifactTray";
import { EvaluationLane } from "./evaluations/EvaluationLane";
import styles from "./WorkPage.module.css";

const stages = ["run", "trace", "evaluate"] as const;
type Stage = (typeof stages)[number];

export function WorkPage() {
  const navigate = useNavigate();
  const params = useParams({ strict: false }) as { threadId?: string; runId?: string };
  const pathname = useLocation({ select: (location) => location.pathname });
  const stage: Stage = pathname.endsWith("/trace") ? "trace" : pathname.endsWith("/evaluate") ? "evaluate" : "run";
  const { connection, openDialog } = useConnectionStore();
  const work = useWorkStore();
  const scope = connection ? connectionScope(connection) : "disconnected";
  const durableScope = connection ? durableConnectionScope(connection) : "disconnected";
  const durableMutationScope = connection ? mutationScope(connection) : "disconnected";
  const ownedWork = work.connectionKey === scope;
  const ownsRoute = ownedWork && (!params.runId || (work.receipt?.run_id === params.runId && work.thread?.thread_id === params.threadId));
  const [selectedAgentId, setSelectedAgentId] = useState(work.assistant?.assistant_id ?? "");
  const [objective, setObjective] = useState(work.objective);
  const [error, setError] = useState("");
  const [recentIds, setRecentIds] = useState<RecentWorkIdentity[]>(() => connection ? readRecentWork(durableScope) : []);
  const uncertainty = work.uncertainByConnection[durableMutationScope] ?? "";
  const draftScope = useRef(scope);
  const drafts = useRef(new Map<string, { selectedAgentId: string; objective: string }>());
  const launchOwner = useRef({ mounted: true, operationId: "" });

  useEffect(() => {
    launchOwner.current.mounted = true;
    return () => { launchOwner.current.mounted = false; };
  }, []);

  useEffect(() => {
    if (draftScope.current === scope) return;
    drafts.current.set(draftScope.current, { selectedAgentId, objective });
    const next = drafts.current.get(scope);
    setSelectedAgentId(next?.selectedAgentId ?? (ownedWork ? work.assistant?.assistant_id ?? "" : ""));
    setObjective(next?.objective ?? (ownedWork ? work.objective : ""));
    setError("");
    draftScope.current = scope;
  }, [objective, ownedWork, scope, selectedAgentId, work.assistant?.assistant_id, work.objective]);

  useEffect(() => { setRecentIds(connection ? readRecentWork(durableScope) : []); }, [connection, durableScope]);

  const assistants = useQuery({
    queryKey: connection ? [connection.epoch, connection.origin, connection.tenantFingerprint, "assistants"] : ["assistants", "disconnected"],
    queryFn: () => listAssistants(connection!),
    enabled: Boolean(connection),
  });
  const run = useQuery({
    queryKey: connection && params.runId ? [connection.epoch, connection.origin, connection.tenantFingerprint, "run", params.runId] : ["run", "idle"],
    queryFn: () => getRun(connection!, params.runId!),
    enabled: Boolean(connection && params.runId),
    refetchInterval: (query) => query.state.data && ["success", "error", "interrupted", "cancelled"].includes(query.state.data.status) ? false : 1_000,
  });
  const evidence = useQuery({
    queryKey: connection && params.runId ? [connection.epoch, connection.origin, connection.tenantFingerprint, "run-evidence", params.runId] : ["run-evidence", "idle"],
    queryFn: () => getRunEvidence(connection!, params.runId!),
    enabled: Boolean(connection && params.runId),
    refetchInterval: (query) => query.state.data?.complete ? false : 1_250,
    retry: (count, caught) => caught instanceof StudioApiError && caught.status === 404 ? count < 3 : count < 1,
  });
  const recentRuns = useQueries({
    queries: connection && !params.runId ? recentIds.slice(0, 6).map((item) => ({
      queryKey: [connection.epoch, connection.origin, connection.tenantFingerprint, "recent-run", item.runId],
      queryFn: () => getRun(connection, item.runId),
      retry: false,
      staleTime: 15_000,
    })) : [],
  });
  const launch = useMutation({
    mutationFn: async (input: { connection: NonNullable<typeof connection>; scope: string; mutationScope: string; durableScope: string; operationId: string; agent: Assistant; objective: string }) => {
      let threadCreated = false;
      try {
        const thread = await createThread(input.connection, input.agent.graph, input.agent.assistant_id);
        threadCreated = true;
        const receipt = await startRun(input.connection, thread, input.agent.assistant_id, input.agent.active_version_id, input.objective);
        return { ...input, thread, receipt };
      } catch (caught) {
        if (caught instanceof StudioApiError && caught.status === 409 && !caught.mayHaveCommitted) {
          throw new StudioApiError("This agent changed after you opened it. Review the current active version, then choose the agent again.", 409);
        }
        if (threadCreated || (caught instanceof StudioApiError && caught.mayHaveCommitted)) {
          work.markUncertain(input.mutationScope, "Rusty may have accepted part or all of this launch. Check server work before allowing another run.");
          throw new StudioApiError("The launch result is uncertain. Studio locked retry to avoid duplicate work.", caught instanceof StudioApiError ? caught.status : 0, true);
        }
        throw caught;
      }
    },
    onSuccess: ({ agent, thread, receipt, objective: exactObjective, scope: initiatingScope, durableScope: initiatingDurableScope, operationId }) => {
      const current = useConnectionStore.getState().connection;
      if (!current || connectionScope(current) !== initiatingScope) return;
      const recent = rememberRecentWork(initiatingDurableScope, { threadId: thread.thread_id, runId: receipt.run_id });
      if (!launchOwner.current.mounted || launchOwner.current.operationId !== operationId) return;
      work.clearUncertain(mutationScope(current));
      work.begin(initiatingScope, agent, exactObjective, thread, receipt);
      setRecentIds(recent);
      setError("");
      navigate({ to: "/work/$threadId/runs/$runId", params: { threadId: thread.thread_id, runId: receipt.run_id } });
    },
    onError: (caught, input) => {
      const current = useConnectionStore.getState().connection;
      if (!current || connectionScope(current) !== scope) return;
      if (caught instanceof StudioApiError && caught.status === 409 && !caught.mayHaveCommitted) {
        work.expirePrepared(input.scope, input.agent.assistant_id, input.agent.active_version_id);
      }
      if (!launchOwner.current.mounted || launchOwner.current.operationId !== input.operationId) return;
      if (caught instanceof StudioApiError && caught.status === 409 && !caught.mayHaveCommitted) {
        setSelectedAgentId("");
        void assistants.refetch();
      }
      setError(caught instanceof Error ? caught.message : "The run could not be started.");
    },
  });

  function submitLaunch() {
    if (!connection) { openDialog(); return; }
    if (uncertainty) return;
    const prepared = ownsRoute && work.assistant?.assistant_id === selectedAgentId ? work.assistant : null;
    const agent = prepared ?? assistants.data?.find((item) => item.assistant_id === selectedAgentId && !item.archived_at);
    if (!agent) { setError("Choose an available agent."); return; }
    const exactObjective = objective.trim();
    if (!exactObjective) { setError("Describe the outcome you need."); return; }
    setError("");
    const operationId = crypto.randomUUID();
    launchOwner.current.operationId = operationId;
    launch.mutate({ connection, scope, mutationScope: durableMutationScope, durableScope, operationId, agent, objective: exactObjective });
  }

  const exactRun = run.data && run.data.run_id === params.runId && run.data.thread_id === params.threadId ? run.data : null;
  const exactEvidence = evidence.data && evidence.data.run_id === params.runId && (!evidence.data.events.length || evidence.data.events.every((event) => event.thread_id === params.threadId)) ? evidence.data : null;
  const routedRun = Boolean(params.runId);
  const envelopeMismatch = Boolean((run.data && !exactRun) || (evidence.data && !exactEvidence));
  const traceReady = Boolean(exactRun && exactEvidence?.events.length);
  const evaluationReady = Boolean(exactEvidence?.complete && exactRun && ["success", "error", "interrupted", "cancelled"].includes(exactRun.status));
  const activeAgent = (ownsRoute ? work.assistant : null)
    ?? (exactRun?.assistant_id ? assistants.data?.find((item) => item.assistant_id === exactRun.assistant_id) : null)
    ?? (!params.runId ? assistants.data?.find((item) => item.assistant_id === selectedAgentId) : null)
    ?? null;
  const routeObjective = ownsRoute ? work.objective : runStudioObjective(exactRun);
  useEffect(() => {
    if (!exactRun || exactRun.run_id !== params.runId || exactRun.thread_id !== params.threadId) return;
    if (recentIds.some((item) => item.runId === exactRun.run_id && item.threadId === exactRun.thread_id)) return;
    setRecentIds(rememberRecentWork(durableScope, { threadId: exactRun.thread_id, runId: exactRun.run_id }));
  }, [durableScope, exactRun, params.runId, params.threadId, recentIds]);
  const currentComparisons = work.comparisons.filter((item) => item.connectionKey === scope);
  useEffect(() => {
    if (!exactRun || !exactEvidence?.complete || currentComparisons.some((item) => item.run.run_id === exactRun.run_id) || !["success", "error", "interrupted", "cancelled"].includes(exactRun.status)) return;
    work.rememberRun({ connectionKey: scope, run: exactRun, evidence: exactEvidence, agentName: activeAgent?.name ?? exactRun.graph, objective: routeObjective });
  }, [activeAgent, currentComparisons, exactEvidence, exactRun, routeObjective, scope, work]);

  function openStage(next: Stage) {
    if (!params.runId || !params.threadId) return;
    const to = next === "run" ? "/work/$threadId/runs/$runId" : next === "trace" ? "/work/$threadId/runs/$runId/trace" : "/work/$threadId/runs/$runId/evaluate";
    navigate({ to, params: { threadId: params.threadId, runId: params.runId } });
  }

  return (
    <section className={`page ${styles.workPage}`} aria-labelledby="work-heading">
      <header className={`page-header ${styles.workHeader}`}><div><span className="eyebrow">Work · durable execution</span><h1 id="work-heading">Run the work.<br /><span>Keep the evidence.</span></h1><p>Give an agent an outcome, follow its execution, and turn the exact result into an evaluation without leaving the thread.</p></div>{currentComparisons.length >= 2 && <Link className="secondary-button" to="/work/compare">Compare runs</Link>}</header>
      {!connection ? <div className="empty-state"><span className="eyebrow">One continuous workspace</span><h2>Open a workspace to start work</h2><p>Your agents, runs, and evidence live together in a Rusty workspace.</p><button className="primary-button" type="button" onClick={openDialog}>Choose workspace</button></div> : (
        <div className={styles.workspace}>
          <header className={styles.contextBar}><div><span>Agent</span><b>{activeAgent?.name ?? exactRun?.graph ?? "New work"}</b></div><div><span>Status</span><b className={styles.status}>{exactRun?.status ?? (launch.isPending ? "starting" : "not started")}</b></div><div className={styles.identity}><span>Thread / run</span><code>{params.threadId ? `${short(params.threadId)} / ${short(params.runId ?? "")}` : "Created when you start"}</code></div></header>
          <nav className={styles.stages} aria-label="Work stages">{stages.map((item, index) => <button type="button" key={item} aria-current={stage === item ? "step" : undefined} onClick={() => openStage(item)} disabled={(item === "trace" && !traceReady) || (item === "evaluate" && !evaluationReady) || (!params.runId && item !== "run")}><span>{index + 1}</span>{item[0].toUpperCase() + item.slice(1)}</button>)}</nav>
          {stage === "run" && !routedRun && uncertainty ? <div className="empty-state" role="alert"><span className="eyebrow">Launch needs review</span><h2>Check Rusty before starting again</h2><p>{uncertainty}</p><button className="secondary-button" type="button" onClick={() => { work.clearUncertain(durableMutationScope); setError(""); }}>I checked the server — allow another run</button></div> : null}
          {stage === "run" && ((!routedRun && !uncertainty) || (routedRun && exactRun && exactEvidence && !run.isError && !evidence.isError && !envelopeMismatch)) && <RunWorkspace assistants={assistants.data ?? []} selectedAgentId={selectedAgentId} setSelectedAgentId={setSelectedAgentId} objective={routedRun ? routeObjective : objective} setObjective={setObjective} submit={submitLaunch} pending={launch.isPending} error={error} run={exactRun} evidence={exactEvidence} recent={recentIds.slice(0, 6).map((identity, index) => ({ identity, run: recentRuns[index]?.data ?? null, loading: recentRuns[index]?.isLoading ?? false, unavailable: recentRuns[index]?.isError ?? false }))} openTrace={() => openStage("trace")} />}
          {routedRun && (run.isError || evidence.isError || envelopeMismatch) && <div className="empty-state" role="alert"><span className="eyebrow">Run evidence unavailable</span><h2>This workspace could not prove the requested run</h2><p>{envelopeMismatch ? "The run status and journal did not agree with this exact thread and run." : run.error instanceof Error ? run.error.message : evidence.error instanceof Error ? evidence.error.message : "Reload the exact run evidence."}</p><button className="secondary-button" type="button" onClick={() => { run.refetch(); evidence.refetch(); }}>Retry evidence</button></div>}
          {routedRun && !run.isError && !evidence.isError && !envelopeMismatch && (!exactRun || !exactEvidence) && <div className={styles.loading} aria-live="polite">Loading exact run evidence…</div>}
          {stage === "trace" && exactRun && exactEvidence && !run.isError && !evidence.isError && !envelopeMismatch && <TraceWorkspace evidence={exactEvidence} run={exactRun} openEvaluate={() => openStage("evaluate")} />}
          {stage === "evaluate" && exactEvidence && exactRun && params.threadId && !run.isError && !evidence.isError && !envelopeMismatch && <EvaluateWorkspace key={`${params.threadId}\0${exactRun.run_id}`} connectionKey={scope} evidence={exactEvidence} run={exactRun} threadId={params.threadId} agent={activeAgent} objective={routeObjective} />}
          {routedRun && stage !== "evaluate" && exactRun && exactEvidence && !run.isError && !evidence.isError && !envelopeMismatch && params.runId && <ArtifactTray runId={params.runId} />}
        </div>
      )}
    </section>
  );
}

interface RecentRunView { identity: RecentWorkIdentity; run: RunSnapshot | null; loading: boolean; unavailable: boolean; }
function RunWorkspace({ assistants, selectedAgentId, setSelectedAgentId, objective, setObjective, submit, pending, error, run, evidence, recent, openTrace }: { assistants: Assistant[]; selectedAgentId: string; setSelectedAgentId: (id: string) => void; objective: string; setObjective: (value: string) => void; submit: () => void; pending: boolean; error: string; run: RunSnapshot | null; evidence: RunEvidence | null; recent: RecentRunView[]; openTrace: () => void }) {
  const active = run && ["pending", "running"].includes(run.status);
  return <div className={styles.runGrid}><section className={styles.composer}><span className="eyebrow">Objective</span><h2>{run ? statusCopy(run.status) : "What outcome do you need?"}</h2>{!run ? <><label>Agent<select value={selectedAgentId} onChange={(event) => setSelectedAgentId(event.target.value)}><option value="">Select an agent</option>{assistants.filter((agent) => !agent.archived_at).map((agent) => <option key={agent.assistant_id} value={agent.assistant_id}>{agent.name}</option>)}</select></label><label>Goal<textarea rows={7} value={objective} onChange={(event) => setObjective(event.target.value)} placeholder="Investigate the customer issue, verify the relevant evidence, and recommend the next action." /></label>{error && <p className={styles.error} role="alert">{error}</p>}<div className={styles.runActions}><span>The thread is created only when you start.</span><button type="button" className="primary-button" onClick={submit} disabled={pending}>{pending ? "Starting…" : "Start run"}</button></div></> : <div className={styles.runSummary}><p>{objective || "This run was opened from its durable identity."}</p><dl><div><dt>Status</dt><dd>{run.status}</dd></div><div><dt>Attempt</dt><dd>{run.attempt}</dd></div><div><dt>Recorded events</dt><dd>{evidence?.events.length ?? 0}</dd></div></dl>{evidence?.events.length ? <button className="primary-button" type="button" onClick={openTrace}>{active ? "Follow trace" : "Inspect trace"}</button> : <p className={styles.waiting}>Waiting for the first recorded step…</p>}</div>}</section><aside className={styles.activity}>{!run && recent.length ? <RecentWork items={recent} /> : <><span className="eyebrow">Live activity</span><h2>{run ? statusCopy(run.status) : "The run will unfold here"}</h2><div className={styles.tracePreview}>{previewEvents(evidence).map((item, index) => <div key={`${item.label}-${index}`}><span className={item.done ? styles.done : ""}>{item.done ? "✓" : index + 1}</span><p><b>{item.label}</b><small>{item.detail}</small></p></div>)}</div></>}</aside></div>;
}

function RecentWork({ items }: { items: RecentRunView[] }) {
  return <section className={styles.recent} aria-labelledby="recent-work-heading"><span className="eyebrow">Recent work</span><h2 id="recent-work-heading">Continue where you left off</h2><ol>{items.map(({ identity, run, loading, unavailable }) => <li key={identity.runId}><Link to="/work/$threadId/runs/$runId" params={{ threadId: identity.threadId, runId: identity.runId }}><span><b>{run?.graph ?? short(identity.runId)}</b><small>{loading ? "Checking current status…" : unavailable ? "Status unavailable · open to retry" : `${statusCopy(run?.status ?? "")} · ${short(identity.threadId)}`}</small></span><i aria-hidden="true">→</i></Link></li>)}</ol><p>Open a run to reload its current status.</p></section>;
}

function TraceWorkspace({ evidence, run, openEvaluate }: { evidence: RunEvidence; run: RunSnapshot | null; openEvaluate: () => void }) {
  const [selectedId, setSelectedId] = useState(evidence.events.at(-1)?.id ?? "");
  const previousLatest = useRef(evidence.events.at(-1)?.id ?? "");
  const [filter, setFilter] = useState("all");
  const filtered = useMemo(() => evidence.events.filter((event) => filter === "all" || eventCategory(event) === filter), [evidence.events, filter]);
  const [page, setPage] = useState(() => Math.max(0, Math.ceil(filtered.length / 120) - 1));
  const window = traceWindow(filtered, page);
  const selected = window.items.find((event) => event.id === selectedId) ?? window.items.at(-1) ?? null;
  const totals = useMemo(() => ({ latency: evidence.events.reduce((sum, event) => sum + BigInt(event.latency_ms ?? "0"), 0n), failed: evidence.events.filter((event) => event.status === "error").length }), [evidence.events]);
  useEffect(() => { if (page !== window.page) setPage(window.page); }, [page, window.page]);
  useEffect(() => { if (selected && selected.id !== selectedId) setSelectedId(selected.id); }, [selected, selectedId]);
  useEffect(() => {
    const latest = evidence.events.at(-1)?.id ?? "";
    if (latest && latest !== previousLatest.current && selectedId === previousLatest.current) {
      setSelectedId(latest); setPage(Math.max(0, Math.ceil(filtered.length / 120) - 1));
    }
    previousLatest.current = latest;
  }, [evidence.events, filtered.length, selectedId]);
  function chooseFilter(next: string) { setFilter(next); setPage(0); setSelectedId(""); }
  return <section className={styles.traceWorkspace} aria-labelledby="trace-heading"><header className={styles.traceHeader}><div><span className="eyebrow">Visual trace</span><h2 id="trace-heading">{run ? statusCopy(run.status) : "Recorded execution"}</h2></div><div className={styles.metrics}><Metric label="Steps" value={String(new Set(evidence.events.map((event) => event.seq)).size)} /><Metric label="Observed latency" value={`${totals.latency} ms`} /><Metric label="Failures" value={String(totals.failed)} tone={totals.failed ? "danger" : "default"} /><Metric label="Evidence" value={evidence.complete ? "Final" : "Live"} /></div></header><div className={styles.traceTools}><div className={styles.traceFilters} role="toolbar" aria-label="Trace filters">{["all", "model", "tool", "memory", "execution", "error"].map((item) => <button key={item} type="button" aria-pressed={filter === item} onClick={() => chooseFilter(item)}>{item}</button>)}</div>{window.pages > 1 && <nav className={styles.tracePager} aria-label="Trace pages"><button type="button" onClick={() => setPage((value) => Math.max(0, value - 1))} disabled={window.page === 0}>Earlier</button><span>{window.start + 1}–{window.end} of {filtered.length}</span><button type="button" onClick={() => setPage((value) => Math.min(window.pages - 1, value + 1))} disabled={window.page === window.pages - 1}>Later</button></nav>}</div><div className={styles.traceGrid}><TraceGraph events={window.items} allEvents={filtered} selectedId={selected?.id ?? ""} onSelect={setSelectedId} /><aside className={styles.eventDetail} aria-live="polite">{selected ? <><header><div><span className="eyebrow">Selected evidence</span><h3>{eventTitle(selected)}</h3></div><span className={styles.eventStatus}>{selected.status}</span></header><dl><div><dt>Sequence</dt><dd>{selected.seq}</dd></div><div><dt>Effect</dt><dd>{selected.effect.replaceAll("_", " ")}</dd></div><div><dt>Recorded</dt><dd>{new Date(selected.recorded_at).toLocaleTimeString()}</dd></div><div><dt>Parent</dt><dd>{selected.parent ? short(selected.parent) : "Run root"}</dd></div></dl><EventEvidence event={selected} /></> : <p>No event matches this filter.</p>}</aside></div>{evidence.complete && <footer className={styles.traceFooter}><span>This final journal is bound to run <code>{short(evidence.run_id)}</code>. {window.pages > 1 ? "Every step remains available through the trace pages." : ""}</span><button className="primary-button" type="button" onClick={openEvaluate}>Evaluate this run</button></footer>}</section>;
}

function EventEvidence({ event }: { event: RunEvent }) {
  const preview = useMemo(() => bytePreview(event.rawJson, 64 * 1024), [event.rawJson]);
  function download() {
    const url = URL.createObjectURL(new Blob([event.rawJson], { type: "application/json" }));
    const anchor = document.createElement("a");
    anchor.href = url; anchor.download = `${event.id.replace(/[^A-Za-z0-9._-]/g, "_").slice(0, 128)}.json`; anchor.click(); URL.revokeObjectURL(url);
  }
  return <details><summary>Input and output evidence</summary><p className={styles.rawBoundary}>May contain prompts, tool results, or other sensitive run data.</p><pre>{preview.text}</pre>{preview.truncated && <div className={styles.rawBoundary}><span>Preview limited to 64 KiB of {preview.bytes.toLocaleString()} bytes.</span><button type="button" onClick={download}>Download exact event</button></div>}</details>;
}

export function traceWindow(events: RunEvent[], requestedPage: number, pageSize = 120) {
  const pages = Math.max(1, Math.ceil(events.length / pageSize));
  const page = Math.min(pages - 1, Math.max(0, Number.isInteger(requestedPage) ? requestedPage : 0));
  const oldestPageSize = pages === 1 ? events.length : events.length - ((pages - 1) * pageSize);
  const start = page === 0 ? 0 : oldestPageSize + ((page - 1) * pageSize);
  const end = page === 0 ? oldestPageSize : Math.min(events.length, start + pageSize);
  return { items: events.slice(start, end), page, pages, start, end };
}

function TraceGraph({ events, allEvents, selectedId, onSelect }: { events: RunEvent[]; allEvents: RunEvent[]; selectedId: string; onSelect: (id: string) => void }) {
  const viewportRef = useRef<HTMLDivElement>(null);
  const positions = useMemo(() => traceGraphLayout(events, allEvents), [allEvents, events]);
  const byId = new Map(positions.map((item) => [item.event.id, item]));
  const allIds = useMemo(() => new Set(allEvents.map((event) => event.id)), [allEvents]);
  const width = Math.max(520, ...positions.map((item) => item.x + 208));
  const height = Math.max(420, ...positions.map((item) => item.y + 76));
  useEffect(() => {
    const viewport = viewportRef.current;
    const selected = positions.find((item) => item.event.id === selectedId);
    if (!viewport || !selected) return;
    const left = Math.max(0, selected.x - (viewport.clientWidth - 184) / 2);
    const top = Math.max(0, selected.y - (viewport.clientHeight - 56) / 2);
    viewport.scrollTo({ left, top, behavior: "auto" });
  }, [positions, selectedId]);
  return <div className={styles.graphViewport} ref={viewportRef}><div className={styles.graphCanvas} style={{ width, height }}><svg className={styles.graphEdges} width={width} height={height} aria-hidden="true">{positions.map((item) => { const parent = item.event.parent ? byId.get(item.event.parent) : null; if (!parent && item.event.parent && allIds.has(item.event.parent)) return <path key={item.event.id} d={`M 0 ${item.y + 28} L ${item.x} ${item.y + 28}`} strokeDasharray="6 6" />; if (!parent) return null; const startX = parent.x + 184, startY = parent.y + 28, endX = item.x, endY = item.y + 28, bend = Math.max(34, Math.abs(endX - startX) * .48); return <path key={item.event.id} d={`M ${startX} ${startY} C ${startX + bend} ${startY}, ${endX - bend} ${endY}, ${endX} ${endY}`} />; })}</svg>{positions.map((item) => <button key={item.event.id} type="button" className={`${styles.visualNode} ${selectedId === item.event.id ? styles.visualNodeSelected : ""}`} style={{ left: item.x, top: item.y }} onClick={() => onSelect(item.event.id)} aria-pressed={selectedId === item.event.id} aria-label={`${eventTitle(item.event)}, sequence ${item.event.seq}, status ${item.event.status}`}><span className={styles.nodeMarker} data-status={item.event.status}>{kindIcon(item.event.kind)}</span><span><b>{eventTitle(item.event)}</b><small>{item.event.latency_ms ? `${item.event.latency_ms} ms` : `sequence ${item.event.seq}`}</small></span></button>)}</div></div>;
}

export function traceGraphLayout(events: RunEvent[], context: RunEvent[] = events) {
  const depthCache = new Map<string, number>();
  for (const event of context) depthCache.set(event.id, event.parent ? Math.min(5, (depthCache.get(event.parent) ?? -1) + 1) : 0);
  return events.map((event, index) => ({ event, x: 26 + (depthCache.get(event.id) ?? 0) * 196, y: 24 + index * 74 }));
}

function EvaluateWorkspace({ connectionKey, evidence, run, threadId, agent, objective }: { connectionKey: string; evidence: RunEvidence; run: RunSnapshot; threadId: string; agent: Assistant | null; objective: string }) {
  const addCase = useWorkStore((state) => state.addCase);
  const allCases = useWorkStore((state) => state.cases);
  const cases = useMemo(() => allCases.filter((item) => item.connectionKey === connectionKey), [allCases, connectionKey]);
  const [caseId, setCaseId] = useState(`run-${run.run_id.slice(0, 8)}`);
  const outputOptions = useMemo(() => evaluationOutputOptions(run.output), [run.output]);
  const [pointer, setPointer] = useState(outputOptions[0]?.pointer ?? "");
  const [expected, setExpected] = useState(() => outputOptions[0] ? evaluationValueText(outputOptions[0].value) : "");
  const [reviewed, setReviewed] = useState(false);
  const [saved, setSaved] = useState(false);
  const [caseError, setCaseError] = useState("");
  const caseReady = reviewed && Boolean(expected.trim()) && Boolean(caseId.trim()) && Boolean(pointer);
  function save(event: FormEvent) {
    event.preventDefault();
    if (!caseReady) return;
    const cleanId = caseId.trim(), cleanExpected = expected.trim();
    if (utf8(cleanId) > 256 || /[\u0000-\u001f\u007f]/.test(cleanId)) { setCaseError("Use a short case name without control characters."); return; }
    if (!validJsonPointer(pointer) || utf8(pointer) > 1_024) { setCaseError("Choose a valid final-state path from this run."); return; }
    if (utf8(cleanExpected) > 16 * 1024) { setCaseError("Expected JSON must be 16 KiB or smaller."); return; }
    let expectedValue: unknown;
    try { expectedValue = parseLossless(cleanExpected); }
    catch { setCaseError("Expected value must be valid JSON."); return; }
    if (cases.some((item) => item.caseId === cleanId)) { setCaseError("Each case name must be unique in this dataset."); return; }
    if (cases.length >= 100) { setCaseError("Download this 100-case dataset before starting another."); return; }
    if (!run.assistant_id) { setCaseError("This run does not carry an exact agent identity, so it cannot be published as an agent evaluation case."); return; }
    const value = { connectionKey, caseId: cleanId, runId: run.run_id, threadId, agentName: agent?.name ?? run.assistant_id, agentId: run.assistant_id, objective, pointer, expected: expectedValue };
    const preview = [...cases, { ...value, id: "preview", createdAt: new Date().toISOString() }];
    if (utf8(evaluationDatasetJsonl(preview)) > 128 * 1024) { setCaseError("Download this dataset before adding more source material."); return; }
    addCase(value); setCaseError(""); setSaved(true);
  }
  function download() {
    const text = evaluationDatasetJsonl(cases);
    const url = URL.createObjectURL(new Blob([text], { type: "application/x-ndjson" }));
    const anchor = document.createElement("a");
    anchor.href = url; anchor.download = "rusty-studio-evaluations@v1.jsonl"; anchor.click(); URL.revokeObjectURL(url);
  }
  return <section className={styles.evaluationShell}>
    <div className={styles.evaluateWorkspace}>
      <div className={styles.evaluationIntro}>
        <span className="eyebrow">Evaluate</span><h2>Turn this run into a reusable test</h2>
        <p>The case begins from the exact run and its final journal. Review the source before saving it as a test.</p>
        <dl><div><dt>Agent</dt><dd>{agent?.name ?? "Run identity only"}</dd></div><div><dt>Run</dt><dd>{short(run.run_id)}</dd></div><div><dt>Events</dt><dd>{evidence.events.length}</dd></div><div><dt>Outcome</dt><dd>{run.status}</dd></div></dl>
        {cases.length > 0 && <section className={styles.dataset}><header><div><span className="eyebrow">Working set</span><h3>{cases.length} reviewed case{cases.length === 1 ? "" : "s"}</h3></div><button type="button" className="secondary-button" onClick={download}>Download JSONL</button></header><ol>{cases.slice(-5).map((item) => <li key={item.id}><b>{item.caseId}</b><span>{item.agentName}</span></li>)}</ol></section>}
      </div>
      <form className={styles.evaluationForm} onSubmit={save}>
        <label>Case name<input value={caseId} onChange={(event) => { setCaseId(event.target.value); setSaved(false); setCaseError(""); }} /></label>
        <label>Frozen input<textarea rows={5} value={objective || "Input is available in the run evidence."} readOnly /></label>
        <label>Final-state path<select value={pointer} onChange={(event) => { const next = outputOptions.find((item) => item.pointer === event.target.value); setPointer(event.target.value); if (next) setExpected(evaluationValueText(next.value)); setSaved(false); setCaseError(""); }}>{outputOptions.length ? outputOptions.map((item) => <option key={item.pointer} value={item.pointer}>{item.pointer}</option>) : <option value="">No final output is available</option>}</select></label>
        <label>Expected value <span className="field-hint">JSON</span><textarea rows={5} value={expected} onChange={(event) => { setExpected(event.target.value); setSaved(false); setCaseError(""); }} placeholder='For example: "approved", true, or {"status":"ready"}' /></label>
        <label className={styles.ack}><input type="checkbox" checked={reviewed} onChange={(event) => setReviewed(event.target.checked)} />I reviewed the frozen input, state path, and expected value, and they are safe to publish.</label>
        {caseError && <p className={styles.error} role="alert">{caseError}</p>}
        <button className="primary-button" type="submit" disabled={!caseReady}>{saved ? "Case saved" : "Add evaluation case"}</button>
        <p>Unsaved cases are cleared when this browser session ends.</p>
      </form>
    </div>
    {cases.length > 0 && <EvaluationLane cases={cases} />}
  </section>;
}

function Metric({ label, value, tone = "default" }: { label: string; value: string; tone?: "default" | "danger" }) { return <span className={tone === "danger" ? styles.metricDanger : styles.metric}><small>{label}</small><b>{value}</b></span>; }
function short(value: string) { return value.length > 18 ? `${value.slice(0, 10)}…${value.slice(-5)}` : value; }
function statusCopy(status: string) { return ({ pending: "Waiting to start", running: "Agent is working", success: "Work completed", interrupted: "Waiting for your input", error: "Run needs attention", cancelled: "Run stopped safely" } as Record<string, string>)[status] ?? "Run status unavailable"; }
function previewEvents(evidence: RunEvidence | null) { if (!evidence?.events.length) return [{ label: "Agent starts", detail: "Waiting for an objective", done: false }, { label: "Model and tools", detail: "Appears as work happens", done: false }, { label: "Result and evidence", detail: "Captured at completion", done: false }]; const latest = evidence.events.slice(-3); return latest.map((event) => ({ label: eventTitle(event), detail: event.node_id ?? `Sequence ${event.seq}`, done: event.status === "ok" })); }
function eventCategory(event: RunEvent) { if (event.status === "error") return "error"; if (event.kind === "model_call") return "model"; if (["tool_call", "remote_call", "wasm_call"].includes(event.kind)) return "tool"; if (event.kind.startsWith("memory_")) return "memory"; return "execution"; }
function eventTitle(event: RunEvent) { return event.node_id ? `${event.node_id} · ${event.kind.replaceAll("_", " ")}` : event.kind.replaceAll("_", " ").replace(/\b\w/g, (letter) => letter.toUpperCase()); }
function kindIcon(kind: string) { if (kind === "model_call") return "M"; if (kind.includes("tool") || kind.includes("call")) return "T"; if (kind.includes("checkpoint")) return "C"; if (kind.includes("interrupt")) return "!"; return "•"; }
function utf8(value: string) { return new TextEncoder().encode(value).byteLength; }
function evaluationValueText(value: unknown) { return stringifyLossless(value, null, 2) ?? "null"; }
function evaluationOutputOptions(output: unknown) {
  if (!output || typeof output !== "object" || Array.isArray(output)) return [];
  return Object.entries(output as Record<string, unknown>).slice(0, 100).map(([key, value]) => ({
    pointer: `/${key.replaceAll("~", "~0").replaceAll("/", "~1")}`,
    value,
  }));
}
function validJsonPointer(value: string) { return value === "" || (value.startsWith("/") && value.split("/").slice(1).every((segment) => !/~(?:[^01]|$)/.test(segment))); }
function runStudioObjective(run: RunSnapshot | null) {
  const metadata = run?.metadata;
  if (!metadata || typeof metadata !== "object" || Array.isArray(metadata)) return "";
  const studio = (metadata as Record<string, unknown>).studio;
  if (!studio || typeof studio !== "object" || Array.isArray(studio)) return "";
  const objective = (studio as Record<string, unknown>).objective;
  return typeof objective === "string" ? objective : "";
}
