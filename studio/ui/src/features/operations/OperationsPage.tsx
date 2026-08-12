import { useQuery } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { useEffect, useState } from "react";
import { getOperationsSnapshot, type OperationAttentionItem } from "../../lib/api/client";
import { useConnectionStore } from "../../state/connection";
import { evidencePreview } from "../../lib/text";
import { ArtifactInspector } from "../work/artifacts/ArtifactInspector";
import { getRunArtifact } from "../../lib/api/artifacts";
import styles from "./OperationsPage.module.css";

const systems = [
  { key: "tasks", label: "Task queue", detail: "Work that needs action", href: "/advanced/legacy?studio=tasks" },
  { key: "automations", label: "Automations", detail: "Event-driven entry points", href: "/advanced/legacy?studio=automations" },
  { key: "schedules", label: "Schedules", detail: "Recurring execution", href: "/advanced/legacy?studio=schedules" },
] as const;

function observedTime(value: string) {
  const date = new Date(value);
  return Number.isNaN(date.valueOf()) ? "Observation time unavailable" : date.toLocaleString();
}

function EvidencePanel({ item, close }: { item: OperationAttentionItem; close: () => void }) {
  const { connection } = useConnectionStore();
  const artifact = useQuery({
    queryKey: connection && item.source === "artifact" && item.artifactId ? [connection.epoch, connection.origin, connection.tenantFingerprint, "artifact", item.artifactId] : ["artifact", "idle"],
    queryFn: () => getRunArtifact(connection!, item.artifactId!),
    enabled: Boolean(connection && item.source === "artifact" && item.artifactId),
  });
  if (item.source === "artifact") {
    return <>
      <aside className={styles.evidence} aria-labelledby="operation-evidence-heading">
        <div className={styles.evidenceHead}>
          <div><span className="eyebrow">Artifact exception</span><h2 id="operation-evidence-heading">{item.title}</h2></div>
          <button className="secondary-button" type="button" onClick={close}>Close</button>
        </div>
        <p>{item.detail}</p>
        <dl>
          <div><dt>Observed</dt><dd>{observedTime(item.observedAt)}</dd></div>
          {item.artifactId && <div><dt>Artifact</dt><dd><code>{evidencePreview(item.artifactId, 64)}</code></dd></div>}
        </dl>
      </aside>
      {artifact.data && <ArtifactInspector artifact={artifact.data} onClose={close} />}
    </>;
  }
  return <aside className={styles.evidence} aria-labelledby="operation-evidence-heading">
    <div className={styles.evidenceHead}>
      <div><span className="eyebrow">Task evidence</span><h2 id="operation-evidence-heading">{item.title}</h2></div>
      <button className="secondary-button" type="button" onClick={close}>Close</button>
    </div>
    <p>{item.detail}</p>
    <dl>
      <div><dt>Task</dt><dd>{evidencePreview(item.id, 256)}</dd></div>
      <div><dt>Observed</dt><dd>{observedTime(item.observedAt)}</dd></div>
      <div><dt>Retry</dt><dd>{item.retryScheduled ? "Scheduled" : "No retry scheduled"}</dd></div>
      {item.runId && <div><dt>Run</dt><dd>{item.runId}</dd></div>}
    </dl>
    {item.runId && item.threadId
      ? <Link className="primary-button" to="/work/$threadId/runs/$runId/trace" params={{ threadId: item.threadId, runId: item.runId }}>Inspect contributing run</Link>
      : <p className={styles.noRun}>No run journal is linked to this task. The task evidence above is the authoritative next starting point.</p>}
  </aside>;
}

export function OperationsPage() {
  const { connection, openDialog } = useConnectionStore();
  const [selected, setSelected] = useState<OperationAttentionItem | null>(null);
  useEffect(() => { setSelected(null); }, [connection?.epoch, connection?.origin, connection?.tenantFingerprint]);
  const snapshot = useQuery({
    queryKey: connection ? [connection.epoch, connection.origin, connection.tenantFingerprint, "operations"] : ["operations", "disconnected"],
    queryFn: () => getOperationsSnapshot(connection!),
    enabled: Boolean(connection),
    refetchInterval: 15_000,
  });
  const data = snapshot.data;
  useEffect(() => {
    if (!selected || !data) return;
    const current = data.attention.find((item) => item.id === selected.id) ?? null;
    if (current !== selected) setSelected(current);
  }, [data, selected]);

  return <section className="page" aria-labelledby="operations-heading">
    <header className="page-header">
      <div><span className="eyebrow">Operations</span><h1 id="operations-heading">Intervene only when needed</h1><p>Terminal task failures appear first. Schedule and automation catalogs stay quiet until you open them.</p></div>
      {!connection && <button className="primary-button" type="button" onClick={openDialog}>Connect Rusty</button>}
      {connection && <button className="secondary-button" type="button" onClick={() => snapshot.refetch()} disabled={snapshot.isFetching}>{snapshot.isFetching ? "Refreshing…" : "Refresh"}</button>}
    </header>

    {!connection ? <div className="empty-state"><span className="eyebrow">Attention queue</span><h2>Connect to load operations</h2><p>Failures and routine systems will remain clearly separated.</p></div>
      : snapshot.isLoading ? <div className={styles.loading}>Loading operational evidence…</div>
      : snapshot.isError ? <div className={styles.attention}><div><span className="eyebrow">Evidence unavailable</span><h2>Operations could not be loaded</h2><p>{snapshot.error instanceof Error ? snapshot.error.message : "Try again."}</p></div></div>
      : <>
        <section className={styles.attention} aria-labelledby="attention-heading">
          <header>
            <div><span className="eyebrow">Needs attention</span><h2 id="attention-heading">{data?.attention.length ? `${data.attention.length} item${data.attention.length === 1 ? "" : "s"}` : "No task failures need action"}</h2></div>
            {data?.unavailable.length ? <span className={styles.unknown}>Not observed: {data.unavailable.join(", ")}</span> : <span className={styles.observed}>Task failure queues and catalogs observed</span>}
          </header>
          {data?.attention.length ? <ol className={styles.attentionList}>{data.attention.map((item) => <li key={item.id}>
            <span className={styles.severity} aria-hidden="true">!</span>
            <div><b>{item.title}</b><p>{item.detail}</p><small>{observedTime(item.observedAt)}</small></div>
            <button type="button" aria-expanded={selected?.id === item.id} onClick={() => setSelected(item)}>Review</button>
          </li>)}</ol> : <p className={styles.clearCopy}>No dead or terminally failed tasks are present in the evidence that loaded.</p>}
        </section>
        {selected && <EvidencePanel item={selected} close={() => setSelected(null)} />}
        <section className={styles.systems} aria-labelledby="systems-heading">
          <div className={styles.systemsHead}><h2 id="systems-heading">Routine systems</h2><span>Observed without becoming a dashboard</span></div>
          <div className={styles.systemGrid}>{systems.map((system) => {
            const count = data?.systems[system.key];
            return <a className={styles.systemCard} href={system.href} key={system.label}><b>{system.label}</b><span>{system.detail}</span><i>{count === null || count === undefined ? "Unknown" : count}</i></a>;
          })}</div>
        </section>
      </>}
  </section>;
}
