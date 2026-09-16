use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use multicore_core::{PersistentSnapshotStore, Snapshot, publish_runtime};

static TEMP_ID: AtomicU64 = AtomicU64::new(0);

const XRAY_ONE: &[u8] = b"{\n  \"revision\": 1\n}\n";
const XRAY_TWO: &[u8] = b"{\"revision\":2}";

fn mihomo(name: &str, port: u16) -> Vec<u8> {
    format!(
        "proxies:\n  - name: {name}\n    type: socks5\n    server: 127.0.0.1\n    port: {port}\nproxy-groups:\n  - name: Proxy\n    type: select\n    proxies: [{name}]\nrules:\n  - MATCH,Proxy\n"
    )
    .into_bytes()
}

struct TempDirectory(PathBuf);

impl TempDirectory {
    fn new() -> Self {
        let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "multicore-core-snapshot-test-{}-{id}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn disk_commit_round_trips_exact_payloads_across_startup() {
    let root = TempDirectory::new();
    let store = PersistentSnapshotStore::open(root.path()).unwrap();
    assert_eq!(store.current_generation(), None);
    store
        .commit(Snapshot::parse(&mihomo("One", 31001), XRAY_ONE).unwrap())
        .unwrap();
    assert_eq!(store.current_generation(), Some(1));
    drop(store);

    let reopened = PersistentSnapshotStore::open(root.path()).unwrap();
    assert_eq!(reopened.current_generation(), Some(1));
    let current = reopened.current().unwrap();
    assert_eq!(current.mihomo.raw_yaml().as_bytes(), mihomo("One", 31001));
    assert_eq!(current.xray.raw_bytes(), XRAY_ONE);

    let generations = fs::read_dir(root.path())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("snapshot-"))
        .collect::<Vec<_>>();
    assert_eq!(generations.len(), 1);
    assert_eq!(
        fs::read(generations[0].path().join("xray.json")).unwrap(),
        XRAY_ONE
    );
}

#[test]
fn subscription_source_and_safe_usage_round_trip_inside_the_protected_generation() {
    let root = TempDirectory::new();
    let store = PersistentSnapshotStore::open(root.path()).unwrap();
    let snapshot = Snapshot::parse(&mihomo("Usage", 31001), XRAY_ONE)
        .unwrap()
        .with_subscription_source(
            "https://user:secret@example.invalid/sub?token=private",
            Some("upload=0; download=938375741110; total=0; expire=1792851157"),
            1_757_959_200,
        )
        .unwrap();
    assert!(!format!("{snapshot:?}").contains("private"));
    store.commit(snapshot).unwrap();
    drop(store);

    let reopened = PersistentSnapshotStore::open(root.path()).unwrap();
    let current = reopened.current().unwrap();
    assert_eq!(
        current.subscription_source_url(),
        Some("https://user:secret@example.invalid/sub?token=private")
    );
    let info = current.subscription_info().unwrap();
    assert_eq!(info.source_host, "example.invalid");
    assert_eq!(info.downloaded_bytes, Some(938_375_741_110));
    assert_eq!(info.total_bytes, None);
    assert_eq!(info.expires_at_unix, Some(1_792_851_157));
    assert_eq!(info.updated_at_unix, 1_757_959_200);

    let generation = root.path().join("snapshot-00000000000000000001");
    assert!(generation.join("subscription.json").is_file());
}

#[test]
fn legacy_generation_without_subscription_record_remains_loadable_without_refresh_source() {
    let root = TempDirectory::new();
    let store = PersistentSnapshotStore::open(root.path()).unwrap();
    store
        .commit(Snapshot::parse(&mihomo("Legacy", 31001), XRAY_ONE).unwrap())
        .unwrap();
    drop(store);

    let reopened = PersistentSnapshotStore::open(root.path()).unwrap();
    let current = reopened.current().unwrap();
    assert_eq!(current.subscription_source_url(), None);
    assert_eq!(current.subscription_info(), None);
}

