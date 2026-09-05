# 03 · Screens

All screens live inside the main card. Widths are fluid; specifics below are the design’s reference values.

## Agents (home)
- Hero h1 28px/500 centered “What agent do you want to build?” on a dotted background (`radial-gradient(var(--line) 1px, transparent 1px)` 18px grid).
- Four entry cards (grid auto-fit ≥210px): **Guided** “Start from a template and fill its slots.” · **Compose** “Write the Rustyprint as Markdown files with live validation.” · **Chat** “Describe the agent; the builder fills the draft.” · **Import** “Bring an agent from Claude Code, Hermes, Letta, or OpenClaw.” Card: panel, line, radius 14, 18px padding, title 16/600, body 13 ink2; hover border ink3 + bg.
- **Published** table: header caps 11px; columns `minmax(130px,2fr) 36px 84px minmax(80px,1.4fr) 158px`, gap 8. Cells: name (600) + second line “channel · description” (12 ink3, ellipsis) · vN mono · autonomy (11/700 colored: read_only ok, supervised warn, full err) · connectors (12 ink2, ellipsis) · sessions count + Edit (secondary, small) + Upgrade (warn outline) on one line, right-aligned.
- **Fleet upgrade** panel (appears after publishing a new version of an agent with sessions, or via Upgrade): header “Fleet upgrade · {agent} vN → vN+1 · summary”, Start upgrade / ×; grid of session chips (id mono, state: awaiting next turn / mid-turn / paused · awaiting resume / adopted / failed) with colored dot; footnote about turn-boundary adoption.

## Drafts
Grid of cards (auto-fill ≥260px): name + source pill (guided/compose/chat/import/improve), description or goal, connector pills, validation label (“Passing” ok / “N open violations” warn), Discard (secondary) + Resume (primary). Empty state: “No drafts. Start one from Agents.”

## Guided
Header: ← Agents, h1 “Guided setup”, step pills 1 Template · 2 Configure · 3 Autonomy (active = accent border/soft fill).
1. **Template**: “Start blank” dashed card first, then template cards (name, tpl vN mono, blurb, connector pills, “N slots · N skills”).
2. **Configure**: template → slot cards (grid `24px 1fr`): badge ✓/·, title, JSON path mono, help, control (text input mono / choice pills), probe line. Slots: Agent name, Goal, Channel kind, Channel target, one credential per connector (SecretRef name only; shows wire-probe status), Model. Blank → the full **AgentDraftForm** plus credential slots for mounted connectors. Right aside: Validation (live list of violations with kind dot, path mono, message) and Template summary.
3. **Autonomy**: three cards read_only / supervised / full with description + coherence note (read_only shows “Conflicts with N mounted Write/Egress tools” in err when applicable). Back / Review.

## Compose (spec editor)
Grid `minmax(130px,170px) minmax(0,1fr)`.
- File tree (bg surface): ←, agent name; files in mono 12px: agent.md, goal.md, directive/ (stable.md, context.md), rules.md, triggers.md, toolsets.md, memory.md, learning.md, assembled-prompt.txt (read-only); active = code bg; dirty dot accent; “+ New file” dashed.
- Editor column: **tab strip** (40px, bg surface): active tab (panel bg, 2px top border ink, dirty dot, filename mono, READ-ONLY badge when applicable), breadcrumb “{agent} / {path}” 12px ink3, Review primary small. **Editor**: scroll container; grid `52px 1fr`; gutter (bg surface, right hairline, line numbers mono 12 right-aligned, current line ink); text layer = `<pre>` with per-line spans (21px line-height, 13px mono, current line bg surface) under a transparent `<textarea>` (caret ink, wrap off, Tab inserts 2 spaces). Tinting: frontmatter `---` ink3; keys ink/600, colon ink3, values ink2, arrays/bools/numbers ok, trailing `  # comment` ink3 italic; headings 700; list bullets ink3; `<!-- tier -->` ink3 italic. **Status bar** 26px mono 11 ink3: save state · Ln, Col · N lines · bytes · language · UTF-8 · Spaces: 2. In production use CodeMirror 6 with a Markdown+YAML-frontmatter mode; keep the gutter/status-bar look.
- Bottom strip (max 38% height, top hairline, auto-fit ≥260px): **Validation** (cards: kind badge schema/coherence/slot, path, message; click jumps to the owning file) and **Resolved** (key/value rows of the parsed draft).
- Editing agent.md frontmatter re-parses into the draft (name, description, model, autonomy, channel, connectors, skills, gate) and regenerates dependent files; editing stable/context writes the directive.

