import { useMutation, useQueryClient, type UseQueryResult } from "@tanstack/react-query";
import { useState } from "react";
import { StudioApiError } from "../../lib/api/client";
import {
  checkConnectorInstanceHealth,
  connectConnectorInstance,
  disableConnectorInstance,
  enableConnectorInstance,
  type ConnectorInstance,
  type ConnectorManifest,
} from "../../lib/api/connectors";
import { evidencePreview } from "../../lib/text";
import { CatalogView } from "./CatalogView";
import { allowedActions, healthCheckTime, shortHash, statePresentation } from "./lifecycle";
import styles from "./ConnectorsPage.module.css";

type LifecycleAction = "connect" | "health" | "disable" | "enable";

function InstanceRow({
  instance,
  manifestName,
}: {
  instance: ConnectorInstance;
  manifestName: string | null;
}) {
  const queryClient = useQueryClient();
  const [catalogOpen, setCatalogOpen] = useState(false);
  const presentation = statePresentation[instance.state];
  const actions = allowedActions(instance.state);

  const refresh = () => queryClient.invalidateQueries({ queryKey: ["connectors", "instances"] });

  const action = useMutation({
    mutationFn: async (kind: LifecycleAction) => {
      if (kind === "connect") return connectConnectorInstance(instance.instance_id);
      if (kind === "health") return checkConnectorInstanceHealth(instance.instance_id);
      if (kind === "disable") return disableConnectorInstance(instance.instance_id);
      return enableConnectorInstance(instance.instance_id);
    },
    onSuccess: refresh,
  });

  const pending = action.isPending ? action.variables : null;
  const errorMessage = action.error instanceof StudioApiError ? action.error.message
    : action.error ? "The action could not be completed."
    : null;

  return (
    <article className={styles.row} role="row" data-state={instance.state}>
      <div className={styles.instanceIdentity} role="cell">
        <div>
          <b>{manifestName ?? evidencePreview(instance.connector_id, 64)}</b>
          <small><code>{instance.instance_id}</code> · {evidencePreview(instance.connector_id, 64)}</small>
          {Object.keys(instance.config).length > 0 && (
            <small>
              <code>{Object.entries(instance.config).map(([name, value]) => `${name}=${value}`).join(" ")}</code>
            </small>
          )}
        </div>
      </div>
      <div role="cell" data-label="State">
        <span className={styles.statePill} data-tone={presentation.tone} title={presentation.summary}>
          {presentation.label}
        </span>
        {instance.state_reason && <small className={styles.stateReason}>{evidencePreview(instance.state_reason, 512)}</small>}
      </div>
      <code role="cell" data-label="Manifest" title={instance.manifest_hash}>{shortHash(instance.manifest_hash)}</code>
      <span role="cell" data-label="Catalog">
        {instance.catalog_generation === null ? <span className={styles.quiet}>No catalog served</span> : `gen ${instance.catalog_generation}`}
      </span>
      <span role="cell" data-label="Last health">{healthCheckTime(instance.last_health_check_ms)}</span>
      <div className={styles.rowActions} role="cell">
        {actions.connect && (
          <button type="button" className="secondary-button" disabled={action.isPending}
            onClick={() => action.mutate("connect")}>
            {pending === "connect" ? "Connecting…" : instance.state === "pending" ? "Connect" : "Reconnect"}
          </button>
        )}
        {actions.health && (
          <button type="button" className="secondary-button" disabled={action.isPending}
            onClick={() => action.mutate("health")}>
            {pending === "health" ? "Checking…" : "Health check"}
          </button>
        )}
        {actions.disable && (
          <button type="button" className="secondary-button" disabled={action.isPending}
            onClick={() => action.mutate("disable")}>
            {pending === "disable" ? "Disabling…" : "Disable"}
          </button>
        )}
        {actions.enable && (
          <button type="button" className="secondary-button" disabled={action.isPending}
            onClick={() => action.mutate("enable")}>
            {pending === "enable" ? "Enabling…" : "Enable"}
          </button>
        )}
        <button
          type="button"
          className="secondary-button"
          aria-expanded={catalogOpen}
          aria-controls={`catalog-${instance.instance_id}`}
          onClick={() => setCatalogOpen((open) => !open)}
        >
          Catalog
        </button>
      </div>
      {errorMessage && (
        <p className={styles.rowError} role="alert">{errorMessage}</p>
      )}
      {catalogOpen && (
        <div className={styles.catalogSlot} id={`catalog-${instance.instance_id}`}>
          <CatalogView instance={instance} />
        </div>
      )}
    </article>
  );
}

export function InstanceFleet({
  instances,
  manifests,
}: {
  instances: UseQueryResult<ConnectorInstance[]>;
  manifests: ConnectorManifest[];
}) {
  const namesByHash = new Map(manifests.map((manifest) => [manifest.hash, manifest.display_name]));
  const list = instances.data ?? [];

  return (
    <section className={styles.section} aria-labelledby="instance-fleet-heading">
      <header className={styles.sectionHead}>
        <div>
          <span className="eyebrow">Instance fleet</span>
          <h2 id="instance-fleet-heading">Instances</h2>
          <p>Live sessions are not durable — after a server restart every instance returns to pending (or failed, when its credentials no longer resolve) and reconnects on demand.</p>
        </div>
      </header>

      {instances.isLoading ? (
        <div className={styles.loading} role="status">Loading instances…</div>
      ) : instances.isError ? (
        <div className={styles.error} role="alert">
          {instances.error instanceof Error ? instances.error.message : "Instances could not be loaded."}
        </div>
      ) : list.length ? (
        <div className={styles.table} role="table" aria-label="Connector instances">
          <div className={styles.tableHead} role="row">
            <span role="columnheader">Instance</span>
            <span role="columnheader">State</span>
            <span role="columnheader">Manifest</span>
            <span role="columnheader">Catalog</span>
            <span role="columnheader">Last health</span>
            <span role="columnheader"><span className="sr-only">Actions</span></span>
          </div>
          {list.map((instance) => (
            <InstanceRow
              key={instance.instance_id}
             
             
              instance={instance}
              manifestName={namesByHash.get(instance.manifest_hash) ?? null}
            />
          ))}
          <footer>{list.length} instance{list.length === 1 ? "" : "s"}</footer>
        </div>
      ) : (
        <div className={styles.emptyInline}>
          <h3>No instances yet</h3>
          <p>Instantiate a registered manifest below. Each declared credential slot binds to a vault connection id — secrets stay in the vault.</p>
        </div>
      )}
    </section>
  );
}
