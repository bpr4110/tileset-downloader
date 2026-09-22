//! Implicit tiling: `3DTILES_implicit_tiling` (3D Tiles 1.0 extension) and
//! `tile.implicitTiling` (3D Tiles 1.1).
//!
//! An implicitly tiled subtree does not list its tiles. It ships a *piece of
//! tree* as a bitstream: which tiles exist, which of those have content, and
//! which child subtrees exist. To mirror the tileset you must decode those
//! bitstreams and walk the subtree graph yourself.
//!
//! Everything in this module follows the normative definitions:
//!
//! * **Bit order** — availability bitstreams are packed least-significant bit
//!   first: bit `i` is bit `i % 8` of byte `i / 8` (3D Metadata / 3D Tiles
//!   `Availability`). This is the single most important detail here; get it
//!   backwards and every tile is silently wrong.
//! * **Tile index** — tiles are ordered by level, then by Morton (Z-order)
//!   index within the level, and the index of a tile is
//!   `(N^level - 1) / (N - 1) + morton(x, y)`, where `N` is 4 for `QUADTREE`
//!   and 8 for `OCTREE`. A subtree's `tileAvailability` holds exactly
//!   `(N^subtreeLevels - 1) / (N - 1)` bits.
//! * **Child subtrees** — `childSubtreeAvailability` holds `N^subtreeLevels`
//!   bits, ordered by the Morton index of the child subtree root one level
//!   below the subtree's bottom row.
//! * **Templates** — `subtrees.uri` is templated with the *subtree root's*
//!   global level/x/y; content URI templates are templated with the *tile's*
//!   global level/x/y.
//! * **Morton order** — the exact interleaving used by CesiumJS
//!   (`MortonOrder.encode2D` / `encode3D`).
//!
//! Subtree documents come in two shapes and both are handled: a JSON document,
//! and a GLB-like binary with a `subt` magic, a JSON chunk, and an internal
//! binary chunk that the JSON's `bufferViews` index into.

use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, Ordering};

use base64::Engine as _;
use serde::Deserialize;
use serde_json::{Map, Value};
use url::Url;

use crate::error::{Error, Result};
use crate::net::Fetcher;
use crate::path_util::resolve;
use crate::tileset::ImplicitSource;

/// Extension key used by 3D Tiles 1.0 tilesets.
pub const MULTIPLE_CONTENTS_EXTENSION: &str = "3DTILES_multiple_contents";

/// Magic prefix of a binary subtree document.
const SUBTREE_MAGIC: &[u8; 4] = b"subt";

/// `magic(4) + version(4) + jsonByteLength(8) + binaryByteLength(8)`.
const SUBTREE_HEADER_LEN: usize = 24;

/// Levels beyond this cannot be addressed by a 32-bit Morton index, and the
/// shift arithmetic below would overflow.
const MAX_LEVELS: u64 = 31;

/// Which way a subtree subdivides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubdivisionScheme {
    /// Each tile has 4 children; Morton indices are 2D.
    Quadtree,
    /// Each tile has 8 children; Morton indices are 3D.
    Octree,
}

impl SubdivisionScheme {
    /// Parse the spec's string form.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "QUADTREE" => Some(Self::Quadtree),
            "OCTREE" => Some(Self::Octree),
            _ => None,
        }
    }

    /// 4 for a quadtree, 8 for an octree. Written `N` throughout the spec.
    pub fn branching_factor(self) -> u64 {
        match self {
            Self::Quadtree => 4,
            Self::Octree => 8,
        }
    }

    /// The largest `subtreeLevels` that still fits a 32-bit Morton index.
    ///
    /// CesiumJS documents 15 for quadtrees and 9 for octrees, leaving a level
    /// of headroom for child subtree coordinates.
    pub fn max_subtree_levels(self) -> u32 {
        match self {
            Self::Quadtree => 15,
            Self::Octree => 9,
        }
    }
}

/// A tile — or a subtree root — in the implicit tileset's global coordinates.
///
/// Level 0 is the tile that carried the implicit tiling extension; coordinates
/// are counted from there, not from the containing subtree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Coordinates {
    /// Level relative to the implicit tileset root, 0-indexed.
    pub level: u64,
    /// X index within the level.
    pub x: u64,
    /// Y index within the level.
    pub y: u64,
    /// Z index within the level. Always 0 for quadtrees.
    pub z: u64,
}

impl Coordinates {
    /// The root tile of an implicit tileset.
    pub fn root() -> Self {
        Self {
            level: 0,
            x: 0,
            y: 0,
            z: 0,
        }
    }

    /// The Morton (Z-order) index of these coordinates within their level.
    pub fn morton(self, scheme: SubdivisionScheme) -> u32 {
        match scheme {
            SubdivisionScheme::Quadtree => morton_2d(self.x as u32, self.y as u32),
            SubdivisionScheme::Octree => morton_3d(self.x as u32, self.y as u32, self.z as u32),
        }
    }
}

/// One implicit tileset: the extension object plus the document it came from.
#[derive(Debug)]
pub struct ImplicitTiling {
    /// URL of the document that declared the extension. Content and subtree URIs
    /// are relative to it, never to the subtree that names them.
    base: Url,
    /// Quadtree or octree.
    subdivision: SubdivisionScheme,
    /// Distinct levels covered by each subtree.
    subtree_levels: u32,
    /// How many levels of the tree hold tiles (`maximumLevel + 1`).
    available_levels: u64,
    /// Template for subtree files.
    subtree_uri_template: String,
    /// Templates for content files, one per content of the implicit tile.
    content_templates: Vec<String>,
    /// Guards the "content availability is missing" warning so it fires once
    /// per tileset rather than once per subtree.
    warned_missing_content: AtomicBool,
}

