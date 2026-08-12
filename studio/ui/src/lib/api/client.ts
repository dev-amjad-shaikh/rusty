import { isLosslessNumber, parse as parseLossless, stringify as stringifyLossless } from "lossless-json";
import { z } from "zod";
import {
  assistantCatalogSchema,
  assistantSchema,
  runEvidenceSchema,
  runReceiptSchema,
  runSnapshotSchema,
  serverInfoSchema,
  threadSchema,
  type Assistant,
  type RunEvidence,
  type RunEvent,
  type RunReceipt,
  type RunSnapshot,
  type ServerInfo,
  type Thread,
} from "../contracts";
import { evidencePreview, isUnicodeScalarString } from "../text";

const DEFAULT_LIMIT = 8 * 1024 * 1024;

export interface ConnectionIdentity {
  epoch: number;
  origin: string;
  apiKey: string;
  tenantFingerprint: string;
}

export function connectionScope(connection: ConnectionIdentity) {
  return `${connection.epoch}|${connection.origin}|${connection.tenantFingerprint}`;
}

export function mutationScope(connection: ConnectionIdentity) {
  return JSON.stringify([connection.origin.replace(/\/$/, ""), connection.tenantFingerprint]);
}

export class StudioApiError extends Error {
  constructor(
    message: string,
    readonly status: number,
    readonly mayHaveCommitted = false,
  ) {
    super(message);
    this.name = "StudioApiError";
  }
}

export function endpoint(connection: ConnectionIdentity, path: string) {
  const normalized = connection.origin.replace(/\/$/, "");
  if (import.meta.env.DEV && normalized === "http://127.0.0.1:8100") return `/api${path}`;
  return `${normalized}${path}`;
}

async function readBounded(response: Response, maxBytes = DEFAULT_LIMIT) {
  const reader = response.body?.getReader();
  if (!reader) return "";
  const chunks: Uint8Array[] = [];
  let total = 0;
  while (true) {
    const { done, value } = await reader.read();
    if (done) break;
    total += value.byteLength;
    if (total > maxBytes) {
      await reader.cancel();
      throw new StudioApiError(`Response exceeded the ${Math.floor(maxBytes / 1024)} KiB safety boundary.`, response.status);
    }
    chunks.push(value);
  }
  const bytes = new Uint8Array(total);
  let offset = 0;
  for (const chunk of chunks) { bytes.set(chunk, offset); offset += chunk.byteLength; }
  try {
    return new TextDecoder("utf-8", { fatal: true }).decode(bytes);
  } catch {
    throw new StudioApiError("The server returned invalid UTF-8.", response.status);
  }
}

function errorMessage(text: string, status: number) {
  try {
    const value = JSON.parse(text) as { message?: unknown; error?: unknown };
    if (typeof value.message === "string") return value.message.slice(0, 2_000);
    if (typeof value.error === "string") return value.error.slice(0, 2_000);
  } catch { /* use status fallback */ }
  return `Rusty returned HTTP ${status}.`;
}

export async function requestText(
  connection: ConnectionIdentity,
  path: string,
  init: RequestInit = {},
  maxBytes = DEFAULT_LIMIT,
) {
  let response: Response;
  try {
    response = await fetch(endpoint(connection, path), {
      ...init,
      headers: {
        Accept: "application/json",
        ...(init.body ? { "Content-Type": "application/json" } : {}),
        ...(connection.apiKey ? { "X-Api-Key": connection.apiKey } : {}),
        ...init.headers,
      },
    });
  } catch {
    throw new StudioApiError("Rusty could not be reached.", 0, init.method !== undefined && init.method !== "GET");
  }
  const text = await readBounded(response, maxBytes);
  if (!response.ok) throw new StudioApiError(errorMessage(text, response.status), response.status, response.status >= 500 || response.status === 408);
  return { status: response.status, text };
}

export function parseJson<T>(text: string, schema: z.ZodType<T>, context: string): T {
  try {
    return schema.parse(JSON.parse(text));
  } catch (error) {
    const reason = error instanceof z.ZodError ? error.issues[0]?.message : "invalid JSON";
    throw new StudioApiError(`${context} did not match the Rusty contract (${reason}).`, 0);
  }
}

export function parseMutationJson<T>(text: string, schema: z.ZodType<T>, context: string, status: number): T {
  try { return parseJson(text, schema, context); }
  catch (caught) {
    throw new StudioApiError(caught instanceof Error ? caught.message : `${context} was not trustworthy.`, status, true);
  }
}

