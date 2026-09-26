import { useEffect, useState } from "react";
import { useNavigate } from "@tanstack/react-router";
import { listOpenProposals, type OpenProposal, capacity, createUser, decideApproval, deleteUser, egressCeiling, forgetUser, estate, estateRoster, estateSpawned, retireIdleSpawned, saveSpawnedSettings, type SpawnedAgent, listApprovals, listNotices, listUsers, llmProviders, markNoticeSeen, mintScimToken, oidcProvider, removeOidcProvider, revokeScimToken, revokeUserSessions, saveEgressCeiling, saveLlmProviders, saveOidcProvider, saveScimGroupRoles, scimConfig, takeEstateBackup, type Approval, type Capacity, type EgressCeiling, type Estate, type EstateRoster, type LlmProviders, type Notice, type OidcProvider, type ScimConfig, type ServerRole, type UserSummary } from "../../engine/net/client";
import { useEngine } from "../../engine/state";
import { useServer } from "../../engine/net/server";
import { setAssistantArchived } from "../../engine/net/client";
import { OvHead, useOverlay } from "../overlay";
import { ago } from "../data";
import { cadenceWords } from "../../engine/forms/agentSpec";
import { argSummary, plainName } from "../agents/words";

/** The notifications drawer: the server's notices to this person, and the
 * runs paused for their decision — decided here, as the platform gate needs. */
