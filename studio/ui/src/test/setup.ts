import "@testing-library/jest-dom/vitest";
import { cleanup } from "@testing-library/react";
import { afterEach, vi } from "vitest";

afterEach(() => cleanup());
Object.defineProperty(window, "scrollTo", { value: vi.fn(), writable: true });
Object.defineProperty(HTMLElement.prototype, "scrollTo", { value: vi.fn(), writable: true });

const storedValues = new Map<string, string>();
Object.defineProperty(globalThis, "localStorage", {
  configurable: true,
  value: {
    get length() { return storedValues.size; },
    clear: () => storedValues.clear(),
    getItem: (key: string) => storedValues.get(String(key)) ?? null,
    key: (index: number) => [...storedValues.keys()][index] ?? null,
    removeItem: (key: string) => storedValues.delete(String(key)),
    setItem: (key: string, value: string) => storedValues.set(String(key), String(value)),
  } satisfies Storage,
});
