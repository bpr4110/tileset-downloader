//! End-to-end tests against a real (in-process) HTTP server.
//!
//! The unit tests cover parsing in isolation; this file covers the thing the
//! tool actually does: crawl a tileset over HTTP and write the mirror to disk.
//! The fixture deliberately includes all four shapes a reference can take — a
//! plain content URI, a 1.1 `contents` array, the 1.0 `3DTILES_multiple_contents`
//! extension, and an external tileset — plus an implicit tiling hierarchy two
//! subtree levels deep, so the bitstream decoding, the child-subtree walk, the
//! `{level}_{x}_{y}` templating, and the rayon download loop are all exercised
//! against bytes that actually came off a socket.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use rayon::ThreadPoolBuilder;
use tempfile::TempDir;
use url::Url;

use tileset_downloader::discovery::DiscoveryOptions;
use tileset_downloader::download::DownloadOptions;
use tileset_downloader::{discover_with, download_all_with, path_util, ClientConfig, Fetcher};

/// The external tileset, which is implicitly tiled two subtree levels deep.
///
/// `subtreeLevels = 2` with `availableLevels = 3` gives a root subtree holding
/// its own tile plus four children (levels 0 and 1), and sixteen child subtrees
/// covering level 2 — a shape where the `{level}` template really changes.
const SUB_TILESET: &str = r#"{
  "asset": { "version": "1.1" },
  "geometricError": 128,
  "root": {
    "boundingVolume": { "box": [0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1] },
    "geometricError": 64,
    "refine": "REPLACE",
    "content": { "uri": "content/{level}_{x}_{y}.b3dm" },
    "implicitTiling": {
      "subdivisionScheme": "QUADTREE",
      "subtreeLevels": 2,
      "availableLevels": 3,
      "subtrees": { "uri": "subtrees/{level}_{x}_{y}.subtree" }
    }
  }
}"#;

/// The entry tileset: every way of naming content, plus an external tileset.
const ROOT_TILESET: &str = r#"{
  "asset": { "version": "1.1", "generator": "integration-test" },
  "geometricError": 512,
  "root": {
    "boundingVolume": { "box": [0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1] },
    "geometricError": 256,
    "refine": "ADD",
    "content": { "uri": "tiles/0.b3dm" },
    "children": [
      { "content": { "uri": "tiles/1.pnts" } },
      { "contents": [ { "uri": "tiles/2.glb" } ] },
      {
        "extensions": {
          "3DTILES_multiple_contents": {
            "contents": [ { "uri": "tiles/3.cmpt" } ]
          }
        }
      },
      { "content": { "uri": "sub/tileset.json" } }
    ]
  }
}"#;

/// A subtree whose root tile has content and whose sixteen child subtrees exist.
const ROOT_SUBTREE: &str = r#"{
  "tileAvailability": { "constant": 1 },
  "contentAvailability": [ { "constant": 1 } ],
  "childSubtreeAvailability": { "constant": 1 }
}"#;

/// A leaf subtree: its own root tile has content, and it subdivides no further.
const LEAF_SUBTREE: &str = r#"{
  "tileAvailability": { "constant": 1 },
  "contentAvailability": [ { "constant": 1 } ],
  "childSubtreeAvailability": { "constant": 0 }
}"#;

/// A tileset served from a nested folder: content beside the document, plus a
/// relative reference that climbs above the tileset folder into `_external/`.
const NESTED_TILESET: &str = r#"{
  "asset": { "version": "1.1" },
  "geometricError": 512,
  "root": {
    "boundingVolume": { "box": [0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1] },
    "geometricError": 256,
    "refine": "ADD",
    "content": { "uri": "0.b3dm" },
    "children": [
      { "content": { "uri": "../shared/tex.png" } }
    ]
  }
}"#;

