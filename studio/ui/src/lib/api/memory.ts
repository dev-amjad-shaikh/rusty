import { z } from "zod";
import { jsonEquivalent, parseJson, parseMutationJson, requestText, StudioApiError } from "./client";
import { isUnicodeScalarString } from "../text";

const instant = z.string().datetime({ offset: true });
const label = z.string().min(1).max(256);
const contentAddress = z.string().regex(/^[0-9a-f]{64}$/);

const memoryScopeSchema = z.enum(["run", "agent", "team", "user", "tenant"]);
const scopeAddressSchema = z.object({
  scope: memoryScopeSchema,
  id: label,
}).strict();

const provenanceAuthorSchema = z.discriminatedUnion("type", [
  z.object({ type: z.literal("agent"), agent_id: label }).strict(),
  z.object({ type: z.literal("human"), human_id: label }).strict(),
  z.object({ type: z.literal("distiller"), name: label }).strict(),
  z.object({ type: z.literal("system") }).strict(),
]);

const memoryEvidenceSchema = z.object({
  run_id: z.string().min(1).optional(),
  event_ids: z.array(z.string().min(1)).default([]),
  correction_id: z.string().min(1).optional(),
  candidate_id: z.string().min(1).optional(),
  source_memory_ids: z.array(contentAddress).default([]),
}).strict();

const validityWindowSchema = z.object({
  valid_from: instant,
  valid_until: instant.optional(),
}).strict();

const payloadRefSchema = z.discriminatedUnion("kind", [
  z.object({ kind: z.literal("inline"), value: z.unknown() }).strict(),
  z.object({
    kind: z.literal("artifact"),
    value: z.object({ sha256: contentAddress, bytes: z.number().int().nonnegative() }).strict(),
  }).strict(),
]);

export const memoryRecordSchema = z.object({
  memory_id: contentAddress,
  kind: z.enum(["fact", "preference", "example", "summary"]),
  scope: scopeAddressSchema,
  key: label.optional(),
  tags: z.array(label).default([]),
  priority: z.number().int().default(0),
  provenance: z.object({
    author: provenanceAuthorSchema,
    evidence: memoryEvidenceSchema.default({ event_ids: [], source_memory_ids: [] }),
    written_at: instant,
  }).strict(),
  confidence: z.number().gt(0).lte(1),
  validity: validityWindowSchema,
  created_at: instant,
  expires_at: instant.optional(),
  supersedes: contentAddress.optional(),
  candidacy: z.enum(["pending"]).optional(),
  content: payloadRefSchema,
  embedding: z.unknown().optional(),
}).strict();

const memoryConflictSchema = z.object({
  scope: scopeAddressSchema,
  key: label,
  memory_ids: z.array(contentAddress).length(2),
  overlap: validityWindowSchema,
}).strict();

const forgetReasonSchema = z.enum(["expired", "retracted", "erasure_request"]);

const writeReceiptSchema = z.object({
  memory_id: contentAddress,
  created: z.boolean(),
  record: memoryRecordSchema,
}).strict();

const queryResponseSchema = z.object({
  records: z.array(memoryRecordSchema),
}).strict();

const correctionReceiptSchema = z.object({
  correction_id: label,
  attribution: z.string().min(1),
  candidate: z.boolean(),
  memory_id: contentAddress,
  created: z.boolean(),
  record: memoryRecordSchema,
  superseded: contentAddress.nullable(),
  example_id: contentAddress.nullable(),
}).strict();

const conflictsResponseSchema = z.object({
  conflicts: z.array(memoryConflictSchema),
}).strict();

const forgetReceiptSchema = z.object({
  forgotten: z.array(contentAddress),
  invalidated: z.array(contentAddress),
  tombstone: z.object({
    memory_id: contentAddress,
    scope: scopeAddressSchema,
    reason: forgetReasonSchema,
    invalidated: z.array(contentAddress).default([]),
  }).strict(),
}).strict();

