# Native Tray and App Icon Implementation Plan

**Goal:** Add one consistent app icon and a functional Windows tray without duplicating connection or route-selection logic.

### Task 1: Lock pure behavior

- [x] Add failing tests for branded RGBA output and tray menu/command mapping.
- [x] Implement the smallest pure icon renderer and tray model required by the tests.

### Task 2: Integrate the native shell

- [x] Add the pinned Windows-only tray dependency and verify the desktop suite/build.
- [x] Add the tray runtime, same-callback actions, hide/restore lifecycle, and smoke-only close override.
- [x] Embed the multi-resolution icon in `MultiCore.exe` and set the Slint window icon.

### Task 3: Polish and verify

- [x] Replace the typographic close glyph and remove the rail status.
- [x] Run format, workspace tests, strict Clippy, release build, package checksum verification, and one-click smoke. Visual review remains available for the next UI iteration.