/// What one subtree document contributes to the download plan.
#[derive(Debug, Default)]
pub struct SubtreeExpansion {
    /// Content files, one per available tile that has content.
    pub content_urls: Vec<Url>,
    /// Child subtrees that exist, with their coordinates.
    pub child_subtrees: Vec<(Coordinates, Url)>,
    /// Buffers referenced by URI rather than held in the binary chunk.
    pub external_buffers: Vec<Url>,
    /// How many tiles the subtree declares available.
    pub available_tiles: usize,
}

impl ImplicitTiling {
    /// Parse the implicit tiling extension of one tile.
    pub fn parse(tileset_url: &Url, source: &ImplicitSource) -> Result<Self> {
        let raw = &source.raw;
        let url = tileset_url.to_string();

        let scheme_name = raw
            .get("subdivisionScheme")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let subdivision =
            SubdivisionScheme::parse(scheme_name).ok_or_else(|| Error::UnsupportedSubdivision {
                url: url.clone(),
                scheme: scheme_name.to_owned(),
            })?;

        let subtree_levels = raw
            .get("subtreeLevels")
            .and_then(Value::as_u64)
            .ok_or_else(|| Error::Implicit {
                url: url.clone(),
                detail: "`subtreeLevels` is missing".to_owned(),
            })?;
        let max_subtree_levels = u64::from(subdivision.max_subtree_levels());
        if subtree_levels == 0 || subtree_levels > max_subtree_levels {
            return Err(Error::Implicit {
                url,
                detail: format!(
                    "`subtreeLevels` is {subtree_levels}, outside the supported range 1..={max_subtree_levels}"
                ),
            });
        }

        // 3D Tiles 1.1 renamed `maximumLevel` to `availableLevels`, and made it
        // the count of levels rather than the highest level index.
        let available_levels = match raw.get("availableLevels").and_then(Value::as_u64) {
            Some(levels) => levels,
            None => raw
                .get("maximumLevel")
                .and_then(Value::as_u64)
                .map(|maximum| maximum + 1)
                .ok_or_else(|| Error::Implicit {
                    url: url.clone(),
                    detail: "neither `availableLevels` nor `maximumLevel` is present".to_owned(),
                })?,
        };
        if available_levels == 0 || available_levels > MAX_LEVELS {
            return Err(Error::Implicit {
                url,
                detail: format!(
                    "`availableLevels` is {available_levels}, expected 1..={MAX_LEVELS}"
                ),
            });
        }

        let subtree_uri_template = raw
            .get("subtrees")
            .and_then(|subtrees| subtrees.get("uri"))
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Implicit {
                url: url.clone(),
                detail: "`subtrees.uri` is missing".to_owned(),
            })?
            .to_owned();

