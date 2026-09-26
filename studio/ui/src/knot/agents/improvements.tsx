import { useEffect, useState } from "react";
import { verifierEvidence, type VerifierFloor } from "../../engine/net/client";
import { useOverlay } from "../overlay";
import type { VersionEvidence } from "../../engine/net/client";
import { REVIEW_MAX_RUNS, type Proposal, type Proposals, type ReviewCadence } from "./proposals";

const CADENCE_WORDS: Record<ReviewCadence, string> = { off: "when asked", hourly: "every hour", daily: "every day", weekly: "every week" };

export const KIND_ICON: Record<string, string> = { instructions: "ti-file-text", tools: "ti-tool", skills: "ti-puzzle", guardrails: "ti-shield-check", model: "ti-cpu", memory: "ti-brain", connectors: "ti-plug-connected" };

/** Suggested improvements: versions filed for this agent, waiting for a person's yes or no. */
export function ImprovementsBlock({ proposals, policy = "person", setPolicy, gate, setGate }: { proposals: Proposals; policy?: "person" | "auto"; setPolicy?: (p: "person" | "auto") => void; gate?: { pass_rate?: number; budget_usd?: number }; setGate?: (g: { pass_rate?: number; budget_usd?: number }) => void }) {
  const [rate, setRate] = useState(String(gate?.pass_rate ?? 100));
  const [cap, setCap] = useState(String(gate?.budget_usd ?? 1));
  useEffect(() => { setRate(String(gate?.pass_rate ?? 100)); setCap(String(gate?.budget_usd ?? 1)); }, [gate?.pass_rate, gate?.budget_usd]);
  const commitGate = () => { const r = Math.min(100, Math.max(50, Number(rate) || 100)); const c = Math.min(20, Math.max(0.05, Number(cap) || 1)); setRate(String(r)); setCap(String(c)); if (r !== (gate?.pass_rate ?? 100) || c !== (gate?.budget_usd ?? 1)) setGate?.({ pass_rate: r, budget_usd: c }); };
  const [floor, setFloor] = useState<VerifierFloor | null>(null);
  useEffect(() => { verifierEvidence().then((e) => setFloor(e.floor)).catch(() => {}); }, []);
  const { toast } = useOverlay();
  const [collapsed, setCollapsed] = useState(false);
  const { list, open, busy, apply, dismiss, judge, asking, rescan, coach, review, setCadence } = proposals;
  const cadence = (review?.metadata?.studio?.cadence as ReviewCadence | undefined) ?? (review ? "daily" : "off");
  const [settingCadence, setSettingCadence] = useState(false);
  async function changeCadence(next: ReviewCadence) {
    setSettingCadence(true);
    try { await setCadence(next); toast(next === "off" ? "The Coach looks only when you ask" : `The Coach looks ${CADENCE_WORDS[next]}, up to ${REVIEW_MAX_RUNS} times`, "ti-calendar-repeat"); }
    catch (err) { toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle"); }
    finally { setSettingCadence(false); }
  }
  async function approve(p: Proposal) {
    try { await apply(p); toast(`Applied — ${p.title}`, "ti-sparkles"); }
    catch (err) { toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle"); }
  }
  async function decline(p: Proposal) {
    try { await dismiss(p, "Dismissed from the builder"); }
    catch (err) { toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle"); }
  }
  const [settings, setSettings] = useState(false);
  const [openDiff, setOpenDiff] = useState<string | null>(null);
  // Where a suggestion came from, in words: a gap the agent filed, or who proposed it.
  const from = (p: Proposal) => { const who = p.evidence[0] ?? ""; return /^gap\b/i.test(who) ? "From something the agent was asked and could not do" : who && who !== "a version" ? `Suggested by ${who}` : "Suggested by the Coach"; };
  // The checks, in one line a person reads.
  const checked = (g: VersionEvidence) => {
    if (g.unevaluated) return { tone: "none", text: "Not checked — the agent has no test cases yet" };
    if (g.suites.some((x) => x.state === "running")) return { tone: "running", text: "Being checked…" };
    const passed = g.suites.reduce((n, x) => n + x.passed, 0); const total = g.suites.reduce((n, x) => n + x.total, 0);
    return g.ok ? { tone: "ok", text: `Checked: passed ${passed} of ${total} test cases` } : { tone: "bad", text: `Checked: failed — passed ${passed} of ${total} test cases` };
  };
  return (
    <div className={`m-card block${collapsed ? " collapsed" : ""}`} data-block data-section="improvements" id="improvements">
      <div className="block-head" onClick={(e) => { if ((e.target as HTMLElement).closest(".head-act button, .m-btn, select")) return; setCollapsed((c) => !c); }}>
        <div className="block-ic"><i className="ti ti-sparkles" /></div>
        <div className="block-titles">
          <div className="block-title">Suggested improvements <span className="cnt">{open.length}</span></div>
          <div className="block-sub">The Coach suggests changes from the agent's own runs{cadence !== "off" ? `, ${CADENCE_WORDS[cadence]}` : ""}.</div>
        </div>
        <div className="head-act">
          <button className="m-btn ghost sm" data-rescan disabled={asking || !coach} title={coach ? undefined : "No Coach on this server"} onClick={() => void rescan()}>{asking ? <><span className="m-spin" style={{ width: 12, height: 12, borderWidth: 2 }} /> Looking…</> : <><i className="ti ti-refresh" /> Check now</>}</button>
          <button className="m-btn ghost sm" data-flow="improvement-settings" onClick={() => setSettings((v) => !v)}><i className="ti ti-settings" /> Settings</button>
          <i className="ti ti-chevron-down chev" />
        </div>
      </div>
      <div className="block-body" data-imp-body>
        {settings && (
          <div className="imp-policy" data-policy>
            <label><i className="ti ti-calendar-repeat" /> Look for improvements <select data-review-cadence value={cadence} disabled={!coach || settingCadence} onChange={(e) => void changeCadence(e.target.value as ReviewCadence)}><option value="off">only when I ask</option><option value="hourly">every hour</option><option value="daily">every day</option><option value="weekly">every week</option></select></label>
            <label style={{ marginTop: 6 }}><i className="ti ti-shield-check" /> When a change passes its checks <select data-promotion-policy value={policy} disabled={!setPolicy} onChange={(e) => setPolicy?.(e.target.value as "person" | "auto")}><option value="person">ask me first</option><option value="auto">apply it</option></select></label>
            {policy === "auto" && floor && !floor.stands && <div className="m-hint">Changes still wait for you until the checker has proved reliable — {floor.why}.</div>}
            <label data-gate-bar style={{ marginTop: 6 }}><i className="ti ti-target" /> A change must pass <input className="m-input" style={{ width: 56, display: "inline-block" }} value={rate} disabled={!setGate} onChange={(e) => setRate(e.target.value.replace(/[^0-9]/g, ""))} onBlur={commitGate} data-flow="gate-pass-rate" />% of the test cases, and never do worse than now</label>
            <label style={{ marginTop: 6 }}><i className="ti ti-coin" /> Spend up to $<input className="m-input" style={{ width: 64, display: "inline-block" }} value={cap} disabled={!setGate} onChange={(e) => setCap(e.target.value.replace(/[^0-9.]/g, ""))} onBlur={commitGate} data-flow="gate-budget" /> checking each change</label>
            {review && review.runs_fired >= (review.max_runs ?? Infinity) && <div className="m-hint">Scheduled checks have used their allowance; choose a schedule again to renew it.</div>}
          </div>
        )}
        {list.length === 0 && <div className="imp-empty">Nothing to suggest right now.</div>}
        {list.map((i) => { const c = i.gate ? checked(i.gate) : null; const id = i.version.version_id; return (
          <div key={id} className={`imp${i.state !== "open" ? " done" : ""}`} data-imp={id}>
            <div className="item-ic"><i className={`ti ${KIND_ICON[i.section] ?? "ti-file-text"}`} /></div>
            <div className="ib">
              <div className="it">{i.title}{i.state === "applied" ? <span className="m-badge good sm" style={{ marginLeft: 6 }}><span className="dot" /> Applied</span> : i.state === "dismissed" ? <span className="m-badge sm" style={{ marginLeft: 6 }}>Dismissed</span> : null}</div>
              <div className="id">{i.desc}</div>
              <div className="iev" style={{ display: "flex", gap: 10, alignItems: "center", flexWrap: "wrap" }}>
                <span>{from(i)}</span>
                {c && <span className={`igate-line ${c.tone}`} data-flow="imp-checked" style={{ color: c.tone === "ok" ? "var(--good)" : c.tone === "bad" ? "var(--bad)" : "var(--ink-500)" }}>{c.text}</span>}
                {(i.diff.length > 0 || i.gate) && <button className="m-btn ghost sm" data-flow="imp-see" onClick={() => setOpenDiff(openDiff === id ? null : id)}>{openDiff === id ? "Hide the change" : "See the change"}</button>}
              </div>
              {openDiff === id && <>
                {i.diff.length > 0 && <div className="diff">{i.diff.map((d, n) => <div key={n} className={d.kind}>{d.kind === "add" ? "+ " : "− "}{d.text}</div>)}</div>}
                {i.gate && <GateLine gate={i.gate} busy={busy === id} onJudge={i.state === "open" ? () => void judge(i).catch((err) => toast(err instanceof Error ? err.message : "the server refused", "ti-alert-triangle")) : undefined} />}
              </>}
            </div>
            {i.state === "open" && <div className="ia"><button className="m-btn ghost sm" data-dismiss disabled={busy === id} onClick={() => void decline(i)}>Dismiss</button><button className="m-btn primary sm" data-approve disabled={busy === id} onClick={() => void approve(i)}><i className="ti ti-check" /> Approve</button></div>}
          </div>
        ); })}
      </div>
    </div>
  );
}

/** The candidate gate's verdict on a proposal: each suite bound to the
 * agent, run against the version when it was filed — passed, failed with
 * why, running, over budget, or nothing judged it. */
function GateLine({ gate, busy, onJudge }: { gate: VersionEvidence; busy: boolean; onJudge?: () => void }) {
  if (gate.unevaluated) return <div className="igate none"><i className="ti ti-shield-off" /> No suite is bound to this agent — nothing judged it. Record cases from its runs in Evals first.</div>;
  const running = gate.suites.some((s) => s.state === "running");
  const canJudge = onJudge && !running && gate.suites.some((s) => s.state === "missing" || s.state === "stale" || s.state === "failed" || s.state === "over budget");
  return (
    <div className={`igate${gate.ok ? " ok" : running ? " running" : " bad"}`}>
      <i className={`ti ${gate.ok ? "ti-shield-check" : running ? "ti-loader-2" : "ti-shield-x"}`} />
      <span className="igate-body">
        {gate.suites.map((s) => (
          <span key={`${s.name}:${s.version}`} className="igate-suite" title={s.state === "stale" ? (s.stale_because ?? []).join("; ") : undefined}>
            <b>{s.name}</b>{" "}
            {s.state === "running" ? `judging, ${s.passed} of ${s.total} passed so far…` : s.state === "missing" ? "not judged" : s.state === "over budget" ? `over budget after ${s.passed}/${s.total}` : `${s.state} ${s.passed}/${s.total}`}{s.below ? ` — ${s.below}` : ""}
            {s.baseline && s.state !== "running" && s.state !== "missing" && <span className="igate-base"> · runs now {s.baseline.passed}/{s.baseline.total}</span>}
            {(s.state === "failed" || s.state === "over budget") && (s.failures ?? []).length > 0 && <span className="igate-why"> — {(s.failures ?? []).map((f) => (f.said.length > 200 ? `${f.said.slice(0, 200).trimEnd()}…` : f.said)).filter(Boolean).slice(0, 2).join(" · ")}</span>}
          </span>
        ))}
      </span>
      {canJudge && <button className="m-btn ghost sm" disabled={busy} onClick={onJudge}>Judge again</button>}
    </div>
  );
}
