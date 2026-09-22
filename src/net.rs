//! HTTP access: one connection-pooling client and one retry policy.
//!
//! Everything network-facing goes through [`Fetcher`]. Because the download
//! loop is `rayon`-parallel and synchronous, this uses
//! [`reqwest::blocking::Client`]: a `Client` owns an internal connection pool
//! and is `Send + Sync`, so a single instance is shared by reference across
//! every worker thread instead of each thread paying for its own TLS setup.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::blocking::Client;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use url::Url;

use crate::error::{Error, Result};

/// Sent unless the user overrides it. Being identifiable is basic etiquette
/// when a single run can issue thousands of requests at a host.
pub const DEFAULT_USER_AGENT: &str =
    concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

/// Everything needed to build the HTTP client and its retry policy.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// `User-Agent` header.
    pub user_agent: String,
    /// Per-request timeout. Use [`Duration::ZERO`] to disable it.
    pub timeout: Duration,
    /// TCP/TLS connect timeout. Use [`Duration::ZERO`] to disable it.
    pub connect_timeout: Duration,
    /// How many times a transient failure is retried before giving up.
    pub retries: u32,
    /// Extra headers, as raw `Name: Value` strings from `--header`.
    pub headers: Vec<String>,
    /// How many idle connections to keep per host.
    pub max_idle_connections_per_host: usize,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            user_agent: DEFAULT_USER_AGENT.to_owned(),
            timeout: Duration::from_secs(60),
            connect_timeout: Duration::from_secs(30),
            retries: 3,
            headers: Vec::new(),
            max_idle_connections_per_host: 32,
        }
    }
}

/// A pooled HTTP client plus the retry policy applied to every request.
#[derive(Debug, Clone)]
pub struct Fetcher {
    client: Client,
    retries: u32,
}

impl Fetcher {
    /// Build a fetcher from the given configuration.
    pub fn new(config: &ClientConfig) -> Result<Self> {
        Ok(Self {
            client: build_client(config)?,
            retries: config.retries,
        })
    }

    /// The underlying client, for callers that need to send their own requests.
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Fetch a URL, retrying transient failures with exponential backoff.
    pub fn get_bytes(&self, url: &Url) -> Result<Vec<u8>> {
        let mut attempt = 0u32;
        loop {
            match self.try_get(url) {
                Ok(bytes) => return Ok(bytes),
                Err(err) if attempt < self.retries && is_transient(&err) => {
                    let delay = backoff(attempt);
                    tracing::warn!(
                        url = %url,
                        attempt = attempt + 1,
                        retries = self.retries,
                        delay_ms = delay.as_millis() as u64,
                        error = %err,
                        "transient failure, retrying"
                    );
                    std::thread::sleep(delay);
                    attempt += 1;
                }
                Err(err) => return Err(err),
            }
        }
    }

    fn try_get(&self, url: &Url) -> Result<Vec<u8>> {
        let response = self
            .client
            .get(url.clone())
            .send()
            .map_err(|source| Error::Request {
                url: url.to_string(),
                source,
            })?;

        let status = response.status();
        if !status.is_success() {
            return Err(Error::HttpStatus {
                url: url.to_string(),
                status: status.as_u16(),
            });
        }

        response
            .bytes()
            .map(|body| body.to_vec())
            .map_err(|source| Error::Request {
                url: url.to_string(),
                source,
            })
    }
}

/// Assemble the client. Proxies are taken from the standard `HTTP_PROXY` /
/// `HTTPS_PROXY` / `NO_PROXY` environment variables, as reqwest does by default.
fn build_client(config: &ClientConfig) -> Result<Client> {
    let mut builder = Client::builder()
        .user_agent(config.user_agent.clone())
        .pool_max_idle_per_host(config.max_idle_connections_per_host)
        .redirect(reqwest::redirect::Policy::limited(10))
        .default_headers(parse_headers(&config.headers)?);

    if !config.timeout.is_zero() {
        builder = builder.timeout(config.timeout);
    }
    if !config.connect_timeout.is_zero() {
        builder = builder.connect_timeout(config.connect_timeout);
    }

    builder
        .build()
        .map_err(|source| Error::ClientBuild { source })
}

/// Parse `Name: Value` strings into a [`HeaderMap`].
fn parse_headers(raw: &[String]) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    for entry in raw {
        let Some((name, value)) = entry.split_once(':') else {
            return Err(Error::InvalidHeader {
                header: entry.clone(),
                reason: "expected `Name: Value`".to_owned(),
            });
        };

        let name = name.trim();
        let name = HeaderName::try_from(name).map_err(|err| Error::InvalidHeader {
            header: entry.clone(),
            reason: err.to_string(),
        })?;
        let value = value.trim();
        let value = HeaderValue::try_from(value).map_err(|err| Error::InvalidHeader {
            header: entry.clone(),
            reason: err.to_string(),
        })?;
        headers.append(name, value);
    }
    Ok(headers)
}

/// Transient failures are worth retrying; everything else is not.
///
/// A 404 will never heal, and retrying a path-traversal rejection is nonsense.
/// But a 503, a 429 (rate limit), a dropped connection, or a timeout very often
/// does heal — and at thousands of tiles, one unlucky request must not sink the
/// whole run.
fn is_transient(err: &Error) -> bool {
    match err {
        Error::HttpStatus { status, .. } => {
            matches!(*status, 408 | 425 | 429 | 500..=599)
        }
        Error::Request { source, .. } => {
            source.is_timeout()
                || source.is_connect()
                || source.is_body()
                || source.is_decode()
                || source.is_request()
        }
        _ => false,
    }
}

/// Exponential backoff with a jitter derived from the wall clock.
///
/// No `rand` dependency is needed: the sub-second part of the current time is
/// enough to desynchronise retries that were triggered in parallel.
fn backoff(attempt: u32) -> Duration {
    let base_ms = 250u64.saturating_mul(1u64 << attempt.min(5));
    let capped_ms = base_ms.min(8_000);
    let jitter_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::from(elapsed.subsec_millis()))
        .unwrap_or(0)
        % 250;
    Duration::from_millis(capped_ms + jitter_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_headers() {
        let headers = parse_headers(&["Authorization: Bearer abc".to_owned()]).expect("parses");
        assert_eq!(headers.get("authorization").unwrap(), "Bearer abc");
    }

    #[test]
    fn rejects_malformed_headers() {
        let err = parse_headers(&["nope".to_owned()]).expect_err("must reject");
        assert!(matches!(err, Error::InvalidHeader { .. }));
    }

    #[test]
    fn classifies_transient_status_codes() {
        for status in [408u16, 425, 429, 500, 503, 599] {
            assert!(
                is_transient(&Error::HttpStatus {
                    url: "https://example.com/x".to_owned(),
                    status
                }),
                "HTTP {status} should be retried"
            );
        }
        for status in [400u16, 401, 403, 404, 410, 422] {
            assert!(
                !is_transient(&Error::HttpStatus {
                    url: "https://example.com/x".to_owned(),
                    status
                }),
                "HTTP {status} should not be retried"
            );
        }
    }

    #[test]
    fn never_retries_local_errors() {
        assert!(!is_transient(&Error::UnsafeUri {
            uri: "../x".to_owned()
        }));
        assert!(!is_transient(&Error::MissingRoot {
            url: "https://example.com/tileset.json".to_owned()
        }));
    }

    #[test]
    fn backoff_grows_then_caps() {
        let first = backoff(0).as_millis();
        let second = backoff(1).as_millis();
        assert!(second > first, "backoff should grow");
        assert!(backoff(30).as_millis() <= 8_000 + 250, "backoff should cap");
    }
}
