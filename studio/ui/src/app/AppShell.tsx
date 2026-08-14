import { Link, Outlet, useRouterState } from "@tanstack/react-router";
import { Suspense, useEffect, useRef } from "react";
import { ConnectionDialog } from "../features/connection/ConnectionDialog";
import { WorkspaceBootstrap, workspaceDisplayName } from "../features/connection/WorkspaceBootstrap";
import { useConnectionStore } from "../state/connection";
import { primaryDestinations as destinations } from "./navigation";
import styles from "./AppShell.module.css";

export function AppShell() {
  const pathname = useRouterState({ select: (state) => state.location.pathname });
  const mainRef = useRef<HTMLElement>(null);
  const previousPath = useRef(pathname);
  const { connection, info, workspaceStatus, openDialog } = useConnectionStore();
  const workspaceName = connection ? workspaceDisplayName(connection.origin)
    : workspaceStatus === "discovering" ? "Opening workspace" : "Workspace unavailable";
  const workspaceDetail = connection && info ? `Rusty ${info.version}`
    : workspaceStatus === "discovering" ? "Finding Rusty on this device" : "Choose or retry a workspace";
  const routeName = destinations.find((item) => pathname.startsWith(item.to))?.label ?? "Studio";
  useEffect(() => {
    if (previousPath.current === pathname) return;
    previousPath.current = pathname;
    requestAnimationFrame(() => mainRef.current?.focus());
  }, [pathname]);

  return (
    <div className={styles.shell}>
      <a className="skip-link" href="#studio-main">Skip to workspace</a>
      <WorkspaceBootstrap />
      <header className={styles.topbar}>
        <Link to="/work" className={styles.brand} aria-label="Rusty Studio home">
          <span className={styles.mark} aria-hidden="true"><i /><i /><i /></span>
          <span><b>rusty</b><strong>studio</strong></span>
        </Link>
        <button type="button" className={styles.connection} onClick={openDialog} aria-label={connection ? `Switch workspace, currently ${workspaceName}` : "Choose a Rusty workspace"}>
          <span className={connection ? styles.connectionDotLive : styles.connectionDot} aria-hidden="true" />
          <span><b>{workspaceName}</b><small>{workspaceDetail}</small></span>
          <i aria-hidden="true">↗</i>
        </button>
        <nav aria-label="Primary navigation" className={styles.navigation}>
          {destinations.map((item) => (
            <Link key={item.to} to={item.to} activeProps={{ "aria-current": "page" }} aria-label={`${item.label} — ${item.description}`}>
              <b>{item.label}</b><span>{item.description}</span>
            </Link>
          ))}
        </nav>
      </header>
      <main className={styles.main} id="studio-main" ref={mainRef} tabIndex={-1} aria-label={`${routeName} workspace`}>
        {!connection && workspaceStatus === "discovering"
          ? <div className={styles.workspaceOpening} role="status" aria-live="polite"><span className={styles.openingMark} aria-hidden="true">R</span><div><span className="eyebrow">Rusty Studio</span><h1>Opening your workspace</h1><p>Finding Rusty on this device.</p></div></div>
          : <Suspense fallback={<div className="route-loading" role="status">Opening workspace…</div>}><Outlet /></Suspense>}
      </main>
      <ConnectionDialog />
    </div>
  );
}
