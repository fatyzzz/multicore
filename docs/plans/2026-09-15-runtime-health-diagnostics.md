# Runtime Health and Diagnostics Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers-optimized:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make MultiCore report connected only when its own two cores and Windows TUN are ready, and expose actionable safe diagnostics in the native UI.

**Architecture:** Runtime publication forces a unique `MultiCore` TUN name. The concrete sidecar controller captures bounded redacted output and gates startup on owned local resources. The daemon publishes one authenticated diagnostics DTO consumed by the existing native Diagnostics panel.

**Tech Stack:** Rust 2024, Tokio, Axum, serde, Slint, native Windows IP Helper route discovery.

**Assumptions:** The packaged target is Windows x64 — non-Windows TUN ownership is reported unsupported. Xray bridge inbounds listen on loopback — non-loopback inbounds are not used as readiness proof. The backend continues supplying full Mihomo YAML and Xray JSON — no other subscription format is added.

---

## File structure

- `crates/multicore-core/src/snapshot.rs`: ephemeral Mihomo TUN plus Xray DNS/interface overlays.
- `crates/multicore-core/src/network.rs`: native connected IPv4 default-route selection.
- `crates/multicore-core/src/sidecar.rs`: output capture, readiness gates, and core diagnostics model.
- `crates/multicore-core/src/event.rs`: reusable public log redaction entry point.
- `crates/multicore-core/src/supervisor.rs`: unchanged orchestration contract; concrete `start` becomes honest.
- `crates/multicore-daemon/src/lib.rs`: authenticated diagnostics contract and backend implementation.
- `apps/multicore-desktop/src/daemon.rs`: diagnostics HTTP client DTOs.
- `apps/multicore-desktop/src/view_model.rs`: presentation state and filters.
- `apps/multicore-desktop/src/main.rs`: diagnostics loading and Slint bindings.
- `apps/multicore-desktop/ui/app.slint`: compact health dashboard and combined feed.

### Task 1: Own the Mihomo TUN name

**Files:** Modify `crates/multicore-core/src/snapshot.rs`; test `crates/multicore-core/tests/runtime_control.rs`.

**Security flag:** none

**Does NOT cover:** It does not change routes, DNS, stack, addresses, selectors, or persisted subscription YAML.

- [x] Add a failing runtime-publication test asserting `tun.device == "MultiCore"`, all sibling TUN fields survive, and snapshot bytes stay unchanged.
- [x] Run `cargo test -p multicore-core --test runtime_control runtime_overlay_owns_a_unique_tun_device` and confirm the assertion fails.
- [x] Insert the runtime-only `tun.device` overlay while rejecting a non-mapping `tun` value.
- [x] Re-run the targeted test and the complete `runtime_control` suite.

### Task 2: Gate sidecars and retain safe logs

**Files:** Modify `crates/multicore-core/src/event.rs`, `crates/multicore-core/src/sidecar.rs`, `crates/multicore-core/Cargo.toml`; test `crates/multicore-core/tests/sidecar_processes.rs`.

**Security flag:** security

**Does NOT cover:** Readiness proves owned local resources, not reachability of a remote proxy or arbitrary public website.

- [x] Add failing tests for early child exit, readiness timeout, successful declared loopback ports, URL/token redaction, line truncation, and ring-buffer eviction.
- [x] Run the targeted sidecar tests and confirm they fail for the missing readiness/log behavior.
- [x] Pipe and continuously drain stdout/stderr, expose sanitized bounded diagnostics, retain the latest redacted daemon session at `%LOCALAPPDATA%\MultiCore\logs\latest-core.log`, and make concrete `start` wait for Xray ports plus Mihomo controller and exact Windows adapter.
- [x] Re-run `cargo test -p multicore-core --test sidecar_processes` and `cargo test -p multicore-core --test operational_core`.

### Task 3: Publish authenticated diagnostics

**Files:** Modify `crates/multicore-daemon/src/lib.rs`; test `crates/multicore-daemon/tests/http_api.rs` and `crates/multicore-daemon/tests/core_backend.rs`.

