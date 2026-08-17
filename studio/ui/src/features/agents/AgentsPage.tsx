import { useQuery } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { listAssistants } from "../../lib/api/client";
import type { Assistant } from "../../lib/contracts";
import { evidencePreview } from "../../lib/text";
import { useRuntimeStore } from "../../state/runtime";
import { PageHeader } from "../../components/PageHeader";
import { humanizeIdentifier } from "./AgentIntentEditor";
import styles from "./AgentsPage.module.css";

export function AgentsPage() {
  const info = useRuntimeStore((state) => state.info);
  const catalog = useQuery({ queryKey: ["assistants"], queryFn: () => listAssistants() });
  const agents = catalog.data ?? [];
  const availableGraphs = new Set<string>(info?.graphs.map((graph) => graph.name) ?? []);
  const active = agents.filter((agent) => !agent.archived_at && availableGraphs.has(agent.graph)).length;
  const unavailable = agents.filter((agent) => !agent.archived_at && !availableGraphs.has(agent.graph)).length;
  const portfolioSummary = catalog.isLoading ? "Loading this workspace…"
    : catalog.isError ? "Agent count unavailable"
    : `${agents.length} in this workspace · ${active} available${unavailable ? ` · ${unavailable} need attention` : ""}`;

  return (
    <section className={`${styles.portfolio} page`} aria-labelledby="agents-heading">
      <PageHeader
        headingId="agents-heading"
        eyebrow="Agents"
        title="Agent portfolio"
        description={portfolioSummary}
        actions={<div className={styles.portfolioActions}>
          <Link className="secondary-button" to="/agents/prompts">Prompt library</Link>
          <Link className="primary-button" to="/agents/new">New agent</Link>
        </div>}
      />

      {catalog.isLoading ? (
        <div className={styles.loading} role="status">Loading agents…</div>
      ) : catalog.isError ? (
        <div className={styles.portfolioEmpty} role="alert">
          <span className={styles.emptyMark} aria-hidden="true">!</span>
          <div><h2>Agents could not be loaded</h2><p>{catalog.error instanceof Error ? catalog.error.message : "Try the request again."}</p></div>
          <button className="primary-button" type="button" onClick={() => catalog.refetch()}>Retry</button>
        </div>
      ) : agents.length ? (
        <AgentTable agents={agents} availableGraphs={availableGraphs} />
      ) : (
        <div className={styles.portfolioEmpty}>
          <span className={styles.emptyMark} aria-hidden="true">A</span>
          <div><h2>No agents yet</h2><p>Create one clear responsibility, then shape its model, knowledge, tools, output, and limits.</p></div>
          <Link className="primary-button" to="/agents/new">Start first agent</Link>
        </div>
      )}
    </section>
  );
}

function AgentTable({ agents, availableGraphs }: { agents: Assistant[]; availableGraphs: Set<string> }) {
  return <div className={styles.agentTable} role="table" aria-label="Agent portfolio">
    <div className={styles.tableHead} role="row">
      <span role="columnheader">Agent</span><span role="columnheader">Status</span><span role="columnheader">Active version</span><span role="columnheader">Behavior</span><span role="columnheader">Versions</span><span role="columnheader"><span className="sr-only">Open</span></span>
    </div>
    {agents.map((agent) => {
      const available = availableGraphs.has(agent.graph);
      const status = agent.archived_at ? "Archived" : available ? "Active" : "Unavailable";
      return <article className={styles.agentRow} role="row" key={agent.assistant_id}>
        <div className={styles.agentIdentity} role="cell">
          <span aria-hidden="true">{initials(agent.name)}</span>
          <div><Link to="/agents/$assistantId" params={{ assistantId: agent.assistant_id }}>{evidencePreview(agent.name, 256)}</Link><small>{description(agent)}</small></div>
        </div>
        <div role="cell" data-label="Status"><span className={status === "Active" ? styles.statusActive : status === "Unavailable" ? styles.statusUnavailable : styles.statusArchived}>{status}</span></div>
        <code role="cell" data-label="Active version" title={agent.active_version_id}>{agent.active_version_id.slice(0, 12)}</code>
        <span role="cell" data-label="Behavior" title={agent.graph}>{humanizeIdentifier(evidencePreview(agent.graph, 256))}</span>
        <span role="cell" data-label="Versions">{agent.version_count}</span>
        <div className={styles.openCell} role="cell"><Link className={styles.openAgent} to="/agents/$assistantId" params={{ assistantId: agent.assistant_id }} aria-label={`Open ${evidencePreview(agent.name, 256)}`}>Open <span aria-hidden="true">→</span></Link></div>
      </article>;
    })}
    <footer>{agents.length} agent{agents.length === 1 ? "" : "s"}</footer>
  </div>;
}

function description(agent: Assistant) {
  return typeof agent.metadata === "object" && agent.metadata && "description" in agent.metadata
    ? evidencePreview(String(agent.metadata.description), 500)
    : "No responsibility added";
}

function initials(name: string) {
  const words = name.trim().split(/\s+/).filter(Boolean);
  return (words.length > 1 ? `${words[0][0]}${words[1][0]}` : name.slice(0, 2)).toUpperCase();
}
