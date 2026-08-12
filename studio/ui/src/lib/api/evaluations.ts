import { z } from "zod";
import { requestText, parseJson, parseMutationJson, type ConnectionIdentity } from "./client";

export const evalCaseSchema = z.object({
  id: z.string(),
  input: z.record(z.string(), z.unknown()),
  expect: z
    .object({
      tool_trajectory: z.array(z.unknown()).optional(),
      state: z.array(z.unknown()).optional(),
      forbid_tools: z.array(z.string()).optional(),
      max_cost_usd: z.number().optional(),
      max_latency_ms: z.number().optional(),
    })
    .optional(),
  tags: z.array(z.string()).optional(),
});

export type EvalCase = z.infer<typeof evalCaseSchema>;

export const datasetVersionSchema = z.object({
  name: z.string(),
  version: z.string(),
  created: z.boolean().optional(),
  case_count: z.number(),
  digest: z.string(),
});

export type DatasetVersion = z.infer<typeof datasetVersionSchema>;

export const datasetVersionsSchema = z.object({
  name: z.string(),
  versions: z.array(datasetVersionSchema),
});

export const datasetCasesSchema = z.object({
  cases: z.array(evalCaseSchema),
});

export const experimentStatusSchema = z.enum(["Queued", "Running", "Complete", "Failed", "Cancelled"]);

export const experimentRecordSchema = z.object({
  experiment_id: z.string(),
  dataset_name: z.string(),
  dataset_version: z.string(),
  candidate_id: z.string(),
  target_metric: z.string(),
  thresholds: z.record(z.string(), z.unknown()),
  status: experimentStatusSchema,
  created_at: z.string(),
  evaluation: z.unknown().optional(),
});

export type ExperimentRecord = z.infer<typeof experimentRecordSchema>;

export const experimentsListSchema = z.object({
  experiments: z.array(experimentRecordSchema),
});

export const gateRecordSchema = z.object({
  name: z.string(),
  blocked_target: z.string(),
  metric: z.string(),
  threshold: z.number(),
  min_evidence: z.number(),
  require_approval: z.boolean(),
  dataset_version: z.string(),
  baseline_experiment_id: z.string().optional().nullable(),
  created_at: z.string(),
});

export type GateRecord = z.infer<typeof gateRecordSchema>;

export const gatesListSchema = z.object({
  gates: z.array(gateRecordSchema),
});

export async function createDataset(
  connection: ConnectionIdentity,
  payload: { name: string; version: string; cases: EvalCase[] },
): Promise<DatasetVersion> {
  const { text, status } = await requestText(connection, "/datasets", {
    method: "POST",
    body: JSON.stringify(payload),
  });
  return parseMutationJson(text, datasetVersionSchema, "Create dataset", status);
}

export async function listDatasets(connection: ConnectionIdentity): Promise<DatasetVersion[]> {
  const { text } = await requestText(connection, "/datasets");
  const parsed = parseJson(text, z.object({ datasets: z.array(datasetVersionSchema) }), "List datasets");
  return parsed.datasets;
}

export async function getDatasetVersions(
  connection: ConnectionIdentity,
  name: string,
): Promise<DatasetVersion[]> {
  const { text } = await requestText(connection, `/datasets/${encodeURIComponent(name)}`);
  const parsed = parseJson(text, datasetVersionsSchema, `Dataset ${name} versions`);
  return parsed.versions;
}

export async function getDatasetCases(
  connection: ConnectionIdentity,
  name: string,
  version: string,
): Promise<EvalCase[]> {
  const { text } = await requestText(
    connection,
    `/datasets/${encodeURIComponent(name)}/versions/${encodeURIComponent(version)}/cases`,
  );
  const parsed = parseJson(text, datasetCasesSchema, `Dataset ${name}@${version} cases`);
  return parsed.cases;
}

export async function createExperiment(
  connection: ConnectionIdentity,
  payload: {
    experiment_id?: string;
    candidate_id: string;
    dataset_name: string;
    dataset_version: string;
    target_metric: string;
    thresholds: Record<string, unknown>;
    baseline_experiment_id?: string;
  },
): Promise<ExperimentRecord> {
  const { text, status } = await requestText(connection, "/experiments", {
    method: "POST",
    body: JSON.stringify(payload),
  });
  return parseMutationJson(text, experimentRecordSchema, "Create experiment", status);
}

export async function listExperiments(connection: ConnectionIdentity): Promise<ExperimentRecord[]> {
  const { text } = await requestText(connection, "/experiments");
  const parsed = parseJson(text, experimentsListSchema, "List experiments");
  return parsed.experiments;
}

export async function createGate(
  connection: ConnectionIdentity,
  payload: {
    name: string;
    blocked_target: string;
    metric: string;
    threshold: number;
    min_evidence?: number;
    require_approval?: boolean;
    dataset_version: string;
    baseline_experiment_id?: string;
  },
): Promise<GateRecord> {
  const { text, status } = await requestText(connection, "/gates", {
    method: "POST",
    body: JSON.stringify(payload),
  });
  return parseMutationJson(text, gateRecordSchema, "Create gate", status);
}

export async function listGates(connection: ConnectionIdentity): Promise<GateRecord[]> {
  const { text } = await requestText(connection, "/gates");
  const parsed = parseJson(text, gatesListSchema, "List gates");
  return parsed.gates;
}
