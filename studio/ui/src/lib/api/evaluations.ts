import { z } from "zod";
import { isLosslessNumber, parse as parseLossless, stringify as stringifyLossless } from "lossless-json";
import { jsonEquivalent, requestText, StudioApiError, type ConnectionIdentity } from "./client";

const timestamp = z.string().datetime({ offset: true });
const exactId = z.string().min(1).max(256);
const safeUnsigned = z.number().int().nonnegative().max(Number.MAX_SAFE_INTEGER);
const U64_MAX = 18_446_744_073_709_551_615n;
const exactUnsigned = z.union([safeUnsigned, z.string().regex(/^(?:0|[1-9][0-9]*)$/)])
  .refine((value) => BigInt(value) <= U64_MAX, "must fit an unsigned 64-bit integer");
const finiteRate = z.number().min(0).max(1);
const statePredicateSchema = z.object({ pointer: z.string(), expected: z.unknown() }).strict();
const expectedToolCallSchema = z.object({
  name: z.string(),
  args: z.record(z.string(), z.unknown()).optional(),
}).strict();

export const evalCaseSchema = z.object({
  id: exactId,
  input: z.record(z.string(), z.unknown()),
  expect: z.object({
    tool_trajectory: z.array(expectedToolCallSchema).optional(),
    state: z.array(statePredicateSchema).optional(),
    forbid_tools: z.array(z.string()).optional(),
    max_cost_usd: z.number().optional(),
    max_latency_ms: exactUnsigned.optional(),
  }).strict().optional(),
  tags: z.array(z.string()).optional(),
  source: z.object({
    run_id: exactId,
    thread_id: exactId,
    agent_id: exactId,
    captured_at: timestamp,
  }).strict(),
}).strict();
export type EvalCase = z.infer<typeof evalCaseSchema>;

export const datasetVersionSchema = z.object({
  name: exactId,
  version: exactId,
  created: z.boolean().optional(),
  created_at: timestamp.optional(),
  case_count: safeUnsigned,
  digest: z.string().regex(/^[0-9a-f]{64}$/),
}).strict();
export type DatasetVersion = z.infer<typeof datasetVersionSchema>;

const latencySchema = z.object({
  min: exactUnsigned, p50: exactUnsigned,
  p95: exactUnsigned, max: exactUnsigned, mean: z.number().nonnegative(),
}).strict();
const runStatusSchema = z.discriminatedUnion("status", [
  z.object({ status: z.literal("done") }).strict(),
  z.object({ status: z.literal("interrupted") }).strict(),
  z.object({ status: z.literal("failed"), error: z.string() }).strict(),
]);
const assertionResultSchema = z.object({
  assertion: z.string(), passed: z.boolean(), expected: z.unknown(), observed: z.unknown(),
  detail: z.string().optional(),
}).strict();
const judgeVerdictSchema = z.object({ score: finiteRate, passed: z.boolean(), rationale: z.string() }).strict();
const runSchema = z.object({
  repetition: safeUnsigned,
  status: runStatusSchema,
  passed: z.boolean(),
  assertions: z.array(assertionResultSchema),
  judge: judgeVerdictSchema.optional(),
  tool_calls: exactUnsigned,
  latency_ms: exactUnsigned,
  cost_usd: z.number().nonnegative(),
  total_tokens: exactUnsigned,
}).strict();
export const experimentReportSchema = z.object({
  format_version: z.literal(1),
  name: exactId,
  dataset_name: exactId,
  dataset_version: exactId,
  runs_per_case: safeUnsigned.positive(),
  max_concurrency: safeUnsigned.positive(),
  cases: z.array(z.object({
    case_id: exactId,
    tags: z.array(z.string()).optional(),
    pass_rate: finiteRate,
    runs: z.array(runSchema),
  }).strict()),
  summary: z.object({
    cases: safeUnsigned, runs: safeUnsigned,
    runs_passed: safeUnsigned, run_pass_rate: finiteRate,
    case_pass_rate: finiteRate, assertions: z.array(z.object({
      assertion: z.string(), passed: safeUnsigned, total: safeUnsigned, rate: finiteRate,
    }).strict()),
    latency_ms: latencySchema, total_cost_usd: z.number().nonnegative(),
    total_tokens: exactUnsigned,
  }).strict(),
}).strict();
export type ExperimentReport = z.infer<typeof experimentReportSchema>;

