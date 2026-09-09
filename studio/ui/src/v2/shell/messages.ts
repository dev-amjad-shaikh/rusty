// Phase 0 message catalog. All user-facing copy goes through here so the
// strings stay i18n-ready (R-X4); the catalog swaps for a locale bundle later.

export const messages = {
  "brand.name": "Rustynome",
  "brand.mark": "R",

  "nav.group.build": "Build",
  "nav.group.test": "Test",
  "nav.group.operate": "Operate",
  "nav.agents": "Agents",
  "nav.drafts": "Drafts",
  "nav.catalog": "Catalog",
  "nav.playground": "Playground",
  "nav.evals": "Evals",
  "nav.inbox": "Inbox",
  "nav.work": "Work",
  "nav.observe": "Observe",
  "nav.learning": "Learning",
  "nav.improve": "Improve",
  "nav.security": "Security",

  "banner.reconnecting": "Showing last snapshot · seq {seq} · live updates paused",
  "banner.offline": "Offline · counts and lists may be stale · actions are queued, not applied",
  "banner.retry": "Retry",

  "shell.theme.light": "Light",
  "shell.theme.dark": "Dark",
  "shell.theme.toggle": "Toggle theme",
  "shell.role": "Role",
  "shell.role.admin": "Admin",
  "shell.role.builder": "Builder",
  "shell.role.operator": "Operator",
  "shell.role.auditor": "Auditor",

  "screen.pending": "This screen is not built yet — it lands with its own story.",
  "screen.none_permitted": "This role has no permitted screens.",

  "a11y.skip": "Skip to content",
  "a11y.nav": "Studio navigation",
  "a11y.banner": "Connection status",
} as const;

export type MessageKey = keyof typeof messages;

export function t(key: MessageKey, params: Record<string, string | number> = {}): string {
  return Object.entries(params).reduce(
    (text, [name, value]) => text.replaceAll(`{${name}}`, String(value)),
    messages[key] as string,
  );
}
