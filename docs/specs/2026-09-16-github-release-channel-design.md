# GitHub Release Channel Design

## Scope

- Publish the source repository at `fatyzzz/multicore`.
- A tag workflow builds the pinned portable bundle with `MULTICORE_UPDATE_REPOSITORY=fatyzzz/multicore`, the exact updater ZIP, and the Inno Setup installer.
- Releases include source/license material required by the bundled pinned cores.
- The desktop continues to accept only the exact `multicore-windows-x64.zip` asset with GitHub's SHA-256 digest and bounded contents.

## Failure modes

- A tag/version mismatch fails before publishing.
- Missing digest, wrong repository URLs, prereleases, or malformed archives remain rejected by the client.
- Unsigned binaries may trigger SmartScreen; code signing is not fabricated and remains an explicit release limitation.
