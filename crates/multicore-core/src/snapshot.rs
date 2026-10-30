use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    fs::{self, OpenOptions},
    io::{self, Write},
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU64, Ordering},
    },
};

use serde::{Deserialize, Serialize};
use serde_yaml_ng::Value as YamlValue;
use thiserror::Error;

use crate::{
    ConfigError, MAX_CONFIG_BYTES, MihomoConfig, XrayConfig,
    fetch::SubscriptionMetadataHeaders,
    parse_mihomo, parse_xray,
    service_logo::{
        MAX_GROUP_ICON_OUTPUT_BYTES, MAX_SERVICE_LOGO_OUTPUT_BYTES, is_canonical_group_icon,
        is_canonical_service_logo,
    },
    store_root::{CapabilityFile, DirectoryCapability},
};

#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub mihomo: MihomoConfig,
    pub xray: XrayConfig,
    subscription: Option<SubscriptionRecord>,
    service_logo_png: Option<Vec<u8>>,
    service_logo_path: Option<PathBuf>,
    service_logo_file: Option<CapabilityFile>,
    /// The subscription named a logo but it could not be downloaded this
    /// time (never persisted); see `with_service_logo_carried_from`.
    service_logo_unfetched: bool,
    /// Normalized PNGs of Mihomo `proxy-groups[].icon` images, keyed by `group_icon_key`
    /// of the icon URL. Attached after the subscription is saved (`with_group_icons`; the
    /// daemon downloads and caches them in the background); generations written by 1.1.2
    /// also carry a `group-icons.json`, which is still read.
    group_icons: BTreeMap<String, GroupIcon>,
}

/// At most this many distinct group icon URLs are downloaded per subscription.
pub const MAX_GROUP_ICONS: usize = 32;
const GROUP_ICONS_FILE: &str = "group-icons.json";
const MAX_GROUP_ICONS_FILE_BYTES: u64 =
    (MAX_GROUP_ICONS * MAX_GROUP_ICON_OUTPUT_BYTES * 4 / 3 + 64 * 1024) as u64;

/// A stable, URL-free identifier for a group icon URL (the first 128 bits of its SHA-256 in
/// hex): the on-disk cache key of that URL's image.
pub fn group_icon_key(url: &str) -> String {
    group_icon_key_bytes(url.as_bytes())
}

/// A downloaded group icon and its content id: the id the UI sees changes whenever the image
/// does, so a provider swapping the picture behind the same URL is never shown stale.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GroupIcon {
    id: String,
    png: Vec<u8>,
}

impl GroupIcon {
    fn new(png: Vec<u8>) -> Self {
        Self {
            id: group_icon_key_bytes(&png),
            png,
        }
    }
}

