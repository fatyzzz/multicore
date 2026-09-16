# Runtime Health and Diagnostics Design

## Scope

MultiCore must report a protected connection only after its own Xray process, Mihomo controller, and uniquely named Windows TUN adapter are ready. The existing Diagnostics panel will become a compact runtime dashboard and show bounded, redacted logs from both cores.

This design does not stop, reconfigure, inspect secrets from, or otherwise manage FlClashX. It also does not use a public internet request as proof of health because another VPN can satisfy that request.

## Premise

The work is necessary: the current process launcher discards both core output streams, and `Supervisor::connect` returns success as soon as both child processes spawn. A running FlClashX adapter therefore makes an ordinary connectivity probe ambiguous while MultiCore can still claim `Connected` without owning a working TUN.

## Considered approaches

1. **Public connectivity probe only.** Smallest change, but invalid when FlClashX is active and therefore rejected.
2. **External monitoring service and persistent log database.** Powerful but disproportionate for a local two-core client and would complicate packaging and secret handling.
3. **Core-local readiness gates plus bounded local diagnostics.** Chosen. It verifies resources owned by MultiCore, keeps the UI feed in memory, and retains only the latest daemon session in one redacted bounded text file for post-exit debugging.

## Architecture and data flow

Runtime publication overlays the subscription's Mihomo `tun.device` with the constant `MultiCore`. Before every Connect it also resolves domain-valued Xray server endpoints while TUN is down, merges those IPs into ephemeral Xray `dns.hosts`, forces endpoint resolution with `sockopt.domainStrategy: ForceIP`, and binds outbound sockets to the current connected Windows IPv4 default interface. It preserves all routing, selectors, proxies, FinalMask, and sibling settings supplied by the backend; the protected subscription snapshot remains byte-exact.

`SidecarProcessController` captures stdout and stderr into a shared bounded ring buffer. Every line is length-limited and passed through the existing credential/URL redactor before storage. The same sanitized record is written to `%LOCALAPPDATA%\MultiCore\logs\latest-core.log`, capped at 1 MiB and replaced when the next daemon session starts. A file-open or write failure disables only disk retention and leaves connection plus the in-memory dashboard operational. It never serializes the Mihomo controller secret or subscription URL.

An actual sidecar `start` does not complete at `spawn`:

- Xray must remain alive and expose every loopback inbound port declared by the generated Xray JSON.
- Mihomo must remain alive, expose its loopback controller port, and on Windows have an operational adapter named `MultiCore`.
- A failed or timed-out readiness gate terminates only the just-started MultiCore sidecar. Supervisor rollback then stops the other MultiCore sidecar. FlClashX is outside the process controller and untouched.

The daemon exposes `GET /v1/diagnostics`, authenticated like the existing local API. It returns Xray/Mihomo/TUN check states and the latest safe core log lines. No configuration payloads, credentials, raw subscription URLs, or controller secrets are returned.

The desktop loads this endpoint when Diagnostics opens. Three small health cards appear above the existing event list, followed by filters and a scrollable combined log/event feed. The main page remains unchanged except that `Connected` is now reachable only after readiness passes.

## Interfaces

Core diagnostics contain:

- `xray`: `stopped | starting | ready | failed`
- `mihomo`: `stopped | starting | ready | failed`
- `tun`: `missing | ready | failed | unsupported`
- bounded log records with monotonic ID, timestamp, engine, severity, and redacted message

The local daemon DTO uses stable snake_case JSON fields. The desktop treats malformed diagnostics as a safe error row without discarding previously loaded logs.

## Error handling

Readiness timeout is an ordinary connection failure, never a successful/degraded green state. The user sees a short safe message on the main screen and the concrete redacted core output in Diagnostics. If rollback fails, the existing degraded-state behavior remains.

## Failure-mode check

- **Critical: another VPN satisfies an internet probe.** Resolved by never using public connectivity as ownership proof.
- **Critical: adapter-name collision.** Resolved by forcing `tun.device: MultiCore` and checking exactly that adapter. If another process already owns that name, readiness fails instead of claiming protection.
- **Critical: core blocks while nobody drains its pipe.** Resolved by starting continuous stdout/stderr readers immediately after spawn and bounding only retained records, not reads.
- **Critical: diagnostics leak credentials to disk.** Resolved by persisting only the already-redacted, line-normalized record, truncating the file on daemon start, and enforcing a 1 MiB cap.
- **Minor: non-Windows builds cannot verify a Windows adapter.** They report TUN verification as unsupported; the current packaged target is Windows x64.
- **Minor: a core can fail after the initial gate.** Diagnostics reports the current child state whenever opened; continuous automatic disconnect/recovery is outside this change.

## Testing

- Runtime publication tests prove the unique TUN overlay and snapshot immutability.
- Process-controller tests prove early exit/readiness timeout rejection, bounded redacted memory/disk logs, disk-failure fallback, and successful loopback readiness.
- Daemon API tests prove authenticated diagnostics JSON and absence of secrets.
- Desktop model/UI tests prove health mapping, filters, safe failures, and compact dashboard structure.
- Full workspace tests, formatting, Clippy, preview captures, packaging, and one-click smoke tests run before release handoff.

## Rollout

No data migration is required. Existing snapshots remain byte-exact; only new ephemeral runtime generations receive the Mihomo TUN and Xray DNS/interface overlays. A saved profile can load without a route; missing interface or endpoint DNS fails the subsequent Connect before either core launches. Closing the prior MultiCore build before packaging is required only to release its binaries; FlClashX remains running throughout.
