/** Flatten a served instance config into dot-path rows. A sealed secret
 * arrives as the `{"rusty_secret": true}` marker and flattens to itself —
 * the list renders it as "set, never rendered". */
export interface ConfigRow {
  path: string;
  value: unknown;
}

function isSecretMarker(value: unknown): boolean {
  return typeof value === "object" && value !== null && !Array.isArray(value)
    && Object.keys(value as Record<string, unknown>).length === 1
    && (value as Record<string, unknown>).rusty_secret === true;
}

export { isSecretMarker };

export function flattenConfig(config: Record<string, unknown>, prefix = ""): ConfigRow[] {
  const rows: ConfigRow[] = [];
  for (const [key, value] of Object.entries(config)) {
    const path = prefix ? `${prefix}.${key}` : key;
    if (isSecretMarker(value)) {
      rows.push({ path, value });
    } else if (typeof value === "object" && value !== null && !Array.isArray(value)) {
      rows.push(...flattenConfig(value as Record<string, unknown>, path));
    } else {
      rows.push({ path, value });
    }
  }
  return rows;
}

export function formatInstant(value: string) {
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return value;
  return `${date.toISOString().slice(0, 16).replace("T", " ")} UTC`;
}

export function hashPreview(hash: string) {
  return hash.length > 12 ? hash.slice(0, 12) : hash;
}
