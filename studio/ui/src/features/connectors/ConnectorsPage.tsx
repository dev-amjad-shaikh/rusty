import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useRef, useState } from "react";
import { StudioApiError } from "../../lib/api/client";
import {
  listConnectorInstances,
  listConnectorManifests,
  sweepConnectors,
  type ConnectorManifest,
  type SweepOutcome,
} from "../../lib/api/connectors";
import { PageHeader } from "../../components/PageHeader";
import { InstanceFleet } from "./InstanceFleet";
import { InstantiatePanel } from "./InstantiatePanel";
import { ManifestGallery } from "./ManifestGallery";
import styles from "./ConnectorsPage.module.css";

export function ConnectorsPage() {
  const queryClient = useQueryClient();

  const manifests = useQuery({
    queryKey: ["connectors", "manifests"],
    queryFn: () => listConnectorManifests(),
  });
  const instances = useQuery({
    queryKey: ["connectors", "instances"],
    queryFn: () => listConnectorInstances(),
  });

  const [instantiateHash, setInstantiateHash] = useState<string | null>(null);
  const instantiateTriggerRef = useRef<HTMLButtonElement | null>(null);
  const instantiateManifest: ConnectorManifest | null = instantiateHash
    ? manifests.data?.find((manifest) => manifest.hash === instantiateHash) ?? null
    : null;

  const [sweepOutcomes, setSweepOutcomes] = useState<SweepOutcome[] | null>(null);
  const sweep = useMutation({
    mutationFn: () => sweepConnectors(),
    onSuccess: (outcomes) => {
      setSweepOutcomes(outcomes);
      void queryClient.invalidateQueries({ queryKey: ["connectors", "instances"] });
    },
  });

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
  const summary = instances.isLoading || manifests.isLoading ? "Loading this workspace…"
    : instances.isError || manifests.isError ? "Connector evidence unavailable"
    : `${fleetCount} instance${fleetCount === 1 ? "" : "s"} · ${manifestCount} manifest${manifestCount === 1 ? "" : "s"} in this workspace`;

  return (
    <section className={`${styles.connectors} page`} aria-labelledby="connectors-heading">
      <PageHeader
        headingId="connectors-heading"
        eyebrow="Operate"
        title="Connectors"
        description={summary}
        actions={
          <button
            className="secondary-button"
            type="button"
            onClick={() => { setSweepOutcomes(null); sweep.mutate(); }}
            disabled={sweep.isPending}
          >
            {sweep.isPending ? "Sweeping…" : "Run health sweep"}
          </button>
        }
      />

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
          instances={instances}
          manifests={manifests.data ?? []}
        />

        <ManifestGallery
          manifests={manifests}
          onInstantiate={openInstantiate}
        />
      </>

      {instantiateManifest && (
        <InstantiatePanel
          manifest={instantiateManifest}
          onClose={closeInstantiate}
          onCreated={closeInstantiate}
        />
      )}
    </section>
  );
}
