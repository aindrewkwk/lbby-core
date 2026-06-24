use flate2::read::GzDecoder;
use serde::Serialize;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};


use crate::config;

// ── Types returned to the frontend ──────────────────────────────────────────

#[derive(Serialize, Clone, Debug)]
pub struct InventorySlot {
    pub slot: i32,
    pub id: String,
    pub count: i32,
    pub display_name: Option<String>,
}

#[derive(Serialize, Clone, Debug)]
pub struct ActiveEffect {
    pub id: String,
    pub amplifier: i32,
    pub duration: i32, // ticks remaining (-1 = infinite)
    pub ambient: bool,
}

#[derive(Serialize, Clone, Debug)]
pub struct PlayerData {
    pub name: String,
    pub uuid: String,
    pub health: f64,
    pub max_health: f64,
    pub food_level: i64,
    pub xp_level: i64,
    pub xp_total: i64,
    pub gamemode: String,
    pub position: [f64; 3],
    pub dimension: String,
    pub inventory: Vec<InventorySlot>,
    pub ender_chest: Vec<InventorySlot>,
    pub active_effects: Vec<ActiveEffect>,
}

// ── NBT helpers ─────────────────────────────────────────────────────────────

fn nbt_to_json(val: &fastnbt::Value) -> serde_json::Value {
    match val {
        fastnbt::Value::Byte(v) => serde_json::Value::Number((*v as i64).into()),
        fastnbt::Value::Short(v) => serde_json::Value::Number((*v as i64).into()),
        fastnbt::Value::Int(v) => serde_json::Value::Number((*v as i64).into()),
        fastnbt::Value::Long(v) => serde_json::Value::Number((*v).into()),
        fastnbt::Value::Float(v) => {
            serde_json::Number::from_f64(*v as f64)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null)
        }
        fastnbt::Value::Double(v) => serde_json::Number::from_f64(*v)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        fastnbt::Value::String(s) => serde_json::Value::String(s.clone()),
        fastnbt::Value::Compound(map) => {
            let obj: serde_json::Map<String, serde_json::Value> =
                map.iter().map(|(k, v)| (k.clone(), nbt_to_json(v))).collect();
            serde_json::Value::Object(obj)
        }
        fastnbt::Value::List(items) => {
            serde_json::Value::Array(items.iter().map(nbt_to_json).collect())
        }
        fastnbt::Value::ByteArray(a) => {
            serde_json::Value::Array(a.iter().map(|b| serde_json::Value::Number((*b as i64).into())).collect())
        }
        fastnbt::Value::IntArray(a) => {
            serde_json::Value::Array(a.iter().map(|b| serde_json::Value::Number((*b as i64).into())).collect())
        }
        fastnbt::Value::LongArray(a) => {
            serde_json::Value::Array(a.iter().map(|b| serde_json::Value::Number((*b).into())).collect())
        }
    }
}

fn nbt_get<'a>(compound: &'a HashMap<String, fastnbt::Value>, key: &str) -> Option<&'a fastnbt::Value> {
    compound.get(key)
}

fn nbt_str(compound: &HashMap<String, fastnbt::Value>, key: &str) -> Option<String> {
    match nbt_get(compound, key)? {
        fastnbt::Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

fn nbt_i32(compound: &HashMap<String, fastnbt::Value>, key: &str) -> Option<i32> {
    match nbt_get(compound, key)? {
        fastnbt::Value::Int(v) => Some(*v),
        fastnbt::Value::Short(v) => Some(*v as i32),
        fastnbt::Value::Byte(v) => Some(*v as i32),
        _ => None,
    }
}

fn nbt_i64(compound: &HashMap<String, fastnbt::Value>, key: &str) -> Option<i64> {
    match nbt_get(compound, key)? {
        fastnbt::Value::Long(v) => Some(*v),
        fastnbt::Value::Int(v) => Some(*v as i64),
        fastnbt::Value::Short(v) => Some(*v as i64),
        fastnbt::Value::Byte(v) => Some(*v as i64),
        _ => None,
    }
}

fn nbt_f32(compound: &HashMap<String, fastnbt::Value>, key: &str) -> Option<f32> {
    match nbt_get(compound, key)? {
        fastnbt::Value::Float(v) => Some(*v),
        _ => None,
    }
}

fn nbt_f64(compound: &HashMap<String, fastnbt::Value>, key: &str) -> Option<f64> {
    match nbt_get(compound, key)? {
        fastnbt::Value::Double(v) => Some(*v),
        fastnbt::Value::Float(v) => Some(*v as f64),
        _ => None,
    }
}

fn nbt_list<'a>(compound: &'a HashMap<String, fastnbt::Value>, key: &str) -> Option<&'a Vec<fastnbt::Value>> {
    match nbt_get(compound, key)? {
        fastnbt::Value::List(v) => Some(v),
        _ => None,
    }
}

