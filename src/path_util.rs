//! Mapping between remote URLs and local, safe file paths.
//!
//! A 3D Tiles tileset is a *tree of relative URIs*. Reproducing that tree on
//! disk means trusting a remote document to name local files, which is exactly
//! the shape of a path-traversal bug. Everything in this module therefore
//! refuses rather than sanitises whenever a URI cannot be represented as a
//! plain nested file.
//!
//! A second, subtler rule: URIs are **never percent-decoded**. The local file
//! name is derived from the encoded form, so `%2E%2E%2F` stays the literal
//! five-character-ish name `%2E%2E%2F` on disk instead of becoming `../`. The
//! fetched URL keeps using the encoded form, so the two stay in sync.
//!
//! Local paths are **relative to the folder that holds the entry
//! `tileset.json`**: a URL with the same origin whose path starts with that
//! folder keeps its in-base remainder, so the entry document and the files
//! beside it stay shallow. Anything else — a different host or port, or a path
//! that climbs above the tileset folder — is bucketed under
//! `_external/<host>[/<port>]/<full-url-path>`. The host is sanitised like a
//! file name, so even an IPv6 address becomes a safe directory name; it is
//! never fed to the segment validator, where its `:` would be rejected as an
//! unsafe segment.

use std::path::{Component, Path, PathBuf};

use url::Url;

use crate::error::{Error, Result};

/// Names Windows refuses to use for files, case-insensitively and regardless of
/// extension. They are prefixed with `_` rather than rejected so a tileset that
/// happens to contain e.g. `aux.b3dm` still downloads.
const RESERVED_WINDOWS_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// The bucket for files that live outside the entry tileset's own folder.
pub const EXTERNAL_DIR: &str = "_external";

/// Resolve a possibly relative URI from a tileset against its base URL.
///
/// A leading `/` means "host root", `../` climbs one level, and anything that
/// parses as an absolute URL wins. This is plain RFC 3986 reference resolution,
/// delegated to [`Url::join`] so we do not reimplement it badly.
pub fn resolve(base: &Url, reference: &str) -> Result<Url> {
    base.join(reference).map_err(|source| Error::Url {
        input: reference.to_owned(),
        source,
    })
}

/// Accept either a direct `tileset.json` URL or a directory URL.
///
/// Users reasonably paste `https://host/tiles/` (the folder they browsed to) as
/// well as `https://host/tiles/tileset.json`. Directory-looking inputs get
/// `tileset.json` appended; the query string of a direct `.json` URL is kept
/// untouched so signed URLs keep working.
pub fn normalize_entry_url(input: &str) -> Result<Url> {
    let mut url = Url::parse(input).map_err(|source| Error::Url {
        input: input.to_owned(),
        source,
    })?;

    match url.scheme() {
        "http" | "https" => {}
        other => {
            return Err(Error::UnsupportedScheme {
                scheme: other.to_owned(),
            })
        }
    }

    let path = url.path().to_owned();
    if !path.to_ascii_lowercase().ends_with(".json") {
        // Force a trailing slash first: `Url::join` drops the last segment when
        // the base does not end in `/`, which would turn `/tiles` into
        // `/tileset.json` instead of `/tiles/tileset.json`.
        if !path.ends_with('/') {
            url.set_path(&format!("{path}/"));
        }
        url = url.join("tileset.json").map_err(|source| Error::Url {
            input: input.to_owned(),
            source,
        })?;
    }

    Ok(url)
}

/// The directory that holds the last path segment of `url`: everything up to
/// and including the final `/`. Queries and fragments are dropped, so the
/// result is usable as `relative_output_path`'s `base`.
///
/// `/a/b/tileset.json` -> `/a/b/`; `/tileset.json` -> `/`; a path containing
/// no `/` at all becomes `/`.
pub fn base_dir(url: &Url) -> Url {
    let mut base = url.clone();
    let path = base.path().to_owned();
    let directory = match path.rfind('/') {
        Some(slash) => &path[..=slash],
        None => "/",
    };
    base.set_path(directory);
    base.set_query(None);
    base.set_fragment(None);
    base
}

