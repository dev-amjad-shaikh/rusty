// The schema-form interpreter: walks a manifest's `connection_specification`
// (draft-07 JSON Schema plus the `rusty_*` presentation hints) and produces a
// typed form model the generic setup form renders. There is no per-connector
// UI code — every connector's form derives from this one walk
// (`docs/connector-surface-design.md`, research section 3).
//
// Graceful degradation is the contract: unknown keywords are ignored, a
// property without a usable `type` renders as text, and a root that is not an
// object schema reports `supported: false` instead of throwing.

export type FormInput = "text" | "password" | "number" | "boolean" | "select";

/** One scalar input in the generated form. */
export interface FormField {
  kind: "field";
  /** Dot path from the config root (`credentials.username`). */
  path: string;
  name: string;
  title: string;
  description: string | null;
  input: FormInput;
  required: boolean;
  /** `rusty_order`; unordered fields sort after ordered ones, by name. */
  order: number;
  /** `rusty_group`; grouped fields render under a shared heading. */
  group: string | null;
  /** `rusty_secret` — renders masked and seals server-side. */
  secret: boolean;
  enumValues: string[] | null;
  /** A fixed `const` value: never rendered, always applied to the config. */
  constValue?: unknown;
  pattern: string | null;
  /** `rusty_pattern_descriptor` — the human hint shown for a pattern field. */
  patternHint: string | null;
  defaultValue?: unknown;
}

/** One `oneOf` variant: its const discriminators plus its own fields. */
export interface VariantChoice {
  /** The joined discriminator const values (the picker's option value). */
  key: string;
  label: string;
  /** Const fields auto-applied to the config when this variant is selected. */
  consts: Record<string, unknown>;
  children: FormNode[];
}

/** A `oneOf` + `const` polymorphic sub-form, rendered as a variant picker. */
export interface VariantNode {
  kind: "variant";
  path: string;
  name: string;
  title: string;
  description: string | null;
  required: boolean;
  order: number;
  group: string | null;
  variants: VariantChoice[];
}

/** A nested object without polymorphism — renders as a fieldset. */
export interface ObjectNode {
  kind: "object";
  path: string;
  name: string;
  title: string;
  description: string | null;
  required: boolean;
  order: number;
  group: string | null;
  children: FormNode[];
}

export type FormNode = FormField | VariantNode | ObjectNode;

/** The interpreted root of one connector's configuration surface. */
export interface ConnectorForm {
  title: string | null;
  nodes: FormNode[];
  /** Group names in first-appearance order (after ordering). */
  groups: string[];
  /** False when the schema is not an object form can be derived from. */
  supported: boolean;
}

type SchemaObject = Record<string, unknown>;

