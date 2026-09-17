# Windows Shell, Tray, Resize, and State Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers-optimized:subagent-driven-development (recommended) or superpowers-optimized:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give MultiCore native resize, reliable tray lifecycle, durable desktop preferences, update-safe data, and a restrained pausable Home background.
**Architecture:** Keep Slint as the visual shell and use its Winit 0.30 accessor only for native resize, placement, focus, and monitor information. Store versioned desktop-only preferences atomically under `%LOCALAPPDATA%\MultiCore`; runtime connection state remains daemon-owned and is never restored. Keep all lifecycle decisions in pure Rust functions so tests do not need a live desktop.
**Tech Stack:** Rust 1.98.1, Slint 1.17, Winit 0.30 through `slint::winit_030`, serde/serde_json, Windows per-user filesystem and tray integration, PowerShell release tests.
**Assumptions:** Assumes the existing custom frame remains — this does not restore native Windows chrome. Assumes a tray can fail independently — hidden startup falls back to a visible window. Assumes reconnect remains explicit — a previous connected state will not trigger automatic TUN startup.

---

## File structure

- Create `apps/multicore-desktop/src/preferences.rs`: preference schema, migration, atomic load/save, corruption quarantine, and data-root resolution.
- Create `apps/multicore-desktop/src/windows_shell.rs`: pure lifecycle/geometry decisions plus Winit resize-direction conversion.
- Modify `apps/multicore-desktop/src/main.rs`: restore/save placement and page, pause/resume ambient motion, hide-to-tray minimize/close, and native resize wiring.
- Modify `apps/multicore-desktop/ui/app.slint`: eight resize hit regions, shell activity properties, and two low-opacity waves behind Home content.
- Modify `apps/multicore-desktop/Cargo.toml`: add only Windows APIs required to harden preference files.
- Modify `scripts/smoke-test-windows-installed-update.ps1`: prove updates preserve mutable application data and do not leak subscription URLs.
- Modify `scripts/run-preview-capture.ps1`: capture minimum/default/wide shell states for visual review.

### Task 1: Atomic desktop preferences

**Files:**
- Create: `apps/multicore-desktop/src/preferences.rs`
- Modify: `apps/multicore-desktop/src/main.rs`
- Modify: `apps/multicore-desktop/Cargo.toml`
- Test: `apps/multicore-desktop/src/preferences.rs`

**Security flag:** `security`

**Does NOT cover:** Preferences never contain live logs, transient errors, connection state, or a plaintext duplicate of a subscription URL.

- [x] **Step 1: Write failing tests**

```rust
#[test]
fn round_trip_keeps_only_durable_desktop_state() {
    let root = tempfile::tempdir().unwrap();
    let store = PreferenceStore::at(root.path().join("preferences.json"));
    let expected = AppPreferences {
        schema_version: 1,
        restored_bounds: Some(WindowBounds { x: 40, y: 60, width: 920, height: 700 }),
        maximized: true,
        visible_page: VisiblePage::Status,
        active_profile_hint: Some("0f15b0f1-2a4b-4e2f-91cc-a82e5fcb5140".into()),
        last_group_by_profile: BTreeMap::from([("profile-a".into(), "group-b".into())]),
        selections_by_profile: BTreeMap::new(),
        ambient_background: true,
    };
    store.save(&expected).unwrap();
    assert_eq!(store.load().unwrap(), expected);
    let raw = std::fs::read_to_string(store.path()).unwrap();
    assert!(!raw.contains("connected"));
    assert!(!raw.contains("https://"));
}

#[test]
fn corrupt_preferences_are_quarantined_without_touching_siblings() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("preferences.json");
    std::fs::write(&path, b"{broken").unwrap();
    std::fs::write(root.path().join("device-identity"), b"sentinel").unwrap();
    let store = PreferenceStore::at(path);
    assert_eq!(store.load().unwrap(), AppPreferences::default());
    assert!(root.path().join("preferences.corrupt.json").is_file());
    assert_eq!(std::fs::read(root.path().join("device-identity")).unwrap(), b"sentinel");
}
```

- [x] **Step 2: Run tests to verify they fail**

Run: `cargo test -p multicore-desktop preferences::tests --locked`
Expected: FAIL because `preferences` and `PreferenceStore` do not exist.

- [x] **Step 3: Implement the preference contract**

Define these exact public data shapes and store entry points:

