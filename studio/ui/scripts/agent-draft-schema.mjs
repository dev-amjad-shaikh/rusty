// Generates `src/v2/draft/agent-draft.gen.ts` from the canonical AgentDraft
// JSON Schema (`src/v2/draft/agent-draft.schema.json`, handoff 04). The schema
// is the single source of truth for the draft document (R-A1): the committed
// TypeScript, the form, and validation anchors all derive from it.
//
//   node scripts/agent-draft-schema.mjs          regenerate the artifact
//   node scripts/agent-draft-schema.mjs --check  exit 1 when the artifact drifts
//
// The vitest drift gate renders in memory and diffs against the committed
// file, so a schema edit without a regeneration fails the build.

import { readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";

const uiRoot = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
export const SCHEMA_PATH = path.join(uiRoot, "src/v2/draft/agent-draft.schema.json");
export const TS_PATH = path.join(uiRoot, "src/v2/draft/agent-draft.gen.ts");

const HEADER = `/**
 * The AgentDraft document (handoff 04), generated from
 * \`agent-draft.schema.json\` by \`npm run generate:draft\` — do not edit by
 * hand. One type shared by Guided, Compose, Chat, Import, and Improve (R-A1).
 */

`;

/** Render the TypeScript module for a parsed AgentDraft schema. */
export function renderDraftTypes(schema) {
  const named = new Map();
  collectNamed(schema, named);
  let out = HEADER;
  for (const [name, itemSchema] of [...named.entries()].sort()) {
    out += renderNamedInterface(name, itemSchema);
    out += "\n";
  }
  out += docBlock(schema.description);
  out += "export interface AgentDraft {\n";
  out += renderFields(schema, 1);
  out += "}\n";
  return out;
}

/** Gather the named item interfaces (`x-name`) declared anywhere in the schema. */
function collectNamed(node, named) {
  if (!node || typeof node !== "object") return;
  if (node["x-name"]) named.set(node["x-name"], node);
  for (const value of Object.values(node)) {
    if (value && typeof value === "object") collectNamed(value, named);
  }
}

function renderNamedInterface(name, schema) {
  let out = docBlock(schema.description);
  out += `export interface ${name} {\n`;
  out += renderFields(schema, 1);
  out += "}\n";
  return out;
}

function renderFields(schema, indent) {
  const pad = "  ".repeat(indent);
  const required = new Set(schema.required ?? []);
  let out = "";
  for (const [field, fieldSchema] of Object.entries(schema.properties ?? {})) {
    const doc = docBlock(fieldSchema.description);
    if (doc) out += pad + doc;
    const optional = required.has(field) ? "" : "?";
    out += `${pad}${field}${optional}: ${renderType(fieldSchema)};\n`;
  }
  return out;
}

function renderType(schema) {
  if (schema["x-ts"]) return schema["x-ts"];
  if (schema.enum) return schema.enum.map((v) => JSON.stringify(v)).join(" | ");
  if (Array.isArray(schema.type)) {
    return schema.type.map((t) => renderType({ ...schema, type: t })).join(" | ");
  }
  switch (schema.type) {
    case "string":
      return "string";
    case "integer":
    case "number":
      return "number";
    case "boolean":
      return "boolean";
    case "null":
      return "null";
    case "array":
      return `${renderType(schema.items)}[]`;
    case "object":
      if (schema.additionalProperties) {
        return `Record<string, ${renderType(schema.additionalProperties)}>`;
      }
      if (schema["x-name"]) return schema["x-name"];
      throw new Error(`object without x-name or additionalProperties: ${JSON.stringify(schema)}`);
    default:
      throw new Error(`unsupported schema shape: ${JSON.stringify(schema)}`);
  }
}

function docBlock(description) {
  const summary = (description ?? "").split("\n")[0].trim();
  return summary ? `/** ${summary} */\n` : "";
}

function main() {
  const schema = JSON.parse(readFileSync(SCHEMA_PATH, "utf8"));
  const rendered = renderDraftTypes(schema);
  let existing = null;
  try {
    existing = readFileSync(TS_PATH, "utf8");
  } catch {
    // first generation
  }
  if (process.argv.includes("--check")) {
    if (existing !== rendered) {
      console.error("agent-draft.gen.ts drifts from the schema — run `npm run generate:draft`");
      process.exit(1);
    }
    console.log("agent-draft.gen.ts: up to date");
    return;
  }
  if (existing === rendered) {
    console.log("agent-draft.gen.ts: unchanged");
    return;
  }
  writeFileSync(TS_PATH, rendered);
  console.log("agent-draft.gen.ts: written");
}

if (process.argv[1] && fileURLToPath(import.meta.url) === path.resolve(process.argv[1])) {
  main();
}
