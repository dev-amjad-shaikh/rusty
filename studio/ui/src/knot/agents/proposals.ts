import { useCallback, useEffect, useRef, useState } from "react";
import { activateAssistantVersion, assistantVersion, assistantVersions, createSchedule, createThread, declineAssistantVersion, deleteSchedule, judgeVersion, listSchedules, runAndWait, versionEvidence, type Assistant, type AssistantIntent, type AssistantVersion, type Schedule, type VersionEvidence } from "../../engine/net/client";
import { useServer } from "../../engine/net/server";
import { ago } from "../data";
import { firstSentence, plainName } from "./words";

/**
 * Suggested improvements = versions filed for this agent that do not run
 * yet: the Coach's (from its review of the runs) and any newer than the
 * active one. Approving activates; dismissing declines with a reason; a
 * request in words goes to the Coach, which files a version if it agrees.
 * One store, read by the page block and the Build pane alike.
 */
export interface DiffLine { kind: "add" | "del"; text: string }
export interface Proposal {
  version: AssistantVersion;
  title: string;
  desc: string;
  /** The block it changes: instructions · tools · skills · guardrails · model · memory. */
  section: string;
  /** Who filed it: the Coach from run analysis, or a person's request. */
  src: "scan" | "ask";
  evidence: string[];
  /** The candidate gate's verdict: every suite bound to the agent, run
   * against this version when it was filed. Null until read. */
  gate: VersionEvidence | null;
  diff: DiffLine[];
  state: "open" | "applied" | "dismissed";
}

const lines = (s: string | undefined) => (s ?? "").split("\n").map((l) => l.trim()).filter(Boolean);

/** What a version changes against what runs, as +/− lines. */
export function diffIntent(running: AssistantIntent, next: AssistantIntent): { diff: DiffLine[]; section: string; changes: string[] } {
  const diff: DiffLine[] = [];
  const changes: string[] = [];
  const a = lines(running.instructions), b = lines(next.instructions);
  const aSet = new Set(a), bSet = new Set(b);
  const del = a.filter((l) => !bSet.has(l)), add = b.filter((l) => !aSet.has(l));
  if (del.length || add.length) changes.push("the charter");
  for (const l of del) diff.push({ kind: "del", text: l });
  for (const l of add) diff.push({ kind: "add", text: l });
  const at = new Set((running.tools ?? []).map((t) => t.name)), bt = new Set((next.tools ?? []).map((t) => t.name));
  const addedT = [...bt].filter((t) => !at.has(t)), droppedT = [...at].filter((t) => !bt.has(t));
  for (const t of droppedT) diff.push({ kind: "del", text: `Removes the tool ${plainName(t)}` });
  for (const t of addedT) diff.push({ kind: "add", text: `Adds the tool ${plainName(t)}` });
  if (addedT.length || droppedT.length) changes.push("tools");
  const as = new Set(running.skills ?? []), bs = new Set(next.skills ?? []);
  const addedS = [...bs].filter((k) => !as.has(k)), droppedS = [...as].filter((k) => !bs.has(k));
  for (const k of droppedS) diff.push({ kind: "del", text: `Removes the skill ${plainName(k)}` });
  for (const k of addedS) diff.push({ kind: "add", text: `Adds the skill ${plainName(k)}` });
  if (addedS.length || droppedS.length) changes.push("skills");
  if ((running.model ?? "") !== (next.model ?? "")) { changes.push("model"); diff.push({ kind: "add", text: `model · ${next.model ?? "deployment default"}` }); }
  if ((running.approval ?? "") !== (next.approval ?? "") || JSON.stringify(running.budget ?? null) !== JSON.stringify(next.budget ?? null)) changes.push("guardrails");
  if (JSON.stringify(running.memory ?? null) !== JSON.stringify(next.memory ?? null)) changes.push("memory");
  const section = changes.includes("the charter") ? "instructions" : changes[0] ?? "instructions";
  return { diff, section, changes };
}

/**
 * A proposal's changes against what runs, carried into the working copy:
 * lines the Coach added to the charter go in where it put them (front or
 * end), lines it removed come out; tools, skills and variables it added or
 * removed are added or removed; a setting it changed takes its value. The
 * person's own unpublished edits survive — the Coach revised what runs, not
 * the draft, and the draft is what Publish activates.
 */
