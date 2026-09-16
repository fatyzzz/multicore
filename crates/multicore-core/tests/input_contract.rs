use multicore_core::{ConfigError, parse_mihomo, parse_xray};

const MIHOMO: &str = r#"tun:
  enable: true
dns:
  enable: true
proxies:
  - name: NL-1
    type: socks5
    server: 127.0.0.1
    port: 31080
  - name: US-1
    type: socks5
    server: 127.0.0.1
    port: 31081
proxy-groups:
  - name: Proxy
    type: select
    proxies: [NL-1, US-1]
rules:
  - MATCH,Proxy
"#;

#[test]
fn mihomo_is_utf8_yaml_mapping_and_extracts_ui_catalog() {
    let parsed = parse_mihomo(MIHOMO.as_bytes()).unwrap();
    assert_eq!(parsed.proxy_names(), &["NL-1", "US-1"]);
    assert_eq!(parsed.groups().len(), 1);
    assert_eq!(parsed.groups()[0].name, "Proxy");
    assert_eq!(parsed.groups()[0].group_type, "select");
    assert_eq!(parsed.groups()[0].proxies, ["NL-1", "US-1"]);

    for invalid in [
        br#"{"proxies":[]}"#.as_slice(),
        br#"["vmess://one"]"#.as_slice(),
        b"dm1lc3M6Ly9leGFtcGxl".as_slice(),
        b"vless://synthetic\nvmess://synthetic".as_slice(),
        &[0xff, 0xfe],
    ] {
        assert!(parse_mihomo(invalid).is_err());
    }

    let oversized = vec![b'a'; 32 * 1024 * 1024 + 1];
    assert_eq!(parse_mihomo(&oversized), Err(ConfigError::TooLarge));
}

#[test]
fn xray_is_opaque_json_and_preserves_exact_input() {
    let raw = b"{\n  \"providerOwned\": true,\n  \"opaque\": [3, 2, 1]\n}\n";
    let parsed = parse_xray(raw).unwrap();
    assert_eq!(parsed.raw_bytes(), raw);
    assert_eq!(parsed.raw_json(), std::str::from_utf8(raw).unwrap());

    assert!(parse_xray(br#"[{"config":1}]"#).is_err());
    assert!(parse_xray(br#"[]"#).is_err());
    assert!(parse_xray(br#""vless://synthetic""#).is_err());
    assert!(parse_xray(b"{broken").is_err());
    assert!(parse_xray(&[0xff]).is_err());
}

#[test]
fn mihomo_extracts_only_unique_literal_loopback_socks_bridges() {
    let parsed = parse_mihomo(
        br#"proxies:
  - { name: Safe One, type: socks5, server: 127.0.0.1, port: 31080 }
  - { name: Safe One, type: socks5, server: 127.0.0.1, port: 31080 }
  - { name: Safe One, type: socks5, server: 127.0.0.1, port: 31099 }
  - { name: Other Loopback, type: SOCKS5, server: 127.0.0.1, port: 65535 }
  - { name: Remote, type: socks5, server: 192.0.2.1, port: 31081 }
  - { name: Hostname, type: socks5, server: localhost, port: 31082 }
  - { name: Zero, type: socks5, server: 127.0.0.1, port: 0 }
  - { name: Too Large, type: socks5, server: 127.0.0.1, port: 65536 }
  - { name: Wrong Type, type: http, server: 127.0.0.1, port: 31083 }
"#,
    )
    .unwrap();

    let bridges = parsed.loopback_socks_mappings();
    assert_eq!(bridges.len(), 2);
    assert_eq!(bridges[0].name, "Safe One");
    assert_eq!(bridges[0].port, 31080);
    assert_eq!(bridges[1].name, "Other Loopback");
    assert_eq!(bridges[1].port, 65535);
}
