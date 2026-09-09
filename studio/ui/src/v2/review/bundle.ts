// The .rustyprint export bundle (R-A8): the spec-file layout from handoff 04
// plus the read-only assembled prompt. Secret values never cross this
// boundary — the bundle carries SecretRef names only, and the scan is
// asserted before anything is offered for download.

import type { AgentDraft } from "../draft/agent-draft.gen";
import { EMPTY_CATALOGS, type DraftCatalogs } from "../draft/catalogs";
import { assemblePrompt } from "./prompt";

export interface BundleFile {
  path: string;
  content: string;
}

export interface SecretScanFinding {
  path: string;
  line: number;
  detail: string;
}

export interface ExportBundle {
  files: BundleFile[];
  scan: { ok: boolean; findings: SecretScanFinding[] };
}

const SECRET_REF_PATTERN = /^rusty:secret:[^\s:]+:[^\s:]+$/;

function frontmatter(entries: [string, string | string[]][]): string {
  const lines = entries.map(([key, value]) =>
    Array.isArray(value) ? `${key}: [${value.join(", ")}]` : `${key}: ${value}`,
  );
  return `---\n${lines.join("\n")}\n---\n`;
}

function agentMd(draft: AgentDraft): string {
  const channel = draft.channelKind === "" ? "" : `${draft.channelKind}:${draft.channelTarget}`;
  return (
    frontmatter([
      ["name", draft.name],
      ["description", draft.description],
      ["model", draft.model],
      ["autonomy", draft.autonomy],
      ["channel", channel],
      ["connectors", draft.connectors],
      ["skills", draft.skills],
      ["gate", draft.gateSuite],
    ]) + `# ${draft.name}\n\n${draft.description}\n`
  );
}

function goalMd(draft: AgentDraft): string {
  const measures = draft.measures
    .map((m) => `- ${m.name} · ${m.source} · ${m.target} · ${m.window}  # ${m.kind}`)
    .join("\n");
  return `# Goal\n\n${draft.goal}\n\n## Measures\n${measures}\n`;
}

function rulesMd(draft: AgentDraft): string {
  return draft.rules.map((rule) => `- ${rule.tool === "" ? "*" : rule.tool}: ${rule.rule}`).join("\n") + "\n";
}

function triggersMd(draft: AgentDraft): string {
  return (
    draft.triggers
      .map((trigger) => `## ${trigger.kind} ${trigger.spec}\nqueue: followup\n\n${trigger.prompt}\n`)
      .join("\n")
  );
}

function toolsetsMd(draft: AgentDraft, catalogs: DraftCatalogs): string {
  return draft.connectors
    .map((id) => {
      const connector = catalogs.connectors.find((c) => c.id === id);
      const secret = draft.secrets[id]?.trim() || "<unbound>";
      const lines = (connector?.tools ?? [])
        .map((tool) => {
          const wrapped = draft.wrapped.includes(tool.id) ? "  → approval_required(org_admins)" : "";
          return `- ${tool.id}  # ${tool.effect}${wrapped}`;
        })
        .join("\n");
      return `## ${connector?.name ?? id}\nsecret: ${secret}\n${lines}\n`;
    })
    .join("\n");
}

function memoryMd(draft: AgentDraft): string {
  return draft.memory
    .map((block) => `## ${block.label}\nlimit: ${block.limit}\nscope: ${block.scope}\n\n${block.description}\n`)
    .join("\n");
}

function learningMd(draft: AgentDraft): string {
  return [
    `review_fork: ${draft.reviewFork ? "on" : "off"}`,
    `consolidation: ${draft.cadence}`,
    "hunting: on",
    `promotion_gate: ${draft.gateSuite}`,
    "",
  ].join("\n");
}

/**
 * Assert the bundle carries no secret values: every `secret:` line holds a
 * SecretRef name (`rusty:secret:<store>:<key>`) or the `<unbound>` slot
 * marker, and no file embeds key material.
 */
export function scanForSecretValues(files: BundleFile[]): SecretScanFinding[] {
  const findings: SecretScanFinding[] = [];
  for (const file of files) {
    file.content.split("\n").forEach((text, index) => {
      const secretLine = /^secret:\s*(.+)$/.exec(text);
      if (secretLine) {
        const value = secretLine[1].trim();
        if (value !== "<unbound>" && !SECRET_REF_PATTERN.test(value)) {
          findings.push({
            path: file.path,
            line: index + 1,
            detail: "secret line carries a value, not a SecretRef name",
          });
        }
      }
      if (text.includes("-----BEGIN") && text.includes("PRIVATE KEY")) {
        findings.push({ path: file.path, line: index + 1, detail: "file embeds private key material" });
      }
    });
  }
  return findings;
}

/** Build the bundle and assert the no-secret-values scan over it. */
export function buildExportBundle(
  draft: AgentDraft,
  catalogs: DraftCatalogs = EMPTY_CATALOGS,
  now?: string,
): ExportBundle {
  const files: BundleFile[] = [
    { path: "agent.md", content: agentMd(draft) },
    { path: "goal.md", content: goalMd(draft) },
    { path: "directive/stable.md", content: draft.stable },
    { path: "directive/context.md", content: draft.context },
    { path: "rules.md", content: rulesMd(draft) },
    { path: "triggers.md", content: triggersMd(draft) },
    { path: "toolsets.md", content: toolsetsMd(draft, catalogs) },
    { path: "memory.md", content: memoryMd(draft) },
    { path: "learning.md", content: learningMd(draft) },
    { path: "assembled-prompt.txt", content: assemblePrompt(draft, catalogs, now).text },
  ];
  const findings = scanForSecretValues(files);
  return { files, scan: { ok: findings.length === 0, findings } };
}
