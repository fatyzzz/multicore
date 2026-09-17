use std::{
    future::Future,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    pin::Pin,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use reqwest::header::{ACCEPT, HeaderMap, HeaderValue, USER_AGENT};
use serde_json::value::RawValue;
use thiserror::Error;

use crate::{
    DeviceIdentity, MAX_CONFIG_BYTES, MAX_SERVICE_LOGO_BYTES, Snapshot, SnapshotSink,
    normalize_service_logo,
};

pub const UA_NATIVE: &str = "multicore-json-massive";
pub const UA_MIHOMO: &str = "multicore-mihomo";
pub const UA_XRAY: &str = "multicore-xray";
pub const UA_SERVICE_LOGO: &str = "multicore-service-logo";
const LOGO_DNS_TIMEOUT: Duration = Duration::from_secs(5);

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
    subscription_client: reqwest::Client,
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
        let subscription_client = reqwest::Client::builder()
            .default_headers(headers)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|_| FetchError::Network)?;
        Ok(Self {
            subscription_client,
        })
    }
}

impl HttpClient for ReqwestHttpClient {
    fn get<'a>(
        &'a self,
        url: &'a str,
        user_agent: &'static str,
    ) -> Pin<Box<dyn Future<Output = Result<HttpResponse, FetchError>> + Send + 'a>> {
        Box::pin(async move {
            if user_agent == UA_SERVICE_LOGO {
                self.get_service_logo(url).await
            } else {
                perform_get(&self.subscription_client, url, user_agent).await
            }
        })
    }
}

impl ReqwestHttpClient {
    async fn get_service_logo(&self, url: &str) -> Result<HttpResponse, FetchError> {
        let parsed = reqwest::Url::parse(url).map_err(|_| FetchError::Network)?;
        let host = parsed.host_str().ok_or(FetchError::Network)?;
        let port = parsed.port_or_known_default().ok_or(FetchError::Network)?;
        let addresses = resolve_global_logo_addresses(host, port).await?;
        let client = build_pinned_logo_client(host, &addresses)?;
        perform_get(&client, url, UA_SERVICE_LOGO).await
    }
}

async fn perform_get(
    client: &reqwest::Client,
    url: &str,
    user_agent: &'static str,
) -> Result<HttpResponse, FetchError> {
    let mut response = request_builder(client, url, user_agent)
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
    let mut body =
        Vec::with_capacity(response.content_length().unwrap_or(0).min(limit as u64) as usize);
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
}

fn request_builder(
    client: &reqwest::Client,
    url: &str,
    user_agent: &'static str,
) -> reqwest::RequestBuilder {
    client
        .get(url)
        .header(USER_AGENT, user_agent)
        .header(ACCEPT, accept_for(user_agent))
}

async fn resolve_global_logo_addresses(
    host: &str,
    port: u16,
) -> Result<Vec<SocketAddr>, FetchError> {
    resolve_global_logo_addresses_with(host, port, LOGO_DNS_TIMEOUT, |host, port| async move {
        tokio::net::lookup_host((host.as_str(), port))
            .await
            .map(|addresses| addresses.collect::<Vec<_>>())
            .map_err(|_| FetchError::Network)
    })
    .await
}

async fn resolve_global_logo_addresses_with<F, Fut>(
    host: &str,
    port: u16,
    timeout: Duration,
    resolver: F,
) -> Result<Vec<SocketAddr>, FetchError>
where
    F: FnOnce(String, u16) -> Fut,
    Fut: Future<Output = Result<Vec<SocketAddr>, FetchError>>,
{
    let mut addresses = if let Ok(ip) = host.parse::<IpAddr>() {
        vec![SocketAddr::new(ip, port)]
    } else {
        tokio::time::timeout(timeout, resolver(host.to_owned(), port))
            .await
            .map_err(|_| FetchError::Network)??
    };
    addresses.sort_unstable();
    addresses.dedup();
    approve_logo_addresses(addresses)
}

fn approve_logo_addresses(addresses: Vec<SocketAddr>) -> Result<Vec<SocketAddr>, FetchError> {
    if addresses.is_empty() || addresses.iter().any(|address| !is_global_ip(address.ip())) {
        return Err(FetchError::Network);
    }
    Ok(addresses)
}

fn build_pinned_logo_client(
    host: &str,
    addresses: &[SocketAddr],
) -> Result<reqwest::Client, FetchError> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .resolve_to_addrs(host, addresses)
        .build()
        .map_err(|_| FetchError::Network)
}

fn is_global_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_global_ipv4(ip),
        IpAddr::V6(ip) => is_global_ipv6(ip),
    }
}

fn is_global_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(a == 0
        || a == 10
        || a == 127
        || (a == 100 && (64..=127).contains(&b))
        || (a == 64 && b == 0 && c == 0)
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 192 && b == 0 && c == 2)
        || (a == 192 && b == 168)
        || (a == 192 && b == 88 && c == 99)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        || a >= 224)
}

