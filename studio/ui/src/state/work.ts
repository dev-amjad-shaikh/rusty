import { create } from "zustand";
import { stringify as stringifyLossless } from "lossless-json";
import type { Assistant, RunEvidence, RunReceipt, RunSnapshot, Thread } from "../lib/contracts";

export interface EvaluationCase {
  id: string;
  caseId: string;
  runId: string;
  threadId: string;
  agentName: string;
  agentId: string;
  objective: string;
  pointer: string;
  expected: unknown;
  createdAt: string;
}

export interface ComparisonRun {
  run: RunSnapshot;
  evidence: RunEvidence;
  agentName: string;
  objective: string;
  capturedAt: string;
}

interface WorkState {
  assistant: Assistant | null;
  objective: string;
  thread: Thread | null;
  receipt: RunReceipt | null;
  cases: EvaluationCase[];
  comparisons: ComparisonRun[];
  uncertain: string;
  prepare: (assistant: Assistant) => void;
  expirePrepared: (assistantId: string, versionId: string) => void;
  begin: (assistant: Assistant, objective: string, thread: Thread, receipt: RunReceipt) => void;
  addCase: (value: Omit<EvaluationCase, "id" | "createdAt">) => void;
  rememberRun: (value: Omit<ComparisonRun, "capturedAt">) => void;
  markUncertain: (message: string) => void;
  clearUncertain: () => void;
  clear: () => void;
}

export const useWorkStore = create<WorkState>((set) => ({
  assistant: null,
  objective: "",
  thread: null,
  receipt: null,
  cases: [],
  comparisons: [],
  uncertain: "",
  prepare: (assistant) => set({ assistant, objective: "", thread: null, receipt: null }),
  expirePrepared: (assistantId, versionId) => set((state) => state.assistant?.assistant_id === assistantId
    && state.assistant.active_version_id === versionId
    && !state.receipt ? { assistant: null, thread: null } : {}),
  begin: (assistant, objective, thread, receipt) => set({ assistant, objective, thread, receipt }),
  addCase: (value) => set((state) => ({
    cases: [...state.cases, { ...value, id: crypto.randomUUID(), createdAt: new Date().toISOString() }],
  })),
  rememberRun: (value) => set((state) => ({
    comparisons: [...state.comparisons.filter((item) => item.run.run_id !== value.run.run_id), { ...value, capturedAt: new Date().toISOString() }].slice(-20),
  })),
  markUncertain: (message) => set({ uncertain: message }),
  clearUncertain: () => set({ uncertain: "" }),
  clear: () => set({ assistant: null, objective: "", thread: null, receipt: null, cases: [], comparisons: [] }),
}));

export function evaluationDatasetJsonl(cases: EvaluationCase[]) {
  const header = stringifyLossless({ kind: "header", format_version: 1, name: "rusty-studio-evaluations", version: "v1" });
  const lines = cases.map((item) => stringifyLossless({ kind: "case", id: item.caseId, input: { objective: item.objective }, expect: { state: [{ pointer: item.pointer, expected: item.expected }] }, tags: ["studio", item.agentName] }));
  return `${[header, ...lines].join("\n")}\n`;
}
