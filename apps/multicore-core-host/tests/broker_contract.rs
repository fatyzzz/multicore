const HOST_MAIN: &str = include_str!("../src/main.rs");
const WINDOWS_PIPE: &str = include_str!("../src/windows_pipe.rs");

#[test]
fn host_command_line_and_diagnostics_never_carry_secrets_or_configs() {
    for forbidden in [
        "--secret",
        "--token",
        "--config",
        "--url",
        "subscription_url",
        "daemon bearer",
    ] {
        assert!(!HOST_MAIN.contains(forbidden));
        assert!(!WINDOWS_PIPE.contains(forbidden));
    }
    for required in ["--pipe", "--protocol", "--server-pid"] {
        assert!(WINDOWS_PIPE.contains(required));
    }
    assert!(HOST_MAIN.contains("privileged broker stopped safely"));
}

#[test]
fn windows_transport_is_bounded_overlapped_and_peer_authenticated() {
    for required in [
        "FILE_FLAG_OVERLAPPED",
        "SECURITY_SQOS_PRESENT",
        "SECURITY_IDENTIFICATION",
        "GetNamedPipeServerProcessId",
        "CancelIoEx",
        "MAX_ELEVATION_FRAME_BYTES",
        "ERROR_MORE_DATA",
    ] {
        assert!(WINDOWS_PIPE.contains(required), "missing {required}");
    }
    assert!(
        WINDOWS_PIPE.find("verify_server_pid").unwrap()
            < WINDOWS_PIPE.find("authentication_frame").unwrap()
    );
    assert!(WINDOWS_PIPE.contains("std::mem::forget(self.buffer.take()"));
    assert!(!WINDOWS_PIPE.contains("Err(HostError::TimedOut) => continue"));
}
