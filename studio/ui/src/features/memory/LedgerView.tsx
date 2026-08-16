import { useMemo, useState, type FormEvent, type ReactNode } from "react";
import type { MemoryQueryInput, MemoryRecord } from "../../lib/api/memory";
import { authorText, scopeAddressText } from "../../lib/api/memory";
import {
  contentPreview,
  formatInstant,
  labelError,
  lifecycleLabels,
  recordAuthorText,
  recordScopeText,
  recordStates,
  recordTitle,
  shortAddress,
  supersededIds,
  authorFromFields,
} from "./memoryModel";
import styles from "./MemoryPage.module.css";

const scopeOptions = ["", "agent", "team", "user", "tenant", "run"] as const;
const kindOptions = ["", "fact", "preference", "example", "summary"] as const;
const authorOptions = ["", "human", "agent", "distiller", "system"] as const;

interface ConsoleDraft {
  scopeType: string;
  scopeId: string;
  kind: string;
  key: string;
  tagsText: string;
  minConfidence: string;
  authorType: string;
  authorId: string;
  includeExpired: boolean;
  includeSuperseded: boolean;
  candidatesOnly: boolean;
}

const emptyDraft: ConsoleDraft = {
  scopeType: "",
  scopeId: "",
  kind: "",
  key: "",
  tagsText: "",
  minConfidence: "",
  authorType: "",
  authorId: "",
  includeExpired: false,
  includeSuperseded: false,
  candidatesOnly: false,
};

function buildQuery(draft: ConsoleDraft): { query: MemoryQueryInput | null; errors: Record<string, string> } {
  const errors: Record<string, string> = {};
  if (draft.scopeType) {
    const scopeIdError = labelError("The scope identity", draft.scopeId, true);
    if (scopeIdError) errors.scopeId = scopeIdError;
  }
  if (draft.key) {
    const keyError = labelError("The key", draft.key, true);
    if (keyError) errors.key = keyError;
  }
  const tags = Array.from(new Set(draft.tagsText.split(",").map((tag) => tag.trim()).filter(Boolean)));
  if (tags.length > 32) errors.tagsText = "Use at most 32 tags.";
  else {
    const bad = tags.find((tag) => labelError("A tag", tag, true));
    if (bad) errors.tagsText = "Every tag must be 256 UTF-8 bytes or fewer with no control characters.";
  }
  let minConfidence: number | undefined;
  if (draft.minConfidence !== "") {
    const parsed = Number(draft.minConfidence);
    if (!/^(?:0(?:\.\d+)?|1(?:\.0+)?)$/.test(draft.minConfidence.trim()) || !Number.isFinite(parsed) || parsed < 0 || parsed > 1) {
      errors.minConfidence = "Use a confidence from 0 through 1.";
    } else {
      minConfidence = parsed;
    }
  }
  if (draft.authorType && draft.authorType !== "system") {
    const authorError = labelError("The author identity", draft.authorId, true);
    if (authorError) errors.authorId = authorError;
  }
  const authoredBy = draft.authorType === "system" ? { type: "system" as const } : authorFromFields(draft.authorType, draft.authorId);
  if (draft.authorType && !authoredBy) errors.authorId = errors.authorId || "Enter the exact author identity.";
  if (Object.keys(errors).length) return { query: null, errors };
  const query: MemoryQueryInput = {
    ...(draft.scopeType ? { scope: { scope: draft.scopeType as NonNullable<MemoryQueryInput["scope"]>["scope"], id: draft.scopeId } } : {}),
    ...(draft.kind ? { kinds: [draft.kind as MemoryRecord["kind"]] } : {}),
    ...(draft.key ? { key: draft.key } : {}),
    ...(tags.length ? { tags } : {}),
    ...(minConfidence !== undefined ? { min_confidence: minConfidence } : {}),
    ...(draft.includeExpired ? { include_expired: true } : {}),
    ...(draft.includeSuperseded ? { include_superseded: true } : {}),
    ...(authoredBy ? { authored_by: authoredBy } : {}),
    ...(draft.candidatesOnly ? { candidates_only: true } : {}),
  };
  return { query, errors };
}

function matchesText(record: MemoryRecord, needle: string) {
  if (!needle) return true;
  const haystack = [
    record.memory_id,
    record.key ?? "",
    scopeAddressText(record.scope),
    authorText(record.provenance.author),
    record.tags.join(" "),
    contentPreview(record, 2_000),
  ].join("\n").toLowerCase();
  return needle.toLowerCase().split(/\s+/).filter(Boolean).every((term) => haystack.includes(term));
}

