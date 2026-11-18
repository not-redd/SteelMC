//! Reader for vanilla Anvil region files (`region/`, `entities/`, `poi/` `*.mca`).

use std::{
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
};

use flate2::read::{GzDecoder, ZlibDecoder};
use simdnbt::owned::NbtCompound;

use crate::{ConvertError, nbt};

const SECTOR_BYTES: usize = 4096;
const CHUNKS_PER_REGION: usize = 1024;
/// Set on the compression byte when the payload lives in a separate `c.<x>.<z>.mcc` file.
const EXTERNAL_FLAG: u8 = 0x80;

/// One vanilla region file, read fully into memory.
pub struct RegionFile {
    path: PathBuf,
    region_x: i32,
    region_z: i32,
    bytes: Vec<u8>,
}

impl RegionFile {
    /// Lists the `r.<x>.<z>.mca` files in `dir`. A missing directory has none.
    pub fn list(dir: &Path) -> Result<Vec<(i32, i32, PathBuf)>, ConvertError> {
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(ConvertError::io(dir, error)),
        };
        let mut regions = Vec::new();
        for entry in entries {
            let path = entry.map_err(|e| ConvertError::io(dir, e))?.path();
            let Some((x, z)) = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(parse_region_name)
            else {
                continue;
            };
            regions.push((x, z, path));
        }
        regions.sort_by_key(|&(x, z, _)| (x, z));
        Ok(regions)
    }

    /// Reads a region file.
    pub fn open(path: &Path, region_x: i32, region_z: i32) -> Result<Self, ConvertError> {
        let bytes = fs::read(path).map_err(|e| ConvertError::io(path, e))?;
        if !bytes.is_empty() && bytes.len() < 2 * SECTOR_BYTES {
            return Err(ConvertError::invalid(path, "region header is truncated"));
        }
        Ok(Self {
            path: path.to_path_buf(),
            region_x,
            region_z,
            bytes,
        })
    }

    /// The region file's path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Chunk positions with an entry in this region.
    pub fn chunk_positions(&self) -> impl Iterator<Item = (i32, i32)> + '_ {
        (0..CHUNKS_PER_REGION)
            .filter(|&index| self.location(index).is_some())
            .map(|index| {
                (
                    self.region_x * 32 + (index % 32) as i32,
                    self.region_z * 32 + (index / 32) as i32,
                )
            })
    }

    /// `(sector offset, sector count)` of a chunk, or `None` when the chunk is absent.
    fn location(&self, index: usize) -> Option<(usize, usize)> {
        let entry = self.bytes.get(index * 4..index * 4 + 4)?;
        let offset =
            usize::from(entry[0]) << 16 | usize::from(entry[1]) << 8 | usize::from(entry[2]);
        (offset != 0).then_some((offset, usize::from(entry[3])))
    }

    /// Reads and decompresses one chunk's NBT. Returns `None` if the chunk is absent.
    pub fn read_chunk(
        &self,
        chunk_x: i32,
        chunk_z: i32,
    ) -> Result<Option<NbtCompound>, ConvertError> {
        let index = (chunk_x.rem_euclid(32) + chunk_z.rem_euclid(32) * 32) as usize;
        let Some((sector, _)) = self.location(index) else {
            return Ok(None);
        };
        let invalid = |message: String| ConvertError::UnconvertibleChunk {
            path: self.path.clone(),
            chunk_x,
            chunk_z,
            message,
        };
        let start = sector * SECTOR_BYTES;
        let header = self
            .bytes
            .get(start..start + 5)
            .ok_or_else(|| invalid("chunk offset is past the end of the file".to_owned()))?;
        let length = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let compression = header[4];
        let payload = if compression & EXTERNAL_FLAG == 0 {
            if length == 0 {
                return Err(invalid("chunk has zero length".to_owned()));
            }
            self.bytes
                .get(start + 5..start + 4 + length)
                .ok_or_else(|| invalid("chunk payload is truncated".to_owned()))?
                .to_vec()
        } else {
            let external = self
                .path
                .with_file_name(format!("c.{chunk_x}.{chunk_z}.mcc"));
            fs::read(&external).map_err(|e| ConvertError::io(&external, e))?
        };
        let raw = decompress(compression & !EXTERNAL_FLAG, &payload).map_err(invalid)?;
        nbt::read_root(&raw).map(Some).map_err(invalid)
    }
}

fn parse_region_name(name: &str) -> Option<(i32, i32)> {
    let rest = name.strip_prefix("r.")?.strip_suffix(".mca")?;
    let (x, z) = rest.split_once('.')?;
    Some((x.parse().ok()?, z.parse().ok()?))
}

/// Vanilla `RegionFileVersion` ids.
fn decompress(compression: u8, payload: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let result = match compression {
        1 => GzDecoder::new(payload).read_to_end(&mut out),
        2 => ZlibDecoder::new(payload).read_to_end(&mut out),
        3 => return Ok(payload.to_vec()),
        4 => {
            return Err(
                "LZ4 region compression is not supported; set `region-file-compression=deflate` \
                 in server.properties and re-save the world in vanilla"
                    .to_owned(),
            );
        }
        other => return Err(format!("unknown region compression type {other}")),
    };
    result.map_err(|e| format!("failed to decompress chunk: {e}"))?;
    Ok(out)
}
