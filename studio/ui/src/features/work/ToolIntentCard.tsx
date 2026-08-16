import { useMemo } from "react";
import type { RunEvent } from "../../lib/contracts";
import { deriveRenderIntentFromEvent, type RenderIntent, type SearchHitView } from "../../lib/api/renderIntents";
import styles from "./ToolIntentCard.module.css";

/**
 * The evidence card for one journaled tool call. The intent is derived
 * client-side from the event's journaled request and result by the rules
 * mirrored from `rusty-core/src/render_intent.rs`, so a replayed run —
 * which journals the same result — renders the identical card. Anything
 * unrecognized renders as the honest generic card; nothing disappears.
 */
export function ToolIntentCard({ event }: { event: RunEvent }) {
  const intent = useMemo(() => deriveRenderIntentFromEvent(event), [event]);
  if (!intent) return null;
  return (
    <section className={styles.card} aria-label={`${intent.kind} card`} data-intent={intent.kind}>
      <IntentBody intent={intent} />
      {"truncated" in intent && intent.truncated ? (
        <p className={styles.clamped}>Clamped for display — the journal holds the full payload.</p>
      ) : null}
    </section>
  );
}

function IntentBody({ intent }: { intent: RenderIntent }) {
  switch (intent.kind) {
    case "terminal": return <Terminal intent={intent} />;
    case "diff": return <Diff intent={intent} />;
    case "search": return <Search intent={intent} />;
    case "read": return <Read intent={intent} />;
    case "table": return <Table intent={intent} />;
    case "link": return <LinkCard intent={intent} />;
    case "web": return <Web intent={intent} />;
    case "generic": return <Generic intent={intent} />;
  }
}

type Intent<K extends RenderIntent["kind"]> = Extract<RenderIntent, { kind: K }>;

function Terminal({ intent }: { intent: Intent<"terminal"> }) {
  return (
    <>
      <p className={styles.command}><span aria-hidden="true">$</span> {intent.command}</p>
      <p className={styles.meta}>
        {intent.cwd ? <>in <code>{intent.cwd}</code> · </> : null}
        {intent.timed_out ? "killed on timeout" : intent.exit_code === null ? "killed" : `exit ${intent.exit_code}`}
      </p>
      {intent.stdout ? <pre className={styles.stream}>{intent.stdout}</pre> : null}
      {intent.stderr ? <pre className={`${styles.stream} ${styles.stderr}`}>{intent.stderr}</pre> : null}
    </>
  );
}

function Diff({ intent }: { intent: Intent<"diff"> }) {
  return (
    <>
      <p className={styles.meta}>Diff over <code>{intent.path}</code></p>
      <div className={styles.diffGrid}>
        <figure>
          <figcaption>Before</figcaption>
          <pre className={styles.stream}>{intent.before}</pre>
        </figure>
        <figure>
          <figcaption>After</figcaption>
          <pre className={styles.stream}>{intent.after}</pre>
        </figure>
      </div>
    </>
  );
}

function Search({ intent }: { intent: Intent<"search"> }) {
  return (
    <>
      <p className={styles.meta}>Search <code>{intent.query}</code> · {intent.hits.length} hit{intent.hits.length === 1 ? "" : "s"}</p>
      {intent.hits.length ? (
        <ol className={styles.hits}>
          {intent.hits.map((hit, index) => <Hit key={`${hit.reference ?? hit.label}-${index}`} hit={hit} />)}
        </ol>
      ) : <p className={styles.meta}>No hits recorded.</p>}
    </>
  );
}

function Hit({ hit }: { hit: SearchHitView }) {
  return (
    <li>
      <span className={styles.hitHead}>
        <b>{hit.label || "Untitled hit"}</b>
        {hit.reference ? <code>{hit.reference}</code> : null}
        {hit.score !== null ? <small>score {hit.score}</small> : null}
      </span>
      {hit.excerpt ? <p>{hit.excerpt}</p> : null}
    </li>
  );
}

function Read({ intent }: { intent: Intent<"read"> }) {
  return (
    <>
      <p className={styles.meta}>Read <code>{intent.path || "document"}</code>{intent.format ? <> · {intent.format}</> : null}</p>
      {intent.excerpt ? <pre className={styles.stream}>{intent.excerpt}</pre> : <p className={styles.meta}>The document was empty.</p>}
    </>
  );
}

function Table({ intent }: { intent: Intent<"table"> }) {
  return (
    <div className={styles.tableScroll}>
      <table className={styles.table}>
        <thead>
          <tr>{intent.columns.map((column, index) => <th key={`${column}-${index}`} scope="col">{column}</th>)}</tr>
        </thead>
        <tbody>
          {intent.rows.map((row, index) => (
            <tr key={index}>{row.map((cell, cellIndex) => <td key={cellIndex}>{cell}</td>)}</tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function LinkCard({ intent }: { intent: Intent<"link"> }) {
  return (
    <p className={styles.link}>
      {intent.title ? <b>{intent.title}</b> : null}
      <a href={intent.url} target="_blank" rel="noreferrer noopener">{intent.url}</a>
    </p>
  );
}

function Web({ intent }: { intent: Intent<"web"> }) {
  return (
    <>
      {intent.url ? <p className={styles.meta}>Page <code>{intent.url}</code></p> : null}
      {intent.excerpt ? <pre className={styles.stream}>{intent.excerpt}</pre> : <p className={styles.meta}>The page rendered no visible text.</p>}
    </>
  );
}

function Generic({ intent }: { intent: Intent<"generic"> }) {
  return (
    <>
      <p className={styles.meta}>{intent.tool ? <><code>{intent.tool}</code> · </> : null}{intent.reason}</p>
      {intent.summary ? <pre className={styles.stream}>{intent.summary}</pre> : null}
    </>
  );
}
