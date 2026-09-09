// Base controls (handoff 01): buttons, inputs, select, badges, pills,
// composer, toast. Styling lives in controls.module.css over tokens.css.

import { useEffect, type KeyboardEvent, type ReactNode } from "react";
import styles from "./controls.module.css";

export type Tone = "ok" | "warn" | "err" | "neutral";

const TONE_CLASS: Record<Tone, string> = {
  ok: styles.toneOk,
  warn: styles.toneWarn,
  err: styles.toneErr,
  neutral: styles.toneNeutral,
};

export function Button(props: {
  variant?: "primary" | "secondary" | "ghost";
  small?: boolean;
  disabled?: boolean;
  onClick?: () => void;
  type?: "button" | "submit";
  ariaLabel?: string;
  children: ReactNode;
}) {
  const variant = props.variant ?? "primary";
  const className = [
    styles.button,
    styles[variant],
    props.small ? styles.small : "",
  ].filter(Boolean).join(" ");
  return (
    <button
      type={props.type ?? "button"}
      className={className}
      disabled={props.disabled}
      onClick={props.onClick}
      aria-label={props.ariaLabel}
    >
      {props.children}
    </button>
  );
}

export function TextInput(props: {
  value: string;
  onChange?: (value: string) => void;
  placeholder?: string;
  ariaLabel?: string;
  mono?: boolean;
}) {
  return (
    <input
      className={styles.input}
      style={props.mono ? { fontFamily: "var(--font-mono)" } : undefined}
      value={props.value}
      placeholder={props.placeholder}
      aria-label={props.ariaLabel}
      onChange={(event) => props.onChange?.(event.target.value)}
    />
  );
}

export function TextArea(props: {
  value: string;
  onChange?: (value: string) => void;
  placeholder?: string;
  ariaLabel?: string;
  rows?: number;
}) {
  return (
    <textarea
      className={styles.input}
      rows={props.rows ?? 3}
      value={props.value}
      placeholder={props.placeholder}
      aria-label={props.ariaLabel}
      onChange={(event) => props.onChange?.(event.target.value)}
    />
  );
}

export function Select(props: {
  value: string;
  onChange?: (value: string) => void;
  ariaLabel?: string;
  options: { value: string; label: string }[];
}) {
  return (
    <span className={styles.selectWrap}>
      <select
        className={styles.select}
        value={props.value}
        aria-label={props.ariaLabel}
        onChange={(event) => props.onChange?.(event.target.value)}
      >
        {props.options.map((option) => (
          <option key={option.value} value={option.value}>{option.label}</option>
        ))}
      </select>
      <svg className={styles.chevron} width="10" height="6" viewBox="0 0 10 6" aria-hidden="true">
        <path d="M1 1l4 4 4-4" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" />
      </svg>
    </span>
  );
}

export function Badge(props: { tone?: Tone; children: ReactNode }) {
  return <span className={`${styles.badge} ${TONE_CLASS[props.tone ?? "neutral"]}`}>{props.children}</span>;
}

export function Pill(props: {
  selected?: boolean;
  onClick?: () => void;
  children: ReactNode;
}) {
  return (
    <button
      type="button"
      className={`${styles.pill} ${props.selected ? styles.pillSelected : ""}`}
      aria-pressed={props.selected ?? false}
      onClick={props.onClick}
    >
      {props.children}
    </button>
  );
}

export function StatusDot(props: { tone: Tone; label: string }) {
  return (
    <span role="status">
      <span className={`${styles.statusDot} ${TONE_CLASS[props.tone]}`} aria-hidden="true" />
      {" "}
      <span className={styles.statusLabel}>{props.label}</span>
    </span>
  );
}

export function Composer(props: {
  value: string;
  onChange: (value: string) => void;
  onSend: () => void;
  placeholder?: string;
  sendLabel: string;
  disabled?: boolean;
  ariaLabel?: string;
}) {
  const send = () => {
    if (!props.disabled && props.value.trim()) props.onSend();
  };
  const onKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      send();
    }
  };
  return (
    <div className={styles.composer}>
      <textarea
        value={props.value}
        placeholder={props.placeholder}
        aria-label={props.ariaLabel}
        onChange={(event) => props.onChange(event.target.value)}
        onKeyDown={onKeyDown}
      />
      <Button variant="primary" disabled={props.disabled || !props.value.trim()} onClick={send}>
        {props.sendLabel}
      </Button>
    </div>
  );
}

export interface ToastItem {
  id: string;
  text: string;
}

export function ToastViewport(props: { toasts: ToastItem[]; onDismiss: (id: string) => void }) {
  const { toasts, onDismiss } = props;
  useEffect(() => {
    if (!toasts.length) return;
    const timers = toasts.map((toast) => setTimeout(() => onDismiss(toast.id), 2_600));
    return () => timers.forEach(clearTimeout);
  }, [toasts, onDismiss]);
  if (!toasts.length) return null;
  return (
    <div className={styles.toastViewport} role="status" aria-live="polite">
      {toasts.map((toast) => (
        <div key={toast.id} className={styles.toast}>{toast.text}</div>
      ))}
    </div>
  );
}
