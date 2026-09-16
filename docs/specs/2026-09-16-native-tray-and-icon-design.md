# Native Tray and App Icon Design

## Goal

Make MultiCore feel like a real Windows network utility: one recognizable app mark, a useful tray surface, and no orphaned status text in the navigation rail.

## Behavior

- The close button hides the window while the tray is available; the explicit tray `Выйти` action terminates the application and its owned daemon.
- The tray shows the current runtime status, one contextual connect/disconnect action, route groups, nodes for the selected group, `Открыть MultiCore`, and `Выйти`.
- Tray route actions invoke the same Slint callbacks as the visible UI. No second selection implementation or direct daemon shortcut is introduced.
- A left-button double click on the tray icon restores the window.
- The packaged smoke test sets a private test-only environment flag so `WM_CLOSE` still verifies graceful process cleanup.

## Visual direction

- Replace the font `×` with a deterministic vector cross.
- Remove the bottom `ГОТОВО / В СЕТИ` label from the navigation rail; connection state remains in the primary surface and tray tooltip.
- Use a compact dual-core link mark: two offset rings connected as one route. It must remain legible at 16 px and use the existing blue/neutral palette.
- The same mark is used by the Slint window, title bar, Windows executable resource, and tray icon.

## Failure handling

- Failure to create a tray icon must not prevent the client from starting; close falls back to terminating the application.
- Menu construction is bounded by the already bounded daemon catalog.
- Unknown or stale menu identifiers are ignored.
