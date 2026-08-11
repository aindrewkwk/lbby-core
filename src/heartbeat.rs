//! Dashboard heartbeat — keeps the app in sync with web.lbby.net.
//!
//! Every N minutes (configurable, default 5) the app POSTs a small JSON
//! payload to `/api/app/heartbeat`. The payload carries:
//!
//!   - `device_id` — stable device fingerprint for license binding
//!   - `servers` — snapshots of local server profiles (the dashboard
//!     upserts them as app-managed servers keyed by device + profile id)
//!
//! The server responds with:
//!
//!   - `license_token` — a fresh JWT if the admin changed the user's tier
//!   - `revoked: true` — if the license was revoked server-side
//!
//! When either arrives the local `license.jwt` is updated automatically
//! so the UI reflects the new tier without a restart.

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::config;
use crate::license;

/// How often we retry after a failed heartbeat (shorter than normal interval).
const RETRY_SECS: u64 = 60;

/// Response shape from `POST /api/app/heartbeat`.
#[derive(Debug, Deserialize, Default)]
struct HeartbeatResponse {
    #[serde(default)]
    license_token: Option<String>,
    #[serde(default)]
    revoked: Option<bool>,
}

/// Snapshot of one local server profile, synced to the dashboard.
/// The dashboard upserts a `servers` row keyed by (user, device_id, local_id).
#[derive(Debug, Clone, Serialize)]
pub struct AppServerSnapshot {
    /// Stable local profile id from profiles.json.
    pub local_id: String,
    pub name: String,
    pub server_type: String,
    pub minecraft_version: String,
    /// running / starting / stopping / stopped (agent vocabulary).
    pub status: String,
    pub player_count: u32,
    pub max_players: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tps: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_used_mb: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_max_mb: Option<u32>,
}

/// Synchronous provider for server snapshots (must never block — use try_lock).
pub type ServersProvider = std::sync::Arc<dyn Fn() -> Vec<AppServerSnapshot> + Send + Sync>;

/// Payload we send to the heartbeat endpoint.
#[derive(Debug, Serialize)]
struct HeartbeatPayload {
    hostname: String,
    /// Stable device fingerprint (32 hex chars) — lets the license server
    /// bind issued JWTs to this machine.
    device_id: String,
    app_version: String,
    os: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    servers: Vec<AppServerSnapshot>,
}

/// Runs the heartbeat loop. Call from a tokio::spawn in main.
/// `on_tier_change` is invoked (best-effort) whenever the tier changes,
/// so the UI can refresh. `servers_provider` (optional) supplies local
/// server snapshots to sync to the dashboard each beat.
pub async fn run(
    on_tier_change: Option<Arc<Mutex<dyn FnMut() + Send>>>,
    servers_provider: Option<ServersProvider>,
) {
    loop {
        let cfg = config::load_config();

        // Bail out if heartbeat is disabled
        if cfg.heartbeat_interval_secs == 0 {
            tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
            continue;
        }

        let interval = tokio::time::Duration::from_secs(cfg.heartbeat_interval_secs.max(30));

        let servers = servers_provider.as_ref().map(|f| f()).unwrap_or_default();

        match send_heartbeat(&cfg, servers).await {
            Ok(resp) => {
                let mut tier_changed = false;

                // Server issued a new license token (tier upgrade / refresh)
                if let Some(token) = resp.license_token {
                    match license::activate(&token) {
                        Ok(_) => {
                            eprintln!("[heartbeat] License updated from server");
                            tier_changed = true;
                        }
                        Err(e) => {
                            eprintln!("[heartbeat] Failed to save server license: {}", e);
                        }
                    }
                }

                // Server says the license was revoked
                if resp.revoked == Some(true) {
                    if let Err(e) = license::deactivate() {
                        eprintln!("[heartbeat] Failed to deactivate revoked license: {}", e);
                    } else {
                        eprintln!("[heartbeat] License revoked by server");
                        tier_changed = true;
                    }
                }

                // Notify the UI if anything changed
                if tier_changed {
                    if let Some(ref cb) = on_tier_change {
                        if let Ok(mut f) = cb.try_lock() {
                            f();
                        }
                    }
                }

                tokio::time::sleep(interval).await;
            }
            Err(e) => {
                eprintln!("[heartbeat] Failed: {}", e);
                // Retry sooner after a failure
                tokio::time::sleep(tokio::time::Duration::from_secs(RETRY_SECS)).await;
            }
        }
    }
}

