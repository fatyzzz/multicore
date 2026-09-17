# Subscription Metadata Headers Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers-optimized:subagent-driven-development (recommended) or superpowers-optimized:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Parse, persist and present safe subscription metadata compatible with INCY, FlClashX and Happ/Rabbit-Hole style response headers, send the required HWID request headers, and place a validated provider logo in the connection control.

**Architecture:** `multicore-core` owns bounded header parsing, merge precedence and private persistence. `multicore-daemon` exposes only safe presentation data, while `multicore-desktop` renders the provider title and announcement without receiving credential-bearing URLs. The existing massive-first fetch contract and last-good snapshot transaction remain unchanged.

**Tech Stack:** Rust 1.98.1, Reqwest 0.13, Serde, Base64 0.22, Axum daemon DTOs, Slint 1.17.

**Assumptions:** The backend returns metadata as HTTP response headers; inline body metadata is excluded. Provider URLs are not exposed in the ordinary status DTO; link opening needs a later explicit endpoint. Real multi-sub persistence is separate; this makes the current 0/1 subscription presentation vector-ready.

---

## File structure

- `crates/multicore-core/src/device_identity.rs` — create/load the owner-only stable Windows HWID and bounded device descriptors.
- `crates/multicore-core/src/fetch.rs` — attach HWID request headers, capture bounded response metadata/logo and apply massive/fallback precedence.
- `crates/multicore-core/src/snapshot.rs` — sanitize, parse and persist presentation metadata.
- `crates/multicore-core/tests/metadata_headers.rs` — end-to-end fetch and persistence contracts.
- `crates/multicore-daemon/src/lib.rs` — safe status DTO projection.
- `crates/multicore-daemon/tests/core_backend.rs` — daemon serialization and last-good refresh behavior.
- `apps/multicore-desktop/src/daemon.rs` — deserialize expanded safe metadata.
- `apps/multicore-desktop/src/view_model.rs` — format title, usage, expiry and announcement state.
- `apps/multicore-desktop/src/main.rs` — map presentation state into Slint properties.
- `apps/multicore-desktop/ui/app.slint` — render the subscription shelf and announcement strip.

### Task 1: Parse and merge bounded response metadata

**Files:**
- Modify: `Cargo.toml`
- Modify: `crates/multicore-core/Cargo.toml`
- Modify: `crates/multicore-core/src/fetch.rs`
- Modify: `crates/multicore-core/src/snapshot.rs`
- Create: `crates/multicore-core/tests/metadata_headers.rs`

**Security flag:** security — parses untrusted response headers and persists provider-controlled text and URLs.

**Does NOT cover:** inline `#metadata` body comments, remote logos, remote settings, HWID, redirects, or automatic refresh scheduling.

- [x] **Step 1: Write failing tests** covering plain and Base64 titles, FlClashX service-name fallback, upload+download usage, prefixed userinfo, bounded announcements and aliases, safe/unsafe URLs, interval clamping, malformed-field fallback, and massive response terminality.
- [x] **Step 2: Verify RED** with `cargo test -p multicore-core --test metadata_headers --locked`; expect missing metadata fields/parsers and the current upload omission.
- [x] **Step 3: Implement minimal parsing** with a typed raw header carrier, aggregate/per-field bounds, Base64 standard/URL-safe decoding, text sanitization, URL validation and field-wise Mihomo→Xray merge.
- [x] **Step 4: Preserve backward compatibility** using `#[serde(default)]` optional fields and source-host fallback for old generations.
- [x] **Step 5: Verify GREEN** with `cargo test -p multicore-core --test metadata_headers --locked` and `cargo test -p multicore-core --lib --locked`; expect all passing.

### Task 2: Send a stable HWID with every subscription request

**Files:**
- Create: `crates/multicore-core/src/device_identity.rs`
- Modify: `crates/multicore-core/src/lib.rs`
- Modify: `crates/multicore-core/src/fetch.rs`
- Modify: `crates/multicore-core/Cargo.toml`
- Test: `crates/multicore-core/tests/device_identity.rs`

**Security flag:** security — derives and persists a stable device identifier and sends device metadata to subscription servers.

**Does NOT cover:** sending raw MachineGuid/hostname/user values, advertising other clients' User-Agents, or exposing HWID in UI/logs/API.

- [x] **Step 1: Write failing tests** for uppercase UUID formatting, deterministic documented derivation, owner-only atomic persistence, corrupt-file recovery, and exact `x-hwid`/`x-device-os`/`x-ver-os`/`x-device-model` headers on all three User-Agent requests.
- [x] **Step 2: Verify RED** with `cargo test -p multicore-core --test device_identity --locked`; expect the identity module and request headers to be absent.
- [x] **Step 3: Implement identity creation/loading** with SHA-256 derivation on Windows, secure random fallback, atomic publication and bounded descriptors.
- [x] **Step 4: Inject the same identity into massive/Mihomo/Xray requests** without logging it or changing massive terminality.
- [x] **Step 5: Verify GREEN** with the target test and `cargo test -p multicore-core --lib --locked`.

