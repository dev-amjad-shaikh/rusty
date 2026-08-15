type PrimaryRoute = "/" | "/agents" | "/agents/prompts" | "/work" | "/operations";

export interface PrimaryDestination {
  to: PrimaryRoute;
  label: string;
  shortLabel: string;
  description: string;
  glyph: string;
  match: (pathname: string) => boolean;
}

interface LifecycleGroup {
  label: string;
  destinations: PrimaryDestination[];
}

export const lifecycleGroups: LifecycleGroup[] = [
  {
    label: "Oversee",
    destinations: [
      { to: "/", label: "Command Center", shortLabel: "Command", description: "See work and exceptions", glyph: "C", match: (path: string) => path === "/" },
    ],
  },
  {
    label: "Build",
    destinations: [
      { to: "/agents", label: "Agent portfolio", shortLabel: "Agents", description: "Design and activate agents", glyph: "A", match: (path: string) => path.startsWith("/agents") && path !== "/agents/prompts" },
      { to: "/agents/prompts", label: "Prompt library", shortLabel: "Prompts", description: "Version and test prompts", glyph: "P", match: (path: string) => path === "/agents/prompts" },
    ],
  },
  {
    label: "Operate",
    destinations: [
      { to: "/work", label: "Run workspace", shortLabel: "Work", description: "Run, trace, and evaluate", glyph: "R", match: (path: string) => path.startsWith("/work") },
      { to: "/operations", label: "Operations", shortLabel: "Operations", description: "Review exceptions", glyph: "!", match: (path: string) => path.startsWith("/operations") },
    ],
  },
] as const;

export const primaryDestinations = lifecycleGroups.flatMap((group) => group.destinations);

export function destinationForPath(pathname: string) {
  return primaryDestinations.find((item) => item.match(pathname));
}