```rust
pub const PREFERENCES_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowBounds { pub x: i32, pub y: i32, pub width: u32, pub height: u32 }

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VisiblePage { #[default] Home, Status, Settings }

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AppPreferences {
    pub schema_version: u32,
    pub restored_bounds: Option<WindowBounds>,
    pub maximized: bool,
    pub visible_page: VisiblePage,
    pub active_profile_hint: Option<String>,
    pub last_group_by_profile: BTreeMap<String, String>,
    pub selections_by_profile: BTreeMap<String, BTreeMap<String, String>>,
    pub ambient_background: bool,
}

impl PreferenceStore {
    pub fn from_local_app_data() -> io::Result<Self>;
    pub fn at(path: PathBuf) -> Self;
    pub fn path(&self) -> &Path;
    pub fn load(&self) -> io::Result<AppPreferences>;
    pub fn save(&self, value: &AppPreferences) -> io::Result<()>;
}
```

`save` writes `preferences.json.tmp`, calls `sync_all`, atomically renames it, syncs the parent directory where supported, and applies the same owner-only Windows DACL pattern already used for snapshot data. `load` accepts schema 1, returns defaults for a missing file, and renames malformed content to `preferences.corrupt.json` without enumerating or modifying sibling paths. Add `mod preferences;` to `main.rs`.

- [x] **Step 4: Run tests to verify they pass**

Run: `cargo test -p multicore-desktop preferences::tests --locked`
Expected: PASS for round-trip, defaults, unknown schema rejection, atomic replacement, and corruption quarantine.

- [x] **Step 5: Commit**

```powershell
git add apps/multicore-desktop/src/preferences.rs apps/multicore-desktop/src/main.rs apps/multicore-desktop/Cargo.toml Cargo.lock
git commit -m "feat(desktop): persist bounded desktop preferences"
```

### Task 2: Geometry and lifecycle decisions

**Files:**
- Create: `apps/multicore-desktop/src/windows_shell.rs`
- Modify: `apps/multicore-desktop/src/main.rs`
- Test: `apps/multicore-desktop/src/windows_shell.rs`

**Security flag:** `none`

**Does NOT cover:** A tray-unavailable process is never hidden; smoke-test close still exits so release verification cannot strand a process.

- [x] **Step 1: Write failing tests**

```rust
#[test]
fn bounds_from_removed_monitor_are_clamped_to_primary_work_area() {
    let saved = WindowBounds { x: 5000, y: 4000, width: 900, height: 700 };
    let monitors = [MonitorRect { x: 0, y: 0, width: 1920, height: 1040, primary: true }];
    assert_eq!(restore_bounds(saved, &monitors), WindowBounds { x: 510, y: 170, width: 900, height: 700 });
}

#[test]
fn minimize_and_close_hide_only_with_recoverable_tray() {
    assert_eq!(minimize_disposition(true), WindowDisposition::HideToTray);
    assert_eq!(minimize_disposition(false), WindowDisposition::MinimizeToTaskbar);
    assert_eq!(close_disposition(true, false), WindowDisposition::HideToTray);
    assert_eq!(close_disposition(false, false), WindowDisposition::Exit);
    assert_eq!(close_disposition(true, true), WindowDisposition::Exit);
}
```

- [x] **Step 2: Run tests to verify they fail**

Run: `cargo test -p multicore-desktop windows_shell::tests --locked`
Expected: FAIL because the shell decision module does not exist.

- [x] **Step 3: Implement pure shell rules**

```rust
pub const MIN_WIDTH: u32 = 700;
pub const MIN_HEIGHT: u32 = 620;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowDisposition { HideToTray, MinimizeToTaskbar, Exit }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResizeEdge { North, NorthEast, East, SouthEast, South, SouthWest, West, NorthWest }

pub fn minimize_disposition(tray_available: bool) -> WindowDisposition;
pub fn close_disposition(tray_available: bool, smoke_mode: bool) -> WindowDisposition;
pub fn restore_bounds(saved: WindowBounds, monitors: &[MonitorRect]) -> WindowBounds;
pub fn parse_resize_edge(value: &str) -> Option<ResizeEdge>;
```

Clamp width/height to the selected monitor work area, keep at least 64 logical pixels of the title bar reachable, and center on the primary work area when the saved rectangle does not intersect any current monitor. Under Windows, map `ResizeEdge` to Winit `ResizeDirection`; keep that conversion behind `cfg(windows)`.

