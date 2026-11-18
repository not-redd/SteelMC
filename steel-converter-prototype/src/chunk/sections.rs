//! Block-state and biome palettes (`WORLD_STORAGE.md` §4.2).
//!
//! Vanilla stores a palette and bit-packed indices per section. Steel stores one palette per
//! chunk, per-section palettes of indices into it, and a different set of bit widths, so
//! every section is unpacked and packed again.

use rustc_hash::FxHashMap;
use simdnbt::owned::{NbtCompound, NbtTag};
use steel_core::chunk_saver::{PersistentBiomeData, PersistentBlockState, PersistentSection};
use steel_registry::{REGISTRY, RegistryExt, vanilla_biomes, vanilla_blocks};
use steel_utils::{BlockStateId, Identifier, nbt::nbt_list_values};

const BLOCKS_PER_SECTION: usize = 4096;
const BIOMES_PER_SECTION: usize = 64;

/// The chunk-wide palettes that every section points into.
#[derive(Default)]
pub struct ChunkPalettes {
    pub block_states: Vec<PersistentBlockState<'static>>,
    block_index: FxHashMap<BlockStateId, u16>,
    pub biomes: Vec<Identifier>,
    biome_index: FxHashMap<Identifier, u16>,
}

impl ChunkPalettes {
    fn block_state(&mut self, state: BlockStateId) -> Result<u16, String> {
        if let Some(&index) = self.block_index.get(&state) {
            return Ok(index);
        }
        let block = REGISTRY
            .blocks
            .by_state_id(state)
            .ok_or_else(|| format!("block state id {} has no block", state.0))?;
        let index = palette_index(self.block_states.len())?;
        self.block_states.push(PersistentBlockState {
            name: block.key.clone(),
            properties: REGISTRY.blocks.properties(state).collect(),
        });
        self.block_index.insert(state, index);
        Ok(index)
    }

    fn biome(&mut self, biome: Identifier) -> Result<u16, String> {
        if let Some(&index) = self.biome_index.get(&biome) {
            return Ok(index);
        }
        if REGISTRY.biomes.id_from_key(&biome).is_none() {
            return Err(format!("unknown biome {biome}"));
        }
        let index = palette_index(self.biomes.len())?;
        self.biomes.push(biome.clone());
        self.biome_index.insert(biome, index);
        Ok(index)
    }

    /// Converts one vanilla section, or an absent one (`None`), which vanilla reads as air
    /// with plains biomes (`PalettedContainerFactory`).
    pub fn section(&mut self, section: Option<&NbtCompound>) -> Result<PersistentSection, String> {
        let block_values = match section.and_then(|s| s.compound("block_states")) {
            Some(states) => self.block_values(states)?,
            None => vec![self.block_state(vanilla_blocks::AIR.default_state())?],
        };
        let biome_values = match section.and_then(|s| s.compound("biomes")) {
            Some(biomes) => self.biome_values(biomes)?,
            None => vec![self.biome(vanilla_biomes::PLAINS.key.clone())?],
        };
        let biomes = match pack(&biome_values, BIOMES_PER_SECTION) {
            Packed::Single(biome) => PersistentBiomeData::Homogeneous { biome },
            Packed::Multiple {
                palette,
                bits_per_entry,
                data,
            } => PersistentBiomeData::Heterogeneous {
                palette,
                bits_per_entry,
                biome_data: data,
            },
        };
        Ok(match pack(&block_values, BLOCKS_PER_SECTION) {
            Packed::Single(block_state) => PersistentSection::Homogeneous {
                block_state,
                biomes,
            },
            Packed::Multiple {
                palette,
                bits_per_entry,
                data,
            } => PersistentSection::Heterogeneous {
                palette,
                bits_per_entry,
                block_data: data,
                biomes,
            },
        })
    }