export function LedgerView({
  search,
  searching,
  searchError,
  onSearch,
  conflictedIds,
  selectedId,
  onSelect,
  detail,
}: {
  search: { query: MemoryQueryInput; records: MemoryRecord[]; searchedAt: Date } | null;
  searching: boolean;
  searchError: string;
  onSearch: (query: MemoryQueryInput) => void;
  conflictedIds: Set<string>;
  selectedId: string;
  onSelect: (id: string) => void;
  detail: ReactNode;
}) {
  const [draft, setDraft] = useState<ConsoleDraft>(emptyDraft);
  const [errors, setErrors] = useState<Record<string, string>>({});
  const [text, setText] = useState("");

  function submit(event: FormEvent) {
    event.preventDefault();
    const built = buildQuery(draft);
    setErrors(built.errors);
    if (built.query) onSearch(built.query);
  }

  const superseded = useMemo(() => supersededIds(search?.records ?? []), [search]);
  const now = search?.searchedAt ?? new Date();
  const visible = useMemo(() => (search?.records ?? []).filter((record) => matchesText(record, text)).slice(0, 200), [search, text]);

  return (
    <>
      <section className={styles.console} aria-labelledby="memory-query-heading">
        <div className={styles.consoleHead}>
          <h2 id="memory-query-heading">Query the namespace</h2>
          <span>Structured retrieval only — Rusty has no similarity search. Absence of a hit is absence of a key, not absence of a fact.</span>
        </div>
        <form onSubmit={submit} noValidate>
          <div className={styles.consoleGrid}>
            <label>
              Scope
              <select value={draft.scopeType} onChange={(event) => setDraft({ ...draft, scopeType: event.target.value })} aria-label="Scope">
                {scopeOptions.map((option) => <option key={option} value={option}>{option ? option[0].toUpperCase() + option.slice(1) : "All scopes"}</option>)}
              </select>
            </label>
            <label>
              Scope identity
              <input value={draft.scopeId} onChange={(event) => setDraft({ ...draft, scopeId: event.target.value })} placeholder={draft.scopeType ? `Exact ${draft.scopeType} id` : "Choose a scope first"}
                aria-invalid={Boolean(errors.scopeId)} aria-describedby={errors.scopeId ? "memory-query-scope-error" : undefined} disabled={!draft.scopeType} />
              {errors.scopeId && <span className={styles.fieldError} id="memory-query-scope-error">{errors.scopeId}</span>}
            </label>
            <label>
              Kind
              <select value={draft.kind} onChange={(event) => setDraft({ ...draft, kind: event.target.value })} aria-label="Kind">
                {kindOptions.map((option) => <option key={option} value={option}>{option ? option[0].toUpperCase() + option.slice(1) : "All kinds"}</option>)}
              </select>
            </label>
            <label>
              Key
              <input value={draft.key} onChange={(event) => setDraft({ ...draft, key: event.target.value })} placeholder="Exact lookup key"
                aria-invalid={Boolean(errors.key)} aria-describedby={errors.key ? "memory-query-key-error" : undefined} />
              {errors.key && <span className={styles.fieldError} id="memory-query-key-error">{errors.key}</span>}
            </label>
            <label>
              Tags
              <input value={draft.tagsText} onChange={(event) => setDraft({ ...draft, tagsText: event.target.value })} placeholder="Comma-separated, all must match"
                aria-invalid={Boolean(errors.tagsText)} aria-describedby={errors.tagsText ? "memory-query-tags-error" : undefined} />
              {errors.tagsText && <span className={styles.fieldError} id="memory-query-tags-error">{errors.tagsText}</span>}
            </label>
            <label>
              Minimum confidence
              <input value={draft.minConfidence} onChange={(event) => setDraft({ ...draft, minConfidence: event.target.value })} placeholder="0 through 1" inputMode="decimal"
                aria-invalid={Boolean(errors.minConfidence)} aria-describedby={errors.minConfidence ? "memory-query-confidence-error" : undefined} />
              {errors.minConfidence && <span className={styles.fieldError} id="memory-query-confidence-error">{errors.minConfidence}</span>}
            </label>
            <label>
              Author
              <select value={draft.authorType} onChange={(event) => setDraft({ ...draft, authorType: event.target.value, authorId: "" })} aria-label="Author type">
                {authorOptions.map((option) => <option key={option} value={option}>{option ? option[0].toUpperCase() + option.slice(1) : "Any author"}</option>)}
              </select>
            </label>
            <label>
              Author identity
              <input value={draft.authorId} onChange={(event) => setDraft({ ...draft, authorId: event.target.value })} placeholder={draft.authorType === "system" ? "System needs no identity" : "Exact author id"}
                aria-invalid={Boolean(errors.authorId)} aria-describedby={errors.authorId ? "memory-query-author-error" : undefined} disabled={!draft.authorType || draft.authorType === "system"} />
              {errors.authorId && <span className={styles.fieldError} id="memory-query-author-error">{errors.authorId}</span>}
            </label>
          </div>
          <div className={styles.consoleChecks}>
            <label><input type="checkbox" checked={draft.includeExpired} onChange={(event) => setDraft({ ...draft, includeExpired: event.target.checked })} />Include expired</label>
            <label><input type="checkbox" checked={draft.includeSuperseded} onChange={(event) => setDraft({ ...draft, includeSuperseded: event.target.checked })} />Include superseded</label>
            <label><input type="checkbox" checked={draft.candidatesOnly} onChange={(event) => setDraft({ ...draft, candidatesOnly: event.target.checked })} />Candidates only</label>
          </div>
          <div className={styles.consoleActions}>
            {searchError && <span className={styles.error} role="alert">{searchError}</span>}
            <button className="secondary-button" type="button" onClick={() => { setDraft(emptyDraft); setErrors({}); }}>Clear</button>
            <button className="primary-button" type="submit" disabled={searching}>{searching ? "Querying…" : "Run query"}</button>
          </div>
        </form>
      </section>

      {search && (
        <div className={styles.searchRow}>
          <label>
            Filter these results
            <input type="search" value={text} onChange={(event) => setText(event.target.value)} placeholder="Match content, key, scope, author, tag, or id…" aria-describedby="memory-search-hint" />
          </label>
          <span className={styles.fieldHint} id="memory-search-hint">Client-side filter over the loaded records; content reads the first 2,000 characters per record. It never re-queries the store.</span>
        </div>
      )}

      <div className={styles.ledgerGrid}>
        <div>
          {!search ? (
            <div className="empty-state">
              <span className="eyebrow">Ledger</span>
              <h2>Run a query to load memory</h2>
              <p>An empty query matches the whole tenant namespace, minus expired and superseded records. Results arrive in the server's deterministic rank order.</p>
            </div>
          ) : visible.length ? (
            <ol className={styles.recordList} aria-label="Memory records">
              {visible.map((record) => {
                const states = recordStates(record, superseded, now);
                return (
                  <li key={record.memory_id}>
                    <button type="button" className={styles.recordItem} aria-current={selectedId === record.memory_id} onClick={() => onSelect(record.memory_id)}
                      aria-label={`Inspect ${recordTitle(record)}, record ${shortAddress(record.memory_id)}`}>
                      <span className={styles.recordTop}>
                        <b>{recordTitle(record)}</b>
                        <span className={styles.badges}>
                          <span className={styles.badge} data-kind={record.kind}>{record.kind}</span>
                          {states.map((state) => <span key={state} className={styles.badge} data-state={state}>{lifecycleLabels[state]}</span>)}
                          {conflictedIds.has(record.memory_id) && <span className={styles.badge} data-state="conflict">Conflict</span>}
                        </span>
                      </span>
                      <p>{contentPreview(record)}</p>
                      <code>{recordScopeText(record)} · {recordAuthorText(record)} · confidence {record.confidence} · {formatInstant(record.created_at)}</code>
                    </button>
                  </li>
                );
              })}
              <footer>
                {visible.length} of {search.records.length} loaded record{search.records.length === 1 ? "" : "s"}
                {search.records.length > 200 ? " · rendering is bounded at 200; narrow the query" : ""}
              </footer>
            </ol>
          ) : (
            <div className="empty-state">
              <span className="eyebrow">No matches</span>
              <h2>No governed memory matched</h2>
              <p>{text ? "No loaded record matches the text filter. Clear it to see everything the query returned." : "Absence of a hit is absence of a key, not absence of a fact — retrieval here is structural."}</p>
            </div>
          )}
        </div>
        {detail ?? (
          <div className="empty-state">
            <span className="eyebrow">Provenance</span>
            <h2>Select a record</h2>
            <p>The detail view cites the record's full provenance spine: author, evidence, validity, and its supersession chain.</p>
          </div>
        )}
      </div>
    </>
  );
}
