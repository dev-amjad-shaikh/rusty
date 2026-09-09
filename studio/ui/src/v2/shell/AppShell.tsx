// Studio v2 app shell (handoff 01 layout shell, 02 IA): canvas, frame,
// grouped sidebar, main card, connection banner, role/scope gating.
// Router-agnostic in Phase 0 — the active route arrives as a prop and nav
// clicks surface through onNavigate; S02+ wires the router underneath.

import { useEffect, useMemo, useState, type ReactNode } from "react";
import type { ConnectionState } from "../api/socket";
import { Button, Select } from "../controls/controls";
import { t, type MessageKey } from "./messages";
import {
  NAV,
  ROLE_SCOPES,
  isRoutePermitted,
  nearestPermittedRoute,
  visibleGroups,
  type Role,
} from "./scopes";
import styles from "./AppShell.module.css";

export interface StudioShellProps {
  role: Role;
  onRoleChange?: (role: Role) => void;
  connection: ConnectionState;
  onRetry?: () => void;
  activeRoute: string;
  onNavigate?: (route: string) => void;
  /** Screen content; Phase 0 renders a placeholder when absent. */
  children?: ReactNode;
}

function ConnectionBanner(props: { connection: ConnectionState; onRetry?: () => void }) {
  const { connection, onRetry } = props;
  if (connection.kind !== "reconnecting" && connection.kind !== "offline") return null;
  const text = connection.kind === "reconnecting"
    ? t("banner.reconnecting", { seq: connection.lastSeq })
    : t("banner.offline");
  const tone = connection.kind === "reconnecting" ? styles.bannerWarn : styles.bannerErr;
  return (
    <div className={`${styles.banner} ${tone}`} role="status" aria-live="polite" aria-label={t("a11y.banner")}>
      <span>{text}</span>
      <Button variant="secondary" small onClick={onRetry}>{t("banner.retry")}</Button>
    </div>
  );
}

export function StudioShell(props: StudioShellProps) {
  const { role, onRoleChange, connection, onRetry, activeRoute, onNavigate, children } = props;
  const [theme, setTheme] = useState<"light" | "dark">("light");
  const scopes = ROLE_SCOPES[role];
  const groups = useMemo(() => visibleGroups(scopes), [scopes]);

  // R-X2: a role change re-routes when the current screen falls out of scope.
  useEffect(() => {
    if (!isRoutePermitted(activeRoute, scopes)) {
      const nearest = nearestPermittedRoute(activeRoute, scopes);
      if (nearest && nearest !== activeRoute) onNavigate?.(nearest);
    }
  }, [role, activeRoute, scopes, onNavigate]);

  const activeItem = NAV.flatMap((group) => group.items).find(
    (item) => activeRoute === item.route || activeRoute.startsWith(`${item.route}/`),
  );
  const permitted = isRoutePermitted(activeRoute, scopes);

  return (
    <div className={styles.canvas} data-theme={theme}>
      <a className={styles.skipLink} href="#studio-v2-main">{t("a11y.skip")}</a>
      <div className={styles.frame}>
        <aside className={styles.sidebar}>
          <div className={styles.logo}>
            <span className={styles.logoMark} aria-hidden="true">{t("brand.mark")}</span>
            <span className={styles.logoName}>{t("brand.name")}</span>
          </div>
          <nav className={styles.nav} aria-label={t("a11y.nav")}>
            {groups.map((group) => (
              <section key={group.id} className={styles.navGroup} aria-labelledby={`v2-nav-${group.id}`}>
                <h2 className={styles.navGroupLabel} id={`v2-nav-${group.id}`}>{t(group.labelKey as MessageKey)}</h2>
                {group.items.map((item) => {
                  const active = item === activeItem;
                  return (
                    <a
                      key={item.route}
                      className={`${styles.navItem} ${active ? styles.navItemActive : ""}`}
                      href={item.route}
                      aria-current={active ? "page" : undefined}
                      onClick={(event) => {
                        event.preventDefault();
                        onNavigate?.(item.route);
                      }}
                    >
                      <svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
                        <path d={item.icon} />
                      </svg>
                      {t(item.labelKey as MessageKey)}
                    </a>
                  );
                })}
              </section>
            ))}
          </nav>
          <div className={styles.footer}>
            <Button variant="secondary" small onClick={() => setTheme(theme === "light" ? "dark" : "light")} ariaLabel={t("shell.theme.toggle")}>
              {theme === "light" ? t("shell.theme.dark") : t("shell.theme.light")}
            </Button>
            <div className={styles.userRow}>
              <span className={styles.avatar} aria-hidden="true">{t("brand.mark")}</span>
              <span className={styles.roleSelect}>
                <Select
                  value={role}
                  ariaLabel={t("shell.role")}
                  onChange={(value) => onRoleChange?.(value as Role)}
                  options={(["admin", "builder", "operator", "auditor"] as const).map((value) => ({
                    value,
                    label: t(`shell.role.${value}` as MessageKey),
                  }))}
                />
              </span>
            </div>
          </div>
        </aside>
        <div className={styles.mainCard}>
          <ConnectionBanner connection={connection} onRetry={onRetry} />
          <main className={styles.main} id="studio-v2-main" tabIndex={-1}>
            {children ?? (
              permitted && activeItem ? (
                <>
                  <h1 className={styles.screenTitle}>{t(activeItem.labelKey as MessageKey)}</h1>
                  <p className={styles.screenPending}>{t("screen.pending")}</p>
                </>
              ) : (
                <p className={styles.screenPending}>{t("screen.none_permitted")}</p>
              )
            )}
          </main>
        </div>
      </div>
    </div>
  );
}
