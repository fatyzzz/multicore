# Windows shell, tray, resize, and durable state design

## Goal

Make MultiCore behave like a native Windows network utility: the frameless window can be resized, minimizing and closing reliably move it to the tray, Windows autostart stays hidden, and user preferences survive ordinary restarts and application updates.

This design also permits one restrained ambient background treatment without weakening readability or keeping unnecessary animation alive while the window is hidden.

## Scope

- Native resize handles for the existing custom frame.
- Minimize-to-tray, close-to-tray, restore, and explicit Exit semantics.
- Hidden Windows autostart through the existing `--background` launch mode.
- Versioned, atomic persistence for desktop preferences and window placement.
- An updater preservation contract for `%LOCALAPPDATA%\MultiCore`.
- A low-motion background wave prototype behind the Home canvas.

Multi-subscription storage and APIs are specified separately in `2026-09-17-multi-subscription-profiles-design.md`.

## Window behavior

The application keeps its custom title bar and frameless window. Eight transparent hit regions cover the four edges and four corners. They invoke Winit's native drag-resize operation, with corners taking priority over edges and the title-bar drag area beginning inside the resize border.

- Minimum client size: `700 × 620` logical pixels.
- Initial size: the last valid size, otherwise `840 × 720`.
- Maximum size: the current monitor work area rather than a fixed application constant.
- Restored placement is clamped to the union of current monitor work areas. A window saved on a disconnected monitor reappears on the primary monitor.
- Maximized state is persisted separately from restored bounds.
- Bounds are persisted after resize/move settles, not on every pointer event.

## Tray and process lifetime

- Title-bar minimize hides the window when the tray exists; it falls back to ordinary taskbar minimization if tray creation failed.
- Title-bar close and native close-request hide the window when the tray exists.
- Tray `Открыть MultiCore` restores, unminimizes, focuses, and redraws the existing window.
- Tray `Выйти` is the only ordinary action that terminates the process and therefore the owned daemon, Mihomo, and Xray descendants.
- A manual launch shows the window.
- Windows launch-at-sign-in continues to register `"<absolute exe>" --background` and starts with only the tray visible.
- If background launch cannot create a tray, the main window is shown. The application must never run with no visible recovery surface.
- A second foreground launch activates the existing instance. A second background launch does not steal focus.

## Durable preferences

Desktop-only state is stored at `%LOCALAPPDATA%\MultiCore\preferences.json` with an explicit schema version. Writes use a same-directory temporary file, flush, atomic rename, and owner-only permissions. A malformed file is quarantined and safe defaults are used.

Persisted fields:

- restored window position and size;
- maximized state;
- last visible page;
- active subscription ID;
- last visible selector group per subscription;
- confirmed selector choices per subscription and group;
- ambient background enabled state;
- future user-facing tray preferences.

Not persisted:

- connected/connecting/disconnecting state;
- pending mutations, inline errors, latency probes, health values, or logs;
- focus/hover state;
- credentials outside the existing private subscription records.

On restart, the daemon remains authoritative. MultiCore starts disconnected unless a future explicit `connect at startup` setting is introduced. The client never infers autoconnect from a previously connected session.

## Update preservation contract

Installed program files and mutable application data remain separate. The updater may replace only the validated installation payload and its own `%LOCALAPPDATA%\MultiCore\updates` staging directory. It must not enumerate, move, rewrite, or delete profile stores, `preferences.json`, device identity, logs, or snapshots.

An integration test places sentinel preferences, a credential-bearing test subscription URL, and selector state in a temporary local-app-data root, applies an update fixture, restarts the client, and proves the sentinels are preserved without appearing in updater logs.

## Ambient background

The accepted prototype uses two broad gray-white vector waves behind all Home content:

- opacity between `0.02` and `0.04`;
- travel no greater than `10px` over `12–18s`;
- no blur bloom, particles, scanlines, or saturated color;
- no motion while the window is hidden or inactive;
- static rendering when Windows client-area animation is disabled;
- no hit testing and no effect on layout.

The effect ships only if native captures preserve text/card contrast and it does not visually compete with the connection-pill mesh. Otherwise the wave layer is removed while the shell work ships unchanged.

## Error handling

- Tray unavailable: show the window and use taskbar minimize/real close.
- Native drag-resize failure: leave the current bounds unchanged; never synthesize repeated pointer moves.
- Invalid saved geometry: ignore it and center the default size on the primary work area.
- Corrupt preferences: quarantine, reset to defaults, and keep profiles untouched.
- Preference write failure: keep the in-memory setting and report a bounded settings error without blocking network operation.

## Testing

- Pure tests for close/minimize/startup dispositions and geometry clamping.
- Source/native tests for all eight resize regions and title-bar priority.
- Windows integration test for foreground/background single-instance activation.
- Tray tests for hide, restore, Exit, and tray-unavailable fallbacks.
- Persistence tests for atomic writes, corruption recovery, schema migration, and no transient-state persistence.
- Updater preservation test with a sentinel subscription URL and preferences.
- Deterministic captures at minimum, default, wide, and maximized sizes.

## Failure-mode review

- **Critical:** hidden launch without a tray would strand the user. The fallback always shows the window.
- **Critical:** saved bounds from a removed monitor would make the window unreachable. Restore always clamps to current work areas.
- **Critical:** updater scope creep could erase subscription URLs. The updater receives no mutable-data path and is tested with sentinels.
- **Minor:** the ambient effect may look decorative or consume paint time. It is isolated, pausable, and removable without affecting shell behavior.

## Non-goals

- Automatic reconnect after restart.
- Persisting live logs or runtime health as if they were current.
- Replacing the custom title bar with native Windows chrome.
- Making animation necessary to understand any state.
