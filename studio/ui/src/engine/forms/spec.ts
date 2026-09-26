// Reading a connection specification.
//
// A connector declares what it needs as a JSON Schema — the same document the
// server validates a config against — with a few presentation keys (`rusty_*`)
// that a validator ignores. Studio renders that document and nothing else: it
// has no list of connectors, no idea what ServiceNow or Salesforce wants, and
// no field it invented. If a connector adds a setting, the form grows; if it
// removes one, the form loses it. That is the whole point of the standard.

export type Schema = Record<string, unknown>;

const str = (schema: Schema, key: string): string | undefined =>
  typeof schema[key] === "string" ? (schema[key] as string) : undefined;

const obj = (schema: Schema, key: string): Schema | undefined =>
  schema[key] && typeof schema[key] === "object" && !Array.isArray(schema[key])
    ? (schema[key] as Schema)
    : undefined;

const arr = (schema: Schema, key: string): unknown[] | undefined =>
  Array.isArray(schema[key]) ? (schema[key] as unknown[]) : undefined;

export interface SpecField {
  path: string[];
  title: string;
  /** How to render it — a secret is never echoed back once saved. */
  kind: "text" | "secret" | "number" | "boolean" | "select";
  required: boolean;
  hint?: string;
  options?: string[];
  placeholder?: string;
}

export interface SpecVariant {
  title: string;
  /** The discriminator the variant fixes (`auth: "basic"`), carried into the
   * config so the server can tell the shapes apart. */
  fixed: Record<string, string>;
  nodes: SpecNode[];
}

export type SpecNode =
  | { kind: "field"; field: SpecField }
  | { kind: "group"; path: string[]; title: string; nodes: SpecNode[] }
  | { kind: "choice"; path: string[]; title: string; variants: SpecVariant[] };

const order = (schema: Schema) =>
  typeof schema.rusty_order === "number" ? (schema.rusty_order as number) : Number.MAX_SAFE_INTEGER;

function fieldKind(schema: Schema): SpecField["kind"] {
  if (schema.rusty_secret === true) return "secret";
  if (arr(schema, "enum")) return "select";
  const type = str(schema, "type");
  if (type === "boolean") return "boolean";
  if (type === "integer" || type === "number") return "number";
  return "text";
}

function isObjectSchema(schema: Schema): boolean {
  return str(schema, "type") === "object" || !!obj(schema, "properties") || !!arr(schema, "oneOf");
}

/** The nodes of one object schema, in the order a builder should read them. */
function objectNodes(schema: Schema, path: string[]): SpecNode[] {
  const properties = obj(schema, "properties") ?? {};
  const required = new Set((arr(schema, "required") ?? []).filter((r): r is string => typeof r === "string"));
  const entries = Object.entries(properties)
    .map(([key, value]) => [key, (value ?? {}) as Schema] as const)
    .filter(([, value]) => value.rusty_hidden !== true && value.const === undefined)
    .sort((a, b) => order(a[1]) - order(b[1]));

  return entries.map(([key, property]): SpecNode => {
    const here = [...path, key];
    const title = str(property, "title") ?? humanize(key);
    const variants = arr(property, "oneOf");
    if (variants) {
      return {
        kind: "choice",
        path: here,
        title,
        variants: variants.map((variant, i) => readVariant((variant ?? {}) as Schema, here, i)),
      };
    }
    if (isObjectSchema(property)) {
      return { kind: "group", path: here, title, nodes: objectNodes(property, here) };
    }
    return {
      kind: "field",
      field: {
        path: here,
        title,
        kind: fieldKind(property),
        required: required.has(key),
        hint: str(property, "description"),
        placeholder: str(property, "rusty_pattern_descriptor"),
        options: (arr(property, "enum") ?? []).map(String),
      },
    };
  });
}

function readVariant(schema: Schema, path: string[], index: number): SpecVariant {
  const properties = obj(schema, "properties") ?? {};
  const fixed: Record<string, string> = {};
  for (const [key, value] of Object.entries(properties)) {
    const constant = (value as Schema)?.const;
    if (typeof constant === "string") fixed[key] = constant;
  }
  return { title: str(schema, "title") ?? `Option ${index + 1}`, fixed, nodes: objectNodes(schema, path) };
}

/** Read a whole connection specification. */
export function readSpec(spec: Schema): SpecNode[] {
  return objectNodes(spec, []);
}

export const pathKey = (path: string[]) => path.join(".");

/** Every field a set of nodes will render, flattened — used to clear values
 * that belong to a variant the builder moved away from. */
