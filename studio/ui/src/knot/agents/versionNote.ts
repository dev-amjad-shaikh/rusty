import type { AssistantVersion } from "../../engine/net/client";

type Meta = { created_by?: { name?: string }; proposed_by?: { name?: string }; why?: string; studio?: { published_at?: string; template?: string } };

/** What a version changed against its parent, in a line — the design's
 * version message, derived from the two configs rather than typed. `parent`
 * is the version saved before this one — a history reads as a changelog. */
export function versionNote(v: AssistantVersion, parent: AssistantVersion | null, modelName: (id: string) => string = (id) => id): string {
  const meta = (v.metadata ?? {}) as Meta;
  // The person's own note comes first; what the diff says follows it.
  const own = (meta.studio as { note?: string } | undefined)?.note?.trim();
  if (own) {
    const derived = versionNoteDerived(v, parent, modelName);
    return derived && derived !== "Created" ? `${own} — ${derived}` : own;
  }
  return versionNoteDerived(v, parent, modelName);
}

function versionNoteDerived(v: AssistantVersion, parent: AssistantVersion | null, modelName: (id: string) => string): string {
  const meta = (v.metadata ?? {}) as Meta;
  const a = parent?.config?.studio_intent ?? {};
  const b = v.config?.studio_intent ?? {};
  const parts: string[] = [];
  if (!parent) {
    return meta.studio?.template ? `Created from the ${meta.studio.template} template` : "Created";
  }
  if (meta.proposed_by?.name) parts.push(`Proposed by ${meta.proposed_by.name}${meta.why ? `: ${meta.why}` : ""}`);
  if (v.name !== parent.name) parts.push(`Renamed to ${v.name}`);
  const da = (parent.metadata as { description?: string } | undefined)?.description ?? "";
  const db = (v.metadata as { description?: string } | undefined)?.description ?? "";
  if (da !== db) parts.push("Description edited");
  if ((a.instructions ?? "") !== (b.instructions ?? "")) {
    const wa = (a.instructions ?? "").trim().split(/\s+/).filter(Boolean).length;
    const wb = (b.instructions ?? "").trim().split(/\s+/).filter(Boolean).length;
    const d = wb - wa;
    parts.push(`Instructions edited${d ? ` (${d > 0 ? "+" : "−"}${Math.abs(d)} words)` : ""}`);
  }
  const ta = new Set((a.tools ?? []).map((t) => t.name)); const tb = new Set((b.tools ?? []).map((t) => t.name));
  const added = [...tb].filter((t) => !ta.has(t)); const removed = [...ta].filter((t) => !tb.has(t));
  if (added.length) parts.push(`+ ${added.join(", ")}`);
  if (removed.length) parts.push(`− ${removed.join(", ")}`);
  const notes = (b.tools ?? []).filter((t) => (t.when ?? "") !== ((a.tools ?? []).find((x) => x.name === t.name)?.when ?? "") && ta.has(t.name));
  if (notes.length) parts.push(`When-to-use note on ${notes.map((t) => t.name).join(", ")}`);
  const sa = new Set(a.skills ?? []); const sb = new Set(b.skills ?? []);
  const sAdded = [...sb].filter((s) => !sa.has(s)); const sRemoved = [...sa].filter((s) => !sb.has(s));
  if (sAdded.length) parts.push(`Skill ${sAdded.join(", ")} added`);
  if (sRemoved.length) parts.push(`Skill ${sRemoved.join(", ")} removed`);
  if ((a.model ?? "") !== (b.model ?? "")) parts.push(b.model ? `Model → ${modelName(b.model)}` : "Model → deployment default");
  if ((a.fallback_model ?? "") !== (b.fallback_model ?? "")) parts.push(b.fallback_model ? `Fallback → ${modelName(b.fallback_model)}` : "Fallback → deployment default");
  const va = new Set((a.variables ?? []).map((x) => x.name)); const vb = new Set((b.variables ?? []).map((x) => x.name));
  const vAdded = [...vb].filter((x) => !va.has(x)); const vRemoved = [...va].filter((x) => !vb.has(x));
  if (vAdded.length) parts.push(`Variable ${vAdded.map((x) => `{{${x}}}`).join(", ")} added`);
  if (vRemoved.length) parts.push(`Variable ${vRemoved.map((x) => `{{${x}}}`).join(", ")} removed`);
  if ((a.approval ?? "") !== (b.approval ?? "")) parts.push("Approval policy changed");
  if (JSON.stringify(a.budget ?? {}) !== JSON.stringify(b.budget ?? {})) parts.push("Budget changed");
  if (JSON.stringify(a.memory ?? {}) !== JSON.stringify(b.memory ?? {})) parts.push("Memory changed");
  if ((a.pool ?? "") !== (b.pool ?? "")) parts.push(b.pool ? `Works the ${b.pool} pool` : "Left its pool");
  const pa = ((parent.metadata ?? {}) as Meta).studio?.published_at; const pb = meta.studio?.published_at;
  if (pb && pb !== pa) parts.push("Published");
  return parts.length ? parts.join(" · ") : "Saved without changes";
}
