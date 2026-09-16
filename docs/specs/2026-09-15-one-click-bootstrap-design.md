# One-click Windows bootstrap design

## Outcome

The portable Windows package starts by double-clicking `MultiCore.exe`. The user does not open a terminal, set environment variables, start the daemon, or install Xray/Mihomo separately.

## Package contract

```text
multicore-windows-x64/
  MultiCore.exe
  runtime/multicore-daemon.exe
  cores/xray.exe
  cores/mihomo.exe
  SHA256SUMS.txt
  THIRD_PARTY_NOTICES.md
  licenses/Xray-core-MPL-2.0.txt
  licenses/mihomo-GPL-3.0.txt
  versions.json
  README.md
```

`versions.json` pins the upstream repository, full commit, tag, exact asset URL/name/entry, archive SHA-256, extracted executable SHA-256, and immutable source URL. The release script refuses a digest mismatch. `SHA256SUMS.txt` excludes itself and lists every other staged file in ordinal relative-path order with `/` separators and lowercase hashes.

The pinned upstream releases, resolved from the official GitHub releases on 2026-09-15, are:

- Xray-core `v26.9.9`, commit `52a412d9e2f5c2a5142b1b4e2ab3771dacb8b120`, `Xray-windows-64.zip`, archive SHA-256 `244deaba2098c2964e49bba90df3707777e5f5f428a82d2f29604015f24beec2`, executable SHA-256 `0d0fc0ea2b05641acb78c01fc36ad694e7b029861b2d5eb93da0e3e9fda9a98f`;
- Mihomo stable `v1.19.30` commit `ac017cdd246ce8bd547653d927e7bf77d7ee73d5`, `mihomo-windows-amd64-compatible-v1.19.30.zip`, archive SHA-256 `289fde5e29d37a5b3326480590d8b3551c5bf7f8737290355c19bce74d57a563`, executable SHA-256 `6ac25fcb26afe8e1bea24b6e6e80805bf884a33232d12e2d78dfa0b6c529ac14`.

The compatible Mihomo amd64 build is selected so the package does not silently require newer x86-64 instruction levels.

## Startup flow

1. In packaged mode, `MultiCore.exe` resolves its own directory and validates that the daemon and both cores are regular PE AMD64 files contained by the package tree before spawning anything. A complete external-daemon override intentionally bypasses packaged-file validation.
2. It creates `%LOCALAPPDATA%\MultiCore` and a cryptographically random daemon bearer token. The token is passed only through the child environment and is never written or logged.
3. It creates a non-inheritable kill-on-close Windows Job Object before process creation, starts `runtime\multicore-daemon.exe` in that job without a console window, requests `127.0.0.1:0`, and captures only the child's bootstrap stdout pipe. Job membership exists from process creation, so no unowned execution window exists.
4. The daemon fully constructs its authenticated router, binds a kernel-selected loopback port, writes and flushes one bounded ASCII readiness line containing only its actual nonzero socket address, then serves its API.
5. The desktop parses the readiness line, constructs its no-proxy/no-redirect loopback client, and verifies `/v1/status` with the random token before enabling normal UI work.
6. A single monotonic startup deadline covers bounded readiness parsing and authenticated probing. If the daemon exits early or startup times out, the existing inline error state shows a short actionable Russian message. Secrets and absolute private paths are omitted.
7. On normal desktop exit the owned daemon is terminated and reaped. On Windows it is additionally assigned to a kill-on-close Job Object so a desktop crash cannot leave the daemon or its Xray/Mihomo descendants running.

## Developer override

The current external-daemon workflow remains available only when both `MULTICORE_DAEMON_URL` and `MULTICORE_DAEMON_TOKEN` are non-empty. Supplying just one is rejected as a configuration error. Normal packaged startup ignores neither core validation nor authentication.

## Security boundaries

- All control listeners must resolve to literal IP loopback addresses; hostnames and non-loopback addresses remain rejected.
- Child processes are spawned by exact filesystem paths with argument arrays, never through a shell. Bootstrap-only environment variables, especially the bearer token, are removed before the daemon launches Xray or Mihomo.
- The readiness line contains no credential, is accepted only from the directly spawned child's pipe, and must contain a literal loopback socket address.
- Release archives are downloaded only from pinned official GitHub URLs and accepted only after SHA-256 verification.
- Xray JSON remains opaque to catalog/UI and byte-exact in durable storage. The ephemeral launch copy receives only the client-local DNS bootstrap and Windows interface binding required to keep Xray endpoint resolution outside the Mihomo TUN loop.

## Failure behavior

Missing, non-regular, wrong-architecture, or package-escaping executable paths, an invalid partial developer override, an early daemon exit, malformed/oversized readiness output, a non-loopback or zero-port readiness address, authentication failure, and startup timeout all produce a usable window with an inline retry/error state. They do not panic, open a console, or expose a token. Authenticity of an unsigned portable folder itself is outside this design; archive digests protect the build input, not a folder modified after publication.

## Non-goals

This slice does not add an installer, code signing, automatic core updates, a Windows service, or a dashboard. It produces a self-contained portable folder with one user-facing executable.
