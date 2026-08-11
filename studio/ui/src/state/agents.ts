import { create } from "zustand";

interface AgentMutationState {
  uncertainByConnection: Record<string, string>;
  markUncertain: (connectionKey: string, message: string) => void;
  clearUncertain: (connectionKey: string) => void;
}

export const useAgentMutationStore = create<AgentMutationState>((set) => ({
  uncertainByConnection: {},
  markUncertain: (connectionKey, message) => set((state) => ({
    uncertainByConnection: { ...state.uncertainByConnection, [connectionKey]: message },
  })),
  clearUncertain: (connectionKey) => set((state) => {
    const next = { ...state.uncertainByConnection };
    delete next[connectionKey];
    return { uncertainByConnection: next };
  }),
}));
