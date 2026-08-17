import { useMemo, useState } from "react";
import { useQueries, useQuery } from "@tanstack/react-query";
import { getSkill, listSkills, type SkillDetail, type SkillMetadata } from "../../lib/api/skills";
import { evidencePreview } from "../../lib/text";
import type { InfoGraph, ToolCapability } from "../../lib/contracts";
import { useRuntimeStore } from "../../state/runtime";
import { PageHeader } from "../../components/PageHeader";
import { SkillDetailDrawer } from "./SkillDetailDrawer";
import { PublishSkill } from "./PublishSkill";
import styles from "./SkillsPage.module.css";

type View = "skills" | "tools";

export function SkillsPage() {
  const info = useRuntimeStore((state) => state.info);
  const [view, setView] = useState<View>("skills");
  const [filter, setFilter] = useState("");
  const [selected, setSelected] = useState("");
  const [publishing, setPublishing] = useState(false);
  const key = ["skills"];
  const catalog = useQuery({ queryKey: key, queryFn: () => listSkills() });
  const skills = useMemo(() => [...(catalog.data ?? [])].sort((a, b) => a.name.localeCompare(b.name)), [catalog.data]);

  const receipts = useQueries({
    queries: skills.slice(0, 100).map((skill) => ({
      queryKey: [...key, "receipt", skill.name],
      queryFn: () => getSkill(skill.name),
      retry: false,
      staleTime: 60_000,
    })),
  });
  const receiptByName = useMemo(() => {
    const map = new Map<string, { receipt: SkillDetail | null; pending: boolean }>();
    skills.slice(0, 100).forEach((skill, index) => {
      const query = receipts[index];
      map.set(skill.name, { receipt: query?.data ?? null, pending: query?.isLoading ?? false });
    });
    return map;
  }, [receipts, skills]);

  const graphs = info?.graphs ?? [];
  const toolCount = graphs.reduce((sum, graph) => sum + (graph.tools?.length ?? 0), 0);
  const summary = catalog.isLoading ? "Loading this workspace…"
    : catalog.isError ? "Skill count unavailable"
    : `${skills.length} skill${skills.length === 1 ? "" : "s"} published · ${toolCount} tool${toolCount === 1 ? "" : "s"} included by behaviors`;

  const visible = useMemo(() => {
    const needle = filter.trim().toLowerCase();
    if (!needle) return skills;
    return skills.filter((skill) => skill.name.toLowerCase().includes(needle) || skill.description.toLowerCase().includes(needle));
  }, [filter, skills]);

  return (
    <section className={`page ${styles.skillsPage}`} aria-labelledby="skills-heading">
      <PageHeader
        headingId="skills-heading"
        eyebrow="Build"
        title="Skills & Tools"
        description={summary}
        detail={catalog.data ? <span className={styles.registryBadge}>registry · {skills.length} admitted</span> : undefined}
        actions={<div className={styles.headerActions}>
          <div className={styles.viewToggle} role="group" aria-label="Skills and tools view">
            <button type="button" aria-pressed={view === "skills"} onClick={() => setView("skills")}>Skills</button>
            <button type="button" aria-pressed={view === "tools"} onClick={() => setView("tools")}>Tools</button>
          </div>
          {!publishing && <button className="primary-button" type="button" onClick={() => setPublishing(true)}>Publish skill</button>}
          {publishing && <button className="secondary-button" type="button" onClick={() => setPublishing(false)}>Back to library</button>}
        </div>}
      />

      {publishing ? (
        <PublishSkill
          onCancel={() => setPublishing(false)}
          onPublished={(name) => { setPublishing(false); setSelected(name); setView("skills"); }}
        />
      ) : view === "tools" ? (
        <ToolsCatalog graphs={graphs} />
      ) : catalog.isLoading ? (
        <div className={styles.loading} role="status">Loading skills…</div>
      ) : catalog.isError ? (
        <div className="empty-state" role="alert">
          <span className="eyebrow">Skill registry</span>
          <h2>Skills could not be loaded</h2>
          <p>{catalog.error instanceof Error ? catalog.error.message : "Try the request again."}</p>
          <button className="primary-button" type="button" onClick={() => catalog.refetch()}>Retry</button>
        </div>
      ) : skills.length === 0 ? (
        <div className="empty-state">
          <span className="eyebrow">Skill registry</span>
          <h2>No skills published yet</h2>
          <p>A skill package is one reviewed SKILL.md with its references and assets, scanned and versioned under an immutable content hash.</p>
          <button className="primary-button" type="button" onClick={() => setPublishing(true)}>Publish first skill</button>
        </div>
      ) : (
        <>
          <div className={styles.toolbar}>
            <label>Filter skills
              <input type="search" value={filter} onChange={(event) => setFilter(event.target.value)} placeholder="Name or description" />
            </label>
          </div>
          {visible.length === 0 ? (
            <div className="empty-state">
              <span className="eyebrow">Filter</span>
              <h2>No skills match this filter</h2>
              <p>Nothing in this workspace's registry matches "{evidencePreview(filter, 120)}".</p>
              <button className="secondary-button" type="button" onClick={() => setFilter("")}>Clear filter</button>
            </div>
          ) : (
            <SkillTable skills={visible} receiptByName={receiptByName} onOpen={setSelected} />
          )}
        </>
      )}

      {selected && !publishing && (
        <SkillDetailDrawer name={selected} onClose={() => setSelected("")} />
      )}
    </section>
  );
}

