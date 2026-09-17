use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use reqwest::header::{ACCEPT, HeaderMap, HeaderValue, USER_AGENT};
use serde_json::value::RawValue;
use thiserror::Error;

use crate::{DeviceIdentity, MAX_CONFIG_BYTES, Snapshot, SnapshotSink};

pub const UA_NATIVE: &str = "multicore-json-massive";
pub const UA_MIHOMO: &str = "multicore-mihomo";
pub const UA_XRAY: &str = "multicore-xray";

#[derive(Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
    metadata: SubscriptionMetadataHeaders,
}

impl std::fmt::Debug for HttpResponse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HttpResponse")
            .field("status", &self.status)
            .field("body_len", &self.body.len())
            .field("metadata_count", &self.metadata.fields.len())
            .finish()
    }
}

const MAX_METADATA_HEADER_BYTES: usize = 16 * 1024;
const MAX_METADATA_FIELD_BYTES: usize = 4 * 1024;

#[derive(Clone, Default, PartialEq, Eq)]
pub(crate) struct SubscriptionMetadataHeaders {
    fields: Vec<(String, String)>,
}

impl SubscriptionMetadataHeaders {
    pub(crate) fn from_legacy_userinfo(value: Option<&str>) -> Self {
        Self::from_pairs(
            value
                .into_iter()
                .map(|value| ("subscription-userinfo", value)),
        )
    }

    fn from_pairs<N, V>(headers: impl IntoIterator<Item = (N, V)>) -> Self
    where
        N: AsRef<str>,
        V: AsRef<str>,
    {
        let mut fields = Vec::new();
        let mut aggregate = 0_usize;
        for (name, value) in headers {
            let name = name.as_ref().to_ascii_lowercase();
            let value = value.as_ref();
            if !is_metadata_header(&name)
                || name.len() > 128
                || value.len() > MAX_METADATA_FIELD_BYTES
                || value.contains(['\r', '\n'])
            {
                continue;
            }
            let size = name.len().saturating_add(value.len());
            if aggregate.saturating_add(size) > MAX_METADATA_HEADER_BYTES {
                break;
            }
            aggregate += size;
            fields.push((name, value.to_owned()));
        }
        Self { fields }
    }

    fn from_header_map(headers: &HeaderMap) -> Self {
        Self::from_pairs(headers.iter().filter_map(|(name, value)| {
            std::str::from_utf8(value.as_bytes())
                .ok()
                .map(|value| (name.as_str(), value))
        }))
    }

    pub(crate) fn values<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.fields
            .iter()
            .filter(move |(candidate, _)| candidate == name)
            .map(|(_, value)| value.as_str())
    }

    pub(crate) fn suffix_values<'a>(
        &'a self,
        suffix: &'a str,
    ) -> impl Iterator<Item = &'a str> + 'a {
        self.fields
            .iter()
            .filter(move |(candidate, _)| {
                candidate == suffix
                    || candidate
                        .strip_suffix(suffix)
                        .is_some_and(|prefix| prefix.ends_with('-'))
            })
            .map(|(_, value)| value.as_str())
    }
}

fn is_metadata_header(name: &str) -> bool {
    matches!(
        name,
        "profile-title"
            | "flclashx-servicename"
            | "content-disposition"
            | "flclashx-servicelogo"
            | "profile-update-interval"
            | "profile-web-page-url"
            | "support-url"
            | "announce"
            | "sub-info-text"
            | "banner-text"
            | "announce-url"
            | "sub-info-button-link"
            | "banner-button-url"
            | "sub-info-button-text"
            | "banner-button-text"
            | "sub-info-color"
    ) || name == "subscription-userinfo"
        || name.ends_with("-subscription-userinfo")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum FetchError {
    #[error("subscription request failed")]
    Network,
    #[error("subscription server returned an unsuccessful status")]
    HttpStatus,
    #[error("native subscription bundle is invalid")]
    InvalidBundle,
    #[error("subscription configuration is invalid")]
    InvalidConfig,
    #[error("subscription snapshot could not be persisted")]
    Persistence,
    #[error("subscription response exceeds its size limit")]
    TooLarge,
}

pub trait HttpClient: Send + Sync {
    fn get<'a>(
        &'a self,
        url: &'a str,
        user_agent: &'static str,
    ) -> Pin<Box<dyn Future<Output = Result<HttpResponse, FetchError>> + Send + 'a>>;
}

#[derive(Clone)]
pub struct ReqwestHttpClient {
    client: reqwest::Client,
}

