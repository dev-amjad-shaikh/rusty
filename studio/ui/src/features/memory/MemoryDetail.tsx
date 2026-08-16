import { useEffect, useRef, useState, type FormEvent } from "react";
import { useQuery } from "@tanstack/react-query";
import { connectionScope, StudioApiError } from "../../lib/api/client";
import { forgetMemory, getMemory, type ForgetReason, type ForgetReceipt, type MemoryConflict, type MemoryRecord } from "../../lib/api/memory";
import { useConnectionStore } from "../../state/connection";
import {
  contentText,
  evidenceSummary,
  formatInstant,
  lifecycleLabels,
  recordAuthorText,
  recordScopeText,
  recordStates,
  recordTitle,
  shortAddress,
  supersededIds,
} from "./memoryModel";
import styles from "./MemoryPage.module.css";

const forgetReasons: { value: ForgetReason; label: string; hint: string }[] = [
  { value: "retracted", label: "Retracted", hint: "The record was wrong, or its correction made it unnecessary." },
  { value: "erasure_request", label: "Erasure request", hint: "A user's or operator's compliance erasure." },
  { value: "expired", label: "Expired", hint: "The record's TTL lapsed; this is the operator-driven sweep." },
];

export function MemoryDetail({
  memoryId,
  records,
  conflicts,
  onSelect,
  onCorrect,
  onForgotten,
  onClose,
}: {
  memoryId: string;
  records: MemoryRecord[];
  conflicts: MemoryConflict[];
  onSelect: (id: string) => void;
  onCorrect: (id: string) => void;
  onForgotten: () => void;
  onClose: () => void;
}) {
  const { connection } = useConnectionStore();
  const headingRef = useRef<HTMLHeadingElement>(null);
  const fromResults = records.find((record) => record.memory_id === memoryId) ?? null;
  const fetched = useQuery({
    queryKey: connection && !fromResults ? [connection.epoch, connection.origin, connection.tenantFingerprint, "memory-record", memoryId] : ["memory-record", "idle"],
    queryFn: () => getMemory(connection!, memoryId),
    enabled: Boolean(connection && !fromResults),
  });
  const record = fromResults ?? fetched.data ?? null;

  const chain = useQuery({
    queryKey: connection && record?.supersedes ? [connection.epoch, connection.origin, connection.tenantFingerprint, "memory-chain", record.memory_id] : ["memory-chain", "idle"],
    queryFn: async () => {
      const ancestors: MemoryRecord[] = [];
      let cursor = record!.supersedes!;
      for (let depth = 0; depth < 8; depth += 1) {
        const ancestor = await getMemory(connection!, cursor);
        ancestors.push(ancestor);
        if (!ancestor.supersedes) break;
        cursor = ancestor.supersedes;
      }
      return ancestors;
    },
    enabled: Boolean(connection && record?.supersedes),
  });

  useEffect(() => { headingRef.current?.focus(); }, [memoryId]);

  if (fetched.isLoading && !record) return <div className={styles.detail} role="status">Loading the exact record…</div>;
  if (!record) {
    return (
      <div className={styles.detail} role="alert">
        <div className={styles.detailHead}>
          <div><span className="eyebrow">Record unavailable</span><h2 ref={headingRef} tabIndex={-1}>This record could not be proven</h2></div>
          <button className="secondary-button" type="button" onClick={onClose}>Close</button>
        </div>
        <p>{fetched.error instanceof Error ? fetched.error.message : "Unknown or forgotten records are indistinguishable by design."}</p>
      </div>
    );
  }

  const superseded = supersededIds([...records, record, ...(chain.data ?? [])]);
  const states = recordStates(record, superseded, new Date());
  const successors = records.filter((candidate) => candidate.supersedes === record.memory_id
    || (candidate.kind === "summary" && candidate.provenance.evidence.source_memory_ids.includes(record.memory_id)));
  const recordConflicts = conflicts.filter((conflict) => conflict.memory_ids.includes(record.memory_id));
  const conflicted = recordConflicts.length > 0;
  const peerIds = Array.from(new Set(recordConflicts.flatMap((conflict) => conflict.memory_ids))).filter((id) => id !== record.memory_id);
  const peers = peerIds.map((id) => {
    const known = records.find((peer) => peer.memory_id === id);
    return { id, label: known ? recordTitle(known) : "Peer record", author: known ? recordAuthorText(known) : "" };
  });
  const ancestors = chain.data ?? [];

  return (
    <article className={styles.detail} aria-labelledby="memory-detail-heading">
      <div className={styles.detailHead}>
        <div>
          <span className="eyebrow">Immutable record</span>
          <h2 id="memory-detail-heading" ref={headingRef} tabIndex={-1}>{recordTitle(record)}</h2>
          <small>Learned {formatInstant(record.created_at)} · content address {shortAddress(record.memory_id)}</small>
        </div>
        <button className="secondary-button" type="button" onClick={onClose}>Close</button>
      </div>
      <div className={styles.badges}>
        <span className={styles.badge} data-kind={record.kind}>{record.kind}</span>
        {states.map((state) => <span key={state} className={styles.badge} data-state={state}>{lifecycleLabels[state]}</span>)}
        {conflicted && <span className={styles.badge} data-state="conflict">Conflict</span>}
      </div>

      <div className={styles.contentBlock}>
        <span>Remembered content</span>
        <pre>{contentText(record)}</pre>
      </div>

      <ol className={styles.spine} aria-label="Provenance spine">
        <li><b>Record</b><span>{record.memory_id}</span></li>
        <li><b>Authored by</b><span>{recordAuthorText(record)} · written {formatInstant(record.provenance.written_at)}</span></li>
        <li><b>Evidence</b><span>{evidenceSummary(record)}</span></li>
        <li><b>Claimed true</b><span>{formatInstant(record.validity.valid_from)} → {record.validity.valid_until ? formatInstant(record.validity.valid_until) : "open-ended"}</span></li>
      </ol>

      <dl className={styles.kv}>
        <div><dt>Scope</dt><dd>{recordScopeText(record)}</dd></div>
        <div><dt>Confidence</dt><dd>{record.confidence} (writer-declared)</dd></div>
        <div><dt>Key</dt><dd>{record.key ?? "—"}</dd></div>
        <div><dt>Priority</dt><dd>{record.priority}</dd></div>
        <div><dt>Tags</dt><dd>{record.tags.length ? record.tags.join(", ") : "—"}</dd></div>
        <div><dt>Expires</dt><dd>{record.expires_at ? formatInstant(record.expires_at) : "does not expire"}</dd></div>
      </dl>

      {(record.supersedes || successors.length > 0) && (
        <div className={styles.chain}>
          <h3>Supersession chain</h3>
          <ol>
            {ancestors.slice().reverse().map((ancestor) => (
              <li key={ancestor.memory_id}>
                <span className={styles.chainMark}>old</span>
                <div>
                  <button type="button" onClick={() => onSelect(ancestor.memory_id)}>{shortAddress(ancestor.memory_id)}</button>
                  <small> {recordTitle(ancestor)} · {formatInstant(ancestor.created_at)}</small>
                </div>
              </li>
            ))}
            {record.supersedes && chain.isLoading && <li><span className={styles.chainMark}>…</span><div><small>Loading replaced records…</small></div></li>}
            <li data-current="true">
              <span className={styles.chainMark}>this</span>
              <div><code>{shortAddress(record.memory_id)}</code><small> {recordTitle(record)}</small></div>
            </li>
            {successors.map((successor) => (
              <li key={successor.memory_id}>
                <span className={styles.chainMark}>{successor.kind === "summary" && !successor.supersedes ? "distilled" : "new"}</span>
                <div>
                  <button type="button" onClick={() => onSelect(successor.memory_id)}>{shortAddress(successor.memory_id)}</button>
                  <small> {recordTitle(successor)} · {formatInstant(successor.created_at)}</small>
                </div>
              </li>
            ))}
          </ol>
        </div>
      )}

      {conflicted && (
        <div className={styles.chain}>
          <h3>Conflicting live memory</h3>
          <p className={styles.panelNote}>The same key has different content over an overlapping validity window. Rusty flags this — it never silently picks a winner. Resolution is a correction you write, not a choice the runtime makes.</p>
          <ol>
            <li data-current="true"><span className={styles.chainMark}>this</span><div><code>{shortAddress(record.memory_id)}</code><small> {recordTitle(record)}</small></div></li>
            {peers.map((peer) => (
              <li key={peer.id}>
                <span className={styles.chainMark}>peer</span>
                <div>
                  <button type="button" onClick={() => onSelect(peer.id)}>{shortAddress(peer.id)}</button>
                  <small> {peer.label}{peer.author ? ` · ${peer.author}` : ""}</small>
                </div>
              </li>
            ))}
          </ol>
        </div>
      )}

      <div className={styles.detailActions}>
        <button className="secondary-button" type="button" onClick={() => onCorrect(record.memory_id)}>Correct this memory</button>
      </div>

      <ForgetPanel record={record} onForgotten={onForgotten} />
    </article>
  );
}

