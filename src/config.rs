use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
    /// Forge / NeoForge / Fabric loader version, or Paper build number as string
    pub loader_version: Option<String>,
    pub ram_mb: u32,
    pub max_players: u32,
    pub server_name: String,
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
    /// Optional user-provided CurseForge API key. Required by CurseForge APIs.
    #[serde(default)]
    pub curseforge_api_key: String,
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
    active_id: String,
    profiles: Vec<ServerProfile>,
}

pub fn generate_remote_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn base_dir() -> PathBuf {
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
        active_id: id.clone(),
        profiles: vec![ServerProfile {
            id,
            name: default_profile_name(&cfg),
            config: cfg,
        }],
    }
}

fn load_profiles_file() -> ProfilesFile {
    let path = profiles_path();
    if let Some(raw) = std::fs::read_to_string(&path).ok() {
        if let Some(mut file) = serde_json::from_str::<ProfilesFile>(&raw).ok() {
            file.profiles.retain(|p| !p.id.trim().is_empty());
            if file.profiles.is_empty() {
                file = default_profiles_file();
            }
            if !file.profiles.iter().any(|p| p.id == file.active_id) {
                file.active_id = file.profiles[0].id.clone();
            }
            let _ = save_profiles_file(&file);
            return file;
        }
        // File exists but is corrupted — back it up before overwriting so
        // the user can manually recover their data.
        let backup = path.with_extension("json.corrupted");
        eprintln!(
            "[lbby] profiles.json is corrupted — backing up to {}",
            backup.display()
        );
        let _ = std::fs::rename(&path, &backup);
    }
    let file = default_profiles_file();
    let _ = save_profiles_file(&file);
    file
}

fn save_profiles_file(file: &ProfilesFile) -> Result<(), String> {
    let path = profiles_path();
    let json = serde_json::to_string_pretty(file).map_err(|e| e.to_string())?;
    // Atomic write: write to a temp file first, then rename. This prevents
    // corruption if the process crashes or loses power mid-write.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &json).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

fn profile_summary(profile: &ServerProfile, active_id: &str) -> ProfileSummary {
    ProfileSummary {
        id: profile.id.clone(),
        name: profile.name.clone(),
        active: profile.id == active_id,
        minecraft_version: profile.config.minecraft_version.clone(),
        server_type: profile.config.server_type.clone(),
        server_path: profile.config.server_path.clone(),
        setup_complete: profile.config.setup_complete,
        game: profile.config.game(),
    }
}

pub fn load_config() -> ServerConfig {
    let file = load_profiles_file();
    file.profiles
        .iter()
        .find(|p| p.id == file.active_id)
        .map(|p| p.config.clone())
        .unwrap_or_default()
}

pub fn save_config(cfg: &ServerConfig) -> Result<(), String> {
    let mut file = load_profiles_file();
    if let Some(profile) = file.profiles.iter_mut().find(|p| p.id == file.active_id) {
        profile.config = cfg.clone();
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

pub fn profiles_state() -> ProfilesState {
    let file = load_profiles_file();
    let active_game = file
        .profiles
        .iter()
        .find(|p| p.id == file.active_id)
        .map(|p| p.config.game())
        .unwrap_or_default();
    ProfilesState {
        active_id: file.active_id.clone(),
        profiles: file.profiles.iter().map(|p| profile_summary(p, &file.active_id)).collect(),
        active_game,
    }
}

pub fn create_profile(name: String, duplicate_current: bool, activate: bool, game: Option<Game>) -> Result<ProfilesState, String> {
    let mut file = load_profiles_file();
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
    }
    let clean_name = name.trim();
    // Give fresh profiles a unique server directory so each profile's
    // mods/plugins stay isolated.  Duplicated profiles keep the source path.
    if !duplicate_current {
        let display_name = if clean_name.is_empty() { "server" } else { clean_name };
        let slug: String = display_name
            .to_lowercase()
            .replace(' ', "-")
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '-')
            .collect();
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        // Collect paths already used by other profiles so we never collide
        let used: std::collections::HashSet<String> = file
            .profiles
            .iter()
            .map(|p| p.config.server_path.clone())
            .collect();
        let game_prefix = cfg.game().display_name().to_lowercase();
        let mut path = home.join(format!("{}-{}", game_prefix, slug));
        let mut n = 2u32;
        while path.exists() || used.contains(&path.to_string_lossy().to_string()) {
            path = home.join(format!("{}-{}-{}", game_prefix, slug, n));
            n += 1;
        }
        cfg.server_path = path.to_string_lossy().to_string();
    }
    let id = uuid::Uuid::new_v4().simple().to_string();
    file.profiles.push(ServerProfile {
        id: id.clone(),
        name: if clean_name.is_empty() { "New Server".to_string() } else { clean_name.to_string() },
        config: cfg,
    });
    if activate {
        file.active_id = id;
    }
    save_profiles_file(&file)?;
    Ok(profiles_state())
}

pub fn rename_profile(id: String, name: String) -> Result<ProfilesState, String> {
    let mut file = load_profiles_file();
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
    let mut file = load_profiles_file();
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
    let mut file = load_profiles_file();
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
