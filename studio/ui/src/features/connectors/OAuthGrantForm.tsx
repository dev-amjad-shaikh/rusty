import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useState, type FormEvent } from "react";
import { z } from "zod";
import { StudioApiError } from "../../lib/api/client";
import { passwordGrantSchema, registerVaultConnection, type ConnectorManifest } from "../../lib/api/connectors";
import styles from "./CredentialForms.module.css";

type GrantInput = z.infer<typeof passwordGrantSchema>;

const grantFields: { key: keyof GrantInput; label: string; secret: boolean; hint: string }[] = [
  { key: "token_url", label: "Token endpoint URL", secret: false, hint: "ServiceNow: https://<instance>.service-now.com/oauth_token.do" },
  { key: "client_id", label: "OAuth client ID", secret: false, hint: "The registered OAuth application's client id" },
  { key: "client_secret", label: "OAuth client secret", secret: true, hint: "Exchanged once at registration, then sealed" },
  { key: "username", label: "Username", secret: false, hint: "The integration account's user name" },
  { key: "password", label: "Password", secret: true, hint: "Re-presented on refresh so access tokens re-mint without a human" },
];

export interface OAuthGrantFormProps {
  manifest: ConnectorManifest;
  pending: boolean;
  onRegistered: (bindings: Record<string, string>) => void;
  beforeSubmit?: () => boolean;
}

export function OAuthGrantForm({ manifest, pending, onRegistered, beforeSubmit }: OAuthGrantFormProps) {
  const queryClient = useQueryClient();
  const [values, setValues] = useState<Record<string, string>>({});

  const grant = useMutation({
    mutationFn: async (input: GrantInput): Promise<Record<string, string>> => {
      const record = await registerVaultConnection(input);
      return Object.fromEntries(manifest.credential_slots.map((slot) => [slot.name, record.connection_id]));
    },
    onSuccess: async (bindings) => {
      await queryClient.invalidateQueries({ queryKey: ["connections"] });
      onRegistered(bindings);
    },
  });

  function submit(event: FormEvent) {
    event.preventDefault();
    if (beforeSubmit && !beforeSubmit()) return;
    const checked = passwordGrantSchema.safeParse({
      token_url: values.token_url?.trim() ?? "",
      client_id: values.client_id?.trim() ?? "",
      client_secret: values.client_secret ?? "",
      username: values.username?.trim() ?? "",
      password: values.password ?? "",
    });
    if (!checked.success) {
      setIssues(checked.error.issues.map((issue) => ({ path: issue.path.map(String).join(".") || "grant", message: issue.message })));
      return;
    }
    setIssues([]);
    grant.mutate(checked.data);
  }

  const [issues, setIssues] = useState<{ path: string; message: string }[]>([]);
  const busy = pending || grant.isPending;
  const grantError = grant.error instanceof StudioApiError ? grant.error.message
    : grant.error ? "The connection could not be registered."
    : null;

  return (
    <form onSubmit={submit} noValidate className={styles.connectionFormBody}>
      <p className={styles.quiet}>
        The OAuth resource-owner password grant. Rusty exchanges these credentials with the token
        endpoint now and seals the result in the vault; on refresh the sealed grant re-mints access
        tokens, so the connection stays alive without a human.
      </p>
      {grantFields.map(({ key, label, secret, hint }) => (
        <div className={styles.grantField} key={key}>
          <label htmlFor={`grant-${key}`}>{label}</label>
          <input id={`grant-${key}`} type={secret ? "password" : "text"} value={values[key] ?? ""} autoComplete="off" spellCheck={false} disabled={busy}
            onChange={(event) => setValues((current) => ({ ...current, [key]: event.target.value }))} />
          <small>{hint}</small>
        </div>
      ))}
      {issues.length > 0 && (
        <div className={styles.fieldErrors} role="alert">
          <b>The grant needs attention:</b>
          <ul>{issues.map((issue, index) => <li key={`${issue.path}-${index}`}><code>{issue.path}</code> — {issue.message}</li>)}</ul>
        </div>
      )}
      {grantError && <p className={styles.error} role="alert">{grantError}</p>}
      <div className={styles.formActions}>
        <button className="primary-button" type="submit" disabled={busy || grantFields.some(({ key }) => !(values[key] ?? "").trim())}>
          {grant.isPending ? "Exchanging…" : "Exchange and instantiate"}
        </button>
      </div>
    </form>
  );
}