export async function getServerInfo(connection: ConnectionIdentity): Promise<ServerInfo> {
  const { text } = await requestText(connection, "/info", {}, 512 * 1024);
  return parseJson(text, serverInfoSchema, "Server information");
}

export async function listAssistants(connection: ConnectionIdentity): Promise<Assistant[]> {
  const { text } = await requestText(connection, "/assistants");
  return parseJson(text, assistantCatalogSchema, "Assistant catalog");
}

export interface CreateAssistantInput {
  assistant_id: string;
  name: string;
  graph: string;
  config: unknown;
  metadata: unknown;
}

export async function createAssistant(connection: ConnectionIdentity, input: CreateAssistantInput): Promise<Assistant> {
  const { status, text } = await requestText(connection, "/assistants", { method: "POST", body: JSON.stringify(input) });
  if (status !== 201) throw new StudioApiError("Assistant creation returned an unproven receipt.", status, true);
  const assistant = parseMutationJson(text, assistantSchema, "Assistant receipt", status);
  if (assistant.assistant_id !== input.assistant_id || assistant.name !== input.name || assistant.graph !== input.graph
    || !jsonEquivalent(assistant.config, input.config) || !jsonEquivalent(assistant.metadata, input.metadata)) {
    throw new StudioApiError("Assistant receipt did not match the exact reviewed agent.", status, true);
  }
  return assistant;
}

export function jsonEquivalent(left: unknown, right: unknown): boolean {
  if (Object.is(left, right)) return true;
  if (Array.isArray(left) || Array.isArray(right)) {
    return Array.isArray(left) && Array.isArray(right) && left.length === right.length
      && left.every((value, index) => jsonEquivalent(value, right[index]));
  }
  if (!left || !right || typeof left !== "object" || typeof right !== "object") return false;
  const leftRecord = left as Record<string, unknown>, rightRecord = right as Record<string, unknown>;
  const leftKeys = Object.keys(leftRecord).sort(), rightKeys = Object.keys(rightRecord).sort();
  return leftKeys.length === rightKeys.length && leftKeys.every((key, index) => key === rightKeys[index]
    && jsonEquivalent(leftRecord[key], rightRecord[key]));
}

export async function createThread(connection: ConnectionIdentity, graph: string, assistantId: string): Promise<Thread> {
  const { status, text } = await requestText(connection, "/threads", {
    method: "POST",
    body: JSON.stringify({ graph, metadata: { assistant_id: assistantId } }),
  }, 512 * 1024);
  if (status !== 201) throw new StudioApiError("Thread creation returned an unproven receipt.", status, true);
  const thread = parseMutationJson(text, threadSchema, "Thread receipt", status);
  if (thread.graph !== graph || !jsonEquivalent(thread.metadata, { assistant_id: assistantId })) {
    throw new StudioApiError("Thread receipt did not match the exact reviewed agent and behavior.", status, true);
  }
  return thread;
}

export async function startRun(
  connection: ConnectionIdentity,
  thread: Thread,
  assistantId: string,
  objective: string,
): Promise<RunReceipt> {
  const { status, text } = await requestText(connection, `/threads/${encodeURIComponent(thread.thread_id)}/runs`, {
    method: "POST",
    body: JSON.stringify({
      input: { objective },
      assistant_id: assistantId,
      multitask_strategy: "reject",
      metadata: { studio: { objective } },
    }),
  }, 512 * 1024);
  if (status !== 202) throw new StudioApiError("Run submission returned an unproven receipt.", status, true);
  const receipt = parseMutationJson(text, runReceiptSchema, "Run receipt", status);
  if (receipt.thread_id !== thread.thread_id) throw new StudioApiError("Run receipt named a different thread.", status, true);
  return receipt;
}

export async function getRun(connection: ConnectionIdentity, runId: string): Promise<RunSnapshot> {
  const { text } = await requestText(connection, `/runs/${encodeURIComponent(runId)}`);
  return parseJson(text, runSnapshotSchema, "Run status");
}

function rawNumber(value: unknown): string | null {
  return value === null || value === undefined ? null : isLosslessNumber(value) ? value.toString() : typeof value === "number" ? String(value) : null;
}

function rawUsage(value: unknown) {
  if (!value || typeof value !== "object") return null;
  const usage = value as Record<string, unknown>;
  const prompt = rawNumber(usage.prompt_tokens), completion = rawNumber(usage.completion_tokens), total = rawNumber(usage.total_tokens);
  return prompt !== null && completion !== null && total !== null ? { prompt_tokens: prompt, completion_tokens: completion, total_tokens: total } : null;
}

