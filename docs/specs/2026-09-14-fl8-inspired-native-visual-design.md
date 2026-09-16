# FL8-inspired native visual redesign

## Approval basis

The user supplied `fl8-old-visual/` as the visual-quality reference after rejecting the first functional wireframe, then explicitly required an original logo and a simpler product structure. The approved direction reinterprets the compact palette, density and state hierarchy under `fl8-old-visual/src/`; it does not copy that shell or the unrelated large dashboard screenshots under `fl8-old-visual/docs/`.

## Scope

Redesign only the native Slint desktop experience and the minimal daemon DTOs needed to expose the already-parsed Mihomo group catalog. Preserve the existing Rust core, opaque Xray handling, subscription contract, authentication, and two-process lifecycle. The supplied application is a visual-quality reference only; none of its Clash Verge screens, backend commands, settings model or branding are imported.

The default window remains compact: 400×700, resizable from 380×600 to 500×800. The product should feel like a small personal network controller, not an administration dashboard.

## Alternatives considered

1. **Simplified MultiCore shell using the reference's visual grammar — selected.** Dark mobile-width layout, restrained glass cards, electric-blue accent, green connected state, an original top brand, one home surface and compact overlay panels. It keeps the quality of the reference while removing its navigation and Clash-specific density.
2. **Large desktop dashboard.** Rejected because traffic charts, multi-column cards and engine controls contradict the one-action product requirement.
3. **Cosmetic recolor of the current screen.** Rejected because the current hierarchy, oversized primary button and diagnostics drawer are the main visual problems; changing colors would not fix them.

## Visual system

- Background: `#0A0A0D`; deepest layer `#06060A`.
- Cards: `#111114`; elevated surface `#1A1A1F`; borders `#242429` or translucent white at 6%.
- Primary text: `#FFFFFF`; secondary `#B0B0B8`; muted `#666670`.
- Accent: `#0091FF`; success `#22C55E`; warning `#F59E0B`; error `#EF4444`.
- Radius scale: 8 / 10 / 14 / 16 px.
- Spacing follows a 4/8 px rhythm, with 16 px outer gutters.
- Typography uses Segoe UI Variable/System UI so Cyrillic remains native and crisp. Normal text is 12–15 px; page/status headlines are 18–24 px.
- Signature motif: an original `Twin Core Route` vector mark — two independent rounded cores joined by one directional route — plus a restrained blue or green halo around the connection card. It must remain legible at 16, 24, 32 and 64 px and must not reuse the supplied application's cat, name or logotype.
- Interaction feedback: opacity/color changes and at most a subtle 150–250 ms glow. Loading uses a compact pending state, not a blank view.

## Information architecture

The shell is one home screen. There is no sidebar, tab bar or bottom navigation.

- Tapping the profile row opens the subscription panel.
- Tapping the current route opens the Mihomo route-selection panel.
- Tapping `События` opens the redacted diagnostics panel.
- Closing a panel always returns to the same home state.

No settings destination is exposed until there are real typed settings. No separate Xray/Mihomo controls exist anywhere.

### Header

- Original `Twin Core Route` mark and `MULTICORE` wordmark.
- Right-side compact state pill: `ГОТОВО`, `В СЕТИ`, `ПОДКЛЮЧЕНИЕ`, `ОШИБКА`, or `СЕРВИС`.
- The whole top strip stays visually quiet; it is not a second toolbar.

### Empty / first-run home

- Centered dual-core mark with a low-opacity blue halo.
- Headline `Добавьте подписку` and one sentence explaining automatic setup.
- Primary button `Вставить ссылку` where clipboard support exists; otherwise an always-visible URL field plus `Добавить`.
- Secondary helper shows the `multicore://install-sub?url=…` scheme without exposing raw config formats.

### Ready / connected home

- One interactive connection card is the visual anchor.
- The card shows state, current profile and current route. Clicking it performs the only primary action: connect or disconnect.
- Connected uses green border/halo; ready uses blue; pending uses amber; error uses red.
- A small profile/update row follows. Traffic and expiry are omitted until real backend fields exist; no fake metrics.
- Mihomo proxy groups appear as horizontally scrollable chips.
- The selected group's nodes appear as compact 52–60 px rows with name, current-selection mark and optional latency only when real data exists.
- Node selection is disabled while a request is pending and visibly rolls back on failure.

