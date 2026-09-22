//! Phase two: fetch every discovered item in parallel.
//!
//! Two properties matter more than speed here:
//!
//! * **Resumability.** A file is written to `<name>.part` and only renamed into
//!   place once it is complete, so an interrupted run never leaves a truncated
//!   tile behind. Files that already exist are skipped unless `--overwrite`.
//! * **Partial failure tolerance.** A single 404 in a 20 000-tile dataset must
//!   not throw away the other 19 999 downloads. Failures are collected into the
//!   [`Summary`] and reported at the end, and the process exits non-zero.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use rayon::ThreadPool;

use crate::discovery::DownloadItem;
use crate::error::{Error, Result};
use crate::net::Fetcher;
use crate::par;
use crate::path_util::join_within;

/// Template for the file-count bar. Exposed so a test can prove it parses.
pub const FILE_BAR_TEMPLATE: &str =
    "{spinner:.green} [{elapsed_precise}] [{bar:35.cyan/blue}] {pos:>5}/{len:<5} {percent:>3}% {per_sec:>11} ETA {eta:>5}  {msg}";

/// Template for the running byte counter.
pub const BYTE_BAR_TEMPLATE: &str = "{spinner:.dim} {bytes:>10} downloaded";

/// How wide a path shown next to the bar may be before it is elided.
const MAX_MESSAGE_CHARS: usize = 60;

/// Knobs for [`download_all`].
#[derive(Debug, Clone)]
pub struct DownloadOptions {
    /// Replace files that already exist instead of skipping them.
    pub overwrite: bool,
    /// Draw progress bars.
    pub progress: bool,
}

impl Default for DownloadOptions {
    fn default() -> Self {
        Self {
            overwrite: false,
            progress: true,
        }
    }
}

/// What happened to one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileOutcome {
    /// Fetched and written.
    Downloaded {
        /// Number of bytes written.
        bytes: u64,
    },
    /// Already on disk and `--overwrite` was not given.
    Skipped,
}

/// A file that could not be downloaded.
#[derive(Debug)]
pub struct Failure {
    /// URL that failed.
    pub url: String,
    /// Local path it would have been written to.
    pub path: PathBuf,
    /// Why it failed.
    pub error: Error,
}

/// Aggregate result of a download run.
#[derive(Debug, Default)]
pub struct Summary {
    /// Files fetched in this run.
    pub downloaded: usize,
    /// Files left alone because they already existed.
    pub skipped: usize,
    /// Bytes fetched in this run.
    pub bytes: u64,
    /// Files that failed, in the order the items were listed.
    pub failures: Vec<Failure>,
    /// Wall-clock time spent downloading.
    pub duration: Duration,
}

impl Summary {
    /// Whether every item either downloaded or was skipped.
    pub fn is_success(&self) -> bool {
        self.failures.is_empty()
    }

    /// Items considered in this run.
    pub fn total(&self) -> usize {
        self.downloaded + self.skipped + self.failures.len()
    }
}

/// Download `items` into `out_dir` using rayon's global thread pool.
pub fn download_all(
    fetcher: &Fetcher,
    items: &[DownloadItem],
    out_dir: &Path,
    options: &DownloadOptions,
) -> Result<Summary> {
    download_all_with(fetcher, items, out_dir, options, None)
}

/// Download `items` into `out_dir`, optionally on an explicit thread pool.
pub fn download_all_with(
    fetcher: &Fetcher,
    items: &[DownloadItem],
    out_dir: &Path,
    options: &DownloadOptions,
    pool: Option<&ThreadPool>,
) -> Result<Summary> {
    let started = Instant::now();
    fs::create_dir_all(out_dir).map_err(|source| Error::io("create directory", out_dir, source))?;

    let bars = Bars::new(items.len(), options.progress);
    let overwrite = options.overwrite;

    let results: Vec<Result<FileOutcome>> = par::map(pool, items, |item| {
        let outcome = fetch_one(fetcher, item, out_dir, overwrite);
        bars.record(item, &outcome);
        outcome
    });

    let mut summary = Summary {
        duration: started.elapsed(),
        ..Summary::default()
    };

    for (item, result) in items.iter().zip(results) {
        match result {
            Ok(FileOutcome::Downloaded { bytes }) => {
                summary.downloaded += 1;
                summary.bytes += bytes;
                tracing::debug!(
                    url = %item.url,
                    path = %item.relative_path.display(),
                    bytes,
                    "downloaded"
                );
            }
            Ok(FileOutcome::Skipped) => {
                summary.skipped += 1;
                tracing::debug!(path = %item.relative_path.display(), "already present, skipped");
            }
            Err(error) => {
                tracing::warn!(
                    url = %item.url,
                    path = %item.relative_path.display(),
                    error = %error,
                    "download failed"
                );
                summary.failures.push(Failure {
                    url: item.url.to_string(),
                    path: item.relative_path.clone(),
                    error,
                });
            }
        }
    }

    bars.finish();
    Ok(summary)
}

