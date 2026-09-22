//! Phase one: work out every file a tileset needs.
//!
//! Discovery is two crawls that run back to back.
//!
//! **Tileset documents.** A breadth-first walk of *tileset* documents: the entry
//! `tileset.json`, plus every `content.uri` that ends in `.json` (an external
//! tileset). Each round fetches and parses the whole frontier in parallel; a
//! `visited` set makes reference cycles harmless.
//!
//! **Implicit tiling.** An implicitly tiled tile does not list children; it
//! points at subtree files that hold availability bitstreams. Those are walked
//! in their own breadth-first crawl — expand a subtree, collect the content
//! URLs it marks available, queue the child subtrees it marks present — one
//! parallel round per subtree level. `availableLevels` bounds the descent, so a
//! malformed bitstream cannot spin forever.
//!
//! Discovery is **fail-fast**: a broken external tileset or a subtree that
//! cannot be decoded means the dataset cannot be described, so there is nothing
//! useful to download. The download phase, by contrast, tolerates individual
//! failures.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use indicatif::{ProgressBar, ProgressStyle};
use rayon::ThreadPool;
use url::Url;

use crate::error::{Error, Result};
use crate::implicit::{Coordinates, ImplicitTiling, SubtreeExpansion};
use crate::net::Fetcher;
use crate::par;
use crate::path_util::{base_dir, relative_output_path, resolve};
use crate::tileset::{ReferenceKind, Scan, Tileset};

/// Hard ceiling on the size of a download plan.
///
/// A constant-`1` `childSubtreeAvailability` with a large `availableLevels`
/// describes an astronomically large tree. That is either a corrupt document or
/// a deliberate way to make a client exhaust memory, so the crawl stops with a
/// clear error instead of trying.
pub const MAX_ITEMS: usize = 5_000_000;

/// What a discovered file is, for logging and categorising.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    /// A `tileset.json` document, including the entry document.
    Tileset,
    /// An implicit tiling `.subtree` document.
    Subtree,
    /// A buffer referenced by a subtree's `buffers[].uri`.
    Buffer,
    /// Tile payload: `b3dm`, `i3dm`, `pnts`, `cmpt`, `glb`, ...
    Content,
}

impl ItemKind {
    /// Short human-readable name, for `--dry-run` output.
    pub fn label(self) -> &'static str {
        match self {
            Self::Tileset => "tileset",
            Self::Subtree => "subtree",
            Self::Buffer => "buffer",
            Self::Content => "content",
        }
    }
}

/// One file to fetch, and where it belongs locally.
#[derive(Debug, Clone)]
pub struct DownloadItem {
    /// Absolute URL to fetch.
    pub url: Url,
    /// Path relative to the output directory, always a safe nested path.
    pub relative_path: PathBuf,
    /// What kind of file this is.
    pub kind: ItemKind,
}

/// The complete plan for mirroring a tileset.
#[derive(Debug, Clone)]
pub struct Discovery {
    /// The normalised entry URL the crawl started from.
    pub entry: Url,
    /// Every file, sorted by local path.
    pub items: Vec<DownloadItem>,
    /// How many external tileset documents were found.
    pub external_tilesets: usize,
    /// How many tiles carried the implicit tiling extension.
    pub implicit_tilesets: usize,
    /// How many subtree documents were found.
    pub subtree_files: usize,
    /// How many subtree buffers were found.
    pub buffer_files: usize,
}

