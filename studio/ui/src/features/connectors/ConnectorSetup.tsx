import { type FormEvent, useMemo, useState } from "react";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import {
  checkConnectorConfig,
  createConnectorInstance,
  type ConnectorInstance,
  type ConnectorManifest,
} from "../../lib/api/connectors";
import { StudioApiError } from "../../lib/api/client";
import {
  buildConfig,
  initialSelections,
  initialValues,
  interpretForm,
  knownPaths,
  pinFieldError,
  type FormValues,
  type VariantSelections,
} from "../../lib/schema-form";
import { SchemaForm } from "./SchemaForm";
import styles from "./ConnectorsPage.module.css";

type CheckState =
  | { kind: "succeeded"; message: string | null }
  | { kind: "failed"; message: string | null };

/** The generic connector setup: schema form + inline test connection + save.
 * The 422 contract pins field errors by dot path; check-first is the obvious
 * path, but save stays independently enabled. */
export function ConnectorSetup({
  manifest,
  onDone,
  onCancel,
}: {
  manifest: ConnectorManifest;
  onDone: (instance: ConnectorInstance) => void;
  onCancel: () => void;
}) {
  const queryClient = useQueryClient();
  const form = useMemo(() => interpretForm(manifest.connection_specification), [manifest]);
  const paths = useMemo(() => knownPaths(form), [form]);
  const [values, setValues] = useState<FormValues>(() => initialValues(form, initialSelections(form)));
  const [selections, setSelections] = useState<VariantSelections>(() => initialSelections(form));
  const [errors, setErrors] = useState<Record<string, string>>({});
  const [generalError, setGeneralError] = useState<string | null>(null);
  const [check, setCheck] = useState<CheckState | null>(null);

  /** Pin a 422's dot-path message to its field; anything else is a panel error. */
  function applyRejection(error: unknown): boolean {
    if (error instanceof StudioApiError && error.status === 422) {
      const pinned = pinFieldError(error.message, paths);
      if (pinned) {
        setErrors({ [pinned.path]: pinned.reason });
        return true;
      }
      setGeneralError(error.message);
      return true;
    }
    return false;
  }

  function editValue(path: string, value: unknown) {
    setValues((current) => ({ ...current, [path]: value }));
    setErrors((current) => {
      const next = { ...current };
      delete next[path];
      return next;
    });
    setGeneralError(null);
    setCheck(null);
  }

  function selectVariant(path: string, variantKey: string) {
    setSelections((current) => ({ ...current, [path]: variantKey }));
    setErrors({});
    setGeneralError(null);
    setCheck(null);
  }

  const testConnection = useMutation({
    mutationFn: () => checkConnectorConfig(manifest.hash, buildConfig(form, values, selections)),
    onSuccess: (outcome) => setCheck({ kind: outcome.status, message: outcome.message ?? null }),
    onError: (error) => {
      if (!applyRejection(error)) {
        setGeneralError(error instanceof Error ? error.message : "The check could not be run.");
      }
    },
  });

  const save = useMutation({
    mutationFn: () => createConnectorInstance({
      manifest_hash: manifest.hash,
      config: buildConfig(form, values, selections),
    }),
    onSuccess: (instance) => {
      queryClient.invalidateQueries({ queryKey: ["connector-instances"] });
      onDone(instance);
    },
    onError: (error) => {
      if (!applyRejection(error)) {
        setGeneralError(error instanceof Error ? error.message : "The connection could not be saved.");
      }
    },
  });

  function submit(event: FormEvent) {
    event.preventDefault();
    if (!save.isPending && !testConnection.isPending) save.mutate();
  }

  if (!form.supported) {
    return (
      <div className={styles.panel}>
        <h2>Set up {manifest.display_name}</h2>
        <p className={styles.panelLead}>
          This connector's configuration schema is not an object form Studio can render.
          Register an instance over the HTTP surface instead.
        </p>
        <div className={styles.formActions}>
          <button className="secondary-button" type="button" onClick={onCancel}>Back</button>
        </div>
      </div>
    );
  }

  return (
    <form className={styles.panel} onSubmit={submit} aria-label={`Set up ${manifest.display_name}`}>
      <h2>Set up {manifest.display_name}</h2>
      <p className={styles.panelLead}>
        {manifest.description}{" "}
        <a href={manifest.documentation_url} target="_blank" rel="noreferrer">Documentation</a>
      </p>

      {generalError && <p className={styles.error} role="alert">{generalError}</p>}

      <SchemaForm
        form={form}
        values={values}
        selections={selections}
        errors={errors}
        onValue={editValue}
        onSelect={selectVariant}
      />

      {check && (
        <p className={check.kind === "succeeded" ? styles.checkOk : styles.checkFailed} role="status">
          {check.kind === "succeeded"
            ? `Connection verified.${check.message ? ` ${check.message}` : ""}`
            : `Connection failed.${check.message ? ` ${check.message}` : ""}`}
        </p>
      )}

      <div className={styles.formActions}>
        <button className="secondary-button" type="button" onClick={onCancel}>Cancel</button>
        <button
          className="secondary-button"
          type="button"
          disabled={testConnection.isPending || save.isPending}
          onClick={() => testConnection.mutate()}
        >
          {testConnection.isPending ? "Testing…" : "Test connection"}
        </button>
        <button className="primary-button" type="submit" disabled={save.isPending || testConnection.isPending}>
          {save.isPending ? "Saving…" : "Save connection"}
        </button>
      </div>
    </form>
  );
}
