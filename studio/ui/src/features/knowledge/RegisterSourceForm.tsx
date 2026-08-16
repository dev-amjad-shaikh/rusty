import { type FormEvent, useMemo, useState } from "react";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import type { ConnectionIdentity } from "../../lib/api/client";
import {
  KNOWLEDGE_MAX_ATTRIBUTION_BYTES,
  KNOWLEDGE_MAX_SOURCE_BYTES,
  KNOWLEDGE_MAX_SOURCE_ID_BYTES,
  KNOWLEDGE_MAX_TITLE_BYTES,
  registerKnowledgeSource,
  type KnowledgeRegisterReceipt,
  type KnowledgeSourceKind,
} from "../../lib/api/knowledge";
import { bodyByteSize, formatBytes, SOURCE_ID_PATTERN } from "./format";
import styles from "./KnowledgePage.module.css";

const kinds: Array<{ value: KnowledgeSourceKind; label: string; hint: string }> = [
  { value: "text", label: "text", hint: "plain prose, chunked on paragraph boundaries" },
  { value: "markdown", label: "markdown", hint: "fence-aware — code blocks never split" },
  { value: "json", label: "json", hint: "chunked as text for now" },
  { value: "csv", label: "csv", hint: "chunked as text for now" },
];

export function RegisterSourceForm({
  connection,
  onDone,
  onCancel,
}: {
  connection: ConnectionIdentity;
  onDone: (sourceId: string) => void;
  onCancel: () => void;
}) {
  const queryClient = useQueryClient();
  const [sourceId, setSourceId] = useState("");
  const [kind, setKind] = useState<KnowledgeSourceKind>("text");
  const [title, setTitle] = useState("");
  const [author, setAuthor] = useState("human:");
  const [body, setBody] = useState("");
  const [confidence, setConfidence] = useState(1);
  const [retentionMode, setRetentionMode] = useState<"pinned" | "ttl">("pinned");
  const [expiresAt, setExpiresAt] = useState("");
  const [receipt, setReceipt] = useState<KnowledgeRegisterReceipt | null>(null);

  const bodyBytes = useMemo(() => bodyByteSize(body), [body]);
  const overCap = bodyBytes > KNOWLEDGE_MAX_SOURCE_BYTES;
  const sourceIdValid = sourceId.length > 0 && sourceId.length <= KNOWLEDGE_MAX_SOURCE_ID_BYTES && SOURCE_ID_PATTERN.test(sourceId);
  const expiresDate = expiresAt ? new Date(expiresAt) : null;
  const ttlInvalid = retentionMode === "ttl" && (!expiresDate || Number.isNaN(expiresDate.getTime()) || expiresDate.getTime() <= Date.now());
  const canSubmit = sourceIdValid && title.trim().length > 0 && title.length <= KNOWLEDGE_MAX_TITLE_BYTES
    && author.trim().length > 0 && author.length <= KNOWLEDGE_MAX_ATTRIBUTION_BYTES
    && body.length > 0 && !overCap && !ttlInvalid;

  const register = useMutation({
    mutationFn: () => registerKnowledgeSource(connection, {
      source_id: sourceId,
      kind,
      title: title.trim(),
      author: author.trim(),
      body,
      confidence,
      retention: retentionMode === "pinned"
        ? { policy: "pinned" }
        : { policy: "ttl", expires_at: expiresDate!.toISOString() },
    }),
    onSuccess: (result) => {
      setReceipt(result);
      queryClient.invalidateQueries({ queryKey: [connection.epoch, connection.origin, connection.tenantFingerprint, "knowledge"] });
    },
  });

  function submit(event: FormEvent) {
    event.preventDefault();
    if (canSubmit && !register.isPending) register.mutate();
  }

  function reset() {
    setReceipt(null);
    setSourceId("");
    setKind("text");
    setTitle("");
    setAuthor("human:");
    setBody("");
    setConfidence(1);
    setRetentionMode("pinned");
    setExpiresAt("");
    register.reset();
  }

  if (receipt) {
    return (
      <div className={styles.panel}>
        <div className={styles.receipt} role="status">
          <h3>{receipt.created ? "Source registered" : "Already registered"}</h3>
          <p>{receipt.created
            ? "The body is stored under its content address and chunked for cited retrieval."
            : "This exact body was already registered under this source id — registration is idempotent, so nothing was written twice."}</p>
          <dl>
            <div><dt>Source id</dt><dd>{receipt.source_id}</dd></div>
            <div><dt>Version</dt><dd>v{receipt.version}</dd></div>
            <div><dt>Content hash</dt><dd>{receipt.content_hash}</dd></div>
            <div><dt>Chunks</dt><dd>{receipt.chunk_count}</dd></div>
          </dl>
          <div className={styles.receiptActions}>
            <button className="primary-button" type="button" onClick={() => onDone(receipt.source_id)}>Open source</button>
            <button className="secondary-button" type="button" onClick={reset}>Register another</button>
          </div>
        </div>
      </div>
    );
  }

  return (
    <form className={styles.panel} onSubmit={submit} aria-label="Register source">
      <h2>Register source</h2>
      <p className={styles.panelLead}>
        Registration is fail-closed: the body is normalized, hashed, and chunked before anything is stored. A different body under an existing id is a correction, not an overwrite.
      </p>
      {register.isError && (
        <p className={styles.error} role="alert">
          {register.error instanceof Error ? register.error.message : "Registration failed."}
        </p>
      )}
      <div className={styles.fields}>
        <div className={styles.choiceStack} role="group" aria-label="Source kind">
          {kinds.map((option) => (
            <button
              key={option.value}
              type="button"
              className={kind === option.value ? styles.selectedChoice : undefined}
              aria-pressed={kind === option.value}
              onClick={() => setKind(option.value)}
            >
              <i aria-hidden="true" />
              <b>{option.label}</b>
              <small>{option.hint}</small>
            </button>
          ))}
        </div>

        <label>
          Source id
          <input
            value={sourceId}
            onChange={(event) => setSourceId(event.target.value)}
            placeholder="travel-policy"
            aria-invalid={sourceId.length > 0 && !sourceIdValid}
            maxLength={KNOWLEDGE_MAX_SOURCE_ID_BYTES}
          />
          <span className={styles.fieldHint}>Stable name shared by every version — letters, digits, . _ : -</span>
        </label>
        <label>
          Title
          <input
            value={title}
            onChange={(event) => setTitle(event.target.value)}
            placeholder="Travel policy, March 2026"
            maxLength={KNOWLEDGE_MAX_TITLE_BYTES}
          />
          <span className={styles.fieldHint}>The name citations render.</span>
        </label>
        <label>
          Author
          <input
            value={author}
            onChange={(event) => setAuthor(event.target.value)}
            placeholder="human:maya"
            maxLength={KNOWLEDGE_MAX_ATTRIBUTION_BYTES}
          />
          <span className={styles.fieldHint}>Provenance — human:you, agent:id, or system. Mandatory, so the source can be audited.</span>
        </label>
        <label>
          Confidence
          <span className={styles.confidenceRow}>
            <input
              type="range"
              min={0.05}
              max={1}
              step={0.05}
              value={confidence}
              onChange={(event) => setConfidence(Number(event.target.value))}
            />
            <output>{confidence.toFixed(2)}</output>
          </span>
          <span className={styles.fieldHint}>A writer-declared claim in (0, 1] — nothing computes it.</span>
        </label>
        <label className={styles.wide}>
          Body
          <textarea
            className={styles.bodyArea}
            value={body}
            onChange={(event) => setBody(event.target.value)}
            placeholder="Paste the source text…"
            aria-invalid={overCap}
          />
          <span className={styles.fieldHint} data-over={overCap}>
            {formatBytes(bodyBytes)} of {formatBytes(KNOWLEDGE_MAX_SOURCE_BYTES)} — oversize bodies fail closed at registration, they are not truncated.
          </span>
        </label>

        <label>
          Retention
          <select value={retentionMode} onChange={(event) => setRetentionMode(event.target.value as "pinned" | "ttl")}>
            <option value="pinned">Pinned — exempt from expiry and sweeps</option>
            <option value="ttl">TTL — expires at a set instant</option>
          </select>
        </label>
        {retentionMode === "ttl" && (
          <label>
            Expires at
            <input
              type="datetime-local"
              value={expiresAt}
              onChange={(event) => setExpiresAt(event.target.value)}
              aria-invalid={ttlInvalid}
            />
            <span className={styles.fieldHint}>The source stops answering retrieval at this instant; a sweep may purge it after.</span>
          </label>
        )}
      </div>
      <div className={styles.formActions}>
        <button className="secondary-button" type="button" onClick={onCancel}>Cancel</button>
        <button className="primary-button" type="submit" disabled={!canSubmit || register.isPending}>
          {register.isPending ? "Registering…" : "Register source"}
        </button>
      </div>
    </form>
  );
}
