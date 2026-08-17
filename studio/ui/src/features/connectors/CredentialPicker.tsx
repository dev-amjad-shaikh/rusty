import type { FormEvent } from "react";
import { type ConnectorManifest, type VaultConnection } from "../../lib/api/connectors";
import { connectionLabel, credentialFlow, usableConnections } from "./lifecycle";
import styles from "./InstantiatePanel.module.css";

export interface CredentialPickerProps {
  manifest: ConnectorManifest;
  connections: VaultConnection[];
  bindings: Record<string, string>;
  slotError: { slot: string | null; message: string } | null;
  generalError: string | null;
  createError: string | null;
  pending: boolean;
  canSwitchToEntry: boolean;
  onBind: (slot: string, connectionId: string) => void;
  onClearSlotError: () => void;
  onSwitchToEntry: () => void;
  onCancel: () => void;
  onSubmit: () => void;
}

export function CredentialPicker({
  manifest,
  connections,
  bindings,
  slotError,
  generalError,
  createError,
  pending,
  canSwitchToEntry,
  onBind,
  onClearSlotError,
  onSwitchToEntry,
  onCancel,
  onSubmit,
}: CredentialPickerProps) {
  const flow = credentialFlow(manifest);
  const usable = usableConnections(manifest, connections);

  function submit(event: FormEvent) {
    event.preventDefault();
    onSubmit();
  }

  return (
    <form onSubmit={submit}>
      <div className={styles.slotBindings}>
        {manifest.credential_slots.map((slot) => {
          const failed = slotError?.slot === slot.name;
          return (
            <div className={styles.slotRow} key={slot.name} data-invalid={failed || undefined}>
              <label htmlFor={`slot-${slot.name}`}>
                <code>{slot.name}</code>
                {slot.description && <small>{slot.description}</small>}
              </label>
              {usable.length === 0 ? (
                <span className={styles.quiet}>
                  No usable vault connections in this workspace — every record is revoked, expired,
                  or of another auth kind.
                </span>
              ) : (
                <select
                  id={`slot-${slot.name}`}
                  value={bindings[slot.name] ?? ""}
                  disabled={pending}
                  aria-invalid={failed || undefined}
                  aria-describedby={failed ? `slot-${slot.name}-error` : undefined}
                  onChange={(event) => {
                    onBind(slot.name, event.target.value);
                    if (failed) onClearSlotError();
                  }}
                >
                  <option value="">Choose a vault connection…</option>
                  {usable.map((record) => (
                    <option key={record.connection_id} value={record.connection_id}>
                      {connectionLabel(record)}
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

      {generalError && <p className={styles.error} role="alert">{generalError}</p>}
      {createError && <p className={styles.error} role="alert">{createError}</p>}

      <div className={styles.formActions}>
        {canSwitchToEntry && (
          <button className="secondary-button" type="button" onClick={onSwitchToEntry} disabled={pending}>
            Connect new credentials instead
          </button>
        )}
        <button className="secondary-button" type="button" onClick={onCancel} disabled={pending}>Cancel</button>
        <button className="primary-button" type="submit" disabled={pending || usable.length === 0}>
          {pending ? "Instantiating…" : "Instantiate connector"}
        </button>
      </div>
    </form>
  );
}
