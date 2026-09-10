// lbby-core — Shared library for Lbby agent and app.
// Contains all game server management logic, without any UI or web server dependencies.

pub mod app_state;
pub mod automodpack;
pub mod backup;
pub mod boot_failure_analyzer;
pub mod boot_validator;
pub mod cloudflare;
pub mod config;
pub mod crash_attribution;
pub mod debug_report;
pub mod dependency_graph;
pub mod dependency_resolver;
pub mod errors;
pub mod file_cache;
pub mod forge;
pub mod heartbeat;
pub mod helpers;
pub mod install_transaction;
pub mod jar_metadata;
pub mod java;
pub mod license;
pub mod loader_compat_advisor;
pub mod minecraft_properties;
pub mod mod_compat;
pub mod mod_services;
pub mod mod_side;
pub mod modpack_discovery;
pub mod node_api;
pub mod player_actions;
#[cfg(feature = "sqlite")]
pub mod player_stats;
pub mod playit;
pub mod remote;
pub mod runtime_remediator;
pub mod server;
pub mod server_launch;
pub mod stats;
pub mod steamcmd;
pub mod terraria_config;
pub mod tmod_services;
pub mod validation_orchestrator;
pub mod version_fetch;

// Re-export commonly used types for convenience
pub use app_state::{
    ActionResult, AppEventSender, AppState, BannedIp, BannedPlayer, ModInfo, OperationKind,
    PregenState, ShutdownStatus, WhitelistEntry,
};
pub use config::{Game, ServerConfig, ServerType};
pub use errors::SafetyError;
pub use helpers::remote_kill_server_and_playit;
pub use playit::PlayitState;
pub use server::{ServerManager, ServerStatus};
pub use stats::ServerStats;
