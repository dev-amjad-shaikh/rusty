import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useRef, useState, type FormEvent } from "react";
import { StudioApiError } from "../../lib/api/client";
import {
  passwordGrantSchema,
  registerVaultConnection,
  type VaultConnection,
} from "../../lib/api/connectors";
import styles from "./ConnectorsPage.module.css";

interface FieldIssue {
  path: string;
  message: string;
}

const fields: { key: keyof typeof passwordGrantSchema.shape; label: string; secret: boolean; hint: string }[] = [
  { key: "token_url", label: "Token endpoint URL", secret: false, hint: "ServiceNow: https://<instance>.service-now.com/oauth_token.do" },
  { key: "client_id", label: "OAuth client ID", secret: false, hint: "The registered OAuth application's client id" },
  { key: "client_secret", label: "OAuth client secret", secret: true, hint: "Exchanged once at registration, then sealed" },
  { key: "username", label: "Username", secret: false, hint: "The integration account's user name" },
  { key: "password", label: "Password", secret: true, hint: "Re-presented on refresh so access tokens re-mint without a human" },
];

export function RegisterConnectionForm({
  onRegistered,
  onCancel,
}: {
  onRegistered: (record: VaultConnection) => void;
  onCancel: () => void;
}) {
  const queryClient = useQueryClient();
  const [values, setValues] = useState<Record<string, string>>({});
  const [issues, setIssues] = useState<FieldIssue[]>([]);
  const receiptRef = useRef<HTMLDivElement>(null);

  const register = useMutation({
    mutationFn: registerVaultConnection,
    onSuccess: async () => {
      setIssues([]);
      await queryClient.invalidateQueries({ queryKey: ["connections"] });
      requestAnimationFrame(() => receiptRef.current?.focus());
    },
  });

  function submit(event: FormEvent) {
    event.preventDefault();
    const checked = passwordGrantSchema.safeParse({
      token_url: values.token_url?.trim() ?? "",
      client_id: values.client_id?.trim() ?? "",
      client_secret: values.client_secret ?? "",
      username: values.username?.trim() ?? "",
      password: values.password ?? "",
    });
    if (!checked.success) {
      setIssues(checked.error.issues.map((issue) => ({
        path: issue.path.map(String).join(".") || "grant",
        message: issue.message,
      })));
      return;
    }
    setIssues([]);
    register.mutate(checked.data);
  }

  const serverError = register.error instanceof StudioApiError ? register.error.message
    : register.error ? "The connection could not be registered."
    : null;

  return (
    <form className={styles.connectionForm} onSubmit={submit} aria-labelledby="register-connection-heading" noValidate>
      <div className={styles.connectionFormHead}>
        <h3 id="register-connection-heading">Register a vault connection</h3>
        <p>
          The OAuth resource-owner password grant. Rusty exchanges these credentials with the token
          endpoint now and seals the result in the vault — the values cross this form once and are
          never shown again. On refresh the sealed grant re-mints access tokens, so the connection
          stays alive without a human.
        </p>
      </div>
      {fields.map(({ key, label, secret, hint }) => (
        <div className={styles.grantField} key={key}>
          <label htmlFor={`grant-${key}`}>{label}</label>
          <input
            id={`grant-${key}`}
            type={secret ? "password" : "text"}
            value={values[key] ?? ""}
            autoComplete="off"
            spellCheck={false}
            disabled={register.isPending}
            onChange={(event) => setValues((current) => ({ ...current, [key]: event.target.value }))}
          />
          <small>{hint}</small>
        </div>
      ))}
      {issues.length > 0 && (
        <div className={styles.fieldErrors} role="alert">
          <b>The connection needs attention:</b>
          <ul>
            {issues.map((issue, index) => (
              <li key={`${issue.path}-${index}`}><code>{issue.path}</code> — {issue.message}</li>
            ))}
          </ul>
        </div>
      )}
      {serverError && <p className={styles.error} role="alert">{serverError}</p>}
      {register.isSuccess && (
        <div className={styles.receipt} role="status" tabIndex={-1} ref={receiptRef}>
          <b>Connection registered.</b>
          <span>Connection <code>{register.data.connection_id}</code> · {register.data.provider} · {register.data.status}</span>
          <button type="button" className="secondary-button" onClick={() => onRegistered(register.data)}>Bind and continue</button>
        </div>
      )}
      {!register.isSuccess && (
        <div className={styles.formActions}>
          <button className="secondary-button" type="button" onClick={onCancel} disabled={register.isPending}>Cancel</button>
          <button
            className="primary-button"
            type="submit"
            disabled={register.isPending || fields.some(({ key }) => !(values[key] ?? "").trim())}
          >
            {register.isPending ? "Registering…" : "Register connection"}
          </button>
        </div>
      )}
    </form>
  );
}
