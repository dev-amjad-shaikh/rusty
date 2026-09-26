import type { Assistant, AssistantIntent, AssistantVersion } from "../engine/net/client";

/** The six category colours the prototype paints tiles with. */
export const COLORS: Record<string, string> = { blue: "var(--cat-blue)", teal: "var(--cat-teal)", plum: "var(--cat-plum)", amber: "var(--cat-amber)", rose: "var(--cat-rose)", orange: "var(--cat-orange)" };
export const COLOR_BG: Record<string, string> = { blue: "var(--cat-blue-bg)", teal: "var(--cat-teal-bg)", plum: "var(--cat-plum-bg)", amber: "var(--cat-amber-bg)", rose: "var(--cat-rose-bg)", orange: "var(--cat-orange-bg)" };
export const ICONS = ["ti-headset", "ti-robot", "ti-chart-pie", "ti-mail-fast", "ti-code", "ti-search", "ti-hierarchy-2", "ti-bug", "ti-brain", "ti-bolt"];

/** An agent's look: what the person chose (kept on the agent's metadata),
 * else one derived from its name so the same agent always looks the same. */
export interface Look { color: string; icon: string }

const hash = (s: string) => [...s].reduce((h, c) => (h * 31 + c.charCodeAt(0)) >>> 0, 7);
const ICON_BY_WORD: [RegExp, string][] = [
  [/triage|support|desk|ticket|concierge/i, "ti-headset"], [/revenue|analyst|report|metric|chart/i, "ti-chart-pie"], [/sdr|outreach|mail|email/i, "ti-mail-fast"],
  [/bug|repro|code|engineer/i, "ti-bug"], [/research|search|reader|web/i, "ti-search"], [/ontology|curator|graph/i, "ti-hierarchy-2"], [/coach|consolidat|composer|improve/i, "ti-brain"], [/invoice|billing|reconcil/i, "ti-file-invoice"],
];
export function lookOf(a: { name: string; metadata?: Assistant["metadata"] & { studio?: { color?: string; icon?: string } } }): Look {
  const chosen = (a.metadata as { studio?: { color?: string; icon?: string } } | undefined)?.studio;
  const keys = Object.keys(COLORS);
  const color = chosen?.color && COLORS[chosen.color] ? chosen.color : keys[hash(a.name) % keys.length];
  const icon = chosen?.icon || ICON_BY_WORD.find(([re]) => re.test(a.name))?.[1] || "ti-robot";
  return { color, icon };
}

/** `Support Triage` → `support-triage`. */
export const slug = (s: string) => s.toLowerCase().trim().replace(/&/g, "and").replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "");

/** The platform's own agents: made by the service, not a person. */
/** The platform's own three agents — the ones Rusty seeds — which the
 * workspace does not list as a person's work. An agent built by the Composer
 * on someone's behalf, or spawned by another agent, is theirs and is listed:
 * keying this off `kind: "service"` hid every one of them. */
export const isPlatform = (a: Assistant) => (a.metadata as { created_by?: { principal_id?: string } } | undefined)?.created_by?.principal_id === "rusty";

/** Draft or published: an agent whose newest version is not the active one
 * has unpublished work. Without the version list, an agent with no active
 * version is a draft. */
/** Published = the newest version is the one that runs AND the studio published it once (or it has run before the mark existed). Draft otherwise — a fresh agent is a draft until Publish. */
export function statusOf(a: Assistant, versions?: AssistantVersion[], everRan = false): "draft" | "published" {
  if (!a.active_version_id) return "draft";
  const marked = !!(a.metadata as { studio?: { published_at?: string } } | undefined)?.studio?.published_at;
  if (!marked && !everRan) return "draft";
  if (!versions?.length) return "published";
  const newest = [...versions].sort((x, y) => y.created_at.localeCompare(x.created_at))[0];
  return newest.version_id === a.active_version_id ? "published" : "draft";
}

export const intentOf = (a: Assistant | null | undefined): AssistantIntent => a?.config?.studio_intent ?? {};

/** `{{name}}` placeholders in a charter. */
export const variablesIn = (text: string): string[] => [...new Set([...text.matchAll(/\{\{\s*([a-zA-Z_][\w.]*)\s*\}\}/g)].map((m) => m[1]))];

/** A rough token count: the prototype's rule, four characters a token. */
export const tokensOf = (text: string) => Math.max(0, Math.round(text.trim().length / 4));

/** `3 min ago`, `2h ago`, `yesterday`. */
export function ago(iso: string | null | undefined, now = Date.now()): string {
  if (!iso) return "—";
  const t = Date.parse(iso); if (Number.isNaN(t)) return "—";
  const s = Math.max(0, Math.round((now - t) / 1000));
  if (s < 60) return "just now";
  const m = Math.round(s / 60); if (m < 60) return `${m}m ago`;
  const h = Math.round(m / 60); if (h < 24) return `${h}h ago`;
  const d = Math.round(h / 24); if (d === 1) return "Yesterday";
  if (d < 14) return `${d} days ago`;
  return new Date(t).toLocaleDateString(undefined, { month: "short", day: "numeric" });
}

/** `12.4k`. */
export const compact = (n: number) => (n >= 1000 ? `${(n / 1000).toFixed(n >= 10_000 ? 0 : 1)}k` : String(n));

/** A tool's connector: `servicenow.list-records` → `servicenow`; a built-in has none. */
export const connectorOf = (tool: string) => (tool.includes(".") ? tool.split(".")[0].replace(/@.*$/, "") : null);

/** The icon a tool row shows, by what it does. */
export function toolIcon(name: string, effect?: string): string {
  const n = name.toLowerCase();
  if (/search|list|get|read|fetch|lookup|find/.test(n)) return "ti-search";
  if (/create|post|send|write|file|add/.test(n)) return "ti-pencil";
  if (/delete|remove|void|archive/.test(n)) return "ti-alert-triangle";
  if (/memory|recall|remember/.test(n)) return "ti-brain";
  if (/ask|agents/.test(n)) return "ti-arrows-split";
  if (effect === "non_idempotent") return "ti-arrow-up-right-circle";
  return "ti-tool";
}

/** Read / Write / Destructive, from the effect class. */
export function riskOf(effect: string): "read" | "write" | "destructive" {
  if (effect === "read_only" || effect === "pure") return "read";
  if (effect === "non_idempotent") return "destructive";
  return "write";
}
