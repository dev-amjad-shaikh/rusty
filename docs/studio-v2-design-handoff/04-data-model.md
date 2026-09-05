# 04 · Data model

## AgentDraft (client) ≡ Blueprint document (server, `contracts:blueprint`)
```ts
interface AgentDraft {
  source: 'guided'|'compose'|'chat'|'import'|'improve'|'test';
  agentId?: string;            // set when editing a published blueprint
  base?: AgentDraft & { version: number }; // snapshot for diff
  template?: string | null;
  // identity
  name: string; description: string; model: string;   // model_ref, prefix-routed
  autonomy: 'read_only'|'supervised'|'full';
  // goal
  goal: string;
  measures: { name: string; source: 'outcome'|'connector'|'eval'; target: string; window: string; kind: 'target'|'gate'|'guardrail' }[];
  // directive (frozen 3 tiers; volatile is platform-generated)
  stable: string; context: string;
  // tools
  connectors: string[];        // RegistryRef ids; tools derive from connectors
  wrapped: string[];           // tool ids wrapped in approval_required(org_admins)
  rules: { tool: string /* '' = all */; rule: string }[];
  secrets: Record<connectorId, SecretRefName>;   // 'rusty:secret:<store>:<key>' — names only
  // skills
  skills: string[];            // skill ids; index shows Promoted only at runtime
  // memory
  memory: { label: string; description: string; limit: number; scope: 'agent'|'user' }[];
  // channels & triggers
  channelKind: ''|'slack'|'teams'|'email'|'web'; channelTarget: string;
  triggers: { kind: 'cron'|'event'; spec: string; prompt: string }[];
  // learning
  reviewFork: boolean; cadence: string /* cron */; gateSuite: string;
}
```

## Spec-file layout (Compose) — Markdown + YAML frontmatter
```
agent.md               ---\nname, description, model, autonomy, channel, connectors: [...], skills: [...], gate\n---\n# Name\n\ndescription
goal.md                # Goal / ## Measures (- name · source · target · window  # kind)
directive/stable.md    plain markdown (stable tier)
directive/context.md   plain markdown (context tier)
rules.md               - <tool|*>: rule
triggers.md            ## cron <spec> / ## event <connector.event>, queue: followup, seed prompt
toolsets.md            ## Connector, secret: <SecretRef|<unbound>>, - tool.id  # Effect  → approval_required(org_admins)
memory.md              ## label, limit, scope, description
learning.md            review_fork, consolidation, hunting, promotion_gate
assembled-prompt.txt   read-only, generated
```
Parsing rule: agent.md frontmatter is authoritative for identity/connectors/skills/gate/channel; directive files for tiers. Unknown connector/skill ids are dropped (catalog decides).

## Assembled prompt (read-only projection)
- **stable**: `## Goal` + goal + “You are measured on: name target (window); …” → stable text → `## Tool rules` list.
- **context**: context text.
- **volatile**: `## Skills` (Promoted only: “- id: description”), `## Memory` (“- label (limit): description”), `## Now` timestamp · channel.
Byte count displayed. Never editable; the prompt is frozen per session.

## Validation rules (identical set for save / publish / import / load)
| path | kind | rule |
|---|---|---|
| identity.name | schema | required |
| directive.stable | schema | non-empty |
| goal.statement | schema | required |
| goal.measures | coherence | ≥ 1 measure |
| goal.measures[i] | schema | name and target required |
| autonomy + toolsets.<tool> | coherence | read_only with any Write/Execute/Egress tool → violation on autonomy and on each tool |
| connectors.secret | slot | each mounted connector needs a SecretRef name |
| channels[0] | slot | connectors mounted ⇒ a channel or a trigger; channel kind ⇒ target non-empty |
| memory[i].description | coherence | non-empty (load-bearing prompt content) |
| memory[i].label | coherence | unique |
| tool_rules[i] | coherence/schema | referenced tool must be mounted; rule text non-empty |
| triggers[i].spec | schema/coherence | cron = 5 fields; event chosen; event source must be a mounted connector |
| triggers[i].prompt | schema | seed prompt required |
| learning.promotion_gate | coherence | must name an eval suite |
All violations returned together; each anchored to a control and to a spec file.

## Publish preconditions
0 violations ∧ eval gate = Passing for the current draft content ∧ caller has `blueprints:<id>:publish`. Governance-significant diffs (autonomy, approval wrappers, triggers) are flagged and confirmation is logged. Publishing appends an immutable version; if sessions exist, a fleet-upgrade operation is offered (adoption at turn boundaries; paused sessions adopt after resume; failures stay on the prior version).

## Other entities the UI renders (read-only projections)
Task {id, title, description, agentId, status: todo|in_progress|in_review|done|failed, priority, source, acceptance, attempts[{state, meta, reason, sessionId}]} · Approval obligation {id, agent, session, tool, args, why, effect, egress, waited, expires, sticky_allowed} · Decided approval {decision, chain: decided_by, accountable, level, receipt, version} · Memory block {label, scope, limit, used, description, content, lastWrite, recall, consolidation, origin} · Skill ledger entry {mutation: Create|Patch|AddReference|Promote|Demote|Archive|Restore|Rollback, actor, note, ts, content_hash} · Gap {id, statement, origin, status: Open|Hunting|Parked|Closed|Dismissed, priority, evidence, meta} · Eval suite {id, agentId, gates, versions[], cases[{name, expect, results[]}]} · Egress rule {dest, method, path, binary, action} · Secret ref {ref, store, dependents, probe} · Audit receipt {time, tool, principal, session, hmacVerified} · Channel adapter {id, name, status, queue, capabilities, schema fields} · Package manifest {id, kind, version, publisher, capabilities[], doctor{config_repairs[], state_migrations[]}, files[], signature}.