        Ok(Self {
            base: tileset_url.clone(),
            subdivision,
            subtree_levels: subtree_levels as u32,
            available_levels,
            subtree_uri_template,
            content_templates: source.content_templates.clone(),
            warned_missing_content: AtomicBool::new(false),
        })
    }

    /// The root tile of this implicit tileset.
    pub fn root(&self) -> Coordinates {
        Coordinates::root()
    }

    /// Number of bits in a subtree's `tileAvailability`.
    pub fn tile_bit_count(&self) -> u64 {
        let branching = self.subdivision.branching_factor();
        (branching.pow(self.subtree_levels) - 1) / (branching - 1)
    }

    /// Resolve the subtree file for a subtree root's coordinates.
    pub fn subtree_url(&self, coordinates: Coordinates) -> Result<Url> {
        resolve(
            &self.base,
            &substitute(&self.subtree_uri_template, coordinates),
        )
    }

    /// Decode one subtree document and enumerate everything it makes available.
    ///
    /// Fetching happens only for external buffers, which are rare (the
    /// `bufferViews` of a normal subtree index its own binary chunk).
    pub fn expand_subtree(
        &self,
        fetcher: &Fetcher,
        coordinates: Coordinates,
        subtree_url: &Url,
        bytes: &[u8],
    ) -> Result<SubtreeExpansion> {
        let (json, binary) = split_subtree_document(bytes, subtree_url)?;
        let document: SubtreeDocument =
            serde_json::from_slice(json).map_err(|source| Error::Json {
                url: subtree_url.to_string(),
                source,
            })?;

        let mut expansion = SubtreeExpansion::default();

        let buffers = resolve_buffers(
            fetcher,
            subtree_url,
            &document,
            binary,
            &mut expansion.external_buffers,
        )?;
        let views = buffer_view_slices(&document, &buffers, subtree_url)?;

        let branching = self.subdivision.branching_factor();
        let tile_bits = self.tile_bit_count();
        let child_bits = branching.pow(self.subtree_levels);

        let tile_availability = read_availability(
            &document.tile_availability,
            tile_bits,
            &views,
            subtree_url,
            "tileAvailability",
        )?;

        // Content availability is optional in the schema. Its absence is
        // ambiguous, and silently downloading nothing is the worst possible
        // outcome for a downloader, so fall back to tile availability and say
        // so loudly.
        let mut assumed_content = false;
        let content_availability = match document.content_availability() {
            Some(entries) => entries
                .iter()
                .enumerate()
                .map(|(index, raw)| {
                    read_availability(
                        raw,
                        tile_bits,
                        &views,
                        subtree_url,
                        &format!("contentAvailability[{index}]"),
                    )
                })
                .collect::<Result<Vec<_>>>()?,
            None => {
                assumed_content = true;
                vec![tile_availability.clone()]
            }
        };

        let child_availability = read_availability(
            &document.child_subtree_availability,
            child_bits,
            &views,
            subtree_url,
            "childSubtreeAvailability",
        )?;

        if assumed_content && !self.warned_missing_content.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                url = %subtree_url,
                "subtree has no `contentAvailability`; assuming every available tile has content"
            );
        }

        // A subtree may override the tileset's content template.
        let override_template = document.content_uri.clone();
        let templates: Vec<&str> = match override_template.as_deref() {
            Some(uri) => vec![uri],
            None => self.content_templates.iter().map(String::as_str).collect(),
        };

        expansion.available_tiles = self.collect_tiles(
            coordinates,
            &tile_availability,
            &content_availability,
            &templates,
            &mut expansion.content_urls,
        );

        self.collect_child_subtrees(
            coordinates,
            &child_availability,
            document.subtree_uri.as_deref(),
            subtree_url,
            &mut expansion.child_subtrees,
        )?;

        Ok(expansion)
    }

    /// Resolve one content template for a specific tile.
    pub fn content_url(&self, coordinates: Coordinates, template: &str) -> Result<Url> {
        resolve(&self.base, &substitute(template, coordinates))
    }

    /// Walk every tile of one subtree, appending content URLs.
    fn collect_tiles(
        &self,
        root: Coordinates,
        tile_availability: &Availability,
        content_availability: &[Availability],
        templates: &[&str],
        out: &mut Vec<Url>,
    ) -> usize {
        let branching = self.subdivision.branching_factor();
        let mut available = 0usize;

        if templates.is_empty() && !content_availability.is_empty() {
            tracing::debug!(
                level = root.level,
                "subtree has content availability but the implicit tile declares no content URI"
            );
        }

        for local_level in 0..self.subtree_levels {
            let level = root.level + u64::from(local_level);
            if level >= self.available_levels {
                break;
            }

            let offset = level_offset(branching, u64::from(local_level));
            let level_bits = branching.pow(local_level);

            for morton in 0..level_bits {
                let index = offset + morton;
                if !tile_availability.get(index) {
                    continue;
                }
                available += 1;

                let (local_x, local_y, local_z) = decode_local(self.subdivision, morton as u32);
                let tile = Coordinates {
                    level,
                    x: (root.x << local_level) + local_x,
                    y: (root.y << local_level) + local_y,
                    z: (root.z << local_level) + local_z,
                };

                for (content_index, availability) in content_availability.iter().enumerate() {
                    if !availability.get(index) {
                        continue;
                    }
                    let Some(template) = content_template(templates, content_index) else {
                        continue;
                    };
                    match self.content_url(tile, template) {
                        Ok(url) => out.push(url),
                        Err(error) => tracing::warn!(
                            %error,
                            template,
                            "skipping a content URI that does not resolve"
                        ),
                    }
                }
            }
        }

        available
    }

    /// Enumerate child subtrees from `childSubtreeAvailability`.
    fn collect_child_subtrees(
        &self,
        root: Coordinates,
        child_availability: &Availability,
        subtree_uri_override: Option<&str>,
        current_url: &Url,
        out: &mut Vec<(Coordinates, Url)>,
    ) -> Result<()> {
        let branching = self.subdivision.branching_factor();
        let level = root.level + u64::from(self.subtree_levels);

        // Past the last available level there is nothing left to subdivide into.
        if level >= self.available_levels {
            return Ok(());
        }

        let child_bits = branching.pow(self.subtree_levels);
        for morton in 0..child_bits {
            if !child_availability.get(morton) {
                continue;
            }

            let (local_x, local_y, local_z) = decode_local(self.subdivision, morton as u32);
            let child = Coordinates {
                level,
                x: (root.x << self.subtree_levels) + local_x,
                y: (root.y << self.subtree_levels) + local_y,
                z: (root.z << self.subtree_levels) + local_z,
            };

            let url = match subtree_uri_override {
                Some(template) => resolve(current_url, &substitute(template, child))?,
                None => self.subtree_url(child)?,
            };
            out.push((child, url));
        }

        Ok(())
    }
}

