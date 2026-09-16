# Initial Dual-Core Client Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers-optimized:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver a runnable Windows-first Rust client that imports Mihomo YAML plus opaque Xray JSON, controls both cores, and exposes one simple native connect UI.

**Architecture:** `multicore-core` parses Mihomo for groups/UI, preserves Xray JSON unchanged, owns atomic snapshots, events, and lifecycle state. `multicore-daemon` exposes a localhost API and owns sidecar processes. `multicore-desktop` is a native Slint frontend and contains no networking or config logic.

**Tech Stack:** Rust 1.98.1, Tokio 1.53, Reqwest 0.13, Serde/serde_json, serde_yaml_ng 0.10, Axum 0.8, tracing, Slint 1.17.

**Assumptions:**
- Xray payload is server-generated and already contains Shadowsocks+FinalMask plus matching SOCKS inbounds — the client validates JSON syntax only.
- Mihomo payload is UTF-8 YAML mapping — base64, URI lists, and JSON-formatted Clash configs are rejected.
- Sidecar binaries are supplied in the configured cores directory — automatic core downloading is not part of this slice.
- Mihomo remains the only TUN/DNS owner — Xray TUN and Xray process routing are never generated.

---

## File structure

- `Cargo.toml` — workspace dependency and member registry.
- `crates/multicore-core/` — Mihomo subscription model, opaque Xray storage, state machine, redacted events, process supervisor.
- `crates/multicore-daemon/` — localhost HTTP API and persistent application state.
- `apps/multicore-desktop/` — native Slint executable and daemon client.
- `fixtures/` — synthetic YAML/JSON only; no live subscription secrets.

### Task 1: Workspace and strict subscription model

**Files:** `Cargo.toml`, `.gitignore`, `rust-toolchain.toml`, `crates/multicore-core/Cargo.toml`, `crates/multicore-core/src/{lib.rs,subscription.rs}`

**Security flag:** security — parses remote untrusted input and enforces size/type boundaries.

**Does NOT cover:** network fetching, disk snapshots, or sidecar execution.

- [ ] Create the workspace manifests with pinned major/minor dependency ranges.
- [ ] Write failing tests for UTF-8 Mihomo mapping, rejected JSON-looking Mihomo body, Mihomo group extraction, valid opaque Xray JSON, invalid JSON, and byte preservation.
- [ ] Run `cargo test -p multicore-core subscription` and confirm missing parser failures.
- [ ] Implement `SubscriptionBundle`, `MihomoProfile`, and opaque `XrayConfig` with a 32 MiB per-body limit.
- [ ] Run `cargo test -p multicore-core subscription` and confirm all parser tests pass.

### Task 2: Atomic fetch and profile snapshot

**Files:** `crates/multicore-core/src/{fetch.rs,snapshot.rs}`, `crates/multicore-core/tests/fetch.rs`

**Security flag:** security — handles subscription URLs, redirects, credentials, and persistent secrets.

**Does NOT cover:** schemes other than HTTPS/HTTP or any subscription conversion.

- [ ] Write failing HTTP fixture tests proving `multicore-json-massive` is attempted first and `multicore-mihomo`/`multicore-xray` fallback is applied only when both bodies validate.
- [ ] Write a failing test proving a partial refresh leaves the last snapshot untouched.
- [ ] Implement bounded Reqwest downloads, per-UA cache metadata, redacted errors, and temp-file-plus-rename publication.
- [ ] Run `cargo test -p multicore-core fetch snapshot` and confirm atomicity and UA assertions pass.

### Task 3: Opaque runtime publication and lifecycle state

**Files:** `crates/multicore-core/src/{runtime.rs,state.rs,event.rs,supervisor.rs}`, `crates/multicore-core/tests/runtime.rs`

**Security flag:** security — generates local listener configuration and launches privileged networking processes.

**Does NOT cover:** downloading core binaries or letting Xray own TUN/DNS.

- [ ] Write failing fixtures proving Mihomo groups/proxies are exposed to UI while Xray bytes are published without semantic inspection or field loss.
- [ ] Write failing state tests for `Empty → Ready → Connecting → Connected` and rollback to `Ready` on either process failure.
- [ ] Implement runtime file publication, structured redacted events, Xray-first/Mihomo-second startup, reverse-order shutdown, and rollback.
- [ ] Run `cargo test -p multicore-core runtime state supervisor` and confirm byte preservation, ordering, and rollback pass.

### Task 4: Headless daemon API

**Files:** `crates/multicore-daemon/Cargo.toml`, `crates/multicore-daemon/src/{main.rs,api.rs,app.rs}`, `crates/multicore-daemon/tests/api.rs`

**Security flag:** security — local control boundary must not accept remote callers or expose subscription secrets.

**Does NOT cover:** remote administration, LAN binding, or browser CORS.

- [ ] Write failing API tests for status, import, connect, disconnect, and redacted events.
- [ ] Implement an Axum server bound only to `127.0.0.1`, with bearer token authentication and no CORS middleware.
- [ ] Wire daemon transitions to `multicore-core`; return actionable Russian errors with correlation IDs.
- [ ] Run `cargo test -p multicore-daemon` and confirm unauthorized requests fail and lifecycle endpoints pass.

### Task 5: Native one-button Slint desktop

**Files:** `apps/multicore-desktop/Cargo.toml`, `apps/multicore-desktop/build.rs`, `apps/multicore-desktop/src/{main.rs,client.rs,view_model.rs}`, `apps/multicore-desktop/ui/app.slint`, `docs/design/visual-system.md`

**Security flag:** none.

**Does NOT cover:** raw config editors, charts, arbitrary overrides, or separate core controls.

- [ ] Define the visual system: graphite surfaces, warm white type, lime status accent, 8px rhythm, 48px controls, keyboard focus, reduced motion.
- [ ] Write failing view-model tests for Empty, Importing, Ready, Connecting, Connected, Error, and degraded daemon states.
- [ ] Implement a 440×620 adaptive native window with profile input/onboarding, one contextual action button, current route line, and a details drawer.
- [ ] Connect UI callbacks to the daemon client without putting network/config logic in `.slint`.
- [ ] Run `cargo test -p multicore-desktop` and `cargo build -p multicore-desktop`.

### Task 6: Integration and live compatibility probe

**Files:** `crates/multicore-core/tests/integration.rs`, `README.md`, `project-map.md`, `session-log.md`

**Security flag:** security — optional live test uses a secret subscription URL from environment only.

**Does NOT cover:** committing the live URL, UUIDs, endpoints, SNI, keys, or downloaded subscription bodies.

- [ ] Add an ignored live test reading `MULTICORE_TEST_SUB_URL`, fetching with `multicore-mihomo` and `multicore-xray`, and reporting counts only.
- [ ] Run all unit/integration tests and Clippy with warnings denied.
- [ ] Run the ignored live probe with the URL injected only through the process environment and verify temporary bodies are removed.
- [ ] Build release binaries and document exact run/configuration commands.
- [ ] Perform full spec, security, and UI quality review; resolve all findings.
