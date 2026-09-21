// Shared application state and event sender — used by both agent and app.
// The agent wraps this in axum::State, the app wraps it in Tauri managed state.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, Mutex};

use crate::cloudflare::CloudflareTunnelState;
use crate::playit::PlayitState;
use crate::server::{ServerManager, ServerStatus};
use crate::stats::ServerStats;

const CONSOLE_BUFFER_CAP: usize = 2000;
const RECENT_PLAYERS_CAP: usize = 50;

/// Tracks which safety-critical operation is currently in progress.
/// Used to prevent conflicting operations from running concurrently.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OperationKind {
    None,
    Starting,
    Stopping,
    Recovering,
    Restoring,
    Importing,
    Exporting,
    Installing,
    BackingUp,
    Resetting,
    DeletingProfile,
    ModMutation,
    PluginMutation,
}

impl Default for OperationKind {
    fn default() -> Self {
        OperationKind::None
    }
}

/// RAII guard for safety-critical operations. Acquisition fails immediately
/// when another operation owns the coordinator; dropping the guard always
/// resets the state, including early-return and error paths.
pub struct OperationGuard<'a> {
    guard: tokio::sync::MutexGuard<'a, OperationKind>,
}

impl<'a> OperationGuard<'a> {
    pub async fn acquire(
        operation: &'a Mutex<OperationKind>,
        kind: OperationKind,
    ) -> Result<Self, String> {
        let mut guard = operation
            .try_lock()
            .map_err(|_| "Another operation is already in progress".to_string())?;
        if *guard != OperationKind::None {
            return Err(format!(
                "Another operation is already in progress: {guard:?}"
            ));
        }
        *guard = kind;
        Ok(Self { guard })
    }
}

impl Drop for OperationGuard<'_> {
    fn drop(&mut self) {
        *self.guard = OperationKind::None;
    }
}

#[derive(Default, Clone, Serialize)]
pub struct PregenState {
    pub running: bool,
    pub total: u32,
    pub completed: u32,
    pub cancel_requested: bool,
}

#[derive(Default, Clone, Serialize)]
pub struct ShutdownStatus {
    pub server_running: bool,
    pub playit_running: bool,
    pub cloudflare_running: bool,
    pub remote_control_running: bool,
    pub any_running: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BannedPlayer {
    pub name: String,
    pub uuid: String,
    pub created: String,
    pub source: String,
    pub expires: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WhitelistEntry {
    pub name: String,
    pub uuid: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BannedIp {
    pub ip: String,
    pub name: String,
    pub created: String,
    pub source: String,
    pub expires: String,
    pub reason: String,
}

/// Provider that originally delivered this mod artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModProvider {
    Modrinth,
    CurseForge,
    Manual,
    Unknown,
}

impl Default for ModProvider {
    fn default() -> Self {
        ModProvider::Unknown
    }
}

/// Loader identified from the mod JAR's own metadata.
/// Mirrors jar_metadata::LoaderMetadataKind but adds Unknown/MultiLoader.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DetectedLoader {
    Fabric,
    Quilt,
    Forge,
    NeoForge,
    MultiLoader,
    Unknown,
}

impl Default for DetectedLoader {
    fn default() -> Self {
        DetectedLoader::Unknown
    }
}

/// Readability status of the mod artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModStatus {
    Readable,
    Unreadable,
}

impl Default for ModStatus {
    fn default() -> Self {
        ModStatus::Readable
    }
}

// ── 4B.3C: Update & removal types ────────────────────────────────────────

/// Status of an update check for a single mod.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum UpdateStatus {
    /// Provider receipt confirmed, no newer compatible version found.
    UpToDate,
    /// A newer compatible version is available from the trusted provider.
    UpdateAvailable,
    /// No provider receipt; identity could not be resolved to a provider.
    ProviderUnknown,
    /// Provider receipt exists but the provider API call failed.
    ProviderUnavailable,
    /// Provider queried successfully but no compatible update exists.
    NoCompatibleUpdate,
    /// Artifact metadata unreadable; cannot determine update status.
    Unreadable,
    /// A conflict was detected (e.g., loader mismatch, version constraint).
    Conflict,
}