function parseRunEvidence(text: string): RunEvidence {
  let normal: unknown;
  let lossless: unknown;
  try {
    normal = JSON.parse(text);
    lossless = parseLossless(text);
  } catch {
    throw new StudioApiError("Run evidence was not valid JSON.", 0);
  }
  const envelope = normal as { run_id?: unknown; complete?: unknown; events?: unknown[] };
  const rawEnvelope = lossless as { events?: unknown[] };
  if (!Array.isArray(envelope.events) || !Array.isArray(rawEnvelope.events) || envelope.events.length !== rawEnvelope.events.length) {
    throw new StudioApiError("Run evidence did not contain one exact event array.", 0);
  }
  const events = envelope.events.map((event, index) => {
    const value = event as Record<string, unknown>;
    const raw = rawEnvelope.events![index] as Record<string, unknown>;
    return {
      ...value,
      seq: rawNumber(raw.seq),
      latency_ms: rawNumber(raw.latency_ms),
      tokens: rawUsage(raw.tokens),
      cost_usd: rawNumber(raw.cost_usd),
      rawJson: stringifyLossless(raw),
    };
  });
  return runEvidenceSchema.parse({ run_id: envelope.run_id, complete: envelope.complete, events });
}

export async function getRunEvidence(connection: ConnectionIdentity, runId: string): Promise<RunEvidence> {
  const { text } = await requestText(connection, `/runs/${encodeURIComponent(runId)}/events`);
  const evidence = parseRunEvidence(text);
  if (evidence.run_id !== runId || evidence.events.some((event: RunEvent) => event.run_id !== runId)) {
    throw new StudioApiError("Run evidence crossed run identity.", 0);
  }
  const threadIds = new Set(evidence.events.map((event) => event.thread_id));
  if (threadIds.size > 1) throw new StudioApiError("Run evidence crossed thread identity.", 0);
  const seqs = evidence.events.map((event) => BigInt(event.seq));
  if (seqs.some((seq, index) => seq !== BigInt(index))) throw new StudioApiError("Run evidence sequence was incomplete.", 0);
  const seen = new Set<string>();
  for (const event of evidence.events) {
    if (event.id !== `${runId}:${event.seq}` || seen.has(event.id)) {
      throw new StudioApiError("Run evidence event identity was invalid.", 0);
    }
    if (event.parent !== null && !seen.has(event.parent)) {
      throw new StudioApiError("Run evidence causal order was invalid.", 0);
    }
    seen.add(event.id);
  }
  return evidence;
}

const operationTaskSchema = z.object({
  task_id: z.string().min(1),
  kind: z.string().min(1),
  pool: z.string().min(1),
  status: z.enum(["failed", "dead"]),
  last_error: z.string().nullable(),
  next_attempt_at: z.string().datetime({ offset: true }).nullable(),
  run_id: z.string().max(256).nullable(),
  thread_id: z.string().max(256).nullable(),
  updated_at: z.string().datetime({ offset: true }),
});
const operationTasksSchema = z.array(operationTaskSchema);
const cronSummarySchema = z.array(z.object({ cron_id: z.string().min(1) }));
const triggerSummarySchema = z.array(z.object({ trigger_id: z.string().min(1), enabled: z.boolean() }));

export interface OperationAttentionItem {
  id: string;
  source: "task";
  title: string;
  detail: string;
  observedAt: string;
  runId: string | null;
  threadId: string | null;
  retryScheduled: boolean;
}

export interface OperationsSnapshot {
  attention: OperationAttentionItem[];
  systems: {
    tasks: number | null;
    automations: number | null;
    schedules: number | null;
  };
  unavailable: string[];
}

async function projected<T>(connection: ConnectionIdentity, path: string, schema: z.ZodType<T>, context: string) {
  const { text } = await requestText(connection, path);
  return parseJson(text, schema, context);
}

