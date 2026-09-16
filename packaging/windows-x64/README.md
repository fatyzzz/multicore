# MultiCore Windows x64 portable preview

Next to `MultiCore.exe`, this file describes the generated portable payload; in
the source tree it also serves as input to `scripts/package-windows-release.ps1`.
The portable folder starts with `MultiCore.exe` and contains only the
desktop, its private daemon and update helper, the two pinned upstream cores,
notices, licenses, the version manifest, and checksums.

This packaging flow supports local previews and can emit the full-bundle ZIP
consumed by the client updater. It is not an installer, a cryptographically
signed release channel, or a complete public redistribution workflow. Before distributing
a package publicly, preserve immutable corresponding-source artifacts for every
copyleft component, verify their hashes independently, satisfy the MPL-2.0 and
GPL-3.0 obligations, and obtain appropriate legal review.

The pinned core release assets are downloaded only by the developer packaging
script. Runtime updates replace the complete verified portable package; neither
core is independently updated. Content staging is
deterministic while each pinned upstream asset remains available; this is not a
claim that the upstream executables are reproducible compiler outputs.

For a local preview, keep the generated tree intact and double-click
`MultiCore.exe`. Runtime state is stored separately under `%LOCALAPPDATA%\MultiCore`.
The latest daemon session's redacted core output is retained at
`%LOCALAPPDATA%\MultiCore\logs\latest-core.log` and capped at 1 MiB.
Verify `SHA256SUMS.txt` before use; it covers every package file except itself.

## GitHub Releases update asset

Build the desktop with a compile-time-pinned repository and emit the exact asset
name expected by the updater:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts\package-windows-release.ps1 `
  -DestinationPath dist\multicore-windows-x64 `
  -UpdateRepository owner/repository `
  -ReleaseAssetPath dist\release\multicore-windows-x64.zip
```

Publish `multicore-windows-x64.zip` on a stable GitHub Release tagged
`v<major>.<minor>.<patch>`. The updater requires GitHub's `sha256:` asset digest,
then verifies the archive's internal `SHA256SUMS.txt` before directory replacement.
Do not enable a public update channel until release publishing credentials,
corresponding-source obligations, and signing policy are ready.
