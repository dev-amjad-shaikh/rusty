import { useQueries, useQuery } from "@tanstack/react-query";
import { Link, useNavigate } from "@tanstack/react-router";
import { useRef, useState } from "react";
import type { Assistant, RunSnapshot } from "../../lib/contracts";
import { getOperationsSnapshot, getRun, listAssistants, type OperationAttentionItem } from "../../lib/api/client";
import { evidencePreview } from "../../lib/text";
import { readRecentWork, type RecentWorkIdentity } from "../../state/recentWork";
import { useRuntimeStore } from "../../state/runtime";
import { useWorkStore } from "../../state/work";
import { PageHeader } from "../../components/PageHeader";
import { RustyCardFrame, type RustyCardTone } from "../../components/RustyCardFrame";
import styles from "./CommandCenter.module.css";

type RunLane = "queued" | "working" | "needs" | "stuck" | "done";
type BoardFilter = "all" | "active" | "attention";

interface ExactRecentRun {
  identity: RecentWorkIdentity;
  run: RunSnapshot;
}

const lanes: Array<{ key: RunLane; label: string }> = [
  { key: "queued", label: "Queued" },
  { key: "working", label: "Working" },
  { key: "needs", label: "Needs you" },
  { key: "stuck", label: "Stuck" },
  { key: "done", label: "Done" },
];

