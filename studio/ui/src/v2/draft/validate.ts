// The AgentDraft validation report (handoff 04, validation table): one rule
// set, identical for save / publish / import / load (R-A3). Every violation
// is anchored twice — to a form control (path) and to a spec file — so the
// form, the Compose editor, and the Review screen point at the same place.

import {
  AGENT_DRAFT_SCHEMA,
  EMPTY_CATALOGS,
  mountedTools,
  specFileFor,
  type DraftCatalogs,
  type DraftSchema,
} from "./catalogs";
import type { AgentDraft } from "./agent-draft.gen";

export type ViolationKind = "schema" | "coherence" | "slot";

export interface Violation {
  /** Draft control anchor, e.g. `name`, `measures[0].target`, `secrets.slack`. */
  path: string;
  kind: ViolationKind;
  /** Stable rule id, for keyed messages and tests. */
  rule: string;
  message: string;
  /** Spec-file anchor from the 04 layout, e.g. `agent.md`, `directive/stable.md`. */
  specFile: string;
}

/** Message copy per rule id — one keyed catalog, ready to lift into i18n (R-X4). */
export const VIOLATION_MESSAGES: Record<string, string> = {
  "identity.name.required": "Name is required.",
  "directive.stable.non_empty": "The stable tier is load-bearing — write the identity and guidance.",
  "goal.statement.required": "The goal is required — it is what the agent is judged on.",
  "goal.measures.min": "No measures. Add at least one so the agent knows what it is judged on.",
  "goal.measures.name": "A measure needs a name.",
  "goal.measures.target": "A measure needs a target.",
  "autonomy.read_only.conflict": "Read-only autonomy conflicts with a mounted Write/Execute/Egress tool.",
  "toolset.read_only.conflict": "This tool writes, executes, or egresses — unavailable under read-only autonomy.",
  "connectors.secret.required": "This connector needs a SecretRef name.",
  "channel.required": "Connectors are mounted — add a channel or a trigger.",
  "channel.target.required": "A channel kind needs a target.",
  "memory.description.required": "A memory block needs a description — it is load-bearing prompt content.",
  "memory.label.unique": "Memory labels must be unique.",
  "tool_rules.dangling": "This rule names a tool that is not mounted.",
  "tool_rules.text.required": "A rule needs text — it compiles into the prompt and the guard.",
  "triggers.cron.fields": "A cron schedule has 5 fields.",
  "triggers.event.required": "Choose a connector event.",
  "triggers.event.mounted": "The event source must be a mounted connector.",
  "triggers.prompt.required": "A trigger needs a seed prompt.",
  "learning.gate.required": "The promotion gate must name an eval suite.",
  "learning.gate.known": "The promotion gate must name a known eval suite.",
};

/** True when the spec parses as a 5-field cron expression. */
export function isCronSpec(spec: string): boolean {
  const fields = spec.trim().split(/\s+/);
  return fields.length === 5 && fields.every((f) => /^[0-9*,\/-]+$/.test(f));
}

/**
 * Validate a draft against the full rule set. Pure and synchronous — the
 * live hook debounces it behind pause-typing (R-A3).
 */
