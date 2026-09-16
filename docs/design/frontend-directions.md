# MultiCore UI directions

Status: design proposal only. This document does not authorize changes to the Slint production UI.

## Design read

Reading this as a native Windows VPN client for everyday users, with a calm trust-first utility language, leaning toward Fluent 2 behavior and metrics while remaining native Slint. The `design-taste-frontend` anti-slop rules are used for hierarchy, restraint, consistency, and copy. Its own scope excludes dashboards, so dense product patterns follow Windows and Fluent conventions instead of landing-page patterns.

Recommended dials for the product shell:

- `DESIGN_VARIANCE: 4`. Stable alignment and predictable navigation matter more than expressive composition.
- `MOTION_INTENSITY: 3`. Motion is limited to state feedback and panel transitions.
- `VISUAL_DENSITY: 5`. Home remains sparse; routes, diagnostics, and settings can be denser.

## Current-state audit

### What exists

The current Slint window is a narrow, responsive 420-620 px client with one vertical content stream. It includes:

- subscription source, usage, expiry, refresh, and on-demand URL replacement;
- one connection surface for ready, busy, connected, and error states;
- Mihomo route group chips and node rows;
- same-window diagnostics with Xray, Mihomo, and TUN checks plus filtered events;
- keyboard focus, Escape dismissal, safe display DTOs, and deterministic preview states.

The visual system is graphite dark mode, Segoe UI Variable Text, a blue interaction accent, green connected status, 10/14 px radii, and 44 px minimum targets.

### What should be preserved

- One obvious connect or disconnect action.
- Safe subscription metadata without revealing its URL.
- Route selection derived from Mihomo, including real flags and labels.
- Same-window diagnostics, keyboard access, redaction, and semantic state copy.
- A compact native footprint with no permanent sidebar.
- Xray and Mihomo as implementation details outside technical views.

### What should be retired

- A page title that consumes a full row but adds no orientation in a one-window app.
- Multiple large bordered cards with nearly equal emphasis.
- Uppercase state eyebrows as the primary status signal.
- A persistent list of every node on Home.
- Route group chips placed above a long node list. This turns Home into a selector browser.
- Diagnostic checks presented as equal tiles without explaining component ownership.
- Bright blue borders on every selected surface. Blue should identify action and focus, not decorate containers.
- Decorative status dots, glows, gradients, traffic charts, and fake telemetry.

### Reference interpretation

`fl8-old-visual` is useful as a functional inventory, not as a visual model. Its strongest lesson is that subscription, current node, network mode, traffic, and settings are all expected product capabilities. Its sidebar, dashboard grid, glow layers, repeated cards, charts, and per-feature color coding would overload a compact native client.

## Information architecture

The application should have four destinations. Navigation lives in a compact bottom bar because the window is narrow and the primary destinations are stable.

| Destination | User question | Contents |
| --- | --- | --- |
| Home | Am I protected, and where am I connected? | connection, current route, two compact selectors, subscription summary |
| Routes | Where should traffic go? | groups, nodes, search when needed, selection state |
| Activity | Is everything healthy? | runtime chain, diagnostics, recent redacted events |
| Settings | How should the app behave? | launch behavior, connection behavior, subscription management, app information |

Bottom navigation labels: `Главная`, `Маршруты`, `Состояние`, `Настройки`. Use one consistent monochrome icon family. The active item gets a subtle accent fill or 2 px indicator, not a glowing icon.

Home is intentionally simple:

1. Connection state and one primary action.
2. Current route summary.
3. Two compact selectors near the bottom: route group and node.
4. Subscription information in one quiet row.

Routes owns browsing. Activity owns diagnostics. Settings owns infrequent configuration. Home does not become a dashboard for the other three.

## Direction A: Native Control Center (recommended)

This is the best balance of clarity, Windows familiarity, and implementation fit.

### Composition

At 480 x 720:

```text
┌──────────────────────────────────┐
│ MultiCore                    ⋯   │  44
│                                  │
│        Соединение защищено       │
│       Germany [de] · 42 ms       │
│                                  │
│          [ Отключить ]           │  188-216
│                                  │
│ Маршрут                          │
│ Сервер                    [⌄]    │  48
│ Germany [de]              [⌄]    │  48
│                                  │
│ subscription.example 873.9 GB    │
│ Осталось 39 дней       Обновить  │  64
│                                  │
│ Главная Маршруты Состояние Настр.│  56
└──────────────────────────────────┘
```

