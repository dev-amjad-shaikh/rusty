import { type FormEvent, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  correctKnowledgeSource,
  getKnowledgeChunk,
  getKnowledgeSource,
  KNOWLEDGE_MAX_SOURCE_BYTES,
  type KnowledgeChunkRecord,
  type KnowledgeCorrectionReceipt,
  type KnowledgeSource,
  type KnowledgeSourceTombstone,
} from "../../lib/api/knowledge";
import { evidencePreview } from "../../lib/text";
import { bodyByteSize, formatBytes, formatInstant, hashPreview, retentionState } from "./format";
import styles from "./KnowledgePage.module.css";

export function SourceDetail({
  sourceId,
  onBack,
}: {
  sourceId: string;
  onBack: () => void;
}) {
  const detail = useQuery({
    queryKey: ["knowledge", "source", sourceId],
    queryFn: () => getKnowledgeSource(sourceId),
  });

  if (detail.isLoading) return <div className={styles.loading} role="status">Loading source…</div>;
  if (detail.isError) {
    return (
      <div className={styles.emptyState} role="alert">
        <span className={styles.emptyMark} aria-hidden="true">!</span>
        <div>
          <h2>Source could not be loaded</h2>
          <p>{detail.error instanceof Error ? detail.error.message : "Try the request again."}</p>
        </div>
        <div>
          <button className="secondary-button" type="button" onClick={onBack}>Back</button>
          <button className="primary-button" type="button" onClick={() => detail.refetch()}>Retry</button>
        </div>
      </div>
    );
  }
  if (!detail.data) return null;
  if ("tombstone" in detail.data) return <TombstoneView tombstone={detail.data.tombstone} />;
  return <LiveSource sourceId={sourceId} detail={detail.data} />;
}

function TombstoneView({ tombstone }: { tombstone: KnowledgeSourceTombstone }) {
  return (
    <div className={styles.panel}>
      <h2>{evidencePreview(tombstone.title, 256)}</h2>
      <p className={styles.panelLead}>
        This source was purged. What remains is metadata only — citations in old runs resolve to this record, not to content.
      </p>
      <dl className={styles.metaGrid}>
        <div><dt>Source id</dt><dd>{tombstone.source_id}</dd></div>
        <div><dt>Scope</dt><dd><code>{tombstone.scope.scope}:{tombstone.scope.id}</code></dd></div>
        <div><dt>Reason</dt><dd>TTL expired</dd></div>
        <div><dt>Purged at</dt><dd>{formatInstant(tombstone.purged_at)}</dd></div>
        <div><dt>Purged versions</dt><dd>{tombstone.purged_hashes.length}</dd></div>
        <div><dt>Purged hashes</dt><dd><code>{tombstone.purged_hashes.map(hashPreview).join(", ")}</code></dd></div>
      </dl>
    </div>
  );
}

