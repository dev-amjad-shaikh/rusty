import { z } from "zod";
import {
  parseJson,
  parseMutationJson,
  requestText,
  StudioApiError,
} from "./client";
import { isUnicodeScalarString } from "../text";

// The knowledge plane's registration and query ceilings, mirrored from
// rusty-core so the UI can fail early and label inputs honestly.
export const KNOWLEDGE_MAX_SOURCE_BYTES = 1024 * 1024;
export const KNOWLEDGE_MAX_SOURCE_ID_BYTES = 128;
export const KNOWLEDGE_MAX_TITLE_BYTES = 512;
export const KNOWLEDGE_MAX_ATTRIBUTION_BYTES = 512;
export const KNOWLEDGE_MAX_RESULTS_CEILING = 100;
export const KNOWLEDGE_MAX_RESULT_BYTES_CEILING = 1024 * 1024;
export const KNOWLEDGE_DEFAULT_MAX_RESULTS = 20;
export const KNOWLEDGE_DEFAULT_MAX_RESULT_BYTES = 64 * 1024;

const timestampSchema = z.string().datetime({ offset: true });
const hashSchema = z.string().regex(/^[0-9a-f]{64}$/);

export const knowledgeSourceKindSchema = z.enum(["text", "markdown", "json", "csv"]);
export type KnowledgeSourceKind = z.infer<typeof knowledgeSourceKindSchema>;

export const knowledgeRetentionSchema = z.discriminatedUnion("policy", [
  z.object({ policy: z.literal("ttl"), expires_at: timestampSchema }),
  z.object({ policy: z.literal("pinned") }),
]);
export type KnowledgeRetention = z.infer<typeof knowledgeRetentionSchema>;

const scopeAddressSchema = z.object({
  scope: z.enum(["run", "agent", "team", "user", "tenant"]),
  id: z.string().min(1),
});
export type KnowledgeScopeAddress = z.infer<typeof scopeAddressSchema>;

const knowledgeSourceSchema = z.object({
  source_id: z.string().min(1),
  scope: scopeAddressSchema,
  kind: knowledgeSourceKindSchema,
  title: z.string().min(1),
  author: z.string().min(1),
  confidence: z.number().gt(0).lte(1),
  created_at: timestampSchema,
  retention: knowledgeRetentionSchema,
  content_hash: hashSchema,
  body_hash: hashSchema,
  content_bytes: z.number().int().nonnegative(),
  version: z.number().int().positive(),
  supersedes: hashSchema.optional(),
});
export type KnowledgeSource = z.infer<typeof knowledgeSourceSchema>;

const listedSourceSchema = knowledgeSourceSchema.omit({ body_hash: true }).extend({
  chunk_count: z.number().int().nonnegative(),
});
export type ListedKnowledgeSource = z.infer<typeof listedSourceSchema>;

const sourceTombstoneSchema = z.object({
  source_id: z.string().min(1),
  scope: scopeAddressSchema,
  title: z.string().min(1),
  purged_hashes: z.array(hashSchema),
  reason: z.enum(["expired"]),
  purged_at: timestampSchema,
});
export type KnowledgeSourceTombstone = z.infer<typeof sourceTombstoneSchema>;

const chunkRecordSchema = z.object({
  chunk_id: z.string().min(1),
  source_id: z.string().min(1),
  source_hash: hashSchema,
  chunk_index: z.number().int().nonnegative(),
  byte_start: z.number().int().nonnegative(),
  byte_end: z.number().int().nonnegative(),
  content_address: hashSchema,
  bytes: z.number().int().nonnegative(),
  word_count: z.number().int().nonnegative(),
});
export type KnowledgeChunkRecord = z.infer<typeof chunkRecordSchema>;

const citationSchema = z.object({
  source_id: z.string().min(1),
  source_hash: hashSchema,
  title: z.string().min(1),
  chunk_id: z.string().min(1),
  chunk_index: z.number().int().nonnegative(),
  content_address: hashSchema,
  byte_start: z.number().int().nonnegative(),
  byte_end: z.number().int().nonnegative(),
});
export type KnowledgeCitation = z.infer<typeof citationSchema>;

