import { type FormEvent, useEffect, useRef, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { getServerInfo, StudioApiError } from "../../lib/api/client";
import { useConnectionStore } from "../../state/connection";
import { useWorkStore } from "../../state/work";
import { localWorkspaceOrigin, workspaceDiscoveryMessage, workspaceDisplayName } from "./WorkspaceBootstrap";
import styles from "./ConnectionDialog.module.css";

export function normalizeWorkspaceOrigin(value: string) {
  const parsed = new URL(value.trim());
  const path = parsed.pathname.replace(/\/$/, "");
  if (!['http:', 'https:'].includes(parsed.protocol) || parsed.username || parsed.password
    || !["", "/api"].includes(path) || parsed.search || parsed.hash) throw new Error("invalid origin");
  return `${parsed.origin}${path}`;
}

export function ConnectionDialog() {
  const queryClient = useQueryClient();
  const { connection, dialogOpen, closeDialog, connect, discoveryError, suggestedOrigin } = useConnectionStore();
  const [origin, setOrigin] = useState("");
  const [apiKey, setApiKey] = useState("");
  const [error, setError] = useState("");
  const [checking, setChecking] = useState<"local" | "custom" | "">("");
  const [advancedOpen, setAdvancedOpen] = useState(false);
  const headingRef = useRef<HTMLHeadingElement>(null);
  const dialogRef = useRef<HTMLElement>(null);
  const openerRef = useRef<HTMLElement | null>(null);
  const wasOpen = useRef(false);
  const localOrigin = localWorkspaceOrigin();
  const currentIsLocal = connection?.origin === localOrigin
    || connection?.origin === window.location.origin.replace(/\/$/, "");

  useEffect(() => {
    if (dialogOpen && !wasOpen.current) {
      openerRef.current = document.activeElement instanceof HTMLElement ? document.activeElement : null;
      setOrigin(connection?.origin ?? suggestedOrigin ?? localOrigin);
      setApiKey("");
      setError("");
      setAdvancedOpen(discoveryError.toLowerCase().includes("access key"));
      requestAnimationFrame(() => headingRef.current?.focus());
    } else if (!dialogOpen && wasOpen.current) {
      requestAnimationFrame(() => openerRef.current?.focus());
    }
    wasOpen.current = dialogOpen;
  }, [connection?.origin, dialogOpen, localOrigin, suggestedOrigin]);

  if (!dialogOpen) return null;

  async function openWorkspace(nextOrigin: string, nextApiKey: string, source: "local" | "custom") {
    setChecking(source);
    setError("");
    try {
      const provisional = { epoch: 0, origin: nextOrigin, apiKey: nextApiKey, tenantFingerprint: "checking" };
      const info = await getServerInfo(provisional);
      queryClient.clear();
      useWorkStore.getState().clear();
      await connect(nextOrigin, nextApiKey, info);
    } catch (caught) {
      setError(source === "local" ? workspaceDiscoveryMessage(caught)
        : caught instanceof StudioApiError ? caught.message : "Rusty could not be reached.");
      if (source === "local" && caught instanceof StudioApiError && (caught.status === 401 || caught.status === 403)) {
        setOrigin(localOrigin);
        setAdvancedOpen(true);
      }
    } finally {
      setChecking("");
    }
  }

  async function submit(event: FormEvent) {
    event.preventDefault();
    let normalized = "";
    try {
      normalized = normalizeWorkspaceOrigin(origin);
    } catch {
      setError("Enter an http or https address for a Rusty server. Only its optional /api gateway path is allowed.");
      return;
    }
    await openWorkspace(normalized, apiKey, "custom");
  }

  const busy = Boolean(checking);
  return (
    <div className={styles.backdrop} role="presentation" onMouseDown={(event) => { if (event.target === event.currentTarget && !busy) closeDialog(); }}>
      <section ref={dialogRef} className={styles.dialog} role="dialog" aria-modal="true" aria-labelledby="connection-heading" aria-describedby="connection-copy" onKeyDown={(event) => {
        if (event.key === "Escape" && !busy) { event.preventDefault(); closeDialog(); return; }
        if (event.key !== "Tab") return;
        const focusable = Array.from(dialogRef.current?.querySelectorAll<HTMLElement>('button:not([disabled]), input:not([disabled]), summary') ?? [])
          .filter((element) => element.tagName === "SUMMARY" || !element.closest("details:not([open])"));
        if (!focusable.length) return;
        const first = focusable[0], last = focusable.at(-1)!;
        if (event.shiftKey && (document.activeElement === first || document.activeElement === headingRef.current)) { event.preventDefault(); last.focus(); }
        else if (!event.shiftKey && document.activeElement === last) { event.preventDefault(); first.focus(); }
      }}>
        <header>
          <div><span className="eyebrow">Workspace</span><h2 id="connection-heading" ref={headingRef} tabIndex={-1}>{connection ? "Switch workspace" : "Open a workspace"}</h2></div>
          <button type="button" className={styles.close} onClick={closeDialog} disabled={busy} aria-label="Close workspace dialog">×</button>
        </header>
        <p id="connection-copy">Studio normally finds Rusty on this device. Choose another deployment only when you need it.</p>

        {connection && <div className={styles.current}><span>Current workspace</span><b>{workspaceDisplayName(connection.origin)}</b></div>}

        {!currentIsLocal && <div className={styles.localChoice}>
          <div><b>Local workspace</b><span>{discoveryError || "Use the Rusty server running with this Studio."}</span></div>
          <button type="button" className="primary-button" disabled={busy} onClick={() => void openWorkspace(localOrigin, "", "local")}>{checking === "local" ? "Opening…" : "Use local workspace"}</button>
        </div>}

        <details className={styles.advanced} open={advancedOpen} onToggle={(event) => setAdvancedOpen(event.currentTarget.open)}>
          <summary>Use another server or access key</summary>
          <form onSubmit={submit}>
            <label>Server address<input value={origin} onChange={(event) => setOrigin(event.target.value)} inputMode="url" autoComplete="url" spellCheck={false} /></label>
            <label>Access key <span>Leave blank for open mode</span><input value={apiKey} onChange={(event) => setApiKey(event.target.value)} type="password" autoComplete="off" /></label>
            <div className={styles.actions}><button type="button" className="secondary-button" onClick={closeDialog} disabled={busy}>{connection ? "Keep current" : "Cancel"}</button><button type="submit" className="primary-button" disabled={busy}>{checking === "custom" ? "Opening…" : "Open workspace"}</button></div>
          </form>
        </details>
        {error && <p className={styles.error} role="alert">{error}</p>}
        <p className={styles.privacy}>Access keys remain in page memory and never appear in the URL.</p>
      </section>
    </div>
  );
}
