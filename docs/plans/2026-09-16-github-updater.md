# GitHub Releases Updater Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers-optimized:subagent-driven-development (recommended) or superpowers-optimized:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a bounded, full-bundle GitHub Releases updater with verified staging and rollback.
**Architecture:** A focused update library owns parsing, download validation, archive validation, and transactional publication. The desktop owns UX and graceful handoff; an out-of-process Rust helper performs replacement after the desktop exits.
**Tech Stack:** Rust 2024, reqwest blocking/Rustls, sha2 0.10, zip 6.0 with deflate only, Slint 1.17, GitHub REST Releases API.
**Assumptions:** Releases contain `multicore-windows-x64.zip` and GitHub publishes a SHA-256 asset digest — older releases without a digest are intentionally rejected. The repository is compile-time pinned — builds without it can check neither GitHub nor install updates.

## Files

- Create `crates/multicore-update/` for update-domain and archive logic.
- Create `apps/multicore-updater/` for the out-of-process apply helper.
- Create `apps/multicore-desktop/src/updater.rs` for UI orchestration.
- Modify workspace/dependency manifests, desktop UI/main, packaging scripts/tests, README, and package notices.

### Task 1: Parse a release safely

**Security flag:** security

**Does NOT cover:** Drafts and prereleases; `/releases/latest` intentionally excludes them.

- [ ] Write failing tests for `owner/repo`, `v1.2.3`, version ordering, exact asset name, exact size, and mandatory `sha256:` digest.
- [ ] Run `cargo test -p multicore-release release` and observe compilation failure because the crate/API is absent.
- [ ] Implement bounded repository, version, and GitHub response types; re-run the target tests.

### Task 2: Validate the complete bundle

**Security flag:** security

- [ ] Write fixture ZIP tests for valid content and rejection of traversal, absolute paths, duplicate normalized paths, symlinks, unlisted/extra files, missing executables, bad hashes, too many entries, and expanded-size limits.
- [ ] Run the archive test target and observe the expected failures.
- [ ] Implement streaming SHA-256 verification and safe extraction with `zip` default features disabled and `deflate` enabled; re-run archive tests.

### Task 3: Publish transactionally

**Security flag:** security

**Does NOT cover:** Runtime crashes after Windows successfully starts the new executable; the old directory is retained for manual recovery on the next launch.

- [ ] Write integration tests that replace a fixture install and that force publication failure after backup creation.
- [ ] Observe both tests fail before the publisher exists.
- [ ] Implement wait, canonical target validation, sibling staging, directory rename, rollback, and relaunch; re-run integration tests.

### Task 4: Add desktop update UX

**Security flag:** security

**Does NOT cover:** Installing while Connected, Connecting, Disconnecting, importing, or applying a route selection; the action remains disabled until Ready.

- [ ] Write update-state tests for unconfigured, checking, current, available, downloading, error, and applying states plus the runtime-idle gate.
- [ ] Implement a background startup check, manual retry, bounded download, helper copy/spawn, and graceful exit handoff.
- [ ] Add a Settings row with current/latest version and one contextual action, plus a title-bar availability dot.
- [ ] Run `cargo test -p multicore-desktop` and the deterministic empty/ready/connected/error previews.

### Task 5: Package the release asset

**Security flag:** security

- [ ] Extend the packaging contract test so it first fails because `runtime/multicore-updater.exe` and `multicore-windows-x64.zip` are absent.
- [ ] Build the helper with the pinned toolchain, include it in `SHA256SUMS.txt`, and produce the deterministic release ZIP beside the portable directory.
- [ ] Pass `MULTICORE_UPDATE_REPOSITORY` through the release build after validating `owner/repository` syntax.
- [ ] Run `scripts/test-package-windows-release.ps1` and verify both directory and archive inventories/digests.

### Task 6: Full verification

**Security flag:** security

- [ ] Run `cargo fmt --all -- --check`, `cargo test --workspace --all-targets --locked`, strict workspace Clippy, the packaging contract, deterministic visual captures, and no-profile one-click smoke.
- [ ] Scan modified production files for `TODO`, `FIXME`, placeholders, and unimplemented branches.
- [ ] Document the exact GitHub release publishing command/asset contract and the unsigned-channel limitation.
