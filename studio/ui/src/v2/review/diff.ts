// Field-level diff of a draft against its published head (R-A8), grouped by
// the Review sections from handoff 03. Pure: the screen renders whatever this
// returns and never re-derives it. Governance-significant sections —
// Toolsets (approval wrappers), Autonomy, Triggers — carry the GOVERNANCE
// badge the publish confirmation logs.

import type { AgentDraft } from "../draft/agent-draft.gen";

export type DiffSign = "+" | "~" | "-";

export interface DiffRow {
  sign: DiffSign;
  /** Field label, rendered mono, e.g. `model`, `triggers[0].spec`. */
  field: string;
  /** New value (or the only value for +/-). */
  value: string;
  /** Old value for `~` rows, rendered struck through. */
  oldValue?: string;
}

export interface SectionDiff {
  /** Section id, stable for tests and keys. */
  id: string;
  /** Display label in handoff order. */
  label: string;
  rows: DiffRow[];
  /** GOVERNANCE badge: autonomy, approval wrappers, or triggers changed. */
  governance: boolean;
}

const SECTION_ORDER: { id: string; label: string; governance: boolean }[] = [
  { id: "identity", label: "Identity", governance: false },
  { id: "goal", label: "Goal", governance: false },
  { id: "directive", label: "Directive", governance: false },
  { id: "toolsets", label: "Toolsets", governance: true },
  { id: "tool-rules", label: "Tool rules", governance: false },
  { id: "skills", label: "Skills", governance: false },
  { id: "memory", label: "Memory", governance: false },
  { id: "channels", label: "Channels", governance: false },
  { id: "triggers", label: "Triggers", governance: true },
  { id: "autonomy", label: "Autonomy", governance: true },
  { id: "learning", label: "Learning", governance: false },
];

function scalar(rows: DiffRow[], field: string, next: string, base: string | undefined): void {
  if (base === undefined) {
    if (next.trim() !== "") rows.push({ sign: "+", field, value: next });
  } else if (next !== base) {
    if (next.trim() === "") rows.push({ sign: "-", field, value: base });
    else rows.push({ sign: "~", field, value: next, oldValue: base });
  }
}

function listRows(
  rows: DiffRow[],
  field: string,
  next: string[],
  base: string[] | undefined,
  render: (value: string) => string = (value) => value,
): void {
  const old = base ?? [];
  for (const value of next) {
    if (!old.includes(value)) rows.push({ sign: "+", field, value: render(value) });
  }
  for (const value of old) {
    if (!next.includes(value)) rows.push({ sign: "-", field, value: render(value) });
  }
}

/** Render one measure as a single diffable line. */
function measureLine(m: AgentDraft["measures"][number]): string {
  return `${m.name} · ${m.source} · ${m.target} · ${m.window} · ${m.kind}`;
}

function memoryLine(m: AgentDraft["memory"][number]): string {
  return `${m.label} (${m.limit}, ${m.scope}): ${m.description}`;
}

function ruleLine(r: AgentDraft["rules"][number]): string {
  return `${r.tool === "" ? "*" : r.tool}: ${r.rule}`;
}

function triggerLine(t: AgentDraft["triggers"][number]): string {
  return `${t.kind} ${t.spec} → ${t.prompt}`;
}

function sectionRows(id: string, draft: AgentDraft, base: AgentDraft | undefined): DiffRow[] {
  const rows: DiffRow[] = [];
  switch (id) {
    case "identity":
      scalar(rows, "name", draft.name, base?.name);
      scalar(rows, "description", draft.description, base?.description);
      scalar(rows, "model", draft.model, base?.model);
      break;
    case "goal":
      scalar(rows, "goal", draft.goal, base?.goal);
      listRows(rows, "measures", draft.measures.map(measureLine), base?.measures.map(measureLine));
      break;
    case "directive":
      scalar(rows, "stable", draft.stable, base?.stable);
      scalar(rows, "context", draft.context, base?.context);
      break;
    case "toolsets": {
      listRows(rows, "connectors", draft.connectors, base?.connectors);
      listRows(rows, "wrapped", draft.wrapped, base?.wrapped, (id) => `${id} → approval_required(org_admins)`);
      const secretKeys = new Set([...Object.keys(draft.secrets), ...Object.keys(base?.secrets ?? {})]);
      for (const key of [...secretKeys].sort()) {
        scalar(rows, `secrets.${key}`, draft.secrets[key] ?? "", base?.secrets[key]);
      }
      break;
    }
    case "tool-rules":
      listRows(rows, "rules", draft.rules.map(ruleLine), base?.rules.map(ruleLine));
      break;
    case "skills":
      listRows(rows, "skills", draft.skills, base?.skills);
      break;
    case "memory":
      listRows(rows, "memory", draft.memory.map(memoryLine), base?.memory.map(memoryLine));
      break;
    case "channels": {
      const next = draft.channelKind === "" ? "" : `${draft.channelKind} · ${draft.channelTarget}`;
      const prev = base === undefined || base.channelKind === "" ? "" : `${base.channelKind} · ${base.channelTarget}`;
      scalar(rows, "channel", next, base === undefined ? undefined : prev);
      break;
    }
    case "triggers":
      listRows(rows, "triggers", draft.triggers.map(triggerLine), base?.triggers.map(triggerLine));
      break;
    case "autonomy":
      scalar(rows, "autonomy", draft.autonomy, base?.autonomy);
      break;
    case "learning":
      scalar(rows, "reviewFork", draft.reviewFork ? "on" : "off", base === undefined ? undefined : base.reviewFork ? "on" : "off");
      scalar(rows, "cadence", draft.cadence, base?.cadence);
      scalar(rows, "gateSuite", draft.gateSuite, base?.gateSuite);
      break;
  }
  return rows;
}

/**
 * Diff a draft against its base snapshot (`draft.base`, the published head).
 * With no base the draft is new: every filled field appears as a `+` row.
 * Empty sections are omitted; governance is only flagged when the section
 * actually changed.
 */
export function diffDraft(draft: AgentDraft): SectionDiff[] {
  const base = draft.base;
  return SECTION_ORDER.map(({ id, label, governance }) => {
    const rows = sectionRows(id, draft, base);
    return { id, label, rows, governance: governance && rows.length > 0 };
  }).filter((section) => section.rows.length > 0);
}

/** Version pill copy: "new · v1" for a first publish, "v3 → v4" otherwise. */
export function versionLabel(draft: AgentDraft): string {
  const baseVersion = draft.base?.version;
  return baseVersion === undefined ? "new · v1" : `v${baseVersion} → v${baseVersion + 1}`;
}

/** Next version number a publish would append. */
export function nextVersion(draft: AgentDraft): number {
  return (draft.base?.version ?? 0) + 1;
}
