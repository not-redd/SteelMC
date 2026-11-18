//! Player data (`WORLD_STORAGE.md` §7).
//!
//! `PlayerDataFile` and its parts are private to `steel-core`, so their wire layout is
//! reproduced here field for field. Steel rejects the whole player file over one bad item,
//! so every item is decoded with Steel's own `ItemStack` first, and dropped (and reported) if
//! Steel cannot read it.

mod progress;

use std::{
    fs,
    io::{self, Cursor},
    path::{Path, PathBuf},
};

use simdnbt::{
    borrow,
    owned::{NbtCompound, NbtTag},
};
use steel_core::chunk_saver::PersistentEntity;
use steel_registry::item_stack::ItemStack;
use uuid::Uuid;
use wincode::SchemaWrite;

use crate::{ConvertError, chunk::EntityConverter, nbt, report::ConversionReport, version};

const PLAYER_MAGIC: [u8; 4] = *b"STLP";
const PLAYER_STORAGE_VERSION: u16 = 10;
const PLAYER_DATA_VERSION: i32 = 7;
const GLOBAL_MAGIC: [u8; 4] = *b"STLG";
const GLOBAL_STORAGE_VERSION: u16 = 1;
const GLOBAL_PLAYER_DATA_VERSION: i32 = 1;
/// Steel slots for vanilla `equipment` (§7.4).
const EQUIPMENT_SLOTS: [(&str, i8); 7] = [
    ("feet", 36),
    ("legs", 37),
    ("chest", 38),
    ("head", 39),
    ("offhand", 40),
    ("body", 41),
    ("saddle", 42),
];
/// Vanilla `Player` defaults for fields a player file may leave out.
const DEFAULT_HEALTH: f32 = 20.0;
const DEFAULT_FOOD_LEVEL: i32 = 20;
const DEFAULT_SATURATION: f32 = 5.0;
const DEFAULT_FLYING_SPEED: f32 = 0.05;
const DEFAULT_WALKING_SPEED: f32 = 0.1;

#[derive(SchemaWrite)]
struct PlayerDataFile {
    data_version: i32,
    pos: [f64; 3],
    motion: [f64; 3],
    rotation: [f32; 2],
    on_ground: bool,
    fall_flying: bool,
    remaining_fire_ticks: i32,
    ticks_frozen: i32,
    is_in_powder_snow: bool,
    was_in_powder_snow: bool,
    has_visual_fire: bool,
    health: f32,
    game_mode: i32,
    prev_game_mode: Option<i32>,
    abilities: AbilitiesFile,
    inventory: Vec<SlotFile>,
    selected_slot: i32,
    world: String,
    food_level: i32,
    food_saturation_level: f32,
    food_exhaustion_level: f32,
    food_tick_timer: i32,
    experience_level: i32,
    experience_progress: f32,
    experience_total: i32,
    enchantment_seed: i32,
    score: i32,
    seen_credits: bool,
    root_vehicle: Option<RootVehicleFile>,
    respawn_config: Option<RespawnConfigFile>,
    ender_pearls: Vec<EnderPearlFile>,
    ender_items: Vec<SlotFile>,
}

#[derive(SchemaWrite)]
struct RootVehicleFile {
    attach: [u8; 16],
    entity: PersistentEntity,
}

#[derive(SchemaWrite)]
struct RespawnConfigFile {
    dimension: String,
    pos: [i32; 3],
    yaw: f32,
    pitch: f32,
    forced: bool,
}

#[derive(SchemaWrite)]
struct EnderPearlFile {
    world: String,
    entity: PersistentEntity,
}

#[derive(SchemaWrite)]
struct AbilitiesFile {
    invulnerable: bool,
    flying: bool,
    may_fly: bool,
    instabuild: bool,
    may_build: bool,
    flying_speed: f32,
    walking_speed: f32,
}

#[derive(SchemaWrite)]
struct SlotFile {
    slot: i8,
    item_nbt: Vec<u8>,
}

#[derive(SchemaWrite)]
struct GlobalPlayerDataFile {
    data_version: i32,
    last_active_domain: String,
}