## Chat builder
Grid `minmax(0,1.3fr) minmax(0,1fr)`. Left: header (←, “Builder”), transcript (max-width 720, user bubbles accent-soft right-aligned; builder bubbles panel/line left; streaming caret blink), after each builder turn a pill “Draft updated · {summary}” (the `<agent_draft>` block is stripped from prose), suggestion chips, composer (Enter sends, Shift+Enter newline). Right: “Draft” header, the **AgentDraftForm**, footer with validation summary + Review.
Protocol: outbound = text + full draft + catalogs (connector ids, skill ids, model ids, tool ids); inbound = prose + one `<agent_draft>` JSON block; every id is filtered against the catalogs before merge; parse failure = no form change.

## Import
Source cards: Claude Code (CLAUDE.md · .claude/skills · .mcp.json · settings.json), Hermes (SOUL.md · MEMORY.md · USER.md · skills/), Letta Agent File (*.af), OpenClaw (SOUL.md · openclaw.plugin.json · skills/). Drop zone until a source is picked (“Credentials are replaced with SecretRef placeholders”). Scan state: spinner + streamed file lines. Mapping table `minmax(0,1.2fr) 20px minmax(0,1.2fr) 72px`: source artifact + note → Rustyprint field + note, status **resolved** (ok) / **bind** (warn) / **unresolved** (err). Aside: counts, note, “Import as draft” (new blueprint_id; enters as draft; unresolved refs block publish).

## Review (the convergence screen)
Header: ← {source}, h1 draft name, version pill (“new · v1” or “v3 → v4”), Edit spec · Ask builder · Playground (secondary).
Left: **Changes** — one card per section (Identity, Goal, Directive, Toolsets, Tool rules, Skills, Memory, Channels, Triggers, Autonomy, Learning). Rows: sign (+ ok for new, ~ warn for changed with strikethrough old value), field mono, value. GOVERNANCE badge on Toolsets (approval wrappers changed), Autonomy, Triggers.
Right (sticky): **Validation** card (summary + violation cards, click → Compose at the owning file) · **Assembled prompt** card (bytes, Copy, Show/Hide → tiers stable/context/volatile as mono pre) · **Eval gate** card (state Not run / Running / Passing / Stale / Failing; suite · N cases; streamed case results ✓/✗ + score; Run gate / Re-run) · **Publish vN** primary (disabled with hint until 0 violations and gate Passing; absent without publish scope, replaced by a note) · **Export .rustyprint bundle** (secondary).

## Playground
Grid `minmax(0,1.3fr) minmax(0,1fr)`. Header: “Playground”, agent select (current draft or any published version), “isolated session” pill, Reset. Transcript: user/agent bubbles; **activity lines** (dot + “Searching · tool.id”); **approval card** (line border, dot warn + “Approval required”, scope; Action/Arguments/Reason rows; Approve primary, Reject secondary; decided state text). Suggestions + composer.
Right: **Events** inspector (header with count + “Save as eval case”): rows `34px 130px 1fr`: position 001…, kind mono (ToolCall/ToolResult accent, RunPaused warn, Turn* ink3), summary; expand → five guard stages (pre_execute / guards / execute / post_execute / finalize with outcome color: Deny err, Ask warn, else ok) + payload pre.

## Evals
Auto-fit ≥320 (suite list stacks above results when narrow). Suite list: id mono, pass %, cases · gates. Results: suite id, gate pill, base/head version selects, “Run against {head}”; four stat cards (Pass rate, Newly passing, Regressed, Unchanged); table `minmax(0,1fr) 40px 40px 82px 78px`: case + expectation, ✓/✗ base, ✓/✗ head, change label (regressed rows tinted err-soft), Playground button. Footnote on recorded-session cases.

## Inbox
Tabs with counts: Action required (count badge accent when > 0) · Attention · Info · Decided. Max-width 900.
- Approval card: line border, header (warn dot, title, agent · session, waiting, “expires in …” warn), rows Action (mono) / Arguments / Why (italic quote) / Touches (effect badge + egress mono); actions Approve (primary), Always allow (secondary, when sticky_allowed), Reject (secondary), “Open session →” ghost right. Without approvals:decide → note.
- Attention/Info item: colored dot, title, body, meta, optional action (View traces / Open metrics).
- Decided: decision label (APPROVED ok / REJECTED err), tool, agent · session · time, chain chips “decided by → accountable → level → receipt → version”.
- Empty state: “Nothing needs you.”

