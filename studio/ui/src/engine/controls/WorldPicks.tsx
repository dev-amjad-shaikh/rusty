import type { World } from "../net/client";
import { worldsByConnector, worldsForAgent } from "../worlds";

/**
 * Where a run acts: one stand-in when the agent reaches one system, one
 * pick per system when it reaches several with stand-ins. `value` is the
 * chosen world names in system order; the first is the run's `world`, all
 * of them its `worlds`. An agent with no stand-in for any system gets a
 * disabled pick that says so.
 */
export function WorldPicks({ toolNames, worlds, value, onChange, idPrefix, hint }: {
  toolNames: string[]; worlds: World[]; value: string[]; onChange: (names: string[]) => void; idPrefix: string;
  /** What a stand-in means here, in one sentence. */
  hint?: string;
}) {
  const perConnector = worldsByConnector(toolNames, worlds);
  const several = perConnector.length > 1;
  const offered = worldsForAgent(toolNames, worlds);
  const selectStyle = { font: "inherit", fontSize: 13, padding: 6 } as const;
  if (several) {
    return (
      <div data-field={`${idPrefix}-worlds`} style={{ display: "grid", gap: 8 }}>
        {perConnector.map((p, i) => (
          <label key={p.connector} style={{ display: "grid", gap: 4, fontSize: 12.5 }}>
            <span style={{ color: "var(--ink2)" }}>World for <b>{p.connector}</b></span>
            <select id={`${idPrefix}-world-${p.connector}`} className="rn-select" data-field={`${idPrefix}-world-${p.connector}`} aria-label={`World for ${p.connector}`} value={value[i] ?? ""} style={selectStyle}
              onChange={(e) => { const next = perConnector.map((q, j) => (j === i ? e.target.value : value[j] ?? "")); onChange(next); }}>
              <option value="">the live system</option>
              {p.worlds.map((w) => <option key={w.world_id} value={w.name}>{w.name} · stands in for {w.stands_for}</option>)}
            </select>
          </label>
        ))}
        {hint && <span style={{ fontSize: 12, color: "var(--ink3)" }}>{hint}</span>}
      </div>
    );
  }
  return (
    <select id={`${idPrefix}-world`} className="rn-select" data-field={`${idPrefix}-world`} aria-label="In world" value={value[0] ?? ""} disabled={offered.length === 0} title={hint} style={selectStyle}
      onChange={(e) => onChange(e.target.value ? [e.target.value] : [])}>
      <option value="">{offered.length === 0 ? "no stand-in for this agent's systems" : "the live systems"}</option>
      {offered.map((w) => <option key={w.world_id} value={w.name}>{w.name} · stands in for {w.stands_for}</option>)}
    </select>
  );
}

/** The chosen names as a run's config: the first as `world`, all as `worlds`. */
export function worldFields(names: string[]): { world?: string; worlds?: string[] } {
  const chosen = names.filter(Boolean);
  if (chosen.length === 0) return {};
  return chosen.length > 1 ? { world: chosen[0], worlds: chosen } : { world: chosen[0] };
}
