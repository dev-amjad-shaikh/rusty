import { create } from "zustand";

interface AgentMutationState {
  uncertain: string;
  markUncertain: (message: string) => void;
  clearUncertain: () => void;
}

export const useAgentMutationStore = create<AgentMutationState>((set) => ({
  uncertain: "",
  markUncertain: (message) => set({ uncertain: message }),
  clearUncertain: () => set({ uncertain: "" }),
}));