/// Every path the *implicit* half of the fixture should produce.
fn implicit_paths() -> BTreeSet<String> {
    let mut paths = BTreeSet::new();

    // The root subtree's own tile (level 0) and its four children (level 1).
    paths.insert("sub/content/0_0_0.b3dm".to_owned());
    for x in 0..2 {
        for y in 0..2 {
            paths.insert(format!("sub/content/1_{x}_{y}.b3dm"));
        }
    }
    paths.insert("sub/subtrees/0_0_0.subtree".to_owned());

    // Sixteen child subtrees at level 2, each with exactly one tile.
    for x in 0..4 {
        for y in 0..4 {
            paths.insert(format!("sub/content/2_{x}_{y}.b3dm"));
            paths.insert(format!("sub/subtrees/2_{x}_{y}.subtree"));
        }
    }

    paths
}

/// Build the full route table, optionally omitting one path to simulate a 404.
fn routes(missing: Option<&str>) -> HashMap<String, Vec<u8>> {
    let mut routes: HashMap<String, Vec<u8>> = HashMap::new();
    let mut add = |path: &str, body: Vec<u8>| {
        if Some(path) != missing {
            routes.insert(path.to_owned(), body);
        }
    };

    add("/tileset.json", ROOT_TILESET.as_bytes().to_vec());
    add("/tiles/0.b3dm", b"tile-0".to_vec());
    add("/tiles/1.pnts", b"tile-1".to_vec());
    add("/tiles/2.glb", b"tile-2".to_vec());
    add("/tiles/3.cmpt", b"tile-3".to_vec());
    add("/sub/tileset.json", SUB_TILESET.as_bytes().to_vec());
    add(
        "/sub/subtrees/0_0_0.subtree",
        ROOT_SUBTREE.as_bytes().to_vec(),
    );
    for x in 0..4 {
        for y in 0..4 {
            add(
                &format!("/sub/subtrees/2_{x}_{y}.subtree"),
                LEAF_SUBTREE.as_bytes().to_vec(),
            );
        }
    }
    for path in implicit_paths() {
        if path.ends_with(".b3dm") {
            add(&format!("/{path}"), format!("body-of-{path}").into_bytes());
        }
    }

    routes
}

#[test]
fn mirrors_explicit_and_implicit_tilesets_end_to_end() {
    let server = TestServer::start(routes(None));
    let output = TempDir::new().expect("temp dir");

    let pool = ThreadPoolBuilder::new()
        .num_threads(4)
        .build()
        .expect("thread pool");
    let fetcher = Fetcher::new(&ClientConfig::default()).expect("client");
    let entry = path_util::normalize_entry_url(&server.url("/tileset.json")).expect("entry URL");

    let discovery = discover_with(
        &fetcher,
        &entry,
        &DiscoveryOptions { progress: false },
        Some(&pool),
    )
    .expect("discovery");

    // 6 explicitly referenced files: the entry document, four tiles, and the
    // external tileset. Then 17 subtrees and 21 contents from implicit tiling.
    assert_eq!(discovery.external_tilesets, 1);
    assert_eq!(discovery.implicit_tilesets, 1);
    assert_eq!(discovery.subtree_files, 17);
    assert_eq!(discovery.len(), 6 + 17 + 21);

    let summary = download_all_with(
        &fetcher,
        &discovery.items,
        output.path(),
        &DownloadOptions {
            overwrite: false,
            progress: false,
        },
        Some(&pool),
    )
    .expect("download");

    assert!(summary.is_success(), "failures: {:?}", summary.failures);
    assert_eq!(summary.downloaded, discovery.len());
    assert_eq!(summary.skipped, 0);

    // Every planned path exists, with the bytes the server served.
    let mut expected: BTreeSet<String> = implicit_paths();
    for path in [
        "tileset.json",
        "tiles/0.b3dm",
        "tiles/1.pnts",
        "tiles/2.glb",
        "tiles/3.cmpt",
        "sub/tileset.json",
    ] {
        expected.insert(path.to_owned());
    }
    assert_eq!(collect_files(output.path()), expected);

    assert_eq!(read(&output.path().join("tiles/0.b3dm")), "tile-0");
    assert_eq!(
        read(&output.path().join("sub/content/2_3_2.b3dm")),
        "body-of-sub/content/2_3_2.b3dm"
    );
    assert_eq!(
        read(&output.path().join("sub/tileset.json")),
        SUB_TILESET,
        "the external tileset must be mirrored so the result works offline"
    );

    // Nothing half-written may be left behind.
    assert!(
        !has_extension(output.path(), "part"),
        "a .part file survived the run"
    );

    // A second run is a no-op: everything already exists.
    let second = download_all_with(
        &fetcher,
        &discovery.items,
        output.path(),
        &DownloadOptions {
            overwrite: false,
            progress: false,
        },
        Some(&pool),
    )
    .expect("second download");
    assert_eq!(second.downloaded, 0);
    assert_eq!(second.skipped, discovery.len());
    assert!(second.is_success());
}

