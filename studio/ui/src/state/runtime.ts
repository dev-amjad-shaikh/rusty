import { create } from "zustand";
import type { ServerInfo } from "../lib/contracts";

// Studio runs against exactly one Rusty backend: the local one. This store
// tracks whether that backend has answered yet.
interface RuntimeState {
  info: ServerInfo | null;
  status: "starting" | "ready" | "unavailable";
  error: string;
  attempt: number;
  retry: () => void;
  accept: (attempt: number, info: ServerInfo) => void;
  fail: (attempt: number, error: string) => void;
}

export const useRuntimeStore = create<RuntimeState>((set) => ({
  info: null,
  status: "starting",
  error: "",
  attempt: 0,
  retry: () => set((state) => ({ info: null, status: "starting", error: "", attempt: state.attempt + 1 })),
  accept: (attempt, info) => set((state) => state.attempt === attempt && state.status === "starting"
    ? { info, status: "ready" as const, error: "" } : {}),
  fail: (attempt, error) => set((state) => state.attempt === attempt && state.status === "starting"
    ? { status: "unavailable" as const, error } : {}),
}));