export const comparisonSchema = z.object({
  baseline: exactId,
  candidate: exactId,
  thresholds: z.object({ max_pass_rate_drop: z.number(), max_latency_p95_ratio: z.number() }).strict(),
  assertion_deltas: z.array(z.object({
    assertion: z.string(), baseline_rate: finiteRate, candidate_rate: finiteRate, delta: z.number(),
  }).strict()),
  case_deltas: z.array(z.object({
    case_id: exactId,
    baseline_pass_rate: z.number().nullable(),
    candidate_pass_rate: z.number().nullable(),
    change: z.enum(["improved", "regressed", "unchanged", "added", "removed"]),
  })),
  latency: z.object({
    baseline_p50: exactUnsigned, candidate_p50: exactUnsigned, p50_ratio: z.number().nullable(),
    baseline_p95: exactUnsigned, candidate_p95: exactUnsigned, p95_ratio: z.number().nullable(),
  }).strict(),
  baseline_cost_usd: z.number(), candidate_cost_usd: z.number(),
  regressions: z.array(z.discriminatedUnion("regression", [
    z.object({ regression: z.literal("assertion_pass_rate"), assertion: z.string(), baseline: finiteRate, candidate: finiteRate }).strict(),
    z.object({ regression: z.literal("case_pass_rate"), case_id: exactId, baseline: finiteRate, candidate: finiteRate }).strict(),
    z.object({ regression: z.literal("latency_p95"), baseline_ms: exactUnsigned, candidate_ms: exactUnsigned, ratio: z.number() }).strict(),
  ])), regressed: z.boolean(),
}).strict();
export type Comparison = z.infer<typeof comparisonSchema>;

export const experimentStatusSchema = z.discriminatedUnion("phase", [
  z.object({ phase: z.literal("queued") }).strict(),
  z.object({ phase: z.literal("running"), completed_runs: safeUnsigned, total_runs: safeUnsigned }).strict(),
  z.object({ phase: z.literal("complete") }).strict(),
  z.object({ phase: z.literal("failed"), reason: z.string() }).strict(),
  z.object({ phase: z.literal("cancelled") }).strict(),
]);
export const experimentRecordSchema = z.object({
  experiment_id: exactId,
  dataset_name: exactId,
  dataset_version: exactId,
  candidate_id: exactId,
  config: z.object({
    runs_per_case: safeUnsigned.positive(), max_concurrency: safeUnsigned.positive(),
    target_metric: exactId,
    thresholds: z.object({ max_pass_rate_drop: z.number(), max_latency_p95_ratio: z.number() }).strict(),
  }).strict(),
  status: experimentStatusSchema,
  created_at: timestamp,
  updated_at: timestamp,
  baseline_report: experimentReportSchema.optional(),
  candidate_report: experimentReportSchema.optional(),
  comparison: comparisonSchema.optional(),
}).strict();
const gateMetricSchema = z.discriminatedUnion("metric", [
  z.object({ metric: z.literal("minimum_runs") }).strict(),
  z.object({ metric: z.literal("minimum_run_pass_rate") }).strict(),
  z.object({ metric: z.literal("minimum_case_pass_rate") }).strict(),
  z.object({ metric: z.literal("maximum_total_cost_usd") }).strict(),
  z.object({ metric: z.literal("comparison_available") }).strict(),
  z.object({ metric: z.literal("maximum_cost_ratio") }).strict(),
  z.object({ metric: z.literal("maximum_regressions") }).strict(),
  z.object({ metric: z.literal("no_removed_cases") }).strict(),
  z.object({ metric: z.literal("assertion_pass_rate"), assertion: z.string() }).strict(),
  z.object({ metric: z.literal("tag_pass_rate"), tag: z.string() }).strict(),
]);
const gateCheckSchema = z.object({
  metric: gateMetricSchema, passed: z.boolean(), observed: z.unknown(), required: z.unknown(), detail: z.string(),
}).strict();
export type ExperimentRecord = z.infer<typeof experimentRecordSchema>;
export const experimentSummarySchema = experimentRecordSchema.omit({
  baseline_report: true, candidate_report: true, comparison: true,
});
export type ExperimentSummary = z.infer<typeof experimentSummarySchema>;