/// Send a single heartbeat to the dashboard.
async fn send_heartbeat(
    cfg: &config::ServerConfig,
    servers: Vec<AppServerSnapshot>,
) -> Result<HeartbeatResponse, String> {
    let url = format!(
        "{}/api/app/heartbeat",
        cfg.dashboard_url.trim_end_matches('/')
    );

    let payload = HeartbeatPayload {
        hostname: get_hostname(),
        device_id: crate::license::device_fingerprint(),
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        os: std::env::consts::OS.to_string(),
        servers,
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| format!("HTTP client error: {}", e))?;

    let mut req = client
        .post(&url)
        .header("Content-Type", "application/json")
        .json(&payload);
    if !cfg.app_token.is_empty() {
        req = req.header("Authorization", format!("Bearer {}", cfg.app_token));
    }
    let resp = req.send().await
        .map_err(|e| format!("Request failed: {}", e))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Server returned {}: {}", status, body));
    }

    resp.json::<HeartbeatResponse>()
        .await
        .map_err(|e| format!("Invalid response: {}", e))
}

/// Build server snapshots from all local profiles. The ACTIVE profile reports
/// live stats from `state`; every other profile reports as stopped. Uses
/// `try_lock` so this never blocks the heartbeat loop.
pub fn build_snapshots(state: &crate::app_state::AppState) -> Vec<AppServerSnapshot> {
    let profiles = config::profiles_state();

    // Live status + stats for the active server (best-effort, non-blocking).
    let live_status: Option<String> = state
        .server
        .try_lock()
        .ok()
        .map(|guard| guard.status.sync_status().to_string());
    let live_stats: Option<crate::stats::ServerStats> =
        state.stats.try_lock().ok().map(|guard| (*guard).clone());

    profiles
        .profiles
        .into_iter()
        .map(|p| {
            let active = p.active;
            let server_type = serde_json::to_value(&p.server_type)
                .ok()
                .and_then(|v| v.as_str().map(|s| s.to_string()))
                .unwrap_or_default();
            let version = if p.game.is_terraria() {
                p.terraria_version
            } else {
                p.minecraft_version
            };

            if active {
                let stats = live_stats.clone().unwrap_or_default();
                let status = live_status
                    .clone()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "stopped".to_string());
                let running = status == "running";
                AppServerSnapshot {
                    local_id: p.id,
                    name: p.name,
                    server_type,
                    minecraft_version: version,
                    status,
                    player_count: if running { stats.players_online } else { 0 },
                    max_players: if running { stats.players_max } else { 0 },
                    tps: if running && stats.tps > 0.0 {
                        Some(stats.tps)
                    } else {
                        None
                    },
                    memory_used_mb: if running && stats.ram_used_mb > 0 {
                        Some(stats.ram_used_mb)
                    } else {
                        None
                    },
                    memory_max_mb: if running && stats.ram_max_mb > 0 {
                        Some(stats.ram_max_mb)
                    } else {
                        None
                    },
                }
            } else {
                AppServerSnapshot {
                    local_id: p.id,
                    name: p.name,
                    server_type,
                    minecraft_version: version,
                    status: "stopped".to_string(),
                    player_count: 0,
                    max_players: 0,
                    tps: None,
                    memory_used_mb: None,
                    memory_max_mb: None,
                }
            }
        })
        .collect()
}

/// Get the machine hostname without external crates.
fn get_hostname() -> String {
    // Try environment variables first (works on all platforms)
    if let Ok(h) = std::env::var("HOSTNAME") {
        if !h.is_empty() {
            return h;
        }
    }
    if let Ok(h) = std::env::var("COMPUTERNAME") {
        if !h.is_empty() {
            return h;
        }
    }
    // Fallback: run `hostname` command
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}
