// Eval gate state (handoff 03 Review rail): Not run / Running / Passing /
// Stale / Failing. The machine is pure — the screen feeds it run results from
// the REST client and re-derives staleness whenever the draft content moves.

import type { AgentDraft } from "../draft/agent-draft.gen";

export type GateStatus = "not_run" | "running" | "passing" | "stale" | "failing";

export interface GateCaseResult {
  name: string;
  pass: boolean;
  score: number;
}

export interface GateState {
  /** Run in flight, if any. */
  runId: string | null;
  /** Content fingerprint the last completed run evaluated. */
  contentHash: string | null;
  /** Streamed case results from the last run. */
  cases: GateCaseResult[];
  /** Overall pass/fail of the last completed run. */
  passed: boolean | null;
}

export const INITIAL_GATE: GateState = {
  runId: null,
  contentHash: null,
  cases: [],
  passed: null,
};

/** JSON with object keys sorted at every depth, so the fingerprint is stable. */
function stableStringify(value: unknown): string {
  if (Array.isArray(value)) return `[${value.map(stableStringify).join(",")}]`;
  if (value !== null && typeof value === "object") {
    const entries = Object.entries(value as Record<string, unknown>)
      .filter(([, v]) => v !== undefined)
      .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))
      .map(([k, v]) => `${JSON.stringify(k)}:${stableStringify(v)}`);
    return `{${entries.join(",")}}`;
  }
  return JSON.stringify(value);
}

/**
 * A stable fingerprint of the draft content the gate evaluates. Sorted JSON
 * folded with FNV-1a — cheap, deterministic, and good enough to detect "the
 * draft moved since the last run".
 */
export function contentHash(draft: AgentDraft): string {
  const stable = stableStringify(draft);
  let hash = 0x811c9dc5;
  for (let i = 0; i < stable.length; i++) {
    hash ^= stable.charCodeAt(i);
    hash = Math.imul(hash, 0x01000193) >>> 0;
  }
  return hash.toString(16).padStart(8, "0");
}

/** Mark a gate run started; prior results stay visible until it lands. */
export function gateStarted(state: GateState, runId: string): GateState {
  return { ...state, runId };
}

/** Record a streamed case result for the run in flight. */
export function gateCaseResult(state: GateState, result: GateCaseResult): GateState {
  const cases = [...state.cases.filter((c) => c.name !== result.name), result];
  return { ...state, cases };
}

/** Complete the run in flight against the fingerprint it evaluated. */
export function gateCompleted(
  state: GateState,
  hash: string,
  passed: boolean,
  cases: GateCaseResult[],
): GateState {
  return { runId: null, contentHash: hash, cases, passed };
}

/**
 * The rendered status. A completed run goes Stale as soon as the draft
 * content no longer matches the fingerprint the run evaluated.
 */
export function gateStatus(state: GateState, currentHash: string): GateStatus {
  if (state.runId !== null) return "running";
  if (state.passed === null) return "not_run";
  if (state.contentHash !== currentHash) return "stale";
  return state.passed ? "passing" : "failing";
}

export const GATE_LABEL: Record<GateStatus, string> = {
  not_run: "Not run",
  running: "Running",
  passing: "Passing",
  stale: "Stale",
  failing: "Failing",
};
