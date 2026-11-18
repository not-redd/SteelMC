//! Data version checks.
//!
//! Steel has no data fixers, so the converter only accepts data already written by
//! Minecraft 26.3 or newer. Vanilla upgrades chunks lazily, so a world whose `level.dat` is
//! current can still hold old chunks: every vanilla file is checked on its own.

use std::path::Path;

use simdnbt::owned::NbtCompound;
use steel_utils::version::{DATA_VERSION, MINECRAFT_VERSION};

use crate::ConvertError;

/// Fails unless `found` is at least the targeted 26.3 data version.
///
/// A missing `DataVersion` means data from before 1.9, the same as vanilla assumes.
pub fn check(path: &Path, found: Option<i32>) -> Result<(), ConvertError> {
    let found = found.unwrap_or(-1);
    if found >= DATA_VERSION {
        return Ok(());
    }
    Err(ConvertError::UnsupportedVersion {
        path: path.to_path_buf(),
        found,
        required: DATA_VERSION,
        required_name: MINECRAFT_VERSION,
    })
}

/// Checks the `DataVersion` stored at the top level of a vanilla compound.
pub fn check_data_version(path: &Path, compound: &NbtCompound) -> Result<(), ConvertError> {
    check(path, compound.int("DataVersion"))
}

/// Whether `found` is newer than the version Steel targets, which may hold data Steel does
/// not know yet.
pub const fn is_newer(found: i32) -> bool {
    found > DATA_VERSION
}
