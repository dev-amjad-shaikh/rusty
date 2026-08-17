import { z } from "zod";
import {
  parseJson,
  parseMutationJson,
  requestText,
  StudioApiError,
} from "./client";

// The schema-driven connector surface (`docs/connector-surface-design.md`,
// `rusty-server/src/connectors.rs`). The `connection_specification` is an
// arbitrary draft-07 JSON Schema — it stays `z.unknown()` at the envelope
// level and is interpreted by `../schema-form`, never validated here.

const hashSchema = z.string().regex(/^[0-9a-f]{64}$/);
const timestampSchema = z.string().datetime({ offset: true });

const operationAuthSchema = z.discriminatedUnion("style", [
  z.object({ style: z.literal("basic"), username: z.string(), password: z.string() }),
  z.object({ style: z.literal("bearer"), token: z.string() }),
]);

const connectorOperationSchema = z.object({
  name: z.string().min(1),
  description: z.string(),
  method: z.enum(["GET", "POST", "PATCH", "PUT", "DELETE"]),
  path: z.string().startsWith("/"),
  effect: z.enum(["read_only", "idempotent", "compensatable", "irreversible"]),
  params_schema: z.unknown(),
  headers: z.array(z.tuple([z.string(), z.string()])).optional(),
  auth: z.array(operationAuthSchema).optional(),
  max_response_bytes: z.number().int().positive().optional(),
});
export type ConnectorOperation = z.infer<typeof connectorOperationSchema>;

export const connectorManifestSchema = z.object({
  id: z.string().min(1),
  version: z.string().min(1),
  display_name: z.string().min(1),
  description: z.string(),
  documentation_url: z.string().startsWith("https://"),
  base_url: z.string().startsWith("https://"),
  connection_specification: z.unknown(),
  operations: z.array(connectorOperationSchema).min(1),
  check: z.string().min(1),
  hash: hashSchema,
});
export type ConnectorManifest = z.infer<typeof connectorManifestSchema>;

const manifestCatalogSchema = z.object({ manifests: z.array(connectorManifestSchema) });

/** The served instance shape: non-secret config plus `{"rusty_secret": true}`
 * markers where sealed secrets sit — secrets never render. */
export const connectorInstanceSchema = z.object({
  instance_id: z.string().min(1),
  manifest_hash: hashSchema,
  config: z.record(z.string(), z.unknown()),
  created_at: timestampSchema,
});
export type ConnectorInstance = z.infer<typeof connectorInstanceSchema>;

const instanceListSchema = z.object({ instances: z.array(connectorInstanceSchema) });

/** The Airbyte check verdict: `{"status", "message"?}`. */
const checkOutcomeSchema = z.object({
  status: z.enum(["succeeded", "failed"]),
  message: z.string().optional(),
});
export type ConnectorCheckOutcome = z.infer<typeof checkOutcomeSchema>;

const catalogToolSchema = z.object({
  name: z.string().min(1),
  description: z.string(),
  parameters_schema: z.unknown(),
  effect: z.enum(["pure", "read_only", "idempotent", "compensatable", "non_idempotent"]),
});
export type ConnectorCatalogTool = z.infer<typeof catalogToolSchema>;

const instanceCatalogSchema = z.object({
  instance_id: z.string().min(1),
  manifest_hash: hashSchema,
  tools: z.array(catalogToolSchema),
});
export type ConnectorCatalog = z.infer<typeof instanceCatalogSchema>;

export async function listConnectors(): Promise<ConnectorManifest[]> {
  const { text } = await requestText("/connectors");
  return parseJson(text, manifestCatalogSchema, "Connector catalog").manifests;
}

export async function listConnectorInstances(): Promise<ConnectorInstance[]> {
  const { text } = await requestText("/connectors/instances");
  return parseJson(text, instanceListSchema, "Connector instances").instances;
}

export interface CreateConnectorInstanceInput {
  manifest_hash: string;
  config: Record<string, unknown>;
}

export async function createConnectorInstance(
  input: CreateConnectorInstanceInput,
): Promise<ConnectorInstance> {
  const { status, text } = await requestText("/connectors/instances", {
    method: "POST",
    body: JSON.stringify({ manifest_hash: input.manifest_hash, config: input.config }),
  });
  if (status !== 201) {
    throw new StudioApiError("Connector setup returned an unproven receipt.", status, true);
  }
  const instance = parseMutationJson(text, connectorInstanceSchema, "Connector instance receipt", status);
  if (instance.manifest_hash !== input.manifest_hash) {
    throw new StudioApiError("Connector instance receipt named a different connector.", status, true);
  }
  return instance;
}

/** The setup gate: check a candidate config pre-save. A schema rejection
 * arrives as a 422 whose message names the failing dot path — the form pins
 * field errors from it (`pinFieldError` in `../schema-form`). */
export async function checkConnectorConfig(
  manifestHash: string,
  config: Record<string, unknown>,
): Promise<ConnectorCheckOutcome> {
  const { text } = await requestText("/connectors/check", {
    method: "POST",
    body: JSON.stringify({ manifest_hash: manifestHash, config }),
  });
  return parseJson(text, checkOutcomeSchema, "Connector check");
}

/** The edit gate: re-check a live instance's stored config. */
export async function checkConnectorInstance(instanceId: string): Promise<ConnectorCheckOutcome> {
  const { text } = await requestText("/connectors/check", {
    method: "POST",
    body: JSON.stringify({ instance_id: instanceId }),
  });
  return parseJson(text, checkOutcomeSchema, "Connector re-check");
}

export async function getConnectorCatalog(instanceId: string): Promise<ConnectorCatalog> {
  const { text } = await requestText(`/connectors/instances/${encodeURIComponent(instanceId)}/catalog`);
  const catalog = parseJson(text, instanceCatalogSchema, "Connector tool catalog");
  if (catalog.instance_id !== instanceId) {
    throw new StudioApiError("Connector tool catalog named a different instance.", 0);
  }
  return catalog;
}
