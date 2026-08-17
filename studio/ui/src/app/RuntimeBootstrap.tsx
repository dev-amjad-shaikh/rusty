import { useEffect } from "react";
import { getServerInfo, StudioApiError } from "../lib/api/client";
import { useRuntimeStore } from "../state/runtime";

export const RUNTIME_POLL_MS = 2_000;

// Polls the local backend until it answers. While Rusty is still compiling or
// starting the store stays in "starting"; only a definitive refusal (an access
// key Studio does not have, or a payload that is not Rusty) flips to
// "unavailable".
export function RuntimeBootstrap() {
  const status = useRuntimeStore((state) => state.status);
  const attempt = useRuntimeStore((state) => state.attempt);

  useEffect(() => {
    if (status !== "starting") return;
    let cancelled = false;
    const poll = async () => {
      try {
        const info = await getServerInfo();
        if (!cancelled) useRuntimeStore.getState().accept(attempt, info);
      } catch (error) {
        if (cancelled) return;
        if (error instanceof StudioApiError && (error.status === 401 || error.status === 403)) {
          useRuntimeStore.getState().fail(attempt, "This Rusty server needs an access key. Set VITE_RUSTY_API_KEY and restart Studio.");
        } else if (error instanceof StudioApiError && error.status !== 0 && error.status < 500) {
          useRuntimeStore.getState().fail(attempt, error.message);
        }
        // Anything else means the backend is still booting; keep polling.
      }
    };
    void poll();
    const timer = window.setInterval(() => void poll(), RUNTIME_POLL_MS);
    return () => { cancelled = true; window.clearInterval(timer); };
  }, [attempt, status]);

  return null;
}