/// The `content.uri` template for one content index.
fn content_template<'a>(templates: &'a [&'a str], index: usize) -> Option<&'a str> {
    match templates.get(index) {
        Some(template) => Some(template),
        // Some producers emit one template and a multi-element availability
        // array. Prefer the template we have over dropping the tile.
        None if templates.len() == 1 => Some(templates[0]),
        None => None,
    }
}

/// Fill a template URI's `{level}`/`{x}`/`{y}`/`{z}` variables.
fn substitute(template: &str, coordinates: Coordinates) -> String {
    if !template.contains('{') {
        return template.to_owned();
    }
    template
        .replace("{level}", &coordinates.level.to_string())
        .replace("{x}", &coordinates.x.to_string())
        .replace("{y}", &coordinates.y.to_string())
        .replace("{z}", &coordinates.z.to_string())
}

/// First bit index at a level: `(N^level - 1) / (N - 1)`.
fn level_offset(branching: u64, level: u64) -> u64 {
    (branching.pow(level as u32) - 1) / (branching - 1)
}

/// Split a subtree document into its JSON and binary chunks.
///
/// Spec subtrees are either plain JSON or the binary container; the magic tells
/// them apart. The chunk lengths are `uint64` in the container, but CesiumJS
/// reads only the low 32 bits, and so do we — anything larger does not fit in
/// memory anyway.
fn split_subtree_document<'a>(bytes: &'a [u8], url: &Url) -> Result<(&'a [u8], &'a [u8])> {
    if !bytes.starts_with(SUBTREE_MAGIC) {
        return Ok((bytes, &[]));
    }

    if bytes.len() < SUBTREE_HEADER_LEN {
        return Err(Error::Availability {
            url: url.to_string(),
            detail: format!(
                "binary subtree is {} bytes, shorter than the {SUBTREE_HEADER_LEN}-byte header",
                bytes.len()
            ),
        });
    }

    let json_length = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
    let binary_length = u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]) as usize;

    let json_end = SUBTREE_HEADER_LEN
        .checked_add(json_length)
        .ok_or_else(|| malformed(url, "jsonByteLength overflows"))?;
    let binary_end = json_end
        .checked_add(binary_length)
        .ok_or_else(|| malformed(url, "binaryByteLength overflows"))?;
    if binary_end > bytes.len() {
        return Err(malformed(
            url,
            &format!(
                "header claims {binary_end} bytes but the document is {} bytes",
                bytes.len()
            ),
        ));
    }

    Ok((
        &bytes[SUBTREE_HEADER_LEN..json_end],
        &bytes[json_end..binary_end],
    ))
}

fn malformed(url: &Url, detail: &str) -> Error {
    Error::Availability {
        url: url.to_string(),
        detail: detail.to_owned(),
    }
}

/// The JSON chunk of a subtree document.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubtreeDocument {
    #[serde(default)]
    buffers: Vec<BufferHeader>,
    #[serde(default)]
    buffer_views: Vec<BufferViewHeader>,
    tile_availability: Value,
    #[serde(default)]
    content_availability: Option<Vec<Value>>,
    child_subtree_availability: Value,
    /// 3D Tiles 1.0 draft: overrides the tileset's content template.
    #[serde(default)]
    content_uri: Option<String>,
    /// 3D Tiles 1.0 draft: overrides the tileset's subtree template.
    #[serde(default)]
    subtree_uri: Option<String>,
    #[serde(default)]
    extensions: Option<Map<String, Value>>,
}

impl SubtreeDocument {
    /// Content availability in either the 1.1 array form or the 1.0
    /// `3DTILES_multiple_contents` extension form.
    fn content_availability(&self) -> Option<&Vec<Value>> {
        if let Some(entries) = &self.content_availability {
            return Some(entries);
        }
        self.extensions
            .as_ref()?
            .get(MULTIPLE_CONTENTS_EXTENSION)?
            .get("contentAvailability")?
            .as_array()
    }
}

#[derive(Debug, Deserialize)]
struct BufferHeader {
    #[serde(default)]
    uri: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BufferViewHeader {
    buffer: usize,
    #[serde(default)]
    byte_offset: usize,
    byte_length: usize,
}

/// Load every buffer a subtree references.
///
/// Buffer 0 without a `uri` is the document's own binary chunk, following the
/// GLB convention.
fn resolve_buffers<'a>(
    fetcher: &Fetcher,
    subtree_url: &Url,
    document: &SubtreeDocument,
    binary: &'a [u8],
    external: &mut Vec<Url>,
) -> Result<Vec<Cow<'a, [u8]>>> {
    let mut buffers = Vec::with_capacity(document.buffers.len());

    for (index, header) in document.buffers.iter().enumerate() {
        match header.uri.as_deref() {
            Some(uri) => {
                let url = resolve(subtree_url, uri)?;
                let bytes = fetcher.get_bytes(&url)?;
                external.push(url);
                buffers.push(Cow::Owned(bytes));
            }
            None if index == 0 => buffers.push(Cow::Borrowed(binary)),
            None => {
                return Err(malformed(
                    subtree_url,
                    &format!("buffer {index} has no `uri`; only buffer 0 may be the binary chunk"),
                ))
            }
        }
    }

    Ok(buffers)
}

/// Slice each buffer view out of its buffer.
fn buffer_view_slices<'a>(
    document: &SubtreeDocument,
    buffers: &'a [Cow<'a, [u8]>],
    subtree_url: &Url,
) -> Result<Vec<&'a [u8]>> {
    let mut views = Vec::with_capacity(document.buffer_views.len());

    for (index, header) in document.buffer_views.iter().enumerate() {
        let buffer = buffers.get(header.buffer).ok_or_else(|| {
            malformed(
                subtree_url,
                &format!(
                    "bufferView {index} refers to missing buffer {}",
                    header.buffer
                ),
            )
        })?;

        let end = header
            .byte_offset
            .checked_add(header.byte_length)
            .ok_or_else(|| {
                malformed(subtree_url, &format!("bufferView {index} length overflows"))
            })?;
        let slice = buffer.get(header.byte_offset..end).ok_or_else(|| {
            malformed(
                subtree_url,
                &format!(
                    "bufferView {index} needs bytes {}..{end} but buffer {} is {} bytes",
                    header.byte_offset,
                    header.buffer,
                    buffer.len()
                ),
            )
        })?;
        views.push(slice);
    }

    Ok(views)
}

