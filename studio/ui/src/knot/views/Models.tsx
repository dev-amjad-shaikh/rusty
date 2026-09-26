import { useEffect, useMemo, useState } from "react";
import { apiBase, llmProviders, saveLlmProviders, testLlmProvider, type LlmProvider, type LlmProviderInput, type LlmProviders } from "../../engine/net/client";
import { useServer } from "../../engine/net/server";
import { Badge, OvHead, useOverlay } from "../overlay";
import { compact } from "../data";
import { shortModel } from "../agents/words";

/** The provider's logo mark, by where it lives. */
const logoOf = (p: LlmProvider) => (/fireworks/i.test(p.base_url + p.name) ? "ti-flame" : /anthropic/i.test(p.base_url + p.name) ? "ti-sparkles" : /openai/i.test(p.base_url) ? "ti-circle-dashed" : /googleapis/i.test(p.base_url) ? "ti-circle" : /^https?:\/\/(10\.|100\.|192\.168|127\.|localhost)/.test(p.base_url) ? "ti-server" : "ti-cpu");
const price = (p: LlmProvider) => (p.price_input_per_m != null || p.price_output_per_m != null ? `$${p.price_input_per_m ?? "—"} / $${p.price_output_per_m ?? "—"}` : "—");
const asInput = (p: LlmProvider): LlmProviderInput => ({ id: p.id, name: p.name, base_url: p.base_url, model: p.model, extra_body: p.extra_body ?? null, price_input_per_m: p.price_input_per_m ?? null, price_output_per_m: p.price_output_per_m ?? null, price_cached_input_per_m: p.price_cached_input_per_m ?? null });

