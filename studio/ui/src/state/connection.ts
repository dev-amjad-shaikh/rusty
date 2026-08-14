import { create } from "zustand";
import type { ServerInfo } from "../lib/contracts";
import type { ConnectionIdentity } from "../lib/api/client";

interface ConnectionState {
  connection: ConnectionIdentity | null;
  info: ServerInfo | null;
  workspaceStatus: "discovering" | "ready" | "unavailable";
  discoveryAttempt: number;
  discoveryError: string;
  suggestedOrigin: string;
  dialogOpen: boolean;
  openDialog: () => void;
  closeDialog: () => void;
  retryDiscovery: () => void;
  acceptDiscovery: (attempt: number, origin: string, info: ServerInfo) => Promise<void>;
  failDiscovery: (attempt: number, error: string, suggestedOrigin: string) => void;
  connect: (origin: string, apiKey: string, info: ServerInfo) => Promise<void>;
  disconnect: () => void;
}

let epoch = 0;
const pageSalt = crypto.getRandomValues(new Uint8Array(32));

async function fingerprint(apiKey: string) {
  const key = new TextEncoder().encode(apiKey || "open");
  const material = new Uint8Array(pageSalt.length + 1 + key.length);
  material.set(pageSalt); material[pageSalt.length] = 0; material.set(key, pageSalt.length + 1);
  const digest = await crypto.subtle.digest("SHA-256", material);
  return Array.from(new Uint8Array(digest).slice(0, 16), (byte) => byte.toString(16).padStart(2, "0")).join("");
}

export const useConnectionStore = create<ConnectionState>((set) => ({
  connection: null,
  info: null,
  workspaceStatus: "discovering",
  discoveryAttempt: 0,
  discoveryError: "",
  suggestedOrigin: "",
  dialogOpen: false,
  openDialog: () => set((state) => ({
    dialogOpen: true,
    workspaceStatus: state.connection ? "ready" : "unavailable",
  })),
  closeDialog: () => set({ dialogOpen: false }),
  retryDiscovery: () => set((state) => ({
    connection: null,
    info: null,
    dialogOpen: false,
    workspaceStatus: "discovering",
    discoveryAttempt: state.discoveryAttempt + 1,
    discoveryError: "",
  })),
  acceptDiscovery: async (attempt, origin, info) => {
    const tenantFingerprint = await fingerprint("");
    set((state) => {
      if (state.discoveryAttempt !== attempt || state.workspaceStatus !== "discovering"
        || state.dialogOpen || state.connection) return {};
      return {
        connection: {
          epoch: ++epoch,
          origin: origin.replace(/\/$/, ""),
          apiKey: "",
          tenantFingerprint,
        },
        info,
        workspaceStatus: "ready" as const,
        discoveryError: "",
        suggestedOrigin: origin,
      };
    });
  },
  failDiscovery: (attempt, error, suggestedOrigin) => set((state) => {
    if (state.discoveryAttempt !== attempt || state.workspaceStatus !== "discovering"
      || state.dialogOpen || state.connection) return {};
    return {
      workspaceStatus: "unavailable" as const,
      discoveryError: error,
      suggestedOrigin,
    };
  }),
  connect: async (origin, apiKey, info) => {
    const tenantFingerprint = await fingerprint(apiKey);
    set({
      connection: {
        epoch: ++epoch,
        origin: origin.replace(/\/$/, ""),
        apiKey,
        tenantFingerprint,
      },
      info,
      workspaceStatus: "ready",
      discoveryError: "",
      suggestedOrigin: origin.replace(/\/$/, ""),
      dialogOpen: false,
    });
  },
  disconnect: () => set({
    connection: null,
    info: null,
    workspaceStatus: "unavailable",
    discoveryError: "Choose a workspace to continue.",
    dialogOpen: false,
  }),
}));