export function mergeProposal(draft: AssistantIntent, running: AssistantIntent, next: AssistantIntent): AssistantIntent {
  const out: AssistantIntent = { ...draft };
  const base = lines(running.instructions); const mine = lines(next.instructions);
  if ((running.instructions ?? "") !== (next.instructions ?? "")) {
    if ((draft.instructions ?? "") === (running.instructions ?? "")) out.instructions = next.instructions;
    else {
      const added = mine.filter((l) => !base.includes(l)); const removed = base.filter((l) => !mine.includes(l));
      const firstKept = mine.findIndex((l) => base.includes(l));
      const atFront = firstKept === -1 ? false : mine.slice(0, firstKept).length > 0 && added.length > 0 && mine.slice(0, firstKept).every((l) => added.includes(l));
      const kept = (draft.instructions ?? "").split("\n").filter((l) => !removed.includes(l.trim()));
      out.instructions = atFront ? [...added, "", ...kept].join("\n") : [...kept, "", ...added].join("\n");
    }
  }
  const names = (t: { name: string }[] | undefined) => (t ?? []).map((x) => x.name);
  const tAdded = (next.tools ?? []).filter((t) => !names(running.tools).includes(t.name)); const tRemoved = names(running.tools).filter((n) => !names(next.tools).includes(n));
  if (tAdded.length || tRemoved.length) out.tools = [...(draft.tools ?? []).filter((t) => !tRemoved.includes(t.name)), ...tAdded.filter((t) => !names(draft.tools).includes(t.name))];
  const sAdded = (next.skills ?? []).filter((x) => !(running.skills ?? []).includes(x)); const sRemoved = (running.skills ?? []).filter((x) => !(next.skills ?? []).includes(x));
  if (sAdded.length || sRemoved.length) out.skills = [...new Set([...(draft.skills ?? []).filter((x) => !sRemoved.includes(x)), ...sAdded])];
  const vAdded = (next.variables ?? []).filter((v) => !names(running.variables).includes(v.name)); const vRemoved = names(running.variables).filter((n) => !names(next.variables).includes(n));
  if (vAdded.length || vRemoved.length) out.variables = [...(draft.variables ?? []).filter((v) => !vRemoved.includes(v.name)), ...vAdded.filter((v) => !names(draft.variables).includes(v.name))];
  for (const key of ["model", "approval", "format", "pool", "budget", "memory", "output", "binding"] as const) {
    if (JSON.stringify(running[key] ?? null) !== JSON.stringify(next[key] ?? null)) (out as Record<string, unknown>)[key] = next[key];
  }
  return out;
}


/** What the Coach is asked when it reviews an agent: with a request in
 * words it files what the person asks for; without one it reads the runs
 * for a pattern and files a charter only if the charter is the cause. */
export function reviewAsk(agent: Assistant, text: string): string {
  return text.trim() ? `Review the agent ${agent.name} (id ${agent.assistant_id}): read its charter, tools and skills, and its recent runs. The person asks: ${text.trim()} — file ONE revision with agents.revise that does what they ask: the charter (add_first / replace), the tools it may call (add_tools, exact names from catalog.tools or a connected system's operations from catalog.connectors), the skills it follows (register a procedure with skills.register, then add_skills). When they ask for a capability the agent lacks, add the tool or skill AND the charter step that says when to use it. Say in two sentences what you changed and why.` : `Review the agent ${agent.name} (id ${agent.assistant_id}): read its charter, its goal and how far its live runs are from it, the gaps it filed that stay open (agents.read gives all three), and its recent runs. Find the one pattern behind the failed verdicts that explains the shortfall, and file one revised charter with agents.revise only when the charter is the cause — say the hypothesis and the runs that show it. When the shortfall is a missing tool or knowledge the gaps name, say so instead of revising the charter. When the goal is on target and nothing failed, say so and change nothing.`;
}

/** The Coach on a cadence: the schedule on the Coach that reviews this agent, capped from day one. */
export type ReviewCadence = "off" | "hourly" | "daily" | "weekly";
export const REVIEW_INTERVALS: Record<Exclude<ReviewCadence, "off">, number> = { hourly: 3600, daily: 86_400, weekly: 604_800 };
/** The most reviews a cadence may ever fire before a person renews it. */
export const REVIEW_MAX_RUNS = 30;
/** The most tokens those reviews may spend, all told, before a person renews it. */
export const REVIEW_MAX_TOKENS = 600_000;
export const reviewScheduleOf = (schedules: Schedule[], coachId: string, agentId: string) => schedules.find((c) => c.assistant_id === coachId && c.metadata?.studio?.coach_review_of === agentId) ?? null;

