# GitHub Releases updater design

## Decision

MultiCore will check one compile-time-pinned GitHub repository for the latest stable release. It updates the complete Windows portable bundle, never an individual executable.

The repository is supplied at build time as `MULTICORE_UPDATE_REPOSITORY=owner/repository`. Local builds without that setting remain functional and show that the release channel is not configured. Users cannot edit the repository in the UI.

## Release contract

- Release tag: `v<semver>`.
- Asset name: `multicore-windows-x64.zip`.
- Archive content: the portable payload at the ZIP root, including `MultiCore.exe`, `runtime/multicore-daemon.exe`, `runtime/multicore-updater.exe`, both cores, notices, licenses, `versions.json`, and `SHA256SUMS.txt`.
- GitHub's release asset `digest` must be present as `sha256:<64 lowercase hex>` and must match the downloaded archive.
- `SHA256SUMS.txt` must cover every other regular archive file exactly once; unlisted files, duplicate paths, links, absolute paths, traversal, and oversized archives are rejected.

## User flow

1. After the window opens, the desktop performs one bounded background check. Settings also exposes `Проверить`.
2. If a newer stable version exists, Settings shows the version and an `Обновить` action; the title bar shows only a quiet blue dot.
3. The action downloads to `%LOCALAPPDATA%\MultiCore\updates`, verifies the GitHub digest, then copies the updater helper to that staging directory.
4. The helper is started with the current process id, staged archive, expected digest, and current portable directory. The desktop requests its normal graceful exit.
5. The helper waits for the desktop to exit, safely extracts and validates the full payload beside the current directory, renames the current directory to a backup, publishes the new directory, and launches the new `MultiCore.exe`.
6. A failed extraction or publish restores and relaunches the old directory. A successfully replaced previous package is retained beside the current package for manual recovery.

## Security boundaries

- Only `https://api.github.com/repos/<pinned owner>/<pinned repo>/releases/latest` is queried.
- Redirects and decoded bodies are bounded. The download must originate from GitHub's release asset URL and match the API-provided size and digest.
- Update application is refused while the client is connected or mutating runtime state.
- The helper derives no commands from the archive and never invokes PowerShell, cmd, or an installer script.
- The target directory is canonicalized and must contain the running `MultiCore.exe` plus `SHA256SUMS.txt`; drive roots and arbitrary directories are rejected.

Transport digest verification protects against corruption and a mismatched asset. It does not protect against compromise of the pinned GitHub repository or its publishing credentials. Authenticode or an embedded offline signing key remains required before calling the public channel cryptographically signed.

## Alternatives considered

1. Replace only `MultiCore.exe`. Rejected because daemon, updater, cores, notices, and manifest can become version-skewed.
2. Download and run a release installer. Rejected because the current product is portable and an opaque installer expands the trust boundary.
3. Full-bundle staged replacement with a small Rust helper. Chosen because it is atomic at the directory boundary, testable, and preserves rollback.

## Failure modes

- No repository is configured: the UI reports an unavailable channel and all VPN functions continue normally.
- GitHub is offline, rate-limited, or returns malformed metadata: last-known application state is untouched and the user can retry.
- The archive is corrupt or malicious: digest, path, inventory, size, and PE checks reject it before the install directory changes.
- The install directory is not writable or files are locked: publication aborts; if the old directory was already renamed, it is restored.
- The app is connected: installation is disabled until disconnect, so the helper never races live cores/TUN.

## Testing

- Pure tests cover repository/tag parsing, version comparison, release selection, digest parsing, and update state transitions.
- Archive tests cover valid bundles plus traversal, duplicate, extra, missing, bad-hash, and oversize cases.
- Helper integration tests apply a fixture bundle and force a publish failure to prove rollback.
- Packaging tests require the updater executable, create the release ZIP, and verify its digest and internal checksum inventory.
- Network tests use a local HTTP fixture; CI does not depend on live GitHub.
