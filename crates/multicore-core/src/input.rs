use std::collections::HashSet;

use serde_json::Value as JsonValue;
use serde_yaml_ng::Value as YamlValue;
use thiserror::Error;

pub const MAX_CONFIG_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyGroup {
    pub name: String,
    pub group_type: String,
    pub proxies: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MihomoSocksMapping {
    pub name: String,
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MihomoConfig {
    raw: String,
    document: YamlValue,
    proxy_names: Vec<String>,
    groups: Vec<ProxyGroup>,
    loopback_socks_mappings: Vec<MihomoSocksMapping>,
}

impl MihomoConfig {
    pub fn raw_yaml(&self) -> &str {
        &self.raw
    }

    pub fn document(&self) -> &YamlValue {
        &self.document
    }

    pub fn proxy_names(&self) -> &[String] {
        &self.proxy_names
    }

    pub fn groups(&self) -> &[ProxyGroup] {
        &self.groups
    }

    pub fn loopback_socks_mappings(&self) -> &[MihomoSocksMapping] {
        &self.loopback_socks_mappings
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XrayConfig {
    raw: Vec<u8>,
}

impl XrayConfig {
    pub fn raw_bytes(&self) -> &[u8] {
        &self.raw
    }

    pub fn raw_json(&self) -> &str {
        // Construction validates UTF-8 and the bytes never change afterwards.
        std::str::from_utf8(&self.raw).expect("validated UTF-8")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ConfigError {
    #[error("configuration exceeds the 32 MiB limit")]
    TooLarge,
    #[error("configuration is not UTF-8")]
    InvalidUtf8,
    #[error("expected a Mihomo YAML mapping")]
    InvalidMihomo,
    #[error("expected an Xray JSON object")]
    InvalidXray,
}

pub fn parse_mihomo(bytes: &[u8]) -> Result<MihomoConfig, ConfigError> {
    check_size(bytes)?;
    let raw = std::str::from_utf8(bytes).map_err(|_| ConfigError::InvalidUtf8)?;
    let trimmed = raw.trim_start_matches('\u{feff}').trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        return Err(ConfigError::InvalidMihomo);
    }

    let document: YamlValue =
        serde_yaml_ng::from_str(raw).map_err(|_| ConfigError::InvalidMihomo)?;
    let mapping = document.as_mapping().ok_or(ConfigError::InvalidMihomo)?;

    const MIHOMO_ROOT_KEYS: &[&str] = &[
        "proxies",
        "proxy-groups",
        "rules",
        "rule-providers",
        "proxy-providers",
        "tun",
        "dns",
        "mixed-port",
        "socks-port",
        "redir-port",
        "tproxy-port",
        "listeners",
        "mode",
    ];
    if !MIHOMO_ROOT_KEYS
        .iter()
        .any(|key| mapping.contains_key(YamlValue::String((*key).into())))
    {
        return Err(ConfigError::InvalidMihomo);
    }

    validate_mihomo_sequence(mapping, "rules")?;
    validate_mihomo_sequence(mapping, "proxies")?;
    validate_mihomo_sequence(mapping, "proxy-groups")?;

    if let Some(proxies) = mapping
        .get(YamlValue::String("proxies".into()))
        .and_then(YamlValue::as_sequence)
    {
        for proxy in proxies {
            if string_field(proxy, "name").is_none() || string_field(proxy, "type").is_none() {
                return Err(ConfigError::InvalidMihomo);
            }
        }
    }
    if let Some(groups) = mapping
        .get(YamlValue::String("proxy-groups".into()))
        .and_then(YamlValue::as_sequence)
    {
        for group in groups {
            if string_field(group, "name").is_none() || string_field(group, "type").is_none() {
                return Err(ConfigError::InvalidMihomo);
            }
        }
    }

    let proxy_names = mapping
        .get(YamlValue::String("proxies".into()))
        .and_then(YamlValue::as_sequence)
        .into_iter()
        .flatten()
        .filter_map(|proxy| string_field(proxy, "name"))
        .map(ToOwned::to_owned)
        .collect();

    let groups = mapping
        .get(YamlValue::String("proxy-groups".into()))
        .and_then(YamlValue::as_sequence)
        .into_iter()
        .flatten()
        .filter_map(|group| {
            let name = string_field(group, "name")?.to_owned();
            let group_type = string_field(group, "type")?.to_owned();
            let proxies = group
                .as_mapping()?
                .get(YamlValue::String("proxies".into()))
                .and_then(YamlValue::as_sequence)
                .into_iter()
                .flatten()
                .filter_map(YamlValue::as_str)
                .map(ToOwned::to_owned)
                .collect();
            Some(ProxyGroup {
                name,
                group_type,
                proxies,
            })
        })
        .collect();

    let mut seen_mappings = HashSet::new();
    let loopback_socks_mappings = mapping
        .get(YamlValue::String("proxies".into()))
        .and_then(YamlValue::as_sequence)
        .into_iter()
        .flatten()
        .filter_map(|proxy| {
            let name = string_field(proxy, "name")?;
            if !string_field(proxy, "type")?.eq_ignore_ascii_case("socks5")
                || string_field(proxy, "server")? != "127.0.0.1"
            {
                return None;
            }
            let port = proxy
                .as_mapping()?
                .get(YamlValue::String("port".into()))?
                .as_u64()
                .and_then(|port| u16::try_from(port).ok())?;
            if port == 0 || !seen_mappings.insert(name) {
                return None;
            }
            Some(MihomoSocksMapping {
                name: name.to_owned(),
                port,
            })
        })
        .collect();

    Ok(MihomoConfig {
        raw: raw.to_owned(),
        document,
        proxy_names,
        groups,
        loopback_socks_mappings,
    })
}

pub fn parse_xray(bytes: &[u8]) -> Result<XrayConfig, ConfigError> {
    check_size(bytes)?;
    let raw = std::str::from_utf8(bytes).map_err(|_| ConfigError::InvalidUtf8)?;
    let document: JsonValue = serde_json::from_str(raw).map_err(|_| ConfigError::InvalidXray)?;
    if !document.is_object() {
        return Err(ConfigError::InvalidXray);
    }
    Ok(XrayConfig {
        raw: bytes.to_vec(),
    })
}

fn check_size(bytes: &[u8]) -> Result<(), ConfigError> {
    if bytes.len() > MAX_CONFIG_BYTES {
        Err(ConfigError::TooLarge)
    } else {
        Ok(())
    }
}

fn string_field<'a>(value: &'a YamlValue, key: &str) -> Option<&'a str> {
    value
        .as_mapping()?
        .get(YamlValue::String(key.into()))?
        .as_str()
}

fn validate_mihomo_sequence(
    mapping: &serde_yaml_ng::Mapping,
    key: &str,
) -> Result<(), ConfigError> {
    if mapping
        .get(YamlValue::String(key.into()))
        .is_some_and(|value| !value.is_sequence())
    {
        Err(ConfigError::InvalidMihomo)
    } else {
        Ok(())
    }
}