    /// Chunk-palette indices for all 4096 blocks, or one value for a single-entry palette.
    fn block_values(&mut self, states: &NbtCompound) -> Result<Vec<u16>, String> {
        let palette = palette_entries(states, "block_states")?;
        let mapped: Vec<u16> = palette
            .iter()
            .map(|entry| self.block_state(resolve_block_state(entry)?))
            .collect::<Result<_, _>>()?;
        // Vanilla stores no data for a single-entry palette and at least 4 bits otherwise.
        let bits = if mapped.len() == 1 {
            0
        } else {
            ceil_log2(mapped.len()).max(4)
        };
        expand(
            &mapped,
            states.long_array("data"),
            bits,
            BLOCKS_PER_SECTION,
            "block",
        )
    }

    fn biome_values(&mut self, biomes: &NbtCompound) -> Result<Vec<u16>, String> {
        let palette = palette_entries(biomes, "biomes")?;
        let mapped: Vec<u16> = palette
            .iter()
            .map(|entry| {
                let NbtTag::String(name) = entry else {
                    return Err("biome palette entry is not a string".to_owned());
                };
                self.biome(parse_identifier(&name.to_str())?)
            })
            .collect::<Result<_, _>>()?;
        let bits = ceil_log2(mapped.len());
        expand(
            &mapped,
            biomes.long_array("data"),
            bits,
            BIOMES_PER_SECTION,
            "biome",
        )
    }
}

/// A container's `palette` list after vanilla's `ListTag` unwrapping: a list that mixes
/// strings and compounds stores each string as `{"": <string>}`.
///
/// A missing or empty palette is an error: vanilla fails the chunk read without one, and
/// throws "Use of an uninitialized palette" on the first read of an empty one.
fn palette_entries(container: &NbtCompound, name: &str) -> Result<Vec<NbtTag>, String> {
    let palette = container
        .list("palette")
        .map(nbt_list_values)
        .ok_or_else(|| format!("{name} has no palette"))?;
    if palette.is_empty() {
        return Err(format!("{name} has an empty palette"));
    }
    Ok(palette)
}

fn palette_index(len: usize) -> Result<u16, String> {
    u16::try_from(len).map_err(|_| "chunk palette has more than 65536 entries".to_owned())
}

/// Unpacks vanilla `SimpleBitStorage` data into chunk-palette indices.
fn expand(
    mapped_palette: &[u16],
    data: Option<&[i64]>,
    bits: usize,
    entries: usize,
    name: &str,
) -> Result<Vec<u16>, String> {
    if bits == 0 {
        return Ok(vec![mapped_palette[0]]);
    }
    let data = data.ok_or_else(|| format!("{name} palette has several entries but no data"))?;
    let per_word = 64 / bits;
    if data.len() != entries.div_ceil(per_word) {
        return Err(format!(
            "{name} data has {} longs, expected {} for {bits} bits",
            data.len(),
            entries.div_ceil(per_word)
        ));
    }
    let mask = (1u64 << bits) - 1;
    (0..entries)
        .map(|i| {
            let index = ((data[i / per_word] as u64) >> ((i % per_word) * bits)) & mask;
            mapped_palette.get(index as usize).copied().ok_or_else(|| {
                format!(
                    "{name} data references palette entry {index} of {}",
                    mapped_palette.len()
                )
            })
        })
        .collect()
}

enum Packed {
    Single(u16),
    Multiple {
        palette: Vec<u16>,
        bits_per_entry: u8,
        data: Box<[u64]>,
    },
}

/// Packs chunk-palette indices into Steel's section layout.
///
/// The section palette holds only values that are actually used, so a section with one
/// distinct value is always homogeneous, which Steel requires.
fn pack(values: &[u16], entries: usize) -> Packed {
    let mut palette: Vec<u16> = Vec::new();
    let mut local = FxHashMap::default();
    let indices: Vec<u32> = values
        .iter()
        .map(|&value| {
            *local.entry(value).or_insert_with(|| {
                palette.push(value);
                (palette.len() - 1) as u32
            })
        })
        .collect();
    if palette.len() == 1 {
        return Packed::Single(palette[0]);
    }
    debug_assert_eq!(indices.len(), entries);
    let bits = steel_bits_for_palette_len(palette.len());
    let per_word = 64 / usize::from(bits);
    let mut data = vec![0u64; entries.div_ceil(per_word)];
    for (i, &index) in indices.iter().enumerate() {
        data[i / per_word] |= u64::from(index) << ((i % per_word) * usize::from(bits));
    }
    Packed::Multiple {
        palette,
        bits_per_entry: bits,
        data: data.into_boxed_slice(),
    }
}

