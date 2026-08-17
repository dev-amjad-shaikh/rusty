const STORAGE_KEY = "rusty-studio:recent-work:v2";
const MAX_RUNS = 12;

export interface RecentWorkIdentity {
  threadId: string;
  runId: string;
  savedAt: string;
}

export function readRecentWork(): RecentWorkIdentity[] {
  return readStorage().filter(validIdentity).slice(0, MAX_RUNS);
}

export function rememberRecentWork(value: Omit<RecentWorkIdentity, "savedAt">) {
  if (!validId(value.threadId) || !validId(value.runId)) return readRecentWork();
  const next = [{ ...value, savedAt: new Date().toISOString() }, ...readStorage().filter((item) => item.runId !== value.runId)].slice(0, MAX_RUNS);
  writeStorage(next);
  return next;
}

function readStorage(): RecentWorkIdentity[] {
  try {
    const value = JSON.parse(sessionStorage.getItem(STORAGE_KEY) ?? "null") as unknown;
    return Array.isArray(value) ? value.filter(validIdentity).slice(0, MAX_RUNS) : [];
  } catch {
    return [];
  }
}

function writeStorage(value: RecentWorkIdentity[]) {
  try { sessionStorage.setItem(STORAGE_KEY, JSON.stringify(value)); } catch { /* continuation remains available for this page */ }
}

function validId(value: unknown): value is string { return typeof value === "string" && value.length > 0 && value.length <= 256 && !/[\u0000-\u001f\u007f]/.test(value); }
function validIdentity(value: unknown): value is RecentWorkIdentity {
  if (!value || typeof value !== "object") return false;
  const item = value as Partial<RecentWorkIdentity>;
  return validId(item.threadId) && validId(item.runId) && typeof item.savedAt === "string" && Number.isFinite(Date.parse(item.savedAt));
}
