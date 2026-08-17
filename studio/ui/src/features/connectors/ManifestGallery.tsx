import { useState } from "react";
import type { UseQueryResult } from "@tanstack/react-query";
import type { ConnectorManifest } from "../../lib/api/connectors";
import { evidencePreview } from "../../lib/text";
import { providerKindLabel, shortHash } from "./lifecycle";
import { RegisterManifestForm } from "./RegisterManifestForm";
import styles from "./ManifestGallery.module.css";

export function ManifestGallery({
  manifests,
  onInstantiate,
}: {
  manifests: UseQueryResult<ConnectorManifest[]>;
  onInstantiate: (hash: string, trigger: HTMLButtonElement) => void;
}) {
  const [registerOpen, setRegisterOpen] = useState(false);
  const list = manifests.data ?? [];

  return (
    <section className={styles.section} aria-labelledby="manifest-gallery-heading">
      <header className={styles.sectionHead}>
        <div>
          <span className="eyebrow">Manifest gallery</span>
          <h2 id="manifest-gallery-heading">Registered manifests</h2>
          <p>Content-addressed connector contracts. Re-posting the same bytes converges; different bytes register a new entry — there is no update path.</p>
        </div>
        <button
          className="secondary-button"
          type="button"
          aria-expanded={registerOpen}
          aria-controls="register-manifest"
          onClick={() => setRegisterOpen((open) => !open)}
        >
          {registerOpen ? "Close register" : "Register manifest"}
        </button>
      </header>

      {registerOpen && (
        <RegisterManifestForm onRegistered={() => setRegisterOpen(false)} />
      )}

      {manifests.isLoading ? (
        <div className={styles.loading} role="status">Loading manifests…</div>
      ) : manifests.isError ? (
        <div className={styles.error} role="alert">
          {manifests.error instanceof Error ? manifests.error.message : "Manifests could not be loaded."}
        </div>
      ) : list.length ? (
        <div className={styles.table} role="table" aria-label="Registered connector manifests">
          <div className={styles.tableHead} role="row">
            <span role="columnheader">Connector</span>
            <span role="columnheader">Provider</span>
            <span role="columnheader">Credential slots</span>
            <span role="columnheader">Content hash</span>
            <span role="columnheader"><span className="sr-only">Actions</span></span>
          </div>
          {list.map((manifest) => (
            <article className={styles.row} role="row" key={manifest.hash}>
              <div className={styles.manifestIdentity} role="cell">
                <div>
                  <b>{evidencePreview(manifest.display_name, 128)}</b>
                  <small>{evidencePreview(manifest.description, 500)}</small>
                </div>
                <code>{manifest.id} · v{manifest.version}</code>
              </div>
              <div role="cell" data-label="Provider">
                <span className={styles.providerBadge}>{providerKindLabel(manifest.provider.kind)}</span>
              </div>
              <div role="cell" data-label="Credential slots">
                {manifest.credential_slots.length ? (
                  <ul className={styles.slotList}>
                    {manifest.credential_slots.map((slot) => (
                      <li key={slot.name} title={slot.description}><code>{slot.name}</code></li>
                    ))}
                  </ul>
                ) : (
                  <span className={styles.quiet}>None declared</span>
                )}
              </div>
              <code role="cell" data-label="Content hash" title={manifest.hash}>{shortHash(manifest.hash)}</code>
              <div className={styles.rowActions} role="cell">
                <button
                  className="secondary-button"
                  type="button"
                  onClick={(event) => onInstantiate(manifest.hash, event.currentTarget)}
                >
                  Instantiate
                </button>
              </div>
            </article>
          ))}
          <footer>{list.length} manifest{list.length === 1 ? "" : "s"}</footer>
        </div>
      ) : (
        <div className={styles.emptyInline}>
          <h3>No manifests registered</h3>
          <p>Register a manifest to declare what a connector provides and which credential slots an instance needs.</p>
          <button className="primary-button" type="button" onClick={() => setRegisterOpen(true)}>Register first manifest</button>
        </div>
      )}
    </section>
  );
}
