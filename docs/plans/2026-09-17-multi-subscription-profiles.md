# Multi-Subscription Profiles Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers-optimized:subagent-driven-development (recommended) or superpowers-optimized:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Support up to 16 isolated subscription profiles with one authoritative active profile, offline legacy migration, per-profile selectors, authenticated APIs, and a compact anchored desktop switcher.
**Architecture:** Add a core `ProfileStore` above the existing generation-based `PersistentSnapshotStore`; each profile owns its own snapshot root and selector state, while one atomically written index owns ordering and the active ID. Refactor the daemon runtime to load and publish only the active profile, expose safe profile DTOs without URLs, and reject activation/deletion while connected. Extend the desktop client/view model/UI after the daemon contract is executable and tested.
**Tech Stack:** Rust 1.98.1, Tokio/Axum, serde/serde_json, existing atomic snapshot store, reqwest loopback client, Slint 1.17.
**Assumptions:** Assumes profiles never merge — selecting Mihomo from one URL and Xray from another is unsupported. Assumes exact trimmed URL equality is the duplicate rule — semantically equivalent rewritten URLs remain distinct. Assumes profile activation requires disconnected state — connected hot-swap is rejected with HTTP 409.

---

## File structure

- Create `crates/multicore-core/src/profile.rs`: IDs, index, profile stores, offline migration, selector persistence, and atomic profile operations.
- Modify `crates/multicore-core/src/lib.rs`: export bounded profile types.
- Modify `crates/multicore-core/Cargo.toml`: add `tempfile` for profile-store integration tests.
- Create `crates/multicore-core/tests/profile_store.rs`: migration, isolation, corruption, duplicate lookup, selector, and rollback coverage.
- Modify `crates/multicore-daemon/src/lib.rs`: profile-aware backend trait/runtime/API and safe DTOs.
- Modify `crates/multicore-daemon/src/main.rs`: open the profile store using legacy snapshots as migration input.
- Create `crates/multicore-daemon/tests/subscription_profiles_api.rs`: authenticated endpoint/status/error contract.
- Modify `apps/multicore-desktop/src/daemon.rs`: profile DTOs and requests.
- Modify `apps/multicore-desktop/src/view_model.rs`: profile-list mutation state and safe presentation.
- Modify `apps/multicore-desktop/src/main.rs`: async profile list/activate/refresh/delete wiring.
- Modify `apps/multicore-desktop/ui/app.slint`: anchored subscription menu and Settings add flow.
- Modify `apps/multicore-desktop/src/preferences.rs`: validate profile restore hints and persist per-profile group/selection choices.

### Task 1: Profile identity and atomic index

**Files:**
- Create: `crates/multicore-core/src/profile.rs`
- Modify: `crates/multicore-core/src/lib.rs`
- Modify: `crates/multicore-core/Cargo.toml`
- Create: `crates/multicore-core/tests/profile_store.rs`

**Security flag:** `security`

**Does NOT cover:** Profile IDs are locally generated opaque UUIDs; provider titles, hosts, and URLs never become IDs or directory names.

- [ ] **Step 1: Write failing index and isolation tests**

```rust
#[test]
fn index_contains_no_source_urls_and_only_one_active_profile() {
    let root = tempfile::tempdir().unwrap();
    let store = ProfileStore::open(root.path().join("profiles"), None).unwrap();
    let first = store.create_profile(snapshot("https://one.invalid/private-a")).unwrap();
    let second = store.create_profile(snapshot("https://two.invalid/private-b")).unwrap();
    store.activate(&first).unwrap();
    assert_eq!(store.active_id(), Some(first.clone()));
    let index = std::fs::read_to_string(root.path().join("profiles/index.json")).unwrap();
    assert!(!index.contains("https://"));
    assert!(!index.contains("private-a"));
    assert!(!index.contains("private-b"));
    assert_eq!(store.list().unwrap().len(), 2);
    assert_ne!(first, second);
}

#[test]
fn seventeenth_profile_is_rejected_without_changing_index() {
    let root = tempfile::tempdir().unwrap();
    let store = ProfileStore::open(root.path().join("profiles"), None).unwrap();
    for index in 0..16 { store.create_profile(snapshot(&format!("https://{index}.invalid/sub"))).unwrap(); }
    let before = std::fs::read(root.path().join("profiles/index.json")).unwrap();
    assert!(matches!(store.create_profile(snapshot("https://overflow.invalid/sub")), Err(ProfileError::LimitReached)));
    assert_eq!(std::fs::read(root.path().join("profiles/index.json")).unwrap(), before);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p multicore-core --test profile_store --locked`
