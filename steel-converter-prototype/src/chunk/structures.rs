//! Structure starts and references (`WORLD_STORAGE.md` §4.9).
//!
//! Prototype scope: jigsaw, mineshaft, and the scattered-feature families (desert pyramid,
//! jungle temple, swamp hut, buried treasure) keep their full placement payload. Other
//! families (template-backed, stronghold, fortress, ocean monument) and pieces whose data
//! Steel cannot represent are written as `Procedural(Unimplemented)` with their real piece
//! type, bounding box, junctions and projection. That is the documented fallback: the start
//! still exists for `/locate` and references, but chunks Steel generates later place none of
//! that piece. The report counts every such piece.

use std::collections::BTreeMap;

use simdnbt::owned::{NbtCompound, NbtTag};
use steel_core::chunk_saver::{
    PersistentBoundingBox, PersistentDesertPyramidPieceData, PersistentJigsawJunction,
    PersistentJigsawPieceData, PersistentJungleTemplePieceData, PersistentMineshaftPieceData,
    PersistentMineshaftPieceKind, PersistentPoolElement, PersistentProceduralPieceData,
    PersistentProcessorList, PersistentStructurePiece, PersistentStructurePiecePayload,
    PersistentStructureReference, PersistentStructureStart, PersistentSwampHutPieceData,
};
use steel_utils::{Identifier, PackedChunkPos};

use crate::nbt;

/// Pieces written without placement data, counted by vanilla piece id.
pub type UnimplementedPieces = BTreeMap<String, usize>;

/// Converts vanilla `structures.References`.
pub fn references(structures: &NbtCompound) -> Result<Vec<PersistentStructureReference>, String> {
    let Some(references) = structures.compound("References") else {
        return Ok(Vec::new());
    };
    references
        .iter()
        .filter_map(|(key, value)| {
            let positions = value.long_array()?;
            (!positions.is_empty()).then(|| {
                Ok(PersistentStructureReference {
                    structure: identifier(&key.to_str())?,
                    references: positions
                        .iter()
                        .copied()
                        .map(PackedChunkPos::from_raw)
                        .collect(),
                })
            })
        })
        .collect()
}

/// Converts vanilla `structures.starts`, skipping `INVALID` starts, which Steel represents by
/// absence.
pub fn starts(
    structures: &NbtCompound,
    unimplemented: &mut UnimplementedPieces,
) -> Result<Vec<PersistentStructureStart>, String> {
    let Some(starts) = structures.compound("starts") else {
        return Ok(Vec::new());
    };
    let mut converted = Vec::new();
    for (key, start) in starts.iter() {
        let Some(start) = start.compound() else {
            continue;
        };
        if nbt::string(start, "id").as_deref() == Some("INVALID") {
            continue;
        }
        let pieces: Vec<PersistentStructurePiece> = nbt::compounds(start, "Children")
            .iter()
            .map(|piece| self::piece(piece, unimplemented))
            .collect::<Result<_, _>>()?;
        if pieces.is_empty() {
            continue;
        }
        let structure = key.to_str();
        converted.push(PersistentStructureStart {
            structure: identifier(&structure)?,
            chunk_x: start
                .int("ChunkX")
                .ok_or_else(|| format!("{structure} start has no ChunkX"))?,
            chunk_z: start
                .int("ChunkZ")
                .ok_or_else(|| format!("{structure} start has no ChunkZ"))?,
            references: start.int("references").unwrap_or_default(),
            pieces,
        });
    }
    Ok(converted)
}

/// Whether the chunk holds a start vanilla would load, so an `empty` chunk is worth keeping.
pub fn has_valid_start(structures: &NbtCompound) -> bool {
    structures.compound("starts").is_some_and(|starts| {
        starts.values().any(|start| {
            start.compound().is_some_and(|start| {
                nbt::string(start, "id").as_deref() != Some("INVALID")
                    && !nbt::compounds(start, "Children").is_empty()
            })
        })
    })
}

fn piece(
    piece: &NbtCompound,
    unimplemented: &mut UnimplementedPieces,
) -> Result<PersistentStructurePiece, String> {
    let id = nbt::string(piece, "id").ok_or("structure piece has no id")?;
    let bounding_box =
        bounding_box(piece.int_array("BB")).ok_or_else(|| format!("{id} piece has no BB"))?;
    let orientation = piece.int("O").unwrap_or(-1) as i8;
    let gen_depth = piece.int("GD").unwrap_or_default();

    let (payload, ground_level_delta, projection, junctions) = if id == "minecraft:jigsaw" {
        let element = piece
            .compound("pool_element")
            .ok_or("jigsaw piece has no pool_element")?;
        let projection = element_projection(element)?;
        let junctions = nbt::compounds(piece, "junctions")
            .iter()
            .map(junction)
            .collect::<Result<_, _>>()?;
        let payload = match jigsaw_payload(piece, element)? {
            Some(payload) => payload,
            None => unimplemented_payload(&id, unimplemented),
        };
        (
            payload,
            piece.int("ground_level_delta").unwrap_or(1),
            projection,
            junctions,
        )
    } else {
        let payload = match procedural_payload(&id, piece) {
            Some(payload) => PersistentStructurePiecePayload::Procedural(payload),
            None => unimplemented_payload(&id, unimplemented),
        };
        // Vanilla's non-pool pieces: ground level 0, no projection, no junctions.
        (payload, 0, -1, Vec::new())
    };

    Ok(PersistentStructurePiece {
        piece_type: identifier(&id)?,
        bounding_box,
        gen_depth,
        orientation,
        payload,
        ground_level_delta,
        projection,
        junctions,
    })
}

