# 01 · Design system

## Fonts
- UI: **Instrument Sans** 400/500/600/700 (Google Fonts). Fallback system-ui.
- Mono: **JetBrains Mono** 400/500/600 — ids, paths, code, tool names, timestamps, cron, secrets, status-bar.

## Type scale (px) — no half sizes
10 (micro badges, caps labels) · 11 (meta, table headers caps, gutter) · 12 (secondary text, small buttons, mono values) · 13 (body, inputs, buttons) · 14 (nav items, card titles, chat) · 16 (screen h1 inside panes) · 18 (page h1) · 22–28 (home hero only).
Section labels: 11px / 600 / letter-spacing .06em / uppercase / ink3.
Line-height: body 1.5; code editor 21px fixed.

## Color tokens (CSS custom properties on `[data-theme]`)
| token | light | dark | use |
|---|---|---|---|
| --canvas | #e8e8e8 | #0f0f0f | page background behind the app card |
| --bg | #f7f7f7 | #171717 | sidebar + secondary surfaces, gutters |
| --panel | #ffffff | #1d1d1d | cards, main content card, inputs |
| --line | #e6e6e6 | #2c2c2c | all hairlines |
| --ink | #141414 | #f0f0f0 | primary text |
| --ink2 | #6b6b6b | #a3a3a3 | secondary text |
| --ink3 | #9b9b9b | #6f6f6f | tertiary/meta text, placeholders |
| --accent | #141414 | #f0f0f0 | primary buttons, selected borders, active states |
| --accent-soft | #ececec | #2a2a2a | selected/active fills |
| --ok / --ok-soft | #1f9d6a / #e6f6ee | #3fbf85 / #14301f | passing, Read effect, Promoted |
| --warn / --warn-soft | #b7791f / #fbf1dc | #d9a541 / #2f2712 | coherence issues, Write effect, Trial, approvals waiting |
| --err / --err-soft | #d3322b / #fbe6e4 | #ef6b5f / #3a1a17 | schema errors, Execute/Egress, failures |
| --code | #f2f2f2 | #242424 | code blocks, chips, avatars |
| --focus | #1f9d6a | #3fbf85 | composer ring, focus-visible outline |
| --shadow | 0 1px 2px rgba(0,0,0,.04) | 0 1px 2px rgba(0,0,0,.5) | cards |

Primary-button text is `var(--panel)` (white on black in light; dark on light in dark). Text contrast ≥ 4.5:1 everywhere; status colors are used for dots, badges, and short labels — never for body text on tinted fills.

## Radii
Controls 8px · cards 10–12px · main content card 14px · app frame 18px · pills 999px · avatars 6–9px.

## Controls
- **Primary button**: bg accent, text panel, 8×14 padding (small 6×12), 13px/600 (small 12px), radius 8, no border.
- **Secondary button**: bg panel, 1px line border, ink text, 8×12 (small 6×10), 13px/500.
- **Ghost/inline**: transparent, ink2, no border (e.g. “Open session →”).
- **Pill chips**: 999px radius, line border; selected = accent border + accent-soft fill.
- **Inputs / textareas**: panel bg, 1px line, radius 8, 8×10 padding, 13px, placeholder ink3, focus border ink3 + 2px focus outline on focus-visible.
- **Select**: same as input, appearance none, custom chevron SVG at right 9px, right padding 28px. Use `background-color` (shorthand `background` wipes the chevron).
- **Badges**: 10px/700/letter-spacing .05em, 1px border in status color, transparent fill (effect class, skill state, kind).
- **Status dot**: 7–8px circle in status color, paired with a text label.
- **Composer**: panel bg, radius 14, 1px focus-colored border + `0 0 0 3px var(--ok-soft)` ring, textarea + primary Send.
- **Toast**: bottom-center, ink bg / bg text, radius 8, 2.6 s.

## Layout shell
- Body: canvas color, 12px padding, full viewport height, overflow hidden.
- App frame: flex row, bg `--bg`, 1px line, radius 18, shadow `0 8px 30px rgba(0,0,0,.06)`.
- Sidebar: 228px (border-box), padding 18/14/14, **no right border**, bg `--bg`. Logo row (30px accent square with mono “R” + “Rustynome” 17px/600), grouped nav (group label caps → items 8×10 padding, radius 8, 14px/500, 17px stroke icons in ink2, active = accent-soft fill), scroll region flex:1, pinned footer: theme toggle, user row with role select.
- Main card: flex:1, margin 12, panel bg, 1px line, radius 14, overflow hidden; optional connection banner on top; screens fill it.
- Every two-pane screen uses proportional tracks (e.g. `minmax(0,1.3fr) minmax(0,1fr)`) or `repeat(auto-fit,minmax(320px,1fr))` so panes stack at < ~700px. Never fixed-min side panels. Tables use shrinkable tracks; minimum sums must stay under ~450px.

## Theming
`data-theme="light|dark"` on the root; tokens above. A third theme was trialed and removed — keep exactly two.

## Iconography
Inline 24-viewBox stroke icons (1.6px, round caps) in ink2. No emoji.
