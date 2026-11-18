//! `level.toml` for each world (`WORLD_STORAGE.md` §5).
//!
//! Vanilla keeps the seed, time, weather, game rules and clocks once per server; Steel keeps
//! them in every world's `level.toml`, so they are copied into each world.

use std::{
    fs,
    path::{Path, PathBuf},
};

use rustc_hash::FxHashMap;
use simdnbt::owned::{NbtCompound, NbtTag};
use steel_core::level_data::{LevelData, RespawnData, SpawnPoint, WeatherState, WorldBorderData};
use steel_registry::{REGISTRY, RegistryExt};
use steel_utils::{BlockPos, Identifier, types::Difficulty};

use crate::{ConvertError, VanillaDimension, nbt, report::ConversionReport, version};

/// Vanilla `level.dat` `spawn`, the server's single world spawn.
struct VanillaSpawn {
    dimension: String,
    pos: [i32; 3],
    yaw: f32,
    pitch: f32,
}

/// Server-wide vanilla state that goes into every world's `level.toml`.
pub struct ServerLevelData {
    seed: i64,
    /// `level.dat` `GameType`, the game mode new players get.
    game_type: i32,
    game_time: i64,
    difficulty: Difficulty,
    difficulty_locked: bool,
    initialized: bool,
    spawn: Option<VanillaSpawn>,
    weather: WeatherState,
    game_rules: FxHashMap<String, serde_json::Value>,
    world_clocks: toml::Table,
    /// `world_gen_settings.dat` `dimensions`.
    dimensions: NbtCompound,
    world_gen_settings_path: PathBuf,
}

impl ServerLevelData {
    /// Reads `level.dat` and the server-wide saved data, and checks the save's version.
    pub fn read(input: &Path, report: &mut ConversionReport) -> Result<Self, ConvertError> {
        let level_dat_path = input.join("level.dat");
        let level_dat = nbt::read_gzip_file(&level_dat_path)?;
        let data = level_dat
            .compound("Data")
            .ok_or_else(|| ConvertError::invalid(&level_dat_path, "level.dat has no Data"))?;
        version::check_data_version(&level_dat_path, data)?;
        if data.int("DataVersion").is_some_and(version::is_newer) {
            report.warnings.push(format!(
                "{} was saved by a newer Minecraft than Steel targets; newer data may be lost",
                level_dat_path.display()
            ));
        }

        let data_dir = input.join("data/minecraft");
        let world_gen_settings_path = data_dir.join("world_gen_settings.dat");
        let world_gen = nbt::read_saved_data(&world_gen_settings_path)?
            .ok_or_else(|| ConvertError::invalid(&world_gen_settings_path, "file is missing"))?;
        let seed = world_gen
            .long("seed")
            .ok_or_else(|| ConvertError::invalid(&world_gen_settings_path, "no seed"))?;
        if !nbt::boolean(&world_gen, "generate_structures").unwrap_or(true) {
            report.warnings.push(
                "generate_structures is off in vanilla; Steel has no such setting and will \
                 generate structures in new chunks"
                    .to_owned(),
            );
        }

        let difficulty_settings = data.compound("difficulty_settings");
        let difficulty = match difficulty_settings
            .and_then(|s| nbt::string(s, "difficulty"))
            .as_deref()
        {
            Some("peaceful") => Difficulty::Peaceful,
            Some("easy") => Difficulty::Easy,
            Some("normal") | None => Difficulty::Normal,
            Some("hard") => Difficulty::Hard,
            Some(other) => {
                return Err(ConvertError::invalid(
                    &level_dat_path,
                    format!("unknown difficulty {other:?}"),
                ));
            }
        };
        let game_type = nbt::int(data, "GameType").unwrap_or_default();
        report.default_gamemode = match game_type {
            0 => Some("survival"),
            1 => Some("creative"),
            2 => Some("adventure"),
            3 => Some("spectator"),
            _ => None,
        };

        let weather_path = data_dir.join("weather.dat");
        let weather =
            nbt::read_saved_data(&weather_path)?.map_or_else(WeatherState::default, |weather| {
                WeatherState {
                    raining: nbt::boolean(&weather, "raining").unwrap_or_default(),
                    rain_time: nbt::int(&weather, "rain_time").unwrap_or_default(),
                    thundering: nbt::boolean(&weather, "thundering").unwrap_or_default(),
                    thunder_time: nbt::int(&weather, "thunder_time").unwrap_or_default(),
                    clear_weather_time: nbt::int(&weather, "clear_weather_time")
                        .unwrap_or_default(),
                }
            });

        Ok(Self {
            seed,
            game_type,
            game_time: nbt::integer(data, "Time").unwrap_or_default(),
            difficulty,
            difficulty_locked: difficulty_settings
                .and_then(|s| nbt::boolean(s, "locked"))
                .unwrap_or_default(),
            initialized: nbt::boolean(data, "initialized").unwrap_or_default(),
            spawn: data.compound("spawn").and_then(|spawn| {
                let &[x, y, z] = spawn.int_array("pos")? else {
                    return None;
                };
                Some(VanillaSpawn {
                    dimension: nbt::string(spawn, "dimension")
                        .unwrap_or_else(|| "minecraft:overworld".to_owned()),
                    pos: [x, y, z],
                    yaw: nbt::float(spawn, "yaw").unwrap_or_default(),
                    pitch: nbt::float(spawn, "pitch").unwrap_or_default(),
                })
            }),
            weather,
            game_rules: game_rules(&data_dir.join("game_rules.dat"), report)?,
            world_clocks: world_clocks(&data_dir.join("world_clocks.dat"))?,
            dimensions: world_gen
                .compound("dimensions")
                .cloned()
                .unwrap_or_default(),
            world_gen_settings_path,
        })
    }