impl Default for UpdateStatus {
    fn default() -> Self {
        UpdateStatus::UpToDate
    }
}

/// Outcome of a single-mod update attempt within update-all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum UpdateOutcome {
    Updated,
    /// Artifact updated but receipt metadata could not be persisted.
    UpdatedUntracked,
    Skipped,
    Failed,
    UpToDate,
    Conflict,
    ProviderUnavailable,
}

impl Default for UpdateOutcome {
    fn default() -> Self {
        UpdateOutcome::UpToDate
    }
}

/// Per-item result from update_all_mods.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateItemResult {
    pub file_name: String,
    pub display_name: String,
    pub outcome: UpdateOutcome,
    pub detail: String,
}

/// Structured result from update_all_mods.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateAllResult {
    pub mods: Vec<ModInfo>,
    pub results: Vec<UpdateItemResult>,
    pub warning: Option<String>,
}

/// A mod that depends on a target mod (used for removal conflict display).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependentInfo {
    pub file_name: String,
    pub display_name: String,
    pub mod_id: String,
    pub kind: String, // "required"
}

/// Result of a remove_mod attempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoveResult {
    pub success: bool,
    pub dependents: Vec<DependentInfo>,
    pub warning: Option<String>,
}

/// Server compatibility classification.
/// Re-exports mod_compat::ServerCompatibility for the ModInfo model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModCompatibility {
    ServerOk,
    ClientOnly,
    Both,
    Unknown,
}

impl Default for ModCompatibility {
    fn default() -> Self {
        ModCompatibility::Unknown
    }
}

/// How confident the compatibility classification is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModCompatConfidence {
    Explicit,
    None,
}

impl Default for ModCompatConfidence {
    fn default() -> Self {
        ModCompatConfidence::None
    }
}

/// Source of the compatibility determination.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModCompatSource {
    FabricMetadata,
    QuiltMetadata,
    ForgeMetadata,
    NeoForgeMetadata,
    ConflictingMetadata,
    None,
}

impl Default for ModCompatSource {
    fn default() -> Self {
        ModCompatSource::None
    }
}

/// Normalized dependency info for inventory display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InventoryDependency {
    pub mod_id: String,
    pub kind: String,
    pub version_requirement: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModInfo {
    pub file_name: String,
    pub display_name: String,
    pub version: String,
    pub authors: Vec<String>,
    pub description: String,
    pub icon_data_url: Option<String>,

    // ── 4B.3B identity fields ──────────────────────────────────────────
    /// Opaque stable identity within this profile inventory.
    /// Backend-generated. Not a filesystem path.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub inventory_id: String,

    /// Declared mod ID from loader metadata (fabric.mod.json id, mods.toml modId).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mod_id: Option<String>,

    /// Loader detected from the JAR's own metadata.
    #[serde(default)]
    pub loader: DetectedLoader,

    /// Provider that delivered this artifact.
    #[serde(default)]
    pub provider: ModProvider,

    /// Provider project ID (e.g. Modrinth project slug/ID, CurseForge mod ID).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,

    /// Provider file/version ID for update tracking.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_version_id: Option<String>,

    /// SHA-512 hash of the installed artifact, if computed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,

    /// Server compatibility classification.
    #[serde(default)]
    pub compatibility: ModCompatibility,

    /// Confidence in the compatibility classification.
    #[serde(default)]
    pub compatibility_confidence: ModCompatConfidence,

    /// Source metadata that produced the compatibility result.
    #[serde(default)]
    pub compatibility_source: ModCompatSource,

    /// Human-readable reason for the compatibility classification.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub compatibility_reason: String,

    /// Normalized dependency metadata from loader manifest.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependency_metadata: Vec<InventoryDependency>,

    /// Whether the JAR was readable.
    #[serde(default)]
    pub status: ModStatus,
}

