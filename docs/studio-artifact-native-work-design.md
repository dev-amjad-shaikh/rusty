# Studio Phase 2 — Artifact-native Work

## Customer outcome

A person can open a completed run, see its produced outputs, preview supported evidence, download exact bytes safely, inspect lineage, and move between the output and the trace step that produced it. Artifacts stay contextual inside Work; there is no separate Artifacts destination.

## Route map

Existing Work routes remain primary:

- `/work/:threadId/runs/:runId` — Run stage
- `/work/:threadId/runs/:runId/trace` — Trace stage
- `/work/:threadId/runs/:runId/evaluate` — Evaluate stage

Artifact inspection is a dialog inside the Run and Trace stages. Future phases may route artifact detail as a sheet on mobile, but Phase 2 keeps the workspace intact.

## Query and mutation map

- `GET /artifacts?run_id={runId}` — list run artifacts (TanStack Query)
- `GET /artifacts/{artifact_id}` — artifact record
- `GET /artifacts/{artifact_id}/preview` — derived preview (text, JSON, image PPM, audio waveform)
- `GET /artifacts/{artifact_id}/bytes` — exact bytes (streamed download)
- `POST /artifacts/{artifact_id}/release` — retention release
- `GET /artifacts/names/{name}/versions` — named version history

## Desktop wireframe

```
[context bar: agent / run / status]
[Run | Trace | Evaluate]

Run/Trace workspace
  ┌─────────────────────────────────────┐
  │                                     │
  │   stage content                     │
  │                                     │
  └─────────────────────────────────────┘
  Outputs (collapsible tray)
  ┌─────────┐ ┌─────────┐ ┌─────────┐
  │ weekly  │ │ chart   │ │ audio   │
  │ report  │ │ png     │ │ clip    │
  └─────────┘ └─────────┘ └─────────┘
```

Clicking an artifact opens a modal inspector:

```
┌────────────────────────────────────────────────┐
│ weekly-report                        [Close]   │
├────────────────────────────────────────────────┤
│ Identity: a1b2…e3f4                              │
│ Kind: file   Type: text/plain   Size: 42 B     │
│ Retention: Receipt protected                   │
├────────────────────────────────────────────────┤
│ Lineage: Run → Effect → Event                    │
├────────────────────────────────────────────────┤
│ [Text preview]                                 │
├────────────────────────────────────────────────┤
│ [Download exact bytes]                         │
└────────────────────────────────────────────────┘
```

## Mobile wireframe (390 px)

The stage workspace stacks vertically. The output tray becomes a single-column list. The inspector fills the viewport as a bottom sheet.

## States

- Loading: "Loading outputs…"
- Empty: "No artifacts were committed for this run."
- Error: "Could not load outputs for this run." + retry
- Preview unavailable: honest reason from server
- Bytes unavailable: tombstone preserving identity/lineage

## Backend capability and gaps

Reused:
- `/artifacts` list, record, preview, bytes
- `ArtifactCommitted` event in run journal

Pending for follow-up:
- `/artifacts/{id}/release` UI
- `/artifacts/names/{name}/versions` version history
- Operations exception handoff for `artifact_unavailable`
- Canvas-based PPM image renderer (current seam: base64 PPM data URL may not render in all browsers)

## Files owned

- `studio/ui/src/lib/api/artifacts.ts` (new)
- `studio/ui/src/lib/api/client.ts` (exports)
- `studio/ui/src/features/work/artifacts/Artifacts.module.css` (new)
- `studio/ui/src/features/work/artifacts/ArtifactTray.tsx` (new)
- `studio/ui/src/features/work/artifacts/ArtifactInspector.tsx` (new)
- `studio/ui/src/features/work/artifacts/ArtifactTray.test.tsx` (new)
- `studio/ui/src/features/work/WorkPage.tsx` (tray integration)