fn unimplemented_payload(
    id: &str,
    unimplemented: &mut UnimplementedPieces,
) -> PersistentStructurePiecePayload {
    *unimplemented.entry(id.to_owned()).or_default() += 1;
    PersistentStructurePiecePayload::Procedural(PersistentProceduralPieceData::Unimplemented)
}

/// `PoolElementStructurePiece` data. `None` when the element uses data Steel's pool elements
/// cannot hold (inline processor lists or features, or `override_liquid_settings`).
fn jigsaw_payload(
    piece: &NbtCompound,
    element: &NbtCompound,
) -> Result<Option<PersistentStructurePiecePayload>, String> {
    let Some(pool_element) = pool_element(element)? else {
        return Ok(None);
    };
    let (Some(x), Some(y), Some(z)) = (piece.int("PosX"), piece.int("PosY"), piece.int("PosZ"))
    else {
        return Err("jigsaw piece has no PosX/PosY/PosZ".to_owned());
    };
    let rotation =
        rotation(&nbt::string(piece, "rotation").ok_or("jigsaw piece has no rotation")?)?;
    // Absent means vanilla's `JigsawStructure.DEFAULT_LIQUID_SETTINGS`.
    let liquid_settings = match nbt::string(piece, "liquid_settings").as_deref() {
        None | Some("apply_waterlogging") => 0,
        Some("ignore_waterlogging") => 1,
        Some(other) => return Err(format!("unknown liquid_settings {other:?}")),
    };
    Ok(Some(PersistentStructurePiecePayload::Jigsaw(
        PersistentJigsawPieceData {
            pool_element,
            position: [x, y, z],
            rotation,
            liquid_settings,
        },
    )))
}

fn pool_element(element: &NbtCompound) -> Result<Option<PersistentPoolElement>, String> {
    let element_type =
        nbt::string(element, "element_type").ok_or("pool element has no element_type")?;
    let projection = element_projection(element)?;
    Ok(match element_type.as_str() {
        "minecraft:empty_pool_element" => Some(PersistentPoolElement::Empty),
        "minecraft:single_pool_element" | "minecraft:legacy_single_pool_element" => {
            if element.contains("override_liquid_settings") {
                return Ok(None);
            }
            let Some(processors) = processors(element.get("processors"))? else {
                return Ok(None);
            };
            let location = identifier(
                &nbt::string(element, "location").ok_or("pool element has no location")?,
            )?;
            Some(if element_type == "minecraft:single_pool_element" {
                PersistentPoolElement::Single {
                    location,
                    processors,
                    projection,
                }
            } else {
                PersistentPoolElement::LegacySingle {
                    location,
                    processors,
                    projection,
                }
            })
        }
        "minecraft:feature_pool_element" => {
            let Some(feature) = nbt::string(element, "feature") else {
                return Ok(None);
            };
            Some(PersistentPoolElement::Feature {
                feature: identifier(&feature)?,
                projection,
            })
        }
        "minecraft:list_pool_element" => {
            let mut elements = Vec::new();
            for child in nbt::compounds(element, "elements") {
                let Some(child) = pool_element(child)? else {
                    return Ok(None);
                };
                elements.push(child);
            }
            Some(PersistentPoolElement::List {
                elements,
                projection,
            })
        }
        other => return Err(format!("unknown pool element type {other:?}")),
    })
}

/// A processor list is either a registry id or an inline `{processors: [...]}` list; Steel
/// can hold the id or an empty inline list.
fn processors(tag: Option<&NbtTag>) -> Result<Option<PersistentProcessorList>, String> {
    match tag {
        Some(NbtTag::String(id)) => Ok(Some(PersistentProcessorList::Registry(identifier(
            &id.to_str(),
        )?))),
        Some(NbtTag::Compound(inline)) if nbt::compounds(inline, "processors").is_empty() => {
            Ok(Some(PersistentProcessorList::Empty))
        }
        Some(NbtTag::Compound(_)) => Ok(None),
        _ => Err("pool element has no processors".to_owned()),
    }
}

