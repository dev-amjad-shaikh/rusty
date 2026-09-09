// The read-only assembled-prompt projection (handoff 04): stable, context,
// and volatile tiers, with the byte count the Review card displays. Pure —
// the same text the Compose editor shows read-only, frozen per session.

import type { AgentDraft } from "../draft/agent-draft.gen";
import { EMPTY_CATALOGS, type DraftCatalogs } from "../draft/catalogs";

export interface PromptTiers {
  stable: string;
  context: string;
  volatile: string;
}

export interface AssembledPrompt extends PromptTiers {
  /** UTF-8 byte count of the full assembled prompt. */
  bytes: number;
  /** The full text Copy puts on the clipboard: tiers joined by a blank line. */
  text: string;
}

/** Stable tier: goal, measures, the stable text, then the compiled tool rules. */
export function stableTier(draft: AgentDraft): string {
  const parts: string[] = [`## Goal\n${draft.goal.trim()}`];
  if (draft.measures.length > 0) {
    const lines = draft.measures
      .map((m) => `${m.name} ${m.target} (${m.window})`)
      .join("; ");
    parts.push(`You are measured on: ${lines}`);
  }
  parts.push(draft.stable.trim());
  if (draft.rules.length > 0) {
    const lines = draft.rules
      .map((rule) => `- ${rule.tool === "" ? "*" : rule.tool}: ${rule.rule}`)
      .join("\n");
    parts.push(`## Tool rules\n${lines}`);
  }
  return parts.filter((part) => part !== "").join("\n\n");
}

/** Volatile tier: skills with descriptions, memory blocks, and the Now line. */
export function volatileTier(
  draft: AgentDraft,
  catalogs: DraftCatalogs,
  now: string,
): string {
  const parts: string[] = [];
  if (draft.skills.length > 0) {
    const lines = draft.skills
      .map((id) => {
        const description = catalogs.skills.find((skill) => skill.id === id)?.description ?? "";
        return description ? `- ${id}: ${description}` : `- ${id}`;
      })
      .join("\n");
    parts.push(`## Skills\n${lines}`);
  }
  if (draft.memory.length > 0) {
    const lines = draft.memory
      .map((block) => `- ${block.label} (${block.limit}): ${block.description}`)
      .join("\n");
    parts.push(`## Memory\n${lines}`);
  }
  const channel = draft.channelKind === "" ? "unbound" : `${draft.channelKind} · ${draft.channelTarget}`;
  parts.push(`## Now\n${now} · ${channel}`);
  return parts.join("\n\n");
}

/** Assemble all three tiers and count UTF-8 bytes of the joined text. */
export function assemblePrompt(
  draft: AgentDraft,
  catalogs: DraftCatalogs = EMPTY_CATALOGS,
  now: string = new Date().toISOString(),
): AssembledPrompt {
  const tiers: PromptTiers = {
    stable: stableTier(draft),
    context: draft.context.trim(),
    volatile: volatileTier(draft, catalogs, now),
  };
  const text = [tiers.stable, tiers.context, tiers.volatile]
    .filter((tier) => tier !== "")
    .join("\n\n");
  return { ...tiers, text, bytes: new TextEncoder().encode(text).byteLength };
}
