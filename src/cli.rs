//! Command line interface definition.

use std::path::PathBuf;

use clap::Parser;

/// Kept short; `--help` prints it verbatim.
pub const ABOUT: &str = "Download a complete 3D Tiles tileset from a URL into a local directory.";

/// Rendered after the option list by `--help`.
pub const EXAMPLES: &str = "\
Examples:
  # tileset.json URL, output into ./mytiles
  tileset-downloader https://example.com/tileset.json ./mytiles

  # a directory URL works too: tileset.json is appended for you
  tileset-downloader https://example.com/3dtiles/ ./mytiles

  # 32 parallel fetches, 8 retries, signed-url style auth header
  tileset-downloader https://api.example.com/tileset.json out \\
      -j 32 --retries 8 -H 'Authorization: Bearer <token>'

  # see what would be downloaded without downloading it
  tileset-downloader https://example.com/tileset.json out --dry-run";

/// Parsed command line arguments.
#[derive(Debug, Parser)]
#[command(
    name = "tileset-downloader",
    version,
    about = ABOUT,
    after_help = EXAMPLES,
    max_term_width = 100
)]
pub struct Cli {
    /// Root tileset URL: either a `tileset.json` or the directory holding it.
    #[arg(value_name = "URL")]
    pub url: String,

    /// Directory to write the tileset into. Created if missing.
    #[arg(value_name = "DIR")]
    pub out: PathBuf,

    /// Number of files to fetch in parallel. Defaults to the logical CPU count.
    #[arg(short = 'j', long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
    pub concurrency: Option<u32>,

    /// Extra request header, e.g. `-H 'Authorization: Bearer xyz'`. Repeatable.
    #[arg(short = 'H', long = "header", value_name = "NAME: VALUE")]
    pub headers: Vec<String>,

    /// Re-download files that already exist in DIR.
    #[arg(long)]
    pub overwrite: bool,

    /// Discover everything, print the plan, and exit without downloading.
    #[arg(long)]
    pub dry_run: bool,

    /// Hide progress bars. Also implied by --quiet.
    #[arg(long)]
    pub no_progress: bool,

    /// Per-request timeout in seconds. 0 disables the timeout.
    #[arg(long, value_name = "SECONDS", default_value_t = 60)]
    pub timeout: u64,

    /// Connection timeout in seconds. 0 disables the timeout.
    #[arg(long, value_name = "SECONDS", default_value_t = 30)]
    pub connect_timeout: u64,

    /// Retries per request for transient failures (timeouts, HTTP 429/5xx).
    #[arg(long, value_name = "N", default_value_t = 3)]
    pub retries: u32,

    /// Override the User-Agent header.
    #[arg(long, value_name = "STRING")]
    pub user_agent: Option<String>,

    /// Increase verbosity: `-v` for debug logs, `-vv` for trace logs.
    #[arg(short = 'v', long, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Only log warnings and errors.
    #[arg(short = 'q', long)]
    pub quiet: bool,
}

impl Cli {
    /// Effective number of parallel downloads.
    pub fn concurrency(&self) -> usize {
        self.concurrency
            .map(|value| value as usize)
            .unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map(usize::from)
                    .unwrap_or(4)
            })
    }

    /// Whether progress bars should be drawn at all.
    pub fn progress_enabled(&self) -> bool {
        !self.no_progress && !self.quiet
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_minimum_invocation() {
        let cli = Cli::parse_from(["tileset-downloader", "https://e.com/tileset.json", "out"]);
        assert_eq!(cli.url, "https://e.com/tileset.json");
        assert_eq!(cli.out, PathBuf::from("out"));
        assert!(!cli.overwrite);
        assert_eq!(cli.retries, 3);
        assert!(cli.concurrency() >= 1);
        assert!(cli.progress_enabled());
    }

    #[test]
    fn rejects_zero_concurrency() {
        assert!(Cli::try_parse_from([
            "tileset-downloader",
            "https://e.com/tileset.json",
            "out",
            "-j",
            "0"
        ])
        .is_err());
    }

    #[test]
    fn quiet_disables_progress() {
        let cli = Cli::parse_from([
            "tileset-downloader",
            "https://e.com/tileset.json",
            "out",
            "--quiet",
        ]);
        assert!(!cli.progress_enabled());
    }

    #[test]
    fn accepts_repeated_headers() {
        let cli = Cli::parse_from([
            "tileset-downloader",
            "https://e.com/tileset.json",
            "out",
            "-H",
            "A: 1",
            "-H",
            "B: 2",
        ]);
        assert_eq!(cli.headers, vec!["A: 1".to_owned(), "B: 2".to_owned()]);
    }
}