- [x] **Step 4: Run tests to verify they pass**

Run: `cargo test -p multicore-desktop windows_shell::tests --locked`
Expected: PASS including multi-monitor negative coordinates, oversized saved bounds, and all eight resize strings.

- [x] **Step 5: Commit**

```powershell
git add apps/multicore-desktop/src/windows_shell.rs apps/multicore-desktop/src/main.rs
git commit -m "feat(desktop): define native shell lifecycle rules"
```

### Task 3: Wire native resize, restore, and tray semantics

**Files:**
- Modify: `apps/multicore-desktop/src/main.rs`
- Modify: `apps/multicore-desktop/ui/app.slint`
- Test: `apps/multicore-desktop/src/main.rs`

**Security flag:** `none`

**Does NOT cover:** This task does not persist connected state and does not switch profiles from the tray.

- [x] **Step 1: Extend source-contract tests before wiring**

```rust
#[test]
fn shell_wiring_has_resize_and_preference_flush_boundaries() {
    let source = include_str!("main.rs");
    assert!(source.contains("ui.on_window_resize"));
    assert!(source.contains("drag_resize_window"));
    assert!(source.contains("PreferenceStore::from_local_app_data"));
    assert!(source.contains("TimerMode::Repeated"));
    assert!(source.contains("minimize_disposition(tray_available)"));
}
```

- [x] **Step 2: Run the focused test to verify it fails**

Run: `cargo test -p multicore-desktop window_tests::shell_wiring_has_resize_and_preference_flush_boundaries --locked`
Expected: FAIL because resize and preference wiring are absent.

- [x] **Step 3: Restore and observe native placement**

Before `ui.show()`, load preferences, set `local-page`, apply clamped restored outer position/inner size through `with_winit_window`, and restore maximized state last. Start a 500 ms repeated timer that reads position, inner size, maximized state, and page; write only after a value has remained unchanged for one tick. Never write while minimized or while no native window is available.

Remove the fixed Slint `max-width`/`max-height`. On restore and monitor changes, set Winit's maximum inner size to the current monitor work area while retaining the `700x620` minimum; recompute before starting a native edge drag so moving the window between monitors cannot retain a stale cap.

- [x] **Step 4: Wire all window callbacks**

`window-drag` keeps `drag_window()`. `window-resize(edge)` parses the bounded edge name and invokes Winit `drag_resize_window`. `window-minimize` uses `HideToTray` or `set_minimized(true)`. Both title-bar and native close requests use the same close decision. `ShowWindow` restores, shows, focuses, requests redraw, and sets `shell-active = true`. Explicit tray Exit flushes preferences before `quit_event_loop`.

- [x] **Step 5: Run desktop tests**

Run: `cargo test -p multicore-desktop --all-targets --locked`
Expected: PASS; existing background-start, smoke-close, tray, import, and updater tests remain green.

- [x] **Step 6: Commit**

```powershell
git add apps/multicore-desktop/src/main.rs apps/multicore-desktop/ui/app.slint
git commit -m "feat(desktop): add native resize and tray-first window lifecycle"
```

### Task 4: Add the restrained ambient background

**Files:**
- Modify: `apps/multicore-desktop/ui/app.slint`
- Modify: `apps/multicore-desktop/src/main.rs`
- Test: `apps/multicore-desktop/src/view_model.rs`

**Security flag:** `none`

**Does NOT cover:** The waves do not carry status meaning, accept input, use saturated color, or animate while hidden/inactive/reduced-motion is active.

- [x] **Step 1: Add a failing UI contract test**

```rust
#[test]
fn home_background_is_bounded_pausable_and_non_interactive() {
    let source = include_str!("../ui/app.slint");
    assert!(source.contains("ambient-background-enabled"));
    assert!(source.contains("shell-active"));
    assert!(source.contains("wave-timer"));
    assert!(source.contains("opacity: 0.03"));
    assert!(!source.contains("scanline"));
    assert!(!source.contains("particle"));
}
```

- [x] **Step 2: Run the test to verify it fails**

Run: `cargo test -p multicore-desktop view_model::tests::home_background_is_bounded_pausable_and_non_interactive --locked`
Expected: FAIL because ambient properties and the wave layer do not exist.

- [x] **Step 3: Implement the wave layer behind Home content**

