//! Player statistics and advancements (`WORLD_STORAGE.md` §7.5, §7.6).

use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use chrono::DateTime;
use serde::Serialize;

use crate::{ConvertError, version};

/// Vanilla `PlayerAdvancements` criterion timestamp format.
const ADVANCEMENT_DATE_FORMAT: &str = "%Y-%m-%d %H:%M:%S %z";

#[derive(Serialize)]
struct StatsFile {
    stats: BTreeMap<String, BTreeMap<String, i32>>,
}

/// `players/stats/<uuid>.json` → `stats/<uuid>.toml`: the same keys without `DataVersion`.
pub fn convert_stats(input: &Path, output: &Path) -> Result<(), ConvertError> {
    let json = read_json(input)?;
    check_json_version(input, &json)?;
    let mut stats = BTreeMap::new();
    if let Some(types) = json.get("stats").and_then(serde_json::Value::as_object) {
        for (stat_type, values) in types {
            let Some(values) = values.as_object() else {
                return Err(ConvertError::invalid(
                    input,
                    format!("stats.{stat_type} is not an object"),
                ));
            };
            let converted: BTreeMap<String, i32> = values
                .iter()
                .filter_map(|(stat, value)| {
                    Some((stat.clone(), i32::try_from(value.as_i64()?).ok()?))
                })
                .collect();
            stats.insert(stat_type.clone(), converted);
        }
    }
    let text = toml::to_string_pretty(&StatsFile { stats })
        .map_err(|e| ConvertError::invalid(output, e.to_string()))?;
    write(output, text.as_bytes())
}

#[derive(Serialize)]
struct AdvancementsFile {
    entries: BTreeMap<String, BTreeMap<String, SystemTime>>,
}

/// `players/advancements/<uuid>.json` → `advancements/<uuid>.json`: completed criteria only,
/// as serde `SystemTime`, wrapped in `entries`.
pub fn convert_advancements(input: &Path, output: &Path) -> Result<(), ConvertError> {
    let json = read_json(input)?;
    check_json_version(input, &json)?;
    let Some(advancements) = json.as_object() else {
        return Err(ConvertError::invalid(input, "not a JSON object"));
    };
    let mut entries = BTreeMap::new();
    for (id, progress) in advancements {
        if id == "DataVersion" {
            continue;
        }
        let Some(criteria) = progress
            .get("criteria")
            .and_then(serde_json::Value::as_object)
        else {
            continue;
        };
        let mut done = BTreeMap::new();
        for (criterion, obtained) in criteria {
            let Some(obtained) = obtained.as_str() else {
                return Err(ConvertError::invalid(
                    input,
                    format!("{id} {criterion} has no timestamp"),
                ));
            };
            done.insert(
                criterion.clone(),
                parse_timestamp(obtained).map_err(|m| ConvertError::invalid(input, m))?,
            );
        }
        if !done.is_empty() {
            entries.insert(id.clone(), done);
        }
    }
    let text = serde_json::to_string_pretty(&AdvancementsFile { entries })
        .map_err(|e| ConvertError::invalid(output, e.to_string()))?;
    write(output, text.as_bytes())
}

fn parse_timestamp(value: &str) -> Result<SystemTime, String> {
    let parsed = DateTime::parse_from_str(value, ADVANCEMENT_DATE_FORMAT)
        .map_err(|e| format!("invalid advancement timestamp {value:?}: {e}"))?;
    let seconds = u64::try_from(parsed.timestamp())
        .map_err(|_| format!("advancement timestamp {value:?} is before 1970"))?;
    Ok(UNIX_EPOCH + Duration::from_secs(seconds))
}

fn read_json(path: &Path) -> Result<serde_json::Value, ConvertError> {
    let text = fs::read_to_string(path).map_err(|e| ConvertError::io(path, e))?;
    serde_json::from_str(&text)
        .map_err(|e| ConvertError::invalid(path, format!("invalid JSON: {e}")))
}

fn check_json_version(path: &Path, json: &serde_json::Value) -> Result<(), ConvertError> {
    let found = json
        .get("DataVersion")
        .and_then(serde_json::Value::as_i64)
        .and_then(|version| i32::try_from(version).ok());
    version::check(path, found)
}

fn write(path: &Path, bytes: &[u8]) -> Result<(), ConvertError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| ConvertError::io(parent, e))?;
    }
    fs::write(path, bytes).map_err(|e| ConvertError::io(path, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advancement_timestamps_keep_their_utc_offset() {
        let time = parse_timestamp("2026-11-18 01:58:42 +0800").expect("vanilla timestamp");
        let seconds = time
            .duration_since(UNIX_EPOCH)
            .expect("after epoch")
            .as_secs();
        // 2026-11-17 17:58:42 UTC.
        assert_eq!(seconds, 1_794_938_322);
    }
}
