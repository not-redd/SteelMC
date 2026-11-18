//! Entities (`WORLD_STORAGE.md` §4.6).
//!
//! Vanilla keeps full-chunk entities in `entities/*.mca` and proto-chunk entities in the
//! chunk's own `entities` list. Steel stores both inside the chunk.

use std::collections::BTreeMap;

use rustc_hash::FxHashSet;
use simdnbt::owned::NbtCompound;
use steel_core::chunk_saver::PersistentEntity;
use uuid::Uuid;

use crate::nbt::{self, int};

/// Vanilla keys that map to `PersistentEntity` fields. Everything else is the type's own
/// `addAdditionalSaveData` output and goes into `nbt_data`.
const BASE_KEYS: [&str; 21] = [
    "id",
    "UUID",
    "Pos",
    "Motion",
    "Rotation",
    "fall_distance",
    "Fire",
    "TicksFrozen",
    "HasVisualFire",
    "OnGround",
    "NoGravity",
    "Invulnerable",
    "Air",
    "PortalCooldown",
    "CustomName",
    "CustomNameVisible",
    "Silent",
    "Glowing",
    "Tags",
    "data",
    "Passengers",
];

/// Entity types whose `EntityType.canSerialize()` is false. Steel never stores them.
const NOT_SERIALIZABLE: [&str; 4] = [
    "minecraft:player",
    "minecraft:leash_knot",
    "minecraft:lightning_bolt",
    "minecraft:fishing_bobber",
];

/// Vanilla `Entity.getMaxAirSupply()` for nearly every type, used when `Air` is absent.
const DEFAULT_AIR_SUPPLY: i32 = 300;

/// Converts vanilla entities while keeping UUIDs unique across one dimension.
///
/// Steel drops a full-chunk entity tree whose UUID is already loaded and crashes on saving a
/// proto chunk that holds two equal UUIDs, so duplicates are dropped here.
#[derive(Default)]
pub struct EntityConverter {
    seen_uuids: FxHashSet<[u8; 16]>,
    /// Converted root-entity counts by type.
    pub converted: BTreeMap<String, usize>,
    /// Entities that were dropped, with the reason.
    pub dropped: BTreeMap<String, usize>,
}

impl EntityConverter {
    /// Converts one root entity and its passengers. Returns `None` if the entity is dropped.
    pub fn convert_root(&mut self, entity: &NbtCompound) -> Option<PersistentEntity> {
        let converted = self.convert(entity)?;
        *self
            .converted
            .entry(converted.entity_type.to_string())
            .or_default() += 1;
        Some(converted)
    }

    /// Records that a root entity could not be placed in any converted chunk.
    pub fn drop_unplaced(&mut self) {
        self.drop("no converted chunk at its position");
    }

    fn drop(&mut self, reason: &str) {
        *self.dropped.entry(reason.to_owned()).or_default() += 1;
    }

    fn convert(&mut self, entity: &NbtCompound) -> Option<PersistentEntity> {
        let Some(id) = nbt::string(entity, "id") else {
            self.drop("missing id");
            return None;
        };
        if NOT_SERIALIZABLE.contains(&id.as_str()) {
            self.drop("type is never saved");
            return None;
        }
        let Ok(entity_type) = id.parse() else {
            self.drop("invalid id");
            return None;
        };
        let Some(pos) = vec3(entity, "Pos") else {
            self.drop("missing Pos");
            return None;
        };
        let uuid = match entity.int_array("UUID") {
            Some(ints) => {
                let Some(uuid) = nbt::uuid_bytes(ints) else {
                    self.drop("malformed UUID");
                    return None;
                };
                uuid
            }
            // Vanilla keeps the random UUID the entity was constructed with.
            None => *Uuid::new_v4().as_bytes(),
        };
        if !self.seen_uuids.insert(uuid) {
            self.drop("duplicate UUID");
            return None;
        }

        let passengers = nbt::compounds(entity, "Passengers")
            .iter()
            .filter_map(|passenger| self.convert(passenger))
            .collect();
        let rotation = nbt::floats(entity, "Rotation").unwrap_or_default();

        let mut additional = entity.clone();
        for key in BASE_KEYS {
            additional.remove(key);
        }

        Some(PersistentEntity {
            entity_type,
            uuid,
            pos,
            motion: vec3(entity, "Motion").unwrap_or_default(),
            rotation: [
                rotation.first().copied().unwrap_or_default(),
                rotation.get(1).copied().unwrap_or_default(),
            ],
            fall_distance: nbt::double(entity, "fall_distance").unwrap_or_default(),
            remaining_fire_ticks: int(entity, "Fire").unwrap_or_default(),
            ticks_frozen: int(entity, "TicksFrozen").unwrap_or_default(),
            is_in_powder_snow: false,
            was_in_powder_snow: false,
            has_visual_fire: nbt::boolean(entity, "HasVisualFire").unwrap_or_default(),
            on_ground: nbt::boolean(entity, "OnGround").unwrap_or_default(),
            no_gravity: nbt::boolean(entity, "NoGravity").unwrap_or_default(),
            invulnerable: nbt::boolean(entity, "Invulnerable").unwrap_or_default(),
            air_supply: int(entity, "Air").unwrap_or(DEFAULT_AIR_SUPPLY),
            portal_cooldown: int(entity, "PortalCooldown").unwrap_or_default(),
            custom_name_nbt: entity.get("CustomName").map_or_else(Vec::new, |name| {
                let mut root = NbtCompound::new();
                root.insert("CustomName", name.clone());
                nbt::compound_body(&root)
            }),
            custom_name_visible: nbt::boolean(entity, "CustomNameVisible").unwrap_or_default(),
            silent: nbt::boolean(entity, "Silent").unwrap_or_default(),
            glowing: nbt::boolean(entity, "Glowing").unwrap_or_default(),
            tags: entity
                .list("Tags")
                .and_then(|tags| tags.strings())
                .unwrap_or_default()
                .iter()
                .map(|tag| tag.to_str().into_owned())
                .collect(),
            custom_data_nbt: entity
                .compound("data")
                .map_or_else(Vec::new, nbt::optional_compound_body),
            nbt_data: nbt::compound_body(&additional),
            passengers,
        })
    }
}

/// The chunk an entity belongs in, from its `Pos`.
pub fn chunk_of(entity: &NbtCompound) -> Option<(i32, i32)> {
    let [x, _, z] = vec3(entity, "Pos")?;
    Some(((x.floor() as i32) >> 4, (z.floor() as i32) >> 4))
}

fn vec3(compound: &NbtCompound, key: &str) -> Option<[f64; 3]> {
    match nbt::doubles(compound, key)?.as_slice() {
        &[x, y, z] => Some([x, y, z]),
        _ => None,
    }
}
