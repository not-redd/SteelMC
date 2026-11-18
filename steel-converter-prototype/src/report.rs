//! What a conversion did, and what Steel config it needs.

use std::{collections::BTreeMap, fmt};

use steel_core::chunk::status::ChunkStatus;

use crate::chunk::UnimplementedPieces;

/// One converted dimension.
#[derive(Debug, Default)]
pub struct DimensionReport {
    /// Vanilla dimension id, e.g. `minecraft:overworld`.
    pub dimension: String,
    /// Steel world name inside domain `minecraft`.
    pub world: String,
    /// Steel generator to configure for the world.
    pub generator: String,
    /// Whether this is the domain's default world.
    pub default_world: bool,
    /// Written chunks by status.
    pub chunks: BTreeMap<ChunkStatus, usize>,
    /// `empty` chunks without a structure start, which neither vanilla nor Steel keeps.
    pub skipped_empty_chunks: usize,
    /// Chunks with no `Status`, which vanilla reads as absent.
    pub skipped_statusless_chunks: usize,
    /// Vanilla files written by a newer Minecraft than Steel targets.
    pub newer_version_files: usize,
    /// Chunks at `light` or later that vanilla did not mark as lit. Steel uses their stored
    /// light as-is, where vanilla would relight them.
    pub unlit_chunks: usize,
    /// Block entities outside their chunk, which vanilla drops too.
    pub dropped_block_entities: usize,
    /// Scheduled ticks outside their chunk, which vanilla drops too.
    pub dropped_ticks: usize,
    /// POI ticket entries written.
    pub pois: usize,
    /// Structure pieces written without placement data, by piece id.
    pub unimplemented_pieces: UnimplementedPieces,
    /// Converted root entities by type.
    pub entities: BTreeMap<String, usize>,
    /// Dropped entities by reason.
    pub dropped_entities: BTreeMap<String, usize>,
}

impl DimensionReport {
    pub(crate) fn add_chunk(&mut self, status: ChunkStatus, pois: usize) {
        *self.chunks.entry(status).or_default() += 1;
        self.pois += pois;
    }
}

/// The result of a conversion.
#[derive(Debug, Default)]
pub struct ConversionReport {
    /// Converted dimensions, in conversion order.
    pub dimensions: Vec<DimensionReport>,
    /// Converted players.
    pub players: usize,
    /// Player items that Steel would reject, which would make it discard the whole player
    /// file (`WORLD_STORAGE.md` §7.4). Each entry names the player, slot and reason.
    pub dropped_player_items: Vec<String>,
    /// Vanilla files that Steel has no storage for (`WORLD_STORAGE.md` §8.1).
    pub unsupported_files: Vec<String>,
    /// Anything else the user should know.
    pub warnings: Vec<String>,
    /// Steel's `default_gamemode` matching vanilla `level.dat` `GameType`.
    pub default_gamemode: Option<&'static str>,
}

impl ConversionReport {
    /// A `config/worlds.toml` that serves the converted save from `save_path`.
    ///
    /// Steel only loads dimensions that `worlds.toml` names, and chunks generated later only
    /// line up with converted ones when the generator matches.
    #[must_use]
    pub fn worlds_toml(&self, save_path: &str) -> String {
        let mut lines = vec![
            "#:schema https://raw.githubusercontent.com/Steel-Foundation/SteelMC/refs/heads/master/package-content/worlds.schema.json".to_owned(),
            format!("save_path = {}", toml_string(save_path)),
        ];
        if let Some(gamemode) = self.default_gamemode {
            lines.push(format!("default_gamemode = \"{gamemode}\""));
        }
        lines.extend(
            [
                "",
                "[storage]",
                "type = \"steel:disk\"",
                "",
                "[player_storage]",
                "type = \"steel:disk\"",
                "",
                "[domains.minecraft]",
                "default = true",
            ]
            .map(str::to_owned),
        );
        for dimension in &self.dimensions {
            lines.push(String::new());
            lines.push("[[domains.minecraft.worlds]]".to_owned());
            lines.push(format!("name = {}", toml_string(&dimension.world)));
            lines.push(format!("generator = {}", toml_string(&dimension.generator)));
            if dimension.default_world {
                lines.push("default = true".to_owned());
            }
        }
        lines.push(String::new());
        lines.join("\n")
    }
}

fn toml_string(value: &str) -> String {
    toml::Value::String(value.to_owned()).to_string()
}

impl fmt::Display for ConversionReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Conversion report")?;
        for dimension in &self.dimensions {
            let total: usize = dimension.chunks.values().sum();
            writeln!(
                f,
                "\n{} -> minecraft:{} ({total} chunks)",
                dimension.dimension, dimension.world
            )?;
            for (status, count) in &dimension.chunks {
                writeln!(f, "  {status:?}: {count}")?;
            }
            let counters = [
                (
                    "skipped empty chunks without a structure start",
                    dimension.skipped_empty_chunks,
                ),
                (
                    "skipped chunks without a status",
                    dimension.skipped_statusless_chunks,
                ),
                (
                    "files from a newer Minecraft version",
                    dimension.newer_version_files,
                ),
                (
                    "lit-status chunks vanilla had not lit (light kept as stored)",
                    dimension.unlit_chunks,
                ),
                (
                    "block entities outside their chunk (dropped)",
                    dimension.dropped_block_entities,
                ),
                (
                    "scheduled ticks outside their chunk (dropped)",
                    dimension.dropped_ticks,
                ),
                ("POI ticket entries", dimension.pois),
            ];
            for (label, count) in counters {
                if count > 0 {
                    writeln!(f, "  {label}: {count}")?;
                }
            }
            write_counts(f, "entities", &dimension.entities)?;
            write_counts(f, "dropped entities", &dimension.dropped_entities)?;
            write_counts(
                f,
                "structure pieces without placement data (not continued in new chunks)",
                &dimension.unimplemented_pieces,
            )?;
        }
        writeln!(f, "\nplayers: {}", self.players)?;
        for item in &self.dropped_player_items {
            writeln!(f, "  dropped item: {item}")?;
        }
        if !self.unsupported_files.is_empty() {
            writeln!(
                f,
                "\nvanilla files Steel has no storage for (not converted):"
            )?;
            for file in &self.unsupported_files {
                writeln!(f, "  {file}")?;
            }
        }
        if !self.warnings.is_empty() {
            writeln!(f, "\nwarnings:")?;
            for warning in &self.warnings {
                writeln!(f, "  {warning}")?;
            }
        }
        Ok(())
    }
}

fn write_counts(
    f: &mut fmt::Formatter<'_>,
    label: &str,
    counts: &BTreeMap<String, usize>,
) -> fmt::Result {
    if counts.is_empty() {
        return Ok(());
    }
    let total: usize = counts.values().sum();
    writeln!(f, "  {label}: {total}")?;
    for (key, count) in counts {
        writeln!(f, "    {key}: {count}")?;
    }
    Ok(())
}
