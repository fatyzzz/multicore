use std::{collections::BTreeMap, fs, path::PathBuf, sync::Arc, time::SystemTime};

use multicore_core::{
    MAX_PROFILE_INDEX_BYTES, MAX_PROFILES, MAX_SELECTIONS, PersistentSnapshotStore, ProfileError,
    ProfileStore, Snapshot, normalize_service_logo,
};

#[cfg(windows)]
use std::path::Path;

fn snapshot(source: &str, name: &str) -> Arc<Snapshot> {
    let mihomo = format!(
        "proxies:\n  - name: {name}\n    type: socks5\n    server: 127.0.0.1\n    port: 31001\nproxy-groups:\n  - name: Proxy\n    type: select\n    proxies: [{name}]\nrules:\n  - MATCH,Proxy\n"
    );
    Arc::new(
        Snapshot::parse(mihomo.as_bytes(), br#"{"outbounds":[]}"#)
            .unwrap()
            .with_subscription_source(source, None, 1_700_000_000)
            .unwrap(),
    )
}

struct TempDirectory(tempfile::TempDir);

impl TempDirectory {
    fn new() -> Self {
        Self(tempfile::tempdir().unwrap())
    }

    fn profiles(&self) -> PathBuf {
        self.0.path().join("profiles")
    }

    fn path(&self) -> &std::path::Path {
        self.0.path()
    }
}

#[derive(Debug, PartialEq, Eq)]
struct TreeEntry {
    directory: bool,
    bytes: Vec<u8>,
    len: u64,
    readonly: bool,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    mode: u32,
}

fn capture_tree(root: &std::path::Path) -> BTreeMap<String, TreeEntry> {
    fn entry_state(path: &std::path::Path, directory: bool) -> TreeEntry {
        let metadata = fs::symlink_metadata(path).unwrap();
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            metadata.permissions().mode()
        };
        TreeEntry {
            directory,
            bytes: if directory {
                Vec::new()
            } else {
                fs::read(path).unwrap()
            },
            len: metadata.len(),
            readonly: metadata.permissions().readonly(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            mode,
        }
    }

    fn walk(
        root: &std::path::Path,
        path: &std::path::Path,
        result: &mut BTreeMap<String, TreeEntry>,
    ) {
        let mut entries = fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap())
            .collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).unwrap();
            let directory = metadata.is_dir();
            result.insert(
                path.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/"),
                entry_state(&path, directory),
            );
            if directory {
                walk(root, &path, result);
            }
        }
    }

    let mut result = BTreeMap::new();
    result.insert(".".to_owned(), entry_state(root, true));
    walk(root, root, &mut result);
    result
}

#[cfg(unix)]
fn set_legacy_modes(root: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(root, fs::Permissions::from_mode(0o755)).unwrap();
    for (index, path) in fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .enumerate()
    {
        let mode = if path.is_dir() {
            0o751
        } else {
            0o640 + index as u32 % 8
        };
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        if path.is_dir() {
            for (child_index, child) in fs::read_dir(&path)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .enumerate()
            {
                fs::set_permissions(
                    child,
                    fs::Permissions::from_mode(0o640 + child_index as u32 % 8),
                )
                .unwrap();
            }
        }
    }
}