fn is_global_ipv6(ip: Ipv6Addr) -> bool {
    if let Some(ipv4) = ip.to_ipv4_mapped() {
        return is_global_ipv4(ipv4);
    }
    let segments = ip.segments();
    (segments[0] & 0xe000) == 0x2000
        && !(segments[0] == 0x2001 && (segments[1] <= 0x01ff || segments[1] == 0x0db8))
        && segments[0] != 0x2002
        && !(segments[0] == 0x3fff && segments[1] < 0x1000)
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
            let mut snapshot = parse_native(&response.body)?
                .with_subscription_metadata(url, &response.metadata, current_unix_time())
                .map_err(|_| FetchError::InvalidBundle)?;
            self.fetch_optional_logo(&mut snapshot).await;
            return store.save(snapshot).map_err(|_| FetchError::Persistence);
        }

        let (mihomo, xray) = tokio::join!(
            self.client.get(url, UA_MIHOMO),
            self.client.get(url, UA_XRAY)
        );
        let mihomo = success_response(mihomo?)?;
        let xray = success_response(xray?)?;
        let mut snapshot = Snapshot::parse(&mihomo.body, &xray.body)
            .map_err(|_| FetchError::InvalidConfig)?
            .with_merged_subscription_metadata(
                url,
                &mihomo.metadata,
                &xray.metadata,
                current_unix_time(),
            )
            .map_err(|_| FetchError::InvalidConfig)?;
        self.fetch_optional_logo(&mut snapshot).await;
        store.save(snapshot).map_err(|_| FetchError::Persistence)
    }

    async fn fetch_optional_logo(&self, snapshot: &mut Snapshot) {
        let Some(url) = snapshot.take_subscription_logo_url() else {
            return;
        };
        let Ok(response) = self.client.get(&url, UA_SERVICE_LOGO).await else {
            return;
        };
        if response.status_is_success()
            && let Some(png) = normalize_service_logo(&response.body)
        {
            snapshot.set_service_logo_png(png);
        }
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
    if user_agent == UA_SERVICE_LOGO {
        "image/png, image/jpeg, image/webp, image/svg+xml"
    } else if user_agent == UA_NATIVE {
        "application/vnd.multicore.bundle+json, application/json"
    } else {
        "application/json, application/yaml, text/yaml, text/plain"
    }
}

fn response_limit(user_agent: &str) -> usize {
    if user_agent == UA_SERVICE_LOGO {
        MAX_SERVICE_LOGO_BYTES
    } else {
        MAX_CONFIG_BYTES
    }
}

#[cfg(test)]
mod logo_network_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn logo_address_policy_rejects_non_global_and_mixed_answers() {
        for address in [
            "0.0.0.0",
            "10.0.0.1",
            "100.64.0.1",
            "64.0.0.1",
            "127.0.0.1",
            "169.254.1.1",
            "172.16.0.1",
            "192.168.0.1",
            "192.0.2.1",
            "192.88.99.1",
            "198.18.0.1",
            "224.0.0.1",
            "::",
            "::1",
            "fc00::1",
            "fe80::1",
            "ff02::1",
            "2001:db8::1",
            "2001:2::1",
            "2002::1",
            "3fff::1",
        ] {
            let ip = address.parse().unwrap();
            assert!(!is_global_ip(ip), "accepted {address}");
        }
        for address in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(is_global_ip(address.parse().unwrap()), "rejected {address}");
        }
        assert_eq!(
            approve_logo_addresses(vec![
                "1.1.1.1:443".parse().unwrap(),
                "127.0.0.1:443".parse().unwrap(),
            ]),
            Err(FetchError::Network)
        );
    }

    #[tokio::test]
    async fn logo_dns_resolution_has_an_independent_bounded_timeout() {
        let started = std::time::Instant::now();
        let result = resolve_global_logo_addresses_with(
            "pending.example",
            443,
            Duration::from_millis(1),
            |_, _| std::future::pending(),
        )
        .await;
        assert_eq!(result, Err(FetchError::Network));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn pinned_logo_client_connects_only_to_supplied_address_without_identity_headers() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0_u8; 4096];
            let length = stream.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..length]).to_ascii_lowercase();
            assert!(request.contains("host: public-logo.example"));
            assert!(request.contains("user-agent: multicore-service-logo"));
            for header in ["x-hwid:", "x-device-os:", "x-ver-os:", "x-device-model:"] {
                assert!(!request.contains(header));
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
        });
        let client = build_pinned_logo_client("public-logo.example", &[address]).unwrap();
        let response = perform_get(
            &client,
            &format!("http://public-logo.example:{}/logo", address.port()),
            UA_SERVICE_LOGO,
        )
        .await
        .unwrap();
        assert_eq!(response.status, 200);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn pinned_logo_transport_caps_declared_and_streamed_responses() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request).await.unwrap();
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        MAX_SERVICE_LOGO_BYTES + 1
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });
        let client = build_pinned_logo_client("public-logo.example", &[address]).unwrap();
        assert_eq!(
            perform_get(
                &client,
                &format!("http://public-logo.example:{}/logo", address.port()),
                UA_SERVICE_LOGO,
            )
            .await,
            Err(FetchError::TooLarge)
        );
        server.await.unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request).await.unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            let chunk = [b'x'; 64 * 1024];
            for _ in 0..=(MAX_SERVICE_LOGO_BYTES / chunk.len()) {
                if stream.write_all(&chunk).await.is_err() {
                    break;
                }
            }
        });
        let client = build_pinned_logo_client("public-logo.example", &[address]).unwrap();
        assert_eq!(
            perform_get(
                &client,
                &format!("http://public-logo.example:{}/logo", address.port()),
                UA_SERVICE_LOGO,
            )
            .await,
            Err(FetchError::TooLarge)
        );
        server.await.unwrap();
    }
}
