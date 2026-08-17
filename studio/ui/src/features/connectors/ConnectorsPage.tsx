import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { PageHeader } from "../../components/PageHeader";
import { listConnectorInstances, listConnectors, type ConnectorManifest } from "../../lib/api/connectors";
import { ConnectorGallery } from "./ConnectorGallery";
import { ConnectorSetup } from "./ConnectorSetup";
import { InstanceList } from "./InstanceList";
import styles from "./ConnectorsPage.module.css";

type ConnectorsView =
  | { tab: "gallery" }
  | { tab: "instances" }
  | { tab: "setup"; manifest: ConnectorManifest };

const mainTabs = [
  { tab: "gallery" as const, label: "Connectors" },
  { tab: "instances" as const, label: "Connections" },
];

export function ConnectorsPage() {
  const [view, setView] = useState<ConnectorsView>({ tab: "gallery" });
  const activeTab = view.tab === "setup" ? "gallery" : view.tab;

  const manifests = useQuery({ queryKey: ["connectors"], queryFn: listConnectors });
  const instances = useQuery({ queryKey: ["connector-instances"], queryFn: listConnectorInstances });

  return (
    <section className={`${styles.connectors} page`} aria-labelledby="connectors-heading">
      {view.tab === "setup" ? (
        <PageHeader
          variant="compact"
          headingId="connectors-heading"
          eyebrow="Build · Connectors"
          title={`Set up ${view.manifest.display_name}`}
          description="Fields derive from the connector's own schema — test the connection, then save."
          actions={<button className={styles.backButton} type="button" onClick={() => setView({ tab: "gallery" })}>
            ← Back to connectors
          </button>}
        />
      ) : (
        <PageHeader
          headingId="connectors-heading"
          eyebrow="Build"
          title="Connectors"
          description="Schema-driven connectors — the manifest's JSON Schema is the entire setup surface."
        />
      )}

      {view.tab !== "setup" && (
        <nav className={styles.tabs} aria-label="Connector sections">
          {mainTabs.map(({ tab, label }) => (
            <button
              key={tab}
              type="button"
              className={`${styles.tab} ${activeTab === tab ? styles.activeTab : ""}`}
              aria-current={activeTab === tab ? "page" : undefined}
              onClick={() => setView({ tab })}
            >{label}</button>
          ))}
        </nav>
      )}

      {view.tab === "setup" ? (
        <ConnectorSetup
          manifest={view.manifest}
          onDone={() => setView({ tab: "instances" })}
          onCancel={() => setView({ tab: "gallery" })}
        />
      ) : manifests.isPending ? (
        <div className={styles.loading}>Loading connectors…</div>
      ) : manifests.isError ? (
        <div className={styles.panel}>
          <h2>Connectors could not be loaded</h2>
          <p className={styles.error} role="alert">
            {manifests.error instanceof Error ? manifests.error.message : "The connector surface did not answer."}
          </p>
          <div className={styles.formActions}>
            <button className="secondary-button" type="button" onClick={() => manifests.refetch()}>Retry</button>
          </div>
        </div>
      ) : view.tab === "instances" ? (
        instances.isPending ? (
          <div className={styles.loading}>Loading connections…</div>
        ) : instances.isError ? (
          <div className={styles.panel}>
            <h2>Connections could not be loaded</h2>
            <p className={styles.error} role="alert">
              {instances.error instanceof Error ? instances.error.message : "The connector surface did not answer."}
            </p>
            <div className={styles.formActions}>
              <button className="secondary-button" type="button" onClick={() => instances.refetch()}>Retry</button>
            </div>
          </div>
        ) : (
          <InstanceList instances={instances.data} manifests={manifests.data} />
        )
      ) : (
        <ConnectorGallery manifests={manifests.data} onSetup={(manifest) => setView({ tab: "setup", manifest })} />
      )}
    </section>
  );
}
