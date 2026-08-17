import { z } from "zod";
import { isLosslessNumber, parse as parseLossless } from "lossless-json";
import { assistantSchema, type Assistant } from "../contracts";
import { jsonEquivalent, parseJson, requestText, StudioApiError } from "./client";

const VERSION_RESPONSE_LIMIT = 2 * 1024 * 1024;
const versionId = z.string().regex(/^av-[0-9a-f]{64}$/);
const instant = z.string().datetime({ offset: true });

const versionSummarySchema = z.object({
  version_id: versionId,
  parent_version_id: versionId.optional(),
  graph: z.string().min(1).max(256),
  created_at: instant,
  active: z.boolean(),
}).strict();

const versionSchema = versionSummarySchema.omit({ graph: true }).extend({
  name: z.string().min(1).max(1_024),
  graph: z.string().min(1).max(256),
  config: z.unknown(),
  metadata: z.unknown(),
}).strict();

const historySchema = z.object({
  assistant_id: z.string().min(1).max(256),
  active_version_id: versionId,
  assistant: assistantSchema,
  versions: z.array(versionSummarySchema).max(256),
}).strict();

const exactVersionSchema = z.object({
  assistant_id: z.string().min(1).max(256),
  active_version_id: versionId,
  version: versionSchema,
}).strict();

const createVersionReceiptSchema = exactVersionSchema.extend({ created: z.boolean() }).strict();
const activationReceiptSchema = z.object({ assistant: assistantSchema, activated: z.boolean() }).strict();
const lifecycleReceiptSchema = z.object({
  assistant: assistantSchema,
  changed: z.boolean(),
  lifecycle: z.enum(["active", "archived"]),
}).strict();

export type AssistantVersionSummary = z.infer<typeof versionSummarySchema>;
export type AssistantVersion = z.infer<typeof versionSchema>;
export type AssistantHistory = z.infer<typeof historySchema>;

export interface AssistantVersionFields {
  name: string;
  graph: string;
  config: unknown;
  metadata: unknown;
}

function assertSafeJsonNumbers(text: string, context: string) {
  let root: unknown;
  try { root = parseLossless(text); }
  catch { return; }
  const pending = [root];
  while (pending.length) {
    const value = pending.pop();
    if (isLosslessNumber(value)) {
      const raw = value.toString();
      if (/^-?(?:0|[1-9][0-9]*)$/.test(raw)) {
        const integer = BigInt(raw);
        if (integer < BigInt(Number.MIN_SAFE_INTEGER) || integer > BigInt(Number.MAX_SAFE_INTEGER)) {
          throw new StudioApiError(`${context} contains an integer the visual workspace cannot preserve exactly.`, 0);
        }
      }
    } else if (Array.isArray(value)) pending.push(...value);
    else if (value && typeof value === "object") pending.push(...Object.values(value));
  }
}

function parseAssistantJson<T>(text: string, schema: z.ZodType<T>, context: string) {
  assertSafeJsonNumbers(text, context);
  return parseJson(text, schema, context);
}

function parseAssistantMutation<T>(text: string, schema: z.ZodType<T>, context: string, status: number) {
  try { return parseAssistantJson(text, schema, context); }
  catch (caught) {
    throw new StudioApiError(caught instanceof Error ? caught.message : `${context} was not trustworthy.`, status, true);
  }
}

function rustStringCompare(left: string, right: string) {
  const encoder = new TextEncoder();
  const a = encoder.encode(left), b = encoder.encode(right);
  const length = Math.min(a.length, b.length);
  for (let index = 0; index < length; index += 1) {
    if (a[index] !== b[index]) return a[index] - b[index];
  }
  return a.length - b.length;
}