const chunkFetchSchema = z.object({
  citation: citationSchema,
  text: z.string(),
  word_count: z.number().int().nonnegative(),
});
export type KnowledgeChunkFetch = z.infer<typeof chunkFetchSchema>;

const citedChunkSchema = z.object({
  citation: citationSchema,
  text: z.string(),
  score: z.number(),
  word_count: z.number().int().nonnegative(),
});
export type KnowledgeCitedChunk = z.infer<typeof citedChunkSchema>;

const librarySchema = z.object({
  sources: z.array(listedSourceSchema),
  tombstones: z.array(sourceTombstoneSchema),
});
export type KnowledgeLibrary = z.infer<typeof librarySchema>;

const sourceDetailSchema = z.union([
  z.object({
    source: knowledgeSourceSchema,
    versions: z.number().int().positive(),
    chunks: z.array(chunkRecordSchema),
  }),
  z.object({ tombstone: sourceTombstoneSchema }),
]);
export type KnowledgeSourceDetail = z.infer<typeof sourceDetailSchema>;

const registerReceiptSchema = z.object({
  source_id: z.string().min(1),
  content_hash: hashSchema,
  version: z.number().int().positive(),
  chunk_count: z.number().int().nonnegative(),
  created: z.boolean(),
});
export type KnowledgeRegisterReceipt = z.infer<typeof registerReceiptSchema>;

const correctionReceiptSchema = z.object({
  source_id: z.string().min(1),
  content_hash: hashSchema,
  version: z.number().int().positive(),
  supersedes: hashSchema.nullable().optional(),
  chunk_count: z.number().int().nonnegative(),
});
export type KnowledgeCorrectionReceipt = z.infer<typeof correctionReceiptSchema>;

const queryResponseSchema = z.object({
  query: z.string(),
  results: z.array(citedChunkSchema),
});
export type KnowledgeQueryResponse = z.infer<typeof queryResponseSchema>;

const purgeEntrySchema = z.object({
  source_id: z.string().min(1),
  source_hash: hashSchema,
  body_hash: hashSchema,
  scope: scopeAddressSchema,
  title: z.string().min(1),
  version: z.number().int().positive(),
  expires_at: timestampSchema,
  chunk_count: z.number().int().nonnegative(),
  chunk_bytes: z.number().int().nonnegative(),
});
export type KnowledgePurgeEntry = z.infer<typeof purgeEntrySchema>;

const retentionPlanSchema = z.object({
  entries: z.array(purgeEntrySchema),
  total_chunk_bytes: z.number().int().nonnegative(),
});
export type KnowledgeRetentionPlan = z.infer<typeof retentionPlanSchema>;

const retentionReceiptSchema = z.object({
  plan: retentionPlanSchema,
  tombstones: z.array(sourceTombstoneSchema),
});
export type KnowledgeRetentionReceipt = z.infer<typeof retentionReceiptSchema>;

function sourcePath(sourceId: string) {
  return `/knowledge/sources/${encodeURIComponent(sourceId)}`;
}

export async function listKnowledgeSources(): Promise<KnowledgeLibrary> {
  const { text } = await requestText("/knowledge/sources");
  return parseJson(text, librarySchema, "Knowledge library");
}

export async function getKnowledgeSource(sourceId: string): Promise<KnowledgeSourceDetail> {
  const { text } = await requestText(sourcePath(sourceId));
  const detail = parseJson(text, sourceDetailSchema, "Knowledge source");
  if ("source" in detail && detail.source.source_id !== sourceId) {
    throw new StudioApiError("Knowledge source named a different source.", 0);
  }
  if ("tombstone" in detail && detail.tombstone.source_id !== sourceId) {
    throw new StudioApiError("Knowledge tombstone named a different source.", 0);
  }
  return detail;
}