fn group_icon_key_bytes(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(crate) fn is_group_icon_key(key: &str) -> bool {
    key.len() == 32
        && key
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// A lightweight lease that keeps a validated, app-owned service-logo file addressable.
///
/// On Linux the exposed path names the retained file descriptor. On Windows the retained handle
/// prevents the validated file from being replaced while another process reads it.
#[derive(Clone, Debug)]
pub struct ServiceLogoLease {
    retained: CapabilityFile,
}

impl ServiceLogoLease {
    pub(crate) fn new(retained: CapabilityFile) -> Self {
        Self { retained }
    }

    /// Valid only while this lease lives (on Linux it names the retained descriptor).
    pub fn path(&self) -> Option<&Path> {
        self.retained.path()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscriptionInfo {
    pub source_host: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub uploaded_bytes: Option<u64>,
    #[serde(default)]
    pub downloaded_bytes: Option<u64>,
    #[serde(default)]
    pub total_bytes: Option<u64>,
    #[serde(default)]
    pub expires_at_unix: Option<u64>,
    pub updated_at_unix: u64,
    #[serde(default)]
    pub refresh_interval_secs: Option<u64>,
    #[serde(default)]
    pub announcement_text: Option<String>,
    #[serde(default)]
    pub announcement_action_label: Option<String>,
    #[serde(default)]
    pub announcement_tone: Option<AnnouncementTone>,
    /// `dumb-mode: on`: the provider asks clients to show only the first proxy group.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dumb_mode: bool,
    /// `app-language: ru|en`: the interface language the provider asks for. The desktop
    /// applies it until the user picks a language in Settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_language: Option<multicore_i18n::Lang>,
}

impl SubscriptionInfo {
    pub fn used_bytes(&self) -> Option<u64> {
        match (self.uploaded_bytes, self.downloaded_bytes) {
            (Some(uploaded), Some(downloaded)) => Some(uploaded.saturating_add(downloaded)),
            (Some(uploaded), None) => Some(uploaded),
            (None, Some(downloaded)) => Some(downloaded),
            (None, None) => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AnnouncementTone {
    Info,
    Success,
    Danger,
}

impl AnnouncementTone {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Success => "success",
            Self::Danger => "danger",
        }
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SubscriptionRecord {
    source_url: String,
    info: SubscriptionInfo,
    #[serde(default)]
    targets: SubscriptionTargets,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct SubscriptionTargets {
    #[serde(default)]
    home_url: Option<String>,
    #[serde(default)]
    support_url: Option<String>,
    #[serde(default)]
    announcement_url: Option<String>,
    #[serde(default)]
    logo_url: Option<String>,
}

impl std::fmt::Debug for SubscriptionRecord {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SubscriptionRecord")
            .field("source_url", &"[REDACTED]")
            .field("info", &self.info)
            .finish()
    }
}

impl Snapshot {
    pub fn parse(mihomo: &[u8], xray: &[u8]) -> Result<Self, ConfigError> {
        // Both halves are fully constructed before a Snapshot can exist.
        let mihomo = parse_mihomo(mihomo)?;
        let xray = parse_xray(xray)?;
        Ok(Self {
            mihomo,
            xray,
            subscription: None,
            service_logo_png: None,
            service_logo_path: None,
            service_logo_file: None,
            service_logo_unfetched: false,
            group_icons: BTreeMap::new(),
        })
    }

    pub fn with_subscription_source(
        mut self,
        source_url: &str,
        userinfo_header: Option<&str>,
        updated_at_unix: u64,
    ) -> Result<Self, ConfigError> {
        let metadata = SubscriptionMetadataHeaders::from_legacy_userinfo(userinfo_header);
        self.attach_subscription(
            source_url,
            ParsedSubscriptionMetadata::parse(&metadata),
            updated_at_unix,
        )?;
        Ok(self)
    }

    pub(crate) fn with_subscription_metadata(
        mut self,
        source_url: &str,
        headers: &SubscriptionMetadataHeaders,
        updated_at_unix: u64,
    ) -> Result<Self, ConfigError> {
        self.attach_subscription(
            source_url,
            ParsedSubscriptionMetadata::parse(headers),
            updated_at_unix,
        )?;
        Ok(self)
    }

    pub(crate) fn with_merged_subscription_metadata(
        mut self,
        source_url: &str,
        mihomo: &SubscriptionMetadataHeaders,
        xray: &SubscriptionMetadataHeaders,
        updated_at_unix: u64,
    ) -> Result<Self, ConfigError> {
        let metadata = ParsedSubscriptionMetadata::parse(mihomo)
            .merge(ParsedSubscriptionMetadata::parse(xray));
        self.attach_subscription(source_url, metadata, updated_at_unix)?;
        Ok(self)
    }

    fn attach_subscription(
        &mut self,
        source_url: &str,
        metadata: ParsedSubscriptionMetadata,
        updated_at_unix: u64,
    ) -> Result<(), ConfigError> {
        let source_url = source_url.trim();
        let parsed = reqwest::Url::parse(source_url).map_err(|_| ConfigError::InvalidMihomo)?;
        if source_url.len() > 8192
            || !matches!(parsed.scheme(), "http" | "https")
            || parsed.host_str().is_none()
        {
            return Err(ConfigError::InvalidMihomo);
        }
        let source_host = parsed
            .host_str()
            .expect("validated subscription URL has a host")
            .to_owned();
        self.subscription = Some(SubscriptionRecord {
            source_url: source_url.to_owned(),
            info: SubscriptionInfo {
                display_name: metadata.title.unwrap_or_else(|| source_host.clone()),
                source_host,
                uploaded_bytes: metadata.uploaded_bytes,
                downloaded_bytes: metadata.downloaded_bytes,
                total_bytes: metadata.total_bytes.into_option(),
                expires_at_unix: metadata.expires_at_unix.into_option(),
                updated_at_unix,
                refresh_interval_secs: metadata.refresh_interval_secs,
                announcement_text: metadata.announcement_text.into_option(),
                announcement_action_label: metadata.announcement_action_label,
                announcement_tone: metadata.announcement_tone,
                dumb_mode: metadata.dumb_mode.unwrap_or(false),
                app_language: metadata.app_language,
            },
            targets: metadata.targets,
        });
        Ok(())
    }

    pub fn subscription_source_url(&self) -> Option<&str> {
        self.subscription
            .as_ref()
            .map(|subscription| subscription.source_url.as_str())
    }

    pub fn subscription_info(&self) -> Option<&SubscriptionInfo> {
        self.subscription
            .as_ref()
            .map(|subscription| &subscription.info)
    }

    pub fn subscription_home_url(&self) -> Option<&str> {
        self.subscription.as_ref()?.targets.home_url.as_deref()
    }

    pub fn subscription_support_url(&self) -> Option<&str> {
        self.subscription.as_ref()?.targets.support_url.as_deref()
    }

    pub fn subscription_announcement_url(&self) -> Option<&str> {
        self.subscription
            .as_ref()?
            .targets
            .announcement_url
            .as_deref()
    }

    /// The provider logo URL of a just-downloaded subscription (never persisted: a reloaded
    /// snapshot has none).
    pub fn subscription_logo_url(&self) -> Option<&str> {
        self.subscription.as_ref()?.targets.logo_url.as_deref()
    }

    pub(crate) fn take_subscription_logo_url(&mut self) -> Option<String> {
        self.subscription.as_mut()?.targets.logo_url.take()
    }

    pub(crate) fn set_service_logo_png(&mut self, png: Vec<u8>) {
        self.service_logo_png = Some(png);
        self.service_logo_path = None;
        self.service_logo_file = None;
        self.service_logo_unfetched = false;
    }

    pub(crate) fn mark_service_logo_unfetched(&mut self) {
        self.service_logo_unfetched = true;
    }

    /// A refresh re-downloads the provider logo; when that download fails
    /// (a network blip, or a fake-ip VPN answering DNS) the refreshed profile
    /// used to be saved without any logo. Keeps `previous`'s logo in that
    /// case only - a subscription that no longer names a logo drops it.
    pub fn with_service_logo_carried_from(self: Arc<Self>, previous: &Snapshot) -> Arc<Self> {
        let carry_logo = self.service_logo_unfetched
            && self.service_logo_png.is_none()
            && previous.service_logo_png.is_some();
        if !carry_logo {
            return self;
        }
        let mut carried = Arc::unwrap_or_clone(self);
        if let Some(png) = previous.service_logo_png.clone() {
            carried.set_service_logo_png(png);
        }
        Arc::new(carried)
    }

    /// The distinct, fetchable (absolute HTTPS) group icon URLs of the Mihomo config, in
    /// config order, at most `MAX_GROUP_ICONS`, each with its `group_icon_key`.
    pub fn group_icon_urls(&self) -> Vec<(String, String)> {
        let mut seen = BTreeSet::new();
        self.mihomo
            .groups()
            .iter()
            .filter_map(|group| group.icon.as_deref())
            .filter_map(|icon| validate_url(icon, false, true))
            .filter(|url| seen.insert(url.clone()))
            .take(MAX_GROUP_ICONS)
            .map(|url| (group_icon_key(&url), url))
            .collect()
    }

    /// This snapshot with the canonical icon PNGs `icons` (by `group_icon_key`) attached for
    /// the icon URLs its config names; other entries are ignored, as are images that are not
    /// canonical group icons. The original is left untouched.
    #[must_use]
    pub fn with_group_icons(
        self: Arc<Self>,
        icons: &std::collections::BTreeMap<String, Vec<u8>>,
    ) -> Arc<Self> {
        let attach: Vec<(String, Vec<u8>)> = self
            .group_icon_urls()
            .into_iter()
            .filter_map(|(key, _)| {
                let png = icons.get(&key)?;
                let unchanged = self
                    .group_icons
                    .get(&key)
                    .is_some_and(|current| current.png == *png);
                (!unchanged && is_canonical_group_icon(png)).then(|| (key, png.clone()))
            })
            .collect();
        if attach.is_empty() {
            return self;
        }
        let mut updated = Arc::unwrap_or_clone(self);
        for (key, png) in attach {
            updated.set_group_icon_png(key, png);
        }
        Arc::new(updated)
    }

    /// The icon URLs the config names whose image this snapshot does not hold.
    pub fn missing_group_icon_urls(&self) -> Vec<(String, String)> {
        self.group_icon_urls()
            .into_iter()
            .filter(|(key, _)| !self.group_icons.contains_key(key))
            .collect()
    }

    /// The icons this snapshot holds, by `group_icon_key` (to seed an icon cache).
    pub fn group_icon_pngs(&self) -> std::collections::BTreeMap<String, Vec<u8>> {
        self.group_icons
            .iter()
            .map(|(key, icon)| (key.clone(), icon.png.clone()))
            .collect()
    }

    /// The content ids of the icons this snapshot holds.
    pub fn group_icon_ids(&self) -> BTreeSet<String> {
        self.group_icons
            .values()
            .map(|icon| icon.id.clone())
            .collect()
    }

    /// The first proxy group named `group_name` names a fetchable icon image.
    pub fn names_group_icon(&self, group_name: &str) -> bool {
        self.mihomo
            .groups()
            .iter()
            .find(|group| group.name == group_name)
            .and_then(|group| group.icon.as_deref())
            .and_then(|icon| validate_url(icon, false, true))
            .is_some()
    }

    fn set_group_icon_png(&mut self, key: String, png: Vec<u8>) {
        self.group_icons.insert(key, GroupIcon::new(png));
    }

    /// The opaque content id of the downloaded icon of the first proxy group named
    /// `group_name`, when that group names an icon image and the image is available.
    pub fn group_icon_id(&self, group_name: &str) -> Option<String> {
        let icon = self
            .mihomo
            .groups()
            .iter()
            .find(|group| group.name == group_name)?
            .icon
            .as_deref()?;
        let key = group_icon_key(&validate_url(icon, false, true)?);
        self.group_icons.get(&key).map(|icon| icon.id.clone())
    }

    /// The canonical PNG behind a `group_icon_id`.
    pub fn group_icon_png(&self, id: &str) -> Option<&[u8]> {
        self.group_icons
            .values()
            .find(|icon| icon.id == id)
            .map(|icon| icon.png.as_slice())
    }

    pub fn service_logo_path(&self) -> Option<&Path> {
        self.service_logo_path.as_deref()
    }

    pub fn service_logo_lease(&self) -> Option<ServiceLogoLease> {
        self.service_logo_file
            .as_ref()
            .cloned()
            .map(|retained| ServiceLogoLease { retained })
    }
}

#[derive(Default)]
struct ParsedSubscriptionMetadata {
    title: Option<String>,
    uploaded_bytes: Option<u64>,
    downloaded_bytes: Option<u64>,
    total_bytes: MetadataValue<u64>,
    expires_at_unix: MetadataValue<u64>,
    refresh_interval_secs: Option<u64>,
    announcement_text: MetadataValue<String>,
    announcement_action_label: Option<String>,
    announcement_tone: Option<AnnouncementTone>,
    dumb_mode: Option<bool>,
    app_language: Option<multicore_i18n::Lang>,
    targets: SubscriptionTargets,
}

#[derive(Default)]
enum MetadataValue<T> {
    #[default]
    Missing,
    Suppressed,
    Value(T),
}

impl<T> MetadataValue<T> {
    fn or(self, fallback: Self) -> Self {
        match self {
            Self::Missing => fallback,
            present => present,
        }
    }

    fn into_option(self) -> Option<T> {
        match self {
            Self::Value(value) => Some(value),
            Self::Missing | Self::Suppressed => None,
        }
    }

    fn is_missing(&self) -> bool {
        matches!(self, Self::Missing)
    }
}

impl ParsedSubscriptionMetadata {
    fn parse(headers: &SubscriptionMetadataHeaders) -> Self {
        let mut metadata = Self {
            title: first_decoded_text(headers, "profile-title", 128, true)
                .or_else(|| first_vendor_decoded_text(headers, "servicename", 128, true))
                .or_else(|| content_disposition_title(headers)),
            ..Self::default()
        };
        for header in headers.suffix_values("subscription-userinfo") {
            parse_subscription_userinfo(header, &mut metadata);
        }
        metadata.refresh_interval_secs = headers
            .values("profile-update-interval")
            .find_map(parse_refresh_interval);
        metadata.announcement_text = first_announcement(headers);
        metadata.announcement_action_label =
            first_plain_text(headers, &["sub-info-button-text", "banner-button-text"], 32);
        metadata.announcement_tone = headers
            .values("sub-info-color")
            .find_map(|value| match value.trim().to_ascii_lowercase().as_str() {
                "blue" => Some(AnnouncementTone::Info),
                "green" => Some(AnnouncementTone::Success),
                "red" => Some(AnnouncementTone::Danger),
                _ => None,
            });
        metadata.targets.home_url = first_url(headers, &["profile-web-page-url"], false, false);
        metadata.targets.support_url = first_url(headers, &["support-url"], true, false);
        metadata.targets.announcement_url = first_url(
            headers,
            &["announce-url", "sub-info-button-link", "banner-button-url"],
            true,
            false,
        );
        metadata.targets.logo_url = first_vendor_url(headers, "servicelogo", false, true);
        metadata.dumb_mode = headers.suffix_values("dumb-mode").find_map(parse_switch);
        // `app-language` or `<vendor>-app-language`: `ru`/`en` in any case, or a regional tag
        // (`en-US`) for its base language. Other values are ignored.
        metadata.app_language = headers
            .suffix_values("app-language")
            .find_map(multicore_i18n::Lang::from_tag);
        metadata
    }

    fn merge(self, fallback: Self) -> Self {
        Self {
            title: self.title.or(fallback.title),
            uploaded_bytes: self.uploaded_bytes.or(fallback.uploaded_bytes),
            downloaded_bytes: self.downloaded_bytes.or(fallback.downloaded_bytes),
            total_bytes: self.total_bytes.or(fallback.total_bytes),
            expires_at_unix: self.expires_at_unix.or(fallback.expires_at_unix),
            refresh_interval_secs: self
                .refresh_interval_secs
                .or(fallback.refresh_interval_secs),
            announcement_text: self.announcement_text.or(fallback.announcement_text),
            announcement_action_label: self
                .announcement_action_label
                .or(fallback.announcement_action_label),
            announcement_tone: self.announcement_tone.or(fallback.announcement_tone),
            dumb_mode: self.dumb_mode.or(fallback.dumb_mode),
            app_language: self.app_language.or(fallback.app_language),
            targets: SubscriptionTargets {
                home_url: self.targets.home_url.or(fallback.targets.home_url),
                support_url: self.targets.support_url.or(fallback.targets.support_url),
                announcement_url: self
                    .targets
                    .announcement_url
                    .or(fallback.targets.announcement_url),
                logo_url: self.targets.logo_url.or(fallback.targets.logo_url),
            },
        }
    }
}

/// `on`/`off` style provider switches: on|off, true|false, yes|no, 1|0 in any case.
fn parse_switch(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" | "1" => Some(true),
        "off" | "false" | "no" | "0" => Some(false),
        _ => None,
    }
}

fn parse_subscription_userinfo(header: &str, metadata: &mut ParsedSubscriptionMetadata) {
    for field in header.split(';') {
        let Some((name, value)) = field.split_once('=') else {
            continue;
        };
        let Ok(value) = value.trim().parse::<u64>() else {
            continue;
        };
        match name.trim().to_ascii_lowercase().as_str() {
            "upload" if metadata.uploaded_bytes.is_none() => metadata.uploaded_bytes = Some(value),
            "download" if metadata.downloaded_bytes.is_none() => {
                metadata.downloaded_bytes = Some(value)
            }
            "total" if metadata.total_bytes.is_missing() => {
                metadata.total_bytes = if value == 0 {
                    MetadataValue::Suppressed
                } else {
                    MetadataValue::Value(value)
                }
            }
            "expire" if metadata.expires_at_unix.is_missing() => {
                metadata.expires_at_unix = if value == 0 {
                    MetadataValue::Suppressed
                } else {
                    MetadataValue::Value(value)
                }
            }
            _ => {}
        }
    }
}

fn first_decoded_text(
    headers: &SubscriptionMetadataHeaders,
    name: &str,
    limit: usize,
    reject_credential_like: bool,
) -> Option<String> {
    headers.values(name).find_map(|value| {
        let decoded = decode_text(value)?;
        if reject_credential_like {
            sanitize_title_candidate(&decoded, limit)
        } else {
            sanitize_text(&decoded, limit)
        }
    })
}

fn first_vendor_decoded_text(
    headers: &SubscriptionMetadataHeaders,
    suffix: &str,
    limit: usize,
    reject_credential_like: bool,
) -> Option<String> {
    headers.vendor_suffix_values(suffix).find_map(|value| {
        let decoded = decode_text(value)?;
        if reject_credential_like {
            sanitize_title_candidate(&decoded, limit)
        } else {
            sanitize_text(&decoded, limit)
        }
    })
}

fn sanitize_title_candidate(value: &str, limit: usize) -> Option<String> {
    if is_control_heavy(value) {
        return None;
    }
    let sanitized = sanitize_text(value, limit)?;
    (!looks_credential_like(&sanitized)).then_some(sanitized)
}

fn is_control_heavy(value: &str) -> bool {
    let mut total = 0_usize;
    let mut unsafe_scalars = 0_usize;
    for character in value.chars() {
        total += 1;
        if character.is_control() || is_bidi_control(character) {
            unsafe_scalars += 1;
        }
    }
    unsafe_scalars > 8 && unsafe_scalars.saturating_mul(4) > total
}

fn first_plain_text(
    headers: &SubscriptionMetadataHeaders,
    names: &[&str],
    limit: usize,
) -> Option<String> {
    names.iter().find_map(|name| {
        headers
            .values(name)
            .find_map(|value| sanitize_text(value, limit))
    })
}

fn first_announcement(headers: &SubscriptionMetadataHeaders) -> MetadataValue<String> {
    for name in ["announce", "sub-info-text", "banner-text"] {
        for value in headers.values(name) {
            let Some(decoded) = decode_text(value) else {
                continue;
            };
            if decoded.trim() == "0" {
                return MetadataValue::Suppressed;
            }
            if let Some(sanitized) = sanitize_text(&decoded, 512) {
                return MetadataValue::Value(sanitized);
            }
        }
    }
    MetadataValue::Missing
}

fn looks_credential_like(value: &str) -> bool {
    value.contains("://")
        || value
            .split_once('@')
            .is_some_and(|(authority, host)| authority.contains(':') && !host.is_empty())
}

fn decode_text(value: &str) -> Option<String> {
    let Some(encoded) = value.strip_prefix("base64:") else {
        return Some(value.to_owned());
    };
    use base64::Engine as _;
    let bytes = [
        &base64::engine::general_purpose::STANDARD,
        &base64::engine::general_purpose::STANDARD_NO_PAD,
        &base64::engine::general_purpose::URL_SAFE,
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
    ]
    .into_iter()
    .find_map(|engine| engine.decode(encoded.trim()).ok())?;
    String::from_utf8(bytes).ok()
}

fn sanitize_text(value: &str, limit: usize) -> Option<String> {
    let mut output = String::new();
    let mut pending_space = false;
    for character in value.chars() {
        if is_bidi_control(character) {
            continue;
        }
        if character.is_whitespace() || character.is_control() {
            pending_space = !output.is_empty();
            continue;
        }
        if pending_space && output.chars().count() < limit {
            output.push(' ');
        }
        pending_space = false;
        if output.chars().count() >= limit {
            break;
        }
        output.push(character);
    }
    (!output.is_empty()).then_some(output)
}

fn is_bidi_control(character: char) -> bool {
    matches!(
        character,
        '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
}

fn content_disposition_title(headers: &SubscriptionMetadataHeaders) -> Option<String> {
    headers.values("content-disposition").find_map(|value| {
        value.split(';').skip(1).find_map(|parameter| {
            let (name, value) = parameter.trim().split_once('=')?;
            if !name.eq_ignore_ascii_case("filename") {
                return None;
            }
            sanitize_title_candidate(value.trim().trim_matches('"'), 128)
        })
    })
}

fn parse_refresh_interval(value: &str) -> Option<u64> {
    let hours = value.trim().parse::<u128>().ok()?;
    let seconds = hours
        .saturating_mul(60 * 60)
        .clamp(15 * 60, 30 * 24 * 60 * 60);
    Some(seconds as u64)
}

fn first_url(
    headers: &SubscriptionMetadataHeaders,
    names: &[&str],
    allow_tg: bool,
    https_only: bool,
) -> Option<String> {
    names.iter().find_map(|name| {
        headers
            .values(name)
            .find_map(|value| validate_url(value, allow_tg, https_only))
    })
}

fn first_vendor_url(
    headers: &SubscriptionMetadataHeaders,
    suffix: &str,
    allow_tg: bool,
    https_only: bool,
) -> Option<String> {
    headers
        .vendor_suffix_values(suffix)
        .find_map(|value| validate_url(value, allow_tg, https_only))
}

fn validate_url(value: &str, allow_tg: bool, https_only: bool) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value.len() > 2048 || value.chars().any(char::is_control) {
        return None;
    }
    let parsed = reqwest::Url::parse(value).ok()?;
    let scheme_allowed = if https_only {
        parsed.scheme() == "https"
    } else {
        matches!(parsed.scheme(), "http" | "https") || (allow_tg && parsed.scheme() == "tg")
    };
    if !scheme_allowed
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return None;
    }
    Some(parsed.to_string())
}

#[derive(Debug, Default)]
pub struct AtomicSnapshot {
    current: RwLock<Option<Arc<Snapshot>>>,
}

impl AtomicSnapshot {
    pub fn current(&self) -> Option<Arc<Snapshot>> {
        self.current.read().expect("snapshot lock poisoned").clone()
    }

    pub fn commit(&self, snapshot: Snapshot) -> Arc<Snapshot> {
        let snapshot = Arc::new(snapshot);
        *self.current.write().expect("snapshot lock poisoned") = Some(snapshot.clone());
        snapshot
    }
}

pub trait SnapshotSink: Send + Sync {
    fn save(&self, snapshot: Snapshot) -> Result<SnapshotCommit<Arc<Snapshot>>, PersistenceError>;
}

impl SnapshotSink for AtomicSnapshot {
    fn save(&self, snapshot: Snapshot) -> Result<SnapshotCommit<Arc<Snapshot>>, PersistenceError> {
        Ok(SnapshotCommit {
            committed: self.commit(snapshot),
            maintenance_debt: None,
        })
    }
}

#[derive(Debug, Error)]
pub enum PersistenceError {
    #[error("snapshot persistence operation failed")]
    Io(#[from] io::Error),
    #[error("snapshot publication committed but directory durability maintenance is required")]
    MaintenanceDebt(#[source] io::Error),
}

#[derive(Debug)]
pub struct SnapshotCommit<T> {
    committed: T,
    maintenance_debt: Option<io::Error>,
}

impl<T> SnapshotCommit<T> {
    pub fn committed(&self) -> &T {
        &self.committed
    }

    pub fn maintenance_debt(&self) -> Option<&io::Error> {
        self.maintenance_debt.as_ref()
    }

    pub fn into_parts(self) -> (T, Option<io::Error>) {
        (self.committed, self.maintenance_debt)
    }

    pub fn into_committed(self) -> T {
        self.committed
    }
}

impl<T> std::ops::Deref for SnapshotCommit<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.committed
    }
}

#[derive(Debug)]
pub struct PersistentSnapshotStore {
    root: DirectoryCapability,
    current: RwLock<Option<(u64, Arc<Snapshot>)>>,
    next_generation: AtomicU64,
    commit_lock: Mutex<()>,
    #[cfg(test)]
    logo_retention_fault: std::sync::atomic::AtomicBool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedRuntime {
    pub directory: PathBuf,
    pub mihomo_config: PathBuf,
    pub xray_config: PathBuf,
}

/// A fully written and durable runtime generation which is not active yet. Dropping or aborting
/// it removes only this generation; committing it makes it active and then attempts old-generation
/// cleanup without turning an already durable activation into a failure.
#[derive(Debug)]
pub struct StagedRuntime {
    root: PathBuf,
    paths: PublishedRuntime,
    finished: bool,
}

impl StagedRuntime {
    pub fn paths(&self) -> &PublishedRuntime {
        &self.paths
    }

    pub fn commit(mut self) -> Result<PublishedRuntime, PersistenceError> {
        self.finished = true;
        let paths = self.paths.clone();
        // Cleanup is deliberately best-effort: failure cannot roll back the already durable
        // snapshot/controller activation. A later successful commit retries all old generations.
        let _ = cleanup_superseded_runtimes(&self.root, &paths.directory);
        Ok(paths)
    }

    pub fn abort(mut self) -> Result<(), PersistenceError> {
        remove_staged_runtime(&self.root, &self.paths.directory)?;
        self.finished = true;
        Ok(())
    }
}

impl Drop for StagedRuntime {
    fn drop(&mut self) {
        if !self.finished {
            let _ = remove_staged_runtime(&self.root, &self.paths.directory);
        }
    }
}

/// Private Mihomo controller settings injected only into an ephemeral runtime copy.
#[derive(Clone, PartialEq, Eq)]
pub struct MihomoRuntimeControl {
    address: SocketAddr,
    secret: String,
    xray_outbound_interface: Option<String>,
    external_ui_directory: Option<PathBuf>,
    /// System-proxy mode: TUN is disabled and Mihomo instead listens on this loopback
    /// mixed (HTTP+SOCKS) port, which the OS proxy settings point at.
    proxy_mixed_port: Option<u16>,
}

impl std::fmt::Debug for MihomoRuntimeControl {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MihomoRuntimeControl")
            .field("address", &self.address)
            .field("secret", &"[REDACTED]")
            .field(
                "xray_outbound_interface",
                &self
                    .xray_outbound_interface
                    .as_ref()
                    .map(|_| "[CONFIGURED]"),
            )
            .field("external_ui_directory", &self.external_ui_directory)
            .field("proxy_mixed_port", &self.proxy_mixed_port)
            .finish()
    }
}

impl MihomoRuntimeControl {
    pub fn new(address: SocketAddr, secret: impl Into<String>) -> io::Result<Self> {
        let secret = secret.into();
        if !address.ip().is_loopback() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Mihomo controller address must be loopback",
            ));
        }
        if secret.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Mihomo controller secret must not be empty",
            ));
        }
        Ok(Self {
            address,
            secret,
            xray_outbound_interface: None,
            external_ui_directory: None,
            proxy_mixed_port: None,
        })
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// Switches the runtime to system-proxy mode: the provider's `tun` is kept but disabled,
    /// and Mihomo listens on `127.0.0.1:<port>` only (`bind-address`, no LAN access). Without
    /// TUN no core needs administrator rights.
    pub fn with_proxy_mode(mut self, port: u16) -> io::Result<Self> {
        if port == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Mihomo proxy port must be a concrete loopback port",
            ));
        }
        self.proxy_mixed_port = Some(port);
        Ok(self)
    }

    /// The loopback mixed port in system-proxy mode; `None` in TUN mode.
    pub fn proxy_mixed_port(&self) -> Option<u16> {
        self.proxy_mixed_port
    }

    pub fn with_xray_outbound_interface(
        mut self,
        interface: impl Into<String>,
    ) -> io::Result<Self> {
        let interface = interface.into();
        let interface = interface.trim();
        if interface.is_empty() || interface.len() > 256 || interface.chars().any(char::is_control)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Xray outbound interface must be a non-empty Windows interface alias",
            ));
        }
        self.xray_outbound_interface = Some(interface.to_owned());
        Ok(self)
    }

    /// Points Mihomo's `external-ui` at a vendored, already-staged static dashboard directory so
    /// the runtime overlay serves it itself (see `crates/multicore-core/src/snapshot.rs` docs on
    /// `stage_runtime_with_mihomo_control`). The directory must be an absolute, existing directory
    /// containing an `index.html`; this crate never downloads or writes dashboard content, it only
    /// references a path the caller has already validated (checksum-verified at package time).
    ///
    /// Mihomo additionally requires this path to be a subpath of its own home directory, or listed
    /// in the `SAFE_PATHS` environment variable of the process that spawns it — this crate cannot
    /// enforce that from here; the caller that launches the Mihomo process is responsible for it.
    pub fn with_external_ui_directory(mut self, directory: impl Into<PathBuf>) -> io::Result<Self> {
        let directory = directory.into();
        if !directory.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Mihomo external-ui directory must be an absolute path",
            ));
        }
        if directory
            .as_os_str()
            .to_str()
            .is_none_or(|value| value.chars().any(char::is_control))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Mihomo external-ui directory must be valid, control-character-free Unicode",
            ));
        }
        if !fs::metadata(&directory).is_ok_and(|metadata| metadata.is_dir()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Mihomo external-ui directory must exist",
            ));
        }
        if !fs::metadata(directory.join("index.html")).is_ok_and(|metadata| metadata.is_file()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Mihomo external-ui directory must contain index.html",
            ));
        }
        self.external_ui_directory = Some(directory);
        Ok(self)
    }
}