impl Default for ModInfo {
    fn default() -> Self {
        Self {
            file_name: String::new(),
            display_name: String::new(),
            version: String::new(),
            authors: Vec::new(),
            description: String::new(),
            icon_data_url: None,
            inventory_id: String::new(),
            mod_id: None,
            loader: DetectedLoader::Unknown,
            provider: ModProvider::Unknown,
            project_id: None,
            file_version_id: None,
            hash: None,
            compatibility: ModCompatibility::Unknown,
            compatibility_confidence: ModCompatConfidence::None,
            compatibility_source: ModCompatSource::None,
            compatibility_reason: String::new(),
            dependency_metadata: Vec::new(),
            status: ModStatus::Readable,
        }
    }
}

/// Result of a mod install operation.
///
/// `InstalledTracked`  = success + receipt persisted  → `warning == None`
/// `InstalledUntracked` = success + receipt failure   → `warning == Some(msg)`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallResult {
    pub mods: Vec<ModInfo>,
    /// `None` = fully tracked. `Some(msg)` = installed but provider metadata
    /// could not be saved.  Frontend should show the warning text to the user.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionResult {
    /// Action ID — serialized as "actionId" for the dashboard API.
    #[serde(rename = "actionId")]
    pub action_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ── 4B.4A: Plugin types ──────────────────────────────────────────────────

/// Plugin platform — NOT reusing ServerType for classification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum PluginPlatform {
    Bukkit,
    Spigot,
    Paper,
    Purpur,
    Folia,
    Velocity,
    Waterfall,
    BungeeCord,
    Unknown,
}

impl Default for PluginPlatform {
    fn default() -> Self {
        PluginPlatform::Unknown
    }
}

/// Plugin status — NOT reusing mod ServerCompatibility.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum PluginStatus {
    Readable,
    Unreadable,
    UnknownMetadata,
}

impl Default for PluginStatus {
    fn default() -> Self {
        PluginStatus::UnknownMetadata
    }
}

/// Plugin compatibility with current server.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum PluginCompatibility {
    Compatible,
    PlatformMismatch,
    VersionUnknown,
    FoliaUnknown,
    FoliaCompatible,
    FoliaIncompatible,
    ProxyPlugin,
    /// Candidate is for a proxy server (Velocity/Bungee) but current server is not proxy.
    ProxyMismatch,
    /// Provider MC version does not match the profile MC version.
    MinecraftVersionMismatch,
    /// Provider identity conflicts with an already-installed plugin.
    IdentityConflict,
    Unknown,
    Unreadable,
}

impl Default for PluginCompatibility {
    fn default() -> Self {
        PluginCompatibility::Unknown
    }
}

/// Plugin provider.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum PluginProvider {
    Modrinth,
    Hangar,
    SpigotMC,
    CurseForge,
    Manual,
    Unknown,
}

impl Default for PluginProvider {
    fn default() -> Self {
        PluginProvider::Unknown
    }
}

/// Plugin dependency.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginDependency {
    pub name: String,
    /// true = hard (depend), false = soft (softdepend)
    pub required: bool,
    /// from loadbefore field
    pub load_before: bool,
}

/// Plugin inventory entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginInfo {
    pub inventory_id: String,
    pub file_name: String,
    pub display_name: Option<String>,
    pub plugin_name: Option<String>,
    pub version: Option<String>,
    pub main_class: Option<String>,
    pub authors: Vec<String>,
    pub description: Option<String>,
    pub website: Option<String>,
    pub api_version: Option<String>,
    pub platforms: Vec<PluginPlatform>,
    pub provider: Option<PluginProvider>,
    pub project_id: Option<String>,
    pub file_version_id: Option<String>,
    pub artifact_hash: Option<String>,
    pub status: PluginStatus,
    pub dependencies: Vec<PluginDependency>,
    pub folia_supported: Option<bool>,
}

