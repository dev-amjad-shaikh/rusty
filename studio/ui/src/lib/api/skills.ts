import { z } from "zod";
import { endpoint, parseJson, parseMutationJson, requestText, StudioApiError, type ConnectionIdentity } from "./client";
import { isUnicodeScalarString } from "../text";

const hash = z.string().regex(/^[0-9a-f]{64}$/);
const skillName = z.string().regex(/^[a-z0-9]+(?:-[a-z0-9]+)*$/).max(128);

export const scanFindingSchema = z.object({
  severity: z.enum(["warning", "denial"]),
  kind: z.enum(["embedded_script", "credentialed_url", "base64_blob"]),
  location: z.string().min(1).max(1_024),
  detail: z.string().max(2_048),
}).strict();
export type ScanFinding = z.infer<typeof scanFindingSchema>;

const provenanceSchema = z.object({
  source: z.discriminatedUnion("type", [
    z.object({ type: z.literal("local_path"), path: z.string().min(1).max(4_096) }).strict(),
    z.object({ type: z.literal("registry"), name: z.string().min(1).max(256) }).strict(),
  ]),
  author: z.string().min(1).max(256),
  content_hash: hash,
}).strict();

export const skillMetadataSchema = z.object({
  name: skillName,
  description: z.string().min(1).max(4_096),
  revision: z.number().int().positive(),
  content_hash: hash,
  license: z.string().max(256).optional(),
  allowed_tools: z.array(z.string().max(128)).max(64).optional().default([]),
  compatibility: z.string().max(1_024).optional(),
}).strict();
export type SkillMetadata = z.infer<typeof skillMetadataSchema>;

const scanSummarySchema = z.object({
  clean: z.boolean(),
  warnings: z.array(scanFindingSchema).max(1_000),
  warning_count: z.number().int().nonnegative(),
}).strict();

const receiptCore = {
  metadata: skillMetadataSchema,
  name: skillName,
  revision: z.number().int().positive(),
  content_hash: hash,
  provenance: provenanceSchema,
  scan: scanSummarySchema,
};

const skillListSchema = z.object({ skills: z.array(skillMetadataSchema).max(10_000) }).strict();
const skillDetailSchema = z.object({ ...receiptCore, revisions: z.number().int().positive() }).strict();
const skillVersionSchema = z.object(receiptCore).strict();
const publishReceiptSchema = z.object({ ...receiptCore, already_registered: z.boolean() }).strict();
const skillBodySchema = z.object({
  name: skillName,
  revision: z.number().int().positive(),
  content_hash: hash,
  body: z.string().max(1024 * 1024),
}).strict();
const skillHistorySchema = z.object({ name: skillName, history: z.array(skillMetadataSchema).max(10_000) }).strict();

export type SkillReceipt = z.infer<typeof skillVersionSchema>;
export type SkillDetail = z.infer<typeof skillDetailSchema>;
export type SkillBody = z.infer<typeof skillBodySchema>;
export type PublishReceipt = z.infer<typeof publishReceiptSchema>;

export class SkillScanDenied extends Error {
  constructor(
    message: string,
    readonly findings: ScanFinding[],
  ) {
    super(message);
    this.name = "SkillScanDenied";
  }
}

function skillPath(name: string) {
  return `/skills/${encodeURIComponent(name)}`;
}

function checkReceiptIdentity(receipt: SkillReceipt | SkillDetail | PublishReceipt, name: string, status: number) {
  if (receipt.name !== name || receipt.metadata.name !== name || receipt.metadata.revision !== receipt.revision
    || receipt.metadata.content_hash !== receipt.content_hash || receipt.provenance.content_hash !== receipt.content_hash) {
    throw new StudioApiError("Skill receipt crossed skill or content identity.", status, status >= 500);
  }
}

export async function listSkills(connection: ConnectionIdentity): Promise<SkillMetadata[]> {
  const { text } = await requestText(connection, "/skills");
  return parseJson(text, skillListSchema, "Skill catalog").skills;
}

export async function getSkill(connection: ConnectionIdentity, name: string): Promise<SkillDetail> {
  const { status, text } = await requestText(connection, skillPath(name));
  const detail = parseJson(text, skillDetailSchema, "Skill detail");
  checkReceiptIdentity(detail, name, status);
  return detail;
}

export async function getSkillBody(connection: ConnectionIdentity, name: string): Promise<SkillBody> {
  const { status, text } = await requestText(connection, `${skillPath(name)}/body`);
  const body = parseJson(text, skillBodySchema, "Skill body");
  if (body.name !== name) throw new StudioApiError("Skill body crossed skill identity.", status);
  return body;
}

