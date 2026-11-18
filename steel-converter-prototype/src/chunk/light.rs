//! Block and sky light (`WORLD_STORAGE.md` §4.4).

use simdnbt::owned::NbtCompound;
use steel_core::chunk_saver::{PersistentLightData, PersistentLightSection};

use super::DimensionLayout;

/// Bytes in a vanilla `DataLayer`: 4096 nibbles.
const DATA_LAYER_BYTES: usize = 2048;

/// Copies vanilla `BlockLight`/`SkyLight` arrays as initialized sections.
///
/// Both formats use the same nibble layout, so the arrays copy unchanged. Steel's light
/// sections are indexed from one padding section below the world. Sections without an array
/// are left out, which Steel reads the same way vanilla does.
pub fn convert(
    sections: &[NbtCompound],
    layout: &DimensionLayout,
) -> Result<PersistentLightData, String> {
    let lowest_light_section = layout.min_y.div_euclid(16) - 1;
    let light_section_count = layout.height / 16 + 2;
    let mut light = PersistentLightData::default();
    for section in sections {
        let y = i32::from(section.byte("Y").ok_or("section has no Y")?);
        let index = y - lowest_light_section;
        if !(0..light_section_count).contains(&index) {
            continue;
        }
        let index = index as u32;
        for (key, layer, enabled) in [
            ("BlockLight", &mut light.block, true),
            ("SkyLight", &mut light.sky, layout.has_skylight),
        ] {
            if !enabled {
                continue;
            }
            let Some(data) = section.byte_array(key) else {
                continue;
            };
            if data.len() != DATA_LAYER_BYTES {
                return Err(format!(
                    "section {y} {key} has {} bytes, expected {DATA_LAYER_BYTES}",
                    data.len()
                ));
            }
            layer.push(PersistentLightSection::Initialized {
                section_index: index,
                data: data.to_vec(),
            });
        }
    }
    Ok(light)
}