The connection region is not another card. It is the page's open center, with a 64 px power control, a state headline, current node, and one action. The current route and subscription use compact list-row grammar below it. This produces a real hierarchy instead of a card stack.

### Interaction model

- Clicking the power control or its adjacent action changes connection state.
- The group and node rows open lightweight same-window selector sheets. They do not expand a long list inline.
- `Маршруты` opens the full browser for users who need to compare nodes.
- Subscription refresh remains visible but subordinate. URL replacement moves to Settings.
- The overflow menu contains only contextual low-frequency actions: `Скопировать диагностику`, `Открыть журнал`, `Выход`.

### Why it works

- A first-time user sees exactly one decision.
- Experienced users reach group and node selection in one click.
- The bottom of Home is stable even when connection copy changes.
- It adapts cleanly from 420 x 640 to 620 x 900 without inventing more content.

### Risk

The selector sheet needs careful keyboard focus and Escape behavior. This is a known, bounded Slint interaction rather than a new navigation architecture.

## Direction B: Connection Ledger

A denser, more technical direction for users who want immediate operational context.

### Composition

Home uses a two-column status ledger below a compact connection header:

```text
┌──────────────────────────────────┐
│ MultiCore                    ⋯   │
│ ● Подключено        [Отключить]  │
│ Germany [de]          42 ms      │
├──────────────────────────────────┤
│ Маршрут            │ Подписка    │
│ Сервер │ subscription.example    │
│ Germany [de]       │ 39 дней     │
├──────────────────────────────────┤
│ Цепочка: TUN › Mihomo › Xray     │
│ Все компоненты работают          │
└──────────────────────────────────┘
│ Главная Маршруты Состояние Настр.│
```

This direction surfaces the runtime chain on Home in a single quiet row. It is more informative without becoming a full dashboard.

### Interaction model

- Route and subscription cells open their destination pages.
- The runtime-chain row opens Activity.
- The connect action stays in the first visual band.
- No charts, memory counters, or transfer rates appear unless real, actionable requirements are approved.

### Strength

It explains the dual-core product earlier and makes degraded operation visible without opening diagnostics.

### Risk

It makes implementation terminology more prominent than most consumers need. It is better for a technical audience than a general VPN audience.

## Direction C: Quiet Command Bar

A highly compact direction that treats the app as a system utility.

### Composition

Home has a persistent command strip at the bottom above navigation:

```text
┌──────────────────────────────────┐
│ MultiCore                    ⋯   │
│                                  │
│          Можно подключиться      │
│   Трафик пойдет через Auto       │
│                                  │
│          [ Подключиться ]        │
│                                  │
│ subscription.example · 39 дней   │
│                                  │
│ [Сервер⌄] [Auto⌄]                │
│ Главная Маршруты Состояние Настр.│
└──────────────────────────────────┘
```

The two selectors become compact command buttons. The center remains visually empty and calm.

### Strength

This is the simplest and most recognizable appliance-like experience. It makes the product feel fast and focused.

### Risk

Long localized group and node names will strain two adjacent controls. The bottom command bar also needs a vertical fallback at the 420 px minimum width. It is less suitable if subscription metadata expands later.

## Recommendation

Choose Direction A, Native Control Center.

It removes the current card-stack feel while preserving every important capability. It keeps Home consumer-simple, gives routes and diagnostics dedicated places, and avoids exposing Xray/Mihomo terminology until it is useful. Direction B is a viable technical variant. Direction C is visually clean but less robust for Russian localization and long node labels.

## Visual system for Direction A

### Color

One dark theme for the current release. Keep a future light palette token-compatible, but do not mix themes inside the app.

| Semantic token | Proposed value | Use |
| --- | --- | --- |
| `canvas` | `#0D0F12` | app background |
| `surface-1` | `#15181D` | selector sheets and elevated rows |
| `surface-2` | `#1C2026` | hover and pressed surfaces |
| `stroke` | `#2A3038` | sparse boundaries |
| `text-primary` | `#F3F5F7` | state and primary labels |
| `text-secondary` | `#B8C0CA` | supporting copy |
| `text-muted` | `#7F8996` | metadata |
| `accent` | `#4C9DFF` | primary action, focus, selection |
| `success` | `#3CCB7F` | connected state only |
| `warning` | `#E7AA45` | pending and degraded only |
| `danger` | `#F06A6A` | destructive and failed only |