export function CommandCenter() {
  const navigate = useNavigate();
  const work = useWorkStore();
  const info = useRuntimeStore((state) => state.info);
  const [boardFilter, setBoardFilter] = useState<BoardFilter>("all");
  const allWorkFilterRef = useRef<HTMLButtonElement>(null);
  const recentIdentities = readRecentWork();
  const assistants = useQuery({
    queryKey: ["assistants"],
    queryFn: () => listAssistants(),
  });
  const operations = useQuery({
    queryKey: ["operations"],
    queryFn: () => getOperationsSnapshot(),
    refetchInterval: 15_000,
  });
  const recentQueries = useQueries({
    queries: recentIdentities.map((identity) => ({
      queryKey: ["run", identity.runId],
      queryFn: () => getRun(identity.runId),
      retry: false,
    })),
  });
  const exactRuns = recentQueries.flatMap((query, index): ExactRecentRun[] => {
    const run = query.data;
    const identity = recentIdentities[index];
    return run && identity && run.run_id === identity.runId && run.thread_id === identity.threadId ? [{ identity, run }] : [];
  });
  const mismatchedRuns = recentQueries.filter((query, index) => Boolean(query.data) && (query.data!.run_id !== recentIdentities[index]?.runId || query.data!.thread_id !== recentIdentities[index]?.threadId)).length;
  const agentById = new Map((assistants.data ?? []).map((agent) => [agent.assistant_id, agent]));
  const availableGraphs = new Set(info?.graphs.map((graph) => graph.name) ?? []);
  const availableAgents = (assistants.data ?? []).filter((agent) => !agent.archived_at && availableGraphs.has(agent.graph));
  const starterAgent = availableAgents.length === 1 ? availableAgents[0] : null;
  const grouped = groupRuns(exactRuns);
  const attention = operations.data?.attention ?? [];
  const visibleRuns = filterRuns(grouped, boardFilter);
  const visibleAttention = boardFilter === "active" ? [] : attention;
  const evidenceUnavailable = operations.data?.unavailable ?? [];
  const loadingRuns = recentQueries.some((query) => query.isLoading);
  const unavailableRuns = recentQueries.filter((query) => query.isError).length;
  const runEvidencePartial = unavailableRuns > 0 || mismatchedRuns > 0;
  const attentionEvidencePartial = operations.isError || evidenceUnavailable.length > 0;
  const evidencePartial = runEvidencePartial || attentionEvidencePartial;
  const runningCount = grouped.working.length;
  const needsCount = grouped.needs.length + attention.length;
  const stuckCount = grouped.stuck.length;
  const blockedCount = needsCount + stuckCount;
  const boardEmpty = Object.values(grouped).every((items) => items.length === 0) && attention.length === 0;
  const verifiedEmpty = boardEmpty && !loadingRuns && !operations.isLoading && !evidencePartial;
  const laneCount = (key: RunLane) => key === "needs" ? visibleRuns.needs.length + visibleAttention.length : visibleRuns[key].length;
  const totalVisible = lanes.reduce((sum, lane) => sum + laneCount(lane.key), 0);
  const visibleBoardEmpty = totalVisible === 0;
  const verifiedFilterEmpty = boardFilter !== "all" && visibleBoardEmpty && !loadingRuns && !operations.isLoading && !evidencePartial;
  const blockedRuns = grouped.needs.length + grouped.stuck.length;
  const oldestAttention = attention.reduce((min, item) => { const at = Date.parse(item.observedAt); return Number.isNaN(at) ? min : Math.min(min, at); }, Number.POSITIVE_INFINITY);
  const oldestWait = blockedCount > 0 && blockedRuns === 0 && Number.isFinite(oldestAttention) ? ageLabel(new Date(oldestAttention).toISOString()) : "";

  function beginFirstWork() {
    if (!starterAgent) return;
    work.prepare(starterAgent);
    navigate({ to: "/work" });
  }

  return <section className={`page ${styles.command}`} aria-labelledby="command-heading">
    <PageHeader
      headingId="command-heading"
      eyebrow="Command center"
      title="Work board"
      variant="board"
      detail={<><span className={styles.liveSummary}><i aria-hidden="true" />{loadingRuns || operations.isLoading ? "Checking work…" : runEvidencePartial ? "Work status incomplete" : attentionEvidencePartial ? `${runningCount} running · attention status incomplete` : `${runningCount} running · ${needsCount} need you · ${stuckCount} stuck`}</span>{!evidencePartial && blockedCount > 0 && <span className={styles.nowBadge}>Now: {blockedCount} blocked{oldestWait ? ` · oldest ${oldestWait}` : ""}</span>}</>}
      description="Session runs and current operational exceptions."
      actions={<div className={styles.boardFilters} role="group" aria-label="Filter work board">{([ ["all", "All work"], ["active", "Active"], ["attention", "Needs attention"] ] as Array<[BoardFilter, string]>).map(([key, label]) => <button ref={key === "all" ? allWorkFilterRef : undefined} key={key} type="button" aria-pressed={boardFilter === key} onClick={() => setBoardFilter(key)}>{label}</button>)}</div>}
    />

    {(evidenceUnavailable.length > 0 || operations.isError || unavailableRuns > 0 || mismatchedRuns > 0) && <div className={styles.evidenceWarning} role="status">
      <span aria-hidden="true">!</span><p><b>Some evidence is unavailable.</b> {[...evidenceUnavailable, ...(operations.isError ? ["operations"] : []), ...(unavailableRuns ? [`${unavailableRuns} recent run${unavailableRuns === 1 ? "" : "s"}`] : []), ...(mismatchedRuns ? [`${mismatchedRuns} crossed run ${mismatchedRuns === 1 ? "identity" : "identities"}`] : [])].join(", ")} could not be verified.</p>
    </div>}

    <section className={styles.board} aria-label="Current work">
      {verifiedEmpty && <div className={styles.emptyBoard}>
        <div><h2>{starterAgent ? "Ready for the first task" : availableAgents.length > 1 ? "Choose the next agent" : assistants.isLoading ? "Checking available agents" : assistants.isError ? "Agent availability is unknown" : (assistants.data?.length ?? 0) > 0 ? "Agents need attention" : "Build your first agent"}</h2><p>{starterAgent ? `${evidencePreview(starterAgent.name, 120)} is available. Start one real objective and follow it across this board.` : availableAgents.length > 1 ? `${availableAgents.length} agents are available. Choose the right one for the objective before work starts.` : assistants.isLoading ? "Rusty is checking which active definition can start work." : assistants.isError ? "Open the portfolio to refresh agent availability before starting work." : (assistants.data?.length ?? 0) > 0 ? "Open the portfolio to restore an archived agent or choose an available behavior." : "Shape its responsibility and capabilities, then bring its first task back here."}</p></div>
        {starterAgent ? <button type="button" onClick={beginFirstWork}>Start with {evidencePreview(starterAgent.name, 80)}</button> : availableAgents.length > 1 ? <Link to="/work">Choose an agent</Link> : assistants.isLoading ? null : assistants.isError || (assistants.data?.length ?? 0) > 0 ? <Link to="/agents">Review agents</Link> : <Link to="/agents/new">Create first agent</Link>}
      </div>}
      {verifiedFilterEmpty && <div className={styles.emptyBoard}>
        <div><h2>{boardFilter === "active" ? "No active work" : "Nothing needs attention"}</h2><p>Other work is still available on the full board.</p></div>
        <button type="button" onClick={() => { setBoardFilter("all"); requestAnimationFrame(() => allWorkFilterRef.current?.focus()); }}>Show all work</button>
      </div>}
      {!verifiedEmpty && !verifiedFilterEmpty && <div className={styles.lanes}>
        {lanes.map((lane) => {
          const count = laneCount(lane.key);
          return <section className={styles.lane} data-lane={lane.key} key={lane.key} aria-labelledby={`lane-${lane.key}`}>
          <div className={styles.laneMeter} aria-hidden="true"><span style={{ width: count === 0 ? "0%" : `${Math.max(8, Math.round(count / Math.max(1, totalVisible) * 100))}%` }} /></div>
          <header><span className={styles.laneSignal} data-tone={lane.key} aria-hidden="true" /><h2 id={`lane-${lane.key}`}>{lane.label}</h2><b>{count}</b></header>
          <div className={styles.cards}>
            {visibleRuns[lane.key].map((item) => <RunCard key={item.run.run_id} item={item} lane={lane.key} agent={item.run.assistant_id ? agentById.get(item.run.assistant_id) : undefined} />)}
            {lane.key === "needs" && visibleAttention.slice(0, 6).map((item) => <ExceptionCard key={`${item.source}-${item.id}`} item={item} />)}
            {lane.key === "needs" && visibleAttention.length > 6 && <Link className={styles.moreAttention} to="/operations">Review {visibleAttention.length - 6} more in Operations</Link>}
            {count === 0 && <p className={styles.emptyLane}>{loadingRuns || operations.isLoading ? "Checking evidence…" : emptyLaneCopy(lane.key)}</p>}
          </div>
        </section>;})}
      </div>}
    </section>

  </section>;
}