/** LLM Gateway: every provider the deployment routes to, the chain, the models. */
export function ModelsView() {
  const { open, toast } = useOverlay();
  const runs = useServer((s) => s.runs);
  const [cfg, setCfg] = useState<LlmProviders | null>(null);
  const [problem, setProblem] = useState<string | null>(null);
  // The list first, fast; the cache figures follow from the journal scan.
  const reload = () => llmProviders().then((c) => { setCfg(c); setProblem(null); return llmProviders(true).then((full) => setCfg(full)).catch(() => {}); }).catch((e) => setProblem(e instanceof Error ? e.message : "the providers could not be read"));
  useEffect(() => { void reload(); }, []);
  const providers = cfg?.providers ?? [];
  const cache = cfg?.cache;
  const callsOf = (p: LlmProvider) => cache?.models[p.model]?.calls ?? 0;
  const totalCalls = Object.values(cache?.models ?? {}).reduce((n, m) => n + m.calls, 0);
  const promptTokens = Object.values(cache?.models ?? {}).reduce((n, m) => n + m.prompt_tokens, 0);
  const cachedTokens = Object.values(cache?.models ?? {}).reduce((n, m) => n + m.cached_tokens, 0);
  const hitRate = promptTokens ? Math.round((cachedTokens / promptTokens) * 100) : 0;

  // Runs per day over the last fourteen days, from the newest runs the server lists.
  const days = useMemo(() => {
    const out: { key: string; label: string; runs: number; failed: number }[] = [];
    const today = new Date(); today.setHours(0, 0, 0, 0);
    for (let i = 13; i >= 0; i--) { const d = new Date(today.getTime() - i * 86_400_000); out.push({ key: d.toISOString().slice(0, 10), label: d.toLocaleDateString(undefined, { month: "short", day: "numeric" }), runs: 0, failed: 0 }); }
    for (const r of runs) { const k = r.created_at.slice(0, 10); const d = out.find((x) => x.key === k); if (d) { d.runs++; if (r.status === "error" || r.status === "failed") d.failed++; } }
    return out;
  }, [runs]);
  const axis = <div className="spark-x">{[0, 3, 6, 9, 12].map((i) => <span key={i}>{days[i]?.label}</span>)}</div>;
  const bars = (vals: number[], err?: boolean[]) => { const max = Math.max(1, ...vals); return <div className="spark">{vals.map((v, i) => <div key={i} style={{ height: `${Math.round((v / max) * 100)}%` }} title={String(v)} className={err?.[i] ? "err" : undefined} />)}</div>; };

  const primary = providers.find((p) => p.id === cfg?.primary);
  const fallback = providers.find((p) => p.id === cfg?.fallback);
  const active = providers.filter((p) => p.role);

  async function save(next: LlmProviderInput[], primaryId: string | null, fallbackId: string | null, said: string) {
    try { const c = await saveLlmProviders({ providers: next, primary: primaryId, fallback: fallbackId }); setCfg(c); toast(c.applied_error ? c.applied_error : said, c.applied_error ? "ti-alert-triangle" : "ti-cpu"); }
    catch (e) { toast(e instanceof Error ? e.message : "the server refused", "ti-alert-triangle"); }
  }
  const openProvider = (p?: LlmProvider) => open("drawer", <ProviderDrawer provider={p} all={providers} cfg={cfg!} onSave={save} />);

  return (
    <div className="view library active" id="view-models">
      <div className="lib-top"><div className="crumbs"><span>Rustynome</span><i className="ti ti-chevron-right" style={{ color: "var(--ink-300)", fontSize: 15 }} /><b>AI models</b></div><div className="sp" /></div>
      <div className="lib-page">
        <div className="lib-hero">
          <div className="lh-ic"><i className="ti ti-cpu" /></div>
          <div className="lh-main"><div className="lib-eyebrow">Platform</div><h1 className="lib-title">AI models</h1><p className="lib-lead">The AI models your agents think with: which one answers first, which steps in if it does not, and what they cost.</p></div>
          <div className="lh-act"><button className="m-btn secondary" onClick={() => open("modal", <EndpointModal />)}><i className="ti ti-link" /> Endpoint &amp; keys</button> <button className="m-btn primary" disabled={!cfg} onClick={() => openProvider()}><i className="ti ti-plus" /> Add provider</button></div>
        </div>
        {problem && <div className="m-alert"><i className="ti ti-alert-triangle" style={{ color: "var(--bad)", fontSize: 16 }} /><div className="a-body"><div className="a-text">{problem}</div></div></div>}
        {!cfg && !problem && <div className="thread-empty" style={{ padding: 30 }}><span className="m-spin" style={{ width: 16, height: 16, borderWidth: 2 }} /> Reading the providers…</div>}
        <div className="lib-stats" hidden={!cfg}>
          <Stat v={cache ? compact(totalCalls) : "…"} l={cache ? `Model calls in the last ${cache.runs} runs` : "Model calls · counting…"} />
          <Stat v={compact(promptTokens)} l="Tokens sent" />
          <Stat v={String(hitRate)} u="%" l="Reused from cache" />
          <Stat v={String(providers.length)} l={`Providers · ${active.length} in use`} />
        </div>
        <div className="two-col" style={{ marginBottom: 26 }}>
          <div className="chart-card"><div className="ch"><span className="t">Runs per day</span><span className="v">{compact(days.reduce((n, d) => n + d.runs, 0))}</span></div>{bars(days.map((d) => d.runs))}{axis}</div>
          <div className="chart-card"><div className="ch"><span className="t">Runs that failed</span><span className="v">{compact(days.reduce((n, d) => n + d.failed, 0))}</span></div>{bars(days.map((d) => d.failed), days.map((d) => d.failed > 0))}{axis}</div>
        </div>

        <div className="lib-cat"><span>Providers</span><span className="ln" /><span className="gc">{active.length} active</span></div>
        <div className="prov-grid">
          {providers.map((p) => (
            <div key={p.id} className="prov" data-prov={p.id} onClick={() => openProvider(p)}>
              <div className="pt"><div className="pl"><i className={`ti ${logoOf(p)}`} /></div><div className="pn">{p.name}</div>{p.role ? <Badge tone="good">{p.role === "primary" ? "Primary" : "Fallback"}</Badge> : <button className="m-btn secondary sm" onClick={(e) => { e.stopPropagation(); openProvider(p); }}>Route</button>}</div>
              <div className="pk"><span className="kdot" style={p.has_key ? undefined : { background: "var(--ink-300)" }} />{p.has_key ? "key saved" : "no key"} · <span title={p.model}>{shortModel(p.model)}</span></div>
              <div className="pm"><div>Calls<b>{compact(callsOf(p))}</b></div><div>Price in / out<b>{price(p)}</b></div></div>
            </div>
          ))}
          {cfg && providers.length === 0 && <div className="thread-empty" style={{ gridColumn: "1 / -1", padding: 30 }}><i className="ti ti-cpu" />No provider yet. Add one — every agent routes through it.</div>}
        </div>

        <div className="lib-cat"><span>Which model answers</span><span className="ln" /></div>
        <div className="route-card">
          <div className="route">
            <div className="rn">Default</div>
            <div className="rchain">{primary ? <span className="m-chip" title={primary.model}>{primary.name}</span> : <span className="m-chip">none chosen</span>}{fallback && <><i className="ti ti-arrow-right" /><span className="m-chip" title={fallback.model}>{fallback.name}</span></>}</div>
            <div className="rstat">{primary ? (fallback ? `${primary.name} answers first; ${fallback.name} steps in if it fails` : `${primary.name} answers; if it fails, the run fails — add a second model to step in`) : "Choose a model to answer first"}</div>
            <button className="m-btn icon ghost sm" disabled={!cfg} onClick={() => open("modal", <RouteModal cfg={cfg!} onSave={save} />)}><i className="ti ti-pencil" /></button>
          </div>
        </div>

        <div className="lib-cat"><span>Model catalog</span><span className="ln" /><span className="gc">{providers.length} models</span></div>
        <div className="lib-table-wrap"><table className="m-table">
          <thead><tr><th>Model</th><th>Provider</th><th>Price per million tokens (in / out)</th><th>Calls</th><th>Reused from cache</th><th>Key</th><th className="sw-cell">Role</th></tr></thead>
          <tbody>
            {providers.map((p) => { const m = cache?.models[p.model]; const hit = m?.cache_hit_rate; return (
              <tr key={p.id} className={`clickable${p.role ? "" : " off"}`} onClick={() => openProvider(p)}>
                <td><div className="tl-row-name"><div className="tl-ic"><i className={`ti ${logoOf(p)}`} /></div><span style={{ fontWeight: 600 }} title={p.model}>{shortModel(p.model)}</span></div></td>
                <td style={{ color: "var(--ink-600)" }}>{p.name}</td>
                <td style={{ fontFamily: "var(--font-mono)", fontSize: "var(--fs-xs)", color: "var(--ink-600)" }}>{price(p)}</td>
                <td><span className="lat"><span style={{ width: `${totalCalls ? Math.round((callsOf(p) / totalCalls) * 100) : 0}%` }} /></span><span style={{ fontFamily: "var(--font-mono)", fontSize: "var(--fs-xs)" }}>{callsOf(p)}</span></td>
                <td>{hit != null ? <Badge tone={hit >= 0.5 ? "good" : "warn"}>{Math.round(hit * 100)}%</Badge> : <span style={{ color: "var(--ink-400)" }}>—</span>}</td>
                <td>{p.has_key ? <Badge tone="good">saved</Badge> : <span style={{ color: "var(--ink-400)" }}>not needed</span>}</td>
                <td className="sw-cell">{p.role ? <Badge tone="accent">{p.role === "primary" ? "answers first" : "steps in"}</Badge> : <span style={{ color: "var(--ink-400)" }}>—</span>}</td>
              </tr>
            ); })}
          </tbody>
        </table></div>
      </div>
    </div>
  );
}

