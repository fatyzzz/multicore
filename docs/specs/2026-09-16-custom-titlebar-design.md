# Custom title bar design

## Decision

MultiCore will use a 44 px frameless Slint title bar that belongs to the current Control Center visual system. It replaces the native Windows caption without creating a second application header.

The bar contains:

- a compact `MULTICORE` label on the left;
- a flexible drag region in the middle;
- a small update-available indicator inside the drag region;
- 44×44 minimize, maximize/restore, and close controls on the right.

The drag region starts a native Winit window drag. Double-click toggles maximize. The controls expose keyboard focus and accessible labels. The close button follows the existing graceful Slint event-loop shutdown so the bootstrap guard still owns daemon cleanup.

## Alternatives considered

1. Keep the Windows caption and recolor it. Lowest engineering cost, but it remains visually detached from the app and cannot carry the update signal.
2. Reimplement moving/resizing with raw Win32 messages. It offers total control but duplicates Winit behavior and increases DPI, snap, and multi-monitor failure risk.
3. Use a frameless Slint bar and delegate movement/maximize/minimize to Slint/Winit. Chosen: it matches the native stack and keeps operating-system window behavior.

## Scope and non-goals

- The existing navigation rail and page layout remain intact below the bar.
- The title bar does not become a branding hero, toolbar, or status dashboard.
- This change does not redesign the application logo.
- Windows snap/resizing remains the responsibility of Winit; MultiCore does not emulate resize borders.

## Failure modes

- If Winit is not the selected backend, native drag is unavailable. The desktop build already pins `backend-winit`; the callback fails harmlessly while window buttons still work through Slint.
- A frameless window can lose native resize affordances on unsupported backends. Windows x64 is the supported release target and receives a dedicated smoke/build check.
- A drag callback attached over caption buttons would steal clicks. The drag `TouchArea` is limited to the flexible middle region and never overlaps controls.

## Verification

- Slint component compilation proves callbacks and layout are valid.
- Rust unit tests cover the maximize-state transition helper.
- Deterministic preview capture checks the 820×720 shell and the 700 px minimum width.
- One-click smoke still closes the visible window gracefully and leaves no packaged child process.
