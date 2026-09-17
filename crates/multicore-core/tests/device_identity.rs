use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
};

use multicore_core::{
    DeviceIdentity, HttpClient, ReqwestHttpClient, UA_MIHOMO, UA_NATIVE, UA_XRAY, derive_incy_hwid,
    is_valid_hwid,
};

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "multicore-device-identity-{}-{}",
            std::process::id(),
            random_suffix()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn random_suffix() -> String {
    let mut bytes = [0_u8; 8];
    getrandom::fill(&mut bytes).unwrap();
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[test]
fn documented_incy_derivation_is_byte_exact_and_uppercase() {
    let hwid = derive_incy_hwid(
        "01234567-89ab-cdef-0123-456789abcdef",
        "DESKTOP-TEST",
        "windows",
        "x86_64",
        "alice",
    );
    assert_eq!(hwid, "DA91DA42-1CC9-8995-58D2-A73BF1DE7512");
    assert!(is_valid_hwid(&hwid));
}

#[test]
fn validator_rejects_wrong_case_shape_and_characters() {
    assert!(!is_valid_hwid("da91da42-1cc9-8995-58d2-a73bf1de7512"));
    assert!(!is_valid_hwid("DA91DA421CC9899558D2A73BF1DE7512"));
    assert!(!is_valid_hwid("DA91DA42-1CC9-8995-58D2-A73BF1DE751"));
    assert!(!is_valid_hwid("GA91DA42-1CC9-8995-58D2-A73BF1DE7512"));
    assert!(!is_valid_hwid("DA91DA42_1CC9-8995-58D2-A73BF1DE7512"));
}

#[test]
fn identity_is_stable_recovers_corruption_and_leaves_no_temporary_file() {
    let root = TestDirectory::new();
    let first = DeviceIdentity::load_or_create(&root.0).unwrap();
    let first_hwid = first.hwid().to_owned();
    assert!(is_valid_hwid(&first_hwid));
    assert_eq!(
        fs::read_to_string(root.0.join("identity/device_id")).unwrap(),
        first_hwid
    );

    let second = DeviceIdentity::load_or_create(&root.0).unwrap();
    assert_eq!(second.hwid(), first_hwid);

    fs::write(root.0.join("identity/device_id"), b"corrupt raw secret").unwrap();
    let recovered = DeviceIdentity::load_or_create(&root.0).unwrap();
    assert!(is_valid_hwid(recovered.hwid()));
    assert_ne!(recovered.hwid(), "corrupt raw secret");
    assert_eq!(
        fs::read_to_string(root.0.join("identity/device_id")).unwrap(),
        recovered.hwid()
    );
    let entries: Vec<_> = fs::read_dir(root.0.join("identity"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(entries.len(), 2);
    assert!(entries.iter().any(|name| name == "device_id"));
    assert!(entries.iter().any(|name| name == ".device_id.lock"));
    assert_eq!(
        fs::metadata(root.0.join("identity/.device_id.lock"))
            .unwrap()
            .len(),
        0
    );
}

#[test]
fn persistence_contains_only_the_final_hwid() {
    let root = TestDirectory::new();
    let identity = DeviceIdentity::load_or_create(&root.0).unwrap();
    let identity_directory = root.0.join("identity");
    let files: Vec<_> = fs::read_dir(&identity_directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(files.len(), 2);
    let device_id = identity_directory.join("device_id");
    let lock = identity_directory.join(".device_id.lock");
    assert!(files.contains(&device_id));
    assert!(files.contains(&lock));
    assert_eq!(fs::read_to_string(device_id).unwrap(), identity.hwid());
    assert_eq!(fs::metadata(lock).unwrap().len(), 0);
    let debug = format!("{identity:?}");
    assert!(!debug.contains(identity.hwid()));
}

#[tokio::test]
async fn one_real_client_sends_one_identity_with_every_subscription_user_agent() {
    let root = TestDirectory::new();
    let raw_values = [
        "01234567-89ab-cdef-0123-456789abcdef",
        "DESKTOP-TEST",
        "Windows 11 Test Build",
        "fixture-arch",
        "alice",
    ];
    let derived = derive_incy_hwid(
        raw_values[0],
        raw_values[1],
        raw_values[2],
        raw_values[3],
        raw_values[4],
    );
    fs::create_dir(root.0.join("identity")).unwrap();
    fs::write(root.0.join("identity/device_id"), &derived).unwrap();
    let identity = DeviceIdentity::load_or_create(&root.0).unwrap();
    let expected_hwid = identity.hwid().to_owned();
    let expected_version = identity.os_version().to_owned();
    let expected_model = identity.device_model().to_owned();
    let client = ReqwestHttpClient::new(identity).unwrap();
    let (url, requests, server) = capture_requests(3);

    for user_agent in [UA_NATIVE, UA_MIHOMO, UA_XRAY] {
        client.get(&url, user_agent).await.unwrap();
    }
    server.join().unwrap();
    let requests: Vec<_> = requests.into_iter().collect();
    assert_eq!(requests.len(), 3);
    for (request, user_agent) in requests.iter().zip([UA_NATIVE, UA_MIHOMO, UA_XRAY]) {
        assert_header(request, "x-hwid", &expected_hwid);
        assert_header(request, "x-device-os", "windows");
        assert_header(request, "x-ver-os", &expected_version);
        assert_header(request, "x-device-model", &expected_model);
        assert_header(request, "user-agent", user_agent);
        let accept = if user_agent == UA_NATIVE {
            "application/vnd.multicore.bundle+json, application/json"
        } else {
            "application/json, application/yaml, text/yaml, text/plain"
        };
        assert_header(request, "accept", accept);
        for forbidden in raw_values {
            assert!(!request.contains(forbidden));
        }
    }
}

fn capture_requests(count: usize) -> (String, mpsc::IntoIter<String>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, receiver) = mpsc::channel();
    let server = thread::spawn(move || {
        for _ in 0..count {
            let (mut stream, _) = listener.accept().unwrap();
            sender.send(read_request(&mut stream)).unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .unwrap();
        }
    });
    (
        format!("http://{address}/subscription"),
        receiver.into_iter(),
        server,
    )
}

fn read_request(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        let read = stream.read(&mut chunk).unwrap();
        bytes.extend_from_slice(&chunk[..read]);
        if read == 0 || bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    String::from_utf8(bytes).unwrap()
}

fn assert_header(request: &str, name: &str, expected: &str) {
    let expected = format!("{name}: {expected}");
    assert!(
        request
            .lines()
            .any(|line| line.eq_ignore_ascii_case(&expected)),
        "missing {expected:?} in {request:?}"
    );
}

#[cfg(windows)]
#[test]
fn identity_directory_and_file_have_owner_only_protected_acls() {
    let root = TestDirectory::new();
    DeviceIdentity::load_or_create(&root.0).unwrap();
    assert_protected_single_principal_dacl(&root.0.join("identity"));
    assert_protected_single_principal_dacl(&root.0.join("identity/device_id"));
    assert_protected_single_principal_dacl(&root.0.join("identity/.device_id.lock"));
}

#[cfg(windows)]
#[test]
fn identity_rejects_a_reparse_component_above_the_data_directory() {
    use std::{
        os::windows::process::CommandExt,
        process::{Command, Stdio},
    };

    let root = TestDirectory::new();
    let target = root.0.join("real-parent");
    let link = root.0.join("substituted-parent");
    fs::create_dir(&target).unwrap();
    let status = Command::new("cmd.exe")
        .args(["/d", "/c", "mklink", "/J"])
        .arg(&link)
        .arg(&target)
        .creation_flags(0x0800_0000)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "failed to create a test junction");
    assert!(DeviceIdentity::load_or_create(&link.join("data")).is_err());
    assert!(!target.join("data").exists());
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
    assert_eq!(status, ERROR_SUCCESS);
    let mut control = 0_u16;
    let mut revision = 0_u32;
    assert_ne!(
        unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) },
        0
    );
    assert_ne!(control & SE_DACL_PROTECTED, 0);
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
    assert_eq!(size.AceCount, 1);
    let mut raw_ace = ptr::null_mut();
    assert_ne!(unsafe { GetAce(dacl, 0, &mut raw_ace) }, 0);
    let ace = unsafe { &*raw_ace.cast::<ACCESS_ALLOWED_ACE>() };
    assert_eq!(ace.Header.AceType, 0, "ACE must be ACCESS_ALLOWED");
    assert_eq!(ace.Mask, FILE_ALL_ACCESS);
    let inheritance = if fs::metadata(path).unwrap().is_dir() {
        (CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE) as u8
    } else {
        0
    };
    assert_eq!(ace.Header.AceFlags & 3, inheritance);

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