function Stat({ v, u, l }: { v: string; u?: string; l: string }) {
  return <div className="lib-stat"><div className="ls-v">{v}{u && <span className="u">{u}</span>}</div><div className="ls-l">{l}</div></div>;
}

/** One provider: its key (write-only), where it lives, the model, the prices. */
function ProviderDrawer({ provider, all, cfg, onSave }: { provider?: LlmProvider; all: LlmProvider[]; cfg: LlmProviders; onSave: (next: LlmProviderInput[], primary: string | null, fallback: string | null, said: string) => Promise<void> }) {
  const { close } = useOverlay();
  const [f, setF] = useState<LlmProviderInput>(provider ? asInput(provider) : { id: "", name: "", base_url: "", model: "", api_key: "" });
  const [key, setKey] = useState("");
  const [test, setTest] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const isNew = !provider;
  const id = isNew ? (f.id || f.name.toLowerCase().trim().replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "")) : provider.id;
  const set = (k: keyof LlmProviderInput) => (e: React.ChangeEvent<HTMLInputElement>) => setF((x) => ({ ...x, [k]: e.target.type === "number" ? (e.target.value === "" ? null : Number(e.target.value)) : e.target.value }));
  const body = (): LlmProviderInput => ({ ...f, id, ...(key.trim() ? { api_key: key.trim() } : {}) });
  async function save() {
    setBusy(true);
    const rest = all.filter((p) => p.id !== id).map(asInput);
    const next = [...rest, body()];
    const primary = cfg.primary ?? (next.length === 1 ? id : null);
    await onSave(next, primary, cfg.fallback, isNew ? `${f.name} connected` : `${f.name} saved`);
    setBusy(false); close();
  }
  async function remove() {
    setBusy(true);
    const next = all.filter((p) => p.id !== id).map(asInput);
    await onSave(next, cfg.primary === id ? (next[0]?.id ?? null) : cfg.primary, cfg.fallback === id ? null : cfg.fallback, `${provider!.name} removed`);
    setBusy(false); close();
  }
  async function probe() {
    // A new key is saved first so the server tests what it will run with.
    setTest("testing");
    try {
      if (isNew || key.trim()) { await onSave([...all.filter((p) => p.id !== id).map(asInput), body()], cfg.primary ?? id, cfg.fallback, "saved"); setKey(""); }
      const r = await testLlmProvider(id);
      setTest(r.ok ? `ok · ${r.model ?? f.model} · ${r.latency_ms} ms` : `failed · ${r.error ?? "no answer"}`);
    } catch (e) { setTest(`failed · ${e instanceof Error ? e.message : "no answer"}`); }
  }
  return (
    <div className="m-drawer">
      <OvHead icon={provider ? logoOf(provider) : "ti-cpu"} title={provider?.name ?? "Add provider"} sub={provider ? (provider.role ? `${provider.role === "primary" ? "Answers first" : "Steps in if the first fails"} · ${shortModel(provider.model)}` : "Not in use") : "Any OpenAI-compatible endpoint: a vendor, or a box you run."} />
      <div className="ov-body">
        {isNew && <div className="frow two"><div className="fld"><label className="fld-label">Name</label><input className="m-input" value={f.name} onChange={set("name")} placeholder="Fireworks" /></div><div className="fld"><label className="fld-label">Id</label><input className="m-input" value={id} onChange={set("id")} style={{ fontFamily: "var(--font-mono)" }} /></div></div>}
        <div className="fld"><label className="fld-label">API key{provider?.has_key ? <span className="opt"> — held; paste a new one to rotate</span> : ""}</label><div className="m-input-group"><i className="ti ti-key" /><input type="password" value={key} onChange={(e) => setKey(e.target.value)} placeholder={provider?.has_key ? "••••••••" : "Paste a key from the provider console"} autoComplete="off" /></div></div>
        <div className="fld"><label className="fld-label">Base URL</label><input className="m-input" value={f.base_url} onChange={set("base_url")} placeholder="https://api.fireworks.ai/inference/v1" style={{ fontFamily: "var(--font-mono)" }} /></div>
        <div className="fld"><label className="fld-label">Model</label><input className="m-input" value={f.model} onChange={set("model")} placeholder="accounts/fireworks/models/…" style={{ fontFamily: "var(--font-mono)" }} /></div>
        <div className="frow two"><div className="fld"><label className="fld-label">Price in ($ / 1M)</label><input className="m-input" type="number" step="0.01" value={f.price_input_per_m ?? ""} onChange={set("price_input_per_m")} /></div><div className="fld"><label className="fld-label">Price out ($ / 1M)</label><input className="m-input" type="number" step="0.01" value={f.price_output_per_m ?? ""} onChange={set("price_output_per_m")} /></div></div>
        <div className="fld"><label className="fld-label">Cached input ($ / 1M) <span className="opt">— optional</span></label><input className="m-input" type="number" step="0.01" value={f.price_cached_input_per_m ?? ""} onChange={set("price_cached_input_per_m")} /></div>
        {provider && <div className="danger-zone"><div className="dz"><b>Remove provider</b><span>{provider.role === "primary" ? "The next provider becomes primary." : "Agents routed to it fall through to the primary."}</span></div><button className="m-btn danger sm" disabled={busy} onClick={() => void remove()}>Remove</button></div>}
      </div>
      <div className="ov-foot">
        <button className="m-btn ghost sm" disabled={!f.base_url || !f.model || test === "testing"} onClick={() => void probe()}>{test === "testing" ? <><span className="m-spin" style={{ width: 13, height: 13, borderWidth: 2 }} /> Testing…</> : test ? <><i className={`ti ${test.startsWith("ok") ? "ti-circle-check" : "ti-alert-circle"}`} style={{ color: test.startsWith("ok") ? "var(--good)" : "var(--bad)" }} /> {test}</> : <><i className="ti ti-activity" /> Test key</>}</button>
        <div className="sp" /><button className="m-btn ghost" data-close>Cancel</button><button className="m-btn primary" disabled={busy || !f.name || !f.base_url || !f.model} onClick={() => void save()}>{isNew ? "Connect" : "Save"}</button>
      </div>
    </div>
  );
}

