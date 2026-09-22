//! The error type shared by the library and the binary.
//!
//! Every fallible operation returns [`Result`]. Errors are deliberately
//! *typed* rather than stringly, because the downloader needs to distinguish
//! "retry me" failures (timeouts, HTTP 5xx, HTTP 429) from "never retry me"
//! failures (HTTP 404, a path-traversal URI, malformed JSON).

use std::path::PathBuf;

/// Every way discovery or downloading can fail.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The user supplied a URL that `url` could not parse, or a tileset
    /// referenced a URI that is not a legal URL.
    #[error("invalid URL `{input}`")]
    Url {
        /// The offending input, kept verbatim so the message is actionable.
        input: String,
        /// Parser diagnostic.
        #[source]
        source: url::ParseError,
    },

    /// The entry URL used a scheme this tool cannot fetch.
    #[error("unsupported URL scheme `{scheme}` (expected `http` or `https`)")]
    UnsupportedScheme {
        /// The scheme that was found.
        scheme: String,
    },

    /// The underlying HTTP client could not be constructed.
    #[error("failed to build the HTTP client")]
    ClientBuild {
        /// Reason the client could not be built.
        #[source]
        source: reqwest::Error,
    },

    /// A `--header` value was not a valid HTTP header.
    #[error("invalid header `{header}`: {reason}")]
    InvalidHeader {
        /// The header the user passed on the command line.
        header: String,
        /// Why it was rejected.
        reason: String,
    },

    /// The transport itself failed: DNS, TLS, connect, timeout, body, decode.
    #[error("request to `{url}` failed")]
    Request {
        /// URL that was being fetched.
        url: String,
        /// Transport diagnostic, used to decide whether a retry is worthwhile.
        #[source]
        source: reqwest::Error,
    },

    /// The server answered, but not with a success status.
    #[error("server replied HTTP {status} for `{url}`")]
    HttpStatus {
        /// URL that was being fetched.
        url: String,
        /// The status code, stored as a number so it can be matched on.
        status: u16,
    },

    /// A fetched document was not valid JSON.
    #[error("could not parse JSON from `{url}`")]
    Json {
        /// URL the JSON came from.
        url: String,
        /// Parser diagnostic, including line and column.
        #[source]
        source: serde_json::Error,
    },

    /// A tileset document parsed as JSON but had no `root` tile.
    #[error("tileset `{url}` has no `root` tile (empty or malformed tileset)")]
    MissingRoot {
        /// URL of the offending tileset.
        url: String,
    },

    /// A relative URI tried to escape the output directory, or was otherwise
    /// not representable as a plain relative path.
    #[error(
        "refusing to write `{uri}`: it does not resolve to a safe path inside the output directory"
    )]
    UnsafeUri {
        /// The URI or path that was rejected.
        uri: String,
    },

    /// Two different remote URLs mapped to the same local file.
    #[error("`{first}` and `{second}` both map to `{path}`, so the download would be ambiguous")]
    PathCollision {
        /// Local path that collided.
        path: PathBuf,
        /// URL that claimed the path first.
        first: String,
        /// URL that collided with it.
        second: String,
    },

    /// The implicit tiling extension used a subdivision scheme we do not know.
    #[error("implicit tiling in `{url}` uses unsupported subdivision scheme `{scheme}`")]
    UnsupportedSubdivision {
        /// URL of the tileset that declared the extension.
        url: String,
        /// The scheme that was declared.
        scheme: String,
    },

    /// An availability bitstream or subtree document was structurally invalid.
    #[error("malformed implicit tiling data in `{url}`: {detail}")]
    Availability {
        /// URL of the subtree or tileset that failed to parse.
        url: String,
        /// Human readable explanation.
        detail: String,
    },

    /// Any other implicit-tiling specific failure.
    #[error("implicit tiling error in `{url}`: {detail}")]
    Implicit {
        /// URL of the tileset that declared the extension.
        url: String,
        /// Human readable explanation.
        detail: String,
    },

    /// A tileset asked for an implausible number of files.
    #[error("refusing to continue: more than {limit} files were discovered (the tileset is either malformed or genuinely enormous)")]
    TooManyItems {
        /// The cap that was hit.
        limit: usize,
    },

    /// The worker pool could not be created.
    #[error("failed to create a {threads}-thread worker pool")]
    ThreadPool {
        /// Thread count that was requested.
        threads: usize,
        /// rayon's diagnostic.
        #[source]
        source: rayon::ThreadPoolBuildError,
    },

    /// The run finished, but some files could not be downloaded.
    #[error("{failed} of {total} files failed to download; see the errors above")]
    PartialFailure {
        /// How many files failed.
        failed: usize,
        /// How many files were considered.
        total: usize,
    },

    /// A file could not be read, created, written, or renamed.
    #[error("failed to {action} `{path}`")]
    Io {
        /// What we were trying to do, e.g. `"create directory"`.
        action: &'static str,
        /// Path involved.
        path: PathBuf,
        /// OS diagnostic.
        #[source]
        source: std::io::Error,
    },
}

impl Error {
    /// Build an [`Error::Io`] with the path attached.
    pub fn io(action: &'static str, path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            action,
            path: path.into(),
            source,
        }
    }
}

/// Convenience alias used throughout the crate.
pub type Result<T, E = Error> = core::result::Result<T, E>;

/// Flatten an error and its `source()` chain into a single log line.
///
/// `thiserror`'s `Display` shows only the outermost message, which loses the
/// interesting part (the OS or parser diagnostic). Logging the whole chain
/// keeps the CLI output honest without anyone having to re-run with a
/// backtrace.
pub fn format_chain(err: &Error) -> String {
    use std::error::Error as _;

    let mut rendered = err.to_string();
    let mut cursor = err.source();
    while let Some(cause) = cursor {
        rendered.push_str(": ");
        rendered.push_str(&cause.to_string());
        cursor = cause.source();
    }
    rendered
}