function RunCard({ item, agent, lane }: { item: ExactRecentRun; agent?: Assistant; lane: RunLane }) {
  const objective = runObjective(item.run);
  const label = objective || agent?.name || item.run.graph;
  const agentName = agent ? evidencePreview(agent.name, 100) : evidencePreview(item.run.graph, 100);
  return <RustyCardFrame tone={lane}><Link className={styles.workCard} data-lane={lane} to="/work/$threadId/runs/$runId" params={{ threadId: item.identity.threadId, runId: item.identity.runId }} aria-label={`Open ${evidencePreview(label, 180)}, status ${item.run.status}`}>
    {(lane === "working" || lane === "needs" || lane === "stuck") && <span className={styles.riskSignal} aria-hidden="true" />}
    <h3>{evidencePreview(label, 220)}</h3>
    <span className={styles.agentRow}><span>{initials(agentName)}</span><b>{agentName}</b></span>
    <p className={styles.cardStatus}>{runState(item.run)}</p>
    {lane === "working" && <span className={styles.activityBar} aria-hidden="true"><i /></span>}
    <small><span>{openedContext(item.identity.savedAt)}</span>{item.run.attempt > 1 && <span>Retry {item.run.attempt - 1}</span>}</small>
  </Link></RustyCardFrame>;
}

