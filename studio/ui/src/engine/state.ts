import { create } from "zustand";
export type Role = "admin" | "builder" | "operator" | "auditor";

export type Theme = "light" | "dark";
/** `partial`: the server answered, but one of the calls the studio depends
 * on failed (a refusal, a fault) — what depends on it is empty and the
 * banner names it; not an outage. */
export type ConnState = "live" | "reconnecting" | "offline" | "partial";
/** Where the workspace's records come from. `demo` means this browser's own
 * simulation: nothing has been published to a server and runs are replayed by
 * the studio's engine. `server` means the records were read from a Rusty
 * server. There is no server client yet, so every workspace starts as a demo
 * and the shell says so on every screen. */
export type TruthSource = "server" | "demo";

function read(key: string): string | null {
  try { return localStorage.getItem(key); } catch { return null; }
}
function write(key: string, value: string) {
  try { localStorage.setItem(key, value); } catch { /* private mode; ignore */ }
}

export function applyTheme(theme: Theme) {
  document.documentElement.setAttribute("data-theme", theme);
}

const initialTheme: Theme = read("rn.theme") === "dark" ? "dark" : "light";
const initialRole = (read("rn.role") as Role) || "admin";

interface EngineState {
  theme: Theme;
  role: Role;
  conn: { state: ConnState; seq: number; message: string };
  truth: TruthSource;
  setTheme: (t: Theme) => void;
  toggleTheme: () => void;
  setRole: (r: Role) => void;
  setConn: (c: Partial<EngineState["conn"]>) => void;
  setTruth: (t: TruthSource) => void;
}

export const useEngine = create<EngineState>((set, get) => ({
  theme: initialTheme,
  role: ["admin", "builder", "operator", "auditor"].includes(initialRole) ? initialRole : "admin",
  conn: { state: "live", seq: 0, message: "" },
  truth: "demo",
  setTruth: (truth) => set({ truth }),
  setTheme: (theme) => { applyTheme(theme); write("rn.theme", theme); set({ theme }); },
  toggleTheme: () => get().setTheme(get().theme === "light" ? "dark" : "light"),
  setRole: (role) => { write("rn.role", role); set({ role }); },
  setConn: (c) => set((s) => ({ conn: { ...s.conn, ...c } })),
}));

export function initTheme() {
  applyTheme(initialTheme);
}