export const gateRecordSchema = z.object({
  name: exactId,
  blocked_target: exactId,
  experiment_id: exactId,
  dataset_name: exactId,
  dataset_version: exactId,
  policy: z.record(z.string(), z.unknown()),
  decision: z.object({
    format_version: z.literal(1), policy: exactId, candidate: exactId,
    baseline: exactId.nullable(), outcome: z.enum(["allow", "block"]), checks: z.array(gateCheckSchema),
  }).strict(),
  created_at: timestamp,
}).strict();
export type GateRecord = z.infer<typeof gateRecordSchema>;

const candidateRecordSchema = z.object({
  candidate: z.object({
    candidate_id: z.string().regex(/^[0-9a-f]{64}$/),
    content: z.object({ kind: exactId }).passthrough(),
    created_at: timestamp,
  }).passthrough(),
  status: z.enum(["created", "evaluated", "promoted", "rolled_back"]),
}).passthrough();
export type EvaluationCandidate = z.infer<typeof candidateRecordSchema>;

function normalizeLossless(root: unknown): unknown {
  const scalar = (value: unknown): unknown => {
    if (!isLosslessNumber(value)) return value;
    const raw = value.toString();
    if (/^-?(?:0|[1-9][0-9]*)$/.test(raw)) {
      const integer = BigInt(raw);
      if (integer >= BigInt(Number.MIN_SAFE_INTEGER) && integer <= BigInt(Number.MAX_SAFE_INTEGER)) return Number(raw);
      return raw;
    }
    const numeric = Number(raw);
    if (!Number.isFinite(numeric)) throw new Error("non-finite number");
    return numeric;
  };
  const first = scalar(root);
  if (!first || typeof first !== "object" || isLosslessNumber(root)) return first;
  const result: unknown = Array.isArray(root) ? [] : {};
  const pending: Array<{ source: Record<string, unknown> | unknown[]; target: Record<string, unknown> | unknown[] }> = [{ source: root as Record<string, unknown> | unknown[], target: result as Record<string, unknown> | unknown[] }];
  while (pending.length) {
    const { source, target } = pending.pop()!;
    for (const [key, raw] of Object.entries(source)) {
      const value = scalar(raw);
      if (value && typeof value === "object" && !isLosslessNumber(raw)) {
        const child: unknown = Array.isArray(raw) ? [] : {};
        (target as Record<string, unknown>)[key] = child;
        pending.push({ source: raw as Record<string, unknown> | unknown[], target: child as Record<string, unknown> | unknown[] });
      } else {
        (target as Record<string, unknown>)[key] = value;
      }
    }
  }
  return result;
}

function parseEvaluationJson<T>(text: string, schema: z.ZodType<T>, context: string): T {
  try {
    return schema.parse(normalizeLossless(parseLossless(text)));
  } catch (error) {
    const reason = error instanceof z.ZodError ? error.issues[0]?.message : "invalid JSON";
    throw new StudioApiError(`${context} did not match the Rusty contract (${reason}).`, 0);
  }
}

function parseEvaluationMutation<T>(text: string, schema: z.ZodType<T>, context: string, status: number): T {
  try { return parseEvaluationJson(text, schema, context); }
  catch (caught) {
    throw new StudioApiError(caught instanceof Error ? caught.message : `${context} was not trustworthy.`, status, true);
  }
}