export function useProposals(agent: Assistant | null, onMerge?: (merge: (draft: AssistantIntent) => AssistantIntent) => void) {
  const agents = useServer((s) => s.assistants);
  const [list, setList] = useState<Proposal[]>([]);
  const [busy, setBusy] = useState<string | null>(null);
  const [said, setSaid] = useState<{ who: "user" | "coach"; text: string }[]>([]);
  const [asking, setAsking] = useState(false);
  const [review, setReview] = useState<Schedule | null>(null);
  const decided = useRef<Map<string, "applied" | "dismissed">>(new Map());
  const coach = agents.find((a) => a.name === "Coach" && !a.archived_at) ?? null;

  const load = useCallback(async (a: Assistant) => {
    const v = await assistantVersions(a.assistant_id);
    const activeNow = v.versions.find((x) => x.version_id === v.active_version_id) ?? null;
    const running = agents.find((x) => x.assistant_id === a.assistant_id)?.config?.studio_intent ?? a.config?.studio_intent ?? {};
    // Proposals are what an agent filed (the Coach's reviews); a person's own newer versions are the draft Publish activates.
    // The listing omits metadata, so candidates are read in full before they are told apart.
    // Every proposal above the active version, however many working-copy
    // saves sit above it: the listing says who proposed a version, so only
    // proposals are read in full (a listing without the mark falls back to
    // reading the newest eight).
    const aboveAll = v.versions.filter((x) => !x.active && !x.declined && (activeNow === null || x.created_at > activeNow.created_at)).sort((x, y) => (x.created_at < y.created_at ? 1 : -1));
    const marked = aboveAll.some((x) => "proposed_by" in x);
    const above = (marked ? aboveAll.filter((x) => !!x.proposed_by) : aboveAll.slice(0, 8)).slice(0, 40);
    const full = (await Promise.all(above.map((c) => assistantVersion(a.assistant_id, c.version_id).catch(() => null)))).filter((x) => x && !!(x.metadata as { proposed_by?: unknown } | undefined)?.proposed_by);
    const out: Proposal[] = [];
    const gates = await Promise.all(full.map((ver) => (ver ? versionEvidence(a.assistant_id, ver.version_id).then((r) => r.evidence).catch(() => null) : Promise.resolve(null))));
    for (const [n, ver] of full.entries()) {
      if (!ver) continue;
      const next = ver.config?.studio_intent ?? {};
      if (JSON.stringify(next) === JSON.stringify(running)) continue; // changes nothing against what runs
      const meta = (ver.metadata ?? {}) as { proposed_by?: { name?: string; kind?: string; reason?: string } | string; created_by?: { name?: string; kind?: string }; reason?: string; description?: string };
      const proposer = typeof meta.proposed_by === "object" ? meta.proposed_by : null;
      const byService = !!proposer || meta.created_by?.kind === "service";
      // The reason in a person's words: tool ids as names, the first
      // sentence as the title (a dot inside `agents.list` is not one).
      const reason = (proposer?.reason ?? meta.reason ?? "").replace(/`([a-z0-9_.:-]+)`/gi, (_m: string, id: string) => plainName(id));
      const head = reason ? firstSentence(reason.split("\n")[0].split(" — ")[0], 90).replace(/…$/, "") : "";
      const { diff, section, changes } = diffIntent(running, next);
      out.push({
        version: ver,
        title: head || `Revise ${changes.join(", ") || "the agent"}`,
        desc: reason && reason.length > head.length ? reason.slice(head.length).replace(/^[\s.—-]+/, "") : `Changes ${changes.join(", ") || "the configuration"} · suggested ${ago(ver.created_at)}`,
        section, src: byService ? "scan" : "ask",
        evidence: [proposer?.name ?? meta.created_by?.name ?? "a version", `v${v.versions.length - v.versions.findIndex((x) => x.version_id === ver.version_id)}`],
        gate: gates[n],
        diff, state: decided.current.get(ver.version_id) ?? "open",
      });
    }
    setList(out);
    const coachNow = agents.find((x) => x.name === "Coach" && !x.archived_at) ?? null;
    if (coachNow) setReview(reviewScheduleOf(await listSchedules().catch(() => []), coachNow.assistant_id, a.assistant_id));
  }, [agents]);
  useEffect(() => { setList([]); setSaid([]); decided.current = new Map(); if (agent) void load(agent).catch(() => setList([])); }, [agent?.assistant_id]); // eslint-disable-line react-hooks/exhaustive-deps
  // A suite still running against a proposal: the verdict is read again
  // until it lands.
  const judging = list.some((p) => p.state === "open" && p.gate?.suites.some((s) => s.state === "running"));
  useEffect(() => {
    if (!judging || !agent) return;
    const t = setInterval(() => { void load(agent).catch(() => {}); }, 12_000);
    return () => clearInterval(t);
  }, [judging, agent, load]);

  /** The Coach on a cadence: one schedule on the Coach per agent, with the
   * review ask as its message and a run cap from day one; off removes it. */
  const setCadence = useCallback(async (cadence: ReviewCadence) => {
    if (!agent || !coach) return;
    if (review) await deleteSchedule(review.cron_id);
    if (cadence === "off") { setReview(null); return; }
    const made = await createSchedule({
      assistant_id: coach.assistant_id,
      interval_secs: REVIEW_INTERVALS[cadence],
      input: { messages: [{ role: "user", content: reviewAsk(agent, "") }] },
      max_runs: REVIEW_MAX_RUNS,
      max_tokens: REVIEW_MAX_TOKENS,
      metadata: { studio: { coach_review_of: agent.assistant_id, cadence } },
    });
    setReview(made);
  }, [agent, coach, review]);

  /** Judge a proposal by hand: its suites run again under the candidate budget. */
  const judge = useCallback(async (p: Proposal) => {
    if (!agent) return;
    setBusy(p.version.version_id);
    try {
      const r = await judgeVersion(agent.assistant_id, p.version.version_id);
      setList((l) => l.map((x) => (x.version.version_id === p.version.version_id ? { ...x, gate: r.evidence } : x)));
    } finally { setBusy(null); }
  }, [agent]);

  /** Apply: the proposal's changes go into the working copy, which Publish
   * activates. The proposal's own version is marked as taken up so it leaves
   * the queue. Without a working copy to merge into, it activates outright. */
  const apply = useCallback(async (p: Proposal, overrideReason?: string) => {
    if (!agent) return;
    setBusy(p.version.version_id);
    try {
      if (onMerge) {
        const running = agents.find((x) => x.assistant_id === agent.assistant_id)?.config?.studio_intent ?? agent.config?.studio_intent ?? {};
        const next = p.version.config?.studio_intent ?? {};
        onMerge((draft) => mergeProposal(draft, running, next));
        await declineAssistantVersion(agent.assistant_id, p.version.version_id, overrideReason ?? "taken into the working copy");
      } else {
        await activateAssistantVersion(agent.assistant_id, p.version.version_id, agent.active_version_id ?? "", overrideReason);
        await useServer.getState().refresh();
      }
      decided.current.set(p.version.version_id, "applied");
      setList((l) => l.map((x) => (x.version.version_id === p.version.version_id ? { ...x, state: "applied" } : x)));
    } finally { setBusy(null); }
  }, [agent, agents, onMerge]);

  const dismiss = useCallback(async (p: Proposal, reason: string) => {
    if (!agent) return;
    setBusy(p.version.version_id);
    try {
      await declineAssistantVersion(agent.assistant_id, p.version.version_id, reason);
      decided.current.set(p.version.version_id, "dismissed");
      setList((l) => l.map((x) => (x.version.version_id === p.version.version_id ? { ...x, state: "dismissed" } : x)));
    } finally { setBusy(null); }
  }, [agent]);

  /** A request in words: the Coach reviews the agent with it and files a version if it agrees. */
  const ask = useCallback(async (text: string) => {
    if (!agent) return;
    setSaid((s) => [...s, { who: "user", text }]);
    if (!coach) { setSaid((s) => [...s, { who: "coach", text: "There is no Coach on this server to review the agent." }]); return; }
    setAsking(true);
    try {
      const thread = await createThread(coach.graph);
      const ask = reviewAsk(agent, text);
      const result = await runAndWait(thread.thread_id, coach.assistant_id, [{ role: "user", content: ask }]);
      const reply = [...(result.output?.messages ?? [])].reverse().find((m) => m.role === "assistant" && m.content && !(m.tool_calls?.length))?.content ?? null;
      setSaid((s) => [...s, { who: "coach", text: reply ?? (result.error ? `The Coach's run ended: ${result.message ?? result.error}` : result.status === "interrupted" ? "The Coach paused for a decision — see Notifications." : "The Coach said nothing.") }]);
      await load(agent);
    } catch (err) {
      setSaid((s) => [...s, { who: "coach", text: err instanceof Error ? err.message : "the Coach could not be reached" }]);
    } finally { setAsking(false); }
  }, [agent, coach, load]);

  const open = list.filter((p) => p.state === "open");
  return { list, open, busy, apply, dismiss, judge, ask, asking, said, coach, review, setCadence, rescan: () => ask(""), reload: () => agent && load(agent) };
}

export type Proposals = ReturnType<typeof useProposals>;
