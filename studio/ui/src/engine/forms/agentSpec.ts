// The specification the Composer proposes and a person approves. One shape
// on both sides: the Composer writes it as a fenced `agent` block, the Build
// surface renders it as the agent, and the charter the server stores is
// composed from it the same way whether a person clicks Create or tells the
// Composer to.

export interface ToolUse {
  name: string;
  /** When to use it, in the Composer's words. */
  when: string;
}

export interface AgentSpec {
  name: string;
  purpose: string;
  role: string;
  /** `proposed` (review it), `created` (exists), `blocked` (needs things first). */
  status: "proposed" | "created" | "blocked";
  assistant_id: string;
  instructions: string[];
  tools: ToolUse[];
  skills: string[];
  constraints: string[];
  output: string;
  done_when: string[];
  /** What the Composer says must be added first, each with where. */
  needs: string[];
  /** When it runs on its own — "every 2 hours", "every day", a 5-field cron
   * expression (UTC) — or "" when a person always starts it. */
  schedule: string;
  /** What it is told each time it runs on its own. */
  standing_message: string;
  /** The task-queue pool the agent works, if any. */
  pool: string;
  /** The most one run may spend — "20000 tokens", "$0.05", "20000 tokens,
   * $0.05" — or "" for no bound. */
  budget: string;
}

/** A budget in words as the server takes it, or null when the words name
 * no bound. "none" is null. */
export function budgetOf(words: string): { max_tokens?: number; max_cost_usd?: number } | null {
  const text = words.trim().toLowerCase();
  if (!text || /^`?(none|no|unlimited|-)`?\.?$/.test(text)) return null;
  const out: { max_tokens?: number; max_cost_usd?: number } = {};
  const tokens = text.match(/([\d][\d,]*(?:\.\d+)?)\s*(k|m)?\s*tokens?/);
  if (tokens) {
    const n = parseFloat(tokens[1].replace(/,/g, "")) * (tokens[2] === "k" ? 1000 : tokens[2] === "m" ? 1_000_000 : 1);
    if (n > 0) out.max_tokens = Math.round(n);
  }
  const usd = text.match(/\$\s*([\d][\d,]*(?:\.\d+)?)|([\d][\d,]*(?:\.\d+)?)\s*(?:usd|dollars?)/);
  if (usd) {
    const n = parseFloat((usd[1] ?? usd[2]).replace(/,/g, ""));
    if (n > 0) out.max_cost_usd = n;
  }
  return out.max_tokens || out.max_cost_usd ? out : null;
}

/** A budget back in words. */
export function budgetWords(b: { max_tokens?: number | string; max_cost_usd?: number | string } | undefined | null): string {
  if (!b) return "";
  const parts: string[] = [];
  const tokens = Number(b.max_tokens);
  if (tokens > 0) parts.push(`${tokens.toLocaleString()} tokens`);
  const usd = Number(String(b.max_cost_usd ?? "").replace(/^\$/, ""));
  if (usd > 0) parts.push(`$${usd}`);
  return parts.join(", ");
}

export const emptySpec = (): AgentSpec => ({
  name: "", purpose: "", role: "", status: "proposed", assistant_id: "",
  instructions: [], tools: [], skills: [], constraints: [], output: "", done_when: [], needs: [],
  schedule: "", standing_message: "", budget: "", pool: "",
});

const SCALARS = new Set(["name", "purpose", "role", "status", "assistant_id", "output", "schedule", "standing_message", "budget", "pool"]);

/** A cadence as the server takes it, or null when the words are not one.
 * "every 30 minutes" / "every 2 hours" / "every day" / "hourly" / "daily" /
 * "every morning" (09:00 UTC) / a 5-field cron expression. */
