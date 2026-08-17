import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useRef, useState, type FormEvent } from "react";
import { StudioApiError } from "../../lib/api/client";
import {
  configParamNamedInError,
  createConnectorInstance,
  listVaultConnections,
  slotNamedInError,
  type ConnectorInstance,
  type ConnectorManifest,
} from "../../lib/api/connectors";
import { connectionLabel, credentialFlow, providerKindLabel, resolvedBaseUrlPreview, usableConnections } from "./lifecycle";
import { RegisterConnectionForm } from "./RegisterConnectionForm";
import styles from "./ConnectorsPage.module.css";

/// Field label for a config param: `instance_type` reads as "Instance type".
function configParamLabel(name: string) {
  const words = name.replace(/_/g, " ");
  return words.charAt(0).toUpperCase() + words.slice(1);
}

export function InstantiatePanel({
  manifest,
  onClose,
  onCreated,
}: {
  manifest: ConnectorManifest;
  onClose: () => void;
  onCreated: (instance: ConnectorInstance) => void;
}) {
  const queryClient = useQueryClient();
  const headingRef = useRef<HTMLHeadingElement>(null);
  const [bindings, setBindings] = useState<Record<string, string>>({});
  const [slotError, setSlotError] = useState<{ slot: string | null; message: string } | null>(null);
  const [config, setConfig] = useState<Record<string, string>>({});
  const [configError, setConfigError] = useState<{ param: string | null; message: string } | null>(null);
  // `null` lets the vault answer decide: usable connections open the
  // picker, an empty vault drops straight into credential entry.
  const [modeOverride, setModeOverride] = useState<"pick" | "new" | null>(null);
  const [chaining, setChaining] = useState(false);

  useEffect(() => {
    headingRef.current?.focus();
  }, []);

  const connections = useQuery({
    queryKey: ["connections"],
    queryFn: () => listVaultConnections(),
  });

  const flow = credentialFlow(manifest);
  const usable = usableConnections(manifest, connections.data ?? []);
  const mode = modeOverride ?? (usable.length > 0 ? "pick" : "new");

  const create = useMutation({
    mutationFn: (input: { credentials: Record<string, string>; config: Record<string, string> }) =>
      createConnectorInstance({ manifest_hash: manifest.hash, credentials: input.credentials, config: input.config }),
    onSuccess: async (instance) => {
      await queryClient.invalidateQueries({ queryKey: ["connectors", "instances"] });
      onCreated(instance);
    },
    onError: (error) => {
      const message = error instanceof StudioApiError ? error.message : "The instance could not be created.";
      if (error instanceof StudioApiError) {
        const configParam = configParamNamedInError(error.message);
        if (configParam) setConfigError({ param: configParam, message });
        else setSlotError({ slot: slotNamedInError(error.message), message });
      } else {
        setSlotError({ slot: null, message });
      }
      // A credential-entry chain that fails at instantiation keeps the
      // fresh bindings: the picker shows them, and retrying is one click.
      setChaining(false);
      setModeOverride("pick");
    },
  });

  // Config values go out trimmed and limited to the manifest's declared
  // params — the server answers any other shape with a 422.
  function trimmedConfig(): Record<string, string> {
    return Object.fromEntries(
      manifest.config_params.map((param) => [param.name, (config[param.name] ?? "").trim()]),
    );
  }

  // Every declared config param needs a value before either submit path
  // runs; the server would refuse an empty one anyway.
  function gateConfig(): boolean {
    setConfigError(null);
    for (const param of manifest.config_params) {
      if (!(config[param.name] ?? "").trim()) {
        setConfigError({ param: param.name, message: `config param \`${param.name}\` requires a value` });
        return false;
      }
    }
    return true;
  }

  // Registration and instantiation in one motion: the form's bindings go
  // straight into the create call, never through an id the operator reads.
  function onConnectionRegistered(freshBindings: Record<string, string>) {
    setSlotError(null);
    if (!gateConfig()) return;
    setChaining(true);
    setBindings((current) => {
      const next = { ...current };
      for (const slot of manifest.credential_slots) {
        if (!next[slot.name] && freshBindings[slot.name]) next[slot.name] = freshBindings[slot.name];
      }
      return next;
    });
    create.mutate({ credentials: freshBindings, config: trimmedConfig() });
  }

  function submit(event: FormEvent) {
    event.preventDefault();
    setSlotError(null);
    if (!gateConfig()) return;
    create.mutate({
      credentials: Object.fromEntries(Object.entries(bindings).filter(([, id]) => id)),
      config: trimmedConfig(),
    });
  }

  const pending = chaining || create.isPending;
  const generalError = slotError && !slotError.slot ? slotError.message : null;
  const baseUrlPreview = resolvedBaseUrlPreview(manifest, config);

  return (
    <aside className={styles.instantiatePanel} aria-labelledby="instantiate-heading">
      <header className={styles.panelHead}>
        <div>
          <span className="eyebrow">Instantiate</span>
          <h2 id="instantiate-heading" ref={headingRef} tabIndex={-1}>{manifest.display_name}</h2>
        </div>
        <button className="secondary-button" type="button" onClick={onClose} disabled={pending}>Close</button>
      </header>

      <dl className={styles.panelMeta}>
        <div><dt>Connector</dt><dd><code>{manifest.id}</code> · v{manifest.version}</dd></div>
        <div><dt>Provider</dt><dd>{providerKindLabel(manifest.provider.kind)}</dd></div>
        <div><dt>Pinned manifest hash</dt><dd><code title={manifest.hash}>{manifest.hash}</code></dd></div>
      </dl>
      <p className={styles.panelNote}>
        The instance pins this exact manifest by content hash. Each declared credential slot binds to a
        vault connection — the slot carries a connection id, never secret material. Credentials entered
        here cross the form once and seal into the vault first.
      </p>

      {manifest.config_params.length > 0 && (
        <div className={styles.slotBindings} aria-label="Instance configuration">
          {manifest.config_params.map((param) => {
            const failed = configError?.param === param.name;
            return (
              <div className={styles.grantField} key={param.name}>
                <label htmlFor={`config-${param.name}`}>{configParamLabel(param.name)}</label>
                <input
                  id={`config-${param.name}`}
                  type="text"
                  value={config[param.name] ?? ""}
                  autoComplete="off"
                  spellCheck={false}
                  disabled={pending}
                  aria-invalid={failed || undefined}
                  aria-describedby={failed ? `config-${param.name}-error` : undefined}
                  onChange={(event) => {
                    setConfig((current) => ({ ...current, [param.name]: event.target.value }));
                    if (failed) setConfigError(null);
                  }}
                />
                {param.description && <small>{param.description}</small>}
              </div>
            );
          })}
          {baseUrlPreview && (
            <p className={styles.quiet}>This instance will call <code>{baseUrlPreview}</code></p>
          )}
          {configError && (
            <p
              className={styles.errorInline}
              role="alert"
              id={configError.param ? `config-${configError.param}-error` : undefined}
            >
              {configError.message}
            </p>
          )}
        </div>
      )}

      {manifest.credential_slots.length === 0 ? (
        <form onSubmit={submit}>
          <p className={styles.quiet}>This manifest declares no credential slots.</p>
          <div className={styles.formActions}>
            <button className="secondary-button" type="button" onClick={onClose} disabled={pending}>Cancel</button>
            <button className="primary-button" type="submit" disabled={pending}>
              {pending ? "Instantiating…" : "Instantiate connector"}
            </button>
          </div>
        </form>
      ) : connections.isLoading ? (
        <p className={styles.quiet} role="status">Loading vault connections…</p>
      ) : connections.isError ? (
        <p className={styles.errorInline} role="alert">
          {connections.error instanceof Error ? connections.error.message : "Vault connections could not be loaded."}
        </p>
      ) : mode === "new" && flow.kind !== "picker" ? (
        <>
          {usable.length === 0 && (
            <p className={styles.quiet}>
              No usable connection in this workspace yet — enter the credentials once. Rusty seals them
              into the vault, binds the slots, and instantiates.
            </p>
          )}
          <RegisterConnectionForm
            manifest={manifest}
            chaining={pending}
            onRegistered={onConnectionRegistered}
            onCancel={() => (usable.length > 0 ? setModeOverride("pick") : onClose())}
            beforeSubmit={gateConfig}
          />
        </>
      ) : (
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
                        setBindings((current) => ({ ...current, [slot.name]: event.target.value }));
                        if (failed) setSlotError(null);
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
          {create.isError && !slotError && !configError && (
            <p className={styles.error} role="alert">
              {create.error instanceof StudioApiError ? create.error.message : "The instance could not be created."}
            </p>
          )}

          <div className={styles.formActions}>
            {flow.kind !== "picker" && (
              <button
                className="secondary-button"
                type="button"
                onClick={() => setModeOverride("new")}
                disabled={pending}
              >
                Connect new credentials instead
              </button>
            )}
            <button className="secondary-button" type="button" onClick={onClose} disabled={pending}>Cancel</button>
            <button className="primary-button" type="submit" disabled={pending || usable.length === 0}>
              {pending ? "Instantiating…" : "Instantiate connector"}
            </button>
          </div>
        </form>
      )}
    </aside>
  );
}