export type MemoryScope = z.infer<typeof memoryScopeSchema>;
export type ScopeAddress = z.infer<typeof scopeAddressSchema>;
export type ProvenanceAuthor = z.infer<typeof provenanceAuthorSchema>;
export type MemoryEvidence = z.infer<typeof memoryEvidenceSchema>;
export type MemoryRecord = z.infer<typeof memoryRecordSchema>;
export type MemoryConflict = z.infer<typeof memoryConflictSchema>;
export type ForgetReason = z.infer<typeof forgetReasonSchema>;
export type WriteMemoryReceipt = z.infer<typeof writeReceiptSchema>;
export type CorrectionReceipt = z.infer<typeof correctionReceiptSchema>;
export type ForgetReceipt = z.infer<typeof forgetReceiptSchema>;

export interface MemoryQueryInput {
  scope?: ScopeAddress;
  kinds?: MemoryRecord["kind"][];
  key?: string;
  tags?: string[];
  min_confidence?: number;
  include_expired?: boolean;
  include_superseded?: boolean;
  authored_by?: ProvenanceAuthor;
  candidates_only?: boolean;
}

export function scopeAddressText(scope: ScopeAddress) {
  return `${scope.scope}:${scope.id}`;
}

export function authorText(author: ProvenanceAuthor) {
  switch (author.type) {
    case "agent": return `agent:${author.agent_id}`;
    case "human": return `human:${author.human_id}`;
    case "distiller": return `distiller:${author.name}`;
    case "system": return "system";
  }
}

function inlineContent(record: MemoryRecord) {
  return record.content.kind === "inline" ? record.content.value : undefined;
}

function recordMatchesQuery(record: MemoryRecord, query: MemoryQueryInput) {
  if (query.scope && !jsonEquivalent(record.scope, query.scope)) return false;
  if (query.kinds?.length && !query.kinds.includes(record.kind)) return false;
  if (query.key !== undefined && record.key !== query.key) return false;
  if (query.tags?.length && !query.tags.every((tag) => record.tags.includes(tag))) return false;
  if (query.min_confidence !== undefined && record.confidence < query.min_confidence) return false;
  if (query.candidates_only && !record.candidacy) return false;
  if (query.authored_by && !jsonEquivalent(record.provenance.author, query.authored_by)) return false;
  return true;
}

export async function queryMemory(query: MemoryQueryInput): Promise<MemoryRecord[]> {
  const { text } = await requestText("/memory/query", { method: "POST", body: JSON.stringify(query) });
  const { records } = parseJson(text, queryResponseSchema, "Memory query");
  const stray = records.find((record) => !recordMatchesQuery(record, query));
  if (stray) throw new StudioApiError("Memory query returned a record outside the exact filters.", 0);
  return records;
}

export interface WriteMemoryInput {
  kind: MemoryRecord["kind"];
  scope: ScopeAddress;
  content: unknown;
  author: ProvenanceAuthor;
  key?: string;
  tags?: string[];
  confidence?: number;
  valid_until?: string;
  expires_at?: string;
}

export async function writeMemory(input: WriteMemoryInput): Promise<WriteMemoryReceipt> {
  const strings = [input.scope.id, input.key ?? "", ...(input.tags ?? [])];
  if (input.author.type === "agent") strings.push(input.author.agent_id);
  if (input.author.type === "human") strings.push(input.author.human_id);
  if (input.author.type === "distiller") strings.push(input.author.name);
  if (!strings.every(isUnicodeScalarString)) {
    throw new StudioApiError("Memory write input contained invalid Unicode.", 0);
  }
  const { status, text } = await requestText("/memory", { method: "POST", body: JSON.stringify(input) });
  if (![200, 201].includes(status)) throw new StudioApiError("Memory write returned an unproven receipt.", status, true);
  const receipt = parseMutationJson(text, writeReceiptSchema, "Memory write receipt", status);
  if (receipt.created !== (status === 201) || receipt.memory_id !== receipt.record.memory_id) {
    throw new StudioApiError("Memory write receipt did not match the mutation status or record identity.", status, true);
  }
  const record = receipt.record;
  if (record.kind !== input.kind || !jsonEquivalent(record.scope, input.scope) || !jsonEquivalent(record.provenance.author, input.author)) {
    throw new StudioApiError("Memory write receipt named a different kind, scope, or author than the reviewed write.", status, true);
  }
  const content = inlineContent(record);
  if (content === undefined || !jsonEquivalent(content, input.content)) {
    throw new StudioApiError("Memory write receipt did not carry the exact reviewed content.", status, true);
  }
  const expectedConfidence = input.confidence ?? (input.author.type === "human" ? 1 : undefined);
  if (expectedConfidence !== undefined && record.confidence !== expectedConfidence) {
    throw new StudioApiError("Memory write receipt declared a different confidence than the reviewed write.", status, true);
  }
  if (input.key !== undefined && record.key !== input.key) {
    throw new StudioApiError("Memory write receipt named a different lookup key.", status, true);
  }
  return receipt;
}

