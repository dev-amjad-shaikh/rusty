// Time, the way the studio says it.

/** "just now" · "4m ago" · "2h ago" · "3d ago". */
export function ago(at: number, now = Date.now()): string {
  const s = Math.max(0, Math.round((now - at) / 1000));
  if (s < 60) return "just now";
  if (s < 3600) return `${Math.round(s / 60)}m ago`;
  if (s < 86400) return `${Math.round(s / 3600)}h ago`;
  return `${Math.round(s / 86400)}d ago`;
}

/** A wall-clock stamp, or a dash when the record carries none. */
export function clock(at?: number): string {
  return at ? new Date(at).toLocaleTimeString([], { hour12: false }) : "—";
}
