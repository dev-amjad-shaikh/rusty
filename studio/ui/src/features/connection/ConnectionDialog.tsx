import { type FormEvent, useEffect, useRef, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { getServerInfo, StudioApiError } from "../../lib/api/client";
import { useConnectionStore } from "../../state/connection";
import styles from "./ConnectionDialog.module.css";

export function ConnectionDialog() {
  const queryClient = useQueryClient();
  const { dialogOpen, closeDialog, connect } = useConnectionStore();
  const [origin, setOrigin] = useState("http://127.0.0.1:8100");
  const [apiKey, setApiKey] = useState("");
  const [error, setError] = useState("");
  const [checking, setChecking] = useState(false);
  const headingRef = useRef<HTMLHeadingElement>(null);
  const dialogRef = useRef<HTMLElement>(null);
  const openerRef = useRef<HTMLElement | null>(null);
  const wasOpen = useRef(false);

  useEffect(() => {
    if (dialogOpen && !wasOpen.current) {
      openerRef.current = document.activeElement instanceof HTMLElement ? document.activeElement : null;
      requestAnimationFrame(() => headingRef.current?.focus());
    } else if (!dialogOpen && wasOpen.current) {
      requestAnimationFrame(() => openerRef.current?.focus());
    }
    wasOpen.current = dialogOpen;
  }, [dialogOpen]);

  if (!dialogOpen) return null;

  async function submit(event: FormEvent) {
    event.preventDefault();
    let normalized = "";
    try {
      const parsed = new URL(origin.trim());
      if (!['http:', 'https:'].includes(parsed.protocol) || parsed.username || parsed.password || (parsed.pathname !== "/" && parsed.pathname !== "") || parsed.search || parsed.hash) throw new Error("invalid origin");
      normalized = parsed.origin;
    } catch {
      setError("Enter the full http or https address for your Rusty server.");
      return;
    }
    setChecking(true);
    setError("");
    try {
      const provisional = { epoch: 0, origin: normalized, apiKey, tenantFingerprint: "checking" };
      const info = await getServerInfo(provisional);
      queryClient.clear();
      await connect(normalized, apiKey, info);
    } catch (caught) {
      setError(caught instanceof StudioApiError ? caught.message : "Rusty could not be reached.");
    } finally {
      setChecking(false);
    }
  }

  return (
    <div className={styles.backdrop} role="presentation" onMouseDown={(event) => { if (event.target === event.currentTarget && !checking) closeDialog(); }}>
      <section ref={dialogRef} className={styles.dialog} role="dialog" aria-modal="true" aria-labelledby="connection-heading" aria-describedby="connection-copy" onKeyDown={(event) => {
        if (event.key === "Escape" && !checking) { event.preventDefault(); closeDialog(); return; }
        if (event.key !== "Tab") return;
        const focusable = Array.from(dialogRef.current?.querySelectorAll<HTMLElement>('button:not([disabled]), input:not([disabled])') ?? []);
        if (!focusable.length) return;
        const first = focusable[0], last = focusable.at(-1)!;
        if (event.shiftKey && (document.activeElement === first || document.activeElement === headingRef.current)) { event.preventDefault(); last.focus(); }
        else if (!event.shiftKey && document.activeElement === last) { event.preventDefault(); first.focus(); }
      }}>
        <header>
          <div><span className="eyebrow">Connection</span><h2 id="connection-heading" ref={headingRef} tabIndex={-1}>Connect your Rusty server</h2></div>
          <button type="button" className={styles.close} onClick={closeDialog} disabled={checking} aria-label="Close connection dialog">×</button>
        </header>
        <p id="connection-copy">Studio talks directly to your deployment. The access key stays in page memory and is never placed in the URL.</p>
        <form onSubmit={submit}>
          <label>Server address<input value={origin} onChange={(event) => setOrigin(event.target.value)} inputMode="url" autoComplete="url" spellCheck={false} /></label>
          <label>Access key <span>Optional in open mode</span><input value={apiKey} onChange={(event) => setApiKey(event.target.value)} type="password" autoComplete="off" /></label>
          {error && <p className={styles.error} role="alert">{error}</p>}
          <div className={styles.actions}><button type="button" className="secondary-button" onClick={closeDialog} disabled={checking}>Cancel</button><button type="submit" className="primary-button" disabled={checking}>{checking ? "Checking…" : "Connect"}</button></div>
        </form>
      </section>
    </div>
  );
}