Rules:

- Accent is singular. Green is semantic status, not a second interaction accent.
- Avoid pure black, pure white, outer glow, gradients, and translucent glass.
- Borders appear only where they clarify control boundaries.
- Selection uses one accent fill plus a checkmark. Do not also add a glow and thick border.

### Typography

Use `Segoe UI Variable Text` throughout, matching Windows. Use `Segoe UI Variable Display` only if it is reliably available through the native stack; otherwise do not create a second family.

| Role | Size / line height | Weight |
| --- | --- | --- |
| Connection state | 24 / 30 px | 600 |
| Page title | 20 / 26 px | 600 |
| Section title | 14 / 20 px | 600 |
| Control label | 14 / 20 px | 500 |
| Body | 13 / 19 px | 400 |
| Metadata | 12 / 16 px | 400 |
| Numeric value | 16 / 22 px | 600, tabular figures where supported |

Avoid uppercase Russian labels. Use sentence case: `Подключено`, `Подключение`, `Не удалось подключиться`.

### Shape and spacing

- Base spacing: 4 px.
- Window inset: 20 px at 480 px width; 16 px at 420 px.
- Section gap: 24 px.
- Row height: 48 px.
- Primary button height: 44 px minimum, 48 px preferred.
- Bottom navigation: 56 px.
- Radius rule: 8 px controls, 12 px sheets and large surfaces, full circle only for the power control.
- No card inside card. Sheets can contain rows separated by spacing or one-sided dividers.

### Iconography

- Use one Windows-aligned outline family at a consistent optical 18-20 px size.
- Keep the existing custom power asset if its stroke matches the family.
- Flags are content, not decoration, and remain in dedicated 24-28 px tiles.
- No product logo in the content area. `MultiCore` may appear once in the title/utility row until a distinct app icon is approved.

### Motion

- 120 ms press feedback for buttons and rows.
- 160-200 ms opacity and vertical-offset transition for selector sheets.
- Connection busy state uses a determinate text transition or restrained spinner only when duration is unknown.
- No pulsing connected indicator, floating backgrounds, pointer tracking, or perpetual animation.
- Reduced-motion mode removes sheet translation and keeps an immediate opacity change.

## Screen contracts

### Home

The state message is the headline. Good copy examples:

| State | Headline | Supporting text | Action |
| --- | --- | --- | --- |
| Empty | `Добавьте подписку` | `Нужна ссылка с настройками MultiCore.` | `Добавить подписку` |
| Ready | `Можно подключиться` | `Маршрут: Auto` | `Подключиться` |
| Connecting | `Подключаемся` | `Настраиваем защищенный маршрут.` | disabled `Подключаемся` |
| Connected | `Соединение защищено` | `Germany [de]` | `Отключить` |
| Disconnecting | `Отключаемся` | `Возвращаем системную сеть.` | disabled `Отключаемся` |
| Degraded | `Нужно внимание` | `Один из компонентов не отвечает.` | `Проверить состояние` |
| Failed | `Не удалось подключиться` | Plain, redacted reason | `Повторить` |

Do not use `ГОТОВО`, `ПОДКЛЮЧЕНО`, or decorative state eyebrows.

### Routes

- Header: `Маршруты`.
- First control: group selector. Second: node list for the selected group.
- Search is hidden until the list exceeds a practical threshold such as 12 nodes.
- Rows show flag/icon, display name, optional real latency, and selection check.
- Unknown latency is blank or `Не измерено`, never invented.
- Disabled selection while disconnected explains: `Можно выбрать сейчас. Маршрут применится после подключения.` if queuing is supported.
- Empty group: `В этой группе нет доступных маршрутов.`

### Activity and diagnostics

Use an ownership chain instead of three equal metric cards:

```text
Системный трафик
  TUN              Работает
  Mihomo           Выбирает правила и маршрут
  Xray             Передает защищенный трафик
Интернет
```

Mapping:

| Layer | User-facing role | Technical owner | Healthy copy |
| --- | --- | --- | --- |
| Capture | Receives system traffic | Mihomo TUN | `TUN работает` |
| Policy | Applies rules and selected group/node | Mihomo | `Маршрут выбран` |
| Transport | Carries the final protected outbound | Xray | `Транспорт работает` |

