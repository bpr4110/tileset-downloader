//! The `tileset.json` object model and the walk over its tile tree.
//!
//! Only the fields that matter for downloading are modelled. Everything else
//! (bounding volumes, geometric error, transforms, metadata) is ignored by
//! serde, which is deliberate: the downloader must not care about what a tile
//! *means*, only about which URIs it names.
//!
//! Three ways of naming content are supported, because all three exist in the
//! wild:
//!
//! * `content.uri` (3D Tiles 1.1) and its 1.0 spelling `content.url`;
//! * `contents[].uri` (3D Tiles 1.1 multiple contents);
//! * `extensions."3DTILES_multiple_contents".contents[].uri` (the 1.0 extension).
//!
//! A `content.uri` that ends in `.json` is an **external tileset**: a reference
//! to another tileset document that must itself be fetched and walked, not a
//! file to download as a tile.

use serde::Deserialize;
use serde_json::Value;

/// Extension key for implicit tiling (3D Tiles 1.1).
pub const IMPLICIT_TILING_EXTENSION: &str = "3DTILES_implicit_tiling";
/// Extension key for the 1.0 spelling of multiple contents.
pub const MULTIPLE_CONTENTS_EXTENSION: &str = "3DTILES_multiple_contents";

/// A parsed `tileset.json`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tileset {
    /// Tileset metadata; used only to log the version.
    #[serde(default)]
    pub asset: Asset,
    /// The root of the tile tree. Absent only in malformed tilesets.
    #[serde(default)]
    pub root: Option<Tile>,
    /// Extensions this tileset uses.
    #[serde(default)]
    pub extensions_used: Vec<String>,
    /// Extensions a consumer must understand.
    #[serde(default)]
    pub extensions_required: Vec<String>,
}

impl Tileset {
    /// Parse a tileset document.
    pub fn from_slice(bytes: &[u8]) -> std::result::Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }

    /// Walk the whole tile tree and collect every URI it references.
    ///
    /// Iterative rather than recursive: quadtree tilesets routinely nest 20+
    /// levels, and a hostile one could nest far deeper.
    pub fn scan(&self) -> Scan {
        let mut scan = Scan::default();
        let mut stack: Vec<&Tile> = self.root.iter().collect();

        while let Some(tile) = stack.pop() {
            match tile.implicit_source() {
                // With implicit tiling the tile's `content.uri` is a template
                // such as `content/{level}_{x}_{y}.b3dm`. Downloading it
                // literally would fetch a file with braces in its name, so the
                // tile is handed to the implicit expander instead.
                Some(implicit) => scan.implicits.push(implicit),
                None => {
                    for uri in tile.content_uris() {
                        scan.references.push(Reference {
                            kind: classify(uri),
                            uri: uri.to_owned(),
                        });
                    }
                }
            }
            // Reversed, so popping yields the children in document order and the
            // plan is reproducible run to run.
            stack.extend(tile.children.iter().rev());
        }

        scan
    }
}

/// Tileset `asset` object.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Asset {
    /// Spec version the tileset claims, e.g. `"1.0"` or `"1.1"`.
    #[serde(default)]
    pub version: Option<String>,
    /// Tool that produced the tileset.
    #[serde(default)]
    pub generator: Option<String>,
}

/// A single tile: some content, some children, or both.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tile {
    /// Singly named content (1.0: `content.url`, 1.1: `content.uri`).
    #[serde(default)]
    pub content: Option<Content>,
    /// Multiply named content (3D Tiles 1.1).
    #[serde(default)]
    pub contents: Vec<Content>,
    /// Child tiles.
    #[serde(default)]
    pub children: Vec<Tile>,
    /// 3D Tiles 1.1 core implicit tiling object (`tile.implicitTiling`).
    #[serde(default)]
    pub implicit_tiling: Option<Value>,
    /// Raw extensions, of which two are understood.
    #[serde(default)]
    pub extensions: Option<serde_json::Map<String, Value>>,
}

impl Tile {
    /// Look up one extension object by key.
    pub fn extension(&self, name: &str) -> Option<&Value> {
        self.extensions.as_ref()?.get(name)
    }

    /// Every URI this tile names, across all three spellings.
    pub fn content_uris(&self) -> Vec<&str> {
        let mut uris = Vec::new();

        if let Some(uri) = self.content.as_ref().and_then(Content::uri) {
            uris.push(uri);
        }
        for content in &self.contents {
            if let Some(uri) = content.uri() {
                uris.push(uri);
            }
        }
        if let Some(contents) = self
            .extension(MULTIPLE_CONTENTS_EXTENSION)
            .and_then(|ext| ext.get("contents"))
            .and_then(Value::as_array)
        {
            for content in contents {
                if let Some(uri) = content
                    .get("uri")
                    .or_else(|| content.get("url"))
                    .and_then(Value::as_str)
                {
                    uris.push(uri);
                }
            }
        }

        uris
    }

