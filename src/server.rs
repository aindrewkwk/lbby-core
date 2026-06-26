use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::process::ChildStdin;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ServerStatus {
    Stopped,
    Starting,
    Running,
    Stopping,
}

pub struct ServerManager {
    pub status: ServerStatus,
    pub stdin: Option<ChildStdin>,
    pub pid: Option<u32>,
    /// True when the user explicitly clicked Stop (or Restart). False when the
    /// server exited on its own (crash). The wait task uses this to decide
    /// whether to trigger an auto-restart.
    pub stop_requested: bool,
}

impl ServerManager {
    pub fn new() -> Self {
        Self {
            status: ServerStatus::Stopped,
            stdin: None,
            pid: None,
            stop_requested: false,
        }
    }
}

/// Stub: start the Minecraft server — full implementation to be migrated from monolithic lib.rs.
pub async fn start_server(app: std::sync::Arc<crate::app_state::AppEventSender>) -> Result<(), String> {
    let _ = app;
    Err("start_server not yet implemented in lbby-core".to_string())
}

/// Stub: stop the Minecraft server — full implementation to be migrated from monolithic lib.rs.
pub async fn stop_server(app: std::sync::Arc<crate::app_state::AppEventSender>) -> Result<(), String> {
    let _ = app;
    Err("stop_server not yet implemented in lbby-core".to_string())
}

// ── Extracted generic server functions ────────────────────────────────────────