export function NotificationsDrawer() {
  const navigate = useNavigate();
  const { close, toast } = useOverlay();
  const [notices, setNotices] = useState<Notice[] | null>(null);
  const [pending, setPending] = useState<Approval[]>([]);
  // Proposals waiting for a person's yes or no, across agents: the inbox is where a decision waits.
  const [proposals, setProposals] = useState<OpenProposal[]>([]);
  const [deciding, setDeciding] = useState("");
  const load = () => {
    void listNotices().then(setNotices).catch(() => setNotices([]));
    void listApprovals("pending").then(setPending).catch(() => setPending([]));
    void listOpenProposals().then(setProposals).catch(() => setProposals([]));
  };
  useEffect(load, []);
  const unread = (notices ?? []).filter((n) => !n.seen_at).length + pending.length;
  const tone = (n: Notice) => (n.about.kind === "approval" ? "warn" : n.about.kind === "task" || n.about.kind === "sweep" ? "bad" : n.about.kind === "schedule" || n.about.kind === "webhook" ? "warn" : "good");
  const icon = (n: Notice) => (n.about.kind === "approval" ? "ti-hand-stop" : n.about.kind === "sweep" ? "ti-target-arrow" : n.about.kind === "task" ? "ti-alert-triangle" : n.about.kind === "schedule" ? "ti-clock-hour-4" : n.about.kind === "webhook" ? "ti-webhook" : "ti-bell");
  const assistants = useServer((s) => s.assistants);
  const [held, setHeld] = useState<Record<string, string>>({});
  async function decide(a: Approval, decision: "approve" | "deny") {
    setDeciding(a.run_id);
    try { await decideApproval(a.run_id, decision); toast(decision === "approve" ? "Approved — the run continues" : "Denied — the run finishes without it", decision === "approve" ? "ti-check" : "ti-x"); setHeld((h) => { const n = { ...h }; delete n[a.run_id]; return n; }); load(); }
    catch (err) { const m = err instanceof Error ? err.message : "the server refused"; setHeld((h) => ({ ...h, [a.run_id]: m })); }
    finally { setDeciding(""); }
  }
  /** The run's agent was put away after the run paused: bring it back, then decide. */
  async function restoreThen(a: Approval, decision: "approve" | "deny") {
    const agent = assistants.find((x) => x.assistant_id === a.assistant_id);
    if (!agent) { setHeld((h) => ({ ...h, [a.run_id]: "the agent is gone" })); return; }
    setDeciding(a.run_id);
    try { await setAssistantArchived(agent.assistant_id, agent.active_version_id ?? "", false); await useServer.getState().refresh(); toast(`${agent.name} restored`, "ti-robot"); }
    catch (err) { setHeld((h) => ({ ...h, [a.run_id]: err instanceof Error ? err.message : "the server refused" })); setDeciding(""); return; }
    setDeciding("");
    await decide(a, decision);
  }
  async function markAll() {
    for (const n of notices ?? []) if (!n.seen_at) await markNoticeSeen(n.notice_id).catch(() => {});
    load(); toast("All caught up");
  }
  return (
    <div className="m-drawer">
      <OvHead icon="ti-bell" title="Notifications" sub={`${pending.length ? `${pending.length} waiting for your yes · ` : ""}${unread} unread${proposals.length ? ` · ${proposals.length} improvement${proposals.length === 1 ? "" : "s"} suggested` : ""}`} />
      <div className="ov-body">
        {pending.length > 0 && <div className="cat-label" style={{ marginBottom: 6 }} data-inbox-approvals><span>Waiting for your yes</span><span className="ln" /><span className="gc">{pending.length}</span></div>}
        {pending.map((a) => (
          <div key={a.run_id} className="notif warn unread" data-approval={a.run_id}>
            <div className="ni"><i className="ti ti-hand-stop" /></div>
            <div className="nb">
              <div className="nt">{a.agent_name ?? "An agent"} wants to {a.requests.length === 1 ? plainName(a.requests[0].tool) : `do ${a.requests.length} things`}</div>
              <div className="nd" title={a.requests.map((r) => `${r.tool} ${JSON.stringify(r.arguments ?? {})}`).join("\n")}>{a.requests.map((r) => { const what = argSummary(JSON.stringify(r.arguments ?? {})); return a.requests.length === 1 ? (what ? `“${what}”` : "") : `${plainName(r.tool)}${what ? ` · “${what}”` : ""}`; }).filter(Boolean).join(" · ")}{" "}It changes something outside, so it waits for your yes.</div>
              <div className="nm">{ago(a.requested_at)}</div>
              {held[a.run_id] && <div className="m-alert" style={{ marginTop: 8 }}><i className="ti ti-alert-triangle" style={{ color: "var(--bad)", fontSize: 16 }} /><div className="a-body"><div className="a-text">{held[a.run_id]}</div></div></div>}
              <div style={{ display: "flex", gap: 6, marginTop: 8, flexWrap: "wrap" }}>
                {/archived/.test(held[a.run_id] ?? "") ? (
                  <><button className="m-btn primary sm" disabled={deciding === a.run_id} onClick={() => void restoreThen(a, "approve")}><i className="ti ti-robot" /> Restore agent &amp; approve</button><button className="m-btn secondary sm" disabled={deciding === a.run_id} onClick={() => void restoreThen(a, "deny")}>Restore &amp; deny</button></>
                ) : (
                  <><button className="m-btn primary sm" disabled={deciding === a.run_id} onClick={() => void decide(a, "approve")}><i className="ti ti-check" /> Approve</button><button className="m-btn secondary sm" disabled={deciding === a.run_id} onClick={() => void decide(a, "deny")}>Deny</button></>
                )}
              </div>
            </div>
          </div>
        ))}
        {proposals.length > 0 && <div className="cat-label" style={{ marginBottom: 6 }} data-inbox-proposals><span>Suggested improvements</span><span className="ln" /><span className="gc">{proposals.length}</span></div>}
        {proposals.map((p) => (
          <div key={p.version_id} className="notif good unread" data-proposal={p.version_id}>
            <div className="ni"><i className="ti ti-sparkles" /></div>
            <div className="nb">
              <div className="nt">{/^gap\b/i.test(p.proposed_by) ? `${p.agent_name} was asked something it could not do — a fix is suggested` : `${p.proposed_by === "Coach" ? "The Coach" : p.proposed_by} suggests a change to ${p.agent_name}`}</div>
              <div className="nd">{(() => { const why = (p.why || "No reason given.").replace(/`([a-z0-9_.:-]+)`/gi, (_m: string, id: string) => plainName(id)); return why.length > 220 ? `${why.slice(0, 220)}…` : why; })()}</div>
              <div className="nm">{ago(p.created_at)}</div>
              <div style={{ display: "flex", gap: 6, marginTop: 8 }}><button className="m-btn secondary sm" data-flow="review-proposal" onClick={() => { close(); navigate({ to: "/agents/$id", params: { id: p.assistant_id } }); }}><i className="ti ti-arrow-right" /> Review on {p.agent_name}</button></div>
            </div>
          </div>
        ))}
        {notices === null && <div className="thread-empty"><i className="ti ti-bell" />Reading…</div>}
        {notices?.map((n) => (
          <div key={n.notice_id} className={`notif ${tone(n)}${n.seen_at ? "" : " unread"}`} data-notice={n.notice_id} onClick={() => {
            void markNoticeSeen(n.notice_id).catch(() => {});
            close();
            if (n.about.run_id) navigate({ to: "/analytics", search: { run: n.about.run_id } });
            else if (n.about.dataset) navigate({ to: "/evals", search: { dataset: n.about.dataset } });
            else navigate({ to: "/agents" });
          }}>
            <div className="ni"><i className={`ti ${icon(n)}`} /></div>
            <div className="nb"><div className="nt">{n.title}</div><div className="nd">{n.text}</div><div className="nm">{ago(n.created_at)}</div></div>
          </div>
        ))}
        {notices?.length === 0 && pending.length === 0 && <div className="thread-empty"><i className="ti ti-bell" />Nothing needs you.</div>}
      </div>
      <div className="ov-foot"><button className="m-btn ghost sm" onClick={() => void markAll()}>Mark all read</button><div className="sp" /><button className="m-btn secondary" data-close>Done</button></div>
    </div>
  );
}

/** Workspace settings: what the deployment really holds — its models, the people on it, how they sign in, what it may reach, and where it lives. */
const TABS = ["Workspace", "Members", "Sign-in", "Security", "Estate"] as const;
type Tab = (typeof TABS)[number];
const ROLES: ServerRole[] = ["admin", "builder", "operator", "auditor"];

