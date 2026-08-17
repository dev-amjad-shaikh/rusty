import { z } from "zod";
import { requestText, parseJson, parseMutationJson, StudioApiError } from "./client";

const id = z.string().min(1).max(256);
const instant = z.string().datetime({ offset: true });
// ProvenanceAuthor (rusty-core/src/memory.rs): tagged on `type`,
// snake_case — human/system/agent/distiller, never an open string.
const authorSchema = z.discriminatedUnion("type", [
  z.object({ type: z.literal("human"), human_id: z.string().min(1) }).strict(),
  z.object({ type: z.literal("system") }).strict(),
  z.object({ type: z.literal("agent"), agent_id: z.string().min(1) }).strict(),
  z.object({ type: z.literal("distiller"), name: z.string().min(1) }).strict(),
]).nullable();

const effectSchema = z.enum(["pure", "read_only", "idempotent", "compensatable", "non_idempotent"]);

const gateDeclarationSchema = z.object({
  policy: z.string().min(1),
  dataset_version: z.string().min(1),
}).strict().nullable();

const environmentSchema = z.object({
  name: id,
  // The server omits `gate` when none is declared (the dev floor).
  gate: gateDeclarationSchema.optional(),
  approval_required: z.boolean(),
  created_by: authorSchema,
  created_at: instant,
}).strict();

const environmentListSchema = z.object({
  environments: z.array(environmentSchema),
}).strict();

const environmentResponseSchema = z.object({
  environment: environmentSchema,
}).strict();

const registryPinSchema = z.object({
  surface: z.string().min(1),
  candidate_id: z.string().regex(/^[0-9a-f]{64}$/),
}).strict();

const revisionContentSchema = z.object({
  graph: z.string().min(1),
  graph_hash: z.string().min(1),
  // Omitted when the revision binds the graph directly.
  assistant: z.string().max(256).optional(),
  source_environment: z.string().min(1),
  pins: z.array(registryPinSchema),
}).strict();

const revisionSchema = z.object({
  revision_id: z.string().regex(/^[0-9a-f]{64}$/),
  content: revisionContentSchema,
  author: authorSchema,
  created_at: instant,
}).strict();

const revisionListSchema = z.object({
  revisions: z.array(revisionSchema),
}).strict();

// `GET /deployments/revisions/{id}` answers only the revision; the
// creation handler adds `created` (see createRevision).
const revisionResponseSchema = z.object({
  revision: revisionSchema,
}).strict();

const pointerSlotSchema = z.object({
  revision_id: z.string().regex(/^[0-9a-f]{64}$/),
  fraction: z.number().min(0).max(1),
}).strict();

// DeploymentPointer (rusty-core/src/deploy.rs): `surface` is always
// present; `active` and `canary` are omitted while their slot is empty.
const deploymentPointerSchema = z.object({
  surface: z.string().min(1),
  active: z.string().regex(/^[0-9a-f]{64}$/).nullable().optional(),
  canary: pointerSlotSchema.nullable().optional(),
}).strict();

// `GET …/pointer` answers only the pointer.
const pointerResponseSchema = z.object({
  pointer: deploymentPointerSchema,
}).strict();

// The pointer-mutation receipt (promote/rollback/canary/clear): `201`
// with the journaled move, `200 {applied: false}` for a converged
// re-issue; `event_id` rides along when an event was appended.
const pointerReceiptSchema = z.object({
  applied: z.boolean(),
  journaled: z.boolean(),
  event_id: z.string().min(1).nullable().optional(),
  pointer: deploymentPointerSchema,
}).strict();

const recentRunsSchema = z.object({
  runs: z.number().int().nonnegative(),
  errored: z.number().int().nonnegative(),
  interrupted: z.number().int().nonnegative(),
}).strict();

const environmentBoardSchema = z.object({
  environment: id,
  gate: gateDeclarationSchema,
  approval_required: z.boolean(),
  active_revision: z.string().regex(/^[0-9a-f]{64}$/).nullable(),
  canary: pointerSlotSchema.nullable(),
  last_gate_decision: z.unknown().nullable(),
  recent_runs: z.object({
    active: recentRunsSchema,
    canary: recentRunsSchema,
  }).strict(),
}).strict();

const deploymentHealthSchema = z.object({
  environments: z.array(environmentBoardSchema),
  deployment_chain_head: z.string().max(128).nullable(),
}).strict();

// RunEvent (rusty-core/src/record.rs): `seq` is a u64, `thread_id` is
// always present, and the optional measures serialize as null.
const deploymentEventSchema = z.object({
  id: id,
  run_id: id,
  thread_id: id,
  node_id: id.nullable(),
  seq: z.number().int().nonnegative(),
  kind: z.string().min(1),
  effect: effectSchema,
  input: z.unknown().nullable(),
  output: z.unknown().nullable(),
  latency_ms: z.number().nullable(),
  tokens: z.unknown().nullable(),
  cost_usd: z.number().nullable(),
  status: z.enum(["ok", "error", "interrupted"]),
  parent: id.nullable(),
  recorded_at: instant,
}).strict();

