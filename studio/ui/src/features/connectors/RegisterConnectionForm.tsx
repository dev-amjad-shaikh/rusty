import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useState, type FormEvent } from "react";
import { z } from "zod";
import { StudioApiError } from "../../lib/api/client";
import {
  apiKeyConnectionSchema,
  basicConnectionSchema,
  passwordGrantSchema,
  registerApiKeyConnection,
  registerBasicConnection,
  registerVaultConnection,
  type ConnectorManifest,
} from "../../lib/api/connectors";
import { credentialFlow } from "./lifecycle";
import styles from "./ConnectorsPage.module.css";

interface FieldIssue {
  path: string;
  message: string;
}

function issuesFrom(error: z.ZodError): FieldIssue[] {
  return error.issues.map((issue) => ({
    path: issue.path.map(String).join(".") || "credentials",
    message: issue.message,
  }));
}

const grantFields: { key: keyof typeof passwordGrantSchema.shape; label: string; secret: boolean; hint: string }[] = [
  { key: "token_url", label: "Token endpoint URL", secret: false, hint: "ServiceNow: https://<instance>.service-now.com/oauth_token.do" },
  { key: "client_id", label: "OAuth client ID", secret: false, hint: "The registered OAuth application's client id" },
  { key: "client_secret", label: "OAuth client secret", secret: true, hint: "Exchanged once at registration, then sealed" },
  { key: "username", label: "Username", secret: false, hint: "The integration account's user name" },
  { key: "password", label: "Password", secret: true, hint: "Re-presented on refresh so access tokens re-mint without a human" },
];

/**
 * The credential-entry half of the instantiate journey, shaped by the
 * manifest's auth declaration: a basic-auth connector asks for the
 * instance username and password (sealed as a connection pair — one leg
 * per slot, because the vault bridge resolves each slot through its own
 * connection), a single-slot connector asks for one key value, and the
 * OAuth2 password grant stays available behind a disclosure for the
 * single-slot flows it can actually fill. On success the panel receives
 * the slot → connection bindings and instantiates in the same motion.
 */
