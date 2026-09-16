# Windows Installer Design

## Scope

- Produce an Inno Setup per-user installer for the verified portable directory.
- Install under `{localappdata}\Programs\MultiCore` so the transactional updater can replace files without elevation.
- Create Start Menu and optional Desktop shortcuts, an optional background autostart task, and a normal uninstaller.
- Preserve user profiles/logs on uninstall unless the user explicitly removes them later.

## Failure modes

- Installer build fails closed if the package inventory or compiler path is invalid.
- Installing over a running client prompts for closure; it does not replace live binaries blindly.
- TUN privilege requirements remain runtime concerns and are not hidden by the installer.
