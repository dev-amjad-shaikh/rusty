import { z } from "zod";
import { endpoint, parseJson, parseMutationJson, requestText, StudioApiError } from "./client";
import type { ConnectionIdentity } from "./client";

const id = z.string().min(1).max(256);
const sha256hex = z.string().regex(/^[0-9a-f]{64}$/);

const mediaKindSchema = z.enum(["file", "image", "audio", "data"]);

const retentionPolicySchema = z.discriminatedUnion("policy", [
  z.object({ policy: z.literal("pinned") }).strict(),
  z.object({ policy: z.literal("days"), days: z.number().int().min(0) }).strict(),
  z.object({ policy: z.literal("receipt_bound") }).strict(),
]);

const artifactLineageSchema = z.object({
  run_id: id,
  effect_id: z.unknown(),
  event_id: id,
}).strict();

const artifactVersionSchema = z.object({
  sha256: sha256hex,
  bytes: z.number().int().nonnegative(),
  committed_at: z.string().datetime({ offset: true }),
}).strict();

export const runArtifactSchema = z.object({
  artifact_id: sha256hex,
  name: id.optional(),
  media_kind: mediaKindSchema,
  media_type: z.string().max(256).optional(),
  lineage: artifactLineageSchema,
  versions: z.array(artifactVersionSchema).default([]),
  retention: retentionPolicySchema,
  created_at: z.string().datetime({ offset: true }),
}).strict();

export const artifactPreviewSchema = z.discriminatedUnion("kind", [
  z.object({
    kind: z.literal("text"),
    text: z.string(),
    truncated: z.boolean(),
    source_bytes: z.number().int().nonnegative(),
  }).strict(),
  z.object({
    kind: z.literal("json"),
    value: z.unknown(),
    source_bytes: z.number().int().nonnegative(),
  }).strict(),
  z.object({
    kind: z.literal("image"),
    format: z.string(),
    width: z.number().int().nonnegative(),
    height: z.number().int().nonnegative(),
    thumb_width: z.number().int().nonnegative(),
    thumb_height: z.number().int().nonnegative(),
    pixels_ppm_hex: z.string(),
  }).strict(),
  z.object({
    kind: z.literal("audio"),
    format: z.string(),
    duration_ms: z.number().int().nonnegative(),
    sample_rate: z.number().int().nonnegative(),
    channels: z.number().int().nonnegative(),
    frames: z.number().int().nonnegative(),
    peaks: z.array(z.number().int().nonnegative()),
  }).strict(),
  z.object({
    kind: z.literal("empty"),
    reason: z.string(),
  }).strict(),
]);

export type MediaKind = z.infer<typeof mediaKindSchema>;
export type RetentionPolicy = z.infer<typeof retentionPolicySchema>;
export type RunArtifact = z.infer<typeof runArtifactSchema>;
export type ArtifactPreview = z.infer<typeof artifactPreviewSchema>;

const artifactListSchema = z.object({ artifacts: z.array(runArtifactSchema) }).strict();
const namedVersionsSchema = z.object({ name: id, current: sha256hex, versions: z.array(artifactVersionSchema) }).strict();
const previewResponseSchema = z.object({ artifact_id: sha256hex, preview: artifactPreviewSchema }).strict();
const releaseResponseSchema = z.object({
  artifact_id: sha256hex,
  released: z.boolean(),
  converged: z.boolean(),
  pruned: z.boolean(),
  journal_event_id: id,
}).strict();

export async function listRunArtifacts(connection: ConnectionIdentity, options: { run_id?: string; name?: string; media_kind?: MediaKind } = {}) {
  const params = new URLSearchParams();
  if (options.run_id) params.set("run_id", options.run_id);
  if (options.name) params.set("name", options.name);
  if (options.media_kind) params.set("media_kind", options.media_kind);
  const { text } = await requestText(connection, `/artifacts?${params.toString()}`);
  return parseJson(text, artifactListSchema, "Artifact list");
}

export async function getRunArtifact(connection: ConnectionIdentity, artifactId: string) {
  const { text } = await requestText(connection, `/artifacts/${artifactId}`);
  return parseJson(text, runArtifactSchema, "Artifact");
}

export async function getRunArtifactNamed(connection: ConnectionIdentity, name: string) {
  const { text } = await requestText(connection, `/artifacts/names/${name}`);
  return parseJson(text, runArtifactSchema, "Named artifact");
}

export async function listRunArtifactVersions(connection: ConnectionIdentity, name: string) {
  const { text } = await requestText(connection, `/artifacts/names/${name}/versions`);
  return parseJson(text, namedVersionsSchema, "Artifact versions");
}

export async function getRunArtifactPreview(connection: ConnectionIdentity, artifactId: string) {
  const { text } = await requestText(connection, `/artifacts/${artifactId}/preview`);
  return parseJson(text, previewResponseSchema, "Artifact preview");
}

export async function getRunArtifactBytes(connection: ConnectionIdentity, artifactId: string) {
  const response = await fetch(endpoint(connection, `/artifacts/${artifactId}/bytes`), {
    headers: connection.apiKey ? { "X-Api-Key": connection.apiKey } : {},
  });
  if (!response.ok) {
    throw new StudioApiError(`Artifact bytes returned ${response.status}.`, response.status, response.status >= 500);
  }
  return response;
}

export async function releaseRunArtifact(connection: ConnectionIdentity, artifactId: string, input: { released_by: string; reason?: string }) {
  const { text, status } = await requestText(connection, `/artifacts/${artifactId}/release`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(input),
  });
  return parseMutationJson(text, releaseResponseSchema, "Artifact release", status);
}