export function RegisterConnectionForm({
  manifest,
  chaining,
  onRegistered,
  onCancel,
}: {
  manifest: ConnectorManifest;
  /** The panel is turning a successful registration straight into the instantiate call. */
  chaining: boolean;
  onRegistered: (bindings: Record<string, string>) => void;
  onCancel: () => void;
}) {
  const queryClient = useQueryClient();
  const flow = credentialFlow(manifest);
  const [values, setValues] = useState<Record<string, string>>({});
  const [issues, setIssues] = useState<FieldIssue[]>([]);

  const registered = async (bindings: Record<string, string>) => {
    setIssues([]);
    await queryClient.invalidateQueries({ queryKey: ["connections"] });
    onRegistered(bindings);
  };

  const register = useMutation({
    mutationFn: async (
      input: z.infer<typeof basicConnectionSchema> | z.infer<typeof apiKeyConnectionSchema>,
    ): Promise<Record<string, string>> => {
      if (flow.kind === "basic" && "username" in input) {
        const pair = await registerBasicConnection(input);
        return {
          [flow.usernameSlot.name]: pair.username_connection.connection_id,
          [flow.passwordSlot.name]: pair.password_connection.connection_id,
        };
      }
      if (flow.kind === "key" && "key" in input) {
        const record = await registerApiKeyConnection(input);
        return { [flow.slot.name]: record.connection_id };
      }
      throw new StudioApiError("This manifest has no tailored credential form.", 0);
    },
    onSuccess: (bindings) => { void registered(bindings); },
  });

  const grant = useMutation({
    mutationFn: async (input: z.infer<typeof passwordGrantSchema>): Promise<Record<string, string>> => {
      const record = await registerVaultConnection(input);
      // The grant's minted access token is the slot material: every slot
      // the manifest declares binds the one connection.
      return Object.fromEntries(manifest.credential_slots.map((slot) => [slot.name, record.connection_id]));
    },
    onSuccess: (bindings) => { void registered(bindings); },
  });

  function submit(event: FormEvent) {
    event.preventDefault();
    if (flow.kind === "basic") {
      const checked = basicConnectionSchema.safeParse({
        username: values.username?.trim() ?? "",
        password: values.password ?? "",
      });
      if (!checked.success) { setIssues(issuesFrom(checked.error)); return; }
      setIssues([]);
      register.mutate(checked.data);
      return;
    }
    if (flow.kind === "key") {
      const checked = apiKeyConnectionSchema.safeParse({ key: values.key ?? "" });
      if (!checked.success) { setIssues(issuesFrom(checked.error)); return; }
      setIssues([]);
      register.mutate(checked.data);
    }
  }

  function submitGrant(event: FormEvent) {
    event.preventDefault();
    const checked = passwordGrantSchema.safeParse({
      token_url: values.token_url?.trim() ?? "",
      client_id: values.client_id?.trim() ?? "",
      client_secret: values.client_secret ?? "",
      username: values.grant_username?.trim() ?? "",
      password: values.grant_password ?? "",
    });
    if (!checked.success) { setIssues(issuesFrom(checked.error)); return; }
    setIssues([]);
    grant.mutate(checked.data);
  }

  const pending = chaining || register.isPending || grant.isPending;
  const serverError = register.error instanceof StudioApiError ? register.error.message
    : register.error ? "The connection could not be registered."
    : null;
  const grantError = grant.error instanceof StudioApiError ? grant.error.message
    : grant.error ? "The connection could not be registered."
    : null;

  const head = flow.kind === "basic"
    ? {
        title: "Connect with instance credentials",
        copy: "Basic authentication. The username and password cross this form once, seal into the vault as a connection pair — one leg per slot — and bind below. The instance never holds raw secret bytes.",
      }
    : {
        title: "Connect with an API key",
        copy: "The key crosses this form once and seals into the vault. The slot binds the minted connection id, never the key itself.",
      };

  return (
    <div className={styles.connectionForm} aria-labelledby="register-connection-heading">
      <div className={styles.connectionFormHead}>
        <h3 id="register-connection-heading">{head.title}</h3>
        <p>{head.copy}</p>
      </div>

      <form onSubmit={submit} noValidate className={styles.connectionFormBody}>
        {flow.kind === "basic" ? (
          <>
            <div className={styles.grantField}>
              <label htmlFor="cred-username">Instance username</label>
              <input
                id="cred-username"
                type="text"
                value={values.username ?? ""}
                autoComplete="off"
                spellCheck={false}
                disabled={pending}
                onChange={(event) => setValues((current) => ({ ...current, username: event.target.value }))}
              />
              <small>{flow.usernameSlot.description}</small>
            </div>
            <div className={styles.grantField}>
              <label htmlFor="cred-password">Instance password</label>
              <input
                id="cred-password"
                type="password"
                value={values.password ?? ""}
                autoComplete="off"
                spellCheck={false}
                disabled={pending}
                onChange={(event) => setValues((current) => ({ ...current, password: event.target.value }))}
              />
              <small>{flow.passwordSlot.description}</small>
            </div>
          </>
        ) : flow.kind === "key" ? (
          <div className={styles.grantField}>
            <label htmlFor="cred-key">API key</label>
            <input
              id="cred-key"
              type="password"
              value={values.key ?? ""}
              autoComplete="off"
              spellCheck={false}
              disabled={pending}
              onChange={(event) => setValues((current) => ({ ...current, key: event.target.value }))}
            />
            <small>{flow.slot.description}</small>
          </div>
        ) : null}

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

        <div className={styles.formActions}>
          <button className="secondary-button" type="button" onClick={onCancel} disabled={pending}>Cancel</button>
          <button
            className="primary-button"
            type="submit"
            disabled={
              pending ||
              (flow.kind === "basic"
                ? !(values.username ?? "").trim() || !(values.password ?? "")
                : flow.kind === "key"
                  ? !(values.key ?? "").trim()
                  : true)
            }
          >
            {pending ? "Connecting…" : "Connect and instantiate"}
          </button>
        </div>
      </form>

      {flow.kind === "key" && (
        <details className={styles.advanced}>
          <summary>Advanced: OAuth2 password grant</summary>
          <form onSubmit={submitGrant} noValidate className={styles.connectionFormBody}>
            <p className={styles.quiet}>
              The OAuth resource-owner password grant. Rusty exchanges these credentials with the token
              endpoint now and seals the result in the vault; on refresh the sealed grant re-mints access
              tokens, so the connection stays alive without a human.
            </p>
            {grantFields.map(({ key, label, secret, hint }) => {
              const inputKey = key === "username" ? "grant_username" : key === "password" ? "grant_password" : key;
              return (
                <div className={styles.grantField} key={key}>
                  <label htmlFor={`grant-${key}`}>{label}</label>
                  <input
                    id={`grant-${key}`}
                    type={secret ? "password" : "text"}
                    value={values[inputKey] ?? ""}
                    autoComplete="off"
                    spellCheck={false}
                    disabled={pending}
                    onChange={(event) => setValues((current) => ({ ...current, [inputKey]: event.target.value }))}
                  />
                  <small>{hint}</small>
                </div>
              );
            })}
            {grantError && <p className={styles.error} role="alert">{grantError}</p>}
            <div className={styles.formActions}>
              <button
                className="primary-button"
                type="submit"
                disabled={pending || grantFields.some(({ key }) => {
                  const inputKey = key === "username" ? "grant_username" : key === "password" ? "grant_password" : key;
                  return !(values[inputKey] ?? "").trim();
                })}
              >
                {grant.isPending ? "Exchanging…" : "Exchange and instantiate"}
              </button>
            </div>
          </form>
        </details>
      )}
    </div>
  );
}
