# One-click Windows Bootstrap Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers-optimized:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Produce a portable Windows x64 package that includes the pinned latest prerelease Xray and Mihomo cores and starts from one double-click on `MultiCore.exe`.

**Architecture:** The desktop either uses an explicitly configured external daemon or launches its packaged daemon with a random bearer token and loopback port zero. The daemon announces the kernel-selected address over its captured stdout, the desktop authenticates a status probe, and a Windows Job Object owns the complete child tree. A digest-locked PowerShell packager downloads and stages both official cores with license notices.

**Tech Stack:** Rust 2024, Slint, Axum/Tokio, `getrandom`, `windows-sys`, PowerShell 5.1+, GitHub release assets.

**Assumptions:**

- Assumes Windows x86-64 with `%LOCALAPPDATA%` available — packaged auto-start will not run on another OS.
- Assumes the pinned GitHub asset URLs remain downloadable — packaging will fail closed rather than accept an unverified replacement.
- Assumes both external daemon variables are supplied together — a partial override is rejected and never falls back silently.
- Assumes portable-folder distribution — this plan does not provide an installer, code signing, updater, service, or elevation broker.

---

## File structure

- Create `apps/multicore-desktop/src/bootstrap.rs`: startup mode selection, package path validation, daemon process/job lifetime, readiness parsing, authenticated wait.
- Modify `apps/multicore-desktop/src/main.rs`: Windows GUI subsystem and bootstrap-owned client construction.
- Modify `apps/multicore-desktop/Cargo.toml`: direct random and Windows process API dependencies.
- Modify `crates/multicore-daemon/src/lib.rs` and `src/main.rs`: tested, flushed loopback readiness announcement after router construction and binding.
- Modify `crates/multicore-core/src/sidecar.rs`: remove bootstrap credentials from third-party core environments.
- Create `scripts/package-windows-release.ps1`: pinned download, digest verification, extraction, license staging, atomic package publication.
- Create `scripts/test-package-windows-release.ps1`: isolated package-contract and failure-atomicity checks.
- Create `packaging/windows-x64/versions.json`, `README.md`, `THIRD_PARTY_NOTICES.md`, and license texts: reproducible source and redistribution record.
- Modify `README.md`, `state.md`, `project-map.md`, and `session-log.md`: one-click instructions and durable architecture state.

### Task 1: Daemon readiness protocol

**Files:**
- Modify: `crates/multicore-daemon/src/lib.rs`
- Modify: `crates/multicore-daemon/src/main.rs`
- Create: `crates/multicore-daemon/tests/readiness.rs`

**Security flag:** `security`

**Does NOT cover:** Readiness announces an address only for a daemon launched with `MULTICORE_DAEMON_READY_STDOUT=1`; normal manual daemon stdout behavior is unchanged, and authentication is still required.

- [x] **Step 1: Write failing tests** proving the writer emits and flushes exactly one bounded ASCII `MULTICORE_READY {address}\n` line for a resolved nonzero loopback address, rejects zero-port/non-loopback addresses, is disabled without the opt-in variable, and cannot contain the bearer token fixture `secret-marker`; add a spawned-binary test that authenticates `/v1/status` after reading the real line.
- [x] **Step 2: Verify RED** with `cargo test -p multicore-daemon --test readiness`; expect a compile failure because the readiness formatter/publication API does not exist.
- [x] **Step 3: Implement the protocol** in the library with an injected writer; in the binary construct `try_router` first, bind, obtain `listener.local_addr()`, validate loopback/nonzero port, and write `MULTICORE_READY {address}\n` through a locked/flushed stdout only when `MULTICORE_DAEMON_READY_STDOUT=1`.
- [x] **Step 4: Verify GREEN** with `cargo test -p multicore-daemon --test readiness` and `cargo test -p multicore-daemon`; expect all tests to pass.

### Task 2: Desktop-owned daemon bootstrap

**Files:**
- Create: `apps/multicore-desktop/src/bootstrap.rs`
- Modify: `apps/multicore-desktop/src/main.rs`
- Modify: `apps/multicore-desktop/Cargo.toml`
- Modify: `crates/multicore-core/src/sidecar.rs`
- Test: `crates/multicore-core/tests/sidecar_processes.rs`

**Security flag:** `security`

**Does NOT cover:** The desktop never owns or kills a daemon selected by the complete external URL+token override; auto mode accepts only the packaged relative paths and literal loopback readiness output.