#[test]
fn a_missing_tile_is_reported_without_losing_the_rest() {
    let server = TestServer::start(routes(Some("/tiles/1.pnts")));
    let output = TempDir::new().expect("temp dir");

    let fetcher = Fetcher::new(&ClientConfig::default()).expect("client");
    let entry = path_util::normalize_entry_url(&server.url("/tileset.json")).expect("entry URL");

    let discovery = discover_with(
        &fetcher,
        &entry,
        &DiscoveryOptions { progress: false },
        None,
    )
    .expect("discovery");

    let summary = download_all_with(
        &fetcher,
        &discovery.items,
        output.path(),
        &DownloadOptions {
            overwrite: false,
            progress: false,
        },
        None,
    )
    .expect("download");

    assert!(!summary.is_success());
    assert_eq!(summary.failures.len(), 1);
    assert!(summary.failures[0].url.ends_with("/tiles/1.pnts"));
    assert_eq!(summary.downloaded, discovery.len() - 1);

    // The rest of the mirror is intact and usable.
    assert!(output.path().join("tiles/0.b3dm").exists());
    assert!(output.path().join("sub/content/0_0_0.b3dm").exists());
    assert!(!output.path().join("tiles/1.pnts").exists());
}

#[test]
fn a_directory_url_is_normalised_to_tileset_json() {
    let server = TestServer::start(routes(None));
    let output = TempDir::new().expect("temp dir");

    let fetcher = Fetcher::new(&ClientConfig::default()).expect("client");
    // Note: no `/tileset.json` suffix; the tool must add it.
    let entry = path_util::normalize_entry_url(&server.url("/")).expect("entry URL");
    assert!(entry.path().ends_with("/tileset.json"));

    let discovery = discover_with(
        &fetcher,
        &entry,
        &DiscoveryOptions { progress: false },
        None,
    )
    .expect("ok");
    let summary = download_all_with(
        &fetcher,
        &discovery.items,
        output.path(),
        &DownloadOptions {
            overwrite: false,
            progress: false,
        },
        None,
    )
    .expect("download");

    assert!(summary.is_success());
    assert!(output.path().join("tileset.json").exists());
}

