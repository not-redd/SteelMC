//! Prototype converter from a vanilla Minecraft 26.3 save to Steel's world storage.
//!
//! The target format is specified in `WORLD_STORAGE.md`. Only saves already written by the
//! Minecraft version Steel targets are accepted: Steel has no data fixers, so older data must
//! be upgraded by vanilla first. Every vanilla file's `DataVersion` is checked, because
//! vanilla upgrades chunks lazily and a current `level.dat` can sit next to old chunks.

mod anvil;
mod chunk;
mod error;
mod level;
mod nbt;
mod player;
mod report;
mod saved_data;
mod version;

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use steel_registry::{
    dimension_type::DimensionType,
    vanilla_dimension_types::{OVERWORLD, THE_END, THE_NETHER},
};

use crate::{chunk::DimensionLayout, level::ServerLevelData, player::PlayerTarget};
pub use crate::{
    error::ConvertError,
    report::{ConversionReport, DimensionReport},
};

/// The Steel domain the vanilla dimensions become (`WORLD_STORAGE.md` §1.1).
const DOMAIN: &str = "minecraft";

/// What to convert and where to.
#[derive(Debug, Clone)]
pub struct ConvertOptions {
    /// The vanilla save folder (the one holding `level.dat`).
    pub input: PathBuf,
    /// The Steel `save_path` to write. It must not hold a Steel save yet.
    pub output: PathBuf,
}

impl ConvertOptions {
    /// Converts the vanilla save at `input` into the Steel save path `output`.
    pub fn new(input: impl Into<PathBuf>, output: impl Into<PathBuf>) -> Self {
        Self {
            input: input.into(),
            output: output.into(),
        }
    }
}

/// A vanilla dimension and the Steel world it becomes.
///
/// Steel has no data-pack dimensions, so only vanilla's three exist. The generator fields
/// are vanilla's default `world_gen_settings.dat` generator for the dimension, which is the
/// only generator each Steel world reproduces (`WORLD_STORAGE.md` §5.1).
pub(crate) struct VanillaDimension {
    /// Vanilla dimension id, also its dimension type id and its Steel world key.
    pub id: &'static str,
    /// Steel world name, and vanilla's `dimensions/minecraft/<world>` folder.
    pub world: &'static str,
    pub dimension_type: &'static DimensionType,
    pub default_world: bool,
    pub noise_settings: &'static str,
    pub biome_source: &'static str,
    pub biome_preset: Option<&'static str>,
    /// Steel generator id for `config/worlds.toml`.
    pub generator: &'static str,
}

const VANILLA_DIMENSIONS: [VanillaDimension; 3] = [
    VanillaDimension {
        id: "minecraft:overworld",
        world: "overworld",
        dimension_type: &OVERWORLD,
        default_world: true,
        noise_settings: "minecraft:overworld",
        biome_source: "minecraft:multi_noise",
        biome_preset: Some("minecraft:overworld"),
        generator: "minecraft:overworld",
    },
    VanillaDimension {
        id: "minecraft:the_nether",
        world: "the_nether",
        dimension_type: &THE_NETHER,
        default_world: false,
        noise_settings: "minecraft:nether",
        biome_source: "minecraft:multi_noise",
        biome_preset: Some("minecraft:nether"),
        generator: "minecraft:the_nether",
    },
    VanillaDimension {
        id: "minecraft:the_end",
        world: "the_end",
        dimension_type: &THE_END,
        default_world: false,
        noise_settings: "minecraft:end",
        biome_source: "minecraft:the_end",
        biome_preset: None,
        generator: "minecraft:the_end",
    },
];

/// Converts a vanilla save into a new Steel save path.
///
/// Stop vanilla before converting. The output must not hold a Steel save already: the
/// converter only creates files, so merging into an existing save could mix two worlds.
pub async fn convert(options: &ConvertOptions) -> Result<ConversionReport, ConvertError> {
    steel_registry::init_vanilla_registry();
    let input = options.input.as_path();
    let output = options.output.as_path();
    for existing in [output.join(DOMAIN), output.join("global")] {
        if existing.exists() {
            return Err(ConvertError::OutputExists { path: existing });
        }
    }

    let mut report = ConversionReport::default();
    let server = ServerLevelData::read(input, &mut report)?;
    for dimension in server.unsupported_dimensions(&VANILLA_DIMENSIONS) {
        report.warnings.push(format!(
            "dimension {dimension} is not converted: Steel has no data-pack dimensions"
        ));
    }
    let dimensions: Vec<&VanillaDimension> = VANILLA_DIMENSIONS
        .iter()
        .filter(|dimension| server.has_dimension(dimension))
        .collect();
    for dimension in &dimensions {
        server.check_generator(dimension)?;
    }

    let pool = rayon::ThreadPoolBuilder::new()
        .thread_name(|index| format!("converter-{index}"))
        .build()
        .map_err(|e| ConvertError::io(output, io::Error::other(e)))?;

    let domain_dir = output.join(DOMAIN);
    let mut dimension_dirs = Vec::new();
    for dimension in &dimensions {
        let dimension_dir = input.join("dimensions/minecraft").join(dimension.world);
        let world_dir = domain_dir.join("worlds").join(dimension.world);
        fs::create_dir_all(&world_dir).map_err(|e| ConvertError::io(&world_dir, e))?;

        let mut dimension_report = DimensionReport {
            dimension: dimension.id.to_owned(),
            world: dimension.world.to_owned(),
            generator: dimension.generator.to_owned(),
            default_world: dimension.default_world,
            ..DimensionReport::default()
        };
        chunk::convert_dimension(
            &dimension_dir,
            &world_dir,
            DimensionLayout::new(dimension.dimension_type),
            &pool,
            &mut dimension_report,
        )
        .await?;
        server.write_level_toml(dimension, &dimension_dir, &world_dir, &mut report)?;
        saved_data::chunk_tickets(&dimension_dir, &world_dir, &mut report)?;
        if dimension.default_world {
            saved_data::scoreboard(input, &world_dir, &mut report)?;
            saved_data::command_storage(input, &world_dir)?;
            saved_data::random_sequences(input, &world_dir)?;
        }
        report.dimensions.push(dimension_report);
        dimension_dirs.push((dimension.world, dimension_dir));
    }

    let worlds: Vec<String> = dimensions
        .iter()
        .map(|dimension| dimension.id.to_owned())
        .collect();
    player::convert_players(
        input,
        &PlayerTarget {
            save_root: output,
            domain: DOMAIN,
            worlds: &worlds,
            default_game_mode: server.game_type(),
        },
        &mut report,
    )?;

    let dimension_dirs: Vec<(&str, &Path)> = dimension_dirs
        .iter()
        .map(|(world, dir)| (*world, dir.as_path()))
        .collect();
    report.unsupported_files = saved_data::unsupported_files(input, &dimension_dirs);
    Ok(report)
}
