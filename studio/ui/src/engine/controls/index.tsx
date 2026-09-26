import type { ButtonHTMLAttributes, ReactNode, SelectHTMLAttributes, InputHTMLAttributes, TextareaHTMLAttributes } from "react";
import "./controls.css";

export type Tone = "ok" | "warn" | "err" | "ink" | "focus" | "ink3";

type ButtonVariant = "primary" | "secondary" | "ghost" | "warn" | "danger";
export function Button({ variant = "secondary", size, className = "", ...rest }:
  { variant?: ButtonVariant; size?: "sm" } & ButtonHTMLAttributes<HTMLButtonElement>) {
  const cls = ["rn-btn", `rn-btn--${variant}`, size === "sm" ? "rn-btn--sm" : "", className].filter(Boolean).join(" ");
  return <button type="button" className={cls} {...rest} />;
}

export function Pill({ selected, code, as, className = "", children, ...rest }:
  { selected?: boolean; code?: boolean; as?: "button" | "span" } & ButtonHTMLAttributes<HTMLButtonElement>) {
  const cls = ["rn-pill", as === "span" ? "rn-pill--static" : "", code ? "rn-pill--code" : "", className].filter(Boolean).join(" ");
  if (as === "span") return <span className={cls}>{children}</span>;
  return <button type="button" className={cls} aria-pressed={selected} {...rest}>{children}</button>;
}

/** Badge — effect boundary, skill/gap state, or kind. Tone carries meaning. */
export function Badge({ tone = "ink", children }: { tone?: Extract<Tone, "ok" | "warn" | "err" | "ink">; children: ReactNode }) {
  return <span className={`rn-badge rn-badge--${tone}`}>{children}</span>;
}

/** A status dot beside a text label — the design's status idiom. */
export function StatusDot({ tone }: { tone: Tone }) {
  return <span className={`rn-dot rn-dot--${tone}`} aria-hidden="true" />;
}

export function SectionLabel({ children }: { children: ReactNode }) {
  return <div className="rn-section-label">{children}</div>;
}

export function Field({ label, hint, htmlFor, children }: { label?: ReactNode; hint?: ReactNode; htmlFor?: string; children: ReactNode }) {
  return (
    <div className="rn-field">
      {label && <label htmlFor={htmlFor}>{label}</label>}
      {children}
      {hint && <span className="rn-hint">{hint}</span>}
    </div>
  );
}

export function Input({ mono, className = "", ...rest }: { mono?: boolean } & InputHTMLAttributes<HTMLInputElement>) {
  return <input className={["rn-input", mono ? "rn-input--mono" : "", className].filter(Boolean).join(" ")} {...rest} />;
}

export function Textarea({ mono, className = "", ...rest }: { mono?: boolean } & TextareaHTMLAttributes<HTMLTextAreaElement>) {
  return <textarea className={["rn-textarea", mono ? "rn-textarea--mono" : "", className].filter(Boolean).join(" ")} {...rest} />;
}

export function Select({ className = "", children, ...rest }: SelectHTMLAttributes<HTMLSelectElement>) {
  return <select className={["rn-select", className].filter(Boolean).join(" ")} {...rest}>{children}</select>;
}

export function Mono({ children }: { children: ReactNode }) {
  return <code className="rn-mono">{children}</code>;
}

// ── The screen vocabulary (docs/design/studio-ux-rebuild-2026-09-13.md) ──
// One line per fact, a chip for its state, the screen leads. Every screen
// composes these; none hand-rolls them again.

/** eyebrow · h1 · one-line sub · actions at the right. */
export function PageHeader({ eyebrow, title, sub, actions, chip }: { eyebrow?: ReactNode; title: ReactNode; sub?: ReactNode; actions?: ReactNode; chip?: ReactNode }) {
  return (
    <header className="rn-page-header">
      <div className="rn-page-header__text">
        {eyebrow && <div className="rn-eyebrow">{eyebrow}</div>}
        <div className="rn-page-header__title-row"><h1 className="rn-page-title">{title}</h1>{chip}</div>
        {sub && <p className="rn-page-sub">{sub}</p>}
      </div>
      {actions && <div className="rn-page-header__actions">{actions}</div>}
    </header>
  );
}