export function cadenceOf(text: string): { interval_secs?: number; cron_expr?: string } | null {
  const t = text.trim().toLowerCase().replace(/^every\s+/, "");
  if (!t || t === "none" || t === "never") return null;
  if (/^(\S+\s+){4}\S+$/.test(t) && /^[\d*\/,\-\s]+$/.test(t)) return { cron_expr: t };
  if (t === "morning" || t === "day at 9" || t === "day at 09:00") return { cron_expr: "0 9 * * *" };
  if (t === "weekday morning" || t === "weekday") return { cron_expr: "0 9 * * 1-5" };
  if (t === "hourly" || t === "hour") return { interval_secs: 3600 };
  if (t === "daily" || t === "day" || t === "night" || t === "nightly") return { interval_secs: 86400 };
  if (t === "minute") return { interval_secs: 60 };
  if (t === "week" || t === "weekly") return { interval_secs: 7 * 86400 };
  const m = t.match(/^(\d+)\s*(minute|min|hour|hr|day|week)s?$/);
  if (!m) return null;
  const n = Number(m[1]);
  if (!Number.isFinite(n) || n < 1) return null;
  const unit = m[2].startsWith("min") ? 60 : m[2].startsWith("h") ? 3600 : m[2] === "day" ? 86400 : 7 * 86400;
  return { interval_secs: n * unit };
}

/** A cadence in words, from what the server holds. */
export function cadenceWords(c: { interval_secs?: number; cron_expr?: string }): string {
  if (c.cron_expr) {
    // The common shapes in words: daily, on weekdays, on one day of the week; anything else as written.
    const m = /^(\d{1,2}) (\d{1,2}) \* \* (\*|[0-6](?:-[0-6])?|[0-6](?:,[0-6])+)$/.exec(c.cron_expr.trim());
    if (m) {
      const at = `${m[2].padStart(2, "0")}:${m[1].padStart(2, "0")} (UTC)`;
      const DAYS = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
      if (m[3] === "*") return `every day at ${at}`;
      if (m[3] === "1-5") return `every weekday at ${at}`;
      if (/^[0-6]$/.test(m[3])) return `every ${DAYS[Number(m[3])]} at ${at}`;
    }
    return `on the schedule ${c.cron_expr} (UTC)`;
  }
  const secs = c.interval_secs ?? 0;
  for (const [word, size] of [["week", 7 * 86400], ["day", 86400], ["hour", 3600], ["minute", 60]] as [string, number][]) {
    if (secs % size === 0) { const n = secs / size; return n === 1 ? `every ${word}` : `every ${n} ${word}s`; }
  }
  return `every ${secs} seconds`;
}
const LISTS = new Set(["instructions", "tools", "skills", "constraints", "done_when", "needs"]);

/** The last fenced ```agent block in a text, or null. */
export function findAgentBlock(text: string): string | null {
  const re = /```agent[^\n]*\n([\s\S]*?)```/g;
  let last: string | null = null;
  for (const m of text.matchAll(re)) last = m[1];
  return last;
}

/** Parse the block's line format: `key: value` scalars, `key:` followed by
 * `- item` lines for lists. Unknown keys are ignored; a missing key is
 * empty. Never throws — the Composer's block is text from a model. */
