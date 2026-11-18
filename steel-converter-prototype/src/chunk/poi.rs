//! Points of interest (`WORLD_STORAGE.md` §4.10).

use simdnbt::owned::NbtCompound;
use steel_core::chunk_saver::PersistentPoi;

use super::{ChunkContext, DimensionLayout};
use crate::nbt;

/// Converts one vanilla POI chunk (`{Sections: {"<y>": {Valid, Records}}}`) to Steel's ticket
/// state.
///
/// Steel finds POIs from block states on load and only takes `free_tickets` from the file.
/// Records of a section vanilla marked invalid are skipped: vanilla rebuilds such a section
/// from the blocks with full tickets (`PoiSection.refresh`), and Steel does the same for
/// positions without an entry.
pub fn convert(
    poi_chunk: &NbtCompound,
    context: ChunkContext,
    layout: &DimensionLayout,
) -> Result<Vec<PersistentPoi>, String> {
    let Some(sections) = poi_chunk.compound("Sections") else {
        return Ok(Vec::new());
    };
    let mut pois = Vec::new();
    for section in sections.values() {
        let Some(section) = section.compound() else {
            continue;
        };
        if !nbt::boolean(section, "Valid").unwrap_or_default() {
            continue;
        }
        for record in nbt::compounds(section, "Records") {
            let &[x, y, z] = record.int_array("pos").ok_or("POI record has no pos")? else {
                return Err("POI record pos is not three ints".to_owned());
            };
            if !context.contains_block(x, z) || !layout.contains_y(y) {
                continue;
            }
            pois.push(PersistentPoi {
                x: x.rem_euclid(16) as u8,
                y: y as i16,
                z: z.rem_euclid(16) as u8,
                free_tickets: record.int("free_tickets").unwrap_or_default().max(0) as u32,
            });
        }
    }
    Ok(pois)
}
