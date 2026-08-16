import { useQuery } from "@tanstack/react-query";
import { useState, type FormEvent } from "react";
import { StudioApiError, type ConnectionIdentity } from "../../lib/api/client";
import {
  getInstanceCatalog,
  liveGenerationInError,
  type ConnectorInstance,
} from "../../lib/api/connectors";
import { evidencePreview } from "../../lib/text";
import { effectLabel, shortHash } from "./lifecycle";
import styles from "./ConnectorsPage.module.css";

export function CatalogView({
  connection,
  scope,
  instance,
}: {
  connection: ConnectionIdentity;
  scope: string;
  instance: ConnectorInstance;
}) {
  const [pin, setPin] = useState<number | null>(null);
  const [pinDraft, setPinDraft] = useState("");

  const catalog = useQuery({
    queryKey: [scope, "connectors", "catalog", instance.instance_id, pin ?? "live"],
    queryFn: () => getInstanceCatalog(connection, instance.instance_id, pin ?? undefined),
  });

  const mismatch = catalog.error instanceof StudioApiError && catalog.error.status === 409
    ? { message: catalog.error.message, live: liveGenerationInError(catalog.error.message) }
    : null;
  const preCatalog = catalog.error instanceof StudioApiError && catalog.error.status === 404;

  function submitPin(event: FormEvent) {
    event.preventDefault();
    const value = Number(pinDraft);
    if (Number.isSafeInteger(value) && value > 0) setPin(value);
  }

  return (
    <section className={styles.catalog} aria-label={`Catalog for ${instance.instance_id}`}>
      {catalog.isLoading ? (
        <p className={styles.quiet} role="status">Loading catalog…</p>
      ) : mismatch ? (
        <div className={styles.pinMismatch} role="alert">
          <p>{mismatch.message}</p>
          <div className={styles.formActions}>
            {mismatch.live !== null && (
              <button type="button" className="primary-button" onClick={() => { setPin(mismatch.live); setPinDraft(String(mismatch.live)); }}>
                Load live generation {mismatch.live}
              </button>
            )}
            <button type="button" className="secondary-button" onClick={() => { setPin(null); setPinDraft(""); }}>
              Load current catalog
            </button>
          </div>
        </div>
      ) : preCatalog ? (
        <div className={styles.preCatalog}>
          <p>{catalog.error instanceof Error ? catalog.error.message : "This instance has served no catalog yet."}</p>
        </div>
      ) : catalog.isError ? (
        <p className={styles.error} role="alert">
          {catalog.error instanceof Error ? catalog.error.message : "The catalog could not be loaded."}
        </p>
      ) : catalog.data ? (
        <>
          <header className={styles.catalogHead}>
            <div>
              <span className={styles.generation}>generation {catalog.data.generation}</span>
              <code title={catalog.data.hash}>{shortHash(catalog.data.hash)}</code>
            </div>
            <form className={styles.pinForm} onSubmit={submitPin}>
              <label htmlFor={`pin-${instance.instance_id}`}>Pin generation</label>
              <input
                id={`pin-${instance.instance_id}`}
                inputMode="numeric"
                pattern="[0-9]*"
                value={pinDraft}
                onChange={(event) => setPinDraft(event.target.value)}
                placeholder={String(catalog.data.generation)}
              />
              <button type="submit" className="secondary-button" disabled={!pinDraft.trim()}>Load pinned</button>
              {pin !== null && (
                <button type="button" className="secondary-button" onClick={() => { setPin(null); setPinDraft(""); }}>Unpin</button>
              )}
            </form>
          </header>
          {catalog.data.tools.length ? (
            <ul className={styles.toolList}>
              {catalog.data.tools.map((tool) => (
                <li key={tool.name} className={styles.toolRow}>
                  <div>
                    <code>{evidencePreview(tool.name, 128)}</code>
                    <span className={styles.effectBadge} data-effect={tool.effect}>{effectLabel(tool.effect)}</span>
                  </div>
                  <p>{evidencePreview(tool.description, 500)}</p>
                </li>
              ))}
            </ul>
          ) : (
            <p className={styles.quiet}>This generation serves no tools.</p>
          )}
          <p className={styles.refreshNote}>Generations bump only when the derived catalog bytes change; an identical refresh keeps the current generation.</p>
        </>
      ) : null}
    </section>
  );
}
