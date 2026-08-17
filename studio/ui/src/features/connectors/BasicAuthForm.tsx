import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useState, type FormEvent } from "react";
import { z } from "zod";
import { StudioApiError } from "../../lib/api/client";
import { basicConnectionSchema, registerBasicConnection, type ConnectorManifest } from "../../lib/api/connectors";
import { credentialFlow } from "./lifecycle";
import styles from "./CredentialForms.module.css";

interface FieldIssue { path: string; message: string }

function issuesFrom(error: z.ZodError): FieldIssue[] {
  return error.issues.map((issue) => ({ path: issue.path.map(String).join(".") || "credentials", message: issue.message }));
}

export interface BasicAuthFormProps {
  manifest: ConnectorManifest;
  pending: boolean;
  onRegistered: (bindings: Record<string, string>) => void;
  onCancel: () => void;
  beforeSubmit?: () => boolean;
}

export function BasicAuthForm({ manifest, pending, onRegistered, onCancel, beforeSubmit }: BasicAuthFormProps) {
  const queryClient = useQueryClient();
  const flow = credentialFlow(manifest);
  const [values, setValues] = useState<Record<string, string>>({});
  const [issues, setIssues] = useState<FieldIssue[]>([]);

  const register = useMutation({
    mutationFn: async (input: { username: string; password: string }): Promise<Record<string, string>> => {
      if (flow.kind !== "basic") throw new StudioApiError("This manifest has no tailored credential form.", 0);
      const pair = await registerBasicConnection(input);
      return {
        [flow.usernameSlot.name]: pair.username_connection.connection_id,
        [flow.passwordSlot.name]: pair.password_connection.connection_id,
      };
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
    const checked = basicConnectionSchema.safeParse({
      username: values.username?.trim() ?? "",
      password: values.password ?? "",
    });
    if (!checked.success) { setIssues(issuesFrom(checked.error)); return; }
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
        <h3 id="register-connection-heading">Connect with instance credentials</h3>
        <p>Basic authentication. The username and password cross this form once, seal into the vault as a connection pair — one leg per slot — and bind below. The instance never holds raw secret bytes.</p>
      </div>
      <form onSubmit={submit} noValidate className={styles.connectionFormBody}>
        {flow.kind === "basic" && (
          <>
            <div className={styles.grantField}>
              <label htmlFor="cred-username">Instance username</label>
              <input id="cred-username" type="text" value={values.username ?? ""} autoComplete="off" spellCheck={false} disabled={busy}
                onChange={(event) => setValues((current) => ({ ...current, username: event.target.value }))} />
              <small>{flow.usernameSlot.description}</small>
            </div>
            <div className={styles.grantField}>
              <label htmlFor="cred-password">Instance password</label>
              <input id="cred-password" type="password" value={values.password ?? ""} autoComplete="off" spellCheck={false} disabled={busy}
                onChange={(event) => setValues((current) => ({ ...current, password: event.target.value }))} />
              <small>{flow.passwordSlot.description}</small>
            </div>
          </>
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
          <button className="primary-button" type="submit" disabled={busy || !(values.username ?? "").trim() || !(values.password ?? "")}>
            {busy ? "Connecting…" : "Connect and instantiate"}
          </button>
        </div>
      </form>
    </div>
  );
}
