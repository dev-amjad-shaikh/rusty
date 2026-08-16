import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useRef, useState } from "react";
import { connectionScope, StudioApiError } from "../../lib/api/client";
import {
  listConnectorInstances,
  listConnectorManifests,
  sweepConnectors,
  type ConnectorManifest,
  type SweepOutcome,
} from "../../lib/api/connectors";
import { useConnectionStore } from "../../state/connection";
import { PageHeader } from "../../components/PageHeader";
import { InstanceFleet } from "./InstanceFleet";
import { InstantiatePanel } from "./InstantiatePanel";
import { ManifestGallery } from "./ManifestGallery";
import styles from "./ConnectorsPage.module.css";

export function ConnectorsPage() {
  const { connection, openDialog } = useConnectionStore();
  const queryClient = useQueryClient();
  const scope = connection ? connectionScope(connection) : "disconnected";

  const manifests = useQuery({
    queryKey: [scope, "connectors", "manifests"],
    queryFn: () => listConnectorManifests(connection!),
    enabled: Boolean(connection),
  });
  const instances = useQuery({
    queryKey: [scope, "connectors", "instances"],
    queryFn: () => listConnectorInstances(connection!),
    enabled: Boolean(connection),
  });

  const [instantiateHash, setInstantiateHash] = useState<string | null>(null);
  const instantiateTriggerRef = useRef<HTMLButtonElement | null>(null);
  const instantiateManifest: ConnectorManifest | null = instantiateHash
    ? manifests.data?.find((manifest) => manifest.hash === instantiateHash) ?? null
    : null;

  const [sweepOutcomes, setSweepOutcomes] = useState<SweepOutcome[] | null>(null);
  const sweep = useMutation({
    mutationFn: () => sweepConnectors(connection!),
    onSuccess: (outcomes) => {
      setSweepOutcomes(outcomes);
      void queryClient.invalidateQueries({ queryKey: [scope, "connectors", "instances"] });
    },
  });

  // Connection epoch changes retire panel and sweep evidence from the
  // previous workspace; nothing carries across the boundary.
  useEffect(() => {
    setInstantiateHash(null);
    setSweepOutcomes(null);
    sweep.reset();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [scope]);

  function openInstantiate(hash: string, trigger: HTMLButtonElement) {
    instantiateTriggerRef.current = trigger;
    setInstantiateHash(hash);
  }

  function closeInstantiate() {
    setInstantiateHash(null);
    requestAnimationFrame(() => instantiateTriggerRef.current?.focus());
  }

  const fleetCount = instances.data?.length;
  const manifestCount = manifests.data?.length;
  const summary = !connection ? "Open a workspace to manage connectors."
    : instances.isLoading || manifests.isLoading ? "Loading this workspace…"
    : instances.isError || manifests.isError ? "Connector evidence unavailable"
    : `${fleetCount} instance${fleetCount === 1 ? "" : "s"} · ${manifestCount} manifest${manifestCount === 1 ? "" : "s"} in this workspace`;

  return (
    <section className={`${styles.connectors} page`} aria-labelledby="connectors-heading">
      <PageHeader
        headingId="connectors-heading"
        eyebrow="Operate"
        title="Connectors"
        description={summary}
        actions={!connection ? (
          <button className="primary-button" type="button" onClick={openDialog}>Choose workspace</button>
        ) : (
          <button
            className="secondary-button"
            type="button"
            onClick={() => { setSweepOutcomes(null); sweep.mutate(); }}
            disabled={sweep.isPending}
          >
            {sweep.isPending ? "Sweeping…" : "Run health sweep"}
          </button>
        )}
      />

      {!connection ? (
        <div className="empty-state">
          <span className="eyebrow">Connector plane</span>
          <h2>Open a workspace to work with connectors</h2>
          <p>Manifests, instances, and served catalogs are tenant-scoped. Credentials bind to vault connections — raw secrets never enter this screen.</p>
        </div>
      ) : (
        <>
          {sweep.isError && (
            <p className={styles.error} role="alert">
              {sweep.error instanceof StudioApiError ? sweep.error.message : "The health sweep could not be completed."}
            </p>
          )}
          {sweepOutcomes && (
            <section className={styles.sweepResults} aria-labelledby="sweep-heading" aria-live="polite">
              <header>
                <span className="eyebrow">Health sweep</span>
                <h2 id="sweep-heading">
                  {sweepOutcomes.length ? `${sweepOutcomes.length} instance${sweepOutcomes.length === 1 ? "" : "s"} re-checked` : "Nothing to re-check"}
                </h2>
              </header>
              {sweepOutcomes.length ? (
                <ol className={styles.sweepList}>
                  {sweepOutcomes.map((outcome) => (
                    <li key={outcome.instance_id}>
                      <code>{outcome.instance_id}</code>
                      <span className={styles.sweepTransition}>
                        {outcome.previous_state.state} → {outcome.current_state.state}
                      </span>
                      {outcome.current_state.reason && <span className={styles.sweepReason}>{outcome.current_state.reason}</span>}
                      {outcome.catalog_bumped && <span className={styles.sweepBump}>catalog bumped</span>}
                    </li>
                  ))}
                </ol>
              ) : (
                <p className={styles.sweepQuiet}>No healthy or degraded instances. Pending, failed, and disabled instances are not swept.</p>
              )}
            </section>
          )}

          <InstanceFleet
            connection={connection}
            scope={scope}
            instances={instances}
            manifests={manifests.data ?? []}
          />

          <ManifestGallery
            connection={connection}
            scope={scope}
            manifests={manifests}
            onInstantiate={openInstantiate}
          />
        </>
      )}

      {connection && instantiateManifest && (
        <InstantiatePanel
          connection={connection}
          scope={scope}
          manifest={instantiateManifest}
          onClose={closeInstantiate}
          onCreated={closeInstantiate}
        />
      )}
    </section>
  );
}