### Task 3: Cache and project safe metadata and service logo

**Files:**
- Modify: `crates/multicore-daemon/src/lib.rs`
- Modify: `crates/multicore-daemon/tests/core_backend.rs`

**Security flag:** security — controls which persisted provider data crosses the authenticated local API.

**Does NOT cover:** returning raw subscription URLs, arbitrary remote images, or directly opening provider links.

- [ ] **Step 1: Write failing tests** asserting title/usage/announcement/tone/availability serialization, bounded HTTPS `flclashx-servicelogo` download/cache with invalid-image fallback, absence of raw subscription URLs, and preservation of last-good metadata/logo after failed refresh.
- [ ] **Step 2: Verify RED** with `cargo test -p multicore-daemon --test core_backend --locked`; expect missing DTO fields.
- [ ] **Step 3: Extend `SubscriptionInfoDto`** with sanitized values and link-availability booleans only.
- [ ] **Step 4: Verify GREEN** with the target test and `cargo test -p multicore-daemon --lib --locked`.

### Task 4: Present the real subscription identity, announcement and logo

**Files:**
- Modify: `apps/multicore-desktop/src/daemon.rs`
- Modify: `apps/multicore-desktop/src/view_model.rs`
- Modify: `apps/multicore-desktop/src/main.rs`
- Modify: `apps/multicore-desktop/ui/app.slint`

**Security flag:** security — renders untrusted provider text and gates external actions.

**Does NOT cover:** fetching remote logos, arbitrary provider colors, multiple persisted profiles, or opening URLs before an explicit authenticated action endpoint exists.

- [ ] **Step 1: Write failing Rust/source-contract tests** requiring `profile-title` presentation, source-host fallback, saturating used traffic, announcement state, semantic tone, cached service logo inside the connection control with built-in fallback, stable empty states, and no literal `gate8`/`work` labels.
- [ ] **Step 2: Verify RED** with `cargo test -p multicore-desktop view_model --locked`; expect missing presentation fields and Slint properties.
- [ ] **Step 3: Implement the presentation mapping** and a compact announcement strip below the vector-ready subscription shelf. Keep provider text plain, elided/wrapped within bounds, and keyboard/accessibility semantics intact.
- [ ] **Step 4: Verify GREEN** with `cargo test -p multicore-desktop --lib --locked` and `cargo check -p multicore-desktop --locked`.

### Task 5: Integrate automatic latency and the refreshed visual shell

**Files:**
- Modify: `apps/multicore-desktop/src/daemon.rs`
- Modify: `apps/multicore-desktop/src/view_model.rs`
- Modify: `apps/multicore-desktop/src/main.rs`
- Modify: `apps/multicore-desktop/ui/app.slint`
- Modify: `apps/multicore-desktop/ui/components.slint`
- Modify: `apps/multicore-desktop/ui/theme.slint`
- Test: existing desktop latency and UI source-contract tests

**Security flag:** none

**Does NOT cover:** true multi-sub storage/switching or starting TUN during verification.

- [ ] **Step 1: Write failing tests** requiring no manual latency button, selected-group targeted requests, single-flight rerun coalescing, automatic load/group-switch/visible 60-second triggers, and 79/80/149/150 ms tone boundaries.
- [ ] **Step 2: Verify RED** with the focused desktop unit tests.
- [ ] **Step 3: Implement automatic targeted latency** using the daemon's existing `group_ids` support and retain last-known values for inactive groups.
- [ ] **Step 4: Implement the native Signal Grid shell** with restrained state glow, local mesh layers, stable tabular latency column, compact subscription shelf, and one-page route hierarchy.
- [ ] **Step 5: Read Impeccable `craft-floor.md` immediately before the UI edits, then run the bounded native screenshot/review workflow without starting TUN.**

### Task 6: Final verification

**Files:**
- Modify: `DESIGN.md`
- Modify: `.impeccable/design.json`
- Modify: relevant release notes if behavior changes are user-visible

**Security flag:** security — final scans must prove that URLs, credentials and raw configs are absent.

**Does NOT cover:** publishing or installing a release unless separately requested after verification.

- [ ] Run `cargo fmt --all -- --check`.
- [ ] Run `cargo test --workspace --all-targets --locked`.
- [ ] Run strict workspace Clippy with warnings denied.
- [ ] Run deterministic preview captures for empty, ready, connected, populated, announcement and error states; never start TUN.
- [ ] Run credential/source scans excluding the supplied reference tree and build output.
- [ ] Run the Impeccable finish reviewer and document the approved native design system.
