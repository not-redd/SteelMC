//! Block entities (`WORLD_STORAGE.md` §4.5).

use simdnbt::owned::NbtCompound;
use steel_core::chunk_saver::PersistentBlockEntity;

use super::{ChunkContext, DimensionLayout};
use crate::nbt;

/// Vanilla's pending block-entity marker id.
const DUMMY: &str = "DUMMY";

/// Keys Steel stores outside `nbt_data`. `keepPacked` only tells vanilla that the entry is
/// pending NBT; Steel decides that from the entry itself.
const METADATA_KEYS: [&str; 5] = ["id", "x", "y", "z", "keepPacked"];

/// Converts the chunk's block entities. Entries outside the chunk are dropped, as vanilla
/// does when it loads them; Steel would reject the whole chunk instead.
pub fn convert(
    block_entities: &[NbtCompound],
    context: ChunkContext,
    layout: &DimensionLayout,
    dropped: &mut usize,
) -> Result<Vec<PersistentBlockEntity>, String> {
    let mut converted = Vec::with_capacity(block_entities.len());
    for entry in block_entities {
        let (Some(x), Some(y), Some(z)) = (entry.int("x"), entry.int("y"), entry.int("z")) else {
            return Err("block entity has no position".to_owned());
        };
        if !context.contains_block(x, z) || !layout.contains_y(y) {
            *dropped += 1;
            continue;
        }
        let id = nbt::string(entry, "id").ok_or("block entity has no id")?;
        let entity_type = if id == DUMMY {
            None
        } else {
            Some(
                id.parse()
                    .map_err(|_| format!("invalid block entity id {id:?}"))?,
            )
        };
        let mut data = entry.clone();
        for key in METADATA_KEYS {
            data.remove(key);
        }
        converted.push(PersistentBlockEntity {
            x: x.rem_euclid(16) as u8,
            y: y as i16,
            z: z.rem_euclid(16) as u8,
            entity_type,
            nbt_data: if id == DUMMY {
                Vec::new()
            } else {
                nbt::compound_body(&data)
            },
        });
    }
    Ok(converted)
}