function ForgetPanel({ record, onForgotten }: { record: MemoryRecord; onForgotten: () => void }) {
  const { connection } = useConnectionStore();
  const [armed, setArmed] = useState(false);
  const [reason, setReason] = useState<ForgetReason>("retracted");
  const [confirmation, setConfirmation] = useState("");
  const [pending, setPending] = useState(false);
  const [error, setError] = useState("");
  const [receipt, setReceipt] = useState<ForgetReceipt | null>(null);
  const exactMatch = confirmation.trim() === record.memory_id;

  useEffect(() => {
    setArmed(false);
    setConfirmation("");
    setError("");
    setReceipt(null);
  }, [record.memory_id, connection?.epoch, connection?.origin, connection?.tenantFingerprint]);

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (!connection || !exactMatch || pending) return;
    const scopeAtStart = connectionScope(connection);
    setPending(true);
    setError("");
    try {
      const result = await forgetMemory(connection, record.memory_id, reason);
      const current = useConnectionStore.getState().connection;
      if (!current || connectionScope(current) !== scopeAtStart) return;
      setReceipt(result);
    } catch (caught) {
      const current = useConnectionStore.getState().connection;
      if (!current || connectionScope(current) !== scopeAtStart) return;
      setError(caught instanceof StudioApiError ? caught.message : "The record could not be forgotten.");
    } finally {
      const current = useConnectionStore.getState().connection;
      if (current && connectionScope(current) === scopeAtStart) setPending(false);
    }
  }

  if (receipt) {
    return (
      <div className={styles.receipt} role="status">
        <h3>Forgotten — this cannot be undone</h3>
        <p>
          The record was deleted from the store.
          {receipt.invalidated.length ? ` ${receipt.invalidated.length} dependent summar${receipt.invalidated.length === 1 ? "y was" : "ies were"} invalidated and deleted with it.` : " No dependent summaries were invalidated."}
          {" "}A metadata-only tombstone — id, scope, reason, never the content — is the receipt.
        </p>
        <code>{receipt.tombstone.memory_id}</code>
        <div className={styles.receiptActions}>
          <button className="secondary-button" type="button" onClick={onForgotten}>Back to the ledger</button>
        </div>
      </div>
    );
  }

  return (
    <section className={styles.dangerPanel} aria-labelledby="memory-forget-heading">
      <h3 id="memory-forget-heading">Forget this memory</h3>
      {!armed ? (
        <>
          <p>
            Forgetting deletes this record from the store, and every summary built on it is invalidated and deleted with it.
            Journals keep their hash-chained evidence — the deletion removes the record, not the history of its existence.
            This cannot be undone.
          </p>
          <div className={styles.panelActions}>
            <button className="dangerButton" type="button" onClick={() => setArmed(true)}>I understand — continue</button>
          </div>
        </>
      ) : (
        <form onSubmit={submit} noValidate>
          <label>
            Why is it forgotten
            <select value={reason} onChange={(event) => setReason(event.target.value as ForgetReason)}>
              {forgetReasons.map((item) => <option key={item.value} value={item.value}>{item.label}</option>)}
            </select>
            <span className={styles.fieldHint}>{forgetReasons.find((item) => item.value === reason)?.hint} Carried on the tombstone.</span>
          </label>
          <label>
            Type the full memory id to confirm
            <code>{record.memory_id}</code>
            <input value={confirmation} onChange={(event) => setConfirmation(event.target.value)} placeholder="The exact 64-character content address" autoComplete="off" spellCheck={false} />
            <span className={styles.fieldHint}>The button stays locked until the id matches exactly. There is no undo and no recovery copy.</span>
          </label>
          {error && <p className={styles.error} role="alert">{error}</p>}
          <div className={styles.panelActions}>
            <button className="secondary-button" type="button" onClick={() => { setArmed(false); setConfirmation(""); setError(""); }}>Cancel</button>
            <button className="dangerButton" type="submit" disabled={!exactMatch || pending}>{pending ? "Forgetting…" : "Forget permanently"}</button>
          </div>
        </form>
      )}
    </section>
  );
}