/// An availability bitstream, in either of its two forms.
#[derive(Debug, Clone)]
enum Availability {
    /// One value for every element.
    Constant(bool),
    /// Packed bits, least-significant bit first.
    Bits(Vec<u8>),
}

impl Availability {
    /// Read bit `index`.
    fn get(&self, index: u64) -> bool {
        match self {
            Self::Constant(value) => *value,
            Self::Bits(bytes) => {
                let byte = bytes.get((index >> 3) as usize);
                match byte {
                    Some(byte) => (byte >> (index % 8)) & 1 == 1,
                    None => false,
                }
            }
        }
    }
}

/// Turn one `Availability` JSON object into an [`Availability`].
///
/// Three spellings are accepted: `constant`, `bitstream` as a buffer-view index
/// (current schema), and `bitstream` as a base64 string (the 3D Tiles 1.0
/// draft), plus the older `bufferView` key.
fn read_availability(
    raw: &Value,
    length_bits: u64,
    views: &[&[u8]],
    url: &Url,
    what: &str,
) -> Result<Availability> {
    if let Some(constant) = raw.get("constant") {
        let value = constant
            .as_bool()
            .or_else(|| constant.as_u64().map(|value| value != 0))
            .unwrap_or(false);
        return Ok(Availability::Constant(value));
    }

    let expected_bytes = length_bits.div_ceil(8) as usize;

    if let Some(bitstream) = raw.get("bitstream") {
        if let Some(index) = bitstream.as_u64() {
            return buffer_view_availability(index as usize, expected_bytes, views, url, what);
        }
        if let Some(encoded) = bitstream.as_str() {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|error| {
                    malformed(
                        url,
                        &format!("{what}: `bitstream` is not valid base64 ({error})"),
                    )
                })?;
            if bytes.len() != expected_bytes {
                return Err(malformed(
                    url,
                    &format!(
                        "{what}: bitstream is {} bytes but {expected_bytes} are required for {length_bits} bits",
                        bytes.len()
                    ),
                ));
            }
            return Ok(Availability::Bits(bytes));
        }
        return Err(malformed(
            url,
            &format!("{what}: `bitstream` is neither a bufferView index nor a base64 string"),
        ));
    }

    if let Some(index) = raw.get("bufferView").and_then(Value::as_u64) {
        return buffer_view_availability(index as usize, expected_bytes, views, url, what);
    }

    Err(malformed(
        url,
        &format!("{what}: neither `bitstream` nor `constant` is present"),
    ))
}

fn buffer_view_availability(
    index: usize,
    expected_bytes: usize,
    views: &[&[u8]],
    url: &Url,
    what: &str,
) -> Result<Availability> {
    let bytes = views
        .get(index)
        .ok_or_else(|| malformed(url, &format!("{what}: bufferView {index} does not exist")))?;
    if bytes.len() != expected_bytes {
        return Err(malformed(
            url,
            &format!(
                "{what}: bufferView {index} is {} bytes but {expected_bytes} are required",
                bytes.len()
            ),
        ));
    }
    Ok(Availability::Bits(bytes.to_vec()))
}

// ---------------------------------------------------------------------------
// Morton order, ported from CesiumJS `MortonOrder.js`.
// ---------------------------------------------------------------------------

fn insert_one_spacing(mut value: u32) -> u32 {
    value = (value ^ (value << 8)) & 0x00ff_00ff;
    value = (value ^ (value << 4)) & 0x0f0f_0f0f;
    value = (value ^ (value << 2)) & 0x3333_3333;
    value = (value ^ (value << 1)) & 0x5555_5555;
    value
}

fn insert_two_spacing(mut value: u32) -> u32 {
    value = (value ^ (value << 16)) & 0x0300_00ff;
    value = (value ^ (value << 8)) & 0x0300_f00f;
    value = (value ^ (value << 4)) & 0x030c_30c3;
    value = (value ^ (value << 2)) & 0x0924_9249;
    value
}

fn remove_one_spacing(mut value: u32) -> u32 {
    value &= 0x5555_5555;
    value = (value ^ (value >> 1)) & 0x3333_3333;
    value = (value ^ (value >> 2)) & 0x0f0f_0f0f;
    value = (value ^ (value >> 4)) & 0x00ff_00ff;
    value = (value ^ (value >> 8)) & 0x0000_ffff;
    value
}

fn remove_two_spacing(mut value: u32) -> u32 {
    value &= 0x0924_9249;
    value = (value ^ (value >> 2)) & 0x030c_30c3;
    value = (value ^ (value >> 4)) & 0x0300_f00f;
    value = (value ^ (value >> 8)) & 0xff00_00ff;
    value = (value ^ (value >> 16)) & 0x0000_03ff;
    value
}

/// Interleave two coordinates into a 32-bit Morton index.
pub fn morton_2d(x: u32, y: u32) -> u32 {
    insert_one_spacing(x) | (insert_one_spacing(y) << 1)
}

/// Interleave three coordinates into a 30-bit Morton index.
pub fn morton_3d(x: u32, y: u32, z: u32) -> u32 {
    insert_two_spacing(x) | (insert_two_spacing(y) << 1) | (insert_two_spacing(z) << 2)
}

fn decode_2d(morton: u32) -> (u32, u32) {
    (remove_one_spacing(morton), remove_one_spacing(morton >> 1))
}

fn decode_3d(morton: u32) -> (u32, u32, u32) {
    (
        remove_two_spacing(morton),
        remove_two_spacing(morton >> 1),
        remove_two_spacing(morton >> 2),
    )
}

