//! `data/*.toml` saved data (`WORLD_STORAGE.md` §6).

use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::Path,
};

use serde::Serialize;
use simdnbt::owned::NbtTag;
use steel_utils::Identifier;

use crate::{ConvertError, nbt, report::ConversionReport};

/// `steel level = vanilla level + 97` (§6.1).
const TICKET_LEVEL_OFFSET: i32 = 97;
/// The ticket types Steel persists (§6.1).
const PERSISTENT_TICKET_TYPES: [&str; 2] = ["minecraft:forced", "minecraft:portal"];
/// Vanilla `ObjectiveCriteria` registered with `readOnly = true`. Every other criterion,
/// including every stat criterion, is writable.
const READ_ONLY_CRITERIA: [&str; 6] = ["health", "food", "air", "armor", "xp", "level"];

#[derive(Serialize)]
struct ChunkTickets {
    tickets: Vec<ChunkTicket>,
}

#[derive(Serialize)]
struct ChunkTicket {
    #[serde(rename = "type")]
    ticket_type: String,
    chunk_x: i32,
    chunk_z: i32,
    level: u8,
    ticks_left: i64,
}

/// `dimensions/…/data/minecraft/chunk_tickets.dat` → `data/chunk_tickets.toml`.
pub fn chunk_tickets(
    dimension_dir: &Path,
    world_dir: &Path,
    report: &mut ConversionReport,
) -> Result<(), ConvertError> {
    let path = dimension_dir.join("data/minecraft/chunk_tickets.dat");
    let Some(data) = nbt::read_saved_data(&path)? else {
        return Ok(());
    };
    let mut tickets = Vec::new();
    for ticket in nbt::compounds(&data, "tickets") {
        let ticket_type = nbt::string(ticket, "type").unwrap_or_default();
        if !PERSISTENT_TICKET_TYPES.contains(&ticket_type.as_str()) {
            continue;
        }
        let (Some(&[chunk_x, chunk_z]), Some(level)) =
            (ticket.int_array("chunk_pos"), nbt::int(ticket, "level"))
        else {
            report
                .warnings
                .push(format!("{}: malformed ticket dropped", path.display()));
            continue;
        };
        let Ok(level) = u8::try_from(level + TICKET_LEVEL_OFFSET) else {
            report.warnings.push(format!(
                "{}: ticket level {level} out of range; dropped",
                path.display()
            ));
            continue;
        };
        tickets.push(ChunkTicket {
            ticket_type,
            chunk_x,
            chunk_z,
            level,
            ticks_left: nbt::integer(ticket, "ticks_left").unwrap_or_default(),
        });
    }
    write_toml(
        &world_dir.join("data/chunk_tickets.toml"),
        &ChunkTickets { tickets },
    )
}

#[derive(Serialize, Default)]
struct Scoreboard {
    objectives: BTreeMap<String, Objective>,
    scores: BTreeMap<String, BTreeMap<String, Score>>,
    teams: BTreeSet<String>,
    holder_teams: BTreeMap<String, String>,
}

#[derive(Serialize)]
struct Objective {
    read_only: bool,
}

#[derive(Serialize)]
struct Score {
    value: i32,
    locked: bool,
}

