//! Chunk conversion (`WORLD_STORAGE.md` §4).
//!
//! Each vanilla region is converted on its own: its chunks are decoded in parallel, the
//! matching `entities/` and `poi/` regions are merged in, and the chunks are written through
//! Steel's own `RegionManager`, so the region file layout is always the one Steel reads.

mod block_entities;
mod entities;
mod light;
mod poi;
mod sections;
mod structures;
mod ticks;

use std::{
    fs, mem,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use rayon::prelude::*;
use rustc_hash::FxHashMap;
use simdnbt::owned::{NbtCompound, NbtTag};
use steel_core::{
    chunk::status::ChunkStatus,
    chunk_saver::{
        PersistentChunk, PersistentHeightmap, PersistentLightData, PersistentSection,
        PreparedChunkSave, RegionManager,
    },
};
use steel_registry::dimension_type::DimensionType;
use steel_utils::ChunkPos;

use crate::{ConvertError, anvil::RegionFile, nbt, report::DimensionReport, version};
pub use entities::EntityConverter;
pub use structures::UnimplementedPieces;

/// The vertical extent of a dimension, from its dimension type. Steel chunks do not store
/// it (`WORLD_STORAGE.md` §4).
#[derive(Debug, Clone, Copy)]
pub struct DimensionLayout {
    pub min_y: i32,
    pub height: i32,
    pub has_skylight: bool,
}

impl DimensionLayout {
    pub const fn new(dimension_type: &DimensionType) -> Self {
        Self {
            min_y: dimension_type.min_y,
            height: dimension_type.height,
            has_skylight: dimension_type.has_skylight,
        }
    }

    const fn min_section_y(&self) -> i32 {
        self.min_y.div_euclid(16)
    }

    const fn section_count(&self) -> i32 {
        self.height / 16
    }

    pub const fn contains_y(&self, y: i32) -> bool {
        y >= self.min_y && y < self.min_y + self.height
    }
}

/// The chunk being converted.
#[derive(Debug, Clone, Copy)]
pub struct ChunkContext {
    pub x: i32,
    pub z: i32,
}

impl ChunkContext {
    pub const fn contains_block(self, x: i32, z: i32) -> bool {
        x >> 4 == self.x && z >> 4 == self.z
    }
}

/// A converted chunk waiting for its entities and POIs.
struct PendingChunk {
    context: ChunkContext,
    status: ChunkStatus,
    chunk: PersistentChunk<'static>,
    /// Root entities vanilla stored in the proto chunk itself.
    proto_entities: Vec<NbtCompound>,
}

/// What one vanilla chunk contributes to the report.
#[derive(Default)]
struct ChunkStats {
    newer_version: bool,
    unlit: bool,
    dropped_block_entities: usize,
    dropped_ticks: usize,
    unimplemented_pieces: UnimplementedPieces,
}

impl ChunkStats {
    fn record(self, report: &mut DimensionReport) {
        report.newer_version_files += usize::from(self.newer_version);
        report.unlit_chunks += usize::from(self.unlit);
        report.dropped_block_entities += self.dropped_block_entities;
        report.dropped_ticks += self.dropped_ticks;
        for (piece, count) in self.unimplemented_pieces {
            *report.unimplemented_pieces.entry(piece).or_default() += count;
        }
    }
}

enum ChunkOutcome {
    Converted(Box<PendingChunk>, ChunkStats),
    /// An `empty` chunk without a structure start. Vanilla does not keep these either.
    EmptyWithoutStart,
    /// No `Status`; vanilla reads such a chunk as absent.
    NoStatus,
}

/// Converts every chunk of one vanilla dimension into `world_dir/region/`.
pub async fn convert_dimension(
    vanilla_dir: &Path,
    world_dir: &Path,
    layout: DimensionLayout,
    pool: &rayon::ThreadPool,
    report: &mut DimensionReport,
) -> Result<(), ConvertError> {
    let region_dir = world_dir.join("region");
    fs::create_dir_all(&region_dir).map_err(|e| ConvertError::io(&region_dir, e))?;
    let manager = RegionManager::new(&region_dir);
    let mut entities = EntityConverter::default();

    for (region_x, region_z, path) in RegionFile::list(&vanilla_dir.join("region"))? {
        let region = RegionFile::open(&path, region_x, region_z)?;
        let mut chunks = Vec::new();
        // Decoding and repacking is CPU-bound, so it runs on the rayon pool. The converter
        // has nothing else to do meanwhile, so blocking this task is fine.
        let outcomes = pool.install(|| decode_region(&region, layout))?;
        for outcome in outcomes {
            match outcome {
                ChunkOutcome::Converted(chunk, stats) => {
                    stats.record(report);
                    chunks.push(*chunk);
                }
                ChunkOutcome::EmptyWithoutStart => report.skipped_empty_chunks += 1,
                ChunkOutcome::NoStatus => report.skipped_statusless_chunks += 1,
            }
        }

        let file_name = format!("r.{region_x}.{region_z}.mca");
        attach_entities(
            &vanilla_dir.join("entities").join(&file_name),
            region_x,
            region_z,
            &mut chunks,
            &mut entities,
            report,
        )?;
        attach_pois(
            &vanilla_dir.join("poi").join(&file_name),
            region_x,
            region_z,
            layout,
            &mut chunks,
            report,
        )?;

        for pending in chunks {
            report.add_chunk(pending.status, pending.chunk.pois.len());
            let pos = ChunkPos::new(pending.context.x, pending.context.z);
            manager
                .save_chunk_data(
                    PreparedChunkSave {
                        pos,
                        status: pending.status,
                        persistent: pending.chunk,
                        handled_runtime_entity_ids: Vec::new(),
                    },
                    pool,
                )
                .await
                .map_err(|e| ConvertError::io(&region_dir, e))?;
        }
    }

    manager
        .close_all()
        .await
        .map_err(|e| ConvertError::io(&region_dir, e))?;
    report.entities = entities.converted;
    report.dropped_entities = entities.dropped;
    Ok(())
}

fn decode_region(
    region: &RegionFile,
    layout: DimensionLayout,
) -> Result<Vec<ChunkOutcome>, ConvertError> {
    let positions: Vec<(i32, i32)> = region.chunk_positions().collect();
    positions
        .into_par_iter()
        .filter_map(|(x, z)| match region.read_chunk(x, z) {
            Ok(Some(chunk)) => Some(decode_chunk(
                region.path(),
                &chunk,
                ChunkContext { x, z },
                layout,
            )),
            Ok(None) => None,
            Err(error) => Some(Err(error)),
        })
        .collect()
}

fn decode_chunk(
    path: &Path,
    vanilla: &NbtCompound,
    context: ChunkContext,
    layout: DimensionLayout,
) -> Result<ChunkOutcome, ConvertError> {
    let unconvertible = |message: String| ConvertError::UnconvertibleChunk {
        path: path.to_path_buf(),
        chunk_x: context.x,
        chunk_z: context.z,
        message,
    };
    let data_version = vanilla.int("DataVersion");
    version::check(path, data_version)?;
    let mut counters = ChunkStats {
        newer_version: data_version.is_some_and(version::is_newer),
        ..ChunkStats::default()
    };

    let Some(vanilla_status) = nbt::string(vanilla, "Status").filter(|s| !s.is_empty()) else {
        return Ok(ChunkOutcome::NoStatus);
    };
    let mut status = parse_status(&vanilla_status)
        .ok_or_else(|| unconvertible(format!("unknown status {vanilla_status:?}")))?;
    check_position(vanilla, context, layout).map_err(unconvertible)?;

    let empty = NbtCompound::new();
    let structures = vanilla.compound("structures").unwrap_or(&empty);
    if status == ChunkStatus::Empty {
        if !structures::has_valid_start(structures) {
            return Ok(ChunkOutcome::EmptyWithoutStart);
        }
        // Steel never loads `empty` chunks (`WORLD_STORAGE.md` §4.1).
        status = ChunkStatus::StructureStarts;
    }

    let (palettes, sections, light) = blocks_and_light(vanilla, layout).map_err(unconvertible)?;
    if status >= ChunkStatus::Light && !nbt::boolean(vanilla, "isLightOn").unwrap_or_default() {
        // Vanilla relights such a chunk; Steel uses stored light as-is from `Light` on.
        counters.unlit = true;
    }

    let block_entities = block_entities::convert(
        nbt::compounds(vanilla, "block_entities"),
        context,
        &layout,
        &mut counters.dropped_block_entities,
    )
    .map_err(unconvertible)?;
    let block_ticks = ticks::convert(
        nbt::compounds(vanilla, "block_ticks"),
        context,
        &layout,
        &mut counters.dropped_ticks,
    )
    .map_err(unconvertible)?;
    let fluid_ticks = ticks::convert(
        nbt::compounds(vanilla, "fluid_ticks"),
        context,
        &layout,
        &mut counters.dropped_ticks,
    )
    .map_err(unconvertible)?;
    let postprocessing = postprocessing(vanilla, &layout).map_err(unconvertible)?;
    let structure_starts = structures::starts(structures, &mut counters.unimplemented_pieces)
        .map_err(unconvertible)?;
    let structure_references = structures::references(structures).map_err(unconvertible)?;
    let proto_entities = if status == ChunkStatus::Full {
        Vec::new()
    } else {
        nbt::compounds(vanilla, "entities").to_vec()
    };

    let chunk = PersistentChunk {
        last_modified: unix_now(),
        block_states: palettes.block_states,
        biomes: palettes.biomes,
        sections,
        block_entities,
        entities: Vec::new(),
        block_ticks,
        fluid_ticks,
        // Steel recomputes missing heightmaps from the blocks on load (§4.3).
        heightmaps: Vec::<PersistentHeightmap>::new(),
        light,
        postprocessing,
        structure_starts,
        structure_references,
        pois: Vec::new(),
    };
    Ok(ChunkOutcome::Converted(
        Box::new(PendingChunk {
            context,
            status,
            chunk,
            proto_entities,
        }),
        counters,
    ))
}

/// Checks that the chunk is stored at its own position and has the dimension's height.
fn check_position(
    vanilla: &NbtCompound,
    context: ChunkContext,
    layout: DimensionLayout,
) -> Result<(), String> {
    let (Some(x_pos), Some(z_pos)) = (vanilla.int("xPos"), vanilla.int("zPos")) else {
        return Err("chunk has no xPos/zPos".to_owned());
    };
    if (x_pos, z_pos) != (context.x, context.z) {
        return Err(format!("chunk is stored in the slot of ({x_pos}, {z_pos})"));
    }
    if let Some(y_pos) = vanilla.int("yPos")
        && y_pos != layout.min_section_y()
    {
        return Err(format!(
            "chunk starts at section {y_pos}, but the dimension type starts at section {}",
            layout.min_section_y()
        ));
    }
    Ok(())
}

/// Every section of the dimension, bottom to top, and the chunk's light.
fn blocks_and_light(
    vanilla: &NbtCompound,
    layout: DimensionLayout,
) -> Result<
    (
        sections::ChunkPalettes,
        Vec<PersistentSection>,
        PersistentLightData,
    ),
    String,
> {
    let vanilla_sections = nbt::compounds(vanilla, "sections");
    let mut by_y: FxHashMap<i32, &NbtCompound> = FxHashMap::default();
    for section in vanilla_sections {
        let y = section.byte("Y").ok_or("section has no Y")?;
        by_y.insert(i32::from(y), section);
    }
    let mut palettes = sections::ChunkPalettes::default();
    let sections = (0..layout.section_count())
        .map(|i| {
            let y = layout.min_section_y() + i;
            palettes
                .section(by_y.get(&y).copied())
                .map_err(|message| format!("section {y}: {message}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let light = light::convert(vanilla_sections, &layout)?;
    Ok((palettes, sections, light))
}

/// Vanilla `ChunkStatus` registry names, which are its serialized form.
fn parse_status(name: &str) -> Option<ChunkStatus> {
    Some(match name.strip_prefix("minecraft:").unwrap_or(name) {
        "empty" => ChunkStatus::Empty,
        "structure_starts" => ChunkStatus::StructureStarts,
        "structure_references" => ChunkStatus::StructureReferences,
        "biomes" => ChunkStatus::Biomes,
        "terrain" => ChunkStatus::Terrain,
        "features" => ChunkStatus::Features,
        "initialize_light" => ChunkStatus::InitializeLight,
        "light" => ChunkStatus::Light,
        "spawn" => ChunkStatus::Spawn,
        "full" => ChunkStatus::Full,
        _ => return None,
    })
}

/// Vanilla `PostProcessing`: one list of packed shorts per section, the same packing Steel
/// uses (`WORLD_STORAGE.md` §4.8).
fn postprocessing(
    vanilla: &NbtCompound,
    layout: &DimensionLayout,
) -> Result<Vec<Vec<u16>>, String> {
    let Some(lists) = vanilla.list("PostProcessing") else {
        return Ok(Vec::new());
    };
    let lists = match lists.lists() {
        Some(lists) => lists,
        None if lists.compounds().is_none() && lists.shorts().is_none() => return Ok(Vec::new()),
        None => return Err("PostProcessing is not a list of lists".to_owned()),
    };
    if lists.len() > layout.section_count() as usize {
        return Err(format!(
            "PostProcessing has {} sections, the dimension has {}",
            lists.len(),
            layout.section_count()
        ));
    }
    Ok(lists
        .iter()
        .map(|list| {
            list.shorts()
                .unwrap_or_default()
                .into_iter()
                .map(|packed| packed as u16)
                .collect()
        })
        .collect())
}

/// Places the root entities of the region's `entities/` file and of its proto chunks.
///
/// Each entity goes in the chunk at its position when that chunk is in this region and has
/// the same kind (full or proto) as the chunk it came from. Otherwise it stays in the chunk
/// it was stored in, which Steel still loads, with a warning (§4.6).
fn attach_entities(
    entity_region_path: &Path,
    region_x: i32,
    region_z: i32,
    chunks: &mut [PendingChunk],
    converter: &mut EntityConverter,
    report: &mut DimensionReport,
) -> Result<(), ConvertError> {
    let index: FxHashMap<(i32, i32), usize> = chunks
        .iter()
        .enumerate()
        .map(|(i, chunk)| ((chunk.context.x, chunk.context.z), i))
        .collect();

    let mut sources: Vec<((i32, i32), bool, Vec<NbtCompound>)> = Vec::new();
    if entity_region_path.exists() {
        let entity_region = RegionFile::open(entity_region_path, region_x, region_z)?;
        let positions: Vec<(i32, i32)> = entity_region.chunk_positions().collect();
        for (x, z) in positions {
            let Some(mut entity_chunk) = entity_region.read_chunk(x, z)? else {
                continue;
            };
            let data_version = entity_chunk.int("DataVersion");
            version::check(entity_region_path, data_version)?;
            if data_version.is_some_and(version::is_newer) {
                report.newer_version_files += 1;
            }
            let entities = match entity_chunk.take("Entities") {
                Some(NbtTag::List(list)) => list.compounds().unwrap_or_default().to_vec(),
                _ => Vec::new(),
            };
            sources.push(((x, z), true, entities));
        }
    }
    for chunk in chunks.iter_mut() {
        let entities = mem::take(&mut chunk.proto_entities);
        if !entities.is_empty() {
            sources.push(((chunk.context.x, chunk.context.z), false, entities));
        }
    }

    for (stored, from_full_chunk, entities) in sources {
        for entity in &entities {
            let at_position = entities::chunk_of(entity)
                .and_then(|pos| index.get(&pos).copied())
                .filter(|&i| (chunks[i].status == ChunkStatus::Full) == from_full_chunk);
            let Some(target) = at_position.or_else(|| index.get(&stored).copied()) else {
                converter.drop_unplaced();
                continue;
            };
            if let Some(converted) = converter.convert_root(entity) {
                chunks[target].chunk.entities.push(converted);
            }
        }
    }
    Ok(())
}

/// Adds vanilla `poi/` ticket state to the region's full chunks.
fn attach_pois(
    poi_region_path: &Path,
    region_x: i32,
    region_z: i32,
    layout: DimensionLayout,
    chunks: &mut [PendingChunk],
    report: &mut DimensionReport,
) -> Result<(), ConvertError> {
    if !poi_region_path.exists() {
        return Ok(());
    }
    let poi_region = RegionFile::open(poi_region_path, region_x, region_z)?;
    for chunk in chunks
        .iter_mut()
        .filter(|chunk| chunk.status == ChunkStatus::Full)
    {
        let context = chunk.context;
        let Some(poi_chunk) = poi_region.read_chunk(context.x, context.z)? else {
            continue;
        };
        let data_version = poi_chunk.int("DataVersion");
        version::check(poi_region_path, data_version)?;
        if data_version.is_some_and(version::is_newer) {
            report.newer_version_files += 1;
        }
        chunk.chunk.pois = poi::convert(&poi_chunk, context, &layout).map_err(|message| {
            ConvertError::UnconvertibleChunk {
                path: poi_region_path.to_path_buf(),
                chunk_x: context.x,
                chunk_z: context.z,
                message,
            }
        })?;
    }
    Ok(())
}

fn unix_now() -> u32 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u32::try_from(elapsed.as_secs()).unwrap_or(u32::MAX)
        })
}
