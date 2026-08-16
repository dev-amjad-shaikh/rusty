import { z } from "zod";
import type { RunEvent } from "../contracts";

// Provider-neutral render intents: the client-side mirror of
// `rusty-core/src/render_intent.rs`.
//
// The server serves journaled tool evidence (kind, input, output, status),
// not presentation. The render-intent union is derived from that evidence by
// pure rules, and this module is the deliberate TypeScript mirror of the
// Rust derivation so Studio renders rich tool cards with zero per-tool UI
// code — and a replayed run renders byte-identical cards, because replay
// serves the same journaled result the live run recorded.
//
// The parity contract is `rusty-core/tests/golden/render_intents.json`:
// the same `(tool, arguments, result)` triple in, the same serialized
// intent out on both sides. Any change to the derivation rules must move
// both implementations and the golden file in one commit
// (`UPDATE_GOLDEN=1 cargo test -p rusty-agent-runtime --test render_intents`).
//
// Known text-level divergences the fixtures deliberately avoid:
// - Rust `serde_json` prints integral floats as `3.0`; JS prints `3`.
// - Rust serializes JSON objects with sorted keys (BTreeMap); JS preserves
//   insertion order. Both only surface inside compact-JSON table cells and
//   generic summaries, never in structured fields.
// - Rust counts Unicode scalar values when clamping; zod counts UTF-16 code
//   units. The derivation clamps by code points, matching Rust.

export const TRUNCATION_MARKER = "…[truncated]";
export const MAX_INTENT_EXCERPT_CHARS = 2_048;
export const MAX_INTENT_LABEL_CHARS = 160;
export const MAX_INTENT_SUMMARY_CHARS = 512;
export const MAX_INTENT_SEARCH_HITS = 20;
export const MAX_INTENT_TABLE_ROWS = 50;
export const MAX_INTENT_TABLE_COLUMNS = 12;

const excerpt = z.string().max(MAX_INTENT_EXCERPT_CHARS + TRUNCATION_MARKER.length);
const label = z.string().max(MAX_INTENT_LABEL_CHARS + TRUNCATION_MARKER.length);
const summary = z.string().max(MAX_INTENT_SUMMARY_CHARS + TRUNCATION_MARKER.length);

export const searchHitViewSchema = z.object({
  label,
  reference: label.nullable(),
  excerpt,
  score: z.number().int().nonnegative().nullable(),
}).strict();
export type SearchHitView = z.infer<typeof searchHitViewSchema>;

export const renderIntentSchema = z.discriminatedUnion("kind", [
  z.object({
    kind: z.literal("generic"),
    tool: label,
    reason: z.string().min(1),
    summary,
  }).strict(),
  z.object({
    kind: z.literal("terminal"),
    command: label,
    cwd: label.nullable(),
    exit_code: z.number().int().nullable(),
    timed_out: z.boolean(),
    truncated: z.boolean(),
    stdout: excerpt,
    stderr: excerpt,
  }).strict(),
  z.object({
    kind: z.literal("diff"),
    path: label,
    before: excerpt,
    after: excerpt,
    truncated: z.boolean(),
  }).strict(),
  z.object({
    kind: z.literal("search"),
    query: label,
    hits: z.array(searchHitViewSchema).max(MAX_INTENT_SEARCH_HITS),
    truncated: z.boolean(),
  }).strict(),
  z.object({
    kind: z.literal("read"),
    path: label,
    format: label.nullable(),
    excerpt,
    truncated: z.boolean(),
  }).strict(),
  z.object({
    kind: z.literal("table"),
    columns: z.array(label).max(MAX_INTENT_TABLE_COLUMNS),
    rows: z.array(z.array(label).max(MAX_INTENT_TABLE_COLUMNS)).max(MAX_INTENT_TABLE_ROWS),
    truncated: z.boolean(),
  }).strict(),
  z.object({
    kind: z.literal("link"),
    url: label,
    title: label.nullable(),
  }).strict(),
  z.object({
    kind: z.literal("web"),
    url: label.nullable(),
    title: label.nullable(),
    excerpt,
    truncated: z.boolean(),
  }).strict(),
]);
export type RenderIntent = z.infer<typeof renderIntentSchema>;

type Record_ = Record<string, unknown>;

