import { useState } from "react";
import type { AssistantIntent } from "../../engine/net/client";
import type { AgentDraft } from "./useAgent";

/** The platform's blocks every agent with memory carries; a builder may declare more. */
const DEFAULT_LABELS = ["person", "working", "decisions"];
const labelOk = (l: string) => /^[a-z0-9_-]{1,32}$/.test(l);
type Declared = NonNullable<NonNullable<AssistantIntent["memory"]>["blocks"]>;

/** The blocks this working copy declares beyond the platform's three: label, what it holds, its limit. A block declared here renders in the draft's test runs and is editable in View memory before the draft is published. */
export function DeclaredBlocks({ intent, edit }: { intent: AssistantIntent; edit: (change: (d: AgentDraft) => AgentDraft) => void }) {
  const declared: Declared = intent.memory?.blocks ?? [];
  const [label, setLabel] = useState("");
  const [description, setDescription] = useState("");
  const [limit, setLimit] = useState(1500);
  const taken = new Set([...DEFAULT_LABELS, ...declared.map((b) => b.label)]);
  const canAdd = labelOk(label) && !taken.has(label) && description.trim().length > 0;
  const set = (blocks: Declared) => edit((d) => ({ ...d, intent: { ...d.intent, memory: { ...(d.intent.memory ?? {}), blocks } } }));
  return (
    <div style={{ marginTop: 10 }}>
      {declared.length > 0 && <div className="f-mini-label">Declared by this agent</div>}
      {declared.map((b) => (
        <div key={b.label} className="mem-stat" data-declared-block={b.label} style={{ alignItems: "flex-start" }}>
          <span style={{ fontFamily: "var(--font-mono)" }}>{b.label}</span>
          <b style={{ fontWeight: 500 }}>{b.description || "—"} · {(b.char_limit ?? 1500).toLocaleString()} chars</b>
          <button className="m-btn ghost sm icon" title="Remove this declaration (what the block holds stays in memory)" onClick={() => set(declared.filter((x) => x.label !== b.label))}><i className="ti ti-x" /></button>
        </div>
      ))}
      <div className="f-mini-label" style={{ marginTop: 8 }}>Declare a block</div>
      <div style={{ display: "grid", gridTemplateColumns: "140px 1fr 90px auto", gap: 6, alignItems: "center" }}>
        <input className="m-input" placeholder="label (a-z, 0-9, _ -)" value={label} onChange={(e) => setLabel(e.target.value.trim().toLowerCase())} style={{ fontFamily: "var(--font-mono)" }} data-flow="block-label" />
        <input className="m-input" placeholder="what it holds, in one line" value={description} onChange={(e) => setDescription(e.target.value)} data-flow="block-description" />
        <input className="m-input" type="number" min={100} max={8000} step={100} value={limit} onChange={(e) => setLimit(Math.min(8000, Math.max(100, Number(e.target.value) || 1500)))} title="Character limit" data-flow="block-limit" />
        <button className="m-btn secondary sm" disabled={!canAdd} data-flow="block-declare" onClick={() => { set([...declared, { label, description: description.trim(), char_limit: limit }]); setLabel(""); setDescription(""); setLimit(1500); }}><i className="ti ti-plus" /> Declare</button>
      </div>
      {label && taken.has(label) && <div className="m-hint" style={{ marginTop: 4 }}>{DEFAULT_LABELS.includes(label) ? `${label} is a platform block already on every agent.` : `${label} is declared already.`}</div>}
    </div>
  );
}