Expected: FAIL because `ProfileStore` and profile types do not exist.

- [ ] **Step 3: Implement bounded profile primitives**

```rust
pub const MAX_PROFILES: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProfileId(String);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileSummary {
    pub id: ProfileId,
    pub created_at_unix: u64,
    pub active: bool,
    pub available: bool,
}

pub struct ProfileStore { root: PathBuf, index: RwLock<ProfileIndex> }

impl ProfileStore {
    pub fn open(root: impl Into<PathBuf>, legacy_root: Option<PathBuf>) -> Result<Self, ProfileError>;
    pub fn list(&self) -> Result<Vec<ProfileSummary>, ProfileError>;
    pub fn active_id(&self) -> Option<ProfileId>;
    pub fn active_snapshot(&self) -> Result<Option<(ProfileId, u64, Arc<Snapshot>)>, ProfileError>;
    pub fn create_profile(&self, snapshot: Arc<Snapshot>) -> Result<ProfileId, ProfileError>;
    pub fn replace_profile(&self, id: &ProfileId, snapshot: Arc<Snapshot>) -> Result<u64, ProfileError>;
    pub fn activate(&self, id: &ProfileId) -> Result<Arc<Snapshot>, ProfileError>;
    pub fn delete(&self, id: &ProfileId) -> Result<Option<ProfileId>, ProfileError>;
    pub fn find_exact_source(&self, trimmed_url: &str) -> Result<Option<ProfileId>, ProfileError>;
}
```

Generate 16 random bytes, set RFC 4122 version-4 and variant bits, and format lowercase UUID text. Validate all deserialized IDs before joining paths. Write `index.json.tmp`, sync, rename, and apply owner-only permissions. Publish an index only after the target profile snapshot is loadable.

Deletion first commits an index without the profile, then removes only the resolved
`profiles/<validated-id>` directory. Cleanup failure is reported as maintenance debt and
cannot resurrect the removed index entry.

- [ ] **Step 4: Run profile-store tests**

Run: `cargo test -p multicore-core --test profile_store --locked`
Expected: PASS for unique IDs, source secrecy, limit, create/replace/activate/delete isolation, and corrupt child handling.

- [ ] **Step 5: Commit**

```powershell
git add crates/multicore-core/src/profile.rs crates/multicore-core/src/lib.rs crates/multicore-core/Cargo.toml Cargo.lock crates/multicore-core/tests/profile_store.rs
git commit -m "feat(core): add isolated subscription profile store"
```

### Task 2: Offline legacy migration and selector persistence

**Files:**
- Modify: `crates/multicore-core/src/profile.rs`
- Modify: `crates/multicore-core/tests/profile_store.rs`

**Security flag:** `security`

**Does NOT cover:** Migration never performs HTTP requests, deletes legacy generations, or guesses renamed group/node IDs by list position.

- [ ] **Step 1: Add failing migration and selection tests**