export function validateDraft(
  draft: AgentDraft,
  catalogs: DraftCatalogs = EMPTY_CATALOGS,
  schema: DraftSchema = AGENT_DRAFT_SCHEMA,
): Violation[] {
  const violations: Violation[] = [];
  const add = (path: string, kind: ViolationKind, rule: string, message?: string) => {
    violations.push({
      path,
      kind,
      rule,
      message: message ?? VIOLATION_MESSAGES[rule] ?? rule,
      specFile: specFileFor(path, schema),
    });
  };

  // identity / directive / goal — schema rules.
  if (!draft.name.trim()) add("name", "schema", "identity.name.required");
  if (!draft.stable.trim()) add("stable", "schema", "directive.stable.non_empty");
  if (!draft.goal.trim()) add("goal", "schema", "goal.statement.required");

  // measures — at least one, each with a name and a target.
  if (draft.measures.length === 0) add("measures", "coherence", "goal.measures.min");
  draft.measures.forEach((measure, i) => {
    if (!measure.name.trim()) add(`measures[${i}].name`, "schema", "goal.measures.name");
    if (!measure.target.trim()) add(`measures[${i}].target`, "schema", "goal.measures.target");
  });

  // autonomy × toolsets — read_only forbids Write/Execute/Egress tools, on
  // the autonomy control and on each offending tool chip.
  const tools = mountedTools(draft, catalogs);
  if (draft.autonomy === "read_only") {
    const offenders = tools.filter((tool) => tool.effect !== "read");
    if (offenders.length > 0) {
      add(
        "autonomy",
        "coherence",
        "autonomy.read_only.conflict",
        `Read-only autonomy conflicts with ${offenders.map((t) => t.id).join(", ")}.`,
      );
      for (const tool of offenders) {
        add(`toolset.${tool.id}`, "coherence", "toolset.read_only.conflict");
      }
    }
  }

  // secrets — one SecretRef name per mounted connector.
  for (const id of draft.connectors) {
    if (!draft.secrets[id]?.trim()) {
      const name = catalogs.connectors.find((c) => c.id === id)?.name ?? id;
      add(`secrets.${id}`, "slot", "connectors.secret.required", `${name} needs a SecretRef name.`);
    }
  }

  // channel — mounted connectors need a channel or a trigger; a channel kind
  // needs a target.
  const hasChannel = draft.channelKind !== "" && draft.channelTarget.trim() !== "";
  if (draft.connectors.length > 0 && !hasChannel && draft.triggers.length === 0) {
    add("channelKind", "slot", "channel.required");
  }
  if (draft.channelKind !== "" && !draft.channelTarget.trim()) {
    add("channelTarget", "slot", "channel.target.required");
  }

  // memory — descriptions are load-bearing prompt content; labels unique.
  const seenLabels = new Map<string, number>();
  draft.memory.forEach((block, i) => {
    if (!block.description.trim()) {
      add(`memory[${i}].description`, "coherence", "memory.description.required");
    }
    const label = block.label.trim();
    if (label) {
      const first = seenLabels.get(label);
      if (first === undefined) seenLabels.set(label, i);
      else add(`memory[${i}].label`, "coherence", "memory.label.unique");
    }
  });

  // tool rules — the referenced tool must be mounted ('' = all); text required.
  const toolIds = new Set(tools.map((tool) => tool.id));
  draft.rules.forEach((rule, i) => {
    if (rule.tool !== "" && !toolIds.has(rule.tool)) {
      add(`rules[${i}].tool`, "coherence", "tool_rules.dangling", `${rule.tool} is not mounted.`);
    }
    if (!rule.rule.trim()) add(`rules[${i}].rule`, "schema", "tool_rules.text.required");
  });

  // triggers — cron is 5 fields; an event is chosen and comes from a mounted
  // connector; every trigger carries a seed prompt.
  draft.triggers.forEach((trigger, i) => {
    if (trigger.kind === "cron") {
      if (!isCronSpec(trigger.spec)) add(`triggers[${i}].spec`, "schema", "triggers.cron.fields");
    } else if (!trigger.spec.trim()) {
      add(`triggers[${i}].spec`, "schema", "triggers.event.required");
    } else {
      const source = trigger.spec.split(".", 1)[0];
      if (!draft.connectors.includes(source)) {
        add(
          `triggers[${i}].spec`,
          "coherence",
          "triggers.event.mounted",
          `${source} is not a mounted connector.`,
        );
      }
    }
    if (!trigger.prompt.trim()) add(`triggers[${i}].prompt`, "schema", "triggers.prompt.required");
  });

  // learning — the promotion gate names a known eval suite.
  if (!draft.gateSuite.trim()) {
    add("gateSuite", "coherence", "learning.gate.required");
  } else if (catalogs.evalSuites.length > 0 && !catalogs.evalSuites.includes(draft.gateSuite)) {
    add("gateSuite", "coherence", "learning.gate.known", `${draft.gateSuite} is not a known eval suite.`);
  }

  return violations;
}