impl PersistentSnapshotStore {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, PersistenceError> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root)?;
        secure_directory(&root)?;
        Self::open_capability(DirectoryCapability::open(&root)?)
    }

    pub(crate) fn open_capability(root: DirectoryCapability) -> Result<Self, PersistenceError> {
        let mut generations = generation_directories(&root)?;
        for (_, directory) in &generations {
            for config in [
                "mihomo.yaml",
                "xray.json",
                "subscription.json",
                "service-logo.png",
                GROUP_ICONS_FILE,
            ] {
                match directory.file_metadata(OsStr::new(config))? {
                    Some((true, _)) => directory.secure_regular_file(OsStr::new(config))?,
                    Some((false, _))
                        if !matches!(config, "service-logo.png" | GROUP_ICONS_FILE) =>
                    {
                        return Err(PersistenceError::Io(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "snapshot config is not a regular file",
                        )));
                    }
                    Some((false, _)) | None => {}
                }
            }
        }
        generations.sort_unstable_by_key(|entry| std::cmp::Reverse(entry.0));
        let highest_id = generations.first().map_or(0, |(id, _)| *id);
        let mut current = None;
        for (generation, directory) in generations {
            if let Some(snapshot) = load_generation(&directory) {
                current = Some((generation, Arc::new(snapshot)));
                break;
            }
        }

        Ok(Self {
            root,
            current: RwLock::new(current),
            next_generation: AtomicU64::new(highest_id.saturating_add(1).max(1)),
            commit_lock: Mutex::new(()),
            #[cfg(test)]
            logo_retention_fault: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Loads the newest valid generation from an existing legacy root without creating,
    /// locking, permission-repairing, or otherwise mutating that tree.
    pub(crate) fn load_latest_readonly(
        root: &Path,
    ) -> Result<Option<Arc<Snapshot>>, PersistenceError> {
        let root = DirectoryCapability::open_readonly(root)?;
        let mut generations = generation_directories(&root)?;
        generations.sort_unstable_by_key(|entry| std::cmp::Reverse(entry.0));
        Ok(generations
            .into_iter()
            .find_map(|(_, directory)| load_generation(&directory).map(Arc::new)))
    }

    pub fn current(&self) -> Option<Arc<Snapshot>> {
        self.current_with_generation().map(|(_, snapshot)| snapshot)
    }

    pub fn current_with_generation(&self) -> Option<(u64, Arc<Snapshot>)> {
        self.current
            .read()
            .expect("persistent snapshot lock poisoned")
            .as_ref()
            .map(|(generation, snapshot)| (*generation, snapshot.clone()))
    }

    pub fn current_generation(&self) -> Option<u64> {
        self.current
            .read()
            .expect("persistent snapshot lock poisoned")
            .as_ref()
            .map(|(generation, _)| *generation)
    }

    pub fn commit(
        &self,
        snapshot: Snapshot,
    ) -> Result<SnapshotCommit<Arc<Snapshot>>, PersistenceError> {
        self.commit_status(snapshot)
    }

    pub fn commit_status(
        &self,
        snapshot: Snapshot,
    ) -> Result<SnapshotCommit<Arc<Snapshot>>, PersistenceError> {
        let outcome = self.commit_with_generation_status(snapshot)?;
        let ((_, snapshot), maintenance_debt) = outcome.into_parts();
        Ok(SnapshotCommit {
            committed: snapshot,
            maintenance_debt,
        })
    }

    pub fn commit_with_generation(
        &self,
        snapshot: Snapshot,
    ) -> Result<SnapshotCommit<(u64, Arc<Snapshot>)>, PersistenceError> {
        self.commit_with_generation_status(snapshot)
    }

    pub fn commit_with_generation_status(
        &self,
        mut snapshot: Snapshot,
    ) -> Result<SnapshotCommit<(u64, Arc<Snapshot>)>, PersistenceError> {
        let _guard = self
            .commit_lock
            .lock()
            .expect("snapshot commit lock poisoned");
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
        let published_name = format!("snapshot-{generation:020}");
        let (staging_name, staging) = create_snapshot_staging(&self.root, &published_name)?;
        snapshot.service_logo_path = None;
        snapshot.service_logo_file = None;

        let publication = (|| {
            staging.write_file(
                OsStr::new("mihomo.yaml"),
                snapshot.mihomo.raw_yaml().as_bytes(),
            )?;
            staging.write_file(OsStr::new("xray.json"), snapshot.xray.raw_bytes())?;
            if let Some(subscription) = &snapshot.subscription {
                let mut persisted = subscription.clone();
                persisted.targets.logo_url = None;
                let encoded = serde_json::to_vec(&persisted).map_err(io::Error::other)?;
                staging.write_file(OsStr::new("subscription.json"), &encoded)?;
            }
            if let Some(logo) = &snapshot.service_logo_png {
                staging.write_file(OsStr::new("service-logo.png"), logo)?;
            }
            if !snapshot.group_icons.is_empty() {
                staging.write_file(
                    OsStr::new(GROUP_ICONS_FILE),
                    &encode_group_icons(&snapshot.group_icons)?,
                )?;
            }
            staging.sync()?;
            self.root
                .rename_child(&staging, OsStr::new(&published_name))?;
            Ok::<_, io::Error>(self.root.sync())
        })();
        drop(staging);

        let directory_sync = match publication {
            Ok(directory_sync) => directory_sync,
            Err(error) => {
                if let Err(cleanup) = self.root.remove_child_tree(&staging_name) {
                    return Err(PersistenceError::Io(io::Error::new(
                        cleanup.kind(),
                        format!(
                            "snapshot publication failed ({error}); staging cleanup failed: {cleanup}"
                        ),
                    )));
                }
                return Err(PersistenceError::Io(error));
            }
        };
        let mut maintenance_debt = directory_sync.err();
        if snapshot.service_logo_png.is_some() {
            #[cfg(test)]
            let inject_retention_failure = self
                .logo_retention_fault
                .swap(false, std::sync::atomic::Ordering::SeqCst);
            #[cfg(not(test))]
            let inject_retention_failure = false;
            let retention = if inject_retention_failure {
                Err(io::Error::other(
                    "injected post-publication logo retention failure",
                ))
            } else {
                self.root
                    .open_child(OsStr::new(&published_name))
                    .and_then(|directory| {
                        directory
                            .read_file_retained(
                                OsStr::new("service-logo.png"),
                                MAX_SERVICE_LOGO_OUTPUT_BYTES as u64,
                            )
                            .map(|(_, retained)| retained)
                    })
            };
            match retention {
                Ok(retained) => {
                    snapshot.service_logo_path = retained.path().map(Path::to_owned);
                    snapshot.service_logo_file = Some(retained);
                }
                Err(error) => {
                    maintenance_debt = Some(combine_persistence_debt(maintenance_debt, error));
                }
            }
        }
        let snapshot = Arc::new(snapshot);
        *self
            .current
            .write()
            .expect("persistent snapshot lock poisoned") = Some((generation, snapshot.clone()));
        prune_superseded_generations(&self.root, generation);
        Ok(SnapshotCommit {
            committed: (generation, snapshot),
            maintenance_debt,
        })
    }
}

/// How many of the newest snapshot generations are kept on disk. More than
/// one, so loading still falls back to an older valid generation if the
/// newest one is damaged.
const RETAINED_GENERATIONS: u64 = 3;

/// Every publish used to add a generation directory that was never
/// removed, so a long-lived profile collected one per refresh - and every
/// profile list rescans all of them. After a successful publish, removes
/// generations older than the newest `RETAINED_GENERATIONS`, plus staging
/// directories a previous process left behind when it died mid-publish.
/// Best-effort: an entry that is still held open (for example a leased
/// service logo) is simply retried on the next publish.
fn prune_superseded_generations(root: &DirectoryCapability, newest: u64) {
    let Ok(entries) = root.entries() else {
        return;
    };
    let own_staging_suffix = format!(".staging-{}", std::process::id());
    for entry in entries {
        if !entry.is_directory {
            continue;
        }
        let Some(name) = entry.name.to_str() else {
            continue;
        };
        let superseded = name
            .strip_prefix("snapshot-")
            .and_then(|id| id.parse::<u64>().ok())
            .is_some_and(|id| id.saturating_add(RETAINED_GENERATIONS) <= newest);
        let abandoned_staging = name.starts_with(".snapshot-")
            && name.contains(".staging-")
            && !name.ends_with(&own_staging_suffix);
        if superseded || abandoned_staging {
            let _ = root.remove_child_tree(OsStr::new(name));
        }
    }
}

impl SnapshotSink for PersistentSnapshotStore {
    fn save(&self, snapshot: Snapshot) -> Result<SnapshotCommit<Arc<Snapshot>>, PersistenceError> {
        self.commit_status(snapshot)
    }
}

#[cfg(test)]
impl PersistentSnapshotStore {
    pub(crate) fn inject_directory_sync_failure(&self) {
        self.root.inject_sync_failure();
    }

    fn inject_logo_retention_failure(&self) {
        self.logo_retention_fault
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

pub fn publish_runtime(
    snapshot: &Snapshot,
    runtime_root: impl AsRef<Path>,
) -> Result<PublishedRuntime, PersistenceError> {
    stage_runtime_payloads(
        snapshot.mihomo.raw_yaml().as_bytes(),
        snapshot.xray.raw_bytes(),
        runtime_root,
    )?
    .commit()
}

/// Publishes ephemeral core configs whose private controller and optional physical-interface
/// binding are owned by the daemon. The persisted subscription snapshot is never mutated.
pub fn publish_runtime_with_mihomo_control(
    snapshot: &Snapshot,
    runtime_root: impl AsRef<Path>,
    control: &MihomoRuntimeControl,
) -> Result<PublishedRuntime, PersistenceError> {
    stage_runtime_with_mihomo_control(snapshot, runtime_root, control)?.commit()
}

pub fn stage_runtime_with_mihomo_control(
    snapshot: &Snapshot,
    runtime_root: impl AsRef<Path>,
    control: &MihomoRuntimeControl,
) -> Result<StagedRuntime, PersistenceError> {
    let mut document = snapshot.mihomo.document().clone();
    let mapping = document.as_mapping_mut().ok_or_else(|| {
        PersistenceError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "validated Mihomo document is not a mapping",
        ))
    })?;
    for key in [
        "external-controller-unix",
        "external-controller-pipe",
        "external-controller-tls",
        "external-controller-cors",
        "external-controller-routing-mark",
        "cors",
        "external-doh-server",
        "global-client-fingerprint",
    ] {
        mapping.remove(YamlValue::String(key.to_owned()));
    }
    // The REALITY policy keys of the patched core stay, with a value it accepts (or not at all).
    crate::config_sanitize::normalize_reality_policy(mapping);
    let external_ui_keys: Vec<_> = mapping
        .keys()
        .filter(|key| {
            key.as_str()
                .is_some_and(|key| key.starts_with("external-ui"))
        })
        .cloned()
        .collect();
    for key in external_ui_keys {
        mapping.remove(&key);
    }
    mapping.insert(
        YamlValue::String("external-controller".to_owned()),
        YamlValue::String(control.address.to_string()),
    );
    mapping.insert(
        YamlValue::String("secret".to_owned()),
        YamlValue::String(control.secret.clone()),
    );
    if let Some(external_ui_directory) = &control.external_ui_directory {
        let external_ui_directory = external_ui_directory.to_str().ok_or_else(|| {
            PersistenceError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "Mihomo external-ui directory is not valid Unicode",
            ))
        })?;
        mapping.insert(
            YamlValue::String("external-ui".to_owned()),
            YamlValue::String(external_ui_directory.to_owned()),
        );
    }
    let ipv6_off = mapping
        .get(YamlValue::String("ipv6".to_owned()))
        .and_then(YamlValue::as_bool)
        == Some(false);
    let tun_key = YamlValue::String("tun".to_owned());
    let tun = mapping
        .entry(tun_key)
        .or_insert_with(|| YamlValue::Mapping(Default::default()));
    let tun = tun.as_mapping_mut().ok_or_else(|| {
        PersistenceError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "validated Mihomo tun section is not a mapping",
        ))
    })?;
    tun.insert(
        YamlValue::String("device".to_owned()),
        YamlValue::String("MultiCore".to_owned()),
    );
    // Mihomo v1.19.32 changed its implicit TUN stack from gvisor to the new
    // `mips` stack. Keep the stack every earlier MultiCore release ran on
    // unless the provider chose one explicitly.
    tun.entry(YamlValue::String("stack".to_owned()))
        .or_insert_with(|| YamlValue::String("gvisor".to_owned()));
    // Without IPv6 (`ipv6: false`, or an explicitly empty `inet6-address`) Mihomo gives the TUN
    // no IPv6 address, so auto-route installs no IPv6 routes and a dual-stack machine's native
    // IPv6 bypasses the tunnel. sing-tun's strict-route then makes IPv6 unreachable for every
    // process but Mihomo (Windows: a WFP block on ALE_AUTH_CONNECT_V6; Linux: an `unreachable`
    // rule for AF_INET6), and IPv4 still goes through the tunnel. A provider's explicit
    // strict-route, or auto-route turned off, stays their choice.
    let tun_has_no_ipv6 = ipv6_off
        || tun
            .get(YamlValue::String("inet6-address".to_owned()))
            .and_then(YamlValue::as_sequence)
            .is_some_and(Vec::is_empty);
    let auto_route = tun
        .get(YamlValue::String("auto-route".to_owned()))
        .and_then(YamlValue::as_bool)
        != Some(false);
    let strict_route_key = YamlValue::String("strict-route".to_owned());
    if tun_has_no_ipv6 && auto_route && tun.get(&strict_route_key).is_none_or(YamlValue::is_null) {
        tun.insert(strict_route_key, YamlValue::Bool(true));
    }
    if let Some(port) = control.proxy_mixed_port {
        tun.insert(
            YamlValue::String("enable".to_owned()),
            YamlValue::Bool(false),
        );
        mapping.insert(
            YamlValue::String("mixed-port".to_owned()),
            YamlValue::Number(port.into()),
        );
        mapping.insert(
            YamlValue::String("bind-address".to_owned()),
            YamlValue::String("127.0.0.1".to_owned()),
        );
        mapping.insert(
            YamlValue::String("allow-lan".to_owned()),
            YamlValue::Bool(false),
        );
    }
    crate::mihomo_dns_guard::apply(mapping);
    route_global_mode_through_first_group(mapping);
    let profile_key = YamlValue::String("profile".to_owned());
    let profile = mapping
        .entry(profile_key)
        .or_insert_with(|| YamlValue::Mapping(Default::default()));
    if !profile.is_mapping() {
        *profile = YamlValue::Mapping(Default::default());
    }
    profile
        .as_mapping_mut()
        .expect("profile was replaced by mapping")
        .insert(
            YamlValue::String("store-selected".to_owned()),
            YamlValue::Bool(false),
        );
    if control.xray_outbound_interface.is_some() {
        let domains = normalized_endpoint_domains(snapshot)?;
        if !domains.is_empty() {
            // Mihomo's TUN device hijacks every outbound port-53 packet on the
            // system (`dns-hijack: [any:53]`) regardless of source process or
            // destination address, so Xray's own scoped-resolver query for an
            // endpoint domain never actually reaches the real internet: it is
            // always redirected back into Mihomo itself. Mihomo then tries to
            // resolve it through its normal (proxy-routed) nameservers, which
            // depend on a working proxy connection - but that connection is
            // exactly what resolving this domain is needed to establish. A
            // live capture on real hardware confirmed this: the endpoint
            // domain never resolved, indefinitely, once the TUN adapter was
            // up, even though the same resolver answered fine for every other
            // domain. `nameserver-policy` makes Mihomo answer a hijacked query
            // for these specific domains directly against the plain resolver
            // Xray's own scoped entry already uses, breaking the cycle.
            let dns_key = YamlValue::String("dns".to_owned());
            let dns = mapping
                .entry(dns_key)
                .or_insert_with(|| YamlValue::Mapping(Default::default()));
            if !dns.is_mapping() {
                *dns = YamlValue::Mapping(Default::default());
            }
            let dns = dns.as_mapping_mut().expect("dns was replaced by mapping");
            let policy_key = YamlValue::String("nameserver-policy".to_owned());
            let policy = dns
                .entry(policy_key)
                .or_insert_with(|| YamlValue::Mapping(Default::default()));
            if !policy.is_mapping() {
                *policy = YamlValue::Mapping(Default::default());
            }
            let policy = policy
                .as_mapping_mut()
                .expect("nameserver-policy was replaced by mapping");
            for domain in &domains {
                policy.insert(
                    YamlValue::String(domain.clone()),
                    YamlValue::String(ENDPOINT_DNS_RESOLVER.to_owned()),
                );
            }
            // In fake-ip mode Mihomo answers before `nameserver-policy` is consulted, so the
            // hijacked query would get a fake address that only means something inside the
            // tunnel Xray deliberately dials around.
            crate::mihomo_dns_guard::exempt_from_fake_ip(dns, &domains);
        }
    }
    let mihomo = serde_yaml_ng::to_string(&document)
        .map_err(|error| PersistenceError::Io(io::Error::new(io::ErrorKind::InvalidData, error)))?;
    let xray = xray_runtime_bytes(snapshot, control.xray_outbound_interface.as_deref())?;
    stage_runtime_payloads(mihomo.as_bytes(), &xray, runtime_root)
}