export function SettingsModal({ tab: start = "Workspace" }: { tab?: Tab }) {
  const [tab, setTab] = useState<Tab>(start);
  const [users, setUsers] = useState<UserSummary[] | null>(null);
  const me = useServer((s) => s.me);
  useEffect(() => { void listUsers().then(setUsers).catch(() => setUsers([])); }, []);
  return (
    <div className="m-modal lg">
      <OvHead icon="ti-settings" title="Workspace settings" sub={`${me?.tenant ?? "workspace"} · ${users ? `${users.length} member${users.length === 1 ? "" : "s"}` : "…"}`} />
      <div className="ov-body">
        <div className="m-seg set-tabs">{TABS.map((t) => <button key={t} className={tab === t ? "on" : ""} data-tab={t} onClick={() => setTab(t)}>{t}</button>)}</div>
        <div data-pane>
          {tab === "Workspace" && <WorkspaceTab />}
          {tab === "Members" && <MembersTab users={users} onChanged={() => void listUsers().then(setUsers).catch(() => {})} />}
          {tab === "Sign-in" && <SignInTab />}
          {tab === "Security" && <SecurityTab />}
          {tab === "Estate" && <EstateTab />}
        </div>
      </div>
      <div className="ov-foot"><div className="sp" /><button className="m-btn secondary" data-close>Done</button></div>
    </div>
  );
}