export interface Step { id: string; name: string; meta?: string }

/** The steps across the top: done, current, ahead. A done step is a way back. */
export function Stepper({ steps, current, onGo }: { steps: Step[]; current: string; onGo?: (id: string) => void }) {
  const at = Math.max(0, steps.findIndex((s) => s.id === current));
  return (
    <ol className="rn-stepper" aria-label="Steps">
      {steps.map((s, i) => {
        const state = i < at ? "done" : i === at ? "current" : "ahead";
        const clickable = state === "done" && !!onGo;
        return (
          <li key={s.id} className={`rn-step rn-step--${state}`} data-step={s.id} data-state={state} aria-current={state === "current" ? "step" : undefined}>
            <button type="button" className="rn-step__btn" disabled={!clickable} onClick={() => clickable && onGo?.(s.id)}>
              <span className="rn-step__dot">{state === "done" ? "✓" : i + 1}</span>
              <span className="rn-step__text"><span className="rn-step__name">{s.name}</span>{s.meta && <span className="rn-step__meta">{s.meta}</span>}</span>
            </button>
            {i < steps.length - 1 && <span className="rn-step__line" aria-hidden="true" />}
          </li>
        );
      })}
    </ol>
  );
}

/** A small state word with a dot, on a soft ground: live · draft · needs you. */
export function Chip({ tone = "ink", children, dot = true }: { tone?: "ok" | "warn" | "err" | "ink" | "accent"; children: ReactNode; dot?: boolean }) {
  return <span className={`rn-chip rn-chip--${tone}`}>{dot && <span className="rn-chip__dot" aria-hidden="true" />}{children}</span>;
}

/** title · state chip · up to a few one-line facts · an action at the right. */
export function FactCard({ title, chip, facts, action, children, empty, icon, ...rest }: { title: ReactNode; chip?: ReactNode; facts?: ReactNode[]; action?: ReactNode; children?: ReactNode; empty?: ReactNode; icon?: ReactNode } & Record<`data-${string}`, string | undefined>) {
  const shown = (facts ?? []).filter((f) => f !== null && f !== undefined && f !== false && f !== "");
  return (
    <section className="rn-fact" {...rest}>
      <div className="rn-fact__head">
        {icon && <span className="rn-fact__icon" aria-hidden="true">{icon}</span>}
        <span className="rn-fact__title">{title}</span>
        {chip}
        <span style={{ flex: 1 }} />
        {action}
      </div>
      {children !== undefined && children !== null && children !== false
        ? children
        : shown.length ? <ul className="rn-fact__facts">{shown.map((f, i) => <li key={i}>{f}</li>)}</ul> : empty ? <div className="rn-fact__empty">{empty}</div> : null}
    </section>
  );
}

/** One big number, its label, an optional bar. */
export function StatCard({ value, label, bar, tone = "ink", hint }: { value: ReactNode; label: ReactNode; bar?: number; tone?: "ok" | "warn" | "err" | "ink" | "accent"; hint?: ReactNode }) {
  return (
    <div className={`rn-stat rn-stat--${tone}`}>
      <div className="rn-stat__value">{value}</div>
      <div className="rn-stat__label">{label}</div>
      {bar !== undefined && <div className="rn-bar"><div className="rn-bar__fill" style={{ width: `${Math.max(0, Math.min(100, bar))}%` }} /></div>}
      {hint && <div className="rn-stat__hint">{hint}</div>}
    </div>
  );
}

/** Initials in a tinted tile — an agent, a person, a system. */
export function Avatar({ name, size = 36, tint }: { name: string; size?: number; tint?: number }) {
  const initials = name.split(/[\s_-]+/).filter(Boolean).slice(0, 2).map((w) => w[0]?.toUpperCase() ?? "").join("") || "?";
  const hue = tint ?? [...name].reduce((h, c) => (h * 31 + c.charCodeAt(0)) % 360, 7);
  return (
    <span className="rn-avatar" style={{ width: size, height: size, fontSize: Math.round(size * 0.36), ["--avatar-hue" as string]: hue }} aria-hidden="true">{initials}</span>
  );
}

