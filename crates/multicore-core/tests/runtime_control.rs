use std::{
    collections::BTreeMap,
    fs,
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
};

use multicore_core::{
    MihomoRuntimeControl, PersistentSnapshotStore, Snapshot, default_ipv4_interface,
    publish_runtime_with_mihomo_control, stage_runtime_with_mihomo_control,
    xray_outbound_server_domains,
};
use serde_json::Value as JsonValue;
use serde_yaml_ng::Value;

static TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[test]
fn runtime_overlay_owns_a_unique_tun_device() {
    let mihomo = br#"
proxies: []
tun:
  enable: true
  device: FlClashX
  stack: system
  auto-route: true
  strict-route: true
  dns-hijack: [any:53]
"#;
    let snapshot = Snapshot::parse(mihomo, br#"{}"#).unwrap();
    let directory = TempDirectory::new();
    let control =
        MihomoRuntimeControl::new("127.0.0.1:19090".parse().unwrap(), "ephemeral").unwrap();

    let published =
        publish_runtime_with_mihomo_control(&snapshot, directory.path(), &control).unwrap();
    let runtime: Value =
        serde_yaml_ng::from_slice(&fs::read(published.mihomo_config).unwrap()).unwrap();
    let tun = runtime
        .as_mapping()
        .unwrap()
        .get(Value::String("tun".to_owned()))
        .and_then(Value::as_mapping)
        .unwrap();

    assert_eq!(
        tun.get(Value::String("device".to_owned()))
            .and_then(Value::as_str),
        Some("MultiCore")
    );
    assert_eq!(
        tun.get(Value::String("stack".to_owned()))
            .and_then(Value::as_str),
        Some("system")
    );
    assert_eq!(
        tun.get(Value::String("auto-route".to_owned()))
            .and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(snapshot.mihomo.raw_yaml().as_bytes(), mihomo);
}

#[test]
fn runtime_xray_overlay_binds_every_outbound_without_mutating_snapshot() {
    let xray = br#"{
  "inbounds": [],
  "outbounds": [
    {
      "protocol": "shadowsocks",
      "settings": { "address": "edge.example" },
      "streamSettings": {
        "finalmask": { "enabled": true },
        "sockopt": { "tcpFastOpen": true, "interface": "OldTunnel" }
      }
    },
    {
      "protocol": "shadowsocks",
      "settings": { "servers": [{ "address": "backup.example" }] },
      "streamSettings": { "network": "tcp" }
    }
  ]
}"#;
    let snapshot = Snapshot::parse(b"proxies: []\n", xray).unwrap();
    let directory = TempDirectory::new();
    let control = MihomoRuntimeControl::new("127.0.0.1:19090".parse().unwrap(), "ephemeral")
        .unwrap()
        .with_xray_outbound_interface("Ethernet")
        .unwrap()
        .with_xray_resolved_hosts(BTreeMap::from([
            (
                "edge.example".to_owned(),
                vec!["203.0.113.7".parse::<IpAddr>().unwrap()],
            ),
            (
                "backup.example".to_owned(),
                vec![
                    "198.51.100.9".parse::<IpAddr>().unwrap(),
                    "2001:db8::9".parse::<IpAddr>().unwrap(),
                ],
            ),
        ]))
        .unwrap();

    let published =
        publish_runtime_with_mihomo_control(&snapshot, directory.path(), &control).unwrap();
    let runtime: JsonValue =
        serde_json::from_slice(&fs::read(published.xray_config).unwrap()).unwrap();
    let outbounds = runtime["outbounds"].as_array().unwrap();

    for outbound in outbounds {
        assert_eq!(
            outbound["streamSettings"]["sockopt"]["interface"],
            "Ethernet"
        );
        assert_eq!(
            outbound["streamSettings"]["sockopt"]["domainStrategy"],
            "ForceIP"
        );
    }
    assert_eq!(outbounds[0]["streamSettings"]["finalmask"]["enabled"], true);
    assert_eq!(
        outbounds[0]["streamSettings"]["sockopt"]["tcpFastOpen"],
        true
    );
    assert_eq!(outbounds[1]["streamSettings"]["network"], "tcp");
    assert_eq!(runtime["dns"]["hosts"]["edge.example"], "203.0.113.7");
    assert_eq!(
        runtime["dns"]["hosts"]["backup.example"],
        serde_json::json!(["198.51.100.9", "2001:db8::9"])
    );
    assert_eq!(snapshot.xray.raw_bytes(), xray);
}

#[test]
fn xray_server_domain_extraction_supports_finalmask_and_standard_shadowsocks_shapes() {
    let snapshot = Snapshot::parse(
        b"proxies: []\n",
        br#"{
          "outbounds": [
            {"settings": {"address": "edge.example"}},
            {"settings": {"servers": [
              {"address": "backup.example"},
              {"address": "203.0.113.10"},
              {"address": "edge.example"}
            ]}}
          ]
        }"#,
    )
    .unwrap();

    assert_eq!(
        xray_outbound_server_domains(&snapshot).unwrap(),
        vec!["backup.example".to_owned(), "edge.example".to_owned()]
    );
}