function canonicalRustJson(value: unknown): string {
  if (isLosslessNumber(value)) return value.toString();
  if (typeof value === "number" && Number.isFinite(value)) return JSON.stringify(value);
  if (value === null || typeof value === "boolean" || typeof value === "string") return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map(canonicalRustJson).join(",")}]`;
  if (value && typeof value === "object") {
    const record = value as Record<string, unknown>;
    return `{${Object.keys(record).sort(rustStringCompare).map((key) => `${JSON.stringify(key)}:${canonicalRustJson(record[key])}`).join(",")}}`;
  }
  throw new StudioApiError("Assistant version contained a non-JSON value.", 0);
}

export async function assistantVersionContentAddress(rawVersion: unknown) {
  if (!rawVersion || typeof rawVersion !== "object" || Array.isArray(rawVersion)) {
    throw new StudioApiError("Assistant version body was unavailable for integrity verification.", 0);
  }
  const raw = rawVersion as Record<string, unknown>;
  const body = {
    parent_version_id: raw.parent_version_id ?? null,
    name: raw.name,
    graph: raw.graph,
    config: raw.config,
    metadata: raw.metadata,
  };
  const bytes = new TextEncoder().encode(canonicalRustJson(body));
  const digest = await crypto.subtle.digest("SHA-256", bytes);
  return `av-${Array.from(new Uint8Array(digest), (byte) => byte.toString(16).padStart(2, "0")).join("")}`;
}

async function verifyContentAddress(rawVersion: unknown, expectedVersionId: string) {
  const actual = await assistantVersionContentAddress(rawVersion);
  if (actual !== expectedVersionId) throw new StudioApiError("Assistant version body did not match its immutable content address.", 0);
}

function exactVersionId(value: string, context: string) {
  if (!versionId.safeParse(value).success) throw new StudioApiError(`${context} was not an exact assistant version.`, 0);
}

async function verifyHistory(value: AssistantHistory, rawAssistant: unknown, assistantId: string) {
  if (value.assistant_id !== assistantId || value.assistant.assistant_id !== assistantId
    || value.assistant.active_version_id !== value.active_version_id
    || value.assistant.version_count !== value.versions.length) {
    throw new StudioApiError("Assistant history did not match the selected agent.", 0);
  }
  const ids = new Set(value.versions.map((item) => item.version_id));
  const active = value.versions.find((item) => item.version_id === value.active_version_id);
  const roots = value.versions.filter((item) => item.parent_version_id === undefined);
  const parentsExist = value.versions.every((item) => item.parent_version_id === undefined
    || (item.parent_version_id !== item.version_id && ids.has(item.parent_version_id)));
  const acyclic = value.versions.every((item) => {
    const seen = new Set<string>();
    let current: AssistantVersionSummary | undefined = item;
    while (current?.parent_version_id) {
      if (seen.has(current.version_id)) return false;
      seen.add(current.version_id);
      current = value.versions.find((candidate) => candidate.version_id === current?.parent_version_id);
    }
    return Boolean(current);
  });
  if (ids.size !== value.versions.length || !active
    || value.versions.filter((item) => item.active).length !== 1
    || !active.active || active.graph !== value.assistant.graph || roots.length !== 1 || !parentsExist || !acyclic) {
    throw new StudioApiError("Assistant history did not contain one coherent immutable lineage.", 0);
  }
  await verifyContentAddress({ ...(rawAssistant as Record<string, unknown>), parent_version_id: active.parent_version_id ?? null }, active.version_id);
  return value;
}

export async function getAssistant(assistantId: string) {
  const { text } = await requestText(`/assistants/${encodeURIComponent(assistantId)}`, {}, 512 * 1024);
  const assistant = parseAssistantJson(text, assistantSchema, "Assistant");
  if (assistant.assistant_id !== assistantId) throw new StudioApiError("Assistant evidence did not match the requested agent.", 0);
  return assistant;
}

export async function listAssistantVersions(assistantId: string) {
  const { text } = await requestText(`/assistants/${encodeURIComponent(assistantId)}/versions`, {}, VERSION_RESPONSE_LIMIT);
  const raw = parseLossless(text) as { assistant?: unknown };
  return verifyHistory(parseAssistantJson(text, historySchema, "Assistant history"), raw.assistant, assistantId);
}

export async function getAssistantVersion(assistantId: string, summary: AssistantVersionSummary, expectedActiveVersionId: string) {
  const requestedVersionId = summary.version_id;
  exactVersionId(requestedVersionId, "Requested version");
  const { text } = await requestText(`/assistants/${encodeURIComponent(assistantId)}/versions/${encodeURIComponent(requestedVersionId)}`, {}, VERSION_RESPONSE_LIMIT);
  const value = parseAssistantJson(text, exactVersionSchema, "Assistant version");
  const raw = parseLossless(text) as { version?: unknown };
  await verifyContentAddress(raw.version, requestedVersionId);
  if (value.assistant_id !== assistantId || value.active_version_id !== expectedActiveVersionId
    || value.version.version_id !== requestedVersionId || value.version.parent_version_id !== summary.parent_version_id
    || value.version.graph !== summary.graph || value.version.created_at !== summary.created_at
    || value.version.active !== summary.active || value.version.active !== (value.active_version_id === requestedVersionId)) {
    throw new StudioApiError("Assistant version evidence did not match the selected history.", 0);
  }
  return value;
}

export async function createAssistantVersion(
  assistantId: string,
  baseVersionId: string,
  fields: AssistantVersionFields,
) {
  exactVersionId(baseVersionId, "Base version");
  const { status, text } = await requestText(`/assistants/${encodeURIComponent(assistantId)}/versions`, {
    method: "POST",
    body: JSON.stringify({ base_version_id: baseVersionId, ...fields }),
  }, VERSION_RESPONSE_LIMIT);
  if (status !== 200 && status !== 201) throw new StudioApiError("Version save returned an unproven receipt.", status, true);
  try {
    const value = parseAssistantMutation(text, createVersionReceiptSchema, "Version receipt", status);
    const raw = parseLossless(text) as { version?: unknown };
    await verifyContentAddress(raw.version, value.version.version_id);
    if (value.assistant_id !== assistantId || value.active_version_id !== baseVersionId
      || value.created !== (status === 201) || value.version.parent_version_id !== baseVersionId
      || value.version.active || value.version.name !== fields.name || value.version.graph !== fields.graph
      || !jsonEquivalent(value.version.config, fields.config) || !jsonEquivalent(value.version.metadata, fields.metadata)) {
      throw new StudioApiError("Version receipt did not match the exact staged definition.", status, true);
    }
    return value;
  } catch (caught) {
    throw new StudioApiError(caught instanceof Error ? caught.message : "Version receipt was not trustworthy.", status, true);
  }
}

export async function activateAssistantVersion(
  assistantId: string,
  target: AssistantVersion,
  expectedActiveVersionId: string,
  expectedVersionCount: number,
) {
  exactVersionId(target.version_id, "Target version");
  exactVersionId(expectedActiveVersionId, "Active version");
  const { status, text } = await requestText(`/assistants/${encodeURIComponent(assistantId)}/versions/${encodeURIComponent(target.version_id)}/activate`, {
    method: "POST",
    body: JSON.stringify({ expected_active_version_id: expectedActiveVersionId }),
  }, VERSION_RESPONSE_LIMIT);
  if (status !== 200) throw new StudioApiError("Version activation returned an unproven receipt.", status, true);
  const value = parseAssistantMutation(text, activationReceiptSchema, "Activation receipt", status);
  const assistant = value.assistant;
  if (assistant.assistant_id !== assistantId || assistant.active_version_id !== target.version_id
    || assistant.version_count !== expectedVersionCount || assistant.name !== target.name || assistant.graph !== target.graph
    || !jsonEquivalent(assistant.config, target.config) || !jsonEquivalent(assistant.metadata, target.metadata)) {
    throw new StudioApiError("Activation receipt did not match the exact reviewed version.", status, true);
  }
  return value;
}

export async function setAssistantLifecycle(
  snapshot: Assistant,
  action: "archive" | "restore",
) {
  exactVersionId(snapshot.active_version_id, "Active version");
  const { status, text } = await requestText(`/assistants/${encodeURIComponent(snapshot.assistant_id)}/${action}`, {
    method: "POST",
    body: JSON.stringify({ expected_active_version_id: snapshot.active_version_id }),
  }, VERSION_RESPONSE_LIMIT);
  if (status !== 200) throw new StudioApiError("Lifecycle change returned an unproven receipt.", status, true);
  const value = parseAssistantMutation(text, lifecycleReceiptSchema, "Lifecycle receipt", status);
  const assistant = value.assistant;
  const archived = action === "archive";
  if (value.lifecycle !== (archived ? "archived" : "active") || assistant.assistant_id !== snapshot.assistant_id
    || assistant.active_version_id !== snapshot.active_version_id || assistant.version_count !== snapshot.version_count
    || assistant.created_at !== snapshot.created_at || assistant.name !== snapshot.name || assistant.graph !== snapshot.graph
    || !jsonEquivalent(assistant.config, snapshot.config) || !jsonEquivalent(assistant.metadata, snapshot.metadata)
    || archived !== Boolean(assistant.archived_at)) {
    throw new StudioApiError("Lifecycle receipt did not match the exact reviewed agent.", status, true);
  }
  return value;
}