export function parseAgentSpec(block: string): AgentSpec {
  const spec = emptySpec();
  let list: keyof AgentSpec | null = null;
  for (const raw of block.split("\n")) {
    const line = raw.replace(/\s+$/, "");
    if (!line.trim()) continue;
    const item = line.match(/^\s*[-*•]\s+(.*)$/);
    if (item && list) {
      const value = item[1].trim();
      // "- none" under a list is the Composer saying the list is empty.
      if (!value || /^`?(none|n\/a|nothing|-)`?\.?$/i.test(value)) continue;
      if (list === "tools") {
        const m = value.match(/^`?([A-Za-z0-9_.:@-]+)`?\s*(?:[—–-]|:)?\s*(.*)$/);
        spec.tools.push(m ? { name: m[1], when: m[2].trim() } : { name: value, when: "" });
      } else if (list === "skills") {
        // A skill line may carry a note like a tool line does; the name is
        // what precedes it.
        const m = value.match(/^`?([A-Za-z0-9_.:@-]+)`?\s*(?:[—–-]|:)?\s*(.*)$/);
        (spec[list] as string[]).push(m ? m[1] : value.replace(/^`(.*)`$/, "$1"));
      } else {
        (spec[list] as string[]).push(value.replace(/^`(.*)`$/, "$1"));
      }
      continue;
    }
    const kv = line.match(/^([a-z_]+):\s*(.*)$/);
    if (!kv) { list = null; continue; }
    const [, key, value] = kv;
    if (LISTS.has(key)) { list = key as keyof AgentSpec; continue; }
    list = null;
    if (SCALARS.has(key)) {
      const v = value.trim().replace(/^["']|["']$/g, "");
      if (key === "status") spec.status = v === "created" ? "created" : v === "blocked" ? "blocked" : "proposed";
      else (spec as unknown as Record<string, string>)[key] = v;
    }
  }
  return spec;
}

/** The charter the server stores. The Composer composes the same shape when
 * told to create directly, so a person's Create and the Composer's agree. */
export function composeCharter(spec: AgentSpec): string {
  const parts: string[] = [];
  if (spec.role.trim()) parts.push(spec.role.trim());
  else if (spec.name.trim() || spec.purpose.trim()) parts.push(`You are ${spec.name.trim() || "this agent"}. ${spec.purpose.trim()}`.trim());
  const list = (items: string[]) => items.map((i) => `- ${i}`).join("\n");
  if (spec.instructions.length) parts.push(`What you must do:\n${list(spec.instructions)}`);
  if (spec.tools.length) parts.push(`Tools and when to use them:\n${list(spec.tools.map((t) => (t.when ? `${t.name} — ${t.when}` : t.name)))}`);
  if (spec.skills.length) parts.push(`Skills you follow:\n${list(spec.skills)}`);
  if (spec.constraints.length) parts.push(`What you must never do:\n${list(spec.constraints)}`);
  if (spec.output.trim()) parts.push(`How you answer:\n${spec.output.trim()}`);
  if (spec.done_when.length) parts.push(`Done when:\n${list(spec.done_when)}`);
  return parts.join("\n\n");
}

/** Why the spec cannot be created yet. */
export function specProblems(spec: AgentSpec, offered: Set<string> | null): string[] {
  const problems: string[] = [];
  if (!spec.name.trim()) problems.push("It needs a name.");
  if (!spec.role.trim() && !spec.purpose.trim()) problems.push("Say what it is or what it is for.");
  if (!spec.instructions.length) problems.push("It needs at least one instruction.");
  if (offered) {
    const missing = spec.tools.filter((t) => !offered.has(t.name)).map((t) => t.name);
    if (missing.length) problems.push(`Not on the platform: ${missing.join(", ")}.`);
  }
  if (hasSchedule(spec)) {
    if (!cadenceOf(spec.schedule)) problems.push(`"${spec.schedule.trim()}" is not a cadence — say "every 2 hours", "every day", or a 5-field cron expression.`);
    if (!spec.standing_message.trim()) problems.push("An agent that runs on its own needs to be told what to do each time.");
  }
  return problems;
}

/** Whether the spec asks for the agent to run on its own. */
export const hasSchedule = (spec: AgentSpec): boolean => {
  const t = spec.schedule.trim().toLowerCase();
  return t !== "" && t !== "none" && t !== "never";
};

/** The block as text — what the panel hands back when a person edits it. */
export function formatAgentSpec(spec: AgentSpec): string {
  const lines: string[] = [];
  const scalar = (k: string, v: string) => { if (v.trim()) lines.push(`${k}: ${v.trim()}`); };
  scalar("name", spec.name); scalar("purpose", spec.purpose); scalar("role", spec.role);
  lines.push(`status: ${spec.status}`);
  scalar("assistant_id", spec.assistant_id);
  const list = (k: string, items: string[]) => { if (items.length) { lines.push(`${k}:`); for (const i of items) lines.push(`- ${i}`); } };
  list("instructions", spec.instructions);
  list("tools", spec.tools.map((t) => (t.when ? `${t.name} — ${t.when}` : t.name)));
  list("skills", spec.skills);
  list("constraints", spec.constraints);
  scalar("output", spec.output);
  list("done_when", spec.done_when);
  list("needs", spec.needs);
  scalar("schedule", spec.schedule);
  scalar("standing_message", spec.standing_message);
  scalar("budget", spec.budget);
  scalar("pool", spec.pool);
  return lines.join("\n");
}

/** Words that begin sentences and mean nothing on their own; a capital there is grammar, not a name. */
const NOT_A_NAME = new Set(["A", "An", "The", "It", "Its", "If", "For", "When", "Someone", "Our", "Each", "Every", "Never", "Always", "Use", "Read", "Answer", "Call", "Ask", "Given", "Then", "This", "That", "These", "Those", "One", "No", "Not", "And", "But", "Or", "So", "In", "On", "At", "To", "Of", "By", "With", "From", "Before", "After", "Only", "Also", "Say", "Report", "Return", "Check", "Search", "Find", "List", "Tell", "Give", "Keep", "Make", "Do", "Does", "Is", "Are", "Was", "Were", "Be", "Has", "Have", "Had", "Will", "Would", "Should", "Can", "May", "Must", "You", "Your", "We", "They", "He", "She", "I", "My", "Me", "Us", "Them", "Their", "What", "Which", "Who", "Where", "Why", "How", "Yes"]);

/**
 * The facts a request states that the proposed charter does not carry:
 * numbers, ids, and names, with the sentences that hold them. A model
 * summarises; a charter without the coordinates, the room list or the
 * desk map makes an agent that says it does not know.
 */
/** The parameters that make a tool read whatever it is told. */
const WHATEVER_PARAMS = new Set(["table", "object", "sobject", "table_name", "entity"]);
const NOT_A_TABLE = new Set(["the", "and", "for", "from", "with", "that", "name", "names", "each", "this", "its", "api", "data"]);

/** Whether a charter names a table: `table incident`, `table: ts_query`, `object Opportunity`. */
export function namesATable(text: string): boolean {
  for (const m of text.toLowerCase().matchAll(/\b(table|object)\b[\s:=`"']*([a-z][a-z0-9_]{2,})/g)) {
    if (!NOT_A_TABLE.has(m[2])) return true;
  }
  return false;
}

/**
 * The spec's tools that take a table or object parameter while the charter
 * names none: each such agent guesses which table to read. `required` maps a
 * tool name to its required parameter names, from the server's catalog.
 */
export function tablesNotNamed(spec: AgentSpec, required: Map<string, string[]>): { tool: string; param: string }[] {
  const charter = [spec.role, ...spec.instructions, ...spec.constraints, ...spec.tools.map((t) => t.when ?? "")].join("\n");
  if (namesATable(charter)) return [];
  const out: { tool: string; param: string }[] = [];
  for (const t of spec.tools) {
    const param = (required.get(t.name) ?? []).find((p) => WHATEVER_PARAMS.has(p));
    if (param) out.push({ tool: t.name, param });
  }
  return out;
}

export function factsNotCarried(request: string, spec: AgentSpec): { facts: string[]; sentences: string[] } {
  const carried = [spec.role, ...spec.instructions, ...spec.constraints, spec.output, ...spec.done_when, ...spec.tools.map((t) => `${t.name} ${t.when ?? ""}`)].join("\n").toLowerCase();
  const facts: string[] = [];
  const seen = new Set<string>();
  const consider = (fact: string) => { const key = fact.toLowerCase(); if (!seen.has(key) && !carried.includes(key)) { seen.add(key); facts.push(fact); } };
  for (const m of request.matchAll(/-?\d+\.\d+|\b\d{2,}\b/g)) consider(m[0]);
  for (const m of request.matchAll(/\b[A-Z]{2,6}\d{3,}\b/g)) consider(m[0]);
  for (const m of request.matchAll(/\b[a-z0-9]+(?:-[a-z0-9]+)+\b/g)) consider(m[0]);
  // A capitalised word is a name unless it only opens a sentence: "Dont
  // you need…" states no fact called Dont.
  for (const m of request.matchAll(/\b[A-Z][a-zA-Z]{2,}\b/g)) {
    const before = request.slice(0, m.index).trimEnd();
    const opensSentence = before === "" || /[.!?;:]$/.test(before);
    if (!opensSentence && !NOT_A_NAME.has(m[0])) consider(m[0]);
  }
  const sentences = request.split(/(?<=[.;:])\s+|\n+/).map((x) => x.trim()).filter((x) => x.length > 0 && facts.some((f) => x.toLowerCase().includes(f.toLowerCase())));
  return { facts, sentences };
}
