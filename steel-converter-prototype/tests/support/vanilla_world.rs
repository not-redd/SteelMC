//! Minimal reader for vanilla Anvil region files.
//!
//! This is the e2e oracle, so it shares no code with the converter: a bug in the converter's
//! own region reader must not be able to hide itself by producing the same wrong answer here.

use std::{
    collections::BTreeMap,
    fs,
    io::{Cursor, Read},
    path::{Path, PathBuf},
};

use flate2::read::{GzDecoder, ZlibDecoder};
use rustc_hash::FxHashMap;
use simdnbt::owned::{self, NbtCompound, NbtTag};

const SECTOR_BYTES: usize = 4096;
const BLOCKS_PER_SECTION: usize = 4096;

/// One vanilla dimension's `region/` directory with every region file kept in memory.
pub struct VanillaRegions {
    dir: PathBuf,
    files: FxHashMap<(i32, i32), Vec<u8>>,
}

/// A decoded vanilla chunk: its status and each section's block states in vanilla index order.
pub struct VanillaChunk {
    pub status: String,
    /// Sections keyed by section Y.
    sections: FxHashMap<i32, VanillaSection>,
}

struct VanillaSection {
    /// Canonical `namespace:id[key=value,...]` strings.
    palette: Vec<String>,
    /// `None` when the palette has a single entry.
    indices: Option<Vec<u16>>,
}

impl VanillaRegions {
    pub fn open(dir: &Path) -> Self {
        let mut files = FxHashMap::default();
        for entry in fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display())) {
            let path = entry.expect("region dir entry").path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let mut parts = name.split('.');
            let (Some("r"), Some(x), Some(z), Some("mca"), None) = (
                parts.next(),
                parts.next(),
                parts.next(),
                parts.next(),
                parts.next(),
            ) else {
                continue;
            };
            let (Ok(x), Ok(z)) = (x.parse(), z.parse()) else {
                continue;
            };
            let bytes = fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            files.insert((x, z), bytes);
        }
        Self {
            dir: dir.to_path_buf(),
            files,
        }
    }

    /// The saved `Status` of every chunk in the regions.
    pub fn chunk_statuses(&self) -> BTreeMap<(i32, i32), String> {
        let mut statuses = BTreeMap::new();
        for (&(region_x, region_z), bytes) in &self.files {
            for index in 0..1024 {
                if !chunk_present(bytes, index) {
                    continue;
                }
                let chunk_x = region_x * 32 + (index % 32) as i32;
                let chunk_z = region_z * 32 + (index / 32) as i32;
                let chunk = self
                    .read_chunk(chunk_x, chunk_z)
                    .expect("present chunk should decode");
                statuses.insert((chunk_x, chunk_z), chunk.status);
            }
        }
        statuses
    }

    pub fn read_chunk(&self, chunk_x: i32, chunk_z: i32) -> Option<VanillaChunk> {
        self.read_chunk_nbt(chunk_x, chunk_z)
            .map(|nbt| VanillaChunk::from_nbt(&nbt))
    }

    /// The chunk's root compound as vanilla saved it.
    pub fn read_chunk_nbt(&self, chunk_x: i32, chunk_z: i32) -> Option<NbtCompound> {
        let bytes = self
            .files
            .get(&(chunk_x.div_euclid(32), chunk_z.div_euclid(32)))?;
        let index = (chunk_x.rem_euclid(32) + chunk_z.rem_euclid(32) * 32) as usize;
        if !chunk_present(bytes, index) {
            return None;
        }
        let offset = usize::from(bytes[index * 4]) << 16
            | usize::from(bytes[index * 4 + 1]) << 8
            | usize::from(bytes[index * 4 + 2]);
        let start = offset * SECTOR_BYTES;
        let length =
            u32::from_be_bytes(bytes[start..start + 4].try_into().expect("4 bytes")) as usize;
        let compression = bytes[start + 4];
        let payload = if compression & 0x80 == 0 {
            bytes[start + 5..start + 4 + length].to_vec()
        } else {
            let path = self.dir.join(format!("c.{chunk_x}.{chunk_z}.mcc"));
            fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
        };
        let nbt = decompress(compression & 0x7F, &payload);
        Some(
            owned::read(&mut Cursor::new(&nbt[..]))
                .expect("chunk NBT")
                .unwrap()
                .as_compound(),
        )
    }
}