/// `data/minecraft/scoreboard.dat` → `scoreboard.toml` in the domain default world (§6.2).
///
/// Steel refuses to start on a score for a missing objective or a membership in a missing
/// team, so such entries are dropped with a warning.
pub fn scoreboard(
    input: &Path,
    default_world_dir: &Path,
    report: &mut ConversionReport,
) -> Result<(), ConvertError> {
    let path = input.join("data/minecraft/scoreboard.dat");
    let Some(data) = nbt::read_saved_data(&path)? else {
        return Ok(());
    };
    let mut scoreboard = Scoreboard::default();
    for objective in nbt::compounds(&data, "Objectives") {
        let Some(name) = nbt::string(objective, "Name") else {
            continue;
        };
        let criteria = nbt::string(objective, "CriteriaName").unwrap_or_default();
        scoreboard.objectives.insert(
            name,
            Objective {
                read_only: READ_ONLY_CRITERIA.contains(&criteria.as_str()),
            },
        );
    }
    for team in nbt::compounds(&data, "Teams") {
        let Some(name) = nbt::string(team, "Name") else {
            continue;
        };
        for player in team
            .list("Players")
            .and_then(|list| list.strings())
            .unwrap_or_default()
        {
            scoreboard
                .holder_teams
                .insert(player.to_str().into_owned(), name.clone());
        }
        scoreboard.teams.insert(name);
    }
    for score in nbt::compounds(&data, "PlayerScores") {
        let (Some(holder), Some(objective)) =
            (nbt::string(score, "Name"), nbt::string(score, "Objective"))
        else {
            continue;
        };
        if !scoreboard.objectives.contains_key(&objective) {
            report.warnings.push(format!(
                "score of {holder} for missing objective {objective} dropped"
            ));
            continue;
        }
        scoreboard.scores.entry(holder).or_default().insert(
            objective,
            Score {
                value: nbt::int(score, "Score").unwrap_or_default(),
                // Vanilla's `Locked` defaults to false; Steel's to true, so always write it.
                locked: nbt::boolean(score, "Locked").unwrap_or(false),
            },
        );
    }
    if !nbt::compounds(&data, "Teams").is_empty() || data.contains("DisplaySlots") {
        report
            .warnings
            .push("scoreboard display slots, criteria, display names and team settings are not stored by Steel".to_owned());
    }
    write_toml(&default_world_dir.join("data/scoreboard.toml"), &scoreboard)
}

#[derive(Serialize)]
struct CommandStorage {
    entries: BTreeMap<String, Vec<u8>>,
}

/// `data/<namespace>/command_storage.dat` → `command_storage.toml` in the domain default
/// world (§6.3).
pub fn command_storage(input: &Path, default_world_dir: &Path) -> Result<(), ConvertError> {
    let data_dir = input.join("data");
    let namespaces = match fs::read_dir(&data_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(ConvertError::io(&data_dir, error)),
    };
    let mut entries = BTreeMap::new();
    for namespace in namespaces {
        let namespace = namespace.map_err(|e| ConvertError::io(&data_dir, e))?;
        // Only folders are namespaces; files such as Finder's `.DS_Store` can sit beside them.
        if !namespace.path().is_dir() {
            continue;
        }
        let Some(namespace_name) = namespace.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let path = namespace.path().join("command_storage.dat");
        let Some(data) = nbt::read_saved_data(&path)? else {
            continue;
        };
        let Some(contents) = data.compound("contents") else {
            continue;
        };
        for (key, value) in contents.iter() {
            let Some(value) = value.compound().filter(|value| !value.is_empty()) else {
                continue;
            };
            let id = format!("{namespace_name}:{}", key.to_str());
            if id.parse::<Identifier>().is_err() {
                return Err(ConvertError::invalid(
                    &path,
                    format!("storage id {id:?} is invalid"),
                ));
            }
            entries.insert(id, nbt::compound_body(value));
        }
    }
    if entries.is_empty() {
        return Ok(());
    }
    write_toml(
        &default_world_dir.join("data/command_storage.toml"),
        &CommandStorage { entries },
    )
}

#[derive(Serialize)]
struct RandomSequences {
    salt: i32,
    include_world_seed: bool,
    include_sequence_id: bool,
    sequences: BTreeMap<String, RandomSequence>,
}

#[derive(Serialize)]
struct RandomSequence {
    source: [i64; 2],
}

