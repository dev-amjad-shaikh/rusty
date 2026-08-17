import type {
  ConnectorForm,
  FormField,
  FormNode,
  FormValues,
  VariantNode,
  VariantSelections,
} from "../../lib/schema-form";
import { humanizeName } from "../../lib/schema-form";
import styles from "./ConnectorsPage.module.css";

// The generic schema-driven setup form: every connector renders through this
// one component from the interpreted `connection_specification` — no
// per-connector UI code (`research/airbyte-connector-configuration-model.md`,
// section 3).

export interface SchemaFormProps {
  form: ConnectorForm;
  values: FormValues;
  selections: VariantSelections;
  /** 422-pinned field errors, keyed by dot path. */
  errors: Record<string, string>;
  onValue: (path: string, value: unknown) => void;
  onSelect: (path: string, variantKey: string) => void;
}

interface NodeListProps {
  nodes: FormNode[];
  values: FormValues;
  selections: VariantSelections;
  errors: Record<string, string>;
  onValue: (path: string, value: unknown) => void;
  onSelect: (path: string, variantKey: string) => void;
}

function fieldId(path: string) {
  return `cfg-${path.replace(/[^A-Za-z0-9_-]+/g, "-")}`;
}

function FieldInput({ field, value, error, onValue }: {
  field: FormField;
  value: unknown;
  error: string | undefined;
  onValue: (path: string, value: unknown) => void;
}) {
  const id = fieldId(field.path);
  const hint = field.description ?? field.patternHint;
  const describedBy = [error ? `${id}-error` : null, hint ? `${id}-hint` : null].filter(Boolean).join(" ") || undefined;
  if (field.input === "boolean") {
    return (
      <div className={styles.field}>
        <span className={styles.checkRow}>
          <input
            id={id}
            type="checkbox"
            checked={value === true}
            onChange={(event) => onValue(field.path, event.target.checked)}
            aria-invalid={error ? true : undefined}
            aria-describedby={describedBy}
          />
          <label htmlFor={id}>{field.title}</label>
        </span>
        {hint && <span className={styles.fieldHint} id={`${id}-hint`}>{hint}</span>}
        {error && <span className={styles.fieldError} id={`${id}-error`} role="alert">{error}</span>}
      </div>
    );
  }
  return (
    <div className={styles.field}>
      <label htmlFor={id}>{field.title}</label>
      {field.input === "select" ? (
        <select
          id={id}
          value={typeof value === "string" ? value : ""}
          onChange={(event) => onValue(field.path, event.target.value)}
          aria-invalid={error ? true : undefined}
          aria-describedby={describedBy}
          aria-required={field.required}
        >
          <option value="">Choose…</option>
          {field.enumValues!.map((option) => <option key={option} value={option}>{option}</option>)}
        </select>
      ) : (
        <input
          id={id}
          type={field.input === "password" ? "password" : field.input === "number" ? "number" : "text"}
          value={typeof value === "string" || typeof value === "number" ? String(value) : ""}
          onChange={(event) => onValue(field.path, event.target.value)}
          placeholder={field.patternHint ?? undefined}
          autoComplete="off"
          aria-invalid={error ? true : undefined}
          aria-describedby={describedBy}
          aria-required={field.required}
        />
      )}
      {hint && <span className={styles.fieldHint} id={`${id}-hint`}>{hint}</span>}
      {error && <span className={styles.fieldError} id={`${id}-error`} role="alert">{error}</span>}
    </div>
  );
}

function VariantPicker({ node, values, selections, errors, onValue, onSelect }: Omit<NodeListProps, "nodes"> & { node: VariantNode }) {
  const id = fieldId(node.path);
  const selected = selections[node.path] ?? node.variants[0]?.key;
  const chosen = node.variants.find((variant) => variant.key === selected) ?? node.variants[0];
  const error = errors[node.path];
  return (
    <div className={styles.variant}>
      <div className={styles.field}>
        <label htmlFor={id}>{node.title}</label>
        <select
          id={id}
          value={selected ?? ""}
          onChange={(event) => onSelect(node.path, event.target.value)}
          aria-invalid={error ? true : undefined}
          aria-describedby={error ? `${id}-error` : undefined}
        >
          {node.variants.map((variant) => <option key={variant.key} value={variant.key}>{variant.label}</option>)}
        </select>
        {node.description && <span className={styles.fieldHint}>{node.description}</span>}
        {error && <span className={styles.fieldError} id={`${id}-error`} role="alert">{error}</span>}
      </div>
      {chosen && chosen.children.length > 0 && (
        <div className={styles.variantFields}>
          <NodeList nodes={chosen.children} values={values} selections={selections} errors={errors} onValue={onValue} onSelect={onSelect} />
        </div>
      )}
    </div>
  );
}

function NodeList({ nodes, values, selections, errors, onValue, onSelect }: NodeListProps) {
  return (
    <>
      {nodes.map((node) => {
        if (node.kind === "field") {
          // Standalone consts are applied to the config, never rendered.
          if (node.constValue !== undefined) return null;
          return <FieldInput key={node.path} field={node} value={values[node.path]} error={errors[node.path]} onValue={onValue} />;
        }
        if (node.kind === "variant") {
          return (
            <VariantPicker
              key={node.path}
              node={node}
              values={values}
              selections={selections}
              errors={errors}
              onValue={onValue}
              onSelect={onSelect}
            />
          );
        }
        return (
          <fieldset key={node.path} className={styles.fieldset}>
            <legend>{node.title}</legend>
            {node.description && <p className={styles.fieldHint}>{node.description}</p>}
            {errors[node.path] && <p className={styles.fieldError} role="alert">{errors[node.path]}</p>}
            <NodeList nodes={node.children} values={values} selections={selections} errors={errors} onValue={onValue} onSelect={onSelect} />
          </fieldset>
        );
      })}
    </>
  );
}

export function SchemaForm({ form, values, selections, errors, onValue, onSelect }: SchemaFormProps) {
  const ungrouped = form.nodes.filter((node) => !node.group);
  return (
    <div className={styles.schemaForm}>
      <div className={styles.fields}>
        <NodeList nodes={ungrouped} values={values} selections={selections} errors={errors} onValue={onValue} onSelect={onSelect} />
      </div>
      {form.groups.map((group) => (
        <section key={group} className={styles.group} aria-label={humanizeName(group)}>
          <h3>{humanizeName(group)}</h3>
          <div className={styles.fields}>
            <NodeList
              nodes={form.nodes.filter((node) => node.group === group)}
              values={values}
              selections={selections}
              errors={errors}
              onValue={onValue}
              onSelect={onSelect}
            />
          </div>
        </section>
      ))}
    </div>
  );
}
