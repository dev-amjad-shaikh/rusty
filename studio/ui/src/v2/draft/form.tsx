// AgentDraftForm (R-A1): one form for every entry path, generated from the
// AgentDraft JSON Schema. The engine walks the schema's properties, groups
// them by x-section, and dispatches each field on its x-control hint — a
// field added to the schema renders here (and therefore in Guided, Compose,
// Chat, Import, and Improve) without per-path code. Violations anchor under
// the control whose path they name (R-A3).

import type { ReactNode } from "react";
import { Badge, Button, Pill, Select, TextArea, TextInput } from "../controls/controls";
import {
  AGENT_DRAFT_SCHEMA,
  EMPTY_CATALOGS,
  mountedTools,
  type DraftCatalogs,
  type DraftSchema,
  type FieldSchema,
  type SectionMeta,
} from "./catalogs";
import type { Violation } from "./validate";
import type { AgentDraft } from "./agent-draft.gen";
import styles from "./form.module.css";

export interface AgentDraftFormProps {
  draft: AgentDraft;
  onChange: (draft: AgentDraft) => void;
  /** The live validation report; each violation renders under its control. */
  violations?: Violation[];
  catalogs?: DraftCatalogs;
  /** Defaults to the committed schema; injectable to prove schema-drivenness. */
  schema?: DraftSchema;
}

interface Ctx {
  draft: AgentDraft;
  catalogs: DraftCatalogs;
  violations: Violation[];
}

interface SectionGroup {
  meta?: SectionMeta;
  fields: [string, FieldSchema][];
}

export function AgentDraftForm(props: AgentDraftFormProps) {
  const { draft, onChange, violations = [], catalogs = EMPTY_CATALOGS, schema = AGENT_DRAFT_SCHEMA } = props;
  const ctx: Ctx = { draft, catalogs, violations };
  const setField = (key: string, value: unknown) =>
    onChange({ ...draft, [key]: value } as AgentDraft);
  return (
    <div className={styles.form}>
      {groupSections(schema).map((section, i) => (
        <section
          key={section.meta?.id ?? `other-${i}`}
          className={styles.section}
          aria-label={section.meta?.label}
        >
          {section.meta && (
            <div className={styles.sectionHeader}>
              <h3 className={styles.sectionTitle}>{section.meta.label}</h3>
              {section.meta.note && <span className={styles.sectionNote}>{section.meta.note}</span>}
            </div>
          )}
          {section.fields.map(([key, field]) => (
            <Field
              key={key}
              path={key}
              field={field}
              value={(draft as unknown as Record<string, unknown>)[key]}
              set={(value) => setField(key, value)}
              ctx={ctx}
            />
          ))}
        </section>
      ))}
    </div>
  );
}

/** Group visible fields by their x-section, in x-sections order. */
function groupSections(schema: DraftSchema): SectionGroup[] {
  const metas = schema["x-sections"] ?? [];
  const bySection = new Map<string, [string, FieldSchema][]>();
  const loose: [string, FieldSchema][] = [];
  for (const [key, field] of Object.entries(schema.properties)) {
    if (field["x-control"] === "hidden") continue;
    const id = field["x-section"];
    if (id) {
      const list = bySection.get(id) ?? [];
      list.push([key, field]);
      bySection.set(id, list);
    } else {
      loose.push([key, field]);
    }
  }
  const groups: SectionGroup[] = metas
    .filter((meta) => bySection.has(meta.id))
    .map((meta) => ({ meta, fields: bySection.get(meta.id)! }));
  for (const [id, fields] of bySection) {
    if (!metas.some((meta) => meta.id === id)) groups.push({ meta: { id, label: id }, fields });
  }
  if (loose.length) groups.push({ fields: loose });
  return groups;
}

interface FieldProps {
  path: string;
  field: FieldSchema;
  value: unknown;
  set: (value: unknown) => void;
  ctx: Ctx;
  /** The containing list item, for sibling-aware controls (trigger-spec). */
  item?: Record<string, unknown>;
}

function Field(props: FieldProps) {
  const { path, field, ctx } = props;
  const control = field["x-control"] ?? "text";
  if (control === "hidden") return null;
  const mine = ctx.violations.filter((v) => v.path === path);
  return (
    <div className={`${styles.field} ${mine.length ? styles.invalid : ""}`}>
      {field["x-label"] && <span className={styles.fieldLabel}>{field["x-label"]}</span>}
      {renderControl(props, control)}
      {field["x-help"] && <p className={styles.fieldHelp}>{field["x-help"]}</p>}
      <FieldViolations violations={mine} />
    </div>
  );
}

function FieldViolations({ violations }: { violations: Violation[] }) {
  if (!violations.length) return null;
  return (
    <ul className={styles.violations} aria-live="polite">
      {violations.map((v) => (
        <li key={`${v.rule}:${v.path}`} className={styles.violation}>
          <Badge tone={v.kind === "schema" ? "err" : v.kind === "coherence" ? "warn" : "neutral"}>
            {v.kind}
          </Badge>
          <span>{v.message}</span>
          {v.specFile && <span className={styles.violationFile}>{v.specFile}</span>}
        </li>
      ))}
    </ul>
  );
}