#[test]
fn runtime_xray_overlay_treats_null_optional_sections_as_absent() {
    let xray = br#"{
      "dns": null,
      "outbounds": [
        {"settings": {"address": "edge.example"}, "streamSettings": null},
        {"streamSettings": {"sockopt": null}}
      ]
    }"#;
    let snapshot = Snapshot::parse(b"proxies: []\n", xray).unwrap();
    let directory = TempDirectory::new();
    let control = MihomoRuntimeControl::new("127.0.0.1:19090".parse().unwrap(), "ephemeral")
        .unwrap()
        .with_xray_outbound_interface("Wi-Fi")
        .unwrap()
        .with_xray_resolved_hosts(BTreeMap::from([(
            "edge.example".to_owned(),
            vec!["203.0.113.7".parse::<IpAddr>().unwrap()],
        )]))
        .unwrap();

    let published =
        publish_runtime_with_mihomo_control(&snapshot, directory.path(), &control).unwrap();
    let runtime: JsonValue =
        serde_json::from_slice(&fs::read(published.xray_config).unwrap()).unwrap();

    assert_eq!(runtime["dns"]["hosts"]["edge.example"], "203.0.113.7");
    assert_eq!(
        runtime["outbounds"][0]["streamSettings"]["sockopt"]["interface"],
        "Wi-Fi"
    );
    assert_eq!(
        runtime["outbounds"][1]["streamSettings"]["sockopt"]["interface"],
        "Wi-Fi"
    );
}

#[test]
fn runtime_xray_overlay_rejects_an_empty_outbound_set() {
    let xray = br#"{"outbounds": []}"#;
    let snapshot = Snapshot::parse(b"proxies: []\n", xray).unwrap();
    let directory = TempDirectory::new();
    let control = MihomoRuntimeControl::new("127.0.0.1:19090".parse().unwrap(), "ephemeral")
        .unwrap()
        .with_xray_outbound_interface("Ethernet")
        .unwrap();

    assert!(publish_runtime_with_mihomo_control(&snapshot, directory.path(), &control).is_err());
    assert_eq!(snapshot.xray.raw_bytes(), xray);
}

