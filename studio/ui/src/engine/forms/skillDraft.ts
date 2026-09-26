// A skill, as a builder writes one without knowing the file format. The
// server owns the rules (`rusty-core::skill`); this composes the SKILL.md
// it parses and pre-checks the two things a person gets wrong most.

export interface SkillDraft {
  name: string;
  description: string;
  license: string;
  evalGate: string;
  tools: string[];
  body: string;
}

export const LICENSES = ["", "Apache-2.0", "MIT", "BSD-3-Clause", "Proprietary"] as const;

/** The skeleton a new skill starts from — the sections a procedure needs. */
export const BODY_SKELETON = `## When to use

Say what situation this skill is for, in the agent's terms.

## Tools you use

## Workflow

1. Read the whole input before acting.
2. Do the work with the tools this skill names.
3. Check the result against what done looks like before answering.

## Done looks like

- The outcome the person asked for is observable, not just described.
`;

/** One step of the workflow: what to do, and the tool it binds to when it
 * calls one. A binding is a real name from the catalog, so the studio can
 * say whether it is connected and the platform can tell which steps a
 * connector's change breaks. */
export interface SkillStep { tool: string; text: string }

/** A procedure, as the editor holds it: the sections a skill needs, plus
 * whatever else the author wrote, kept verbatim. The SKILL.md body is
 * composed from this and parsed back into it, so Source and the structured
 * editor edit the same thing. */
export interface SkillShape {
  when: string;
  /** The tools the procedure uses, each with why — the union of what the
   * steps bind and what the author added. */
  tools: { name: string; why: string }[];
  steps: SkillStep[];
  done: string[];
  /** Sections the shape does not know, kept as written. */
  rest: string;
}

const SECTION = /^\s{0,3}##\s+(.+?)\s*$/;
/** A tool's name, whole: `connector.operation`, `connector@instance.operation`,
 * or a bare built-in like `read_document` — never a path, a query or a
 * `field=value`. */
export const TOOL_NAME = /^(?:[a-z0-9][a-z0-9_-]*(?:@[a-z0-9]+)?\.[a-z0-9][a-z0-9_-]*|[a-z][a-z0-9_]{2,})$/;
const TOOL_SPAN = /`([^`]+)`/g;
const kind = (heading: string): "when" | "tools" | "steps" | "done" | null => {
  const h = heading.trim().toLowerCase();
  if (/^when/.test(h)) return "when";
  if (/^tools?\b/.test(h)) return "tools";
  if (/^(workflow|method|steps|procedure)\b/.test(h)) return "steps";
  if (/^done\b/.test(h)) return "done";
  return null;
};

/** The body's sections by their headings; text before the first heading
 * counts as "when". */
export function parseSkillBody(body: string): SkillShape {
  const shape: SkillShape = { when: "", tools: [], steps: [], done: [], rest: "" };
  const lines = body.replace(/\r\n?/g, "\n").split("\n");
  let current: ReturnType<typeof kind> | "rest" = "when";
  let restHeading = "";
  const buf: Record<string, string[]> = { when: [], tools: [], steps: [], done: [] };
  const rest: string[] = [];
  for (const line of lines) {
    const m = SECTION.exec(line);
    if (m) {
      const k = kind(m[1]);
      if (k) { current = k; continue; }
      current = "rest"; restHeading = line; rest.push("", restHeading); continue;
    }
    if (current === "rest") rest.push(line); else buf[current].push(line);
  }
  shape.when = buf.when.join("\n").trim();
  for (const line of buf.tools) {
    const m = /^\s*[-*•]\s*`?([^`\s]+)`?\s*(?:[—–-]+\s*(.*))?$/.exec(line);
    if (m) shape.tools.push({ name: m[1], why: (m[2] ?? "").trim() });
  }
  for (const line of buf.steps) {
    const m = /^\s*(?:\d+[.)]|[-*•])\s+(.*)$/.exec(line);
    if (!m) continue;
    let text = m[1].trim();
    const spans = [...text.matchAll(TOOL_SPAN)].map((x) => x[1]).filter((name) => TOOL_NAME.test(name));
    const tool = spans[0] ?? "";
    // The composed form "Call `tool` — words" comes back as the words alone.
    if (tool) text = text.replace(new RegExp(`^Call \\\`${tool.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}\\\`\\s*(?:[—–:-]+\\s*)?`), "").trim();
    shape.steps.push({ tool, text });
  }
  for (const line of buf.done) {
    const m = /^\s*[-*•]\s+(.*)$/.exec(line);
    if (m) shape.done.push(m[1].trim());
    else if (line.trim()) shape.done.push(line.trim());
  }
  shape.rest = rest.join("\n").trim();
  return shape;
}