/// Where converted players go and what they may refer to.
pub struct PlayerTarget<'a> {
    /// The Steel save root (`save_path`).
    pub save_root: &'a Path,
    /// The domain the vanilla dimensions became.
    pub domain: &'a str,
    /// Steel world keys that exist after conversion.
    pub worlds: &'a [String],
    /// Vanilla `level.dat` `GameType`, used when a player file has none.
    pub default_game_mode: i32,
}

/// Converts every player in `players/`.
pub fn convert_players(
    input: &Path,
    target: &PlayerTarget<'_>,
    report: &mut ConversionReport,
) -> Result<(), ConvertError> {
    let players_dir = input.join("players");
    let domain_dir = target.save_root.join(target.domain).join("players");
    for (uuid, path) in player_files(&players_dir.join("data"), "dat")? {
        let vanilla = nbt::read_gzip_file(&path)?;
        version::check_data_version(&path, &vanilla)?;
        let file = convert_player(&vanilla, uuid, target, report);
        write_envelope(
            &domain_dir.join("data").join(uuid.to_string()),
            PLAYER_MAGIC,
            PLAYER_STORAGE_VERSION,
            wincode::serialize(&file),
        )?;
        write_envelope(
            &target
                .save_root
                .join("global/players/data")
                .join(uuid.to_string()),
            GLOBAL_MAGIC,
            GLOBAL_STORAGE_VERSION,
            wincode::serialize(&GlobalPlayerDataFile {
                data_version: GLOBAL_PLAYER_DATA_VERSION,
                last_active_domain: target.domain.to_owned(),
            }),
        )?;
        report.players += 1;
    }
    for (uuid, path) in player_files(&players_dir.join("stats"), "json")? {
        progress::convert_stats(&path, &domain_dir.join(format!("stats/{uuid}.toml")))?;
    }
    for (uuid, path) in player_files(&players_dir.join("advancements"), "json")? {
        progress::convert_advancements(
            &path,
            &domain_dir.join(format!("advancements/{uuid}.json")),
        )?;
    }
    Ok(())
}

/// `<uuid>.<extension>` files in `dir`. Vanilla's `.dat_old` backups are skipped.
fn player_files(dir: &Path, extension: &str) -> Result<Vec<(Uuid, PathBuf)>, ConvertError> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(ConvertError::io(dir, error)),
    };
    let mut files = Vec::new();
    for entry in entries {
        let path = entry.map_err(|e| ConvertError::io(dir, e))?.path();
        if path.extension().and_then(|e| e.to_str()) != Some(extension) {
            continue;
        }
        let Some(uuid) = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| Uuid::parse_str(stem).ok())
        else {
            continue;
        };
        files.push((uuid, path));
    }
    files.sort();
    Ok(files)
}

