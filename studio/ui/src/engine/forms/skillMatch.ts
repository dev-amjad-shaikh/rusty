import type { ServerSkill } from "../net/client";

/** The skills whose procedure names one of these tools: the ones the
 * library ships for a system, or the ones an agent's tool set makes
 * relevant. A skill that names no tool follows nothing in particular. */
export function skillsForTools(skills: ServerSkill[], tools: Iterable<string>): ServerSkill[] {
  const set = new Set(tools);
  return skills.filter((s) => (s.allowed_tools ?? []).some((t) => set.has(t))).sort((a, b) => a.name.localeCompare(b.name));
}

/** The skills that follow a connector: any tool of theirs is `<id>.<operation>`. */
export function skillsForConnector(skills: ServerSkill[], connectorId: string): ServerSkill[] {
  const prefix = `${connectorId}.`;
  return skills.filter((s) => (s.allowed_tools ?? []).some((t) => t.startsWith(prefix))).sort((a, b) => a.name.localeCompare(b.name));
}
