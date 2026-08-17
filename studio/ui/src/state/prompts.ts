import { create } from "zustand";

interface PromptMutationState {
  uncertain: string;
  markUncertain: (message: string) => void;
  clearUncertain: () => void;
}

export const usePromptMutationStore = create<PromptMutationState>((set) => ({
  uncertain: "",
  markUncertain: (message) => set({ uncertain: message }),
  clearUncertain: () => set({ uncertain: "" }),
}));