impl Discovery {
    /// Number of files in the plan.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the plan is empty.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// Knobs for discovery.
#[derive(Debug, Clone)]
pub struct DiscoveryOptions {
    /// Draw a spinner while crawling.
    pub progress: bool,
}

impl Default for DiscoveryOptions {
    fn default() -> Self {
        Self { progress: true }
    }
}

/// Discover everything reachable from `entry` using rayon's global pool.
pub fn discover(fetcher: &Fetcher, entry: &Url) -> Result<Discovery> {
    discover_with(fetcher, entry, &DiscoveryOptions::default(), None)
}

/// Discover everything reachable from `entry`, optionally on an explicit pool.
pub fn discover_with(
    fetcher: &Fetcher,
    entry: &Url,
    options: &DiscoveryOptions,
    pool: Option<&ThreadPool>,
) -> Result<Discovery> {
    let spinner = Spinner::new(options.progress);
    let base = base_dir(entry);
    let mut collector = Collector::new(base);

    // The entry document is part of the tileset too, so it is both fetched and
    // saved. Without this the mirror would be unusable offline.
    collector.add(entry.clone(), ItemKind::Tileset)?;

    let mut discovery = Discovery {
        entry: entry.clone(),
        items: Vec::new(),
        external_tilesets: 0,
        implicit_tilesets: 0,
        subtree_files: 0,
        buffer_files: 0,
    };
    let mut tilings: Vec<ImplicitTiling> = Vec::new();

    // ---- Crawl 1: tileset documents --------------------------------------
    let mut visited: HashSet<String> = HashSet::from([entry.as_str().to_owned()]);
    let mut frontier: Vec<Url> = vec![entry.clone()];

    while !frontier.is_empty() {
        let batch = std::mem::take(&mut frontier);
        let scans: Vec<Result<Scan>> = par::map(pool, &batch, |url| fetch_scan(fetcher, url));

        for (document_url, scan) in batch.iter().zip(scans) {
            let scan = scan?;

            for reference in &scan.references {
                // Every URI is relative to the document that named it.
                let resolved = resolve(document_url, &reference.uri)?;
                match reference.kind {
                    ReferenceKind::Tileset => {
                        discovery.external_tilesets += 1;
                        collector.add(resolved.clone(), ItemKind::Tileset)?;
                        if visited.insert(resolved.as_str().to_owned()) {
                            frontier.push(resolved);
                        }
                    }
                    ReferenceKind::Content => collector.add(resolved, ItemKind::Content)?,
                }
            }

            for source in &scan.implicits {
                tilings.push(ImplicitTiling::parse(document_url, source)?);
                discovery.implicit_tilesets += 1;
            }
        }

        spinner.update(&collector, frontier.len());
    }

    // ---- Crawl 2: implicit tiling subtrees --------------------------------
    expand_implicit(
        fetcher,
        &tilings,
        &mut collector,
        &mut discovery,
        pool,
        &spinner,
    )?;

    spinner.finish();

    // Deterministic output: the same tileset always produces the same plan.
    collector
        .items
        .sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    discovery.items = collector.items;
    Ok(discovery)
}

/// Walk the subtree graph of every implicit tileset, one level per round.
fn expand_implicit(
    fetcher: &Fetcher,
    tilings: &[ImplicitTiling],
    collector: &mut Collector,
    discovery: &mut Discovery,
    pool: Option<&ThreadPool>,
    spinner: &Spinner,
) -> Result<()> {
    if tilings.is_empty() {
        return Ok(());
    }

    let mut queue: Vec<SubtreeJob> = Vec::new();
    for (index, tiling) in tilings.iter().enumerate() {
        let coordinates = tiling.root();
        queue.push(SubtreeJob {
            tiling: index,
            coordinates,
            url: tiling.subtree_url(coordinates)?,
        });
    }

    while !queue.is_empty() {
        let batch = std::mem::take(&mut queue);

        for job in &batch {
            collector.add(job.url.clone(), ItemKind::Subtree)?;
            discovery.subtree_files += 1;
        }

        let expansions: Vec<Result<SubtreeExpansion>> = par::map(pool, &batch, |job| {
            let tiling = &tilings[job.tiling];
            let bytes = fetcher.get_bytes(&job.url)?;
            tiling.expand_subtree(fetcher, job.coordinates, &job.url, &bytes)
        });

        for (job, expansion) in batch.iter().zip(expansions) {
            let expansion = expansion?;

            for url in expansion.external_buffers {
                collector.add(url, ItemKind::Buffer)?;
                discovery.buffer_files += 1;
            }
            for url in expansion.content_urls {
                collector.add(url, ItemKind::Content)?;
            }
            // Children carry the URL they were reached by, so a subtree that
            // overrides the template keeps working.
            for (coordinates, url) in expansion.child_subtrees {
                queue.push(SubtreeJob {
                    tiling: job.tiling,
                    coordinates,
                    url,
                });
            }
        }

        spinner.update(collector, queue.len());
    }

    Ok(())
}

/// Fetch and parse one tileset document.
fn fetch_scan(fetcher: &Fetcher, url: &Url) -> Result<Scan> {
    let bytes = fetcher.get_bytes(url)?;
    let tileset = Tileset::from_slice(&bytes).map_err(|source| Error::Json {
        url: url.to_string(),
        source,
    })?;

    if tileset.root.is_none() {
        return Err(Error::MissingRoot {
            url: url.to_string(),
        });
    }

    tracing::debug!(
        url = %url,
        version = tileset.asset.version.as_deref().unwrap_or("unknown"),
        generator = tileset.asset.generator.as_deref().unwrap_or("unknown"),
        "parsed tileset"
    );

    Ok(tileset.scan())
}

/// One subtree to fetch: which implicit tileset it belongs to and where it is.
#[derive(Debug)]
struct SubtreeJob {
    /// Index into the caller's `tilings` slice.
    tiling: usize,
    /// Global coordinates of the subtree root.
    coordinates: Coordinates,
    /// Where to fetch it, already resolved.
    url: Url,
}

/// Accumulates download items, rejecting duplicates and path collisions.
#[derive(Debug)]
struct Collector {
    /// The directory holding the entry `tileset.json`; the local tree mirrors
    /// the URL relative to this.
    base: Url,
    items: Vec<DownloadItem>,
    seen_urls: HashSet<String>,
    path_owner: HashMap<PathBuf, String>,
}

impl Collector {
    /// A collector that maps every URL relative to `base` (see
    /// [`crate::path_util::relative_output_path`]).
    fn new(base: Url) -> Self {
        Self {
            base,
            items: Vec::new(),
            seen_urls: HashSet::new(),
            path_owner: HashMap::new(),
        }
    }

