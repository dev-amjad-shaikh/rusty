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
import { credentialFlow, providerKindLabel, usableConnections } from "./lifecycle";
import { ConfigFields, trimmedConfig, validateConfig } from "./ConfigFields";
import { CredentialPicker } from "./CredentialPicker";
import { BasicAuthForm } from "./BasicAuthForm";
import { ApiKeyForm } from "./ApiKeyForm";
import styles from "./InstantiatePanel.module.css";

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
  const [modeOverride, setModeOverride] = useState<"pick" | "new" | null>(null);
  const [chaining, setChaining] = useState(false);

  useEffect(() => { headingRef.current?.focus(); }, []);

  const connections = useQuery({ queryKey: ["connections"], queryFn: () => listVaultConnections() });
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
        const configParam = configParamNamedInError(message);
        if (configParam) setConfigError({ param: configParam, message });
        else setSlotError({ slot: slotNamedInError(message), message });
      } else {
        setSlotError({ slot: null, message });
      }
      setChaining(false);
      setModeOverride("pick");
    },
  });

  function gateConfig(): boolean {
    const error = validateConfig(manifest, config);
    setConfigError(error);
    return error === null;
  }

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
    create.mutate({ credentials: freshBindings, config: trimmedConfig(manifest, config) });
  }

  function submitFromPicker() {
    setSlotError(null);
    if (!gateConfig()) return;
    create.mutate({
      credentials: Object.fromEntries(Object.entries(bindings).filter(([, id]) => id)),
      config: trimmedConfig(manifest, config),
    });
  }

  function submitNoSlots(event: FormEvent) {
    event.preventDefault();
    create.mutate({ credentials: {}, config: trimmedConfig(manifest, config) });
  }

  const pending = chaining || create.isPending;
  const generalError = slotError && !slotError.slot ? slotError.message : null;
  const createError = create.isError && !slotError && !configError
    ? (create.error instanceof StudioApiError ? create.error.message : "The instance could not be created.")
    : null;

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

      <ConfigFields
        manifest={manifest}
        values={config}
        error={configError}
        disabled={pending}
        onChange={(name, value) => setConfig((current) => ({ ...current, [name]: value }))}
        onClearError={() => setConfigError(null)}
      />

      {manifest.credential_slots.length === 0 ? (
        <form onSubmit={submitNoSlots}>
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
          {flow.kind === "basic" ? (
            <BasicAuthForm
              manifest={manifest}
              pending={pending}
              onRegistered={onConnectionRegistered}
              onCancel={() => (usable.length > 0 ? setModeOverride("pick") : onClose())}
              beforeSubmit={gateConfig}
            />
          ) : (
            <ApiKeyForm
              manifest={manifest}
              pending={pending}
              onRegistered={onConnectionRegistered}
              onCancel={() => (usable.length > 0 ? setModeOverride("pick") : onClose())}
              beforeSubmit={gateConfig}
            />
          )}
        </>
      ) : (
        <CredentialPicker
          manifest={manifest}
          connections={connections.data ?? []}
          bindings={bindings}
          slotError={slotError}
          generalError={generalError}
          createError={createError}
          pending={pending}
          canSwitchToEntry={flow.kind !== "picker"}
          onBind={(slot, id) => setBindings((current) => ({ ...current, [slot]: id }))}
          onClearSlotError={() => setSlotError(null)}
          onSwitchToEntry={() => setModeOverride("new")}
          onCancel={onClose}
          onSubmit={submitFromPicker}
        />
      )}
    </aside>
  );
}
