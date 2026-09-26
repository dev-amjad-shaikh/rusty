// What the server says, held where the screens can read it. This is not a
// second world: it is the one world, fetched. Nothing here is seeded, nothing
// persists to localStorage, and when the server is unreachable the screens show
// that rather than something that looks like data.

import { create } from "zustand";
import { useEngine } from "../state";
import { health, listAssistants, listRuns, listSkills, me, ServerError, type Assistant, type Me, type Run, type ServerSkill, onSignedOut } from "./client";

export type Reach = "unknown" | "reaching" | "up" | "down";

interface ServerState {
  reach: Reach;
  /** Why the server could not be reached, in the words the browser used. */
  problem: string | null;
  components: { component: string; status: string }[];
  assistants: Assistant[];
  runs: Run[];
  skills: ServerSkill[];
  /** Who this browser is to the server. `null` until the server answers. */
  me: Me | null;
  /** The server is up and answered 401: this browser's key (or lack of one)
   * was refused. Distinct from unreachable, which is a different problem
   * with a different fix. */
  refused: boolean;
  /** Ask the server what it has. Safe to call repeatedly. */
  refresh: () => Promise<void>;
  /** A light check while the server reads as up: one health call; a wire
   * failure marks the server down (the snapshot stays, the banner comes up). */
  probe: () => Promise<void>;
}

export const useServer = create<ServerState>((set, get) => ({
  reach: "unknown",
  problem: null,
  components: [],
  assistants: [],
  runs: [],
  skills: [],
  me: null,
  refused: false,

  refresh: async () => {
    set({ reach: "reaching" });
    try {
      const h = await health();
      // Ask for everything the studio shows. What answered is shown; what
      // did not is empty and named — a server that answers is up, whatever
      // one of its calls said, and "not answering" is kept for the wire.
      const asked = await Promise.allSettled([listAssistants(), listRuns(), listSkills(), me()]);
      const [assistants, runs, skills, who] = asked;
      const failed = asked.find((a): a is PromiseRejectedResult => a.status === "rejected");
      if (failed && failed.reason instanceof ServerError && (failed.reason.status === 401 || failed.reason.status === 0)) throw failed.reason;
      if (failed && !(failed.reason instanceof ServerError)) throw failed.reason;
      const person = who.status === "fulfilled" ? who.value : null;
      set({
        reach: "up",
        problem: failed ? `${failed.reason.path} → ${failed.reason.message}` : null,
        components: h.components,
        assistants: assistants.status === "fulfilled" ? assistants.value : [],
        runs: runs.status === "fulfilled" ? runs.value : [],
        skills: skills.status === "fulfilled" ? skills.value : [],
        me: person,
        refused: false,
      });
      // The role is the server's to say. The studio's role switch was a
      // preview of this; with a principal answering, it is no longer a choice.
      if (person?.roles[0]) useEngine.getState().setRole(person.roles[0]);
      // The records on screen now come from a server, so the demo banner is no
      // longer true and comes down.
      useEngine.getState().setTruth("server");
      useEngine.getState().setConn(
        failed
          ? { state: "partial", message: `The server answered, but ${failed.reason.path} failed: ${failed.reason.message} · what depends on it is empty until it answers` }
          : { state: "live", message: "" },
      );
    } catch (err) {
      // Nobody signed in is not an outage: the server is up and answering.
      // Say exactly that, keep the world marked real, and let the shell
      // show the sign-in — no demo banner, no "unreachable".
      if (err instanceof ServerError && err.status === 401) {
        set({ reach: "up", problem: "nobody is signed in", components: [], assistants: [], runs: [], skills: [], me: null, refused: true });
        useEngine.getState().setTruth("server");
        useEngine.getState().setConn({ state: "live", message: "" });
        return;
      }
      // A refusal is not an outage. A server with principals answers 401 to
      // a browser presenting no key, or the wrong one; saying "unreachable"
      // for that sends someone to restart a server that is fine.
      const problem = err instanceof ServerError
        ? (err.status === 0 ? err.message : `${err.path} → ${err.message}`)
        : "the server could not be reached";
      lost(set, problem);
    }
  },

  probe: async () => {
    if (get().reach !== "up") return;
    try {
      await health();
    } catch (err) {
      if (err instanceof ServerError && err.status === 0) lost(set, err.message);
    }
  },
}));

/** The server stopped answering. What was on screen stays as the last
 * snapshot — the person, the lists — under a banner that says none of it
 * is live; emptying it would only make the outage look like an empty
 * deployment. */
function lost(set: (partial: Partial<ServerState>) => void, problem: string) {
  set({ reach: "down", problem, refused: false });
  useEngine.getState().setTruth("demo");
  useEngine.getState().setConn({ state: "offline", message: `Rusty server unreachable · ${problem}` });
}

// A 401 anywhere means the session ended: the shell sends the person to
// sign in, and every list empties rather than pretending.
onSignedOut(() => {
  useServer.setState({ refused: true, me: null, problem: "your session ended — sign in again" });
});