export async function createDataset(connection: ConnectionIdentity, payload: { name: string; version: string; cases: EvalCase[] }) {
  const { text, status } = await requestText(connection, "/datasets", { method: "POST", body: stringifyLossless(payload) });
  const receipt = parseEvaluationMutation(text, datasetVersionSchema, "Publish dataset", status);
  if (![200, 201].includes(status) || receipt.name !== payload.name || receipt.version !== payload.version
    || receipt.case_count !== payload.cases.length || receipt.created !== (status === 201)) {
    throw new StudioApiError("Dataset receipt did not match the exact published version.", status, true);
  }
  return receipt;
}
export async function listDatasets(connection: ConnectionIdentity) {
  const { text } = await requestText(connection, "/datasets");
  return parseEvaluationJson(text, z.object({ datasets: z.array(datasetVersionSchema), truncated: z.boolean() }).strict(), "Dataset catalog");
}
export async function getDatasetCases(connection: ConnectionIdentity, name: string, version: string) {
  const { text } = await requestText(connection, `/datasets/${encodeURIComponent(name)}/versions/${encodeURIComponent(version)}/cases`);
  return parseEvaluationJson(text, z.object({ cases: z.array(evalCaseSchema) }), "Dataset cases").cases;
}
export interface CreateExperimentInput {
  experiment_id: string; candidate_id: string; dataset_name: string; dataset_version: string;
  runs_per_case: number; max_concurrency: number; target_metric: string;
  thresholds: { max_pass_rate_drop: number; max_latency_p95_ratio: number };
}
export async function createExperiment(connection: ConnectionIdentity, payload: CreateExperimentInput) {
  const { text, status } = await requestText(connection, "/experiments", { method: "POST", body: JSON.stringify(payload) });
  const receipt = parseEvaluationMutation(text, experimentSummarySchema, "Start experiment", status);
  const expectedConfig = { runs_per_case: payload.runs_per_case, max_concurrency: payload.max_concurrency, target_metric: payload.target_metric, thresholds: payload.thresholds };
  if (![200, 201].includes(status) || receipt.experiment_id !== payload.experiment_id
    || receipt.candidate_id !== payload.candidate_id || receipt.dataset_name !== payload.dataset_name
    || receipt.dataset_version !== payload.dataset_version || !jsonEquivalent(receipt.config, expectedConfig)) {
    throw new StudioApiError("Experiment receipt did not match the exact reviewed plan.", status, true);
  }
  return receipt;
}
export async function listExperiments(connection: ConnectionIdentity) {
  const { text } = await requestText(connection, "/experiments");
  return parseEvaluationJson(text, z.object({ experiments: z.array(experimentSummarySchema), truncated: z.boolean() }).strict(), "Experiment catalog");
}
export async function listEvaluationCandidates(connection: ConnectionIdentity) {
  const { text } = await requestText(connection, "/learn/candidates");
  return parseEvaluationJson(text, z.object({ candidates: z.array(candidateRecordSchema) }), "Candidate catalog").candidates;
}
export async function getExperiment(connection: ConnectionIdentity, id: string) {
  const { text } = await requestText(connection, `/experiments/${encodeURIComponent(id)}`);
  return parseEvaluationJson(text, experimentRecordSchema, "Experiment");
}
export async function cancelExperiment(connection: ConnectionIdentity, id: string) {
  const { text, status } = await requestText(connection, `/experiments/${encodeURIComponent(id)}/cancel`, { method: "POST" });
  if (status !== 200) throw new StudioApiError("Experiment cancellation returned an unproven receipt.", status, true);
  return parseEvaluationMutation(text, z.object({ experiment_id: z.literal(id), cancellation_requested: z.literal(true) }), "Cancel experiment", status);
}
export async function createGate(connection: ConnectionIdentity, payload: { name: string; blocked_target: string; experiment_id: string; policy: Record<string, unknown>; acknowledged: boolean }) {
  const { text, status } = await requestText(connection, "/gates", { method: "POST", body: JSON.stringify(payload) });
  const receipt = parseEvaluationMutation(text, gateRecordSchema, "Save release gate", status);
  if (![200, 201].includes(status) || receipt.name !== payload.name || receipt.blocked_target !== payload.blocked_target
    || receipt.experiment_id !== payload.experiment_id || !jsonEquivalent(receipt.policy, payload.policy)
    || receipt.decision.policy !== payload.name) {
    throw new StudioApiError("Gate receipt did not match the exact reviewed policy and experiment.", status, true);
  }
  return receipt;
}
export async function listGates(connection: ConnectionIdentity) {
  const { text } = await requestText(connection, "/gates");
  return parseEvaluationJson(text, z.object({ gates: z.array(gateRecordSchema), truncated: z.boolean() }).strict(), "Release gates");
}