/** The tools a shape uses: what the steps bind, then what the author added. */
export function shapeTools(shape: SkillShape): { name: string; why: string }[] {
  const out: { name: string; why: string }[] = [];
  const seen = new Set<string>();
  for (const step of shape.steps) {
    if (step.tool && !seen.has(step.tool)) { seen.add(step.tool); out.push({ name: step.tool, why: shape.tools.find((t) => t.name === step.tool)?.why ?? "" }); }
  }
  for (const t of shape.tools) if (!seen.has(t.name)) { seen.add(t.name); out.push(t); }
  return out;
}

/** The body a shape composes: the four sections, then the rest verbatim.
 * A step that binds a tool names it in backticks, so the model reads the
 * exact name and the platform can find the binding again. */
export function composeSkillBody(shape: SkillShape): string {
  const out: string[] = [];
  out.push("## When to use", "", shape.when.trim() || "Say what situation this skill is for.", "");
  const tools = shapeTools(shape);
  out.push("## Tools you use", "");
  for (const t of tools) out.push(`- \`${t.name}\`${t.why.trim() ? ` — ${t.why.trim()}` : ""}`);
  if (tools.length) out.push("");
  out.push("## Workflow", "");
  shape.steps.forEach((step, i) => {
    const text = step.text.trim();
    const named = step.tool && text.includes(`\`${step.tool}\``);
    out.push(`${i + 1}. ${step.tool && !named ? `Call \`${step.tool}\`${text ? ` — ${text}` : ""}` : text || "…"}`);
  });
  if (shape.steps.length) out.push("");
  out.push("## Done looks like", "");
  for (const d of shape.done) if (d.trim()) out.push(`- ${d.trim()}`);
  if (shape.rest.trim()) out.push("", shape.rest.trim());
  return out.join("\n").replace(/\n{3,}/g, "\n\n").trim() + "\n";
}

export const emptySkillDraft = (): SkillDraft => ({
  name: "", description: "", license: "", evalGate: "", tools: [], body: BODY_SKELETON,
});

/** Why a name would be refused, or null. Mirrors the server's rule: kebab-case,
 * at most 64 bytes. The server still decides. */
export function skillNameProblem(name: string): string | null {
  const n = name.trim();
  if (!n) return "A skill needs a name.";
  if (n.length > 64) return "At most 64 characters.";
  if (!/^[a-z0-9]+(-[a-z0-9]+)*$/.test(n)) return "Lowercase letters, digits and single hyphens: `triage-and-route`.";
  return null;
}

/** The SKILL.md the server registers. Front matter is `key: value` lines; the
 * tool list is comma-separated, as the parser reads it. */
export function composeSkillMd(d: SkillDraft): string {
  const lines = ["---", `name: ${d.name.trim()}`, `description: ${d.description.trim().replace(/\s+/g, " ")}`];
  if (d.license) lines.push(`license: ${d.license}`);
  if (d.tools.length) lines.push(`allowed-tools: ${d.tools.join(", ")}`);
  if (d.evalGate.trim()) lines.push(`eval-gate: ${d.evalGate.trim()}`);
  lines.push("---", "", d.body.trim(), "");
  return lines.join("\n");
}
