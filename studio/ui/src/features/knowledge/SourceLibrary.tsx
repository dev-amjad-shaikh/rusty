import { useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import type { ConnectionIdentity } from "../../lib/api/client";
import { listKnowledgeSources, type ListedKnowledgeSource, type KnowledgeSourceKind } from "../../lib/api/knowledge";
import { evidencePreview } from "../../lib/text";
import { formatInstant, hashPreview, retentionState } from "./format";
import styles from "./KnowledgePage.module.css";

const kindFilters: Array<{ value: "" | KnowledgeSourceKind; label: string }> = [
  { value: "", label: "All kinds" },
  { value: "text", label: "Text" },
  { value: "markdown", label: "Markdown" },
  { value: "json", label: "JSON" },
  { value: "csv", label: "CSV" },
];

export function SourceLibrary({
  connection,
  onOpenSource,
  onRegister,
}: {
  connection: ConnectionIdentity;
  onOpenSource: (sourceId: string) => void;
  onRegister: () => void;
}) {
  const [filter, setFilter] = useState("");
  const [kind, setKind] = useState<"" | KnowledgeSourceKind>("");
  const library = useQuery({
    queryKey: [connection.epoch, connection.origin, connection.tenantFingerprint, "knowledge", "sources"],
    queryFn: () => listKnowledgeSources(connection),
  });

  const sources = useMemo(
    () => [...(library.data?.sources ?? [])].sort((a, b) => a.source_id.localeCompare(b.source_id)),
    [library.data],
  );
  const visible = useMemo(() => {
    const needle = filter.trim().toLowerCase();
    return sources.filter((source) =>
      (!kind || source.kind === kind)
      && (!needle
        || source.title.toLowerCase().includes(needle)
        || source.source_id.toLowerCase().includes(needle)
        || source.author.toLowerCase().includes(needle)));
  }, [sources, filter, kind]);

  if (library.isLoading) return <div className={styles.loading} role="status">Loading sources…</div>;
  if (library.isError) {
    return (
      <div className={styles.emptyState} role="alert">
        <span className={styles.emptyMark} aria-hidden="true">!</span>
        <div>
          <h2>Sources could not be loaded</h2>
          <p>{library.error instanceof Error ? library.error.message : "Try the request again."}</p>
        </div>
        <div><button className="primary-button" type="button" onClick={() => library.refetch()}>Retry</button></div>
      </div>
    );
  }

  const tombstones = library.data?.tombstones ?? [];

  return (
    <>
      {sources.length === 0 ? (
        <div className={styles.emptyState}>
          <span className={styles.emptyMark} aria-hidden="true">K</span>
          <div>
            <h2>No sources yet</h2>
            <p>Register the first governed source. It gets a content address, a chunk inventory, and a retention policy — retrieval always cites it exactly.</p>
          </div>
          <div><button className="primary-button" type="button" onClick={onRegister}>Register first source</button></div>
        </div>
      ) : (
        <>
          <div className={styles.filterRow}>
            <label>
              Filter
              <input
                type="search"
                value={filter}
                placeholder="Title, source id, or author"
                onChange={(event) => setFilter(event.target.value)}
              />
            </label>
            <label>
              Kind
              <select value={kind} onChange={(event) => setKind(event.target.value as "" | KnowledgeSourceKind)}>
                {kindFilters.map(({ value, label }) => <option key={value} value={value}>{label}</option>)}
              </select>
            </label>
            <span className={styles.filterSummary}>{visible.length} of {sources.length} shown</span>
          </div>
          {visible.length === 0 ? (
            <div className={styles.emptyState}>
              <span className={styles.emptyMark} aria-hidden="true">K</span>
              <div>
                <h2>Nothing matches</h2>
                <p>No source matches this filter. Clear it to see the whole library.</p>
              </div>
              <div><button className="secondary-button" type="button" onClick={() => { setFilter(""); setKind(""); }}>Clear filter</button></div>
            </div>
          ) : (
            <div className={styles.sourceTable} role="table" aria-label="Knowledge sources">
              <div className={styles.tableHead} role="row">
                <span role="columnheader">Source</span>
                <span role="columnheader">Kind</span>
                <span role="columnheader">Author</span>
                <span role="columnheader">Confidence</span>
                <span role="columnheader">Version</span>
                <span role="columnheader">Retention</span>
                <span role="columnheader">Chunks</span>
                <span role="columnheader"><span className="sr-only">Open</span></span>
              </div>
              {visible.map((source) => <SourceRow key={source.source_id} source={source} onOpen={() => onOpenSource(source.source_id)} />)}
              <footer>{visible.length} source{visible.length === 1 ? "" : "s"}</footer>
            </div>
          )}
        </>
      )}

      {tombstones.length > 0 && (
        <section className={styles.tombstones} aria-labelledby="tombstones-heading">
          <h2 id="tombstones-heading">Purged sources</h2>
          <p className={styles.tombstoneNote}>
            Tombstones are metadata only — bodies and chunks are gone. Citations in old runs still resolve to these records.
          </p>
          <ul className={styles.tombstoneList}>
            {tombstones.map((tombstone) => (
              <li className={styles.tombstoneItem} key={tombstone.source_id}>
                <b>{evidencePreview(tombstone.title, 256)}</b>
                <code>{tombstone.source_id}</code>
                <span>{tombstone.purged_hashes.length} version{tombstone.purged_hashes.length === 1 ? "" : "s"} purged · {formatInstant(tombstone.purged_at)}</span>
                <button type="button" className={styles.backButton} onClick={() => onOpenSource(tombstone.source_id)}>
                  View record
                </button>
              </li>
            ))}
          </ul>
        </section>
      )}
    </>
  );
}

function SourceRow({ source, onOpen }: { source: ListedKnowledgeSource; onOpen: () => void }) {
  const retention = retentionState(source.retention);
  const retentionClass = retention.tone === "pinned" ? styles.chipPinned : retention.tone === "live" ? styles.chipLive : styles.chipExpired;
  return (
    <article className={styles.sourceRow} role="row">
      <div className={styles.sourceIdentity} role="cell">
        <button type="button" onClick={onOpen}>{evidencePreview(source.title, 256)}</button>
        <small>{source.source_id} · {hashPreview(source.content_hash)}</small>
      </div>
      <div role="cell" data-label="Kind"><span className={`${styles.chip} ${styles.chipKind}`}>{source.kind}</span></div>
      <span role="cell" data-label="Author" title={source.author}>{evidencePreview(source.author, 128)}</span>
      <span role="cell" data-label="Confidence">{source.confidence.toFixed(2)}</span>
      <span role="cell" data-label="Version">v{source.version}</span>
      <div role="cell" data-label="Retention"><span className={`${styles.chip} ${retentionClass}`}>{retention.label}</span></div>
      <span role="cell" data-label="Chunks">{source.chunk_count}</span>
      <div className={styles.openCell} role="cell">
        <button className={styles.openSource} type="button" onClick={onOpen} aria-label={`Open ${evidencePreview(source.title, 256)}`}>
          Open <span aria-hidden="true">→</span>
        </button>
      </div>
    </article>
  );
}
