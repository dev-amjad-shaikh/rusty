import { type FormEvent, useState } from "react";
import { useMutation } from "@tanstack/react-query";
import {
  KNOWLEDGE_DEFAULT_MAX_RESULT_BYTES,
  KNOWLEDGE_DEFAULT_MAX_RESULTS,
  KNOWLEDGE_MAX_RESULT_BYTES_CEILING,
  KNOWLEDGE_MAX_RESULTS_CEILING,
  queryKnowledge,
  type KnowledgeQueryResponse,
} from "../../lib/api/knowledge";
import { formatBytes } from "./format";
import styles from "./KnowledgePage.module.css";

export function QueryConsole() {
  const [text, setText] = useState("");
  const [maxResults, setMaxResults] = useState(KNOWLEDGE_DEFAULT_MAX_RESULTS);
  const [maxBytes, setMaxBytes] = useState(KNOWLEDGE_DEFAULT_MAX_RESULT_BYTES);
  const [response, setResponse] = useState<KnowledgeQueryResponse | null>(null);

  const limitsValid = Number.isInteger(maxResults) && maxResults >= 1 && maxResults <= KNOWLEDGE_MAX_RESULTS_CEILING
    && Number.isInteger(maxBytes) && maxBytes >= 1 && maxBytes <= KNOWLEDGE_MAX_RESULT_BYTES_CEILING;
  const canRun = text.trim().length > 0 && limitsValid;

  const run = useMutation({
    mutationFn: () => queryKnowledge(text.trim(), { max_results: maxResults, max_bytes: maxBytes }),
    onSuccess: (result) => setResponse(result),
  });

  function submit(event: FormEvent) {
    event.preventDefault();
    runQuery();
  }

  function runQuery() {
    if (canRun && !run.isPending) run.mutate();
  }

  return (
    <div className={styles.panel}>
      <h2>Query console</h2>
      <p className={styles.panelLead}>
        Test retrieval over the tenant's live sources. Every result is a cited chunk — text with its citation, never bare text. Ranking is deterministic: the same query returns the same order.
      </p>
      <form onSubmit={submit} aria-label="Knowledge query">
        <div className={styles.queryRow}>
          <label>
            Query
            <input
              value={text}
              onChange={(event) => setText(event.target.value)}
              placeholder="What is the hotel cap in Berlin?"
            />
          </label>
        </div>
        <div className={styles.limitsRow}>
          <label>
            Max results
            <input
              type="number"
              min={1}
              max={KNOWLEDGE_MAX_RESULTS_CEILING}
              value={maxResults}
              onChange={(event) => setMaxResults(Number(event.target.value))}
              aria-invalid={!(Number.isInteger(maxResults) && maxResults >= 1 && maxResults <= KNOWLEDGE_MAX_RESULTS_CEILING)}
            />
          </label>
          <label>
            Max bytes
            <input
              type="number"
              min={1}
              max={KNOWLEDGE_MAX_RESULT_BYTES_CEILING}
              step={1024}
              value={maxBytes}
              onChange={(event) => setMaxBytes(Number(event.target.value))}
              aria-invalid={!(Number.isInteger(maxBytes) && maxBytes >= 1 && maxBytes <= KNOWLEDGE_MAX_RESULT_BYTES_CEILING)}
            />
          </label>
          <span className={styles.fieldHint}>
            Both ceilings truncate: packing stops at the first result that would exceed one. Ceilings 1–{KNOWLEDGE_MAX_RESULTS_CEILING} results, 1 B–{formatBytes(KNOWLEDGE_MAX_RESULT_BYTES_CEILING)}.
          </span>
          <button className="primary-button" type="button" onClick={runQuery} disabled={!canRun || run.isPending}>
            {run.isPending ? "Querying…" : "Run query"}
          </button>
        </div>
      </form>

      {run.isError && (
        <p className={styles.error} role="alert" style={{ marginTop: 14 }}>
          {run.error instanceof Error ? run.error.message : "The query failed."}
        </p>
      )}

      {response && (
        <section aria-label="Query results" style={{ marginTop: 18 }}>
          <h2 className={styles.sectionTitle}>
            {response.results.length === 0 ? "No results" : `${response.results.length} cited chunk${response.results.length === 1 ? "" : "s"}`}
          </h2>
          {response.results.length === 0 ? (
            <p className={styles.sectionNote}>No live source matched. Superseded and expired sources are filtered before ranking.</p>
          ) : (
            <ol className={styles.resultList} style={{ margin: 0, padding: 0, listStyle: "none" }}>
              {response.results.map((result, index) => (
                <li className={styles.resultCard} key={`${result.citation.content_address}-${index}`}>
                  <header>
                    <b>{result.citation.title}</b>
                    <code>{result.citation.chunk_id}</code>
                    <span className={styles.score}>score {result.score.toFixed(3)}</span>
                  </header>
                  <pre className={styles.resultText}>{result.text}</pre>
                  <dl className={styles.citationCard} aria-label={`Citation for ${result.citation.chunk_id}`}>
                    <div><dt>Source id</dt><dd>{result.citation.source_id}</dd></div>
                    <div><dt>Chunk id</dt><dd>{result.citation.chunk_id}</dd></div>
                    <div><dt>Content address</dt><dd>{result.citation.content_address}</dd></div>
                    <div><dt>Byte range</dt><dd>{result.citation.byte_start}–{result.citation.byte_end}</dd></div>
                    <div><dt>Words</dt><dd>{result.word_count}</dd></div>
                  </dl>
                </li>
              ))}
            </ol>
          )}
        </section>
      )}
    </div>
  );
}