impl Default for PluginInfo {
    fn default() -> Self {
        Self {
            inventory_id: String::new(),
            file_name: String::new(),
            display_name: None,
            plugin_name: None,
            version: None,
            main_class: None,
            authors: Vec::new(),
            description: None,
            website: None,
            api_version: None,
            platforms: Vec::new(),
            provider: None,
            project_id: None,
            file_version_id: None,
            artifact_hash: None,
            status: PluginStatus::UnknownMetadata,
            dependencies: Vec::new(),
            folia_supported: None,
        }
    }
}

/// Plugin receipt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginReceipt {
    pub schema_version: u32,
    pub provider: PluginProvider,
    pub project_id: String,
    pub file_version_id: Option<String>,
    pub filename: String,
    pub artifact_hash: String,
    pub platforms: Vec<PluginPlatform>,
    pub mc_version: Option<String>,
}

/// Plugin compatibility result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginCompatResult {
    pub compatible: PluginCompatibility,
    pub source: String,
    pub reason: String,
}

// ── 4B.4B: Provider-aware plugin types ────────────────────────────────

/// Release channel priority for candidate selection.
/// Deterministic ordering: Release > Beta > Alpha.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ReleaseChannel {
    Release,
    Beta,
    Alpha,
}

impl Default for ReleaseChannel {
    fn default() -> Self {
        ReleaseChannel::Release
    }
}

/// Normalized plugin candidate from any provider.
/// Uses Option for fields that not all providers supply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginCandidate {
    pub provider: PluginProvider,
    pub project_id: String,
    pub file_version_id: Option<String>,
    pub title: String,
    pub description: Option<String>,
    pub authors: Vec<String>,
    pub download_url: String,
    pub filename: String,
    pub hashes: PluginCandidateHashes,
    pub game_versions: Vec<String>,
    pub platforms: Vec<PluginPlatform>,
    pub release_channel: ReleaseChannel,
    pub published_at: Option<String>,
    pub icon_url: Option<String>,
    /// Pre-computed compatibility with the current server config.
    /// None when not yet computed (e.g. direct construction in tests).
    #[serde(default)]
    pub compatibility: Option<PluginCompatibility>,
}

/// Hashes from a provider. Strongest documented hash first.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct PluginCandidateHashes {
    pub sha512: Option<String>,
    pub sha256: Option<String>,
    pub sha1: Option<String>,
}

/// Structured install result from provider install.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PluginInstallResult {
    Installed(PluginInfo),
    AlreadyInstalled,
    Conflict(String),
    Incompatible(PluginCompatResult),
    InstalledUntracked(PluginInfo),
    ProviderUnavailable(String),
}

/// Provider capability flags — what a provider actually supports.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginProviderCapabilities {
    pub provider: PluginProvider,
    pub search: bool,
    pub project_lookup: bool,
    pub version_resolution: bool,
    pub direct_download: bool,
    pub hash_verification: bool,
    pub platform_filtering: bool,
    pub mc_version_filtering: bool,
}

/// Search result from a plugin provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginSearchResult {
    pub candidates: Vec<PluginCandidate>,
    pub provider: PluginProvider,
    pub has_more: bool,
    pub total_hits: Option<u32>,
}

