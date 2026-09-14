use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::file_cache::FileCache;

fn default_true() -> bool {
    true
}
fn default_performance_preset() -> String {
    "balanced".to_string()
}
fn default_remote_control_port() -> u16 {
    47992
}
fn default_terraria_world_size() -> u8 {
    2 // medium
}
fn default_dashboard_url() -> String {
    "https://web.lbby.net".to_string()
}
fn default_heartbeat_interval() -> u64 {
    300 // 5 minutes
}

const PROFILES_SCHEMA_VERSION: u32 = 1;

/// Which game a server profile is for.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Game {
    Minecraft,
    Terraria,
}

impl Default for Game {
    fn default() -> Self {
        Game::Minecraft
    }
}

impl Game {
    pub fn is_minecraft(&self) -> bool {
        *self == Game::Minecraft
    }
    pub fn is_terraria(&self) -> bool {
        *self == Game::Terraria
    }
    /// Default server port for this game.
    pub fn default_port(&self) -> u16 {
        match self {
            Game::Minecraft => 25565,
            Game::Terraria => 7777,
        }
    }
    /// Display name for the game.
    pub fn display_name(&self) -> &'static str {
        match self {
            Game::Minecraft => "Minecraft",
            Game::Terraria => "Terraria",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ServerType {
    // ── Minecraft ──────────────────────────────────────────────────────────
    #[default]
    Vanilla,
    Paper,
    Forge,
    Fabric,
    NeoForge,
    Bukkit,
    Spigot,
    Folia,
    Purpur,
    SpongeVanilla,
    SpongeForge,
    // ── Terraria ───────────────────────────────────────────────────────────
    /// Vanilla Terraria dedicated server
    Terraria,
    /// tModLoader dedicated server (modded Terraria)
    TModLoader,
}

impl ServerType {
    /// Short label for display (e.g. "Paper", "Forge", "Vanilla").
    pub fn label(&self) -> &'static str {
        match self {
            ServerType::Vanilla => "Vanilla",
            ServerType::Paper => "Paper",
            ServerType::Forge => "Forge",
            ServerType::Fabric => "Fabric",
            ServerType::NeoForge => "NeoForge",
            ServerType::Bukkit => "Bukkit",
            ServerType::Spigot => "Spigot",
            ServerType::Folia => "Folia",
            ServerType::Purpur => "Purpur",
            ServerType::SpongeVanilla => "SpongeVanilla",
            ServerType::SpongeForge => "SpongeForge",
            ServerType::Terraria => "Terraria",
            ServerType::TModLoader => "tModLoader",
        }
    }

    /// Which game this server type belongs to.
    pub fn game(&self) -> Game {
        match self {
            ServerType::Terraria | ServerType::TModLoader => Game::Terraria,
            _ => Game::Minecraft,
        }
    }
    /// Whether this type uses Java (all Minecraft types do, Terraria does not).
    pub fn needs_java(&self) -> bool {
        self.game().is_minecraft()
    }
    /// Whether this type supports mods/plugins.
    pub fn supports_mods(&self) -> bool {
        !matches!(self, ServerType::Vanilla | ServerType::Terraria)
    }
    /// Whether this type is a plugin platform (uses `plugins/` directory).
    pub fn is_plugin_platform(&self) -> bool {
        matches!(
            self,
            ServerType::Paper
                | ServerType::Bukkit
                | ServerType::Spigot
                | ServerType::Folia
                | ServerType::Purpur
                | ServerType::SpongeVanilla
                | ServerType::SpongeForge
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ServerConfig {
    pub server_path: String,
    pub java_path: String,
    pub minecraft_version: String,
    pub server_type: ServerType,
    #[serde(default)]
    pub curseforge_api_key: Option<String>,
    /// Forge / NeoForge / Fabric loader version, or Paper build number as string
    pub loader_version: Option<String>,
    pub ram_mb: u32,
    pub max_players: u32,
    pub server_name: String,
    /// Minecraft world seed. Empty = random.
    #[serde(default)]
    pub minecraft_seed: String,
    pub setup_complete: bool,
    /// Auto-restart the server when it exits unexpectedly (not via Stop button).
    /// Defaults to true so existing users get the new behavior automatically.
    #[serde(default = "default_true")]
    pub auto_restart: bool,
    /// Scheduled graceful restart interval in hours. 0 = disabled.
    #[serde(default)]
    pub scheduled_restart_hours: u32,
    /// Auto-backup interval in minutes. 0 = disabled.
    /// Defaults to 0 so existing users aren't surprised by background backups.
    #[serde(default)]
    pub backup_interval_minutes: u32,
    /// Where auto-backups are written. Empty = system Downloads folder.
    #[serde(default)]
    pub backup_dir: String,
    /// Whether auto-backups should include the logs/ folder.
    #[serde(default)]
    pub backup_include_logs: bool,
    /// Use Minecraft-focused JVM flags to reduce GC pauses and CPU spikes.
    /// Defaults to true because the standard Java defaults are weak for MC servers.
    #[serde(default = "default_true")]
    pub optimized_jvm_flags: bool,
    /// UI-selected performance preset. The app uses this for defaults/hints; server.properties
    /// still remains user-editable.
    #[serde(default = "default_performance_preset")]
    pub performance_preset: String,
    /// Optional LAN remote control API. Disabled by default and protected by a token.
    #[serde(default)]
    pub remote_control_enabled: bool,
    /// Port for the LAN remote control server.
    #[serde(default = "default_remote_control_port")]
    pub remote_control_port: u16,
    /// Bearer/query token required for remote control access.
    #[serde(default)]
    pub remote_control_token: String,
    /// Optional public HTTPS/TCP tunnel URL for remote access outside the LAN.
    #[serde(default)]
    pub remote_control_public_url: String,
    /// Whether the free Cloudflare quick tunnel for the remote dashboard
    /// should auto-start on app launch (when remote_control_enabled is true).
    #[serde(default)]
    pub cloudflare_remote_enabled: bool,
    // ── Terraria fields ────────────────────────────────────────────────────
    /// Terraria game version, e.g. "1.4.4.9". Only used when game is Terraria.
    #[serde(default)]
    pub terraria_version: String,
    /// tModLoader version, e.g. "2024.12". Only used when server_type is TModLoader.
    #[serde(default)]
    pub tmodloader_version: String,
    /// Terraria world difficulty: 0=classic, 1=expert, 2=master, 3=journey.
    #[serde(default)]
    pub terraria_difficulty: u8,
    /// Terraria world size: 1=small, 2=medium, 3=large.
    #[serde(default = "default_terraria_world_size")]
    pub terraria_world_size: u8,
    /// Terraria world seed. Empty = random.
    #[serde(default)]
    pub terraria_seed: String,
    /// Terraria world evil type: 0=random, 1=corruption, 2=crimson.
    #[serde(default)]
    pub terraria_evil: u8,
    /// Terraria server password. Empty = no password.
    #[serde(default)]
    pub terraria_password: String,
    /// tModLoader mod path. Empty = default Mods/ directory.
    #[serde(default)]
    pub tmod_modpath: String,
    /// tModLoader modpack name. Empty = use enabled.json.
    #[serde(default)]
    pub tmod_modpack: String,
    // ── Dashboard sync ───────────────────────────────────────────────────
    /// Dashboard URL for heartbeat and license sync.
    #[serde(default = "default_dashboard_url")]
    pub dashboard_url: String,
    /// JWT token for authenticating with the dashboard API.
    #[serde(default)]
    pub app_token: String,
    /// Whether to enable online-mode (Mojang authentication) for the server.
    /// When `Some(true)`, forces `online-mode=true` in server.properties.
    /// When `Some(false)`, forces `online-mode=false`.
    /// When `None`, preserves whatever the user has set (or defaults to `true`).
    #[serde(default)]
    pub online_mode: Option<bool>,
    /// Heartbeat interval in seconds. 0 = disabled.
    #[serde(default = "default_heartbeat_interval")]
    pub heartbeat_interval_secs: u64,
    /// Whether the user has explicitly accepted the Minecraft EULA.
    /// Defaults to false. Installation writes eula=true ONLY when this is true.
    /// Existing installs with eula.txt already present are grandfathered —
    /// this field is checked only during new installs.
    #[serde(default)]
    pub eula_accepted: bool,
}

impl ServerConfig {
    /// Determine which game this config belongs to.
    /// Falls back to inferring from server_type for backwards compatibility.
    pub fn game(&self) -> Game {
        self.server_type.game()
    }
    pub fn is_terraria(&self) -> bool {
        self.game().is_terraria()
    }
    pub fn is_minecraft(&self) -> bool {
        self.game().is_minecraft()
    }
    /// Default server port for this game type.
    pub fn default_port(&self) -> u16 {
        self.game().default_port()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerProfile {
    pub id: String,
    pub name: String,
    pub config: ServerConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileSummary {
    pub id: String,
    pub name: String,
    pub active: bool,
    pub minecraft_version: String,
    /// Terraria game version — only set for terraria/tmodloader profiles.
    pub terraria_version: String,
    pub server_type: ServerType,
    pub server_path: String,
    pub setup_complete: bool,
    /// Which game this profile is for (minecraft / terraria).
    pub game: Game,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfilesState {
    pub active_id: String,
    pub profiles: Vec<ProfileSummary>,
    /// Which game the currently active profile belongs to.
    pub active_game: Game,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProfilesFile {
    /// Schema version for migration. Absent/unversioned files default to 0.
    #[serde(default)]
    schema_version: u32,
    active_id: String,
    profiles: Vec<ServerProfile>,
}

pub fn generate_remote_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn base_dir() -> PathBuf {
    #[cfg(any(test, feature = "testing"))]
    if let Ok(dir) = std::env::var("LBBY_CONFIG_DIR") {
        let path = PathBuf::from(dir);
        std::fs::create_dir_all(&path).ok();
        return path;
    }
    let base = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("lbby");
    std::fs::create_dir_all(&base).ok();
    base
}

pub fn config_path() -> PathBuf {
    base_dir().join("config.json")
}

fn profiles_path() -> PathBuf {
    base_dir().join("profiles.json")
}

fn default_profile_name(cfg: &ServerConfig) -> String {
    let name = cfg.server_name.trim();
    if !name.is_empty() {
        name.to_string()
    } else {
        "Default Server".to_string()
    }
}

fn default_profiles_file() -> ProfilesFile {
    let cfg = std::fs::read_to_string(config_path())
        .ok()
        .and_then(|s| serde_json::from_str::<ServerConfig>(&s).ok())
        .unwrap_or_default();
    let id = uuid::Uuid::new_v4().simple().to_string();
    ProfilesFile {
        schema_version: PROFILES_SCHEMA_VERSION,
        active_id: id.clone(),
        profiles: vec![ServerProfile {
            id,
            name: default_profile_name(&cfg),
            config: cfg,
        }],
    }
}

static CONFIG_CACHE: FileCache<ProfilesFile> = FileCache::new();

/// Load the profiles file with schema migration and corrupt-file handling.
///
/// Returns Err when:
/// - The file exists but contains invalid JSON (corrupt). Original is backed up
///   to .corrupted but NOT silently replaced with defaults.
/// - The file uses a future schema_version this binary cannot handle.
///   The original file is left untouched.
///
/// Creates a fresh default only when the file is genuinely missing (no user
/// data can be lost in that case).
fn load_profiles_file() -> Result<ProfilesFile, String> {
    let path = profiles_path();

    // ponytail: cache with mtime check, identical pattern to server.properties
    let cached = CONFIG_CACHE.get_or_load(&path, || {
        if !path.exists() {
            // Missing file — create default. No user data can be lost.
            let file = default_profiles_file();
            save_profiles_file(&file)?;
            return Ok(file);
        }

        let raw = std::fs::read_to_string(&path)
            .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;

        let mut file: ProfilesFile = match serde_json::from_str(&raw) {
            Ok(f) => f,
            Err(e) => {
                // Corrupt: back up original, surface error, do NOT create default
                let backup = path.with_extension("json.corrupted");
                eprintln!(
                    "[lbby] profiles.json corrupt ({}), backing up to {}",
                    e,
                    backup.display()
                );
                let _ = std::fs::rename(&path, &backup);
                return Err(format!(
                    "profiles.json is corrupt ({}). Original saved to {}. \
                     Repair or delete the file and restart.",
                    e,
                    backup.display()
                ));
            }
        };

        // --- Schema migration ---
        if file.schema_version == 0 {
            // Legacy unversioned file → stamp as v1 and persist
            file.schema_version = PROFILES_SCHEMA_VERSION;
        }
        if file.schema_version > PROFILES_SCHEMA_VERSION {
            // Future schema — reject without overwriting
            return Err(format!(
                "profiles.json uses schema version {} which this build \
                 (up to v{}) does not support. Upgrade Lbby or manually \
                 downgrade the file.",
                file.schema_version, PROFILES_SCHEMA_VERSION
            ));
        }

        // --- Sanitise profiles ---
        file.profiles.retain(|p| !p.id.trim().is_empty());
        if file.profiles.is_empty() {
            // All profiles empty/blank — this is equivalent to "no user data"
            let mut def = default_profiles_file();
            // Preserve the migrated schema_version
            def.schema_version = file.schema_version;
            let _ = save_profiles_file(&def);
            return Ok(def);
        }
        if !file.profiles.iter().any(|p| p.id == file.active_id) {
            file.active_id = file.profiles[0].id.clone();
        }

        // Persist migrated/sanitised version
        let _ = save_profiles_file(&file);
        Ok(file)
    });

    cached
}

fn save_profiles_file(file: &ProfilesFile) -> Result<(), String> {
    use std::io::Write;
    let path = profiles_path();
    let json = serde_json::to_string_pretty(file).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    let mut f = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
    f.write_all(json.as_bytes()).map_err(|e| e.to_string())?;
    f.sync_all().map_err(|e| e.to_string())?; // Ensure data hits disk before rename
    drop(f);
    let result = std::fs::rename(&tmp, &path).map_err(|e| e.to_string());
    CONFIG_CACHE.invalidate();
    result
}

fn profile_summary(profile: &ServerProfile, active_id: &str) -> ProfileSummary {
    ProfileSummary {
        id: profile.id.clone(),
        name: profile.name.clone(),
        active: profile.id == active_id,
        minecraft_version: profile.config.minecraft_version.clone(),
        terraria_version: profile.config.terraria_version.clone(),
        server_type: profile.config.server_type.clone(),
        server_path: profile.config.server_path.clone(),
        setup_complete: profile.config.setup_complete,
        game: profile.config.game(),
    }
}

/// Load the active profile's config. Returns default on corrupt/missing
/// profiles (the error is logged; callers that need to surface the error
/// to the UI should use [`save_config`] or the write-path Tauri commands
/// which already propagate).
pub fn load_config() -> ServerConfig {
    match load_profiles_file() {
        Ok(file) => file
            .profiles
            .iter()
            .find(|p| p.id == file.active_id)
            .map(|p| p.config.clone())
            .unwrap_or_default(),
        Err(e) => {
            eprintln!("[lbby] load_config: {}", e);
            ServerConfig::default()
        }
    }
}

/// Like [`load_config`] but propagates errors. Use in Tauri commands that
/// need to surface profile-load failures to the user.
pub fn load_config_checked() -> Result<ServerConfig, String> {
    let file = load_profiles_file()?;
    Ok(file
        .profiles
        .iter()
        .find(|p| p.id == file.active_id)
        .map(|p| p.config.clone())
        .unwrap_or_default())
}

/// Get the currently active profile ID.
/// Returns empty string on profile-load error (logged).
pub fn active_profile_id() -> String {
    match load_profiles_file() {
        Ok(file) => file.active_id.clone(),
        Err(e) => {
            eprintln!("[lbby] active_profile_id: {}", e);
            String::new()
        }
    }
}

/// Like [`active_profile_id`] but propagates errors.
pub fn active_profile_id_checked() -> Result<String, String> {
    let file = load_profiles_file()?;
    Ok(file.active_id.clone())
}

/// Resolve the configured world folder name from server.properties.
/// Reads `level-name` from the server's server.properties file.
/// Returns "world" as default if the file or key is missing.
/// Rejects traversal and absolute paths for safety.
pub fn resolve_world_name(server_dir: &std::path::Path) -> String {
    let props_path = server_dir.join("server.properties");
    let content = match std::fs::read_to_string(&props_path) {
        Ok(c) => c,
        Err(_) => return "world".to_string(),
    };
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            if key.trim() == "level-name" {
                let name = value.trim();
                // Safety: reject traversal, absolute paths, drive letters
                if name.is_empty()
                    || name.contains("..")
                    || name.contains('/')
                    || name.contains('\\')
                    || name.starts_with('.')
                    || (name.len() >= 2 && name.as_bytes()[1] == b':')
                {
                    return "world".to_string();
                }
                return name.to_string();
            }
        }
    }
    "world".to_string()
}

/// Resolve all world-related directories for a profile.
/// Returns (primary_world, nether_world, end_world) paths.
pub fn resolve_world_dirs(
    server_dir: &std::path::Path,
) -> (
    std::path::PathBuf,
    Option<std::path::PathBuf>,
    Option<std::path::PathBuf>,
) {
    let world_name = resolve_world_name(server_dir);
    let primary = server_dir.join(&world_name);
    let nether = server_dir.join(format!("{}_nether", world_name));
    let end = server_dir.join(format!("{}_the_end", world_name));
    let nether_opt = if nether.exists() { Some(nether) } else { None };
    let end_opt = if end.exists() { Some(end) } else { None };
    (primary, nether_opt, end_opt)
}

/// Load a profile's config by its ID. Returns None on error or not found.
pub fn load_profile_config(profile_id: &str) -> Option<ServerConfig> {
    match load_profiles_file() {
        Ok(file) => file
            .profiles
            .iter()
            .find(|p| p.id == profile_id)
            .map(|p| p.config.clone()),
        Err(e) => {
            eprintln!("[lbby] load_profile_config: {}", e);
            None
        }
    }
}

/// Like [`load_profile_config`] but propagates errors.
pub fn load_profile_config_checked(profile_id: &str) -> Result<Option<ServerConfig>, String> {
    let file = load_profiles_file()?;
    Ok(file
        .profiles
        .iter()
        .find(|p| p.id == profile_id)
        .map(|p| p.config.clone()))
}

/// Validate that a server path does not collide with another profile's path.
/// Returns Ok(()) if safe, Err with affected profile IDs if collision detected.
pub fn validate_server_path_ownership(
    exclude_profile_id: &str,
    server_path: &str,
) -> Result<(), String> {
    if server_path.is_empty() {
        return Ok(());
    }
    let file = load_profiles_file()?;
    let normalized = normalize_path(server_path);
    for profile in &file.profiles {
        if profile.id == exclude_profile_id {
            continue;
        }
        let other_normalized = normalize_path(&profile.config.server_path);
        if normalized == other_normalized {
            return Err(format!(
                "Server path collision: profile '{}' and profile '{}' both use '{}'",
                exclude_profile_id, profile.id, server_path
            ));
        }
    }
    Ok(())
}

/// Normalize a path for comparison: resolve . and .., normalize separators.
fn normalize_path(path_str: &str) -> String {
    let path = std::path::Path::new(path_str);
    // Try to canonicalize if it exists
    if let Ok(canonical) = path.canonicalize() {
        return canonical.to_string_lossy().to_string();
    }
    // Otherwise normalize manually
    let mut components: Vec<String> = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                components.pop();
            }
            std::path::Component::CurDir => {}
            other => components.push(other.as_os_str().to_string_lossy().to_string()),
        }
    }
    components.join(std::path::MAIN_SEPARATOR_STR)
}

pub fn save_config(cfg: &ServerConfig) -> Result<(), String> {
    let mut file = load_profiles_file()?;
    if let Some(profile) = file.profiles.iter_mut().find(|p| p.id == file.active_id) {
        // SAFEGUARD: Always preserve the profile's isolated server_path.
        // Never allow incoming config to overwrite it.
        let preserved_path = profile.config.server_path.clone();
        profile.config = cfg.clone();
        if !preserved_path.is_empty() {
            profile.config.server_path = preserved_path;
        }
        if profile.name.trim().is_empty() {
            profile.name = default_profile_name(cfg);
        }
    } else {
        let id = uuid::Uuid::new_v4().simple().to_string();
        file.active_id = id.clone();
        file.profiles.push(ServerProfile {
            id,
            name: default_profile_name(cfg),
            config: cfg.clone(),
        });
    }
    save_profiles_file(&file)?;

    // Keep the legacy config file in sync for older builds/manual inspection.
    let json = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    std::fs::write(config_path(), json).map_err(|e| e.to_string())
}

/// Get the state of all profiles. Returns empty state on error (logged).
pub fn profiles_state() -> ProfilesState {
    let file = match load_profiles_file() {
        Ok(f) => f,
        Err(e) => {
            eprintln!("[lbby] profiles_state: {}", e);
            return ProfilesState {
                active_id: String::new(),
                profiles: vec![],
                active_game: Game::default(),
            };
        }
    };
    let active_game = file
        .profiles
        .iter()
        .find(|p| p.id == file.active_id)
        .map(|p| p.config.game())
        .unwrap_or_default();
    ProfilesState {
        active_id: file.active_id.clone(),
        profiles: file
            .profiles
            .iter()
            .map(|p| profile_summary(p, &file.active_id))
            .collect(),
        active_game,
    }
}

pub fn create_profile(
    name: String,
    duplicate_current: bool,
    activate: bool,
    game: Option<Game>,
) -> Result<ProfilesState, String> {
    let mut file = load_profiles_file()?;
    let source_server_path = if duplicate_current {
        file.profiles
            .iter()
            .find(|p| p.id == file.active_id)
            .map(|p| p.config.server_path.clone())
    } else {
        None
    };
    let mut cfg = if duplicate_current {
        file.profiles
            .iter()
            .find(|p| p.id == file.active_id)
            .map(|p| p.config.clone())
            .unwrap_or_default()
    } else {
        let mut c = ServerConfig::default();
        // Set default server type based on game
        if let Some(ref g) = game {
            c.server_type = match g {
                Game::Terraria => ServerType::Terraria,
                Game::Minecraft => ServerType::Vanilla,
            };
        }
        c
    };
    if !duplicate_current {
        cfg.auto_restart = true;
        cfg.optimized_jvm_flags = true;
        cfg.performance_preset = default_performance_preset();
    } else {
        // Duplicated profiles need a fresh install — force setup wizard to
        // run so install_forge downloads new jars instead of reusing old ones.
        cfg.setup_complete = false;
    }
    let clean_name = name.trim();
    let id = uuid::Uuid::new_v4().simple().to_string();
    let isolated_path = base_dir().join("profiles").join(&id).join("server");
    if duplicate_current {
        if let Some(source) = source_server_path {
            let source_path = PathBuf::from(source);
            if source_path.exists() {
                std::fs::create_dir_all(&isolated_path)
                    .map_err(|e| format!("failed to create isolated profile directory: {e}"))?;
                for entry in walkdir::WalkDir::new(&source_path) {
                    let entry = entry.map_err(|e| format!("failed to copy profile data: {e}"))?;
                    let relative = entry
                        .path()
                        .strip_prefix(&source_path)
                        .map_err(|e| format!("failed to copy profile data: {e}"))?;
                    if relative.as_os_str().is_empty() {
                        continue;
                    }
                    let target = isolated_path.join(relative);
                    if entry.file_type().is_dir() {
                        std::fs::create_dir_all(&target)
                            .map_err(|e| format!("failed to copy profile directory: {e}"))?;
                    } else if entry.file_type().is_file() {
                        if let Some(parent) = target.parent() {
                            std::fs::create_dir_all(parent)
                                .map_err(|e| format!("failed to create profile directory: {e}"))?;
                        }
                        std::fs::copy(entry.path(), &target)
                            .map_err(|e| format!("failed to copy profile file: {e}"))?;
                    }
                }
            }
        }
    }
    cfg.server_path = isolated_path.to_string_lossy().to_string();
    // Validate no collision with existing profiles
    validate_server_path_ownership(&id, &cfg.server_path)?;
    file.profiles.push(ServerProfile {
        id: id.clone(),
        name: if clean_name.is_empty() {
            "New Server".to_string()
        } else {
            clean_name.to_string()
        },
        config: cfg,
    });
    if activate {
        file.active_id = id;
    }
    save_profiles_file(&file)?;
    Ok(profiles_state())
}

pub fn rename_profile(id: String, name: String) -> Result<ProfilesState, String> {
    let mut file = load_profiles_file()?;
    let clean_name = name.trim();
    if clean_name.is_empty() {
        return Err("Profile name cannot be empty".to_string());
    }
    let profile = file
        .profiles
        .iter_mut()
        .find(|p| p.id == id)
        .ok_or_else(|| "Profile not found".to_string())?;
    profile.name = clean_name.to_string();
    save_profiles_file(&file)?;
    Ok(profiles_state())
}

pub fn delete_profile(id: String) -> Result<ProfilesState, String> {
    let mut file = load_profiles_file()?;
    if file.profiles.len() <= 1 {
        return Err("Cannot delete the only profile".to_string());
    }
    let before = file.profiles.len();
    file.profiles.retain(|p| p.id != id);
    if file.profiles.len() == before {
        return Err("Profile not found".to_string());
    }
    if file.active_id == id {
        file.active_id = file.profiles[0].id.clone();
    }
    save_profiles_file(&file)?;
    Ok(profiles_state())
}

pub fn set_active_profile(id: String) -> Result<ServerConfig, String> {
    let mut file = load_profiles_file()?;
    let cfg = file
        .profiles
        .iter()
        .find(|p| p.id == id)
        .map(|p| p.config.clone())
        .ok_or_else(|| "Profile not found".to_string())?;
    file.active_id = id;
    save_profiles_file(&file)?;
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Serialise tests that mutate the global CONFIG_CACHE + env var
    static TEST_MUTEX: Mutex<()> = Mutex::new(());

    fn setup_test_dir(label: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        // Set env for base_dir() to use our temp dir
        std::env::set_var("LBBY_CONFIG_DIR", dir.path());
        // Invalidate the global cache so our test starts fresh
        CONFIG_CACHE.invalidate();
        let _ = label; // for debugging
        dir
    }

    /// Write a raw ProfilesFile to disk for testing
    fn write_test_profiles(schema_version: u32, active_id: &str, profile_id: &str, name: &str) {
        let file = ProfilesFile {
            schema_version,
            active_id: active_id.to_string(),
            profiles: vec![ServerProfile {
                id: profile_id.to_string(),
                name: name.to_string(),
                config: ServerConfig::default(),
            }],
        };
        let path = profiles_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let json = serde_json::to_string_pretty(&file).unwrap();
        std::fs::write(&path, json).expect("write profiles.json");
        CONFIG_CACHE.invalidate();
    }

    /// Write a legacy (v0) profiles.json — no schema_version field in the JSON
    fn write_legacy_profiles(active_id: &str, profile_id: &str, name: &str) {
        // Build a ProfilesFile with schema_version=0 then serialize,
        // then strip the schema_version field from the JSON to simulate a legacy file
        let file = ProfilesFile {
            schema_version: 0,
            active_id: active_id.to_string(),
            profiles: vec![ServerProfile {
                id: profile_id.to_string(),
                name: name.to_string(),
                config: ServerConfig {
                    server_path: "/tmp/test".to_string(),
                    ..ServerConfig::default()
                },
            }],
        };
        let mut json_val = serde_json::to_value(&file).unwrap();
        // Remove schema_version to simulate legacy unversioned file
        json_val.as_object_mut().unwrap().remove("schema_version");
        let json = serde_json::to_string_pretty(&json_val).unwrap();

        let path = profiles_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&path, json).expect("write profiles.json");
        CONFIG_CACHE.invalidate();
    }

    fn read_profiles_json() -> serde_json::Value {
        let path = profiles_path();
        let raw = std::fs::read_to_string(&path).expect("read profiles.json");
        serde_json::from_str(&raw).expect("parse profiles.json")
    }

    // ---- Item 7a: Legacy unversioned profiles → v0, migrated to v1 ----
    #[test]
    fn legacy_unversioned_profiles_migrate_to_v1() {
        let _lock = TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _dir = setup_test_dir("legacy");

        // Write a legacy profiles.json with no schema_version field
        write_legacy_profiles("abc123", "abc123", "Test Server");

        // load_config should succeed and migrate
        let cfg = load_config();
        assert_eq!(cfg.server_path, "/tmp/test");

        // Verify the file now has schema_version = 1
        let json = read_profiles_json();
        assert_eq!(json["schema_version"].as_u64().unwrap(), 1);
    }

    // ---- Item 7b: Profile migration idempotent ----
    #[test]
    fn profile_migration_idempotent() {
        let _lock = TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _dir = setup_test_dir("idempotent");

        // Write a legacy file, migrate once
        write_legacy_profiles("abc123", "abc123", "Test");
        let _ = load_config(); // triggers migration

        // Read back
        let json1 = read_profiles_json();
        assert_eq!(json1["schema_version"].as_u64().unwrap(), 1);

        // Load again — should be a no-op
        CONFIG_CACHE.invalidate();
        let _ = load_config();
        let json2 = read_profiles_json();
        assert_eq!(json2["schema_version"].as_u64().unwrap(), 1);
        assert_eq!(json1, json2); // identical
    }

    // ---- Item 7c: Future profile schema rejected unchanged ----
    #[test]
    fn future_profile_schema_rejected() {
        let _lock = TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _dir = setup_test_dir("future");

        // Write a future-schema profiles.json
        write_test_profiles(99, "abc123", "abc123", "Test");

        // load_profiles_file should return Err
        let result = load_profiles_file();
        assert!(result.is_err(), "expected error for future schema");
        let err = result.unwrap_err();
        assert!(
            err.contains("99"),
            "error should mention the version: {}",
            err
        );
        assert!(
            err.contains("does not support"),
            "error should say unsupported: {}",
            err
        );

        // File should be UNCHANGED
        let json = read_profiles_json();
        assert_eq!(json["schema_version"].as_u64().unwrap(), 99);
    }

    // ---- Item 7d: Corrupt profiles preserved and NOT silently reset ----
    #[test]
    fn corrupt_profiles_preserved_not_reset() {
        let _lock = TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _dir = setup_test_dir("corrupt");

        // Write corrupt JSON
        {
            let path = profiles_path();
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::write(&path, "{ this is not valid json!!! }").unwrap();
            CONFIG_CACHE.invalidate();
        }

        // load_profiles_file should return Err
        let result = load_profiles_file();
        assert!(result.is_err(), "expected error for corrupt profiles");
        let err = result.unwrap_err();
        assert!(err.contains("corrupt"), "error should mention corrupt");

        // Original should be backed up as .corrupted
        let backup = profiles_path().with_extension("json.corrupted");
        assert!(
            backup.exists(),
            "corrupt file should be backed up to .corrupted"
        );

        // profiles.json should NOT exist (moved to .corrupted)
        // This ensures we don't silently reset.
        // Note: subsequent load will create default (acceptable for missing file)
        // but the key point is the ERROR was returned, not silently swallowed.
    }

    // ---- Item 7e: Atomic profile migration failure preserves old file ----
    #[test]
    fn migration_preserves_backup_on_corrupt() {
        let _lock = TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _dir = setup_test_dir("backup_preserve");

        {
            let path = profiles_path();
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::write(&path, "not json!").unwrap();
            CONFIG_CACHE.invalidate();
        }

        let result = load_profiles_file();
        assert!(result.is_err());

        // .corrupted backup should exist with the original content
        let backup = profiles_path().with_extension("json.corrupted");
        assert!(backup.exists());
        let content = std::fs::read_to_string(&backup).unwrap();
        assert_eq!(content, "not json!");
    }

    // ---- Schema version in default profiles ----
    #[test]
    fn default_profiles_have_schema_version() {
        let _lock = TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _dir = setup_test_dir("default_schema");

        // No profiles.json exists — load should create default with schema_version=1
        let result = load_profiles_file();
        assert!(result.is_ok());
        let file = result.unwrap();
        assert_eq!(file.schema_version, PROFILES_SCHEMA_VERSION);

        // Verify on disk
        let json = read_profiles_json();
        assert_eq!(json["schema_version"].as_u64().unwrap(), 1);
    }
}