function isRecord(value: unknown): value is Record_ {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function field(value: unknown, name: string): unknown {
  return isRecord(value) ? value[name] : undefined;
}

function asString(value: unknown): string | undefined {
  return typeof value === "string" ? value : undefined;
}

function asBool(value: unknown): boolean | undefined {
  return typeof value === "boolean" ? value : undefined;
}

function asI32(value: unknown): number | null {
  return typeof value === "number" && Number.isInteger(value) && value >= -2_147_483_648 && value <= 2_147_483_647
    ? value
    : null;
}

function asU64(value: unknown): number | null {
  return typeof value === "number" && Number.isInteger(value) && value >= 0 && value <= Number.MAX_SAFE_INTEGER
    ? value
    : null;
}

function isControl(char: string): boolean {
  return /[\p{Cc}]/u.test(char);
}

function clampChars(text: string, maxChars: number): [string, boolean] {
  const chars = [...text];
  if (chars.length <= maxChars) return [text, false];
  const budget = Math.max(0, maxChars - [...TRUNCATION_MARKER].length);
  return [chars.slice(0, budget).join("") + TRUNCATION_MARKER, true];
}

// Multi-line fields keep newlines and tabs; every other control character
// flattens to a space — a journaled payload must not smuggle terminal
// escapes into a card.
function cleanBlock(text: string, maxChars: number): [string, boolean] {
  const cleaned = [...text].map((char) => (isControl(char) && char !== "\n" && char !== "\t" ? " " : char)).join("");
  return clampChars(cleaned, maxChars);
}

function cleanLine(text: string, maxChars: number): string {
  const cleaned = [...text].map((char) => (isControl(char) ? " " : char)).join("");
  return clampChars(cleaned, maxChars)[0];
}

function blockField(result: unknown, name: string): [string, boolean] {
  const text = asString(field(result, name));
  return text === undefined ? ["", false] : cleanBlock(text, MAX_INTENT_EXCERPT_CHARS);
}

// One table cell: strings as themselves, everything else as its compact
// JSON text — matching `cell_text` on the Rust side.
function cellText(value: unknown): string {
  return typeof value === "string" ? value : JSON.stringify(value) ?? "";
}

function generic(tool: string, reason: string, result: unknown): RenderIntent {
  const raw = result === null || result === undefined ? "" : typeof result === "string" ? result : JSON.stringify(result) ?? "";
  return {
    kind: "generic",
    tool: cleanLine(tool, MAX_INTENT_LABEL_CHARS),
    reason,
    summary: cleanLine(raw, MAX_INTENT_SUMMARY_CHARS),
  };
}

function tableIntent(columns: string[], rows: string[][], truncated: boolean): RenderIntent {
  const clampedColumns = columns.slice(0, MAX_INTENT_TABLE_COLUMNS).map((column) => cleanLine(column, MAX_INTENT_LABEL_CHARS));
  const clampedRows = rows.slice(0, MAX_INTENT_TABLE_ROWS).map((row) => row.map((cell) => cleanLine(cell, MAX_INTENT_LABEL_CHARS)));
  return {
    kind: "table",
    columns: clampedColumns,
    rows: clampedRows,
    truncated: truncated || rows.length > MAX_INTENT_TABLE_ROWS || columns.length > MAX_INTENT_TABLE_COLUMNS,
  };
}

function searchIntent(query: string, hits: SearchHitView[]): RenderIntent {
  return {
    kind: "search",
    query: cleanLine(query, MAX_INTENT_LABEL_CHARS),
    hits: hits.slice(0, MAX_INTENT_SEARCH_HITS),
    truncated: hits.length > MAX_INTENT_SEARCH_HITS,
  };
}

function terminalIntent(args: unknown, result: unknown): RenderIntent | null {
  let command: string;
  const shell = asString(field(args, "command"));
  if (shell !== undefined) {
    command = shell;
  } else {
    const program = asString(field(args, "program"));
    if (program === undefined) return null;
    const entries = field(args, "args");
    const parts = Array.isArray(entries) ? entries.map((entry) => asString(entry) ?? "") : [];
    command = [program, ...parts].join(" ");
  }
  const [stdout, outClamped] = blockField(result, "stdout");
  const [stderr, errClamped] = blockField(result, "stderr");
  const cwd = asString(field(result, "cwd"));
  return {
    kind: "terminal",
    command: cleanLine(command, MAX_INTENT_LABEL_CHARS),
    cwd: cwd === undefined ? null : cleanLine(cwd, MAX_INTENT_LABEL_CHARS),
    exit_code: asI32(field(result, "exit_code")),
    timed_out: asBool(field(result, "timed_out")) ?? false,
    truncated: (asBool(field(result, "truncated")) ?? false) || outClamped || errClamped,
    stdout,
    stderr,
  };
}

function knowledgeSearchIntent(result: unknown): RenderIntent | null {
  const query = asString(field(result, "query"));
  const hits = field(result, "results");
  if (query === undefined || !Array.isArray(hits)) return null;
  return searchIntent(query, hits.map((hit) => {
    const reference = asString(field(hit, "id"));
    return {
      label: cleanLine(asString(field(hit, "title")) ?? "", MAX_INTENT_LABEL_CHARS),
      reference: reference === undefined ? null : cleanLine(reference, MAX_INTENT_LABEL_CHARS),
      excerpt: cleanBlock(asString(field(hit, "excerpt")) ?? "", MAX_INTENT_EXCERPT_CHARS)[0],
      score: asU64(field(hit, "score")),
    };
  }));
}

function sessionSearchIntent(args: unknown, result: unknown): RenderIntent | null {
  const query = asString(field(args, "query"));
  const hits = field(result, "results");
  if (query === undefined || !Array.isArray(hits)) return null;
  return searchIntent(query, hits.map((hit) => {
    const reference = asString(field(hit, "run_id"));
    return {
      label: cleanLine(asString(field(hit, "event_id")) ?? "", MAX_INTENT_LABEL_CHARS),
      reference: reference === undefined ? null : cleanLine(reference, MAX_INTENT_LABEL_CHARS),
      excerpt: cleanBlock(asString(field(hit, "excerpt")) ?? "", MAX_INTENT_EXCERPT_CHARS)[0],
      score: asU64(field(hit, "score")),
    };
  }));
}

function readIntent(args: unknown, result: unknown): RenderIntent | null {
  const content = asString(field(result, "content"));
  if (content === undefined) return null;
  const [excerptText, truncated] = cleanBlock(content, MAX_INTENT_EXCERPT_CHARS);
  const format = asString(field(result, "kind"));
  return {
    kind: "read",
    path: cleanLine(asString(field(result, "path")) ?? asString(field(args, "path")) ?? "", MAX_INTENT_LABEL_CHARS),
    format: format === undefined ? null : cleanLine(format, MAX_INTENT_LABEL_CHARS),
    excerpt: excerptText,
    truncated,
  };
}

function navigateIntent(args: unknown, result: unknown): RenderIntent | null {
  const url = asString(field(result, "url")) ?? asString(field(args, "url"));
  if (url === undefined) return null;
  const title = asString(field(result, "title"));
  return {
    kind: "link",
    url: cleanLine(url, MAX_INTENT_LABEL_CHARS),
    title: title === undefined ? null : cleanLine(title, MAX_INTENT_LABEL_CHARS),
  };
}

function browserReadIntent(result: unknown): RenderIntent | null {
  const text = asString(field(result, "text"));
  if (text === undefined) return null;
  const [excerptText, clamped] = cleanBlock(text, MAX_INTENT_EXCERPT_CHARS);
  const url = asString(field(result, "url"));
  return {
    kind: "web",
    url: url === undefined ? null : cleanLine(url, MAX_INTENT_LABEL_CHARS),
    title: null,
    excerpt: excerptText,
    truncated: (asBool(field(result, "truncated")) ?? false) || clamped,
  };
}

function sessionTraceIntent(result: unknown): RenderIntent | null {
  if (!isRecord(result) || !("target" in result)) return null;
  const ancestors = Array.isArray(result.ancestors) ? result.ancestors : [];
  const descendants = Array.isArray(result.descendants) ? result.descendants : [];
  const row = (role: string, event: unknown): string[] => [
    role,
    "seq" in (isRecord(event) ? event : {}) ? cellText(field(event, "seq")) : "",
    asString(field(event, "kind")) ?? "",
    asString(field(event, "node_id")) ?? "",
    asString(field(event, "status")) ?? "",
    isRecord(event) && "latency_ms" in event ? cellText(event.latency_ms) : "",
  ];
  const rows = [
    ...ancestors.map((event) => row("ancestor", event)),
    row("target", result.target),
    ...descendants.map((event) => row("descendant", event)),
  ];
  return tableIntent(["role", "seq", "kind", "node", "status", "latency_ms"], rows, asBool(field(result, "truncated")) ?? false);
}

function inspectTextIntent(result: unknown): RenderIntent | null {
  if (!isRecord(result)) return null;
  const metrics = ["words", "characters", "bytes", "lines"];
  if (!metrics.every((metric) => metric in result)) return null;
  return tableIntent(["metric", "value"], metrics.map((metric) => [metric, cellText(result[metric])]), false);
}

function calculatorIntent(args: unknown, result: unknown): RenderIntent | null {
  const operation = asString(field(args, "operation"));
  if (!isRecord(result) || !("result" in result) || operation === undefined || !isRecord(args)) return null;
  if (!("left" in args) || !("right" in args)) return null;
  return tableIntent(
    ["operation", "left", "right", "result"],
    [[operation, cellText(args.left), cellText(args.right), cellText(result.result)]],
    false,
  );
}

function namedIntent(tool: string, args: unknown, result: unknown): RenderIntent | null {
  switch (tool) {
    case "run_cli": return terminalIntent(args, result);
    case "search_knowledge": return knowledgeSearchIntent(result);
    case "session_search": return sessionSearchIntent(args, result);
    case "read_document": return readIntent(args, result);
    case "browser_navigate": return navigateIntent(args, result);
    case "browser_read": return browserReadIntent(result);
    case "browser_screenshot":
      return generic(tool, "binary screenshot payloads stay in the journal; render intents carry text only", null);
    case "session_trace": return sessionTraceIntent(result);
    case "inspect_text": return inspectTextIntent(result);
    case "calculator": return calculatorIntent(args, result);
    default: return null;
  }
}

// Shape-keyed derivations for tools the mirror does not know by name —
// connector pack operations, MCP tools, embedder tools.
function structuralIntent(result: unknown): RenderIntent | null {
  if (isRecord(result)) {
    const before = asString(result.before);
    const after = asString(result.after);
    if (before !== undefined && after !== undefined) {
      const [beforeText, beforeClamped] = cleanBlock(before, MAX_INTENT_EXCERPT_CHARS);
      const [afterText, afterClamped] = cleanBlock(after, MAX_INTENT_EXCERPT_CHARS);
      const path = asString(result.path);
      return {
        kind: "diff",
        path: path === undefined ? "result" : cleanLine(path, MAX_INTENT_LABEL_CHARS),
        before: beforeText,
        after: afterText,
        truncated: beforeClamped || afterClamped,
      };
    }
    const url = asString(result.url);
    if (url !== undefined) {
      const title = asString(result.title);
      return {
        kind: "link",
        url: cleanLine(url, MAX_INTENT_LABEL_CHARS),
        title: title === undefined ? null : cleanLine(title, MAX_INTENT_LABEL_CHARS),
      };
    }
    return null;
  }
  if (!Array.isArray(result) || result.length === 0 || !result.every(isRecord)) return null;
  const columns: string[] = [];
  for (const entry of result.slice(0, MAX_INTENT_TABLE_ROWS)) {
    for (const key of Object.keys(entry)) {
      if (!columns.includes(key)) columns.push(key);
    }
  }
  const rows = result.slice(0, MAX_INTENT_TABLE_ROWS).map((entry) =>
    columns.slice(0, MAX_INTENT_TABLE_COLUMNS).map((column) => (column in entry ? cellText(entry[column]) : ""))
  );
  return tableIntent(columns, rows, columns.length > MAX_INTENT_TABLE_COLUMNS || result.length > MAX_INTENT_TABLE_ROWS);
}

/**
 * Derive the render intent for one journaled tool call. Pure and total:
 * the same `(tool, arguments, result)` triple always derives the same
 * intent — the client-side half of the replay-identity invariant. Mirrors
 * `rusty_core::render_intent::render_intent`.
 */
export function deriveRenderIntent(tool: string, args: unknown, result: unknown): RenderIntent {
  return namedIntent(tool, args, result)
    ?? structuralIntent(result)
    ?? generic(tool, "no render intent matches the journaled result shape", result);
}

function inlinePayload(payload: unknown): unknown | undefined {
  return isRecord(payload) && payload.kind === "inline" && "value" in payload ? payload.value : undefined;
}

/**
 * Derive the render intent for one journaled run event. Returns `null` for
 * anything that is not a `tool_call`; failed calls and content-addressed
 * payloads degrade to honest generic cards. Mirrors
 * `rusty_core::render_intent::render_intent_from_event`.
 */
export function deriveRenderIntentFromEvent(event: RunEvent): RenderIntent | null {
  if (event.kind !== "tool_call") return null;
  if (event.input === null || event.input === undefined) {
    return generic("", "the journaled tool call carries no request payload", null);
  }
  const request = inlinePayload(event.input);
  if (request === undefined) {
    return generic("", "the tool call request is content-addressed; its bytes live in the journal artifact map", null);
  }
  const tool = asString(field(request, "tool")) ?? "";
  const args = field(request, "arguments") ?? null;
  if (event.status === "error") {
    const output = inlinePayload(event.output);
    const message = asString(field(output, "error")) ?? "the journal records the failure without an error payload";
    return {
      kind: "generic",
      tool: cleanLine(tool, MAX_INTENT_LABEL_CHARS),
      reason: "the tool call failed; the journal carries the error",
      summary: cleanLine(message, MAX_INTENT_SUMMARY_CHARS),
    };
  }
  if (event.output === null || event.output === undefined) {
    return generic(tool, "the journaled tool call carries no result payload", null);
  }
  const result = inlinePayload(event.output);
  if (result === undefined) {
    return generic(tool, "the tool call result is content-addressed; its bytes live in the journal artifact map", null);
  }
  return deriveRenderIntent(tool, args, result);
}
