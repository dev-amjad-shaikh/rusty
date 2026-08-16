import { useQuery } from "@tanstack/react-query";
import { Link, useLocation } from "@tanstack/react-router";
import { useEffect, useRef, useState, type RefObject } from "react";
import { getOperationsSnapshot, type OperationAttentionItem } from "../../lib/api/client";
import { useConnectionStore } from "../../state/connection";
import { evidencePreview } from "../../lib/text";
import { ArtifactInspector } from "../work/artifacts/ArtifactInspector";
import { getRunArtifact } from "../../lib/api/artifacts";
import { PageHeader } from "../../components/PageHeader";
import styles from "./OperationsPage.module.css";

const systems = [
  { key: "tasks", label: "Task queue", detail: "Work that needs action" },
  { key: "automations", label: "Automations", detail: "Event-driven entry points" },
  { key: "schedules", label: "Schedules", detail: "Recurring execution" },
] as const;

const modes = [
  { to: "/operations" as const, label: "Attention" },
  { to: "/operations/releases" as const, label: "Releases" },
  { to: "/operations" as const, hash: "systems", label: "Systems" },
];

function observedTime(value: string) {
  const date = new Date(value);
  return Number.isNaN(date.valueOf()) ? "Observation time unavailable" : date.toLocaleString();
}

function ModeNav() {
  const { pathname, hash } = useLocation({ select: (location) => ({ pathname: location.pathname, hash: location.hash }) });
  return (
    <nav className={styles.modeNav} aria-label="Operations modes">
      {modes.map((mode) => (
        <Link
          key={mode.label}
          to={mode.to}
          hash={mode.hash}
          aria-current={pathname === mode.to && (mode.hash ? hash === `#${mode.hash}` : true) ? "page" : undefined}
        >
          {mode.label}
        </Link>
      ))}
    </nav>
  );
}

