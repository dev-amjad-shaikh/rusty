import { useEffect } from "react";
import { Link, Outlet, useNavigate, useRouterState } from "@tanstack/react-router";
import { OverlayHost, useOverlay } from "./overlay";
import { useServer } from "../engine/net/server";
import { useEngine } from "../engine/state";
import { NotificationsDrawer, SettingsModal } from "./flows/shellFlows";
import "./knot.css";

/** The icon strip's destinations, in the prototype's order. */
export const VIEWS: { view: string; to: string; icon: string; title: string }[] = [
  { view: "home", to: "/home", icon: "ti-layout-dashboard", title: "Home" },
  { view: "agents", to: "/agents", icon: "ti-robot", title: "Agents" },
  { view: "skills", to: "/skills", icon: "ti-puzzle", title: "Skills" },
  { view: "tools", to: "/tools", icon: "ti-tool", title: "Tools" },
  { view: "connectors", to: "/connectors", icon: "ti-plug-connected", title: "Connectors" },
  { view: "knowledge", to: "/knowledge", icon: "ti-database", title: "Knowledge" },
  { view: "evals", to: "/evals", icon: "ti-test-pipe", title: "Tests" },
  { view: "models", to: "/models", icon: "ti-cpu", title: "AI models" },
  { view: "analytics", to: "/analytics", icon: "ti-chart-dots-3", title: "Activity" },
];

/** The shell: the 56px icon strip and the active view. */
export function Knot() {
  return (
    <OverlayHost>
      <Shell />
    </OverlayHost>
  );
}

function Shell() {
  const pathname = useRouterState({ select: (s) => s.location.pathname });
  const navigate = useNavigate();
  const { open } = useOverlay();
  const reach = useServer((s) => s.reach);
  const refused = useServer((s) => s.refused);
  const theme = useEngine((s) => s.theme);
  useEffect(() => { void useServer.getState().refresh(); }, []);
  // Keep the shared lists current. This polled only while the server was
  // *down* — a reconnect loop — so once it was up the agents, runs and skills
  // never refreshed again except on the handful of actions that ask: an
  // agent archived elsewhere stayed in the rail, and a label read Draft
  // until a reload. Down: every ten seconds, to notice it come back. Up:
  // every thirty, and at once when the tab regains focus. Not gated on the
  // tab being visible: a hidden tab that comes back showing an hour-old
  // list is the same complaint again.
  useEffect(() => {
    const every = reach === "down" ? 10_000 : 30_000;
    const timer = setInterval(() => { void useServer.getState().refresh(); }, every);
    const onFocus = () => { void useServer.getState().refresh(); };
    window.addEventListener("focus", onFocus);
    return () => { clearInterval(timer); window.removeEventListener("focus", onFocus); };
  }, [reach]);
  useEffect(() => { if (refused && pathname !== "/login") navigate({ to: "/login" }); }, [refused, pathname, navigate]);
  useEffect(() => { document.documentElement.setAttribute("data-theme", theme); }, [theme]);
  const active = VIEWS.find((v) => pathname === v.to || pathname.startsWith(`${v.to}/`))?.view ?? (pathname === "/" ? "agents" : "");
  return (
    <div className="shell" data-screen-label="Agent Builder">
      <nav className="strip">
        <div className="strip-mark"><svg viewBox="0 0 24 24" fill="none"><path d="M4 7l8-4 8 4-8 4-8-4z" fill="currentColor" opacity=".9" /><path d="M4 12l8 4 8-4M4 17l8 4 8-4" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round" /></svg></div>
        {VIEWS.map((v) => (
          <Link key={v.view} to={v.to} className={`strip-btn${active === v.view ? " active" : ""}`} data-view={v.view} title={v.title}><i className={`ti ${v.icon}`} /></Link>
        ))}
        <div className="strip-sp" />
        <NotificationsButton onOpen={() => open("drawer", <NotificationsDrawer />)} />
        <div className="strip-btn" data-flow="settings" title="Settings" onClick={() => open("modal", <SettingsModal />)}><i className="ti ti-settings" /></div>
      </nav>
      <Outlet />
    </div>
  );
}

function NotificationsButton({ onOpen }: { onOpen: () => void }) {
  // A dot while a run waits on a decision.
  const unread = useServer((s) => s.runs.filter((r) => r.status === "interrupted" && !r.decision).length);
  return (
    <div className="strip-btn" data-flow="notifications" title="Notifications" onClick={onOpen}>
      <i className="ti ti-bell" />{unread > 0 && <span className="strip-dot" />}
    </div>
  );
}