export async function getOperationsSnapshot(connection: ConnectionIdentity): Promise<OperationsSnapshot> {
  const [deadResult, failedResult, schedulesResult, triggersResult] = await Promise.allSettled([
    projected(connection, "/tasks?status=dead", operationTasksSchema, "Dead-letter tasks"),
    projected(connection, "/tasks?status=failed", operationTasksSchema, "Failed tasks"),
    projected(connection, "/crons", cronSummarySchema, "Schedule catalog"),
    projected(connection, "/triggers", triggerSummarySchema, "Automation catalog"),
  ]);
  const unavailable: string[] = [];
  if (deadResult.status === "rejected" || failedResult.status === "rejected") unavailable.push("task queue");
  if (schedulesResult.status === "rejected") unavailable.push("schedules");
  if (triggersResult.status === "rejected") unavailable.push("automations");
  const dead = deadResult.status === "fulfilled" ? deadResult.value : [];
  const failed = failedResult.status === "fulfilled" ? failedResult.value : [];
  const items = [...dead, ...failed.filter((task) => !task.next_attempt_at)]
    .sort((a, b) => b.updated_at.localeCompare(a.updated_at))
    .slice(0, 100)
    .map((task): OperationAttentionItem => ({
      id: task.task_id,
      source: "task",
      title: task.status === "dead" ? `${evidencePreview(task.kind, 160)} exhausted its retries` : `${evidencePreview(task.kind, 160)} stopped`,
      detail: task.last_error ? evidencePreview(task.last_error, 500) : `Task in ${evidencePreview(task.pool, 160)} needs review.`,
      observedAt: task.updated_at,
      runId: task.run_id,
      threadId: task.thread_id,
      retryScheduled: Boolean(task.next_attempt_at),
    }));
  return {
    attention: items,
    systems: {
      tasks: deadResult.status === "fulfilled" && failedResult.status === "fulfilled" ? dead.length + failed.length : null,
      automations: triggersResult.status === "fulfilled" ? triggersResult.value.length : null,
      schedules: schedulesResult.status === "fulfilled" ? schedulesResult.value.length : null,
    },
    unavailable,
  };
}

const provenanceSchema = z.discriminatedUnion("type", [
  z.object({ type: z.literal("human"), human_id: z.string().min(1) }).strict(),
  z.object({ type: z.literal("agent"), agent_id: z.string().min(1) }).strict(),
  z.object({ type: z.literal("distiller"), name: z.string().min(1) }).strict(),
  z.object({ type: z.literal("system") }).strict(),
]);
const promptCommitSchema = z.object({
  candidate_id: z.string().regex(/^[0-9a-f]{64}$/),
  committed_at: z.string().datetime({ offset: true }),
});
const promptArtifactSchema = z.object({
  surface: z.string().startsWith("prompt:"),
  family: z.literal("prompt"),
  owner: provenanceSchema,
  commits: z.array(promptCommitSchema).optional().default([]),
  created_at: z.string().datetime({ offset: true }),
}).strict();
const promptArtifactsSchema = z.object({ artifacts: z.array(promptArtifactSchema) }).strict();
const promptHistorySchema = z.object({
  surface: z.string().startsWith("prompt:"),
  family: z.literal("prompt"),
  owner: provenanceSchema,
  commits: z.array(z.object({
    candidate_id: z.string().regex(/^[0-9a-f]{64}$/),
    committed_at: z.string().datetime({ offset: true }),
    author: provenanceSchema.nullable(),
    status: z.enum(["created", "evaluated", "promoted", "rolled_back"]).nullable(),
  }).strict()),
}).strict().superRefine((value, context) => {
  value.commits.forEach((commit, index) => {
    if ((commit.author === null) !== (commit.status === null)) context.addIssue({ code: "custom", path: ["commits", index], message: "candidate join was partial" });
  });
});
const promptCandidateSchema = z.object({
  candidate: z.object({
    candidate_id: z.string().regex(/^[0-9a-f]{64}$/),
    content: z.object({ kind: z.literal("prompt"), name: z.string(), prompt: z.string() }).strict(),
    distilled_by: provenanceSchema,
    evidence: z.object({ run_ids: z.array(z.string()).optional(), correction_ids: z.array(z.string()).optional(), memory_ids: z.array(z.string()).optional() }).strict().optional(),
    created_at: z.string().datetime({ offset: true }),
  }).strict(),
  status: z.enum(["created", "evaluated", "promoted", "rolled_back"]),
}).passthrough();

export type PromptArtifact = z.infer<typeof promptArtifactSchema>;
export type PromptHistory = z.infer<typeof promptHistorySchema>;
export type PromptCandidateRecord = z.infer<typeof promptCandidateSchema>;

export async function listPromptArtifacts(connection: ConnectionIdentity) {
  const { text } = await requestText(connection, "/registry/artifacts?family=prompt");
  return parseJson(text, promptArtifactsSchema, "Prompt library").artifacts;
}

function promptPath(name: string) { return encodeURIComponent(name); }

export async function getPromptHistory(connection: ConnectionIdentity, name: string) {
  const { text } = await requestText(connection, `/registry/artifacts/prompt/${promptPath(name)}/commits`);
  const history = parseJson(text, promptHistorySchema, "Prompt history");
  if (history.surface !== `prompt:${name}`) throw new StudioApiError("Prompt history named a different artifact.", 0);
  return history;
}

