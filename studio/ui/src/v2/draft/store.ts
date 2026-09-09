// Draft persistence (R-A9): every entry path autosaves into one local store;
// the Drafts screen lists them with Resume / Discard. Storage is injectable
// so tests and (later) a server-backed store share the same calls.

import { useEffect, useRef } from "react";
import type { AgentDraft } from "./agent-draft.gen";

export interface DraftRecord {
  id: string;
  draft: AgentDraft;
  createdAt: string;
  updatedAt: string;
}

const STORAGE_KEY = "rusty.studio.v2.drafts";

export const AUTOSAVE_DEBOUNCE_MS = 800;

function defaultStorage(): Storage {
  return window.localStorage;
}

function readAll(storage: Storage): DraftRecord[] {
  try {
    const raw = storage.getItem(STORAGE_KEY);
    if (!raw) return [];
    const parsed: unknown = JSON.parse(raw);
    if (!Array.isArray(parsed)) return [];
    return parsed as DraftRecord[];
  } catch {
    return [];
  }
}

function writeAll(storage: Storage, records: DraftRecord[]): void {
  storage.setItem(STORAGE_KEY, JSON.stringify(records));
}

/** All saved drafts, most recently updated first. */
export function listDrafts(storage: Storage = defaultStorage()): DraftRecord[] {
  return readAll(storage).sort((a, b) => b.updatedAt.localeCompare(a.updatedAt));
}

/** Insert or replace a draft record, stamping `updatedAt`. */
export function saveDraft(
  record: DraftRecord,
  storage: Storage = defaultStorage(),
  now: string = new Date().toISOString(),
): DraftRecord {
  const stamped = { ...record, updatedAt: now };
  const rest = readAll(storage).filter((existing) => existing.id !== record.id);
  writeAll(storage, [...rest, stamped]);
  return stamped;
}

export function discardDraft(id: string, storage: Storage = defaultStorage()): void {
  writeAll(
    storage,
    readAll(storage).filter((record) => record.id !== id),
  );
}

export function newDraftId(): string {
  if (typeof crypto !== "undefined" && "randomUUID" in crypto) return crypto.randomUUID();
  return `draft-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
}

/** Persist the record after each pause in editing. */
export function useDraftAutosave(
  record: DraftRecord | null,
  storage?: Storage,
  debounceMs: number = AUTOSAVE_DEBOUNCE_MS,
): void {
  const first = useRef(true);
  useEffect(() => {
    if (!record) return;
    if (first.current) {
      first.current = false;
      return;
    }
    const timer = setTimeout(() => saveDraft(record, storage), debounceMs);
    return () => clearTimeout(timer);
  }, [record, storage, debounceMs]);
}
