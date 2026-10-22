//! Reads the game's combat log (`<flavour>/Logs/WoWCombatLog*.txt`) for the creatures that
//! died during each successful boss encounter. Inside dungeons and raids the addon cannot
//! read hostile GUIDs (secret values), the log file still has them; the addon turns logging
//! on there (`/fc combatlog`).
//!
//! Only encounter IDs, creature IDs, their names and times leave the computer: no player
//! names, no raw lines. The kills travel as the snapshot's top-level `combatLog`, next to
//! the addon's catalogs and apart from them (addon SCHEMA.md section 15); the worker
//! matches each kill to the catalog of its client version.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// An encounter still open this long (log time) before the newest line never ended, e.g.
/// after a crash or disconnect; it is dropped instead of holding the read position.
const STALE_ENCOUNTER_SECONDS: f64 = 3600.0;

/// A log file untouched for this long is finished: WoW starts a new file per session.
const ACTIVE_FILE_SECONDS: u64 = 600;

/// COMBATLOG_OBJECT_REACTION_HOSTILE: only hostile creatures are boss candidates.
const REACTION_HOSTILE: u64 = 0x40;

#[derive(Debug, Clone, PartialEq)]
pub struct Death {
    pub npc_id: i64,
    pub name: Option<String>,
    pub seconds_before_end: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Kill {
    pub encounter_id: i64,
    pub name: Option<String>,
    pub difficulty_id: Option<i64>,
    pub instance_id: Option<i64>,
    /// Unix seconds of ENCOUNTER_END; exact when the log line carries its UTC offset,
    /// otherwise the log's local clock read as UTC.
    pub at: i64,
    /// BUILD_VERSION and PROJECT_ID of the last COMBAT_LOG_VERSION line, to pick the
    /// catalog of the same client version.
    pub version: Option<String>,
    pub project_id: Option<i64>,
    pub deaths: Vec<Death>,
}

struct OpenEncounter {
    encounter_id: i64,
    name: Option<String>,
    difficulty_id: Option<i64>,
    instance_id: Option<i64>,
    started_at: f64,
    /// Byte offset of the ENCOUNTER_START line, where reading resumes while it is open.
    line_start: u64,
    /// Last death per NPC ID, in log time.
    deaths: Vec<(i64, Option<String>, f64)>,
}

#[derive(Default)]
struct LogParser {
    version: Option<String>,
    project_id: Option<i64>,
    open: Option<OpenEncounter>,
    kills: Vec<Kill>,
    last_time: f64,
}

impl LogParser {
    fn line(&mut self, line: &str, line_start: u64, fallback_year: i64) {
        let Some((stamp, body)) = line.split_once("  ") else {
            return;
        };
        let event = body.split(',').next().unwrap_or("");
        if !matches!(
            event,
            "COMBAT_LOG_VERSION" | "ENCOUNTER_START" | "ENCOUNTER_END" | "UNIT_DIED"
        ) {
            return;
        }
        let fields = split_fields(body.trim_end());
        if event == "COMBAT_LOG_VERSION" {
            self.header(&fields);
            return;
        }
        let Some(time) = parse_timestamp(stamp, fallback_year) else {
            return;
        };
        self.last_time = time;
        match event {
            "ENCOUNTER_START" => {
                let Some(encounter_id) = int_field(&fields, 1) else {
                    return;
                };
                self.open = Some(OpenEncounter {
                    encounter_id,
                    name: text_field(&fields, 2),
                    difficulty_id: int_field(&fields, 3),
                    instance_id: int_field(&fields, 5),
                    started_at: time,
                    line_start,
                    deaths: Vec::new(),
                });
            }
            "UNIT_DIED" => {
                let Some(open) = self.open.as_mut() else {
                    return;
                };
                let Some(npc_id) = fields.get(5).and_then(|guid| npc_id_of(guid)) else {
                    return;
                };
                let hostile = fields
                    .get(7)
                    .and_then(|flags| u64::from_str_radix(flags.trim_start_matches("0x"), 16).ok())
                    .is_none_or(|flags| flags & REACTION_HOSTILE != 0);
                if !hostile {
                    return;
                }
                let name = text_field(&fields, 6);
                open.deaths.retain(|(id, _, _)| *id != npc_id);
                open.deaths.push((npc_id, name, time));
            }
            "ENCOUNTER_END" => {
                let Some(open) = self.open.take() else {
                    return;
                };
                if int_field(&fields, 1) != Some(open.encounter_id)
                    || int_field(&fields, 5) != Some(1)
                {
                    return;
                }
                self.kills.push(Kill {
                    encounter_id: open.encounter_id,
                    name: open.name,
                    difficulty_id: open.difficulty_id,
                    instance_id: open.instance_id,
                    at: time.floor() as i64,
                    version: self.version.clone(),
                    project_id: self.project_id,
                    deaths: open
                        .deaths
                        .into_iter()
                        .map(|(npc_id, name, died)| Death {
                            npc_id,
                            name,
                            seconds_before_end: ((time - died) * 1000.0).round() / 1000.0,
                        })
                        .collect(),
                });
            }
            _ => {}
        }
    }