const deploymentJournalSchema = z.object({
  run_id: id,
  events: z.array(deploymentEventSchema),
  complete: z.boolean(),
}).strict();

const secretRecordSchema = z.object({
  name: id,
  environment: id,
  set_by: authorSchema,
  created_at: instant,
  // Omitted until the first rotation beneath the scoped name.
  rotated_at: instant.optional(),
}).strict();

const secretListSchema = z.object({
  secrets: z.array(secretRecordSchema),
}).strict();

const shadowRefusalSchema = z.object({
  kind: z.string().min(1),
  effect: effectSchema,
  effect_id: z.string().regex(/^[0-9a-f]{64}$/),
  input_hash: z.string().min(1),
  served: z.boolean(),
}).strict();

const shadowOutcomeSchema = z.union([
  z.literal("completed"),
  z.object({ failed: z.object({ error: z.string() }).strict() }).strict(),
]);

// `POST /deployments/shadows` answers 201 with the shadow's id and the
// journaled ShadowVerdict (rusty-core/src/deploy.rs) verbatim.
const shadowVerdictSchema = z.object({
  shadow_run_id: id,
  verdict: z.object({
    tenant: z.string().min(1),
    shadow_run_id: id,
    source_run_id: z.string().min(1),
    revision_id: z.string().regex(/^[0-9a-f]{64}$/),
    refusals: z.array(shadowRefusalSchema),
    matched: z.number().int().nonnegative(),
    unserved: z.number().int().nonnegative(),
    unrequested: z.array(z.string()),
    outcome: shadowOutcomeSchema,
    completed_at: instant,
  }).strict(),
}).strict();

export type DeploymentEnvironment = z.infer<typeof environmentSchema>;
export type DeploymentRevision = z.infer<typeof revisionSchema>;
export type DeploymentPointer = z.infer<typeof deploymentPointerSchema>;
export type EnvironmentBoard = z.infer<typeof environmentBoardSchema>;
export type DeploymentHealth = z.infer<typeof deploymentHealthSchema>;
export type DeploymentEvent = z.infer<typeof deploymentEventSchema>;
export type DeploymentJournal = z.infer<typeof deploymentJournalSchema>;
export type DeploymentSecret = z.infer<typeof secretRecordSchema>;
export type ShadowVerdict = z.infer<typeof shadowVerdictSchema>;

function parse<T>(text: string, schema: z.ZodType<T>, context: string): T {
  return parseJson(text, schema, context);
}

export async function listEnvironments(): Promise<DeploymentEnvironment[]> {
  const { text } = await requestText("/deployments/environments");
  return parse(text, environmentListSchema, "Environment list").environments;
}

export async function declareEnvironment(
  input: {
    name: string;
    gate: { policy: string; dataset_version: string } | null;
    approval_required: boolean;
    author: { type: "human"; human_id: string } | { type: "system" };
  },
): Promise<{ created: boolean; environment: DeploymentEnvironment }> {
  const { status, text } = await requestText("/deployments/environments", {
    method: "POST",
    body: JSON.stringify(input),
  });
  if (![200, 201].includes(status)) {
    throw new StudioApiError("Environment declaration returned an unproven receipt.", status, true);
  }
  const receipt = parseMutationJson(text, environmentResponseSchema.extend({ created: z.boolean() }), "Environment declaration", status);
  if (receipt.environment.name !== input.name
      || JSON.stringify(receipt.environment.gate ?? null) !== JSON.stringify(input.gate)
      || receipt.environment.approval_required !== input.approval_required) {
    throw new StudioApiError("Environment receipt did not match the reviewed declaration.", status, true);
  }
  return { created: status === 201, environment: receipt.environment };
}

export async function getEnvironment(name: string): Promise<DeploymentEnvironment> {
  const { text } = await requestText(`/deployments/environments/${encodeURIComponent(name)}`);
  return parse(text, environmentResponseSchema, `Environment ${name}`).environment;
}

export async function listRevisions(): Promise<DeploymentRevision[]> {
  const { text } = await requestText("/deployments/revisions");
  return parse(text, revisionListSchema, "Revision list").revisions;
}

export async function getRevision(revisionId: string): Promise<DeploymentRevision> {
  const { text } = await requestText(`/deployments/revisions/${encodeURIComponent(revisionId)}`);
  return parse(text, revisionResponseSchema, "Revision").revision;
}

export async function createRevision(
  input: {
    graph: string;
    source_environment: string;
    surfaces: string[];
    author: { type: "human"; human_id: string } | { type: "system" };
  },
): Promise<{ created: boolean; revision: DeploymentRevision }> {
  const { status, text } = await requestText("/deployments/revisions", {
    method: "POST",
    body: JSON.stringify(input),
  }, 512 * 1024);
  if (![200, 201].includes(status)) {
    throw new StudioApiError("Revision creation returned an unproven receipt.", status, true);
  }
  const receipt = parseMutationJson(text, revisionResponseSchema.extend({ created: z.boolean() }), "Revision receipt", status);
  if (receipt.revision.content.graph !== input.graph || receipt.revision.content.source_environment !== input.source_environment
      || receipt.revision.content.pins.length !== input.surfaces.length
      || !receipt.revision.content.pins.every((pin: { surface: string }) => input.surfaces.includes(pin.surface))) {
    throw new StudioApiError("Revision receipt did not match the reviewed pin set.", status, true);
  }
  return { created: status === 201, revision: receipt.revision };
}

