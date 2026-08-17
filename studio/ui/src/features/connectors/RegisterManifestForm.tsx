import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useRef, useState, type FormEvent } from "react";
import { StudioApiError } from "../../lib/api/client";
import {
  manifestPayloadSchema,
  registerConnectorManifest,
  type ManifestPayload,
  type ManifestReceipt,
} from "../../lib/api/connectors";
import styles from "./ConnectorsPage.module.css";

const manifestTemplate = `{
  "id": "brave-search",
  "version": "1.0.0",
  "display_name": "Brave Search",
  "description": "Bounded web search over an HTTPS endpoint.",
  "provider": {
    "kind": "http_search",
    "base_url": "https://api.search.example.com/search",
    "auth": { "header": "X-Api-Key", "credential_slot": "api_key" }
  },
  "capabilities": ["web search"],
  "credential_slots": [
    { "name": "api_key", "description": "Search API key issued to this tenant" }
  ]
}`;

const fieldHints: [string, string][] = [
  ["id", "kebab-case, at most 64 bytes — the connector's stable identity"],
  ["version", "opaque version string; committed to by the content hash"],
  ["display_name / description", "human-facing; trimmed, control-free"],
  ["provider.kind", "mcp_stdio { command, args, env_allowlist } or http_search { base_url (https:// only), auth }"],
  ["capabilities", "short summary strings; sorted and deduplicated by the server"],
  ["credential_slots", "[{ name, description }] — names only, never values; [a-z][a-z0-9_]*"],
  ["hash", "optional — the server recomputes it; a disagreement is a 422"],
];

interface FieldIssue {
  path: string;
  message: string;
}

export function RegisterManifestForm({
  onRegistered,
}: {
  onRegistered: () => void;
}) {
  const queryClient = useQueryClient();
  const [draft, setDraft] = useState("");
  const [issues, setIssues] = useState<FieldIssue[]>([]);
  const [receipt, setReceipt] = useState<ManifestReceipt | null>(null);
  const receiptRef = useRef<HTMLDivElement>(null);

  const register = useMutation({
    mutationFn: (payload: ManifestPayload) => registerConnectorManifest(payload),
    onSuccess: async (result) => {
      setReceipt(result);
      setIssues([]);
      await queryClient.invalidateQueries({ queryKey: ["connectors", "manifests"] });
      requestAnimationFrame(() => receiptRef.current?.focus());
    },
  });

  function submit(event: FormEvent) {
    event.preventDefault();
    setReceipt(null);
    let parsed: unknown;
    try {
      parsed = JSON.parse(draft);
    } catch (error) {
      setIssues([{ path: "manifest JSON", message: error instanceof Error ? error.message : "The manifest is not valid JSON." }]);
      return;
    }
    const checked = manifestPayloadSchema.safeParse(parsed);
    if (!checked.success) {
      setIssues(checked.error.issues.map((issue) => ({
        path: issue.path.length ? issue.path.map(String).join(".") : "manifest",
        message: issue.message,
      })));
      return;
    }
    setIssues([]);
    register.mutate(checked.data);
  }

  const serverError = register.error instanceof StudioApiError ? register.error.message
    : register.error ? "The manifest could not be registered."
    : null;

  return (
    <form id="register-manifest" className={styles.registerForm} onSubmit={submit} aria-labelledby="register-manifest-heading">
      <div className={styles.registerCopy}>
        <h3 id="register-manifest-heading">Register a manifest</h3>
        <p>Paste the manifest JSON. Registration is idempotent by content hash — the same bytes converge, new bytes register a new entry.</p>
        <dl className={styles.fieldHints}>
          {fieldHints.map(([field, hint]) => (
            <div key={field}><dt><code>{field}</code></dt><dd>{hint}</dd></div>
          ))}
        </dl>
      </div>
      <div className={styles.registerEditor}>
        <label htmlFor="manifest-json">Manifest JSON</label>
        <textarea
          id="manifest-json"
          value={draft}
          onChange={(event) => setDraft(event.target.value)}
          placeholder={manifestTemplate}
          rows={16}
          spellCheck={false}
          disabled={register.isPending}
        />
        {issues.length > 0 && (
          <div className={styles.fieldErrors} role="alert">
            <b>The manifest needs attention:</b>
            <ul>
              {issues.map((issue, index) => (
                <li key={`${issue.path}-${index}`}><code>{issue.path}</code> — {issue.message}</li>
              ))}
            </ul>
          </div>
        )}
        {serverError && <p className={styles.error} role="alert">{serverError}</p>}
        {receipt && (
          <div className={styles.receipt} role="status" tabIndex={-1} ref={receiptRef}>
            <b>{receipt.already_registered ? "Already registered — identical content." : "Manifest registered."}</b>
            <span>Connector <code>{receipt.id}</code> · v{receipt.version} · hash <code title={receipt.manifest_hash}>{receipt.manifest_hash.slice(0, 12)}</code></span>
            <button type="button" className="secondary-button" onClick={onRegistered}>Done</button>
          </div>
        )}
        <div className={styles.formActions}>
          <button className="primary-button" type="submit" disabled={register.isPending || !draft.trim()}>
            {register.isPending ? "Registering…" : "Register manifest"}
          </button>
        </div>
      </div>
    </form>
  );
}