pub struct AppState {
    pub server: Mutex<ServerManager>,
    pub playit: Mutex<PlayitState>,
    pub stats: Mutex<ServerStats>,
    pub online_players: Mutex<HashSet<String>>,
    pub console_buffer: StdMutex<VecDeque<String>>,
    pub recent_auto_restarts: Mutex<VecDeque<std::time::Instant>>,
    pub recent_players: Mutex<VecDeque<(String, String)>>,
    pub last_gametime_sample: Mutex<Option<(u64, std::time::Instant)>>,
    pub pregen: Mutex<PregenState>,
    pub config_write_lock: Mutex<()>,
    pub remote_control: Mutex<Option<tokio::task::JoinHandle<()>>>,
    pub remote_control_active_token: Mutex<String>,
    pub cloudflare_remote: Mutex<CloudflareTunnelState>,
    pub action_results: Mutex<Vec<ActionResult>>,
    pub current_operation: Mutex<OperationKind>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            server: Mutex::new(ServerManager::new()),
            playit: Mutex::new(PlayitState::default()),
            stats: Mutex::new(ServerStats::default()),
            online_players: Mutex::new(HashSet::new()),
            console_buffer: StdMutex::new(VecDeque::with_capacity(CONSOLE_BUFFER_CAP)),
            recent_auto_restarts: Mutex::new(VecDeque::new()),
            recent_players: Mutex::new(VecDeque::new()),
            last_gametime_sample: Mutex::new(None),
            pregen: Mutex::new(PregenState::default()),
            config_write_lock: Mutex::new(()),
            remote_control: Mutex::new(None),
            remote_control_active_token: Mutex::new(String::new()),
            cloudflare_remote: Mutex::new(CloudflareTunnelState::default()),
            action_results: Mutex::new(Vec::new()),
            current_operation: Mutex::new(OperationKind::None),
        }
    }

    pub fn push_console_line(&self, line: String) {
        // We use try_lock to avoid blocking the hot path if someone
        // else is reading the buffer (e.g. a console snapshot request).
        if let Ok(mut buf) = self.console_buffer.try_lock() {
            if buf.len() >= CONSOLE_BUFFER_CAP {
                buf.pop_front();
            }
            buf.push_back(line);
        }
    }

    pub fn record_player_join(&self, name: String) {
        if let Ok(mut recent) = self.recent_players.try_lock() {
            let ts = chrono::Utc::now().to_rfc3339();
            recent.push_front((name, ts));
            while recent.len() > RECENT_PLAYERS_CAP {
                recent.pop_back();
            }
        }
    }

    /// Acquire a mod-mutation guard. Rejects if:
    ///   - Server is not Stopped or Error
    ///   - Another operation is already in progress (Installing, Restoring, etc.)
    ///   - Another mod mutation is already held
    /// Closes TOCTOU by re-verifying server status after acquiring the guard.
    pub async fn require_mod_mutation_ready(&self) -> Result<OperationGuard<'_>, String> {
        // 1. Fast check: server must be stopped or in error
        {
            let srv = self.server.lock().await;
            if !matches!(srv.status, ServerStatus::Stopped | ServerStatus::Error) {
                return Err(format!(
                    "Cannot modify mods while server is {:?}. Stop the server first.",
                    srv.status
                ));
            }
        }
        // 2. Acquire exclusive mutation slot
        let guard =
            OperationGuard::acquire(&self.current_operation, OperationKind::ModMutation).await?;
        // 3. Re-verify server status under guard (closes check-then-act race)
        {
            let srv = self.server.lock().await;
            if !matches!(srv.status, ServerStatus::Stopped | ServerStatus::Error) {
                return Err(format!(
                    "Server state changed during mod operation setup: {:?}",
                    srv.status
                ));
            }
        }
        Ok(guard)
    }
    /// Acquire a plugin-mutation guard. Same server-status and overlap checks as mod mutation.
    /// ModMutation and PluginMutation conflict (both change server-owned content).
    pub async fn require_plugin_mutation_ready(&self) -> Result<OperationGuard<'_>, String> {
        {
            let srv = self.server.lock().await;
            if !matches!(srv.status, ServerStatus::Stopped | ServerStatus::Error) {
                return Err(format!(
                    "Cannot modify plugins while server is {:?}. Stop the server first.",
                    srv.status
                ));
            }
        }
        let guard =
            OperationGuard::acquire(&self.current_operation, OperationKind::PluginMutation).await?;
        {
            let srv = self.server.lock().await;
            if !matches!(srv.status, ServerStatus::Stopped | ServerStatus::Error) {
                return Err(format!(
                    "Server state changed during plugin operation setup: {:?}",
                    srv.status
                ));
            }
        }
        Ok(guard)
    }
}