```rust
#[test]
fn legacy_generation_migrates_offline_and_remains_as_rollback_material() {
    let root = tempfile::tempdir().unwrap();
    let legacy = root.path().join("snapshots");
    PersistentSnapshotStore::open(&legacy).unwrap().commit((*snapshot("https://legacy.invalid/secret")).clone()).unwrap();
    let store = ProfileStore::open(root.path().join("profiles"), Some(legacy.clone())).unwrap();
    let (_, _, migrated) = store.active_snapshot().unwrap().unwrap();
    assert_eq!(migrated.subscription_source_url(), Some("https://legacy.invalid/secret"));
    assert!(legacy.join("snapshot-00000000000000000001").is_dir());
}

#[test]
fn selections_restore_only_when_group_and_node_still_exist() {
    let root = tempfile::tempdir().unwrap();
    let store = ProfileStore::open(root.path().join("profiles"), None).unwrap();
    let id = store.create_profile(snapshot("https://one.invalid/sub")).unwrap();
    store.save_selection(&id, 7, "group-a", "node-b").unwrap();
    assert_eq!(store.valid_selections(&id, 7, &[(&"group-a", &["node-b", "node-c"])]).unwrap()["group-a"], "node-b");
    assert!(store.valid_selections(&id, 8, &[(&"group-a", &["node-c"])]).unwrap().is_empty());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p multicore-core --test profile_store legacy_generation_migrates_offline selections_restore_only --locked`
Expected: FAIL because migration and selection APIs are absent.

- [ ] **Step 3: Implement migration and per-profile selections**

When `profiles/index.json` is absent, load the latest valid legacy snapshot, create one profile store, commit the full snapshot including cached logo bytes, reload it, then publish the index. Leave legacy paths untouched. Store `selections.json` as `{schema_version, catalog_revision, selections}` beside each profile snapshot root; apply the same temp/sync/rename discipline. Expose:

```rust
pub fn save_selection(&self, id: &ProfileId, revision: u64, group_id: &str, node_id: &str) -> Result<(), ProfileError>;
pub fn valid_selections(&self, id: &ProfileId, revision: u64, catalog: &[(&str, &[&str])]) -> Result<BTreeMap<String, String>, ProfileError>;
```

Bound IDs to 256 bytes of visible ASCII, reject separators/control bytes, and discard invalid selections without failing profile startup.

- [ ] **Step 4: Run core tests**

Run: `cargo test -p multicore-core --all-targets --locked`
Expected: PASS; existing byte-exact Xray and snapshot rollback tests remain green.

- [ ] **Step 5: Commit**

```powershell
git add crates/multicore-core/src/profile.rs crates/multicore-core/tests/profile_store.rs
git commit -m "feat(core): migrate legacy profile and persist selectors"
```

### Task 3: Profile-aware daemon backend

**Files:**
- Modify: `crates/multicore-daemon/src/lib.rs`
- Modify: `crates/multicore-daemon/src/main.rs`
- Modify: `crates/multicore-daemon/tests/core_backend.rs`
- Test: `crates/multicore-daemon/tests/core_backend.rs`

**Security flag:** `security`

**Does NOT cover:** Activation and deletion return conflicts while connecting/connected/degraded; no core process is hot-swapped.

- [ ] **Step 1: Add failing backend behavior tests**

```rust
#[tokio::test]
async fn profiles_stay_isolated_and_connected_activation_is_rejected() {
    let backend = profile_backend_fixture().await;
    let first = backend.import_subscription(ImportSubscriptionRequest { url: "https://one.invalid/sub".into() }).await.unwrap();
    let first_id = first.active_profile_id.unwrap();
    let second = backend.import_subscription(ImportSubscriptionRequest { url: "https://two.invalid/sub".into() }).await.unwrap();
    let second_id = second.active_profile_id.unwrap();
    assert_ne!(first_id, second_id);
    backend.connect().await.unwrap();
    assert!(matches!(backend.activate_subscription(first_id).await, Err(ProfileMutationError::Connected)));
    backend.disconnect().await.unwrap();
    let restored = backend.activate_subscription(first_id).await.unwrap();
    assert_eq!(restored.active_profile_id.as_deref(), Some(first_id.as_str()));
}
```

- [ ] **Step 2: Run the focused test to verify it fails**

Run: `cargo test -p multicore-daemon --test core_backend profiles_stay_isolated --locked`
Expected: FAIL because the backend still owns one `PersistentSnapshotStore`.

- [ ] **Step 3: Refactor runtime ownership**

