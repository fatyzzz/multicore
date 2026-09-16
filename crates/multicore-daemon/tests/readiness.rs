use std::{
    ffi::OsStr,
    io::{self, BufRead, BufReader, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::Duration,
};

use multicore_daemon::{MAX_READINESS_LINE_BYTES, publish_readiness_if_enabled};

const TOKEN: &str = "secret-marker";

#[derive(Default)]
struct RecordingWriter {
    bytes: Vec<u8>,
    flushes: usize,
}

impl Write for RecordingWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flushes += 1;
        Ok(())
    }
}

#[test]
fn readiness_is_one_exact_bounded_ascii_line_and_is_flushed() {
    let address: SocketAddr = "127.0.0.1:43123".parse().unwrap();
    let mut writer = RecordingWriter::default();

    let published =
        publish_readiness_if_enabled(&mut writer, address, Some(OsStr::new("1"))).unwrap();

    assert!(published);
    assert_eq!(writer.bytes, b"MULTICORE_READY 127.0.0.1:43123\n");
    assert!(writer.bytes.is_ascii());
    assert!(writer.bytes.len() <= MAX_READINESS_LINE_BYTES);
    assert_eq!(
        writer.bytes.iter().filter(|byte| **byte == b'\n').count(),
        1
    );
    assert_eq!(writer.flushes, 1, "readiness must be explicitly flushed");
    assert!(
        !writer
            .bytes
            .windows(TOKEN.len())
            .any(|part| part == TOKEN.as_bytes())
    );
}

#[test]
fn readiness_accepts_a_literal_ipv6_loopback_address() {
    let address: SocketAddr = "[::1]:43123".parse().unwrap();
    let mut writer = RecordingWriter::default();

    assert!(publish_readiness_if_enabled(&mut writer, address, Some(OsStr::new("1"))).unwrap());
    assert_eq!(writer.bytes, b"MULTICORE_READY [::1]:43123\n");
    assert_eq!(writer.flushes, 1);
}

#[test]
fn readiness_rejects_zero_port_and_non_loopback_without_writing() {
    for address in [
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        "192.0.2.1:43123".parse().unwrap(),
    ] {
        let mut writer = RecordingWriter::default();
        let error =
            publish_readiness_if_enabled(&mut writer, address, Some(OsStr::new("1"))).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(writer.bytes.is_empty());
        assert_eq!(writer.flushes, 0);
    }
}

#[test]
fn readiness_is_disabled_unless_the_opt_in_value_is_exactly_one() {
    for opt_in in [None, Some(OsStr::new("")), Some(OsStr::new("true"))] {
        let mut writer = RecordingWriter::default();
        let published =
            publish_readiness_if_enabled(&mut writer, "127.0.0.1:43123".parse().unwrap(), opt_in)
                .unwrap();

        assert!(!published);
        assert!(writer.bytes.is_empty());
        assert_eq!(writer.flushes, 0);
    }
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn spawned_daemon_announces_real_address_and_serves_authenticated_status() {
    let root = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_multicore-daemon"))
        .env("MULTICORE_DAEMON_TOKEN", TOKEN)
        .env("MULTICORE_DAEMON_ADDR", "127.0.0.1:0")
        .env("MULTICORE_DAEMON_DATA_DIR", root.path())
        .env("MULTICORE_XRAY_BIN", root.path().join("xray-unused"))
        .env("MULTICORE_MIHOMO_BIN", root.path().join("mihomo-unused"))
        .env("MULTICORE_MIHOMO_CONTROLLER_ADDR", "127.0.0.1:19090")
        .env("MULTICORE_DAEMON_READY_STDOUT", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut guard = ChildGuard(child);

    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        let result = reader.read_line(&mut line).map(|_| line);
        let _ = sender.send(result);
    });
    let line = receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("daemon did not announce readiness")
        .expect("failed to read daemon stdout");

    assert!(line.len() <= MAX_READINESS_LINE_BYTES);
    assert!(line.is_ascii());
    assert!(line.ends_with('\n'));
    assert_eq!(line.matches('\n').count(), 1);
    assert!(!line.contains(TOKEN));
    let address: SocketAddr = line
        .strip_prefix("MULTICORE_READY ")
        .and_then(|value| value.strip_suffix('\n'))
        .expect("unexpected readiness prefix")
        .parse()
        .expect("readiness must contain a literal SocketAddr");
    assert!(address.ip().is_loopback());
    assert_ne!(address.port(), 0);

    let response = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap()
        .get(format!("http://{address}/v1/status"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);

    guard.0.kill().unwrap();
    guard.0.wait().unwrap();
}
