import { Link, Outlet } from "@tanstack/react-router";
import { useQueryClient } from "@tanstack/react-query";
import { ConnectionDialog } from "../features/connection/ConnectionDialog";
import { useConnectionStore } from "../state/connection";
import { primaryDestinations as destinations } from "./navigation";
import styles from "./AppShell.module.css";

export function AppShell() {
  const queryClient = useQueryClient();
  const { connection, openDialog, disconnect } = useConnectionStore();

  function leave() {
    queryClient.clear();
    disconnect();
  }

  return (
    <div className={styles.shell}>
      <header className={styles.topbar}>
        <Link to="/work" className={styles.brand} aria-label="Rusty Studio home">
          <span className={styles.mark} aria-hidden="true">R</span>
          <span>Rusty <strong>Studio</strong></span>
        </Link>
        <div className={styles.connection}>
          <span className={connection ? styles.connectionDotLive : styles.connectionDot} aria-hidden="true" />
          <span><b>{connection ? "Connected" : "No server selected"}</b>{!connection && <small>Connect to begin</small>}</span>
          <button type="button" onClick={connection ? leave : openDialog}>{connection ? "Disconnect" : "Connect"}</button>
        </div>
      </header>
      <div className={styles.frame}>
        <aside className={styles.sidebar}>
          <nav aria-label="Primary navigation" className={styles.navigation}>
            {destinations.map((item) => (
              <Link key={item.to} to={item.to} activeProps={{ "aria-current": "page" }}>
                <b>{item.label}</b><span>{item.description}</span>
              </Link>
            ))}
          </nav>
        </aside>
        <main className={styles.main}>
          <Outlet />
        </main>
      </div>
      <ConnectionDialog />
    </div>
  );
}
