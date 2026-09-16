# Quiet Signal Native UI Implementation Plan

> Historical plan. The user subsequently rejected the flat Quiet Signal result and re-approved the earlier `fl8-old-visual/src`-inspired interaction direction. Current behavior is documented in `docs/design/visual-system.md`.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers-optimized:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the rejected MultiCore presentation with the approved Quiet Signal one-screen native interface without changing daemon behavior.

**Architecture:** Keep the existing Rust view model and daemon client contracts. Rebuild the Slint layer around responsive layouts, repository-owned controls, inline subscription and route regions, and one optional diagnostics details surface. Visual fixtures exercise real serialized DTOs through the loopback client.

**Tech Stack:** Rust 1.98.1, Slint 1.17, SVG, PowerShell visual fixture/capture scripts.

**Assumptions:** MultiCore stays a compact Windows desktop utility and Mihomo remains the only route catalog owner — this plan will not add engine controls, dashboards, WebView features, or bundled cores.

---

## File structure

- `apps/multicore-desktop/assets/multicore-mark.svg` — simplified original Twin Route mark.
- `apps/multicore-desktop/ui/theme.slint` — Quiet Signal color, typography, spacing, focus, and radius tokens.
- `apps/multicore-desktop/ui/components.slint` — custom status, action, chip, row, and details primitives.
- `apps/multicore-desktop/ui/app.slint` — responsive one-screen composition and diagnostics surface.
- `apps/multicore-desktop/src/main.rs` — bind the preserved view-model state to the simplified Slint contract.
- `apps/multicore-desktop/src/view_model.rs` — presentation/source-contract tests for the new shell; behavior changes only if required by the inline layout.
- `scripts/preview-fixture.ps1` — secret-free local DTO fixture.
- `scripts/run-preview-capture.ps1` — deterministic window-state capture and cleanup.
- `docs/design/visual-system.md` — final Quiet Signal rules.
- `README.md` and `dist/multicore-windows-x64/**` — honest preview handoff.

### Task 1: Quiet Signal visual primitives

**Files:**
- Modify: `apps/multicore-desktop/assets/multicore-mark.svg`
- Modify: `apps/multicore-desktop/ui/theme.slint`
- Modify: `apps/multicore-desktop/ui/components.slint`
- Test: `apps/multicore-desktop/src/view_model.rs`

**Security flag:** none

**Does NOT cover:** screen composition, daemon calls, or state transitions.

- [x] Add a failing source-contract test that rejects `GlowCard`, pill radius `999px`, stock visible `Button` styling, inherited FL8 palette coupling, and missing fixed-height status primitives.
- [x] Run `cargo test -p multicore-desktop quiet_signal_primitives` and observe failure against the current components.
- [x] Implement `StatusMark`, `PrimaryAction`, `QuietChip`, `RouteNodeRow`, and `DetailsSurface` with explicit heights, 44 px targets, 2 px focus, and accessible state labels.
- [x] Replace the current SVG with an original two-node/one-route mark and document its geometry.
- [x] Run `cargo test -p multicore-desktop quiet_signal_primitives`, `cargo build -p multicore-desktop`, and desktop Clippy; expect all to pass.

### Task 2: Responsive inline home

**Files:**
- Modify: `apps/multicore-desktop/ui/app.slint`
- Modify: `apps/multicore-desktop/src/main.rs`
- Modify: `apps/multicore-desktop/src/view_model.rs`

**Security flag:** none

**Does NOT cover:** backend DTOs, selection semantics, or new settings/features.

- [x] Add failing contract tests requiring layout containers instead of fixed `y` positions, inline empty subscription, inline group/node rendering, one dominant primary action, no permanent Events row, and no full-window route/subscription overlays.
- [x] Add/retain view-model tests for Empty, importing, Ready, Connected, Error, catalog loading/empty, optimistic selection, rollback, and reconciliation presentation.
- [x] Run the focused tests and observe failures caused only by the old Slint structure.
- [x] Recompose the window with 16 px gutters, a compact header, fixed connection region, inline subscription/profile region, inline group chips, and a remaining-height node scroll region.
- [x] Keep diagnostics accessible from the header with the existing redacted events and Escape/focus restoration behavior.
- [x] Preserve the callback boundary: `import-url`, `primary-action`, `select-catalog-group`, `select-catalog-node`, event filtering, and safe display-only DTOs.
- [x] Run `cargo test -p multicore-desktop --all-targets`, `cargo build -p multicore-desktop`, and `cargo clippy -p multicore-desktop --all-targets -- -D warnings`; expect all to pass.

### Task 3: Visual proof and release preview

**Files:**
- Modify: `scripts/preview-fixture.ps1`
- Modify: `scripts/run-preview-capture.ps1`
- Modify: `docs/design/visual-system.md`
- Modify: `README.md`
- Regenerate: `artifacts/screenshots/**`
- Regenerate: `dist/multicore-windows-x64/**`

**Security flag:** security — fixture and release scans must never contain the user subscription URL, token, raw config, or legacy User-Agent.

**Does NOT cover:** installer, bundled Mihomo/Xray, updater, signing, or production daemon liveness hardening.

- [x] Run the local fixture and capture Empty/Ready, Connected, Routes, Error, and pending-selection states at 380×600, 400×640, and 520×800; terminate all fixture/client processes afterward.
- [x] Inspect every screenshot for clipping, overlap, weak contrast, inconsistent controls, excessive empty space, and more than one dominant action; fix UI and recapture until accepted.
- [x] Update `docs/design/visual-system.md` and `README.md` with the real interaction model and preview limitations.
- [x] Run `cargo fmt --all -- --check`, `cargo test --workspace --all-targets`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo build --workspace --release`.
- [x] Scan source/docs excluding `fl8-old-visual` and build output for FL8/Clash branding, `incy`, `flclashx`, the supplied gate8 URL/UUID, raw secrets, `todo!`, `unimplemented!`, and production placeholder paths.
- [x] Replace `dist/multicore-windows-x64` with exactly `multicore-desktop.exe`, `multicore-daemon.exe`, and `README.md`; record SHA-256 and sizes.
- [x] Run `git diff --check` and verify no fixture, desktop, daemon, Mihomo, or Xray process remains running.
