# Studio release workspace design

## Scope

Add the Releases mode to Operations. This is Phase 1 of productizing Rusty's R0.12 deployment control plane inside the typed Studio.

Supported customer journey:

```
evaluated run → immutable revision → staging environment → shadow evidence → gate → canary → promotion or rollback
```

Unsupported in Phase 1: secret set/rotate/revoke controls (read-only metadata only), multi-environment promotion chains beyond one environment selection, and approval-token issuance.

## Product model

Operations keeps three internal modes, selected with a segmented control under the Operations heading:

- **Attention** — failures and blocked work; default.
- **Releases** — environments, revisions, gates, canaries, promotion, rollback.
- **Systems** — schedules, automations, task queue.

The primary nav (Agents / Work / Operations) does not change. The segment control is the only added chrome.

## Information architecture

New routes:

```text
/operations
/operations/releases
/operations/releases/:environment
/operations/releases/:environment/revisions/:revisionId
```

Environment and revision identities appear in URLs. No secrets, payloads, server origins, or approval tokens ever appear in URLs.

## Layout

Desktop:

```text
┌──────────────────────────────────────────────────────────────────────┐
│ Operations    [ Attention | Releases | Systems ]                     │
├──────────────────────────────────────────────────────────────────────┤
│ Evidence spine                                                        │
│ Agent version → Run → Evaluation → Revision → Environment            │
├───────────────┬────────────────────────────────┬─────────────────────┤
│ Environments  │ Current decision               │ Evidence inspector  │
│               │ (one dominant action)        │ exact identity/pins │
├───────────────┴────────────────────────────────┴─────────────────────┤
│ Deployment timeline                                                  │
└──────────────────────────────────────────────────────────────────────┘
```

Mobile (<=390 px):

- Environments become a labelled selector.
- Evidence spine becomes a horizontal scroll with a "current step" indicator, or a compact disclosure on very narrow screens.
- Decision area appears above evidence.
- Primary action sits in a stable bottom action region.
- Exact evidence moves into a full-width disclosure.

## State ownership

- TanStack Query owns server state; query keys begin with connection identity.
- URL owns selected environment and revision.
- Local state owns open disclosures, draft author name, selected run for shadow, and acknowledged-check state for destructive actions.
- Connection changes cancel in-flight mutations and clear query cache.

## Evidence spine

A read-only visual timeline showing the durable chain from agent version to run to evaluation to revision to environment. Each step links to its authoritative evidence:

- Agent version → Agents page.
- Run → Work run/trace.
- Evaluation → Work evaluate (future) or exact run evidence.
- Revision → revision detail panel.
- Environment → selected environment.

Missing steps render as "not available" rather than inferred.

## Decision states

The center canvas shows one current decision for the selected environment:

1. **Nothing serves here.** Action: "Choose a revision".
2. **Choose a revision.** List revisions; action: "Prepare promotion".
3. **Review shadow evidence.** Action: "Start canary" or "Reject revision".
4. **Gate blocked.** Show failing checks; action: "Re-evaluate" or "Choose another revision".
5. **Ready for canary.** Action: "Start canary".
6. **Canary collecting evidence.** Show active/canary run counts; actions: "Promote", "Clear canary".
7. **Ready to promote.** Action: "Promote to [environment]".
8. **Rollback available.** Action: "Roll back to [previous revision]".
9. **State changed elsewhere.** Action: "Refresh and review".

Only one dominant action is shown. Destructive/consequential actions require an acknowledgement checkbox and author input.

## Environment declaration

Dialog collects:

- Name (validated to Rusty environment tag rules: 1-64 UTF-8 bytes, no whitespace, control chars, `@`, or `/`).
- Optional gate policy name and dataset version.
- Approval required checkbox.
- Author (human id).

Review screen shows plain-language summary before `POST /deployments/environments`.

Accept `201 created:true` and `200 created:false` with matching declaration as success. Treat `409` as a conflict, not an overwrite.

## Revision creation

Form collects:

- Registered graph (select from server info).
- Optional assistant identity.
- Source environment (select from declared environments).
- Registry surfaces to freeze (multi-select from registry surfaces).
- Author.

Review screen shows complete frozen pin set and a statement that later registry changes will not alter this revision. Uses `POST /deployments/revisions`.

## Shadow review

Requires selecting an exact completed run from Work. The run evidence is loaded and `POST /deployments/shadows` is called.

Result shows:

- Completed or failed.
- Refused effects.
- Unserved effects.
- Recorded effects the candidate did not request.
- Links to source trace and shadow evidence.
- Exact payload disclosure in a bounded disclosure.

## Gate decision

Gated environments run the gate server-side on canary declare or promotion. After action, load `/deployments/health` and `/deployments/journal`.

Gate refusal renders each check:

- Metric, observed value, required value, pass/fail, server explanation.

## Canary

Fraction choices: 1%, 5%, 10%, 25%, 50%, plus exact numeric input.

Shows current active revision, candidate revision, gate decision, current run evidence, and blast radius. Requires acknowledgement.

Uses `PUT /deployments/environments/{name}/canary`. Supports `DELETE` to clear and promotion to graduate.

## Promotion

Review compares current active and proposed revisions, gate decision, canary evidence (if present), approval requirement, and consequence. If approval is required and Studio has no approval token, the action is blocked with missing dependency.

Uses `POST /deployments/environments/{name}/promote`.

## Rollback

Shows environment, current revision, exact previous revision derived from deployment chain, frozen pins returning, and required cause. Uses `POST /deployments/environments/{name}/rollback`. Action label: "Roll back to [short revision]".

## Timeline

Loads `/deployments/journal`. Renders known deployment events in human language with actor, environment, revision short id, and timestamp. Unknown future event kinds render as bounded uninterpreted evidence.

Known events: revision_registered, environment_declared, gate_decision_recorded, canary_declared, canary_cleared, revision_promoted, revision_rolled_back, shadow_run_started, shadow_effect_refused, shadow_verdict, env_secret_set, env_secret_revoked, env_secret_denied.

## Secrets metadata

Read-only list from `GET /deployments/secrets?environment=`. Shows name, environment, set by, created time, rotation time. Never calls `/deployments/secrets/resolve`.

## Mutation safety

- Capture connection epoch in mutation key.
- Validate strict receipts before updating cache.
- `409` reloads environment state and requires fresh review.
- Network/5xx/ambiguous outcomes lock unsafe retry until an authoritative read proves the result or the operator explicitly abandons a safe-to-duplicate action.
- Late responses from a different tenant/epoch are discarded.

## Copy rules

Use customer language. Avoid "POST", "CAS", "422", "payload", "slot". Prefer "Prepare revision", "Start canary", "Promote to production", "Gate blocked this release", "No recent run evidence", "The environment changed while you were reviewing it".

## Test plan

1. Environment declaration: create, converge, conflict.
2. Revision creation with frozen pins.
3. Shadow: success, divergence, failure.
4. Gate allow/refuse.
5. Canary declare/clear/promote.
6. Promotion and rollback.
7. Journal timeline renders events.
8. Connection-switch and tenant races discard stale state.
9. Mobile 390 px layout.
10. Accessibility: keyboard, focus, live region, screen-reader structure.
