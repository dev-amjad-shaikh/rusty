import type { ConnectorManifest } from "../../lib/api/connectors";
import { resolvedBaseUrlPreview } from "./lifecycle";
import styles from "./InstantiatePanel.module.css";

function configParamLabel(name: string) {
  const words = name.replace(/_/g, " ");
  return words.charAt(0).toUpperCase() + words.slice(1);
}

export interface ConfigFieldsProps {
  manifest: ConnectorManifest;
  values: Record<string, string>;
  error: { param: string | null; message: string } | null;
  disabled: boolean;
  onChange: (name: string, value: string) => void;
  onClearError: () => void;
}

export function ConfigFields({ manifest, values, error, disabled, onChange, onClearError }: ConfigFieldsProps) {
  if (manifest.config_params.length === 0) return null;
  const baseUrlPreview = resolvedBaseUrlPreview(manifest, values);

  return (
    <div className={styles.slotBindings} aria-label="Instance configuration">
      {manifest.config_params.map((param) => {
        const failed = error?.param === param.name;
        return (
          <div className={styles.grantField} key={param.name}>
            <label htmlFor={`config-${param.name}`}>{configParamLabel(param.name)}</label>
            <input
              id={`config-${param.name}`}
              type="text"
              value={values[param.name] ?? ""}
              autoComplete="off"
              spellCheck={false}
              disabled={disabled}
              aria-invalid={failed || undefined}
              aria-describedby={failed ? `config-${param.name}-error` : undefined}
              onChange={(event) => {
                onChange(param.name, event.target.value);
                if (failed) onClearError();
              }}
            />
            {param.description && <small>{param.description}</small>}
          </div>
        );
      })}
      {baseUrlPreview && <p className={styles.quiet}>This instance will call <code>{baseUrlPreview}</code></p>}
      {error && (
        <p className={styles.errorInline} role="alert" id={error.param ? `config-${error.param}-error` : undefined}>
          {error.message}
        </p>
      )}
    </div>
  );
}

export function trimmedConfig(manifest: ConnectorManifest, values: Record<string, string>): Record<string, string> {
  return Object.fromEntries(
    manifest.config_params.map((param) => [param.name, (values[param.name] ?? "").trim()]),
  );
}

export function validateConfig(
  manifest: ConnectorManifest,
  values: Record<string, string>,
): { param: string | null; message: string } | null {
  for (const param of manifest.config_params) {
    if (!(values[param.name] ?? "").trim()) {
      return { param: param.name, message: `config param \`${param.name}\` requires a value` };
    }
  }
  return null;
}
