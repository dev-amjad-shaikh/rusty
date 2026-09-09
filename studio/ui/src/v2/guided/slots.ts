// Slot cards (R-A4, handoff 03 "Guided" step 2): the template document
// determines which slots the author fills — Agent name, Goal, Channel kind,
// Channel target, one credential per mounted connector (SecretRef names
// only, with the wire-probe status line), and Model. Start-blank skips the
// slot layer and renders the full AgentDraftForm. Unfilled slots surface as
// slot-kind violations alongside the schema/coherence report (R-A4 AC).

import { AGENT_DRAFT_SCHEMA, specFileFor } from "../draft/catalogs";
import type { Violation } from "../draft/validate";
import type { AgentDraft } from "../draft/agent-draft.gen";

export type SlotControl = "text" | "channel-kind" | "model";

export interface GuidedSlot {
  /** Draft anchor path, shared with the validation report — e.g. `name`, `secrets.slack`. */
  path: string;
  title: string;
  help: string;
  control: SlotControl;
  mono: boolean;
  /** Credential slots show the wire-probe status line. */
  probe: boolean;
}

/** Wire-probe status for a SecretRef (names only, never values). */
export interface SecretProbe {
  status: "ok" | "pending" | "failed";
  detail?: string;
}

function fieldHelp(path: string): string {
  return AGENT_DRAFT_SCHEMA.properties[path]?.description ?? "";
}

/**
 * Derive the slot list from the template document. Order follows the
 * handoff: name, goal, channel kind, channel target, one credential per
 * mounted connector, model. Credential slots exist because the document
 * mounts the connector — add a connector and its slot appears.
 */
export function deriveSlots(document: AgentDraft): GuidedSlot[] {
  return [
    {
      path: "name",
      title: "Agent name",
      help: fieldHelp("name"),
      control: "text",
      mono: false,
      probe: false,
    },
    {
      path: "goal",
      title: "Goal",
      help: fieldHelp("goal"),
      control: "text",
      mono: false,
      probe: false,
    },
    {
      path: "channelKind",
      title: "Channel kind",
      help: fieldHelp("channelKind"),
      control: "channel-kind",
      mono: false,
      probe: false,
    },
    {
      path: "channelTarget",
      title: "Channel target",
      help: fieldHelp("channelTarget"),
      control: "text",
      mono: true,
      probe: false,
    },
    ...document.connectors.map(
      (id): GuidedSlot => ({
        path: `secrets.${id}`,
        title: `${id} credential`,
        help: "SecretRef name only — never a value. rusty:secret:<store>:<key>",
        control: "text",
        mono: true,
        probe: true,
      }),
    ),
    {
      path: "model",
      title: "Model",
      help: fieldHelp("model"),
      control: "model",
      mono: true,
      probe: false,
    },
  ];
}

/** True when the draft currently fills the slot. */
export function slotFilled(draft: AgentDraft, slot: GuidedSlot): boolean {
  if (slot.path.startsWith("secrets.")) {
    const id = slot.path.slice("secrets.".length);
    return (draft.secrets[id] ?? "").trim() !== "";
  }
  if (slot.path === "channelTarget") {
    // Not applicable until a channel kind is chosen; then it must be non-empty.
    return draft.channelKind === "" || draft.channelTarget.trim() !== "";
  }
  const value = (draft as unknown as Record<string, unknown>)[slot.path];
  return typeof value === "string" ? value.trim() !== "" : value != null;
}

/**
 * True when leaving the slot unfilled must surface as a violation. Channel
 * slots bind to the validation table's channel row: a kind is owed once
 * connectors are mounted (a trigger also satisfies it), and a chosen kind
 * owes a target.
 */
export function slotRequired(draft: AgentDraft, slot: GuidedSlot): boolean {
  if (slot.path === "channelKind") {
    return draft.connectors.length > 0 && draft.triggers.length === 0;
  }
  if (slot.path === "channelTarget") {
    return draft.channelKind !== "";
  }
  return true;
}

/** One slot-kind violation per required-but-unfilled slot (R-A4 AC). */
export function slotViolations(draft: AgentDraft, slots: GuidedSlot[]): Violation[] {
  return slots
    .filter((slot) => slotRequired(draft, slot) && !slotFilled(draft, slot))
    .map((slot) => ({
      path: slot.path,
      kind: "slot" as const,
      rule: "guided.slot.unfilled",
      message: `${slot.title} is unfilled.`,
      specFile: specFileFor(slot.path),
    }));
}

/**
 * Merge slot violations into the live report. Paths the shared rule set
 * already flags (name, goal, secrets.*, channel rows) are not double-listed
 * — slot violations add coverage for what only the slot layer knows about,
 * such as the model.
 */
export function withSlotViolations(
  base: Violation[],
  draft: AgentDraft,
  slots: GuidedSlot[],
): Violation[] {
  const covered = new Set(base.map((v) => v.path));
  return [...base, ...slotViolations(draft, slots).filter((v) => !covered.has(v.path))];
}
