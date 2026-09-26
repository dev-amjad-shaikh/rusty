import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState, type ReactNode } from "react";

/**
 * The overlay engine the prototype's flows.js provides: one host for a
 * drawer or a modal at a time (`.m-overlay.show[.drawer]`), a toast host,
 * Esc and scrim-click to close. Flows render their own `.m-drawer` /
 * `.m-modal` markup into it.
 */
interface OverlayApi {
  open: (kind: "drawer" | "modal", node: ReactNode) => void;
  close: () => void;
  toast: (message: string, icon?: string) => void;
  /** The full-screen create wizard (`.wz-overlay#wizard`), above the views, under the overlay. */
  openWizard: (node: ReactNode) => void;
  closeWizard: () => void;
}

const Ctx = createContext<OverlayApi | null>(null);

export function useOverlay(): OverlayApi {
  const api = useContext(Ctx);
  if (!api) throw new Error("useOverlay outside <OverlayHost>");
  return api;
}

interface Toast { id: number; message: string; icon: string; out: boolean }

export function OverlayHost({ children }: { children: ReactNode }) {
  const [panel, setPanel] = useState<{ kind: "drawer" | "modal"; node: ReactNode } | null>(null);
  const [toasts, setToasts] = useState<Toast[]>([]);
  const [wizard, setWizard] = useState<ReactNode | null>(null);
  const openWizard = useCallback((node: ReactNode) => setWizard(node), []);
  const closeWizard = useCallback(() => setWizard(null), []);
  const seq = useRef(0);
  const close = useCallback(() => setPanel(null), []);
  const open = useCallback((kind: "drawer" | "modal", node: ReactNode) => setPanel({ kind, node }), []);
  const toast = useCallback((message: string, icon = "ti-check") => {
    const id = ++seq.current;
    setToasts((t) => [...t, { id, message, icon, out: false }]);
    setTimeout(() => setToasts((t) => t.map((x) => (x.id === id ? { ...x, out: true } : x))), 2600);
    setTimeout(() => setToasts((t) => t.filter((x) => x.id !== id)), 2880);
  }, []);
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => { if (e.key === "Escape" && panel) close(); };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [panel, close]);
  const api = useMemo(() => ({ open, close, toast, openWizard, closeWizard }), [open, close, toast, openWizard, closeWizard]);
  return (
    <Ctx.Provider value={api}>
      {children}
      <div className={`wz-overlay${wizard ? " show" : ""}`} id="wizard">{wizard}</div>
      <div className={`m-overlay${panel ? " show" : ""}${panel?.kind === "drawer" ? " drawer" : ""}`} id="overlay" onMouseDown={(e) => { if (e.target === e.currentTarget) close(); }} onClick={(e) => { if ((e.target as HTMLElement).closest("[data-close]")) close(); }}>
        {panel?.node}
      </div>
      <div className="m-toast-host" id="toastHost">
        {toasts.map((t) => <div key={t.id} className={`m-toast${t.out ? " out" : ""}`}><span className="t-ic"><i className={`ti ${t.icon}`} /></span><span>{t.message}</span></div>)}
      </div>
    </Ctx.Provider>
  );
}

/** The overlay header every drawer and modal shares. */
export function OvHead({ icon, bg, fg, title, sub, logo }: { icon: string; bg?: string; fg?: string; title: ReactNode; sub?: ReactNode; logo?: boolean }) {
  return (
    <div className="ov-head">
      <div className={`oh-ic${logo ? " logo" : ""}`} style={bg || fg ? { background: bg, color: fg } : undefined}><i className={`ti ${icon}`} /></div>
      <div className="oh-titles"><div className="ov-title">{title}</div>{sub && <div className="ov-sub">{sub}</div>}</div>
      <div className="ov-close" data-close><i className="ti ti-x" /></div>
    </div>
  );
}

/** A `.m-badge`: a 7px dot and a word. */
export function Badge({ tone, children, sm = true }: { tone?: "good" | "warn" | "bad" | "info" | "accent" | "solid"; children: ReactNode; sm?: boolean }) {
  return <span className={`m-badge${tone ? ` ${tone}` : ""}${sm ? " sm" : ""}`}><span className="dot" />{children}</span>;
}

/** A `.m-switch`. */
export function Switch({ on, onChange, title }: { on: boolean; onChange?: (on: boolean) => void; title?: string }) {
  return <div className={`m-switch${on ? " on" : ""}`} data-switch role="switch" aria-checked={on} title={title} onClick={(e) => { e.stopPropagation(); onChange?.(!on); }} />;
}

/** The handoff's pop menu: anchored under a button, one click picks an item, any other click closes it. */
export function openMenu(anchor: HTMLElement, items: ({ label: string; icon: string; danger?: boolean; run: () => void } | "-")[]) {
  document.querySelector(".pop-menu")?.remove();
  const pop = document.createElement("div");
  pop.className = "m-menu pop-menu";
  pop.innerHTML = items.map((i) => (i === "-" ? '<div class="m-menu-sep"></div>' : `<div class="m-menu-item${i.danger ? " danger" : ""}" data-mi="${i.label}"><i class="ti ${i.icon} ic"></i>${i.label}</div>`)).join("");
  document.body.appendChild(pop);
  const r = anchor.getBoundingClientRect();
  pop.style.top = `${r.bottom + 6}px`;
  pop.style.left = `${Math.min(r.right - pop.offsetWidth, window.innerWidth - pop.offsetWidth - 12)}px`;
  const close = () => { pop.remove(); document.removeEventListener("mousedown", away); };
  const away = (e: MouseEvent) => { if (!pop.contains(e.target as Node)) close(); };
  setTimeout(() => document.addEventListener("mousedown", away), 0);
  pop.addEventListener("click", (e) => { const mi = (e.target as HTMLElement).closest("[data-mi]") as HTMLElement | null; if (!mi) return; const item = items.find((i) => i !== "-" && i.label === mi.dataset.mi); close(); if (item && item !== "-") item.run(); });
}