    /// The implicit tiling declaration on this tile, if any.
    ///
    /// 3D Tiles 1.0 carried it in the `3DTILES_implicit_tiling` extension; 1.1
    /// promoted it to a core `implicitTiling` property. Both are accepted, with
    /// the extension winning when a document somehow has both, matching
    /// CesiumJS.
    pub fn implicit_source(&self) -> Option<ImplicitSource> {
        let raw = self
            .extension(IMPLICIT_TILING_EXTENSION)
            .or(self.implicit_tiling.as_ref())?
            .clone();

        Some(ImplicitSource {
            raw,
            // With implicit tiling these URIs are *templates*; they are filled
            // in per tile while expanding subtrees.
            content_templates: self.content_uris().into_iter().map(str::to_owned).collect(),
        })
    }
}

/// A tile's content reference.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Content {
    /// The content URI. `url` is the 3D Tiles 1.0 spelling.
    #[serde(default, alias = "url")]
    pub uri: Option<String>,
}

impl Content {
    /// The URI, whichever spelling the document used.
    pub fn uri(&self) -> Option<&str> {
        self.uri.as_deref()
    }
}

/// What a referenced URI points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReferenceKind {
    /// A tile payload: `b3dm`, `i3dm`, `pnts`, `cmpt`, `glb`, `gltf`, ...
    Content,
    /// Another tileset document that must be walked in turn.
    Tileset,
}

/// A URI referenced by a tile.
#[derive(Debug, Clone)]
pub struct Reference {
    /// The URI exactly as written in the tileset, still relative.
    pub uri: String,
    /// Whether this is tile payload or another tileset document.
    pub kind: ReferenceKind,
}

/// A tile carrying the implicit tiling extension.
#[derive(Debug, Clone)]
pub struct ImplicitSource {
    /// The extension object, parsed later by [`crate::implicit`].
    pub raw: Value,
    /// The tile's `content.uri` value(s), still holding `{level}`/`{x}`/`{y}`
    /// placeholders. A subtree document may override these.
    pub content_templates: Vec<String>,
}

/// Everything one tileset document references.
#[derive(Debug, Clone, Default)]
pub struct Scan {
    /// Concrete URIs to fetch.
    pub references: Vec<Reference>,
    /// Implicit tiling subtrees to expand.
    pub implicits: Vec<ImplicitSource>,
}