/// Pool element projection as Steel's `i8`. Vanilla's empty element has no `projection`
/// field; Steel treats it as rigid.
fn element_projection(element: &NbtCompound) -> Result<i8, String> {
    match nbt::string(element, "projection").as_deref() {
        None | Some("rigid") => Ok(0),
        Some("terrain_matching") => Ok(1),
        Some(other) => Err(format!("unknown projection {other:?}")),
    }
}

fn junction(junction: &NbtCompound) -> Result<PersistentJigsawJunction, String> {
    Ok(PersistentJigsawJunction {
        source_x: junction.int("source_x").unwrap_or_default(),
        source_ground_y: junction.int("source_ground_y").unwrap_or_default(),
        source_z: junction.int("source_z").unwrap_or_default(),
        delta_y: junction.int("delta_y").unwrap_or_default(),
        dest_projection: match nbt::string(junction, "dest_proj").as_deref() {
            Some("rigid") => 0,
            Some("terrain_matching") => 1,
            other => return Err(format!("unknown junction dest_proj {other:?}")),
        },
    })
}

/// Vanilla `Rotation.LEGACY_CODEC` names.
fn rotation(name: &str) -> Result<i8, String> {
    match name {
        "NONE" => Ok(0),
        "CLOCKWISE_90" => Ok(1),
        "CLOCKWISE_180" => Ok(2),
        "COUNTERCLOCKWISE_90" => Ok(3),
        other => Err(format!("unknown rotation {other:?}")),
    }
}

/// Procedural families whose vanilla piece NBT maps field for field.
fn procedural_payload(id: &str, piece: &NbtCompound) -> Option<PersistentProceduralPieceData> {
    let flag = |key: &str| nbt::boolean(piece, key).unwrap_or_default();
    // `ScatteredFeaturePiece.heightPosition`; vanilla starts it at -1.
    let height_position = piece.int("HPos").unwrap_or(-1);
    let mineshaft = |kind| {
        Some(PersistentProceduralPieceData::Mineshaft(
            PersistentMineshaftPieceData {
                mineshaft_type: piece.int("MST").unwrap_or_default() as i8,
                kind,
            },
        ))
    };
    match id {
        "minecraft:btp" => Some(PersistentProceduralPieceData::BuriedTreasure),
        "minecraft:tedp" => Some(PersistentProceduralPieceData::DesertPyramid(
            PersistentDesertPyramidPieceData {
                height_position,
                has_placed_chest: [0, 1, 2, 3].map(|i| flag(&format!("hasPlacedChest{i}"))),
            },
        )),
        "minecraft:tejp" => Some(PersistentProceduralPieceData::JungleTemple(
            PersistentJungleTemplePieceData {
                height_position,
                placed_main_chest: flag("placedMainChest"),
                placed_hidden_chest: flag("placedHiddenChest"),
                placed_trap1: flag("placedTrap1"),
                placed_trap2: flag("placedTrap2"),
            },
        )),
        "minecraft:tesh" => Some(PersistentProceduralPieceData::SwampHut(
            PersistentSwampHutPieceData {
                height_position,
                spawned_witch: flag("Witch"),
                spawned_cat: flag("Cat"),
            },
        )),
        "minecraft:msroom" => mineshaft(PersistentMineshaftPieceKind::Room {
            child_entrance_boxes: piece
                .list("Entrances")
                .and_then(|list| list.int_arrays())
                .unwrap_or_default()
                .iter()
                .filter_map(|bb| bounding_box(Some(bb.as_slice())))
                .collect(),
        }),
        "minecraft:mscorridor" => mineshaft(PersistentMineshaftPieceKind::Corridor {
            has_rails: flag("hr"),
            spider_corridor: flag("sc"),
            has_placed_spider: flag("hps"),
            num_sections: piece.int("Num").unwrap_or_default(),
        }),
        "minecraft:mscrossing" => mineshaft(PersistentMineshaftPieceKind::Crossing {
            // `Direction.LEGACY_ID_CODEC_2D`, the same 0=S 1=W 2=N 3=E order Steel uses.
            direction: piece.int("D").unwrap_or_default() as i8,
            is_two_floored: flag("tf"),
        }),
        "minecraft:msstairs" => mineshaft(PersistentMineshaftPieceKind::Stairs),
        _ => None,
    }
}

fn bounding_box(bb: Option<&[i32]>) -> Option<PersistentBoundingBox> {
    let &[min_x, min_y, min_z, max_x, max_y, max_z] = bb? else {
        return None;
    };
    Some(PersistentBoundingBox {
        min_x,
        min_y,
        min_z,
        max_x,
        max_y,
        max_z,
    })
}

fn identifier(value: &str) -> Result<Identifier, String> {
    value
        .parse()
        .map_err(|_| format!("invalid identifier {value:?}"))
}