/// In `mode: global` Mihomo sends every connection through its GLOBAL selector. Without a GLOBAL
/// group of the provider's own, Mihomo builds one whose default member is DIRECT, so a connected
/// client proxied nothing, and a location picked in the subscription's groups (the only ones the
/// app shows) changed nothing. A GLOBAL group listing those groups, first one first, makes global
/// mode use the location picked in the app. A group that itself lists GLOBAL is left out (Mihomo
/// rejects the loop); a provider's own GLOBAL group is kept as written.
fn route_global_mode_through_first_group(mapping: &mut serde_yaml_ng::Mapping) {
    let global_mode = mapping
        .get(YamlValue::String("mode".to_owned()))
        .and_then(YamlValue::as_str)
        .is_some_and(|mode| mode.trim().eq_ignore_ascii_case("global"));
    if !global_mode {
        return;
    }
    let Some(groups) = mapping
        .get_mut(YamlValue::String("proxy-groups".to_owned()))
        .and_then(YamlValue::as_sequence_mut)
    else {
        return;
    };
    let name = |group: &YamlValue| {
        group
            .get("name")
            .and_then(YamlValue::as_str)
            .map(str::to_owned)
    };
    if groups
        .iter()
        .any(|group| name(group).as_deref() == Some("GLOBAL"))
    {
        return;
    }
    let lists_global = |group: &YamlValue| {
        group
            .get("proxies")
            .and_then(YamlValue::as_sequence)
            .is_some_and(|members| {
                members
                    .iter()
                    .any(|member| member.as_str() == Some("GLOBAL"))
            })
    };
    let mut members: Vec<YamlValue> = groups
        .iter()
        .filter(|group| !lists_global(group))
        .filter_map(name)
        .map(YamlValue::String)
        .collect();
    if members.is_empty() {
        return;
    }
    members.push(YamlValue::String("DIRECT".to_owned()));
    let mut global = serde_yaml_ng::Mapping::new();
    global.insert(
        YamlValue::String("name".to_owned()),
        YamlValue::String("GLOBAL".to_owned()),
    );
    global.insert(
        YamlValue::String("type".to_owned()),
        YamlValue::String("select".to_owned()),
    );
    global.insert(
        YamlValue::String("proxies".to_owned()),
        YamlValue::Sequence(members),
    );
    groups.push(YamlValue::Mapping(global));
}

