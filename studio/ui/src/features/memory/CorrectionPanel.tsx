import { useEffect, useState, type FormEvent } from "react";
import { connectionScope, StudioApiError } from "../../lib/api/client";
import { getMemory, submitCorrection, type CorrectionReceipt, type MemoryRecord, type ScopeAddress } from "../../lib/api/memory";
import { useConnectionStore } from "../../state/connection";
import { contentPreview, contentValue, labelError, mintCorrectionId, parseContentJson, recordScopeText, recordTitle, shortAddress } from "./memoryModel";
import styles from "./MemoryPage.module.css";

const correctionScopes = ["run", "agent", "team", "user", "tenant"] as const;

export function CorrectionPanel({
  initialTargetId,
  onSubmitted,
  onInspect,
}: {
  initialTargetId: string;
  onSubmitted: () => void;
  onInspect: (memoryId: string) => void;
}) {
  const { connection } = useConnectionStore();
  const [targetId, setTargetId] = useState(initialTargetId);
  const [target, setTarget] = useState<MemoryRecord | null>(null);
  const [targetError, setTargetError] = useState("");
  const [loadingTarget, setLoadingTarget] = useState(false);
  const [corrected, setCorrected] = useState("");
  const [scopeType, setScopeType] = useState("");
  const [scopeId, setScopeId] = useState("");
  const [author, setAuthor] = useState("");
  const [rationale, setRationale] = useState("");
  const [correctionId, setCorrectionId] = useState(mintCorrectionId);
  const [errors, setErrors] = useState<Record<string, string>>({});
  const [error, setError] = useState("");
  const [pending, setPending] = useState(false);
  const [receipt, setReceipt] = useState<CorrectionReceipt | null>(null);

  async function loadTarget(id: string) {
    if (!connection) return;
    const clean = id.trim();
    if (!/^[0-9a-f]{64}$/.test(clean)) {
      setTargetError("A memory id is the exact 64-character content address.");
      setTarget(null);
      return;
    }
    const scopeAtStart = connectionScope(connection);
    setLoadingTarget(true);
    setTargetError("");
    try {
      const record = await getMemory(connection, clean);
      const current = useConnectionStore.getState().connection;
      if (!current || connectionScope(current) !== scopeAtStart) return;
      setTarget(record);
      setScopeType(record.scope.scope);
      setScopeId(record.scope.id);
      const value = contentValue(record);
      setCorrected(value === undefined ? "" : JSON.stringify(value, null, 2) ?? "");
      setReceipt(null);
      setCorrectionId(mintCorrectionId());
    } catch (caught) {
      const current = useConnectionStore.getState().connection;
      if (!current || connectionScope(current) !== scopeAtStart) return;
      setTarget(null);
      setTargetError(caught instanceof StudioApiError ? caught.message : "The target record could not be loaded.");
    } finally {
      const current = useConnectionStore.getState().connection;
      if (current && connectionScope(current) === scopeAtStart) setLoadingTarget(false);
    }
  }

  useEffect(() => {
    if (initialTargetId) void loadTarget(initialTargetId);
    // Loading exactly the target the panel was opened for, once per mount.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (!connection || !target || pending) return;
    const next: Record<string, string> = {};
    const scopeIdError = labelError("The scope identity", scopeId, true);
    if (scopeIdError) next.scopeId = scopeIdError;
    const authorError = labelError("Your identity", author, true);
    if (authorError) next.author = authorError;
    const parsed = parseContentJson(corrected);
    if (parsed.error) next.corrected = parsed.error;
    setErrors(next);
    if (Object.keys(next).length) return;

    const scopeAtStart = connectionScope(connection);
    setPending(true);
    setError("");
    setReceipt(null);
    try {
      const result = await submitCorrection(connection, {
        correction_id: correctionId,
        author,
        targetMemoryId: target.memory_id,
        corrected: parsed.value,
        scope: { scope: scopeType as ScopeAddress["scope"], id: scopeId },
        ...(rationale.trim() ? { rationale: rationale.trim() } : {}),
      });
      const current = useConnectionStore.getState().connection;
      if (!current || connectionScope(current) !== scopeAtStart) return;
      setReceipt(result);
      onSubmitted();
    } catch (caught) {
      const current = useConnectionStore.getState().connection;
      if (!current || connectionScope(current) !== scopeAtStart) return;
      setError(caught instanceof StudioApiError ? caught.message : "The correction could not be submitted.");
    } finally {
      const current = useConnectionStore.getState().connection;
      if (current && connectionScope(current) === scopeAtStart) setPending(false);
    }
  }

  const targetContent = target ? contentValue(target) : undefined;
  const unchanged = Boolean(target) && corrected.trim() === (targetContent === undefined ? "" : JSON.stringify(targetContent, null, 2) ?? "");

  return (
    <section className={styles.panel} aria-labelledby="memory-correct-heading">
      <div className={styles.panelHead}>
        <div>
          <span className="eyebrow">Correction loop</span>
          <h2 id="memory-correct-heading">Correct a memory</h2>
        </div>
      </div>
      <p>
        A correction never edits the record it targets. It writes a new, attributed record — your identity and the
        correction id travel in its provenance — and a same-key correction supersedes the prior record. At agent scope
        or wider the correction is held as a candidate pending evaluation; run scope is adopted directly.
      </p>

      <form onSubmit={(event) => { event.preventDefault(); void loadTarget(targetId); }} noValidate>
        <div className={styles.formGrid}>
          <label className={styles.wide}>
            Memory to correct
            <input value={targetId} onChange={(event) => setTargetId(event.target.value)} placeholder="The record's 64-character content address"
              aria-invalid={Boolean(targetError)} aria-describedby={targetError ? "memory-target-error" : undefined} />
            {targetError && <span className={styles.fieldError} id="memory-target-error">{targetError}</span>}
          </label>
        </div>
        <div className={styles.panelActions}>
          <button className="secondary-button" type="submit" disabled={loadingTarget}>{loadingTarget ? "Loading…" : "Load record"}</button>
        </div>
      </form>

      {target && (
        <>
          <div className={styles.targetSummary}>
            <b>{recordTitle(target)}</b>
            <small>{target.memory_id}</small>
            <small>{recordScopeText(target)}</small>
            <p>{contentPreview(target, 320)}</p>
          </div>
          <form onSubmit={submit} noValidate>
            <div className={styles.formGrid}>
              <label>
                Scope of the corrected record
                <select value={scopeType} onChange={(event) => setScopeType(event.target.value)}>
                  {correctionScopes.map((option) => <option key={option} value={option}>{option[0].toUpperCase() + option.slice(1)}</option>)}
                </select>
                <span className={styles.fieldHint}>{scopeType === "run"
                  ? "Run scope is adopted directly — it affects only the run that produced it."
                  : "Agent scope or wider becomes a candidate pending evaluation — a wrong correction at a wide scope is a production incident with a name attached."}</span>
              </label>
              <label>
                Scope identity
                <input value={scopeId} onChange={(event) => setScopeId(event.target.value)}
                  aria-invalid={Boolean(errors.scopeId)} aria-describedby={errors.scopeId ? "memory-correct-scope-error" : undefined} />
                {errors.scopeId && <span className={styles.fieldError} id="memory-correct-scope-error">{errors.scopeId}</span>}
              </label>
              <label>
                Your identity
                <input value={author} onChange={(event) => setAuthor(event.target.value)} placeholder="Who is correcting"
                  aria-invalid={Boolean(errors.author)} aria-describedby={errors.author ? "memory-correct-author-error" : undefined} />
                {errors.author && <span className={styles.fieldError} id="memory-correct-author-error">{errors.author}</span>}
              </label>
              <label>
                Correction id <span className={styles.fieldHint}>minted once; a retried submission converges on it</span>
                <input value={correctionId} readOnly aria-readonly="true" />
              </label>
              <label className={styles.wide}>
                Corrected content <span className={styles.fieldHint}>JSON — what the record should have asserted</span>
                <textarea rows={6} value={corrected} onChange={(event) => setCorrected(event.target.value)}
                  aria-invalid={Boolean(errors.corrected)} aria-describedby={errors.corrected ? "memory-correct-content-error" : undefined} />
                {errors.corrected && <span className={styles.fieldError} id="memory-correct-content-error">{errors.corrected}</span>}
              </label>
              <label className={styles.wide}>
                Rationale <span className={styles.fieldHint}>optional — why the correction is right</span>
                <input value={rationale} onChange={(event) => setRationale(event.target.value)} />
              </label>
            </div>
            {unchanged && <p className={styles.panelNote}>The corrected content is identical to the target's. A correction should change what the record asserts.</p>}
            {error && <p className={styles.error} role="alert">{error}</p>}
            <div className={styles.panelActions}>
              <button className="primary-button" type="submit" disabled={pending || Boolean(unchanged)}>{pending ? "Submitting…" : "Submit correction"}</button>
            </div>
          </form>
        </>
      )}

      {receipt && target && (
        <div className={styles.receipt} role="status">
          <h3>{receipt.created ? (receipt.candidate ? "Correction held as a candidate" : "Correction adopted") : "Correction already recorded"}</h3>
          <p className={receipt.candidate ? styles.candidateNote : undefined}>
            {receipt.created
              ? receipt.candidate
                ? "Attribution travels with the record; evaluation decides whether it serves. Query with “Candidates only” to review the queue."
                : "Run scope adopts directly — the corrected record serves the run that produced it."
              : "This correction id was already recorded; the receipt names the record the first submission wrote."}
          </p>
          <code>{receipt.attribution}</code>
          <div className={styles.chain}>
            <h3>Old → new</h3>
            <ol>
              <li>
                <span className={styles.chainMark}>old</span>
                <div>
                  <button type="button" onClick={() => onInspect(receipt.superseded ?? target.memory_id)}>{shortAddress(receipt.superseded ?? target.memory_id)}</button>
                  <small> {recordTitle(target)}{receipt.superseded && receipt.superseded !== target.memory_id ? " · the top-ranked live record at this key, not the correction target" : ""}</small>
                </div>
              </li>
              <li data-current="true">
                <span className={styles.chainMark}>new</span>
                <div>
                  <button type="button" onClick={() => onInspect(receipt.memory_id)}>{shortAddress(receipt.memory_id)}</button>
                  <small> {receipt.record.key ?? "corrected record"} · confidence {receipt.record.confidence}</small>
                </div>
              </li>
            </ol>
          </div>
        </div>
      )}
    </section>
  );
}
