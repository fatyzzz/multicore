# MultiCore Design System

## Direction

Native Control Center: a compact Windows network utility with one obvious daily action and deeper operational truth in dedicated sections. It is not a mobile app stretched onto desktop, a provider-branded dashboard, or a grid of equal cards.

## Composition

- Default window: approximately 820 × 720 px; minimum 700 × 620 px.
- A narrow icon-bearing left navigation rail owns only `Главная`, `Состояние`, and `Настройки`.
- The main pane uses an open connection region, flat rows, and sparse separators. Do not wrap every section in a bordered card.
- `Главная` is the daily one-page workspace: connect/disconnect, subscription freshness, every route group, and the selected group's server list remain visible together.
- Selecting a group replaces the inline server list in place. Route selection never navigates away from `Главная`.
- `Состояние` owns real engine/TUN health and redacted logs. Never invent traffic or latency.
- `Настройки` owns Windows lifecycle, update, privacy, and advanced controls only when the backend can actually apply them.

## Color

- `canvas` `#0D0F12`
- `surface-1` `#15181D`
- `surface-2` `#1C2026`
- `stroke` `#2A3038`
- `text-primary` `#F3F5F7`
- `text-secondary` `#B8C0CA`
- `text-muted` `#7F8996`
- `accent` `#4C9DFF`
- `success` `#3CCB7F`
- `warning` `#E7AA45`
- `danger` `#F06A6A`

Accent is singular. Green reports connected health; it is not a second CTA color. No gradients, glass, halos, or pure black/white.

## Typography

Use `Segoe UI Variable Text`. Sentence case only; no uppercase Russian status labels.

- Connection state: 24 px / 600
- Page title: 20 px / 600
- Section title and controls: 14 px / 500–600
- Body: 13 px / 400
- Metadata: 12 px / 400
- Numeric values: 16 px / 600 where needed

## Geometry and Density

- 4 px spacing base; 16–20 px pane inset; 24 px section gap.
- Rows: 44–48 px. Primary action: 48 px.
- Radii: 8 px controls, 12 px larger surfaces; pills only for compact status/filter controls.
- Use one boundary mechanism per surface. No nested cards.
- Motion: 120 ms press feedback and at most one 160–200 ms page/sheet transition; no perpetual animation.

## Interaction Truth

- Controls name the action: `Подключить`, `Отключить`, `Обновить`, `Повторить`.
- Group navigation, selected node, queued node, and applied route are visually distinct concepts.
- Connected selections apply immediately; disconnected selections say `Будет применено после подключения` on the affected row.
- Mutations expose local pending, success, and actionable failure states.
- Every interactive target is at least 44 px and keyboard focus remains visible.

## Anti-patterns

- Decorative logo/header stacks, centered mobile title bars, mystery `…` as the main navigation.
- Ambiguous subscription numbers without `использовано/осталось` semantics.
- Card soup, fake charts, raw YAML/JSON, duplicated engine switches, or provider marketing.
- Emoji standing in for UI icons. Route flags remain allowed because they are data.
- Settings that look functional before a real backend exists.
