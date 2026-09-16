# Quiet Signal Native UI Redesign

> Superseded on 15 September 2026 after the user reasserted the supplied `fl8-old-visual/src` UX as the approved reference. The current contract is recorded in `docs/design/visual-system.md` and restores the interactive connection card, reference palette, and flag/symbol route identity without importing the reference backend.

## Decision

Replace the current card-and-overlay interface with an original compact native utility named **Quiet Signal**. The redesign is justified by observed release screenshots: the status pill expands across the window, the 380×600 layout clips, the connection state is repeated, and the route overlay looks like a web modal. Shipping the current presentation would make an otherwise functional client feel broken.

The change is proportional: backend contracts and Rust state machines remain intact; only Slint composition, presentation mapping, original visual assets, and visual fixtures change.

## Scope

- One responsive 400×640 native window, usable from 380×600 through 520×800.
- A quiet 56 px header with the original MultiCore mark, product name, and compact status dot/text.
- One 128–144 px connection surface with status, current profile/route, and one explicit connect/disconnect button.
- Inline subscription onboarding when no profile exists: URL field, one add action, inline progress/error, and the `multicore://install-sub?url=…` scheme hint.
- A compact saved-subscription row after import with a replace action.
- Inline Mihomo route chooser: horizontally scrolling group chips and a vertically scrolling node list. Only the node list scrolls during normal use.
- Events move out of the primary hierarchy into one quiet `Диагностика` header action and a simple same-window details surface.
- Existing connection, import, catalog, selection, reconciliation, authentication, and accessibility behavior remains unchanged.

## Visual System

- Style: flat graphite native utility; no blur, glow, gradient, mesh, giant pills, or decorative animation.
- Palette: original neutral graphite surfaces with a cool indigo-blue action color. Green, amber, and red appear only in compact semantic status indicators.
- Typography: Segoe UI Variable Text with four levels: 12 px metadata, 14 px body, 16 px control/value, 22 px state title.
- Spacing: 16 px window gutters; 8 px and 12 px internal rhythm.
- Radius: 8 px controls, 12 px state surface; no pill radius except the small status dot itself.
- Borders: one-pixel neutral separators; two-pixel focus treatment.
- Primary action: exactly one visually dominant button on the main screen.

## Original Identity

Redraw the repository-owned SVG into a simpler Twin Route mark: two solid offset nodes joined by one diagonal routed stroke. It must not reuse FL8/Clash geometry, fonts, icons, names, or source.

## State Behavior

- Empty: inline subscription form replaces connection actions; disabled connection is not shown as the first action.
- Importing: URL input and add action lock; progress stays in the same region.
- Ready/Disconnected: connection action is primary; catalog remains readable and group navigation remains local.
- Connecting/Disconnecting: action locks and status text changes without layout movement.
- Connected: compact green status, disconnect action, interactive Mihomo nodes.
- Error/Degraded: safe inline message near the connection surface; diagnostics remains available.
- Selection pending: selected node updates optimistically, node mutations lock, and authoritative reconciliation uses the existing model behavior.

## Non-goals

- No traffic graphs, memory counters, fake telemetry, quota, expiry, dashboard, sidebar, bottom navigation, or separate engine controls.
- No WebView, Vue, Tailwind, Zashboard, raw config editor, editable User-Agent, or copied reference assets.
- No installer, bundled cores, updater, or signing as part of this visual task.

## Failure-mode Check

1. **Small-window overflow:** avoid absolute vertical coordinates; compose with layouts and give remaining height only to the node list.
2. **Route list overwhelms the primary task:** cap its visual priority with compact rows and keep the connection action fixed above it.
3. **Native widgets break the visual language:** replace visible stock buttons with repository-owned Slint controls while retaining keyboard, focus, disabled, and accessibility semantics.
4. **Visual simplification breaks backend behavior:** preserve the existing callback/property contract where possible and lock it with source-contract and view-model tests before replacing composition.

## Verification

- Compile and test all desktop targets.
- Capture Empty/Ready, Connected, Routes, Error, and selection-pending states at 380×600, 400×640, and 520×800.
- Reject any screenshot with clipping, overlapping text, a second dominant action, giant colored shapes, or opaque-ID/raw-secret output.
- Run full workspace tests, Clippy, format, forbidden-brand/UA/secret scans, then build the release preview.