export async function getPromptCandidate(connection: ConnectionIdentity, candidateId: string) {
  const { text } = await requestText(connection, `/learn/candidates/${encodeURIComponent(candidateId)}`);
  const record = parseJson(text, promptCandidateSchema, "Prompt version");
  if (record.candidate.candidate_id !== candidateId) throw new StudioApiError("Prompt version named a different candidate.", 0);
  return record;
}

export interface SavePromptVersionInput { name: string; prompt: string; humanId: string; runId: string; artifactExists: boolean; }

async function sha256(value: string) {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(value));
  return Array.from(new Uint8Array(digest), (byte) => byte.toString(16).padStart(2, "0")).join("");
}

export async function savePromptVersion(connection: ConnectionIdentity, input: SavePromptVersionInput) {
  if (![input.name, input.prompt, input.humanId, input.runId].every(isUnicodeScalarString)) {
    throw new StudioApiError("Prompt version input contained invalid Unicode.", 0);
  }
  const content = { kind: "prompt" as const, name: input.name, prompt: input.prompt };
  const candidateId = await sha256(JSON.stringify(content));
  const createdAt = new Date().toISOString();
  if (!input.artifactExists) {
    const declaration = await requestText(connection, "/registry/artifacts", { method: "POST", body: JSON.stringify({ family: "prompt", name: input.name, owner: { type: "human", human_id: input.humanId } }) }, 512 * 1024);
    if (![200, 201].includes(declaration.status)) throw new StudioApiError("Prompt declaration returned an unproven receipt.", declaration.status, true);
    const declarationSchema = z.object({ surface: z.string(), created: z.boolean(), artifact: promptArtifactSchema }).strict();
    const receipt = parseMutationJson(declaration.text, declarationSchema, "Prompt declaration receipt", declaration.status);
    if (receipt.surface !== `prompt:${input.name}` || receipt.artifact.surface !== receipt.surface || receipt.created !== (declaration.status === 201)
      || !jsonEquivalent(receipt.artifact.owner, { type: "human", human_id: input.humanId })) throw new StudioApiError("Prompt declaration receipt did not match the mutation status or owner.", declaration.status, true);
  }
  const candidate = { candidate_id: candidateId, content, distilled_by: { type: "human", human_id: input.humanId }, evidence: { run_ids: [input.runId] }, created_at: createdAt };
  const creation = await requestText(connection, "/learn/candidates", { method: "POST", body: JSON.stringify({ candidate, run_id: input.runId }) }, 1024 * 1024);
  if (![200, 201].includes(creation.status)) throw new StudioApiError("Prompt version returned an unproven receipt.", creation.status, true);
  const creationSchema = z.object({ candidate_id: z.string(), created: z.boolean(), record: promptCandidateSchema }).strict();
  const created = parseMutationJson(creation.text, creationSchema, "Prompt version receipt", creation.status);
  if (created.candidate_id !== candidateId || created.record.candidate.candidate_id !== candidateId || created.record.candidate.content.name !== input.name || created.record.candidate.content.prompt !== input.prompt || created.created !== (creation.status === 201)
    || !jsonEquivalent(created.record.candidate.distilled_by, { type: "human", human_id: input.humanId })
    || !jsonEquivalent(created.record.candidate.evidence?.run_ids, [input.runId])) throw new StudioApiError("Prompt version receipt did not match the reviewed draft, evidence, author, or mutation status.", creation.status, true);
  const commit = await requestText(connection, `/registry/artifacts/prompt/${promptPath(input.name)}/commits`, { method: "POST", body: JSON.stringify({ candidate_id: candidateId }) }, 512 * 1024);
  if (commit.status !== 200) throw new StudioApiError("Prompt commit returned an unproven receipt.", commit.status, true);
  const commitSchema = z.union([
    z.object({ surface: z.string(), committed: z.literal(true), commit: promptCommitSchema, commits: z.number().int().positive() }).strict(),
    z.object({ surface: z.string(), committed: z.literal(false), commits: z.number().int().nonnegative() }).strict(),
  ]);
  const committed = parseMutationJson(commit.text, commitSchema, "Prompt commit receipt", commit.status);
  if (committed.surface !== `prompt:${input.name}` || (committed.committed && committed.commit.candidate_id !== candidateId)) throw new StudioApiError("Prompt commit receipt named different evidence.", commit.status, true);
  return { candidateId, created: created.created, committed: committed.committed };
}
