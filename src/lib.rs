// lbby-core — Shared library for Lbby agent and app.
// Contains all game server management logic, without any UI or web server dependencies.

pub mod app_state;
pub mod helpers;
pub mod automodpack;
pub mod backup;
pub mod cloudflare;
pub mod config;
pub mod debug_report;
pub mod java;
pub mod license;
pub mod mod_services;
pub mod player_actions;
pub mod player_stats;
pub mod playit;
pub mod remote;
pub mod server;
pub mod stats;
pub mod steamcmd;
pub mod terraria_config;
pub mod tmod_services;

// Re-export commonly used types for convenience
pub use app_state::{ActionResult, AppState, AppEventSender, BannedPlayer, ModInfo, PregenState, ShutdownStatus};
pub use config::{ServerConfig, ServerType};
pub use helpers::remote_kill_server_and_playit;
pub use server::{ServerManager, ServerStatus};
pub use stats::ServerStats;