The title `Состояние` is user-facing. `Диагностика` is a subsection or action. Technical names are visible here because they help support and troubleshooting.

Below the chain:

- `Проверить снова` as a secondary action;
- filter segmented control: `Все`, `Система`, `Ошибки`;
- redacted event list with component and timestamp;
- `Скопировать диагностику` with an explicit privacy note.

Error copy names the failing layer and next safe action. Example: `Mihomo не запустился. Проверьте права TUN и повторите подключение.` Do not show raw config paths, URLs, tokens, UUIDs, or controller output.

### Settings

Group settings by user intent, not implementation module.

`Запуск`:

- `Запускать MultiCore при входе в Windows`;
- `Подключаться автоматически`;
- helper copy: `Подключение начнется после загрузки сохраненной подписки.`

`Подписка`:

- safe source host;
- last successful update;
- usage and expiry;
- `Обновить сейчас`;
- `Заменить ссылку`;
- destructive `Удалить подписку` behind confirmation.

`Поведение окна`:

- `Сворачивать в область уведомлений`;
- `Закрытие окна не отключает VPN` with clear current behavior.

`О приложении`:

- app version, Xray version, Mihomo version;
- open-source notices;
- data and log locations only if the user can act on them.

Autostart states need explicit feedback:

- loading: control disabled with `Проверяем настройку Windows`;
- saved: inline `Настройка сохранена` announced politely;
- permission failure: `Windows не разрешила изменить автозапуск. Откройте параметры автозагрузки.`;
- unavailable: switch disabled with reason, never silently reverting.

### Subscription onboarding

Use a same-window focused page, not an extra card inserted into a dense Home layout.

- Title: `Добавить подписку`.
- Field label: `Ссылка на подписку`.
- Placeholder: `https://example.com/...`.
- Helper: `Ссылка хранится локально и не показывается в интерфейсе после сохранения.`
- Primary action: `Добавить`.
- Loading: `Проверяем подписку`.
- Invalid: `Не удалось прочитать подписку. Проверьте ссылку и попробуйте снова.`
- Partial dual-format failure: `Подписка неполная: не получены настройки Xray и Mihomo.`

## Responsive behavior

- 420-499 px: 16 px window inset, full-width primary action, bottom navigation labels may use 11 px but never disappear.
- 500-620 px: 20-24 px inset; Home remains one column. Extra width increases whitespace, not content count.
- 640 px height: subscription summary compresses to one line of metadata; no control is clipped. Routes remain on their own page.
- 900 px height: do not add fake metrics. Let the central connection region breathe or show one additional line of helpful state detail.
- Long route names elide in the closed selector and wrap to two lines in the full Routes list.

## State and accessibility requirements

- Visible keyboard focus is 2 px and does not depend on color alone.
- Tab order follows visual order. Selector sheet focus is trapped until commit or dismissal, then restored to its opener.
- Enter and Space activate buttons and rows. Escape closes sheets and returns to the invoking control.
- Every status includes text. Color never carries the meaning by itself.
- Busy state preserves layout dimensions to avoid visual jumps.
- A failed refresh keeps last-good subscription and route data visible.
- Destructive subscription removal names the consequence: `Подписка и сохраненные маршруты будут удалены. Активное соединение будет отключено.`
- Target contrast: WCAG AA for all labels and controls, including disabled explanatory text.

## Anti-slop acceptance checklist

- One interaction accent across every screen.
- No purple gradient, glow, glass, fake traffic chart, or decorative orb.
- No repeated logo in title, Home, and navigation.
- No three equal feature cards or equal diagnostic tiles.
- No decorative status dots. A dot is allowed only for actual live runtime state.
- No uppercase eyebrow above every section.
- No cards around content that is already grouped by spacing and headings.
- No raw Xray or Mihomo controls on Home.
- No invented latency, bandwidth, memory, uptime, or traffic values.
- No route browser embedded into Home.
- No motion without feedback or state-transition purpose.
- Empty, loading, success, pending, degraded, and error states are specified.
- Russian copy is plain, short, and consistent in register.

## Decision needed before implementation

Approve one direction and decide whether bottom navigation is acceptable for a Windows desktop utility. If bottom navigation is rejected, Direction A can use a compact top tab strip with the same four destinations, but this should be a deliberate product choice rather than a styling change made during implementation.