/// Turn a resolved content URL into the path (relative to the output directory)
/// that it should be written to.
///
/// The local path mirrors the URL *relative to the folder that holds the entry
/// `tileset.json`*: when `url` shares `base`'s origin and its path starts with
/// `base`'s (which always ends in `/`, so this is a segment boundary), the
/// in-base remainder is kept — the entry document itself is therefore shallow.
/// Anything outside that folder is bucketed under
/// `_external/<host>[/<port>]/<full-url-path>`, where a non-default port is
/// its own segment.
///
/// Segments are rejected — not rewritten — when they are `..`, contain a
/// backslash, or contain a colon (`C:` drive prefix, NTFS alternate data
/// stream). Empty segments and `.` are elided, which also collapses duplicate
/// slashes.
pub fn relative_output_path(url: &Url, base: &Url) -> Result<PathBuf> {
    let same_origin = url.scheme() == base.scheme()
        && url.host_str() == base.host_str()
        && url.port_or_known_default() == base.port_or_known_default();

    if same_origin && url.path().starts_with(base.path()) {
        let remainder = &url.path()[base.path().len()..];
        let mapped = map_segments(remainder, url)?;
        if mapped.as_os_str().is_empty() {
            // A URL equal to the base directory is a folder, not a file.
            return Err(Error::UnsafeUri {
                uri: url.as_str().to_owned(),
            });
        }
        return Ok(mapped);
    }

    let mut external = PathBuf::from(EXTERNAL_DIR);
    external.push(sanitize_host(url.host_str().unwrap_or_default()));
    if let Some(port) = url.port() {
        external.push(port.to_string());
    }
    external.push(map_segments(url.path(), url)?);
    Ok(external)
}

/// Map the path segments of `path` into a relative path, validating each one.
///
/// This is the validation core shared by the in-base and `_external` branches:
/// empty segments and `.` are elided, `..` and any segment containing `\` or
/// `:` are rejected, and everything else is escaped with [`sanitize_segment`].
/// `url` is used only for error messages.
fn map_segments(path: &str, url: &Url) -> Result<PathBuf> {
    let mut relative = PathBuf::new();

    for segment in path.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".." || segment.contains('\\') || segment.contains(':') {
            return Err(Error::UnsafeUri {
                uri: url.as_str().to_owned(),
            });
        }
        relative.push(sanitize_segment(segment));
    }

    Ok(relative)
}

/// Join a validated relative path onto the output root.
///
/// The relative path is re-validated with [`Component`], so even a path that
/// arrives from somewhere other than [`relative_output_path`] cannot escape.
pub fn join_within(root: &Path, relative: &Path) -> Result<PathBuf> {
    if relative.as_os_str().is_empty() {
        return Err(Error::UnsafeUri {
            uri: relative.display().to_string(),
        });
    }
    for component in relative.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err(Error::UnsafeUri {
                uri: relative.display().to_string(),
            });
        }
    }
    Ok(root.join(relative))
}

/// Escape the handful of names Windows treats specially.
fn sanitize_segment(segment: &str) -> String {
    let stem = segment
        .split('.')
        .next()
        .unwrap_or(segment)
        .to_ascii_uppercase();
    let needs_escape = RESERVED_WINDOWS_NAMES.contains(&stem.as_str())
        || segment.ends_with(' ')
        || segment.ends_with('.');
    if needs_escape {
        format!("_{segment}")
    } else {
        segment.to_owned()
    }
}