fn xray_runtime_bytes(
    snapshot: &Snapshot,
    outbound_interface: Option<&str>,
) -> Result<Vec<u8>, PersistenceError> {
    let Some(outbound_interface) = outbound_interface else {
        return Ok(snapshot.xray.raw_bytes().to_vec());
    };
    let invalid =
        |message: String| PersistenceError::Io(io::Error::new(io::ErrorKind::InvalidData, message));
    let mut document: serde_json::Value = serde_json::from_slice(snapshot.xray.raw_bytes())
        .map_err(|error| invalid(format!("validated Xray document is invalid: {error}")))?;
    let domains = normalized_endpoint_domains(snapshot)?;
    if !domains.is_empty()
        && (contains_endpoint_dns_tag(&document) || selects_endpoint_dns_outbound(&document))
    {
        return Err(invalid(
            "Xray subscription captures a reserved endpoint DNS tag".to_owned(),
        ));
    }
    let root = document
        .as_object_mut()
        .ok_or_else(|| invalid("validated Xray document is not an object".to_owned()))?;
    let outbounds = root
        .get_mut("outbounds")
        .and_then(serde_json::Value::as_array_mut)
        .ok_or_else(|| invalid("validated Xray outbounds section is not an array".to_owned()))?;
    if outbounds.is_empty() {
        return Err(invalid(
            "validated Xray outbounds section must not be empty".to_owned(),
        ));
    }

    for outbound in outbounds.iter_mut() {
        let outbound = outbound
            .as_object_mut()
            .ok_or_else(|| invalid("validated Xray outbound is not an object".to_owned()))?;
        let must_resolve_endpoint = !outbound_server_domains(outbound).is_empty();
        let stream_settings = outbound
            .entry("streamSettings")
            .or_insert_with(|| serde_json::Value::Object(Default::default()));
        if stream_settings.is_null() {
            *stream_settings = serde_json::Value::Object(Default::default());
        }
        let stream_settings = stream_settings
            .as_object_mut()
            .ok_or_else(|| invalid("validated Xray streamSettings is not an object".to_owned()))?;
        let sockopt = stream_settings
            .entry("sockopt")
            .or_insert_with(|| serde_json::Value::Object(Default::default()));
        if sockopt.is_null() {
            *sockopt = serde_json::Value::Object(Default::default());
        }
        let sockopt = sockopt
            .as_object_mut()
            .ok_or_else(|| invalid("validated Xray sockopt is not an object".to_owned()))?;
        sockopt.insert(
            "interface".to_owned(),
            serde_json::Value::String(outbound_interface.to_owned()),
        );
        if must_resolve_endpoint {
            sockopt.insert(
                "domainStrategy".to_owned(),
                serde_json::Value::String("ForceIP".to_owned()),
            );
        }
    }

    if !domains.is_empty() {
        // Append, never replace the provider's first/default outbound. DNS uses
        // a literal resolver IP, so this dial cannot recurse into endpoint DNS.
        //
        // Deliberately NOT bound to `outbound_interface`: a live-hardware
        // capture proved Mihomo's own DNS-leak protection (`strict-route`)
        // silently blackholes any UDP:53 packet explicitly source-bound to
        // the physical adapter - proven with a raw socket test that failed
        // identically for this endpoint domain and for an unrelated control
        // domain on port 53, while the exact same bind on a TCP/443 socket
        // (matching every other outbound above) connected fine. So this
        // query must NOT try to escape the tunnel by binding to the real
        // interface; it goes out unbound instead, which Mihomo's own TUN
        // captures via its system-wide DNS hijack and, thanks to the
        // `nameserver-policy` override above, answers directly and
        // correctly for exactly these domains.
        outbounds.push(serde_json::json!({
            "tag": ENDPOINT_DNS_OUTBOUND_TAG,
            "protocol": "freedom",
            "settings": {"domainStrategy": "AsIs"},
            "streamSettings": {"sockopt": {"domainStrategy": "AsIs"}}
        }));
        let dns = root
            .entry("dns")
            .or_insert_with(|| serde_json::Value::Object(Default::default()));
        if dns.is_null() {
            *dns = serde_json::Value::Object(Default::default());
        }
        let dns = dns
            .as_object_mut()
            .ok_or_else(|| invalid("validated Xray dns section is not an object".to_owned()))?;
        let servers = dns.entry("servers").or_insert(serde_json::Value::Null);
        // Preserve the old system resolver for unrelated Xray DNS consumers.
        // Endpoint lookups are scoped to the generated final-query entry.
        if servers.is_null() {
            *servers = serde_json::json!(["localhost"]);
        }
        let servers = servers.as_array_mut().ok_or_else(|| {
            invalid("validated Xray dns.servers section is not an array".to_owned())
        })?;
        if servers.is_empty() {
            servers.push(serde_json::json!("localhost"));
        }
        // Xray uses clients[0] even when skipFallback/disableFallback leave no
        // eligible clients. Keep the original resolver in that emergency slot,
        // with no matches and excluded from the normal fallback list.
        let mut emergency = match &servers[0] {
            serde_json::Value::String(address) => serde_json::json!({"address": address}),
            serde_json::Value::Object(_) => servers[0].clone(),
            _ => {
                return Err(invalid(
                    "validated Xray DNS server is not an object or string".to_owned(),
                ));
            }
        };
        emergency["domains"] = serde_json::json!([]);
        emergency["skipFallback"] = serde_json::json!(true);
        // Xray silently adds local-TLD/dotless rules to every localhost DNS
        // entry. Those would run before our scoped resolver. Such endpoints
        // require private DNS and cannot safely use this public-DNS policy.
        if emergency["address"]
            .as_str()
            .is_some_and(|address| address.trim().eq_ignore_ascii_case("localhost"))
            && domains.iter().any(|domain| local_dns_domain(domain))
        {
            return Err(invalid(
                "Xray public endpoint DNS cannot override local-only domains".to_owned(),
            ));
        }
        servers.insert(0, serde_json::json!({
            "address": ENDPOINT_DNS_RESOLVER,
            "domains": domains.iter().map(|domain| format!("full:{domain}")).collect::<Vec<_>>(),
            "skipFallback": true, "finalQuery": true,
            "disableCache": false, "serveStale": false,
            "tag": ENDPOINT_DNS_TAG, "timeoutMs": ENDPOINT_DNS_TIMEOUT_MS
        }));
        servers.insert(0, emergency);
        let routing = root.entry("routing").or_insert(serde_json::Value::Null);
        if routing.is_null() {
            *routing = serde_json::json!({});
        }
        let routing = routing
            .as_object_mut()
            .ok_or_else(|| invalid("validated Xray routing section is not an object".to_owned()))?;
        let rules = routing.entry("rules").or_insert(serde_json::Value::Null);
        if rules.is_null() {
            *rules = serde_json::json!([]);
        }
        let rules = rules.as_array_mut().ok_or_else(|| {
            invalid("validated Xray routing.rules section is not an array".to_owned())
        })?;
        rules.insert(
            0,
            serde_json::json!({
                "type": "field", "inboundTag": [ENDPOINT_DNS_TAG],
                "outboundTag": ENDPOINT_DNS_OUTBOUND_TAG
            }),
        );
    }

    serde_json::to_vec(&document)
        .map_err(|error| invalid(format!("Xray runtime serialization failed: {error}")))
}

