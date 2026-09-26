import { useCallback, useEffect, useRef, useState } from "react";
import { activateAssistantVersion, assistantVersion, assistantVersions, createAssistantVersion, declineAssistantVersion, type Assistant, type AssistantIntent, type AssistantVersion } from "../../engine/net/client";
import { useServer } from "../../engine/net/server";
import { statusOf } from "../data";

/**
 * The agent under the builder: its newest version as the working copy,
 * every change a new version on the server after a short debounce (the
 * prototype's "Saved just now"), Publish activating the newest.
 */
export interface AgentDraft {
  name: string;
  description: string;
  intent: AssistantIntent;
  metadata: NonNullable<Assistant["metadata"]> & { studio?: { color?: string; icon?: string } };
  recursion_limit?: number;
}

/** The builder's default step limit, shown on the slider and written with it. */
export const DEFAULT_STEPS = 25;

export type SaveState = "saved" | "saving" | "unsaved" | "failed";

export function useAgent(agent: Assistant | null) {
  const [versions, setVersions] = useState<AssistantVersion[]>([]);
  const [draft, setDraft] = useState<AgentDraft | null>(null);
  const [save, setSave] = useState<SaveState>("saved");
  const [savedAt, setSavedAt] = useState<number | null>(null);
  const [problem, setProblem] = useState<string | null>(null);
  const base = useRef<string | null>(null);
  /** The newest version the builder made or loaded — what Publish activates. */
  const latest = useRef<string | null>(null);
  const timer = useRef<number | null>(null);
  const pending = useRef<AgentDraft | null>(null);

  const load = useCallback(async (a: Assistant) => {
    try {
      const v = await assistantVersions(a.assistant_id);
      const list = v.versions;
      setVersions(list);
      // The listing carries lineage only. The server files a new version on top of the one that
      // serves (a compare-and-set on the active version), so the working copy is the person's own
      // newest draft above the active version when there is one, else the active version itself.
      const isProposal = (x: AssistantVersion) => { const m = x.metadata as { proposed_by?: unknown } | undefined; return !!m?.proposed_by; };
      const active = list.find((x) => x.active) ?? null;
      // The listing omits metadata, so the versions above the active one are read in full before they are told apart.
      // Proposals are not the working copy: the listing marks them, so they
      // are skipped before the newest six are read in full.
      const above = list.filter((x) => !x.active && !x.declined && !x.proposed_by && (!active || x.created_at > active.created_at)).sort((x, y) => y.created_at.localeCompare(x.created_at)).slice(0, 6);
      const aboveFull = (await Promise.all(above.map((x) => assistantVersion(a.assistant_id, x.version_id).catch(() => null)))).filter((x): x is AssistantVersion => !!x);
      const draftVersion = aboveFull.find((x) => !isProposal(x)) ?? null;
      const fromId = draftVersion?.version_id ?? active?.version_id ?? [...list].sort((x, y) => y.created_at.localeCompare(x.created_at))[0]?.version_id;
      const newest = draftVersion ?? (fromId ? await assistantVersion(a.assistant_id, fromId) : null);
      const from = newest ?? { name: a.name, config: a.config, metadata: a.metadata, version_id: a.active_version_id ?? "" };
      base.current = active?.version_id ?? a.active_version_id ?? from.version_id;
      latest.current = from.version_id;
      setDraft({ name: from.name ?? a.name, description: from.metadata?.description ?? "", intent: from.config?.studio_intent ?? {}, metadata: (from.metadata ?? {}) as AgentDraft["metadata"], recursion_limit: from.config?.recursion_limit });
      setSave("saved");
    } catch (err) {
      setProblem(err instanceof Error ? err.message : "the agent could not be read");
    }
  }, []);
  useEffect(() => { setDraft(null); setVersions([]); setProblem(null); if (agent) void load(agent); }, [agent?.assistant_id]); // eslint-disable-line react-hooks/exhaustive-deps
  // The served version moved under the builder — a Coach proposal applied, a
  // restore, a publish from elsewhere — so the working copy is read again in
  // place (no reset: the editor keeps its scroll and focus).
  const servedRef = useRef<string | null | undefined>(undefined);
  useEffect(() => {
    const served = agent?.active_version_id ?? null;
    if (servedRef.current !== undefined && servedRef.current !== served && agent) void load(agent);
    servedRef.current = served;
  }, [agent?.active_version_id]); // eslint-disable-line react-hooks/exhaustive-deps

  const persist = useCallback(async (a: Assistant, d: AgentDraft) => {
    setSave("saving");
    try {
      const made = await createAssistantVersion(a.assistant_id, {
        base_version_id: base.current ?? a.active_version_id ?? "",
        name: d.name,
        graph: a.graph,
        metadata: (({ proposed_by: _p, why: _w, ...rest }) => ({ ...rest, description: d.description }))(d.metadata as Record<string, unknown>),
        // The step limit the builder shows is the one that runs: written on
        // every save, so an agent never silently takes the graph's own ceiling.
        config: { studio_intent: d.intent, recursion_limit: d.recursion_limit ?? DEFAULT_STEPS },
      });
      // Versions are content-addressed: a working copy edited back to what
      // runs lands on the running version itself, and the draft version it
      // came from would otherwise stay the newest and read back on the
      // next load as if the edit had never happened. It is superseded.
      // The same holds for any older version the save lands on: every
      // newer working-copy version above it is now stale.
      if (made.created === false) {
        const all = await assistantVersions(a.assistant_id).catch(() => null);
        const stale = (all?.versions ?? []).filter((x) => !x.active && !x.declined && x.version_id !== made.version.version_id && x.created_at > made.version.created_at);
        for (const x of stale) {
          const full = await assistantVersion(a.assistant_id, x.version_id).catch(() => null);
          if (full && (full.metadata as { proposed_by?: unknown } | undefined)?.proposed_by) continue;
          await declineAssistantVersion(a.assistant_id, x.version_id, "superseded: the working copy was edited back to an earlier version").catch(() => {});
        }
        if (stale.length) setVersions((v) => v.map((x) => (stale.some((s) => s.version_id === x.version_id) ? { ...x, declined: { reason: "superseded", by: {}, at: new Date().toISOString(), version_id: x.version_id } } : x)));
      }
      latest.current = made.version.version_id;
      setVersions((v) => [made.version, ...v.filter((x) => x.version_id !== made.version.version_id)]);
      setSave("saved"); setSavedAt(Date.now());
      void useServer.getState().refresh();
    } catch (err) {
      setSave("failed"); setProblem(err instanceof Error ? err.message : "the change was not saved");
    }
  }, []);

  /** Change the working copy; the version lands after 900 ms of quiet. */
  const edit = useCallback((change: (d: AgentDraft) => AgentDraft) => {
    if (!agent) return;
    setDraft((d) => {
      if (!d) return d;
      const next = change(d);
      pending.current = next;
      return next;
    });
    setSave("unsaved");
    if (timer.current) window.clearTimeout(timer.current);
    timer.current = window.setTimeout(() => { if (pending.current) void persist(agent, pending.current); }, 900);
  }, [agent, persist]);

  /** Publish: the newest version becomes the one that runs. */
  const publish = useCallback(async (reason?: string, note?: string) => {
    if (!agent) return null;
    // The studio's own mark: published once, kept on the metadata the version carries.
    const current = pending.current ?? draft;
    const marked = !!(current?.metadata.studio as { published_at?: string } | undefined)?.published_at;
    // A note the person writes for this version rides on its metadata, so
    // Versions reads their words before the derived ones.
    if (current && (!marked || note || (pending.current && save !== "saved"))) {
      if (timer.current) window.clearTimeout(timer.current);
      // Publishing is the review a spawned agent waits for: the stamp comes off here.
      const reviewed = (({ awaiting_review: _a, ...rest }) => rest)(current.metadata as Record<string, unknown>) as typeof current.metadata;
      const stamped = marked && !("awaiting_review" in current.metadata) ? current : { ...current, metadata: { ...reviewed, studio: { ...(current.metadata.studio ?? {}), published_at: (current.metadata.studio as { published_at?: string } | undefined)?.published_at ?? new Date().toISOString() } } };
      const next = note ? { ...stamped, metadata: { ...stamped.metadata, studio: { ...(stamped.metadata.studio ?? {}), note } } } : stamped;
      pending.current = next; setDraft(next);
      await persist(agent, next);
    }
    const target = latest.current ?? base.current;
    if (!target) return null;
    const out = await activateAssistantVersion(agent.assistant_id, target, agent.active_version_id ?? "", reason);
    void useServer.getState().refresh();
    void load(agent);
    return out;
  }, [agent, versions, save, persist, load, draft]);

  const ran = useServer((s) => s.runs).some((r) => r.assistant_id === agent?.assistant_id);
  const status = agent ? statusOf(agent, versions, ran) : "draft";
  return { draft, edit, save, savedAt, status, versions, problem, publish, reload: () => agent && load(agent) };
}