function WorkspaceTab() {
  const { toast } = useOverlay();
  const me = useServer((s) => s.me);
  const theme = useEngine((s) => s.theme);
  const toggleTheme = useEngine((s) => s.toggleTheme);
  const [providers, setProviders] = useState<LlmProviders | null>(null);
  useEffect(() => { void llmProviders().then(setProviders).catch(() => setProviders(null)); }, []);
  const pick = async (slot: "primary" | "fallback", id: string) => {
    if (!providers) return;
    try { const c = await saveLlmProviders({ providers: providers.providers.map((p) => ({ id: p.id, name: p.name, base_url: p.base_url, model: p.model, extra_body: p.extra_body ?? null, price_input_per_m: p.price_input_per_m ?? null, price_output_per_m: p.price_output_per_m ?? null, price_cached_input_per_m: p.price_cached_input_per_m ?? null })), primary: slot === "primary" ? id : providers.primary, fallback: slot === "fallback" ? (id || null) : providers.fallback }); setProviders(c); toast(c.applied_error ?? "Applied live", c.applied_error ? "ti-alert-triangle" : "ti-cpu"); }
    catch (err) { toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle"); }
  };
  return (
    <div>
      <div className="set-row"><div className="sk"><div className="n">Workspace</div><div className="d">The tenant this browser is signed into</div></div><input className="m-input" value={me?.tenant ?? ""} readOnly /></div>
      <div className="set-row"><div className="sk"><div className="n">Default model</div><div className="d">Every run's reasoning engine unless an agent picks its own</div></div><select className="m-input" value={providers?.primary ?? ""} onChange={(e) => void pick("primary", e.target.value)} disabled={!providers}>{providers?.providers.map((p) => <option key={p.id} value={p.id}>{p.name} · {p.model}</option>)}</select></div>
      <div className="set-row"><div className="sk"><div className="n">Fallback model</div><div className="d">Used when the primary fails</div></div><select className="m-input" value={providers?.fallback ?? ""} onChange={(e) => void pick("fallback", e.target.value)} disabled={!providers}><option value="">None</option>{providers?.providers.filter((p) => p.id !== providers.primary).map((p) => <option key={p.id} value={p.id}>{p.name} · {p.model}</option>)}</select></div>
      <div className="set-row"><div className="sk"><div className="n">Signed in as</div><div className="d">{me?.roles.join(" · ") ?? ""}</div></div><span className="mono" style={{ fontSize: 12.5 }}>{me?.principal.name ?? "—"}</span></div>
      <div className="set-row"><div className="sk"><div className="n">Theme</div><div className="d">This browser</div></div><div className="m-seg"><button className={theme === "light" ? "on" : ""} onClick={() => theme !== "light" && toggleTheme()}>Light</button><button className={theme === "dark" ? "on" : ""} onClick={() => theme !== "dark" && toggleTheme()}>Dark</button></div></div>
    </div>
  );
}

function MembersTab({ users, onChanged }: { users: UserSummary[] | null; onChanged: () => void }) {
  const { toast } = useOverlay();
  const me = useServer((s) => s.me);
  const admin = !!me?.roles.includes("admin");
  const [adding, setAdding] = useState(false);
  const [forgetting, setForgetting] = useState<{ id: string; name: string; reason: string } | null>(null);
  const [removing, setRemoving] = useState<{ id: string; name: string } | null>(null);
  const [f, setF] = useState({ username: "", name: "", role: "builder" as ServerRole, password: "" });
  const [busy, setBusy] = useState(false);
  const say = (err: unknown) => toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle");
  async function add() {
    setBusy(true);
    try { await createUser({ username: f.username.trim(), name: f.name.trim() || undefined, roles: [f.role], password: f.password }); toast(`${f.name || f.username} added as ${f.role}`, "ti-user-plus"); setAdding(false); setF({ username: "", name: "", role: "builder", password: "" }); onChanged(); }
    catch (err) { say(err); } finally { setBusy(false); }
  }
  return (
    <div>
      {users === null && <div className="thread-empty">Reading…</div>}
      {users?.map((u) => (
        <div key={u.id} className="member">
          <div className="av" style={{ background: "var(--ink-900)" }}>{(u.name ?? u.id).split(/\s+/).map((w) => w[0]).join("").slice(0, 2).toUpperCase()}</div>
          <div className="mn">{u.name ?? u.id}<small>{u.id}{u.external ? " · signs in with single sign-on" : ""}{u.active === false ? " · no longer active" : ""}</small></div>
          <span className="item-tag">{u.roles.join(" · ")}</span>
          {admin && u.id !== me?.principal.id && <><button className="m-btn ghost sm" title="Sign the person out everywhere" onClick={async () => { try { await revokeUserSessions(u.id); toast(`${u.name ?? u.id} signed out everywhere`, "ti-logout"); } catch (err) { say(err); } }}><i className="ti ti-logout" /></button><button className="m-btn ghost sm" title="Remove the account" data-flow="remove-person" onClick={() => setRemoving({ id: u.id, name: u.name ?? u.id })}><i className="ti ti-trash" /></button><button className="m-btn ghost sm" title="Forget for good: their memory, conversations, runs and grants go, every agent's notes about them are scrubbed, and their key is destroyed" data-flow="forget-person" onClick={() => setForgetting({ id: u.id, name: u.name ?? u.id, reason: "" })}><i className="ti ti-eraser" /></button></>}
        </div>
      ))}
      {users?.length === 0 && <div className="thread-empty">Nobody else is on this workspace.</div>}
      {admin && !adding && <div style={{ display: "flex", gap: 8, marginTop: 14, alignItems: "center" }}><button className="m-btn secondary sm" data-invite onClick={() => setAdding(true)}><i className="ti ti-user-plus" /> Add a person</button><button className="m-btn ghost sm" data-flow="forget-someone" title="A person removed earlier still has memory, runs and a key on the box until they are forgotten" onClick={() => setForgetting({ id: "", name: "", reason: "" })}><i className="ti ti-eraser" /> Forget someone who already left</button></div>}
      {removing && (
        <div className="m-card" style={{ padding: 14, marginTop: 14 }} data-remove-form>
          <div style={{ fontWeight: 600, marginBottom: 6 }}>Remove {removing.name}'s account?</div>
          <div className="m-hint" style={{ marginBottom: 10 }}>They are signed out and can no longer sign in. Their history stays on the box — memory, conversations, runs — until someone forgets them for good.</div>
          <div style={{ display: "flex", gap: 8 }}>
            <button className="m-btn danger sm" data-flow="remove-confirm" disabled={busy} onClick={async () => { setBusy(true); try { await deleteUser(removing.id); toast(`${removing.name} removed`, "ti-user-minus"); setRemoving(null); onChanged(); } catch (err) { say(err); } finally { setBusy(false); } }}><i className="ti ti-trash" /> Remove</button>
            <button className="m-btn ghost sm" data-flow="remove-cancel" onClick={() => setRemoving(null)}>Cancel</button>
          </div>
        </div>
      )}
      {forgetting && (
        <div className="m-card" style={{ padding: 14, marginTop: 14 }} data-forget-form>
          <div style={{ fontWeight: 600, marginBottom: 6 }}>Forget {forgetting.name || "a person"} for good</div>
          <div className="m-hint" style={{ marginBottom: 10 }}>Everything kept in their name goes: their memory and the notes agents proposed about them, every agent's block lines that name them, their conversations, the runs that acted for them, the connections granted in their name — and their key is destroyed, so any sealed copy anywhere is ciphertext from now on. What stays is a tombstone with the reason. This cannot be undone.</div>
          {!forgetting.name && <div className="fld" style={{ marginBottom: 8 }}><label className="fld-label">Sign-in name</label><input className="m-input" placeholder="the id they signed in with" value={forgetting.id} onChange={(e) => setForgetting({ ...forgetting, id: e.target.value })} autoComplete="off" /></div>}
          <div className="fld"><label className="fld-label">Why — kept on the tombstone</label><input className="m-input" placeholder="left the company; erasure requested 2026-09-23" value={forgetting.reason} onChange={(e) => setForgetting({ ...forgetting, reason: e.target.value })} autoComplete="off" /></div>
          <div style={{ display: "flex", gap: 8, marginTop: 10 }}>
            <button className="m-btn danger sm" data-flow="forget-confirm" disabled={busy || !forgetting.id.trim() || !forgetting.reason.trim()} onClick={async () => {
              setBusy(true);
              try {
                const r = await forgetUser(forgetting.id.trim(), forgetting.reason.trim());
                const lines = (r.blocks_scrubbed ?? []).reduce((n, b) => n + b.lines_removed, 0);
                toast(`${forgetting.name || forgetting.id} forgotten — ${r.memory.forgotten} memor${r.memory.forgotten === 1 ? "y" : "ies"}, ${lines} block line${lines === 1 ? "" : "s"} in ${(r.blocks_scrubbed ?? []).length} agent block${(r.blocks_scrubbed ?? []).length === 1 ? "" : "s"}, ${r.runs_removed} run${r.runs_removed === 1 ? "" : "s"}, ${r.threads_removed} conversation${r.threads_removed === 1 ? "" : "s"}${r.key_destroyed ? ", key destroyed" : ""}`, "ti-eraser");
                setForgetting(null); onChanged();
              } catch (err) { say(err); } finally { setBusy(false); }
            }}><i className="ti ti-eraser" /> Forget for good</button>
            <button className="m-btn ghost sm" onClick={() => setForgetting(null)}>Cancel</button>
          </div>
        </div>
      )}
      {adding && (
        <div className="m-card" style={{ padding: 14, marginTop: 14 }}>
          <div className="frow two"><div className="fld"><label className="fld-label">Sign-in name</label><input className="m-input" value={f.username} onChange={(e) => setF({ ...f, username: e.target.value })} autoComplete="off" /></div><div className="fld"><label className="fld-label">Name</label><input className="m-input" value={f.name} onChange={(e) => setF({ ...f, name: e.target.value })} /></div></div>
          <div className="frow two"><div className="fld"><label className="fld-label">Role</label><select className="m-input" value={f.role} onChange={(e) => setF({ ...f, role: e.target.value as ServerRole })}>{ROLES.map((r) => <option key={r}>{r}</option>)}</select></div><div className="fld"><label className="fld-label">First password <span className="opt">— they change it after signing in</span></label><input className="m-input" type="password" value={f.password} onChange={(e) => setF({ ...f, password: e.target.value })} autoComplete="new-password" /></div></div>
          <div style={{ display: "flex", gap: 8, justifyContent: "flex-end" }}><button className="m-btn ghost sm" onClick={() => setAdding(false)}>Cancel</button><button className="m-btn primary sm" disabled={busy || !f.username.trim() || !f.password} onClick={() => void add()}><i className="ti ti-check" /> Add</button></div>
        </div>
      )}
    </div>
  );
}

function SignInTab() {
  const { toast } = useOverlay();
  const [oidc, setOidc] = useState<{ provider: OidcProvider | null; callback_url: string } | null>(null);
  const [scim, setScim] = useState<ScimConfig | null>(null);
  const [editing, setEditing] = useState(false);
  const [f, setF] = useState({ name: "", issuer: "", client_id: "", client_secret: "", default_role: "builder" as ServerRole, scopes: "openid profile email" });
  const [minted, setMinted] = useState<string | null>(null);
  const say = (err: unknown) => toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle");
  const load = () => { oidcProvider().then((o) => { setOidc(o); if (o.provider) setF((x) => ({ ...x, name: o.provider!.name, issuer: o.provider!.issuer, client_id: o.provider!.client_id, default_role: o.provider!.default_role, scopes: o.provider!.scopes })); }).catch(() => setOidc({ provider: null, callback_url: "" })); scimConfig().then(setScim).catch(() => setScim(null)); };
  useEffect(load, []);
  return (
    <div>
      <div className="cat-label"><span>Single sign-on (OIDC)</span><span className="ln" /></div>
      {oidc?.provider && !editing && <div className="kv"><span className="k">Provider</span><span className="v">{oidc.provider.name}</span><span className="k">Issuer</span><span className="v mono">{oidc.provider.issuer}</span><span className="k">Client</span><span className="v mono">{oidc.provider.client_id}{oidc.provider.has_secret ? " · secret held" : ""}</span><span className="k">New people join as</span><span className="v">{oidc.provider.default_role}</span><span className="k">Callback</span><span className="v mono">{oidc.callback_url}</span></div>}
      {oidc && !oidc.provider && !editing && <div className="m-hint">No identity provider; people sign in with a name and password.</div>}
      {editing && (
        <div className="m-card" style={{ padding: 14 }}>
          <div className="frow two"><div className="fld"><label className="fld-label">Name</label><input className="m-input" value={f.name} onChange={(e) => setF({ ...f, name: e.target.value })} placeholder="Okta" /></div><div className="fld"><label className="fld-label">Issuer URL</label><input className="m-input" value={f.issuer} onChange={(e) => setF({ ...f, issuer: e.target.value })} placeholder="https://…/.well-known/openid-configuration's issuer" style={{ fontFamily: "var(--font-mono)" }} /></div></div>
          <div className="frow two"><div className="fld"><label className="fld-label">Client id</label><input className="m-input" value={f.client_id} onChange={(e) => setF({ ...f, client_id: e.target.value })} style={{ fontFamily: "var(--font-mono)" }} /></div><div className="fld"><label className="fld-label">Client secret{oidc?.provider?.has_secret ? <span className="opt"> — held; paste to rotate</span> : ""}</label><input className="m-input" type="password" value={f.client_secret} onChange={(e) => setF({ ...f, client_secret: e.target.value })} autoComplete="off" /></div></div>
          <div className="frow two"><div className="fld"><label className="fld-label">New people join as</label><select className="m-input" value={f.default_role} onChange={(e) => setF({ ...f, default_role: e.target.value as ServerRole })}>{ROLES.map((r) => <option key={r}>{r}</option>)}</select></div><div className="fld"><label className="fld-label">Scopes</label><input className="m-input" value={f.scopes} onChange={(e) => setF({ ...f, scopes: e.target.value })} /></div></div>
          {oidc?.callback_url && <div className="m-hint">Register this callback with the provider: <span className="mono">{oidc.callback_url}</span></div>}
          <div style={{ display: "flex", gap: 8, justifyContent: "flex-end", marginTop: 10 }}><button className="m-btn ghost sm" onClick={() => setEditing(false)}>Cancel</button><button className="m-btn primary sm" disabled={!f.name || !f.issuer || !f.client_id} onClick={async () => { try { await saveOidcProvider({ name: f.name, issuer: f.issuer, client_id: f.client_id, ...(f.client_secret ? { client_secret: f.client_secret } : {}), default_role: f.default_role, scopes: f.scopes }); toast("Identity provider saved", "ti-shield-lock"); setEditing(false); load(); } catch (err) { say(err); } }}><i className="ti ti-check" /> Save</button></div>
        </div>
      )}
      {!editing && <div style={{ display: "flex", gap: 8, marginTop: 10 }}><button className="m-btn secondary sm" onClick={() => setEditing(true)}><i className="ti ti-pencil" /> {oidc?.provider ? "Edit" : "Set up"}</button>{oidc?.provider && <button className="m-btn ghost sm" onClick={async () => { try { await removeOidcProvider(); toast("Identity provider removed"); load(); } catch (err) { say(err); } }}>Remove</button>}</div>}
      <div className="cat-label" style={{ marginTop: 22 }}><span>Directory sync (SCIM)</span><span className="ln" /></div>
      {scim && <div className="kv"><span className="k">Endpoint</span><span className="v mono">{scim.base_url}</span><span className="k">Token</span><span className="v">{scim.has_token ? `held${scim.token_minted_at ? ` · minted ${ago(scim.token_minted_at)}` : ""}` : "none"}</span><span className="k">Last seen</span><span className="v">{scim.last_seen_at ? ago(scim.last_seen_at) : "never"}</span><span className="k">People</span><span className="v">{scim.provisioned} provisioned · {scim.deactivated} deactivated</span></div>}
      {minted && <div className="m-alert info"><div className="a-ic"><i className="ti ti-key" /></div><div className="a-body"><div className="a-title">Token minted — shown once</div><div className="a-text mono" style={{ fontFamily: "var(--font-mono)", fontSize: 11.5, overflowWrap: "anywhere" }}>{minted}</div></div></div>}
      <div style={{ display: "flex", gap: 8, marginTop: 10 }}><button className="m-btn secondary sm" onClick={async () => { try { const r = await mintScimToken(); setMinted(r.token); setScim(r.config); } catch (err) { say(err); } }}><i className="ti ti-key" /> {scim?.has_token ? "Rotate token" : "Mint token"}</button>{scim?.has_token && <button className="m-btn ghost sm" onClick={async () => { try { const r = await revokeScimToken(); setScim(r.config); setMinted(null); toast("SCIM token revoked"); } catch (err) { say(err); } }}>Revoke</button>}</div>
      {scim && scim.groups && scim.groups.length > 0 && (
        <><div className="cat-label" style={{ marginTop: 22 }}><span>Groups → roles</span><span className="ln" /></div>
          {scim.groups.map((g) => { const cur = scim.group_roles.find((x) => x.group === g.display_name || x.group === g.id)?.role ?? ""; return <div key={g.id} className="set-row"><div className="sk"><div className="n">{g.display_name}</div><div className="d">{g.members} member{g.members === 1 ? "" : "s"}</div></div><select className="m-input" style={{ width: 140 }} value={cur} onChange={async (e) => { const next = scim.group_roles.filter((x) => x.group !== g.display_name && x.group !== g.id); if (e.target.value) next.push({ group: g.display_name, role: e.target.value as ServerRole }); try { setScim(await saveScimGroupRoles(next)); toast("Group roles saved"); } catch (err) { say(err); } }}><option value="">no role</option>{ROLES.map((r) => <option key={r}>{r}</option>)}</select></div>; })}
        </>
      )}
    </div>
  );
}

function SecurityTab() {
  const { toast } = useOverlay();
  const [ceiling, setCeiling] = useState<EgressCeiling | null>(null);
  const [host, setHost] = useState("");
  const say = (err: unknown) => toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle");
  useEffect(() => { egressCeiling().then(setCeiling).catch(() => setCeiling(null)); }, []);
  const save = async (next: { open: boolean; hosts: { host: string; note?: string | null }[] }) => { try { setCeiling(await saveEgressCeiling(next)); toast("Saved", "ti-shield-check"); } catch (err) { say(err); } };
  if (!ceiling) return <div className="thread-empty">Reading…</div>;
  return (
    <div>
      <div className="set-row"><div className="sk"><div className="n">Only the sites below</div><div className="d">{ceiling.open ? "Off: agents and their connections may reach any site on the internet." : "On: agents and their connections may reach only the sites listed here."}</div></div><div className={`m-switch${ceiling.open ? "" : " on"}`} data-switch onClick={() => void save({ open: !ceiling.open, hosts: ceiling.hosts.map((h) => ({ host: h.host, note: h.note ?? null })) })} /></div>
      <div className="cat-label"><span>Sites agents may reach</span><span className="ln" /><span className="gc">{ceiling.hosts.length}</span></div>
      {ceiling.hosts.map((h) => <div key={h.host} className="scope-row"><div className="sb"><div>{h.host}</div><div className="sd">{[h.note === "RUSTY_EGRESS_ALLOW" ? "set in the server's configuration" : h.note && !/^in use by .* when the ceiling was first kept$/.test(h.note) ? h.note : "", h.used_by?.length ? `used by ${[...new Set(h.used_by.map((u) => u.name))].join(", ")}` : ""].filter(Boolean).join(" · ")}</div></div><button className="m-btn ghost sm icon" title="Remove" onClick={() => void save({ open: ceiling.open, hosts: ceiling.hosts.filter((x) => x.host !== h.host).map((x) => ({ host: x.host, note: x.note ?? null })) })}><i className="ti ti-trash" /></button></div>)}
      <div style={{ display: "flex", gap: 8, marginTop: 10 }}><input className="m-input" placeholder="Add a site, e.g. api.example.com" value={host} onChange={(e) => setHost(e.target.value)} /><button className="m-btn secondary sm" disabled={!host.trim()} onClick={() => { void save({ open: ceiling.open, hosts: [...ceiling.hosts.map((x) => ({ host: x.host, note: x.note ?? null })), { host: host.trim(), note: null }] }); setHost(""); }}><i className="ti ti-plus" /> Allow</button></div>
      {ceiling.unlisted.length > 0 && <><div className="cat-label" style={{ marginTop: 22 }}><span>Reached while it was off, not on the list</span><span className="ln" /></div>{ceiling.unlisted.map((u) => <div key={u.host} className="scope-row"><div className="sb"><div>{u.host}</div><div className="sd">by {u.connections.map((c) => c.name).join(", ")}</div></div><button className="m-btn ghost sm" onClick={() => void save({ open: ceiling.open, hosts: [...ceiling.hosts.map((x) => ({ host: x.host, note: x.note ?? null })), { host: u.host, note: null }] })}>Allow</button></div>)}</>}
    </div>
  );
}

function EstateTab() {
  const { toast } = useOverlay();
  const [est, setEst] = useState<Estate | null>(null);
  const [cap, setCap] = useState<Capacity | null>(null);
  const [roster, setRoster] = useState<EstateRoster | null>(null);
  // The fleet agents made: idle ones are retired here, or nightly after the default.
  const [spawned, setSpawned] = useState<{ spawned: SpawnedAgent[]; retire_idle_days: number; retire_idle_days_default: number } | null>(null);
  const [showRetired, setShowRetired] = useState(false);

  const agents = useServer((s) => s.assistants);
  const agentName = (id?: string | null) => (id ? agents.find((a) => a.assistant_id === id)?.name ?? id : null);
  const [busy, setBusy] = useState(false);
  const load = () => { estate().then(setEst).catch(() => setEst(null)); capacity(60).then(setCap).catch(() => setCap(null)); estateRoster(7).then(setRoster).catch(() => setRoster(null)); estateSpawned().then(setSpawned).catch(() => setSpawned(null)); };
  useEffect(load, []);
  const mb = (b: number) => `${(b / 1_048_576).toFixed(1)} MB`;
  return (
    <div>
      {est && <div className="kv"><span className="k">Stored</span><span className="v" title={est.store.path}>{est.store.kind === "files" ? "in files on this machine" : `in ${est.store.kind}`} · {mb(est.store.bytes)}</span><span className="k">Version</span><span className="v">{est.server_version}</span>{est.restored_from && <><span className="k">Restored</span><span className="v" title={est.restored_from.archive}>from a backup, {ago(est.restored_from.at)}</span></>}</div>}
      {cap && <><div className="cat-label"><span>Capacity · last {cap.window.minutes} min</span><span className="ln" /></div><div className="kv"><span className="k">Now</span><span className="v">{cap.now.runs_running} running · {cap.now.runs_queued} queued</span><span className="k">Finished</span><span className="v">{cap.window.runs_finished} runs · {cap.window.runs_per_minute.toFixed(2)}/min</span><span className="k">Execution</span><span className="v">median {cap.window.median_execution_ms != null ? `${(cap.window.median_execution_ms / 1000).toFixed(1)}s` : "—"} · slowest {cap.window.slowest_execution_ms != null ? `${(cap.window.slowest_execution_ms / 1000).toFixed(1)}s` : "—"}</span></div><div className="m-hint">{cap.bites_first}</div></>}
      {roster && <>
        <div className="cat-label" style={{ marginTop: 22 }}><span>The last {roster.days} days</span><span className="ln" /></div>
        <div className="lib-table-wrap"><table className="m-table" data-roster><thead><tr><th>Day</th><th className="num">Runs</th><th className="num">Notes kept</th><th className="num">Space used</th></tr></thead><tbody>
          {roster.by_day.map((d) => <tr key={d.day}><td>{new Date(`${d.day}T12:00:00Z`).toLocaleDateString(undefined, { weekday: "short", month: "short", day: "numeric" })}</td><td className="num">{d.runs}</td><td className="num">{d.notes}</td><td className="num">{mb(d.journal_bytes)}</td></tr>)}
        </tbody></table></div>
        <div className="m-hint">Run records use {mb(roster.journal_bytes_total)} in all · {roster.notes_total.toLocaleString()} notes kept.</div>
        {roster.schedules.length > 0 && <>
          <div className="cat-label" style={{ marginTop: 16 }}><span>Schedules</span><span className="ln" /><span className="gc">{roster.schedules.length}</span></div>
          {roster.schedules.map((s) => <div key={s.cron_id} className="scope-row" data-roster-schedule={s.cron_id}><div className="sb"><div>{agentName(s.assistant_id) ?? "A schedule"}{s.interval_secs || s.cron_expr ? ` · runs ${cadenceWords({ interval_secs: s.interval_secs ?? undefined, cron_expr: s.cron_expr ?? undefined })}` : ""}</div><div className="sd">{s.runs_fired} run{s.runs_fired === 1 ? "" : "s"} so far{s.last_run_at ? ` · last ${ago(s.last_run_at)}` : ""}{s.late_max_secs != null && s.late_max_secs > 0 ? ` · up to ${Math.round(s.late_max_secs / 60)} min late` : ""}</div></div>{s.drift === "late" && <span className="item-tag bad">Running late</span>}</div>)}
        </>}
      </>}
      {spawned && spawned.spawned.length > 0 && (() => {
        // Helpers — agents an agent made — in the words of the person who looks after them.
        const live = spawned.spawned.filter((s) => !s.archived_at);
        const retired = spawned.spawned.length - live.length;
        const rows = showRetired ? spawned.spawned : live;
        const after = spawned.retire_idle_days;
        const facts = (s: SpawnedAgent) => [
          ...s.schedules.map((c) => `runs ${cadenceWords({ interval_secs: c.interval_secs ?? undefined, cron_expr: c.cron_expr ?? undefined })}`),
          s.notes ? `${s.notes} note${s.notes === 1 ? "" : "s"}` : "",
          s.open_gaps ? `${s.open_gaps} open question${s.open_gaps === 1 ? "" : "s"}` : "",
        ].filter(Boolean).join(" · ");
        const saveAfter = async (days: number) => { setBusy(true); try { await saveSpawnedSettings(days); toast(days > 0 ? `Idle helpers are retired after ${days} days` : "Helpers are never retired on their own", "ti-check"); load(); } catch (err) { toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle"); } finally { setBusy(false); } };
        return <>
          <div className="cat-label" style={{ marginTop: 22 }}><span>Helper agents</span><span className="ln" /><span className="gc">{live.length}</span></div>
          <div className="m-hint" style={{ marginBottom: 8 }}>Agents your agents made to help with their work. When one is retired, what it learned goes back to the agent that made it.</div>
          <div className="lib-table-wrap"><table className="m-table" data-spawned><thead><tr><th>Helper</th><th>Made by</th><th>Last active</th><th>Status</th></tr></thead><tbody>
            {rows.map((s) => <tr key={s.assistant_id} data-spawned-agent={s.assistant_id}>
              <td><div>{s.name}</div>{facts(s) && <div style={{ fontSize: 12, color: "var(--ink-500)" }} data-flow="spawned-facts">{facts(s)}</div>}</td>
              <td style={{ color: "var(--ink-600)" }}>{agentName(s.spawned_by.assistant_id) ?? "—"}</td>
              <td style={{ color: "var(--ink-500)" }}>{s.last_run_at ? ago(s.last_run_at) : "never"}</td>
              <td>{s.archived_at ? <span className="item-tag">Retired</span> : s.idle_days >= 7 ? <span className="m-badge warn sm">Idle {s.idle_days} days</span> : <span className="m-badge good sm"><span className="dot" /> Active</span>}</td>
            </tr>)}
            {rows.length === 0 && <tr><td colSpan={4} style={{ color: "var(--ink-500)" }}>No active helpers.</td></tr>}
          </tbody></table></div>
          <div style={{ display: "flex", gap: 8, alignItems: "center", marginTop: 8, flexWrap: "wrap" }}>
            <span style={{ fontSize: 12, color: "var(--ink-600)" }}>Retire a helper after</span>
            <select className="m-input" style={{ width: 120 }} value={after} disabled={busy} data-flow="nightly-idle-days" onChange={(e) => void saveAfter(Number(e.target.value))}>
              {[...new Set([7, 14, 30, 60, after].filter((d) => d > 0))].sort((a, b) => a - b).map((d) => <option key={d} value={d}>{d} idle days</option>)}
              <option value={0}>never</option>
            </select>
            <span className="sp" />
            {retired > 0 && <button className="m-btn ghost sm" onClick={() => setShowRetired((v) => !v)}>{showRetired ? "Hide retired" : `Show retired (${retired})`}</button>}
            <button className="m-btn secondary sm" data-flow="retire-idle" disabled={busy || after === 0} title={after === 0 ? "Choose how many idle days first" : undefined} onClick={async () => { setBusy(true); try { const r = await retireIdleSpawned(after); toast(r.retired.length ? `Retired ${r.retired.map((x) => x.name).join(", ")}` : `No helper has been idle ${r.idle_days} days`, "ti-moon"); load(); } catch (err) { toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle"); } finally { setBusy(false); } }}>Retire idle helpers now</button>
          </div>
        </>;
      })()}
      <div className="cat-label" style={{ marginTop: 22 }}><span>Backups</span><span className="ln" /><span className="gc">{est?.backups.length ?? 0}</span></div>
      {est?.backups.slice(0, 8).map((b) => <div key={b.name} className="scope-row"><div className="sb"><div title={b.name}>{(() => { const m = /(\d{4})(\d{2})(\d{2})T(\d{2})(\d{2})(\d{2})Z/.exec(b.name); return m ? new Date(`${m[1]}-${m[2]}-${m[3]}T${m[4]}:${m[5]}:${m[6]}Z`).toLocaleString(undefined, { dateStyle: "medium", timeStyle: "short" }) : b.name; })()}</div><div className="sd">{mb(b.bytes)}{b.person_keys ? " · personal data locked separately" : ""}</div></div></div>)}
      <button className="m-btn secondary sm" style={{ marginTop: 10 }} disabled={busy} onClick={async () => { setBusy(true); try { const b = await takeEstateBackup(); toast("Backed up", "ti-archive"); load(); } catch (err) { toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle"); } finally { setBusy(false); } }}><i className="ti ti-archive" /> Back up now</button>
    </div>
  );
}
