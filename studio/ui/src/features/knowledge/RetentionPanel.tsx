import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  applyKnowledgeRetention,
  listKnowledgeSources,
  planKnowledgeRetention,
  type KnowledgeRetentionPlan,
  type KnowledgeRetentionReceipt,
} from "../../lib/api/knowledge";
import { formatBytes, formatInstant, hashPreview } from "./format";
import styles from "./KnowledgePage.module.css";

export function RetentionPanel() {
  const queryClient = useQueryClient();
  const [asOf, setAsOf] = useState("");
  const [plan, setPlan] = useState<KnowledgeRetentionPlan | null>(null);
  const [confirming, setConfirming] = useState(false);
  const [receipt, setReceipt] = useState<KnowledgeRetentionReceipt | null>(null);

  const library = useQuery({
    queryKey: ["knowledge", "sources"],
    queryFn: () => listKnowledgeSources(),
  });
  const tombstones = library.data?.tombstones ?? [];

  const sweepInstant = asOf ? new Date(asOf) : null;
  const asOfIso = sweepInstant && !Number.isNaN(sweepInstant.getTime()) ? sweepInstant.toISOString() : undefined;

  const planMutation = useMutation({
    mutationFn: () => planKnowledgeRetention(asOfIso),
    onSuccess: (result) => {
      setPlan(result);
      setConfirming(false);
      setReceipt(null);
    },
  });
  const applyMutation = useMutation({
    mutationFn: () => applyKnowledgeRetention(asOfIso),
    onSuccess: (result) => {
      setReceipt(result);
      setPlan(null);
      setConfirming(false);
      queryClient.invalidateQueries({ queryKey: ["knowledge"] });
    },
  });

  const applyReady = plan !== null && plan.entries.length > 0 && !applyMutation.isPending;

  return (
    <div className={styles.detailStack}>
      <section className={styles.panel} aria-label="Retention sweep">
        <h2>Retention sweep</h2>
        <p className={styles.panelLead}>
          Plan first: the dry-run shows exactly what a sweep would purge, before anything is deleted. Purged bodies and chunks leave metadata-only tombstones, so citations in old runs stay resolvable.
        </p>
        <div className={styles.limitsRow}>
          <label>
            Evaluate as of
            <input
              type="datetime-local"
              value={asOf}
              onChange={(event) => { setAsOf(event.target.value); setPlan(null); setConfirming(false); }}
            />
          </label>
          <span className={styles.fieldHint}>Default: now. An explicit instant is the operator-declared sweep clock.</span>
          <button
            className="secondary-button"
            type="button"
            onClick={() => planMutation.mutate()}
            disabled={planMutation.isPending}
          >
            {planMutation.isPending ? "Planning…" : "Plan sweep"}
          </button>
        </div>

        {planMutation.isError && (
          <p className={styles.error} role="alert" style={{ marginTop: 14 }}>
            {planMutation.error instanceof Error ? planMutation.error.message : "The plan could not be computed."}
          </p>
        )}

        {plan && (
          <div style={{ marginTop: 16 }}>
            {plan.entries.length === 0 ? (
              <p className={styles.sectionNote} role="status">
                Nothing would purge — every source is pinned or still within its TTL.
              </p>
            ) : (
              <>
                <div className={styles.chunkTable} role="table" aria-label="Retention plan">
                  <div className={styles.chunkHead} role="row">
                    <span role="columnheader">Source</span>
                    <span role="columnheader">Title</span>
                    <span role="columnheader">Expired at</span>
                    <span role="columnheader">Chunks</span>
                    <span role="columnheader">Bytes</span>
                    <span role="columnheader">Reason</span>
                  </div>
                  {plan.entries.map((entry) => (
                    <div className={styles.chunkRow} role="row" key={`${entry.source_id}-${entry.source_hash}`}>
                      <span role="cell">{entry.source_id} <small>v{entry.version}</small></span>
                      <span role="cell">{entry.title}</span>
                      <span role="cell">{formatInstant(entry.expires_at)}</span>
                      <span role="cell">{entry.chunk_count}</span>
                      <span role="cell">{formatBytes(entry.chunk_bytes)}</span>
                      <span role="cell">TTL expired</span>
                    </div>
                  ))}
                </div>
                <p className={styles.sectionNote} style={{ marginTop: 10 }}>
                  {plan.entries.length} version{plan.entries.length === 1 ? "" : "s"} would purge · {formatBytes(plan.total_chunk_bytes)} of chunk bytes
                </p>
                {!confirming && !receipt && (
                  <div className={styles.formActions}>
                    <button className="primary-button" type="button" onClick={() => setConfirming(true)} disabled={!applyReady}>
                      Apply sweep
                    </button>
                  </div>
                )}
                {confirming && (
                  <div className={styles.confirmBox} role="alertdialog" aria-label="Confirm retention sweep">
                    <h3>Purge {plan.entries.length} version{plan.entries.length === 1 ? "" : "s"}?</h3>
                    <p>
                      Bodies, chunks, and source records are removed for good. Each purged source leaves a metadata-only tombstone so old citations still resolve. This cannot be undone.
                    </p>
                    {applyMutation.isError && (
                      <p className={styles.error} role="alert">
                        {applyMutation.error instanceof Error ? applyMutation.error.message : "The sweep failed."}
                      </p>
                    )}
                    <div className={styles.formActions}>
                      <button className="secondary-button" type="button" onClick={() => setConfirming(false)} disabled={applyMutation.isPending}>
                        Keep everything
                      </button>
                      <button
                        className="primary-button"
                        type="button"
                        onClick={() => applyMutation.mutate()}
                        disabled={applyMutation.isPending}
                      >
                        {applyMutation.isPending ? "Purging…" : `Confirm purge of ${plan.entries.length} version${plan.entries.length === 1 ? "" : "s"}`}
                      </button>
                    </div>
                  </div>
                )}
              </>
            )}
          </div>
        )}

        {receipt && (
          <div className={styles.receipt} role="status" style={{ marginTop: 16 }}>
            <h3>Sweep applied</h3>
            <p>
              {receipt.plan.entries.length} version{receipt.plan.entries.length === 1 ? "" : "s"} purged · {formatBytes(receipt.plan.total_chunk_bytes)} removed · {receipt.tombstones.length} tombstone{receipt.tombstones.length === 1 ? "" : "s"} written.
            </p>
            {receipt.tombstones.length > 0 && (
              <dl>
                {receipt.tombstones.map((tombstone) => (
                  <div key={tombstone.source_id}>
                    <dt>{tombstone.source_id}</dt>
                    <dd>{tombstone.title} · purged {formatInstant(tombstone.purged_at)}</dd>
                  </div>
                ))}
              </dl>
            )}
          </div>
        )}
      </section>

      <section className={styles.panel} aria-label="Purged sources">
        <h2 className={styles.sectionTitle}>Tombstones</h2>
        <p className={styles.sectionNote}>
          Metadata-only purge receipts. The bytes are gone; the record is what a citation in an old journal resolves to.
        </p>
        {library.isLoading ? (
          <p className={styles.sectionNote} role="status">Loading tombstones…</p>
        ) : tombstones.length === 0 ? (
          <p className={styles.sectionNote}>Nothing has been purged.</p>
        ) : (
          <ul className={styles.tombstoneList}>
            {tombstones.map((tombstone) => (
              <li className={styles.tombstoneItem} key={tombstone.source_id}>
                <b>{tombstone.title}</b>
                <code>{tombstone.source_id}</code>
                <span>{tombstone.purged_hashes.map(hashPreview).join(", ")} · {formatInstant(tombstone.purged_at)}</span>
              </li>
            ))}
          </ul>
        )}
      </section>
    </div>
  );
}
