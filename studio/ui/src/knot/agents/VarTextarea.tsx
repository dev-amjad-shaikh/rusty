import { useMemo, useRef, useState, type CSSProperties, type TextareaHTMLAttributes } from "react";

/**
 * A textarea that offers `{{variables}}` as you type: on `{{` a small list of
 * the names already in use opens; ↑↓ pick, Enter or Tab inserts `{{name}}`.
 * The names come from the caller — the agent's charter and its skills.
 */
export function VarTextarea({ value, onValueChange, names, className, style, ...rest }: { value: string; onValueChange: (v: string) => void; names: string[]; className?: string; style?: CSSProperties } & Omit<TextareaHTMLAttributes<HTMLTextAreaElement>, "value" | "onChange" | "style" | "className">) {
  const ref = useRef<HTMLTextAreaElement>(null);
  const [menu, setMenu] = useState<{ at: number; query: string; index: number } | null>(null);
  const known = useMemo(() => [...new Set(names)].sort(), [names]);
  const matches = menu ? known.filter((n) => n.toLowerCase().startsWith(menu.query.toLowerCase())) : [];
  const shown = menu ? (matches.length ? matches : menu.query.length ? [menu.query] : []) : [];
  function scan(text: string, caret: number) {
    // `{{` before the caret with no `}}` after it on the same token opens the list.
    const before = text.slice(0, caret);
    const m = /\{\{\s*([A-Za-z_][\w.]*)?$/.exec(before);
    if (!m) { setMenu(null); return; }
    setMenu({ at: m.index, query: m[1] ?? "", index: 0 });
  }
  function insert(name: string) {
    const el = ref.current; if (!el || !menu) return;
    const caret = el.selectionStart;
    const next = `${value.slice(0, menu.at)}{{${name}}}${value.slice(caret)}`;
    onValueChange(next);
    setMenu(null);
    const pos = menu.at + name.length + 4;
    requestAnimationFrame(() => { el.focus(); el.setSelectionRange(pos, pos); });
  }
  return (
    <div style={{ position: "relative" }}>
      <textarea ref={ref} className={className} style={style} value={value} {...rest}
        onChange={(e) => { onValueChange(e.target.value); scan(e.target.value, e.target.selectionStart); }}
        onKeyDown={(e) => {
          if (!menu || !shown.length) return;
          if (e.key === "ArrowDown") { e.preventDefault(); setMenu({ ...menu, index: (menu.index + 1) % shown.length }); }
          else if (e.key === "ArrowUp") { e.preventDefault(); setMenu({ ...menu, index: (menu.index - 1 + shown.length) % shown.length }); }
          else if (e.key === "Enter" || e.key === "Tab") { e.preventDefault(); insert(shown[menu.index]); }
          else if (e.key === "Escape") { setMenu(null); }
        }}
        onBlur={() => setTimeout(() => setMenu(null), 150)} />
      {menu && shown.length === 0 && (
        <div className="m-menu pop-menu" style={{ position: "absolute", left: 12, top: "100%", marginTop: 4, zIndex: 20, minWidth: 260 }}>
          <div className="cat-label" style={{ margin: "6px 10px 2px" }}><span>Variables</span><span className="ln" /></div>
          <div className="m-menu-item" style={{ color: "var(--ink-500)", cursor: "default" }}><i className="ti ti-variable ic" />None yet — type a name, it becomes one</div>
        </div>
      )}
      {menu && shown.length > 0 && (
        <div className="m-menu pop-menu" style={{ position: "absolute", left: 12, top: "100%", marginTop: 4, zIndex: 20, minWidth: 220 }}>
          <div className="cat-label" style={{ margin: "6px 10px 2px" }}><span>Variables</span><span className="ln" /></div>
          {shown.map((n, i) => <div key={n} className="m-menu-item" style={i === menu.index ? { background: "var(--bg-muted)" } : undefined} onMouseDown={(e) => { e.preventDefault(); insert(n); }}><i className="ti ti-variable ic" /><span className="mono">{`{{${n}}}`}</span>{!known.includes(n) && <span className="item-tag" style={{ marginLeft: "auto" }}>new</span>}</div>)}
        </div>
      )}
    </div>
  );
}
