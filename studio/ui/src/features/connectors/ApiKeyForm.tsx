import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useState, type FormEvent } from "react";
import { StudioApiError } from "../../lib/api/client";
import { apiKeyConnectionSchema, registerApiKeyConnection, type ConnectorManifest } from "../../lib/api/connectors";
import { credentialFlow } from "./lifecycle";
import { OAuthGrantForm } from "./OAuthGrantForm";
import styles from "./CredentialForms.module.css";

interface FieldIssue { path: string; message: string }

export interface ApiKeyFormProps {
  manifest: ConnectorManifest;
  pending: boolean;
  onRegistered: (bindings: Record<string, string>) => void;
  onCancel: () => void;
  beforeSubmit?: () => boolean;
}

export function ApiKeyForm({ manifest, pending, onRegistered, onCancel, beforeSubmit }: ApiKeyFormProps) {
  const queryClient = useQueryClient();
  const flow = credentialFlow(manifest);
  const [values, setValues] = useState<Record<string, string>>({});
  const [issues, setIssues] = useState<FieldIssue[]>([]);

  const register = useMutation({
    mutationFn: async (input: { key: string }): Promise<Record<string, string>> => {
      if (flow.kind !== "key") throw new StudioApiError("This manifest has no tailored credential form.", 0);
      const record = await registerApiKeyConnection(input);
      return { [flow.slot.name]: record.connection_id };
    },
    onSuccess: async (bindings) => {
      setIssues([]);
      await queryClient.invalidateQueries({ queryKey: ["connections"] });
      onRegistered(bindings);
    },
  });

  function submit(event: FormEvent) {
    event.preventDefault();
    if (beforeSubmit && !beforeSubmit()) return;
    const checked = apiKeyConnectionSchema.safeParse({ key: values.key ?? "" });
    if (!checked.success) {
      setIssues(checked.error.issues.map((issue) => ({ path: issue.path.map(String).join(".") || "credentials", message: issue.message })));
      return;
    }
    setIssues([]);
    register.mutate(checked.data);
  }

  const busy = pending || register.isPending;
  const serverError = register.error instanceof StudioApiError ? register.error.message
    : register.error ? "The connection could not be registered."
    : null;

  return (
    <div className={styles.connectionForm} aria-labelledby="register-connection-heading">
      <div className={styles.connectionFormHead}>
        <h3 id="register-connection-heading">Connect with an API key</h3>
        <p>The key crosses this form once and seals into the vault. The slot binds the minted connection id, never the key itself.</p>
      </div>
      <form onSubmit={submit} noValidate className={styles.connectionFormBody}>
        {flow.kind === "key" && (
          <div className={styles.grantField}>
            <label htmlFor="cred-key">API key</label>
            <input id="cred-key" type="password" value={values.key ?? ""} autoComplete="off" spellCheck={false} disabled={busy}
              onChange={(event) => setValues((current) => ({ ...current, key: event.target.value }))} />
            <small>{flow.slot.description}</small>
          </div>
        )}
        {issues.length > 0 && (
          <div className={styles.fieldErrors} role="alert">
            <b>The connection needs attention:</b>
            <ul>{issues.map((issue, index) => <li key={`${issue.path}-${index}`}><code>{issue.path}</code> — {issue.message}</li>)}</ul>
          </div>
        )}
        {serverError && <p className={styles.error} role="alert">{serverError}</p>}
        <div className={styles.formActions}>
          <button className="secondary-button" type="button" onClick={onCancel} disabled={busy}>Cancel</button>
          <button className="primary-button" type="submit" disabled={busy || !(values.key ?? "").trim()}>
            {busy ? "Connecting…" : "Connect and instantiate"}
          </button>
        </div>
      </form>
      <details className={styles.advanced}>
        <summary>Advanced: OAuth2 password grant</summary>
        <OAuthGrantForm manifest={manifest} pending={busy} onRegistered={onRegistered} beforeSubmit={beforeSubmit} />
      </details>
    </div>
  );
}
