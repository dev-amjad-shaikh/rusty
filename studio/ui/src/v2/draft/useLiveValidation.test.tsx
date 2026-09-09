// Live validation timing (R-A3): the full rule set reruns on pause-typing,
// inside the 300 ms budget, without running on every keystroke.

import { act, renderHook } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { EMPTY_CATALOGS } from "./catalogs";
import { blankDraft } from "./defaults";
import { useLiveValidation, VALIDATION_DEBOUNCE_MS } from "./useLiveValidation";

afterEach(() => vi.useRealTimers());

describe("useLiveValidation", () => {
  it("the debounce sits inside the 300 ms budget", () => {
    expect(VALIDATION_DEBOUNCE_MS).toBeLessThanOrEqual(300);
  });

  it("validates immediately on mount, then on pause-typing", () => {
    vi.useFakeTimers();
    const blank = blankDraft("guided");
    const { result, rerender } = renderHook(({ draft }) => useLiveValidation(draft), {
      initialProps: { draft: blank },
    });
    // Mount: the report is already there — blank drafts show their slots.
    expect(result.current.violations.length).toBeGreaterThan(0);
    expect(result.current.violations.some((v) => v.path === "name")).toBe(true);

    // A keystroke enters the debounce window: pending, report not yet stale-free.
    const named = { ...blank, name: "Support" };
    rerender({ draft: named });
    expect(result.current.pending).toBe(true);
    expect(result.current.violations.some((v) => v.path === "name")).toBe(true);

    // Still inside the window — nothing has rerun.
    act(() => vi.advanceTimersByTime(VALIDATION_DEBOUNCE_MS - 50));
    expect(result.current.violations.some((v) => v.path === "name")).toBe(true);

    // Pause-typing elapsed: the report catches up within the budget.
    act(() => vi.advanceTimersByTime(50));
    expect(result.current.pending).toBe(false);
    expect(result.current.violations.some((v) => v.path === "name")).toBe(false);
  });

  it("rapid keystrokes collapse into one run", () => {
    vi.useFakeTimers();
    const blank = blankDraft("guided");
    const { result, rerender } = renderHook(({ draft }) => useLiveValidation(draft), {
      initialProps: { draft: blank },
    });
    for (const name of ["S", "Su", "Sup", "Support"]) {
      rerender({ draft: { ...blank, name } });
      act(() => vi.advanceTimersByTime(VALIDATION_DEBOUNCE_MS - 100));
    }
    // Never settled mid-stream.
    expect(result.current.pending).toBe(true);
    act(() => vi.advanceTimersByTime(VALIDATION_DEBOUNCE_MS));
    expect(result.current.violations.some((v) => v.path === "name")).toBe(false);
  });

  it("accepts catalogs for coherence rules", () => {
    vi.useFakeTimers();
    const draft = { ...blankDraft("guided"), name: "A", goal: "G", stable: "S", gateSuite: "known" };
    const catalogs = { ...EMPTY_CATALOGS, evalSuites: ["known"] };
    const { result } = renderHook(() => useLiveValidation(draft, catalogs));
    expect(result.current.violations.some((v) => v.path === "gateSuite")).toBe(false);
  });
});