function EvidencePanel({ item, close, headingRef }: { item: OperationAttentionItem; close: () => void; headingRef: RefObject<HTMLHeadingElement | null> }) {
  const { connection } = useConnectionStore();
  const artifact = useQuery({
    queryKey: connection && item.source === "artifact" && item.artifactId ? [connection.epoch, connection.origin, connection.tenantFingerprint, "artifact", item.artifactId] : ["artifact", "idle"],
    queryFn: () => getRunArtifact(connection!, item.artifactId!),
    enabled: Boolean(connection && item.source === "artifact" && item.artifactId),
  });
  if (item.source === "artifact") {
    return <>
      <aside id="operation-evidence" className={styles.evidence} aria-labelledby="operation-evidence-heading">
        <div className={styles.evidenceHead}>
          <div><span className="eyebrow">Artifact exception</span><h2 ref={headingRef} tabIndex={-1} id="operation-evidence-heading">{item.title}</h2></div>
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
  return <aside id="operation-evidence" className={styles.evidence} aria-labelledby="operation-evidence-heading">
    <div className={styles.evidenceHead}>
      <div><span className="eyebrow">Task evidence</span><h2 ref={headingRef} tabIndex={-1} id="operation-evidence-heading">{item.title}</h2></div>
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
  const evidenceHeadingRef = useRef<HTMLHeadingElement>(null);
  const reviewTriggerRef = useRef<HTMLButtonElement | null>(null);
  const pageHeadingRef = useRef<HTMLHeadingElement>(null);
  const attentionHeadingRef = useRef<HTMLHeadingElement>(null);
  const restoreStableFocusRef = useRef(false);
  const evidenceOwnedFocusRef = useRef(false);
  useEffect(() => {
    const rememberFocusOwner = (event: FocusEvent) => {
      const target = event.target instanceof Element ? event.target : null;
      evidenceOwnedFocusRef.current = Boolean(document.getElementById("operation-evidence")?.contains(event.target as Node) || target?.closest("[data-artifact-inspector]"));
    };
    document.addEventListener("focusin", rememberFocusOwner);
    return () => document.removeEventListener("focusin", rememberFocusOwner);
  }, []);
  useEffect(() => { clearEvidenceToStableHeading(); }, [connection?.epoch, connection?.origin, connection?.tenantFingerprint]);
  useEffect(() => {
    if (selected) evidenceHeadingRef.current?.focus();
    else if (restoreStableFocusRef.current) {
      restoreStableFocusRef.current = false;
      (attentionHeadingRef.current ?? pageHeadingRef.current)?.focus();
    }
  }, [selected?.id]);
  useEffect(() => {
    if (window.location.hash === "#systems") document.getElementById("systems-heading")?.scrollIntoView({ behavior: "smooth" });
  }, []);
  function closeEvidence() {
    setSelected(null);
    requestAnimationFrame(() => reviewTriggerRef.current?.focus());
  }
  function clearEvidenceToStableHeading() {
    restoreStableFocusRef.current = evidenceOwnedFocusRef.current;
    evidenceOwnedFocusRef.current = false;
    setSelected(null);
  }
  const snapshot = useQuery({
    queryKey: connection ? [connection.epoch, connection.origin, connection.tenantFingerprint, "operations"] : ["operations", "disconnected"],
    queryFn: () => getOperationsSnapshot(connection!),
    enabled: Boolean(connection),
    refetchInterval: 15_000,
  });
  const data = snapshot.data;
  const taskStatusUnavailable = Boolean(data?.unavailable.includes("task queue"));
  useEffect(() => {
    if (!selected || !data) return;
    const current = data.attention.find((item) => item.id === selected.id) ?? null;
    if (current === selected) return;
    if (current) setSelected(current);
    else clearEvidenceToStableHeading();
  }, [data, selected]);

  return <section className={`page ${styles.operationsPage}`} aria-labelledby="operations-heading">
    <PageHeader headingId="operations-heading" headingRef={pageHeadingRef} eyebrow="Operate" title="Operations" description="Review failed work first. Schedules and automations stay quiet until they need attention." actions={!connection ? <button className="primary-button" type="button" onClick={openDialog}>Choose workspace</button> : <button className="secondary-button" type="button" onClick={() => snapshot.refetch()} disabled={snapshot.isFetching}>{snapshot.isFetching ? "Refreshing…" : "Refresh"}</button>} />
    <ModeNav />

    {!connection ? <div className="empty-state"><span className="eyebrow">Attention queue</span><h2>Open a workspace to review operations</h2><p>Failures and routine systems will remain clearly separated.</p></div>
      : snapshot.isLoading ? <div className={styles.loading}>Loading operational evidence…</div>
      : snapshot.isError ? <div className={styles.attention}><div><span className="eyebrow">Evidence unavailable</span><h2>Operations could not be loaded</h2><p>{snapshot.error instanceof Error ? snapshot.error.message : "Try again."}</p></div></div>
      : <>
        <section className={styles.attention} aria-labelledby="attention-heading">
          <header>
            <div><span className="eyebrow">Needs attention</span><h2 ref={attentionHeadingRef} tabIndex={-1} id="attention-heading">{data?.attention.length ? `${data.attention.length} item${data.attention.length === 1 ? "" : "s"}` : taskStatusUnavailable ? "Task status could not be verified" : "No task failures need action"}</h2></div>
            {data?.unavailable.length ? <span className={styles.unknown}>Unavailable: {data.unavailable.join(", ")}</span> : <span className={styles.observed}>All sources checked</span>}
          </header>
          {data?.attention.length ? <ol className={styles.attentionList}>{data.attention.map((item) => <li key={item.id}>
            <span className={styles.severity} aria-hidden="true">!</span>
            <div><b>{item.title}</b><p>{item.detail}</p><small>{observedTime(item.observedAt)}</small></div>
            <button type="button" aria-controls="operation-evidence" aria-expanded={selected?.id === item.id} onClick={(event) => { reviewTriggerRef.current = event.currentTarget; setSelected(item); }}>Review</button>
          </li>)}</ol> : <p className={styles.clearCopy}>{taskStatusUnavailable ? "Refresh to check for work that may need attention." : "There are no dead or terminally failed tasks."}</p>}
        </section>
        {selected && <EvidencePanel item={selected} close={closeEvidence} headingRef={evidenceHeadingRef} />}
        <section className={styles.systems} id="systems" aria-labelledby="systems-heading">
          <div className={styles.systemsHead}><h2 id="systems-heading">Routine systems</h2><span>Current inventory</span></div>
          <div className={styles.systemGrid}>{systems.map((system) => {
            const count = data?.systems[system.key];
            return <article className={styles.systemCard} key={system.label}><b>{system.label}</b><span>{system.detail}</span><i>{count === null || count === undefined ? "Unknown" : count}</i></article>;
          })}</div>
        </section>
      </>}
  </section>;
}