    /// Vanilla `level.dat` `GameType`, which a player file without `playerGameType` uses.
    pub const fn game_type(&self) -> i32 {
        self.game_type
    }

    /// The vanilla dimensions `world_gen_settings.dat` declares that Steel has no world for.
    pub fn unsupported_dimensions(&self, known: &[VanillaDimension]) -> Vec<String> {
        self.dimensions
            .keys()
            .map(|key| key.to_str().into_owned())
            .filter(|key| !known.iter().any(|dimension| dimension.id == key))
            .collect()
    }

    /// Whether `world_gen_settings.dat` declares the dimension.
    pub fn has_dimension(&self, dimension: &VanillaDimension) -> bool {
        self.dimensions.contains(dimension.id)
    }

    /// Checks that the dimension uses its vanilla dimension type and default generator, the
    /// only ones Steel's matching world can reproduce (`WORLD_STORAGE.md` §5.1).
    pub fn check_generator(&self, dimension: &VanillaDimension) -> Result<(), ConvertError> {
        let path = &self.world_gen_settings_path;
        let unsupported = |message: String| {
            ConvertError::unsupported(path, format!("{}: {message}", dimension.id))
        };
        let stem = self
            .dimensions
            .compound(dimension.id)
            .ok_or_else(|| unsupported("not declared".to_owned()))?;
        let dimension_type = nbt::string(stem, "type");
        if dimension_type.as_deref() != Some(dimension.id) {
            return Err(unsupported(format!(
                "uses dimension type {dimension_type:?}; Steel only has the vanilla {} type",
                dimension.id
            )));
        }
        let generator = stem
            .compound("generator")
            .ok_or_else(|| unsupported("has no generator".to_owned()))?;
        let biome_source = generator.compound("biome_source");
        let matches = nbt::string(generator, "type").as_deref() == Some("minecraft:noise")
            && nbt::string(generator, "settings").as_deref() == Some(dimension.noise_settings)
            && biome_source.and_then(|s| nbt::string(s, "type")).as_deref()
                == Some(dimension.biome_source)
            && biome_source
                .and_then(|s| nbt::string(s, "preset"))
                .as_deref()
                == dimension.biome_preset;
        if !matches {
            return Err(unsupported(format!(
                "uses a non-default generator; this prototype only maps vanilla's default {} \
                 generator to Steel's {}",
                dimension.id, dimension.generator
            )));
        }
        Ok(())
    }

    /// Writes `level.toml` for one converted world.
    pub fn write_level_toml(
        &self,
        dimension: &VanillaDimension,
        dimension_dir: &Path,
        world_dir: &Path,
        report: &mut ConversionReport,
    ) -> Result<(), ConvertError> {
        let mut level = LevelData::new_with_seed_and_difficulty(self.seed, self.difficulty);
        level.difficulty_locked = self.difficulty_locked;
        level.initialized = self.initialized;
        level.weather = self.weather.clone();
        level.game_rules.clone_from(&self.game_rules);
        level.world_border = world_border(&dimension_dir.join("data/minecraft/world_border.dat"))?;
        if let Some(spawn) = &self.spawn {
            if spawn.dimension == dimension.id {
                level.spawn = SpawnPoint {
                    x: spawn.pos[0],
                    y: spawn.pos[1],
                    z: spawn.pos[2],
                    angle: spawn.yaw,
                };
            }
            if dimension.default_world {
                match spawn.dimension.parse::<Identifier>() {
                    Ok(world) => {
                        level.respawn = Some(RespawnData::of(
                            world,
                            BlockPos::new(spawn.pos[0], spawn.pos[1], spawn.pos[2]),
                            spawn.yaw,
                            spawn.pitch,
                        ));
                    }
                    Err(_) => report.warnings.push(format!(
                        "world spawn dimension {:?} is not a valid id",
                        spawn.dimension
                    )),
                }
            }
        }

        let toml::Value::Table(mut table) = toml::Value::try_from(&level)
            .map_err(|e| ConvertError::invalid(world_dir, e.to_string()))?
        else {
            return Err(ConvertError::invalid(
                world_dir,
                "level data did not serialize to a table",
            ));
        };
        // Only the domain default world owns the game time (§5.2).
        if dimension.default_world {
            table.insert("game_time".to_owned(), toml::Value::Integer(self.game_time));
        } else {
            table.remove("game_time");
        }
        table.insert(
            "world_clocks".to_owned(),
            toml::Value::Table(self.world_clocks.clone()),
        );

        let path = world_dir.join("level.toml");
        let text = toml::to_string_pretty(&table)
            .map_err(|e| ConvertError::invalid(&path, e.to_string()))?;
        fs::write(&path, text).map_err(|e| ConvertError::io(&path, e))
    }
}

