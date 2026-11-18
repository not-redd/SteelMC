//! Conversion errors.

use std::{io, path::PathBuf};

use thiserror::Error;

/// A failure that stops the conversion.
///
/// The converter is strict: Steel silently regenerates chunks it cannot decode
/// (`WORLD_STORAGE.md` §0.4), so writing questionable data would lose terrain without telling
/// anyone. Anything Steel would reject is an error here instead.
#[derive(Debug, Error)]
pub enum ConvertError {
    /// Reading or writing a file failed.
    #[error("{}: {source}", path.display())]
    Io {
        /// The file or directory involved.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
    /// A vanilla file is not valid NBT or is missing data it must have.
    #[error("{}: {message}", path.display())]
    InvalidVanillaData {
        /// The vanilla file.
        path: PathBuf,
        /// What is wrong with it.
        message: String,
    },
    /// The save, or part of it, was last written by a Minecraft version older than the
    /// converter supports.
    #[error(
        "{}: data version {found} is older than {required} (Minecraft {required_name}). \
         Open the world in vanilla {required_name} with --forceUpgrade first",
        path.display()
    )]
    UnsupportedVersion {
        /// The file whose `DataVersion` is too old.
        path: PathBuf,
        /// The `DataVersion` found.
        found: i32,
        /// The minimum supported `DataVersion`.
        required: i32,
        /// The Minecraft version of `required`.
        required_name: &'static str,
    },
    /// A chunk holds data Steel has no representation for, so writing it would make Steel
    /// discard and regenerate the chunk.
    #[error("{} chunk ({chunk_x}, {chunk_z}): {message}", path.display())]
    UnconvertibleChunk {
        /// The vanilla region file.
        path: PathBuf,
        /// Chunk X.
        chunk_x: i32,
        /// Chunk Z.
        chunk_z: i32,
        /// Why the chunk cannot be converted.
        message: String,
    },
    /// The save uses a vanilla feature Steel cannot represent, such as a custom world
    /// generator or dimension type.
    #[error("{}: {message}", path.display())]
    Unsupported {
        /// The vanilla file that declares the feature.
        path: PathBuf,
        /// What is unsupported.
        message: String,
    },
    /// The output location already holds a Steel world.
    #[error("{} already exists; convert into an empty save directory", path.display())]
    OutputExists {
        /// The existing path.
        path: PathBuf,
    },
}

impl ConvertError {
    pub(crate) fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    pub(crate) fn unsupported(path: impl Into<PathBuf>, message: impl Into<String>) -> Self {
        Self::Unsupported {
            path: path.into(),
            message: message.into(),
        }
    }

    pub(crate) fn invalid(path: impl Into<PathBuf>, message: impl Into<String>) -> Self {
        Self::InvalidVanillaData {
            path: path.into(),
            message: message.into(),
        }
    }
}
