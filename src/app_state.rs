// Shared application state and event sender — used by both agent and app.
// The agent wraps this in axum::State, the app wraps it in Tauri managed state.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, Mutex};

use crate::cloudflare::CloudflareTunnelState;
use crate::playit::PlayitState;
use crate::server::ServerManager;
use crate::stats::ServerStats;

const CONSOLE_BUFFER_CAP: usize = 2000;
const RECENT_PLAYERS_CAP: usize = 50;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModInfo {
    pub file_name: String,
    pub display_name: String,
    pub version: String,
    pub authors: Vec<String>,
    pub description: String,
    pub icon_data_url: Option<String>,
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

pub struct AppState {
    pub server: Mutex<ServerManager>,
    pub playit: Mutex<PlayitState>,
    pub stats: Mutex<ServerStats>,
    pub online_players: Mutex<HashSet<String>>,
    pub console_buffer: Mutex<VecDeque<String>>,
    pub recent_auto_restarts: Mutex<VecDeque<std::time::Instant>>,
    pub recent_players: Mutex<VecDeque<(String, String)>>,
    pub last_gametime_sample: Mutex<Option<(u64, std::time::Instant)>>,
    pub pregen: Mutex<PregenState>,
    pub config_write_lock: Mutex<()>,
    pub remote_control: Mutex<Option<tokio::task::JoinHandle<()>>>,
    pub remote_control_active_token: Mutex<String>,
    pub cloudflare_remote: Mutex<CloudflareTunnelState>,
    pub action_results: Mutex<Vec<ActionResult>>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            server: Mutex::new(ServerManager::new()),
            playit: Mutex::new(PlayitState::default()),
            stats: Mutex::new(ServerStats::default()),
            online_players: Mutex::new(HashSet::new()),
            console_buffer: Mutex::new(VecDeque::with_capacity(CONSOLE_BUFFER_CAP)),
            recent_auto_restarts: Mutex::new(VecDeque::new()),
            recent_players: Mutex::new(VecDeque::new()),
            last_gametime_sample: Mutex::new(None),
            pregen: Mutex::new(PregenState::default()),
            config_write_lock: Mutex::new(()),
            remote_control: Mutex::new(None),
            remote_control_active_token: Mutex::new(String::new()),
            cloudflare_remote: Mutex::new(CloudflareTunnelState::default()),
            action_results: Mutex::new(Vec::new()),
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

    pub fn path(&self) -> &std::path::Path {
        std::path::Path::new("")
    }
}