export async function getMemory(memoryId: string): Promise<MemoryRecord> {
  const { text } = await requestText(`/memory/${encodeURIComponent(memoryId)}`);
  const record = parseJson(text, memoryRecordSchema, "Memory record");
  if (record.memory_id !== memoryId) throw new StudioApiError("Memory record named a different content address.", 0);
  return record;
}

export interface CorrectionInput {
  correction_id: string;
  author: string;
  targetMemoryId: string;
  corrected: unknown;
  scope: ScopeAddress;
  rationale?: string;
}

export async function submitCorrection(input: CorrectionInput): Promise<CorrectionReceipt> {
  const body = {
    correction_id: input.correction_id,
    author: input.author,
    target: { type: "memory", memory_id: input.targetMemoryId },
    corrected: input.corrected,
    scope: input.scope,
    ...(input.rationale ? { rationale: input.rationale } : {}),
  };
  const { status, text } = await requestText("/memory/corrections", { method: "POST", body: JSON.stringify(body) });
  if (![200, 201].includes(status)) throw new StudioApiError("Correction returned an unproven receipt.", status, true);
  const receipt = parseMutationJson(text, correctionReceiptSchema, "Correction receipt", status);
  if (receipt.created !== (status === 201) || receipt.memory_id !== receipt.record.memory_id) {
    throw new StudioApiError("Correction receipt did not match the mutation status or record identity.", status, true);
  }
  if (receipt.correction_id !== input.correction_id || receipt.record.provenance.evidence.correction_id !== input.correction_id) {
    throw new StudioApiError("Correction receipt named a different correction.", status, true);
  }
  const author = receipt.record.provenance.author;
  if (author.type !== "human" || author.human_id !== input.author || !jsonEquivalent(receipt.record.scope, input.scope)) {
    throw new StudioApiError("Correction receipt carried a different attribution or scope.", status, true);
  }
  const content = inlineContent(receipt.record);
  if (content === undefined || !jsonEquivalent(content, input.corrected)) {
    throw new StudioApiError("Correction receipt did not carry the exact corrected content.", status, true);
  }
  return receipt;
}

export async function listMemoryConflicts(scope?: ScopeAddress): Promise<MemoryConflict[]> {
  const { text } = await requestText("/memory/conflicts", { method: "POST", body: JSON.stringify(scope ? { scope } : {}) });
  const { conflicts } = parseJson(text, conflictsResponseSchema, "Memory conflicts");
  if (scope && conflicts.some((conflict) => !jsonEquivalent(conflict.scope, scope))) {
    throw new StudioApiError("Memory conflicts crossed the requested scope.", 0);
  }
  return conflicts;
}

export async function forgetMemory(memoryId: string, reason: ForgetReason): Promise<ForgetReceipt> {
  const { status, text } = await requestText("/memory/forget", { method: "POST", body: JSON.stringify({ memory_id: memoryId, reason }) });
  if (status !== 200) throw new StudioApiError("Forgetting returned an unproven receipt.", status, true);
  const receipt = parseMutationJson(text, forgetReceiptSchema, "Forget receipt", status);
  if (receipt.tombstone.memory_id !== memoryId || !receipt.forgotten.includes(memoryId)) {
    throw new StudioApiError("Forget receipt named a different record than the confirmed erasure.", status, true);
  }
  return receipt;
}
