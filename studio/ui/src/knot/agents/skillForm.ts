/**
 * A skill as a builder fills it in: what it produces, the tools it uses,
 * its steps, what it must never do and what it gives back. The platform
 * stores a skill as a SKILL.md body; this is the one mapping between the
 * two, so the form and the document view always agree.
 */

export interface SkillForm {
  purpose: string;
  tools: { name: string; when: string }[];
  steps: string[];
  never: string[];
  done: string;
}

/** A new skill picks no tools for the builder — a skill uses a few — unless the agent has only the one. */
export const emptySkillForm = (tools: string[] = []): SkillForm => ({
  purpose: "",
  tools: tools.length === 1 ? [{ name: tools[0], when: "" }] : [],
  steps: [""],
  never: [],
  done: "",
});

const clean = (lines: string[]) => lines.map((l) => l.trim()).filter(Boolean);

/** The SKILL.md body a form writes. Tools ride in backticks: that is how the platform links them. */
export function writeSkillBody(f: SkillForm): string {
  const parts: string[] = [];
  if (f.purpose.trim()) parts.push(f.purpose.trim());
  if (f.tools.length) parts.push(`## Tools you use\n${f.tools.map((t) => `- \`${t.name}\`${t.when.trim() ? ` — ${t.when.trim()}` : ""}`).join("\n")}`);
  const steps = clean(f.steps);
  if (steps.length) parts.push(`## Steps\n${steps.map((s, n) => `${n + 1}. ${s}`).join("\n")}`);
  const never = clean(f.never);
  if (never.length) parts.push(`## Never\n${never.map((s) => `- ${s}`).join("\n")}`);
  if (f.done.trim()) parts.push(`## When done\n${f.done.trim()}`);
  return parts.join("\n\n");
}

const SECTIONS = ["Tools you use", "Steps", "Never", "When done"] as const;

/**
 * A body read back into the form — only when it is one the form could have
 * written. A skill written as a free document (an imported library skill,
 * one edited by hand) returns null and opens as a document instead, so the
 * form never drops a word of it.
 */
export function skillFormOf(body: string): SkillForm | null {
  const text = body.replace(/\r\n/g, "\n").trim();
  const heads = [...text.matchAll(/^## (.+)$/gm)];
  if (heads.some((h) => !(SECTIONS as readonly string[]).includes(h[1].trim()))) return null;
  if (/^#{1,6} /m.test(text.replace(/^## .+$/gm, ""))) return null;
  const first = heads[0]?.index ?? text.length;
  const section = (name: string) => {
    const h = heads.find((x) => x[1].trim() === name);
    if (!h) return null;
    const start = (h.index ?? 0) + h[0].length;
    const next = heads.find((x) => (x.index ?? 0) > (h.index ?? 0));
    return text.slice(start, next?.index ?? text.length).trim();
  };
  const items = (s: string | null, marker: RegExp) => {
    if (!s) return [];
    const lines = s.split("\n").map((l) => l.trim()).filter(Boolean);
    return lines.every((l) => marker.test(l)) ? lines.map((l) => l.replace(marker, "").trim()) : null;
  };
  const toolLines = items(section("Tools you use"), /^-\s+/);
  const steps = items(section("Steps"), /^\d+\.\s+/);
  const never = items(section("Never"), /^-\s+/);
  if (!toolLines || !steps || !never) return null;
  const tools: SkillForm["tools"] = [];
  for (const l of toolLines) {
    const m = /^`([^`]+)`(?:\s+—\s+(.*))?$/.exec(l);
    if (!m) return null;
    tools.push({ name: m[1], when: m[2] ?? "" });
  }
  const done = section("When done") ?? "";
  const purpose = text.slice(0, first).trim();
  if (done.includes("\n") && /^[-\d]/m.test(done)) return null;
  const form: SkillForm = { purpose, tools, steps: steps.length ? steps : [""], never, done };
  // Only a body the form writes back unchanged is the form's.
  return writeSkillBody(form) === text ? form : null;
}
