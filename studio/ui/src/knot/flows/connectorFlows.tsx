import { useEffect, useState } from "react";
import { createMcpServer, createWorld, deleteMcpServer, deleteWorld, allowPluginHosts, installPlugin, loadPluginKnowledge, listPluginLibrary, listPlugins, probeMcpServer, remountMcpServer, resetWorld, uninstallPlugin, worldStarter, type ConnectorInstance, type McpProbe, type McpServer, type PluginOffer, type PluginRecord, type ToolEffect, type World } from "../../engine/net/client";
import { Badge, OvHead, useOverlay } from "../overlay";
import { ago } from "../data";

const EFFECTS: ToolEffect[] = ["read_only", "idempotent", "compensatable", "non_idempotent", "pure"];
const effectWord: Record<ToolEffect, string> = { read_only: "read", idempotent: "write · idempotent", compensatable: "write · compensatable", non_idempotent: "write · gated", pure: "pure" };

/** Mount an MCP server: launch it, see what it offers, name each tool's effect class, mount. */
export function McpMountDrawer({ onMounted }: { onMounted: () => void }) {
  const { close, toast } = useOverlay();
  const [name, setName] = useState("");
  const [command, setCommand] = useState("");
  const [args, setArgs] = useState("");
  const [env, setEnv] = useState("");
  const [probe, setProbe] = useState<McpProbe | null>(null);
  const [effects, setEffects] = useState<Record<string, ToolEffect>>({});
  const [busy, setBusy] = useState<"" | "probing" | "mounting">("");
  const launch = () => ({ command: command.trim(), args: args.trim() ? args.trim().split(/\s+/) : [], env: env.split("\n").map((l) => l.trim()).filter(Boolean).map((l) => { const secret = l.startsWith("!"); const [n, ...v] = l.replace(/^!/, "").split("="); return { name: n.trim(), value: v.join("=").trim(), secret }; }) });
  async function doProbe() {
    setBusy("probing");
    try { const p = await probeMcpServer(launch()); setProbe(p); setEffects(Object.fromEntries(p.tools.map((t) => [t.name, t.suggested_effect]))); if (!name) setName(p.server.name); }
    catch (err) { toast(err instanceof Error ? err.message : "the server could not be launched", "ti-alert-triangle"); }
    finally { setBusy(""); }
  }
  async function mount() {
    setBusy("mounting");
    try { const s = await createMcpServer({ ...launch(), name: name.trim(), tool_effects: effects }); toast(`${s.name} mounted · ${s.status.tools.length} tools`, "ti-server-2"); onMounted(); close(); }
    catch (err) { toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle"); }
    finally { setBusy(""); }
  }
  return (
    <div className="m-drawer" style={{ width: "min(600px,100%)" }}>
      <OvHead icon="ti-server-2" bg="var(--bg-tint)" fg="#fff" logo title="Mount an MCP server" sub="The server launches it, reads what it offers, and each tool joins the catalog with the effect class you name." />
      <div className="ov-body">
        <div className="frow two"><div className="m-field"><label className="m-label">Name</label><input className="m-input" value={name} onChange={(e) => setName(e.target.value)} placeholder="filesystem" /></div><div className="m-field"><label className="m-label">Command</label><input className="m-input" value={command} onChange={(e) => setCommand(e.target.value)} placeholder="npx" style={{ fontFamily: "var(--font-mono)" }} /></div></div>
        <div className="m-field" style={{ marginTop: 12 }}><label className="m-label">Arguments</label><input className="m-input" value={args} onChange={(e) => setArgs(e.target.value)} placeholder="-y @modelcontextprotocol/server-filesystem /srv/docs" style={{ fontFamily: "var(--font-mono)" }} /></div>
        <div className="m-field" style={{ marginTop: 12 }}><label className="m-label">Environment <span className="opt">— one NAME=value per line; prefix ! to seal it</span></label><textarea className="m-textarea" rows={3} value={env} onChange={(e) => setEnv(e.target.value)} placeholder={"!API_TOKEN=…\nLOG_LEVEL=info"} style={{ fontFamily: "var(--font-mono)", fontSize: 12 }} /></div>
        {probe && (
          <>
            <div className="disc-sum" style={{ marginTop: 14 }}><i className="ti ti-check" style={{ color: "var(--good-dot)" }} /> <b>{probe.server.name}</b> {probe.server.version} · {probe.server.protocol} · {probe.tools.length} tools. Name each tool's effect; the gate reads it.</div>
            <div className="ops-list" style={{ marginTop: 10 }}>
              {probe.tools.map((t) => <div key={t.name} className="op-row" style={{ cursor: "default" }}><div className="op-body"><div className="op-name">{t.name}</div><div className="op-desc">{t.description}</div></div><select className="m-select xs" value={effects[t.name]} onChange={(e) => setEffects((x) => ({ ...x, [t.name]: e.target.value as ToolEffect }))}>{EFFECTS.map((e) => <option key={e} value={e}>{effectWord[e]}</option>)}</select></div>)}
            </div>
          </>
        )}
      </div>
      <div className="ov-foot"><button className="m-btn ghost sm" disabled={!command.trim() || busy !== ""} onClick={() => void doProbe()}>{busy === "probing" ? <><span className="m-spin" style={{ width: 13, height: 13, borderWidth: 2 }} /> Launching…</> : <><i className="ti ti-radar-2" /> Probe</>}</button><div className="sp" /><button className="m-btn ghost" data-close>Cancel</button><button className="m-btn primary" disabled={!probe || !name.trim() || busy !== ""} onClick={() => void mount()}><i className="ti ti-plug-connected" /> Mount</button></div>
    </div>
  );
}

export function McpServerDrawer({ s, onChanged }: { s: McpServer; onChanged: () => void }) {
  const { close, toast } = useOverlay();
  const [busy, setBusy] = useState(false);
  const tone = s.status.state === "mounted" ? "good" : s.status.state === "failed" ? "bad" : "warn";
  return (
    <div className="m-drawer">
      <OvHead icon="ti-server-2" bg="var(--bg-tint)" fg="#fff" logo title={s.name} sub={`MCP · ${s.command} ${s.args.join(" ")}`} />
      <div className="ov-body">
        <div className="kv"><span className="k">Status</span><span className="v"><Badge tone={tone}>{s.status.state.replace("_", " ")}</Badge></span><span className="k">Server</span><span className="v mono">{s.status.server ?? "—"}</span><span className="k">Mounted</span><span className="v">{ago(s.created_at)}</span>{s.status.error && <><span className="k">Error</span><span className="v" style={{ color: "var(--bad)" }}>{s.status.error}</span></>}</div>
        <div className="cat-label"><span>Tools</span><span className="ln" /><span className="gc">{s.status.tools.length}</span></div>
        {s.status.tools.map((t) => <div key={t.tool} className="scope-row"><div className="sb"><div className="mono">{t.tool}</div><div className="sd">{t.description}</div></div><Badge tone={t.effect === "read_only" || t.effect === "pure" ? "good" : t.effect === "non_idempotent" ? "bad" : "warn"}>{effectWord[t.effect]}</Badge></div>)}
        {s.status.left_out.length > 0 && <div className="m-hint">Left out (no effect named): {s.status.left_out.join(", ")}</div>}
        <div className="danger-zone"><div className="dz"><b>Unmount {s.name}</b><span>Its tools leave the catalog; agents naming them fail on their next call.</span></div><button className="m-btn danger sm" disabled={busy} onClick={async () => { setBusy(true); try { await deleteMcpServer(s.id); toast(`${s.name} unmounted`); onChanged(); close(); } catch (err) { toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle"); } finally { setBusy(false); } }}>Unmount</button></div>
      </div>
      <div className="ov-foot"><button className="m-btn ghost sm" disabled={busy} onClick={async () => { setBusy(true); try { const r = await remountMcpServer(s.id); toast(`${r.name}: ${r.status.state.replace("_", " ")}`, "ti-refresh"); onChanged(); close(); } catch (err) { toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle"); } finally { setBusy(false); } }}><i className="ti ti-refresh" /> Remount</button><div className="sp" /><button className="m-btn secondary" data-close>Done</button></div>
    </div>
  );
}

/** A stand-in world for a connection: seeded from the dialect's starter, reset to the seed on demand. */
export function WorldDrawer({ w, onChanged }: { w: World; onChanged: () => void }) {
  const { close, toast } = useOverlay();
  const [busy, setBusy] = useState(false);
  const tables = Object.entries(w.records);
  return (
    <div className="m-drawer">
      <OvHead icon="ti-box" bg="var(--cat-teal-bg)" fg="var(--cat-teal)" title={w.name} sub={`Stand-in for ${w.stands_for} · ${w.dialect}`} />
      <div className="ov-body">
        <div className="kv"><span className="k">Connector</span><span className="v">{w.connector}</span><span className="k">Made</span><span className="v">{ago(w.created_at)} by {w.created_by?.name ?? "—"}</span><span className="k">Calls since reset</span><span className="v">{w.calls_since_reset}</span><span className="k">Resets</span><span className="v">{w.reset_count}{w.last_reset_at ? ` · last ${ago(w.last_reset_at)}` : ""}</span>{w.faults_left != null && <><span className="k">Faults left</span><span className="v">{w.faults_left} of {w.seed_faults ?? 0}</span></>}</div>
        <div className="cat-label"><span>Records</span><span className="ln" /></div>
        {tables.map(([t, n]) => <div key={t} className="scope-row"><div className="sb"><div className="mono">{t}</div><div className="sd">{n} now · {w.seed_records[t] ?? 0} in the seed</div></div></div>)}
        <div className="danger-zone"><div className="dz"><b>Delete {w.name}</b><span>Runs that named it show it as gone; suites that reset it lose their stand-in.</span></div><button className="m-btn danger sm" disabled={busy} onClick={async () => { setBusy(true); try { await deleteWorld(w.world_id); toast(`${w.name} deleted`); onChanged(); close(); } catch (err) { toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle"); } finally { setBusy(false); } }}>Delete</button></div>
      </div>
      <div className="ov-foot"><button className="m-btn ghost sm" disabled={busy} onClick={async () => { setBusy(true); try { const r = await resetWorld(w.world_id); toast(`${r.name} back to its seed`, "ti-refresh"); onChanged(); close(); } catch (err) { toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle"); } finally { setBusy(false); } }}><i className="ti ti-refresh" /> Reset to seed</button><div className="sp" /><button className="m-btn secondary" data-close>Done</button></div>
    </div>
  );
}

export function NewWorldModal({ instances, onMade }: { instances: ConnectorInstance[]; onMade: () => void }) {
  const { close, toast } = useOverlay();
  const [instance, setInstance] = useState(instances[0]?.instance_id ?? "");
  const [name, setName] = useState("");
  const [starter, setStarter] = useState<{ connector: string; dialect: string; starter: unknown } | null>(null);
  const [busy, setBusy] = useState(false);
  useEffect(() => { if (!instance) return; worldStarter({ instance_id: instance }).then((s) => { setStarter(s); if (!name) setName(`${s.connector}-twin`); }).catch(() => setStarter(null)); }, [instance]); // eslint-disable-line react-hooks/exhaustive-deps
  return (
    <div className="m-modal lg">
      <OvHead icon="ti-box" bg="var(--cat-teal-bg)" fg="var(--cat-teal)" title="New stand-in" sub="A world answers a connection's calls in the system's own dialect, from a seed, and goes back to the seed on reset." />
      <div className="ov-body">
        <div className="frow two"><div className="fld"><label className="fld-label">Stands in for</label><select className="m-input" value={instance} onChange={(e) => setInstance(e.target.value)}>{instances.map((i) => <option key={i.instance_id} value={i.instance_id}>{i.connector?.display_name ?? i.instance_id}</option>)}</select></div><div className="fld"><label className="fld-label">Name</label><input className="m-input" value={name} onChange={(e) => setName(e.target.value)} style={{ fontFamily: "var(--font-mono)" }} /></div></div>
        {starter ? <div className="m-hint">Dialect <b>{starter.dialect}</b>; the seed starts from the dialect's own starter ({typeof starter.starter === "object" && starter.starter ? Object.keys(starter.starter as object).length : 0} tables).</div> : instance ? <div className="m-hint">No dialect speaks for this connector yet — the stand-in cannot be made.</div> : null}
      </div>
      <div className="ov-foot"><div className="sp" /><button className="m-btn ghost" data-close>Cancel</button><button className="m-btn primary" disabled={busy || !starter || !name.trim()} onClick={async () => { setBusy(true); try { const w = await createWorld({ name: name.trim(), instance_id: instance, dialect: starter!.dialect, seed: starter!.starter }); toast(`${w.name} made`, "ti-box"); onMade(); close(); } catch (err) { toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle"); } finally { setBusy(false); } }}><i className="ti ti-check" /> Make</button></div>
    </div>
  );
}

/** Packages: plugins from the library — connectors and skills installed whole. */
export function PackagesTab({ onChanged }: { onChanged: () => void }) {
  const { toast } = useOverlay();
  const [offers, setOffers] = useState<PluginOffer[]>([]);
  const [installed, setInstalled] = useState<PluginRecord[]>([]);
  const [url, setUrl] = useState("");
  const [busy, setBusy] = useState<string | null>(null);
  const load = () => { listPluginLibrary().then(setOffers).catch(() => {}); listPlugins().then(setInstalled).catch(() => {}); };
  useEffect(load, []);
  const say = (err: unknown) => toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle");
  async function install(src: { library: string } | { url: string }) { const key = "library" in src ? src.library : src.url; setBusy(key); try { const r = await installPlugin(src); toast(`${r.name} installed · ${r.connectors.length} connectors · ${r.skills.length} skills`, "ti-package"); load(); onChanged(); } catch (err) { say(err); } finally { setBusy(null); } }
  return (
    <div className="ov-body" style={{ paddingTop: 14 }}>
      <div className="cat-label"><span>Library</span><span className="ln" /></div>
      {offers.map((o) => { const rec = installed.find((i) => i.id === o.id); return (
        <div key={o.id} className="item">
          <div className="item-ic logo" style={{ background: "var(--bg-tint)", color: "#fff" }}><i className="ti ti-package" /></div>
          <div className="item-body"><div className="item-name">{o.name} <span className="item-tag">v{o.version}</span></div><div className="item-desc">{o.description}</div><div className="item-meta"><span className="item-tag">{o.connectors} connector{o.connectors === 1 ? "" : "s"}</span><span className="item-tag">{o.skills} skill{o.skills === 1 ? "" : "s"}</span><span className="item-tag">{o.publisher}</span></div>{rec && (rec.hosts?.length ?? 0) > 0 && <div className="m-hint" data-plugin-hosts style={{ marginTop: 6 }}><i className="ti ti-world" /> Its skills read {rec.hosts!.length} vendor host{rec.hosts!.length === 1 ? "" : "s"}: {rec.hosts!.join(", ")}{rec.ceiling_open ? " — the ceiling is open, all reachable" : (rec.hosts_outside?.length ?? 0) > 0 ? <> — <b>{rec.hosts_outside!.length} not yet allowed</b> <button className="m-btn secondary sm" data-allow-hosts disabled={busy === `hosts:${rec.id}`} onClick={async () => { setBusy(`hosts:${rec.id}`); try { const r = await allowPluginHosts(rec.id); toast(r.allowed.length ? `${r.allowed.length} host${r.allowed.length === 1 ? "" : "s"} allowed for ${o.name}` : r.note ?? "Nothing to allow", "ti-world"); load(); } catch (err) { say(err); } finally { setBusy(null); } }}>Allow them</button></> : " — all allowed"}</div>}{rec && ((rec.knowledge_shipped ?? 0) > 0 || (rec.knowledge?.length ?? 0) > 0) && <div className="m-hint" data-plugin-knowledge style={{ marginTop: 4 }}><i className="ti ti-book" /> Ships {rec.knowledge_shipped ?? rec.knowledge!.length} knowledge source{(rec.knowledge_shipped ?? rec.knowledge!.length) === 1 ? "" : "s"}{(rec.knowledge?.length ?? 0) > 0 ? <> — loaded: {rec.knowledge!.map((k) => k.title).join(", ")}</> : <> — <b>not loaded</b> <button className="m-btn secondary sm" data-load-knowledge disabled={busy === `knowledge:${rec.id}`} onClick={async () => { setBusy(`knowledge:${rec.id}`); try { const r = await loadPluginKnowledge(rec.id); toast(r.loaded.length ? `${r.loaded.length} source${r.loaded.length === 1 ? "" : "s"} loaded for ${o.name}` : r.note ?? "Nothing to load", "ti-book"); load(); } catch (err) { say(err); } finally { setBusy(null); } }}>Load them</button></>}</div>}</div>
          {rec ? <button className="m-btn ghost sm" disabled={busy === o.id} onClick={async () => { setBusy(o.id); try { await uninstallPlugin(rec.id); toast(`${o.name} removed`); load(); onChanged(); } catch (err) { say(err); } finally { setBusy(null); } }}>Remove</button> : <button className="m-btn secondary sm" disabled={busy === o.id} onClick={() => void install({ library: o.id })}>Install</button>}
        </div>
      ); })}
      {offers.length === 0 && <div className="m-hint">The library holds no packages.</div>}
      <div className="cat-label" style={{ marginTop: 18 }}><span>From a repository</span><span className="ln" /></div>
      <div style={{ display: "flex", gap: 8 }}><input className="m-input" placeholder="https://github.com/org/plugin" value={url} onChange={(e) => setUrl(e.target.value)} style={{ fontFamily: "var(--font-mono)", fontSize: 12 }} /><button className="m-btn secondary sm" disabled={!url.trim() || !!busy} onClick={() => void install({ url: url.trim() })}><i className="ti ti-download" /> Install</button></div>
    </div>
  );
}