#[cfg(windows)]
#[test]
#[ignore = "requires explicit local Xray binary and subscription config paths"]
fn pinned_xray_accepts_the_bootstrap_dns_runtime_overlay() {
    let binary = std::env::var_os("MULTICORE_TEST_XRAY_BIN").unwrap();
    let xray_path = std::env::var_os("MULTICORE_TEST_XRAY_CONFIG").unwrap();
    let xray = fs::read(xray_path).unwrap();
    let snapshot = Snapshot::parse(b"proxies: []\n", &xray).unwrap();
    let mut hosts = BTreeMap::new();
    for domain in xray_outbound_server_domains(&snapshot).unwrap() {
        let mut addresses: Vec<_> = (domain.as_str(), 0)
            .to_socket_addrs()
            .unwrap()
            .map(|address| address.ip())
            .collect();
        addresses.sort_unstable();
        addresses.dedup();
        assert!(!addresses.is_empty());
        hosts.insert(domain, addresses);
    }
    let control = MihomoRuntimeControl::new("127.0.0.1:19090".parse().unwrap(), "ephemeral")
        .unwrap()
        .with_xray_outbound_interface(default_ipv4_interface().unwrap())
        .unwrap()
        .with_xray_resolved_hosts(hosts)
        .unwrap();
    let directory = TempDirectory::new();
    let published =
        publish_runtime_with_mihomo_control(&snapshot, directory.path(), &control).unwrap();

    let status = Command::new(binary)
        .args(["run", "-test", "-c"])
        .arg(published.xray_config)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
}

struct TempDirectory(PathBuf);