/// Vanilla stores `minecraft:<rule>` with byte (boolean) or int values; Steel wants the bare
/// vanilla name with TOML booleans and integers (§5.5).
fn game_rules(
    path: &Path,
    report: &mut ConversionReport,
) -> Result<FxHashMap<String, serde_json::Value>, ConvertError> {
    let Some(rules) = nbt::read_saved_data(path)? else {
        return Ok(FxHashMap::default());
    };
    let mut converted = FxHashMap::default();
    for (key, value) in rules.iter() {
        let key = key.to_str();
        let Ok(id) = key.parse::<Identifier>() else {
            report
                .warnings
                .push(format!("game rule {key:?} is not a valid id; dropped"));
            continue;
        };
        if REGISTRY.game_rules.by_key(&id).is_none() {
            report
                .warnings
                .push(format!("game rule {id} is unknown to Steel; dropped"));
            continue;
        }
        let value = match value {
            NbtTag::Byte(value) => serde_json::Value::Bool(*value != 0),
            NbtTag::Int(value) => serde_json::Value::from(*value),
            _ => {
                report.warnings.push(format!(
                    "game rule {id} has an unexpected NBT type; dropped"
                ));
                continue;
            }
        };
        let name = if id.namespace == Identifier::VANILLA_NAMESPACE {
            id.path.to_string()
        } else {
            id.to_string()
        };
        converted.insert(name, value);
    }
    Ok(converted)
}

/// `world_clocks.dat` → `world_clocks` (§5.3). Steel refuses to load a world with an unknown
/// clock, so one is an error here.
fn world_clocks(path: &Path) -> Result<toml::Table, ConvertError> {
    let Some(clocks) = nbt::read_saved_data(path)? else {
        return Ok(toml::Table::new());
    };
    let mut table = toml::Table::new();
    for (key, state) in clocks.iter() {
        let key = key.to_str();
        let known = key
            .parse::<Identifier>()
            .is_ok_and(|id| REGISTRY.world_clocks.by_key(&id).is_some());
        if !known {
            return Err(ConvertError::unsupported(
                path,
                format!("world clock {key} is unknown to Steel"),
            ));
        }
        let Some(state) = state.compound() else {
            return Err(ConvertError::invalid(
                path,
                format!("world clock {key} is not a compound"),
            ));
        };
        let total_ticks = nbt::integer(state, "total_ticks").ok_or_else(|| {
            ConvertError::invalid(path, format!("world clock {key} has no total_ticks"))
        })?;
        // Defaults are vanilla `ClockState.CODEC`'s.
        let mut clock = toml::Table::new();
        clock.insert("total_ticks".to_owned(), toml::Value::Integer(total_ticks));
        clock.insert(
            "partial_tick".to_owned(),
            toml::Value::Float(f64::from(nbt::float(state, "partial_tick").unwrap_or(0.0))),
        );
        clock.insert(
            "rate".to_owned(),
            toml::Value::Float(f64::from(nbt::float(state, "rate").unwrap_or(1.0))),
        );
        clock.insert(
            "paused".to_owned(),
            toml::Value::Boolean(nbt::boolean(state, "paused").unwrap_or(false)),
        );
        table.insert(key.into_owned(), toml::Value::Table(clock));
    }
    Ok(table)
}

/// A dimension's `world_border.dat`; field names are the same as Steel's (§5.4).
fn world_border(path: &Path) -> Result<WorldBorderData, ConvertError> {
    let mut border = WorldBorderData::default();
    let Some(data) = nbt::read_saved_data(path)? else {
        return Ok(border);
    };
    let doubles = [
        ("center_x", &mut border.center_x),
        ("center_z", &mut border.center_z),
        ("damage_per_block", &mut border.damage_per_block),
        ("safe_zone", &mut border.safe_zone),
        ("size", &mut border.size),
        ("lerp_target", &mut border.lerp_target),
    ];
    for (key, field) in doubles {
        if let Some(value) = nbt::double(&data, key) {
            *field = value;
        }
    }
    if let Some(value) = nbt::int(&data, "warning_blocks") {
        border.warning_blocks = value;
    }
    if let Some(value) = nbt::int(&data, "warning_time") {
        border.warning_time = value;
    }
    if let Some(value) = nbt::integer(&data, "lerp_time") {
        border.lerp_time = value;
    }
    Ok(border)
}
