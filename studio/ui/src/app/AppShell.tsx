import { Link, Outlet, useRouterState } from "@tanstack/react-router";
import { Suspense, useEffect, useRef, useState } from "react";
import { ConnectionDialog } from "../features/connection/ConnectionDialog";
import { WorkspaceBootstrap, workspaceDisplayName } from "../features/connection/WorkspaceBootstrap";
import { useConnectionStore } from "../state/connection";
import { destinationForPath, lifecycleGroups, primaryDestinations } from "./navigation";
import rustyMark from "../assets/rusty-mark.png";
import styles from "./AppShell.module.css";

export function AppShell() {
  const pathname = useRouterState({ select: (state) => state.location.pathname });
  const mainRef = useRef<HTMLElement>(null);
  const commandRef = useRef<HTMLDialogElement>(null);
  const commandButtonRef = useRef<HTMLButtonElement>(null);
  const commandOpenerRef = useRef<HTMLElement | null>(null);
  const previousPath = useRef(pathname);
  const [navigationOpen, setNavigationOpen] = useState(false);
  const [commandOpen, setCommandOpen] = useState(false);
  const [commandQuery, setCommandQuery] = useState("");
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
  useEffect(() => {
    const openCommand = (event: KeyboardEvent) => {
      if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") {
        event.preventDefault();
        commandOpenerRef.current = document.activeElement instanceof HTMLElement ? document.activeElement : commandButtonRef.current;
        setCommandOpen(true);
      }
    };
    window.addEventListener("keydown", openCommand);
    return () => window.removeEventListener("keydown", openCommand);
  }, []);
  useEffect(() => {
    const dialog = commandRef.current;
    if (!dialog) return;
    if (commandOpen && !dialog.open) {
      if (typeof dialog.showModal === "function") dialog.showModal();
      else dialog.setAttribute("open", "");
      requestAnimationFrame(() => dialog.querySelector("input")?.focus());
    } else if (!commandOpen && dialog.open) {
      if (typeof dialog.close === "function") dialog.close();
      else dialog.removeAttribute("open");
      const opener = commandOpenerRef.current;
      commandOpenerRef.current = null;
      requestAnimationFrame(() => opener?.isConnected && opener.focus());
    }
  }, [commandOpen]);
  const commandResults = primaryDestinations.filter((item) => `${item.label} ${item.description}`.toLowerCase().includes(commandQuery.trim().toLowerCase()));
  const closeCommand = () => {
    const dialog = commandRef.current;
    if (dialog?.open) {
      if (typeof dialog.close === "function") dialog.close();
      else dialog.removeAttribute("open");
    }
    const opener = commandOpenerRef.current;
    commandOpenerRef.current = null;
    setCommandOpen(false);
    setCommandQuery("");
    requestAnimationFrame(() => opener?.isConnected && opener.focus({ preventScroll: true }));
  };

  return (
    <div className={styles.shell}>
      <a className="skip-link" href="#studio-main">Skip to workspace</a>
      <WorkspaceBootstrap />
      <header className={styles.topbar}>
        <Link to="/" className={styles.mobileBrand} aria-label="Rusty Agent Platform Command Center">
          <img className={styles.mark} src={rustyMark} alt="" />
        </Link>
        <div className={styles.context} aria-label="Current workspace location"><span>{workspaceName}</span><i aria-hidden="true">/</i><b>{routeName}</b></div>
        <button ref={commandButtonRef} className={styles.command} type="button" onClick={(event) => { commandOpenerRef.current = event.currentTarget; setCommandOpen(true); }}><span>Go to agents, work, prompts, or operations…</span><kbd>Ctrl/⌘ K</kbd></button>
        <button className={styles.menu} type="button" aria-expanded={navigationOpen} aria-controls="studio-navigation" onClick={() => setNavigationOpen((value) => !value)}><span aria-hidden="true">☰</span> Menu</button>
        <button type="button" className={styles.connection} onClick={openDialog} aria-label={connection ? `Switch workspace, currently ${workspaceName}` : "Choose a Rusty workspace"}>
          <span className={connection ? styles.connectionDotLive : styles.connectionDot} aria-hidden="true" />
          <span><b>{workspaceName}</b><small>{workspaceDetail}</small></span>
          <i aria-hidden="true">↗</i>
        </button>
      </header>
      <aside id="studio-navigation" className={styles.sidebar} data-open={navigationOpen}>
        <Link to="/" className={styles.brand} aria-label="Rusty Agent Platform Command Center">
          <img className={styles.mark} src={rustyMark} alt="" />
          <span><b>Rusty</b><strong>Agent platform</strong></span>
        </Link>
        <nav aria-label="Studio lifecycle">
          {lifecycleGroups.map((group) => <section key={group.label} aria-labelledby={`nav-${group.label.toLowerCase()}`}>
            <h2 id={`nav-${group.label.toLowerCase()}`}>{group.label}</h2>
            {group.destinations.map((item) => {
              const active = item.match(pathname);
              return <Link key={item.to} to={item.to} activeOptions={{ exact: true }} aria-current={active ? "page" : undefined} aria-label={`${item.label} — ${item.description}`}>
                <span className={styles.navGlyph} aria-hidden="true"><svg viewBox="0 0 24 24"><path d={item.icon} /></svg></span><span><b>{item.label}</b><small>{item.description}</small></span>
              </Link>;
            })}
          </section>)}
        </nav>
        <p className={styles.shellBoundary}><span aria-hidden="true" /> Local-first agent runtime</p>
        <span className={styles.mechanicalRail} aria-hidden="true"><i /><i /><i /><i /></span>
      </aside>
      <main className={styles.main} id="studio-main" ref={mainRef} tabIndex={-1} aria-label={`${routeName} workspace`}>
        {!connection && workspaceStatus === "discovering"
          ? <div className={styles.workspaceOpening} role="status" aria-live="polite"><img className={styles.openingMark} src={rustyMark} alt="" /><div><span className="eyebrow">Rusty Studio</span><h1>Opening your workspace</h1><p>Finding Rusty on this device.</p></div></div>
          : <Suspense fallback={<div className="route-loading" role="status">Opening workspace…</div>}><Outlet /></Suspense>}
      </main>
      <dialog ref={commandRef} className={styles.commandDialog} aria-labelledby="command-heading" onCancel={(event) => { event.preventDefault(); closeCommand(); }}>
        <header><div><span className="eyebrow">Navigate Rusty</span><h2 id="command-heading">Where do you want to go?</h2></div><button type="button" onClick={closeCommand} aria-label="Close navigation">×</button></header>
        <label><span className="sr-only">Find a Studio workspace</span><input value={commandQuery} onChange={(event) => setCommandQuery(event.target.value)} placeholder="Agents, work, prompts, operations…" /></label>
        <nav aria-label="Matching Studio workspaces">{commandResults.map((item) => <Link key={item.to} to={item.to} onClick={() => { commandOpenerRef.current = commandButtonRef.current; closeCommand(); }}><span className={styles.navGlyph} aria-hidden="true"><svg viewBox="0 0 24 24"><path d={item.icon} /></svg></span><span><b>{item.label}</b><small>{item.description}</small></span></Link>)}{!commandResults.length && <p>No matching workspace.</p>}</nav>
      </dialog>
      <ConnectionDialog />
    </div>
  );
}