    /// `COMBAT_LOG_VERSION,22,ADVANCED_LOG_ENABLED,0,BUILD_VERSION,1.15.7,PROJECT_ID,2`
    fn header(&mut self, fields: &[String]) {
        let value = |key: &str| {
            fields
                .iter()
                .position(|field| field == key)
                .and_then(|index| fields.get(index + 1))
                .cloned()
        };
        self.version = value("BUILD_VERSION");
        self.project_id = value("PROJECT_ID").and_then(|id| id.parse().ok());
    }
}

/// Splits a log line body at commas outside double quotes; quotes are removed.
fn split_fields(body: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    for character in body.chars() {
        match character {
            '"' => quoted = !quoted,
            ',' if !quoted => fields.push(std::mem::take(&mut field)),
            _ => field.push(character),
        }
    }
    fields.push(field);
    fields
}

fn int_field(fields: &[String], index: usize) -> Option<i64> {
    fields.get(index)?.trim().parse().ok()
}

fn text_field(fields: &[String], index: usize) -> Option<String> {
    fields
        .get(index)
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty() && text != "nil")
}

/// `Creature-0-<server>-<instance>-<zone>-<npcID>-<spawn>` (or `Vehicle-…`).
fn npc_id_of(guid: &str) -> Option<i64> {
    let mut parts = guid.split('-');
    let kind = parts.next()?;
    if kind != "Creature" && kind != "Vehicle" {
        return None;
    }
    parts.nth(4)?.parse().ok().filter(|id| *id > 0)
}

/// `10/22/2026 20:15:03.1234-4` (current clients) or `10/22 20:15:03.123` (older ones,
/// without year and offset) as Unix seconds.
fn parse_timestamp(stamp: &str, fallback_year: i64) -> Option<f64> {
    let (date, clock) = stamp.trim().split_once(' ')?;
    let mut date_parts = date.split('/');
    let month: i64 = date_parts.next()?.parse().ok()?;
    let day: i64 = date_parts.next()?.parse().ok()?;
    let year: i64 = match date_parts.next() {
        Some(year) => year.parse().ok()?,
        None => fallback_year,
    };
    // The UTC offset follows the fraction: "20:15:03.1234-4" or "…+2".
    let (clock, offset_hours) = match clock.rfind(['+', '-']) {
        Some(index) if index > 0 => (&clock[..index], clock[index..].parse::<f64>().ok()?),
        _ => (clock, 0.0),
    };
    let mut clock_parts = clock.split(':');
    let hours: f64 = clock_parts.next()?.parse().ok()?;
    let minutes: f64 = clock_parts.next()?.parse().ok()?;
    let seconds: f64 = clock_parts.next()?.parse().ok()?;
    let days = days_from_civil(year, month, day) as f64;
    Some(days * 86400.0 + hours * 3600.0 + minutes * 60.0 + seconds - offset_hours * 3600.0)
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn year_of(unix_seconds: u64) -> i64 {
    let days = (unix_seconds / 86400) as i64 + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    year_of_era + era * 400 + i64::from(month <= 2)
}

/// Kills in `path` from byte `offset` on, and the offset to resume from: the end of the
/// last complete line, or the start of an encounter that is still open in an active file.
fn read_file(
    path: &Path,
    offset: u64,
    active: bool,
    fallback_year: i64,
) -> std::io::Result<(Vec<Kill>, u64)> {
    let mut file = File::open(path)?;
    let mut parser = LogParser::default();
    // The version header is the first line; reading resumes further down.
    if offset > 0 {
        let mut first = String::new();
        BufReader::new(&mut file).read_line(&mut first)?;
        parser.line(&first, 0, fallback_year);
        file.seek(SeekFrom::Start(offset))?;
    }
    let mut reader = BufReader::new(file);
    let mut position = offset;
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        let read = reader.read_until(b'\n', &mut buffer)?;
        if read == 0 || buffer.last() != Some(&b'\n') {
            break;
        }
        let line = String::from_utf8_lossy(&buffer);
        parser.line(&line, position, fallback_year);
        position += read as u64;
    }
    let resume = match &parser.open {
        Some(open) if active && parser.last_time - open.started_at < STALE_ENCOUNTER_SECONDS => {
            open.line_start
        }
        _ => position,
    };
    Ok((parser.kills, resume))
}

