// Draft persistence and the Drafts screen (R-A9): autosave, listing with
// source and validation status, Resume / Discard.

import { render, screen } from "@testing-library/react";
import { act, renderHook } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { blankDraft } from "./defaults";
import { DraftsScreen } from "./DraftsScreen";
import {
  discardDraft,
  listDrafts,
  newDraftId,
  saveDraft,
  useDraftAutosave,
  type DraftRecord,
} from "./store";

function freshStorage(): Storage {
  const map = new Map<string, string>();
  return {
    get length() {
      return map.size;
    },
    clear: () => map.clear(),
    getItem: (key: string) => map.get(String(key)) ?? null,
    key: (index: number) => [...map.keys()][index] ?? null,
    removeItem: (key: string) => void map.delete(String(key)),
    setItem: (key: string, value: string) => void map.set(String(key), String(value)),
  };
}

function record(name: string, updatedAt: string): DraftRecord {
  return {
    id: newDraftId(),
    draft: { ...blankDraft("chat"), name },
    createdAt: updatedAt,
    updatedAt,
  };
}

beforeEach(() => window.localStorage.clear());
afterEach(() => vi.useRealTimers());

describe("draft store", () => {
  it("saves, lists most-recent-first, and discards", () => {
    const storage = freshStorage();
    const older = record("Older", "2026-09-05T10:00:00Z");
    const newer = record("Newer", "2026-09-05T11:00:00Z");
    saveDraft(older, storage, older.updatedAt);
    saveDraft(newer, storage, newer.updatedAt);
    expect(listDrafts(storage).map((r) => r.draft.name)).toEqual(["Newer", "Older"]);
    discardDraft(older.id, storage);
    expect(listDrafts(storage).map((r) => r.draft.name)).toEqual(["Newer"]);
  });

  it("upserts the same id instead of duplicating", () => {
    const storage = freshStorage();
    const draft = record("v1", "2026-09-05T10:00:00Z");
    saveDraft(draft, storage, draft.updatedAt);
    saveDraft({ ...draft, draft: { ...draft.draft, name: "v2" } }, storage, "2026-09-05T12:00:00Z");
    const all = listDrafts(storage);
    expect(all).toHaveLength(1);
    expect(all[0].draft.name).toBe("v2");
    expect(all[0].updatedAt).toBe("2026-09-05T12:00:00Z");
  });

  it("autosaves after a pause, not on the first render", () => {
    vi.useFakeTimers();
    const storage = freshStorage();
    const draft = record("Auto", "2026-09-05T10:00:00Z");
    const { rerender } = renderHook(({ r }) => useDraftAutosave(r, storage, 400), {
      initialProps: { r: draft as DraftRecord | null },
    });
    expect(listDrafts(storage)).toEqual([]);
    rerender({ r: { ...draft, draft: { ...draft.draft, name: "Auto v2" } } });
    act(() => vi.advanceTimersByTime(399));
    expect(listDrafts(storage)).toEqual([]);
    act(() => vi.advanceTimersByTime(1));
    expect(listDrafts(storage)[0]?.draft.name).toBe("Auto v2");
  });
});

describe("DraftsScreen", () => {
  it("shows the handoff empty state", () => {
    render(<DraftsScreen storage={freshStorage()} />);
    expect(screen.getByText("No drafts. Start one from Agents.")).toBeInTheDocument();
  });

  it("lists cards with source, validation label, Resume and Discard", async () => {
    const user = userEvent.setup();
    const storage = freshStorage();
    saveDraft(record("Support bot", "2026-09-05T10:00:00Z"), storage, "2026-09-05T10:00:00Z");
    const resumed: string[] = [];
    render(<DraftsScreen storage={storage} onResume={(r) => resumed.push(r.id)} />);
    expect(screen.getByText("Support bot")).toBeInTheDocument();
    expect(screen.getByText("chat")).toBeInTheDocument();
    // The blank draft carries violations, labeled with the count.
    expect(screen.getByText(/\d+ open violations/)).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Resume" }));
    expect(resumed).toHaveLength(1);
    await user.click(screen.getByRole("button", { name: "Discard" }));
    expect(listDrafts(storage)).toEqual([]);
    expect(screen.getByText("No drafts. Start one from Agents.")).toBeInTheDocument();
  });

  it("labels a clean draft Passing", () => {
    const storage = freshStorage();
    const clean = record("Clean", "2026-09-05T10:00:00Z");
    clean.draft = {
      ...clean.draft,
      description: "d",
      model: "gpt-5",
      goal: "g",
      measures: [{ name: "m", source: "outcome", target: "t", window: "7d", kind: "target" }],
      stable: "s",
      gateSuite: "suite",
    };
    saveDraft(clean, storage, clean.updatedAt);
    render(<DraftsScreen storage={storage} />);
    expect(screen.getByText("Passing")).toBeInTheDocument();
  });
});
