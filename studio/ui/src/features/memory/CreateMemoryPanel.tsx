import { useState, type FormEvent } from "react";
import { connectionScope, StudioApiError } from "../../lib/api/client";
import { writeMemory, type MemoryRecord, type WriteMemoryReceipt } from "../../lib/api/memory";
import { useConnectionStore } from "../../state/connection";
import { authorFromFields, labelError, localInstantToIso, parseContentJson } from "./memoryModel";
import styles from "./MemoryPage.module.css";

const writeScopes = ["agent", "team", "user", "tenant"] as const;
const kindOptions: { value: MemoryRecord["kind"]; hint: string }[] = [
  { value: "fact", hint: "An asserted fact about the world the agent operates in." },
  { value: "preference", hint: "A stated preference, meant to shape future behavior within its scope." },
  { value: "example", hint: "A corrected input/output pair — usually produced by the correction loop." },
  { value: "summary", hint: "A consolidation of other records — usually produced by a distiller." },
];

export function CreateMemoryPanel({ onCreated }: { onCreated: (receipt: WriteMemoryReceipt) => void }) {
  const { connection } = useConnectionStore();
  const [scopeType, setScopeType] = useState<string>("user");
  const [scopeId, setScopeId] = useState("");
  const [kind, setKind] = useState<string>("fact");
  const [key, setKey] = useState("");
  const [tagsText, setTagsText] = useState("");
  const [authorType, setAuthorType] = useState("human");
  const [authorId, setAuthorId] = useState("");
  const [confidence, setConfidence] = useState("");
  const [content, setContent] = useState("");
  const [expiresAt, setExpiresAt] = useState("");
  const [validUntil, setValidUntil] = useState("");
  const [errors, setErrors] = useState<Record<string, string>>({});
  const [error, setError] = useState("");
  const [pending, setPending] = useState(false);
  const [receipt, setReceipt] = useState<WriteMemoryReceipt | null>(null);

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (!connection || pending) return;
    const next: Record<string, string> = {};
    const scopeIdError = labelError("The scope identity", scopeId, true);
    if (scopeIdError) next.scopeId = scopeIdError;
    const keyError = labelError("The key", key, false);
    if (keyError) next.key = keyError;
    const tags = Array.from(new Set(tagsText.split(",").map((tag) => tag.trim()).filter(Boolean)));
    if (tags.length > 32) next.tags = "Use at most 32 tags.";
    else if (tags.some((tag) => labelError("A tag", tag, true))) next.tags = "Every tag must be 256 UTF-8 bytes or fewer with no control characters.";
    const authorIdError = labelError("The author identity", authorId, true);
    if (authorIdError) next.authorId = authorIdError;
    const author = authorFromFields(authorType, authorId);
    if (!author) next.authorId = next.authorId || "Enter the exact author identity.";
    let declaredConfidence: number | undefined;
    if (confidence !== "") {
      const parsed = Number(confidence);
      if (!/^(?:0(?:\.\d+)?|1(?:\.0+)?)$/.test(confidence.trim()) || !Number.isFinite(parsed) || parsed <= 0 || parsed > 1) {
        next.confidence = "Confidence must be in (0, 1] — a value outside the interval is not a claim at all.";
      } else {
        declaredConfidence = parsed;
      }
    }
    if (authorType !== "human" && declaredConfidence === undefined && !next.confidence) {
      next.confidence = "Confidence is required for non-human authors; human-authored records default to 1.0.";
    }
    const parsed = parseContentJson(content);
    if (parsed.error) next.content = parsed.error;
    const expiry = localInstantToIso(expiresAt);
    if (expiry.error) next.expiresAt = expiry.error;
    const until = localInstantToIso(validUntil);
    if (until.error) next.validUntil = until.error;
    setErrors(next);
    if (Object.keys(next).length || !author) return;

    const scopeAtStart = connectionScope(connection);
    setPending(true);
    setError("");
    setReceipt(null);
    try {
      const result = await writeMemory(connection, {
        kind: kind as MemoryRecord["kind"],
        scope: { scope: scopeType as (typeof writeScopes)[number], id: scopeId },
        content: parsed.value,
        author,
        ...(key ? { key } : {}),
        ...(tags.length ? { tags } : {}),
        ...(declaredConfidence !== undefined ? { confidence: declaredConfidence } : {}),
        ...(until.iso ? { valid_until: until.iso } : {}),
        ...(expiry.iso ? { expires_at: expiry.iso } : {}),
      });
      const current = useConnectionStore.getState().connection;
      if (!current || connectionScope(current) !== scopeAtStart) return;
      setReceipt(result);
    } catch (caught) {
      const current = useConnectionStore.getState().connection;
      if (!current || connectionScope(current) !== scopeAtStart) return;
      setError(caught instanceof StudioApiError ? caught.message : "The memory could not be written.");
    } finally {
      const current = useConnectionStore.getState().connection;
      if (current && connectionScope(current) === scopeAtStart) setPending(false);
    }
  }

  return (
    <section className={styles.panel} aria-labelledby="memory-create-heading">
      <div className={styles.panelHead}>
        <div>
          <span className="eyebrow">Guided write</span>
          <h2 id="memory-create-heading">New memory</h2>
        </div>
      </div>
      <p>
        A write mints a content-addressed, immutable record — identity is the content plus its provenance, so the same
        learning written twice converges on one record. Run scope is runtime-only and deliberately absent here.
      </p>
      <form onSubmit={submit} noValidate>
        <div className={styles.formGrid}>
          <label>
            Scope
            <select value={scopeType} onChange={(event) => setScopeType(event.target.value)}>
              {writeScopes.map((option) => <option key={option} value={option}>{option[0].toUpperCase() + option.slice(1)}</option>)}
            </select>
            <span className={styles.fieldHint}>
              {scopeType === "agent" ? "The agent must be registered and declare the private state scope in its manifest." : null}
              {scopeType === "team" ? "Shared by the team's members." : null}
              {scopeType === "user" ? "One end user's memory, across that user's agents and threads." : null}
              {scopeType === "tenant" ? "Configuration-grade memory; the id must be this tenant." : null}
            </span>
          </label>
          <label>
            Scope identity
            <input value={scopeId} onChange={(event) => setScopeId(event.target.value)} placeholder={`Exact ${scopeType} id`}
              aria-invalid={Boolean(errors.scopeId)} aria-describedby={errors.scopeId ? "memory-create-scope-error" : undefined} />
            {errors.scopeId && <span className={styles.fieldError} id="memory-create-scope-error">{errors.scopeId}</span>}
          </label>
          <label>
            Kind
            <select value={kind} onChange={(event) => setKind(event.target.value)}>
              {kindOptions.map((option) => <option key={option.value} value={option.value}>{option.value[0].toUpperCase() + option.value.slice(1)}</option>)}
            </select>
            <span className={styles.fieldHint}>{kindOptions.find((option) => option.value === kind)?.hint}</span>
          </label>
          <label>
            Lookup key <span className={styles.fieldHint}>optional</span>
            <input value={key} onChange={(event) => setKey(event.target.value)} placeholder="The named question this record answers"
              aria-invalid={Boolean(errors.key)} aria-describedby={errors.key ? "memory-create-key-error" : undefined} />
            {errors.key && <span className={styles.fieldError} id="memory-create-key-error">{errors.key}</span>}
          </label>
          <label>
            Tags <span className={styles.fieldHint}>optional, comma-separated</span>
            <input value={tagsText} onChange={(event) => setTagsText(event.target.value)}
              aria-invalid={Boolean(errors.tags)} aria-describedby={errors.tags ? "memory-create-tags-error" : undefined} />
            {errors.tags && <span className={styles.fieldError} id="memory-create-tags-error">{errors.tags}</span>}
          </label>
          <label>
            Author
            <select value={authorType} onChange={(event) => setAuthorType(event.target.value)}>
              <option value="human">Human</option>
              <option value="agent">Agent</option>
              <option value="distiller">Distiller</option>
            </select>
            <span className={styles.fieldHint}>Provenance is mandatory — a record that cannot name its origin cannot be audited.</span>
          </label>
          <label>
            {authorType === "distiller" ? "Distiller name" : authorType === "agent" ? "Agent id" : "Your identity"}
            <input value={authorId} onChange={(event) => setAuthorId(event.target.value)}
              aria-invalid={Boolean(errors.authorId)} aria-describedby={errors.authorId ? "memory-create-author-error" : undefined} />
            {errors.authorId && <span className={styles.fieldError} id="memory-create-author-error">{errors.authorId}</span>}
          </label>
          <label>
            Confidence {authorType === "human" ? <span className={styles.fieldHint}>optional — defaults to 1.0</span> : <span className={styles.fieldHint}>required</span>}
            <input value={confidence} onChange={(event) => setConfidence(event.target.value)} placeholder="(0, 1]" inputMode="decimal"
              aria-invalid={Boolean(errors.confidence)} aria-describedby={errors.confidence ? "memory-create-confidence-error" : undefined} />
            {errors.confidence && <span className={styles.fieldError} id="memory-create-confidence-error">{errors.confidence}</span>}
          </label>
          <label className={styles.wide}>
            Content <span className={styles.fieldHint}>JSON — the exact body the record asserts</span>
            <textarea rows={6} value={content} onChange={(event) => setContent(event.target.value)} placeholder='{"timezone": "Asia/Dubai"}'
              aria-invalid={Boolean(errors.content)} aria-describedby={errors.content ? "memory-create-content-error" : undefined} />
            {errors.content && <span className={styles.fieldError} id="memory-create-content-error">{errors.content}</span>}
          </label>
          <label>
            Valid until <span className={styles.fieldHint}>optional — exclusive end of the claimed-true interval</span>
            <input type="datetime-local" value={validUntil} onChange={(event) => setValidUntil(event.target.value)}
              aria-invalid={Boolean(errors.validUntil)} aria-describedby={errors.validUntil ? "memory-create-until-error" : undefined} />
            {errors.validUntil && <span className={styles.fieldError} id="memory-create-until-error">{errors.validUntil}</span>}
          </label>
          <label>
            Expires <span className={styles.fieldHint}>optional TTL — a retrieval filter, not a reaper</span>
            <input type="datetime-local" value={expiresAt} onChange={(event) => setExpiresAt(event.target.value)}
              aria-invalid={Boolean(errors.expiresAt)} aria-describedby={errors.expiresAt ? "memory-create-expiry-error" : undefined} />
            {errors.expiresAt && <span className={styles.fieldError} id="memory-create-expiry-error">{errors.expiresAt}</span>}
          </label>
        </div>
        {error && <p className={styles.error} role="alert">{error}</p>}
        <div className={styles.panelActions}>
          <button className="primary-button" type="submit" disabled={pending}>{pending ? "Writing…" : "Write memory"}</button>
        </div>
      </form>
      {receipt && (
        <div className={styles.receipt} role="status">
          <h3>{receipt.created ? "Memory written" : "Already stored"}</h3>
          <p>{receipt.created
            ? "The receipt below is the stored record's content address — the write is durable."
            : "An identical record (same content and provenance) was already held. Content addressing made this write idempotent; nothing was duplicated."}</p>
          <code>{receipt.memory_id}</code>
          <div className={styles.receiptActions}>
            <button className="secondary-button" type="button" onClick={() => onCreated(receipt)}>Inspect in the ledger</button>
          </div>
        </div>
      )}
    </section>
  );
}