/// Validate that a Terraria server directory has the files needed to launch.
///
/// Checks for the server binary and (on Windows) companion DLLs that the
/// binary needs to load. Returns Ok(()) if everything looks good, or a
/// descriptive error if something is missing.
pub fn validate_terraria_deps(server_dir: &Path, server_type: &crate::config::ServerType) -> Result<(), String> {
    match server_type {
        crate::config::ServerType::TModLoader => {
            let tmod_dll = server_dir.join("tModLoader.dll");
            if !tmod_dll.exists() {
                return Err(format!(
                    "tModLoader.dll not found at {}. Please reinstall tModLoader from Settings \u{2192} Version.",
                    tmod_dll.display()
                ));
            }
            // Check for bundled dotnet
            let dotnet_dir = server_dir.join("dotnet");
            let dotnet_exe = if cfg!(target_os = "windows") {
                dotnet_dir.join("dotnet.exe")
            } else {
                dotnet_dir.join("dotnet")
            };
            if !dotnet_exe.exists() {
                // Will fall back to system dotnet — just warn, don't fail
                eprintln!("[lbby] Warning: bundled dotnet not found at {}, will try system dotnet", dotnet_exe.display());
            }
        }
        crate::config::ServerType::Terraria => {
            let binary = terraria_server_binary(server_dir);
            if !binary.exists() {
                // Check if tModLoader fallback is available
                let tmod_dll = server_dir.join("tModLoader.dll");
                if !tmod_dll.exists() {
                    return Err(format!(
                        "TerrariaServer binary not found at {}. Please reinstall the server from Settings \u{2192} Version.",
                        binary.display()
                    ));
                }
                // tModLoader fallback is available — that's fine
                return Ok(());
            }
            // Binary exists — check for companion DLLs on Windows
            #[cfg(target_os = "windows")]
            {
                let parent = binary.parent().unwrap_or(server_dir);
                let has_companion = parent.join("SDL2.dll").exists()
                    || parent.join("MonoGame.Framework.dll").exists()
                    || parent.join("FNA.dll").exists()
                    || parent.join("Terraria.exe").exists(); // full game install has Terraria.exe
                if !has_companion {
                    // Check if there are any .dll files at all (SteamCMD copy may use different names)
                    let has_any_dll = std::fs::read_dir(parent)
                        .ok()
                        .and_then(|mut entries| entries.find_map(|e| {
                            e.ok().filter(|e| e.path().extension().map_or(false, |ext| ext == "dll"))
                                .map(|_| true)
                        }))
                        .unwrap_or(false);
                    if !has_any_dll {
                        return Err(format!(
                            "TerrariaServer.exe is missing required companion files (DLLs). \
                             Please reinstall the server from Settings \u{2192} Version, or copy the \
                             full server directory from your Terraria Steam installation."
                        ));
                    }
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// Discover an existing world file in the server's Worlds directory.
///
/// Priority:
/// 1. Exact match: `{server_name}.wld`
/// 2. Any single `.wld` file in the directory
/// 3. None (caller should use -autocreate)
pub fn discover_world_file(server_dir: &Path, server_name: &str) -> Option<PathBuf> {
    let worlds_dir = server_dir.join("Worlds");

    // 1. Exact name match
    let exact = worlds_dir.join(format!("{}.wld", server_name));
    if exact.exists() {
        return Some(exact);
    }

    // 2. Any single .wld file
    let entries: Vec<PathBuf> = std::fs::read_dir(&worlds_dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "wld"))
        .map(|e| e.path())
        .collect();

    match entries.len() {
        0 => None,
        1 => Some(entries.into_iter().next().unwrap()),
        _ => {
            // Multiple worlds — pick the most recently modified
            entries.into_iter().max_by_key(|p| {
                std::fs::metadata(p)
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
            })
        }
    }
}

/// Returns optimized JVM flags for Minecraft servers.
pub fn optimized_jvm_flags() -> &'static [&'static str] {
    &[
        "-XX:+UseG1GC",
        "-XX:+ParallelRefProcEnabled",
        "-XX:MaxGCPauseMillis=200",
        "-XX:+UnlockExperimentalVMOptions",
        "-XX:+DisableExplicitGC",
        "-XX:+AlwaysPreTouch",
        "-XX:G1NewSizePercent=30",
        "-XX:G1MaxNewSizePercent=40",
        "-XX:G1HeapRegionSize=8M",
        "-XX:G1ReservePercent=20",
        "-XX:G1HeapWastePercent=5",
        "-XX:G1MixedGCCountTarget=4",
        "-XX:InitiatingHeapOccupancyPercent=15",
        "-XX:G1MixedGCLiveThresholdPercent=90",
        "-XX:G1RSetUpdatingPauseTimePercent=5",
        "-XX:SurvivorRatio=32",
        "-XX:+PerfDisableSharedMem",
        "-XX:MaxTenuringThreshold=1",
    ]
}

/// Manages JVM args in user_jvm_args.txt, replacing only the managed block.
pub fn upsert_managed_jvm_args(server_dir: &Path, cfg: &crate::config::ServerConfig) -> Result<(), String> {
    std::fs::create_dir_all(server_dir)
        .map_err(|e| format!("Failed to create server directory {}: {}", server_dir.display(), e))?;
    let path = server_dir.join("user_jvm_args.txt");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let start = "# Lbby managed JVM flags - start";
    let end = "# Lbby managed JVM flags - end";

    let mut kept = Vec::new();
    let mut skipping = false;
    for line in existing.lines() {
        if line.trim() == start {
            skipping = true;
            continue;
        }
        if line.trim() == end {
            skipping = false;
            continue;
        }
        if !skipping {
            kept.push(line.to_string());
        }
    }

    let mut managed = vec![
        start.to_string(),
        format!("-Xmx{}M", cfg.ram_mb),
        format!("-Xms{}M", (cfg.ram_mb / 2).max(512)),
    ];
    if cfg.optimized_jvm_flags {
        managed.extend(optimized_jvm_flags().iter().map(|s| s.to_string()));
    }
    managed.push(end.to_string());

    while kept.last().is_some_and(|line| line.trim().is_empty()) {
        kept.pop();
    }
    let mut output = managed.join("\n");
    if !kept.is_empty() {
        output.push_str("\n\n");
        output.push_str(&kept.join("\n"));
    }
    output.push('\n');

    std::fs::write(&path, output)
        .map_err(|e| format!("Failed to write {}: {}", path.display(), e))
}

/// Check whether a string is a valid Minecraft player name (2-16 alphanumeric/underscore chars).
fn is_valid_player_name(s: &str) -> bool {
    let len = s.chars().count();
    (2..=16).contains(&len) && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Parse a player name out of a "joined the game" / "left the game" / "lost connection" line.
/// MC log format example: "...MinecraftServer/]: Fishgod212 joined the game"
pub fn parse_player_event(line: &str) -> Option<(String, bool)> {
    // Returns (name, is_join)
    let body = line.rsplit("]:").next()?.trim();
    if let Some(name) = body.strip_suffix(" joined the game") {
        let n = name.trim();
        if is_valid_player_name(n) { return Some((n.to_string(), true)); }
    }
    if let Some(name) = body.strip_suffix(" left the game") {
        let n = name.trim();
        if is_valid_player_name(n) { return Some((n.to_string(), false)); }
    }
    if let Some(rest) = body.strip_suffix(": Disconnected") {
        if let Some(name) = rest.strip_suffix(" lost connection") {
            let n = name.trim();
            if is_valid_player_name(n) { return Some((n.to_string(), false)); }
        }
    }
    // Generic "<name> lost connection: <reason>"
    if let Some(idx) = body.find(" lost connection:") {
        let n = body[..idx].trim();
        if is_valid_player_name(n) { return Some((n.to_string(), false)); }
    }
    None
}

/// Parse Terraria/tModLoader player join/leave messages.
/// Terraria format: "<name> has joined." / "<name> has left."
/// tModLoader may also use: "<name> has joined" (no period)
pub fn parse_terraria_player_event(line: &str) -> Option<(String, bool)> {
    let trimmed = line.trim();
    // Join: "<name> has joined." or "<name> has joined"
    if let Some(rest) = trimmed.strip_suffix(" has joined.") {
        let n = rest.trim();
        if is_valid_player_name(n) { return Some((n.to_string(), true)); }
    }
    if let Some(rest) = trimmed.strip_suffix(" has joined") {
        let n = rest.trim();
        if is_valid_player_name(n) { return Some((n.to_string(), true)); }
    }
    // Leave: "<name> has left." or "<name> has left"
    if let Some(rest) = trimmed.strip_suffix(" has left.") {
        let n = rest.trim();
        if is_valid_player_name(n) { return Some((n.to_string(), false)); }
    }
    if let Some(rest) = trimmed.strip_suffix(" has left") {
        let n = rest.trim();
        if is_valid_player_name(n) { return Some((n.to_string(), false)); }
    }
    None
}

/// Write common Terraria serverconfig.txt settings for both TModLoader and
/// vanilla-Terraria-fallback-to-TModLoader paths.  Creates the Worlds
/// directory, decides between `-world` (existing) and `-autocreate` (new),
/// and writes the key/value pairs via `update_config_keys`.
pub fn write_terraria_serverconfig(cfg: &crate::config::ServerConfig, server_dir: &std::path::Path) {
    let worlds_dir = server_dir.join("Worlds");
    let config_path = server_dir.join("serverconfig.txt");
    let _ = std::fs::create_dir_all(&worlds_dir);
    let mut updates = std::collections::HashMap::new();
    updates.insert("port".to_string(), cfg.default_port().to_string());
    updates.insert("maxplayers".to_string(), cfg.max_players.to_string());
    updates.insert("motd".to_string(), cfg.server_name.clone());
    updates.insert("worldname".to_string(), cfg.server_name.clone());
    updates.insert("difficulty".to_string(), cfg.terraria_difficulty.to_string());
    updates.insert("secure".to_string(), "0".to_string());
    updates.insert("npcstream".to_string(), "4".to_string());
    updates.insert("language".to_string(), "en-US".to_string());
    // New fields: seed, evil, password
    if !cfg.terraria_seed.is_empty() {
        updates.insert("seed".to_string(), cfg.terraria_seed.clone());
    }
    if cfg.terraria_evil > 0 {
        updates.insert("evil".to_string(), cfg.terraria_evil.to_string());
    }
    if !cfg.terraria_password.is_empty() {
        updates.insert("password".to_string(), cfg.terraria_password.clone());
    }
    // tModLoader-specific: modpath, modpack
    if cfg.server_type == crate::config::ServerType::TModLoader {
        if !cfg.tmod_modpath.is_empty() {
            updates.insert("modpath".to_string(), cfg.tmod_modpath.clone());
        }
        if !cfg.tmod_modpack.is_empty() {
            updates.insert("modpack".to_string(), cfg.tmod_modpack.clone());
        }
    }
    // Use discover_world_file to find existing worlds (exact name match first,
    // then any .wld file, then fall back to autocreate).
    if let Some(world_file) = discover_world_file(server_dir, &cfg.server_name) {
        updates.insert("world".to_string(), world_file.to_string_lossy().to_string());
    } else {
        updates.insert("autocreate".to_string(), cfg.terraria_world_size.to_string());
    }
    if let Err(e) = crate::terraria_config::update_config_keys(&config_path, &updates) {
        eprintln!("[lbby] Warning: failed to update serverconfig.txt: {}", e);
    }
}

/// Get the path to the TerrariaServer binary in a given directory.
/// Checks multiple possible locations (direct, inside .app bundle, subdirectories).
pub fn terraria_server_binary(server_dir: &Path) -> PathBuf {
    // Direct binary
    let direct = if cfg!(target_os = "windows") {
        server_dir.join("TerrariaServer.exe")
    } else {
        server_dir.join("TerrariaServer")
    };
    if direct.exists() {
        return direct;
    }

    // macOS: inside .app bundle
    let app_bundle = server_dir.join("Terraria Server.app/Contents/MacOS/TerrariaServer");
    if app_bundle.exists() {
        return app_bundle;
    }

    // Windows: TerrariaServer.exe might be in a subdirectory
    if cfg!(target_os = "windows") {
        let nested = server_dir.join("Windows").join("TerrariaServer.exe");
        if nested.exists() {
            return nested;
        }
    }

    // Linux: binary might have architecture suffix
    let linux64 = server_dir.join("Linux").join("TerrariaServer.bin.x86_64");
    if linux64.exists() {
        return linux64;
    }
    let linux32 = server_dir.join("Linux").join("TerrariaServer.bin.x86");
    if linux32.exists() {
        return linux32;
    }

    // Return the default path even if it doesn't exist (for error messages)
    direct
}

/// Find the TerrariaServer binary by searching common locations.
/// Used during installation to locate the binary after download/extraction.
pub fn find_terraria_binary(dir: &Path) -> Option<PathBuf> {
    let candidates = vec![
        dir.join("TerrariaServer"),
        dir.join("TerrariaServer.exe"),
        dir.join("Terraria Server.app/Contents/MacOS/TerrariaServer"),
        dir.join("Windows/TerrariaServer.exe"),
        dir.join("Linux/TerrariaServer.bin.x86_64"),
        dir.join("Linux/TerrariaServer.bin.x86"),
    ];

    for candidate in candidates {
        if candidate.exists() {
            return Some(candidate);
        }
    }

    // Recursive search as last resort
    for entry in walkdir::WalkDir::new(dir).max_depth(3).into_iter().flatten() {
        let name = entry.file_name().to_string_lossy();
        if name == "TerrariaServer" || name == "TerrariaServer.exe" || name.starts_with("TerrariaServer.bin") {
            return Some(entry.path().to_path_buf());
        }
    }

    None
}

/// Parse a "The time is N" log line that responds to `time query gametime`.
/// Returns the in-game tick count.
pub fn parse_gametime_line(line: &str) -> Option<u64> {
    // Format: "[12:34:56] [Server thread/INFO]: The time is 12345"
    let body = line.rsplit("]:").next()?.trim();
    let rest = body.strip_prefix("The time is ")?;
    rest.trim().parse().ok()
}

/// Parse a TPS value out of an MC log line.
/// Recognised formats:
///   - Paper:  "TPS from last 1m, 5m, 15m: {color}*20.0, {color}*20.0, {color}*20.0"
///   - Forge:  "Overall: Mean tick time: 12.345 ms. Mean TPS: 20.000"
///   - NeoForge: same as Forge
pub fn parse_tps_line(line: &str) -> Option<f32> {
    if line.contains("Mean TPS:") {
        let after = line.rsplit("Mean TPS:").next()?;
        let tok = after.split_whitespace().next()?;
        let cleaned = tok.trim_end_matches('.');
        let v: f32 = cleaned.parse().ok()?;
        if v > 0.0 && v <= 25.0 { return Some(v); }
    }
    if line.contains("TPS from last") {
        // Strip Minecraft color codes ({color} + 1 char) and the "*" marker
        let after = line.split(':').nth(1)?;
        let cleaned: String = after.chars().filter(|c| !matches!(c, '\u{a7}' | '*')).collect();
        // Walk and grab the first plausible TPS value (1-25)
        let mut buf = String::new();
        for c in cleaned.chars() {
            if c.is_ascii_digit() || c == '.' { buf.push(c); }
            else if !buf.is_empty() {
                if let Ok(v) = buf.parse::<f32>() {
                    if v > 0.0 && v <= 25.0 { return Some(v); }
                }
                buf.clear();
            }
        }
        if !buf.is_empty() {
            if let Ok(v) = buf.parse::<f32>() { if v > 0.0 && v <= 25.0 { return Some(v); } }
        }
    }
    None
}

/// Read banned players from the server's ban storage (JSON for MC, txt for Terraria).
pub async fn get_banned_players() -> Result<Vec<crate::app_state::BannedPlayer>, String> {
    let cfg = crate::config::load_config();
    if cfg.is_terraria() {
        // Terraria stores bans in banlist.txt — one player name per line.
        let path = PathBuf::from(&cfg.server_path).join("banlist.txt");
        if !path.exists() {
            return Ok(Vec::new());
        }
        let raw = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;
        let mut players: Vec<crate::app_state::BannedPlayer> = raw
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .map(|name| crate::app_state::BannedPlayer {
                name: name.to_string(),
                uuid: String::new(),
                created: String::new(),
                source: "Server".to_string(),
                expires: "forever".to_string(),
                reason: String::new(),
            })
            .collect();
        players.sort_by_key(|a| a.name.to_lowercase());
        Ok(players)
    } else {
        // Minecraft stores bans in banned-players.json.
        let path = PathBuf::from(&cfg.server_path).join("banned-players.json");
        if !path.exists() {
            return Ok(Vec::new());
        }
        let raw = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;
        if raw.trim().is_empty() {
            return Ok(Vec::new());
        }
        let mut players: Vec<crate::app_state::BannedPlayer> = serde_json::from_str(&raw)
            .map_err(|e| format!("Failed to parse {}: {}", path.display(), e))?;
        players.sort_by_key(|a| a.name.to_lowercase());
        Ok(players)
    }
}

/// Read and parse server.properties into a key-value map.
pub fn get_server_properties() -> Result<HashMap<String, String>, String> {
    let cfg = crate::config::load_config();
    let path = PathBuf::from(&cfg.server_path).join("server.properties");
    let content = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let mut map = HashMap::new();
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() { continue; }
        if let Some((k, v)) = line.split_once('=') {
            map.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    Ok(map)
}

/// Write a key-value map to server.properties.
pub fn save_server_properties(props: HashMap<String, String>) -> Result<(), String> {
    let cfg = crate::config::load_config();
    let path = PathBuf::from(&cfg.server_path).join("server.properties");
    let mut lines = vec!["# Minecraft server properties".to_string()];
    let mut sorted: Vec<_> = props.iter().collect();
    sorted.sort_by_key(|(k, _)| k.as_str());
    for (k, v) in sorted {
        lines.push(format!("{}={}", k, v));
    }
    std::fs::write(&path, lines.join("\n") + "\n").map_err(|e| e.to_string())
}

/// Save an uploaded PNG as server-icon.png after validation.
pub async fn upload_server_icon(file_path: String) -> Result<(), String> {
    let cfg = crate::config::load_config();
    if cfg.server_path.trim().is_empty() {
        return Err("Server path is not configured".to_string());
    }
    let src = PathBuf::from(file_path);
    let bytes = tokio::fs::read(&src).await.map_err(|e| e.to_string())?;
    validate_server_icon_png(&bytes)?;
    let dest = PathBuf::from(&cfg.server_path).join("server-icon.png");
    tokio::fs::write(&dest, bytes)
        .await
        .map_err(|e| format!("Failed to write {}: {}", dest.display(), e))?;
    Ok(())
}

/// Validate that a PNG is exactly 64x64 pixels (Minecraft server icon requirement).
pub fn validate_server_icon_png(bytes: &[u8]) -> Result<(), String> {
    if bytes.len() < 24 || !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err("Server icon must be a PNG file".to_string());
    }
    let width = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let height = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    if width != 64 || height != 64 {
        return Err(format!("Server icon must be exactly 64x64 pixels, got {}x{}", width, height));
    }
    Ok(())
}

/// Returns the extras subfolder name for the given server type
/// (plugins for Bukkit-family, Mods for Terraria, mods for everything else).
pub fn extras_folder(cfg: &crate::config::ServerConfig) -> &'static str {
    match cfg.server_type {
        crate::config::ServerType::Paper | crate::config::ServerType::Bukkit | crate::config::ServerType::Spigot
        | crate::config::ServerType::Folia | crate::config::ServerType::Purpur => "plugins",
        crate::config::ServerType::Terraria | crate::config::ServerType::TModLoader => "Mods",
        _ => "mods",
    }
}

/// Kill `root_pid` and every descendant on Unix-like systems by walking
/// `pgrep -P` recursively. SIGTERM first, then SIGKILL after a brief grace.
#[cfg(unix)]
pub async fn kill_unix_process_tree(root_pid: u32) {
    fn collect_descendants(root: u32) -> Vec<u32> {
        let mut all = vec![root];
        let mut frontier = vec![root];
        while let Some(p) = frontier.pop() {
            if let Ok(out) = std::process::Command::new("pgrep").args(["-P", &p.to_string()]).output() {
                if out.status.success() {
                    for line in String::from_utf8_lossy(&out.stdout).lines() {
                        if let Ok(child) = line.trim().parse::<u32>() {
                            // Verify the child's parent is still `p` to guard against
                            // PID recycling: a PID returned by pgrep may have been
                            // reused by an unrelated process since we started.
                            let check = std::process::Command::new("ps")
                                .args(["-o", "ppid=", "-p", &child.to_string()])
                                .output();
                            if let Ok(chk_out) = check {
                                let ppid = String::from_utf8_lossy(&chk_out.stdout)
                                    .trim().parse::<u32>().unwrap_or(0);
                                if ppid != p { continue; }
                            }
                            all.push(child);
                            frontier.push(child);
                        }
                    }
                }
            }
        }
        all
    }
    let pids = collect_descendants(root_pid);
    // SIGTERM (15) the whole tree
    for pid in &pids {
        unsafe { libc::kill(*pid as i32, libc::SIGTERM); }
    }
    tokio::time::sleep(tokio::time::Duration::from_millis(800)).await;
    // SIGKILL (9) any survivors
    for pid in &pids {
        unsafe { libc::kill(*pid as i32, libc::SIGKILL); }
    }
}
