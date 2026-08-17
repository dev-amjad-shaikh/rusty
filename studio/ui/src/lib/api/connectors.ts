import { z } from "zod";
import { toolCapabilitySchema } from "../contracts";
import { parseJson, parseMutationJson, requestText, StudioApiError } from "./client";

const sha256hex = z.string().regex(/^[0-9a-f]{64}$/);
const dateTime = z.string().datetime({ offset: true });

// ------------------------------------------------------------------ //
// Manifests
// ------------------------------------------------------------------ //

export const credentialSlotSchema = z.object({
  name: z.string().min(1).max(64),
  description: z.string().max(256),
}).strict();

const searchAuthSchema = z.object({
  header: z.string().min(1).max(128),
  credential_slot: z.string().min(1).max(64),
}).strict();

// The `http_api` provider's wire shapes, mirroring `connector/manifest.rs`:
// auth is internally tagged on `style`, the body on `type`, methods are
// UPPERCASE, effects snake_case, and `default_headers` serializes as
// name/value pairs.
const httpApiAuthSchema = z.discriminatedUnion("style", [
  z.object({
    style: z.literal("bearer_token"),
    credential_slot: z.string().min(1).max(64),
  }).strict(),
  z.object({
    style: z.literal("basic"),
    username_slot: z.string().min(1).max(64),
    password_slot: z.string().min(1).max(64),
  }).strict(),
  z.object({
    style: z.literal("header"),
    header: z.string().min(1).max(128),
    credential_slot: z.string().min(1).max(64),
  }).strict(),
  z.object({
    style: z.literal("query_param"),
    param: z.string().min(1).max(128),
    credential_slot: z.string().min(1).max(64),
  }).strict(),
]);

const httpMethodSchema = z.enum(["GET", "POST", "PATCH", "PUT", "DELETE"]);

const operationBodySchema = z.discriminatedUnion("type", [
  z.object({ type: z.literal("none") }).strict(),
  z.object({
    type: z.literal("json"),
    params: z.array(z.string().min(1).max(64)).max(64),
  }).strict(),
  z.object({
    type: z.literal("graphql"),
    query: z.string().min(1).max(8 * 1024),
  }).strict(),
]);

const operationEffectSchema = z.enum(["read_only", "idempotent", "compensatable", "irreversible"]);

const httpApiOperationSchema = z.object({
  name: z.string().min(1).max(64),
  description: z.string().min(1).max(1024),
  method: httpMethodSchema,
  path: z.string().min(1).max(512),
  params_schema: z.record(z.string(), z.unknown()),
  query_params: z.array(z.string().min(1).max(64)).max(64),
  body: operationBodySchema,
  effect: operationEffectSchema,
  response: z.object({
    projection: z.string().max(256).nullable(),
    max_bytes: z.number().int().positive().nullable(),
  }).strict(),
  timeout_ms: z.number().int().positive().max(60_000).nullable(),
  idempotency_key_header: z.string().min(1).max(128).nullable(),
}).strict();

export const providerKindSchema = z.discriminatedUnion("kind", [
  z.object({
    kind: z.literal("mcp_stdio"),
    command: z.string().min(1).max(512),
    args: z.array(z.string().max(1024)).max(64),
    env_allowlist: z.array(z.string().min(1).max(128)).max(32),
  }).strict(),
  z.object({
    kind: z.literal("http_search"),
    base_url: z.string().min(1).max(2048),
    auth: searchAuthSchema.nullable(),
  }).strict(),
  z.object({
    kind: z.literal("http_api"),
    base_url: z.string().min(1).max(2048),
    auth: httpApiAuthSchema.nullable(),
    default_headers: z.array(z.tuple([z.string().min(1), z.string()])).max(16),
    health_check: z.string().min(1).max(64).nullable(),
    operations: z.array(httpApiOperationSchema).min(1).max(64),
  }).strict(),
]);

export const connectorManifestSchema = z.object({
  id: z.string().min(1).max(64),
  version: z.string().min(1).max(32),
  display_name: z.string().min(1).max(128),
  description: z.string().min(1).max(4 * 1024),
  provider: providerKindSchema,
  capabilities: z.array(z.string().max(256)).max(64),
  credential_slots: z.array(credentialSlotSchema).max(16),
  hash: sha256hex,
}).strict();

/// The `POST /connectors/manifests` payload: the manifest content. `hash`
/// is optional — the server recomputes it and a disagreement is a 422.
export const manifestPayloadSchema = z.object({
  id: z.string().min(1, "id is required"),
  version: z.string().min(1, "version is required"),
  display_name: z.string().min(1, "display_name is required"),
  description: z.string().min(1, "description is required"),
  provider: providerKindSchema,
  capabilities: z.array(z.string()).optional(),
  credential_slots: z.array(credentialSlotSchema).optional(),
  hash: z.string().optional(),
}).strict();

