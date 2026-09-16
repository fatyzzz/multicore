# MultiCore native visual system

MultiCore uses a compact Windows-first native presentation derived from the user-supplied `fl8-old-visual/src` interaction reference: a safe subscription summary, one interactive connection anchor, and inline Mihomo route controls. The default window is 480 × 720 px and the verified responsive range is 420 × 640 through 620 × 900 px.

The application uses Slint's native renderer. It contains no WebView, browser shell, dashboard, sidebar, traffic graph, fake telemetry, raw configuration editor, or separate engine controls.

## Identity

The client does not repeat a logo or product wordmark inside the content area. The standard Windows title bar already identifies `MultiCore`; the native surface starts with a quiet right-aligned utility row. This keeps the product identity out of the connect-and-route hierarchy until a dedicated application icon is designed and approved.

Route flags and semantic emoji use the embedded Twemoji Mozilla COLR/CPAL font copied from the supplied visual reference. The font and artwork attribution is recorded in `packaging/windows-x64/THIRD_PARTY_NOTICES.md`.

## Tokens

All reusable values live in `ui/theme.slint`.

| Token | Value | Use |
| --- | --- | --- |
| `void` | `#08090C` | Window edge |
| `canvas` | `#0A0A0D` | Main background |
| `surface` | `#111114` | Connection surface |
| `elevated` | `#1A1A1F` | Selected and disabled controls |
| `line` | `#242429` | Borders and separators |
| `text-primary` | `#F4F6FB` | Headings and values |
| `text-secondary` | `#B0B0B8` | Supporting copy |
| `text-muted` | `#666670` | Metadata |
| `accent-blue` | `#0091FF` | Primary action and selection |
| `accent-green` | `#22C55E` | Connected state only |
| `warning` | `#F59E0B` | Pending/degraded state only |
| `error` | `#EF4444` | Error state only |
| `focus` | `#66BAFF` | Two-pixel keyboard focus |

Typography uses Segoe UI Variable Text with 12 px metadata, 14 px body labels, 16 px control values, and 22 px state titles; icon-only emoji text uses embedded Twemoji Mozilla. Spacing follows 4, 8, 12, 16, 24, and 32 px. Control radius is 10 px; the connection surface radius is 14 px. Interactive targets are at least 44 px high, while route rows have a fixed 52 px minimum, preferred, and maximum height.

## One-screen hierarchy

1. A fixed 44 px utility row: no logo, duplicate product name, local label, or repeated status. A link icon opens subscription replacement only on demand; an overflow icon opens diagnostics.
2. A fixed 92 px interactive connection surface: a vector power symbol, state eyebrow, headline, and current route. In ready/connected states the whole surface is the connect/disconnect action.
3. Inline subscription onboarding is visible only while no profile exists. With an existing profile, the replacement URL editor occupies no space until the link action opens it; its commit action is `Сохранить`.
4. The separate 44 px primary action exists only during first-run subscription import; it is not duplicated below the connection surface.
5. Inline routes: horizontally scrollable icon/group chips followed by icon-bearing node rows and the only normal vertical scroll region.

At additional height, the action and surfaces keep their fixed size; only the node-list viewport grows. This prevents the primary action from becoming a decorative color block.

## States

| State | Surface behavior | Primary action | Routes |
| --- | --- | --- | --- |
| Empty | Inline URL field and scheme hint | Add subscription, enabled only with input | Empty read-only region |
| Importing | Input and replacement controls lock in place | Disabled progress | Locked |
| Ready/disconnected | Profile and current automatic route remain visible | Click connection surface | Group navigation remains available; nodes are read-only |
| Connecting/disconnecting | Copy changes without layout movement | Connection surface locks | Locked |
| Connected | Compact green status and current route | Click connection surface | Node selection enabled |
| Error/degraded | Safe message stays in the connection surface | Retry/cleanup | Existing safe catalog may remain visible |
| Selection pending | Chosen row updates optimistically in amber | Connection action locks | Node mutations lock until authoritative reconciliation |

Diagnostics replaces the home content in the same native window. It retains Escape close behavior, keyboard focus restoration, severity filters, and redacted event text.

## Interaction and accessibility

- Tab order follows the visible top-to-bottom hierarchy.
- Enter and Space activate custom buttons, chips, rows, and the diagnostics close control.
- Focus is always visible as a two-pixel high-contrast border.
- Status combines text and a dot; selection combines border, fill, dot, and label treatment.
- Leading flag/symbol prefixes are split into dedicated Twemoji-rendered icon tiles without changing the opaque daemon IDs. Names such as `🇩🇪 Germany [de]`, `🌍 Сервер`, and `🎮 Игры` retain their intended identity instead of becoming country-code boxes or generic numbered labels.
- Disabled and pending states retain readable text and expose their state to accessibility APIs.
- A route row's action gate is independent from its visual opacity: selected and read-only rows remain fully legible while pointer, keyboard, and accessibility enabled state accurately report that they cannot be activated.
- State changes are immediate; there are no blur, glow, gradient, pointer-tracking, or decorative motion effects.

## Visual proof

The deterministic loopback fixture provides coherent neutral Russian display DTOs plus fixed non-secret subscription metadata matching the UI contract; it contains no raw configuration, source URL, credentials, or invented latency. `scripts/run-preview-capture.ps1` captures each state in a separate fixture/client process pair, validates that output stays under the workspace screenshot directory, deletes only the six exact expected filenames, verifies the preview port before start, and verifies cleanup afterward. The capture helper settles the real desktop window, sends selection input directly to its HWND in client coordinates, renders an opaque 24-bit full-window frame, validates content/semantic pixel anchors, and accepts output only after two identical valid frames.

Accepted captures:

- `artifacts/screenshots/preview-empty-420x640.png`
- `artifacts/screenshots/preview-ready-480x720.png`
- `artifacts/screenshots/preview-connected-620x900.png`
- `artifacts/screenshots/preview-populated-catalog-620x900.png`
- `artifacts/screenshots/preview-error-420x640.png`
- `artifacts/screenshots/preview-selection-pending-480x720.png`

The small-window captures intentionally show only the portion of the route list that fits the remaining viewport; the list scrolls independently while the utility row, connection surface, onboarding editor, and primary action remain fixed and unclipped.

## Data boundary

Slint owns presentation only. Rust owns state, redaction, background work, and daemon DTO mapping. The desktop consumes authenticated loopback status, refresh, events, catalog, and node-selection endpoints. It displays only the safe source host, real bounded usage/expiry metadata, and never the stored subscription URL. It does not parse Xray JSON, invent metrics, or start bundled cores through the visual preview fixture.
