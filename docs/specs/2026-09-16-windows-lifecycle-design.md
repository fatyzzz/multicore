# Windows Lifecycle Design

## Scope

- Open the native tray menu on one left or right click.
- Keep close-to-tray and explicit Exit behavior.
- Register launch-at-sign-in as `"<installed exe>" --background`.
- In background mode create the tray and event loop without showing the main window.
- Re-read the Run value after every write/delete before reporting success.

## Failure modes

- Tray creation failure falls back to ordinary close/exit and a visible client.
- A moved portable executable is classified stale and can be repaired by toggling autostart.
- Unknown command-line arguments do not silently enable background mode.
