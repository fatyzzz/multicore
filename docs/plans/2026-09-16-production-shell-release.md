# Production Shell and Release Implementation Plan

**Goal:** Ship a compact, selector-correct Windows client with reliable lifecycle behavior, an installer, and a GitHub release/update channel.

### Task 1: Selector semantics

- [ ] Add a regression test proving secondary-group choices do not replace the connection summary.
- [ ] Preserve the daemon primary-group flag and update summaries only from that group.
- [ ] Run the desktop model suite.

### Task 2: Compact Home

- [ ] Replace the 238px hero with a bounded horizontal connection strip.
- [ ] Compact subscription metadata and expand the node viewport.
- [ ] Capture minimum and normal-size fixtures and run UI contract tests.

### Task 3: Tray and autostart

- [ ] Add failing pure tests for one-click tray configuration and background launch commands.
- [ ] Enable the native tray menu on left click.
- [ ] Add `--background`, verified registry round trips, and hidden startup event-loop behavior.
- [ ] Run desktop and Windows-settings tests.

### Task 4: Installer

- [ ] Add an Inno Setup definition and fail-closed build script.
- [ ] Add contract tests for installed inventory, shortcuts, autostart task, and updater compatibility.
- [ ] Build the installer in CI and locally when ISCC is available.

### Task 5: GitHub release channel

- [ ] Add root licenses, release documentation, and a tag-driven Windows workflow.
- [ ] Add tag/version and asset-contract checks.
- [ ] Scan the repository for credentials/private paths, commit, create `fatyzzz/multicore`, and push.
- [ ] Create a release only after the workflow and source-compliance assets are verified.

### Task 6: Verification

- [ ] Run format, full workspace tests, strict Clippy, package/installer contracts, deterministic captures, and one-click smoke without starting TUN.
