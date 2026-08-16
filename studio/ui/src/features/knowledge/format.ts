import type { KnowledgeRetention } from "../../lib/api/knowledge";

export function formatBytes(value: number) {
  if (!Number.isFinite(value) || value < 0) return "—";
  if (value < 1024) return `${value} B`;
  const kib = value / 1024;
  if (kib < 1024) return `${Number.isInteger(kib) ? kib : kib.toFixed(1)} KiB`;
  return `${(kib / 1024).toFixed(2)} MiB`;
}

export function formatInstant(value: string) {
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return value;
  return `${date.toISOString().slice(0, 16).replace("T", " ")} UTC`;
}

export function hashPreview(hash: string) {
  return hash.length > 12 ? hash.slice(0, 12) : hash;
}

export type RetentionTone = "pinned" | "live" | "expired";

export function retentionState(retention: KnowledgeRetention, now = Date.now()): { label: string; tone: RetentionTone } {
  if (retention.policy === "pinned") return { label: "Pinned", tone: "pinned" };
  const expires = new Date(retention.expires_at).getTime();
  if (Number.isNaN(expires)) return { label: "Pinned", tone: "pinned" };
  return expires > now
    ? { label: `Expires ${formatInstant(retention.expires_at)}`, tone: "live" }
    : { label: `Expired ${formatInstant(retention.expires_at)}`, tone: "expired" };
}

export function bodyByteSize(body: string) {
  return new TextEncoder().encode(body).byteLength;
}

export const SOURCE_ID_PATTERN = /^[A-Za-z0-9._:-]+$/;
