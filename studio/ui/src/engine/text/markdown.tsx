import type { ReactNode } from "react";

/**
 * The little Markdown an agent writes, rendered as React elements — never
 * HTML: paragraphs, headings (as bold lines), bulleted and numbered lists,
 * fenced and inline code, **bold**, *italic*, and links shown as their
 * text with the address beside it. Anything else is text as written.
 */
/** What a renderer may do with an inline code span: a tool name becomes a
 * chip that says whether it is connected; null keeps it plain code. */
export interface RenderOptions { code?: (text: string) => ReactNode | null }

export function renderMarkdown(text: string, opts: RenderOptions = {}): ReactNode[] {
  const lines = text.replace(/\r\n?/g, "\n").split("\n");
  const out: ReactNode[] = [];
  let i = 0;
  let key = 0;
  const next = () => key++;
  while (i < lines.length) {
    const line = lines[i];
    if (/^\s*```/.test(line)) {
      const body: string[] = [];
      i++;
      while (i < lines.length && !/^\s*```/.test(lines[i])) { body.push(lines[i]); i++; }
      i++;
      out.push(<pre key={next()} className="rn-md__pre">{body.join("\n")}</pre>);
      continue;
    }
    if (/^\s*$/.test(line)) { i++; continue; }
    // A pipe table: a header row, a rule row of dashes, then rows.
    if (/^\s*\|.*\|\s*$/.test(line) && i + 1 < lines.length && /^\s*\|?\s*:?-{2,}/.test(lines[i + 1])) {
      const cells = (l: string) => l.trim().replace(/^\|/, "").replace(/\|$/, "").split("|").map((c) => c.trim());
      const head = cells(line);
      i += 2;
      const rows: string[][] = [];
      while (i < lines.length && /^\s*\|.*\|\s*$/.test(lines[i])) { rows.push(cells(lines[i])); i++; }
      out.push(
        <div key={next()} className="rn-md__table" style={{ overflowX: "auto" }}>
          <table className="m-table"><thead><tr>{head.map((c, n) => <th key={n}>{inline(c, opts)}</th>)}</tr></thead><tbody>{rows.map((r, n) => <tr key={n}>{r.map((c, k) => <td key={k}>{inline(c, opts)}</td>)}</tr>)}</tbody></table>
        </div>,
      );
      continue;
    }
    const h = /^\s{0,3}(#{1,6})\s+(.*)$/.exec(line);
    if (h) { out.push(<p key={next()} className="rn-md__h"><b>{inline(h[2], opts)}</b></p>); i++; continue; }
    const bullet = /^\s*[-*•]\s+(.*)$/;
    const number = /^\s*(\d+)[.)]\s+(.*)$/;
    if (bullet.test(line) || number.test(line)) {
      const ordered = number.test(line);
      const items: ReactNode[] = [];
      while (i < lines.length && (ordered ? number.test(lines[i]) : bullet.test(lines[i]))) {
        const m = ordered ? number.exec(lines[i])! : bullet.exec(lines[i])!;
        const content = ordered ? m[2] : m[1];
        // A wrapped item continues on indented lines.
        const more: string[] = [];
        i++;
        while (i < lines.length && /^\s{2,}\S/.test(lines[i]) && !bullet.test(lines[i]) && !number.test(lines[i])) { more.push(lines[i].trim()); i++; }
        items.push(<li key={next()}>{inline([content, ...more].join(" "), opts)}</li>);
      }
      out.push(ordered ? <ol key={next()} className="rn-md__list">{items}</ol> : <ul key={next()} className="rn-md__list">{items}</ul>);
      continue;
    }
    // A paragraph: consecutive plain lines, kept on their own lines.
    const para: string[] = [];
    while (i < lines.length && !/^\s*$/.test(lines[i]) && !/^\s*```/.test(lines[i]) && !bullet.test(lines[i]) && !number.test(lines[i]) && !/^\s{0,3}#{1,6}\s+/.test(lines[i])) { para.push(lines[i]); i++; }
    out.push(
      <p key={next()} className="rn-md__p">
        {para.map((l, n) => <span key={n}>{n > 0 && <br />}{inline(l, opts)}</span>)}
      </p>,
    );
  }
  return out;
}

/** Inline: `code`, **bold**, *italic*, [text](url). Left to right, no nesting inside code. */
export function inline(text: string, opts: RenderOptions = {}): ReactNode[] {
  const out: ReactNode[] = [];
  const re = /(`[^`]+`)|(\*\*[^*]+\*\*)|(\*[^*\s][^*]*\*)|(\[[^\]]+\]\([^)\s]+\))/g;
  let last = 0;
  let m: RegExpExecArray | null;
  let k = 0;
  while ((m = re.exec(text)) !== null) {
    if (m.index > last) out.push(text.slice(last, m.index));
    const tok = m[0];
    if (tok.startsWith("`")) {
      const custom = opts.code?.(tok.slice(1, -1));
      out.push(custom != null ? <span key={k++}>{custom}</span> : <code key={k++} className="rn-md__code">{tok.slice(1, -1)}</code>);
    }
    else if (tok.startsWith("**")) out.push(<b key={k++}>{tok.slice(2, -2)}</b>);
    else if (tok.startsWith("[")) {
      const lm = /^\[([^\]]+)\]\(([^)\s]+)\)$/.exec(tok)!;
      const safe = /^https?:\/\//i.test(lm[2]);
      out.push(safe ? <a key={k++} href={lm[2]} target="_blank" rel="noreferrer noopener">{lm[1]}</a> : <span key={k++}>{lm[1]} ({lm[2]})</span>);
    } else out.push(<i key={k++}>{tok.slice(1, -1)}</i>);
    last = m.index + tok.length;
  }
  if (last < text.length) out.push(text.slice(last));
  return out;
}
