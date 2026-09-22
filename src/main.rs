//! The `tileset-downloader` command line binary.
//!
//! Structure: parse arguments, set up logging, build the shared HTTP client and
//! the rayon pool, run discovery, then run the downloads. Logs go to **stderr**
//! and progress bars to **stdout**, so the two never fight over the same
//! terminal line and stdout stays clean enough to pipe.

use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use rayon::ThreadPoolBuilder;
use tracing_subscriber::EnvFilter;

use tileset_downloader::cli::Cli;
use tileset_downloader::discovery::DiscoveryOptions;
use tileset_downloader::download::{download_all_with, DownloadOptions, Summary};
use tileset_downloader::error::{format_chain, Error, Result};
use tileset_downloader::net::{ClientConfig, Fetcher};
use tileset_downloader::{discover_with, path_util};

/// How many individual failures to spell out before summarising the rest.
const MAX_REPORTED_FAILURES: usize = 10;

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging(&cli);

    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!("{}", format_chain(&error));
            ExitCode::FAILURE
        }
    }
}

fn run(cli: &Cli) -> Result<()> {
    let entry = path_util::normalize_entry_url(&cli.url)?;
    let concurrency = cli.concurrency();

    let defaults = ClientConfig::default();
    let config = ClientConfig {
        user_agent: cli.user_agent.clone().unwrap_or(defaults.user_agent),
        timeout: Duration::from_secs(cli.timeout),
        connect_timeout: Duration::from_secs(cli.connect_timeout),
        retries: cli.retries,
        headers: cli.headers.clone(),
        max_idle_connections_per_host: concurrency.max(defaults.max_idle_connections_per_host),
    };
    let fetcher = Fetcher::new(&config)?;

    let pool = ThreadPoolBuilder::new()
        .num_threads(concurrency)
        .thread_name(|index| format!("fetch-{index}"))
        .build()
        .map_err(|source| Error::ThreadPool {
            threads: concurrency,
            source,
        })?;

    tracing::info!(
        url = %entry,
        output = %cli.out.display(),
        concurrency,
        retries = cli.retries,
        "starting"
    );

    let progress = cli.progress_enabled();
    let discovery = discover_with(
        &fetcher,
        &entry,
        &DiscoveryOptions { progress },
        Some(&pool),
    )?;

    tracing::info!(
        files = discovery.len(),
        external_tilesets = discovery.external_tilesets,
        implicit_tilesets = discovery.implicit_tilesets,
        subtrees = discovery.subtree_files,
        buffers = discovery.buffer_files,
        "discovery finished"
    );

    if discovery.is_empty() {
        tracing::warn!("the tileset references no downloadable files");
    }

    if cli.dry_run {
        // The plan *is* the output here, so it goes to stdout.
        for item in &discovery.items {
            println!("{:<8} {}", item.kind.label(), item.url);
        }
        tracing::info!(files = discovery.len(), "--dry-run: nothing downloaded");
        return Ok(());
    }

    let summary = download_all_with(
        &fetcher,
        &discovery.items,
        &cli.out,
        &DownloadOptions {
            overwrite: cli.overwrite,
            progress,
        },
        Some(&pool),
    )?;

    report(&summary, &cli.out);

    if summary.is_success() {
        Ok(())
    } else {
        Err(Error::PartialFailure {
            failed: summary.failures.len(),
            total: summary.total(),
        })
    }
}

/// Log what happened, in a form that is useful for a 3-file tileset and a
/// 300 000-file one alike.
fn report(summary: &Summary, out_dir: &Path) {
    for failure in summary.failures.iter().take(MAX_REPORTED_FAILURES) {
        tracing::error!(
            url = %failure.url,
            path = %failure.path.display(),
            error = %failure.error,
            "download failed"
        );
    }
    if summary.failures.len() > MAX_REPORTED_FAILURES {
        tracing::error!(
            remaining = summary.failures.len() - MAX_REPORTED_FAILURES,
            "…and more failures were suppressed"
        );
    }

    tracing::info!(
        downloaded = summary.downloaded,
        skipped = summary.skipped,
        failed = summary.failures.len(),
        megabytes = format!("{:.1}", summary.bytes as f64 / (1024.0 * 1024.0)),
        seconds = format!("{:.1}", summary.duration.as_secs_f64()),
        output = %out_dir.display(),
        "download finished"
    );

    if summary.skipped > 0 {
        tracing::info!(
            "{} file(s) were already present and left alone (use --overwrite to replace them)",
            summary.skipped
        );
    }
}

/// Send logs to stderr, at a verbosity chosen by `-v`/`-q` or `RUST_LOG`.
fn init_logging(cli: &Cli) {
    let default_level = if cli.quiet {
        "error"
    } else {
        match cli.verbose {
            0 => "info",
            1 => "debug",
            _ => "trace",
        }
    };

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(cli.verbose > 1);

    if cli.verbose > 0 || cli.quiet {
        builder.init();
    } else {
        // Terse by default: a download run is not a log study.
        builder.without_time().init();
    }
}