fn convert_player(
    vanilla: &NbtCompound,
    uuid: Uuid,
    target: &PlayerTarget<'_>,
    report: &mut ConversionReport,
) -> PlayerDataFile {
    let player = uuid.to_string();
    let (inventory, ender_items) = ItemConverter {
        player: &player,
        report,
    }
    .player_items(vanilla);
    let (root_vehicle, ender_pearls) = carried_entities(vanilla);

    let world =
        nbt::string(vanilla, "Dimension").unwrap_or_else(|| "minecraft:overworld".to_owned());
    if !target.worlds.contains(&world) {
        report.warnings.push(format!(
            "player {player} is in {world}, which was not converted"
        ));
    }
    let respawn_config = vanilla.compound("respawn").and_then(|respawn| {
        let &[x, y, z] = respawn.int_array("pos")? else {
            return None;
        };
        Some(RespawnConfigFile {
            dimension: nbt::string(respawn, "dimension")
                .unwrap_or_else(|| "minecraft:overworld".to_owned()),
            pos: [x, y, z],
            yaw: nbt::float(respawn, "yaw").unwrap_or_default(),
            pitch: nbt::float(respawn, "pitch").unwrap_or_default(),
            forced: nbt::boolean(respawn, "forced").unwrap_or_default(),
        })
    });

    let abilities = vanilla.compound("abilities");
    let ability = |key: &str, default: bool| {
        abilities
            .and_then(|a| nbt::boolean(a, key))
            .unwrap_or(default)
    };
    let rotation = nbt::floats(vanilla, "Rotation").unwrap_or_default();
    PlayerDataFile {
        data_version: PLAYER_DATA_VERSION,
        pos: vec3(vanilla, "Pos"),
        motion: vec3(vanilla, "Motion"),
        rotation: [
            rotation.first().copied().unwrap_or_default(),
            rotation.get(1).copied().unwrap_or_default(),
        ],
        on_ground: nbt::boolean(vanilla, "OnGround").unwrap_or_default(),
        fall_flying: nbt::boolean(vanilla, "FallFlying").unwrap_or_default(),
        remaining_fire_ticks: nbt::int(vanilla, "Fire").unwrap_or_default(),
        ticks_frozen: nbt::int(vanilla, "TicksFrozen").unwrap_or_default(),
        is_in_powder_snow: false,
        was_in_powder_snow: false,
        has_visual_fire: nbt::boolean(vanilla, "HasVisualFire").unwrap_or_default(),
        health: nbt::float(vanilla, "Health").unwrap_or(DEFAULT_HEALTH),
        game_mode: nbt::int(vanilla, "playerGameType").unwrap_or(target.default_game_mode),
        // Vanilla writes -1 for "none" (`GameType.byNullableId`).
        prev_game_mode: nbt::int(vanilla, "previousPlayerGameType")
            .filter(|mode| (0..=3).contains(mode)),
        abilities: AbilitiesFile {
            invulnerable: ability("invulnerable", false),
            flying: ability("flying", false),
            may_fly: ability("mayfly", false),
            instabuild: ability("instabuild", false),
            may_build: ability("mayBuild", true),
            flying_speed: abilities
                .and_then(|a| nbt::float(a, "flySpeed"))
                .unwrap_or(DEFAULT_FLYING_SPEED),
            walking_speed: abilities
                .and_then(|a| nbt::float(a, "walkSpeed"))
                .unwrap_or(DEFAULT_WALKING_SPEED),
        },
        inventory,
        selected_slot: nbt::int(vanilla, "SelectedItemSlot")
            .unwrap_or_default()
            .clamp(0, 8),
        world,
        food_level: nbt::int(vanilla, "foodLevel").unwrap_or(DEFAULT_FOOD_LEVEL),
        food_saturation_level: nbt::float(vanilla, "foodSaturationLevel")
            .unwrap_or(DEFAULT_SATURATION),
        food_exhaustion_level: nbt::float(vanilla, "foodExhaustionLevel").unwrap_or_default(),
        food_tick_timer: nbt::int(vanilla, "foodTickTimer").unwrap_or_default(),
        experience_level: nbt::int(vanilla, "XpLevel").unwrap_or_default(),
        experience_progress: nbt::float(vanilla, "XpP").unwrap_or_default(),
        experience_total: nbt::int(vanilla, "XpTotal").unwrap_or_default(),
        enchantment_seed: nbt::int(vanilla, "XpSeed").unwrap_or_default(),
        score: nbt::int(vanilla, "Score").unwrap_or_default(),
        seen_credits: nbt::boolean(vanilla, "seenCredits").unwrap_or_default(),
        root_vehicle,
        respawn_config,
        ender_pearls,
        ender_items,
    }
}

/// The entity the player rides and the ender pearls they threw, which Steel restores from
/// the player file rather than from chunks. They are deduplicated apart from the world's
/// entities for that reason.
fn carried_entities(vanilla: &NbtCompound) -> (Option<RootVehicleFile>, Vec<EnderPearlFile>) {
    let mut entities = EntityConverter::default();
    let root_vehicle = vanilla.compound("RootVehicle").and_then(|vehicle| {
        let attach = vehicle.int_array("Attach").and_then(nbt::uuid_bytes)?;
        let entity = entities.convert_root(vehicle.compound("Entity")?)?;
        Some(RootVehicleFile { attach, entity })
    });
    let ender_pearls = nbt::compounds(vanilla, "ender_pearls")
        .iter()
        .filter_map(|pearl| {
            let world = nbt::string(pearl, "ender_pearl_dimension")?;
            let mut pearl = pearl.clone();
            pearl.remove("ender_pearl_dimension");
            Some(EnderPearlFile {
                world,
                entity: entities.convert_root(&pearl)?,
            })
        })
        .collect();
    (root_vehicle, ender_pearls)
}

