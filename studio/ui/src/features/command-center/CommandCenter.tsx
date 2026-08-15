import { useQueries, useQuery } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import type { Assistant, RunSnapshot } from "../../lib/contracts";
import { getOperationsSnapshot, getRun, listAssistants, type OperationAttentionItem } from "../../lib/api/client";
import { evidencePreview } from "../../lib/text";
import { durableConnectionScope, readRecentWork, type RecentWorkIdentity } from "../../state/recentWork";
import { useConnectionStore } from "../../state/connection";
import styles from "./CommandCenter.module.css";

type RunLane = "queued" | "working" | "attention" | "done";

interface ExactRecentRun {
  identity: RecentWorkIdentity;
  run: RunSnapshot;
}

const lanes: Array<{ key: RunLane; label: string; hint: string }> = [
  { key: "queued", label: "Queued", hint: "Accepted and waiting" },
  { key: "working", label: "Working", hint: "Executing now" },
  { key: "attention", label: "Needs attention", hint: "Stopped or needs review" },
  { key: "done", label: "Done", hint: "Recently completed" },
];

export function CommandCenter() {
  const { connection, info, openDialog } = useConnectionStore();
  const scope = connection ? durableConnectionScope(connection) : "disconnected";
  const recentIdentities = connection ? readRecentWork(scope) : [];
  const assistants = useQuery({
    queryKey: connection ? [connection.epoch, connection.origin, connection.tenantFingerprint, "assistants"] : ["command-agents", "disconnected"],
    queryFn: () => listAssistants(connection!),
    enabled: Boolean(connection),
  });
  const operations = useQuery({
    queryKey: connection ? [connection.epoch, connection.origin, connection.tenantFingerprint, "operations"] : ["command-operations", "disconnected"],
    queryFn: () => getOperationsSnapshot(connection!),
    enabled: Boolean(connection),
    refetchInterval: 15_000,
  });
  const recentQueries = useQueries({
    queries: recentIdentities.map((identity) => ({
      queryKey: [connection!.epoch, connection!.origin, connection!.tenantFingerprint, "run", identity.runId],
      queryFn: () => getRun(connection!, identity.runId),
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
  const grouped = groupRuns(exactRuns);
  const attention = operations.data?.attention ?? [];
  const availableGraphs = new Set(info?.graphs.map((graph) => graph.name) ?? []);
  const activeAgents = assistants.data?.filter((agent) => !agent.archived_at && availableGraphs.has(agent.graph)).length ?? 0;
  const unavailableAgents = assistants.data?.filter((agent) => Boolean(agent.archived_at) || !availableGraphs.has(agent.graph)).length ?? 0;
  const evidenceUnavailable = operations.data?.unavailable ?? [];
  const loadingRuns = recentQueries.some((query) => query.isLoading);
  const unavailableRuns = recentQueries.filter((query) => query.isError).length;

  if (!connection) return <section className={`page ${styles.command}`} aria-labelledby="command-heading">
    <header className={styles.hero}>
      <div><span className="eyebrow">Rusty command center</span><h1 id="command-heading">Bring your agent system<br /><span>into view.</span></h1><p>Open a local or remote Rusty workspace. Your agents, recent runs, and exceptions will meet here without leaving your infrastructure.</p></div>
      <button className="primary-button" type="button" onClick={openDialog}>Choose workspace</button>
    </header>
    <div className={styles.offlineGrid}>
      <article><span>01</span><h2>Build</h2><p>Shape a versioned agent with models, memory, tools, output, and guardrails.</p><Link to="/agents">Start a local draft</Link></article>
      <article><span>02</span><h2>Run</h2><p>Give the agent an objective and follow its exact run evidence in one workspace.</p><button type="button" onClick={openDialog}>Open a workspace</button></article>
      <article><span>03</span><h2>Improve</h2><p>Turn a completed run into evaluation evidence, then compare before release.</p><Link to="/work">See the run workspace</Link></article>
    </div>
  </section>;

  return <section className={`page ${styles.command}`} aria-labelledby="command-heading">
    <header className={styles.hero}>
      <div><span className="eyebrow">Command center · live workspace</span><h1 id="command-heading">The work board</h1><p>Recent runs opened in this Studio session, joined with current operational exceptions.</p></div>
      <div className={styles.heroActions}><Link className="secondary-button" to="/agents">Open agents</Link><Link className="primary-button" to="/work">Start work</Link></div>
    </header>

    <section className={styles.signalStrip} aria-label="Workspace summary">
      <SummarySignal label="Active agents" value={assistants.isError ? "Unknown" : assistants.isLoading ? "…" : String(activeAgents)} tone="live" />
      <SummarySignal label="Recent runs" value={loadingRuns ? "…" : String(exactRuns.length)} />
      <SummarySignal label="Needs attention" value={operations.isError ? "Unknown" : operations.isLoading ? "…" : String(grouped.attention.length + attention.length)} tone={(grouped.attention.length + attention.length) > 0 ? "warn" : "quiet"} />
      <SummarySignal label="Completed" value={loadingRuns ? "…" : String(grouped.done.length)} tone="done" />
    </section>

    {(evidenceUnavailable.length > 0 || operations.isError || unavailableRuns > 0 || mismatchedRuns > 0) && <div className={styles.evidenceWarning} role="status">
      <span aria-hidden="true">!</span><p><b>Some evidence is unavailable.</b> {[...evidenceUnavailable, ...(operations.isError ? ["operations"] : []), ...(unavailableRuns ? [`${unavailableRuns} recent run${unavailableRuns === 1 ? "" : "s"}`] : []), ...(mismatchedRuns ? [`${mismatchedRuns} crossed run ${mismatchedRuns === 1 ? "identity" : "identities"}`] : [])].join(", ")} could not be verified.</p>
    </div>}

    <section className={styles.board} aria-labelledby="board-heading">
      <header className={styles.sectionHead}><div><span className="eyebrow">Current movement</span><h2 id="board-heading">Work board</h2></div><Link to="/work">Open run workspace <span aria-hidden="true">→</span></Link></header>
      <div className={styles.lanes}>
        {lanes.map((lane) => <section className={styles.lane} key={lane.key} aria-labelledby={`lane-${lane.key}`}>
          <header><span className={styles.laneSignal} data-tone={lane.key} aria-hidden="true" /><div><h3 id={`lane-${lane.key}`}>{lane.label}</h3><p>{lane.hint}</p></div><b>{lane.key === "attention" ? grouped.attention.length + attention.length : grouped[lane.key].length}</b></header>
          <div className={styles.cards}>
            {grouped[lane.key].map((item) => <RunCard key={item.run.run_id} item={item} agent={item.run.assistant_id ? agentById.get(item.run.assistant_id) : undefined} />)}
            {lane.key === "attention" && attention.slice(0, 6).map((item) => <ExceptionCard key={`${item.source}-${item.id}`} item={item} />)}
            {grouped[lane.key].length === 0 && (lane.key !== "attention" || attention.length === 0) && <p className={styles.emptyLane}>{loadingRuns || operations.isLoading ? "Checking evidence…" : emptyLaneCopy(lane.key)}</p>}
          </div>
        </section>)}
      </div>
    </section>

    <section className={styles.portfolio} aria-labelledby="portfolio-heading">
      <div><span className="eyebrow">Capability inventory</span><h2 id="portfolio-heading">Agent portfolio</h2><p>Versioned workers currently visible in this workspace.</p></div>
      <div className={styles.portfolioStats}>
        <article><span>Active</span><b>{assistants.isError ? "—" : activeAgents}</b><small>{assistants.isError ? "Catalog unavailable" : "Available definitions"}</small></article>
        <article><span>Unavailable</span><b>{assistants.isError ? "—" : unavailableAgents}</b><small>Archived or missing behavior</small></article>
        <article><span>Automations</span><b>{operations.data?.systems.automations ?? "—"}</b><small>{operations.data?.systems.automations === null ? "Inventory unavailable" : "Configured entry points"}</small></article>
        <article><span>Schedules</span><b>{operations.data?.systems.schedules ?? "—"}</b><small>{operations.data?.systems.schedules === null ? "Inventory unavailable" : "Configured cadences"}</small></article>
      </div>
      <Link className="secondary-button" to="/agents">Manage agents</Link>
    </section>
  </section>;
}

function SummarySignal({ label, value, tone = "neutral" }: { label: string; value: string; tone?: "neutral" | "live" | "warn" | "quiet" | "done" }) {
  return <article data-tone={tone}><span>{label}</span><b>{value}</b></article>;
}

function RunCard({ item, agent }: { item: ExactRecentRun; agent?: Assistant }) {
  const objective = runObjective(item.run);
  const label = objective || agent?.name || item.run.graph;
  return <Link className={styles.workCard} to="/work/$threadId/runs/$runId" params={{ threadId: item.identity.threadId, runId: item.identity.runId }} aria-label={`Open ${evidencePreview(label, 180)}, status ${item.run.status}`}>
    <span className={styles.cardType}>{agent ? evidencePreview(agent.name, 100) : evidencePreview(item.run.graph, 100)}</span>
    <h4>{evidencePreview(label, 220)}</h4>
    <p><span>{statusLabel(item.run.status)}</span><span>Attempt {item.run.attempt}</span></p>
    <small>{compactIdentity(item.run.run_id)}</small>
  </Link>;
}

function ExceptionCard({ item }: { item: OperationAttentionItem }) {
  const destination = item.runId && item.threadId
    ? { to: "/work/$threadId/runs/$runId/trace" as const, params: { threadId: item.threadId, runId: item.runId } }
    : null;
  const content = <><span className={styles.cardType}>{item.source === "artifact" ? "Artifact" : "Operation"}</span><h4>{item.title}</h4><p>{evidencePreview(item.detail, 220)}</p><small>{observedTime(item.observedAt)}</small></>;
  return destination
    ? <Link className={`${styles.workCard} ${styles.exceptionCard}`} to={destination.to} params={destination.params} aria-label={`Inspect ${item.title}`}>{content}</Link>
    : <Link className={`${styles.workCard} ${styles.exceptionCard}`} to="/operations" aria-label={`Review ${item.title} in Operations`}>{content}</Link>;
}

function groupRuns(items: ExactRecentRun[]): Record<RunLane, ExactRecentRun[]> {
  const grouped: Record<RunLane, ExactRecentRun[]> = { queued: [], working: [], attention: [], done: [] };
  for (const item of items) grouped[runLane(item.run.status)].push(item);
  return grouped;
}

function runLane(status: RunSnapshot["status"]): RunLane {
  if (status === "pending") return "queued";
  if (status === "running") return "working";
  if (status === "success") return "done";
  return "attention";
}

function statusLabel(status: RunSnapshot["status"]) {
  return ({ pending: "Queued", running: "Running", success: "Completed", interrupted: "Interrupted", error: "Failed", cancelled: "Cancelled" } as const)[status];
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
  if (lane === "attention") return "No verified exception is in this view.";
  return "Completed runs you open will appear here.";
}

function compactIdentity(value: string) { return value.length > 18 ? `${value.slice(0, 8)}…${value.slice(-6)}` : value; }
function observedTime(value: string) { const date = new Date(value); return Number.isNaN(date.valueOf()) ? "Time unavailable" : date.toLocaleString(); }
