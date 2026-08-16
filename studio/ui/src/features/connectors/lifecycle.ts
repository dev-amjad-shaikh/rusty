import type { LifecycleState } from "../../lib/api/connectors";

export interface StatePresentation {
  label: string;
  tone: "neutral" | "active" | "good" | "warn" | "bad" | "off";
  summary: string;
}

export const statePresentation: Record<LifecycleState, StatePresentation> = {
  pending: { label: "pending", tone: "neutral", summary: "Registered, never connected" },
  connecting: { label: "connecting", tone: "active", summary: "Connection attempt in flight" },
  healthy: { label: "healthy", tone: "good", summary: "Connected and passing health checks" },
  degraded: { label: "degraded", tone: "warn", summary: "Connected but failing health checks" },
  failed: { label: "failed", tone: "bad", summary: "Connection or instantiation failed" },
  disabled: { label: "disabled", tone: "off", summary: "Parked; rejects connection attempts" },
};

/// Which lifecycle actions the server admits from each state. The server
/// remains the authority — a guard violation is a 409 and is surfaced —
/// but the buttons follow these gates so the fleet does not offer actions
/// that can only fail.
export function allowedActions(state: LifecycleState) {
  return {
    connect: state === "pending" || state === "failed" || state === "degraded",
    health: state === "healthy" || state === "degraded",
    disable: state !== "disabled",
    enable: state === "disabled",
  };
}

export function shortHash(hash: string) {
  return hash.slice(0, 12);
}

export function healthCheckTime(lastHealthCheckMs: number | null) {
  if (lastHealthCheckMs === null) return "Never checked";
  const date = new Date(lastHealthCheckMs);
  return Number.isNaN(date.valueOf()) ? "Check time unavailable" : date.toLocaleString();
}

export const effectLabels: Record<string, string> = {
  pure: "pure",
  read_only: "read-only",
  idempotent: "idempotent",
  compensatable: "compensatable",
  non_idempotent: "non-idempotent",
};

export function effectLabel(effect: string) {
  return effectLabels[effect] ?? effect;
}

export const providerKindLabels: Record<string, string> = {
  mcp_stdio: "mcp-stdio",
  http_search: "http-search",
};

export function providerKindLabel(kind: string) {
  return providerKindLabels[kind] ?? kind;
}