function renderControl(props: FieldProps, control: string): ReactNode {
  const { field, value, set, ctx, item } = props;
  const label = field["x-label"] ?? props.path;
  const str = typeof value === "string" ? value : "";

  if (control === "text") {
    return <TextInput value={str} mono={field["x-mono"]} ariaLabel={label} onChange={set} />;
  }
  if (control === "number") {
    return (
      <TextInput
        value={typeof value === "number" ? String(value) : ""}
        mono
        ariaLabel={label}
        onChange={(v) => {
          const n = Number(v);
          if (Number.isFinite(n)) set(n);
        }}
      />
    );
  }
  if (control === "textarea") {
    return <TextArea value={str} ariaLabel={label} onChange={set} />;
  }
  if (control === "boolean") {
    return (
      <Pill selected={Boolean(value)} onClick={() => set(!value)}>
        {value ? "On" : "Off"}
      </Pill>
    );
  }
  if (control === "enum") {
    return (
      <Select
        value={str}
        ariaLabel={label}
        options={(field.enum ?? []).map((v) => ({
          value: v,
          label: field["x-enum-labels"]?.[v] ?? (v || "— none —"),
        }))}
        onChange={set}
      />
    );
  }
  if (control.startsWith("catalog-single:")) {
    const options = catalogOptions(ctx.catalogs, control.slice("catalog-single:".length));
    return (
      <Select
        value={str}
        ariaLabel={label}
        options={[{ value: "", label: "— none —" }, ...options]}
        onChange={set}
      />
    );
  }
  if (control.startsWith("catalog-multi:")) {
    return renderCatalogMulti(ctx, control.slice("catalog-multi:".length), value, set);
  }
  if (control === "connector-secrets") return renderSecrets(props);
  if (control === "toolset") return renderToolset(props);
  if (control === "object-list") return <ListField {...props} />;
  if (control === "tool-ref") {
    const tools = mountedTools(ctx.draft, ctx.catalogs);
    return (
      <Select
        value={str}
        ariaLabel={label}
        options={[{ value: "", label: "all tools" }, ...tools.map((t) => ({ value: t.id, label: t.id }))]}
        onChange={set}
      />
    );
  }
  if (control === "trigger-spec") {
    if (item?.kind === "event") {
      const options = mountedToolsEventOptions(ctx);
      return (
        <Select
          value={str}
          ariaLabel={label}
          options={[{ value: "", label: "choose event" }, ...options]}
          onChange={set}
        />
      );
    }
    return <TextInput value={str} mono ariaLabel={label} placeholder="*/15 * * * *" onChange={set} />;
  }
  return <TextInput value={str} ariaLabel={label} onChange={set} />;
}

function catalogOptions(catalogs: DraftCatalogs, name: string): { value: string; label: string }[] {
  if (name === "connectors") return catalogs.connectors.map((c) => ({ value: c.id, label: c.name }));
  if (name === "skills") return catalogs.skills.map((s) => ({ value: s.id, label: s.id }));
  const list = (catalogs as unknown as Record<string, string[]>)[name] ?? [];
  return list.map((id) => ({ value: id, label: id }));
}

function renderCatalogMulti(
  ctx: Ctx,
  name: string,
  value: unknown,
  set: (value: unknown) => void,
): ReactNode {
  const selected = Array.isArray(value) ? (value as string[]) : [];
  const toggle = (id: string) =>
    set(selected.includes(id) ? selected.filter((s) => s !== id) : [...selected, id]);
  if (name === "connectors") {
    const known = new Set(ctx.catalogs.connectors.map((c) => c.id));
    const extras = selected.filter((id) => !known.has(id));
    return (
      <div className={styles.pillRow}>
        {ctx.catalogs.connectors.map((c) => (
          <Pill key={c.id} selected={selected.includes(c.id)} onClick={() => toggle(c.id)}>
            {c.name} · {c.tools.length} tools
          </Pill>
        ))}
        {extras.map((id) => (
          <Pill key={id} selected onClick={() => toggle(id)}>
            {id}
          </Pill>
        ))}
      </div>
    );
  }
  if (name === "skills") {
    const known = new Set(ctx.catalogs.skills.map((s) => s.id));
    const extras = selected.filter((id) => !known.has(id));
    return (
      <div className={styles.pillRow}>
        {ctx.catalogs.skills.map((s) => (
          <span key={s.id} title={s.description}>
            <Pill selected={selected.includes(s.id)} onClick={() => toggle(s.id)}>
              {s.id}
            </Pill>
          </span>
        ))}
        {extras.map((id) => (
          <Pill key={id} selected onClick={() => toggle(id)}>
            {id}
          </Pill>
        ))}
      </div>
    );
  }
  const options = catalogOptions(ctx.catalogs, name);
  return (
    <div className={styles.pillRow}>
      {options.map((option) => (
        <Pill key={option.value} selected={selected.includes(option.value)} onClick={() => toggle(option.value)}>
          {option.label}
        </Pill>
      ))}
    </div>
  );
}