/// Local coordinates at a level, from a Morton index within that level.
fn decode_local(scheme: SubdivisionScheme, morton: u32) -> (u64, u64, u64) {
    match scheme {
        SubdivisionScheme::Quadtree => {
            let (x, y) = decode_2d(morton);
            (u64::from(x), u64::from(y), 0)
        }
        SubdivisionScheme::Octree => {
            let (x, y, z) = decode_3d(morton);
            (u64::from(x), u64::from(y), u64::from(z))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(input: &str) -> Url {
        Url::parse(input).expect("test URL")
    }

    fn make_tiling(extension: serde_json::Value, templates: &[&str]) -> ImplicitTiling {
        let source = ImplicitSource {
            raw: extension,
            content_templates: templates.iter().map(|t| (*t).to_owned()).collect(),
        };
        ImplicitTiling::parse(&url("https://example.com/tiles/tileset.json"), &source)
            .expect("extension should parse")
    }

    fn quadtree(levels: u64, available_levels: u64, template: &str) -> ImplicitTiling {
        make_tiling(
            serde_json::json!({
                "subdivisionScheme": "QUADTREE",
                "subtreeLevels": levels,
                "availableLevels": available_levels,
                "subtrees": { "uri": "subtrees/{level}_{x}_{y}.subtree" }
            }),
            &[template],
        )
    }

    /// Build a binary subtree document from a JSON chunk and a binary chunk.
    fn binary_subtree(json: &str, binary: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"subt");
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&(json.len() as u64).to_le_bytes());
        out.extend_from_slice(&(binary.len() as u64).to_le_bytes());
        out.extend_from_slice(json.as_bytes());
        out.extend_from_slice(binary);
        out
    }

    #[test]
    fn morton_round_trips() {
        for level in 0..7u32 {
            let dimension = 1u32 << level;
            for x in 0..dimension {
                for y in 0..dimension {
                    assert_eq!(decode_2d(morton_2d(x, y)), (x, y), "2D at {x},{y}");
                    assert_eq!(decode_3d(morton_3d(x, y, 0)), (x, y, 0), "3D at {x},{y},0");
                }
            }
        }
    }

    #[test]
    fn morton_matches_the_spec_example() {
        // Z-order for a quadtree: (0,0)=0, (1,0)=1, (0,1)=2, (1,1)=3.
        assert_eq!(morton_2d(0, 0), 0);
        assert_eq!(morton_2d(1, 0), 1);
        assert_eq!(morton_2d(0, 1), 2);
        assert_eq!(morton_2d(1, 1), 3);
    }

    #[test]
    fn bit_counts_match_the_schema() {
        // "a quadtree with subtreeLevels = 2 will have 5 nodes (1 root and 4 children)"
        let tiling = quadtree(2, 3, "content/{level}_{x}_{y}.b3dm");
        assert_eq!(tiling.tile_bit_count(), 5);

        let octree = make_tiling(
            serde_json::json!({
                "subdivisionScheme": "OCTREE",
                "subtreeLevels": 2,
                "availableLevels": 3,
                "subtrees": { "uri": "subtrees/{level}_{x}_{y}_{z}.subtree" }
            }),
            &[],
        );
        // (8^2 - 1) / 7 = 9
        assert_eq!(octree.tile_bit_count(), 9);
    }

    #[test]
    fn level_offsets_follow_the_formula() {
        assert_eq!(level_offset(4, 0), 0);
        assert_eq!(level_offset(4, 1), 1);
        assert_eq!(level_offset(4, 2), 5);
        assert_eq!(level_offset(8, 2), 9);
    }

    #[test]
    fn templating_uses_global_coordinates() {
        let tiling = quadtree(2, 3, "content/{level}_{x}_{y}.b3dm");
        let coordinates = Coordinates {
            level: 4,
            x: 5,
            y: 6,
            z: 0,
        };
        assert_eq!(
            tiling
                .content_url(coordinates, "content/{level}_{x}_{y}.b3dm")
                .expect("resolves")
                .as_str(),
            "https://example.com/tiles/content/4_5_6.b3dm"
        );
        assert_eq!(
            tiling.subtree_url(coordinates).expect("resolves").as_str(),
            "https://example.com/tiles/subtrees/4_5_6.subtree"
        );
    }

    #[test]
    fn expands_a_constant_quadtree_subtree() {
        let tiling = quadtree(2, 4, "content/{level}_{x}_{y}.b3dm");
        let json = serde_json::json!({
            "tileAvailability": { "constant": 1 },
            "contentAvailability": [ { "constant": 1 } ],
            "childSubtreeAvailability": { "constant": 0 }
        });

        let fetcher = Fetcher::new(&crate::net::ClientConfig::default()).expect("client");
        let expansion = tiling
            .expand_subtree(
                &fetcher,
                Coordinates::root(),
                &url("https://example.com/tiles/subtrees/0_0_0.subtree"),
                json.to_string().as_bytes(),
            )
            .expect("expands");

        assert_eq!(expansion.available_tiles, 5, "1 root + 4 children");
        let uris: Vec<String> = expansion
            .content_urls
            .iter()
            .map(|url| url.path().to_owned())
            .collect();
        assert_eq!(
            uris,
            vec![
                "/tiles/content/0_0_0.b3dm",
                "/tiles/content/1_0_0.b3dm",
                "/tiles/content/1_1_0.b3dm",
                "/tiles/content/1_0_1.b3dm",
                "/tiles/content/1_1_1.b3dm",
            ]
        );
        assert!(expansion.child_subtrees.is_empty());
    }

    #[test]
    fn respects_available_levels() {
        let tiling = quadtree(2, 1, "content/{level}_{x}_{y}.b3dm");
        let json = serde_json::json!({
            "tileAvailability": { "constant": 1 },
            "contentAvailability": [ { "constant": 1 } ],
            "childSubtreeAvailability": { "constant": 1 }
        });

        let fetcher = Fetcher::new(&crate::net::ClientConfig::default()).expect("client");
        let expansion = tiling
            .expand_subtree(
                &fetcher,
                Coordinates::root(),
                &url("https://example.com/tiles/subtrees/0_0_0.subtree"),
                json.to_string().as_bytes(),
            )
            .expect("expands");

        assert_eq!(expansion.available_tiles, 1, "only level 0 is available");
        assert_eq!(expansion.content_urls.len(), 1);
        assert!(
            expansion.child_subtrees.is_empty(),
            "level 2 is past availableLevels"
        );
    }

    #[test]
    fn enumerates_child_subtrees_in_morton_order() {
        let tiling = quadtree(1, 5, "content/{level}_{x}_{y}.b3dm");
        let json = serde_json::json!({
            "tileAvailability": { "constant": 1 },
            "contentAvailability": [ { "constant": 1 } ],
            "childSubtreeAvailability": { "constant": 1 }
        });

        let fetcher = Fetcher::new(&crate::net::ClientConfig::default()).expect("client");
        let expansion = tiling
            .expand_subtree(
                &fetcher,
                Coordinates::root(),
                &url("https://example.com/tiles/subtrees/0_0_0.subtree"),
                json.to_string().as_bytes(),
            )
            .expect("expands");

        let paths: Vec<String> = expansion
            .child_subtrees
            .iter()
            .map(|(_, url)| url.path().to_owned())
            .collect();
        assert_eq!(
            paths,
            vec![
                "/tiles/subtrees/1_0_0.subtree",
                "/tiles/subtrees/1_1_0.subtree",
                "/tiles/subtrees/1_0_1.subtree",
                "/tiles/subtrees/1_1_1.subtree",
            ]
        );
        assert_eq!(expansion.child_subtrees[3].0.x, 1);
        assert_eq!(expansion.child_subtrees[3].0.y, 1);
    }

    #[test]
    fn reads_a_binary_subtree_bitstream() {
        let tiling = quadtree(2, 3, "content/{level}_{x}_{y}.b3dm");
        // Only the subtree root (index 0) is available: that is bit 0 of byte 0,
        // i.e. the least significant bit. Getting this wrong is the classic bug.
        let document = binary_subtree(
            r#"{
                "buffers": [ { "byteLength": 1 } ],
                "bufferViews": [ { "buffer": 0, "byteOffset": 0, "byteLength": 1 } ],
                "tileAvailability": { "bitstream": 0 },
                "contentAvailability": [ { "constant": 1 } ],
                "childSubtreeAvailability": { "constant": 0 }
            }"#,
            &[0b0000_0001],
        );

        let fetcher = Fetcher::new(&crate::net::ClientConfig::default()).expect("client");
        let expansion = tiling
            .expand_subtree(
                &fetcher,
                Coordinates::root(),
                &url("https://example.com/tiles/subtrees/0_0_0.subtree"),
                &document,
            )
            .expect("expands");

        assert_eq!(expansion.available_tiles, 1);
        assert_eq!(expansion.content_urls.len(), 1);
        assert!(expansion.content_urls[0]
            .path()
            .ends_with("content/0_0_0.b3dm"));
    }

    #[test]
    fn reads_a_base64_bitstream() {
        // 0b00001001 -> bits 0 and 3 set. Base64 of that single byte is "CQ==".
        let tiling = quadtree(2, 3, "content/{level}_{x}_{y}.b3dm");
        let json = serde_json::json!({
            "tileAvailability": { "bitstream": "CQ==" },
            "contentAvailability": [ { "bitstream": "CQ==" } ],
            "childSubtreeAvailability": { "constant": 0 }
        });

        let fetcher = Fetcher::new(&crate::net::ClientConfig::default()).expect("client");
        let expansion = tiling
            .expand_subtree(
                &fetcher,
                Coordinates::root(),
                &url("https://example.com/tiles/subtrees/0_0_0.subtree"),
                json.to_string().as_bytes(),
            )
            .expect("expands");

        // Bit 0 is level 0 (0,0); bit 3 is tile index 3, which is level 1,
        // Morton index 2, decoding to (x=0, y=1).
        let paths: Vec<String> = expansion
            .content_urls
            .iter()
            .map(|url| url.path().to_owned())
            .collect();
        assert_eq!(
            paths,
            vec!["/tiles/content/0_0_0.b3dm", "/tiles/content/1_0_1.b3dm"]
        );
    }

    #[test]
    fn missing_content_availability_falls_back_to_tile_availability() {
        // subtreeLevels = 1 means a subtree holds one tile: its own root.
        let tiling = quadtree(1, 2, "content/{level}_{x}_{y}.b3dm");
        let json = serde_json::json!({
            "tileAvailability": { "constant": 1 },
            "childSubtreeAvailability": { "constant": 0 }
        });

        let fetcher = Fetcher::new(&crate::net::ClientConfig::default()).expect("client");
        let expansion = tiling
            .expand_subtree(
                &fetcher,
                Coordinates::root(),
                &url("https://example.com/tiles/subtrees/0_0_0.subtree"),
                json.to_string().as_bytes(),
            )
            .expect("expands");

        assert_eq!(expansion.available_tiles, 1);
        assert_eq!(
            expansion.content_urls.len(),
            1,
            "an absent contentAvailability must not silently download nothing"
        );
    }

    #[test]
    fn rejects_a_bitstream_of_the_wrong_length() {
        let tiling = quadtree(2, 3, "content/{level}_{x}_{y}.b3dm");
        // 5 bits are required, so a 1-byte bitstream is fine; 4 bytes is not.
        let json = serde_json::json!({
            "buffers": [ { "byteLength": 4 } ],
            "bufferViews": [ { "buffer": 0, "byteOffset": 0, "byteLength": 4 } ],
            "tileAvailability": { "bitstream": 0 },
            "childSubtreeAvailability": { "constant": 0 }
        });
        let document = binary_subtree(&json.to_string(), &[0, 0, 0, 0]);

        let fetcher = Fetcher::new(&crate::net::ClientConfig::default()).expect("client");
        let error = tiling
            .expand_subtree(
                &fetcher,
                Coordinates::root(),
                &url("https://example.com/tiles/subtrees/0_0_0.subtree"),
                &document,
            )
            .expect_err("must reject");
        assert!(matches!(error, Error::Availability { .. }), "got {error:?}");
    }

    #[test]
    fn rejects_unsupported_subdivision_schemes() {
        let error = ImplicitTiling::parse(
            &url("https://example.com/tileset.json"),
            &ImplicitSource {
                raw: serde_json::json!({
                    "subdivisionScheme": "HEXTREE",
                    "subtreeLevels": 1,
                    "availableLevels": 1,
                    "subtrees": { "uri": "s/{level}_{x}_{y}.subtree" }
                }),
                content_templates: Vec::new(),
            },
        )
        .expect_err("must reject");
        assert!(matches!(error, Error::UnsupportedSubdivision { .. }));
    }

    #[test]
    fn rejects_oversized_subtree_levels() {
        let error = ImplicitTiling::parse(
            &url("https://example.com/tileset.json"),
            &ImplicitSource {
                raw: serde_json::json!({
                    "subdivisionScheme": "QUADTREE",
                    "subtreeLevels": 20,
                    "availableLevels": 21,
                    "subtrees": { "uri": "s/{level}_{x}_{y}.subtree" }
                }),
                content_templates: Vec::new(),
            },
        )
        .expect_err("must reject");
        assert!(matches!(error, Error::Implicit { .. }));
    }

    #[test]
    fn accepts_the_1_0_maximum_level_spelling() {
        let tiling = make_tiling(
            serde_json::json!({
                "subdivisionScheme": "QUADTREE",
                "subtreeLevels": 2,
                "maximumLevel": 3,
                "subtrees": { "uri": "subtrees/{level}_{x}_{y}.subtree" }
            }),
            &["content/{level}_{x}_{y}.b3dm"],
        );
        assert_eq!(tiling.available_levels, 4);
    }

    #[test]
    fn a_subtree_content_uri_overrides_the_tileset_template() {
        let tiling = quadtree(1, 2, "content/{level}_{x}_{y}.b3dm");
        let json = serde_json::json!({
            "tileAvailability": { "constant": 1 },
            "contentAvailability": [ { "constant": 1 } ],
            "childSubtreeAvailability": { "constant": 0 },
            "contentUri": "override/{level}.b3dm"
        });

        let fetcher = Fetcher::new(&crate::net::ClientConfig::default()).expect("client");
        let expansion = tiling
            .expand_subtree(
                &fetcher,
                Coordinates::root(),
                &url("https://example.com/tiles/subtrees/0_0_0.subtree"),
                json.to_string().as_bytes(),
            )
            .expect("expands");

        assert!(expansion.content_urls[0]
            .path()
            .starts_with("/tiles/override/"));
    }

    #[test]
    fn plain_json_subtrees_are_accepted() {
        // No `subt` magic: the document is JSON with no binary chunk.
        let (json, binary) = split_subtree_document(
            br#"{"tileAvailability":{"constant":1}}"#,
            &url("https://example.com/s.subtree"),
        )
        .expect("splits");
        assert!(!json.is_empty());
        assert!(binary.is_empty());
    }

    #[test]
    fn reads_a_legacy_subtree_uri_override() {
        let tiling = quadtree(1, 5, "content/{level}_{x}_{y}.b3dm");
        let json = serde_json::json!({
            "tileAvailability": { "constant": 1 },
            "contentAvailability": [ { "constant": 1 } ],
            "childSubtreeAvailability": { "constant": 1 },
            "subtreeUri": "other/{level}_{x}_{y}.subtree"
        });

        let fetcher = Fetcher::new(&crate::net::ClientConfig::default()).expect("client");
        let expansion = tiling
            .expand_subtree(
                &fetcher,
                Coordinates::root(),
                &url("https://example.com/tiles/subtrees/0_0_0.subtree"),
                json.to_string().as_bytes(),
            )
            .expect("expands");

        // The override is relative to the subtree file, not the tileset.
        assert!(expansion.child_subtrees[0]
            .1
            .as_str()
            .starts_with("https://example.com/tiles/subtrees/other/"));
    }
}
