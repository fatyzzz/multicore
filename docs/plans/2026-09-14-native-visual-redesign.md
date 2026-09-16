# Native Visual Redesign Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers-optimized:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the rejected wireframe with an original, compact MultiCore Slint interface inspired only by the supplied reference's visual quality and backed by MultiCore-owned daemon contracts.

**Architecture:** The desktop remains a native Slint client with no config or Xray logic. A single home shell owns overlay panels for subscription, route selection and events. The daemon exposes a safe Mihomo-derived catalog; selection mutations are implemented against MultiCore's own control abstraction and never inspect Xray.

**Tech Stack:** Rust 1.98.1, Slint 1.17, Axum 0.8, Tokio 1.53, serde, SVG assets.

**Assumptions:** The supplied reference is licensed GPLv3, so no source or branded asset will be copied — only independently reimplemented color, density and hierarchy ideas are used. Route selection assumes Mihomo exposes a loopback controller configured by the runtime layer — it will not work against a Mihomo process with no controllable local endpoint.

---

## File structure

- `apps/multicore-desktop/assets/multicore-mark.svg` — original scalable product mark.
- `apps/multicore-desktop/ui/theme.slint` — palette, spacing and typography tokens.
- `apps/multicore-desktop/ui/components.slint` — mark, state pill, glow card, navigation row and overlay primitives.
- `apps/multicore-desktop/ui/app.slint` — one-screen composition and panels.
- `apps/multicore-desktop/src/view_model.rs` — panel, catalog and pending-selection presentation state.
- `apps/multicore-desktop/src/daemon.rs` — catalog and selection HTTP DTO/client methods.
- `crates/multicore-daemon/src/lib.rs` — MultiCore catalog/selection API contracts and CoreBackend implementation.
- `crates/multicore-daemon/tests/catalog_api.rs` — serialized HTTP contract and auth tests.
- `docs/design/visual-system.md` — final MultiCore-owned visual rules.

### Task 1: Original brand mark and Slint design primitives

**Files:**
- Create: `apps/multicore-desktop/assets/multicore-mark.svg`
- Create: `apps/multicore-desktop/ui/theme.slint`
- Create: `apps/multicore-desktop/ui/components.slint`
- Modify: `apps/multicore-desktop/build.rs`
- Modify: `docs/design/visual-system.md`

**Security flag:** none

**Does NOT cover:** navigation state, daemon calls, group data or selection behavior.

- [x] Create an original 64×64 `Twin Core Route` SVG using only MultiCore geometry: two rounded colored cores and one shared white route; verify that no FL8 path, name or asset is imported.
- [x] Add Slint tokens for the exact palette `#06060A/#0A0A0D/#111114/#1A1A1F/#242429/#0091FF/#22C55E/#F59E0B/#EF4444` and 8/10/14/16 px radii.
- [x] Implement reusable `BrandMark`, `StatePill`, `GlowCard`, `NavigationRow` and `OverlayPanel` components with 44 px minimum interactive sizes and visible focus borders.
- [x] Compile the Slint modules with `cargo test -p multicore-desktop`; expected result is a successful generated-module build without web assets.

### Task 2: One-screen shell and overlay state

**Files:**
- Modify: `apps/multicore-desktop/ui/app.slint`
- Modify: `apps/multicore-desktop/src/main.rs`
- Modify: `apps/multicore-desktop/src/view_model.rs`
- Modify: `apps/multicore-desktop/src/daemon.rs`

**Security flag:** none

**Does NOT cover:** new daemon endpoints; route panel uses the existing current route until Task 3.

- [x] Add failing view-model tests proving only one of `None`, `Subscription`, `Routes`, or `Events` panels can be open, Escape closes it, and connection pending disables repeated action.
- [x] Align the desktop event DTO with the daemon's monotonic `id` cursor so polling never replays or skips events.
- [x] Run `cargo test -p multicore-desktop view_model`; expected failure is missing `Panel` state/actions.
- [x] Implement a 400×700 shell bounded to 380×600 and 500×800: brand header, compact connection anchor, profile row, route row, events row and three overlay panels without bottom navigation.
- [x] Map Empty/Ready/Connecting/Connected/Error/Degraded to semantic border, halo, label and action copy without engine-specific controls.
- [x] Run `cargo test -p multicore-desktop` and `cargo build -p multicore-desktop`; expected result is all tests and Slint compilation passing.