const ENDPOINT_DNS_TAG: &str = "multicore-endpoint-dns";
const ENDPOINT_DNS_OUTBOUND_TAG: &str = "multicore-endpoint-dns-direct";
/// Xray's own timeout for one endpoint-domain lookup through the scoped
/// resolver (its `timeoutMs`); the pinned Xray retries a failed lookup
/// itself when it dials.
///
/// This was 3000: a defensive value chosen back when the endpoint
/// resolver's own query still had to survive Mihomo routing it through a
/// proxy-dependent nameserver, an indefinite failure mode with no safe
/// fast bound. A live-hardware capture then found and fixed the actual bug
/// (an interface-bound UDP:53 packet silently blackholed by Mihomo's own
/// DNS-leak protection); with that gone, the resolver answers in well
/// under a second even over a real link, so this can be far tighter.
const ENDPOINT_DNS_TIMEOUT_MS: u64 = 1200;
/// Shared between Xray's own scoped resolver entry and the Mihomo
/// `nameserver-policy` override below, so both agree on the one resolver
/// address that must be reachable for endpoint domains to ever resolve.
const ENDPOINT_DNS_RESOLVER: &str = "77.88.8.8";

fn local_dns_domain(domain: &str) -> bool {
    !domain.contains('.')
        || [
            "local",
            "localdomain",
            "localhost",
            "lan",
            "home.arpa",
            "example",
            "invalid",
            "test",
        ]
        .iter()
        .any(|suffix| domain == *suffix || domain.ends_with(&format!(".{suffix}")))
}

fn selects_endpoint_dns_outbound(document: &serde_json::Value) -> bool {
    let matches = |selector: &serde_json::Value| {
        if let Some(prefixes) = selector.as_str() {
            // Xray's StringList accepts comma-separated scalar selectors.
            return prefixes
                .split(',')
                .any(|prefix| ENDPOINT_DNS_OUTBOUND_TAG.starts_with(prefix));
        }
        selector.as_array().is_some_and(|prefixes| {
            prefixes
                .iter()
                .filter_map(serde_json::Value::as_str)
                .any(|prefix| ENDPOINT_DNS_OUTBOUND_TAG.starts_with(prefix))
        })
    };
    document
        .pointer("/routing/balancers")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|balancers| {
            balancers
                .iter()
                .any(|balancer| matches(&balancer["selector"]))
        })
        || matches(&document["observatory"]["subjectSelector"])
        || matches(&document["burstObservatory"]["subjectSelector"])
}

fn contains_endpoint_dns_tag(value: &serde_json::Value) -> bool {
    // Fail closed even for references to a missing provider tag: injecting an
    // outbound with that name could otherwise make an unrelated rule direct.
    match value {
        serde_json::Value::String(value) => {
            value == ENDPOINT_DNS_TAG || value == ENDPOINT_DNS_OUTBOUND_TAG
        }
        serde_json::Value::Array(values) => values.iter().any(contains_endpoint_dns_tag),
        serde_json::Value::Object(values) => values.values().any(contains_endpoint_dns_tag),
        _ => false,
    }
}

/// Xray strips the terminating FQDN dot before DNS matching; shared by both
/// the Xray endpoint-DNS overlay and the Mihomo `nameserver-policy` override
/// below so the two always agree on exactly which domains need it.
fn normalized_endpoint_domains(snapshot: &Snapshot) -> Result<BTreeSet<String>, PersistenceError> {
    Ok(xray_outbound_server_domains(snapshot)?
        .into_iter()
        .map(|domain| {
            let domain = domain.trim();
            domain
                .strip_suffix('.')
                .unwrap_or(domain)
                .to_ascii_lowercase()
        })
        .collect())
}

pub fn xray_outbound_server_domains(snapshot: &Snapshot) -> Result<Vec<String>, PersistenceError> {
    let document: serde_json::Value =
        serde_json::from_slice(snapshot.xray.raw_bytes()).map_err(|error| {
            PersistenceError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("validated Xray document is invalid: {error}"),
            ))
        })?;
    let outbounds = document
        .get("outbounds")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            PersistenceError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "validated Xray outbounds section is not an array",
            ))
        })?;
    let mut domains = BTreeSet::new();
    for outbound in outbounds {
        let outbound = outbound.as_object().ok_or_else(|| {
            PersistenceError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "validated Xray outbound is not an object",
            ))
        })?;
        domains.extend(outbound_server_domains(outbound));
    }
    Ok(domains.into_iter().collect())
}

fn outbound_server_domains(outbound: &serde_json::Map<String, serde_json::Value>) -> Vec<String> {
    let Some(settings) = outbound
        .get("settings")
        .and_then(serde_json::Value::as_object)
    else {
        return Vec::new();
    };
    let direct = settings.get("address").and_then(serde_json::Value::as_str);
    let servers = ["servers", "vnext"]
        .into_iter()
        .filter_map(|key| settings.get(key).and_then(serde_json::Value::as_array))
        .flatten()
        .filter_map(serde_json::Value::as_object)
        .filter_map(|server| server.get("address"))
        .filter_map(serde_json::Value::as_str);
    direct
        .into_iter()
        .chain(servers)
        .filter(|address| address.parse::<IpAddr>().is_err())
        .filter(|address| !address.is_empty() && !address.chars().any(char::is_control))
        .map(ToOwned::to_owned)
        .collect()
}