## Work (board)
Header: h1 “Work”, agent filter pills, count, “+ New task”. Columns 250px fixed, hairline border, bg surface: header dot (todo ink3, in_progress warn, in_review focus, done ink, failed err) + label + count. Card: panel, radius 10, 12px padding: id mono + priority (dot + text; Urgent err, High warn, else ink2/ink3) right; title 13/600; description ellipsis; footer avatar (20px code bg, initials) + agent name + attempt state right (Running warn / Succeeded focus / Failed err).
Detail drawer 340px: id, ×, title, description, Status/Agent/Priority/Source/Acceptance rows, Attempts (state, meta, Session button, failure reason), actions by status: todo → Start; in_review → Mark done (human + tasks:done only), Send back; failed → Rerun; in_progress → Cancel. Note “Only a human principal can move this to Done.”

## Observe
Tabs Logs · Metrics · Traces; agent filter; “live · seq N”.
- Logs: sticky header; rows `80px 1fr 90px 1fr 3fr`: time mono, agent, session mono, kind mono colored, summary; click → replay.
- Metrics: per-agent card: health dot + name + vN + goal + “On target / N off target”; stats grid (Active sessions, Tasks open, Approvals waiting, Failed attempts); measure rows: kind badge (target ok / gate ink / guardrail warn) + name, progress bar, actual mono, “target X”.
- Traces: rows `90px 1.2fr 2fr 70px 60px 90px`: session, agent + vN · channel, span bars (model ink, tool ok, wait warn), duration mono, tools, status.

## Learning
Tabs Memory · Skills ledger · Gap ledger; agent filter.
- Memory: block card: label mono + agent · scope, used/limit + bar (warn > 90%); description; contents pre; Last write, Recall hits, Consolidation, Origin mix; Consolidate now / Write history.
- Skills ledger: table (skill, state badge, retention bar + score, uses); ledger panel: entries (checkbox, mutation mono colored, actor · note, ts); select two → diff (mono ---/+++, -/+ lines) + “Roll back to older entry” (warn); Promote via gate (Trial) / Restore (Cold/Archived).
- Gap ledger: cards sorted by priority: id, status pill (Hunting warn / Open / Parked / Closed ok / Dismissed), origin (speculative in warn), priority bar + score, statement 14/600, agent · citations · meta; actions by state (Park, Dismiss, Promote to open, Resolution), Evidence → replay. Empty: “No open gaps — connect an interaction source to seed one.”

## Improve
Per-agent plan card: header (agent, vN → vN+1, status pill proposed/accepted/dismissed, “Auto-apply after gate” toggle). Analysis: findings with dot (err/warn/ok), text, evidence (ink3). Proposed changes: checkbox rows (kind badge skill/directive/rule/memory/eval, target mono, confidence, description). Footer: “Accept N changes → draft vN+1” (primary; applies selected changes to a new draft and opens Review), Dismiss, footnote depending on auto-apply.

## Security
Tabs Egress policy · Secret refs · Autonomy · Audit · Administration (admin only).
- Egress: rule table (destination, method, path mono, binary, action pill allow ok/deny err toggle), “+ Rule”, “Preview against 7d egress” → warn card listing calls that would newly deny + “Apply policy”.
- Secret refs: reference mono, store, dependents, wire probe (passing ok / pending · not live warn / not run). Footnote: no values anywhere.
- Autonomy: agents sorted full → supervised → read_only with tools list and wrapped count.
- Audit: search input, range select, Export · hashed; table time / receipt tool / principal / session / HMAC (✓ verified ok, ✗ unverifiable err) / Replay.
- Administration: cards Organization, SSO, Retention, Budgets (k/v rows + status).

## Catalog
Left tabs with counts. Every tab has an add entry point: **+ Add connector** (registry panel: signed-index rows with version, publisher, grant summary, allowlist status → Install / Request install; “Custom MCP server” hands off to Tools & MCP), **+ Add channel** (adapter picker → added as available), **+ New** skill, **Add MCP server**, **+ New plugin** (plugins tab lists installed packages with version, kind, doctor status; selecting one loads its manifest).
**Connectors**: rows (name, kind · publisher, effect badges, status dot) + detail aside (Instance URL, Credential = SecretRef name, Wire probe row with Probe button, tools with effect badges, Egress, Feeds, Install/Uninstall). **Channels**: adapter rows (name, capabilities, queue policy, status) + detail (schema-derived fields read-only in design, queue policy pills steer/followup/collect/interrupt with help text, capability pills, bound agents, Pause/Resume intake). **Skills**: list + editor card (file tabs, path, “description N/60” counter red over 60, hash, textarea, ledger line, Ledger, Save to Trial). **Tools & MCP**: add server (URL, transport select, Discover → spinner → discovered tools with effect cycle button, sandbox, wrap toggle, “Register N tools”), registered tools table (id, source, effect, budget, sandbox). **Plugins**: manifest form (id, version, kind pills, capability rows with adders, doctor: config repairs / state migrations) + manifest.json preview, signature/allowlist/eval status, “Sign and submit”.
