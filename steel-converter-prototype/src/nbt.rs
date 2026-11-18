//! Helpers for reading vanilla NBT and writing Steel's NBT byte vectors.

use std::{
    fs,
    io::{self, Cursor, Read},
    path::Path,
};

use flate2::read::GzDecoder;
use simdnbt::owned::{self, NbtCompound, NbtList, NbtTag};

use crate::{ConvertError, version};

/// Reads a gzip-compressed vanilla NBT file and returns its root compound.
pub fn read_gzip_file(path: &Path) -> Result<NbtCompound, ConvertError> {
    let compressed = fs::read(path).map_err(|e| ConvertError::io(path, e))?;
    let mut raw = Vec::new();
    GzDecoder::new(&compressed[..])
        .read_to_end(&mut raw)
        .map_err(|e| ConvertError::invalid(path, format!("not a gzip NBT file: {e}")))?;
    read_root(&raw).map_err(|message| ConvertError::invalid(path, message))
}

/// Parses an uncompressed named root compound.
pub fn read_root(raw: &[u8]) -> Result<NbtCompound, String> {
    match owned::read(&mut Cursor::new(raw)) {
        Ok(owned::Nbt::Some(root)) => Ok(root.as_compound()),
        Ok(owned::Nbt::None) => Err("empty NBT root".to_owned()),
        Err(error) => Err(format!("invalid NBT: {error}")),
    }
}

/// Reads a vanilla saved-data file (`{data: {...}, DataVersion}`) and returns its `data`.
///
/// A missing file is `None`, the same as vanilla, which treats it as empty saved data.
pub fn read_saved_data(path: &Path) -> Result<Option<NbtCompound>, ConvertError> {
    match fs::metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(ConvertError::io(path, error)),
    }
    let mut root = read_gzip_file(path)?;
    version::check_data_version(path, &root)?;
    let data = match root.take("data") {
        Some(NbtTag::Compound(data)) => data,
        Some(_) => return Err(ConvertError::invalid(path, "`data` is not a compound")),
        None => NbtCompound::new(),
    };
    Ok(Some(data))
}

/// Encodes a compound the way Steel stores NBT: its entries followed by `TAG_End`, with no
/// root type byte or name (`WORLD_STORAGE.md` §0.3).
pub fn compound_body(compound: &NbtCompound) -> Vec<u8> {
    let mut bytes = Vec::new();
    compound.write(&mut bytes);
    bytes
}

/// Like [`compound_body`], but an empty compound is an empty vector, Steel's "absent" value
/// for optional entity NBT.
pub fn optional_compound_body(compound: &NbtCompound) -> Vec<u8> {
    if compound.is_empty() {
        Vec::new()
    } else {
        compound_body(compound)
    }
}

/// The compounds of a list, treating a missing key or an empty list as no entries.
pub fn compounds<'a>(compound: &'a NbtCompound, key: &str) -> &'a [NbtCompound] {
    compound
        .list(key)
        .and_then(NbtList::compounds)
        .unwrap_or_default()
}

/// A string value as an owned `String`.
pub fn string(compound: &NbtCompound, key: &str) -> Option<String> {
    compound
        .string(key)
        .map(|value| value.to_str().into_owned())
}

/// A vanilla boolean, stored as a byte.
pub fn boolean(compound: &NbtCompound, key: &str) -> Option<bool> {
    compound.byte(key).map(|value| value != 0)
}

/// Converts a vanilla `int[4]` UUID to RFC 4122 bytes (`WORLD_STORAGE.md` §0.2).
pub fn uuid_bytes(ints: &[i32]) -> Option<[u8; 16]> {
    let [a, b, c, d] = ints else {
        return None;
    };
    let mut bytes = [0u8; 16];
    for (chunk, value) in bytes.as_chunks_mut::<4>().0.iter_mut().zip([a, b, c, d]) {
        *chunk = value.to_be_bytes();
    }
    Some(bytes)
}

/// A list of numbers that vanilla may store as either doubles or floats.
pub fn doubles(compound: &NbtCompound, key: &str) -> Option<Vec<f64>> {
    let list = compound.list(key)?;
    list.doubles().or_else(|| {
        list.floats()
            .map(|f| f.into_iter().map(f64::from).collect())
    })
}

/// An integer of any NBT integer type, the way vanilla `getIntOr`/`getLongOr` accept one.
pub fn integer(compound: &NbtCompound, key: &str) -> Option<i64> {
    match compound.get(key)? {
        NbtTag::Byte(value) => Some(i64::from(*value)),
        NbtTag::Short(value) => Some(i64::from(*value)),
        NbtTag::Int(value) => Some(i64::from(*value)),
        NbtTag::Long(value) => Some(*value),
        _ => None,
    }
}

/// Like [`integer`], narrowed to `i32` the way vanilla's numeric `intValue` does.
pub fn int(compound: &NbtCompound, key: &str) -> Option<i32> {
    integer(compound, key).map(|value| value as i32)
}

/// A float that vanilla may store as any numeric type.
pub fn float(compound: &NbtCompound, key: &str) -> Option<f32> {
    match compound.get(key)? {
        NbtTag::Float(value) => Some(*value),
        NbtTag::Double(value) => Some(*value as f32),
        _ => integer(compound, key).map(|value| value as f32),
    }
}

/// A double that vanilla may store as any numeric type.
pub fn double(compound: &NbtCompound, key: &str) -> Option<f64> {
    match compound.get(key)? {
        NbtTag::Float(value) => Some(f64::from(*value)),
        NbtTag::Double(value) => Some(*value),
        _ => integer(compound, key).map(|value| value as f64),
    }
}

/// A list of floats.
pub fn floats(compound: &NbtCompound, key: &str) -> Option<Vec<f32>> {
    compound.list(key)?.floats()
}