#[test]
fn ids_are_unique_lowercase_v4_uuids_and_index_keeps_urls_secret() {
    let root = TempDirectory::new();
    let store = ProfileStore::open(root.profiles(), None).unwrap();
    let first = store
        .create_profile(snapshot("https://one.invalid/private-a", "One"))
        .unwrap();
    let second = store
        .create_profile(snapshot("https://two.invalid/private-b", "Two"))
        .unwrap();

    assert_ne!(first, second);
    for id in [&first, &second] {
        let text = id.as_str();
        assert_eq!(text.len(), 36);
        assert_eq!(&text[14..15], "4");
        assert!(matches!(&text[19..20], "8" | "9" | "a" | "b"));
        assert_eq!(text, text.to_ascii_lowercase());
    }

    store.activate(&first).unwrap();
    assert_eq!(store.active_id(), Some(first.clone()));
    let index = fs::read_to_string(root.profiles().join("index.json")).unwrap();
    assert!(!index.contains("https://"));
    assert!(!index.contains("private-a"));
    assert!(!index.contains("private-b"));
    let summaries = store.list().unwrap();
    assert_eq!(summaries.len(), 2);
    assert_eq!(summaries.iter().filter(|profile| profile.active).count(), 1);
    assert_eq!(index.matches("active_id").count(), 1);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(root.profiles().join("index.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0);
    }
}

#[cfg(windows)]
#[test]
fn index_keeps_owner_only_protected_acl_across_atomic_replacements() {
    let root = TempDirectory::new();
    let profiles = root.profiles();
    let index = profiles.join("index.json");
    let store = ProfileStore::open(&profiles, None).unwrap();
    assert_protected_single_principal_dacl(&profiles.join(".index.lock"));

    let first = store
        .create_profile(snapshot("https://one.invalid/sub", "One"))
        .unwrap();
    assert_protected_single_principal_dacl(&index);

    store
        .create_profile(snapshot("https://two.invalid/sub", "Two"))
        .unwrap();
    assert_protected_single_principal_dacl(&index);

    store.activate(&first).unwrap();
    assert_protected_single_principal_dacl(&index);
}

#[test]
fn seventeenth_profile_is_rejected_without_changing_index() {
    let root = TempDirectory::new();
    let store = ProfileStore::open(root.profiles(), None).unwrap();
    for index in 0..MAX_PROFILES {
        store
            .create_profile(snapshot(
                &format!("https://{index}.invalid/sub"),
                &format!("Node{index}"),
            ))
            .unwrap();
    }
    let before = fs::read(root.profiles().join("index.json")).unwrap();
    assert!(matches!(
        store.create_profile(snapshot("https://overflow.invalid/sub", "Overflow")),
        Err(ProfileError::LimitReached)
    ));
    assert_eq!(
        fs::read(root.profiles().join("index.json")).unwrap(),
        before
    );
}

#[test]
fn second_store_instance_is_rejected_until_lifetime_lock_is_released() {
    let root = TempDirectory::new();
    let profiles = root.profiles();
    let first = ProfileStore::open(&profiles, None).unwrap();
    let started = std::time::Instant::now();
    assert!(matches!(
        ProfileStore::open(&profiles, None),
        Err(ProfileError::Locked)
    ));
    assert!(started.elapsed() < std::time::Duration::from_secs(2));

    drop(first);
    assert!(ProfileStore::open(&profiles, None).is_ok());
}

#[test]
fn oversized_index_is_rejected_before_deserialization() {
    let root = TempDirectory::new();
    let profiles = root.profiles();
    fs::create_dir(&profiles).unwrap();
    fs::write(
        profiles.join("index.json"),
        vec![b' '; MAX_PROFILE_INDEX_BYTES as usize + 1],
    )
    .unwrap();
    assert!(matches!(
        ProfileStore::open(profiles, None),
        Err(ProfileError::InvalidIndex(reason)) if reason.contains("size limit")
    ));
}

#[test]
fn an_added_profile_leaves_the_active_one_in_place() {
    let root = TempDirectory::new();
    let store = ProfileStore::open(root.profiles(), None).unwrap();
    let first = store
        .create_profile(snapshot("https://first.invalid/sub", "First"))
        .unwrap();
    let added = store
        .add_profile(snapshot("https://added.invalid/sub", "Added"))
        .unwrap();

    assert_ne!(added, first);
    assert_eq!(store.active_id(), Some(first.clone()));
    assert_eq!(
        store
            .find_exact_source("https://added.invalid/sub")
            .unwrap(),
        Some(added.clone())
    );
    drop(store);
    let reopened = ProfileStore::open(root.profiles(), None).unwrap();
    assert_eq!(reopened.active_id(), Some(first));
    assert!(reopened.contains(&added));
}

#[test]
fn create_replace_activate_delete_and_exact_lookup_stay_isolated() {
    let root = TempDirectory::new();
    let profiles = root.profiles();
    let store = ProfileStore::open(&profiles, None).unwrap();
    let first = store
        .create_profile(snapshot("https://same.invalid/sub?a=1", "First"))
        .unwrap();
    let second = store
        .create_profile(snapshot("https://same.invalid/sub?a=2", "Second"))
        .unwrap();

    assert_eq!(
        store
            .find_exact_source("https://same.invalid/sub?a=1")
            .unwrap(),
        Some(first.clone())
    );
    assert_eq!(
        store.find_exact_source("https://same.invalid/sub").unwrap(),
        None
    );
    assert_eq!(store.active_id(), Some(second.clone()));
    assert_eq!(
        store
            .active_snapshot()
            .unwrap()
            .unwrap()
            .2
            .mihomo
            .proxy_names(),
        &["Second"]
    );

    let generation = store
        .replace_profile(
            &first,
            snapshot("https://same.invalid/sub?a=1", "FirstUpdated"),
        )
        .unwrap();
    assert_ne!(generation, 0);
    assert_eq!(
        store
            .active_snapshot()
            .unwrap()
            .unwrap()
            .2
            .mihomo
            .proxy_names(),
        &["Second"]
    );
    assert_eq!(
        store.activate(&first).unwrap().mihomo.proxy_names(),
        &["FirstUpdated"]
    );

    assert_eq!(store.delete(&first).unwrap(), Some(second.clone()));
    assert_eq!(store.active_id(), Some(second.clone()));
    assert!(!profiles.join(first.as_str()).exists());
    assert!(profiles.join(second.as_str()).exists());
    assert_eq!(store.delete(&second).unwrap(), None);
    assert_eq!(store.active_id(), None);
}

#[test]
fn legacy_generation_migrates_offline_and_remains_as_rollback_material() {
    let root = TempDirectory::new();
    let legacy = root.path().join("snapshots");
    let expected = snapshot("https://legacy.invalid/secret", "Legacy");
    PersistentSnapshotStore::open(&legacy)
        .unwrap()
        .commit((*expected).clone())
        .unwrap();
    let logo = normalize_service_logo(
        br##"<svg xmlns="http://www.w3.org/2000/svg" width="2" height="2"><path fill="#123456" d="M0 0h2v2H0z"/></svg>"##,
    )
    .unwrap();
    fs::write(
        legacy
            .join("snapshot-00000000000000000001")
            .join("service-logo.png"),
        &logo,
    )
    .unwrap();
    #[cfg(unix)]
    set_legacy_modes(&legacy);
    let legacy_before = capture_tree(&legacy);

    let store = ProfileStore::open(root.profiles(), Some(legacy.clone())).unwrap();
    let (_, revision, migrated) = store.active_snapshot().unwrap().unwrap();
    assert_ne!(revision, 0);
    assert_eq!(
        migrated.subscription_source_url(),
        Some("https://legacy.invalid/secret")
    );
    assert_eq!(migrated.subscription_info(), expected.subscription_info());
    assert_eq!(migrated.mihomo.raw_yaml(), expected.mihomo.raw_yaml());
    assert_eq!(migrated.xray.raw_bytes(), expected.xray.raw_bytes());
    // The logo path comes from /proc/<pid>/fd; elsewhere on Unix the logo is not shown.
    if cfg!(any(target_os = "linux", target_os = "android", windows)) {
        assert_eq!(
            fs::read(migrated.service_logo_path().unwrap()).unwrap(),
            logo
        );
    }
    assert!(legacy.join("snapshot-00000000000000000001").is_dir());
    assert_eq!(capture_tree(&legacy), legacy_before);
}

#[test]
fn config_only_legacy_snapshot_is_migrated() {
    let root = TempDirectory::new();
    let legacy = root.path().join("snapshots");
    let config_only = Snapshot::parse(
        b"proxies: []\nproxy-groups: []\nrules: []\n",
        br#"{"outbounds":[]}"#,
    )
    .unwrap();
    PersistentSnapshotStore::open(&legacy)
        .unwrap()
        .commit(config_only)
        .unwrap();

    let store = ProfileStore::open(root.profiles(), Some(legacy)).unwrap();
    let (_, _, migrated) = store.active_snapshot().unwrap().unwrap();
    assert_eq!(migrated.subscription_source_url(), None);
    assert_eq!(
        migrated.mihomo.raw_yaml(),
        "proxies: []\nproxy-groups: []\nrules: []\n"
    );
    assert_eq!(migrated.xray.raw_bytes(), br#"{"outbounds":[]}"#);
}

#[test]
fn migration_is_idempotent_and_an_existing_index_wins() {
    let root = TempDirectory::new();
    let profiles = root.profiles();
    let first_legacy = root.path().join("legacy-one");
    PersistentSnapshotStore::open(&first_legacy)
        .unwrap()
        .commit((*snapshot("https://legacy.invalid/one", "One")).clone())
        .unwrap();
    let first = ProfileStore::open(&profiles, Some(first_legacy)).unwrap();
    let id = first.active_id().unwrap();
    drop(first);

    let second_legacy = root.path().join("legacy-two");
    PersistentSnapshotStore::open(&second_legacy)
        .unwrap()
        .commit((*snapshot("https://legacy.invalid/two", "Two")).clone())
        .unwrap();
    let reopened = ProfileStore::open(&profiles, Some(second_legacy)).unwrap();
    assert_eq!(reopened.active_id(), Some(id));
    assert_eq!(reopened.list().unwrap().len(), 1);
    assert_eq!(
        reopened
            .active_snapshot()
            .unwrap()
            .unwrap()
            .2
            .subscription_source_url(),
        Some("https://legacy.invalid/one")
    );
}

#[test]
fn invalid_existing_index_is_not_replaced_from_legacy() {
    let root = TempDirectory::new();
    let profiles = root.profiles();
    fs::create_dir(&profiles).unwrap();
    fs::write(profiles.join("index.json"), b"{broken").unwrap();
    let before = fs::read(profiles.join("index.json")).unwrap();
    let legacy = root.path().join("legacy");
    PersistentSnapshotStore::open(&legacy)
        .unwrap()
        .commit((*snapshot("https://legacy.invalid/sub", "Legacy")).clone())
        .unwrap();

    assert!(matches!(
        ProfileStore::open(&profiles, Some(legacy)),
        Err(ProfileError::InvalidIndex(_))
    ));
    assert_eq!(fs::read(profiles.join("index.json")).unwrap(), before);
    let expected_entries = if cfg!(windows) { 2 } else { 1 };
    assert_eq!(fs::read_dir(&profiles).unwrap().count(), expected_entries);
}

#[test]
fn inactive_snapshot_access_does_not_activate_and_revisions_are_profile_scoped() {
    let root = TempDirectory::new();
    let profiles = root.profiles();
    let store = ProfileStore::open(&profiles, None).unwrap();
    let first = store
        .create_profile(snapshot("https://one.invalid/sub", "One"))
        .unwrap();
    let second = store
        .create_profile(snapshot("https://two.invalid/sub", "Two"))
        .unwrap();
    let active = store.active_id();
    let (first_revision, first_snapshot) = store.profile_snapshot(&first).unwrap();
    let (second_revision, _) = store.profile_snapshot(&second).unwrap();
    assert_eq!(store.active_id(), active);
    assert_eq!(first_snapshot.mihomo.proxy_names(), &["One"]);
    assert_ne!(first_revision, 0);
    assert_ne!(first_revision, second_revision);
    drop(store);

    let reopened = ProfileStore::open(&profiles, None).unwrap();
    assert_eq!(reopened.profile_snapshot(&first).unwrap().0, first_revision);
}

#[test]
fn selections_are_isolated_filtered_and_reset_on_revision_change_or_corruption() {
    let root = TempDirectory::new();
    let profiles = root.profiles();
    let store = ProfileStore::open(&profiles, None).unwrap();
    let first = store
        .create_profile(snapshot("https://one.invalid/sub", "One"))
        .unwrap();
    let second = store
        .create_profile(snapshot("https://two.invalid/sub", "Two"))
        .unwrap();
    let revision = store.profile_snapshot(&first).unwrap().0;
    store
        .save_selection(&first, revision, "group-a", "node-b")
        .unwrap();
    store
        .save_selection(&first, revision, "group-c", "node-d")
        .unwrap();
    let catalog: &[(&str, &[&str])] = &[
        ("group-a", &["node-b", "node-c"]),
        ("group-c", &["missing"]),
    ];
    assert_eq!(
        store.valid_selections(&first, revision, catalog).unwrap()["group-a"],
        "node-b"
    );
    assert_eq!(
        store
            .valid_selections(&first, revision, catalog)
            .unwrap()
            .len(),
        1
    );
    assert!(
        store
            .valid_selections(&second, revision, catalog)
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .valid_selections(&first, revision.wrapping_add(1), catalog)
            .unwrap()
            .is_empty()
    );

    fs::write(
        profiles.join(first.as_str()).join("selections.json"),
        b"{broken",
    )
    .unwrap();
    assert!(
        store
            .valid_selections(&first, revision, catalog)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn selection_bounds_are_checked_before_existing_data_changes() {
    let root = TempDirectory::new();
    let profiles = root.profiles();
    let store = ProfileStore::open(&profiles, None).unwrap();
    let id = store
        .create_profile(snapshot("https://one.invalid/sub", "One"))
        .unwrap();
    let revision = store.profile_snapshot(&id).unwrap().0;
    for index in 0..MAX_SELECTIONS {
        store
            .save_selection(
                &id,
                revision,
                &format!("group-{index:02}"),
                &format!("node-{index:02}"),
            )
            .unwrap();
    }
    let path = profiles.join(id.as_str()).join("selections.json");
    let before = fs::read(&path).unwrap();
    assert!(matches!(
        store.save_selection(&id, revision, "overflow", "node"),
        Err(ProfileError::InvalidSelection(_))
    ));
    assert_eq!(fs::read(&path).unwrap(), before);
    for invalid in ["", "space id", "slash/id", "back\\slash", "\u{7f}"] {
        assert!(matches!(
            store.save_selection(&id, revision, invalid, "node"),
            Err(ProfileError::InvalidSelection(_))
        ));
        assert_eq!(fs::read(&path).unwrap(), before);
    }
}

#[test]
fn concurrent_selection_saves_preserve_every_group() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let root = TempDirectory::new();
    let store = Arc::new(ProfileStore::open(root.profiles(), None).unwrap());
    let id = store
        .create_profile(snapshot("https://one.invalid/sub", "One"))
        .unwrap();
    let revision = store.profile_snapshot(&id).unwrap().0;
    let stop_reader = Arc::new(AtomicBool::new(false));
    let reader = {
        let store = Arc::clone(&store);
        let id = id.clone();
        let stop_reader = Arc::clone(&stop_reader);
        std::thread::spawn(move || {
            while !stop_reader.load(Ordering::Acquire) {
                store.valid_selections(&id, revision, &[]).unwrap();
            }
        })
    };
    let mut threads = Vec::new();
    for index in 0..16 {
        let store = Arc::clone(&store);
        let id = id.clone();
        threads.push(std::thread::spawn(move || {
            store
                .save_selection(
                    &id,
                    revision,
                    &format!("group-{index:02}"),
                    &format!("node-{index:02}"),
                )
                .unwrap();
        }));
    }
    for thread in threads {
        thread.join().unwrap();
    }
    stop_reader.store(true, Ordering::Release);
    reader.join().unwrap();
    let groups = (0..16)
        .map(|index| format!("group-{index:02}"))
        .collect::<Vec<_>>();
    let nodes = (0..16)
        .map(|index| format!("node-{index:02}"))
        .collect::<Vec<_>>();
    let catalog_storage = groups
        .iter()
        .zip(&nodes)
        .map(|(group, node)| (group.as_str(), vec![node.as_str()]))
        .collect::<Vec<_>>();
    let catalog = catalog_storage
        .iter()
        .map(|(group, nodes)| (*group, nodes.as_slice()))
        .collect::<Vec<_>>();
    assert_eq!(
        store
            .valid_selections(&id, revision, &catalog)
            .unwrap()
            .len(),
        16
    );
}

#[cfg(unix)]
#[test]
fn unreadable_selection_file_is_not_treated_as_corrupt_or_overwritten() {
    use std::os::unix::fs::PermissionsExt;

    let root = TempDirectory::new();
    let profiles = root.profiles();
    let store = ProfileStore::open(&profiles, None).unwrap();
    let id = store
        .create_profile(snapshot("https://one.invalid/sub", "One"))
        .unwrap();
    let revision = store.profile_snapshot(&id).unwrap().0;
    store
        .save_selection(&id, revision, "existing", "node-existing")
        .unwrap();
    let path = profiles.join(id.as_str()).join("selections.json");
    let before = fs::read(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();

    assert!(matches!(
        store.save_selection(&id, revision, "new", "node-new"),
        Err(ProfileError::Io(_))
    ));
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(
        store
            .valid_selections(&id, revision, &[("existing", &["node-existing"])])
            .unwrap()["existing"],
        "node-existing"
    );
}

#[cfg(windows)]
#[test]
fn sharing_blocked_selection_file_is_not_treated_as_corrupt_or_overwritten() {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Foundation::GENERIC_READ;

    let root = TempDirectory::new();
    let profiles = root.profiles();
    let store = ProfileStore::open(&profiles, None).unwrap();
    let id = store
        .create_profile(snapshot("https://one.invalid/sub", "One"))
        .unwrap();
    let revision = store.profile_snapshot(&id).unwrap().0;
    store
        .save_selection(&id, revision, "existing", "node-existing")
        .unwrap();
    let path = profiles.join(id.as_str()).join("selections.json");
    let before = fs::read(&path).unwrap();
    let blocker = fs::OpenOptions::new()
        .read(true)
        .access_mode(GENERIC_READ)
        .share_mode(0)
        .open(&path)
        .unwrap();

    assert!(matches!(
        store.save_selection(&id, revision, "new", "node-new"),
        Err(ProfileError::Io(_))
    ));
    drop(blocker);
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(
        store
            .valid_selections(&id, revision, &[("existing", &["node-existing"])])
            .unwrap()["existing"],
        "node-existing"
    );
}

#[test]
fn corrupt_child_is_unavailable_and_failed_activation_keeps_index_active() {
    let root = TempDirectory::new();
    let profiles = root.profiles();
    let store = ProfileStore::open(&profiles, None).unwrap();
    let good = store
        .create_profile(snapshot("https://good.invalid/sub", "Good"))
        .unwrap();
    let corrupt = store
        .create_profile(snapshot("https://bad.invalid/sub", "Bad"))
        .unwrap();
    store.activate(&good).unwrap();
    drop(store);

    let generation = profiles
        .join(corrupt.as_str())
        .join("snapshot-00000000000000000001");
    fs::write(generation.join("xray.json"), b"{broken").unwrap();

    let reopened = ProfileStore::open(&profiles, None).unwrap();
    let summaries = reopened.list().unwrap();
    assert!(
        summaries
            .iter()
            .any(|item| item.id == good && item.available)
    );
    assert!(
        summaries
            .iter()
            .any(|item| item.id == corrupt && !item.available)
    );
    let before = fs::read(profiles.join("index.json")).unwrap();
    assert!(matches!(
        reopened.activate(&corrupt),
        Err(ProfileError::Unavailable(_))
    ));
    assert_eq!(reopened.active_id(), Some(good));
    assert_eq!(fs::read(profiles.join("index.json")).unwrap(), before);
}

#[test]
fn failed_replace_preserves_last_good_snapshot_and_active_index() {
    let root = TempDirectory::new();
    let profiles = root.profiles();
    let store = ProfileStore::open(&profiles, None).unwrap();
    let id = store
        .create_profile(snapshot("https://stable.invalid/sub", "Stable"))
        .unwrap();
    let before = fs::read(profiles.join("index.json")).unwrap();
    let blocked_staging = profiles.join(id.as_str()).join(format!(
        ".snapshot-00000000000000000002.staging-{}",
        std::process::id()
    ));
    fs::create_dir(&blocked_staging).unwrap();

    assert!(
        store
            .replace_profile(&id, snapshot("https://stable.invalid/sub", "Rejected"))
            .is_err()
    );
    assert_eq!(fs::read(profiles.join("index.json")).unwrap(), before);
    assert_eq!(
        store
            .active_snapshot()
            .unwrap()
            .unwrap()
            .2
            .mihomo
            .proxy_names(),
        &["Stable"]
    );
    drop(store);

    let reopened = ProfileStore::open(&profiles, None).unwrap();
    assert_eq!(
        reopened
            .active_snapshot()
            .unwrap()
            .unwrap()
            .2
            .mihomo
            .proxy_names(),
        &["Stable"]
    );
}

#[test]
fn invalid_deserialized_id_is_rejected_before_any_child_path_is_used() {
    let root = TempDirectory::new();
    let profiles = root.profiles();
    fs::create_dir(&profiles).unwrap();
    fs::write(
        profiles.join("index.json"),
        br#"{"schema_version":1,"profiles":[{"id":"../escape","created_at_unix":1}],"active_id":"../escape"}"#,
    )
    .unwrap();

    assert!(matches!(
        ProfileStore::open(profiles, None),
        Err(ProfileError::InvalidIndex(_))
    ));
}

#[test]
fn malformed_empty_and_active_states_are_rejected_in_both_directions() {
    const ID: &str = "8a7f10a1-6ea8-4f65-8f15-711a184019d8";
    let cases = [
        format!(r#"{{"schema_version":1,"profiles":[],"active_id":"{ID}"}}"#),
        format!(
            r#"{{"schema_version":1,"profiles":[{{"id":"{ID}","created_at_unix":1}}],"active_id":null}}"#
        ),
    ];
    for (case, contents) in cases.into_iter().enumerate() {
        let root = TempDirectory::new();
        let profiles = root.path().join(format!("profiles-{case}"));
        fs::create_dir(&profiles).unwrap();
        fs::write(profiles.join("index.json"), contents).unwrap();
        assert!(matches!(
            ProfileStore::open(profiles, None),
            Err(ProfileError::InvalidIndex(_))
        ));
    }
}

#[cfg(unix)]
#[test]
fn retained_profile_root_capability_ignores_path_replacement() {
    let root = TempDirectory::new();
    let profiles = root.profiles();
    let original = root.path().join("original-profiles");
    let store = ProfileStore::open(&profiles, None).unwrap();
    store
        .create_profile(snapshot("https://one.invalid/sub", "One"))
        .unwrap();

    fs::rename(&profiles, &original).unwrap();
    fs::create_dir(&profiles).unwrap();
    fs::write(profiles.join("sentinel"), b"replacement").unwrap();
    store
        .create_profile(snapshot("https://two.invalid/sub", "Two"))
        .unwrap();

    assert_eq!(store.list().unwrap().len(), 2);
    assert_eq!(fs::read(profiles.join("sentinel")).unwrap(), b"replacement");
    assert!(!profiles.join("index.json").exists());
    assert!(original.join("index.json").is_file());
}

#[cfg(unix)]
#[test]
fn index_symlink_replacement_is_replaced_without_following_it() {
    use std::os::unix::fs::symlink;

    let root = TempDirectory::new();
    let profiles = root.profiles();
    let external = root.path().join("external-index");
    let store = ProfileStore::open(&profiles, None).unwrap();
    store
        .create_profile(snapshot("https://one.invalid/sub", "One"))
        .unwrap();

    fs::write(&external, b"external sentinel").unwrap();
    fs::remove_file(profiles.join("index.json")).unwrap();
    symlink(&external, profiles.join("index.json")).unwrap();
    store
        .create_profile(snapshot("https://two.invalid/sub", "Two"))
        .unwrap();

    assert_eq!(fs::read(&external).unwrap(), b"external sentinel");
    assert!(
        !fs::symlink_metadata(profiles.join("index.json"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(store.list().unwrap().len(), 2);
}

#[cfg(unix)]
#[test]
fn delete_rejects_symlink_child_without_touching_external_target_or_resurrecting() {
    use std::os::unix::fs::symlink;

    hostile_cleanup_target_is_not_followed(|target, link| symlink(target, link).unwrap());
}

#[cfg(windows)]
#[test]
fn delete_rejects_junction_child_without_touching_external_target_or_resurrecting() {
    use std::process::Command;

    hostile_cleanup_target_is_not_followed(|target, link| {
        let output = Command::new("cmd")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "junction creation failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    });
}

#[cfg(any(unix, windows))]
fn hostile_cleanup_target_is_not_followed(create_link: impl FnOnce(&PathBuf, &PathBuf)) {
    let root = TempDirectory::new();
    let profiles = root.profiles();
    let external = root.path().join("external");
    fs::create_dir(&external).unwrap();
    fs::write(external.join("sentinel.txt"), b"do not remove").unwrap();

    let store = ProfileStore::open(&profiles, None).unwrap();
    let id = store
        .create_profile(snapshot("https://hostile.invalid/sub", "Hostile"))
        .unwrap();
    let child = profiles.join(id.as_str());
    let retired = root.path().join("retired-real-profile");
    fs::rename(&child, &retired).unwrap();
    create_link(&external, &child);

    let cleanup = store.delete(&id).unwrap_err();
    assert!(matches!(&cleanup, ProfileError::CleanupDebt { .. }));
    assert!(cleanup.mutation_committed());
    assert_eq!(
        fs::read(external.join("sentinel.txt")).unwrap(),
        b"do not remove"
    );
    assert!(child.exists());
    drop(store);

    let reopened = ProfileStore::open(&profiles, None).unwrap();
    assert!(reopened.list().unwrap().is_empty());
    assert_eq!(reopened.active_id(), None);
    assert!(!child.exists(), "startup must remove the UUID link itself");
    assert_eq!(
        fs::read(external.join("sentinel.txt")).unwrap(),
        b"do not remove"
    );
}

#[cfg(windows)]
fn assert_protected_single_principal_dacl(path: &Path) {
    use std::{ffi::c_void, os::windows::ffi::OsStrExt, ptr};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_SUCCESS, LocalFree},
        Security::{
            ACCESS_ALLOWED_ACE, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
            Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT},
            CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation,
            GetSecurityDescriptorControl, GetTokenInformation, OBJECT_INHERIT_ACE,
            PSECURITY_DESCRIPTOR, SE_DACL_PROTECTED, TOKEN_QUERY, TOKEN_USER, TokenUser,
        },
        Storage::FileSystem::FILE_ALL_ACCESS,
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut dacl: *mut ACL = ptr::null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    let status = unsafe {
        GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut dacl,
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    assert_eq!(
        status,
        ERROR_SUCCESS,
        "failed to inspect {}",
        path.display()
    );
    assert!(!dacl.is_null());

    let mut control = 0_u16;
    let mut revision = 0_u32;
    assert_ne!(
        unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) },
        0
    );
    assert_ne!(control & SE_DACL_PROTECTED, 0, "{}", path.display());

    let mut size = ACL_SIZE_INFORMATION::default();
    assert_ne!(
        unsafe {
            GetAclInformation(
                dacl,
                (&mut size as *mut ACL_SIZE_INFORMATION).cast::<c_void>(),
                std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )
        },
        0
    );
    assert_eq!(
        size.AceCount,
        1,
        "unexpected ACL entries for {}",
        path.display()
    );

    let mut raw_ace = ptr::null_mut();
    assert_ne!(unsafe { GetAce(dacl, 0, &mut raw_ace) }, 0);
    let ace = unsafe { &*raw_ace.cast::<ACCESS_ALLOWED_ACE>() };
    assert_eq!(ace.Header.AceType, 0, "ACE must be ACCESS_ALLOWED");
    assert_eq!(ace.Mask, FILE_ALL_ACCESS);
    assert_eq!(
        ace.Header.AceFlags & (CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE) as u8,
        0,
        "index file ACE must not be inheritable"
    );

    let mut token = ptr::null_mut();
    assert_ne!(
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) },
        0
    );
    let mut bytes_needed = 0_u32;
    unsafe {
        GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut bytes_needed);
    }
    assert_ne!(bytes_needed, 0);
    let word_size = std::mem::size_of::<usize>();
    let mut token_info = vec![0_usize; (bytes_needed as usize).div_ceil(word_size)];
    assert_ne!(
        unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                token_info.as_mut_ptr().cast(),
                bytes_needed,
                &mut bytes_needed,
            )
        },
        0
    );
    let token_user = unsafe { &*token_info.as_ptr().cast::<TOKEN_USER>() };
    let ace_sid = (&ace.SidStart as *const u32).cast_mut().cast::<c_void>();
    assert_ne!(unsafe { EqualSid(ace_sid, token_user.User.Sid) }, 0);

    unsafe { CloseHandle(token) };
    unsafe { LocalFree(descriptor.cast()) };
}

#[test]
fn the_group_icon_cache_survives_reopening_and_drops_what_it_cannot_trust() {
    use multicore_core::{CachedGroupIcon, MAX_CACHED_GROUP_ICONS, group_icon_key};
    let directory = TempDirectory::new();
    let png = multicore_core::normalize_group_icon(
        br#"<svg xmlns='http://www.w3.org/2000/svg' width='16' height='16'><rect width='16' height='16' fill='#0f0'/></svg>"#,
    )
    .unwrap();
    {
        let store = ProfileStore::open(directory.profiles(), None).unwrap();
        assert!(store.group_icon_cache().is_empty());
        let mut icons = BTreeMap::new();
        for index in 0..MAX_CACHED_GROUP_ICONS + 3 {
            icons.insert(
                group_icon_key(&format!("https://icons.example/{index}.png")),
                CachedGroupIcon {
                    png: png.clone(),
                    fetched_at_unix: index as u64,
                },
            );
        }
        // Not a canonical icon, and not a key: never written.
        icons.insert(
            group_icon_key("https://icons.example/raw.png"),
            CachedGroupIcon {
                png: b"not a png".to_vec(),
                fetched_at_unix: u64::MAX,
            },
        );
        icons.insert(
            "https://icons.example/plain".into(),
            CachedGroupIcon {
                png: png.clone(),
                fetched_at_unix: u64::MAX,
            },
        );
        store.save_group_icon_cache(&icons).unwrap();
    }
    let store = ProfileStore::open(directory.profiles(), None).unwrap();
    let cached = store.group_icon_cache();
    assert_eq!(cached.len(), MAX_CACHED_GROUP_ICONS);
    // The newest downloads are kept.
    assert!(cached.contains_key(&group_icon_key(&format!(
        "https://icons.example/{}.png",
        MAX_CACHED_GROUP_ICONS + 2
    ))));
    assert!(!cached.contains_key(&group_icon_key("https://icons.example/0.png")));
    assert!(cached.values().all(|icon| icon.png == png));
    // The cache file is not a profile and survives the orphan cleanup on open.
    assert_eq!(store.list().unwrap().len(), 0);

    // A damaged cache reads as empty.
    fs::write(directory.profiles().join("group-icons.json"), b"{not json").unwrap();
    drop(store);
    let store = ProfileStore::open(directory.profiles(), None).unwrap();
    assert!(store.group_icon_cache().is_empty());
}

#[test]
#[cfg_attr(
    not(any(target_os = "linux", target_os = "android", windows)),
    ignore = "the logo path comes from /proc/<pid>/fd, which this platform lacks"
)]
fn a_background_logo_replaces_the_shown_one_under_a_new_path_and_goes_with_its_profile() {
    use multicore_core::CachedServiceLogo;
    let directory = TempDirectory::new();
    let store = ProfileStore::open(directory.profiles(), None).unwrap();
    let id = store
        .create_profile(snapshot("https://one.invalid/sub", "One"))
        .unwrap();
    assert!(store.cached_service_logo(&id).is_none());
    let logo = |color: &str| {
        normalize_service_logo(
            format!("<svg xmlns='http://www.w3.org/2000/svg' width='16' height='16'><rect width='16' height='16' fill='{color}'/></svg>").as_bytes(),
        )
        .unwrap()
    };
    // The logo files in the store (paths handed out are lease paths: /proc/<pid>/fd/<n> on
    // Linux, so files are checked by name here).
    let logo_files = || {
        let mut names: Vec<String> = fs::read_dir(directory.profiles())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .filter(|name| name.starts_with("logo-") && name.ends_with(".png"))
            .collect();
        names.sort();
        names
    };
    let shown = |logo: Option<CachedServiceLogo>| match logo {
        Some(CachedServiceLogo::Logo(lease)) => Some(fs::read(lease.path().unwrap()).unwrap()),
        _ => None,
    };
    let first = store.save_service_logo(&id, &logo("#00ff00")).unwrap();
    assert_eq!(fs::read(first.path().unwrap()).unwrap(), logo("#00ff00"));
    assert_eq!(shown(store.cached_service_logo(&id)), Some(logo("#00ff00")));
    let first_files = logo_files();
    assert_eq!(first_files.len(), 1);
    assert!(store.save_service_logo(&id, b"not a logo").is_err());
    // A new image gets a new file, while the lease still handed out keeps reading what it
    // promised (on Windows that lease keeps the old file until it is released).
    let second = store.save_service_logo(&id, &logo("#ff0000")).unwrap();
    assert_eq!(fs::read(second.path().unwrap()).unwrap(), logo("#ff0000"));
    assert_eq!(fs::read(first.path().unwrap()).unwrap(), logo("#00ff00"));
    assert_eq!(shown(store.cached_service_logo(&id)), Some(logo("#ff0000")));
    drop(first);
    // The same image keeps its file; the old file is gone once nothing holds it.
    let again = store.save_service_logo(&id, &logo("#ff0000")).unwrap();
    let second_files = logo_files();
    assert_eq!(second_files.len(), 1);
    assert_ne!(second_files, first_files);
    // An image seen before becomes the newest again, not the oldest.
    let green_again = store.save_service_logo(&id, &logo("#00ff00")).unwrap();
    drop((second, again, green_again));
    assert_eq!(shown(store.cached_service_logo(&id)), Some(logo("#00ff00")));
    assert_eq!(logo_files().len(), 1);
    let red = store.save_service_logo(&id, &logo("#ff0000")).unwrap();
    drop(red);
    // Reopening keeps it; the cache files are not profiles.
    drop(store);
    let store = ProfileStore::open(directory.profiles(), None).unwrap();
    assert_eq!(store.list().unwrap().len(), 1);
    assert_eq!(shown(store.cached_service_logo(&id)), Some(logo("#ff0000")));
    store.mark_service_logo_removed(&id).unwrap();
    assert!(matches!(
        store.cached_service_logo(&id),
        Some(CachedServiceLogo::Removed)
    ));
    assert!(logo_files().is_empty());
    let third = store.save_service_logo(&id, &logo("#0000ff")).unwrap();
    assert_eq!(shown(store.cached_service_logo(&id)), Some(logo("#0000ff")));
    drop(third);
    store.delete(&id).unwrap();
    assert!(logo_files().is_empty());
    assert!(store.cached_service_logo(&id).is_none());
}