export async function getDeploymentHealth(): Promise<DeploymentHealth> {
  const { text } = await requestText("/deployments/health");
  return parse(text, deploymentHealthSchema, "Deployment health");
}

export async function getEnvironmentPointer(name: string): Promise<DeploymentPointer> {
  const { text } = await requestText(`/deployments/environments/${encodeURIComponent(name)}/pointer`);
  return parse(text, pointerResponseSchema, "Deployment pointer").pointer;
}

export async function promoteRevision(
  environment: string,
  input: {
    revision_id: string;
    author: { type: "human"; human_id: string } | { type: "system" };
  },
): Promise<{ pointer: DeploymentPointer }> {
  const { status, text } = await requestText(
    `/deployments/environments/${encodeURIComponent(environment)}/promote`,
    { method: "POST", body: JSON.stringify(input) },
    512 * 1024,
  );
  if (![200, 201].includes(status)) throw new StudioApiError("Promotion returned an unproven receipt.", status, true);
  const receipt = parseMutationJson(text, pointerReceiptSchema, "Promotion receipt", status);
  if (receipt.pointer.active !== input.revision_id) {
    throw new StudioApiError("Promotion receipt did not name the reviewed revision as active.", status, true);
  }
  return { pointer: receipt.pointer };
}

export async function rollbackRevision(
  environment: string,
  input: {
    author: { type: "human"; human_id: string } | { type: "system" };
    cause: string;
  },
): Promise<{ pointer: DeploymentPointer }> {
  const { status, text } = await requestText(
    `/deployments/environments/${encodeURIComponent(environment)}/rollback`,
    { method: "POST", body: JSON.stringify(input) },
    512 * 1024,
  );
  if (status !== 200 && status !== 201)
    throw new StudioApiError("Rollback returned an unproven receipt.", status, true);
  const receipt = parseMutationJson(text, pointerReceiptSchema, "Rollback receipt", status);
  return { pointer: receipt.pointer };
}

export async function declareCanary(
  environment: string,
  input: {
    revision_id: string;
    fraction: number;
    author: { type: "human"; human_id: string } | { type: "system" };
  },
): Promise<{ pointer: DeploymentPointer }> {
  const { status, text } = await requestText(
    `/deployments/environments/${encodeURIComponent(environment)}/canary`,
    { method: "PUT", body: JSON.stringify(input) },
    512 * 1024,
  );
  if (![200, 201].includes(status)) throw new StudioApiError("Canary declaration returned an unproven receipt.", status, true);
  const receipt = parseMutationJson(text, pointerReceiptSchema, "Canary receipt", status);
  if (receipt.pointer.canary?.revision_id !== input.revision_id || receipt.pointer.canary?.fraction !== input.fraction) {
    throw new StudioApiError("Canary receipt did not match the reviewed declaration.", status, true);
  }
  return receipt;
}

export async function clearCanary(
  environment: string,
  author: { type: "human"; human_id: string } | { type: "system" },
): Promise<{ applied: boolean; pointer: DeploymentPointer }> {
  const { status, text } = await requestText(
    `/deployments/environments/${encodeURIComponent(environment)}/canary`,
    { method: "DELETE", body: JSON.stringify({ author }) },
    512 * 1024,
  );
  if (![200, 201].includes(status)) throw new StudioApiError("Canary clear returned an unproven receipt.", status, true);
  const receipt = parseMutationJson(text, pointerReceiptSchema, "Canary clear receipt", status);
  return { applied: status === 201 || receipt.applied, pointer: receipt.pointer };
}

export async function createShadow(
  input: {
    revision_id: string;
    source: unknown;
    input?: unknown;
    author: { type: "human"; human_id: string } | { type: "system" };
  },
): Promise<ShadowVerdict> {
  const { status, text } = await requestText(
    "/deployments/shadows",
    { method: "POST", body: JSON.stringify(input) },
    512 * 1024,
  );
  if (status !== 201) throw new StudioApiError("Shadow run returned an unproven receipt.", status, true);
  return parseMutationJson(text, shadowVerdictSchema, "Shadow receipt", status);
}

export async function getDeploymentJournal(): Promise<DeploymentJournal> {
  const { text } = await requestText("/deployments/journal");
  return parse(text, deploymentJournalSchema, "Deployment journal");
}

export async function listSecrets(environment?: string): Promise<DeploymentSecret[]> {
  const query = environment ? `?environment=${encodeURIComponent(environment)}` : "";
  const { text } = await requestText(`/deployments/secrets${query}`);
  return parse(text, secretListSchema, "Secret metadata").secrets;
}