fn stage_runtime_payloads(
    mihomo: &[u8],
    xray: &[u8],
    runtime_root: impl AsRef<Path>,
) -> Result<StagedRuntime, PersistenceError> {
    let runtime_root = runtime_root.as_ref();
    fs::create_dir_all(runtime_root)?;
    secure_directory(runtime_root)?;
    let generation = next_named_generation(runtime_root, "runtime-")?;
    let published_name = format!("runtime-{generation:020}");
    let published = runtime_root.join(&published_name);
    let staging = runtime_root.join(format!(".{published_name}.staging-{}", std::process::id()));

    let preparation = (|| {
        fs::create_dir(&staging)?;
        secure_directory(&staging)?;
        write_synced(&staging.join("mihomo.yaml"), mihomo)?;
        write_synced(&staging.join("xray.json"), xray)?;
        sync_directory(&staging)?;
        fs::rename(&staging, &published)
    })();
    if let Err(error) = preparation {
        if let Err(cleanup) = fs::remove_dir_all(&staging)
            && cleanup.kind() != io::ErrorKind::NotFound
        {
            return Err(PersistenceError::Io(io::Error::new(
                cleanup.kind(),
                format!("runtime preparation failed ({error}); staging cleanup failed: {cleanup}"),
            )));
        }
        return Err(PersistenceError::Io(error));
    }
    if let Err(error) = sync_directory(runtime_root) {
        return Err(PersistenceError::MaintenanceDebt(error));
    }

    Ok(StagedRuntime {
        root: runtime_root.to_owned(),
        paths: PublishedRuntime {
            mihomo_config: published.join("mihomo.yaml"),
            xray_config: published.join("xray.json"),
            directory: published,
        },
        finished: false,
    })
}

fn remove_staged_runtime(root: &Path, staged: &Path) -> Result<(), PersistenceError> {
    let canonical_root = fs::canonicalize(root)?;
    let canonical_staged = fs::canonicalize(staged)?;
    if canonical_staged.parent() != Some(canonical_root.as_path())
        || !is_runtime_generation_name(
            canonical_staged
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default(),
        )
    {
        return Err(PersistenceError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "staged runtime escaped its parent",
        )));
    }
    fs::remove_dir_all(canonical_staged)?;
    sync_directory(root)?;
    Ok(())
}

fn generation_directories(
    root: &DirectoryCapability,
) -> Result<Vec<(u64, DirectoryCapability)>, io::Error> {
    let mut generations = Vec::new();
    for entry in root.entries()? {
        if !entry.is_directory {
            continue;
        }
        let name = entry.name;
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(id) = name
            .strip_prefix("snapshot-")
            .and_then(|id| id.parse::<u64>().ok())
        else {
            continue;
        };
        generations.push((id, root.open_child(OsStr::new(name))?));
    }
    Ok(generations)
}

fn next_named_generation(root: &Path, prefix: &str) -> Result<u64, io::Error> {
    let mut highest = 0_u64;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(ToOwned::to_owned) else {
            continue;
        };
        if let Some(id) = name
            .strip_prefix(prefix)
            .and_then(|id| id.parse::<u64>().ok())
        {
            highest = highest.max(id);
        }
    }
    Ok(highest.saturating_add(1).max(1))
}

fn load_generation(directory: &DirectoryCapability) -> Option<Snapshot> {
    let mihomo = directory
        .read_file(OsStr::new("mihomo.yaml"), MAX_CONFIG_BYTES as u64)
        .ok()?;
    let xray = directory
        .read_file(OsStr::new("xray.json"), MAX_CONFIG_BYTES as u64)
        .ok()?;
    let mut snapshot = Snapshot::parse(&mihomo, &xray).ok()?;
    if directory
        .file_metadata(OsStr::new("subscription.json"))
        .ok()
        .flatten()
        .is_some_and(|(regular, len)| regular && len <= 16 * 1024)
        && let Ok(bytes) = directory.read_file(OsStr::new("subscription.json"), 16 * 1024)
        && let Ok(mut record) = serde_json::from_slice::<SubscriptionRecord>(&bytes)
        && let Ok(parsed) = reqwest::Url::parse(&record.source_url)
        && record.source_url.len() <= 8192
        && matches!(parsed.scheme(), "http" | "https")
        && parsed.host_str() == Some(record.info.source_host.as_str())
    {
        if record.info.display_name.is_empty() {
            record
                .info
                .display_name
                .clone_from(&record.info.source_host);
        }
        snapshot.subscription = Some(record);
    }
    if directory
        .file_metadata(OsStr::new("service-logo.png"))
        .ok()
        .flatten()
        .is_some_and(|(regular, len)| regular && len <= MAX_SERVICE_LOGO_OUTPUT_BYTES as u64)
        && let Ok((bytes, retained)) = directory.read_file_retained(
            OsStr::new("service-logo.png"),
            MAX_SERVICE_LOGO_OUTPUT_BYTES as u64,
        )
        && is_canonical_service_logo(&bytes)
    {
        snapshot.service_logo_png = Some(bytes);
        snapshot.service_logo_path = retained.path().map(Path::to_owned);
        snapshot.service_logo_file = Some(retained);
    }
    if directory
        .file_metadata(OsStr::new(GROUP_ICONS_FILE))
        .ok()
        .flatten()
        .is_some_and(|(regular, len)| regular && len <= MAX_GROUP_ICONS_FILE_BYTES)
        && let Ok(bytes) =
            directory.read_file(OsStr::new(GROUP_ICONS_FILE), MAX_GROUP_ICONS_FILE_BYTES)
    {
        snapshot.group_icons = decode_group_icons(&bytes);
    }
    Some(snapshot)
}

fn encode_group_icons(icons: &BTreeMap<String, GroupIcon>) -> io::Result<Vec<u8>> {
    use base64::Engine as _;
    let encoded: BTreeMap<&str, String> = icons
        .iter()
        .map(|(key, icon)| {
            (
                key.as_str(),
                base64::engine::general_purpose::STANDARD.encode(&icon.png),
            )
        })
        .collect();
    serde_json::to_vec(&encoded).map_err(io::Error::other)
}

/// Every entry is re-validated: a damaged or foreign entry is dropped, never shown.
fn decode_group_icons(bytes: &[u8]) -> BTreeMap<String, GroupIcon> {
    use base64::Engine as _;
    let Ok(encoded) = serde_json::from_slice::<BTreeMap<String, String>>(bytes) else {
        return BTreeMap::new();
    };
    encoded
        .into_iter()
        .filter(|(key, _)| is_group_icon_key(key))
        .take(MAX_GROUP_ICONS)
        .filter_map(|(key, value)| {
            let png = base64::engine::general_purpose::STANDARD
                .decode(value)
                .ok()?;
            is_canonical_group_icon(&png).then(|| (key, GroupIcon::new(png)))
        })
        .collect()
}

fn create_snapshot_staging(
    root: &DirectoryCapability,
    published_name: &str,
) -> Result<(OsString, DirectoryCapability), PersistenceError> {
    let name = OsString::from(format!(".{published_name}.staging-{}", std::process::id()));
    let child = root.create_child_for_rename(&name)?;
    Ok((name, child))
}

fn combine_persistence_debt(existing: Option<io::Error>, next: io::Error) -> io::Error {
    match existing {
        Some(existing) => io::Error::new(
            next.kind(),
            format!(
                "directory sync failed ({existing}); post-publication maintenance failed: {next}"
            ),
        ),
        None => next,
    }
}

fn write_synced(path: &Path, bytes: &[u8]) -> Result<(), io::Error> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    secure_file(path)?;
    file.write_all(bytes)?;
    file.flush()?;
    file.sync_all()
}

fn cleanup_superseded_runtimes(root: &Path, active: &Path) -> io::Result<()> {
    let canonical_root = fs::canonicalize(root)?;
    let canonical_active = fs::canonicalize(active)?;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !is_runtime_generation_name(name) {
            continue;
        }
        let file_type = entry.file_type()?;
        if !file_type.is_dir() || file_type.is_symlink() {
            continue;
        }
        let candidate = fs::canonicalize(entry.path())?;
        if candidate == canonical_active {
            continue;
        }
        if candidate.parent() != Some(canonical_root.as_path()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "runtime generation escaped its parent",
            ));
        }
        fs::remove_dir_all(candidate)?;
    }
    Ok(())
}

fn is_runtime_generation_name(name: &str) -> bool {
    name.strip_prefix("runtime-").is_some_and(|suffix| {
        suffix.len() == 20 && suffix.bytes().all(|byte| byte.is_ascii_digit())
    })
}

#[cfg(unix)]
pub(crate) fn secure_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(unix)]
pub(crate) fn secure_file(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(windows)]
pub(crate) fn secure_directory(path: &Path) -> io::Result<()> {
    apply_owner_only_acl(path, true)
}

#[cfg(windows)]
pub(crate) fn secure_file(path: &Path) -> io::Result<()> {
    apply_owner_only_acl(path, false)
}

#[cfg(windows)]
fn apply_owner_only_acl(path: &Path, inheritable: bool) -> io::Result<()> {
    use std::{ffi::c_void, os::windows::ffi::OsStrExt, ptr};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, LocalFree},
        Security::{
            Authorization::{
                EXPLICIT_ACCESS_W, NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT, SET_ACCESS,
                SetEntriesInAclW, SetNamedSecurityInfoW, TRUSTEE_IS_SID, TRUSTEE_IS_USER,
                TRUSTEE_W,
            },
            DACL_SECURITY_INFORMATION, GetTokenInformation, PROTECTED_DACL_SECURITY_INFORMATION,
            SUB_CONTAINERS_AND_OBJECTS_INHERIT, TOKEN_QUERY, TOKEN_USER, TokenUser,
        },
        Storage::FileSystem::FILE_ALL_ACCESS,
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };

    let mut token: HANDLE = ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let result = (|| {
        let mut bytes_needed = 0_u32;
        unsafe {
            GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut bytes_needed);
        }
        if bytes_needed == 0 {
            return Err(io::Error::last_os_error());
        }
        let word_size = std::mem::size_of::<usize>();
        let mut token_info = vec![0_usize; (bytes_needed as usize).div_ceil(word_size)];
        if unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                token_info.as_mut_ptr().cast::<c_void>(),
                bytes_needed,
                &mut bytes_needed,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let user = unsafe { &*(token_info.as_ptr().cast::<TOKEN_USER>()) };
        let mut wide_path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        // Reading the current DACL is cheap and side-effect free; rewriting
        // it is not. Every store open repairs permissions, and the profile
        // list is polled, so an unconditional rewrite touched every snapshot
        // file on every poll - and a security change makes real-time
        // scanners rescan the file, keeping it open right when the next
        // write needs it (the Windows 10 os error 32 on subscription
        // import). Only write when the DACL is not already owner-only.
        if dacl_is_owner_only(&wide_path, user.User.Sid, inheritable) {
            return Ok(());
        }
        let trustee = TRUSTEE_W {
            pMultipleTrustee: ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_USER,
            ptstrName: user.User.Sid.cast(),
        };
        let access = EXPLICIT_ACCESS_W {
            grfAccessPermissions: FILE_ALL_ACCESS,
            grfAccessMode: SET_ACCESS,
            grfInheritance: if inheritable {
                SUB_CONTAINERS_AND_OBJECTS_INHERIT
            } else {
                0
            },
            Trustee: trustee,
        };
        let mut acl = ptr::null_mut();
        let status = unsafe { SetEntriesInAclW(1, &access, ptr::null(), &mut acl) };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        let status = unsafe {
            SetNamedSecurityInfoW(
                wide_path.as_mut_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                acl,
                ptr::null(),
            )
        };
        unsafe { LocalFree(acl.cast()) };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        Ok(())
    })();
    unsafe { CloseHandle(token) };
    result
}

