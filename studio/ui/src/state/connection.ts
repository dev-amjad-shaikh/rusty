import { create } from "zustand";
import type { ServerInfo } from "../lib/contracts";
import type { ConnectionIdentity } from "../lib/api/client";

interface ConnectionState {
  connection: ConnectionIdentity | null;
  info: ServerInfo | null;
  dialogOpen: boolean;
  openDialog: () => void;
  closeDialog: () => void;
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
  dialogOpen: false,
  openDialog: () => set({ dialogOpen: true }),
  closeDialog: () => set({ dialogOpen: false }),
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
      dialogOpen: false,
    });
  },
  disconnect: () => set({ connection: null, info: null, dialogOpen: false }),
}));