### Subscription panel

- One active-profile card with display name and state; subscription URL is never rendered.
- Inline URL field and `Добавить / Заменить` action.
- Importing, invalid input and server failure remain inline and actionable.
- No editable User-Agent field: the three MultiCore UAs are fixed implementation details.

### Events panel

- Filters: `Все`, `Система`, `Ошибки`.
- Rows show timestamp, severity mark and safe message.
- Transport/decode failure is a visible safe row, not an empty list.
- No raw subscription URL, authorization, UUID or config body is displayed.

## Data contracts

Existing `StatusDto` remains the source for connection state, profile, current node, message and degraded status.

Add a read-only catalog DTO backed only by parsed Mihomo YAML:

```text
CatalogDto { revision, groups: [GroupDto] }
GroupDto { id, label, selected, nodes: [NodeDto] }
NodeDto { id, label, selected, delay_ms? }
```

Display labels are untrusted presentation data and are never used as selection identifiers. Suspicious labels are genericized; opaque group/node IDs are resolved only against the matching catalog revision so a subscription refresh cannot redirect a click to another raw Mihomo name. The catalog ships with working node selection. Live latency remains optional and must be omitted when unavailable; delay values are never invented. A node-selection mutation targets Mihomo only; Xray JSON remains opaque and is never inspected by UI or daemon catalog code.

Each `type: select` group owns an independent selected node. The daemon must never model `Сервер`, `Российские сайты`, `Игры`, or any other selector as one global current node: changing one group preserves every sibling group's selection. `GroupDto.selected` identifies the primary group used by the compact connection summary; the desktop's currently visible group is local navigation state and survives catalog refreshes and selection responses.

## Accessibility and interaction

- Every button and row target is at least 44×44 px.
- Focus is visible with a 2 px accent outline/border.
- State is communicated through text and shape as well as color.
- Text contrast meets WCAG AA against the dark background.
- The tab order follows header → page content → secondary rows → the currently open overlay panel.
- Motion is optional polish; correctness and reduced-motion behavior do not depend on animation.

## Failure-mode check

1. **CSS effects do not map directly to Slint.** Critical if copied literally. The implementation uses layered translucent rectangles and borders; backdrop blur, mouse-tracked gradients and web-only filters are intentionally omitted.
2. **The compact shell turns into another dashboard.** Critical. Metrics without real backend values, traffic charts and engine toggles are excluded.
3. **Groups are displayed but cannot be safely changed.** Critical. Interactive rows ship only with a Mihomo-only selection API and rollback behavior; otherwise the catalog is clearly read-only.
4. **Small-window overflow.** The 380×600 size is a required verification target, with scrollable content and panels constrained inside the window.

## Testing

- View-model tests for all connection states and all three pages.
- DTO tests using JSON serialized by the real daemon types.
- Empty/loading/error/degraded/success rendering state assertions.
- Catalog tests proving labels originate only from safe Mihomo group/node presentation data while raw names remain daemon-internal.
- Revision/identity tests proving stale or lossy labels can never select a different Mihomo target.
- Selection tests proving pending disablement and rollback on failure.
- Multi-group regression tests proving sequential selections remain independent and catalog responses do not move the user's visible group.
- Slint compile/build at 380×600, 400×700 and 500×800 constraints.
- Manual visual smoke check of the release executable.

## Brand assets

- Create a new SVG mark under the MultiCore desktop assets, not inside the reference tree.
- Use the same geometry for the in-app 24/32 px mark and the future Windows icon master.
- The wordmark is rendered with native typography so it stays sharp and localizable independently from the icon.
- Do not copy, recolor or trace the reference application's logo.

## Non-goals

- WebView, Vue or Tailwind in the new client.
- Large desktop dashboard, charts or raw core logs on the home page.
- Editable raw YAML/JSON, User-Agent overrides or engine switches.
- Copying FL8 trademarks, logotype or product name; only its visual grammar is reused.
- Importing Clash Verge pages, Tauri commands, settings toggles, editable User-Agent fields, traffic dashboard or backend behavior.
