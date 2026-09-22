//! Mirror a complete [3D Tiles](https://github.com/CesiumGS/3d-tiles) tileset
//! from a URL to a local directory.
//!
//! The crate is split into two phases that the binary runs back to back, and
//! that are exposed separately here so they can be tested (and reused)
//! independently:
//!
//! 1. **Discovery** — [`discover`] fetches the root `tileset.json`, walks the
//!    tile tree, follows external tilesets, expands
//!    `3DTILES_implicit_tiling` subtrees, and produces the complete list of
//!    [`DownloadItem`]s (URL plus the local path it should be written to).
//! 2. **Download** — [`download_all`] fetches every item through a `rayon`
//!    thread pool, writes files atomically, drives a progress bar, and returns
//!    a [`Summary`] of what succeeded, what was skipped, and what failed.
//!
//! Both phases share a single [`Fetcher`], which owns a connection-pooling
//! [`reqwest::blocking::Client`] and the retry policy.
//!
//! ```no_run
//! use std::path::Path;
//! use tileset_downloader::{download_all, discover, ClientConfig, DownloadOptions, Fetcher};
//!
//! # fn main() -> tileset_downloader::Result<()> {
//! let fetcher = Fetcher::new(&ClientConfig::default())?;
//! let url = tileset_downloader::path_util::normalize_entry_url("https://example.com/tileset.json")?;
//! let discovery = discover(&fetcher, &url)?;
//! let summary = download_all(
//!     &fetcher,
//!     &discovery.items,
//!     Path::new("out"),
//!     &DownloadOptions::default(),
//! )?;
//! println!("downloaded {} files", summary.downloaded);
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]

pub mod cli;
pub mod discovery;
pub mod download;
pub mod error;
pub mod implicit;
pub mod net;
pub mod path_util;
pub mod tileset;

pub use crate::discovery::{discover, discover_with, Discovery, DownloadItem, ItemKind};
pub use crate::download::{download_all, download_all_with, DownloadOptions, Summary};
pub use crate::error::{format_chain, Error, Result};
pub use crate::implicit::{Coordinates, ImplicitTiling, SubdivisionScheme, SubtreeExpansion};
pub use crate::net::{ClientConfig, Fetcher};
pub use crate::tileset::Tileset;

/// The parallel-map helper shared by the discovery and download phases.
///
/// Both phases run the same shape of work — "apply an I/O-bound closure to every
/// element of a slice" — and both accept an optional explicit thread pool. This
/// keeps that decision in exactly one place.
pub(crate) mod par {
    use rayon::prelude::*;
    use rayon::ThreadPool;

    /// Map `f` over `items`, on `pool` when one was configured, otherwise on
    /// rayon's global pool.
    ///
    /// `collect()` preserves input order, so callers can zip the results back
    /// against the input slice without tracking indices.
    pub(crate) fn map<T, U>(
        pool: Option<&ThreadPool>,
        items: &[T],
        f: impl Fn(&T) -> U + Sync + Send,
    ) -> Vec<U>
    where
        T: Sync,
        U: Send,
    {
        match pool {
            Some(pool) => pool.install(|| items.par_iter().map(f).collect()),
            None => items.par_iter().map(f).collect(),
        }
    }
}