/// Turn a URL host into a safe single directory name for the `_external`
/// bucket. ASCII alphanumerics, `-`, and `.` survive; every other character —
/// including the `:` and `[`/`]` of an IPv6 host — becomes `_`, and an empty
/// result becomes `"_"`. The result is then escaped like any file name, so a
/// reserved Windows device name cannot appear as a host directory.
fn sanitize_host(host: &str) -> String {
    let sanitized: String = host
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let sanitized = if sanitized.is_empty() {
        "_".to_owned()
    } else {
        sanitized
    };
    sanitize_segment(&sanitized)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(input: &str) -> Url {
        Url::parse(input).expect("test URL should parse")
    }

    #[test]
    fn appends_tileset_json_to_a_directory_url() {
        for (input, expected) in [
            (
                "https://example.com/tiles/",
                "https://example.com/tiles/tileset.json",
            ),
            (
                "https://example.com/tiles",
                "https://example.com/tiles/tileset.json",
            ),
            ("https://example.com", "https://example.com/tileset.json"),
            ("https://example.com/", "https://example.com/tileset.json"),
        ] {
            let normalized = normalize_entry_url(input).expect("should normalize");
            assert_eq!(
                normalized.as_str(),
                expected,
                "{input} should point at a tileset document"
            );
        }
    }

    #[test]
    fn keeps_a_direct_tileset_url_and_its_query() {
        let normalized = normalize_entry_url("https://example.com/a/tileset.json?token=abc")
            .expect("should normalize");
        assert_eq!(
            normalized.as_str(),
            "https://example.com/a/tileset.json?token=abc"
        );
    }

    #[test]
    fn rejects_non_http_schemes() {
        let err = normalize_entry_url("file:///tmp/tileset.json").expect_err("must reject file://");
        assert!(matches!(err, Error::UnsupportedScheme { .. }));
    }

    #[test]
    fn maps_a_content_url_to_a_nested_relative_path() {
        let base = url("https://example.com/");
        let path =
            relative_output_path(&url("https://example.com/a/1/0/3.b3dm"), &base).expect("maps");
        assert_eq!(path, PathBuf::from("a").join("1").join("0").join("3.b3dm"));
    }

    #[test]
    fn drops_query_and_fragment_from_the_local_name() {
        let base = url("https://example.com/");
        let path = relative_output_path(&url("https://example.com/a/tileset.json?v=2#top"), &base)
            .expect("maps");
        assert_eq!(path, PathBuf::from("a").join("tileset.json"));
    }

    #[test]
    fn a_traversing_url_is_normalised_inside_the_output_root() {
        // `Url` removes `..` segments while parsing, so a tileset cannot make us
        // write outside the output directory just by writing `../` in a URI. The
        // `..` check in `relative_output_path` is defence in depth for paths that
        // arrive from anywhere else.
        let base = url("https://example.com/");
        let path = relative_output_path(&url("https://example.com/../a/../../etc/passwd"), &base)
            .expect("maps");
        assert_eq!(path, PathBuf::from("etc").join("passwd"));
        assert!(
            path.components().all(|c| matches!(c, Component::Normal(_))),
            "the local path must stay inside the output root: {path:?}"
        );
    }

    #[test]
    fn encoded_dot_segments_are_neutralised_by_url_normalisation() {
        // The URL standard counts `%2E%2E` as a double-dot path segment, so the
        // parser removes it — and the segment before it — before we ever see the
        // path. Nothing percent-encoded can smuggle a `..` past this function.
        let base = url("https://example.com/");
        let path =
            relative_output_path(&url("https://example.com/a/%2E%2E/b"), &base).expect("maps");
        assert_eq!(path, PathBuf::from("b"));
    }

    #[test]
    fn backslashes_are_normalised_to_separators() {
        // For http(s) URLs the parser folds `\` into `/`, so a backslash can
        // never survive into a file name.
        let base = url("https://example.com/");
        let path =
            relative_output_path(&url("https://example.com/a\\b.b3dm"), &base).expect("maps");
        assert_eq!(path, PathBuf::from("a").join("b.b3dm"));
    }

    #[test]
    fn refuses_a_colon_in_a_segment() {
        // `a:b` is a legal URL path but not a legal Windows file name (`C:`,
        // NTFS alternate data streams), so it is rejected rather than rewritten.
        let base = url("https://example.com/");
        assert!(relative_output_path(&url("https://example.com/a:b"), &base).is_err());
    }

    #[test]
    fn percent_encoding_is_never_decoded() {
        let base = url("https://example.com/");
        let path = relative_output_path(&url("https://example.com/my%2E%2E%2Fsecret.b3dm"), &base)
            .expect("maps");
        assert_eq!(
            path,
            PathBuf::from("my%2E%2E%2Fsecret.b3dm"),
            "decoding here would be the traversal bug"
        );
    }

    #[test]
    fn escapes_reserved_windows_names() {
        let base = url("https://example.com/");
        let path =
            relative_output_path(&url("https://example.com/a/aux.b3dm"), &base).expect("maps");
        assert_eq!(path, PathBuf::from("a").join("_aux.b3dm"));
    }

    #[test]
    fn nested_entry_maps_everything_below_the_tileset_folder() {
        let base = url("https://example.com/a/b/");
        let path = relative_output_path(&url("https://example.com/a/b/tiles/0.b3dm"), &base)
            .expect("maps");
        assert_eq!(path, PathBuf::from("tiles").join("0.b3dm"));
        assert_all_normal(&path);
    }

    #[test]
    fn the_entry_document_itself_is_shallow() {
        let base = url("https://example.com/a/b/");
        let path = relative_output_path(&url("https://example.com/a/b/tileset.json"), &base)
            .expect("maps");
        assert_eq!(path, PathBuf::from("tileset.json"));
        assert_all_normal(&path);
    }

    #[test]
    fn a_host_root_reference_under_the_tileset_folder_stays_in_base() {
        let base = url("https://example.com/a/b/");
        let path =
            relative_output_path(&url("https://example.com/a/b/deep/x.b3dm"), &base).expect("maps");
        assert_eq!(path, PathBuf::from("deep").join("x.b3dm"));
        assert_all_normal(&path);
    }

    #[test]
    fn climbing_above_the_tileset_folder_buckets_the_file_externally() {
        let base = url("https://example.com/a/b/");
        let path =
            relative_output_path(&url("https://example.com/a/shared/x.png"), &base).expect("maps");
        assert_eq!(
            path,
            PathBuf::from("_external")
                .join("example.com")
                .join("a")
                .join("shared")
                .join("x.png")
        );
        assert_all_normal(&path);
    }

    #[test]
    fn a_different_host_is_bucketed_externally() {
        let base = url("https://example.com/a/");
        let path =
            relative_output_path(&url("https://cdn.example.org/t/y.b3dm"), &base).expect("maps");
        assert_eq!(
            path,
            PathBuf::from("_external")
                .join("cdn.example.org")
                .join("t")
                .join("y.b3dm")
        );
        assert_all_normal(&path);
    }

    #[test]
    fn a_non_default_port_is_its_own_external_segment() {
        let base = url("https://example.com/a/");
        let path = relative_output_path(&url("https://cdn.example.org:8443/t/y.b3dm"), &base)
            .expect("maps");
        assert_eq!(
            path,
            PathBuf::from("_external")
                .join("cdn.example.org")
                .join("8443")
                .join("t")
                .join("y.b3dm")
        );
        assert_all_normal(&path);
    }

    #[test]
    fn a_default_port_is_not_emitted() {
        let base = url("https://example.com/a/");
        let path =
            relative_output_path(&url("http://example.com:80/t/y.b3dm"), &base).expect("maps");
        assert_eq!(
            path,
            PathBuf::from("_external")
                .join("example.com")
                .join("t")
                .join("y.b3dm"),
            "a different scheme is external, and the default port must not appear"
        );
        assert_all_normal(&path);
    }

    #[test]
    fn an_external_url_with_a_colon_segment_is_still_rejected() {
        let base = url("https://example.com/a/");
        assert!(
            relative_output_path(&url("https://other.example/x:y.b3dm"), &base).is_err(),
            "a colon in a path segment is never a safe file name"
        );
    }

    #[test]
    fn base_dir_keeps_everything_up_to_the_last_slash() {
        assert_eq!(
            base_dir(&url("https://example.com/a/b/tileset.json")).path(),
            "/a/b/"
        );
        assert_eq!(
            base_dir(&url("https://example.com/tileset.json")).path(),
            "/"
        );
        // Queries and fragments belong to the document, not the folder.
        let base = base_dir(&url("https://example.com/a/b/tileset.json?v=2#top"));
        assert_eq!(base.path(), "/a/b/");
        assert!(base.query().is_none());
        assert!(base.fragment().is_none());
    }

    fn assert_all_normal(path: &PathBuf) {
        assert!(
            path.components().all(|c| matches!(c, Component::Normal(_))),
            "the local path must stay inside the output root: {path:?}"
        );
    }

    #[test]
    fn join_within_rejects_absolute_and_parent_paths() {
        let root = Path::new("out");
        assert!(join_within(root, Path::new("a/b.b3dm")).is_ok());
        assert!(join_within(root, Path::new("../escape.b3dm")).is_err());
        assert!(join_within(root, Path::new("")).is_err());
    }

    #[test]
    fn resolution_follows_rfc3986() {
        let base = url("https://example.com/a/b/tileset.json");
        assert_eq!(
            resolve(&base, "tiles/0.b3dm").expect("joins").as_str(),
            "https://example.com/a/b/tiles/0.b3dm"
        );
        assert_eq!(
            resolve(&base, "../c/1.b3dm").expect("joins").as_str(),
            "https://example.com/a/c/1.b3dm"
        );
        assert_eq!(
            resolve(&base, "/root/2.b3dm").expect("joins").as_str(),
            "https://example.com/root/2.b3dm"
        );
        assert_eq!(
            resolve(&base, "https://cdn.example.org/3.b3dm")
                .expect("joins")
                .as_str(),
            "https://cdn.example.org/3.b3dm"
        );
    }
}