/** icon tile · name · one line · selected — a radio in card form. */
export function ChoiceCard({ icon, name, desc, selected, onSelect, meta, ...rest }: { icon?: ReactNode; name: ReactNode; desc?: ReactNode; selected?: boolean; onSelect?: () => void; meta?: ReactNode } & Record<`data-${string}`, string | undefined>) {
  return (
    <button type="button" className={`rn-choice${selected ? " rn-choice--on" : ""}`} aria-pressed={selected} onClick={onSelect} {...rest}>
      {icon && <span className="rn-choice__icon" aria-hidden="true">{icon}</span>}
      <span className="rn-choice__body"><span className="rn-choice__name">{name}</span>{desc && <span className="rn-choice__desc">{desc}</span>}</span>
      {meta && <span className="rn-choice__meta">{meta}</span>}
    </button>
  );
}

/** icon tile · name · one line · switch — a pick-list row. */
export function SwitchRow({ icon, name, desc, on, onToggle, meta, disabled, ...rest }: { icon?: ReactNode; name: ReactNode; desc?: ReactNode; on: boolean; onToggle: () => void; meta?: ReactNode; disabled?: boolean } & Record<`data-${string}`, string | undefined>) {
  return (
    <div className={`rn-switch-row${on ? " rn-switch-row--on" : ""}`} {...rest}>
      {icon && <span className="rn-switch-row__icon" aria-hidden="true">{icon}</span>}
      <span className="rn-switch-row__body"><span className="rn-switch-row__name">{name}</span>{desc && <span className="rn-switch-row__desc">{desc}</span>}{meta && <span className="rn-switch-row__meta">{meta}</span>}</span>
      <button type="button" role="switch" aria-checked={on} aria-label={typeof name === "string" ? name : undefined} className={`rn-switch${on ? " rn-switch--on" : ""}`} onClick={onToggle} disabled={disabled} />
    </div>
  );
}

/** One icon, one sentence, one action. */
export function EmptyState({ icon, title, body, action }: { icon?: ReactNode; title: ReactNode; body?: ReactNode; action?: ReactNode }) {
  return (
    <div className="rn-empty">
      {icon && <span className="rn-empty__icon" aria-hidden="true">{icon}</span>}
      <div className="rn-empty__title">{title}</div>
      {body && <div className="rn-empty__body">{body}</div>}
      {action && <div className="rn-empty__action">{action}</div>}
    </div>
  );
}

/** A score ring with a bar per dimension — what is answered, what is not. */
export function Readiness({ score, rows, label = "Readiness" }: { score: number; rows: { label: ReactNode; done: boolean; onFix?: () => void }[]; label?: ReactNode }) {
  const pct = Math.max(0, Math.min(100, Math.round(score)));
  return (
    <div className="rn-readiness" data-score={pct}>
      <div className="rn-readiness__top">
        <div className="rn-readiness__ring" style={{ ["--pct" as string]: pct }}><span>{pct}</span></div>
        <div><div className="rn-readiness__label">{label}</div><div className="rn-readiness__sub">{rows.filter((r) => !r.done).length === 0 ? "Everything answered" : `${rows.filter((r) => !r.done).length} still to answer`}</div></div>
      </div>
      <ul className="rn-readiness__rows">
        {rows.map((r, i) => (
          <li key={i} className={r.done ? "is-done" : "is-open"}>
            <span className="rn-readiness__mark" aria-hidden="true">{r.done ? "✓" : "·"}</span>
            <span className="rn-readiness__text">{r.label}</span>
            {!r.done && r.onFix && <button type="button" className="rn-link" onClick={r.onFix}>Fix</button>}
          </li>
        ))}
      </ul>
    </div>
  );
}
