# Least-Privilege Windows Core Host Implementation Plan

**Goal:** Run the desktop shell, tray, updater, subscription refresh, and HKCU autostart without elevation; request UAC only when a connection needs privileged Xray/Mihomo/TUN ownership.

**Architecture:** `MultiCore.exe` and `multicore-daemon.exe` remain `asInvoker`. On the first connect, the daemon creates a user/SYSTEM-only local named pipe and starts `multicore-core-host.exe` with `ShellExecuteExW(..., "runas")`. The elevated host owns a kill-on-close job containing Xray and Mihomo. The broker accepts a small versioned command set and never accepts executable paths, shell commands, subscription URLs, or credentials. Pipe EOF stops both cores and exits the host.

**Security boundary:** The HTTP daemon bearer and the privileged broker session are separate. The pipe is local-only, message framed, bounded to 64 KiB, restricted to the current user SID and SYSTEM, and authenticated by matching both named-pipe peer PID APIs with the PID returned by `ShellExecuteExW`. Production release remains preview-grade until the host and core binaries are Authenticode-signed or installed beneath an admin-owned directory.

---

### Task 1: Lock the manifest and package contract

**Files:**
- Modify: `Cargo.toml`
- Modify: `apps/multicore-desktop/app.manifest`
- Modify: `apps/multicore-desktop/tests/windows_manifest_contract.rs`
- Create: `apps/multicore-core-host/Cargo.toml`
- Create: `apps/multicore-core-host/build.rs`
- Create: `apps/multicore-core-host/app.manifest`
- Create: `apps/multicore-core-host/src/main.rs`
- Modify: `scripts/test-windows-elevation-manifest.ps1`

- [ ] Add failing tests requiring desktop/daemon/updater `asInvoker` and core-host `requireAdministrator`.
- [ ] Add the minimal host crate and embed numeric RT_MANIFEST resource 1.
- [ ] Change production desktop manifest to `asInvoker`.
- [ ] Keep this branch unreleasable until Tasks 2-4 are complete because connect otherwise lacks TUN rights.
- [ ] Run manifest contracts and `cargo check --workspace --all-targets --locked`.

### Task 2: Add a bounded broker protocol and authenticated named pipe

**Files:**
- Create: `crates/multicore-core/src/elevation_protocol.rs`
- Modify: `crates/multicore-core/src/lib.rs`
- Create: `crates/multicore-core/tests/elevation_protocol.rs`
- Create: `apps/multicore-core-host/src/windows_pipe.rs`
- Create: `crates/multicore-daemon/src/elevated_controller.rs`
- Add focused Windows integration tests beside each implementation.

- [ ] Write RED tests for oversized, truncated, trailing, unknown-version, unknown-command, arbitrary-path, and invalid UTF-8 frames.
- [ ] Define fixed commands only: `StartXray`, `StartMihomo`, `Stop`, `Diagnostics`, `Shutdown`.
- [ ] Keep executable paths and broker secrets out of argv, environment, errors, and logs.
- [ ] Create the pipe with `FILE_FLAG_FIRST_PIPE_INSTANCE`, message mode, `PIPE_REJECT_REMOTE_CLIENTS`, current-user/SYSTEM DACL, and 64 KiB maximum frames.
- [ ] Verify client/server PIDs in both directions before exchanging a fresh broker session secret.
- [ ] Treat pipe EOF as a mandatory cleanup signal and UAC cancellation as `ElevationCancelled`.
- [ ] Run protocol tests, Clippy with warnings denied, and leak scans over command lines/log fixtures.

### Task 3: Move Xray and Mihomo ownership into the elevated host

**Files:**
- Modify: `crates/multicore-core/src/sidecar.rs`
- Modify: `crates/multicore-core/src/supervisor.rs` only if error typing must preserve elevation cancellation.
- Modify: `crates/multicore-daemon/src/main.rs`
- Modify: `crates/multicore-daemon/src/lib.rs`
- Modify: `apps/multicore-core-host/src/main.rs`
- Add daemon/core-host integration tests.

- [ ] Write RED tests proving one host per connection, Xray-before-Mihomo ordering, Mihomo rollback, idempotent reconnect, disconnect cleanup, crash/EOF cleanup, and retry after UAC cancellation.
- [ ] Reuse the existing `ProcessController` boundary through an `ElevatedProcessController`.
- [ ] Stage only validated runtime config data; the host resolves fixed packaged core binaries itself and rejects reparse/path escape.
- [ ] Launch each core directly into a host-owned kill-on-close job before it can run uncontained.
- [ ] Keep existing readiness, diagnostics, rollback, and redacted log semantics.
- [ ] Request elevation lazily on the first `StartXray` after `/v1/connect`; background launch and subscription refresh must never open UAC.
- [ ] Run daemon/core suites and a Windows helper-process cleanup integration test without starting TUN.

### Task 4: Fix installer, autostart, and package inventory

**Files:**
- Modify: `packaging/windows-x64/installer.iss`
- Modify: `scripts/test-windows-installer-contract.ps1`
- Modify: `scripts/package-windows-release.ps1`
- Modify: `scripts/build-windows-installer.ps1`
- Modify: `scripts/smoke-test-one-click.ps1`
- Modify: `crates/multicore-update/src/lib.rs`
- Modify: `crates/multicore-update/tests/archive_contract.rs`
- Modify: `.github/workflows/release.yml`

- [ ] Remove `runas` from desktop postinstall launch and forbid elevated desktop/autostart in contract tests.
- [ ] Preserve exact HKCU Run command: quoted `MultiCore.exe --background`.
- [ ] Add `runtime/multicore-core-host.exe` to every exact package/update inventory.
- [ ] Assert background launch creates desktop + daemon only; core-host appears only after explicit connect.
- [ ] Assert update preserves the existing autostart command and relaunches the new unelevated desktop.
- [ ] Run installer, package, archive, one-click, and update contracts.

### Task 5: Artifact-backed least-privilege verification

**Files:**
- Modify: `scripts/smoke-test-windows-installed-update.ps1`
- Modify: `project-map.md`
- Modify: `state.md`

- [ ] Build the portable package and installer.
- [ ] Verify desktop, daemon, and updater manifests are `asInvoker`; core-host is `requireAdministrator`.
- [ ] Launch `--background` unelevated and prove no core-host/UAC path is entered.
- [ ] Manually accept and cancel UAC from Connect; confirm cancellation returns to Ready, acceptance connects, and Disconnect leaves no host/core processes.
- [ ] Run the installed-update preservation smoke and the full workspace format/test/Clippy/package suite.
- [ ] Record the unsigned-preview security limitation explicitly; do not label the writable portable package security-complete.