/** One SecretRef name input per mounted connector (names only, never values). */
function renderSecrets({ value, set, ctx }: FieldProps): ReactNode {
  const secrets = (value as Record<string, string>) ?? {};
  const ids = ctx.draft.connectors;
  if (!ids.length) return <p className={styles.empty}>No connectors mounted.</p>;
  return (
    <div className={styles.list}>
      {ids.map((id) => {
        const name = ctx.catalogs.connectors.find((c) => c.id === id)?.name ?? id;
        const mine = ctx.violations.filter((v) => v.path === `secrets.${id}`);
        return (
          <div key={id} className={`${styles.field} ${mine.length ? styles.invalid : ""}`}>
            <div className={styles.connectorRow}>
              <span className={styles.connectorName}>{name}</span>
              <TextInput
                value={secrets[id] ?? ""}
                mono
                placeholder="rusty:secret:<store>:<key>"
                ariaLabel={`${name} SecretRef name`}
                onChange={(v) => set({ ...secrets, [id]: v })}
              />
            </div>
            <FieldViolations violations={mine} />
          </div>
        );
      })}
    </div>
  );
}

/** The mounted toolset: each tool chip can be wrapped in approval_required. */
function renderToolset({ field, value, set, ctx }: FieldProps): ReactNode {
  const tools = mountedTools(ctx.draft, ctx.catalogs);
  if (!tools.length) {
    return <p className={styles.empty}>{field["x-empty"] ?? "No tools mounted."}</p>;
  }
  const wrapped = Array.isArray(value) ? (value as string[]) : [];
  const toggle = (id: string) =>
    set(wrapped.includes(id) ? wrapped.filter((w) => w !== id) : [...wrapped, id]);
  return (
    <div className={styles.list}>
      {tools.map((tool) => {
        const mine = ctx.violations.filter((v) => v.path === `toolset.${tool.id}`);
        return (
          <div key={tool.id}>
            <div className={styles.toolChip}>
              <span className={styles.toolChipId}>{tool.id}</span>
              <span className={styles.toolChipEffect}>{tool.effect}</span>
              <Pill selected={wrapped.includes(tool.id)} onClick={() => toggle(tool.id)}>
                {wrapped.includes(tool.id) ? "approval_required · org_admins" : "+ wrap"}
              </Pill>
            </div>
            <FieldViolations violations={mine} />
          </div>
        );
      })}
    </div>
  );
}

/** `<connector>.<event>` options from the mounted connectors' catalogs. */
function mountedToolsEventOptions(ctx: Ctx): { value: string; label: string }[] {
  return ctx.draft.connectors.flatMap((id) => {
    const connector = ctx.catalogs.connectors.find((c) => c.id === id);
    return (connector?.events ?? []).map((event) => ({
      value: `${id}.${event}`,
      label: `${id}.${event}`,
    }));
  });
}

/** A repeating group driven by the item schema: add/remove rows, nested fields. */
function ListField({ path, field, value, set, ctx }: FieldProps) {
  const items = Array.isArray(value) ? (value as Record<string, unknown>[]) : [];
  const itemSchema = field.items ?? { properties: {} };
  const setItem = (i: number, key: string, v: unknown) =>
    set(items.map((item, j) => (j === i ? { ...item, [key]: v } : item)));
  return (
    <div className={styles.list}>
      {items.length === 0 && field["x-empty"] && <p className={styles.empty}>{field["x-empty"]}</p>}
      {items.map((item, i) => (
        <div key={i} className={styles.listItem}>
          <div className={styles.listItemHeader}>
            <Button variant="ghost" small ariaLabel={`Remove ${path} ${i + 1}`} onClick={() => set(items.filter((_, j) => j !== i))}>
              ×
            </Button>
          </div>
          {Object.entries(itemSchema.properties ?? {}).map(([key, child]) => (
            <Field
              key={key}
              path={`${path}[${i}].${key}`}
              field={child}
              value={item[key]}
              set={(v) => setItem(i, key, v)}
              ctx={ctx}
              item={item}
            />
          ))}
        </div>
      ))}
      <div>
        <Button variant="secondary" small onClick={() => set([...items, blankItem(itemSchema)])}>
          {field["x-add-label"] ?? "+ Add"}
        </Button>
      </div>
    </div>
  );
}

/** A blank list item from the item schema: enum → first variant, number → 0. */
function blankItem(itemSchema: FieldSchema): Record<string, unknown> {
  const item: Record<string, unknown> = {};
  for (const key of itemSchema.required ?? []) {
    const child = itemSchema.properties?.[key];
    if (!child) continue;
    if (child.enum) item[key] = child.enum[0];
    else if (child.type === "integer" || child.type === "number") item[key] = 0;
    else if (child.type === "boolean") item[key] = false;
    else item[key] = "";
  }
  return item;
}