function isSchemaObject(value: unknown): value is SchemaObject {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function asString(value: unknown): string | null {
  return typeof value === "string" && value.length > 0 ? value : null;
}

function asOrder(value: unknown): number {
  return typeof value === "number" && Number.isFinite(value) ? value : Number.MAX_SAFE_INTEGER;
}

/** `your-instance` → `Your instance` — the fallback label when no title is set. */
export function humanizeName(name: string): string {
  const spaced = name.replace(/[_-]+/g, " ").trim();
  return spaced.length > 0 ? spaced.charAt(0).toUpperCase() + spaced.slice(1) : name;
}

function nodeBase(name: string, path: string, schema: SchemaObject, required: boolean) {
  return {
    path,
    name,
    title: asString(schema.title) ?? humanizeName(name),
    description: asString(schema.description),
    required,
    order: asOrder(schema.rusty_order),
    group: asString(schema.rusty_group),
  };
}

/** The variant label: the variant's title, else its discriminator const. */
function variantLabel(variant: SchemaObject, consts: Record<string, unknown>): string {
  const title = asString(variant.title);
  if (title) return title;
  const first = Object.values(consts)[0];
  return typeof first === "string" ? humanizeName(first) : "Variant";
}

/** Interpret one property schema into a form node, or null when `rusty_hidden`. */
function interpretProperty(name: string, schema: unknown, path: string, required: boolean): FormNode | null {
  if (!isSchemaObject(schema)) {
    // A non-object subschema (rare, e.g. `true`) degrades to a plain text field.
    return { kind: "field", ...nodeBase(name, path, {}, required), input: "text", secret: false, enumValues: null, pattern: null, patternHint: null };
  }
  if (schema.rusty_hidden === true) return null;

  const variants = Array.isArray(schema.oneOf) ? schema.oneOf.filter(isSchemaObject) : [];
  if (variants.length > 0) {
    return {
      kind: "variant",
      ...nodeBase(name, path, schema, required),
      variants: variants.map((variant, index) => {
        const properties = isSchemaObject(variant.properties) ? variant.properties : {};
        const requiredList = Array.isArray(variant.required) ? variant.required.filter((item): item is string => typeof item === "string") : [];
        const consts: Record<string, unknown> = {};
        for (const [prop, sub] of Object.entries(properties)) {
          if (isSchemaObject(sub) && "const" in sub) consts[prop] = sub.const;
        }
        const children = Object.entries(properties)
          .filter(([prop]) => !(prop in consts))
          .map(([prop, sub]) => interpretProperty(prop, sub, `${path}.${prop}`, requiredList.includes(prop)))
          .filter((node): node is FormNode => node !== null);
        const constValues = Object.values(consts);
        const key = constValues.length > 0 && constValues.every((value) => typeof value === "string")
          ? constValues.join("/")
          : `variant-${index}`;
        return { key, label: variantLabel(variant, consts), consts, children: sortNodes(children) };
      }),
    };
  }

  const type = typeof schema.type === "string" ? schema.type : null;
  const properties = isSchemaObject(schema.properties) ? schema.properties : null;
  if (type === "object" || (!type && properties)) {
    return {
      kind: "object",
      ...nodeBase(name, path, schema, required),
      children: interpretChildren(schema, path),
    };
  }

  const secret = schema.rusty_secret === true;
  const enumValues = Array.isArray(schema.enum)
    ? schema.enum.filter((value): value is string => typeof value === "string")
    : null;
  let input: FormInput = "text";
  if (secret) input = "password";
  else if (enumValues && enumValues.length > 0) input = "select";
  else if (type === "integer" || type === "number") input = "number";
  else if (type === "boolean") input = "boolean";
  const field: FormField = {
    kind: "field",
    ...nodeBase(name, path, schema, required),
    input,
    secret,
    enumValues: enumValues && enumValues.length > 0 ? enumValues : null,
    pattern: asString(schema.pattern),
    patternHint: asString(schema.rusty_pattern_descriptor),
  };
  if ("const" in schema) field.constValue = schema.const;
  if ("default" in schema) field.defaultValue = schema.default;
  return field;
}

function interpretChildren(schema: SchemaObject, prefix: string): FormNode[] {
  const properties = isSchemaObject(schema.properties) ? schema.properties : {};
  const requiredList = Array.isArray(schema.required) ? schema.required.filter((item): item is string => typeof item === "string") : [];
  const nodes = Object.entries(properties)
    .map(([name, sub]) => interpretProperty(name, sub, prefix ? `${prefix}.${name}` : name, requiredList.includes(name)))
    .filter((node): node is FormNode => node !== null);
  return sortNodes(nodes);
}

function sortNodes(nodes: FormNode[]): FormNode[] {
  // Stable: unordered peers keep their declaration order.
  return [...nodes].sort((left, right) => left.order - right.order);
}

/** Interpret one `connection_specification` into the form model. */
export function interpretForm(spec: unknown): ConnectorForm {
  if (!isSchemaObject(spec) || spec.type !== "object") {
    return { title: null, nodes: [], groups: [], supported: false };
  }
  const nodes = interpretChildren(spec, "");
  const groups: string[] = [];
  for (const node of nodes) {
    if (node.group && !groups.includes(node.group)) groups.push(node.group);
  }
  return { title: asString(spec.title), nodes, groups, supported: true };
}

// --------------------------------------------------------------------- //
// Form state → config
// --------------------------------------------------------------------- //

/** Field values keyed by dot path (strings for text/number, boolean for checkboxes). */
export type FormValues = Record<string, unknown>;
/** The selected variant key per variant-node path. */
export type VariantSelections = Record<string, string>;

function setPath(target: Record<string, unknown>, path: string, value: unknown) {
  const segments = path.split(".");
  let node = target;
  for (const segment of segments.slice(0, -1)) {
    const next = node[segment];
    if (!isSchemaObject(next)) {
      node[segment] = {};
    }
    node = node[segment] as Record<string, unknown>;
  }
  node[segments[segments.length - 1]] = value;
}

function collectNodes(nodes: FormNode[], selections: VariantSelections, into: FormNode[] = []): FormNode[] {
  for (const node of nodes) {
    if (node.kind === "variant") {
      const chosen = node.variants.find((variant) => variant.key === selections[node.path]) ?? node.variants[0];
      if (chosen) collectNodes(chosen.children, selections, into);
    } else if (node.kind === "object") {
      into.push(node);
      collectNodes(node.children, selections, into);
    } else {
      into.push(node);
    }
  }
  return into;
}

/** Every field node visible under the current variant selections. */
export function visibleFields(form: ConnectorForm, selections: VariantSelections): FormField[] {
  return collectNodes(form.nodes, selections).filter((node): node is FormField => node.kind === "field");
}

/** The initial variant selection: the first variant of every picker. */
export function initialSelections(form: ConnectorForm): VariantSelections {
  const selections: VariantSelections = {};
  const walk = (nodes: FormNode[]) => {
    for (const node of nodes) {
      if (node.kind === "variant" && node.variants.length > 0) selections[node.path] = node.variants[0].key;
      if (node.kind !== "field") walk(node.kind === "variant" ? node.variants.flatMap((variant) => variant.children) : node.children);
    }
  };
  walk(form.nodes);
  return selections;
}

/** The initial field values: declared defaults for visible fields. */
export function initialValues(form: ConnectorForm, selections: VariantSelections): FormValues {
  const values: FormValues = {};
  for (const field of visibleFields(form, selections)) {
    if (field.defaultValue !== undefined) values[field.path] = field.defaultValue;
    else if (field.input === "boolean") values[field.path] = false;
  }
  return values;
}

function relativePath(path: string, prefix: string): string {
  return prefix && path.startsWith(prefix) ? path.slice(prefix.length) : path;
}

function buildInto(config: Record<string, unknown>, nodes: FormNode[], values: FormValues, selections: VariantSelections, prefix: string) {
  for (const node of nodes) {
    if (node.kind === "variant") {
      const chosen = node.variants.find((variant) => variant.key === selections[node.path]) ?? node.variants[0];
      if (!chosen) continue;
      const branch: Record<string, unknown> = {};
      for (const [prop, value] of Object.entries(chosen.consts)) branch[prop] = value;
      buildInto(branch, chosen.children, values, selections, `${node.path}.`);
      setPath(config, relativePath(node.path, prefix), branch);
    } else if (node.kind === "object") {
      const branch: Record<string, unknown> = {};
      buildInto(branch, node.children, values, selections, `${node.path}.`);
      if (Object.keys(branch).length > 0 || node.required) setPath(config, relativePath(node.path, prefix), branch);
    } else {
      if (node.constValue !== undefined) {
        setPath(config, relativePath(node.path, prefix), node.constValue);
        continue;
      }
      const raw = values[node.path];
      if (node.input === "boolean") {
        if (raw === true || node.required) setPath(config, relativePath(node.path, prefix), raw === true);
        continue;
      }
      if (node.input === "number") {
        if (typeof raw === "number" && Number.isFinite(raw)) {
          setPath(config, relativePath(node.path, prefix), raw);
        } else if (typeof raw === "string" && raw.trim() !== "") {
          const parsed = Number(raw);
          if (Number.isFinite(parsed)) setPath(config, relativePath(node.path, prefix), parsed);
        }
        continue;
      }
      if (typeof raw === "string" && raw !== "") setPath(config, relativePath(node.path, prefix), raw);
    }
  }
}

/** Assemble the config object the server validates: visible values, variant
 * const discriminators, and standalone consts; empty optional fields omitted. */
export function buildConfig(form: ConnectorForm, values: FormValues, selections: VariantSelections): Record<string, unknown> {
  const config: Record<string, unknown> = {};
  buildInto(config, form.nodes, values, selections, "");
  return config;
}

// --------------------------------------------------------------------- //
// 422 field pinning
// --------------------------------------------------------------------- //

/** Every pin-able path: fields, objects, and variant pickers. */
export function knownPaths(form: ConnectorForm): string[] {
  const paths: string[] = [];
  const walk = (nodes: FormNode[]) => {
    for (const node of nodes) {
      paths.push(node.path);
      if (node.kind === "object") walk(node.children);
      if (node.kind === "variant") for (const variant of node.variants) walk(variant.children);
    }
  };
  walk(form.nodes);
  return paths;
}

/** Parse the 422 contract's `{dot-path}: {reason}` message into the field it
 * pins. Falls back to the longest known ancestor path (a oneOf parent mismatch
 * stays at the parent path), or null when no known path matches. */
export function pinFieldError(message: string, paths: string[]): { path: string; reason: string } | null {
  const separator = message.indexOf(": ");
  if (separator <= 0) return null;
  const candidate = message.slice(0, separator);
  const reason = message.slice(separator + 2);
  if (!/^[A-Za-z0-9_]+(\.[A-Za-z0-9_]+)*$/.test(candidate)) return null;
  if (paths.includes(candidate)) return { path: candidate, reason };
  // Ancestor fallback: the server may name a path inside a variant branch the
  // form addresses through its picker (e.g. an unknown-property rejection).
  let best: string | null = null;
  for (const path of paths) {
    if (candidate.startsWith(`${path}.`) && (best === null || path.length > best.length)) best = path;
  }
  return best === null ? null : { path: best, reason: `${candidate.slice(best.length + 1)}: ${reason}` };
}
