// A connector described by hand — a system with no OpenAPI document — as a
// builder fills it in. The server composes and registers the manifest
// (`POST /connectors/describe`, the same composer the Composer agent uses);
// this only shapes the draft and names the problems a builder can fix
// before sending it. Every operation becomes one tool, `{id}.{operation}`.

export type AuthStyle = "bearer" | "basic" | "header" | "query" | "none";
export type Method = "GET" | "POST" | "PUT" | "PATCH" | "DELETE";
export type Effect = "read_only" | "idempotent" | "compensatable" | "irreversible";

export interface ParamDraft {
  name: string;
  type: "string" | "integer" | "number" | "boolean";
  required: boolean;
  description: string;
}

export interface OperationDraft {
  name: string;
  description: string;
  method: Method;
  path: string;
  effect: Effect;
  params: ParamDraft[];
  /** For a write: the read-only operation of this connector that finds what
   * the write made, and its arguments as a template over the write's
   * (`"$short_description"` is the value the write sent). */
  reconcile?: { operation: string; arguments: string };
}

export interface ConnectorDraft {
  name: string;
  description: string;
  base_url: string;
  documentation_url: string;
  auth: AuthStyle;
  /** The header or query parameter name, for those two styles. */
  auth_name: string;
  /** The parameterless read the server tests a connection with. */
  check_path: string;
  operations: OperationDraft[];
}

export const emptyConnectorDraft = (): ConnectorDraft => ({
  name: "", description: "", base_url: "", documentation_url: "", auth: "bearer", auth_name: "",
  check_path: "/", operations: [emptyOperation()],
});

export const emptyOperation = (): OperationDraft => ({
  name: "", description: "", method: "GET", path: "/", effect: "read_only", params: [],
});

export const kebab = (text: string) =>
  text.trim().toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-+|-+$/g, "");

/** The default effect for a method — the honest one, which a builder can
 * lower only by saying so. */
export const effectFor = (method: Method): Effect =>
  method === "GET" ? "read_only" : method === "PUT" ? "idempotent" : method === "DELETE" ? "irreversible" : "compensatable";

/** What is still wrong, in the order a builder should fix it. */
export function connectorProblems(d: ConnectorDraft): string[] {
  const problems: string[] = [];
  if (!kebab(d.name)) problems.push("Name the system.");
  if (!/^https:\/\/[^\s/]+/.test(d.base_url.trim())) problems.push("The API root must start with https://.");
  if (!/^https?:\/\//.test(d.documentation_url.trim())) problems.push("Link to its documentation.");
  if ((d.auth === "header" || d.auth === "query") && !d.auth_name.trim()) problems.push("Name the header or parameter the credential goes in.");
  if (!d.check_path.trim().startsWith("/")) problems.push("The check path starts with /.");
  const seen = new Set<string>();
  d.operations.forEach((op, i) => {
    const label = op.name.trim() ? `Operation ${op.name}` : `Operation ${i + 1}`;
    if (!kebab(op.name)) problems.push(`${label}: give it a name.`);
    else if (seen.has(kebab(op.name))) problems.push(`${label}: that name is used twice.`);
    seen.add(kebab(op.name));
    if (!op.description.trim()) problems.push(`${label}: say what it does — the model reads this to choose it.`);
    if (!op.path.trim().startsWith("/")) problems.push(`${label}: the path starts with /.`);
    const inPath = [...op.path.matchAll(/\{([a-zA-Z0-9_]+)\}/g)].map((m) => m[1]);
    for (const p of inPath) if (!op.params.some((x) => x.name === p)) problems.push(`${label}: {${p}} in the path needs a parameter of that name.`);
    op.params.forEach((p) => { if (!/^[a-zA-Z_][a-zA-Z0-9_]*$/.test(p.name)) problems.push(`${label}: “${p.name}” is not a parameter name.`); });
  });
  return problems;
}