impl TempDirectory {
    fn new() -> Self {
        let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "multicore-runtime-control-test-{}-{id}",
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
fn runtime_publish_overrides_only_mihomo_controller_settings() {
    let mihomo = br#"
proxies: []
external-controller: 0.0.0.0:9090
external-controller-unix: /tmp/unsafe.sock
external-controller-pipe: unsafe-pipe
external-controller-tls: 0.0.0.0:9443
external-controller-cors: {allow-private-network: true}
external-controller-routing-mark: 1234
allow-lan: true
secret: old-secret
profile:
  store-selected: true
  store-fake-ip: true
external-ui: ./ui
external-ui-name: old-ui
external-ui-url: https://example.invalid/ui.zip
external-ui-custom-future-key: unsafe
external-doh-server: https://example.invalid/dns-query
global-client-fingerprint: chrome
cors:
  allow-origins: ['*']
"#;
    let xray = br#"{ "marker": "must remain byte exact" }"#;
    let snapshot = Snapshot::parse(mihomo, xray).unwrap();
    let directory = TempDirectory::new();
    let control = MihomoRuntimeControl::new(
        "127.0.0.1:19090".parse::<SocketAddr>().unwrap(),
        "ephemeral-secret",
    )
    .unwrap();

    let published =
        publish_runtime_with_mihomo_control(&snapshot, directory.path(), &control).unwrap();
    let runtime: Value =
        serde_yaml_ng::from_slice(&fs::read(published.mihomo_config).unwrap()).unwrap();
    let mapping = runtime.as_mapping().unwrap();
    let get = |key: &str| mapping.get(Value::String(key.to_owned()));

    assert_eq!(
        get("external-controller").and_then(Value::as_str),
        Some("127.0.0.1:19090")
    );
    assert_eq!(
        get("secret").and_then(Value::as_str),
        Some("ephemeral-secret")
    );
    for key in [
        "external-controller-unix",
        "external-controller-pipe",
        "external-controller-tls",
        "external-controller-cors",
        "external-controller-routing-mark",
        "cors",
        "external-doh-server",
        "global-client-fingerprint",
        "external-ui",
        "external-ui-name",
        "external-ui-url",
        "external-ui-custom-future-key",
    ] {
        assert!(get(key).is_none(), "unsafe runtime key remained: {key}");
    }
    assert_eq!(fs::read(published.xray_config).unwrap(), xray);
    let profile = get("profile").and_then(Value::as_mapping).unwrap();
    assert_eq!(
        profile
            .get(Value::String("store-selected".to_owned()))
            .and_then(Value::as_bool),
        Some(false)
    );
    assert_eq!(
        profile
            .get(Value::String("store-fake-ip".to_owned()))
            .and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(snapshot.mihomo.raw_yaml().as_bytes(), mihomo);
    assert_eq!(snapshot.xray.raw_bytes(), xray);
}

#[test]
fn runtime_publish_removes_superseded_secret_generations_only_after_success() {
    let directory = TempDirectory::new();
    let snapshot = Snapshot::parse(b"proxies: []\n", br#"{}"#).unwrap();
    let first_control =
        MihomoRuntimeControl::new("127.0.0.1:19090".parse().unwrap(), "old-secret").unwrap();
    let first =
        publish_runtime_with_mihomo_control(&snapshot, directory.path(), &first_control).unwrap();
    assert!(first.directory.is_dir());
    let unrelated = directory.path().join("runtime-not-a-generation");
    fs::create_dir(&unrelated).unwrap();

    let second_control =
        MihomoRuntimeControl::new("127.0.0.1:19090".parse().unwrap(), "new-secret").unwrap();
    let second =
        publish_runtime_with_mihomo_control(&snapshot, directory.path(), &second_control).unwrap();
    assert!(second.directory.is_dir());
    assert!(!first.directory.exists());
    assert!(unrelated.exists());
    let published: Vec<_> = fs::read_dir(directory.path())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .strip_prefix("runtime-")
                .is_some_and(|suffix| {
                    suffix.len() == 20 && suffix.bytes().all(|b| b.is_ascii_digit())
                })
        })
        .collect();
    assert_eq!(published.len(), 1);
}

#[test]
fn failed_runtime_publication_keeps_the_last_complete_generation() {
    let directory = TempDirectory::new();
    let snapshot = Snapshot::parse(b"proxies: []\n", br#"{}"#).unwrap();
    let control = MihomoRuntimeControl::new("127.0.0.1:19090".parse().unwrap(), "secret").unwrap();
    let first = publish_runtime_with_mihomo_control(&snapshot, directory.path(), &control).unwrap();
    let blocked = directory.path().join(format!(
        ".runtime-00000000000000000002.staging-{}",
        std::process::id()
    ));
    fs::create_dir(&blocked).unwrap();

    assert!(publish_runtime_with_mihomo_control(&snapshot, directory.path(), &control).is_err());
    assert!(first.directory.is_dir());
    assert!(first.mihomo_config.is_file());
    assert!(first.xray_config.is_file());
}

#[test]
fn staged_runtime_keeps_old_active_until_commit_and_abort_removes_only_new() {
    let directory = TempDirectory::new();
    let snapshot = Snapshot::parse(b"proxies: []\n", br#"{}"#).unwrap();
    let old_control =
        MihomoRuntimeControl::new("127.0.0.1:19090".parse().unwrap(), "old-secret").unwrap();
    let old =
        publish_runtime_with_mihomo_control(&snapshot, directory.path(), &old_control).unwrap();

    let new_control =
        MihomoRuntimeControl::new("127.0.0.1:19090".parse().unwrap(), "new-secret").unwrap();
    let staged =
        stage_runtime_with_mihomo_control(&snapshot, directory.path(), &new_control).unwrap();
    let staged_paths = staged.paths().clone();
    assert!(old.directory.is_dir());
    assert!(staged_paths.directory.is_dir());
    staged.abort().unwrap();
    assert!(old.directory.is_dir());
    assert!(!staged_paths.directory.exists());

    let staged =
        stage_runtime_with_mihomo_control(&snapshot, directory.path(), &new_control).unwrap();
    let committed = staged.commit().unwrap();
    assert!(committed.directory.is_dir());
    assert!(!old.directory.exists());
}

#[cfg(windows)]
#[test]
fn cleanup_failure_does_not_cancel_committed_runtime_and_is_retried_later() {
    use std::os::windows::fs::OpenOptionsExt;

    let directory = TempDirectory::new();
    let snapshot = Snapshot::parse(b"proxies: []\n", br#"{}"#).unwrap();
    let control = MihomoRuntimeControl::new("127.0.0.1:19090".parse().unwrap(), "secret").unwrap();
    let old = publish_runtime_with_mihomo_control(&snapshot, directory.path(), &control).unwrap();
    let locked = fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&old.mihomo_config)
        .unwrap();

    let staged = stage_runtime_with_mihomo_control(&snapshot, directory.path(), &control).unwrap();
    let active = staged.commit().unwrap();
    assert!(active.directory.is_dir());
    assert!(old.directory.is_dir());

    drop(locked);
    let staged = stage_runtime_with_mihomo_control(&snapshot, directory.path(), &control).unwrap();
    let newest = staged.commit().unwrap();
    assert!(newest.directory.is_dir());
    assert!(!old.directory.exists());
    assert!(!active.directory.exists());
}

#[test]
fn runtime_control_rejects_non_loopback_controller() {
    assert!(MihomoRuntimeControl::new("192.0.2.1:19090".parse().unwrap(), "secret").is_err());
}

#[test]
fn runtime_control_debug_output_redacts_secret() {
    let control =
        MihomoRuntimeControl::new("127.0.0.1:19090".parse().unwrap(), "must-never-be-logged")
            .unwrap();
    let debug = format!("{control:?}");
    assert!(debug.contains("[REDACTED]"));
    assert!(!debug.contains("must-never-be-logged"));
}

#[cfg(windows)]
#[test]
fn runtime_paths_have_a_protected_single_principal_dacl() {
    let directory = TempDirectory::new();
    let snapshot = Snapshot::parse(b"proxies: []\n", br#"{}"#).unwrap();
    let control = MihomoRuntimeControl::new("127.0.0.1:19090".parse().unwrap(), "secret").unwrap();
    let published =
        publish_runtime_with_mihomo_control(&snapshot, directory.path(), &control).unwrap();

    for path in [
        directory.path(),
        published.directory.as_path(),
        published.mihomo_config.as_path(),
        published.xray_config.as_path(),
    ] {
        assert_protected_single_principal_dacl(path);
    }
    assert!(!fs::read(&published.mihomo_config).unwrap().is_empty());
    assert_eq!(fs::read(&published.xray_config).unwrap(), br#"{}"#);
    let snapshot_root = directory.path().join("snapshots");
    let store = PersistentSnapshotStore::open(&snapshot_root).unwrap();
    store.commit(snapshot).unwrap();
    let generation = snapshot_root.join("snapshot-00000000000000000001");
    let mihomo = generation.join("mihomo.yaml");
    let xray = generation.join("xray.json");
    for path in [
        snapshot_root.as_path(),
        generation.as_path(),
        mihomo.as_path(),
        xray.as_path(),
    ] {
        assert_protected_single_principal_dacl(path);
    }
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
    let expected_inheritance = if fs::metadata(path).unwrap().is_dir() {
        (CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE) as u8
    } else {
        0
    };
    assert_eq!(
        ace.Header.AceFlags & (CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE) as u8,
        expected_inheritance
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

#[cfg(unix)]
#[test]
fn runtime_paths_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let directory = TempDirectory::new();
    let snapshot = Snapshot::parse(b"proxies: []\n", br#"{}"#).unwrap();
    let control = MihomoRuntimeControl::new("127.0.0.1:19090".parse().unwrap(), "secret").unwrap();
    let published =
        publish_runtime_with_mihomo_control(&snapshot, directory.path(), &control).unwrap();
    for path in [directory.path(), published.directory.as_path()] {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    for path in [published.mihomo_config, published.xray_config] {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let snapshot_root = directory.path().join("snapshots");
    let store = PersistentSnapshotStore::open(&snapshot_root).unwrap();
    store.commit(snapshot).unwrap();
    let generation = snapshot_root.join("snapshot-00000000000000000001");
    for path in [&snapshot_root, &generation] {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    for path in [generation.join("mihomo.yaml"), generation.join("xray.json")] {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
