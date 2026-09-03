import { Link, Outlet, useRouterState } from "@tanstack/react-router";
import { Suspense, useEffect, useRef, useState } from "react";
import { RuntimeBootstrap } from "./RuntimeBootstrap";
import { useRuntimeStore } from "../state/runtime";
import { destinationForPath, lifecycleGroups, primaryDestinations } from "./navigation";
import { useI18n } from "../i18n";
import rustyMark from "../assets/rusty-mark.png";
import styles from "./AppShell.module.css";

const NAV_GROUP_KEYS: Record<string, string> = {
  Oversee: "nav.group.oversee",
  Build: "nav.group.build",
  Prove: "nav.group.prove",
  Operate: "nav.group.operate",
};

const NAV_ITEM_KEYS: Record<string, { label: string; description: string }> = {
  "/": { label: "nav.command_center", description: "nav.command_center.description" },
  "/agents": { label: "nav.agent_portfolio", description: "nav.agent_portfolio.description" },
  "/agents/new": { label: "nav.agent_builder", description: "nav.agent_builder.description" },
  "/agents/prompts": { label: "nav.prompt_library", description: "nav.prompt_library.description" },
  "/skills": { label: "nav.skills_tools", description: "nav.skills_tools.description" },
  "/knowledge": { label: "nav.knowledge", description: "nav.knowledge.description" },
  "/connectors": { label: "nav.connectors", description: "nav.connectors.description" },
  "/work": { label: "nav.run_evaluate", description: "nav.run_evaluate.description" },
  "/memory": { label: "nav.memory", description: "nav.memory.description" },
  "/operations": { label: "nav.operations", description: "nav.operations.description" },
};

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
  const { info, status, error, retry } = useRuntimeStore();
  const { t } = useI18n();

  const workspaceName = status === "ready" ? t("workspace.local")
    : status === "starting" ? t("workspace.starting") : t("workspace.unavailable");
  const workspaceDetail = status === "ready" && info
    ? t("workspace.ready.detail", { version: info.version })
    : status === "starting" ? t("workspace.starting.detail") : t("workspace.unavailable.detail");
  const dest = destinationForPath(pathname);
  const routeName = dest ? t(NAV_ITEM_KEYS[dest.to]?.label ?? "route.fallback") : t("route.fallback");

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
  const commandResults = primaryDestinations.filter((item) => {
    const keys = NAV_ITEM_KEYS[item.to];
    const haystack = `${t(keys.label)} ${t(keys.description)}`.toLowerCase();
    return haystack.includes(commandQuery.trim().toLowerCase());
  });
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
      <a className="skip-link" href="#studio-main">{t("skip_link")}</a>
      <RuntimeBootstrap />
      <header className={styles.topbar}>
        <Link to="/" className={styles.mobileBrand} aria-label={t("aria.command_center")}>
          <img className={styles.mark} src={rustyMark} alt="" />
        </Link>
        <div className={styles.context} aria-label={t("aria.current_workspace")}><span>{workspaceName}</span><i aria-hidden="true">/</i><b>{routeName}</b></div>
        <button ref={commandButtonRef} className={styles.command} type="button" onClick={(event) => { commandOpenerRef.current = event.currentTarget; setCommandOpen(true); }}><span>{t("command.placeholder")}</span><kbd>Ctrl/⌘ K</kbd></button>
        <button className={styles.menu} type="button" aria-expanded={navigationOpen} aria-controls="studio-navigation" onClick={() => setNavigationOpen((value) => !value)}><span aria-hidden="true">☰</span> {t("nav.menu")}</button>
        <div className={styles.connection} role="status" aria-live="polite">
          <span className={status === "ready" ? styles.connectionDotLive : styles.connectionDot} aria-hidden="true" />
          <span><b>{workspaceName}</b><small>{workspaceDetail}</small></span>
        </div>
      </header>
      <aside id="studio-navigation" className={styles.sidebar} data-open={navigationOpen}>
        <Link to="/" className={styles.brand} aria-label={t("aria.command_center")}>
          <img className={styles.mark} src={rustyMark} alt="" />
          <span><b>{t("brand.name")}</b><strong>{t("app.tagline")}</strong></span>
        </Link>
        <nav aria-label={t("aria.studio_lifecycle")}>
          {lifecycleGroups.map((group) => {
            const groupKey = NAV_GROUP_KEYS[group.label];
            return <section key={group.label} aria-labelledby={`nav-${group.label.toLowerCase()}`}>
              <h2 id={`nav-${group.label.toLowerCase()}`}>{t(groupKey)}</h2>
              {group.destinations.map((item) => {
                const keys = NAV_ITEM_KEYS[item.to];
                const active = item.match(pathname);
                return <Link key={item.to} to={item.to} activeOptions={{ exact: true }} aria-current={active ? "page" : undefined} aria-label={`${t(keys.label)} — ${t(keys.description)}`}>
                  <span className={styles.navGlyph} aria-hidden="true"><svg viewBox="0 0 24 24"><path d={item.icon} /></svg></span><span><b>{t(keys.label)}</b><small>{t(keys.description)}</small></span>
                </Link>;
              })}
            </section>;
          })}
        </nav>
        <p className={styles.shellBoundary}><span aria-hidden="true" /> {t("shell.boundary")}</p>
        <span className={styles.mechanicalRail} aria-hidden="true"><i /><i /><i /><i /></span>
      </aside>
      <main className={styles.main} id="studio-main" ref={mainRef} tabIndex={-1} aria-label={`${routeName} workspace`}>
        {status === "starting"
          ? <div className={styles.workspaceOpening} role="status" aria-live="polite"><img className={styles.openingMark} src={rustyMark} alt="" /><div><span className="eyebrow">{t("app.name")}</span><h1>{t("error.starting.title")}</h1><p>{t("error.starting.description")}</p></div></div>
          : status === "unavailable"
            ? <div className={styles.workspaceOpening} role="alert"><img className={styles.openingMark} src={rustyMark} alt="" /><div><span className="eyebrow">{t("app.name")}</span><h1>{t("error.unavailable.title")}</h1><p>{error}</p><button type="button" className="primary-button" onClick={retry}>{t("error.unavailable.retry")}</button></div></div>
            : <Suspense fallback={<div className="route-loading" role="status">{t("loading.opening_workspace")}</div>}><Outlet /></Suspense>}
      </main>
      <dialog ref={commandRef} className={styles.commandDialog} aria-labelledby="command-heading" onCancel={(event) => { event.preventDefault(); closeCommand(); }}>
        <header><div><span className="eyebrow">{t("aria.navigate_studio")}</span><h2 id="command-heading">{t("command.heading")}</h2></div><button type="button" onClick={closeCommand} aria-label={t("command.close")}>×</button></header>
        <label><span className="sr-only">{t("command.label")}</span><input value={commandQuery} onChange={(event) => setCommandQuery(event.target.value)} placeholder={t("command.input_placeholder")} /></label>
        <nav aria-label={t("aria.matching_workspaces")}>{commandResults.map((item) => {
          const keys = NAV_ITEM_KEYS[item.to];
          return <Link key={item.to} to={item.to} onClick={() => { commandOpenerRef.current = commandButtonRef.current; closeCommand(); }}><span className={styles.navGlyph} aria-hidden="true"><svg viewBox="0 0 24 24"><path d={item.icon} /></svg></span><span><b>{t(keys.label)}</b><small>{t(keys.description)}</small></span></Link>;
        })}{!commandResults.length && <p>{t("command.no_results")}</p>}</nav>
      </dialog>
    </div>
  );
}
