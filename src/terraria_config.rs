//! Terraria server configuration management.
//!
//! Terraria uses `serverconfig.txt` (key=value format) instead of Minecraft's
//! `server.properties`. This module handles reading, writing, and generating
//! default config files for vanilla Terraria and tModLoader servers.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Default Terraria server configuration values.
pub fn default_config() -> HashMap<String, String> {
    let mut map = HashMap::new();
    map.insert("port".into(), "7777".into());
    map.insert("maxplayers".into(), "16".into());
    map.insert("password".into(), String::new());
    map.insert("motd".into(), "Welcome to my Terraria server!".into());
    map.insert("worldname".into(), "World".into());
    map.insert("difficulty".into(), "0".into()); // 0=classic, 1=expert, 2=master, 3=journey
    map.insert("autocreate".into(), "2".into()); // 1=small, 2=medium, 3=large
    map.insert("world".into(), String::new());
    map.insert("secure".into(), "0".into());
    map.insert("language".into(), "en-US".into());
    map.insert("npcstream".into(), "4".into());
    map
}

/// Generate a `serverconfig.txt` string for a new Terraria server.
///
/// `world_path` should be the full path to the `.wld` file, or empty to use autocreate.
/// `difficulty`: 0=classic, 1=expert, 2=master, 3=journey
/// `world_size`: 1=small, 2=medium, 3=large
pub fn generate_config(
    max_players: u32,
    server_name: &str,
    world_path: &str,
    difficulty: u8,
    world_size: u8,
) -> String {
    let mut lines = Vec::new();
    lines.push(format!("port=7777"));
    lines.push(format!("maxplayers={}", max_players));
    lines.push(format!("motd={}", server_name));
    lines.push(format!("difficulty={}", difficulty));
    if world_path.is_empty() {
        lines.push(format!("autocreate={}", world_size));
        lines.push(format!("worldname={}", server_name));
    } else {
        lines.push(format!("world={}", world_path));
    }
    lines.push(format!("secure=0"));
    lines.push(format!("npcstream=4"));
    lines.push(String::new()); // trailing newline
    lines.join("\n")
}

/// Parse a `serverconfig.txt` into a key-value map.
pub fn parse_config(path: &Path) -> Result<HashMap<String, String>, String> {
    let content = std::fs::read_to_string(path).map_err(|e| format!("Failed to read config: {}", e))?;
    let mut map = HashMap::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            map.insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    Ok(map)
}

/// Write a config map back to `serverconfig.txt`.
pub fn write_config(path: &Path, config: &HashMap<String, String>) -> Result<(), String> {
    let mut lines: Vec<String> = config
        .iter()
        .map(|(k, v)| format!("{}={}", k, v))
        .collect();
    lines.sort();
    lines.push(String::new()); // trailing newline
    std::fs::write(path, lines.join("\n")).map_err(|e| format!("Failed to write config: {}", e))
}

/// Update specific keys in an existing config file, preserving comments and order.
pub fn update_config_keys(path: &Path, updates: &HashMap<String, String>) -> Result<(), String> {
    let content = if path.exists() {
        std::fs::read_to_string(path).map_err(|e| format!("Failed to read config: {}", e))?
    } else {
        String::new()
    };

    let mut output_lines = Vec::new();
    let mut updated_keys: std::collections::HashSet<String> = std::collections::HashSet::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            output_lines.push(line.to_string());
            continue;
        }
        if let Some((key, _)) = trimmed.split_once('=') {
            let key = key.trim().to_string();
            if let Some(new_value) = updates.get(&key) {
                output_lines.push(format!("{}={}", key, new_value));
                updated_keys.insert(key);
            } else {
                output_lines.push(line.to_string());
            }
        } else {
            output_lines.push(line.to_string());
        }
    }

    // Append any new keys that weren't in the file
    for (key, value) in updates {
        if !updated_keys.contains(key) {
            output_lines.push(format!("{}={}", key, value));
        }
    }

    output_lines.push(String::new()); // trailing newline
    std::fs::write(path, output_lines.join("\n")).map_err(|e| format!("Failed to write config: {}", e))
}

/// Information about a Terraria world file.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WorldInfo {
    pub name: String,
    pub file_name: String,
    pub path: String,
    pub size_bytes: u64,
}

/// List all `.wld` files in the server's Worlds directory.
pub fn list_worlds(server_dir: &Path) -> Result<Vec<WorldInfo>, String> {
    let worlds_dir = server_dir.join("Worlds");
    if !worlds_dir.exists() {
        return Ok(Vec::new());
    }
    let mut worlds = Vec::new();
    for entry in std::fs::read_dir(&worlds_dir).map_err(|e| e.to_string())?.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|x| x == "wld") {
            let meta = entry.metadata().map_err(|e| e.to_string())?;
            let file_name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let name = path
                .file_stem()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            worlds.push(WorldInfo {
                name,
                file_name,
                path: path.to_string_lossy().to_string(),
                size_bytes: meta.len(),
            });
        }
    }
    worlds.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(worlds)
}

/// Create the Worlds directory if it doesn't exist.
pub fn ensure_worlds_dir(server_dir: &Path) -> Result<PathBuf, String> {
    let worlds_dir = server_dir.join("Worlds");
    std::fs::create_dir_all(&worlds_dir).map_err(|e| e.to_string())?;
    Ok(worlds_dir)
}

/// Get the path to `serverconfig.txt` in a server directory.
pub fn config_path(server_dir: &Path) -> PathBuf {
    server_dir.join("serverconfig.txt")
}

/// Difficulty level names for display.
pub fn difficulty_name(level: u8) -> &'static str {
    match level {
        0 => "Classic",
        1 => "Expert",
        2 => "Master",
        3 => "Journey",
        _ => "Unknown",
    }
}

/// World size names for display.
pub fn world_size_name(size: u8) -> &'static str {
    match size {
        1 => "Small",
        2 => "Medium",
        3 => "Large",
        _ => "Unknown",
    }
}