export type CredentialSlot = z.infer<typeof credentialSlotSchema>;
export type ProviderKind = z.infer<typeof providerKindSchema>;
export type ConnectorManifest = z.infer<typeof connectorManifestSchema>;
export type ManifestPayload = z.infer<typeof manifestPayloadSchema>;

const manifestListSchema = z.object({ manifests: z.array(connectorManifestSchema) }).strict();

const manifestReceiptSchema = z.object({
  receipt: z.object({
    id: z.string().min(1),
    version: z.string().min(1),
    manifest_hash: sha256hex,
    already_registered: z.boolean(),
  }).strict(),
}).strict();

export type ManifestReceipt = z.infer<typeof manifestReceiptSchema>["receipt"];

export async function listConnectorManifests(): Promise<ConnectorManifest[]> {
  const { text } = await requestText("/connectors/manifests");
  return parseJson(text, manifestListSchema, "Connector manifests").manifests;
}

export async function registerConnectorManifest(payload: ManifestPayload): Promise<ManifestReceipt> {
  const { status, text } = await requestText("/connectors/manifests", {
    method: "POST",
    body: JSON.stringify(payload),
  });
  if (status !== 201) throw new StudioApiError("Manifest registration returned an unproven receipt.", status, true);
  const { receipt } = parseMutationJson(text, manifestReceiptSchema, "Manifest receipt", status);
  if (receipt.id !== payload.id || receipt.version !== payload.version) {
    throw new StudioApiError("Manifest receipt named a different connector.", status, true);
  }
  return receipt;
}

// ------------------------------------------------------------------ //
// Instances
// ------------------------------------------------------------------ //

export const lifecycleStates = ["pending", "connecting", "healthy", "degraded", "failed", "disabled"] as const;
export const lifecycleStateSchema = z.enum(lifecycleStates);
export type LifecycleState = z.infer<typeof lifecycleStateSchema>;

export const connectorInstanceSchema = z.object({
  instance_id: z.string().min(1),
  connector_id: z.string().min(1),
  manifest_hash: sha256hex,
  credential_slots: z.array(z.string()),
  state: lifecycleStateSchema,
  state_reason: z.string().nullable(),
  consecutive_failures: z.number().int().nonnegative(),
  last_health_check_ms: z.number().int().nonnegative().nullable(),
  catalog_generation: z.number().int().positive().nullable(),
  catalog_hash: sha256hex.nullable(),
  created_at: dateTime,
  updated_at: dateTime,
}).strict();

export type ConnectorInstance = z.infer<typeof connectorInstanceSchema>;

const instanceListSchema = z.object({ instances: z.array(connectorInstanceSchema) }).strict();
const instanceResponseSchema = z.object({ instance: connectorInstanceSchema }).strict();

const lifecycleViewSchema = z.object({
  state: lifecycleStateSchema,
  reason: z.string().nullable(),
}).strict();

export const sweepOutcomeSchema = z.object({
  instance_id: z.string().min(1),
  previous_state: lifecycleViewSchema,
  current_state: lifecycleViewSchema,
  catalog_bumped: z.boolean(),
}).strict();

export type SweepOutcome = z.infer<typeof sweepOutcomeSchema>;

const healthResponseSchema = z.object({
  outcome: sweepOutcomeSchema,
  instance: connectorInstanceSchema,
}).strict();

const sweepResponseSchema = z.object({ outcomes: z.array(sweepOutcomeSchema) }).strict();

export async function listConnectorInstances(): Promise<ConnectorInstance[]> {
  const { text } = await requestText("/connectors/instances");
  return parseJson(text, instanceListSchema, "Connector instances").instances;
}

export interface CreateInstanceInput {
  manifest_hash: string;
  credentials: Record<string, string>;
}

export async function createConnectorInstance(input: CreateInstanceInput): Promise<ConnectorInstance> {
  const { status, text } = await requestText("/connectors/instances", {
    method: "POST",
    body: JSON.stringify(input),
  });
  if (status !== 201) throw new StudioApiError("Connector instantiation returned an unproven receipt.", status, true);
  const { instance } = parseMutationJson(text, instanceResponseSchema, "Connector instance receipt", status);
  if (instance.manifest_hash !== input.manifest_hash) {
    throw new StudioApiError("Connector instance receipt pinned a different manifest.", status, true);
  }
  return instance;
}

async function instanceAction(instanceId: string, action: string): Promise<ConnectorInstance> {
  const { status, text } = await requestText(
    `/connectors/instances/${encodeURIComponent(instanceId)}/${action}`,
    { method: "POST", body: "{}" },
  );
  const { instance } = parseMutationJson(text, instanceResponseSchema, `Connector ${action} receipt`, status);
  if (instance.instance_id !== instanceId) {
    throw new StudioApiError(`Connector ${action} receipt named a different instance.`, status, true);
  }
  return instance;
}