/// Event sender — emits events via a broadcast channel.
/// Used by modules to notify the UI (Tauri) or agent event loop.
pub struct AppEventSender {
    pub state: Arc<AppState>,
    pub tx: broadcast::Sender<serde_json::Value>,
}

impl AppEventSender {
    pub fn new(state: Arc<AppState>) -> Self {
        let (tx, _) = broadcast::channel(256);
        Self { state, tx }
    }

    pub fn emit<S: Serialize>(&self, event: &str, payload: S) -> Result<(), ()> {
        let val = serde_json::json!({ "event": event, "payload": payload });
        let _ = self.tx.send(val);
        Ok(())
    }

    pub fn state(&self) -> Arc<AppState> {
        self.state.clone()
    }
}

// ── 4B.4C: Update, Remove, Runtime types ────────────────────────────────

/// Update status for an installed plugin.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum PluginUpdateStatus {
    /// A compatible newer version is available.
    UpdateAvailable {
        current_version: Option<String>,
        target_version: Option<String>,
        target_filename: String,
        /// Direct download URL for the target artifact.
        download_url: String,
        /// Provider cryptographic hashes for verification.
        hashes: PluginCandidateHashes,
        /// Supported platforms from the candidate.
        platforms: Vec<PluginPlatform>,
        /// Supported MC versions from the candidate.
        game_versions: Vec<String>,
        /// Release channel label (release/beta/alpha).
        release_channel: Option<String>,
    },
    /// The installed version is the latest compatible.
    UpToDate,
    /// Provider request failed (network error, rate limit, etc.).
    ProviderUnavailable(String),
    /// Provider does not support trusted update resolution (SpigotMC, Manual, etc.).
    ProviderUnknown,
    /// Provider works but has no compatible version for current MC/platform.
    NoCompatibleUpdate,
    /// Cannot determine compatibility (missing metadata).
    CompatibilityUnknown,
    /// Identity conflict with another installed plugin.
    Conflict(String),
    /// Plugin JAR is unreadable / no valid descriptors.
    Unreadable,
}

impl PluginUpdateStatus {
    pub fn clone_target_version(&self) -> Option<String> {
        match self {
            Self::UpdateAvailable { target_version, .. } => target_version.clone(),
            _ => None,
        }
    }
    pub fn clone_target_filename(&self) -> Option<String> {
        match self {
            Self::UpdateAvailable {
                target_filename, ..
            } => Some(target_filename.clone()),
            _ => None,
        }
    }
}

/// Per-item outcome of a plugin update operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PluginUpdateOutcome {
    /// Successfully updated; receipt updated.
    Updated(PluginInfo),
    /// Artifact updated but receipt persistence failed.
    UpdatedUntracked(PluginInfo),
    /// Skipped (already up-to-date).
    Skipped,
    /// Update failed; old artifact preserved.
    Failed(String),
    /// No compatible update available.
    UpToDate,
    /// Identity conflict detected during update.
    Conflict(String),
    /// Provider unavailable.
    ProviderUnavailable(String),
    /// Candidate incompatible with current server.
    Incompatible(PluginCompatResult),
}

/// Per-item outcome of a plugin removal operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PluginRemoveOutcome {
    /// Successfully removed; receipt cleaned up.
    Removed,
    /// Artifact removed but receipt cleanup failed.
    RemovedUntracked,
    /// Removal blocked by hard dependents.
    BlockedByDependents { dependents: Vec<String> },
    /// Identity conflict (duplicate installed).
    Conflict(String),
    /// Plugin not found in inventory.
    NotFound,
}

/// Batch update result preserving per-item outcomes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginBatchUpdateResult {
    pub items: Vec<(String, PluginUpdateOutcome)>,
    pub updated_count: u32,
    pub failed_count: u32,
    pub skipped_count: u32,
}

/// Runtime compatibility of an installed plugin against current server config.
/// Separate from descriptor metadata — reflects runtime interpretation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum PluginRuntimeCompatibility {
    Compatible,
    PlatformMismatch,
    FoliaUnknown,
    FoliaCompatible,
    FoliaIncompatible,
    ProxyPlugin,
    ProxyMismatch,
    MinecraftVersionMismatch,
    Unknown,
    Unreadable,
    Ambiguous,
}

