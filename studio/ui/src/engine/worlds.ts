import type { World } from "./net/client";

/** The worlds an agent may act in: those standing in for a system one of
 * its tools calls. An agent that names no tools, or asks other agents,
 * reaches every world. The Playground's rule, shared by every picker. */
export function worldsForAgent(toolNames: string[], worlds: World[]): World[] {
  if (toolNames.length === 0 || toolNames.includes("agents.ask")) return worlds;
  return worlds.filter((w) => toolNames.some((n) => n === w.connector || n.startsWith(`${w.connector}.`) || n.startsWith(`${w.connector}-`)));
}

/** The connector a tool name reaches: the part before the first dot, the
 * way a connection names its tools (`servicenow.list-records`). */
export function connectorOf(toolName: string): string {
  return toolName.split(".")[0] ?? toolName;
}

/** The systems an agent touches that have a stand-in, each with its
 * worlds — one pick per system when a run may be in several worlds. An
 * agent that names no tools, or asks other agents, is one pick of every
 * world (its reach is through others). */
export function worldsByConnector(toolNames: string[], worlds: World[]): { connector: string; worlds: World[] }[] {
  if (toolNames.length === 0 || toolNames.includes("agents.ask")) return [];
  const seen = new Set<string>();
  const out: { connector: string; worlds: World[] }[] = [];
  for (const name of toolNames) {
    const connector = connectorOf(name);
    if (seen.has(connector)) continue;
    seen.add(connector);
    const mine = worlds.filter((w) => w.connector === connector || connector.startsWith(`${w.connector}-`));
    if (mine.length > 0) out.push({ connector, worlds: mine });
  }
  return out;
}