impl ReqwestHttpClient {
    pub fn new(identity: DeviceIdentity) -> Result<Self, FetchError> {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-hwid",
            HeaderValue::from_str(identity.hwid()).map_err(|_| FetchError::Network)?,
        );
        headers.insert("x-device-os", HeaderValue::from_static("windows"));
        headers.insert(
            "x-ver-os",
            HeaderValue::from_str(identity.os_version()).map_err(|_| FetchError::Network)?,
        );
        headers.insert(
            "x-device-model",
            HeaderValue::from_str(identity.device_model()).map_err(|_| FetchError::Network)?,
        );
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|_| FetchError::Network)?;
        Ok(Self { client })
    }
}

impl HttpClient for ReqwestHttpClient {
    fn get<'a>(
        &'a self,
        url: &'a str,
        user_agent: &'static str,
    ) -> Pin<Box<dyn Future<Output = Result<HttpResponse, FetchError>> + Send + 'a>> {
        Box::pin(async move {
            let mut response = self
                .client
                .get(url)
                .header(USER_AGENT, user_agent)
                .header(ACCEPT, accept_for(user_agent))
                .send()
                .await
                .map_err(|_| FetchError::Network)?;
            let status = response.status().as_u16();
            let metadata = SubscriptionMetadataHeaders::from_header_map(response.headers());
            let limit = response_limit(user_agent);
            if response
                .content_length()
                .is_some_and(|size| size > limit as u64)
            {
                return Err(FetchError::TooLarge);
            }
            let mut body = Vec::with_capacity(
                response.content_length().unwrap_or(0).min(limit as u64) as usize,
            );
            while let Some(chunk) = response.chunk().await.map_err(|_| FetchError::Network)? {
                if body.len().saturating_add(chunk.len()) > limit {
                    return Err(FetchError::TooLarge);
                }
                body.extend_from_slice(&chunk);
            }
            Ok(HttpResponse {
                status,
                body,
                metadata,
            })
        })
    }
}

pub struct SubscriptionFetcher<C> {
    client: C,
}

impl<C: HttpClient> SubscriptionFetcher<C> {
    pub fn new(client: C) -> Self {
        Self { client }
    }

    pub async fn refresh<S: SnapshotSink>(
        &self,
        url: &str,
        store: &S,
    ) -> Result<Arc<Snapshot>, FetchError> {
        if let Ok(response) = self.client.get(url, UA_NATIVE).await
            && response.status_is_success()
        {
            let snapshot = parse_native(&response.body)?
                .with_subscription_metadata(url, &response.metadata, current_unix_time())
                .map_err(|_| FetchError::InvalidBundle)?;
            return store.save(snapshot).map_err(|_| FetchError::Persistence);
        }

        let (mihomo, xray) = tokio::join!(
            self.client.get(url, UA_MIHOMO),
            self.client.get(url, UA_XRAY)
        );
        let mihomo = success_response(mihomo?)?;
        let xray = success_response(xray?)?;
        let snapshot = Snapshot::parse(&mihomo.body, &xray.body)
            .map_err(|_| FetchError::InvalidConfig)?
            .with_merged_subscription_metadata(
                url,
                &mihomo.metadata,
                &xray.metadata,
                current_unix_time(),
            )
            .map_err(|_| FetchError::InvalidConfig)?;
        store.save(snapshot).map_err(|_| FetchError::Persistence)
    }
}

impl HttpResponse {
    pub fn new<N, V>(status: u16, body: Vec<u8>, headers: impl IntoIterator<Item = (N, V)>) -> Self
    where
        N: AsRef<str>,
        V: AsRef<str>,
    {
        Self {
            status,
            body,
            metadata: SubscriptionMetadataHeaders::from_pairs(headers),
        }
    }

    fn status_is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

fn success_response(response: HttpResponse) -> Result<HttpResponse, FetchError> {
    if response.status_is_success() {
        Ok(response)
    } else {
        Err(FetchError::HttpStatus)
    }
}

fn current_unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn parse_native(body: &[u8]) -> Result<Snapshot, FetchError> {
    let (mihomo, xray): (String, Box<RawValue>) =
        serde_json::from_slice(body).map_err(|_| FetchError::InvalidBundle)?;
    Snapshot::parse(mihomo.as_bytes(), xray.get().as_bytes()).map_err(|_| FetchError::InvalidBundle)
}

fn accept_for(user_agent: &str) -> &'static str {
    if user_agent == UA_NATIVE {
        "application/vnd.multicore.bundle+json, application/json"
    } else {
        "application/json, application/yaml, text/yaml, text/plain"
    }
}

fn response_limit(_user_agent: &str) -> usize {
    MAX_CONFIG_BYTES
}