fn nbt_compound<'a>(compound: &'a HashMap<String, fastnbt::Value>, key: &str) -> Option<&'a HashMap<String, fastnbt::Value>> {
    match nbt_get(compound, key)? {
        fastnbt::Value::Compound(v) => Some(v),
        _ => None,
    }
}

// ── Core functions ──────────────────────────────────────────────────────────

fn get_server_path() -> Result<PathBuf, String> {
    let cfg = config::load_config();
    let p = PathBuf::from(&cfg.server_path);
    if p.as_os_str().is_empty() {
        return Err("Server path not configured".into());
    }
    Ok(p)
}

/// Resolve a player name to a UUID by reading `usercache.json`.
fn resolve_uuid(name: &str, server_path: &Path) -> Result<String, String> {
    let cache_path = server_path.join("usercache.json");
    if !cache_path.exists() {
        return Err(format!(
            "usercache.json not found at {}. Start the server at least once.",
            cache_path.display()
        ));
    }
    let raw = std::fs::read_to_string(&cache_path)
        .map_err(|e| format!("Failed to read usercache.json: {}", e))?;
    let entries: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("Failed to parse usercache.json: {}", e))?;

    if let Some(arr) = entries.as_array() {
        for entry in arr {
            let n = entry.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if n.eq_ignore_ascii_case(name) {
                if let Some(uuid) = entry.get("uuid").and_then(|v| v.as_str()) {
                    return Ok(uuid.to_string());
                }
            }
        }
    }
    Err(format!("Player '{}' not found in usercache.json. They must have joined the server at least once.", name))
}

/// Read and decompress a player .dat file, returning the root NBT compound.
fn read_player_dat(server_path: &Path, uuid: &str) -> Result<HashMap<String, fastnbt::Value>, String> {
    let dat_path = server_path
        .join("world")
        .join("playerdata")
        .join(format!("{}.dat", uuid));
    if !dat_path.exists() {
        return Err(format!(
            "Player data file not found: {}",
            dat_path.display()
        ));
    }
    let file = std::fs::File::open(&dat_path)
        .map_err(|e| format!("Failed to open {}: {}", dat_path.display(), e))?;
    let mut decoder = GzDecoder::new(file);
    let mut decompressed = Vec::new();
    decoder
        .read_to_end(&mut decompressed)
        .map_err(|e| format!("Failed to decompress {}: {}", dat_path.display(), e))?;
    let nbt: fastnbt::Value = fastnbt::from_bytes(&decompressed)
        .map_err(|e| format!("Failed to parse NBT: {}", e))?;
    match nbt {
        fastnbt::Value::Compound(root) => Ok(root),
        _ => Err("Unexpected NBT root type (expected Compound)".into()),
    }
}

/// Parse the Inventory list from NBT into structured slots.
fn parse_inventory(items: &[fastnbt::Value]) -> Vec<InventorySlot> {
    items
        .iter()
        .filter_map(|item| {
            let c = match item {
                fastnbt::Value::Compound(c) => c,
                _ => return None,
            };
            let id = nbt_str(c, "id").unwrap_or_else(|| "unknown".into());
            let count = nbt_i32(c, "count").unwrap_or(1);
            let slot = nbt_i32(c, "Slot").unwrap_or(0);

            // Try to get display name from tag.display.Name (JSON text component)
            let display_name = nbt_compound(c, "tag")
                .and_then(|tag| nbt_compound(tag, "display"))
                .and_then(|display| nbt_str(display, "Name"))
                .map(|name| {
                    // In 1.20.5+ this is a JSON text component string
                    if name.starts_with('{') {
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&name) {
                            if let Some(text) = v.get("text").and_then(|t| t.as_str()) {
                                return text.to_string();
                            }
                        }
                    }
                    name
                });

            Some(InventorySlot {
                slot,
                id,
                count,
                display_name,
            })
        })
        .collect()
}

