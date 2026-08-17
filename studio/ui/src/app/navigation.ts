type PrimaryRoute = "/" | "/agents" | "/agents/new" | "/agents/prompts" | "/skills" | "/knowledge" | "/connectors" | "/work" | "/memory" | "/operations";

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
      { to: "/skills", label: "Skills & Tools", shortLabel: "Skills", description: "Publish and review skill packages", icon: "M12 3 3 8l9 5 9-5zM3 13l9 5 9-5", match: (path: string) => path.startsWith("/skills") },
      { to: "/knowledge", label: "Knowledge", shortLabel: "Knowledge", description: "Governed sources and retrieval", icon: "M4 4.5h6a2 2 0 0 1 2 2v13a2 2 0 0 0-2-2H4zM20 4.5h-6a2 2 0 0 0-2 2v13a2 2 0 0 1 2-2h6z", match: (path: string) => path.startsWith("/knowledge") },
      { to: "/connectors", label: "Connectors", shortLabel: "Connectors", description: "Schema-driven connector setup", icon: "M9 3.5v4M15 3.5v4M6 7.5h12v4a6 6 0 0 1-12 0zM12 17.5V21", match: (path: string) => path.startsWith("/connectors") },
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
      { to: "/memory", label: "Memory", shortLabel: "Memory", description: "Governed memory, corrections, and conflicts", icon: "M4 6.5c0-1.7 3.6-3 8-3s8 1.3 8 3-3.6 3-8 3-8-1.3-8-3zM4 6.5v11c0 1.7 3.6 3 8 3s8-1.3 8-3v-11", match: (path: string) => path.startsWith("/memory") },
      { to: "/operations", label: "Operations", shortLabel: "Operations", description: "Review exceptions", icon: "M2 12h4l3-7 4 14 3-7h6", match: (path: string) => path.startsWith("/operations") },
    ],
  },
] as const;

export const primaryDestinations = lifecycleGroups.flatMap((group) => group.destinations);

export function destinationForPath(pathname: string) {
  return primaryDestinations.find((item) => item.match(pathname));
}