export async function getKnowledgeChunk(
  sourceId: string,
  chunkId: string | number,
  versionHash?: string,
): Promise<KnowledgeChunkFetch> {
  const pin = versionHash ? `?version=${encodeURIComponent(versionHash)}` : "";
  const { text } = await requestText(
    `${sourcePath(sourceId)}/chunks/${encodeURIComponent(String(chunkId))}${pin}`,
  );
  const chunk = parseJson(text, chunkFetchSchema, "Knowledge chunk");
  if (chunk.citation.source_id !== sourceId) {
    throw new StudioApiError("Knowledge chunk named a different source.", 0);
  }
  if (versionHash && chunk.citation.source_hash !== versionHash) {
    throw new StudioApiError("Knowledge chunk named a different version than the pinned one.", 0);
  }
  return chunk;
}

export interface RegisterKnowledgeSourceInput {
  source_id: string;
  kind: KnowledgeSourceKind;
  title: string;
  author: string;
  body: string;
  confidence?: number;
  retention?: KnowledgeRetention;
}

export async function registerKnowledgeSource(
  input: RegisterKnowledgeSourceInput,
): Promise<KnowledgeRegisterReceipt> {
  if (![input.source_id, input.title, input.author, input.body].every(isUnicodeScalarString)) {
    throw new StudioApiError("Knowledge source input contained invalid Unicode.", 0);
  }
  const payload: Record<string, unknown> = {
    source_id: input.source_id,
    kind: input.kind,
    title: input.title,
    author: input.author,
    body: input.body,
  };
  if (input.confidence !== undefined) payload.confidence = input.confidence;
  if (input.retention !== undefined) payload.retention = input.retention;
  const { status, text } = await requestText("/knowledge/sources", {
    method: "POST",
    body: JSON.stringify(payload),
  });
  if (![200, 201].includes(status)) {
    throw new StudioApiError("Source registration returned an unproven receipt.", status, true);
  }
  const receipt = parseMutationJson(text, registerReceiptSchema, "Source registration receipt", status);
  if (receipt.source_id !== input.source_id || receipt.created !== (status === 201)) {
    throw new StudioApiError("Source registration receipt did not match the reviewed source.", status, true);
  }
  return receipt;
}

export async function correctKnowledgeSource(
  sourceId: string,
  author: string,
  body: string,
): Promise<KnowledgeCorrectionReceipt> {
  if (![author, body].every(isUnicodeScalarString)) {
    throw new StudioApiError("Correction input contained invalid Unicode.", 0);
  }
  const { status, text } = await requestText(`${sourcePath(sourceId)}/correct`, {
    method: "POST",
    body: JSON.stringify({ author, body }),
  });
  if (status !== 201) {
    throw new StudioApiError("Correction returned an unproven receipt.", status, true);
  }
  const receipt = parseMutationJson(text, correctionReceiptSchema, "Correction receipt", status);
  if (receipt.source_id !== sourceId) {
    throw new StudioApiError("Correction receipt named a different source.", status, true);
  }
  return receipt;
}

export interface KnowledgeQueryLimits {
  max_results?: number;
  max_bytes?: number;
}

export async function queryKnowledge(
  textQuery: string,
  limits?: KnowledgeQueryLimits,
): Promise<KnowledgeQueryResponse> {
  if (!isUnicodeScalarString(textQuery)) {
    throw new StudioApiError("Query text contained invalid Unicode.", 0);
  }
  const payload: Record<string, unknown> = { text: textQuery };
  if (limits && (limits.max_results !== undefined || limits.max_bytes !== undefined)) {
    payload.limits = limits;
  }
  const { text } = await requestText("/knowledge/query", {
    method: "POST",
    body: JSON.stringify(payload),
  });
  return parseJson(text, queryResponseSchema, "Knowledge query");
}

export async function planKnowledgeRetention(
  asOf?: string,
): Promise<KnowledgeRetentionPlan> {
  const { text } = await requestText("/knowledge/retention/plan", {
    method: "POST",
    body: JSON.stringify(asOf ? { as_of: asOf } : {}),
  });
  return parseJson(text, retentionPlanSchema, "Retention plan");
}

export async function applyKnowledgeRetention(
  asOf?: string,
): Promise<KnowledgeRetentionReceipt> {
  const { status, text } = await requestText("/knowledge/retention/apply", {
    method: "POST",
    body: JSON.stringify(asOf ? { as_of: asOf } : {}),
  });
  return parseMutationJson(text, retentionReceiptSchema, "Retention receipt", status);
}
