import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useRef, useState, type FormEvent } from "react";
import { StudioApiError, type ConnectionIdentity } from "../../lib/api/client";
import {
  createConnectorInstance,
  listVaultConnections,
  slotNamedInError,
  type ConnectorInstance,
  type ConnectorManifest,
} from "../../lib/api/connectors";
import { providerKindLabel } from "./lifecycle";
import styles from "./ConnectorsPage.module.css";

export function InstantiatePanel({
  connection,
  scope,
  manifest,
  onClose,
  onCreated,
}: {
  connection: ConnectionIdentity;
  scope: string;
  manifest: ConnectorManifest;
  onClose: () => void;
  onCreated: (instance: ConnectorInstance) => void;
}) {
  const queryClient = useQueryClient();
  const headingRef = useRef<HTMLHeadingElement>(null);
  const [bindings, setBindings] = useState<Record<string, string>>({});
  const [slotError, setSlotError] = useState<{ slot: string | null; message: string } | null>(null);

  useEffect(() => {
    headingRef.current?.focus();
  }, []);

  const connections = useQuery({
    queryKey: [scope, "connections"],
    queryFn: () => listVaultConnections(connection),
  });

  const create = useMutation({
    mutationFn: () => createConnectorInstance(connection, {
      manifest_hash: manifest.hash,
      credentials: Object.fromEntries(Object.entries(bindings).filter(([, id]) => id)),
    }),
    onSuccess: async (instance) => {
      await queryClient.invalidateQueries({ queryKey: [scope, "connectors", "instances"] });
      onCreated(instance);
    },
    onError: (error) => {
      const message = error instanceof StudioApiError ? error.message : "The instance could not be created.";
      setSlotError({ slot: error instanceof StudioApiError ? slotNamedInError(error.message) : null, message });
    },
  });

  function submit(event: FormEvent) {
    event.preventDefault();
    setSlotError(null);
    create.mutate();
  }

  const vaultList = connections.data ?? [];
  const generalError = slotError && !slotError.slot ? slotError.message : null;

  return (
    <aside className={styles.instantiatePanel} aria-labelledby="instantiate-heading">
      <header className={styles.panelHead}>
        <div>
          <span className="eyebrow">Instantiate</span>
          <h2 id="instantiate-heading" ref={headingRef} tabIndex={-1}>{manifest.display_name}</h2>
        </div>
        <button className="secondary-button" type="button" onClick={onClose} disabled={create.isPending}>Close</button>
      </header>

      <dl className={styles.panelMeta}>
        <div><dt>Connector</dt><dd><code>{manifest.id}</code> · v{manifest.version}</dd></div>
        <div><dt>Provider</dt><dd>{providerKindLabel(manifest.provider.kind)}</dd></div>
        <div><dt>Pinned manifest hash</dt><dd><code title={manifest.hash}>{manifest.hash}</code></dd></div>
      </dl>
      <p className={styles.panelNote}>
        The instance pins this exact manifest by content hash. Each declared credential slot binds to an existing
        vault connection — the slot carries a connection id, never secret material. Raw secrets cannot be entered here.
      </p>

      <form onSubmit={submit}>
        {manifest.credential_slots.length === 0 ? (
          <p className={styles.quiet}>This manifest declares no credential slots.</p>
        ) : (
          <div className={styles.slotBindings}>
            {manifest.credential_slots.map((slot) => {
              const failed = slotError?.slot === slot.name;
              return (
                <div className={styles.slotRow} key={slot.name} data-invalid={failed || undefined}>
                  <label htmlFor={`slot-${slot.name}`}>
                    <code>{slot.name}</code>
                    {slot.description && <small>{slot.description}</small>}
                  </label>
                  {connections.isLoading ? (
                    <span className={styles.quiet}>Loading vault connections…</span>
                  ) : connections.isError ? (
                    <span className={styles.errorInline} role="alert">
                      {connections.error instanceof Error ? connections.error.message : "Vault connections could not be loaded."}
                    </span>
                  ) : vaultList.length === 0 ? (
                    <span className={styles.quiet}>No vault connections in this workspace. Register a connection first.</span>
                  ) : (
                    <select
                      id={`slot-${slot.name}`}
                      value={bindings[slot.name] ?? ""}
                      disabled={create.isPending}
                      aria-invalid={failed || undefined}
                      aria-describedby={failed ? `slot-${slot.name}-error` : undefined}
                      onChange={(event) => {
                        setBindings((current) => ({ ...current, [slot.name]: event.target.value }));
                        if (failed) setSlotError(null);
                      }}
                    >
                      <option value="">Choose a vault connection…</option>
                      {vaultList.map((record) => (
                        <option key={record.connection_id} value={record.connection_id}>
                          {record.connection_id} · {record.provider}{record.subject ? ` · ${record.subject}` : ""} · {record.status}
                        </option>
                      ))}
                    </select>
                  )}
                  {failed && (
                    <p className={styles.errorInline} role="alert" id={`slot-${slot.name}-error`}>{slotError.message}</p>
                  )}
                </div>
              );
            })}
          </div>
        )}

        {generalError && <p className={styles.error} role="alert">{generalError}</p>}
        {create.isError && !slotError && (
          <p className={styles.error} role="alert">
            {create.error instanceof StudioApiError ? create.error.message : "The instance could not be created."}
          </p>
        )}

        <div className={styles.formActions}>
          <button className="secondary-button" type="button" onClick={onClose} disabled={create.isPending}>Cancel</button>
          <button className="primary-button" type="submit" disabled={create.isPending || connections.isLoading}>
            {create.isPending ? "Instantiating…" : "Instantiate connector"}
          </button>
        </div>
      </form>
    </aside>
  );
}