fn vec3(compound: &NbtCompound, key: &str) -> [f64; 3] {
    match nbt::doubles(compound, key).as_deref() {
        Some(&[x, y, z]) => [x, y, z],
        _ => [0.0; 3],
    }
}

struct ItemConverter<'a> {
    player: &'a str,
    report: &'a mut ConversionReport,
}

impl ItemConverter<'_> {
    /// Steel's inventory (vanilla `Inventory` plus `equipment`, §7.4) and ender chest.
    fn player_items(&mut self, vanilla: &NbtCompound) -> (Vec<SlotFile>, Vec<SlotFile>) {
        let mut inventory: Vec<SlotFile> = nbt::compounds(vanilla, "Inventory")
            .iter()
            .filter_map(|item| {
                let slot = item.byte("Slot")?;
                (0..36)
                    .contains(&slot)
                    .then(|| self.slot(slot, item, "Inventory"))
                    .flatten()
            })
            .collect();
        if let Some(equipment) = vanilla.compound("equipment") {
            for (key, slot) in EQUIPMENT_SLOTS {
                if let Some(item) = equipment.compound(key) {
                    inventory.extend(self.slot(slot, item, key));
                }
            }
        }
        let ender_items = nbt::compounds(vanilla, "EnderItems")
            .iter()
            .filter_map(|item| {
                let slot = item.byte("Slot")?;
                (0..27)
                    .contains(&slot)
                    .then(|| self.slot(slot, item, "EnderItems"))
                    .flatten()
            })
            .collect();
        (inventory, ender_items)
    }

    /// Steel's encoding of a vanilla item, or `None` (reported) if Steel would reject it.
    fn slot(&mut self, slot: i8, item: &NbtCompound, location: &str) -> Option<SlotFile> {
        let mut item = item.clone();
        item.remove("Slot");
        match steel_item_nbt(&item) {
            Ok(Some(item_nbt)) => Some(SlotFile { slot, item_nbt }),
            Ok(None) => None,
            Err(reason) => {
                let id = nbt::string(&item, "id").unwrap_or_default();
                self.report.dropped_player_items.push(format!(
                    "{} {location} slot {slot} ({id}): {reason}",
                    self.player
                ));
                None
            }
        }
    }
}

/// Decodes a vanilla item with Steel's `ItemStack` and returns the compound body Steel
/// itself would save. Air is `None`; Steel never stores it.
fn steel_item_nbt(item: &NbtCompound) -> Result<Option<Vec<u8>>, String> {
    let bytes = nbt::compound_body(item);
    let borrowed =
        borrow::read_compound(&mut Cursor::new(&bytes)).map_err(|e| format!("invalid NBT: {e}"))?;
    let view = borrow::NbtCompound::from(&borrowed);
    let stack = ItemStack::from_borrowed_compound(&view)
        .ok_or("unknown item, count outside 1..=99, or a component Steel cannot decode")?;
    if stack.is_empty() {
        return Ok(None);
    }
    stack
        .validate_persistent_encoding()
        .map_err(|e| e.to_string())?;
    let NbtTag::Compound(compound) = stack.to_nbt_tag_ref() else {
        return Err("item did not encode to a compound".to_owned());
    };
    Ok(Some(nbt::compound_body(&compound)))
}

/// `[magic][version: u16 LE][zstd(level 3, wincode(payload))]` (§7.1).
fn write_envelope(
    path: &Path,
    magic: [u8; 4],
    storage_version: u16,
    serialized: wincode::WriteResult<Vec<u8>>,
) -> Result<(), ConvertError> {
    let payload =
        serialized.map_err(|e| ConvertError::invalid(path, format!("failed to encode: {e}")))?;
    let compressed = zstd::encode_all(&payload[..], 3).map_err(|e| ConvertError::io(path, e))?;
    let mut bytes = Vec::with_capacity(6 + compressed.len());
    bytes.extend_from_slice(&magic);
    bytes.extend_from_slice(&storage_version.to_le_bytes());
    bytes.extend_from_slice(&compressed);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| ConvertError::io(parent, e))?;
    }
    fs::write(path, bytes).map_err(|e| ConvertError::io(path, e))
}