/// Steel's power-of-two widths (`steel-core/src/chunk_saver/bit_pack.rs`, private there).
const fn steel_bits_for_palette_len(len: usize) -> u8 {
    match len {
        0..=2 => 1,
        3..=4 => 2,
        5..=16 => 4,
        17..=256 => 8,
        _ => 16,
    }
}

/// Vanilla `Mth.ceillog2`.
const fn ceil_log2(value: usize) -> usize {
    if value <= 1 {
        0
    } else {
        (value - 1).bit_width() as usize
    }
}

fn parse_identifier(value: &str) -> Result<Identifier, String> {
    value
        .parse()
        .map_err(|_| format!("invalid identifier {value:?}"))
}

/// Resolves one `BlockState.CODEC` palette entry: a bare block id for a block's default
/// state, or `{id, properties}` for any other state.
///
/// Decodes like vanilla's `StateDefinition` codec: each property of the block takes its value
/// from `properties`, a missing or invalid value keeps the default state's, and keys the block
/// does not have are ignored. An unknown block is an error, where vanilla would load air.
fn resolve_block_state(entry: &NbtTag) -> Result<BlockStateId, String> {
    let (id, properties) = match entry {
        NbtTag::String(id) => (id.to_str(), None),
        NbtTag::Compound(state) => (
            state
                .string("id")
                .ok_or("block palette entry has no id")?
                .to_str(),
            state.compound("properties"),
        ),
        _ => return Err("block palette entry is neither a string nor a compound".to_owned()),
    };
    let block = REGISTRY
        .blocks
        .by_key(&parse_identifier(&id)?)
        .ok_or_else(|| format!("unknown block {id}"))?;
    let Some(properties) = properties else {
        return Ok(block.default_state());
    };
    let values: Vec<(&str, String)> = block
        .properties
        .iter()
        .filter_map(|property| {
            let value = properties.string(property.get_name())?.to_str();
            (0..property.value_count())
                .any(|index| property.value_name_from_index(index) == value)
                .then(|| (property.get_name(), value.into_owned()))
        })
        .collect();
    REGISTRY
        .blocks
        .state_id_from_block_defaulted_properties(
            block,
            values.iter().map(|(name, value)| (*name, value.as_str())),
        )
        .ok_or_else(|| format!("block {id} has no state for {values:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_distinct_value_is_homogeneous_even_with_a_larger_vanilla_palette() {
        let values = vec![7u16; BLOCKS_PER_SECTION];
        assert!(matches!(
            pack(&values, BLOCKS_PER_SECTION),
            Packed::Single(7)
        ));
    }

    #[test]
    fn vanilla_five_bit_data_repacks_to_steel_eight_bits() {
        // 17 palette entries: vanilla uses 5 bits (12 per long), Steel 8 bits.
        let mapped: Vec<u16> = (100..117).collect();
        let per_word = 64 / 5;
        let mut data = vec![0i64; BLOCKS_PER_SECTION.div_ceil(per_word)];
        for i in 0..BLOCKS_PER_SECTION {
            data[i / per_word] |= ((i % 17) as i64) << ((i % per_word) * 5);
        }
        let values = expand(&mapped, Some(&data), 5, BLOCKS_PER_SECTION, "block")
            .expect("valid vanilla data");
        assert_eq!(values[18], 101);

        let Packed::Multiple {
            palette,
            bits_per_entry,
            data,
        } = pack(&values, BLOCKS_PER_SECTION)
        else {
            panic!("17 distinct values must be heterogeneous");
        };
        assert_eq!(bits_per_entry, 8);
        assert_eq!(data.len(), 512);
        // Entry 18 is palette index 1 in the second byte of word 2.
        assert_eq!(palette[((data[2] >> 16) & 0xFF) as usize], 101);
    }
}