function SkillTable({ skills, receiptByName, onOpen }: {
  skills: SkillMetadata[];
  receiptByName: Map<string, { receipt: SkillDetail | null; pending: boolean }>;
  onOpen: (name: string) => void;
}) {
  return (
    <div className={styles.skillTable} role="table" aria-label="Skill library">
      <div className={styles.tableHead} role="row">
        <span role="columnheader">Skill</span>
        <span role="columnheader">Revision</span>
        <span role="columnheader">Content hash</span>
        <span role="columnheader">Published by</span>
        <span role="columnheader">Scan</span>
        <span role="columnheader"><span className="sr-only">Open</span></span>
      </div>
      {skills.map((skill) => {
        const entry = receiptByName.get(skill.name);
        const receipt = entry?.receipt ?? null;
        return (
          <article className={styles.skillRow} role="row" key={skill.name}>
            <div className={styles.skillIdentity} role="cell">
              <b title={skill.name}>{skill.name}</b>
              <small>{evidencePreview(skill.description, 160)}</small>
            </div>
            <span role="cell" data-label="Revision">r{skill.revision}</span>
            <code role="cell" data-label="Content hash" title={skill.content_hash}>{skill.content_hash.slice(0, 12)}</code>
            <span className={styles.provenance} role="cell" data-label="Published by">
              {entry?.pending ? "…" : receipt ? evidencePreview(receipt.provenance.author, 80) : "Unavailable"}
              {receipt && <small>{sourceLabel(receipt)}</small>}
            </span>
            <span role="cell" data-label="Scan">
              {entry?.pending ? <span className={styles.scanPending}>checking</span>
                : receipt ? receipt.scan.clean
                  ? <span className={styles.scanClean}>clean</span>
                  : <span className={styles.scanWarn}>{receipt.scan.warning_count} warning{receipt.scan.warning_count === 1 ? "" : "s"}</span>
                : <span className={styles.scanPending}>unavailable</span>}
            </span>
            <div role="cell">
              <button className={styles.openSkill} type="button" onClick={() => onOpen(skill.name)} aria-label={`Open ${skill.name}`}>Open <span aria-hidden="true">→</span></button>
            </div>
          </article>
        );
      })}
      <footer>{skills.length} skill{skills.length === 1 ? "" : "s"}</footer>
    </div>
  );
}

function sourceLabel(receipt: SkillDetail) {
  const source = receipt.provenance.source;
  return source.type === "registry" ? `registry:${source.name}` : `local:${source.path}`;
}

function effectLabel(effect: ToolCapability["effect"]) {
  return ({ pure: "Pure", read_only: "Read only", idempotent: "Idempotent", compensatable: "Compensatable", non_idempotent: "Non-idempotent" } as const)[effect];
}

function humanize(value: string) {
  return value.replaceAll("_", " ").replaceAll("-", " ").replace(/\b\w/g, (letter) => letter.toUpperCase());
}

function ToolsCatalog({ graphs }: { graphs: InfoGraph[] }) {
  if (!graphs.length) {
    return (
      <div className="empty-state">
        <span className="eyebrow">Tool catalog</span>
        <h2>No behaviors are available in this workspace</h2>
        <p>The tool catalog is read from the connected Rusty server; this workspace did not report any behavior graphs.</p>
      </div>
    );
  }
  return (
    <div>
      {graphs.map((graph) => {
        const tools = graph.tools ?? [];
        return (
          <section className={styles.graphBlock} key={graph.name} aria-labelledby={`graph-${graph.name}`}>
            <header>
              <h2 id={`graph-${graph.name}`}>{humanize(graph.name)}</h2>
              <span>{tools.length ? `${tools.length} tool${tools.length === 1 ? "" : "s"} included` : "no tools included"}</span>
            </header>
            {tools.length ? (
              <div role="table" aria-label={`Tools included by ${humanize(graph.name)}`}>
                <div className={styles.toolHead} role="row">
                  <span role="columnheader">Tool</span>
                  <span role="columnheader">Description</span>
                  <span role="columnheader">Effect boundary</span>
                </div>
                {tools.map((tool) => (
                  <div className={styles.toolRow} role="row" key={tool.name}>
                    <code role="cell">{tool.name}</code>
                    <p role="cell">{evidencePreview(tool.description, 240)}</p>
                    <span role="cell"><span className={styles.effectBadge} data-effect={tool.effect}>{effectLabel(tool.effect)}</span></span>
                  </div>
                ))}
              </div>
            ) : (
              <p className={styles.noTools}>This behavior includes no executable tools. Tools listed here are exactly what the behavior admits — nothing is installed dynamically at run time.</p>
            )}
          </section>
        );
      })}
    </div>
  );
}