#[test]
fn startup_ignores_abandoned_staging_and_newer_corrupt_generation() {
    let root = TempDirectory::new();
    let store = PersistentSnapshotStore::open(root.path()).unwrap();
    store
        .commit(Snapshot::parse(&mihomo("Good", 31001), XRAY_ONE).unwrap())
        .unwrap();
    drop(store);

    let staging = root.path().join(".snapshot-00000000000000000002.staging");
    fs::create_dir(&staging).unwrap();
    fs::write(staging.join("mihomo.yaml"), mihomo("Partial", 31002)).unwrap();

    let corrupt = root.path().join("snapshot-00000000000000000999");
    fs::create_dir(&corrupt).unwrap();
    fs::write(corrupt.join("mihomo.yaml"), mihomo("Corrupt", 31003)).unwrap();
    fs::write(corrupt.join("xray.json"), b"{broken").unwrap();

    let reopened = PersistentSnapshotStore::open(root.path()).unwrap();
    let current = reopened.current().unwrap();
    assert_eq!(reopened.current_generation(), Some(1));
    assert_eq!(current.mihomo.proxy_names(), &["Good"]);
    assert_eq!(current.xray.raw_bytes(), XRAY_ONE);

    reopened
        .commit(Snapshot::parse(&mihomo("New", 31004), XRAY_TWO).unwrap())
        .unwrap();
    assert_eq!(reopened.current_generation(), Some(1000));
    drop(reopened);
    let final_store = PersistentSnapshotStore::open(root.path()).unwrap();
    assert_eq!(final_store.current_generation(), Some(1000));
    let current = final_store.current().unwrap();
    assert_eq!(current.mihomo.proxy_names(), &["New"]);
    assert_eq!(current.xray.raw_bytes(), XRAY_TWO);
}

#[test]
fn failed_staging_write_keeps_last_good_in_memory_and_on_restart() {
    let root = TempDirectory::new();
    let store = PersistentSnapshotStore::open(root.path()).unwrap();
    store
        .commit(Snapshot::parse(&mihomo("Good", 31001), XRAY_ONE).unwrap())
        .unwrap();

    let blocked_staging = root.path().join(format!(
        ".snapshot-00000000000000000002.staging-{}",
        std::process::id()
    ));
    fs::create_dir(&blocked_staging).unwrap();
    assert!(
        store
            .commit(Snapshot::parse(&mihomo("Rejected", 31002), XRAY_TWO).unwrap())
            .is_err()
    );
    assert_eq!(store.current_generation(), Some(1));
    assert_eq!(store.current().unwrap().mihomo.proxy_names(), &["Good"]);
    drop(store);

    let reopened = PersistentSnapshotStore::open(root.path()).unwrap();
    assert_eq!(reopened.current_generation(), Some(1));
    assert_eq!(reopened.current().unwrap().mihomo.proxy_names(), &["Good"]);
    assert_eq!(reopened.current().unwrap().xray.raw_bytes(), XRAY_ONE);
}

#[test]
fn runtime_publication_is_byte_exact_and_exposes_only_complete_pairs() {
    let root = TempDirectory::new();
    let abandoned = root
        .path()
        .join(".runtime-00000000000000000001.staging-abandoned");
    fs::create_dir(&abandoned).unwrap();
    fs::write(abandoned.join("mihomo.yaml"), mihomo("Half", 31999)).unwrap();

    let snapshot = Snapshot::parse(&mihomo("Runtime", 31005), XRAY_ONE).unwrap();
    let published = publish_runtime(&snapshot, root.path()).unwrap();
    assert_eq!(
        fs::read(&published.mihomo_config).unwrap(),
        mihomo("Runtime", 31005)
    );
    assert_eq!(fs::read(&published.xray_config).unwrap(), XRAY_ONE);

    for entry in fs::read_dir(root.path()).unwrap().filter_map(Result::ok) {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with("runtime-") {
            assert!(entry.path().join("mihomo.yaml").is_file());
            assert!(entry.path().join("xray.json").is_file());
        }
    }
}