export function fieldsOf(nodes: SpecNode[]): SpecField[] {
  return nodes.flatMap((node) => {
    if (node.kind === "field") return [node.field];
    if (node.kind === "group") return fieldsOf(node.nodes);
    return node.variants.flatMap((v) => fieldsOf(v.nodes));
  });
}

function assign(target: Record<string, unknown>, path: string[], value: unknown) {
  let here = target;
  for (const key of path.slice(0, -1)) {
    if (typeof here[key] !== "object" || here[key] === null) here[key] = {};
    here = here[key] as Record<string, unknown>;
  }
  here[path[path.length - 1]] = value;
}

/**
 * Build the config to send from what the builder typed. Only the chosen branch
 * of a choice contributes, and its discriminator rides along — so the object
 * matches one declared shape exactly, which is what a closed schema demands.
 */
export function buildConfig(
  nodes: SpecNode[],
  values: Record<string, string>,
  chosen: Record<string, number>,
): Record<string, unknown> {
  const config: Record<string, unknown> = {};
  const walk = (list: SpecNode[]) => {
    for (const node of list) {
      if (node.kind === "field") {
        const raw = values[pathKey(node.field.path)];
        if (raw === undefined || raw === "") continue;
        const value =
          node.field.kind === "number" ? Number(raw) : node.field.kind === "boolean" ? raw === "true" : raw;
        assign(config, node.field.path, value);
      } else if (node.kind === "group") {
        walk(node.nodes);
      } else {
        const variant = node.variants[chosen[pathKey(node.path)] ?? 0];
        if (!variant) continue;
        for (const [key, value] of Object.entries(variant.fixed)) assign(config, [...node.path, key], value);
        walk(variant.nodes);
      }
    }
  };
  walk(nodes);
  return config;
}

/** Which required fields of the chosen shape are still empty. Cheap enough to
 * run on every keystroke, and it says the same thing the server would. */
export function missingRequired(
  nodes: SpecNode[],
  values: Record<string, string>,
  chosen: Record<string, number>,
): string[] {
  const missing: string[] = [];
  const walk = (list: SpecNode[]) => {
    for (const node of list) {
      if (node.kind === "field") {
        if (node.field.required && !values[pathKey(node.field.path)]) missing.push(node.field.title);
      } else if (node.kind === "group") {
        walk(node.nodes);
      } else {
        const variant = node.variants[chosen[pathKey(node.path)] ?? 0];
        if (variant) walk(variant.nodes);
      }
    }
  };
  walk(nodes);
  return missing;
}

const isSealed = (value: unknown) =>
  typeof value === "object" && value !== null && (value as Record<string, unknown>).rusty_secret === true;

function readAt(config: Record<string, unknown>, path: string[]): unknown {
  let here: unknown = config;
  for (const key of path) {
    if (typeof here !== "object" || here === null) return undefined;
    here = (here as Record<string, unknown>)[key];
  }
  return here;
}

/**
 * A stored connection's config, as the form would hold it — for rotating a
 * credential without retyping everything else. Sealed values come back from
 * the server as `{rusty_secret: true}` and are left empty: the whole point
 * of rotating is to type the new one. A choice is resolved by whichever
 * variant's discriminators the config carries.
 */
export function valuesFromConfig(
  nodes: SpecNode[],
  config: Record<string, unknown>,
): { values: Record<string, string>; chosen: Record<string, number> } {
  const values: Record<string, string> = {};
  const chosen: Record<string, number> = {};
  const walk = (list: SpecNode[]) => {
    for (const node of list) {
      if (node.kind === "field") {
        const value = readAt(config, node.field.path);
        if (value === undefined || value === null || isSealed(value)) continue;
        values[pathKey(node.field.path)] = String(value);
      } else if (node.kind === "group") {
        walk(node.nodes);
      } else {
        const index = node.variants.findIndex((variant) =>
          Object.entries(variant.fixed).every(([key, fixed]) => readAt(config, [...node.path, key]) === fixed),
        );
        const picked = index < 0 ? 0 : index;
        chosen[pathKey(node.path)] = picked;
        const variant = node.variants[picked];
        if (variant) walk(variant.nodes);
      }
    }
  };
  walk(nodes);
  return { values, chosen };
}

/** A property key as a label: `api_token` → `Api token`, `base.url` → `Base url`. */
function humanize(key: string): string {
  const words = key.replace(/[_.\-]+/g, " ").replace(/([a-z])([A-Z])/g, "$1 $2").trim();
  return words ? words[0].toUpperCase() + words.slice(1) : key;
}
