// Roles → scopes and render-time gating (handoff 02). Capabilities are
// absent, never disabled; a role change re-routes to the nearest permitted
// screen (R-X2).

export type Role = "admin" | "builder" | "operator" | "auditor";

export const ROLE_SCOPES: Record<Role, readonly string[]> = {
  admin: ["*"],
  builder: ["blueprints:read", "blueprints:write", "blueprints:publish", "catalog:read", "evals:*", "observe:read"],
  operator: ["blueprints:read", "catalog:read", "tasks:*", "approvals:decide", "observe:read", "evals:read", "learning:read"],
  auditor: ["observe:read", "audit:read", "security:read", "approvals:read", "learning:read"],
};

/** Scope matching with the `*` and `<prefix>:*` wildcard forms from the role table. */
export function hasScope(held: readonly string[], needed: string): boolean {
  return held.some((scope) =>
    scope === "*" || scope === needed || (scope.endsWith(":*") && needed.startsWith(scope.slice(0, -1)))
  );
}

export function hasAnyScope(held: readonly string[], needed: readonly string[]): boolean {
  return needed.some((scope) => hasScope(held, scope));
}

export interface NavItem {
  route: string;
  labelKey: string;
  /** Any of these scopes admits the item. */
  scopes: readonly string[];
  icon: string;
}

export interface NavGroup {
  id: "build" | "test" | "operate";
  labelKey: string;
  items: readonly NavItem[];
}

const ICONS = {
  agents: "M12 3l7 4v10l-7 4-7-4V7l7-4zM12 8v8M8.5 6l7 4M15.5 6l-7 4",
  drafts: "M6 3h8l4 4v14H6V3zM14 3v4h4M9 12h6M9 16h6",
  catalog: "M4 5h16v4H4zM4 11h7v8H4zM13 11h7v8h-7z",
  playground: "M8 5l11 7-11 7V5z",
  evals: "M4 17l5-6 4 3 7-9M4 21h16",
  inbox: "M4 6h16v12H4zM4 9l8 5 8-5",
  work: "M4 6h5v13H4zM10 6h5v9h-5zM16 6h5v5h-5z",
  observe: "M3 12h4l3-8 4 16 3-8h4",
  learning: "M12 4a8 8 0 108 8M12 8a4 4 0 104 4M12 12h.01",
  improve: "M4 20L14 10M14 4l6 6-4 4-6-6 4-4zM4 20l1-4 3 3-4 1z",
  security: "M12 3l7 3v6c0 4-3 7-7 9-4-2-7-5-7-9V6l7-3z",
} as const;

/** Sidebar navigation in IA order (handoff 02). */
export const NAV: readonly NavGroup[] = [
  {
    id: "build",
    labelKey: "nav.group.build",
    items: [
      { route: "/agents", labelKey: "nav.agents", scopes: ["blueprints:read"], icon: ICONS.agents },
      { route: "/drafts", labelKey: "nav.drafts", scopes: ["blueprints:read"], icon: ICONS.drafts },
      { route: "/catalog", labelKey: "nav.catalog", scopes: ["catalog:read"], icon: ICONS.catalog },
    ],
  },
  {
    id: "test",
    labelKey: "nav.group.test",
    items: [
      { route: "/playground", labelKey: "nav.playground", scopes: ["evals:run"], icon: ICONS.playground },
      { route: "/evals", labelKey: "nav.evals", scopes: ["evals:read"], icon: ICONS.evals },
    ],
  },
  {
    id: "operate",
    labelKey: "nav.group.operate",
    items: [
      { route: "/inbox", labelKey: "nav.inbox", scopes: ["approvals:decide", "approvals:read"], icon: ICONS.inbox },
      { route: "/work", labelKey: "nav.work", scopes: ["tasks:read"], icon: ICONS.work },
      { route: "/observe", labelKey: "nav.observe", scopes: ["observe:read"], icon: ICONS.observe },
      { route: "/learning", labelKey: "nav.learning", scopes: ["learning:read"], icon: ICONS.learning },
      { route: "/improve", labelKey: "nav.improve", scopes: ["blueprints:write"], icon: ICONS.improve },
      { route: "/security", labelKey: "nav.security", scopes: ["security:read"], icon: ICONS.security },
    ],
  },
];

/** Groups with out-of-scope items removed; a group with nothing visible is absent entirely. */
export function visibleGroups(scopes: readonly string[]): NavGroup[] {
  return NAV
    .map((group) => ({ ...group, items: group.items.filter((item) => hasAnyScope(scopes, item.scopes)) }))
    .filter((group) => group.items.length > 0);
}

function itemForRoute(route: string): NavItem | null {
  for (const group of NAV) {
    for (const item of group.items) {
      if (route === item.route || route.startsWith(`${item.route}/`)) return item;
    }
  }
  return null;
}

export function isRoutePermitted(route: string, scopes: readonly string[]): boolean {
  const item = itemForRoute(route);
  return item !== null && hasAnyScope(scopes, item.scopes);
}

/**
 * Where a role change lands when the current screen falls out of scope: the
 * current route when still permitted, otherwise the first permitted route in
 * nav order. `null` when the role holds no screen scope at all.
 */
export function nearestPermittedRoute(route: string, scopes: readonly string[]): string | null {
  if (isRoutePermitted(route, scopes)) return route;
  for (const group of visibleGroups(scopes)) {
    if (group.items.length) return group.items[0].route;
  }
  return null;
}