Add `ambient-background-enabled`, `shell-active`, and `reduced-motion` inputs. Put two clipped, broad gray-white vector/path waves as the first children of `main-pane`; use opacities `0.02` and `0.03`, no blur, and at most 10 px translation. A 12-second Slint timer toggles the phase only when all three gates permit motion; the x/y animations use ease-in-out and 12–18 second durations. All existing cards remain opaque enough to preserve contrast and every wave has no `TouchArea`.

- [x] **Step 4: Wire visibility and reduced-motion state**

Set `shell-active` false before hiding/minimizing and true on restore/show. On Windows, read `SPI_GETCLIENTAREAANIMATION` once during startup; set `reduced-motion = true` when client-area animation is disabled. Persist only the user-facing ambient enabled setting.

- [x] **Step 5: Run compile and UI tests**

Run: `cargo test -p multicore-desktop --all-targets --locked`
Expected: PASS and Slint compilation succeeds at minimum dimensions.

- [x] **Step 6: Commit**

```powershell
git add apps/multicore-desktop/ui/app.slint apps/multicore-desktop/src/main.rs apps/multicore-desktop/src/view_model.rs
git commit -m "feat(desktop): add pausable ambient home waves"
```

### Task 5: Prove updater data preservation

**Files:**
- Modify: `scripts/smoke-test-windows-installed-update.ps1`
- Test: `scripts/smoke-test-windows-installed-update.ps1`

**Security flag:** `security`

**Does NOT cover:** Uninstall may remove the installation directory; this assertion concerns in-app updates and requires mutable data to remain outside that directory.

- [x] **Step 1: Add failing sentinel assertions**

Before invoking the updater, create a test-local `LOCALAPPDATA\MultiCore` containing `preferences.json`, `profiles\index.json`, a profile `subscription.json` with `https://sentinel.invalid/private-token`, `device-identity`, and `logs\latest-core.log`. Hash every sentinel file.

- [ ] **Step 2: Run the installed-update smoke test before preservation wiring**

Run: `powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts\smoke-test-windows-installed-update.ps1 -InstallerPath dist\release\MultiCore-Setup-x64.exe -PackagePath dist\multicore-windows-x64`
Expected: FAIL at the new assertion if the test process does not isolate and preserve its mutable data root.

- [x] **Step 3: Isolate and verify the mutable root**

Set `LOCALAPPDATA` only for the helper/update process, keep the update target equal to `$installRoot\current`, and after apply assert all sentinel hashes match. Capture updater stdout/stderr and assert it contains neither `private-token` nor the full subscription URL. Resolve every recursive cleanup target beneath the GUID-named temporary root before removal.

- [ ] **Step 4: Run updater and installer contracts**

Run: `powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts\test-windows-installer-contract.ps1`
Expected: PASS.

Run: the installed-update smoke command from Step 2 with current artifacts.
Expected: PASS with byte-identical mutable sentinels.

- [x] **Step 5: Commit**

```powershell
git add scripts/smoke-test-windows-installed-update.ps1
git commit -m "test(update): preserve multicore user state across updates"
```

### Task 6: Native visual and release verification

**Files:**
- Modify: `scripts/run-preview-capture.ps1`
- Modify: `project-map.md`
- Modify: `state.md`
- Test: workspace and Windows packaging scripts

**Security flag:** `none`

- [ ] **Step 1: Capture responsive states**

Extend the capture script with `minimum` (`700x620`), `default` (`840x720`), and `wide` (`1100x760`) scenarios. Capture Home idle and hovered connection pill without starting TUN. Visually reject wave contrast above the connection mesh or any resize border visible at rest.

- [ ] **Step 2: Run strict verification**

Run: `cargo fmt --all -- --check`
Expected: PASS.

Run: `cargo test --workspace --all-targets --locked`
Expected: PASS; ignored live-core/TUN tests remain explicitly ignored.

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`
Expected: PASS with zero warnings.

Run: `powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts\test-package-windows-release.ps1`
Expected: PASS.

Run: `powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts\run-preview-capture.ps1`
Expected: PASS and emit all responsive captures.

- [ ] **Step 3: Record the verified shell contract**

Update `project-map.md` with `preferences.rs` and `windows_shell.rs`. Replace `state.md` with the delivered behavior, exact test evidence, artifact path, and the remaining manual TUN check.

- [ ] **Step 4: Commit**

```powershell
git add scripts/run-preview-capture.ps1 project-map.md state.md
git commit -m "docs: record verified native shell lifecycle"
```
