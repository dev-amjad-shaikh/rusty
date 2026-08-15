import type { ReactNode } from "react";
import styles from "./RustyCardFrame.module.css";

export type RustyCardTone = "queued" | "working" | "needs" | "stuck" | "done";

export function RustyCardFrame({ children, tone, className = "" }: { children: ReactNode; tone: RustyCardTone; className?: string }) {
  return <div className={`${styles.frame} ${className}`} data-rusty-card="forged" data-tone={tone}>
    {children}
  </div>;
}
