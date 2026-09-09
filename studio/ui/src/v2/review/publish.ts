// Publish preconditions and the REST calls the Review rail makes (handoff 04
// "Publish preconditions", handoff 06 endpoints). Review is the only place
// Publish exists (R-A2); everything here is reachable from ReviewScreen and
// nowhere else.

import type { AgentDraft } from "../draft/agent-draft.gen";
import type { RestClient } from "../api/rest";
import { hasScope } from "../shell/scopes";
import type { Violation } from "../draft/validate";
import type { GateStatus } from "./gate";

export interface PublishReadiness {
  ready: boolean;
  /** Human-readable hints, in the order the button lists them. */
  reasons: string[];
}

/** The caller holds the publish scope for this blueprint (handoff 04). */
export function canPublish(scopes: readonly string[], agentId: string | undefined): boolean {
  if (hasScope(scopes, "blueprints:publish")) return true;
  return agentId !== undefined && hasScope(scopes, `blueprints:${agentId}:publish`);
}

/**
 * 0 violations ∧ eval gate Passing for the current draft content ∧ publish
 * scope. Anything less and the button stays disabled with the hint.
 */
export function publishReadiness(input: {
  violations: Violation[];
  gate: GateStatus;
  scopes: readonly string[];
  agentId?: string;
}): PublishReadiness {
  const reasons: string[] = [];
  if (input.violations.length > 0) {
    reasons.push(`${input.violations.length} open violation${input.violations.length === 1 ? "" : "s"}`);
  }
  if (input.gate !== "passing") {
    reasons.push(input.gate === "stale" ? "Eval gate is stale — re-run it" : "Eval gate is not Passing");
  }
  if (!canPublish(input.scopes, input.agentId)) {
    reasons.push("You do not hold the publish scope for this blueprint");
  }
  return { ready: reasons.length === 0, reasons };
}

/** What the versions endpoint returns on publish. */
export interface PublishResult {
  blueprint_id: string;
  version: number;
  /** Live sessions on the prior version — when present, offer fleet upgrade. */
  sessions?: number;
}

/** Wire shape for starting a gate run against draft content. */
export interface EvalRunHandle {
  run_id: string;
}

export interface EvalRunReport {
  status: "running" | "passed" | "failed";
  suite?: string;
  cases?: { name: string; pass: boolean; score: number }[];
}

/** Start the eval gate against the draft (handoff 06: POST eval-suites/{id}/run). */
export function runEvalGate(rest: RestClient, suite: string, draft: AgentDraft): Promise<{ data: EvalRunHandle; idempotencyKey: string }> {
  return rest.post<EvalRunHandle>(`/v1/eval-suites/${encodeURIComponent(suite)}/run`, { draft });
}

/** Poll a gate run (handoff 06: GET eval-runs/{id}). */
export function getEvalRun(rest: RestClient, runId: string): Promise<EvalRunReport> {
  return rest.get<EvalRunReport>(`/v1/eval-runs/${encodeURIComponent(runId)}`);
}

/**
 * Append an immutable version. New blueprints go through POST /v1/blueprints;
 * existing ones through POST /v1/blueprints/{id}/versions. The REST client
 * stamps the idempotency key; on 409 the head moved and the caller re-diffs.
 */
export function publishDraft(
  rest: RestClient,
  draft: AgentDraft,
): Promise<{ data: PublishResult; idempotencyKey: string }> {
  if (draft.agentId) {
    return rest.post<PublishResult>(
      `/v1/blueprints/${encodeURIComponent(draft.agentId)}/versions`,
      { draft },
    );
  }
  return rest.post<PublishResult>("/v1/blueprints", { draft });
}

export interface FleetUpgradeHandle {
  upgrade_id: string;
  sessions: number;
}

/** Fleet-upgrade offer accepted (R-A8, R-O7 hook): adoption at turn boundaries. */
export function startFleetUpgrade(
  rest: RestClient,
  blueprintId: string,
  version: number,
): Promise<{ data: FleetUpgradeHandle; idempotencyKey: string }> {
  return rest.post<FleetUpgradeHandle>(`/v1/blueprints/${encodeURIComponent(blueprintId)}/upgrade`, { version });
}