fn chunk_present(region: &[u8], index: usize) -> bool {
    region[index * 4..index * 4 + 3] != [0, 0, 0]
}

fn decompress(compression: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    match compression {
        1 => GzDecoder::new(payload).read_to_end(&mut out),
        2 => ZlibDecoder::new(payload).read_to_end(&mut out),
        3 => return payload.to_vec(),
        other => panic!("unsupported region compression {other}"),
    }
    .expect("decompress chunk");
    out
}

impl VanillaChunk {
    fn from_nbt(chunk: &NbtCompound) -> Self {
        let status = chunk.string("Status").expect("Status").to_string();
        let mut sections = FxHashMap::default();
        for section in chunk
            .list("sections")
            .and_then(|list| list.compounds())
            .unwrap_or_default()
        {
            let y = i32::from(section.byte("Y").expect("section Y"));
            let Some(block_states) = section.compound("block_states") else {
                continue;
            };
            let palette: Vec<String> = block_states
                .list("palette")
                .expect("block palette")
                .as_nbt_tags()
                .into_iter()
                .map(|entry| canonical_state(unwrap_list_element(entry)))
                .collect();
            let indices = block_states
                .long_array("data")
                .map(|data| unpack(data, palette.len()));
            sections.insert(y, VanillaSection { palette, indices });
        }
        Self { status, sections }
    }

    /// The block at local `x`/`z` and absolute `y`. Sections vanilla leaves out are air.
    pub fn block(&self, x: usize, y: i32, z: usize) -> &str {
        let Some(section) = self.sections.get(&y.div_euclid(16)) else {
            return "minecraft:air";
        };
        let local_y = y.rem_euclid(16) as usize;
        let index = local_y << 8 | z << 4 | x;
        match &section.indices {
            Some(indices) => &section.palette[usize::from(indices[index])],
            None => &section.palette[0],
        }
    }
}

/// Vanilla block-state storage: at least 4 bits, entries never span two longs.
fn unpack(data: &[i64], palette_len: usize) -> Vec<u16> {
    let bits = ((palette_len - 1).bit_width() as usize).max(4);
    let per_long = 64 / bits;
    assert_eq!(
        data.len(),
        BLOCKS_PER_SECTION.div_ceil(per_long),
        "vanilla block data length for {bits} bits"
    );
    let mask = (1u64 << bits) - 1;
    (0..BLOCKS_PER_SECTION)
        .map(|i| {
            let word = data[i / per_long] as u64;
            ((word >> ((i % per_long) * bits)) & mask) as u16
        })
        .collect()
}

/// Vanilla `ListTag` stores the strings of a list that also holds compounds as
/// `{"": <string>}` and unwraps them on load.
fn unwrap_list_element(tag: NbtTag) -> NbtTag {
    match tag {
        NbtTag::Compound(mut compound) if compound.len() == 1 && compound.contains("") => {
            compound.take("").expect("wrapper holds its element")
        }
        tag => tag,
    }
}

/// A 26.3 `BlockState.CODEC` palette entry: a bare block id for a default state, or
/// `{id, properties}` listing every property of any other state.
fn canonical_state(entry: NbtTag) -> String {
    let state = match entry {
        NbtTag::String(id) => return id.to_string(),
        NbtTag::Compound(state) => state,
        other => panic!("block palette entry is neither a string nor a compound: {other:?}"),
    };
    let name = state.string("id").expect("palette id").to_string();
    let mut properties: Vec<(String, String)> =
        state
            .compound("properties")
            .map_or_else(Vec::new, |properties| {
                properties
                    .iter()
                    .map(|(key, value)| {
                        (
                            key.to_string(),
                            value.string().expect("string property").to_string(),
                        )
                    })
                    .collect()
            });
    canonical(&name, &mut properties)
}

/// `namespace:id[key=value,...]` with properties sorted by key.
pub fn canonical(name: &str, properties: &mut [(String, String)]) -> String {
    properties.sort();
    if properties.is_empty() {
        return name.to_owned();
    }
    let joined: Vec<String> = properties.iter().map(|(k, v)| format!("{k}={v}")).collect();
    format!("{name}[{}]", joined.join(","))
}