/// Parse active effects from NBT.
fn parse_effects(effects: &[fastnbt::Value]) -> Vec<ActiveEffect> {
    effects
        .iter()
        .filter_map(|eff| {
            let c = match eff {
                fastnbt::Value::Compound(c) => c,
                _ => return None,
            };
            let id = nbt_str(c, "Id").unwrap_or_else(|| "unknown".into());
            let amplifier = nbt_i32(c, "Amplifier").unwrap_or(0);
            let duration = nbt_i32(c, "Duration").unwrap_or(0);
            let ambient = nbt_get(c, "Ambient")
                .and_then(|v| match v {
                    fastnbt::Value::Byte(b) => Some(*b != 0),
                    _ => None,
                })
                .unwrap_or(false);
            Some(ActiveEffect {
                id,
                amplifier,
                duration,
                ambient,
            })
        })
        .collect()
}

/// Map numeric gamemode ID to string.
fn parse_gamemode(id: i32) -> String {
    match id {
        0 => "survival".into(),
        1 => "creative".into(),
        2 => "adventure".into(),
        3 => "spectator".into(),
        _ => format!("unknown({})", id),
    }
}

// ── Tauri commands ──────────────────────────────────────────────────────────

pub fn get_player_full_data(name: String) -> Result<PlayerData, String> {
    let server_path = get_server_path()?;
    let uuid = resolve_uuid(&name, &server_path)?;
    let root = read_player_dat(&server_path, &uuid)?;

    let pos = nbt_list(&root, "Pos")
        .map(|list| {
            let x = list.first().and_then(|v| match v {
                fastnbt::Value::Double(d) => Some(*d),
                _ => None,
            }).unwrap_or(0.0);
            let y = list.get(1).and_then(|v| match v {
                fastnbt::Value::Double(d) => Some(*d),
                _ => None,
            }).unwrap_or(0.0);
            let z = list.get(2).and_then(|v| match v {
                fastnbt::Value::Double(d) => Some(*d),
                _ => None,
            }).unwrap_or(0.0);
            [x, y, z]
        })
        .unwrap_or([0.0, 0.0, 0.0]);

    let inventory = nbt_list(&root, "Inventory")
        .map(|l| parse_inventory(l))
        .unwrap_or_default();

    let ender_chest = nbt_list(&root, "EnderItems")
        .map(|l| parse_inventory(l))
        .unwrap_or_default();

    let active_effects = nbt_list(&root, "ActiveEffects")
        .or_else(|| nbt_list(&root, "active_effects"))
        .map(|l| parse_effects(l))
        .unwrap_or_default();

    Ok(PlayerData {
        name,
        uuid,
        health: nbt_f64(&root, "Health").unwrap_or(20.0),
        max_health: nbt_list(&root, "Attributes")
            .and_then(|attrs| {
                attrs.iter().find_map(|attr| {
                    let c = match attr {
                        fastnbt::Value::Compound(c) => c,
                        _ => return None,
                    };
                    let id = nbt_str(c, "id").unwrap_or_default();
                    if id == "minecraft:generic.max_health" {
                        nbt_f64(c, "base")
                    } else {
                        None
                    }
                })
            })
            .unwrap_or(20.0),
        food_level: nbt_i64(&root, "foodLevel").unwrap_or(20),
        xp_level: nbt_i64(&root, "XpLevel").unwrap_or(0),
        xp_total: nbt_i64(&root, "XpTotal").unwrap_or(0),
        gamemode: nbt_i32(&root, "playerGameType")
            .map(parse_gamemode)
            .unwrap_or_else(|| "survival".into()),
        position: pos,
        dimension: nbt_str(&root, "Dimension").unwrap_or_else(|| "minecraft:overworld".into()),
        inventory,
        ender_chest,
        active_effects,
    })
}

pub fn get_player_inventory_only(name: String) -> Result<Vec<InventorySlot>, String> {
    let server_path = get_server_path()?;
    let uuid = resolve_uuid(&name, &server_path)?;
    let root = read_player_dat(&server_path, &uuid)?;
    Ok(nbt_list(&root, "Inventory")
        .map(|l| parse_inventory(l))
        .unwrap_or_default())
}

pub fn get_player_uuid(name: String) -> Result<String, String> {
    let server_path = get_server_path()?;
    resolve_uuid(&name, &server_path)
}

// clear_player_item is handled on the frontend via send_command("clear <name> <item> [count]")