/** The one route: primary, then the fallback the server turns to when the primary fails. */
function RouteModal({ cfg, onSave }: { cfg: LlmProviders; onSave: (next: LlmProviderInput[], primary: string | null, fallback: string | null, said: string) => Promise<void> }) {
  const { close } = useOverlay();
  const [primary, setPrimary] = useState(cfg.primary ?? "");
  const [fallback, setFallback] = useState(cfg.fallback ?? "");
  return (
    <div className="m-modal lg">
      <OvHead icon="ti-route" title="Default route" sub="Ordered chain — the server tries the primary, then the fallback when it fails." />
      <div className="ov-body">
        <div className="frow two">
          <div className="fld"><label className="fld-label">Answers first</label><select className="m-select" value={primary} onChange={(e) => setPrimary(e.target.value)}>{cfg.providers.map((p) => <option key={p.id} value={p.id}>{p.name} · {shortModel(p.model)}</option>)}</select></div>
          <div className="fld"><label className="fld-label">Steps in if it fails</label><select className="m-select" value={fallback} onChange={(e) => setFallback(e.target.value)}><option value="">None</option>{cfg.providers.filter((p) => p.id !== primary).map((p) => <option key={p.id} value={p.id}>{p.name} · {shortModel(p.model)}</option>)}</select></div>
        </div>
        <div className="m-hint">Applied live: the next model call goes to the new chain; nothing restarts.</div>
      </div>
      <div className="ov-foot"><div className="sp" /><button className="m-btn ghost" data-close>Cancel</button><button className="m-btn primary" disabled={!primary} onClick={async () => { await onSave(cfg.providers.map(asInput), primary, fallback || null, "Route saved"); close(); }}>Save route</button></div>
    </div>
  );
}

function EndpointModal() {
  const { toast } = useOverlay();
  const base = apiBase();
  return (
    <div className="m-modal lg">
      <OvHead icon="ti-link" title="Gateway endpoint" sub="Every agent run goes through the server; point a client at it and name an agent." />
      <div className="ov-body">
        <div className="fld"><label className="fld-label">Base URL</label><div className="copy-row"><span>{base}</span><i className="ti ti-copy" onClick={() => { void navigator.clipboard?.writeText(base); toast("Copied"); }} /></div></div>
        <div className="fld"><label className="fld-label">Example</label><div className="pv-code">{`curl ${base}/threads/{thread}/runs/wait \\\n  -H "Content-Type: application/json" \\\n  -d '{"assistant_id": "…", "input": {"messages": [{"role":"user","content":"hi"}]}}'`}</div></div>
        <div className="m-alert info"><div className="a-ic"><i className="ti ti-info-circle" /></div><div className="a-body"><div className="a-title">Keys are sealed on the server</div><div className="a-text">A provider key is written once and never read back; every call is journalled to the run that made it.</div></div></div>
      </div>
      <div className="ov-foot"><div className="sp" /><button className="m-btn secondary" data-close>Done</button></div>
    </div>
  );
}