#[test]
fn a_nested_entry_mirrors_relative_to_the_tileset_folder() {
    let mut routes = HashMap::new();
    routes.insert(
        "/a/b/tileset.json".to_owned(),
        NESTED_TILESET.as_bytes().to_vec(),
    );
    routes.insert("/a/b/0.b3dm".to_owned(), b"nested-tile-0".to_vec());
    routes.insert("/a/shared/tex.png".to_owned(), b"shared-texture".to_vec());
    let server = TestServer::start(routes);
    let output = TempDir::new().expect("temp dir");

    let fetcher = Fetcher::new(&ClientConfig::default()).expect("client");
    let entry =
        path_util::normalize_entry_url(&server.url("/a/b/tileset.json")).expect("entry URL");

    let discovery = discover_with(
        &fetcher,
        &entry,
        &DiscoveryOptions { progress: false },
        None,
    )
    .expect("discovery");

    let summary = download_all_with(
        &fetcher,
        &discovery.items,
        output.path(),
        &DownloadOptions {
            overwrite: false,
            progress: false,
        },
        None,
    )
    .expect("download");

    assert!(summary.is_success(), "failures: {:?}", summary.failures);
    assert_eq!(summary.downloaded, 3);

    // The output root is the folder holding `tileset.json`, not the whole URL
    // path; the `../shared/tex.png` reference climbs above it and is bucketed
    // under `_external/<host>/<port>/<full-url-path>`.
    let port = server.addr.port();
    let mut expected = BTreeSet::new();
    expected.insert("tileset.json".to_owned());
    expected.insert("0.b3dm".to_owned());
    expected.insert(format!("_external/127.0.0.1/{port}/a/shared/tex.png"));
    assert_eq!(collect_files(output.path()), expected);

    assert_eq!(read(&output.path().join("0.b3dm")), "nested-tile-0");
    assert!(
        !output.path().join("a").exists(),
        "the tileset's folder must not appear at the output root"
    );
}

// ---------------------------------------------------------------------------
// A minimal HTTP/1.1 server: enough to answer one GET per connection.
// ---------------------------------------------------------------------------

struct TestServer {
    addr: SocketAddr,
    running: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl TestServer {
    fn start(routes: HashMap<String, Vec<u8>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let running = Arc::new(AtomicBool::new(true));
        let flag = Arc::clone(&running);
        let routes = Arc::new(routes);

        let handle = thread::spawn(move || {
            for stream in listener.incoming() {
                if !flag.load(Ordering::SeqCst) {
                    break;
                }
                if let Ok(stream) = stream {
                    let routes = Arc::clone(&routes);
                    thread::spawn(move || serve(stream, &routes));
                }
            }
        });

        Self {
            addr,
            running,
            handle: Some(handle),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        // Poke the listener so `accept` returns and the loop can see the flag.
        let _ = TcpStream::connect(self.addr);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn serve(mut stream: TcpStream, routes: &HashMap<String, Vec<u8>>) {
    let Some(path) = read_request_path(&mut stream) else {
        return;
    };

    let body = routes.get(&path);
    let header = match body {
        Some(body) => format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n",
            body.len()
        ),
        None => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned(),
    };

    let _ = stream.write_all(header.as_bytes());
    if let Some(body) = body {
        let _ = stream.write_all(body);
    }
    let _ = stream.flush();
}

/// Read until the end of the request headers and return the request target's path.
fn read_request_path(stream: &mut TcpStream) -> Option<String> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];

    while !buffer.windows(4).any(|window| window == b"\r\n\r\n") && buffer.len() < 64 * 1024 {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            Err(_) => return None,
        }
    }

    let request = String::from_utf8_lossy(&buffer);
    let target = request.lines().next()?.split_whitespace().nth(1)?;
    Some(target.split(['?', '#']).next().unwrap_or(target).to_owned())
}

// ---------------------------------------------------------------------------
// Small filesystem helpers, so assertions stay readable.
// ---------------------------------------------------------------------------

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| panic!("reading {}: {error}", path.display()))
}

fn collect_files(root: &Path) -> BTreeSet<String> {
    let mut files = BTreeSet::new();
    let mut stack = vec![root.to_path_buf()];

    while let Some(directory) = stack.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let relative: PathBuf = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
                files.insert(relative.to_string_lossy().replace('\\', "/"));
            }
        }
    }

    files
}

fn has_extension(root: &Path, extension: &str) -> bool {
    collect_files(root)
        .iter()
        .any(|file| Path::new(file).extension().and_then(|ext| ext.to_str()) == Some(extension))
}

/// Keep `Url` out of the helpers above, but prove the type is what we think.
#[test]
fn entry_urls_are_http_or_https() {
    let url = Url::parse("http://127.0.0.1:1/tileset.json").expect("parses");
    assert_eq!(url.scheme(), "http");
}
