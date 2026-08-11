import { z } from "zod";

const id = z.string().min(1).max(256);
const instant = z.string().datetime({ offset: true });
const jsonValue: z.ZodType<unknown> = z.unknown();

export const graphSchema = z.object({
  name: id,
  channels: z.array(id).max(2_000),
}).strict();

export const serverInfoSchema = z.object({
  service: z.literal("rusty-server"),
  version: z.string().min(1).max(64),
  checkpointer: z.enum(["postgres", "json_file"]),
  server_store: z.enum(["postgres", "json_file"]),
  store_path: z.string().max(4_096),
  graphs: z.array(graphSchema).max(2_000),
}).strict();

export const assistantSchema = z.object({
  assistant_id: id,
  name: z.string().min(1).max(1_024),
  graph: id,
  config: jsonValue,
  metadata: jsonValue,
  created_at: instant,
  active_version_id: id,
  version_count: z.number().int().nonnegative().max(256),
  archived_at: instant.optional(),
}).strict();

export const assistantCatalogSchema = z.array(assistantSchema);

export const threadSchema = z.object({
  thread_id: id,
  tenant: z.string().regex(/^[A-Za-z0-9._-]{1,64}$/),
  graph: id,
  metadata: jsonValue,
  created_at: instant,
}).strict();

export const runStatusSchema = z.enum([
  "pending",
  "running",
  "success",
  "interrupted",
  "error",
  "cancelled",
]);

export const runReceiptSchema = z.object({
  run_id: id,
  thread_id: id,
  status: z.enum(["pending", "running"]),
}).strict();

export const runSnapshotSchema = z.object({
  run_id: id,
  thread_id: id,
  graph: id,
  attempt: z.number().int().nonnegative(),
  status: runStatusSchema,
  output: jsonValue.optional(),
  error: z.string().max(65_536).optional(),
  message: z.string().max(65_536).optional(),
  interrupt: jsonValue.optional(),
}).strict();

export const eventKindSchema = z.enum([
  "super_step_start", "super_step_end", "node_input", "node_output",
  "model_call", "tool_call", "remote_call", "wasm_call", "interrupt",
  "resume", "routing_decision", "checkpoint_written", "effect_receipt",
  "agent_spawn", "agent_exit", "mailbox_send", "mailbox_receive",
  "supervision_event", "coordination_start", "coordination_end", "memory_write",
  "memory_read", "memory_correction", "memory_forget", "candidate_created",
  "candidate_evaluated", "candidate_promoted", "candidate_rolled_back",
  "policy_decision", "config_resolved", "connection_registered",
  "connection_consented", "connection_refreshed", "connection_revoked",
  "credential_handle_issued", "credential_use", "credential_denied",
  "connection_needs_reauth", "artifact_committed", "artifact_retention_released",
  "artifact_pruned", "artifact_unavailable", "capsule_resolved", "capsule_call", "capsule_denied",
  "signing_key_rotated",
]);

export const eventStatusSchema = z.enum(["ok", "error", "interrupted"]);

export const runEventSchema = z.object({
  id,
  run_id: id,
  thread_id: id,
  node_id: id.nullable(),
  seq: z.string().regex(/^(0|[1-9][0-9]*)$/),
  kind: eventKindSchema,
  effect: z.enum(["pure", "read_only", "idempotent", "compensatable", "non_idempotent"]),
  input: jsonValue.nullable(),
  output: jsonValue.nullable(),
  latency_ms: z.string().regex(/^(0|[1-9][0-9]*)$/).nullable(),
  tokens: jsonValue.nullable(),
  cost_usd: z.string().nullable(),
  status: eventStatusSchema,
  parent: id.nullable(),
  recorded_at: instant,
  rawJson: z.string(),
}).strict();

export const runEvidenceSchema = z.object({
  run_id: id,
  events: z.array(runEventSchema),
  complete: z.boolean(),
}).strict();

export type ServerInfo = z.infer<typeof serverInfoSchema>;
export type Assistant = z.infer<typeof assistantSchema>;
export type Thread = z.infer<typeof threadSchema>;
export type RunReceipt = z.infer<typeof runReceiptSchema>;
export type RunSnapshot = z.infer<typeof runSnapshotSchema>;
export type RunEvent = z.infer<typeof runEventSchema>;
export type RunEvidence = z.infer<typeof runEvidenceSchema>;
