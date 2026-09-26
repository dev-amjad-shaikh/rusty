// What an agent may call, read from the server's own catalog.
//
// The studio does not know what tools exist. It asks the server which tools
// the graph offers — the built-in ones and every one a configured connection
// derives — and shows them grouped by where they come from. A connection's
// tools are named `{connector}.{operation}` by the server, and that name is
// the one that runs, so picking here is picking exactly.

export interface CatalogTool {
  name: string;
  description: string;
  effect: string;
  parameters_schema?: Record<string, unknown>;
}

export interface ToolGroup {
  /** "Built in", or the connector a connection derives these from. */
  label: string;
  /** The connector id when the group is a connection's tools. */
  connector?: string;
  /** Where the group's tools come from, when not built in. */
  kind?: "connection" | "mcp" | "platform";
  tools: CatalogTool[];
}

/** The check operation is a gate, not an action — it proves a configuration
 * and returns nothing an agent would reason over. It stays in the catalog and
 * out of the picker. */
const isCheck = (name: string) => /\.check-connection$/.test(name) || /\.check$/.test(name);

/**
 * Group a graph's tools for a builder to choose from. Built-in tools first;
 * then one group per connector, in name order. A connector that has two
 * connections appears as two groups, because the server names the second
 * `{connector}@{instance}.{operation}` and the two are not interchangeable.
 */
/** The prefixes of the platform's own doors — the Composer's tools. */
export const PLATFORM_PREFIXES = new Set(["catalog", "agents", "skills", "connectors"]);

export function groupTools(tools: CatalogTool[], mcpServers: Iterable<string> = []): ToolGroup[] {
  const mcp = new Set(mcpServers);
  const builtIn: CatalogTool[] = [];
  const byConnector = new Map<string, CatalogTool[]>();
  for (const tool of [...tools].sort((a, b) => a.name.localeCompare(b.name))) {
    if (isCheck(tool.name)) continue;
    const dot = tool.name.indexOf(".");
    if (dot < 0) {
      builtIn.push(tool);
      continue;
    }
    const connector = tool.name.slice(0, dot);
    const list = byConnector.get(connector) ?? [];
    list.push(tool);
    byConnector.set(connector, list);
  }
  const groups: ToolGroup[] = [];
  if (builtIn.length) groups.push({ label: "Built in", tools: builtIn });
  for (const [connector, list] of byConnector) {
    const kind = mcp.has(connector) ? "mcp" : PLATFORM_PREFIXES.has(connector) ? "platform" : "connection";
    groups.push({ label: kind === "platform" ? `The platform · ${connector}` : connectorLabel(connector), connector, kind, tools: list });
  }
  return groups;
}

/** `servicenow` → "ServiceNow" is the server's display name, which this
 * catalog does not carry; the id, capitalised, is what can be said honestly.
 * `servicenow@1a1b32af` → "servicenow · 1a1b32af". */
export function connectorLabel(connector: string): string {
  const [id, instance] = connector.split("@");
  const name = id.charAt(0).toUpperCase() + id.slice(1);
  return instance ? `${name} · ${instance}` : name;
}

/** The tools an agent declares, in the shape the server stores them. */
/** The intent's tool list from a selection: sorted, de-duplicated, each with
 * the builder's when-note when one was written. */
export const toolSelection = (names: Iterable<string>, notes?: Map<string, string>): { name: string; when?: string }[] =>
  [...new Set(names)].sort().map((name) => {
    const when = notes?.get(name)?.trim();
    return when ? { name, when } : { name };
  });