/// `data/minecraft/random_sequences.dat` → `random_sequences.toml` in the domain default world
/// (§6.4). Defaults are vanilla `RandomSequences.CODEC`'s.
pub fn random_sequences(input: &Path, default_world_dir: &Path) -> Result<(), ConvertError> {
    let path = input.join("data/minecraft/random_sequences.dat");
    let Some(data) = nbt::read_saved_data(&path)? else {
        return Ok(());
    };
    let mut sequences = BTreeMap::new();
    if let Some(stored) = data.compound("sequences") {
        for (key, sequence) in stored.iter() {
            let source = sequence
                .compound()
                .and_then(|sequence| match sequence.get("source")? {
                    NbtTag::LongArray(longs) => Some(longs.clone()),
                    NbtTag::List(list) => list.longs(),
                    _ => None,
                });
            let Some(&[lo, hi]) = source.as_deref() else {
                return Err(ConvertError::invalid(
                    &path,
                    format!("sequence {} has no 2-long source", key.to_str()),
                ));
            };
            sequences.insert(
                key.to_str().into_owned(),
                RandomSequence { source: [lo, hi] },
            );
        }
    }
    write_toml(
        &default_world_dir.join("data/random_sequences.toml"),
        &RandomSequences {
            salt: nbt::int(&data, "salt").unwrap_or_default(),
            include_world_seed: nbt::boolean(&data, "include_world_seed").unwrap_or(true),
            include_sequence_id: nbt::boolean(&data, "include_sequence_id").unwrap_or(true),
            sequences,
        },
    )
}

/// Vanilla saved data with no Steel storage (§8.1), relative to the save root, that this
/// save actually holds.
pub fn unsupported_files(input: &Path, dimension_dirs: &[(&str, &Path)]) -> Vec<String> {
    let mut found = Vec::new();
    let server_files = [
        "data/minecraft/custom_boss_events.dat",
        "data/minecraft/scheduled_events.dat",
        "data/minecraft/stopwatches.dat",
        "data/minecraft/wandering_trader.dat",
        "data/minecraft/maps",
    ];
    for file in server_files {
        if input.join(file).exists() && !is_empty_saved_data(&input.join(file)) {
            found.push(file.to_owned());
        }
    }
    for (name, dir) in dimension_dirs {
        let raids = dir.join("data/minecraft/raids.dat");
        let has_raids = nbt::read_saved_data(&raids)
            .ok()
            .flatten()
            .is_some_and(|data| !nbt::compounds(&data, "raids").is_empty());
        if has_raids {
            found.push(format!(
                "dimensions/minecraft/{name}/data/minecraft/raids.dat (active raids)"
            ));
        }
        if dir.join("data/minecraft/ender_dragon_fight.dat").exists() {
            found.push(format!(
                "dimensions/minecraft/{name}/data/minecraft/ender_dragon_fight.dat"
            ));
        }
    }
    found
}

/// A saved-data file whose `data` has nothing in it, so nothing is lost by dropping it.
fn is_empty_saved_data(path: &Path) -> bool {
    if path.is_dir() {
        return fs::read_dir(path).is_ok_and(|mut entries| entries.next().is_none());
    }
    nbt::read_saved_data(path)
        .ok()
        .flatten()
        .is_some_and(|data| data.values().all(is_empty_value))
}

fn is_empty_value(value: &NbtTag) -> bool {
    match value {
        NbtTag::Compound(compound) => compound.is_empty(),
        NbtTag::List(list) => list.as_nbt_tags().is_empty(),
        _ => false,
    }
}

fn write_toml(path: &Path, value: &impl Serialize) -> Result<(), ConvertError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| ConvertError::io(parent, e))?;
    }
    let text =
        toml::to_string_pretty(value).map_err(|e| ConvertError::invalid(path, e.to_string()))?;
    fs::write(path, text).map_err(|e| ConvertError::io(path, e))
}

#[cfg(test)]
mod tests {
    use std::{env, process};

    use super::*;

    #[test]
    fn command_storage_skips_files_beside_namespace_folders() {
        let input = env::temp_dir().join(format!("steel-converter-data-{}", process::id()));
        fs::create_dir_all(input.join("data/minecraft")).expect("create data dir");
        // Finder writes `.DS_Store` into every folder it has shown.
        fs::write(input.join("data/.DS_Store"), [0u8; 8]).expect("write .DS_Store");

        let result = command_storage(&input, &input.join("out"));
        let _ = fs::remove_dir_all(&input);
        result.expect("a file in data/ is not a namespace");
    }
}