function LiveSource({
  sourceId,
  detail,
}: {
  sourceId: string;
  detail: { source: KnowledgeSource; versions: number; chunks: KnowledgeChunkRecord[] };
}) {
  const queryClient = useQueryClient();
  const { source, versions, chunks } = detail;
  const [openChunk, setOpenChunk] = useState<number | null>(null);
  const [versionPin, setVersionPin] = useState<string>(source.content_hash);
  const [correcting, setCorrecting] = useState(false);
  const retention = retentionState(source.retention);
  const retentionClass = retention.tone === "pinned" ? styles.chipPinned : retention.tone === "live" ? styles.chipLive : styles.chipExpired;

  const chunk = useQuery({
    queryKey: ["knowledge", "chunk", sourceId, openChunk, versionPin],
    queryFn: () => getKnowledgeChunk(sourceId, openChunk!, versionPin === source.content_hash ? undefined : versionPin),
    enabled: openChunk !== null,
  });

  const knownHashes = [source.content_hash, ...(source.supersedes ? [source.supersedes] : [])];

  return (
    <div className={styles.detailStack}>
      <section className={styles.panel} aria-label="Source metadata">
        <h2>{evidencePreview(source.title, 512)}</h2>
        <p className={styles.panelLead}>
          <code>{source.source_id}</code> · version {source.version} of {versions}
        </p>
        <dl className={styles.metaGrid}>
          <div><dt>Kind</dt><dd>{source.kind}</dd></div>
          <div><dt>Author</dt><dd>{evidencePreview(source.author, 512)}</dd></div>
          <div><dt>Confidence</dt><dd>{source.confidence.toFixed(2)}</dd></div>
          <div><dt>Registered</dt><dd>{formatInstant(source.created_at)}</dd></div>
          <div><dt>Retention</dt><dd><span className={`${styles.chip} ${retentionClass}`}>{retention.label}</span></dd></div>
          <div><dt>Scope</dt><dd><code>{source.scope.scope}:{source.scope.id}</code></dd></div>
          <div><dt>Content hash</dt><dd><code>{source.content_hash}</code></dd></div>
          <div><dt>Body size</dt><dd>{formatBytes(source.content_bytes)}</dd></div>
        </dl>
      </section>

      <section className={styles.panel} aria-label="Version chain">
        <h2 className={styles.sectionTitle}>Version chain</h2>
        <p className={styles.sectionNote}>
          A correction mints a new content hash. Superseded versions stop answering retrieval but stay addressable as evidence.
        </p>
        <ul className={styles.chainList}>
          <li className={styles.chainItem} data-current="true">
            <span className={`${styles.chip} ${styles.chipCurrent}`}>current</span>
            <b>v{source.version}</b>
            <code>{source.content_hash}</code>
            <small>{formatInstant(source.created_at)}</small>
          </li>
          {source.supersedes && (
            <li className={styles.chainItem} data-current="false">
              <span className={`${styles.chip} ${styles.chipSuperseded}`}>superseded</span>
              <b>v{source.version - 1}</b>
              <code>{source.supersedes}</code>
              <small>addressable as evidence</small>
            </li>
          )}
          {versions > 2 && (
            <li className={styles.chainItem} data-current="false">
              <small>{versions - 2} earlier version{versions - 2 === 1 ? "" : "s"} retained in the correction chain.</small>
            </li>
          )}
        </ul>
        {!correcting && (
          <div className={styles.formActions}>
            <button className="secondary-button" type="button" onClick={() => setCorrecting(true)}>Correct this source</button>
          </div>
        )}
        {correcting && (
          <CorrectionForm
           
            sourceId={sourceId}
            defaultAuthor={source.author}
            onCorrected={() => {
              setCorrecting(false);
              setOpenChunk(null);
              setVersionPin("");
              queryClient.invalidateQueries({ queryKey: ["knowledge"] });
            }}
            onCancel={() => setCorrecting(false)}
          />
        )}
      </section>

      <section className={styles.panel} aria-label="Chunk inventory">
        <h2 className={styles.sectionTitle}>Chunks</h2>
        <p className={styles.sectionNote}>{chunks.length} chunk{chunks.length === 1 ? "" : "s"} of the current version. Open one to read its exact bytes and citation.</p>
        {chunks.length === 0 ? (
          <p className={styles.sectionNote}>No chunk records were returned for this version.</p>
        ) : (
          <div className={styles.chunkTable} role="table" aria-label="Chunk inventory">
            <div className={styles.chunkHead} role="row">
              <span role="columnheader">#</span>
              <span role="columnheader">Chunk id</span>
              <span role="columnheader">Byte range</span>
              <span role="columnheader">Bytes</span>
              <span role="columnheader">Words</span>
              <span role="columnheader"><span className="sr-only">View</span></span>
            </div>
            {chunks.map((record) => (
              <div className={styles.chunkRow} role="row" key={record.chunk_id}>
                <span role="cell">{record.chunk_index}</span>
                <span role="cell">{record.chunk_id}</span>
                <span role="cell">{record.byte_start}–{record.byte_end}</span>
                <span role="cell">{formatBytes(record.bytes)}</span>
                <span role="cell">{record.word_count}</span>
                <span role="cell">
                  <button type="button" onClick={() => { setOpenChunk(record.chunk_index); setVersionPin(source.content_hash); }}>
                    View
                  </button>
                </span>
              </div>
            ))}
          </div>
        )}

        {openChunk !== null && (
          <div className={styles.chunkViewer}>
            <header>
              <b>Chunk {openChunk}</b>
              <label style={{ display: "flex", alignItems: "center", gap: 6, color: "var(--text-faint)", font: "300 10.5px var(--mono)" }}>
                Version
                <select
                  aria-label="Chunk version"
                  value={versionPin}
                  onChange={(event) => setVersionPin(event.target.value)}
                >
                  {knownHashes.map((hash, index) => (
                    <option key={hash} value={hash}>
                      {index === 0 ? `current · ${hashPreview(hash)}` : `superseded · ${hashPreview(hash)}`}
                    </option>
                  ))}
                </select>
              </label>
              <button type="button" className={styles.backButton} onClick={() => setOpenChunk(null)}>Close</button>
            </header>
            {chunk.isLoading ? (
              <p className={styles.sectionNote} role="status">Loading chunk…</p>
            ) : chunk.isError ? (
              <p className={styles.error} role="alert">{chunk.error instanceof Error ? chunk.error.message : "Chunk could not be loaded."}</p>
            ) : chunk.data ? (
              <>
                <pre className={styles.chunkText}>{chunk.data.text}</pre>
                <dl className={styles.citationCard} aria-label="Citation">
                  <div><dt>Source</dt><dd>{chunk.data.citation.title}</dd></div>
                  <div><dt>Chunk id</dt><dd>{chunk.data.citation.chunk_id}</dd></div>
                  <div><dt>Content address</dt><dd>{chunk.data.citation.content_address}</dd></div>
                  <div><dt>Byte range</dt><dd>{chunk.data.citation.byte_start}–{chunk.data.citation.byte_end}</dd></div>
                  <div><dt>Words</dt><dd>{chunk.data.word_count}</dd></div>
                </dl>
              </>
            ) : null}
          </div>
        )}
      </section>
    </div>
  );
}

