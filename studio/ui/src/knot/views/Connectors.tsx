import { useEffect, useMemo, useState } from "react";
import { useNavigate } from "@tanstack/react-router";
import { authorizeConnection, checkConnectorInstance, listConnectorInstances, listConnectorManifests, listMcpServers, listWorlds, revokeConnection, rotateConnection, upgradeConnection, type ConnectorInstance, type ConnectorManifest, type McpServer, type World } from "../../engine/net/client";
import { McpServerDrawer, NewWorldModal, WorldDrawer } from "../flows/connectorFlows";
import { useServer } from "../../engine/net/server";
import { Badge, OvHead, useOverlay } from "../overlay";
import { AddConnectorDrawer, CredentialModal, authLabel } from "../flows/agentFlows";
import { UsedStack } from "./Skills";
import { ago, riskOf } from "../data";
import { firstSentence, plainName } from "../agents/words";

const grantWord = (i: ConnectorInstance): [tone: "good" | "warn" | "bad", word: string] => {
  const a = i.authorization;
  if (!a || a.kind === "not_required" || a.kind === "connected") return ["good", "Connected"];
  return a.kind === "expired" ? ["bad", "Expired"] : ["warn", "Needs authorization"];
};

/** Connectors: the library of systems this server can reach, and the connections made to them. */
export function ConnectorsView() {
  const { open } = useOverlay();
  const assistants = useServer((s) => s.assistants);
  const [manifests, setManifests] = useState<ConnectorManifest[]>([]);
  const [instances, setInstances] = useState<ConnectorInstance[]>([]);
  const [mcp, setMcp] = useState<McpServer[]>([]);
  const [worlds, setWorlds] = useState<World[]>([]);
  const [q, setQ] = useState("");
  const reload = () => { listConnectorManifests().then(setManifests).catch(() => {}); listConnectorInstances().then(setInstances).catch(() => {}); listMcpServers().then((r) => setMcp(r.servers)).catch(() => {}); listWorlds().then(setWorlds).catch(() => {}); };
  useEffect(reload, []);
  const byHash = useMemo(() => { const m = new Map<string, ConnectorInstance[]>(); for (const i of instances) m.set(i.manifest_hash, [...(m.get(i.manifest_hash) ?? []), i]); return m; }, [instances]);
  const agentsOf = (i: ConnectorInstance) => assistants.filter((a) => !a.archived_at && (a.config?.studio_intent?.tools ?? []).some((t) => (i.tools ?? []).includes(t.name)));
  const shown = manifests.filter((m) => !q || `${m.display_name} ${m.description}`.toLowerCase().includes(q.toLowerCase()));
  const connected = shown.filter((m) => byHash.has(m.hash)), available = shown.filter((m) => !byHash.has(m.hash));
  const attention = instances.filter((i) => grantWord(i)[0] !== "good");
  const inUse = instances.filter((i) => agentsOf(i).length > 0);
  const card = (m: ConnectorManifest) => {
    const inst = byHash.get(m.hash) ?? [];
    const first = inst[0];
    const grant = first ? grantWord(first) : null;
    const a = authLabel(m);
    return (
      <div key={m.hash} className="lcard" data-conn={m.display_name} onClick={() => first ? open("drawer", <ManageDrawer manifest={m} instance={first} agents={agentsOf(first)} onChanged={reload} standIns={worlds.filter((w) => w.instance_id === first.instance_id || w.connector === m.id)} newer={manifests.filter((x) => x.id === m.id && x.hash !== m.hash && x.version > m.version).sort((x, y) => (x.version < y.version ? 1 : -1))[0] ?? null} />) : open("modal", <CredentialModal manifest={m} onDone={reload} />)}>
        <div className="lcard-top"><div className="lcard-ic logo" style={{ background: "var(--bg-tint)" }}><i className="ti ti-plug-connected" /></div><div style={{ flex: 1 }}><div className="lcard-title">{m.display_name}</div><div className="lcard-sub">{m.operations.length} action{m.operations.length === 1 ? "" : "s"}</div></div>{grant && <Badge tone={grant[0]}>{grant[1]}</Badge>}</div>
        <div className="lcard-desc" title={m.description}>{firstSentence(m.description ?? "", 140)}</div>
        {inst.length > 1 && <div className="lcard-tags"><span className="item-tag">{inst.length} connections</span></div>}
        <div className="lcard-foot">{first ? <><span className="mu"><UsedStack agents={agentsOf(first).map((x) => x.name)} /></span><span className="sp" /><button className="m-btn secondary sm" data-manage>Manage</button></> : <><span className="sp" /><button className="m-btn primary sm" data-connect>Connect</button></>}</div>
      </div>
    );
  };
  return (
    <div className="view library active" id="view-connectors">
      <div className="lib-top"><div className="crumbs"><span>Rusty</span><i className="ti ti-chevron-right" style={{ color: "var(--ink-300)", fontSize: 15 }} /><b>Connectors</b></div><div className="sp" /><div className="lib-search" style={{ maxWidth: 240 }}><i className="ti ti-search" /><input placeholder="Search…" data-libsearch value={q} onChange={(e) => setQ(e.target.value)} /></div></div>
      <div className="lib-page">
        <div className="lib-hero">
          <div className="lh-ic" style={{ background: "var(--cat-teal-bg)", color: "var(--cat-teal)" }}><i className="ti ti-plug-connected" /></div>
          <div className="lh-main"><div className="lib-eyebrow">Library</div><h1 className="lib-title">Connectors</h1><p className="lib-lead">The systems your agents can work in. Connect one once, and any agent you allow can use it.</p></div>
          <div className="lh-act"><button className="m-btn primary" data-new="connector" onClick={() => open("drawer", <AddConnectorDrawer onConnected={reload} />)}><i className="ti ti-plus" /> Browse all</button></div>
        </div>
        <div className="lib-stats">
          <Stat v={String(instances.length)} l="Connected" />
          <Stat v={String(manifests.length)} l="Available" />
          <Stat v={String(inUse.length)} l="In use" />
          <Stat v={String(attention.length)} l="Needs attention" />
        </div>
        {connected.length > 0 && <><div className="lib-cat"><span>Connected</span><span className="ln" /><span className="gc">{connected.length}</span></div><div className="lib-grid">{connected.map(card)}</div></>}
        {mcp.length > 0 && <><div className="lib-cat"><span>MCP servers</span><span className="ln" /><span className="gc">{mcp.length}</span></div><div className="lib-grid">{mcp.map((m) => (
          <div key={m.id} className="lcard" onClick={() => open("drawer", <McpServerDrawer s={m} onChanged={reload} />)}>
            <div className="lcard-top"><div className="lcard-ic logo" style={{ background: "var(--bg-tint)" }}><i className="ti ti-server-2" /></div><div style={{ flex: 1 }}><div className="lcard-title">{m.name}</div><div className="lcard-sub">{m.command} {m.args.join(" ")}</div></div><Badge tone={m.status.state === "mounted" ? "good" : m.status.state === "failed" ? "bad" : "warn"}>{m.status.state.replace("_", " ")}</Badge></div>
            <div className="lcard-desc">{m.status.server ?? (m.status.error ?? "Not mounted yet.")}</div>
            <div className="lcard-tags"><span className="item-tag">MCP</span><span className="item-tag">{m.status.tools.length} tools</span></div>
            <div className="lcard-foot"><span className="sp" /><button className="m-btn secondary sm">Manage</button></div>
          </div>
        ))}</div></>}
        <div className="lib-cat"><span>Stand-ins</span><span className="ln" /><span className="gc">{worlds.length}</span><button className="m-btn ghost sm" style={{ marginLeft: 8 }} disabled={instances.length === 0} onClick={() => open("modal", <NewWorldModal instances={instances} onMade={reload} />)}><i className="ti ti-plus" /> New stand-in</button></div>
        <div className="lib-grid">{worlds.map((w) => (
          <div key={w.world_id} className="lcard" onClick={() => open("drawer", <WorldDrawer w={w} onChanged={reload} />)}>
            <div className="lcard-top"><div className="lcard-ic" style={{ background: "var(--cat-teal-bg)", color: "var(--cat-teal)" }}><i className="ti ti-box" /></div><div style={{ flex: 1 }}><div className="lcard-title">{w.name}</div><div className="lcard-sub">for {w.connector} · {w.dialect}</div></div></div>
            <div className="lcard-desc">Answers {w.stands_for} from a seed of {Object.values(w.seed_records).reduce((n, x) => n + x, 0)} rows; reset {w.reset_count}×, {w.calls_since_reset} calls since.</div>
            <div className="lcard-foot"><span className="mu"><i className="ti ti-refresh" style={{ fontSize: 13 }} /> {w.last_reset_at ? ago(w.last_reset_at) : "never reset"}</span><span className="sp" /><button className="m-btn secondary sm">Manage</button></div>
          </div>
        ))}{worlds.length === 0 && <div className="m-hint" style={{ gridColumn: "1 / -1" }}>No stand-in yet. A stand-in answers a connection's calls from a seed so agents can be tested without touching the live system.</div>}</div>
        {available.length > 0 && <><div className="lib-cat"><span>Library</span><span className="ln" /><span className="gc">{available.length}</span></div><div className="lib-grid">{available.map(card)}</div></>}
        {manifests.length === 0 && <div className="thread-empty" style={{ padding: 40 }}><i className="ti ti-plug-connected" />No connector yet. Browse all to describe a system.</div>}
      </div>
    </div>
  );
}

/** One connection: its grant, where it points, the operations it derives, who uses it, and the way out. */
function ManageDrawer({ manifest, instance, agents, onChanged, newer, standIns = [] }: { manifest: ConnectorManifest; instance: ConnectorInstance; agents: { assistant_id: string; name: string }[]; onChanged: () => void; newer?: ConnectorManifest | null; standIns?: World[] }) {
  const { open, close, toast } = useOverlay();
  const navigate = useNavigate();
  const [busy, setBusy] = useState(false);
  const [check, setCheck] = useState<string | null>(null);
  async function doCheck(world?: string) { setCheck("checking"); try { const r = await checkConnectorInstance(instance.instance_id, world); setCheck(r.status === "succeeded" ? `ok${r.world ? ` · proved against ${r.world}` : ""}` : `failed · ${r.message ?? "no reason given"}`); } catch (err) { setCheck(`failed · ${err instanceof Error ? err.message : "no answer"}`); } }
  const grant = grantWord(instance);
  const a = instance.authorization;
  const expires = a && "expires_at" in a && a.expires_at ? ago(a.expires_at).replace(" ago", "") : null;
  async function reauthorize() {
    setBusy(true);
    try { const r = await authorizeConnection(instance.instance_id); window.open(r.url, "_blank", "noopener"); toast("Consent opened in a new tab; the grant lands when you come back", "ti-shield-lock"); }
    catch (err) { toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle"); }
    finally { setBusy(false); }
  }
  async function disconnect() {
    setBusy(true);
    try { await revokeConnection(instance.instance_id); toast(`${manifest.display_name} disconnected`, "ti-plug-connected-x"); onChanged(); close(); }
    catch (err) { toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle"); }
    finally { setBusy(false); }
  }
  return (
    <div className="m-drawer">
      <OvHead icon="ti-plug-connected" bg="var(--bg-tint)" fg="#fff" logo title={manifest.display_name} sub={`Connected ${ago(instance.created_at)}`} />
      <div className="ov-body">
        <div className="kv"><span className="k">Status</span><span className="v"><Badge tone={grant[0]}>{grant[1]}</Badge></span><span className="k">Connected</span><span className="v">{ago(instance.created_at)}</span><span className="k">Address</span><span className="v" style={{ fontSize: 12, color: "var(--ink-500)" }}>{manifest.base_url.replace(/^https?:\/\//, "")}</span><span className="k">Used by</span><span className="v"><UsedStack agents={agents.map((x) => x.name)} /></span></div>
        <div className="cat-label"><span>What agents can do with it</span><span className="ln" /></div>
        {(instance.tools ?? []).map((t) => { const op = manifest.operations.find((o) => t.endsWith(`.${o.name}`)); const risk = op ? riskOf(op.effect) : "read"; return <div key={t} className="scope-row"><div className="sb"><div title={t}>{plainName(t)}</div><div className="sd">{firstSentence(op?.description ?? "", 120)}</div></div><Badge tone={risk === "read" ? "good" : risk === "write" ? "warn" : "bad"}>{risk === "read" ? "Looks things up" : risk === "write" ? "Changes things · asks first" : "Can't be undone · asks first"}</Badge></div>; })}
        {(instance.tools ?? []).length === 0 && <div className="m-hint">Nothing to do with it yet.</div>}
        <div className="cat-label" style={{ marginTop: 22 }}><span>Health</span><span className="ln" /></div>
        <div className="kv"><span className="k">Grant</span><span className="v">{a ? a.kind.replace("_", " ") : "credential"}</span>{expires && <><span className="k">Expires</span><span className="v">in {expires}</span></>}{a && "refreshable" in a && <><span className="k">Refresh</span><span className="v">{a.refreshable ? "automatic" : "manual"}</span></>}</div>
        <div style={{ display: "flex", gap: 8, flexWrap: "wrap" }}>
          {standIns.map((w) => <button key={w.world_id} className="m-btn ghost sm" disabled={check === "checking"} onClick={() => void doCheck(w.name)}><i className="ti ti-box" /> Test against {w.name}</button>)}
          <button className="m-btn secondary sm" data-test disabled={check === "checking"} onClick={() => void doCheck()}>{check === "checking" ? <><span className="m-spin" style={{ width: 13, height: 13, borderWidth: 2 }} /> Testing…</> : check ? <><i className={`ti ${check.startsWith("ok") ? "ti-circle-check" : "ti-alert-circle"}`} style={{ color: check.startsWith("ok") ? "var(--good)" : "var(--bad)" }} /> {check}</> : <><i className="ti ti-activity" /> Test connection</>}</button>
          {a && (a.kind === "needs_auth" || a.kind === "expired") && <button className="m-btn secondary sm" disabled={busy} onClick={() => void reauthorize()}><i className="ti ti-shield-lock" /> Reauthorize</button>}
          <button className="m-btn ghost sm" onClick={() => open("modal", <RotateModal manifest={manifest} instance={instance} onDone={onChanged} />)}><i className="ti ti-key" /> Rotate credential</button>
          {newer && <button className="m-btn ghost sm" disabled={busy} onClick={async () => { setBusy(true); try { await upgradeConnection(instance.instance_id, newer.hash); toast(`Upgraded to ${manifest.display_name} v${newer.version}`, "ti-arrow-up-circle"); onChanged(); close(); } catch (err) { toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle"); } finally { setBusy(false); } }}><i className="ti ti-arrow-up-circle" /> Upgrade to v{newer.version}</button>}
        </div>
        {agents.length > 0 && <div style={{ marginTop: 14 }}>{agents.map((x) => <a key={x.assistant_id} href="#" className="item-tag" style={{ marginRight: 6, textDecoration: "none" }} onClick={(e) => { e.preventDefault(); close(); navigate({ to: "/agents/$id", params: { id: x.assistant_id } }); }}>{x.name}</a>)}</div>}
        <div className="danger-zone"><div className="dz"><b>Disconnect {manifest.display_name}</b><span>Agents using it fail on their next call; the operations leave their tool lists.</span></div><button className="m-btn danger sm" data-disc disabled={busy} onClick={() => void disconnect()}>Disconnect</button></div>
      </div>
      <div className="ov-foot"><div className="sp" /><button className="m-btn secondary" data-close>Done</button></div>
    </div>
  );
}

function Stat({ v, u, l }: { v: string; u?: string; l: string }) {
  return <div className="lib-stat"><div className="ls-v">{v}{u && <span className="u">{u}</span>}</div><div className="ls-l">{l}</div></div>;
}

/** A new credential on the same connection: the manifest's fields again; the instance id, and every agent's tool list, stay. */
function RotateModal({ manifest, instance, onDone }: { manifest: ConnectorManifest; instance: ConnectorInstance; onDone: () => void }) {
  const { close, toast } = useOverlay();
  const spec = (manifest.connection_specification?.properties ?? {}) as Record<string, { description?: string; title?: string; airbyte_secret?: boolean }>;
  const [values, setValues] = useState<Record<string, string>>(Object.fromEntries(Object.entries(instance.config).filter(([, v]) => typeof v === "string").map(([k, v]) => [k, v as string])));
  const [busy, setBusy] = useState(false);
  return (
    <div className="m-modal">
      <OvHead icon="ti-key" title={`Rotate · ${manifest.display_name}`} sub="Sealed values are never shown; leave a field empty to keep what is held." />
      <div className="ov-body"><div className="frow">
        {Object.entries(spec).map(([k, f]) => { const secret = f.airbyte_secret || /password|secret|token|key/i.test(k); const held = instance.config[k] && typeof instance.config[k] === "object"; return <div key={k} className="m-field"><label className="m-label">{f.title ?? k}{held ? <span className="opt"> — held</span> : ""}</label><input className="m-input" type={secret ? "password" : "text"} placeholder={held ? "••••••••" : f.description ?? ""} value={values[k] ?? ""} onChange={(e) => setValues((v) => ({ ...v, [k]: e.target.value }))} style={{ fontFamily: "var(--font-mono)", fontSize: 12 }} /></div>; })}
      </div></div>
      <div className="ov-foot"><div className="sp" /><button className="m-btn ghost" data-close>Cancel</button><button className="m-btn primary" disabled={busy} onClick={async () => { setBusy(true); try { const cfg = { ...Object.fromEntries(Object.entries(instance.config)), ...Object.fromEntries(Object.entries(values).filter(([, v]) => v !== "")) }; await rotateConnection(instance.instance_id, cfg); toast("Credential rotated", "ti-key"); onDone(); close(); } catch (err) { toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle"); } finally { setBusy(false); } }}><i className="ti ti-check" /> Rotate</button></div>
    </div>
  );
}