export function connectConnectorInstance(instanceId: string) {
  return instanceAction(instanceId, "connect");
}

export function disableConnectorInstance(instanceId: string) {
  return instanceAction(instanceId, "disable");
}

export function enableConnectorInstance(instanceId: string) {
  return instanceAction(instanceId, "enable");
}

export async function checkConnectorInstanceHealth(instanceId: string) {
  const { status, text } = await requestText(
    `/connectors/instances/${encodeURIComponent(instanceId)}/health`,
    { method: "POST", body: "{}" },
  );
  const result = parseMutationJson(text, healthResponseSchema, "Connector health receipt", status);
  if (result.instance.instance_id !== instanceId || result.outcome.instance_id !== instanceId) {
    throw new StudioApiError("Connector health receipt named a different instance.", status, true);
  }
  return result;
}

export async function sweepConnectors(): Promise<SweepOutcome[]> {
  const { status, text } = await requestText("/connectors/sweep", { method: "POST", body: "{}" });
  return parseMutationJson(text, sweepResponseSchema, "Connector sweep receipt", status).outcomes;
}

// ------------------------------------------------------------------ //
// Served catalogs
// ------------------------------------------------------------------ //

const catalogSchema = z.object({
  catalog: z.object({
    instance_id: z.string().min(1),
    generation: z.number().int().positive(),
    hash: sha256hex,
    tools: z.array(toolCapabilitySchema),
  }).strict(),
}).strict();

export type InstanceCatalog = z.infer<typeof catalogSchema>["catalog"];

export async function getInstanceCatalog(instanceId: string, generation?: number): Promise<InstanceCatalog> {
  const params = generation === undefined ? "" : `?generation=${encodeURIComponent(generation)}`;
  const { text } = await requestText(`/connectors/instances/${encodeURIComponent(instanceId)}/catalog${params}`);
  const { catalog } = parseJson(text, catalogSchema, "Connector catalog");
  if (catalog.instance_id !== instanceId) {
    throw new StudioApiError("Connector catalog named a different instance.", 0);
  }
  if (generation !== undefined && catalog.generation !== generation) {
    throw new StudioApiError("Connector catalog answered a different generation than the pin.", 0);
  }
  return catalog;
}

// ------------------------------------------------------------------ //
// Vault connections (slot bindings resolve through these)
// ------------------------------------------------------------------ //

// ConnectionRecord (rusty-core/src/broker.rs): `health` is always
// present; its refresh/failure fields are omitted until they happen.
const connectionHealthSchema = z.object({
  last_refresh_at: dateTime.optional(),
  last_failure: z.object({
    class: z.enum(["transient", "rate_limited", "timeout", "invalid_input", "dependency_failure", "resource_exhausted", "cancelled", "unknown"]),
    detail: z.string(),
    at: dateTime,
  }).strict().optional(),
  consecutive_failures: z.number().int().nonnegative(),
}).strict();

export const vaultConnectionSchema = z.object({
  connection_id: z.string().min(1),
  provider: z.enum(["oauth2_authorization_code", "oauth2_client_credentials", "oauth2_password", "api_key", "basic"]),
  subject: z.string().optional(),
  scopes: z.array(z.string()),
  status: z.enum(["active", "needs_reauth", "revoked"]),
  health: connectionHealthSchema,
  created_at: dateTime,
  updated_at: dateTime,
}).strict();

export type VaultConnection = z.infer<typeof vaultConnectionSchema>;

const vaultConnectionListSchema = z.object({ connections: z.array(vaultConnectionSchema) }).strict();

export async function listVaultConnections(): Promise<VaultConnection[]> {
  const { text } = await requestText("/connections");
  return parseJson(text, vaultConnectionListSchema, "Vault connections").connections;
}

/// The `password_grant` registration payload: the resource-owner
/// credentials the server exchanges with the token endpoint before
/// sealing. Mirrors `PasswordGrant::validate` in the runtime — an
/// `https://` endpoint and four non-empty, bounded values.
export const passwordGrantSchema = z.object({
  token_url: z.string().startsWith("https://", "The token endpoint must be an https:// URL.").max(2048),
  client_id: z.string().min(1, "client_id is required").max(1024),
  client_secret: z.string().min(1, "client_secret is required").max(1024),
  username: z.string().min(1, "username is required").max(1024),
  password: z.string().min(1, "password is required").max(1024),
}).strict();

export type PasswordGrant = z.infer<typeof passwordGrantSchema>;

const connectionReceiptSchema = z.object({ connection: vaultConnectionSchema }).strict();

