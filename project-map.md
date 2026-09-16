# Project Map
_Updated: 2026-09-16 | Git: unborn branch_

## Directory Structure
packaging/windows-x64/ — pinned upstream-core manifest, license texts, notices, and portable-package README.
apps/multicore-desktop/ — native Slint controller and safe loopback daemon client.
crates/multicore-core/ — subscription validation, immutable snapshots, runtime publication, and sidecar supervision.
crates/multicore-daemon/ — authenticated local API, transactional orchestration, catalog, selection, and redacted events.
docs/ — research, approved specifications, implementation plans, and visual rules.
scripts/ — packaging, one-click smoke verification, secret-free visual fixtures, and deterministic Windows capture tooling.

## Key Files
docs/specs/2026-09-15-one-click-bootstrap-design.md — approved portable package, daemon bootstrap, Job Object, and failure contract.
docs/plans/2026-09-15-one-click-bootstrap.md — TDD plan for one-click startup and bundled pinned cores.
apps/multicore-desktop/src/bootstrap.rs — package validation, random local token, at-creation Job Object launch, readiness/status wait, and cleanup.
scripts/package-windows-release.ps1 — pinned Rust build, private-path remapping, exact-pin download, safe ZIP extraction, PE validation, and fail-closed publication.
scripts/smoke-test-one-click.ps1 — exact-window normal-close and forced-crash GUI/bootstrap/authentication/process-cleanup smoke tests.
README.md — build/run guide and current v1 boundaries.
project-map.md — persistent orientation and non-obvious project constraints.
session-log.md — durable architectural decisions and rejected approaches across sessions.
docs/research/2026-09-14-dual-core-client-market.md — market landscape, validated dual-core architecture, feature priorities, and source inventory.
docs/specs/2026-09-14-minimal-ux-subscriptions-design.md — one-screen UX, URL schemes, dual-UA subscription contract, diagnostics, and failure handling.
docs/specs/2026-09-15-quiet-signal-redesign.md — historical flat redesign, superseded after user review by the FL8-inspired interaction direction recorded in `docs/design/visual-system.md`.
docs/plans/2026-09-15-quiet-signal-redesign.md — completed Quiet Signal implementation and release-verification checklist.
apps/multicore-desktop/ui/app.slint — native Control Center with one-page Home routes plus dedicated Status and Settings pages.
apps/multicore-desktop/src/windows_settings.rs — exact-path current-user Windows launch-at-sign-in integration.
apps/multicore-desktop/src/view_model.rs — presentation state, mutation gates, reconciliation, redaction, and UI contract tests.
crates/multicore-daemon/src/lib.rs — authenticated API and transactional Mihomo catalog/selection backend.
crates/multicore-core/src/network.rs — native Windows default-route/interface selection used by the pre-TUN Xray bootstrap.

## Critical Constraints
- Normal Windows startup requires only `MultiCore.exe`; it owns a hidden packaged daemon and descendants through a kill-on-close Job Object.
- WinAPI process-attribute values must outlive `CreateProcessW`; the Job-list handle array is intentionally stored through the call because a temporary slice produced release-only `ERROR_INVALID_HANDLE`.
- The smoke test must enumerate the visible `MultiCore` window by exact PID/title; `Process.MainWindowHandle` selects Winit's service window on this host and does not test a real user close.
- Production packaging builds in a fresh private target directory with Cargo/rustc 1.98.1 and rejects binaries containing the builder's private absolute paths.
- Core archives are pinned to exact official release URLs and archive/executable SHA-256 values; runtime never downloads cores.
- Planned product combines Xray as the outbound/tunnel engine with Mihomo as the policy, routing, and selector engine.
- Desktop-first client should avoid a mandatory WebView and favor a Rust-native application shell where practical.
- Mihomo is the sole owner of TUN, DNS, process routing, rules, and selectors; Xray is the FinalMask-capable outbound transport plane.
- Xray and Mihomo remain separate sidecar processes connected through loopback SOCKS/API boundaries.
- The normal UI exposes one contextual connect button; core details, logs, and typed overrides stay behind `Подробнее`.
- Subscription refresh tries `multicore-json-massive` and stops after a successful HTTP response. An unavailable or unsupported massive endpoint falls back to the atomic Mihomo YAML (`multicore-mihomo`) + Xray JSON (`multicore-xray`) pair.
- Xray subscription JSON stays byte-exact in the protected snapshot. Before every Connect, only the ephemeral launch copy is minimally overlaid with client-local `dns.hosts`, `sockopt.domainStrategy: ForceIP`, and the current Windows default `sockopt.interface`; FinalMask and sibling fields remain intact.
- Xray endpoint domains must be resolved before Mihomo TUN starts. Native Windows IP Helper route discovery and the local resolver are rerun on every Connect; failure aborts before either core launches while the saved profile/UI remain available.
- The provider creates matching named Xray SOCKS inbounds and Mihomo SOCKS proxies; the client never compiles or fingerprints that bridge.
- Accepted subscription payloads are strictly Mihomo YAML plus Xray JSON; no share-link, base64, Clash JSON, sing-box, or converter support.
- The native frontend is Slint; no WebView or browser UI is part of v1.
- The current visual direction is the native Control Center in `PRODUCT.md` and `DESIGN.md`: restrained dark surfaces, a three-item icon rail, one explicit connection action, and route groups/nodes inline on Home; health/mapping/logs and settings remain secondary pages.
- The shell avoids a large logo/header and exposes only one small product label in the rail. Subscription URL replacement stays collapsed until requested.
- Ready-state route choices are stored per group; connected selections are confirmed against Mihomo's live controller state and existing controller connections are closed after a confirmed change.
- Diagnostics mapping is derived only from bounded literal-loopback `socks5` entries in Mihomo YAML; Xray JSON remains opaque and byte-exact.
- Route flags and semantic emoji use the embedded `Twemoji.Mozilla.ttf`; Slint release builds must use `EmbedResourcesKind::EmbedFiles` so the portable client has no external font dependency.
- Deterministic selection capture sends client-coordinate mouse messages directly to the exact desktop HWND; global cursor injection is forbidden because it is focus-racy across sequential scenarios.
- Daemon catalog labels preserve bounded common emoji/flag ranges but still genericize URL, credential, UUID, control, and unsafe arbitrary-punctuation names; the desktop derives display-only icon tiles without changing opaque selection IDs.
- Desktop control traffic is literal-loopback HTTP only, with proxy/redirect disabled and bounded decoded bodies.
- Runtime publication stages new controller files before durable commit and preserves the old launchable generation on abort.
- Core stdout/stderr is redacted before both the bounded diagnostics feed and `%LOCALAPPDATA%\MultiCore\logs\latest-core.log`; disk retention is capped at 1 MiB and failure falls back to memory without blocking startup.

## Hot Files
apps/multicore-desktop/ui/app.slint, apps/multicore-desktop/ui/components.slint, apps/multicore-desktop/src/view_model.rs, crates/multicore-daemon/src/lib.rs, crates/multicore-core/src/snapshot.rs
