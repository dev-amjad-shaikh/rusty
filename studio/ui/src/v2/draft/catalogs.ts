// Structural typing for the AgentDraft JSON Schema at runtime, plus the
// catalogs the form and validation resolve ids against. The schema itself
// stays the source of truth; this module only describes how the studio reads
// it (x- extension keys) and what the host supplies (catalogs).

import canonicalSchema from "./agent-draft.schema.json";

/** The x- extension keys the studio reads from the draft schema. */
export interface FieldSchema {
  description?: string;
  type?: string | string[];
  enum?: string[];
  items?: FieldSchema;
  properties?: Record<string, FieldSchema>;
  required?: string[];
  additionalProperties?: FieldSchema;
  "x-ts"?: string;
  "x-name"?: string;
  "x-control"?: string;
  "x-section"?: string;
  "x-label"?: string;
  "x-help"?: string;
  "x-empty"?: string;
  "x-add-label"?: string;
  "x-mono"?: boolean;
  "x-enum-labels"?: Record<string, string>;
  "x-spec-file"?: string;
}

export interface SectionMeta {
  id: string;
  label: string;
  note?: string;
}

export interface DraftSchema {
  description?: string;
  required?: string[];
  properties: Record<string, FieldSchema>;
  "x-sections"?: SectionMeta[];
}

/** The committed schema, typed for the form engine and validation anchors. */
export const AGENT_DRAFT_SCHEMA = canonicalSchema as DraftSchema;

/** One tool a connector mounts, with the effect class guards reason about. */
export interface CatalogTool {
  id: string;
  effect: "read" | "write" | "execute" | "egress";
}

export interface CatalogConnector {
  id: string;
  name: string;
  tools: CatalogTool[];
  /** Event names this connector can raise, for trigger specs. */
  events: string[];
}

export interface CatalogSkill {
  id: string;
  description: string;
}

/** Everything the form renders choices from and validation filters ids against. */
export interface DraftCatalogs {
  models: string[];
  connectors: CatalogConnector[];
  skills: CatalogSkill[];
  evalSuites: string[];
}

export const EMPTY_CATALOGS: DraftCatalogs = {
  models: [],
  connectors: [],
  skills: [],
  evalSuites: [],
};

/** Tools mounted by the draft's connectors, in mount order. */
export function mountedTools(
  draft: { connectors: string[] },
  catalogs: DraftCatalogs,
): CatalogTool[] {
  return draft.connectors.flatMap(
    (id) => catalogs.connectors.find((c) => c.id === id)?.tools ?? [],
  );
}

/** Map a violation path to its spec file via the schema's x-spec-file hints. */
export function specFileFor(path: string, schema: DraftSchema = AGENT_DRAFT_SCHEMA): string {
  const top = path.split(/[.[]/, 1)[0];
  const key = top === "toolset" ? "wrapped" : top;
  return schema.properties[key]?.["x-spec-file"] ?? "";
}
