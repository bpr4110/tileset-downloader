//! HTTP access: one connection-pooling client and one retry policy.
//!
//! Everything network-facing goes through [`Fetcher`]. Because the download
//! loop is `rayon`-parallel and synchronous, this uses
//! [`reqwest::blocking::Client`]: a `Client` owns an internal connection pool
//! and is `Send + Sync`, so a single instance is shared by reference across
//! every worker thread instead of each thread paying for its own TLS setup.

use std::fs::File;
use std::io::{self, Write};
use std::path::Path;
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
        self.with_retry(url, || self.try_get(url))
    }

    /// Stream a URL straight into `path`, retrying transient failures.
    ///
    /// Unlike [`Fetcher::get_bytes`], the body is never held in memory: it is
    /// copied to the file in chunks, so peak memory per download is one copy
    /// buffer whatever the tile's size. `path` is truncated before every attempt,
    /// so a retry after a mid-body failure starts over rather than appending to a
    /// partial file. Returns the number of bytes written.
    ///
    /// The parent directory must already exist.
    pub fn get_to_file(&self, url: &Url, path: &Path) -> Result<u64> {
        self.with_retry(url, || self.try_get_to_file(url, path))
    }

    /// Run `op`, retrying transient failures with exponential backoff.
    fn with_retry<T>(&self, url: &Url, mut op: impl FnMut() -> Result<T>) -> Result<T> {
        let mut attempt = 0u32;
        loop {
            match op() {
                Ok(value) => return Ok(value),
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

    fn try_get_to_file(&self, url: &Url, path: &Path) -> Result<u64> {
        let mut response =
            self.client
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

        // Truncating create: a retry replaces the previous attempt rather than
        // appending to it.
        let file = File::create(path).map_err(|source| Error::io("create", path, source))?;
        let mut writer = RecordingWriter::new(file);

        match response.copy_to(&mut writer) {
            Ok(bytes) => {
                writer
                    .flush()
                    .map_err(|source| Error::io("flush", path, source))?;
                Ok(bytes)
            }
            // A failure on the write side is local — a full disk, a revoked
            // permission. Retrying it cannot help, and the OS diagnostic is the
            // actionable one, so it becomes an `Io` error rather than a request
            // error that `is_transient` would retry.
            Err(_) if writer.error.is_some() => {
                let error = writer.error.take().expect("checked by the guard");
                Err(Error::io("write", path, error))
            }
            Err(source) => Err(Error::Request {
                url: url.to_string(),
                source,
            }),
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

/// A `Write` wrapper that remembers the first write error.
///
/// [`reqwest::blocking::Response::copy_to`] reports read-side (network) and
/// write-side (disk) failures as the same `reqwest::Error`. reqwest returns its
/// own network errors unchanged, but a disk failure arrives as a generic
/// decoding error — indistinguishable, afterwards, from a corrupt body, and
/// therefore wrongly retried. Catching it here keeps it a local [`Error::Io`].
struct RecordingWriter<W: Write> {
    inner: W,
    error: Option<io::Error>,
}

impl<W: Write> RecordingWriter<W> {
    fn new(inner: W) -> Self {
        Self { inner, error: None }
    }
}

impl<W: Write> Write for RecordingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.inner.write(buf) {
            Ok(written) => Ok(written),
            Err(err) => {
                self.error = Some(err);
                // A placeholder: `copy_to` only needs to know the copy stopped.
                // The real error is handed back by `try_get_to_file`.
                Err(io::Error::other("disk write failed"))
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
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

    #[test]
    fn recording_writer_passes_bytes_through() {
        let mut writer = RecordingWriter::new(Vec::new());
        writer.write_all(b"tile bytes").expect("write succeeds");
        writer.flush().expect("flush succeeds");
        assert_eq!(writer.inner, b"tile bytes");
        assert!(writer.error.is_none(), "a clean write records no error");
    }

    #[test]
    fn recording_writer_remembers_a_disk_failure() {
        let mut writer = RecordingWriter::new(FailingWriter);
        assert!(
            writer.write(b"anything").is_err(),
            "the placeholder must stop the copy"
        );

        let error = writer.error.take().expect("the real error is kept");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(error.to_string(), "no space left on device");
    }

    /// A `Write` that always fails, so the disk-failure path can be exercised
    /// without touching a filesystem.
    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "no space left on device",
            ))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}