fn log_files(flavour_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(flavour_dir.join("Logs")) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        name.starts_with("WoWCombatLog")
                            && name.to_ascii_lowercase().ends_with(".txt")
                    })
        })
        .collect();
    files.sort();
    files
}

/// Kills logged since the given read positions (keyed by file path), and the positions
/// after them. Unreadable files are skipped and keep their position.
pub fn collect(
    flavour_dir: &Path,
    offsets: &HashMap<String, u64>,
) -> (Vec<Kill>, HashMap<String, u64>) {
    let now = SystemTime::now();
    let mut kills = Vec::new();
    let mut positions = HashMap::new();
    for path in log_files(flavour_dir) {
        let key = path.to_string_lossy().into_owned();
        let Ok(metadata) = fs::metadata(&path) else {
            continue;
        };
        let length = metadata.len();
        let mut offset = offsets.get(&key).copied().unwrap_or(0);
        // A shorter file was replaced or truncated: read it from the start.
        if offset > length {
            offset = 0;
        }
        if offset == length {
            positions.insert(key, offset);
            continue;
        }
        let modified = metadata.modified().unwrap_or(now);
        let age = now
            .duration_since(modified)
            .map(|age| age.as_secs())
            .unwrap_or(0);
        let year = year_of(
            modified
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|time| time.as_secs())
                .unwrap_or(0),
        );
        match read_file(&path, offset, age < ACTIVE_FILE_SECONDS, year) {
            Ok((found, resume)) => {
                kills.extend(found);
                positions.insert(key, resume);
            }
            Err(_) => {
                positions.insert(key, offset);
            }
        }
    }
    (kills, positions)
}

