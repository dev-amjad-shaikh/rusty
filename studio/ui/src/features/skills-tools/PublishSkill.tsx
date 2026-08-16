import { useMemo, useState, type FormEvent } from "react";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { connectionScope, StudioApiError, type ConnectionIdentity } from "../../lib/api/client";
import { publishSkill, SkillScanDenied, type PublishReceipt, type ScanFinding } from "../../lib/api/skills";
import { isUnicodeScalarString } from "../../lib/text";
import { rememberPublishedMembers } from "./publishedMembers";
import styles from "./SkillsPage.module.css";

export interface SkillFrontmatterDraft {
  name: string;
  description: string;
  hasBlock: boolean;
  bodyBytes: number;
  issues: string[];
}

export function parseSkillFrontmatter(text: string): SkillFrontmatterDraft {
  const issues: string[] = [];
  let name = "";
  let description = "";
  let bodyBytes = 0;
  const hasBlock = text.startsWith("---\n") || text.startsWith("---\r\n");
  if (!hasBlock) {
    issues.push("Open the package with a frontmatter block: a line containing only ---.");
    return { name, description, hasBlock, bodyBytes, issues };
  }
  const lines = text.split(/\r?\n/);
  const close = lines.findIndex((line, index) => index > 0 && line.trim() === "---");
  if (close === -1) {
    issues.push("Close the frontmatter block with a second line containing only ---.");
    return { name, description, hasBlock, bodyBytes, issues };
  }
  for (const line of lines.slice(1, close)) {
    if (!line.trim() || line.startsWith("#")) continue;
    const separator = line.indexOf(":");
    if (separator === -1) { issues.push(`Frontmatter line "${line.slice(0, 40)}" is not a key: value pair.`); continue; }
    const key = line.slice(0, separator).trim();
    let value = line.slice(separator + 1).trim();
    if (value.length >= 2 && ((value.startsWith('"') && value.endsWith('"')) || (value.startsWith("'") && value.endsWith("'")))) {
      value = value.slice(1, -1);
    }
    if (key === "name") name = value;
    if (key === "description") description = value;
  }
  bodyBytes = new TextEncoder().encode(lines.slice(close + 1).join("\n").trim()).byteLength;
  if (!name) issues.push("Declare name: in the frontmatter.");
  else if (!/^[a-z0-9]+(?:-[a-z0-9]+)*$/.test(name)) issues.push("The name must be kebab-case: lowercase letters, digits, and single dashes.");
  if (!description) issues.push("Declare description: — it is the discovery text agents read first.");
  if (!bodyBytes) issues.push("Write the instruction body below the frontmatter.");
  return { name, description, hasBlock, bodyBytes, issues };
}

export function memberPathProblem(path: string): string {
  if (!path) return "Member path is required.";
  if (new TextEncoder().encode(path).byteLength > 512) return "Member paths stay under 512 bytes.";
  if (!isUnicodeScalarString(path) || /[\u0000-\u001f\u007f]/.test(path)) return "Member paths cannot contain control characters.";
  if (path.includes("\\")) return "Use forward slashes in member paths.";
  if (path.startsWith("/")) return "Member paths are relative — no leading slash.";
  const segments = path.split("/");
  if (segments.some((segment) => !segment || segment === "." || segment === "..")) return "Member paths cannot contain empty, ., or .. segments.";
  return "";
}

const findingKindLabel = { embedded_script: "Embedded script", credentialed_url: "Credentialed URL", base64_blob: "Base64 blob" } as const;

interface MemberRow {
  id: string;
  kind: "reference" | "asset";
  path: string;
  content: string;
}

let rowId = 0;

