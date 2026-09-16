use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use reqwest::header::{ACCEPT, USER_AGENT};
use serde_json::value::RawValue;
use thiserror::Error;

use crate::{MAX_CONFIG_BYTES, Snapshot, SnapshotSink};

pub const UA_NATIVE: &str = "multicore-json-massive";
pub const UA_MIHOMO: &str = "multicore-mihomo";
pub const UA_XRAY: &str = "multicore-xray";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
    pub subscription_userinfo: Option<String>,
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
    pub fn new() -> Result<Self, FetchError> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|_| FetchError::Network)?;
        Ok(Self { client })
    }
}

impl Default for ReqwestHttpClient {
    fn default() -> Self {
        Self::new().expect("static HTTP client configuration must be valid")
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
            let subscription_userinfo = response
                .headers()
                .get("subscription-userinfo")
                .and_then(|value| value.to_str().ok())
                .filter(|value| value.len() <= 1024 && value.is_ascii())
                .map(ToOwned::to_owned);
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
                subscription_userinfo,
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
                .with_subscription_source(
                    url,
                    response.subscription_userinfo.as_deref(),
                    current_unix_time(),
                )
                .map_err(|_| FetchError::InvalidBundle)?;
            return store.save(snapshot).map_err(|_| FetchError::Persistence);
        }

        let (mihomo, xray) = tokio::join!(
            self.client.get(url, UA_MIHOMO),
            self.client.get(url, UA_XRAY)
        );
        let mihomo = success_response(mihomo?)?;
        let xray = success_response(xray?)?;
        let userinfo = mihomo
            .subscription_userinfo
            .as_deref()
            .or(xray.subscription_userinfo.as_deref());
        let snapshot = Snapshot::parse(&mihomo.body, &xray.body)
            .map_err(|_| FetchError::InvalidConfig)?
            .with_subscription_source(url, userinfo, current_unix_time())
            .map_err(|_| FetchError::InvalidConfig)?;
        store.save(snapshot).map_err(|_| FetchError::Persistence)
    }
}

impl HttpResponse {
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