**Security flag:** security

**Does NOT cover:** The endpoint never returns raw configs, environment variables, controller authorization, or subscription URLs.

- [x] Add failing API/backend tests for health fields, bounded log records, authentication, and redaction.
- [x] Run the targeted daemon tests and confirm the diagnostics route is absent.
- [x] Add `GET /v1/diagnostics`, DTO mapping, and backend access to controller diagnostics.
- [x] Re-run both targeted daemon test binaries.

### Task 4: Build the compact native dashboard

**Files:** Modify `apps/multicore-desktop/src/daemon.rs`, `apps/multicore-desktop/src/view_model.rs`, `apps/multicore-desktop/src/main.rs`, `apps/multicore-desktop/ui/app.slint`, `scripts/preview-fixture.ps1`.

**Security flag:** none

**Does NOT cover:** The main screen will not become a full monitoring dashboard; detail stays behind the existing diagnostics action.

- [x] Add failing client/model/source-contract tests for diagnostics fetch, health labels, log filtering, failure retention, and accessible dashboard labels.
- [x] Run `cargo test -p multicore-desktop` and confirm the new tests fail.
- [x] Implement the diagnostics client/model bindings and a three-card health strip above the scrollable feed.
- [x] Re-run desktop tests and capture populated, connected, and error diagnostics previews.

### Task 4.5: Break the Xray endpoint DNS/TUN loop

**Files:** Modify `crates/multicore-core/src/network.rs`, `crates/multicore-core/src/snapshot.rs`, `crates/multicore-daemon/src/main.rs`, `crates/multicore-daemon/src/lib.rs`; test `runtime_control`, daemon binary, and `core_backend`.

**Security flag:** security

- [x] Reproduce the live failure from bounded core logs: Mihomo DNS failed while all Xray Shadowsocks/FinalMask endpoints were domain-valued.
- [x] Add failing tests for runtime host mappings, `ForceIP`, physical-interface binding, null optional sections, empty outbounds, local DNS bootstrap, connect-time regeneration, and offline saved-profile loading.
- [x] Resolve Xray endpoint domains before TUN, merge them into ephemeral `dns.hosts`, bind Xray to the native Windows default route, and regenerate immediately before each Connect without changing the stored JSON.
- [x] Remove the obsolete Mihomo `global-client-fingerprint` only from the runtime copy.
- [x] Validate the transformed current backend payload with pinned Xray `v26.9.9 run -test` without starting TUN.

### Task 4.6: Restore independent Mihomo selectors

**Files:** Modify `crates/multicore-daemon/src/lib.rs`, `crates/multicore-daemon/tests/selection_api.rs`, `crates/multicore-daemon/tests/catalog_api.rs`, and `apps/multicore-desktop/src/view_model.rs`.

**Security flag:** none

- [x] Reproduce the bug with a failing three-group test: choosing a node in a later group erased the earlier groups' selected-node state.
- [x] Store one selected node per interactive Mihomo group and keep the connection summary attached to the primary `Сервер` group.
- [x] Preserve the desktop's local group-navigation state across selection responses and authoritative catalog refreshes.
- [x] Verify the daemon and desktop suites, full workspace matrix, strict Clippy, package hashes, and one-click smoke without starting TUN.

### Task 5: Verify and package

**Files:** Update `README.md`, `state.md`, `session-log.md`, `project-map.md`; rebuild `dist/multicore-windows-x64` through the existing packaging script.

**Security flag:** none

- [x] Verify both smoke modes leave no exact-package process behind; do not stop, inspect, or reconfigure FlClashX.
- [x] Run `cargo fmt --all -- --check`, `cargo test --workspace --all-targets`, and `cargo clippy --workspace --all-targets -- -D warnings`.
- [x] Run seven preview capture checks, package tests, and graceful/crash one-click smoke tests.
- [x] Hand the rebuilt package to the user for the privileged TUN test; the agent must not launch TUN or stop/reconfigure FlClashX.
- [x] Record exact artifact path and SHA-256 hashes.