    /// Record a URL once. URLs already seen are ignored; two *different* URLs
    /// that would land on the same file are an error, because silently letting
    /// one overwrite the other would corrupt the mirror.
    fn add(&mut self, url: Url, kind: ItemKind) -> Result<()> {
        let key = url.as_str().to_owned();
        if !self.seen_urls.insert(key.clone()) {
            return Ok(());
        }
        if self.items.len() >= MAX_ITEMS {
            return Err(Error::TooManyItems { limit: MAX_ITEMS });
        }

        let relative_path = relative_output_path(&url, &self.base)?;
        if let Some(first) = self.path_owner.get(&relative_path) {
            if first != &key {
                return Err(Error::PathCollision {
                    path: relative_path,
                    first: first.clone(),
                    second: key,
                });
            }
        }
        self.path_owner.insert(relative_path.clone(), key);

        self.items.push(DownloadItem {
            url,
            relative_path,
            kind,
        });
        Ok(())
    }
}

/// The discovery spinner, or a no-op.
struct Spinner {
    bar: ProgressBar,
}

impl Spinner {
    fn new(enabled: bool) -> Self {
        if !enabled {
            return Self {
                bar: ProgressBar::hidden(),
            };
        }
        let bar = ProgressBar::new_spinner();
        bar.set_style(
            ProgressStyle::with_template("{spinner:.green} discovering… {msg}")
                .unwrap_or_else(|_| ProgressStyle::default_spinner()),
        );
        bar.enable_steady_tick(Duration::from_millis(200));
        Self { bar }
    }

    fn update(&self, collector: &Collector, queued: usize) {
        self.bar.set_message(format!(
            "{} files found, {queued} pending",
            collector.items.len()
        ));
    }

    fn finish(&self) {
        self.bar.finish_and_clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(input: &str) -> Url {
        Url::parse(input).expect("test URL")
    }

    #[test]
    fn deduplicates_by_url() {
        let mut collector = Collector::new(url("https://e.com/"));
        collector
            .add(url("https://e.com/a/0.b3dm"), ItemKind::Content)
            .expect("first add");
        collector
            .add(url("https://e.com/a/0.b3dm"), ItemKind::Content)
            .expect("duplicate add");
        assert_eq!(collector.items.len(), 1);
    }

    #[test]
    fn rejects_two_urls_that_share_a_local_path() {
        // `aux.b3dm` escapes to `_aux.b3dm`, which is exactly what the literal
        // `_aux.b3dm` maps to — a genuine collision on the same host.
        let mut collector = Collector::new(url("https://e.com/"));
        collector
            .add(url("https://e.com/aux.b3dm"), ItemKind::Content)
            .expect("first add");
        let err = collector
            .add(url("https://e.com/_aux.b3dm"), ItemKind::Content)
            .expect_err("collision must be reported");
        assert!(matches!(err, Error::PathCollision { .. }));
    }

    #[test]
    fn different_hosts_land_in_distinct_external_buckets() {
        let mut collector = Collector::new(url("https://e.com/"));
        collector
            .add(url("https://a.example/x/0.b3dm"), ItemKind::Content)
            .expect("first host");
        collector
            .add(url("https://b.example/x/0.b3dm"), ItemKind::Content)
            .expect("second host must not collide");
        assert_eq!(collector.items.len(), 2);
        assert_eq!(
            collector.items[0].relative_path,
            PathBuf::from("_external")
                .join("a.example")
                .join("x")
                .join("0.b3dm")
        );
        assert_eq!(
            collector.items[1].relative_path,
            PathBuf::from("_external")
                .join("b.example")
                .join("x")
                .join("0.b3dm")
        );
    }

    #[test]
    fn a_traversing_url_still_lands_inside_the_output_root() {
        // URLs cannot climb out of the host root: the parser removes `..`. What
        // matters is that the resulting path is a plain nested path under the
        // output directory, never an absolute path or a parent reference.
        let mut collector = Collector::new(url("https://e.com/"));
        collector
            .add(url("https://e.com/../../etc/passwd"), ItemKind::Content)
            .expect("add");
        let relative = &collector.items[0].relative_path;
        assert_eq!(relative, &PathBuf::from("etc").join("passwd"));
        assert!(relative
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_))));
    }

    #[test]
    fn records_item_kinds() {
        let mut collector = Collector::new(url("https://e.com/"));
        collector
            .add(url("https://e.com/tileset.json"), ItemKind::Tileset)
            .expect("add");
        collector
            .add(url("https://e.com/s/0_0.subtree"), ItemKind::Subtree)
            .expect("add");
        assert_eq!(collector.items[0].kind, ItemKind::Tileset);
        assert_eq!(collector.items[1].kind, ItemKind::Subtree);
    }
}