function CorrectionForm({
  sourceId,
  defaultAuthor,
  onCorrected,
  onCancel,
}: {
  sourceId: string;
  defaultAuthor: string;
  onCorrected: () => void;
  onCancel: () => void;
}) {
  const [author, setAuthor] = useState(defaultAuthor);
  const [body, setBody] = useState("");
  const [receipt, setReceipt] = useState<KnowledgeCorrectionReceipt | null>(null);
  const bodyBytes = bodyByteSize(body);
  const overCap = bodyBytes > KNOWLEDGE_MAX_SOURCE_BYTES;
  const canSubmit = author.trim().length > 0 && body.length > 0 && !overCap;

  const correct = useMutation({
    mutationFn: () => correctKnowledgeSource(sourceId, author.trim(), body),
    onSuccess: (result) => setReceipt(result),
  });

  function submit(event: FormEvent) {
    event.preventDefault();
    if (canSubmit && !correct.isPending) correct.mutate();
  }

  if (receipt) {
    return (
      <div className={styles.receipt} role="status">
        <h3>Correction registered</h3>
        <p>
          Version {receipt.version} is live now; retrieval stopped serving the superseded version. The old version stays addressable as evidence.
        </p>
        <dl>
          <div><dt>Version</dt><dd>v{receipt.version}</dd></div>
          <div><dt>Content hash</dt><dd>{receipt.content_hash}</dd></div>
          {receipt.supersedes && <div><dt>Supersedes</dt><dd>{receipt.supersedes}</dd></div>}
          <div><dt>Chunks</dt><dd>{receipt.chunk_count}</dd></div>
        </dl>
        <div className={styles.receiptActions}>
          <button className="primary-button" type="button" onClick={onCorrected}>View updated source</button>
        </div>
      </div>
    );
  }

  return (
    <form onSubmit={submit} aria-label="Correct source" style={{ marginTop: 14 }}>
      {correct.isError && (
        <p className={styles.error} role="alert">
          {correct.error instanceof Error ? correct.error.message : "Correction failed."}
        </p>
      )}
      <div className={styles.fields}>
        <label>
          Corrector
          <input value={author} onChange={(event) => setAuthor(event.target.value)} placeholder="human:maya" />
          <span className={styles.fieldHint}>Who is making the correction — mandatory, or it is indistinguishable from a rewrite.</span>
        </label>
        <label className={styles.wide}>
          Corrected body
          <textarea
            className={styles.bodyArea}
            value={body}
            onChange={(event) => setBody(event.target.value)}
            placeholder="The full corrected content — it replaces the body as a new version…"
            aria-invalid={overCap}
          />
          <span className={styles.fieldHint} data-over={overCap}>
            {formatBytes(bodyBytes)} of {formatBytes(KNOWLEDGE_MAX_SOURCE_BYTES)} — a byte-identical body is rejected; nothing changed.
          </span>
        </label>
      </div>
      <div className={styles.formActions}>
        <button className="secondary-button" type="button" onClick={onCancel}>Cancel</button>
        <button className="primary-button" type="submit" disabled={!canSubmit || correct.isPending}>
          {correct.isPending ? "Correcting…" : "Register correction"}
        </button>
      </div>
    </form>
  );
}
