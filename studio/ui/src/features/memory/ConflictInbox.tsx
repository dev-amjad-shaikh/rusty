import type { UseQueryResult } from "@tanstack/react-query";
import type { MemoryConflict } from "../../lib/api/memory";
import { scopeAddressText } from "../../lib/api/memory";
import { formatInstant, shortAddress } from "./memoryModel";
import styles from "./MemoryPage.module.css";

export function ConflictInbox({
  conflicts,
  onInspect,
}: {
  conflicts: UseQueryResult<MemoryConflict[]>;
  onInspect: (memoryId: string) => void;
}) {
  if (conflicts.isLoading) return <div className={styles.loading} role="status">Checking for conflicting live memory…</div>;
  if (conflicts.isError) {
    return (
      <div className="empty-state" role="alert">
        <span className="eyebrow">Conflict state unknown</span>
        <h2>Conflicts could not be checked</h2>
        <p>{conflicts.error instanceof Error ? conflicts.error.message : "Try the request again."} Missing conflict evidence is never presented as an all-clear.</p>
        <button className="secondary-button" type="button" onClick={() => conflicts.refetch()}>Retry</button>
      </div>
    );
  }
  const items = conflicts.data ?? [];
  if (!items.length) {
    return (
      <div className="empty-state">
        <span className="eyebrow">Conflict inbox</span>
        <h2>No conflicting live memory</h2>
        <p>No two live records share a key with different content over an overlapping validity window. Detection re-runs each time this view loads.</p>
      </div>
    );
  }
  return (
    <section className={styles.conflicts} aria-labelledby="memory-conflicts-heading">
      <div className={styles.conflictsHead}>
        <div>
          <span className="eyebrow">Conflict inbox</span>
          <h2 id="memory-conflicts-heading">{items.length} conflict{items.length === 1 ? "" : "s"} {items.length === 1 ? "needs" : "need"} a human decision</h2>
        </div>
        <button className="secondary-button" type="button" onClick={() => conflicts.refetch()} disabled={conflicts.isFetching}>{conflicts.isFetching ? "Checking…" : "Recheck"}</button>
      </div>
      <p>
        Live records that share a key but assert different content over an overlapping validity window. Detection is
        evidence; resolution is governance — Rusty surfaces both sides and never silently picks a winner. Write a
        correction to supersede the side that is wrong.
      </p>
      <div className={styles.conflictGrid}>
        {items.map((conflict) => (
          <article className={styles.conflictCard} key={`${scopeAddressText(conflict.scope)}:${conflict.key}:${conflict.memory_ids.join(":")}`}>
            <b>{conflict.key}</b>
            <small>{scopeAddressText(conflict.scope)}</small>
            <small>
              Both claim truth {formatInstant(conflict.overlap.valid_from)} → {conflict.overlap.valid_until ? formatInstant(conflict.overlap.valid_until) : "open-ended"}
            </small>
            <div className={styles.conflictPeers}>
              {conflict.memory_ids.map((id) => (
                <button key={id} type="button" onClick={() => onInspect(id)} aria-label={`Inspect conflicting record ${shortAddress(id)}`}>
                  {shortAddress(id)}
                </button>
              ))}
            </div>
          </article>
        ))}
      </div>
    </section>
  );
}