export async function getSkillHistory(connection: ConnectionIdentity, name: string): Promise<SkillMetadata[]> {
  const { status, text } = await requestText(connection, `${skillPath(name)}/history`);
  const history = parseJson(text, skillHistorySchema, "Skill history");
  if (history.name !== name || !history.history.length || history.history.some((entry) => entry.name !== name)) {
    throw new StudioApiError("Skill history crossed skill identity.", status);
  }
  if (!history.history.every((entry, index) => entry.revision === index + 1)) {
    throw new StudioApiError("Skill history was not one append-only revision sequence.", status);
  }
  return history.history;
}

export async function getSkillVersion(connection: ConnectionIdentity, name: string, revision: number): Promise<SkillReceipt> {
  const { status, text } = await requestText(connection, `${skillPath(name)}/versions/${encodeURIComponent(String(revision))}`);
  const receipt = parseJson(text, skillVersionSchema, "Skill version");
  checkReceiptIdentity(receipt, name, status);
  if (receipt.revision !== revision) throw new StudioApiError("Skill version named a different revision.", status);
  return receipt;
}

export interface SkillFile {
  path: string;
  contentType: string;
  bytes: Uint8Array;
}

export async function getSkillFile(connection: ConnectionIdentity, name: string, path: string, maxBytes = 1024 * 1024): Promise<SkillFile> {
  let response: Response;
  try {
    response = await fetch(endpoint(connection, `${skillPath(name)}/files/${path.split("/").map(encodeURIComponent).join("/")}`), {
      headers: { ...(connection.apiKey ? { "X-Api-Key": connection.apiKey } : {}) },
    });
  } catch {
    throw new StudioApiError("Rusty could not be reached.", 0);
  }
  if (!response.ok) {
    const text = await response.text().catch(() => "");
    let message = `Rusty returned HTTP ${response.status}.`;
    try {
      const value = JSON.parse(text) as { message?: unknown };
      if (typeof value.message === "string") message = value.message.slice(0, 2_000);
    } catch { /* status fallback */ }
    throw new StudioApiError(message, response.status);
  }
  const buffer = await response.arrayBuffer();
  if (buffer.byteLength > maxBytes) {
    throw new StudioApiError(`Skill member exceeded the ${Math.floor(maxBytes / 1024)} KiB safety boundary.`, response.status);
  }
  return { path, contentType: response.headers.get("content-type") ?? "", bytes: new Uint8Array(buffer) };
}

export interface PublishSkillInput {
  skillMd: string;
  references: Record<string, string>;
  assets: Record<string, string>;
  author: string;
}

function encodeHex(bytes: Uint8Array) {
  return Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
}

export async function publishSkill(connection: ConnectionIdentity, input: PublishSkillInput, expectedName: string): Promise<PublishReceipt> {
  if (![input.skillMd, input.author, ...Object.keys(input.references), ...Object.values(input.references),
    ...Object.keys(input.assets), ...Object.values(input.assets)].every(isUnicodeScalarString)) {
    throw new StudioApiError("Skill package input contained invalid Unicode.", 0);
  }
  const assets = Object.fromEntries(Object.entries(input.assets).map(([path, text]) => [path, encodeHex(new TextEncoder().encode(text))]));
  const payload = JSON.stringify({ skill_md: input.skillMd, references: input.references, assets, author: input.author });
  let response: Response;
  try {
    response = await fetch(endpoint(connection, "/skills"), {
      method: "POST",
      headers: {
        Accept: "application/json",
        "Content-Type": "application/json",
        ...(connection.apiKey ? { "X-Api-Key": connection.apiKey } : {}),
      },
      body: payload,
    });
  } catch {
    throw new StudioApiError("Rusty could not be reached.", 0, true);
  }
  const text = await response.text().catch(() => "");
  if (response.status === 422) {
    let body: unknown = null;
    try { body = text ? JSON.parse(text) : null; } catch { body = null; }
    const denial = z.object({
      error: z.literal("scan_denied"),
      message: z.string().max(2_000),
      findings: z.array(scanFindingSchema).min(1).max(1_000),
    }).strict().safeParse(body);
    if (!denial.success) throw new StudioApiError("The scan denial did not match the Rusty contract.", 422);
    throw new SkillScanDenied(denial.data.message, denial.data.findings);
  }
  if (response.status !== 200 && response.status !== 201) {
    let message = `Rusty returned HTTP ${response.status}.`;
    try {
      const value = JSON.parse(text) as { message?: unknown; error?: unknown };
      if (typeof value.message === "string") message = value.message.slice(0, 2_000);
      else if (typeof value.error === "string") message = value.error.slice(0, 2_000);
    } catch { /* status fallback */ }
    throw new StudioApiError(message, response.status, response.status >= 500);
  }
  const receipt = parseMutationJson(text, publishReceiptSchema, "Skill receipt", response.status);
  if (receipt.already_registered !== (response.status === 200)) {
    throw new StudioApiError("Skill receipt did not match the mutation status.", response.status, true);
  }
  checkReceiptIdentity(receipt, expectedName, response.status);
  return receipt;
}
