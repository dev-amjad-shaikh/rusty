import type { MemoryRecord, ProvenanceAuthor } from "../../lib/api/memory";
import { authorText, scopeAddressText } from "../../lib/api/memory";

export type MemoryLifecycle = "active" | "candidate" | "scheduled" | "historical" | "expired" | "superseded";

export const lifecycleLabels: Record<MemoryLifecycle, string> = {
  active: "Active",
  candidate: "Candidate",
  scheduled: "Not yet valid",
  historical: "Validity ended",
  expired: "Expired",
  superseded: "Superseded",
};

export function supersededIds(records: MemoryRecord[]): Set<string> {
  const ids = new Set<string>();
  for (const record of records) {
    if (record.supersedes) ids.add(record.supersedes);
    if (record.kind === "summary") {
      for (const id of record.provenance.evidence.source_memory_ids) ids.add(id);
    }
  }
  return ids;
}

export function recordStates(record: MemoryRecord, superseded: Set<string>, now: Date): MemoryLifecycle[] {
  const states: MemoryLifecycle[] = [];
  if (record.candidacy) states.push("candidate");
  if (superseded.has(record.memory_id)) states.push("superseded");
  if (record.expires_at && new Date(record.expires_at).valueOf() <= now.valueOf()) states.push("expired");
  if (new Date(record.validity.valid_from).valueOf() > now.valueOf()) states.push("scheduled");
  else if (record.validity.valid_until && new Date(record.validity.valid_until).valueOf() <= now.valueOf()) states.push("historical");
  return states.length ? states : ["active"];
}

export function recordTitle(record: MemoryRecord) {
  return record.key ?? `${record.kind} record`;
}

export function recordScopeText(record: MemoryRecord) {
  return scopeAddressText(record.scope);
}

export function recordAuthorText(record: MemoryRecord) {
  return authorText(record.provenance.author);
}

export function contentValue(record: MemoryRecord): unknown {
  return record.content.kind === "inline" ? record.content.value : undefined;
}

export function contentText(record: MemoryRecord, maxBytes = 12_000): string {
  const value = contentValue(record);
  if (value === undefined) {
    const reference = record.content.kind === "artifact" ? record.content.value : null;
    return reference ? `Content held as artifact ${reference.sha256} (${reference.bytes} bytes).` : "Content unavailable.";
  }
  let text: string;
  try {
    text = JSON.stringify(value, null, 2) ?? String(value);
  } catch {
    text = String(value);
  }
  return text.length > maxBytes ? `${text.slice(0, maxBytes)}\n… (preview bounded)` : text;
}

export function contentPreview(record: MemoryRecord, maxBytes = 220): string {
  const value = contentValue(record);
  if (value === undefined) return "Content held as a referenced artifact.";
  let text: string;
  if (typeof value === "string") text = value;
  else {
    try {
      text = JSON.stringify(value) ?? String(value);
    } catch {
      text = String(value);
    }
  }
  return text.length > maxBytes ? `${text.slice(0, maxBytes)}…` : text;
}

export function evidenceSummary(record: MemoryRecord): string {
  const evidence = record.provenance.evidence;
  const parts: string[] = [];
  if (evidence.correction_id) parts.push(`correction ${evidence.correction_id}`);
  if (evidence.candidate_id) parts.push(`candidate ${evidence.candidate_id.slice(0, 12)}…`);
  if (evidence.run_id) parts.push(`run ${evidence.run_id.slice(0, 12)}…`);
  if (evidence.event_ids.length) parts.push(`${evidence.event_ids.length} journaled event${evidence.event_ids.length === 1 ? "" : "s"}`);
  if (evidence.source_memory_ids.length) parts.push(`${evidence.source_memory_ids.length} source record${evidence.source_memory_ids.length === 1 ? "" : "s"}`);
  return parts.length ? parts.join(" · ") : "No derivation evidence — stated directly";
}

export function formatInstant(value: string): string {
  const date = new Date(value);
  return Number.isNaN(date.valueOf()) ? "Time unavailable" : date.toLocaleString();
}

export function shortAddress(value: string): string {
  return value.length > 18 ? `${value.slice(0, 10)}…${value.slice(-5)}` : value;
}

const CONTROL_CHARS = /[\u0000-\u001f\u007f]/;

export function labelError(what: string, value: string, required: boolean): string {
  if (!value && !required) return "";
  if (!value.trim()) return `${what} must be non-empty.`;
  if (new TextEncoder().encode(value).byteLength > 256) return `${what} must be 256 UTF-8 bytes or fewer.`;
  if (CONTROL_CHARS.test(value)) return `${what} must not contain control characters.`;
  return "";
}

export function parseContentJson(text: string): { value: unknown; error: string } {
  if (!text.trim()) return { value: undefined, error: "Content is required — a record with no body is not a memory." };
  try {
    return { value: JSON.parse(text), error: "" };
  } catch {
    return { value: undefined, error: "Content must be valid JSON (a string, number, boolean, object, array, or null)." };
  }
}

export function localInstantToIso(value: string): { iso: string; error: string } {
  if (!value) return { iso: "", error: "" };
  const date = new Date(value);
  if (Number.isNaN(date.valueOf())) return { iso: "", error: "Use a valid date and time." };
  return { iso: date.toISOString(), error: "" };
}

export function authorFromFields(type: string, id: string): ProvenanceAuthor | null {
  if (type === "human" && id) return { type: "human", human_id: id };
  if (type === "agent" && id) return { type: "agent", agent_id: id };
  if (type === "distiller" && id) return { type: "distiller", name: id };
  return null;
}

export function mintCorrectionId() {
  return `studio-correction-${crypto.randomUUID()}`;
}
