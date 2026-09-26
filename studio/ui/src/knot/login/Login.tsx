import { useEffect, useState } from "react";
import { useNavigate } from "@tanstack/react-router";
import { Button, Field, Input } from "../../engine/controls";
import { apiBase, login, oidcPublic, oidcStartUrl, type OidcPublic } from "../../engine/net/client";
import { useServer } from "../../engine/net/server";

/**
 * Sign in. A person is a name and a password; the server answers with who
 * they are and keeps the session in a cookie this browser holds. There is
 * nothing to configure here and nothing kept in the browser — the first
 * administrator's password is in the file the server wrote on its first
 * boot, next to its store.
 */
export function Login() {
  const navigate = useNavigate();
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [refusal, setRefusal] = useState<string | null>(null);
  const [provider, setProvider] = useState<OidcPublic | null>(null);
  // The store knows when a person was signed in and the session ran out;
  // the screen says so, or the drop to this form reads as a fault.
  const ended = useServer((s) => s.refused && s.problem === "your session ended — sign in again");
  useEffect(() => { let live = true; oidcPublic().then((p) => { if (live) setProvider(p); }).catch(() => { if (live) setProvider({ configured: false }); }); return () => { live = false; }; }, []);

  async function submit() {
    if (!username.trim() || !password || busy) return;
    setBusy(true); setRefusal(null);
    try {
      await login(username.trim(), password);
      await useServer.getState().refresh();
      navigate({ to: "/agents" });
    } catch (err) {
      setRefusal(err instanceof Error ? err.message : "the sign-in was refused");
    } finally {
      setBusy(false);
    }
  }

  return (
    <main data-screen="login" style={{ flex: 1, display: "grid", placeItems: "center", padding: 24 }}>
      <form
        onSubmit={(e) => { e.preventDefault(); void submit(); }}
        style={{ width: "min(360px, 100%)", display: "flex", flexDirection: "column", gap: 14, padding: 22, border: "1px solid var(--line)", borderRadius: "var(--r-card-lg)", background: "var(--panel)", boxShadow: "var(--shadow)" }}
      >
        <div>
          <h1 style={{ margin: 0, fontSize: 18, fontWeight: 600 }}>Sign in</h1>
          <span style={{ fontSize: 12, color: "var(--ink3)" }}>to {apiBase()}</span>
        </div>
        {ended && (
          <div data-note="session-ended" style={{ fontSize: 12.5, color: "var(--ink2)", padding: "8px 10px", border: "1px solid var(--line)", borderRadius: "var(--r-card)", background: "var(--bg)" }}>
            Your session ended — a session ends after 12 hours without you. Sign in again to go on where you were.
          </div>
        )}
        <Field label="Sign-in name" htmlFor="login-name">
          <Input id="login-name" data-field="username" autoComplete="username" value={username} onChange={(e) => setUsername(e.target.value)} autoFocus />
        </Field>
        <Field label="Password" htmlFor="login-password">
          <Input id="login-password" data-field="password" type="password" autoComplete="current-password" value={password} onChange={(e) => setPassword(e.target.value)} />
        </Field>
        {refusal && <div data-note="refusal" style={{ fontSize: 12.5, color: "var(--err)" }}>{refusal}</div>}
        <Button variant="primary" type="submit" disabled={busy || !username.trim() || !password} data-action="sign-in">
          {busy ? "Signing in…" : "Sign in"}
        </Button>
        {provider?.configured && (
          <a href={oidcStartUrl("/agents")} data-action="sign-in-oidc" style={{ display: "block", textAlign: "center", padding: "8px 12px", border: "1px solid var(--line)", borderRadius: "var(--r-card)", textDecoration: "none", color: "var(--ink)", fontSize: 13 }}>
            Sign in with {provider.name}
          </a>
        )}
        <span style={{ fontSize: 12, color: "var(--ink3)", lineHeight: 1.5 }}>
          First time here? The server created an administrator on its first boot and wrote the
          password to <code style={{ fontFamily: "var(--font-mono)" }}>bootstrap-admin.txt</code> next to its store.
        </span>
      </form>
    </main>
  );
}