/// One `POST /connections` with verbatim token material: the value seals
/// as the connection's `access_token` — the exact field the connector
/// plane's slot bridge resolves as the slot's secret
/// (`rusty-server/src/connectors.rs::open_credential`). The broker applies
/// no per-provider shape beyond a non-empty `access_token`, so `api_key`
/// and `basic` connections differ only in the provider label and the
/// self-describing custody fields (`username` / `password`).
async function postStaticConnection(
  provider: "api_key" | "basic",
  accessToken: string,
  custody: { username?: string; password?: string },
  subject?: string,
): Promise<VaultConnection> {
  const { status, text } = await requestText("/connections", {
    method: "POST",
    body: JSON.stringify({
      provider,
      ...(subject ? { subject } : {}),
      token: { access_token: accessToken, ...custody },
    }),
  });
  if (status !== 201) throw new StudioApiError("Connection registration returned an unproven receipt.", status, true);
  const { connection: record } = parseMutationJson(text, connectionReceiptSchema, "Connection receipt", status);
  if (record.provider !== provider) {
    throw new StudioApiError("Connection receipt named a different provider.", status, true);
  }
  return record;
}

/// The simple API-key flow: one value, sealed once, bound to the
/// connector's single credential slot.
export const apiKeyConnectionSchema = z.object({
  key: z.string().min(1, "API key is required").max(1024),
}).strict();

export type ApiKeyConnection = z.infer<typeof apiKeyConnectionSchema>;

export async function registerApiKeyConnection(
  input: ApiKeyConnection,
  subject?: string,
): Promise<VaultConnection> {
  return postStaticConnection("api_key", input.key, {}, subject);
}

/// The simple basic-auth flow. The connector plane resolves every declared
/// slot through its own connection id and reads only the connection's
/// `access_token` as the slot's secret, so one form registers a *pair*:
/// the username connection feeds the username slot, the password
/// connection the password slot. Both records carry the account as their
/// subject so the vault list identifies them without the ids.
export const basicConnectionSchema = z.object({
  username: z.string().min(1, "Username is required").max(1024),
  password: z.string().min(1, "Password is required").max(1024),
}).strict();

export type BasicConnection = z.infer<typeof basicConnectionSchema>;

export interface BasicConnectionPair {
  username_connection: VaultConnection;
  password_connection: VaultConnection;
}

export async function registerBasicConnection(input: BasicConnection): Promise<BasicConnectionPair> {
  const usernameConnection = await postStaticConnection(
    "basic",
    input.username,
    { username: input.username },
    input.username,
  );
  try {
    const passwordConnection = await postStaticConnection(
      "basic",
      input.password,
      { password: input.password },
      input.username,
    );
    return { username_connection: usernameConnection, password_connection: passwordConnection };
  } catch (error) {
    // The username leg already committed; say so, or the operator re-enters
    // credentials and the vault grows a silent duplicate.
    const detail = error instanceof StudioApiError ? error.message : "The password connection could not be registered.";
    const status = error instanceof StudioApiError ? error.status : 0;
    throw new StudioApiError(
      `${detail} The username half is already sealed in the vault as \`${usernameConnection.connection_id}\` and appears in the connection list.`,
      status,
      true,
    );
  }
}

/// `POST /connections` with the password grant: the exchange happens
/// server-side (the provider's refusal is the form's 422), and the grant
/// inputs are sealed with the minted tokens so refresh re-mints without a
/// human. The receipt carries the public record — never the material.
export async function registerVaultConnection(grant: PasswordGrant): Promise<VaultConnection> {
  const { status, text } = await requestText("/connections", {
    method: "POST",
    body: JSON.stringify({ provider: "oauth2_password", password_grant: grant }),
  });
  if (status !== 201) throw new StudioApiError("Connection registration returned an unproven receipt.", status, true);
  const { connection: record } = parseMutationJson(text, connectionReceiptSchema, "Connection receipt", status);
  if (record.provider !== "oauth2_password") {
    throw new StudioApiError("Connection receipt named a different provider.", status, true);
  }
  return record;
}

// ------------------------------------------------------------------ //
// Server error reading
// ------------------------------------------------------------------ //

/// The instantiate 422 names the unbound or refused credential slot
/// (`credential slot \`api_key\` …`). Extract it so the form can pin the
/// message to the exact row.
export function slotNamedInError(message: string): string | null {
  const match = /credential slot `([^`]+)`/.exec(message);
  return match ? match[1] : null;
}

/// The catalog pin-mismatch 409 names the live generation
/// (`… does not match the live generation N`). Extract it for the
/// reload affordance.
export function liveGenerationInError(message: string): number | null {
  const match = /live generation (\d+)/.exec(message);
  if (!match) return null;
  const value = Number(match[1]);
  return Number.isSafeInteger(value) && value > 0 ? value : null;
}
