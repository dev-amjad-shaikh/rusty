import type { ConnectionIdentity } from "../lib/api/client";

const STORAGE_KEY = "rusty-studio:recent-work:v1";
const MAX_CONNECTIONS = 5;
const MAX_RUNS = 12;

export interface RecentWorkIdentity {
  threadId: string;
  runId: string;
  savedAt: string;
}

interface StoredRecentWork {
  scopes: Record<string, RecentWorkIdentity[]>;
  order: string[];
}

export function durableConnectionScope(connection: ConnectionIdentity) {
  return JSON.stringify([connection.origin.replace(/\/$/, ""), connection.tenantFingerprint]);
}

export function readRecentWork(scope: string): RecentWorkIdentity[] {
  const stored = readStorage();
  return (stored.scopes[scope] ?? []).filter(validIdentity).slice(0, MAX_RUNS);
}

export function rememberRecentWork(scope: string, value: Omit<RecentWorkIdentity, "savedAt">) {
  if (!validId(value.threadId) || !validId(value.runId)) return readRecentWork(scope);
  const stored = readStorage();
  const next = [{ ...value, savedAt: new Date().toISOString() }, ...(stored.scopes[scope] ?? []).filter((item) => item.runId !== value.runId)].slice(0, MAX_RUNS);
  const order = [scope, ...stored.order.filter((item) => item !== scope)].slice(0, MAX_CONNECTIONS);
  const scopes = Object.fromEntries(order.map((item) => [item, item === scope ? next : (stored.scopes[item] ?? []).filter(validIdentity).slice(0, MAX_RUNS)]));
  writeStorage({ scopes, order });
  return next;
}

function readStorage(): StoredRecentWork {
  try {
    const value = JSON.parse(sessionStorage.getItem(STORAGE_KEY) ?? "null") as Partial<StoredRecentWork> | null;
    if (!value || !value.scopes || typeof value.scopes !== "object" || !Array.isArray(value.order)) return emptyStorage();
    const order = value.order.filter((item): item is string => typeof item === "string").slice(0, MAX_CONNECTIONS);
    const scopes = Object.fromEntries(order.map((scope) => [scope, Array.isArray(value.scopes?.[scope]) ? value.scopes[scope] : []]));
    return { scopes, order };
  } catch {
    return emptyStorage();
  }
}

function writeStorage(value: StoredRecentWork) {
  try { sessionStorage.setItem(STORAGE_KEY, JSON.stringify(value)); } catch { /* continuation remains available for this page */ }
}

function emptyStorage(): StoredRecentWork { return { scopes: {}, order: [] }; }
function validId(value: unknown): value is string { return typeof value === "string" && value.length > 0 && value.length <= 256 && !/[\u0000-\u001f\u007f]/.test(value); }
function validIdentity(value: unknown): value is RecentWorkIdentity {
  if (!value || typeof value !== "object") return false;
  const item = value as Partial<RecentWorkIdentity>;
  return validId(item.threadId) && validId(item.runId) && typeof item.savedAt === "string" && Number.isFinite(Date.parse(item.savedAt));
}
