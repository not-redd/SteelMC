//! Scheduled block and fluid ticks (`WORLD_STORAGE.md` §4.7).

use simdnbt::owned::NbtCompound;
use steel_core::chunk_saver::PersistentTick;

use super::{ChunkContext, DimensionLayout};
use crate::nbt;

/// Converts a vanilla `block_ticks`/`fluid_ticks` list (`SavedTick`: `i`, `x`, `y`, `z`, `t`,
/// `p`), keeping vanilla's order, which is the sub-tick order on load.
///
/// Ticks outside the chunk are dropped, as vanilla's `SavedTick.filterTickListForChunk` does.
/// Steel would reject the whole chunk for them.
pub fn convert(
    ticks: &[NbtCompound],
    context: ChunkContext,
    layout: &DimensionLayout,
    dropped: &mut usize,
) -> Result<Vec<PersistentTick>, String> {
    let mut converted = Vec::with_capacity(ticks.len());
    for tick in ticks {
        let (Some(x), Some(y), Some(z), Some(delay), Some(priority)) = (
            tick.int("x"),
            tick.int("y"),
            tick.int("z"),
            tick.int("t"),
            tick.int("p"),
        ) else {
            return Err("scheduled tick is missing a field".to_owned());
        };
        if !context.contains_block(x, z) || !layout.contains_y(y) {
            *dropped += 1;
            continue;
        }
        let id = nbt::string(tick, "i").ok_or("scheduled tick has no type")?;
        converted.push(PersistentTick {
            x: x.rem_euclid(16) as u8,
            y: y as i16,
            z: z.rem_euclid(16) as u8,
            delay,
            priority: priority.clamp(i32::from(i8::MIN), i32::from(i8::MAX)) as i8,
            tick_type: id
                .parse()
                .map_err(|_| format!("invalid tick type {id:?}"))?,
        });
    }
    Ok(converted)
}
