import { create } from "zustand";

interface PromptMutationState {
  uncertainByConnection: Record<string, string>;
  markUncertain: (connectionKey: string, message: string) => void;
  clearUncertain: (connectionKey: string) => void;
}

export const usePromptMutationStore = create<PromptMutationState>((set) => ({
  uncertainByConnection: {},
  markUncertain: (connectionKey, message) => set((state) => ({ uncertainByConnection: { ...state.uncertainByConnection, [connectionKey]: message } })),
  clearUncertain: (connectionKey) => set((state) => {
    const next = { ...state.uncertainByConnection }; delete next[connectionKey]; return { uncertainByConnection: next };
  }),
}));