export function PublishSkill({ connection, scope, onCancel, onPublished }: {
  connection: ConnectionIdentity;
  scope: string;
  onCancel: () => void;
  onPublished: (name: string) => void;
}) {
  const queryClient = useQueryClient();
  const [skillMd, setSkillMd] = useState("---\nname: \ndescription: \n---\n\n");
  const [author, setAuthor] = useState("");
  const [members, setMembers] = useState<MemberRow[]>([]);
  const [error, setError] = useState("");
  const [uncertain, setUncertain] = useState(false);
  const [findings, setFindings] = useState<ScanFinding[]>([]);

  const parsed = useMemo(() => parseSkillFrontmatter(skillMd), [skillMd]);
  const authorClean = author.trim();
  const authorProblem = !authorClean ? "Name who publishes this package — provenance is mandatory."
    : !isUnicodeScalarString(authorClean) || /[\u0000-\u001f\u007f]/.test(authorClean) ? "The author identity cannot contain control characters."
    : new TextEncoder().encode(authorClean).byteLength > 256 ? "Keep the author identity under 256 bytes." : "";
  const memberProblems = members.map((member) => memberPathProblem(member.path.trim()));

  const publish = useMutation({
    mutationFn: async () => {
      const references: Record<string, string> = {};
      const assets: Record<string, string> = {};
      for (const member of members) {
        const target = member.kind === "reference" ? references : assets;
        target[member.path.trim()] = member.content;
      }
      return publishSkill(connection, { skillMd, references, assets, author: authorClean }, parsed.name);
    },
    onSuccess: async (receipt: PublishReceipt) => {
      const current = connectionScope(connection);
      rememberPublishedMembers(current, receipt.name, members.map((member) => `${member.kind === "reference" ? "references" : "assets"}/${member.path.trim()}`));
      await queryClient.invalidateQueries({ queryKey: [connection.epoch, connection.origin, connection.tenantFingerprint, "skills"] });
      onPublished(receipt.name);
    },
    onError: (caught) => {
      if (caught instanceof SkillScanDenied) {
        setFindings(caught.findings);
        setError(caught.message);
        return;
      }
      setFindings([]);
      setError(caught instanceof Error ? caught.message : "The skill could not be published.");
      if (caught instanceof StudioApiError && caught.mayHaveCommitted) setUncertain(true);
    },
  });

  function submit(event: FormEvent) {
    event.preventDefault();
    setError("");
    setFindings([]);
    if (parsed.issues.length) { setError(parsed.issues[0]); return; }
    if (authorProblem) { setError(authorProblem); return; }
    const firstMemberProblem = memberProblems.find(Boolean);
    if (firstMemberProblem) { setError(firstMemberProblem); return; }
    const seen = new Set<string>();
    for (const member of members) {
      const key = `${member.kind}:${member.path.trim()}`;
      if (seen.has(key)) { setError(`Member ${member.path.trim()} is declared twice as a ${member.kind}.`); return; }
      seen.add(key);
      if (member.kind === "asset" && !member.content) { setError(`Asset ${member.path.trim()} has no content.`); return; }
    }
    publish.mutate();
  }

  function updateMember(id: string, patch: Partial<MemberRow>) {
    setMembers((rows) => rows.map((row) => row.id === id ? { ...row, ...patch } : row));
  }

  return (
    <div className={styles.publishLayout}>
      <form className={styles.publishForm} onSubmit={submit} aria-label="Publish a skill package">
        <label>
          SKILL.md
          <textarea
            rows={16}
            value={skillMd}
            onChange={(event) => { setSkillMd(event.target.value); setError(""); setFindings([]); }}
            placeholder={"---\nname: web-research\ndescription: Search, then summarize with citations.\n---\n\n## When to use\n…"}
            aria-describedby="frontmatter-hints"
          />
        </label>
        <label>
          Author
          <input value={author} onChange={(event) => { setAuthor(event.target.value); setError(""); }} placeholder="operator:ada" />
        </label>

        {members.map((member, index) => (
          <div className={styles.memberRow} key={member.id}>
            <label>Kind
              <select value={member.kind} onChange={(event) => updateMember(member.id, { kind: event.target.value as MemberRow["kind"] })}>
                <option value="reference">Reference</option>
                <option value="asset">Asset</option>
              </select>
            </label>
            <label>Path
              <input value={member.path} onChange={(event) => updateMember(member.id, { path: event.target.value })} placeholder={member.kind === "reference" ? "guide.md" : "logo.bin"} aria-invalid={Boolean(memberProblems[index])} />
            </label>
            <button className={styles.removeMember} type="button" aria-label={`Remove member ${index + 1}`} onClick={() => setMembers((rows) => rows.filter((row) => row.id !== member.id))}>×</button>
            <textarea
              rows={3}
              value={member.content}
              onChange={(event) => updateMember(member.id, { content: event.target.value })}
              placeholder={member.kind === "reference" ? "Markdown loaded on demand by the skill." : "Asset bytes (text here is encoded for you)."}
              aria-label={`Content for ${member.path.trim() || `member ${index + 1}`}`}
            />
            {memberProblems[index] && <p className={styles.error} role="alert">{memberProblems[index]}</p>}
          </div>
        ))}
        <button className={styles.addMember} type="button" onClick={() => setMembers((rows) => [...rows, { id: `member-${++rowId}`, kind: "reference", path: "", content: "" }])}>Add a reference or asset</button>

        {findings.length > 0 && (
          <div className={styles.error} role="alert">
            <p><b>The security scan denied this package.</b> Nothing was registered; your draft is unchanged.</p>
            <ul className={styles.findingList}>
              {findings.map((finding, index) => (
                <li key={`${finding.location}-${finding.kind}-${index}`}>
                  <b>{findingKindLabel[finding.kind]} · denial</b>
                  <code>{finding.location}</code>
                  <span>{finding.detail}</span>
                </li>
              ))}
            </ul>
          </div>
        )}
        {error && !findings.length && <p className={styles.error} role="alert">{error}{uncertain ? " Rusty may have registered this package — check the library before publishing again." : ""}</p>}

        <div className={styles.formActions}>
          <span>Publishing registers one immutable, content-addressed revision. Identical content re-registers idempotently.</span>
          <button className="primary-button" type="submit" disabled={publish.isPending}>{publish.isPending ? "Publishing…" : "Publish skill"}</button>
        </div>
      </form>

      <aside className={styles.publishAside} aria-label="Frontmatter review">
        <span className="eyebrow">Live frontmatter review</span>
        <h2>{parsed.name || "Unnamed skill"}</h2>
        <ul className={styles.hintList} id="frontmatter-hints" aria-live="polite">
          <li data-ok={Boolean(parsed.name && /^[a-z0-9]+(?:-[a-z0-9]+)*$/.test(parsed.name))}><i>{parsed.name ? "✓" : "•"}</i>name {parsed.name ? `· ${parsed.name}` : "is required"}</li>
          <li data-ok={Boolean(parsed.description)}><i>{parsed.description ? "✓" : "•"}</i>description {parsed.description ? "declared" : "is required"}</li>
          <li data-ok={parsed.bodyBytes > 0}><i>{parsed.bodyBytes ? "✓" : "•"}</i>body {parsed.bodyBytes ? `· ${parsed.bodyBytes.toLocaleString()} bytes` : "is empty"}</li>
          <li data-ok={!authorProblem}><i>{!authorProblem ? "✓" : "•"}</i>author {!authorProblem ? `· ${authorClean}` : "is required"}</li>
        </ul>
        {parsed.issues.length > 0 && <p className={styles.error}>{parsed.issues[0]}</p>}
        <button className="secondary-button" type="button" onClick={onCancel}>Cancel</button>
      </aside>
    </div>
  );
}