/// Fetch one item and put it on disk, atomically.
fn fetch_one(
    fetcher: &Fetcher,
    item: &DownloadItem,
    out_dir: &Path,
    overwrite: bool,
) -> Result<FileOutcome> {
    let target = join_within(out_dir, &item.relative_path)?;

    if target.exists() && !overwrite {
        return Ok(FileOutcome::Skipped);
    }

    let body = fetcher.get_bytes(&item.url)?;
    write_atomic(&target, &body)?;
    Ok(FileOutcome::Downloaded {
        bytes: body.len() as u64,
    })
}

/// Write `body` to `target` via a `.part` sibling.
///
/// The rename is the commit point: readers either see the old file or the
/// complete new one, never a half-written tile. `fs::rename` will not overwrite
/// on Windows, so an existing file is removed first.
fn write_atomic(target: &Path, body: &[u8]) -> Result<()> {
    let parent = target.parent().ok_or_else(|| Error::UnsafeUri {
        uri: target.display().to_string(),
    })?;
    fs::create_dir_all(parent).map_err(|source| Error::io("create directory", parent, source))?;

    let temp = temp_path(target);
    fs::write(&temp, body).map_err(|source| Error::io("write", &temp, source))?;

    if target.exists() {
        fs::remove_file(target).map_err(|source| Error::io("replace", target, source))?;
    }
    if let Err(source) = fs::rename(&temp, target) {
        let _ = fs::remove_file(&temp);
        return Err(Error::io("rename into place", target, source));
    }

    Ok(())
}

/// `<name>.b3dm` becomes `<name>.b3dm.part`.
fn temp_path(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    target.with_file_name(name)
}

/// The two progress bars, or a pair of no-ops when progress is off.
struct Bars {
    multi: Option<MultiProgress>,
    files: ProgressBar,
    bytes: ProgressBar,
}

impl Bars {
    fn new(total: usize, enabled: bool) -> Self {
        if !enabled {
            return Self {
                multi: None,
                files: ProgressBar::hidden(),
                bytes: ProgressBar::hidden(),
            };
        }

        let multi = MultiProgress::with_draw_target(ProgressDrawTarget::stdout());

        let files = multi.add(ProgressBar::new(total as u64));
        files.set_style(style(FILE_BAR_TEMPLATE));
        files.set_message("starting");

        let bytes = multi.add(ProgressBar::new_spinner());
        bytes.set_style(style(BYTE_BAR_TEMPLATE));
        bytes.enable_steady_tick(Duration::from_millis(200));

        Self {
            multi: Some(multi),
            files,
            bytes,
        }
    }

    /// Advance the bars, whatever the outcome was. A failed file still counts as
    /// processed, otherwise the bar would never reach 100%.
    fn record(&self, item: &DownloadItem, outcome: &Result<FileOutcome>) {
        if let Ok(FileOutcome::Downloaded { bytes }) = outcome {
            self.bytes.inc(*bytes);
        }
        if let Ok(FileOutcome::Skipped) = outcome {
            self.files
                .set_message(format!("{} (cached)", short(&item.relative_path)));
        } else {
            self.files.set_message(short(&item.relative_path));
        }
        self.files.inc(1);
    }

    fn finish(&self) {
        self.bytes.finish_and_clear();
        self.files.finish_and_clear();
        if let Some(multi) = &self.multi {
            let _ = multi.clear();
        }
    }
}

/// Build a style, degrading to the default rather than panicking on a bad
/// template.
fn style(template: &str) -> ProgressStyle {
    ProgressStyle::with_template(template)
        .unwrap_or_else(|error| {
            tracing::debug!(%error, "falling back to the default progress style");
            ProgressStyle::default_bar()
        })
        .progress_chars("=>-")
}

/// Keep the path next to the bar from pushing the bar itself off the line.
fn short(path: &Path) -> String {
    let text = path.display().to_string();
    let length = text.chars().count();
    if length <= MAX_MESSAGE_CHARS {
        return text;
    }
    let tail: String = text.chars().skip(length - MAX_MESSAGE_CHARS + 1).collect();
    format!("…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_are_valid() {
        assert!(
            ProgressStyle::with_template(FILE_BAR_TEMPLATE).is_ok(),
            "file bar template must parse: {FILE_BAR_TEMPLATE}"
        );
        assert!(
            ProgressStyle::with_template(BYTE_BAR_TEMPLATE).is_ok(),
            "byte bar template must parse: {BYTE_BAR_TEMPLATE}"
        );
    }

    #[test]
    fn part_files_sit_next_to_their_target() {
        let temp = temp_path(Path::new("out/a/0.b3dm"));
        assert_eq!(temp, PathBuf::from("out").join("a").join("0.b3dm.part"));
    }

    #[test]
    fn long_paths_are_elided_not_truncated_at_the_wrong_end() {
        let path = PathBuf::from("a/".repeat(80) + "leaf.b3dm");
        let shortened = short(&path);
        assert!(shortened.chars().count() <= MAX_MESSAGE_CHARS);
        assert!(shortened.ends_with("leaf.b3dm"), "got {shortened}");
    }

    #[test]
    fn short_paths_are_left_alone() {
        assert_eq!(short(Path::new("a/b.b3dm")), "a/b.b3dm");
    }
}