/// The snapshot's top-level `combatLog` for these kills, or None without kills.
pub fn upload(kills: &[Kill]) -> Option<Value> {
    if kills.is_empty() {
        return None;
    }
    let kills: Vec<Value> = kills
        .iter()
        .map(|kill| {
            let mut entry = json!({
                "encounterID": kill.encounter_id,
                "at": kill.at,
                "deaths": kill.deaths.iter().map(|death| {
                    let mut value = json!({ "npcID": death.npc_id, "secondsBeforeEnd": death.seconds_before_end });
                    if let Some(name) = &death.name {
                        value["name"] = json!(name);
                    }
                    value
                }).collect::<Vec<_>>(),
            });
            for (key, value) in [
                ("version", kill.version.as_ref().map(|version| json!(version))),
                ("projectID", kill.project_id.map(|id| json!(id))),
                ("name", kill.name.as_ref().map(|name| json!(name))),
                ("instanceID", kill.instance_id.map(|id| json!(id))),
                ("difficultyID", kill.difficulty_id.map(|id| json!(id))),
            ] {
                if let Some(value) = value {
                    entry[key] = value;
                }
            }
            entry
        })
        .collect();
    Some(json!({ "kills": kills }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> String {
        fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/combatlog.txt"
        ))
        .unwrap()
    }

    fn parse(text: &str) -> Vec<Kill> {
        let mut parser = LogParser::default();
        let mut position = 0;
        for line in text.split_inclusive('\n') {
            parser.line(line, position, 2026);
            position += line.len() as u64;
        }
        parser.kills
    }

    fn temp_flavour(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("foreverdb-combatlog-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("Logs")).unwrap();
        dir
    }

    #[test]
    fn finds_the_deaths_of_a_kill() {
        let kills = parse(&fixture());
        assert_eq!(kills.len(), 2, "the wipe is no kill");
        let kresh = &kills[0];
        assert_eq!(kresh.encounter_id, 586);
        assert_eq!(kresh.name.as_deref(), Some("Kresh"));
        assert_eq!(kresh.instance_id, Some(43));
        assert_eq!(kresh.difficulty_id, Some(1));
        assert_eq!(kresh.version.as_deref(), Some("1.60.1"));
        assert_eq!(kresh.project_id, Some(2));
        let ids: Vec<i64> = kresh.deaths.iter().map(|death| death.npc_id).collect();
        assert_eq!(
            ids,
            vec![3840, 3653],
            "the add, then the boss; players and friendly units are left out"
        );
        assert_eq!(kresh.deaths[1].seconds_before_end, 0.012);
        assert_eq!(kresh.deaths[1].name.as_deref(), Some("Kresh"));
        assert_eq!(kresh.deaths[0].seconds_before_end, 20.5);
    }

    #[test]
    fn keeps_every_member_of_a_council() {
        let kills = parse(&fixture());
        let council = &kills[1];
        assert_eq!(council.encounter_id, 2711);
        assert_eq!(council.name.as_deref(), Some("Hans and Greta"));
        let ids: Vec<i64> = council.deaths.iter().map(|death| death.npc_id).collect();
        assert_eq!(ids, vec![240001, 240002]);
        assert_eq!(
            council.deaths[0].name.as_deref(),
            Some("Hans, the Woodcutter"),
            "commas inside quotes stay"
        );
    }

    #[test]
    fn reads_timestamps_with_and_without_year() {
        let with_offset = parse_timestamp("10/22/2026 20:15:03.500-4", 2000).unwrap();
        let utc = parse_timestamp("10/23/2026 00:15:03.500", 2000).unwrap();
        assert_eq!(with_offset, utc);
        assert_eq!(parse_timestamp("1/1 00:00:00.000", 1970), Some(0.0));
        assert_eq!(year_of(1_792_689_851), 2026);
        assert_eq!(year_of(0), 1970);
    }

    #[test]
    fn resumes_where_it_stopped_and_waits_for_an_open_encounter() {
        let dir = temp_flavour("resume");
        let path = dir.join("Logs").join("WoWCombatLog-102226_201500.txt");
        let text = fixture();
        // Cut the file inside the council fight: the open encounter is read again later.
        let cut = text
            .find("UNIT_DIED,0000000000000000,nil,0x80000000,0x80000000,Creature-0-1-2-3-240002")
            .unwrap();
        let line_start = text[..cut].rfind('\n').unwrap() + 1;
        fs::write(&path, &text[..line_start]).unwrap();
        let key = path.to_string_lossy().into_owned();

        let (kills, positions) = collect(&dir, &HashMap::new());
        assert_eq!(kills.len(), 1);
        let resume = positions[&key];
        assert!(
            resume < line_start as u64,
            "resumes at the open ENCOUNTER_START"
        );

        fs::write(&path, &text).unwrap();
        let (kills, positions) = collect(&dir, &positions);
        assert_eq!(kills.len(), 1);
        assert_eq!(kills[0].encounter_id, 2711);
        assert_eq!(
            kills[0].version.as_deref(),
            Some("1.60.1"),
            "header from the first line"
        );
        assert_eq!(positions[&key], text.len() as u64);

        let (kills, _) = collect(&dir, &positions);
        assert!(kills.is_empty(), "nothing is read twice");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn builds_the_upload_field() {
        assert_eq!(upload(&[]), None);
        let upload = upload(&parse(&fixture())).unwrap();
        let kresh = &upload["kills"][0];
        assert_eq!(kresh["encounterID"], 586);
        assert_eq!(kresh["name"], "Kresh");
        assert_eq!(kresh["version"], "1.60.1");
        assert_eq!(kresh["projectID"], 2);
        assert_eq!(kresh["instanceID"], 43);
        assert_eq!(kresh["difficultyID"], 1);
        assert_eq!(kresh["deaths"][1]["npcID"], 3653);
        assert_eq!(kresh["deaths"][1]["name"], "Kresh");
        assert_eq!(kresh["deaths"][1]["secondsBeforeEnd"], 0.012);
        assert_eq!(upload["kills"].as_array().unwrap().len(), 2);
    }
}
