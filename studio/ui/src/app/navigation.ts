type PrimaryRoute = "/" | "/agents" | "/agents/new" | "/agents/prompts" | "/work" | "/operations";

export interface PrimaryDestination {
  to: PrimaryRoute;
  label: string;
  shortLabel: string;
  description: string;
  icon: string;
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
      { to: "/", label: "Command Center", shortLabel: "Command", description: "See work and exceptions", icon: "M4 13.5V20a1 1 0 0 0 1 1h14a1 1 0 0 0 1-1v-6.5M2.5 12 12 4l9.5 8", match: (path: string) => path === "/" },
      { to: "/agents", label: "Agent Portfolio", shortLabel: "Agents", description: "Review active definitions", icon: "M3.5 3.5h7v7h-7zM13.5 3.5h7v7h-7zM3.5 13.5h7v7h-7zM13.5 13.5h7v7h-7z", match: (path: string) => path === "/agents" || (path.startsWith("/agents/") && path !== "/agents/new" && path !== "/agents/prompts") },
    ],
  },
  {
    label: "Build",
    destinations: [
      { to: "/agents/new", label: "Agent Builder", shortLabel: "Builder", description: "Create a guided definition", icon: "M16.5 3.5l4 4L8 20H4v-4z", match: (path: string) => path === "/agents/new" },
      { to: "/agents/prompts", label: "Prompt Library", shortLabel: "Prompts", description: "Version and test prompts", icon: "M3.5 4.5h17v15h-17zM7 9h4M7 13h7", match: (path: string) => path === "/agents/prompts" },
    ],
  },
  {
    label: "Prove",
    destinations: [
      { to: "/work", label: "Run & Evaluate", shortLabel: "Work", description: "Run, trace, and evaluate", icon: "M10 3v6.5L5 19a1.5 1.5 0 0 0 1.3 2.2h11.4A1.5 1.5 0 0 0 19 19l-5-9.5V3M8.5 3h7", match: (path: string) => path.startsWith("/work") },
    ],
  },
  {
    label: "Operate",
    destinations: [
      { to: "/operations", label: "Operations", shortLabel: "Operations", description: "Review exceptions", icon: "M2 12h4l3-7 4 14 3-7h6", match: (path: string) => path.startsWith("/operations") },
    ],
  },
] as const;

export const primaryDestinations = lifecycleGroups.flatMap((group) => group.destinations);

export function destinationForPath(pathname: string) {
  return primaryDestinations.find((item) => item.match(pathname));
}