/// Decide whether a URI names a tileset document or tile payload.
///
/// Only the path is inspected, so query strings such as
/// `sub/tileset.json?token=...` still classify correctly.
pub fn classify(uri: &str) -> ReferenceKind {
    let path = uri.split(['?', '#']).next().unwrap_or(uri);
    if path.to_ascii_lowercase().ends_with(".json") {
        ReferenceKind::Tileset
    } else {
        ReferenceKind::Content
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(json: &str) -> Scan {
        Tileset::from_slice(json.as_bytes())
            .expect("fixture should parse")
            .scan()
    }

    #[test]
    fn collects_content_from_every_spelling() {
        let scan = scan(
            r#"{
              "asset": { "version": "1.1" },
              "root": {
                "content": { "uri": "a.b3dm" },
                "children": [
                  { "content": { "url": "legacy/1.0-legacy.b3dm" } },
                  { "contents": [ { "uri": "one.pnts" }, { "uri": "two.pnts" } ] },
                  { "extensions": {
                      "3DTILES_multiple_contents": {
                        "contents": [ { "uri": "ext/three.cmpt" }, { "url": "ext/four.glb" } ]
                      }
                  } }
                ]
              }
            }"#,
        );

        let uris: Vec<&str> = scan.references.iter().map(|r| r.uri.as_str()).collect();
        assert_eq!(
            uris,
            vec![
                "a.b3dm",
                "legacy/1.0-legacy.b3dm",
                "one.pnts",
                "two.pnts",
                "ext/three.cmpt",
                "ext/four.glb",
            ]
        );
        assert!(scan
            .references
            .iter()
            .all(|r| r.kind == ReferenceKind::Content));
    }

    #[test]
    fn recognises_external_tilesets() {
        let scan = scan(
            r#"{
              "root": {
                "children": [
                  { "content": { "uri": "sub/tileset.json" } },
                  { "content": { "uri": "sub2/tileset.json?token=abc" } },
                  { "content": { "uri": "tiles/0.b3dm" } }
                ]
              }
            }"#,
        );

        assert_eq!(classify("sub/tileset.json"), ReferenceKind::Tileset);
        assert_eq!(classify("SUB/TILESET.JSON"), ReferenceKind::Tileset);
        assert_eq!(classify("tiles/0.b3dm"), ReferenceKind::Content);
        assert_eq!(classify("tiles/0.b3dm?v=1#x"), ReferenceKind::Content);

        assert_eq!(scan.references.len(), 3);
        assert_eq!(scan.references[0].kind, ReferenceKind::Tileset);
        assert_eq!(scan.references[1].kind, ReferenceKind::Tileset);
        assert_eq!(scan.references[2].kind, ReferenceKind::Content);
    }

    #[test]
    fn implicit_tiling_tiles_contribute_no_literal_uri() {
        let scan = scan(
            r#"{
              "root": {
                "content": { "uri": "content/{level}_{x}_{y}.b3dm" },
                "extensions": {
                  "3DTILES_implicit_tiling": {
                    "subdivisionScheme": "QUADTREE",
                    "subtreeLevels": 5,
                    "maximumLevel": 10,
                    "subtrees": { "uri": "subtrees/{level}_{x}_{y}.subtree" }
                  }
                },
                "children": [ { "content": { "uri": "explicit.b3dm" } } ]
              }
            }"#,
        );

        assert!(
            scan.references.is_empty() || scan.references.iter().all(|r| r.uri == "explicit.b3dm"),
            "the implicit content template must not be treated as a downloadable URI"
        );
        assert_eq!(scan.implicits.len(), 1);
        assert_eq!(
            scan.implicits[0].content_templates,
            vec!["content/{level}_{x}_{y}.b3dm".to_owned()]
        );
    }

    #[test]
    fn accepts_the_1_1_core_implicit_tiling_property() {
        // Real 3D Tiles 1.1 tilesets use the core property, not the 1.0
        // extension. Regression test for treating the content template as a
        // file name.
        let scan = scan(
            r#"{
              "asset": { "version": "1.1" },
              "root": {
                "content": { "uri": "content/content_{level}__{x}_{y}.glb" },
                "implicitTiling": {
                  "subdivisionScheme": "QUADTREE",
                  "subtreeLevels": 3,
                  "availableLevels": 6,
                  "subtrees": { "uri": "subtrees/{level}.{x}.{y}.subtree" }
                }
              }
            }"#,
        );

        assert!(
            scan.references.is_empty(),
            "the content template must not be downloaded literally, but got {:?}",
            scan.references
                .iter()
                .map(|reference| reference.uri.as_str())
                .collect::<Vec<_>>()
        );
        assert_eq!(scan.implicits.len(), 1);
        assert_eq!(
            scan.implicits[0].content_templates,
            vec!["content/content_{level}__{x}_{y}.glb".to_owned()]
        );
    }

    #[test]
    fn deeply_nested_tiles_are_walked_without_recursion() {
        // Each tile nests an object inside an array, and serde_json caps container
        // nesting at 128 levels, so ~60 tiles is as deep as a *parsed* document
        // can be. The walk is iterative regardless: no tile tree can overflow the
        // stack.
        const DEPTH: usize = 60;

        let mut json = String::from(r#"{"root":{"content":{"uri":"deep.b3dm"},"children":["#);
        for _ in 0..DEPTH {
            json.push_str(r#"{"content":{"uri":"deep.b3dm"},"children":["#);
        }
        // Close every inner tile, then the root's children array, the root, and
        // the wrapper object.
        json.push_str(&"]}".repeat(DEPTH));
        json.push_str("]}}");

        let tileset = Tileset::from_slice(json.as_bytes()).expect("parses");
        let scan = tileset.scan();
        assert_eq!(scan.references.len(), DEPTH + 1);
    }

    #[test]
    fn absurdly_deep_documents_are_rejected_before_the_walk() {
        // A hostile tileset cannot reach the walk at all: serde_json's recursion
        // limit rejects the document first.
        let mut json = String::from(r#"{"root":{"content":{"uri":"deep.b3dm"},"children":["#);
        for _ in 0..5_000 {
            json.push_str(r#"{"content":{"uri":"deep.b3dm"},"children":["#);
        }
        json.push_str(&"]}".repeat(5_000));
        json.push_str("]}}");

        assert!(Tileset::from_slice(json.as_bytes()).is_err());
    }
}