### Task 3: MultiCore-owned Mihomo catalog API

**Files:**
- Modify: `crates/multicore-daemon/src/lib.rs`
- Create: `crates/multicore-daemon/tests/catalog_api.rs`
- Modify: `apps/multicore-desktop/src/daemon.rs`
- Modify: `apps/multicore-desktop/src/view_model.rs`

**Security flag:** security — names originate in an untrusted subscription and the endpoint crosses the authenticated local boundary.

**Does NOT cover:** Xray parsing, latency measurement, traffic metrics, provider URLs or raw YAML exposure.

- [x] Add failing daemon tests for authenticated `GET /v1/catalog` returning an opaque `revision`, group/node IDs, safe display labels, selection flags and optional delay from `Snapshot.mihomo.groups()`.
- [x] Add failing desktop fixture tests using the exact serialized daemon catalog DTO and rejecting malformed payloads visibly.
- [x] Implement a cached bounded DTO with no raw YAML, URL, secret or Xray fields; suspicious untrusted labels are genericized, empty profiles return an empty group array, and lossy labels are never identifiers.
- [x] Render scrollable group chips and node rows from the catalog; omit latency text when `delay_ms` is absent.
- [x] Run `cargo test -p multicore-daemon --test catalog_api` and `cargo test -p multicore-desktop`; expected result is both suites passing.

### Task 4: Mihomo-only node selection

**Files:**
- Modify: `crates/multicore-daemon/src/lib.rs`
- Modify: `crates/multicore-daemon/tests/catalog_api.rs`
- Modify: `apps/multicore-desktop/src/daemon.rs`
- Modify: `apps/multicore-desktop/src/view_model.rs`
- Modify: `apps/multicore-desktop/src/main.rs`

**Security flag:** security — mutation crosses the authenticated daemon boundary and must target loopback Mihomo only.

**Does NOT cover:** editing Xray, arbitrary Mihomo API proxying, non-loopback controllers or latency probes.

- [x] Add failing API tests for `PUT /v1/selections/:group_id` with `{revision,node_id}`: authentication, stale revision, unknown group/node rejection, body limit, and no secret reflection.
- [x] Add failing view-model tests proving a selection is disabled while pending, updates on success and restores the previous node with a visible safe error on failure.
- [x] Resolve opaque IDs atomically against the matching current revision, then invoke a narrow `MihomoSelector::select(raw_group, raw_node)`; production implementation must reject non-loopback controller addresses and percent-encode the raw group path.
- [x] Wire `CoreBackend` to validate group/node membership from the current Mihomo catalog before invoking `MihomoSelector`; update only daemon selection state, never Xray JSON.
- [x] Wire the Slint node-row callback through the desktop daemon client and view model.
- [x] Run daemon/desktop tests, then `cargo test --workspace --all-targets` and `cargo clippy --workspace --all-targets -- -D warnings`.

### Task 5: Visual verification and release preview

> Superseded on 2026-09-15 by `docs/plans/2026-09-15-quiet-signal-redesign.md` after the original presentation was rejected during screenshot review.

**Files:**
- Modify: `docs/design/visual-system.md`
- Modify: `README.md`
- Regenerate: `dist/multicore-windows-x64/multicore-desktop.exe`
- Regenerate: `dist/multicore-windows-x64/multicore-daemon.exe`

**Security flag:** none

**Does NOT cover:** installer, bundled Xray/Mihomo binaries, auto-update or code signing.

- [ ] Run view-model and HTTP suites for empty/loading/error/degraded/success and catalog/selection rollback.
- [ ] Build `cargo build --workspace --release` and copy only both MultiCore executables plus README into the ignored preview directory.
- [ ] Launch the desktop against a local fixture daemon and visually inspect 380×600, 400×700 and 500×800 for clipping, focus visibility, readable contrast and a single obvious primary action.
- [ ] Scan source and built documentation for FL8/Clash branding and forbidden legacy User-Agent names; the reference directory itself is excluded from this scan.
- [ ] Run `git diff --check` and a production-code stub scan before reporting completion.