function ExceptionCard({ item }: { item: OperationAttentionItem }) {
  const destination = item.runId && item.threadId
    ? { to: "/work/$threadId/runs/$runId/trace" as const, params: { threadId: item.threadId, runId: item.runId } }
    : null;
  const content = <><span className={styles.riskSignal} aria-hidden="true" /><h3>{item.title}</h3><span className={styles.agentRow}><span>{item.source === "artifact" ? "AR" : "OP"}</span><b>{item.source === "artifact" ? "Artifact" : "Operation"}</b></span><p className={styles.cardStatus}>{evidencePreview(item.detail, 220)}</p><small><span>{waitingTime(item.observedAt)}</span><span>Review</span></small></>;
  return destination
    ? <RustyCardFrame tone="needs"><Link className={styles.workCard} data-lane="needs" to={destination.to} params={destination.params} aria-label={`Inspect ${item.title}`}>{content}</Link></RustyCardFrame>
    : <RustyCardFrame tone="needs"><Link className={styles.workCard} data-lane="needs" to="/operations" aria-label={`Review ${item.title} in Operations`}>{content}</Link></RustyCardFrame>;
}

function groupRuns(items: ExactRecentRun[]): Record<RunLane, ExactRecentRun[]> {
  const grouped: Record<RunLane, ExactRecentRun[]> = { queued: [], working: [], needs: [], stuck: [], done: [] };
  for (const item of items) grouped[runLane(item.run.status)].push(item);
  return grouped;
}

function filterRuns(grouped: Record<RunLane, ExactRecentRun[]>, filter: BoardFilter): Record<RunLane, ExactRecentRun[]> {
  if (filter === "all") return grouped;
  if (filter === "active") return { queued: grouped.queued, working: grouped.working, needs: [], stuck: [], done: [] };
  return { queued: [], working: [], needs: grouped.needs, stuck: grouped.stuck, done: [] };
}

function runLane(status: RunSnapshot["status"]): RunLane {
  if (status === "pending") return "queued";
  if (status === "running") return "working";
  if (status === "interrupted") return "needs";
  if (status === "success" || status === "cancelled") return "done";
  return "stuck";
}

function statusLabel(status: RunSnapshot["status"]) {
  return ({ pending: "Queued", running: "Running", success: "Completed", interrupted: "Waiting for your input", error: "Failed", cancelled: "Stopped safely" } as const)[status];
}

function runState(run: RunSnapshot) {
  const detail = [run.message, run.error].find((value) => typeof value === "string" && value.trim());
  return detail ? evidencePreview(detail, 220) : statusLabel(run.status);
}

function runObjective(run: RunSnapshot) {
  if (!run.metadata || typeof run.metadata !== "object") return "";
  const studio = (run.metadata as Record<string, unknown>).studio;
  if (!studio || typeof studio !== "object") return "";
  const objective = (studio as Record<string, unknown>).objective;
  return typeof objective === "string" ? objective : "";
}

function emptyLaneCopy(lane: RunLane) {
  if (lane === "queued") return "No recent work is waiting.";
  if (lane === "working") return "No recent work is running.";
  if (lane === "needs") return "Nothing is waiting on you.";
  if (lane === "stuck") return "Nothing is stuck.";
  return "Completed runs you open will appear here.";
}

function initials(value: string) { return value.split(/\s+/u).filter(Boolean).slice(0, 2).map((part) => Array.from(part)[0]?.toUpperCase() ?? "").join("") || "AG"; }
function ageLabel(iso: string) {
  const at = Date.parse(iso);
  if (Number.isNaN(at)) return "";
  const seconds = Math.max(0, Math.round((Date.now() - at) / 1000));
  if (seconds < 60) return `${seconds}s`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m`;
  if (seconds < 86_400) return `${Math.floor(seconds / 3600)}h ${Math.floor((seconds % 3600) / 60)}m`;
  return `${Math.floor(seconds / 86_400)}d`;
}
function waitingTime(value: string) { const age = ageLabel(value); return age ? `${age} waiting` : "Time unavailable"; }
function openedContext(value: string) { const date = new Date(value); return Number.isNaN(date.valueOf()) ? "Opened this session" : `Opened ${date.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" })}`; }