- [x] **Step 1: Write failing unit tests** in `bootstrap.rs` for absent/empty/non-Unicode/complete/partial override selection, package layout with spaces, non-regular/wrong-PE/wrong-architecture/package-escaping daemon/core errors, random 32-byte token encoding, malformed/oversized/non-loopback/zero-port readiness rejection, child early exit, one-total-deadline timeout, successful authenticated transient retry, fail-fast authentication failure, and every post-spawn cleanup path.
- [x] **Step 2: Verify RED** with `cargo test -p multicore-desktop bootstrap`; expect missing bootstrap types/functions.
- [x] **Step 3: Implement startup selection and path derivation** using `current_exe()/runtime/multicore-daemon.exe`, `current_exe()/cores/{xray,mihomo}.exe`, and `%LOCALAPPDATA%\MultiCore` with direct argument/env arrays and no shell.
- [x] **Step 4: Implement child lifetime** with at-creation Job Object membership, non-inheritable `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, `CREATE_NO_WINDOW`, null stdin/stderr, a capped readiness reader isolated from the monotonic total deadline, transient-only authenticated status retry, and one terminate/unblock/reap cleanup path; never print token or inherited subscription data.
- [x] **Step 5: Remove bootstrap variables from core children** with explicit `env_remove` calls in `TokioProcessLauncher`, led by a failing process-environment regression test proving Xray/Mihomo cannot read `MULTICORE_DAEMON_TOKEN` or `MULTICORE_DAEMON_READY_STDOUT`.
- [x] **Step 6: Integrate `main.rs`** so bootstrap errors become fixed sanitized Russian `UnavailableDaemonClient` messages, the guard remains alive through UI creation and `ui.run()`, and release builds use `windows_subsystem = "windows"`.
- [x] **Step 7: Verify GREEN** with `cargo test -p multicore-core --test sidecar_processes`, `cargo test -p multicore-desktop`, `cargo clippy -p multicore-desktop --all-targets -- -D warnings`, and `cargo build -p multicore-desktop --release`; inspect the PE subsystem and expect Windows GUI.

### Task 3: Pin and package both latest prerelease cores

**Files:**
- Create: `packaging/windows-x64/versions.json`
- Create: `packaging/windows-x64/README.md`
- Create: `packaging/windows-x64/THIRD_PARTY_NOTICES.md`
- Create: `packaging/windows-x64/licenses/Xray-core-MPL-2.0.txt`
- Create: `packaging/windows-x64/licenses/mihomo-GPL-3.0.txt`
- Create: `scripts/package-windows-release.ps1`
- Create: `scripts/test-package-windows-release.ps1`

**Security flag:** `security`

**Does NOT cover:** The runtime never downloads or updates cores; only the developer packaging script performs network access, and it accepts only the two pinned official HTTPS URLs and SHA-256 values.

- [x] **Step 1: Write the failing package-contract test** that feeds fixture archives/binaries, asserts the exact package tree, verifies `MultiCore.exe` rename and every hash, runs twice into absent destinations for deterministic content, and proves bad hashes, Zip Slip/absolute/UNC/device/ADS/trailing-alias names, duplicate executable entries, symlink metadata, oversized entries, reparse ancestors, and malformed/x86/ARM64/DLL PE inputs fail without modifying any existing destination.
- [x] **Step 2: Verify RED** with `powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/test-package-windows-release.ps1`; expect failure because the packager and manifest do not exist.
- [x] **Step 3: Pin upstream inputs** with full commits, exact asset names/URLs/entries, archive and executable SHA-256 values, immutable source URLs, and Rust `1.98.1` build provenance in `versions.json`.
- [x] **Step 4: Implement fail-closed packaging** with `Invoke-WebRequest`, SHA-256 verification before extraction, exact single-entry streaming with size/path/link checks and create-new writes, coherent non-DLL PE32+ AMD64 validation, same-volume sibling staging, fail-if-destination-exists publication, and an ordered `SHA256SUMS.txt` without host paths or timestamps.
- [x] **Step 5: Stage official license texts and notices** identifying both upstream projects, pinned full commits, immutable corresponding-source locations and hashes, redistribution obligations, and the requirement for release-channel source artifacts/legal review before public distribution.
- [x] **Step 6: Verify GREEN** by running the package-contract test, then the production packager into an absent destination; run `cores/xray.exe version` and `cores/mihomo.exe -v`, compare archive and extracted hashes to the manifest, and inspect the exact staged inventory. Describe content staging as deterministic while the rolling upstream asset remains available, not as a bit-identical compiler build.

### Task 4: One-process smoke test and handoff

**Files:**
- Modify: `README.md`
- Modify: `state.md`
- Modify: `project-map.md`
- Modify: `session-log.md`
- Create: `scripts/smoke-test-one-click.ps1`
- Test: `dist/multicore-windows-x64/**`

**Security flag:** `security`

**Does NOT cover:** The no-profile smoke proves automatic daemon startup/cleanup, authentication enforcement, and desktop crash containment. It does not launch both cores through an imported live profile or assert a successful privileged TUN session on every Windows installation.

- [x] **Step 1: Replace the two-terminal instructions** with double-click/`Start-Process .\MultiCore.exe`, document generated LocalAppData state, package layout, prerelease versions, and the absence of signing/installer/updater.
- [x] **Step 2: Run fresh repository verification** with `cargo fmt --all -- --check`, `cargo test --workspace --all-targets`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo build --workspace --release`; expect all commands to succeed.
- [x] **Step 3: Run a clean-folder smoke test** by launching only `dist\multicore-windows-x64\MultiCore.exe`, observing an authenticated status response, closing it, and proving no owned `multicore-daemon`, Xray, or Mihomo process remains.
- [x] **Step 4: Validate release contents** with SHA-256 regeneration, PE AMD64 checks for all four executables, secret/path scans, and an exact inventory comparison against the package contract.
- [x] **Step 5: Update durable state** with the chosen stdout handshake, Job Object ownership, pinned prereleases, verification evidence, and the final user-facing path.

## Self-review

- Spec coverage: all package, startup, override, security, failure, cleanup, licensing, and handoff requirements map to Tasks 1-4.
- Placeholder scan: no deferred implementation markers are present.
- Type consistency: readiness uses `MULTICORE_READY {SocketAddr}` from daemon output through desktop parsing; package paths match the design tree.
- Scope scan: installer, signing, updater, service, and privileged TUN portability are explicitly excluded by the approved portable-client scope.
