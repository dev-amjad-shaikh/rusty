import type { ConnectorManifest } from "../../lib/api/connectors";
import { hashPreview } from "./format";
import styles from "./ConnectorsPage.module.css";

/** The registered-connector gallery: one card per manifest, "Set up" opens
 * the generic schema form. */
export function ConnectorGallery({
  manifests,
  onSetup,
}: {
  manifests: ConnectorManifest[];
  onSetup: (manifest: ConnectorManifest) => void;
}) {
  if (manifests.length === 0) {
    return (
      <div className={styles.emptyState}>
        <div className={styles.emptyMark} aria-hidden="true">∅</div>
        <div>
          <h2>No connectors registered</h2>
          <p>
            Connector manifests register over the HTTP surface (<code>POST /connectors</code>).
            The demo server seeds the ServiceNow pack on boot.
          </p>
        </div>
      </div>
    );
  }
  return (
    <div className={styles.gallery}>
      {manifests.map((manifest) => (
        <article key={manifest.hash} className={styles.card}>
          <header>
            <h2>{manifest.display_name}</h2>
            <span className={`${styles.chip} ${styles.chipVersion}`}>v{manifest.version}</span>
          </header>
          <p>{manifest.description}</p>
          <div className={styles.cardMeta}>
            <span>{manifest.id}</span>
            <span>{manifest.operations.length} operations</span>
            <span title={manifest.hash}>{hashPreview(manifest.hash)}</span>
            <a href={manifest.documentation_url} target="_blank" rel="noreferrer">Docs</a>
          </div>
          <footer>
            <button className="primary-button" type="button" onClick={() => onSetup(manifest)}>
              Set up
            </button>
          </footer>
        </article>
      ))}
    </div>
  );
}
