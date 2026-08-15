import { Link, Outlet, useRouterState } from "@tanstack/react-router";
import { Suspense, useEffect, useRef, useState } from "react";
import { ConnectionDialog } from "../features/connection/ConnectionDialog";
import { WorkspaceBootstrap, workspaceDisplayName } from "../features/connection/WorkspaceBootstrap";
import { useConnectionStore } from "../state/connection";
import { destinationForPath, lifecycleGroups } from "./navigation";
import styles from "./AppShell.module.css";

export function AppShell() {
  const pathname = useRouterState({ select: (state) => state.location.pathname });
  const mainRef = useRef<HTMLElement>(null);
  const previousPath = useRef(pathname);
  const [navigationOpen, setNavigationOpen] = useState(false);
  const { connection, info, workspaceStatus, openDialog } = useConnectionStore();
  const workspaceName = connection ? workspaceDisplayName(connection.origin)
    : workspaceStatus === "discovering" ? "Opening workspace" : "Workspace unavailable";
  const workspaceDetail = connection && info ? `Rusty ${info.version}`
    : workspaceStatus === "discovering" ? "Finding Rusty on this device" : "Choose or retry a workspace";
  const routeName = destinationForPath(pathname)?.label ?? "Studio";
  useEffect(() => {
    if (previousPath.current === pathname) return;
    previousPath.current = pathname;
    setNavigationOpen(false);
    requestAnimationFrame(() => mainRef.current?.focus());
  }, [pathname]);

  return (
    <div className={styles.shell}>
      <a className="skip-link" href="#studio-main">Skip to workspace</a>
      <WorkspaceBootstrap />
      <header className={styles.topbar}>
        <Link to="/" className={styles.brand} aria-label="Rusty Studio Command Center">
          <span className={styles.mark} aria-hidden="true">R</span>
          <span><b>rusty</b><strong>studio</strong></span>
        </Link>
        <button className={styles.menu} type="button" aria-expanded={navigationOpen} aria-controls="studio-navigation" onClick={() => setNavigationOpen((value) => !value)}><span aria-hidden="true">☰</span> Menu</button>
        <button type="button" className={styles.connection} onClick={openDialog} aria-label={connection ? `Switch workspace, currently ${workspaceName}` : "Choose a Rusty workspace"}>
          <span className={connection ? styles.connectionDotLive : styles.connectionDot} aria-hidden="true" />
          <span><b>{workspaceName}</b><small>{workspaceDetail}</small></span>
          <i aria-hidden="true">↗</i>
        </button>
      </header>
      <aside id="studio-navigation" className={styles.sidebar} data-open={navigationOpen}>
        <nav aria-label="Studio lifecycle">
          {lifecycleGroups.map((group) => <section key={group.label} aria-labelledby={`nav-${group.label.toLowerCase()}`}>
            <h2 id={`nav-${group.label.toLowerCase()}`}>{group.label}</h2>
            {group.destinations.map((item) => {
              const active = item.match(pathname);
              return <Link key={item.to} to={item.to} aria-current={active ? "page" : undefined} aria-label={`${item.label} — ${item.description}`}>
                <span className={styles.navGlyph} aria-hidden="true"><i>{item.glyph}</i></span><span><b>{item.label}</b><small>{item.description}</small></span>
              </Link>;
            })}
          </section>)}
        </nav>
        <p className={styles.shellBoundary}><span aria-hidden="true" /> Local-first agent runtime</p>
      </aside>
      <main className={styles.main} id="studio-main" ref={mainRef} tabIndex={-1} aria-label={`${routeName} workspace`}>
        {!connection && workspaceStatus === "discovering"
          ? <div className={styles.workspaceOpening} role="status" aria-live="polite"><span className={styles.openingMark} aria-hidden="true">R</span><div><span className="eyebrow">Rusty Studio</span><h1>Opening your workspace</h1><p>Finding Rusty on this device.</p></div></div>
          : <Suspense fallback={<div className="route-loading" role="status">Opening workspace…</div>}><Outlet /></Suspense>}
      </main>
      <ConnectionDialog />
    </div>
  );
}