/// Whether `path` already carries exactly the DACL `apply_owner_only_acl`
/// would write: protected, one explicit allow-all ACE for `user`, with the
/// same inheritance flags. Any failure to read it counts as "no", so the
/// caller falls back to writing it.
#[cfg(windows)]
fn dacl_is_owner_only(
    wide_path: &[u16],
    user: windows_sys::Win32::Security::PSID,
    inheritable: bool,
) -> bool {
    use std::{ffi::c_void, ptr};
    use windows_sys::Win32::{
        Foundation::{ERROR_SUCCESS, LocalFree},
        Security::{
            ACCESS_ALLOWED_ACE, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
            Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT},
            DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation,
            GetSecurityDescriptorControl, PSECURITY_DESCRIPTOR, SE_DACL_PROTECTED,
        },
        Storage::FileSystem::FILE_ALL_ACCESS,
    };
    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
    const INHERITANCE_FLAGS: u8 = 0x1f;
    let expected_flags: u8 = if inheritable { 0x03 } else { 0x00 };

    let mut dacl: *mut ACL = ptr::null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    let status = unsafe {
        GetNamedSecurityInfoW(
            wide_path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut dacl,
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != ERROR_SUCCESS || descriptor.is_null() {
        return false;
    }
    let matches = (|| {
        if dacl.is_null() {
            return false;
        }
        let mut control = 0_u16;
        let mut revision = 0_u32;
        if unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) } == 0
            || control & SE_DACL_PROTECTED == 0
        {
            return false;
        }
        let mut size = ACL_SIZE_INFORMATION {
            AceCount: 0,
            AclBytesInUse: 0,
            AclBytesFree: 0,
        };
        if unsafe {
            GetAclInformation(
                dacl,
                (&mut size as *mut ACL_SIZE_INFORMATION).cast::<c_void>(),
                std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )
        } == 0
            || size.AceCount != 1
        {
            return false;
        }
        let mut ace: *mut c_void = ptr::null_mut();
        if unsafe { GetAce(dacl, 0, &mut ace) } == 0 || ace.is_null() {
            return false;
        }
        let ace = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
        ace.Header.AceType == ACCESS_ALLOWED_ACE_TYPE
            && ace.Header.AceFlags & INHERITANCE_FLAGS == expected_flags
            && ace.Mask == FILE_ALL_ACCESS
            && unsafe { EqualSid(ptr::addr_of!(ace.SidStart).cast_mut().cast(), user) } != 0
    })();
    unsafe { LocalFree(descriptor.cast()) };
    matches
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn secure_directory(_path: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "owner-only filesystem protection is unsupported on this platform",
    ))
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn secure_file(_path: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "owner-only filesystem protection is unsupported on this platform",
    ))
}

#[cfg(unix)]
pub(crate) fn sync_directory(path: &Path) -> Result<(), io::Error> {
    fs::File::open(path)?.sync_all()
}

#[cfg(windows)]
pub(crate) fn sync_directory(path: &Path) -> Result<(), io::Error> {
    DirectoryCapability::open(path)?.sync()
}

#[cfg(test)]
mod persistence_tests {
    use super::*;

    fn minimal_snapshot() -> Snapshot {
        Snapshot::parse(
            b"proxies: []
proxy-groups: []
rules: []
",
            br#"{"outbounds":[]}"#,
        )
        .unwrap()
    }

    fn generation_names(root: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(root)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("snapshot-") || name.starts_with(".snapshot-"))
            .collect();
        names.sort();
        names
    }

    #[test]
    fn publishing_keeps_only_the_newest_generations_and_sweeps_abandoned_staging() {
        let root = tempfile::tempdir().unwrap();
        let store = PersistentSnapshotStore::open(root.path()).unwrap();
        // A staging directory a crashed earlier process left behind.
        let abandoned = root
            .path()
            .join(".snapshot-00000000000000000099.staging-4294967295");
        fs::create_dir(&abandoned).unwrap();
        fs::write(abandoned.join("mihomo.yaml"), b"partial").unwrap();

        for _ in 0..5 {
            store.commit(minimal_snapshot()).unwrap();
        }

        assert_eq!(
            generation_names(root.path()),
            [
                "snapshot-00000000000000000003",
                "snapshot-00000000000000000004",
                "snapshot-00000000000000000005",
            ]
        );
        assert_eq!(store.current_generation(), Some(5));
        let reopened = PersistentSnapshotStore::open(root.path()).unwrap();
        assert_eq!(reopened.current_generation(), Some(5));
    }

    #[test]
    fn an_entry_that_cannot_be_pruned_never_fails_the_publish() {
        let root = tempfile::tempdir().unwrap();
        let store = PersistentSnapshotStore::open(root.path()).unwrap();
        store.commit(minimal_snapshot()).unwrap();
        // Hold a file of generation 1 open, the way a leased logo would be.
        #[cfg(windows)]
        let _held = {
            use std::os::windows::fs::OpenOptionsExt;
            fs::OpenOptions::new()
                .read(true)
                .share_mode(1) // FILE_SHARE_READ only: blocks deletion
                .open(
                    root.path()
                        .join("snapshot-00000000000000000001")
                        .join("mihomo.yaml"),
                )
                .unwrap()
        };

        for _ in 0..3 {
            store.commit(minimal_snapshot()).unwrap();
        }

        assert_eq!(store.current_generation(), Some(4));
    }

    #[test]
    fn committed_outcome_carries_snapshot_when_post_publication_sync_fails() {
        let root = tempfile::tempdir().unwrap();
        let store = PersistentSnapshotStore::open(root.path()).unwrap();
        store.inject_directory_sync_failure();
        let snapshot = Snapshot::parse(
            b"proxies: []\nproxy-groups: []\nrules: []\n",
            br#"{"outbounds":[]}"#,
        )
        .unwrap();

        let outcome = store.commit_with_generation(snapshot).unwrap();
        assert!(outcome.maintenance_debt().is_some());
        let ((generation, committed), debt) = outcome.into_parts();
        assert_eq!(generation, 1);
        assert!(debt.is_some());
        assert!(Arc::ptr_eq(&committed, &store.current().unwrap()));
    }

    #[test]
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "android", windows)),
        ignore = "the logo path comes from /proc/<pid>/fd, which this platform lacks"
    )]
    fn committed_outcome_survives_post_publication_logo_retention_failure() {
        let root = tempfile::tempdir().unwrap();
        let store = PersistentSnapshotStore::open(root.path()).unwrap();
        let mut snapshot = Snapshot::parse(
            b"proxies: []\nproxy-groups: []\nrules: []\n",
            br#"{"outbounds":[]}"#,
        )
        .unwrap();
        snapshot.set_service_logo_png(b"persisted logo bytes".to_vec());
        let first = store.commit(snapshot).unwrap().into_committed();
        assert!(first.service_logo_path().is_some());

        store.inject_logo_retention_failure();
        let outcome = store.commit_with_generation((*first).clone()).unwrap();
        assert!(outcome.maintenance_debt().is_some());
        let ((generation, committed), debt) = outcome.into_parts();
        assert_eq!(generation, 2);
        assert!(debt.is_some());
        assert!(committed.service_logo_path().is_none());
        assert!(Arc::ptr_eq(&committed, &store.current().unwrap()));
        assert!(
            root.path()
                .join("snapshot-00000000000000000002/service-logo.png")
                .is_file()
        );
    }
}

#[cfg(all(test, windows))]
mod owner_only_acl_tests {
    use super::{dacl_is_owner_only, secure_directory, secure_file};
    use std::{os::windows::ffi::OsStrExt, path::Path};

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    fn current_user_sid() -> Vec<usize> {
        use windows_sys::Win32::{
            Foundation::{CloseHandle, HANDLE},
            Security::{GetTokenInformation, TOKEN_QUERY, TokenUser},
            System::Threading::{GetCurrentProcess, OpenProcessToken},
        };
        let mut token: HANDLE = std::ptr::null_mut();
        assert_ne!(
            unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) },
            0
        );
        let mut needed = 0_u32;
        unsafe { GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed) };
        let mut buffer = vec![0_usize; (needed as usize).div_ceil(std::mem::size_of::<usize>())];
        assert_ne!(
            unsafe {
                GetTokenInformation(
                    token,
                    TokenUser,
                    buffer.as_mut_ptr().cast(),
                    needed,
                    &mut needed,
                )
            },
            0
        );
        unsafe { CloseHandle(token) };
        buffer
    }

    fn change_time(path: &Path) -> i64 {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_BASIC_INFO, FileBasicInfo, GetFileInformationByHandleEx,
        };
        let file = std::fs::File::open(path).unwrap();
        let mut info = FILE_BASIC_INFO::default();
        assert_ne!(
            unsafe {
                GetFileInformationByHandleEx(
                    file.as_raw_handle(),
                    FileBasicInfo,
                    (&mut info as *mut FILE_BASIC_INFO).cast(),
                    std::mem::size_of::<FILE_BASIC_INFO>() as u32,
                )
            },
            0
        );
        info.ChangeTime
    }

    #[test]
    fn detects_owner_only_dacl_only_after_it_was_applied() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("mihomo.yaml");
        std::fs::write(&file, b"x").unwrap();
        let token = current_user_sid();
        let sid = unsafe {
            (*token
                .as_ptr()
                .cast::<windows_sys::Win32::Security::TOKEN_USER>())
            .User
            .Sid
        };

        assert!(
            !dacl_is_owner_only(&wide(&file), sid, false),
            "inherited DACL"
        );
        secure_file(&file).unwrap();
        assert!(dacl_is_owner_only(&wide(&file), sid, false));
        assert!(
            !dacl_is_owner_only(&wide(&file), sid, true),
            "a file DACL must not be mistaken for the inheritable directory one"
        );

        let directory = temp.path().join("snapshot-00000000000000000001");
        std::fs::create_dir(&directory).unwrap();
        assert!(!dacl_is_owner_only(&wide(&directory), sid, true));
        secure_directory(&directory).unwrap();
        assert!(dacl_is_owner_only(&wide(&directory), sid, true));
    }

    /// Regression coverage for the Windows 10 import failure: repairing an
    /// already-correct DACL must not touch the file at all, because every
    /// security change makes real-time scanners reopen it.
    #[test]
    fn securing_an_already_secured_file_does_not_modify_it() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("xray.json");
        std::fs::write(&file, b"{}").unwrap();
        secure_file(&file).unwrap();
        let before = change_time(&file);

        std::thread::sleep(std::time::Duration::from_millis(50));
        secure_file(&file).unwrap();

        assert_eq!(change_time(&file), before, "the DACL was rewritten again");

        // Self-check that ChangeTime really reflects DACL writes: applying a
        // *different* DACL (the inheritable directory form) must move it.
        std::thread::sleep(std::time::Duration::from_millis(50));
        secure_directory(&file).unwrap();
        assert_ne!(
            change_time(&file),
            before,
            "ChangeTime does not track DACL writes"
        );
    }
}