Replace `CoreBackend.store` with `ProfileStore`. Add `active_profile_id: Option<ProfileId>` to `CoreRuntime`. Constructor startup loads only `active_snapshot()`. Import fetches into `AtomicSnapshot`, finds an exact trimmed URL match, replaces or creates that profile, activates it, and then prepares its controller. Targeted refresh replaces only the chosen store; inactive refresh never changes runtime. Activation rebuilds catalog/controller metadata after the profile index commit. Delete selects the next ordered profile and loads it, or returns Empty.

Add `active_profile_id: Option<String>` to `StatusDto`; it always comes from the
authoritative profile index and is `None` only in the empty state.

Define the mutation error used by both backend tests and HTTP mapping:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProfileMutationError { NotFound, Connected, LimitReached, Storage }
```

- [ ] **Step 4: Persist confirmed selector changes**

After a ready-state queued choice is accepted, store it for the active profile. While connected, write only after Mihomo's live `now` response confirms the selected node. During snapshot/catalog rebuild, restore only IDs present in that catalog revision.

- [ ] **Step 5: Update daemon bootstrap**

Open `ProfileStore::open(data_directory.join("profiles"), Some(data_directory.join("snapshots")))` and pass it to `CoreBackend::new_transactional_with_selector`. Keep runtime/log/device paths unchanged.

- [ ] **Step 6: Run daemon backend suites**

Run: `cargo test -p multicore-daemon --test core_backend --test selection_api --locked`
Expected: PASS for import rollback, connect/disconnect, profile conflicts, and confirmed selection persistence.

- [ ] **Step 7: Commit**

```powershell
git add crates/multicore-daemon/src/lib.rs crates/multicore-daemon/src/main.rs crates/multicore-daemon/tests/core_backend.rs crates/multicore-daemon/tests/selection_api.rs
git commit -m "feat(daemon): make runtime profile-aware"
```

### Task 4: Authenticated profile HTTP API

**Files:**
- Modify: `crates/multicore-daemon/src/lib.rs`
- Create: `crates/multicore-daemon/tests/subscription_profiles_api.rs`

**Security flag:** `security`

**Does NOT cover:** Responses never include source URLs, raw configs, credential-bearing paths, or provider-controlled unbounded strings. The existing validated app-owned service-logo cache path remains allowed for the local authenticated desktop client.

- [ ] **Step 1: Write failing endpoint tests**

```rust
#[tokio::test]
async fn list_is_authenticated_safe_and_activation_conflicts_are_typed() {
    let app = router(profile_backend(), "secret");
    let unauthorized = request(&app, Method::GET, "/v1/subscriptions", None, None).await;
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    let listed = request(&app, Method::GET, "/v1/subscriptions", Some("secret"), None).await;
    assert_eq!(listed.status(), StatusCode::OK);
    let body = body_text(listed).await;
    assert!(!body.contains("https://"));
    assert!(!body.contains("token="));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p multicore-daemon --test subscription_profiles_api --locked`
Expected: FAIL because the collection and targeted routes are absent.

- [ ] **Step 3: Define safe DTOs and backend methods**

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubscriptionListDto { pub active_profile_id: Option<String>, pub profiles: Vec<SubscriptionProfileDto> }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubscriptionProfileDto {
    pub id: String,
    pub active: bool,
    pub available: bool,
    pub display_name: String,
    pub used_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
    pub expires_at_unix: Option<u64>,
    pub service_logo_path: Option<String>,
}
```

Extend `Backend` with list, activate, targeted refresh, and delete methods; implement them in `CoreBackend` and `MemoryBackend`.

Response shapes stay explicit: list and targeted refresh return
`SubscriptionListDto`; activation and deletion return `StatusDto`; the existing import
and active-refresh endpoints continue returning `StatusDto` for compatibility. The
desktop reloads the list after every successful mutation.

- [ ] **Step 4: Register exact routes and errors**

Register `GET /subscriptions`, `PUT /subscriptions/{profile_id}/activate`, `POST /subscriptions/{profile_id}/refresh`, and `DELETE /subscriptions/{profile_id}` before fallback. Validate opaque IDs before backend calls. Map missing IDs to `404 profile_not_found`, connected/degraded mutation to `409 profile_busy`, limit to `409 profile_limit`, and all internal failures to the existing redacted error shape. Keep `POST /subscriptions/refresh` as the active-profile alias.

- [ ] **Step 5: Run API suites**

Run: `cargo test -p multicore-daemon --test subscription_profiles_api --test http_api --locked`
Expected: PASS for authentication, methods, body limits, 404/409 mapping, and redaction.

- [ ] **Step 6: Commit**

```powershell
git add crates/multicore-daemon/src/lib.rs crates/multicore-daemon/tests/subscription_profiles_api.rs
git commit -m "feat(api): expose safe subscription profile operations"
```

### Task 5: Desktop client and view-model profile state

**Files:**
- Modify: `apps/multicore-desktop/src/daemon.rs`
- Modify: `apps/multicore-desktop/src/view_model.rs`
- Test: `apps/multicore-desktop/src/daemon.rs`
- Test: `apps/multicore-desktop/src/view_model.rs`

**Security flag:** `security`

**Does NOT cover:** Desktop debug output and presentation types do not store or print source URLs.

- [ ] **Step 1: Write failing client and state-machine tests**

```rust
#[test]
fn profile_list_decodes_without_a_source_url_field() {
    let value: DaemonSubscriptionList = serde_json::from_str(r#"{"active_profile_id":"a","profiles":[{"id":"a","active":true,"available":true,"display_name":"Work","used_bytes":1,"total_bytes":2,"expires_at_unix":null,"service_logo_path":null}]}"#).unwrap();
    assert_eq!(value.profiles[0].display_name, "Work");
    assert!(!format!("{value:?}").contains("https://"));
}

#[test]
fn stale_profile_mutation_cannot_replace_a_newer_list() {
    let mut model = ready_model();
    let old = model.begin_profile_activation("a").unwrap();
    let newer = model.begin_profile_list().unwrap();
    assert!(!model.finish_profile_activation(old, Ok(status_for("a"))));
    assert!(model.finish_profile_list(newer, Ok(list_with_active("b"))));
    assert_eq!(model.profile_presentation().active_id.as_deref(), Some("b"));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p multicore-desktop daemon::tests::profile_list view_model::tests::stale_profile_mutation --locked`
Expected: FAIL because profile DTOs and mutations are absent.

- [ ] **Step 3: Extend `DaemonClient`**

Add `subscriptions`, `activate_subscription`, `refresh_subscription_by_id`, and `delete_subscription`. Percent-encode IDs as single path segments, enforce existing bounded JSON response limits, classify 404/409 safely, and add loopback request tests proving bearer auth and exact methods.

- [ ] **Step 4: Add profile presentation and mutation tokens**

Define `SubscriptionProfilePresentation` and `SubscriptionProfilesPresentation` with bounded title/usage/expiry/logo state plus loading/error/pending ID. Follow existing monotonic mutation tokens: import success refreshes list and catalog; activate success clears stale catalog/latencies then reloads; targeted refresh changes only matching presentation metadata; delete closes the menu when the last profile is removed. Reject mutations in connected/busy presentation states before HTTP.

- [ ] **Step 5: Run desktop unit tests**

Run: `cargo test -p multicore-desktop --all-targets --locked`
Expected: PASS including stale-response suppression and secret-redaction tests.

- [ ] **Step 6: Commit**

```powershell
git add apps/multicore-desktop/src/daemon.rs apps/multicore-desktop/src/view_model.rs
git commit -m "feat(desktop): model subscription profile operations"
```

### Task 6: Compact anchored profile switcher

**Files:**
- Modify: `apps/multicore-desktop/src/main.rs`
- Modify: `apps/multicore-desktop/ui/app.slint`
- Modify: `apps/multicore-desktop/src/preferences.rs`
- Modify: `apps/multicore-desktop/src/view_model.rs`
- Test: `apps/multicore-desktop/src/view_model.rs`

**Security flag:** `security`

**Does NOT cover:** No permanent sidebar page, merged server list, tray profile switcher, or visible saved URL list is introduced.

- [ ] **Step 1: Add failing UI contract tests**

```rust
#[test]
fn subscription_shelf_opens_compact_anchored_profile_menu() {
    let source = include_str!("../ui/app.slint");
    assert!(source.contains("profile-menu-open"));
    assert!(source.contains("subscription-profiles"));
    assert!(source.contains("activate-subscription-profile"));
    assert!(source.contains("delete-subscription-profile"));
    assert!(source.contains("Добавить подписку"));
    assert!(!source.contains("NavItem { label: \"Подписки\""));
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p multicore-desktop view_model::tests::subscription_shelf_opens_compact_anchored_profile_menu --locked`
Expected: FAIL because the profile menu properties/callbacks are absent.

- [ ] **Step 3: Wire asynchronous operations**

Load the profile list after startup status, import, activation, targeted refresh, and deletion. Each callback takes a view-model token, performs daemon work off the event loop, then applies only current completions. Activation/deletion while connected surface one bounded inline message and do not close the menu.

- [ ] **Step 4: Implement the anchored menu**

Make the subscription strip itself focusable/clickable except its refresh action. Open a 320–380 px anchored surface directly below it, capped to available height with scrolling. Each row shows bounded logo/title/usage-expiry, active check, compact refresh, and a delete action behind an in-row confirmation. The footer contains `Добавить подписку`, which opens Settings with an empty URL editor. Escape, outside click, page change, or successful activation closes the menu.

- [ ] **Step 5: Restore per-profile UI hints**

After receiving the authoritative daemon list, accept `active_profile_hint` only if it equals the daemon active ID; otherwise overwrite the hint. Restore `last_group_by_profile` only when that opaque group exists. Apply saved selector hints only through the existing ready-state selection pipeline, never by editing generated configuration files.

- [ ] **Step 6: Run native captures and desktop tests**

Run: `cargo test -p multicore-desktop --all-targets --locked`
Expected: PASS.

Run: `powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts\run-preview-capture.ps1`
Expected: PASS with empty, one-profile, many-profile, loading, and error menu captures at 700x620 and 840x720.

- [ ] **Step 7: Commit**

```powershell
git add apps/multicore-desktop/src/main.rs apps/multicore-desktop/ui/app.slint apps/multicore-desktop/src/preferences.rs apps/multicore-desktop/src/view_model.rs scripts/run-preview-capture.ps1
git commit -m "feat(desktop): add compact subscription profile switcher"
```

### Task 7: Workspace verification and update-preservation regression

**Files:**
- Modify: `scripts/smoke-test-windows-installed-update.ps1`
- Modify: `README.md`
- Modify: `project-map.md`
- Modify: `state.md`
- Test: workspace, updater, package, and capture suites

**Security flag:** `security`

- [ ] **Step 1: Expand update sentinels to two profiles**

Create two profile roots with distinct safe fixtures, exact selector files, active ID, preferences hint, and a credential-bearing URL marker. Apply the packaged update and assert byte-identical profiles/index/preferences/device identity, no credential in output, and no mutation outside update staging/install payload.

- [ ] **Step 2: Run strict verification**

Run: `cargo fmt --all -- --check`
Expected: PASS.

Run: `cargo test --workspace --all-targets --locked`
Expected: PASS; live TUN tests remain ignored.

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`
Expected: PASS with zero warnings.

Run: `powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts\test-package-windows-release.ps1`
Expected: PASS.

Run: the installed-update smoke command with current installer/package.
Expected: PASS with both profiles preserved.

- [ ] **Step 3: Document operator-visible behavior**

Update README with the 16-profile limit, one-active/no-merge rule, offline migration, connected mutation restriction, data paths, and safe API endpoint list. Update `project-map.md` and `state.md` with final module ownership, evidence, artifact path, and manual live-TUN/profile-switch checks.

- [ ] **Step 4: Commit**

```powershell
git add scripts/smoke-test-windows-installed-update.ps1 README.md project-map.md state.md
git commit -m "docs: record verified multi-subscription behavior"
```