/// Update check result for a single installed plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginUpdateCheckResult {
    pub inventory_id: String,
    pub plugin_name: Option<String>,
    pub current_version: Option<String>,
    pub status: PluginUpdateStatus,
    pub provider: PluginProvider,
}

/// Dependency graph node for an installed plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginDependencyNode {
    pub inventory_id: String,
    pub plugin_name: Option<String>,
    pub hard_dependencies: Vec<String>,
    pub soft_dependencies: Vec<String>,
    pub load_before: Vec<String>,
}

/// Full dependency graph for installed plugins.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginDependencyGraph {
    pub nodes: Vec<PluginDependencyNode>,
    pub conflicts: Vec<String>,
}

/// Installed plugin info enriched with runtime compatibility.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginInventoryEntry {
    pub info: PluginInfo,
    pub runtime_compat: PluginRuntimeCompatibility,
    pub update_status: Option<PluginUpdateStatus>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn operation_guard_rejects_overlap_and_resets_on_drop() {
        let operation = Mutex::new(OperationKind::None);
        let guard = OperationGuard::acquire(&operation, OperationKind::Starting)
            .await
            .unwrap();
        assert!(
            OperationGuard::acquire(&operation, OperationKind::BackingUp)
                .await
                .is_err()
        );
        drop(guard);
        assert!(
            OperationGuard::acquire(&operation, OperationKind::BackingUp)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn mod_mutation_conflicts_with_installing() {
        let operation = Mutex::new(OperationKind::None);
        let _guard = OperationGuard::acquire(&operation, OperationKind::Installing)
            .await
            .unwrap();
        assert!(
            OperationGuard::acquire(&operation, OperationKind::ModMutation)
                .await
                .is_err(),
            "ModMutation must be rejected while Installing is held"
        );
    }

    #[tokio::test]
    async fn installing_conflicts_with_mod_mutation() {
        let operation = Mutex::new(OperationKind::None);
        let _guard = OperationGuard::acquire(&operation, OperationKind::ModMutation)
            .await
            .unwrap();
        assert!(
            OperationGuard::acquire(&operation, OperationKind::Installing)
                .await
                .is_err(),
            "Installing must be rejected while ModMutation is held"
        );
    }

    #[tokio::test]
    async fn mod_mutation_conflicts_with_restoring() {
        let operation = Mutex::new(OperationKind::None);
        let _guard = OperationGuard::acquire(&operation, OperationKind::Restoring)
            .await
            .unwrap();
        assert!(
            OperationGuard::acquire(&operation, OperationKind::ModMutation)
                .await
                .is_err(),
            "ModMutation must be rejected while Restoring is held"
        );
    }

    #[tokio::test]
    async fn mod_mutation_allows_after_drop() {
        let operation = Mutex::new(OperationKind::None);
        let guard = OperationGuard::acquire(&operation, OperationKind::ModMutation)
            .await
            .unwrap();
        assert!(
            OperationGuard::acquire(&operation, OperationKind::ModMutation)
                .await
                .is_err()
        );
        drop(guard);
        assert!(
            OperationGuard::acquire(&operation, OperationKind::ModMutation)
                .await
                .is_ok(),
            "ModMutation must be acquirable after previous guard is dropped"
        );
    }

    #[tokio::test]
    async fn plugin_mutation_rejected_while_running() {
        let state = AppState::new();
        {
            let mut srv = state.server.lock().await;
            srv.set_status(ServerStatus::Running, None);
        }
        let result = state.require_plugin_mutation_ready().await;
        match result {
            Ok(_) => panic!("require_plugin_mutation_ready must reject while server is Running"),
            Err(e) => {
                let msg = e.to_lowercase();
                assert!(
                    msg.contains("running"),
                    "Error must mention the Running state, got: {msg}"
                );
            }
        }
    }
}
