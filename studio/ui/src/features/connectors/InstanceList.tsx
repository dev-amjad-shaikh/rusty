import { useState } from "react";
import { useMutation, useQuery } from "@tanstack/react-query";
import {
  checkConnectorInstance,
  getConnectorCatalog,
  type ConnectorInstance,
  type ConnectorManifest,
} from "../../lib/api/connectors";
import { flattenConfig, formatInstant, isSecretMarker } from "./format";
import styles from "./ConnectorsPage.module.css";

function CatalogDisclosure({ instanceId }: { instanceId: string }) {
  const [open, setOpen] = useState(false);
  const catalog = useQuery({
    queryKey: ["connector-catalog", instanceId],
    queryFn: () => getConnectorCatalog(instanceId),
    enabled: open,
    staleTime: 60_000,
  });
  return (
    <details
      className={styles.catalog}
      onToggle={(event) => setOpen(event.currentTarget.open)}
    >
      <summary>Tool catalog</summary>
      {open && catalog.isPending && <p className={styles.fieldHint}>Deriving tools…</p>}
      {open && catalog.isError && (
        <p className={styles.fieldError} role="alert">
          {catalog.error instanceof Error ? catalog.error.message : "The catalog could not be derived."}
        </p>
      )}
      {catalog.data && (
        <ul className={styles.toolList}>
          {catalog.data.tools.map((tool) => (
            <li key={tool.name}>
              <code>{tool.name}</code>
              <span className={`${styles.chip} ${styles.chipEffect}`}>{tool.effect}</span>
              <small>{tool.description}</small>
            </li>
          ))}
        </ul>
      )}
    </details>
  );
}

function InstanceCard({ instance, manifests }: { instance: ConnectorInstance; manifests: ConnectorManifest[] }) {
  const manifest = manifests.find((candidate) => candidate.hash === instance.manifest_hash);
  const recheck = useMutation({ mutationFn: () => checkConnectorInstance(instance.instance_id) });
  const rows = flattenConfig(instance.config);
  return (
    <article className={styles.instance}>
      <header>
        <b>{manifest ? manifest.display_name : "Unknown connector"}</b>
        <code>{instance.instance_id}</code>
        <time dateTime={instance.created_at}>{formatInstant(instance.created_at)}</time>
        <button
          className={styles.recheck}
          type="button"
          disabled={recheck.isPending}
          onClick={() => recheck.mutate()}
        >
          {recheck.isPending ? "Checking…" : "Re-check"}
        </button>
      </header>
      {recheck.data && (
        <p className={styles.recheckResult} data-ok={recheck.data.status === "succeeded"} role="status">
          {recheck.data.status === "succeeded"
            ? `Connection verified.${recheck.data.message ? ` ${recheck.data.message}` : ""}`
            : `Connection failed.${recheck.data.message ? ` ${recheck.data.message}` : ""}`}
        </p>
      )}
      {recheck.isError && (
        <p className={styles.recheckResult} data-ok="false" role="alert">
          {recheck.error instanceof Error ? recheck.error.message : "The check could not be run."}
        </p>
      )}
      <ul className={styles.configList}>
        {rows.map((row) => (
          <li key={row.path}>
            <b>{row.path}</b>
            {isSecretMarker(row.value)
              ? <span className={`${styles.chip} ${styles.chipSealed}`}>set — sealed</span>
              : <span>{String(row.value)}</span>}
          </li>
        ))}
      </ul>
      <CatalogDisclosure instanceId={instance.instance_id} />
    </article>
  );
}

/** The live instances: non-secret config visible, secrets masked, per-instance
 * re-check and the derived tool catalog. */
export function InstanceList({
  instances,
  manifests,
}: {
  instances: ConnectorInstance[];
  manifests: ConnectorManifest[];
}) {
  if (instances.length === 0) {
    return (
      <div className={styles.emptyState}>
        <div className={styles.emptyMark} aria-hidden="true">∅</div>
        <div>
          <h2>No connections yet</h2>
          <p>Set up a connector from the gallery — a successful test connection is the gate, then save.</p>
        </div>
      </div>
    );
  }
  return (
    <div className={styles.instanceList}>
      {instances.map((instance) => (
        <InstanceCard key={instance.instance_id} instance={instance} manifests={manifests} />
      ))}
    </div>
  );
}
