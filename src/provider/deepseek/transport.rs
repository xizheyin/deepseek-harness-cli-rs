//! Replaceable HTTP byte-stream boundary and the real reqwest transport.

use std::{collections::BTreeMap, fmt};

use futures_util::{FutureExt, StreamExt, future::BoxFuture, stream::BoxStream};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

pub(super) type ByteStream = BoxStream<'static, Result<Vec<u8>, TransportError>>;

pub(super) trait HttpTransport: Send + Sync {
    fn send(
        &self,
        request: HttpRequest,
        cancellation: CancellationToken,
    ) -> BoxFuture<'_, Result<HttpResponse, TransportError>>;
}

pub(super) struct HttpRequest {
    url: String,
    headers: BTreeMap<String, HttpHeaderValue>,
    body: Vec<u8>,
}

impl HttpRequest {
    pub(super) fn new(url: String, body: Vec<u8>) -> Self {
        Self {
            url,
            headers: BTreeMap::new(),
            body,
        }
    }

    pub(super) fn insert_header(
        &mut self,
        name: impl Into<String>,
        value: impl Into<String>,
        sensitive: bool,
    ) {
        self.headers.insert(
            name.into().to_ascii_lowercase(),
            HttpHeaderValue {
                value: value.into(),
                sensitive,
            },
        );
    }

    #[cfg(test)]
    pub(super) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(|value| value.value.as_str())
    }

    #[cfg(test)]
    pub(super) fn body(&self) -> &[u8] {
        &self.body
    }
}

impl fmt::Debug for HttpRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let headers = self
            .headers
            .iter()
            .map(|(name, value)| {
                (
                    name,
                    if value.sensitive {
                        "[REDACTED]"
                    } else {
                        value.value.as_str()
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        formatter
            .debug_struct("HttpRequest")
            .field("url", &self.url)
            .field("headers", &headers)
            .field("body_bytes", &self.body.len())
            .finish()
    }
}

struct HttpHeaderValue {
    value: String,
    sensitive: bool,
}

pub(super) struct HttpResponse {
    status: u16,
    headers: BTreeMap<String, String>,
    body: Option<ByteStream>,
}

impl HttpResponse {
    pub(super) fn new(
        status: u16,
        headers: BTreeMap<String, String>,
        body: Option<ByteStream>,
    ) -> Self {
        Self {
            status,
            headers,
            body,
        }
    }

    pub(super) fn status(&self) -> u16 {
        self.status
    }

    pub(super) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }

    pub(super) fn take_body(&mut self) -> Option<ByteStream> {
        self.body.take()
    }
}

impl fmt::Debug for HttpResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpResponse")
            .field("status", &self.status)
            .field("header_names", &self.headers.keys().collect::<Vec<_>>())
            .field("has_body", &self.body.is_some())
            .finish()
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("HTTP transport failed during {phase}")]
pub(super) struct TransportError {
    phase: &'static str,
}

impl TransportError {
    pub(super) fn new(phase: &'static str) -> Self {
        Self { phase }
    }
}

pub(super) struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    pub(super) fn new() -> Result<Self, DeepSeekProviderBuildError> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .build()
            .map_err(|_| DeepSeekProviderBuildError)?;
        Ok(Self { client })
    }
}

impl HttpTransport for ReqwestTransport {
    fn send(
        &self,
        request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> BoxFuture<'_, Result<HttpResponse, TransportError>> {
        async move {
            let mut builder = self.client.post(&request.url);
            for (name, value) in request.headers {
                builder = builder.header(name, value.value);
            }
            let response = builder
                .body(request.body)
                .send()
                .await
                .map_err(|_| TransportError::new("request send"))?;
            let status = response.status().as_u16();
            let headers = response
                .headers()
                .iter()
                .filter_map(|(name, value)| {
                    value
                        .to_str()
                        .ok()
                        .map(|value| (name.as_str().to_ascii_lowercase(), value.to_owned()))
                })
                .collect();
            let body = response
                .bytes_stream()
                .map(|item| {
                    item.map(|bytes| bytes.to_vec())
                        .map_err(|_| TransportError::new("response body"))
                })
                .boxed();
            Ok(HttpResponse::new(status, headers, Some(body)))
        }
        .boxed()
    }
}

/// The reusable HTTPS client could not be initialized.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("failed to initialize the DeepSeek HTTPS client")]
pub struct DeepSeekProviderBuildError;
