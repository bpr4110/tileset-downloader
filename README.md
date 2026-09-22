# tileset-downloader

Download a complete [3D Tiles](https://github.com/CesiumGS/3d-tiles) tileset — the `tileset.json`, every external tileset, every implicit tiling subtree, and every tile payload — from a URL into a local directory.

A 3D Tiles tileset is not one file. It is a _tree_: a root document naming content and children, children that may be further tilesets, and (in 3D Tiles 1.1) availability _bitstreams_ that describe tiles which are never listed anywhere. Pointing a browser at `tileset.json` and hitting save gets you one file and a broken dataset. This tool walks the whole thing and mirrors it.

```
tileset-downloader https://example.com/3dtiles/tileset.json ./mytiles
```

---

## Contents

- [Features](#features)
- [Prerequisites](#prerequisites)
- [Installation](#installation)
- [Usage](#usage)
- [What gets downloaded](#what-gets-downloaded)
- [Output layout](#output-layout)
- [How it works](#how-it-works)
- [Limitations](#limitations)
- [FAQ](#faq)
- [Troubleshooting](#troubleshooting)
- [Development](#development)
- [License](#license)

---

## Features

|                          |                                                                                                                                                                        |
| ------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Complete mirroring**   | Follows external tilesets, decodes implicit tiling subtree bitstreams, and collects every content URI, so the result works offline except `_external/` files.          |
| **Parallel fetching**    | A [`rayon`](https://github.com/rayon-rs/rayon) thread pool fetches many files at once, sharing one connection-pooling HTTP client.                                     |
| **Live progress**        | An [`indicatif`](https://github.com/console-rs/indicatif) progress bar shows files done, throughput, ETA, and the current path, plus a running byte counter.           |
| **Structured logging**   | [`tracing`](https://github.com/tokio-rs/tracing) with `-v`/`-vv` levels or a `RUST_LOG` filter. Logs go to stderr, progress to stdout, so neither corrupts the other.  |
| **Resumable**            | Existing files are skipped unless `--overwrite`. Each file is written to `*.part` and renamed only when complete, so an interrupted run never leaves a truncated tile. |
| **Fault tolerant**       | Timeouts, HTTP 429/5xx, and dropped connections are retried with exponential backoff. One bad tile in 100 000 does not discard the other 99 999.                       |
| **Safe by construction** | A remote document can never make the tool write outside the output directory; hostile URIs are rejected rather than sanitised.                                         |
| **Dry run**              | `--dry-run` prints the full plan without downloading a byte.                                                                                                           |
| **No native TLS**        | Uses `rustls`, so there is no OpenSSL to install.                                                                                                                      |

---

## Prerequisites

- **Rust 1.75 or newer** (edition 2021). Install via [rustup](https://rustup.rs/):
  ```sh
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  ```
  On Windows, download `rustup-init.exe` from [rustup.rs](https://rustup.rs/).
  ```sh
  rustc --version   # should print 1.75.0 or higher
  ```
- **Network access** to the host serving the tileset.
- **Disk space** for the tileset. Photogrammetry tilesets range from a few MB to hundreds of GB; use `--dry-run` first if you are unsure.
- Nothing else. No OpenSSL, no system TLS libraries, no C compiler for the default feature set.

---

## Installation

From a clone of this repository:

```sh
cargo build --release
./target/release/tileset-downloader --help      # or target\release\tileset-downloader.exe on Windows
```

Or install it onto your `PATH`:

```sh
cargo install --path .
tileset-downloader --help
```

---

## Usage

```
tileset-downloader [OPTIONS] <URL> <DIR>
```

`<URL>` may be either the `tileset.json` itself or the directory containing it — if the URL does not look like a `.json` file, `tileset.json` is appended:

```sh
tileset-downloader https://example.com/tiles/tileset.json ./mytiles
tileset-downloader https://example.com/tiles/         ./mytiles   # same thing
```

### Options

| Option                         | Default                        | Description                                                                                            |
| ------------------------------ | ------------------------------ | ------------------------------------------------------------------------------------------------------ |
| `-j, --concurrency <N>`        | logical CPU count              | How many files to fetch in parallel. Raise it for many small tiles; lower it to be gentle on a server. |
| `-H, --header <NAME: VALUE>`   | —                              | Extra request header. Repeatable. Use for `Authorization`, API keys, or `Referer` checks.              |
| `--overwrite`                  | off                            | Re-download files that are already present.                                                            |
| `--dry-run`                    | off                            | Discover everything, print the plan to stdout, exit. Downloads nothing.                                |
| `--no-progress`                | off                            | Hide progress bars (implied by `--quiet`).                                                             |
| `--timeout <SECONDS>`          | `60`                           | Per-request timeout. `0` disables it.                                                                  |
| `--connect-timeout <SECONDS>`  | `30`                           | Connection timeout. `0` disables it.                                                                   |
| `--retries <N>`                | `3`                            | Retries per request for _transient_ failures (timeouts, HTTP 408/425/429/5xx). `0` disables retries.   |
| `--user-agent <STRING>`        | `tileset-downloader/<version>` | Override the `User-Agent`.                                                                             |
| `-v, --verbose`                | off                            | `-v` = debug logs, `-vv` = trace logs.                                                                 |
| `-q, --quiet`                  | off                            | Only warnings and errors; also hides progress.                                                         |
| `-h, --help` / `-V, --version` | —                              | Help and version.                                                                                      |

Log verbosity can also be set with `RUST_LOG`, which overrides `-v`/`-q`:

```sh
RUST_LOG=tileset_downloader=debug,reqwest=info tileset-downloader https://example.com/tileset.json out
```

### Examples

```sh
# Basic mirror
tileset-downloader https://example.com/3dtiles/tileset.json ./mytiles

# See exactly what would happen first
tileset-downloader https://example.com/3dtiles/tileset.json ./mytiles --dry-run

# Many small tiles: fetch 64 at a time
tileset-downloader https://example.com/tileset.json ./mytiles -j 64

# Authenticated / signed tileset
tileset-downloader https://api.example.com/tileset.json ./mytiles \
    -H 'Authorization: Bearer <token>'

# Re-download every file, even ones already present
tileset-downloader https://example.com/tileset.json ./mytiles --overwrite

# Be a good citizen on a fragile server
tileset-downloader https://example.com/tileset.json ./mytiles -j 4 --retries 8 --timeout 120
```

### Exit codes

| Code | Meaning                                                                                         |
| ---- | ----------------------------------------------------------------------------------------------- |
| `0`  | Every file downloaded or was already present.                                                   |
| `1`  | Discovery failed, _or_ one or more files failed to download. The failures are listed on stderr. |

---

## What gets downloaded

| 3D Tiles feature                                                                | Support                                      |
| ------------------------------------------------------------------------------- | -------------------------------------------- |
| `content.uri` (1.1) and `content.url` (1.0)                                     | Yes                                          |
| `contents[]` — multiple contents (1.1)                                          | Yes                                          |
| `extensions."3DTILES_multiple_contents"` (1.0)                                  | Yes                                          |
| External tilesets — `content.uri` ending in `.json`, followed recursively       | Yes                                          |
| `tile.implicitTiling` — implicit tiling as a core 1.1 property                  | Yes                                          |
| `extensions."3DTILES_implicit_tiling"` — implicit tiling as a 1.0 extension     | Yes                                          |
| `QUADTREE` and `OCTREE` subdivision schemes                                     | Yes                                          |
| `.subtree` as plain JSON, or as the binary container with a JSON + binary chunk | Yes                                          |
| Availability as `{"constant": 1}` or as a bitstream                             | Yes                                          |
| Bitstreams as a `bufferView` reference, or base64 (the 1.0 draft form)          | Yes                                          |
| Subtree buffers with an external `uri` (fetched, and mirrored too)              | Yes                                          |
| `contentUri` / `subtreeUri` overrides inside a subtree (1.0 draft)              | Yes                                          |
| Tile payload formats (`.b3dm`, `.i3dm`, `.pnts`, `.cmpt`, `.glb`, …)            | Treated as opaque bytes; no format is parsed |

Unknown extensions are ignored; the tool only cares about which URIs a tileset names, never what a tile _means_.

---

## Output layout

The output root mirrors the **folder that holds the entry `tileset.json`**: `tileset.json` lands directly in the output directory, the files beside it keep their relative position, and anything outside that folder is bucketed under `_external/` (see below). So for

```
https://example.com/3dtiles/v2/tileset.json
```

with a content URI of `tiles/0/0/3.b3dm`, the result is

```
out/
├── tileset.json
└── tiles/
    └── 0/
        └── 0/
            └── 3.b3dm
```

Exactly the tree below the folder that holds `tileset.json`, with the URL prefix above it stripped.

### Files outside the tileset folder: `_external/`

A reference that leaves the tileset folder — a `../` that climbs above it, a different host or port, or a host-root path that resolves above it — is written under

```
out/_external/<host>[/<port>]/<full-url-path>
```

So `../shared/tex.png` from the example above becomes

```
out/_external/example.com/3dtiles/shared/tex.png
```

A non-default port is its own segment (`_external/cdn.example.org:8443/...` becomes `_external/cdn.example.org/8443/...`).

Relative URIs inside the tileset folder keep resolving when the output directory is served over HTTP, because the URL prefix that is common to the whole tileset is stripped consistently. Files in `_external/` do **not** keep their relative position: a tileset that reaches above its own folder (or across hosts) names those files by a relative path the mirror no longer reproduces, so if you need such a file to work offline, fetch it separately or rewrite its URI. Tilesets whose references stay inside their own folder — the overwhelming majority — are unaffected.

Query strings and fragments are dropped from local names (`tileset.json?token=…` becomes `tileset.json`), and any segment that cannot be a safe file name (`..`, a backslash, a colon, a Windows device name) is rejected or escaped.

---

## How it works

The run has two phases.

**1. Discovery** (`src/discovery.rs`)

- _Tileset documents._ A breadth-first crawl. Each round fetches and parses the entire frontier in parallel; any `content.uri` ending in `.json` is another tileset, queued for the next round. A visited set makes reference cycles harmless.
- _Implicit tiling._ An implicitly tiled tile lists no children; it points at `.subtree` files. Each subtree is decoded into three availability bitstreams — which tiles exist, which have content, which child subtrees exist — and walked one subtree level per parallel round.

Discovery is **fail-fast**: a broken external tileset or an undecodable subtree means the dataset cannot be described, so there is nothing worth downloading.

**2. Download** (`src/download.rs`)

- Every item is fetched through a shared `reqwest` blocking client inside a `rayon` pool, then written to `<name>.part` and atomically renamed.
- Failures are collected, not propagated: the run finishes, reports every failure, and exits non-zero.

Supporting modules: `src/tileset.rs` (the `tileset.json` model and tile walk), `src/implicit.rs` (subtree decoding, Morton order, availability bits), `src/net.rs` (client + retry policy), `src/path_util.rs` (URL → safe local path), `src/error.rs` (typed errors).

### Correctness notes

The implicit tiling code is written to the normative definitions rather than to a guess, because getting any of it wrong silently downloads the wrong tiles:

- Availability bits are packed **least-significant bit first**: bit `i` is bit `i % 8` of byte `i / 8`.
- A tile's index within a subtree is `(N^level - 1) / (N - 1) + morton(x, y)`, where `N` is 4 for `QUADTREE` and 8 for `OCTREE`; `tileAvailability` holds exactly `(N^subtreeLevels - 1) / (N - 1)` bits and `childSubtreeAvailability` holds `N^subtreeLevels`.
- Morton order is the exact interleaving used by CesiumJS (`MortonOrder.encode2D` / `encode3D`).
- `subtrees.uri` is templated with the **subtree root's global** level/x/y; content templates are templated with the **tile's global** level/x/y.

---

## Limitations

- **No 3D Tiles auth beyond headers.** Cesium ion and similar services issue per-session signed URLs. Resolve the asset endpoint yourself and pass the resulting URL (or use `-H` for a bearer token).
- **No bandwidth or request-rate throttling.** `-j` is the only lever. Very large datasets can be heavy on the origin server; see [FAQ](#faq).
- **No content verification.** Downloads are not checksummed and tiles are not validated — the tool does not parse `.b3dm`/`.glb` at all.
- **No incremental sync.** `--overwrite` re-fetches everything; there is no "only what changed" mode.
- **A `contentAvailability` that is absent is assumed to mean "every available tile has content".** The schema makes the field optional. Assuming the opposite would silently download nothing, which is the worse failure for a downloader, so the tool errs toward downloading and logs a warning. If you see spurious 404s and that warning, the tileset genuinely has content-less tiles.
- **Two different URLs that would map to the same local path abort the run.** This avoids silently overwriting one file with another. It is rare, but a tileset that names two URLs whose sanitised paths coincide (for example a Windows device name and its escaped form) will trip it.
- **Files pulled in from outside the tileset folder lose their relative position.** They are kept under `_external/<host>[/<port>]/...`, which is not the path the tileset references, so they will not resolve when you serve the mirror. Fetch them separately or rewrite their URIs if you need those to work offline. A tileset that stays inside its own folder — the normal case — is unaffected.
- **Path collisions are checked, but a mirror is only as trustworthy as its source.** Nothing stops a tileset from being _wrong_; the tool only guarantees it will not be _unsafe_.

---

## FAQ

**Do I need to download the whole tileset to view it?**

For a local viewer, yes — that is the point of the tool. Some formats allow partial fetching at runtime, but only against the original server.

**What is a `.subtree` file, and why are there so many?**

In 3D Tiles 1.1, implicitly tiled datasets describe tiles with bitstreams instead of listing them. A `.subtree` file holds the availability bits for a fixed-depth chunk of the tree. They are tiny (a few hundred bytes each) and are part of the dataset, so they are downloaded too — without them the tileset will not load.

**Why is the download flat at the top level, with `_external/` for everything outside the tileset?**

Because the tool mirrors the folder that holds the entry `tileset.json`, not the whole URL path: `tileset.json` and the files beside it land directly in the output directory, so relative URIs keep resolving. Anything outside that folder — a different host, port, or a `../` that climbs above it — is kept under `_external/<host>[/<port>]/...`. See [Output layout](#output-layout). Serve the output directory as-is (for example `python -m http.server` inside it) and a tileset whose references stay inside its own folder works unchanged; `_external/` files are the exception.

**Can I re-run it after it failed halfway?**

Yes, and you should. Files already on disk are skipped by default, so a re-run only fetches what is missing. Add `--overwrite` only when you want to replace existing files.

**Will it hammer the server?**

It opens as many parallel requests as you ask for with `-j`, no more, and it sends an identifying `User-Agent`. Default is your CPU count. On a shared or fragile origin, use `-j 4`, raise `--retries`, and raise `--timeout`. The retry backoff respects HTTP 429/5xx instead of hammering through them.

**Does it work behind a proxy?**

Yes. `reqwest` honours the standard `HTTP_PROXY`, `HTTPS_PROXY`, and `NO_PROXY` environment variables.

**Does it work with Cesium ion assets?**

Not directly: ion URLs require a token and expire. Call the ion endpoint (`GET https://api.cesium.com/v1/assets/<id>/endpoint?access_token=…`), take the `url` from the response, and pass that to this tool. The tool itself does not perform ion authentication.

**How do I check what it will do before committing to a 200 GB download?**

```sh
tileset-downloader <url> ./out --dry-run | wc -l
```

`--dry-run` performs the full discovery crawl and prints one line per file.

**It says success but my viewer shows nothing.**

Serve the directory over HTTP rather than opening files with `file://` — browsers block `file://` requests made by web viewers for security reasons. `python -m http.server 8000` inside the output directory is enough.

**Can I use it as a library?**

Yes, `tileset-downloader` exposes a `lib` target: `discover()` produces the plan, `download_all()` executes it. See the crate-level docs in `src/lib.rs`.

---

## Troubleshooting

| Symptom                                                   | Cause and fix                                                                                                                                                                                      |
| --------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `server replied HTTP 404`                                 | The tile exists in the tileset but not on the server, or the tileset is relative to a different base. Try the `tileset.json` URL specifically rather than a directory URL.                         |
| `server replied HTTP 403`                                 | The host wants an `Authorization` or `Referer` header, or is blocking the default `User-Agent`. Use `-H`, or `--user-agent`.                                                                       |
| `invalid URL` / `unsupported URL scheme`                  | Only `http` and `https` are supported. `file://` URLs are deliberately rejected.                                                                                                                   |
| `could not parse JSON from …`                             | The URL returned HTML (a login page, an error page, or a directory listing) instead of JSON. Check the URL in a browser.                                                                           |
| `tileset … has no root tile`                              | The document is not a tileset, or is an empty stub.                                                                                                                                                |
| `refusing to continue: more than N files were discovered` | A malformed or hostile implicit tiling availability stream describes an unbounded tree. The cap is a guard against exhausting memory.                                                              |
| `malformed implicit tiling data in …`                     | A `.subtree` document did not match the spec: a bitstream of the wrong length, a buffer view outside its buffer, or a missing `tileAvailability`. Usually a producer bug worth reporting upstream. |
| `refusing to write … it does not resolve to a safe path`  | A URI contained a segment that cannot be a safe file name. The URL is printed; this is a safety refusal, not a bug.                                                                                |
| Certificates fail behind a corporate proxy                | The proxy is re-signing TLS. Install its root CA into the system trust store, or point `SSL_CERT_FILE` at the CA bundle.                                                                           |
| Progress bar renders as garbage                           | Some terminals and CI logs do not understand the escape codes. Use `--no-progress` (or `--quiet`).                                                                                                 |
| Very slow with many small files                           | Raise `-j`. The default is the CPU count, but downloads are I/O-bound, so `-j 32` or higher is often much faster.                                                                                  |

Add `-v` for per-file debug logs, or `-vv` for trace logs including every retry.

---

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

`cargo test` needs no network:

- **Unit tests** live beside the code they cover — Morton encoding round trips, availability bit decoding (including a hand-built binary `.subtree`), tile-index arithmetic, path-traversal refusals, retry classification, and CLI parsing.
- **`tests/integration.rs`** starts a real HTTP server on loopback and mirrors a fixture with every reference spelling, an external tileset, and a two-level implicit tiling hierarchy, plus a nested entry that pins the tileset-relative output layout and its `_external/` bucket, then asserts the exact file set, the exact bytes, that a second run skips everything, and that a single 404 fails only that file.

Because the tileset format has sharp edges, the implicit tiling implementation was verified against real datasets rather than only fixtures. Mirroring `CesiumGS/3d-tiles-samples` `1.1/SparseImplicitQuadtree` and `1.1/SparseImplicitOctree` reproduces the repository's `content/` and `subtrees/` directories **exactly** — same file count, same names, nothing missing, nothing extra (2D and 3D Morton decoding, sparse availability, and `{level}` templating all included).

The `Cargo.lock` file is tracked, because this is a binary crate: reproducible builds matter more than the library convention of ignoring it.

---

## License

MIT — see [LICENSE](LICENSE).

This project is not affiliated with Cesium. 3D Tiles is an open specification maintained by the [CesiumGS](https://github.com/CesiumGS) community.
